//! `mv`：安全地改个工作树里的名字。
//!
//! 这一层的价值全在"什么时候不动手"：源不存在、目标已存在、路径想逃出项目目录 ——
//! 三种都必须在 `fs::rename` **之前**拦下。改名之后库里还没记，所以用例也顺手把
//! "下一次 commit 长什么样"钉住：按 FR-1 折成**改名**（`R`），且**新块数为 0**
//! （内容寻址的必然）。折叠必须在 FR-10 熔断之前发生，否则大规模 `mv` 会被当成大规模删除。
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
            io_bucket: Default::default(),
            store_slot: Default::default(),
            cmd: Cmd::Init,
        }
    }

    fn mv(&self, src: &str, dst: &str) -> twosync::error::Result<()> {
        twosync::cli::m0::mv(&self.cli(), src, dst)
    }

    fn exists(&self, rel: &str) -> bool {
        work(&self.root(), rel).exists()
    }

    /// 与 `commit()` 同一条路，但**不给** `--allow-mass-delete`：用来测 FR-10 会不会误拦。
    fn commit_gated(&self) -> twosync::error::Result<build::Built> {
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
                allow_mass_delete: false,
            },
        )
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

/// **FR-1 的改名**：同一份内容换了路径 ⇒ 记一次改名，不复制数据也不占传输。
/// 折叠之前账面是"一删一增"（`archive` 那一层是新目录，仍算一条新增）。
#[test]
fn a_renamed_file_commits_as_a_rename_with_no_new_blocks() {
    let site = Site::new(&[("big.bin", &[7u8; 100_000])]);
    let first = site.commit();
    assert_eq!((first.changes.add, first.changes.delete), (1, 0));
    assert!(first.blocks_stored > 0, "首轮总得写真块");

    site.mv("big.bin", "archive/big.bin").unwrap();
    let second = site.commit();
    let cs = &second.changes;
    assert_eq!(
        (cs.rename, cs.add, cs.delete),
        (1, 1, 0),
        "改名 1 条 + 新目录 1 条，删除侧该被折叠掉：{cs:?}"
    );
    assert_eq!(
        cs.renames,
        vec![("big.bin".to_string(), "archive/big.bin".to_string())],
        "样本要给 `(旧, 新)`：{cs:?}"
    );
    assert_eq!(
        cs.deleted_blobs.len(),
        0,
        "内容还在新树里 ⇒ 它不是被删的 blob"
    );
    assert_eq!(
        cs.deleted_bytes, 0,
        "改名不该记出删除字节（FR-10 的字节闸会虚高）"
    );
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
    // 4 而不是 2：目录条目本身也各算一条（`d` 与 `d/e`）。目录的 hash 是子树 tree hash，
    // 整棵子树原样搬走 ⇒ 两侧 hash 相同 ⇒ 连目录自己一起配成改名（FR-1）。
    let cs = &b.changes;
    assert_eq!((cs.rename, cs.add, cs.delete), (4, 0, 0), "{cs:?}");
    assert!(
        b.commit.is_some(),
        "折叠后 add/delete 都归零：`is_empty` 漏算 rename 就会把它当无改动拒掉，改名永远进不了历史"
    );
    assert_eq!(b.blobs_new, 0, "内容没变 ⇒ 零个新 blob");
    assert_eq!(b.blocks_stored, 0, "整棵子树搬家也不该写一个新块");
    let mut pairs = cs.renames.clone();
    pairs.sort();
    assert_eq!(
        pairs,
        vec![
            ("d".to_string(), "d2".to_string()),
            ("d/a.txt".to_string(), "d2/a.txt".to_string()),
            ("d/e".to_string(), "d2/e".to_string()),
            ("d/e/b.txt".to_string(), "d2/e/b.txt".to_string()),
        ],
        "{pairs:?}"
    );
}

/// FR-10 的熔断看的是折叠**之后**的删除数：一次大规模 `mv`（把一半文件挪进子目录）
/// 不该被当成"删了一半仓库"。这也是折叠必须在 `check_mass_delete` 之前的原因。
#[test]
fn a_mass_rename_does_not_trip_the_delete_circuit_breaker() {
    // 30 个内容互不相同的文件：改名配对要求 hash 相同，内容撞车的文件配不出改名。
    let names: Vec<String> = (0..30).map(|i| format!("f{i}.bin")).collect();
    let bodies: Vec<Vec<u8>> = (0..30u8).map(|i| vec![i; 64]).collect();
    let files: Vec<(&str, &[u8])> = names
        .iter()
        .zip(bodies.iter())
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .collect();
    let site = Site::new(&files);
    site.commit();
    for i in 0..15 {
        site.mv(&format!("f{i}.bin"), &format!("moved/f{i}.bin"))
            .expect("改名本身该成功");
    }
    // 不给 `--allow-mass-delete`：15/30 = 50% > 20%，**没折叠就会被熔断**。
    let b = site
        .commit_gated()
        .unwrap_or_else(|e| panic!("15 条改名被当成大规模删除了：{e}"));
    assert_eq!(b.changes.delete, 0, "{:?}", b.changes);
    assert_eq!(b.changes.rename, 15, "{:?}", b.changes);
    // `moved` 这一层是真新增（旧树里没有对应的空/同内容目录），它不该被折掉。
    assert_eq!(b.changes.add, 1, "{:?}", b.changes);
}
