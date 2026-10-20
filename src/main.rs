//! twosync CLI 入口：解析参数 → 按批次分发 → 错误统一打印成一行 + 非零退出码。

use std::process::ExitCode;

use clap::Parser;
use twosync::cli::cmd::{DaemonArgs, PullArgs, PushArgs};
use twosync::cli::{m0, m1, m2};
use twosync::cli::{Cli, Cmd, Import};
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
        Cmd::Version => m0::version(cli),
        Cmd::Project(cmd) => m0::project(cli, cmd),
        Cmd::Plan => m0::plan(cli),
        Cmd::Apply {
            msg,
            allow_mass_delete,
            fast_import,
            max_mbps,
        } => m0::apply(
            cli,
            msg.as_deref(),
            *allow_mass_delete,
            Import {
                fast: *fast_import,
                max_mbps: *max_mbps,
            },
        ),
        Cmd::Scan { force, batch, only } => m0::scan(cli, *force, *batch, only.as_deref()),
        Cmd::Commit {
            msg,
            allow_mass_delete,
            fast_import,
            max_mbps,
        } => m0::commit(
            cli,
            msg.as_deref(),
            *allow_mass_delete,
            Import {
                fast: *fast_import,
                max_mbps: *max_mbps,
            },
        ),
        Cmd::Push {
            peer,
            agent_bin,
            remote_root,
            project,
            max_send_mib,
            max_mbps,
            allow_mass_delete,
        } => m1::push(
            cli,
            &PushArgs {
                peer,
                agent_bin,
                remote_root: remote_root.as_deref(),
                project: project.as_deref(),
                max_send_mib: *max_send_mib,
                max_mbps: *max_mbps,
                allow_mass_delete: *allow_mass_delete,
            },
        ),
        Cmd::Pull {
            peer,
            agent_bin,
            remote_root,
            project,
            max_recv_mib,
            max_mbps,
            allow_mass_delete,
        } => m1::pull(
            cli,
            &PullArgs {
                peer,
                agent_bin,
                remote_root: remote_root.as_deref(),
                project: project.as_deref(),
                max_recv_mib: *max_recv_mib,
                max_mbps: *max_mbps,
                allow_mass_delete: *allow_mass_delete,
            },
        ),
        Cmd::Daemon {
            interval,
            debounce,
            rounds,
            max_mbps,
            no_push,
            peer,
            agent_bin,
            allow_mass_delete,
            gc_every_secs,
        } => m2::daemon(
            cli,
            &DaemonArgs {
                interval: *interval,
                debounce: *debounce,
                rounds: *rounds,
                max_mbps: *max_mbps,
                no_push: *no_push,
                peer: peer.as_deref(),
                agent_bin,
                allow_mass_delete: *allow_mass_delete,
                gc_every_secs: *gc_every_secs,
            },
        ),
        Cmd::Agent { project } => m1::agent(cli, project.as_deref()),
        Cmd::Gc {
            auto_slice,
            sweep,
            apply,
            unlink_max_files,
            max_mbps,
            quota_mib,
        } => m1::gc(
            cli,
            *auto_slice,
            *sweep,
            *apply,
            *unlink_max_files,
            *max_mbps,
            *quota_mib,
        ),
        Cmd::Peer { cmd } => m1::peer(cli, cmd.clone()),
        Cmd::Log { max_count } => m0::log(cli, *max_count),
        Cmd::Show { rev, path } => m0::show(cli, rev, path.as_deref()),
        Cmd::Restore {
            at,
            path,
            to,
            force,
        } => m0::restore(cli, at, path, to.as_deref(), *force),
        Cmd::Config(cmd) => m0::config(cli, cmd),
        Cmd::Mv { src, dst } => m0::mv(cli, src, dst),
        Cmd::Status => m0::status(cli),
        Cmd::Diff { at } => m0::diff(cli, at),
        Cmd::LsFiles { at, prefix } => m0::ls_files(cli, at, prefix.as_deref()),
        Cmd::Checkout {
            at,
            prefix,
            apply,
            allow_mass_delete,
            to,
            force,
        } => m0::checkout(
            cli,
            at,
            prefix.as_deref(),
            *apply,
            *allow_mass_delete,
            to.as_deref(),
            *force,
        ),
    }
}
