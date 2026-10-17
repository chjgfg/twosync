use std::fs;
use std::path::{Path, PathBuf};

use storage_engines::{EngineConfig, EngineKind, KvEngine, StorageConfig};

use crate::error::{Error, Result};

/// 目标目录所在的文件系统**是否大小写不敏感**。§8「折叠碰撞必须拒」那道门要有它才知道该不该拦。
///
/// 只有一条可靠的路：**在那个目录里**造一个探针文件，再看它另一种大小写的写法是不是同一个东西。
/// 换目录就是换规矩（同一次挂载才同一套），所以探针不能放在别处测。写完立刻删。
///
/// 探不了（只读盘、没权限、目录还不存在）就退回 `cfg!(windows)`：NTFS 默认不敏感，其余按敏感
/// 处理。这是**猜**，不是测——调用方拒了要能说清为什么拒。
pub fn dest_case_insensitive(dir: &Path) -> bool {
    static PROBE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = PROBE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // 两个名字只差一个字母的大小写 ⇒ 折叠之后相同。带上 pid + 自增序号：并发进程与同一进程
    // 的多次调用都不撞（本机 `SystemTime` 粒度约 15.6 ms，光靠时间戳会撞名）。
    let tag = format!("{}.{seq}", std::process::id());
    let lower = dir.join(format!(".tsprobe-a{tag}"));
    let upper = dir.join(format!(".tsprobe-A{tag}"));
    let Ok(handle) = fs::File::create(&lower) else {
        return cfg!(windows);
    };
    drop(handle);
    let hit = fs::symlink_metadata(&upper).is_ok();
    let _ = fs::remove_file(&lower);
    hit
}

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

/// 本站点的身份（§9.5.1 的 TOFU 表绑的就是它）。第一次用到时生成并落盘。
///
/// **这不是密钥，也不打算当密钥用**：v1 不给 commit 签名（§9.5.2），所以"对端自报的身份"本身
/// 不值钱。它回答的问题是另一个、而且是很值钱的一个：**我现在拨到的这台机器，是不是我当初确认过的
/// 那一个站点**。SSH 的 host key 只保证"这台主机还是那台主机"，保证不了"那台主机上跑的还是那个站点"
/// ——跳板机、换目录、重装、两台机器共用一个 hostname 都会变。
///
/// 生成源取"时间 + 进程 id + 站点根路径"折成 16 字节。要的是**唯一**，不是抗伪造：抗伪造归 SSH。
pub fn site_id(root: &Path) -> Result<Vec<u8>> {
    let p = site_dir(root).join("site-id");
    if let Ok(b) = fs::read(&p)
        && b.len() == 16
    {
        return Ok(b);
    }
    let mut h = blake3::Hasher::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    h.update(&now.as_nanos().to_le_bytes());
    h.update(&std::process::id().to_le_bytes());
    h.update(root.to_string_lossy().as_bytes());
    let id = h.finalize().as_bytes()[..16].to_vec();
    // 落盘走 tmp → rename：半截的 id 文件会让下一次读到错误长度，然后**再生成一个不同的 id**，
    // 而对端表里记的是旧的——那正好伪造出一次"身份变了"。
    fs::create_dir_all(site_dir(root))?;
    let tmp = p.with_extension("tmp");
    fs::write(&tmp, &id)?;
    fs::rename(&tmp, &p)?;
    Ok(id)
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
