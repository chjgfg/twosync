//! §10.8 引擎矩阵：同一套验证换引擎跑。
//!
//! 这一条存在的理由不是"多测几个引擎"这么轻：整个项目的卖点之一是"存储基座可以换"，
//! 而**所有既有用例都写死 `EngineKind::LsmTree`**（67 处）。也就是说"换引擎还能跑"这句话
//! 从来没有被跑过。这里分两层，因为两层的失败模式完全不同：
//!
//! ① **画像层**（FR-21）：`check_durability_gate` 现问 `durability_profile()`，三要件缺一项就拒。
//!    这一层要的是"哪个引擎根本不该被打开、缺口说清楚了没有"。
//! ② **场景层**：凡是过了画像那道的引擎，跑**同一条**本地流程，断言**计数相同**。
//!    只用计数（条目数 / 变更数 / 块数 / 字节数），不用墙钟 ——
//!    这台机器 run 间摆 ±2×，而结构不对就是真不对。
//!
//! 手工验证（换引擎跑同一条命令，人也能看一遍）：
//!
//! ```text
//! for E in lsm_tree bplus_tree bitcask memory; do
//!   rm -rf /tmp/tsm && cargo run -q --release -- --root /tmp/tsm --engine $E init
//!   echo x > /tmp/tsm/p/a.txt
//!   cargo run -q --release -- --root /tmp/tsm --engine $E project add p
//!   cargo run -q --release -- --root /tmp/tsm --engine $E scan
//!   cargo run -q --release -- --root /tmp/tsm --engine $E commit -m v1
//! done
//! ```
//! 后两个应当停在 FR-21 那句"未通过启动自检，缺口：…"上。

mod common;

use std::fs;
use std::path::Path;

use storage_engines::KvEngine;
use twosync::error::Error;
use twosync::store::build::{self, MAIN};
use twosync::store::cas::Cas;
use twosync::store::checkout::{self, ExportOpts};
use twosync::store::keys::{self, Hash, ProjectId};
use twosync::store::{gc, history, object, worktree};
use twosync::utils::{check_durability_gate, db_dir, dir_size, objects_dir, open_store};

const BLOCK: u64 = 4096;


/// 画像不合格的那两种在这里不是失败，是**预期**：先探一次，探不过就整段跳过。
/// 除此之外的错误（打不开、路径不对）一律照红，不许借"这个引擎不支持"蒙过去。
fn open_if_admitted(root: &Path, engine: &str) -> Option<Box<dyn KvEngine>> {
    match open_store(root, common::by_name(engine)) {
        Ok(db) => Some(db),
        Err(e @ Error::DurabilityGate { .. }) => {
            println!("{engine} 过不了 FR-21 那道闸 ⇒ 场景层跳过：{e}");
            None
        }
        Err(e) => panic!("{engine} 开库失败（不是画像问题）：{e}"),
    }
}

/// ① 画像层：四个引擎各自的 `durability_profile()` 与闸门结论。
///
/// 这张表是**跑出来的**，不是照文档抄的（FR-21 的理由就挂在这句上：文档会过期，画像不会）。
/// 这里**绕开** `open_store` 直接 `StorageConfig::open()`：`open_store` 自己就带着那道闸，
/// 用它就只能看到"被拒"，看不到"缺哪一项"（第一个版本就是这么写的，结果 bitcask 直接在
/// 开库那一步红了，报的还是闸的原话 —— 用例没跑成，而画像的读数一个都没拿到）。
/// 断言写成"至少一个过、至少一个被拒、缺口逐项指名"，而不是钉死哪两个过 ——
/// 三个 bool 是引擎的内部事实，会随版本变；而"没通过自检就不许用"是**我们的**契约。
#[test]
fn only_the_engines_with_a_full_durability_profile_can_be_opened() {
    let mut passed: Vec<String> = Vec::new();
    let mut refused: Vec<(String, String)> = Vec::new();
    for name in common::ALL_ENGINES {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".twosync")).unwrap();
        let db = storage_engines::StorageConfig {
            kind: common::by_name(name),
            data_dir: db_dir(tmp.path()),
            engine: storage_engines::EngineConfig::default(),
        }
        .open()
        .unwrap_or_else(|e| panic!("{name} 连开都开不起来：{e}"));
        let p = db.durability_profile();
        match check_durability_gate(&*db) {
            Ok(()) => passed.push(format!(
                "{name}(wal={},fsync={},sum={})",
                p.transaction_wal, p.fsync_on_commit, p.data_checksum
            )),
            Err(e) => refused.push((name.to_string(), e.to_string())),
        }
    }
    println!("过闸的引擎  = {passed:?}");
    for (n, why) in &refused {
        println!("拒开的引擎  = {n}: {why}");
    }

    assert!(
        passed.iter().any(|s| s.starts_with("lsm_tree")),
        "默认引擎过不了闸，整个基座就有问题了"
    );
    assert!(
        !refused.is_empty(),
        "四个引擎全过闸 = 画像在说谎（memory 引擎不可能有事务 WAL）"
    );
    // 缺口必须**逐项列出来**：只说"不合格"的那句报错等于没说。
    for (n, why) in &refused {
        assert!(
            why.contains("无事务 WAL")
                || why.contains("commit 不 fsync")
                || why.contains("数据记录无校验和"),
            "{n} 的拒绝要指名缺哪一项，实际：{why}"
        );
        assert!(why.contains(n), "报错里必须带引擎名：{why}");
    }
}

/// ② 场景层：同一条本地流程在**每一个能打开的引擎**上跑，计数必须一模一样。
///
/// 覆盖六件事：扫描盖章、提交建树、改名按 FR-1 记成一笔（不是删+增）、`restore` 从任意历史版本
/// 取回**字节**、`checkout --to` 导出到工作树外、`gc --sweep --apply` 的 mark-sweep 两趟。
/// 会话层（push/pull）不在这里：它拿的是 `&dyn KvEngine`，方向与引擎无关由构造保证。
#[test]
fn one_local_flow_produces_identical_counts_on_every_openable_engine() {
    let mut per_engine: Vec<(String, Counts)> = Vec::new();
    for name in common::ALL_ENGINES {
        let Some(c) = flow(name) else { continue };
        println!("场景层 {name} = {c:?}");
        per_engine.push((name.to_string(), c));
    }
    assert!(
        per_engine.len() >= 2,
        "至少要有两个引擎跑完同一条流程，否则'换引擎'这句话仍然没被验证过：{per_engine:?}"
    );
    let base = &per_engine[0];
    for (name, c) in &per_engine[1..] {
        assert_eq!(
            c, &base.1,
            "{name} 与 {} 在同一套动作下计数不同 ⇒ 有一处依赖了那个引擎的偶然行为",
            base.0
        );
    }
}

/// 计数：**结构量**，不是时间量。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Counts {
    /// 第一轮扫描看见的条目数（含目录行）
    walked: usize,
    /// 稳态第二轮：一个 hash 都不该重算
    rehashed_when_unchanged: usize,
    /// 第二版的变更四格。`rename` 必须是 1 而 `add`/`delete` 各 0 —— 那是 FR-1 的判据
    add: u32,
    modify: u32,
    delete: u32,
    rename: u32,
    /// 提交之后 head 的个数
    heads: usize,
    /// CAS 里块的**文件**个数。这套语料下是 6，而且这个数必须能解释清楚：
    /// `d/b.bin` 是 20 000 个相同的 `7` ⇒ 它的四个满块**内容一模一样** ⇒ 内容寻址去重成 1 个文件，
    /// 加上 3616 B 的末块 = 2 个文件；再加 `a.txt` 两版(18 B / 27 B)、`c.txt`(1 B)、`late.txt`(9 B) = 6。
    /// （第一次跑到这个数时对不上账，专门打了一遍块文件名才确认是去重而不是丢块 —— 注释留在这儿
    /// 是为了下一次不用再打。）
    blocks: u64,
    /// 从第一版取回那个文件的字节数（历史必须真能取回字节，不只是 hash）
    restored_bytes: u64,
    /// `checkout --to` 导出到工作树外的文件个数
    exported_files: usize,
    /// mark-sweep 之后仍然活着的 commit 个数（两版都该活着：head + 亲代）
    commits_after_gc: usize,
    /// 真被 `unlink` 掉的块文件个数。**这套流程里恒为 0**（历史还可达 ⇒ gc 不许删），
    /// 留这一格是为了让"什么都不许删"这件事在跨引擎对比里也可见。
    unlinked: usize,
    /// 活事务必须归零（还非 0 就是漏了 commit/drop）
    active_txns: u64,
}

fn flow(engine: &str) -> Option<Counts> {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    fs::create_dir_all(root.join(".twosync")).unwrap();
    let work = root.join("p");
    fs::create_dir_all(work.join("d")).unwrap();
    fs::write(work.join("a.txt"), b"first version of a").unwrap();
    fs::write(work.join("d/b.bin"), vec![7u8; 20_000]).unwrap();
    fs::write(work.join("d/c.txt"), b"c").unwrap();
    let mut reg = twosync::cli::registry::Registry::default();
    reg.add("p", "p").unwrap();
    reg.save(&root).unwrap();

    let db = open_if_admitted(&root, engine)?;
    let pid: ProjectId = keys::project_id("p");
    let store = Cas::new(&objects_dir(&root), BLOCK).unwrap();

    // ── 第一轮：全量扫描 + 提交 ──
    let first_scan = worktree::scan(&*db, pid, &work, false, 50).unwrap();
    let v1 = commit(&*db, pid, &store, &work, "v1").expect("第一轮必然有改动");
    let heads_after_first = build::ref_heads(&*db, pid, MAIN).unwrap().len();
    assert_eq!(heads_after_first, 1, "一次提交 = 一个 head");

    // 稳态第二轮：什么都不动 ⇒ 一个 hash 都不该重算（这条在引擎之间最容易露馅：
    // 粗筛读的是上一行的 size/mtime，某个引擎把值存歪了就会每轮全重算）。
    let steady = worktree::scan(&*db, pid, &work, false, 50).unwrap();

    // ── 第三轮：增一条、改一条、删一条、**改名**一条 ──
    fs::write(work.join("late.txt"), b"brand new").unwrap();
    fs::write(work.join("a.txt"), b"second version of a, longer").unwrap();
    fs::remove_file(work.join("d/c.txt")).unwrap();
    fs::rename(work.join("d/b.bin"), work.join("d/renamed.bin")).unwrap();
    worktree::scan(&*db, pid, &work, false, 50).unwrap();
    let built = build::build(
        &*db,
        pid,
        &store,
        &work,
        build::Meta {
            peer: "matrix",
            msg: "v2",
            ts: 1,
            allow_mass_delete: true,
        },
    )
    .unwrap();
    let v2 = built.commit.expect("有改动就该产生版本");
    let ch = &built.changes;

    // head 集合在会话之前恒为 1，而且指的必须就是刚提交的那一个（引擎之间不该有差别）
    let heads_now = build::ref_heads(&*db, pid, MAIN).unwrap();
    assert_eq!(heads_now, vec![v2], "提交之后 head 必须指向新版本");
    let heads = heads_now.len();

    // ── 历史必须真能取回**字节**：从 v1 那一份里把 a.txt 读出来 ──
    let c1 = object::read_commit(&*db, pid, &v1).unwrap();
    let e = history::resolve(&*db, pid, &c1.tree, "a.txt")
        .unwrap()
        .expect("v1 里有 a.txt");
    let blob = build::read_blob(&*db, pid, &e.hash).unwrap();
    let back = root.join("check-a.txt");
    store.read_blob(&blob, &back).unwrap();
    let restored_bytes = fs::read(&back).unwrap().len() as u64;

    // ── `checkout --to`：导出到工作树**外**（目标在工作树里必须被拒，§17 补记十一）──
    let out_dir = root.join("out");
    let exp = checkout::export(
        &*db,
        pid,
        &store,
        &ExportOpts {
            target: &v1,
            prefix: None,
            dest: &out_dir,
            work_root: &work,
            force: false,
            apply: true,
        },
    )
    .unwrap();
    assert!(out_dir.join("d").join("b.bin").is_file(), "导出的大文件要在");

    // ── 回收：mark-sweep 两趟（§9.3）。**这里断言的是"什么都不许删"**：
    //    c.txt 虽然从工作树没了，但 v1 仍可达、仍引用那份 blob ⇒ 块必须留着（不变量 (d)）。
    //    这正是 2026-10-20 那条 daemon-gc 用例栽过的同一个坑，这次**在写文档之前**就被用例拦下：
    //    gc 无权删历史，它只能收"没有版本引用的块"和"引擎里的旧版本行"。
    let plan = gc::sweep(&*db, &store, pid, false).unwrap();
    let unlinked = gc::unlink_pending(&*db, &store, pid, usize::MAX, false)
        .unwrap()
        .files;
    assert_eq!(
        plan.commits_kept, 2,
        "两版都可达（head + 亲代），一个都不许判成垃圾：{plan:?}"
    );
    assert_eq!(
        (plan.blocks_pending, plan.commits_dropped, unlinked),
        (0, 0, 0),
        "历史仍可达的块不许进 pending、不许删文件：{plan:?}"
    );

    let commits = history::walk(&*db, pid, &build::ref_heads(&*db, pid, MAIN).unwrap(), 50)
        .unwrap()
        .len();
    let health = db.health();
    let out = Counts {
        walked: first_scan.walked,
        rehashed_when_unchanged: steady.hashed,
        add: ch.add,
        modify: ch.modify,
        delete: ch.delete,
        rename: ch.rename,
        heads,
        blocks: dir_size(&objects_dir(&root)).files,
        restored_bytes,
        exported_files: exp.files,
        commits_after_gc: commits,
        unlinked,
        active_txns: health.active_txns,
    };
    Some(out)
}

fn commit(
    db: &dyn KvEngine,
    pid: ProjectId,
    store: &Cas,
    work: &Path,
    msg: &str,
) -> Option<Hash> {
    build::build(
        db,
        pid,
        store,
        work,
        build::Meta {
            peer: "matrix",
            msg,
            ts: 1,
            allow_mass_delete: true,
        },
    )
    .unwrap()
    .commit
}
