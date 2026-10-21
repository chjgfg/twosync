//! `stash`（M4 第二层最后一条）。她存在的理由只有一句：**"落库但别成为 head"**。
//! 库层为此新开的出口是 `build::build_detached`，命令层的后果是三条，三条都要有用例：
//! 1. 工作树被清回 head，那份改动**没丢**，她挂在 `refs/stash` 上（于是同时被 `gc` 保下来 §9.3）；
//! 2. `head` 一字不动 ⇒ 不会被下一次 `push` 当历史送出去（同步只读 `main`）；
//! 3. **只有一格**：第二条 push 必须拒绝，而不是挤掉第一条（被挤掉的那条只剩这一个名字，
//!    名字一没她就不可达，`gc` 连块一起收走 = 丢用户的改动）。
//!
//! 另外钉两条口径：只报那一趟一个字都不写（FR-7），以及"吃的是最近一次 `scan` 的快照"（不替你 scan）。

mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use storage_engines::KvEngine;
use twosync::cli::cmd::StashCmd;
use twosync::cli::m4;
use twosync::cli::registry::Registry;
use twosync::cli::{Cmd, Cli};
use twosync::store::build::{self, MAIN, STASH};
use twosync::store::cas::Cas;
use twosync::store::keys::{self, Hash, ProjectId};
use twosync::store::{gc, object, worktree};
use twosync::utils::{dir_size, objects_dir, open_store};

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

    fn write(&self, rel: &str, body: &str) {
        let p = self.work().join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.work().join(rel)).unwrap()
    }

    fn exists(&self, rel: &str) -> bool {
        self.work().join(rel).exists()
    }

    fn scan(&self) {
        let work = self.work();
        worktree::scan(&*self.db, self.pid, &work, false, 100).unwrap();
    }

    /// `scan` + `commit`（走的是**会移 head** 的那一支，用来造历史）。
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

    fn stash(&self, cmd: StashCmd) -> twosync::error::Result<()> {
        m4::stash(&self.cli(), &cmd)
    }

    fn heads(&self, r: &str) -> Vec<Hash> {
        build::ref_heads(&*self.db, self.pid, r).unwrap()
    }

    fn rows(&self) -> u64 {
        self.db.health().total_rows
    }

    fn sweep(&self, dry: bool) -> gc::Plan {
        let store = Cas::new(&objects_dir(&self.root()), BLK).unwrap();
        gc::sweep(&*self.db, &store, self.pid, dry).unwrap()
    }

    fn blob_row(&self, content: &[u8]) -> bool {
        let hash = *blake3::hash(content).as_bytes();
        let tx = self.db.begin();
        let got = tx.get(&keys::blob_key(self.pid, &hash)).is_some();
        tx.commit();
        got
    }
}

fn push(apply: bool) -> StashCmd {
    StashCmd::Push {
        msg: None,
        apply,
        allow_mass_delete: false,
    }
}

fn back(pop: bool, apply: bool) -> StashCmd {
    if pop {
        StashCmd::Pop {
            apply,
            allow_mass_delete: false,
        }
    } else {
        StashCmd::Apply {
            apply,
            allow_mass_delete: false,
        }
    }
}

/// 脏工作树 + 一次 `scan`，返回那条未提交改动（改 a.txt，另加 b.txt）。
fn dirty(site: &Site) {
    site.write("a.txt", "two");
    site.write("b.txt", "bee");
    site.scan();
}

#[test]
fn stashing_moves_the_worktree_back_to_head_and_keeps_the_work_under_a_name() {
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1");
    dirty(&site);

    site.stash(push(true)).unwrap();

    // 工作树清回了 head
    assert_eq!(site.read("a.txt"), "one", "push --apply 要把工作树清回 head");
    assert!(!site.exists("b.txt"), "新增那条也该从工作树上消失");
    // head 一字没动（这正是 `commit` 办不到的事）
    assert_eq!(site.heads(MAIN), vec![v1], "stash 不许移 head");
    // 那份改动挂在 refs/stash 上，亲代就是当时的 head
    let stashed = site.heads(STASH);
    assert_eq!(stashed.len(), 1, "stash 要落在 `refs/stash` 上：{stashed:?}");
    let c = object::read_commit(&*site.db, site.pid, &stashed[0]).unwrap();
    assert_eq!(c.parents, vec![v1], "她的亲代就是 push 那一刻的 head");
    assert!(c.stats.add == 1 && c.stats.modify == 1, "{:?}", c.stats);
    // 内容真的进了库：b.txt 的字节在 CAS 里取得回来
    assert!(site.blob_row(b"bee"), "stash 的字节必须已经落库");
    // 她不冒充 tag 也不冒充分支
    let all = build::all_named_refs(&*site.db, site.pid).unwrap();
    assert!(all.contains_key(STASH), "{all:?}");
    assert!(!all.contains_key("refs"), "她就是一个内部名字，不该多出别的形状");
}

/// **只有一格**：第二条 push 必须拒绝，并且第一条一字不动。
/// 覆盖式的 `git stash` 便利换的是"被挤掉的那条只剩一个名字挂着 ⇒ 不可达 ⇒ gc 收走"，
/// 那在这个工具里就是丢用户的改动。
#[test]
fn pushing_a_second_stash_is_refused_and_leaves_the_first_alone() {
    let site = Site::new();
    site.write("a.txt", "one");
    site.commit("v1");
    dirty(&site);
    site.stash(push(true)).unwrap();
    let first = site.heads(STASH)[0];

    // 工作树现在是干净的（刚被清回 head），所以第二次 push 先撞"没有可存的改动"；
    // 要真造出第二条，得再弄脏一次。
    site.write("a.txt", "three");
    site.scan();
    let e = site
        .stash(push(true))
        .expect_err("已经有 stash 时不许覆盖");
    assert!(e.to_string().contains("已经有一条 stash"), "{e}");
    assert_eq!(
        site.heads(STASH),
        vec![first],
        "被拒那一趟第一条 stash 不许被动过"
    );
    // 工作树也不许被动过：那条 `a.txt = three` 还在盘上
    assert_eq!(site.read("a.txt"), "three", "拒绝 = 什么都没做，包括没清工作树");
}

/// `apply` 取回但**留着**名字（可以反复 apply）；`pop` 取回之后才删名字。
/// 顺序在这里是判据本身：**先落地成功、后删名字**——反过来若落地被闸挡下，名字已经没了。
#[test]
fn apply_keeps_the_stash_and_pop_removes_it_only_after_landing() {
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1");
    dirty(&site);
    site.stash(push(true)).unwrap();
    assert!(!site.exists("b.txt"));

    site.stash(back(false, true)).unwrap();
    assert_eq!(site.read("a.txt"), "two", "apply 要把那份改动落回工作树");
    assert_eq!(site.read("b.txt"), "bee");
    assert_eq!(site.heads(STASH).len(), 1, "apply 不动名字");
    assert_eq!(site.heads(MAIN), vec![v1], "apply 也不动 head");

    // 反复 apply 是幂等的（同一棵树再落一次，不该报错也不该改历史）
    site.stash(back(false, true)).unwrap();
    assert_eq!(site.read("b.txt"), "bee");

    site.stash(back(true, true)).unwrap();
    assert!(site.heads(STASH).is_empty(), "pop 之后名字要没了");
    assert_eq!(site.read("a.txt"), "two", "pop 的落地结果与 apply 相同");
    assert_eq!(site.heads(MAIN), vec![v1], "pop 也不碰 head");
}

/// **只报那一趟一个字都不写**：`push` 连 commit 都不建，`apply`/`pop` 既不碰工作树也不删名字。
#[test]
fn every_stash_subcommand_dry_run_writes_nothing() {
    let site = Site::new();
    site.write("a.txt", "one");
    site.commit("v1");
    dirty(&site);

    let rows = site.rows();
    site.stash(push(false)).unwrap();
    assert_eq!(site.rows(), rows, "push 的只报那一趟连 commit 都不该建");
    assert!(site.heads(STASH).is_empty());
    assert_eq!(site.read("a.txt"), "two", "只报不许碰工作树");

    site.stash(push(true)).unwrap();
    let rows2 = site.rows();
    site.stash(back(true, false)).unwrap();
    assert_eq!(site.rows(), rows2, "pop 的只报那一趟不该删名字");
    assert_eq!(site.heads(STASH).len(), 1);
    assert_eq!(site.read("a.txt"), "one", "只报不许把工作树改回去");
}

/// stash 与 `gc` 的三方关系（§9.3）：**挂着就收不到，丢掉名字就收得到**。
/// 这一条是"挂在名字上"这件事的全部意义 —— 否则"没提交的改动"在库里就是一笔不可达的历史。
#[test]
fn a_stash_holds_her_history_out_of_gc_until_it_is_dropped() {
    let site = Site::new();
    site.write("a.txt", "one");
    site.commit("v1");
    dirty(&site);
    site.stash(push(true)).unwrap();

    // head 只有 v1，但 stash 挂着那一笔 ⇒ 保留集是两笔，一条都不该丢
    let p1 = site.sweep(true);
    assert_eq!(p1.commits_kept, 2, "v1 + stash：{p1:?}");
    assert_eq!(p1.commits_dropped, 0, "{p1:?}");
    assert!(site.blob_row(b"bee"), "她扛着的那份改动不许被 gc 收走");

    site.stash(StashCmd::Drop).unwrap();
    // 这一趟是**真收**（dry=false）：待删表只有落改动那一趟才会写（`dry` 只算账不副作用，§9.3），
    // 所以下面 `unlink_pending` 才有东西可删。第一版用例在这里用了 dry，于是 unlink 永远拿到 0 个。
    let p2 = site.sweep(false);
    assert_eq!(p2.commits_kept, 1, "只剩 head：{p2:?}");
    assert_eq!(p2.commits_dropped, 1, "stash 那一笔现在不可达了：{p2:?}");
    assert!(p2.blocks_pending >= 1, "归零的块要落 pending 表：{p2:?}");
    assert!(!site.exists("b.txt"), "台架前提：工作树是干净的，删块不会牵动盘上的东西");
    assert!(!site.blob_row(b"bee"), "真收那一趟就该把她的清单行抹掉");
    let files_before = dir_size(&objects_dir(&site.root())).files;
    let store = Cas::new(&objects_dir(&site.root()), BLK).unwrap();
    let unlinked = gc::unlink_pending(&*site.db, &store, site.pid, usize::MAX, false)
        .unwrap()
        .files;
    assert!(unlinked >= 1, "pending 要落地，实删 {unlinked} 个");
    assert!(
        dir_size(&objects_dir(&site.root())).files < files_before,
        "块文件数必须真的少了"
    );
}

/// 口径与 `commit` 一致：**吃最近一次 `scan` 的快照**，不替你 scan。
/// 盘上改了东西而没 scan ⇒ 她"看不见"那份改动，于是报告"没有可存的"，一个字都不写。
#[test]
fn stash_eats_the_last_scan_and_does_not_scan_for_you() {
    let site = Site::new();
    site.write("a.txt", "one");
    site.commit("v1");
    // 只改盘，**不 scan**
    site.write("a.txt", "two");

    let rows = site.rows();
    site.stash(push(true)).unwrap();
    assert_eq!(site.rows(), rows, "没 scan 就没有可存的：不该写任何东西");
    assert!(site.heads(STASH).is_empty());
    assert_eq!(site.read("a.txt"), "two", "更不该动工作树");

    // 补一次 scan，同一份改动就存下来了
    site.scan();
    site.stash(push(true)).unwrap();
    assert_eq!(site.heads(STASH).len(), 1, "scan 之后就认这份改动");
    assert_eq!(site.read("a.txt"), "one");
}

/// 干净的工作树（相对 head 没有改动）不该产生一条空 stash —— 与"不产生空提交"同一条规矩：
/// 碎 commit 会把引擎版本链白白撑大（§9.6.2）。
#[test]
fn a_clean_worktree_stashes_nothing() {
    let site = Site::new();
    site.write("a.txt", "one");
    site.commit("v1");
    let rows = site.rows();

    site.stash(push(true)).unwrap();
    assert_eq!(site.rows(), rows, "干净工作树不该写任何东西");
    assert!(site.heads(STASH).is_empty());
    assert!(site.heads(MAIN).len() == 1);
}
