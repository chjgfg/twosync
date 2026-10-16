//! 只读历史层：`log` 的遍历、`show` 的路径解析、rev 前缀解析。
//!
//! 这一层的用例**全部手搓对象**（`put_tree`/`put_commit`/`set_ref`），不碰文件系统也不碰
//! CAS：读层唯一的依赖是引擎里的对象，把扫描和落块卷进来就等于用写层测写层——写层一坏，
//! 读层的用例跟着一起红，那就分不清是谁的问题了。真实链路的证明在 CLI 那一遍跑（模块末注释）。
//!
//! `fmt_ts` 的日历换算放在 `src/store/history.rs` 的单元测试里（那是纯函数，不必起引擎）。

use storage_engines::{EngineKind, KvEngine};
use twosync::error::Error;
use twosync::store::build;
use twosync::store::history;
use twosync::store::keys::{self, Hash, Kind, ProjectId};
use twosync::store::object::{self, Stats, TreeEntry};
use twosync::utils::open_store;

const PID: ProjectId = 11;

fn db() -> (tempfile::TempDir, Box<dyn KvEngine>) {
    let site = tempfile::tempdir().unwrap();
    let engine = open_store(site.path(), EngineKind::LsmTree).unwrap();
    (site, engine)
}

fn file(name: &str, seed: u8, size: u64) -> TreeEntry {
    TreeEntry {
        name: name.into(),
        kind: Kind::File,
        hash: [seed; 32],
        size,
        mode: 0o100644,
    }
}

fn dir(name: &str, tree: Hash) -> TreeEntry {
    TreeEntry {
        name: name.into(),
        kind: Kind::Dir,
        hash: tree,
        size: 0,
        mode: 0o40755,
    }
}

fn commit(db: &dyn KvEngine, pid: ProjectId, parents: &[Hash], tree: &Hash, ts: i64) -> Hash {
    object::put_commit(
        db,
        pid,
        &object::Commit {
            parents: parents.to_vec(),
            tree: *tree,
            peer: "t".into(),
            ts,
            msg: format!("ts={ts}"),
            stats: Stats {
                add: 1,
                modify: 0,
                delete: 0,
                rename: 0,
                bytes: 1,
            },
        },
    )
    .unwrap()
}

#[test]
fn walk_returns_commits_newest_first_and_honours_the_limit() {
    let (_site, engine) = db();
    let tree = object::put_tree(&*engine, PID, &[file("a", 1, 1)]).unwrap();
    let c1 = commit(&*engine, PID, &[], &tree, 100);
    let c2 = commit(&*engine, PID, &[c1], &tree, 200);
    let c3 = commit(&*engine, PID, &[c2], &tree, 300);

    let all = history::walk(&*engine, PID, &[c3], 0).unwrap();
    assert_eq!(
        all.iter().map(|n| n.hash).collect::<Vec<_>>(),
        vec![c3, c2, c1],
        "时间倒序，且一条不落"
    );
    assert_eq!(
        all[0].commit.ts, 300,
        "Node 必须连 commit 体一起给，否则调用方要再读一次"
    );

    let two = history::walk(&*engine, PID, &[c3], 2).unwrap();
    assert_eq!(two.iter().map(|n| n.hash).collect::<Vec<_>>(), vec![c3, c2]);
}

/// 菱形：两个 head 共享祖先。`log` 里祖先只能出现一次，多 head 时也不能漏掉另一条链。
#[test]
fn walk_covers_every_head_and_visits_a_shared_ancestor_once() {
    let (_site, engine) = db();
    let tree = object::put_tree(&*engine, PID, &[file("a", 1, 1)]).unwrap();
    let base = commit(&*engine, PID, &[], &tree, 100);
    let left = commit(&*engine, PID, &[base], &tree, 200);
    let right = commit(&*engine, PID, &[base], &tree, 300);
    build::set_ref(&*engine, PID, build::MAIN, &[left, right]).unwrap();

    let heads = build::ref_heads(&*engine, PID, build::MAIN).unwrap();
    assert_eq!(heads.len(), 2);
    let all = history::walk(&*engine, PID, &heads, 0).unwrap();
    assert_eq!(all.len(), 3, "共同祖先不许重复：{all:?}");
    assert_eq!(
        all.iter().map(|n| n.commit.ts).collect::<Vec<_>>(),
        vec![300, 200, 100]
    );
}

/// 同一时间戳的两个 commit：次级键（hash）决定顺序，所以两次遍历必须给同一个答案。
/// 没有这条，§10.4 的差分表在两台机器上就对不上。
#[test]
fn same_timestamp_orders_are_stable() {
    let (_site, engine) = db();
    // 两棵不同的树：内容寻址下"同 ts + 同内容"就是**同一个 commit**（第一版我拿同一棵树提了
    // 两次，得到一个 hash、两条断言全变空转），要测破平就必须让 hash 真的不同。
    let t1 = object::put_tree(&*engine, PID, &[file("a", 1, 1)]).unwrap();
    let t2 = object::put_tree(&*engine, PID, &[file("b", 2, 2)]).unwrap();
    let a = commit(&*engine, PID, &[], &t1, 500);
    let b = commit(&*engine, PID, &[], &t2, 500);
    assert_ne!(a, b, "同 ts 的两个 commit 该有两个 hash");
    let want: Vec<Hash> = {
        let mut v = vec![a, b];
        v.sort_by(|x, y| y.cmp(x)); // 与 Node::cmp 的破平方向一致：hash 倒序
        v
    };
    for _ in 0..3 {
        let got = history::walk(&*engine, PID, &[a, b], 0).unwrap();
        assert_eq!(got.iter().map(|n| n.hash).collect::<Vec<_>>(), want);
    }
}

#[test]
fn resolve_walks_nested_paths_one_level_at_a_time() {
    let (_site, engine) = db();
    let deep = object::put_tree(&*engine, PID, &[file("deep.txt", 9, 5)]).unwrap();
    let top =
        object::put_tree(&*engine, PID, &[dir("inner", deep), file("leaf.txt", 8, 4)]).unwrap();
    let root = object::put_tree(&*engine, PID, &[dir("top", top)]).unwrap();

    let e = history::resolve(&*engine, PID, &root, "top/inner/deep.txt")
        .unwrap()
        .expect("三层路径该解析出来");
    assert_eq!(e.kind, Kind::File);
    assert_eq!(e.size, 5);
    assert_eq!(e.hash, [9u8; 32]);

    let d = history::resolve(&*engine, PID, &root, "top/inner")
        .unwrap()
        .expect("目录也是条目");
    assert_eq!(d.kind, Kind::Dir);
    assert_eq!(d.hash, deep, "目录条目的 hash 就是它自己的 tree");

    let r = history::resolve(&*engine, PID, &root, "")
        .unwrap()
        .expect("空路径 = 根");
    assert_eq!(r.kind, Kind::Dir);
    assert_eq!(r.hash, root);
    // 带斜杠与 `.` 的写法必须等价：用户在 shell 里补全出来的路径长这样。
    assert_eq!(
        history::resolve(&*engine, PID, &root, "/./top/inner/deep.txt")
            .unwrap()
            .map(|e| e.hash),
        Some([9u8; 32])
    );
}

#[test]
fn resolve_reports_missing_paths_as_none_not_as_corruption() {
    let (_site, engine) = db();
    let root = object::put_tree(&*engine, PID, &[file("a.txt", 1, 3)]).unwrap();

    // 尾斜杠与 `.` 一样被当噪音滤掉，所以 "a.txt/" 解析到的就是 a.txt 本身（见上一条用例）。
    for p in ["nope.txt", "a.txt/b", "../etc/passwd"] {
        assert_eq!(
            history::resolve(&*engine, PID, &root, p).unwrap(),
            None,
            "{p} 该是「不存在」而不是报错（与 git 同口径）"
        );
    }
    // 存在但读不出来 = 另一类错误，不许被上面那条吞掉。
    let ghost = [0xABu8; 32];
    assert!(matches!(
        history::resolve(&*engine, PID, &ghost, "x"),
        Err(Error::MissingObject { .. })
    ));
}

#[test]
fn rev_resolution_accepts_head_and_full_hash_but_rejects_garbage() {
    let (_site, engine) = db();
    let tree = object::put_tree(&*engine, PID, &[file("a", 1, 1)]).unwrap();
    let c1 = commit(&*engine, PID, &[], &tree, 100);
    let c2 = commit(&*engine, PID, &[c1], &tree, 200);
    build::set_ref(&*engine, PID, build::MAIN, &[c2]).unwrap();

    assert_eq!(
        history::resolve_rev(&*engine, PID, "HEAD").unwrap(),
        vec![c2]
    );
    assert_eq!(
        history::resolve_rev(&*engine, PID, "main").unwrap(),
        vec![c2]
    );
    assert_eq!(
        history::resolve_rev(&*engine, PID, &keys::hex(&c1)).unwrap(),
        vec![c1],
        "完整 hash 必须能查到，这是 gc/push 的输入形态"
    );

    // 空前缀 = 匹配全表 ⇒ 走"不唯一"那条分支（构造 hash 碰撞不现实，这一条是它的替身）。
    assert!(
        matches!(
            history::resolve_rev(&*engine, PID, ""),
            Err(Error::Codec(_))
        ),
        "两个 commit 时空前缀必须报不唯一"
    );
    // 合法但查无此物 ≠ 语法错误：格式不对才报 Codec，格式对而对象不在报 MissingObject。
    for bad in ["zz", "abc"] {
        assert!(
            matches!(
                history::resolve_rev(&*engine, PID, bad),
                Err(Error::Codec(_))
            ),
            "{bad} 不是合法的 rev 写法"
        );
    }
    assert!(matches!(
        history::resolve_rev(&*engine, PID, "000000"),
        Err(Error::MissingObject { .. })
    ));
    assert!(matches!(
        history::resolve_rev(&*engine, PID, &keys::hex(&[0u8; 32])),
        Err(Error::MissingObject { .. })
    ));
}

/// `ls-files` 的底座：前序字典序 + 完整路径 + 目录也算一条。
#[test]
fn walk_tree_lists_paths_in_lexicographic_preorder() {
    let (_site, engine) = db();
    let deep = object::put_tree(&*engine, PID, &[file("deep.txt", 9, 5)]).unwrap();
    let top =
        object::put_tree(&*engine, PID, &[dir("inner", deep), file("leaf.txt", 8, 4)]).unwrap();
    let root = object::put_tree(
        &*engine,
        PID,
        &[dir("b", top), file("a.txt", 1, 1), dir("z", top)],
    )
    .unwrap();

    let mut got: Vec<String> = Vec::new();
    history::walk_tree(&*engine, PID, &root, &mut |path, e| {
        got.push(format!("{}:{}", e.kind as u8, path));
        Ok(())
    })
    .unwrap();
    assert_eq!(
        got,
        vec![
            "0:a.txt",
            "1:b",
            "1:b/inner",
            "0:b/inner/deep.txt",
            "0:b/leaf.txt",
            "1:z",
            "1:z/inner",
            "0:z/inner/deep.txt",
            "0:z/leaf.txt",
        ],
        "父在子前、同级按字节序；目录自己也要占一条（空目录只有靠它才表达得出来）"
    );
}

/// 空树也要能走完：`ls-files` 对一个空版本该给"共 0 条"而不是报错。
#[test]
fn walk_tree_on_an_empty_version_yields_nothing() {
    let (_site, engine) = db();
    let mut n = 0;
    history::walk_tree(&*engine, PID, &object::empty_tree(), &mut |_, _| {
        n += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(n, 0);
}

/// `HEAD~N` 语法：沿第一亲代回溯。`~0` 必须原样返回，越界要报"到第几步没亲代了"，
/// 而不是安静地停在根 commit 上——那会让 `checkout --at HEAD~99` 看起来"成功"。
#[test]
fn rev_supports_first_parent_walk() {
    let (_site, engine) = db();
    let tree = object::put_tree(&*engine, PID, &[file("a", 1, 1)]).unwrap();
    let c1 = commit(&*engine, PID, &[], &tree, 100);
    let c2 = commit(&*engine, PID, &[c1], &tree, 200);
    let c3 = commit(&*engine, PID, &[c2], &tree, 300);
    build::set_ref(&*engine, PID, build::MAIN, &[c3]).unwrap();

    for (rev, want) in [
        ("HEAD", vec![c3]),
        ("HEAD~0", vec![c3]),
        ("HEAD~1", vec![c2]),
        ("HEAD~2", vec![c1]),
        ("HEAD^", vec![c2]),
        ("HEAD~1^", vec![c1]),
        ("main~1", vec![c2]),
        // 两种写法在这里同义 ⇒ 链式后缀是步数相加，不是"第 N 亲"。
        ("HEAD~1~1", vec![c1]),
    ] {
        assert_eq!(
            history::resolve_rev(&*engine, PID, rev).unwrap(),
            want,
            "{rev} 解析错了"
        );
    }
    // 越界：c1 是根，没有亲代。
    let err = history::resolve_rev(&*engine, PID, "HEAD~9").unwrap_err();
    assert!(
        matches!(err, Error::MissingObject { .. }),
        "越界该报缺亲代，实际 {err}"
    );
    // c3 →(1) c2 →(2) c1 →(3) 没了：断在第 3 步，报出来要能数得对。
    assert!(
        err.to_string().contains("第 3 步"),
        "要说清断在哪一步：{err}"
    );
    // 后缀不是数字 ⇒ 整串当基座，报"看不懂"，不替用户猜。
    assert!(matches!(
        history::resolve_rev(&*engine, PID, "HEAD~x"),
        Err(Error::Codec(_))
    ));
}

/// 多 head 时 `~N` 有歧义：**必须报错**，不许挑一个 head 往回走。
#[test]
fn tilde_on_a_multi_head_ref_is_ambiguous() {
    let (_site, engine) = db();
    let tree = object::put_tree(&*engine, PID, &[file("a", 1, 1)]).unwrap();
    let base = commit(&*engine, PID, &[], &tree, 100);
    let left = commit(&*engine, PID, &[base], &tree, 200);
    let right = commit(&*engine, PID, &[base], &tree, 300);
    build::set_ref(&*engine, PID, build::MAIN, &[left, right]).unwrap();

    let err = history::resolve_rev(&*engine, PID, "HEAD~1").unwrap_err();
    assert!(matches!(err, Error::Codec(_)), "实际 {err}");
    assert!(
        err.to_string().contains("2 个 head"),
        "得说清歧义在哪：{err}"
    );
    // 但指定具体 commit 就照走：歧义只在"从哪个 head 出发"这一层。
    assert_eq!(
        history::resolve_rev(&*engine, PID, &format!("{}~1", keys::hex(&right))).unwrap(),
        vec![base]
    );
}
