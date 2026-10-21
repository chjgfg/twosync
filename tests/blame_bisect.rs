//! `blame`（路径级归属）与 `bisect`（无状态二分）。两条都是**只读**命令，判据住在库层
//! （[`history::blame_path`] / [`history::bisect_step`]），所以用例也钉在库层 —— 措辞是 `format!`
//! 出来的，钉不住；**枚举与条数**才钉得住。
//!
//! 三条要紧的性质：
//! 1. `blame` 只列**动过她**的那些本，且 `Content`/`Mode`/`KindSwap`/`Appeared`/`Disappeared` 分得开
//!    （`mode` 进树的 hash 但不进 blob 的 hash，§8 ⇒ "只改了权限"是一类真实的变化）；
//! 2. 窗口截断必须**自己说**（`truncated`），不然最老那一行会被读成"她第一次出现"；
//! 3. `bisect` 的保证只有一条：**候选属于怀疑集 ⇒ 每问一次区间严格变小**（`suspects > 1` 时）。
//!    历史里有合并时不保证正好缩一半，所以那条循环用例比"中点等于第 N 本"更该钉。

mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use storage_engines::KvEngine;
use twosync::cli::registry::Registry;
use twosync::store::build::{self, MAIN};
use twosync::store::cas::Cas;
use twosync::store::keys::{self, Hash, Kind, ProjectId};
use twosync::store::{history, object, worktree};
use twosync::utils::objects_dir;

const BLK: u64 = 4096;

struct Site {
    tmp: tempfile::TempDir,
    db: Arc<dyn KvEngine>,
    pid: ProjectId,
}

impl Site {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".twosync")).unwrap();
        common::pin_engine_in_config(tmp.path());
        let db: Arc<dyn KvEngine> =
            Arc::from(twosync::utils::open_store(tmp.path(), common::engine()).unwrap());
        let mut reg = Registry::default();
        reg.add("p", "p").unwrap();
        reg.save(tmp.path()).unwrap();
        Self {
            tmp,
            db,
            pid: keys::project_id("p"),
        }
    }

    fn root(&self) -> PathBuf {
        self.tmp.path().to_path_buf()
    }

    fn work(&self) -> PathBuf {
        self.root().join("p")
    }

    fn write(&self, rel: &str, body: &str) {
        let p = self.work().join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    /// `scan` + `commit`，时间戳由调用方给（`walk` 的排序键就是它，用例里要的是**确定的顺序**）。
    fn commit(&self, msg: &str, ts: i64) -> Hash {
        let work = self.work();
        worktree::scan(&*self.db, self.pid, &work, false, 100).unwrap();
        let store = Cas::new(&objects_dir(&self.root()), BLK).unwrap();
        build::build(
            &*self.db,
            self.pid,
            &store,
            &work,
            build::Meta {
                peer: "t",
                msg,
                ts,
                allow_mass_delete: true,
            },
        )
        .unwrap()
        .commit
        .expect("用例里每次都要有改动")
    }

    fn heads(&self) -> Vec<Hash> {
        build::ref_heads(&*self.db, self.pid, MAIN).unwrap()
    }

    fn blame(&self, path: &str, max: usize) -> history::PathHistory {
        history::blame_path(&*self.db, self.pid, &self.heads(), path, max).unwrap()
    }

    fn bisect(&self, bad: &Hash, good: &Hash) -> history::BisectVerdict {
        history::bisect_step(&*self.db, self.pid, bad, good).unwrap()
    }

    /// 只把两笔"手写"的 commit 摆上台（用于 `mode` 单独变那一支：扫描器在 Windows 上造不出这个形状，
    /// 而判据得能被钉住）。树与 commit 都走**同一套编码**，所以库层看不出区别。
    fn handwritten(&self, a_mode: u32, b_mode: u32) -> (Hash, Hash) {
        let blob = *blake3::hash(b"payload").as_bytes();
        let mk = |mode: u32| object::TreeEntry {
            name: "a.txt".to_string(),
            kind: Kind::File,
            hash: blob,
            size: 7,
            mode,
        };
        let t1 = object::put_tree(&*self.db, self.pid, &[mk(a_mode)]).unwrap();
        let t2 = object::put_tree(&*self.db, self.pid, &[mk(b_mode)]).unwrap();
        let c1 = self.handwritten_commit(&[], t1, 1);
        let c2 = self.handwritten_commit(&[c1], t2, 2);
        build::set_ref(&*self.db, self.pid, MAIN, &[c2]).unwrap();
        (c1, c2)
    }

    fn handwritten_commit(&self, parents: &[Hash], tree: Hash, ts: i64) -> Hash {
        object::put_commit(
            &*self.db,
            self.pid,
            &object::Commit {
                parents: parents.to_vec(),
                tree,
                peer: "local".to_string(),
                ts,
                msg: format!("handwritten ts={ts}"),
                stats: object::Stats {
                    add: 0,
                    modify: 0,
                    delete: 0,
                    rename: 0,
                    bytes: 0,
                },
            },
        )
        .unwrap()
    }
}

/// 只列动过她的那些本：`a.txt` 被改过一次 ⇒ 两行（出现 + 内容换），`b.txt` 只有一行。
#[test]
fn blame_lists_only_the_commits_that_touched_the_path() {
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1", 1);
    site.write("a.txt", "two");
    let v2 = site.commit("v2", 2);
    site.write("b.txt", "bee");
    let v3 = site.commit("v3 加了 b", 3);

    let a = site.blame("a.txt", 0);
    let hits: Vec<(Hash, history::PathChange)> = a
        .hits
        .iter()
        .map(|h| (h.node.hash, h.change.clone()))
        .collect();
    assert_eq!(
        hits,
        vec![
            (v1, history::PathChange::Appeared),
            (
                v2,
                history::PathChange::Content {
                    to: *blake3::hash(b"two").as_bytes()
                }
            ),
        ],
        "a.txt 的归属应该只有这两本（v3 没碰她）"
    );
    assert_eq!(a.scanned, 3, "窗口走满三本");
    assert!(!a.truncated);
    let now = a.now.clone().expect("a.txt 现在还在");
    assert_eq!(now.hash, *blake3::hash(b"two").as_bytes());

    let b = site.blame("b.txt", 0);
    assert_eq!(
        b.hits
            .iter()
            .map(|h| (h.node.hash, h.change.clone()))
            .collect::<Vec<_>>(),
        vec![(v3, history::PathChange::Appeared)]
    );
}

/// 目录的"被动过"= 她那棵子树的 hash 变了。这是**粗**的（子树里任何一处改动都算目录变），
/// 所以命令输出里对目录不会说"改了几个文件" —— 判据与措辞得对上。
#[test]
fn a_directory_is_attributed_when_a_child_inside_it_changes() {
    let site = Site::new();
    site.write("sub/a.txt", "one");
    let v1 = site.commit("v1 建目录", 1);
    site.write("sub/c.txt", "cee");
    let v2 = site.commit("v2 目录里加一个", 2);

    // 期望值**独立算一次**：从 v2 那一本的树里逐层取 `sub`。用 `blame` 的返回值当期望值
    // 就成了自己证自己，那条用例什么都钉不住。
    let v2_tree = object::read_commit(&*site.db, site.pid, &v2).unwrap().tree;
    let want = history::resolve(&*site.db, site.pid, &v2_tree, "sub")
        .unwrap()
        .expect("v2 里 sub 还在")
        .hash;
    let d = site.blame("sub", 0);
    assert_eq!(
        d.hits
            .iter()
            .map(|h| (h.node.hash, h.change.clone()))
            .collect::<Vec<_>>(),
        vec![
            (v1, history::PathChange::Appeared),
            (v2, history::PathChange::Content { to: want })
        ],
        "目录那一层的 hash 变了就该算"
    );
    assert_eq!(d.now.unwrap().kind, Kind::Dir);
}

/// `mode` 单独是一支：内容（blob hash）没变而条目变了 ⇒ 报 `Mode` 不报 `Content`。
/// **为什么手写两笔 commit**：Windows 上 `fs::write` 造不出这个形状，而 §8 那条规矩
/// （`mode` 进树的 hash、不进 blob 的 hash）正是为了让这一类改动**看得见** —— 看不见的那类
/// 判据必须有办法被测到。
#[test]
fn a_mode_only_change_is_attributed_as_mode_not_content() {
    let site = Site::new();
    let (c1, c2) = site.handwritten(0o100644, 0o100755);

    let h = site.blame("a.txt", 0);
    let hits: Vec<history::PathChange> = h.hits.iter().map(|x| x.change.clone()).collect();
    assert_eq!(
        hits,
        vec![
            history::PathChange::Appeared,
            history::PathChange::Mode { to: 0o100755 }
        ],
        "只有权限位变了 ⇒ 第二行必须是 Mode（{} → {}）",
        keys::short(&c1),
        keys::short(&c2)
    );
}

/// 窗口截断要**自己说**。不说出来，最老那一行就会被读成"她第一次出现"——那正是
/// `git log -n` 与 `blame` 混在一起时最常见的误读。
#[test]
fn blame_says_so_when_the_window_is_cut_off() {
    let site = Site::new();
    site.write("a.txt", "one");
    site.commit("v1", 1);
    site.write("a.txt", "two");
    site.commit("v2", 2);
    site.write("a.txt", "three");
    site.commit("v3", 3);

    let h = site.blame("a.txt", 1);
    assert!(h.truncated, "窗口只走 1 本 ⇒ truncated 必须为真");
    assert_eq!(h.scanned, 1);
    assert_eq!(h.hits.len(), 1, "窗口里只看得见最近那本的改动");
    assert_eq!(h.hits[0].change, history::PathChange::Appeared);

    // 同一条路径走全史：三本都动过，且**最老那一行是真正的 Appeared**
    let all = site.blame("a.txt", 0);
    assert!(!all.truncated);
    assert_eq!(all.hits.len(), 3);
    assert!(matches!(
        all.hits[0].change,
        history::PathChange::Appeared
    ));
}

/// 问一条从来没有过的路径不是错误：`hits` 空、`now` 是 `None`。
/// （`show`/`restore` 对不存在的路径会报错，那两边是"要东西"；这里是"看一眼"，不该拦。）
#[test]
fn blame_on_a_path_that_never_existed_is_empty_not_an_error() {
    let site = Site::new();
    site.write("a.txt", "one");
    site.commit("v1", 1);
    let h = site.blame("nope.txt", 0);
    assert!(h.hits.is_empty());
    assert!(h.now.is_none());
    assert_eq!(h.scanned, 1, "窗口照走：她一条没动，不代表没看过");
}

/// **删除也是归属**：她消失的那一本要出现在表里（`Disappeared`），`now` 变 `None`。
#[test]
fn a_deletion_is_attributed_to_the_commit_that_removed_it() {
    let site = Site::new();
    site.write("a.txt", "one");
    let v1 = site.commit("v1", 1);
    site.write("gone.txt", "bye");
    let v2 = site.commit("v2 加了 gone", 2);
    fs::remove_file(site.work().join("gone.txt")).unwrap();
    let v3 = site.commit("v3 删了 gone", 3);
    let _ = v1;

    let h = site.blame("gone.txt", 0);
    assert_eq!(
        h.hits
            .iter()
            .map(|x| (x.node.hash, x.change.clone()))
            .collect::<Vec<_>>(),
        vec![
            (v2, history::PathChange::Appeared),
            (v3, history::PathChange::Disappeared),
        ]
    );
    assert!(h.now.is_none(), "现在没有她");
}

/// 线性历史上中点就是中点：七本里坏在最外、好在最老 ⇒ 怀疑集 6 本，候选是 v4。
#[test]
fn bisect_picks_the_middle_of_the_suspect_range() {
    let site = Site::new();
    let mut line: Vec<Hash> = Vec::new();
    for i in 1..=7 {
        site.write(&format!("f{i}.txt"), &format!("body {i}"));
        line.push(site.commit(&format!("v{i}"), i as i64));
    }
    let step = match site.bisect(&line[6], &line[0]) {
        history::BisectVerdict::Step(s) => s,
        other => panic!("应该是正常的一步，实际 {other:?}"),
    };
    assert_eq!(step.suspects, 6, "v2..v7");
    assert_eq!(step.candidate, line[3], "中点该是 v4");
}

/// **这条才是 `bisect` 的全部承诺**：候选属于怀疑集 ⇒ 每一次分类之后区间严格变小（直到只剩一本）。
/// 不保证"正好一半"（历史里有合并时时间序不等于拓扑序），所以用例钉的是缩小而不是对折。
#[test]
fn bisect_range_shrinks_every_step_until_one_suspect_is_left() {
    let site = Site::new();
    let mut line: Vec<Hash> = Vec::new();
    for i in 1..=9 {
        site.write(&format!("f{i}.txt"), &format!("body {i}"));
        line.push(site.commit(&format!("v{i}"), i as i64));
    }
    let (good, mut bad) = (line[0], line[8]);
    let mut seen: Vec<Hash> = Vec::new();
    let mut prev = usize::MAX;
    for _ in 0..12 {
        let s = match site.bisect(&bad, &good) {
            history::BisectVerdict::Step(s) => s,
            other => panic!("线性历史不该走到退化分支：{other:?}"),
        };
        if s.suspects == 1 {
            // 收到只剩一本：这一本就是上一轮问过的那一本（**收敛的最后一步，不是死循环**），
            // 命令在这一格改口报"结论"而不是"下一步"。
            assert_eq!(s.candidate, bad, "只剩一本时她就该是答案");
            assert!(seen.contains(&s.candidate), "只剩一本时重复是正常的：{seen:?}");
            return;
        }
        assert!(!seen.contains(&s.candidate), "候选被重复问过 ⇒ 区间没缩小：{seen:?}");
        assert!(s.suspects < prev, "区间必须严格变小：{prev} → {}", s.suspects);
        prev = s.suspects;
        seen.push(s.candidate);
        // 模拟"这一本还是坏的"：把上界收到候选（最常见的用法，区间因此至少去掉候选本身）
        bad = s.candidate;
    }
    panic!("十二步还没收到一本（seen={seen:?}）");
}

/// 两种退化形状都要**分开**报：措辞不同、下一步也不同。（"两端之间一条都没有"那一支不存在，
/// 理由写在 [`history::BisectVerdict`] 的文档里 —— 不写到不了的地方去。）
#[test]
fn bisect_refuses_endpoints_that_are_not_on_one_line() {
    let site = Site::new();
    site.write("a.txt", "one");
    let base = site.commit("base", 1);
    site.write("a.txt", "left");
    let left = site.commit("left", 2);
    // 另起一条**不相关**的线（清空 head 再提 ⇒ 新根）
    build::set_ref(&*site.db, site.pid, MAIN, &[]).unwrap();
    site.write("z.txt", "zzz");
    let other = site.commit("另一条线", 3);
    build::set_ref(&*site.db, site.pid, MAIN, &[left]).unwrap();

    // 两端不在同一条线上（`other` 不是 `left` 的祖先）
    assert_eq!(
        site.bisect(&left, &other),
        history::BisectVerdict::GoodNotAncestor
    );
    // 写反了（`base` 反而是 `left` 的祖先… 这里用 left/base 的正反两向）
    assert_eq!(
        site.bisect(&base, &left),
        history::BisectVerdict::Reversed,
        "`base` 比 `left` 老：坏的那一本反而更老"
    );
}
