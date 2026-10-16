//! 把历史里的一个路径写回工作树。**§7.3 铁律的家**。
//!
//! # 铁律在这里的形状
//!
//! 「任何工作树删除在落地前，必须确认该 blob 的块已在 CAS 且已进某个 commit」——
//! 覆盖一个已存在的本地文件就是一次删除。所以 `--force` 不是"闭眼覆盖"，它只跳过
//! "确认目标存在"这一步，**不跳过**这两条：
//!
//! 1. 本地这一版必须**已经被扫描过**（`W` 里有它的行）。没扫过的内容我们连它是什么都不知道。
//! 2. 它对应的 blob 必须**在库里**（`B` 清单在）且**块都在磁盘上**（`Cas::missing` 为空）。
//!
//! 第 2 条用逐块 `stat` 而不是逐块重算 hash：重算是 `verify`/`gc --fsck` 的活，放进
//! 一次 `restore` 会把 10 GB 文件变成分钟级。
//!
//! # 为什么写不出"半个文件"
//!
//! `Cas::read_blob` 是流式写 tmp、**边写边校验**、全对才 `rename`。所以要么拿到完整且
//! hash 相符的文件，要么原文件根本没被动过——恢复路径不需要"失败后回滚"这种二次保险。
//!
//! # 两个平台缺口（如实记，不假装过了）
//!
//! - **Windows 建符号链接要权限或开发者模式**，失败会以 `Error::Io`（os error 1314）冒出来。
//! - **Windows 侧 `mode` 恒 0**（`worktree::mode_of` 非 unix 返回 0），所以 owner 可执行位
//!   只在 unix 上恢复；Windows 上不设置也不报错，因为那里根本没有这个概念可还原。

use std::io::Read as _;
use std::path::{Path, PathBuf};

use storage_engines::KvEngine;

use crate::error::{Error, Result};
use crate::store::keys::{self, Hash, Kind, ProjectId};
use crate::store::{build, cas, history, object, worktree};

#[derive(Debug)]
pub struct Restored {
    pub dest: PathBuf,
    pub kind: Kind,
    /// 写出的字节数（符号链接是目标文本的长度）。
    pub bytes: u64,
    pub blocks: usize,
    /// 符号链接的目标文本；文件为 `None`。
    pub target: Option<String>,
    /// §7.3 的门是否真的检查过本地旧版本（目标本来不存在时不需要检查）。
    pub gate_checked: bool,
}

/// 从 `commit` 取 `path` 写到工作树。
///
/// * `dest = None` ⇒ 写到 `<work_root>/<path>`，这是**会覆盖本地内容**的那条路，过 §7.3。
/// * `dest = Some(d)` ⇒ 写到别处，不碰工作树 ⇒ 不需要那道门（也不受它保护）。
#[allow(clippy::too_many_arguments)]
pub fn restore(
    db: &dyn KvEngine,
    pid: ProjectId,
    store: &cas::Cas,
    work_root: &Path,
    commit: &Hash,
    path: &str,
    dest: Option<&Path>,
    force: bool,
) -> Result<Restored> {
    let c = object::read_commit(db, pid, commit)?;
    let e = history::resolve(db, pid, &c.tree, path)?.ok_or_else(|| Error::NoSuchPath {
        rev: keys::short(commit),
        path: path.to_string(),
    })?;

    let dest = match dest {
        Some(d) => d.to_path_buf(),
        None => work_root.join(path.replace('/', std::path::MAIN_SEPARATOR_STR)),
    };
    let mut gate_checked = false;
    if dest.exists() {
        if !force {
            return Err(Error::LocalExists { path: dest });
        }
        // 覆盖之前先确认"被覆盖的这一版"取回来了才谈得上覆盖。
        check_local_is_recoverable(db, store, pid, work_root, path)?;
        gate_checked = true;
    }

    // blob 只能在各自的分支里读：目录条目的 hash 是 **tree**，拿它去查 `B` 会报成
    // "blob 对象缺失"，把一句"这活归 checkout"的实话变成一次看起来像磁盘损坏的错误。
    match e.kind {
        Kind::File => {
            let blob = build::read_blob(db, pid, &e.hash)?;
            store.read_blob(&blob, &dest)?;
            apply_owner_exec(&dest, e.mode)?;
            Ok(Restored {
                dest,
                kind: e.kind,
                bytes: blob.size,
                blocks: blob.blocks.len(),
                target: None,
                gate_checked,
            })
        }
        Kind::Symlink => {
            let blob = build::read_blob(db, pid, &e.hash)?;
            let target = link_target(&blob, store)?;
            make_symlink(&target, &dest)?;
            let bytes = target.len() as u64;
            Ok(Restored {
                dest,
                kind: e.kind,
                bytes,
                blocks: blob.blocks.len(),
                target: Some(target),
                gate_checked,
            })
        }
        Kind::Dir => Err(Error::NotYet {
            what: format!("恢复目录（{path}）"),
            milestone: "那是 `checkout` 的活：它要一次处理整棵子树与删除",
        }),
    }
}

/// §7.3：确认工作树里**当前**这一版已经在历史里、且块都在。
///
/// `pub` 是给 `checkout` 用的——它要覆盖/删除的每一个文件都过这同一道门。**两道门必须是
/// 同一个函数**：`checkout` 是本项目唯一会批量删工作树文件的命令，给它另写一份检查，
/// 就是给"哪一版算已备份"留下第二个定义。
///
/// `path_in_w` 是 `W` 键里那条**斜杠路径**（不是磁盘路径）：调用方手里通常只有前者，
/// 而 `abs` 必须由 `work_root` 拼出来，不能让每个调用方各拼一次。
#[allow(clippy::too_many_arguments)] // 六个都是"去哪、看哪一版"的坐标，收成一个结构体反而更难读
pub fn check_local_is_recoverable(
    db: &dyn KvEngine,
    store: &cas::Cas,
    pid: ProjectId,
    work_root: &Path,
    path: &str,
) -> Result<()> {
    let abs = work_root.join(path);
    let not_in = |why: String| Error::NotInHistory {
        path: abs.clone(),
        why,
    };
    let raw = {
        let tx = db.begin();
        let got = tx.get(&keys::worktree_key(pid, path));
        tx.commit();
        got
    }
    .ok_or_else(|| {
        not_in(String::from(
            "工作树里这一条没被扫描过，所以它从没进过任何 commit",
        ))
    })?;
    let Some(bytes) = keys::strip(&raw).ok() else {
        return Err(not_in(String::from(
            "W 记录的信封标签不对，等于没有可信的旧状态",
        )));
    };
    let Ok(e) = postcard::from_bytes::<worktree::WtEntry>(bytes) else {
        return Err(not_in(String::from("W 记录解不开")));
    };

    // 先确认**磁盘上这一版就是 W 记的那一版**（与扫描器同一把尺：`logical_size` + `mtime_of`，
    // 两个都从 `worktree` import，绝不在这里另算一遍）。少了这一步，"扫过之后又改过"的内容
    // 会被当成"已经在历史里"直接覆盖掉——而那恰好是 §7.3 要拦的事。
    let md = std::fs::symlink_metadata(&abs)?;
    let kind = if md.is_symlink() {
        Kind::Symlink
    } else if md.is_dir() {
        Kind::Dir
    } else {
        Kind::File
    };
    if e.kind != kind
        || worktree::logical_size(&md, kind) != e.size
        || worktree::mtime_of(&md) != e.mtime_ns
    {
        return Err(not_in(String::from(
            "磁盘上的内容比最后一次扫描更新（kind/size/mtime 不符）",
        )));
    }

    let blob = build::read_blob(db, pid, &e.blob_hash)
        .map_err(|_| not_in(format!("它的 blob {} 不在库里", keys::short(&e.blob_hash))))?;
    let missing = store.missing(&blob);
    if !missing.is_empty() {
        return Err(not_in(format!(
            "它的块有 {}/{} 个不在磁盘上（例如 {}）",
            missing.len(),
            blob.blocks.len(),
            keys::short(&missing[0])
        )));
    }
    Ok(())
}

/// 把符号链接的目标文本从 CAS 取回来。**`show` 也用它**：既然目标已经进了历史，
/// 那"这条链接指向哪儿"就该能直接答出来，不该再回一句"只能看到一个 hash"。
///
/// 只给链接用：它一次物化整段字节，所以有 4 KiB 硬上限（文件走 `Cas::read_blob` 的流式路径）。
pub fn link_target(blob: &cas::Blob, store: &cas::Cas) -> Result<String> {
    if blob.size > 4096 {
        return Err(Error::CorruptBlock {
            hash: keys::short(&blob.hash),
            path: PathBuf::from("(符号链接清单)"),
            why: format!("符号链接目标长 {} 字节，超过 4096 的界限", blob.size),
        });
    }
    let tmp = store.temp_for("link");
    let res = (|| -> Result<String> {
        store.read_blob(blob, &tmp)?;
        let mut buf = Vec::new();
        std::fs::File::open(&tmp)?.read_to_end(&mut buf)?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    })();
    let _ = std::fs::remove_file(&tmp);
    res
}

pub fn make_symlink(target: &str, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, dest)?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(target, dest).map_err(|e| {
        Error::Io(std::io::Error::other(format!(
            "Windows 建符号链接失败（目标 {target:?}）：需要开发者模式或管理员权限。原始错误：{e}"
        )))
    })?;
    Ok(())
}

/// §8：只同步 owner 可执行位，不还原整串 mode。
pub fn apply_owner_exec(dest: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let exec = mode & 0o100 != 0;
        std::fs::set_permissions(
            dest,
            std::fs::Permissions::from_mode(if exec { 0o755 } else { 0o644 }),
        )?;
    }
    #[cfg(not(unix))]
    {
        let _ = (dest, mode); // Windows 没有 POSIX 位；见模块头那条缺口
    }
    Ok(())
}
