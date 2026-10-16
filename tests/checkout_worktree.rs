//! `checkout`：整棵子树落地，以及它前面那四道闸。
//!
//! 重心全在"什么时候不许动"：只报模式、脏工作树、FR-10 熔断、§7.3 逐文件门。
//! 一条命令会批量删工作树文件，所以每种"不许"都要有自己的用例，少一种就是少一道闸。
//!
//! 手工验证（跑完 `rm -rf /tmp/ts7`）：
//!
//! ```text
//! cargo build --release && TS=./target/release/twosync.exe
//! rm -rf /tmp/ts7 && $TS --root /tmp/ts7 init
//! mkdir -p /tmp/ts7/lab/d && echo v1 > /tmp/ts7/lab/a.txt && echo b > /tmp/ts7/lab/d/b.txt
//! $TS --root /tmp/ts7 project add lab && $TS --root /tmp/ts7 scan && $TS --root /tmp/ts7 commit -m v1
//! echo v2 > /tmp/ts7/lab/a.txt && rm /tmp/ts7/lab/d/b.txt && echo c > /tmp/ts7/lab/c.txt
//! $TS --root /tmp/ts7 scan && $TS --root /tmp/ts7 commit -m v2
//! $TS --root /tmp/ts7 ls-files --at <v1 的前缀>          # 该列出 a.txt / d / d/b.txt
//! $TS --root /tmp/ts7 checkout --at <v1 的前缀>          # 只报：覆盖 1 / 建 1 / 删 1
//! $TS --root /tmp/ts7 checkout --at <v1 的前缀> --apply  # 真做
//! cat /tmp/ts7/lab/a.txt                                 # 必须是 v1
//! ```

use std::fs;
use std::path::PathBuf;

use storage_engines::{EngineKind, KvEngine};
use twosync::error::Error;
use twosync::store::cas::Cas;
use twosync::store::checkout::{self, Opts};
use twosync::store::keys::{self, Hash, ProjectId};
use twosync::store::{build, object, worktree};
use twosync::utils::{objects_dir, open_store};

const BLK: u64 = 4096;

struct Site {
    db: Box<dyn KvEngine>,
    pid: ProjectId,
    tmp: tempfile::TempDir,
}

impl Site {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let db = open_store(tmp.path(), EngineKind::LsmTree).unwrap();
        let pid = keys::project_id("p");
        Self { db, pid, tmp }
    }

    fn proj(&self) -> PathBuf {
        self.tmp.path().join("p")
    }

    fn abs(&self, rel: &str) -> PathBuf {
        self.proj()
            .join(rel.replace('/', std::path::MAIN_SEPARATOR_STR))
    }

    fn write(&self, rel: &str, body: &str) {
        let p = self.abs(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.abs(rel)).unwrap()
    }

    fn exists(&self, rel: &str) -> bool {
        self.abs(rel).exists()
    }

    fn scan(&self) {
        let proj = self.proj();
        worktree::scan(&*self.db, self.pid, &proj, false, 100).unwrap();
    }

    fn commit(&self) -> Hash {
        self.scan();
        let proj = self.proj();
        let store = Cas::new(&objects_dir(self.tmp.path()), BLK).unwrap();
        build::build(
            &*self.db,
            self.pid,
            &store,
            &proj,
            build::Meta {
                peer: "t",
                msg: "用例",
                ts: 1,
                allow_mass_delete: true, // 这些用例故意大批删，测的是别的闸
            },
        )
        .unwrap()
        .commit
        .expect("用例里每次都要有改动")
    }

    fn checkout(
        &self,
        target: &Hash,
        prefix: Option<&str>,
        opts: Opts,
    ) -> twosync::error::Result<checkout::Report> {
        let store = Cas::new(&objects_dir(self.tmp.path()), BLK).unwrap();
        checkout::checkout(
            &*self.db,
            self.pid,
            &store,
            &self.proj(),
            target,
            prefix,
            opts,
        )
    }
}

/// v1：`a.txt` + `d/b.txt`；v2：`a.txt` 改了、`d/b.txt` 删了、多了 `c.txt`。
/// 回到 v1 应当是：覆盖 1、建目录 1、建文件 1、删 1。
fn two_versions() -> (Site, Hash, Hash) {
    let site = Site::new();
    site.write("a.txt", "v1");
    site.write("d/b.txt", "b");
    let v1 = site.commit();
    site.write("a.txt", "v2-longer");
    fs::remove_file(site.abs("d/b.txt")).unwrap();
    site.write("c.txt", "c");
    let v2 = site.commit();
    (site, v1, v2)
}

#[test]
fn dry_run_reports_everything_and_touches_nothing() {
    let (site, v1, _) = two_versions();
    let rows = site.db.health().total_rows;

    let r = site
        .checkout(&v1, None, Opts::default())
        .expect("只报模式不该被任何闸拦下");
    assert_eq!(
        (r.overwrite, r.mkdir, r.create, r.delete, r.rmdir),
        (1, 0, 1, 1, 0),
        "报告本身错了，后面三道闸就都在保护错的东西：{r:?}"
    );
    // `d` 两侧都有、只是内容不同 ⇒ 它本身不算 mkdir，只有里面的 `b.txt` 算新增。
    assert!(!r.applied);
    assert_eq!(
        r.sample.iter().filter(|s| s.starts_with('-')).count(),
        1,
        "样本里该看得见那条删除"
    );
    // 磁盘：v2 的样子一点没变。
    assert_eq!(site.read("a.txt"), "v2-longer");
    assert!(site.exists("c.txt") && !site.exists("d/b.txt"));
    // 引擎：一行都没多（`plan` 只读，两趟遍历都不写）。
    assert_eq!(site.db.health().total_rows, rows);
}

#[test]
fn apply_rebuilds_the_older_tree_and_leaves_head_alone() {
    let (site, v1, v2) = two_versions();
    let r = site
        .checkout(
            &v1,
            None,
            Opts {
                apply: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(r.applied);
    assert_eq!(site.read("a.txt"), "v1");
    assert_eq!(site.read("d/b.txt"), "b");
    assert!(!site.exists("c.txt"), "v1 里没有 c.txt，它该被删掉");

    let head = build::ref_heads(&*site.db, site.pid, build::MAIN).unwrap();
    assert_eq!(head, vec![v2], "checkout 只改工作树，不许动 head");
}

#[test]
fn a_second_checkout_of_the_same_version_is_a_noop() {
    let (site, v1, _) = two_versions();
    let opts = Opts {
        apply: true,
        ..Default::default()
    };
    site.checkout(&v1, None, opts).unwrap();
    site.scan(); // 落地之后必须重扫，否则 `W` 说的还是旧话
    let again = site.checkout(&v1, None, Opts::default()).unwrap();
    assert!(again.is_noop(), "第二次应当无事可做：{again:?}");
}

/// 扫描器没见过的改动 ⇒ §7.3 逐文件门拦下。注意拦它的不是"工作树脏不脏"，而是
/// "**磁盘与 `W` 不符**"：前者拦一次就把命令废掉了（checkout 自己会让工作树变脏），
/// 后者才是真正在保护的那个东西。
#[test]
fn an_unscanned_local_change_is_not_overwritten() {
    let (site, v1, _) = two_versions();
    site.write("a.txt", "本地改了但没 scan");
    let err = site
        .checkout(
            &v1,
            None,
            Opts {
                apply: true,
                ..Default::default()
            },
        )
        .expect_err("扫描器没见过的内容不许被覆盖");
    match err {
        Error::NotInHistory { why, .. } => assert!(
            why.contains("比最后一次扫描更新"),
            "要说清是「没扫过」而不是含糊的「脏」，实际 {why}"
        ),
        other => panic!("应是 NotInHistory，实际 {other}"),
    }
    assert_eq!(site.read("a.txt"), "本地改了但没 scan");
}

/// 扫过了、但那版没 commit ⇒ 还是拦。与上一条是两种不同的丢法：一种"扫描器没见过"，
/// 一种"见过了但没进历史"。只报模式则两种都不该拦——它根本不动手。
#[test]
fn a_scanned_but_uncommitted_change_is_still_protected() {
    let (site, v1, _) = two_versions();
    site.write("a.txt", "扫过了，可还没 commit");
    site.scan();
    let err = site
        .checkout(
            &v1,
            None,
            Opts {
                apply: true,
                ..Default::default()
            },
        )
        .expect_err("没进历史的本地内容不许被覆盖");
    match err {
        Error::NotInHistory { why, .. } => assert!(
            why.contains("不在库里") || why.contains("没被扫描过"),
            "实际 {why}"
        ),
        other => panic!("应是 NotInHistory，实际 {other}"),
    }
    assert_eq!(site.read("a.txt"), "扫过了，可还没 commit");
    site.checkout(&v1, None, Opts::default())
        .expect("只报模式不动手，不需要过门");
}

#[test]
fn prefix_checkout_only_touches_that_subtree() {
    let (site, v1, _) = two_versions();
    let r = site
        .checkout(
            &v1,
            Some("d"),
            Opts {
                apply: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!((r.create, r.delete, r.overwrite), (1, 0, 0), "{r:?}");
    assert_eq!(site.read("d/b.txt"), "b", "子树里的文件回来了");
    assert_eq!(site.read("a.txt"), "v2-longer", "子树外的东西一律不许碰");
    assert!(site.exists("c.txt"), "同上");
}

#[test]
fn prefix_must_be_a_directory_and_must_exist_in_the_target() {
    let (site, v1, _) = two_versions();
    let err = site
        .checkout(&v1, Some("a.txt"), Opts::default())
        .expect_err("单文件该指回 restore");
    assert!(matches!(err, Error::NotYet { .. }), "实际 {err}");
    assert!(
        matches!(
            site.checkout(&v1, Some("nope/"), Opts::default()),
            Err(Error::NoSuchPath { .. })
        ),
        "打错路径不能报成磁盘坏了"
    );
}

/// 未跟踪文件挡住 `rmdir` 时：**原样留下、改账、不报错**。
/// 静默删掉用户没提交过的东西，是这条命令最坏的一种失败。
#[test]
fn an_untracked_file_keeps_its_directory_and_the_report_honest() {
    let site = Site::new();
    site.write("a.txt", "v1");
    site.write("d/b.txt", "b");
    site.commit();
    // 删掉 b.txt 再提一次 ⇒ HEAD 的树里 `d` 是个空目录，磁盘上也是空的。
    fs::remove_file(site.abs("d/b.txt")).unwrap();
    site.commit();
    // 扫描器**没见过**的文件：`W` 不知道它，所以它会一路活到 rmdir 那一步。
    site.write("d/keepme.txt", "未跟踪");

    // 手工造一个"没有 d"的目标版本。用真提交做不出来：那次提交会顺手把 `W` 也改成没有 d，
    // 于是删除判定根本不会触发。
    let (_, a) = build::LiveRows::new(&*site.db, site.pid)
        .map(|r| r.unwrap())
        .find(|(p, _)| p == "a.txt")
        .expect("a.txt 该在快照里");
    let tree = object::put_tree(
        &*site.db,
        site.pid,
        &[object::TreeEntry {
            name: "a.txt".into(),
            kind: keys::Kind::File,
            hash: a.blob_hash,
            size: a.size,
            mode: a.mode,
        }],
    )
    .unwrap();
    let target = object::put_commit(
        &*site.db,
        site.pid,
        &object::Commit {
            parents: Vec::new(),
            tree,
            peer: "t".into(),
            ts: 2,
            msg: "没有 d 的目标版本".into(),
            stats: object::Stats {
                add: 0,
                modify: 0,
                delete: 1,
                rename: 0,
                bytes: 0,
            },
        },
    )
    .unwrap();

    let r = site
        .checkout(
            &target,
            None,
            Opts {
                apply: true,
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("这一趟没有任何东西该被拦：{e}"));
    assert_eq!(r.rmdir, 0, "删不掉的目录不许记成删了：{r:?}");
    assert_eq!(r.skipped, 1, "要有一笔账交代它被留下：{r:?}");
    assert_eq!(site.read("d/keepme.txt"), "未跟踪");
    assert!(site.abs("d").is_dir());
}
