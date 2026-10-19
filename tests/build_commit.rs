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

/// §17 补记十五（2026-10-17，由 §10.1(b) 的随机用例抓出来的真 bug）。
///
/// `src`（目录）与 `src.rs`（兄弟文件）这一对名字在 `W` 的**字节序**里排成
/// `src` < `src.rs` < `src/main.rs`——目录自己那一行与它的子项被隔开了。流式建树的栈只问
/// "上一层走完没有"，于是把 `src` 结算两次，父层出现两条同名条目，`commit` 报
/// "目录清单里有重名条目"。修法是**目录行的键尾带 `/`**（`keys::worktree_dir_key`），
/// 让"目录自己 + 它全部子项"共享前缀、在字节序里必然连续。
///
/// 这条用例钉的是结果：这种形状必须提得动，而且 `src` 在父层只许有一条。
#[test]
fn prefix_colliding_sibling_builds_exactly_one_dir_entry() {
    let site = Site::new();
    let proj = site.proj();
    write(&proj.join("src/main.rs"), b"fn main() {}");
    write(&proj.join("src.rs"), b"stub");
    write(&proj.join("src/util/cmp.rs"), b"cmp"); // 第二层也要被隔开一次才算数
    write(&proj.join("src0"), b"other"); // '/'(0x2F) < '0'(0x30)：另一头的邻居

    let built = site.commit(cas::DEFAULT_BLOCK, false);
    assert!(built.commit.is_some(), "这一串名字必须提得动");
    let root = object::read_tree(&*site.db, site.pid, &built.tree).unwrap();
    let named = |n: &str| root.iter().filter(|e| e.name == n).count();
    assert_eq!(named("src"), 1, "目录 src 只许有一条：{:?}", root);
    assert_eq!(named("src.rs"), 1, "4 个文件 + 2 个目录都在账上");
    assert_eq!(named("src0"), 1);
    assert_eq!(built.entries, 6, "4 文件 + src + src/util");

    // 而且不是一条"空目录"：子项必须真的在里面。
    let src = root.iter().find(|e| e.name == "src").expect("src 该在根层");
    assert_eq!(src.kind, Kind::Dir);
    let inner = object::read_tree(&*site.db, site.pid, &src.hash).unwrap();
    assert!(
        inner.iter().any(|e| e.name == "main.rs" && e.kind == Kind::File),
        "src 层内容不对：{:?}",
        inner
    );
    let util = inner.iter().find(|e| e.name == "util").expect("util 该在 src 里");
    assert_eq!(util.kind, Kind::Dir);
    let deep = object::read_tree(&*site.db, site.pid, &util.hash).unwrap();
    assert_eq!(deep.len(), 1, "src/util 只该有 cmp.rs：{:?}", deep);
    assert_eq!(deep[0].name, "cmp.rs");
}

/// 同一族形状，走**只读**那条路：`plan` 与 `commit` 共用同一个遍历，重名条目不该只出现在写侧。
#[test]
fn plan_tolerates_the_same_shape() {
    let site = Site::new();
    let proj = site.proj();
    write(&proj.join("data/x.bin"), b"x");
    write(&proj.join("data.bin/y.bin"), b"y");
    write(&proj.join("data.bin/y.bin.z"), b"z");
    site.scan();
    let built = build::plan(&*site.db, site.pid).expect("plan 不许因为名字撞前缀就报错");
    assert_eq!(built.entries, 5, "3 文件 + data + data.bin");
}

/// 限定子树那一趟**不许把子树自己的目录行也算成一个子项**。
///
/// `LiveRows::with_prefix("d")` 的键前缀就是 `W|p|d/`，而目录行 `d` 的键（补记十五之后带
/// 尾随斜杠）**正好等于前缀** ⇒ 尾巴是空串。收进来会得到一个尾随斜杠的路径 `d/`，
/// 下游 `align` 的补帧能转成死循环（2026-10-17 键序改动第一版就把 `checkout --prefix` 挂住了）。
#[test]
fn subtree_scan_excludes_the_subtrees_own_dir_row() {
    let site = Site::new();
    let proj = site.proj();
    write(&proj.join("a.txt"), b"a");
    write(&proj.join("d/b.txt"), b"b");
    write(&proj.join("d/e/c.txt"), b"c");

    let built = site.commit(cas::DEFAULT_BLOCK, false);
    assert_eq!(built.entries, 5, "a.txt + d + d/b.txt + d/e + d/e/c.txt");

    let root = object::read_tree(&*site.db, site.pid, &built.tree).unwrap();
    let d = root.iter().find(|e| e.name == "d").expect("根层该有 d");
    assert_eq!(d.kind, Kind::Dir);

    let sub = build::compare_subtree(&*site.db, site.pid, None, "d").unwrap();
    assert_eq!(
        sub.entries, 3,
        "子树那趟只该看见 d 的子项：b.txt + e + e/c.txt"
    );
    assert_eq!(
        sub.tree, d.hash,
        "单独走一棵子树必须得到与全树里那一格**同一个** hash"
    );
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

/// 上限盖在**单个目录**上，不再盖在项目总数上。
///
/// 这条是融合遍历的回归测试：老写法里 `plan` 要把整棵新树攒在内存中留给 diff 递归，
/// 于是"项目文件多到一定程度就连 `status` 都跑不动"。现在比对压在每层结算时做完，
/// 清单出栈即丢，所以只有"某一层大到装不下"才会撞线。
#[test]
fn the_cap_is_per_directory_not_per_project() {
    let site = Site::new();
    for d in 0..5 {
        for i in 0..5 {
            write(&site.proj().join(format!("dir{d}/f{i}.txt")), b"x");
        }
    }
    site.scan();
    // 25 个文件（+ 5 个目录行 = 30 条 W）> 上限 6，但没有任何**一层**超过 6 ⇒ 必须能
    // 正常算完（老写法在这里就报错，因为它攒的是全项目的条目表）。
    assert_eq!(
        build::plan_capped(&*site.db, site.pid, 6).unwrap().entries,
        30
    );

    // 同一批数据挤在一层里就超限：报的是**拒绝**，不是把机器 OOM 掉（NFR-3 红线），
    // 而且必须说清是哪一层——只报"超上限"用户不知道该拆哪个目录。
    for i in 0..5 {
        write(&site.proj().join(format!("flat{i}.txt")), b"x");
    }
    site.scan();
    match build::plan_capped(&*site.db, site.pid, 6) {
        // 根帧此时已有 5 个目录条目，`flat0..2` 依次进账，第 7 条越线。
        Err(Error::TooManyEntries { n, max, dir }) => {
            assert_eq!((n, max), (7, 6));
            assert_eq!(dir, ".", "根目录的报错路径要写 `.`，不能是个空字符串");
        }
        other => panic!(
            "超限该报 TooManyEntries，实际 {:?}",
            other.map(|b| b.entries)
        ),
    }
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

/// `diff --from/--to` 那一类比对走 `build::diff`：**两棵树都在库里**，自顶向下、
/// 未变子树整块跳过。顺序也钉一下：样本按目录内名字升序，输出才可比对。
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
    let cs = build::diff(&*site.db, site.pid, Some(&t1), &t2).unwrap();
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

/// `diff --at <rev>` 走的是融合遍历（工作树那侧的树从来没进过库），所以它必须与
/// `plan`（同一趟、参照换成 HEAD）给出口径一致的结果。
///
/// 这一条盯的是新写法的两个静默失败面：`at` 前缀拼错会少一整棵子树，
/// 参照侧选错版本会把"跟谁比"答错。
#[test]
fn compare_against_any_rev_matches_the_walk_that_reads_head() {
    let site = Site::new();
    write(&site.proj().join("keep/old.txt"), b"v1");
    write(&site.proj().join("keep/new.txt"), b"only-in-later");
    let v1 = site.commit(cas::DEFAULT_BLOCK, false).commit.unwrap();
    fs::remove_file(site.proj().join("keep/new.txt")).unwrap();
    fs::write(site.proj().join("keep/old.txt"), b"v2-longer").unwrap();
    write(&site.proj().join("other/x.bin"), b"x");
    site.commit(cas::DEFAULT_BLOCK, false);

    // 工作树 == HEAD ⇒ `plan` 说没有变更。
    assert!(build::plan(&*site.db, site.pid).unwrap().changes.is_empty());
    // 同一份工作树跟 v1 比 ⇒ 一处改、一处删、一处新增（整个 other/ 子树各记一次）。
    let t1 = object::read_commit(&*site.db, site.pid, &v1).unwrap().tree;
    let cs = build::compare(&*site.db, site.pid, Some(t1))
        .unwrap()
        .changes;
    assert_eq!((cs.add, cs.modify, cs.delete), (2, 1, 1), "{:?}", cs.sample);
    let mut got: Vec<String> = cs
        .sample
        .iter()
        .map(|c| format!("{} {}", c.what.sign(), c.path))
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            "+ other".to_string(),
            "+ other/x.bin".to_string(),
            "- keep/new.txt".to_string(),
            "~ keep/old.txt".to_string(),
        ]
    );
}

/// `status` 的实时那一半：**`W` 说干净，盘上说有出入**，两个都得看得见。
///
/// 这条钉的是整个功能的立身之处：出入**只报不进账**。§8「扫描器不许判删除」一个字没改——
/// 真要把这次删除变成历史，还得走 `scan` → `commit`（并且照旧过 FR-10）。
#[test]
fn unscanned_changes_show_in_reconcile_but_not_in_the_book() {
    let site = Site::new();
    write(&site.proj().join("keep/a.txt"), b"a");
    write(&site.proj().join("keep/c.txt"), b"c");
    site.commit(cas::DEFAULT_BLOCK, false);

    // 扫描之后：新增一个、删掉一个，都不再 scan。
    write(&site.proj().join("keep/b.txt"), b"b");
    fs::remove_file(site.proj().join("keep/a.txt")).unwrap();

    // 第一步：走一趟盘选可疑层。动了 `keep/` ⇒ 它必须进来；根**也可能**进来，
    // 因为判据带 1 s 的回写滞后余量（`MTIME_LAG`：NTFS 的父目录时间戳滞后实测几十 ms），
    // 而用例里根是几毫秒前刚建的。多比一层只花 readdir，不影响结果。
    let epoch = site.epoch();
    let view = worktree::view_since(&site.proj(), epoch).unwrap();
    assert!(
        view.suspects.iter().any(|s| s == "keep"),
        "被动过的那层该进可疑集合：{:?}",
        view.suspects
    );
    let suspects = view.suspects.clone();

    // 第二步：`plan` 本来要走的那趟 `W` 流顺路收名单（**不另开一次分页扫描**）。
    let rows = site.db.health().total_rows;
    let want: std::collections::HashSet<&str> = suspects.iter().map(String::as_str).collect();
    let mut db_names: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    let p = build::plan_observed(&*site.db, site.pid, &mut |path, _e| {
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        if want.contains(parent) {
            db_names
                .entry(parent.to_string())
                .or_default()
                .push(name.to_string());
        }
    })
    .unwrap();
    assert!(
        p.changes.is_empty(),
        "没扫描就不该有变更进账：{:?}",
        p.changes
    );

    // 第三步：比那一层。一增一删，名单是给人的。
    let rec = worktree::reconcile(&view, &db_names);
    assert_eq!((rec.only_disk, rec.only_w), (1, 1), "{rec:?}");
    let mut got = rec.sample.clone();
    got.sort();
    assert_eq!(
        got,
        vec!["+ keep/b.txt".to_string(), "- keep/a.txt".to_string()],
        "路径要能直接拿去 `ls`：{:?}",
        rec.sample
    );
    assert_eq!(
        site.db.health().total_rows,
        rows,
        "对账是只读的：一行都不许多（§10.1 只读判据）"
    );
}

/// 建一个 `dirs` 个目录（各 4 个文件）的站点：扫一次 + 提一次，然后只跑 `plan`，
/// 返回**这一次 plan** 的折数账与清单装载读数。规模做成参数，是因为要判的是"随不随规模长"。
fn plan_at_two_scales(dirs: usize) -> (twosync::utils::CallCounts, build::ManifestLoad) {
    let tmp = tempfile::tempdir().unwrap();
    for d in 0..dirs {
        for i in 0..4 {
            write(
                &tmp.path()
                    .join("p")
                    .join(format!("d{d:03}"))
                    .join(format!("f{i}.bin")),
                format!("内容 {d}-{i}"),
            );
        }
    }
    let meter = twosync::utils::Meter::default();
    let db = twosync::utils::counted(
        open_store(tmp.path(), EngineKind::LsmTree).unwrap(),
        &meter,
        false,
    );
    let pid = keys::project_id("p");
    let proj = tmp.path().join("p");
    worktree::scan(&*db, pid, &proj, false, 100).unwrap();
    let store = Cas::new(&objects_dir(tmp.path()), cas::DEFAULT_BLOCK).unwrap();
    build::build(
        &*db,
        pid,
        &store,
        &proj,
        build::Meta {
            peer: "test-peer",
            msg: "起点",
            ts: 1,
            allow_mass_delete: false,
        },
    )
    .unwrap();
    // 起点之后才算账：前面那次 commit 也走装载器，混进来就读不出 plan 自己的形状。
    let base = meter.snapshot();
    let b = build::plan(&*db, pid).unwrap();
    (meter.snapshot().since(base), b.manifests)
}

/// 这一条是这次改写（O(目录数) 次点读 ⇒ 一次 `T|p` 装载）的**形状判据**。
///
/// 不写"plan 只许折 N 次"那种绝对数——那会被 `LiveRows` 的分页宽度牵着走，一改就红。
/// 要判的是**随不随目录数长**：同一构建、两种规模（10 vs 40 个目录 ⇒ 4 倍清单），
/// 点读账必须一模一样。原来它是 10 与 40（每条一次 `read_tree`），现在它是常数。
#[test]
fn plans_reference_reads_do_not_grow_with_the_number_of_directories() {
    let (small, lm) = plan_at_two_scales(10);
    let (big, bm) = plan_at_two_scales(40);
    assert_eq!(
        small.point_read, big.point_read,
        "参照侧的点读账随目录数长了（装载器没生效？）：{} vs {}",
        small.line(),
        big.line()
    );
    assert!(
        small.point_read <= 2,
        "plan 的参照侧只剩个位数点读才对，实际 {}",
        small.line()
    );
    // 反过来也要证明"这两个站点确实不一样大"——否则相等是因为两边都没干活。
    assert!(
        lm.rows < bm.rows,
        "装载的行数必须随目录数长：{} vs {}",
        lm.rows,
        bm.rows
    );
    assert!(
        bm.rows >= 40,
        "40 个目录至少该有 40 张清单，实际 {}",
        bm.rows
    );
    assert!(
        !bm.over_budget && !lm.over_budget,
        "这两档远不到 4 MiB 预算，不该退回点读"
    );
    // 内存判据用**行数与字节数**，不用进程高水位（§11.4b 结论 3）。
    assert!(
        bm.bytes < object_budget_probe(),
        "装载的 value 字节必须报得出一个有界的数，实际 {:.2} MiB",
        bm.bytes as f64 / 1048576.0
    );
}

/// 预算本身也进判据：用例不许假设一个魔法数。
fn object_budget_probe() -> u64 {
    twosync::store::object::TreeIndex::BUDGET_BYTES
}

// ── 窄扫那两轮（§11.4b 的边界，现在兑现成代码）───────────────────────────────
//
// 事件轮只走变化的那几个目录，**不推进活性线**（`Scope::Dir` 不写 `Z`）。
// 于是这一轮：新增/修改照常进历史，**删除一个都不报** —— 那正是"宁可慢，不可假删"的代价。
// 下一轮全量才把删除确认掉。两条用例分别钉这两半。

#[test]
fn a_narrow_round_reports_no_deletions_and_the_next_full_round_does() {
    let site = Site::new();
    let proj = site.proj();
    write(&proj.join("a/x.bin"), b"the x");
    write(&proj.join("b/y.bin"), b"the y");
    let first = site.commit(cas::DEFAULT_BLOCK, false);
    assert_eq!(first.changes.add, 4, "两个文件 + 两个目录：{first:?}");

    // 盘上删掉 a/x.bin，但这一轮**只窄扫 b/**（模拟：事件落在 b）。
    fs::remove_file(proj.join("a/x.bin")).expect("删文件");
    worktree::scan_scoped(
        &*site.db,
        site.pid,
        &proj,
        &worktree::Scope::Dir("b".to_string()),
        false,
        500,
    )
    .expect("窄扫");
    let second = site.build(cas::DEFAULT_BLOCK, false);
    assert_eq!(
        second.changes.delete, 0,
        "窄扫轮报出了删除 ⇒ 那就是假删除的形状（没走过 a/ 凭什么说 x.bin 没了）"
    );
    assert_eq!(second.changes.add, 0, "b/ 没变就不该有新增");
    assert!(
        second.commit.is_none(),
        "什么都没变的一轮不该产生提交，也不该移 head"
    );

    // 下一次**全量**轮：a/ 被走了一遍，x.bin 没被盖章 ⇒ 删除在这里被确认。
    site.scan();
    let third = site.build(cas::DEFAULT_BLOCK, false);
    assert_eq!(
        third.changes.delete, 1,
        "全量轮必须把那一笔删除补上，否则窄扫就成了永久漏记"
    );
    assert!(third.commit.is_some(), "有变更就该提交");
    let listed = build::ref_heads(&*site.db, site.pid, build::MAIN).unwrap();
    assert_eq!(listed.len(), 1, "还是单 head：{listed:?}");
}

#[test]
fn a_narrow_round_picks_up_a_new_file_in_the_scanned_dir() {
    // 另一半：窄扫买到的东西。事件所在目录里的**新增**必须当场进历史，
    // 否则"事件驱动的秒级同步"就是一句空话（只剩省时间，没有更快）。
    let site = Site::new();
    let proj = site.proj();
    write(&proj.join("a/x.bin"), b"the x");
    write(&proj.join("b/y.bin"), b"the y");
    site.commit(cas::DEFAULT_BLOCK, false);

    write(&proj.join("b/z.bin"), b"the z");
    worktree::scan_scoped(
        &*site.db,
        site.pid,
        &proj,
        &worktree::Scope::Dir("b".to_string()),
        false,
        500,
    )
    .expect("窄扫 b/");
    let built = site.build(cas::DEFAULT_BLOCK, false);
    assert_eq!(built.changes.add, 1, "只该多那一个新文件：{built:?}");
    assert_eq!(built.changes.delete, 0, "a/ 根本没被走过 ⇒ 不许报任何删除");
    assert!(built.commit.is_some(), "有新增就该提交");
    // 树里确实有它：`b/z.bin` 在，`a/x.bin` 也还在（活性线还是上一次全量轮那条）。
    let heads = build::ref_heads(&*site.db, site.pid, build::MAIN).unwrap();
    let c = object::read_commit(&*site.db, site.pid, &heads[0]).unwrap();
    let root = object::read_tree(&*site.db, site.pid, &c.tree).unwrap();
    let b_dir = root.iter().find(|e| e.name == "b").expect("根上要有 b");
    let kids = object::read_tree(&*site.db, site.pid, &b_dir.hash).unwrap();
    assert_eq!(kids.len(), 2, "b/ 里 y 与 z：{kids:?}");
}

/// 复用那一条的另一半：**没在历史里的东西不能凭空引用**。
/// 文件是这轮新建的、随后又被删掉 ⇒ `B` 行不存在 ⇒ 我们从来没拿到过它的字节，必须报错，
/// 不许造一条"引用不存在内容"的清单（那正是 §9.3 要防的悬空形状）。
#[test]
fn a_file_that_vanished_before_it_was_ever_committed_is_an_error() {
    let site = Site::new();
    let proj = site.proj();
    write(&proj.join("a/x.bin"), b"the x");
    site.commit(cas::DEFAULT_BLOCK, false);

    // 新建一个文件、扫到它（于是有了 `W` 行与 `blob_hash`），然后在 commit 之前删掉。
    write(&proj.join("a/fresh.bin"), b"never committed");
    site.scan();
    fs::remove_file(proj.join("a/fresh.bin")).expect("删掉新文件");
    let err = site
        .try_build(cas::DEFAULT_BLOCK)
        .expect_err("内容从没进过历史 ⇒ 不能凭空引用");
    let msg = err.to_string();
    assert!(msg.contains("还没进过任何历史版本"), "要说清为什么不能复用：{msg}");
    assert!(
        site.head().len() == 1,
        "失败的那次提交不许留下半个版本"
    );
}
