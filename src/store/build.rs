//! 从工作树快照构建一个 commit：落块 → 写清单 → 自底向上建树 → 比对 → 移 head。
//!
//! # 内存
//!
//! 条目表是 O(文件数)，所以有 `MAX_ENTRIES` **显式上限**：超了就报错拒绝，绝不 OOM
//! （OOM 会把同机的策略进程一起拖下水，那是 NFR-3 的红线）。流式栈式聚合是下一步，
//! 到时候这个上限应该消失而不是调大。
//!
//! # 为什么 refcnt 必须在这里就记对
//!
//! `K.refcnt` 是 GC 唯一的依据。少记一次 = 块被提前删除 = 历史取不回来（不变量 (b) 破）。
//! 所以"新 blob 才加引用、老 blob 不重复加"这条判断（`is_new_blob`）不能省，也不能推迟到
//! GC 那轮再补——GC 看不到"本该 +1 却没 +1"的差额。

use std::collections::{BTreeMap, BTreeSet};
use std::format;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use storage_engines::KvEngine;
use storage_engines::SearchQuery;

use crate::error::{Error, Result};
use crate::store::history::TreeStore;
use crate::store::keys::{self, Hash, Kind, ProjectId};
use crate::store::worktree::WtEntry;
use crate::store::{cas, object};

/// 一次最多聚合多少条目。超过就拒绝，不 OOM。
pub const MAX_ENTRIES: usize = 200_000;
/// 分页读引擎的窗口。
pub const PAGE: usize = 2_000;
/// v1 唯一参与同步的 ref。
pub const MAIN: &str = "main";
/// `status`/`diff` 一次最多列多少条路径。再多就只给计数，不给内存。
pub const SAMPLE_MAX: usize = 30;
/// FR-10 的字节闸：一次删除超过 2 GB 就得有人签字。
const MASS_DELETE_BYTES: u64 = 2 << 30;
/// FR-10 的条数闸：超过已知条目的两成，**且**不少于这个绝对数，才算大规模删除。
const MASS_DELETE_RATIO: f64 = 0.2;
const MASS_DELETE_MIN: u32 = 20;

/// 统一走 `keys::strip` 去信封标签，并带上"这是哪一类记录"——只说"解码失败"的报错
/// 在排查磁盘损坏时没有用。
fn dec<T: DeserializeOwned>(raw: &[u8], what: &str) -> Result<T> {
    postcard::from_bytes(keys::strip(raw)?)
        .map_err(|e| Error::Codec(format!("{what} 解码失败：{e}")))
}

fn enc<T: Serialize>(v: &T, what: &str) -> Result<Vec<u8>> {
    postcard::to_allocvec(v).map_err(|e| Error::Codec(format!("{what} 编码失败：{e}")))
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ChangeSet {
    pub add: u32,
    pub modify: u32,
    pub delete: u32,
    /// 被删条目的 blob hash。§7.3 铁律要它：工作树删除落地前必须确认这些块还在库里。
    pub deleted_blobs: Vec<Hash>,
    pub bytes: u64,
    /// 只算被删文件（目录恒 0）。FR-10 的字节闸看的是这个，不是 `bytes`。
    pub deleted_bytes: u64,
    /// 变更路径样本，**上限 `SAMPLE_MAX`**。`status`/`diff` 要能列出"到底是哪几个文件"，
    /// 而列出全部是 O(文件数) —— 那违反 §9.6.1 的常数内存承诺。计数才是完整的。
    pub sample: Vec<Change>,
    /// 因为上限被丢掉多少条（0 = 样本就是全量）。
    pub sample_more: usize,
}

impl ChangeSet {
    pub fn is_empty(&self) -> bool {
        self.add + self.modify + self.delete == 0
    }

    /// 记一条变更：计数永远记全，路径只留前 `SAMPLE_MAX` 条。
    fn note(&mut self, what: ChangeKind, path: String, size: u64) {
        match what {
            ChangeKind::Add => {
                self.add += 1;
                self.bytes += size;
            }
            ChangeKind::Modify => {
                self.modify += 1;
                self.bytes += size;
            }
            ChangeKind::Delete => {
                self.delete += 1;
                self.deleted_bytes += size;
            }
        }
        if self.sample.len() < SAMPLE_MAX {
            self.sample.push(Change { what, path, size });
        } else {
            self.sample_more += 1;
        }
    }
}

/// 一条变更的样本。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Change {
    pub what: ChangeKind,
    pub path: String,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChangeKind {
    Add,
    Modify,
    Delete,
}

impl ChangeKind {
    pub fn sign(self) -> &'static str {
        match self {
            ChangeKind::Add => "+",
            ChangeKind::Modify => "~",
            ChangeKind::Delete => "-",
        }
    }
}

/// `commit` 眼中的工作树：**最新一轮扫描**留下的行。
#[derive(Debug)]
pub struct Snapshot {
    /// 引擎按字节序返回，所以这里天然按路径升序。
    pub entries: Vec<(String, WtEntry)>,
    /// 这批行共同的 `scanned_at`；0 = 表是空的，也就是还没扫描过。
    pub epoch: i64,
    /// 库里 `W` 行的总数（含本轮没看见的）。FR-10 的分母用它。
    pub known: u32,
}

/// 块的引用记账（`K` 值）。`k` 前缀是它的排序索引，两者必须同事务改（不变量 (g)）。
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockMeta {
    pub size: u32,
    pub refcnt: u64,
}

#[derive(Debug, Default)]
pub struct Built {
    pub commit: Option<Hash>,
    pub tree: Hash,
    pub changes: ChangeSet,
    pub entries: usize,
    /// 本轮扫描的 epoch 与库里全部行数：命令层用它提示"上一轮扫描没覆盖到"这种情况。
    pub epoch: i64,
    pub known: u32,
    /// 本次新落盘的块数与字节（FR-5 的可核对数字）。
    pub blocks_stored: usize,
    pub block_bytes: u64,
    pub blobs_new: usize,
    /// `plan` 算出来但**没落盘**的那些目录清单，供 `diff` 递归（`build` 恒为空）。
    pub trees: TreeStore,
}

/// 一次提交要带的外部信息。单独成结构体而不是一路摊成参数：`build` 已经有四个
/// "从哪儿拿数据"的参数，再加四个"这次提交是谁写的"就没人读得懂了。
#[derive(Debug, Clone, Copy)]
pub struct Meta<'a> {
    /// 产生这次提交的 peer（M1 之前恒为 `"local"`）。
    pub peer: &'a str,
    pub msg: &'a str,
    /// 提交时间戳。**只用于显示与保留策略，不参与裁决**（§4），所以由调用方给而不是内部取时。
    pub ts: i64,
    /// 越过 FR-10 删除熔断（命令行 `--allow-mass-delete`）。
    pub allow_mass_delete: bool,
}

/// HEAD 指向的 commit 的树；还没有任何 commit 时是 `None`。
///
/// 多 head 时取第一个 —— 与 `plan`/`build` 现有语义一致（真正的 union 是 M3 的活）。
fn head_tree(db: &dyn KvEngine, pid: ProjectId) -> Result<Option<Hash>> {
    match ref_heads(db, pid, MAIN)?.first() {
        Some(h) => Ok(Some(object::read_commit(db, pid, h)?.tree)),
        None => Ok(None),
    }
}

/// 只读地算出"如果现在 commit，树会长什么样、与 HEAD 差在哪"。**一个字节都不写**。
///
/// 它存在的理由是 `status` / `diff`：这两条必须能在任何时刻跑，包括磁盘快满、或者扫描
/// 之后文件又被改了的时候。让它们直接复用 `build` 就等于"看一眼"要落块、写对象、撑大
/// 版本链——那已经不是看一眼了。
///
/// 代价说清：`plan` 用 `W` 里现成的整文件 hash，**不复核磁盘**。所以它可能报出一个随后
/// 被 `commit` 以 `StaleScan` 拒掉的改动。这是"按最后一次扫描说话"的定义，不是 bug。
pub fn plan(db: &dyn KvEngine, pid: ProjectId) -> Result<Built> {
    let snap = snapshot(db, pid, MAX_ENTRIES)?;
    let prev_tree = head_tree(db, pid)?;
    let mut built = Built {
        entries: snap.entries.len(),
        epoch: snap.epoch,
        known: snap.known,
        ..Default::default()
    };
    let mut sink = PlanSink::default();
    built.tree = assemble(db, pid, &snap, &mut built, &mut sink)?;
    built.trees = sink.trees;
    built.changes = diff(db, pid, prev_tree.as_ref(), &built.tree, &built.trees)?;
    Ok(built)
}

/// 构建并落库一个 commit。无改动时返回 `commit: None`，**不产生空提交也不移 head**。
pub fn build(
    db: &dyn KvEngine,
    pid: ProjectId,
    store: &cas::Cas,
    work_root: &Path,
    meta: Meta<'_>,
) -> Result<Built> {
    let snap = snapshot(db, pid, MAX_ENTRIES)?;
    let prev_heads = ref_heads(db, pid, MAIN)?;
    let prev_tree = head_tree(db, pid)?;

    let mut built = Built {
        entries: snap.entries.len(),
        epoch: snap.epoch,
        known: snap.known,
        ..Default::default()
    };
    built.tree = assemble(
        db,
        pid,
        &snap,
        &mut built,
        &mut WriteSink { store, work_root },
    )?;

    // build 已经把树写进库了，diff 直接读库，不必再带一份内存副本。
    built.changes = diff(
        db,
        pid,
        prev_tree.as_ref(),
        &built.tree,
        &TreeStore::default(),
    )?;
    if built.changes.is_empty() {
        return Ok(built); // 不产生空提交：碎 commit 会把引擎版本链白白撑大（§9.6.2）
    }
    check_mass_delete(
        built.changes.delete,
        built.changes.deleted_bytes,
        snap.known,
        meta.allow_mass_delete,
    )?;

    let c = object::Commit {
        parents: prev_heads.clone(),
        tree: built.tree,
        peer: meta.peer.to_string(),
        ts: meta.ts,
        msg: meta.msg.to_string(),
        stats: object::Stats {
            add: built.changes.add,
            modify: built.changes.modify,
            delete: built.changes.delete,
            rename: 0, // 改名检测是 FR-17 的活，M0 收尾时补；现在一律按 add+delete 记
            bytes: built.changes.bytes,
        },
    };
    let commit = object::put_commit(db, pid, &c)?;
    set_ref(db, pid, MAIN, &[commit])?;
    built.commit = Some(commit);
    Ok(built)
}

/// 组装阶段"要不要落盘"的两个决策点。
///
/// 用 trait 而不是 `write: bool`：布尔参数会一路传到调用链上，谁传反了谁就静默产出半成品；
/// 这里两个实现各自只有一条路可走，编译器替你记住。
pub trait Sink {
    /// 一个文件 / 符号链接条目 → 它的 blob hash。
    fn blob_of(
        &mut self,
        db: &dyn KvEngine,
        pid: ProjectId,
        path: &str,
        e: &WtEntry,
        built: &mut Built,
    ) -> Result<Hash>;
    /// 一份目录清单 → tree hash（写或不写）。
    fn tree_of(
        &mut self,
        db: &dyn KvEngine,
        pid: ProjectId,
        entries: &[object::TreeEntry],
    ) -> Result<Hash>;
}

/// 只算不写，喂给 `plan`。算出来的清单留在 `trees` 里给 diff 递归用。
#[derive(Default)]
struct PlanSink {
    trees: TreeStore,
}

impl Sink for PlanSink {
    fn blob_of(
        &mut self,
        _db: &dyn KvEngine,
        _pid: ProjectId,
        _path: &str,
        e: &WtEntry,
        _built: &mut Built,
    ) -> Result<Hash> {
        Ok(e.blob_hash)
    }

    fn tree_of(
        &mut self,
        _db: &dyn KvEngine,
        _pid: ProjectId,
        entries: &[object::TreeEntry],
    ) -> Result<Hash> {
        // 存**排好序的**那份：从库里读回来的是规范序，两边形状一样，diff 才不可能
        // 因为顺序差异给出两种答案。
        let mut sorted = entries.to_vec();
        sorted.sort();
        let hash = object::tree_hash(&sorted);
        self.trees.remember(hash, sorted);
        Ok(hash)
    }
}

/// 真落盘，喂给 `build`。
struct WriteSink<'a> {
    store: &'a cas::Cas,
    work_root: &'a Path,
}

impl Sink for WriteSink<'_> {
    fn blob_of(
        &mut self,
        db: &dyn KvEngine,
        pid: ProjectId,
        path: &str,
        e: &WtEntry,
        built: &mut Built,
    ) -> Result<Hash> {
        match e.kind {
            Kind::File => put_blob(db, pid, self.store, self.work_root, path, e, built),
            Kind::Symlink => put_link(db, pid, self.store, path, e, built),
            Kind::Dir => Err(Error::Codec(format!(
                "{path} 是目录，它没有 blob：这一层归 tree"
            ))),
        }
    }

    fn tree_of(
        &mut self,
        db: &dyn KvEngine,
        pid: ProjectId,
        entries: &[object::TreeEntry],
    ) -> Result<Hash> {
        object::put_tree(db, pid, entries)
    }
}

/// 快照 → 一棵根 tree。**这段逻辑全站只有这一份**：顶层目录静默丢失那个 bug 就住在这里，
/// 给 `plan` 复制一份等于再埋一个同根因的坑，而且第二份没人会去测。
fn assemble<S: Sink>(
    db: &dyn KvEngine,
    pid: ProjectId,
    snap: &Snapshot,
    built: &mut Built,
    sink: &mut S,
) -> Result<Hash> {
    let mut dirs: BTreeMap<String, Vec<object::TreeEntry>> = BTreeMap::new();
    let mut known: BTreeSet<String> = BTreeSet::new();
    // 每个目录的 mode 来自它自己的 W 记录（§8：只同步 owner 可执行位）。
    let mut dir_mode: BTreeMap<String, u32> = BTreeMap::new();

    for (path, e) in &snap.entries {
        let (dir, name) = split(path);
        ensure_dir(&mut dirs, &mut known, dir);
        if e.kind == Kind::Dir {
            ensure_dir(&mut dirs, &mut known, path);
            dir_mode.insert(path.clone(), e.mode);
            continue;
        }
        let hash = sink.blob_of(db, pid, path, e, built)?;
        dirs.entry(dir.to_string())
            .or_default()
            .push(object::TreeEntry {
                name: name.to_string(),
                kind: e.kind,
                hash,
                size: e.size, // 目录/链接的逻辑大小恒 0，定义在 `worktree::logical_size`
                mode: e.mode,
            });
    }

    // 逆字典序 = 先子后父（"a" < "a/b"），所以子目录的 tree hash 一定先算好。
    let mut tree_of: BTreeMap<String, Hash> = BTreeMap::new();
    for dir in known.iter().rev() {
        let mut list = dirs.remove(dir).unwrap_or_default();
        list.extend(collect_children(&known, &tree_of, &dir_mode, dir)?);
        let hash = sink.tree_of(db, pid, &list)?;
        tree_of.insert(dir.clone(), hash);
    }
    Ok(tree_of.get("").copied().unwrap_or_else(object::empty_tree))
}

/// 分页读 `W` 前缀，只保留**最新一轮扫描**盖过章的行。
///
/// **必须分页**：引擎的 `scan` 返回物化 `Vec`，一次全表扫在 100 万条下是 150–250 MB
/// （§9.6.1）。
///
/// epoch 的取法就是"最大 `scanned_at`"：扫描器给本轮看见的每一行盖同一个数，包括那些
/// 内容没变、只续了期的行（见 `worktree` 模块头）。于是 `scanned_at != epoch` 只有一个
/// 含义——**上一轮之后就没再被看见**，也就是被删了。这一步不许提前到扫描里做：
/// "没看见 = 删除"必须由拿着上一版 tree 的一方判定，扫描单边判断会违反 §8。
pub fn snapshot(db: &dyn KvEngine, pid: ProjectId, max: usize) -> Result<Snapshot> {
    let prefix = keys::project_prefix(keys::TAG_WORKTREE, pid);
    let mut all: Vec<(String, WtEntry)> = Vec::new();
    let mut page = 0usize;
    loop {
        let found = {
            let tx = db.begin();
            let found = tx.search_keys(&SearchQuery::prefix(prefix.clone(), page, PAGE));
            tx.commit();
            found
        };
        for r in found.items {
            let Some((_, path)) = r.key.split_at_checked(prefix.len()) else {
                continue;
            };
            // `value` 只在 `include_deleted(true)` 时才会是 None（tombstone），我们没开。
            let Some(raw) = r.value else { continue };
            if all.len() >= max {
                return Err(Error::TooManyEntries { n: max + 1, max });
            }
            all.push((
                String::from_utf8_lossy(path).into_owned(),
                dec(&raw, "WtEntry")?,
            ));
        }
        if found.total_pages <= page + 1 {
            break;
        }
        page += 1;
    }

    let epoch = all.iter().map(|(_, e)| e.scanned_at).max().unwrap_or(0);
    let known = all.len() as u32;
    let entries = all
        .into_iter()
        .filter(|(_, e)| e.scanned_at == epoch)
        .collect();
    Ok(Snapshot {
        entries,
        epoch,
        known,
    })
}

/// FR-10：删除条目 > 已知条目的 20%，或删除字节 > 2 GB ⇒ 这个 commit 不算数。
///
/// 为什么放在 diff 之后、`put_commit` 之前：删除的**定义**只有一份，就是树比对的结果，
/// 而它同时也是写进 `commit.stats` 的那个数。前面已经落盘的块与目录对象不会被任何 commit
/// 引用，§9.3 的 GC 会回收，所以在这里拒绝依然是安全的。
/// 两侧都要过这道闸（§16.3）：它防的是"客户端 bug 把误 `rm -rf` 变成合法历史"，跟谁发起无关。
///
/// 收的是两个数而不是 `&ChangeSet`：`checkout` 也要用同一道闸，而它手里没有 ChangeSet。
/// **两个调用方共用一个阈值函数**，否则"20% 且不少于 20 条"这个口径会在第二处漂掉。
pub fn check_mass_delete(deleted: u32, deleted_bytes: u64, known: u32, allow: bool) -> Result<()> {
    if allow || deleted == 0 || known == 0 {
        return Ok(());
    }
    // 比例与绝对值**取大**：纯 20% 在小项目上不可用——删 2 个文件里的 1 个就是 50%，
    // 那个仓库从此再也提不了交。熔断要拦的是规模，不是比例。
    let threshold = ((known as f64 * MASS_DELETE_RATIO) as u32).max(MASS_DELETE_MIN);
    if deleted > threshold || deleted_bytes > MASS_DELETE_BYTES {
        return Err(Error::MassDelete {
            deleted,
            known,
            deleted_bytes,
        });
    }
    Ok(())
}

/// 把一个文件切块落进 CAS，并写/复用它的 `B` 清单。返回 blob hash。
fn put_blob(
    db: &dyn KvEngine,
    pid: ProjectId,
    store: &cas::Cas,
    work_root: &Path,
    path: &str,
    e: &WtEntry,
    built: &mut Built,
) -> Result<Hash> {
    let abs = work_root.join(path.replace('/', std::path::MAIN_SEPARATOR_STR));
    let outcome = store.put_file(&abs)?;
    if outcome.blob.hash != e.blob_hash {
        // 扫描之后文件又被人改了：这一条本轮不提交，等下一轮扫描。绝不静默用新内容。
        return Err(Error::StaleScan {
            path: path.to_string(),
        });
    }
    record_blob(db, pid, &outcome, path, built)?;
    Ok(e.blob_hash)
}

/// 把符号链接的**目标文本**当 blob 落进 CAS。
///
/// 为什么用 blob 而不是给 `TreeEntry` 加一个 `target` 字段：加字段 = 改编码 = 已存历史的
/// tree hash 全体作废，而那时两台机器都已经有数据了（M1 之后代价就变成迁移）。目标文本
/// 本身就是一个几十字节的小对象，当 blob 存正好复用去重与 refcnt，git 也是这么做的。
fn put_link(
    db: &dyn KvEngine,
    pid: ProjectId,
    store: &cas::Cas,
    path: &str,
    e: &WtEntry,
    built: &mut Built,
) -> Result<Hash> {
    let target = e.symlink_target.as_deref().ok_or_else(|| {
        Error::Codec(format!(
            "{path} 标成符号链接却没有目标文本：W 记录不完整，拒绝写进历史"
        ))
    })?;
    let outcome = store.put_bytes(target.as_bytes())?;
    if outcome.blob.hash != e.blob_hash {
        return Err(Error::StaleScan {
            path: path.to_string(),
        });
    }
    record_blob(db, pid, &outcome, path, built)?;
    Ok(e.blob_hash)
}

/// 把一个已经落盘的 blob 记进库：`B` 清单 + 每块引用数。**文件和符号链接共用这一条**。
///
/// 已存在的 blob 直接返回：清单是内容寻址的，重写一遍只会多长一个版本行（§9.6.2），
/// 而引用数再加一次就会凭空虚高——虚高的 refcnt 让 GC 永远删不掉那块（不变量 (b) 的
/// 反面），所以这个"先看后写"不是优化，是正确性。
fn record_blob(
    db: &dyn KvEngine,
    pid: ProjectId,
    outcome: &cas::PutOutcome,
    what: &str,
    built: &mut Built,
) -> Result<()> {
    let key = keys::blob_key(pid, &outcome.blob.hash);
    let is_new_blob = {
        let tx = db.begin();
        let missing = tx.get(&key).is_none();
        tx.commit();
        missing
    };
    if !is_new_blob {
        return Ok(()); // 清单已在库里，块引用数不该再加一次
    }

    built.blobs_new += 1;
    built.blocks_stored += outcome.stored;
    built.block_bytes += outcome.stored_bytes;

    let tx = db.begin();
    if !tx.set(&key, keys::wrap(enc(&outcome.blob, "Blob")?)) {
        return Err(Error::WriteConflict(what.to_string()));
    }
    tx.commit();

    // 全零块不落盘，所以也不记引用数：给 `[00;32]` 记一条 `K` 会让 GC 去找一个永不存在的文件。
    // 同一文件里重复出现的块各记一次——GC 也是按清单里的引用逐条减的。
    for b in outcome.blob.blocks.iter().filter(|b| !b.zero) {
        bump_block(db, pid, b.hash, b.size)?;
    }
    Ok(())
}

/// 一个块被新 blob 引用一次：`K.refcnt += 1`，并同步 `k` 排序索引（不变量 (g)）。
fn bump_block(db: &dyn KvEngine, pid: ProjectId, hash: Hash, size: u32) -> Result<()> {
    let kkey = keys::block_key(pid, &hash);
    let prev = {
        let tx = db.begin();
        let got = tx.get(&kkey).map(|raw| dec::<BlockMeta>(&raw, "BlockMeta"));
        tx.commit();
        got.transpose()?
    };

    let (old_refcnt, meta) = match prev {
        Some(m) => (
            m.refcnt,
            BlockMeta {
                size: m.size,
                refcnt: m.refcnt + 1,
            },
        ),
        None => (0, BlockMeta { size, refcnt: 1 }),
    };
    let body = keys::wrap(postcard::to_allocvec(&meta).map_err(|e| Error::Codec(e.to_string()))?);

    let tx = db.begin();
    if !tx.set(&kkey, body) {
        return Err(Error::WriteConflict(keys::short(&hash)));
    }
    if old_refcnt > 0 {
        // 旧计数档位的索引行必须删掉，否则 GC 会在旧档位上看见一个已经不存在的引用数。
        tx.delete(&keys::refcnt_key(pid, old_refcnt, &hash));
    }
    if !tx.set(&keys::refcnt_key(pid, meta.refcnt, &hash), Vec::new()) {
        return Err(Error::WriteConflict(keys::short(&hash)));
    }
    tx.commit();
    Ok(())
}

/// 配对遍历的回调类型。收成别名有两个理由：三处签名要重复它，而且 clippy 说得对——
/// 这个函数指针类型已经难读到"看不出参数顺序"了，而顺序错一个是静默的语义错误。
///
/// 生命周期标在 `+ 'a` 而**不是**标在参数上：参数里那些引用每次调用都换一批，那需要
/// `for<'b>`（写成省略形式就是编译器自己给的高阶约束）；`'a` 管的是闭包本身能活多久。
/// 反过来写（`&'a str` 挂在参数上）会把闭包逼成 `'static`，借用局部计数器直接编译不过。
pub type Visit<'a> = dyn FnMut(ChangeKind, &str, Option<&object::TreeEntry>, Option<&object::TreeEntry>) -> Result<()>
    + 'a;

/// 两棵树的**配对遍历**：全站只有这一份。
///
/// 为什么不各写一份：`diff`（只数数）和 `checkout`（要真落地）问的是同一个问题——这一层里
/// 谁多了、谁少了、谁变了、哪些子树可以整块跳过。分两处写，"跳过未变子树"这类判断就会
/// 在两处各错一次，而 `checkout` 错的那一次是**删文件**。
///
/// 回调里 `old`/`new` 谁是 `None` 由 `kind` 决定：`Add` 没有 old，`Delete` 没有 new。
/// 目录的 `Add`/`Delete` 先回调目录本身、再带着空的一侧递归进去，所以整棵子树的每个后代
/// 都会被记一次——与 `ChangeSet` 的计数口径一致。
///
/// `prefix` 是为了回调能拿到**可直接喂给 `restore`/`checkout` 的完整路径**。
pub fn walk_pair(
    db: &dyn KvEngine,
    pid: ProjectId,
    trees: &TreeStore,
    prefix: &str,
    old: &[object::TreeEntry],
    new: &[object::TreeEntry],
    visit: &mut Visit<'_>,
) -> Result<()> {
    let o: BTreeMap<&str, &object::TreeEntry> = old.iter().map(|e| (e.name.as_str(), e)).collect();
    let n: BTreeMap<&str, &object::TreeEntry> = new.iter().map(|e| (e.name.as_str(), e)).collect();

    for (name, ne) in &n {
        let full = join(prefix, name);
        let Some(oe) = o.get(*name) else {
            visit(ChangeKind::Add, &full, None, Some(ne))?;
            if ne.kind == Kind::Dir {
                let kids = trees.entries(db, pid, &ne.hash)?;
                walk_pair(db, pid, trees, &full, &[], &kids, visit)?;
            }
            continue;
        };
        if oe.hash == ne.hash && oe.mode == ne.mode {
            continue; // 子树 hash 相同 ⇒ 整块跳过，一个条目都不读（NFR-6）
        }
        if oe.kind == Kind::Dir && ne.kind == Kind::Dir {
            let kids_old = object::read_tree(db, pid, &oe.hash)?;
            let kids_new = trees.entries(db, pid, &ne.hash)?;
            walk_pair(db, pid, trees, &full, &kids_old, &kids_new, visit)?;
        } else {
            visit(ChangeKind::Modify, &full, Some(oe), Some(ne))?;
        }
    }
    // 判删除只能问"这个名字还在不在新清单里"，不能问"刚才有没有被我碰过"：
    // 未变更的条目上一轮就 continue 了，拿那条线筛会把好文件报成删除。
    for oe in o.values().filter(|e| !n.contains_key(&e.name.as_str())) {
        let full = join(prefix, &oe.name);
        // **先递归再回调自己**（后序）：`checkout` 要在同一趟流式遍历里 `remove_dir`，
        // 而目录只有空了才删得掉。计数不受顺序影响，所以 `diff` 那边没有代价。
        if oe.kind == Kind::Dir {
            let kids = object::read_tree(db, pid, &oe.hash)?;
            walk_pair(db, pid, trees, &full, &kids, &[], visit)?;
        }
        visit(ChangeKind::Delete, &full, Some(oe), None)?;
    }
    Ok(())
}

/// 递归比对两棵树。**子树 hash 相同就整块跳过**——这就是 NFR-6 的 O(变更目录数)。
///
/// `pub` 是给 `diff --at <rev>` 用的：那条要拿任意两个版本比，不只是跟 HEAD 比。
pub fn diff(
    db: &dyn KvEngine,
    pid: ProjectId,
    old: Option<&Hash>,
    new: &Hash,
    new_trees: &TreeStore,
) -> Result<ChangeSet> {
    let mut out = ChangeSet::default();
    if old == Some(new) || (old.is_none() && *new == object::empty_tree()) {
        return Ok(out);
    }
    let old_list = match old {
        Some(h) => object::read_tree(db, pid, h)?,
        None => Vec::new(),
    };
    let new_list = new_trees.entries(db, pid, new)?;
    let mut collect = |kind: ChangeKind,
                       path: &str,
                       oe: Option<&object::TreeEntry>,
                       ne: Option<&object::TreeEntry>|
     -> Result<()> {
        // 只有文件有 blob：目录的 hash 是 tree，符号链接的 hash 是目标文本的 blob。
        if kind == ChangeKind::Delete
            && let Some(e) = oe
            && e.kind == Kind::File
        {
            out.deleted_blobs.push(e.hash);
        }
        let size = ne.or(oe).map_or(0, |e| e.size);
        out.note(kind, path.to_string(), size);
        Ok(())
    };
    walk_pair(db, pid, new_trees, "", &old_list, &new_list, &mut collect)?;
    Ok(out)
}

fn ensure_dir(
    dirs: &mut BTreeMap<String, Vec<object::TreeEntry>>,
    known: &mut BTreeSet<String>,
    dir: &str,
) {
    dirs.entry(dir.to_string()).or_default();
    let mut cur = dir;
    loop {
        known.insert(cur.to_string());
        match cur.rsplit_once('/') {
            Some((parent, _)) => cur = parent,
            None => break,
        }
    }
}

/// 把 `dir` 的直接子目录打包成条目（子目录的 tree hash 已经算好了）。
///
/// 找不到某个子目录的 hash 就**报错**，不许跳过：跳过 = 那棵子树从历史里静消失，
/// 而调用方是按"逆序一定算好了"这个前提写的（见 `build` 里那圈 `known.iter().rev()`）。
fn collect_children(
    known: &BTreeSet<String>,
    tree_of: &BTreeMap<String, Hash>,
    dir_mode: &BTreeMap<String, u32>,
    dir: &str,
) -> Result<Vec<object::TreeEntry>> {
    let mut kids = Vec::new();
    for candidate in known {
        // 只能用 `split`，不能用 `split_opt`：顶层目录（"sub"）里没有 `/`，`split_opt`
        // 返回 None 就被 `continue` 掉了 ⇒ 整个顶层目录连同它下面的一切**静默消失**在历史里。
        let (parent, name) = split(candidate);
        if parent != dir || name.is_empty() {
            continue;
        }
        let hash = *tree_of.get(candidate).ok_or_else(|| {
            Error::Codec(format!(
                "子目录 {candidate} 的 tree 还没算出来：逆序聚合的前提被破坏"
            ))
        })?;
        kids.push(object::TreeEntry {
            name: name.to_string(),
            kind: Kind::Dir,
            hash,
            size: 0,
            mode: dir_mode.get(candidate).copied().unwrap_or(0o40755),
        });
    }
    Ok(kids)
}

fn split(path: &str) -> (&str, &str) {
    split_opt(path).unwrap_or(("", path))
}

/// 目录前缀 + 条目名 → 完整路径（`/` 分隔，与 `W` 的键尾一致）。
fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

fn split_opt(path: &str) -> Option<(&str, &str)> {
    let (dir, name) = path.rsplit_once('/')?;
    if name.is_empty() {
        return None;
    }
    Some((dir, name))
}

/// `main` 当前指向的 commit 集合。
///
/// 注意：这里**不存 `is_head` 标记**（§9.2 草案里的 `is_head` 已废弃，见 §17）。
/// 多 head 时真正的 head 集合 = 这些 commit 里没有后代的那些，那个可达性过滤是 M3
/// union 的活；现在 `set_ref` 只会写进一个 commit，所以集合大小恒为 1，过滤无从触发。
pub fn ref_heads(db: &dyn KvEngine, pid: ProjectId, r: &str) -> Result<Vec<Hash>> {
    let mut prefix = vec![keys::TAG_HEAD];
    prefix.extend_from_slice(&pid.to_be_bytes());
    prefix.extend_from_slice(r.as_bytes());
    prefix.push(0x00);
    let tx = db.begin();
    let rows = tx.prefix_scan(&prefix);
    tx.commit();
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let key = &row.key[prefix.len()..];
        if key.len() != 32 {
            continue;
        }
        let mut h = [0u8; 32];
        h.copy_from_slice(key);
        out.push(h);
    }
    Ok(out)
}

/// 把 ref 设成给定集合：先删旧行再写新行，同一事务。
pub fn set_ref(db: &dyn KvEngine, pid: ProjectId, r: &str, to: &[Hash]) -> Result<()> {
    let old = ref_heads(db, pid, r)?;
    let tx = db.begin();
    for h in &old {
        tx.delete(&keys::head_key(pid, r, h));
    }
    for h in to {
        if !tx.set(&keys::head_key(pid, r, h), keys::wrap(vec![1u8])) {
            return Err(Error::WriteConflict(keys::short(h)));
        }
    }
    tx.commit();
    Ok(())
}

/// 读一个 blob 清单（`restore`/`checkout`/§10 不变量 (b) 用）。
pub fn read_blob(db: &dyn KvEngine, pid: ProjectId, hash: &Hash) -> Result<cas::Blob> {
    let key = keys::blob_key(pid, hash);
    let raw = {
        let tx = db.begin();
        // 先把结果算出来、再 commit、最后 `?`。漏掉 commit 不会漏事务（引擎 0.4.0 的
        // `impl Drop for Transaction` 会自动回滚并从 active_txn 里摘掉自己），但每次
        // 取不到都要留一条"未 commit 即被丢弃"的 warn，读路径不该制造这种噪音。
        let got = tx.get(&key).ok_or_else(|| Error::MissingObject {
            hash: keys::short(hash),
            kind: "blob".to_string(),
        });
        tx.commit();
        got?
    };
    dec(&raw, "Blob")
}
