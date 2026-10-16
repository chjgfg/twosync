//! 工作树扫描：把 root 下每个路径的当前状态写进 `W|p|path`（§9.2）。
//!
//! 三条不显然但必须守住的规则：
//! - **扫描只记录"看到了什么"，绝不记录"没看到 = 已删除"**。删除判定属于 `commit`
//!   （对比上一版 tree），否则一次遍历失败就变成一次大规模误删（§8）。
//! - 读不了的文件（被占用/权限）标 deferred 跳过，**不许当 missing**（§7.2）。
//! - 一批一个事务：事务持有期间引擎 vacuum 整轮跳过（§9.6.2），所以不许"一次大事务扫全树"。
//!
//! # 「看见过」这件事怎么留下证据
//!
//! 前两条规则合起来有个后果：`W` 表只增不删，光看它永远看不出谁被删了。所以扫描给
//! **每一轮看见的行**盖本轮时间戳 `scanned_at`，`commit` 只认**最新一轮**的行，其余当作已删除
//! （见 `build::snapshot`）。粗筛未命中的行也要续期（省的是 blake3，不是那次 `set`）；读不了
//! 的行同样续期，否则 §7.2 的 deferred 会伪装成删除。
//! 代价：稳态轮也要重写全部行 ⇒ 每个键多一个版本行。回收靠引擎 vacuum，`doctor` 的
//! `rows_above_one` 是这条的量表；一轮中途被打断（只有半轮带新 epoch）由 FR-10 熔断兜住。
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

/// 条目类型统一住在 `keys`（对象层要用同一个枚举）。这里再导出一次是为了让
/// `worktree::Kind` 这个既有路径继续可用，同时保证只有一份定义。
pub use crate::store::keys::Kind;

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

/// 一轮**完整跑完**的扫描留下的标记（`Z|p`）。为什么不能靠"行里最大的 `scanned_at`"，
/// 见 `keys::TAG_SCAN` 那段。
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanMark {
    /// 本轮的 `scanned_at`。凡是 `scanned_at >= epoch` 的 `W` 行都算"还活着"。
    pub epoch: i64,
    /// 本轮盖章的行数（新增 + 续期）。它与 `walked` 的差就是读失败又没续上期的那些。
    pub seen: u32,
    pub walked: u32,
    /// 第几轮完整扫描（从 1 起）。`project status` 里给个人看，也方便对账。
    pub round: u32,
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
    /// 本轮盖章的行数 = 写进 `Z.seen` 的那个数。`walked` 比它大是因为根目录自己不入库、
    /// 以及读失败又没续上期的行。
    pub seen: usize,
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
        // 攒满一窗就落盘并换事务。放在循环**开头**，三个 push 点就共用这一处；
        // 代价是 pending 最多 batch+1 条，不是 batch 条。
        if pending.len() >= batch {
            rep.seen += flush(&mut *tx, &mut pending)?;
            tx.commit();
            drop(tx);
            tx = db.begin(); // 一事务一批，别让它跨整棵树
        }

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
        if !force
            && let Some(mut prev) = prev_entry(&*tx, &key)
                .filter(|p| p.kind == kind && p.size == size && p.mtime_ns == mtime_ns)
        {
            rep.skipped += 1;
            rep.bytes += prev.size;
            // 内容没变，但「本轮看见了」必须留痕，否则下一轮会被 `commit` 判成删除。
            prev.scanned_at = now;
            pending.push((key, encode(&prev)?));
            continue;
        }

        let (blob_hash, size, symlink_target) = match digest(entry.path(), kind, &mut buf) {
            Ok(v) => v,
            Err(_) => {
                // 打不开/读失败 = deferred，**不是** missing，更不是删除（§8 第一杀手）。
                rep.deferred += 1;
                refresh_liveness(&*tx, &key, now, &mut pending);
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
        pending.push((key, encode(&value)?));
    }
    if !pending.is_empty() {
        rep.seen += flush(&mut *tx, &mut pending)?;
        tx.commit();
    }

    // 「这一轮完整走完了」的标记。**必须在全部行落盘之后**再写：反过来做，崩溃会留下一个
    // 指向半轮数据的 epoch，而半轮 epoch 正是我们要消灭的那种假删除的来源。
    drop(tx);
    let round = last_mark(db, pid).map_or(0, |m| m.round);
    let mark = ScanMark {
        epoch: now,
        seen: rep.seen as u32,
        walked: rep.walked as u32,
        round: round + 1,
    };
    let tx = db.begin();
    if !tx.set(&keys::scan_key(pid), encode_mark(&mark)?) {
        return Err(Error::WriteConflict("Z 扫描标记".to_string()));
    }
    tx.commit();

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
///
/// `pub` 是给 `restore` 的 §7.3 门用的：**"变没变"这件事全站只能有一把尺**，多一处定义
/// 就是多一处上面那个 bug 的候选产地。
pub fn logical_size(md: &Metadata, kind: Kind) -> u64 {
    match kind {
        // symlink 也一样：它的身份是"目标路径的 hash"，`md.len()` 是重解析点的大小，
        // 存进去的是 0 ⇒ 两边定义必须一致，否则同一个 bug 换个马甲又来一次。
        Kind::Dir | Kind::Symlink => 0,
        Kind::File => md.len(),
    }
}

/// 与 `logical_size` 同理：`pub` 出去给 §7.3 的门复用，"mtime 怎么算"也只许有一处定义。
pub fn mtime_of(md: &Metadata) -> i64 {
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

/// 读一条已有的 `W` 记录。读不到/标签不对/解不开一律返回 `None` = 当新条目处理：
/// 这三种情况下库里那行都不是可信的旧状态，重算一次 hash 是安全的。
fn prev_entry(tx: &dyn storage_engines::KvTxn, key: &[u8]) -> Option<WtEntry> {
    let raw = tx.get(key)?;
    let bytes = keys::strip(&raw).ok()?;
    postcard::from_bytes(bytes).ok()
}

/// 最近一次**完整**扫描的标记。`None` = 从没完整跑完过 ⇒ `commit` 不该认为有任何活行。
///
/// 解不开也返回 `None`（保守）：宁可报"还没扫描过"，也不要拿着一个可疑的 epoch 去判删除。
pub fn last_mark(db: &dyn KvEngine, pid: ProjectId) -> Option<ScanMark> {
    let raw = {
        let tx = db.begin();
        let got = tx.get(&keys::scan_key(pid));
        tx.commit();
        got
    }?;
    postcard::from_bytes(keys::strip(&raw).ok()?).ok()
}

/// 写引擎前的最后一步：加信封标签（见 `keys::VALUE_TAG`）。
fn encode_mark(m: &ScanMark) -> Result<Vec<u8>> {
    Ok(keys::wrap(
        postcard::to_allocvec(m).map_err(|e| Error::Codec(e.to_string()))?,
    ))
}

fn encode(e: &WtEntry) -> Result<Vec<u8>> {
    Ok(keys::wrap(
        postcard::to_allocvec(e).map_err(|err| Error::Codec(err.to_string()))?,
    ))
}

/// 「本轮看见了，但内容没算出来」的行：把旧记录的 `scanned_at` 续期后原样写回。
///
/// 不返回错误是刻意的：这一条读不了已经记在 `deferred` 上了，续期失败不该让整个扫描停。
/// 真失败的结果是那一行在下一次 `commit` 里显示成删除，而 FR-10 的熔断正是给这种情况准备的。
fn refresh_liveness(
    tx: &dyn storage_engines::KvTxn,
    key: &[u8],
    now: i64,
    pending: &mut Vec<(Vec<u8>, Vec<u8>)>,
) {
    let Some(mut prev) = prev_entry(tx, key) else {
        return;
    };
    prev.scanned_at = now;
    if let Ok(encoded) = encode(&prev) {
        pending.push((key.to_vec(), encoded));
    }
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

/// 落一整批。返回**写了几行**——`ScanMark.seen` 要的就是这个数，
/// 而它只能来自真正落盘的那一步，不能由别处推算。
fn flush(
    tx: &mut dyn storage_engines::KvTxn,
    pending: &mut Vec<(Vec<u8>, Vec<u8>)>,
) -> Result<usize> {
    let n = pending.len();
    tx.batch_set(pending)
        .map_err(|keys| Error::WriteConflict(format!("{:?}", keys.first())))?;
    pending.clear();
    Ok(n)
}

/// 本轮的"现在"。**一次扫描只取一次**：同一个 epoch 是 `commit` 判删除的依据，
/// 中途重新取会让一批行带着两个不同的时间戳。命令层也用它，所以只有这一处定义。
pub fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}
