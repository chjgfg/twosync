//! twosync — 跨 Windows/Linux 的 P2P 微型 git，为大二进制文件优化。
//!
//! 范围与需求见 `REQUIREMENTS.md`（v0.4）。这里只放**要复用的东西**：站点级目录布局与
//! 存储基座的接入点（FR-3 / FR-21）。命令行入口在 `main.rs`，引擎行为的实机验证在
//! `tests/engine_smoke.rs`。

use std::path::{Path, PathBuf};

use storage_engines::engine::{EngineConfig, EngineKind, KvEngine, StorageConfig};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error(transparent)]
    Engine(#[from] storage_engines::engine::EngineError),
    #[error("引擎 {engine} 未通过启动自检，缺口：{gaps}")]
    DurabilityGate { engine: String, gaps: String },
}

/// 站点级引擎数据目录的父目录（§13）：`<root>/.twosync/db`。
///
/// 引擎会再按名字建一层子目录（`db/lsm_tree/`），所以**换引擎不会自动带数据**。
pub fn db_dir(root: &Path) -> PathBuf {
    root.join(".twosync").join("db")
}

/// 打开存储基座（FR-3），并在返回前过一遍 FR-21 自检。
///
/// 默认选 `EngineKind::LsmTree`；`bplus_tree` 留作崩溃安全对照，`memory` 只允许出现在
/// 测试里（§9.6.5）。
pub fn open_store(root: &Path, kind: EngineKind) -> Result<Box<dyn KvEngine>, StoreError> {
    let cfg = StorageConfig {
        kind,
        data_dir: db_dir(root),
        engine: EngineConfig::default(),
    };
    let db = cfg.open()?;
    check_durability_gate(db.as_ref())?;
    Ok(db)
}

/// FR-21：画像三要件不全 = 拒绝启动，并说清缺哪一项。
///
/// 三家的画像互不等价（§9.6.5），而 §9.6.3 的静默损坏风险直接取决于 `data_checksum`，
/// 所以这道闸必须存在，且要靠 `durability_profile()` 现问、不许照文档记忆判断。
pub fn check_durability_gate(db: &dyn KvEngine) -> Result<(), StoreError> {
    let p = db.durability_profile();
    let mut gaps = Vec::new();
    if !p.transaction_wal {
        gaps.push("无事务 WAL");
    }
    if !p.fsync_on_commit {
        gaps.push("commit 不 fsync");
    }
    if !p.data_checksum {
        gaps.push("数据记录无校验和");
    }
    if !gaps.is_empty() {
        return Err(StoreError::DurabilityGate {
            engine: p.engine.to_string(),
            gaps: gaps.join("、"),
        });
    }
    Ok(())
}
