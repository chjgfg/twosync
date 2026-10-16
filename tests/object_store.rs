//! 对象层：tree / commit 的规范编码、幂等写入、以及"索引不许漂"。
//!
//! 这里最要紧的一条是 `tree_hash_is_canonical` 里那个字面量：它钉的是**磁盘格式**。
//! 哪天有人改了字段顺序、排序规则或编码库，老库里的 tree hash 会全体对不上，
//! 而所有单元测试仍然会绿（两边一起变）——只有这个钉子会红。

use storage_engines::EngineKind;
use twosync::error::Error;
use twosync::store::keys::{self, Kind, ProjectId};
use twosync::store::object::{
    self, Commit, Stats, TreeEntry, empty_tree, parents_of, put_commit, put_tree, read_commit,
    read_tree,
};
use twosync::utils::open_store;

const PID: ProjectId = 7;

fn entry(name: &str, kind: Kind, seed: u8, size: u64, mode: u32) -> TreeEntry {
    TreeEntry {
        name: name.into(),
        kind,
        hash: [seed; 32],
        size,
        mode,
    }
}

fn db() -> (tempfile::TempDir, Box<dyn storage_engines::KvEngine>) {
    let site = tempfile::tempdir().unwrap();
    let engine = open_store(site.path(), EngineKind::LsmTree).unwrap();
    (site, engine)
}

#[test]
fn tree_hash_is_canonical() {
    let a = vec![
        entry("src", Kind::Dir, 1, 0, 0o40755),
        entry("README.md", Kind::File, 2, 17, 0o100644),
    ];
    let mut b = a.clone();
    b.reverse();
    assert_eq!(
        object::tree_hash(&a),
        object::tree_hash(&b),
        "同一组条目换个插入顺序，hash 必须一样"
    );

    // 钉死磁盘格式：变了就得升 chunk_param/格式版本，不许悄悄改。
    assert_eq!(
        keys::hex(&object::tree_hash(&a)),
        // 钉死磁盘格式：变了就得升格式版本，不许悄悄改。
        "d22411059e7f0e1c58f9687e1960096218214a5cfaae5956e1b6113edd2de6cc"
    );
}

#[test]
fn empty_tree_is_shared_and_readable() {
    let (_site, engine) = db();
    let hash = put_tree(&*engine, PID, &[]).unwrap();
    assert_eq!(hash, empty_tree());
    assert!(
        read_tree(&*engine, PID, &hash).unwrap().is_empty(),
        "空目录不必落盘也该能读"
    );
    assert_eq!(engine.health().total_rows, 0, "空目录不该产生任何行");
}

#[test]
fn read_tree_roundtrips_and_rejects_duplicate_names() {
    let (_site, engine) = db();
    let entries = vec![
        entry("a.txt", Kind::File, 9, 3, 0o100644),
        entry("sub", Kind::Dir, 8, 0, 0o40755),
        entry("link", Kind::Symlink, 7, 0, 0o120777),
    ];
    let hash = put_tree(&*engine, PID, &entries).unwrap();
    let back = read_tree(&*engine, PID, &hash).unwrap();
    assert_eq!(back.len(), 3);
    assert_eq!(back[0].name, "a.txt", "读回来必须已按 name 字节序排列");
    assert_eq!(back[2].name, "sub");
    let mut want = entries;
    want.sort();
    assert_eq!(back, want, "内容必须一字不差（顺序按 name 字节序）");

    let dup = vec![
        entry("x", Kind::File, 1, 1, 0),
        entry("x", Kind::Dir, 2, 0, 0),
    ];
    assert!(
        matches!(put_tree(&*engine, PID, &dup), Err(Error::Codec(_))),
        "重名必须拒"
    );
}

/// §8 的承诺：可执行位要能同步 ⇒ 它必须进 hash。
#[test]
fn mode_change_changes_tree_hash() {
    let plain = vec![entry("run.sh", Kind::File, 4, 12, 0o100644)];
    let exec = vec![entry("run.sh", Kind::File, 4, 12, 0o100755)];
    assert_ne!(object::tree_hash(&plain), object::tree_hash(&exec));
}

#[test]
fn commit_roundtrips_with_parent_order() {
    let (_site, engine) = db();
    let p1 = [0x11u8; 32];
    let p2 = [0x22u8; 32];
    let tree = put_tree(&*engine, PID, &[entry("f", Kind::File, 5, 1, 0o100644)]).unwrap();
    let c = Commit {
        parents: vec![p1, p2],
        tree,
        peer: "win-01".into(),
        ts: 1_700_000_000,
        msg: "sync: 1 add".into(),
        stats: Stats {
            add: 1,
            modify: 0,
            delete: 0,
            rename: 0,
            bytes: 1,
        },
    };
    let hash = put_commit(&*engine, PID, &c).unwrap();
    assert_eq!(read_commit(&*engine, PID, &hash).unwrap(), c);
    assert_eq!(
        parents_of(&*engine, PID, &hash).unwrap(),
        vec![p1, p2],
        "ordinal 顺序即 parents 顺序"
    );
}

/// 不变量 (g) 的雏形：`C` 体里的 parents 与 `c` 索引必须一致。
#[test]
fn parents_index_matches_commit_body() {
    let (_site, engine) = db();
    let tree = empty_tree();
    for n in 0..3u8 {
        let c = Commit {
            parents: (0..=n).map(|i| [i; 32]).collect(),
            tree,
            peer: "u".into(),
            ts: 0,
            msg: String::new(),
            stats: Stats {
                add: 0,
                modify: 0,
                delete: 0,
                rename: 0,
                bytes: 0,
            },
        };
        let hash = put_commit(&*engine, PID, &c).unwrap();
        assert_eq!(parents_of(&*engine, PID, &hash).unwrap(), c.parents);
    }
}

/// 幂等：重写同一棵 tree / 同一个 commit 不该再产生版本行。
/// 这条守着模块头那句"10 万目录 × 1000 commit = 1 亿行垃圾"。
#[test]
fn re_put_creates_no_extra_rows() {
    let (_site, engine) = db();
    let entries = vec![
        entry("a", Kind::File, 1, 1, 0),
        entry("b", Kind::File, 2, 1, 0),
    ];
    let hash = put_tree(&*engine, PID, &entries).unwrap();
    let rows_after_first = engine.health().total_rows;

    for _ in 0..5 {
        assert_eq!(put_tree(&*engine, PID, &entries).unwrap(), hash);
    }
    assert_eq!(
        engine.health().total_rows,
        rows_after_first,
        "重复写同一内容不该长版本行"
    );
}

/// 缺失与损坏是两件事（不许混为一谈，否则磁盘损坏会被当成"再传一次"）。
#[test]
fn missing_object_is_reported_as_missing() {
    let (_site, engine) = db();
    let ghost = [0xABu8; 32];
    let err = read_tree(&*engine, PID, &ghost).expect_err("不存在的 tree 必须报错");
    assert!(
        matches!(err, Error::MissingObject { .. }),
        "应是缺失，实际 {err}"
    );
}
