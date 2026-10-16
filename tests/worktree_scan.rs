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
