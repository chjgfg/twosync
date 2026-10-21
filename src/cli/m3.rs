//! M3 批次：双向同步与多 head 的裁决面。
//!
//! 已落 `merge` —— §7.1 那三条出路的命令面（原来只有半条：`checkout --at <head>` 能选检出目标，
//! 但 head 集合还是两个，下一次 `commit` 又得重挑）。三条都在这儿：
//!
//! | 出路 | 旗标 | 做什么 |
//! |---|---|---|
//! | 结构合并 | `--keep-both` | 改动路径**无交集**时生成一个两亲 commit，head 收成它（§1：不做内容级三方合并） |
//! | 选本地 | `--take-local` | 认出一个"本站自己写的" head，走 `reset` 那两步（落地 + 挪 head） |
//! | 选对端 | `--take-remote` | 同上，取"某场会话收进来并落到 head 上"的那一个 |
//!
//! 三条共同的规矩：**认不出来或认出多个就拒绝**，不许挑一个（§7"union 之后不许猜"）。
//! 来源这件事目前是**从会话账 `S` 倒推**的（会随历史变旧），所以 `--take-*` 只在答案唯一时才动；
//! 要把来源变成永久事实得往 `H` 的值里写东西 —— 那条记在 §14 Q18，等拍板。

use std::collections::BTreeSet;
use std::format;

use crate::cli::{m0, m4, Cli};
use crate::error::{Error, Result};
use crate::store::{build, history, keys, object, sessions, worktree};

/// `twosync merge [出路旗标] [--apply]`。
pub fn merge(
    cli: &Cli,
    keep_both: bool,
    take_local: bool,
    take_remote: bool,
    apply: bool,
    allow_mass_delete: bool,
) -> Result<()> {
    let chosen = [keep_both, take_local, take_remote].iter().filter(|x| **x).count();
    if chosen > 1 {
        return Err(Error::BadInput(
            "`--keep-both` / `--take-local` / `--take-remote` 是三选一，不是叠加：\
             合并与选边是两个相反的裁决"
                .to_string(),
        ));
    }

    let (project, _) = m0::resolve_project(cli)?;
    let db = cli.store()?;
    let pid = keys::project_id(&project);
    let heads = build::ref_heads(&*db, pid, build::MAIN)?;

    println!("project   = {project}  (id={pid:#010x})");
    println!(
        "head      = {} 个：{}",
        heads.len(),
        heads.iter().map(keys::short).collect::<Vec<_>>().join(" ")
    );

    if heads.len() < 2 {
        if chosen == 0 {
            println!("裁决      = 没有要裁的：head 集合只有一个（或空）。union 之后才会出现两个");
            return Ok(());
        }
        return Err(Error::BadInput(
            "head 集合里没有两个以上 commit ⇒ 没有'另一边'可取。这一条不该在这里动".to_string(),
        ));
    }

    let cs = history::collisions(&*db, pid, &heads)?;
    if cs.is_empty() {
        println!("碰撞      = 0 条（各 head 动的路径没有交集 ⇒ 可以结构合并）");
    } else {
        println!(
            "碰撞      = {} 条路径两边各改成不同东西（这几条 §7.1 明令不许机器裁）",
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
            println!("          …另有 {} 条", cs.len() - 10);
        }
    }

    match chosen {
        0 => {
            println!("出路      = §7.1 三条，选一条：");
            println!("             --keep-both    路径无交集时结构合并成一个两亲 commit（不做内容合并，§1）");
            println!("             --take-local   取本站自己写的那一个 head（落地 + 挪 head 两步）");
            println!("             --take-remote  取那场会话收进来的那一个 head");
            println!("             也可以直接点名：`reset <commit>`（更明确，不依赖来源判断）");
            println!("模式      = 只报（不带 --apply 一个字节都不写）");
            return Ok(());
        }
        _ if take_local || take_remote => {
            let pick = pick_side(&*db, pid, &heads, take_remote)?;
            println!(
                "选边      = 取{}那一边：{}",
                if take_remote { "对端" } else { "本地" },
                keys::hex(&pick)
            );
            // 复用 `reset`：它已经管好了顺序（先检出、最后挪 ref）与代价提示（被丢的 head 会被 gc 收走）。
            return m4::reset(cli, &keys::hex(&pick), apply, allow_mass_delete);
        }
        _ => {}
    }

    // ── --keep-both：结构合并 ──
    if heads.len() != 2 {
        return Err(Error::BadInput(format!(
            "{} 个 head 的结构合并要先定祖先怎么折叠（v1 是 1+1 台，§2.3）。现在只有两个 head 才合：\
             先用 `reset <hash>` 或 tag 把场面收成两个",
            heads.len()
        )));
    }
    let (h0, h1) = (heads[0], heads[1]);
    let c0 = object::read_commit(&*db, pid, &h0)?;
    let c1 = object::read_commit(&*db, pid, &h1)?;
    let index = object::parent_index(&*db, pid)?;
    let bases = history::merge_base_trees(&*db, pid, &index, &h0, &h1)?;
    println!(
        "共同祖先  = {}",
        if bases.is_empty() {
            "没有（两台各自从空开始 ⇒ 只并集，任何同路径不同内容都算冲突）".to_string()
        } else {
            format!(
                "{} 个：{}",
                bases.len(),
                bases.iter().map(keys::short).collect::<Vec<_>>().join(" ")
            )
        }
    );
    // 只报那一趟连**清单行**都不写（`Emit::Report` 只算 hash）：这句话命令自己当着人说过，
    // 所以它得有实现而不只是注释（FR-7）。
    let merged = history::merge_trees(
        &*db,
        pid,
        &c0.tree,
        &c1.tree,
        &bases,
        if apply {
            history::Emit::Store
        } else {
            history::Emit::Report
        },
    )?;
    let tree = match merged {
        history::Merge::Conflict { paths } => {
            println!(
                "合不了    = {} 条路径两边各改成不同东西（§7.1：有交集只能人选，或交给真 git）",
                paths.len()
            );
            for p in paths.iter().take(10) {
                println!("          - {p}");
            }
            return Err(Error::BadInput(format!(
                "--keep-both 被 {} 条同路径冲突挡住 ⇒ 换 --take-local / --take-remote / reset <hash>",
                paths.len()
            )));
        }
        history::Merge::Done { tree } => tree,
    };

    let msg = format!("merge --keep-both: {} + {}", keys::short(&h0), keys::short(&h1));
    if !apply {
        println!(
            "将生成    = 一个两亲 commit：tree {}，亲 {}",
            keys::short(&tree),
            [h0, h1].iter().map(keys::short).collect::<Vec<_>>().join(" + ")
        );
        println!("             （head 会从 2 个收成 1 个 = 这个合并；两个 parent 都留在库里）");
        println!("模式      = 只报。加 --apply 才写真东西");
        return Ok(());
    }
    let commit = object::put_commit(
        &*db,
        pid,
        &object::Commit {
            parents: vec![h0, h1],
            tree,
            peer: m0::PEER_LOCAL.to_string(),
            ts: worktree::now_ns(),
            msg,
            // 合并 commit 的 `stats` 是**空的**：它的变更都在两个 parent 各自那一格里。
            // 往这里填并集数字会让人以为"这些改动是这一次产生的"，那是双记。
            stats: object::Stats {
                add: 0,
                modify: 0,
                delete: 0,
                rename: 0,
                bytes: 0,
            },
        },
    )?;
    build::set_ref(&*db, pid, build::MAIN, &[commit])?;
    println!(
        "合并      = {}  tree {}",
        keys::hex(&commit),
        keys::short(&tree)
    );
    println!(
        "head      = 收成 1 个（{list} 都还在库里，`reset`/`log` 都追得回来）",
        list = [h0, h1].iter().map(keys::short).collect::<Vec<_>>().join(" + ")
    );
    println!(
        "工作树    = 没动。要落到这一版就 `checkout --at {}`（head 已经是一个了，那道闸开了）",
        keys::short(&commit)
    );
    Ok(())
}

/// 从 head 集合里认出"本地那一边"或"对端那一边"。**只有答案唯一才返回**，否则报错。
///
/// 依据是会话账 `S`：本端在收（`pull` / `agent` 收）的那些场里 `accepted` 落过的 commit ⇒ 对端来的。
/// 两个都算对端来的（比如互相推过）⇒ 这里就必须拒绝：那条记录说的是"曾经收过"，
/// 不是"这个 head 现在属于谁"。
fn pick_side(
    db: &dyn storage_engines::KvEngine,
    pid: keys::ProjectId,
    heads: &[keys::Hash],
    want_remote: bool,
) -> Result<keys::Hash> {
    let remote: BTreeSet<keys::Hash> = sessions::accepted_here(db, pid)?;
    let mut cands: Vec<keys::Hash> = heads
        .iter()
        .filter(|h| want_remote == remote.contains(*h))
        .copied()
        .collect();
    if cands.len() == 1 {
        return Ok(cands.remove(0));
    }
    let which = if want_remote { "对端" } else { "本地" };
    if cands.is_empty() {
        return Err(Error::BadInput(format!(
            "认不出哪个是{which}那一边：会话账里 {} 个 head 的归属都是'{which}'。要用就 `reset <hash>` 点名（§14 Q18 记着怎么把它变成永久事实）",
            heads.len()
        )));
    }
    Err(Error::BadInput(format!(
        "{which}那一边有 {} 个候选（{}）⇒ 不挑。用 `reset <hash>` 点名，或先把另一边 tag 住再裁",
        cands.len(),
        cands.iter().map(keys::short).collect::<Vec<_>>().join(" ")
    )))
}
