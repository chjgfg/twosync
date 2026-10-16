use clap::Subcommand;
use storage_engines::EngineKind;

/// 把"未知引擎"这种输入错误交给 clap：每个 handler 就少一个 else 分支，退出码也统一。
pub fn engine_parser(s: &str) -> Result<EngineKind, String> {
    EngineKind::parse(s)
        .ok_or_else(|| format!("未知引擎 {s:?}；可用：lsm_tree / bplus_tree / bitcask / memory"))
}

/// 全部子命令的清单。加一条命令 = 这里加一个变体 + 对应批次文件加一个函数 + `main` 加一个分支。
///
/// 只加**已经能实现**的变体：变体进了枚举，`main` 的 match 就必须接得住，别提前铺空壳。
#[derive(Subcommand, Clone, Copy)]
pub enum Cmd {
    // ── M0 ─────────────────────────────────────────────────────────
    /// 建站点目录布局（幂等，§13）
    Init,
    /// 打开存储基座，跑 FR-21 启动自检并打印引擎健康度
    Doctor {
        /// 引擎：lsm_tree / bplus_tree / bitcask / memory（后两个应被自检闸拦下）
        #[arg(long, default_value_t = EngineKind::LsmTree, value_parser = engine_parser)]
        engine: EngineKind,
    },
    // ── M1 ── push / agent / gc / peer add|list|trust
    // ── M2 ── daemon / watch
    // ── M3 ── pull / sync
    // ── M4 ── branch / tag / reset / revert / stash / cherry-pick / blame / bisect
}
