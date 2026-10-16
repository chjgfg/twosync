//! 工作树扫描：把 root 下每个路径的当前状态写进 `W|p|path`（§9.2）。
//!
//! 三条不显然但必须守住的规则：
//! - **扫描只记录"看到了什么"，绝不记录"没看到 = 已删除"**。删除判定属于 `commit`
//!   （对比上一版 tree），否则一次遍历失败就变成一次大规模误删（§8）。
//! - 读不了的文件（被占用/权限）标 deferred 跳过，**不许当 missing**（§7.2）。
//! - 一批一个事务：事务持有期间引擎 vacuum 整轮跳过（§9.6.2），所以不许"一次大事务扫全树"。
//!
//! # 手工验证（`cargo test` 之外，跑给人看的那一遍）
//!
//! ```text
//! cargo fmt && cargo clippy --all-targets && cargo test
//! rm -rf /tmp/ts3 && cargo run -q -- --root /tmp/ts3 init
//! mkdir -p /tmp/ts3/lab/data                      # 语料必须落在 root/<项目名> 下
//! head -c 33554432 /dev/zero > /tmp/ts3/lab/data/big.bin
//! cd /tmp/ts3/lab/data && for i in $(seq 1 5000); do echo "f$i" > f$i.txt; done
//! cargo run -q -- --root /tmp/ts3 project add lab # 不给 --path ⇒ path = "lab"
//! cargo run -q -- --root /tmp/ts3 scan
//! cargo run -q -- --root /tmp/ts3 scan          # 第二次应 hashed=0、skipped=5002
//! cargo run -q -- --root /tmp/ts3 scan --force  # 全重算，看 throughput 那一行
//! ```
//!
//! 期望：`peak heap` 那一行**与文件数、最大文件都无关**（只跟 `--batch` 与 `HASH_BUF`
//! 有关，这是 §9.6.1 的量尺）；第二次 `hashed=0` 说明 size+mtime 粗筛生效；`--force`
//! 那次的 `throughput` 才是这台机器真实的 blake3 速度。
//!
//! 注意语料为什么放在 `/tmp/ts3/lab/data` 而不是别处：注册表里的 `path` 是**相对 root**
//! 的（§16.1），`check_rel_path` 明确拒绝绝对路径，所以 `--root` 和语料位置必须对得上。

use std::fs::{self, File, Metadata};
use std::io::Read as _;
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use storage_engines::KvEngine;

use crate::error::{Error, Result};
use crate::store::keys::{self, Hash, ProjectId};

/// 1 MiB：hash 是流式的，10 GB 文件也只占这么多
const HASH_BUF: usize = 1 << 20;
/// 重算样本的上限：够诊断用，又不至于让 `scan` 的记忆变成 O(文件数)。
const SAMPLE_MAX: usize = 10;

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WtEntry {
    pub kind: Kind,
    /// **整文件** blake3（扮演 git blob OID 的角色）。块切分留给 `commit`：那样每轮扫描便宜，
    /// 而"版本成为事实"的那一刻才付切块代价。
    pub blob_hash: Hash,
    /// u64 不是风格问题：`size: u32` 装不下一个 10 GB 文件，而大文件正是本项目的存在理由。
    pub size: u64,
    pub mtime_ns: i64,
    pub mode: u32,
    pub symlink_target: Option<String>,
    pub scanned_at: i64,
}

#[derive(Debug, Default)]
pub struct Report {
    pub walked: usize,
    pub hashed: usize,
    pub skipped: usize,
    pub deferred: usize,
    /// 本轮**实际重算 hash** 的字节数。吞吐只能用它算——`bytes` 含 skipped，
    /// 用它当分子会把"跳过"显示成"速度暴跌"。
    pub hashed_bytes: u64,
    /// 全部条目（含 skipped）的字节合计，用于"这棵树多大"。
    pub bytes: u64,
    /// 被重算的路径样本，**最多 10 条**（再多就违反 §9.6.1 的常数内存承诺）。
    /// 用途：第二轮本该 hashed=0，出现 hashed=1 时要能问出"是哪一个、为什么"。
    pub sample: Vec<String>,
    pub elapsed_ms: u128,
}

pub fn scan(
    db: &dyn KvEngine,
    pid: ProjectId,
    root: &Path,
    force: bool,
    batch: usize,
) -> Result<Report> {
    let started = Instant::now();
    let mut rep = Report::default();
    let mut buf = vec![0u8; HASH_BUF];
    let mut pending: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(batch);
    let now = now_ns();
    let mut tx = db.begin();
    for found in walker(root) {
        let Ok(entry) = found else {
            rep.deferred += 1;
            continue;
        };
        let Some(rel) = rel_path(root, entry.path()) else {
            continue;
        };
        let Ok(md) = fs::symlink_metadata(entry.path()) else {
            rep.deferred += 1;
            continue;
        };
        rep.walked += 1;
        let kind = kind_of(&md);
        let mtime_ns = mtime_of(&md);
        let key = keys::worktree_key(pid, &rel);
        // size + mtime 都没变 ⇒ 不重算 hash（§8 允许粗筛，正确性仍由 hash 兜底）。
        // let 链（edition 2024）：比嵌套 if 少一层缩进，也不会触发 collapsible_if。
        let size = logical_size(&md, kind);
        if !force && let Some(prev) = unchanged(&*tx, &key, kind, size, mtime_ns) {
            rep.skipped += 1;
            rep.bytes += prev.size;
            continue;
        }

        let (blob_hash, size, symlink_target) = match digest(entry.path(), kind, &mut buf) {
            Ok(v) => v,
            Err(_) => {
                // 打不开/读失败 = deferred，**不是** missing，更不是删除（§8 第一杀手）。
                rep.deferred += 1;
                continue;
            }
        };
        rep.hashed += 1;
        rep.bytes += size;
        rep.hashed_bytes += size;
        if rep.sample.len() < SAMPLE_MAX {
            rep.sample.push(rel);
        }

        let value = WtEntry {
            kind,
            blob_hash,
            size,
            mtime_ns,
            mode: mode_of(&md),
            symlink_target,
            scanned_at: now,
        };
        let encoded = postcard::to_allocvec(&value).map_err(|e| Error::Codec(e.to_string()))?;
        pending.push((key, encoded));

        if pending.len() >= batch {
            flush(&mut *tx, &mut pending)?;
            tx.commit();
            drop(tx);
            tx = db.begin(); // 一事务一批，别让它跨整棵树
        }
    }
    if !pending.is_empty() {
        flush(&mut *tx, &mut pending)?;
        tx.commit();
    }

    rep.elapsed_ms = started.elapsed().as_millis();
    Ok(rep)
}

/// `require_git(false)`：没有 `.git` 的目录也要认 `.gitignore`；`hidden(false)`：隐藏文件
/// 照样同步，只强制排除 `.git`/`.twosync`（FR-12，配置取消不了）。
fn walker(root: &Path) -> ignore::Walk {
    ignore::WalkBuilder::new(root)
        .require_git(false)
        .hidden(false)
        .filter_entry(|e| !forced_excluded(e.file_name().to_string_lossy().as_ref()))
        .build()
}

fn forced_excluded(name: &str) -> bool {
    name == ".git" || name == ".twosync" || name == ".twosync-project"
}

fn rel_path(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let s = rel.to_string_lossy().replace('\\', "/");
    (!s.is_empty()).then_some(s)
}

fn kind_of(md: &Metadata) -> Kind {
    if md.is_symlink() {
        Kind::Symlink
    } else if md.is_dir() {
        Kind::Dir
    } else {
        Kind::File
    }
}

/// 目录的逻辑大小恒为 0：目录没有"内容"，它的变化由子项表达，所以**不向 OS 要 `nFileSize`**。
///
/// 这不是洁癖：NTFS 上条目多的目录会报告非 0 的 `nFileSize`，而 `digest` 给目录存的
/// 是 0 —— 两边定义不一致，粗筛就永远不命中，那个目录每轮白算一次 hash。实测 ts3 语料
/// 每轮 `hashed=1`、样本正是 `data`（5001 个孩子的目录），而 3 个孩子的同名目录是 0 所以没暴露。
fn logical_size(md: &Metadata, kind: Kind) -> u64 {
    match kind {
        // symlink 也一样：它的身份是"目标路径的 hash"，`md.len()` 是重解析点的大小，
        // 存进去的是 0 ⇒ 两边定义必须一致，否则同一个 bug 换个马甲又来一次。
        Kind::Dir | Kind::Symlink => 0,
        Kind::File => md.len(),
    }
}

fn mtime_of(md: &Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn mode_of(md: &Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        md.permissions().mode()
    }
    #[cfg(not(unix))]
    {
        let _ = md;
        0 // Windows 没有 POSIX mode；只同步 owner 可执行位的方案见 §8
    }
}

fn unchanged(
    tx: &dyn storage_engines::KvTxn,
    key: &[u8],
    kind: Kind,
    len: u64,
    mtime_ns: i64,
) -> Option<WtEntry> {
    let raw = tx.get(key)?;
    let prev: WtEntry = postcard::from_bytes(&raw).ok()?;
    (prev.kind == kind && prev.size == len && prev.mtime_ns == mtime_ns).then_some(prev)
}

/// 流式 hash：常数内存。返回 (内容 hash, 字节数, symlink 目标)。
fn digest(path: &Path, kind: Kind, buf: &mut [u8]) -> std::io::Result<(Hash, u64, Option<String>)> {
    match kind {
        Kind::Dir => Ok((keys::hash_of(b"twosync:dir"), 0, None)),
        Kind::Symlink => {
            let target = fs::read_link(path)?;
            let s = target.to_string_lossy().replace('\\', "/");
            Ok((keys::hash_of(s.as_bytes()), 0, Some(s)))
        }
        Kind::File => {
            let mut f = File::open(path)?;
            let mut hasher = blake3::Hasher::new();
            let mut total = 0u64;
            loop {
                let n = f.read(buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                total += n as u64;
            }
            Ok((*hasher.finalize().as_bytes(), total, None))
        }
    }
}

fn flush(tx: &mut dyn storage_engines::KvTxn, pending: &mut Vec<(Vec<u8>, Vec<u8>)>) -> Result<()> {
    tx.batch_set(pending)
        .map_err(|keys| Error::WriteConflict(format!("{:?}", keys.first())))?;
    pending.clear();
    Ok(())
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}
