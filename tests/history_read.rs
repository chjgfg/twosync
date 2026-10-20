//! 只读历史层：`log` 的遍历、`show` 的路径解析、rev 前缀解析。
//!
//! 这一层的用例**全部手搓对象**（`put_tree`/`put_commit`/`set_ref`），不碰文件系统也不碰
//! CAS：读层唯一的依赖是引擎里的对象，把扫描和落块卷进来就等于用写层测写层——写层一坏，
//! 读层的用例跟着一起红，那就分不清是谁的问题了。真实链路的证明在 CLI 那一遍跑（模块末注释）。
//!
//! `fmt_ts` 的日历换算放在 `src/store/history.rs` 的单元测试里（那是纯函数，不必起引擎）。

mod common;

use storage_engines::KvEngine;
use twosync::error::Error;
use twosync::store::build;
use twosync::store::history;
use twosync::store::keys::{self, Hash, Kind, ProjectId};
use twosync::store::object::{self, Stats, TreeEntry};
use twosync::utils::open_store;

const PID: ProjectId = 11;

fn db() -> (tempfile::TempDir, Box<dyn KvEngine>) {
    let site = tempfile::tempdir().unwrap();
    let engine = open_store(site.path(), common::engine()).unwrap();
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

/// `parent_index` 必须把**序数**留着：合并提交的第一个亲代（ordinal 0）是"主亲代"，
/// `topo_pending` 与 §7.1 的合并语义都按下标取它。顺序丢了就是静默的历史错乱。
#[test]
fn the_parent_index_keeps_ordinal_order_including_merges() {
    let (_d, engine) = db();
    let tree = object::put_tree(&*engine, PID, &[file("a", 1, 1)]).unwrap();
    let g = commit(&*engine, PID, &[], &tree, 100);
    let l = commit(&*engine, PID, &[g], &tree, 200);
    let r = commit(&*engine, PID, &[g], &tree, 300);
    // 一个真合并：两个亲代，故意按 (r, l) 的顺序写进去 ⇒ 索引必须按 ordinal 还回 (r, l)
    let m = commit(&*engine, PID, &[r, l], &tree, 400);

    let index = object::parent_index(&*engine, PID).unwrap();
    assert_eq!(index.get(&m).unwrap(), &vec![r, l], "ordinal 就是下标，不许按 hash 排序");
    assert_eq!(index.get(&l).unwrap(), &vec![g]);
    assert_eq!(index.get(&r).unwrap(), &vec![g]);
    assert!(!index.contains_key(&g), "根提交没有亲代行");

    // 闭包（纯函数那份）走同样的图：合并的两个亲代都要到根
    let got = history::reachable_from(&index, &[m], &[]);
    assert_eq!(got, [m, r, l, g].into_iter().collect::<std::collections::BTreeSet<_>>());
}

/// 一次前缀扫换来的那条路径必须与"逐步点查亲代"**等价**——这条断言是这次改写的安全带：
/// 差一个 commit 就等于 GC 少标一个保留集对象（把活数据判成垃圾）。
#[test]
fn the_one_scan_closure_equals_the_step_by_step_walk() {
    let (_d, engine) = db();
    let tree = object::put_tree(&*engine, PID, &[file("a", 1, 1)]).unwrap();
    let g = commit(&*engine, PID, &[], &tree, 100);
    let a1 = commit(&*engine, PID, &[g], &tree, 200);
    let a2 = commit(&*engine, PID, &[a1], &tree, 300);
    let b1 = commit(&*engine, PID, &[g], &tree, 400);
    let m = commit(&*engine, PID, &[a2, b1], &tree, 500);
    build::set_ref(&*engine, PID, build::MAIN, &[m, b1]).unwrap();

    // 两个 head（`m` 与还挂着的 `b1`）+ 一份"外部也算已知"的种子，形状与 push 那边一致
    let heads = [m, b1];
    let extra = [a1];
    let strict = history::reachable_strict(&*engine, PID, &heads, &extra).unwrap();
    let index = object::parent_index(&*engine, PID).unwrap();
    assert_eq!(
        strict,
        history::reachable_from(&index, &heads, &extra),
        "一次性装索引的那份必须与严格遍历同一份集合"
    );
    assert_eq!(strict.len(), 5, "m/a2/a1/b1/g 五个：{strict:?}");
    // 缺亲代行（不变量 (g) 坏了）时闭包只少走那一截，不许 panic
    assert!(history::reachable_from(&index, &[g], &[]).contains(&g), "根也要在集合里");
}

// ── 这一节的最后一条是**例外** ────────────────────────────────────────────────
// 上面全部用例只测行为。下面这条不测行为，只**量代价**：§11.4b 已经把规矩钉死了
// ——"整段前缀扫 = 1 折，但把命中的值全物化（实测口径 ≈ 207 B/键）"，所以任何"改成一次装载"
// 都必须先有数，才许写进实现。它要在 12 GiB 站点（`D:\ts10g`，项目 `lab`）上手动跑：
// `cargo test --test history_read -- --ignored --nocapture`
#[test]
#[ignore = "需要 12 GiB 量测台 D:\\ts10g（不在 CI 上）"]
fn ten_gib_manifest_load_cost_is_counted_before_the_batch_loader_ships() {
    use twosync::utils::{counted, Meter};

    let root = std::path::Path::new(r"D:\ts10g");
    if !root.join(".twosync").exists() {
        eprintln!("跳过：没有 {root:?}");
        return;
    }
    let meter = Meter::default();
    let db = counted(
        open_store(root, common::engine()).expect("open_store"),
        &meter,
        false,
    );
    let pid = keys::project_id("lab");

    // ① 这套历史有多大（**只用一次前缀扫**的口径）。
    //    注意这里**不能**调 `marked_objects`：它从每个 commit 出发逐层 `read_tree`，
    //    代价是 O(commit 数 × 目录数) 次点读 = 这一轮要消掉的那个成本本身（debug 下一折 ≈ 4 s，
    //    一次量测会跑到天荒地老）。"当前快照有几个目录"改从 `W` 行算：那是一份扫描结果，
    //    顺带把"W 行到底能不能整段装载"这个数也量出来（§11.4b 结论 1 留的口子）。
    let heads = build::ref_heads(&*db, pid, build::MAIN).expect("ref_heads");
    let commits = history::reachable_strict(&*db, pid, &heads, &[]).expect("reachable");

    // ② `W|p` 整段扫：行数、value 字节、快照里的目录数（按"文件路径去掉最后一段"去重）。
    let wrows = scan_rows(&*db, keys::TAG_WORKTREE, pid);
    let mut w_bytes = 0u64;
    let mut dirs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let prefix_w = prefix_of(keys::TAG_WORKTREE, pid);
    for r in &wrows {
        if let Some(v) = r.value.as_ref() {
            w_bytes += v.len() as u64;
        }
        let Some(tail) = r.key.get(prefix_w.len()..) else { continue };
        let path = String::from_utf8_lossy(tail).into_owned();
        // 目录行自己带尾随 `/`；文件行取它的父段。两种都归一到"它属于哪个目录"。
        let trimmed = path.trim_end_matches('/').to_string();
        match trimmed.rfind('/') {
            Some(i) => dirs.insert(trimmed[..i].to_string()),
            None => dirs.insert(String::new()),
        };
    }

    // ③ `T|p` 整段扫：行数、value 字节、条目总数。
    let rows = scan_rows(&*db, keys::TAG_TREE, pid);
    let mut value_bytes = 0u64;
    let mut entries_total = 0usize;
    let mut undecoded = 0usize;
    for r in &rows {
        let Some(v) = r.value.as_ref() else { continue };
        value_bytes += v.len() as u64;
        let Ok(raw) = keys::strip(v) else {
            undecoded += 1;
            continue;
        };
        match postcard::from_bytes::<Vec<TreeEntry>>(raw) {
            Ok(list) => entries_total += list.len(),
            Err(_) => undecoded += 1,
        }
    }
    // 桶从 0 起算（这个 `Meter` 是本次探针独有的），所以直接取快照就是"这条用例折了几次"。
    let counts = meter.snapshot();

    println!("== 12 GiB 站点的目录清单装载代价（project lab）");
    println!(
        "规模         = head {} · 可达 commit {} · 快照目录 {}（从 W 行去重）",
        heads.len(),
        commits.len(),
        dirs.len()
    );
    println!(
        "T|p 整段扫    = 行 {} · value {:.2} MiB · 清单条目 {} · 平均每张 {:.0} B",
        rows.len(),
        value_bytes as f64 / 1048576.0,
        entries_total,
        value_bytes as f64 / rows.len().max(1) as f64
    );
    println!(
        "W|p 整段扫    = 行 {} · value {:.2} MiB（这一条是给§11.4b 结论 1 补的数：W 不能整段装载）",
        wrows.len(),
        w_bytes as f64 / 1048576.0
    );
    println!(
        "历史倍数      = T 行 {} ÷ 快照目录 {} = {:.1}× ⇒ 整段装载比按 head 走多拿这么多",
        rows.len(),
        dirs.len(),
        rows.len() as f64 / dirs.len().max(1) as f64
    );
    println!("折数账        = {}", counts.line());
    assert_eq!(undecoded, 0, "有一行的 value 解不出来：装载器不能假定编码是齐的");
    assert!(
        !rows.is_empty() && !wrows.is_empty(),
        "站点是空的 ⇒ 这条量测没有意义，去看是不是开错了库"
    );
}

/// `tag|pid` 的前缀字节。
fn prefix_of(tag: u8, pid: ProjectId) -> Vec<u8> {
    let mut p = vec![tag];
    p.extend_from_slice(&pid.to_be_bytes());
    p
}

/// **一次**前缀扫，把命中的行原样搬回来（键与 value 都在）。
/// 探针用它，是为了让"整段装载到底搬多少东西"这个数与后面要写的装载器走同一条调用。
fn scan_rows(db: &dyn KvEngine, tag: u8, pid: ProjectId) -> Vec<storage_engines::ExportRecord> {
    let prefix = prefix_of(tag, pid);
    let tx = db.begin();
    let rows = tx.prefix_scan(&prefix);
    tx.commit();
    rows
}

// ── §7 第五行：collision 检测（v1 的语义是"union，不猜"，所以先把"哪条路径两边不一样"算准）──

/// 造一棵只含一个文件的树。
fn tree_with(db: &dyn KvEngine, name: &str, seed: u8) -> Hash {
    object::put_tree(db, PID, &[file(name, seed, 10)]).unwrap()
}

fn paths(cs: &[history::Collision]) -> Vec<String> {
    cs.iter().map(|c| c.path.clone()).collect()
}

#[test]
fn two_heads_that_changed_the_same_path_differently_collide() {
    let (_d, engine) = db();
    let base = tree_with(&*engine, "a.bin", 1);
    let g = commit(&*engine, PID, &[], &base, 100);
    let h1 = commit(&*engine, PID, &[g], &tree_with(&*engine, "a.bin", 2), 200);
    let h2 = commit(&*engine, PID, &[g], &tree_with(&*engine, "a.bin", 3), 300);

    let cs = history::collisions(&*engine, PID, &[h1, h2]).unwrap();
    assert_eq!(paths(&cs), vec!["a.bin".to_string()], "两边把 a.bin 改成了不同东西：{cs:?}");
    assert_eq!(cs[0].heads.len(), 2, "要报出卷进来的两个 head：{cs:?}");
    assert!(cs[0].heads.contains(&h1) && cs[0].heads.contains(&h2));
}

#[test]
fn disjoint_changes_and_identical_changes_are_not_collisions() {
    let (_d, engine) = db();
    let base = object::put_tree(
        &*engine,
        PID,
        &[file("a.bin", 1, 10), file("b.bin", 2, 10)],
    )
    .unwrap();
    let g = commit(&*engine, PID, &[], &base, 100);

    // ① 各改各的路径 ⇒ union 的正常态（§7 第四行），一条都不该报。
    let left = object::put_tree(&*engine, PID, &[file("a.bin", 9, 10), file("b.bin", 2, 10)]).unwrap();
    let right = object::put_tree(&*engine, PID, &[file("a.bin", 1, 10), file("b.bin", 8, 10)]).unwrap();
    let h1 = commit(&*engine, PID, &[g], &left, 200);
    let h2 = commit(&*engine, PID, &[g], &right, 300);
    assert!(
        history::collisions(&*engine, PID, &[h1, h2]).unwrap().is_empty(),
        "路径无交集却报了 collision ⇒ 会把每次 union 都变成报警"
    );

    // ② 两边改成**一模一样**的东西 ⇒ 也没有要人选的东西（字节都一样，裁什么？）
    let same = object::put_tree(&*engine, PID, &[file("a.bin", 7, 10), file("b.bin", 2, 10)]).unwrap();
    let h3 = commit(&*engine, PID, &[g], &same, 400);
    let h4 = commit(&*engine, PID, &[g], &same, 500);
    assert!(
        history::collisions(&*engine, PID, &[h3, h4]).unwrap().is_empty(),
        "同一条改动不该算冲突"
    );

}

/// 单 head 是常态（每次会话验收都会问一遍 collision），所以那一格必须**零引擎调用**。
/// 这条用的是"从建库起就套着计数器"的句柄：同一份库第二个句柄只会读到旧视图，
/// 量不到真东西（`Site`/`Cli` 缓存句柄之后这条更要注意）。
#[test]
fn a_single_head_costs_the_engine_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let meter = twosync::utils::Meter::default();
    let engine = twosync::utils::counted(
        open_store(tmp.path(), common::engine()).unwrap(),
        &meter,
        false,
    );
    let t = tree_with(&*engine, "a.bin", 1);
    let h = commit(&*engine, PID, &[], &t, 100);
    let base = meter.snapshot();

    let cs = history::collisions(&*engine, PID, &[h]).unwrap();
    assert!(cs.is_empty(), "单 head 没有可对的另一边：{cs:?}");
    assert_eq!(
        meter.snapshot().since(base).folds(),
        0,
        "单 head 的 collision 检测付了引擎调用：{}",
        meter.snapshot().since(base).line()
    );
}

#[test]
fn two_independent_roots_collide_on_every_shared_path_they_differ_on() {
    // 没有共同祖先（两台机器各自从空开始）：与"空树"比 ⇒ 两边都有、内容不同的那条就是冲突。
    let (_d, engine) = db();
    let a = commit(&*engine, PID, &[], &tree_with(&*engine, "same.bin", 1), 100);
    let b = commit(&*engine, PID, &[], &tree_with(&*engine, "same.bin", 2), 200);
    let cs = history::collisions(&*engine, PID, &[a, b]).unwrap();
    assert_eq!(paths(&cs), vec!["same.bin".to_string()], "实际 {cs:?}");

    // 而"只有一边有的路径"不算冲突：那是纯粹的并集，没有要人选的东西。
    let c = commit(&*engine, PID, &[], &tree_with(&*engine, "only.c", 3), 400);
    let cs2 = history::collisions(&*engine, PID, &[a, c]).unwrap();
    assert!(cs2.is_empty(), "各有一条独有路径 ⇒ 不该报：{cs2:?}");
}
