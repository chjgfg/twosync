//! `restore`：从历史取回字节，以及 §7.3 铁律的那道门。
//!
//! 这批用例的重心不是"能不能写出来"（那由 CAS 的 `read_blob` 保证），而是**什么时候不许写**：
//! 覆盖一次本地内容 = 一次删除，所以门必须拦下三种情况——没 `--force`、扫过之后又改了、
//! 改了但还没 commit。少拦一种，"历史找得回来"就会在一次误操作后变成空话。
//!
//! 手工验证（真跑一遍给人看，跑完 `rm -rf /tmp/ts5`）：
//!
//! ```text
//! cargo build --release && TS=./target/release/twosync.exe
//! rm -rf /tmp/ts5 && $TS --root /tmp/ts5 init
//! mkdir -p /tmp/ts5/lab && echo v1 > /tmp/ts5/lab/a.txt
//! $TS --root /tmp/ts5 project add lab && $TS --root /tmp/ts5 scan && $TS --root /tmp/ts5 commit -m v1
//! echo v2 > /tmp/ts5/lab/a.txt && $TS --root /tmp/ts5 scan && $TS --root /tmp/ts5 commit -m v2
//! $TS --root /tmp/ts5 restore --at HEAD~ a.txt          # 现在还没有 ~ 语法，用 show 拿首个 hash
//! $TS --root /tmp/ts5 restore --at <首个 commit 的前缀> --to /tmp/back.txt a.txt
//! cat /tmp/back.txt                                      # 必须是 v1
//! ```

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use storage_engines::KvEngine;
use twosync::error::Error;
use twosync::store::cas::Cas;
use twosync::store::keys::{self, Hash, Kind, ProjectId};
use twosync::store::worktree::WtEntry;
use twosync::store::{build, restore, worktree};
use twosync::utils::{objects_dir, open_store};

/// 小块的默认块大小：让"多块文件"在几 KiB 的语料上就能测到。
const BLK: u64 = 4096;

struct Site {
    db: Box<dyn KvEngine>,
    pid: ProjectId,
    tmp: tempfile::TempDir,
}

impl Site {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let db = open_store(tmp.path(), common::engine()).unwrap();
        let pid = keys::project_id("p");
        Self { db, pid, tmp }
    }

    fn proj(&self) -> PathBuf {
        self.tmp.path().join("p")
    }

    fn cas(&self) -> Cas {
        Cas::new(&objects_dir(self.tmp.path()), BLK).unwrap()
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        let p = self
            .proj()
            .join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, bytes).unwrap();
    }

    fn scan(&self) {
        let proj = self.proj();
        worktree::scan(&*self.db, self.pid, &proj, false, 100).unwrap();
    }

    /// 扫 + 提，返回 commit。
    fn commit(&self) -> Hash {
        self.scan();
        self.build_only()
    }

    /// 只提不扫：手工摆好 `W` 记录之后要直接提交（epoch 必须与那次扫描一致，见符号链接用例）。
    fn build_only(&self) -> Hash {
        let proj = self.proj();
        let store = self.cas();
        let b = build::build(
            &*self.db,
            self.pid,
            &store,
            &proj,
            build::Meta {
                peer: "t",
                msg: "用例",
                ts: 1,
                allow_mass_delete: false,
            },
        )
        .unwrap();
        b.commit.expect("用例里每次都要有改动")
    }

    /// 最近一次扫描盖的 epoch。
    fn epoch_of(&self, rel: &str) -> i64 {
        let raw = {
            let tx = self.db.begin();
            let got = tx.get(&keys::worktree_key(self.pid, rel));
            tx.commit();
            got
        }
        .expect("这条用例里该有它");
        // 必须显式标 `WtEntry`：`postcard::from_bytes` 的目标类型是从**返回位置**推的，
        // 而这个函数返回 i64 —— 不标注它就会拿 i64 去解 WtEntry 的字节，postcard 又不介意
        // 尾部多余字节，于是安静地解出 0，测试拿到一个假 epoch。（实测踩过。）
        let w: WtEntry = postcard::from_bytes(keys::strip(&raw).unwrap()).unwrap();
        w.scanned_at
    }

    fn head(&self) -> Hash {
        build::ref_heads(&*self.db, self.pid, build::MAIN).unwrap()[0]
    }

    fn restore(
        &self,
        from: &Hash,
        path: &str,
        dest: Option<&Path>,
        force: bool,
    ) -> twosync::error::Result<restore::Restored> {
        let store = self.cas();
        restore::restore(
            &*self.db,
            self.pid,
            &store,
            &self.proj(),
            from,
            path,
            dest,
            force,
        )
    }
}

#[test]
fn restore_to_another_place_returns_the_exact_bytes() {
    let site = Site::new();
    let body: Vec<u8> = (0..20_000usize).map(|i| (i % 253) as u8).collect();
    site.write("data/big.bin", &body);
    let c = site.commit();
    assert!(
        body.len() > 4 * BLK as usize,
        "得真的跨多块才测得出流式拼接"
    );

    let out = site.tmp.path().join("out/big.bin");
    let r = site.restore(&c, "data/big.bin", Some(&out), false).unwrap();
    assert_eq!(r.bytes, body.len() as u64);
    assert_eq!(r.blocks, 5, "20000 / 4096 向上取整");
    assert!(!r.gate_checked, "写到别处不碰工作树 ⇒ 不需要 §7.3 的门");
    assert_eq!(fs::read(&out).unwrap(), body);
}

#[test]
fn restore_refuses_to_overwrite_without_force() {
    let site = Site::new();
    site.write("a.txt", b"v1");
    let first = site.commit();
    site.write("a.txt", b"v2");
    let second = site.commit();

    let err = site
        .restore(&first, "a.txt", None, false)
        .expect_err("默认不许覆盖");
    match err {
        Error::LocalExists { hint, .. } => assert!(
            hint.contains("--force"),
            "restore 的这条该告诉人有 --force：{hint}"
        ),
        other => panic!("应是 LocalExists，实际 {other}"),
    }
    assert_eq!(fs::read_to_string(site.proj().join("a.txt")).unwrap(), "v2");
    assert_eq!(site.head(), second, "被拒的 restore 不许动 head");
}

/// 门的第一种拦法：**磁盘比最后一次扫描更新**。
/// 少了这一条检查，`W` 里那个"旧但已提交"的 blob 会让门自己打开，然后覆盖掉扫描器
/// 从没见过的那次修改——正是 §7.3 要防的事，却由检查本身放行。
#[test]
fn force_still_refuses_when_the_disk_is_newer_than_the_last_scan() {
    let site = Site::new();
    site.write("a.txt", b"v1");
    let first = site.commit();

    // 改了内容，但**不 scan**：W 还指着 v1（v1 确实在历史里，门不能只看 W）。
    site.write("a.txt", b"a much longer replacement that changes the size");
    let err = site
        .restore(&first, "a.txt", None, true)
        .expect_err("扫过之后又改过的内容不许被覆盖");
    match err {
        Error::NotInHistory { why, .. } => assert!(
            why.contains("比最后一次扫描更新"),
            "报错得说清下一步该干什么，实际 {why}"
        ),
        other => panic!("应是 NotInHistory，实际 {other}"),
    }
    assert!(
        site.proj().join("a.txt").exists(),
        "被拦下时本地内容必须还在"
    );
}

/// 门的第二种拦法：**扫过了、但那一版还没 commit** ⇒ blob 根本不在库里。
#[test]
fn force_refuses_when_the_local_version_was_scanned_but_not_committed() {
    let site = Site::new();
    site.write("a.txt", b"v1");
    let first = site.commit();

    site.write("a.txt", b"v2-not-committed");
    site.scan(); // 只 scan：W 指着 v2 的 hash，而 v2 从没进过任何 commit
    let err = site
        .restore(&first, "a.txt", None, true)
        .expect_err("没进过历史的本地内容不许被覆盖");
    match err {
        Error::NotInHistory { why, .. } => assert!(
            why.contains("不在库里") || why.contains("没被扫描过"),
            "实际 {why}"
        ),
        other => panic!("应是 NotInHistory，实际 {other}"),
    }
}

/// 走完整个闭环：改了 → scan → commit → 这时才允许 --force 覆盖回去。
#[test]
fn force_succeeds_once_the_local_version_is_committed() {
    let site = Site::new();
    site.write("a.txt", b"v1");
    let first = site.commit();

    site.write("a.txt", b"v2");
    let second = site.commit();
    assert_ne!(first, second);

    let r = site
        .restore(&first, "a.txt", None, true)
        .expect("v2 已经进历史，这时覆盖是安全的");
    assert!(r.gate_checked);
    assert_eq!(fs::read_to_string(site.proj().join("a.txt")).unwrap(), "v1");
    assert_eq!(site.head(), second, "restore 不写历史，head 不该动");
}

#[test]
fn restore_reports_missing_paths_and_points_directories_at_checkout() {
    let site = Site::new();
    site.write("sub/a.txt", b"x");
    let c = site.commit();
    let out = site.tmp.path().join("out");

    assert!(matches!(
        site.restore(&c, "sub/nope.txt", Some(&out), false),
        Err(Error::NoSuchPath { .. })
    ));
    // 目录恢复要一次处理整棵子树与删除，那是 `checkout` 的形状，不是 `restore` 的。
    let err = site
        .restore(&c, "sub", Some(&out), false)
        .expect_err("目录不该被当成文件恢复");
    assert!(matches!(err, Error::NotYet { .. }), "实际 {err}");
}

/// 符号链接的目标文本必须真的进得了历史、也出得来。
///
/// 这条是 §17 补记三那个缺口的守卫：以前 `TreeEntry` 只存目标串的 hash，于是"目标是什么"
/// 这件事在历史里永久丢失。现在目标本身当一个小 blob 落 CAS（**不动 tree 编码**），
/// 所以这里断言的是"字节能原样取回来"。
#[test]
fn symlink_target_survives_into_history_and_back() {
    let site = Site::new();
    site.write("readme.md", b"hi");
    site.scan();
    let target = "docs/guide.md";
    let hash = keys::hash_of(target.as_bytes());

    // 手工插一条符号链接的 W 记录：Windows 上建真链接要权限，而这里要验的是**存储层**
    // 能不能保住目标文本，与 OS 让不让建链接无关。
    // `scanned_at` 必须等于刚才那次扫描的 epoch —— 否则 `commit` 的"只认最新一轮"过滤会
    // 把这条当成已删除，blob 根本不会被写，测的就不是我想测的东西了（第一版就栽在这）。
    let e = WtEntry {
        kind: Kind::Symlink,
        blob_hash: hash,
        size: 0,
        mtime_ns: 0,
        mode: 0,
        symlink_target: Some(target.to_string()),
        scanned_at: site.epoch_of("readme.md"),
    };
    let tx = site.db.begin();
    tx.set(
        &keys::worktree_key(site.pid, "latest"),
        keys::wrap(postcard::to_allocvec(&e).unwrap()),
    );
    tx.commit();

    let c = site.build_only();
    let blob = build::read_blob(&*site.db, site.pid, &hash).unwrap();
    assert_eq!(blob.size, target.len() as u64);

    let back = site.tmp.path().join("out/target.txt");
    site.cas().read_blob(&blob, &back).unwrap();
    assert_eq!(
        fs::read_to_string(&back).unwrap(),
        target,
        "目标文本必须一字不差地取回来"
    );

    // 真建链接：能建就验链接指向，建不了（Windows 无权限）就必须把原因说清楚。
    let link = site.tmp.path().join("out/latest");
    match site.restore(&c, "latest", Some(&link), false) {
        Ok(r) => {
            assert_eq!(r.target.as_deref(), Some(target));
            let got = fs::read_link(&link).unwrap();
            assert_eq!(got.to_string_lossy().replace('\\', "/"), target);
        }
        Err(Error::Io(e)) => {
            let m = e.to_string();
            assert!(
                m.contains("开发者模式") || m.contains("权限"),
                "建不了链接时要交代为什么，实际 {m}"
            );
        }
        Err(other) => panic!("不该的错误类型：{other}"),
    }
}

/// `--to` 的覆盖**不看工作树那一份**。
///
/// §7.3 那道门查的是"工作树里的本地内容是不是已经在历史里"，而 `--to` 的目标根本不是
/// 那个文件。修之前这里无条件走门，于是「工作树这一版没扫过」会让一次跟工作树毫无关系的
/// 导出被 `NotInHistory` 拒掉——一句关于另一个文件的拒绝。判据只有一份门，但**门只管
/// 它管得着的路径**。
#[test]
fn restoring_to_another_place_does_not_consult_the_worktree_gate() {
    let site = Site::new();
    site.write("a.txt", b"v1");
    let first = site.commit();
    // 工作树这一版被改了，而且**没有再 scan** ⇒ 走工作树那条路时门一定会拒。
    site.write("a.txt", b"local-new-never-scanned");
    let out = site.tmp.path().join("out.bin");
    fs::write(&out, b"something-else-here").unwrap();

    let err = site
        .restore(&first, "a.txt", Some(&out), false)
        .expect_err("目标已有就必须先问人");
    assert!(
        matches!(err, Error::LocalExists { .. }),
        "拒的理由必须是目标路径，不是工作树：{err}"
    );
    assert_eq!(fs::read(&out).unwrap(), b"something-else-here");

    let r = site
        .restore(&first, "a.txt", Some(&out), true)
        .unwrap_or_else(|e| panic!("--to 不该受工作树的门管：{e}"));
    assert!(!r.gate_checked, "没查门就该如实说没查");
    assert_eq!(fs::read(&out).unwrap(), b"v1", "导出必须落地");
    assert_eq!(
        fs::read(site.proj().join("a.txt")).unwrap(),
        b"local-new-never-scanned",
        "工作树一个字都不该被动"
    );
}
