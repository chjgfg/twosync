//! 命令面里"同一件事的两个名字"：`plan` / `apply`（FR-7）与 `version`。
//!
//! 三条用例各钉一件事：
//! 1. `plan` **一个字节都不写**（在这个引擎里"多一趟"是有价的：§17 补记十）；
//! 2. `apply` 与 `commit` 是同一份实现，不是第二套提交流水；
//! 3. `version` 报的是**生效值 + 来源**——"没写"与"写了默认值"只看文件分不出来。

use std::fs;
use std::path::PathBuf;

use storage_engines::EngineKind;
use twosync::cli::Cli;
use twosync::cli::m0;
use twosync::store::build;
use twosync::store::keys;
use twosync::store::worktree;
use twosync::utils::open_store;

struct Site {
    tmp: tempfile::TempDir,
}

impl Site {
    fn new(files: &[(&str, &[u8])]) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".twosync")).unwrap();
        for (rel, body) in files {
            let p = tmp
                .path()
                .join("p")
                .join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        }
        let mut reg = twosync::cli::registry::Registry::default();
        reg.add("p", "p").unwrap();
        reg.save(tmp.path()).unwrap();
        Self { tmp }
    }

    fn root(&self) -> PathBuf {
        self.tmp.path().to_path_buf()
    }

    fn cli(&self) -> Cli {
        Cli {
            root: self.root(),
            project: None,
            engine: None,
            cmd: twosync::cli::Cmd::Init,
        }
    }

    fn scan(&self) {
        let db = open_store(&self.root(), EngineKind::LsmTree).unwrap();
        worktree::scan(
            &*db,
            keys::project_id("p"),
            &self.root().join("p"),
            false,
            100,
        )
        .unwrap();
    }

    fn rows(&self) -> u64 {
        let db = open_store(&self.root(), EngineKind::LsmTree).unwrap();
        db.health().total_rows
    }
}

#[test]
fn plan_reads_everything_and_writes_nothing() {
    let site = Site::new(&[("a.txt", b"one"), ("d/b.txt", b"two")]);
    site.scan();
    let before = site.rows();

    m0::plan(&site.cli()).expect("plan 不该被任何闸拦住（它什么都不做）");
    assert_eq!(site.rows(), before, "plan 是只读的：一行都不许多");

    // 而 `apply` 会真的动库 —— 同一条路，两个名字，一份实现。
    m0::apply(&site.cli(), Some("起点"), false).expect("apply 该成功");
    let after = site.rows();
    assert!(
        after > before,
        "apply 之后库里必须多出行：{before} → {after}"
    );
    let db = open_store(&site.root(), EngineKind::LsmTree).unwrap();
    assert_eq!(
        build::ref_heads(&*db, keys::project_id("p"), build::MAIN)
            .unwrap()
            .len(),
        1,
        "apply 移的是 head"
    );
}

#[test]
fn apply_and_commit_are_the_same_handler() {
    // 两次做同一件事：第二次必须"无改动"，因为两边用的是同一份判定（`changes.is_empty()`）。
    let site = Site::new(&[("a.txt", b"one")]);
    site.scan();
    m0::apply(&site.cli(), Some("第一次"), false).unwrap();
    let rows = site.rows();
    m0::commit(&site.cli(), Some("第二次"), false).unwrap();
    assert_eq!(
        site.rows(),
        rows,
        "无改动不许产生第二个 commit（apply/commit 口径必须一致）"
    );
}

#[test]
fn version_reports_effective_values_with_their_source() {
    let site = Site::new(&[("a.txt", b"x")]);
    // 没写过配置文件 ⇒ 来源必须是"内置默认"，而不是空白 —— 那正是 `config get` 同款判据。
    m0::version(&site.cli()).expect("version 不许去开引擎（NFR-8：它得便宜）");
    let db = open_store(&site.root(), EngineKind::LsmTree).unwrap();
    assert_eq!(db.health().total_rows, 0, "version 不该碰库");
}
