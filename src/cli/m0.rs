//! M0 批次：本地、不联网。
//!
//! init project add/list/use/remove/status scan commit mv status log diff show
//! ls-files restore checkout plan apply config doctor

use std::fs;

use storage_engines::EngineKind;

use crate::cli::Cli;
use crate::cli::cmd::ProjectCmd;
use crate::cli::registry::Registry;
use crate::error::Result;
use crate::store::keys;
use crate::utils::{
    db_dir, journal_dir, log_dir, objects_dir, open_store, projects_file, site_dir,
};

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
pub fn doctor(cli: &Cli, engine: EngineKind) -> Result<()> {
    let db = open_store(&cli.root, engine)?;
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
