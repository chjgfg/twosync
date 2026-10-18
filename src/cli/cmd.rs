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

    /// 版本与**生效**配置（引擎、block_size、站点根、二进制大小）
    ///
    /// 只报运行时确证得了的东西：依赖版本以 `Cargo.lock` 为准，这里不硬编一个会过期的数字。
    Version,

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
        /// 落块**不等 fsync**（§14 Q11）。10 GiB 首建库的 84 分钟几乎全是等 fsync（§11.1）。
        /// 放弃的不是写序（仍是 `tmp → rename`），而是"清单落盘前块已持久"这一半：
        /// 断电可能留下与清单不符的块，读回时按 hash 必报错，恢复办法是重新导入那一版。
        /// 用不用会在输出里打印出来，不许静默生效。
        #[arg(long)]
        fast_import: bool,
        /// 写块仓的平均速率上限，单位 MiB/s（0 = 不限速）。NFR-2 明写首建库"必须可限速"，
        /// NFR-3 要的是别把同机策略进程的延迟拖走
        #[arg(long, default_value_t = 0.0)]
        max_mbps: f64,
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
    /// 改名会记进历史（FR-1）：`mv` 之后 `scan` + `commit`，同 blob_hash 的"一删一增"被
    /// `ChangeSet::fold_renames()` 折成 `rename`，`stats.rename` 就是它的条数；整批改名也
    /// 不会被 FR-10 当成"删了一半仓库"（折叠在熔断之前）。配对窗口有上限
    /// （`build::RENAME_PAIR_MAX`），超了就放弃检测并打印一句为什么。
    Mv {
        /// 仓库内原路径（`/` 分隔）
        src: String,
        /// 仓库内新路径；父目录不存在会自动建
        dst: String,
    },

    /// 工作树（按最后一次扫描）与 HEAD 差在哪。**只读，一个字节都不写**
    ///
    /// 它不看文件内容，只看 `W` 与上一版树的差 —— 所以"扫描之后又改了"的那部分
    /// 预演"这次会提交什么"，并预告 FR-10 熔断会不会拦（FR-7 的 plan 侧，默认不执行）
    ///
    /// 比 `status` 多说的两件都是"只有要提交才需要知道"的：熔断预演、head 会怎么动。
    Plan,
    /// 执行 `plan` 预演的那次提交（FR-7 的 apply 侧）。**就是 `commit`**，同一份实现。
    Apply {
        /// 提交说明
        #[arg(long, short = 'm')]
        msg: Option<String>,
        /// 越过 FR-10 删除熔断
        #[arg(long)]
        allow_mass_delete: bool,
        /// 同 `commit --fast-import`（这两个命令是同一份实现，开关也必须一样）
        #[arg(long)]
        fast_import: bool,
        /// 同 `commit --max-mbps`
        #[arg(long, default_value_t = 0.0)]
        max_mbps: f64,
    },
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
    /// 把这个项目推给一个对端（M1）。传输 = `ssh <peer> "<远端 twosync> agent"`：
    /// 不开新端口、复用系统 sshd（§9.5），本端只把它的 stdin/stdout 当一根双向管。
    /// 单向推送：**只让历史过去，不动对端的工作树**（那是 M3 的 `pull`/检出）。
    Push {
        /// 对端标识，直接交给 ssh：`[user@]host[:port]`（端口要用 `-p` 就写进 `~/.ssh/config`）
        peer: String,
        /// 远端的 twosync 可执行文件（在远端 PATH 里时给个名字就够）
        #[arg(long, default_value = "twosync")]
        agent_bin: String,
        /// 远端的站点根（它自己的 `--root`）。**省略 = 用 `project pair` 记下的那个目录**——
        /// 也就是说没配对过的项目压根推不出去（§9.5.3：绝不自动在对端建目录）
        #[arg(long)]
        remote_root: Option<String>,
        /// 只推这一个项目；省略 = 当前项目
        #[arg(long)]
        project: Option<String>,
        /// 本次会话最多发多少 MiB 的块（§9.4.5 的配额）。0 = 不限
        #[arg(long, default_value_t = 0.0)]
        max_send_mib: f64,
        /// 本端越过 FR-10 熔断。**注意对端仍然按它自己的闸判**——这条旗标不跨机器传
        #[arg(long)]
        allow_mass_delete: bool,
    },

    /// 会话的服务端：从 stdin 读帧、往 stdout 写帧。**给 `ssh` 调，不是给人敲的**
    ///
    /// stdout 只能有帧，诊断一律走 stderr（多写一行到 stdout，对端就把它当帧解，
    /// 报出来像"对面在攻击"，其实是我们自己污染通道）。
    Agent {
        /// 只服务这一个项目（白名单）。省略 = 本端注册表里的全部项目
        #[arg(long)]
        project: Option<String>,
    },

    /// 回收：现在只做**引擎版本链**那一半（全库一轮 `vacuum`）；块级配额淘汰（FR-9）还没做
    ///
    /// 为什么需要它：10 GiB 基线里"0 变更的空转提交 45.2 s → 246.8 s"，同期 `rows_above_one`
    /// 20 215 → 82 704。每轮 `scan` 都要给每一行盖新版本（活性判据的命根子），
    /// 而**没人调用过 vacuum** —— 这一条补的就是那半个调用。
    Gc {
        /// 顺手把引擎的自动回收切片宽度设成 N 行（0 = 关闭）
        ///
        /// 门面（`storage_engines::KvEngine`）没有暴露分片 `vacuum_bounded`，所以 `gc` 的
        /// 回收是全库一刀；要"不制造 IO 突发"就把这条交给引擎的小圈（§14 记了引擎侧的缺口）。
        #[arg(long)]
        auto_slice: Option<u64>,

        /// §9.3 的 mark-sweep：回收**不可达**的 commit/tree/blob，以及 refcnt 归 0 的块
        ///
        /// 不带 `--apply` 时只报账、一个字都不改（先看"为什么这个库突然大了"）。
        /// **前提是本站点没有别的写者**：与 `commit`/`push`/`agent` 并发会把正在成形的历史判成垃圾。
        #[arg(long)]
        sweep: bool,
        /// 配合 `--sweep`：真的落改动。不带就是 dry-run
        #[arg(long)]
        apply: bool,
        /// 配额上限（MiB，站点级）。0 = 用 §14 Q12 的默认口径（工作树体积 × 20%，只管首份之后的增量）
        #[arg(long, default_value_t = 0.0)]
        quota_mib: f64,
    },

    /// 对端身份表（§9.5.1 的 TOFU）：`list` 看库里记了谁，`forget` 忘掉一条以便重新确认
    ///
    /// **没有 `add`**：手工 add 要求人先知道对方的站点身份，而那个身份只能从"第一次连上它"看到
    /// ⇒ 正确的形态是首连自动入表（`push` 里做），`add` 没有独立价值。里程碑那行原来写的三条，
    /// 兑现成两条 + 一条如实的"为什么要少一条"。
    Peer {
        #[command(subcommand)]
        cmd: PeerCmd,
    },

    // ── M1 剩下的 ── 块级配额淘汰（FR-9）
    // ── M2 ── daemon / watch
    // ── M3 ── pull / sync
    // ── M4 ── branch / tag / reset / revert / stash / cherry-pick / blame / bisect
    // ── M2 ── daemon / watch
    // ── M3 ── pull / sync
    // ── M4 ── branch / tag / reset / revert / stash / cherry-pick / blame / bisect
}

/// 首次导入的两枚旋钮（§14 Q11）。它们**不改变历史内容**，只改变落盘的等待方式与速率，
/// 所以不进对象编码、不进 commit。收成一对而不是散在函数签名里：`apply` 与 `commit`
/// 必须拿到同一对值，两个 `bool` 挨着摆是会被传反的（本项目已经为"两份参数表"买过两次单）。
#[derive(Debug, Clone, Copy, Default)]
pub struct Import {
    /// true = 落块不等 fsync
    pub fast: bool,
    /// 平均写速率上限，MiB/s；0 = 不限速
    pub max_mbps: f64,
}

impl Import {
    /// 命令行给的是 MiB/s，块仓要的是字节/秒。`0`/负数一律当"不限速"。
    pub fn rate_bytes_per_sec(&self) -> u64 {
        if self.max_mbps <= 0.0 {
            0
        } else {
            (self.max_mbps * 1048576.0) as u64
        }
    }
}

/// §9.5.1 的 TOFU 表操作。
#[derive(Subcommand, Clone)]
pub enum PeerCmd {
    /// 列出已知对端：名字、它的站点身份、第一次确认的时间
    List,
    /// 忘掉一个对端：下一次连接会重新确认它（对端重装过就走这条，不是把历史当坏了）
    Forget { name: String },
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

    /// 确认这个 project 与对端配对了：记下**对端的站点根**（§9.5.3）
    ///
    /// 为什么要有这一步而不是让 `push` 自己决定：一个打错的项目名如果不要求"先配对"，就会把整棵
    /// 工作树复制到对端一个没人监督的路径下。**两边各配一次**才是"已确认配对"——发起端由此知道
    /// 往哪个站点根写，接收端由此承认"这个项目我愿意收"（它那一侧只查"配没配"，不查"是谁"，
    /// 理由是对端身份可以照抄，§9.5.2 与 §15）。
    Pair {
        name: String,
        /// 对端的站点根（它自己的 `--root`；按 §13 的约定，项目目录 = 那个根下的 `<name>`）
        peer_root: String,
    },
    /// 取消配对：只清配对记录，不动工作树、不动历史、不动 CAS
    Unpair { name: String },

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
