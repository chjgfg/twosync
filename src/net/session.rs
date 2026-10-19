//! §7 那七步的实现，**与传输无关**：只要求对端是一根 `Read + Write` 的管子。
//!
//! 真实管子是 `ssh <peer> twosync agent` 的 stdin/stdout（§9.5：复用系统 sshd，不开新端口，
//! 也就不引入异步运行时——单核机上少一个常驻 runtime 就是少一份 CPU 预算）。测试里换成
//! 一对内存管道 ⇒ **同一份会话代码**，M1 的正确性不必等 SSH 才能验。
//!
//! # 帧的先后（两边必须一模一样，所以写在这里当唯一权威）
//!
//! ```text
//! → Hello                    ← Hello
//! → BeginSession             ← HeadsState
//! → WantHave* → HaveDone     ← MissingBlocks* → MissingDone
//! → PushBlocks{n,bytes} + n 段裸字节
//! → PushObjects* → ObjectsDone
//! → VerifyCommit{hash}       ← UnionAccept | RejectCommit
//! ```
//!
//! 变长集合（`WantHave`/`MissingBlocks`/`PushObjects`）**分多帧 + 一个终止帧**：一帧放不下
//! 百万个 hash（`MAX_FRAME` 8 MiB），而"说完了"这件事必须有显式信号，否则接收方只能猜。
//!
//! # 落盘顺序 = FR-13 的顺序，与网络到达顺序无关
//!
//! 清单引用一个块之前，块必须已在盘上。网络上谁先到我们管不住，所以接收侧按这个顺序落：
//! **数据段先写**（`Cas::put_bytes` 内部仍是 `tmp → fsync → rename` + 写前自证），
//! **清单随后**走 [`build::adopt_blob`]（与本地提交同一个函数），
//! **`VerifyCommit` 是最后一道闸**：过得了才动 head。
//!
//! 中途崩溃留下的是"没人引用的 `B`/`K` 行 + 已经落盘但没被引用的块"。那**不是损坏**：
//! §9.3 的引用计数 GC 干的就是这个（FR-13 的"半成品"由 journal 重放/丢弃，那是 M1 后半）。
//! 这条承诺现在**有用例兑着**：`tests/net_session.rs::objects_that_land_without_verify_are_gc_garbage`
//! 把对端掐在"对象全进库、`VerifyCommit` 没到"那一刻，然后要求 `sweep` 把 `C`/`T`/`B`/`K` 收成零、
//! 第二趟把块文件删净、站点还能被一次完整 push 追平。
//!
//! # 为什么接收侧要检查 `legal_name`
//!
//! 名字里带 `/`、`\`、`..` 的条目一旦进了树对象，要等 M3 检出时才发现就晚了——那时它已经是
//! 某个合法 commit 的一部分，撤回来要动 head。入库前先拒，代价是每条目一次字符串检查。

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};

use serde::{Deserialize, Serialize};
use storage_engines::KvEngine;

use crate::error::{Error, Result};
use crate::net::frame::{self, Proto, PROTO_VER};
use crate::store::build;
use crate::store::cas::{self, Cas};
use crate::store::history;
use crate::store::keys::{self, Hash, Kind, ProjectId};
use crate::store::object;

/// 一根双向管子。不要求 `Seek`：SSH 管道也不能 seek。
pub trait Io: Read + Write {}
impl<T: Read + Write> Io for T {}

/// 发起一次 push 的坐标。
pub struct PushOpts<'a> {
    pub project: &'a str,
    /// 本端身份（写进 `Hello`）。**这是站点身份，不是密钥**，见 [`crate::utils::site_id`]。
    pub peer_id: &'a [u8],
    /// 拨的那个对端的名字（`ssh` 的目标）。它就是 TOFU 表 `P` 行的键；**空 = 不做 TOFU**
    /// （单测里点对点跑协议时要的正是这个口子）。
    pub peer_name: &'a [u8],
    /// 本端能力位；两边按位与之后生效。
    pub caps: u32,
    /// §9.4.5 的会话配额：**发起方自己按自己的清单算**，不等对端报。
    pub max_block_bytes: u64,
    /// 越过 FR-10 熔断。注意这只影响**本端**；对端仍按它自己的闸判（那是 §9.4 要点）。
    pub allow_mass_delete: bool,
}

impl Default for PushOpts<'_> {
    fn default() -> Self {
        Self {
            project: "",
            peer_id: b"",
            peer_name: b"",
            caps: frame::caps::FIXED_CHUNK | frame::caps::SPARSE | frame::caps::XATTR_MODE,
            max_block_bytes: u64::MAX,
            allow_mass_delete: false,
        }
    }
}

/// 一次 push 的账。**`bytes_sent` 就是 §10.4 差分表的原材料**（与 git 比传输字节）。
#[derive(Debug, Default, Clone)]
pub struct Report {
    pub commits: usize,
    pub blobs_sent: usize,
    pub trees_sent: usize,
    /// 协商之后真正上线的块数与字节数。
    pub blocks_sent: usize,
    pub bytes_sent: u64,
    /// 对端已经有、**没上线路**的块数：这就是去重的直接读数。
    pub blocks_skipped: usize,
    /// 对端接受后的 head 集合（多 head 是正常态，§4.1）。
    pub accepted: Vec<Hash>,
    /// 两边交集后的能力位。
    pub caps: u32,
    /// `true` = 这个对端是**第一次**见到，身份刚刚入表（§9.5.1 的告警归命令层打）。
    pub first_contact: bool,
}

// ── TOFU 表（§9.5.1，`P` 前缀）────────────────────────────────────────────
//
// 表放在**引擎里**而不是 `peers.json`：一份数据两处存就要同步，而同步是漂移的产地（§15.15 那条
// 同源教训）。要看就 `peer list` 从库里读。`P` 行没有 project 前缀（它是站点级的，§9.2）。

/// 一条对端记录。**只有"它自报的站点身份 + 第一次见到的时间"**：没有密钥、没有签名（§9.5.2）。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub site_id: Vec<u8>,
    /// epoch **纳秒**（与 `Commit::ts` 同一个单位，`history::fmt_ts` 直接能打印）。
    pub first_seen: i64,
}

/// 全部已知对端 `(名字, 记录)`，按名字排序。
pub fn read_peers(db: &dyn KvEngine) -> Result<Vec<(String, Peer)>> {
    let prefix = [keys::TAG_PEER];
    let tx = db.begin();
    let rows = tx.prefix_scan(&prefix);
    tx.commit();
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        // 键 = `P|名字`，所以名字就是标签之后那一段（它可以含任意字节，显示时按 lossy 处理）。
        let Some(name) = r.key.get(1..) else { continue };
        let Some(raw) = r.value else { continue };
        let body = keys::strip(&raw)?;
        let p: Peer = postcard::from_bytes(body)
            .map_err(|e| Error::Codec(format!("Peer 解码失败：{e}")))?;
        out.push((String::from_utf8_lossy(name).into_owned(), p));
    }
    // 只按名字排：`Peer` 没有序（它是个记录，不是个可比较的量），而"表按对端名排序"才是人想要的。
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// 忘掉一个对端（下次连接重新 TOFU）。返回是否真有这一条。
pub fn forget_peer(db: &dyn KvEngine, name: &[u8]) -> Result<bool> {
    let key = keys::peer_key(name);
    let tx = db.begin();
    let existed = tx.get(&key).is_some();
    if existed {
        tx.delete(&key);
    }
    tx.commit();
    Ok(existed)
}

/// 核身份：没见过就记下并返回 `true`（首连），见过且一致返回 `false`，不一致**报错**。
fn check_or_record_peer(db: &dyn KvEngine, name: &[u8], their_site: &[u8]) -> Result<bool> {
    let key = keys::peer_key(name);
    let tx = db.begin();
    let recorded = match tx.get(&key) {
        Some(raw) => Some(
            postcard::from_bytes::<Peer>(keys::strip(&raw)?)
                .map_err(|e| Error::Codec(format!("Peer 解码失败：{e}")))?,
        ),
        None => None,
    };
    tx.commit();
    if let Some(p) = &recorded {
        if p.site_id != their_site {
            return Err(Error::PeerIdentityChanged {
                peer: String::from_utf8_lossy(name).into_owned(),
                want: hex(&p.site_id),
                got: hex(their_site),
            });
        }
        return Ok(false);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as i64;
    let body = postcard::to_allocvec(&Peer { site_id: their_site.to_vec(), first_seen: now })
        .map_err(|e| Error::Codec(format!("Peer 编码失败：{e}")))?;
    let tx = db.begin();
    if !tx.set(&key, keys::wrap(body)) {
        return Err(Error::WriteConflict(hex(name)));
    }
    tx.commit();
    Ok(true)
}

/// 身份是字节，报错与打印都要看得见：小写 hex，不打印不可见字符。
pub fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        use std::fmt::Write as _;
        let _ = write!(s, "{x:02x}");
    }
    s
}

/// 服务端一场的账（`agent` 打印 + NFR-10 溯源）。
#[derive(Debug, Default, Clone)]
pub struct ServeReport {
    pub project: String,
    pub blobs_in: usize,
    pub trees_in: usize,
    pub blocks_written: usize,
    pub bytes_in: u64,
    /// 被 FR-10 拒了还是接了。
    pub accepted: bool,
    pub reason: String,
    pub heads_after: Vec<Hash>,
    /// 本端这场是**发送方**（对面 `pull` 我们）时，这里带的是 `push_body` 那份账；接收方是 `None`。
    ///
    /// 为什么不摊平成上面那几个 `*_in` 字段：发送方关心"上线/跳过"，接收方关心"落盘/入库"，
    /// 两组数**语义不同**，混在一列里迟早有人拿错的那个数去填 §10.4 的差分表。
    pub sent: Option<Report>,
    /// `true` = 这个对端是**第一次**见到，身份刚刚入表（§9.5.1）。只有发起方那侧可能是真。
    pub first_contact: bool,
}

/// 发起端。
pub fn push(io: &mut impl Io, db: &dyn KvEngine, cas: &Cas, opts: &PushOpts<'_>) -> Result<Report> {
    let pid = keys::project_id(opts.project);
    let mut rep = Report::default();

    // ① 握手。协议版本不等就直接停：让两边算出同一棵树是本项目全部承诺的地基。
    frame::encode(
        io,
        &Proto::Hello {
            proto_ver: PROTO_VER,
            peer_id: opts.peer_id.to_vec(),
            caps: opts.caps,
        },
    )?;
    let theirs = match frame::decode(io)? {
        Proto::Hello { proto_ver, caps, peer_id } => (proto_ver, caps, peer_id),
        other => return Err(unexpected(&other, "Hello")),
    };
    if theirs.0 != PROTO_VER {
        return Err(Error::BadInput(format!(
            "对端协议版本 {} ≠ 本端 {PROTO_VER}：先对齐版本再同步",
            theirs.0
        )));
    }
    rep.caps = opts.caps & theirs.1;

    // ①' §9.5.1 TOFU：**在说出任何一个项目名字之前**先核对对端身份。
    // 首连 = 入表并告警；不符 = 一字节都不发就停。顺序是这条的全部价值——晚一步就已经把
    // 项目名、head 集合与历史深度泄露给了一个不是对方的机器。
    if !opts.peer_name.is_empty() {
        rep.first_contact = check_or_record_peer(db, opts.peer_name, &theirs.2)?;
    }

    // ② 开场：带上本地 head 集合，换回对方的。
    let my_heads = build::ref_heads(db, pid, build::MAIN)?;
    if my_heads.is_empty() {
        return Err(Error::BadInput(format!(
            "项目 {:?} 本端还没有任何 head（先 scan + commit）",
            opts.project
        )));
    }
    frame::encode(
        io,
        &Proto::BeginSession {
            project: opts.project.to_string(),
            pid,
            my_heads: my_heads.clone(),
        },
    )?;
    let (their_heads, their_ancestors) = match frame::decode(io)? {
        Proto::HeadsState { heads, ancestors } => Ok((heads, ancestors)),
        // 对端在报名字之后就Abort = 它不认这个项目（没注册，或注册了没配对，§9.5.3）。
        // 不接这一支的话这里会报"期望 HeadsState，收到 Abort{…}"，把一条**有原因的拒绝**
        // 说成一次协议走样——那会把人往错的方向查。
        Proto::Abort { msg } => Err(Error::BadInput(format!("对端拒绝开场：{msg}"))),
        other => Err(unexpected(&other, "HeadsState")),
    }?;

    // ③–⑦ 发送方那一半。**抽成一个函数不是为了好看**：`pull` 的反方向会话里，
    // 本端是接收方、对面是发送方，而对面跑的就是这一份实现 —— 两条路必须同一条代码，
    // 否则"push 能过、pull 出来的东西却过不了闸"这种分裂迟早会出现在第二份实现里。
    push_body(
        io,
        db,
        cas,
        opts,
        &SendCtx {
            pid,
            my_heads: &my_heads,
            their_heads: &their_heads,
            their_ancestors: &their_ancestors,
        },
        &mut rep,
    )?;
    Ok(rep)
}

/// 发送方一场的"两边各有什么"。收成结构而不是四个位置参数（同 `ReceiveCtx`、`PushOpts`）。
#[derive(Clone, Copy)]
pub struct SendCtx<'a> {
    pub pid: ProjectId,
    /// **发送方**自己的 head 集合（非空，调用方保证）。
    pub my_heads: &'a [Hash],
    /// 接收方的 head 集合与它自报的祖先：差集算的就是"我可达 \ 它已知的这些"。
    pub their_heads: &'a [Hash],
    pub their_ancestors: &'a [Hash],
}

/// 发送方的步骤 ③–⑦：算差集 → 收集要运的对象与块 → `want/have` 协商 → 先块后清单 → 等 verdict。
///
/// 握手（`Hello`）、TOFU、开场（`BeginSession`/`PullRequest`）都由调用方做完：那三段在
/// "我拨出去"与"别人拨我、让我发"两种情形里不一样，而这一段完全一样。
pub fn push_body(
    io: &mut impl Io,
    db: &dyn KvEngine,
    cas: &Cas,
    opts: &PushOpts<'_>,
    ctx: &SendCtx<'_>,
    rep: &mut Report,
) -> Result<()> {
    let SendCtx { pid, my_heads, their_heads, their_ancestors } = *ctx;
    // ③ union 判定：要推的 = 从我方 head 可达、且不在"对端 head + 对端自报祖先"里的那些。
    let mine = reachable(db, pid, my_heads, &[]);
    let theirs_reachable = reachable(db, pid, their_heads, their_ancestors);
    let to_send = topo_pending(db, pid, &mine, &theirs_reachable);
    rep.commits = to_send.len();

    // ④ 收集要运的对象与块。清单先攒在手里，分帧时按估算切（编码器还会兜一次真上限）。
    //
    // **送差集，不是送整棵树**（§17 补记廿三：12 GiB 站点上这一条值 3 MiB 管子与两万次对端写盘）。
    // 块有 `want/have` 协商，所以脏块只发一次；清单原来没有协商 ⇒ 每次推送都把待发 commit 的**全部**
    // `B`/`T` 重新发一遍，于是"改 1 MiB"变成"发 20 011 份清单"。现在拿亲代当基线，同名同 hash 的
    // 子项整块跳过（那一棵子树逐字节相同 ⇒ 对面早就有）。
    let mut out = Outbound::default();
    for h in &to_send {
        let c = object::read_commit(db, pid, h)?;
        let base = c
            .parents
            .iter()
            .find(|p| theirs_reachable.contains(*p))
            .map(|p| object::read_commit(db, pid, p).map(|pc| pc.tree))
            .transpose()?;
        collect_changed(db, pid, &c.tree, base.as_ref(), &mut out)?;
        out.commits.push(c);
    }
    // ⑤ 块协商。配额按**协商之后**真正要上线的字节算（见下面 `send_bytes`），
    // 所以这里不先估一份"想要的总量"——那个数在对面有库存时永远是高的。
    for chunk in chunk_hashes(&out.need) {
        frame::encode(io, &Proto::WantHave { want: chunk })?;
    }
    frame::encode(io, &Proto::HaveDone)?;
    let mut missing: BTreeSet<Hash> = BTreeSet::new();
    loop {
        match frame::decode(io)? {
            Proto::MissingBlocks { hashes } => missing.extend(hashes),
            Proto::MissingDone => break,
            Proto::Abort { msg } => return Err(Error::BadInput(format!("对端中止：{msg}"))),
            other => return Err(unexpected(&other, "MissingBlocks/MissingDone")),
        }
    }

    let to_write: Vec<Hash> = out
        .need
        .iter()
        .cloned()
        .filter(|h| missing.contains(h))
        .collect();
    rep.blocks_skipped = out.need.len() - to_write.len();
    let send_bytes: u64 = to_write
        .iter()
        .map(|h| *out.block_size.get(h).unwrap_or(&0) as u64)
        .sum();
    if send_bytes > opts.max_block_bytes {
        frame::encode(
            io,
            &Proto::Abort {
                msg: format!(
                    "对端缺 {} 个块 / {:.1} MiB，超过本端会话配额 {:.1} MiB：一个字没发",
                    to_write.len(),
                    send_bytes as f64 / 1048576.0,
                    opts.max_block_bytes as f64 / 1048576.0
                ),
            },
        )?;
        return Err(Error::BadInput(format!(
            "没发任何东西：对端缺 {:.1} MiB，配额只剩 {:.1} MiB",
            send_bytes as f64 / 1048576.0,
            opts.max_block_bytes as f64 / 1048576.0
        )));
    }

    // ⑥ 先块后清单。
    if !to_write.is_empty() {
        frame::encode(
            io,
            &Proto::PushBlocks {
                count: to_write.len() as u64,
                bytes: send_bytes,
            },
        )?;
        for h in &to_write {
            let size = *out.block_size.get(h).unwrap_or(&0);
            let body = cas.read_block(h, size)?;
            frame::write_segment(io, h, &body)?;
            // 上行字节也过**同一个桶**（FR-15 / NFR-3"总预算不叠加"）：`daemon` 一个进程里
            // scan 写盘、commit 落块、push 上行、gc 删除用的是同一个 `Bucket`（`Cas::with_bucket`）。
            // 放在 `write_segment` 之后：没发出去的不该扣预算（与 CAS 的 `rename` 之后才扣同理）。
            cas.pace(body.len() as u64);
            rep.blocks_sent += 1;
            rep.bytes_sent += body.len() as u64;
        }
    }
    for frame_body in pack_objects(&out) {
        // 计数按**对象**数，不按帧数：一帧能装多少是 `MAX_FRAME` 决定的实现细节，
        // 拿帧数当"传了几份清单"会在大包库上少报一个量级（分帧边界一动，读数就跟着跳）。
        if let Proto::PushObjects {
            blobs: fb,
            trees: ft,
            ..
        } = &frame_body
        {
            rep.blobs_sent += fb.len();
            rep.trees_sent += ft.len();
        }
        frame::encode(io, &frame_body)?;
    }
    frame::encode(io, &Proto::ObjectsDone)?;

    // ⑦ 收尾：验最后一个 commit。**没有新东西时也要验**——接收方因此只有一条代码路径，
    // 不用猜"对方是不是什么都没推"。
    let newest = to_send.last().unwrap_or(&my_heads[0]);
    frame::encode(io, &Proto::VerifyCommit { hash: *newest })?;
    match frame::decode(io)? {
        Proto::UnionAccept { to_heads, .. } => {
            rep.accepted = to_heads;
        }
        Proto::RejectCommit { reason, .. } => {
            return Err(Error::BadInput(format!(
                "对端拒绝 commit {}：{reason}",
                keys::short(newest)
            )));
        }
        other => return Err(unexpected(&other, "UnionAccept/RejectCommit")),
    }
    Ok(())
}

/// 发起一次 `pull`：握手 → TOFU → `PullRequest`（本端的 head 集合 + 自报祖先 + 愿意收的配额），
/// 然后本端跑**接收方那一半**。对面在 `agent` 里变成发送方（`serve` 的 `Side::Send`）。
///
/// 为什么不开第二套协议：这里的每一道闸（FR-10 熔断、§9.4.1 路径校验、缺块自证、闭合性差集）
/// 都住在**收的那一侧**，与谁拨号无关。写第二份就迟早有一份漏掉闸——本项目已经为"两份实现"买过单。
///
/// 本端**没有 head 也能 pull**（对面可能比我们早）：那 `my_heads` 是空集合，对面算出的差集
/// 就是它的全部历史，本端从第一版开始收。`push` 那边没有这一条（没 head 就没东西可推），
/// 所以这里不照抄那个报错。
pub fn pull(
    io: &mut impl Io,
    db: &dyn KvEngine,
    cas: &Cas,
    opts: &PushOpts<'_>,
) -> Result<ServeReport> {
    let pid = keys::project_id(opts.project);
    let mut rep = ServeReport {
        project: opts.project.to_string(),
        ..Default::default()
    };

    // ① 握手。与 `push` 一字一样：版本不等就停，别让两边各算各的树。
    frame::encode(
        io,
        &Proto::Hello {
            proto_ver: PROTO_VER,
            peer_id: opts.peer_id.to_vec(),
            caps: opts.caps,
        },
    )?;
    let theirs = match frame::decode(io)? {
        Proto::Hello {
            proto_ver,
            caps,
            peer_id,
        } => (proto_ver, caps, peer_id),
        other => return Err(unexpected(&other, "Hello")),
    };
    if theirs.0 != PROTO_VER {
        return Err(Error::BadInput(format!(
            "对端协议版本 {} ≠ 本端 {}：先对齐版本再同步",
            theirs.0, PROTO_VER
        )));
    }

    // ①' TOFU：**在说出任何一个项目名字之前**先核对对端身份（与 `push` 同一条顺序、同一个理由）。
    if !opts.peer_name.is_empty() {
        rep.first_contact = check_or_record_peer(db, opts.peer_name, &theirs.2)?;
    }

    // ② 反方向开场：把"我有什么"一次说全，对面才有办法算差集。
    let my_heads = build::ref_heads(db, pid, build::MAIN)?;
    let known = reachable(db, pid, &my_heads, &[]);
    let mut anc: Vec<Hash> = known.iter().cloned().collect();
    anc.sort();
    frame::encode(
        io,
        &Proto::PullRequest {
            project: opts.project.to_string(),
            pid,
            heads: my_heads.clone(),
            ancestors: anc,
            max_bytes: opts.max_block_bytes,
        },
    )?;

    // ③ 对面先报它自己的 head 集合（`UnionAccept.from_heads` 要用），然后开始发。
    //     它自报的祖先本端**用不上**：要算差集的是发送方，而它已经从上面的 `PullRequest` 拿到了我们这一侧。
    let their_heads = match frame::decode(io)? {
        Proto::HeadsState { heads, .. } => heads,
        Proto::Abort { msg } => {
            return Err(Error::BadInput(format!("对端拒绝开场：{msg}")));
        }
        other => return Err(unexpected(&other, "HeadsState")),
    };
    serve_body(
        io,
        db,
        cas,
        &ReceiveCtx {
            pid,
            our_heads: &my_heads,
            their_heads: &their_heads,
            known: &known,
            allow_mass_delete: opts.allow_mass_delete,
        },
        &mut rep,
    )?;
    Ok(rep)
}

/// 服务端一场会话。`resolve` 把项目名换成本端 pid，**换不成就给一句给人看的原因**。
///
/// 为什么是 `Result` 而不是 `Option`：§9.5.3 之后"认不得这个项目"有两种完全不同的理由——
/// 没注册（`project add`）与注册了但没配对（`project pair`）。把它们都说成"没有这个项目"，
/// 对端就只能猜；而猜的表现是一边反复重试一条永远不会被接受的会话。
///
/// 一条连接上可以连着调用多次 ⇒ FR-19 的"20 个 project 一轮只 1 次认证"就是外层循环：
/// 每场会话只多一轮 `BeginSession`/`HeadsState`，不再重新拨号、不再重新输密码。
pub fn serve(
    io: &mut impl Io,
    db: &dyn KvEngine,
    cas: &Cas,
    my_peer_id: &[u8],
    my_caps: u32,
    resolve: &mut dyn FnMut(&str) -> std::result::Result<ProjectId, String>,
    allow_mass_delete: bool,
) -> Result<ServeReport> {
    let mut rep = ServeReport::default();

    let (their_ver, their_caps) = match frame::decode(io)? {
        Proto::Hello { proto_ver, caps, .. } => (proto_ver, caps),
        other => return Err(unexpected(&other, "Hello")),
    };
    if their_ver != PROTO_VER {
        let msg = format!("对端协议版本 {their_ver} ≠ 本端 {PROTO_VER}");
        frame::encode(io, &Proto::Abort { msg: msg.clone() })?;
        return Err(Error::BadInput(msg));
    }
    frame::encode(
        io,
        &Proto::Hello {
            proto_ver: PROTO_VER,
            peer_id: my_peer_id.to_vec(),
            caps: my_caps & their_caps,
        },
    )?;

    // 开场帧决定本端这一场站哪一边：`BeginSession` = 对面推给我们（收），`PullRequest` =
    // 对面要我们推过去（发）。§16.3 那句"拨号方向对称"落到实现上就是这一个 match。
    enum Side {
        Receive,
        Send,
    }
    let (project, pid_claimed, their_heads, their_ancestors, side, quota) = match frame::decode(io)? {
        Proto::BeginSession {
            project,
            pid,
            my_heads,
        } => (project, pid, my_heads, Vec::new(), Side::Receive, u64::MAX),
        Proto::PullRequest {
            project,
            pid,
            heads,
            ancestors,
            max_bytes,
        } => (project, pid, heads, ancestors, Side::Send, max_bytes),
        other => return Err(unexpected(&other, "BeginSession/PullRequest")),
    };
    frame::check_project_id(&project, pid_claimed)?;
    let pid = match resolve(&project) {
        Ok(p) => p,
        Err(why) => {
            // 原因原样回给对方：他那边看到的是"我对端不肯收这个项目"，而不是一句我们替他编的猜测。
            let msg = format!("本端拒绝项目 {project:?}：{why}");
            frame::encode(io, &Proto::Abort { msg: msg.clone() })?;
            return Err(Error::BadInput(msg));
        }
    };
    rep.project = project.clone();

    let our_heads = build::ref_heads(db, pid, build::MAIN)?;
    let known = reachable(db, pid, &our_heads, &[]);
    match side {
        Side::Receive => {
            let mut anc: Vec<Hash> = known.iter().cloned().collect();
            anc.sort();
            frame::encode(
                io,
                &Proto::HeadsState {
                    heads: our_heads.clone(),
                    ancestors: anc,
                },
            )?;
            // 接收方那一半。抽出来与 `push_body` 对称：`pull` 的时候本端就走这条路，而开场那
            // 几帧在两种方向里形状不同，这一段一个字都一样。
            serve_body(
                io,
                db,
                cas,
                &ReceiveCtx {
                    pid,
                    our_heads: &our_heads,
                    their_heads: &their_heads,
                    known: &known,
                    allow_mass_delete,
                },
                &mut rep,
            )?;
        }
        Side::Send => {
            // 本端当发送方。对面已经把它的 head 集合与自报祖先带来了 —— 那正是差集要的两样，
            // 所以这里**不再发 `HeadsState`**：发了就变成两边各等一帧，会话卡死。
            if our_heads.is_empty() {
                let msg = format!("本端项目 {project:?} 还没有任何 head：没有东西可发（先 scan + commit）");
                frame::encode(io, &Proto::Abort { msg: msg.clone() })?;
                return Err(Error::BadInput(msg));
            }
            // 配额这一趟换了声明的人：`pull` 的那一方在 `PullRequest.max_bytes` 里说它愿意收多少，
            // 本端照它自我了断（超了就 `Abort`，一个字不发）。§9.4.5 的"发起方自己算清单"没被削弱：
            // 算清单的还是发送方，只是上限由收货的一方给——那本来就是收货方的预算。
            let caps = my_caps & their_caps;
            // 先把本端的 head 集合报过去，再当发送方。`pull` 的差集不需要它（对面在
            // `PullRequest` 里已经把它那份交来了），但**接收方需要**：`UnionAccept.from_heads`
            // 要原样带回"你当时有哪些 head"，而那一半的账是按头算的。
            {
                let mut anc: Vec<Hash> = known.iter().cloned().collect();
                anc.sort();
                frame::encode(
                    io,
                    &Proto::HeadsState {
                        heads: our_heads.clone(),
                        ancestors: anc,
                    },
                )?;
            }
            let opts = PushOpts {
                project: &project,
                peer_id: my_peer_id,
                // 空 = 不做 TOFU：本端是被拨的那一侧，对端身份由**它**在开场时核我们（§9.5.1）。
                peer_name: &[],
                caps,
                max_block_bytes: quota,
                allow_mass_delete,
            };
            let mut sent = Report { caps, ..Default::default() };
            push_body(
                io,
                db,
                cas,
                &opts,
                &SendCtx {
                    pid,
                    my_heads: &our_heads,
                    their_heads: &their_heads,
                    their_ancestors: &their_ancestors,
                },
                &mut sent,
            )?;
            rep.accepted = true;
            rep.reason = "本端是发送方：对面已验收".to_string();
            rep.heads_after = sent.accepted.clone();
            rep.sent = Some(sent);
        }
    }
    Ok(rep)
}

/// 接收方一场的上下文。收成结构而不是八个位置参数（同 `build::Meta`、`PushOpts` 那条理由）。
#[derive(Clone, Copy)]
pub struct ReceiveCtx<'a> {
    pub pid: ProjectId,
    /// 本端当前的 head 集合（验收时 union 的那一半）。
    pub our_heads: &'a [Hash],
    /// 对面报上来的 head 集合（`UnionAccept.from_heads` 要原样带回去）。
    pub their_heads: &'a [Hash],
    /// 本端**已经可达**的对象闭包。差集那道闸要用它当基线（§15.22），所以由调用方算一次
    /// 传进来：同一场会话里它被问很多次，每次重算就是又一次全库折叠。
    pub known: &'a BTreeSet<Hash>,
    /// 本端的 FR-10 旗标。**对端的旗标不跨机器传**（§9.4 要点）。
    pub allow_mass_delete: bool,
}

/// 接收方：`want/have` 答复 → 块落盘（FR-13 的顺序在这里）→ 收清单 → 过闸 → 回 verdict。
pub fn serve_body(
    io: &mut impl Io,
    db: &dyn KvEngine,
    cas: &Cas,
    ctx: &ReceiveCtx<'_>,
    rep: &mut ServeReport,
) -> Result<()> {
    let ReceiveCtx { pid, our_heads, their_heads, known, allow_mass_delete } = *ctx;
    // 收 want 集合。
    let mut want: BTreeSet<Hash> = BTreeSet::new();
    loop {
        match frame::decode(io)? {
            Proto::WantHave { want: w } => want.extend(w),
            Proto::HaveDone => break,
            other => return Err(unexpected(&other, "WantHave/HaveDone")),
        }
    }
    let missing: Vec<Hash> = want
        .iter()
        .filter(|h| !cas.contains(h))
        .cloned()
        .collect();
    for chunk in chunk_hashes(&missing) {
        frame::encode(io, &Proto::MissingBlocks { hashes: chunk })?;
    }
    frame::encode(io, &Proto::MissingDone)?;

    // 块：**先落盘**，FR-13 的顺序在这里成立。没有块要发时发起方直接给对象帧，
    // 所以这里用"当前帧"往下推，而不是每个分支各读一次（各读一次会吃掉一帧）。
    let mut msg = frame::decode(io)?;
    if let Proto::PushBlocks { count, bytes } = msg {
        {
            rep.bytes_in = bytes;
            for _ in 0..count {
                let (hash, body) = frame::read_segment(io)?;
                if cas.contains(&hash) {
                    continue; // 重放/并发：同 hash 同内容，谁先到都一样
                }
                let out = cas.put_bytes(&body)?;
                if out.blob.hash != hash || out.blob.blocks.len() != 1 {
                    // 段的名字与内容不符 = 传输或本地 CAS 出了问题，绝不能当"缺块"混过去。
                    let msg = format!(
                        "收到的块自称 {}，内容算出来是 {}：拒绝整场会话",
                        keys::short(&hash),
                        keys::short(&out.blob.hash)
                    );
                    frame::encode(io, &Proto::Abort { msg: msg.clone() })?;
                    return Err(Error::CorruptBlock {
                        hash: keys::short(&hash),
                        path: "(网络接收)".into(),
                        why: msg,
                    });
                }
                rep.blocks_written += 1;
            }
        }
        msg = frame::decode(io)?;
    }

    // 清单与 commit。
    //
    // `got` 记下**这一场会话真的收到过**哪些 tree/blob：验收那道闸要用它做差集
    // （[`verify_closure_delta`]，§15.22）。
    let mut got = Got::default();
    loop {
        match msg {
            Proto::PushObjects {
                blobs,
                trees,
                commits,
            } => {
                for entries in &trees {
                    for e in entries {
                        keys::legal_name(&e.name)
                            .map_err(|er| note_abort(io, &er.to_string()).err().unwrap_or(er))?;
                    }
                    let h = object::put_tree(db, pid, entries)?;
                    // 树的身份就是条目集合的 blake3，`put_tree` 落盘前已经自证过 ⇒ 直接用它返回的 hash。
                    got.trees.insert(h);
                    rep.trees_in += 1;
                }
                for b in &blobs {
                    build::adopt_blob(db, pid, b, "push")
                        .map_err(|er| note_abort(io, &er.to_string()).err().unwrap_or(er))?;
                    got.blobs.insert(b.hash);
                    rep.blobs_in += 1;
                }
                for c in &commits {
                    object::put_commit(db, pid, c)?;
                }
            }
            Proto::ObjectsDone => break,
            other => return Err(unexpected(&other, "PushObjects/ObjectsDone")),
        }
        msg = frame::decode(io)?;
    }

    // 验收：闭合性 + 熔断 + head 前移。
    //
    // **这一读的 EOF 与"管子干净收尾"是两件事**，所以要单独报成错误而不是被 `agent` 的 EOF 短路咽掉：
    // 走到这里意味着块与清单**都已经进库**（`adopt_blob` 落过 `B`/`K` 行），只差最后一道闸。
    // 对端在这之后把连接断了，本站点就留下一整套没有任何 ref 可达的对象——那是垃圾而不是损坏，
    // 但它必须**看得见**（否则 `agent` 报"正常退出"，而实际上有一场会话被截断过）。
    // 回收由 §9.3 的 mark-sweep 负责：保留集是"全部 ref 的可达闭包"，这一整套不在里面。
    let newest = match frame::decode(io) {
        Ok(Proto::VerifyCommit { hash }) => hash,
        Ok(other) => return Err(unexpected(&other, "VerifyCommit")),
        Err(e) => {
            if matches!(&e, Error::Io(io) if io.kind() == std::io::ErrorKind::UnexpectedEof) {
                return Err(Error::BadInput(
                    "会话在 `VerifyCommit` 之前管子到尾：块与清单已经收下、head 一步没动 ⇒ \
                     库里现在有一整套**没有人引用**的对象，`gc --sweep --apply` 会把它收掉（§9.3）\n  \
                     这不算会话失败：动 head 是最后一步，没走完 = 什么都没生效（FR-13）".to_string(),
                ));
            }
            return Err(e);
        }
    };
    let verdict = accept(
        db,
        cas,
        &Accept {
            pid,
            our_heads,
            their_heads,
            known,
            newest: &newest,
            allow_mass_delete,
            got: &got,
        },
    );
    match verdict {
        Ok((to_heads, why)) => {
            rep.accepted = true;
            rep.reason = why;
            rep.heads_after = to_heads.clone();
            frame::encode(
                io,
                &Proto::UnionAccept {
                    from_heads: their_heads.to_vec(),
                    to_heads,
                },
            )?;
        }
        Err(e) => {
            rep.accepted = false;
            rep.reason = e.to_string();
            frame::encode(
                io,
                &Proto::RejectCommit {
                    hash: newest,
                    reason: e.to_string(),
                    mass_delete: mass_delete_of(&e),
                },
            )?;
        }
    }
    Ok(())
}

// ── 验收 ──────────────────────────────────────────────────────────────────

/// 接一个 commit：先确认它**闭得上**（对象齐、块在盘上），再对**新出现的**每个 commit 过
/// FR-10 熔断，最后才动 head。顺序错了就会出现"head 指向一个取不回来的版本"。
///
/// 坐标收成 [`Accept`] 不只是躲 `too_many_arguments`：这几样必须**来自同一场会话**——`known` 是本场
/// "收之前本端可达集"，`their_heads` 是对端本场报的头。拆成一排参数就会出现"拿上一场的 known 判这一场"，
/// 而那种错的表象是一次本该拦住的熔断放行了。
fn accept(db: &dyn KvEngine, cas: &Cas, a: &Accept<'_>) -> Result<(Vec<Hash>, String)> {
    let pid = a.pid;
    let allow = a.allow_mass_delete;
    let incoming: Vec<Hash> = {
        let now = reachable(db, pid, &[*a.newest], &[]);
        let mut v: Vec<Hash> = now.difference(a.known).cloned().collect();
        v.sort();
        v
    };
    let mut why = String::new();
    for h in &incoming {
        let c = object::read_commit(db, pid, h)?;
        // 差集基线 = 这个 commit **在本端读得到的那一个亲代**（发送端用的是"在对面可达集里的第一个亲代"，
        // 两边用同一棵亲代树 ⇒ 规则对称；对不上就是悬空引用，报出去而不是静默接受）。
        let parent_tree = match c.parents.iter().find(|p| object::read_commit(db, pid, p).is_ok()) {
            Some(p) => Some(object::read_commit(db, pid, p)?.tree),
            None => None,
        };
        let mut closed = 0usize;
        let inherited = verify_closure_delta(db, pid, cas, &c.tree, parent_tree.as_ref(), a.got, &mut closed)?;
        if let Some(pt) = &parent_tree {
            let known = count_entries(db, pid, pt)?;
            let d = build::diff(db, pid, Some(pt), &c.tree)?;
            build::check_mass_delete(d.delete, d.deleted_bytes, known, allow)?;
            if d.delete > 0 {
                why = format!(
                    "接受 {}：{} 个对象闭合、{} 个从未变的上代继承，删除 {} 条 / {:.1} MiB 在闸内",
                    keys::short(h),
                    closed,
                    inherited,
                    d.delete,
                    d.deleted_bytes as f64 / 1048576.0
                );
            }
        } else {
            why = format!(
                "接受 {}：{} 个对象闭合、{} 个继承（首个提交或亲代不在本端 ⇒ 全量校验）",
                keys::short(h),
                closed,
                inherited
            );
        }
    }
    let mut cand: Vec<Hash> = a.our_heads.to_vec();
    cand.extend_from_slice(a.their_heads);
    cand.push(*a.newest);
    let heads = maximal_heads(db, pid, &cand);
    build::set_ref(db, pid, build::MAIN, &heads)?;
    Ok((heads, why))
}

/// [`accept`] 的坐标，见那里的注释：**同一场会话**的四样东西。
struct Accept<'a> {
    pid: ProjectId,
    our_heads: &'a [Hash],
    their_heads: &'a [Hash],
    known: &'a BTreeSet<Hash>,
    newest: &'a Hash,
    allow_mass_delete: bool,
    /// 这一场会话**真的收到**的对象（验收走差集要用它，见 [`verify_closure_delta`]）
    got: &'a Got,
}

/// 这一场会话收到过哪些 tree/blob。**为什么非记不可**：发送端现在只送差集（补记廿三），
/// 于是"新 commit 引用的对象"分成两类——这次推来的、以及"与亲代那一字相同所以没推"的。
/// 收端要分辨这两类只能靠自己记得收过什么。
#[derive(Default)]
struct Got {
    trees: BTreeSet<Hash>,
    blobs: BTreeSet<Hash>,
}

/// 一棵树的闭包能不能取回来 —— **差集版**（§15.22；发送端只送差集之后，这里也必须只验差集，
/// 否则一次 4 MiB 的增量推送要在收端读两万遍清单，而门面每次读调用折一遍全库（§9.6.1））。
///
/// 每个条目只有三种合法去向：
/// 1. **这一场会话收到的**（在 `got` 里）⇒ 逐块查盘（`cas.contains`），少一个块就拒；
/// 2. **与亲代同一位置逐字节相同的**（同 kind 同 hash，且亲代那一棵在 `got` 之外从没被这次推来）
///    ⇒ 继承，不必再读：不变量"head 只在闭包校验通过之后才前移"保证了它当年验过；
/// 3. **两者都不是** ⇒ 悬空引用 ⇒ 拒整个 commit（这就是原来那条 `MissingObject`）。
///
/// 返回"按继承算过去"的对象数，报账用（`why` 里那句"从未变的上代继承"）。
///
/// **这条改动把担保范围挪了一段，必须说清**：原来每次推送都会把整仓库重验一遍，所以它顺带也是个
/// 磁盘腐烂探测器；现在不是了——**它保证的是"这次推的东西自洽、引用得到"，不再保证"全仓库没有 rot"**。
/// rot 的归属改给读路径与自检：`restore`/`checkout`/`verify` 逐块按 hash 自证（§9.4.7 规矩 3，
/// 损坏永远报错不静默），`gc --sweep` 会报出"账上有、盘上没"的缺口（§9.3 的 orphan/missing 两格），
/// FR-21 的 `doctor` 在启动时开库自证。这个取舍写在 §15.22，不藏。
///
/// `parent = None`（首个提交、或亲代不在本端）⇒ 第三类之外的东西全都必须在 `got` 里 ⇒ 自动退回全量校验。
fn verify_closure_delta(
    db: &dyn KvEngine,
    pid: ProjectId,
    cas: &Cas,
    root: &Hash,
    parent: Option<&Hash>,
    got: &Got,
    counted: &mut usize,
) -> Result<usize> {
    let mut inherited = 0usize;
    // 栈里是 (这次要验的树, 亲代同一位置的树)
    let mut stack: Vec<(Hash, Option<Hash>)> = vec![(*root, parent.copied())];
    while let Some((h, ph)) = stack.pop() {
        let entries = object::read_tree(db, pid, &h)?;
        let older = match &ph {
            Some(x) if *x != h => Some(object::read_tree(db, pid, x)?),
            // 亲代这一层与新的一模一样（同 hash）⇒ 整棵继承，不必展开。
            // 走到这里通常说明这棵树根本没在 `got` 里，上面那条分支已经先挡住了。
            _ => None,
        };
        for e in entries {
            let old = older
                .as_ref()
                .and_then(|list| list.iter().find(|o| o.name == e.name));
            match e.kind {
                Kind::Dir => {
                    if got.trees.contains(&e.hash) {
                        // 这次推来的层：继续展开，与亲代同名项配对以便下一层也能判继承
                        stack.push((e.hash, old.map(|o| o.hash)));
                    } else if let Some(o) = old {
                        if o.kind == e.kind && o.hash == e.hash {
                            inherited += 1; // 整棵子树与上代逐字节相同 ⇒ 当年验过，不必再读
                        } else {
                            return Err(Error::MissingObject {
                                hash: keys::short(&e.hash),
                                kind: "tree（清单引用它，既没推来、也与上代不同）".to_string(),
                            });
                        }
                    } else {
                        return Err(Error::MissingObject {
                            hash: keys::short(&e.hash),
                            kind: "tree（清单引用它，但这一场会话没收到它）".to_string(),
                        });
                    }
                }
                Kind::File | Kind::Symlink => {
                    if got.blobs.contains(&e.hash) {
                        let blob = build::read_blob(db, pid, &e.hash).map_err(|_| {
                            Error::MissingObject {
                                hash: keys::short(&e.hash),
                                kind: "blob（对端推的清单里引用它，但没推它本身）".to_string(),
                            }
                        })?;
                        if blob.hash != e.hash {
                            return Err(Error::CorruptBlock {
                                hash: keys::short(&e.hash),
                                path: "(清单对不上自己的 hash)".into(),
                                why: "blob 的 B 行内容算出的 hash 与 tree 条目引用的不符".to_string(),
                            });
                        }
                        for b in blob.blocks.iter().filter(|b| !b.zero) {
                            if !cas.contains(&b.hash) {
                                return Err(Error::MissingObject {
                                    hash: keys::short(&b.hash),
                                    kind: "block（清单引用，盘上没有）".to_string(),
                                });
                            }
                        }
                        *counted += 1;
                    } else if let Some(o) = old {
                        if o.kind == e.kind && o.hash == e.hash {
                            inherited += 1;
                        } else {
                            return Err(Error::MissingObject {
                                hash: keys::short(&e.hash),
                                kind: "blob（清单引用它，既没推来、也与上代不同）".to_string(),
                            });
                        }
                    } else {
                        return Err(Error::MissingObject {
                            hash: keys::short(&e.hash),
                            kind: "blob（清单引用它，但这一场会话没收到它）".to_string(),
                        });
                    }
                }
            }
        }
    }
    Ok(inherited)
}

fn count_entries(db: &dyn KvEngine, pid: ProjectId, root: &Hash) -> Result<u32> {
    let mut n = 0u32;
    let mut stack = vec![*root];
    while let Some(h) = stack.pop() {
        for e in object::read_tree(db, pid, &h)? {
            if e.kind == Kind::Dir {
                stack.push(e.hash);
            }
            n += 1;
        }
    }
    Ok(n)
}

// ── 集合与遍历的小工具（都是纯函数，单独可测） ─────────────────────────────

/// 从若干起点出发的可达 commit 集合。解不开亲代的 hash 只算它自己——那是对端自报的祖先，
/// 我们本来就没有它的对象。
pub fn reachable(db: &dyn KvEngine, pid: ProjectId, heads: &[Hash], extra: &[Hash]) -> BTreeSet<Hash> {
    // **这里故意把错误咽掉**：读不到亲代就当没有亲代 ⇒ 少算一份"对端已知"⇒ 最坏是多推几个对象，
    // 幂等、无害。GC 走同一份遍历但**不咽**（`history::reachable_strict`）：那边少标一个 commit
    // 就是把活数据判成垃圾删掉。两种容错的差别写在 `history` 那侧，别被"统一"掉。
    history::reachable_strict(db, pid, heads, extra).unwrap_or_default()
}

/// head 集合的**极大元**：谁能被**别的候选**走到，谁就不是头（它是那个头的祖先）。
/// 快进是它的特例（旧头被新头盖住 ⇒ 只剩一个头）；分叉时两个都留（§4.1 只增不减）。
///
/// 判据的方向是这个函数的全部难度，我第一次写反过（"我的可达集里有没有别人"），而且**写反了
/// 还自洽**：单头时两种写法都返回那一个头，只有两个候选时才露馅——所以用例里必须同时摆
/// 快进与分叉两种形状（`tests/net_session.rs` 的 `maximal_heads_…`）。
pub fn maximal_heads(db: &dyn KvEngine, pid: ProjectId, candidates: &[Hash]) -> Vec<Hash> {
    let mut uniq: Vec<Hash> = candidates.to_vec();
    uniq.sort();
    uniq.dedup();
    // 每个候选的可达集只算一次：头数是个位数（§4.1"只增不减"是有限集合，不是全历史）。
    // 而且**所有候选共用同一份亲代索引**（一次 `c|p` 前缀扫）：原来每个候选走一遍
    // `reachable`，而门面每次读调用折一遍全库（§9.6.1）⇒ 候选数 × 一次全库折。
    let index = object::parent_index(db, pid).unwrap_or_default();
    let reach: Vec<BTreeSet<Hash>> = uniq
        .iter()
        .map(|c| history::reachable_from(&index, std::slice::from_ref(c), &[]))
        .collect();
    uniq.iter()
        .enumerate()
        .filter(|(i, c)| {
            !reach
                .iter()
                .enumerate()
                .any(|(j, r)| *i != j && r.contains(*c))
        })
        .map(|(_, c)| *c)
        .collect()
}

/// 待推 commit 的**亲代在前**序（接收方读 commit 时不会撞 `MissingObject`）。
/// 一轮轮收"亲代都就位了"的那些；没有进展就把剩下的原样附上（有环或缺亲，让验收去报错，
/// 而不是在这里静默丢东西）。
pub fn topo_pending(
    db: &dyn KvEngine,
    pid: ProjectId,
    mine: &BTreeSet<Hash>,
    skip: &BTreeSet<Hash>,
) -> Vec<Hash> {
    let mut todo: Vec<Hash> = mine.difference(skip).cloned().collect();
    todo.sort();
    let mut done: BTreeSet<Hash> = skip.clone();
    let mut out = Vec::with_capacity(todo.len());
    // 亲代索引**一次装好**（同一场会话里这里与 `maximal_heads` 各折一遍全库，不再按提交数折）
    let index = object::parent_index(db, pid).unwrap_or_default();
    while !todo.is_empty() {
        let mut ready = Vec::new();
        let mut rest = Vec::new();
        for h in todo.iter().copied() {
            let parents = index.get(&h).cloned().unwrap_or_default();
            if parents.iter().all(|p| done.contains(p)) {
                ready.push(h);
            } else {
                rest.push(h);
            }
        }
        if ready.is_empty() {
            out.extend(rest);
            return out;
        }
        for h in &ready {
            done.insert(*h);
        }
        out.extend(ready);
        todo = rest;
    }
    out
}

/// 一趟 push 要运出去的东西。**四个累加器收成一个结构**不是为了好看：
/// `collect_changed` 原来要接 8 个参数（clippy 直接拒绝），而那四个本来就同生同死——
/// 它们是"这一场会话的差集"这一个概念的四格账。
#[derive(Default)]
struct Outbound {
    /// 要送的对端缺失的块（问过 `want/have` 的那些）
    need: Vec<Hash>,
    /// 每个 need 块的 size，用来算配额与 `PushBlocks` 的 `bytes`
    block_size: BTreeMap<Hash, u32>,
    blobs: BTreeMap<Hash, cas::Blob>,
    trees: BTreeMap<Hash, Vec<object::TreeEntry>>,
    commits: Vec<object::Commit>,
}

/// 只收集"**对端还没有**"的那部分对象。
///
/// `base` = 对端**已经有**的那一棵树（本端这个 commit 的亲代，且它落在对端自报的 head/祖先闭包里）。
/// 判据是"**同名的子项 kind 与 hash 都相同**"⇒ 那一整棵子树逐字节相同 ⇒ 对面早就有它，连一个 blob 都不
/// 必去读。`base = None`（首个提交、或分叉来的 commit 亲代不在对端那边）就退回"整棵树都送"。
///
/// **为什么安全**：跳过发送 = 假设对端真有。假设错了也不是静默的历史错乱——收端的
/// [`verify_closure`] 会把新 commit 的整棵树重新走一遍并逐块查盘，缺一个就整个 commit 拒掉。
/// 所以"对端自报的祖先"只被用来**省字节**，不当信任依据（那条限制在 §15.21 与 §9.5.2）。
///
/// **为什么必须是这一条而不是"清单也做一次 want/have"**：后者要加两种帧、且把一次往返压到每个对象上；
/// 而差集在我们自己手里就能算出来（两棵树本来就在本端库上），一次网络往返都不多。
fn collect_changed(
    db: &dyn KvEngine,
    pid: ProjectId,
    root: &Hash,
    base: Option<&Hash>,
    out: &mut Outbound,
) -> Result<()> {
    // 栈里每一项是"(要展开的新树, 对端已有的同一位置的老树)"。深度优先，同层只读两棵树。
    let mut stack: Vec<(Hash, Option<Hash>)> = Vec::new();
    let level = |h: Hash, b: Option<Hash>, stack: &mut Vec<(Hash, Option<Hash>)>| {
        if b == Some(h) {
            return; // 这一整棵一字未动
        }
        stack.push((h, b));
    };
    level(*root, base.copied(), &mut stack);
    while let Some((h, b)) = stack.pop() {
        if out.trees.contains_key(&h) {
            continue;
        }
        let entries = object::read_tree(db, pid, &h)?;
        let older = match &b {
            Some(bh) if *bh != h => Some(object::read_tree(db, pid, bh)?),
            _ => None,
        };
        for e in entries.iter() {
            let old = older
                .as_ref()
                .and_then(|list| list.iter().find(|o| o.name == e.name));
            // 同名、同类型、同 hash ⇒ 那一整块对面早就有，不必再问
            if let Some(o) = old
                && o.kind == e.kind
                && o.hash == e.hash
            {
                continue;
            }
            match e.kind {
                Kind::Dir => level(e.hash, old.map(|o| o.hash), &mut stack),
                Kind::File | Kind::Symlink => {
                    if out.blobs.contains_key(&e.hash) {
                        continue;
                    }
                    let blob = build::read_blob(db, pid, &e.hash)?;
                    for blk in blob.blocks.iter().filter(|x| !x.zero) {
                        // 块在多个文件里出现只问一次：`block_size` 的键集合就是"要问过对端
                        // 哪些块"的判据，Vacant 那一支才是第一次见到它。
                        if let std::collections::btree_map::Entry::Vacant(v) =
                            out.block_size.entry(blk.hash)
                        {
                            v.insert(blk.size);
                            out.need.push(blk.hash);
                        }
                    }
                    out.blobs.insert(e.hash, blob);
                }
            }
        }
        // 走到这一层说明它变了（没变的早在上面跳掉了）⇒ 要送。
        out.trees.insert(h, entries);
    }
    Ok(())
}

/// 按估算尺寸把对象切成多帧。估算而不是精确编码长度：省掉"每加一条就重编一次"的 O(n²)，
/// 而真超限时编码器自己会兜（返回 `BadInput`），所以估小了也只是多切一帧，不会发出解不出的帧。
fn pack_objects(pending: &Outbound) -> Vec<Proto> {
    let budget = (frame::MAX_FRAME / 2) as usize;
    let mut out = Vec::new();
    let mut cur_blobs: Vec<cas::Blob> = Vec::new();
    let mut cur_trees: Vec<Vec<object::TreeEntry>> = Vec::new();
    let mut used = 0usize;
    let mut flush = |cur_blobs: &mut Vec<cas::Blob>, cur_trees: &mut Vec<Vec<object::TreeEntry>>| {
        if !cur_blobs.is_empty() || !cur_trees.is_empty() {
            out.push(Proto::PushObjects {
                blobs: std::mem::take(cur_blobs),
                trees: std::mem::take(cur_trees),
                commits: Vec::new(),
            });
        }
    };
    for b in pending.blobs.values() {
        let cost = 40 + b.blocks.len() * 41;
        if used + cost > budget {
            flush(&mut cur_blobs, &mut cur_trees);
            used = 0;
        }
        used += cost;
        cur_blobs.push(b.clone());
    }
    for t in pending.trees.values() {
        let cost: usize = t.iter().map(|e| e.name.len() + 45).sum();
        if used + cost > budget {
            flush(&mut cur_blobs, &mut cur_trees);
            used = 0;
        }
        used += cost;
        cur_trees.push(t.clone());
    }
    flush(&mut cur_blobs, &mut cur_trees);
    if !pending.commits.is_empty() {
        out.push(Proto::PushObjects {
            blobs: Vec::new(),
            trees: Vec::new(),
            commits: pending.commits.clone(),
        });
    }
    out
}

fn chunk_hashes(hashes: &[Hash]) -> Vec<Vec<Hash>> {
    let per = ((frame::MAX_FRAME / 4) as usize / 32).max(1);
    if hashes.is_empty() {
        return vec![Vec::new()];
    }
    hashes.chunks(per).map(|c| c.to_vec()).collect()
}

fn unexpected(msg: &Proto, wanted: &str) -> Error {
    Error::BadInput(format!("协议走样：这个位置该收到 {wanted}，实际 {msg:?}"))
}

fn note_abort(io: &mut impl Io, msg: &str) -> Result<()> {
    frame::encode(io, &Proto::Abort { msg: msg.to_string() })
}

fn mass_delete_of(e: &Error) -> Option<(u32, u32, u64)> {
    match e {
        Error::MassDelete {
            deleted,
            known,
            deleted_bytes,
        } => Some((*deleted, *known, *deleted_bytes)),
        _ => None,
    }
}
