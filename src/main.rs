//! twosync CLI 入口：解析参数 → 按批次分发 → 错误统一打印成一行 + 非零退出码。

use std::process::ExitCode;

use clap::Parser;
use twosync::cli::cmd::{DaemonArgs, PullArgs, PushArgs};
use twosync::cli::{m0, m1, m2, m3, m4};
use twosync::cli::{Cli, Cmd, Import};
use twosync::error::Result;

fn main() -> ExitCode {
    let cli = Cli::parse();
    // 结构化日志（NFR-10 / FR-17）：JSON 文件 + 按日轮转，落在 <root>/.twosync/log/。
    // **agent 的 stdout 只能有帧**，所以 writer 只走文件，绝不碰 stdout/stderr 的协议通道。
    // WorkerGuard 必须活到进程结束（drop 才 flush）——绑在 main 作用域。
    // 初始化失败（建不了目录等）不拦命令：日志是观测，不是正确性前提，降级成无日志并警告。
    let _guard = init_logging(&cli);
    match dispatch(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// 接 tracing-subscriber：EnvFilter（RUST_LOG，默认 info）+ JSON 文件层 + 按日轮转。
/// 返回的 guard drop 时 flush 缓冲；拿不到就 None（命令照常跑，只是没有文件日志）。
fn init_logging(cli: &Cli) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let log_dir = twosync::utils::log_dir(&cli.root);
    if std::fs::create_dir_all(&log_dir).is_err() {
        return None;
    }
    let appender = tracing_appender::rolling::daily(&log_dir, "twosync.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let file_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(writer)
        .with_target(true);
    // 只装文件层：给人看的打印仍走各命令的 println!（agent 的 stdout 纪律不被破坏）。
    let _ = tracing_subscriber::registry().with(filter).with(file_layer).try_init();
    Some(guard)
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
            max_cpu_mbps,
        } => m0::apply(
            cli,
            msg.as_deref(),
            *allow_mass_delete,
            Import {
                fast: *fast_import,
                max_mbps: *max_mbps,
                max_cpu_mbps: *max_cpu_mbps,
            },
        ),
        Cmd::Scan { force, batch, only, max_cpu_mbps } => {
            m0::scan(cli, *force, *batch, only.as_deref(), *max_cpu_mbps)
        }
        Cmd::Commit {
            msg,
            allow_mass_delete,
            fast_import,
            max_mbps,
            max_cpu_mbps,
            amend,
        } => {
            if *amend {
                m4::amend(cli, msg.as_deref(), *allow_mass_delete)
            } else {
                m0::commit(
                    cli,
                    msg.as_deref(),
                    *allow_mass_delete,
                    Import {
                        fast: *fast_import,
                        max_mbps: *max_mbps,
                        max_cpu_mbps: *max_cpu_mbps,
                    },
                )
            }
        }
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
                just_committed: false,
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
                just_committed: false,
            },
        ),
        Cmd::Daemon {
            interval,
            debounce,
            rounds,
            max_mbps,
            max_cpu_mbps,
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
                max_cpu_mbps: *max_cpu_mbps,
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
        Cmd::Prune {
            days,
            keep,
            apply,
            max_mbps,
        } => m4::prune(cli, *days, *keep, *apply, *max_mbps),
        Cmd::Tag { cmd } => m4::tag(cli, cmd),
        Cmd::Branch { cmd } => m4::branch(cli, cmd),
        Cmd::Blame { path, max } => m4::blame(cli, path, *max),
        Cmd::Stash { cmd } => m4::stash(cli, cmd),
        Cmd::Bisect { bad, good } => m4::bisect(cli, bad, good),
        Cmd::Revert {
            rev,
            apply,
            allow_mass_delete,
        } => m4::revert(cli, rev, *apply, *allow_mass_delete),
        Cmd::CherryPick {
            rev,
            apply,
            allow_mass_delete,
        } => m4::cherry_pick(cli, rev, *apply, *allow_mass_delete),
        Cmd::Merge {
            keep_both,
            take_local,
            take_remote,
            apply,
            allow_mass_delete,
        } => m3::merge(
            cli,
            *keep_both,
            *take_local,
            *take_remote,
            *apply,
            *allow_mass_delete,
        ),
        Cmd::Reset {
            rev,
            apply,
            allow_mass_delete,
        } => m4::reset(cli, rev, *apply, *allow_mass_delete),
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
        Cmd::Status { peer, agent_bin } => m0::status(cli, peer.as_deref(), agent_bin),
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
