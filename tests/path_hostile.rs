//! §10.3 路径对抗：敌意路径要么**拒绝**，要么**不崩**，且**永远不许在根外面留东西**。
//!
//! 这一层的价值不在"覆盖率高"，而在那条不变的性质：本项目是把路径拼成磁盘路径去写的
//! （`restore` / `checkout`），所以一个 `..` 就是一次逃出项目目录。清单按 §10.3 抄：
//! `../../`、`\\?\`、UNC、`CON`、尾空格/点、超长名、仅大小写不同、NFC/NFD 混合、含 ANSI 转义。
//!
//! 手工验证（跑完 `rm -rf /tmp/tsf`）：
//!
//! ```text
//! cargo build --release && TS=./target/release/twosync.exe
//! rm -rf /tmp/tsf && $TS --root /tmp/tsf init
//! echo hi > /tmp/tsf/lab/a.txt && $TS --root /tmp/tsf project add lab
//! $TS --root /tmp/tsf scan && $TS --root /tmp/tsf commit -m 起点
//! $TS --root /tmp/tsf restore --at HEAD "../../etc/x"   # 该拒：路径含 ..
//! $TS --root /tmp/tsf restore --at HEAD "C:/Windows/x"  # 该拒：库里没有这条路径
//! $TS --root /tmp/tsf checkout --at HEAD --to /tmp/tsf/lab/inside   # 该拒：目标在工作树里
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use storage_engines::EngineKind;
use twosync::error::Error;
use twosync::store::build::{self, Meta};
use twosync::store::cas::{self, Cas};
use twosync::store::checkout::{self, ExportOpts};
use twosync::store::keys::{self, Hash, Kind, ProjectId};
use twosync::store::{history, object, restore, worktree};
use twosync::utils::{objects_dir, open_store};

/// §10.3 的清单。**都不在库里**，所以每一条都必须被拒——判据不是"报错"这一件事，
/// 而是"报错 + 不崩 + 根外什么都没多出来"。
/// 解析成"根"而不是"查不到"的两种写法：`resolve` 滤掉空段与 `.`，所以它们等价于 `""`。
///
/// 这不是路径逃逸口子——落地点仍然是 `<work_root>/...` 本身，`restore` 因为类型是目录而拒，
/// `checkout --prefix` 则把它当成"整棵树"（与不给 `--prefix` 同义）。这条语义要显式钉住，
/// 否则下一个人会以为空路径/斜杠是"查不到"。
const ROOT_ALIKE: &[&str] = &["", "/"];

fn hostile() -> Vec<String> {
    let literal: &[&str] = &[
        "/etc/passwd",
        "..",
        "../escape",
        "./../escape",
        "a/../../escape",
        "a.txt/../b.txt",
        "C:/Windows/win.ini",
        "\\\\?\\C:\\Windows\\x",
        "\\\\server\\share\\x",
        "a\\b",
        "CON",
        "com1.txt",
        "nul",
        "\u{1b}[31mred.txt",
        "尾随点.txt.",
        "尾随空格.txt ",
        "d/../d/b.txt",
        "\r",
        "\n",
        "a\nb",
        "\u{0}",
    ];
    let mut v: Vec<String> = literal.iter().map(|s| s.to_string()).collect();
    // 超长名：Linux 上单段 255 字节就建不出文件，Windows 上整路径撞 MAX_PATH。
    // 这一条测的不是"能不能建"，而是"库里查不到时绝不 panic、也不往根外写"。
    v.push(format!("{}.bin", "x".repeat(300)));
    v.push("y".repeat(400));
    v
}

struct Site {
    tmp: tempfile::TempDir,
    db: Box<dyn storage_engines::KvEngine>,
    pid: ProjectId,
}

impl Site {
    fn new(files: &[(&str, &[u8])]) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let db = open_store(tmp.path(), EngineKind::LsmTree).unwrap();
        let pid = keys::project_id("p");
        for (rel, body) in files {
            let p = Self::at(tmp.path(), rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        }
        Self { tmp, db, pid }
    }

    fn root(&self) -> PathBuf {
        self.tmp.path().to_path_buf()
    }

    fn proj(&self) -> PathBuf {
        self.root().join("p")
    }

    fn at(root: &Path, rel: &str) -> PathBuf {
        root.join("p")
            .join(rel.replace('/', std::path::MAIN_SEPARATOR_STR))
    }

    fn commit(&self) -> Hash {
        let proj = self.proj();
        worktree::scan(&*self.db, self.pid, &proj, false, 100).unwrap();
        let store = self.store();
        build::build(
            &*self.db,
            self.pid,
            &store,
            &proj,
            Meta {
                peer: "t",
                msg: "用例",
                ts: 1,
                allow_mass_delete: true,
            },
        )
        .unwrap()
        .commit
        .expect("用例每次都要有改动")
    }

    fn store(&self) -> Cas {
        Cas::new(&objects_dir(&self.root()), cas::DEFAULT_BLOCK).unwrap()
    }

    fn tree(&self, commit: &Hash) -> Hash {
        object::read_commit(&*self.db, self.pid, commit)
            .unwrap()
            .tree
    }
}

/// 整棵站点目录的文件名清单。用来证明"一轮敌意尝试之后什么都没多出来"。
fn listing(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let rel = e.path().strip_prefix(root).unwrap().display().to_string();
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(e.path());
            }
            out.push(rel);
        }
    }
    out.sort();
    out
}

#[test]
fn hostile_paths_are_refused_and_leave_nothing_behind() {
    let site = Site::new(&[("a.txt", b"hi"), ("d/b.txt", b"bee")]);
    let c = site.commit();
    let tree = site.tree(&c);
    let store = site.store();
    let proj = site.proj();
    let before = listing(&site.root());

    // 上面那一族：解析成根 ⇒ 拒绝的理由必须是"目录归 checkout 管"，不是"库里没有"。
    for probe in ROOT_ALIKE {
        let root_entry = history::resolve(&*site.db, site.pid, &tree, probe)
            .unwrap()
            .expect("`{probe:?}` 该解析成根");
        assert_eq!(root_entry.kind, Kind::Dir, "`{probe:?}` 不是根");
        let err = restore::restore(&*site.db, site.pid, &store, &proj, &c, probe, None, true)
            .expect_err("restore 一个目录该拒（那是 checkout 的活）");
        assert!(matches!(err, Error::NotYet { .. }), "实际 {err}");
        // 同一个判据也要盖住"库里有、盘上也存在着"的目录：那同样不是覆盖，是 checkout 的活。
        let err = restore::restore(&*site.db, site.pid, &store, &proj, &c, "d", None, true)
            .expect_err("restore 一个存在的目录也该拒");
        assert!(
            matches!(err, Error::NotYet { .. }),
            "目录该报「归 checkout 管」，实际却是 {err}"
        );
    }

    for p in hostile() {
        // 读侧：resolve 必须给 None（"这个版本里没有这条路径"），不许 panic。
        assert!(
            history::resolve(&*site.db, site.pid, &tree, p.as_str())
                .unwrap()
                .is_none(),
            "`{p:?}` 竟然在库里有对应条目"
        );

        // 写侧：`restore --force` 也必须拒。允许的错误只有"没有这条路径"与"目录归 checkout 管"。
        let err = restore::restore(
            &*site.db,
            site.pid,
            &store,
            &proj,
            &c,
            p.as_str(),
            None,
            true,
        )
        .err()
        .unwrap_or_else(|| panic!("`{p:?}` 竟然写成功了——那一写就在根外面"));
        assert!(
            matches!(err, Error::NoSuchPath { .. } | Error::NotYet { .. }),
            "`{p:?}` 报的是 {err}"
        );

        // 前缀侧：`checkout --prefix` 走同一条判据。
        let err = checkout::checkout(
            &*site.db,
            site.pid,
            &store,
            &proj,
            &c,
            Some(p.as_str()),
            checkout::Opts::default(),
        )
        .err()
        .unwrap_or_else(|| panic!("`--prefix {p:?}` 竟然被接受了"));
        assert!(
            matches!(err, Error::NoSuchPath { .. } | Error::NotYet { .. }),
            "`--prefix {p:?}` 报的是 {err}"
        );
    }

    // 一整轮敌意尝试之后：目录树一字未变。这一条比上面所有断言加起来都硬。
    assert_eq!(
        listing(&site.root()),
        before,
        "拒了还留下文件 = 静默的目录污染"
    );
}

/// 导出目标落在工作树里 ⇒ 拒。防的是"下一轮 `scan` 把导出的副本当成项目内容"。
#[test]
fn export_into_the_worktree_is_refused() {
    let site = Site::new(&[("a.txt", b"hi")]);
    let c = site.commit();
    let store = site.store();
    let inside = site.proj().join("out");
    let err = checkout::export(
        &*site.db,
        site.pid,
        &store,
        &ExportOpts {
            target: &c,
            prefix: None,
            dest: &inside,
            work_root: &site.proj(),
            force: false,
            apply: true,
        },
    )
    .expect_err("工作树内的导出目标必须被拒");
    assert!(matches!(err, Error::BadInput(_)), "实际 {err}");
    assert!(!inside.exists());
}

/// **古怪但合法**的名字必须能整趟走完：扫 → 提 → 导出到别处 → 字节全等。
///
/// 这些名字两边平台都合法（`#%`、`=+,.`、中日韩、空格、NFC/NFD 两种形式、长名字；
/// 制表符与尾随点/空格在 Windows 上就不合法了，所以不收进来——收了就是测平台而不是测自己），
/// 它们正是"字符串处理有没有偷偷归一化"的试纸：任何一处把分隔符/大小写/Unicode 形式折叠掉，
/// 这里就会少一个文件，或者取回串了档的内容。
#[test]
fn weird_but_legal_names_round_trip() {
    let long = format!("{}.bin", "长".repeat(60));
    let nfc = "café-NFC.txt";
    let nfd = "café-NFD.txt"; // 带组合符：与 nfc 只差在 Unicode 形式
    let files: Vec<(String, Vec<u8>)> = vec![
        ("带 空格 和 中文.bin".to_string(), b"content-a".to_vec()),
        ("a#b%c(d).txt".to_string(), b"content-b".to_vec()),
        ("s=e+,.txt".to_string(), b"content-c".to_vec()),
        (long, b"content-d".to_vec()),
        (nfc.to_string(), b"content-e".to_vec()),
        (nfd.to_string(), b"content-f".to_vec()),
    ];
    let names: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .collect();
    let site = Site::new(&names);
    let c = site.commit();

    // 六条都在树里，且各是文件条目（NFC/NFD 没被折成同一条 ⇒ 计数本身就是探针）。
    let mut got: Vec<String> = Vec::new();
    history::walk_tree(&*site.db, site.pid, &site.tree(&c), &mut |path, e| {
        assert_eq!(e.kind, Kind::File, "{path} 类型不对");
        got.push(path.to_string());
        Ok(())
    })
    .unwrap();
    let mut want: Vec<String> = files.iter().map(|(n, _)| n.clone()).collect();
    got.sort();
    want.sort();
    assert_eq!(got, want, "树里的名字集合被改动过（归一化/折叠的嫌疑）");

    // 导出一份，逐文件比字节。
    let out = site.root().join("export");
    let store = site.store();
    checkout::export(
        &*site.db,
        site.pid,
        &store,
        &ExportOpts {
            target: &c,
            prefix: None,
            dest: &out,
            work_root: &site.proj(),
            force: false,
            apply: true,
        },
    )
    .unwrap();
    for (name, body) in &files {
        let got = fs::read(out.join(name.replace('/', std::path::MAIN_SEPARATOR_STR)))
            .unwrap_or_else(|e| panic!("`{name}` 取不回来：{e}"));
        assert_eq!(&got, body, "`{name}` 内容不对（名字相似导致串档？）");
    }
}

/// 仅大小写不同的两个名字：**只在区分大小写的文件系统上才存在**。
/// Windows 上根本建不出这两个文件，所以这条挂 `#[cfg(unix)]`；它要证明的是"库里的身份是
/// 原始字节，不做大小写折叠"（`keys::fold_path` 只服务于比较/排除，不参与身份）。
#[cfg(unix)]
#[test]
fn case_only_differing_names_stay_distinct() {
    let site = Site::new(&[("a.txt", b"upper"), ("A.txt", b"lower")]);
    let c = site.commit();
    let mut n = 0;
    history::walk_tree(&*site.db, site.pid, &site.tree(&c), &mut |_, _| {
        n += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(n, 2, "两个都是独立条目：折叠就等于把一份历史塞给两个路径");
}

// ── §8 的大小写门：折叠相等的名字，落到不区分大小写的盘上必须拒 ────────────

/// 一条最小清单条目（门只看名字，hash 是真块无所谓——检出门不读字节）。
fn entry(name: &str, body: &[u8]) -> object::TreeEntry {
    object::TreeEntry {
        name: name.to_string(),
        kind: Kind::File,
        hash: keys::hash_of(body),
        size: body.len() as u64,
        mode: 0o100644,
    }
}

/// 检测器本身：只有"折叠之后相等、字节不同"才算碰撞。
#[test]
fn fold_detector_pairs_case_twins_only() {
    let twins = vec![entry("README.md", b"a"), entry("Readme.md", b"b")];
    assert_eq!(
        object::fold_collision(&twins),
        Some(("README.md".to_string(), "Readme.md".to_string())),
        "这一对在不敏感的目标盘上是同一个格子"
    );
    let distinct = vec![entry("readme.md", b"a"), entry("notes.md", b"b")];
    assert_eq!(object::fold_collision(&distinct), None);
    // 完全同名是另一件事（`put_tree` 自己就拒），不该混进"折叠碰撞"这一类。
    let same = vec![entry("x.md", b"a"), entry("x.md", b"b")];
    assert_eq!(object::fold_collision(&same), None, "同名同字不算折叠碰撞");
}

/// 门随目标盘变：**同一条历史**在大小写敏感的盘上合法、搬到不敏感的盘上就该拒。
/// 树是手工造的（`put_tree`），所以这条在 Windows 开发机上也测得到——盘上根本建不出
/// 一对仅大小写不同的文件（§10.3 例外①），只有绕开磁盘才测得动这条门。
#[test]
fn case_gate_refuses_only_when_the_target_folds() {
    let site = Site::new(&[("keep.txt", b"keep")]);
    let list = vec![
        entry("README.md", b"one"),
        entry("Readme.md", b"two"),
        entry("notes.md", b"n"),
    ];
    let tree = object::put_tree(&*site.db, site.pid, &list).unwrap();

    checkout::check_case_collisions(&*site.db, site.pid, &tree, "", false)
        .expect("区分大小写的盘上这是两个合法文件，不该拦");
    let err = checkout::check_case_collisions(&*site.db, site.pid, &tree, "", true)
        .expect_err("不敏感的目标盘上这对名字会落到同一个格子");
    match err {
        Error::CaseCollision { dir, a, b } => {
            assert_eq!(dir, ".", "根层的显示名该是 `.`，不是空串");
            assert_eq!((a.as_str(), b.as_str()), ("README.md", "Readme.md"));
        }
        other => panic!("报错类型不对：{other}"),
    }

    // 子树作用域：报错要说清是**哪一层**碰撞，否则用户不知道该动哪个目录。
    let dir_entry = object::TreeEntry {
        name: "docs".to_string(),
        kind: Kind::Dir,
        hash: tree,
        size: 0,
        mode: 0o40755,
    };
    let root = object::put_tree(&*site.db, site.pid, &[dir_entry]).unwrap();
    let e = checkout::check_case_collisions(&*site.db, site.pid, &root, "", true)
        .expect_err("子树里的碰撞也要被 walk_levels 走到");
    assert!(
        format!("{e}").contains("docs"),
        "报错没带目录名：{e}"
    );
}

/// 折叠比"仅大小写"严一点：NFC 的 `é` 与 NFD 的 `é` 也算同一个名字。
/// NTFS 上它们其实是两个文件，但 APFS 会把两种写法当成同一个 ⇒ 宁可多拒一次
/// （拒绝可恢复），不静默吃掉文件（不可恢复）。
/// **身份那一半没归一化**（`case_only_differing_names_stay_distinct` 钉着），
/// 那一半要连带存原始磁盘名 ⇒ §14 Q13。
#[test]
fn fold_also_collapses_normalization_forms() {
    let nfc = "\u{e9}.md"; // 单个码点 é
    let nfd = "e\u{301}.md"; // e + 组合音标
    assert_ne!(nfc, nfd, "两串字节本来就不同，测的就是折叠把它收成同一个");
    let list = vec![entry(nfc, b"a"), entry(nfd, b"b")];
    assert_eq!(
        object::fold_collision(&list),
        Some((nfc.to_string(), nfd.to_string()))
    );
}

/// 探针必须**自己收尾**：它是唯一落在用户工作树里的临时文件，留下残骸就等于一个只读命令
/// 污染了项目内容。同时扫描器永久排除这个名字前缀，防"create 与 remove 之间被杀"。
#[test]
fn case_probe_is_self_cleaning_and_excluded() {
    let dir = tempfile::tempdir().unwrap();
    let _ = twosync::utils::dest_case_insensitive(dir.path());
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        0,
        "探针文件没删干净"
    );

    let site = Site::new(&[("keep.txt", b"keep")]);
    fs::write(site.proj().join(".tsprobe-a1234.5"), b"x").unwrap();
    worktree::scan(&*site.db, site.pid, &site.proj(), false, 100).unwrap();
    let tx = site.db.begin();
    let keys: Vec<String> = tx
        .prefix_scan(&keys::project_prefix(keys::TAG_WORKTREE, site.pid))
        .into_iter()
        .map(|r| String::from_utf8_lossy(&r.key).to_string())
        .collect();
    tx.commit();
    assert_eq!(
        keys.len(),
        1,
        "只该有 keep.txt 一行：{keys:?}"
    );
    assert!(keys[0].ends_with("keep.txt"), "{keys:?}");
}

/// 端到端：**不区分大小写的目标盘**上导出这一对名字必须被拒。
///
/// 挂 `#[cfg(windows)]` 是如实标注而不是想办法凑条件：NTFS 给探针的答案是"不敏感"，
/// Linux 的 tempdir 是"敏感"，那条门在那边本来就不该拦（`case_gate_refuses_only_when_the_target_folds`
/// 已经用手工传参把 `true` 与 `false` 两侧都测过了）。这条测的是**接线**：
/// 命令真的会去探针、真的在写任何东西之前就停下。
///
/// 树是手工 `put_tree` + `put_commit` 造的——盘上根本建不出这对名字，绕开磁盘才测得动。
#[cfg(windows)]
#[test]
fn export_refuses_case_twins_on_an_insensitive_destination() {
    let site = Site::new(&[("keep.txt", b"keep")]);
    let list = vec![entry("NOTE.md", b"one"), entry("Note.md", b"two")];
    let tree = object::put_tree(&*site.db, site.pid, &list).unwrap();
    let target = object::put_commit(
        &*site.db,
        site.pid,
        &object::Commit {
            parents: vec![],
            tree,
            peer: "t".to_string(),
            ts: 9,
            msg: "手工造的树".to_string(),
            stats: object::Stats {
                add: 2,
                modify: 0,
                delete: 0,
                rename: 0,
                bytes: 0,
            },
        },
    )
    .unwrap();

    let dest = site.root().join("out");
    let store = site.store();
    let err = checkout::export(
        &*site.db,
        site.pid,
        &store,
        &ExportOpts {
            target: &target,
            prefix: None,
            dest: &dest,
            work_root: &site.proj(),
            force: false,
            apply: true,
        },
    )
    .expect_err("NTFS 上 NOTE.md 与 Note.md 是同一个格子，必须拒");
    let msg = err.to_string();
    assert!(msg.contains("大小写碰撞"), "报错不对：{msg}");
    assert!(msg.contains("NOTE.md") && msg.contains("Note.md"), "没报出是哪两个名字：{msg}");
    // 停下必须发生在**写任何东西之前**：被拒之后目标里不该有一半的导出物。
    let left = fs::read_dir(&dest).map(|mut d| d.count()).unwrap_or(0);
    assert_eq!(left, 0, "被拒却留下了 {left} 个文件");
}
