//! 对象层：tree / commit 的规范编码、幂等写入、以及"索引不许漂"。
//!
//! 这里最要紧的一条是 `tree_hash_is_canonical` 里那个字面量：它钉的是**磁盘格式**。
//! 哪天有人改了字段顺序、排序规则或编码库，老库里的 tree hash 会全体对不上，
//! 而所有单元测试仍然会绿（两边一起变）——只有这个钉子会红。

use storage_engines::EngineKind;
use twosync::error::Error;
use twosync::store::keys::{self, Hash, Kind, ProjectId};
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

// ── 批量装载器 `TreeIndex`（§11.4d：把 O(目录数) 次点读换成一次前缀扫）──────────────
// 这三条是这次改写的**安全带**：装载器接管了原来 `read_tree` 的位置，所以它必须
// ①给出与点读完全一样的答案，②在超预算时退回点读而不丢答案，③保留同一次 hash 自证。

/// 造一棵三层树（外加一个空目录），把清单都落库，返回根 hash。
///
/// 层是真的层（根 → a → sub → leaf），因为要对照的正是"逐层点读"与"一次装载"；
/// `empty` 那条指向 `empty_tree()`——空目录从不落盘，装载器对它的特判也在这一条里被走到。
fn three_level_site(engine: &dyn storage_engines::KvEngine) -> Hash {
    let leaf = put_tree(engine, PID, &[entry("z.bin", Kind::File, 3, 5, 0o100644)]).unwrap();
    let sub = put_tree(
        engine,
        PID,
        &[TreeEntry {
            name: "inner".into(),
            kind: Kind::Dir,
            hash: leaf,
            size: 0,
            mode: 0o40755,
        }],
    )
    .unwrap();
    let a = put_tree(
        engine,
        PID,
        &[
            TreeEntry {
                name: "inner".into(),
                kind: Kind::Dir,
                hash: sub,
                size: 0,
                mode: 0o40755,
            },
            entry("note", Kind::File, 4, 9, 0o100644),
        ],
    )
    .unwrap();
    put_tree(
        engine,
        PID,
        &[
            TreeEntry {
                name: "a".into(),
                kind: Kind::Dir,
                hash: a,
                size: 0,
                mode: 0o40755,
            },
            TreeEntry {
                name: "empty".into(),
                kind: Kind::Dir,
                hash: empty_tree(),
                size: 0,
                mode: 0o40755,
            },
            entry("README.md", Kind::File, 2, 17, 0o100644),
        ],
    )
    .unwrap()
}

#[test]
fn the_batched_manifests_answer_exactly_like_point_reads() {
    let (_site, engine) = db();
    let root = three_level_site(&*engine);
    let ix = object::TreeIndex::load(&*engine, PID).unwrap();
    assert!(!ix.over_budget(), "三层树几十字节，不该超预算");
    assert_eq!(ix.loaded(), ix.rows(), "预算内：搬回来的行必须都解出来了");
    assert!(ix.rows() >= 3, "至少 leaf/a/根 三张：{}", ix.rows());

    // 逐张对照：装载的答案 == 点读的答案（含根）。
    let mut checked = 0;
    for h in collect_hashes(&*engine, &root) {
        assert_eq!(
            ix.list(&*engine, PID, &h).unwrap(),
            read_tree(&*engine, PID, &h).unwrap(),
            "装载与点读给出不同的清单：{}",
            keys::hex(&h)
        );
        checked += 1;
    }
    assert!(checked >= 3, "对照的张数太少，等于没测：{checked}");

    // 空目录共享那一条 hash、且**从不落盘** ⇒ 装载器必须有同一个特判，否则整条路径报缺失。
    assert_eq!(ix.list(&*engine, PID, &empty_tree()).unwrap(), Vec::new());
    // 缺失在两条路上也必须是同一个错。
    let ghost = [0xABu8; 32];
    let err = ix.list(&*engine, PID, &ghost).unwrap_err();
    assert!(matches!(err, Error::MissingObject { .. }), "实际 {err}");
}

#[test]
fn over_budget_gives_the_answers_back_but_keeps_nothing() {
    let (_site, engine) = db();
    let root = three_level_site(&*engine);
    // 预算给 1 字节 ⇒ 第一行就超。这是"历史长到装不下"的等价类，不必真造 4 MiB。
    let ix = object::TreeIndex::with_budget(&*engine, PID, 1).unwrap();
    assert!(ix.over_budget(), "给了 1 字节预算必须标超");
    assert_eq!(ix.loaded(), 0, "超预算必须把已解出来的表交还——留着它就是违背 NFR-1");
    assert!(ix.rows() >= 3, "行数照实报，退回不等于没看见：{}", ix.rows());
    assert!(ix.bytes() >= 1);
    for h in collect_hashes(&*engine, &root) {
        assert_eq!(
            ix.list(&*engine, PID, &h).unwrap(),
            read_tree(&*engine, PID, &h).unwrap(),
            "退回点读那一路的答案必须与整段装载一致（降级只降速度，不降正确性）"
        );
    }
}

#[test]
fn the_batch_load_self_verifies_where_a_point_read_would() {
    let (_site, engine) = db();
    let root = three_level_site(&*engine);
    // 把根那一行的 value 换成**另一份合法编码**（能解码，但内容与 hash 不符）。
    let bogus: Vec<TreeEntry> = vec![entry("tampered", Kind::File, 9, 1, 0o100644)];
    let bytes = postcard::to_allocvec(&bogus).unwrap();
    let tx = engine.begin();
    assert!(tx.set(&keys::tree_key(PID, &root), keys::wrap(bytes)));
    tx.commit();

    // 点读那一路本来就报 CorruptBlock（这条是既有担保）；装载那一路**必须一样**。
    let one = read_tree(&*engine, PID, &root).unwrap_err();
    assert!(matches!(one, Error::CorruptBlock { .. }), "点读没拦住：{one}");
    let batch = object::TreeIndex::load(&*engine, PID).unwrap_err();
    assert!(
        matches!(batch, Error::CorruptBlock { .. }),
        "批量装载少了一次自证，实际 {batch}"
    );
}

/// 从根往下把 hash 收集起来（走点读，故意不用被测的那个装载器）。
fn collect_hashes(engine: &dyn storage_engines::KvEngine, root: &Hash) -> Vec<Hash> {
    let mut out = Vec::new();
    let mut stack = vec![*root];
    while let Some(h) = stack.pop() {
        out.push(h);
        for e in read_tree(engine, PID, &h).unwrap() {
            if e.kind == Kind::Dir && e.hash != empty_tree() {
                stack.push(e.hash);
            }
        }
    }
    out
}
