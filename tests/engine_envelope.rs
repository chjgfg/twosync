//! 引擎「值信封」边界的实测（storage-engines README 已知局限第 7 条）。
//!
//! 门面 `set` 把 value 当**已打信封**的逻辑值存，`get` 一律剥标签：
//! `[0x00] 用户字节` = 永久；`[0x01] expire(u64 LE) 用户字节` = 带 TTL。
//! 于是**裸字节**打头是 0x00/0x01 时会被吃掉 1 或 9 字节，甚至被判成"已过期"读成 None。
//!
//! 这条用例不是给引擎写的装饰：twosync 存的是 postcard 编码的结构体，
//! **第一个字节常常就是枚举的变体号**（`Kind::File = 0`、`Kind::Dir = 1`），
//! 也就是说这个坑对我们是**默认命中**的，不是边缘情况。

use storage_engines::EngineKind;
use twosync::store::keys;
use twosync::store::keys::Kind;
use twosync::store::worktree::WtEntry;
use twosync::utils::open_store;

/// 存进去再读回来，返回 `get` 的原始结果。
fn roundtrip(value: &[u8]) -> Option<Vec<u8>> {
    let site = tempfile::tempdir().unwrap();
    let db = open_store(site.path(), EngineKind::LsmTree).unwrap();
    {
        let tx = db.begin();
        assert!(tx.set(b"k", value.to_vec()), "set 返回 false = 写冲突");
        tx.commit();
    }
    let tx = db.begin();
    let got = tx.get(b"k");
    tx.commit();
    got
}

/// 把边界**量出来**并打印：一旦引擎改了行为，这里就会变。
#[test]
fn value_envelope_boundary_is_measured_not_assumed() {
    let mut broken = Vec::new();
    for tag in 0u8..=4 {
        let value: Vec<u8> = std::iter::once(tag)
            .chain(std::iter::repeat_n(0xABu8, 40))
            .collect();
        match roundtrip(&value) {
            None => broken.push(format!("0x{tag:02x} → 读成 None")),
            Some(back) if back != value => broken.push(format!(
                "0x{tag:02x} → 长度 {} 应为 {}",
                back.len(),
                value.len()
            )),
            Some(_) => {}
        }
    }
    println!("打头字节受损清单：{broken:?}");
    // 不断言"全都好"——那是引擎的事，我们改不动。真正属于我们的守卫在下面两条。
}

/// **属于我们的**不变量：任何写进引擎的 value 都先过 `keys::wrap`，所以首字节永远是
/// `VALUE_TAG`（既不是 0x00 也不是 0x01），与内部字段的枚举编号无关。
#[test]
fn twosync_values_never_start_with_envelope_tags() {
    for kind in [Kind::File, Kind::Dir, Kind::Symlink] {
        let e = WtEntry {
            kind,
            blob_hash: [7u8; 32],
            size: 1,
            mtime_ns: 2,
            mode: 3,
            symlink_target: None,
            scanned_at: 4,
        };
        let raw = postcard::to_allocvec(&e).unwrap();
        // 裸编码确实会踩坑（File 首字节 0x00、Dir 0x01）——这正是要 wrap 的理由。
        let bytes = keys::wrap(raw);
        assert_eq!(bytes[0], keys::VALUE_TAG, "wrap 之后首字节必须是标签");
        assert!(bytes[0] != 0x00 && bytes[0] != 0x01, "标签本身不许撞信封");
        assert_eq!(
            keys::strip(&bytes).unwrap().to_vec(),
            postcard::to_allocvec(&e).unwrap()
        );
    }
}

/// 端到端：wrap 之后的值真存真读，三种 kind 都必须一字不差回来。
#[test]
fn wt_entry_roundtrips_through_the_engine() {
    for kind in [Kind::File, Kind::Dir, Kind::Symlink] {
        let e = WtEntry {
            kind,
            blob_hash: [9u8; 32],
            size: 4096,
            mtime_ns: 1_700_000_000_000_000_000,
            mode: 0o100644,
            symlink_target: None,
            scanned_at: 1,
        };
        let bytes = keys::wrap(postcard::to_allocvec(&e).unwrap());
        assert_eq!(
            roundtrip(&bytes).as_deref(),
            Some(bytes.as_slice()),
            "kind={kind:?} 的 wrap 值往返不符，说明引擎信封行为与 README 记录不一致"
        );
    }
}

/// 上游复现件：`set` 与 `batch_set` 对同一个裸字节的处理**不一致**。
///
/// 实测（storage-engines 0.4.0）：同一个 `[0x00, 0xAB*40]`，`set` 写进去读回来少 1 字节，
/// `batch_set` 写进去读回来完好。twosync 已经用 `keys::wrap` 让自己不依赖这个行为，
/// 所以这条默认忽略，留作引擎侧定论时的复现脚本：
///   cargo test --test engine_envelope -- --ignored
/// 不一致就意味着：扫描写进去的值和探针写进去的值行为不同，而两者走的是不同代码路径。
#[test]
#[ignore = "上游复现件：storage-engines 的 set 与 batch_set 信封行为不一致，待其定论"]
fn set_and_batch_set_agree_on_raw_bytes() {
    for tag in [0x00u8, 0x01, 0x02] {
        let value: Vec<u8> = std::iter::once(tag)
            .chain(std::iter::repeat_n(0xABu8, 40))
            .collect();
        let site = tempfile::tempdir().unwrap();
        let db = open_store(site.path(), EngineKind::LsmTree).unwrap();

        let via_set = {
            let tx = db.begin();
            assert!(tx.set(b"one".as_slice().to_vec().as_slice(), value.clone()));
            tx.commit();
            let tx = db.begin();
            let g = tx.get(b"one");
            tx.commit();
            g
        };
        let via_batch = {
            let tx = db.begin();
            tx.batch_set(&[(b"two".to_vec(), value.clone())]).unwrap();
            tx.commit();
            let tx = db.begin();
            let g = tx.get(b"two");
            tx.commit();
            g
        };
        assert_eq!(
            via_set, via_batch,
            "tag=0x{tag:02x}：两条写路径读回来的值不一样"
        );
    }
}
