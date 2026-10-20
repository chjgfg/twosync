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
//! $TS --root /tmp/ts7 checkout --at <v1 的前缀> --to /tmp/ts7/out --apply  # 导出，工作树不动
//! $TS --root /tmp/ts7 checkout --at <v1 的前缀> --to /tmp/ts7/lab/o # 必须被拒（目标在工作树里）
//! ```

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use storage_engines::{EngineKind, KvEngine};
use twosync::error::Error;
use twosync::store::cas::Cas;
use twosync::store::checkout::{self, Opts};
use twosync::store::keys::{self, Hash, ProjectId};
use twosync::store::{build, object, restore, worktree};
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

    /// `--to` 的落点：**必须在工作树外**（`tmp/out`，而工作树是 `tmp/p`）。
    fn out(&self) -> PathBuf {
        self.tmp.path().join("out")
    }

    fn export(
        &self,
        target: &Hash,
        prefix: Option<&str>,
        dest: &Path,
        force: bool,
        apply: bool,
    ) -> twosync::error::Result<checkout::Export> {
        let store = Cas::new(&objects_dir(self.tmp.path()), BLK).unwrap();
        checkout::export(
            &*self.db,
            self.pid,
            &store,
            &checkout::ExportOpts {
                target,
                prefix,
                dest,
                work_root: &self.proj(),
                force,
                apply,
            },
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

// ── `checkout --to`：导出到另一个目录 ─────────────────────────────────
//
// 这批用例盯的是分工：`--to` **不碰工作树、不碰历史**，所以 §7.3 与 FR-10 在这里都没有
// 对象，取而代之的是一道这里才有的闸——目标不许落在工作树里。

/// 导出把版本写到别处，工作树与库**一个字节都不许多**。
#[test]
fn export_writes_elsewhere_and_leaves_worktree_and_history_alone() {
    let (site, v1, _) = two_versions();
    let out = site.out();
    let rows = site.db.health().total_rows;

    let dry = site.export(&v1, None, &out, false, false).unwrap();
    assert_eq!(
        (dry.files, dry.dirs),
        (2, 1),
        "v1 = a.txt + d/ + d/b.txt：{dry:?}"
    );
    assert!(!out.exists(), "只报模式连目录都不许建");

    let done = site.export(&v1, None, &out, false, true).unwrap();
    assert!(done.applied);
    assert_eq!(fs::read_to_string(out.join("a.txt")).unwrap(), "v1");
    assert_eq!(
        fs::read_to_string(out.join("d").join("b.txt")).unwrap(),
        "b"
    );
    // 磁盘：工作树还是 v2 那个样子。
    assert_eq!(site.read("a.txt"), "v2-longer");
    assert!(site.exists("c.txt") && !site.exists("d/b.txt"));
    // 库：导出一行都不该写（head 也没动）。
    assert_eq!(site.db.health().total_rows, rows, "导出不该进库一行");
    assert_eq!(
        build::ref_heads(&*site.db, site.pid, build::MAIN)
            .unwrap()
            .len(),
        1
    );
}

/// 目标在工作树里 ⇒ 拒。防的是"下一轮 `scan` 把导出的副本当成项目内容"，
/// 而它比字符串比较严：拼出来的 `..` 要按**规范路径**判，不然 `p/../out` 会被误拒、
/// `p/./x` 会被误放。
#[test]
fn export_refuses_a_destination_inside_the_worktree() {
    let (site, v1, _) = two_versions();
    let inside = site.abs("exported");
    let err = site
        .export(&v1, None, &inside, false, true)
        .expect_err("工作树里的目标必须被拒");
    assert!(
        matches!(err, Error::BadInput(_)),
        "该是「你说错目录了」，不是某个 IO 错误：{err}"
    );
    assert!(!inside.exists(), "拒了就该什么都没建");
    assert_eq!(site.read("a.txt"), "v2-longer");

    // 同一句 `..` 写到工作树**外面**就得放行——判据是规范路径，不是字符串里有没有 `p`。
    let up = site.proj().join("..").join("out2");
    site.export(&v1, None, &up, false, true)
        .expect("目标其实不在工作树里");
    assert_eq!(fs::read_to_string(up.join("a.txt")).unwrap(), "v1");
}

/// 目标目录里已有的东西默认**不覆盖**，而且这笔账要看得见；`--force` 才盖。
#[test]
fn export_leaves_existing_paths_alone_unless_forced() {
    let (site, v1, _) = two_versions();
    let out = site.out();
    site.export(&v1, None, &out, false, true).unwrap();
    fs::write(out.join("a.txt"), "这是别的项目放在这儿的东西").unwrap();

    let r = site.export(&v1, None, &out, false, true).unwrap();
    assert_eq!(
        (r.files, r.skipped),
        (0, 2),
        "两个文件都在 ⇒ 只跳不写：{r:?}"
    );
    assert_eq!(
        fs::read_to_string(out.join("a.txt")).unwrap(),
        "这是别的项目放在这儿的东西",
        "没给 --force 就是不许动"
    );

    let f = site.export(&v1, None, &out, true, true).unwrap();
    assert_eq!(
        (f.files, f.overwrite, f.skipped),
        (0, 2, 0),
        "--force 才覆盖，且算覆盖不算新增：{f:?}"
    );
    assert_eq!(fs::read_to_string(out.join("a.txt")).unwrap(), "v1");
}

/// `--prefix` 导出的是**那一棵子树的内容**，摊在 dest 下（跟 `restore --to` 同一条规矩：
/// dest 就是那一棵子树的根），而不是把子树名字再套一层。
#[test]
fn export_prefix_writes_that_subtree_into_the_dest_itself() {
    let (site, v1, _) = two_versions();
    let out = site.out();
    let r = site.export(&v1, Some("d"), &out, false, true).unwrap();
    assert_eq!((r.files, r.dirs), (1, 0), "{r:?}");
    assert_eq!(fs::read_to_string(out.join("b.txt")).unwrap(), "b");
    assert!(!out.join("d").exists(), "不该把子树名字再套一层");
    assert!(!out.join("a.txt").exists(), "前缀之外的一条都不许多写");
}

/// 前缀的两种错法与 `checkout` 同一个口径：目标树里没有 ⇒ `NoSuchPath`，
/// 指向的是文件 ⇒ `NotYet`（那活归 `restore`）。
#[test]
fn export_prefix_checks_the_target_tree_like_checkout_does() {
    let (site, v1, _) = two_versions();
    let out = site.out();
    assert!(
        matches!(
            site.export(&v1, Some("nope"), &out, false, true),
            Err(Error::NoSuchPath { .. })
        ),
        "打错路径要说清是哪个版本里没有什么"
    );
    assert!(
        matches!(
            site.export(&v1, Some("a.txt"), &out, false, true),
            Err(Error::NotYet { .. })
        ),
        "单文件是 restore 的活，这里不许糊过去"
    );
    assert!(!out.exists(), "两次都该在动手之前就被拒");
}

// ── §7.2：现在读不了的项，检出层怎么处理 ───────────────────────


/// §7.2 的两道闸是两个不同的动作：`checkout` **跳过**这一条、其余照常；`restore` 对她**报错**。
///
/// 为什么不统一：`checkout` 是批量落地，一条不让动就把整棵树卡住等于惩罚无关的路径；
/// `restore` 只动一条，静默跳过就变成"命令成功了但什么都没发生"。
#[test]
fn a_deferred_path_is_skipped_by_checkout_and_refused_by_restore() {
    let site = Site::new();
    site.write("a.txt", "one");
    site.write("keep.txt", "keep");
    let v1 = site.commit();
    site.write("a.txt", "two - the committed version");
    site.commit();

    // 盘上又变了一次，然后被别的进程占住 ⇒ 本轮读不到她，`W` 里留的还是 v2 那一版。
    site.write("a.txt", "three - unreadable until its owner lets go");
    let on_disk = fs::metadata(site.abs("a.txt")).unwrap().len();
    let guard = common::lock_out(&site.abs("a.txt"));
    site.scan();

    // ① **不是删除**（§8 第一杀手）：与 HEAD 比，plan 一条都不报——那行被续期了，没被丢掉
    let p = build::plan(&*site.db, site.pid).unwrap();
    assert_eq!(p.changes.delete, 0, "deferred 被报成了删除");
    assert_eq!(p.changes.add, 0);
    assert_eq!(p.changes.modify, 0, "读不到不等于内容变了");

    // ② `checkout` 到 v1：a.txt 本该被覆盖 ⇒ 跳过她
    let rep = site
        .checkout(
            &v1,
            None,
            Opts {
                apply: true,
                allow_mass_delete: true,
            },
        )
        .unwrap();
    assert_eq!(rep.deferred, 1, "闸没生效：{rep:?}");
    assert_eq!(rep.overwrite, 0, "覆盖了一份读不到的本地内容");
    assert!(!rep.is_noop(), "全被跳过时不该说成「工作树已经是这个样子」");
    assert_eq!(
        fs::metadata(site.abs("a.txt")).unwrap().len(),
        on_disk,
        "盘上那一版被盖掉了"
    );

    // ③ `restore` 同一条件：**报错**，不是跳过
    let store = Cas::new(&objects_dir(site.tmp.path()), BLK).unwrap();
    let e = restore::restore(
        &*site.db,
        site.pid,
        &store,
        &site.proj(),
        &v1,
        "a.txt",
        None,
        true,
    )
    .unwrap_err();
    assert!(matches!(e, Error::Deferred { .. }), "实际报的是：{e}");
    assert!(e.to_string().contains("读不了"), "错误里要能看出为什么：{e}");

    // ④ 同一份内容导到**工作树外**不受此限：那一刀盖的不是这份读不到的东西
    fs::create_dir_all(site.out()).unwrap();
    let got = restore::restore(
        &*site.db,
        site.pid,
        &store,
        &site.proj(),
        &v1,
        "a.txt",
        Some(&site.out().join("a.txt")),
        true,
    )
    .unwrap();
    assert_eq!(got.bytes, 3, "导出的应当是 v1 的 one");

    // ⑤ 解锁 + 提交之后闸就开了：本地那一版进了历史，checkout 照常覆盖
    drop(guard);
    let v3 = site.commit();
    assert_ne!(v3, v1, "台架前提：三轮内容各不相同");
    assert!(
        worktree::list_deferred(&*site.db, site.pid)
            .unwrap()
            .is_empty(),
        "读得通了名单还在，说明全量轮的活性戳没收她"
    );
    let rep = site
        .checkout(
            &v1,
            None,
            Opts {
                apply: true,
                allow_mass_delete: true,
            },
        )
        .unwrap();
    assert_eq!(rep.deferred, 0, "闸开了还拦着 = 第二个 bug");
    assert_eq!(rep.overwrite, 1);
    assert_eq!(site.read("a.txt"), "one");
}

/// 名单的**形状**：deferred 的是一个**目录**时，她整棵子树都不许检出。
///
/// 这条容易漏：扫描器对一个进不去的目录报的是目录本身，里面的项本轮根本没被看过；
/// 而检出侧如果只比叶子路径，就会往一个"我们没能证实"的目录里正常写文件。
#[test]
fn a_deferred_directory_blocks_its_whole_subtree() {
    let site = Site::new();
    site.write("d/a.txt", "one");
    let v1 = site.commit();
    site.write("d/a.txt", "two - a longer committed body");
    let v2 = site.commit();

    // 造一个"目录读不了"的状态：直接写一行 `R|p|d`（`chmod`/句柄锁在 Windows 上管不住目录，
    // 而这条用例要验的是**检出侧怎么用名单**，不是扫描器怎么产生名单——那半边在 worktree_scan 里）。
    let rec = worktree::DeferredRec {
        reason: "用例伪造：进不去的目录".to_string(),
        at_ns: 1,
    };
    let tx = site.db.begin();
    assert!(tx.set(
        &keys::deferred_key(site.pid, "d"),
        keys::wrap(keys::enc(&rec, "deferred 记录").unwrap())
    ));
    tx.commit();

    // v1 里 d/a.txt 是 "one" ⇒ 本来要覆盖，但她爹在名单上
    let rep = site
        .checkout(
            &v1,
            None,
            Opts {
                apply: true,
                allow_mass_delete: true,
            },
        )
        .unwrap();
    assert_eq!(rep.deferred, 1, "目录级名单没管住子树：{rep:?}");
    assert_eq!(rep.overwrite, 0);
    assert_eq!(fs::metadata(site.abs("d/a.txt")).unwrap().len(), 29);
    let _ = v2;
}
