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

    /// 站点级配置（`<root>/.twosync/config.toml`）
    #[command(subcommand)]
    Config(ConfigCmd),

    /// 改个工作树里的路径名（文件或目录都行）
    ///
    /// 它只做"安全地 rename"这一件事：源不存在就拒、目标已存在就拒、父目录自动建。
    /// **本轮不会记成改名**：`stats.rename` 恒 0，改名检测是 FR-17 的活，现在扫出来是
    /// 一次删除 + 一次新增。块是内容寻址的，所以磁盘与传输都不受影响，只有账面上难看。
    Mv {
        /// 仓库内原路径（`/` 分隔）
        src: String,
        /// 仓库内新路径；父目录不存在会自动建
        dst: String,
    },

    /// 工作树（按最后一次扫描）与 HEAD 差在哪。**只读，一个字节都不写**
    ///
    /// 它不看文件内容，只看 `W` 与上一版树的差 —— 所以"扫描之后又改了"的那部分
    /// 不在这里（要先 `scan`）。这一条与 git 的 `status` 语义相同，代价也相同。
    Status,

    /// 工作树与任意版本的差异，列出具体路径
    Diff {
        /// 和谁比：`HEAD` / ref 名 / commit hash 前缀
        #[arg(long, default_value = "HEAD")]
        at: String,
    },

    /// 列出一个版本里的全部路径（流式打印，不物化）
    LsFiles {
        /// 列哪个版本
        #[arg(long, default_value = "HEAD")]
        at: String,
        /// 只列这个目录下面的
        #[arg(long)]
        prefix: Option<String>,
    },

    /// 把一个版本落到工作树，或（`--to`）导出到别的目录。**默认只报不动手。**
    ///
    /// 落到工作树这一条是唯一会批量删工作树文件的命令，所以闸的顺序固定：先只报，再过
    /// FR-10 熔断，真执行时每个覆盖/删除/盖到已有路径上动作再各过一遍 §7.3。它**不动 head**
    /// （v1 没有分支）。`--to` 是另一件事：不比对工作树、不删、不动 head，因此那两道门在这里
    /// 没有对象，改成一条新闸——**目标不许在工作树里**（导出的副本会被下一轮 scan 当成项目内容）。
    Checkout {
        /// 落到哪个版本
        #[arg(long, default_value = "HEAD")]
        at: String,
        /// 只动这一棵子树；单文件请用 `restore`
        #[arg(long)]
        prefix: Option<String>,
        /// 真的执行。省略时只打印要做什么
        #[arg(long)]
        apply: bool,
        /// 越过 FR-10 删除熔断
        #[arg(long)]
        allow_mass_delete: bool,
        /// 导出到另一个目录：不碰工作树、不动 head（目标不许在工作树里）
        #[arg(long)]
        to: Option<PathBuf>,
        /// 配合 --to：覆盖目标上已存在的路径
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

    /// 摘掉一个项目。**只动注册表**：工作树、历史、CAS 块一概不碰（§16.1）
    ///
    /// `project_id` 由名字派生，所以再 `add` 同名会接上同一份历史——摘掉不是"忘掉"。
    Remove {
        name: String,
        /// 连带回收 CAS 空间。跨 project 的 refcnt 回收是 M1 `gc` 的活，现在会明确拒绝。
        #[arg(long)]
        purge: bool,
    },

    /// 项目体检：完整扫描过没有、head 在哪、库里多少行
    ///
    /// 每一项都要开引擎数一遍键前缀（`key_count` 是 O(匹配行数)），所以这条和 `doctor`
    /// 一样只适合人工触发，不许进任何自动循环（NFR-8）。
    Status {
        /// 省略 = 全部项目的概览
        name: Option<String>,
    },
}

/// `config` 的三个动作。键清单在 `config::Config::KEYS`，那里同时是校验的唯一入口。
#[derive(Subcommand, Clone)]
pub enum ConfigCmd {
    /// 列出生效值与来源；不带 KEY 就列全部
    Get { key: Option<String> },
    /// 写一个键。值会先校验，未知键直接拒——**存进去一个没人读的键比报错难查得多**
    Set { key: String, value: String },
    /// 删掉一个键，回到内置默认
    Unset { key: String },
}
