# twosync — 需求与范围说明

版本 **v0.2**（模型重写）｜2026-10-15｜状态：待确认 §14 的 6 个开放问题后冻结 M0 范围
上一版 v0.1（"收敛/双向同步"模型）留档于 `REQUIREMENTS-v0.1.md`，差异见 §16。

---

## 0. 一句话

**一个只服务两个站点、跨 Windows/Linux、为大二进制文件优化的微型 git。**
Win10 上配好"哪些项目要同步"，Ubuntu 那端接手同一个项目、做同样的文件操作；每一端的每次改动都变成一个**不可变 commit** 进本端版本库，改动（增/改/删/改名）随 push 传播到对端。历史一定找得回来——因为真值来源是版本库，不是"扫描两个目录然后猜差异"。

v0.1 的"两个目录持续收敛"模型已被替换，理由见 §16 第一条。

## 1. 背景与真实场景

- 项目体积 2–10 GB，含大量不适合进 git 主线的二进制资产/数据/产物。
- 服务器：**1 vCPU / 6 GB RAM**，同机还跑着其他常驻项目。这是全部资源预算的硬约束来源。
- 本地 Windows 10（10.0.19045）是主要编辑端；Ubuntu 端也会直接改文件。**两端都会写**是既定事实，不是例外。
- 工作流诉求：跨 OS 搬同一份工程，改完之后能回答"这个文件上周三是什么样、是谁在哪个改动里删掉的"。
- 为什么不是 git 本体（三条都写在需求里，不是随口一说）：
  1. **巨型二进制**。git 的 pack 在 window（默认约 10 MiB）内找 delta 基对象，对二进制字节级 diff 效果很差；一个 10 GB 文件改 1 MB，git 里基本等于全量存 + 全量传。FR-5 的块级增量是本工具对 git 唯一的实质领先，也是它存在的理由。
  2. **历史不可有界**。git 的历史只增不减，`gc` 只能回收不可达对象；本工具要的是"配额内留尽量多版本"（NFR-5），git 表达不了这个意图。
  3. **跨 OS 归一化**。§8 那张表（非法字符、`\\?\` 长路径、执行位、稀疏、大小写折叠）在 git 里会天天撞 `core.autocrlf`/`core.filemode`/symlink 那类配置墙。
- 项目源码本体仍应由一个 bare git 仓库走 SSH 管；twosync 管的是**工作树里那些不该进 git 的东西**。两者不互斥，§2 第 1 条保持不变。

## 2. 非目标（明确不做，防止范围膨胀）

1. **不做 git 的替代品**。不提供分支管理、tag、rebase、交互式 merge、blame。版本库内部只有一个 `main` ref。源码协作仍走真 git。
2. **不做三方文本合并**（v1 内）。同一路径在分叉两侧都被改 → 报人，不猜（§7）。
3. v1 只支持两个站点；协议里保留 `site_id`，不实现 mesh、不实现仲裁者。
4. 不做 GUI / Web 面板。CLI + 结构化日志。
5. 不做多用户 ACL。认证直接复用 SSH key。
6. 不做 rsync 字节码兼容，也不做 git 协议兼容（思想借用，不互操作）。
7. 不自动转换 CRLF/LF。**同步是字节级的**，检出时不做任何"内容友好化"。
8. 不做实时协作/字符级 CRDT。收敛粒度是 commit。

## 3. 术语

| 术语 | 定义 |
|---|---|
| site | 一个站点（local / remote），各自持有一份独立的版本库 + 工作树 |
| root | 一个站点的同步根目录；所有路径都是相对它的相对路径 |
| worktree | root 下的真实文件树，是某个 commit 的检出结果 + 未提交改动 |
| block | 定长内容块（默认 4 MiB），CAS 的存储单元，键 = blake3(块内容) |
| blob | 一个文件内容的逻辑标识 = 有序块清单（+ 稀疏标记），不可变 |
| tree | 一个目录清单：`name → (kind, blob_hash, mode, size)`，按目录切分 |
| commit | `(parent, tree, site, ts, msg, stats)`，不可变；v1 单亲，`--keep-both` 时可双亲 |
| ref | 指向某 commit 的可变指针，每站一个 `main`；**前进只能靠快进** |
| CAS | 内容寻址块仓：`<repo>/objects/<hash[0:2]>/<hash[2:]>` |
| divergence | 两站 ref 互不可达的状态；本工具里唯一真正的"冲突" |
| session | 一次 rendezvous：自动提交 → 交换 ref → 快进判定 → 块协商 → 传输 → 检出 → ref 前进 |

## 4. 一致性模型（最重要的设计承诺，必须写进 README）

**版本库语义，不是收敛语义。** 精确承诺四条：

1. **快进不变量**：任何一次 ref 前进，新 commit 必须从当前 ref 可达。不存在"覆盖对端我不知道的历史"这条代码路径——不是拒绝，是**压根没有这个 API**。
2. **等价性判据**：两站 `main` 指向同一 commit ⇒ 两站工作树内容与元数据等价（平台固有不可移植项按 §8 明说并计入 report）。
3. **分叉绝不自动裁决**：两侧各有对方没有的 commit → 停止，只报告（§7）。
4. **不可变 + 可取回**：任何 commit 自建立起内容不再改变，其块集合可从 CAS 完整重建且 hash 匹配；GC 只删不可达块（§9.3）。

**mtime 与时钟完全不进正确性论证。** 只当"快速发现可能变了的文件"提示用（§8）。Lamport/base/`dirty` 那套游标字段已从模型里删除（v0.1 §4/§9 遗留）。

## 5. 功能需求（FR）

| # | 需求 | 验收方式 |
|---|---|---|
| FR-1 | 把工作树改动变成 commit：add/modify/delete/**显式 rename**/mode。改名由 `mv` 子命令或同 blob_hash 匹配得出，记在 commit 里，不靠事后猜 | §11 不变量 (b) |
| FR-2 | 变更检测 = `notify` + debounce + **周期全量扫描兜底**；扫描错误的后果只能是"本轮漏提交、下轮补上"，**不得**导致已提交历史丢失或工作树被删 | 拔网/杀进程/inotify 溢出后仍正确 |
| FR-3 | 对象库与索引用 SQLite（磁盘态，非内存），重启不重扫全量 | 重启后首周期只扫脏子树 |
| FR-4 | 传输复用 SSH（`russh`：exec 对端 `twosync agent --stdio`），**不新开监听端口** | 服务器 `ss -ltnp` 无新增端口 |
| FR-5 | **块级增量**：push 前做 `want/have` 块清单协商，只传对端缺失的块；默认定长 4 MiB，CDC 列为 M4 增强 | 改 10 GB 文件中的 1 MB → 传输量 ≤ 若干 MB，**且与 git 的传输量对比要写进 README** |
| FR-6 | 海量小文件批量打包（单连接多路复用 + 批帧），不把 RTT 打爆 | 1 万个 4 KB 文件初次 push 耗时可解释 |
| FR-7 | `plan` 子命令输出本次会提交什么 + 会话会不会快进，**默认不执行**；`apply` 才执行 | 任何破坏性动作都有 dry-run 前置 |
| FR-8 | 命令集：`status/log/diff/checkout/restore/commit/push/pull/plan/apply/gc/agent`；`restore <path> --at <commit>` 与 `checkout <commit> -- <path>` 必须能取回任意历史版本 | 每条有单测 |
| FR-9 | **有界历史**：块仓配额默认 ≤ root 体积 20%，超出时按"最老且不可达优先 / 同 commit 整批淘汰"回收，**不得**删掉仍被保留 commit 引用的块。淘汰顺序与保留意图（版本数 N / 天数 D）都可配 | §11 不变量 (d) |
| FR-10 | **删除熔断**（接收侧）：传入 commit 的删除条目数 > 已知条目的 20% 或删除字节 > 2 GB → 拒绝快进、退出码非 0，需 `--allow-mass-delete`。理由是防客户端 bug 把一次误 `rm -rf` 变成合法历史，不是防并发 | 清空本地 root 后 sync，对端不动并报警 |
| FR-11 | **分叉处理**（§7）：默认拒绝；出路 = `--take-local` / `--take-remote` / `--keep-both`（仅路径无交集时允许结构合并）/ 交给 git | 表格逐行有单测 |
| FR-12 | 排除规则用 gitignore 语义（`ignore` crate）；`**/.git/`、`.twosync/` 为内置强制排除（配置取消即报错） | 排除 .git 后不会污染仓库 |
| FR-13 | 崩溃一致性：块写入 → `fsync` → 才允许被清单引用；ref 更新是最后一步且原子；op 级 journal，重启后重放或丢弃半成品 | §11 crash injection |
| FR-14 | 断点续传：块级恢复 + 会话游标持久化；中断不产生半文件、不重复传已到位的块 | 传输中拔网线 |
| FR-15 | 保护同机其他进程：CPU/带宽令牌桶（`--max-cpu`/`--max-bps`），**GC 与扫描同样受限速管辖**，可选高负载退避 | NFR-2/3 实测 |
| FR-16 | `status`：本地/远端 ref 位置、ahead/behind、未提交改动数、块仓用量与配额、上次会话耗时/传输字节/CPU 峰值、分叉列表 | 指标可脚本消费 |
| FR-17 | 后台常驻：Linux systemd user unit + Windows 计划任务，日志轮转 | 重启机器后自动恢复 |
| FR-18 | 跨 OS 归一化在**检出层**实现（§8），不改内容、不"友好化" | §11 路径对抗 fuzz |

## 6. 非功能需求（NFR，数字为先验估计，M1 结束前必须换成实测值）

- **NFR-1 内存**：稳态常驻 RSS ≤ **80 MB**。索引与清单留在磁盘，靠页缓存而非堆。超出视为架构错误，不许靠加内存解决。
- **NFR-2 CPU**：单核机器上不许吃满一核。首次 10 GB 建库（全量 hash + 切块 + 落 CAS）预算 ≤ 8 min 且必须可限速；稳态增量会话 CPU 峰值 ≤ **10% 单核**。注意 CAS 化之后建库比 v0.1 的"只 hash 不搬字节"更重 IO——这条必须实测，不接受纸面数字。
- **NFR-3 IO 干扰**：读写走 buffered + 顺序大块，Linux 侧尽力 `ionice`/`ioprio_set`。**后台 GC 不得制造 IO 突发**（单核机上这是最容易被忽略的一项）。不允许出现"同步启动导致策略进程延迟抖动"。
- **NFR-4 带宽**：默认上限 = 上行探测值 80%，可硬配 `--max-bps`。
- **NFR-5 磁盘**：块仓 ≤ root 体积 20%（FR-9），超限淘汰并写日志；`status` 必须能报出"当前历史深度相当于几个全量副本"，否则配额是不是够用没人知道。
- **NFR-6 commit 生成成本**：自动提交一轮的耗时是 **O(变更目录数)**，不是 O(文件总数)。10 万小文件的工程改 3 个文件，建 commit 不许重抄全树清单。
- **NFR-7 可扩展规模**：单站 100 万文件 / 10 万 commit 内，`log`/`diff`/快进判定保持线性可预测，不出现两两比对。
- **NFR-8 启动**：冷启动到能报"有无 ahead/behind" ≤ 一次全量扫描；热启动 ≤ 1 s。
- **NFR-9 单二进制**：目标 ≤ 15 MB，Linux 侧静态 musl，无外部 runtime。
- **NFR-10 日志**：`tracing` 结构化，每条操作可溯源到 `(session_id, commit_hash, path)`。

## 7. 分叉与删除的处理（枚举完整，缺项即 bug）

v0.1 那张 16 行"相对 base 的动作为 A/B"的裁决表**整张作废**——它存在的前提是没有可信历史。现在只有四种 ref 关系：

| 本地 `main` | 远端 `main` | 判定 | 动作 |
|---|---|---|---|
| == 远端祖先 | 领先 | 我该拉 | 快进：拉缺失块 → 检出 → ref 前进 |
| 领先 | 我的后代 | 该推 | 快进：推缺失块 → 对端 ref 前进 |
| 相同 | 相同 | 已同步 | 无传输 |
| 互不可达 | 互不可达 | **divergence** | **停止并报告**，绝不自动挑一边（见下） |

**7.1 divergence 的出路**（默认第一条，即 FR-11）：`plan` 列出两侧各自的 commit、涉及路径、字节数；`--take-local` / `--take-remote` 由人挑一边（败方 commit 仍在本地库，历史不丢）；`--keep-both` **仅在两侧改动路径无交集时**生成一个双亲合并 commit（纯结构合并，不做内容合并）；同一路径两侧都被改 → 只能人挑或交给真 git 合并。

**7.2 唯一剩下的"冲突"位置**：工作树相对本地 `main` 有**未提交**改动，而检出目标要改同一路径。规则：会话开头先自动 commit 本地改动（于是工作树必然 clean）；提交不了的项（文件被占用、权限不足，§8）标 `deferred` 并**拒绝检出这些路径**，其余路径照常前进。deferred 列表必须在 `status` 里可见。这张小表是 LWW 唯一换了地方的地方，其余全在版本库层消解。

**7.3 铁律**（保留 v0.1 §7.2）：任何工作树删除在落地前，必须确认该 blob 的块已在 CAS 且已进某个 commit。做不到就拒绝这个 op，不允许"先删再想办法"。

## 8. 跨平台差异矩阵（Windows ↔ Ubuntu，最容易翻车的地方）

| 议题 | 事实 | 策略 |
|---|---|---|
| 路径大小写 | NTFS 不敏感 / ext4 敏感 | 清单键 NFC 归一化 + `/` 分隔；维护 case-folded→canonical 唯一映射。若检出会把两个仅大小写不同的文件写到 Windows 同一位置 → **报错拒绝**，绝不静默覆盖 |
| 非法字符/保留名 | Windows 禁 `<>:"\|?*`、保留 `CON/NUL/COM1`、禁尾点与尾空格 | commit 时就净化并拒绝该路径（记入 report），**不要**等到检出那端才炸 |
| mtime 精度 | NTFS 100 ns、ext4 ns、部分文件系统 2 s、网络盘更差 | 只作脏判定提示；提交前必须 hash 确认。**不参与任何正确性判定** |
| ~~时钟偏移~~ | 跨 OS 不可信 | v0.1 需要估算 skew 来做 LWW，现在**不需要**：commit 时间戳只用于显示和保留策略，不用于裁决 |
| 文件被占用 | Windows sharing violation 让打开失败 | 该项 `deferred`，本轮不提交、不检出（§7.2）；**绝不当作 delete 或 missing** |
| 符号链接 | Windows 需特权/开发者模式 | link 目标作为 blob 元数据同步；目标在 root 外 → 拒绝（§10）。无法建 link 时按 `skip \| copy-file` 降级并明写 |
| 权限位 | Unix mode vs Windows ACL | 只同步 owner 可执行位（可关）；ACL/xattr/owner 不同步，report 里明说 |
| 稀疏文件 | 大文件常带空洞 | 全零块在 blob 清单里标 `is_zero` 不占空间，检出端 `set_len` + 稀疏重建；否则 10 G 稀疏文件会被"压实"成真实 10 G，配额瞬间被吃穿 |
| 长路径 >260 | Win32 需 `\\?\` 前缀 | 统一在 path 层封装，Windows 下验证 300+ 字符用例 |
| 硬链接 | 一端多目录同 inode | v1 不识别，按普通文件各存一份引用（CAS 下只占一份字节，但会各自改名）；明确写进已知限制 |
| 换行/编码 | — | 不转换（§2 第 7 条）。跨 OS 编辑同一文本文件请交给 git，不要交给本工具 |

## 9. 协议、数据模型与安全

### 9.1 通道与消息

单条 SSH 连接上 exec 对端 agent（`twosync agent --stdio --protocol 1`），帧 = `u32 len + postcard`。认证 = SSH key（host key pinning 交 ssh 侧），不自建证书。

`Hello{proto_ver, site_id, caps}` / `BeginSession{local_ref}` / `RefState{ref, ancestors[]}`（对端自报可达祖先，用于快进判定与 `have` 采样）/ `WantHave{block_hashes[]}`（bloom 或全清单，按 caps 协商）/ `MissingBlocks{hashes[]}` / `PushBatch{stream}` / `PushCommit{commit, trees, blobs}` / `VerifyCommit{hash}` / `FastForward{from, to}` / `RejectFF{reason, mass_delete_stats}` / `EndSession{}` / `GcProposal{policy}`。

要点：块以 hash 为身份 ⇒ **重复推送天然是 no-op**，这是断点续传与崩溃重放的基石（v0.1 靠 `op_id` 达成，现在免费）。`caps` 位协商 CDC / sparse / xattr / have 表示形式。`FastForward` 前必须两侧都过 FR-10 熔断。

### 9.2 SQLite schema（草案）

```
refs(name PK, commit_hash)
commits(hash PK, parent_hash, site_id, ts, tree_hash, msg, n_add, n_mod, n_del, n_ren, bytes_logical)
trees(hash PK, dir_path_hint, encoded)                    -- 目录清单，blob 形式但键是内容 hash
tree_entries(tree_hash, name, kind, blob_hash, size, mode) -- 主索引：(tree_hash,name) PK
blocks(hash PK, size, chunk_param, refcnt, last_access_ts, pinned)
blob_blocks(blob_hash, ordinal, block_hash, is_zero)      -- 文件 → 块清单（FR-5/CAS 的核心）
worktree(path PK, kind, blob_hash, size, mtime_hint_ns, mode, symlink_target, scanned_at)
sessions(session_id, started_at, local_ref, remote_ref, ff_result, blocks_sent, blocks_recv, bytes, cpu_ms_peak, result)
sites(site_id, last_seen)
```
对比 v0.1：**删掉** `rev_site/rev_lamport/base_rev_*/dirty` 与 `history` 表（历史改由 `commits`+`blob_blocks`+`blocks` 表达，不再按站点单调计数）。

### 9.3 GC 与保留（配额的可实现路径）

保留集 = `refs` 可达 commit ∪ 按 FR-9 策略选出的更老 commit（天数 D / 版本数 N）∪ 显式 `pinned`。淘汰 = 删除整批 commit 的清单行 → `refcnt--` → 扫 `refcnt==0` 落 pending-delete 列表 → 异步 `unlink`（**不许在 decrement 现场内联删除**，否则 kill 在中间会让清单引用已消失的块）。删除动作全程挂 FR-15 令牌桶。

### 9.4 安全

1. **路径穿越**：远端提供的路径必须"NFC 归一化 → 拒绝对路径/盘符/UNC/`..` 分段 → 拼 root → canonicalize 校验仍在 root 内（含 symlink 解析后）"，任一步失败 = 拒绝整个 commit。
2. 终端输出转义控制字符/ANSI，防恶意文件名做终端注入。
3. agent 只接受同步语义消息，**没有"执行任意命令"这类消息**，不 `shell` 拼接字符串。
4. 低权限用户运行；不 setuid；`.twosync/` 权限 `0700`。
5. 配额：`PushBatch` 单帧上限 + 每会话最大字节数 + 块仓硬上限。**磁盘写满是同机其他项目的第一杀手**，而版本化之后写满的速度比 v0.1 更快，这条优先级上升。
6. 静态加密（历史里可能留有旧密钥/客户数据）见 §14 Q4；默认关，开启则 `chacha20poly1305` + 本地 keyfile。注意：**历史版本本身就是一个放大的泄漏面**，这是版本化换来的新风险。
7. 完整性：CAS 读取时按块 hash 校验，不符即报错并拒绝检出（不是静默重传，避免把损坏当缺失）。

## 10. 验收标准与测试策略（"拿得出手"就靠这一节）

1. **不变量测试（`proptest`）**：随机生成两端操作序列（add/modify/delete/rename/mkdir/symlink）+ 随机 push/pull 交错，断言：
   - (a) 两站 `main` 相等 ⇒ 两站检出树等价（排除 §8 明说的不可移植项）；
   - (b) 任意 commit 能从 CAS 完整重建，逐 blob hash 匹配；
   - (c) **非快进必被拒**——构造任何交错序列，都不得出现"一方的历史被另一方覆盖"；
   - (d) GC 后保留集仍满足 (b)，且淘汰顺序符合 FR-9 声明，用量不超配额。
2. **crash injection**：在块写入中途、`fsync` 前后、清单提交前、ref 更新前分别 `kill -9`；重启后断言：无半文件、无 orphan tmp 存活超 5 s、无"清单引用不存在的块"、不变量 (a)(b)(c)(d) 仍成立、不重复传输已到位的块。
3. **路径对抗 fuzz**：`../../`、`\\?\`、UNC、`CON`、尾空格/点、超长名、仅大小写不同的两文件、NFC/NFD 混合、含 ANSI 转义 → 全部拒绝且不崩。
4. **与 git 的量化差分**（本节取代 v0.1 的"与 `rsync -n --delete` 比对"）：同一份 10 GB 语料 + 同一串变更脚本，跑 ①本工具 ②git（含 `git add -A; commit; push`）③Syncthing 的默认结果，比 **传输字节数 / 磁盘占用增长 / 完成耗时 / 误删次数**。本工具必须在"传输字节"和"磁盘占用增长"上明确赢过 git，否则它没有存在理由——这张表是 README 的主图。
5. **真机长跑**：10 GB 混合语料（1 个大稀疏文件 + 10 万个 4 KB 小文件 + 若干二进制资产），随机变更脚本跑 72 h，记录 RSS/CPU/带宽/会话耗时/历史深度曲线。
6. **基准（`criterion`）**：全量扫描、脏判定、commit 生成（NFR-6）、块切分+hash、`want/have` 协商、清单 diff。
7. 判据：1/2/3 未全绿，**禁止发任何 release**。

## 11. 里程碑

| 阶段 | 交付 | 可验证判据 |
|---|---|---|
| **M0 骨架** | `cargo init --lib --name twosync`；CLI(`commit/log/diff/status/plan/version`)；**CAS 块仓 + blob/tree/commit 读写**；SQLite schema + 迁移；扫描器 + 变更检测 + 自动建 commit。**先不联网** | 扫 10 GB 建库，给出实测耗时/RSS/CPU（先把 NFR-1/2 变真数字）；改 1 MB 后 `commit` 成本符合 NFR-6；`restore --at` 能取回任意历史版本 |
| **M1 单向 push** | SSH 通道 + `RefState/WantHave/PushBatch/PushCommit/FastForward` + FR-10 熔断 + FR-9 配额 GC + FR-13 原子写 | 本地删 30% 文件 → 对端拒绝快进并提示；10 GB 文件改 1 MB → 传输 ≤ 8 MB；crash injection 通过；§10.4 差分表首版数字 |
| **M2 常驻** | watch + debounce + 周期兜底 → 自动 commit + 自动 push/pull；断点续传；限速（含 GC）；`status` 指标；systemd/计划任务 | 拔网重连后自动续传；稳态 CPU ≤ 10% 单核实测；重启机器自动恢复且不误提交 |
| **M3 双向 + 分叉** | 两站各自独立提交；§7 四行表全实现；`--take-local/--take-remote/--keep-both`；§7.2 deferred 机制 | 不变量 (a)(b)(c) 通过；§7 表逐行有单测；分叉场景必须停而非猜（人工验证） |
| **M4 硬化** | CDC 变长分块、稀疏重建、批传输调优、72 h 长跑、README/文档/发布 | §10.4 差分表与基准数据入库；指标写进 README |

M0–M1 **不需要**回答 §14 任何问题就能开工（单站版本库 + 单向 push 与分叉策略无关）。

## 12. 技术选型

| 用途 | 选型 | 理由 / 备选 |
|---|---|---|
| CLI | `clap` 4 (derive) | 事实标准 |
| 异步 | `tokio`，`worker_threads` 默认 1–2（为单核让路） | 备选 `smol` |
| 文件事件 | `notify` 6 + 自研 debounce/inotify-overflow | 事件不可靠是已知事实，必须有兜底扫描；现在扫描出错的后果轻多了 |
| 哈希 | `blake3` | 单核吞吐最优。**落盘 CAS 键用满 32 B**；20 B 截断只允许出现在线上传输与比对提示，不许当主键 |
| CDC | `fastcdc`（Gear） | M4 再用。注意换分块参数 = 块身份全变，`blocks.chunk_param` 现在就要留出来分命名空间，M4 补不了 |
| 对象/索引 | `rusqlite`（bundled SQLite，WAL） | 角色变更：它不再是"同步状态机"，而是 `refs` + 目录清单 + 块反查表，相当于 git 的 `refs/` + pack idx。字节内容一律不进库（在 CAS 文件里）。你熟存储引擎，这里要控制的只有 schema 与迁移纪律；若日后想换成自研引擎，M0 就划一条 `trait ObjectStore` 边界 |
| 排除规则 | `ignore` crate | gitignore 语义，零学习成本 |
| 传输 | `russh`（exec 复用系统 sshd） | 备选 `ssh2`（libssh2，同步 API 要包一层） |
| 序列化 | `postcard` | 比 bincode 省、无版本耦合坑；在 `Hello` 里协商 |
| 日志 | `tracing` + `tracing-subscriber`（JSON file + 轮转） | |
| 限速 | 自研令牌桶（`tokio::time`），CPU/带宽各一桶，**GC 与扫描共用** | 必须自研：要能随负载退避 |
| 测试 | `proptest`、`criterion`、`cargo-fuzz`（路径层） | §10 的地基 |
| 工程 | `cargo xtask` 或 `justfile` | 不依赖托管 CI 也能一键跑全套验证 |

## 13. 命名

- 目录/crate：`twosync`。模型换成 git-for-files 后这名字不算贴切（`twosync` 暗示同步而非版本库），但目录里还没有一行代码，现在改名成本仍是 0。候选：`twogit`/`filevcs`/`crossbox`。
- 子命令：`twosync commit` / `push` / `pull` / `status` / `log` / `diff` / `checkout` / `restore <path> --at <commit>` / `gc` / `plan` / `apply` / `agent` / `config`。
- 数据目录：`<root>/.twosync/`（`objects/` CAS、`index.db`、`journal/`、`log/`）。

## 14. 待决问题（回答后本文档升 v0.3 并冻结 M0 范围）

1. **自动 commit 的粒度**：每次静默期结束就提交（Syncthing 感，历史很碎）/ 每 N 分钟合并成一个 / 只有显著变更才提交？→ 决定 10 GB 工程的历史深度和 `log` 的可读性，也决定 FR-15 的唤醒节奏。
2. commit message 谁写：自动生成 `sync: 12 add / 3 mod / 1 del`，还是允许给某些路径打标签？
3. 2–10 GB 的构成：文件数量级、最大单文件、小文件占比 → 决定块参数（4 MiB 是否合适）、NFR-5 配额、`want/have` 用 bloom 还是全清单。
4. 历史里可能留有旧版本密钥/客户数据，**需要静态加密吗**（§9.4.6）。版本化让泄漏面变大，这条比 v0.1 更该认真答。
5. 除 `.git/` 外必须内置排除什么（venv/`node_modules`/数据库热文件/锁文件/正在运行的二进制）？→ 进 FR-12 强制清单。特别地：**SQLite 自己的 WAL/临时文件若在被同步的 root 里，会产生物理不一致的版本历史**，这条要单独确认。
6. 那台 1 核 6 G 上是否有对延迟敏感的常驻进程（策略/交易类）？→ 决定 FR-15 退避与 NFR-3 是否成为 M1 阻塞项。

（v0.1 的 Q1「服务器侧写入是程序产出还是人编辑」已不再是阻塞项——版本库模型天生支持两端都写，只有 M3 的合并范围受它影响。）

## 15. 已知限制（README 必须原样列出）

1. 单 ref，无分支；跨站协作仍需真 git。
2. 同一路径分叉只能人选一边，不合并内容。
3. 硬链接不识别（§8）。
4. 不转 CRLF/LF，不做文本友好化。
5. 只两站。
6. Windows 侧 symlink 需开发者模式或特权，否则按 `skip/copy-file` 降级。
7. 配额内历史会被 GC——**它不是备份**，需要长期留存请另开 bare git 或对象存储。

## 16. 修订记录（v0.1 → v0.2）

模型从"两目录持续收敛（Syncthing 型）"改为"两端各自版本库 + 快进传播（git 型）"，依据用户 2026-10-15 的表述："我就是想做一个能直接在两个操作系统之间传输文件的 git"，并明确"版本都要记录下来，不然会乱、历史会找不到"。

- **删除**：v0.1 §4 收敛一致性 + Lamport/base 游标；§7 十六行冲突裁决表（含 LWW、删改冲突、双改名、`.twosync-conflict` 文件命名）；§8 时钟偏移行；FR-10 策略项 `conflict.delete_vs_modify` / `both_modify`；FR-17 改名启发式；§9 `history` 表与 `rev_*`/`dirty` 字段。
- **保留但改理由**：FR-9 → FR-10 删除熔断，从"防并发误删"改为"防客户端 bug 把误 `rm -rf` 变成合法历史"；v0.1 §7.2 铁律 → §7.3 原样保留。
- **新增**：§0/§1 为何不是 git；§4 快进不变量；§7.2 残留冲突（工作树未提交 × 检出）；FR-8 命令集；FR-9/GC 与 §9.3 保留策略；§9.2 对象模型 schema；§10.4 与 git 的量化差分（取代与 rsync 比对）；NFR-6 commit 生成成本；§15 已知限制；§14 Q1/Q2 自动提交粒度。
- **升级**：§9.4.5 磁盘写满风险（版本化后写得更快）；§9.4.6 静态加密（历史 = 新泄漏面）。
- **§11 里程碑**：M0 从"出 plan"改为"能建库、能取回历史"；M3 从"实现 §7 全表"改为"分叉处理"；M0–M1 现在完全不依赖 §14。
- 未改动：NFR-1/2/3/4/8/9 的资源数字与硬约束、FR-4 SSH 复用、FR-5 块级增量、§10 安全 1–4、非目标 1–8（仅第 2 条措辞随 §7 调整）。
