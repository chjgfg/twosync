//! §10.8 的第三层：**四引擎在同一负载下的 RSS / 墙钟(单线程 CPU) / 延迟分位 / 磁盘增长**。
//! 前两层的读数（画像层 + 场景层计数相同）在 `engine_matrix.rs`；这一层要的是"换引擎换来换掉多少"。
//!
//! **`#[ignore]` 而且一次只测一个引擎**，这不是偷懒是物理限制：进程峰值 RSS（`PeakWorkingSetSize` /
//! `VmHWM`）是**进程一生 monotone** 的高水位，四个引擎挤在同一个测试进程里就只能拿到一个共享峰值，
//! 那张表就成了假数。所以按 `TWOSYNC_PERF_ENGINE` 一个引擎一个进程地跑：
//!
//! ```text
//! foreach E in lsm_tree bplus_tree bitcask memory:
//!   TWOSYNC_PERF_ENGINE=$E cargo test --release --test engine_perf -- --ignored --nocapture
//! ```
//!
//! 口径（每一列都得说自己怎么来的）：
//! - 站点在 `%TEMP%`（本机 = **C: SSD**）。换盘必须重测：同形状 fsync 单价 C:/D: 差 33 倍（§15 第 15 条）。
//! - **墙钟只当"这一列在同一张表里可比"用**，不当跨 run 的证据（本机 run 间 ±2×，§17 补记廿八）。
//! - CPU 那一列诚实写法是"单线程 ⇒ CPU 秒 ≈ 墙钟秒"，NFR-6 真正要的是 `频率 × 这一列`。
//! - 折数（门面调用次数）用 `utils::Meter` 现测，**每阶段取增量**；内存两列都要：我们的堆峰值
//!   只看得见走 `GlobalAlloc` 的量，引擎自己 mmap 的那部分只有进程 RSS 看得见。

mod common;

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use storage_engines::{EngineConfig, StorageConfig};
use twosync::allocstats;
use twosync::store::build;
use twosync::store::cas::Cas;
use twosync::store::{checkout, gc, history, keys, worktree};
use twosync::utils::{check_durability_gate, counted, db_dir, dir_size, Meter};

const BLOCK: u64 = 4096;
/// 负载形状：8 个目录 × 125 个文件 × 16 KiB = 1 000 条路径 / 16 MiB / 4 096 个块。
const DIRS: usize = 8;
const PER_DIR: usize = 125;
const FILE_KIB: usize = 16;
/// 延迟分位的样本量。31 个样本 ⇒ "p99" 与最大值同一个人，所以表里报的是 min/中位/p95/max 四个数。
const SAMPLES: usize = 31;

struct Phase {
    name: &'static str,
    wall: Duration,
    folds: u64,
    heap_peak: usize,
}

impl Phase {
    fn line(&self) -> String {
        format!(
            "{:<22} {:>9.1} ms {:>7} 折 {:>9.2} MiB 堆峰",
            self.name,
            self.wall.as_secs_f64() * 1000.0,
            self.folds,
            self.heap_peak as f64 / 1_048_576.0,
        )
    }
}

fn write_payload(work: &Path) -> usize {
    let mut n = 0usize;
    let mut buf = vec![0u8; FILE_KIB * 1024];
    for d in 0..DIRS {
        let dir = work.join(format!("d{d}"));
        fs::create_dir_all(&dir).unwrap();
        for i in 0..PER_DIR {
            // 内容按 (目录, 序号) 派生：**四个引擎拿到的是同一份字节**，
            // 不然"哪个引擎省了空间"这种比较就成了比语料。
            let seed = format!("d{d}/f{i}.txt");
            for (k, b) in buf.iter_mut().enumerate() {
                *b = (blake3::hash(format!("{seed}#{k}").as_bytes()).as_bytes()[k % 32]) ^ (k as u8);
            }
            fs::write(dir.join(format!("f{i}.txt")), &buf).unwrap();
            n += 1;
        }
    }
    n
}

#[test]
#[ignore = "§10.8 性能表：必须按 TWOSYNC_PERF_ENGINE 一个引擎一个进程地跑（进程峰值 RSS 只能这样归因）"]
fn engine_perf_table() {
    let engine = std::env::var("TWOSYNC_PERF_ENGINE")
        .expect("要设 TWOSYNC_PERF_ENGINE=lsm_tree|bplus_tree|bitcask|memory（一次一个引擎）");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".twosync")).unwrap();
    let work = root.join("p");
    fs::create_dir_all(&work).unwrap();

    // 绕开 `open_store`：那一条带着 FR-21 闸，`bitcask`/`memory` 会被挡在测量之前。
    // 这一层的任务恰恰是"给四个引擎都拿到数,并且把闸门结论原样列在旁边"。
    let raw = StorageConfig {
        kind: common::by_name(&engine),
        data_dir: db_dir(root),
        engine: EngineConfig::default(),
    }
    .open()
    .unwrap_or_else(|e| panic!("{engine} 连开都开不起来：{e}"));
    let gate = check_durability_gate(&*raw).err().map(|e| e.to_string());
    let meter = Meter::default();
    let db = counted(raw, &meter, false);
    let pid = keys::project_id("p");
    let store = Cas::new(&twosync::utils::objects_dir(root), BLOCK).unwrap();

    let files = write_payload(&work);
    let mut phases: Vec<Phase> = Vec::new();
    let mut measure = |name: &'static str, f: &mut dyn FnMut()| {
        let m0 = meter.snapshot();
        let wm = allocstats::watermark();
        let t = Instant::now();
        f();
        let m1 = meter.snapshot();
        phases.push(Phase {
            name,
            wall: t.elapsed(),
            // 增量：折数是"这一条命令付了多少次门面调用"，不是"到这里累计付了多少"
            folds: m1.folds().saturating_sub(m0.folds()),
            heap_peak: wm.peak_growth(),
        });
    };

    measure("scan 全量", &mut || {
        worktree::scan(&*db, pid, &work, false, 2000).unwrap();
    });
    let mut head = keys::Hash::default();
    measure("commit 首次(1000 条)", &mut || {
        let built = build::build(
            &*db,
            pid,
            &store,
            &work,
            build::Meta {
                peer: "perf",
                msg: "perf v1",
                ts: 1,
                allow_mass_delete: false,
            },
        )
        .unwrap();
        head = built.commit.expect("首次提交必有改动");
    });
    measure("plan(只读)", &mut || {
        let p = build::plan(&*db, pid).unwrap();
        assert!(!p.changes.is_empty() || p.entries > 0, "plan 读不到东西，测的是空气");
    });
    measure("log 全史", &mut || {
        let n = history::walk(&*db, pid, &[head], 0).unwrap();
        assert!(!n.is_empty());
    });
    measure("checkout 对照同一版", &mut || {
        let r = checkout::checkout(
            &*db,
            pid,
            &store,
            &work,
            &head,
            None,
            checkout::Opts {
                apply: false,
                allow_mass_delete: false,
            },
        )
        .unwrap();
        assert!(r.is_noop(), "同一版比对同一版本该是空的，实际 {r:?}");
    });
    measure("gc --sweep(dry)", &mut || {
        let p = gc::sweep(&*db, &store, pid, true).unwrap();
        assert_eq!(p.commits_dropped, 0, "head 可达的历史不许被判成垃圾：{p:?}");
    });
    measure("commit 稳态(改 1 文件)", &mut || {
        fs::write(work.join("d3/f7.txt"), [7u8; FILE_KIB * 1024]).unwrap();
        worktree::scan(&*db, pid, &work, false, 2000).unwrap();
        let built = build::build(
            &*db,
            pid,
            &store,
            &work,
            build::Meta {
                peer: "perf",
                msg: "perf v2",
                ts: 2,
                allow_mass_delete: false,
            },
        )
        .unwrap();
        assert_eq!(built.changes.modify, 1, "稳态那一刀只该改 1 条：{:?}", built.changes);
        head = built.commit.expect("改了就该有新 commit");
    });

    // ── 延迟分位：把最常被周期叫的那条只读命令重复 31 次 ──
    let mut lat: Vec<Duration> = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let t = Instant::now();
        let _ = build::plan(&*db, pid).unwrap();
        lat.push(t.elapsed());
    }
    lat.sort();

    let h = db.health();
    let dbs = dir_size(&db_dir(root));
    let objs = dir_size(&twosync::utils::objects_dir(root));
    let rss = allocstats::peak_rss();

    println!("=== §10.8 引擎性能表 · {engine} ===");
    println!(
        "负载      = {files} 条路径 / {} MiB 原始 / {} 块（block {} B，定长）",
        files * FILE_KIB / 1024,
        files * FILE_KIB * 1024 / BLOCK as usize,
        BLOCK
    );
    println!(
        "FR-21 闸  = {}",
        match &gate {
            None => "过".to_string(),
            Some(why) => format!("**拒**（{why}）⇒ 这一行只作对照，不许当可用配置"),
        }
    );
    for p in &phases {
        println!("{}", p.line());
    }
    let pct = |q: f64| -> f64 {
        let i = ((SAMPLES as f64 - 1.0) * q).round() as usize;
        lat[i].as_secs_f64() * 1000.0
    };
    println!(
        "{:<22} min {:>6.1} / 中位 {:>6.1} / p95 {:>6.1} / max {:>6.1} ms（{SAMPLES} 次，尾延迟要更多样本才读得出）",
        "plan 的延迟分位",
        pct(0.0),
        pct(0.5),
        pct(0.95),
        pct(1.0)
    );
    println!(
        "库磁盘    = {:>8.2} MiB / {} 行（alive≈全库物化行数，含版本链）",
        dbs.bytes as f64 / 1_048_576.0,
        h.total_rows
    );
    println!(
        "CAS 磁盘  = {:>8.2} MiB / {} 个块文件",
        objs.bytes as f64 / 1_048_576.0,
        objs.files
    );
    println!(
        "进程峰值 RSS = {}",
        match &rss {
            Some(r) => format!(
                "{:.2} MiB（当前 {:.2} MiB）⇒ 一个进程只测一个引擎才归因得清",
                r.peak_bytes as f64 / 1_048_576.0,
                r.now_bytes as f64 / 1_048_576.0
            ),
            None => "本机拿不到（这列留空，不许猜）".to_string(),
        }
    );
    println!(
        "口径      = 站点在 `%TEMP%`（本机 C: SSD）；release 构建；单线程 ⇒ CPU 秒 ≈ 墙钟秒；\
         墙钟 run 间 ±2× ⇒ 只用来在同一次运行内比较，不当跨 run 证据"
    );
}
