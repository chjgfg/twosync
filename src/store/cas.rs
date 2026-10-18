//! 内容寻址块仓（CAS）：`<root>/.twosync/objects/<hash[0:2]>/<hash[2:]>`。
//!
//! 这是本项目对 git 唯一的技术领先点（§1 理由①）：git 的 blob 粒度是整个文件，这里是块。
//! 10 GB 文件改 1 MB，git 存全量传全量，这里只多一份脏块。
//!
//! # 四条规矩，每条都有用例钉着
//!
//! 1. **写序**（FR-13）：写 tmp → `fsync` → `rename`，然后才允许被清单引用。反过来做，
//!    崩溃后清单就会指向一个不存在的块。
//! 2. **全零块不落盘**（§8 稀疏）：清单里标 `zero`，重建时补零。否则 10 G 稀疏文件会被
//!    "压实"成真实 10 G，配额瞬间吃穿。
//! 3. **读必校验**（§9.4.7）：块字节流过来就重算 hash，不符即报错。**不许静默重传**——
//!    把损坏当成缺失，等于把数据损坏伪装成一次普通同步。
//! 4. **写前自证**：`copy_block` 边拷边算 hash，与清单里的不符就报错。这一条专门抓
//!    "扫描之后、落盘之前源文件被人改了"——那种情况下按 hash 存盘会把错内容永久钉死。
//!
//! # 内存：两遍流式，与块大小无关
//!
//! 第一遍 `plan` 只读+算 hash，一个字节都不写；第二遍只把**缺失的那些块**按字节段
//! seek+copy 落盘。所以常驻内存是一个 `CHUNK` 缓冲 + 该文件的清单（每块 41 B），
//! 10 GB 文件的清单也才 ~100 KB。
//!
//! 顺带修正一处我先前说错的：第二遍**不需要**重算 hash——第一遍的清单已经给出每块
//! 的 hash 和偏移，第二遍只是"照单搬运缺的那几段"。所以代价是 1 次全读 + 1 次脏块读。
//!
//! # 两个开关：`--fast-import` 与限速（§14 Q11，2026-10-17 定案）
//!
//! 10 GiB 首建库实测 5 071 s，而归因表（§11.1）说明那 84 分钟**几乎全在等 fsync**：
//! 同一块机械盘上 fsync 单价 85.8 ms，不 fsync 快 33 倍。所以：
//!
//! - [`Cas::with_fsync(false)`]：落块**不等** fsync。**注意它放弃的是什么**——
//!   不是顺序（仍是 `tmp → rename`，规矩 1 的顺序没动），而是"清单落盘之前块已经持久"这一半。
//!   于是断电可能留下"清单说有、盘上其实是半截"的块。这种情况**不会静默**：
//!   读回时按 hash 校验必失败（规矩 3 报 `CorruptBlock`），恢复办法是把那一版重新导入，
//!   而不是假装它还在。默认 `true`；只有显式 `--fast-import` 才关，且命令行会打印放弃了什么。
//! - [`Cas::with_rate_limit`]：往 CAS 写盘的平均速率闸（漏桶）。NFR-2 明写首建库"必须可限速"，
//!   NFR-3 要的是"同步启动不许把同机策略进程的延迟拖走"——限速对象是写字节，不是 CPU。
//!
//! 这两个开关合起来就是"首次导入是磁盘速度的函数"这一事实的**可操作形式**：
//! 想要快就 `--fast-import`，想要稳就默认，想要不打扰同机进程就 `--max-mbps`。

use std::format;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::store::keys::{self, Hash};

/// 默认块大小 4 MiB（FR-5）。M4 换 CDC 时由 `keys` 里的 `chunk_param` 区分命名空间。
pub const DEFAULT_BLOCK: u64 = 4 << 20;
/// 流式缓冲：常驻内存由它决定，不是块大小。
const CHUNK: usize = 64 << 10;
/// 全零块的哨兵 hash——它永远不落盘，`contains` 直接返回 true。
const ZERO_HASH: Hash = [0u8; 32];

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 临时文件名里的 pid：两个进程不会撞同一个 tmp；同一进程内再叠一个自增序号。
fn tmp_tag() -> String {
    format!(
        "{}.{:08x}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockRef {
    pub hash: Hash,
    /// 该块在源文件里的逻辑长度（末块会短于 block_size）。
    pub size: u32,
    /// 全零块：不占磁盘空间，重建时补零。
    pub zero: bool,
}

/// 一个文件版本 = 整文件 hash + 有序块清单。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Blob {
    pub hash: Hash,
    pub size: u64,
    pub block_size: u64,
    pub blocks: Vec<BlockRef>,
}

/// `put_file` 的结果，用来把"只写脏块"变成可核对的数字（§10.4 差分表要用）。
#[derive(Debug, Clone)]
pub struct PutOutcome {
    pub blob: Blob,
    /// 本次新落盘的块数与字节数。
    pub stored: usize,
    pub stored_bytes: u64,
    /// 已在库中、直接复用的块数。
    pub reused: usize,
    /// 全零块数（既不写也不占空间）。
    pub zero: usize,
}

impl PutOutcome {
    /// 逻辑字节里真正写了多少——"改 1 MB 只写 1 MB"就是这一条。
    pub fn wrote_all_content(&self) -> bool {
        self.stored == 0 && self.zero == 0 && !self.blob.blocks.is_empty()
    }
}

pub struct Cas {
    dir: PathBuf,
    block_size: u64,
    /// 落块要不要等 fsync（默认 `true`）。关掉放弃了什么，见模块头。
    fsync_blocks: bool,
    /// CAS 写字节的平均速率闸。`None` = 不限速。放在 `Mutex` 里是因为 `Cas` 全程按
    /// `&self` 传（`put_file`/`read_blob` 都是），而限速需要累计已写字节。
    pacer: Mutex<Option<Pacer>>,
}

/// 漏桶限速：**只保证平均速率**，不削瞬时突发。
///
/// 为什么用"从第一次写开始的累计期望时间"而不是"每秒令牌数"：要防的是**长时间占着磁盘**
/// （NFR-3 的延迟抖动），不是"某一个 64 KiB 写太快"。累计式写法不需要后台任务，
/// 也就不会自己变成一个新的 IO/CPU 突发源。
pub struct Pacer {
    max_bps: u64,
    start: Instant,
    written: u64,
}

impl Pacer {
    fn new(max_bps: u64) -> Self {
        Self {
            max_bps,
            start: Instant::now(),
            written: 0,
        }
    }

    /// 记 `n` 字节；如果按平均速率还没到该写完的时间就睡一会儿。
    fn charge(&mut self, n: u64) {
        self.written += n;
        let want = Duration::from_secs_f64(self.written as f64 / self.max_bps as f64);
        let gone = self.start.elapsed();
        if want > gone {
            std::thread::sleep(want - gone);
        }
    }
}

impl Cas {
    pub fn new(objects_dir: &Path, block_size: u64) -> Result<Self> {
        fs::create_dir_all(objects_dir)?;
        Ok(Self {
            dir: objects_dir.to_path_buf(),
            block_size,
            fsync_blocks: true,
            pacer: Mutex::new(None),
        })
    }

    /// `false` = 落块不等 fsync（`--fast-import`）。放弃的东西与恢复办法见模块头，
    /// **调用方必须把它打印出来**，不许静默生效。
    pub fn with_fsync(mut self, on: bool) -> Self {
        self.fsync_blocks = on;
        self
    }

    /// 限制写 CAS 的平均字节/秒；`0` 当"不限速"处理（省得调用方传个 0 变成除零）。
    pub fn with_rate_limit(mut self, max_bps: u64) -> Self {
        let pacer = if max_bps == 0 {
            None
        } else {
            Some(Pacer::new(max_bps))
        };
        self.pacer = Mutex::new(pacer);
        self
    }

    /// 命令层用它把"放弃了什么"如实印出来。
    pub fn fsync_blocks(&self) -> bool {
        self.fsync_blocks
    }

    pub fn rate_limit_bps(&self) -> Option<u64> {
        self.pacer
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(|p| p.max_bps)
    }

    /// 从这个 `Cas` 建立起累计扣过桶的字节数（写 + 删）；没开限速时是 `None`。
    ///
    /// 存在的唯一理由是 §9.3/FR-15 那条"删除也要过同一个桶"**必须可核对**：用例拿它断言
    /// "一趟 unlink 扣掉的字节 = 被删块的 size 之和"，这比掐墙钟可靠（单核机上计时用例本身就是噪声源）。
    pub fn paced_bytes(&self) -> Option<u64> {
        self.pacer
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(|p| p.written)
    }

    /// 块在库内的相对路径：`<hash[0:2]>/<hash[2:]>`，两级目录避免单目录爆十万文件。
    pub fn path_of(hash: &Hash) -> PathBuf {
        let hex = keys::hex(hash);
        PathBuf::from(&hex[0..2]).join(&hex[2..])
    }

    pub fn contains(&self, hash: &Hash) -> bool {
        *hash == ZERO_HASH || self.dir.join(Self::path_of(hash)).exists()
    }

    /// 块仓的根目录。GC 要**扫盘**找孤儿文件（盘上有、账上没有），除这个之外没有别的路。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 删一个块。返回是否真删了（`false` = 文件本来就不在，pending 表上的账照样要清）。
    ///
    /// 调用点是 §9.3 的**第二趟**（`gc::unlink_pending`）：归零那一刻只往 `D` 表记一行，真正碰文件
    /// 系统在这里，且每个块删之前先从 FR-15 的桶里扣自己的 size。删之前 `K`/`k`/`D` 三行已在同一事务
    /// 里清掉——崩在中间留下的是"该删的文件还在"（垃圾），而不是"清单引用不存在的块"（损坏）。
    /// 全零块本来就不落盘，所以这里对它是 no-op。
    pub fn remove_block(&self, hash: &Hash) -> Result<bool> {
        if *hash == ZERO_HASH {
            return Ok(false);
        }
        let p = self.dir.join(Self::path_of(hash));
        match fs::remove_file(&p) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// 在块仓目录里占一个临时文件名。**同一目录**才有原子 `rename` 的保证，
    /// 所以外部（`restore` 读符号链接目标）要写临时文件也用它，不许自己拼名字。
    pub fn temp_for(&self, tag: &str) -> PathBuf {
        self.dir.join(format!(".tmp.{}.{tag}", tmp_tag()))
    }

    /// 第一遍：流式切块 + 算 hash，**不写任何字节**。
    pub fn plan(&self, file: &Path) -> Result<Blob> {
        let mut src = File::open(file)?;
        let mut whole = blake3::Hasher::new();
        let mut block_hasher = blake3::Hasher::new();
        let mut blocks = Vec::new();
        let mut buf = vec![0u8; CHUNK];
        let mut block_len: u64 = 0;
        let mut all_zero = true;
        let mut total: u64 = 0;

        loop {
            // 一次最多读到块边界为止：保证 block_len 永远不超过 block_size，
            // 块边界就完全由我们自己决定，不依赖 CHUNK 能否整除块大小。
            let want = ((self.block_size - block_len) as usize).min(CHUNK);
            let n = src.read(&mut buf[..want])?;
            if n == 0 {
                break;
            }
            whole.update(&buf[..n]);
            block_hasher.update(&buf[..n]);
            block_len += n as u64;
            total += n as u64;
            all_zero &= buf[..n].iter().all(|b| *b == 0);

            if block_len == self.block_size {
                blocks.push(close_block(&mut block_hasher, block_len, all_zero));
                block_len = 0;
                all_zero = true;
            }
        }
        if block_len > 0 {
            blocks.push(close_block(&mut block_hasher, block_len, all_zero));
        }
        Ok(Blob {
            hash: *whole.finalize().as_bytes(),
            size: total,
            block_size: self.block_size,
            blocks,
        })
    }

    /// 把文件切块入库：已存在的块直接复用，全零块不落盘。
    pub fn put_file(&self, file: &Path) -> Result<PutOutcome> {
        let blob = self.plan(file)?;
        let mut src = File::open(file)?;
        let mut out = PutOutcome {
            blob: blob.clone(),
            stored: 0,
            stored_bytes: 0,
            reused: 0,
            zero: 0,
        };
        let mut offset = 0u64;
        for b in &blob.blocks {
            if b.zero {
                out.zero += 1;
            } else if self.contains(&b.hash) {
                out.reused += 1;
            } else {
                self.copy_block(&mut src, offset, b)?;
                out.stored += 1;
                out.stored_bytes += b.size as u64;
            }
            offset += b.size as u64;
        }
        Ok(out)
    }

    /// 重建到目标路径（`restore`/`checkout` 用）。全程校验，写完才 rename。
    pub fn read_blob(&self, blob: &Blob, dest: &Path) -> Result<()> {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = temp_in(dest.parent().unwrap_or(Path::new(".")));
        let mut f = File::create(&tmp)?;
        let mut check = blake3::Hasher::new();
        for b in &blob.blocks {
            if b.zero {
                write_zeros(&mut f, &mut check, b.size)?;
            } else {
                self.copy_out(&b.hash, b.size, &mut f, &mut check)?;
            }
        }
        f.flush()?;
        f.sync_all()?;
        drop(f);
        if check.finalize().as_bytes() != &blob.hash {
            let _ = fs::remove_file(&tmp);
            return Err(Error::CorruptBlock {
                hash: keys::short(&blob.hash),
                path: dest.to_path_buf(),
                why: "整文件 hash 与清单不符（块都对但组合不对）".to_string(),
            });
        }
        fs::rename(&tmp, dest)?;
        Ok(())
    }

    /// 把一个块读进内存。**M1 的发送侧与 `verify` 用**：`read_blob` 是"整块写到目的地"，
    /// 而网络要的是字节本身，所以这里给一份带自证的读取（`copy_out` 会比对块名）。
    /// 内存代价 = 一个块：4 MiB 是默认块大小的上限，超过它（`--block-size` 最大 64 MiB）
    /// 调用方要自己考虑 §9.6.1 的预算，别在这里偷偷物化整库。
    pub fn read_block(&self, hash: &Hash, size: u32) -> Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(size as usize);
        self.copy_out(hash, size, &mut buf, &mut blake3::Hasher::new())?;
        Ok(buf)
    }

    /// 逐块校验（`gc --fsck` 与 §10.2 用）。返回校验过的块数。
    pub fn verify(&self, blob: &Blob) -> Result<usize> {
        let mut n = 0;
        for b in &blob.blocks {
            if b.zero {
                continue;
            }
            let mut sink = std::io::sink();
            self.copy_out(&b.hash, b.size, &mut sink, &mut blake3::Hasher::new())?;
            n += 1;
        }
        Ok(n)
    }

    /// 把一小段字节当 blob 入库。**只用于路径级别的小对象**——现在唯一的用户是符号链接的
    /// 目标文本（§17 补记三：`TreeEntry` 对链接只存 hash，若不把目标本身当 blob 落盘，
    /// 那个 hash 就永远解不回文本，历史里就重建不出链接）。
    ///
    /// 它一次物化整段字节，所以拿它存文件等于作废 NFR-1 的常数内存承诺。
    /// 与 `put_file` 共用 `copy_block`：同一套"tmp → fsync → 自证 → rename"。
    pub fn put_bytes(&self, bytes: &[u8]) -> Result<PutOutcome> {
        let mut out = PutOutcome {
            blob: Blob {
                hash: keys::hash_of(bytes),
                size: bytes.len() as u64,
                block_size: self.block_size,
                blocks: Vec::new(),
            },
            stored: 0,
            stored_bytes: 0,
            reused: 0,
            zero: 0,
        };
        let mut src = std::io::Cursor::new(bytes);
        for (i, chunk) in bytes.chunks(self.block_size as usize).enumerate() {
            let zero = chunk.iter().all(|b| *b == 0);
            let hash = if zero {
                ZERO_HASH
            } else {
                keys::hash_of(chunk)
            };
            let b = BlockRef {
                hash,
                size: chunk.len() as u32,
                zero,
            };
            if b.zero {
                out.zero += 1;
            } else if self.contains(&b.hash) {
                out.reused += 1;
            } else {
                self.copy_block(&mut src, i as u64 * self.block_size, &b)?;
                out.stored += 1;
                out.stored_bytes += b.size as u64;
            }
            out.blob.blocks.push(b);
        }
        Ok(out)
    }

    /// 清单里**磁盘上已经不在了**的块。§7.3 的"块已在 CAS"检查用它：
    /// 逐块 `stat` 而不是逐块重算 hash——后者是 `verify` 的活，属于 `gc --fsck`，
    /// 不该出现在一次 `restore` 的路上（10 GB 文件会把它变成分钟级）。
    pub fn missing(&self, blob: &Blob) -> Vec<Hash> {
        blob.blocks
            .iter()
            .filter(|b| !b.zero)
            .map(|b| b.hash)
            .filter(|h| !self.contains(h))
            .collect()
    }

    /// 把一个块从源reader的 `offset` 拷进库：tmp → fsync → 自证 hash → rename。
    ///
    /// 泛型而不是写死 `File`：`put_file` 传 `File`、`put_bytes` 传 `Cursor<&[u8]>`，
    /// 两边共用这**同一份**落盘代码。这里要是复制两份，"忘了 fsync 就 rename"这类事故
    /// 只会修好一半——而 FR-13 要的是全中。
    fn copy_block<R: Read + Seek>(&self, src: &mut R, offset: u64, b: &BlockRef) -> Result<()> {
        src.seek(SeekFrom::Start(offset))?;
        let final_path = self.dir.join(Self::path_of(&b.hash));
        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = temp_in(&self.dir);
        let res = (|| -> Result<()> {
            let mut out = File::create(&tmp)?;
            let mut check = blake3::Hasher::new();
            let mut buf = vec![0u8; CHUNK];
            let mut left = b.size as usize;
            while left > 0 {
                let want = left.min(CHUNK);
                let n = src.read(&mut buf[..want])?;
                if n == 0 {
                    return Err(Error::Io(std::io::Error::other(format!(
                        "读块时提前 EOF：offset={offset} 还需 {left} 字节"
                    ))));
                }
                check.update(&buf[..n]);
                out.write_all(&buf[..n])?;
                left -= n;
            }
            out.flush()?;
            if self.fsync_blocks {
                out.sync_all()?; // ← 先 fsync 再 rename（FR-13）；`--fast-import` 关的就是这一行
            }
            drop(out);
            // 自证：源文件在这一会儿里被人改过的话，这里就会不符，而不是存进一个错块。
            if check.finalize().as_bytes() != &b.hash {
                return Err(Error::CorruptBlock {
                    hash: keys::short(&b.hash),
                    path: src_path_note(offset),
                    why: "源文件在扫描之后被改动，块内容与清单 hash 不符".to_string(),
                });
            }
            Ok(())
        })();

        match res {
            Ok(()) => match fs::rename(&tmp, &final_path) {
                Ok(()) => {
                    sync_dir(final_path.parent().unwrap_or(&self.dir));
                    self.pace(b.size as u64);
                    Ok(())
                }
                // 同名块已存在（并发或重跑）：内容 hash 相同 ⇒ 谁赢都一样，清掉自己的 tmp。
                Err(e) if final_path.exists() => {
                    let _ = fs::remove_file(&tmp);
                    let _ = e;
                    Ok(())
                }
                Err(e) => {
                    let _ = fs::remove_file(&tmp);
                    Err(Error::Io(e))
                }
            },
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    /// 把刚写进 CAS 的字节数记进速率闸。放在 `rename` **之后**：被拒/重名复用的那次不该扣预算。
    /// 从漏桶里扣 `n` 字节：不够就睡到够。
    ///
    /// 公开的理由是 §9.3 那句"删除动作全程挂 FR-15 令牌桶"：**删除也要过同一个桶**——
    /// 一次 unlink 几万个块文件在机械盘上同样是突发。写与删共用一个 `Pacer` 是刻意的：
    /// 两个各自的桶加起来就是双倍的速率，而配额是按"这台机器一共能吃多少 IO"定的。
    pub fn pace(&self, n: u64) {
        let mut guard = self.pacer.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(p) = guard.as_mut() {
            p.charge(n);
        }
    }

    /// 把库里一个块流式拷进 `dst`。**两个 hasher 各管各的**：
    /// - `block_hasher` 只装这一块，用来验块名（规矩 3）；
    /// - `whole` 累积整个文件，供 `read_blob` 末尾对清单 hash。
    ///
    /// 这里踩过一个真坑：早期只传一个 hasher，拿整文件累积值去比单块 hash——第一块
    /// 恰好相等所以所有用例都绿，直到"同一文件里有两个相同块"才炸。
    fn copy_out(
        &self,
        hash: &Hash,
        size: u32,
        dst: &mut impl Write,
        whole: &mut blake3::Hasher,
    ) -> Result<()> {
        let mut block_hasher = blake3::Hasher::new();
        let path = self.dir.join(Self::path_of(hash));
        // **缺失与损坏是两件事**（§9.4.7）：缺失可以合法（那一块还没同步到），损坏永远不合法。
        // 让 `NotFound` 裸穿出去，调用方就分不清"我这端没这东西"和"盘出事了"——
        // `push` 会把它报成一次 IO 故障，而它其实是"先同步过来再说"。
        let mut f = File::open(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::MissingObject {
                    hash: keys::short(hash),
                    kind: "块".to_string(),
                }
            } else {
                Error::Io(e)
            }
        })?;
        let mut buf = vec![0u8; CHUNK];
        let mut left = size as usize;
        while left > 0 {
            let want = left.min(CHUNK);
            let n = f.read(&mut buf[..want])?;
            if n == 0 {
                return Err(Error::CorruptBlock {
                    hash: keys::short(hash),
                    path: path.clone(),
                    why: format!("文件比清单短：还差 {left} 字节"),
                });
            }
            whole.update(&buf[..n]);
            block_hasher.update(&buf[..n]);
            dst.write_all(&buf[..n])?;
            left -= n;
        }
        if block_hasher.finalize().as_bytes() != hash {
            return Err(Error::CorruptBlock {
                hash: keys::short(hash),
                path,
                why: "块内容与其文件名（hash）不符".to_string(),
            });
        }
        Ok(())
    }
}

fn close_block(hasher: &mut blake3::Hasher, len: u64, zero: bool) -> BlockRef {
    let hash = if zero {
        ZERO_HASH
    } else {
        *hasher.finalize().as_bytes()
    };
    *hasher = blake3::Hasher::new();
    BlockRef {
        hash,
        size: len as u32,
        zero,
    }
}

/// 补零用的常量缓冲：`static` 而非 `const`，否则每个调用点都会复制一份 64 KiB 进栈帧。
static ZEROS: [u8; CHUNK] = [0u8; CHUNK];

fn write_zeros(dst: &mut impl Write, hasher: &mut blake3::Hasher, size: u32) -> Result<()> {
    let mut left = size as usize;
    while left > 0 {
        let want = left.min(CHUNK);
        hasher.update(&ZEROS[..want]);
        dst.write_all(&ZEROS[..want])?;
        left -= want;
    }
    Ok(())
}

fn temp_in(dir: &Path) -> PathBuf {
    dir.join(format!(".tmp.{}", tmp_tag()))
}

fn src_path_note(offset: u64) -> PathBuf {
    PathBuf::from(format!("(源文件 offset={offset})"))
}

/// rename 之后补一次目录 fsync：否则"文件已存在"这件事本身可能还没落盘。
/// Windows 没有可移植的目录 fsync，跳过并如实记录在 §15。
fn sync_dir(_dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = File::open(_dir) {
        let _ = d.sync_all();
    }
}
