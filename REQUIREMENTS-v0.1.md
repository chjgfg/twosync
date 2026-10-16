# twosync — 需求与范围说明

版本 v0.1（草案）｜2026-10-15｜状态：待确认 §14 的 6 个开放问题后进入 M0

---

## 0. 一句话

两个目录（本地 Windows 一份、服务器 Linux 一份）**持续双向收敛到同一状态**：任何一边的增、删、改都会传播到另一边；被覆盖或被删除的内容不会静默消失，可以按版本号取回。不依赖 GitHub/GitLab 这类代码托管，不依赖对象存储，两端各跑一个 Rust 单二进制。

## 1. 背景与真实场景

- 项目体积 2–10 GB，含大量不适合放进 git 主线的二进制资产/数据/产物。
- 服务器：**1 vCPU / 6 GB RAM**，且同机还跑着其他项目（长期驻留进程）。这是全部资源预算的硬约束来源。
- 本地：Windows 10/11（10.0.19045）。
- 两侧都会产生写入：本地是主要编辑端，服务器端也存在写入（这一点的确切性质见 §14 Q1，它决定 M3 的冲突要不要做到内容级合并）。
- 现有工具的定位：Syncthing 能解决问题但不是 Rust、不可控、常驻数百 MB；rsync 只能单向且删除语义危险。**本项目要的是"资源占用可解释、裁决规则可枚举、删除可回滚"的自研实现**，同时作为练手项目要能拿出可验证的正确性论证（见 §11）。

## 2. 非目标（明确不做，防止范围膨胀）

1. **不做 git 的替代品**。没有分支、没有历史图谱、不做三方文本合并（v1 内）。项目源码本体仍应由一个 bare 仓库走 SSH 管，twosync 只负责"文件树收敛"。
2. 不做 CRDT / 实时协作编辑。语义是"会话级收敛"，不是字符级合并。
3. **v1 只支持两个站点**（一个 local、一个 remote）。协议里预留 `site_id`（§9），但不实现 mesh、不实现仲裁者、不处理三端以上的因果顺序。
4. 不做 GUI、不做 Web 面板。CLI + 子命令 + 结构化日志。
5. 不做多用户 ACL / 细粒度权限系统。认证直接复用 SSH。
6. 不做字节码兼容的 rsync 协议实现（可借用其块算法思想，不做互操作）。
7. 不自动转换 CRLF/LF。**同步是字节级的**，不做任何"内容友好化"。
8. 不追求跨站点的强一致或线性一致（见 §4）。

## 3. 术语

| 术语 | 定义 |
|---|---|
| root | 一个站点的同步根目录，所有路径都是相对它的相对路径 |
| entry | root 下受管的一个路径对象（file/dir/symlink），带稳定 `entry_id` |
| rev | 一次写入产生的版本号，形式 `(site_id, lamport)` |
| base | 上一次成功收敛时双方共同认可的 entry 状态，冲突判定用的"共同祖先" |
| plan | 一次会话中算出的有序操作集（add/modify/delete/rename/...），未执行前可 dry-run 输出 |
| apply | 执行 plan 并提交；每个 op 幂等、可中断、可回滚 |
| history | 版本存储：被覆盖/删除的字节按 rev 保留，可取回 |
| rendezvous | 一次完整的双向同步会话（握手→交换状态→算 plan→执行→提交游标） |

## 4. 一致性模型（最重要的一条设计承诺，必须写进 README）

**收敛语义，不是强一致。** 精确承诺三条：

1. 一次 rendezvous 成功结束且没有"真冲突"时，两个 root 的文件树在**内容与元数据上等价**（判定标准见 §8 的比较规则）。
2. rendezvous 进行中，每个站点只保证看到自己本地已提交的状态；**不提供**"任一端读取到全局一致快照"。中途被 kill 见 §12 的崩溃不变量。
3. **真冲突**（同一 entry 在两次 rendezvous 之间被两端同时写过不同内容）**绝不自动猜测合并**。按 §7 的枚举表裁决，且失败方字节 100% 进 history。

每端维护单调 Lamport 计数 + 每 entry 的写入者标识，用来判定"谁相对 base 动过"。mtime 只作为 LWW 的参考输入和脏判定加速，**不作为正确性依据**（跨平台时钟不可信，§8）。

## 5. 功能需求（FR）

| # | 需求 | 验收方式 |
|---|---|---|
| FR-1 | 传播 add / modify / delete / rename（含目录移动）四种意图，双向 | §11 property test |
| FR-2 | 变更检测 = `notify` 事件 + debounce，**外加周期性全量扫描兜底**；事件丢失不得导致永久不一致 | 拔网/杀进程/写满 inotify 用例后仍能收敛 |
| FR-3 | 元数据索引落 SQLite（本地文件，非内存态），重启不重扫全量 | 重启后首周期只扫脏子树 |
| FR-4 | 传输通道复用 SSH（`russh`：exec 远端 `twosync agent --stdio`），**不新开监听端口** | 服务器 `ss -ltnp` 无新增端口 |
| FR-5 | 大文件分块增量：默认定长 4 MiB 块起步，只传脏块；变长 content-defined 分块（CDC）列为 M4 可选增强 | 改 10 GB 文件中的 1 MB，传输量 ≤ 若干 MB 而非 10 GB |
| FR-6 | 海量小文件批量打包传输（单连接多路复用 + 批帧），不把 RTT 打爆 | 1 万个 4 KB 文件的初次同步耗时可解释 |
| FR-7 | `plan` 子命令输出人类可读 + `--json` 的操作计划，**默认不执行**；`apply` 才执行 | 任何破坏性动作都有 dry-run 前置 |
| FR-8 | history / 回收站：删除与覆盖的文件移到 `<root>/.twosync/versions/<entry_id>/<rev>`，保留策略可配（版本数 N、天数 D、容量上限 C，LRU 淘汰） | 误删 100 个文件后可一条命令还原 |
| FR-9 | **删除熔断闸门**：单个 plan 内"删除条目数 > 已知的 X%（默认 20%）"或"删除字节 > Y GB（默认 2 GB）"→ 拒绝执行并退出码非 0，需 `--allow-mass-delete` 显式覆盖 | 把本地 root 清空后 sync，服务器不动并报警 |
| FR-10 | 冲突裁决按 §7 枚举表实现，策略项 `conflict.delete_vs_modify = modify_wins(默认) \| delete_wins \| halt`、`conflict.both_modify = lww(默认) \| halt` | 表格逐行有单测 |
| FR-11 | 排除规则采用 gitignore 语义（直接用 `ignore` crate），`**/.git/`、`.twosync/` 为**内置强制排除**（不可被配置取消，取消时报错） | 排除 .git 后同步不会污染仓库 |
| FR-12 | 崩溃一致性：所有写入走同目录 `tmp` + `fsync` + 原子 rename；op 级 journal，重启后重放或丢弃半成品 | §11 crash injection |
| FR-13 | 断点续传：chunk 级恢复 + 会话游标持久化；网络中断不产生半文件 | 传输中拔网线 |
| FR-14 | 保护同机其他进程：CPU/带宽令牌桶限速（`--max-cpu`/`--max-bps`），以及可选"高负载退避"（loadavg 或指定进程活跃时暂停同步） | NFR-2/3 实测 |
| FR-15 | `status` 子命令：当前游标、脏 entry 数、上次会话耗时/传输字节/CPU 峰值、待冲突列表 | 指标可脚本消费 |
| FR-16 | 后台常驻：Linux systemd user unit + Windows 计划任务/服务，日志轮转 | 重启机器后自动恢复 |
| FR-17 | rename 识别：同一 hash+size 在配对时间窗内"一删一增"视为重命名，只传播路径变更、不重传内容 | 8 GB 目录改名，传输量 < 1 MB |

## 6. 非功能需求（NFR，数字为先验估计，M1 结束前必须换成实测值）

- **NFR-1 内存**：稳态常驻 RSS ≤ **80 MB**（含索引）。估算依据：10 GB / 平均 200 KB ≈ 5 万条目，`entries` 行 ≈ 100 B → ~5 MB，加块摘要与 SQLite 页缓存；索引留在磁盘，靠页缓存而不是堆。超出即视为架构错误，不允许靠"加大内存"解决。
- **NFR-2 CPU**：单核机器上，同步任务不得吃满一核。首次 10 GB 全量 hash 预算 ≤ 5 min 且必须可限速；稳态增量周期 CPU 峰值 ≤ **10% 单核**。blake3 软件实现单核吞吐约 GB/s 量级，但弱 vCPU + 磁盘 IO 下实际按 200–500 MB/s 估，所以 10 GB 需要 20–50 s 纯 hash，限速后按分钟计。**这里必须实测，不接受纸面数字。**
- **NFR-3 IO 干扰**：读写走 `buffered + 顺序大块`，Linux 侧尽力 `ionice`(class 3)/`ioprio_set`；不允许出现"同步启动导致策略进程延迟抖动"的情况（若 §14 Q2 确认为交易机，此条升级为 M1 阻塞项）。
- **NFR-4 带宽**：默认上限 = 上行探测值的 80%，可硬配 `--max-bps`。
- **NFR-5 磁盘**：history 目录默认 ≤ root 体积的 **20%**，超限 LRU 淘汰并写日志。
- **NFR-6 可扩展文件规模**：单站 100 万条目内 plan 计算时间保持线性可预测（不出现 O(n²) 的两两比对）。
- **NFR-7 启动时间**：冷启动到能报"是否需要同步" ≤ 全量扫描耗时；热启动 ≤ 1 s。
- **NFR-8 单二进制**：目标 ≤ 15 MB，静态链接 musl（Linux 服务器侧），无外部 runtime 依赖。
- **NFR-9 日志**：`tracing` 结构化输出，每条 op 可溯源到 `(session_id, op_id, entry_id, rev)`。

## 7. 冲突与删除的裁决规则（必须枚举完整，缺项即 bug）

`A/B` 表示相对 base 的动作为 add/modify/delete/rename/none。

| base→local | base→remote | 判定 | 动作 |
|---|---|---|---|
| add | none | local 新增 | 复制到 remote |
| none | add | remote 新增 | 复制到 local |
| add | add, hash 相同 | 幂等重复创建 | 标记同一 rev，无传输 |
| add | add, hash 不同 | 同路径竞争创建 | 按 §7.1 保留两侧，胜者占原路径 |
| modify | none | local 改 | 传脏块到 remote，旧版进 history |
| none | modify | remote 改 | 反向同理 |
| modify | modify, hash 相同 | 等价修改 | 无传输，合并 rev |
| modify | modify, hash 不同 | **真冲突** | LWW(mtime + 时钟偏移修正)；败方字节进 history；mtime 差 < 2 s → `halt` |
| delete | none | local 删除意图 | remote 删除（移入 history） |
| none | delete | remote 删除意图 | 同理 |
| delete | modify | **删改冲突** | 默认 `modify_wins`：文件保留远端内容，本地删除意图记入 history 并写进 report 的 conflict 列表（理由：不可静默吃掉刚产生的内容） |
| modify | delete | 同上，对称 | 同上 |
| rename | none | 纯改名 | 只做路径变更，**零字节传输** |
| rename | rename 到不同路径 | 双改名 | 较新 rev 占路径，另一路径写 `.twosync-conflict-<rev>` 文件 |
| delete | delete | 双方都删 | 一次删除，history 保留一份 |
| none | none, 仅 mode/mtime 变 | 元数据抖动 | 可执行位在 unix↔unix 传播；mtime-only 不触发传输 |

**7.1 保留两侧的落地形式**：胜者占原路径；败者落 `<path>.twosync-conflict.<site>-<rev>`，同时字节进 history。这与 Syncthing 的 `.sync-conflict` 习惯一致，便于用户迁移。

**7.2 铁律**：任何 `delete` 类动作在本地执行前，必须确认该 entry 的当前字节已在 history 中（或 hash 与 history 中某 rev 相同）。做不到就拒绝执行该 op，不允许"先删再想办法"。

## 8. 跨平台差异矩阵（Windows ↔ Linux，本项目最容易翻车的地方）

| 议题 | 事实 | 策略 |
|---|---|---|
| 路径大小写 | NTFS 不敏感 / ext4 敏感 | 索引用 NFC 归一化 + `'/'` 分隔；维护 case-folded→canonical 唯一映射。若 plan 会把两个仅大小写不同的文件写到 Windows 同一位置 → **报错拒绝**，绝不静默覆盖 |
| 非法字符/保留名 | Windows 禁 `<>:"\|?*`、保留 `CON/NUL/COM1`、禁尾部点与空格 | 生成文件名时预检；对远端传来的路径做净化并拒绝，report 里单列 |
| mtime 精度 | NTFS 100 ns、ext4 ns、部分文件系统 2 s、网络盘更差 | 快速脏判定允许 size+mtime 粗筛，但**删除/覆盖前必须 hash 确认**；比较时按双方可信精度取整（默认 1 s 粒度，可配） |
| 时钟偏移 | 两端 NTP 偏差会污染 LWW | 维护 last-seen 双向往返估算 skew，LWW 用"校正后 mtime"；偏移超阈值（默认 5 s）→ 冲突转 `halt` |
| 文件被占用 | Windows sharing violation 会让打开失败 | 视为 `dirty-deferred` 重试并保留，**绝不当作 delete 或 missing**（这是会造成大规模误删的一类 bug） |
| 符号链接 | Windows 需特权/开发者模式 | 默认把 symlink 作为 link 元数据同步；目标在 root 外 → 拒绝（§10）。跨平台无法建 link 时按配置 `skip \| copy-file` 降级 |
| 权限位 | Unix mode / uid / gid vs Windows ACL | 只同步 owner 可执行位（可关）；ACL、xattr、owner 不同步，并在 report 里明说 |
| 稀疏文件 | 大文件常带空洞 | 全零块跳过，接收端 `set_len`+sparse 重建；否则 10 G 稀疏文件会被"压实"成真实 10 G |
| 长路径 >260 | Win32 需 `\\?\` 前缀 | 统一在 path 层封装，Windows 下验证 300+ 字符路径用例 |
| 硬链接 | 一端多目录同 inode | v1 不识别，按普通文件复制；明确写进已知限制 |

## 9. 协议与数据模型草案

**通道**：单条 SSH 连接上 exec 远端 agent（`twosync agent --stdio --protocol 1`），帧 = `u32 len + bincode/postcard`。认证 = SSH key（host key pinning 由 ssh 侧负责），不自建证书体系。

**消息集（v1）**：
`Hello{proto_ver, site_id, caps}` / `BeginSession{base_cursor}` / `StateDelta{cursor, entries[]}`（带分页与压缩）/ `PlanOffer{ops[], mass_delete_stats}` / `PlanVerdict{accept\|reject, reason}` / `FetchChunks{entry_id, block_ranges[]}` / `PushBatch{stream}` / `CommitOp{op_id, hash}` / `AbortOp{op_id}` / `EndSession{new_cursor}` / `PruneHistory{policy}`。

**要点**：
- 每个 op 带 `op_id` + 内容 hash → **重复提交是 no-op**，这是断点续传和崩溃重放的基石。
- `caps` 位协商 rename-detect / CDC / sparse / xattr，向后兼容。
- `PlanOffer` 阶段就把 FR-9 的熔断统计（删除条目数、删除字节数）传给对方，两边都同意才进入 `PushBatch`。

**SQLite schema（草案）**：
```
entries(entry_id PK, path UNIQUE, kind, size, mtime_ns, hash, chunk_param, rev_site, rev_lamport, base_rev_site, base_rev_lamport, dirty, updated_at)
chunks(entry_id, block_ordinal, block_hash)                 -- 仅大文件
history(entry_id, rev_site, rev_lamport, hash, size, stored_path, created_at, reason)
sessions(session_id, started_at, base_cursor, new_cursor, ops, bytes_sent, bytes_recv, cpu_ms_peak, result)
sites(site_id, lamport, last_seen, clock_skew_ns)
```

## 10. 安全要求

1. **路径穿越**：所有远端提供路径必须"先 NFC 归一化 → 拒绝绝对路径/盘符/UNC/`..` 分段 → 拼 root → canonicalize 校验仍在 root 内（含 symlink 解析后）"。任何一步失败 = 拒绝整个 op。
2. 终端输出对控制字符/ANSI 序列转义，防止恶意文件名做终端注入。
3. agent 只接受同步语义的消息，**没有"执行任意命令"这类消息**，也不 `shell` 拼接任何字符串。
4. 以低权限用户运行；不 setuid；history 目录权限 `0700`。
5. 限速之外要有配额：`PushBatch` 单帧上限、每会话最大字节数，防止对端被攻破后打爆本机磁盘（磁盘写满是同机其他项目的第一杀手）。
6. 是否静态加密（备份介质泄漏场景）见 §14 Q5，默认关，若开则 `chacha20poly1305` + 本地 keyfile。

## 11. 验收标准与测试策略（"拿得出手"就靠这一节）

1. **property-based（`proptest`）**：随机生成两端操作序列（add/modify/delete/rename/mkdir/symlink）+ 随机 rendezvous 轮数，断言三条不变量：
   - (a) 无真冲突场景下，N 轮后两树 `hash(tree)` 相等；
   - (b) 任何被覆盖或被删除的内容，都能按 `(entry_id, rev)` 从 history 完整取回且 hash 匹配；
   - (c) history 体积不超配额，且淘汰顺序符合 LRU 声明。
2. **crash injection**：在 apply 的每个阶段（帧边界、块写入中途、rename 前、journal 提交前）注入 `kill -9`，重启后断言：无半文件、无 orphan tmp 残留超 5 s、不变量 (a)(b) 仍成立、不重复传输已完成块。
3. **路径对抗 fuzz**：`../../`、`\\?\`、UNC、`CON`、尾部空格/点、超长名、仅大小写不同的两文件、NFC/NFD 混合、含 ANSI 转义的文件名 → 全部拒绝且不崩。
4. **差分测试**：同一份树变更，与 `rsync -n --delete`（以及 Syncthing 的默认结果）比对操作计划差异，人工核对分歧。既是正确性交叉验证，也是量化 delta 收益的素材。
5. **真机长跑**：构造 10 GB 混合语料（1 个大稀疏文件 + 10 万个 4 KB 小文件 + 若干二进制资产），随机变更脚本跑 72 h，记录 RSS/CPU/带宽/收敛时间曲线并写进 README。
6. **性能基准（`criterion`）**：全量扫描、脏判定、plan 计算、分块 diff、批量小文件打包，各给基线数字。
7. 判据：以上 1/2/3 未全绿，**禁止发任何 release**。

## 12. 里程碑

| 阶段 | 交付 | 可验证判据 |
|---|---|---|
| **M0 骨架** | `cargo init --lib --name twosync`；CLI(`plan/apply/status/scan/version`)、配置文件、SQLite schema + 迁移、单站扫描器 + hash、**dry-run 输出 plan**。先不联网 | 扫 10 GB 出 plan，给出实测扫描耗时与 RSS（先把 NFR-1/2 变成真数字） |
| **M1 单向 mirror** | SSH 通道 + `StateDelta/PlanOffer/PushBatch/CommitOp` + 定长分块增量 + **FR-9 熔断** + history + 原子写 | 本地删 30% 文件 → 服务器拒绝执行并提示；本地改 10 GB 文件的 1 MB → 传输量 ≤ 8 MB；crash injection 通过 |
| **M2 常驻** | watch + debounce + 周期兜底扫描、断点续传、限速、`status` 指标、systemd/计划任务 | 拔网重连后自动续传；稳态 CPU ≤ 10% 单核实测 |
| **M3 双向** | 每端 lamport/base、§7 全表实现、冲突 report、rename 检测(FR-17) | property test (a)(b) 通过；§7 表格逐行有单测 |
| **M4 硬化** | CDC 变长分块、稀疏、批传输调优、72 h 长跑、README/文档/发布 | 差分测试与基准数据入库，指标写进 README |

M0–M2 阶段**语义上是单向的**，因此不需要回答 §14 Q1 就能开工；Q1 只阻塞 M3。

## 13. 技术选型

| 用途 | 选型 | 理由 / 备选 |
|---|---|---|
| CLI | `clap` 4 (derive) | 事实标准 |
| 异步 | `tokio`，`worker_threads` 可配（默认 1–2，为单核机器让路） | 备选 `smol`（体积更小） |
| 文件事件 | `notify` 6 + 自研 debounce/inotify-overflow 处理 | 事件不可靠是已知事实，必须有兜底扫描 |
| 哈希 | `blake3` | 单核吞吐最优；块摘要 20 B 定长 |
| CDC | `fastcdc`（Gear 哈希） | M4 再用；不要一开始就为它设计索引 |
| 索引 | `rusqlite`（bundled SQLite，WAL） | 你熟存储引擎，这里要控制的只有 schema 与迁移纪律 |
| 排除规则 | `ignore` crate | 直接复用 gitignore 语义，零学习成本 |
| 传输 | `russh`（server/exec 复用系统 sshd） | 备选 `ssh2`（libssh2，绑定简单但同步 API 需包一层） |
| 序列化 | `bincode` 或 `postcard` | postcard 更省、无版本耦合坑；先固定一种并在 `Hello` 里协商 |
| 日志 | `tracing` + `tracing-subscriber`（JSON file layer + 轮转） | |
| 限速 | 自研令牌桶（`tokio::time`），CPU 与带宽各一桶 | 必须自研：要能随负载退避 |
| 测试 | `proptest`、`criterion`、`cargo-fuzz`（路径解析层） | §11 的地基 |
| 工程 | `cargo xtask` 或 `justfile` 驱动 CI 之外的验证动作 | 不依赖托管 CI 也能一键跑全套测试 |

## 14. 待决问题（回答后本文档升 v0.2 并冻结 M0 范围）

1. **服务器侧的写入是程序自动产出，还是有人会直接编辑文件？两边会不会在同一个 rendezvous 周期内同时改同一个文件？** → 决定 M3 冲突只需 LWW+双保留，还是要上三方文本合并（后者是另一个量级的工程，且大概率应该交给 git 而不是本工具）。
2. 那台 1 核 6 G 机器上是否有**对延迟敏感**的常驻进程（策略/交易类）？→ 决定 FR-14 的"高负载退避"从 M2 提到 M1 阻塞项。
3. 2–10 GB 的构成：文件数量级？最大单文件多大？小文件占比？→ 决定分块参数（4 MiB 定长是否合适）、索引规模、rename/CDC 的优先级。
4. 会不会出现**第三个端**（另一台笔电、NAS）？→ 两端假设能否放宽为"协议支持多 site 但实现仍为 2 个"。
5. history 里可能留有敏感数据（旧版本密钥、客户数据），**需要静态加密吗**？不加密的话 history 就是一个 20% 体积的明文备份泄漏面。
6. 除 `.git/` 外，必须内置排除什么（venv/`node_modules`/数据库热文件/锁文件/正在运行的二进制）？→ 进 FR-11 的强制清单。

## 15. 命名

- 目录/crate：`twosync`（已确认 crates.io 上 `twosync` 无同名包，`converge`/`seesaw`/`pingpong` 均已被占）。
- 二进制与子命令：`twosync plan` / `apply` / `status` / `scan` / `restore <entry_id> --rev <r>` / `agent`。
- 数据目录：`<root>/.twosync/`（索引、history、journal、日志）。
- 若你想换名，现在改的成本是 0（目录刚建，还没有代码）。
