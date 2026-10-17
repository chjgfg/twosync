//! CLI 层。`Cmd` 是命令清单，实现按里程碑分批放在 `m0.rs`…`m4.rs`，
//! 分发在 `main.rs`（加一条命令要动三处：枚举变体 + 批次函数 + match 分支）。

use std::path::PathBuf;

use clap::Parser;

pub mod cmd;
pub mod config;
pub mod m0;
pub mod m1;
pub mod m2;
pub mod m3;
pub mod m4;
pub mod registry;

use crate::cli::cmd::engine_parser;
use crate::cli::config::Config;
use crate::error::Result;
use crate::utils::open_store;
pub use cmd::{Cmd, Import, PeerCmd};
use storage_engines::{EngineKind, KvEngine};

/// 全局参数。`Cli` 直接住在本模块，不另开 `cli.rs`——那会撞 `clippy::module_inception`
/// （`cli::cli`），而且它只有这一个结构体，单独成文件没有省下任何东西。
#[derive(Parser)]
#[command(
    name = "twosync",
    version,
    about = "P2P 微型 git：跨 Windows/Linux 同步大二进制项目"
)]
pub struct Cli {
    /// 站点级同步根目录（下面挂多个 project，§13）
    #[arg(long, default_value = ".", global = true)]
    pub root: PathBuf,

    /// 作用的项目；省略时用 `project use` 选中的那个
    #[arg(long, global = true)]
    pub project: Option<String>,

    /// 存储基座引擎（§10.8 换引擎跑同一套验证）
    ///
    /// 省略时依次取 `config` 里的 `engine`、内置默认 `lsm_tree`。**这里不给 clap 默认值**：
    /// 给了就分不清"用户写了"和"没写"，配置文件永远压不过一个凭空出现的值。
    #[arg(long, global = true, value_parser = engine_parser)]
    pub engine: Option<EngineKind>,

    #[command(subcommand)]
    pub cmd: Cmd,
}

impl Cli {
    /// 打开存储基座。"命令行 > 配置文件 > 内置默认"这条顺序**只在这里实现一次**——
    /// 两处各判一遍，就会出现 `status` 看着 lsm、`commit` 写进 bplus 的那种分裂。
    ///
    /// 也不许在命令层自己拼 `StorageConfig`：那会绕过 `open_store` 里的 FR-21 启动自检。
    pub fn store(&self) -> Result<Box<dyn KvEngine>> {
        let kind = match self.engine {
            Some(k) => k,
            None => Config::load(&self.root)?.engine()?,
        };
        open_store(&self.root, kind)
    }

    /// CAS 块大小。同 `store()`：优先级判定只有一处。
    pub fn block_size(&self) -> Result<u64> {
        Ok(Config::load(&self.root)?.block_size())
    }
}
