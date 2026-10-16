//! 只读的历史视图：`log` / `show` 的底座，`ls-files` / `restore` / `checkout` 以后也走这里。
//!
//! # 为什么单独一层
//!
//! 这几条命令干的都是同一件麻烦事：**从某个 commit 的根 tree 出发，按路径走到一个条目**。
//! 如果把它写在 `build`（建树那侧）里，读路径就依赖写路径——而读路径必须在 GC 之后、
//! 在对端只推了半个版本树的时候依然能用。§10 不变量 (b)「历史取不回来」最常见的制造方式
//! 就是"读的那条路恰好是写的那条路的一个副产品"。
//!
//! # 内存
//!
//! 这一层全部是**逐条读、常数持有**：`walk` 的堆里只放前沿（线性历史 = 1 个），
//! `resolve` 每次只物化一层目录。唯一 O(历史长度) 的东西是 `seen` 集合，所以 `log`
//! 有 `--max-count`，而"0 = 全部"必须写成显式选项而不是默认（十万历史 × 32 B 也要 3 MB）。

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap, HashSet};

use storage_engines::{KvEngine, SearchQuery};

use crate::error::{Error, Result};
use crate::store::keys::{self, Hash, Kind, ProjectId};
use crate::store::{build, object};

/// 一个 commit 连同它的身份。`log`/`show` 都要同时拿到 hash 和内容，分开传会漏。
#[derive(Debug, Clone)]
pub struct Node {
    pub hash: Hash,
    pub commit: object::Commit,
}

/// 排序键：**时间倒序**，同秒用 hash 倒序破平。
///
/// 破平不是装饰：`ts` 由调用方传入（不参与裁决，§4），两台机器同一秒内的两个 commit
/// 若没有确定的次级键，`log` 的输出顺序就会随平台变，而 §10.4 的差分表要求可比对。
impl Ord for Node {
    fn cmp(&self, other: &Self) -> Ordering {
        self.commit
            .ts
            .cmp(&other.commit.ts)
            .then_with(|| self.hash.cmp(&other.hash))
    }
}

impl PartialOrd for Node {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Node {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash
    }
}

impl Eq for Node {}

/// 从给定起点集合按时间倒序取 commit，`max = 0` 表示不限。
///
/// 用堆而不是 DFS：DFS 给出的是"某一条链连续"，`log` 要的是全局时间序；多 head 时两者
/// 结果不同，而多 head 是这个模型的正常态（§4）。菱形历史的共同祖先只出现一次（`seen`）。
pub fn walk(db: &dyn KvEngine, pid: ProjectId, from: &[Hash], max: usize) -> Result<Vec<Node>> {
    let mut seen: HashSet<Hash> = HashSet::with_capacity(from.len());
    let mut heap: BinaryHeap<Node> = BinaryHeap::new();
    for h in from {
        if seen.insert(*h) {
            heap.push(read_node(db, pid, h)?);
        }
    }

    let mut out = Vec::new();
    while let Some(node) = heap.pop() {
        for p in &node.commit.parents {
            if seen.insert(*p) {
                heap.push(read_node(db, pid, p)?);
            }
        }
        let stop = max > 0 && out.len() + 1 >= max;
        out.push(node);
        if stop {
            break; // 到量就停：剩下的留给下一次 `log`，别为了"完整"把内存撑开
        }
    }
    Ok(out)
}

fn read_node(db: &dyn KvEngine, pid: ProjectId, hash: &Hash) -> Result<Node> {
    Ok(Node {
        hash: *hash,
        commit: object::read_commit(db, pid, hash)?,
    })
}

/// 一份"树从哪儿读"：内存里那份（`plan` 算出来、**还没落盘**），或者库里的。
///
/// 为什么住在读层而不是写层：`TreeStore` 说的是"我要看一棵树"，产出它的可以是 `plan`
/// （内存）也可以是 `commit`（库）。放这儿，写层来借；反过来就成了"读依赖写"，
/// 而本模块开头明令禁止那件事。
#[derive(Debug, Default, Clone)]
pub struct TreeStore(BTreeMap<Hash, Vec<object::TreeEntry>>);

impl TreeStore {
    /// 取一份目录清单：内存里有就用内存的，否则读库。
    pub fn entries(
        &self,
        db: &dyn KvEngine,
        pid: ProjectId,
        hash: &Hash,
    ) -> Result<Vec<object::TreeEntry>> {
        if let Some(entries) = self.0.get(hash) {
            return Ok(entries.clone());
        }
        object::read_tree(db, pid, hash) // 空树在那里有特判，不查表也读得出来
    }

    /// `plan` 边算边往里放。同 hash 必然同内容（内容寻址），先到先得。
    pub(crate) fn remember(&mut self, hash: Hash, entries: Vec<object::TreeEntry>) {
        self.0.entry(hash).or_insert(entries);
    }
}

/// 按路径走到某个条目。见 [`resolve_in`]——这一条只是"树都在库里"那个常见情形。
pub fn resolve(
    db: &dyn KvEngine,
    pid: ProjectId,
    root: &Hash,
    path: &str,
) -> Result<Option<object::TreeEntry>> {
    resolve_in(db, pid, &TreeStore::default(), root, path)
}

/// 同一件事，但树可以先查内存：`checkout` 要在工作树那一侧走子树，而那棵树是 `plan`
/// 算出来的、从来没进过库。**逐层走的逻辑只有一份**，两边共用。
///
/// 返回 `None` 表示"这个版本里没有这条路径"，**中间层不是目录也算 `None`**：git 就是
/// 这么报的（`path 'a.txt/b' does not exist in HEAD`），为一种输入错误再开一种错误类型
/// 不值。真正要区分的是"不存在"与"存在但读不出来"，后者走 `read_tree` 的 `MissingObject`
/// / `CorruptBlock`，不会被这一条吞掉。
///
/// `..` 不需要特判：清单里不可能有这个名字（扫描器从不产出），所以它自然就是"不存在"。
/// 但**从历史里取出的路径拼成磁盘路径时必须在调用方再校验一次**（`restore`/`checkout` 的活）。
pub fn resolve_in(
    db: &dyn KvEngine,
    pid: ProjectId,
    trees: &TreeStore,
    root: &Hash,
    path: &str,
) -> Result<Option<object::TreeEntry>> {
    let mut cur = *root;
    // 先收集成 Vec：`Filter` 迭代器没有 `peek`，而"还有没有下游"必须能问第二次。
    // 段数就那么多，这点内存换掉一层 Peekable 的噪音是划算的。
    let parts: Vec<&str> = path
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    for (depth, name) in parts.iter().enumerate() {
        let entries = trees.entries(db, pid, &cur)?;
        let Some(e) = entries.into_iter().find(|e| e.name == *name) else {
            return Ok(None);
        };
        if depth + 1 == parts.len() {
            return Ok(Some(e));
        }
        if e.kind != Kind::Dir {
            return Ok(None); // 还有下游，但这一层不是目录
        }
        cur = e.hash;
    }
    Ok(Some(object::TreeEntry {
        name: String::new(),
        kind: Kind::Dir,
        hash: cur,
        size: 0,
        mode: 0o40755,
    }))
}

/// [`walk_tree`] 与 [`descend`] 的回调：拿到（完整路径, 条目）。
pub type TreeVisit<'a> = dyn FnMut(&str, &object::TreeEntry) -> Result<()> + 'a;

/// 深度优先列出一棵树的全部路径，**边遍历边回调**：`ls-files` 面对一百万个文件时
/// 也不许先把它们装进一个 `Vec`（§9.6.1）。
///
/// 这里用递归而不是显式栈：显式栈要拿到**字典序前序**就得连未读的兄弟一起压进去，
/// 内存反而回到 O(条目数)；递归的栈深 = 路径层数，而层数被 `PATH_MAX` 钉在几百以内。
pub fn walk_tree(
    db: &dyn KvEngine,
    pid: ProjectId,
    root: &Hash,
    visit: &mut TreeVisit<'_>,
) -> Result<()> {
    // 空树在这里有特判（`read_tree`），所以根是空的也能正常走完。
    for e in object::read_tree(db, pid, root)? {
        visit(&e.name, &e)?;
        if e.kind == Kind::Dir {
            descend(db, pid, &e, "", visit)?;
        }
    }
    Ok(())
}

fn descend(
    db: &dyn KvEngine,
    pid: ProjectId,
    dir: &object::TreeEntry,
    prefix: &str,
    visit: &mut TreeVisit<'_>,
) -> Result<()> {
    let path = if prefix.is_empty() {
        dir.name.clone()
    } else {
        format!("{prefix}/{}", dir.name)
    };
    for e in object::read_tree(db, pid, &dir.hash)? {
        visit(&format!("{path}/{}", e.name), &e)?;
        if e.kind == Kind::Dir {
            descend(db, pid, &e, &path, visit)?;
        }
    }
    Ok(())
}
/// 把用户写的 rev 解析成 commit 集合：`HEAD`/ref 名 → 该 ref 的 head 集合；
/// 否则按 **commit hash 的十六进制前缀**查（长度须为偶数，不唯一就报错）。
pub fn resolve_rev(db: &dyn KvEngine, pid: ProjectId, rev: &str) -> Result<Vec<Hash>> {
    if rev.eq_ignore_ascii_case("head") || rev == build::MAIN {
        return build::ref_heads(db, pid, build::MAIN);
    }
    let bytes = match hex_prefix(rev) {
        Some(b) => b,
        None => {
            return Err(Error::Codec(format!(
                "rev {rev:?} 看不懂：既不是 HEAD/{}，也不是十六进制 commit 前缀",
                build::MAIN
            )));
        }
    };
    let mut pattern = vec![keys::TAG_COMMIT];
    pattern.extend_from_slice(&pid.to_be_bytes());
    // 尾巴的起点是「标签 + pid」之后，**不是** pattern 之后：pattern 里还带着用户输入的那
    // 段 hash，用它当偏移会让完整 hash（64 字符）恰好切出空尾巴 ⇒ 查自己的全 ID 反而失败。
    let tail_at = pattern.len();
    pattern.extend_from_slice(&bytes);
    let page = {
        let tx = db.begin();
        let page = tx.search_keys(&SearchQuery::prefix(pattern, 0, 2));
        tx.commit();
        page
    };
    if page.total > 1 {
        return Err(Error::Codec(format!(
            "{rev} 匹配到 {} 个 commit：前缀再长一点（16 个字符起通常唯一）",
            page.total
        )));
    }
    let Some(first) = page.items.first() else {
        return Err(Error::MissingObject {
            hash: rev.to_string(),
            kind: "commit".to_string(),
        });
    };
    let tail = first
        .key
        .get(tail_at..)
        .filter(|t| t.len() == 32)
        .ok_or_else(|| Error::Codec("C 键的尾巴不是 32 字节：库里的键格式不对".to_string()))?;
    let mut h = [0u8; 32];
    h.copy_from_slice(tail);
    Ok(vec![h])
}

/// 十六进制前缀 → 原始字节。奇数长度与非法字符都返回 `None`（调用方负责报错）。
fn hex_prefix(rev: &str) -> Option<Vec<u8>> {
    if !rev.len().is_multiple_of(2) || rev.len() > 64 {
        return None;
    }
    let b = rev.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 2);
    for pair in b.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

/// epoch 纳秒 → `YYYY-MM-DD HH:MM:SS UTC`。
///
/// 不引 `chrono`/`time`：这里只需要一个显示用的日历换算，而多一个依赖就多一份
/// "它会不会在热路径上分配"的账。算法是 Howard Hinnant 的 days→civil（公历 proleptic）。
pub fn fmt_ts(ns: i64) -> String {
    let secs = ns.div_euclid(1_000_000_000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 钉一个已知时刻：日历换算错了所有历史显示都会偏，而没人会去看秒。
    #[test]
    fn fmt_ts_matches_known_instants() {
        assert_eq!(fmt_ts(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(fmt_ts(1_700_000_000_000_000_000), "2023-11-14 22:13:20 UTC");
        // 闰年与跨世纪：2024-02-29 / 2000-02-29（都是当天 00:00 UTC，别把 12:00 当 00:00——
        // 我第一版就把 1_709_208_000 写错过一次，差了整整半天还看不出来）
        assert_eq!(fmt_ts(1_709_164_800_000_000_000), "2024-02-29 00:00:00 UTC");
        assert_eq!(fmt_ts(951_782_400_000_000_000), "2000-02-29 00:00:00 UTC");
        assert_eq!(fmt_ts(-1), "1969-12-31 23:59:59 UTC", "负数不许偏一天");
        // 单位陷阱：这一条收的是**秒**，于是落在 1970 年头两秒。函数不报错、输出还像个日期，
        // 所以把它钉成期望值：谁把秒传进来，这条会告诉他"传的是秒"。
        assert_eq!(fmt_ts(1_709_164_800), "1970-01-01 00:00:01 UTC");
    }

    #[test]
    fn hex_prefix_requires_even_length() {
        assert_eq!(hex_prefix("ab"), Some(vec![0xAB]));
        assert_eq!(hex_prefix("00ff"), Some(vec![0x00, 0xFF]));
        assert!(hex_prefix("abc").is_none(), "奇数长度没法成对解码");
        assert!(hex_prefix("zz").is_none());
        assert!(
            hex_prefix(&"0".repeat(66)).is_none(),
            "超过 32 字节不是我们的 hash"
        );
        assert_eq!(hex_prefix("").unwrap(), Vec::<u8>::new(), "空前缀 = 全表");
    }
}
