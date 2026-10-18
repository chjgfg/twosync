//! §9.3 的回收：一趟 mark-sweep 清掉**不可达**的对象与块，并给出配额账。
//!
//! # 淘汰集合为什么只有"不可达"
//!
//! FR-9 的原话是"超出配额按最老且不可达优先……**不得**删仍被保留 head 可达的块"。这两半其实是
//! 同一件事：一个 commit 只要还从某个 ref 可达，删它的块就违反不变量 (d)，而那是**不可逆**的那类错。
//! 所以这里的集合是严格的：
//!
//! ```text
//! 保留 = 全部 ref 的可达闭包（commit）∪ 这些 commit 的 tree ∪ 这些 tree 的 blob
//! 回收 = 不在保留集里的一切（块的死活不另算，交给 refcnt —— 它本来就是跨项目共享的账）
//! ```
//!
//! 真的需要腾地方（**保留集本身就超配额**）时，这一版不擅自丢历史，而是把账报出来：决定丢哪一版是
//! 策略命令（M4 的 `reset` / `prune --expire`）的活，不是一个大半夜自动跑的循环的活。这条边界是设计，
//! 不是没做完——它把"谁有权删历史"留在人敲的命令里。
//!
//! # 为什么块的死活看 refcnt 而不是看保留集
//!
//! CAS 是**站点级共享**的（§13），同一个块可以同时被两个项目的两份清单引用。mark 阶段只走本项目
//! 的历史，看不见另一个项目；而 `K`/`k` 那两行是"每次采纳一份清单就 +1"的账（`build::adopt_blob`），
//! 与它对称地 -1 才是正确的动作。所以：**blob 被回收 ⇒ 它的块 refcnt-1；refcnt 归 0 ⇒ 文件才删**。
//!
//! # 顺序（FR-13 的镜像）+ 为什么删文件要单独一趟
//!
//! 先改账（`K`/`k`/`B`/`T`/`C` 行），**后**删文件。反过来崩在中间就会出现"清单还引用着一个已经不存在的
//! 块"——那是要报错给用户的磁盘级不一致。所以 [`sweep`] 只做记账：refcnt 归零的块写进
//! **pending-delete 表**（§9.3 的那个列表，`D|p|hash`），真正碰文件系统的是 [`unlink_pending`]，
//! 它每删一个先过 [`Cas::pace`] 的桶（FR-15），并受 `max_files` 限额。分成两趟之后最坏情况是
//! "账上说该删、文件还在"——那是垃圾，不是损坏，下一趟接着删就行。
//!
//! # 一处与 §9.2 那句话的偏差（故意的）
//!
//! §9.2 说"GC 找 refcnt 0 走 `k|p|0u64_be` 前缀扫，不用全表扫"。**这一版改成了扫全部 `K` 行**，
//! 因为 dry-run 必须能报出"**将要**归零几个块"，而那些块此刻还挂在 `k|p|1`、`k|p|2` 上——只扫 0 档
//! 会永远报 0（`tests/gc_sweep.rs` 第一次跑就抓到这个）。`k` 索引照旧维护（`adopt_blob`/这里的删除
//! 都动它），将来要做"只清已归零的"快速通道随时可以再走那条索引。
//!
//! 代价不是"点查次数"：0.4.0 的门面**每次读调用都折一遍全库**（§9.6.1、§14 Q10），逐块点查在 10 GiB
//! 站点上就是 21 792 次全库折叠。所以 [`scan_blocks`] 一次 `K|p` 前缀扫拿到
//! `(hash, size, refcnt)` 全量，归零判断、配额账与孤儿核对都在内存里做。
//!
//! 如实把这条成本的形状写清楚：**"读全部块"的那 21 792 折变成了 1 折，但每回收一个对象仍然要折一次**
//! （[`decrement_block`] 的读+写、清单/`C`/`T` 行的删除各一次）⇒ 总账是 `O(被回收的对象数)` 折，
//! 而不是 `O(库里全部块数)` 折。稳态（没什么可回收）因此是便宜的；一次删掉一整个 10 GiB 历史则是贵的，
//! 而那正是 §14 Q14（门面上没有分片版 `vacuum_bounded`）还没解决的地方——**别把这条当成已解决**。
//!
//! 另有一格内存账：`K` 行每条约 40 字节（32 字节 hash 键 + 值），21 792 条 ≈ 1 MB 量级，
//! 10 GiB 站点仍在 RSS 80 MB 预算内；真要扩到百万块，这里得改成分片扫（记进 §15 的已知限制，
//! 不假装已经解决）。
//!
//! # 一条硬前提：单写者
//!
//! [`sweep`] 不能与 `commit`/`push`/`agent` 并发。采纳一份清单是"写 `B` 行 → 逐块 `refcnt++`"两步，
//! 中间插一次 sweep 就会把一份**正在被采纳**的清单判成不可达并把它独占的块删掉。
//!
//! 这条前提在跨进程方向上**不是靠自觉**：`storage_engines` 拿的是库级文件锁，第二个句柄 open 直接被拒
//! （实测 `kind: WouldBlock`，报文原话"请先释放全部句柄再 open"），所以"一条 `gc` 与一条 `commit`
//! 同时握着同一个站点"在这台机器上根本开不了张（`tests/gc_sweep.rs` 的 `the_pending_table_survives_a_reopen`
//! 就是先撞上这把锁才写对的）。同一进程内仍然只有"一条命令一个句柄"这个约定在守着。
//!
//! # 跑法
//!
//! `twosync gc --sweep`（先看账，一个字不改）/ `twosync gc --sweep --apply`（真回收）。
//! 用例在 `tests/gc_sweep.rs`：可达的一律不动、不可达的整批消失、块数与 refcnt 对账、
//! 以及"崩在中间的孤儿文件被数出来"。

use std::collections::{BTreeSet, HashMap};

use storage_engines::KvEngine;

use crate::error::{Error, Result};
use crate::store::build::{self, BlockMeta};
use crate::store::cas::{self, Cas};
use crate::store::keys::{self, Hash, Kind, ProjectId};
use crate::store::{history, object};

/// 一趟回收的账。`dry = true` 时这些数字是"**将要**动多少"，一个字节都没改。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// 保留集里的事务数（= 全部 ref 的可达闭包）。报出来是为了让人核对"保留得对不对"。
    pub commits_kept: usize,
    pub commits_dropped: usize,
    pub trees_dropped: usize,
    pub blobs_dropped: usize,
    /// refcnt 归零、**已记进 pending 表**的块数与字节（§9.3 的那个列表）。
    /// 这一格不等于"文件已删"——删是 [`unlink_pending`] 那一趟的活。
    pub blocks_pending: usize,
    pub bytes_pending: u64,
    /// pending 表里现在一共欠着多少（含上一次崩在中途没删完的）。
    pub pending_total: usize,
    pub pending_bytes: u64,
    /// 已经删掉的文件数与字节：[`sweep`] 自己不删，所以它恒为 0，由 `run_gc` 在 unlink 之后填。
    pub blocks_unlinked: usize,
    pub bytes_freed: u64,
    /// pending 表上有、文件本来就不在（崩在一次 unlink 的中间、或手工挪走过）：账要清，但没什么可删。
    pub blocks_missing: usize,
    /// 盘上有文件、库里没有 `K` 行 = 崩溃留下的孤儿。**这一版只数不删**（见模块头那条顺序）。
    pub orphan_files: usize,
    pub orphan_bytes: u64,
}

/// pending-delete 一行的值。带 `size` 是为了让 unlink 那一趟**不必回头点查 `K`**
/// （门面每次读调用折全库，§9.6.1：一次要删两万个块时，这一下就是两万折）。
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug)]
pub struct PendingDel {
    pub size: u32,
}

/// 一次限额 unlink 的账。
#[derive(Debug, Default, Clone, Copy)]
pub struct Unlinked {
    pub files: usize,
    pub bytes: u64,
    pub missing: usize,
    pub left: usize,
}

/// 把 pending 表里的块**按 FR-15 的桶限额删掉**。
///
/// 与 [`sweep`] 分成两个动作就是 §9.3 要的形状：归零是记账，删文件是后面一趟可中断、可续、
/// 可限速的活。每删一个之前先从桶里扣它的字节（`Cas::pace`），所以一次 `gc` 不会在单核机上
/// 制造"突然几千次 unlink + 元数据更新"的尖峰。
pub fn unlink_pending(
    db: &dyn KvEngine,
    cas: &Cas,
    pid: ProjectId,
    max_files: usize,
    dry: bool,
) -> Result<Unlinked> {
    let mut out = Unlinked::default();
    let mut prefix = vec![keys::TAG_PENDING];
    prefix.extend_from_slice(&pid.to_be_bytes());
    let tx = db.begin();
    let rows = tx.prefix_scan(&prefix);
    tx.commit();
    if rows.len() as usize > max_files {
        out.left = rows.len() as usize - max_files;
    }
    for row in rows.iter().take(max_files) {
        let Some(tail) = row.key.get(prefix.len()..) else { continue };
        if tail.len() != 32 {
            continue;
        }
        let mut h = [0u8; 32];
        h.copy_from_slice(tail);
        let size = match &row.value {
            Some(v) => keys::rec::<PendingDel>(v, "PendingDel")?.size,
            None => 0,
        };
        if dry {
            // 只报账：**不扣桶**。桶是"这台机器真的吃掉了这些 IO 预算"的账，dry-run 什么都没删。
            out.files += 1;
            out.bytes += u64::from(size);
            continue;
        }
        // 扣桶在前：睡的是"这次删除的额度"，所以被限速的是删除本身，不是清账。
        cas.pace(u64::from(size));
        // 清账（K / k / D）在前、删文件在后：崩在中间留下的是"该删的文件还在"，那是垃圾不是损坏；
        // 反过来就会留下"账上还在、文件已经没了"，而 `verify`/`restore` 会把那当成磁盘损坏报出去。
        let tx = db.begin();
        tx.delete(&keys::pending_key(pid, &h));
        tx.delete(&keys::block_key(pid, &h));
        tx.delete(&keys::refcnt_key(pid, 0, &h));
        tx.commit();
        if cas.remove_block(&h)? {
            out.files += 1;
            out.bytes += u64::from(size);
        } else {
            out.missing += 1;
        }
    }
    Ok(out)
}

/// pending 表现在的规模（报账用；也是"上一次崩在哪儿"的读数）。
pub fn pending_total(db: &dyn KvEngine, pid: ProjectId) -> Result<(usize, u64)> {
    let mut prefix = vec![keys::TAG_PENDING];
    prefix.extend_from_slice(&pid.to_be_bytes());
    let tx = db.begin();
    let rows = tx.prefix_scan(&prefix);
    tx.commit();
    let mut bytes = 0u64;
    for row in &rows {
        if let Some(v) = &row.value {
            bytes += u64::from(keys::rec::<PendingDel>(v, "PendingDel")?.size);
        }
    }
    Ok((rows.len(), bytes))
}

/// 跑一趟 mark-sweep。`dry = true` 只算账、不落任何改动。
pub fn sweep(db: &dyn KvEngine, cas: &Cas, pid: ProjectId, dry: bool) -> Result<Plan> {
    let mut p = Plan::default();

    // ── mark：保留集 ──
    // 全部 ref，不是只看 `main`：M4 有 branch/tag 之后，只看一个 ref 就会把还挂在别处的气判成垃圾。
    let heads = build::all_ref_heads(db, pid, None)?;
    let keep_commits = history::reachable_strict(db, pid, &heads, &[])?;
    let (keep_trees, keep_blobs) = history::marked_objects(db, pid, &keep_commits)?;
    p.commits_kept = keep_commits.len();

    // ── sweep：commit ──
    for h in scan_hashes(db, keys::TAG_COMMIT, pid)? {
        if keep_commits.contains(&h) {
            continue;
        }
        p.commits_dropped += 1;
        if !dry {
            drop_commit(db, pid, &h)?;
        }
    }

    // ── sweep：tree ──
    for h in scan_hashes(db, keys::TAG_TREE, pid)? {
        if keep_trees.contains(&h) {
            continue;
        }
        p.trees_dropped += 1;
        if !dry {
            let tx = db.begin();
            tx.delete(&keys::tree_key(pid, &h));
            tx.commit();
        }
    }

    // ── sweep：blob ──
    // 先把"哪些清单要掉"算出来，并**按块累计将要发生的 -1**。为什么不能像原来那样"边掉边减账、
    // 最后扫一遍 `k|p|0`"：dry-run 那一刻归零还没发生，索引里一个 0 都没有 ⇒ dry-run 报的块数
    // 永远是 0，"先看账"这一步就白看了（这条是 `tests/gc_sweep.rs` 第一次跑就抓出来的）。
    // 现在两种模式走**同一个 candidate 计算**，只是副作用不同。
    let mut drop_blobs: Vec<cas::Blob> = Vec::new();
    let mut sim: HashMap<Hash, u64> = HashMap::new();
    for h in scan_hashes(db, keys::TAG_BLOB, pid)? {
        if keep_blobs.contains(&h) {
            continue;
        }
        let blob = build::read_blob(db, pid, &h)?;
        for b in blob.blocks.iter().filter(|b| !b.zero) {
            *sim.entry(b.hash).or_default() += 1;
        }
        drop_blobs.push(blob);
    }
    p.blobs_dropped = drop_blobs.len();

    // 要归零的块 = 现在的 refcnt 减去将要发生的 -1 之后等于 0。
    //
    // **一次 `K|p` 前缀扫拿全表，不许逐块点查**：0.4.0 的门面每次读调用都折一遍全库（§9.6.1、
    // §14 Q10），21 792 次点查就是 21 792 次全库折叠——那会让 `gc` 自己变成这台机器上最贵的一条命令。
    // 扫一遍拿到 `(hash, size, refcnt)` 之后，判断与孤儿核对都在内存里做。
    let blocks = scan_blocks(db, pid)?;
    let known: BTreeSet<Hash> = blocks.iter().map(|(h, _, _)| *h).collect();
    let mut free: Vec<(Hash, u32)> = Vec::new();
    for (h, size, refcnt) in &blocks {
        // `saturating_sub` 而不是直接减：账要是坏了（-1 的次数比 refcnt 还多），这里不该 panic 也不该
        // 溢出成一个大数把块留下——那种情况由不变量 (g) 的用例与 `--fsck` 去抓。
        let drops = sim.get(h).copied().unwrap_or(0);
        if refcnt.saturating_sub(drops) == 0 {
            free.push((*h, *size));
        }
    }

    // 本次归零的块 = 待删账。**两种模式都先算出来**，只有副作用不同。
    p.blocks_pending = free.len();
    p.bytes_pending = free.iter().map(|(_, s)| u64::from(*s)).sum();
    if !dry {
        for blob in &drop_blobs {
            for b in blob.blocks.iter().filter(|b| !b.zero) {
                decrement_block(db, pid, &b.hash)?;
            }
            let tx = db.begin();
            tx.delete(&keys::blob_key(pid, &blob.hash));
            tx.commit();
        }
        // 归零的块**不在这里删文件**：落 pending-delete 列表（§9.3），删是后面那一趟限额的活。
        // 为什么必须分两趟：在 decrements 现场 `unlink` 的话，崩在中间就是"清单引用一个不存在
        // 的块"——那是要报错给用户的磁盘级不一致，而且来不及救。分成两趟之后，最坏情况是
        // "账上说该删、文件还在"，那是垃圾，不是损坏。
        for (h, size) in &free {
            let body = keys::wrap(keys::enc(&PendingDel { size: *size }, "PendingDel")?);
            let tx = db.begin();
            if !tx.set(&keys::pending_key(pid, h), body) {
                return Err(Error::WriteConflict(keys::short(h)));
            }
            tx.commit();
        }
    }

    // pending 表的规模：含上一次崩在中途、还没删完的那些。
    let (pend_now, pend_bytes) = pending_total(db, pid)?;
    p.pending_total = pend_now;
    p.pending_bytes = pend_bytes;
    if dry {
        // dry 模式没写表，所以表上那个数是"现在的"；本次将要记进去的得自己加上，
        // 否则"先看账"这一步看不到总量（§17 补记十九）。
        p.pending_total += free.len();
        p.pending_bytes += p.bytes_pending;
    }

    // ── 孤儿文件：盘上有、账上没有 ──
    let (n, bytes) = count_orphans(cas, &known)?;
    p.orphan_files = n;
    p.orphan_bytes = bytes;
    Ok(p)
}

/// 一次 `K|p` 前缀扫：`(块 hash, size, refcnt)` 全量。
///
/// 只扫一遍而不是逐块点查，理由写在 [`sweep`] 里那条评论上（门面的每次读调用都折全库）。
/// 值里带 `size` 与 `refcnt`，所以配额账与归零判断用的是同一份数据——不另起一次扫描。
fn scan_blocks(db: &dyn KvEngine, pid: ProjectId) -> Result<Vec<(Hash, u32, u64)>> {
    let mut prefix = vec![keys::TAG_BLOCK];
    prefix.extend_from_slice(&pid.to_be_bytes());
    let tx = db.begin();
    let rows = tx.prefix_scan(&prefix);
    tx.commit();
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let Some(tail) = row.key.get(prefix.len()..) else { continue };
        if tail.len() != 32 {
            continue;
        }
        let Some(v) = &row.value else { continue };
        let meta = keys::rec::<BlockMeta>(v, "BlockMeta")?;
        let mut h = [0u8; 32];
        h.copy_from_slice(tail);
        out.push((h, meta.size, meta.refcnt));
    }
    Ok(out)
}

/// 某个 tag 下本项目全部行的 32 字节尾（对象 hash）。**只收 hash，不收值**：一百万个 commit 的
/// 库里，把值一起物化就是 §9.6.1 明令避免的那种内存。
fn scan_hashes(db: &dyn KvEngine, tag: u8, pid: ProjectId) -> Result<Vec<Hash>> {
    let mut prefix = vec![tag];
    prefix.extend_from_slice(&pid.to_be_bytes());
    let tx = db.begin();
    let rows = tx.prefix_scan(&prefix);
    tx.commit();
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let Some(tail) = row.key.get(prefix.len()..) else { continue };
        if tail.len() != 32 {
            continue;
        }
        let mut h = [0u8; 32];
        h.copy_from_slice(tail);
        out.push(h);
    }
    Ok(out)
}

/// 删一个 commit：`C` 行与它的**全部亲代索引行**（`c|p|hash|ordinal`）同事务删。
/// 少删一半索引，下一次 `reachable` 就会走进一条指向不存在事务的亲代边。
fn drop_commit(db: &dyn KvEngine, pid: ProjectId, hash: &Hash) -> Result<()> {
    let mut prefix = vec![keys::TAG_PARENT];
    prefix.extend_from_slice(&pid.to_be_bytes());
    prefix.extend_from_slice(hash);
    let tx = db.begin();
    let rows = tx.prefix_scan(&prefix);
    tx.commit();
    let tx = db.begin();
    tx.delete(&keys::commit_key(pid, hash));
    for row in &rows {
        tx.delete(&row.key);
    }
    tx.commit();
    Ok(())
}

/// 一份清单被回收 ⇒ 它引用的每个块 refcnt-1。与 `build::bump_block` 严格对称：
/// 同样在一个事务里同时改 `K` 与 `k`（不变量 (g) 的反方向）。
fn decrement_block(db: &dyn KvEngine, pid: ProjectId, hash: &Hash) -> Result<()> {
    let kkey = keys::block_key(pid, hash);
    let prev = {
        let tx = db.begin();
        let got = tx.get(&kkey).map(|raw| keys::rec::<BlockMeta>(&raw, "BlockMeta"));
        tx.commit();
        got.transpose()?
    };
    let Some(meta) = prev else { return Ok(()) };
    let old = meta.refcnt;
    let new = old.saturating_sub(1);
    let body = keys::wrap(keys::enc(&BlockMeta { size: meta.size, refcnt: new }, "BlockMeta")?);
    let tx = db.begin();
    if !tx.set(&kkey, body) {
        return Err(Error::WriteConflict(keys::short(hash)));
    }
    tx.delete(&keys::refcnt_key(pid, old, hash));
    if !tx.set(&keys::refcnt_key(pid, new, hash), Vec::new()) {
        return Err(Error::WriteConflict(keys::short(hash)));
    }
    tx.commit();
    Ok(())
}

/// 数一遍"文件在盘上、`K` 行不在库里"。这些是崩溃残留（`serve` 先落块、清单还没到就被打断）。
///
/// 判据用 `K` 行而不是 refcnt：**没有 `K` 行 = 从来没有一份清单认领过它**。全零块本来就不落盘，
/// 所以它不会被算成孤儿；而 `.tmp.` 打头的文件是写了一半的临时件，不属于块仓的账，跳过。
/// `known` 由调用方一次扫描给出——这里绝不能再点查，理由同 [`scan_blocks`]。
pub fn count_orphans(cas: &Cas, known: &BTreeSet<Hash>) -> Result<(usize, u64)> {
    let mut n = 0usize;
    let mut bytes = 0u64;
    let mut stack = vec![cas.dir().to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if e.file_name().to_string_lossy().starts_with(".tmp.") {
                continue;
            }
            let Some(hash) = block_hash_from_path(&p) else {
                continue;
            };
            if known.contains(&hash) {
                continue;
            }
            n += 1;
            bytes += std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        }
    }
    Ok((n, bytes))
}

/// 从 `<dir>/<hh>/<hex[2:]>` 还原 hash。形状不对一律 `None`（不许把奇怪的文件硬编成一个 hash）。
fn block_hash_from_path(p: &std::path::Path) -> Option<Hash> {
    let file = p.file_name()?.to_str()?;
    let parent = p.parent()?.file_name()?.to_str()?;
    if parent.len() != 2 || !parent.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut s = String::with_capacity(64);
    s.push_str(parent);
    s.push_str(file);
    keys::unhex(&s)
}

/// 保留集的形状，给命令层打印用（"这一版为什么没删东西"要看的就是这四个数）。
pub fn keep_report(db: &dyn KvEngine, pid: ProjectId) -> Result<(usize, usize, usize)> {
    let heads = build::all_ref_heads(db, pid, None)?;
    let commits = history::reachable_strict(db, pid, &heads, &[])?;
    let (trees, blobs) = history::marked_objects(db, pid, &commits)?;
    Ok((commits.len(), trees.len(), blobs.len()))
}

/// 供用例与命令层核对：某个块现在的 refcnt。
pub fn refcnt_of(db: &dyn KvEngine, pid: ProjectId, hash: &Hash) -> Result<Option<u64>> {
    let tx = db.begin();
    let raw = tx.get(&keys::block_key(pid, hash));
    tx.commit();
    match raw {
        Some(body) => Ok(Some(keys::rec::<BlockMeta>(&body, "BlockMeta")?.refcnt)),
        None => Ok(None),
    }
}

/// 目录清单里的 blob hash（用例与 `gc` 的核对都用它）。
pub fn blob_of_file(db: &dyn KvEngine, pid: ProjectId, path: &str) -> Result<Hash> {
    let heads = build::all_ref_heads(db, pid, None)?;
    let newest = *heads
        .first()
        .ok_or_else(|| Error::BadInput("项目还没有 head".to_string()))?;
    let c = object::read_commit(db, pid, &newest)?;
    let e = history::resolve(db, pid, &c.tree, path)?
        .ok_or_else(|| Error::NoSuchPath { rev: keys::short(&newest), path: path.to_string() })?;
    if e.kind != Kind::File {
        return Err(Error::BadInput(format!("{path} 不是文件")));
    }
    Ok(e.hash)
}
