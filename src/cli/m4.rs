//! M4 批次：图上重写与重放。branch / tag / reset / revert / stash / cherry-pick / blame / bisect / commit --amend
//!
//! 已落第一条：**`tag`**。实现一条命令就在这里加 `pub fn <命令>(cli: &Cli, ...) -> Result<()>`，
//! 并在 `cmd.rs` 的 `Cmd` 与 `main.rs` 的 match 各加一处（**只这两处**，别的都是绕路）。
//!
//! # 为什么 tag 是这一批的第一条
//!
//! §9.3 的保留集写的是"所有 head 的可达闭包 ∪ 按 FR-9 选出的更老 commit ∪ **显式 `pinned`**"。
//! 前两项早就有实现，第三项一直没地方落——`gc` 那句 `build::all_ref_heads(db, pid, None)`
//! 取的是**全部 ref**，所以"把一个 commit 挂在某个 ref 上"这件事本身就是 pin 的机制。
//! tag 就是那个 ref 里语义最窄、最不抢方向盘的一种：它不跟着 `commit` 移动，
//! 于是不会与 head 集合那套 union / FR-10 熔断逻辑互相干扰。

use std::format;

use crate::cli::cmd::TagCmd;
use crate::cli::{m0, Cli};
use crate::error::{Error, Result};
use crate::store::{build, cas, checkout, history, keys, object};
use crate::utils::objects_dir;

pub fn tag(cli: &Cli, cmd: &TagCmd) -> Result<()> {
    let (name, _) = m0::resolve_project(cli)?;
    let db = cli.store()?;
    let pid = keys::project_id(&name);
    match cmd {
        TagCmd::List => list(&*db, pid, &name),
        TagCmd::Add { name: t, at } => add(&*db, pid, &name, t, at.as_deref()),
        TagCmd::Rm { name: t } => rm(&*db, pid, &name, t),
    }
}

/// `reset`：把 head 集合挪到一个 commit，`--apply` 时把工作树落成那个样子。
///
/// **顺序是这条命令的全部设计**：先做检出、最后才挪 ref。FR-13 写的就是
/// "head 集合更新是最后一步"——反过来做，检出被 FR-10 熔断挡下的那一次就会留下
/// "head 已经挪了、工作树还是旧的、而报出来像失败"的中间态；现在这个顺序下失败意味着
/// **什么都没动**，人可以直接改参数重跑。
pub fn reset(cli: &Cli, rev: &str, apply: bool, allow_mass_delete: bool) -> Result<()> {
    let (project, root) = m0::resolve_project(cli)?;
    let db = cli.store()?;
    let pid = keys::project_id(&project);
    // 与 `checkout`/`restore` 同一道闸：多解就报错，不许挑第一个。
    let target = m0::one_commit(history::resolve_rev(&*db, pid, rev)?, rev)?;
    // 证它真在库里。挪一个指不到的 commit = 把整个项目的历史起点弄丢。
    let commit = object::read_commit(&*db, pid, &target)?;
    let old = build::ref_heads(&*db, pid, build::MAIN)?;
    let lost: Vec<String> = old
        .iter()
        .filter(|h| **h != target)
        .map(|h| keys::short(h).to_string())
        .collect();

    println!("project   = {project}  (id={pid:#010x})");
    println!("工作树    = {}", root.display());
    println!(
        "现在      = {} 个 head：{}",
        old.len(),
        if old.is_empty() {
            "（一个都没有）".to_string()
        } else {
            old.iter().map(keys::short).collect::<Vec<_>>().join(" ")
        }
    );
    println!(
        "挪到      = {}  ({})  {}  {}",
        keys::hex(&target),
        rev,
        history::fmt_ts(commit.ts),
        commit.msg
    );
    if !lost.is_empty() {
        println!("被丢的 head = {} 个：{}", lost.len(), lost.join(" "));
        println!(
            "危险      = 它们要是没有别的 ref 挂着，下一次 `gc --sweep --apply` 就可以把那段历史\
             当垃圾收走（§9.3：不可达 = 可删）。要保住就先 `tag add <名字> --at <hash>`。"
        );
    }
    if !apply {
        println!("模式      = 只报（FR-7）。加 --apply 才会动 head 与工作树，这一趟一个字没改");
        return Ok(());
    }

    // 先落地（含 §7.3 逐文件门与 FR-10 熔断），成功后才挪 ref。
    let store = cas::Cas::new(&objects_dir(&cli.root), cli.block_size()?)?;
    let rep = checkout::checkout(
        &*db,
        pid,
        &store,
        &root,
        &target,
        None,
        checkout::Opts {
            apply: true,
            allow_mass_delete,
        },
    )?;
    build::set_ref(&*db, pid, build::MAIN, &[target])?;
    println!(
        "动作      = 建 {} / 建目录 {} / 覆盖 {} / 删 {} / 删目录 {} / 跳过 {}{}",
        rep.create,
        rep.mkdir,
        rep.overwrite,
        rep.delete,
        rep.rmdir,
        rep.skipped,
        if rep.deferred > 0 {
            format!(" / deferred 不动 {}", rep.deferred)
        } else {
            String::new()
        }
    );
    println!(
        "head      = 已挪到 {}（现在 {} 个 head）",
        keys::short(&target),
        build::ref_heads(&*db, pid, build::MAIN)?.len()
    );
    Ok(())
}

/// `tag list`：每个名字一行，带它指向的 commit、那一刻的时间与说明。
///
/// `main` **不列**：它是 head 集合（`status`/`log` 那两格已经在报了），列进来只会让人以为
/// "tag 也有一个 main"。删 tag 时那道拒绝用的是同一批保留名，见 [`check_tag_name`]。
fn list(db: &dyn storage_engines::KvEngine, pid: keys::ProjectId, project: &str) -> Result<()> {
    println!("project   = {project}  (id={pid:#010x})");
    let mut n = 0usize;
    for (name, heads) in build::all_named_refs(db, pid)? {
        if name == build::MAIN {
            continue;
        }
        n += 1;
        for h in heads {
            // 读不出来的 commit 不是"没有 tag"：那一行照样要印，并说明读不回什么。
            match object::read_commit(db, pid, &h) {
                Ok(c) => println!(
                    "  {name}  {}  {}  {}",
                    keys::hex(&h),
                    history::fmt_ts(c.ts),
                    c.msg
                ),
                Err(e) => println!(
                    "  {name}  {}  （commit 读不回来：{e}）",
                    keys::hex(&h)
                ),
            }
        }
    }
    if n == 0 {
        println!("  （没有 tag。打一个：twosync tag add <名字> [--at <rev>]）");
    }
    Ok(())
}

fn add(
    db: &dyn storage_engines::KvEngine,
    pid: keys::ProjectId,
    project: &str,
    tag: &str,
    at: Option<&str>,
) -> Result<()> {
    check_tag_name(tag)?;
    let target = match at {
        Some(rev) => {
            let mut hs = history::resolve_rev(db, pid, rev)?;
            // 一个名字只能指一个 commit。多解就报错，**不许挑第一个**（那是猜）。
            if hs.len() != 1 {
                return Err(Error::BadInput(format!(
                    "`{rev}` 指向 {} 个 commit，一个 tag 只能钉住一个。请写清楚（`{rev}~0` 或用 hash 前缀）",
                    hs.len()
                )));
            }
            hs.remove(0)
        }
        None => {
            let heads = build::ref_heads(db, pid, build::MAIN)?;
            if heads.len() != 1 {
                return Err(Error::BadInput(format!(
                    "当前项目有 {} 个 head（多 head 是正常状态，§7 第四行），省略 `--at` 就等于替人选一个 ⇒ 请写 `--at <rev>`",
                    heads.len()
                )));
            }
            heads[0]
        }
    };
    // 先证它真在库里：`--at` 给 hash 前缀时 `resolve_rev` 已经查过，但给 `HEAD~3` 那种
    // 走的是亲代链，末尾那个 commit 若缺行就只有这儿能发现。挂一个指不到东西的名字 = 空头支票。
    let commit = object::read_commit(db, pid, &target)?;
    let old = build::ref_heads(db, pid, tag)?;
    if old.contains(&target) {
        println!("project   = {project}  (id={pid:#010x})");
        println!("tag {tag}  = 已经指着 {}，什么都没改", keys::short(&target));
        return Ok(());
    }
    if !old.is_empty() {
        return Err(Error::BadInput(format!(
            "tag {tag:?} 已经指着别的东西（{}）。要挪位置先 `tag rm {tag}` —— 故意不做 `--force`：\
             pin 的意义就是「不会被人一敲就松开」"
                ,
            old.iter().map(keys::short).collect::<Vec<_>>().join(" ")
        )));
    }
    build::set_ref(db, pid, tag, &[target])?;
    println!("project   = {project}  (id={pid:#010x})");
    println!("tag {tag}   = {}  ({})", keys::hex(&target), history::fmt_ts(commit.ts));
    println!("说明      = {}", commit.msg);
    // 这一句是这条命令的全部意义所在：不说清"它从此保住了谁"，pin 就只是一个字符串。
    println!(
        "效果      = 这个 commit 现在被 ref 担保 ⇒ `gc --sweep` 的保留集里它跑不掉（§9.3）。\
         松开它要 `tag rm {tag}`，之后回收才可能把它当垃圾"
    );
    Ok(())
}

fn rm(
    db: &dyn storage_engines::KvEngine,
    pid: keys::ProjectId,
    project: &str,
    tag: &str,
) -> Result<()> {
    check_tag_name(tag)?;
    let old = build::ref_heads(db, pid, tag)?;
    if old.is_empty() {
        return Err(Error::BadInput(format!("没有 tag {tag:?}（`tag list` 看有什么）")));
    }
    build::set_ref(db, pid, tag, &[])?;
    println!("project   = {project}  (id={pid:#010x})");
    println!(
        "删掉 tag  = {tag}（原来指着 {}）",
        old.iter().map(keys::short).collect::<Vec<_>>().join(" ")
    );
    println!(
        "注意      = 这些 commit 现在只由别的 ref 担保了。别的一个都没挂 ⇒ 下一次 `gc --sweep --apply` \
         就可以把它们当垃圾收走（§9.3）"
    );
    Ok(())
}

/// tag 的名字规矩。**每一条都是为了一句"不许猜"**，所以宁缺毋滥：
///
/// - `main` / `head`（不分大小写）是保留名：那两个格子由 `commit`/会话的 union 逻辑管，
///   被一个静态名字占住就会出现"删掉 tag 之后 head 集合少了一半"这种事。
/// - 长得像 hash 前缀（连续 7 个以上十六进制字符）的不收：`rev_base` 里命名引用排在 hash
///   之前，名字与 hash 撞脸时**解析顺序就成了裁决**，而那是人看不见的。
///   （见 `store::history::rev_base` 那段注释。）
/// - 控制字符 / 空白 / 开头的 `-`：`-` 会被下一个人抄成选项，空白会让 `--at` 那种写法分词分错。
fn check_tag_name(tag: &str) -> Result<()> {
    if tag.is_empty() {
        return Err(Error::BadInput("tag 名字不能是空的".to_string()));
    }
    if tag.len() > 128 {
        return Err(Error::BadInput(format!(
            "tag 名字 {} 个字符，太长（上限 128）",
            tag.len()
        )));
    }
    if tag.eq_ignore_ascii_case("head") || tag == build::MAIN {
        return Err(Error::BadInput(format!(
            "{tag:?} 是保留名：那两个 head 集合由 `commit` 与会话的 union 逻辑管，不能挂成一个不会动的名字"
        )));
    }
    if tag.starts_with('-') {
        return Err(Error::BadInput(format!(
            "tag 名字 {tag:?} 以 `-` 开头：抄进命令行时会被当成选项解析"
        )));
    }
    if tag.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(Error::BadInput(format!(
            "tag 名字 {tag:?} 里有空白或控制字符：`--at`/脚本读它都会分词分错"
        )));
    }
    let hexy = tag.chars().all(|c| c.is_ascii_hexdigit());
    if hexy && tag.len() >= 7 {
        return Err(Error::BadInput(format!(
            "tag 名字 {tag:?} 会长得像 commit hash 前缀 ⇒ 解析时两者必然撞脸，换一个字。这是故意的：\
             撞脸时谁先查谁赢，而那是人看不见的裁决"
        )));
    }
    Ok(())
}
