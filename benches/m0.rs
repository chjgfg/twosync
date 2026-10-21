//! §10.6 的基准：把"M0 的几条成本承诺"变成**可持续对比**的曲线，而不是一次性读数。
//!
//! # 为什么要有这个文件
//!
//! §11.1 那张 10 GiB 表很好，但它有两个毛病：① 一次性的，下次改动没法比；② 混着磁盘状态
//! （fsync 排队、页缓存冷热）。基准的做法是**把规模当自变量**，同一构建跑多个点，
//! 看的是"每单位成本是否随规模上升"（§17 补记十三那条判据），不是绝对耗时。
//!
//! # 测什么（§10.6 的清单里，现在就有对象的那几项）
//!
//! | 组 | 对应判据 | 规模 |
//! |---|---|---|
//! | `scan_steady` / `scan_forced` | 脏判定（§8 的 size+mtime 粗筛）与"全量重算"差多少 | 200 / 1 000 / 4 000 条 |
//! | `hash_throughput` | NFR-2 的 CPU 侧：blake3 切块 hash 的 MiB/s | 4 / 16 / 64 MiB 单文件 |
//! | `plan_only` | (f1) 与 §9.6.1：只读遍历随条目数的曲线 | 200 / 1 000 / 4 000 条 |
//! | `commit_incremental` | NFR-6：改 k 个文件之后提交一轮的成本随 k 走 | k = 1 / 10 / 100 |
//! | `log_walk` | NFR-7：`log`/union 的可达性遍历随历史长度走 | 5 / 25 / 100 个 commit |
//!
//! # 两条如实的边界
//!
//! - **窗口大小（`build::PAGE`）是编译期常量**，这一套测不了"调参曲线"。要做成运行期旋钮
//!   才有那条基准 ⇒ 记在 §10.6 的待办里，不假装测过。
//! - 二进制挂着 `allocstats` 计数分配器（lib 里 `#[global_allocator]`），每次 alloc 多几条
//!   原子指令 ⇒ **绝对值偏慢一点点，趋势不受影响**。要拿绝对吞吐对比硬件基准，用
//!   `--no-default-features` 的构建（还没做这个开关，先记着）。
//!
//! 跑法：`cargo bench`（release，criterion 默认每个点跑到 5 s 稳定为止 ⇒ 全套是分钟级）。
//! CI/快速看一眼：`cargo bench -- --test`（每个点只跑一次，只看能不能跑）。

use std::fs;
use std::path::{Path, PathBuf};

use criterion::{Criterion, Throughput};
use storage_engines::EngineKind;
use twosync::store::build::{self, Meta};
use twosync::store::cas::{self, Cas};
use twosync::store::keys;
use twosync::store::worktree;
use twosync::utils::{objects_dir, open_store};

/// 一个基准站点：临时目录 + 引擎 + 项目名固定的 pid。
///
/// 与测试里那份 `Site` 是三份同形代码（测试文件各自也有一份）。这里不复用是有意的：
/// 基准要自己控制"每一轮开始前是什么状态"，共享 helper 会把它拽成测试的形状。
/// 真正的判据逻辑（扫描/提交）在 `store` 里，只有一份。
struct Site {
    db: Box<dyn storage_engines::KvEngine>,
    pid: u32,
    tmp: tempfile::TempDir,
}

impl Site {
    /// 引擎按 `TWOSYNC_BENCH_ENGINE` 选（默认 `lsm_tree`），与测试台架那条 `TWOSYNC_TEST_ENGINE`
    /// 同一个形状 —— 这样 §10.6 的基准与 §10.8 的矩阵能用同一套"换引擎再跑一遍"的口径，
    /// 而不是"基准只在默认引擎上跑过，矩阵在两个引擎上绿过"两句各自成立却对不上。
    /// 名字解析走引擎自己的 `EngineKind::parse`（一份实现）；认不出就直接红，不悄悄退回默认。
    fn open() -> Site {
        let tmp = tempfile::tempdir().expect("tempdir");
        let engine = match std::env::var("TWOSYNC_BENCH_ENGINE") {
            Ok(s) if !s.is_empty() => {
                EngineKind::parse(&s).unwrap_or_else(|| panic!("不认识 TWOSYNC_BENCH_ENGINE={s:?}"))
            }
            _ => EngineKind::LsmTree,
        };
        let db = open_store(tmp.path(), engine).expect("open_store");
        Site {
            db,
            pid: keys::project_id("bench"),
            tmp,
        }
    }

    fn proj(&self) -> PathBuf {
        self.tmp.path().join("bench")
    }

    fn cas(&self, block: u64) -> Cas {
        Cas::new(&objects_dir(self.tmp.path()), block).expect("CAS")
    }

    /// 铺 `n` 个文件：每 10 个一层目录，内容互不相同（不去重 ⇒ 条目数与块数都是 `n` 的函数）。
    fn seed(&self, n: usize) {
        let root = self.proj();
        for i in 0..n {
            let dir = root.join(format!("d{:05}", i / 10));
            fs::create_dir_all(&dir).expect("建目录");
            let body = distinct((i as u64) * 1_000_003, 1024);
            fs::write(dir.join(format!("f{:02}.bin", i % 10)), body).expect("写文件");
        }
    }

    fn touch(&self, k: usize, salt: u64) {
        let root = self.proj();
        for i in 0..k {
            let p = root.join(format!("d{:05}", i / 10)).join(format!("f{:02}.bin", i % 10));
            fs::write(&p, distinct(salt * 7919 + i as u64, 1024)).expect("改文件");
        }
    }

    fn scan(&self) {
        worktree::scan(&*self.db, self.pid, &self.proj(), false, 2000).expect("scan");
    }

    fn scan_forced(&self) {
        worktree::scan(&*self.db, self.pid, &self.proj(), true, 2000).expect("scan");
    }

    fn commit(&self, msg: &str, ts: i64) {
        let store = self.cas(cas::DEFAULT_BLOCK);
        build::build(
            &*self.db,
            self.pid,
            &store,
            &self.proj(),
            Meta {
                peer: "bench",
                msg,
                ts,
                allow_mass_delete: true,
            },
        )
        .expect("commit");
    }
}

/// 周期不重复的字节：`(i as u8)` 那种写法会被 CAS 去重成一块，基准就变成"传一块"的假数。
fn distinct(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn sizes() -> Vec<usize> {
    vec![200, 1000, 4000]
}

/// 脏判定 vs 全量重算：同一份库，`force=false` 应该几乎不读文件内容。
fn scan_steady_vs_forced(c: &mut Criterion) {
    let mut g = c.benchmark_group("scan");
    for n in sizes() {
        let site = Site::open();
        site.seed(n);
        site.scan();
        site.commit("基线", 1);
        site.scan(); // 第二次起就是稳态

        g.bench_function(format!("steady_{n}"), |b| {
            b.iter(|| site.scan());
        });
        g.bench_function(format!("forced_{n}"), |b| {
            b.iter(|| site.scan_forced());
        });
    }
    g.finish();
}

/// 切块 + hash 的纯 CPU 吞吐（NFR-2 说"首次 10 GB 建库更重 IO"，那 IO 之外的下限就是这条）。
fn hash_throughput(c: &mut Criterion) {
    let mut g = c.benchmark_group("hash");
    let dir = tempfile::tempdir().unwrap();
    for mib in [4u64, 16, 64] {
        let p = dir.path().join(format!("f{mib}.bin"));
        fs::write(&p, distinct(0x5eed + mib, (mib * 1024 * 1024) as usize)).unwrap();
        let store = Cas::new(&objects_dir(dir.path()), cas::DEFAULT_BLOCK).unwrap();
        g.throughput(Throughput::Bytes(mib * 1024 * 1024));
        g.bench_function(format!("plan_{mib}mib"), |b| {
            b.iter(|| store.plan(Path::new(&p)).map(|blob| blob.blocks.len()).unwrap());
        });
    }
    g.finish();
}

/// 只读遍历（`plan`）随条目数的曲线 = (f1) 的那条判据在基准里的样子。
fn plan_only(c: &mut Criterion) {
    let mut g = c.benchmark_group("plan");
    for n in sizes() {
        let site = Site::open();
        site.seed(n);
        site.scan();
        site.commit("基线", 1);
        g.bench_function(format!("n_{n}"), |b| {
            b.iter(|| build::plan(&*site.db, site.pid).map(|p| p.entries).unwrap());
        });
    }
    g.finish();
}

/// NFR-6：改 k 个文件之后提交一轮的成本随 k 走。
///
/// 注意这一组**每轮都在改状态**（提交会移 head），所以量的是"增量提交"的真实循环，
/// 不是同一件事重复 n 次。历史一直变长 ⇒ 这条曲线如果上扬，上扬本身就是答案（Merkle 重建）。
fn commit_incremental(c: &mut Criterion) {
    let mut g = c.benchmark_group("commit");
    for k in [1usize, 10, 100] {
        let site = Site::open();
        site.seed(2000);
        site.scan();
        site.commit("起点", 1);
        let mut round = 0u64;
        g.bench_function(format!("touch_{k}"), |b| {
            b.iter(|| {
                round += 1;
                site.touch(k, round);
                site.scan();
                site.commit("增量", round as i64);
            });
        });
    }
    g.finish();
}

/// NFR-7：`log` / union 的可达性遍历随历史长度走，必须线性。
fn log_walk(c: &mut Criterion) {
    let mut g = c.benchmark_group("history");
    for target in [5usize, 25, 100] {
        let site = Site::open();
        site.seed(200);
        site.scan();
        for i in 0..target {
            site.touch(1, i as u64 + 1);
            site.scan();
            site.commit("一轮", i as i64 + 1);
        }
        let heads = build::ref_heads(&*site.db, site.pid, build::MAIN).unwrap();
        g.bench_function(format!("walk_{target}"), |b| {
            b.iter(|| {
                twosync::store::history::walk(&*site.db, site.pid, &heads, 0)
                    .map(|v| v.len())
                    .unwrap()
            });
        });
    }
    g.finish();
}

/// 自己写 main 而不用 `criterion_group!` + `criterion_main!`：为了一个开关 ——
/// **`TWOSYNC_BENCH_QUICK=1` 走缩样档**（`criterion_group!` 生成的函数自己 `configure_from_args()`，
/// 从外面塞不进任何配置）。
///
/// 为什么要有这一档：跨引擎比基准要跑两遍，而 bplus 在只读密集上贵 5–13 倍（§10.8 第三层实测），
/// 全档两遍不可控。缩样档只回答"哪家贵、贵几倍"，**尾延迟读不出来**。
/// ⚠️ 两种档的数不许混进同一张表：§10.6 那张是全档，跨引擎这张是缩样档，各自标自己。
/// （顺带一条踩过的坑：`cargo bench -- --mode quick` 不管用 —— criterion 0.5 不认 libtest 这个参数，
/// 报 `Unrecognized option: 'mode'`。这句是实测不是推测。）
fn main() {
    let mut crit = Criterion::default();
    if std::env::var_os("TWOSYNC_BENCH_QUICK").is_some() {
        crit = crit
            .sample_size(10)
            .warm_up_time(std::time::Duration::from_millis(500))
            .measurement_time(std::time::Duration::from_millis(1_500));
        println!("采样档 = 缩样(10 样本 / 0.5 s 预热 / 1.5 s 测量)：读倍数，不读尾延迟");
    }
    let mut crit = crit.configure_from_args();
    scan_steady_vs_forced(&mut crit);
    hash_throughput(&mut crit);
    plan_only(&mut crit);
    commit_incremental(&mut crit);
    log_walk(&mut crit);
    crit.final_summary();
}
