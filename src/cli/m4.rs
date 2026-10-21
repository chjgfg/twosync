//! M4 批次：图上重写与重放。branch / tag / reset / revert / stash / cherry-pick / blame / bisect / commit --amend
//!
//! 已落三条：**`tag`**（钉住一个 commit）、**`reset`**（挪 head 集合）、**`branch`**（可移动的本地名字）。
//! 实现一条命令就在这里加 `pub fn <命令>(cli: &Cli, ...) -> Result<()>`，
//! 并在 `cmd.rs` 的 `Cmd` 与 `main.rs` 的 match 各加一处（**只这两处**，别的都是绕路）。
//!
//! # 为什么 tag 是这一批的第一条
//!
//! §9.3 的保留集写的是"所有 head 的可达闭包 ∪ 按 FR-9 选出的更老 commit ∪ **显式 `pinned`**"。
//! 前两项早就有实现，第三项一直没地方落——`gc` 那句 `build::all_ref_heads(db, pid, None)`
//! 取的是**全部 ref**，所以"把一个 commit 挂在某个 ref 上"这件事本身就是 pin 的机制。
//! tag 就是那个 ref 里语义最窄、最不抢方向盘的一种：它不跟着 `commit` 移动，
//! 于是不会与 head 集合那套 union / FR-10 熔断逻辑互相干扰。
//!
//! # branch 与 tag 是同一格里的两种脾气
//!
//! 机制一个字没加：都是一个 ref 指一个 commit（§9.2 的 `H|p|名|0x00|hash`），差别只在**允不允许挪**
//! ——tag 是钉子（`tag add` 撞上已有名字直接拒绝，连 `--force` 都不给），branch 是活口（`branch move`
//! 是它的正职）。分开存是因为 `tag list` 与 `branch list` 必须各自列对：分支的名字住在
//! [`build::BRANCH_PREFIX`] 那一格，`rev_base` 两种拼法都试，所以 `reset 我的分支` 直接能用。
//!
//! **没有"当前分支"，这是故意的**：`commit` 只移 `main`，同步只看 `main`（§5 那句"v1 不参与同步"）。
//! 要做"当前分支"就得让 `commit`/`push`/`pull`/`status`/`gc` 五处都改读那个游标，
//! 而 §2.3 的 1+1 台位里根本没有第三个东西要同步 —— 那是 M5 多端的事，不在这里顺手做。

use std::collections::{BTreeMap, BTreeSet};
use std::format;

use crate::cli::cmd::{BranchCmd, StashCmd, TagCmd};
use crate::cli::{m0, Cli};
use crate::error::{Error, Result};
use crate::store::keys::Hash;
use crate::store::{build, cas, checkout, history, keys, object, worktree};
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

/// `branch`：可移动的本地名字。**不参与同步**，只是把一段历史挂住 + 给 revspec 一个能敲的名字。
pub fn branch(cli: &Cli, cmd: &BranchCmd) -> Result<()> {
    let (name, _) = m0::resolve_project(cli)?;
    let db = cli.store()?;
    let pid = keys::project_id(&name);
    match cmd {
        BranchCmd::List => branch_list(&*db, pid, &name),
        BranchCmd::Add { name: b, at } => branch_add(&*db, pid, &name, b, at.as_deref()),
        BranchCmd::Rm { name: b } => branch_rm(&*db, pid, &name, b),
        BranchCmd::Move { name: b, to } => branch_move(&*db, pid, &name, b, to),
        BranchCmd::Rename { from, to } => branch_rename(&*db, pid, &name, from, to),
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
    m0::note_checkout_dirty(&rep);
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
/// "tag 也有一个 main"。分支也不列：那是 `branch list` 的地盘（两者共用 `H` 那一格，
/// 靠 [`build::BRANCH_PREFIX`] 分家）——**这里滤不掉就会把分支报成 tag**，而人接着就会
/// 用 `tag rm` 去删它。删 tag 时那道拒绝用的是同一批保留名，见 [`check_ref_name`]。
fn list(db: &dyn storage_engines::KvEngine, pid: keys::ProjectId, project: &str) -> Result<()> {
    println!("project   = {project}  (id={pid:#010x})");
    let mut n = 0usize;
    for (name, heads) in build::all_named_refs(db, pid)? {
        if name == build::MAIN
            || name.starts_with(build::BRANCH_PREFIX)
            || name == build::STASH
        {
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
    check_ref_name(tag, "tag")?;
    let target = sole_target(db, pid, at)?;
    // 先证它真在库里：`--at` 给 hash 前缀时 `resolve_rev` 已经查过，但给 `HEAD~3` 那种
    // 走的是亲代链，末尾那个 commit 若缺行就只有这儿能发现。挂一个指不到东西的名字 = 空头支票。
    let commit = object::read_commit(db, pid, &target)?;
    let refs = build::all_named_refs(db, pid)?;
    let old = match holder(&refs, tag) {
        Some((stored, old)) if stored != tag => {
            return Err(Error::BadInput(format!(
                "{tag:?} 已经被**分支**占着（同一个 ref 格子，只是带 `refs/heads/` 前缀）。\
                 tag 与分支不能同名：同名时 `rev_base` 先命中 tag，那个分支就被藏住了 —— \
                 而 `branch list` 与 `reset {tag}` 说的是两个东西"
            )));
        }
        Some((_, old)) => old.to_vec(),
        None => Vec::new(),
    };
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

/// "一个名字只钉一个 commit" 那条规矩的执行者，`tag add` 与 `branch add` 共用。
///
/// 给了 `--at` 就解析它，多解拒绝；没给就用当前 head 集合，而那时**必须恰好一个**——
/// 多 head 时"当前"有两个以上的东西，替人挑一个就是猜（§7.1 不许）。
/// 两条闸分开写是因为报错要分开：前一种该教人写清楚 revspec，后一种该教人加 `--at`。
fn sole_target(db: &dyn storage_engines::KvEngine, pid: keys::ProjectId, at: Option<&str>) -> Result<Hash> {
    match at {
        Some(rev) => {
            let mut hs = history::resolve_rev(db, pid, rev)?;
            // 一个名字只能指一个 commit。多解就报错，**不许挑第一个**（那是猜）。
            if hs.len() != 1 {
                return Err(Error::BadInput(format!(
                    "`{rev}` 指向 {} 个 commit，一个名字只能钉住一个。请写清楚（`{rev}~0` 或用 hash 前缀）",
                    hs.len()
                )));
            }
            Ok(hs.remove(0))
        }
        None => {
            let heads = build::ref_heads(db, pid, build::MAIN)?;
            if heads.len() != 1 {
                return Err(Error::BadInput(format!(
                    "当前项目有 {} 个 head（多 head 是正常状态，§7 第四行），省略 `--at` 就等于替人选一个 ⇒ 请写 `--at <rev>`",
                    heads.len()
                )));
            }
            Ok(heads[0])
        }
    }
}

/// 这个名字（tag 与分支**一起看**）被谁占着：返回它存储用的 ref 名与那一格里的 commit。
///
/// 按显示名比而不是按存储名比，正是为了跨类型撞名：只查自己那一格就会让 `tag foo` 与分支 `foo`
/// 同时存在，那时解析先命中 tag、分支被藏住 ⇒ 两个命令指着两个东西而人看不出来。
fn holder<'a>(
    refs: &'a BTreeMap<String, Vec<Hash>>,
    name: &str,
) -> Option<(&'a str, &'a [Hash])> {
    refs.iter()
        .find(|(k, _)| build::ref_display(k) == name)
        .map(|(k, hs)| (k.as_str(), hs.as_slice()))
}

fn rm(
    db: &dyn storage_engines::KvEngine,
    pid: keys::ProjectId,
    project: &str,
    tag: &str,
) -> Result<()> {
    check_ref_name(tag, "tag")?;
    // 按显示名找（[`holder`]）而不是只查字面名那一格：撞名的分支会走到这一句，报错要说清
    // "那个东西归 `branch rm` 管"，否则人只在 `branch list` 里见过它，接着就更糊涂。
    let refs = build::all_named_refs(db, pid)?;
    let old = match holder(&refs, tag) {
        Some((stored, old)) if stored == tag => old.to_vec(),
        Some((_, _)) => {
            return Err(Error::BadInput(format!(
                "{tag:?} 是一个**分支**不是 tag ⇒ 删它用 `branch rm {tag}`"
            )));
        }
        None => {
            return Err(Error::BadInput(format!("没有 tag {tag:?}（`tag list` 看有什么）")));
        }
    };
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

/// ref 名字（tag 与分支**共用一道闸**）的规矩。**每一条都是为了一句"不许猜"**，所以宁缺毋滥：
///
/// - `main` / `head`（不分大小写）是保留名：那两个格子由 `commit`/会话的 union 逻辑管，
///   被一个静态名字占住就会出现"删掉 tag 之后 head 集合少了一半"这种事。
/// - 长得像 hash 前缀（连续 7 个以上十六进制字符）的不收：`rev_base` 里命名引用排在 hash
///   之前，名字与 hash 撞脸时**解析顺序就成了裁决**，而那是人看不见的。
///   （见 `store::history::rev_base` 那段注释。）
/// - 以 `refs/` 开头的不收：分支就存在 `refs/heads/<名字>` 那一格（§9.2），允许这种名字
///   等于允许人伪造另一个命名空间 —— 那时 [`holder`] 的"显示名相同就算撞名"会失配。
/// - 控制字符 / 空白 / 开头的 `-`：`-` 会被下一个人抄成选项，空白会让 `--at` 那种写法分词分错。
///
/// 两类名字共用这一份实现是有意的：**两套闸迟早会长得不一样**，那时"tag 与分支同名"这种
/// 最难查的歧义就会从缝里钻进来（`rev_base` 先命中 tag，分支被藏住）。
fn check_ref_name(tag: &str, kind: &str) -> Result<()> {
    if tag.is_empty() {
        return Err(Error::BadInput(format!("{kind} 名字不能是空的")));
    }
    if tag.len() > 128 {
        return Err(Error::BadInput(format!(
            "{kind} 名字 {} 个字符，太长（上限 128）",
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
            "{kind} 名字 {tag:?} 以 `-` 开头：抄进命令行时会被当成选项解析"
        )));
    }
    if tag.starts_with(build::BRANCH_PREFIX) {
        return Err(Error::BadInput(format!(
            "{kind} 名字 {tag:?} 以 {p:?} 开头：那是分支自己那一格的前缀（`refs/heads/`），\
             手工写进来会让 `branch list` 与 `tag list` 各看见半个它（{p} 后面只要名字本身）",
            p = build::BRANCH_PREFIX
        )));
    }
    if tag.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(Error::BadInput(format!(
            "{kind} 名字 {tag:?} 里有空白或控制字符：`--at`/脚本读它都会分词分错"
        )));
    }
    let hexy = tag.chars().all(|c| c.is_ascii_hexdigit());
    if hexy && tag.len() >= 7 {
        return Err(Error::BadInput(format!(
            "{kind} 名字 {tag:?} 会长得像 commit hash 前缀 ⇒ 解析时两者必然撞脸，换一个字。这是故意的：\
             撞脸时谁先查谁赢，而那是人看不见的裁决"
        )));
    }
    Ok(())
}

// ════════════════════════════ branch ════════════════════════════
//
// 下面这一组都是 `H|p|名|0x00|hash` 那一格的读写，没有任何一处碰工作树 —— 所以它们**不带 `--apply`**。
// 闸的口径就这一句：**动工作树的才要 `--apply`**（`reset`/`checkout`/`gc` 那一族），只动一个名字的不要。
// 但不要 `--apply` ≠ 不要说清代价：`branch rm`/`branch move` 松开的是 gc 的担保，那一句必须打印出来。

/// `branch list`：每个分支一行，外加它与 `main` 差多少条 commit。
///
/// 为什么报差多少而不是只列名字：分支在这个工具里**不参与同步**（§5、§16.2），它的全部用处就是
/// "有一条 main 之外的线还挂着"。不报条数，`branch list` 就只是一张名字表，而人真正要决定的是
/// "这一支还要不要留"（留着 = gc 收不到那段历史）。
///
/// 代价：亲代索引一次前缀扫（[`object::parent_index`]）+ 每个分支**一次内存里的**闭包
/// （[`history::reachable_from`] 不碰引擎）⇒ O(commit 数 × 32 B)，与分支数无关的引擎调用只有那一次。
/// 这里不为了"少装一次索引"退回逐条点读：门面每读一次折一遍全库（§9.6.1）。
fn branch_list(db: &dyn storage_engines::KvEngine, pid: keys::ProjectId, project: &str) -> Result<()> {
    println!("project   = {project}  (id={pid:#010x})");
    let refs = build::all_named_refs(db, pid)?;
    let main_heads = build::ref_heads(db, pid, build::MAIN)?;
    println!(
        "main      = {} 个 head（同步只看这一个，分支不参与 push/pull）",
        main_heads.len()
    );
    let branches: Vec<(&str, &[Hash])> = refs
        .iter()
        .filter(|(k, _)| k.starts_with(build::BRANCH_PREFIX))
        .map(|(k, hs)| (k.as_str(), hs.as_slice()))
        .collect();
    if branches.is_empty() {
        println!("分支      = 0 个（建一个：`twosync branch add <名字> [--at <rev>]`）");
        return Ok(());
    }
    let index = object::parent_index(db, pid)?;
    let mine = history::reachable_from(&index, &main_heads, &[]);
    let mut pinned: BTreeSet<Hash> = BTreeSet::new();
    println!("分支      = {} 个", branches.len());
    for (stored, hs) in branches {
        let reach = history::reachable_from(&index, hs, &[]);
        let ahead = reach.difference(&mine).count();
        let behind = mine.difference(&reach).count();
        pinned.extend(reach.difference(&mine).copied());
        // 一个分支正常只有一个 tip。多出来不是"挑第一个印"的理由 ⇒ 每个都印，形状不对让人看得见。
        for h in hs {
            match object::read_commit(db, pid, h) {
                Ok(c) => println!(
                    "  {}  {}  {}  {}",
                    build::ref_display(stored),
                    keys::hex(h),
                    history::fmt_ts(c.ts),
                    c.msg
                ),
                Err(e) => println!(
                    "  {}  {}  （commit 读不回来：{e}）",
                    build::ref_display(stored),
                    keys::hex(h)
                ),
            }
        }
        // 时间/说明长度不一，做不到假对齐：位置关系另起一行说。
        println!(
            "          {}",
            if ahead == 0 {
                format!("已并入 main（main 比它多 {behind} 条）")
            } else {
                format!("main 之外 {ahead} 条 / main 比它多 {behind} 条")
            }
        );
    }
    if pinned.is_empty() {
        println!("效果      = 这些分支一条都没多保住（都已经在 main 里）⇒ gc 本来就不会收（§9.3）");
    } else {
        println!(
            "效果      = 这些分支一共保住 {} 条 main 之外的 commit ⇒ `gc --sweep` 收不到它们（§9.3）。\
             要松开哪一支就 `branch rm <名字>`，删之前看它报的\"松开了几条\"",
            pinned.len()
        );
    }
    Ok(())
}

fn branch_add(
    db: &dyn storage_engines::KvEngine,
    pid: keys::ProjectId,
    project: &str,
    name: &str,
    at: Option<&str>,
) -> Result<()> {
    check_ref_name(name, "分支")?;
    let target = sole_target(db, pid, at)?;
    // 与 `tag add` 同一句理由：名字指着库里读不回来的 commit = 空头支票，等要 `reset` 过去才发现。
    let commit = object::read_commit(db, pid, &target)?;
    let refs = build::all_named_refs(db, pid)?;
    if let Some((stored, _)) = holder(&refs, name) {
        return Err(Error::BadInput(format!(
            "{name:?} 这个名字已经被{what}占着（tag 与分支共用一格，名字不能相同）。\
             要挪位置用 `branch move {name} --to <rev>`，要换名字用 `branch rename`",
            what = if stored == build::branch_ref(name).as_str() {
                "分支"
            } else {
                "tag"
            }
        )));
    }
    build::set_ref(db, pid, &build::branch_ref(name), &[target])?;
    println!("project   = {project}  (id={pid:#010x})");
    println!(
        "分支      = {name}  → {}  ({})",
        keys::hex(&target),
        history::fmt_ts(commit.ts)
    );
    println!("说明      = {}", commit.msg);
    let m = build::MAIN;
    println!(
        "注意      = 分支**不参与同步**（第一版只看 `{m}`，§5/§16.2）。它在这里做两件事：\
         ① 把这段历史挂住 ⇒ `gc --sweep` 收不到它（§9.3）；② 给 revspec 一个能敲的名字 ⇒ \
         `reset {name}` / `checkout --at {name}` 直接可用"
    );
    println!(
        "不会动    = `commit` 只移 `{m}`，分支不会自己往前走 ⇒ 要跟上就 `branch move {name} --to HEAD`"
    );
    Ok(())
}

fn branch_rm(
    db: &dyn storage_engines::KvEngine,
    pid: keys::ProjectId,
    project: &str,
    name: &str,
) -> Result<()> {
    check_ref_name(name, "分支")?;
    let full = build::branch_ref(name);
    let refs = build::all_named_refs(db, pid)?;
    let old = match refs.get(&full) {
        Some(hs) => hs.clone(),
        None if holder(&refs, name).is_some() => {
            return Err(Error::BadInput(format!(
                "{name:?} 是一个 tag 不是分支 ⇒ 删它用 `tag rm {name}`"
            )));
        }
        None => {
            return Err(Error::BadInput(format!(
                "没有分支 {name:?}（`branch list` 看有什么）"
            )));
        }
    };
    // 删之前先算清"这一步真正松开了什么"，判据见 [`history::unheld_by_other_refs`]。
    let index = object::parent_index(db, pid)?;
    let lost = history::unheld_by_other_refs(&refs, &index, &full, &old);

    build::set_ref(db, pid, &full, &[])?;
    println!("project   = {project}  (id={pid:#010x})");
    println!(
        "删掉分支  = {name}（原来指着 {}）",
        old.iter().map(keys::short).collect::<Vec<_>>().join(" ")
    );
    if lost.is_empty() {
        println!("效果      = 什么都没松开：那段历史 `main` 或别的 ref 也挂着，gc 本来就不会收");
    } else {
        println!(
            "效果      = 这 {} 条 commit 现在**没有任何 ref 挂着** ⇒ 下一次 `gc --sweep --apply` \
             会把它们当垃圾收走（§9.3：不可达 = 可删）",
            lost.len()
        );
        println!(
            "补救      = 先 `tag add <名字> --at {}` 再删（或者留着这个分支）。真要收之后还有救吗——没有，\
             gc 是删块，不回收站",
            keys::short(&lost[0])
        );
    }
    Ok(())
}

fn branch_move(
    db: &dyn storage_engines::KvEngine,
    pid: keys::ProjectId,
    project: &str,
    name: &str,
    to: &str,
) -> Result<()> {
    check_ref_name(name, "分支")?;
    let full = build::branch_ref(name);
    let refs = build::all_named_refs(db, pid)?;
    let Some(old) = refs.get(&full) else {
        return Err(Error::BadInput(format!(
            "没有分支 {name:?}（`branch list` 看有什么）"
        )));
    };
    let old = old.clone();
    // 这里**不给"省略 `--to`"**：`tag add` 省的是"钉住当前"，而 move 省"挪到当前"就是在替人猜
    // "你要的是 head 还是 head~3"。名字要动就往一个确定的 commit 上动。
    let mut hs = history::resolve_rev(db, pid, to)?;
    if hs.len() != 1 {
        return Err(Error::BadInput(format!(
            "`{to}` 指向 {} 个 commit ⇒ 分支只能指一个。写清楚（`{to}~0` 或 hash 前缀）",
            hs.len()
        )));
    }
    let target = hs.remove(0);
    let commit = object::read_commit(db, pid, &target)?;
    let index = object::parent_index(db, pid)?;
    let lost = history::unheld_by_other_refs(&refs, &index, &full, &old);

    println!("project   = {project}  (id={pid:#010x})");
    println!(
        "分支 {name} = {} → {}",
        old.iter().map(keys::short).collect::<Vec<_>>().join(" "),
        keys::hex(&target)
    );
    println!("          {}  {}", history::fmt_ts(commit.ts), commit.msg);
    if !lost.is_empty() {
        println!(
            "危险      = 旧 tip 松开之后有 {} 条 commit 不再被任何 ref 挂着 ⇒ `gc --sweep --apply` \
             会把那段历史当垃圾收走（§9.3）。要留住先 `tag add <名字> --at {}`",
            lost.len(),
            keys::short(&lost[0])
        );
    }
    build::set_ref(db, pid, &full, &[target])?;
    println!("动作      = 已挪（工作树一个字没动，要落地就 `checkout --at {name}`）");
    Ok(())
}

fn branch_rename(
    db: &dyn storage_engines::KvEngine,
    pid: keys::ProjectId,
    project: &str,
    from: &str,
    to: &str,
) -> Result<()> {
    check_ref_name(from, "分支")?;
    check_ref_name(to, "分支")?;
    let old_full = build::branch_ref(from);
    let refs = build::all_named_refs(db, pid)?;
    let Some(heads) = refs.get(&old_full) else {
        return Err(Error::BadInput(format!(
            "没有分支 {from:?}（`branch list` 看有什么）"
        )));
    };
    let heads = heads.clone();
    if holder(&refs, to).is_some() {
        return Err(Error::BadInput(format!(
            "{to:?} 这个名字已经有人占了（tag 与分支共用一格）⇒ 换个名字，或先把占着它的删掉"
        )));
    }
    // **先建新再删旧**：`set_ref` 是一笔事务，但两步之间不是。反过来做，中间崩的那一次会留下
    // "旧名字已删、新名字没建"的空档 ⇒ 那段历史当场变得无 ref 挂着，而 gc 就在下一次；
    // 现在这个顺序最坏是**两个名字指着同一个 commit**，谁也不丢。
    build::set_ref(db, pid, &build::branch_ref(to), &heads)?;
    build::set_ref(db, pid, &old_full, &[])?;
    println!("project   = {project}  (id={pid:#010x})");
    println!(
        "改名      = {from} → {to}（还是指着 {}）",
        heads.iter().map(keys::short).collect::<Vec<_>>().join(" ")
    );
    println!("注意      = 老名字从此查不到 ⇒ 脚本/别名里写 `{from}` 的地方要跟着改（revspec 是按名字查的）");
    Ok(())
}

// ════════════════════════ revert / cherry-pick ════════════════════════

/// 两条命令的差别只有"哪一版当另一侧"，所以做成枚举而不是 `bool`：传反一个布尔就静默产出
/// 另一种历史，而这两种历史在账面上长得一模一样（都是单亲 commit）。
enum Which {
    Revert,
    CherryPick,
}

impl Which {
    fn name(&self) -> &'static str {
        match self {
            Which::Revert => "revert",
            Which::CherryPick => "cherry-pick",
        }
    }
}

pub fn revert(cli: &Cli, rev: &str, apply: bool, allow_mass_delete: bool) -> Result<()> {
    reapply(cli, rev, Which::Revert, apply, allow_mass_delete)
}

pub fn cherry_pick(cli: &Cli, rev: &str, apply: bool, allow_mass_delete: bool) -> Result<()> {
    reapply(cli, rev, Which::CherryPick, apply, allow_mass_delete)
}

/// `revert` / `cherry-pick` 的共用实现：**同一把结构三方尺，只是参数摆位不同**。
///
/// | 命令 | ours（我们这一侧） | theirs（另一侧） | base（共同祖先） |
/// |---|---|---|---|
/// | `revert c` | `main` 那一版的树 | `c` 的**亲代**那一版 | `c` 的树 |
/// | `cherry-pick c` | `main` 那一版的树 | `c` 的树 | `c` 的**亲代**那一版 |
///
/// 为什么这不是"再写一个合并器"：撤销 = "把亲代那一版当另一侧再合一次"，摘取 = "把 c 那一版当
/// 另一侧再合一次"。冲突判据、目录递归、"有冲突就不落盘"全都复用 §7.1 那一份 —— **"报账"与"动手"
/// 必须共用一把尺**（`branch` 那批同一条规矩）。差别只在三个参数各站哪个位置。
///
/// 与 `merge --keep-both` 同一条边界：**只做路径级结构合并，不做内容三方合并**（§1、§16.2 第三层）
/// ⇒ 同一个文件被两边各改成不同东西就报冲突、拒绝、一个字不写。
///
/// 顺序仍是 FR-13：**先算账（`Emit::Report`，连清单行都不写）→ 只报那一趟到此为止 → `--apply` 才
/// 落盘（`Emit::Store`，两次的树 hash 必须一致）→ 检出 → 最后挪 ref**。
/// 代价：整棵树的遍历做两遍。这是稀有命令，用一遍遍历换"只报真的一字不写"值。
fn reapply(
    cli: &Cli,
    rev: &str,
    which: Which,
    apply: bool,
    allow_mass_delete: bool,
) -> Result<()> {
    let (project, root) = m0::resolve_project(cli)?;
    let db = cli.store()?;
    let pid = keys::project_id(&project);
    let heads = build::ref_heads(&*db, pid, build::MAIN)?;
    // 落点必须唯一：两个 head 时"撤销到哪一个上面"没有定义，而挑一个就是猜（§7.1）。
    if heads.len() != 1 {
        return Err(Error::BadInput(format!(
            "{} 要往唯一一个 head 上落，而现在有 {} 个 ⇒ 先 `merge --keep-both` 或 `reset <hash>` 把场面收成一个",
            which.name(),
            heads.len()
        )));
    }
    let onto = heads[0];
    let head_commit = object::read_commit(&*db, pid, &onto)?;
    // 与 `checkout`/`reset` 同一道闸：一个 rev 写到达两个 commit 就报错，不许挑第一个。
    let target = m0::one_commit(history::resolve_rev(&*db, pid, rev)?, rev)?;
    let commit = object::read_commit(&*db, pid, &target)?;
    if commit.parents.len() > 1 {
        return Err(Error::BadInput(format!(
            "{rev:?} 是 {} 个亲代的合并 commit ⇒ {what}它之前要先说撤哪一边，那是裁决不是默认值\
             （git 要 `-m N`，本项目不给这个旋钮）。想丢掉那一支就 `reset <hash>` 点名",
            commit.parents.len(),
            what = which.name()
        )));
    }
    // 无亲代 = 与**空树**比：`revert` 一个根就是把它引入的一切撤掉 —— 那会正撞 FR-10，也正是它该撞的地方。
    let parent_tree = match commit.parents.first() {
        Some(p) => object::read_commit(&*db, pid, p)?.tree,
        None => object::empty_tree(),
    };
    let ours = head_commit.tree;
    let (theirs, base, msg) = match which {
        Which::Revert => (
            parent_tree,
            commit.tree,
            format!("revert {}: {}", keys::short(&target), commit.msg),
        ),
        Which::CherryPick => (
            commit.tree,
            parent_tree,
            format!("cherry-pick {}: {}", keys::short(&target), commit.msg),
        ),
    };

    println!("project   = {project}  (id={pid:#010x})");
    println!("工作树    = {}", root.display());
    println!("动作      = {} {rev}", which.name());
    println!("落点      = {}（当前唯一 head）", keys::short(&onto));
    println!(
        "作用对象  = {}  {}  {}",
        keys::short(&target),
        history::fmt_ts(commit.ts),
        commit.msg
    );
    println!(
        "基        = {}",
        match commit.parents.first() {
            Some(p) => format!("亲代 {}", keys::short(p)),
            None => "没有亲代 ⇒ 与空树比".to_string(),
        }
    );

    // 一眼就能判的空：另一侧与落点是同一棵树 ⇒ 五规则一条都不会改（`add +0` 那种空提交不许产生，
    // 碎 commit 会把引擎版本链白白撑大，§9.6.2）。
    if theirs == ours {
        println!("结果      = 什么都没带来（那一版与落点是同一棵树）⇒ 不产生空提交，head 不动");
        return Ok(());
    }

    let new_tree = match history::merge_trees(
        &*db,
        pid,
        &ours,
        &theirs,
        &[base],
        history::Emit::Report,
    )? {
        history::Merge::Conflict { paths } => {
            println!(
                "合不了    = {} 条路径两边各改成不同东西（§7.1：有交集只能人选）",
                paths.len()
            );
            for p in paths.iter().take(10) {
                println!("          - {p}");
            }
            return Err(Error::BadInput(format!(
                "{} 被 {} 条同路径冲突挡住 ⇒ 这几条要人裁决（`checkout --at` 选一边，或交给真 git）",
                which.name(),
                paths.len()
            )));
        }
        history::Merge::Done { tree } => tree,
    };
    let mut cs = build::diff(&*db, pid, Some(&ours), &new_tree)?;
    cs.fold_renames();
    if cs.is_empty() {
        println!("结果      = 什么都没带来（每一条要么已经一样、要么那一边没动过）⇒ 不产生空提交");
        return Ok(());
    }
    // FR-10 在**写任何东西之前**过一遍：撤掉一个"加了两万文件"的 commit 就是撤两万条删除。
    let known = count_paths(&*db, pid, &ours)?;
    build::check_mass_delete(cs.delete, cs.deleted_bytes, known, allow_mass_delete)?;
    println!(
        "改动      = +{} ~{} -{} 改名 {}  入 {:.1} MiB / 删 {:.1} MiB（分母 = 落点的 {known} 条）",
        cs.add,
        cs.modify,
        cs.delete,
        cs.rename,
        cs.bytes as f64 / 1_048_576.0,
        cs.deleted_bytes as f64 / 1_048_576.0
    );
    println!(
        "将生成    = 一笔单亲 commit：parents = [{}]，tree = {}",
        keys::short(&onto),
        keys::short(&new_tree)
    );
    println!("          = head 从 {} 挪过去，工作树跟着落地", keys::short(&onto));
    if !apply {
        println!("模式      = 只报（FR-7）。这一趟连清单行都没写：上面那棵树是**算**出来的（`Emit::Report`）");
        return Ok(());
    }

    // 真落盘那一次走同一套遍历，只是这次把中间清单写进库。
    let stored = match history::merge_trees(
        &*db,
        pid,
        &ours,
        &theirs,
        &[base],
        history::Emit::Store,
    )? {
        history::Merge::Done { tree } => tree,
        history::Merge::Conflict { paths } => {
            return Err(Error::BadInput(format!(
                "第二次遍历报出 {} 条冲突，而第一次说能合 ⇒ 站点在两次遍历之间被动过 ⇒ 这一趟拒绝，\
                 先查是不是有 `daemon` 或另一个 `commit` 在写同一个库",
                paths.len()
            )));
        }
    };
    if stored != new_tree {
        return Err(Error::BadInput(
            "两次遍历出来的树不是同一棵 ⇒ 同上，库在动 ⇒ 拒绝".to_string(),
        ));
    }
    let made = object::put_commit(
        &*db,
        pid,
        &object::Commit {
            parents: vec![onto],
            tree: stored,
            peer: m0::PEER_LOCAL.to_string(),
            ts: worktree::now_ns(),
            msg,
            stats: object::Stats {
                add: cs.add,
                modify: cs.modify,
                delete: cs.delete,
                rename: cs.rename,
                bytes: cs.bytes,
            },
        },
    )?;
    println!("生成      = {}  tree {}", keys::hex(&made), keys::short(&stored));

    // 先落地、最后挪 ref（FR-13，与 `reset` 同一个顺序）：被 §7.3 逐文件门或 FR-10 挡下的那一次，
    // head 一个字都没动，人可以直接改参数重跑。
    let store = cas::Cas::new(&objects_dir(&cli.root), cli.block_size()?)?;
    let rep = checkout::checkout(
        &*db,
        pid,
        &store,
        &root,
        &made,
        None,
        checkout::Opts {
            apply: true,
            allow_mass_delete,
        },
    )?;
    build::set_ref(&*db, pid, build::MAIN, &[made])?;
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
    m0::note_checkout_dirty(&rep);
    println!("head      = 已挪到 {}（原来的 {} 留在库里，还是别的 commit 的亲代）", keys::short(&made), keys::short(&onto));
    Ok(())
}

/// 树里有多少条路径 —— FR-10 那句"已知条目"的分母。
///
/// `commit` 那侧用的是"本轮扫到的行数"，这里用**落点那一版**的条目数：两者说的是同一件事
/// （"你现在有多少东西"），不是"这次动了多少"。流式走完不物化（[`history::walk_tree`] 的回调形状），
/// 否则这条只报的命令自己要占 O(文件数) 的内存。
fn count_paths(db: &dyn storage_engines::KvEngine, pid: keys::ProjectId, tree: &Hash) -> Result<u32> {
    let mut n = 0u32;
    let mut visit = |_p: &str, _e: &object::TreeEntry| {
        n += 1;
        Ok(())
    };
    history::walk_tree(db, pid, tree, &mut visit)?;
    Ok(n)
}

// ════════════════════════ blame / bisect ════════════════════════
//
// 这两条都是**只读**命令 ⇒ 不带 `--apply`（闸的口径与 `branch` 那批同一句：动工作树的才要）。
// 判据住在 `store::history`（`blame_path` / `bisect_step`），这一层只负责措辞 ——
// 枚举能被用例钉住，`format!` 出来的那句话钉不住。

/// `blame <path>`：这条路径被哪些 commit 动过（**路径级**，不是行级；理由见 [`history::blame_path`]）。
pub fn blame(cli: &Cli, path: &str, max: usize) -> Result<()> {
    let (project, _) = m0::resolve_project(cli)?;
    let db = cli.store()?;
    let pid = keys::project_id(&project);
    let heads = build::ref_heads(&*db, pid, build::MAIN)?;
    if heads.is_empty() {
        return Err(Error::BadInput(
            "head 集合是空的：这个项目还没有历史".to_string(),
        ));
    }
    let h = history::blame_path(&*db, pid, &heads, path, max)?;
    println!("project   = {project}  (id={pid:#010x})");
    println!("路径      = {path}");
    println!(
        "窗口      = 走了 {} 本 commit（{}）",
        h.scanned,
        if max == 0 {
            "--max 0 = 全部，内存 O(历史长度)"
        } else {
            "--max 上限"
        }
    );
    println!("动过她    = {} 本", h.hits.len());
    for hit in &h.hits {
        println!(
            "  {}  {}  {:<8}  {}",
            keys::short(&hit.node.hash),
            history::fmt_ts(hit.node.commit.ts),
            hit.node.commit.peer,
            change_word(&hit.change)
        );
    }
    println!(
        "现在      = {}",
        match &h.now {
            Some(e) => format!(
                "{}，{} B，mode {}，内容 {}",
                kind_word(e),
                e.size,
                e.mode,
                keys::short(&e.hash)
            ),
            None => "这一版里**没有**这条路径".to_string(),
        }
    );
    // 三条"命令自己要说"的边界：窗口截断、行级不提供、以及下一步该敲什么。
    if h.truncated {
        println!(
            "注意      = 窗口截断在第 {} 本 ⇒ 最老那一行**不代表**她第一次出现，往前还有（--max 0 走全史）",
            max
        );
    }
    println!(
        "行级归属  = 不提供。4 MiB 块 + 二进制语料里\"行\"不是任何一层的身份（`B` 清单只有块号/hash/大小），做出来的是看起来像 `git blame` 而语义是猜的东西"
    );
    println!("看整文件  = `show <rev> <path>`；取回她 = `restore --at <rev> <path>`");
    Ok(())
}

/// 归属那一行的措辞。判据在 [`history::PathChange`]，这里只有话术。
fn change_word(c: &history::PathChange) -> String {
    use history::PathChange as P;
    match c {
        P::Appeared => "出现（新增）".to_string(),
        P::Disappeared => "消失（删除）".to_string(),
        P::Content { to } => format!("内容换成 {}", keys::short(to)),
        P::Mode { to } => format!("只改权限 mode → {to:o}"),
        P::KindSwap => "类型互换（文件↔目录↔符号链接）".to_string(),
    }
}

fn kind_word(e: &object::TreeEntry) -> &'static str {
    match e.kind {
        keys::Kind::Dir => "目录",
        keys::Kind::Symlink => "符号链接",
        keys::Kind::File => "文件",
    }
}

/// `bisect --bad <rev> --good <rev>`：无状态的二分定位（取舍的理由见 [`history::bisect_step`]）。
pub fn bisect(cli: &Cli, bad: &str, good: &str) -> Result<()> {
    let (project, _) = m0::resolve_project(cli)?;
    let db = cli.store()?;
    let pid = keys::project_id(&project);
    let bad_c = m0::one_commit(history::resolve_rev(&*db, pid, bad)?, bad)?;
    let good_c = m0::one_commit(history::resolve_rev(&*db, pid, good)?, good)?;
    println!("project   = {project}  (id={pid:#010x})");
    println!(
        "坏的那一本  = {}  {}",
        keys::short(&bad_c),
        head_line(&*db, pid, &bad_c)?
    );
    println!(
        "好的那一本  = {}  {}",
        keys::short(&good_c),
        head_line(&*db, pid, &good_c)?
    );
    let step = match history::bisect_step(&*db, pid, &bad_c, &good_c)? {
        history::BisectVerdict::Step(s) => s,
        history::BisectVerdict::GoodNotAncestor => {
            return Err(Error::BadInput(format!(
                "`{good}` 不是 `{bad}` 的祖先 ⇒ 两端不在同一条线上，\"之间\"没有定义。先确认哪个真的更坏（这里不替你猜，也不给第三种写法）"
            )));
        }
        history::BisectVerdict::Reversed => {
            return Err(Error::BadInput(format!(
                "`{bad}` 反而是 `{good}` 的祖先 ⇒ 你说坏的那一本更**老**，两端写反了？"
            )));
        }
    };
    println!(
        "区间      = 怀疑 {} 本（坏的闭包 {} 本 − 好的闭包 {} 本）",
        step.suspects, step.bad_closure, step.good_closure
    );
    println!(
        "候选      = {}  {}",
        keys::hex(&step.candidate),
        head_line(&*db, pid, &step.candidate)?
    );
    if step.suspects == 1 {
        // 区间收到只剩一本 ⇒ 她就是结论。这时候再给"下一步怎么问"是多余的，而且会让人以为
        // 还要再跑一次才能定（用例 `bisect_range_shrinks_every_step_until_one_suspect_is_left`
        // 钉的就是这一格：唯一的候选会被重复问一次，那是收敛的最后一步，不是死循环）。
        println!(
            "结论      = 区间只剩这一本 ⇒ 元凶就是 {}（上面那一条候选）",
            keys::short(&step.candidate)
        );
    } else {
        println!(
            "下一步    = `checkout --at {} --apply` 去测这一本，测完再跑一次：还是坏 → `bisect --bad {} --good {good}`；变好了 → `bisect --bad {bad} --good {}`",
            keys::short(&step.candidate),
            keys::short(&step.candidate),
            keys::short(&step.candidate),
        );
        println!(
            "保证      = 候选属于怀疑集 ⇒ 每问一次区间**严格变小**。历史里有合并时**不保证**正好缩一半（时间序不等于拓扑序）。这一步**不落游标** ⇒ 没有\"bisect 卡住要先 reset\"那种坏账"
        );
    }
    Ok(())
}

/// 一行 commit 摘要（时间 + 来源 + 说明）。
fn head_line(db: &dyn storage_engines::KvEngine, pid: keys::ProjectId, h: &Hash) -> Result<String> {
    let c = object::read_commit(db, pid, h)?;
    Ok(format!("{}  {:<8}  {}", history::fmt_ts(c.ts), c.peer, c.msg))
}

// ════════════════════════ stash ════════════════════════

/// `stash`：把"已扫描、未提交"的那份改动定稿成一笔 commit，挂在 `refs/stash` 那个名字上，
/// 然后（带 `--apply` 时）把工作树清回 head。
///
/// 她为什么值得单独一条命令，而不是"你先 `commit` 一下"：`commit` 会**移 head**，那份半成品就此
/// 成了历史的一部分，还会被下一次 `push` 送出去。`stash` 要的是"落库但别成为 head"——那个出口
/// 本轮刚开出来（[`build::build_detached`]）。挂在名字上另外换来两件事：她被 `gc` 保下来
/// （§9.3 的保留集是全部 ref），同时**不参与同步**（`net::session` 三处读的都是 `main`）。
///
/// **只有一格**（`refs/stash` 是一个名字，不是一个栈）：git 那个 stash 栈要的是"多条的排序与找回"，
/// 那是另一个子系统。第二条存在时直接**拒绝**，不悄悄把第一条挤掉——被挤掉的那条只剩这一个名字挂着，
/// 名字一没她就不可达，下一次 `gc` 连块一起收走，那是丢用户的改动，不是丢缓存。
pub fn stash(cli: &Cli, cmd: &StashCmd) -> Result<()> {
    let (project, root) = m0::resolve_project(cli)?;
    let db = cli.store()?;
    let pid = keys::project_id(&project);
    let store = cas::Cas::new(&objects_dir(&cli.root), cli.block_size()?)?;
    println!("project   = {project}  (id={pid:#010x})");
    println!("工作树    = {}", root.display());
    let ctx = StashCtx {
        db: &*db,
        pid,
        bucket: &cli.io_bucket,
        store: &store,
        root: &root,
    };
    // 三条会动工作树的子命令在**落地那一步之后各自补一次全量 `scan`**（理由见
    // [`rescan_after_landing`]）。补扫点放在落地点里面而不是这里：`push` 发现"没有可存的改动"时
    // 根本没落地，在外面判断 `apply` 旗标会白扫一遍。
    match cmd {
        StashCmd::Push { msg, apply, allow_mass_delete } => {
            ctx.push(msg.as_deref(), *apply, *allow_mass_delete)
        }
        StashCmd::List => ctx.list(),
        StashCmd::Apply { apply, allow_mass_delete } => ctx.back(*apply, *allow_mass_delete, false),
        StashCmd::Pop { apply, allow_mass_delete } => ctx.back(*apply, *allow_mass_delete, true),
        StashCmd::Drop => ctx.drop(),
    }
}

/// `stash` 四条子命令共用的**现场**。收成结构而不是给每个函数塞八个参数：
/// 这五样是同一次操作的坐标（同一份库、同一个限速桶、同一个 CAS、同一个工作树根），
/// 不是调用方可选的旋钮——`clippy` 的 7 参上限报的就是那个形状（与 `Merger`/`PushArgs` 同一处理）。
struct StashCtx<'a> {
    db: &'a dyn storage_engines::KvEngine,
    pid: keys::ProjectId,
    bucket: &'a cas::Bucket,
    store: &'a cas::Cas,
    root: &'a std::path::Path,
}

impl StashCtx<'_> {
    /// 把工作树的现状重新落进 `W`。**为什么是 `stash` 的责任而不是 `checkout` 的**：
    /// 全量命令（`checkout`/`reset`）的口径本来就是"工作树变了而 `W` 还是旧的 ⇒ 下一步 `scan`"，
    /// 但 stash 的定义就是"把同一份改动搬进搬出"——搬出去之后紧接着要搬回来，中间没有让人 scan 的那一步。
    /// 少了这一次扫，`stash apply` 会按**旧快照**判"她已经在工作树上了"，于是落地打成 `建 0 / 覆盖 0`
    /// 而盘上一个字节没变（这条不是推理：`apply_keeps_the_stash_and_pop_removes_it_only_after_landing`
    /// 第一次跑就撞上）。搬回来的那一次也要扫：`pop` 之后紧接着就是 `commit`，而 `commit` 吃的正是 `W`。
    ///
    /// 代价照实说：一次全量 `scan`（12 GiB 站点 ≈ 68 s 量级，§11.4b）。不做窄扫是有理由的：
    /// 窄扫那一轮**不推进活性线**，因此一个删除都不报（§11.4b 的修订），而 stash 搬进搬出最常碰的就是删除。
    fn rescan_after_landing(&self) -> Result<()> {
        let (db, pid, root) = (self.db, self.pid, self.root);
        let rep = worktree::scan_scoped(
            db,
            pid,
            root,
            &worktree::Scope::Full,
            false,
            2000,
            self.bucket,
        )?;
        println!(
            "补扫      = 重新认了一遍工作树（{} 条活着 / 走过 {} 条）⇒ 只报的 `plan`/`status` 与接下来的 `commit` 吃的都是这一份快照",
            rep.walked, rep.hashed
        );
        Ok(())
    }

    /// 那条 stash 替你扛着的是**哪一份改动**：她自己相对她亲代（= push 那一刻的 head）的差。
    ///
    /// 不拿"当前 head"当基线：push 之后 head 可能又动了，那时"相对 head"里会混进别人的改动，
    /// 报出来就像 stash 里多/少东西了。她的亲代才是她真正装着的那一份，这个数不会随时间漂。
    fn carries(&self, commit: &keys::Hash) -> Result<build::ChangeSet> {
        let (db, pid) = (self.db, self.pid);
        let c = object::read_commit(db, pid, commit)?;
        let base = match c.parents.first() {
            Some(p) => Some(object::read_commit(db, pid, p)?.tree),
            None => None,
        };
        let mut cs = build::diff(db, pid, base.as_ref(), &c.tree)?;
        cs.fold_renames();
        Ok(cs)
    }

    fn push(
        &self,
        msg: Option<&str>,
        apply: bool,
        allow_mass_delete: bool,
    ) -> Result<()> {
        let (db, pid, store, root) = (self.db, self.pid, self.store, self.root);
        // 先看账：`plan` 吃的是最近一次 `scan` 的快照（不替你 scan，与 `commit` 同一口径）。
        let p = build::plan(db, pid)?;
        if p.changes.is_empty() {
            println!("存不了    = 没有已扫描未提交的改动（工作树相对 head 干净，或者还没 `scan`）");
            return Ok(());
        }
        if !build::ref_heads(db, pid, build::STASH)?.is_empty() {
            return Err(Error::BadInput(
                "已经有一条 stash 了 ⇒ 先 `stash pop`（取回并删掉她）或 `stash drop`（丢掉她）。 \
                 这里不许覆盖：被挤掉的那条只剩 `refs/stash` 一个名字挂着，名字一没她就不可达，\
                 下一次 `gc --sweep --apply` 会连块一起收走——那是丢用户的改动，不是丢缓存"
                    .to_string(),
            ));
        }
        println!(
            "要存      = +{} ~{} -{} 改名 {}  入 {:.1} MiB（分母 = 站上的 {} 条）",
            p.changes.add,
            p.changes.modify,
            p.changes.delete,
            p.changes.rename,
            p.changes.bytes as f64 / 1_048_576.0,
            p.known
        );
        if !apply {
            println!("模式      = 只报（FR-7：动工作树的才要 --apply）。这一趟连 commit 都没建");
            return Ok(());
        }
        let heads = build::ref_heads(db, pid, build::MAIN)?;
        if heads.len() != 1 {
            return Err(Error::BadInput(format!(
                "stash 要落在唯一一个 head 上（她的亲代就是那一个），而现在有 {} 个 ⇒ 先 `merge` 或 `reset` 收成一个",
                heads.len()
            )));
        }
        let built = build::build_detached(
            db,
            pid,
            store,
            root,
            build::Meta {
                peer: m0::PEER_LOCAL,
                msg: msg.unwrap_or("stash 未提交改动"),
                ts: worktree::now_ns(),
                allow_mass_delete,
            },
        )?;
        let Some(commit) = built.commit else {
            // 上面 `plan` 说有改动、这里没建出 commit ⇒ 两次读之间有人动了库（另一个 `commit`/`daemon`）。
            return Err(Error::BadInput(
                "扫描快照与落库那一趟对不上（说有的改动没建出 commit）⇒ 这一趟什么都没写，先看 `status`"
                    .to_string(),
            ));
        };
        // 顺序是**先建 → 挂名字 → 再清工作树**。反过来（先清再建）会让中间任何一步失败都等于
        // 把那份改动唯一的存在形式（工作树）先擦掉了。
        build::set_ref(db, pid, build::STASH, &[commit])?;
        println!("stash     = {}  tree {}", keys::hex(&commit), keys::short(&built.tree));
        println!(
            "挂在      = {:?}（她因此被 gc 保下来 §9.3；不参与同步，会话只读 `{}`）",
            build::STASH,
            build::MAIN
        );
        let rep = checkout::checkout(
            db,
            pid,
            store,
            root,
            &heads[0],
            None,
            checkout::Opts {
                apply: true,
                allow_mass_delete,
            },
        )?;
        m0::note_checkout_dirty(&rep);
        println!(
            "工作树    = 已清回 head：建 {} / 覆盖 {} / 删 {} / 跳过 {}{}",
            rep.create,
            rep.overwrite,
            rep.delete,
            rep.skipped,
            if rep.deferred > 0 {
                format!(" / deferred 不动 {}", rep.deferred)
            } else {
                String::new()
            }
        );
        self.rescan_after_landing()?;
        println!("取回      = `stash apply --apply`（留着她）或 `stash pop --apply`（取回并且删掉她）");
        Ok(())
    }

    fn list(&self) -> Result<()> {
        let (db, pid) = (self.db, self.pid);
        let Some(commit) = build::ref_heads(db, pid, build::STASH)?.pop() else {
            println!("stash     = 没有（存一份：`twosync stash push --apply`）");
            return Ok(());
        };
        let c = object::read_commit(db, pid, &commit)?;
        println!(
            "stash     = {}  {}  {:<8}  {}",
            keys::short(&commit),
            history::fmt_ts(c.ts),
            c.peer,
            c.msg
        );
        let cs = self.carries(&commit)?;
        println!(
            "她扛着    = +{} ~{} -{} 改名 {}（这是 push 那一刻的账，不随 head 再动而漂）",
            cs.add, cs.modify, cs.delete, cs.rename
        );
        println!("取回      = `stash apply --apply` 或者 `stash pop --apply`（后者连名字一起删）");
        Ok(())
    }

    /// `stash apply` / `stash pop`：把那一版落回工作树。`pop` 在落地**成功之后**才删名字。
    fn back(
        &self,
        apply: bool,
        allow_mass_delete: bool,
        pop: bool,
    ) -> Result<()> {
        let (db, pid, store, root) = (self.db, self.pid, self.store, self.root);
        let Some(commit) = build::ref_heads(db, pid, build::STASH)?.pop() else {
            return Err(Error::BadInput(
                "没有 stash（`stash list` 看有什么）".to_string(),
            ));
        };
        let cs = self.carries(&commit)?;
        println!(
            "stash     = {}（她扛着 +{} ~{} -{}）",
            keys::short(&commit),
            cs.add,
            cs.modify,
            cs.delete
        );
        if !apply {
            println!(
                "模式      = 只报（FR-7）。加 --apply 才会动工作树{}",
                if pop { "，并且删掉那条 stash" } else { "" }
            );
            return Ok(());
        }
        let rep = checkout::checkout(
            db,
            pid,
            store,
            root,
            &commit,
            None,
            checkout::Opts {
                apply: true,
                allow_mass_delete,
            },
        )?;
        m0::note_checkout_dirty(&rep);
        println!(
            "工作树    = 落地：建 {} / 建目录 {} / 覆盖 {} / 删 {} / 跳过 {}{}",
            rep.create,
            rep.mkdir,
            rep.overwrite,
            rep.delete,
            rep.skipped,
            if rep.deferred > 0 {
                format!(" / deferred 不动 {}", rep.deferred)
            } else {
                String::new()
            }
        );
        // 补扫放在**删名字之前**：`pop` 的下一步通常是 `commit`，而 `commit` 吃的是 `W`；
        // 少了这一扫，`commit` 看到的就是搬回来**之前**的那份快照（会把刚取回的改动当成"没变化"）。
        self.rescan_after_landing()?;
        if pop {
            // 顺序：**先落地、后删名字**。反过来若 checkout 被 §7.3 逐文件门或 FR-10 挡下，
            // 名字已经没了 ⇒ 那份改动就真的不可达了。
            build::set_ref(db, pid, build::STASH, &[])?;
            println!(
                "stash     = 已删（pop 的语义就是取回并且丢掉名字）。她从此没有 ref 挂着 ⇒ \
                 **尽快 `scan` + `commit`**：否则下一次 `gc --sweep --apply` 会把那一版连块一起收走（§9.3）"
            );
        } else {
            println!("stash     = 仍然挂着（apply 不动名字；不要了再 `stash drop`）");
        }
        Ok(())
    }

    fn drop(&self) -> Result<()> {
        let (db, pid) = (self.db, self.pid);
        let old = build::ref_heads(db, pid, build::STASH)?;
        if old.is_empty() {
            return Err(Error::BadInput(
                "没有 stash 可丢（`stash list` 看有什么）".to_string(),
            ));
        }
        let refs = build::all_named_refs(db, pid)?;
        let index = object::parent_index(db, pid)?;
        let lost = history::unheld_by_other_refs(&refs, &index, build::STASH, &old);
        build::set_ref(db, pid, build::STASH, &[])?;
        println!(
            "丢掉 stash = {}（工作树一个字没动）",
            old.iter().map(keys::short).collect::<Vec<_>>().join(" ")
        );
        if lost.is_empty() {
            println!("效果      = 什么都没松开：那一版还有别的 ref 挂着，gc 本来也收不到");
        } else {
            println!(
                "效果      = 松开 {} 条 commit（它们不再被任何 ref 挂着）⇒ 下一次 `gc --sweep --apply` \
                 会把那一版连块一起收走（§9.3）。改动本身还在工作树里，要留就 `scan` + `commit`",
                lost.len()
            );
        }
        Ok(())
    }
}
