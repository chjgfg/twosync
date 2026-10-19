//! M1 的会话：两个各自独立的站点，中间一根**内存管子**。
//!
//! 为什么不测真 SSH：§9.5 的传输只是"一根双向管"，正确性全在会话层。用 `mpsc` 当管子，
//! 同一份 `push`/`serve` 代码就能在 CI 里跑完，也不占磁盘（真 SSH 要一台对端机器）。
//! SSH 那一层的接线是 `cli::m1::push` 起 `ssh <peer> twosync agent` 的子进程，测的是同一套帧。
//!
//! 跑法：`cargo test --test net_session`
//!
//! 这套用例盯的是四件容易悄悄坏掉的事：
//! 1. **对端接受后 head 真的前移**，而且能 `log`/`show` 到推过去的版本；
//! 2. **第二次推送一个字都不发**（内容寻址去重 ⇒ 断点续传的基石，也是 §10.4 差分表的分子）；
//! 3. **大删除的 commit 被对端拒**，并且拒绝的是那一个 commit（M1 验收格）；
//! 4. 缺一个块就不许动 head（不变量 (b)：head 指向取不回来的版本 = 假历史）。

use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};

use storage_engines::EngineKind;
use twosync::cli::m1;
use twosync::error::{Error, Result as TsResult};
use twosync::net::frame::{self, Proto, PROTO_VER};
use twosync::net::session::{self, PushOpts, Report, ServeReport};
use twosync::store::build::{self, MAIN, Meta};
use twosync::store::cas::{self, Cas};
use twosync::store::gc;
use twosync::store::history;
use twosync::store::keys;
use twosync::store::object;
use twosync::store::worktree;
use twosync::utils::{self, objects_dir, open_store, site_id};

const PROJECT: &str = "shared";
/// §10.4 差分表那一臂用的项目名（语料是 `D:\ts10g\dt\a\lab`，与上面那套内存管道用例无关）。
const PROJECT_DT: &str = "lab";

// ── 内存管子 ────────────────────────────────────────────────────────────────

/// 单向队列拼成的双向管。`write` 把整块塞进队列（**无界** ⇒ 不会像真管道那样把写方堵死，
/// 这正是测试要的：测协议，不测背压）。
struct Pipe {
    tx: Sender<Vec<u8>>,
    rx: Receiver<Vec<u8>>,
    pending: VecDeque<u8>,
}

impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pending.is_empty() {
            match self.rx.recv() {
                Ok(chunk) => self.pending.extend(chunk),
                Err(_) => return Ok(0), // 对端关掉 = EOF，会话正常收尾
            }
        }
        let n = buf.len().min(self.pending.len());
        for (i, b) in self.pending.drain(..n).enumerate() {
            buf[i] = b;
        }
        Ok(n)
    }
}

impl Write for Pipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.tx
            .send(buf.to_vec())
            .map_err(|_| io::Error::other("对端已关闭"))?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn duplex() -> (Pipe, Pipe) {
    let (a_tx, a_rx) = mpsc::channel::<Vec<u8>>();
    let (b_tx, b_rx) = mpsc::channel::<Vec<u8>>();
    (
        Pipe {
            tx: a_tx,
            rx: b_rx,
            pending: VecDeque::new(),
        },
        Pipe {
            tx: b_tx,
            rx: a_rx,
            pending: VecDeque::new(),
        },
    )
}

// ── 站点 ───────────────────────────────────────────────────────────────────

struct Site {
    db: Box<dyn storage_engines::KvEngine>,
    pid: u32,
    tmp: tempfile::TempDir,
}

impl Site {
    fn open() -> Site {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = open_store(tmp.path(), EngineKind::LsmTree).expect("open_store");
        Site {
            db,
            pid: keys::project_id(PROJECT),
            tmp,
        }
    }

    /// 与 [`Site::open`] 同一件事，但把引擎套一层**门面调用计数器**（`utils::Meter`）。
    ///
    /// 为什么要这个形状：`open_store` 只在环境变量存在时才包（默认路径零开销），而测试要的是
    /// "**我自己**拿把手"——所以先正常开、再手工包，读账走把手而不是往 `KvEngine` 上加一个
    /// `fn counts()`（那等于为了让一把尺子去污染四个引擎的实现）。
    fn metered(meter: &twosync::utils::Meter) -> Site {
        let tmp = tempfile::tempdir().expect("tempdir");
        let inner = open_store(tmp.path(), EngineKind::LsmTree).expect("open_store");
        Site {
            db: twosync::utils::counted(inner, meter, false),
            pid: keys::project_id(PROJECT),
            tmp,
        }
    }

    fn proj(&self) -> PathBuf {
        self.tmp.path().join(PROJECT)
    }

    fn cas(&self) -> Cas {
        Cas::new(&objects_dir(self.tmp.path()), cas::DEFAULT_BLOCK).expect("CAS")
    }

    /// 写一个文件并提交。返回 commit hash 列表长度变化与否由调用方判。
    fn commit(&self, files: &[(&str, &[u8])], ts: i64) -> build::Built {
        let root = self.proj();
        fs::create_dir_all(&root).expect("项目目录");
        for (rel, body) in files {
            let p = root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).expect("父目录");
            }
            fs::write(&p, body).expect("写文件");
        }
        worktree::scan(&*self.db, self.pid, &root, false, 500).expect("scan");
        let store = self.cas();
        build::build(
            &*self.db,
            self.pid,
            &store,
            &root,
            Meta {
                peer: "a",
                msg: "用例",
                ts,
                allow_mass_delete: true,
            },
        )
        .expect("commit")
    }

    fn delete_and_commit(&self, paths: &[&str], ts: i64, allow: bool) -> Result<build::Built, Error> {
        let root = self.proj();
        for p in paths {
            fs::remove_file(root.join(p.replace('/', std::path::MAIN_SEPARATOR_STR))).expect("删文件");
        }
        worktree::scan(&*self.db, self.pid, &root, false, 500).expect("scan");
        let store = self.cas();
        build::build(
            &*self.db,
            self.pid,
            &store,
            &root,
            Meta {
                peer: "a",
                msg: "删一批",
                ts,
                allow_mass_delete: allow,
            },
        )
    }

    fn heads(&self) -> Vec<[u8; 32]> {
        build::ref_heads(&*self.db, self.pid, MAIN).expect("heads")
    }

    fn block_count(&self) -> usize {
        count_blocks(&objects_dir(self.tmp.path()))
    }
}

/// 数 CAS 里真实落了几个块文件（跳过 `.tmp.` 那些写一半的）。
///
/// 自由函数而不是 `Site` 的方法：对端在子进程跑完之前**不能**开着同一个 LSM（§9.6 的锁语义），
/// 所以那一份账只能在子进程退出之后、用裸目录去数。
fn count_blocks(dir: &Path) -> usize {
    let mut n = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            if e.path().is_dir() {
                stack.push(e.path());
            } else if !e.file_name().to_string_lossy().starts_with(".tmp.") {
                n += 1;
            }
        }
    }
    n
}

// ── 用例 ───────────────────────────────────────────────────────────────────

/// 每个 `i` 一个**互不相同**的流。`| 1` 那种"保证非零"的写法不许出现在种子这里：
/// 它把 (0,1)、(2,3)、(4,5) 各自折成同一个种子 ⇒ 一半文件内容相同 ⇒ CAS 正确去重，
/// 而用例把它们当成"6 个文件 6 份清单"来断言。**这是本文件第二次栽在同一个形状上**
/// （第一次是 10 GiB 生成器的 `i as u8`），所以规矩是：**生成器必须先自证两两不同**。
fn body(i: usize, len: usize) -> Vec<u8> {
    let mut x = (i as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 31) as u8
        })
        .collect()
}

fn seed(n: usize) -> Vec<(String, Vec<u8>)> {
    let v: Vec<(String, Vec<u8>)> = (0..n)
        .map(|i| (format!("d{i}/f{i}.bin"), body(i, 300_000)))
        .collect();
    // 尺子自证（§10.1 那条规矩对测试台架同样适用）：内容必须两两不同，否则
    // "n 个文件 ⇒ n 份清单 ⇒ n 个块"的分母是假的，用例就是在断言去重之后的数。
    let mut uniq = std::collections::BTreeSet::new();
    for (_, b) in &v {
        assert!(uniq.insert(b), "生成器造出了重复内容 ⇒ 用例的分母不作数");
    }
    v
}

// ── 一整场会话 ──────────────────────────────────────────────────────────────

/// 新开一对内存管道，一头 `push`、另一头在**线程里** `serve`。
///
/// 为什么要两条线程：会话是"我说一句、等一句答"的形状，同线程串行跑就是发起端在等一个
/// 还没被读走的回帧 ⇒ 两边都不动 = 死锁。
///
/// 这里返回**原始** `Result`：`a_missing_block_stops_the_head_from_moving` 要看的正是
/// "两边各自怎么错的"（发起端读块失败、接收端只是管子到尾），一 panic 就什么线索都不剩了。
fn round(
    a: &Site,
    b: &Site,
    opts: &PushOpts<'_>,
) -> (Result<Report, Error>, Result<ServeReport, Error>) {
    let (mut pa, mut pb) = duplex();
    std::thread::scope(|sc| {
        let h = sc.spawn(|| {
            let cas_b = b.cas();
            let mut resolve = |_n: &str| Ok::<u32, String>(b.pid);
            session::serve(&mut pb, &*b.db, &cas_b, b"peer-b", 0xFFFF_FFFF, &mut resolve, false)
        });
        let cas_a = a.cas();
        let sent = session::push(&mut pa, &*a.db, &cas_a, opts);
        // **先关写端再 join**。真管子上发起端一退出 ssh 就把 stdin 关了，agent 读到尾自己收尾;
        // 这里的 `Pipe` 若不 drop，Sender 还活着 ⇒ 对面的 `recv()` 永远等不到"关了"⇒ 两边互相等。
        // （这条不是测试的临时补丁，是把台架改成与真实传输同语义：少了它，报错路径没法验。）
        drop(pa);
        let served = match h.join() {
            Ok(r) => r,
            Err(_) => Err(Error::BadInput("serve 线程 panic（见上面的 panic 输出）".to_string())),
        };
        (sent, served)
    })
}

/// 同一件事，但任何一方报错就直接炸出来：多数用例要的是"会话成功"，把两层 `Result` 摊平成
/// 一个 `(Report, ServeReport)`，断言里就不用写一串 `.unwrap()`。
fn round_ok(a: &Site, b: &Site, opts: &PushOpts<'_>) -> (Report, ServeReport) {
    let (sent, served) = round(a, b, opts);
    (sent.expect("push 该成功"), served.expect("serve 该成功"))
}

fn opts<'a>(project: &'a str) -> PushOpts<'a> {
    PushOpts {
        project,
        peer_id: b"peer-a",
        ..Default::default()
    }
}

// ── 真子进程那一侧的公共动作 ──────────────────────────────────────────────

/// 命令行可执行文件（cargo 给集成测试留的 `CARGO_BIN_EXE_<name>`，它会自动把 bin 编出来）。
const EXE: &str = env!("CARGO_BIN_EXE_twosync");

/// 在一个空目录上**只注册**项目（不配对）。§9.5.3 之后"注册了"不等于"愿意收"，
/// 所以这一条专门给"没配对就该拒"的用例用。
fn add_project(exe: &str, root: &Path, project: &str) {
    fs::create_dir_all(root.join(project)).expect("项目目录");
    let out = Command::new(exe)
        .args(["--root", &root.to_string_lossy(), "project", "add", project])
        .output()
        .expect("起不了 project add");
    assert!(
        out.status.success(),
        "project add 失败：{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// 注册 + 配对（§9.5.3：两边各确认一次）。`agent` 的白名单是"注册且已配对"，
/// 所以绝大多数用例走这一条；`--remote-root`/`peer_root` 在这里只是个占位路径，
/// 会话不读它的内容。
fn register(exe: &str, root: &Path, project: &str, peer_root: &str) {
    add_project(exe, root, project);
    let out = Command::new(exe)
        .args([
            "--root",
            &root.to_string_lossy(),
            "project",
            "pair",
            project,
            peer_root,
        ])
        .output()
        .expect("起不了 project pair");
    assert!(
        out.status.success(),
        "project pair 失败：{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// 把 `agent` 起成子进程并拼成一根管子。**测试要走的就是 `push` 走的那条构造**，
/// 所以这里不自己拼 `Duplex` 的字段。
fn agent_child(exe: &str, root: &Path, project: &str) -> m1::Duplex {
    let child = Command::new(exe)
        .args([
            "--root",
            &root.to_string_lossy(),
            "agent",
            "--project",
            project,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // agent 的诊断走 stderr，继承进来：`--nocapture` 时看得见它到底说了什么
        .stderr(Stdio::inherit())
        .spawn()
        .expect("起不了 agent 子进程");
    m1::Duplex::from_child(child).expect("管子两头都得在")
}

/// 一次成功的推送：对端 head 前移，块真的落在对端的 CAS 里，服务端的账与发起方对得上。
#[test]
fn push_moves_the_peer_head_and_lands_the_blocks() {
    let a = Site::open();
    let b = Site::open();
    let files = seed(6);
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, body)| (p.as_str(), body.as_slice()))
        .collect();
    let head = a.commit(&refs, 1).commit.expect("用例首轮必有提交");

    let (report, served) = round_ok(&a, &b, &opts(PROJECT));

    assert_eq!(report.commits, 1, "推一个 commit");
    assert_eq!(
        report.blocks_sent,
        a.block_count(),
        "对端是空库 ⇒ 本端所有块都该上线"
    );
    assert!(report.bytes_sent > 0);
    assert_eq!(report.accepted, vec![head], "对端接受后的 head 就是推过去的那个");
    assert_eq!(b.heads(), vec![head], "对端的 H/main 真的前移了");
    assert_eq!(b.block_count(), a.block_count(), "对端 CAS 里块数要对得上");
    assert_eq!(served.blocks_written, report.blocks_sent);
    assert_eq!(served.blobs_in, 6, "6 个文件 6 份清单");
    assert!(served.accepted, "服务端也得记成被接受：{}", served.reason);
}

/// 第二遍推同一个版本：**一个字节都不发**。这是 §10.4 差分表的地板（无变化必须是 0），
/// 真正"改了才传"的那一格在 `an_incremental_push_sends_only_the_changed_blocks`。
#[test]
fn a_second_push_sends_no_bytes() {
    let a = Site::open();
    let b = Site::open();
    let files = seed(4);
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, b_)| (p.as_str(), b_.as_slice()))
        .collect();
    a.commit(&refs, 1);

    // 一场管子连开**两场**会话 = FR-19 的形状（`agent` 就是循环 `serve`，认证只在 ssh 那一层）。
    // 第二遍没有新 commit ⇒ 待推集合是空的：§10.4 差分表的地板（"什么都不变"必须是 0 字节，
    // 不是"少了几个块"），也是断点续传的基石。
    let (mut pa, mut pb) = duplex();
    let (first, second) = std::thread::scope(|sc| {
        let h = sc.spawn(|| {
            let cas_b = b.cas();
            let mut resolve = |_n: &str| Ok::<u32, String>(b.pid);
            let one = session::serve(&mut pb, &*b.db, &cas_b, b"peer-b", 0xFFFF_FFFF, &mut resolve, false)
                .expect("serve 第一场");
            let two = session::serve(&mut pb, &*b.db, &cas_b, b"peer-b", 0xFFFF_FFFF, &mut resolve, false)
                .expect("serve 第二场");
            (one, two)
        });
        let cas_a = a.cas();
        let o = opts(PROJECT);
        let one = session::push(&mut pa, &*a.db, &cas_a, &o).expect("push 第一遍");
        let two = session::push(&mut pa, &*a.db, &cas_a, &o).expect("push 第二遍");
        drop(pa);
        let (s1, s2) = h.join().expect("serve 线程不该 panic");
        ((one, s1), (two, s2))
    });

    assert!(first.0.bytes_sent > 0, "第一遍必须真的传东西");
    assert_eq!(first.1.blobs_in, 4, "第一场：4 份清单落对端");
    assert_eq!(
        second.0.commits, 0,
        "两边 head 已经相等 ⇒ 待推集合是空的（这才是「一个字节都不发」的原因）"
    );
    assert_eq!(second.0.blocks_sent, 0, "对端全有了 ⇒ 一块都不发");
    assert_eq!(second.0.bytes_sent, 0, "字节数也必须为 0");
    assert_eq!(second.0.blobs_sent, 0, "清单也一样不发");
    assert_eq!(
        second.0.blocks_skipped, 0,
        "跳过的必须是 0：待推集合本身就是空的，连「想要」都谈不上。\
         拿上次发过的数充「去重读数」等于把 §10.4 的分子编出来"
    );
    assert_eq!(
        second.1.blocks_written, 0,
        "服务端那一侧也必须一个块都没重写（它按清单查过库了）"
    );
    assert!(second.1.accepted, "空推进不该被拒：{}", second.1.reason);
}

/// M1 那条硬判据的形状：**改 1 个字节 ⇒ 只传那一个块**（原文是"10 GB 文件改 1 MB → 传输 ≤ 8 MB"）。
/// 这里用 6 个文件、每个 300 KB（一块一文件）来钉住同一个机制，代价是常数级。
#[test]
fn an_incremental_push_sends_only_the_changed_blocks() {
    let a = Site::open();
    let b = Site::open();
    let files = seed(6);
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, b_)| (p.as_str(), b_.as_slice()))
        .collect();
    a.commit(&refs, 1);
    round_ok(&a, &b, &opts(PROJECT));

    // 本端只改第 3 个文件的一个字节并提交
    let victim = a.proj().join("d3").join("f3.bin");
    let mut body = fs::read(&victim).expect("读原文件");
    body[1234] ^= 0xFF;
    fs::write(&victim, &body).expect("改一个字节");
    worktree::scan(&*a.db, a.pid, &a.proj(), false, 500).expect("scan");
    let store = a.cas();
    let built = build::build(
        &*a.db,
        a.pid,
        &store,
        &a.proj(),
        Meta {
            peer: "a",
            msg: "改一个字节",
            ts: 2,
            allow_mass_delete: true,
        },
    )
    .expect("增量提交");
    assert_eq!(built.blocks_stored, 1, "本端就该只新写一个块");
    assert_eq!(built.blobs_new, 1, "只有那个文件的清单是新的");

    let (rep, served) = round_ok(&a, &b, &opts(PROJECT));
    assert_eq!(rep.commits, 1, "只推那一个新 commit");
    assert_eq!(rep.blocks_sent, 1, "改一个字节 ⇒ 上线一个块，不是六个");
    // **差集之后 `blocks_skipped` 该是 0**，而这不是"去重没了"：另外 5 个文件的清单压根没被收集，
    // 所以连问都没问（`need` 里只有那个新块）。"问过但对面已有"那种跳过只在**一份清单内部**还出现，
    // 下面 `a_changed_blob_still_negotiates_its_unchanged_blocks` 钉的就是那一格。
    assert_eq!(
        rep.blocks_skipped, 0,
        "未变的文件不再进 want/have 名单：{rep:?}"
    );
    assert_eq!(rep.blobs_sent, 1, "六份清单里只有变了的那一份要发：{rep:?}");
    assert_eq!(rep.trees_sent, 2, "只有 d3 与 root 这两层变了：{rep:?}");
    assert_eq!(
        served.bytes_in,
        rep.bytes_sent,
        "两边对账：发出去的字节 == 落进对端仓的字节"
    );
    assert_eq!(b.block_count(), a.block_count(), "对端块数追平本端");
    assert_eq!(b.heads(), a.heads(), "两边 head 追平");
}

/// 差集优化之后 `blocks_skipped` 还剩什么可跳：**同一份清单里那些没动过的块**。
///
/// 为什么单独一条：上面那条用例改完之后 `blocks_skipped` 变成 0（另外 5 个文件的清单根本没被收集，
/// 连问都没问），如果只看那一条，"只传脏块"这件事就退化成"整份文件的重发也没省"。定长 4 MiB 切块下
/// 改中间那一块 ⇒ 前后两块逐字节相同 ⇒ 新清单引用 3 块、问过 3 块、对面已有 2 块、上线 1 块。
/// 这一条把 FR-5 真正的意思钉住：**省的是块，不是文件**。
#[test]
fn a_changed_blob_still_negotiates_its_unchanged_blocks() {
    let a = Site::open();
    let b = Site::open();
    // 9 MiB ⇒ 三块：4 + 4 + 1（块与块内容互不相同，`body` 的种子保证）
    let nine = body(9, 9 * 1024 * 1024);
    a.commit(&[("big.bin", &nine)], 1);
    round_ok(&a, &b, &opts(PROJECT));

    // 改第二块中间的一个字节 ⇒ 只有这一块变脏
    let victim = a.proj().join("big.bin");
    let mut raw = fs::read(&victim).expect("读原文件");
    let off = 5 * 1024 * 1024;
    raw[off] ^= 0xFF;
    fs::write(&victim, &raw).expect("改一个字节");
    worktree::scan(&*a.db, a.pid, &a.proj(), false, 500).expect("scan");
    let store = a.cas();
    let built = build::build(
        &*a.db,
        a.pid,
        &store,
        &a.proj(),
        Meta {
            peer: "a",
            msg: "改中间那块",
            ts: 2,
            allow_mass_delete: true,
        },
    )
    .expect("增量提交");
    assert_eq!(built.blocks_stored, 1, "本端只新写一块（另外两块内容没变）");

    let (rep, served) = round_ok(&a, &b, &opts(PROJECT));
    assert_eq!(rep.blocks_sent, 1, "上线的只有脏掉那一块：{rep:?}");
    assert_eq!(
        rep.blocks_skipped, 2,
        "同一份清单里那两个未变的块问过、对面已有 ⇒ 这才是 want/have 的账：{rep:?}"
    );
    assert_eq!(rep.blobs_sent, 1, "只有一份清单变了：{rep:?}");
    assert_eq!(rep.trees_sent, 1, "文件在项目根，只有 root 这一层：{rep:?}");
    assert_eq!(served.bytes_in, rep.bytes_sent, "两边对账");
    assert_eq!(b.block_count(), a.block_count(), "对端块数追平本端");
    assert_eq!(b.heads(), a.heads(), "两边 head 追平");
}


/// 本地删掉一大半再提交：本端提得动（`allow_mass_delete` 是发起方自己的选择），
/// **对端必须拒**，而且拒绝的是那个 commit —— 之前的历史仍可用。
#[test]
fn the_peer_refuses_a_mass_delete_commit_and_keeps_the_old_history() {
    let a = Site::open();
    let b = Site::open();
    let files = seed(40);
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, b_)| (p.as_str(), b_.as_slice()))
        .collect();
    let first = a.commit(&refs, 1).commit.unwrap();

    // 先正常推一次，让对端有"上一个版本"可回退
    round_ok(&a, &b, &opts(PROJECT));
    assert_eq!(b.heads(), vec![first]);

    // 本端删掉 30 个（75%）并允许自己提交
    let doomed: Vec<String> = (0..30).map(|i| format!("d{i}/f{i}.bin")).collect();
    let as_str: Vec<&str> = doomed.iter().map(|s| s.as_str()).collect();
    let big = a
        .delete_and_commit(&as_str, 2, true)
        .expect("本端允许自己提交大删除");
    let bad = big.commit.expect("大删除提交本身要成立");

    // 发起方放开自己，仍然挡不住对端的闸 —— 这条测的就是"两边各自判"
    let o = PushOpts {
        allow_mass_delete: true,
        ..opts(PROJECT)
    };
    let (push_err, served) = round(&a, &b, &o);
    let err = push_err.expect_err("对端该拒");
    let served = served.expect("服务端本身该正常收完这一场，只是把 commit 拒了");
    assert!(
        err.to_string().contains("熔断") || err.to_string().contains("拒绝"),
        "报错要能看出是谁拒的：{err}"
    );
    assert!(!served.accepted, "服务端记录里也必须是被拒：{}", served.reason);
    assert_eq!(
        b.heads(),
        vec![first],
        "被拒的是那个 commit，不是整场会话：对端 head 必须还停在上一版"
    );
    assert_ne!(bad, first);
}

/// head 前移之前必须确认"闭得上"：偷偷抹掉一个块文件，接的那一端要拒绝并把 head 留在原地。
#[test]
fn a_missing_block_stops_the_head_from_moving() {
    let a = Site::open();
    let b = Site::open();
    let files = seed(3);
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, b_)| (p.as_str(), b_.as_slice()))
        .collect();
    a.commit(&refs, 1);

    // 制造"清单说有、盘上没有"：把本端的一个块改名，push 时 WantHave 会说对端缺，
    // 但读那块会失败 ⇒ 发起方自己先报错（写前自证那一族）。
    let objects = objects_dir(a.tmp.path());
    let mut victim: Option<PathBuf> = None;
    let mut stack = vec![objects];
    while let Some(d) = stack.pop() {
        if victim.is_some() {
            break;
        }
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            if e.path().is_dir() {
                stack.push(e.path());
            } else {
                victim = Some(e.path());
                break;
            }
        }
    }
    let victim = victim.expect("用例里至少有块");
    fs::rename(&victim, victim.with_extension("gone")).expect("改名");

    let (sent, served) = round(&a, &b, &opts(PROJECT));
    let err = match sent {
        Ok(_) => panic!("本地少一个块却推成功了 ⇒ 写前自证没挡住"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("缺失") || err.contains("校验"),
        "报错要说清是那块读不到：{err}"
    );
    assert!(
        served.is_err(),
        "发起端一报错就关了管子 ⇒ 服务端只能读到尾，不该当成一场成功的会话"
    );
    assert!(b.heads().is_empty(), "对端压根没接受任何 commit：head 一步都没动");
}

/// 纯函数：union 判定的两条方向都不能错（快进收成一个头、分叉留两个）。
#[test]
fn maximal_heads_covers_fast_forward_and_divergence() {
    let a = Site::open();
    let files = seed(2);
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, b_)| (p.as_str(), b_.as_slice()))
        .collect();
    let h1 = a.commit(&refs, 1).commit.unwrap();
    let h2 = a.commit(&[("more.bin", body(9, 1000).as_slice())], 2)
        .commit
        .expect("第二轮该有提交");

    // 快进：{h1, h2} 的极大元只有 h2
    assert_eq!(session::maximal_heads(&*a.db, a.pid, &[h1, h2]), vec![h2]);
    // 反过来给序也不能变结果（极大元与输入顺序无关）
    assert_eq!(session::maximal_heads(&*a.db, a.pid, &[h2, h1]), vec![h2]);
    // 单点：还是它自己
    assert_eq!(session::maximal_heads(&*a.db, a.pid, &[h1]), vec![h1]);

    // 分叉：把 head 倒回 h1 再提一轮 ⇒ h2 与 h3 互不可达，两个都得留，公共祖先 h1 被剔掉。
    // **这一条是判据方向的另一半**：只测快进的话，"我的可达集里有没有别人"那个反着写的版本
    // 也会通过（单头时两种写法同结果）。
    build::set_ref(&*a.db, a.pid, MAIN, &[h1]).expect("把 head 倒回 h1");
    let h3 = a
        .commit(&[("other.bin", body(11, 1000).as_slice())], 3)
        .commit
        .expect("分叉那一轮该有提交");
    let mut want = vec![h2, h3];
    want.sort();
    let mut gave = session::maximal_heads(&*a.db, a.pid, &[h1, h2, h3]);
    assert!(!gave.contains(&h1), "h1 是 h2/h3 的祖先，不该再算头：{gave:?}");
    gave.sort();
    assert_eq!(gave, want, "分叉的两个头都要保留（§4.1 只增不减）");

    // 拓扑序：亲代在前
    let all = session::reachable(&*a.db, a.pid, &[h2], &[]);
    let order = session::topo_pending(&*a.db, a.pid, &all, &Default::default());
    assert_eq!(order, vec![h1, h2], "h1 必须在 h2 之前：{order:?}");
}

/// 真子进程 + 真 OS 管道跑完整一场 `push → agent`。
///
/// 为什么单独一条：`cli::m1` 那两层（拼远端命令、`Duplex`、`finish()` 等退出码）在内存管道用例里
/// **一行都没执行过**；而内存管道是**无界**的 ⇒ "大帧会不会把写端堵死"这件事只有真管道测得出来
/// （本机已经栽过两次：20 000 行 stdout 把采样器堵死过一次，见 §17 补记十四）。
///
/// 对端只用命令行（`project add` + `agent`），而**验证在子进程退出之后才开句柄**：同一份 LSM 目录
/// 两个进程同时开着是不行的（§9.6），所以这里刻意把两侧的时段错开。
#[test]
fn push_over_a_real_child_process_and_pipes() {
    let a = Site::open();
    let files = seed(3);
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, b_)| (p.as_str(), b_.as_slice()))
        .collect();
    let head = a.commit(&refs, 1).commit.expect("用例首轮必有提交");
    let tree = object::read_commit(&*a.db, a.pid, &head)
        .expect("读本端的 commit")
        .tree;
    let sent = a.block_count();
    assert!(sent >= 3, "3 个文件至少 3 个块：{sent}");

    let b_tmp = tempfile::tempdir().expect("对端站点");
    register(EXE, b_tmp.path(), PROJECT, "D:/peer-site-root");
    let mut io = agent_child(EXE, b_tmp.path(), PROJECT);

    let rep = session::push(&mut io, &*a.db, &a.cas(), &opts(PROJECT))
        .expect("真管道上的一次 push 该成功");
    io.finish().expect("关掉 stdin 之后 agent 该读到尾、退出码 0");

    assert_eq!(rep.blocks_sent, sent, "对端是空站 ⇒ 块全上线");
    assert_eq!(rep.accepted, vec![head], "发起方拿到的 head 就是推过去的那个");

    // 子进程已经退出 ⇒ 现在才允许自己开对端那份库
    let b_db = open_store(b_tmp.path(), EngineKind::LsmTree).expect("开对端的库");
    let pid = keys::project_id(PROJECT);
    assert_eq!(
        build::ref_heads(&*b_db, pid, MAIN).expect("对端 head"),
        vec![head],
        "对端的 H/main 真的前移了"
    );
    assert_eq!(
        count_blocks(&objects_dir(b_tmp.path())),
        sent,
        "块必须真的落在对端的 CAS 里，不只是发出去了"
    );
    assert_eq!(
        object::read_commit(&*b_db, pid, &head)
            .expect("读回对端的 commit")
            .tree,
        tree,
        "同一份历史：两边的根 tree 必须一字不差"
    );
}

/// §10.2 的第一刀：**对端在会话中途被杀**。FR-13 的承诺是"动 head 是最后一步"，所以半路死的对端
/// 必须留下三样东西：head 不动、清单不进库、落下的块只是孤块（块级回收已经做了，见下面第二刀与 §9.3）。
/// 更要紧的是**下一次完整 push 能追平**——半成品不许把站点锁死。
///
/// 这一条**手工驱动协议**而不用 `session::push`：要的就是"只发两个段然后杀掉"这个精确时刻，
/// 而 `push` 一旦开始就不会在中途被我们掐住（掐住的地方也就不是"对端死了"而是"我这边出事了"）。
#[test]
fn a_peer_killed_mid_session_leaves_no_half_applied_history() {
    let a = Site::open();
    let files = seed(3);
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, b_)| (p.as_str(), b_.as_slice()))
        .collect();
    let head = a.commit(&refs, 1).commit.expect("用例首轮必有提交");
    let tree = object::read_commit(&*a.db, a.pid, &head)
        .expect("读 commit")
        .tree;

    // 凑齐要发的块：走一遍树拿 blob hash，再从清单里取 (hash, size)。
    let mut blocks: Vec<(keys::Hash, u32)> = Vec::new();
    history::walk_tree(&*a.db, a.pid, &tree, &mut |_path, e| {
        if e.kind == keys::Kind::File {
            let b = build::read_blob(&*a.db, a.pid, &e.hash).expect("读 blob");
            for blk in &b.blocks {
                blocks.push((blk.hash, blk.size));
            }
        }
        Ok(())
    })
    .expect("walk 树");
    assert_eq!(blocks.len(), 3, "3 个文件各 1 块：{blocks:?}");

    let b_tmp = tempfile::tempdir().expect("对端站点");
    register(EXE, b_tmp.path(), PROJECT, "D:/peer-site-root");
    // 这一条**不能用 `agent_child`**：它要自己捏着子进程的 stdin/stdout 手工驱动协议，
    // 好在"只写完两块"的那一刻把进程杀掉。
    let mut child = Command::new(EXE)
        .args([
            "--root",
            &b_tmp.path().to_string_lossy(),
            "agent",
            "--project",
            PROJECT,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("起不了 agent 子进程");
    let mut w = child.stdin.take().expect("piped stdin");
    let mut r = child.stdout.take().expect("piped stdout");

    frame::encode(
        &mut w,
        &Proto::Hello {
            proto_ver: PROTO_VER,
            peer_id: b"crash-test".to_vec(),
            caps: u32::MAX,
        },
    )
    .expect("发 Hello");
    expect_frame(frame::decode(&mut r), |p| matches!(p, Proto::Hello { .. }), "Hello");

    frame::encode(
        &mut w,
        &Proto::BeginSession {
            project: PROJECT.to_string(),
            pid: a.pid,
            my_heads: vec![head],
        },
    )
    .expect("发 BeginSession");
    expect_frame(
        frame::decode(&mut r),
        |p| matches!(p, Proto::HeadsState { .. }),
        "HeadsState",
    );

    frame::encode(
        &mut w,
        &Proto::WantHave {
            want: blocks.iter().map(|(h, _)| *h).collect(),
        },
    )
    .expect("发 WantHave");
    frame::encode(&mut w, &Proto::HaveDone).expect("发 HaveDone");
    let mut missing: Vec<keys::Hash> = Vec::new();
    loop {
        let p = frame::decode(&mut r).expect("读 MissingBlocks");
        match p {
            Proto::MissingBlocks { hashes } => missing.extend(hashes),
            Proto::MissingDone => break,
            other => panic!("该收 MissingBlocks/MissingDone，收到 {other:?}"),
        }
    }
    assert_eq!(missing.len(), 3, "空站当对端 ⇒ 三块全缺");

    // announce 三块，只**写完两块**就掐断 ⇒ 第三块与全部清单都还没到。
    let bytes: u64 = blocks.iter().map(|(_, s)| *s as u64).sum();
    frame::encode(&mut w, &Proto::PushBlocks { count: 3, bytes }).expect("发 PushBlocks");
    for (h, size) in &blocks[..2] {
        let body = a.cas().read_block(h, *size).expect("读本端块");
        frame::write_segment(&mut w, h, &body).expect("发块段");
    }
    w.flush().expect("flush");
    child.kill().expect("杀掉对端");
    let _ = child.wait();
    drop(w);
    drop(r);

    let pid = keys::project_id(PROJECT);
    let b_db = open_store(b_tmp.path(), EngineKind::LsmTree).expect("对端的库必须还开得起来");
    assert!(
        build::ref_heads(&*b_db, pid, MAIN)
            .expect("读 head")
            .is_empty(),
        "半路死的会话不许动 head"
    );
    assert!(
        object::read_commit(&*b_db, pid, &head).is_err(),
        "commit 对象压根没发过去 ⇒ 库里不该有它"
    );
    assert!(
        count_blocks(&objects_dir(b_tmp.path())) <= 2,
        "最多落下已经发过去的那两块，不许有更多"
    );

    // 块文件落了两块，但**账上一行都不该有**：`serve` 只在清单到达时才 `adopt_blob`（写 `B`/`K`），
    // 而清单在这一刀之后才会在网络上出现。这一条盯的就是"半应用的历史"里最危险的那种——
    // 文件在、账也在、而 commit 不在 ⇒ 那才是需要 GC 判断的悬空态。
    let (krows, brows) = {
        let tx = b_db.begin();
        let k = tx.prefix_scan(&{
            let mut p = vec![keys::TAG_BLOCK];
            p.extend_from_slice(&pid.to_be_bytes());
            p
        });
        let b = tx.prefix_scan(&{
            let mut p = vec![keys::TAG_BLOB];
            p.extend_from_slice(&pid.to_be_bytes());
            p
        });
        tx.commit();
        (k.len(), b.len())
    };
    assert!(krows == 0, "崩在清单之前 ⇒ `K` 行该是 0 行，实际 {krows}");
    assert!(brows == 0, "崩在清单之前 ⇒ `B` 行该是 0 行，实际 {brows}");
    drop(b_db);

    // §12 那条"直接去撞 `storage-engines` 的 WAL 恢复"的狗粮项：**崩过的站点重新打开跑 FR-21 自检**。
    // 起子进程而不是在测试里调 `m0::doctor`——FR-21 说的就是"进程启动时"那一次，同进程里再开一个
    // 句柄走不到那条路径（而且上一个句柄还开着会撞库锁，见 `tests/gc_sweep.rs` 那条 `WouldBlock`）。
    let out = Command::new(EXE)
        .args(["--root", &b_tmp.path().to_string_lossy(), "doctor"])
        .output()
        .expect("崩过的站点跑 doctor");
    let rep = String::from_utf8_lossy(&out.stdout).to_string();
    println!("doctor(崩溃之后) = {}", rep.replace('\n', " | "));
    assert!(
        out.status.success(),
        "重开一个崩过的站点必须起得来，stderr = {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(rep.contains("engine      = lsm_tree"), "读数缺行：{rep}");
    // `active_txns` 是**新句柄**的账，本来就归 0 ⇒ 这一格证明的是"WAL 恢复之后库能干净重开"，
    // 不是"崩掉的那个进程没留残留事务"（后者在同进程里根本观测不到，别把它读成已验证）。
    assert!(rep.contains("active_txns = 0"), "重开之后不该看得见活跃事务：{rep}");

    // 恢复：下一次**完整**会话必须把它追平，半成品不许把站点锁死。
    let mut io = agent_child(EXE, b_tmp.path(), PROJECT);
    let rep =
        session::push(&mut io, &*a.db, &a.cas(), &opts(PROJECT)).expect("崩溃之后的一次完整 push");
    io.finish().expect("agent 该正常收尾");
    assert!(rep.bytes_sent > 0, "第三块得补发出去");
    // 这一对数是 §11 的 M2 判据"**拔网重连自动续传**"的实际形状：重连不是"从头再传一遍"。
    // 协商（`want/have`）问的是"对面 CAS 里**文件**在不在"，所以已经落盘的块会被跳过——
    // 哪怕它们一行账都还没写（上面刚验过 `K`/`B` 都是 0 行）。
    assert_eq!(
        rep.blocks_sent + rep.blocks_skipped,
        3,
        "三块都得在账上，要么上线要么跳过：{rep:?}"
    );
    assert!(
        rep.blocks_sent < 3,
        "重连把已经落盘的块也重发了 ⇒ 那不叫续传（上线 {} / 跳过 {}）",
        rep.blocks_sent,
        rep.blocks_skipped
    );
    println!(
        "续传读数 = 上线 {} 块 / {:.1} MiB，跳过 {} 块（已经落盘的那几块）",
        rep.blocks_sent,
        rep.bytes_sent as f64 / 1048576.0,
        rep.blocks_skipped
    );

    let b_db = open_store(b_tmp.path(), EngineKind::LsmTree).expect("再开一次对端的库");
    assert_eq!(
        build::ref_heads(&*b_db, pid, MAIN).expect("读 head"),
        vec![head],
        "追平之后 head 到位"
    );
    assert_eq!(
        object::read_commit(&*b_db, pid, &head)
            .expect("读回 commit")
            .tree,
        tree,
        "追平之后的根 tree 与本端一致"
    );
}

/// §10.2 的**第二刀**：清单、tree、commit **都已经进库**了，`VerifyCommit` 还没到就对端就没了。
///
/// 这一刀的残留与第一刀根本不同：第一刀只留下"没人引用的块文件"（账上一个字节都没写），
/// 这一刀留下的是**一整套对象**——`C`/`T`/`B` 行齐全、`K` 账也加了、块也落盘了，只是**没有任何 ref 指着它**。
/// 于是它同时验三件硬事：
/// 1. `VerifyCommit` 是唯一动 head 的闸（对象齐了 head 也不许动）；
/// 2. §9.3 的 mark-sweep 判垃圾用的是"**全部 ref 的可达闭包**"⇒ 这一整套必须被回收，
///    包括 `K` 账与块文件（`session.rs` 模块头那句"中途崩溃留下的是没人引用的 `B`/`K` 行，
///    §9.3 的引用计数 GC 干的就是这个"到这里才第一次被用例兑现）；
/// 3. 回收之后站点还能被一次完整 push 追平（GC 不许把站点做废）。
///
/// **这一刀不用 `kill`，用的是"发完对象帧就关掉写端"**，理由是一条关于测试台架的硬事实：
/// `kill` 的时刻我们**观测不到**对端读到哪儿了。这一条第一版写的就是 `child.kill()`，
/// 结果"commit 行确实在库里"当场失败——进程在读到那帧之前就被杀了，于是断言的其实是"什么都没落"，
/// 与第一刀重复。**要断言"没落"可以用 `kill`，要断言"已落"必须有一个确定性同步点**；
/// 管子对读方的 EOF 就是这样的点：进管子的字节会先被读完，然后才到尾。
#[test]
fn objects_that_land_without_verify_are_gc_garbage() {
    let a = Site::open();
    let files = seed(3);
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, b_)| (p.as_str(), b_.as_slice()))
        .collect();
    let head = a.commit(&refs, 1).commit.expect("用例首轮必有提交");
    let tree = object::read_commit(&*a.db, a.pid, &head)
        .expect("读 commit")
        .tree;

    // 逐层收对象：root tree + 每个子目录的 tree（`seed` 是 `d{i}/f{i}.bin` ⇒ 4 棵树）、
    // 每个文件的清单、以及 commit 本体。块的 `(hash,size)` 从清单里取。
    let mut queue: VecDeque<keys::Hash> = vec![tree].into_iter().collect();
    let mut trees: Vec<Vec<object::TreeEntry>> = Vec::new();
    let mut blobs: Vec<cas::Blob> = Vec::new();
    let mut blocks: Vec<(keys::Hash, u32)> = Vec::new();
    while let Some(h) = queue.pop_front() {
        let entries = object::read_tree(&*a.db, a.pid, &h).expect("读 tree");
        for e in &entries {
            match e.kind {
                keys::Kind::Dir => queue.push_back(e.hash),
                keys::Kind::File => {
                    let b = build::read_blob(&*a.db, a.pid, &e.hash).expect("读 blob");
                    for blk in &b.blocks {
                        blocks.push((blk.hash, blk.size));
                    }
                    blobs.push(b);
                }
                _ => {}
            }
        }
        trees.push(entries);
    }
    let commit = object::read_commit(&*a.db, a.pid, &head).expect("读 commit");
    assert_eq!(blobs.len(), 3, "三个文件三份清单：{blobs:?}");
    assert_eq!(trees.len(), 4, "root + 三个子目录：{}", trees.len());
    assert_eq!(blocks.len(), 3, "一份清单一块：{blocks:?}");

    let b_tmp = tempfile::tempdir().expect("对端站点");
    register(EXE, b_tmp.path(), PROJECT, "D:/peer-site-root");
    let mut child = Command::new(EXE)
        .args([
            "--root",
            &b_tmp.path().to_string_lossy(),
            "agent",
            "--project",
            PROJECT,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("起不了 agent 子进程");
    let mut w = child.stdin.take().expect("piped stdin");
    let mut r = child.stdout.take().expect("piped stdout");
    // stderr 要**在 wait 之前 take 走**：这一刀的报错文本就是本用例的断言对象——截断必须看得见。
    let mut err = child.stderr.take().expect("piped stderr");

    // 这里**不用闭包**包 `send`：闭包会一直握着 `&mut w`，后面 `write_segment(&mut w, ..)` 与
    // `w.flush()` 就借用冲突了（编译器说得对——两根写头本来就该串着用）。
    frame::encode(
        &mut w,
        &Proto::Hello {
            proto_ver: PROTO_VER,
            peer_id: b"crash2".to_vec(),
            caps: u32::MAX,
        },
    )
    .expect("发 Hello");
    expect_frame(frame::decode(&mut r), |p| matches!(p, Proto::Hello { .. }), "Hello");
    frame::encode(
        &mut w,
        &Proto::BeginSession {
            project: PROJECT.to_string(),
            pid: a.pid,
            my_heads: vec![head],
        },
    )
    .expect("发 BeginSession");
    expect_frame(
        frame::decode(&mut r),
        |p| matches!(p, Proto::HeadsState { .. }),
        "HeadsState",
    );
    frame::encode(
        &mut w,
        &Proto::WantHave {
            want: blocks.iter().map(|(h, _)| *h).collect(),
        },
    )
    .expect("发 WantHave");
    frame::encode(&mut w, &Proto::HaveDone).expect("发 HaveDone");
    let mut missing: Vec<keys::Hash> = Vec::new();
    loop {
        match frame::decode(&mut r).expect("读 MissingBlocks") {
            Proto::MissingBlocks { hashes } => missing.extend(hashes),
            Proto::MissingDone => break,
            other => panic!("该收 MissingBlocks/MissingDone，收到 {other:?}"),
        }
    }
    assert_eq!(missing.len(), 3, "空站当对端 ⇒ 三块全缺");

    let bytes: u64 = blocks.iter().map(|(_, s)| *s as u64).sum();
    frame::encode(&mut w, &Proto::PushBlocks { count: 3, bytes }).expect("发 PushBlocks");
    for (h, size) in &blocks {
        let body = a.cas().read_block(h, *size).expect("读本端块");
        frame::write_segment(&mut w, h, &body).expect("发块段");
    }
    // 对象也全发过去：四棵树 + 三份清单 + commit 本体，然后 `ObjectsDone`。
    frame::encode(
        &mut w,
        &Proto::PushObjects {
            blobs,
            trees,
            commits: vec![commit],
        },
    )
    .expect("发 PushObjects");
    frame::encode(&mut w, &Proto::ObjectsDone).expect("发 ObjectsDone");
    w.flush().expect("flush");
    // **掐在这里**：`VerifyCommit` 还没发出去。关写端 ⇒ 对端把管子里的帧全读完，然后在等
    // `VerifyCommit` 那一读上撞到管子到尾 ⇒ 它退出，而**动 head 那一步永远没执行**。
    drop(w);
    let status = child.wait().expect("等对端把管子读完");
    let mut log = String::new();
    err.read_to_string(&mut log).expect("读 agent 的 stderr");
    drop(r);
    assert!(
        !status.success(),
        "半路到尾的会话该让 agent 非零退出（那不是『管子干净到尾』的正常收尾），stderr = {log}"
    );
    assert!(
        log.contains("没有人引用") || log.contains("gc --sweep"),
        "截断要在 stderr 上说清楚'留下了什么、谁来收'，实际是：{log}"
    );

    let pid = keys::project_id(PROJECT);
    let b_db = open_store(b_tmp.path(), EngineKind::LsmTree).expect("崩过的库要能重开");
    let cas = Cas::new(&objects_dir(b_tmp.path()), cas::DEFAULT_BLOCK).expect("对端 CAS");
    assert!(
        build::ref_heads(&*b_db, pid, MAIN)
            .expect("读 head")
            .is_empty(),
        "对象齐了也不许动 head：`VerifyCommit` 是唯一那道闸"
    );
    assert!(
        object::read_commit(&*b_db, pid, &head).is_ok(),
        "这一刀的残留**就是**一整套对象：commit 行此刻确实在库里"
    );
    assert_eq!(count_blocks(&objects_dir(b_tmp.path())), 3, "三块都落了盘");
    let count = |tag: u8| -> usize {
        let mut p = vec![tag];
        p.extend_from_slice(&pid.to_be_bytes());
        let tx = b_db.begin();
        let n = tx.prefix_scan(&p).len();
        tx.commit();
        n
    };
    assert_eq!(count(keys::TAG_BLOB), 3, "`B` 行");
    assert_eq!(count(keys::TAG_TREE), 4, "`T` 行");
    assert_eq!(count(keys::TAG_COMMIT), 1, "`C` 行");
    assert_eq!(count(keys::TAG_BLOCK), 3, "`K` 行（adopt_blob 加过 refcnt）");

    // ── §9.3：这一整套没有任何 ref 可达 ⇒ sweep 该把它全收掉 ──
    let plan = gc::sweep(&*b_db, &cas, pid, false).expect("收崩残留");
    assert_eq!(plan.commits_kept, 0, "保留集是空的（没有任何 ref）：{plan:?}");
    assert_eq!(plan.commits_dropped, 1, "{plan:?}");
    assert_eq!(plan.trees_dropped, 4, "{plan:?}");
    assert_eq!(plan.blobs_dropped, 3, "{plan:?}");
    assert_eq!(plan.blocks_pending, 3, "三块全归零：{plan:?}");
    assert_eq!(
        count(keys::TAG_BLOCK),
        3,
        "sweep 只记账 ⇒ `K` 行还在，等第二趟"
    );
    let ul = gc::unlink_pending(&*b_db, &cas, pid, usize::MAX, false).expect("第二趟");
    assert_eq!(ul.files, 3, "{ul:?}");
    assert_eq!(gc::pending_total(&*b_db, pid).expect("读表"), (0, 0));
    drop(b_db);
    assert_eq!(count_blocks(&objects_dir(b_tmp.path())), 0, "GC 之后块仓该是空的");

    // ── 站点没被做废：一次完整 push 照样追平 ──
    let a_db = &*a.db;
    let mut io = agent_child(EXE, b_tmp.path(), PROJECT);
    let rep = session::push(&mut io, a_db, &a.cas(), &opts(PROJECT)).expect("GC 之后的一次完整 push");
    io.finish().expect("agent 该正常收尾");
    assert_eq!(rep.blocks_sent, 3, "块被 GC 删掉了 ⇒ 这次得重发：{rep:?}");
    let b_db = open_store(b_tmp.path(), EngineKind::LsmTree).expect("再开一次");
    assert_eq!(
        build::ref_heads(&*b_db, pid, MAIN).expect("读 head"),
        vec![head],
        "追平之后 head 到位"
    );
}

/// 把"这一帧必须是某一种"这件事写成一处：手工驱动协议的用例里，读错帧的报错要能看出**在等谁**。
fn expect_frame(got: TsResult<Proto>, ok: impl Fn(&Proto) -> bool, want: &str) {
    let p = got.unwrap_or_else(|e| panic!("该收到 {want}，读帧就失败了：{e}"));
    if !ok(&p) {
        panic!("该收到 {want}，收到 {p:?}");
    }
}

/// §9.5.1 TOFU 的三半：**首连入表**、**同一站点再连不报首连**、**换个站点冒充同一个名字就拒**。
/// 最后那一半要核的不只是"报错了"，还有"拒在建身份之前 ⇒ 对面一个字都没收到"。
#[test]
fn tofu_records_the_first_peer_and_refuses_a_changed_identity() {
    let a = Site::open();
    let files = seed(2);
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, b_)| (p.as_str(), b_.as_slice()))
        .collect();
    a.commit(&refs, 1);

    let b1 = tempfile::tempdir().expect("对端站点一");
    register(EXE, b1.path(), PROJECT, "D:/peer-site-root");
    let o = PushOpts {
        peer_name: b"peer-b",
        ..opts(PROJECT)
    };

    // ① 首连：成功，并报"这是第一次见"
    let mut io = agent_child(EXE, b1.path(), PROJECT);
    let rep = session::push(&mut io, &*a.db, &a.cas(), &o).expect("首连该成功");
    assert!(rep.first_contact, "首连必须报「第一次见」");
    io.finish().expect("agent 收尾");
    let rows = session::read_peers(&*a.db).expect("读 TOFU 表");
    assert_eq!(rows.len(), 1, "表里该只有一条：{rows:?}");
    assert_eq!(rows[0].0, "peer-b");
    assert_eq!(
        rows[0].1.site_id,
        site_id(b1.path()).expect("b1 的身份"),
        "记的就是对方站点自己的身份，不是它自报的字符串"
    );

    // ② 同一站点第二次连：身份一致 ⇒ 不再报首连
    let mut io = agent_child(EXE, b1.path(), PROJECT);
    let rep = session::push(&mut io, &*a.db, &a.cas(), &o).expect("第二次也该成功");
    assert!(!rep.first_contact, "同一站点不该再报首连");
    assert_eq!(rep.blocks_sent, 0, "没有新东西 ⇒ 一个块都不发");
    io.finish().expect("agent 收尾");

    // ③ **换个站点、同一个对端名字** ⇒ 拒。这就是 TOFU 与"信任一切"的差别。
    let b2 = tempfile::tempdir().expect("对端站点二");
    register(EXE, b2.path(), PROJECT, "D:/peer-site-root");
    let mut io = agent_child(EXE, b2.path(), PROJECT);
    let err = session::push(&mut io, &*a.db, &a.cas(), &o)
        .expect_err("名字相同、站点不同 ⇒ 必须拒，不是提示");
    assert!(err.to_string().contains("身份变了"), "报错要说清是身份变了：{err}");
    io.finish().expect("拒了也要把子进程收干净");

    // 拒的那一侧一个字都没落：会话在 `BeginSession` 之前就断了
    let b2_db = open_store(b2.path(), EngineKind::LsmTree).expect("开 b2");
    assert!(
        build::ref_heads(&*b2_db, keys::project_id(PROJECT), MAIN)
            .expect("读 head")
            .is_empty(),
        "对端不该有任何 head：项目名字都还没说出口"
    );
    assert_eq!(
        count_blocks(&objects_dir(b2.path())),
        0,
        "一个块都不许落进冒牌站点"
    );
    drop(b2_db);
    let rows = session::read_peers(&*a.db).expect("再读表");
    assert_eq!(
        rows[0].1.site_id,
        site_id(b1.path()).expect("b1"),
        "表不能被冒牌货覆盖：记的必须还是原来那个站点"
    );

    // ④ 对端确实重装过的正解：忘掉，然后重新确认（不是把历史当坏了）
    assert!(session::forget_peer(&*a.db, b"peer-b").expect("forget"));
    let mut io = agent_child(EXE, b2.path(), PROJECT);
    let rep = session::push(&mut io, &*a.db, &a.cas(), &o).expect("forget 之后该重新接受");
    assert!(rep.first_contact, "重新确认就该再报一次首连");
    io.finish().expect("agent 收尾");
}

/// §10.4 差分表里 **twosync 那一列的"传输字节"**。挂 `#[ignore]`：它要预先造好的语料，
/// 不是每次 `cargo test` 都该跑的测量。跑法：
///
/// ```text
/// node D:\ts10g\gendt.js 'D:\ts10g\dt\a\lab'                 # 512 MiB 大文件 + 200×250 KiB
/// twosync --root D:\ts10g\dt\a project add lab && scan && commit -m base
/// cargo test --test net_session -- --ignored --nocapture transfer_bytes_for_the_table
/// ```
///
/// 为什么这条放在测试里而不是命令行：本机没有 sshd（§11.0 那一格），`push` 起不了真管子。
/// 但**传输字节是协议层的读数**，与管子是 ssh 还是本地进程无关——这条走的正是
/// `Duplex::from_child` 那根真 OS 管道，所以它是实测，不是从 CAS 增量推算出来的。
#[test]
#[ignore = "要 D:/ts10g/dt 上预先造好的语料；§10.4 差分表专用"]
fn transfer_bytes_for_the_table() {
    // 正斜杠：Windows 的路径 API 认它，而写在字符串里不用转义（反斜杠在 Rust 源码里得写两遍）。
    let a = PathBuf::from(std::env::var("TS_DT_A").unwrap_or_else(|_| "D:/ts10g/dt/a".into()));
    let b = PathBuf::from(std::env::var("TS_DT_B").unwrap_or_else(|_| "D:/ts10g/dt/b".into()));
    let db = open_store(&a, EngineKind::LsmTree).expect("开 a 的库");
    let pid = keys::project_id(PROJECT_DT);
    let cas_a = Cas::new(&objects_dir(&a), cas::DEFAULT_BLOCK).expect("a 的 CAS");

    // ① 首推：对面是空站 ⇒ 这一次的字节数就是"全量"的参照
    let mut io = agent_child(EXE, &b, PROJECT_DT);
    let o = PushOpts { project: PROJECT_DT, peer_id: b"dt-a", peer_name: b"dt-b", ..Default::default() };
    let first = session::push(&mut io, &*db, &cas_a, &o).expect("首推");
    io.finish().expect("agent 收尾");
    println!(
        "DT_PUSH1 blocks={} bytes={} ({:.1} MiB) commits={}",
        first.blocks_sent,
        first.bytes_sent,
        first.bytes_sent as f64 / 1048576.0,
        first.commits
    );

    // ② 与 twosync/git 两臂**同一形状**的变更，再扫再提，然后第二次推。
    //
    // **尺子自检**：先读回那 4 个字节，确认写下去的确实与原来不同。差分表曾经栽过一次——
    // 第二臂跑在第一臂已经改过的语料上，git 于是"3.75 s 只涨 42 KiB"，看着像它的胜利，
    // 其实是实验被污染了（§10.4 的读数是 0 也要先证明变更落到了盘上）。
    let lab = a.join(PROJECT_DT);
    let off = 128 * 1024 * 1024;
    let mut big = fs::File::options().read(true).write(true).open(lab.join("big.bin")).expect("开 big.bin");
    use std::io::{Seek, SeekFrom};
    let mut before = [0u8; 4];
    big.seek(SeekFrom::Start(off)).expect("seek");
    big.read_exact(&mut before).expect("读回补丁区");
    let patch = vec![0x5cu8; 1024 * 1024];
    big.seek(SeekFrom::Start(off)).expect("seek back");
    big.write_all(&patch).expect("改 1 MiB");
    let mut after = [0u8; 4];
    big.seek(SeekFrom::Start(off)).expect("seek verify");
    big.read_exact(&mut after).expect("读回验证");
    drop(big);
    assert_ne!(before.to_vec(), after.to_vec(), "补丁没落到盘上 ⇒ 这一轮的'增量'是假的");
    let small_path = lab.join("many").join("d00").join("f0.bin");
    let mut sb = fs::read(&small_path).expect("读小文件");
    sb[0] = sb[0].wrapping_add(1);
    fs::write(&small_path, &sb).expect("写小文件");

    worktree::scan(&*db, pid, &lab, false, 2000).expect("scan");
    build::build(
        &*db,
        pid,
        &cas_a,
        &lab,
        Meta { peer: "dt", msg: "incr1mib", ts: 2, allow_mass_delete: true },
    )
    .expect("commit");

    let mut io = agent_child(EXE, &b, PROJECT_DT);
    let second = session::push(&mut io, &*db, &cas_a, &o).expect("第二推");
    io.finish().expect("agent 收尾");
    println!(
        "DT_PUSH2 blocks={} bytes={} ({:.1} MiB) commits={} skipped={}",
        second.blocks_sent,
        second.bytes_sent,
        second.bytes_sent as f64 / 1048576.0,
        second.commits,
        second.blocks_skipped
    );
}

/// §9.5.3 的接收端那一半：**注册了但没配对 ⇒ 拒**，而且一个字都不许落。
/// 这条存在的理由是"绝不自动在对端建目录"——一个打错的项目名如果只要注册过就收，
/// 就等于把整棵历史写进对方一个没准备过的格子里。
#[test]
fn the_agent_refuses_a_registered_but_unpaired_project() {
    let a = Site::open();
    let files = seed(2);
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, b_)| (p.as_str(), b_.as_slice()))
        .collect();
    a.commit(&refs, 1);

    let b = tempfile::tempdir().expect("对端站点");
    add_project(EXE, b.path(), PROJECT); // 只注册，不 pair
    let mut io = agent_child(EXE, b.path(), PROJECT);
    let err = session::push(&mut io, &*a.db, &a.cas(), &opts(PROJECT))
        .expect_err("没配对就该拒，不是收下来再说");
    let msg = err.to_string();
    assert!(
        msg.contains("没配对") || msg.contains("project pair"),
        "报错要给出一条能走的路，而不是'协议走样'：{msg}"
    );
    // 项目级拒绝（没注册 / 没配对）让 agent **非零退出**，而 commit 级拒绝（FR-10 熔断）是正常
    // 收尾、退出码 0。这个区别是故意的：前者是"我连这个项目都不谈"，后者是"谈了，但你这一版我不收"。
    let done = io.finish();
    assert!(
        matches!(&done, Err(e) if e.to_string().contains("退出码")),
        "项目级拒绝要让发起端看见非零收尾：{done:?}"
    );

    let b_db = open_store(b.path(), EngineKind::LsmTree).expect("开对端的库");
    assert!(
        build::ref_heads(&*b_db, keys::project_id(PROJECT), MAIN)
            .expect("读 head")
            .is_empty(),
        "被拒的会话不许动 head"
    );
    assert_eq!(
        count_blocks(&objects_dir(b.path())),
        0,
        "也不许落一个块：拒绝发生在开场，对象一个都没发"
    );
    drop(b_db);

    // 配上对之后，同一份历史就该收得下了（说明刚才那一刀是配对挡的，不是别的东西）
    let out = Command::new(EXE)
        .args([
            "--root",
            &b.path().to_string_lossy(),
            "project",
            "pair",
            PROJECT,
            "D:/peer-site-root",
        ])
        .output()
        .expect("起不了 project pair");
    assert!(out.status.success(), "pair 失败：{}", String::from_utf8_lossy(&out.stderr));
    let mut io = agent_child(EXE, b.path(), PROJECT);
    let rep = session::push(&mut io, &*a.db, &a.cas(), &opts(PROJECT))
        .expect("配对之后同一份历史就该收下");
    io.finish().expect("agent 收尾");
    assert!(rep.bytes_sent > 0, "这一把是真的传了东西");
    let b_db = open_store(b.path(), EngineKind::LsmTree).expect("再开对端的库");
    assert_eq!(
        build::all_ref_heads(&*b_db, keys::project_id(PROJECT), None).expect("读 head"),
        a.heads(),
        "配上对之后 head 追平了"
    );
}

/// 套在管子外面的字节计数器。**为什么要有这么一层**：判据问的是"传输量"，而
/// `Report.bytes_sent` 只累加**块段的裸字节**（`session.rs` 里那句 `rep.bytes_sent += body.len()`），
/// 协商帧（`WantHave`/`MissingBlocks`）、清单帧与 commit 帧都不在它里面。12 GiB 站点上这部分只有
/// 几十 KiB 量级，但"大概只有"不能写进表 ⇒ 这一条自己数总字节，两个数一起报。
struct Wire {
    io: m1::Duplex,
    out: u64,
}

impl Read for Wire {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.io.read(buf)
    }
}

impl Write for Wire {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.io.write(buf)?;
        self.out += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.io.flush()
    }
}

impl Wire {
    /// 收尾并交出累计写出的字节。`Duplex::finish` 要**拿走**管子（它得关掉写端等子进程退出），
    /// 所以计数只能在收尾这一步一起交出来。
    fn finish(self) -> TsResult<u64> {
        let out = self.out;
        self.io.finish()?;
        Ok(out)
    }
}

/// §11 的 M1 判据里那一格 **"10 GB 文件改 1 MB ⇒ 传输 ≤ 8 MB"的量级读数**（机制在同机 6 文件那把
/// 上早就过了：改 1 字节 ⇒ 传 1 块；这里要的是 12 GiB 站点上的绝对数 + 协商开销不随规模爆掉的证据）。
/// 挂 `#[ignore]`：它要一个 12 GiB 的站点与一个已经追平到基线的对端，setup 见下面。
///
/// # 站点是怎么搭起来的（这一步的口径必须写清楚，否则读数会被误读成"首建库很快"）
///
/// 对端 `D:/ts10p` **不是经会话填出来的**，而是：
/// - `.twosync/objects` 用 `cp -al` **硬链**到本端的块文件（21 794 个，一个字节都不多占盘；
///   CAS 是写一次不改的，所以共享 inode 安全 —— 收到新块是"新建文件"，不会动到本端那份）；
/// - `.twosync/db` 冷拷贝（26 MiB）⇒ 对端的 head 与全部 `B`/`T`/`K` 账就是本端的基线；
/// - 然后两边各跑一次 `project pair`（§9.5.3）。
///
/// ⇒ **这一格测的是"增量那一趟上线多少字节"，不含首份全量**；首份全量的代价在 §11.1 与 §11.2 里另有读数。
///
/// # 两臂故意差在块边界上（判据的上限就是照这个形状写的）
///
/// | 臂 | 改动 | 脏块 | 期望上线 |
/// |---|---|---|---|
/// | aligned | 1 MiB 全落在同一个 4 MiB 块内 | 1 | 4 MiB + 帧 |
/// | cross | 1 MiB 跨两个 4 MiB 块（各沾 512 KiB） | 2 | 8 MiB + 帧 ⇒ **正好贴着上限** |
///
/// # 两个自己证明对照做成了（§10.4 栽过一次：变更没落盘就把"增量"读成 0）
///
/// ① `blocks_skipped` 必须远大于 0 ⇒ 对端**真的已经**有其余那些块，否则这一推就是全量；
/// ② 会话之后对端的 head 必须前移、根 tree 与本端一致 ⇒ "传到了"而不是"传了没生效"。
///
/// 跑法（变更与提交都在命令行上做：那两腿是 35 s / 71 s 量级，失败时要能单独重跑，不该塞进测量用例）：
///
/// ```text
/// node D:\ts10g\mut10g.js D:\ts10g\lab aligned
/// twosync --root D:/ts10g --project lab scan
/// twosync --root D:/ts10g --project lab commit -m arm-aligned
/// TS_BIG_CEIL_MIB=5 cargo test --test net_session -- --ignored --nocapture incremental_transfer_at_ten_gib
/// ```
#[test]
#[ignore = "要 D:/ts10g（12 GiB 站点）与 D:/ts10p（对端：块仓硬链 + db 冷拷贝到基线）；§11 的增量传输量级读数专用"]
fn incremental_transfer_at_ten_gib() {
    let a = PathBuf::from(std::env::var("TS_BIG_A").unwrap_or_else(|_| "D:/ts10g".into()));
    let b = PathBuf::from(std::env::var("TS_BIG_B").unwrap_or_else(|_| "D:/ts10p".into()));
    let name = std::env::var("TS_BIG_PROJ").unwrap_or_else(|_| "lab".to_string());
    let ceil_mib: f64 = std::env::var("TS_BIG_CEIL_MIB")
        .ok()
        .map(|v| v.parse().expect("TS_BIG_CEIL_MIB 要是一个数"))
        .unwrap_or(8.0);

    let pid = keys::project_id(&name);
    let t_start = std::time::Instant::now();
    let db = open_store(&a, EngineKind::LsmTree).expect("开本端的库");
    let cas_a = Cas::new(&objects_dir(&a), cas::DEFAULT_BLOCK).expect("本端 CAS");
    let a_heads = build::all_ref_heads(&*db, pid, None).expect("本端 head");
    assert!(!a_heads.is_empty(), "本端没有 head ⇒ 这一格没得测");

    // 对端在推之前的样子：块数与 head（推之后拿这两个数做对照）
    let t_heads = t_start.elapsed().as_secs_f64();
    let blocks_before = count_blocks(&objects_dir(&b));
    let t_count0 = t_start.elapsed().as_secs_f64();
    let b_db0 = open_store(&b, EngineKind::LsmTree).expect("开对端的库");
    let b_heads_before = build::all_ref_heads(&*b_db0, pid, None).expect("对端 head");
    drop(b_db0);
    let t_setup = t_start.elapsed().as_secs_f64();
    println!(
        "TEN_GIB 相位：开本端库+读 head {t_heads:.1} s / 数对端 21 794 个块文件 {:.1} s / \
         开对端库+读 head {:.1} s ⇒ 会话之前合计 {t_setup:.1} s",
        t_count0 - t_heads,
        t_setup - t_count0
    );

    let mut wire = Wire { io: agent_child(EXE, &b, &name), out: 0 };
    let o = PushOpts {
        project: &name,
        peer_id: b"ten-gib-site",
        peer_name: b"ten-gib-peer",
        ..Default::default()
    };
    // 相位计时（§15.22 的第一版归因被臂 4 的实测推翻了，见补记廿四 ⇒ 这次先量再说话）。
    // 特别注意 `count_blocks`：它要在 5400 转的盘上走 21 794 个块文件**两遍**，本身就可能比会话还贵。
    use std::time::Instant;
    let t0 = Instant::now();
    let rep = session::push(&mut wire, &*db, &cas_a, &o).expect("增量推送");
    let t_push = t0.elapsed();
    let wire_bytes = wire.finish().expect("agent 收尾");
    let t_child = t0.elapsed();
    let blocks_after = count_blocks(&objects_dir(&b));
    let t_count = t0.elapsed();

    println!(
        "TEN_GIB blocks_sent={} skipped={} blob帧={} tree帧={} commits={}\n\
         TEN_GIB 块裸字节={} B ({:.2} MiB)\n\
         TEN_GIB 管子总字节={} B ({:.2} MiB)  ← 含协商/清单/commit 帧\n\
         TEN_GIB 相位：push={:.1} s（含等子进程退出 {}） / 之后再数盘 {:.1} s；开库+读 head+第一次数盘 之前已花 {:.1} s",
        rep.blocks_sent,
        rep.blocks_skipped,
        rep.blobs_sent,
        rep.trees_sent,
        rep.commits,
        rep.bytes_sent,
        rep.bytes_sent as f64 / 1048576.0,
        wire_bytes,
        wire_bytes as f64 / 1048576.0,
        t_push.as_secs_f64(),
        (t_child - t_push).as_secs_f64(),
        (t_count - t_child).as_secs_f64(),
        t_setup,
    );

    // ── 对照 ①：对端不是空的，否则"增量"是假的 ──
    // `commits == 0` 是合法的跑法：**没有新东西**时这一条就变成"固定成本探针"（量具自己那部分开销），
    // 臂与臂之间要比的正是这个固定成本，所以不许改语料也能单独跑。
    if rep.commits == 0 {
        assert_eq!(rep.bytes_sent, 0, "没有待推的 commit ⇒ 一个字节都不该发");
        assert_eq!(blocks_after, blocks_before, "没有待推的 commit ⇒ 对端一个文件都不该多");
        return;
    }
    assert!(rep.blocks_skipped >= 1000, "对端应该已经有其余那些块：{rep:?}");
    assert!(rep.blocks_sent >= 1, "脏块一个都没发 ⇒ 这一刀没落到盘上：{rep:?}");
    assert!(
        (rep.bytes_sent as f64) <= ceil_mib * 1048576.0,
        "块裸字节 {:.2} MiB 超过上限 {ceil_mib} MiB",
        rep.bytes_sent as f64 / 1048576.0
    );
    assert!(
        (wire_bytes as f64) <= ceil_mib * 1048576.0 + 1048576.0,
        "管子总字节 {:.2} MiB 超了（上限 {ceil_mib} MiB + 1 MiB 帧开销）",
        wire_bytes as f64 / 1048576.0
    );

    // ── 对照 ②：推过去了、也生效了 ──
    assert_eq!(
        blocks_after,
        blocks_before + rep.blocks_sent,
        "对端的块数应该正好长出这么多"
    );
    let b_db = open_store(&b, EngineKind::LsmTree).expect("推完再开对端的库");
    let b_heads = build::all_ref_heads(&*b_db, pid, None).expect("对端 head");
    assert_ne!(b_heads, b_heads_before, "对端 head 必须前移");
    assert_eq!(b_heads, a_heads, "两边 head 应该追平");
    for h in &a_heads {
        let ta = object::read_commit(&*db, pid, h).expect("本端 commit").tree;
        let tb = object::read_commit(&*b_db, pid, h).expect("对端 commit").tree;
        assert_eq!(ta, tb, "根 tree 必须一致（{h:?}）");
    }
}

/// 差集那道闸**仍然**必须拒"谎报的引用"：新树里某个条目既不是这一场收到的、也没有亲代可继承 ⇒ 悬空。
/// 这条用例是 §15.22 那笔交换的另一半——省的是"未变的部分不必重读"，**不是**把校验关掉。
///
/// 写法上是手工驱动协议：要用一个 `push` 永远不会构造出来的对象（引用一个哪里都不存在的 blob hash），
/// 所以绕开发送端，直接对着 `session::serve` 说话。
#[test]
fn a_dangling_reference_in_a_delta_push_is_still_refused() {
    let a = Site::open();
    let b = Site::open();

    // 伪造：一棵引用"不存在 blob"的树，加一个指向它的 commit。写进 a 的库只是为了拿到两个 hash。
    let ghost: keys::Hash = [0xEE; 32];
    let entries = vec![object::TreeEntry {
        name: "x.bin".into(),
        kind: keys::Kind::File,
        hash: ghost,
        size: 10,
        mode: 0o644,
    }];
    let tree = object::put_tree(&*a.db, a.pid, &entries).expect("伪造 tree");
    let forged = object::Commit {
        parents: Vec::new(),
        tree,
        peer: "a".to_string(),
        ts: 9,
        msg: "谎报的引用".to_string(),
        stats: object::Stats { add: 1, modify: 0, delete: 0, rename: 0, bytes: 10 },
    };
    let fake = object::put_commit(&*a.db, a.pid, &forged).expect("伪造 commit");

    let (mut pa, mut pb) = duplex();
    let cas_b = b.cas();
    let served = std::thread::scope(|sc| {
        let h = sc.spawn(|| {
            let mut resolve = |_n: &str| Ok::<u32, String>(b.pid);
            session::serve(&mut pb, &*b.db, &cas_b, b"peer-b", 0xFFFF_FFFF, &mut resolve, false)
        });
        frame::encode(
            &mut pa,
            &Proto::Hello { proto_ver: PROTO_VER, peer_id: b"liar".to_vec(), caps: u32::MAX },
        )
        .expect("发 Hello");
        expect_frame(frame::decode(&mut pa), |p| matches!(p, Proto::Hello { .. }), "Hello");
        frame::encode(
            &mut pa,
            &Proto::BeginSession { project: PROJECT.to_string(), pid: a.pid, my_heads: vec![fake] },
        )
        .expect("发 BeginSession");
        expect_frame(
            frame::decode(&mut pa),
            |p| matches!(p, Proto::HeadsState { .. }),
            "HeadsState",
        );
        frame::encode(&mut pa, &Proto::WantHave { want: Vec::new() }).expect("发 WantHave");
        frame::encode(&mut pa, &Proto::HaveDone).expect("发 HaveDone");
        loop {
            match frame::decode(&mut pa).expect("读 MissingBlocks") {
                Proto::MissingBlocks { .. } => {}
                Proto::MissingDone => break,
                other => panic!("该收 MissingBlocks/MissingDone，收到 {other:?}"),
            }
        }
        // 只给清单：那个 blob 与它的块都故意不推
        frame::encode(
            &mut pa,
            &Proto::PushObjects { blobs: Vec::new(), trees: vec![entries], commits: vec![forged] },
        )
        .expect("发 PushObjects");
        frame::encode(&mut pa, &Proto::ObjectsDone).expect("发 ObjectsDone");
        frame::encode(&mut pa, &Proto::VerifyCommit { hash: fake }).expect("发 VerifyCommit");
        let rep = match frame::decode(&mut pa).expect("读 verdict") {
            p @ Proto::RejectCommit { .. } => {
                let Proto::RejectCommit { reason, .. } = p else { unreachable!() };
                assert!(
                    reason.contains("没收到它") || reason.contains("与上代不同"),
                    "拒绝的理由要说清是悬空引用，实际：{reason}"
                );
                h.join().expect("serve 线程")
            }
            other => panic!("该收 RejectCommit，收到 {other:?}"),
        };
        drop(pa);
        rep
    })
    .expect("serve 该正常返回（拒绝是判决，不是会话故障）");
    assert!(!served.accepted, "谎报的引用必须被拒：{}", served.reason);

    // 而且 head 一步没动：清单落进了库（那是等 GC 的垃圾），但没有"生效"这件事。
    // 用 `b.db` 这个**已经开着的**句柄读，不要再 `open_store` 一次——引擎的库级文件锁连同进程双开
    // 都拒（`WouldBlock`，见 `tests/gc_sweep.rs` 那条跨进程重开的用例）。
    assert!(
        served.heads_after.is_empty(),
        "被拒的会话报出来的 head 集合该是空的：{:?}",
        served.heads_after
    );
    assert!(
        build::all_ref_heads(&*b.db, b.pid, None)
            .expect("读 head")
            .is_empty(),
        "谎报的引用不许动 head"
    );
}

/// §15.22 那笔交换的**账单**，写成用例而不是只写进文档：把差集校验之后，"某个**未变的**块在对端烂了"
/// 这一种磁盘腐烂**不再被 push 那道闸抓到**（它本来就不是协议错误：这次没推它，亲代也是这么指的）。
/// 但读路径必须抓到，而且不能静默给半截内容（§9.4.7 规矩 3）。
///
/// 这条用例存在的意义是：**不许把"推送变快了"读成"全仓库不再有损坏风险"**——风险换了看守，没消失。
#[test]
fn an_inherited_rot_slips_the_push_gate_and_is_caught_on_read() {
    let a = Site::open();
    let b = Site::open();
    let files = seed(2);
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, b_)| (p.as_str(), b_.as_slice()))
        .collect();
    a.commit(&refs, 1);
    round_ok(&a, &b, &opts(PROJECT));

    // 对端第一个文件的块：这一刀之后**不会**被重发（内容没变），所以先把它"弄烂"。
    // 读对端的账用**已经开着的那个句柄**（`b.db`）：引擎是库级文件锁，同进程再 open 一次会 `WouldBlock`。
    let head0 = *build::ref_heads(&*b.db, b.pid, MAIN)
        .expect("b 的 head")
        .first()
        .expect("有 head");
    let tree0 = object::read_commit(&*b.db, b.pid, &head0).expect("读 commit").tree;
    let e0 = history::resolve(&*b.db, b.pid, &tree0, "d0/f0.bin")
        .expect("resolve")
        .expect("f0 在树里");
    let blob0 = build::read_blob(&*a.db, a.pid, &e0.hash).expect("读本端同一个 blob");
    let victim_block = blob0.blocks.iter().find(|b| !b.zero).expect("至少一块").hash;
    let cas_b = b.cas();
    let p = cas_b.dir().join(Cas::path_of(&victim_block));
    fs::remove_file(&p).expect("手工抹掉对端那个未变的块（模拟腐烂）");

    // 本端只改**第二个**文件再推 ⇒ 差集里 f0 全部继承 ⇒ 这道闸放行了（这就是被挪走的那半担保）
    let second = a.proj().join("d1").join("f1.bin");
    let mut body2 = fs::read(&second).expect("读 f1");
    body2[7] ^= 0xFF;
    fs::write(&second, &body2).expect("改 f1");
    worktree::scan(&*a.db, a.pid, &a.proj(), false, 500).expect("scan");
    let store = a.cas();
    build::build(
        &*a.db,
        a.pid,
        &store,
        &a.proj(),
        Meta { peer: "a", msg: "只改 f1", ts: 2, allow_mass_delete: true },
    )
    .expect("增量提交");
    let (rep, served) = round_ok(&a, &b, &opts(PROJECT));
    assert!(served.accepted, "腐烂的是未变的那个文件 ⇒ 差集校验放行，这是 §15.22 明写的取舍：{}", served.reason);
    assert_eq!(rep.blocks_sent, 1, "只推 f1 那一块：{rep:?}");

    // 但读路径必须抓到，而且是报错不是静默
    let err = cas_b
        .read_blob(&blob0, &b.tmp.path().join("out.bin"))
        .expect_err("盘上少了块 ⇒ 读它必须报错，不许给半截内容");
    let msg = err.to_string();
    assert!(
        msg.contains("块") || msg.contains("block"),
        "报错要指到块这一层，实际：{msg}"
    );
}

/// 把门面上"一次 push 折了几遍全库"钉成**整数断言**（§17 补记廿六）。
///
/// 为什么必须有这把尺子：本机墙钟 run 间是 ±2×（§11.3 里同形状的臂 3 = 226 s、臂 4 = 327 s），
/// 所以"我把它变快了"这种话不能拿计时当证据。而**折全库的次数是整数**，可以断言、可以回归。
///
/// 这条断言的是**发起端与仓库大小无关**——那正是差集推送（补记廿三）的全部意义：
/// 未变的子树不收集 ⇒ 不读 ⇒ 不折。收端那一侧此刻**还在**随目录数涨（`count_entries` 与
/// `build::diff` 按层读），所以这里只钉发起端，并把两边的数都印出来当 §15.25 的读数。
///
/// 跑法：`cargo test --test net_session -- --nocapture the_push_fold_count`
#[test]
fn the_push_fold_count_does_not_grow_with_the_repo_on_the_sending_side() {
    // 两个规模：3 个条目与 24 个条目（同形状：每条目一个自己的目录）。
    // 差集成立 ⇒ 发起端的折数**一模一样**；差集不成立，24 那把会多出二十几次点读。
    let mut by_n: Vec<(usize, utils::CallCounts, utils::CallCounts)> = Vec::new();
    for n in [3usize, 24usize] {
        let (sent, recv) = incremental_push_counts(n);
        println!(
            "PUSH_FOLDS n={n} 发起端：{} | 收端：{}",
            sent.line(),
            recv.line()
        );
        by_n.push((n, sent, recv));
    }

    let small = &by_n[0].1;
    let big = &by_n[1].1;
    assert_eq!(
        small.folds(),
        big.folds(),
        "发起端折了 {} vs {} 遍：差集退化成了按条目数折（补记廿三那条修复没了）",
        small.folds(),
        big.folds()
    );
    // 绝对上界：这一趟真该做的事只有"读两份 head + 三份可达闭包(各一次装索引) + 差集那几棵变化的树"。
    // 留了余量，但**不许悄悄涨回几十倍** —— 24 条目的站点都不该超过这个数。
    assert!(small.folds() <= 30, "发起端折了 {} 遍，比设计值宽：{small:?}", small.folds());

    // 收端此刻仍然随规模涨：**这是已知成本，不是这条用例要修的东西**（§15.25），
    // 但把它钉成一条会失败的断言比不写强 —— 哪天有人把它修了，这条断言会红，逼他改注释。
    assert!(
        by_n[1].2.folds() > by_n[0].2.folds(),
        "收端折数没有随条目数涨？那 §15.25 那条已知成本已经被修掉了，来更新本节"
    );
}

/// 建一个 `n` 条目的站点、推一版、改掉**其中一个**文件再推一版，返回**第二推**两边的账。
fn incremental_push_counts(n: usize) -> (utils::CallCounts, utils::CallCounts) {
    let ma = utils::Meter::default();
    let mb = utils::Meter::default();
    let a = Site::metered(&ma);
    let b = Site::metered(&mb);
    let files = seed(n);
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, b_)| (p.as_str(), b_.as_slice()))
        .collect();
    a.commit(&refs, 1);
    round_ok(&a, &b, &opts(PROJECT)); // 首推：整仓库都在这儿，不计入下面的差集账

    let victim = a.proj().join("d1").join("f1.bin");
    let mut body = fs::read(&victim).expect("读原文件");
    body[9] ^= 0xFF;
    fs::write(&victim, &body).expect("改一个字节");
    worktree::scan(&*a.db, a.pid, &a.proj(), false, 500).expect("scan");
    let store = a.cas();
    build::build(
        &*a.db,
        a.pid,
        &store,
        &a.proj(),
        Meta {
            peer: "a",
            msg: "改一个字节",
            ts: 2,
            allow_mass_delete: true,
        },
    )
    .expect("增量提交");

    let before_a = ma.snapshot();
    let before_b = mb.snapshot();
    let (rep, _served) = round_ok(&a, &b, &opts(PROJECT));
    assert_eq!(rep.blocks_sent, 1, "这一推就该上线一个脏块：{rep:?}");
    assert_eq!(rep.blobs_sent, 1, "只有一份清单变了：{rep:?}");
    (
        ma.snapshot().since(before_a),
        mb.snapshot().since(before_b),
    )
}

// ── `pull`：同一套帧，反方向（M2，§16.3"拨号方向对称"）────────────────────────
//
// 这一组用例盯的是"换边之后闸还在不在"：`push` 那一侧验过的每一件事（闭合性、FR-10、
// 配额、空集合必须是 0 字节），在 pull 里都必须由**另一边**重新成立一次。

/// 反方向的一轮：一头 `pull`（本端收），另一头 `serve`（对面变成发送方）。
/// 与 [`round`] 一样必须两条线程——会话是"说一句等一句"的形状。
fn round_pull(
    puller: &Site,
    sender: &Site,
    opts: &PushOpts<'_>,
) -> (Result<ServeReport, Error>, Result<ServeReport, Error>) {
    let (mut pa, mut pb) = duplex();
    std::thread::scope(|sc| {
        let h = sc.spawn(|| {
            let cas_s = sender.cas();
            let mut resolve = |_n: &str| Ok::<u32, String>(sender.pid);
            session::serve(
                &mut pb,
                &*sender.db,
                &cas_s,
                b"peer-sender",
                0xFFFF_FFFF,
                &mut resolve,
                false,
            )
        });
        let cas_p = puller.cas();
        let got = session::pull(&mut pa, &*puller.db, &cas_p, opts);
        drop(pa);
        let served = match h.join() {
            Ok(r) => r,
            Err(_) => Err(Error::BadInput("serve 线程 panic（见上面的 panic 输出）".into())),
        };
        (got, served)
    })
}

#[test]
fn a_pull_brings_the_history_to_a_site_with_no_head() {
    // 新机器/重装过的站点：**本端一个 head 都没有**，而"两边各 push 一次"在这种形状里
    // 有一边根本推不动（发起方自己没 head 就直接报错）。这一条演的正是那个缺口。
    let a = Site::open();
    let b = Site::open();
    let files = seed(4);
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, body)| (p.as_str(), body.as_slice()))
        .collect();
    a.commit(&refs, 1);
    assert!(b.heads().is_empty(), "台架前提：B 还没有任何历史");

    let (got, served) = round_pull(&b, &a, &opts(PROJECT));
    let got = got.expect("空站点也该拉得动");
    let served = served.expect("对面（发送方）该成功");

    assert!(got.accepted, "本端验收：{}", got.reason);
    assert_eq!(got.blobs_in, 4, "4 份清单落到本端");
    assert!(got.blocks_written > 0, "块必须真的落盘");
    assert_eq!(
        b.heads(),
        a.heads(),
        "拉完之后两边的 head 集合要一模一样（差一个字节都不行）"
    );
    assert_eq!(b.block_count(), a.block_count(), "本端 CAS 的块数该对得上对面");
    // 对面那份账必须记成"我是发送方"，而不是伪装成一场收东西的会话。
    let sent = served.sent.expect("对面是发送方 ⇒ sent 必须有值");
    assert_eq!(sent.commits, 1, "它推了一个 commit：{sent:?}");
    assert!(sent.blocks_sent > 0);
    assert!(served.heads_after == a.heads(), "发送方的 head 集合不因发送而改变");
}

#[test]
fn pull_then_pull_converges_two_independently_committed_sites() {
    // §7 的 union 那一行，走 pull 这条路：两边各有一笔**互不相干**的提交，
    // 互相拉一次之后两边的 head 集合必须相等，而且**两笔历史都还在**（谁也没被覆盖掉）。
    let a = Site::open();
    let b = Site::open();
    a.commit(&[("x0/f.bin", b"the content on side A")], 1);
    b.commit(&[("y0/f.bin", b"the content on side B")], 2);
    let (ha, hb) = (a.heads(), b.heads());
    assert_ne!(ha, hb, "台架前提：两边各有各的 head");

    // B 拉 A：B 收下 A 的历史，自己的那一笔不动 ⇒ union。
    let (got, _) = round_pull(&b, &a, &opts(PROJECT));
    got.expect("第一次 pull");
    let after_first = b.heads();
    assert_eq!(after_first.len(), 2, "B 应该同时持有自己那一笔与 A 那一笔：{after_first:?}");
    assert!(after_first.contains(&ha[0]) && after_first.contains(&hb[0]));

    // A 拉 B：A 收到 B 那一笔（以及它自己那笔的孙——B 那边已经合并过的 head 集合）。
    let (got, _) = round_pull(&a, &b, &opts(PROJECT));
    got.expect("第二次 pull");
    assert_eq!(
        a.heads(),
        b.heads(),
        "互相拉一次之后两边的 head 集合必须相等（§7 union 的可核对形状）"
    );
    assert!(a.heads().contains(&ha[0]) && a.heads().contains(&hb[0]), "两笔历史都还在：{:#?}", a.heads());
    // 内容两边都能取回：拉过来的不是"只有 head 的空壳"。
    assert_eq!(a.block_count(), b.block_count(), "块数也该一致");
}

#[test]
fn pulling_from_a_site_with_nothing_to_send_is_refused_with_a_reason() {
    // 对面是个空站点 ⇒ 它必须**明确说**"没有东西可发"，而不是让本端等到管子到尾。
    // 常驻循环第一天的常态就是这个形状（所以 `daemon` 里 pull 失败不进退避曲线）。
    let a = Site::open();
    let b = Site::open();
    a.commit(&[("x0/f.bin", b"only side A has this")], 1);
    let before = a.heads();
    let err = round_pull(&a, &b, &opts(PROJECT))
        .0
        .expect_err("对面没有 head ⇒ 该被拒");
    let msg = err.to_string();
    assert!(
        msg.contains("还没有任何 head"),
        "报错要说清是对面空，实际：{msg}"
    );
    assert_eq!(a.heads(), before, "被拒的一轮不许动本端 head");
}

/// 真子进程那一条路也必须能反着走：对面是**真的 `twosync agent` 进程**，本端 `pull`。
///
/// 为什么值得再起一次进程：`Side::Send` 那半是这次新加的分支，它在真管子上的收尾形状
/// （发送完之后 agent 读到 EOF、退出码 0）只有跨进程才验得到——内存管道里 `drop(pa)`
/// 就能造出尾，而 agent 那边是 `stdin` 被关闭。
#[test]
fn pull_over_a_real_child_process_and_pipes() {
    let peer_tmp = tempfile::tempdir().expect("对面站点");
    register(EXE, peer_tmp.path(), PROJECT, "D:/peer-site-root");
    // 对面的内容用**真命令**造（scan + commit）：这份历史不是我们手搓的对象，
    // 才是"从一个装了 twosync 的站点把历史拉回来"。
    for i in 0..3 {
        let p = peer_tmp
            .path()
            .join(PROJECT)
            .join(format!("d{i}"))
            .join(format!("f{i}.bin"));
        fs::create_dir_all(p.parent().expect("父目录")).expect("mkdir");
        fs::write(&p, body(i, 120_000)).expect("写文件");
    }
    for sub in ["scan", "commit"] {
        let out = Command::new(EXE)
            .args([
                "--root",
                &peer_tmp.path().to_string_lossy(),
                "--project",
                PROJECT,
                sub,
            ])
            .output()
            .expect("起不了对面的子命令");
        assert!(
            out.status.success(),
            "对面 {sub} 失败：{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // 本端：全新站点，一个 head 都没有。
    let puller = Site::open();
    assert!(puller.heads().is_empty(), "台架前提：本端还没有历史");
    let mut io = agent_child(EXE, peer_tmp.path(), PROJECT);
    let rep = session::pull(&mut io, &*puller.db, &puller.cas(), &opts(PROJECT))
        .expect("真管道上的一次 pull 该成功");
    io.finish()
        .expect("关写端之后 agent（那一侧是发送方）该读到尾、退出码 0");

    assert!(rep.accepted, "本端该验收：{}", rep.reason);
    assert_eq!(rep.blobs_in, 3, "3 份清单落到本端");
    assert!(rep.blocks_written >= 3, "块要真的落盘：{}", rep.blocks_written);
    assert_eq!(puller.heads().len(), 1, "本端 head 该恰好一个");
    assert_eq!(puller.block_count(), 3, "本端 CAS 里该有那 3 个块");
    // 拉回来的不是"只有一个 head 的空壳"：内容逐字节要对得上对面写的东西。
    let got = object::read_commit(&*puller.db, puller.pid, &puller.heads()[0]).expect("读 commit");
    let list = object::read_tree(&*puller.db, puller.pid, &got.tree).expect("读根清单");
    assert_eq!(list.len(), 3, "根上三个目录：{list:?}");
    for (i, e) in list.iter().enumerate() {
        assert_eq!(e.name, format!("d{i}"), "目录名对得上：{e:?}");
        assert_eq!(e.kind, keys::Kind::Dir);
        let inner = object::read_tree(&*puller.db, puller.pid, &e.hash).expect("读子层");
        assert_eq!(inner.len(), 1, "每层一个文件");
        assert_eq!(inner[0].size, 120_000, "尺寸是从盘上算出来的，不是听来的");
    }
}

#[test]
fn the_puller_declares_the_quota_and_the_sender_honors_it() {
    // §9.4.5 换了方向也必须成立：配额由**收货的一方**声明（`PullRequest.max_bytes`），
    // 由发送方自己算清单并自我了断 —— 一个字都不发，而不是发一半。
    let a = Site::open();
    let b = Site::open();
    let files = seed(4); // 4 × 300 000 B，远大于这里给的 1 字节上限
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, body)| (p.as_str(), body.as_slice()))
        .collect();
    a.commit(&refs, 1);

    let tight = PushOpts {
        project: PROJECT,
        max_block_bytes: 1,
        ..Default::default()
    };
    let (got, _) = round_pull(&b, &a, &tight);
    let err = got.expect_err("超出本端声明的配额 ⇒ 该失败");
    assert!(
        err.to_string().contains("配额"),
        "要看得懂是哪道闸拦的，实际：{err}"
    );
    assert!(b.heads().is_empty(), "被配额拦住的这一轮不许留任何 head");
    assert_eq!(b.block_count(), 0, "一个字都没收：这才是 Abort 的意思");
}
