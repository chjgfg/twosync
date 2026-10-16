//! M0 批次：本地、不联网。
//!
//! init project add/list/use/remove/status scan commit mv status log diff show
//! ls-files restore checkout plan apply config doctor

use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use crate::allocstats;
use crate::cli::Cli;
use crate::cli::cmd::ProjectCmd;
use crate::cli::registry::Registry;
use crate::error::{Error, Result};
use crate::store::{build, cas, keys, worktree};
use crate::utils::{
    db_dir, journal_dir, log_dir, objects_dir, open_store, projects_file, site_dir,
};

/// `Commit.peer` 的占位值。真正的 peer id 要等 M1 的 `peer add|trust`；现在填什么都一样——
/// 它**只是元数据，不参与任何裁决**（§9.5.2 / §15.7），所以不值得为它提前做一套身份。
const PEER_LOCAL: &str = "local";

/// 建站点目录布局。幂等：`create_dir_all` 对已存在的目录直接返回 Ok，
/// 所以不需要"先 exists 再建"（那样反而是 TOCTOU）。
pub fn init(cli: &Cli) -> Result<()> {
    let root = &cli.root;
    for dir in [
        site_dir(root),
        objects_dir(root),
        db_dir(root),
        journal_dir(root),
        log_dir(root),
    ] {
        let fresh = !dir.exists();
        fs::create_dir_all(&dir)?;
        println!(
            "{} {}",
            if fresh { "创建" } else { "已存在" },
            dir.display()
        );
    }

    let projects = projects_file(root);
    if projects.exists() {
        println!("已存在 {}", projects.display());
    } else {
        fs::write(&projects, "# twosync 项目注册表（§16.1）\n")?;
        println!("写入 {}", projects.display());
    }
    Ok(())
}

/// 打开基座 → FR-21 自检 → 打印健康度。句柄用完即弃，不跨命令持有（§9.6.2）。
pub fn doctor(cli: &Cli) -> Result<()> {
    let db = open_store(&cli.root, cli.engine)?;
    // NFR-8：`health()` 是 O(物理行数)，只允许出现在这种人工触发的诊断路径上。
    let h = db.health();
    println!("engine      = {}", db.name());
    println!(
        "db          = {}",
        db.path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<内存>".to_string())
    );
    println!("rows        = {}", h.total_rows);
    println!("live_keys   = {}", h.live_keys);
    println!("max_chain   = {}", h.max_chain);
    println!("rows_above  = {}", h.rows_above_one);
    println!("active_txns = {}", h.active_txns);
    Ok(())
}

/// `project add|list|use`。注册表是所有键的前置：`project_id` 由名字派生。
pub fn project(cli: &Cli, cmd: &ProjectCmd) -> Result<()> {
    let mut reg = Registry::load(&cli.root)?;
    match cmd {
        ProjectCmd::Add { name, path } => {
            // 存成 `/` 分隔的相对路径（§8）；不给 --path 就按 root/<name> 约定放。
            let rel = match path {
                Some(p) => p.to_string_lossy().replace('\\', "/"),
                None => name.clone(),
            };
            reg.add(name, &rel)?;
            reg.save(&cli.root)?;
            println!(
                "已注册 {name} → {rel}  (id={:#010x})",
                keys::project_id(name)
            );
        }
        ProjectCmd::List => {
            if reg.projects.is_empty() {
                println!("（还没有项目：twosync project add <name>）");
            }
            for (name, p) in &reg.projects {
                let mark = if reg.current.as_deref() == Some(name.as_str()) {
                    "*"
                } else {
                    " "
                };
                println!(
                    "{mark} {name:20} id={:#010x}  {}",
                    keys::project_id(name),
                    p.path
                );
            }
        }
        ProjectCmd::Use { name } => {
            reg.select(name)?;
            reg.save(&cli.root)?;
            println!("当前项目 = {name}");
        }
    }
    Ok(())
}

/// 项目作用域：`--project` 显式指定 > `project use` 选中的那个。
/// 返回 (项目名, 项目绝对路径)。名字要单独拿出去，因为 `project_id` 是**从名字派生**的。
fn resolve_project(cli: &Cli) -> Result<(String, PathBuf)> {
    let reg = Registry::load(&cli.root)?;
    let project = match &cli.project {
        Some(name) => reg
            .projects
            .get(name)
            .ok_or_else(|| Error::Registry(format!("没有项目 {name}")))?,
        None => reg.current()?,
    };
    let root = cli.root.join(&project.path);
    if !root.is_dir() {
        return Err(Error::Registry(format!(
            "项目目录不存在：{}",
            root.display()
        )));
    }
    Ok((project.name.clone(), root))
}

pub fn scan(cli: &Cli, force: bool, batch: usize) -> Result<()> {
    let (name, root) = resolve_project(cli)?;
    let db = open_store(&cli.root, cli.engine)?;
    let pid = keys::project_id(&name);
    let wm = allocstats::watermark();
    let rep = worktree::scan(&*db, pid, &root, force, batch)?;
    let peak = wm.peak_growth();

    let secs = rep.elapsed_ms as f64 / 1000.0;
    println!("project     = {name}  (id={pid:#010x})");
    println!("root        = {}", root.display());
    println!("walked      = {}", rep.walked);
    println!(
        "hashed      = {}  (skipped {} / deferred {})",
        rep.hashed, rep.skipped, rep.deferred
    );
    println!(
        "bytes       = {:.1} MiB  (本轮重算 {:.1} MiB)",
        rep.bytes as f64 / 1048576.0,
        rep.hashed_bytes as f64 / 1048576.0
    );
    println!("elapsed     = {secs:.3} s");
    // 吞吐只按**实际 hash 的字节**算：拿 rep.bytes 当分子会把 skipped 的字节也算进去，
    // 于是"全都跳过了"反而显示成 0 MiB/s。
    println!(
        "throughput  = {:.1} MiB/s",
        rep.hashed_bytes as f64 / 1048576.0 / secs.max(0.001)
    );
    // §9.6.1 的量尺：常数内存 ⇒ 这个数与文件数、最大文件都无关，只与 batch/HASH_BUF 有关。
    println!("peak heap   = {:.1} MiB", peak as f64 / 1048576.0);
    // 第二轮本该 hashed=0；不为 0 时得能问出"是哪一个"。样本本身有界（≤10 条）。
    if rep.hashed > 0 && rep.hashed <= rep.sample.len() {
        println!("重算的是：{}", rep.sample.join(", "));
    }
    if rep.deferred > 0 {
        println!(
            "注意：{n} 项读不了，已 deferred（不是删除）",
            n = rep.deferred
        );
    }
    Ok(())
}

/// 把 `W` 表的**最新一轮**快照固化成 commit（FR-4）：落块 → 建树 → 比对 → 移 head。
///
/// 这一条**不重新扫描**，只吃上一次 `scan` 的结果——所以刚改完文件没 scan 就 commit，
/// 提交的是上一次扫描看见的状态。中间文件又变了的话会撞 `StaleScan`，不会静默用新内容。
pub fn commit(cli: &Cli, msg: Option<&str>, allow_mass_delete: bool) -> Result<()> {
    let (name, root) = resolve_project(cli)?;
    let db = open_store(&cli.root, cli.engine)?;
    let pid = keys::project_id(&name);
    // 块仓是**站点级**的一份（挂在 `--root` 下），不是每项目一份：跨项目去重靠这个。
    let store = cas::Cas::new(&objects_dir(&cli.root), cas::DEFAULT_BLOCK)?;

    let started = Instant::now();
    let wm = allocstats::watermark();
    let b = build::build(
        &*db,
        pid,
        &store,
        &root,
        build::Meta {
            peer: PEER_LOCAL,
            msg: msg.unwrap_or("（未填写说明）"),
            ts: worktree::now_ns(),
            allow_mass_delete,
        },
    )?;
    let peak = wm.peak_growth();

    println!("project     = {name}  (id={pid:#010x})");
    println!("root        = {}", root.display());
    match b.commit {
        Some(h) => println!("commit      = {}", keys::hex(&h)),
        None => println!("commit      = （无改动：不产生空提交，head 不动）"),
    }
    println!("tree        = {}", keys::hex(&b.tree));
    let c = &b.changes;
    println!(
        "changes     = +{} ~{} -{}   入 {:.1} MiB / 删 {:.1} MiB",
        c.add,
        c.modify,
        c.delete,
        c.bytes as f64 / 1048576.0,
        c.deleted_bytes as f64 / 1048576.0
    );
    println!(
        "cas         = blob {} 新，块 {} 新写入 {:.1} MiB",
        b.blobs_new,
        b.blocks_stored,
        b.block_bytes as f64 / 1048576.0
    );
    println!("entries     = {}  (库里 W 行 {})", b.entries, b.known);
    if b.entries == 0 && b.known > 0 {
        println!(
            "注意：本轮快照一条都没有而库里有 {n} 行 ⇒ 上一次 scan 可能中断过，先重跑 scan",
            n = b.known
        );
    }
    println!("elapsed     = {:.3} s", started.elapsed().as_secs_f64());
    // 与 scan 同一把尺：这一条要物化条目表（O(文件数)），所以它**不是**常数内存，
    // 上限由 MAX_ENTRIES 显式兜着。这里打印是为了让那次权衡可见。
    println!("peak heap   = {:.1} MiB", peak as f64 / 1048576.0);
    Ok(())
}
