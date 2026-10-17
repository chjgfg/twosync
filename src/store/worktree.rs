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
//! 期望：第二次 `hashed=0` 说明 size+mtime 粗筛生效；`--force` 那次的 `throughput` 才是
//! 这台机器真实的 blake3 速度。
//!
//! `peak heap` **不是**与文件数无关（2026-10-17 实测更正，见 §9.6.1 与 §17 补记十）：扫描器
//! 自己那一层确实只有 `--batch` + `HASH_BUF`，但引擎每次 `commit` 都要在库里做一次全库级的活，
//! 所以现场 22 000 行 / 库内十万键的站点上地板是 **~25 MiB**，而且 `--batch` 调大只会双输
//! （100 → 9.7 s/24.9 MiB，20 000 → 23.9 s/31.7 MiB）。想看这一条用大语料：
//!
//! ```sh
//! cargo run -q --release -- --root /tmp/ts6 status   # 20000 文件 / 2000 目录
//! ```
//!
//! 注意语料为什么放在 `/tmp/ts3/lab/data` 而不是别处：注册表里的 `path` 是**相对 root**
//! 的（§16.1），`check_rel_path` 明确拒绝绝对路径，所以 `--root` 和语料位置必须对得上。

use std::cmp::Ordering;
use std::collections::BTreeMap;
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
    walk_builder(root).build()
}

/// 排除语义的唯一入口：`scan` 用它（整棵树），`list_dir` 用它（`max_depth(1)`）。
/// **必须是同一个函数**——不然对账报的"未跟踪"会把扫描器排除掉的东西说成漏记。
fn walk_builder(dir: &Path) -> ignore::WalkBuilder {
    let mut b = ignore::WalkBuilder::new(dir);
    b.require_git(false).hidden(false);
    b.filter_entry(|e| !forced_excluded(e.file_name().to_string_lossy().as_ref()));
    b
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

// ── 「扫描之后又发生了什么」：一趟整树走盘 + 只留可疑层的名单 ────────────────
//
// **为什么不是"两条升序流归并"**：朴素想法是 `W` 流升序、盘上也走一遍升序、两个游标对推。
// 但**盘的 DFS 序不等于键的字节序**：`'/'`(0x2F) 在键里就是个普通字符，所以 `src`、`src.rs`、
// `src/main.rs` 三行在库里的顺序是"目录自己 → 同名前缀的兄弟 → 子项"，而 DFS 会先钻进 `src/`。
// 照 DFS 写归并，凡是"目录名是某个兄弟文件名前缀"的目录（`src` 与 `src.rs` 这种常见得不能再
// 常见）就会把未扫的文件错报成"盘上已删"——一个只读命令学会撒谎，比慢更糟。
//
// 换形状：不比两条全局流，只比**可疑目录自己的那一层**。
// 1. 可疑判据 = 目录自己的 mtime 晚于 `Z.epoch`（减一个回写滞后余量，见 [`MTIME_LAG`]）。这正是 git 的 untracked cache 用的失效判据
//    （`dir.h` 里 `untracked_cache_dir { stat_data, .. }`）；实测 `stat` 约 33 µs/次 ⇒
//    2 000 个目录 0.07 s。
// 2. **只走一趟盘**：`walker(root)` 是前序，目录自己排在它的子项之前 ⇒ 读到目录就能当场决定
//    "这一层要不要收名单"，收到名单的层进 `DiskView::at`，其余层的子项直接丢。
//    这里有个用真金白银换来的教训：最早的版本给**每个目录**建一次 `walk_builder(dir)
//    .max_depth(1)`，2 001 次构造每次都要顺着祖先链去找 `.gitignore`，实测把 `status`
//    从 4.1 s 拖到 12.3 s（同一份库对照 HEAD）。走盘的开销在 `readdir` 之外的那套设置上。
// 3. `W` 那一侧**复用 `status` 本来就要走的那趟流**（`build::plan_observed` 的观察口）：在这个
//    引擎里"多一趟"= 多几次全库折叠（§17 补记十），不能白付。
// 4. 排除语义与 `scan` **同一条流**（就是那个 `walker(root)`，不是另起一次遍历），所以被
//    `.gitignore`/强制排除挡住的东西不会在这里冒出来变成"未跟踪"。两份账不一致是静默的。
//
// 这份结果**只进打印，绝不进建树**：§8「扫描器不许判删除」那条防线一个字不改。

/// 一趟走盘的产物：可疑目录 + 每一层盘上的名字。
#[derive(Debug, Default)]
pub struct DiskView {
    /// 可疑目录（相对路径，`""` = 根），升序。名字表在 [`at`](Self::at) 里。
    pub suspects: Vec<String>,
    /// `目录 → 该层盘上子项的名字（升序）`。**只有可疑层才有条目**，所以这是
    /// O(可疑层的子项数) 而不是 O(文件数)。
    pub at: BTreeMap<String, Vec<String>>,
    /// 比过 mtime 的目录数（给"这一趟花在哪"一个可核对的数）。
    pub dirs: usize,
    /// `stat` 不了的条目：这一层没法比，两边都不算（与扫描器的 `deferred` 同一把尺）。
    pub unreadable: usize,
    /// 可疑层多到上限，后面的层不再收名单（只报计数）。
    pub truncated: bool,
}

/// 名单上限：够诊断用，又不让对账的记忆变成 O(变更量)。
const RECON_SAMPLE: usize = 30;
/// 可疑目录超过这个数就只报计数不报名单——再往前就是 O(全树) 内存了。
///
/// `pub` 是给命令层判断"名单是不是被截了"用的：不暴露这个数，`status` 就只能撒谎说名单是全的。
pub const RECON_SUSPECT_MAX: usize = 512;

/// 一趟对账的账。口径与 `build::ChangeSet` 一致：计数全量，名单有界。
#[derive(Debug, Default)]
pub struct Reconciled {
    pub suspect: usize,
    /// 盘上有、`W` 没记：新建还没扫，或在一次没扫完的扫描之后出现。
    pub only_disk: u64,
    /// `W` 里有（且活着）、盘上已经没有了。**只是告知**：删除要等下一轮扫描才进账。
    pub only_w: u64,
    /// 走盘时读不了的条目数（继承扫描器的判据：不许当成"没有"）。
    pub unreadable: u64,
    pub sample: Vec<String>,
    pub sample_more: usize,
    pub truncated: bool,
}

impl Reconciled {
    fn note(&mut self, line: String) {
        if self.sample.len() < RECON_SAMPLE {
            self.sample.push(line);
        } else {
            self.sample_more += 1;
        }
    }
}

/// 目录 mtime 的**回写滞后**余量。
///
/// 实测（Windows / NTFS / 本机）：往 `keep/` 里写一个文件之后，紧接着用 `walkdir` 缓存的
/// metadata 读到该目录 mtime = epoch **− 1.3 ms**，而几十毫秒后再 `fs::metadata` 读到的是
/// epoch **+ 39.5 ms**——父目录的时间戳是**滞后**更新的。所以判据必须是
/// `mtime > epoch − 余量` 而不是 `> epoch`：误差只会往"报旧"这一侧偏，余量放在前面就是
/// 宁可多比几层（只花钱在 readdir），不可漏比（漏了就是`status` 撒谎）。
/// 1 s 是刻意宽松的：这台机器上量到的滞后是几十毫秒级。
/// git 的 untracked cache 撞的是同一堵墙，它的文档原话是
/// "Before setting it to `true`, you should check that mtime is working properly on your system"。
const MTIME_LAG: i64 = 1_000_000_000;

/// 走一趟 `root`，把"扫描之后被动过的目录"和**那些目录的子项名单**一起收回来。
///
/// `epoch` 是那一轮扫描**开始时**取的戳，所以判据用 `>`：扫描期间被人动过的目录也算可疑——
/// 宁可多比一层，不可漏。根目录自己单独 `stat`（`walker` 的第一条就是根，`rel_path` 给它 `None`）。
pub fn view_since(root: &Path, epoch: i64) -> Result<DiskView> {
    let mut out = DiskView::default();
    match fs::metadata(root) {
        Ok(md) => {
            // 根自己也是"比过一层"，计入 dirs：不然 status 那句"比过 N 层"会少报一层。
            out.dirs += 1;
            if mtime_of(&md) > epoch - MTIME_LAG {
                open_layer(&mut out, "");
            }
        }
        // 根读不了：没什么可比的，但要说出来（不许静默变成"盘是空的"）。
        Err(_) => {
            out.unreadable += 1;
            return Ok(out);
        }
    }

    // 前序：目录自己先于它的子项 ⇒ 决定"要不要收这一层"和"收这一层的名字"在同一条流上完成。
    for found in walker(root) {
        let Ok(entry) = found else {
            out.unreadable += 1;
            continue;
        };
        let Some(rel) = rel_path(root, entry.path()) else {
            continue;
        };
        let Ok(md) = entry.metadata() else {
            out.unreadable += 1;
            continue;
        };
        let is_dir = md.is_dir() && !md.is_symlink();
        if is_dir {
            out.dirs += 1;
            if mtime_of(&md) > epoch - MTIME_LAG {
                open_layer(&mut out, &rel);
            }
        }
        // 子项（含目录自己那条）只在其父层被收时才留下。
        let (parent, name) = rel
            .rsplit_once('/')
            .map_or(("", rel.as_str()), |(p, n)| (p, n));
        if let Some(list) = out.at.get_mut(parent) {
            list.push(name.to_string());
        }
    }
    for list in out.at.values_mut() {
        list.sort();
    }
    out.suspects.sort();
    Ok(out)
}

/// 把 `rel` 这一层登记成可疑层（并开好收名单的槽）。超上限就只报计数。
///
/// 同一个目录被登记两次是可能的（根先登一次，走到根那条时又登一次）吗？不会：根由
/// `fs::metadata` 单独处理，`walker` 给的根那条 `rel_path` 是 `None` ⇒ 不进循环。
/// 已有的键走 `Occupied` 分支，只保证槽存在，不会重复进 `suspects`。
fn open_layer(out: &mut DiskView, rel: &str) {
    if out.at.contains_key(rel) {
        return;
    }
    if out.suspects.len() >= RECON_SUSPECT_MAX {
        out.truncated = true;
        return;
    }
    out.suspects.push(rel.to_string());
    out.at.insert(rel.to_string(), Vec::new());
}

/// 把盘上那一层与库里那一层比掉。**纯比较，不碰盘也不碰库**（所以它可单测）。
///
/// `db_names` 是 `status` 在同一路 `W` 流里按父目录收上来的：同一个目录的行按键序到达，
/// 尾巴就是名字升序 ⇒ 一层内部可以放心两游标归并，不存在 [`view_since`] 开头那个跨层交错。
pub fn reconcile(view: &DiskView, db_names: &BTreeMap<String, Vec<String>>) -> Reconciled {
    let mut rep = Reconciled {
        suspect: view.suspects.len(),
        unreadable: view.unreadable as u64,
        truncated: view.truncated,
        ..Default::default()
    };
    let empty = Vec::new();
    for rel in &view.suspects {
        let disk = view.at.get(rel).unwrap_or(&empty);
        let db = db_names.get(rel).unwrap_or(&empty);
        let (mut i, mut j) = (0usize, 0usize);
        while i < disk.len() && j < db.len() {
            match disk[i].cmp(&db[j]) {
                Ordering::Less => {
                    rep.only_disk += 1;
                    rep.note(format!("+ {}", lead(&disk[i], rel)));
                    i += 1;
                }
                Ordering::Equal => {
                    i += 1;
                    j += 1;
                }
                Ordering::Greater => {
                    rep.only_w += 1;
                    rep.note(format!("- {}", lead(&db[j], rel)));
                    j += 1;
                }
            }
        }
        // 尾巴：一边耗尽之后剩下的全是差异。
        for d in &disk[i..] {
            rep.only_disk += 1;
            rep.note(format!("+ {}", lead(d, rel)));
        }
        for b in &db[j..] {
            rep.only_w += 1;
            rep.note(format!("- {}", lead(b, rel)));
        }
    }
    rep
}

/// 样本里的路径：顶层不带前缀，子目录带上目录名，让人能直接 `ls`。
fn lead(name: &str, rel: &str) -> String {
    if rel.is_empty() {
        name.to_string()
    } else {
        format!("{rel}/{name}")
    }
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
