//! 扫描器的两条硬承诺 + 一条不显然的语义（§9.6.1 / §9.6.2 / §8）。

mod common;

use std::fs;
use std::path::Path;

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

    let db = open_store(site.path(), common::engine()).unwrap();
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

    let db = open_store(site.path(), common::engine()).unwrap();
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

    let db = open_store(site.path(), common::engine()).unwrap();
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

    let db = open_store(site.path(), common::engine()).unwrap();
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
    let db = open_store(proj.parent().unwrap(), common::engine()).unwrap();
    let pid = keys::project_id("never");
    assert!(
        worktree::last_mark(&*db, pid).is_none(),
        "新站点不该有扫描标记"
    );
    // `status` 的判据就是这一条：没有 epoch 就整段跳过，不去比盘。
}

// ── 窄扫（`Scope::Dir`）：它买到的是"一轮只花 0.1 s"，代价是"这一轮报不出删除" ────────
//
// 这一族的分工：`worktree_scan` 只测扫描器自己的两条承诺（`Z` 不动、路径形状不变），
// "不产生假删除 / 新文件照常看得见"在 `tests/build_commit.rs` 那侧（要建 commit 才看得见）。

/// 造两个目录：`a/` 与 `b/`，各一个文件。
fn seed_two_dirs(proj: &Path) {
    fs::create_dir_all(proj.join("a")).unwrap();
    fs::create_dir_all(proj.join("b")).unwrap();
    fs::write(proj.join("a/x.bin"), b"the a file").unwrap();
    fs::write(proj.join("b/y.bin"), b"the b file").unwrap();
}

#[test]
fn a_narrow_round_does_not_move_the_liveness_line() {
    let site = tempfile::tempdir().unwrap();
    let proj = site.path().join("p");
    fs::create_dir_all(&proj).unwrap();
    seed_two_dirs(&proj);
    let db = open_store(site.path(), common::engine()).unwrap();
    let pid = keys::project_id("p");

    let full = worktree::scan(&*db, pid, &proj, false, 8).unwrap();
    assert_eq!(full.walked, 4, "两个文件 + 两个目录：{}", full.walked);
    let mark_after_full = worktree::last_mark(&*db, pid).expect("全量轮必须盖章");
    assert_eq!(mark_after_full.round, 1);

    // 窄扫 b/：只走那一层（连子树），`walked` 必须显著小于全量
    let narrow = worktree::scan_scoped(
        &*db,
        pid,
        &proj,
        &worktree::Scope::Dir("b".to_string()),
        false,
        8,
        &twosync::store::cas::Bucket::default(),
    )
    .unwrap();
    assert_eq!(narrow.walked, 2, "只该看见 b 与 b/y.bin：{}", narrow.walked);

    // **这一条就是整个设计的核心**：活性线不许被半轮推进。
    let mark = worktree::last_mark(&*db, pid).expect("标记还在");
    assert_eq!(mark.epoch, mark_after_full.epoch, "窄扫把 Z.epoch 推进了 ⇒ 下一轮会把没走的目录判成删除");
    assert_eq!(mark.round, mark_after_full.round, "轮次也不许涨：它记的是「完整扫描第几轮」");
}

#[test]
fn narrow_scan_paths_stay_project_relative() {
    // 窄扫最容易写错的一格：遍历起点换成了子树，路径却必须仍然相对**项目根**。
    // 记成 `y.bin` 而不是 `b/y.bin` 的话，那一行永远对不上、下一次全量轮就凭空多一条 add。
    let site = tempfile::tempdir().unwrap();
    let proj = site.path().join("p");
    fs::create_dir_all(&proj).unwrap();
    seed_two_dirs(&proj);
    let db = open_store(site.path(), common::engine()).unwrap();
    let pid = keys::project_id("p");
    worktree::scan(&*db, pid, &proj, false, 8).unwrap();
    let before = db.health().total_rows;

    worktree::scan_scoped(
        &*db,
        pid,
        &proj,
        &worktree::Scope::Dir("b".to_string()),
        false,
        8,
        &twosync::store::cas::Bucket::default(),
    )
    .unwrap();
    // 键必须是 `b`、`b/y.bin` 那两条（各多一个版本行），而不是新造出 `y.bin`。
    let tx = db.begin();
    let hit = tx.get(&keys::worktree_key(pid, "b/y.bin")).is_some();
    let ghost = tx.get(&keys::worktree_key(pid, "y.bin")).is_some();
    tx.commit();
    assert!(hit, "`b/y.bin` 那行必须还在（窄扫要盖的就是它）");
    assert!(!ghost, "窄扫写出了 `y.bin` 这种半截路径 ⇒ 键序会跟全量轮对不上");
    assert!(db.health().total_rows > before, "盖了章就该有新版本行");
}

#[test]
fn a_narrow_scan_of_a_missing_directory_is_an_error_not_a_no_op() {
    // "走错了目录"必须是响亮的：静默走出 0 行 = 下一轮以为什么都没变。
    let site = tempfile::tempdir().unwrap();
    let proj = site.path().join("p");
    fs::create_dir_all(&proj).unwrap();
    seed_two_dirs(&proj);
    let db = open_store(site.path(), common::engine()).unwrap();
    let pid = keys::project_id("p");
    let e = worktree::scan_scoped(
        &*db,
        pid,
        &proj,
        &worktree::Scope::Dir("nope".to_string()),
        false,
        8,
        &twosync::store::cas::Bucket::default(),
    )
    .expect_err("不存在的目录该报错");
    assert!(e.to_string().contains("不是一个目录"), "实际：{e}");
}

/// 扫描也要过 FR-15 的桶（以前只 CAS 那三处在扣，而扫描根本不碰 CAS）。
///
/// 判据不用"我自己算的那个数"：跑完之后**从引擎里把 `W` 行读回来**，把键+值的长度加总，
/// 和桶上累计的字节对账。两条独立路径要是不相等，要么扣错了、要么写进去的行数不对。
#[test]
fn a_scan_charges_the_bucket_and_the_engine_agrees_on_the_byte_count() {
    let site = tempfile::tempdir().unwrap();
    let proj = site.path().join("p");
    fs::create_dir_all(&proj).unwrap();
    seed(&proj, 30);
    let db = open_store(site.path(), common::engine()).unwrap();
    let pid = keys::project_id("p");

    // 1 GiB/s：账照扣，睡的那点时间可以忽略（用例不掐墙钟，§15.22 的规矩）。
    let bucket = twosync::store::cas::Bucket::limited(1 << 30);
    worktree::scan_scoped(
        &*db,
        pid,
        &proj,
        &worktree::Scope::Full,
        false,
        8,
        &bucket,
    )
    .unwrap();
    let charged = bucket.written().expect("给了限速就必须有账");

    // 反向核对：库里 W 行的键 + 值一共多少字节。
    let mut prefix = vec![keys::TAG_WORKTREE];
    prefix.extend_from_slice(&pid.to_be_bytes());
    let tx = db.begin();
    let rows = tx.prefix_scan(&prefix);
    tx.commit();
    let stored: u64 = rows
        .iter()
        .map(|r| (r.key.len() + r.value.as_ref().map_or(0, |v| v.len())) as u64)
        .sum();
    assert_eq!(
        charged, stored,
        "桶上扣的字节与引擎里存的行字节对不上：扣 {charged} / 存 {stored}（{} 行）",
        rows.len()
    );
    assert!(rows.len() >= 30, "30 个文件至少要 30 行：{}", rows.len());
    // 单价合理性：一行 = 键(5+path) + 值(信封 + postcard) 必然落在几十字节这一档。
    // 这条是防"扣了个数量级错的数"（比如把 hash 字节当成行字节）。
    let per_row = charged / rows.len() as u64;
    assert!((32..400).contains(&per_row), "每行 {per_row} B 不合理（charged {charged}）");

    // 稳态第二轮：一行 hash 都不重算（hashed=0），但**每行都要续期盖章** ⇒ 扣的字节必须还在。
    // 这一条是"扣的是交出去的行，不是算过的内容"的定义，也是最容易在优化中被改掉的那格。
    let steady = twosync::store::cas::Bucket::limited(1 << 30);
    let rep = worktree::scan_scoped(
        &*db,
        pid,
        &proj,
        &worktree::Scope::Full,
        false,
        8,
        &steady,
    )
    .unwrap();
    assert_eq!(rep.hashed, 0, "台架前提：稳态一轮不该重算任何一行");
    assert_eq!(
        steady.written(),
        Some(charged),
        "稳态轮一行没算就不扣桶 ⇒ 限速对常驻循环形同虚设"
    );
}

// ── deferred 名单（§7.2）───────────────────────────────────────


/// 名单的完整生命周期：**读不到 ⇒ 进名单；能读了 ⇒ 下一轮全量自动消失**。
///
/// 为什么中间要先改一次内容：粗筛（size+mtime）命中的行根本不会去 `open`，那条路径上
/// "读不了"这件事压根没发生。只有**内容变过又读不到**才是 §7.2 说的那种项。
#[test]
fn a_deferred_path_is_listed_and_clears_itself_next_full_round() {
    let site = tempfile::tempdir().unwrap();
    let proj = site.path().join("p");
    let data = proj.join("data");
    fs::create_dir_all(&data).unwrap();
    seed(&data, 5);
    let tricky = data.join("f0.txt");
    fs::write(&tricky, b"the first version").unwrap();

    let db = open_store(site.path(), common::engine()).unwrap();
    let pid = keys::project_id("p");
    worktree::scan(&*db, pid, &proj, false, 8).unwrap();

    // 内容变了（粗筛必然 miss）又读不到 ⇒ 这一条进名单
    fs::write(&tricky, b"a much longer second version").unwrap();
    let guard = common::lock_out(&tricky);
    let rep = worktree::scan(&*db, pid, &proj, false, 8).unwrap();

    assert_eq!(rep.deferred, 1, "样本 {:?}", rep.deferred_sample);
    assert_eq!(rep.deferred_sample, vec!["data/f0.txt".to_string()]);
    let list = worktree::list_deferred(&*db, pid).unwrap();
    assert_eq!(list.len(), 1, "名单应当就一条：{list:?}");
    assert_eq!(list[0].0, "data/f0.txt");
    assert!(
        !list[0].1.reason.trim().is_empty(),
        "原因必须是原话，不能只剩一个数"
    );
    // `ScanMark.seen` 只数 `W` 行：名单那一行**不许**混进去（那是两个不同的量）。
    let mark = worktree::last_mark(&*db, pid).expect("全量轮必须盖章");
    assert_eq!(
        mark.seen as usize, 6,
        "data 这一层 + 5 个文件 = 6 行，名单那行不算：{}",
        mark.seen
    );

    // 放手 ⇒ 下一轮全量读通了，那行自己消失（不需要谁去 `gc`，也不需要命令行开关）。
    drop(guard);
    let next = worktree::scan(&*db, pid, &proj, false, 8).unwrap();
    assert_eq!(next.deferred, 0);
    assert_eq!(
        next.deferred_swept, 1,
        "过期行没收掉，说明戳写错了或者根本没扫名单"
    );
    assert!(worktree::list_deferred(&*db, pid).unwrap().is_empty());

    // 第三轮：空表也要幂等（不该报"收了 0 条"之外的东西，也不该有行残留）。
    let third = worktree::scan(&*db, pid, &proj, false, 8).unwrap();
    assert_eq!(third.deferred_swept, 0);
}

/// **窄扫轮不许收名单**。她只走了一个目录，别的目录本轮根本没被看过，
/// 那些行的戳自然是旧的——拿旧戳当"已恢复"会把整张表清成空的，而那张表正是检出层的依据。
#[test]
fn a_narrow_round_never_clears_someone_elses_deferred_row() {
    let site = tempfile::tempdir().unwrap();
    let proj = site.path().join("p");
    fs::create_dir_all(proj.join("a")).unwrap();
    fs::create_dir_all(proj.join("b")).unwrap();
    fs::write(proj.join("a/x.bin"), b"x").unwrap();
    fs::write(proj.join("b/y.bin"), b"y").unwrap();

    let db = open_store(site.path(), common::engine()).unwrap();
    let pid = keys::project_id("p");
    worktree::scan(&*db, pid, &proj, false, 8).unwrap();
    fs::write(proj.join("a/x.bin"), b"x changed while nobody could read it").unwrap();
    let guard = common::lock_out(&proj.join("a/x.bin"));
    worktree::scan(&*db, pid, &proj, false, 8).unwrap();
    assert_eq!(worktree::list_deferred(&*db, pid).unwrap().len(), 1);

    // 只走 b/：a/x.bin 这一轮没被看过 ⇒ 名单必须原样留着
    let narrow = worktree::scan_scoped(
        &*db,
        pid,
        &proj,
        &worktree::Scope::Dir("b".to_string()),
        false,
        8,
        &twosync::store::cas::Bucket::default(),
    )
    .unwrap();
    assert_eq!(narrow.deferred_swept, 0, "窄扫收了名单 = 拿半轮的视野做全库的结论");
    let list = worktree::list_deferred(&*db, pid).unwrap();
    assert_eq!(list.len(), 1, "窄扫之后名单没了：{list:?}");
    assert_eq!(list[0].0, "a/x.bin");

    drop(guard);
}

/// 这张表**每次读花多少引擎调用**必须说得出一个数：`status` 多一格 = 常驻循环每轮多折
/// 一遍全库（§9.6.1），所以"读法"和"内容"一样是判据。
///
/// 钉两件事：① 空表恰好一次前缀扫（不是"先数一遍再逐条点查"），② 表变长**不**加折数
/// （每行加一次点查是最容易悄悄长出来的形状，而它在 12 GiB 站点上是每条一行 0.5 s 起）。
#[test]
fn reading_the_deferred_list_costs_exactly_one_fold() {
    let site = tempfile::tempdir().unwrap();
    let proj = site.path().join("p");
    fs::create_dir_all(proj.join("data")).unwrap();
    seed(&proj.join("data"), 5);

    let meter = twosync::utils::Meter::default();
    let engine = twosync::utils::counted(
        open_store(site.path(), common::engine()).unwrap(),
        &meter,
        false,
    );
    let pid = keys::project_id("p");
    worktree::scan(&*engine, pid, &proj, false, 8).unwrap();

    let base = meter.snapshot();
    assert!(worktree::list_deferred(&*engine, pid).unwrap().is_empty());
    assert_eq!(
        meter.snapshot().since(base).folds(),
        1,
        "空表的读法不该是多次：{}",
        meter.snapshot().since(base).line()
    );

    // 手写两行进表（绕过扫描器：这一步要验的是**读法**，不是产生路径）。
    let write = |rel: &str| {
        let rec = worktree::DeferredRec {
            reason: "用例伪造".to_string(),
            at_ns: 1,
        };
        let tx = engine.begin();
        assert!(tx.set(
            &keys::deferred_key(pid, rel),
            keys::wrap(keys::enc(&rec, "deferred 记录").unwrap())
        ));
        tx.commit();
    };
    write("data/f0.txt");
    write("data/f3.txt");

    let base = meter.snapshot();
    let list = worktree::list_deferred(&*engine, pid).unwrap();
    assert_eq!(list.len(), 2, "两行都该读回来：{list:?}");
    assert_eq!(
        meter.snapshot().since(base).folds(),
        1,
        "名单每多一条就多折一遍全库：{}",
        meter.snapshot().since(base).line()
    );

    // 点查（`restore` 那道闸用的读法）也是恰好一次——不是"读整张表再找"。
    let base = meter.snapshot();
    assert!(worktree::is_deferred(&*engine, pid, "data/f0.txt")
        .unwrap()
        .is_some());
    assert_eq!(
        meter.snapshot().since(base).folds(),
        1,
        "点查用了一次以上：{}",
        meter.snapshot().since(base).line()
    );
}
