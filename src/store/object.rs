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
    if sorted.windows(2).any(|w| w[0].name == w[1].name) {
        return Err(Error::Codec("目录清单里有重名条目".to_string()));
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
