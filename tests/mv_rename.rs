//! `mv`：安全地改个工作树里的名字。
//!
//! 这一层的价值全在"什么时候不动手"：源不存在、目标已存在、路径想逃出项目目录 ——
//! 三种都必须在 `fs::rename` **之前**拦下。改名之后库里还没记，所以用例也顺手把
//! "下一次 commit 长什么样"钉住：账面 -1/+1，但**新块数为 0**（内容寻址的必然）。
//!
//! 手工验证（跑完 `rm -rf /tmp/tsm`）：
//!
//! ```text
//! cargo build --release && TS=./target/release/twosync.exe
//! rm -rf /tmp/tsm && $TS --root /tmp/tsm init
//! mkdir -p /tmp/tsm/lab/d && echo hello > /tmp/tsm/lab/a.txt
//! $TS --root /tmp/tsm project add lab && $TS --root /tmp/tsm scan && $TS --root /tmp/tsm commit -m 起点
//! $TS --root /tmp/tsm mv a.txt d/a.txt
//! $TS --root /tmp/tsm mv d/a.txt d/a.txt        # 该拒：目标已存在
//! $TS --root /tmp/tsm mv a.txt ../../etc/x      # 该拒：路径不许含 ..
//! $TS --root /tmp/tsm scan && $TS --root /tmp/tsm commit -m 改名
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use storage_engines::EngineKind;
use twosync::cli::registry::Registry;
use twosync::cli::{Cli, Cmd};
use twosync::error::Error;
use twosync::store::build;
use twosync::store::cas::{self, Cas};
use twosync::store::keys;
use twosync::store::worktree;
use twosync::utils::{objects_dir, open_store};

struct Site {
    tmp: tempfile::TempDir,
}

impl Site {
    fn new(files: &[(&str, &[u8])]) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join(".twosync")).unwrap();
        let mut reg = Registry::default();
        for (rel, body) in files {
            let p = work(root, rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, body).unwrap();
        }
        reg.add("p", "p").unwrap();
        reg.save(root).unwrap();
        Self { tmp }
    }

    fn root(&self) -> PathBuf {
        self.tmp.path().to_path_buf()
    }

    fn cli(&self) -> Cli {
        Cli {
            root: self.root(),
            project: None,
            // None = 让 `Cli::store()` 去问配置文件，再退到内置默认：这条路径也要被用例走到。
            engine: None,
            // mv 不看 cmd，但 `Cli` 要求它存在：给个最便宜的变体，不假装是别的命令。
            cmd: Cmd::Init,
        }
    }

    fn mv(&self, src: &str, dst: &str) -> twosync::error::Result<()> {
        twosync::cli::m0::mv(&self.cli(), src, dst)
    }

    fn exists(&self, rel: &str) -> bool {
        work(&self.root(), rel).exists()
    }

    /// 扫 + 提，返回这次提交的账。
    fn commit(&self) -> build::Built {
        let root = self.root();
        let db = open_store(&root, EngineKind::LsmTree).unwrap();
        let pid = keys::project_id("p");
        let proj = root.join("p");
        worktree::scan(&*db, pid, &proj, false, 100).unwrap();
        let store = Cas::new(&objects_dir(&root), cas::DEFAULT_BLOCK).unwrap();
        build::build(
            &*db,
            pid,
            &store,
            &proj,
            build::Meta {
                peer: "t",
                msg: "用例",
                ts: 1,
                allow_mass_delete: true,
            },
        )
        .unwrap()
    }
}

fn work(root: &Path, repo_path: &str) -> PathBuf {
    root.join("p")
        .join(repo_path.replace('/', std::path::MAIN_SEPARATOR_STR))
}

#[test]
fn mv_renames_in_place_and_refuses_to_clobber() {
    let site = Site::new(&[("a.txt", b"hello"), ("keep.txt", b"same")]);
    site.commit();

    site.mv("a.txt", "moved.txt").expect("改名该成功");
    assert!(!site.exists("a.txt"));
    assert_eq!(fs::read(work(&site.root(), "moved.txt")).unwrap(), b"hello");

    let err = site
        .mv("keep.txt", "moved.txt")
        .expect_err("目标已存在必须拒");
    assert!(matches!(err, Error::LocalExists { .. }), "实际 {err}");
    assert_eq!(
        fs::read(work(&site.root(), "moved.txt")).unwrap(),
        b"hello",
        "被拒时不许已经动过手"
    );
    assert!(site.exists("keep.txt"));
}

/// 路径校验必须是**动手之前**做的：`..` / 绝对路径 / 反斜杠 / 空串都算参数不合法。
#[test]
fn mv_rejects_paths_that_escape_or_misuse_the_separator() {
    let site = Site::new(&[("a.txt", b"x")]);
    for (src, dst) in [
        ("../outside", "b.txt"),
        ("a.txt", "../outside"),
        ("a.txt", "/tmp/abs"),
        ("a.txt", "sub\\win.txt"),
        ("", "b.txt"),
        ("a.txt", ""),
    ] {
        let err = site.mv(src, dst).expect_err("{src} → {dst} 不该被放行");
        assert!(matches!(err, Error::BadInput(_)), "实际 {err}");
    }
    assert!(site.exists("a.txt"), "校验失败时一个字节都不许动");
}

#[test]
fn mv_creates_the_destination_parent_and_reports_a_missing_source() {
    let site = Site::new(&[("a.txt", b"x")]);
    site.mv("a.txt", "deep/er/b.txt").expect("父目录该自动建");
    assert!(site.exists("deep/er/b.txt"));

    let err = site.mv("nope.txt", "b.txt").expect_err("源不存在必须报错");
    assert!(matches!(err, Error::BadInput(_)), "实际 {err}");
}

/// 改名进历史的样子：账面是"一删一增"，但**新块为 0**——内容寻址让改名不复制数据。
#[test]
fn a_renamed_file_commits_as_delete_plus_add_with_no_new_blocks() {
    let site = Site::new(&[("big.bin", &[7u8; 100_000])]);
    let first = site.commit();
    assert_eq!((first.changes.add, first.changes.delete), (1, 0));
    assert!(first.blocks_stored > 0, "首轮总得写真块");

    site.mv("big.bin", "archive/big.bin").unwrap();
    let second = site.commit();
    assert_eq!(second.changes.delete, 1);
    assert_eq!(second.changes.add, 2, "archive 目录本身也算一条新增");
    assert_eq!(
        second.blobs_new, 0,
        "同一个 blob 清单已在库里 ⇒ 不该再写一遍"
    );
    assert_eq!(second.blocks_stored, 0, "改名不该产生任何新块");
}

/// 目录改名：整棵子树跟着走，且没有一步是"先删后建"的中间态。
#[test]
fn renaming_a_directory_moves_the_whole_subtree() {
    let site = Site::new(&[("d/a.txt", b"a"), ("d/e/b.txt", b"b"), ("keep", b"k")]);
    site.commit();
    site.mv("d", "d2").unwrap();
    assert!(!site.exists("d/a.txt"));
    assert!(site.exists("d2/a.txt") && site.exists("d2/e/b.txt"));

    let b = site.commit();
    // 4 而不是 2：目录条目本身也算删除（`d` 与 `d/e`），新增那侧同理。
    assert_eq!(b.changes.delete, 4, "{:?}", b.changes.sample);
    assert_eq!(b.changes.add, 4);
    assert_eq!(b.blobs_new, 0, "内容没变 ⇒ 零个新 blob");
}
