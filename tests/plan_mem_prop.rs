//! §10.1 **(f1)**：twosync **自己这一层**的峰值堆与文件总数无关。
//!
//! # 为什么这条不能"直接测"
//!
//! `allocstats` 量的是**整个进程**的堆，而引擎每次读调用都要折一遍全库（§9.6.1、补记九/十）——
//! 折叠本身就住在堆上。文件一多，库就大，峰值就涨；直接断言"峰值与文件数无关"会把引擎那笔账
//! 算到我们头上（而这正是 2026-10-17 我犯的错：拿两个一起变的量讲了个因果故事）。
//!
//! 所以这里做**只动一维**的对照：两个站点，一个 150 个文件、一个 1 500 个文件，提完交之后
//! **用 `X` 前缀的填充行把库内字节宽度钉成同一个数**。宽度相同 ⇒ 每次折叠的瞬度量相同
//! ⇒ 两次测量之差只剩"建树那一层"。
//!
//! # 对照是否成立由用例自己断言
//!
//! 两侧库宽差超过 1% 就直接报"对照没做成"。**这条断言比结论重要**：库宽一旦不齐，
//! 下面那条比值判据验的就不是 (f1)。
//!
//! # 一条如实的边界
//!
//! 这个增量里**也包含引擎在读路径上顺手做的事**（memtable 切换、compaction 触发），所以
//! (f1) 在这里是一个**上界**判据，不是"纯我们那层"的分离测量；真超了要先复测确认是谁。
//! 分离测量得等有界读出口（§14 Q10）。
//!
//! 跑法：`cargo test --test plan_mem_prop -- --test-threads=1`（两个用例都要量堆，
//! 并行会互相抬峰值——锁已经加了，`--test-threads=1` 只是双保险）。

use std::fs;

use storage_engines::{EngineKind, KvEngine};
use twosync::allocstats;
use twosync::store::build;
use twosync::store::cas::Cas;
use twosync::store::keys;
use twosync::store::worktree;
use twosync::utils::{objects_dir, open_store};

/// **本文件所有用例都要先进这把锁**：`CURRENT`/`PEAK` 是进程级原子数，同一个测试二进制里
/// 并行的用例会互相把峰值抬上去（解释全文见 `engine_smoke.rs` 的开头）。
static ALLOC_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    ALLOC_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// 填充行用的标签：§9.2 的十一类里没有 `X`，它不属于我们，只是用来撑库宽。
const PAD_TAG: u8 = b'X';
const PAD_BODY: usize = 200;
/// 判据本身（§10.1(f)、NFR-1）。
const BUDGET: usize = 8 << 20;
/// "平"的容差：库宽钉齐之后，10 倍文件数不许让峰值动超过这么多。
const FLAT: usize = 1 << 20;

struct Site {
    db: Box<dyn KvEngine>,
    pid: u32,
    tmp: tempfile::TempDir,
}

impl Site {
    fn new() -> Site {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = open_store(tmp.path(), EngineKind::LsmTree).expect("open_store");
        Site {
            db,
            pid: keys::project_id("p"),
            tmp,
        }
    }

    fn proj(&self) -> std::path::PathBuf {
        self.tmp.path().join("p")
    }

    fn scan(&self) {
        worktree::scan(&*self.db, self.pid, &self.proj(), false, 500).expect("scan");
    }

    fn commit(&self) {
        let store = Cas::new(&objects_dir(self.tmp.path()), 4 << 20).expect("CAS");
        build::build(
            &*self.db,
            self.pid,
            &store,
            &self.proj(),
            build::Meta {
                peer: "mem",
                msg: "库宽基线",
                ts: 1,
                allow_mass_delete: false,
            },
        )
        .expect("commit");
    }

    /// 库内**全部存活键 + 值**的字节数 = 折叠一次要装多少东西。
    fn live_bytes(&self) -> usize {
        let tx = self.db.begin();
        let total = tx
            .scan(None, None)
            .into_iter()
            .map(|r| r.key.len() + r.value.unwrap_or_default().len())
            .sum();
        tx.commit();
        total
    }

    /// 把库宽补到 `target`。**不用 `key_count` 取序号**：那个数要是少报了，就会反复重写同一批键、
    /// 宽度永远不涨 ⇒ 死循环。这里用自己递增的序号，外加一条"必须真的在涨"的守卫。
    ///
    /// **一批写多少要按余量算**：固定 500 行 = 一次 106 KB（每行 13 B 键 + 200 B 值），
    /// 最后那一批会直接冲过 `target` 几万个字节，两个站点的"钉齐"就钉不住了
    /// （2026-10-17 第一版就是这样，对照断言立刻红给我看：176 164 vs 137 396）。
    fn pad_to(&self, target: usize) {
        let mut seq = 0usize;
        let mut last = self.live_bytes();
        while last < target {
            // 一行占的字节：键 = 标签 1 + pid 4 + 序号 8，值 = `PAD_BODY`。
            let per = 13 + PAD_BODY;
            let want = (target - last).div_ceil(per).clamp(1, 2000);
            let tx = self.db.begin();
            for _ in 0..want {
                let mut key = vec![PAD_TAG];
                key.extend_from_slice(&self.pid.to_be_bytes());
                key.extend_from_slice(&(seq as u64).to_be_bytes());
                seq += 1;
                if !tx.set(&key, vec![0xA5u8; PAD_BODY]) {
                    panic!("填充行写入被拒（写冲突）——§9.6.4 那条 bug 在这里就是真 bug");
                }
            }
            tx.commit();
            let now = self.live_bytes();
            assert!(now > last, "填充停了：写了 {seq} 行而库宽还是 {now}");
            last = now;
            assert!(seq < 200_000, "填充超过 {seq} 行还没到 {target}，库宽涨得太慢");
        }
    }
}

/// 造 `n` 个文件：每 10 个一层目录，每个 1 KiB 且内容互不相同（不去重 ⇒ 块数与文件数同步长）。
fn seed_shallow(site: &Site, n: usize) {
    let proj = site.proj();
    for i in 0..n {
        let dir = proj.join(format!("d{:04}", i / 10));
        fs::create_dir_all(&dir).expect("建目录");
        fs::write(dir.join(format!("f{:02}.bin", i % 10)), tag_bytes(i, 1024)).expect("写文件");
    }
}

/// 由序号唯一决定的一段字节（两个站点用同样的函数 ⇒ 内容形状可比）。
fn tag_bytes(i: usize, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    for (j, b) in v.iter_mut().enumerate() {
        *b = (i * 31 + j) as u8 ^ 0x5a;
    }
    v
}

/// 提交完之后的一侧站点，连同它的自然库宽。
fn committed(n: usize) -> (Site, usize) {
    let site = Site::new();
    seed_shallow(&site, n);
    site.scan();
    site.commit();
    let width = site.live_bytes();
    (site, width)
}

/// `plan` 这一趟的峰值增量。
fn plan_peak(site: &Site) -> usize {
    let wm = allocstats::watermark();
    build::plan(&*site.db, site.pid).expect("plan");
    wm.peak_growth()
}

/// 把两侧库宽钉到同一个数，返回钉完之后的实际宽度（差超过 1% 直接红）。
fn pin_width(a: &Site, b: &Site) -> (usize, usize) {
    let target = a.live_bytes().max(b.live_bytes()) + 64 * 1024;
    a.pad_to(target);
    b.pad_to(target);
    let (wa, wb) = (a.live_bytes(), b.live_bytes());
    let wider = wa.max(wb);
    assert!(
        wa.abs_diff(wb) * 100 <= wider,
        "对照没做成：库宽 {wa} vs {wb}，差 {} 字节超过 1% ⇒ 下面比的就不是 (f1)。\
         先查 `pad_to` 的粒度（填充行 {PAD_BODY} B）",
        wa.abs_diff(wb)
    );
    (wa, wb)
}

#[test]
fn plan_peak_heap_is_flat_once_the_db_width_is_pinned() {
    let _g = serial();

    let (small, s_raw) = committed(150);
    let (big, b_raw) = committed(1500);
    let (w_small, w_big) = pin_width(&small, &big);

    let p_small = plan_peak(&small);
    let p_big = plan_peak(&big);

    assert!(
        p_small < BUDGET && p_big < BUDGET,
        "§10.1(f) 的 8 MiB：150 文件时 {p_small}，1500 文件时 {p_big}"
    );
    assert!(
        p_big.abs_diff(p_small) <= FLAT,
        "库宽已钉成同一个数（{w_small} ≈ {w_big}，自然宽度是 {s_raw} / {b_raw}），\
         10 倍文件数却让峰值动了 {} ⇒ 有一层随文件数长，不是 (f1) 说的常数层",
        p_big.abs_diff(p_small)
    );

    // 顺带钉住"条目全在账上"：1 500 文件 + 150 目录，一层都没撞上限。
    let built = build::plan(&*big.db, big.pid).expect("plan 1500");
    assert_eq!(built.entries, 1650, "1 500 文件 + 150 个目录都该在账上");
}

/// 同样 100 个文件，**深树**（每个文件埋到 25 层）与**宽树**（一层 100 个）：
/// 条目总数一样，但栈的形状完全不同（一个吃递归深度、一个吃单层向量）。
/// 如果 (f1) 只在"浅而窄"成立，这一条就会红。
#[test]
fn deep_and_wide_trees_cost_the_same() {
    let _g = serial();

    let deep = Site::new();
    for i in 0..100 {
        // 前 5 层共享，后面 20 层每个文件自己的 ⇒ 路径深 25 层。
        let mut rel = String::new();
        for d in 0..5 {
            rel.push_str(&format!("s{d}/"));
        }
        for d in 0..20 {
            rel.push_str(&format!("t{}{}/", i % 7, d));
        }
        rel.push_str(&format!("f{i}.bin"));
        let abs = deep.proj().join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        fs::create_dir_all(abs.parent().expect("有父目录")).expect("深层目录");
        fs::write(&abs, tag_bytes(i, 64)).expect("写文件");
    }
    deep.scan();
    deep.commit();

    let wide = Site::new();
    fs::create_dir_all(wide.proj()).expect("宽目录");
    for i in 0..100 {
        fs::write(wide.proj().join(format!("f{i:03}.bin")), tag_bytes(i, 64)).expect("写文件");
    }
    wide.scan();
    wide.commit();

    let (wd, ww) = pin_width(&deep, &wide);
    let (pd, pw) = (plan_peak(&deep), plan_peak(&wide));
    assert!(
        pd.abs_diff(pw) <= FLAT,
        "同样 100 个文件，深树峰值 {pd} 与宽树 {pw} 差超过 1 MiB（库宽已钉齐：{wd} ≈ {ww}）\
         ⇒ 栈的形状随规模变，(f1) 只对一种树成立"
    );
    assert!(pd < BUDGET && pw < BUDGET, "深 {pd} / 宽 {pw}，超 8 MiB");
}
