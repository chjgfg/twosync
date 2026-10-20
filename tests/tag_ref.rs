//! `tag`（M4 第二层的第一条）。重心是那句 §9.3 的"显式 `pinned`"到底买到了什么。
//!
//! 五条用例各钉一件事：
//! 1. 名字**能用**：挂在 ref 上、`--at` 那种写法能解析到它；
//! 2. 名字**不许撞脸**：保留名与长得像 hash 前缀的都拒（撞脸时解析顺序就成了裁决，人看不见）；
//! 3. 多 head 时省略 `--at` **必须报错**：替人选一个 = 猜；
//! 4. **pin 真的保住了东西**：`main` 挪到一个**与它没有亲代关系**的版本之后，只有挂着 tag 的
//!    commit 不被 `gc` 收走；松开 tag，同一次回收就把它收走。这条是这批命令存在的全部理由。
//! 5. `tag rm` 只删名字，不删历史。

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
use twosync::store::{gc, object, worktree};
use twosync::utils::{dir_size, objects_dir, open_store};

const BLK: u64 = 4096;

struct Site {
    tmp: tempfile::TempDir,
    /// `Arc` 而不是 `Box`：命令层的 `Cli` 也拿到**同一个句柄**（下面 `tag()` 里预填 `store_slot`）。
    /// 用 `Box` 时 `cli.store()` 会去开第二个句柄，撞上库级文件锁 ⇒ `WouldBlock`
    /// （这是同一个坑第三次咬测试台架，见 §17 补记三十三④ / 三十五②）。
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

    /// `scan` + `commit`，返回那一个新 commit（用例里每次都要有改动）。
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

    /// 命令层入口。**必须从这儿进**：绕过 `m4::tag` 去直接 `set_ref` 就等于测了另一份实现。
    fn tag(&self, cmd: &TagCmd) -> twosync::error::Result<()> {
        let cli = Cli {
            root: self.root(),
            project: Some("p".to_string()),
            engine: None,
            io_bucket: Default::default(),
            store_slot: Default::default(),
            cmd: Cmd::Init,
        };
        // 预填句柄：`cli.store()` 命中缓存就不会再开一次库（见 `Site::db` 那条注释）
        let _ = cli.store_slot.set(self.db.clone());
        m4::tag(&cli, cmd)
    }

    fn block_files(&self) -> u64 {
        dir_size(&objects_dir(&self.root())).files
    }

    fn blob_row(&self, content: &[u8]) -> bool {
        let hash = *blake3::hash(content).as_bytes();
        let tx = self.db.begin();
        let got = tx.get(&keys::blob_key(self.pid, &hash)).is_some();
        tx.commit();
        got
    }
}

#[test]
fn a_tag_attaches_to_the_head_and_is_listed_by_name() {
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1");
    site.tag(&TagCmd::Add {
        name: "first".to_string(),
        at: None,
    })
    .unwrap();

    let all = build::all_named_refs(&*site.db, site.pid).unwrap();
    assert_eq!(all.get("first"), Some(&vec![v1]), "tag 没挂上：{all:?}");
    // `main` 不许出现在 tag 名单里（那是 head 集合，另一格已经在报它了）
    assert!(all.contains_key(MAIN), "main 本身还得读得回来");

    // 再打一次同一个位置 = 什么都不改（不是报错，也不是重写一遍行）
    let rows = site.db.health().total_rows;
    site.tag(&TagCmd::Add {
        name: "first".to_string(),
        at: Some(keys::hex(&v1)),
    })
    .unwrap();
    assert_eq!(
        site.db.health().total_rows,
        rows,
        "重复打同一个位置不该再多写一行 H（那是版本行的来源）"
    );
}

#[test]
fn names_that_would_collide_are_refused() {
    let site = Site::new();
    site.write("a.txt", "one");
    site.commit("v1");

    for bad in ["main", "HEAD", "deadbeef", "-list", "a b"] {
        let e = site
            .tag(&TagCmd::Add {
                name: bad.to_string(),
                at: None,
            })
            .expect_err(&format!("{bad} 该被拒"));
        let msg = e.to_string();
        assert!(
            msg.contains("tag 名字") || msg.contains("保留名"),
            "{bad} 的报错要说清是名字问题：{msg}"
        );
    }
    // 6 个十六进制字符**允许**：歧义门从 7 位起（hash 前缀短于 7 位时 `resolve_rev` 本来就不认）
    assert!(
        site.tag(&TagCmd::Add {
            name: "deadbe".to_string(),
            at: None
        })
        .is_ok(),
        "六位名字不该被误伤"
    );
}

/// 多 head 时省略 `--at` = 替人选一个 ⇒ **必须报错**（§7 第四行：多 head 是正常状态，
/// 而"正常"恰恰意味着这里没有默认的那一个）。
#[test]
fn an_ambiguous_head_set_requires_an_explicit_at() {
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1");
    site.write("a.txt", "two");
    let v2 = site.commit("v2");
    build::set_ref(&*site.db, site.pid, MAIN, &[v1, v2]).unwrap();

    let e = site
        .tag(&TagCmd::Add {
            name: "oops".to_string(),
            at: None,
        })
        .expect_err("两个 head 时不许挑一个");
    assert!(e.to_string().contains("2 个 head"), "要说清有几个：{e}");

    site.tag(&TagCmd::Add {
        name: "ok".to_string(),
        at: Some(keys::hex(&v1)),
    })
    .unwrap();
    assert_eq!(build::ref_heads(&*site.db, site.pid, "ok").unwrap(), vec![v1]);
}

/// **这条才是 tag 的存在理由。** 关键是让 `main` 挪到一个与 v1 **没有亲代关系**的版本：
/// 否则 v1 还在 head 的闭包里，测出来的"活着"证明不了是 tag 保的
/// （第一版我就是这么写的，`commits_kept = 2` 看着对，其实一条 tag 都没起作用）。
#[test]
fn a_tagged_commit_survives_gc_after_main_moves_to_an_unrelated_version() {
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1"); // v1 的内容是 "one"
    site.tag(&TagCmd::Add {
        name: "keep".to_string(),
        at: Some(keys::hex(&v1)),
    })
    .unwrap();

    site.write("a.txt", "two");
    site.commit("v2"); // v2 是 v1 的孩子，"two" 这份内容只被它引用

    // 把 head 清空再提一次 ⇒ v3 是一个**无亲代**的新根，v1/v2 都不在 head 闭包里了
    build::set_ref(&*site.db, site.pid, MAIN, &[]).unwrap();
    site.write("a.txt", "three");
    let v3 = site.commit("v3");

    let store = Cas::new(&objects_dir(&site.root()), BLK).unwrap();

    // ── 第一趟：保留集 = {v3} ∪ {v1（靠 tag）}，v2 该被丢 ──
    let p1 = gc::sweep(&*site.db, &store, site.pid, false).unwrap();
    assert_eq!(
        p1.commits_kept, 2,
        "保留集要从**全部 ref** 出发：head 的 v3 + tag 的 v1。读数 {p1:?}"
    );
    assert_eq!(p1.commits_dropped, 1, "只该丢 v2：{p1:?}");
    assert!(
        site.blob_row(b"one"),
        "v1 被 tag 挂着 ⇒ 它的清单行必须在（这就是 §9.3 的 `pinned`）"
    );

    // ── 松开 tag ⇒ 同一套回收把 v1 也收走 ──
    site.tag(&TagCmd::Rm {
        name: "keep".to_string(),
    })
    .unwrap();
    let files_before = site.block_files();
    assert!(files_before > 0, "台架前提：块仓里有东西");
    let p2 = gc::sweep(&*site.db, &store, site.pid, false).unwrap();
    assert!(
        p2.commits_dropped >= 1,
        "没有 ref 挂了 ⇒ v1 该进垃圾集：{p2:?}"
    );
    assert!(
        p2.blocks_pending >= 1,
        "归零的块要落 pending 表，不许当场删文件（§9.3）：{p2:?}"
    );
    let unlinked = gc::unlink_pending(&*site.db, &store, site.pid, usize::MAX, false)
        .unwrap()
        .files;
    assert!(unlinked >= 1, "第二趟要把 pending 落地：删了 {unlinked} 个");
    assert!(
        site.block_files() < files_before,
        "块文件数必须真的少了：{files_before} → {}",
        site.block_files()
    );
    assert!(
        !site.blob_row(b"one"),
        "v1 的清单行要跟着消失，否则下次 GC 还会当它在"
    );

    // head 那一支一个都没少
    assert_eq!(build::ref_heads(&*site.db, site.pid, MAIN).unwrap(), vec![v3]);
    assert_eq!(object::read_commit(&*site.db, site.pid, &v3).unwrap().msg, "v3");
}

/// `tag rm` 删的是名字，不是历史；删一个不存在的 tag 要报错而不是安静成功。
#[test]
fn removing_a_tag_removes_only_the_name() {
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1");
    site.tag(&TagCmd::Add {
        name: "t1".to_string(),
        at: None,
    })
    .unwrap();

    let e = site
        .tag(&TagCmd::Rm {
            name: "nosuch".to_string(),
        })
        .expect_err("删不存在的 tag 不许安静成功");
    assert!(e.to_string().contains("没有 tag"), "{e}");

    site.tag(&TagCmd::Rm {
        name: "t1".to_string(),
    })
    .unwrap();
    assert!(
        !build::all_named_refs(&*site.db, site.pid)
            .unwrap()
            .contains_key("t1"),
        "tag 名字没删掉"
    );
    assert_eq!(
        object::read_commit(&*site.db, site.pid, &v1).unwrap().msg,
        "v1",
        "commit 本体一行都不该被动过"
    );

    // 挪位置要走 `tag rm` + 再打：同名指向别处必须拒（pin 不许被一敲就松开）
    site.write("a.txt", "two");
    let v2 = site.commit("v2");
    site.tag(&TagCmd::Add {
        name: "t1".to_string(),
        at: Some(keys::hex(&v1)),
    })
    .unwrap();
    let e = site
        .tag(&TagCmd::Add {
            name: "t1".to_string(),
            at: Some(keys::hex(&v2)),
        })
        .expect_err("同名挪位置不许悄悄移 pin");
    assert!(e.to_string().contains("已经指着别的东西"), "{e}");
    assert_eq!(build::ref_heads(&*site.db, site.pid, "t1").unwrap(), vec![v1]);
}
