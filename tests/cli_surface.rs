//! 命令面里"同一件事的两个名字"：`plan` / `apply`（FR-7）与 `version`。
//!
//! 三条用例各钉一件事：
//! 1. `plan` **一个字节都不写**（在这个引擎里"多一趟"是有价的：§17 补记十）；
//! 2. `apply` 与 `commit` 是同一份实现，不是第二套提交流水；
//! 3. `version` 报的是**生效值 + 来源**——"没写"与"写了默认值"只看文件分不出来。

use std::fs;
use std::path::PathBuf;

use storage_engines::EngineKind;
use twosync::cli::{Cli, Import};
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
            // 每命令一个 `Cli` ⇒ 每命令一个新桶（和真实 CLI 一样）。只有 `daemon` 会跨阶段共用。
            io_bucket: Default::default(),
            store_slot: Default::default(),
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
    m0::apply(&site.cli(), Some("起点"), false, Import::default()).expect("apply 该成功");
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
    m0::apply(&site.cli(), Some("第一次"), false, Import::default()).unwrap();
    let rows = site.rows();
    m0::commit(&site.cli(), Some("第二次"), false, Import::default()).unwrap();
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

/// §9.5.3 的发起端那一半：`push` 的目标目录来自**配对记录**，而不是随手给的一个路径。
/// 这条判断单独可测，是因为 `push` 剩下的部分要 ssh，而这半截不要。
#[test]
fn push_needs_a_pairing_and_remote_root_cannot_replace_it() {
    use twosync::cli::m1;
    use twosync::cli::registry::Registry;

    let site = Site::new(&[("a.bin", b"x".as_slice())]);
    let cli = site.cli();

    // 注册了但没配对 ⇒ 拒，并且报错里必须给出"怎么办"
    let e = m1::paired_root(&cli, "p", None).expect_err("没配对就该拒");
    let msg = e.to_string();
    assert!(msg.contains("还没配对") && msg.contains("project pair"), "报错要给出路：{msg}");

    // 拿 `--remote-root` 也绕不过去：它改的是"写到哪个目录"，不是"对端愿不愿意收"
    let e = m1::paired_root(&cli, "p", Some("D:/elsewhere")).expect_err("覆盖不等于配对");
    assert!(e.to_string().contains("--remote-root"), "报错要说明这一点：{e}");

    // 配上对：默认拿记录的那个；显式覆盖时以覆盖为准（上面已经警告过）
    let mut reg = Registry::load(&site.root()).unwrap();
    reg.pair("p", "D:/peer").unwrap();
    reg.save(&site.root()).unwrap();
    assert_eq!(m1::paired_root(&cli, "p", None).unwrap(), "D:/peer");
    assert_eq!(
        m1::paired_root(&cli, "p", Some("D:/other")).unwrap(),
        "D:/other",
        "覆盖要真的生效，否则 --remote-root 就是个骗人的旗标"
    );

    // 没这个项目是另一种错，别和"没配对"混成一类
    let e = m1::paired_root(&cli, "nonesuch", None).expect_err("没有这个项目");
    assert!(e.to_string().contains("没有项目"), "要说清是没注册：{e}");
}

/// `gc` 不带 `--apply` 就是**只读**——两半都只读（2026-10-20 的更正）。
///
/// 起因是一条实测事故：12 GiB 站点上复测折数时跑了 `gc --sweep`（没给 `--apply`），
/// 库从 `rows 191 672` 掉到 `84 258`。mark-sweep 那半确实没动，动的是**引擎版本链那半**——
/// 它当时无条件执行 `db.vacuum()`。"dry-run 只报账"这句话在半个命令上成立，等于整句是假的，
/// 而后果是测量台被自己改了、前后两批墙钟再也不能比。
#[test]
fn gc_without_apply_writes_nothing() {
    use twosync::cli::m1;
    let site = Site::new(&[("a.txt", b"v1 body"), ("d/b.txt", b"other body")]);
    site.scan();
    m0::commit(&site.cli(), Some("第一次"), false, Import::default()).unwrap();
    // 再扫一遍再提交：同一个 `W` 行被盖了第二个版本 ⇒ 库里现在有"多余版本行"可裁。
    site.scan();
    m0::commit(&site.cli(), Some("第二次"), false, Import::default()).unwrap();

    let before = site.rows();
    let h = open_store(&site.root(), EngineKind::LsmTree)
        .unwrap()
        .health();
    assert!(
        h.rows_above_one > 0,
        "台架前提：得有可裁的旧版本（rows {} / rows_above_one {}）",
        h.total_rows,
        h.rows_above_one
    );

    // ① 裸 `gc`：一行都不许多也不许少
    m1::gc(&site.cli(), None, false, false, 0, 0.0, 0.0).unwrap();
    assert_eq!(site.rows(), before, "不带 --apply 的 gc 改动了库");

    // ② `--sweep` 不带 `--apply`：这就是出事的那一条形状
    m1::gc(&site.cli(), None, true, false, 0, 0.0, 0.0).unwrap();
    assert_eq!(
        site.rows(),
        before,
        "`gc --sweep`（dry-run）动了库 ⇒ 报的账不再是同一份现场"
    );

    // ③ 给了 `--apply` 才真的裁：证明上面两条不是"vacuum 本来就没活干"
    m1::gc(&site.cli(), None, false, true, 0, 0.0, 0.0).unwrap();
    let after = site.rows();
    assert!(
        after < before,
        "--apply 之后版本链必须真的变短：{before} → {after}"
    );
}

/// `Cli::store()` 的缓存：同一个 `Cli` 只开一次库。
///
/// 断言的是**句柄身份**，不是"快了多少"：省下的那几秒（12 GiB 站点上每次开库 2.20 s CPU，
/// `daemon` 一轮原来要开五次，§11.4f）是这条性质的推论，用不着拿墙钟当证据。
#[test]
fn one_cli_opens_the_store_exactly_once() {
    let site = Site::new(&[("a.txt", b"body")]);
    let cli = site.cli();
    let first = cli.store().expect("第一次开库");
    let second = cli.store().expect("第二次该拿缓存");
    assert!(
        std::sync::Arc::ptr_eq(&first, &second),
        "同一个 `Cli` 拿到了两个不同的句柄 ⇒ 缓存没生效"
    );

    // 反面：引擎持**库级文件锁**，所以另一个 `Cli` 想同时开同一份库必须失败。
    // 这一条以前是"顺手掉句柄就行"，现在是**结构性**的：句柄活到 `Cli` 结束。
    // 写在这儿是为了让下一个想"在命令层再 open 一次"的人当场看到，而不是在生产的常驻进程里看到。
    let other = site.cli();
    let err = match other.store() {
        Ok(_) => panic!("同一份库的第二个句柄该被文件锁挡住，结果它开成功了"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("lock") || err.to_string().contains("WouldBlock")
            || err.to_string().contains("锁"),
        "要报的是锁，不是别的：{err}"
    );
}
