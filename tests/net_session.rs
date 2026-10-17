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
use twosync::store::history;
use twosync::store::keys;
use twosync::store::object;
use twosync::store::worktree;
use twosync::utils::{objects_dir, open_store};

const PROJECT: &str = "shared";

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
            let mut resolve = |_n: &str| Some(b.pid);
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
            let mut resolve = |_n: &str| Some(b.pid);
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
    assert_eq!(
        rep.blocks_skipped, 5,
        "新 commit 一共引用 6 个块，对端已经有另外 5 个 ⇒ 这才是去重的读数（协商回来的账）"
    );
    assert_eq!(
        served.bytes_in,
        rep.bytes_sent,
        "两边对账：发出去的字节 == 落进对端仓的字节"
    );
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
    let b_root = b_tmp.path().to_str().expect("utf-8 路径").to_string();
    let exe = env!("CARGO_BIN_EXE_twosync");

    // 对端先注册项目：`agent` 的白名单就是注册表（没注册的项目直接 Abort，§9.5）
    fs::create_dir_all(b_tmp.path().join(PROJECT)).expect("对端项目目录");
    let reg = Command::new(exe)
        .args(["--root", &b_root, "project", "add", PROJECT])
        .output()
        .expect("起不了 project add");
    assert!(
        reg.status.success(),
        "project add 失败：{}",
        String::from_utf8_lossy(&reg.stderr)
    );

    let child = Command::new(exe)
        .args(["--root", &b_root, "agent", "--project", PROJECT])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // agent 的诊断走 stderr，继承到测试输出里：`--nocapture` 时能看见它到底说了什么
        .stderr(Stdio::inherit())
        .spawn()
        .expect("起不了 agent 子进程");
    let mut io = m1::Duplex::from_child(child).expect("管子两头都得在");

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
/// 必须留下三样东西：head 不动、清单不进库、落下的块只是孤块（等块级 GC，那一半还没做）。
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
    let b_root = b_tmp
        .path()
        .to_str()
        .expect("utf-8 路径")
        .to_string();
    let exe = env!("CARGO_BIN_EXE_twosync");
    fs::create_dir_all(b_tmp.path().join(PROJECT)).expect("对端项目目录");
    let reg = Command::new(exe)
        .args(["--root", &b_root, "project", "add", PROJECT])
        .output()
        .expect("起不了 project add");
    assert!(
        reg.status.success(),
        "project add 失败：{}",
        String::from_utf8_lossy(&reg.stderr)
    );

    let mut child = Command::new(exe)
        .args(["--root", &b_root, "agent", "--project", PROJECT])
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
    drop(b_db);

    // 恢复：下一次**完整**会话必须把它追平，半成品不许把站点锁死。
    let child = Command::new(exe)
        .args(["--root", &b_root, "agent", "--project", PROJECT])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("重起 agent");
    let mut io = m1::Duplex::from_child(child).expect("管子两头都得在");
    let rep =
        session::push(&mut io, &*a.db, &a.cas(), &opts(PROJECT)).expect("崩溃之后的一次完整 push");
    io.finish().expect("agent 该正常收尾");
    assert!(rep.bytes_sent > 0, "第三块得补发出去");

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

/// 把"这一帧必须是某一种"这件事写成一处：手工驱动协议的用例里，读错帧的报错要能看出**在等谁**。
fn expect_frame(got: TsResult<Proto>, ok: impl Fn(&Proto) -> bool, want: &str) {
    let p = got.unwrap_or_else(|e| panic!("该收到 {want}，读帧就失败了：{e}"));
    if !ok(&p) {
        panic!("该收到 {want}，收到 {p:?}");
    }
}


