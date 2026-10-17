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
//!
//! # 为什么接收侧要检查 `legal_name`
//!
//! 名字里带 `/`、`\`、`..` 的条目一旦进了树对象，要等 M3 检出时才发现就晚了——那时它已经是
//! 某个合法 commit 的一部分，撤回来要动 head。入库前先拒，代价是每条目一次字符串检查。

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{Read, Write};

use serde::{Deserialize, Serialize};
use storage_engines::KvEngine;

use crate::error::{Error, Result};
use crate::net::frame::{self, Proto, PROTO_VER};
use crate::store::build;
use crate::store::cas::{self, Cas};
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
        other => Err(unexpected(&other, "HeadsState")),
    }?;

    // ③ union 判定：要推的 = 从我方 head 可达、且不在"对端 head + 对端自报祖先"里的那些。
    let mine = reachable(db, pid, &my_heads, &[]);
    let theirs_reachable = reachable(db, pid, &their_heads, &their_ancestors);
    let to_send = topo_pending(db, pid, &mine, &theirs_reachable);
    rep.commits = to_send.len();

    // ④ 收集要运的对象与块。清单先攒在手里，分帧时按估算切（编码器还会兜一次真上限）。
    let mut trees: BTreeMap<Hash, Vec<object::TreeEntry>> = BTreeMap::new();
    let mut blobs: BTreeMap<Hash, cas::Blob> = BTreeMap::new();
    let mut block_size: BTreeMap<Hash, u32> = BTreeMap::new();
    let mut need: Vec<Hash> = Vec::new();
    let mut commits: Vec<object::Commit> = Vec::new();
    for h in &to_send {
        let c = object::read_commit(db, pid, h)?;
        collect_tree(
            db,
            pid,
            &c.tree,
            &mut trees,
            &mut blobs,
            &mut block_size,
            &mut need,
        )?;
        commits.push(c);
    }
    // ⑤ 块协商。配额按**协商之后**真正要上线的字节算（见下面 `send_bytes`），
    // 所以这里不先估一份"想要的总量"——那个数在对面有库存时永远是高的。
    for chunk in chunk_hashes(&need) {
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

    let to_write: Vec<Hash> = need.iter().cloned().filter(|h| missing.contains(h)).collect();
    rep.blocks_skipped = need.len() - to_write.len();
    let send_bytes: u64 = to_write
        .iter()
        .map(|h| *block_size.get(h).unwrap_or(&0) as u64)
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
            let size = *block_size.get(h).unwrap_or(&0);
            let body = cas.read_block(h, size)?;
            frame::write_segment(io, h, &body)?;
            rep.blocks_sent += 1;
            rep.bytes_sent += body.len() as u64;
        }
    }
    for frame_body in pack_objects(&blobs, &trees, &commits) {
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
    Ok(rep)
}

/// 服务端一场会话。`resolve` 把项目名换成本端 pid（没注册就 `None`）。
///
/// 一条连接上可以连着调用多次 ⇒ FR-19 的"20 个 project 一轮只 1 次认证"就是外层循环：
/// 每场会话只多一轮 `BeginSession`/`HeadsState`，不再重新拨号、不再重新输密码。
pub fn serve(
    io: &mut impl Io,
    db: &dyn KvEngine,
    cas: &Cas,
    my_peer_id: &[u8],
    my_caps: u32,
    resolve: &mut dyn FnMut(&str) -> Option<ProjectId>,
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

    let (project, pid_claimed, their_heads) = match frame::decode(io)? {
        Proto::BeginSession {
            project,
            pid,
            my_heads,
        } => (project, pid, my_heads),
        other => return Err(unexpected(&other, "BeginSession")),
    };
    frame::check_project_id(&project, pid_claimed)?;
    let Some(pid) = resolve(&project) else {
        let msg = format!("本端没有注册项目 {project:?}：先 `project add` 再同步");
        frame::encode(io, &Proto::Abort { msg: msg.clone() })?;
        return Err(Error::BadInput(msg));
    };
    rep.project = project.clone();

    let our_heads = build::ref_heads(db, pid, build::MAIN)?;
    let known = reachable(db, pid, &our_heads, &[]);
    let mut anc: Vec<Hash> = known.iter().cloned().collect();
    anc.sort();
    frame::encode(
        io,
        &Proto::HeadsState {
            heads: our_heads.clone(),
            ancestors: anc,
        },
    )?;

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
                    object::put_tree(db, pid, entries)?;
                    rep.trees_in += 1;
                }
                for b in &blobs {
                    build::adopt_blob(db, pid, b, "push")
                        .map_err(|er| note_abort(io, &er.to_string()).err().unwrap_or(er))?;
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
    let newest = match frame::decode(io)? {
        Proto::VerifyCommit { hash } => hash,
        other => return Err(unexpected(&other, "VerifyCommit")),
    };
    let verdict = accept(
        db,
        cas,
        &Accept {
            pid,
            our_heads: &our_heads,
            their_heads: &their_heads,
            known: &known,
            newest: &newest,
            allow_mass_delete,
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
                    from_heads: their_heads.clone(),
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
    Ok(rep)
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
        let mut closed = 0usize;
        verify_closure(db, pid, cas, &c.tree, &mut closed)?;
        if let Some(p) = c.parents.first() {
            let parent = object::read_commit(db, pid, p)?;
            let known = count_entries(db, pid, &parent.tree)?;
            let d = build::diff(db, pid, Some(&parent.tree), &c.tree)?;
            build::check_mass_delete(d.delete, d.deleted_bytes, known, allow)?;
            if d.delete > 0 {
                why = format!(
                    "接受 {}：{} 个对象闭合，删除 {} 条 / {:.1} MiB 在闸内",
                    keys::short(h),
                    closed,
                    d.delete,
                    d.deleted_bytes as f64 / 1048576.0
                );
            }
        } else {
            why = format!("接受 {}：{} 个对象闭合（首个提交）", keys::short(h), closed);
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
}

/// 一棵树的闭包能不能取回来：每层清单读得到、每个 File/Symlink 有 `B` 清单、
/// 清单里每个非零块**在盘上**。这三条少一条，历史就是假的（不变量 (b)）。
fn verify_closure(
    db: &dyn KvEngine,
    pid: ProjectId,
    cas: &Cas,
    root: &Hash,
    counted: &mut usize,
) -> Result<()> {
    let mut stack = vec![*root];
    while let Some(h) = stack.pop() {
        let entries = object::read_tree(db, pid, &h)?;
        for e in entries {
            match e.kind {
                Kind::Dir => stack.push(e.hash),
                Kind::File | Kind::Symlink => {
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
                }
            }
        }
    }
    Ok(())
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
    let mut seen = BTreeSet::new();
    let mut q: VecDeque<Hash> = VecDeque::new();
    for h in heads.iter().chain(extra.iter()) {
        if seen.insert(*h) {
            q.push_back(*h);
        }
    }
    while let Some(h) = q.pop_front() {
        for p in object::parents_of(db, pid, &h).unwrap_or_default() {
            if seen.insert(p) {
                q.push_back(p);
            }
        }
    }
    seen
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
    let reach: Vec<BTreeSet<Hash>> = uniq
        .iter()
        .map(|c| reachable(db, pid, std::slice::from_ref(c), &[]))
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
    while !todo.is_empty() {
        let mut ready = Vec::new();
        let mut rest = Vec::new();
        for h in todo.iter().copied() {
            let parents = object::parents_of(db, pid, &h).unwrap_or_default();
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

fn collect_tree(
    db: &dyn KvEngine,
    pid: ProjectId,
    root: &Hash,
    trees: &mut BTreeMap<Hash, Vec<object::TreeEntry>>,
    blobs: &mut BTreeMap<Hash, cas::Blob>,
    block_size: &mut BTreeMap<Hash, u32>,
    need: &mut Vec<Hash>,
) -> Result<()> {
    let mut stack = vec![*root];
    while let Some(h) = stack.pop() {
        if trees.contains_key(&h) {
            continue;
        }
        let entries = object::read_tree(db, pid, &h)?;
        for e in entries.iter() {
            match e.kind {
                Kind::Dir => stack.push(e.hash),
                Kind::File | Kind::Symlink => {
                    if blobs.contains_key(&e.hash) {
                        continue;
                    }
                    let blob = build::read_blob(db, pid, &e.hash)?;
                    for b in blob.blocks.iter().filter(|b| !b.zero) {
                        // 块在多个文件里出现只问一次：`block_size` 的键集合就是"要问过对端
                        // 哪些块"的判据，Vacant 那一支才是第一次见到它。
                        if let std::collections::btree_map::Entry::Vacant(v) =
                            block_size.entry(b.hash)
                        {
                            v.insert(b.size);
                            need.push(b.hash);
                        }
                    }
                    blobs.insert(e.hash, blob);
                }
            }
        }
        trees.insert(h, entries);
    }
    Ok(())
}

/// 按估算尺寸把对象切成多帧。估算而不是精确编码长度：省掉"每加一条就重编一次"的 O(n²)，
/// 而真超限时编码器自己会兜（返回 `BadInput`），所以估小了也只是多切一帧，不会发出解不出的帧。
fn pack_objects(
    blobs: &BTreeMap<Hash, cas::Blob>,
    trees: &BTreeMap<Hash, Vec<object::TreeEntry>>,
    commits: &[object::Commit],
) -> Vec<Proto> {
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
    for b in blobs.values() {
        let cost = 40 + b.blocks.len() * 41;
        if used + cost > budget {
            flush(&mut cur_blobs, &mut cur_trees);
            used = 0;
        }
        used += cost;
        cur_blobs.push(b.clone());
    }
    for t in trees.values() {
        let cost: usize = t.iter().map(|e| e.name.len() + 45).sum();
        if used + cost > budget {
            flush(&mut cur_blobs, &mut cur_trees);
            used = 0;
        }
        used += cost;
        cur_trees.push(t.clone());
    }
    flush(&mut cur_blobs, &mut cur_trees);
    if !commits.is_empty() {
        out.push(Proto::PushObjects {
            blobs: Vec::new(),
            trees: Vec::new(),
            commits: commits.to_vec(),
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
