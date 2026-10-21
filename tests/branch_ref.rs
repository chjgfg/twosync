//! `branch`（M4 第二层的第三条）。它买到的东西只有两样，两条各钉一组用例：
//! ① 一段历史被 ref 担保 ⇒ `gc` 收不到（§9.3，与 tag 同一机制）；
//! ② 名字可以直接当 revspec ⇒ `reset <分支>` / `checkout --at <分支>` 不用敲哈希。
//!
//! 另有三条"不许"：不许与 tag 同名（共用一格，同名时解析先命中 tag、分支被藏住）；
//! 不许自己往前走（`commit` 只移 `main`）；不许在歧义面前挑一个（`--to HEAD` 撞上多 head 就拒）。
//! 还有两条判据住在库层的算账：`unheld_by_other_refs` 要**减掉其余全部 ref**，只比 `main`
//! 会把两个分支共用的祖先报成"要变垃圾" —— 那是吓人不是报账。

mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use storage_engines::KvEngine;
use twosync::cli::cmd::{BranchCmd, TagCmd};
use twosync::cli::m4;
use twosync::cli::registry::Registry;
use twosync::cli::{Cmd, Cli};
use twosync::store::build::{self, MAIN};
use twosync::store::cas::Cas;
use twosync::store::keys::{self, Hash, ProjectId};
use twosync::store::{gc, history, object, worktree};
use twosync::utils::{dir_size, objects_dir, open_store};

const BLK: u64 = 4096;

struct Site {
    tmp: tempfile::TempDir,
    /// `Arc` 而不是 `Box`：`Cli` 也拿**同一个句柄**（`cli()` 里预填 `store_slot`），
    /// 否则库级文件锁会把第二次打开顶成 `WouldBlock`（§17 补记三十五②）。
    db: Arc<dyn KvEngine>,
    pid: ProjectId,
}

impl Site {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".twosync")).unwrap();
        common::pin_engine_in_config(tmp.path());
        let db: Arc<dyn KvEngine> = Arc::from(open_store(tmp.path(), common::engine()).unwrap());
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

    /// `scan` + `commit`，返回那一个新 commit。
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

    /// 命令层入口。**必须从这儿进**：绕过 `m4::branch` 去直接 `set_ref` 就等于测了另一份实现。
    fn branch(&self, cmd: &BranchCmd) -> twosync::error::Result<()> {
        m4::branch(&self.cli(), cmd)
    }

    fn tag(&self, cmd: &TagCmd) -> twosync::error::Result<()> {
        m4::tag(&self.cli(), cmd)
    }

    fn heads(&self, r: &str) -> Vec<Hash> {
        build::ref_heads(&*self.db, self.pid, r).unwrap()
    }

    fn blob_row(&self, content: &[u8]) -> bool {
        let hash = *blake3::hash(content).as_bytes();
        let tx = self.db.begin();
        let got = tx.get(&keys::blob_key(self.pid, &hash)).is_some();
        tx.commit();
        got
    }

    fn sweep(&self, dry: bool) -> gc::Plan {
        let store = Cas::new(&objects_dir(&self.root()), BLK).unwrap();
        gc::sweep(&*self.db, &store, self.pid, dry).unwrap()
    }

    fn unlink_all(&self) -> usize {
        let store = Cas::new(&objects_dir(&self.root()), BLK).unwrap();
        gc::unlink_pending(&*self.db, &store, self.pid, usize::MAX, false)
            .unwrap()
            .files
    }
}

fn add(name: &str, at: Option<Hash>) -> BranchCmd {
    BranchCmd::Add {
        name: name.to_string(),
        at: at.map(|h| keys::hex(&h)),
    }
}

/// ①名字存在哪一格，②`tag list` 与 `branch list` 会不会互相把对方列进自己。
///
/// 后者是这批命令里最容易写错的一处：两者共用 `H|p|名|0x00|hash`，只靠 `refs/heads/` 前缀分家。
/// 分不家就会出现"打了个 tag 结果 `branch list` 里多出一条"。
#[test]
fn a_branch_lives_under_its_own_prefix_and_a_tag_cannot_take_the_same_name() {
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1");

    site.branch(&add("dev", None)).unwrap();
    assert_eq!(
        site.heads(&build::branch_ref("dev")),
        vec![v1],
        "分支要存在 `refs/heads/` 那一格"
    );
    assert!(
        site.heads("dev").is_empty(),
        "字面名那一格（tag 的地盘）不该被分支写进去"
    );

    // 反向也拒：先有分支，再打同名 tag ⇒ 若允许，解析先命中 tag、分支从此被藏住
    let e = site
        .tag(&TagCmd::Add {
            name: "dev".to_string(),
            at: None,
        })
        .expect_err("同名不许跨过分支");
    assert!(e.to_string().contains("分支"), "报错要说出撞的是谁：{e}");

    // 先有 tag，再建同名分支 ⇒ 同一条规矩
    site.tag(&TagCmd::Add {
        name: "keep".to_string(),
        at: None,
    })
    .unwrap();
    let e = site
        .branch(&add("keep", None))
        .expect_err("同名不许跨过 tag");
    assert!(e.to_string().contains("tag"), "报错要说出撞的是谁：{e}");
}

/// 名字要能当 revspec 用，否则分支只是个标签：`reset <分支>` 才是"跳到那一支"的那一步。
/// 全写法 `refs/heads/dev` 也要认（它是库里的真名）。
#[test]
fn a_branch_name_works_as_a_revspec() {
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1");
    site.write("a.txt", "two");
    let v2 = site.commit("v2");
    site.branch(&add("old", Some(v1))).unwrap();

    for rev in ["old", "refs/heads/old", "old~0"] {
        assert_eq!(
            history::resolve_rev(&*site.db, site.pid, rev).unwrap(),
            vec![v1],
            "`{rev}` 该解析到分支的 tip"
        );
    }

    // 命令层走一遍（dry-run）：证明"分支名"这条路在 `reset` 那一侧真能跑到底，而不只是库里的查表。
    m4::reset(&site.cli(), "old", false, false).unwrap();
    assert_eq!(site.heads(MAIN), vec![v2], "只报模式不许动 head");

    m4::reset(&site.cli(), "old", true, true).unwrap();
    assert_eq!(site.heads(MAIN), vec![v1], "带 --apply 就该落到分支那一版");
    // 工作树跟着落地（`reset` 的老规矩：先检出、最后挪 ref）
    assert_eq!(fs::read_to_string(site.work().join("a.txt")).unwrap(), "one");
}

/// 分支**不会自己往前走** —— 这句在 `branch add` 的输出里当着人说过，所以必须有用例钉住：
/// 没有它，"这条分支还跟不跟得上 main"就只能靠人记住 `commit` 只移 `main` 这件事。
#[test]
fn a_branch_does_not_walk_forward_by_itself() {
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1");
    site.branch(&add("dev", None)).unwrap();

    site.write("a.txt", "two");
    let v2 = site.commit("v2");
    assert_eq!(site.heads(MAIN), vec![v2], "commit 移的是 main");
    assert_eq!(
        site.heads(&build::branch_ref("dev")),
        vec![v1],
        "分支必须还钉在 v1"
    );

    // 要跟上只能点名 `move`
    site.branch(&BranchCmd::Move {
        name: "dev".to_string(),
        to: "HEAD".to_string(),
    })
    .unwrap();
    assert_eq!(site.heads(&build::branch_ref("dev")), vec![v2]);
}

/// 这条是 `branch` 的**存在理由之一**：与 tag 同一机制（§9.3 的"显式 `pinned`"），
/// 所以必须单独证一次"是分支保住的，不是 head 保住的" —— 台架与 `tag_ref.rs` 那条同形：
/// 让 `main` 挪到一个与 v1 **没有亲代关系**的新根，否则 v1 还在 head 闭包里，测不出东西。
#[test]
fn a_branch_pins_history_the_way_a_tag_does() {
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1");
    site.branch(&add("keep", Some(v1))).unwrap();

    site.write("a.txt", "two");
    site.commit("v2"); // v2 是 v1 的孩子

    build::set_ref(&*site.db, site.pid, MAIN, &[]).unwrap();
    site.write("a.txt", "three");
    let v3 = site.commit("v3"); // 无亲代的新根

    let p1 = site.sweep(true);
    assert_eq!(
        p1.commits_kept, 2,
        "保留集 = head 的 v3 + 分支的 v1。读数 {p1:?}"
    );
    assert_eq!(p1.commits_dropped, 1, "只该丢 v2：{p1:?}");
    assert!(
        site.blob_row(b"one") && site.blob_row(b"two"),
        "只报那一趟（dry）一个字都不许删"
    );

    // 真收一趟：v2 走掉，v1 因为分支还在而活着 —— **这一条才是分支的作用**
    let p1b = site.sweep(false);
    assert_eq!(p1b.commits_kept, 2, "第二次读数该与 dry 那趟一字不差：{p1b:?}");
    assert!(
        !site.blob_row(b"two"),
        "v2 没有 ref 挂着 ⇒ 它的 blob 行要跟着消失"
    );
    assert!(
        site.blob_row(b"one"),
        "v1 被分支挂着 ⇒ 它的 blob 行不许消失（这就是 §9.3 的 `pinned`）"
    );
    let files_before = dir_size(&objects_dir(&site.root())).files;
    assert!(files_before > 0, "台架前提：块仓里有东西");
    assert!(
        site.unlink_all() >= 1,
        "归零的块要落 pending 表并在这一步落地"
    );
    assert!(
        dir_size(&objects_dir(&site.root())).files < files_before,
        "块文件数必须真的少了"
    );

    // 松开分支 ⇒ 同一套回收把 v1 也收走
    site.branch(&BranchCmd::Rm {
        name: "keep".to_string(),
    })
    .unwrap();
    let p2 = site.sweep(false);
    assert_eq!(
        p2.commits_dropped, 1,
        "只剩 v1 可丢（v2 上一趟已经收过了）：{p2:?}"
    );
    assert!(p2.blocks_pending >= 1, "归零的块要落 pending 表：{p2:?}");
    assert!(site.unlink_all() >= 1, "pending 要落地");
    assert!(
        !site.blob_row(b"one"),
        "分支删掉之后 v1 那一版该从库里消失"
    );
    assert_eq!(site.heads(MAIN), vec![v3], "head 那一支一条不少");
}

/// `branch rm` 那句"松开了几条 commit"的判据：要**减掉其余全部 ref**，不是只比 `main`。
///
/// 这条是纯库层用例（命令只是把数打印出来）。两个分支共用同一个根、`main` 已经挪去别处：
/// 删第一个 ⇒ 松开 0 条（第二个还挂着）；删第二个 ⇒ 松开 1 条。
/// 只比 main 的实现会在第一步就报"松开 1 条"，那是吓人。
#[test]
fn only_the_commits_nobody_else_holds_are_released() {
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1"); // 无亲代：闭包就是它自己
    site.branch(&add("a", Some(v1))).unwrap();
    site.branch(&add("b", Some(v1))).unwrap();

    build::set_ref(&*site.db, site.pid, MAIN, &[]).unwrap();
    site.write("a.txt", "three");
    let v3 = site.commit("v3");

    let refs = build::all_named_refs(&*site.db, site.pid).unwrap();
    let index = object::parent_index(&*site.db, site.pid).unwrap();
    let lost_a = history::unheld_by_other_refs(&refs, &index, &build::branch_ref("a"), &[v1]);
    assert!(
        lost_a.is_empty(),
        "分支 b 还挂着 v1 ⇒ 删 a 一条都不该松开，实际 {lost_a:?}"
    );

    let mut after_rm = refs.clone();
    after_rm.remove(&build::branch_ref("a"));
    let lost_b = history::unheld_by_other_refs(&after_rm, &index, &build::branch_ref("b"), &[v1]);
    assert_eq!(lost_b, vec![v1], "只剩 b 挂着 ⇒ 删 b 就松开 v1");

    // `main` 那一支不在任何一次"松开"里：它自己就是 ref，闭包算的是它自己的账
    let lost_main = history::unheld_by_other_refs(&refs, &index, MAIN, &[v3]);
    assert_eq!(lost_main, vec![v3], "main 挪走时 v3 只被 main 挂着");
}

/// 改名：tip 跟着走、老名字从此查不到；撞名（tag 或别的分支）一律拒。
/// **先建新再删旧**这个顺序在这条用例里表现为"任何一次拒绝都不留下半个名字"。
#[test]
fn renaming_keeps_the_tip_and_refuses_a_taken_name() {
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1");
    site.branch(&add("old", Some(v1))).unwrap();
    site.tag(&TagCmd::Add {
        name: "taken".to_string(),
        at: None,
    })
    .unwrap();

    let e = site
        .branch(&BranchCmd::Rename {
            from: "old".to_string(),
            to: "taken".to_string(),
        })
        .expect_err("不许把分支改到一个 tag 占着的名上");
    assert!(e.to_string().contains("已经有人占了"), "{e}");
    assert_eq!(
        site.heads(&build::branch_ref("old")),
        vec![v1],
        "被拒那一趟一个字都不该动"
    );
    assert_eq!(site.heads("taken"), vec![v1], "那个 tag 也不该被动过");

    site.branch(&BranchCmd::Rename {
        from: "old".to_string(),
        to: "new".to_string(),
    })
    .unwrap();
    assert!(
        site.heads(&build::branch_ref("old")).is_empty(),
        "老名字要删掉"
    );
    assert_eq!(site.heads(&build::branch_ref("new")), vec![v1], "tip 要跟着走");
    assert_eq!(site.heads(MAIN), vec![v1], "main 一个字没动");
}

/// `move` 的歧义门：多 head 时 `--to HEAD` 指着两个 commit ⇒ 拒，且 ref 不许动。
/// （`--to` 是必填的：省略它等于猜"你要 head 还是 head~3"，见 `m4::branch_move`。）
#[test]
fn moving_needs_an_unambiguous_target() {
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1");
    site.write("a.txt", "two");
    let v2 = site.commit("v2");
    site.branch(&add("dev", Some(v1))).unwrap();
    build::set_ref(&*site.db, site.pid, MAIN, &[v1, v2]).unwrap();

    let e = site
        .branch(&BranchCmd::Move {
            name: "dev".to_string(),
            to: "HEAD".to_string(),
        })
        .expect_err("两个 head 时不许挑一个");
    assert!(e.to_string().contains("2 个 commit"), "要说清有几个：{e}");
    assert_eq!(
        site.heads(&build::branch_ref("dev")),
        vec![v1],
        "被拒那一趟分支不许挪"
    );

    // 点名一个 hash 就动了
    site.branch(&BranchCmd::Move {
        name: "dev".to_string(),
        to: keys::hex(&v2),
    })
    .unwrap();
    assert_eq!(site.heads(&build::branch_ref("dev")), vec![v2]);
    assert_eq!(site.heads(MAIN), vec![v1, v2], "head 集合不许被 `move` 碰");
    assert_eq!(
        fs::read_to_string(site.work().join("a.txt")).unwrap(),
        "two",
        "`move` 只动名字，工作树不许被它改写"
    );
}

/// 名字闸与 tag **共用一份实现**（`check_ref_name`）：两套闸迟早长得不一样，
/// 那时"一个名字指着两个东西"就会从缝里钻进来。`refs/` 开头那条是分支专用的：
/// 允许它等于允许人伪造另一个命名空间。
#[test]
fn names_that_would_collide_are_refused() {
    let site = Site::new();
    site.write("a.txt", "one");
    site.commit("v1");

    for bad in ["main", "HEAD", "deadbeef", "-list", "a b", "refs/heads/x"] {
        let e = site
            .branch(&add(bad, None))
            .expect_err(&format!("{bad} 该被拒"));
        let msg = e.to_string();
        assert!(
            msg.contains("分支 名字") || msg.contains("保留名") || msg.contains("前缀"),
            "{bad} 的报错要说清是名字问题：{msg}"
        );
    }
    // 6 个十六进制字符允许（歧义门从 7 位起），带 `/` 的允许（分支名天然会写成 `feature/x`）
    for ok in ["deadbe", "feature/x"] {
        assert!(
            site.branch(&add(ok, None)).is_ok(),
            "{ok} 不该被误伤"
        );
    }
    assert_eq!(
        site.heads(&build::branch_ref("feature/x")).len(),
        1,
        "带斜杠的名字要真存下来"
    );
}
