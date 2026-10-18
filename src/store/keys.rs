//! §9.2 的键编码。
//!
//! 规则只有三条，但整个存储层都建立在它们上面：
//! 1. 键 = `1 B 标签 + project_id(4 B BE) + 主体`；
//! 2. 数值一律 **BE** 编码 ⇒ **字节序 == 逻辑序**，所以"按 refcnt 排序"能直接靠前缀扫拿到；
//! 3. **变长段必须放在键尾**（path / ref_name），否则前缀扫会串。变长段后面还要再接定长段时，
//!    中间必须插 `0x00` 分隔符——见 [`head_key`]。

use serde::{Deserialize, Serialize};

/// 我们写进引擎的每个 value 都以这个字节开头。
///
/// **为什么必须有**：引擎的 `set` 把 value 当"已打 TTL 信封"的逻辑值处理，首字节 `0x00`
/// 会被当"永久"标签吃掉 1 字节、`0x01` 会被当 TTL 吃掉 9 字节（README 已知局限第 7 条，
/// 本仓库 `tests/engine_envelope.rs` 有实测）。而 postcard 编码的 `WtEntry`/`Vec<TreeEntry>`
/// 首字节常常就是枚举变体号（`Kind::File = 0`），也就是说这个坑对我们是**默认命中**的。
///
/// 更麻烦的是引擎的 `set` 与 `batch_set` 在这个问题上行为不一致（实测：同一个裸字节，
/// `set` 少 1 字节、`batch_set` 完好）。所以对策不能是"统一走 batch_set"——那等于把
/// 正确性押在引擎的偶然行为上。前置标签让这个不变量**归我们所有**。
///
/// 顺带它也是格式版本位：将来换编码只需新增 `VALUE_TAG_V2`，读侧按首字节分流。
pub const VALUE_TAG: u8 = b'T'; // 0x54：既不是 0x00 也不是 0x01

/// 打包：加标签前缀。写引擎前最后一步。
pub fn wrap(bytes: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    out.push(VALUE_TAG);
    out.extend_from_slice(&bytes);
    out
}

/// 拆包：零拷贝去掉标签。首字节不对就是数据损坏或格式不认，**不许猜**。
pub fn strip(raw: &[u8]) -> Result<&[u8], crate::error::Error> {
    // `Some(&VALUE_TAG)` 而不是 `Some((VALUE_TAG, _))`：const 模式不享受 match 默认绑定
    // 模式，直接写在 `&u8` 的位置上会类型不匹配。
    match raw.first() {
        Some(&VALUE_TAG) => Ok(&raw[1..]),
        Some(&other) => Err(crate::error::Error::Codec(format!(
            "value 标签不认识：0x{other:02x}（期望 0x{VALUE_TAG:02x}）——格式版本或磁盘内容有问题"
        ))),
        None => Err(crate::error::Error::Codec(
            "value 是空的，连标签都没有".to_string(),
        )),
    }
}

/// 记录编码的唯一一份。**值本身不带信封**，写引擎前还要过 [`wrap`]。
///
/// 为什么收在这儿而不是各模块自己写：`build`/`object`/`gc` 三处原先各有一份
/// `postcard::to_allocvec` + 自己的错误文案，报错口径会漂（"解码失败"到底是谁说的、带不带记录类型名，
/// 排查磁盘损坏时就是这个区别）。错误里必须带 `what`——只说"解码失败"的报错等于没说。
pub fn enc<T: serde::Serialize>(v: &T, what: &str) -> crate::error::Result<Vec<u8>> {
    postcard::to_allocvec(v).map_err(|e| crate::error::Error::Codec(format!("{what} 编码失败：{e}")))
}

/// 读一条记录：去信封 + 解码。见 [`enc`] 的同一条理由。
pub fn rec<T: serde::de::DeserializeOwned>(raw: &[u8], what: &str) -> crate::error::Result<T> {
    postcard::from_bytes(strip(raw)?)
        .map_err(|e| crate::error::Error::Codec(format!("{what} 解码失败：{e}")))
}

/// 条目类型。**工作树记录与对象清单共用这一个枚举**：两边各写一份的话，W→tree 的转换
/// 就要手写映射，而映射表是最容易漂的地方（`copy_out` 那个 hasher bug 是同一类错误）。
///
/// 变体顺序是磁盘格式的一部分：`File=0 / Dir=1 / Symlink=2`，改动会让已存的 `W` 记录错位。
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

/// blake3 摘要，**落盘用满 32 B**（§12：20 B 截断只许出现在线上传输与比对提示）。
pub type Hash = [u8; 32];
pub type ProjectId = u32;

// ── 标签（§9.2 表）──────────────────────────────────────────────
pub const TAG_COMMIT: u8 = b'C';
pub const TAG_PARENT: u8 = b'c';
pub const TAG_TREE: u8 = b'T';
pub const TAG_BLOB: u8 = b'B';
pub const TAG_BLOCK: u8 = b'K';
pub const TAG_REFCNT: u8 = b'k';
pub const TAG_WORKTREE: u8 = b'W';
pub const TAG_HEAD: u8 = b'H';
/// 最近一次**完整跑完**的扫描标记：`Z|p` → `worktree::ScanMark`。
///
/// 为什么要有它（不靠"行里最大的 `scanned_at`"）：那个算法在扫描中途被打断时会把
/// **半轮**的 epoch 当成最新，于是没被走到的那一半全部看起来像被删除——一次 Ctrl-C
/// 就制造一次假的大规模删除，只等 FR-10 去拦。改成"只有完整扫描才推进 epoch"之后，
/// 中断的那半轮既不推进 epoch、也不丢数据（见 `build::snapshot` 的 `>=` 规则）。
pub const TAG_SCAN: u8 = b'Z';
pub const TAG_FOLDED: u8 = b'A';
pub const TAG_SESSION: u8 = b'S';
pub const TAG_PEER: u8 = b'P';
/// pending-delete：refcnt 已归零、等着被限额 unlink 的块（§9.3 的那个"列表"）。
///
/// 为什么要有这么一行而不是"当场删文件"： decrements 现场删文件是 §9.3 明令禁止的——
/// 崩在中间就是"清单引用一个不存在的块"。归零的块先进这张表，unlink 是**后面一趟限额的活**，
/// 崩了下次接着做，而且 `gc` 能报出"还欠着多少个没删"。
pub const TAG_PENDING: u8 = b'D';

/// 内容 hash 的唯一入口。换算法/改截断长度只改这一处
pub fn hash_of(bytes: &[u8]) -> Hash {
    *blake3::hash(bytes).as_bytes()
}

/// 由名字**派生**项目 id，不是自增。
///
/// 两侧各自 `project add` 同一个项目必须算出同一个 id，否则 `W|1|…` 与 `W|3|…` 是两个
/// 键空间，push 时 commit/tree 的引用全对不上，而 union 会把它当成两个不相干的项目。
/// 代价：改项目名 = 换一个新项目（要写进 §16.1）。
pub fn project_id(name: &str) -> ProjectId {
    let h = hash_of(format!("twosync:project:{name}").as_bytes());
    u32::from_be_bytes([h[0], h[1], h[2], h[3]])
}

fn keyed(tag: u8, pid: ProjectId, tail: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(5 + tail.len());
    k.push(tag);
    k.extend_from_slice(&pid.to_be_bytes());
    k.extend_from_slice(tail);
    k
}

/// 某标签 + 某项目的公共前缀，喂给 `prefix_scan` / `key_count`。
pub fn project_prefix(tag: u8, pid: ProjectId) -> Vec<u8> {
    keyed(tag, pid, &[])
}

pub fn commit_key(pid: ProjectId, hash: &Hash) -> Vec<u8> {
    keyed(TAG_COMMIT, pid, hash)
}

/// ordinal 在后 ⇒ 一个 commit 的全部亲 = `prefix_scan(c|p|hash)`，且按 ordinal 有序。
pub fn parent_key(pid: ProjectId, hash: &Hash, ordinal: u32) -> Vec<u8> {
    let mut tail = hash.to_vec();
    tail.extend_from_slice(&ordinal.to_be_bytes());
    keyed(TAG_PARENT, pid, &tail)
}

pub fn tree_key(pid: ProjectId, hash: &Hash) -> Vec<u8> {
    keyed(TAG_TREE, pid, hash)
}

pub fn blob_key(pid: ProjectId, hash: &Hash) -> Vec<u8> {
    keyed(TAG_BLOB, pid, hash)
}

pub fn block_key(pid: ProjectId, hash: &Hash) -> Vec<u8> {
    keyed(TAG_BLOCK, pid, hash)
}

/// 按 refcnt 排序的二级索引：GC 找 0 是 `prefix_scan(k|p|0u64_be)`，不是全表扫（§9.3）。
///
/// **主表 `K` 与这张 `k` 必须同事务内一起改**，否则漂移；不变量 (g) 钉的就是这件事。
pub fn refcnt_key(pid: ProjectId, refcnt: u64, hash: &Hash) -> Vec<u8> {
    let mut tail = refcnt.to_be_bytes().to_vec();
    tail.extend_from_slice(hash);
    keyed(TAG_REFCNT, pid, &tail)
}

/// path 在键尾 ⇒ `prefix_scan(W|p|"src/")` 就是一棵子树（§8 的"只扫脏子树"靠它）。
///
/// **目录行是例外，键尾多一个 `/`**（见 [`worktree_dir_key`]）。理由不是美观：`W` 的键序
/// 就是字节序，而 `'/'`(0x2F) 夹在 `.`(0x2E) 与 `0`(0x30) 之间 ⇒ 目录行若叫 `src`，它的子项
/// `src/main.rs` 就会被兄弟文件 `src.rs` 隔开，而流式建树的栈只认"上一层走完没有"——
/// 结果是同一个目录被建两次帧、父层出现两条同名条目、`commit` 直接失败（§17 补记十五，
/// 由 §10.1(b) 的随机用例抓到）。键成 `src/` 之后"目录自己 + 它全部子项"共享同一个前缀，
/// 在字节序里**必然连续**，栈式遍历的前提才成立。git 的 tree 排序是同一条规矩。
pub fn worktree_key(pid: ProjectId, path: &str) -> Vec<u8> {
    keyed(TAG_WORKTREE, pid, path.as_bytes())
}

/// 目录行。读回来时由 `build::LiveRows` 去掉尾随的 `/`，所以下游看到的路径形状不变。
pub fn worktree_dir_key(pid: ProjectId, path: &str) -> Vec<u8> {
    let mut tail = path.as_bytes().to_vec();
    tail.push(b'/');
    keyed(TAG_WORKTREE, pid, &tail)
}

/// 扫描标记键。`Z|p` 定长 5 字节、无尾段，所以 `project_prefix(TAG_SCAN, pid)` 就是它本身。
pub fn scan_key(pid: ProjectId) -> Vec<u8> {
    project_prefix(TAG_SCAN, pid)
}

/// ref_name 是变长的，后面还要接 32 B hash ⇒ 必须用 `0x00` 分隔，否则
/// `H|p|"main"+hashA` 可能被解释成 `H|p|"main…"+…`，两个项目的 head 会互相串。
pub fn head_key(pid: ProjectId, ref_name: &str, hash: &Hash) -> Vec<u8> {
    debug_assert!(!ref_name.contains('\0'), "ref_name 不许含 NUL");
    let mut tail = ref_name.as_bytes().to_vec();
    tail.push(0x00);
    tail.extend_from_slice(hash);
    keyed(TAG_HEAD, pid, &tail)
}

/// 大小写折叠唯一映射（§8）：值 = canonical path。
pub fn folded_key(pid: ProjectId, folded: &str) -> Vec<u8> {
    keyed(TAG_FOLDED, pid, folded.as_bytes())
}

pub fn session_key(pid: ProjectId, session_id: u64) -> Vec<u8> {
    keyed(TAG_SESSION, pid, &session_id.to_be_bytes())
}

/// 站点级，无 project 前缀（§9.5 的 TOFU 表）。
pub fn peer_key(peer_id: &[u8]) -> Vec<u8> {
    let mut k = vec![TAG_PEER];
    k.extend_from_slice(peer_id);
    k
}

/// pending-delete 的一行：`D|p|hash` → `{size}`。**值里带 size** 是有意的：
/// unlink 那一趟要按字节数从 FR-15 的桶里扣，而这不该再回头点查 `K`（门面每次读折全库，§9.6.1）。
pub fn pending_key(pid: ProjectId, hash: &Hash) -> Vec<u8> {
    keyed(TAG_PENDING, pid, hash)
}

/// NFC 归一化助手。**目前只服务于比较**（`fold_path`），还没有用于"仓库内路径的身份"——
/// 那一半要连带把原始磁盘名存进 `W`/清单才安全（在不自动归一化的文件系统上，NFC 形式
/// 找不回 NFD 名字的文件），属格式决定 ⇒ §14 Q13。
///
/// 背景：macOS 把名字存成 **NFD**（`é` = `e` + 组合音标），Linux 存原始字节 ⇒ 同一张脸
/// 两串字节。至少要让她在**比较**上相等，否则大小写/唯一性这类门形同虚设。
///
/// 已经是 NFC 的名字（绝大多数）返回 `Borrowed`，不多分配一次。
pub fn nfc(path: &str) -> std::borrow::Cow<'_, str> {
    if unicode_normalization::is_nfc(path) {
        std::borrow::Cow::Borrowed(path)
    } else {
        use unicode_normalization::UnicodeNormalization;
        std::borrow::Cow::Owned(path.nfc().collect())
    }
}

/// 一个**仓库内条目名**（不是路径）合不合法。M1 的接收侧在把对端清单落库之前先过这一关：
/// "名字里带 `/`"或"`..`"这种东西一旦进了树对象，等到 M3 检出时才发现就太晚了——那时它已经是
/// 某个合法 commit 的一部分，撤回来要动 head。
///
/// **两个分隔符都查，且与本机无关**：Linux 上跑的服务端必须拒掉带 `\` 的名字，
/// 否则那份历史推到 Windows 上就写不出来了（§8 的非法字符表是跨端的）。
///
/// 规则只有一份：将来 `checkout`/`restore` 落地时也该调它。现在 `history::resolve` 是"按段查表"，
/// 这类名字**查不到**——但"查不到"与"拒收"是两件事，别混（§10.3 的教训）。
pub fn legal_name(name: &str) -> crate::error::Result<()> {
    let bad = name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\u{0}');
    if bad {
        return Err(crate::error::Error::BadInput(format!(
            "对端给的条目名 {name:?} 不合法：不许是空、`.`、`..`，也不许含分隔符或 NUL"
        )));
    }
    Ok(())
}

/// §8：折叠键 = **NFC** + `/` 分隔 + 小写。只用于比较/唯一性，不参与任何 hash 身份。
///
/// 折叠这一路的用途只有一个：回答"这两个名字放到大小写不敏感的盘上是不是同一个格子"。
pub fn fold_path(path: &str) -> String {
    nfc(path)
        .replace('\\', "/")
        .to_lowercase()
}

/// 前缀的**上界** = 字典序里第一个不以它开头的键。给 `scan(Some(lo), Some(hi))` 当 `hi`。
///
/// 返回 `None` 表示"没有上界"（前缀全 `0xFF` 或空），调用方该传 `None` 给 `scan`。
pub fn prefix_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut out = prefix.to_vec();
    while let Some(last) = out.last_mut() {
        if *last == 0xFF {
            out.pop(); // 进位：截断到前一位再 +1
        } else {
            *last += 1;
            return Some(out);
        }
    }
    None
}

/// 分页游标：读完 `key` 之后，下一窗从哪儿开始。
///
/// `key + [0x00]` 严格大于 `key`，且小于任何以 `key` 为前缀的更长键 ⇒ 只跳过这一条。
/// 前提：我们的键尾段（UTF-8 path / 定长 hash / 带 0x00 分隔的 ref）不会正好以 `0x00` 结尾。
pub fn cursor_after(key: &[u8]) -> Vec<u8> {
    let mut next = key.to_vec();
    next.push(0x00);
    next
}

pub fn hex(hash: &Hash) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// [`hex`] 的反向：从文件名还原 hash。长度不对或含非十六进制字符就返回 `None` ——
/// 调用方是在**扫盘**，盘上任何奇怪的东西都不该被硬编成"当作某个 hash"处理。
pub fn unhex(s: &str) -> Option<Hash> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = (b[i * 2] as char).to_digit(16)?;
        let lo = (b[i * 2 + 1] as char).to_digit(16)?;
        *slot = (hi * 16 + lo) as u8;
    }
    Some(out)
}

/// `log`/`status` 里显示用的短摘要。
pub fn short(hash: &Hash) -> String {
    hex(hash)[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn h(seed: u8) -> Hash {
        [seed; 32]
    }

    /// 派生而非自增：这是"两侧独立 add 也对得上"的根。
    #[test]
    fn project_id_is_derived_and_stable() {
        assert_eq!(project_id("trading-arb"), project_id("trading-arb"));
        assert_ne!(project_id("trading-arb"), project_id("trading-arb2"));
        assert_ne!(project_id("a"), project_id("b"));
        // 钉一个具体值：改算法/改前缀串必须让这条红，而不是悄悄换一套 id。
        assert_eq!(project_id("trading-arb"), 0xcea0_7932);
    }

    /// BE 的全部意义：字节序 == 逻辑序。
    #[test]
    fn numeric_fields_sort_in_byte_order() {
        let pid = project_id("p");
        // 先按数值生成、倒过来、再按字节排序：若 BE 生效，应当回到 0..=300。
        // 用 LE 的话 256 会排到 1 前面，所以跨过 256 的区间才有判别力。
        let mut shuffled: Vec<(u64, Vec<u8>)> = (0..=300u64)
            .map(|r| (r, refcnt_key(pid, r, &h(0))))
            .collect();
        shuffled.reverse();
        shuffled.sort_by(|a, b| a.1.cmp(&b.1));
        let nums: Vec<u64> = shuffled.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            nums,
            (0..=300u64).collect::<Vec<_>>(),
            "按字节排序应等于按数值排序"
        );

        let p10 = parent_key(pid, &h(1), 10);
        let p200 = parent_key(pid, &h(1), 200);
        assert!(p10 < p200, "ordinal 也必须字节序即逻辑序");
    }

    #[test]
    fn prefix_upper_bound_increments_with_carry() {
        assert_eq!(prefix_upper_bound(&[0x00, 0x41]), Some(vec![0x00, 0x42]));
        assert_eq!(prefix_upper_bound(&[0x00, 0xFF]), Some(vec![0x01]));
        assert_eq!(prefix_upper_bound(&[0xFF]), None, "全 0xFF ⇒ 无上界");
        assert_eq!(prefix_upper_bound(&[]), None);
    }

    /// 上界必须"含住所有该前缀的键、且不含下一个前缀的第一个键"。
    #[test]
    fn prefix_upper_bound_bounds_exactly_the_prefix() {
        let pid = project_id("p");
        let lo = project_prefix(TAG_WORKTREE, pid);
        let hi = prefix_upper_bound(&lo).unwrap();
        for path in ["a", "a/b", "a/b/c", "\u{7f}", "\u{ffff}"] {
            let k = worktree_key(pid, path);
            assert!(k >= lo && k < hi, "{path} 应落在 [{lo:?}, {hi:?}) 内");
        }
        let other = project_prefix(TAG_WORKTREE, pid.wrapping_add(1));
        assert!(other >= hi || other < lo, "相邻项目的键不许落进这一窗");
    }

    #[test]
    fn cursor_after_skips_only_the_read_key() {
        let keys: BTreeSet<Vec<u8>> = ["a", "ab", "ac", "b"]
            .into_iter()
            .map(|p| worktree_key(1, p))
            .collect();
        let first = worktree_key(1, "a");
        let cursor = cursor_after(&first);

        // 游标只负责"跳过已读的这一条"，不负责收尾：b 仍在范围内。
        // 这里必须用 owned `Vec<u8>` 当 range 边界：写 `cursor.as_slice()` 会让
        // `BTreeSet::range` 把查询类型推成 `&[u8]`，报 `Vec<u8>: Borrow<&[u8]>` 不满足。
        let rest: Vec<_> = keys.range(cursor.clone()..).cloned().collect();
        assert_eq!(rest.len(), 3, "游标只该跳过 a 本身");
        assert_eq!(
            rest[0],
            worktree_key(1, "ab"),
            "ab 必须还在，不能被游标当成 a 一起跳过"
        );

        // 一窗 = 游标当下界 + 前缀上界当上界；两者配合才切得出 "a" 这一子树。
        let upper = prefix_upper_bound(&worktree_key(1, "a")).unwrap();
        let window: Vec<_> = keys.range(cursor..upper).cloned().collect();
        assert_eq!(window, vec![worktree_key(1, "ab"), worktree_key(1, "ac")]);
    }

    /// 目录行的键尾带 `/` ⇒ "目录自己 + 它全部子项"在字节序里**连续**。
    /// 这是流式建树的栈唯一成立的前提：不连续时一个目录会被结算两次，父层就出现两条同名条目
    /// （§17 补记十五那个真 bug）。这里把"谁排在谁前面"逐位钉死，改排序规则立刻红。
    #[test]
    fn dir_keys_keep_a_subtree_contiguous() {
        let pid = project_id("p");
        // (显示名, 键)：`src` 是目录，其余是文件/目录混排，专门挑 `'.'` `'/'` `'0'` 这三个字节。
        let rows = [
            ("f:main.rs", worktree_key(pid, "main.rs")),
            ("f:src.rs", worktree_key(pid, "src.rs")),
            ("d:src", worktree_dir_key(pid, "src")),
            ("f:src/a", worktree_key(pid, "src/a")),
            ("d:src/n", worktree_dir_key(pid, "src/n")),
            ("f:src/n/x", worktree_key(pid, "src/n/x")),
            ("f:src0", worktree_key(pid, "src0")),
        ];
        let mut sorted = rows.clone();
        sorted.sort_by(|a, b| a.1.cmp(&b.1));
        let order: Vec<&str> = sorted.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            order,
            vec![
                "f:main.rs",
                "f:src.rs",
                "d:src",
                "f:src/a",
                "d:src/n",
                "f:src/n/x",
                "f:src0",
            ],
            "`src` 这一层必须自己开个头、子项紧跟其后，中间不许插进任何别的条目"
        );
        // 子树前缀扫的边界也一并钉住：`src/` 的上界正好把 `src0` 挡在外面。
        let lo = worktree_dir_key(pid, "src");
        let hi = prefix_upper_bound(&lo).unwrap();
        assert!(worktree_key(pid, "src0") >= hi, "兄弟 `src0` 不该落在 src 的子树窗里");
    }

    /// path 在尾段 ⇒ 子树前缀扫成立，且不会误吞同目录名的兄弟。
    #[test]
    fn path_tail_makes_subtree_scans_exact() {
        let pid = project_id("p");
        let dir = worktree_key(pid, "src/");
        assert!(worktree_key(pid, "src/a.rs").starts_with(&dir));
        assert!(!worktree_key(pid, "srcx/a.rs").starts_with(&dir));
    }

    /// ref_name 变长 + 分隔符：少了 0x00，两个 head 就能互相伪装。
    #[test]
    fn head_keys_are_unambiguous() {
        let pid = project_id("p");
        let a = head_key(pid, "main", &h(0xAA));
        let b = head_key(pid, "mainx", &h(0xBB));
        assert_ne!(a, b);
        assert!(
            !b.starts_with(&a[..a.len() - 32]),
            "短 ref 的键不该是长 ref 的前缀骨架"
        );
        assert_eq!(head_key(pid, "main", &h(1)), head_key(pid, "main", &h(1)));
    }

    #[test]
    fn fold_path_collapses_case_and_separators() {
        assert_eq!(fold_path("Src\\Readme.MD"), fold_path("src/readme.md"));
        assert_eq!(fold_path("a/b"), "a/b");
        assert_ne!(
            fold_path("a/b"),
            fold_path("a\\b\\"),
            "尾分隔符不同不该被抹平成一样"
        );
    }

    #[test]
    fn hash_is_full_32_bytes_and_content_addressed() {
        assert_eq!(hash_of(b"x"), hash_of(b"x"));
        assert_ne!(hash_of(b"x"), hash_of(b"y"));
        assert_eq!(hex(&hash_of(b"abc")).len(), 64, "落盘键用满 32 B，不许截断");
        assert_eq!(short(&hash_of(b"abc")).len(), 8);
    }
}
