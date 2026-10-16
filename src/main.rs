//! twosync CLI 入口：解析参数 → 按批次分发 → 错误统一打印成一行 + 非零退出码。

use std::process::ExitCode;

use clap::Parser;
use twosync::cli::m0;
use twosync::cli::{Cli, Cmd};
use twosync::error::Result;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// `Cmd: Copy`，所以这里能一边按值取 `cli.cmd`、一边把 `&cli` 传下去。
/// 加一条命令要动三处：`cmd.rs` 的枚举变体 + 批次文件里的函数 + 这里一个分支。
fn dispatch(cli: &Cli) -> Result<()> {
    match cli.cmd {
        // ── M0 ──
        Cmd::Init => m0::init(cli),
        Cmd::Doctor { engine } => m0::doctor(cli, engine),
    }
}
