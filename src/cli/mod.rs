//! CLI 层。`Cmd` 是命令清单，实现按里程碑分批放在 `m0.rs`…`m4.rs`，
//! 分发在 `main.rs`（加一条命令要动三处：枚举变体 + 批次函数 + match 分支）。

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;

pub mod cmd;
pub mod config;
pub mod m0;
pub mod m1;
pub mod m2;
pub mod m3;
pub mod m4;
pub mod registry;

use crate::cli::cmd::engine_parser;
use crate::cli::config::Config;
use crate::error::Result;
use crate::utils::open_store;
pub use cmd::{Cmd, Import, PeerCmd};
use storage_engines::{EngineKind, KvEngine};

/// 全局参数。`Cli` 直接住在本模块，不另开 `cli.rs`——那会撞 `clippy::module_inception`
/// （`cli::cli`），而且它只有这一个结构体，单独成文件没有省下任何东西。
#[derive(Parser)]
#[command(
    name = "twosync",
    version,
    about = "P2P 微型 git：跨 Windows/Linux 同步大二进制项目"
)]
pub struct Cli {
    /// 站点级同步根目录（下面挂多个 project，§13）
    #[arg(long, default_value = ".", global = true)]
    pub root: PathBuf,

    /// 作用的项目；省略时用 `project use` 选中的那个
    #[arg(long, global = true)]
    pub project: Option<String>,

    /// 存储基座引擎（§10.8 换引擎跑同一套验证）
    ///
    /// 省略时依次取 `config` 里的 `engine`、内置默认 `lsm_tree`。**这里不给 clap 默认值**：
    /// 给了就分不清"用户写了"和"没写"，配置文件永远压不过一个凭空出现的值。
    #[arg(long, global = true, value_parser = engine_parser)]
    pub engine: Option<EngineKind>,

    /// **这一个进程唯一的 IO 桶**（FR-15 / §16.3"资源预算不叠加"）。不由命令行填，
    /// `--max-mbps` 的各条命令通过 [`Cli::budget`] 把它灌进这里。
    ///
    /// 为什么住在 `Cli` 而不是各命令自己 `new` 一个：单条命令只有一个阶段，看不出差别；
    /// `daemon` 一轮有 scan→commit→push 三个阶段，各开各的桶就是三倍预算，
    /// 而那正是 §16.3 点名叫出来的错误形状。
    #[arg(skip)]
    pub io_bucket: crate::store::cas::Bucket,

    /// 第一次 [`Cli::store`] 时打开、之后**一直复用**的那个引擎句柄。同样不由命令行填
    /// （`pub` 只为测试能拼出 `Cli`；命令层一律走 [`Cli::store`]，别自己去 `open_store`，
    /// 那会绕过 FR-21 启动自检与这个缓存）。
    ///
    /// 为什么是 `Arc` 而不是把 `&dyn KvEngine` 直接递出去：49 处调用点写的是 `&*db`，
    /// 换成引用会让它们全变成 `clippy::deref_addrof`（一次纯 API 改动不该逼出 49 处手改），
    /// 而 `Arc` 的克隆是免费的、`&*db` 那个形状也照常成立。
    #[arg(skip)]
    pub store_slot: std::sync::OnceLock<Arc<dyn KvEngine>>,

    #[command(subcommand)]
    pub cmd: Cmd,
}

impl Cli {
    /// 拿到本进程的 IO 桶，并把它设成 `max_bps` 的预算（`0` = 这条命令没给限速，不改已有预算）。
    ///
    /// 所有 `Cas::with_bucket(&cli.budget(x))` 都指向**同一个**把手：漏桶记的是"从第一次扣字节
    /// 起的累计期望时间"，所以共用把手才能让整轮的平均速率被一个数管住。
    pub fn budget(&self, max_bps: u64) -> crate::store::cas::Bucket {
        self.io_bucket.configure(max_bps);
        self.io_bucket.clone()
    }

    /// 打开存储基座。"命令行 > 配置文件 > 内置默认"这条顺序**只在这里实现一次**——
    /// 两处各判一遍，就会出现 `status` 看着 lsm、`commit` 写进 bplus 的那种分裂。
    ///
    /// 也不许在命令层自己拼 `StorageConfig`：那会绕过 `open_store` 里的 FR-21 启动自检。
    ///
    /// **句柄是缓存的**（本进程第一次打开之后一直复用），理由是一个实测数：12 GiB 站点上
    /// "只开库 + `health()`" 的 `doctor` 要 2.20 s CPU（§11.4f），而 `daemon` 一轮原来要开五次
    /// （scan / 读 head 两次 / commit / push）⇒ **每轮约 8.8 s CPU 纯开库**，比窄扫本身（约 1 s）
    /// 还贵。常驻循环里这叫结构性浪费，而且它随阶段数线性涨——加一个阶段就多 2.2 s。
    ///
    /// 代价要写清：句柄活到 `Cli` 结束，所以"同一个进程里再开一个句柄去开同一份库"这条路径
    /// 从此走不通（引擎持**库级文件锁**，第二个句柄 `WouldBlock`）。本来也不该走：要开另一份站点
    /// 就换 `--root`（那是另一个 `Cli`），要开同一份就用这个函数。
    pub fn store(&self) -> Result<Arc<dyn KvEngine>> {
        if let Some(db) = self.store_slot.get() {
            return Ok(db.clone());
        }
        let kind = match self.engine {
            Some(k) => k,
            None => Config::load(&self.root)?.engine()?,
        };
        let db: Arc<dyn KvEngine> = Arc::from(open_store(&self.root, kind)?);
        // 真并发时会有一个人 `set` 输：他那份句柄当场释放（文件锁也还回去），大家看到赢的那个。
        let _ = self.store_slot.set(db);
        Ok(self
            .store_slot
            .get()
            .expect("上面要么 set 成功，要么已经有人 set 过")
            .clone())
    }

    /// CAS 块大小。同 `store()`：优先级判定只有一处。
    pub fn block_size(&self) -> Result<u64> {
        Ok(Config::load(&self.root)?.block_size())
    }
}
