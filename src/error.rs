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
    /// 快照隔离下的写冲突。§9.6.4：**这是程序 bug，不是"重试一下"**——
    /// 全站按单写者设计，出现冲突说明有两个写者。
    #[error("写冲突，键 {0:?}")]
    WriteConflict(String),
    #[error("编码/解码失败：{0}")]
    Codec(String),
}

/// 全项目只有一种错误，所以签名统一写 `Result<T>`。
pub type Result<T> = std::result::Result<T, Error>;
