//! 门面调用计数器这把尺子**自己**要先被验（§17 补记廿六）。
//!
//! 跑法：`cargo test --test utils_metrics`。
//!
//! # 为什么这一族用例必须存在
//!
//! 从这一版起，"我把它变快了"这种话只能用**折全库的次数**来说（§15.22：本机墙钟 run 间 ±2×，
//! 计时在这个粒度上不算证据）。那么计数器本身如果数错，后面所有结论都是空的——它和被它量的代码
//! 一样需要测试。这里验四件事：
//! 1. **包装不改行为**（转发是透明的：读写、删除、`exists`、前缀扫、版本链都照原样）；
//! 2. **计数与调用一一对应**（读三类各一次 ⇒ `folds() = 3`；每开一个事务收尾一次 ⇒ `txn_end` 对得上）；
//! 3. **写与诊断各进各的格子**，不混进读账里（`write` / `admin`）；
//! 4. **把手不串账**：两个 `Meter` 包两个引擎，读数互不影响（这是 `since()` 能用的前提）。
//!
//! # 一条被验出来的事实（不是本文件的重点，但记在这儿最顺手）
//!
//! `storage_engines` 0.4.0 的 `commit()` **不看写集空不空**：只读事务一样会 append 一条
//! Commit 记录并 `wal.sync()`（`lsm_tree_mvcc.rs` 的 `commit_with_options`）。
//! 所以 `txn_end` 这一格不是"事务数"这种装饰性指标，它就是**这条命令等了几次盘的下界**。

use storage_engines::{EngineKind, KvEngine};
use twosync::utils::{CallCounts, Meter, counted, open_store};

fn counted_store(meter: &Meter) -> (Box<dyn KvEngine>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    // `open_store` 会过 FR-21 自检闸，所以这里拿到的就是命令层真正在用的那种句柄。
    // 临时目录必须**跟着句柄一起活着**：`TempDir` 一 drop 就把库删了（这也是上面那个
    // "先拿把手再包"的写法要自己管生命周期的原因）。
    let inner = open_store(tmp.path(), EngineKind::LsmTree).expect("open_store");
    // announce = false：用例里不要往 stderr 刷账。
    (counted(inner, meter, false), tmp)
}

/// 用一个临时目录把引擎"养住"：`tempfile::TempDir` 一 drop 就把目录删了。
struct Harness {
    db: Box<dyn KvEngine>,
    meter: Meter,
    _tmp: tempfile::TempDir,
}

impl Harness {
    fn new() -> Harness {
        let tmp = tempfile::tempdir().expect("tempdir");
        let inner = open_store(tmp.path(), EngineKind::LsmTree).expect("open_store");
        let meter = Meter::default();
        let db = counted(inner, &meter, false);
        Harness {
            db,
            meter,
            _tmp: tmp,
        }
    }

    fn counts(&self) -> CallCounts {
        self.meter.snapshot()
    }
}

/// ① 转发必须是**透明**的：这条不通过，后面所有计数都毫无意义（我们量的就该是引擎本来的行为）。
#[test]
fn the_wrapper_does_not_change_what_the_engine_does() {
    let h = Harness::new();
    let tx = h.db.begin();
    assert!(tx.set(b"k1", b"v1".to_vec()));
    assert!(tx.set(b"k2", b"v2".to_vec()));
    tx.commit();

    let tx = h.db.begin();
    assert_eq!(tx.get(b"k1").as_deref(), Some(&b"v1"[..]), "点读要原样读得到");
    assert!(tx.exists(b"k2"), "exists 得照实");
    assert_eq!(tx.prefix_scan(b"k").len(), 2, "前缀扫照实");
    tx.commit();

    let tx = h.db.begin();
    assert!(tx.delete(b"k1"));
    tx.commit();
    let tx = h.db.begin();
    assert_eq!(tx.get(b"k1"), None, "删除之后读不到才算透明");
    assert_eq!(tx.prefix_scan(b"k").len(), 1);
    tx.commit();

    assert_eq!(h.db.name(), "lsm_tree", "名字都得是里面那个引擎的");
    assert!(h.db.path().is_some(), "path 也得转出去（`doctor` 靠它报库在哪）");
}

/// ② 读三类各调用一次 ⇒ `folds() = 3`，而**每个事务都收尾一次** ⇒ `txn_end` 对得上。
/// 这一条钉的是"计数器不数漏也不数重"。
#[test]
fn reads_are_counted_one_to_one_and_every_txn_is_closed() {
    let h = Harness::new();
    let tx = h.db.begin();
    tx.set(b"a", b"1".to_vec());
    tx.commit();
    let base = h.counts();

    let tx = h.db.begin();
    tx.get(b"a"); // 点读
    tx.prefix_scan(b"a"); // 扫描
    tx.key_count(b"a"); // 也是扫描一类
    tx.commit();

    let got = h.counts().since(base);
    assert_eq!(got.point_read, 1, "点读只调了一次");
    assert_eq!(got.scan, 2, "两次扫描类调用");
    assert_eq!(got.folds(), 3, "**折全库的次数**就是这把尺子的读数");
    assert_eq!(got.write, 0, "基线之后只有读 ⇒ 写那一格不该动（上面那次 `set` 在基线之前）");
    assert_eq!(got.txn_end, 1, "一个事务一次收尾");
    // 绝对账也核一次：第一笔 `set` 确实被数进了写格（不然 `write` 这格就是空的假象）
    assert_eq!(h.counts().write, 1, "{:?}", h.counts());

    // 只读事务也算一次收尾：引擎的 `commit()` 不看写集空不空，照样 sync WAL。
    let before = h.counts().txn_end;
    let tx = h.db.begin();
    assert_eq!(tx.get(b"zzz"), None);
    tx.commit();
    assert_eq!(h.counts().txn_end - before, 1, "只读 commit 也被数到（它同样等一次盘）");
}

/// ③ 写与诊断各进各格：`health()`/`vacuum()` 是 O(全库) 但**属于诊断**，
/// 把它们混进读账会让人以为"这条命令读了一百遍库"，而那其实是 `doctor` 允许做的事（NFR-8）。
#[test]
fn writes_and_admin_ops_get_their_own_buckets() {
    let h = Harness::new();
    let tx = h.db.begin();
    for i in 0..5 {
        tx.set(&[b'w', i], vec![i]);
    }
    tx.batch_set(&[(b"x".to_vec(), b"y".to_vec())]).expect("batch_set");
    tx.commit();

    let c = h.counts();
    assert_eq!(c.write, 6, "五次 set + 一次 batch_set");
    assert_eq!(c.folds(), 0, "纯写路径不该有折数");
    assert_eq!(c.txn_end, 1);

    let h2 = Harness::new();
    let _ = h2.db.health();
    h2.db.vacuum().expect("vacuum");
    let c2 = h2.counts();
    assert_eq!(c2.admin, 2, "health + vacuum 各一次，算在诊断格");
    assert_eq!(c2.folds(), 0, "诊断不许伪装成读路径");
    assert_eq!(c2.txn_end, 0, "health/vacuum 不开事务，别给事务数掺水");
}

/// ④ 两个把手互不串账——这是 `since()`（"这一趟花了多少"）能用的前提。
#[test]
fn two_meters_do_not_share_a_ledger() {
    let m1 = Meter::default();
    let m2 = Meter::default();
    let mut dbs = Vec::new();
    let mut kept = Vec::new();
    for m in [&m1, &m2] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let inner = open_store(tmp.path(), EngineKind::LsmTree).expect("open_store");
        dbs.push(counted(inner, m, false));
        kept.push(tmp);
    }

    let tx = dbs[0].begin();
    tx.get(b"nothing");
    tx.commit();
    let tx = dbs[1].begin();
    for _ in 0..3 {
        tx.prefix_scan(b"nothing");
    }
    tx.commit();

    assert_eq!(m1.snapshot().folds(), 1, "第一个把手只该数到自己那一次");
    assert_eq!(m2.snapshot().folds(), 3, "第二个把手不该看到第一个的账");
    // `since` 是**逐格 saturating 相减**：减别人更大的账会得到 0，而不是下溢成一个吓人的大数；
    // 自己被数到的那一格照实留下。这样才能安全地写 `snapshot().since(before)` 当"这一趟的账"。
    let cross = m1.snapshot().since(m2.snapshot());
    assert_eq!(cross.point_read, 1, "m1 的 1 次点读没人可减");
    assert_eq!(cross.scan, 0, "0 - 3 走 saturating，不许回绕成 u64 的大数");

    // 把手在句柄之后 drop 也不炸：计数住在 `Arc` 里，不依赖引擎活着
    let (db, _tmp) = counted_store(&m1);
    drop(db);
    let _ = m1.snapshot();
}
