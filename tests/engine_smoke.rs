//! 存储基座的实机验证（FR-3 / FR-21 / §9.6）。
//!
//! 这些用例的存在理由不是"覆盖率"，而是 §9.6 那六条约束**照 0.4.0 的行为写进文档**了；
//! 引擎改版时这里是第一个变红的地方。

use std::path::Path;

use storage_engines::engine::{EngineKind, KvEngine, open_engine};
use twosync::{StoreError, open_store};

fn open() -> (Box<dyn KvEngine>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db =
        open_engine(EngineKind::LsmTree, Some(Path::new(dir.path()))).expect("open lsm_tree");
    (db, dir)
}

#[test]
fn engine_opens_and_roundtrips() {
    let (db, _dir) = open();
    let tx = db.begin();
    assert!(tx.set(b"C\x00\x00\x00\x01hello", b"world".to_vec()), "set 被拒 = 写冲突");
    assert_eq!(tx.get(b"C\x00\x00\x00\x01hello").as_deref(), Some(&b"world"[..]));
    tx.commit();
}

/// §9.6.1：扫描返回物化 `Vec`，每行两个堆分配 ⇒ 分页是硬要求，不是优化项。
#[test]
fn prefix_scan_is_materialized_so_windows_are_required() {
    const N: u32 = 500;
    let (db, _dir) = open();
    {
        let tx = db.begin();
        for i in 0..N {
            assert!(tx.set(format!("W\x00\x00\x00\x01p{i:05}").as_bytes(), vec![0u8; 32]));
        }
        tx.commit();
    }
    let tx = db.begin();
    let hit = tx.prefix_scan(b"W\x00\x00\x00\x01");
    assert_eq!(hit.len(), N as usize);
    assert_eq!(hit.first().unwrap().key, format!("W\x00\x00\x00\x01p00000").into_bytes());
    tx.commit();
}

/// §9.6.2：`active_txns > 0` 会让引擎 vacuum 整轮跳过 ⇒ 事务不许跨阶段持有。
#[test]
fn open_txn_is_visible_in_health_and_closed_after_drop() {
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
    let dir = tempfile::tempdir().expect("tempdir");
    for kind in [EngineKind::LsmTree, EngineKind::BPlusTree] {
        let db = open_store(dir.path(), kind).unwrap_or_else(|e| panic!("{kind} 应过闸: {e}"));
        assert_eq!(db.name(), kind.as_str());
    }
}

/// FR-21：bitcask 缺数据记录校验和、memory 无事务 WAL，两者都必须被拒——**不许静默降级**。
#[test]
fn startup_gate_rejects_engines_that_cannot_prove_durability() {
    let dir = tempfile::tempdir().expect("tempdir");
    for kind in [EngineKind::Bitcask, EngineKind::Memory] {
        let err = open_store(dir.path(), kind)
            .err()
            .unwrap_or_else(|| panic!("{kind} 竟通过了 FR-21 自检"));
        match err {
            StoreError::DurabilityGate { engine, gaps } => {
                assert_eq!(engine, kind.as_str());
                assert!(!gaps.is_empty(), "缺口必须被说清楚");
            }
            other => panic!("期望 DurabilityGate，实际 {other:?}"),
        }
    }
}
