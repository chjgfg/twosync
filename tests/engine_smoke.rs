//! 存储基座的实机验证（FR-3 / FR-21 / §9.6）。
//!
//! 这些用例的存在理由不是"覆盖率"，而是 §9.6 那六条约束**照 0.4.0 的行为写进文档**了；
//! 引擎改版时这里是第一个变红的地方。
//!
//! 注意：`open_store` 必须从 `twosync::utils` 引，不许在测试里另写一份——之前它住在测试文件里，
//! 结果 FR-21 自检闸在产品代码里被 `let _ =` 丢掉，用例照样绿。

use std::path::Path;

use storage_engines::SearchQuery;
use storage_engines::engine::{EngineKind, KvEngine, open_engine};
use twosync::allocstats;
use twosync::error::Error;
use twosync::utils::open_store;

fn open() -> (Box<dyn KvEngine>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = open_engine(EngineKind::LsmTree, Some(Path::new(dir.path()))).expect("open lsm_tree");
    (db, dir)
}

/// **本文件所有用例都要先进这把锁。**
///
/// `allocstats` 那两个原子数（`CURRENT`/`PEAK`）是**进程级**的，而 cargo 默认让同一个测试
/// 二进制里的用例并行跑——两条线程互相把对方的峰值抬上去，那两条归因实验的比值断言就会
/// 随机红（我 2026-10-17 就这么踩到一次：单独跑绿、全跑红）。这不是保险丝，是这把尺的
/// 使用条件：**它量的是整个进程的堆，所以一次只能量一个**。
/// 跨文件不冲突：每个集成测试文件是独立进程，静态量各自一份。
static ALLOC_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    // 中毒也要继续：这里的 panic 来自别的用例，锁本身没有坏数据。
    ALLOC_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

#[test]
fn engine_opens_and_roundtrips() {
    let _g = serial();
    let (db, _dir) = open();
    let tx = db.begin();
    assert!(
        tx.set(b"C\x00\x00\x00\x01hello", b"world".to_vec()),
        "set 被拒 = 写冲突"
    );
    assert_eq!(
        tx.get(b"C\x00\x00\x00\x01hello").as_deref(),
        Some(&b"world"[..])
    );
    tx.commit();
}

/// §9.6.1：扫描返回物化 `Vec`。
///
/// **2026-10-17 更正过判据**：原来这里写的是"分页是硬要求，不是优化项"，前提是"每行的
/// 物化量随窗口走"。实测（见下面 `search_keys_folds_the_whole_db`）前提不成立：
/// `search_keys`/`prefix_scan` 每次调用都先把**整库**折叠一遍，窗口只决定最后那一片
/// 拷贝多少条。所以分页省下的是"每次调用返回物的尾部"，**没省**下每次调用那一次全库折叠
/// ⇒ 页数就是全库折叠次数，窗口该往**大**调（详见 §17 补记十）。
#[test]
fn prefix_scan_is_materialized_so_windows_are_required() {
    let _g = serial();
    const N: u32 = 500;
    let (db, _dir) = open();
    {
        let tx = db.begin();
        for i in 0..N {
            assert!(tx.set(
                format!("W\x00\x00\x00\x01p{i:05}").as_bytes(),
                vec![0u8; 32]
            ));
        }
        tx.commit();
    }
    let tx = db.begin();
    let hit = tx.prefix_scan(b"W\x00\x00\x00\x01");
    assert_eq!(hit.len(), N as usize);
    assert_eq!(
        hit.first().unwrap().key,
        "W\x00\x00\x00\x01p00000".as_bytes().to_vec()
    );
    tx.commit();
}

/// 归因实验**：读路径的峰值挂在整个库上，不挂在命中数上，也不挂在窗口上。
///
/// 两侧各塞 3 条 `W` 行（命中数、页数因此完全相同），一侧另外 200 条别的标签、另一侧
/// 8 000 条。如果"扫描物化的是命中那一页"，两侧峰值该差不多；实测差 40 倍。
/// 引擎侧的根据：`lsm_tree/kv_ops.rs::search_keys` → `collect_for_search` →
/// `collect_latest_raw`，而它第一步是 `scan_latest_light(u64::MAX)`（**全库**），
/// 第二步给 winners 里的**每一行**取值，之后才按 pattern 过滤、才切页。
///
/// 这一条是 §10.5 长跑与 NFR-1 目前最大的单项，也是给上游那条 patch 的根据：
/// `collect_latest_raw` 需要收 `[start, end)` 边界，或者至少"先按 pattern 筛 key、
/// 只给胜出的那一页取值"。
#[test]
fn search_keys_folds_the_whole_db() {
    let _g = serial();
    let (small, _d1) = open();
    fill(&*small, b'K', 200);
    fill(&*small, b'W', 3);
    let (big, _d2) = open();
    fill(&*big, b'K', 8_000);
    fill(&*big, b'W', 3);

    let (p_small, total_a, pages_a) = search_peak(&*small, 2_000);
    let (p_big, total_b, pages_b) = search_peak(&*big, 2_000);
    assert_eq!(
        (total_a, pages_a),
        (total_b, pages_b),
        "两侧命中数与页数必须一样，否则比的就不是同一件事"
    );
    // 窗口也缩到 100 条：命中仍是 3 行，峰值若跟着窗口走就该掉下来，不跟才说明物化的是整库。
    let (p_narrow, _, _) = search_peak(&*big, 100);

    println!("命中 {total_a} 行 / {pages_a} 页");
    println!("库   203 行 ⇒ 一次 search_keys 峰值 = {p_small:>9} B");
    println!("库 8 003 行 ⇒ 一次 search_keys 峰值 = {p_big:>9} B");
    println!("同样 8 003 行、窗口缩到 100 ⇒ {p_narrow:>9} B");
    assert!(
        p_big > p_small * 20,
        "峰值没跟着库的总大小走（{p_small} → {p_big}），判据要重写"
    );
    assert!(
        p_narrow * 2 > p_big,
        "窗口缩小 20 倍峰值却几乎没动（{p_big} → {p_narrow}）⇒ 全库折叠，分页挡不住内存"
    );
}

/// **写侧归因**：按扫描器的节奏（一事务一批、批 100）写 2 000 行，峰值跟"这个库本来有多大"
/// 走还是跟"这次写多少"走？
///
/// 问这个是因为现场量到 `scan` 在 22 000 行的站点上峰值 24.9 MiB，而把 `--batch` 从 100
/// 调到 20 000 只让它涨到 31.7 MiB——**那一层 22 MiB 的地板不是我们那个批缓冲**
/// （100 行 ≈ 15 KiB）。读侧已经证明是"每次调用折一遍全库"，这条问的是写侧有没有同一笔账。
///
/// 判据（实测）：同样写 2 000 行，库从 203 键涨到 20 203 键，峰值按**全库键数**涨
/// ⇒ 写侧与读侧同因（每次事务提交都在库里做一次全库级的活），`--batch` 不是内存旋钮。
#[test]
fn write_peak_tracks_the_whole_db_too() {
    let _g = serial();
    let (small, _d1) = open();
    fill(&*small, b'K', 200);
    let (p_small, _r) = write_batches(&*small, b'W', 2_000);
    let (big, _d2) = open();
    fill(&*big, b'K', 20_000);
    let (p_big, _r) = write_batches(&*big, b'W', 2_000);

    println!("空库起点   203 键 + 写 2000 行 ⇒ 峰值 {p_small:>9} B");
    println!("已有库 20 203 键 + 写 2000 行 ⇒ 峰值 {p_big:>9} B");
    assert!(
        p_big > p_small * 4,
        "写侧峰值没跟着全库大小走（{p_small} → {p_big}）⇒ 归因要改"
    );
}

/// 扫描器的写法：一事务一批、批 100，事务不许跨阶段持有（§9.6.2）。
fn write_batches(db: &dyn KvEngine, tag: u8, n: usize) -> (usize, usize) {
    let wm = allocstats::watermark();
    let mut written = 0;
    for chunk in 0..n.div_ceil(100) {
        let tx = db.begin();
        for i in (chunk * 100)..n.min((chunk + 1) * 100) {
            assert!(
                tx.set(
                    format!("{}y\x00\x00\x00\x01{i:07}", tag as char).as_bytes(),
                    vec![0u8; 32]
                ),
                "set 被拒 = 写冲突"
            );
            written += 1;
        }
        tx.commit();
    }
    (wm.peak_growth(), written)
}

/// 分批写：一个事务攒 4 万条会把事务自己的缓冲算进峰值，那是另一笔账（§9.6.2 也要求
/// 事务不许跨阶段持有）。
fn fill(db: &dyn KvEngine, tag: u8, n: usize) {
    for chunk in 0..n.div_ceil(5_000) {
        let tx = db.begin();
        for i in (chunk * 5_000)..n.min((chunk + 1) * 5_000) {
            assert!(
                tx.set(
                    format!("{}{}\x00\x00\x00\x01{i:07}", tag as char, 'x').as_bytes(),
                    vec![0u8; 32]
                ),
                "set 被拒 = 写冲突"
            );
        }
        tx.commit();
    }
}

fn search_peak(db: &dyn KvEngine, window: usize) -> (usize, usize, usize) {
    let wm = allocstats::watermark();
    let page = {
        let tx = db.begin();
        let page = tx.search_keys(&SearchQuery::prefix(b"Wx\x00\x00\x00\x01", 0, window));
        tx.commit();
        page
    };
    let peak = wm.peak_growth();
    (peak, page.total, page.total_pages)
}

/// §9.6.2：`active_txns > 0` 会让引擎 vacuum 整轮跳过 ⇒ 事务不许跨阶段持有。
#[test]
fn open_txn_is_visible_in_health_and_closed_after_drop() {
    let _g = serial();
    let (db, _dir) = open();
    assert_eq!(db.health().active_txns, 0, "开局不该有活跃事务");
    {
        let _tx = db.begin();
        assert_eq!(db.health().active_txns, 1, "持有时必须被 health 看见");
    }
    assert_eq!(db.health().active_txns, 0, "drop 后必须归零");
}

/// FR-21：过闸的引擎必须真能过（画像三要件齐全）。
#[test]
fn lsm_and_bplus_pass_the_startup_gate() {
    let _g = serial();
    let dir = tempfile::tempdir().expect("tempdir");
    for kind in [EngineKind::LsmTree, EngineKind::BPlusTree] {
        let db = open_store(dir.path(), kind).unwrap_or_else(|e| panic!("{kind} 应过闸: {e}"));
        assert_eq!(db.name(), kind.as_str());
    }
}

/// FR-21：bitcask 缺数据记录校验和、memory 无事务 WAL，两者都必须被拒——**不许静默降级**。
#[test]
fn startup_gate_rejects_engines_that_cannot_prove_durability() {
    let _g = serial();
    let dir = tempfile::tempdir().expect("tempdir");
    for kind in [EngineKind::Bitcask, EngineKind::Memory] {
        let err = open_store(dir.path(), kind)
            .err()
            .unwrap_or_else(|| panic!("{kind} 竟通过了 FR-21 自检"));
        match err {
            Error::DurabilityGate { engine, gaps } => {
                assert_eq!(engine, kind.as_str());
                assert!(!gaps.is_empty(), "缺口必须被说清楚");
            }
            other => panic!("期望 DurabilityGate，实际 {other:?}"),
        }
    }
}
