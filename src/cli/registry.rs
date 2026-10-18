//! §16.1 的项目注册表：`<root>/.twosync/projects.toml`。
//!
//! 它是**站点级配置**，不进 KV 引擎——引擎里的键要用 `project_id`，而 `project_id`
//! 由名字派生（`store::keys::project_id`），所以注册表必须先于任何键存在。
//!
//! # 手工验证（`cargo test` 之外，跑给人看的那一遍）
//!
//! ```text
//! cargo fmt && cargo clippy --all-targets && cargo test
//! rm -rf /tmp/ts2 && cargo run -q -- --root /tmp/ts2 init
//! cargo run -q -- --root /tmp/ts2 project add trading-arb
//! cargo run -q -- --root /tmp/ts2 project add data-lab --path lab/data
//! cargo run -q -- --root /tmp/ts2 project list
//! cargo run -q -- --root /tmp/ts2 project use data-lab && cargo run -q -- --root /tmp/ts2 project list
//! cargo run -q -- --root /tmp/ts2 project add trading-arb     # 应报"已存在"，非零退出
//! cat /tmp/ts2/.twosync/projects.toml                          # 看落盘长什么样
//! ```
//!
//! 期望：`list` 里 `*` 从 `trading-arb` 移到 `data-lab`；两个 id 每次跑都一样（这就是派生
//! 而非自增的意义）；重名那次退出码非 0，且 `projects.toml` 里原条目还在。

use std::{collections::BTreeMap, fs, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
    error::{Error, Result},
    utils::projects_file,
};

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Project {
    pub name: String,
    /// 相对站点 root、一律用 `/` 分隔的工作树目录。
    /// 存相对路径而不是绝对路径：换机器/换挂载点不用重写配置（§8 的跨平台纪律）。
    pub path: String,
    /// 对端目录，M1 才用；None = 尚未配对（绝不自动在对端建目录，§9.5.3）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_path: Option<String>,
    /// 在 FR-12 内置排除之外追加的 gitignore 规则。
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// `BTreeMap` 而非 `HashMap`：`list` 输出必须稳定，否则每次跑都换顺序、diff 不了。
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Registry {
    #[serde(default)]
    pub current: Option<String>,
    #[serde(default)]
    pub projects: BTreeMap<String, Project>,
}

impl Registry {
    /// 文件不存在 ⇒ 空表（首次 `init` 之前也会走到这里）。**解析失败必须报错，不许当成
    /// 空表**——否则"读→改→写"一圈下来，一次坏文件就把整份注册表静默清空。
    pub fn load(root: &Path) -> Result<Self> {
        let file = projects_file(root);
        if !file.exists() {
            return Ok(Self::default());
        }
        let ctx = |what: String| Error::Registry(format!("{}: {what}", file.display()));
        let text = fs::read_to_string(&file).map_err(|e| ctx(e.to_string()))?;
        toml::from_str::<Self>(&text).map_err(|e| ctx(e.to_string()))
    }

    /// 原子写：同目录 tmp + rename（FR-13 的纪律，注册表也要守）。
    pub fn save(&self, root: &Path) -> Result<()> {
        let file = projects_file(root);
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

    /// 注册项目。重名直接拒——`project_id` 由名字派生，改名等于换一个新项目（§16.1）。
    pub fn add(&mut self, name: &str, path: &str) -> Result<&Project> {
        check_name(name)?;
        check_rel_path(path).map_err(Error::BadInput)?;
        if self.projects.contains_key(name) {
            let err = format!("项目 {name} 已存在；改名等于换一个新项目（id 由名字派生）");
            return Err(Error::Registry(err));
        }
        let project = Project {
            name: name.to_string(),
            path: path.to_string(),
            peer_path: None,
            exclude: Vec::new(),
        };
        self.projects.insert(name.to_string(), project);
        if self.current.is_none() {
            self.current = Some(name.to_string()); // 第一个项目自动选中，省一次 `use`
        }
        Ok(&self.projects[name])
    }

    /// 摘掉一个项目。**只动注册表**：工作树、历史、CAS 块一概不碰（§16.1）。
    ///
    /// `current` 指向它时一并清空，**不许**自动改选别的项目——下一条命令作用在哪个目录上
    /// 必须是用户明确选的，静默换一个项目就是往错的项目里提交。
    pub fn remove(&mut self, name: &str) -> Result<Project> {
        let p = self
            .projects
            .remove(name)
            .ok_or_else(|| Error::Registry(format!("没有项目 {name}")))?;
        if self.current.as_deref() == Some(name) {
            self.current = None;
        }
        Ok(p)
    }

    /// 记下"这个项目的对端站点根"（§9.5.3 的配对）。**原样存**，不按本端的规则校验：
    /// 那是**对端**的路径，对端若是 Linux 就得写 `/home/...`，拿 `check_rel_path` 管它是错的。
    pub fn pair(&mut self, name: &str, peer_root: &str) -> Result<&mut Project> {
        let p = self
            .projects
            .get_mut(name)
            .ok_or_else(|| Error::Registry(format!("没有项目 {name}；先 project add")))?;
        p.peer_path = Some(peer_root.to_string());
        Ok(p)
    }

    /// 取消配对：只清记录。历史、CAS、工作树一概不动（与 `remove` 同一个口径）。
    pub fn unpair(&mut self, name: &str) -> Result<bool> {
        let p = self
            .projects
            .get_mut(name)
            .ok_or_else(|| Error::Registry(format!("没有项目 {name}")))?;
        Ok(p.peer_path.take().is_some())
    }

    pub fn select(&mut self, name: &str) -> Result<()> {
        if !self.projects.contains_key(name) {
            let err = format!("没有项目 {name}；先 project add");
            return Err(Error::Registry(err));
        }
        self.current = Some(name.to_string());
        Ok(())
    }

    pub fn current(&self) -> Result<&Project> {
        let name = self.current.as_ref().ok_or_else(|| {
            Error::Registry("还没选项目：先跑 twosync project use <name>".to_string())
        })?;
        self.projects
            .get(name)
            .ok_or_else(|| Error::Registry(format!("current 指向不存在的项目 {name}")))
    }
}

fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err(Error::BadInput(format!("项目名不合法：{name:?}")));
    }
    Ok(())
}

/// 路径必须是相对、`/` 分隔、不含 `..`。§9.4.1 的穿越防线，本地配置这一层就先挡住。
///
/// `pub(crate)` 且**只返回消息字符串**：这条规则同时被注册表（`project add`）和命令行
/// （`mv` / `checkout --prefix`）用，两处都想带上"是哪个参数"的前缀。规则留一份、
/// 消息各自包，免得两份校验器对同一个路径给出两种答案。
pub(crate) fn check_rel_path(path: &str) -> std::result::Result<(), String> {
    if path.is_empty() || path.starts_with('/') || path.contains('\\') {
        return Err(format!("路径必须是 / 分隔的相对路径：{path:?}"));
    }
    if Path::new(path).is_absolute() {
        return Err(format!("路径不许是绝对路径：{path:?}"));
    }
    if path.split('/').any(|seg| seg == "..") {
        return Err(format!("路径不许含 ..：{path:?}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn load_missing_file_is_empty_not_fatal() {
        let dir = root();
        let reg = Registry::load(dir.path()).expect("没有注册表该给空表");
        assert!(reg.projects.is_empty());
        assert!(reg.current.is_none());
    }

    #[test]
    fn add_then_reload_roundtrips() {
        let dir = root();
        let mut reg = Registry::default();
        reg.add("trading-arb", "work/trading-arb").expect("add");
        reg.save(dir.path()).expect("save");

        let back = Registry::load(dir.path()).expect("reload");
        assert_eq!(back.projects.len(), 1);
        assert_eq!(back.projects["trading-arb"].path, "work/trading-arb");
        assert_eq!(
            back.current.as_deref(),
            Some("trading-arb"),
            "第一个项目应自动选中"
        );
    }

    /// 重名不许覆盖——覆盖一次就把已有配置冲掉了。
    #[test]
    fn duplicate_name_is_rejected_and_original_survives() {
        let dir = root();
        let mut reg = Registry::default();
        reg.add("p", "orig").unwrap();
        reg.save(dir.path()).unwrap();

        let mut again = Registry::load(dir.path()).unwrap();
        assert!(again.add("p", "hijack").is_err(), "重名必须拒");
        again.save(dir.path()).unwrap();
        assert_eq!(
            Registry::load(dir.path()).unwrap().projects["p"].path,
            "orig"
        );
    }

    /// 坏文件必须报错，不许被当成空表再写回去。
    #[test]
    fn corrupt_file_is_reported_not_treated_as_empty() {
        let dir = root();
        Registry::default().add("p", "x").unwrap();
        let file = projects_file(dir.path());
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "this is = not = toml [[[").unwrap();

        let err = Registry::load(dir.path()).expect_err("坏文件必须红");
        assert!(
            err.to_string().contains("projects.toml"),
            "错误里要带是哪个文件：{err}"
        );
    }

    #[test]
    fn save_leaves_no_tmp_behind() {
        let dir = root();
        Registry::default().add("p", "x").unwrap();
        Registry::default().save(dir.path()).unwrap();
        let left: Vec<_> = fs::read_dir(projects_file(dir.path()).parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(left.is_empty(), "原子写的临时文件没清干净：{left:?}");
    }

    #[test]
    fn select_unknown_and_missing_current_both_err() {
        let mut reg = Registry::default();
        assert!(reg.select("nope").is_err());
        assert!(
            reg.current().is_err(),
            "没选过项目要报清楚，不许默默挑第一个"
        );
        reg.add("a", "a").unwrap();
        reg.add("b", "b").unwrap();
        reg.select("b").unwrap();
        assert_eq!(reg.current().unwrap().name, "b");
    }

    #[test]
    fn absolute_and_traversing_paths_are_rejected() {
        let mut reg = Registry::default();
        assert!(reg.add("p", "/etc/passwd").is_err());
        assert!(reg.add("p", "../outside").is_err());
        assert!(reg.add("p", "a\\b").is_err(), "反斜杠不许进配置（§8）");
        assert!(reg.add("a/b", "x").is_err(), "名字里不许有分隔符");
    }

    /// 摘掉条目 = 只动注册表。顺带钉两件事：`current` 清空而不是自动改选，
    /// 以及"再 add 同名会接上同一份历史"（id 由名字派生，这是 §16.1 的必然后果）。
    #[test]
    fn remove_touches_only_the_registry_and_clears_current() {
        let dir = root();
        let mut reg = Registry::default();
        reg.add("a", "a").unwrap();
        reg.add("b", "b").unwrap();
        reg.select("a").unwrap();
        reg.save(dir.path()).unwrap();

        let gone = reg.remove("a").unwrap();
        assert_eq!(gone.path, "a", "要把摘掉的项目回吐给命令层打印");
        assert_eq!(
            reg.current, None,
            "不许自动改选 b：作用在哪个目录必须是人明确选的"
        );
        assert!(reg.remove("a").is_err(), "摘两次是打错字，不是幂等操作");
        reg.save(dir.path()).unwrap();

        let back = Registry::load(dir.path()).unwrap();
        assert_eq!(back.projects.len(), 1);
        assert!(back.projects.contains_key("b"));
        // 摘掉之后再加同名，落回的是**同一个** `project_id`（派生而非自增），所以库里那份
        // 历史直接接得上。这件事由 `keys::project_id_is_derived_and_stable` 钉住，
        // 这里不再重复断言一遍同义反复。
        let mut again = back;
        again.add("a", "a").unwrap();
        assert_eq!(again.projects["a"].path, "a");
        assert_eq!(
            again.current.as_deref(),
            Some("a"),
            "空 current 时 add 会重新自动选中"
        );
    }
}
