//! 站点级配置：`<root>/.twosync/config.toml`。
//!
//! # 只存"今天真有人读"的键
//!
//! `config` 最容易长成一个摆设：键存进去了，读的人却还没写，于是"配了没生效"变成最难查
//! 那一类 bug。所以现在只有两个键，且各自有明确的消费者：
//!
//! - `engine` —— 每条命令打开存储基座时的默认引擎（`Cli::store`）。命令行 `--engine` 优先。
//! - `block_size` —— CAS 的定长块大小（FR-5，默认 4 MiB）。`Cli::block_size` 给命令层，
//!   所有 `Cas::new` 都从这里取值。
//!
//! FR-9 的配额/保留策略、FR-12 的排除规则**故意还没加**：它们的消费者（`gc`、扫描器的
//! gitignore 追加）要等 M1。加进来就是先写一个没人读的键。
//!
//! # 换 `block_size` 为什么不动已有数据
//!
//! 每个 blob 的清单里都写着自己的 `block_size`，块文件按内容 hash 命名，所以读旧 blob
//! 完全不依赖当前配置值 ⇒ 改它只影响**之后**新建的 blob。真正要紧的是别把它调小：
//! 10 GB 文件按 64 KiB 切是 16 万个块，清单和 refcnt 行都会爆，所以下限卡死在 64 KiB。

use std::fs;

use serde::{Deserialize, Serialize};
use storage_engines::EngineKind;

use crate::error::{Error, Result};
use crate::utils::site_dir;

/// 默认块大小与 `cas::DEFAULT_BLOCK` 同值。这里重述一遍是为了让"配置里没写"和
/// "写了默认值"两种情况在 `config get` 里给出同一个答案。
pub const DEFAULT_BLOCK: u64 = 4 << 20;
const MIN_BLOCK: u64 = 64 << 10;
const MAX_BLOCK: u64 = 256 << 20;

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// 存成字符串而不是 `EngineKind`：`EngineKind` 是引擎那边的类型，不该出现在我们的
    /// 落盘格式里。这里自己解析，引擎换了枚举也不会把配置文件带坏。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_size: Option<u64>,
}

impl Config {
    /// 全部可配键。`config get`（不带参数）与"未知键"的报错都从这里出。
    pub const KEYS: &'static [&'static str] = &["engine", "block_size"];

    /// 缺文件 = 全默认（首次 `init` 之前也会走到这里）。**解析失败必须报错**，不许当成
    /// 空表再写回去——那和注册表那条是同一个道理：一次坏文件清空全部配置。
    pub fn load(root: &std::path::Path) -> Result<Self> {
        let file = Self::path(root);
        if !file.exists() {
            return Ok(Self::default());
        }
        let ctx = |what: String| Error::Registry(format!("{}: {what}", file.display()));
        let text = fs::read_to_string(&file).map_err(|e| ctx(e.to_string()))?;
        toml::from_str::<Self>(&text).map_err(|e| ctx(e.to_string()))
    }

    /// 原子写：同目录 tmp + rename（FR-13 的纪律，配置也要守）。
    pub fn save(&self, root: &std::path::Path) -> Result<()> {
        let file = Self::path(root);
        let ctx = |what: String| Error::Registry(format!("{}: {what}", file.display()));
        let text = toml::to_string_pretty(self).map_err(|e| ctx(e.to_string()))?;
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent).map_err(|e| ctx(e.to_string()))?;
        }
        let tmp = file.with_extension("toml.tmp");
        fs::write(&tmp, &text).map_err(|e| ctx(e.to_string()))?;
        fs::rename(&tmp, &file).map_err(|e| ctx(e.to_string()))?;
        Ok(())
    }

    pub fn path(root: &std::path::Path) -> std::path::PathBuf {
        site_dir(root).join("config.toml")
    }

    pub fn get(&self, key: &str) -> Option<String> {
        match key {
            "engine" => self.engine.clone(),
            "block_size" => self.block_size.map(|b| b.to_string()),
            _ => None,
        }
    }

    /// 生效值 + 来源。`config get` 打印的是这个：光看文件看不出"没写"和"写了默认值"的区别。
    pub fn effective(&self, key: &str) -> Result<(String, &'static str)> {
        match key {
            "engine" => match &self.engine {
                Some(e) => Ok((e.clone(), "配置文件")),
                None => Ok((EngineKind::LsmTree.as_str().to_string(), "内置默认")),
            },
            "block_size" => match self.block_size {
                Some(b) => Ok((b.to_string(), "配置文件")),
                None => Ok((DEFAULT_BLOCK.to_string(), "内置默认")),
            },
            other => Err(unknown_key(other)),
        }
    }

    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        match key {
            "engine" => {
                let kind =
                    EngineKind::parse(value).ok_or_else(|| Error::BadInput(bad_engine(value)))?;
                self.engine = Some(kind.as_str().to_string());
            }
            "block_size" => {
                let n = parse_size(value).ok_or_else(|| {
                    Error::BadInput(format!("块大小看不懂：{value:?}（例 4MiB / 4194304）"))
                })?;
                if !(MIN_BLOCK..=MAX_BLOCK).contains(&n) {
                    return Err(Error::BadInput(format!(
                        "块大小 {n} 超出范围 [{MIN_BLOCK}, {MAX_BLOCK}]（64 KiB ~ 256 MiB）"
                    )));
                }
                self.block_size = Some(n);
            }
            other => return Err(unknown_key(other)),
        }
        Ok(())
    }

    /// 返回 `false` = 这个键本来就没写（unset 一个没设过的键不是错误，但要说清）。
    pub fn unset(&mut self, key: &str) -> Result<bool> {
        match key {
            "engine" => Ok(self.engine.take().is_some()),
            "block_size" => Ok(self.block_size.take().is_some()),
            other => Err(unknown_key(other)),
        }
    }

    pub fn engine(&self) -> Result<EngineKind> {
        match &self.engine {
            Some(s) => EngineKind::parse(s).ok_or_else(|| Error::BadInput(bad_engine(s))),
            None => Ok(EngineKind::LsmTree),
        }
    }

    pub fn block_size(&self) -> u64 {
        self.block_size.unwrap_or(DEFAULT_BLOCK)
    }
}

fn unknown_key(key: &str) -> Error {
    Error::BadInput(format!(
        "没有这个配置键：{key:?}；可配的键是 {}",
        Config::KEYS.join(" / ")
    ))
}

fn bad_engine(value: &str) -> String {
    format!("未知引擎 {value:?}；可用：lsm_tree / bplus_tree / bitcask / memory")
}

/// `4194304` / `4MiB` / `4M` / `512K` / `1G`。大小写不敏感，只认 K/M/G 三个量级。
fn parse_size(s: &str) -> Option<u64> {
    let t = s.trim();
    let upper = t.to_ascii_uppercase();
    for (suffix, mul) in [
        ("MIB", 1u64 << 20),
        ("KIB", 1u64 << 10),
        ("GIB", 1u64 << 30),
    ] {
        if let Some(num) = upper.strip_suffix(suffix) {
            return num.trim().parse::<u64>().ok().map(|n| n * mul);
        }
    }
    for (suffix, mul) in [("M", 1u64 << 20), ("K", 1u64 << 10), ("G", 1u64 << 30)] {
        if let Some(num) = upper.strip_suffix(suffix) {
            return num.trim().parse::<u64>().ok().map(|n| n * mul);
        }
    }
    t.parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 后缀换算错一位，块大小就差 1024 倍——而它不会报错，只会让块数暴涨或块过大。
    #[test]
    fn size_suffixes_parse() {
        assert_eq!(parse_size("4194304"), Some(4 << 20));
        assert_eq!(parse_size("4MiB"), Some(4 << 20));
        assert_eq!(parse_size("4mib"), Some(4 << 20));
        assert_eq!(parse_size("4M"), Some(4 << 20));
        assert_eq!(parse_size("512K"), Some(512 << 10));
        assert_eq!(parse_size(" 1 G "), Some(1 << 30));
        assert_eq!(parse_size("0"), Some(0), "0 能解析出来，由范围检查去拒");
        assert_eq!(parse_size("abc"), None);
        assert_eq!(parse_size(""), None);
    }

    #[test]
    fn set_validates_and_roundtrips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        assert_eq!(cfg.engine().unwrap(), EngineKind::LsmTree);
        assert_eq!(cfg.block_size(), DEFAULT_BLOCK);
        // "没写"和"写了默认值"在 effective() 里要能分辨。
        assert_eq!(cfg.effective("block_size").unwrap().1, "内置默认");

        cfg.set("block_size", "1MiB").unwrap();
        cfg.set("engine", "bplus_tree").unwrap();
        assert_eq!(cfg.effective("block_size").unwrap().1, "配置文件");
        cfg.save(dir.path()).unwrap();

        let back = Config::load(dir.path()).unwrap();
        assert_eq!(back, cfg);
        assert_eq!(back.block_size(), 1 << 20);
        assert_eq!(back.engine().unwrap().as_str(), "bplus_tree");
    }

    /// 未知键、坏值都必须拒：**存进去一个没人读的键，比报错难查得多**。
    #[test]
    fn unknown_keys_and_bad_values_are_rejected() {
        let mut cfg = Config::default();
        assert!(matches!(cfg.set("blocksize", "1"), Err(Error::BadInput(_))));
        assert!(matches!(
            cfg.set("engine", "sqlite"),
            Err(Error::BadInput(_))
        ));
        assert!(
            matches!(cfg.set("block_size", "8"), Err(Error::BadInput(_))),
            "低于 64 KiB 该拒"
        );
        assert!(
            matches!(cfg.set("block_size", "1G"), Err(Error::BadInput(_))),
            "高于 256 MiB 该拒"
        );
        assert!(cfg.get("nope").is_none());
        assert_eq!(cfg, Config::default(), "被拒的 set 一个字段都不许改");
    }

    /// 坏文件不许被当成"没有配置"：那会让下一次 save 把整份配置冲掉。
    #[test]
    fn corrupt_file_is_reported_not_treated_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = Config::path(dir.path());
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "this is = not = toml [[[").unwrap();
        let err = Config::load(dir.path()).expect_err("坏文件必须红");
        assert!(
            err.to_string().contains("config.toml"),
            "错误里要带是哪个文件：{err}"
        );
    }

    #[test]
    fn unset_reports_whether_it_actually_removed_something() {
        let mut cfg = Config::default();
        assert!(!cfg.unset("engine").unwrap(), "本来就没写");
        cfg.set("engine", "memory").unwrap();
        assert!(cfg.unset("engine").unwrap());
        assert_eq!(cfg.engine().unwrap(), EngineKind::LsmTree, "回到内置默认");
        assert!(matches!(cfg.unset("nope"), Err(Error::BadInput(_))));
    }
}
