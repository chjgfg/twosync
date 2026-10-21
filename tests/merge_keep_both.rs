//! `merge`（§7.1 三条出路的命令面）。这批用例盯的是"**不许猜**"那一半：
//! 合并只在路径无交集时发生，认出多/认出零就拒绝，而"报模式"那一趟一个字节都不许多。
//!
//! 造两个 head 的形状与真实场景一致：**共同祖先 + 两个各自的孩子**
//! （`pull`/`push` union 之后就是这个样子）。这里用 `set_ref` 手工搭，不用假数据。

mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use storage_engines::KvEngine;
use twosync::cli::{m3, m4};
use twosync::cli::registry::Registry;
use twosync::cli::{Cmd, Cli};
use twosync::store::build::{self, MAIN};
use twosync::store::cas::Cas;
use twosync::store::keys::{self, Hash, ProjectId};
use twosync::store::{history, object, sessions, worktree};
use twosync::utils::{open_store, objects_dir};

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
        let db: Arc<dyn KvEngine> =
            Arc::from(open_store(tmp.path(), common::engine()).unwrap());
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

    fn write(&self, rel: &str, body: &str) {
        let p = self.work().join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    fn commit(&self, msg: &str) -> Hash {
        let work = self.work();
        worktree::scan(&*self.db, self.pid, &work, false, 100).unwrap();
        let store = Cas::new(&objects_dir(&self.root()), BLK).unwrap();
        build::build(
            &*self.db,
            self.pid,
            &store,
            &work,
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

    /// head 集合。**键序 = 哈希字节序**，不是插入序 ⇒ 断言两边都要先排序。
    fn heads(&self) -> Vec<Hash> {
        let mut h = build::ref_heads(&*self.db, self.pid, MAIN).unwrap();
        h.sort();
        h
    }

    fn rows(&self) -> u64 {
        self.db.health().total_rows
    }

    /// 树里有哪些**叶子路径**（递归，够断言"两边都还在"）。
    fn leaves(&self, tree: &Hash) -> Vec<String> {
        let mut out = Vec::new();
        self.walk(tree, "", &mut out);
        out.sort();
        out
    }

    fn walk(&self, tree: &Hash, prefix: &str, out: &mut Vec<String>) {
        for e in object::read_tree(&*self.db, self.pid, tree).unwrap() {
            let path = if prefix.is_empty() {
                e.name.clone()
            } else {
                format!("{prefix}/{}", e.name)
            };
            if e.kind == keys::Kind::Dir {
                self.walk(&e.hash, &path, out);
            } else {
                out.push(path);
            }
        }
    }

    /// 共同祖先 + 两个各自的孩子：`base` 之后一边只加 `name_l`、另一边只加 `name_r`。
    ///
    /// 关键那一步是**把工作树换回 `base` 的样子**再造另一边（删掉 `name_l`）：`commit` 吃的
    /// 是上一次 `scan` 的快照，不删的话右边的树里会连着带上左边的文件，两条边就不"无交集"了，
    /// 这条用例也就什么都测不到。
    fn fork(&self, base: &Hash, name_l: &str, name_r: &str) -> (Hash, Hash) {
        build::set_ref(&*self.db, self.pid, MAIN, &[*base]).unwrap();
        self.write(name_l, "only on the left");
        let l = self.commit(&format!("left adds {name_l}"));
        fs::remove_file(self.work().join(name_l.replace('/', std::path::MAIN_SEPARATOR_STR))).unwrap();
        build::set_ref(&*self.db, self.pid, MAIN, &[*base]).unwrap();
        self.write(name_r, "only on the right");
        let r = self.commit(&format!("right adds {name_r}"));
        build::set_ref(&*self.db, self.pid, MAIN, &[l, r]).unwrap();
        (l, r)
    }

    fn merge(&self, keep_both: bool, take_local: bool, take_remote: bool, apply: bool) -> twosync::error::Result<()> {
        m3::merge(&self.cli(), keep_both, take_local, take_remote, apply, true)
    }
}

/// 两个 head 的期望值：`ref_heads` 给的是哈希字节序，比较前先排一次。
fn pair(a: Hash, b: Hash) -> Vec<Hash> {
    let mut v = vec![a, b];
    v.sort();
    v
}

#[test]
fn a_plain_merge_only_reports_and_writes_nothing() {
    let site = Site::new();
    site.write("a.txt", "base");
    let base = site.commit("base");
    let (l, r) = site.fork(&base, "left.txt", "right.txt");
    assert_eq!(site.heads().len(), 2);
    let rows = site.rows();

    site.merge(false, false, false, false).unwrap();

    assert_eq!(site.rows(), rows, "不带旗标的 `merge` 是看一眼，不是动一手");
    assert_eq!(site.heads(), pair(l, r));
}

#[test]
fn keep_both_merges_disjoint_paths_into_one_two_parent_commit() {
    let site = Site::new();
    site.write("a.txt", "base");
    let base = site.commit("base");
    site.write("left.txt", "from the left side");
    build::set_ref(&*site.db, site.pid, MAIN, &[base]).unwrap();
    let l = site.commit("left adds left.txt");
    site.write("right.txt", "from the right side");
    build::set_ref(&*site.db, site.pid, MAIN, &[base]).unwrap();
    let r = site.commit("right adds right.txt");
    build::set_ref(&*site.db, site.pid, MAIN, &[l, r]).unwrap();

    // 报模式：一个字都不许多
    let rows = site.rows();
    site.merge(true, false, false, false).unwrap();
    assert_eq!(site.rows(), rows, "--keep-both 不带 --apply 不该写库");

    site.merge(true, false, false, true).unwrap();

    let heads = site.heads();
    assert_eq!(heads.len(), 1, "合并之后 head 必须收成那一个");
    let m = object::read_commit(&*site.db, site.pid, &heads[0]).unwrap();
    assert_eq!(m.parents, pair(l, r).into_iter().collect::<Vec<_>>(), "两亲都要在");
    assert!(m.stats.is_empty(), "合并 commit 的变更在两个 parent 各自那一格里");
    let mut leaves = site.leaves(&m.tree);
    leaves.sort();
    assert_eq!(
        leaves,
        vec!["a.txt".to_string(), "left.txt".to_string(), "right.txt".to_string()],
        "两边的新增都得活下来，一个都不许丢"
    );

    // 合并结果必须**可检出**：head 只剩一个，`one_commit` 那道闸就该开了
    let one = history::resolve_rev(&*site.db, site.pid, "HEAD").unwrap();
    assert_eq!(one, heads);
}

#[test]
fn keep_both_refuses_when_both_sides_touched_the_same_path_differently() {
    let site = Site::new();
    site.write("a.txt", "base");
    let base = site.commit("base");
    site.write("a.txt", "left says one thing");
    build::set_ref(&*site.db, site.pid, MAIN, &[base]).unwrap();
    let l = site.commit("left edits a.txt");
    site.write("a.txt", "right says another");
    build::set_ref(&*site.db, site.pid, MAIN, &[base]).unwrap();
    let r = site.commit("right edits a.txt");
    build::set_ref(&*site.db, site.pid, MAIN, &[l, r]).unwrap();
    let rows = site.rows();

    let e = site
        .merge(true, false, false, true)
        .expect_err("同路径两边各改一份 ⇒ §7.1 明令不许合");
    assert!(e.to_string().contains("--keep-both 被"), "{e}");
    assert_eq!(
        site.rows(),
        rows,
        "被拒的那一趟不许留下半个合并 commit（写了就永远留在库里）"
    );
    assert_eq!(site.heads(), pair(l, r), "被拒的那一趟不许动 head");
}

/// 两边都往同一层里加**同名不同内容**的文件 ⇒ 也是冲突。
/// 这条单独钉，因为它是最容易写错的一种：父目录 hash 不同不代表子项冲突。
#[test]
fn two_different_additions_in_the_same_directory_are_a_conflict_too() {
    let site = Site::new();
    site.write("d/keep.txt", "k");
    let base = site.commit("base");
    site.write("d/new.txt", "left version");
    build::set_ref(&*site.db, site.pid, MAIN, &[base]).unwrap();
    let l = site.commit("left adds d/new");
    site.write("d/new.txt", "right version");
    build::set_ref(&*site.db, site.pid, MAIN, &[base]).unwrap();
    let r = site.commit("right adds d/new");
    build::set_ref(&*site.db, site.pid, MAIN, &[l, r]).unwrap();

    let e = site
        .merge(true, false, false, true)
        .expect_err("同名不同内容的新增该算冲突，不许静默留一边");
    assert!(e.to_string().contains("1 条"), "冲突条数要说出来：{e}");
}

#[test]
fn take_side_refuses_when_the_origin_is_not_unique() {
    let site = Site::new();
    site.write("a.txt", "base");
    let base = site.commit("base");
    let (l, r) = site.fork(&base, "left.txt", "right.txt");
    // 会话账是空的 ⇒ "本地"这一边有两个候选（谁都不是收进来的）
    let rows = site.rows();

    let e = site
        .merge(false, true, false, true)
        .expect_err("认不出唯一的一边时必须拒绝");
    let msg = e.to_string();
    assert!(msg.contains("2 个候选") || msg.contains("认不出"), "{msg}");
    assert!(msg.contains("reset"), "拒绝要给明路：{msg}");
    assert_eq!(site.rows(), rows, "拒绝的那一趟什么都不该写");
    assert_eq!(site.heads(), pair(l, r));

    // `--take-remote` 同理：零个候选也是拒绝，不是"那就取另一个"
    let e = site
        .merge(false, false, true, true)
        .expect_err("没有一边是收进来的 ⇒ 不许猜");
    assert!(e.to_string().contains("认不出"), "{e}");
}

#[test]
fn take_remote_picks_the_head_this_site_accepted_and_lands_it() {
    let site = Site::new();
    site.write("a.txt", "base");
    let base = site.commit("base");
    let (l, r) = site.fork(&base, "left.txt", "right.txt");

    // 伪造一场"本端收进来 r"的会话账（`pull` 落地时写的就是这种行：dir 在收、accepted=r）
    sessions::record(
        &*site.db,
        site.pid,
        &sessions::SessionRecord {
            ts: 1,
            project: "p".to_string(),
            peer: "peer-x".to_string(),
            dir: sessions::Dir::Pull,
            blocks: 1,
            bytes: 3,
            skipped: 0,
            ms: 1,
            our_heads: vec![l, r],
            their_heads: vec![r],
            accepted: Some(r),
        },
    )
    .unwrap();

    site.merge(false, false, true, true).unwrap();
    assert_eq!(site.heads(), vec![r], "--take-remote 要把收进来的那一个留下");
    assert_eq!(fs::read_to_string(site.work().join("a.txt")).unwrap(), "base");

    // 反过来：本地那一边此时就是 l（`--take-local` 认的是"不是收进来的那个"）
    build::set_ref(&*site.db, site.pid, MAIN, &[l, r]).unwrap();
    site.merge(false, true, false, true).unwrap();
    assert_eq!(site.heads(), vec![l], "--take-local 要把本站自己写的那个留下");
}

/// 三条出路只能选一条：`--keep-both --take-local` 这种组合**必须报错**，
/// 因为"合并"与"丢掉另一边"是两个相反的裁决。
#[test]
fn the_three_outcomes_are_mutually_exclusive() {
    let site = Site::new();
    site.write("a.txt", "base");
    let base = site.commit("base");
    site.fork(&base, "left.txt", "right.txt");
    let e = site
        .merge(true, true, false, true)
        .expect_err("两个相反的裁决不许同时给");
    assert!(e.to_string().contains("三选一"), "{e}");

    // 只有一个 head 时也裁决不了（但**不该**是报错：那时根本没有另一边）
    let solo = Site::new();
    solo.write("a.txt", "x");
    solo.commit("only");
    solo.merge(false, false, false, false).unwrap();
    let e = solo.merge(false, true, false, true).expect_err("没有第二边时旗标该被拒");
    assert!(e.to_string().contains("没有"), "{e}");
}

/// `merge --take-*` 走的是 `reset` 那两步 ⇒ 代价提示（被丢的 head 会被 gc 收走）也得在。
#[test]
fn taking_a_side_goes_through_the_same_warning_as_reset() {
    let site = Site::new();
    site.write("a.txt", "base");
    let base = site.commit("base");
    let (l, r) = site.fork(&base, "left.txt", "right.txt");
    sessions::record(
        &*site.db,
        site.pid,
        &sessions::SessionRecord {
            ts: 1,
            project: "p".to_string(),
            peer: "peer-x".to_string(),
            dir: sessions::Dir::ServeIn,
            blocks: 0,
            bytes: 0,
            skipped: 0,
            ms: 1,
            our_heads: vec![l, r],
            their_heads: vec![r],
            accepted: Some(r),
        },
    )
    .unwrap();
    // 先给被丢的那一个挂上 tag ⇒ 证明这条路径认得"要保的东西"
    m4::tag(
        &site.cli(),
        &twosync::cli::cmd::TagCmd::Add {
            name: "keep-me".to_string(),
            at: Some(keys::hex(&l)),
        },
    )
    .unwrap();

    site.merge(false, false, true, true).unwrap();
    assert_eq!(site.heads(), vec![r]);
    // 被丢的 l 有 tag 挂着 ⇒ gc 一个都不能真删
    let store = Cas::new(&objects_dir(&site.root()), BLK).unwrap();
    let p = twosync::store::gc::sweep(&*site.db, &store, site.pid, false).unwrap();
    assert_eq!(p.commits_dropped, 0, "tag 挂着的 l 不许被收走：{p:?}");
}
