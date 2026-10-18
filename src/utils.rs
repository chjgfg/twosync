use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use storage_engines::{
    DurabilityProfile, EngineConfig, EngineError, EngineKind, ExportRecord, IncrError, KeyMeta,
    KvBulk, KvEngine, KvTxn, MvccHealth, SearchPage, SearchQuery, StorageConfig, VacuumStats,
};

use crate::error::{Error, Result};

/// 目标目录所在的文件系统**是否大小写不敏感**。§8「折叠碰撞必须拒」那道门要有它才知道该不该拦。
///
/// 只有一条可靠的路：**在那个目录里**造一个探针文件，再看它另一种大小写的写法是不是同一个东西。
/// 换目录就是换规矩（同一次挂载才同一套），所以探针不能放在别处测。写完立刻删。
///
/// 探不了（只读盘、没权限、目录还不存在）就退回 `cfg!(windows)`：NTFS 默认不敏感，其余按敏感
/// 处理。这是**猜**，不是测——调用方拒了要能说清为什么拒。
pub fn dest_case_insensitive(dir: &Path) -> bool {
    static PROBE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = PROBE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // 两个名字只差一个字母的大小写 ⇒ 折叠之后相同。带上 pid + 自增序号：并发进程与同一进程
    // 的多次调用都不撞（本机 `SystemTime` 粒度约 15.6 ms，光靠时间戳会撞名）。
    let tag = format!("{}.{seq}", std::process::id());
    let lower = dir.join(format!(".tsprobe-a{tag}"));
    let upper = dir.join(format!(".tsprobe-A{tag}"));
    let Ok(handle) = fs::File::create(&lower) else {
        return cfg!(windows);
    };
    drop(handle);
    let hit = fs::symlink_metadata(&upper).is_ok();
    let _ = fs::remove_file(&lower);
    hit
}

/// 站点级数据根：`<root>/.twosync`。FR-12 的强制排除以它为准，扫描器永不进这里。
pub fn site_dir(root: &Path) -> PathBuf {
    root.join(".twosync")
}

/// 引擎数据目录的父目录（§13）：`<root>/.twosync/db`。
///
/// 引擎会再按名字建一层子目录（`db/lsm_tree/`），所以换引擎不会自动带数据。
pub fn db_dir(root: &Path) -> PathBuf {
    site_dir(root).join("db")
}

/// 内容寻址块仓（§13）：文件字节只在这里，不进引擎。
pub fn objects_dir(root: &Path) -> PathBuf {
    site_dir(root).join("objects")
}

pub fn journal_dir(root: &Path) -> PathBuf {
    site_dir(root).join("journal")
}

pub fn log_dir(root: &Path) -> PathBuf {
    site_dir(root).join("log")
}

/// 项目注册表（§16.1）。
pub fn projects_file(root: &Path) -> PathBuf {
    site_dir(root).join("projects.toml")
}

/// 本站点的身份（§9.5.1 的 TOFU 表绑的就是它）。第一次用到时生成并落盘。
///
/// **这不是密钥，也不打算当密钥用**：v1 不给 commit 签名（§9.5.2），所以"对端自报的身份"本身
/// 不值钱。它回答的问题是另一个、而且是很值钱的一个：**我现在拨到的这台机器，是不是我当初确认过的
/// 那一个站点**。SSH 的 host key 只保证"这台主机还是那台主机"，保证不了"那台主机上跑的还是那个站点"
/// ——跳板机、换目录、重装、两台机器共用一个 hostname 都会变。
///
/// 生成源取"时间 + 进程 id + 站点根路径"折成 16 字节。要的是**唯一**，不是抗伪造：抗伪造归 SSH。
pub fn site_id(root: &Path) -> Result<Vec<u8>> {
    let p = site_dir(root).join("site-id");
    if let Ok(b) = fs::read(&p)
        && b.len() == 16
    {
        return Ok(b);
    }
    let mut h = blake3::Hasher::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    h.update(&now.as_nanos().to_le_bytes());
    h.update(&std::process::id().to_le_bytes());
    h.update(root.to_string_lossy().as_bytes());
    let id = h.finalize().as_bytes()[..16].to_vec();
    // 落盘走 tmp → rename：半截的 id 文件会让下一次读到错误长度，然后**再生成一个不同的 id**，
    // 而对端表里记的是旧的——那正好伪造出一次"身份变了"。
    fs::create_dir_all(site_dir(root))?;
    let tmp = p.with_extension("tmp");
    fs::write(&tmp, &id)?;
    fs::rename(&tmp, &p)?;
    Ok(id)
}

/// 打开存储基座（FR-3），并在返回前过一遍 FR-21 自检。
///
/// **所有要读写元数据的命令都从这里拿句柄**：自己拼 `StorageConfig` 就会绕过自检闸，
/// 而自检闸存在的理由是四家的崩溃安全画像互不等价（§9.6.5）。
///
/// `TWOSYNC_COUNT_CALLS=1` ⇒ 再套一层门面调用计数器（文件末尾那一段）。**不设环境变量时
/// 一个字节都不改**：默认路径上没有原子加、也没有 Drop 打印，量出来的才是发布版的行为。
pub fn open_store(root: &Path, kind: EngineKind) -> Result<Box<dyn KvEngine>> {
    let cfg = StorageConfig {
        kind,
        data_dir: db_dir(root),
        engine: EngineConfig::default(),
    };
    let db = cfg.open()?;
    check_durability_gate(db.as_ref())?;
    if std::env::var_os(COUNT_ENV).is_some() {
        eprintln!("门面计数器已挂上（{COUNT_ENV}）：句柄释放时把各类门面调用打到 stderr");
        return Ok(counted(db, &Meter::default(), true));
    }
    Ok(db)
}

/// FR-21：画像三要件不全 = 报错并说清缺哪一项。
///
/// 靠 `durability_profile()` 现问，不许照文档记忆判断——文档会过期，画像不会。
pub fn check_durability_gate(db: &dyn KvEngine) -> Result<()> {
    let p = db.durability_profile();
    let mut gaps = Vec::new();
    if !p.transaction_wal {
        gaps.push("无事务 WAL");
    }
    if !p.fsync_on_commit {
        gaps.push("commit 不 fsync");
    }
    if !p.data_checksum {
        gaps.push("数据记录无校验和");
    }
    if !gaps.is_empty() {
        return Err(Error::DurabilityGate {
            engine: p.engine.to_string(),
            gaps: gaps.join("、"),
        });
    }
    Ok(())
}

/// 一棵目录树的字节数与条目数（NFR-5 的磁盘账要用：CAS / 引擎 / 工作树各占多少）。
///
/// 只算**普通文件**，且**不跟随符号链接**（含指向目录的链接）：跟随就可能在自引用的树上
/// 转圈，而配额关心的是真占了多少磁盘。链接本身按名字长度记账不算字节。
/// 出错（权限等）不中断整趟，只把这一条计入 `unreadable` —— 报"少了多少"比假装全量准。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DirSize {
    pub bytes: u64,
    pub files: u64,
    pub dirs: u64,
    pub unreadable: u64,
}

pub fn dir_size(root: &Path) -> DirSize {
    let mut out = DirSize::default();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            out.unreadable += 1;
            continue;
        };
        out.dirs += 1;
        for e in entries.flatten() {
            let Ok(md) = fs::symlink_metadata(e.path()) else {
                out.unreadable += 1;
                continue;
            };
            let ft = md.file_type();
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(e.path());
            } else {
                out.bytes += md.len();
                out.files += 1;
            }
        }
    }
    out
}

// ── 门面调用计数器（§17 补记廿六）────────────────────────────────────────────
//
// 为什么要有它：本机墙钟 run 间是 ±2×（§11.3 两臂 226 s / 327 s 同形状），**这个粒度上计时不算证据**。
// 而 §9.6.1 那条"门面每次读调用折一遍全库"是所有固定成本的根源 ⇒ 判优只能靠**折的次数**。
// 它同时回答另一件事：写类与"事务收尾"分格，是因为每次 `commit()` 带一次 WAL fsync
// （§11.1：耗时 = 事务数 × 盘单价，机械盘 85.8 ms），所以事务数本身就是成本的一项。

/// 打开句柄时挂计数器的环境变量名。**只在 [`open_store`] 里读一次**。
pub const COUNT_ENV: &str = "TWOSYNC_COUNT_CALLS";

/// 按**代价形状**分桶，不按方法名分桶 —— 这是这把尺子唯一的设计决定。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CallCounts {
    /// `get` / `exists` / `get_meta` / `multi_get` / `get_ttl`
    pub point_read: u64,
    /// `scan` / `prefix_scan` / `reverse_scan` / `seek*` / `search_keys` / `key_count` / `export_*`
    pub scan: u64,
    /// `set` / `delete` / `batch_*` / `incr` / `decr` / TTL 写
    pub write: u64,
    /// `commit()` + `rollback()`：事务数（= WAL fsync 次数的下界）
    pub txn_end: u64,
    /// `health` / `vacuum` / `flush` / `checkpoint`：诊断路径，允许它是 O(全库)
    pub admin: u64,
}

impl CallCounts {
    /// **"折了几遍全库"**：两个读桶之和。§15.22 那条固定成本就是这个数 × 一次全库折的代价。
    pub fn folds(&self) -> u64 {
        self.point_read + self.scan
    }

    /// 这一段区间的账（计数器是累计的，用例要的是"这一趟花了多少"）。
    /// 用 `saturating_sub`：把手在两个快照之间被别的线程加过也不会下溢成一个大数把结论带偏。
    pub fn since(&self, earlier: CallCounts) -> CallCounts {
        CallCounts {
            point_read: self.point_read.saturating_sub(earlier.point_read),
            scan: self.scan.saturating_sub(earlier.scan),
            write: self.write.saturating_sub(earlier.write),
            txn_end: self.txn_end.saturating_sub(earlier.txn_end),
            admin: self.admin.saturating_sub(earlier.admin),
        }
    }

    pub fn line(&self) -> String {
        format!(
            "点读 {} / 扫描 {} ⇒ 折全库 {} 次；写 {} / 事务收尾 {}（≈ WAL fsync 次数）/ 诊断 {}",
            self.point_read,
            self.scan,
            self.folds(),
            self.write,
            self.txn_end,
            self.admin
        )
    }
}

/// 可共享的计数把手。用法：**先拿把手，再把引擎包起来** —— 包装体交出去之后就变成
/// `Box<dyn KvEngine>`，测试没法再 downcast 回来读账，把手是唯一不留"取账专用接口"的办法
/// （往 `KvEngine` 上加一个 `fn counts(&self)` 才是坏路子：那是为了让一个测量工具而污染四个引擎的实现）。
#[derive(Clone, Default)]
pub struct Meter(Arc<Mutex<CallCounts>>);

impl Meter {
    pub fn snapshot(&self) -> CallCounts {
        *self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn add(&self, f: impl FnOnce(&mut CallCounts)) {
        f(&mut self.0.lock().unwrap_or_else(|p| p.into_inner()));
    }
}

/// 门面的一层透明包装：除计数之外**不改任何行为**，每个方法原样转发。
pub struct Counted {
    inner: Box<dyn KvEngine>,
    meter: Meter,
    /// true = 句柄释放时把账打到 stderr（命令行那条路）；测试里给 false，免得刷屏。
    announce: bool,
}

/// 用 `meter` 把 `inner` 包起来。
pub fn counted(inner: Box<dyn KvEngine>, meter: &Meter, announce: bool) -> Box<dyn KvEngine> {
    Box::new(Counted {
        inner,
        meter: meter.clone(),
        announce,
    })
}

impl Drop for Counted {
    fn drop(&mut self) {
        if self.announce {
            eprintln!(
                "门面读数 [{}] = {}",
                self.inner.name(),
                self.meter.snapshot().line()
            );
        }
    }
}

impl KvEngine for Counted {
    fn name(&self) -> &'static str {
        self.inner.name()
    }
    fn path(&self) -> Option<&Path> {
        self.inner.path()
    }
    fn begin(&self) -> Box<dyn KvTxn + '_> {
        Box::new(CountedTxn {
            inner: self.inner.begin(),
            meter: &self.meter,
        })
    }
    // 批量导入会话不计数：twosync 现在不走它（`bulk` 只在引擎侧的导入实验里出现）。
    // 为它再写一层包装就是把"尺子"变成"第二套实现"——要量它的时候再加。
    fn begin_bulk(&self) -> Box<dyn KvBulk + '_> {
        self.inner.begin_bulk()
    }
    fn export_latest_visible(&self, include_deleted: bool) -> Vec<ExportRecord> {
        self.meter.add(|c| c.scan += 1);
        self.inner.export_latest_visible(include_deleted)
    }
    fn vacuum(&self) -> std::result::Result<VacuumStats, EngineError> {
        self.meter.add(|c| c.admin += 1);
        self.inner.vacuum()
    }
    fn health(&self) -> MvccHealth {
        self.meter.add(|c| c.admin += 1);
        self.inner.health()
    }
    fn set_vacuum_auto_slice(&self, rows: u64) {
        self.inner.set_vacuum_auto_slice(rows)
    }
    fn vacuum_auto_slice(&self) -> u64 {
        self.inner.vacuum_auto_slice()
    }
    fn durability_profile(&self) -> DurabilityProfile {
        self.inner.durability_profile()
    }
    fn flush(&self) -> std::result::Result<(), EngineError> {
        self.meter.add(|c| c.admin += 1);
        self.inner.flush()
    }
    fn checkpoint(&self) -> std::result::Result<(), EngineError> {
        self.meter.add(|c| c.admin += 1);
        self.inner.checkpoint()
    }
    fn merge(&self) -> std::result::Result<(), EngineError> {
        self.meter.add(|c| c.admin += 1);
        self.inner.merge()
    }
}

/// 事务的计数包装。`'a` 挂在 [`Counted`] 上：引擎句柄活着，事务才活着（与引擎自己的语义一致）。
pub struct CountedTxn<'a> {
    inner: Box<dyn KvTxn + 'a>,
    meter: &'a Meter,
}

impl KvTxn for CountedTxn<'_> {
    fn version(&self) -> u64 {
        self.inner.version()
    }
    fn set(&self, key: &[u8], value: Vec<u8>) -> bool {
        self.meter.add(|c| c.write += 1);
        self.inner.set(key, value)
    }
    fn set_with_ttl(&self, key: &[u8], value: Vec<u8>, ttl: Duration) -> bool {
        self.meter.add(|c| c.write += 1);
        self.inner.set_with_ttl(key, value, ttl)
    }
    fn delete(&self, key: &[u8]) -> bool {
        self.meter.add(|c| c.write += 1);
        self.inner.delete(key)
    }
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.meter.add(|c| c.point_read += 1);
        self.inner.get(key)
    }
    fn exists(&self, key: &[u8]) -> bool {
        self.meter.add(|c| c.point_read += 1);
        self.inner.exists(key)
    }
    fn get_meta(&self, key: &[u8]) -> Option<KeyMeta> {
        self.meter.add(|c| c.point_read += 1);
        self.inner.get_meta(key)
    }
    fn batch_set(&self, items: &[(Vec<u8>, Vec<u8>)]) -> std::result::Result<(), Vec<u8>> {
        self.meter.add(|c| c.write += 1);
        self.inner.batch_set(items)
    }
    fn batch_delete(&self, keys: &[Vec<u8>]) -> std::result::Result<(), Vec<u8>> {
        self.meter.add(|c| c.write += 1);
        self.inner.batch_delete(keys)
    }
    fn multi_get(&self, keys: &[Vec<u8>]) -> Vec<Option<Vec<u8>>> {
        self.meter.add(|c| c.point_read += 1);
        self.inner.multi_get(keys)
    }
    fn incr(&self, key: &[u8], delta: i64) -> std::result::Result<i64, IncrError> {
        self.meter.add(|c| c.write += 1);
        self.inner.incr(key, delta)
    }
    fn decr(&self, key: &[u8], delta: i64) -> std::result::Result<i64, IncrError> {
        self.meter.add(|c| c.write += 1);
        self.inner.decr(key, delta)
    }
    fn incr_with_ttl(
        &self,
        key: &[u8],
        delta: i64,
        ttl: Duration,
    ) -> std::result::Result<i64, IncrError> {
        self.meter.add(|c| c.write += 1);
        self.inner.incr_with_ttl(key, delta, ttl)
    }
    fn scan(&self, start: Option<&[u8]>, end: Option<&[u8]>) -> Vec<ExportRecord> {
        self.meter.add(|c| c.scan += 1);
        self.inner.scan(start, end)
    }
    fn reverse_scan(&self, start: Option<&[u8]>, end: Option<&[u8]>) -> Vec<ExportRecord> {
        self.meter.add(|c| c.scan += 1);
        self.inner.reverse_scan(start, end)
    }
    fn prefix_scan(&self, prefix: &[u8]) -> Vec<ExportRecord> {
        self.meter.add(|c| c.scan += 1);
        self.inner.prefix_scan(prefix)
    }
    fn seek(&self, key: &[u8]) -> Option<ExportRecord> {
        self.meter.add(|c| c.scan += 1);
        self.inner.seek(key)
    }
    fn seek_prev(&self, key: &[u8]) -> Option<ExportRecord> {
        self.meter.add(|c| c.scan += 1);
        self.inner.seek_prev(key)
    }
    fn search_keys(&self, query: &SearchQuery) -> SearchPage {
        self.meter.add(|c| c.scan += 1);
        self.inner.search_keys(query)
    }
    fn key_count(&self, prefix: &[u8]) -> usize {
        self.meter.add(|c| c.scan += 1);
        self.inner.key_count(prefix)
    }
    fn get_ttl(&self, key: &[u8]) -> Option<Option<Duration>> {
        self.meter.add(|c| c.point_read += 1);
        self.inner.get_ttl(key)
    }
    fn refresh_ttl(&self, key: &[u8], ttl: Duration) -> bool {
        self.meter.add(|c| c.write += 1);
        self.inner.refresh_ttl(key, ttl)
    }
    fn persist_key(&self, key: &[u8]) -> bool {
        self.meter.add(|c| c.write += 1);
        self.inner.persist_key(key)
    }
    fn purge_expired(&self) -> usize {
        self.meter.add(|c| c.write += 1);
        self.inner.purge_expired()
    }
    fn export_latest_visible(&self, include_deleted: bool) -> Vec<ExportRecord> {
        self.meter.add(|c| c.scan += 1);
        self.inner.export_latest_visible(include_deleted)
    }
    fn commit(&self) {
        self.meter.add(|c| c.txn_end += 1);
        self.inner.commit()
    }
    fn rollback(&self) {
        self.meter.add(|c| c.txn_end += 1);
        self.inner.rollback()
    }
}
