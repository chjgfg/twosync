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
| FR-3 | 元数据与清单存 `storage-engines`（默认 `lsm_tree`，运行时可切换），磁盘态、重启不重扫全量；**文件字节不进引擎**（在 CAS 里）。所有扫描**必须分页、且尽量少调用**（§9.6.1：窗口在这里是 CPU 旋钮不是内存旋钮） | 重启后首周期只扫脏子树；100 万条下扫描峰值 ≤ **8 MiB**，且**与块大小、文件总数无关**（定义见 NFR-1；**现状：这一条在 0.4.0 上未达成，判据已拆成 (f1)/(f2)，见 NFR-1 与 §9.6.1 的实测更正**） |
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
  **现状（2026-10-17 实测，未达成，需要裁决）**：twosync 自己那一层已经做到与文件总数无关（建树与比对合成一趟流式，§17 补记九），但引擎每次读/写调用都要折一遍全库（§9.6.1），于是现场数字是：`status` 峰值 **37.7 MiB**、`scan` **24.9 MiB**、`commit` **40.6 MiB**（22 000 行 / 库内约十万个存活键）。**这条判据现在过不去，而且不是靠调窗口能过的**。三个选项：① 等上游给 `collect_latest_raw` 加边界（§14 Q10，引擎里 `scan_latest_light_window` 已经写了一半）；② 把判据改成「twosync 这层 ≤ 8 MiB + 全库折叠次数有界」并把引擎那部分单列；③ 换门面直用具体引擎（放弃 §10.8 矩阵）。**默认走 ①，② 作为 M0 收口时的书面降级，③ 不做**——要改口径请明说，我先按这个记。
- **NFR-2 CPU**：单核机器上不许吃满一核。首次 10 GB 建库（全量 hash + 切块 + 落 CAS + 写清单进引擎）预算 ≤ 8 min 且必须可限速；稳态增量会话 CPU 峰值 ≤ **10% 单核**。CAS 化之后建库比 v0.1 的"只 hash 不搬字节"更重 IO——这条必须实测，不接受纸面数字。
- **NFR-3 IO 干扰**：读写走 buffered + 顺序大块，Linux 侧尽力 `ionice`/`ioprio_set`。**后台 GC、引擎 vacuum、LSM compaction 都不得制造 IO 突发**（单核机上这是最容易被忽略的一项）。不允许出现"同步启动导致策略进程延迟抖动"。
- **NFR-4 带宽**：默认上限 = 上行探测值 80%，可硬配 `--max-bps`。
- **NFR-5 磁盘**：CAS ≤ root 体积 20%（FR-9，站点级）；`status` 必须能报出"当前历史深度相当于几个全量副本"、**跨 project 去重省下了多少**、以及**引擎自身占了多少字节**（MVCC 版本行也是磁盘账），否则配额是不是够用没人知道。
- **NFR-6 commit 生成成本**：自动提交一轮的耗时是 **O(变更目录数)**，不是 O(文件总数)。10 万小文件的工程改 3 个文件，建 commit 不许重抄全树清单。
  **实测更正（2026-10-17，见 §17 补记九）**：这条在"每次重建 Merkle 树"的模型下不可能按字面成立——建树必须给**每一个**目录算一次 hash 才能知道它没变，所以那一腿恒为 O(目录数)。融合 diff 之后 `commit` 参照侧多出每目录一次点查，而 `put_tree` 本来就已经有每目录一次 `get`（幂等检查，见 §17 补记二），量级没变。真正 O(变更目录数) 的只有"两棵树都在库上"的比对（`walk_pair`，`diff --from/--to` 那一类）。**而 22 000 行的站点上，`commit`/`status` 的耗时大头既不是这两者、也不是 diff，是 `W` 的分页扫描（11–15 s）**——NFR-6 想验收的那件事，得先把那一笔账归因清楚（补记九）。
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

1. **扫描物化的是「整个库」，不是「那一页」**（**2026-10-17 实测更正**——本条原文说"分页就能挡住内存"，前提是错的）。四个引擎的 `scan` / `prefix_scan` / `scan_foreach` / `search_keys` 全都先调 `collect_latest_raw`（在各 `*_mvcc.rs`）：第一步 `scan_latest_light(u64::MAX)` 折的是**全库**，第二步给折出来的**每一行**取值，之后才按 pattern 过滤、才切页。⇒ 每调用一次就是 O(全库存活键) 的内存。隔离实测 **207 B/键**：同样命中 3 行、同样 1 页，库 203 键 ⇒ 峰值 21 KB，库 8 003 键 ⇒ 8.29 MB；把窗口从 2000 缩到 100，峰值**一字节不变**（`tests/engine_smoke.rs::search_keys_folds_the_whole_db`）。写侧同因：空库按批写 2 000 行 ⇒ 1.0 MB，同一个节奏写进已有 20 203 键的库 ⇒ 5.7 MB（`write_peak_tracks_the_whole_db_too`）。
   所以规则是**反的**：① **少调用**——窗口按"整个前缀尽量一次读完"取（`build::PAGE = 20_000`；同一份库实测 `status` 2 000 → 9.99 s、20 000 → **2.87 s**、100 000 → 2.07 s，而峰值 35.8 / 37.7 / 35.4 MiB 基本不动）；② **不许用 `seek` 推进游标**（那等于每前进一条就折一次全库）；③ 我们这一层的内存有界靠**流式结算**（§17 补记九/十），不靠窗口大小。
   结论一句话：**NFR-1 那个"峰值 ≤ 8 MiB"在 0.4.0 上不可能由 twosync 侧达成**，缺口在引擎（→ §14 Q10）。
   仍然有效的那半句：release 与 debug 差 5–40 倍、同规模抖动 8 倍 ⇒ 离开 profile 的数字没有意义。当年在 5 002 条目的库上量到 `peak heap = 5.6 MiB` 并据此说"常数内存成立"，现在看明白：那是因为**库本来就小**，全库折叠那一项还没长起来。
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
   - (f) **分页不变量**：100 万条规模下任何扫描的峰值新增分配 ≤ **8 MiB**、且与块大小与文件总数无关，扫描期间 `health().active_txns` 不得长期 > 0（§9.6.1/2）；**判据已按 2026-10-17 的实测拆成两段**（§9.6.1、NFR-1 现状）：(f1) *twosync 这一层*——建树/比对的栈与页缓冲，可以现在就断言与文件总数无关；(f2) *引擎那一次全库折叠*——O(全库存活键)，**在 0.4.0 上只能断言"次数有界"（= 窗口取大之后的调用数），量级不归我们管**，等 §14 Q10 落地再并回 8 MiB。
   - (g) **二级索引一致性**：任何改 `K.refcnt` 的路径都必须同步改 `k` 索引——用随机序列断言两者永不漂移（这是手工索引最容易烂掉的地方）。
2. **crash injection**：在块写入中途、`fsync` 前后、清单提交前、head 集合更新前分别 `kill -9`；重启后断言：无半文件、无 orphan tmp 存活超 5 s、无"清单引用不存在的块"、不变量 (a)–(g) 仍成立、不重复传输已到位的块。**其中一半用例是在撞引擎的 WAL 恢复路径**（§15.10 明说这件事）。
3. **路径对抗 fuzz**：`../../`、`\\?\`、UNC、`CON`、尾空格/点、超长名、仅大小写不同的两文件、NFC/NFD 混合、含 ANSI 转义 → 全部拒绝且不崩。
   **落地状态（`tests/path_hostile.rs`，2026-10-17）**：敌意清单在 `restore` / `checkout --prefix` /
   `history::resolve` 三条路上逐个试，判据是"**报错 + 不崩 + 整棵站点目录一字未变**"（最后那条才是硬的，
   前面全是它的必要条件）。古怪但合法的名字（空格、中日韩、`#%`、`=+,.`、NFC/NFD 两种形式、120 字长名）
   走"扫 → 提 → 导出到别处 → 逐文件比字节"全等。**两条如实标注的例外**：
   ① 仅大小写不同的两个文件在 Windows 上根本建不出来 ⇒ 那条用例挂 `#[cfg(unix)]`；
   ② 制表符、尾随点/空格在 Windows 上是**非法文件名**，收进"合法名字"清单就会变成测平台而不是测自己，所以不收。
   顺带钉住一条容易被误当成漏洞的语义：`""` 与 `"/"` 解析到**根**（`resolve` 滤掉空段与 `.`），
   所以它们的拒绝理由是"目录归 checkout 管"，而 `checkout --prefix ""` = 整棵树——这个语义不许漂。
4. **与 git 的量化差分**（取代 v0.1 的"与 `rsync -n --delete` 比对"）：同一份 10 GB 语料 + 同一串变更脚本，跑 ①本工具 ②git（`git add -A; commit; push`）③Syncthing 默认结果，比 **传输字节数 / 磁盘占用增长 / 完成耗时 / 误删次数**。本工具必须在"传输字节"和"磁盘占用增长"上明确赢过 git，否则它没有存在理由——这张表是 README 的主图，也是 §1 那三条理由的兑现证明。
5. **真机长跑**：10 GB 混合语料（1 个大稀疏文件 + 10 万个 4 KB 小文件 + 若干二进制资产），跨 Windows↔Ubuntu，随机变更脚本跑 72 h，记录 RSS/CPU/带宽/会话耗时/历史深度/head 数量，**外加 `health().max_chain` 与 `rows_above_one` 两条曲线**（§9.6.2 的复发通道只能靠时间暴露）。
6. **基准（`criterion`）**：全量扫描、脏判定、commit 生成（NFR-6）、块切分+hash、`want/have` 协商、清单 diff、**union 可达性遍历（NFR-7）**、**分页扫描窗口大小调参**。
7. 判据：1/2/3 未全绿，**禁止发任何 release**；且 release 时 `storage-engines` 必须是 §9.7 的版本号依赖，不许残留 `path`（提交前用 `cargo tree -p storage-engines` 核对）。
8. **引擎矩阵**（本项目独有的素材）：同一套 §10.1 不变量 + §10.6 基准在 `lsm_tree` / `bplus_tree` 上各跑一遍，`memory` 上跑全量 proptest（无持久化 = 快一个量级，适合当回归主力）。四引擎在同一真实负载下的 RSS/CPU/p99/磁盘增长，加上 `health()` 读数必须一致这条约束，就是"自研引擎吃自己狗粮"能拿出的最硬的表。

## 11. 里程碑

| 阶段 | 交付 | 可验证判据 |
|---|---|---|
| **M0 骨架** | `cargo init --lib --name twosync`；CLI(`commit/log/diff/show/checkout/restore/status/plan/apply/version`，**这一轮补齐 plan/apply/version**)；**CAS 块仓 + blob/tree/commit 读写**；§9.2 键编码 + 引擎接入（FR-21 自检）；扫描器 + 变更检测 + 自动建 commit；**project 注册表**。**先不联网** | 扫 10 GB 建库给出实测耗时/RSS/CPU（NFR-1/2 变真数字）；**扫描峰值按拆后的 (f1)/(f2) 两段验收（≤ 8 MiB 那条在 0.4.0 上过不了，见 §9.6.1）**；改 1 MB 后 `commit` 成本符合 NFR-6；`restore --at` 取回任意历史版本；`checkout --to` 能把版本导出到工作树外（目标在工作树里必须拒，§17 补记十一）；`status` 能报出"扫描之后的增删"（盘上对账，只进打印不进账，§17 补记十二）；`H` 前缀能表达多 head、`K`/`k` 双写不漂移；改名按 FR-1 记进 `commit.stats.rename`（不是一删一增）；`status` 报出 §9.6.2 的引擎健康度与 NFR-5 的磁盘账；§10.3 的路径对抗清单成文用例 |
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
10. **上游要开一个有界的读出口**（2026-10-17 实测后新增，M0 收口的实际卡点）：`collect_latest_raw` 现在无论 `scan`/`prefix_scan`/`scan_foreach`/`search_keys` 都先折**全库**（`scan_latest_light(u64::MAX)` + 给每一行取值），所以 NFR-1 的 8 MiB 判据在门面这一侧没有可达路径。三条现成的落点，按代价排：
    ① `collect_latest_raw` 收一个 `[start, end)` 边界——**引擎里 `lsm_tree.rs:983 scan_latest_light_window(from, snap_seq, max_rows_per_source, group_suffix)` 已经把带游标的轻扫描写好了**，MVCC 那层没接过去，这是最小的一刀；
    ② 门面把已有的 `prefix_scan_foreach` 提到 `Transaction` trait 上（`engine.rs` 里 `foreach` 出现 0 次，四个引擎的具体类型上都有）；
    ③ `scan_foreach` 那句文档"不收集全库到单一大 Vec"目前是**反的**，先改注释也别留着骗下一个人。
    → 要我做哪一步、还是先按 NFR-1 的 (f1)/(f2) 书面降级把 M0 收掉？
   **2026-10-17 更新**：用户已另派人修引擎侧（「每次读/写调用折全库」这一笔不在 twosync 这边了）。在它落地前，twosync 的读路径策略维持 §9.6.1 改判后的口径：**窗口是 CPU 旋钮，尽量少调用**。

（v0.1 的 Q1「服务器侧写入是程序产出还是人编辑」已不再是阻塞项——版本库 + 多 head 天生支持两端都写。）

## 15. 已知限制（README 必须原样列出）

1. **不兼容 git 对象格式与 git 协议**，所以真 git 命令读不了本库；手感照 git 自建（§16.2），语义相近但**不是**同一实现。原因见 §1 第 1、2 条与 §17。
2. `branch`/`tag` 只是本地命名，v1 **不参与同步**；跨站协作仍需真 git。
3. collision 只能人选一边，不合并内容。
4. 硬链接不识别（§8）。
5. 不转 CRLF/LF，不做文本友好化。
6. v1 实现只有 1+1 个 peer；**多端、NAT 穿透、部分连通路由不做**（§2.3、M5）。
7. `commits.peer_id` 不签名（§9.5.2），历史来源可被本机伪造；对等身份靠 TOFU 指纹表。
8. **`status` 的"盘上对账"只报存在性差异，不报内容差异**：判据是目录 mtime，而"往文件里写内容"
   不改父目录的 mtime。要看内容变化仍然只有 `scan` 一条路（§17 补记十二）。
9. **目录 mtime 的回写是滞后的**（NTFS 实测：写完子项后立刻读父目录，时间戳比真相旧 40 ms 量级）。
   所以可疑判据带 1 s 余量（宁可多比几层 readdir），极端情况下仍可能漏一层——这一条与 git 的
   `core.untrackedCache` 撞的是同一堵墙，它的文档原话是"先确认你这台机器的 mtime 靠得住"。
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

- **`plan` / `apply` 是 FR-7 的命名对，不是两条流水线**：`plan` 只读（预演"这次会提交什么"+ FR-10 熔断预演 + head 会怎么动），
  `apply` 是执行侧**且就是 `commit` 同一个函数**。两份"怎么提交"迟早会有一份漏掉某道闸，所以宁可同名不同写法。
  union 判定与 collision 列表要等 M1/M3 才有对象：`plan` 现在就把这件事明说出来，不放空字段。
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
   **【2026-10-17 已按这句话执行：`commit` 一侧的上限已经取消，见下面"补记七"。】**
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

### v0.4 补记七（2026-10-17，扫描标记 `Z` + 流式建树：`commit` 的内存与文件数脱钩）

**新增 `Z|p` 扫描标记（`worktree::ScanMark`）**，只在**一轮扫描完整跑完**之后写。它同时解决两件事：

1. **中断的扫描不再伪装成大规模删除。** 老写法拿"行里最大的 `scanned_at`"当 epoch，于是一轮
   被 Ctrl-C 拦腰的扫描会把半棵树认成"最新"，剩下半棵看起来全被删了——只能靠 FR-10 事后拦。
   现在 epoch 只由完整扫描推进，活性判据是 `scanned_at >= Z.epoch`：中断那半轮盖过的行更新、
   照样算活，没盖到的行还留着上一次完整扫描的章 ⇒ 一条都不丢。`Z` 缺失（从没完整扫过）时
   **一律不算活**，报"先 scan"，而不是退回去拿最大值。
2. `project status` 有了"第几轮 / 盖章多少行"这种能核对的数字。

**建树改成栈式流式**（`build::LiveRows` + `build::Builder`）：分页读 `W`、按升序路径维护一条
祖先栈，栈顶不是当前路径的父目录就结算出栈。**内存 = O(最大单个目录的子项数 + 目录深度)**。
`MAX_ENTRIES` 从 `commit` 这条路上**取消**了——它原来守的是那张 O(文件数) 的条目表，而表已经
不存在；`plan` 还留着一个上限（`plan_capped` 可注入），因为它要把算出来的树留给 `diff` 递归。
**这条没做完**：真正的解法是把 diff 融进建树（每层结算时顺手与旧树对应层比掉），那样
`TreeStore` 也能消失。分桶 + `collect_children` 那套老代码整体删除——顶层目录静默丢失那个 bug
就住在那里，栈式没有那份第二索引可写错。

**实测（release，Windows，同一台机器，全部是 `commit` 一行的 `peak heap`）**：

| 语料 | 条目数 | 老实现 | 新实现 |
|---|---|---|---|
| 2004 条（2000 个文件挤在 `data/` 一个目录里） | 2 004 | 5.3 MiB | — |
| 20 000 个文件摊在 2 000 个目录里 | 22 000 | 按 5.3 MiB/2004 外推 ≈ 58 MiB（**外推，非实测**） | **6.1 MiB** |
| 20 000 个文件挤在一个目录里（最坏情况） | 20 001 | — | **8.5 MiB** |

两行新数字之比就是这次改造的形状：条目数 ×11，峰值只 ×1.2；而最坏情况（全挤在一个目录）
比摊开只多 2.4 MiB——**多出来的正是根那一帧必须拿在手里的顶层清单**，这是树对象格式的固有
代价，git 的 tree 同理。结论：**NFR-1 那条"与文件总数无关"对 `commit` 成立了，对 `status`/`diff`
还差最后一步（融合 diff）。**

**`project remove` / `project status` 落地**（FR-19 补齐）。`remove` 严格按 §16.1：**只动注册表**，
工作树、历史、CAS 一概不碰，`--purge` 在改注册表**之前**就明确拒绝（回收要按跨 project 的
refcnt 走，那是 M1 的 `gc`）。`current` 清空而**不自动改选**——下一条命令作用在哪个目录必须是
人明确选的。现场验证过派生 id 的价值：摘掉 `lab` 再 `add lab`，`status` 直接显示
`head = c889b03f … 22000 行 W / 未提交 +0 ~0 -0`，同一份历史原样接上。

**这一轮我自己写错的三处**（都被用例抓回来）：① `LiveRows::next` 在最后一页先把缓冲丢了
（`fetch` 说"没有更多页"时那一页的数据还在 buf 里）⇒ 17 个用例全红；② 上限断言照抄了老的
"逐条计数"预期，而新实现是"整层一次结算"，`(4,3)` 应为 `(5,3)`；③ 两个测试在**迭代之前**读
计数器（流式对象的计数器是迭代时填的），断言恒为 0。第 ③ 类尤其值得记：**流式 API 的统计量
只能在消费之后读**，否则测试会安静地通过或安静地测错东西。

85 用例绿（新增 3 条 + 注册表 1 条），clippy --all-targets 0 警告。M0 剩余：`mv` / `config` /
`--at HEAD~n` 相对写法 / `status` 的未跟踪文件视图 / 把 diff 融进建树。

### v0.4 补记八（2026-10-17，`mv` / `config` / `HEAD~n`：M0 的三个小命令清完）

**`HEAD~N` / `^N`**：`history::resolve_rev` 现在先剥后缀再解析基座，剥完沿**第一亲代**走 N 步。
三条判据值得记：① `~0` 是合法写法，"认出后缀"和"步数为 0"是两件事，用 `steps==0` 当"没后缀"
判据会把 `HEAD~0` 报成看不懂（用例抓的）；② 两个符号在这里同义 ⇒ 链式写法是**步数相加**，
一次 `rfind` 只剥一个，必须循环（`HEAD~2^` 会剩下 `HEAD~2` 当基座去查 hash）；③ 越界要报
"断在第几步"而不是安静停在根 commit——否则 `checkout --at HEAD~99` 会"成功"地把人扔到初始版本。
多 head 时 `HEAD~1` **报错**，不替用户挑一条链（那是 M3 的决策）。

**`mv`**：只做"安全改名"——源不存在、目标已存在、路径想逃出项目目录，三种都在 `fs::rename`
**之前**拦。没有 `--force`：想覆盖就该显式先 `restore`/删，少一个静默丢数据的口子。
路径校验不重写一遍，改成 `registry::check_rel_path` 返回**消息字符串**、由调用方各自包前缀
（规则一份，消息两处）。实测改名的账：`changes = +1 ~0 -1` 而 `blob 0 新、块 0 新写入`
——内容寻址让改名既不复制数据也不占传输，只是账面上还是"一删一增"（改名检测 = FR-17）。

**`config`**（`<root>/.twosync/config.toml`）：**只存今天真有人读的键**，现在就是
`engine` 与 `block_size` 两个。`config` 最容易长成一个摆设——键存进去了、读的人还没写，
于是"配了没生效"变成最难查那一类。FR-9 的配额/保留策略、FR-12 的排除规则**故意没加**，
它们的消费者（`gc`、扫描器 gitignore 追加）在 M1。
优先级"命令行 > 配置文件 > 内置默认"**只在 `Cli::store()` 里实现一次**；`--engine` 因此
从 clap 的 `default_value_t` 改成 `Option`——给了默认值就分不清"用户写了"和"没写"，
配置文件永远压不过一个凭空出现的值。`config get` 打印**生效值 + 来源**，因为"没写"和
"写了默认值"只看文件分不出来。
换 `block_size` 为什么不动已有数据：每个 blob 清单里写着自己的 `block_size`，块文件按内容
hash 命名 ⇒ 读旧 blob 不依赖当前配置。现场验过：配 `1MiB` 后新 blob 的清单是
`1 块 × 1048576 B`。下限卡 64 KiB、上限 256 MiB（调太小会让 10 GB 文件爆出 16 万个块）。
换 `engine` 会**换数据目录**（`db/<engine>/`），实测 `doctor` 立刻 `rows=0` —— 所以
`config set engine` 必须把这句话打印出来，而不是让人以为数据会跟过去。

**顺手修的一个说谎**：`LocalExists` 的文案原来固定写"加 --force 才覆盖"，而 `mv` 根本没有
`--force`。错误类型该共用（同一件事：目标已存在、我没动手），**建议不该共用** ⇒
变体现在带 `hint` 字段，由抛出的那一层给。

98 用例绿（新增 12 条：`mv` 5、config 5、rev 2），clippy 0 警告。
M0 只剩：`status` 的未跟踪文件视图、把 diff 融进建树（让 `plan` 也摆脱条目上限）、
以及 `checkout --at` 之外的 `--to` 目录级检出。

### v0.4 补记九（2026-10-17，diff 融进建树：`TreeStore` 删除、上限从"项目"挪到"单层"）

**改了什么**。每层目录结算时顺手与参照版本的对应层比掉，清单出栈即丢 ⇒ `plan` 不必再把
整棵新树留给 diff：`history::TreeStore`、`history::resolve_in` 整体删除，`Built` 少一个字段，
`PlanSink` 退化成零字段的 `HashOnly`。分类规则收成**一份** `diff_level(…, nest)`，两种嵌套
策略差别写进类型：`Recurse`（两棵树都在库上 ⇒ hash 相同整块子树跳过）与 `Settled`（子层已在
自己结算点比过，再递归就是重复计数）。`walk_pair` 现在只是 `Recurse` 的入口，签名少了 `trees`。

- **`MAX_ENTRIES` → `MAX_DIR_ENTRIES`**：上限从"项目总条目数"改成"**单个目录**条目数"，
  报错带目录名（根印 `.`）。这不是改数字，是换掉被拒绝的那件事：老上限意味着"项目大到
  一定程度连 `status` 都跑不动"。
- **`LiveRows::with_prefix`** ⇒ `--prefix` 变成"根帧 + 键前缀"，`checkout` 的 `roots()`/`dir_kids()`
  删除；吐出来的路径仍是**工作树完整路径**，落地动作不必各处再拼一次前缀。
- **`checkout` 上同一趟遍历，方向是反的**：工作树是流式那侧（`new`）、目标版本是参照侧（`old`），
  所以 `Add` = 磁盘上要**删**、`Delete` = 要**建**。参数改名 `reference`/`worktree`（靠位置猜方向
  就是下一个"把删读成建"的产地）；`write()` 现在先 `create_dir_all(parent)`，因为融合遍历
  自底向上，新目录里的文件会比目录自己先报出来（老写法自顶向下不会）。
- `status` 补了 `耗时 / peak heap` 一行（与 `scan`/`commit` 同一把尺，否则 NFR-1 那句承诺
  只是注释）；`print_changes` 打印前对那份 30 条副本按路径排序——样本序现在是遍历序（深在前）。

**代价说清（权衡，不是免费午餐）**：比对自底向上，而"这一层变了没有"要到结算时才第一次知道，
那时已经没有新清单可留 ⇒ 参照侧只能压在**下栈时**读。于是 `plan`/`commit` 每有一个目录就多
一次 `get`。语料 = 20 000 个文件摊在 2 000 个目录里（22 000 行 `W`，19.5 MiB）：

| 同一站点同一份库，release | 融合前（base） | 融合后 |
|---|---|---|
| `status` 耗时 | 11.9 / 11.5 s | 14.9 / 13.2 / 13.8 s |
| `status` peak heap | **26.4 MiB** | **25.1 MiB** |
| `commit`（有 1 个历史版本，6 处变更） | — | 20.5 s / 28.5 MiB |
| `commit`（首次，20 000 个新块） | — | 118.8 s / 51.1 MiB |

内存只降 1.3 MiB —— 22 000 条清单本来就只值 2~3 MiB。**真正被删掉的是那条会拒绝服务的上限**：
25 个文件摊在 5 个目录里、上限压到 6 照样算完，同样数据挤进一层就在第 7 条拒
（用例 `the_cap_is_per_directory_not_per_project`）。多花的 2~3 s 归因到那 2 001 次点查：
`ls-files` 走同样 2 001 次 `read_tree` 再打印 22 000 行只要 1.08 s，而 `doctor`（只开引擎）0.3 s。

**顺带量到一条更值钱、还没归因完的**：`scan` 的峰值随 **`W` 行数**线性上升，与要不要重算 hash
无关——22 000 行 `hashed=0`（全部按 mtime/size 跳过）仍然 **22.6 MiB**，2 000 行 4.1 MiB
（≈ 每行 1 KiB + 固定 2~3 MiB）。扫描器自己的遍历是常数（`--batch 100`），**所以这 1 KiB/行
不住在我们这一侧，住在引擎的写缓冲/批次里**。§9.6 ① 说"扫描必须分页否则顶穿 NFR-1"——现在
看清楚：分页挡的是**读**侧，写侧还有一条按行数长的账要查（M0 收口前必须先归因，别先调参数）。

> **这一段的归因是错的，下一段（补记十）里已经推翻**：既不是"写缓冲"，也不是按 `W` 行数——
> 是按**全库存活键数**，读侧写侧同一个因。当时两个语料的 `W` 行数恰好与库的总大小同步增长，
> 我就把相关当成了因果。留着原文不删，是因为这个错法本身值得记住。

**这一轮我自己搞错的一处，值得记住**：A/B 表刚拿到手时我把 base 的 peak heap 记成了 18.5 MiB
——那其实是自己那行 `耗时 = 18.986 s` 看串了列，而 base 二进制**从来没打印过内存**。
补三行插桩、重编译之后才拿到真值 26.4 MiB。**对照实验的每一侧都必须真的被测过**，
否则表里那一格是猜的，而猜的那一格恰好是这次改造的主张。

99 用例绿，clippy `--all-targets` 0 警告。M0 只剩：`status` 的未跟踪文件视图、`checkout --to`
目录级检出；外加一条新的待办：**scan 峰值 1 KiB/行 的归因**。

### v0.4 补记十（2026-10-17，归因落地：引擎每次读/写调用都折**全库**，NFR-1 的判据被推翻重写）

补记九留了个"1 KiB/行、大概住在写缓冲里"的猜测。两条隔离实验把它证伪了，顺便把真因钉死。

**实验一（读侧）** `tests/engine_smoke.rs::search_keys_folds_the_whole_db`：两侧都只放
**3 条** `W` 行（命中数、页数完全相同），另一侧放别的标签把库撑大。

| 全库存活键 | 一次 `search_keys` 峰值 | 窗口 |
|---|---|---|
| 203 | 40 451 B | 2 000 |
| 8 003 | **1 581 179 B** | 2 000 |
| 8 003 | **与上一行一字节不差** | **100** |

⇒ 峰值 ∝ **全库存活键**（198 B/键），与命中数、与窗口都无关。窗口缩小 20 倍内存**一点没降**
——"分页挡住内存"这句从 v0.4 起就写在 §9.6.1 里，它是错的。
（同一档实验最初跑的是 40 003 键 ⇒ 8 285 876 B / 207 B/键，窗口 100 同样是 8 285 876 B；
后来把语料缩到 8 000 键好让套件跑得动，比值不变。）

**根因在源码里，不用猜**：`lsm_tree/kv_ops.rs:252 search_keys` → `collect_for_search` →
`lsm_tree_mvcc.rs:838 collect_latest_raw`，第一步是 `scan_latest_light(u64::MAX)`（**不带边界**），
第二步给 winners 里**每一行**取值，pattern 过滤和切页都发生在这之后。`prefix_scan`、`scan`、
以及文档写着"不收集全库到单一大 Vec"的 `scan_foreach`（`kv_ops.rs:179`，注释与实际相反）
走的是同一条；四个引擎全部同构。

**实验二（写侧）** `write_peak_tracks_the_whole_db_too`：按扫描器的节奏（一事务一批、批 100）
写 2 000 行——空库起点 203 键 ⇒ 1.03 MB；同样 2 000 行写进已有 20 203 键的库 ⇒ 5.70 MB。
⇒ 写侧与读侧同一个因：**每次事务提交也在库里做一次全库级的活**。所以 `scan` 那 22~25 MiB
的地板不是"我们攒了 22 000 行"，是"库有十万个键"。

**顺带把 `--batch` 也量了**（现场 `scan`，同一站点）：100 → 9.7 s / 24.9 MiB；2 000 → 16.4 s /
27.8 MiB；20 000 → 23.9 s / 31.7 MiB。**大批次在时间和内存上双输**，`--batch 100` 是对的，
这个旋钮不是杠杆——原来以为"批越大越省提交"，实测省不了。

**twosync 侧能动的只剩"少调用"**：`build::PAGE` 从 2 000 改成 **20 000**（同一份库 `status`
9.99 s → **2.87 s**，峰值 35.8 → 37.7 MiB，几乎不动；再大到 100 000 只多省 0.8 s，而页缓冲
按行数长，不取）。这一条把 §9.6.1 的规则方向反写了：**窗口在这里是 CPU 旋钮，不是内存旋钮**。
门面（`engine.rs` 的 `Transaction`）只有 `scan`/`prefix_scan`/`search_keys`，`foreach`
**一个都没透出来**（`grep -c foreach engine.rs` = 0），所以"流式读"这条路我们这边没有。

**判据改写**（不是放宽，是把不归我们管的那半单独记账）：NFR-1 的 `≤ 8 MiB` 在 0.4.0 上过不了，
现场 `status` 37.7 MiB、`scan` 24.9 MiB、`commit` 40.6 MiB。§10.1(f) 拆成 (f1) twosync 这层
与文件总数无关（可断言、已断言）/ (f2) 引擎那次全库折叠 O(全库存活键)（只能断言"次数有界"）。
FR-3、§11 M0 验收行同步改口。**上游那三刀写进 §14 Q10**，其中最便宜的一刀是引擎里
**已经存在**的 `lsm_tree.rs:983 scan_latest_light_window(...)`——带 `from`、带每源行数窗口、
带续扫游标，MVCC 层没接过去而已。

**这一轮我自己又踩的一次，值得记进量尺的说明书**：两条归因实验**单独跑绿、整套跑红**。
原因是 `allocstats` 的 `CURRENT`/`PEAK` 是**进程级**的两个原子数，而 cargo 默认让同一个测试
二进制里的用例**并行**跑——两条线程互相把对方的峰值抬起来，比值断言就随机翻。修法不是放宽
阈值，是给 `tests/engine_smoke.rs` 全部用例上串行锁（文件里写清楚了为什么）。
**"全局计数器 + 并行测试"这个组合从一开始就不能测内存**，§10.1(f) 那种断言以后都要带着这条。

101 用例绿（新增 2 条归因实验），clippy 0 警告。M0 只剩：`status` 未跟踪文件视图、`checkout --to` 目录级检出，
以及一个**要人拍板**的问题（Q10：等上游 / 书面降级 / 放弃门面直用具体引擎）。

### v0.4 补记十一（2026-10-17，`checkout --to` 目录级导出 + `restore --to` 那道门管错了对象）

**`--to` 不是"少做一点的 checkout"，是另一件事**：把一个版本（整树或 `--prefix` 那一棵子树）
完整写到**另一个目录**，不比对工作树、不删任何东西、不动 head。所以 §7.3 与 FR-10 在这里
**没有对象可保护**——那两道闸防的都是"工作树里的本地内容被抹掉"，而目标目录跟工作树无关。
它带来一道这里才有的新闸：**目标不许落在工作树里**。防的不是覆盖，是**污染版本库**：
往自己要同步的目录里塞一份历史副本，下一轮 `scan` 会把它当项目内容提交，同一份字节在 CAS 里
白占一倍。判据用 `canonicalize` 后的**规范路径**比较（取"最近一个存在的祖先"，因为目标常常
还不存在）——字符串比较会误拒 `p/../out`、误放 `p/./x`。

其余四条判据：
- **默认只报**（沿用 FR-8 那一对形状），`--apply` 才动手；只报模式连目标目录都不建。
- **目标上已有的路径默认不覆盖**，记 `skipped` 并打印一句为什么；`--force` 才盖，且算
  `overwrite` 不算 `files`。不套 §7.3：那道门查的是工作树那一份，跟目标不是同一个文件。
- 遍历用 `history::walk_tree`（**前序** ⇒ 父目录先于子项，且逐条回调不物化路径表），
  但**顶层文件的父目录就是 dest 自己**、它不是树里的条目 ⇒ 写入前补一次 `create_dir_all(parent)`。
- `--prefix` 导出的是**子树的内容**，摊在 dest 下（与 `restore --to` 同一条规矩），
  不再套一层子树名；前缀的两种错法（树里没有 / 指向文件）与 `checkout` 同一个 `dir_only`。

**顺手修了一个管错对象的门**：`restore --to <已有路径> --force` 以前会**无条件**走 §7.3，
于是"工作树里那版没扫过"会让一次跟工作树毫无关系的导出被 `NotInHistory` 拒掉——
**一句关于另一个文件的拒绝**。现在门只在目标就是工作树路径时查；不查的时候
`gate_checked = false` 如实打印（"目标是工作树外的路径，那道门管不到它"）。
教训与 §16.2 那条老规矩同源：**判据只有一份，但它只管它管得着的路径**——把门"复用"到
另一个对象上，看起来是不添新逻辑，实际是给那个对象安了一条不属于它的规则。

**实测**（ts6 站点，20 000 文件 / 2 000 目录；release）：`checkout --at v1 --prefix d007 --to <外> --apply`
⇒ `写文件 10 / 建目录 0 / 链接 0 / 覆盖 0 / 跳过 0 共 10.0 KiB`，`elapsed = 0.027 s`，
与同版本的工作树文件 `cmp` **字节全等**；再导一次同样目标 ⇒ `跳过 10`；
`--to /tmp/ts6/lab/o` ⇒ 明确报"落在项目工作树里面"并拒掉。库内行数一根没动（导出不写库）。

107 用例绿（新增 6 条：`--to` 5 条 + `restore --to` 门 1 条），clippy 0 警告。
M0 只剩 **`status` 的未跟踪文件视图**——它的前置是扫描器给"见过但没记录"的路径留痕
（`deferred`：打不开/读失败的那些，今天连 `W` 行都没有，所以 `status` 永远看不见它们），
以及 FR-12 排除规则落地后同一视图的第二个来源。

### v0.4 补记十二（2026-10-17，`status` 的"盘上对账"：一趟走盘 + 只比可疑那层）

**要解决的是什么**：`status` 过去只看 `W`，所以"扫描之后新建/删除的文件"它一个字都不说，
末尾那句"扫描之后又改的文件不在这里，先 scan"就是这个瞎区的自供状。这一条补上存在性视图，
M0 到此清完。

**为什么不是留痕（A 方案）**：讨论过"扫描器给'见过但没记录'的路径留一条 `X|p|path`"。
它结构上够不到主问题——我们的模型里"扫到就进账"（没有 `add` 这一步），**新建的文件一定会被
下一次 `scan` 记进 `W`**，所以痕里永远不会有它们；痕只能装"读不了"和"被排除"两种。
git 也不留这种痕：它没有独立的扫描步骤，`status` 就是当场遍历。git 持久化的是**结果缓存**
（`dir.h` 的 `untracked_cache_dir { stat_data, .. }`，按目录存、靠目录 mtime 失效），
不是失败痕迹。所以最后落在"照 git 走盘 + 用目录 mtime 剪枝"，A 降级为
"deferred 计数放进已有的 `Z` 一行"（`Z` 每轮只写一次，热路径零额外写）。

**形状**（`worktree::view_since` + `reconcile`）：
1. 走**一趟** `walker(root)`（与 `scan` 同一条流 ⇒ 排除语义天然一致，被 `.gitignore` 挡住的
   东西不会在这里冒出来变成"未跟踪"）。前序遍历保证目录自己排在子项之前 ⇒ 读到目录就当场决定
   "这一层要不要收名单"，只有可疑层留名字 ⇒ 内存 = O(可疑层的子项数)，不是 O(文件数)。
2. 可疑判据 = 目录 mtime 晚于 `Z.epoch`（带余量，见下）。
3. 库里那一层的名单**复用 `status` 本来就要走的那趟 `W` 流**：新增 `build::plan_observed`
   的观察口（每读一条活行回调一次）。**不另开一次分页扫描**——在这个引擎里多一趟就是多几次
   全库折叠（补记十）。

**一个用真金白银换来的教训（我这一轮引入又改掉的回归）**：第一版给**每个目录**建一次
`walk_builder(dir).max_depth(1)`，觉得这样"排除语义与 scan 同一份函数"就算一致。同一份库上
`status` 从 4.1 s 涨到 **12.3 s**。归因不是猜的：分段计时给 `view_since = 0.43 s`（改成整树
一趟之后），`plan_observed = 3.0–3.7 s` ⇒ 那 8 s 全在 2 001 次 `WalkBuilder` 构造上（每次都要
顺着祖先链找 `.gitignore`）。**"复用了同一个函数"不等于"复用了同一份工作"**——判据一致要的是
同一套规则，不是把遍历重做 2 001 遍。

**否决掉的那个自然写法（用例钉着）**：`src`、`src.rs`、`src/main.rs` 三行在 `W` 里的字节序是
"目录自己 → 同名前缀的兄弟 → 子项"，因为 `'/'` = 0x2F 夹在 `.`(0x2E) 与 `0`(0x30) 之间；
而盘的 DFS 会先钻进 `src/`。**照着 DFS 做全局两游标归并，凡是"目录名是某个兄弟文件名前缀"的
目录就会把未扫的文件报成"盘上已删"**。所以比较单位定成"一层目录内部"——层内两边都是同一份
升序序列，两游标才成立（`prefix_colliding_sibling_is_not_reported_as_deleted`）。

**NTFS 的目录 mtime 是滞后回写的**（这条是现场量出来的，不是文档抄的）：往 `keep/` 写完一个
文件，用遍历缓存的 metadata 立刻读父目录 = `epoch − 1.3 ms`，几十毫秒后 `fs::metadata` 读到
= `epoch + 39.5 ms`。所以判据必须是 `mtime > epoch − MTIME_LAG`（1 s，刻意宽松），
方向只有一个：**宁可多比几层（花的只是 readdir），不可漏比（漏了就是只读命令撒谎）**。
git 的 `core.untrackedCache` 撞的是同一堵墙，它的文档原话就是
"Before setting it to `true`, you should check that mtime is working properly on your system"。
§15 新增第 8、9 条已知限制：看不见内容变化（写文件不改父目录 mtime）；滞后极端情况下仍可能漏一层。

**只进打印，绝不进账**：`§8`「扫描器不许判删除」一个字没改。打印分成两段并明写
"提醒 = 上面这些还没进账"。用例 `unscanned_changes_show_in_reconcile_but_not_in_the_book`
钉的就是这个分叉：**同一时刻 `W` 说"变更 = 无"、对账说"未扫 1 / 已删 1"**，两者都对。

**实测**（ts6，20 000 文件 / 2 000 目录 / 同一份库交替对照）：

| | HEAD（改动前） | 这版 |
|---|---|---|
| `status` 耗时 | 4.37 / 4.12 / 4.10 s | 4.61 / 5.12 / 4.33 s |
| `status` peak heap | 46.7 MiB | 46.7 MiB |
| 其中走盘 `view_since` | — | **0.32–0.39 s**（比过 2 000 个目录的 mtime，收 2 层名单） |

⇒ 对账的净成本 **+8%**，内存不动（多出来的只有可疑层的名字）。上限：可疑层 512、名单 30 条，
超了就只报计数并**承认名单不全**（`suspect_list_is_capped_and_says_so`）。

115 用例绿（新增 8 条：对账 6 + 上限/假阳性 2），clippy 0 警告。**M0 的功能面到此收完**；
剩下的都是账：`§14 Q10`（上游有界读出口）等人拍板，`§10.4` 与 git 的量化差分要等 M1 联网。

### v0.4 补记十三（2026-10-17，M0 收尾：`plan`/`apply`/`version`、FR-1 改名、status 的必填数字、§10.3 路径对抗）

用户的话是"除了引擎那笔账，看看 M0 还有什么没弄，一块弄了，**并保持写的功能可以正常用**"。
把 §11 的 M0 交付/判据逐格对着代码核了一遍，缺四块，都在这一条里补上（引擎侧那一块他已另派人修）。

**1. 命令面缺三条：`version` / `plan` / `apply`**（§11 的 M0 CLI 清单里一直写着 `plan`、`version`，从没做）。
- `plan` = FR-7 的只读那一半。它比 `status` 多说的两件，都是"只有要提交才需要知道"的：
  **FR-10 熔断预演**（用与 `build` 完全同源的三个数 ⇒ 这里判"会拒"，`commit` 就一定拒）和
  `tree` 变了没有 ⇒ head 会不会动。union/collision 是 M3 的事，**明说出来，不放空字段**。
- `apply` = 执行侧，**直接调 `commit` 那个函数**。两份"怎么提交"迟早有一份漏掉某道闸，
  这条规矩在 `checkout`（两趟共用一个 `Job::on`）和 `plan`/`build`（共用 `assemble`）上已经各买过一次单。
- `version` 报**生效值 + 来源**（`lsm_tree (内置默认)`），和 `config get` 同一判据；
  依赖版本不硬编（"版本以 Cargo.lock 为准：`cargo tree -p storage-engines`"），并顺手把
  **二进制体积**打出来——NFR-9 的 ≤ 15 MB 得有人在现场能读到。
  它**不开引擎**（NFR-8 的"热启动 ≤ 1 s"就是被这种命令吃掉的）。

**2. FR-1 的显式改名**：`commit.stats.rename` 一直是硬编的 `0`，注释写着"改名检测是 FR-17 的活"——
**两处都错**：FR-17 是后台常驻，改名的正本在 FR-1（"改名由 `mv` 子命令**或同 blob_hash 匹配**得出，
记在 commit 里，不靠事后猜"）。引用与实现一起补：`ChangeSet::fold_renames()` 用 hash 配对，
配成功一对就 `add-1 / delete-1 / rename+1`，把字节与 `deleted_blobs` 也一并撤掉（内容还在新树里，
它不是被删的 blob），**并且必须在 FR-10 熔断之前**——否则一次大规模 `mv` 会被熔断当成"删了一半仓库"
（新增用例 `a_mass_rename_does_not_trip_the_delete_circuit_breaker`：30 个文件改 15 个名字，
50% > 20%，不折叠就必然被拒）。目录也参与配对：目录的 hash 是子树 tree hash，整棵搬走两侧相同 ⇒
一次 `mv d d2` 折成 4 对改名（`d`、`d/e` 与两个文件）。
另一个坑是 `is_empty()`：**必须把 rename 算进去**，否则纯改名的那轮会被当"无改动"拒掉，
改名永远进不了历史（用例把这条钉成断言）。
配对窗口有上限（`RENAME_PAIR_MAX = 4096` ≈ 0.5 MB），超了就放弃检测并打印说明——
改名是账面与传输优化，不该为它吃掉常数内存的承诺。

**3. `status` 的必填数字**（§9.6.2 与 NFR-5 两处明文要求，之前一个都没报）：
引擎健康度三项 `active_txns / max_chain / rows_above_one`，磁盘三项
`工作树 / CAS / 引擎` 各自字节数 + 文件数与块数，加一句 **CAS/工作树 = N ⇒ 约 N 份全量数据**
（这就是"历史深度相当于几个全量副本"的可读形式）。
**"跨 project 去重省下多少"故意不给数**：它等于 Σ(全部 `B` 清单声明字节) − CAS 实际字节，
需要一次全量清单遍历 ⇒ 那是 `gc`（M1）的活。宁可印一行"要 gc 才知道"，也不在 `status` 里
藏一个把命令变成分钟级的循环。NFR-8 说 `health()` 是 O(物理行数) 不许进热路径——`status` 是人敲的，
与 `doctor` 同类；真正的热路径是 M2 的 agent/daemon，它自己走缓存。

**4. §10.3 路径对抗成文**（`tests/path_hostile.rs`）：清单照抄文档一条不落，判据是三条一起——
**报错 + 不崩 + 整棵站点目录一字未变**，最后一条才硬（前两条只是它的必要条件）。
合法但古怪的名字（空格、中日韩、`#%`、`=+,.`、NFC/NFD 两种形式、120 字长名）走
"扫 → 提 → 导出到别处 → 逐文件比字节全等"。两条如实标注的例外见 §10.3。

**顺手抓到的一处顺序错误**：`restore --at X ""`（解析到**根**）以前会报"拒绝覆盖工作树根目录：
工作树里这一条没被扫描过"——**目录根本没有"被覆盖"这回事**，类型分诊必须在覆盖门之前。
这与补记四那条（先读 blob 再判类型，把"这活归 checkout"的实话变成一次看起来像磁盘损坏的错误）
是同一族。现在：先看 kind ⇒ 目录直接 `NotYet`；再谈目标存在与 §7.3。同时把存在性判据从
`dest.exists()` 换成 `symlink_metadata().is_ok()`——断链不是"不存在"，否则 `make_symlink`
会撞在一个存在但指向空的路径上，报一句看起来像权限问题的错。

**写路径体检**（release，全新站点，每条都真跑）：`scan → plan(+3 R0，熔断通过) → apply(=commit) →
mv → scan → plan(+1 ~0 -0 **R1**，样本 `R src/main.rs → lib/main.rs`) → commit → log(带 R 列) →
show(`stats = +1 ~0 -0 改名 1`) → restore --at HEAD~1 → checkout --to 根外(建 2/建目录 2) →
status(盘上对账抓到 `restore` 刚写回去的那个文件，且 `引擎/磁盘/历史/去重` 四行齐全)`。

122 用例绿（这一轮 +7：改名 3、路径对抗 3、命令面 3，去掉一条被替代的旧断言），clippy 0 警告。
**M0 剩余**：只有"10 GB 语料的实测数字"这一件在跑（`scan_cold` 已经出来：**20 214 条目 / 11.9 GiB
9.765 s ⇒ 1 246 MiB/s，peak heap 16.4 MiB**——原来 ts3 上那个 7 MiB/s 是启动开销主导的小样本读数，
不是吞吐上限），`commit`/`status` 的 RSS 采样器第一轮写坏了（PS 5.1 拿不到 `TotalProcessorTime`，
采到全 0），已换成 `Get-Process -Id` 轮询，跑完补录。
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
