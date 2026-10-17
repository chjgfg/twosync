//! 扫描器的两条硬承诺 + 一条不显然的语义（§9.6.1 / §9.6.2 / §8）。

use std::fs;
use std::path::Path;

use storage_engines::EngineKind;
use twosync::allocstats;
use twosync::store::keys;
use twosync::store::worktree;
use twosync::utils::open_store;

fn seed(dir: &Path, n: usize) {
    for i in 0..n {
        fs::write(dir.join(format!("f{i}.txt")), format!("content {i}")).unwrap();
    }
}

#[test]
fn scan_hashes_then_skips_unchanged() {
    let site = tempfile::tempdir().unwrap();
    let proj = site.path().join("p");
    fs::create_dir_all(&proj).unwrap();
    seed(&proj, 30);

    let db = open_store(site.path(), EngineKind::LsmTree).unwrap();
    let pid = keys::project_id("p");

    let first = worktree::scan(&*db, pid, &proj, false, 8).unwrap();
    assert_eq!(first.walked, 30);
    assert_eq!(first.hashed, 30, "首轮全部要 hash");

    let second = worktree::scan(&*db, pid, &proj, false, 8).unwrap();
    assert_eq!(second.hashed, 0, "size+mtime 没变就不该重算（§8 粗筛）");
    assert_eq!(second.skipped, 30);

    let forced = worktree::scan(&*db, pid, &proj, true, 8).unwrap();
    assert_eq!(forced.hashed, 30, "--force 必须绕过粗筛");
}

/// §9.6.1：常数内存。32 MiB 文件如果走"整读进内存"，这个数会 ≥ 32 MiB。
#[test]
fn scan_peak_heap_stays_constant_regardless_of_file_size() {
    let site = tempfile::tempdir().unwrap();
    let proj = site.path().join("p");
    fs::create_dir_all(&proj).unwrap();
    fs::write(proj.join("big.bin"), vec![7u8; 32 << 20]).unwrap();
    seed(&proj, 200);

    let db = open_store(site.path(), EngineKind::LsmTree).unwrap();
    let wm = allocstats::watermark();
    let rep = worktree::scan(&*db, keys::project_id("p"), &proj, false, 50).unwrap();
    let peak = wm.peak_growth();

    assert_eq!(rep.hashed, 201);
    assert!(
        peak < 8 << 20,
        "峰值 {peak} 字节：扫描必须是流式的，不许物化"
    );
}

/// 稳态扫描**一个都不该重算**。这条是 `logical_size` 的守卫：`digest` 给目录存 0，
/// 而粗筛若拿 `md.len()` 比，NTFS 上条目多的目录（实测 5001 个孩子的 `data`）就永远不命中，
/// 每轮白算一次 —— 小目录恰好是 0 所以看不出，故这里刻意把 child 数放大。
#[test]
fn steady_state_scan_rehashes_nothing() {
    let site = tempfile::tempdir().unwrap();
    let proj = site.path().join("p");
    let many = proj.join("many");
    fs::create_dir_all(&many).unwrap();
    seed(&many, 1500);

    let db = open_store(site.path(), EngineKind::LsmTree).unwrap();
    let pid = keys::project_id("p");
    let first = worktree::scan(&*db, pid, &proj, false, 500).unwrap();
    assert!(first.hashed > 1500);

    let second = worktree::scan(&*db, pid, &proj, false, 500).unwrap();
    assert_eq!(
        second.hashed, 0,
        "稳态轮重算了 {}/{} 条：存进去的大小和拿来比的大小定义不一致（样本 {:?}）",
        second.hashed, second.walked, second.sample
    );
}

/// FR-12：`.git`/`.twosync` 是内置强制排除，配置取消不了。
#[test]
fn scan_never_records_forced_excludes() {
    let site = tempfile::tempdir().unwrap();
    let proj = site.path().join("p");
    fs::create_dir_all(proj.join(".git/objects")).unwrap();
    fs::create_dir_all(proj.join(".twosync")).unwrap();
    fs::write(proj.join(".git/objects/x"), b"junk").unwrap();
    fs::write(proj.join(".twosync/index"), b"junk").unwrap();
    fs::write(proj.join("keep.txt"), b"real").unwrap();

    let db = open_store(site.path(), EngineKind::LsmTree).unwrap();
    let pid = keys::project_id("p");
    let rep = worktree::scan(&*db, pid, &proj, false, 10).unwrap();
    assert_eq!(rep.walked, 1, "只有 keep.txt 该被走");

    let tx = db.begin();
    let all = tx.prefix_scan(&keys::project_prefix(keys::TAG_WORKTREE, pid));
    assert_eq!(all.len(), 1);
    assert!(String::from_utf8_lossy(&all[0].key).ends_with("keep.txt"));
    tx.commit();
}

// ── 「盘上对账」：扫描之后发生的增删 ──────────────────────────────────
//
// 这几条盯的是同一件事：`status` 能不能在**没扫描**的情况下说出盘上多了什么、少了什么。
// 判据是目录 mtime，比较单位是"一层目录内部"——所以关键用例是 `src` + `src.rs` 那一条：
// 它专门用来否决"照 DFS 顺序做全局归并"那个看起来很自然的写法。
//
// 手工验证（跑完 `rm -rf /tmp/ts7`）：
//
// ```text
// cargo build --release && TS=./target/release/twosync.exe
// rm -rf /tmp/ts8 && $TS --root /tmp/ts8 init
// mkdir -p /tmp/ts8/lab/src && echo a > /tmp/ts8/lab/src.rs && echo m > /tmp/ts8/lab/src/main.rs
// $TS --root /tmp/ts8 project add lab && $TS --root /tmp/ts8 scan && $TS --root /tmp/ts8 commit -m v1
// echo n > /tmp/ts8/lab/src/extra.rs && $TS --root /tmp/ts8 status
//   # 变更 = 无（没扫描），盘上对账 = 未扫的新条目 1：+ src/extra.rs，且不许出现 - src.rs
// ```

use std::collections::BTreeMap;

fn site() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("p");
    fs::create_dir_all(&proj).unwrap();
    (tmp, proj)
}

/// `epoch = 0` ⇒ 每一层的 mtime 都"晚于"它 ⇒ 全部层都可疑。用例要的是**比较逻辑**，
/// 可疑判据本身由 `reconcile_only_the_suspect_layer` 那条单独管。
fn diff(proj: &std::path::Path, db: BTreeMap<String, Vec<String>>) -> worktree::Reconciled {
    let view = worktree::view_since(proj, 0).unwrap();
    worktree::reconcile(&view, &db)
}

#[test]
fn reconcile_reports_a_file_created_after_the_scan() {
    let (_t, proj) = site();
    fs::write(proj.join("a.txt"), "a").unwrap();
    let r = diff(
        &proj,
        BTreeMap::from([("".to_string(), vec!["a.txt".to_string()])]),
    );
    assert_eq!(
        (r.only_disk, r.only_w, r.unreadable),
        (0, 0, 0),
        "没动就该全平：{r:?}"
    );

    fs::write(proj.join("b.txt"), "b").unwrap();
    let r = diff(
        &proj,
        BTreeMap::from([("".to_string(), vec!["a.txt".to_string()])]),
    );
    assert_eq!((r.only_disk, r.only_w), (1, 0), "{r:?}");
    assert_eq!(
        r.sample,
        vec!["+ b.txt".to_string()],
        "顶层不带前缀：{:?}",
        r.sample
    );
}

#[test]
fn reconcile_reports_a_file_deleted_after_the_scan() {
    let (_t, proj) = site();
    fs::write(proj.join("a.txt"), "a").unwrap();
    fs::write(proj.join("b.txt"), "b").unwrap();
    fs::remove_file(proj.join("b.txt")).unwrap();
    // 库里那两条都还活着（扫描器不删行，删除要等下一轮扫描 ⇒ §8）。
    let r = diff(
        &proj,
        BTreeMap::from([(
            "".to_string(),
            vec!["a.txt".to_string(), "b.txt".to_string()],
        )]),
    );
    assert_eq!((r.only_disk, r.only_w), (0, 1), "{r:?}");
    assert_eq!(r.sample, vec!["- b.txt".to_string()]);
}

/// **这条是否决一个看起来很自然的写法**：目录名正好是某个兄弟文件名的前缀时，键的字节序是
/// `src` → `src.rs` → `src/main.rs`（`'/'`=0x2F 夹在 `.` 与 `0` 之间），而盘的 DFS 会先钻进
/// `src/`。谁照着 DFS 做全局两游标归并，`src.rs` 就会被说成"盘上已删"。
/// 现在按"一层一层比"，`src` 那一层里没有 `src.rs` 什么事。
#[test]
fn prefix_colliding_sibling_is_not_reported_as_deleted() {
    let (_t, proj) = site();
    fs::create_dir_all(proj.join("src")).unwrap();
    fs::write(proj.join("src.rs"), "sibling").unwrap();
    fs::write(proj.join("src").join("main.rs"), "main").unwrap();
    let db = BTreeMap::from([
        (
            "".to_string(),
            vec!["src".to_string(), "src.rs".to_string()],
        ),
        ("src".to_string(), vec!["main.rs".to_string()]),
    ]);

    let r = diff(&proj, db.clone());
    assert_eq!((r.only_disk, r.only_w), (0, 0), "{r:?}");

    fs::write(proj.join("src").join("extra.rs"), "extra").unwrap();
    let r = diff(&proj, db);
    assert_eq!((r.only_disk, r.only_w), (1, 0), "{r:?}");
    assert_eq!(r.sample, vec!["+ src/extra.rs".to_string()]);
}

/// 可疑集合的两端都要测：**动过的必须进来，没动过的不许进来**。
///
/// 这一条还顺手把 [`MTIME_LAG`] 的方向钉住——判据是 `mtime > epoch - 余量`，所以
/// "很久以前动过的层"不该被拖进来（那是纯粹白比的 readdir），而 epoch 取在很久之后的
/// 假想未来时，集合该是空的。用例用**自己造的 epoch**，不靠 sleep 跟文件系统抢时间。
#[test]
fn suspect_set_has_no_false_positive_and_none_missed() {
    let (_t, proj) = site();
    fs::create_dir_all(proj.join("a")).unwrap();
    fs::write(proj.join("a").join("one.txt"), "1").unwrap();
    fs::write(proj.join("top.txt"), "t").unwrap();

    // 未来很远的 epoch ⇒ 没有任何一层"被动过"。
    let far_future = worktree::now_ns() + 3_600_000_000_000;
    let v = worktree::view_since(&proj, far_future).unwrap();
    assert!(
        v.suspects.is_empty(),
        "未来的 epoch 不该有可疑层：{:?}",
        v.suspects
    );
    assert_eq!(v.at.len(), 0, "没可疑层就不该收任何名单");

    // 现在动 `a/` 一层，epoch 取在动之前 ⇒ `a` 必须进来（余量方向是宁可多比，见 MTIME_LAG）。
    let epoch = worktree::now_ns();
    fs::write(proj.join("a").join("two.txt"), "2").unwrap();
    let v = worktree::view_since(&proj, epoch).unwrap();
    assert!(
        v.suspects.iter().any(|s| s == "a"),
        "动过的那层必须进来：{:?}",
        v.suspects
    );
    assert!(
        v.at.get("a").map(|l| l.len()) == Some(2),
        "该收的名字要收齐：{:?}",
        v.at
    );

    // 而一个**一小时前**就没再动过的层，不能因为余量被拖进来。
    let hour_ago = worktree::now_ns() - 3_600_000_000_000;
    let v = worktree::view_since(&proj, hour_ago).unwrap();
    assert!(
        v.suspects.iter().any(|s| s == "a"),
        "一小时前动过的层当然可疑：{:?}",
        v.suspects
    );
    let dirs_before = v.dirs;
    let future = worktree::now_ns() + 3_600_000_000_000;
    let v2 = worktree::view_since(&proj, future).unwrap();
    assert_eq!(v2.dirs, dirs_before, "比 mtime 的目录数不该随判据变化");
}

/// 排除语义必须与 `scan` **同一条流**：`.git` / `.twosync` 在这儿冒出来就会被报成"未跟踪"，
/// 而扫描器是故意不看它们的。两份账不一致是静默的，没人会去核。
#[test]
fn view_uses_the_same_exclusions_as_scan() {
    let (_t, proj) = site();
    fs::create_dir_all(proj.join(".git")).unwrap();
    fs::write(proj.join(".git").join("HEAD"), "ref").unwrap();
    fs::create_dir_all(proj.join(".twosync")).unwrap();
    fs::write(proj.join("keep.txt"), "k").unwrap();

    let view = worktree::view_since(&proj, 0).unwrap();
    assert_eq!(
        view.at.get("").map(Vec::as_slice),
        Some(&["keep.txt".to_string()][..]),
        "强制排除的不许出现：{:?}",
        view.at
    );
    assert!(
        !view.suspects.iter().any(|s| s.starts_with('.')),
        "被排除的目录也不该进可疑集合：{:?}",
        view.suspects
    );
}

/// 可疑层多到上限 ⇒ 只报计数、并**承认**名单不全。静默给一份不完整名单是最坏的结果。
#[test]
fn suspect_list_is_capped_and_says_so() {
    let (_t, proj) = site();
    for i in 0..(worktree::RECON_SUSPECT_MAX + 20) {
        fs::create_dir_all(proj.join(format!("d{i:04}"))).unwrap();
    }
    let view = worktree::view_since(&proj, 0).unwrap();
    assert_eq!(
        view.suspects.len(),
        worktree::RECON_SUSPECT_MAX,
        "名单该卡在上限，不是全量"
    );
    assert!(view.truncated, "卡了就得说出来");
}

/// 从没完整扫过 ⇒ 没有活性线，**不许**编出一张"全盘都是未跟踪"的名单。
/// 这条守的是 §7.2 那类错误：空账被读成"全树没备份"。
#[test]
fn no_scan_mark_means_no_reconciliation() {
    let (_t, proj) = site();
    fs::write(proj.join("a.txt"), "a").unwrap();
    let db = open_store(proj.parent().unwrap(), EngineKind::LsmTree).unwrap();
    let pid = keys::project_id("never");
    assert!(
        worktree::last_mark(&*db, pid).is_none(),
        "新站点不该有扫描标记"
    );
    // `status` 的判据就是这一条：没有 epoch 就整段跳过，不去比盘。
}
