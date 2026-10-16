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

    /// 把上一次 `scan` 的结果固化成一个 commit：落块 → 建树 → 比对 → 移 head（FR-4）
    ///
    /// 吃的是 `W` 表，**不重新扫描**：`scan` 与 `commit` 分开是刻意的，扫描负责"看见"，
    /// commit 负责"成为历史"，中间那段时间工作树又变了的话会撞 `StaleScan` 而不是静默入库。
    Commit {
        /// 提交说明；省略时写占位串（与 git 一样不拦空说明，但会照样显示出来）
        #[arg(short, long)]
        msg: Option<String>,
        /// 越过 FR-10 删除熔断（删除条目 > 已知的 20% 且 > 20 条，或 > 2 GiB）
        #[arg(long)]
        allow_mass_delete: bool,
    },

    /// 列出 head 可达的 commit，时间倒序（FR-4 的历史视图）
    Log {
        /// 最多列几条；0 = 全部。内存是 O(历史长度)，别在十万历史上用 0
        #[arg(short = 'n', long, default_value_t = 20)]
        max_count: usize,
    },

    /// 看一个版本：commit 本身、某一层的清单，或一个文件的块清单
    ///
    /// 只读，**不吐文件字节**（10 GB 文件会把终端淹掉）；要字节得等 `restore`。
    Show {
        /// `HEAD` / ref 名 / commit hash 的十六进制前缀（长度须为偶数）
        #[arg(default_value = "HEAD")]
        rev: String,
        /// 仓库内路径（`/` 分隔）；省略 = 提交本身 + 根目录一层
        path: Option<String>,
    },

    /// 把历史里的一个路径写回工作树（M0 验收项「`restore --at` 取回任意历史版本」）
    ///
    /// 默认**不覆盖**已存在的本地文件；加 `--force` 也仍然过 §7.3 铁律：被覆盖的那一版
    /// 必须已经在历史里（有 `B` 清单且块都在磁盘上），否则拒绝并让你先 commit。
    Restore {
        /// 从哪个版本取：`HEAD` / ref 名 / commit hash 前缀
        #[arg(long, default_value = "HEAD")]
        at: String,
        /// 仓库内路径（`/` 分隔）
        path: String,
        /// 写到别处而不是工作树里（不碰本地文件，因此也不需要 §7.3 的门）
        #[arg(long)]
        to: Option<PathBuf>,
        /// 允许覆盖工作树里已存在的文件
        #[arg(long)]
        force: bool,
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
