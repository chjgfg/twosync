//! M1 批次：联网单向 + 回收。`push` / `agent` / `peer list|forget` / `gc`（本文件已实现），
//! 还缺的三格如实列在下面。
//!
//! # 传输为什么是"一根子进程的管子"
//!
//! §9.5 定的就是这条：不开新端口、复用系统 `sshd`。所以 `push` 做的全部事情是起一条
//! `ssh <peer> "<远端 twosync> agent --root <远端站点根>"`，把它的 stdout/stdin 拼成
//! [`session::Io`] 交给会话层——**协议不关心管子那头是 ssh 还是别的进程**，
//! 测试里塞一对内存管道跑的是同一份代码（`tests/net_session.rs`）。
//! 也因此不引入 `russh`/异步运行时：单核 6 G 的机器上少一个常驻 runtime 就是少一份预算，
//! 而 SSH 本来就在那台机器上跑着。
//!
//! # `agent` 的输出纪律
//!
//! **stdout 只能有帧**，给人看的一律走 stderr。多往 stdout 写一行，对端就把那一行当帧解，
//! 报出来是"协议走样"——看起来像对面在攻击，其实是我们自己污染了通道。
//!
//! # 现在还缺的三件事（如实标，别当 M1 已完成）
//!
//! - **真 SSH 那条链路还没跑过**：本机只有 ssh 客户端、没有 sshd，所以会话代码验过的是
//!   "内存管道 + 真子进程 stdio"两种管子（`tests/net_session.rs`）。开对端 sshd 要授权，
//!   这一格留在最后。
//! - **块级配额淘汰（FR-9 的"超出配额按最老且不可达优先"）没做**：`gc --sweep` 只清**不可达**的，
//!   保留集本身超配额时它只报账不丢历史（那是 M4 的 `reset`/`prune --expire` 的决策权）。
//!   所以 `push` 只保证"对端缺什么才传什么"，不保证对端磁盘装得下。
//! - **§14 Q14**：门面 `KvEngine` 没有分片版 `vacuum_bounded` ⇒ `gc` 的引擎那一半仍是"全库一刀"，
//!   在单核机械盘上就是个 IO 突发（实测 94.8 % 单核 / 40 s）。要真消除突发得动引擎侧，
//!   这里不 downcast 凑（理由写在 [`gc`] 的文档里）。
//!
//! # `gc` 为什么排在这三条里
//!
//! 它不是网络活儿，是**记账活儿**：10 GiB 基线上"0 变更的空转提交 45.2 s → 246.8 s"
//! （§11.1 复验表）就是"没人调用过 vacuum"这一条的账单。§9.3 的 mark-sweep 是另一半：
//! 不可达对象与归零块的回收拆成"先落 `D` 表、后限额删文件"两趟，细节与能力边界写在
//! [`store::gc`](crate::store::gc) 的模块头。

use std::format;
use std::io::{self, Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use crate::cli::Cli;
use crate::cli::cmd::PeerCmd;
use crate::cli::cmd::PullArgs;
use crate::cli::cmd::PushArgs;
use crate::cli::m0;
use crate::error::{Error, Result};
use crate::net::frame::caps;
use crate::net::session::{self, PushOpts};
use crate::store::build;
use crate::store::cas::Cas;
use crate::store::gc;
use crate::store::history;
use crate::store::keys;
use crate::store::sessions::{self, Dir, SessionRecord};
use crate::store::worktree;
use crate::utils::{self, objects_dir, site_id};

/// 写一行会话账（FR-16 的"上次会话耗时/传输字节/对端 head"）。
///
/// **写失败只警告，不改命令的成败**：同步已经发生了，让一次已完成的 push 因为记账失败而退出码
/// 非零，会让 `daemon` 走退避曲线、甚至再推一遍——那是把"账本没写上"升级成"数据出事"。
/// 磁盘真满了由下一次 `status`/`doctor` 的磁盘账暴露（FR-14 说的第一杀手就是它）。
fn note_session(db: &dyn storage_engines::KvEngine, pid: keys::ProjectId, r: &SessionRecord) {
    if let Err(e) = sessions::record(db, pid, r) {
        println!("会话账      = 没写上（{e}）⇒ 这一场的数只在屏幕上，`status` 读不到");
    }
}

/// 一根管子：读一头、写一头。会话层只认 `Read + Write` 这个形状，所以子进程的 stdio
/// 与进程自己的 stdio（`agent`）共用同一份会话代码。
///
/// `child` 只有在发起端才有——它的退出码是"对端 agent 有没有非零退出"的唯一信号。
pub struct Duplex {
    r: Box<dyn Read>,
    w: Box<dyn Write>,
    child: Option<Child>,
}

impl Read for Duplex {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.r.read(buf)
    }
}

impl Write for Duplex {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.w.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}

impl Duplex {
    /// 把一个"stdin/stdout 都是管道"的子进程拼成一根管子。
    ///
    /// 单独成一个构造函数不是为了好看：集成测试要用**同一个**构造去起 `twosync agent`，
    /// 因为真 OS 管道有缓冲上限（内存管道没有），"8 MiB 的帧会不会把写端堵死"只有走这条路
    /// 才暴露得出来。测试与生产共用一份，才不会测的是一根永远不会堵的管子。
    pub fn from_child(mut child: Child) -> Result<Duplex> {
        let out = child
            .stdout
            .take()
            .ok_or_else(|| Error::BadInput("子进程没有 stdout：piped() 没开".to_string()))?;
        let inp = child
            .stdin
            .take()
            .ok_or_else(|| Error::BadInput("子进程没有 stdin：piped() 没开".to_string()))?;
        Ok(Duplex {
            r: Box::new(out),
            w: Box::new(inp),
            child: Some(child),
        })
    }

    /// 关掉写端并等子进程退出。`push` 用完必须调：不关 stdin，远端 `agent` 会一直等在管子上。
    /// 公开的原因与 [`Duplex::from_child`] 同一条：测试要走完同一条收尾，不能只测半截。
    pub fn finish(mut self) -> Result<()> {
        drop(self.w);
        match self.child.as_mut() {
            Some(c) => {
                let status = c.wait()?;
                if !status.success() {
                    return Err(Error::BadInput(format!(
                        "对端进程退出码 {status}：会话帧已经报完，但它非零退出 ⇒ \
                         真实原因在**远端**的 stderr 里（agent 的诊断都写那一侧）。两种非零退出要分得开：\
                         项目级拒绝（没注册/没配对）走这一支，commit 级拒绝（FR-10 熔断）是正常收尾、退出码 0"
                    )));
                }
                Ok(())
            }
            None => Ok(()),
        }
    }
}

/// `push` 的一次会话：起 ssh、握手、协商、传块与清单、等对端 verdict。
pub fn push(cli: &Cli, a: &PushArgs) -> Result<()> {
    // 一次解构，后面全部按原名用：把结构体拆成局部变量，比到处写 `a.xxx` 少一层噪音，
    // 也不会出现"参数表和函数体两套命名"（`remote_root_opt` 那种）。
    let PushArgs {
        peer,
        agent_bin,
        remote_root,
        project,
        max_send_mib,
        max_mbps,
        allow_mass_delete,
    } = *a;
    // 会话耗时从"起 ssh"开始算（FR-16 那一格要的是"上次会话花了多久"，拨号时间也是它的代价）。
    let started = Instant::now();
    let (name, _) = match project {
        Some(p) => (p.to_string(), cli.root.clone()),
        None => m0::resolve_project(cli)?,
    };
    let db = cli.store()?;
    let pid = keys::project_id(&name);
    // `--max-mbps` 挂在这个进程的桶上（`cli.budget`）：`session::push` 每发完一个块段就扣一次字节
    // （FR-15 的同一个桶；`commit` 写盘与 `gc` 删除走的是同一条 `Cas::pace` 路径）。
    let cas = Cas::new(&objects_dir(&cli.root), cli.block_size()?)?
        .with_bucket(&cli.budget((max_mbps * 1048576.0) as u64));
    if let Some(bps) = cas.rate_limit_bps() {
        println!(
            "上行限速    = {:.1} MiB/s（FR-15 的桶，与同进程的 commit/gc **共用同一个把手**；扣的是真发出去的块字节）",
            bps as f64 / 1048576.0
        );
    }
    let heads = build::ref_heads(&*db, pid, build::MAIN)?;
    if heads.is_empty() {
        return Err(Error::BadInput(format!(
            "项目 {name:?} 还没有任何 head：先 scan + commit 再推"
        )));
    }

    let remote_root = paired_root(cli, &name, remote_root)?;
    // 远端命令整体交给对端的 shell 解析，所以路径自己带引号。
    let remote_cmd = format!("{agent_bin} --root \"{remote_root}\" agent");
    let child = Command::new("ssh")
        .arg(peer)
        .arg(&remote_cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| {
            Error::BadInput(format!(
                "起不了 `ssh {peer} {remote_cmd}`：{e}\n  \
                 对端要能免密登录，且那边装了 twosync（或换个能用的 ssh 客户端）"
            ))
        })?;
    let mut io = Duplex::from_child(child)?;
    // 本端身份：`.twosync/site-id`，第一次用到时生成（§9.5.1）。
    let site = site_id(&cli.root)?;

    let opts = PushOpts {
        project: &name,
        peer_id: &site,
        peer_name: peer.as_bytes(),
        caps: caps::FIXED_CHUNK | caps::SPARSE | caps::XATTR_MODE,
        max_block_bytes: if max_send_mib > 0.0 {
            (max_send_mib * 1048576.0) as u64
        } else {
            u64::MAX
        },
        allow_mass_delete,
    };
    let rep = match session::push(&mut io, &*db, &cas, &opts) {
        Ok(rep) => rep,
        Err(e) => {
            // **先收尾再写报错**：关写端 → 对端读到尾 → 等它退出。反过来（先 `try_wait` 再收尾）
            // 两件事都会坏：`try_wait` 在"管子已到尾但进程还没被回收"的那一刻给 `None`，报错里就
            // 只剩结果没有原因；而对端还在等输入时直接 `wait` 会把自己卡死。
            let why = match io.finish() {
                Ok(()) => e.to_string(),
                Err(done) => format!(
                    "{e}\n  对端进程那一侧的读数：{done}\n  \
                     没装 twosync、免密登录失败、sshd 根本没起，都会落到这一支——\
                     真实原因在 ssh 自己打的几行里（它写的是本端 stderr）"
                ),
            };
            return Err(Error::BadInput(why));
        }
    };
    if rep.first_contact {
        println!(
            "TOFU      = 首次连接 {peer}，它的站点身份已进 P 表。以后**身份一变就拒**；\
             对端确实重装过就 `peer forget {peer}` 重新确认"
        );
    }

    println!("project     = {name}");
    println!(
        "peer        = {peer}  (经 ssh 起的 agent；TOFU：本端认的是它的**站点身份**，\
         host key 那一层仍归 ssh)"
    );
    println!(
        "commits     = {}，清单 blobs {} 份 / tree {} 份",
        rep.commits, rep.blobs_sent, rep.trees_sent
    );
    println!(
        "blocks      = 上线 {} 个 / {:.1} MiB；跳过 {} 个（对端已有）",
        rep.blocks_sent,
        rep.bytes_sent as f64 / 1048576.0,
        rep.blocks_skipped
    );
    println!(
        "head(对端)  = {}",
        rep.accepted
            .iter()
            .map(keys::short)
            .collect::<Vec<_>>()
            .join(", ")
    );
    note_session(
        &*db,
        pid,
        &SessionRecord {
            ts: worktree::now_ns(),
            project: name.clone(),
            peer: peer.to_string(),
            dir: Dir::Push,
            blocks: rep.blocks_sent,
            bytes: rep.bytes_sent,
            skipped: rep.blocks_skipped,
            ms: started.elapsed().as_millis() as u64,
            our_heads: build::ref_heads(&*db, pid, build::MAIN)?,
            their_heads: rep.their_heads.clone(),
            accepted: rep.accepted.first().copied(),
        },
    );
    io.finish()
}

/// `pull`：拨到对端，让它把它那边的历史**推给我们**（同一套帧，本端站在收的那一侧）。
///
/// 与 `push` 的差别只有三处，其余全共用（起 ssh 的引号规矩、TOFU 的顺序、配对闸门、管子收尾）：
/// 1. 会话开成 `PullRequest`（本端的 head 集合 + 自报祖先 + **本端愿意收的配额**）；
/// 2. 配额换人声明：`--max-recv-mib` 是收货方的预算，由对面（发送方）照它自我了断；
/// 3. 验收在这台机器上做，所以 **FR-10 熔断看的是本端的旗标**——`pull` 回来的那一版要是删了
///    半个项目，拒的是我们，不是对面。
pub fn pull(cli: &Cli, a: &PullArgs) -> Result<()> {
    let PullArgs {
        peer,
        agent_bin,
        remote_root,
        project,
        max_recv_mib,
        max_mbps,
        allow_mass_delete,
    } = *a;
    let started = Instant::now();
    let (name, _) = match project {
        Some(p) => (p.to_string(), cli.root.clone()),
        None => m0::resolve_project(cli)?,
    };
    let db = cli.store()?;
    let pid = keys::project_id(&name);
    // 收进来的块要落 CAS ⇒ 限速挂在这个进程的桶上（与 commit/push/gc 同一个把手，FR-15）。
    let cas = Cas::new(&objects_dir(&cli.root), cli.block_size()?)?
        .with_bucket(&cli.budget((max_mbps * 1048576.0) as u64));
    if let Some(bps) = cas.rate_limit_bps() {
        println!(
            "落盘限速    = {:.1} MiB/s（FR-15 的桶，与同进程的 commit/push/gc 共用一个把手）",
            bps as f64 / 1048576.0
        );
    }

    let remote_root = paired_root(cli, &name, remote_root)?;
    let remote_cmd = format!("{agent_bin} --root \"{remote_root}\" agent");
    let child = Command::new("ssh")
        .arg(peer)
        .arg(&remote_cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| {
            Error::BadInput(format!(
                "起不了 `ssh {peer} {remote_cmd}`：{e}\n  \
                 对端要能免密登录，且那边装了 twosync"
            ))
        })?;
    let mut io = Duplex::from_child(child)?;
    let site = site_id(&cli.root)?;
    let opts = PushOpts {
        project: &name,
        peer_id: &site,
        peer_name: peer.as_bytes(),
        caps: caps::FIXED_CHUNK | caps::SPARSE | caps::XATTR_MODE,
        max_block_bytes: if max_recv_mib > 0.0 {
            (max_recv_mib * 1048576.0) as u64
        } else {
            u64::MAX
        },
        allow_mass_delete,
    };
    let rep = match session::pull(&mut io, &*db, &cas, &opts) {
        Ok(rep) => rep,
        Err(e) => {
            // 先收尾再报错（与 `push` 同一条理由：不关写端，对面会一直等在管子上）。
            let why = match io.finish() {
                Ok(()) => e.to_string(),
                Err(done) => format!("{e}\n  对端进程那一侧的读数：{done}"),
            };
            return Err(Error::BadInput(why));
        }
    };
    if rep.first_contact {
        println!(
            "TOFU      = 首次连接 {peer}，它的站点身份已进 P 表。以后**身份一变就拒**；\
             对端确实重装过就 `peer forget {peer}` 重新确认"
        );
    }
    println!("project     = {name}");
    println!("peer        = {peer}  (经 ssh 起的 agent；这一场本端是**接收方**)");
    println!(
        "收          = 块 {} 个 / {:.1} MiB 落盘，清单 blobs {} 份 / tree {} 份",
        rep.blocks_written,
        rep.bytes_in as f64 / 1048576.0,
        rep.blobs_in,
        rep.trees_in
    );
    println!(
        "head(本端)  = {}",
        rep.heads_after
            .iter()
            .map(keys::short)
            .collect::<Vec<_>>()
            .join(", ")
    );
    // 验收是**本端**做的，所以拒绝的原因在这里就有，不必等对面报错回来。
    // 但会话必须仍然以非零退出收尾：对面（发送方）那边 `push_body` 会收到我们的 `RejectCommit`
    // 并以错误结束，我们若安静退场，两边看到的就是"成功了"和"失败了"两个故事。
    note_session(
        &*db,
        pid,
        &SessionRecord {
            ts: worktree::now_ns(),
            project: name.clone(),
            peer: peer.to_string(),
            dir: Dir::Pull,
            // 接收方向：`blocks`/`bytes` 是**落盘**的数；`skipped` 在这里没有意义（协商是对面算的）。
            blocks: rep.blocks_written,
            bytes: rep.bytes_in,
            skipped: 0,
            ms: started.elapsed().as_millis() as u64,
            our_heads: rep.heads_after.clone(),
            their_heads: rep.their_heads.clone(),
            // 本端就是验收的那一侧：拒了就没有新 head。
            accepted: if rep.accepted { rep.heads_after.first().copied() } else { None },
        },
    );
    let verdict = io.finish();
    if !rep.accepted {
        return Err(Error::BadInput(format!(
            "本端拒绝收这一版：{}\n  （FR-10 是按**本端**的账判的；确认过了再加 --allow-mass-delete）",
            rep.reason
        )));
    }
    verdict
}

/// 服务端：stdin 读帧、stdout 写帧。**给人用 `ssh` 调，不是给人敲的**。
///
/// 一条连接上连续服务多场会话 ⇒ FR-19 的"20 个 project 一轮只 1 次认证"。管子 EOF
/// 就是"对端挂了"，正常退出（不是错误，别报成会话失败）。
/// **唯一的例外**："块与清单都已收下、只差 `VerifyCommit`"那一读上的 EOF——那里断 = 库里留下
/// 一整套无人可达的对象，会话层把它报成 `BadInput`（不是 `Io(UnexpectedEof)`），于是走错误分支、
/// 非零退出，stderr 原话写着"`gc --sweep --apply` 会把它收掉"（§9.3、`net::session` 那段注释）。
pub fn agent(cli: &Cli, only: Option<&str>) -> Result<()> {
    let db = cli.store()?;
    let cas = Cas::new(&objects_dir(&cli.root), cli.block_size()?)?;
    // 白名单 = 本端注册表（`--project` 再收窄到那一个）。注册表一定要读：§9.5.3 的配对记录就在
    // 里面，只看"名字在不在"等于没查配对。
    let mut projects = crate::cli::registry::Registry::load(&cli.root)?.projects;
    if let Some(n) = only {
        projects = projects
            .into_iter()
            .filter(|(name, _)| name == n)
            .collect::<std::collections::BTreeMap<_, _>>();
    }
    let projects = projects;
    // 服务端在 Hello 里报的是**本站点的身份**（`.twosync/site-id`），不是环境变量：
    // 发起端的 TOFU 表要绑的是一个稳定的东西，而"谁调用我时传什么"是可以随便改的。
    let site = site_id(&cli.root)?;
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut io = Duplex {
        r: Box::new(stdin.lock()),
        w: Box::new(stdout.lock()),
        child: None,
    };

    let mut sessions = 0usize;
    loop {
        let mut resolve = |name: &str| -> std::result::Result<u32, String> {
            let Some(p) = projects.get(name) else {
                return Err("本端没注册这个项目：先 `project add`（§9.5.3 绝不自动建目录）".to_string());
            };
            // 注册了不等于愿意收：本端也得配过对（两边各一次 = "两边都确认"）。
            if p.peer_path.is_none() {
                return Err(format!(
                    "本端注册了但没配对：先 `project pair {name} <对端站点根>`"
                ));
            }
            Ok(keys::project_id(name))
        };
        match session::serve(&mut io, &*db, &cas, &site, !0, &mut resolve, false) {
            Ok(rep) => {
                sessions += 1;
                // 服务端也要记账（这一侧的账只有它自己知道：落盘块数、验收结果、当时看到的对面 head）。
                // `peer` 留空是**如实**：被拨的一侧在协议里拿不到"谁拨的"——对端身份可以照抄（§9.5.2
                // 不签名），所以这里不编一个名字出来，宁可空着让人看见"这一格没有"。
                let (dir, blocks, bytes, skipped) = match &rep.sent {
                    Some(s) => (Dir::ServeOut, s.blocks_sent, s.bytes_sent, s.blocks_skipped),
                    None => (Dir::ServeIn, rep.blocks_written, rep.bytes_in, 0usize),
                };
                note_session(
                    &*db,
                    keys::project_id(&rep.project),
                    &SessionRecord {
                        ts: worktree::now_ns(),
                        project: rep.project.clone(),
                        peer: String::new(),
                        dir,
                        blocks,
                        bytes,
                        skipped,
                        // 服务端没有"整场会话"的起点计时：`serve` 已经在里面读完了帧，
                        // 所以这里能给的只有**这一场结束时刻**的纳秒低位（不是耗时）。
                        // 不给假数：`status` 打印 `ms` 时会写明发起方才有耗时。
                        ms: 0,
                        our_heads: rep.heads_after.clone(),
                        their_heads: rep.their_heads.clone(),
                        accepted: if rep.accepted {
                            rep.heads_after.first().copied()
                        } else {
                            None
                        },
                    },
                );
                // 两种角色两种账：`sent` 有值 = 对面 `pull` 了我们，这一场本端是**发送方**。
                // 打印形状必须分开——"上线/跳过"与"落盘/入库"不是同一组数（§10.4 差分表就吃这个数）。
                match &rep.sent {
                    Some(sent) => eprintln!(
                        "agent #{sessions}: project={} 本端是**发送方**（对面 pull）：上线 {} 块 / {:.1} MiB，\
                         跳过 {} 个（对面已有），清单 blobs {} 份 / tree {} 份；对面 head = {}",
                        rep.project,
                        sent.blocks_sent,
                        sent.bytes_sent as f64 / 1048576.0,
                        sent.blocks_skipped,
                        sent.blobs_sent,
                        sent.trees_sent,
                        sent.accepted
                            .iter()
                            .map(keys::short)
                            .collect::<Vec<_>>()
                            .join(",")
                    ),
                    None => eprintln!(
                        "agent #{sessions}: project={} 收块 {} 个 / {:.1} MiB，清单 {} 份，tree {} 份 ⇒ {}；head = {}",
                        rep.project,
                        rep.blocks_written,
                        rep.bytes_in as f64 / 1048576.0,
                        rep.blobs_in,
                        rep.trees_in,
                        if rep.accepted { "接受" } else { "拒绝" },
                        rep.heads_after
                            .iter()
                            .map(keys::short)
                            .collect::<Vec<_>>()
                            .join(",")
                    ),
                }
                if !rep.accepted {
                    eprintln!("agent:   原因 = {}", rep.reason);
                }
            }
            Err(e) => {
                if matches!(&e, Error::Io(io) if io.kind() == io_kind_eof()) {
                    return Ok(());
                }
                return Err(e);
            }
        }
    }
}

/// `UnexpectedEof` 单独抽出来：管子读到尾与"对端把连接断了"在会话层是同一件事，
/// 但不能跟其它 IO 错误混在一起——那样一填就写反了。
fn io_kind_eof() -> std::io::ErrorKind {
    std::io::ErrorKind::UnexpectedEof
}


/// `gc`：**引擎版本回收**（全库一轮 `vacuum()`，前后各读一次 §9.6.2 的 `health()`），
/// 外加 `--sweep` 的 §9.3 mark-sweep（两趟：记账 → 限额删）。
///
/// 病根见模块头：`scan` 每轮都要给每一行盖新版本（活性判据的命根子，不能省），而回收这一步
/// 在 M0 的流水线里从来没被调用过。
///
/// # 分片回收为什么不是这里做的（一条真实的能力边界）
///
/// `storage_engines` 的**具体** MVCC 类型上有 `vacuum_bounded(after, budget_rows)`（游标 +
/// 每圈预算），但门面 `KvEngine` 只暴露 `vacuum()`（全库一轮）与
/// `set_vacuum_auto_slice()`（引擎自己的小圈：每攒够 N 行新版本裁一片，默认 16 Ki 行）。
/// 我们手里只有 `Box<dyn KvEngine>` ⇒ **这一版 `gc` 就是全库一刀**，不假装能分片。
/// 两个后果与出路：
/// - 想"不制造 IO 突发"就用 `--auto-slice` 把回收交给引擎的常流小圈（M2 的 agent 走这条），
///   而不是在这里 sleep 假装限速；
/// - 想让 `gc` 自己分片，需要门面把 `vacuum_bounded` 提上来 ⇒ 记进 §14 的引擎侧问题，
///   **不在这里 downcast 凑**：凑出来的代码要在四个具体类型上各写一遍，等于把引擎的
///   类型选择漏进 CLI 层。
pub fn gc(
    cli: &Cli,
    auto_slice: Option<u64>,
    sweep: bool,
    apply: bool,
    unlink_max_files: usize,
    max_mbps: f64,
    quota_mib: f64,
) -> Result<()> {
    let db = cli.store()?;
    let before = db.health();
    println!("engine      = {}", db.name());
    println!(
        "回收前      = rows {} / live {} / max_chain {} / rows_above_one {} / active_txns {}",
        before.total_rows, before.live_keys, before.max_chain, before.rows_above_one, before.active_txns
    );
    println!("自动回收切片 = {} 行（0 = 关闭，版本行只增不减）", db.vacuum_auto_slice());

    if let Some(rows) = auto_slice {
        db.set_vacuum_auto_slice(rows);
        println!(
            "改为        = {} 行（已生效，下一次提交起按这个切片自裁）",
            db.vacuum_auto_slice()
        );
    }

    let stats = db.vacuum()?;
    println!(
        "本轮        = 读 {} 行，裁掉 {} 个旧版本（水位 xmin {}）",
        stats.rows_scanned, stats.versions_removed, stats.xmin
    );
    if matches!(stats.blob_rewritten, Some(true)) {
        println!("blob 文件   = 已重写（外置 value 被压实过）");
    }
    if stats.skipped_active_txn {
        println!(
            "注意        = 有活跃事务 ⇒ **整轮跳过，一个版本都没裁**（`versions_removed = 0` \
             不等于「没垃圾」，是「没开工」，§9.6.2）"
        );
    }

    let after = db.health();
    println!(
        "回收后      = rows {} / live {} / max_chain {} / rows_above_one {} / active_txns {}",
        after.total_rows, after.live_keys, after.max_chain, after.rows_above_one, after.active_txns
    );
    println!(
        "净减        = {} 行（{} → {}）",
        before.total_rows.saturating_sub(after.total_rows),
        before.total_rows,
        after.total_rows
    );

    if sweep {
        // §9.3 的 mark-sweep。逐项目走：保留集是**按项目**的历史，而块的 refcnt 是**跨项目**的账，
        // 所以"哪个块能删"只由 refcnt 说话，不由某个项目的保留集说话（理由写在 `store::gc` 模块头）。
        println!(
            "回收        = mark-sweep {}；前提是本站点没有别的写者，与 commit/push 并发会误删",
            if apply { "**落改动**" } else { "只报账（dry-run）" }
        );
        let unlink_max = if unlink_max_files == 0 { usize::MAX } else { unlink_max_files };
        let cas = Cas::new(&objects_dir(&cli.root), cli.block_size()?)?
            .with_bucket(&cli.budget((max_mbps * 1048576.0) as u64));
        let cap = if unlink_max == usize::MAX {
            "不限个数".to_string()
        } else {
            format!("最多 {unlink_max} 个块")
        };
        println!(
            "删除闸门    = 单趟上限 {cap}，限速 {}；每删一个先从 FR-15 的桶里扣它自己的 \
             size（§9.3：删除与写入共用一个桶，两个各自的桶加起来就是双倍速率）",
            match cas.rate_limit_bps() {
                Some(bps) => format!("{:.1} MiB/s", bps as f64 / 1048576.0),
                None => "**未设**（`--max-mbps` 不给就是放任突发）".to_string(),
            }
        );
        let reg = crate::cli::registry::Registry::load(&cli.root)?;
        let mut names: Vec<&String> = reg.projects.keys().collect();
        names.sort();
        let mut all = gc::Plan::default();
        for name in names {
            let pid = keys::project_id(name);
            let p = gc::sweep(&*db, &cas, pid, !apply)?;
            // 归零已经记进 pending 表了；删文件是第二趟（限额、可续）。dry 模式只报"将要删哪些"。
            let ul = gc::unlink_pending(&*db, &cas, pid, unlink_max, !apply)?;
            let (pt, pb) = gc::pending_total(&*db, pid)?;
            all.commits_kept += p.commits_kept;
            all.commits_dropped += p.commits_dropped;
            all.trees_dropped += p.trees_dropped;
            all.blobs_dropped += p.blobs_dropped;
            all.blocks_pending += p.blocks_pending;
            all.bytes_pending += p.bytes_pending;
            all.pending_total += pt;
            all.pending_bytes += pb;
            all.blocks_unlinked += ul.files;
            all.bytes_freed += ul.bytes;
            all.blocks_missing += ul.missing;
            all.orphan_files += p.orphan_files;
            all.orphan_bytes += p.orphan_bytes;
            println!(
                "  {name:<12} 保留 {:>5} 事务；回收 事务 {:>3} / tree {:>4} / 清单 {:>5}；\
                 本次待删 {:>5} 块 / {}，{} {:>5} / 表上还欠 {:>5}",
                p.commits_kept,
                p.commits_dropped,
                p.trees_dropped,
                p.blobs_dropped,
                p.blocks_pending,
                size_h(p.bytes_pending),
                if apply { "实删" } else { "将删" },
                ul.files + ul.missing,
                pt,
            );
            if pb > 0 {
                println!(
                    "             pending 表合计 {}（{pt} 个块）没删完 ⇒ 再跑一次 `gc --sweep --apply`\
                     就续上；被 `--unlink-max-files` 截住的还有 {} 个",
                    size_h(pb),
                    ul.left
                );
            }
        }
        println!(
            "合计        = 回收 事务 {} / tree {} / 清单 {}；记进待删 {} 块 / {}，\
             本趟实删 {} 块 / {}",
            all.commits_dropped,
            all.trees_dropped,
            all.blobs_dropped,
            all.blocks_pending,
            size_h(all.bytes_pending),
            all.blocks_unlinked,
            size_h(all.bytes_freed)
        );
        if all.blocks_missing > 0 {
            println!(
                "账清文件没清 = {} 个 ⇒ 以前崩在一次删除的中间（§9.3 那个 pending-delete 形状的残留）",
                all.blocks_missing
            );
        }
        if all.orphan_files > 0 {
            println!(
                "孤儿文件    = {} 个 / {}：盘上有、`K` 账上没有 ⇒ 会话在「块落完、清单还没到」\
                 时被打断。这一版**只数不删**，因为删它要跟保留集一起判断（见 §9.3 的顺序）",
                all.orphan_files,
                size_h(all.orphan_bytes)
            );
        }

        // 配额账（§14 Q12 定案：配额管的是**首次导入之后**的增量 ⇒ 上限 = 工作树 × (1 + 20%)，
        // 首份全量不计入——12 GB 的内容不可能存进 2.4 GB，那条在 §11.1 实测是 100.1%）。
        //
        // 工作树按**注册表里逐个项目的目录**累加，不拿 `--root` 整棵树去量：站点根下面还挂着
        // `.twosync/`（CAS 自己！）与别的工具的目录，量它会得出"工作树 30 GB"这种 doubled 的数
        // （第一版就踩过：`dir_size(--root)` 把 CAS 算进工作树，于是配额比实际松了一倍）。
        let mut work = 0u64;
        for (name, proj) in &reg.projects {
            let d = utils::dir_size(&cli.root.join(&proj.path));
            work += d.bytes;
            println!(
                "  工作树 {name:<12} = {}（{} 个文件，读不到 {} 项）",
                size_h(d.bytes),
                d.files,
                d.unreadable
            );
        }
        let cas_bytes = utils::dir_size(&objects_dir(&cli.root)).bytes;
        let limit = if quota_mib > 0.0 {
            (quota_mib * 1048576.0) as u64
        } else {
            work + work / 5
        };
        println!(
            "配额        = CAS {} / 工作树 {} ⇒ 上限（{}）{}，{}",
            size_h(cas_bytes),
            size_h(work),
            if quota_mib > 0.0 {
                "--quota-mib"
            } else {
                "默认 工作树×(1+20%)"
            },
            size_h(limit),
            if cas_bytes > limit {
                format!("现超 {}", size_h(cas_bytes - limit))
            } else {
                format!("没超（还余 {}）", size_h(limit - cas_bytes))
            }
        );
        if cas_bytes > limit {
            // "剩下的都可达"这句话在有孤儿文件时是**假的**，所以逐条列原因，别一句话盖掉。
            let mut why = vec!["仍被保留 head 可达的字节 ⇒ gc 无权删（不变量 (d)）".to_string()];
            if all.orphan_files > 0 {
                why.push(format!(
                    "{} 个孤儿文件 / {} ⇒ 会话崩在「块落完、清单还没到」之间，这一版只数不删",
                    all.orphan_files,
                    size_h(all.orphan_bytes)
                ));
            }
            if all.pending_total > 0 {
                why.push(format!(
                    "pending 表上还欠 {} 个块 / {} ⇒ 再跑一次 `gc --sweep --apply` 就删完",
                    all.pending_total,
                    size_h(all.pending_bytes)
                ));
            }
            println!("还超着      = 回收之后仍然超配额，账在：{}", why.join("；"));
            println!("              要真腾地方得由人决定丢哪一版：那是 M4 的 `reset`/`prune --expire`");
        }
    }
    Ok(())
}

/// 报账的体积单位：10 GiB 站点上 MiB 是对的，但小站点用 `{:.1} MiB` 会把 4 KiB 印成 `0.0 MiB`
/// ——而 `gc` 的账必须在小站点上也读得出非零，否则"孤儿 1 个 / 0.0 MiB"这种句子等于没说。
fn size_h(b: u64) -> String {
    if b >= 1 << 20 {
        format!("{:.1} MiB", b as f64 / 1048576.0)
    } else if b >= 1 << 10 {
        format!("{:.1} KiB", b as f64 / 1024.0)
    } else {
        format!("{b} B")
    }
}

/// `peer list|forget`：§9.5.1 的 TOFU 表。表只有库里那一份（`P` 前缀），
/// 不落 `peers.json` —— 一份数据两处存就要同步，而同步是漂移的产地。
pub fn peer(cli: &Cli, cmd: PeerCmd) -> Result<()> {
    let db = cli.store()?;
    match cmd {
        PeerCmd::List => {
            let rows = session::read_peers(&*db)?;
            if rows.is_empty() {
                println!("(还没有已知对端：`push` 成功一次就会把对方记进 P 表)");
                return Ok(());
            }
            println!("对端          站点身份                                首次确认");
            for (name, p) in rows {
                println!(
                    "{:<14} {:<40} {}",
                    name,
                    session::hex(&p.site_id),
                    history::fmt_ts(p.first_seen)
                );
            }
        }
        PeerCmd::Forget { name } => {
            if session::forget_peer(&*db, name.as_bytes())? {
                println!("已忘掉 {name}：下一次 push 会重新确认它（首连告警会再打一次）");
            } else {
                println!("{name} 不在表里：没有什么可忘的（名字要跟当初 `push` 给 ssh 的那个一字不差）");
            }
        }
    }
    Ok(())
}

/// §9.5.3 的发起端那一半：**没配对的项目推不出去**。单独成一个函数不是为了好看，是因为
/// 这条判断必须能被测到——`push` 剩下的部分要 ssh，而这半截不需要。
///
/// `--remote-root` 只允许改"写到哪个目录"，**不能代替配对**：能不能写仍然由对端那一侧的
/// 配对记录决定（两边各确认一次）。覆盖与记录不一致时打印一句再照办——人会拿它连测试站点，
/// 静默拒绝比警告更烦人，但这句话必须留下，否则改了路径却以为配对生效。
pub fn paired_root(cli: &Cli, name: &str, over: Option<&str>) -> Result<String> {
    let reg = crate::cli::registry::Registry::load(&cli.root)?;
    let paired = match reg.projects.get(name) {
        Some(p) => p.peer_path.clone(),
        None => return Err(Error::Registry(format!("没有项目 {name}；先 project add"))),
    };
    let Some(paired) = paired else {
        return Err(Error::BadInput(format!(
            "项目 {name:?} 还没配对，推不出去：先 `project pair {name} <对端站点根>`（§9.5.3）。\n  \
             --remote-root 不能代替这一步——它只决定写到哪个目录，不决定对端愿不愿意收"
        )));
    };
    match over {
        Some(o) if o != paired.as_str() => {
            println!(
                "注意      = 远端站点根用了 --remote-root {o}，与配对记录 {paired} 不一致；\
                 只影响写到哪个目录，配对本身仍然算数"
            );
            Ok(o.to_string())
        }
        Some(o) => Ok(o.to_string()),
        None => Ok(paired),
    }
}
