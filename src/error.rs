use thiserror::Error;

/// twosync 的统一错误类型：所有命令的返回都是 `Result<(), Error>`，由 `main` 一处打印。
#[derive(Debug, Error)]
pub enum Error {
    /// 存储基座自己的错误（打开引擎、路径要求、不支持的操作）。原样透传，不许改写原因。
    #[error(transparent)]
    Engine(#[from] storage_engines::EngineError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// FR-21：画像三要件不全 = 拒绝启动。缺口必须逐项列出来。
    #[error("引擎 {engine} 未通过启动自检，缺口：{gaps}")]
    DurabilityGate { engine: String, gaps: String },
    /// 项目注册表的问题：重名、未知项目、文件损坏。消息里必须带路径或项目名。
    #[error("注册表：{0}")]
    Registry(String),
    /// 用户给的东西不合法（项目名、仓库内路径、rev 写法）。**与"库里有东西坏了"是两件事**：
    /// 这一类要人改参数，那一类要人查磁盘。
    #[error("参数不合法：{0}")]
    BadInput(String),
    /// 快照隔离下的写冲突。§9.6.4：**这是程序 bug，不是"重试一下"**——
    /// 全站按单写者设计，出现冲突说明有两个写者。
    #[error("写冲突，键 {0:?}")]
    WriteConflict(String),
    #[error("编码/解码失败：{0}")]
    Codec(String),
    /// 对象在库里找不到。与 `CorruptBlock` 是**两件事**：缺失可以合法（还没同步到），
    /// 损坏永远不合法。混为一谈就会把磁盘损坏当成一次普通的重传。
    #[error("{kind} 对象缺失：{hash}")]
    MissingObject { hash: String, kind: String },
    /// CAS 的字节与其 hash 对不上 = **磁盘损坏**，不是"块缺失"。§9.4.7：损坏必须报错，
    /// 不许静默重传——那会把数据损坏伪装成一次普通同步。
    #[error("块校验失败 {hash} @ {path}: {why}")]
    CorruptBlock {
        hash: String,
        path: std::path::PathBuf,
        why: String,
    },
    /// 扫描之后、落盘之前源文件又被人改了 ⇒ 这一条本轮不提交。
    /// **绝不静默用新内容**：那会把一个没被扫描证实过的版本写进历史。
    #[error("工作树在扫描之后被改动：{path}（本轮跳过，等下一轮扫描）")]
    StaleScan { path: String },
    /// **单个目录**的条目数上限。融合遍历之后内存只跟单层挂钩，所以这条红线也从
    /// "项目总条目数"挪到了这里——超限直接**拒绝**而不是 OOM：OOM 会把同机的策略进程
    /// 一起拖下水，那是 NFR-3 的红线。报错必须带目录名，否则用户不知道该拆哪一层。
    #[error("目录 {dir:?} 的条目数 {n} 超过单层上限 {max}：这一层装不下，请把它拆小或分开同步")]
    TooManyEntries { n: usize, max: usize, dir: String },
    /// FR-10 删除熔断。防的是"客户端 bug 把误 `rm -rf` 变成合法历史"，与谁是发起方无关。
    #[error("删除熔断：本次将删 {deleted} 条 / {} MiB（已知 {known} 条）。确认无误再加 --allow-mass-delete",
        deleted_bytes / 1048576)]
    MassDelete {
        deleted: u32,
        known: u32,
        deleted_bytes: u64,
    },
    /// 读命令（`show`/`restore`）要了一条不存在的路径。**与 `MissingObject` 是两件事**：
    /// 后者是"库该有却没有"（损坏或还没同步到），这一条是"那个版本里本来就没有"。
    /// 混在一起就会把"用户打错路径"报成"磁盘坏了"。
    #[error("{path} 在 {rev} 这个版本里不存在")]
    NoSuchPath { rev: String, path: String },
    /// 目标已存在而不肯覆盖。这是**默认行为**：一次打错参数的恢复或改名就静默吃掉本地
    /// 没备份的内容，是不可逆的那类事故。
    ///
    /// `hint` 由抛出的那一层给：`restore` 能 `--force`，`mv` 压根没这个旗标。共用一个错误
    /// 类型没问题，**建议共用就会说谎**。
    #[error("{path} 已经存在：{hint}")]
    LocalExists {
        path: std::path::PathBuf,
        hint: &'static str,
    },
    /// §7.3 铁律：本地这一版还没进过历史 ⇒ 不许覆盖。"进过历史"= 有 `B` 清单**且**块都在。
    #[error("拒绝覆盖 {path}：{why}。先 scan + commit 把本地这一版存进历史，再 --force")]
    NotInHistory {
        path: std::path::PathBuf,
        why: String,
    },
    /// §8：目标盘**不区分大小写**，而这一层的两个名字折叠后落到同一个格子（`README.md` 与
    /// `Readme.md`）。历史里它们是合法的两个文件（在 ext4 上就是这么存的），检出到 NTFS 上
    /// 就是后写的把先写的静默吃掉 ⇒ **报错拒绝**，不许覆盖、也不许改名瞎猜。
    #[error("大小写碰撞：{dir} 这一层的 {a} 与 {b} 在目标盘上是同一个格子。请去掉其中一个，或检出到区分大小写的文件系统")]
    CaseCollision {
        dir: String,
        a: String,
        b: String,
    },
    /// 设计上明确留给后面某个里程碑的行为。**不是**"忘了做"，报出来要能看出归谁。
    #[error("{what}：还没做，{milestone}")]
    NotYet {
        what: String,
        milestone: &'static str,
    },
    /// §9.5.1 TOFU：这次连上的对端与表里记的**不是同一个站点**。这一条是**拒绝会话**，不是提示。
    ///
    /// 报错里必须带上"怎么恢复"：对端重装过是要重新确认（`peer forget`），而不是让人以为历史坏了。
    #[error("对端 {peer} 的身份变了：表里记的是 {want}，这次报的是 {got}。\
         要么对端换过站点（`peer forget {peer}` 重新确认一次），要么你在往**不是它**的机器上推")]
    PeerIdentityChanged {
        peer: String,
        want: String,
        got: String,
    },
}

/// 全项目只有一种错误，所以签名统一写 `Result<T>`。
pub type Result<T> = std::result::Result<T, Error>;
