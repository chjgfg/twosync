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
//! **第二种台架：指向一个现成站点**（量 12 GiB 那一档的只读列）。`TWOSYNC_PERF_ROOT` 给了 root 就
//! 必须同时给 `TWOSYNC_PERF_READ_ONLY=1`，因为台架里有三个阶段是写库的（`scan` / `commit` /
//! `gc --sweep`，最后那个连 dry-run 都不是只读，见下面跳过它那段注释）。只读模式下 `log` 与
//! `checkout` 用的是**站点自己的** `main` head，不是台架现建的 commit。
//!
//! ```text
//! TWOSYNC_PERF_ENGINE=lsm_tree TWOSYNC_PERF_ROOT=D:/ts10g TWOSYNC_PERF_PROJECT=lab \
//! TWOSYNC_PERF_READ_ONLY=1 cargo test --release --test engine_perf -- --ignored --nocapture
//! ```
//!
//! 口径（每一列都得说自己怎么来的）：
//! - 站点在 `%TEMP%`（本机 = **C: SSD**）；只读台架那一档的站点由 `TWOSYNC_PERF_ROOT` 给，
//!   盘型按路径认（本机 **D: = 5400 转机械**）。换盘必须重测：同形状 fsync 单价 C:/D: 差 33 倍（§15 第 15 条）。
//! - **墙钟只当"这一列在同一张表里可比"用**，不当跨 run 的证据（本机 run 间 ±2×，§17 补记廿八）。
//! - CPU 那一列诚实写法是"单线程 ⇒ CPU 秒 ≈ 墙钟秒"，NFR-6 真正要的是 `频率 × 这一列`。
//! - 折数（门面调用次数）用 `utils::Meter` 现测，**每阶段取增量**；内存两列都要：我们的堆峰值
//!   只看得见走 `GlobalAlloc` 的量，引擎自己 mmap 的那部分只有进程 RSS 看得见。

mod common;

use std::fs;
use std::path::{Path, PathBuf};
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
/// 延迟分位的默认样本量（小站）。31 个样本 ⇒ "p99" 与最大值同一个人，所以表里报的是
/// min/中位/p95/max 四个数。只读台架默认 7，见下面 `samples` 那一段。
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
    // 两种台架：**自建小站**（默认，tempdir + 1 000 条自造文件）与**指向一个现成站点**
    // （`TWOSYNC_PERF_ROOT` + `TWOSYNC_PERF_PROJECT`）。后者必须配 `TWOSYNC_PERF_READ_ONLY=1`
    // 一起用：拿别人的站点做测量，不等于有权改它。
    let given = std::env::var("TWOSYNC_PERF_ROOT").ok().filter(|s| !s.is_empty());
    let read_only = std::env::var_os("TWOSYNC_PERF_READ_ONLY").is_some();
    if given.is_some() && !read_only {
        panic!(
            "指着现成站点就不许跑会动库的阶段（没给 `TWOSYNC_PERF_READ_ONLY=1`）：\
             `scan` / `commit` / `gc --sweep` 都会改这个站点的库，那不叫测量，叫改动"
        );
    }
    let project = std::env::var("TWOSYNC_PERF_PROJECT").unwrap_or_else(|_| "p".to_string());
    let tmp = if given.is_some() { None } else { Some(tempfile::tempdir().unwrap()) };
    let root = match (&given, &tmp) {
        (Some(s), _) => PathBuf::from(s),
        (None, Some(t)) => t.path().to_path_buf(),
        (None, None) => unreachable!(),
    };
    let work = root.join(&project);
    if given.is_none() {
        fs::create_dir_all(root.join(".twosync")).unwrap();
        fs::create_dir_all(&work).unwrap();
    }
    // 样本量：小站上一次 `plan` 是毫秒级，31 次不占什么；12 GiB 那种站点一次就是秒级，
    // 31 次要占十分钟盘头 ⇒ 只读台架默认降到 7（表里仍给 min/中位/max，只是尾延迟更不准）。
    let samples = std::env::var("TWOSYNC_PERF_SAMPLES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n >= 3)
        .unwrap_or(if read_only { 7 } else { SAMPLES });

    // 绕开 `open_store`：那一条带着 FR-21 闸，`bitcask`/`memory` 会被挡在测量之前。
    // 这一层的任务恰恰是"给四个引擎都拿到数,并且把闸门结论原样列在旁边"。
    let raw = StorageConfig {
        kind: common::by_name(&engine),
        data_dir: db_dir(&root),
        engine: EngineConfig::default(),
    }
    .open()
    .unwrap_or_else(|e| panic!("{engine} 连开都开不起来：{e}"));
    let gate = check_durability_gate(&*raw).err().map(|e| e.to_string());
    let meter = Meter::default();
    let db = counted(raw, &meter, false);
    let pid = keys::project_id(&project);
    let store = Cas::new(&twosync::utils::objects_dir(&root), BLOCK).unwrap();

    let files = if read_only { 0 } else { write_payload(&work) };
    // 只读台架多出来的这一列是"这一趟没测/测的是什么"的账，表下面原样打印。
    let mut notes: Vec<String> = Vec::new();
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

    // `log` 与 `checkout` 要有 head 可走。小站里 head 是台架自己刚建的那一笔；
    // 现成站点里 head 必须是**站点自己的** `main` —— 这一层的任务是量现成的历史，不是替它造历史。
    let mut heads: Vec<keys::Hash> = Vec::new();
    if read_only {
        heads = build::ref_heads(&*db, pid, build::MAIN).unwrap();
        if heads.is_empty() {
            panic!(
                "站点 {root:?} 的项目 `{project}` 的 `main` 上没有 head ⇒ 只读台架没历史可走，\
                 换一个提交过的站点（或先 `--project {project} commit`）"
            );
        }
        notes.push(format!(
            "站点      = {root:?} / 项目 = `{project}` / `main` 上 {} 个 head（盘型按路径认：本机 C: = SSD、D: = 5400 转机械）",
            heads.len()
        ));
        // 台架里的 `BLOCK` 只参与**写**（每个 blob 的清单自己记着它的块大小，见 `cli::config`
        // 模块头那条"换 `block_size` 为什么不动已有数据"），
        // 只读模式一个字不写 ⇒ 这一列与块大小无关，别说成"这站点是 4 KiB 块"。
        notes.push(
            "块大小    = 与本台架无关 ⇒ `Cas` 的 `block_size` 只在写块时用，这一趟只读，站点里每个 blob 按自己清单里那个数存"
                .to_string(),
        );
    }

    if !read_only {
        measure("scan 全量", &mut || {
            worktree::scan(&*db, pid, &work, false, 2000).unwrap();
        });
    }
    if !read_only {
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
            heads = vec![built.commit.expect("首次提交必有改动")];
        });
    }
    measure("plan(只读)", &mut || {
        let p = build::plan(&*db, pid).unwrap();
        assert!(!p.changes.is_empty() || p.entries > 0, "plan 读不到东西，测的是空气");
        if read_only {
            notes.push(format!(
                "plan 的账  = {} 条在册 / 已扫描未提交 {} 项（+{} ~{} -{} 改名 {}）",
                p.entries,
                p.known,
                p.changes.add,
                p.changes.modify,
                p.changes.delete,
                p.changes.rename
            ));
        }
    });
    measure("log 全史", &mut || {
        let n = history::walk(&*db, pid, &heads, 0).unwrap();
        assert!(!n.is_empty());
        if read_only {
            notes.push(format!("log 走到的 = {} 笔 commit（这一列的形状 = 历史深度 × 每笔的亲数）", n.len()));
        }
    });
    measure("checkout 对照同一版", &mut || {
        let r = checkout::checkout(
            &*db,
            pid,
            &store,
            &work,
            &heads[0],
            None,
            checkout::Opts {
                apply: false,
                allow_mass_delete: false,
            },
        );
        match r {
            Ok(rep) => {
                if read_only {
                    // 现成站点的工作树**允许是脏的**（这就是它被人用过的证据），所以这里不断言空，
                    // 只把报到的账列出来 —— 那一趟读的是整棵子树，墙钟与折数照量。
                    notes.push(format!(
                        "checkout    = 对 head 报出 建 {} 覆盖 {} 删 {}，工作树脏 {} 条 ⇒ 只报未落地（FR-7）",
                        rep.create, rep.overwrite, rep.delete, rep.dirty
                    ));
                } else {
                    assert!(rep.is_noop(), "同一版比对同一版本该是空的，实际 {rep:?}");
                }
            }
            Err(e) => notes.push(format!(
                "checkout    = 这一趟被闸挡了（{e}）⇒ 墙钟与折数还是量到了，但那是**挡之前**的一半"
            )),
        }
    });
    if read_only {
        // **这一档在现成站点上整段跳过**：不带 `--apply` 的 `gc --sweep` 也不是只读 —— 它会驱动
        // 引擎自己的 `vacuum` / `vacuum_auto_slice`（10-20 实测：12 GiB 测量台的 `rows` 从
        // 191 672 掉到 84 258，动的不是 mark-sweep 那一半）。把被测对象改小之后再量它，
        // 这一列和上一列就不在同一台机器上了。小站那边照旧跑：那张表要的正是"这条命令付几次门面调用"。
        notes.push(
            "gc --sweep(dry) = **跳过** ⇒ dry-run 不是只读，它会驱动引擎的 auto-vacuum 把库改小，\
             量完再量就不等价（§10.8 第二层那格的同一教训）"
                .to_string(),
        );
    } else {
        measure("gc --sweep(dry)", &mut || {
            let p = gc::sweep(&*db, &store, pid, true).unwrap();
            assert_eq!(p.commits_dropped, 0, "head 可达的历史不许被判成垃圾：{p:?}");
        });
    }
    if !read_only {
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
            heads = vec![built.commit.expect("改了就该有新 commit")];
        });
    }

    // ── 延迟分位：把最常被周期叫的那条只读命令重复 `samples` 次 ──
    let mut lat: Vec<Duration> = Vec::with_capacity(samples);
    for _ in 0..samples {
        let t = Instant::now();
        let _ = build::plan(&*db, pid).unwrap();
        lat.push(t.elapsed());
    }
    lat.sort();

    let h = db.health();
    let dbs = dir_size(&db_dir(&root));
    let objs = dir_size(&twosync::utils::objects_dir(&root));
    let rss = allocstats::peak_rss();

    println!("=== §10.8 引擎性能表 · {engine} ===");
    if read_only {
        println!(
            "负载      = **现成站点，只读** ⇒ `scan` / `commit` / `gc --sweep` 三段整段跳过，\
             `log` 与 `checkout` 走站点自己的 head"
        );
    } else {
        println!(
            "负载      = {files} 条路径 / {} MiB 原始 / {} 块（block {} B，定长）",
            files * FILE_KIB / 1024,
            files * FILE_KIB * 1024 / BLOCK as usize,
            BLOCK
        );
    }
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
    for n in &notes {
        println!("{n}");
    }
    let pct = |q: f64| -> f64 {
        let i = ((samples as f64 - 1.0) * q).round() as usize;
        lat[i].as_secs_f64() * 1000.0
    };
    println!(
        "{:<22} min {:>6.1} / 中位 {:>6.1} / p95 {:>6.1} / max {:>6.1} ms（{samples} 次，尾延迟要更多样本才读得出）",
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
        "口径      = {}；release 构建；单线程 ⇒ CPU 秒 ≈ 墙钟秒；\
         墙钟 run 间 ±2× ⇒ 只用来在同一次运行内比较，不当跨 run 证据",
        if read_only {
            "站点由 `TWOSYNC_PERF_ROOT` 给（盘型按路径认：本机 C: = SSD、D: = 5400 转机械）"
        } else {
            "站点在 `%TEMP%`（本机 = C: SSD）"
        }
    );
}
