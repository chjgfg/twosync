//! M2 批次：常驻。`daemon`（本文件已实现）· watch 窄扫 / pull / 断点续传的其余部分（还没做）。
//!
//! # 一轮长什么样
//!
//! ```text
//! 触发（事件静默 debounce 秒 / 或兜底间隔到）
//!   → scan（全量）→ commit（无变更 ⇒ 不产生提交、不动 head）
//!   → 配对了就先 pull（收对面的历史）再 push（§16.3"拨号方向对称"）
//! ```
//!
//! 顺序执行、单线程。三个阶段共用 **`Cli` 上那一个 IO 桶**（`cli.budget` ⇒ `Cas::with_bucket`）：
//! `commit` 落块、`push` 上行、（将来）`gc` 删除扣的是同一份预算，累计字节从同一个起点算，
//! 所以 `--max-mbps` 管的是**整轮平均**，不是"每个阶段各 8 MiB/s"。FR-15 / §16.3 要的就是这个数。
//!
//! # 默认 900 s 是量出来的，不是拍的
//!
//! 12 GiB 站点上"什么都不改"的一轮 `scan` 实测 **68.2 s / 20 215 次点读**（§11.4b），
//! 按 NFR-2 的"稳态增量会话 CPU 峰值 ≤ 10 % 单核"倒推：68 ÷ 0.10 ≈ 680 s ⇒ 取 **900 s** 留余量。
//! 所以"事件来了立刻跑一轮"与"最坏 15 分钟兜底一轮"是同一件事的两面：**窄扫没做之前，
//! 每一轮都是全量**，事件只能提前它，不能缩小它。
//!
//! # 为什么这一版不做"只扫变更目录"的窄扫（一个会咬人的语义问题，不是偷懒）
//!
//! 活性判定是 `W` 行的 `scanned_at >= Z.epoch`（`build::LiveRows`）。一轮只重扫事件所在的几个目录
//! ⇒ 其它目录的行全都留在旧 epoch ⇒ `commit` 会把它们看成"盘上已删" ⇒ **一次假删除**，
//! 而这正是 §8 列的第一杀手、也是 FR-10 熔断存在的原因。
//! 三条出路，都要动判定形状，所以单独排：① 目录级 epoch（`Z|p|<dir>`，删除判定改成逐目录）；
//! ② 窄扫轮**不产生删除**（只 add/modify，删除留给下一次全量轮确认）；③ 让 `commit` 能拿到
//! "本轮扫过哪些目录"的名单。没定下来之前，宁可慢，不可假删。
//!
//! # 跑法
//!
//! ```text
//! twosync --root <站点根> --project <p> daemon --no-push --rounds 2      # 先看两轮，不联网
//! twosync --root <站点根> --project <p> daemon --peer <host> --max-mbps 8 # 常驻
//! ```
//! 用例在 `tests/daemon.rs`（两站点 + `--rounds`，验"有变更才提交、没变更不产生空提交"与退避曲线）。

use notify::Watcher;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use crate::cli::cmd::{DaemonArgs, Import, PullArgs, PushArgs};
use crate::cli::{m0, m1, Cli};
use crate::error::{Error, Result};
use crate::store::build;

/// 连续失败之后下一次重试要等多久：**1 → 2 → 4 → 8 …封顶 `cap`**。
///
/// 纯函数，因为这条曲线是唯一决定"网络抖成什么样之后还打得通"的东西，写死在循环里就没人能验它。
/// `failures = 0` ⇒ 0（成功过就别白等）。
pub fn backoff_secs(failures: u32, cap: u64) -> u64 {
    if failures == 0 {
        return 0;
    }
    // `min(failures-1, 20)`：`1 << 20` 已经远超任何合理的 cap，留着只是防止 u32 移位溢出成 0。
    (1u64 << (failures - 1).min(20)).min(cap.max(1))
}

/// 一轮的触发原因。报账必须说清是哪一种，否则"这一轮为什么花了 68 s"没人答得上来。
#[derive(Debug, PartialEq, Eq)]
enum Trigger {
    /// 第一轮：立刻跑，不等事件（也让 `--rounds 1` 在测试里是确定行为）
    First,
    /// 事件静默 `debounce` 秒之后的那一下
    Quiet(PathBuf),
    /// 兜底间隔到了
    Periodic,
}

/// 常驻循环。旋钮全在 [`DaemonArgs`] 里（八项，位置参数会被传反，见那个结构的注释）。
pub fn daemon(cli: &Cli, args: &DaemonArgs) -> Result<()> {
    let DaemonArgs {
        interval,
        debounce,
        rounds,
        max_mbps,
        no_push,
        peer,
        agent_bin,
        allow_mass_delete,
    } = *args;
    let (name, _) = match &cli.project {
        Some(p) => (p.clone(), cli.root.clone()),
        None => m0::resolve_project(cli)?,
    };
    if !no_push && peer.is_none() {
        return Err(Error::BadInput(
            "daemon 要推送就得给 --peer <host>（或明确 --no-push 只本地提交）".to_string(),
        ));
    }
    let reg = crate::cli::registry::Registry::load(&cli.root)?;
    let work = match reg.projects.get(&name) {
        Some(p) => cli.root.join(&p.path),
        None => {
            return Err(Error::BadInput(format!(
                "项目 {name:?} 不在注册表里：先 `project add {name}`"
            )));
        }
    };
    let interval = Duration::from_secs(interval.max(1));
    let debounce = Duration::from_secs(debounce.max(1));

    println!(
        "daemon      = project {name} 根 {} · 兜底 {} s · 静默 {} s · 限速 {}",
        work.display(),
        interval.as_secs(),
        debounce.as_secs(),
        if max_mbps > 0.0 {
            format!("{max_mbps:.1} MiB/s")
        } else {
            "未设".to_string()
        }
    );
    println!(
        "注意        = 每一轮都是**全量 scan**（12 GiB 站点实测 68.2 s/轮 ⇒ 兜底间隔按 10 % 单核倒推）。\n\
                  窄扫还没做：它要先解决删除判定（`scanned_at >= epoch` 会把没重扫的目录看成已删，§8 第一杀手）。"
    );

    // 事件订阅。建不了 watcher（权限、平台、网络盘）不是错误：**退化成纯周期**，但必须说出来。
    // `watcher` 必须绑定在这个作用域：它一 drop 事件就断，而循环层看不出任何区别。
    let (events, _watcher) = match watch_tree(&work) {
        Ok(pair) => pair,
        Err(e) => {
            println!("watch       = 起不来（{e}）⇒ 本轮按纯周期跑，事件不会提前触发");
            (None, None)
        }
    };

    let mut failures = 0u32;
    let mut round = 0u64;
    let mut next_forced = Instant::now();
    loop {
        round += 1;
        let trigger = if round == 1 {
            Trigger::First
        } else {
            wait(events.as_ref(), next_forced, debounce)
        };
        next_forced = Instant::now() + interval;
        let started = Instant::now();
        println!("第 {round} 轮   = 触发 = {trigger:?}");

        // ── scan + commit ──
        // head 前后各读一次：`commit` 自己会打印，但"有没有真的产生提交"这件事循环层要知道
        // （一次 `H` 前缀扫，与 68 s 的一轮比可以忽略）。
        m0::scan(cli, false, 2000)?;
        let db = cli.store()?;
        let pid = crate::store::keys::project_id(&name);
        let before = build::all_ref_heads(&*db, pid, None)?;
        drop(db);
        m0::commit(
            cli,
            Some(&format!("daemon 第 {round} 轮（{trigger:?}）")),
            allow_mass_delete,
            Import {
                fast: false,
                max_mbps,
            },
        )?;
        let db = cli.store()?;
        let after = build::all_ref_heads(&*db, pid, None)?;
        drop(db);
        let committed = after != before;

        // ── pull（先收，再送）──
        // 顺序是有理由的：先把对面的历史收进来，本端的 head 集合就完整了，随后那次 push
        // 才是"互相补齐"而不是"各推各的"。§16.3 那句"拨号方向对称"在常驻循环里就是这两步。
        //
        // **pull 失败不进退避曲线**：对端还是个空站点（没有 head）时它必然拒开场，
        // 那是**正常状态**不是网络故障；把它算进 `failures` 会让第一天的循环一路退避到 15 分钟。
        let mut pulled = false;
        if !no_push {
            match m1::pull(
                cli,
                &PullArgs {
                    peer: peer.unwrap_or(""),
                    agent_bin,
                    remote_root: None,
                    project: Some(&name),
                    // 常驻循环不给体积上限：它的预算是**速率**（`--max-mbps`），
                    // 而"这次愿意被灌多少"是一次性人工操作的旋钮。
                    max_recv_mib: 0.0,
                    max_mbps,
                    allow_mass_delete,
                },
            ) {
                Ok(()) => pulled = true,
                Err(e) => println!("pull 失败   = {e}\n             不算失败：这一轮照旧尝试 push"),
            }
        }

        // ── push ──
        let mut pushed = false;
        if !no_push {
            // 没提交也推：对端可能比我们新（它自己提交过）时，push 是"让对端也看到我们的 head"，
            // 没新东西时它自己会报 `blocks_sent = 0`，代价是一次会话。
            match m1::push(
                cli,
                &PushArgs {
                    peer: peer.unwrap_or(""),
                    agent_bin,
                    // 不给 `--remote-root`：常驻循环拿的是 `project pair` 记下的那一份，
                    // 让人在命令行上手改远端目录不是 daemon 该有权做的事（§9.5.3）。
                    remote_root: None,
                    project: Some(&name),
                    max_send_mib: 0.0,
                    max_mbps,
                    allow_mass_delete,
                },
            ) {
                Ok(()) => {
                    pushed = true;
                    failures = 0;
                }
                Err(e) => {
                    failures += 1;
                    let backoff = backoff_secs(failures, interval.as_secs());
                    println!(
                        "push 失败   = {e}\n             连续第 {failures} 次 ⇒ 这一轮之后退避 {backoff} s\
                         （退避不跨过兜底点，只顺延）"
                    );
                    // 睡完退避再进下一轮的等待：兜底时钟不重置，所以最坏只晚这么多。
                    let w = Duration::from_secs(backoff);
                    if w < interval {
                        std::thread::sleep(w);
                    }
                }
            }
        }
        println!(
            "第 {round} 轮   = {} · 拉={} · 推={} · 墙钟 {:.1} s",
            if committed {
                "有提交"
            } else {
                "无变更，未提交"
            },
            if no_push {
                "跳过".to_string()
            } else if pulled {
                "收到".to_string()
            } else {
                "未收到".to_string()
            },
            if no_push {
                "未推送（--no-push）".to_string()
            } else if pushed {
                "已推送".to_string()
            } else {
                "推送失败".to_string()
            },
            started.elapsed().as_secs_f64()
        );
        // 桶账：**自进程起**的累计扣桶，跨阶段一个把手。这一行就是"共享"的证据：
        // 一轮里 commit 落的块与 push 上行的块都算在同一个数里（各开各的桶时这里会只看到一半）。
        // 如实的边界：`scan` 写的是引擎行、不落块，不扣这个桶（限速只管真搬字节的那三处）。
        if let Some(bytes) = cli.io_bucket.written() {
            println!(
                "扣桶        = 累计 {:.2} MiB（{:.1} MiB/s 的把手，进程起、跨 commit/push 共享）",
                bytes as f64 / 1048576.0,
                cli.io_bucket.limit().unwrap_or(0) as f64 / 1048576.0
            );
        }
        if rounds != 0 && round >= rounds {
            println!("跑满 {round} 轮，退出（--rounds）");
            return Ok(());
        }
    }
}

/// 起一个递归 watcher。**必须把 watcher 养住**：它一 drop，事件就没了，而调用方看不出任何区别
/// ——这种"静默失效"比报错坏，所以返回类型把它逼着调用方拿在手里。
fn watch_tree(
    work: &Path,
) -> Result<(
    Option<Receiver<notify::Event>>,
    Option<notify::RecommendedWatcher>,
)> {
    let (tx, rx) = mpsc::channel::<notify::Event>();
    let mut watcher = notify::recommended_watcher(
        move |res: std::result::Result<notify::Event, notify::Error>| {
            if let Ok(ev) = res {
                // 事件流是尽力而为：投不进去（这一轮还没结束）就丢掉，反正兜底间隔保证最终一致。
                // **不能因为事件积压而卡住循环** —— 那正是 §11.4b 那类"量具自己变成负载"的形状。
                let _ = tx.send(ev);
            }
        },
    )
    .map_err(|e| Error::BadInput(format!("watch 起不来：{e}")))?;
    watcher
        .watch(work, notify::RecursiveMode::Recursive)
        .map_err(|e| Error::BadInput(format!("watch {work:?} 失败：{e}")))?;
    Ok((Some(rx), Some(watcher)))
}

/// 等到下一次触发的原因。**两种结束方式**：事件静默 `debounce` 秒，或者兜底时刻到。
///
/// 没有 watcher（`events = None`）时**必须睡到兜底时刻**：早退会让循环层把"纯周期"跑成
/// 死循环全量扫描——那一轮是 68 s，不是 0，退化路径比主路径更凶就没人发现了。
fn wait(
    events: Option<&Receiver<notify::Event>>,
    deadline: Instant,
    debounce: Duration,
) -> Trigger {
    let Some(rx) = events else {
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        return Trigger::Periodic;
    };
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Trigger::Periodic;
        }
        // 一直等到"下一个事件"或"兜底时刻"，二者先到哪个算哪个。
        let budget = (deadline - now).min(debounce);
        match rx.recv_timeout(budget) {
            Ok(ev) => {
                // 收到事件 ⇒ 开始计静默期：连续 `debounce` 秒没有新事件才算稳定。
                let path = ev.paths.first().cloned().unwrap_or_default();
                while rx.recv_timeout(debounce).is_ok() {}
                return Trigger::Quiet(path);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // 这一段窗口里没事件：兜底时刻到了就是周期触发，否则回到循环继续等。
            }
            // watcher 内部线程没了（断连）⇒ 这一轮起再不会有事件。不睡到兜底就会空转成全量扫死循环。
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                return Trigger::Periodic;
            }
        }
    }
}
