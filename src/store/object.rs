//! 对象层：`tree` / `commit` 的规范编码与读写（§9.2 的 `T` / `C` / `c` 三类键）。
//!
//! # 规范编码（跨平台必须一致，否则两台机器算出不同的 hash）
//!
//! - 目录条目**按 `name` 的字节序升序**排列，重名直接拒（重名两条 = 上游 bug，静默保留会
//!   让同一棵树有两个 hash）。
//! - 数值一律 BE（`keys` 模块的约定），字符串走 UTF-8 字节。
//! - `hash` 覆盖**含 parents 的完整 commit 编码**；存储时把 parents 另拆到 `c` 前缀，
//!   是为了反向遍历（`log`、可达性判定）不必先解整个 commit 体。拆出去的是索引，
//!   不是第二真值——两者必须同事务写入，用例 `parents_index_matches_commit_body` 钉这条。
//!
//! # 幂等与"别重复写"
//!
//! 对象是内容寻址的，重写同一条内容结果一样。但**重复 `set` 会在 MVCC 里多长一个版本行**，
//! 10 万目录 × 1000 个 commit 就是 1 亿行垃圾。所以 `put_*` 先查后写；这一跳 `get` 在
//! diff 层会被"已知集合"替代（引擎点查目前 0.86 ms/次，见 §17 补记）。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use storage_engines::KvEngine;

use crate::error::{Error, Result};
use crate::store::keys::{self, Hash, Kind, ProjectId};

/// 目录里的一条。`hash` 的含义随 `kind` 变：
/// File → blob hash；Dir → 子目录的 tree hash；Symlink → 目标路径的 hash。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TreeEntry {
    pub name: String,
    pub kind: Kind,
    pub hash: Hash,
    /// 逻辑字节数：目录与符号链接恒为 0（定义见 `worktree::logical_size`）。
    pub size: u64,
    /// Unix 模式位。§8 只同步 owner 可执行位，但它**进 hash**——
    /// 否则改完权限两台机器会算出同一棵树，可执行位就永远同步不掉。
    pub mode: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Stats {
    pub add: u32,
    pub modify: u32,
    pub delete: u32,
    pub rename: u32,
    pub bytes: u64,
}

impl Stats {
    pub fn is_empty(&self) -> bool {
        self.add + self.modify + self.delete + self.rename == 0
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    /// 0 = 初始提交；1 = 常态；≥2 = `--keep-both` 的结构合并（§7.1）。
    pub parents: Vec<Hash>,
    pub tree: Hash,
    /// 产生该提交的 peer id。**只是元数据，不签名**（§9.5.2 / §15.7）。
    pub peer: String,
    /// 只用于显示与保留策略，**不参与任何裁决**（§4）。
    pub ts: i64,
    pub msg: String,
    pub stats: Stats,
}

fn conflict(key: &[u8]) -> Error {
    Error::WriteConflict(String::from_utf8_lossy(key).into_owned())
}

/// 规范字节：**不加**标签前缀。hash 必须覆盖内容本身，不能把我们的存储信封算进去。
fn enc<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    postcard::to_allocvec(v).map_err(|e| Error::Codec(format!("对象编码失败：{e}")))
}

fn dec<T: serde::de::DeserializeOwned>(raw: &[u8], what: &str) -> Result<T> {
    postcard::from_bytes(keys::strip(raw)?)
        .map_err(|e| Error::Codec(format!("{what} 解码失败：{e}")))
}

/// 空目录的 hash。所有空目录共享这一条，不需要落盘。
pub fn empty_tree() -> Hash {
    tree_hash(&[])
}

pub fn tree_hash(entries: &[TreeEntry]) -> Hash {
    keys::hash_of(&canonical(entries).expect("空清单不可能重名"))
}

/// 排序 + 重名校验，产出用于 hash 的字节。**hash 与落盘必须用同一份字节**，
/// 否则读回来的对象 hash 对不上自己。
fn canonical(entries: &[TreeEntry]) -> Result<Vec<u8>> {
    let mut sorted = entries.to_vec();
    sorted.sort();
    // 重名时报出**是哪个名字**：只说"有重名"的话，下一个人只能从头把遍历打印一遍
    // （2026-10-17 定位一条随机序列里的重名条目就是这样）。
    if let Some(w) = sorted.windows(2).find(|w| w[0].name == w[1].name) {
        return Err(Error::Codec(format!(
            "目录清单里有重名条目：{}（该层 {} 条，两条分别是 {:?} 与 {:?}）",
            w[0].name,
            sorted.len(),
            w[0].kind,
            w[1].kind
        )));
    }
    enc(&sorted)
}

/// 写一个目录对象，返回它的 hash。已存在则不重复写。
pub fn put_tree(db: &dyn KvEngine, pid: ProjectId, entries: &[TreeEntry]) -> Result<Hash> {
    // 空目录不落盘：`read_tree` 对 `empty_tree()` 有特判，写了就是永远读不到的死行。
    if entries.is_empty() {
        return Ok(empty_tree());
    }
    let bytes = canonical(entries)?;
    let hash = keys::hash_of(&bytes);
    let key = keys::tree_key(pid, &hash);
    let tx = db.begin();
    // 内容寻址：已有即等价。早返回，不产生第二个版本行（见模块头那条垃圾账）。
    if tx.get(&key).is_some() {
        tx.commit();
        return Ok(hash);
    }
    if !tx.set(&key, keys::wrap(bytes)) {
        return Err(conflict(&key));
    }
    tx.commit();
    Ok(hash)
}

pub fn read_tree(db: &dyn KvEngine, pid: ProjectId, hash: &Hash) -> Result<Vec<TreeEntry>> {
    if *hash == empty_tree() {
        return Ok(Vec::new());
    }
    let key = keys::tree_key(pid, hash);
    let tx = db.begin();
    let raw = tx.get(&key).ok_or_else(|| Error::MissingObject {
        hash: keys::short(hash),
        kind: "tree".to_string(),
    })?;
    tx.commit();
    let entries: Vec<TreeEntry> = dec(&raw, "tree")?;
    // 自证：读回来的清单重算 hash 必须等于键里的 hash（§9.4.7 在对象层的对应物）。
    if tree_hash(&entries) != *hash {
        return Err(Error::CorruptBlock {
            hash: keys::short(hash),
            path: String::from_utf8_lossy(&key).into_owned().into(),
            why: "tree 内容与它的 hash 不符".to_string(),
        });
    }
    Ok(entries)
}

/// 写一个 commit：`C` 存完整体，`c` 存 parents 索引，**同一个事务里一起写**。
pub fn put_commit(db: &dyn KvEngine, pid: ProjectId, c: &Commit) -> Result<Hash> {
    let body = enc(c)?;
    let hash = keys::hash_of(&body);
    let key = keys::commit_key(pid, &hash);
    let tx = db.begin();
    if tx.get(&key).is_some() {
        tx.commit();
        return Ok(hash);
    }
    if !tx.set(&key, keys::wrap(body)) {
        return Err(conflict(&key));
    }
    for (n, p) in c.parents.iter().enumerate() {
        let pk = keys::parent_key(pid, &hash, n as u32);
        // 同一个事务里写索引：分开写就会出现「C 有 parents 而 c 索引缺一半」的中间态。
        if !tx.set(&pk, keys::wrap(p.to_vec())) {
            return Err(conflict(&pk));
        }
    }
    tx.commit();
    Ok(hash)
}

pub fn read_commit(db: &dyn KvEngine, pid: ProjectId, hash: &Hash) -> Result<Commit> {
    let key = keys::commit_key(pid, hash);
    let tx = db.begin();
    let raw = tx.get(&key).ok_or_else(|| Error::MissingObject {
        hash: keys::short(hash),
        kind: "commit".to_string(),
    })?;
    tx.commit();
    let c: Commit = dec(&raw, "commit")?;
    if keys::hash_of(&enc(&c)?) != *hash {
        return Err(Error::CorruptBlock {
            hash: keys::short(hash),
            path: String::from_utf8_lossy(&key).into_owned().into(),
            why: "commit 内容与它的 hash 不符".to_string(),
        });
    }
    Ok(c)
}

/// 走 `c` 索引取 parents：按 ordinal 升序，顺序就是 parents 的顺序。
pub fn parents_of(db: &dyn KvEngine, pid: ProjectId, hash: &Hash) -> Result<Vec<Hash>> {
    let prefix = {
        let mut p = vec![keys::TAG_PARENT];
        p.extend_from_slice(&pid.to_be_bytes());
        p.extend_from_slice(hash);
        p
    };
    let tx = db.begin();
    let rows = tx.prefix_scan(&prefix);
    tx.commit();
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(dec::<Hash>(&r.value.unwrap_or_default(), "parent")?);
    }
    Ok(out)
}

/// 本项目的**整个亲代索引**：`child → [parent…]`，**一次** `c|p` 前缀扫装进内存。
///
/// 为什么要它（§15.22、§17 补记廿五）：门面**每次读调用都折一遍全库**（§9.6.1），而历史遍历
/// 原来每走一步就 `parents_of` 一次 ⇒ 一次 0 字节的推送在 12 GiB 站点上要 ~90 次折 ≈ 130 s。
/// 亲代索引是整个 push/log/GC 路径里**唯一既小又常被整份需要**的索引：一行 = 32 B 键尾 + 32 B 值，
/// 十万个提交也只有 ~10 MB，而换来的是"一次折代替 N 次折"。
///
/// 内存形状要如实说：这是 `O(提交数)`。百万提交级别的站点要改成按 head 分段加载（记进 §15）。
pub fn parent_index(db: &dyn KvEngine, pid: ProjectId) -> Result<BTreeMap<Hash, Vec<Hash>>> {
    let mut prefix = vec![keys::TAG_PARENT];
    prefix.extend_from_slice(&pid.to_be_bytes());
    let tx = db.begin();
    let rows = tx.prefix_scan(&prefix);
    tx.commit();
    let mut out: BTreeMap<Hash, Vec<Hash>> = BTreeMap::new();
    for r in rows {
        // 键尾是 `child(32) + ordinal(4)`； ordinal 的存在就是为了一个提交能有多个亲代（§7.1 合并）。
        let tail = match r.key.get(prefix.len()..) {
            Some(t) if t.len() == 36 => t,
            _ => continue,
        };
        let mut child = [0u8; 32];
        child.copy_from_slice(&tail[..32]);
        // ordinal 决定同一 child 下亲代的先后（0 = 第一个亲代，合并提交里那是"主亲代"）。
        let ord = u32::from_be_bytes([tail[32], tail[33], tail[34], tail[35]]);
        let Some(v) = &r.value else { continue };
        let parent = dec::<Hash>(v, "parent")?;
        let slot = out.entry(child).or_default();
        while slot.len() <= ord as usize {
            slot.push([0u8; 32]); // 占位，保证下标就是 ordinal（缺号不会静默挪位）
        }
        slot[ord as usize] = parent;
    }
    for (_, list) in out.iter_mut() {
        // 去掉占位：上面那步可能让 `slot[..k]` 留空（例如索引被半路删过——那是不变量 (g) 的病，
        // 这里不掩盖，但也不把占位当亲代返回）。
        list.retain(|h| *h != [0u8; 32]);
    }
    Ok(out)
}

/// 一张目录清单的**批量装载器**：`tree hash → 条目`，一次 `T|p` 前缀扫换掉 O(目录数) 次点读。
///
/// 为什么要它（§11.4 那张折数账、§17 补记廿九）：门面每次读调用折一遍全库（§9.6.1），而
/// `plan`/`status`/`checkout`/`gc --sweep` 都要"逐目录读参照清单"⇒ 12 GiB 站点上 204 个目录
/// 就是 209 次折 ≈ release 100 s。整段装载把它换成 **1 次折**。
///
/// **代价必须写在这里**：前缀扫会把命中的 value 全物化。12 GiB 站点实测（§11.4d）
/// `T|p` = 211 行 / value 0.88 MiB / 清单条目 20 534 ⇒ 解成 `TreeEntry` 之后大约再翻一倍。
/// 行数随**历史**增长（改过的目录留一张新清单，没变的目录靠内容寻址复用同一张），所以有预算：
/// value 超过 [`TreeIndex::BUDGET_BYTES`] 就**不再整段留**，[`TreeIndex::list`] 自动退回逐条点读
/// （= 老行为，慢但不吃内存）。这一条不是"防御性编程"：本项目的硬约束是 RSS ≤ 80 MB
/// （NFR-1），而这个装载器唯一的失控维度就是历史长度。
#[derive(Debug)]
pub struct TreeIndex {
    map: BTreeMap<Hash, Vec<TreeEntry>>,
    /// 整段扫搬回来的 value 字节数：判预算用，也是"这一次装载到底多大"的读数。
    bytes: u64,
    rows: usize,
    /// true = 超预算 ⇒ `list` 逐条点读。装载本身不报错：退回的那条路只是慢，不是坏。
    over: bool,
}

impl TreeIndex {
    /// 整段装载的预算，按 value 字节算：**4 MiB**。为什么是这个数（都是 12 GiB 站点实测/推算，§11.4d）：
    /// `T|p` 的 value 实测 0.88 MiB / 211 行 / 20 534 条条目，而解成 `TreeEntry` 之后每条是
    /// `String(24 B 头) + hash(32) + size(8) + mode(4) + kind` ≈ 82 B + 名字堆 ⇒ **约 2.1× 放大**，
    /// 也就是这一站解出来 ≈ 1.9 MB。4 MiB 的 value 预算 ⇒ 解出来 ≈ 9 MB，压在 NFR-1 的
    /// "扫描峰值 ≤ 8 MiB"同一量级上（这条命令不是 scan，但它和 scan 抢的是同一个 80 MB RSS）。
    /// 余量 4.5×；超出去就退回点读——**慢，但不吃内存**，而那才是 NFR-1 要的保护。
    pub const BUDGET_BYTES: u64 = 4 << 20;

    /// 一次前缀扫把 `T|p` 装进来，预算取 [`TreeIndex::BUDGET_BYTES`]。
    pub fn load(db: &dyn KvEngine, pid: ProjectId) -> Result<Self> {
        Self::with_budget(db, pid, Self::BUDGET_BYTES)
    }

    /// 预算可注入的那一份 —— 用例要能演出"超预算退回点读"这一支，而不必造 4 MiB 的历史
    /// （与 `build::plan_capped` 对 `MAX_DIR_ENTRIES` 同一个做法：**上限是参数，不是魔法数**）。
    ///
    /// **每行都自证**：解出的清单重算 hash 必须等于键尾那 32 字节 —— 批量路径不许比
    /// [`read_tree`] 少一次校验，否则 §9.4.7 那条担保就只在点读那一边成立。
    pub fn with_budget(db: &dyn KvEngine, pid: ProjectId, budget_bytes: u64) -> Result<Self> {
        let mut prefix = vec![keys::TAG_TREE];
        prefix.extend_from_slice(&pid.to_be_bytes());
        let tx = db.begin();
        let rows = tx.prefix_scan(&prefix);
        tx.commit();
        let mut out = Self {
            map: BTreeMap::new(),
            bytes: 0,
            rows: rows.len(),
            over: false,
        };
        for r in rows {
            let Some(v) = r.value.as_ref() else { continue };
            out.bytes += v.len() as u64;
            if out.bytes > budget_bytes {
                // 超预算：把已经解出来的部分立刻交还，后面全靠点读。留着它才是违背 NFR-1。
                out.map = BTreeMap::new();
                out.over = true;
                continue;
            }
            // 键尾就是 hash；形状不对（被别的 tag 撞进来、或半截键）就跳过而不是当成损坏 ——
            // 这一条与 `parent_index` 一致：装载器只认它自己那一段。
            let Some(tail) = r.key.get(prefix.len()..) else { continue };
            if tail.len() != 32 {
                continue;
            }
            let mut hash = [0u8; 32];
            hash.copy_from_slice(tail);
            let entries: Vec<TreeEntry> = dec(v, "tree")?;
            if tree_hash(&entries) != hash {
                return Err(Error::CorruptBlock {
                    hash: keys::short(&hash),
                    path: String::from_utf8_lossy(&r.key).into_owned().into(),
                    why: "tree 内容与它的 hash 不符（批量装载这一路）".to_string(),
                });
            }
            out.map.insert(hash, entries);
        }
        Ok(out)
    }

    /// 拿一张清单。`empty_tree()` 恒空（`put_tree` 不落盘，见那里的注释）；预算内查表 =
    /// **零引擎调用**；超预算退回点读（带它自己的自证）。
    pub fn list(&self, db: &dyn KvEngine, pid: ProjectId, hash: &Hash) -> Result<Vec<TreeEntry>> {
        if *hash == empty_tree() {
            return Ok(Vec::new());
        }
        if self.over {
            return read_tree(db, pid, hash);
        }
        self.map.get(hash).cloned().ok_or_else(|| Error::MissingObject {
            hash: keys::short(hash),
            kind: "tree".to_string(),
        })
    }

    /// 装进来的清单张数（**不等于** `rows`：超预算时表是空的，而 `rows` 还是真实行数）。
    pub fn loaded(&self) -> usize {
        self.map.len()
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// 命令层必须把它印出来（"这次是整段装载还是退回点读"），不许静默换路。
    pub fn over_budget(&self) -> bool {
        self.over
    }
}

/// 同一层里**折叠后相等**的两个名字（`README.md` 与 `Readme.md`）。给第一对，按字节序确定。
///
/// 只在目标盘大小写不敏感时才是错误（§8：报错拒绝，绝不静默覆盖）。在大小写敏感的文件系统
/// 上这两个名字是合法的两个文件，**历史必须容得下它们**——所以这一条不是扫描/提交的门，
/// 而是**检出的门**（见 `checkout::assert_fold_safe`）。
///
/// 内存 = 一层的条目数：清单本来就在手里（`read_tree` 返回的就是整层），没有多留一份。
pub fn fold_collision(entries: &[TreeEntry]) -> Option<(String, String)> {
    let mut seen: BTreeMap<String, &str> = BTreeMap::new();
    for e in entries {
        let folded = keys::fold_path(&e.name);
        let hit = seen.insert(folded, e.name.as_str());
        if let Some(prev) = hit
            && prev != e.name
        {
            return Some((prev.to_string(), e.name.clone()));
        }
    }
    None
}
