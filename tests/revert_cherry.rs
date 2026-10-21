//! `revert` 与 `cherry-pick`（M4 第二层第四、五条）。它们共用一次**结构三方合并**，只是参数摆位不同
//! （`ours` 永远是当前 head；撤销时另一侧是**亲代那一版**、摘取时另一侧是**那个 commit**）。
//!
//! 用例的重心不在"改动对不对"（那是五规则本身，已在 `merge_keep_both.rs` 钉过），而在**这两条命令
//! 各自搬来的三道闸**：
//! 1. **只报那一趟一个字都不写**（FR-7）——连清单行都不许写，所以命令打印的那句必须有实现；
//! 2. **FR-10 在写任何东西之前过**：撤掉一个"加了 25 个文件"的 commit 就是 25 条删除；
//! 3. **不许猜**：多 head 时落点不唯一 ⇒ 拒；撤一个合并 commit 要先说撤哪一边 ⇒ 不给 `-m` 这个旋钮；
//!    同一个文件两边各改 ⇒ 报冲突、拒绝、head 不动。
//!
//! 另外钉两条语义性的：撤销一个"删除"是把文件**取回来**（不是再删一次），以及**不产生空提交**
//! （碎 commit 会把引擎版本链白白撑大，§9.6.2）。

mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use storage_engines::KvEngine;
use twosync::cli::m0;
use twosync::cli::m4;
use twosync::cli::registry::Registry;
use twosync::cli::{Cmd, Cli};
use twosync::store::build::{self, MAIN};
use twosync::store::cas::Cas;
use twosync::store::keys::{self, Hash, ProjectId};
use twosync::store::{object, worktree};
use twosync::utils::objects_dir;

const BLK: u64 = 4096;

struct Site {
    tmp: tempfile::TempDir,
    db: Arc<dyn KvEngine>,
    pid: ProjectId,
}

impl Site {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".twosync")).unwrap();
        common::pin_engine_in_config(tmp.path());
        let db: Arc<dyn KvEngine> = Arc::from(twosync::utils::open_store(tmp.path(), common::engine()).unwrap());
        let mut reg = Registry::default();
        reg.add("p", "p").unwrap();
        reg.save(tmp.path()).unwrap();
        Self {
            tmp,
            db,
            pid: keys::project_id("p"),
        }
    }

    fn root(&self) -> PathBuf {
        self.tmp.path().to_path_buf()
    }

    fn work(&self) -> PathBuf {
        self.root().join("p")
    }

    fn write(&self, rel: &str, body: &str) {
        let p = self.work().join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    fn rm(&self, rel: &str) {
        fs::remove_file(self.work().join(rel)).unwrap();
    }

    fn exists(&self, rel: &str) -> bool {
        self.work().join(rel).exists()
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.work().join(rel)).unwrap()
    }

    /// `scan` + `commit`，返回那一个新 commit。
    fn commit(&self, msg: &str) -> Hash {
        self.scan();
        let store = Cas::new(&objects_dir(&self.root()), BLK).unwrap();
        build::build(
            &*self.db,
            self.pid,
            &store,
            &self.work(),
            build::Meta {
                peer: "t",
                msg,
                ts: 1,
                allow_mass_delete: true,
            },
        )
        .unwrap()
        .commit
        .expect("用例里每次都要有改动")
    }

    /// 只扫不提。**落盘类命令判断"磁盘上现在有什么"吃的就是这一份快照**，所以在用例里
    /// 从外面动过文件之后必须补一次（最后一条用例专门钉住"忘了补"的后果）。
    fn scan(&self) {
        let work = self.work();
        worktree::scan(&*self.db, self.pid, &work, false, 100).unwrap();
    }

    fn cli(&self) -> Cli {
        let cli = Cli {
            root: self.root(),
            project: Some("p".to_string()),
            engine: None,
            io_bucket: Default::default(),
            store_slot: Default::default(),
            cmd: Cmd::Init,
        };
        let _ = cli.store_slot.set(self.db.clone());
        cli
    }

    /// 命令层入口：绕过 `m4::revert` 去直接调 `merge_trees` 就等于测了另一份实现。
    fn revert(&self, rev: &str, apply: bool) -> twosync::error::Result<()> {
        m4::revert(&self.cli(), rev, apply, false)
    }

    fn revert_allow_mass(&self, rev: &str) -> twosync::error::Result<()> {
        m4::revert(&self.cli(), rev, true, true)
    }

    fn cherry(&self, rev: &str, apply: bool) -> twosync::error::Result<()> {
        m4::cherry_pick(&self.cli(), rev, apply, false)
    }

    fn checkout(&self, rev: &Hash, apply: bool) -> twosync::error::Result<()> {
        m0::checkout(&self.cli(), &keys::hex(rev), None, apply, true, None, false)
    }

    fn heads(&self) -> Vec<Hash> {
        build::ref_heads(&*self.db, self.pid, MAIN).unwrap()
    }

    fn rows(&self) -> u64 {
        self.db.health().total_rows
    }
}

/// 撤销 = 把**亲代那一版**当另一侧再合一次：内容回到旧字节，工作树跟着落地。
#[test]
fn reverting_a_modify_puts_the_old_bytes_back_and_lands_the_worktree() {
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1");
    site.write("a.txt", "two");
    let v2 = site.commit("v2");
    assert_eq!(site.read("a.txt"), "two");

    // 先只报：一个字都不写
    let rows = site.rows();
    site.revert("HEAD", false).unwrap();
    assert_eq!(site.rows(), rows, "只报那一趟连清单行都不许写（FR-7）");
    assert_eq!(site.heads(), vec![v2], "只报不许动 head");
    assert_eq!(site.read("a.txt"), "two", "只报不许碰工作树");

    site.revert("HEAD", true).unwrap();
    let made = site.heads();
    assert_eq!(made.len(), 1);
    let c = object::read_commit(&*site.db, site.pid, &made[0]).unwrap();
    assert_eq!(c.parents, vec![v2], "revert 是**新增**一笔单亲 commit，不是把历史剪掉（§4 只增不减）");
    assert_eq!(c.tree, {
        // 结果那棵树应当与 v1 的树**同一棵**：内容寻址下同内容同 hash，撤销一次不该造出新清单
        object::read_commit(&*site.db, site.pid, &v1).unwrap().tree
    });
    assert!(c.msg.starts_with("revert "), "msg 要说出撤的是谁：{}", c.msg);
    assert_eq!(c.stats.modify, 1, "撤销一次修改就是账面上一条 modify：{:?}", c.stats);
    assert_eq!(site.read("a.txt"), "one", "--apply 之后工作树要落地");
}

/// 撤销一个**删除**是把文件取回来。判据故意选在"五规则里走的是第③条另一半"的那一支：
/// base = c 的树（没有这条路径）、ours 也没有、theirs = 亲代（有）⇒ 取 theirs。
/// 写反了会变成"再删一次"，而两种结果在 `log` 里长得一模一样。
#[test]
fn reverting_a_deletion_brings_the_file_back() {
    let site = Site::new();
    site.write("a.txt", "one");
    site.write("b.txt", "two");
    site.commit("v1");
    site.rm("b.txt");
    site.commit("v2 删了 b");
    assert!(!site.exists("b.txt"));

    site.revert("HEAD", true).unwrap();
    assert!(site.exists("b.txt"), "撤销删除要把文件取回来");
    assert_eq!(site.read("b.txt"), "two");
    let made = site.heads()[0];
    let c = object::read_commit(&*site.db, site.pid, &made).unwrap();
    assert_eq!(c.stats.add, 1, "取回一条 = add 1：{:?}", c.stats);
}

/// 同一个文件被"撤完之后"又改过一次 ⇒ 结构上两边各改成不同东西 ⇒ **拒绝，head 不动**。
/// 这一条是 §16.2 第三层那句"内容三方合并不做"的落点：不给猜一个版本，也不给 `.merge-file` 那种临时文件。
#[test]
fn reverting_a_file_that_was_touched_again_is_refused() {
    let site = Site::new();
    site.write("a.txt", "one");
    site.commit("v1");
    site.write("a.txt", "two");
    let v2 = site.commit("v2");
    site.write("a.txt", "three");
    let v3 = site.commit("v3");

    let e = site
        .revert(&keys::hex(&v2), true)
        .expect_err("两边各改过同一个文件 ⇒ 不许机器裁");
    assert!(e.to_string().contains("1 条同路径冲突"), "{e}");
    assert_eq!(site.heads(), vec![v3], "被拒那一趟 head 一个字都不动");
    assert_eq!(site.read("a.txt"), "three", "被拒那一趟工作树也不许动");
}

/// 多 head 时"撤到哪一个上面"没有定义 ⇒ 拒（§7.1 不许挑一个）。
#[test]
fn reverting_onto_an_ambiguous_head_set_is_refused() {
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1");
    site.write("a.txt", "two");
    let v2 = site.commit("v2");
    build::set_ref(&*site.db, site.pid, MAIN, &[v1, v2]).unwrap();

    let e = site.revert("HEAD", true).expect_err("两个 head 时不许挑一个当落点");
    assert!(e.to_string().contains("2 个"), "要说清有几个 head：{e}");
    assert_eq!(site.heads(), vec![v1, v2]);
}

/// 撤一个**合并 commit** 要先回答"撤哪一边"，那是裁决不是默认值 ⇒ 明说不给这个旋钮（git 要 `-m N`）。
#[test]
fn reverting_a_merge_commit_is_refused_rather_than_guessing_a_side() {
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1");
    site.write("b.txt", "two");
    let v2 = site.commit("v2");
    // 直接摆一笔两亲 commit 上台（测的是 `revert` 的拒绝，不是合并器）：树用 v2 那一棵就够了，
    // 这条用例要钉的是"两个亲代 ⇒ 不许猜哪一边"，与树的内容无关。
    let t = object::read_commit(&*site.db, site.pid, &v2).unwrap().tree;
    let m = object::put_commit(
        &*site.db,
        site.pid,
        &object::Commit {
            parents: vec![v1, v2],
            tree: t,
            peer: "local".to_string(),
            ts: 1,
            msg: "merge".to_string(),
            stats: object::Stats {
                add: 0,
                modify: 0,
                delete: 0,
                rename: 0,
                bytes: 0,
            },
        },
    )
    .unwrap();
    build::set_ref(&*site.db, site.pid, MAIN, &[m]).unwrap();

    let e = site.revert("HEAD", true).expect_err("两亲的撤哪边是裁决");
    let msg = e.to_string();
    assert!(msg.contains("2 个亲代") && msg.contains("-m"), "{msg}");
    assert_eq!(site.heads(), vec![m]);
}

/// 摘取 = 把"另一个 commit 相对它亲代的改动"搬到当前落点上。
/// 台架故意先 `reset` 回 v1（那条带 b.txt 的线不再在 head 上），再把它摘回来。
#[test]
fn cherry_picking_brings_another_lines_change_onto_the_current_head() {
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1");
    site.write("b.txt", "two");
    let v2 = site.commit("v2 加了 b");
    // 撤到 v2 之前的那一版 ⇒ head 只认 a.txt，但 v2 还在库里（tag 挂着它才有 pin，这里靠 revspec 点名）
    build::set_ref(&*site.db, site.pid, MAIN, &[v1]).unwrap();
    fs::remove_file(site.work().join("b.txt")).unwrap();
    // 补一次 `scan`：不然命令看到的是旧快照，还以为 b.txt 在位（最后一条用例钉的就是忘了补的后果）
    site.scan();
    assert!(!site.exists("b.txt"));

    // 只报那一趟不写
    let rows = site.rows();
    site.cherry(&keys::hex(&v2), false).unwrap();
    assert_eq!(site.rows(), rows, "只报那一趟一个字都不写");

    site.cherry(&keys::hex(&v2), true).unwrap();
    assert!(site.exists("b.txt"), "摘取要把那一条改动搬过来");
    let made = site.heads()[0];
    let c = object::read_commit(&*site.db, site.pid, &made).unwrap();
    assert_eq!(c.parents, vec![v1], "摘落点仍是当前 head，不是把 v2 那一条接上");
    assert_eq!(c.stats.add, 1, "{:?}", c.stats);
    assert!(c.msg.starts_with("cherry-pick "), "{msg}", msg = c.msg);
}

/// 摘一条**已经在落点上**的改动 ⇒ 什么都没带来 ⇒ 不许产生空提交（碎 commit 撑大引擎版本链，§9.6.2）。
///
/// 台架故意让 `theirs != ours`（落点比 v2 多一条 `c.txt`），这样走的是**第二道**空判：
/// 合并算完了、账面上却一条改动都没有。只挡"两侧同一棵树"那种是第一道判，挡不住这一种。
#[test]
fn picking_a_change_that_is_already_there_produces_no_empty_commit() {
    let site = Site::new();
    site.write("a.txt", "one");
    site.commit("v1");
    site.write("b.txt", "two");
    let v2 = site.commit("v2 加了 b");
    site.write("c.txt", "three");
    let v3 = site.commit("v3 又加了 c");

    let rows = site.rows();
    site.cherry(&keys::hex(&v2), true).unwrap();
    assert_eq!(site.heads(), vec![v3], "v2 的改动已经在落点上 ⇒ 不该再多一笔");
    assert_eq!(site.rows(), rows, "合并算完了但一条都没变 ⇒ 连对象都不许多写");
}

/// **FR-10 必须在新命令上照样生效**：撤掉一个"加了 25 个文件"的 commit 就是 25 条删除，
/// 正过绝对下限（20 条）那一道。带 `--allow-mass-delete` 才许落地。
#[test]
fn reverting_a_mass_add_hits_the_delete_gate_before_writing_anything() {
    let site = Site::new();
    site.write("seed.txt", "seed");
    site.commit("v1 只有 seed");
    for i in 0..25 {
        site.write(&format!("many/f{i}.txt"), &format!("body {i}"));
    }
    let v2 = site.commit("v2 加了 25 个");
    assert_eq!(site.heads(), vec![v2]);

    let rows = site.rows();
    let e = site
        .revert(&keys::hex(&v2), true)
        .expect_err("撤 25 条 = 大规模删除，该被熔断挡下");
    assert!(e.to_string().contains("删除"), "要报的是熔断而不是别的：{e}");
    assert_eq!(site.heads(), vec![v2], "挡下 ⇒ head 不动");
    assert_eq!(site.rows(), rows, "挡下 ⇒ 什么都没写（这道闸走在 put_commit 之前）");

    site.revert_allow_mass(&keys::hex(&v2)).unwrap();
    let made = site.heads()[0];
    assert_ne!(made, v2, "过了闸就该真落一笔");
    assert_eq!(
        object::read_commit(&*site.db, site.pid, &made).unwrap().parents,
        vec![v2],
        "撤销仍然是**新增**一笔，不是把 v2 剪掉"
    );
    assert!(!site.exists("many/f0.txt"), "过了闸就该真落地");
    assert!(
        site.exists("seed.txt"),
        "撤的只是 v2 那 25 条：seed 不在它的改动里，不该被牵动"
    );
}

/// **落地判的是最近一次 `scan` 的快照，不是当场 stat 每条路径**（那是 O(文件数) 次 `open` 的老账，
/// §11.4e）。这条用例是写 `cherry-pick` 时踩出来的，它同时管着 `checkout`/`reset --apply`：
/// 在工具外面删掉文件而没 `scan` ⇒ 命令认为那条路径已经在位，**不把她取回来**。
/// 后果不是坏数据（历史与 head 都对），是"以为落到位了而其实没落"，所以命令必须把这句打出来
/// （`m0::note_checkout_dirty`），而补一次 `scan` 就恢复正常 —— 两个方向都钉住。
#[test]
fn landing_trusts_the_last_scan_so_an_unscanned_external_delete_is_not_restored() {
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1");
    site.write("b.txt", "two");
    let v2 = site.commit("v2 加了 b");
    build::set_ref(&*site.db, site.pid, MAIN, &[v1]).unwrap();
    // 外面删掉，**故意不 scan**：`W` 里还留着 b.txt 那一行
    fs::remove_file(site.work().join("b.txt")).unwrap();

    site.cherry(&keys::hex(&v2), true).unwrap();
    let made = site.heads()[0];
    assert_ne!(made, v1, "commit 该照常说一声就落了（账面没问题）");
    assert!(
        !site.exists("b.txt"),
        "没补 `scan` 就被期待重建 —— 这条断言钉的是**当前行为**，不是愿望"
    );

    // 补一次 `scan` 之后再检出那一版：她回来了。这一句同时证明 remedy 不需要新命令。
    site.scan();
    site.checkout(&made, true).unwrap();
    assert!(site.exists("b.txt"), "scan 过之后 checkout 就该把她取回来");
    assert_eq!(site.read("b.txt"), "two");
}
