//! M0 批次：本地、不联网。
//!
//! init project add/list/use/remove/status scan commit mv status log diff show
//! ls-files restore checkout plan apply config doctor

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use storage_engines::KvEngine;

use crate::allocstats;
use crate::cli::Cli;
use crate::cli::cmd::ProjectCmd;
use crate::cli::registry::Registry;
use crate::error::{Error, Result};
use crate::store::keys::{self, Hash, Kind, ProjectId};
use crate::store::{build, cas, checkout, history, object, restore, worktree};
use crate::utils::{
    db_dir, journal_dir, log_dir, objects_dir, open_store, projects_file, site_dir,
};

/// `Commit.peer` 的占位值。真正的 peer id 要等 M1 的 `peer add|trust`；现在填什么都一样——
/// 它**只是元数据，不参与任何裁决**（§9.5.2 / §15.7），所以不值得为它提前做一套身份。
const PEER_LOCAL: &str = "local";

/// 建站点目录布局。幂等：`create_dir_all` 对已存在的目录直接返回 Ok，
/// 所以不需要"先 exists 再建"（那样反而是 TOCTOU）。
pub fn init(cli: &Cli) -> Result<()> {
    let root = &cli.root;
    for dir in [
        site_dir(root),
        objects_dir(root),
        db_dir(root),
        journal_dir(root),
        log_dir(root),
    ] {
        let fresh = !dir.exists();
        fs::create_dir_all(&dir)?;
        println!(
            "{} {}",
            if fresh { "创建" } else { "已存在" },
            dir.display()
        );
    }

    let projects = projects_file(root);
    if projects.exists() {
        println!("已存在 {}", projects.display());
    } else {
        fs::write(&projects, "# twosync 项目注册表（§16.1）\n")?;
        println!("写入 {}", projects.display());
    }
    Ok(())
}

/// 打开基座 → FR-21 自检 → 打印健康度。句柄用完即弃，不跨命令持有（§9.6.2）。
pub fn doctor(cli: &Cli) -> Result<()> {
    let db = open_store(&cli.root, cli.engine)?;
    // NFR-8：`health()` 是 O(物理行数)，只允许出现在这种人工触发的诊断路径上。
    let h = db.health();
    println!("engine      = {}", db.name());
    println!(
        "db          = {}",
        db.path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<内存>".to_string())
    );
    println!("rows        = {}", h.total_rows);
    println!("live_keys   = {}", h.live_keys);
    println!("max_chain   = {}", h.max_chain);
    println!("rows_above  = {}", h.rows_above_one);
    println!("active_txns = {}", h.active_txns);
    Ok(())
}

/// `project add|list|use`。注册表是所有键的前置：`project_id` 由名字派生。
pub fn project(cli: &Cli, cmd: &ProjectCmd) -> Result<()> {
    let mut reg = Registry::load(&cli.root)?;
    match cmd {
        ProjectCmd::Add { name, path } => {
            // 存成 `/` 分隔的相对路径（§8）；不给 --path 就按 root/<name> 约定放。
            let rel = match path {
                Some(p) => p.to_string_lossy().replace('\\', "/"),
                None => name.clone(),
            };
            reg.add(name, &rel)?;
            reg.save(&cli.root)?;
            println!(
                "已注册 {name} → {rel}  (id={:#010x})",
                keys::project_id(name)
            );
        }
        ProjectCmd::List => {
            if reg.projects.is_empty() {
                println!("（还没有项目：twosync project add <name>）");
            }
            for (name, p) in &reg.projects {
                let mark = if reg.current.as_deref() == Some(name.as_str()) {
                    "*"
                } else {
                    " "
                };
                println!(
                    "{mark} {name:20} id={:#010x}  {}",
                    keys::project_id(name),
                    p.path
                );
            }
        }
        ProjectCmd::Use { name } => {
            reg.select(name)?;
            reg.save(&cli.root)?;
            println!("当前项目 = {name}");
        }
        ProjectCmd::Remove { name, purge } => {
            // 先判 `--purge` 再动手：回收还不支持，不能把注册表改了才说后半截没做。
            if *purge {
                return Err(Error::NotYet {
                    what: format!("project remove --purge {name}"),
                    milestone: "回收要按跨 project 的 refcnt 走，那是 M1 的 `gc`（§9.3）",
                });
            }
            let p = reg.remove(name)?;
            reg.save(&cli.root)?;
            let pid = keys::project_id(name);
            let st = proj_stats(&*db_or_open(cli)?, pid)?;
            println!("已摘掉 {name}  (id={pid:#010x} → {})", p.path);
            println!("  工作树没动 : {}", cli.root.join(&p.path).display());
            println!(
                "  历史也没动 : W {} 行 / commit {} 个 / head {} 个",
                st.wt_rows,
                st.commit_rows,
                st.heads.len()
            );
            println!("  再 `project add {name}` 会接上**同一份**历史（id 由名字派生，§16.1）");
            if st.heads.is_empty() {
                println!("  这个项目还没有 commit，摘掉不留任何东西");
            } else {
                println!("  要真回收磁盘：等 M1 的 `gc`（跨 project 的 refcnt 只有它算得清）");
            }
            println!("  `current` 已清空：下一条命令请先 `project use <name>`");
        }
        ProjectCmd::Status { name } => {
            let db = db_or_open(cli)?;
            match name {
                None => {
                    if reg.projects.is_empty() {
                        println!("（还没有项目：twosync project add <name>）");
                        return Ok(());
                    }
                    println!(
                        "{:<4} {:<20} {:<11} {:<9} {:<21} {:>7} {:>7}",
                        "", "project", "id", "head", "最后完整扫描", "W 行", "commit"
                    );
                    for n in reg.projects.keys() {
                        print_row(&*db, &reg, n)?;
                    }
                }
                Some(n) => {
                    let p = reg
                        .projects
                        .get(n)
                        .ok_or_else(|| Error::Registry(format!("没有项目 {n}")))?;
                    let pid = keys::project_id(n);
                    let st = proj_stats(&*db, pid)?;
                    let work = cli.root.join(&p.path);
                    println!("project   = {n}  (id={pid:#010x})");
                    println!(
                        "path      = {}  ({})",
                        work.display(),
                        if work.is_dir() {
                            "在"
                        } else {
                            "目录不存在！"
                        }
                    );
                    println!(
                        "current   = {}",
                        if reg.current.as_deref() == Some(n.as_str()) {
                            "是（后续命令默认作用在它）"
                        } else {
                            "否"
                        }
                    );
                    match st.mark {
                        Some(m) => println!(
                            "最后扫描  = {}  第 {} 轮，盖章 {}/{} 行",
                            history::fmt_ts(m.epoch),
                            m.round,
                            m.seen,
                            m.walked
                        ),
                        None => println!(
                            "最后扫描  = 从没**完整**跑完过 ⇒ commit 认为没有任何活行，先 scan"
                        ),
                    }
                    if st.heads.is_empty() {
                        println!("head      = （还没有 commit）");
                    }
                    for h in &st.heads {
                        let c = object::read_commit(&*db, pid, h)?;
                        println!(
                            "head      = {}  {}  {:<7} +{} ~{} -{}  {}",
                            keys::short(h),
                            history::fmt_ts(c.ts),
                            c.peer,
                            c.stats.add,
                            c.stats.modify,
                            c.stats.delete,
                            c.msg
                        );
                    }
                    println!(
                        "库里      = W {} 行 / commit {} 行",
                        st.wt_rows, st.commit_rows
                    );
                    let plan = build::plan(&*db, pid)?;
                    let cs = &plan.changes;
                    println!(
                        "未提交    = +{} ~{} -{}  (按最后一次扫描；再改的先 scan)",
                        cs.add, cs.modify, cs.delete
                    );
                }
            }
        }
    }
    Ok(())
}

/// 一个项目在引擎里的体检数字。
///
/// 每一项都要开引擎数一遍键前缀（`key_count` 是 O(匹配行数)），所以这条和 `doctor` 一样
/// 只许人工触发，不许进任何自动循环（NFR-8）。
struct ProjStats {
    mark: Option<worktree::ScanMark>,
    wt_rows: usize,
    commit_rows: usize,
    heads: Vec<Hash>,
}

fn proj_stats(db: &dyn KvEngine, pid: ProjectId) -> Result<ProjStats> {
    let count = |tag: u8| {
        let tx = db.begin();
        let n = tx.key_count(&keys::project_prefix(tag, pid));
        tx.commit();
        n
    };
    Ok(ProjStats {
        mark: worktree::last_mark(db, pid),
        wt_rows: count(keys::TAG_WORKTREE),
        commit_rows: count(keys::TAG_COMMIT),
        heads: build::ref_heads(db, pid, build::MAIN)?,
    })
}

fn print_row(db: &dyn KvEngine, reg: &Registry, name: &str) -> Result<()> {
    let pid = keys::project_id(name);
    let st = proj_stats(db, pid)?;
    let mark = match st.mark {
        Some(m) => format!("{} 第{}轮", history::fmt_ts(m.epoch), m.round),
        None => "从没完整扫过".to_string(),
    };
    let head = match st.heads.len() {
        0 => "—".to_string(),
        1 => keys::short(&st.heads[0]),
        n => format!("{n}head"),
    };
    let star = if reg.current.as_deref() == Some(name) {
        "*"
    } else {
        " "
    };
    println!(
        "{star:<4} {:<20} {:<11} {:<9} {:<21} {:>7} {:>7}",
        name,
        format!("{pid:#010x}"),
        head,
        mark,
        st.wt_rows,
        st.commit_rows
    );
    Ok(())
}

/// `project` 里几条命令都要开引擎，但 `add`/`list`/`use` 不需要——所以不在函数开头统一开。
fn db_or_open(cli: &Cli) -> Result<Box<dyn KvEngine>> {
    open_store(&cli.root, cli.engine)
}

/// 项目作用域：`--project` 显式指定 > `project use` 选中的那个。
/// 返回 (项目名, 项目绝对路径)。名字要单独拿出去，因为 `project_id` 是**从名字派生**的。
fn resolve_project(cli: &Cli) -> Result<(String, PathBuf)> {
    let reg = Registry::load(&cli.root)?;
    let project = match &cli.project {
        Some(name) => reg
            .projects
            .get(name)
            .ok_or_else(|| Error::Registry(format!("没有项目 {name}")))?,
        None => reg.current()?,
    };
    let root = cli.root.join(&project.path);
    if !root.is_dir() {
        return Err(Error::Registry(format!(
            "项目目录不存在：{}",
            root.display()
        )));
    }
    Ok((project.name.clone(), root))
}

pub fn scan(cli: &Cli, force: bool, batch: usize) -> Result<()> {
    let (name, root) = resolve_project(cli)?;
    let db = open_store(&cli.root, cli.engine)?;
    let pid = keys::project_id(&name);
    let wm = allocstats::watermark();
    let rep = worktree::scan(&*db, pid, &root, force, batch)?;
    let peak = wm.peak_growth();

    let secs = rep.elapsed_ms as f64 / 1000.0;
    println!("project     = {name}  (id={pid:#010x})");
    println!("root        = {}", root.display());
    println!("walked      = {}", rep.walked);
    println!(
        "hashed      = {}  (skipped {} / deferred {})",
        rep.hashed, rep.skipped, rep.deferred
    );
    println!(
        "bytes       = {:.1} MiB  (本轮重算 {:.1} MiB)",
        rep.bytes as f64 / 1048576.0,
        rep.hashed_bytes as f64 / 1048576.0
    );
    println!("elapsed     = {secs:.3} s");
    // 吞吐只按**实际 hash 的字节**算：拿 rep.bytes 当分子会把 skipped 的字节也算进去，
    // 于是"全都跳过了"反而显示成 0 MiB/s。
    println!(
        "throughput  = {:.1} MiB/s",
        rep.hashed_bytes as f64 / 1048576.0 / secs.max(0.001)
    );
    // §9.6.1 的量尺：常数内存 ⇒ 这个数与文件数、最大文件都无关，只与 batch/HASH_BUF 有关。
    println!("peak heap   = {:.1} MiB", peak as f64 / 1048576.0);
    // 第二轮本该 hashed=0；不为 0 时得能问出"是哪一个"。样本本身有界（≤10 条）。
    if rep.hashed > 0 && rep.hashed <= rep.sample.len() {
        println!("重算的是：{}", rep.sample.join(", "));
    }
    if rep.deferred > 0 {
        println!(
            "注意：{n} 项读不了，已 deferred（不是删除）",
            n = rep.deferred
        );
    }
    Ok(())
}

/// 把 `W` 表的**最新一轮**快照固化成 commit（FR-4）：落块 → 建树 → 比对 → 移 head。
///
/// 这一条**不重新扫描**，只吃上一次 `scan` 的结果——所以刚改完文件没 scan 就 commit，
/// 提交的是上一次扫描看见的状态。中间文件又变了的话会撞 `StaleScan`，不会静默用新内容。
pub fn commit(cli: &Cli, msg: Option<&str>, allow_mass_delete: bool) -> Result<()> {
    let (name, root) = resolve_project(cli)?;
    let db = open_store(&cli.root, cli.engine)?;
    let pid = keys::project_id(&name);
    // 块仓是**站点级**的一份（挂在 `--root` 下），不是每项目一份：跨项目去重靠这个。
    let store = cas::Cas::new(&objects_dir(&cli.root), cas::DEFAULT_BLOCK)?;

    let started = Instant::now();
    let wm = allocstats::watermark();
    let b = build::build(
        &*db,
        pid,
        &store,
        &root,
        build::Meta {
            peer: PEER_LOCAL,
            msg: msg.unwrap_or("（未填写说明）"),
            ts: worktree::now_ns(),
            allow_mass_delete,
        },
    )?;
    let peak = wm.peak_growth();

    println!("project     = {name}  (id={pid:#010x})");
    println!("root        = {}", root.display());
    match b.commit {
        Some(h) => println!("commit      = {}", keys::hex(&h)),
        None => println!("commit      = （无改动：不产生空提交，head 不动）"),
    }
    println!("tree        = {}", keys::hex(&b.tree));
    let c = &b.changes;
    println!(
        "changes     = +{} ~{} -{}   入 {:.1} MiB / 删 {:.1} MiB",
        c.add,
        c.modify,
        c.delete,
        c.bytes as f64 / 1048576.0,
        c.deleted_bytes as f64 / 1048576.0
    );
    println!(
        "cas         = blob {} 新，块 {} 新写入 {:.1} MiB",
        b.blobs_new,
        b.blocks_stored,
        b.block_bytes as f64 / 1048576.0
    );
    println!("entries     = {}  (库里 W 行 {})", b.entries, b.known);
    if b.entries == 0 && b.known > 0 {
        println!(
            "注意：本轮快照一条都没有而库里有 {n} 行 ⇒ 上一次 scan 可能中断过，先重跑 scan",
            n = b.known
        );
    }
    println!("elapsed     = {:.3} s", started.elapsed().as_secs_f64());
    // 与 scan 同一把尺：这一条要物化条目表（O(文件数)），所以它**不是**常数内存，
    // 上限由 MAX_ENTRIES 显式兜着。这里打印是为了让那次权衡可见。
    println!("peak heap   = {:.1} MiB", peak as f64 / 1048576.0);
    Ok(())
}

/// `log`：从 head 集合出发按时间倒序列出 commit。
///
/// 起点是**集合**不是单个 ref（§4：多 head 是正常态）。真正的 union/裁决是 M3 的活，
/// 这里只保证"两个 head 的历史都能列出来、共同祖先不重复"。
pub fn log(cli: &Cli, max_count: usize) -> Result<()> {
    let (name, _) = resolve_project(cli)?;
    let db = open_store(&cli.root, cli.engine)?;
    let pid = keys::project_id(&name);
    let heads = build::ref_heads(&*db, pid, build::MAIN)?;
    if heads.is_empty() {
        println!("project {name} (id={pid:#010x}) 还没有 commit：先 scan，再 commit");
        return Ok(());
    }
    println!(
        "project   = {name}  (id={pid:#010x})  head = {}",
        heads.len()
    );
    if heads.len() > 1 {
        println!("注意：多 head。这里只把它们都当起点，union 与 collision 判定是 M3。");
    }

    let nodes = history::walk(&*db, pid, &heads, max_count)?;
    for n in &nodes {
        let s = &n.commit.stats;
        println!(
            "{}  {}  {:<7} +{:<5} ~{:<4} -{:<4} {}",
            keys::short(&n.hash),
            history::fmt_ts(n.commit.ts),
            n.commit.peer,
            s.add,
            s.modify,
            s.delete,
            n.commit.msg,
        );
    }
    if max_count > 0 && nodes.len() == max_count {
        println!("（已达 {max_count} 条上限；--max-count 0 = 全部，但那是 O(历史长度) 内存）");
    }
    Ok(())
}

/// `show`：一个 commit 的元数据 + 某一层的清单，或一个文件的块清单。
pub fn show(cli: &Cli, rev: &str, path: Option<&str>) -> Result<()> {
    let (name, _) = resolve_project(cli)?;
    let db = open_store(&cli.root, cli.engine)?;
    let pid = keys::project_id(&name);
    // 只有符号链接那一支会真去读字节（目标文本存在 CAS 里，不在引擎里）。
    let store = cas::Cas::new(&objects_dir(&cli.root), cas::DEFAULT_BLOCK)?;
    let heads = history::resolve_rev(&*db, pid, rev)?;
    let hash = one_commit(heads, rev)?;
    let c = object::read_commit(&*db, pid, &hash)?;

    println!("project   = {name}  (id={pid:#010x})");
    println!("commit    = {}", keys::hex(&hash));
    println!("tree      = {}", keys::hex(&c.tree));
    println!(
        "parents   = {}",
        if c.parents.is_empty() {
            "（无，初始提交）".to_string()
        } else {
            c.parents
                .iter()
                .map(keys::short)
                .collect::<Vec<_>>()
                .join(" ")
        }
    );
    println!("ts        = {}  (epoch {} ns)", history::fmt_ts(c.ts), c.ts);
    println!("peer      = {}", c.peer);
    println!("msg       = {}", c.msg);
    let s = &c.stats;
    println!(
        "stats     = +{} ~{} -{}  改名 {}   入 {:.1} MiB",
        s.add,
        s.modify,
        s.delete,
        s.rename,
        s.bytes as f64 / 1048576.0
    );

    match path {
        None => {
            let entries = object::read_tree(&*db, pid, &c.tree)?;
            println!("根目录 {} 条：", entries.len());
            for e in &entries {
                print_entry(e);
            }
        }
        Some(p) => {
            let e = history::resolve(&*db, pid, &c.tree, p)?.ok_or_else(|| Error::NoSuchPath {
                rev: rev.to_string(),
                path: p.to_string(),
            })?;
            print_entry(&e);
            match e.kind {
                Kind::File => print_blob(&*db, pid, &e.hash)?,
                Kind::Symlink => {
                    // 目标文本本身就在 CAS 里（§17 补记三），所以这里能直接答"指向哪儿"。
                    let blob = build::read_blob(&*db, pid, &e.hash)?;
                    let target = restore::link_target(&blob, &store)?;
                    println!(
                        "  链接目标  = {target}  ({} B / {} 块)",
                        blob.size,
                        blob.blocks.len()
                    );
                }
                Kind::Dir => {
                    let kids = object::read_tree(&*db, pid, &e.hash)?;
                    println!("目录 {} 条：", kids.len());
                    for k in &kids {
                        print_entry(k);
                    }
                }
            }
        }
    }
    Ok(())
}

fn print_blob(db: &dyn KvEngine, pid: ProjectId, hash: &Hash) -> Result<()> {
    let blob = build::read_blob(db, pid, hash)?;
    let zero = blob.blocks.iter().filter(|b| b.zero).count();
    println!("  blob      = {}", keys::hex(&blob.hash));
    println!(
        "  block     = {} 块 × {} B（其中全零 {zero} 块不落盘）",
        blob.blocks.len(),
        blob.block_size
    );
    for (i, b) in blob.blocks.iter().take(4).enumerate() {
        println!(
            "  [{i}] {}  {} B{}",
            keys::short(&b.hash),
            b.size,
            if b.zero { "  (zero)" } else { "" }
        );
    }
    if blob.blocks.len() > 4 {
        println!("  … 其余 {} 块省略", blob.blocks.len() - 4);
    }
    Ok(())
}

fn print_entry(e: &object::TreeEntry) {
    let kind = match e.kind {
        Kind::File => "文件",
        Kind::Dir => "目录",
        Kind::Symlink => "链接",
    };
    println!(
        "  {kind}  {:>12} B  mode {:o}  {}  {}",
        e.size,
        e.mode,
        keys::short(&e.hash),
        e.name
    );
}

/// `show` 与 `restore` 共用：rev 解析出来的集合必须恰好一个。
///
/// 多 head 在这里**报错而不是取第一个**：取第一个 = 静默挑一个版本给用户看/写回工作树，
/// 而 union 与 collision 判定是 M3 的活。宁可在 M3 之前拒绝服务，也不做一个"看起来成功、
/// 内容却是随机一条链"的恢复。
fn one_commit(heads: Vec<Hash>, rev: &str) -> Result<Hash> {
    match heads.as_slice() {
        [one] => Ok(*one),
        [] => Err(Error::MissingObject {
            hash: rev.to_string(),
            kind: "commit".to_string(),
        }),
        many => Err(Error::Codec(format!(
            "{rev} 指向 {} 个 commit：多 head 的 union 还没做（M3），请指定具体 commit",
            many.len()
        ))),
    }
}

/// `restore`：从某个版本取一个路径回工作树（或取到别处）。
pub fn restore(cli: &Cli, at: &str, path: &str, to: Option<&Path>, force: bool) -> Result<()> {
    let (name, root) = resolve_project(cli)?;
    let db = open_store(&cli.root, cli.engine)?;
    let pid = keys::project_id(&name);
    let store = cas::Cas::new(&objects_dir(&cli.root), cas::DEFAULT_BLOCK)?;
    let commit = one_commit(history::resolve_rev(&*db, pid, at)?, at)?;

    let r = restore::restore(&*db, pid, &store, &root, &commit, path, to, force)?;
    println!("project   = {name}  (id={pid:#010x})");
    println!("from      = {}", keys::hex(&commit));
    println!("path      = {path}  ({})", kind_word(r.kind));
    println!("dest      = {}", r.dest.display());
    println!("bytes     = {} / {} 块", r.bytes, r.blocks);
    if let Some(t) = &r.target {
        println!("链接目标  = {t}");
    }
    println!(
        "§7.3      = {}",
        if r.gate_checked {
            "目标已存在，已确认被覆盖的那一版在历史里"
        } else {
            "目标不存在，不需要门"
        }
    );
    Ok(())
}

/// `ls-files`：列一个版本里的路径。**流式打印，不物化**（一百万个文件也不先装进 Vec）。
///
/// 与 git 的差别：这里连目录一起列。目录在本项目里是真条目（它带 mode，也可能是空的），
/// 藏起来就会让"这个版本里到底有什么"少一层。
pub fn ls_files(cli: &Cli, at: &str, prefix: Option<&str>) -> Result<()> {
    let (name, _) = resolve_project(cli)?;
    let db = open_store(&cli.root, cli.engine)?;
    let pid = keys::project_id(&name);
    let commit = one_commit(history::resolve_rev(&*db, pid, at)?, at)?;
    let c = object::read_commit(&*db, pid, &commit)?;

    let root = match prefix {
        None => c.tree,
        Some(p) => {
            let clean = p.trim_matches('/');
            let e =
                history::resolve(&*db, pid, &c.tree, clean)?.ok_or_else(|| Error::NoSuchPath {
                    rev: at.to_string(),
                    path: p.to_string(),
                })?;
            if e.kind != Kind::Dir {
                return Err(Error::NotYet {
                    what: format!("--prefix {p} 指向的不是目录"),
                    milestone: "看单个文件用 `show <rev> <path>`",
                });
            }
            e.hash
        }
    };
    let mut n = 0usize;
    history::walk_tree(&*db, pid, &root, &mut |path, e| {
        n += 1;
        println!("{:>10} B  {}  {}", e.size, keys::short(&e.hash), path);
        Ok(())
    })?;
    println!(
        "共 {n} 条  (project={name} rev={} prefix={})",
        keys::short(&commit),
        prefix.unwrap_or(".")
    );
    Ok(())
}

/// `checkout`：把一个版本落到工作树。默认**只报不动手**。
pub fn checkout(
    cli: &Cli,
    at: &str,
    prefix: Option<&str>,
    apply: bool,
    allow_mass_delete: bool,
) -> Result<()> {
    let (name, root) = resolve_project(cli)?;
    let db = open_store(&cli.root, cli.engine)?;
    let pid = keys::project_id(&name);
    let store = cas::Cas::new(&objects_dir(&cli.root), cas::DEFAULT_BLOCK)?;
    let commit = one_commit(history::resolve_rev(&*db, pid, at)?, at)?;

    let started = Instant::now();
    let r = checkout::checkout(
        &*db,
        pid,
        &store,
        &root,
        &commit,
        prefix,
        checkout::Opts {
            apply,
            allow_mass_delete,
        },
    )?;

    println!("project   = {name}  (id={pid:#010x})");
    println!("目标      = {}  ({at})", keys::hex(&commit));
    println!("工作树    = {}", root.display());
    if r.is_noop() {
        println!("动作      = 无（工作树已经是这个样子）");
        return Ok(());
    }
    println!(
        "动作      = 建 {} / 建目录 {} / 覆盖 {} / 删 {} / 删目录 {} / 跳过 {}",
        r.create, r.mkdir, r.overwrite, r.delete, r.rmdir, r.skipped
    );
    if r.dirty > 0 {
        println!(
            "提醒      = 工作树相对 HEAD 另有 {n} 处未提交改动（`status` 能列出来）。这次只碰上面列出的路径，其余原样留下。",
            n = r.dirty
        );
    }
    for line in &r.sample {
        println!("  {line}");
    }
    if r.sample_more > 0 {
        println!(
            "  …另有 {} 条未列出（列表上限 {}）",
            r.sample_more,
            build::SAMPLE_MAX
        );
    }
    if r.applied {
        println!("已执行。工作树变了而 `W` 还是旧的 ⇒ 下一步 `scan`，再看 `status`。");
    } else {
        println!("没有动任何东西：加 --apply 才真做。");
    }
    println!("elapsed   = {:.3} s", started.elapsed().as_secs_f64());
    Ok(())
}

fn kind_word(k: Kind) -> &'static str {
    match k {
        Kind::File => "文件",
        Kind::Dir => "目录",
        Kind::Symlink => "符号链接",
    }
}

/// `status`：工作树（按最后一次扫描）与 HEAD 的差。**走 `build::plan`，不落任何盘**。
pub fn status(cli: &Cli) -> Result<()> {
    let (name, _) = resolve_project(cli)?;
    let db = open_store(&cli.root, cli.engine)?;
    let pid = keys::project_id(&name);
    let p = build::plan(&*db, pid)?;

    println!("project   = {name}  (id={pid:#010x})");
    let heads = build::ref_heads(&*db, pid, build::MAIN)?;
    println!(
        "head      = {}",
        if heads.is_empty() {
            "（还没有 commit）".to_string()
        } else {
            heads.iter().map(keys::short).collect::<Vec<_>>().join(" ")
        }
    );
    match p.epoch {
        0 => println!("最后扫描  = 从来没有扫过：先 twosync scan"),
        e => println!(
            "最后扫描  = {}  ({}/{} 条被看见)",
            history::fmt_ts(e),
            p.entries,
            p.known
        ),
    }
    print_changes(&p.changes);
    println!("注意：比的是「最后一次扫描」与 HEAD；扫描之后又改的文件不在这里，先 scan。");
    Ok(())
}

/// `diff --at REV`：同一份变更表，只是比较对象换成任意一个版本。
pub fn diff(cli: &Cli, at: &str) -> Result<()> {
    let (name, _) = resolve_project(cli)?;
    let db = open_store(&cli.root, cli.engine)?;
    let pid = keys::project_id(&name);
    let commit = one_commit(history::resolve_rev(&*db, pid, at)?, at)?;
    let want = object::read_commit(&*db, pid, &commit)?;
    let p = build::plan(&*db, pid)?;
    let c = build::diff(&*db, pid, Some(&want.tree), &p.tree, &p.trees)?;

    println!("project   = {name}  (id={pid:#010x})");
    println!(
        "对比      = 工作树(按最后扫描) vs {}  ({at})",
        keys::short(&commit)
    );
    print_changes(&c);
    Ok(())
}

/// `status` 与 `diff` 共用。路径只列 `SAMPLE_MAX` 条：**计数是全的，列表是有界的**。
fn print_changes(c: &build::ChangeSet) {
    if c.is_empty() {
        println!("变更      = 无");
        return;
    }
    println!(
        "变更      = +{} ~{} -{}   入 {:.1} KiB / 删 {:.1} KiB",
        c.add,
        c.modify,
        c.delete,
        c.bytes as f64 / 1024.0,
        c.deleted_bytes as f64 / 1024.0
    );
    for ch in &c.sample {
        println!("  {} {:>10} B  {}", ch.what.sign(), ch.size, ch.path);
    }
    if c.sample_more > 0 {
        println!(
            "  …另有 {} 条未列出（列表上限 {}）",
            c.sample_more,
            build::SAMPLE_MAX
        );
    }
}
