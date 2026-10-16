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
use twosync::store::build::{self, BlockMeta, MAX_ENTRIES};
use twosync::store::cas::{self, Cas};
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

/// 条目表是 O(文件数) 的权宜实现 ⇒ 超限必须**拒绝**，不是把机器 OOM 掉（NFR-3）。
#[test]
fn too_many_rows_are_refused_not_oomed() {
    let site = Site::new();
    for i in 0..5 {
        write(&site.proj().join(format!("f{i}.txt")), b"x");
    }
    worktree::scan(&*site.db, site.pid, &site.proj(), false, 10).unwrap();

    let snap = build::snapshot(&*site.db, site.pid, MAX_ENTRIES).unwrap();
    assert_eq!(snap.entries.len(), 5);
    assert_eq!(snap.known, 5);
    assert!(snap.epoch > 0, "epoch 来自扫描时间戳");

    match build::snapshot(&*site.db, site.pid, 3) {
        Err(Error::TooManyEntries { n, max }) => assert_eq!((n, max), (4, 3)),
        other => panic!("超限该报 TooManyEntries，实际 {other:?}"),
    }
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
