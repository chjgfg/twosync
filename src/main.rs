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

/// 必须 `match &cli.cmd`：`Cmd` 装了带 `String`/`PathBuf` 的变体（`Project`）之后就不再是
/// `Copy`，按值匹配会报 E0507「cannot move out of ... behind a shared reference」。
/// 加一条命令要动三处：`cmd.rs` 的枚举变体 + 批次文件里的函数 + 这里一个分支。
fn dispatch(cli: &Cli) -> Result<()> {
    match &cli.cmd {
        // ── M0 ──
        Cmd::Init => m0::init(cli),
        Cmd::Doctor => m0::doctor(cli),
        Cmd::Project(cmd) => m0::project(cli, cmd),
        Cmd::Scan { force, batch } => m0::scan(cli, *force, *batch),
        Cmd::Commit {
            msg,
            allow_mass_delete,
        } => m0::commit(cli, msg.as_deref(), *allow_mass_delete),
        Cmd::Log { max_count } => m0::log(cli, *max_count),
        Cmd::Show { rev, path } => m0::show(cli, rev, path.as_deref()),
        Cmd::Restore {
            at,
            path,
            to,
            force,
        } => m0::restore(cli, at, path, to.as_deref(), *force),
        Cmd::Status => m0::status(cli),
        Cmd::Diff { at } => m0::diff(cli, at),
    }
}
