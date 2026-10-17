//! §10.1 里**现在就能跑**的两条不变量，随机生成整棵工作树来跑：
//!
//! - **(b) 任意 commit 都能从 CAS 完整重建，逐 blob 比字节**。不是只比 hash——只比 hash
//!   等于没比：清单错但整文件 hash 恰好对上，这种数构造得出来。
//! - **(g) `K.refcnt` 与 `k` 排序索引永不漂移**，而且**三方**一起核：`K` 主表、`k` 索引、
//!   以及**独立地从全部 `B` 清单重算一遍引用数**。只核前两样是不够的——"该 +1 没 +1"会让
//!   两边一起错（GC 照着错数提前删块），第三样才是外部账。
//!
//! 剩下三条为什么不在这里：**(a)(c)(e) 要有第二个 peer 才有对象**（union / collision，M3），
//! **(d) 是 `gc` 的账**（M1）。给它们写"看起来测了"的用例比不测更糟。
//! **(f1) 内存那条**住在 `tests/plan_mem_prop.rs`：它要独占分配器水位线，不能和这批挤在一个进程里。
//!
//! 没覆盖到的（如实标，别当已过）：符号链接的 blob 往返（`restore_worktree` / `checkout_worktree`
//! 有定向用例，随机生成在 Windows 上要特权 ⇒ 这里不扇）、执行位 mode、被排除的路径。
//!
//! 跑法：`cargo test --test invariants_proptest`（约 20 s）。用例数在 `Config` 里收到 24：
//! 每轮都要真建目录、真落块、真开引擎，默认 256 轮会把这组用例变成分钟级。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use storage_engines::EngineKind;
use twosync::store::build::{self, BlockMeta, MAIN};
use twosync::store::cas::{Blob, Cas};
use twosync::store::keys::{self, Hash, Kind, ProjectId};
use twosync::store::{history, object, worktree};
use twosync::utils::{objects_dir, open_store};

/// 块大小取 4 KiB 而不是默认 4 MiB：默认值下随机小文件永远只有一个块，
/// "多块 + 末块短 + 块跨文件共享"三条路径一次都走不到，而 (b) 恰恰是**关于块清单**的判据。
const BLOCK: u64 = 4096;

/// 组件词表。刻意放了两类容易出事的：
/// - `src` 与 `src.rs`：**目录名是某个兄弟文件名的前缀**——§17 补记十一那条字节序陷阱靠它复现；
/// - 空格、中文、`#`：路径当键尾时的分隔与折叠。
/// - 不放"仅大小写不同"的两个名字：Windows 上根本建不出来，那是测平台不是测自己（§10.3 例外①）。
const NAMES: &[&str] = &[
    "src",
    "src.rs",
    "data",
    "data.bin",
    "a b",
    "中文档",
    "x#1",
    "n0",
    "n1",
    "a_long_name_that_is_still_legal",
];
/// 长度挑过一轮：0（空文件）、差一块、正好一块、跨两块——块清单的边界都在这几个数上。
const LENS: &[u32] = &[0, 1, 4095, 4096, 4097, 8192, 12288];

fn name_strat() -> BoxedStrategy<String> {
    prop::sample::select(NAMES)
        .prop_map(|s| s.to_string())
        .boxed()
}

/// 三种内容形状：唯一字节 / 全零（走 CAS 的"零块不落盘"）/ 前半全零后半有字节
/// ——**同一个文件里既有零块又有实块**，这是最容易把清单与 refcnt 算岔的形状。
/// 同样的 (tag, len, shape) 必然算出同样的字节 ⇒ 块跨文件共享是自然出现的，不用特判。
fn bytes_strat() -> BoxedStrategy<Vec<u8>> {
    (0u8..24u8, prop::sample::select(LENS), 0u8..3u8)
        .prop_map(|(tag, len, shape)| make_bytes(tag, len, shape))
        .boxed()
}

fn make_bytes(tag: u8, len: u32, shape: u8) -> Vec<u8> {
    let n = len as usize;
    match shape {
        0 => (0..n).map(|i| mix(i, tag)).collect(),
        1 => vec![0u8; n],
        _ => {
            let mut v = vec![0u8; n];
            for (i, b) in v.iter_mut().enumerate().skip(n / 2) {
                *b = mix(i, tag);
            }
            v
        }
    }
}

/// 随下标不重复的字节。**判据要的是"块与块不同"**：`(i as u8)` 那种写法周期 256，
/// 一个 12 KiB 文件的三块内容完全相同 ⇒ 被 CAS 去重成一块，(b) 的清单路径与 (g) 的
/// 引用数都只在"一块"上过——那是假绿。
fn mix(i: usize, tag: u8) -> u8 {
    let x = (i as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(tag as u64 * 4_0503);
    ((x >> 23) as u8) ^ 0x5a
}

#[derive(Debug, Clone)]
struct Spec {
    comps: Vec<String>,
    bytes: Vec<u8>,
}

fn spec_strat() -> BoxedStrategy<Spec> {
    (
        prop::collection::vec(name_strat(), 1..=4),
        bytes_strat(),
    )
        .prop_map(|(comps, bytes)| Spec { comps, bytes })
        .boxed()
}

#[derive(Debug, Clone)]
enum Mut {
    Del(usize),
    Rewrite(usize, Vec<u8>),
    Rename(usize, Vec<String>),
}

fn mut_strat() -> BoxedStrategy<Mut> {
    prop_oneof![
        (0usize..40).prop_map(Mut::Del),
        (0usize..40, bytes_strat()).prop_map(|(i, b)| Mut::Rewrite(i, b)),
        (0usize..40, prop::collection::vec(name_strat(), 1..=3)).prop_map(|(i, c)| Mut::Rename(i, c)),
    ]
    .boxed()
}

// ── 把一堆 (路径, 字节) 装成一棵不会自相矛盾的树 ─────────────────────────────
//
// 随机路径会撞：`a` 既可能是文件也可能是目录。**不许两个都留**——盘上存不下，
// 而"静默覆盖"会让 expected 表里同一个 blob hash 对两份不同字节，(b) 就退化成测生成器。
// 规矩是先到先得，后来的整条丢掉。

enum Layer {
    File(Vec<u8>),
    Dir(BTreeMap<String, Layer>),
}

fn insert(map: &mut BTreeMap<String, Layer>, comps: &[String], bytes: &[u8]) -> bool {
    let name = &comps[0];
    if comps.len() == 1 {
        return match map.get(name.as_str()) {
            Some(_) => false,
            None => {
                map.insert(name.clone(), Layer::File(bytes.to_vec()));
                true
            }
        };
    }
    if !map.contains_key(name.as_str()) {
        // 中途失败时这里可能留下一个空目录：它不产生任何条目也不落盘，所以不影响判据。
        map.insert(name.clone(), Layer::Dir(BTreeMap::new()));
    }
    match map
        .get_mut(name.as_str())
        .expect("上面刚确认过这一项存在")
    {
        Layer::File(_) => false,
        Layer::Dir(next) => insert(next, &comps[1..], bytes),
    }
}

fn flatten(map: &BTreeMap<String, Layer>, prefix: &str, out: &mut Vec<(String, Vec<u8>)>) {
    for (name, child) in map {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        match child {
            Layer::File(b) => out.push((path, b.clone())),
            Layer::Dir(next) => flatten(next, &path, out),
        }
    }
}

/// 盘上已经被占住的名字 = 所有文件 + 所有文件的每一层祖先目录。
fn occupied(files: &[(String, Vec<u8>)]) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    for (p, _) in files {
        set.insert(p.clone());
        let mut prefix = String::new();
        for comp in p.split('/').take(p.split('/').count() - 1) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(comp);
            set.insert(prefix.clone());
        }
    }
    set
}

/// 改名 / 新增的落点：`cand` 自己、以及它的**任意一层祖先**都不能已被占用（文件或目录都算），
/// 反过来也一样——某个已有文件在 `cand` 下面时，`cand` 必然出现在它的祖先集合里，同一条检查
/// 就挡住了。少了祖先这一半，Windows 上就是"拒绝访问 / 已存在"（2026-10-17 第一版栽过）。
fn free_slot(files: &[(String, Vec<u8>)], cand: &str) -> bool {
    if cand.is_empty() {
        return false;
    }
    let occ = occupied(files);
    let mut prefix = String::new();
    for comp in cand.split('/') {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(comp);
        if occ.contains(&prefix) {
            return false;
        }
    }
    true
}

/// 删掉一个文件之后把**空了的祖先目录**一并收掉：留下的空目录会继续占名字，
/// 而用例的模型里"占住的名字"必须能从 `files` 算出来（见上面 `occupied`）。
fn prune_empty_dirs(proj: &Path, path: &str) {
    let parts: Vec<&str> = path.split('/').collect();
    for cut in (1..parts.len()).rev() {
        let dir = proj.join(parts[..cut].join("/").replace('/', std::path::MAIN_SEPARATOR_STR));
        let Ok(mut entries) = fs::read_dir(&dir) else {
            break;
        };
        if entries.next().is_some() {
            break;
        }
        let _ = fs::remove_dir(&dir);
    }
}

struct Site {
    db: Box<dyn storage_engines::KvEngine>,
    pid: ProjectId,
    objects: PathBuf,
    /// 声明在引擎**之后** ⇒ 析构时先关引擎再删目录（Windows 上反过来会撞"句柄还没放掉"）。
    tmp: tempfile::TempDir,
}

impl Site {
    fn new() -> Site {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = open_store(tmp.path(), EngineKind::LsmTree).expect("open_store");
        Site {
            db,
            pid: keys::project_id("p"),
            objects: objects_dir(tmp.path()),
            tmp,
        }
    }

    fn proj(&self) -> PathBuf {
        self.tmp.path().join("p")
    }

    fn cas(&self) -> Cas {
        Cas::new(&self.objects, BLOCK).expect("CAS")
    }

    fn abs(&self, path: &str) -> PathBuf {
        self.proj().join(path.replace('/', std::path::MAIN_SEPARATOR_STR))
    }

    fn write_one(&self, path: &str, bytes: &[u8]) {
        let abs = self.abs(path);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).expect("建父目录");
        }
        fs::write(&abs, bytes).expect("写文件");
    }

    fn scan(&self) {
        worktree::scan(&*self.db, self.pid, &self.proj(), false, 200).expect("scan");
    }

    fn commit(&self, msg: &str, ts: i64) -> build::Built {
        let store = self.cas();
        build::build(
            &*self.db,
            self.pid,
            &store,
            &self.proj(),
            build::Meta {
                peer: "proptest",
                msg,
                ts,
                // 这批用例会随机删掉一批文件，而 FR-10 的固定下限（20 行）在小站点上很容易被
                // 随机数撞到。熔断本身有定向用例（`mv_rename.rs` 里"删一半必拒"与"整批改名不拒"），
                // 这里放开是为了让 (b)(g) 能跑到随机序列的深处。
                allow_mass_delete: true,
            },
        )
        .unwrap_or_else(|e| {
            let dump = self.debug_dump();
            panic!("commit 失败：{e}
{dump}");
        })
    }

    /// 出错时把"盘上有什么 / 库里 W 有什么 / 本轮 epoch 是多少"一次打全。
    /// 重名条目这类错误只看错误信息是定不了位的——要么盘上真有两个同名条目，
    /// 要么库里有一行不该算活的旧行，这两种的修法完全不同。
    fn debug_dump(&self) -> String {
        use twosync::store::worktree::WtEntry;
        let mut out = String::new();
        out.push_str(&format!("盘上：{}
", self.ls_disk()));
        if let Some(m) = worktree::last_mark(&*self.db, self.pid) {
            out.push_str(&format!("Z.epoch = {} (round {})
", m.epoch, m.round));
        }
        out.push_str("W 行：
");
        for (key, raw) in rows(&*self.db, &keys::project_prefix(keys::TAG_WORKTREE, self.pid)) {
            let path = String::from_utf8_lossy(&key[5..]).to_string();
            let Ok(bytes) = keys::strip(raw.as_deref().unwrap_or_default()) else {
                out.push_str(&format!("  {path} <信封坏>
"));
                continue;
            };
            match postcard::from_bytes::<WtEntry>(bytes) {
                Ok(e) => out.push_str(&format!("  {path} kind={:?} scanned_at={}
", e.kind, e.scanned_at)),
                Err(_) => out.push_str(&format!("  {path} <解不开>
")),
            }
        }
        out
    }

    fn ls_disk(&self) -> String {
        let mut lines = Vec::new();
        fn walk(dir: &Path, prefix: &str, lines: &mut Vec<String>) {
            let Ok(rd) = fs::read_dir(dir) else { return };
            let mut names: Vec<(String, bool)> = Vec::new();
            for e in rd.flatten() {
                let is_dir = e.path().is_dir();
                names.push((e.file_name().to_string_lossy().to_string(), is_dir));
            }
            names.sort();
            for (n, is_dir) in names {
                let p = if prefix.is_empty() { n.clone() } else { format!("{prefix}/{n}") };
                lines.push(format!("{}{}", p, if is_dir { "/" } else { "" }));
                if is_dir {
                    walk(&dir.join(&n), &p, lines);
                }
            }
        }
        walk(&self.proj(), "", &mut lines);
        lines.sort();
        lines.join(" ")
    }

    fn heads(&self) -> Vec<Hash> {
        build::ref_heads(&*self.db, self.pid, MAIN).expect("heads")
    }

    /// 扫 + 提一轮，返回本轮的 `Built`。
    fn round(
        &self,
        files: &[(String, Vec<u8>)],
        expected: &mut BTreeMap<Hash, Vec<u8>>,
        msg: &str,
        ts: i64,
    ) -> build::Built {
        for (path, bytes) in files {
            self.write_one(path, bytes);
            expected.insert(keys::hash_of(bytes), bytes.clone());
        }
        self.scan();
        self.commit(msg, ts)
    }
}

/// `prefix_scan` 的行：value 一律带回 `Option`，**不许在这里滤空**——`k` 索引行的值就是空的，
/// 滤掉会让 (g) 变成"索引一条都没有"的假红。
fn rows(db: &dyn storage_engines::KvEngine, prefix: &[u8]) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
    let tx = db.begin();
    let out = tx
        .prefix_scan(prefix)
        .into_iter()
        .map(|r| (r.key, r.value))
        .collect();
    tx.commit();
    out
}

/// 全部 `B` 清单里被引用的非零块，逐条计数。**这是 (g) 的第三方对照**：它只读清单，
/// 不看 `K` 也不看 `k`，所以"两边一起错"它照样能抓到。
fn refs_from_blobs(db: &dyn storage_engines::KvEngine, pid: ProjectId) -> BTreeMap<Hash, u64> {
    let mut want: BTreeMap<Hash, u64> = BTreeMap::new();
    for (_, raw) in rows(db, &keys::project_prefix(keys::TAG_BLOB, pid)) {
        let Ok(bytes) = keys::strip(raw.as_deref().unwrap_or_default()) else {
            panic!("B 行的信封解不开");
        };
        let Ok(blob) = postcard::from_bytes::<Blob>(bytes) else {
            panic!("B 行解不成 Blob");
        };
        for b in blob.blocks.iter().filter(|b| !b.zero) {
            *want.entry(b.hash).or_insert(0) += 1;
        }
    }
    want
}

fn hash_at(key: &[u8], at: usize, what: &str) -> Hash {
    let Some(tail) = key.get(at..at + 32) else {
        panic!("{what} 的键尾缺字节：{key:?}");
    };
    tail.try_into().expect("32 字节切片必然能成 Hash")
}

/// (g) 的三方核对 + `K.size` 与块文件真实长度。返回 (块数, 引用数之和)。
fn check_refcnt(site: &Site) -> Result<(usize, u64), TestCaseError> {
    let want = refs_from_blobs(&*site.db, site.pid);

    let mut table: BTreeMap<Hash, BlockMeta> = BTreeMap::new();
    for (key, raw) in rows(&*site.db, &keys::project_prefix(keys::TAG_BLOCK, site.pid)) {
        let bytes = keys::strip(raw.as_deref().unwrap_or_default()).expect("K 行的信封");
        let meta: BlockMeta = postcard::from_bytes(bytes).expect("K 行解不成 BlockMeta");
        table.insert(hash_at(&key, 5, "K"), meta);
    }

    // 同一个 hash 在 `k` 里只许有**一个**档位，且必须等于 `K.refcnt`。
    let mut slots: BTreeMap<Hash, Vec<u64>> = BTreeMap::new();
    for (key, _) in rows(&*site.db, &keys::project_prefix(keys::TAG_REFCNT, site.pid)) {
        let refcnt = u64::from_be_bytes(
            key[5..13]
                .try_into()
                .map_err(|_| TestCaseError::Fail("k 键的 refcnt 段不是 8 字节".into()))?,
        );
        slots.entry(hash_at(&key, 13, "k")).or_default().push(refcnt);
    }

    prop_assert_eq!(
        slots.len(),
        table.len(),
        "`k` 索引覆盖的块数 {} 与 `K` 主表行数 {} 不等（漂移）",
        slots.len(),
        table.len()
    );
    for (hash, got) in &slots {
        prop_assert_eq!(
            got.len(),
            1,
            "块 {} 在 `k` 里有 {} 个档位 {:?}：旧档位没删，GC 会在旧档位上看见已经不存在的引用数",
            keys::short(hash),
            got.len(),
            got
        );
        let meta = table
            .get(hash)
            .ok_or_else(|| TestCaseError::Fail(format!("`k` 有 {} 但 `K` 没有", keys::short(hash)).into()))?;
        prop_assert_eq!(
            got[0], meta.refcnt,
            "块 {} 的索引档位 {} 与主表 refcnt {} 不等",
            keys::short(hash),
            got[0],
            meta.refcnt
        );
        prop_assert!(
            meta.refcnt >= 1,
            "块 {} 的 refcnt 是 0，但它还在库里 ⇒ GC 会删掉一个仍被引用的块",
            keys::short(hash)
        );
        // 记账的 size 必须等于 CAS 里那个文件的真实长度：size 写歪，配额与传输都算错。
        let abs = site.objects.join(Cas::path_of(hash));
        let len = fs::metadata(&abs)
            .map_err(|e| TestCaseError::Fail(format!("块文件读不到 {}：{e}", abs.display()).into()))?;
        prop_assert_eq!(
            len.len(),
            meta.size as u64,
            "块 {} 的 K.size 与磁盘实际字节不等",
            keys::short(hash)
        );
    }

    // 与独立重算比：既不许少记（GC 提前删块）也不许多记（永远删不掉）。
    prop_assert_eq!(
        table.len(),
        want.len(),
        "`K` 主表与「全部 B 清单重算出的块集合」的键数不等：{} vs {}",
        table.len(),
        want.len()
    );
    let mut sum = 0u64;
    for (hash, n) in &want {
        let meta = table
            .get(hash)
            .ok_or_else(|| TestCaseError::Fail(format!("清单引用了 {} 但 `K` 里没有这一行", keys::short(hash)).into()))?;
        prop_assert_eq!(
            meta.refcnt, *n,
            "块 {} 的 refcnt {} 与清单重算值 {} 不符（少记 = 提前删块，多记 = 永远删不掉）",
            keys::short(hash),
            meta.refcnt,
            n
        );
        sum += n;
    }
    Ok((table.len(), sum))
}

/// (b)：**每个** commit 的每个 blob 都从 CAS 重建一次并逐字节比对（不变量写的是"任意 commit"，
/// 只看 HEAD 等于没测历史）。返回重建过的文件数。
fn check_rebuild(
    site: &Site,
    store: &Cas,
    expected: &BTreeMap<Hash, Vec<u8>>,
    out: &Path,
) -> Result<usize, TestCaseError> {
    let heads = site.heads();
    prop_assert!(!heads.is_empty(), "提交过就该有 head");
    let nodes = history::walk(&*site.db, site.pid, &heads, 0).expect("walk 历史");
    prop_assert!(!nodes.is_empty(), "head 可达集是空的");

    let mut checked = 0usize;
    for node in &nodes {
        let mut listed: Vec<(String, object::TreeEntry)> = Vec::new();
        history::walk_tree(&*site.db, site.pid, &node.commit.tree, &mut |p, e| {
            listed.push((p.to_string(), e.clone()));
            Ok(())
        })
        .expect("走一棵树");
        for (path, e) in listed {
            if e.kind != Kind::File {
                continue;
            }
            let want = expected.get(&e.hash).ok_or_else(|| {
                TestCaseError::Fail(format!("{path}: 清单里的 blob {} 不是我写过的任何内容", keys::short(&e.hash)).into())
            })?;
            prop_assert_eq!(e.size as usize, want.len(), "{}: tree 里的 size 与原始字节数不等", path);
            let blob = build::read_blob(&*site.db, site.pid, &e.hash).expect("读 B 行");
            prop_assert_eq!(blob.hash, e.hash, "{}: B 清单自己的 hash 与 tree 条目不符", path);
            prop_assert_eq!(blob.size as usize, want.len(), "{}: 清单声明的字节数不符", path);
            let missing = store.missing(&blob);
            prop_assert!(
                missing.is_empty(),
                "{}: 清单引用了 {} 个不在磁盘上的块（不变量 (b) 破）",
                path,
                missing.len()
            );
            let dest = out.join(format!("{checked}.bin"));
            store
                .read_blob(&blob, &dest)
                .map_err(|er| TestCaseError::Fail(format!("{}: 重建失败 {er}", path).into()))?;
            let got = fs::read(&dest)
                .map_err(|er| TestCaseError::Fail(format!("{}: 读回失败 {er}", path).into()))?;
            prop_assert_eq!(
                got.as_slice(),
                want.as_slice(),
                "{}: 从 CAS 重建出来的字节与当初写进去的不一样",
                path
            );
            checked += 1;
        }
    }
    Ok(checked)
}

/// 把 specs 装成一棵不撞车的树（**不落盘**，落盘统一走 `Site::round`）。
fn to_tree(specs: &[Spec]) -> Vec<(String, Vec<u8>)> {
    let mut root: BTreeMap<String, Layer> = BTreeMap::new();
    for s in specs {
        insert(&mut root, &s.comps, &s.bytes);
    }
    let mut files = Vec::new();
    flatten(&root, "", &mut files);
    files
}

/// 把随机 ops 落到盘上。**增量改，不推倒重写**：整树重写等于一次"删掉全部"，
/// 那是 FR-10 熔断的用例要管的事，不是这两条不变量的对象。返回真正生效的次数。
fn apply_ops(
    site: &Site,
    files: &mut Vec<(String, Vec<u8>)>,
    ops: &[Mut],
) -> Result<usize, TestCaseError> {
    let mut applied = 0usize;
    for m in ops {
        if files.is_empty() {
            break;
        }
        match m {
            Mut::Del(i) => {
                let (path, _) = files.remove(i % files.len());
                let abs = site.abs(&path);
                if abs.exists() {
                    fs::remove_file(&abs)
                        .map_err(|e| TestCaseError::Fail(format!("删 {path} 失败 {e}").into()))?;
                    prune_empty_dirs(&site.proj(), &path);
                    applied += 1;
                }
            }
            Mut::Rewrite(i, bytes) => {
                let n = i % files.len();
                files[n].1 = bytes.clone();
                site.write_one(&files[n].0, bytes);
                applied += 1;
            }
            Mut::Rename(i, comps) => {
                let cand = comps.join("/");
                if !free_slot(files, &cand) {
                    continue;
                }
                let n = i % files.len();
                let (old, bytes) = files[n].clone();
                let to = site.abs(&cand);
                if let Some(parent) = to.parent() {
                    fs::create_dir_all(parent).expect("改名要建父目录");
                }
                fs::rename(site.abs(&old), &to)
                    .map_err(|e| TestCaseError::Fail(format!("改名 {old} 失败 {e}").into()))?;
                // 搬空之后的旧目录也要收掉：`occupied` 是从 `files` 算的，留一个空目录在盘上
                // 就是"库里算不出但盘上占着"的名字，下一次改名会撞上去。
                prune_empty_dirs(&site.proj(), &old);
                files[n] = (cand, bytes);
                applied += 1;
            }
        }
    }
    Ok(applied)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 96,
        failure_persistence: ::std::option::Option::None,
        ..ProptestConfig::default()
    })]

    /// (b) 任意 commit 逐 blob 可重建 + 最新树必须正好等于盘上。
    #[test]
    fn every_commit_rebuilds_byte_for_byte(
        specs in prop::collection::vec(spec_strat(), 3..14),
        extra in prop::collection::vec(spec_strat(), 1..8),
        ops in prop::collection::vec(mut_strat(), 1..8)
    ) {
        let site = Site::new();
        fs::create_dir_all(site.proj()).expect("项目目录");
        let store = site.cas();
        let out = tempfile::tempdir().expect("重建输出目录");
        let mut expected = BTreeMap::new();

        let mut files = to_tree(&specs);
        prop_assume!(!files.is_empty(), "随机路径全撞车，这一轮没有对象");
        let first = site.round(&files, &mut expected, "第一轮", 1);
        prop_assert!(first.commit.is_some(), "首轮有 {} 个文件却没产生 commit", files.len());

        // 第二轮该不该有提交，判据不是"我对盘动过手"，而是**树变了没有**：
        // 把一个空文件重写成同样的空内容，盘上写入是真的、历史却没错（96 轮第一次跑就
        // 抓到我把这条当成"必须提交"，那是断言错，不是产品错）。
        let before: BTreeMap<String, Hash> = files
            .iter()
            .map(|(p, b)| (p.clone(), keys::hash_of(b)))
            .collect();
        let applied = apply_ops(&site, &mut files, &ops)?;
        for s in &extra {
            let cand = s.comps.join("/");
            if free_slot(&files, &cand) {
                files.push((cand, s.bytes.clone()));
            }
        }
        let second = site.round(&files, &mut expected, "第二轮", 2);
        let after: BTreeMap<String, Hash> = files
            .iter()
            .map(|(p, b)| (p.clone(), keys::hash_of(b)))
            .collect();
        if before == after {
            prop_assert!(
                second.commit.is_none(),
                "树一字没变（盘上动了 {} 处，但内容/路径都一样）却产生了提交：{:?}",
                applied,
                second.changes
            );
        } else {
            prop_assert!(
                second.commit.is_some(),
                "树变了却没产生提交。之前 {} 项、之后 {} 项，差异在第一处：{:?}",
                before.len(),
                after.len(),
                before
                    .iter()
                    .zip(after.iter())
                    .find(|(a, b)| a != b)
                    .map(|(a, b)| (a.0.clone(), b.0.clone()))
            );
        }

        let checked = check_rebuild(&site, &store, &expected, out.path())?;
        prop_assert!(checked >= files.len(), "重建只走了 {} 个文件，盘上却有 {}", checked, files.len());

        // 另一半判据：内容取回来了，但**树里多一条或少一条同样是撒谎**。
        let heads = site.heads();
        let nodes = history::walk(&*site.db, site.pid, &heads, 0).expect("walk 历史");
        let mut listed: Vec<(String, object::TreeEntry)> = Vec::new();
        history::walk_tree(&*site.db, site.pid, &nodes[0].commit.tree, &mut |p, e| {
            listed.push((p.to_string(), e.clone()));
            Ok(())
        })
        .expect("走最新树");
        let got: BTreeMap<String, Hash> = listed
            .iter()
            .filter(|(_, e)| e.kind == Kind::File)
            .map(|(p, e)| (p.clone(), e.hash))
            .collect();
        let want: BTreeMap<String, Hash> = files
            .iter()
            .map(|(p, b)| (p.clone(), keys::hash_of(b)))
            .collect();
        prop_assert_eq!(got, want, "最新树与盘上清单不等");
    }

    /// (g) `K` / `k` / 全部 `B` 清单重算，三方在两轮融资后仍然一致。
    #[test]
    fn refcnt_table_and_index_never_drift(
        specs in prop::collection::vec(spec_strat(), 3..14),
        ops in prop::collection::vec(mut_strat(), 1..8)
    ) {
        let site = Site::new();
        fs::create_dir_all(site.proj()).expect("项目目录");

        let mut expected = BTreeMap::new();
        let mut files = to_tree(&specs);
        prop_assume!(!files.is_empty(), "随机路径全撞车，这一轮没有对象");
        site.round(&files, &mut expected, "第一轮", 1);
        let (blocks, refs) = check_refcnt(&site)?;
        // 空文件的清单里一个块都没有（"整份语料都是空文件"是合法输入，proptest 真的生成了它），
        // 所以**不能**先断"有文件就该有块"——那是拿我的假设当判据。只断"引用和与块数同向"。
        if blocks > 0 {
            prop_assert!(
                refs >= blocks as u64,
                "引用之和 {} 小于块数 {} ⇒ 有块 refcnt 为 0",
                refs,
                blocks
            );
        } else {
            prop_assert_eq!(refs, 0, "一块都没有 ⇒ 引用和也该是 0");
        }

        apply_ops(&site, &mut files, &ops)?;
        site.round(&files, &mut expected, "第二轮", 2);
        // 第二轮之后再核一遍：**共享块**只能 +1 一次，全零块一根都不许记。
        let (blocks2, refs2) = check_refcnt(&site)?;
        prop_assert!(
            blocks2 >= blocks,
            "块数从 {} 掉到 {}：老 blob 的行被人删了",
            blocks,
            blocks2
        );
        prop_assert!(
            refs2 >= refs,
            "引用之和从 {} 掉到 {}：改一轮盘反而少记了引用",
            refs,
            refs2
        );
    }
}

/// 尺子也要被量一次：如果 `check_refcnt` 本身是空的，上面那条 proptest 会永远绿。
/// 这里先立一个"两个文件共享第一个块"的最小库（块 5 / 引用 6），再手工把某个块的 `k` 行
/// 挪到 refcnt=99 的档位上（**主表不动**），断言核对方必须报错。
#[test]
fn refcnt_checker_actually_catches_drift() {
    let site = Site::new();
    fs::create_dir_all(site.proj()).unwrap();
    let mut expected = BTreeMap::new();

    let a = make_bytes(7, 12288, 0);
    let mut b = a[..BLOCK as usize].to_vec();
    b.extend_from_slice(&make_bytes(9, 8192, 0));
    assert_eq!(a.len(), b.len(), "两个文件同长，只有第一个块相同");

    site.round(
        &[("a.bin".to_string(), a.clone()), ("b.bin".to_string(), b.clone())],
        &mut expected,
        "第一轮",
        1,
    );
    let (blocks, refs) = check_refcnt(&site).expect("干净的库必须过");
    assert_eq!(blocks, 5, "各 3 块、共享第一块 ⇒ 5 个块");
    assert_eq!(refs, 6, "两个 blob 各声明 3 块 ⇒ 引用和 6（其中共享那块 refcnt=2）");

    // 人为制造一次漂移：把一条索引行改到 99 档位上，主表不动。
    let (key, _) = rows(
        &*site.db,
        &keys::project_prefix(keys::TAG_REFCNT, site.pid),
    )
    .into_iter()
    .next()
    .expect("有 k 索引行");
    let hash = hash_at(&key, 13, "k");
    let tx = site.db.begin();
    tx.delete(&key);
    tx.set(&keys::refcnt_key(site.pid, 99, &hash), Vec::new());
    tx.commit();

    let err = check_refcnt(&site).expect_err("改了索引必须被抓住");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("档位") || msg.contains("refcnt"),
        "报错信息没用上：{msg}"
    );
}

/// (b) 的同类自检：把一个块文件截短一字节，重建**必须**报错，不许静默给出错字节。
/// （`missing()` 只查存在性，所以它必须先过——这正是"块在但坏了"这一类的检测路径。）
#[test]
fn rebuild_checker_actually_catches_a_damaged_block() {
    let site = Site::new();
    fs::create_dir_all(site.proj()).unwrap();
    let mut expected = BTreeMap::new();
    let bytes = make_bytes(3, 12288, 0);
    site.round(&[("only.bin".to_string(), bytes.clone())], &mut expected, "第一轮", 1);

    let store = site.cas();
    let out = tempfile::tempdir().unwrap();
    assert!(
        check_rebuild(&site, &store, &expected, out.path()).is_ok(),
        "没损坏时该过"
    );

    let blocks: Vec<Hash> = build::read_blob(&*site.db, site.pid, &keys::hash_of(&bytes))
        .expect("B 行")
        .blocks
        .iter()
        .map(|x| x.hash)
        .collect();
    let victim = site.objects.join(Cas::path_of(&blocks[0]));
    let data = fs::read(&victim).unwrap();
    fs::write(&victim, &data[..data.len() - 1]).unwrap();

    let err = check_rebuild(&site, &store, &expected, out.path()).expect_err("块被截短必须报错");
    assert!(
        format!("{err:?}").contains("重建失败"),
        "报错内容不对：{err:?}"
    );
}
