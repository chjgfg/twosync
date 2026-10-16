//! `checkout`：把一个版本（整棵树或一棵子树）落到工作树。
//!
//! **这是本项目唯一会批量删工作树文件的命令**，所以它的形状全绕着这件事设计。
//!
//! # 默认只报不动手
//!
//! 这就是 FR-8 层一里 `plan` / `apply` 那一对命令的形状：先看要干什么，再决定干不干。
//! 实现上是**两趟遍历**：第一趟只数数（并过熔断闸），第二趟才执行。一趟遍历的代价是
//! O(变更目录数)（未变子树整块跳过，NFR-6），跑两遍换来"动手之前已经看过全貌"，划算。
//!
//! # 三道闸，顺序固定
//!
//! 1. 只报模式（默认）：一个字节都不写。
//! 2. FR-10 熔断：要删的规模超阈值 ⇒ 整个操作拒掉（`--allow-mass-delete` 才放行）。
//!    闸必须在执行之前，否则"删到一半被拒"比"不删"糟得多。
//! 3. §7.3 逐文件门：**覆盖、删除、以及"新增但目标路径上已经有个东西"**，都过
//!    `restore::check_local_is_recoverable`。用的是同一个函数、同一把尺；给这条命令
//!    另写一份检查 = 给"哪一版算已备份"留第二个定义。
//!
//! # 曾经有的那道"脏工作树前置闸"为什么被拿掉
//!
//! 第一版是"工作树相对 HEAD 有未提交改动就拒"。听起来像 git，实际在本项目里站不住：
//! `checkout` 只改工作树、不动 head（v1 没有分支），所以**它自己每执行一次，就把工作树
//! 变成相对 HEAD 永久脏**。那道闸会让第二次 checkout 直接跑不动，把命令本身废掉。
//!
//! 真正的危险从来不是"W 与 HEAD 不一致"，而是"磁盘与 W 不一致"（扫描器没见过的内容
//! 被覆盖）。后者是逐路径的性质，而且 §7.3 那道门已经逐字检查它（kind/size/mtime 与 `W`
//! 必须相符）。所以现在：脏不脏只在报告里**告知**（`Report.dirty`），不再当闸门。
//!
//! # 它不动 head
//!
//! v1 只有一个 ref、没有分支，所以 `checkout` 改的是**工作树**，`main` 还在原地。
//! 分支切换是 M4 `branch` 的活。执行完必须再 `scan`，否则 `W` 说的还是旧话。
//!
//! # 已知不做的两件事
//!
//! - **类型互换**（同一路径上一边是文件、一边是目录）：记 `skipped` 并原样保留，
//!   不做"先删后建"。这种路径多半是用户或某个 bug 搞出来的，静默二选一太贵。
//! - **目录的 mode 变化**：`walk_pair` 对两侧同为目录的情况是直接递归，不看 mode；
//!   而 Windows 侧 mode 恒 0（§17 补记三），所以现在恢复目录权限位没有意义。

use std::fs;
use std::path::{Path, PathBuf};

use storage_engines::KvEngine;

use crate::error::{Error, Result};
use crate::store::keys::{self, Hash, Kind, ProjectId};
use crate::store::{build, cas, history, object, restore};

/// 一次 `checkout` 的三个开关。它们全是"敢不敢动手"的档位，拆成三个位置参数迟早会被
/// 调用方传反，所以收成一个结构体。
#[derive(Debug, Default, Clone, Copy)]
pub struct Opts {
    /// 真的执行。`false` = 只报一遍。
    pub apply: bool,
    /// 越过 FR-10 删除熔断。
    pub allow_mass_delete: bool,
}

/// 一次 `checkout` 的账。**计数是全的，样本是有界的**（与 `ChangeSet` 同一口径）。
#[derive(Debug, Default, Clone)]
pub struct Report {
    pub create: usize,
    pub mkdir: usize,
    pub overwrite: usize,
    pub delete: usize,
    pub rmdir: usize,
    /// 类型互换、以及"预告要删但删不掉"等原样留下的条目。
    pub skipped: usize,
    /// 工作树相对 HEAD 的未提交改动数。**只告知，不拦**（理由见模块头）。
    pub dirty: u32,
    pub deleted_bytes: u64,
    pub sample: Vec<String>,
    pub sample_more: usize,
    /// `true` = 真的动了手；`false` = 只报了一遍。
    pub applied: bool,
}

impl Report {
    /// 注意 `dirty` **不在**里面：工作树可以既是脏的、又不需要这次 checkout 动任何东西。
    pub fn is_noop(&self) -> bool {
        self.create + self.mkdir + self.overwrite + self.delete + self.rmdir + self.skipped == 0
    }

    fn note(&mut self, line: String) {
        if self.sample.len() < build::SAMPLE_MAX {
            self.sample.push(line);
        } else {
            self.sample_more += 1;
        }
    }
}

/// 把 `target` 版本落到工作树。`prefix = Some(p)` 时只动 `p` 那一棵子树。
pub fn checkout(
    db: &dyn KvEngine,
    pid: ProjectId,
    store: &cas::Cas,
    work_root: &Path,
    target: &Hash,
    prefix: Option<&str>,
    opts: Opts,
) -> Result<Report> {
    // 工作树那一侧来自 `plan`：它算出的树**没落盘**，所以清单必须跟着一起传下去。
    let cur = build::plan(db, pid)?;
    let commit = object::read_commit(db, pid, target)?;
    let (old, new, root) = roots(db, pid, &cur, &commit.tree, prefix)?;
    let ctx = Ctx {
        db,
        pid,
        store,
        work_root,
        cur: &cur,
        root: &root,
        old: &old,
        new: &new,
    };

    let mut rep = run(&ctx, false)?;
    build::check_mass_delete(
        rep.delete as u32,
        rep.deleted_bytes,
        cur.entries as u32,
        opts.allow_mass_delete,
    )?;
    if !opts.apply {
        return Ok(rep);
    }
    rep = run(&ctx, true)?;
    rep.applied = true;
    Ok(rep)
}

/// 两趟遍历共用的坐标。收成一个结构体不只是躲 `too_many_arguments`：
/// "只报"和"真干"必须看**同一份**树、写到**同一个** `work_root` 下——少一个参数就会
/// 出现"预告的是 A，干的是 B"。这个约束现在写在类型上。
struct Ctx<'a> {
    db: &'a dyn KvEngine,
    pid: ProjectId,
    store: &'a cas::Cas,
    work_root: &'a Path,
    cur: &'a build::Built,
    root: &'a str,
    old: &'a [object::TreeEntry],
    new: &'a [object::TreeEntry],
}

/// 一趟遍历。`apply = false` 只记账，`true` 边记账边落地。
///
/// 两趟共用同一个 `Job::on`：**分类只有一份**。早先"只数"和"真干"是两个函数，
/// 于是 `skipped` 的判定条件写了两遍——那是下一个静默偏差的产地。
fn run(ctx: &Ctx<'_>, apply: bool) -> Result<Report> {
    let mut job = Job {
        ctx,
        apply,
        rep: Report::default(),
    };
    build::walk_pair(
        ctx.db,
        ctx.pid,
        &ctx.cur.trees,
        ctx.root,
        ctx.old,
        ctx.new,
        &mut |kind, path, oe, ne| job.on(kind, path, oe, ne),
    )?;
    // 只告知，不拦：见模块头"曾经有的那道脏闸"。数字来自 `plan`，与 `status` 同一个口径。
    job.rep.dirty = ctx.cur.changes.add + ctx.cur.changes.modify + ctx.cur.changes.delete;
    Ok(job.rep)
}

/// 一趟遍历的执行者。
struct Job<'a> {
    ctx: &'a Ctx<'a>,
    apply: bool,
    rep: Report,
}

impl Job<'_> {
    fn on(
        &mut self,
        kind: build::ChangeKind,
        path: &str,
        oe: Option<&object::TreeEntry>,
        ne: Option<&object::TreeEntry>,
    ) -> Result<()> {
        let entry = ne.or(oe).expect("walk_pair 至少给一侧");
        self.rep.note(format!("{} {path}", kind.sign()));
        match kind {
            build::ChangeKind::Add => match entry.kind {
                Kind::Dir => {
                    self.rep.mkdir += 1;
                    self.mkdir(path)
                }
                _ => {
                    self.rep.create += 1;
                    self.write(path, entry)
                }
            },
            build::ChangeKind::Modify => {
                let from = oe.expect("Modify 两侧都在");
                if from.kind != entry.kind {
                    // 文件 ↔ 目录互换：v1 原样留着，不做"先删后建"。
                    self.rep.skipped += 1;
                    return Ok(());
                }
                self.rep.overwrite += 1;
                self.overwrite(path, from, entry)
            }
            build::ChangeKind::Delete => match entry.kind {
                Kind::Dir => {
                    self.rep.rmdir += 1;
                    self.rmdir(path)
                }
                _ => {
                    self.rep.delete += 1;
                    self.rep.deleted_bytes += entry.size;
                    self.remove_file(path)
                }
            },
        }
    }

    /// 下面每个落地步骤都遵守同一条：只报模式就直接返回；真执行时失败就让整个 `checkout`
    /// 报错中止。**不许吞错误继续走**——那会留下一个半新半旧的工作树还报告成功。
    fn mkdir(&mut self, path: &str) -> Result<()> {
        if self.apply {
            fs::create_dir_all(self.dest(path))?;
        }
        Ok(())
    }

    fn write(&mut self, path: &str, to: &object::TreeEntry) -> Result<()> {
        if !self.apply {
            return Ok(());
        }
        let abs = self.dest(path);
        // "新增"的路径上可能已经有个**扫描器没见过**的文件（未跟踪内容）。直接 rename 盖过去
        // 就是静默删除，所以只要那儿有东西，就照 §7.3 一样先问一遍。
        if std::fs::symlink_metadata(&abs).is_ok() {
            restore::check_local_is_recoverable(
                self.ctx.db,
                self.ctx.store,
                self.ctx.pid,
                self.ctx.work_root,
                path,
            )?;
        }
        create(self.ctx.db, self.ctx.pid, self.ctx.store, to, &abs)
    }

    fn overwrite(
        &mut self,
        path: &str,
        from: &object::TreeEntry,
        to: &object::TreeEntry,
    ) -> Result<()> {
        if !self.apply || from.kind == Kind::Dir {
            // 两侧都是目录且内容不同：子项会在递归里各自处理，这里不需要动目录本身。
            return Ok(());
        }
        let abs = self.dest(path);
        // §7.3：被覆盖的那一版必须先已经在历史里。
        restore::check_local_is_recoverable(
            self.ctx.db,
            self.ctx.store,
            self.ctx.pid,
            self.ctx.work_root,
            path,
        )?;
        remove_existing(&abs)?;
        create(self.ctx.db, self.ctx.pid, self.ctx.store, to, &abs)
    }

    fn remove_file(&mut self, path: &str) -> Result<()> {
        if !self.apply {
            return Ok(());
        }
        // 同一条门：删掉的本地版本也得取回来才行。
        restore::check_local_is_recoverable(
            self.ctx.db,
            self.ctx.store,
            self.ctx.pid,
            self.ctx.work_root,
            path,
        )?;
        fs::remove_file(self.dest(path))?;
        Ok(())
    }

    fn rmdir(&mut self, path: &str) -> Result<()> {
        if !self.apply {
            return Ok(());
        }
        // 后序遍历保证这一刻子项已经处理完；还在（夹了未跟踪文件）就原样留着并**改账**。
        match fs::remove_dir(self.dest(path)) {
            Ok(()) => Ok(()),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound) => {
                self.rep.rmdir -= 1;
                self.rep.skipped += 1;
                Ok(())
            }
            Err(_) => {
                self.rep.rmdir -= 1;
                self.rep.skipped += 1;
                self.rep.note(format!("! {path} 非空或删不掉，已原样留下"));
                Ok(())
            }
        }
    }

    fn dest(&self, path: &str) -> PathBuf {
        self.ctx
            .work_root
            .join(path.replace('/', std::path::MAIN_SEPARATOR_STR))
    }
}

/// 按目标条目写出一个文件 / 建一个目录 / 建一个链接。
fn create(
    db: &dyn KvEngine,
    pid: ProjectId,
    store: &cas::Cas,
    to: &object::TreeEntry,
    dest: &Path,
) -> Result<()> {
    match to.kind {
        Kind::Dir => Ok(fs::create_dir_all(dest)?),
        Kind::File => {
            let blob = build::read_blob(db, pid, &to.hash)?;
            store.read_blob(&blob, dest)?;
            restore::apply_owner_exec(dest, to.mode)
        }
        Kind::Symlink => {
            let blob = build::read_blob(db, pid, &to.hash)?;
            let target = restore::link_target(&blob, store)?;
            restore::make_symlink(&target, dest)
        }
    }
}

/// 覆盖之前先把旧的挪开：链接指向的目标不是它自己，`symlink_file` 撞已存在路径会失败。
fn remove_existing(abs: &Path) -> Result<()> {
    match fs::symlink_metadata(abs) {
        Ok(md) if md.file_type().is_dir() && !md.file_type().is_symlink() => Ok(()),
        Ok(_) => Ok(fs::remove_file(abs)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io(e)),
    }
}

/// 解析出要比对的两份**根清单**，以及样本里路径的起始前缀。
fn roots(
    db: &dyn KvEngine,
    pid: ProjectId,
    cur: &build::Built,
    target_tree: &Hash,
    prefix: Option<&str>,
) -> Result<(Vec<object::TreeEntry>, Vec<object::TreeEntry>, String)> {
    let clean = prefix.unwrap_or("").trim_matches('/');
    if clean.is_empty() {
        return Ok((
            cur.trees.entries(db, pid, &cur.tree)?,
            object::read_tree(db, pid, target_tree)?,
            String::new(),
        ));
    }
    // 目标侧没有这一层 = 用户打错路径，必须报错。
    let want = history::resolve(db, pid, target_tree, clean)?.ok_or_else(|| Error::NoSuchPath {
        rev: keys::short(target_tree),
        path: clean.to_string(),
    })?;
    let new = dir_kids(db, pid, &history::TreeStore::default(), &want, clean)?;
    // 工作树侧没有 = 正常，那一棵子树全是新增。
    let Some(have) = history::resolve_in(db, pid, &cur.trees, &cur.tree, clean)? else {
        return Ok((Vec::new(), new, clean.to_string()));
    };
    Ok((
        dir_kids(db, pid, &cur.trees, &have, clean)?,
        new,
        clean.to_string(),
    ))
}

/// `--prefix` 只接受目录：`checkout` 的比对单位是"一层目录"，单文件那是 `restore` 的活。
/// 允许文件前缀的话，就得给"路径根"再造一套第二语义，两条命令的边界会糊掉。
fn dir_kids(
    db: &dyn KvEngine,
    pid: ProjectId,
    trees: &history::TreeStore,
    e: &object::TreeEntry,
    path: &str,
) -> Result<Vec<object::TreeEntry>> {
    if e.kind != Kind::Dir {
        return Err(Error::NotYet {
            what: format!(
                "`checkout --prefix {path}` 指向的是{}，不是目录",
                kind_word(e.kind)
            ),
            milestone: "单个文件请用 `restore`，它一次处理一个路径",
        });
    }
    trees.entries(db, pid, &e.hash)
}

fn kind_word(k: Kind) -> &'static str {
    match k {
        Kind::File => "文件",
        Kind::Dir => "目录",
        Kind::Symlink => "符号链接",
    }
}
