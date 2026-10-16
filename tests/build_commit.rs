//! `commit` 流水线：`W` 快照 → CAS 落块 → 建树 → 比对 → 移 head。
//!
//! 这批用例盯的是四件容易悄悄坏掉的事：
//! 1. **删除只能由 epoch 判定**。扫描给本轮看见的每一行盖 `scanned_at`，`commit` 只认最新
//!    一轮。所以 `no_change_build_makes_no_commit` 不只是"别产生空提交"，它是**续期机制**的
//!    守卫：稳态扫描一旦漏刷某一行，那行就会被报成删除，这条立刻红。
//! 2. **引用数不许重复加**（模块头讲的 `is_new_blob`），也不许漏改 `k` 索引（不变量 (g)）。
//! 3. **历史必须真能取回字节**——只验 hash 不验字节等于没验。
//! 4. **熔断要在移 head 之前**（FR-10）。

use std::fs;
use std::path::Path;

use storage_engines::{EngineKind, KvEngine};
use twosync::error::Error;
use twosync::store::build::{self, BlockMeta};
use twosync::store::cas::{self, Cas};
use twosync::store::history;
use twosync::store::keys::{self, Hash, Kind, ProjectId};
use twosync::store::object;
use twosync::store::worktree;
use twosync::utils::{objects_dir, open_store};

/// 一个测试站点：`.twosync` 挂在 `site` 下，项目目录是 `site/p`。
struct Site {
    db: Box<dyn KvEngine>,
    pid: ProjectId,
    /// 字段声明在引擎**之后** ⇒ 析构时先关引擎再删临时目录。反过来在 Windows 上会撞上
    /// "句柄还没放掉就删文件"。Rust 的字段按声明顺序析构，不是按字母。
    tmp: tempfile::TempDir,
}

impl Site {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let db = open_store(tmp.path(), EngineKind::LsmTree).unwrap();
        let pid = keys::project_id("p");
        Self { db, pid, tmp }
    }

    fn proj(&self) -> std::path::PathBuf {
        self.tmp.path().join("p")
    }

    /// 扫一遍 + 提一次。返回 `Built`。块大小由调用方给，好让"块共享"可测。
    fn commit(&self, block_size: u64, allow_mass_delete: bool) -> build::Built {
        fs::create_dir_all(self.proj()).unwrap();
        self.scan();
        self.build(block_size, allow_mass_delete)
    }

    fn scan(&self) {
        let proj = self.proj();
        worktree::scan(&*self.db, self.pid, &proj, false, 100).unwrap();
    }

    /// 只提不扫：用来构造"扫描之后工作树又变了"那种状态。
    fn build(&self, block_size: u64, allow_mass_delete: bool) -> build::Built {
        self.try_build_with(block_size, allow_mass_delete).unwrap()
    }

    /// 提一次，把错误交回去（熔断/超限/StaleScan 这类判定要用）。
    fn try_build(&self, block_size: u64) -> twosync::error::Result<build::Built> {
        self.try_build_with(block_size, false)
    }

    fn try_build_with(
        &self,
        block_size: u64,
        allow_mass_delete: bool,
    ) -> twosync::error::Result<build::Built> {
        let proj = self.proj();
        let store = Cas::new(&objects_dir(self.tmp.path()), block_size).unwrap();
        build::build(
            &*self.db,
            self.pid,
            &store,
            &proj,
            build::Meta {
                peer: "test-peer",
                msg: "用例提交",
                ts: 1,
                allow_mass_delete,
            },
        )
    }

    /// 最近一次完整扫描的 epoch。
    fn epoch(&self) -> i64 {
        worktree::last_mark(&*self.db, self.pid)
            .expect("用例里扫过")
            .epoch
    }

    fn head(&self) -> Vec<Hash> {
        build::ref_heads(&*self.db, self.pid, build::MAIN).unwrap()
    }

    fn block(&self, hash: &Hash) -> Option<BlockMeta> {
        let key = keys::block_key(self.pid, hash);
        let raw = {
            let tx = self.db.begin();
            let got = tx.get(&key);
            tx.commit();
            got
        }?;
        postcard::from_bytes(keys::strip(&raw).ok()?).ok()
    }

    fn refcnt_index(&self, refcnt: u64, hash: &Hash) -> bool {
        let key = keys::refcnt_key(self.pid, refcnt, hash);
        let tx = self.db.begin();
        let got = tx.get(&key).is_some();
        tx.commit();
        got
    }
}

fn bh(bytes: &[u8]) -> Hash {
    *blake3::hash(bytes).as_bytes()
}

fn write(path: &Path, bytes: impl AsRef<[u8]>) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, bytes).unwrap();
}

#[test]
fn first_commit_adds_every_path_and_moves_head() {
    let site = Site::new();
    let proj = site.proj();
    write(&proj.join("a.txt"), b"one");
    write(&proj.join("sub/b.txt"), b"two");

    let built = site.commit(cas::DEFAULT_BLOCK, false);
    let commit = built.commit.expect("首轮必须有提交");
    assert_eq!(site.head(), vec![commit], "head 只该指向这一个 commit");
    assert_eq!(built.entries, 3, "a.txt + sub + sub/b.txt");
    assert_eq!(built.known, 3);
    // 目录也算一条 add：它是树里的一个条目，FR-10 的分母与它同源。
    assert_eq!(
        (
            built.changes.add,
            built.changes.modify,
            built.changes.delete
        ),
        (3, 0, 0),
        "首轮全新增：{:?}",
        built.changes
    );
    assert_eq!(built.blobs_new, 2, "两个文件两份清单，目录不落 blob");

    let c = object::read_commit(&*site.db, site.pid, &commit).unwrap();
    assert!(c.parents.is_empty(), "首轮没有亲代");
    assert_eq!(c.tree, built.tree);
    assert_eq!(c.stats.add, 3, "commit 里记的 stats 必须与 diff 同源");
}

/// 这条是**续期机制**的守卫：稳态扫描若不把未变行的 `scanned_at` 刷成本轮，
/// 那些行就会被 epoch 过滤器当成删除，这里拿到的就不是 `None` 而是 MassDelete 了。
#[test]
fn no_change_build_makes_no_commit() {
    let site = Site::new();
    let proj = site.proj();
    write(&proj.join("a.txt"), b"one");
    write(&proj.join("sub/b.txt"), b"two");

    let first = site.commit(cas::DEFAULT_BLOCK, false).commit.unwrap();
    let again = site.commit(cas::DEFAULT_BLOCK, false);
    assert!(
        again.commit.is_none(),
        "无改动不该产生空提交：{:?}",
        again.changes
    );
    assert!(again.changes.is_empty());
    assert_eq!(again.entries, 3, "本轮该看见全部三条（续期成功）");
    assert_eq!(site.head(), vec![first], "head 不许动");
}

#[test]
fn deleted_file_counts_as_delete_and_keeps_its_blob() {
    let site = Site::new();
    let proj = site.proj();
    write(&proj.join("a.txt"), b"aaa");
    write(&proj.join("keep.txt"), b"keep");
    let first = site.commit(cas::DEFAULT_BLOCK, false).commit.unwrap();
    let blob_a = {
        let tree = object::read_commit(&*site.db, site.pid, &first)
            .unwrap()
            .tree;
        let entries = object::read_tree(&*site.db, site.pid, &tree).unwrap();
        entries
            .iter()
            .find(|e| e.name == "a.txt")
            .expect("树里该有 a.txt")
            .hash
    };

    fs::remove_file(proj.join("a.txt")).unwrap();
    let built = site.commit(cas::DEFAULT_BLOCK, false);
    let commit = built.commit.expect("删除是一次改动");
    assert_eq!(built.changes.delete, 1);
    assert_eq!(built.changes.deleted_blobs, vec![blob_a]);
    assert_eq!(built.entries, 1, "只剩 keep.txt");

    // §7.3：commit 阶段一个字节都不许删——旧 commit 还要能取回 a.txt。
    assert_eq!(
        build::read_blob(&*site.db, site.pid, &blob_a).unwrap().size,
        3
    );
    let c = object::read_commit(&*site.db, site.pid, &commit).unwrap();
    assert_eq!(c.parents, vec![first], "head 是快进，亲代就是上一个 commit");
}

#[test]
fn modified_file_counts_as_modify_not_add() {
    let site = Site::new();
    write(&site.proj().join("a.txt"), b"v1");
    site.commit(cas::DEFAULT_BLOCK, false);

    write(&site.proj().join("a.txt"), b"v2-longer");
    let built = site.commit(cas::DEFAULT_BLOCK, false);
    assert_eq!(
        (
            built.changes.add,
            built.changes.modify,
            built.changes.delete
        ),
        (0, 1, 0),
        "改内容只该记 modify：{:?}",
        built.changes
    );
    assert_eq!(built.blobs_new, 1, "老 blob 的块还在，只有新块要写");
}

#[test]
fn mass_delete_gate_blocks_head_and_the_flag_lifts_it() {
    let site = Site::new();
    for i in 0..100 {
        write(&site.proj().join(format!("f{i}.txt")), b"x".repeat(10));
    }
    let first = site.commit(cas::DEFAULT_BLOCK, false).commit.unwrap();

    for i in 0..30 {
        fs::remove_file(site.proj().join(format!("f{i}.txt"))).unwrap();
    }
    site.scan();
    let err = site
        .try_build(cas::DEFAULT_BLOCK)
        .expect_err("删掉 30% 必须触发 FR-10");
    match err {
        Error::MassDelete {
            deleted,
            known,
            deleted_bytes,
        } => assert_eq!(
            (deleted, known, deleted_bytes),
            (30, 100, 300),
            "熔断的三个数都得可对账"
        ),
        other => panic!("应是 MassDelete，实际 {other}"),
    }
    assert_eq!(site.head(), vec![first], "被拒的 commit 不许动 head");

    let built = site.commit(cas::DEFAULT_BLOCK, true);
    assert_eq!(built.changes.delete, 30);
    assert_ne!(site.head()[0], first, "签字之后才推进");
}

/// 绝对下限存在的理由：小项目里"删一个文件"按比例就是 50%，纯比例的闸会让这种仓库永远
/// 提不了交。FR-10 防的是误 `rm -rf`，那是规模问题。
#[test]
fn single_delete_in_a_small_project_is_not_mass_delete() {
    let site = Site::new();
    write(&site.proj().join("a.txt"), b"one");
    write(&site.proj().join("b.txt"), b"two");
    site.commit(cas::DEFAULT_BLOCK, false);

    fs::remove_file(site.proj().join("a.txt")).unwrap();
    let built = site.commit(cas::DEFAULT_BLOCK, false);
    assert_eq!(
        built.changes.delete, 1,
        "2 个文件删 1 个（50%）不该触发熔断"
    );
}

/// 历史能不能真的取回字节，是"版本库"这个说法的全部代价。
#[test]
fn committed_bytes_roundtrip_through_cas() {
    let site = Site::new();
    let body: Vec<u8> = (0..300_000usize).map(|i| (i % 251) as u8).collect();
    write(&site.proj().join("big.bin"), &body);
    let commit = site.commit(64 << 10, false).commit.unwrap();

    let tree = object::read_commit(&*site.db, site.pid, &commit)
        .unwrap()
        .tree;
    let entries = object::read_tree(&*site.db, site.pid, &tree).unwrap();
    let hash = entries.iter().find(|e| e.name == "big.bin").unwrap().hash;

    let blob = build::read_blob(&*site.db, site.pid, &hash).unwrap();
    assert_eq!(blob.size, body.len() as u64);
    assert_eq!(blob.block_size, 64 << 10);
    assert_eq!(blob.blocks.len(), 5, "300000 / 65536 向上取整");
    assert_eq!(blob.hash, hash, "清单的整文件 hash 就是树里那个");

    let dest = site.tmp.path().join("out/big.bin");
    let store = Cas::new(&objects_dir(site.tmp.path()), 64 << 10).unwrap();
    store.read_blob(&blob, &dest).unwrap();
    assert_eq!(fs::read(&dest).unwrap(), body, "取回来的字节必须一字不差");
}

#[test]
fn identical_files_share_one_blob_and_one_reference() {
    let site = Site::new();
    write(&site.proj().join("x.txt"), b"same");
    write(&site.proj().join("y.txt"), b"same");
    let built = site.commit(cas::DEFAULT_BLOCK, false);
    assert_eq!(built.blobs_new, 1, "内容相同 ⇒ 一份清单");

    let block = bh(b"same");
    assert_eq!(
        site.block(&block).map(|m| m.refcnt),
        Some(1),
        "两个文件共享同一 blob ⇒ 引用数只加一次"
    );
    assert!(site.refcnt_index(1, &block), "k 索引必须与 K 同档位");
}

/// 两个文件**共享一个块**但整文件不同：这才是 refcnt 真正的用途。
/// 顺带钉不变量 (g)：计数搬档位时旧档位的索引行必须消失。
#[test]
fn shared_block_between_two_blobs_bumps_refcnt_to_two() {
    let site = Site::new();
    let head = vec![1u8; 4096];
    write(&site.proj().join("a.bin"), [&head[..], b"x"].concat());
    write(&site.proj().join("b.bin"), [&head[..], b"y"].concat());
    let built = site.commit(4096, false);
    assert_eq!(built.blobs_new, 2, "整文件不同 ⇒ 两份清单");

    let shared = bh(&head);
    assert_eq!(site.block(&shared).map(|m| m.refcnt), Some(2));
    assert!(site.refcnt_index(2, &shared), "新档位要有索引");
    assert!(
        !site.refcnt_index(1, &shared),
        "不变量 (g)：旧档位的索引行必须一起删掉，否则 GC 会数错"
    );
}

/// 扫描之后文件被人改了 ⇒ 拒这一轮，绝不静默用新内容。
#[test]
fn stale_scan_is_refused() {
    let site = Site::new();
    write(&site.proj().join("a.txt"), b"one");
    write(&site.proj().join("b.txt"), b"two");
    worktree::scan(&*site.db, site.pid, &site.proj(), false, 10).unwrap();

    // 手工伪造：时间戳还是本轮的，但 blob_hash 已经不是这个文件的内容了。
    let key = keys::worktree_key(site.pid, "b.txt");
    let raw = {
        let tx = site.db.begin();
        let got = tx.get(&key).unwrap();
        tx.commit();
        got
    };
    let mut e: worktree::WtEntry = postcard::from_bytes(keys::strip(&raw).unwrap()).unwrap();
    e.blob_hash = [0xEEu8; 32];
    let tx = site.db.begin();
    tx.set(&key, keys::wrap(postcard::to_allocvec(&e).unwrap()));
    tx.commit();

    let err = site
        .try_build(cas::DEFAULT_BLOCK)
        .expect_err("必须报错，不许提交一个没被扫描证实过的内容");
    assert!(matches!(err, Error::StaleScan { .. }), "实际是 {err}");
}

/// 流式读 `W`：活行数、总行数、epoch 都得对，而且**不许把不活的行吐出去**。
#[test]
fn live_rows_stream_and_count() {
    let site = Site::new();
    for i in 0..5 {
        write(&site.proj().join(format!("f{i}.txt")), b"x");
    }
    site.scan();

    let mut rows = build::LiveRows::new(&*site.db, site.pid);
    let mut paths = Vec::new();
    for item in &mut rows {
        let (path, _) = item.unwrap();
        paths.push(path);
    }
    assert!(rows.complete, "扫描完整跑完就该有 Z 标记");
    assert_eq!(paths.len(), 5, "引擎按字节序给，所以这里天然升序");
    assert_eq!(paths, {
        let mut v = paths.clone();
        v.sort();
        v
    });
    assert_eq!((rows.live, rows.seen_rows), (5, 5));
    assert!(rows.mark > 0, "epoch 来自扫描标记");
}

/// 上限现在守在**真正还在攒内存的那一处**：`plan` 要把算出来的树留给 diff 递归。
/// `commit` 这一侧已经没有条目上限了（树直接进库，内存只剩"最大单目录 + 深度"）。
#[test]
fn plan_is_capped_where_it_still_accumulates() {
    let site = Site::new();
    for i in 0..5 {
        write(&site.proj().join(format!("f{i}.txt")), b"x");
    }
    site.scan();
    match build::plan_capped(&*site.db, site.pid, 3) {
        // 上限数的是"攒在内存里的条目总数"：先结算的是根目录那一层，5 个文件一次超掉。
        Err(Error::TooManyEntries { n, max }) => assert_eq!((n, max), (5, 3)),
        other => panic!(
            "超限该报 TooManyEntries，实际 {:?}",
            other.map(|b| b.entries)
        ),
    }
    // 上限放宽之后就正常：报错必须是"拒绝"，不是把机器 OOM 掉（NFR-3 红线）。
    assert_eq!(
        build::plan_capped(&*site.db, site.pid, 1000)
            .unwrap()
            .entries,
        5
    );
}

/// 一轮扫描**中途被打断**不再伪装成大规模删除。
///
/// 老写法拿"行里最大的 scanned_at"当 epoch，于是半轮新数据 + 半轮旧数据 ⇒ 旧的那半
/// 看起来全被删了，一次 Ctrl-C 就制造一次假删除（只等 FR-10 去拦）。现在 epoch 只由
/// **完整跑完**的扫描推进，活性判据是 `>=`：中断那半轮盖过的行更新、照样算活，
/// 没盖到的行还留着上一次完整扫描的章 ⇒ 一条都不丢。
#[test]
fn an_interrupted_scan_does_not_look_like_a_mass_deletion() {
    let site = Site::new();
    for i in 0..6 {
        write(&site.proj().join(format!("f{i}.txt")), b"x");
    }
    let first = site.commit(cas::DEFAULT_BLOCK, false).commit.unwrap();

    // 手工模拟"扫描走到一半被杀"：只有两行被盖到更新时间戳。
    let later = site.epoch() + 1_000_000;
    for p in ["f0.txt", "f1.txt"] {
        let key = keys::worktree_key(site.pid, p);
        let raw = {
            let tx = site.db.begin();
            let got = tx.get(&key).unwrap();
            tx.commit();
            got
        };
        let mut e: worktree::WtEntry = postcard::from_bytes(keys::strip(&raw).unwrap()).unwrap();
        e.scanned_at = later;
        let tx = site.db.begin();
        tx.set(&key, keys::wrap(postcard::to_allocvec(&e).unwrap()));
        tx.commit();
    }

    // 计数器要迭代完才有效（它是流式的，没有"预先数好"这一步）。
    let mut rows = build::LiveRows::new(&*site.db, site.pid);
    let n = rows.by_ref().count();
    assert_eq!(
        (n, rows.live as usize, rows.seen_rows),
        (6, 6, 6),
        "六行都该算活着（两行更新、四行是上一次完整扫描的章）"
    );

    let again = site.commit(cas::DEFAULT_BLOCK, false);
    assert_eq!(again.commit, None, "什么都没变 ⇒ 不该产生提交");
    assert_eq!(again.changes.delete, 0, "半轮扫描不许被读成删除");
    assert_eq!(site.head(), vec![first]);
}

/// 回归：`collect_children` 早先用 `split_opt` 取父目录，而**顶层目录里没有 `/`** ⇒ 返回
/// None 被 `continue` 掉，于是整个 `top/` 连同它下面的一切从历史里静默消失，
/// 而块照样落盘、`entries` 照样是 4、其余所有用例照样绿。
/// 这是本项目里第一个"只有查树本身才能发现"的 bug，所以这条不许省。
#[test]
fn nested_directories_survive_into_history() {
    let site = Site::new();
    write(&site.proj().join("top/inner/deep.txt"), b"deep");
    write(&site.proj().join("top/leaf.txt"), b"leaf");
    let commit = site.commit(cas::DEFAULT_BLOCK, false).commit.unwrap();

    let tree = object::read_commit(&*site.db, site.pid, &commit)
        .unwrap()
        .tree;
    let root = object::read_tree(&*site.db, site.pid, &tree).unwrap();
    assert_eq!(names(&root), vec!["top"], "根清单里必须有顶层目录本身");
    assert_eq!(root[0].kind, Kind::Dir);

    let top = object::read_tree(&*site.db, site.pid, &root[0].hash).unwrap();
    assert_eq!(names(&top), vec!["inner", "leaf.txt"]);
    let inner = object::read_tree(&*site.db, site.pid, &top[0].hash).unwrap();
    assert_eq!(names(&inner), vec!["deep.txt"]);

    // 目录也算条目：top + inner + 两个文件 = 4。
    let built = site.commit(cas::DEFAULT_BLOCK, false);
    assert_eq!(built.changes.add, 0, "第二次提交没有改动");
}

fn names(entries: &[object::TreeEntry]) -> Vec<&str> {
    entries.iter().map(|e| e.name.as_str()).collect()
}

/// `plan`（`status`/`diff` 的底座）必须**一个字节都不写**，且算出来的树与随后真
/// commit 的树完全一致。第二条才是这条用例的价值：两条路径共用 `assemble`，
/// 一旦哪天有人给 `build` 加了 `plan` 没跟上的步骤，`status` 就开始骗人而测试全绿。
#[test]
fn plan_writes_nothing_and_matches_the_commit_that_follows() {
    let site = Site::new();
    write(&site.proj().join("top/leaf.txt"), b"one");
    site.commit(cas::DEFAULT_BLOCK, false);

    write(&site.proj().join("top/new.txt"), b"two");
    fs::remove_file(site.proj().join("top/leaf.txt")).unwrap();
    site.scan();

    let rows = site.db.health().total_rows;
    let p = build::plan(&*site.db, site.pid).unwrap();
    assert_eq!(
        site.db.health().total_rows,
        rows,
        "看一眼不该产生任何版本行"
    );
    assert_eq!(
        (p.changes.add, p.changes.modify, p.changes.delete),
        (1, 0, 1)
    );

    let b = site.build(cas::DEFAULT_BLOCK, false);
    assert_eq!(b.tree, p.tree, "plan 的树必须等于真 commit 的树");
    assert_eq!(
        b.changes.sample, p.changes.sample,
        "status 列出的路径必须与随后提交的一致"
    );
}

/// 列表有界、计数全量，且样本给的是**能直接喂给 `restore` 的完整路径**。
#[test]
fn change_sample_lists_full_paths_and_stops_at_the_cap() {
    let site = Site::new();
    for i in 0..40 {
        write(&site.proj().join(format!("dir{i}/f.txt")), b"x");
    }
    site.commit(cas::DEFAULT_BLOCK, false);
    for i in 0..40 {
        fs::remove_file(site.proj().join(format!("dir{i}/f.txt"))).unwrap();
    }
    // 40 / 80 行 = 50% ⇒ 触发 FR-10，这一条要签字（空掉的目录本身没被删，不算删除）。
    let built = site.commit(cas::DEFAULT_BLOCK, true);
    let cs = &built.changes;
    assert_eq!(cs.delete, 40);
    assert_eq!(cs.sample.len(), build::SAMPLE_MAX, "列表必须有界");
    assert_eq!(cs.sample_more, 40 - build::SAMPLE_MAX);
    assert!(
        cs.sample
            .iter()
            .all(|c| c.what == build::ChangeKind::Delete),
        "样本里混进了别的类型：{:?}",
        cs.sample[0]
    );
    assert!(
        cs.sample.iter().all(|c| c.path.ends_with("/f.txt")),
        "路径得是完整的（{:?}），只给文件名的话 restore 不知道该恢复哪个",
        cs.sample[0].path
    );
}

/// `diff --at <旧版本>` 走的是同一个 `diff`，但比较对象是任意两棵树。
/// 顺序也钉一下：样本按目录内名字升序，输出才可比对。
#[test]
fn diff_against_an_older_commit_lists_paths_in_order() {
    let site = Site::new();
    write(&site.proj().join("a.txt"), b"v1");
    let v1 = site.commit(cas::DEFAULT_BLOCK, false).commit.unwrap();
    write(&site.proj().join("a.txt"), b"v2-longer");
    write(&site.proj().join("b.txt"), b"new");
    let v2 = site.commit(cas::DEFAULT_BLOCK, false).commit.unwrap();

    let t1 = object::read_commit(&*site.db, site.pid, &v1).unwrap().tree;
    let t2 = object::read_commit(&*site.db, site.pid, &v2).unwrap().tree;
    let cs = build::diff(
        &*site.db,
        site.pid,
        Some(&t1),
        &t2,
        &history::TreeStore::default(),
    )
    .unwrap();
    assert_eq!((cs.add, cs.modify, cs.delete), (1, 1, 0));
    let got: Vec<(String, String)> = cs
        .sample
        .iter()
        .map(|c| (c.what.sign().to_string(), c.path.clone()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("~".to_string(), "a.txt".to_string()),
            ("+".to_string(), "b.txt".to_string())
        ]
    );
}
