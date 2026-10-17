use std::fs;
use std::path::{Path, PathBuf};

use storage_engines::{EngineConfig, EngineKind, KvEngine, StorageConfig};

use crate::error::{Error, Result};

/// 站点级数据根：`<root>/.twosync`。FR-12 的强制排除以它为准，扫描器永不进这里。
pub fn site_dir(root: &Path) -> PathBuf {
    root.join(".twosync")
}

/// 引擎数据目录的父目录（§13）：`<root>/.twosync/db`。
///
/// 引擎会再按名字建一层子目录（`db/lsm_tree/`），所以换引擎不会自动带数据。
pub fn db_dir(root: &Path) -> PathBuf {
    site_dir(root).join("db")
}

/// 内容寻址块仓（§13）：文件字节只在这里，不进引擎。
pub fn objects_dir(root: &Path) -> PathBuf {
    site_dir(root).join("objects")
}

pub fn journal_dir(root: &Path) -> PathBuf {
    site_dir(root).join("journal")
}

pub fn log_dir(root: &Path) -> PathBuf {
    site_dir(root).join("log")
}

/// 项目注册表（§16.1）。
pub fn projects_file(root: &Path) -> PathBuf {
    site_dir(root).join("projects.toml")
}

/// 打开存储基座（FR-3），并在返回前过一遍 FR-21 自检。
///
/// **所有要读写元数据的命令都从这里拿句柄**：自己拼 `StorageConfig` 就会绕过自检闸，
/// 而自检闸存在的理由是四家的崩溃安全画像互不等价（§9.6.5）。
pub fn open_store(root: &Path, kind: EngineKind) -> Result<Box<dyn KvEngine>> {
    let cfg = StorageConfig {
        kind,
        data_dir: db_dir(root),
        engine: EngineConfig::default(),
    };
    let db = cfg.open()?;
    check_durability_gate(db.as_ref())?;
    Ok(db)
}

/// FR-21：画像三要件不全 = 报错并说清缺哪一项。
///
/// 靠 `durability_profile()` 现问，不许照文档记忆判断——文档会过期，画像不会。
pub fn check_durability_gate(db: &dyn KvEngine) -> Result<()> {
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
        return Err(Error::DurabilityGate {
            engine: p.engine.to_string(),
            gaps: gaps.join("、"),
        });
    }
    Ok(())
}

/// 一棵目录树的字节数与条目数（NFR-5 的磁盘账要用：CAS / 引擎 / 工作树各占多少）。
///
/// 只算**普通文件**，且**不跟随符号链接**（含指向目录的链接）：跟随就可能在自引用的树上
/// 转圈，而配额关心的是真占了多少磁盘。链接本身按名字长度记账不算字节。
/// 出错（权限等）不中断整趟，只把这一条计入 `unreadable` —— 报"少了多少"比假装全量准。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DirSize {
    pub bytes: u64,
    pub files: u64,
    pub dirs: u64,
    pub unreadable: u64,
}

pub fn dir_size(root: &Path) -> DirSize {
    let mut out = DirSize::default();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            out.unreadable += 1;
            continue;
        };
        out.dirs += 1;
        for e in entries.flatten() {
            let Ok(md) = fs::symlink_metadata(e.path()) else {
                out.unreadable += 1;
                continue;
            };
            let ft = md.file_type();
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(e.path());
            } else {
                out.bytes += md.len();
                out.files += 1;
            }
        }
    }
    out
}
