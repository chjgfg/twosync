//! M0 批次：本地、不联网。
//!
//! init project add/list/use/remove/status scan commit mv status log diff show
//! ls-files restore checkout plan apply config doctor

use std::{format, fs};
use std::path::{Path, PathBuf};
use std::time::Instant;

use storage_engines::KvEngine;

use crate::allocstats;
use crate::cli::Cli;
use crate::cli::cmd::{ConfigCmd, Import, ProjectCmd};
use crate::cli::config::Config;
use crate::cli::registry::{self, Registry};
use crate::error::{Error, Result};
use crate::store::keys::{self, Hash, Kind, ProjectId};
use crate::store::{build, cas, checkout, history, object, restore, sessions, worktree};
use crate::utils::{self, db_dir, journal_dir, log_dir, objects_dir, projects_file, site_dir};

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
    let db = cli.store()?;
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
                    "{mark} {name:20} id={:#010x}  {}  对端={}",
                    keys::project_id(name),
                    p.path,
                    p.peer_path.as_deref().unwrap_or("(未配对)")
                );
            }
        }
        ProjectCmd::Use { name } => {
            reg.select(name)?;
            reg.save(&cli.root)?;
            println!("当前项目 = {name}");
        }
        ProjectCmd::Pair { name, peer_root } => {
            // 原样存对端路径（那是**对端**的写法），只把 Windows 的反斜杠留成它自己的样子：
            // 校验它等于替对端决定规矩，而对端可能是 Linux。空串要挡——空路径会让 push 拼出
            // `--root ""`，那是往一个谁都说不清的地方写。
            if peer_root.trim().is_empty() {
                return Err(Error::BadInput(
                    "对端站点根不能是空的；给对端 `twosync --root <目录>` 用的那个 <目录>".to_string(),
                ));
            }
            reg.pair(name, peer_root)?;
            reg.save(&cli.root)?;
            println!("已配对 {name} → 对端站点根 {peer_root}");
            println!(
                "注意      = 只配本端还不够：§9.5.3 要**两边都确认**。对端也得在那台机器上跑一次\
                 `twosync --root <对端根> project pair {name} <本端站点根>`"
            );
        }
        ProjectCmd::Unpair { name } => {
            let had = reg.unpair(name)?;
            reg.save(&cli.root)?;
            println!(
                "{}",
                if had {
                    format!("已取消 {name} 的配对（历史、CAS、工作树都没动；push 会先拒，直到重新 pair）")
                } else {
                    format!("{name} 本来就没配对")
                }
            );
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
                            "head      = {}  {}  {:<7} +{} ~{} -{} R{}  {}",
                            keys::short(h),
                            history::fmt_ts(c.ts),
                            c.peer,
                            c.stats.add,
                            c.stats.modify,
                            c.stats.delete,
                            c.stats.rename,
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
/// （句柄本身在 `Cli` 里缓存，所以"这里开一次、那里再开一次"已经是免费的。）
fn db_or_open(cli: &Cli) -> Result<std::sync::Arc<dyn storage_engines::KvEngine>> {
    cli.store()
}

/// 项目作用域：`--project` 显式指定 > `project use` 选中的那个。
/// 返回 (项目名, 项目绝对路径)。名字要单独拿出去，因为 `project_id` 是**从名字派生**的。
/// "你指哪个项目"这条判断只有一份（`m1::push` 也调它，别在别处再写一遍注册表查询）。
pub(crate) fn resolve_project(cli: &Cli) -> Result<(String, PathBuf)> {
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

pub fn scan(cli: &Cli, force: bool, batch: usize, only: Option<&str>) -> Result<()> {
    let (name, root) = resolve_project(cli)?;
    let db = cli.store()?;
    let pid = keys::project_id(&name);
    let wm = allocstats::watermark();
    // `--only <dir>` = 窄扫。范围必须打出来：同一句"walked = 12"在全量轮与窄扫轮里意思完全不同，
    // 而只有全量轮会推进活性线（`Z`），这直接决定下一句 `commit` 能不能报删除。
    let scope = match only {
        None => worktree::Scope::Full,
        Some(d) => worktree::Scope::Dir(d.to_string()),
    };
    let rep = worktree::scan_scoped(&*db, pid, &root, &scope, force, batch, &cli.io_bucket)?;
    let peak = wm.peak_growth();

    let secs = rep.elapsed_ms as f64 / 1000.0;
    println!("project   = {name}  (id={pid:#010x})");
    println!("root        = {}", root.display());
    println!("范围      = {}", scope.label());
    if !scope.is_full() {
        println!(
            "活性线    = **没推进**（窄扫只走一个目录，不能拿它当「本轮全体看见过」）⇒ \
             这一轮之后的 commit 报不出删除，删除要等下一次全量 scan"
        );
    }
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
            "注意：{n} 项读不了，已记进 deferred 名单（不是删除，检出会跳过她们）",
            n = rep.deferred
        );
        if !rep.deferred_sample.is_empty() {
            let more = rep.deferred - rep.deferred_sample.len();
            println!(
                "          {list}{more_txt}（全量看 `status`）",
                list = rep.deferred_sample.join(", "),
                more_txt = if more > 0 {
                    format!("…另 {more} 条")
                } else {
                    String::new()
                }
            );
        }
    }
    // 只有全量轮会收名单（窄扫没资格宣布别处的项好了），所以窄扫轮这一行不打。
    if rep.deferred_swept > 0 {
        println!(
            "deferred 名单收掉 {n} 行过期项（这一轮读得通了，或盘上已经没有她）",
            n = rep.deferred_swept
        );
    }
    Ok(())
}

/// 把 `W` 表的**最新一轮**快照固化成 commit（FR-4）：落块 → 建树 → 比对 → 移 head。
///
/// 这一条**不重新扫描**，只吃上一次 `scan` 的结果——所以刚改完文件没 scan 就 commit，
/// 提交的是上一次扫描看见的状态。中间文件又变了的话会撞 `StaleScan`，不会静默用新内容。
pub fn commit(cli: &Cli, msg: Option<&str>, allow_mass_delete: bool, import: Import) -> Result<()> {
    let (name, root) = resolve_project(cli)?;
    let db = cli.store()?;
    let pid = keys::project_id(&name);
    // 块仓是**站点级**的一份（挂在 `--root` 下），不是每项目一份：跨项目去重靠这个。
    let store = cas::Cas::new(&objects_dir(&cli.root), cli.block_size()?)?
        .with_fsync(!import.fast)
        .with_bucket(&cli.budget(import.rate_bytes_per_sec()));

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
    println!("tree      = {}", keys::hex(&b.tree));
    let c = &b.changes;
    println!(
        "changes     = +{} ~{} -{} R{}   入 {:.1} MiB / 删 {:.1} MiB",
        c.add,
        c.modify,
        c.delete,
        c.rename,
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
    // 盘上没有、历史里有的那几条：窄扫轮或"scan 之后被删"的场景，内容复用上一版。
    if b.reused_missing > 0 {
        println!(
            "复用        = {} 条文件已不在盘上、但内容早已在历史里 ⇒ 直接沿用那一版（下一次全量 scan 会把删除确认掉）",
            b.reused_missing
        );
    }
    // 在盘上但**读不了**的那几条（被占用/权限）：保持历史里那一版，整轮不许因为她们停下来。
    // 与上面那格分开报，因为处置不同——前者下一次全量扫描自动变成删除，后者会一直卡着等人放手。
    if b.kept_locked > 0 {
        println!(
            "deferred    = {} 条这一轮读不了（被占用/权限）⇒ 保持历史里那一版，其余照常提交；              名单逐条看 `status`（§7.2）",
            b.kept_locked
        );
    }
    // 参照侧清单的读法（一次前缀扫 or 退回点读）：这一行的数字就是"O(目录数) 次折被消掉"的凭据。
    println!("{}", b.manifests.line());
    // 这两枚旋钮各自放弃了点什么，必须打出来——不许静默生效（§14 Q11）。
    if import.fast {
        println!(
            "落盘        = --fast-import：**没有**等块 fsync。断电可能留下与清单不符的块；              读回会按 hash 报 CorruptBlock（不会静默给错数据），恢复办法是把这一版重新导入。"
        );
    }
    if import.max_mbps > 0.0 {
        println!(
            "落盘        = 写块仓限速 {:.1} MiB/s（平均速率，漏桶；不削瞬时突发）",
            import.max_mbps
        );
    }
    if b.entries == 0 && b.known > 0 {
        println!(
            "注意：本轮快照一条都没有而库里有 {n} 行 ⇒ 上一次 scan 可能中断过，先重跑 scan",
            n = b.known
        );
    }
    println!("elapsed     = {:.3} s", started.elapsed().as_secs_f64());
    // 与 scan 同一把尺：融合遍历之后内存只跟**单个目录**挂钩，所以这里的峰值应当在
    // 「最大单层目录 × 深度」这一档，与文件总数无关。打印是为了让那次权衡可核对。
    println!("peak heap   = {:.1} MiB", peak as f64 / 1048576.0);
    Ok(())
}

/// `log`：从 head 集合出发按时间倒序列出 commit。
///
/// 起点是**集合**不是单个 ref（§4：多 head 是正常态）。真正的 union/裁决是 M3 的活，
/// 这里只保证"两个 head 的历史都能列出来、共同祖先不重复"。
pub fn log(cli: &Cli, max_count: usize) -> Result<()> {
    let (name, _) = resolve_project(cli)?;
    let db = cli.store()?;
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
            "{}  {}  {:<7} +{:<5} ~{:<4} -{:<4} R{:<4} {}",
            keys::short(&n.hash),
            history::fmt_ts(n.commit.ts),
            n.commit.peer,
            s.add,
            s.modify,
            s.delete,
            s.rename,
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
    let db = cli.store()?;
    let pid = keys::project_id(&name);
    // 只有符号链接那一支会真去读字节（目标文本存在 CAS 里，不在引擎里）。
    let store = cas::Cas::new(&objects_dir(&cli.root), cli.block_size()?)?;
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
            "{rev} 指向 {} 个 commit ⇒ 落地必须人选一个（§7：union 之后不许猜）。\
             先看 `status` 的「碰撞」那一格：没有同路径冲突时随便挑一个 head 检出现都行；\
             有冲突就按 FR-11 那三条出路选。也可以直接给具体 commit。",
            many.len()
        ))),
    }
}

/// `restore`：从某个版本取一个路径回工作树（或取到别处）。
pub fn restore(cli: &Cli, at: &str, path: &str, to: Option<&Path>, force: bool) -> Result<()> {
    let (name, root) = resolve_project(cli)?;
    let db = cli.store()?;
    let pid = keys::project_id(&name);
    let store = cas::Cas::new(&objects_dir(&cli.root), cli.block_size()?)?;
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
        match (r.gate_checked, to) {
            (true, _) => "目标已存在，已确认被覆盖的那一版在历史里",
            // 门查的是工作树那一份；`--to` 的目标在别处，所以这里不是"没查"，是"查不着"。
            (false, Some(_)) => "目标是工作树外的路径，那道门管不到它（覆盖与否看 --force）",
            (false, None) => "目标不存在，不需要门",
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
    let db = cli.store()?;
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

/// `checkout`：把一个版本落到工作树，或（`--to`）导出到另一个目录。默认**只报不动手**。
#[allow(clippy::too_many_arguments)]
pub fn checkout(
    cli: &Cli,
    at: &str,
    prefix: Option<&str>,
    apply: bool,
    allow_mass_delete: bool,
    to: Option<&Path>,
    force: bool,
) -> Result<()> {
    let (name, root) = resolve_project(cli)?;
    let db = cli.store()?;
    let pid = keys::project_id(&name);
    let store = cas::Cas::new(&objects_dir(&cli.root), cli.block_size()?)?;
    let commit = one_commit(history::resolve_rev(&*db, pid, at)?, at)?;
    let started = Instant::now();

    if let Some(dest) = to {
        let r = checkout::export(
            &*db,
            pid,
            &store,
            &checkout::ExportOpts {
                target: &commit,
                prefix,
                dest,
                work_root: &root,
                force,
                apply,
            },
        )?;
        println!("project   = {name}  (id={pid:#010x})");
        println!("目标      = {}  ({at})", keys::hex(&commit));
        println!("导出到    = {}", dest.display());
        println!(
            "动作      = 写文件 {} / 建目录 {} / 链接 {} / 覆盖 {} / 跳过 {}   共 {:.1} KiB",
            r.files,
            r.dirs,
            r.links,
            r.overwrite,
            r.skipped,
            r.bytes as f64 / 1024.0
        );
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
        if r.skipped > 0 {
            // 这里不套 §7.3：那条门查的是工作树里那一份，而目标路径上的东西跟它不是一个文件。
            println!(
                "跳过的是目标目录里已有的路径。加 --force 会直接覆盖——那份内容不在本项目历史里，门也拦不住它。"
            );
        }
        println!(
            "{}",
            if r.applied {
                "已导出。工作树没动，head 也没动。"
            } else {
                "没有动任何东西：加 --apply 才真做。"
            }
        );
        println!("elapsed   = {:.3} s", started.elapsed().as_secs_f64());
        return Ok(());
    }

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
        "动作      = 建 {} / 建目录 {} / 覆盖 {} / 删 {} / 删目录 {} / 跳过 {}{}",
        r.create,
        r.mkdir,
        r.overwrite,
        r.delete,
        r.rmdir,
        r.skipped,
        // deferred 单列，不混进"跳过"：那一格是形状不合适（类型互换），这一格是磁盘上读不到。
        if r.deferred > 0 {
            format!(" / deferred 不动 {}", r.deferred)
        } else {
            String::new()
        }
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

/// `config`：读写站点级配置。真正的规则都在 `cli::config::Config` 里，这里只负责打印
/// "生效值 + 来源"——因为**没写**和**写了默认值**是两件事，只看文件看不出来。
pub fn config(cli: &Cli, cmd: &ConfigCmd) -> Result<()> {
    let mut cfg = Config::load(&cli.root)?;
    match cmd {
        ConfigCmd::Get { key } => {
            let keys: Vec<&str> = match key {
                Some(k) => vec![k.as_str()],
                None => Config::KEYS.to_vec(),
            };
            for k in keys {
                let (v, src) = cfg.effective(k)?;
                // 命令行 `--engine` 压过配置文件：不标出来的话，人会对着一个没生效的值做决定。
                let src = if k == "engine" && cli.engine.is_some() {
                    "命令行 --engine"
                } else {
                    src
                };
                println!("{k:<11} = {v:<12}  ({src})");
            }
        }
        ConfigCmd::Set { key, value } => {
            cfg.set(key, value)?;
            cfg.save(&cli.root)?;
            println!(
                "已写入 {key} = {value}  ({})",
                Config::path(&cli.root).display()
            );
            match key.as_str() {
                "engine" => println!(
                    "  注意：换引擎 = 换数据目录（.twosync/db/<engine>/），旧引擎里的数据不会自动带过来。`doctor` 看一眼再动手"
                ),
                "block_size" => println!(
                    "  只影响之后新建的 blob：每个 blob 清单里写着自己的块大小，旧 blob 照原样读"
                ),
                _ => {}
            }
        }
        ConfigCmd::Unset { key } => {
            let had = cfg.unset(key)?;
            cfg.save(&cli.root)?;
            if had {
                println!("已删掉 {key}，回到内置默认");
            } else {
                println!("{key} 本来就没写，仍是内置默认");
            }
        }
    }
    Ok(())
}

const MV_HINT: &str = "mv 这一条没有覆盖语义，先把它挪开或删掉（删之前想想 §7.3）";

/// `mv`：改个工作树里的路径名。**只动磁盘，不碰库**——改名要等下一次 `scan` + `commit`
/// 才进历史，那时按 FR-1 折成一次改名（同 hash 配对），账面是 `R`，不是"一删一增"。
///
/// 目标已存在一律拒（`LocalExists`）。这里没有 `--force`：`mv` 想覆盖的话，用户应该显式
/// 先 `restore`/`rm`，少一个静默丢数据的口子。
pub fn mv(cli: &Cli, src: &str, dst: &str) -> Result<()> {
    let (_, root) = resolve_project(cli)?;
    registry::check_rel_path(src).map_err(|m| Error::BadInput(format!("src {m}")))?;
    registry::check_rel_path(dst).map_err(|m| Error::BadInput(format!("dst {m}")))?;
    let from = work_path(&root, src);
    let to = work_path(&root, dst);
    if to.symlink_metadata().is_ok() {
        return Err(Error::LocalExists {
            path: to,
            hint: MV_HINT,
        });
    }
    let md = from
        .symlink_metadata()
        .map_err(|_| Error::BadInput(format!("{} 不存在（工作树 {src}）", from.display())))?;
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(&from, &to)?;
    println!(
        "已改名 {src} → {dst}  ({})",
        if md.is_dir() { "目录" } else { "文件" }
    );
    println!("  库里还没记：下一步 `scan` 然后 `commit`");
    println!("  下一次 commit 会按 FR-1 折成改名（R）；块是内容寻址的 ⇒ 磁盘和传输都不受影响");
    Ok(())
}

/// 仓库内 `/` 路径 → 工作树里的磁盘路径。分隔符换算只在这一处。
fn work_path(root: &Path, repo_path: &str) -> PathBuf {
    root.join(repo_path.replace('/', std::path::MAIN_SEPARATOR_STR))
}

/// `status`：工作树（按最后一次扫描）与 HEAD 的差。**走 `build::plan`，不落任何盘**。
pub fn status(cli: &Cli) -> Result<()> {
    let (name, root) = resolve_project(cli)?;
    let db = cli.store()?;
    let pid = keys::project_id(&name);
    // 与 scan/commit 同一把尺：融合遍历之后 NFR-1 的"内存与文件数无关"要能在**这条命令**
    // 上被读出来，否则那句承诺只是注释。耗时也一起给——比对是 O(目录数) 次点查，
    // 那一侧的代价必须同样可见。
    let started = Instant::now();
    let wm = allocstats::watermark();
    // 实时那一半，第一步：走一趟盘，收"最后扫描之后被动过的目录"和那些层的名单。
    // 用的就是 `scan` 那条 `walker(root)` ⇒ 排除语义天然一致，也不会给每个目录重造一次
    // gitignore 查找（那件事我实测过：2 001 次构造把 `status` 从 4.1 s 拖到 12.3 s）。
    let epoch = worktree::last_mark(&*db, pid).map_or(0, |m| m.epoch);
    let view = if epoch == 0 {
        worktree::DiskView::default()
    } else {
        worktree::view_since(&root, epoch)?
    };
    let suspects: Vec<String> = view.suspects.clone();
    // 第二步：`plan` 本来要走的那一趟 `W` 流**顺路**把可疑目录的名字收上来。
    // 不另开一趟扫描 —— 在这个引擎里"另开一趟"= 多几次全库折叠（§17 补记十）。
    let want: std::collections::HashSet<&str> = suspects.iter().map(String::as_str).collect();
    let mut db_names: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    let p = build::plan_observed(&*db, pid, &mut |path, _e| {
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        if want.contains(parent) {
            db_names
                .entry(parent.to_string())
                .or_default()
                .push(name.to_string());
        }
    })?;
    // 第三步：纯比较，不再碰盘也不碰库。
    let rec = worktree::reconcile(&view, &db_names);
    let peak = wm.peak_growth();

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
    // 上次会话那一格（FR-16：耗时/传输字节/对端 head 集合）。读的是 `S` 表最新一行。
    // **措辞是判据的一部分**：它是"上次会话时看到的那一面"，不是对端现状——对面后来又提交了几轮，
    // 这一行看不见（§15：不签名、不实时）。缩成"对端的 head"就会有人拿它当真相排产。
    match sessions::last(&*db, pid)? {
        None => println!("上次会话  = 从来没有同步过（库里没有 S 行）"),
        Some(r) => {
            println!(
                "上次会话  = {} · {} · 对端 {:?} · {} 块 / {:.2} MiB{}",
                history::fmt_ts(r.ts),
                r.dir.label(),
                r.peer,
                r.blocks,
                r.bytes as f64 / 1048576.0,
                if r.dir.is_send() {
                    format!("，跳过 {} 块（对面已有）", r.skipped)
                } else {
                    String::new()
                }
            );
            // 耗时只有**发起方**那一侧记得到：服务端的 `serve` 是从中途被叫起来的，
            // 它没有"整场会话"的起点。给 0 会被读成"这场只花了 0 秒"，所以宁可写"没记"。
            if r.ms > 0 {
                println!("          = 墙钟 {:.1} s（发起方计时）", r.ms as f64 / 1000.0);
            } else {
                println!("          = 耗时未记（这一侧是被拨的，没有整场起点）");
            }
            let ab = sessions::ahead_behind(&*db, pid, &heads, &r.their_heads)?;
            println!("          = {}", ab.line());
        }
    }
    print_changes(&p.changes);
    // 参照侧清单的读法：`status` 复用的就是 `plan` 那一趟遍历，所以同一个数（§11.4d）。
    println!("{}", p.manifests.line());
    // FR-16 的"碰撞列表"那一格（§7 第五行）。head 只有一个时 `collisions` 直接返回空，
    // **不付任何引擎调用**，所以这条打印在正常站点上是免费的。
    let cs = history::collisions(&*db, pid, &heads)?;
    if cs.is_empty() {
        if heads.len() > 1 {
            println!("碰撞      = {} 个 head 并集，没有同路径冲突（§7 第四行：多 head 是正常状态）", heads.len());
        }
    } else {
        println!(
            "碰撞      = **{} 条路径被两个 head 各改成不同东西**（v1 不猜，人选一条路径只能整体选一个 head）",
            cs.len()
        );
        for c in cs.iter().take(10) {
            println!(
                "          - {} （卷进 {} 个 head：{}）",
                c.path,
                c.heads.len(),
                c.heads.iter().map(keys::short).collect::<Vec<_>>().join(" ")
            );
        }
        if cs.len() > 10 {
            println!("          …另有 {} 条（全量要脚本读的话这里加 --json）", cs.len() - 10);
        }
        println!(
            "出路      = `checkout --at <某个 head>` 选一边（其余 head 仍留在库里，历史不丢）；             路径无交集才谈 `--keep-both` 的结构合并；有交集要内容级合并请交给真 git（§1）"
        );
    }
    // FR-16 / §7.2：deferred 名单。健康站点上是空表 ⇒ 这里什么都不打（只花一次前缀扫，
    // 与碰撞那一格同一口径：正常形状下不产生噪音）。
    let df = worktree::list_deferred(&*db, pid)?;
    if !df.is_empty() {
        println!(
            "deferred  = **{} 条路径现在读不了**（§7.2：不进删除判定，检出会跳过她们）",
            df.len()
        );
        for (p, r) in df.iter().take(10) {
            println!("          - {p} —— {}", r.reason);
        }
        if df.len() > 10 {
            println!("          …另有 {} 条", df.len() - 10);
        }
        println!("处理      = 让占用她的进程放手或修好权限，再 `scan` 一轮（读得通了这行自动消失）");
    }
    // 实时那一半：**只报存在性差异**。内容改了这里看不见——目录 mtime 不表达文件内容变化，
    // 那要重算 hash 才知道（与 git 的 untracked cache 同一个限制）。
    if epoch == 0 {
        println!("盘上对账  = 从来没有完整扫描过 ⇒ 没有活性线可比：先 twosync scan");
    } else if rec.suspect == 0 {
        println!(
            "盘上对账  = 没有目录在最后扫描之后被动过（比过 {} 层 mtime）⇒ 没有漏记的增删",
            view.dirs
        );
    } else {
        let cut = if rec.truncated {
            format!("（已达上限 {}，名单不全）", worktree::RECON_SUSPECT_MAX)
        } else {
            String::new()
        };
        println!(
            "盘上对账  = {} 个目录在最后扫描之后被动过{cut}：未扫的新条目 {} / 盘上已没了 {} / 读不了 {}",
            rec.suspect, rec.only_disk, rec.only_w, rec.unreadable
        );
        for line in &rec.sample {
            println!("  {line}");
        }
        if rec.sample_more > 0 {
            println!("  …另有 {} 条未列出", rec.sample_more);
        }
        // 这一句是这条命令全部的诚实所在：差异**没有**进账，`W` 才是记账真值。
        println!(
            "提醒      = 上面这些还没进账。要让它们成为变更：先 `scan`，再看 `status`/`commit`。"
        );
    }
    // §9.6.2 明写 `status` 必须报这三项。NFR-8 说 `health()` 是 O(物理行数) 不许进热路径——
    // `status` 是人敲的命令，与 `doctor` 同类；M2 的 agent/daemon 才是热路径，它自己走缓存。
    let h = db.health();
    println!(
        "引擎      = {}  active_txns={}  max_chain={}  rows_above_one={}",
        db.name(),
        h.active_txns,
        h.max_chain,
        h.rows_above_one
    );
    // NFR-5 的磁盘账：配额够不够用，取决于这三个数加不加起来看得见。
    let cas_size = utils::dir_size(&objects_dir(&cli.root));
    let engine_size = utils::dir_size(&db_dir(&cli.root));
    let work_size = utils::dir_size(&root);
    println!(
        "磁盘      = 工作树 {:.1} MiB({} 文件) / CAS {:.1} MiB({} 块) / 引擎 {:.1} MiB",
        work_size.bytes as f64 / 1048576.0,
        work_size.files,
        cas_size.bytes as f64 / 1048576.0,
        cas_size.files,
        engine_size.bytes as f64 / 1048576.0
    );
    if work_size.bytes > 0 {
        // "几个全量副本"就是 CAS/工作树这个比值：它直接回答"留这么多历史吃得消吗"。
        let ratio = cas_size.bytes as f64 / work_size.bytes as f64;
        println!("历史      = CAS / 工作树 = {ratio:.2} ⇒ 块仓里约等于 {ratio:.1} 份全量数据");
    }
    // 这一项故意**不**给数：它需要遍历全部 `B` 清单把"声明字节"加起来，那是 `gc`（M1）的活。
    // 宁可说一句"要 gc 才知道"，也不要在这里塞一个把 `status` 变成分钟级的隐藏循环。
    println!(
        "去重      = 跨 project 省下多少 = Σ(blob 声明字节) − CAS 实际字节；需要全量清单 ⇒ 归 `gc`（M1）"
    );
    println!(
        "耗时      = {:.3} s   peak heap = {:.1} MiB",
        started.elapsed().as_secs_f64(),
        peak as f64 / 1048576.0
    );
    println!("注意：与 HEAD 的差按「最后一次扫描」算；「盘上对账」那一段才是此刻盘上的样子。");
    Ok(())
}

/// `version`：版本与**生效**配置。断言走 [`version_lines`]（打印出来的东西测不到）。
pub fn version(cli: &Cli) -> Result<()> {
    for line in version_lines(cli)? {
        println!("{line}");
    }
    Ok(())
}

fn version_lines(cli: &Cli) -> Result<Vec<String>> {
    let cfg = Config::load(&cli.root)?;
    let (engine, esrc) = cfg.effective("engine")?;
    // 命令行 `--engine` 压过配置文件（优先级只在 `Cli::store()` 里实现一次，这里报的是同一口径）。
    let esrc = if cli.engine.is_some() {
        "命令行 --engine"
    } else {
        esrc
    };
    let (block, bsrc) = cfg.effective("block_size")?;
    let mut out = vec![
        format!("twosync   = {}", env!("CARGO_PKG_VERSION")),
        format!("站点根    = {}", cli.root.display()),
        format!("引擎      = {engine:<10} ({esrc})"),
        format!("block_size = {block:<10} ({bsrc})"),
        "存储基座  = storage-engines".to_string(),
        "            版本以 Cargo.lock 为准（`cargo tree -p storage-engines`）；这里不硬编一个会过期的数字"
            .to_string(),
    ];
    if let Ok(exe) = std::env::current_exe() {
        let n = fs::metadata(&exe).map(|m| m.len()).unwrap_or(0);
        // NFR-9 的判据就在这行：单二进制 ≤ 15 MB。
        out.push(format!(
            "二进制    = {}  ({:.1} MiB)",
            exe.display(),
            n as f64 / 1048576.0
        ));
    }
    Ok(out)
}

/// `plan`：FR-7 的只读那一半。**一个字节都不写**，`apply` 才执行。
///
/// 它比 `status` 多说的两件，都是"只有要提交才需要知道"的：**FR-10 熔断预演**
/// （用与 `build` 完全同源的三个数 ⇒ 这里判"会拒"，commit 就一定拒）和 head 会怎么动。
/// union / collision 是 M3 的活：现在没有对端，报 `NotYet` 之前先把"无对象可算"说清楚。
pub fn plan(cli: &Cli) -> Result<()> {
    let (name, _) = resolve_project(cli)?;
    let db = cli.store()?;
    let pid = keys::project_id(&name);
    let started = Instant::now();
    let wm = allocstats::watermark();
    let p = build::plan(&*db, pid)?;
    let peak = wm.peak_growth();
    let cs = &p.changes;

    println!("project     = {name}  (id={pid:#010x})");
    println!("预演      = 若现在 commit（按最后一次扫描，不复核磁盘）");
    print_changes(cs);
    println!(
        "tree        = {}  {}",
        keys::short(&p.tree),
        if cs.is_empty() {
            "与 HEAD 相同 ⇒ 不会产生 commit"
        } else {
            "与 HEAD 不同 ⇒ commit +1、head 前移"
        }
    );
    // 熔断预演：`known` 与 build 用的是同一个数（库里 W 行全量，不是本轮活行）。
    match build::check_mass_delete(cs.delete, cs.deleted_bytes, p.known, false) {
        Ok(()) => println!("熔断      = 通过（不必 --allow-mass-delete）"),
        Err(e) => println!("熔断      = 会被拒：{e}"),
    }
    println!("union     = 现在只有本机一个 head：union 判定与 collision 列表要等 M1/M3 才有对象");
    // 参照侧清单是怎么读的（整段装载 or 退回点读）——换装载形状省下的折数必须能核对（§11.4d）。
    println!("{}", p.manifests.line());
    println!(
        "耗时        = {:.3} s   peak heap = {:.1} MiB",
        started.elapsed().as_secs_f64(),
        peak as f64 / 1048576.0
    );
    println!("注意      = plan 只读；要真做是 `apply`（= `commit`）。磁盘上更晚的改动先 `scan`。");
    Ok(())
}

/// `apply`：FR-7 的执行那一半。**与 `commit` 同一份实现**，不是第二份代码——
/// "怎么提交"这件事有两份实现迟早会在其中一份里漏掉一道闸（本项目为这事记过不止一条）。
pub fn apply(cli: &Cli, msg: Option<&str>, allow_mass_delete: bool, import: Import) -> Result<()> {
    println!("apply     = commit 的执行侧（FR-7 的命名对），走的是同一个函数");
    commit(cli, msg, allow_mass_delete, import)
}

/// `diff --at REV`：同一份变更表，只是比较对象换成任意一个版本。
pub fn diff(cli: &Cli, at: &str) -> Result<()> {
    let (name, _) = resolve_project(cli)?;
    let db = cli.store()?;
    let pid = keys::project_id(&name);
    let commit = one_commit(history::resolve_rev(&*db, pid, at)?, at)?;
    let want = object::read_commit(&*db, pid, &commit)?;
    // 与 `status` 同一趟融合遍历，只是参照版本从 HEAD 换成 `at`。工作树那一侧的树从来没
    // 进过库，所以"跟任意版本比"能用的就是这棵树——融合之前它还得先被 `plan` 攒进内存。
    let p = build::compare(&*db, pid, Some(want.tree))?;

    println!("project   = {name}  (id={pid:#010x})");
    println!(
        "对比      = 工作树(按最后扫描) vs {}  ({at})",
        keys::short(&commit)
    );
    print_changes(&p.changes);
    Ok(())
}

/// `status` 与 `diff` 共用。路径只列 `SAMPLE_MAX` 条：**计数是全的，列表是有界的**。
fn print_changes(c: &build::ChangeSet) {
    if c.is_empty() {
        println!("变更      = 无");
        return;
    }
    println!(
        "变更      = +{} ~{} -{} R{}   入 {:.1} KiB / 删 {:.1} KiB",
        c.add,
        c.modify,
        c.delete,
        c.rename,
        c.bytes as f64 / 1024.0,
        c.deleted_bytes as f64 / 1024.0
    );
    // 样本按路径排完再列：融合遍历是**自底向上**结算的，深层目录的变更会比同层的浅路径
    // 先出来（`d007/sub/deep.bin` 排在 `d007/f0.bin` 前面），人读着跳。
    // 排序只发生在这 30 条的副本上——`ChangeSet` 本身保持遍历序，`checkout` 依赖它。
    let mut sample = c.sample.clone();
    sample.sort_by(|a, b| a.path.cmp(&b.path));
    for ch in &sample {
        println!("  {} {:>10} B  {}", ch.what.sign(), ch.size, ch.path);
    }
    if c.sample_more > 0 {
        println!(
            "  …另有 {} 条未列出（列表上限 {}）",
            c.sample_more,
            build::SAMPLE_MAX
        );
    }
    // 改名单独列（FR-1）：它是"同一份内容换了路径"，不复制数据也不占传输，所以既不算 +也不算 -。
    for (old, new) in &c.renames {
        println!("  R {old} → {new}");
    }
    if c.rename_more > 0 {
        println!("  …另有 {} 对改名未列出", c.rename_more);
    }
    if c.rename_gave_up {
        println!(
            "注意      = 变更条数超过配对上限 {}，改名只报了部分（改名是账面与传输优化，不影响正确性）",
            build::RENAME_PAIR_MAX
        );
    }
}
