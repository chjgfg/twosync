# twosync — 需求与范围说明

版本 **v0.4**（存储基座换成 storage-engines）｜2026-10-15｜状态：待确认 §14 的 9 个开放问题后冻结 M0 范围
历史版本留档：`REQUIREMENTS-v0.1.md`（收敛模型）、`REQUIREMENTS-v0.2.md`（单 ref 版本库模型 + SQLite）。逐版差异见 §17。

---

## 0. 一句话

**一个 P2P 的、跨 Windows/Linux、为大二进制文件优化的微型 git，存储基座是自研的 `storage-engines`。**
Win10 上配好"哪些项目要同步"，Ubuntu 那端接手同一个项目、做同样的文件操作；每一端的每次改动都变成一个**不可变 commit** 进本端版本库，历史经对等连接双向流动。历史一定找得回来——因为真值来源是版本库，不是"扫描两个目录然后猜差异"。

站点之间**没有主从**：任何一端都能发起会话、都能推、都能拉。这与 git 的 P2P 性质同源（任何一份 clone 都能推给任何一份，不需要中心权威）。

## 1. 背景与真实场景

- 项目体积 2–10 GB，含大量不适合进 git 主线的二进制资产/数据/产物。一个站点上有**多个这样的项目**。
- 服务器：**1 vCPU / 6 GB RAM**，同机还跑着其他常驻项目。这是全部资源预算的硬约束来源。
- 本地 Windows 10（10.0.19045）是主要编辑端；Ubuntu 端也会直接改文件。**两端都会写**是既定事实，不是例外。
- 工作流诉求：跨 OS 搬同一份工程，改完之后能回答"这个文件上周三是什么样、是谁在哪个改动里删掉的"。
- 为什么不是 git 本体（三条都在需求里，不是随口一说）：
  1. **巨型二进制**。git 的对象格式里 **blob 的粒度就是整个文件**：每个版本存一份 zlib 全量 blob，pack 的 delta 只在 window（默认约 10 MiB）内找基对象、对二进制几乎无效。一个 10 GB 文件改 1 MB，git 存的是全量、传的也是全量。FR-5 的块级增量是本工具对 git 唯一的实质领先，也是它存在的理由。
  2. **历史不可有界**。git 的历史只增不减，`gc` 只能回收不可达对象；本工具要的是"配额内留尽量多版本"（NFR-5），git 表达不了这个意图。
  3. **跨 OS 归一化**。§8 那张表（非法字符、`\\?\` 长路径、执行位、稀疏、大小写折叠）在 git 里会天天撞 `core.autocrlf`/`core.filemode`/symlink 那类配置墙。
- **由此得出的一个不可兼得**（§17 记这一条，免得后人误读成"没想明白"）：想要"git 命令原样可用"就得采用 git 对象格式，而 git 对象格式容不下上面第 1、2 条。所以本工具**自定对象格式、自建命令面**，形状照 git（§16.2）。
- 项目源码本体仍应由一个 bare git 仓库走 SSH 管；twosync 管的是**工作树里那些不该进 git 的东西**。两者不互斥。
- **为什么自研引擎当基座**：§9.2 的访问路径全是 KV 形状的（点查、按前缀扫、按序扫），没有 join、没有动态谓词，SQL 层本来就是浪费的。用 `storage-engines` 之后，"同一套不变量在 LSM / B+ 树 / 纯内存上跑通"本身就成了 README 里能拿出来的表（§10.8），而不是内部实验。

## 2. 非目标（明确不做，防止范围膨胀）

1. **不做 git 的替代品**：命令面照 git 的形状自建（§16.2），但**不做内容级三方合并**，因此 `merge` 只在两侧改动路径无交集时做结构合并，`rebase -i` 的冲突解决不提供。源码协作仍走真 git。
2. **v1 实现只连一个对端（1+1）**，但**协议与存储按多站点/多 heads 设计**（§4、§7、§9.2）。理由：单 ref → 多 heads 是存储格式层面的改动，留着单 ref 上线再加第三端只能重做格式并写迁移；现在改的成本近乎零。
3. **不做 mesh 路由与 NAT 穿透**：两台都在家用路由器后面时谁拨谁、hole-punching、中继转发，整个子系统推到 M5（§16.4）。v1 假设至少一端可被 SSH 拨通，或两端各自定时拨向可拨通的那一侧。
4. 不做仲裁者 / 中心服务器 / 云端目录服务。
5. 不做 GUI / Web 面板。CLI + 结构化日志。
6. 不做多用户 ACL。认证复用 SSH key + 本地对等指纹表（§9.5）。
7. 不做 rsync 字节码兼容，也不做 git 协议 / git 对象格式兼容（思想与手感借用，不互操作）。
8. 不自动转换 CRLF/LF。**同步是字节级的**，检出时不做任何"内容友好化"。
9. 不做实时协作/字符级 CRDT。收敛粒度是 commit。
10. **不自研第五个存储引擎**：本项目的存储层就是 `storage-engines`，不在这里再写一份 KV。

## 3. 术语

| 术语 | 定义 |
|---|---|
| peer | 一个对等站点（不再有 local/remote 之分，只有"我"和"它"） |
| root | 一个 peer 的同步根目录，下面挂着 N 个 project |
| project | 一份独立的版本库 + 一个工作树目录，有自己的名字、排除规则、head 集合 |
| worktree | project 目录下真实文件树 = 某 commit 的检出结果 + 未提交改动 |
| block | 定长内容块（默认 4 MiB），CAS 的存储单元，键 = blake3(块内容) |
| blob | 一个文件内容的逻辑标识 = 有序块清单（+ 稀疏标记），不可变 |
| tree | 一个目录清单：`name → (kind, blob_hash, mode, size)`，按目录切分 |
| commit | `(parents[], tree, peer, ts, msg, stats)`，不可变；parents 是 0..N 个 |
| head | "最前沿"的 commit；一个 project 在一个 peer 上**同时可有多个 head** |
| ref | 命名的 head 集合。同步只看 `main`；`branch`/`tag` 是本地命名，**v1 不参与同步**（§16.2） |
| CAS | 内容寻址块仓，**站点级共享**：`<root>/.twosync/objects/<hash[0:2]>/<hash[2:]>`，块元数据行带 `project_id` 命名空间 |
| 引擎 | `storage_engines::engine::KvEngine`（默认 `lsm_tree`），只存**元数据与清单**，不存文件字节 |
| union | 两个 peer 的 head 集合取可达并集，取代 v0.2 的"快进"判定（§7） |
| collision | 同一路径被两个不同 head 各自改过——本工具里唯一需要人裁决的情形 |
| session | 一次 rendezvous：自动提交 → 交换 heads → union 判定 → 块协商 → 传输 → 检出 → head 集合前移 |

## 4. 一致性模型（最重要的设计承诺，必须写进 README）

**版本库语义，不是收敛语义；并集语义，不是覆盖语义。** 精确承诺五条：

1. **只增不减不变量**：任何一次 head 集合前移，结果必须**包含**前驱集合的全部可达 commit（union），或经人显式丢弃（`--take-*`）。不存在"用对端状态覆盖我方我不知道的历史"这条代码路径——不是拒绝，是**压根没有这个 API**。
2. **单 ref 是特例**：1+1、且从不本地分叉时，head 集合恒为 1 个元素，union 退化为快进。v0.2 那张四行表是这套模型的一个投影，不是另一套规则。
3. **等价性判据**：两 peer 的 `main` head 集合相等 ⇒ 两 peer 的工作树内容与元数据等价（平台固有不可移植项按 §8 明说并计入 report）。
4. **collision 绝不自动裁决**：同一路径被两个 head 改过 → 停止并报告（§7.1）。多 head 本身是正常状态，**不是**错误；只有碰撞才需要人。
5. **不可变 + 可取回**：任何 commit 自建立起内容不再改变，其块集合可从 CAS 完整重建且 hash 匹配；GC 只删对所有保留 head 都不可达的块（§9.3）。

**mtime 与时钟完全不进正确性论证。** 只当"快速发现可能变了的文件"提示用（§8）。Lamport/base/`dirty` 游标、以及 v0.2 的"唯一 main ref"假设，都已从模型里删除。

## 5. 功能需求（FR）

| # | 需求 | 验收方式 |
|---|---|---|
| FR-1 | 把工作树改动变成 commit：add/modify/delete/**显式 rename**/mode。改名由 `mv` 子命令或同 blob_hash 匹配得出，记在 commit 里，不靠事后猜 | §10 不变量 (b) |
| FR-2 | 变更检测 = `notify` + debounce + **周期全量扫描兜底**；扫描错误的后果只能是"本轮漏提交、下轮补上"，**不得**导致已提交历史丢失或工作树被删 | 拔网/杀进程/inotify 溢出后仍正确 |
| FR-3 | 元数据与清单存 `storage-engines`（默认 `lsm_tree`，运行时可切换），磁盘态、重启不重扫全量；**文件字节不进引擎**（在 CAS 里）。所有扫描**必须分页**（§9.6.1） | 重启后首周期只扫脏子树；100 万条下扫描峰值 ≤ **8 MiB**，且**与块大小、文件总数无关**（定义见 NFR-1） |
| FR-4 | 传输复用 SSH（`russh`：exec 对端 `twosync agent --stdio`），**不新开监听端口** | 服务器 `ss -ltnp` 无新增端口 |
| FR-5 | **块级增量**：推之前做 `want/have` 块清单协商，只传对端缺失的块；默认定长 4 MiB，CDC 列为 M4 增强 | 改 10 GB 文件中的 1 MB → 传输量 ≤ 若干 MB，**与 git 的传输量对比写进 README** |
| FR-6 | 海量小文件批量打包（**一条 SSH 连接多路复用 N 个 project + 批帧**），不把 RTT 打爆 | 1 万个 4 KB 文件的初次推送耗时可解释 |
| FR-7 | `plan` 输出本次会提交什么 + union 判定结果 + 会碰撞的路径，**默认不执行**；`apply` 才执行 | 任何破坏性动作都有 dry-run 前置 |
| FR-8 | 命令面（git 形状，分层见 §16.2）：第一层 `status/log/diff/show/checkout/restore/commit/push/pull/plan/apply/gc/agent/config`；第二层 `branch/tag/reset/revert/stash/cherry-pick/blame/bisect` | 每条有单测 |
| FR-9 | **有界历史**：CAS 配额默认 ≤ root 体积 20%（**站点级全局配额**，非每项目），超出时按"最老且不可达优先 / 同 commit 整批淘汰"回收，**不得**删掉仍被保留 head 可达的块。淘汰顺序与保留意图（版本数 N / 天数 D）可配 | §10 不变量 (d) |
| FR-10 | **删除熔断**：commit 的删除条目数 > max(已知条目的 20%, **20 条**) 或删除字节 > 2 GB → 拒绝该 commit 进 head 集合、退出码非 0，需 `--allow-mass-delete`。理由是防客户端 bug 把误 `rm -rf` 变成合法历史，不是防并发。**绝对下限是本版加的**：纯 20% 在小项目上不可用（2 个文件删 1 个就是 50%，那个仓库从此提不了交），而熔断要拦的是规模不是比例。本地 `commit` 与接收侧**都过这道闸** | 清空本地 root 后 sync，对端不动并报警；100 条里删 30 条 → 本地 commit 也拒 |
| FR-11 | **多 head 与碰撞处理**（§7）：默认 union；出路 = `--take-local` / `--take-remote` / `--keep-both`（仅无碰撞时结构合并）/ 交给 git | 表格逐行有单测 |
| FR-12 | 排除规则用 gitignore 语义（`ignore` crate）；`**/.git/`、`.twosync/` 为内置强制排除（配置取消即报错） | 排除 .git 后不会污染仓库 |
| FR-13 | 崩溃一致性：块写入 → `fsync` → 才允许被清单引用；head 集合更新是最后一步且原子；op 级 journal，重启后重放或丢弃半成品。**引擎事务的持久化点由 `fsync_on_commit` 提供，但 CAS 文件的 fsync 顺序仍归 twosync 自己保证** | §10 crash injection |
| FR-14 | 断点续传：块级恢复 + 会话游标持久化；中断不产生半文件、不重复传已到位的块 | 传输中拔网线 |
| FR-15 | 保护同机其他进程：CPU/带宽令牌桶（`--max-cpu`/`--max-bps`），**GC、扫描、引擎 vacuum 同样受限速管辖**；**project 增加时总预算不叠加**（全部共用同一桶）；可选高负载退避 | NFR-2/3 实测 |
| FR-16 | `status`：每 project 的本地/对端 head 集合、ahead/behind、未提交改动数、CAS 用量与全局配额、上次会话耗时/传输字节/CPU 峰值、碰撞列表、**引擎健康度（`active_txns` / `max_chain` / `rows_above_one`）** | 指标可脚本消费 |
| FR-17 | 后台常驻：Linux systemd user unit + Windows 计划任务，日志轮转；**任一端都能作为会话发起方**（对等，§16.3） | 重启机器后自动恢复；Windows 休眠错过几轮后由对端拉平 |
| FR-18 | 跨 OS 归一化在**检出层**实现（§8），不改内容、不"友好化" | §10 路径对抗 fuzz |
| FR-19 | **多项目管理**：`project add/list/use/remove/status`（§16.1）；每 project 独立版本库与配置，共享站点级 CAS 与共享引擎实例（按 `project_id` 键前缀分区）；一轮 session 可跨多个 project | 20 个 project 一轮跑完，仍只 1 次 SSH 认证 |
| FR-20 | **revspec 子集**：`HEAD`、`<hash>`、`HEAD~3`、`HEAD^`、`<branch>`、`main:path`，加 `--stat/--hard/--patch` 等 flag 拼写 | 手感迁移是硬需求（§16.2 末） |
| FR-21 | **启动自检**：读 `KvEngine::durability_profile()`，`transaction_wal ∧ fsync_on_commit ∧ data_checksum` 三项不全 = **拒绝启动并说明缺哪一项**（§9.6.5） | 单测：注入 memory/bitcask 画像必须拒 |

## 6. 非功能需求（NFR，数字为先验估计，M1 结束前必须换成实测值）

> **记录规矩（2026-10-16 立，起因见 §17 补记）**：本项目任何性能数字必须注明 **profile（debug / release）+ 硬件 + 数据形状**，
> 三样缺一律视为无效数字——不许进文档、不许进提交信息、更不许据此下结论。这套代码上 debug 与 release 实测差 5–40 倍。

- **NFR-1 内存**：稳态常驻 RSS ≤ **80 MB**（**跨全部 project 之和**，不是每项目）。扫描/写块期间的**峰值新增分配 ≤ 8 MiB**，构成是 `2×CHUNK(64 KiB) + 一批键值 + 该文件清单`；**真正的不变量是「与块大小、与文件总数都无关」**——原来写的「块 4 MiB」与「峰值 ≤ 4 MB」自相矛盾（见 §17 补记），靠**两遍流式**（§9.6.1）才同时成立：第一遍只算 hash 不写字节，第二遍只搬缺失的块。超出视为架构错误，不许靠加内存解决。
- **NFR-2 CPU**：单核机器上不许吃满一核。首次 10 GB 建库（全量 hash + 切块 + 落 CAS + 写清单进引擎）预算 ≤ 8 min 且必须可限速；稳态增量会话 CPU 峰值 ≤ **10% 单核**。CAS 化之后建库比 v0.1 的"只 hash 不搬字节"更重 IO——这条必须实测，不接受纸面数字。
- **NFR-3 IO 干扰**：读写走 buffered + 顺序大块，Linux 侧尽力 `ionice`/`ioprio_set`。**后台 GC、引擎 vacuum、LSM compaction 都不得制造 IO 突发**（单核机上这是最容易被忽略的一项）。不允许出现"同步启动导致策略进程延迟抖动"。
- **NFR-4 带宽**：默认上限 = 上行探测值 80%，可硬配 `--max-bps`。
- **NFR-5 磁盘**：CAS ≤ root 体积 20%（FR-9，站点级）；`status` 必须能报出"当前历史深度相当于几个全量副本"、**跨 project 去重省下了多少**、以及**引擎自身占了多少字节**（MVCC 版本行也是磁盘账），否则配额是不是够用没人知道。
- **NFR-6 commit 生成成本**：自动提交一轮的耗时是 **O(变更目录数)**，不是 O(文件总数)。10 万小文件的工程改 3 个文件，建 commit 不许重抄全树清单。
- **NFR-7 可扩展规模**：单 peer 100 万文件 / 10 万 commit / 20 project 内，`log`/`diff`/union 判定保持线性可预测，不出现两两比对。**union 判定不许退化成"两边 head 相乘"**。
- **NFR-8 启动**：冷启动到能报"有无 ahead/behind" ≤ 一次全量扫描；热启动 ≤ 1 s（**打开引擎 + 读 `health()` 必须落在这 1 s 内**，`health()` 是 O(物理行数)，不许放热路径）。
- **NFR-9 单二进制**：目标 ≤ 15 MB，Linux 侧静态 musl，无外部 runtime。`storage-engines` 依赖只有 5 个（serde/bincode/memmap2/fs4/log），体积友好。
- **NFR-10 日志**：`tracing` 结构化，每条操作可溯源到 `(session_id, project_id, commit_hash, path)`。

## 7. 多 head、碰撞与删除的处理（枚举完整，缺项即 bug）

v0.1 那张 16 行"相对 base 动作为 A/B"的裁决表**整张作废**（它存在的前提是没有可信历史）；v0.2 的四行快进表**升级为并集表**。真值不再是"一个 ref"，而是"head 集合"：

| 我的 heads | 它的 heads | 判定 | 动作 |
|---|---|---|---|
| 被 H 全部包含 | H | 我该拉 | 拉缺失 commit + 缺失块 → 检出 → heads = H |
| 全被对端可达 | 包含我的 | 该推 | 推缺失块 + commit → 对端 heads = 并集 |
| 相同 | 相同 | 已同步 | 无传输 |
| 不相交但可并 | — | **多 head（正常状态）** | heads = 并集；无碰撞则任一 head 都可作检出目标，报告列出全部 head |
| 并集后**同一路径被两个 head 改动** | — | **collision** | **停止并报告**，绝不自动挑一边（§7.1） |

**7.1 collision 的出路**（默认第一条，即 FR-11）：`plan` 列出各 head 的来源 peer、涉及路径、字节数；`--take-local` / `--take-remote` 由人挑一个 head 作检出目标（**其余 head 仍留在本地库，历史不丢**，只是不再是 head）；`--keep-both` 仅在这些 head 的改动**路径无交集**时生成一个多亲合并 commit；有交集 → 只能人挑，或交给真 git 合并。

**7.2 未提交改动 × 检出目标**（v0.2 残留的那一处，原样保留）：会话开头先自动 commit 本地改动（于是工作树必然 clean）；提交不了的项（文件被占用、权限不足，§8）标 `deferred` 并**拒绝检出这些路径**，其余照常。deferred 列表必须在 `status` 里可见。这是 LWW 唯一换了地方的地方，其余全在版本库层消解。

**7.3 铁律**（保留 v0.1 §7.2）：任何工作树删除在落地前，必须确认该 blob 的块已在 CAS 且已进某个 commit。做不到就拒绝这个 op，不允许"先删再想办法"。

**7.4 为什么多 head 是前提而非妥协**：三端 + 只允许快进 = 无解（A、B 各自提交，C 收到第二个时无法快进，永远卡住）。任何"以后加第三端"的想法都必须落在 union 语义上，或落在"每项目单写者"这种削弱版上。§2.2 因此把格式改动提前到现在。

## 8. 跨平台差异矩阵（Windows ↔ Ubuntu，最容易翻车的地方）

| 议题 | 事实 | 策略 |
|---|---|---|
| 路径大小写 | NTFS 不敏感 / ext4 敏感 | 清单键 NFC 归一化 + `/` 分隔；维护 case-folded→canonical 唯一映射。若检出会把两个仅大小写不同的文件写到 Windows 同一位置 → **报错拒绝**，绝不静默覆盖 |
| 非法字符/保留名 | Windows 禁 `<>:"\|?*`、保留 `CON/NUL/COM1`、禁尾点与尾空格 | commit 时就净化并拒绝该路径（记入 report），**不要**等到检出那端才炸 |
| mtime 精度 | NTFS 100 ns、ext4 ns、部分文件系统 2 s、网络盘更差 | 只作脏判定提示；提交前必须 hash 确认。**不参与任何正确性判定** |
| ~~时钟偏移~~ | 跨 OS 不可信 | 不需要估算 skew：commit 时间戳只用于显示和保留策略，不用于裁决（v0.1 为 LWW 才需要它） |
| 文件被占用 | Windows sharing violation 让打开失败 | 该项 `deferred`，本轮不提交、不检出（§7.2）；**绝不当作 delete 或 missing** |
| 符号链接 | Windows 需特权/开发者模式 | link 目标作为 blob 元数据同步；目标在 project root 外 → 拒绝（§9.4.1）。无法建 link 时按 `skip \| copy-file` 降级并明写 |
| 权限位 | Unix mode vs Windows ACL | 只同步 owner 可执行位（可关）；ACL/xattr/owner 不同步，report 里明说 |
| 稀疏文件 | 大文件常带空洞 | 全零块在 blob 清单里标 `is_zero` 不占空间，检出端 `set_len` + 稀疏重建；否则 10 G 稀疏文件会被"压实"成真实 10 G，配额瞬间被吃穿 |
| 长路径 >260 | Win32 需 `\\?\` 前缀 | 统一在 path 层封装，Windows 下验证 300+ 字符用例 |
| 硬链接 | 一端多目录同 inode | v1 不识别，按普通文件各存一份清单（CAS 下只占一份字节，但改名不会联动）；写进已知限制 |
| 换行/编码 | — | 不转换（§2.8）。跨 OS 编辑同一文本文件请交给 git，不要交给本工具 |
| 文件锁 | 引擎自己占一把库锁（`.lock`），Windows 上 `SystemTime` 粒度约 15.6 ms | 站点级**单实例**：一个 `twosync` 进程持一个引擎句柄，子命令走同一进程或拒绝并发（拿不到锁就报错，不重试）。测试临时目录必须带序号，否则并发用例互踩 |

## 9. 协议、数据模型、信任与安全

### 9.1 通道与消息

单条 SSH 连接上 exec 对端 agent（`twosync agent --stdio --protocol 1`）。**一条连接服务多个 project**：帧 = `u32 len + postcard`，消息体带 `project_id`，各 project 的状态机在这条连接上交错推进（FR-6/FR-19）。认证 = SSH key + 本地对等指纹表（§9.5）。

`Hello{proto_ver, peer_id, caps}` / `BeginSession{project_id, my_heads[]}` / `HeadsState{heads[], ancestors[]}`（对端自报 head 集合与可达祖先，用于 §7 判定与 `have` 采样）/ `WantHave{block_hashes[]}`（bloom 或全清单，按 caps 协商）/ `MissingBlocks{hashes[]}` / `PushBatch{stream}` / `PushCommit{commit, parents, trees, blobs}` / `VerifyCommit{hash}` / `UnionAccept{from_heads, to_heads}` / `RejectCommit{reason, mass_delete_stats}` / `CollisionReport{paths[], heads[]}` / `EndSession{}` / `GcProposal{policy}`。

要点：块以 hash 为身份 ⇒ **重复推送天然是 no-op**，这是断点续传与崩溃重放的基石（v0.1 靠 `op_id` 达成，现在免费）。`caps` 位协商 CDC / sparse / xattr / have 表示形式 / 多 project 复用。`UnionAccept` 前两侧都必须过 FR-10 熔断；`RejectCommit` 拒绝的是**那个 commit**，不是整场会话（其余 project、其余 head 照常推进）。

### 9.2 存储模型：KV 键编码（取代 v0.2 的 SQLite DDL）

**为什么能换**：§9.2 需要的操作全是点查、按前缀扫、按序扫——`KvEngine` 门面的 `get/multi_get/set/batch_set/scan/prefix_scan/seek/key_count/incr` 全覆盖，没有一处需要 join 或动态谓词。SQL 层本来就是浪费的。

编码约定：键 = `1 B 类型标签 + project_id: u32 BE + 主体`；所有数值 **BE 编码**以保证"字节序 = 逻辑序"；`hash` 是 32 B blake3；值用 `postcard`。

| 标签 | 键 | 值 | 访问路径 |
|---|---|---|---|
| `C` | `C\|p\|commit_hash` | commit 记录：tree_hash, peer_id, ts, msg, n_add/n_mod/n_del/n_ren, bytes_logical | 点查；`log` 沿 parents 遍历 |
| `c` | `c\|p\|commit_hash\|ordinal_be` | parent_hash | `prefix_scan(c\|p\|hash)` = 一个 commit 的全部亲 |
| `T` | `T\|p\|tree_hash` | 目录清单（name/kind/blob_hash/mode/size 数组） | 检出与 diff 的单位；一次点查一个目录 |
| `B` | `B\|p\|blob_hash` | 块清单：`[block_hash]` + `is_zero` 位图 | 文件内容重建 |
| `K` | `K\|p\|block_hash` | 块元数据：size, chunk_param, refcnt, last_access_ts, pinned | `want/have` 反查、去重统计 |
| `k` | `k\|p\|refcnt_be\|block_hash` | 空（存在即索引） | **按 refcnt 排序的二级索引**：GC 找 0 是 `prefix_scan(k\|p\|\x00…)`，不是全表扫 |
| `W` | `W\|p\|path` | worktree 状态：kind, blob_hash, size, mtime_hint_ns, mode, symlink_target, scanned_at | 脏判定；大小写唯一性检查；**`scanned_at` 兼任"本轮看见过"的存活性标记**（见 §8 与 §17 补记） |
| `H` | `H\|p\|ref_name\|commit_hash` | 空（存在即 head）。**原设计的 `is_head` 标记已废弃**：head 身份已经在键里，值再存一份就是两个真值 | head 集合 = `prefix_scan(H\|p\|main\|\x00)`；**一对多天然表达**（这是 v0.2 单主键 `refs` 撑不住多 head 的根因） |
| `A` | `A\|p\|path_folded` | canonical path | 大小写折叠唯一映射（§8） |
| `S` | `S\|p\|session_id_be` | 会话统计 | `status` |
| `P` | `P\|peer_id`（无 project 前缀，站点级） | TOFU 指纹 + 拨号偏好 | §9.5 |

**不用列族/多桶**：LSM 里确实有 `cf.rs`，但门面的 `KvEngine` **完全没有暴露 CF**（`EngineConfig` 只透传 B+ 树的 `order`/`max_cache_pages`）。一旦用 CF 就丢了"运行时切换四个引擎"这个卖点，而键前缀能拿到同样的分区效果 + 跨 project 前缀扫 + 四引擎同码可跑。

相对 v0.2 的 schema：**删** `refs/heads` 表（改 `H` 前缀）、`commit_parents` 表（改 `c` 前缀）、`tree_entries` 表（折进 `T` 的值里，一次点查拿整个目录，符合 NFR-6）、`sites`（改 `P`）；**新增** `k` 这个 refcnt 二级索引——SQLite 时代它是 `CREATE INDEX`，现在必须自己编码，**这是换 KV 的真实代价：每条二级索引都是一次手工编码 + 一份"更新时不许忘"的守卫**。

### 9.3 GC 与保留（配额的可实现路径）

保留集 = **所有 head commit 的可达闭包** ∪ 按 FR-9 策略选出的更老 commit（天数 D / 版本数 N）∪ 显式 `pinned`。淘汰 = 删除整批 commit 的清单行 → `refcnt--`（**用 `KvTxn::decr`，不许 read-modify-write**，那是两个事务、中间崩了就漏减）→ 从 `k` 索引取 `refcnt==0` → 落 pending-delete 列表 → 异步 `unlink`（**不许在 decrement 现场内联删除**，否则 kill 在中间会让清单引用已消失的块）。删除动作全程挂 FR-15 令牌桶。共享 CAS 意味着 refcnt 跨 project：`project remove` 只能 decrement，不能假设块属于它。

### 9.4 安全

1. **路径穿越**：对端提供的路径必须"NFC 归一化 → 拒绝对路径/盘符/UNC/`..` 分段 → 拼 project root → canonicalize 校验仍在 root 内（含 symlink 解析后）"，任一步失败 = 拒绝整个 commit。
2. 终端输出转义控制字符/ANSI，防恶意文件名做终端注入。
3. agent 只接受同步语义消息，**没有"执行任意命令"这类消息**，不 `shell` 拼接字符串。
4. 低权限用户运行；不 setuid；`.twosync/` 权限 `0700`。
5. 配额：`PushBatch` 单帧上限 + 每会话每 project 最大字节数 + CAS 硬上限。**磁盘写满是同机其他项目的第一杀手**，而版本化 + 多 project 让它来得比 v0.1 更快。
6. 静态加密（历史里可能留有旧密钥/客户数据）见 §14 Q4；默认关，开启则 `chacha20poly1305` + 本地 keyfile。**历史版本本身就是一个放大的泄漏面**，这是版本化换来的新风险。
7. 完整性：CAS 读取时按块 hash 校验，不符即报错并拒绝检出（不是静默重传，避免把损坏当缺失）。**引擎侧的校验缺口由这一条兜住**（§9.6.3）。

### 9.5 信任模型（P2P 带来的新东西）

v0.2 隐含"我拨的服务器 = 可信"，靠 SSH host key 单方面兜住。对等化之后要显式回答：

1. **TOFU 指纹表**（`P` 前缀）：首次连接的 peer pubkey 指纹入表并告警，之后不符 = **拒绝会话**（不是提示）。
2. **commit 来源不签名**：`commits.peer_id` 只是元数据（显示谁产生的），v1 **不做身份签名**，所以"历史可被本机伪造"这一限制真实存在，写进 §15.7。若要更强：commit 加 Ed25519 签名（key 本地存），列为 M5。
3. **一个 peer 不能污染另一个的目录布局**：`project add` 是本机行为；会话只对**两边同名且都已确认配对**的 project 交换状态，绝不自动在对端建目录。

### 9.6 引擎约束（来自 `storage-engines`，违反即事故）

换基座不是换个依赖那么轻。以下六条是读它的门面源码得到的**事实**，不是猜测：

1. **扫描返回物化 `Vec<ExportRecord>`**，每行两个堆分配（`key: Vec<u8>` + `value: Option<Vec<u8>>`）。100 万条一次全表扫 ≈ 150–250 MB 峰值 → **直接顶穿 NFR-1 的 80 MB**。规则：任何扫描必须**分页**——`scan(Some(lo), Some(hi))` 定窗 + `seek` 推进游标，窗口大小按 NFR-1 的 8 MiB 反推（约 2000 条一批）。**这是 M0 的阻塞项，不是优化项**（§10.1(f) 有断言）。实测（**release**，Windows 10 开发机，5002 条目 / 32 MiB）：`peak heap = 5.6 MiB`，与那个 32 MiB 大文件无关 ⇒ 常数内存成立；同一份数据同一份代码的 **debug 构建**慢 5–40 倍、且同规模抖动 8 倍，所以离开 profile 的数字没有意义（§6 引言）。
2. **`active_txns > 0` 时 vacuum 整轮跳过**（引擎明写）。所以一个跨阶段持有的只读事务 = 版本链无限增长 = 你在 0.4.0 刚堵掉的"随 uptime 恶化"通道在 twosync 里复活。规则：**事务生命周期 ≤ 单个阶段**，plan 计算分片、每片一个短事务、算完立刻 drop；`status` 必须报 `health().{active_txns, max_chain, rows_above_one}`，M2 起按分钟采样并画进 §10.5 长跑曲线。
3. **LSM 的 `blob_threshold` 默认 4 KiB，超过就写 `blobs/*.blob` 裸文件，而画像里 `blob_checksum = false`——损坏是静默的**。一个 1000 条目的目录清单正好超 4 KiB。两条缓解：① 引擎里每个 value 读回时**自证**——`T`/`B`/`K` 的键本身就是内容的 blake3，重算一次 hash 即可，零额外格式；② 想调大阈值只能绕开门面直接用 `lsm_tree::MVCC`（会丢掉运行时切换四引擎的能力），或给上游加一个透传 knob → 记进 §14 Q8。
4. **写冲突是真拒绝**：`set`/`delete` 返回 `false`。twosync 全站按**单写者**设计（一个提交线程；`worker_threads = 1–2` 与此一致）。拿到 `false` 意味着**程序有 bug**，必须报错/panic，**不许循环重试**——重试会把一个逻辑错误变成静默的历史错乱。
5. **三家的崩溃安全画像互不等价**：Bitcask 主日志无记录校验和（排除，别当默认）；B+ 树是唯一有原地页写 + 双写缓冲的；LSM 的 SST 有 CRC 但 blob 没有；memory 干脆无 WAL。所以 FR-21 的启动自检是**必须的**，而 `memory` 只允许出现在测试里（`EngineKind::is_production_ok()` 已有这个谓词，直接用它，别自己判断）。
6. **三套回收不许互相咬**：引擎 MVCC 版本回收（自动切片默认 16 Ki 行）+ LSM blob 旁路回收 + twosync 自己的块 refcnt 回收。规则：磁盘释放**只认 twosync 自己的 pending-delete 列表**，绝不依赖引擎 vacuum 的时机；引擎侧唯一诉求是"版本链不许无限涨"。`gc` 子命令要能分别报出这三层各占多少字节。

### 9.7 引入方式（已核实：0.4.0 就在 crates.io 上，不需要 tag 也不需要 rev）

`crates.io` 上 **0.4.0 已发布**（2026-10-15，未 yank，`max_version = 0.4.0`）。所以走**版本号依赖**：tag 这件事直接作废，它只在改用 git 依赖时才有意义。

```toml
storage-engines = "0.4.0"          # 默认：crates.io 快照，可复现、CI 友好
# 只在"同时要改引擎"时临时换成 path，改完必须换回来：
# storage-engines = { path = "../storage-engines" }
```

**path 依赖的真实风险**：它是活目录，crates.io 那份是冻结快照。用 path 期间在引擎里修的 bug，twosync 立刻吃到、但**发布出去的二进制吃不到**——两边行为不一致，§10.4 那张差分表的数字也就对不上发布版本。所以约定：默认版本号依赖，path 仅用于调试引擎，且提交前 `cargo tree -p storage-engines` 必须能一眼看出用的哪一个。

（上游 README 里"crates.io 最新仍是 0.3.0"那句是 0.4.0 发布前写的，已过期，该在 `storage-engines` 侧改掉。）


## 10. 验收标准与测试策略（"拿得出手"就靠这一节）

1. **不变量测试（`proptest`）**：随机生成多 peer 操作序列（add/modify/delete/rename/mkdir/symlink）+ 随机 push/pull 交错。v1 用 2 peer，但**同一套生成器必须能跑出 3 peer 序列**，用来提前验 §4.1 与 union 遍历（不实现网络，只在内存里跑图）。断言：
   - (a) 两 peer `main` head 集合相等 ⇒ 两 peer 检出树等价（排除 §8 明说的不可移植项）；
   - (b) 任意 commit 能从 CAS 完整重建，逐 blob hash 匹配；
   - (c) **只增不减**——任何交错序列都不得出现"某 peer 丢掉了它曾经可达的 commit"（除非人显式 `--take-*`）；
   - (d) GC 后保留集仍满足 (b)，淘汰顺序符合 FR-9，用量不超全局配额，且**跨 project 共享的块不会因单个 project 淘汰而被误删**；
   - (e) collision 判定完备：构造"同路径被两个 head 改动"必被报出；构造"路径无交集的多 head"必不得报 collision；
   - (f) **分页不变量**：100 万条规模下任何扫描的峰值新增分配 ≤ **8 MiB**、且与块大小与文件总数无关，扫描期间 `health().active_txns` 不得长期 > 0（§9.6.1/2）；
   - (g) **二级索引一致性**：任何改 `K.refcnt` 的路径都必须同步改 `k` 索引——用随机序列断言两者永不漂移（这是手工索引最容易烂掉的地方）。
2. **crash injection**：在块写入中途、`fsync` 前后、清单提交前、head 集合更新前分别 `kill -9`；重启后断言：无半文件、无 orphan tmp 存活超 5 s、无"清单引用不存在的块"、不变量 (a)–(g) 仍成立、不重复传输已到位的块。**其中一半用例是在撞引擎的 WAL 恢复路径**（§15.10 明说这件事）。
3. **路径对抗 fuzz**：`../../`、`\\?\`、UNC、`CON`、尾空格/点、超长名、仅大小写不同的两文件、NFC/NFD 混合、含 ANSI 转义 → 全部拒绝且不崩。
4. **与 git 的量化差分**（取代 v0.1 的"与 `rsync -n --delete` 比对"）：同一份 10 GB 语料 + 同一串变更脚本，跑 ①本工具 ②git（`git add -A; commit; push`）③Syncthing 默认结果，比 **传输字节数 / 磁盘占用增长 / 完成耗时 / 误删次数**。本工具必须在"传输字节"和"磁盘占用增长"上明确赢过 git，否则它没有存在理由——这张表是 README 的主图，也是 §1 那三条理由的兑现证明。
5. **真机长跑**：10 GB 混合语料（1 个大稀疏文件 + 10 万个 4 KB 小文件 + 若干二进制资产），跨 Windows↔Ubuntu，随机变更脚本跑 72 h，记录 RSS/CPU/带宽/会话耗时/历史深度/head 数量，**外加 `health().max_chain` 与 `rows_above_one` 两条曲线**（§9.6.2 的复发通道只能靠时间暴露）。
6. **基准（`criterion`）**：全量扫描、脏判定、commit 生成（NFR-6）、块切分+hash、`want/have` 协商、清单 diff、**union 可达性遍历（NFR-7）**、**分页扫描窗口大小调参**。
7. 判据：1/2/3 未全绿，**禁止发任何 release**；且 release 时 `storage-engines` 必须是 §9.7 的版本号依赖，不许残留 `path`（提交前用 `cargo tree -p storage-engines` 核对）。
8. **引擎矩阵**（本项目独有的素材）：同一套 §10.1 不变量 + §10.6 基准在 `lsm_tree` / `bplus_tree` 上各跑一遍，`memory` 上跑全量 proptest（无持久化 = 快一个量级，适合当回归主力）。四引擎在同一真实负载下的 RSS/CPU/p99/磁盘增长，加上 `health()` 读数必须一致这条约束，就是"自研引擎吃自己狗粮"能拿出的最硬的表。

## 11. 里程碑

| 阶段 | 交付 | 可验证判据 |
|---|---|---|
| **M0 骨架** | `cargo init --lib --name twosync`；CLI(`commit/log/diff/show/checkout/restore/status/plan/version`)；**CAS 块仓 + blob/tree/commit 读写**；§9.2 键编码 + 引擎接入（FR-21 自检）；扫描器 + 变更检测 + 自动建 commit；**project 注册表**。**先不联网** | 扫 10 GB 建库给出实测耗时/RSS/CPU（NFR-1/2 变真数字）；**分页扫描峰值 ≤ 8 MiB 且与块大小/文件数无关（release 实测）**；改 1 MB 后 `commit` 成本符合 NFR-6；`restore --at` 取回任意历史版本；`H` 前缀能表达多 head、`K`/`k` 双写不漂移 |
| **M1 单向推送** | SSH 通道 + `HeadsState/WantHave/PushBatch/PushCommit/UnionAccept` + **多 project 复用一条连接** + FR-10 熔断 + FR-9 配额 GC + FR-13 原子写 | 本地删 30% 文件 → 对端拒绝该 commit 并提示；10 GB 文件改 1 MB → 传输 ≤ 8 MB；crash injection 通过；§10.4 差分表首版数字；20 project 一轮只 1 次认证 |
| **M2 常驻（仍 1+1）** | watch + debounce + 周期兜底 → 自动 commit + 自动 push/pull；**拨号方向对称**（任一端可发起，§16.3）；断点续传；限速（含 GC 与 vacuum，总预算不叠加）；`status` 指标含引擎健康度；systemd/计划任务 | 拔网重连自动续传；稳态 CPU ≤ 10% 单核实测；**72 h 内 `max_chain` 不单调上升**；Windows 休眠后由对端拉平且不误提交 |
| **M3 双向 + 多 head** | 两 peer 独立提交；§7 五行表全实现；union 判定；collision 报告 + 四条出路；§7.2 deferred | 不变量 (a)(b)(c)(e) 通过；§7 表逐行有单测；collision 必须停而非猜（人工验证） |
| **M4 硬化** | CDC 变长分块、稀疏重建、批传输调优、**第二层命令（branch/tag/reset/revert/stash/cherry-pick/blame/bisect）**、§10.8 引擎矩阵、72 h 长跑、README/发布 | §10.4 差分表与基准数据入库；指标写进 README；依赖是 crates.io 版本号、不残留 path（§9.7） |
| **M5 多端（暂不承诺）** | 真 3+ peer：部分连通路由、NAT 穿透或自建中继、commit 签名 | 需先重开范围评审；**不在 v1 交付路径上** |

M0–M1 **不需要**回答 §14 任何问题就能开工。

## 12. 技术选型

| 用途 | 选型 | 理由 / 备选 |
|---|---|---|
| **存储基座** | `storage-engines = "0.4.0"`（crates.io，见 §9.7） | 取代 v0.2 的 rusqlite。门面 `KvEngine`/`KvTxn` 对象安全、`Send + Sync`、快照隔离、`prefix_scan`/`seek`/`key_count`/`incr`/`health`/`vacuum` 齐全，§9.2 的访问路径全覆盖；依赖只 5 个、`fs4` 提供 fsync。**代价见 §9.6** |
| 默认引擎 | `lsm_tree` | 写密集的清单/元数据 + 大量前缀扫，LSM 天然对口；SST 有 CRC。对照：`bplus_tree`（唯一有 dblwr，崩溃安全最好，读放大低）、`memory`（仅测试） |
| 引擎参数 | `EngineConfig` 只透传 B+ 树 `order`/`max_cache_pages`；LSM 的 `blob_threshold` **当前不可透传** | §9.6.3 + §14 Q8 |
| CLI | `clap` 4.6 (derive) | 事实标准；git 手感靠 revspec 自定义 parser（FR-20），不靠框架 |
| 异步 | `tokio`，`worker_threads` 默认 1（单写者，§9.6.4） | 备选 `smol` |
| 文件事件 | `notify` 8.2 + 自研 debounce/inotify-overflow | 事件不可靠是已知事实，必须兜底扫描；现在扫描出错的后果轻多了 |
| 哈希 | `blake3` | 单核吞吐最优。**落盘 CAS 键用满 32 B**；20 B 截断只允许出现在线上传输与比对提示，不许当主键 |
| CDC | `fastcdc`（Gear） | M4 再用。换分块参数 = 块身份全变，`K` 值里的 `chunk_param` 现在就要留命名空间，M4 补不了 |
| 序列化 | `postcard` | 引擎的 value 与线上帧都用它；比 bincode 省、无版本耦合坑，在 `Hello` 里协商 |
| 排除规则 | `ignore` crate | gitignore 语义，零学习成本 |
| 传输 | `russh`（exec 复用系统 sshd） | 备选 `ssh2`（libssh2，同步 API 要包一层） |
| 日志 | `tracing` + `tracing-subscriber`（JSON file + 轮转） | 引擎也用 `log`，接 `tracing-subscriber` 的 compat 层 |
| 限速 | 自研令牌桶（`tokio::time`），CPU/带宽各一桶，**全 project 共用，GC/扫描/引擎 vacuum 都挂上去** | 必须自研：要能随负载退避，且 N 个 project 不能各自一桶把单核吃满 |
| 测试 | `proptest`、`criterion`、`cargo-fuzz`（路径层） | §10 的地基 |
| 工程 | `cargo xtask` 或 `justfile` | 不依赖托管 CI 也能一键跑全套验证；§10.8 矩阵与 §9.7 依赖切换都放这里 |

## 13. 命名与数据目录

- 目录/crate：`twosync`。P2P + 多 project 之后这名字更不准确了（既不是"两个"，也不只是 "sync"），但目录里还没有一行代码，改名成本仍是 0。候选：`p2git`/`filevcs`/`crossbox`/`manyroad`。
- 数据目录：
  ```
  <root>/.twosync/                    # 站点级
    projects.toml                     # 项目注册表：name -> 本地路径/对端路径/排除/保留策略
    objects/<hh>/<hex>                # 站点级共享 CAS（全局配额落在这里；字节只在这儿）
    db/lsm_tree/                      # storage-engines 的数据目录（换引擎不自动带数据）
    peers.json                        # TOFU 指纹表（也写进引擎 P 前缀，此处为可读副本）
    journal/  log/
  <project_root>/.twosync-project/     # 每项目：heads 快照与配置覆盖（在工作树里，必须强制排除）
  ```
  注意 `db/<engine>/` 是**引擎自己的格式**，twosync 不许直接读它的文件；一切访问过 `KvEngine`。

## 14. 待决问题（回答后本文档升 v0.5 并冻结 M0 范围）

1. **自动 commit 的粒度**：每次静默期结束就提交（历史很碎）/ 每 N 分钟合并成一个 / 只有显著变更才提交？→ 决定 10 GB 工程的历史深度与 `log` 可读性，也决定 FR-15 的唤醒节奏。**注意它同时决定引擎的版本链涨速**（§9.6.2）——碎 commit = 热点键 `H`/`K` 高频更新。
2. commit message 谁写：自动生成 `sync: 12 add / 3 mod / 1 del`，还是允许给某些路径打标签？
3. 2–10 GB 的构成：文件数量级、最大单文件、小文件占比 → 决定块参数（4 MiB 是否合适）、NFR-5 配额、`want/have` 用 bloom 还是全清单、**以及 §9.6.1 的分页窗口大小**。
4. 历史里可能留有旧版本密钥/客户数据，**需要静态加密吗**（§9.4.6）。版本化让泄漏面变大，这条比 v0.1 更该认真答。
5. 除 `.git/` 外必须内置排除什么（venv/`node_modules`/数据库热文件/锁文件/正在运行的二进制）？→ 进 FR-12 强制清单。特别地：**`storage-engines` 自己的 `db/<engine>/` 与 WAL 若落在被同步的 root 里，会产生物理不一致的版本历史**——你自己的 KV 文件同理。
6. 那台 1 核 6 G 上是否有对延迟敏感的常驻进程（策略/交易类）？→ 决定 FR-15 退避与 NFR-3 是否成为 M1 阻塞项。**若 twosync 与 trading-arb 同机，还要确认两者不共享同一个引擎进程**（twosync 的 GC/vacuum 是 IO 突发源）。
7. **project 数量与总规模**：打算同步几个 project、合计多少 GB？→ 决定共享 CAS 能省多少、"站点级 20% 全局配额"够不够分（20 个 project 抢同一池要不要按 project 保底）。
8. **要不要给上游加 knob**：`EngineConfig` 目前不透传 LSM 的 `blob_threshold`（默认 4 KiB），而目录清单很容易超过它、落到无校验和的裸 blob（§9.6.3）。三个选项：① 给门面加 LSM 参数透传（改 `storage-engines`）；② twosync 直用 `lsm_tree::MVCC`、放弃运行时切换与 §10.8 矩阵；③ 保持现状、靠内容 hash 自证兜住。**我倾向 ①+③ 同时做**。
9. **引擎改版怎么跟**：`storage-engines` 之后修 bug 会发 0.4.1/0.5.0，twosync 是宽版本自动吃升级，还是锁 `=0.4.0` 手动升？→ 建议锁补丁级，因为 §9.6 那六条约束是照 0.4.0 的行为写的，引擎行为变了要重新核一遍。

（v0.1 的 Q1「服务器侧写入是程序产出还是人编辑」已不再是阻塞项——版本库 + 多 head 天生支持两端都写。）

## 15. 已知限制（README 必须原样列出）

1. **不兼容 git 对象格式与 git 协议**，所以真 git 命令读不了本库；手感照 git 自建（§16.2），语义相近但**不是**同一实现。原因见 §1 第 1、2 条与 §17。
2. `branch`/`tag` 只是本地命名，v1 **不参与同步**；跨站协作仍需真 git。
3. collision 只能人选一边，不合并内容。
4. 硬链接不识别（§8）。
5. 不转 CRLF/LF，不做文本友好化。
6. v1 实现只有 1+1 个 peer；**多端、NAT 穿透、部分连通路由不做**（§2.3、M5）。
7. `commits.peer_id` 不签名（§9.5.2），历史来源可被本机伪造；对等身份靠 TOFU 指纹表。
8. Windows 侧 symlink 需开发者模式或特权，否则按 `skip/copy-file` 降级。
9. 配额内历史会被 GC——**它不是备份**，需要长期留存请另开 bare git 或对象存储。
10. **存储基座是一个自称「教学/学习性质、不是生产级数据库」的库**（`storage-engines` 0.4.0，已在 crates.io，见 §9.7）。因此 twosync 的正确性论证**不引用它的测试数量当背书**，而是靠 §10.2 的 crash injection 直接去撞它的 WAL 恢复路径 + §9.6 的六条约束自检。撞出引擎 bug 是预期的狗粮结果，届时修在引擎侧、twosync 不叠第二层补丁。

## 16. 附录：多项目布局与命令面（v0.3 新增）

### 16.1 多项目布局

- 一个 peer 的 `root` 下 N 个 project，每个 project 一份**独立版本库**（独立 heads/commits/trees），但**共享站点级 CAS**（§13 的 `objects/`）与**共享同一个引擎实例**（按 `project_id` 键前缀分区，§9.2）。
- **为什么先共享**：共享池拆成独立库只需按 `project_id` 前缀过滤；独立库合并成共享要重算跨 project refcnt 并做数据迁移。既然暂时不定，就选以后省事的那头。代价如实写在 §9.3——refcnt 跨 project，`project remove` 只能 decrement。
- 配额是全局的（FR-9/NFR-5）：磁盘是同一块盘，"每 project 20%"会在项目变多时静默超预算。
- 会话**跨 project 一次跑完**：一条 SSH 连接、一次认证、N 个 project 的状态机交错推进（FR-6/FR-19），限速桶全站共用（FR-15）。
- `project remove` 默认只删注册表条目 + decrement refcnt，**不删工作树、不删历史**；`--purge` 才连带回收 CAS 空间。

### 16.2 命令面分三层（这是"git 命令我都要有"的诚实拆解）

| 层 | 命令 | 代价 | 归属 |
|---|---|---|---|
| 一：读历史（有 DAG 就白送） | `status log diff show checkout restore ls-files gc` | 低 | M0–M1 |
| 二：图上重写/重放 | `branch tag reset revert stash cherry-pick commit --amend blame bisect project use` | 中。`branch` = ref 命名空间（`H` 前缀里换 `ref_name`）；`blame/bisect` = DAG 遍历；`stash` = 一个临时 commit，近乎免费 | M3–M4 |
| 三：内容三方合并 | `merge`（有同路径交集时）、`rebase -i` 的冲突解决 | **另一个量级的工程，且与 §1 的分工矛盾**（跨 OS 编辑同一份源码本该交给真 git）→ 不提供，给明确报错 | 不做 |

- **revspec 才是手感迁移的真正来源**，值得单独实现（FR-20）：`HEAD`、`<hash>`、`HEAD~3`、`HEAD^`、`branch:path`，配 `--stat/--hard/--patch`。工作量比 `merge` 小两个数量级，体验差别却是决定性的。
- `push`/`pull` 在 P2P 下是**同一操作的两侧**（§16.3），不是 client/server 动词；v1 的 `pull` 语义 = "把对端 commit 收进来并检出"。

### 16.3 对等（P2P）具体改了什么

1. **发起权对称**：两端都可定时拨向对端。拨号方向由 `peers.dial_preference` + "谁在线谁先动"决定；一轮会话无论谁发起，动作都是"双向 union + 按需双向检出"。FR-17 的判据（Windows 休眠后由对端拉平）落在这儿。
2. **熔断在两侧都生效**：FR-10 的检查发生在**接收侧**，所以谁发起都不影响安全性——这是版本库模型对 P2P 最友好的地方。
3. **认证要双向**：v0.2 靠 host key 单方面 pin；现在加 TOFU 指纹表（§9.5.1）。
4. **不变的东西**：无中心服务器、无新增监听端口（仍走 SSH）、资源预算不叠加、内容寻址天然幂等。

### 16.4 明确推迟的网络活儿（避免污染 M0）

- **NAT 穿透 / 中继**：两台都在家用路由器后面时无法互拨。hole-punching（UDP + STUN 类发现）或自建中继 hop 是**一整个子系统**，且与"不开新端口"的 FR-4 有张力。→ M5，且需重开范围评审。
- **部分连通收敛**：A-B 直连、B-C 直连、A-C 不通时，历史经 B 传播且不变形。union 语义让这件事**原理上成立**（B 是 C 的可达性来源），但证明与测试需要 3-peer 生成器，§10.1 已提前留口子。→ M5。

## 17. 修订记录

### v0.4 补记（2026-10-16，实测更正 + CAS 落地）

**更正 `51b68e9` 提交信息里的数字**：那条写"点查 4.3–36 ms/次"，来源是 **debug 构建**，作废。
release 重测（Windows 10 开发机，5002 条目 / 32 MiB，同库同码）：

| 操作 | release | debug |
|---|---|---|
| 5002 次点查 `get` | 4.574 s ⇒ **0.86 ms/次** | 21.7–183 s ⇒ 4.3–36 ms/次（同规模 8 倍抖动） |
| 5002 次「读 + hash + 写」 | 4.665 s ⇒ 0.93 ms/条 | 71.3 s |
| 纯文件系统遍历 5002 项 | 0.118 s | — |
| 进程启动 + 打开引擎 | 0.099 s | — |

归因结论：**卡在引擎的点查路径**（1 万行、60 B 值，0.86 ms 仍比合理值高约两个数量级）。
已排除四项：版本链（`max_chain=3`）、活跃事务（`active_txns=0`）、磁盘遍历（0.118 s）、
blob 旁路（值只有 60 B）。**"随行数线性变贵"当时是假设、现在仍未证**——需要 1k/5k/20k/50k
行的曲线，已交回 `storage-engines` 侧。twosync 这边无论结论如何都要改：脏判定从
"每文件一次 `get`"改成按批 `multi_get`。

**CAS 落地（`src/store/cas.rs`）的两个决定**：
1. **两遍流式（方案 A）**：第一遍只算块 hash 不写字节，第二遍按清单只搬缺失的块。代价 =
   1 次全读 + 1 次脏块读；换来"改 1 MB 只写一块（4 MiB）"以及**常驻内存与块大小无关**。
   放弃的 B（边读边写 tmp、事后删）会把 10 GB 文件全量写一遍再删掉，等于把省下的写赔回去。
2. **同一文件内的重复块也只存一份**（内容寻址的必然结果）：8 MiB 的重复 pattern 只占 4 MiB，
   而清单仍完整描述 8 MiB。这是实测出来的，不是设计时想到的。

**这一轮被新用例抓到的真 bug**：`copy_out` 早期只传一个 hasher，拿**整文件累积值**去比**单块
hash**——第一块恰好相等所以全套用例都绿，直到加进"同一文件内两个相同块"这条用例才炸。
教训已写进函数注释：**校验用的哈希器必须与被校验的范围一一对应，否则"通过"只是巧合。**

**同步修订**：§6 引言新增"性能数字必须注明 profile + 硬件 + 数据形状"的规矩；
NFR-1 / FR-3 / §9.6.1 / §10.1(f) 四处的"峰值 ≤ 4 MB"统一改为"≤ 8 MiB 且与块大小、
文件总数无关"——原来的 4 MB 与 FR-5 的 4 MiB 块自相矛盾，两遍流式才让两者同时成立。

### v0.4 补记二（2026-10-16 深夜，`commit` 流水线落地）

**新增一条扫描↔commit 的接口规矩（§8 / §9.2 的 `W` 行已同步）**：`W` 表只增不删（§8 禁止扫描判删除），
所以光读 `W` **永远看不出谁被删了**。现在扫描给**本轮看见的每一行**盖 `scanned_at`（含内容没变、
只走粗筛命中的，也含读失败 deferred 的——后者不续期就会把 §7.2 伪装成删除），`commit` 只认
`max(scanned_at)` 那一批，其余即删除。代价是实测出来的：稳态扫描也要重写全部行 ⇒
`doctor` 的 `rows_above_one` = **2002**（2004 条目的库，每键多一个版本行）。回收靠引擎 vacuum
（`active_txns=0` 时才能跑），所以 **`rows_above_one` 从"诊断项"升格为"必须盯的量表"**。

**FR-10 加了绝对下限 20 条**（口径变更，见 §2 该行）：纯 20% 会让"2 个文件删 1 个"这种日常操作
永久卡死小仓库。用例 `single_delete_in_a_small_project_is_not_mass_delete` 钉的就是这一条。
本地 `commit` 与接收侧过同一道闸，且闸在 `put_commit` 之前、diff 之后——删除只有一个定义。

**这一轮被新用例抓到的真 bug（本项目第一个"只有查树本身才能发现"的）**：`collect_children` 用
`split_opt` 取父目录，而**顶层目录名里没有 `/`** ⇒ 返回 `None` 被 `continue`，于是整个顶层目录
连同它下面的一切**静默消失**在历史里。所有其他信号都是正常的：块照落、`entries` 照是 4、
`changes.add` 只是变小而没人报错。改用 `split`（无 `/` ⇒ 父为 `""`）后由
`nested_directories_survive_into_history` 钉住。教训与 `copy_out` 那条同源：**只要有一层"跳过"
是静默的，用例就必须直接查那一层的产物。**

**实测（release，Windows 10 开发机，2004 条目 / 8.0 MiB，LSM）**：

| 操作 | 耗时 | 备注 |
|---|---|---|
| `scan` 首轮（2004 条全 hash） | 0.392 s | 20.4 MiB/s，peak heap 2.5 MiB |
| `commit` 首轮（2002 blob / 2003 块） | **12.075 s** | peak heap 5.3 MiB |
| `commit` 无改动 | 0.732 s | 返回"无改动"，不产生空提交、head 不动 |
| `commit` 改 1 + 删 3 | 0.733 s | `+0 ~1 -3`，`entries 2001 / 库里 W 行 2004` |

固定开销 ≈ 0.73 s（分页快照 + 建树 + diff），首轮多出的 **11.3 s 花在"每块一次 fsync+rename"
与"每块三次 KV 写（`B`/`K`/`k`）"这两处**，**还没拆开**——下一步归因先分这两块，再决定要不要
像 git 的 packfile 那样把小文件打包。按 §6 的规矩：这两个候选在给出数字之前不许当结论写。

**这一轮明确没做的三件**（不是遗漏，是排序）：
1. `MAX_ENTRIES = 200_000` 是**权宜**：条目表是 O(文件数)，2004 条就 5.3 MiB ⇒ 20 万条顶穿 NFR-1。
   超限现在是**报错拒绝**而不是 OOM（NFR-3 红线：OOM 会拖下同机的策略进程）。
   解法是流式栈式聚合，届时这个上限应该消失而不是调大。
2. `restore` / `checkout` 还没有命令 ⇒ "历史能取回字节"目前只有用例
   `committed_bytes_roundtrip_through_cas` 证明，**CLI 还做不到**。
3. `stats.rename` 恒 0（改名按 add+delete 记），FR-17 的改名检测没做。

### v0.4 补记三（2026-10-16 深夜，`log` / `show` 落地 = 历史第一次可见）

**新增 `src/store/history.rs`（只读层）**：`walk`（多起点、时间倒序、共同祖先只走一次）、
`resolve`（按路径逐层走 tree）、`resolve_rev`（`HEAD`/ref 名/十六进制前缀 → commit）。
单独成一层而不是塞进 `build`，是因为读路径必须在 GC 之后、在对端只推了半个版本树时依然能用——
**"读的那条路恰好是写的某条路的副产品"就是 §10 不变量 (b) 最常见的破法**。

**新错误 `NoSuchPath`**：「那个版本里本来就没有这条路径」与 `MissingObject`「库该有却没有」
是两件事，混起来就会把"用户打错路径"报成"磁盘坏了"。中间层不是目录也算"不存在"（git 同口径）。

**这一轮用例抓到的两个 bug**：
1. `resolve_rev` 取 hash 尾巴时用了**整个 pattern 的长度**当偏移（含用户输入的那段 hash），
   于是"用完整 64 字符 hash 查自己的 commit"反而失败。gc/push 的输入形态正是完整 hash，
   这个 bug 会直接堵死 M1。由 `rev_resolution_accepts_head_and_full_hash_but_rejects_garbage` 抓到。
2. 测"同时间戳靠 hash 破平"时，我第一版拿**同一棵树**提了两次 ⇒ 内容寻址下那是同一个 commit，
   断言在比一个空转的相等。改成两棵不同的树并加 `assert_ne!(a, b)` 才算真测到。
   教训：**内容寻址库里"提两次"不等于"两个对象"，写测多对象的用例必须先证明它们真的不同。**

**单位陷阱钉桩**：`fmt_ts` 收的是**纳秒**（`Commit.ts` 就是 ns），传秒进去不报错、安静落在
1970 年。测试里我自己就写错过一次（把 epoch 秒当 ns 传），现在那条错误输入被钉成期望值。

**这一轮暴露、但不在本轮修的两个缺口**（都要改磁盘格式或跨平台口径，需拍板）：
1. `TreeEntry` 对符号链接只存**目标的 hash**，没存目标文本（`W` 里有 `symlink_target`，
   建树时丢了）⇒ **历史无法重建符号链接**，`checkout` 会做出一个悬空链接。修它要动 tree 编码
   = 所有已存 hash 作废，所以必须现在定：要么把目标文本放进条目，要么给它一个特殊 blob。
   **【当晚已解决，见下面"补记四"：选了第三条路——目标文本当小 blob 落 CAS，不动编码。】**
2. Windows 侧 `mode` 恒 0（`mode_of` 非 unix 直接返回 0），所以 §8"只同步 owner 可执行位"
   目前只有 Linux 那半边成立。`show` 的输出里 `mode 0` 就是这个，不是显示 bug。

### v0.4 补记四（2026-10-16 深夜，`restore` 落地 = M0 验收项「取回任意历史版本」达成）

**符号链接那个缺口用第三条路堵上了，且不动编码**：目标文本本身当**一个小 blob** 落 CAS
（git 就是这个做法）。`TreeEntry` 一个字节没改 ⇒ 已存历史全部继续有效。链路已在真 Windows
上跑通：`scan` → `commit`（blob 1 新）→ `show HEAD link.md` 直接答出
`链接目标 = .../docs/guide.md (63 B / 1 块)` → `restore` 建出真链接。
`restore::link_target` 有 4 KiB 硬上限：它一次物化整段，只够路径级别的小对象，文件走流式那条路。

**`restore` 的默认是"不覆盖"**，`--force` 也只跳过"目标存在"这一步，**不跳过** §7.3 的门。
门拦三种情况，每种都要能自己解释：① 没 `--force` → `LocalExists`；② **磁盘比最后一次扫描新**
（kind/size/mtime 与 `W` 不符）→ 拒绝，因为扫描器从没见过当前内容；③ 扫过了但那版没 commit
（`B` 不在库里，或块不全在）→ 拒绝，提示先 commit。
第 ② 条是写第一版时漏的：只看 `W` 的话，"扫过之后又改过"的内容会被判成"已经在历史里"从而**放行覆盖**，
恰好是铁律要拦的那件事。检查用的尺子直接 import `worktree::logical_size` / `mtime_of`，
不在这里另算一遍（§17 补记二那个目录大小 bug 的根因就是两处定义）。
`missing()` 逐块 `stat` 而不是逐块重算 hash——重算属于 `verify`/`gc --fsck`，放进一次 restore
会把 10 GB 文件变成分钟级。

**实测（release，Windows）**：`restore --to` 取回 20 000 B / 5 块字节全等；`--force` 覆盖回 v1
成功且 head 不动；改 3 个文件不 scan → 拒绝且本地内容还在；100 条里删 25 条 →
`删除熔断：本次将删 25 条 / 0 MiB（已知 100 条）` 退出码 1，加 `--allow-mass-delete` 后 `+0 ~0 -25` 通过。

**这一轮被用例抓到的三个错，全是我自己写的**（产品代码只有一处，另两处是测试与文案）：
1. `restore` 先读 blob 再分派 kind ⇒ 恢复目录时报成"blob 对象缺失"（目录的 hash 是 tree），
   把一句"这活归 `checkout`"的实话伪装成磁盘损坏。改成每个分支自己读自己的 blob。
2. 测试辅助 `epoch_of` 忘了标目标类型：`postcard::from_bytes` 的**目标类型从返回位置推导**，
   函数返回 `i64`，于是它拿 `i64` 去解 `WtEntry` 的字节，而 postcard 不介意尾部多余字节 ⇒
   安静解出 0，测试拿着假 epoch 以为产品有 bug。**泛型反序列化函数的返回类型必须显式标注。**
3. `show` 里"还不能从历史重建符号链接"这句话在缺口补上之后就成了假话 —— 顺手改成把目标读出来。
   **改完行为要回头搜一遍自己写过的说明文字**，否则文档/注释/输出会替用户保留旧世界观。

**M0 剩余**：`status` / `diff` / `ls-files` / `mv` / `checkout`（目录级恢复）/ `config`，
以及把 `MAX_ENTRIES` 换成流式栈式聚合。Windows 的 `mode` 恒 0 那条缺口仍在（见补记三第 2 条）。

### v0.4 补记五（2026-10-16 深夜，`status` / `diff` 落地 = 只读路径与写路径正式分家）

**`build::plan()`：只读地算出"现在 commit 会长什么样、与 HEAD 差在哪"，一个字节都不写。**
`status` 与 `diff --at <rev>` 都走它。实测（2005 条目的站点）：跑 `status` + 两次 `diff`，
引擎 `rows` **12027 → 12027，差 0**。这条数字是"只读"承诺的唯一证据，长跑期间要复测。

**组装阶段用 `Sink` trait 分两种落盘策略**（`PlanSink` 只算 hash / `WriteSink` 真落块+写对象），
而不是给 `assemble` 加一个 `write: bool`：布尔参数会一路传到调用链上，谁传反了就静默产出半成品。
更重要的是**树组装逻辑只留一份**——顶层目录静默丢失那个 bug 就住在这段里，
给 `plan` 复制一份等于再埋一个同根因的坑，而且第二份没人会去测（用例
`plan_writes_nothing_and_matches_the_commit_that_follows` 直接断言两次的根 tree 与样本全等）。

**`plan` 第一版踩的坑值得记下来**：它算 hash 但不写 tree，于是 `diff` 递归时要读新树的
某一层 ⇒ `MissingObject{tree}`。**"只读路径复用写路径的产物"这件事是双向的**：`history.rs`
的模块头说不许让读依赖写，这里撞到的是另一半——读路径自己产出的中间结果也不能假装在库里。
解法是 `TreeStore`：`PlanSink` 边算边把清单留在内存（`build` 留空，因为它已经写进库了），
`diff` 先查内存再查库。存进去的清单**排序后再放**，与 `read_tree` 的形状一致，
免得 diff 因为顺序差异在两条路上给出不同答案。

**变更样本：计数全量，列表有界**（`SAMPLE_MAX = 30`，超出只报 `sample_more`）。
样本给的是**完整路径**（`data/sub/deep.txt`），因为 `status` 列出来的东西下一步就要喂给
`restore`；只给文件名的话用户不知道该恢复哪个。实测 `diff --at <首个 commit>` 正确给出
`+1 ~1 -5` 并列出三层深的完整路径。

**顺带**：`ChangeSet` 现在带 `sample`/`sample_more`，`build::diff` 转为 `pub`（任意两棵树可比）。
M0 剩余：`ls-files` / `mv` / `checkout`（目录级恢复）/ `config` / `restore` 的 `--at HEAD~n` 语法，
以及把 `MAX_ENTRIES` 换成流式栈式聚合。Windows `mode` 恒 0 那条仍在。

### v0.4 补记六（2026-10-17 凌晨，`ls-files` / `checkout` 落地）

**`ls-files`**：`history::walk_tree` 深度优先、**边遍历边回调**，一百万个文件也不会先装进
`Vec`（§9.6.1）。这里刻意用递归而不是显式栈：要拿到字典序前序，显式栈得把未读的兄弟一起压进去，
内存反而回到 O(条目数)；而递归深度被 `PATH_MAX` 钉在几百以内。目录也占一条（空目录只有靠它表达）。

**`checkout`**：两趟遍历，第一趟只记账并过 FR-10 熔断，第二趟才落地。分类逻辑**只有一份**
（`Job::on`）——早先"只数"和"真干"是两个函数，`skipped` 的判定条件写了两遍，那是下一个静默
偏差的产地。落地动作全部复用 `restore` 的那套（`check_local_is_recoverable` / `link_target` /
`make_symlink` / `apply_owner_exec`），**不给 checkout 另写一份 §7.3 检查**。
`--prefix` 只接受目录：单文件是 `restore` 的活，两条命令的边界不许糊。

**本轮最重要的判断修正：把"脏工作树前置闸"拿掉。** 第一版照 git 的样子写"工作树相对 HEAD
有未提交改动就拒，`--force` 才放行"，用例立刻红了两次，原因相同：**`checkout` 只改工作树、
不动 head（v1 没有分支），所以它每执行一次就把工作树变成相对 HEAD 永久脏**——那道闸会在第一次
apply 之后把命令彻底废掉。真正的危险从来不是"W 与 HEAD 不一致"，而是"**磁盘与 W 不一致**"
（扫描器没见过的内容被覆盖），而那是逐路径的性质，§7.3 那道门本来就按 kind/size/mtime 逐字查。
现在：脏不脏只在报告里**告知**（`Report.dirty`，与 `status` 同口径），不再当闸；`--force`
这个开关随之删掉——**不留一个已经什么都不做的旗标**。
补的一条：`Add` 落到一个**已存在**的路径上（未跟踪文件挡在那儿）也要过 §7.3，否则
`read_blob` 的 `rename` 就是一次静默删除。

**结构收敛**：`walk_pair` 成为两棵树配对遍历的唯一一份（`diff` 与 `checkout` 共用），删除目录
改成**后序**回调，这样 `checkout` 能在同一趟流式遍历里 `remove_dir`（不用另存一份待删目录列表，
那会是 O(目录数) 内存）。`TreeStore` 从 `build` 移到 `history`——它表达的是"我要看一棵树"，
放读层让写层来借；放写层就成了"读依赖写"，正是 `history` 模块头禁止的方向。

**一个 Rust 坑值得记**：回调类型别名 `type Visit<'a> = dyn FnMut(&'a str, ...) -> Result<()>`
把生命周期标在**参数**上，会逼闭包成为 `'static`，借用局部计数器直接编译不过；标在
`+ 'a` 上、参数写省略形式（编译器给 `for<'b>` 高阶约束）才是对的。

**实测（release，Windows）**：`ls-files` 3 条含目录本身；`checkout` 只报 = 建 1 / 覆盖 1 / 删 1，
耗时 0.000 s 且引擎 `rows` 一字不增；`--apply` 0.007 s 后 `a.txt` 回到 v1、`c.txt` 消失、
`head` 仍在 v2；紧接着第二次 `checkout` 同一版本报"无"（旧那道脏闸就是在这儿把命令卡死的）；
未跟踪文件 `d/keepme.txt` 在整趟操作后原样还在。

**已知不做**：类型互换（文件 ↔ 目录）记 `skipped` 并原样保留；目录 mode 变化不落地
（Windows 侧 mode 恒 0，见补记三第 2 条）。M0 剩余：`mv` / `config` / `--at HEAD~n` 相对写法 /
`status` 的未跟踪文件视图（需要扫描器给"见过但没记录"留痕迹），以及流式栈式聚合替换
`MAX_ENTRIES`。

### v0.3 → v0.4（2026-10-15，同日）

依据用户指令："我不想用 rusqlite、libsqlite3-sys 这两个，我想用 `D:\develop\workspace\rust_workspace\storage-engines` 这个发出的 crate"。

- **§9.2 整节重写**：SQLite DDL → **KV 键编码表**（11 个标签前缀，BE 编码保证字节序=逻辑序，`postcard` 值）。删 `refs`/`commit_parents`/`tree_entries`/`sites` 四张表；新增 `k`（按 refcnt 排序的二级索引）——SQLite 时代它是 `CREATE INDEX`，现在是手工编码，**这是换 KV 的真实代价**，并因此加了不变量 (g)"主表与二级索引不许漂移"。
- **不用列族/多桶**（原 v0.3 建议的 `project_id` 前缀方案得到确认）：门面 `KvEngine` 完全没暴露 LSM 的 `cf.rs`，用了就丢运行时切换四引擎这个卖点。
- **新增 §9.6 引擎约束六条**（读门面源码所得，非猜测）：① 扫描物化 `Vec` → 必须分页，否则 100 万条 ≈ 150–250 MB 峰值，**顶穿 NFR-1**；② `active_txns>0` 让 vacuum 整轮跳过 → 事务生命周期 ≤ 单阶段，否则 0.4.0 刚修的那类"随 uptime 恶化"通道复活；③ LSM `blob_threshold` 4 KiB + blob 无校验和 → 内容 hash 自证 + §14 Q8；④ 写冲突是真拒绝 → 单写者，拿到 `false` 当 bug 报、不重试；⑤ 三家画像互不等价 → 新增 FR-21 启动自检；⑥ 三套回收（引擎版本/blob/块 refcnt）不许互相依赖。
- **§9.7 引入方式（同日核实后改写）**：`crates.io` 上 0.4.0 已发布（2026-10-15，未 yank，`max_version = 0.4.0`）⇒ 用**版本号依赖**，**不需要 tag、也不需要锁 rev**（tag 只在走 git 依赖时才有意义）。`path` 仅用于调试引擎，并记清「活目录 vs 冻结快照」会让发布产物与本地行为不一致、§10.4 数字对不上发布版本。§10.7 判据相应改为「不许残留 path」。上游 README 那句「crates.io 最新仍是 0.3.0」是发布前写的，已过期。
- **新增**：FR-16/FR-15 加引擎健康度与 vacuum 限速；NFR-1 加"扫描峰值 ≤ 4 MB"；NFR-5 加"引擎自身占多少字节"；NFR-8 加"`health()` 是 O(物理行数) 不许进热路径"；§8 加"文件锁/单实例"一行；§10.1(f)(g) 两条不变量；§10.5 长跑加 `max_chain` 曲线；**§10.8 引擎矩阵**（四引擎同负载对比，取代 v0.3 里"日后想换自研引擎"那句）；§15.10 已知限制。
- **§12 选型**：`rusqlite` 行删除，换成「存储基座 / 默认引擎 / 引擎参数」三行；`tokio worker_threads` 默认从 1–2 收到 1（单写者）。初始化时实测解析到的版本：`storage-engines 0.4.0`、`blake3 1.8.7`、`postcard 1.1.3`、`clap 4.6.7`、`tokio 1.53.3`、`notify 8.2.0`（旧文档写的 notify 6 已更正）、`ignore 0.4.33`、`thiserror 2.0.21`、`tracing 0.1.44`；dev 依赖 `proptest 1.12.0`、`tempfile 3.27.0`。
- **§14**：新增 Q8（要不要给上游加 `blob_threshold` 透传）、Q9（打 tag / 发布策略）；Q1 补"碎 commit 会加速引擎版本链"；Q5 把"SQLite WAL"改成"`storage-engines` 自己的 `db/<engine>/` 与 WAL"；Q6 补"与 trading-arb 不共享引擎进程"。
- **§2**：加第 10 条非目标"不自研第五个存储引擎"。
- 未改动：§4 五承诺、§7 五行 union 表、§8 其余行、§9.1/§9.4/§9.5、§11 里程碑结构（只在 M0/M2/M4 判据里加了内存与链长断言）。

### v0.2 → v0.3（2026-10-15，同日）

依据用户表述："我的想法你明白吗？我的是 p2p 的想法"，以及"git 的命令和操作我都要有，我用我自己的命令进行项目切换"。

- **模型升级**：单 `main` ref + 快进 → **head 集合 + 可达并集（union）**。§4 新增"只增不减不变量"与"单 ref 是特例"；§7 四行表 → 五行表，`divergence` 术语换成 `collision`，并明确"多 head 是正常状态、不是错误"；§7.4 记录为什么这是格式层面的前提（三端 + 快进 = 无解）。
- **新增 §16 附录**：多项目布局（站点级共享 CAS + 全局配额 + 一连接多 project 复用）、三层命令表（含"第三层不做"的理由）、P2P 具体改动清单、明确推迟的 NAT/多端网络子系统。
- **新增需求**：FR-19 项目管理、FR-20 revspec；§9.5 信任模型（TOFU 指纹表、`peer_id` 不签名、对端不自动建目录）。FR-8 命令集扩成两层；FR-17 加"任一端可发起"；FR-15 加"预算不随 project 叠加"。
- **schema 改动**（当时）：`refs(name PK, commit_hash)` → heads 一对多；`commits.parent_hash` → 多亲；`sites` → `peers`；各表加 `project_id`。v0.4 已把这套关系表形态换成 §9.2 的键编码。
- **§2 非目标改写**：第 2 条"只支持两站"松成"实现 1+1、协议与存储按多站点设计"；新增"不做 NAT 穿透 / mesh 路由 / 中心服务器"；第 1 条从"不提供分支/tag/blame"改为"提供 branch/tag/blame（M4），但不做内容三方合并"。
- **§1 新增"不可兼得"条目**：要 git 命令原样可用就得采用 git 对象格式，而该格式的 blob 粒度（整文件）容不下块级增量与有界保留 → 故自定格式 + 自建 git 形状命令面。这条同时是 §15.1 的理由。
- **NFR 收紧**：NFR-1 内存与 NFR-5 配额改为**跨全部 project 之和**；NFR-7 加"union 判定不得退化成 head 相乘"；NFR-10 溯源键加 `project_id`。
- **§10 测试**：不变量 (a) 判据改为 head 集合相等；(c) 改为"只增不减"；新增 (e) collision 判定完备性；生成器允许跑出 3-peer 序列提前验格式；基准加 union 遍历。
- **§11 里程碑**：M1 加多 project 复用；M2 加拨号对称；M3 从"分叉处理"改为"多 head + union"；新增 M5 多端（不承诺）。
- **§15 已知限制**：新增"不兼容 git 对象格式"（理由指向 §1/§17，避免被误读成没想明白）、"`peer_id` 不签名"、"v1 只 1+1"。
- **引用审计修正**（同批）：v0.2 里残留的 v0.1 编号全部纠正——FR 表验收列 `§11 不变量/crash injection/路径对抗 fuzz` → `§10.x`；§8 符号链接行 `拒绝（§10）` → `§9.4.1`；§1 末行去掉对"§2 第 1 条"的悬空指认。

### v0.1 → v0.2（2026-10-15，同日）

模型从"两目录持续收敛（Syncthing 型）"改为"两端各自版本库 + 快进传播（git 型）"，依据用户表述："我就是想做一个能直接在两个操作系统之间传输文件的 git"，并明确"版本都要记录下来，不然会乱、历史会找不到"。

- **删除**：v0.1 §4 收敛一致性 + Lamport/base 游标；§7 十六行冲突裁决表（含 LWW、删改冲突、双改名、`.sync-conflict` 式文件命名）；§8 时钟偏移行；`conflict.*` 策略项；改名启发式；`history` 表与 `rev_*`/`dirty` 字段。
- **保留但改理由**：删除熔断从"防并发误删"改为"防客户端 bug 把误 `rm -rf` 变成合法历史"；§7.3 铁律原样保留至今。
- **新增**：为何不是 git（§1 三条理由）；快进不变量；§7.2 残留冲突（未提交 × 检出）；命令集 FR；有界历史与 GC；对象模型 schema；与 git 的量化差分（取代与 rsync 比对）；NFR-6 commit 生成成本；已知限制节。
- **升级**：磁盘写满风险（版本化后写得更快）；静态加密（历史 = 新泄漏面）。
- 未改动：NFR 资源数字与 1 核 6 G 硬约束、SSH 复用不开端口、块级增量、安全 1–4、非目标骨架。
