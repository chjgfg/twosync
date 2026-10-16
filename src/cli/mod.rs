//! CLI 层。`Cmd` 是命令清单，实现按里程碑分批放在 `m0.rs`…`m4.rs`，
//! 分发在 `main.rs`（加一条命令要动三处：枚举变体 + 批次函数 + match 分支）。

use std::path::PathBuf;

use clap::Parser;

pub mod cmd;
pub mod m0;
pub mod m1;
pub mod m2;
pub mod m3;
pub mod m4;

pub use cmd::Cmd;

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

    #[command(subcommand)]
    pub cmd: Cmd,
}
