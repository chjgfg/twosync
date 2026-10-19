//! 会话账（`S` 前缀）与 `ahead_behind`：FR-16 那三格"历史事实"的地基。
//!
//! 这批用例只测**账本本身**（写入/取回/坏账容忍/差集口径）。真会话怎么产生这些数，
//! 在 `tests/net_session.rs` 那边（`push`/`pull` 各写一条），这里不重复起管道。

use storage_engines::{EngineKind, KvEngine};
use twosync::store::keys::{self, Hash, ProjectId};
use twosync::store::object::{self, Commit, Stats};
use twosync::store::sessions::{self, Dir, SessionRecord};
use twosync::utils::open_store;

const PID: ProjectId = 21;

fn db() -> (tempfile::TempDir, Box<dyn KvEngine>) {
    let site = tempfile::tempdir().unwrap();
    let engine = open_store(site.path(), EngineKind::LsmTree).unwrap();
    (site, engine)
}

/// 造一条记录。`ts` 同时是键里的会话 id，所以两种 ts 必须不一样才测得出顺序。
fn rec(ts: i64, dir: Dir, blocks: usize, peer: &str) -> SessionRecord {
    SessionRecord {
        ts,
        project: "p".into(),
        peer: peer.into(),
        dir,
        blocks,
        bytes: blocks as u64 * 1024,
        skipped: 1,
        ms: 7000,
        our_heads: vec![[ts as u8; 32]],
        their_heads: vec![],
        accepted: None,
    }
}

#[test]
fn the_latest_session_is_the_one_status_shows() {
    let (_d, engine) = db();
    sessions::record(&*engine, PID, &rec(1_000, Dir::Push, 3, "ubuntu")).unwrap();
    sessions::record(&*engine, PID, &rec(2_000, Dir::Pull, 5, "ubuntu")).unwrap();
    // 乱序写也要拿对最新的那条：先写 2 再写 1 的对照在下面 `recent` 的排序里。
    let last = sessions::last(&*engine, PID)
        .unwrap()
        .expect("有两条就该读出一条");
    assert_eq!(last.ts, 2_000, "最新一条应当是 ts 2000：{last:?}");
    assert_eq!(
        last.dir,
        Dir::Pull,
        "方向必须原样回来（`blocks` 的含义跟着它变）"
    );
    assert_eq!(last.blocks, 5);

    let two = sessions::recent(&*engine, PID, 2).unwrap();
    assert_eq!(
        two.iter().map(|r| r.ts).collect::<Vec<_>>(),
        vec![2_000, 1_000],
        "`recent` 是**倒序**（最近的在前），否则打印会拿最旧那条当上次"
    );
    let one = sessions::recent(&*engine, PID, 1).unwrap();
    assert_eq!(one.len(), 1, "要 1 条就给 1 条：{}", one.len());

    // 幂等/覆盖：同一个 ts 再写一次不产生第二行（键就是 ts）。
    let rows_before = engine.health().total_rows;
    sessions::record(&*engine, PID, &rec(2_000, Dir::Push, 9, "ubuntu")).unwrap();
    assert_eq!(
        sessions::recent(&*engine, PID, 9).unwrap().len(),
        2,
        "覆盖不该多出一行"
    );
    assert!(
        engine.health().total_rows > rows_before,
        "覆盖会产生一个版本行（引擎的 MVCC 本性），所以 `gc` 才是必需品"
    );
    assert_eq!(
        sessions::last(&*engine, PID).unwrap().unwrap().dir,
        Dir::Push,
        "同一把键后写的赢"
    );
}

#[test]
fn a_garbage_row_does_not_stop_the_reading_side() {
    // `status` 是一条"看一眼"的命令：一行坏账（半截键、解不出的值）不许把它卡死。
    // 这一条不是"防御性编程"，而是这条命令的定位 —— 报错应该出自 `doctor`/`gc`，不是 `status`。
    let (_d, engine) = db();
    sessions::record(&*engine, PID, &rec(5_000, Dir::ServeIn, 2, "")).unwrap();
    let tx = engine.begin();
    // 半截键尾（不是 8 字节）：手工拼键，`keys` 只导出它自己那两种形状。
    let mut short = vec![keys::TAG_SESSION];
    short.extend_from_slice(&PID.to_be_bytes());
    short.extend_from_slice(b"short");
    tx.set(&short, keys::wrap(vec![0x01u8]));
    // 形状对、内容解不出来
    tx.set(&keys::session_key(PID, 6_000), keys::wrap(vec![0xFFu8; 9]));
    tx.commit();

    let rows = sessions::recent(&*engine, PID, 10).unwrap();
    assert_eq!(rows.len(), 1, "两条坏账都被跳过，好那条还在：{rows:?}");
    assert_eq!(rows[0].ts, 5_000);
}

/// 亲手造一条 commit 并把 `C`/`c` 两处都写进去（`put_commit` 同事务做这两件事）。
fn commit(engine: &dyn KvEngine, parents: &[Hash], seed: u8, ts: i64) -> Hash {
    let tree = object::put_tree(
        engine,
        PID,
        &[object::TreeEntry {
            name: "f".into(),
            kind: keys::Kind::File,
            hash: [seed; 32],
            size: 10,
            mode: 0o100644,
        }],
    )
    .unwrap();
    object::put_commit(
        engine,
        PID,
        &Commit {
            parents: parents.to_vec(),
            tree,
            peer: "test".into(),
            ts,
            msg: format!("c{seed}"),
            stats: Stats {
                add: 1,
                modify: 0,
                delete: 0,
                rename: 0,
                bytes: 10,
            },
        },
    )
    .unwrap()
}

#[test]
fn ahead_behind_counts_commits_not_heads() {
    let (_d, engine) = db();
    // 一条链 g → a → b：本端 head = b（背后三个 commit），对面只看到 g。
    let g = commit(&*engine, &[], 1, 100);
    let a = commit(&*engine, &[g], 2, 200);
    let b = commit(&*engine, &[a], 3, 300);

    let ab = sessions::ahead_behind(&*engine, PID, &[b], &[g]).unwrap();
    assert_eq!(ab.ours, 2, "本端比对面多 a 与 b：{ab:?}");
    assert_eq!(ab.theirs, 0, "对面没有我们看不到的东西：{ab:?}");
    // 判据是"该拉/该推"的文字，两个数错一个就会说反话。
    let line = ab.line();
    assert!(
        line.contains("该推") && !line.contains("该拉 ⇒"),
        "读数：{line}"
    );

    // 反向：对面有整条链，我们停在 g ⇒ 该拉。
    let rev = sessions::ahead_behind(&*engine, PID, &[g], &[b]).unwrap();
    assert_eq!((rev.ours, rev.theirs), (0, 2), "反向读数：{rev:?}");
    assert!(rev.line().contains("该拉"), "读数：{}", rev.line());

    // 两边相等 ⇒ 一句话说"没有新的"，不许出现"该拉/该推"。
    let same = sessions::ahead_behind(&*engine, PID, &[b], &[b]).unwrap();
    assert_eq!((same.ours, same.theirs), (0, 0));
    assert!(same.line().contains("一致"), "读数：{}", same.line());
}

#[test]
fn a_branch_makes_behind_count_the_other_side_whole_chain() {
    // 分叉：两边各自往前走了几步（union 的正常态）。`theirs` 必须是"对面那一支的 commit 数"，
    // 不是"对面 head 的个数"——那正是 §7 表第一行要的那个数。
    let (_d, engine) = db();
    let g = commit(&*engine, &[], 1, 100);
    let a1 = commit(&*engine, &[g], 2, 200);
    let a2 = commit(&*engine, &[a1], 3, 300);
    let b1 = commit(&*engine, &[g], 4, 400);

    let ab = sessions::ahead_behind(&*engine, PID, &[a2], &[b1]).unwrap();
    assert_eq!(ab.ours, 2, "我们这一支 a1/a2：{ab:?}");
    assert_eq!(
        ab.theirs, 1,
        "对面那一支只有 b1（g 是公共祖先，不算）：{ab:?}"
    );
}
