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
use storage_engines::EngineKind;

/// §10.8：让**同一批用例**换引擎跑。名字来自环境变量，默认还是 `lsm_tree`（生产默认值）。
///
/// ```text
/// TWOSYNC_TEST_ENGINE=bplus_tree cargo test
/// ```
///
/// 为什么用环境变量而不是给每个用例加参数：38 个站点构造点手写两遍会变成"两批不同步的用例"，
/// 而矩阵要的恰恰是**同一批**。环境变量是这里唯一的开关，名字不认识就直接红——
/// 拼错引擎名却照样跑完全绿，是这种台架最坏的失效方式。
///
/// **不适用**的三个文件：`engine_smoke.rs` / `engine_envelope.rs` / `utils_metrics.rs` ——
/// 那些用例的主题就是某个具体引擎的脾气，换成别的引擎它们就没在测任何东西了。
pub fn engine() -> EngineKind {
    match std::env::var(ENV) {
        Ok(s) => by_name(&s),
        Err(_) => EngineKind::LsmTree,
    }
}

/// 按名字取引擎（`engine_matrix.rs` 那张表要逐个点名，不能靠环境变量）。
pub fn by_name(name: &str) -> EngineKind {
    EngineKind::parse(name)
        .unwrap_or_else(|| panic!("传入的引擎名 {name:?} 不认识（试 {ALL_ENGINES:?}）"))
}

/// 四个引擎一个不漏的名单。**只有这一份**：以前 `engine_matrix.rs` 与这里各写一份，
/// 那种复制会随引擎增加而漂移（加了一个引擎，矩阵跑了、开关的报错文本没跟着更新）。
pub const ALL_ENGINES: [&str; 4] = ["lsm_tree", "bplus_tree", "bitcask", "memory"];

/// 把 `engine()` 选中的那个引擎**写进站点配置**（`.twosync/config`）。
///
/// 为什么需要：`open_store(root, common::engine())` 只管用例自己那一份句柄，而被 spawn 出来的
/// `twosync agent` 子进程是**另一个进程**——它按"命令行 > 配置文件 > 内置默认"自己解析引擎
/// （那条优先级只实现一次，就在 `Cli::store`）。不写配置就会出现"本端 bplus_tree、对端
/// lsm_tree"：两边的库根本不在同一个目录下（`db/<engine>/`），于是会话看起来"什么都没收到"，
/// 看着像跨引擎的 bug，其实是台架没接通。
///
/// **默认那批一个字节都不写**：`engine` 没写过 ⇒ `version` 报的来源是"内置默认"，
/// 那是另一条用例要验的形状，不能被这个开关顺手改掉。
pub fn pin_engine_in_config(root: &Path) {
    let want = engine();
    if want == EngineKind::LsmTree {
        return; // 默认：不写配置，保持"配置文件里没这一行"
    }
    let mut cfg = twosync::cli::config::Config::default();
    cfg.set("engine", want.as_str())
        .unwrap_or_else(|e| panic!("把 {want} 写进配置失败：{e}"));
    cfg.save(root).unwrap_or_else(|e| panic!("保存 {root:?} 的配置失败：{e}"));
}

const ENV: &str = "TWOSYNC_TEST_ENGINE";

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
