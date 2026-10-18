//! §9.3 的 mark-sweep 用例：**该留的一个不许少，该删的一个不许留**。
//!
//! 跑法：`cargo test --test gc_sweep`。
//!
//! # 为什么这一族用例的分量比别的重
//!
//! GC 是本项目里唯一一个"**写操作是删除**"的路径：别的 bug 顶多让一次同步失败，这一条的 bug 直接把
//! 历史删掉，而且删完之后 `doctor`/`status` 看起来都是干净的。所以每个用例都要**两头都断言**：
//! 不可达的确实消失了，**同时**可达的对象、清单、块、字节数一个都没少。
//!
//! # §9.3 的两趟形状，每趟都要单独断言
//!
//! `sweep` 只**记账**（refcnt 归零 ⇒ 往 `D` 表写一行），`unlink_pending` 才碰文件系统（限额 + 过桶）。
//! 分开是为了崩溃：崩在两趟之间留下的是"该删的文件还在"（垃圾，下一趟接着删），而不是"清单引用一个
//! 不存在的块"（损坏）。所以这里的用例断言的是**五种**残留形状都各归各位：
//! 记账了但没删（限额 0）、删了一半（限额 1，剩下的仍在表上）、**换个进程重开库**还能接着删、
//! 崩在中途（文件已没、账还在 ⇒ `missing`）、以及"删除真的扣了 FR-15 的桶"
//! （用 `Cas::paced_bytes` 核对字节数，不掐墙钟）。
//!
//! # 测试用的形状
//!
//! 块大小取 4 KiB（`Cas::new(.., BLOCK)`），这样"两个不同 blob 共享一个块"能用几 KiB 的文件造出来：
//! 文件 A = P（一块），文件 B = P ++ Q（两块，第一块与 A 相同）⇒ `K` 行里 P 的 refcnt = 2。
//! 默认 4 MiB 的块要造这个得写 8 MiB 语料，用例不该那么贵。

use std::fs;
use std::path::{Path, PathBuf};

use storage_engines::EngineKind;
use twosync::store::build::{self, Meta};
use twosync::store::cas::Cas;
use twosync::store::gc;
use twosync::store::history;
use twosync::store::keys::{self, Hash, Kind};
use twosync::store::object;
use twosync::store::worktree;
use twosync::utils::{objects_dir, open_store};

const PROJECT: &str = "gcsite";
const BLOCK: u64 = 4096;

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
        Cas::new(&objects_dir(self.tmp.path()), BLOCK).expect("CAS")
    }

    /// 写文件 → scan → commit。`files` 是本这一版要**存在**的全部文件（其余会被当成删除）。
    fn commit(&self, files: &[(&str, &[u8])], ts: i64) -> Hash {
        let root = self.proj();
        fs::create_dir_all(&root).expect("项目目录");
        // 先清掉上一版留下的文件：用例要的是"这一版只有这些条目"，否则删除会混进账里。
        for e in walk_files(&root) {
            let rel = e.strip_prefix(&root).expect("相对路径");
            let want = files.iter().any(|(p, _)| Path::new(p) == rel);
            if !want {
                fs::remove_file(&e).expect("删掉旧文件");
            }
        }
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
                peer: "t",
                msg: "用例",
                ts,
                allow_mass_delete: true,
            },
        )
        .expect("commit")
        .commit
        .expect("这一轮该有提交")
    }

    fn heads(&self) -> Vec<Hash> {
        build::ref_heads(&*self.db, self.pid, build::MAIN).expect("heads")
    }

    /// 把站点关掉（**释放库锁**）但保住临时目录：用来演"上一条命令退出、下一条命令起来"。
    ///
    /// 必须真的 drop：`storage_engines` 拿的是库级文件锁，同进程里两个句柄并存会直接被拒
    /// （实测 `kind: WouldBlock`，"数据库锁被占用 … 请先释放全部句柄再 open"）。这条也正是
    /// §9.6.4 单写者前提在引擎侧的兜底——`gc` 与 `commit` 不可能在两个进程里同时握着库。
    fn release(site: Site) -> tempfile::TempDir {
        let Site { db, tmp, .. } = site;
        drop(db);
        tmp
    }

    fn block_files(&self) -> usize {
        let mut n = 0;
        let mut stack = vec![objects_dir(self.tmp.path())];
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
}

fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            if e.path().is_dir() {
                stack.push(e.path());
            } else {
                out.push(e.path());
            }
        }
    }
    out
}

/// 周期不重复的字节：两块不同的 4 KiB 内容，用来造"P"和"Q"。
fn pattern(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 31) as u8
        })
        .collect()
}

/// blob 的块哈希表（`B` 行是唯一的真相来源）。
fn blocks_of(db: &dyn storage_engines::KvEngine, pid: u32, blob: &Hash) -> Vec<(Hash, u32)> {
    let b = build::read_blob(db, pid, blob).expect("读 blob");
    b.blocks.iter().map(|x| (x.hash, x.size)).collect()
}

/// 树里某个路径的 blob。
fn blob_at(site: &Site, path: &str) -> Hash {
    let head = *site.heads().first().expect("有 head");
    let c = object::read_commit(&*site.db, site.pid, &head).expect("读 commit");
    let e = history::resolve(&*site.db, site.pid, &c.tree, path)
        .expect("resolve")
        .unwrap_or_else(|| panic!("{path} 不在树里"));
    assert_eq!(e.kind, Kind::File, "{path} 得是个文件");
    e.hash
}

/// 主用例：**不可达的那一版整批消失，可达的一根毛都不动**。
#[test]
fn an_unreachable_commit_is_swept_while_the_reachable_one_is_untouched() {
    let p = pattern(0xA11CE, 4096);
    let q = pattern(0xB0B, 4096);
    let site = Site::open();
    // C1：只有 a（内容 P）。C2：a 还在，再加 b（内容 P ++ Q ⇒ 两块，第一块与 a 共享）。
    let c1 = site.commit(&[("a.bin", &p)], 1);
    let mut ab = p.clone();
    ab.extend_from_slice(&q);
    site.commit(&[("a.bin", &p), ("b.bin", &ab)], 2);

    // 倒回 head 之前先把该留的东西记下来（之后 `b.bin` 就不在树里了，只能靠这三份快照核对）。
    let a_hash = blob_at(&site, "a.bin");
    let blob_b = blob_at(&site, "b.bin");
    let blocks_b = blocks_of(&*site.db, site.pid, &blob_b);
    let blocks_a = blocks_of(&*site.db, site.pid, &a_hash);
    assert_eq!(blocks_b.len(), 2, "b 是 8 KiB ⇒ 两块：{blocks_b:?}");
    let shared = blocks_b[0].0;
    let only_b = blocks_b[1].0;
    assert_eq!(shared, blocks_a[0].0, "b 的第一块就是 a 那块");

    // 把 head 倒回 C1 ⇒ C2 整批不可达。这正是"被丢弃的分叉 / reset 之后"的形状。
    build::set_ref(&*site.db, site.pid, build::MAIN, &[c1]).expect("倒回 head");

    // ── dry-run：账要报对，但要一个字都不改 ──
    let files_before = site.block_files();
    let plan = gc::sweep(&*site.db, &site.cas(), site.pid, true).expect("dry sweep");
    assert_eq!(plan.commits_kept, 1, "保留集就是 C1：{plan:?}");
    assert_eq!(plan.commits_dropped, 1, "C2 该被判不可达");
    assert_eq!(plan.blobs_dropped, 1, "只有 b 的清单不可达");
    assert_eq!(plan.blocks_pending, 1, "只有 Q 那块该被判归零：{plan:?}");
    assert_eq!(plan.blocks_unlinked, 0, "dry-run 连'已删'都不能报：{plan:?}");
    assert_eq!(site.block_files(), files_before, "dry-run 一个文件都不许删");
    assert_eq!(
        count_rows(&*site.db, keys::TAG_PENDING, site.pid),
        0,
        "dry-run 也不许往 pending 表上落行"
    );

    // ── 真回收：第一趟只记账，删文件是限额的第二趟（§9.3） ──
    let plan = gc::sweep(&*site.db, &site.cas(), site.pid, false).expect("sweep");
    assert_eq!(plan.commits_dropped, 1);
    assert_eq!(plan.blocks_pending, 1, "Q 那块该记进 pending 表：{plan:?}");
    assert_eq!(plan.bytes_pending, u64::from(blocks_b[1].1));
    assert_eq!(plan.blocks_unlinked, 0, "sweep 自己不删文件");
    assert!(
        site.cas().contains(&only_b),
        "文件必须还在——删除是第二趟可中断、可续的活"
    );
    assert_eq!(
        gc::refcnt_of(&*site.db, site.pid, &only_b).expect("读账"),
        Some(0),
        "账已归零，等第二趟清"
    );

    // 该删的账先清、文件随后删
    assert!(
        build::read_blob(&*site.db, site.pid, &blob_b).is_err(),
        "b 的清单该被删掉"
    );
    assert_eq!(
        commit_hashes(&site),
        vec![c1],
        "`C` 行里只该剩下可达的那一条"
    );
    let ul = gc::unlink_pending(&*site.db, &site.cas(), site.pid, usize::MAX, false).expect("unlink");
    assert_eq!(ul.files, 1, "第二趟删一个：{ul:?}");
    assert_eq!(ul.bytes, u64::from(blocks_b[1].1));
    assert!(!site.cas().contains(&only_b), "Q 那块的文件这下该没了");
    assert_eq!(
        gc::refcnt_of(&*site.db, site.pid, &only_b).expect("读账"),
        None,
        "`K` 行与 pending 行一起清（先清账再删文件，反了就会留下悬空引用）"
    );
    assert_eq!(gc::pending_total(&*site.db, site.pid).expect("读表"), (0, 0));

    // 该留的一个都不能少
    assert!(
        object::read_commit(&*site.db, site.pid, &c1).is_ok(),
        "C1 必须还能读"
    );
    assert!(build::read_blob(&*site.db, site.pid, &a_hash).is_ok(), "可达的清单必须还在");
    assert!(site.cas().contains(&shared), "P 那块仍被可达的 a 引用 ⇒ 必须还在盘上");
    assert_eq!(
        gc::refcnt_of(&*site.db, site.pid, &shared).expect("refcnt"),
        Some(1),
        "共享块的 refcnt 从 2 掉到 1，不是掉到 0"
    );
    for (h, _) in &blocks_a {
        assert!(site.cas().contains(h), "可达清单引用的块 {} 不见了", keys::short(h));
    }
    assert_eq!(site.block_files(), files_before - 1, "只该少一个文件（Q 那块）");
    assert_eq!(
        site.heads(),
        vec![c1],
        "sweep 不许动 head：它只删不可达的东西"
    );
}

/// `C` 前缀下的全部 commit hash（排序后返回，断言才不依赖扫描顺序）。
fn commit_hashes(site: &Site) -> Vec<Hash> {
    let mut prefix = vec![keys::TAG_COMMIT];
    prefix.extend_from_slice(&site.pid.to_be_bytes());
    let tx = site.db.begin();
    let rows = tx.prefix_scan(&prefix);
    tx.commit();
    let mut out: Vec<Hash> = rows
        .iter()
        .filter_map(|r| {
            let tail = r.key.get(prefix.len()..)?;
            if tail.len() != 32 {
                return None;
            }
            let mut h = [0u8; 32];
            h.copy_from_slice(tail);
            Some(h)
        })
        .collect();
    out.sort();
    out
}

/// `--sweep` 不带 `--apply` 的那一半：**报账可以，改一个字都不行**。
#[test]
fn a_dry_run_reports_but_changes_nothing() {
    let p = pattern(0x1234, 4096);
    let site = Site::open();
    let c1 = site.commit(&[("a.bin", &p)], 1);
    let mut q = p.clone();
    q.extend_from_slice(&pattern(0x99, 4096));
    site.commit(&[("a.bin", &p), ("b.bin", &q)], 2);
    build::set_ref(&*site.db, site.pid, build::MAIN, &[c1]).expect("倒回");

    let files = site.block_files();
    let plan = gc::sweep(&*site.db, &site.cas(), site.pid, true).expect("dry");
    assert!(plan.commits_dropped > 0, "dry-run 也要把'将要删什么'报出来");
    assert_eq!(
        plan.blocks_pending, 1,
        "dry-run 的待删数必须报得出来——它原来永远报 0（§17 补记十九）：{plan:?}"
    );
    assert!(plan.bytes_pending > 0, "字节数也得报：{plan:?}");
    assert_eq!(plan.pending_total, 1, "dry 模式把'将记进表'的也算进 total：{plan:?}");
    assert_eq!(
        gc::pending_total(&*site.db, site.pid).expect("读表"),
        (0, 0),
        "但表上真一个都不许多"
    );
    assert_eq!(site.block_files(), files, "dry-run 一个文件都不许删");
    assert_eq!(
        count_rows(&*site.db, keys::TAG_BLOB, site.pid),
        2,
        "dry-run 连清单行都不该动"
    );
    assert_eq!(
        count_rows(&*site.db, keys::TAG_PENDING, site.pid),
        0,
        "dry-run 也不该落 pending 行"
    );
}

/// pending 表存在的意义就是"这一趟没删完，下一趟接着删"。这里演一遍崩在中途：
/// 记完 pending 之后把文件手工抹掉（等价于"上一趟删了文件但没清账"），第二趟必须
/// 把账清干净并报出 `missing`，而不是 panic、也不是把垃圾留在表上。
#[test]
fn the_pending_list_is_resumable_after_a_crash_mid_unlink() {
    let p = pattern(0xF00D, 4096);
    let site = Site::open();
    let c1 = site.commit(&[("a.bin", &p)], 1);
    let mut r = p.clone();
    r.extend_from_slice(&pattern(0xBEEF, 4096));
    site.commit(&[("a.bin", &p), ("b.bin", &r)], 2);
    let blob_b = blob_at(&site, "b.bin");
    let only_b = blocks_of(&*site.db, site.pid, &blob_b)[1].0;
    build::set_ref(&*site.db, site.pid, build::MAIN, &[c1]).expect("倒回");

    gc::sweep(&*site.db, &site.cas(), site.pid, false).expect("sweep");
    assert_eq!(gc::pending_total(&*site.db, site.pid).expect("读表").0, 1);

    // 假装上一趟死在"文件已删、账未清"的那一刻
    let path = site.cas().dir().join(Cas::path_of(&only_b));
    fs::remove_file(&path).expect("手工抹掉块文件");
    let ul = gc::unlink_pending(&*site.db, &site.cas(), site.pid, usize::MAX, false).expect("unlink");
    assert_eq!(ul.missing, 1, "文件已经不在 ⇒ 报 missing，不 panic：{ul:?}");
    assert_eq!(ul.files, 0);
    assert_eq!(
        gc::pending_total(&*site.db, site.pid).expect("再读表"),
        (0, 0),
        "账必须清掉，否则下一趟永远背着它"
    );
    assert_eq!(
        gc::refcnt_of(&*site.db, site.pid, &only_b).expect("读账"),
        None,
        "`K` 行也跟着清"
    );
}

/// `--unlink-max-files` 截断的是"这一趟删几个"，剩下的必须仍然记在表上（可续），
/// 而不是被顺手丢掉。这里一次删一个，连删三趟，把"可中断、可续"演成断言。
#[test]
fn a_bounded_unlink_pass_leaves_the_rest_on_the_pending_list() {
    let site = Site::open();
    let keep = pattern(7, 4096);
    let c1 = site.commit(&[("keep.bin", &keep)], 1);
    // 第二版多两个文件 ⇒ 两个"只被不可达清单引用"的块，pending 表上该有两条。
    let x = pattern(0x111, 4096);
    let y = pattern(0x222, 4096);
    site.commit(&[("keep.bin", &keep), ("x.bin", &x), ("y.bin", &y)], 2);
    let bx = blocks_of(&*site.db, site.pid, &blob_at(&site, "x.bin"))[0].0;
    let by = blocks_of(&*site.db, site.pid, &blob_at(&site, "y.bin"))[0].0;
    assert_ne!(bx, by, "两块内容不同才测得出限额");
    build::set_ref(&*site.db, site.pid, build::MAIN, &[c1]).expect("倒回");

    let plan = gc::sweep(&*site.db, &site.cas(), site.pid, false).expect("sweep");
    assert_eq!(plan.blocks_pending, 2, "两个块该归零：{plan:?}");
    assert_eq!(
        gc::pending_total(&*site.db, site.pid).expect("读表"),
        (2, 8192),
        "两条各 4 KiB"
    );

    // 限额 0 ⇒ 一个都不删，但欠着的账必须原样留着，并且报出"还欠几个"。
    let ul = gc::unlink_pending(&*site.db, &site.cas(), site.pid, 0, false).expect("限额 0");
    assert_eq!(ul.files, 0, "限额 0 就是 0");
    assert_eq!(ul.left, 2, "要报出还欠 2 个：{ul:?}");
    assert!(
        site.cas().contains(&bx) && site.cas().contains(&by),
        "文件必须都还在盘上"
    );

    // 放开一档：删掉一个，另一个仍在表上——这一条是"下一趟接着删"的凭据。
    let ul = gc::unlink_pending(&*site.db, &site.cas(), site.pid, 1, false).expect("限额 1");
    assert_eq!(ul.files, 1, "一次只删一个：{ul:?}");
    assert_eq!(ul.left, 1);
    assert_eq!(
        gc::pending_total(&*site.db, site.pid).expect("再读表").0,
        1,
        "剩下的仍记在表上，不是被跳过就忘了"
    );
    let gone = usize::from(!site.cas().contains(&bx)) + usize::from(!site.cas().contains(&by));
    assert_eq!(gone, 1, "恰好少了一个文件");

    let ul = gc::unlink_pending(&*site.db, &site.cas(), site.pid, 5, false).expect("放开");
    assert_eq!(ul.files, 1, "表上只剩一个 ⇒ 限额用不完不该虚报");
    assert_eq!(ul.left, 0);
    assert_eq!(gc::pending_total(&*site.db, site.pid).expect("收尾"), (0, 0));
    assert!(!site.cas().contains(&bx) && !site.cas().contains(&by), "两个都该消失");
    assert!(
        site.cas().contains(&blocks_of(&*site.db, site.pid, &blob_at(&site, "keep.bin"))[0].0),
        "可达的 keep.bin 那块一个趟次都不该被碰到"
    );
}

/// FR-15 那句"**删除也过同一个桶**"必须可核对，否则限速只是命令行上一个看起来生效的旋钮。
/// 这里不掐墙钟（单核机上计时用例本身就是噪声源），改断言"累计扣桶的字节 = 被删块的 size 之和"。
#[test]
fn the_unlink_pass_charges_the_same_bucket_as_a_write() {
    let site = Site::open();
    let keep = pattern(11, 4096);
    let c1 = site.commit(&[("keep.bin", &keep)], 1);
    let x = pattern(0x555, 4096);
    let y = pattern(0x666, 4096);
    site.commit(&[("keep.bin", &keep), ("x.bin", &x), ("y.bin", &y)], 2);
    let want: u64 = ["x.bin", "y.bin"]
        .iter()
        .map(|n| u64::from(blocks_of(&*site.db, site.pid, &blob_at(&site, n))[0].1))
        .sum();
    assert_eq!(want, 8192, "两个各一块 4 KiB：{want}");
    build::set_ref(&*site.db, site.pid, build::MAIN, &[c1]).expect("倒回");

    // 限额设得极大 ⇒ 真的不会睡觉，但桶照样累计（`paced_bytes` 就是这条用例的尺子）。
    let cas = Cas::new(&objects_dir(site.tmp.path()), BLOCK)
        .expect("CAS")
        .with_rate_limit(1 << 30);
    gc::sweep(&*site.db, &cas, site.pid, false).expect("sweep");
    assert_eq!(cas.paced_bytes(), Some(0), "sweep 不碰文件系统 ⇒ 一个字节都不该扣桶");

    let dry = gc::unlink_pending(&*site.db, &cas, site.pid, usize::MAX, true).expect("dry unlink");
    assert_eq!(dry.files, 2, "dry 要报出'将要删 2 个'：{dry:?}");
    assert_eq!(cas.paced_bytes(), Some(0), "但 dry 什么都没删 ⇒ 桶不许扣");

    let ul = gc::unlink_pending(&*site.db, &cas, site.pid, usize::MAX, false).expect("unlink");
    assert_eq!(ul.files, 2, "两块都该删掉：{ul:?}");
    assert_eq!(
        cas.paced_bytes(),
        Some(want),
        "扣桶的字节必须等于被删块的 size 之和（§9.3/FR-15 的那句话是硬的）"
    );
}

/// pending 表的全部价值在于"**换个进程也还认**"：`gc` 是一条命令、`unlink` 是下一条（或明天那一次）。
/// 所以这里在记完账之后**重开一个引擎句柄**再删——同进程同一句柄读到的表证明不了持久性。
#[test]
fn the_pending_table_survives_a_reopen() {
    let site = Site::open();
    let keep = pattern(31, 4096);
    let c1 = site.commit(&[("keep.bin", &keep)], 1);
    let x = pattern(0x777, 4096);
    site.commit(&[("keep.bin", &keep), ("x.bin", &x)], 2);
    let bx = blocks_of(&*site.db, site.pid, &blob_at(&site, "x.bin"))[0].0;
    build::set_ref(&*site.db, site.pid, build::MAIN, &[c1]).expect("倒回");
    let pid = site.pid;

    gc::sweep(&*site.db, &site.cas(), pid, false).expect("sweep 记账");
    assert_eq!(gc::pending_total(&*site.db, pid).expect("读表").0, 1);
    assert!(site.cas().contains(&bx), "sweep 这一趟文件必须还在");

    // 上一条命令退出（释放库锁），下一条命令起来：表必须还认。
    let tmp = Site::release(site);
    let db2 = open_store(tmp.path(), EngineKind::LsmTree).expect("重开库");
    assert_eq!(
        gc::pending_total(&*db2, pid).expect("新句柄读表").0,
        1,
        "表是落在库里的，不是内存里的清单"
    );
    let cas = Cas::new(&objects_dir(tmp.path()), BLOCK).expect("CAS");
    let ul = gc::unlink_pending(&*db2, &cas, pid, usize::MAX, false).expect("第二趟");
    assert_eq!(ul.files, 1, "{ul:?}");
    assert!(!cas.contains(&bx), "这下文件才真没了");
    assert_eq!(gc::pending_total(&*db2, pid).expect("收尾"), (0, 0));
    assert_eq!(
        gc::refcnt_of(&*db2, pid, &bx).expect("读账"),
        None,
        "新句柄看到的 `K` 行也是清掉的"
    );
}

/// 崩溃残留的形状：块文件在盘上、`K` 账上没有 ⇒ **数出来，但不删**。
/// 这是 `serve` 先落块、清单还没到就被打断留下的东西（§10.2 那条用例造的就是它）。
#[test]
fn orphan_files_are_counted_not_deleted() {
    let site = Site::open();
    let p = pattern(0x77, 4096);
    site.commit(&[("a.bin", &p)], 1);

    // 手工往 CAS 里塞一个"没人认领"的块：走 CAS 自己的落盘路径，但**不写任何 `B`/`K` 行**。
    let loose = pattern(0xDEAD, 4096);
    let out = site.cas().put_bytes(&loose).expect("put_bytes");
    let files = site.block_files();
    assert!(files >= 2, "现在该有 a 的块 + 这个孤儿：{files}");

    let plan = gc::sweep(&*site.db, &site.cas(), site.pid, false).expect("sweep");
    assert_eq!(plan.orphan_files, 1, "孤儿要数得出来：{plan:?}");
    assert_eq!(plan.orphan_bytes, loose.len() as u64);
    assert_eq!(
        site.block_files(),
        files,
        "这一版不许删孤儿文件（删它要跟保留集一起判断，见 §9.3 的顺序）"
    );
    assert_eq!(
        gc::refcnt_of(&*site.db, site.pid, &out.blob.blocks[0].hash).expect("refcnt"),
        None,
        "孤儿本来就没有 `K` 行——正因为它没有，才被数成孤儿"
    );
}

/// 两个不同清单共享一个块时，删掉其中一个**只减一次引用**（跨项目共享的同一条机制）。
#[test]
fn a_shared_block_survives_the_removal_of_one_adopter() {
    let p = pattern(0xAAAA, 4096);
    let mut pq = p.clone();
    pq.extend_from_slice(&pattern(0xBBBB, 4096));
    let site = Site::open();
    let c1 = site.commit(&[("a.bin", &p)], 1);
    site.commit(&[("a.bin", &p), ("b.bin", &pq)], 2);

    let a_hash = blob_at(&site, "a.bin");
    let shared = blocks_of(&*site.db, site.pid, &a_hash)[0].0;
    assert_eq!(
        gc::refcnt_of(&*site.db, site.pid, &shared).expect("refcnt"),
        Some(2),
        "P 这块同时被 a 与 b 的清单引用"
    );

    build::set_ref(&*site.db, site.pid, build::MAIN, &[c1]).expect("倒回 head ⇒ b 不可达");
    let plan = gc::sweep(&*site.db, &site.cas(), site.pid, false).expect("sweep");
    assert_eq!(plan.blobs_dropped, 1, "只回收 b 的清单");
    assert_eq!(
        gc::refcnt_of(&*site.db, site.pid, &shared).expect("refcnt"),
        Some(1),
        "对称地 -1：还剩 a 那一份引用"
    );
    assert!(site.cas().contains(&shared), "refcnt 还没归零 ⇒ 文件必须还在");
}

fn count_rows(db: &dyn storage_engines::KvEngine, tag: u8, pid: u32) -> usize {
    let mut prefix = vec![tag];
    prefix.extend_from_slice(&pid.to_be_bytes());
    let tx = db.begin();
    let rows = tx.prefix_scan(&prefix);
    tx.commit();
    rows.len()
}
