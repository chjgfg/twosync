//! twosync CLI 入口。
//!
//! M0 阶段只接上 `doctor`：打开存储基座、跑 FR-21 启动自检、打印引擎健康度。
//! 其余子命令按 §16.2 的分层随里程碑进来，不在这里铺空壳。

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use storage_engines::engine::{EngineKind, KvEngine};
use twosync::open_store;

#[derive(Parser)]
#[command(name = "twosync", version, about = "P2P 微型 git：跨 Windows/Linux 同步大二进制项目")]
struct Cli {
    /// 站点级同步根目录（下面挂多个 project，§13）
    #[arg(long, default_value = ".", global = true)]
    root: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 打开存储基座，跑启动自检并打印引擎健康度
    Doctor {
        /// 引擎：lsm_tree / bplus_tree / bitcask / memory（后两个应被自检闸拦下）
        #[arg(long, default_value = "lsm_tree")]
        engine: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Doctor { engine } => doctor(&cli.root, &engine),
    }
}

fn doctor(root: &Path, engine: &str) -> ExitCode {
    let Some(kind) = EngineKind::parse(engine) else {
        eprintln!("未知引擎 {engine:?}；可用：lsm_tree / bplus_tree / bitcask / memory");
        return ExitCode::FAILURE;
    };
    let db = match open_store(root, kind) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    report(&*db);
    ExitCode::SUCCESS
}

/// NFR-8：`health()` 是 O(物理行数)，只允许出现在这种人工触发的诊断路径上。
fn report(db: &dyn KvEngine) {
    let h = db.health();
    println!("engine      = {}", db.name());
    println!(
        "db          = {}",
        db.path().map(|p| p.display().to_string()).unwrap_or_else(|| "<内存>".to_string())
    );
    println!("rows        = {}", h.total_rows);
    println!("live_keys   = {}", h.live_keys);
    println!("max_chain   = {}", h.max_chain);
    println!("rows_above  = {}", h.rows_above_one);
    println!("active_txns = {}", h.active_txns);
}
