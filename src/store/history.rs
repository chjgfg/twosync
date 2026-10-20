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
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashSet, VecDeque};

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

/// 按路径走到某个条目：**逐层读库**，一次只物化一层清单。
///
/// 返回 `None` 表示"这个版本里没有这条路径"，**中间层不是目录也算 `None`**：git 就是
/// 这么报的（`path 'a.txt/b' does not exist in HEAD`），为一种输入错误再开一种错误类型
/// 不值。真正要区分的是"不存在"与"存在但读不出来"，后者走 `read_tree` 的 `MissingObject`
/// / `CorruptBlock`，不会被这一条吞掉。
///
/// `..` 不需要特判：清单里不可能有这个名字（扫描器从不产出），所以它自然就是"不存在"。
/// 但**从历史里取出的路径拼成磁盘路径时必须在调用方再校验一次**（`restore`/`checkout` 的活）。
pub fn resolve(
    db: &dyn KvEngine,
    pid: ProjectId,
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
        let entries = object::read_tree(db, pid, &cur)?;
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

/// 从一组 head（外加一批"也算已知的"哈希）出发把亲代闭包走完。**错误一律照实抛出**。
///
/// 为什么必须是严格的：GC 用同一个闭包当**保留集**，少标一个 commit 就等于把一个还活着的对象
/// 判成垃圾删掉——那是不可逆的那类错。`net::session` 那边也走这一份实现，但它**故意**把错误
/// 咽掉（读不到亲代就当没有亲代），因为那里的后果只是"多推几个对象"，幂等、无害。
/// 同一次遍历、两种容错，差别写在这儿，别在下一个人手里被"统一"掉。
pub fn reachable_strict(
    db: &dyn KvEngine,
    pid: ProjectId,
    heads: &[Hash],
    extra: &[Hash],
) -> Result<BTreeSet<Hash>> {
    // **一次** `c|p` 前缀扫装好整个亲代索引，然后在内存里走闭包（[`reachable_from`]）。
    // 原来每往前一步就一次前缀扫，而门面每次读调用折一遍全库（§9.6.1）⇒ 12 GiB 站点上一次
    // "0 字节可推"的会话光历史遍历就烧掉 ~90 次折（§15.22、§17 补记廿五）。
    Ok(reachable_from(&object::parent_index(db, pid)?, heads, extra))
}

/// 纯函数版闭包：给定亲代索引，从 `heads` + `extra` 走完可达集，**一次引擎调用都不做**。
///
/// 分出来有两个用处：① `reachable_strict` 只负责"怎么把索引搬进内存"，闭包本身可单测；
/// ② `net::session` 那边（`maximal_heads` / `topo_pending`）要在**同一份索引**上算好几个闭包，
/// 那样才是"一次折"而不是"每算一次折一遍全库"。
pub fn reachable_from(
    index: &std::collections::BTreeMap<Hash, Vec<Hash>>,
    heads: &[Hash],
    extra: &[Hash],
) -> BTreeSet<Hash> {
    let mut seen = BTreeSet::new();
    let mut q: VecDeque<Hash> = VecDeque::new();
    for h in heads.iter().chain(extra.iter()) {
        if seen.insert(*h) {
            q.push_back(*h);
        }
    }
    while let Some(h) = q.pop_front() {
        for p in index.get(&h).map(|v| v.as_slice()).unwrap_or(&[]) {
            if seen.insert(*p) {
                q.push_back(*p);
            }
        }
    }
    seen
}

/// 一棵树的闭包里出现过的**全部对象**：commit 的 tree、每一层 tree、每一个 blob。
///
/// GC 的保留集要的就是这个（"哪些 `B`/`T` 行还有人引用"），而它必须在**不物化整棵树**的前提下走完
/// ——深度优先、边收哈希边丢条目，内存只剩三个哈希集合。
pub fn marked_objects(
    db: &dyn KvEngine,
    pid: ProjectId,
    commits: &BTreeSet<Hash>,
) -> Result<(BTreeSet<Hash>, BTreeSet<Hash>)> {
    // GC 是**这些命令里最宽的一趟**：它要走完保留集里每个 commit 的每一层清单
    // ⇒ 逐条点读的账是 O(commit 数 × 目录数)（12 GiB 站点上 `gc --sweep` 干过 226 折）。
    // 装载一次就够：表里有的都在手里，超预算才退回点读。
    let trees = object::TreeIndex::load(db, pid)?;
    let mut tree_set = BTreeSet::new();
    let mut blobs = BTreeSet::new();
    for c in commits {
        let commit = object::read_commit(db, pid, c)?;
        let mut stack = vec![commit.tree];
        while let Some(t) = stack.pop() {
            if !tree_set.insert(t) {
                continue;
            }
            for e in trees.list(db, pid, &t)? {
                match e.kind {
                    Kind::Dir => stack.push(e.hash),
                    Kind::File | Kind::Symlink => {
                        blobs.insert(e.hash);
                    }
                }
            }
        }
    }
    Ok((tree_set, blobs))
}

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
    // **一次** `T|p` 前缀扫换掉 O(目录数) 次点读（§11.4d，与 `build::Builder` 同一个装载器）。
    // 超预算时 `list` 自动退回点读，所以这一趟的最坏内存是被预算钉住的，不是被历史钉住的。
    let trees = object::TreeIndex::load(db, pid)?;
    // 空树在这里有特判（`read_tree`），所以根是空的也能正常走完。
    for e in trees.list(db, pid, root)? {
        visit(&e.name, &e)?;
        if e.kind == Kind::Dir {
            descend(db, pid, &trees, &e, "", visit)?;
        }
    }
    Ok(())
}

fn descend(
    db: &dyn KvEngine,
    pid: ProjectId,
    trees: &object::TreeIndex,
    dir: &object::TreeEntry,
    prefix: &str,
    visit: &mut TreeVisit<'_>,
) -> Result<()> {
    let path = if prefix.is_empty() {
        dir.name.clone()
    } else {
        format!("{prefix}/{}", dir.name)
    };
    for e in trees.list(db, pid, &dir.hash)? {
        visit(&format!("{path}/{}", e.name), &e)?;
        if e.kind == Kind::Dir {
            descend(db, pid, trees, &e, &path, visit)?;
        }
    }
    Ok(())
}
/// 把**每一层的整份清单**交给回调（回调报错就中断）。给"这一层内部有没有折叠碰撞"这类
/// **按层**判定用（§8 的大小写门）。
///
/// 与 [`walk_tree`] 的区别只是交付粒度：这里一层一次，条目对象不复制。
/// 内存 = 手上这一层 + 递归栈上各祖先层（层数被路径长度钉住），**与文件总数无关**（§9.6.1）。
/// 代价说清：它要**每层一次 `get`**，也就是 O(目录数) 次引擎调用——所以只在真要落盘的
/// 命令上跑（`checkout` / `restore --to`），`ls-files` 那种看一眼的活儿不查。
pub fn walk_levels(
    db: &dyn KvEngine,
    pid: ProjectId,
    root: &Hash,
    on_level: &mut dyn FnMut(&str, &[object::TreeEntry]) -> Result<()>,
) -> Result<()> {
    let trees = object::TreeIndex::load(db, pid)?;
    let list = trees.list(db, pid, root)?;
    on_level("", &list)?;
    descend_levels(db, pid, &trees, "", &list, on_level)
}

fn descend_levels(
    db: &dyn KvEngine,
    pid: ProjectId,
    trees: &object::TreeIndex,
    prefix: &str,
    list: &[object::TreeEntry],
    on_level: &mut dyn FnMut(&str, &[object::TreeEntry]) -> Result<()>,
) -> Result<()> {
    for e in list {
        if e.kind != Kind::Dir {
            continue;
        }
        let path = if prefix.is_empty() {
            e.name.clone()
        } else {
            format!("{prefix}/{}", e.name)
        };
        let inner = trees.list(db, pid, &e.hash)?;
        on_level(&path, &inner)?;
        descend_levels(db, pid, trees, &path, &inner, on_level)?;
    }
    Ok(())
}

/// 把用户写的 rev 解析成 commit 集合。语法三档：
/// - `HEAD` / ref 名 → 该 ref 的 head 集合（多 head 就返回多个）
/// - 十六进制前缀（偶数长度）→ 唯一匹配的那个 commit
/// - 以上任一种后面接 `~N` 或 `^N`（省略 N 就是 1）→ 沿**第一亲代**回溯 N 步
///
/// 为什么只跟第一亲代：`~` 在 git 里就是这个语义；而"多亲时该走哪一条"是一个**决策**，
/// 不是解析细节——那是 M3 union / `branch` 的活，这里不许偷偷替用户选一条链。
pub fn resolve_rev(db: &dyn KvEngine, pid: ProjectId, rev: &str) -> Result<Vec<Hash>> {
    let (base, steps) = split_rev(rev);
    let heads = rev_base(db, pid, base)?;
    if steps == 0 {
        return Ok(heads);
    }
    if heads.len() != 1 {
        return Err(Error::Codec(format!(
            "`{rev}` 有歧义：{base} 指向 {} 个 head，`~N` 只能从一个 commit 出发",
            heads.len()
        )));
    }
    let mut cur = heads[0];
    for step in 1..=steps {
        let parents = object::parents_of(db, pid, &cur)?;
        cur = *parents.first().ok_or_else(|| Error::MissingObject {
            hash: format!("{}（回溯到第 {step} 步时没有亲代）", keys::short(&cur)),
            kind: "commit".to_string(),
        })?;
    }
    Ok(vec![cur])
}

/// 拆出 `~N` / `^N` 后缀。两种写法语义相同（本项目不需要"第 N 亲"那套语法），所以链式
/// 后缀就是**步数相加**：`HEAD~2^` 与 `HEAD~3` 是同一个 commit。一次 `rfind` 只能剥掉
/// 一个后缀，链式写法会剩下个 `HEAD~2` 当基座去查 hash ⇒ 必须循环剥。
///
/// 后缀不是数字、或者标记出现在开头（`~2` 这种没有基座），就整串当基座交给下游报错——
/// 报"看不懂这个 rev"比替用户猜一个数诚实。
fn split_rev(rev: &str) -> (&str, usize) {
    let mut steps = 0usize;
    let mut found = false;
    let mut base = rev;
    while let Some(i) = base.rfind(['~', '^']).filter(|i| *i > 0) {
        let n = match &base[i + 1..] {
            "" => 1,
            other => match other.parse::<usize>() {
                Ok(n) => n,
                Err(_) => break,
            },
        };
        steps += n;
        found = true; // `HEAD~0` 是合法写法：步数是 0，但后缀**确实被认出来了**
        base = &base[..i];
    }
    if found { (base, steps) } else { (rev, 0) }
}

/// 不含回溯后缀的那部分怎么解析：ref 名或十六进制前缀。
fn rev_base(db: &dyn KvEngine, pid: ProjectId, rev: &str) -> Result<Vec<Hash>> {
    if rev.eq_ignore_ascii_case("head") || rev == build::MAIN {
        return build::ref_heads(db, pid, build::MAIN);
    }
    // **命名引用（tag/branch）排在 hex 前缀之前**。理由不是好看：一个 8 个字符的 tag 名
    // 完全可以同时是合法的 hash 前缀，顺序反了就会出现"人写的是名字、系统按前缀猜到了别的
    // commit"。这种歧义不许靠解析顺序解决，所以 `tag add` 那一侧直接拒绝长得像 hash 前缀的
    // 名字（`cli::m4::check_tag_name`），两边合起来才是"不猜"。
    let named = build::ref_heads(db, pid, rev)?;
    if !named.is_empty() {
        return Ok(named);
    }
    let bytes = match hex_prefix(rev) {
        Some(b) => b,
        None => {
            return Err(Error::Codec(format!(
                "rev {rev:?} 看不懂：既不是 HEAD/{m}，也不是已知的 tag，也不是十六进制 commit 前缀（可接 `~N`）",
                m = build::MAIN
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

// ── §7 第五行：collision（并集之后同一条路径被两个 head 各改成不同的东西）─────────────
//
// v1 的语义是"union，不猜"：多 head 是**正常状态**（§7.4），只有当两条线**动了同一条路径**、
// 且动出来的结果不一样时，才必须由人选（FR-11）。所以这里要做的不是"怎么合"，而是
// **把"哪条路径两边不一样"算准**——算漏了就是自动替用户选了一边，算多了就是每次 union 都在报警。

/// 一条冲突：路径 + 卷进去的那几个 head（按哈希排序，稳定输出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collision {
    pub path: String,
    pub heads: Vec<Hash>,
}

/// head 集合里的 collision 列表（`heads.len() < 2` 时恒为空）。
///
/// 判据是"**与共同祖先比，两边都动了这条路径，而且动出来的不一样**"：
/// - 只有一边动 ⇒ 不冲突（另一边跟着走，这是 union 的本意）；
/// - 两边动到**同一个结果**（同 hash 同 mode）⇒ 也不冲突（字节一样的两份改动没有可裁的东西）。
///
/// 共同祖先不止一个（criss-cross）时用的是"与**任一个**祖先比都一样就算没动"这条近似：
/// 真正的 recursive merge base 是一个**决策**（选哪几个虚拟祖先、多个基怎么叠），不是实现细节，
/// 这里不偷偷替 v1 定。真要上多端合并再记进 §14。
///
/// 引擎调用：亲代索引一次 + 目录清单一次（都是前缀扫），剩下的是内存里走 ⇒ 两个 head、
/// 每个差 1 万条路径时约 2 万条 `String`，这是判据的固有代价，只在 head 数 ≥ 2 时才付。
pub fn collisions(db: &dyn KvEngine, pid: ProjectId, heads: &[Hash]) -> Result<Vec<Collision>> {
    if heads.len() < 2 {
        return Ok(Vec::new());
    }
    let index = object::parent_index(db, pid)?;
    let trees = object::TreeIndex::load(db, pid)?;
    let mut mine = BTreeMap::new();
    for h in heads {
        let c = object::read_commit(db, pid, h)?;
        mine.insert(*h, c.tree);
    }
    let mut by_path: BTreeMap<String, BTreeSet<Hash>> = BTreeMap::new();
    for (i, a) in heads.iter().enumerate() {
        for b in &heads[i + 1..] {
            let Some((a_tree, b_tree)) = mine.get(a).zip(mine.get(b)) else {
                continue;
            };
            // 共同祖先是 **commit** 哈希，而清单要的是它们的 tree —— 这一步换算不能省
            // （省了就是拿 commit 哈希去查 `T`，报出来的会是 MissingObject）。
            let base_trees: Vec<Hash> = merge_bases(&index, a, b)
                .iter()
                .map(|c| object::read_commit(db, pid, c).map(|cc| cc.tree))
                .collect::<Result<_>>()?;
            let ca = changed_paths(db, pid, &trees, &base_trees, a_tree)?;
            let cb = changed_paths(db, pid, &trees, &base_trees, b_tree)?;
            for (path, ea) in &ca {
                let Some(eb) = cb.get(path) else { continue };
                if ea == eb {
                    continue; // 两边改成同一个东西：没有要人选的东西
                }
                let entry = by_path.entry(path.clone()).or_default();
                entry.insert(*a);
                entry.insert(*b);
            }
        }
    }
    Ok(by_path
        .into_iter()
        .map(|(path, hs)| Collision { path, heads: hs.into_iter().collect() })
        .collect())
}

/// `a` 与 `b` 的**极大**共同祖先（可能多个，见 [`collisions`] 那条近似）。空 = 没有共同祖先。
fn merge_bases(index: &std::collections::BTreeMap<Hash, Vec<Hash>>, a: &Hash, b: &Hash) -> Vec<Hash> {
    let ra = reachable_from(index, &[*a], &[]);
    let rb = reachable_from(index, &[*b], &[]);
    let common: Vec<Hash> = ra.intersection(&rb).cloned().collect();
    // 极大 = 不是别的共同祖先的**严格祖先**。用闭包比，一次一个：共同祖先本来就少。
    common
        .iter()
        .filter(|c| {
            !common
                .iter()
                .any(|d| d != *c && reachable_from(index, &[*d], &[]).contains(*c))
        })
        .cloned()
        .collect()
}

/// 一棵树相对**若干基的清单**（`bases` 是 tree 哈希，不是 commit 哈希）的变更集：路径 → 那条路径上的 `(hash, mode)`（新增时基里没有 ⇒ 也算变更）。
///
/// `bases` 为空 = 没有共同祖先 ⇒ 与空树比，两边所有路径都算"自己动的"。
fn changed_paths(
    db: &dyn KvEngine,
    pid: ProjectId,
    trees: &object::TreeIndex,
    bases: &[Hash],
    head: &Hash,
) -> Result<BTreeMap<String, (Hash, u32)>> {
    let mut out = BTreeMap::new();
    collect_changed(db, pid, trees, bases, head, "", &mut out)?;
    Ok(out)
}

/// 深度优先比一层：`head` 这一层的每条与基层的同名条比，不同就记下来；目录继续往下走。
fn collect_changed(
    db: &dyn KvEngine,
    pid: ProjectId,
    trees: &object::TreeIndex,
    bases: &[Hash],
    head: &Hash,
    prefix: &str,
    out: &mut BTreeMap<String, (Hash, u32)>,
) -> Result<()> {
    // 基那一侧可能有几张候选清单（criss-cross）：只要**任何**一个基里这条路径一模一样，
    // 就算"这一侧没动过它"（§7 那条近似的落点）。
    let base_lists: Vec<Vec<object::TreeEntry>> = bases
        .iter()
        .map(|h| trees.list(db, pid, h))
        .collect::<Result<_>>()?;
    for e in trees.list(db, pid, head)? {
        let path = if prefix.is_empty() {
            e.name.clone()
        } else {
            format!("{prefix}/{}", e.name)
        };
        let untouched = base_lists.iter().any(|list| {
            list.iter()
                .any(|x| x.name == e.name && x.hash == e.hash && x.mode == e.mode)
        });
        if untouched {
            // 这一条与某个基一模一样。目录的情况到这里就够了：**整棵子树没动**，
            // 不用下去（NFR-6 那条 hash 短路在冲突检测上的同一份用法）。
            continue;
        }
        if e.kind == Kind::Dir {
            // 目录自己不一样不算一条冲突：子项各算各的（否则改一个文件会把整层报成冲突）。
            collect_changed(db, pid, trees, bases, &e.hash, &path, out)?;
        } else {
            out.insert(path, (e.hash, e.mode));
        }
    }
    Ok(())
}
