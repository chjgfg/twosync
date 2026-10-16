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
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use storage_engines::KvEngine;
use storage_engines::SearchQuery;

use crate::error::{Error, Result};
use crate::store::keys::{self, Hash, Kind, ProjectId};
use crate::store::worktree::WtEntry;
use crate::store::{cas, object};

/// 一次最多聚合多少条目。超过就拒绝，不 OOM。
pub const MAX_ENTRIES: usize = 200_000;
/// 分页读引擎的窗口。
pub const PAGE: usize = 2_000;
/// v1 唯一参与同步的 ref。
pub const MAIN: &str = "main";
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
}

impl ChangeSet {
    pub fn is_empty(&self) -> bool {
        self.add + self.modify + self.delete == 0
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
    let prev_tree = match prev_heads.first() {
        Some(h) => Some(object::read_commit(db, pid, h)?.tree),
        None => None,
    };

    let mut built = Built {
        entries: snap.entries.len(),
        epoch: snap.epoch,
        known: snap.known,
        ..Default::default()
    };
    let mut dirs: BTreeMap<String, Vec<object::TreeEntry>> = BTreeMap::new();
    let mut known: BTreeSet<String> = BTreeSet::new();
    // 每个目录的 mode 来自它自己的 W 记录（§8：只同步 owner 可执行位）。
    let mut dir_mode: BTreeMap<String, u32> = BTreeMap::new();

    for (path, e) in &snap.entries {
        let (dir, name) = split(path);
        ensure_dir(&mut dirs, &mut known, dir);
        match e.kind {
            Kind::Dir => {
                ensure_dir(&mut dirs, &mut known, path);
                dir_mode.insert(path.clone(), e.mode);
            }
            Kind::File => {
                let blob = put_blob(db, pid, store, work_root, path, e, &mut built)?;
                dirs.entry(dir.to_string())
                    .or_default()
                    .push(object::TreeEntry {
                        name: name.to_string(),
                        kind: Kind::File,
                        hash: blob,
                        size: e.size,
                        mode: e.mode,
                    });
            }
            Kind::Symlink => {
                // 目标文本本身必须进 CAS，否则树里那个 hash 永远解不回文本（§17 补记三）。
                // 走 blob 而不是给 TreeEntry 加字段：git 也是这么做的，且**不动编码** ⇒
                // 已存的历史不会作废。链接目标很短，`put_bytes` 的物化代价可以忽略。
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
                record_blob(db, pid, &outcome, path, &mut built)?;
                dirs.entry(dir.to_string())
                    .or_default()
                    .push(object::TreeEntry {
                        name: name.to_string(),
                        kind: Kind::Symlink,
                        hash: e.blob_hash,
                        size: 0,
                        mode: e.mode,
                    });
            }
        }
    }

    // 逆字典序 = 先子后父（"a" < "a/b"），所以子目录的 tree hash 一定先算好。
    let mut tree_of: BTreeMap<String, Hash> = BTreeMap::new();
    for dir in known.iter().rev() {
        let mut list = dirs.remove(dir).unwrap_or_default();
        list.extend(collect_children(&known, &tree_of, &dir_mode, dir)?);
        let hash = object::put_tree(db, pid, &list)?;
        tree_of.insert(dir.clone(), hash);
    }
    built.tree = tree_of.get("").copied().unwrap_or_else(object::empty_tree);

    built.changes = diff(db, pid, prev_tree.as_ref(), &built.tree)?;
    if built.changes.is_empty() {
        return Ok(built); // 不产生空提交：碎 commit 会把引擎版本链白白撑大（§9.6.2）
    }
    check_mass_delete(&built.changes, snap.known, meta.allow_mass_delete)?;

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
pub fn check_mass_delete(cs: &ChangeSet, known: u32, allow: bool) -> Result<()> {
    if allow || cs.delete == 0 || known == 0 {
        return Ok(());
    }
    // 比例与绝对值**取大**：纯 20% 在小项目上不可用——删 2 个文件里的 1 个就是 50%，
    // 那个仓库从此再也提不了交。熔断防的是"误 rm -rf"，那是规模问题不是比例问题。
    let threshold = ((known as f64 * MASS_DELETE_RATIO) as u32).max(MASS_DELETE_MIN);
    let too_many = cs.delete > threshold;
    let too_big = cs.deleted_bytes > MASS_DELETE_BYTES;
    if too_many || too_big {
        return Err(Error::MassDelete {
            deleted: cs.delete,
            known,
            deleted_bytes: cs.deleted_bytes,
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

/// 递归比对两棵树。**子树 hash 相同就整块跳过**——这就是 NFR-6 的 O(变更目录数)。
fn diff(db: &dyn KvEngine, pid: ProjectId, old: Option<&Hash>, new: &Hash) -> Result<ChangeSet> {
    let mut out = ChangeSet::default();
    if old == Some(new) || (old.is_none() && *new == object::empty_tree()) {
        return Ok(out);
    }
    let old_list = match old {
        Some(h) => object::read_tree(db, pid, h)?,
        None => Vec::new(),
    };
    let new_list = object::read_tree(db, pid, new)?;
    walk_dir(db, pid, &old_list, &new_list, &mut out)?;
    Ok(out)
}

/// 同一目录名下一棵子树的全部差异。删除 = 在 `old` 里而**不在** `new` 里的那些名字。
fn walk_dir(
    db: &dyn KvEngine,
    pid: ProjectId,
    old: &[object::TreeEntry],
    new: &[object::TreeEntry],
    out: &mut ChangeSet,
) -> Result<()> {
    let o: BTreeMap<&str, &object::TreeEntry> = old.iter().map(|e| (e.name.as_str(), e)).collect();
    let n: BTreeMap<&str, &object::TreeEntry> = new.iter().map(|e| (e.name.as_str(), e)).collect();

    for (name, ne) in &n {
        let Some(oe) = o.get(*name) else {
            out.add += 1;
            out.bytes += ne.size;
            if ne.kind == Kind::Dir {
                let kids = object::read_tree(db, pid, &ne.hash)?;
                walk_dir(db, pid, &[], &kids, out)?;
            }
            continue;
        };
        if oe.hash == ne.hash && oe.mode == ne.mode {
            continue; // 子树 hash 相同 ⇒ 整块跳过，一个条目都不读（NFR-6）
        }
        if oe.kind == Kind::Dir && ne.kind == Kind::Dir {
            let kids_old = object::read_tree(db, pid, &oe.hash)?;
            let kids_new = object::read_tree(db, pid, &ne.hash)?;
            walk_dir(db, pid, &kids_old, &kids_new, out)?;
        } else {
            out.modify += 1;
            out.bytes += ne.size;
        }
    }
    // 判删除只能问"这个名字还在不在新清单里"，不能问"刚才有没有被我改过"：
    // 未变更的条目上一轮就 continue 了，拿那条线筛会把好文件报成删除。
    for oe in o.values().filter(|e| !n.contains_key(&e.name.as_str())) {
        out.delete += 1;
        out.deleted_bytes += oe.size;
        // 只有文件有 blob：目录的 hash 是 tree，符号链接的 hash 是目标串。
        if oe.kind == Kind::File {
            out.deleted_blobs.push(oe.hash);
        }
        if oe.kind == Kind::Dir {
            let kids = object::read_tree(db, pid, &oe.hash)?;
            walk_dir(db, pid, &kids, &[], out)?;
        }
    }
    Ok(())
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
