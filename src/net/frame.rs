//! M1 的线格式（§9.4 / §7 session 七步）。
//!
//! # 为什么帧类型长这样
//!
//! 一次 `push` 真正要运的是三样东西：**头**（谁的哪些 commit）、**对象**（`B`/`T`/`C` 三类
//! 清单）、**块字节**（CAS 里的数据）。三样的尺寸差好几个数量级，所以**分开两种通道**：
//!
//! - **控制帧**：[`Proto`] 枚举，`postcard` 编码，前面挂 4 字节 BE 长度。单帧上限
//!   [`MAX_FRAME`]，超了直接拒收**再分配之前**（§9.4.5：磁盘写满是同机其他项目的第一杀手，
//!   而对端可控的就是"我要发多大一帧"）。
//! - **数据段**：块字节不进 `Proto`（否则一场 push 要把整批块先在内存里攒一份，直接顶穿
//!   NFR-1）。`PushObjects` 之后跟一段裸流：`[32B hash][u32 len][bytes]`，读一边写一边，
//!   内存恒定一个块。
//!
//! # 顺序无关，靠的是内容寻址
//!
//! 对象与块都以 hash 为身份 ⇒ **重复推送天然是 no-op**（§9.4 要点）。于是
//! "一批太大"就切成多帧发，收到哪帧算哪帧，不需要序号、不需要重传状态机；
//! 崩溃重放也只是"再推一遍"。v0.1 靠 `op_id` 才拿到的断点续传，这里是免费的。
//!
//! # 传输不在这一层
//!
//! [`Proto`] 只要求 `Read + Write`。真实通道是 `ssh <peer> twosync agent` 的 stdin/stdout
//! （§9.5：复用系统 sshd，不开新端口），测试里换成一对内存管道即可——**同一份会话代码**。
//! 所以这个模块不依赖 tokio、不依赖 russh：单核机上少一个常驻异步运行时就是少一份 CPU 预算。

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::store::cas::Blob;
use crate::store::keys::{self, Hash, ProjectId};
use crate::store::object::{Commit, TreeEntry};

/// 协议版本。只在**不兼容**时 +1；加可选字段不算（postcard 的枚举布局会变，所以
/// 新增消息类型算不算不兼容？算——老对端解不出未知变体 ⇒ 必须走版本协商，不许偷偷加）。
pub const PROTO_VER: u32 = 1;

/// 单帧上限。§9.4.5 的"PushBatch 单帧上限"就是这一条。
/// 8 MiB 的来由：一个目录清单在 20 万条目时约 10 MB ⇒ 超限就得多帧，
/// 而"一帧吃掉 8 MiB 内存"是 NFR-1 预算里可以接受的最坏值（进程峰值另有一笔引擎账）。
pub const MAX_FRAME: u32 = 8 << 20;

/// 一个数据段的块字节上限：**必须 ≥ 配置的 block_size**，否则大文件永远传不完。
/// 上限取 64 MiB（`--block-size` 的允许上界，见 `cli::config`）。
pub const MAX_SEGMENT: u32 = 64 << 20;

/// 能力位（`Hello.caps`）。两边按位与之后生效；**谁都不许假装支持自己没有的东西**。
pub mod caps {
    /// 定长分块（默认 4 MiB）。所有实现都必须支持，留位是为了 CDC 出现时能显式说明差异。
    pub const FIXED_CHUNK: u32 = 1 << 0;
    /// 变长 CDC：块身份随参数全变 ⇒ 没对上就等于两个库（§9.2 `chunk_param` 那条）。
    pub const CDC: u32 = 1 << 1;
    /// 稀疏：清单里有全零块标记。
    pub const SPARSE: u32 = 1 << 2;
    /// `have` 集合用 bloom 而不是全清单（十万块的项目里两者差一个数量级）。
    pub const BLOOM_HAVE: u32 = 1 << 3;
    /// §8 的执行位/符号链接语义。
    pub const XATTR_MODE: u32 = 1 << 4;
}

/// 对端自报的"我有哪些块"的两种表示（§9.4）。全清单是底线，bloom 是省流量的加解密。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum HaveSet {
    /// 精确列表。收到方直接按集合查。
    Full(Vec<Hash>),
    /// Bloom：**只能回答"一定没有/可能有"** ⇒ 有 = 还要逐块 `contains()` 复核（本地 stat），
    /// 没有 = 直接进 `Missing`。所以它可以少传很多字节而**不损失正确性**。
    Bloom { seeds: Vec<u64>, bits: u64 },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum Proto {
    /// 握手。`peer_id` 是 TOFU 指纹表的键（§9.5），**不签名**。
    Hello {
        proto_ver: u32,
        peer_id: Vec<u8>,
        caps: u32,
    },
    /// 开一场某项目的会话。`pid` 由 `project` 派生，两边都算一遍并核（见 `begin`）。
    BeginSession {
        project: String,
        pid: ProjectId,
        /// 发起方这一轮的 head 集合（它自己 commit 完的）。
        my_heads: Vec<Hash>,
    },
    /// 对端的 head 集合 + 它自报的可达祖先（用于 §7 的 union 判定与 `have` 采样）。
    HeadsState {
        heads: Vec<Hash>,
        ancestors: Vec<Hash>,
    },
    /// "这些块我缺，你有的话给我"。
    WantHave {
        want: Vec<Hash>,
    },
    /// 对方的答复：**它缺**的那些块。
    ///
    /// 这里故意不带字节数——接收方压根没有这些块，报不出准确大小，而发起方手里就有清单
    /// （每块 size 都在 `B` 里），配额该由它自己算。留一个"猜的字节数"只会让两边算出两个数。
    MissingBlocks {
        hashes: Vec<Hash>,
    },
    /// `WantHave` 的列表发完了（变长集合必须有个"说完了"，否则接收方不知道还要等多帧）。
    HaveDone,
    /// `MissingBlocks` 同上。
    MissingDone,
    /// 对象清单的最后一帧之后发一次，接收方由此进入 `VerifyCommit` 等待态。
    ObjectsDone,
    /// 对象推送：清单类小对象打包发。**可分多帧**，顺序无关（内容寻址）。
    PushObjects {
        blobs: Vec<Blob>,
        trees: Vec<Vec<TreeEntry>>,
        /// 随帧的 commit（可空：先推对象、最后一帧才带 commit）。
        commits: Vec<Commit>,
    },
    /// 块数据段的头：后面紧跟 `count` 段裸字节，每段 `[32B hash][u32 len][bytes]`。
    PushBlocks {
        count: u64,
        bytes: u64,
    },
    /// "把 hash 是这个的 commit 接进来"：接收方在这一帧之后**必须**过 FR-10 熔断与 §9.4.1 路径校验。
    VerifyCommit {
        hash: Hash,
    },
    /// 接受：`to_heads` 是接收方接受后的新 head 集合（只增不减 ⇒ 它必须包含 `from_heads`）。
    UnionAccept {
        from_heads: Vec<Hash>,
        to_heads: Vec<Hash>,
    },
    /// 拒绝的是**这一个 commit**，不是整场会话（§9.4 要点）。
    RejectCommit {
        hash: Hash,
        reason: String,
        /// 熔断时带上三个数，让发起方能看懂"为什么拒、要加哪个旗标"。
        mass_delete: Option<(u32, u32, u64)>,
    },
    /// 同一批路径被两个 head 各改过（M3 才有对象；类型先到位，`session` 里不许填假数据）。
    CollisionReport {
        paths: Vec<String>,
        heads: Vec<Hash>,
    },
    /// `gc` 的提案：保留策略 + 打算删多少。默认只要**建议**，删除由本端按 §9.3 决定。
    GcProposal {
        keep_versions: u32,
        keep_days: i64,
        quota_bytes: u64,
    },
    /// 出错了：消息原样打印。整场会话作废（与 `RejectCommit` 的区别就是"这条不行"还是"这次不行"）。
    Abort {
        msg: String,
    },
    EndSession {
        ok: bool,
    },
}

/// 帧的线上长度前缀字节数（BE）。
const LEN_BYTES: usize = 4;

/// 编码一帧：`[u32 BE 长度][postcard 体]`。超上限直接报错，不许发出自己都解不出的帧。
pub fn encode(w: &mut impl std::io::Write, msg: &Proto) -> Result<()> {
    let body = postcard::to_allocvec(msg)
        .map_err(|e| Error::Codec(format!("帧编码失败：{e}")))?;
    let n = body.len();
    if n > MAX_FRAME as usize {
        return Err(Error::BadInput(format!(
            "单帧 {n} B 超过上限 {} B：拆帧再发（内容寻址，多帧没有顺序要求）",
            MAX_FRAME
        )));
    }
    w.write_all(&(n as u32).to_be_bytes())?;
    w.write_all(&body)?;
    w.flush()?;
    Ok(())
}

/// 解一帧。**先查长度再分配**（§9.4.5：长度是对端可控的）。
pub fn decode(r: &mut impl std::io::Read) -> Result<Proto> {
    let mut head = [0u8; LEN_BYTES];
    r.read_exact(&mut head)
        .map_err(|e| map_eof(e, "读帧长度"))?;
    let n = u32::from_be_bytes(head) as usize;
    if n > MAX_FRAME as usize {
        return Err(Error::BadInput(format!(
            "对端声明单帧 {n} B，超过上限 {} B：拒绝，不分配",
            MAX_FRAME
        )));
    }
    let mut body = vec![0u8; n];
    r.read_exact(&mut body).map_err(|e| map_eof(e, "读帧体"))?;
    postcard::from_bytes(&body)
        .map_err(|e| Error::Codec(format!("帧解码失败（长度 {n}）：{e}")))
}

/// 对端半路断开与"真的读到底"是两件事：前者要说人话，后者要冒出去让调用方区分。
fn map_eof(e: std::io::Error, what: &str) -> Error {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("通道在{what}时断开"),
        ))
    } else {
        Error::Io(e)
    }
}

/// 写一个块数据段：`[hash][len][bytes]`，并**自己校验源字节与 hash 相符**——
/// 与 `cas::copy_block` 的"写前自证"同一族规矩：发出去的字节必须对得上它的名字，
/// 否则对端会把它当损坏拒收，而我们已经花掉了这段带宽。
pub fn write_segment(w: &mut impl std::io::Write, hash: &Hash, bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_SEGMENT as usize {
        return Err(Error::BadInput(format!(
            "单个块 {} B 超过上限 {MAX_SEGMENT} B",
            bytes.len()
        )));
    }
    if &keys::hash_of(bytes) != hash {
        return Err(Error::CorruptBlock {
            hash: keys::short(hash),
            path: format!("(发送前自证，{} B)", bytes.len()).into(),
            why: "要发的字节与它的块名不符 ⇒ 本地 CAS 已经坏了，不许推给对端".to_string(),
        });
    }
    w.write_all(hash)?;
    w.write_all(&(bytes.len() as u32).to_be_bytes())?;
    w.write_all(bytes)?;
    w.flush()?;
    Ok(())
}

/// 读一个块数据段，返回（hash, 字节）。**读的时候不校验**：校验在读完落盘之前做
/// 也来得及，而这里要防的是"对端报个天文数字长度把内存吃光"。
pub fn read_segment(r: &mut impl std::io::Read) -> Result<(Hash, Vec<u8>)> {
    let mut hash = [0u8; 32];
    r.read_exact(&mut hash).map_err(|e| map_eof(e, "读块 hash"))?;
    let mut len = [0u8; 4];
    r.read_exact(&mut len).map_err(|e| map_eof(e, "读块长度"))?;
    let n = u32::from_be_bytes(len) as usize;
    if n > MAX_SEGMENT as usize {
        return Err(Error::BadInput(format!(
            "对端声明一个块 {n} B，超过上限 {MAX_SEGMENT} B：拒绝，不分配"
        )));
    }
    let mut bytes: Vec<u8> = vec![0u8; n];
    r.read_exact(&mut bytes)
        .map_err(|e| map_eof(e, "读块字节"))?;
    Ok((hash, bytes))
}

/// `BeginSession` 的一致性核：`pid` 必须等于由 `project` 名字派生的那个。
///
/// 两边各自 `project add` 同一项目算出同一个 id 是 §9.2 的立身之本（名字派生，不是自增），
/// 这一条就是它在**线上**的守卫：不等就说明两侧对"这个项目叫什么"的理解已经分叉，
/// 继续走下去的结果是键空间错开、union 把一个项目当两个。宁可在这里停。
pub fn check_project_id(project: &str, pid: ProjectId) -> Result<()> {
    let want = keys::project_id(project);
    if want != pid {
        return Err(Error::BadInput(format!(
            "项目 {project:?} 的 id 对不上：对端报 {pid:#010x}，本地算出 {want:#010x} ⇒ \
             两边对同一个名字派生出了不同 id（改过算法？），先别同步"
        )));
    }
    Ok(())
}
