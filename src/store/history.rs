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
    entry_at(root, path, &|h| object::read_tree(db, pid, h))
}

/// 与 [`resolve`] **同一个走法**，只是每一层的清单来自已经装载好的 [`object::TreeIndex`]。
///
/// 为什么要两份入口而不是一份：单条路径的查询（`show`/`restore`）不该为了一次点读把整库清单
/// 装进内存（§11.4d 那条预算），而 `blame` 这种"同一片目录要读几十遍"的必须装一次 ——
/// 后者把 `O(版本数 × 层数)` 次门面读换成**一次前缀扫**（每次门面读都折一遍全库，§9.6.1）。
/// 走法只有一份（[`entry_at`]），所以两边不可能一个认 `..` 一个不认。
pub fn resolve_cached(
    db: &dyn KvEngine,
    pid: ProjectId,
    trees: &object::TreeIndex,
    root: &Hash,
    path: &str,
) -> Result<Option<object::TreeEntry>> {
    entry_at(root, path, &|h| trees.list(db, pid, h))
}

/// 逐层走到那条条目。**这里是唯一一份走法**，两种清单来源（点读 / 装载索引）都走它。
fn entry_at(
    root: &Hash,
    path: &str,
    list: &dyn Fn(&Hash) -> Result<Vec<object::TreeEntry>>,
) -> Result<Option<object::TreeEntry>> {
    let mut cur = *root;
    // 先收集成 Vec：`Filter` 迭代器没有 `peek`，而"还有没有下游"必须能问第二次。
    // 段数就那么多，这点内存换掉一层 Peekable 的噪音是划算的。
    let parts: Vec<&str> = path
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    for (depth, name) in parts.iter().enumerate() {
        let entries = list(&cur)?;
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
    // 名字（`cli::m4::check_ref_name`），两边合起来才是"不猜"。
    // 先查字面名再查带前缀的名：`refs/heads/x` 这种全写法也要能直接敲出来，而 tag 与分支的名字
    // 都不许以 `refs/` 开头（同一道闸 ⇒ 两种拼法不可能同时命中 ⇒ 不存在"谁先查谁赢"的暗裁决）。
    // 两次查表用的是**同一次**前缀扫（`all_named_refs`）：门面每扫一次就折一遍全库（§9.6.1），
    // 为"多认一种名字"再加点一次 `ref_heads` 就是把每条 revspec 变贵一倍。
    let refs = build::all_named_refs(db, pid)?;
    for name in [rev.to_string(), build::branch_ref(rev)] {
        if let Some(hs) = refs.get(&name) {
            return Ok(hs.clone());
        }
    }
    let bytes = match hex_prefix(rev) {
        Some(b) => b,
        None => {
            return Err(Error::Codec(format!(
                "rev {rev:?} 看不懂：既不是 HEAD/{m}，也不是已知的 tag 或分支，也不是十六进制 commit 前缀（可接 `~N`）",
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

/// 删掉/挪走 `skip` 这一格之后，`from` 的闭包里**再没有别的 ref 挂着**的那些 commit。
///
/// 为什么要减掉"其余全部 ref"而不是只减 `main`：两个分支常常共用祖先，只比 main 会把那些
/// 报成"要变垃圾"，那是吓人不是报账。`gc` 的保留集正是从全部 ref 出发的
/// （[`build::all_ref_heads`]），所以这里算的是同一件事，命令里报的数才与回收真会做的对得上。
///
/// 纯函数（refs/亲代索引都在内存里，一次引擎调用都不做），因为这条判据要能单测：
/// "松开几条"写错方向（多报/少报）都会让人在错误的时刻去 `tag add`。
pub fn unheld_by_other_refs(
    refs: &BTreeMap<String, Vec<Hash>>,
    index: &BTreeMap<Hash, Vec<Hash>>,
    skip: &str,
    from: &[Hash],
) -> Vec<Hash> {
    let reach = reachable_from(index, from, &[]);
    let mut others: BTreeSet<Hash> = BTreeSet::new();
    for (k, hs) in refs {
        if k == skip {
            continue;
        }
        others.extend(reachable_from(index, hs, &[]));
    }
    reach.difference(&others).copied().collect()
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

/// 落盘类命令共用的"这一本对她做了什么"（`blame` 用）。
///
/// **判据在这层、措辞在命令层**：枚举能被用例钉住，而 `format!` 出来的那句话钉不住。
/// 判据是**条目本身**（`kind`+`hash`+`mode`）：与更老的那一本不同就算"动过"。
/// `mode` 单独一支是有真内容的：`mode` 进树的 hash 但不进 blob 的 hash（§8 那条），
/// 所以 `chmod` 之后"内容没变而这一本确实动过她"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathChange {
    /// 更老的那一本里没有她
    Appeared,
    /// 这一本里没有她了
    Disappeared,
    /// 同一种类型，内容（blob/子树）换了
    Content { to: Hash },
    /// 内容一样、只有权限位变了
    Mode { to: u32 },
    /// 文件与目录/符号链接互换（形状变了，光看 hash 说不清）
    KindSwap,
}

/// 一次动过：哪一本 + 对她做了什么。
#[derive(Debug, Clone)]
pub struct PathHit {
    pub node: Node,
    pub change: PathChange,
}

/// [`blame_path`] 的产物。**`truncated` 必须带回来**：窗口截断时最老那一行不代表"她第一次出现"，
/// 命令不说这一句就是在撒谎。
#[derive(Debug)]
pub struct PathHistory {
    pub hits: Vec<PathHit>,
    /// 窗口里走了多少本
    pub scanned: usize,
    pub truncated: bool,
    /// 最新那一本里她的样子（`None` = 现在没有这条路径）
    pub now: Option<object::TreeEntry>,
}

/// **路径级归属**：按从老到新返回"动过这条路径的那些 commit"。
///
/// 为什么不是行级（这是产品边界，不是偷懒）：本工具的语料是 2–10 GB 的大二进制，切块单位 4 MiB，
/// 而"行"不是任何一层的身份 —— `B` 清单里只有 `(块号, hash, 大小)`。硬做行级 blame 得到的是
/// 看起来像 `git blame`、语义是猜的东西。
///
/// 代价：**一次 `T|p` 前缀扫 + 每本一次内存里逐层查**（[`resolve_cached`]）。
/// 换成逐本点读就是 `O(版本数 × 层数)` 次门面读，而每次门面读折一遍全库（§9.6.1）。
/// `max = 0` 是全部 ⇒ 内存 `O(历史长度)`，与 [`walk`] 同一条警告。
pub fn blame_path(
    db: &dyn KvEngine,
    pid: ProjectId,
    heads: &[Hash],
    path: &str,
    max: usize,
) -> Result<PathHistory> {
    let trees = object::TreeIndex::load(db, pid)?;
    let nodes = walk(db, pid, heads, max)?;
    let mut hits: Vec<PathHit> = Vec::new();
    let mut older: Option<object::TreeEntry> = None;
    // 从**老到新**走：这样"第一次出现"与"消失"两种情形读起来是顺的。
    for node in nodes.iter().rev() {
        let here = resolve_cached(db, pid, &trees, &node.commit.tree, path)?;
        let change = match (&older, &here) {
            (Some(a), Some(b)) if a == b => None,
            (Some(a), Some(b)) => Some(if a.kind != b.kind {
                PathChange::KindSwap
            } else if a.hash != b.hash {
                PathChange::Content { to: b.hash }
            } else {
                PathChange::Mode { to: b.mode }
            }),
            (None, Some(_)) => Some(PathChange::Appeared),
            (Some(_), None) => Some(PathChange::Disappeared),
            (None, None) => None,
        };
        if let Some(change) = change {
            hits.push(PathHit {
                node: node.clone(),
                change,
            });
        }
        older = here;
    }
    let now = match nodes.first() {
        Some(n) => resolve_cached(db, pid, &trees, &n.commit.tree, path)?,
        None => None,
    };
    Ok(PathHistory {
        hits,
        scanned: nodes.len(),
        truncated: max > 0 && nodes.len() >= max,
        now,
    })
}

/// 无状态二分的一步（[`bisect_step`] 的产物）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BisectStep {
    pub candidate: Hash,
    pub suspects: usize,
    pub bad_closure: usize,
    pub good_closure: usize,
}

/// [`bisect_step`] 那**两种**退化形状（正常的那一支走 `Step`）。**逼进类型而不是拿魔法返回值当哨兵**：
/// 每一条的措辞都不同，而且都必须在命令层说出来（吞掉一条就等于给用户一个错的中点）。
///
/// 反过来，"两端之间一条都没有"那一支**不写**，而且能证它不存在：`bad` 永远在自己的闭包里，
/// 所以只要 `bad` 不在 `good` 的闭包中（`Reversed` 那一条已经查过），怀疑集就至少还剩 `bad` 自己。
/// 写一个到不了的分支 = 让下一个人去测一条永远绿的路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BisectVerdict {
    Step(BisectStep),
    /// `--good` 不在 `--bad` 的祖先里 ⇒ 两端不在同一条线上，"之间"没有定义
    GoodNotAncestor,
    /// `--bad` 反而是 `--good` 的祖先 ⇒ 两端写反了
    Reversed,
}

/// **无状态**二分的一步：从 `bad` 的闭包减掉 `good` 的闭包，取时间最靠中间的那一本。
///
/// 不落游标（git 那套 `BISECT_LOG`）是取舍不是省事：状态要么写库要么写文件，就有了"bisect 卡住
/// 要先 reset"这类坏账。这里每次重算，代价是把上一条的答案抄回命令行。
///
/// 保证只有一条，而且是能证的那条：**候选属于怀疑集** ⇒ 每问一次区间严格变小。历史里有合并时
/// 时间序不等于拓扑序，所以**不保证**正好缩一半 —— 这句话要命令自己说。
pub fn bisect_step(db: &dyn KvEngine, pid: ProjectId, bad: &Hash, good: &Hash) -> Result<BisectVerdict> {
    let index = object::parent_index(db, pid)?;
    let bad_set = reachable_from(&index, &[*bad], &[]);
    let good_set = reachable_from(&index, &[*good], &[]);
    // **先判"反了"，再判"不在一条线上"**：两端写反时 `good` 的闭包确实包含 `bad`，而 `bad` 的闭包
    // 不含 `good` —— 顺序反过来就只会报"不在同一条线上"，而那句是给不了人下一步的
    //（正确提示是"两端换个位置"）。这是用例 `bisect_refuses_endpoints_that_are_not_on_one_line`
    // 第一版就抓到的：判据对，措辞的可达性错了。
    if good_set.contains(bad) {
        return Ok(BisectVerdict::Reversed);
    }
    if !bad_set.contains(good) {
        return Ok(BisectVerdict::GoodNotAncestor);
    }
    let suspects: BTreeSet<Hash> = bad_set.difference(&good_set).copied().collect();
    // 时间序：`walk` 是堆序（时间倒序），窗口给"怀疑集 + good 那一支"那么大 —— 这**不少于** `bad`
    // 的闭包（闭包 = 怀疑集 ⊔ 与 good 重合的那部分），所以一次都能收回来，不用退回别的序。
    let ordered = walk(db, pid, &[*bad], suspects.len() + good_set.len())?;
    let line: Vec<Hash> = ordered
        .iter()
        .filter(|n| suspects.contains(&n.hash))
        .map(|n| n.hash)
        .collect();
    let mid = line.len() / 2;
    Ok(BisectVerdict::Step(BisectStep {
        candidate: line[mid],
        suspects: suspects.len(),
        bad_closure: bad_set.len(),
        good_closure: good_set.len(),
    }))
}

/// 合并出来的两种结果。`Conflict` 带的是**冲突路径**，不是错误：§7.1 要的是"报出来给人选"，
/// 而"报错"与"给人看"在命令层是两件事（报错的出口非零退出，这里应当只让人看见清单）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merge {
    Done { tree: Hash },
    Conflict { paths: Vec<String> },
}

/// 两个 commit 的共同祖先的 **tree** 哈希（不是 commit 哈希——这个换算是 `6ee9e3e` 那轮
/// 我自己踩过一次的坑：拿 commit 哈希去查 `T` 会报成 `MissingObject`）。
pub fn merge_base_trees(
    db: &dyn KvEngine,
    pid: ProjectId,
    index: &std::collections::BTreeMap<Hash, Vec<Hash>>,
    a: &Hash,
    b: &Hash,
) -> Result<Vec<Hash>> {
    merge_bases(index, a, b)
        .iter()
        .map(|c| object::read_commit(db, pid, c).map(|cc| cc.tree))
        .collect()
}

/// 合并结果要不要**落盘**。
///
/// **用枚举不用 `bool`**（同 [`build::Sink`] 那条老规矩）：布尔会一路传到调用链上去，
/// 谁传反了谁就静默产出"只报那一趟其实写了盘"——而 `merge`/`revert` 命令自己当着人说过
/// "不带 `--apply` 一个字节都不写"（FR-7）。两个实现各自只有一条路：`Report` 只算 hash
/// （[`object::tree_hash`] 与落盘用的是同一套规范字节，见 `object_store.rs` 的 `tree_hash_is_canonical`），
/// `Store` 才 `put_tree`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emit {
    Report,
    Store,
}

/// **结构合并**两棵树的同一层（§1："只做两侧改动路径无交集时的结构合并，不做内容级三方合并"）。
///
/// 逐条五分类，判据与 [`changed_paths`] 用的是同一把尺（"与**任一**共同祖先一模一样"= 没动过）：
/// 1. 两边同名条**完全相同**（`hash`+`mode`）⇒ 取它（目录走 hash 短路：整棵子树不必下去）；
/// 2. 两边都是目录且不一样 ⇒ **递归下去**（子项各算各的，父层自己不算一条冲突——与碰撞判定同一条规矩）；
/// 3. 有一边相对祖先**没动** ⇒ 取动了的那一边的结果（包括它把这条删掉 ⇒ 删除生效）；
/// 4. 只有一边有这一条、而祖先也没有 ⇒ 那是**新增**，取存在的那一边；
/// 5. 剩下就是"两边各改成不同的东西"（或一边改一边删）⇒ 记进冲突路径，**这一层一个字都不写**。
///
/// 冲突是**收集完再返回**的，不是遇到就停：只报第一条冲突等于让人猜还有几条。
/// 内存：一次只握着一层的清单（`TreeIndex` 命中时零引擎调用），与 `walk_tree` 同一形状。
pub fn merge_trees(
    db: &dyn KvEngine,
    pid: ProjectId,
    a: &Hash,
    b: &Hash,
    bases: &[Hash],
    emit: Emit,
) -> Result<Merge> {
    let index = object::TreeIndex::load(db, pid)?;
    let mut m = Merger {
        db,
        pid,
        index: &index,
        conflicts: Vec::new(),
        emit,
    };
    let tree = m.level(bases, a, b, "")?;
    if !m.conflicts.is_empty() {
        return Ok(Merge::Conflict {
            paths: std::mem::take(&mut m.conflicts),
        });
    }
    Ok(Merge::Done {
        // 根这一层不会合空（两边都是 commit 的 tree，空目录有 `empty_tree` 特判），
        // 这里兜的是"两边都指向同一棵空树"那种退化形状。
        tree: tree.unwrap_or_else(object::empty_tree),
    })
}

/// 逐层合并的执行者。**收成结构而不是给函数塞八个参数**：`db`/`pid`/清单索引/冲突收集器
/// 是每一层都要带的四样，它们是同一次合并的"现场"，不是调用方可选的旋钮
/// （clippy 的 7 参上限报的就是那形状 —— 参照 `PushArgs` 那条同样的处理，不加 `#[allow]`）。
struct Merger<'a> {
    db: &'a dyn KvEngine,
    pid: ProjectId,
    index: &'a object::TreeIndex,
    conflicts: Vec<String>,
    emit: Emit,
}

impl Merger<'_> {
    /// 合并一层。返回 `None` = 这一层合完是**空的**（该目录两边都被删 ⇒ 父层因此不放这一条）。
    fn level(&mut self, bases: &[Hash], a: &Hash, b: &Hash, prefix: &str) -> Result<Option<Hash>> {
        let la = self.index.list(self.db, self.pid, a)?;
        let lb = self.index.list(self.db, self.pid, b)?;
        // 祖先可能有几个（criss-cross）。"没动过"是**对任意一个祖先**成立就算。
        let base_lists: Vec<Vec<object::TreeEntry>> = bases
            .iter()
            .map(|h| self.index.list(self.db, self.pid, h))
            .collect::<Result<_>>()?;

        // 并集的名字：三份清单（两边 + 各祖先）去重后排序 ⇒ 合并结果与遍历顺序无关。
        let mut names: Vec<&str> = Vec::new();
        for l in [&la, &lb] {
            for e in l {
                if !names.contains(&e.name.as_str()) {
                    names.push(&e.name);
                }
            }
        }
        for l in &base_lists {
            for e in l {
                if !names.contains(&e.name.as_str()) {
                    names.push(&e.name);
                }
            }
        }
        names.sort_unstable();

        let mut out: Vec<object::TreeEntry> = Vec::new();
        for name in names {
            let ea = la.iter().find(|x| x.name == name);
            let eb = lb.iter().find(|x| x.name == name);
            let path = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}/{name}")
            };
            // ① 两边完全一样（含"两边都没有"= 都删了）⇒ 直接取，连递归都省。
            if ea == eb {
                if let Some(x) = ea {
                    out.push((*x).clone());
                }
                continue;
            }
            // ② 两边都是目录且不一样 ⇒ 递归。放在"没动过"之前：目录 hash 不同**不代表**子项冲突。
            if ea.is_some_and(|x| x.kind == Kind::Dir) && eb.is_some_and(|x| x.kind == Kind::Dir) {
                let sub_bases: Vec<Hash> = base_lists
                    .iter()
                    .filter_map(|l| l.iter().find(|x| x.name == name))
                    .filter(|x| x.kind == Kind::Dir)
                    .map(|x| x.hash)
                    .collect();
                let merged =
                    self.level(&sub_bases, &ea.unwrap().hash, &eb.unwrap().hash, &path)?;
                if let Some(h) = merged {
                    let mut e = ea.unwrap().clone();
                    e.hash = h;
                    // size 恒 0（定义在 `worktree::logical_size`）。mode 取 a 侧：目录本身两边不同
                    // 而内容能合，是"权限各自改"那种，v1 不裁决，取第一个 head 那一侧。
                    out.push(e);
                }
                // 子树合空了 ⇒ 这个目录两边都没了，父层不放这条。
                continue;
            }
            // ③ 有一边相对祖先没动 ⇒ 取动了的那一边（包括它把这条删掉）。
            let a_untouched = base_lists
                .iter()
                .any(|l| l.iter().find(|x| x.name == name).is_some_and(|x| Some(x) == ea));
            let b_untouched = base_lists
                .iter()
                .any(|l| l.iter().find(|x| x.name == name).is_some_and(|x| Some(x) == eb));
            if a_untouched {
                if let Some(x) = eb {
                    out.push((*x).clone());
                }
                continue;
            }
            if b_untouched {
                if let Some(x) = ea {
                    out.push((*x).clone());
                }
                continue;
            }
            // ④ 祖先里都没有 ⇒ 有一边纯粹是新增，取存在的那一边
            //    （与 `collisions` 同一条判据：只有"两边都有且不一样"才算冲突）。
            if ea.is_none() || eb.is_none() {
                out.push(ea.or(eb).unwrap().clone());
                continue;
            }
            // ⑤ 两边各改成不同东西 ⇒ 记下来，这一层不写盘。
            self.conflicts.push(path);
        }

        if self
            .conflicts
            .iter()
            .any(|p| p.starts_with(prefix) && p.len() > prefix.len())
        {
            // 这一层（或它的子层）里有冲突 ⇒ 别再 put_tree，否则父层会拿到一个 hash 去引用
            // 一份"其实没合上"的清单。上层靠 conflicts 非空来收场。
            return Ok(None);
        }
        if out.is_empty() {
            return Ok(None);
        }
        // `Report` 那一支只算 hash：`tree_hash` 与 `put_tree` 用的是同一套规范字节，
        // 所以两种模式给出的 hash 必然一致（`revert` 就靠这条一致性先算账、后落盘）。
        let tree = match self.emit {
            Emit::Store => object::put_tree(self.db, self.pid, &out)?,
            Emit::Report => object::tree_hash(&out),
        };
        Ok(Some(tree))
    }
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
