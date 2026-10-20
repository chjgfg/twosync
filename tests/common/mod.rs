//! 测试台架里**共用**的那一件：让一个文件"这一刻读不了"。
//!
//! 为什么放在这里而不是各测试文件里各抄一份：这个 helper 的正确性依赖平台机制
//! （Windows 的 `share_mode(0)` / 类 Unix 的 `chmod 000`），要改就得一起改。
//! 抄三份的后果是"某一份悄悄不灵了而另外两份还在"，而那正是 §7.2 那批用例最不该有的形状
//! ——它们验的是"读不到的项不许拖垮一轮"，量具自己失效就等于什么都没验。
//!
//! 用法：`mod common;` 然后 `let guard = common::lock_out(path);`，`drop(guard)` 解锁。
//! 守卫必须活到用例结束之后再 drop（临时目录的析构顺序在 Windows 上会撞上"句柄没放就删文件"）。

#![allow(dead_code)] // 每个测试二进制只用得到其中一部分，不许为此报错

use std::fs;
use std::path::{Path, PathBuf};

/// 见模块头。字段按平台分两种，`Drop` 里各回各的家。
pub struct Locked {
    path: PathBuf,
    /// Windows：占着文件的句柄，drop 就是解锁。
    #[cfg(windows)]
    held: fs::File,
    /// 类 Unix：原来的权限位，drop 时写回去。
    #[cfg(not(windows))]
    saved: fs::Permissions,
}

/// 锁住一个**已存在**的文件。
///
/// 类 Unix 那一半在 root 下不灵（root 无视权限位）——那时候用例会红在
/// "deferred 计数是 0"，而不是假绿，这是它比"手工造一条 R 行"强的地方。
pub fn lock_out(path: &Path) -> Locked {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let held = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(path)
            .unwrap();
        Locked {
            path: path.to_path_buf(),
            held,
        }
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let saved = fs::metadata(path).unwrap().permissions();
        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
        Locked {
            path: path.to_path_buf(),
            saved,
        }
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        #[cfg(not(windows))]
        fs::set_permissions(&self.path, self.saved.clone()).unwrap();
        #[cfg(windows)]
        {
            // 解锁本身不需要写代码（`held` 一 drop 句柄就放了）。这里读一次句柄、一次路径，
            // 为的是自证**锁下对了文件**：两个长度对不上就是这个台架在骗自己。
            let by_handle = self.held.metadata().map(|m| m.len()).unwrap_or(0);
            let by_path = fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
            assert_eq!(by_handle, by_path, "锁的句柄与记录的路径不是同一个文件");
        }
    }
}
