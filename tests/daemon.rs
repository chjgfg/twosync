//! M2 的常驻循环：`daemon` 的一轮到底做了什么，以及"一个进程一份 IO 预算"到底是不是真的。
//!
//! 四条用例各钉一件事（§11.4b 的形状，也是 §8"宁可慢不可假删"的那条取舍）：
//! 1. **有变更才提交**：连着跑两轮、中间什么都不改 ⇒ 第二轮必须**不产生空提交**（head 一个字节都没动）；
//! 2. 退避曲线：连续失败要等 1 → 2 → 4 …并**封顶**，而且失败时永远不等于 0（0 就是对着网络打锤）；
//! 3. 共享把手：同一个 `Bucket` 被配置两次不许把累计字节与起点重置掉，`0` 不许把已有的限速解除；
//! 4. 一轮的落块确实扣在**那一个**桶上（这一条是"总预算不叠加"能被核对的证据，不是口头承诺）。
//!
//! 跑法：`cargo test --test daemon`

use std::fs;
use std::path::{Path, PathBuf};

use storage_engines::EngineKind;
use twosync::cli::cmd::DaemonArgs;
use twosync::cli::{m2, Cli};
use twosync::store::{build, cas::Bucket, keys};
use twosync::utils::open_store;

struct Site {
    tmp: tempfile::TempDir,
}

impl Site {
    fn new(files: &[(&str, &[u8])]) -> Site {
        let tmp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(tmp.path().join(".twosync")).expect("mkdir .twosync");
        for (rel, body) in files {
            let p = tmp
                .path()
                .join("p")
                .join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
            fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            fs::write(p, body).expect("write");
        }
        let mut reg = twosync::cli::registry::Registry::default();
        reg.add("p", "p").expect("registry add");
        reg.save(tmp.path()).expect("registry save");
        Site { tmp }
    }

    fn root(&self) -> PathBuf {
        self.tmp.path().to_path_buf()
    }

    fn work(&self) -> PathBuf {
        self.root().join("p")
    }

    /// `cmd` 给 `Init`：daemon 只看 `--root`/`--project`，而 `Cli` 要求那个字段存在。
    /// 桶从外面递进来，用例才能在跑完之后读同一份账。
    fn cli(&self, bucket: Bucket) -> Cli {
        Cli {
            root: self.root(),
            project: Some("p".to_string()),
            engine: Some(EngineKind::LsmTree),
            io_bucket: bucket,
            store_slot: Default::default(),
            cmd: twosync::cli::Cmd::Init,
        }
    }

    fn write(&self, rel: &str, body: &[u8]) {
        let p = self
            .work()
            .join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(p, body).expect("write");
    }

    fn heads(&self) -> Vec<keys::Hash> {
        let db = open_store(&self.root(), EngineKind::LsmTree).expect("open");
        build::ref_heads(&*db, keys::project_id("p"), build::MAIN).expect("ref_heads")
    }

    /// 从 head 往下走亲代闭包 ⇒ "这个站点一共几个 commit"。
    fn commits(&self) -> usize {
        let db = open_store(&self.root(), EngineKind::LsmTree).expect("open");
        let pid = keys::project_id("p");
        let heads = build::ref_heads(&*db, pid, build::MAIN).expect("ref_heads");
        twosync::store::history::reachable_strict(&*db, pid, &heads, &[])
            .expect("reachable")
            .len()
    }

    fn parents_of(&self, h: &keys::Hash) -> Vec<keys::Hash> {
        let db = open_store(&self.root(), EngineKind::LsmTree).expect("open");
        twosync::store::object::parents_of(&*db, keys::project_id("p"), h).expect("parents_of")
    }
}

/// 只跑一轮、不联网：其余旋钮取与命令行默认一致的值，用例里不藏第二次定义。
fn one_round(max_mbps: f64) -> DaemonArgs<'static> {
    DaemonArgs {
        interval: 900,
        debounce: 1,
        rounds: 1,
        max_mbps,
        no_push: true,
        peer: None,
        agent_bin: "twosync",
        allow_mass_delete: false,
    }
}

#[test]
fn a_round_commits_only_when_something_changed() {
    let site = Site::new(&[("a.txt", b"one")]);

    // 第一轮：站点还没有历史 ⇒ 必须产出一个 commit。
    m2::daemon(&site.cli(Bucket::default()), &one_round(0.0)).expect("round 1");
    let h1 = match site.heads().as_slice() {
        [only] => *only,
        other => panic!("第一轮之后应当恰好一个 head，拿到 {} 个", other.len()),
    };
    assert_eq!(site.commits(), 1, "第一轮应当只有那一个 commit");

    // 第二轮：**什么都没改**。这一轮照样跑完整 scan（68 s 的那半边），但不许产生提交。
    m2::daemon(&site.cli(Bucket::default()), &one_round(0.0)).expect("round 2");
    assert_eq!(
        site.heads(),
        vec![h1],
        "无变更的一轮不许移 head（空提交会把历史刷成一串同内容的 commit）"
    );
    assert_eq!(site.commits(), 1, "无变更的一轮不许多出 commit");

    // 第三轮：改一个字节 ⇒ 必须新提交，且挂在上一轮那个 head 后面。
    site.write("a.txt", b"two");
    m2::daemon(&site.cli(Bucket::default()), &one_round(0.0)).expect("round 3");
    let h2 = match site.heads().as_slice() {
        [only] => *only,
        other => panic!("第三轮之后应当恰好一个 head，拿到 {} 个", other.len()),
    };
    assert_ne!(h2, h1, "有变更的一轮必须移 head");
    assert_eq!(site.commits(), 2, "第三轮应当只多出那一个 commit");
    assert_eq!(
        site.parents_of(&h2),
        vec![h1],
        "daemon 的提交要接在上一轮后面（成链，不是并列出一个新 head）"
    );
}

#[test]
fn backoff_doubles_and_is_capped() {
    // 成功过 ⇒ 不白等。
    assert_eq!(m2::backoff_secs(0, 900), 0, "没有失败就不许退避");
    // 曲线：1 → 2 → 4 → 8 → 16。
    let curve: Vec<u64> = (1..=5).map(|f| m2::backoff_secs(f, 900)).collect();
    assert_eq!(curve, vec![1, 2, 4, 8, 16], "退避必须是倍增的");
    // 封顶：再多失败也不会指数爆掉（爆掉等于"下一轮在 2 的 30 次方秒之后"）。
    assert_eq!(m2::backoff_secs(40, 900), 900, "退避要压在兜底间隔上");
    // 而失败时**永远不为 0**：0 就是对着对端打锤，比不重试更坏。
    for f in 1..=50 {
        assert!(m2::backoff_secs(f, 0) > 0, "连续第 {f} 次失败退避成了 0 秒");
    }
}

#[test]
fn the_shared_budget_keeps_its_account_when_configured_again() {
    // FR-15"总预算不叠加"的机制就在这三行里：桶住在 `Cli`，各阶段只是**配置**它。
    // 如果第二次 configure 把 `Pacer` 换成新的，累计字节归零、起点重算 ⇒ 一整轮的平均速率没人管了。
    let rate = 1u64 << 30; // 1 GiB/s：扣桶是真的扣，但睡的那点时间可以忽略（用例不掐墙钟）
    let b = Bucket::default();
    assert_eq!(b.limit(), None, "没配置过就是不限速");

    b.configure(rate);
    assert_eq!(b.limit(), Some(rate));
    b.charge(1000);
    assert_eq!(b.written(), Some(1000));

    // 第二个阶段带着**同一个值**再配置一次：账必须留着。
    b.configure(rate);
    assert_eq!(
        b.written(),
        Some(1000),
        "重复配置把累计账重置了（共享桶就白共享了）"
    );
    b.charge(2000);
    assert_eq!(b.written(), Some(3000), "累计必须跨阶段");

    // `0` = "这条命令没说话"，不许把已有的预算解除。
    b.configure(0);
    assert_eq!(
        b.limit(),
        Some(rate),
        "一个不带 --max-mbps 的阶段把整轮的限速关掉了"
    );
    assert_eq!(b.written(), Some(3000));

    // 给了**不同**的值 ⇒ 换成新预算（不取小者；调用方打的就是它自己那一条的值）。
    b.configure(rate / 2);
    assert_eq!(b.limit(), Some(rate / 2));
}

#[test]
fn a_rounds_block_writes_charge_the_processes_bucket() {
    // 1 MiB 的新文件、默认块大小 4 MiB ⇒ 这一轮正好落一个块。跑完之后读同一个把手的账。
    let body: Vec<u8> = vec![0x5c; 1 << 20];
    let site = Site::new(&[("big.bin", &body)]);
    let cli = site.cli(Bucket::default());
    // 1000 MiB/s ⇒ 1 MiB 只该睡约 1 ms：速率是给"能核对账"用的，不是让用例等盘。
    m2::daemon(&cli, &one_round(1000.0)).expect("限速的一轮");
    let charged = cli
        .io_bucket
        .written()
        .expect("给了 --max-mbps 就必须有桶账");
    assert!(
        (1 << 20..(1 << 20) + 4096).contains(&charged),
        "落块扣桶应当正好是那一个 1 MiB 块，实际 {charged}"
    );
}

#[test]
fn daemon_refuses_to_push_without_a_peer() {
    let site = Site::new(&[("a.txt", b"one")]);
    let args = DaemonArgs {
        no_push: false,
        peer: None,
        ..one_round(0.0)
    };
    let err = m2::daemon(&site.cli(Bucket::default()), &args).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("--peer") && msg.contains("--no-push"),
        "报错要说清两条出路，实际：{msg}"
    );
    assert!(site.heads().is_empty(), "参数不对的一轮不许先把站点写脏");
}

/// 兜底间隔与静默期都得至少 1 秒：0 会立刻变成"下一轮的等待时间是零"⇒ 全量 scan 死循环。
#[test]
fn zero_intervals_are_clamped_to_one_second() {
    fn clamp_check(root: &Path, interval: u64, debounce: u64) {
        // 这里只跑第一轮（rounds = 1），所以钳制的效果是"这一轮正常跑完就退出"，
        // 而不是"卡在 wait() 里等一个已经过去的兜底时刻"。
        let site_cli = Cli {
            root: root.to_path_buf(),
            project: Some("p".to_string()),
            engine: Some(EngineKind::LsmTree),
            io_bucket: Bucket::default(),
            store_slot: Default::default(),
            cmd: twosync::cli::Cmd::Init,
        };
        m2::daemon(
            &site_cli,
            &DaemonArgs {
                interval,
                debounce,
                ..one_round(0.0)
            },
        )
        .expect("interval/debounce 给 0 也要能跑完一轮");
    }
    let site = Site::new(&[("a.txt", b"one")]);
    clamp_check(&site.root(), 0, 0);
    assert_eq!(site.commits(), 1, "第一轮照样要出提交");
}
