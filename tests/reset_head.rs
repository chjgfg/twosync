//! `reset`（M4 第二层第二条）。它只多 `checkout` 一件事：**动 head 集合**——而那正是
//! §7.1"人选一边"缺的那块糖。代价也必须测：被丢的 head 没有别的 ref 挂着就成了 GC 的垃圾。
//!
//! 四条用例各钉一件事：
//! 1. 挪得对、落得对（工作树跟着变成那一版）；
//! 2. 不带 `--apply` 时**一行都不许多**（FR-7 那条规矩在这里也要成立）；
//! 3. `HEAD` 指向两个 commit 时必须报错，不许挑一个；
//! 4. **被丢的那个 commit 真的会变成垃圾**，而 `tag` 挂着它就不会——这条同时钉住
//!    §9.3 的"不可达 = 可删"与 `reset` 那句危险提示不是客套话。

mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use storage_engines::KvEngine;
use twosync::cli::cmd::TagCmd;
use twosync::cli::m4;
use twosync::cli::registry::Registry;
use twosync::cli::{Cmd, Cli};
use twosync::store::build::{self, MAIN};
use twosync::store::cas::Cas;
use twosync::store::keys::{self, Hash, ProjectId};
use twosync::store::gc;
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

    /// 一个走命令层的 `Cli`，**预填同一个句柄**（否则 `cli.store()` 开第二个句柄会撞库锁，
    /// 见 §17 补记三十八③）。
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
        twosync::store::worktree::scan(&*self.db, self.pid, &work, false, 100).unwrap();
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

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.work().join(rel.replace('/', std::path::MAIN_SEPARATOR_STR)))
            .unwrap()
    }

    fn heads(&self) -> Vec<Hash> {
        build::ref_heads(&*self.db, self.pid, MAIN).unwrap()
    }

    fn reset(&self, rev: &str, apply: bool) -> twosync::error::Result<()> {
        m4::reset(&self.cli(), rev, apply, true)
    }

    fn sweep(&self) -> gc::Plan {
        let store = Cas::new(&objects_dir(&self.root()), BLK).unwrap();
        gc::sweep(&*self.db, &store, self.pid, false).unwrap()
    }
}




#[test]
fn a_reset_moves_the_head_and_lands_the_worktree() {
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1");
    site.write("a.txt", "two");
    let v2 = site.commit("v2");
    assert_eq!(site.heads(), vec![v2]);

    site.reset(&keys::hex(&v1), true).unwrap();

    assert_eq!(site.heads(), vec![v1], "head 要挪过去，不然 `checkout` 之后还得再挑一次");
    assert_eq!(site.read("a.txt"), "one", "工作树要落到那一版");
    // 挪完之后 `commit` 的亲代是新 head，不是原来那个（这条是"head 是唯一真相"的下游证明）
    site.write("a.txt", "three");
    let v3 = site.commit("v3");
    let c3 = twosync::store::object::read_commit(&*site.db, site.pid, &v3).unwrap();
    assert_eq!(c3.parents, vec![v1], "reset 之后的提交必须挂在选定的那一支上");
}

/// FR-7 的规矩在 `reset` 上也要成立：不带 `--apply` 的那一趟**只许读**。
/// 这里连库里的行数都不许多——"只报"如果偷偷写了什么，下一次同类命令的基线就不对了。
#[test]
fn a_reset_without_apply_changes_not_a_single_row() {
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1");
    site.write("a.txt", "two");
    let v2 = site.commit("v2");
    let rows = site.db.health().total_rows;
    let heads_before = site.heads();

    site.reset(&keys::hex(&v1), false).unwrap();

    assert_eq!(site.db.health().total_rows, rows, "只报的一趟不该写库");
    assert_eq!(site.heads(), heads_before, "head 不许动");
    assert_eq!(site.read("a.txt"), "two", "工作树不许动");
    let _ = v2;
    let _ = v1;
}

/// 多 head 时 `reset HEAD` = 有两个解 ⇒ **必须报错**。这条测的是"挪 head 的命令自己不许
/// 在挪哪个上猜"（§7：union 之后不许猜）。
#[test]
fn resetting_onto_an_ambiguous_rev_is_refused() {
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1");
    site.write("a.txt", "two");
    let v2 = site.commit("v2");
    build::set_ref(&*site.db, site.pid, MAIN, &[v1, v2]).unwrap();

    let e = site
        .reset("HEAD", true)
        .expect_err("两个 head 时不许替人选一个");
    assert!(
        e.to_string().contains("指向 2 个 commit"),
        "要说清有几个解：{e}"
    );
    assert_eq!(site.heads(), vec![v1, v2], "报错的那一趟不许已经动过头");

    // 指定一个具体 commit 就能挪（这就是 §7.1 的"人选一边"）
    site.reset(&keys::hex(&v1), true).unwrap();
    assert_eq!(site.heads(), vec![v1]);
}

/// **这条才是 `reset` 的代价**。形状是：`main` 在 v2 上，人挪回 v1 ⇒ v2 不再被任何 ref 担保
/// （v1 不是它的孩子，方向反了），于是 §9.3 的"不可达 = 可删"立刻生效。
/// 同一个动作先挂上 tag，v2 就还在——这正是 `tag` 那条 pin 的用武之地。
#[test]
fn an_abandoned_head_becomes_gc_garbage_unless_a_tag_holds_it() {
    // ① 不挂 tag：挪回 v1，v2 变成垃圾
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1");
    site.write("a.txt", "two");
    let v2 = site.commit("v2");
    assert_eq!(site.heads(), vec![v2], "台架前提：main 本来在 v2 上");
    site.reset(&keys::hex(&v1), true).unwrap();
    assert!(
        !site.heads().contains(&v2),
        "reset 之后 v2 不再被任何 head 担保（还有没有别的 ref 是另一件事）"
    );
    let p = site.sweep();
    assert_eq!(
        (p.commits_kept, p.commits_dropped),
        (1, 1),
        "只剩 v1 的闭包 ⇒ v2 该进垃圾集：{p:?}"
    );
    assert!(
        p.blocks_pending >= 1,
        "v2 独占的那些块该落 pending（不许当场删）：{p:?}"
    );
    assert!(
        build::read_blob(&*site.db, site.pid, &keys::hash_of(b"two")).is_err(),
        "v2 被丢之后，它独占的那份清单行也该消失"
    );

    // ② 先挂 tag：同一个动作，v2 活着
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1");
    site.write("a.txt", "two");
    let v2 = site.commit("v2");
    m4::tag(
        &site.cli(),
        &TagCmd::Add {
            name: "before-reset".to_string(),
            at: Some(keys::hex(&v2)),
        },
    )
    .unwrap();
    site.reset(&keys::hex(&v1), true).unwrap();
    let p = site.sweep();
    assert_eq!(
        (p.commits_kept, p.commits_dropped),
        (2, 0),
        "tag 挂着 v2 ⇒ 保留集从**全部 ref** 出发，一个都不许丢：{p:?}"
    );
    assert!(
        build::read_blob(&*site.db, site.pid, &keys::hash_of(b"two")).is_ok(),
        "pin 住的 v2 那份清单行必须还在"
    );
    assert_eq!(build::ref_heads(&*site.db, site.pid, "before-reset").unwrap(), vec![v2]);

    // 而 head 那一条仍然是干净的：reset 之后 `m0::status` 那套逻辑看到的只有一个 head
    assert_eq!(site.heads(), vec![v1]);
}
