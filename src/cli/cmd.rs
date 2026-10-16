use std::path::PathBuf;

use clap::Subcommand;
use storage_engines::EngineKind;

/// 把"未知引擎"这种输入错误交给 clap：每个 handler 就少一个 else 分支，退出码也统一。
pub fn engine_parser(s: &str) -> Result<EngineKind, String> {
    EngineKind::parse(s)
        .ok_or_else(|| format!("未知引擎 {s:?}；可用：lsm_tree / bplus_tree / bitcask / memory"))
}

/// 全部子命令的清单。加一条命令 = 这里加一个变体 + 对应批次文件加一个函数 + `main` 加一个分支。
///
/// 只加**已经能实现**的变体：变体进了枚举，`main` 的 match 就必须接得住，别提前铺空壳。
#[derive(Subcommand, Clone)]
pub enum Cmd {
    // ── M0 ─────────────────────────────────────────────────────────
    /// 建站点目录布局（幂等，§13）
    Init,
    /// 打开存储基座，跑 FR-21 启动自检并打印引擎健康度
    ///
    /// 引擎由全局 `--engine` 决定（`cli/mod.rs`），不在这里重复声明：全局参数会传播进
    /// 子命令，同名再来一次就是自己跟自己撞。
    Doctor,

    /// 项目注册表（§16.1）
    #[command(subcommand)]
    Project(ProjectCmd),

    /// 扫描工作树，把每个路径的当前状态写进 `W` 前缀（FR-2）
    Scan {
        /// 忽略 size+mtime 粗筛，全部重算 hash
        #[arg(long)]
        force: bool,
        /// 一个事务写多少条（§9.6.2：事务不许跨全树）
        #[arg(long, default_value_t = 2000)]
        batch: usize,
    },
    // ── M1 ── push / agent / gc / peer add|list|trust
    // ── M2 ── daemon / watch
    // ── M3 ── pull / sync
    // ── M4 ── branch / tag / reset / revert / stash / cherry-pick / blame / bisect
}

#[derive(Subcommand, Clone)]
pub enum ProjectCmd {
    /// 注册一个项目；省略 --path 时用 root/<name>
    Add {
        name: String,
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// 列出全部项目，`*` 标当前
    List,
    /// 切换后续命令默认作用的项目
    Use { name: String },
}
