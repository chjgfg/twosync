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

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
}

impl Cas {
    pub fn new(objects_dir: &Path, block_size: u64) -> Result<Self> {
        fs::create_dir_all(objects_dir)?;
        Ok(Self {
            dir: objects_dir.to_path_buf(),
            block_size,
        })
    }

    /// 块在库内的相对路径：`<hash[0:2]>/<hash[2:]>`，两级目录避免单目录爆十万文件。
    pub fn path_of(hash: &Hash) -> PathBuf {
        let hex = keys::hex(hash);
        PathBuf::from(&hex[0..2]).join(&hex[2..])
    }

    pub fn contains(&self, hash: &Hash) -> bool {
        *hash == ZERO_HASH || self.dir.join(Self::path_of(hash)).exists()
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

    /// 把一个块从源文件的 `offset` 拷进库：tmp → fsync → 自证 hash → rename。
    fn copy_block(&self, src: &mut File, offset: u64, b: &BlockRef) -> Result<()> {
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
            out.sync_all()?; // ← 先 fsync 再 rename（FR-13）
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
        let mut f = File::open(&path)?;
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
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".tmp.{}.{seq:08x}", std::process::id()))
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
