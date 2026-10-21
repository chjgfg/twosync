//! 会话账（`S` 前缀，§9.2）：每场同步留一行读数，给 `status` 看"上次到底发生了什么"。
//!
//! # 为什么要把这些数存下来而不是当场算
//!
//! FR-16 要的 `status` 字段里有三样是**历史事实**，不是当前状态：上次会话的耗时、传输字节、
//! 以及**当时看到的对端 head 集合**。前两样只有发起方知道（收端只知道自己落了多少盘），
//! 第三样在会话结束后就取不到了（要取就得再拨一次号）。所以发起方在结束时把它写进库：
//! `status` 于是能回答"我离上次跟对端说话过了几个 commit"（ahead/behind），
//! 而这条正是 §7 表第一行"我该拉"的判据来源。
//!
//! # 一条如实的边界
//!
//! 这份记录是**本端视角**的自报，不是对端的真值：对面在我们上次说话之后又提交了几轮，
//! 这里看不见。所以 `status` 打印的是"上次会话时对端的 head"，措辞不许缩成"对端的 head"——
//! 那会让人以为它是对端现状（§15 的限制同源：不签名、不实时）。
//!
//! # 键与增长
//!
//! 键 = `S|p|<会话 id 大端 8 字节>`，**只追加不改**（会话是不可事件）。会话 id 用开始时刻的
//! 纳秒数（同进程内两次会话不会撞；真撞上了后写者覆盖前者，代价只是少一条历史读数，
//! 不是丢数据）。行数 = 会话数 ⇒ 一台机器一天几百轮 × 一年 ≈ 十几万行，
//! 回收交给 `gc`（`S` 行不在保留集里就是垃圾）；这里不做单独的轮转。

use serde::{Deserialize, Serialize};
use storage_engines::KvEngine;

use crate::error::{Error, Result};
use crate::store::keys::{self, Hash, ProjectId};
use crate::store::{history, object};

/// 一场会话站在**本端**视角的读数。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SessionRecord {
    /// 会话开始的时刻（纳秒）。它同时就是键里的会话 id。
    pub ts: i64,
    /// 项目名。**键里已经按 `pid` 分区**，这一份是给打印用的（`status` 要说清"上次那场是哪条线"）。
    pub project: String,
    /// 对面的名字（`push`/`pull` 是 ssh 目标，`agent` 侧是本端的对端名不可知 ⇒ 空串）。
    pub peer: String,
    /// 这一场本端是发送方（push）、接收方（agent 被推）、还是拉取方（pull，本端收）。
    pub dir: Dir,
    /// 上线/落盘的块数与字节数。**方向不同含义不同**，见 `Dir`：发送方是"上线"，接收方是"落盘"。
    pub blocks: usize,
    pub bytes: u64,
    /// 被协商跳过（对面已有）的块数。只有发送方向才有意义。
    pub skipped: usize,
    /// 墙钟毫秒。注明：这是**当时那台机器**的数，跨盘型不可比（§17 的盘价那条）。
    pub ms: u64,
    /// 会话结束时**本端**的 head 集合。
    pub our_heads: Vec<Hash>,
    /// 会话结束时**对面**的 head 集合（发起方是对面的回帧里拿的，接收方是开场帧里拿的）。
    pub their_heads: Vec<Hash>,
    /// 对面最后验收的那个 commit（`None` = 被拒或没动 head）。
    pub accepted: Option<Hash>,
}

/// 本端在这场会话里站的方向。收成枚举而不是布尔：`blocks`/`bytes` 的含义跟着它变，
/// 一个 `bool` 字段名（`is_sender`）在读打印语句时仍然会被人读反。
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Dir {
    /// 本端发起并**发送**（`push`）。
    #[default]
    Push,
    /// 本端发起并**接收**（`pull`）。
    Pull,
    /// 本端是被拨的那一侧，这一场在收（`agent` 面对 `BeginSession`）。
    ServeIn,
    /// 本端是被拨的那一侧，这一场在发（`agent` 面对 `PullRequest`）。
    ServeOut,
}

impl Dir {
    /// 本端是发送方吗？`blocks`/`bytes` 是不是"上线"而不是"落盘"。
    pub fn is_send(&self) -> bool {
        matches!(self, Dir::Push | Dir::ServeOut)
    }

    pub fn label(&self) -> &'static str {
        match self {
            Dir::Push => "push（本端发送）",
            Dir::Pull => "pull（本端接收）",
            Dir::ServeIn => "agent 收",
            Dir::ServeOut => "agent 发（对面 pull）",
        }
    }
}

/// 写一行会话账。**覆盖已有键**是允许的（同纳秒撞车），代价见模块头。
pub fn record(db: &dyn KvEngine, pid: ProjectId, r: &SessionRecord) -> Result<()> {
    let key = keys::session_key(pid, r.ts as u64);
    let body =
        postcard::to_allocvec(r).map_err(|e| Error::Codec(format!("会话账编码失败：{e}")))?;
    let tx = db.begin();
    if !tx.set(&key, keys::wrap(body)) {
        let why = Error::WriteConflict(String::from_utf8_lossy(&key).into_owned());
        tx.commit();
        return Err(why);
    }
    tx.commit();
    Ok(())
}

/// 取**最近**一条会话账（键序就是时间序，所以反向数第一条）。
pub fn last(db: &dyn KvEngine, pid: ProjectId) -> Result<Option<SessionRecord>> {
    let mut rows = recent(db, pid, 1)?;
    Ok(rows.pop())
}

/// 最近 `n` 条（时间倒序）。`status` 只用 1 条，将来 `--history` 会要更多。
pub fn recent(db: &dyn KvEngine, pid: ProjectId, n: usize) -> Result<Vec<SessionRecord>> {
    let mut prefix = vec![keys::TAG_SESSION];
    prefix.extend_from_slice(&pid.to_be_bytes());
    let tx = db.begin();
    let rows = tx.prefix_scan(&prefix);
    tx.commit();
    let mut out: Vec<SessionRecord> = Vec::new();
    for r in rows {
        // 键尾就是 8 字节会话 id。形状不对/解不出的行**跳过而不是报错**：
        // `status` 是一条"看一眼"的命令，它不该被一行坏账卡死（坏了由 `gc`/`doctor` 那边管）。
        let Some(tail) = r.key.get(prefix.len()..) else {
            continue;
        };
        if tail.len() != 8 {
            continue;
        }
        let Some(v) = r.value.as_ref() else { continue };
        let Ok(raw) = keys::strip(v) else { continue };
        let Ok(rec) = postcard::from_bytes::<SessionRecord>(raw) else {
            continue;
        };
        out.push(rec);
    }
    // 键序 = 时间升序 ⇒ 倒着数才是"最近 n 条"。
    out.sort_by_key(|r| r.ts);
    out.reverse();
    out.truncate(n);
    Ok(out)
}

/// **本站自己收进来、并落到自己 head 上的那些 commit**（`merge --take-remote` 用来认"哪一个是对方来的"）。
///
/// 只看本端在**收**的那两向（`Pull`、`agent 收`）：`Push` 那一条记的 `accepted` 是**对面**验收的
/// commit，把它算进来就会得出"我推出去的东西是对面来的"这种反话。
///
/// 这是一条**会随历史变旧**的判据（§14 Q18 记着：要把来源变成永久事实，得往 `H` 的值里写）。
/// 所以现在这样用是安全的：它只用来在"两个 head 里认出一个对方来的"，
/// **认不出或认出多个时调用方必须拒绝**（`merge` 就是这么做死的），不许挑一个。
pub fn accepted_here(db: &dyn KvEngine, pid: ProjectId) -> Result<std::collections::BTreeSet<Hash>> {
    let mut out = std::collections::BTreeSet::new();
    for r in recent(db, pid, usize::MAX)? {
        if r.dir.is_send() {
            continue;
        }
        if let Some(h) = r.accepted {
            out.insert(h);
        }
    }
    Ok(out)
}

/// `ahead`/`behind`：本端 head 与"上次会话时看到的对端 head"之间各差几个 commit。
///
/// 用的是**可达闭包的差**，不是 head 个数的差：head 集合里一个 commit 可能带着整条链，
/// 而 §7 那行"我该拉"要的是"对面有多少版本我还没有"。两边都从各自的 head 出发走亲代闭包，
/// 一次前缀扫（`parent_index`）就够 —— 这里绝不逐条点读（那是 §11.4 那张表的教训）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AheadBehind {
    /// 本端有、对面（上次那一面）没有。
    pub ours: usize,
    /// 对面有、本端没有 ⇒ 非零就是"该拉"。
    pub theirs: usize,
}

impl AheadBehind {
    /// 给 `status` 的一行话。两个数都零就是"与上次看到的一致"。
    pub fn line(&self) -> String {
        if self.ours == 0 && self.theirs == 0 {
            "与上次会话时看到的对端一致（没有新的要拉，也没有新的要推）".to_string()
        } else {
            format!(
                "本端多 {} 个 commit、对端多 {} 个（{} ⇒ 该拉；{} ⇒ 该推）",
                self.ours,
                self.theirs,
                if self.theirs > 0 { "有" } else { "无" },
                if self.ours > 0 { "有" } else { "无" },
            )
        }
    }
}

/// 算 `ahead`/`behind`：**一次** `c|p` 前缀扫描好亲代索引，两边各走一遍闭包（`reachable_from`）。
///
/// 为什么不用 head 个数相减：一个 head 背后可能带着十来个 commit，而 §7 表第一行"我该拉"
/// 要的是"对面有多少版本我还没有"。
pub fn ahead_behind(
    db: &dyn KvEngine,
    pid: ProjectId,
    our_heads: &[Hash],
    their_heads: &[Hash],
) -> Result<AheadBehind> {
    let index = object::parent_index(db, pid)?;
    let ours = history::reachable_from(&index, our_heads, &[]);
    let theirs = history::reachable_from(&index, their_heads, &[]);
    Ok(AheadBehind {
        ours: ours.difference(&theirs).count(),
        theirs: theirs.difference(&ours).count(),
    })
}
