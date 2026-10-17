//! 从工作树快照构建一个 commit：落块 → 写清单 → 自底向上建树 → **边结算边比对** → 移 head。
//!
//! # 内存：一趟，不是两趟
//!
//! 融合之前是「先把新树攒出来（`plan` 攒在内存、`commit` 落进库），再自顶向下 diff」。
//! 那条路的代价是 `plan` 必须留着 O(文件数) 的清单给 diff 递归，于是有 `MAX_ENTRIES`
//! 那个**项目级**上限——它意味着"项目大到一定程度就提不了交、连看一眼都不行"。
//!
//! 现在每层目录结算时顺手与参照版本对应层比掉，清单出栈即丢，所以栈上同时只有
//! **一条祖先链的新旧两份清单**：**我们这一层**的内存尺度是「最大单个目录 × 深度」，
//! 与文件总数无关。上限也因此改成 [`MAX_DIR_ENTRIES`]——收的是**一个目录的条目数**，
//! 兜的是"某一层大到装不下"这一种形状；超了照样报错拒绝，绝不 OOM（OOM 会把同机的策略
//! 进程一起拖下水，那是 NFR-3 的红线）。
//!
//! **但进程峰值不等于这一层**：引擎门面没有游标，每调用一次 `search_keys`/`prefix_scan`
//! 都要先把整个库折叠一遍（实测 207 B/键，见 [`PAGE`] 与 `tests/engine_smoke.rs`）。
//! 所以 NFR-1 那句"读路径内存与文件数无关"在 0.4.0 上要读成"**我们这侧**无关，
//! 引擎那侧 O(全库存活键)"——这一条已经就地改进 §9.6.1 的判据，别让它停在口头。
//!
//! # 换来的是什么代价（这是权衡，不是免费午餐）
//!
//! 比对自底向上，而"这一层变了没有"要到结算时才第一次知道 ⇒ 想知道就得先拿到这一层的旧
//! 清单 ⇒ 参照版本那一侧的读只能压在**下栈时**。于是 `plan`/`commit` 每有一个目录就点查
//! 一次它的旧清单：**O(目录数) 次 `get`**，不是 O(文件数)。"hash 相同整块子树跳过"那条
//! NFR-6 的快路径现在只活在 [`walk_pair`] 里（两棵树都在库上，可以自顶向下走，
//! 也就是 `diff --from/--to` 那一类比对）。
//!
//! # 为什么 refcnt 必须在这里就记对
//!
//! `K.refcnt` 是 GC 唯一的依据。少记一次 = 块被提前删除 = 历史取不回来（不变量 (b) 破）。
//! 所以"新 blob 才加引用、老 blob 不重复加"这条判断（`is_new_blob`）不能省，也不能推迟到
//! GC 那轮再补——GC 看不到"本该 +1 却没 +1"的差额。

use std::collections::BTreeMap;
use std::format;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use storage_engines::KvEngine;
use storage_engines::SearchQuery;

use crate::error::{Error, Result};
use crate::store::keys::{self, Hash, Kind, ProjectId};
use crate::store::worktree::WtEntry;
use crate::store::{cas, object, worktree};

/// **一个目录**最多攒多少条目。超过就拒绝，不 OOM。
///
/// 它替代了老的项目级上限：项目级的那一条意味着"文件多到一定程度连 `status` 都跑不动"，
/// 而融合遍历之后**我们这一层**的内存只跟单层挂钩（进程峰值另有一笔引擎账，见下面 [`PAGE`]），
/// 所以盖子也该盖在单层上。
pub const MAX_DIR_ENTRIES: usize = 200_000;
/// 分页读引擎的窗口。**2026-10-17 实测后改判：这个旋钮管的是 CPU，不是内存。**
///
/// 引擎门面（`dyn KvEngine` 的 `Transaction`）只有 `scan` / `prefix_scan` / `search_keys`
/// 三个读出口，**没有** foreach/游标（`prefix_scan_foreach` 只活在各引擎的具体类型上，
/// 门面 0.4.0 一个都没透出来）。而 `search_keys` 内部先 `collect_latest_raw` 把**整个库**
/// 折成 `winners`、再按 pattern 过滤、最后才切页 ⇒ 一次调用必然物化全库存活键
/// （实测 207 B/键），窗口缩到 100 也一字节不少
/// （`tests/engine_smoke.rs::search_keys_folds_the_whole_db` 钉着这条）。
///
/// 所以**页数 = 全库折叠的次数**。原来取 2 000 的理由是"内存与文件数无关"，那个理由在
/// 0.4.0 上不成立，代价却是 22 000 行的项目读一遍 `W` 要折叠 11 次。同一份库上实测
/// `status`：2 000 → 9.99 s、20 000 → **2.87 s**、100 000 → 2.07 s，而峰值
/// 35.8 / 37.7 / 35.4 MiB 基本不动（大头就是那一次全库折叠）。取 20 000：折叠次数除以 10，
/// 我们自己那一层的页缓冲只多约 2 MiB，仍然是有界的。
/// **真正的内存阀在引擎里**——等上游给 `collect_latest_raw` 加 `[start,end)` 边界，
/// 或者门面把 `prefix_scan_foreach` 透出来，这一条才能改回小窗口（§17 补记十）。
pub const PAGE: usize = 20_000;
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
    plan_capped(db, pid, MAX_DIR_ENTRIES)
}

/// `plan` 的实现，但**单层**内存上限可注入——给用例留着"超限要报错、不许 OOM"这条可测性。
pub fn plan_capped(db: &dyn KvEngine, pid: ProjectId, dir_cap: usize) -> Result<Built> {
    walk_readonly(db, pid, head_tree(db, pid)?, "", dir_cap)
}

/// 工作树（按最后一次扫描）与**任意一个版本**的差：`diff --at REV` 用的就是这一条。
///
/// 与 [`plan`] 同一个遍历、同一个口径，只是参照版本换了一个。`old_root = None` 的语义是
/// "没有可比的历史"⇒ 全部按新增记，而且参照侧一次都不读。
pub fn compare(db: &dyn KvEngine, pid: ProjectId, old_root: Option<Hash>) -> Result<Built> {
    walk_readonly(db, pid, old_root, "", MAX_DIR_ENTRIES)
}

/// `compare` 的限定版：只比 `at` 那一棵子树（`checkout --prefix` 的坐标）。
///
/// 回调拿到的路径是**工作树里的完整路径**，不是相对 `at` 的——否则每一个落地动作都要
/// 再把前缀拼回去，而拼错一次就是写到别的目录去。
pub fn compare_subtree(
    db: &dyn KvEngine,
    pid: ProjectId,
    old_root: Option<Hash>,
    at: &str,
) -> Result<Built> {
    walk_readonly(db, pid, old_root, at, MAX_DIR_ENTRIES)
}

fn walk_readonly(
    db: &dyn KvEngine,
    pid: ProjectId,
    old_root: Option<Hash>,
    at: &str,
    dir_cap: usize,
) -> Result<Built> {
    let mut rows = LiveRows::with_prefix(db, pid, at);
    let mut built = Built::default();
    let mut changes = ChangeSet::default();
    let walk = Walk {
        db,
        pid,
        old_root,
        at,
        dir_cap,
    };
    walk.run(
        &mut rows,
        &mut HashOnly,
        &mut built,
        &mut |kind, path, oe, ne| {
            note_change(&mut changes, kind, path, oe, ne);
            Ok(())
        },
    )?;
    // 这三个数只能在遍历**之后**读：迭代器是分页的，走完才是全量。
    built.entries = rows.live as usize;
    built.known = rows.seen_rows;
    built.epoch = rows.mark;
    built.changes = changes;
    Ok(built)
}

/// 变更回调 → `ChangeSet` 的那一笔账。**只有一份**：`plan`/`commit`/`checkout` 报的是
/// 同一个数，口径才不会在第二处漂掉。
fn note_change(
    cs: &mut ChangeSet,
    kind: ChangeKind,
    path: &str,
    oe: Option<&object::TreeEntry>,
    ne: Option<&object::TreeEntry>,
) {
    // 只有文件有 blob：目录的 hash 是 tree，符号链接的 hash 是目标文本的 blob。
    if kind == ChangeKind::Delete
        && let Some(e) = oe
        && e.kind == Kind::File
    {
        cs.deleted_blobs.push(e.hash);
    }
    cs.note(kind, path.to_string(), ne.or(oe).map_or(0, |e| e.size));
}

/// 构建并落库一个 commit。无改动时返回 `commit: None`，**不产生空提交也不移 head**。
pub fn build(
    db: &dyn KvEngine,
    pid: ProjectId,
    store: &cas::Cas,
    work_root: &Path,
    meta: Meta<'_>,
) -> Result<Built> {
    let prev_heads = ref_heads(db, pid, MAIN)?;
    let prev_tree = head_tree(db, pid)?;

    let mut rows = LiveRows::new(db, pid);
    let mut built = Built::default();
    let mut changes = ChangeSet::default();
    let walk = Walk {
        db,
        pid,
        old_root: prev_tree,
        at: "",
        dir_cap: MAX_DIR_ENTRIES,
    };
    // 建树与比对是同一趟：每结算一层就顺手与参照版本比掉，清单出栈即丢。
    // 融合之前这里要先攒树、再自顶向下读一遍库来 diff。
    walk.run(
        &mut rows,
        &mut WriteSink { store, work_root },
        &mut built,
        &mut |kind, path, oe, ne| {
            note_change(&mut changes, kind, path, oe, ne);
            Ok(())
        },
    )?;
    let known = rows.seen_rows;
    built.entries = rows.live as usize;
    built.known = known;
    built.epoch = rows.mark;
    built.changes = changes;
    if built.changes.is_empty() {
        return Ok(built); // 不产生空提交：碎 commit 会把引擎版本链白白撑大（§9.6.2）
    }
    check_mass_delete(
        built.changes.delete,
        built.changes.deleted_bytes,
        known,
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

/// 组装阶段"要不要落盘"的决策点。
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

/// 只算 hash、不落盘，也不**留**任何清单：喂给 `plan` / `compare` / `checkout`。
///
/// 融合之后它连一个字段都没有了——老版本要往里塞 `TreeStore`，是因为 diff 得等整棵树
/// 攒完才能自顶向下递归；现在比对压在每层结算的那一刻，清单用完就丢。
pub struct HashOnly;

impl Sink for HashOnly {
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
        // `tree_hash` 内部就是"排序 + 重名校验 + 取 hash"，与落盘那份字节完全同一套规范序，
        // 所以 `plan` 算出的树与随后 `commit` 落库的树必然同 hash（用例钉着这条）。
        Ok(object::tree_hash(entries))
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

/// 一条 `W` 流的读取器：按 [`PAGE`] 翻页、按活性线过滤、**我们自己这一层只持有一窗**。
///
/// 为什么不再像老实现那样先攒一个 `Vec`：那本身就是 O(文件数)，而融合遍历之后根本不需要
/// 它——每层目录结算完就把清单丢了。
///
/// 但**别把这条读成"内存与文件总数无关"**：引擎门面没有游标，`search_keys` 每次调用都要先把
/// 整个库折叠一遍（见 [`PAGE`] 那段与 `tests/engine_smoke.rs`），所以进程峰值是
/// **O(全库存活键)**、住在引擎里，而我们这一层是 O(窗口)。CPU 同理是 O(页数 × 全库)。
/// 上游给出带边界的读出口之前，这条就是 0.4.0 的地板；已经把判据写进 §9.6.1 与 §17 补记十。
pub struct LiveRows<'a> {
    db: &'a dyn KvEngine,
    prefix: Vec<u8>,
    /// 非空 = 只读这一棵子树的行（`checkout --prefix`）。键前缀多拼一个 `/`，
    /// 于是 `W|p|a/b` 这一条（目录自己）落在窗外、不参与建树——根帧由 `Walk::at` 给出。
    at: String,
    /// 活性线：`scanned_at >= epoch` 的行才算活着。
    epoch: i64,
    /// `Z` 缺失 = 从没完整扫描过 ⇒ 一律不算活（把活性线抬到 `i64::MAX` 让谁都对不上）。
    pub complete: bool,
    page: usize,
    buf: std::vec::IntoIter<(String, WtEntry)>,
    done: bool,
    /// 扫过的全部 `W` 行（含不活的）。FR-10 的分母用它；迭代走完才完整。
    pub seen_rows: u32,
    /// 其中活着的行数 = 本轮要进树的数量。
    pub live: u32,
    /// 最近一次完整扫描的 epoch，原样带给命令层显示。
    pub mark: i64,
}

impl<'a> LiveRows<'a> {
    pub fn new(db: &'a dyn KvEngine, pid: ProjectId) -> Self {
        Self::with_prefix(db, pid, "")
    }

    /// 只读 `at` 那一棵子树。给出的路径仍然是**工作树完整路径**：
    /// 每一层帧都按完整路径算父目录，相对路径会让根帧的子项挂到错误的层上去。
    pub fn with_prefix(db: &'a dyn KvEngine, pid: ProjectId, at: &str) -> Self {
        let mut prefix = keys::project_prefix(keys::TAG_WORKTREE, pid);
        if !at.is_empty() {
            prefix.extend_from_slice(at.as_bytes());
            prefix.push(b'/');
        }
        let mut me = Self {
            db,
            prefix,
            at: at.to_string(),
            epoch: i64::MAX,
            complete: false,
            page: 0,
            buf: Vec::new().into_iter(),
            done: false,
            seen_rows: 0,
            live: 0,
            mark: 0,
        };
        if let Some(m) = worktree::last_mark(db, pid) {
            me.epoch = m.epoch;
            me.complete = true;
            me.mark = m.epoch;
        }
        me
    }

    /// 键尾巴 → 路径。限定子树时尾巴是**相对 `at`** 的，必须拼回完整路径。
    fn path_of(&self, tail: &[u8]) -> String {
        let tail = String::from_utf8_lossy(tail);
        if self.at.is_empty() {
            tail.into_owned()
        } else {
            format!("{}/{}", self.at, tail)
        }
    }

    /// 取下一页。返回 `false` = 已经没有更多页。
    fn fetch(&mut self) -> Result<bool> {
        let found = {
            let tx = self.db.begin();
            let found = tx.search_keys(&SearchQuery::prefix(self.prefix.clone(), self.page, PAGE));
            tx.commit();
            found
        };
        let mut keep = Vec::with_capacity(found.items.len());
        for r in found.items {
            self.seen_rows += 1;
            let Some((_, path)) = r.key.split_at_checked(self.prefix.len()) else {
                continue;
            };
            // `value` 只在 `include_deleted(true)` 时才是 None（tombstone），我们没开。
            let Some(raw) = r.value else { continue };
            let e: WtEntry = dec(&raw, "WtEntry")?;
            if e.scanned_at >= self.epoch {
                keep.push((self.path_of(path), e));
            }
        }
        self.buf = keep.into_iter();
        self.page += 1;
        Ok(self.page < found.total_pages)
    }
}

impl Iterator for LiveRows<'_> {
    type Item = Result<(String, WtEntry)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(v) = self.buf.next() {
                self.live += 1;
                return Some(Ok(v));
            }
            if self.done {
                return None;
            }
            // 先清缓冲再判结束：`fetch` 报"没有更多页"的那一页，数据**还在 buf 里**，
            // 先返回 None 就等于把最后一页整个丢掉（老写法是一次性攒完，没这个坑）。
            match self.fetch() {
                Ok(more) => self.done = !more,
                Err(e) => {
                    // 解不开就是数据坏了，**不许当成"没有更多"**：静默少一半行，
                    // 下一轮 commit 会把那些路径全判成删除。
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

/// 一个还没结算的目录。
struct Frame {
    dir: String,
    mode: u32,
    entries: Vec<object::TreeEntry>,
    /// 这一层在**参照版本**里的清单。`None` = 参照里没有这一层（整棵都是新的），
    /// 按空清单比 ⇒ 子项全记 Add。
    old: Option<Vec<object::TreeEntry>>,
}

/// `d` 是 `p` 的**真**祖先（或根）。路径都是 `/` 分隔、无尾斜杠。
fn is_ancestor(d: &str, p: &str) -> bool {
    d.is_empty() || p.strip_prefix(d).is_some_and(|rest| rest.starts_with('/'))
}

/// 从 `cur` 往 `parent` 方向再下一层。`cur` 必须是 `parent` 的祖先。
fn next_ancestor(cur: &str, parent: &str) -> String {
    let rest = parent
        .strip_prefix(cur)
        .unwrap_or(parent)
        .trim_start_matches('/');
    let seg = rest.split('/').next().unwrap_or(rest);
    if cur.is_empty() {
        seg.to_string()
    } else {
        format!("{cur}/{seg}")
    }
}

/// 融合遍历的坐标：跟哪个版本比、从哪个目录开始、单层的内存盖子多大。
///
/// 收成一个结构体是因为这四样必须**一起**成立：比对参照、起点、上限三者散着传，
/// 就会出现"`--prefix` 用了子树却拿整棵树当参照"这种只错一个参数的静默结果。
pub struct Walk<'a> {
    pub db: &'a dyn KvEngine,
    pub pid: ProjectId,
    /// 参照版本（通常是 HEAD，`checkout` 传目标版本）的根 tree。`None` = 库里还没有
    /// 历史 ⇒ 全部按新增记，并且参照侧一次都不读。
    pub old_root: Option<Hash>,
    /// 根帧对应的工作树目录：`""` = 整个工作树，`checkout --prefix a/b` 传 `"a/b"`。
    pub at: &'a str,
    /// 单个目录的条目上限（[`MAX_DIR_ENTRIES`]）。
    pub dir_cap: usize,
}

impl Walk<'_> {
    /// 一趟做完：读 `rows` → 逐层建树（`sink` 决定落不落盘）→ 每层结算时与参照比掉。
    ///
    /// `rows` 必须是**升序路径流**（[`LiveRows`] 给的就是这个序）。返回时
    /// `built.tree` = 结果根清单的 hash，`visit` 已经收到全部变更。
    pub fn run<S: Sink>(
        &self,
        rows: &mut LiveRows<'_>,
        sink: &mut S,
        built: &mut Built,
        visit: &mut Visit<'_>,
    ) -> Result<()> {
        let mut b = Builder::new(self, built)?;
        for item in &mut *rows {
            let (path, e) = item?;
            b.row(&path, &e, sink, visit)?;
        }
        built.tree = b.finish(sink, visit)?;
        Ok(())
    }
}

/// 流式建树 + 逐层比对的**栈**。
///
/// 内存 = O(一条祖先链上各层的清单，新旧各一份)，与文件总数无关。根那一帧装着顶层全部
/// 条目——树对象本身就是一层一份清单，要算它的 hash 就得把这一层拿在手里，这是格式的
/// 固有代价，git 的 tree 也一样。
///
/// 老实现是"先按目录把所有条目分桶，再按逆字典序自底向上收"。顶层目录静默丢失那个 bug
/// （§17 补记二）就住在那套分桶 + `collect_children` 里，而它编译期防不住；栈式只需要问
/// 一句"栈顶还是不是我的父目录"，收完立刻结算并比对，没有第二份索引要维护。
struct Builder<'a, 'w> {
    db: &'w dyn KvEngine,
    pid: ProjectId,
    dir_cap: usize,
    built: &'a mut Built,
    stack: Vec<Frame>,
}

impl<'a, 'w> Builder<'a, 'w> {
    fn new(w: &Walk<'w>, built: &'a mut Built) -> Result<Self> {
        // 根帧的参照清单只在"真有历史"时读一次；没有历史就连这一次都不读。
        let old = match w.old_root {
            Some(h) => Some(object::read_tree(w.db, w.pid, &h)?),
            None => None,
        };
        Ok(Self {
            db: w.db,
            pid: w.pid,
            built,
            dir_cap: w.dir_cap,
            stack: vec![Frame {
                dir: w.at.to_string(),
                mode: 0o40755,
                entries: Vec::new(),
                old,
            }],
        })
    }

    fn row(
        &mut self,
        path: &str,
        e: &WtEntry,
        sink: &mut dyn Sink,
        visit: &mut Visit<'_>,
    ) -> Result<()> {
        let (parent, name) = split(path);
        self.align(parent, sink, visit)?;
        if e.kind == Kind::Dir {
            // 目录自己也要成为父目录的一条条目，但它的 hash 要等结算完才知道 ⇒
            // 这里只压一帧，条目在 `close_top` 里补。
            self.push(path, e.mode)?;
            return Ok(());
        }
        let hash = sink.blob_of(self.db, self.pid, path, e, self.built)?;
        let cap = self.dir_cap;
        let top = self.top();
        if top.entries.len() + 1 > cap {
            return Err(Error::TooManyEntries {
                n: top.entries.len() + 1,
                max: cap,
                // 根目录的内部表示是空字符串，直接印出来是 `""`——用户看不懂该拆哪一层。
                dir: if top.dir.is_empty() {
                    ".".to_string()
                } else {
                    top.dir.clone()
                },
            });
        }
        top.entries.push(object::TreeEntry {
            name: name.to_string(),
            kind: e.kind,
            hash,
            size: e.size, // 目录/链接的逻辑大小恒 0，定义在 `worktree::logical_size`
            mode: e.mode,
        });
        Ok(())
    }

    /// 下一层目录：压栈，顺手把这一层的**参照清单**取出来。
    ///
    /// 只在父层参照清单存在、且里面同名条目确实是目录时才点查一次（O(目录数) 次 `get`
    /// 的账记在模块头）。同名的参照条目是文件时按"参照里没有这一层"处理：新目录里的每个
    /// 文件各记一次 Add，同名那一层在父层记 Modify——真实工作树里一个路径不可能同时是
    /// 文件和目录，这一格是给"上游 bug 或手工塞记录"留的诚实答案。
    fn push(&mut self, dir: &str, mode: u32) -> Result<()> {
        let name = dir.rsplit_once('/').map_or(dir, |(_, n)| n);
        let want = self
            .stack
            .last()
            .and_then(|p| p.old.as_ref())
            .and_then(|list| list.iter().find(|e| e.name == name))
            .filter(|e| e.kind == Kind::Dir)
            .map(|e| e.hash);
        let old = match want {
            Some(h) => Some(object::read_tree(self.db, self.pid, &h)?),
            None => None,
        };
        self.stack.push(Frame {
            dir: dir.to_string(),
            mode,
            entries: Vec::new(),
            old,
        });
        Ok(())
    }

    /// 把栈对齐到 `parent`：比它深的帧先结算出栈，比它浅的缺帧补上。
    fn align(&mut self, parent: &str, sink: &mut dyn Sink, visit: &mut Visit<'_>) -> Result<()> {
        while self.stack.len() > 1 {
            let top = self.top();
            if top.dir == parent || is_ancestor(&top.dir, parent) {
                break;
            }
            self.close_top(sink, visit)?;
        }
        // 中间层没有 `W` 行（比如手工塞进一条深层路径）⇒ 按段序补空帧。少了这一步，
        // 那条路径会挂到错误的父目录上去。老实现的 `ensure_dir` 干的是同一件事。
        while self.top().dir != parent {
            let cur = self.top().dir.clone();
            let next = next_ancestor(&cur, parent);
            self.push(&next, 0o40755)?;
        }
        Ok(())
    }

    /// 结算栈顶：算 hash → 与参照比掉 → 把目录条目补进父帧。
    fn close_top(&mut self, sink: &mut dyn Sink, visit: &mut Visit<'_>) -> Result<()> {
        let f = self.stack.pop().expect("根帧不许结算");
        let e = settle(self.db, self.pid, &f, sink, visit)?;
        self.top().entries.push(e);
        Ok(())
    }

    fn finish(mut self, sink: &mut dyn Sink, visit: &mut Visit<'_>) -> Result<Hash> {
        while self.stack.len() > 1 {
            self.close_top(sink, visit)?;
        }
        let f = self.stack.pop().expect("根帧一直在");
        Ok(settle(self.db, self.pid, &f, sink, visit)?.hash)
    }

    fn top(&mut self) -> &mut Frame {
        self.stack.last_mut().expect("根帧一直在")
    }
}

/// **结算一层**：算出这一层的 hash、顺手与参照版本的同一层比掉、给出补进父帧的目录条目。
///
/// 融合遍历的全部意义在这一条里：比对用的就是 `f.entries` 这份清单，而它出栈即丢，
/// 所以内存里没有"整棵新树"这种东西。顺序也不能换——先比对再算 hash 或者反过来都行，
/// 但**在结算里做**是唯一不用把清单留给第二趟的做法。
fn settle(
    db: &dyn KvEngine,
    pid: ProjectId,
    f: &Frame,
    sink: &mut dyn Sink,
    visit: &mut Visit<'_>,
) -> Result<object::TreeEntry> {
    let hash = sink.tree_of(db, pid, &f.entries)?;
    diff_level(
        db,
        pid,
        &f.dir,
        f.old.as_deref().unwrap_or(&[]),
        &f.entries,
        Nest::Settled,
        visit,
    )?;
    let name = f.dir.rsplit_once('/').map_or(f.dir.as_str(), |(_, n)| n);
    Ok(object::TreeEntry {
        name: name.to_string(),
        kind: Kind::Dir,
        hash,
        size: 0,
        mode: f.mode,
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

/// 一层的比对里，"两侧同为目录且不同"该怎么办。**这是融合遍历与 `walk_pair` 唯一的区别**，
/// 所以把它收成一个枚举而不是再抄一份分类代码。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Nest {
    /// 立刻递归进子层（两棵树都在库里，自顶向下）：hash 相同的子树整块跳过，一次读都不发。
    /// 这就是 NFR-6 的 O(变更目录数)。
    Recurse,
    /// 不递归：子层在**自己的结算点**已经比过了，这里再走一遍就是重复计数。
    Settled,
}

/// **一层目录**里两份清单的分类规则——全站只有这一份。
///
/// 为什么不各写一份：`diff`（只数数）、融合遍历（`plan`/`commit`）和 `checkout`（要真落地）
/// 问的是同一个问题——这一层里谁多了、谁少了、谁变了。分处写，"未变更就跳过"这类判断就会
/// 在各处各错一次，而 `checkout` 错的那一次是**删文件**。
///
/// 回调里 `old`/`new` 谁是 `None` 由 `kind` 决定：`Add` 没有 old，`Delete` 没有 new。
/// 目录的 `Add`/`Delete` 会连带把整棵子树的每个后代各记一次——与 `ChangeSet` 的计数口径一致。
///
/// `prefix` 是这一层的**完整路径**（`Walk::at` 非空时也带前缀），回调因此可以直接给出
/// 能喂给 `restore`/`checkout` 的路径，不必在每一处再拼一次。
fn diff_level(
    db: &dyn KvEngine,
    pid: ProjectId,
    prefix: &str,
    old: &[object::TreeEntry],
    new: &[object::TreeEntry],
    nest: Nest,
    visit: &mut Visit<'_>,
) -> Result<()> {
    let o: BTreeMap<&str, &object::TreeEntry> = old.iter().map(|e| (e.name.as_str(), e)).collect();
    let n: BTreeMap<&str, &object::TreeEntry> = new.iter().map(|e| (e.name.as_str(), e)).collect();

    for (name, ne) in &n {
        let full = join(prefix, name);
        let Some(oe) = o.get(*name) else {
            visit(ChangeKind::Add, &full, None, Some(ne))?;
            // `Settled` 时不递归：新目录里的子项在它自己结算时已经逐个报过 Add 了。
            if ne.kind == Kind::Dir && nest == Nest::Recurse {
                let kids = object::read_tree(db, pid, &ne.hash)?;
                diff_level(db, pid, &full, &[], &kids, nest, visit)?;
            }
            continue;
        };
        if oe.hash == ne.hash && oe.mode == ne.mode {
            continue; // 子树 hash 相同 ⇒ 整块跳过，一个条目都不读（NFR-6）
        }
        if oe.kind == Kind::Dir && ne.kind == Kind::Dir {
            if nest == Nest::Recurse {
                let kids_old = object::read_tree(db, pid, &oe.hash)?;
                let kids_new = object::read_tree(db, pid, &ne.hash)?;
                diff_level(db, pid, &full, &kids_old, &kids_new, nest, visit)?;
            }
            // 目录本身不算一条变更：它的变化全在子层里，子层各记各的。
            continue;
        }
        visit(ChangeKind::Modify, &full, Some(oe), Some(ne))?;
    }
    // 判删除只能问"这个名字还在不在新清单里"，不能问"刚才有没有被我碰过"：
    // 未变更的条目上一轮就 continue 了，拿那条线筛会把好文件报成删除。
    for oe in o.values().filter(|e| !n.contains_key(&e.name.as_str())) {
        let full = join(prefix, &oe.name);
        // **删除侧两种模式都必须递归**：只在参照里存在的那些目录，工作树侧根本没有帧，
        // 不递归就永远数不到它的后代。
        if oe.kind == Kind::Dir {
            let kids = object::read_tree(db, pid, &oe.hash)?;
            diff_level(db, pid, &full, &kids, &[], nest, visit)?;
        }
        // 先递归再回调自己（后序）：`checkout` 要在同一趟遍历里 `remove_dir`，
        // 而目录只有空了才删得掉。计数不受顺序影响，所以 `diff` 那边没有代价。
        visit(ChangeKind::Delete, &full, Some(oe), None)?;
    }
    Ok(())
}

/// 两棵**都在库里**的树的配对遍历，自顶向下、未变子树整块跳过。
///
/// `pub` 是给 `diff --at <rev>` 与 `checkout` 之外那类"任意两版相比"留的（M1/M2 的
/// `--from/--to`）。工作树相对某个版本的差**不走这条**——那棵树没落盘，走 [`Walk::run`]。
pub fn walk_pair(
    db: &dyn KvEngine,
    pid: ProjectId,
    prefix: &str,
    old: &[object::TreeEntry],
    new: &[object::TreeEntry],
    visit: &mut Visit<'_>,
) -> Result<()> {
    diff_level(db, pid, prefix, old, new, Nest::Recurse, visit)
}

/// 递归比对两棵库里树，产出一张变更表。**子树 hash 相同就整块跳过**。
///
/// `pub` 是给 `diff --at <rev>` 用的：那条要拿任意两个版本比，不只是跟 HEAD 比。
/// 工作树 vs 某个版本走 [`compare`]，因为工作树那侧的树从来没进过库。
pub fn diff(
    db: &dyn KvEngine,
    pid: ProjectId,
    old: Option<&Hash>,
    new: &Hash,
) -> Result<ChangeSet> {
    let mut out = ChangeSet::default();
    if old == Some(new) || (old.is_none() && *new == object::empty_tree()) {
        return Ok(out);
    }
    let old_list = match old {
        Some(h) => object::read_tree(db, pid, h)?,
        None => Vec::new(),
    };
    let new_list = object::read_tree(db, pid, new)?;
    let mut collect = |kind: ChangeKind,
                       path: &str,
                       oe: Option<&object::TreeEntry>,
                       ne: Option<&object::TreeEntry>|
     -> Result<()> {
        note_change(&mut out, kind, path, oe, ne);
        Ok(())
    };
    walk_pair(db, pid, "", &old_list, &new_list, &mut collect)?;
    Ok(out)
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
