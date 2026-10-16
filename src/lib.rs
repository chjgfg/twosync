//! twosync — 跨 Windows/Linux 的 P2P 微型 git，为大二进制文件优化。
//!
//! 范围与需求见 `REQUIREMENTS.md`（v0.4）。模块分工：
//! - [`utils`]：站点级目录布局 + 存储基座接入点（FR-3 / FR-21），**要复用的东西都在这**
//! - [`error`]：唯一的错误类型 [`Error`](error::Error) 与 `Result<T>` 别名
//! - [`cli`]：命令清单与按里程碑分批的实现；分发在 `main.rs`
//!
//! 引擎行为的实机验证在 `tests/engine_smoke.rs`。

pub mod cli;
pub mod error;
pub mod store;
pub mod utils;
