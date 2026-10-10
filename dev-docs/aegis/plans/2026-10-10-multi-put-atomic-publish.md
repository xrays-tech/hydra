# 实施计划：控制面发布原子化（multi_put）+ 收养硬化（cas）

- **日期**：2026-10-10
- **状态**：**已实施完成（T1–T4 ✓，full gate GREEN：`.acceptance/round10-gate.log` — `OVERALL=GREEN` / `entries=99` / `GATE COMPLETE`，2026-10-10；此前 arachne 门禁 315/315——adoption 已于复审修复 F1 后接入 gate/CI 显式 `--test` 清单并由守卫防复发，此后自动复现——三进程 drill PASSED、fmt/clippy clean）**（oracle 审核修订已应用 + 复审修复 F1–F4 已应用；批次明细见 §6）
- **作者**：编排智能体（综合 lib-1「arachne-kv 0.4.0 新原语语义调研」、exp-1「hydra 控制面 KV 用法盘点」、ora-1「设计评审」三份结论；分项来源见 §8）
- **基线**：`487ff25`（main；工作区无待提交代码改动，`LOCAL_TEST_ENV.md` 为 gitignored 本地笔记）
- **对应需求**：hydra 已把控制面依赖 `arachne-kv` 从 0.3.1 升到 **0.4.0**（`c236547`，全量门禁 99/99 绿）。0.4.0 新增六个原语（`multi_put` / `cas` / `watch` / `get_stale_prefix` / `get_stale_range` / `ApplyOutcome`），当前 hydra 对其**零使用**。本计划评估并落地其中两处真实收益：**P0 发布路径 `multi_put` 原子化**、**P1 数据目录收养 `cas(NotExists)` 硬化**；`watch` / `get_stale_prefix` 收益存疑，仅列为 P2 spike，不纳入本次实施。
- **本文档的结构**：§1 Goal 与验收 是本次实施的可测判据；§2 背景（0.4.0 原语 + 现状）；§3 设计（P0 / P1 / 明确不改）；§4 决策记录；§5 风险与失败模型变化；§6 任务分批；§7 测试计划；§8 参考来源。
- **约束（来自 ora-1 设计评审，实现不得偏离）**：
  - **拒绝上游对 `multi_put` 的 "whole-tree re-set every key" 框定**——hydra 的内容寻址只写变更实体，照上游措辞做整树写会 O(tree) 过 raft、回归掉本地跳过优化；
  - `cas` 用于收养时必须走 **`without_redirect()`**，保持「仅 leader 收养」语义（`cas` 默认走 propose 转发，follower 也能转发收养，会改变失败模型）；
  - 本次**不引入** `watch` / `get_stale_prefix/range` / `ApplyOutcome`（理由见 §3.3、§4）。

---

## 1. Goal 与验收

### 1.1 P0：配置发布原子化（`multi_put`）

**Goal**：把 `publish_inner`（`arachne_store.rs:322-377`）自认非原子的 **N+2 次独立 propose**（逐实体 put → toc put → head put）收敛为**一条原子日志项**：`multi_put([未命中实体…, toc, head])`。收益 = 吞吐/延迟（少 N+1 次 leader 往返）+ 提交真正原子（关闭失败时「实体已写、head 未动」的半写窗口）。

> 如实框定：**这是吞吐/窗口收敛，不是正确性修复**——内容寻址已保证任意交错都留下完整树（`arachne_keys.rs:109-131`），现有 head-last 顺序也已崩溃安全。原子化的价值是**消除非原子窗口本身**与减少往返。

**验收（可测）**：

| # | 验收项 | 判定 |
|---|---|---|
| A1 | 一次发布 = 一条 raft 日志项 | 断言方法：用 `get_stale_with_index(ctl/head)` 的 index 增量——每次发布会话恰好 **+1**（现状为 +N+2）；未变更实体不重发，`multi_put` 只携带「未命中实体 + toc + head」（测试断言存储键/日志项数）。toc/head 不做本地存在性跳过：每次发布会话必然重写二者，实体全命中时仍写 toc+head 属可接受冗余 |
| A2 | 原子性：超限整批拒发 | 构造 **5 个各 ~1MiB 的实体**（合计 >4MiB、单值均 <1MiB——现状无批总量上限、这样能发，改造后是回归、整批拒发）→ 前置 `InvalidArgument` 拒绝、**整批不进日志（断言 `ctl/head` 的 index 不变）**、`refused` 计量递增、本地 SQLite 提交仍成立（不残留半写）。注：`refused` 映射需**新增**——当前 `publish` 的 `InvalidArgument` 经 `map_err` 记为 `"error"` 标签而非 `"refused"`；「靠下次写/`/reload` 补」仅在下一版配置重新落入上限内成立，持续超限时 503 会持续 |
| A3 | 并发双发布仍安全 | 依据 §3.1 推理（批原子 + 内容寻址）：交错发布后 head 指向完整树；`arachne_store.rs:414-442` 仅作「键不冲突」旁证 |
| A4 | head 命名即完整树 | `head` 仍为 toc 哈希、指向该批次原子写出的 toc（含全部实体条目） |
| A5 | 回归 | arachne rust in-process 测试 + 六个 arachne 集群 drill + 全量门禁全绿 |

### 1.2 P1：数据目录收养硬化（`cas(NotExists)`）

**Goal**：把 `preflight_cluster_id` 的「`get_stale` 陈旧读 → None 则 `without_redirect().put`」换成 `without_redirect().cas(HANDSHAKE_CLUSTER_KEY, NotExists, Put(expected))`（键 = `hydra/ctl/cluster_id`，见 §3.2.1），**关闭「读到 None 后被另一 cluster_id 覆写」的 TOCTOU 窗口**。

**验收（可测）**：

| # | 验收项 | 判定 |
|---|---|---|
| B1 | 竞速收养**不同** cluster_id | 两节点并发 → 恰一个成功，另一个按 mismatch 拒绝（`NotApplied{current_value}` → 既有 mismatch 分支），不出现「后者无声覆写」 |
| B2 | 竞速收养**相同** cluster_id | 都成功（幂等：`NotApplied` 但值一致**在 hydra 侧 wrapper 判为成功**、测试断言 `Ok(())` 而非库返回 `Applied`）；测试夹具 = 同一 raft 组的两个 handle 并发对同一 key 做 `cas` |
| B3 | 未收养时语义保留 | 冷启动未收养窗口仍返回 `PENDING_ADOPTION`，100 ms 轮询 / 10 s deadline 不变 |
| B4 | leader-only 语义保留（**P1 前置 spike，T3 的 gate 前置**） | `without_redirect().cas` 在 follower 上的行为先实测再动工：应不转发、按原前置语义拒绝；**若实测与预期不符，§3.2 整段要改**，验证通过才进 T3 —— **✅ 2026-10-10 spike 实测通过**（follower `Err(NotLeader)` 不转发、leader 重复 cas `NotApplied` 值不动、少数派 `Err(Timeout)` 在映射族内），已进 T3 |
| B5 | 回归 | 同 A5 |

---

## 2. 背景

### 2.1 arachne-kv 0.4.0 新增原语（lib-1，语义已对照 registry 源码 / v0.4.0 tag）

| 原语 | 语义要点 | 边限 |
|---|---|---|
| `multi_put(&[(k,v)])` | 一条命令 = 一个 log entry = 一个 session = 一次 apply；读者在单一 watermark 全见或全不见；重复键后者胜 | `MAX_MULTI_PUT_ENTRIES=4096`、`MAX_MULTI_PUT_TOTAL_BYTES=4MiB`，前置拒绝、**不分片**；返回 `()`（无逐键回执） |
| `cas(key, CasPred, CasOp)` | 谓词 `IndexEquals(i)`（oracle-locked）/ `ValueEquals(v)` / `NotExists`；失败=合法 `CasResult::NotApplied{current_index,current_value}`（非错误）；失败也占 session、可跨快照持久 | 单 key；Payload 须 ≤ `max_value_bytes` |
| `watch(prefix, limit)` | 注册即返回一致前缀快照 + 单调事件流（严格 `>`）；落后断连（有界队列 1024）；任何节点本地订阅 | 快照超 limit → `Busy` 拒绝 |
| `get_stale_prefix/range` | 单次一致快照多 key 读 + 整段 applied index | 服务端钳 10000 条；无历史 |
| `ApplyOutcome` | 写回传 apply 结果；对外仅 `cas` 显式化 | `propose_with_outcome` 是 doc-hidden 临时缝，非承诺 API |

### 2.2 hydra 控制面现状（exp-1，文件:行）

- **键体系**（`arachne_keys.rs:1-53` 模块 doc 含全键空间；各键另见 `:87`、`:105`、`:129`）：`hydra/ctl/head`（toc 哈希 = 提交点，单写者靠约定 `arachne_store.rs:3-5`）、`hydra/cfg/toc/<hash>`、`hydra/cfg/e/<path>/<hash>`（**内容寻址**，新旧版本共存，GC 是 open task）、`hydra/probe/leader`。
- **发布**（`publish_inner`，`arachne_store.rs:322-377`）：本地 `get_stale` 跳过已存实体（`:357-363, :359`）→ 逐实体 `put`（`:362`）→ `put(toc)`（`:367`）→ `put(head)`（`:373`，提交点）——**N+2 次独立 propose、非原子**（`:338-339` 注释自认「the sequence is not atomic」）。
- **读/物化**（`main.rs:467-511`、`arachne_materializer.rs:204-302`、`arachne_materialize.rs:106`）：每秒 `head_with_index` 轮询 + 手工代际门拒回滚 + 1s→60s 退避。
- **收养**（`arachne_node.rs:865-962`）：`get_stale(cluster_id)` 读 → None 则 `without_redirect().put` → `PENDING_ADOPTION` 字符串匹配 + 100 ms 轮询 / 10 s deadline。
- **管理写**：本地 SQLite 提交 → encode → `ArachneConfigStore::publish`，`reload_lock` 全局互斥（`handlers.rs:260`）；publish 失败 503、**有意不做重试队列**（`arachne_publish.rs:19-24`）。
- **0.4 新原语当前零使用**（src + tests 均为 0），测试也零覆盖（能力边界尚未被触碰）。

---

## 3. 设计

### 3.1 P0：`publish_inner` → `multi_put`（`arachne_store.rs:322-377`）

**改动**（保持内容寻址的全部既有性质，只把"多次独立 propose"合并为一次）：

1. 保留 `:357-363` 的本地 `get_stale` 跳过（决定**本次要写哪些实体**——这是内容寻址的核心收益，`multi_put` 只携带未命中实体，不做整树）；hydra 从不删除实体（`arachne_store.rs:237-240`），故「本地存在 ⇒ 跳过」与随后 `multi_put` 之间无删除竞态（`:354-356` 承重）；
2. 把 `:366-375` 的 `toc` 与 `head` 并把未命中实体集合为**一次 `multi_put([…未命中实体, toc, head])`**，作为一条原子日志项；`multi_put` 拒绝空批（`validate_multi_put`），因 toc+head 恒在、批 ≥2 恒安全——未命中实体可空（全命中），但批内必须仍含 toc+head；`record_arachne_config_bytes`（`arachne_store.rs:316`）仍按逻辑配置大小统计、语义不变；toc/head 的哈希语义不变（toc 哈希 = toc 字节内容哈希）；
3. **容量拒绝路径**：`multi_put` 前置 `InvalidArgument`（`>4096` 条 / `>4MiB`）是 fail-stop、整批不进日志。此路径必须：
   - 接到 `refused` 计量——**需新增 `InvalidArgument`→`refused` 判别**（当前 `publish` 的 `InvalidArgument` 经 `map_err` 记为 `"error"` 标签、不是 `"refused"`；计数点在 `arachne_publish.rs:76`），并订正 `cluster.md:378` 现有「超 1 MiB ⇒ refused」与代码不符的措辞；
   - 恢复现有「发布失败 ⇒ 本地 DB 已改但未提交控制面、由下次写或 `/reload` 重试」语义（对齐 ADR-0001 乙-full 写入序列）——**该补救仅在下一版配置重新落入上限内成立**，配置持续超限时 503 会持续；
   - **实现者不得静默分批**（分批会破坏原子性承诺）；超限即整批拒发并报清晰错误。
4. **更新文档断言**：`arachne_store.rs:7-12`（三步式提交叙事）、`:14-17`、`:18-29`（读侧叙事）、`:351-356` 里「head-last 顺序保证崩溃安全」的表述要随失败模型变化更新为「原子提交（multi_put），全成或全不成；head 仍为唯一提交点」（`7-12` 与 `18-29` 的三步式/读侧叙事**必须重写**，非仅改断言）。涉及 `cluster.md`（publish 指标行 + 收养段口径）与 `dev-docs/aegis/plans/2026-10-05-arachne-control-plane.md` 的验收 4（内容哈希=版本）口径核对（完整清单见 §6 T4）。

**不改变**：`reload_lock` 串行化管理写；`single-writer` 约定（`arachne_store.rs:3-5`）——`multi_put` 不带来锁，并发写靠内容寻址已安全；head 仍是 toc 哈希、仍是提交点；返回值 `()`（无逐键回执，`Ok` = 已 apply、**被接受的命令**恰好一次 session——校验失败发生在 propose 前、不占 session）。

**容量与内容寻址历史堆积的关系**：历史堆积影响**读**（前缀扫到旧版本），不影响**写**——`multi_put` 只发送本次要写的键，所以 4MiB/4096 上限与 GC 未落地**不冲突**（冲突只在「照上游整树写」时出现，已拒绝）。

### 3.2 P1：preflight 收养 → `without_redirect().cas(NotExists)`（`arachne_node.rs:865-962`）

**改动**：

1. 把 `get_stale(cluster_id)` 读后 `without_redirect().put(cluster_id)`（`:885-887`）换成 **`without_redirect().cas(HANDSHAKE_CLUSTER_KEY, CasPred::NotExists, CasOp::Put(expected))`**；目标键是常量 `HANDSHAKE_CLUSTER_KEY`（= `hydra/ctl/cluster_id`，`arachne_node.rs:831`）——**勿对 `ctl_head` 误用 cas**；
2. `without_redirect().cas(NotExists)` 的**三种走向**（前置 spike 见 §1.2 B4，T3 的 gate）：
   - `Applied` → 成功（本节点首次收养）；
   - `NotApplied{current_value: Some(v)}` 且 `v == expected` → 成功（幂等，覆盖 `B2`「同 id 都 Ok」）——**这是 hydra 侧 wrapper 约定**：库对 `NotExists` 谓词失败只回 `NotApplied`（携带 current_value），**没有**「等值即成功」的库级语义，比较在 wrapper 里做；
   - `NotApplied{current_value: Some(v)}` 且 `v != expected` → 走现有 mismatch 拒绝分支（`NotApplied`/mismatch 报错）；
   - **`Err(NotLeader | QuorumUnavailable | Timeout | Busy)` → 保留映射 `PENDING_ADOPTION` 字符串契约**（沿用 `arachne_node.rs:891-897` 现有映射）——错误不改变收养状态机，仍走轮询；注意 `Err(Timeout)` 是「结果未知」而非「确定未提交」（见 §5）；
3. **必须保留 `without_redirect()`**：`cas` 默认走 `propose_with_redirect`（`handle.rs:219`），follower 的 `redirect` 会把收养转发给 leader——这改变既有「仅 leader 可收养」语义。`without_redirect()` 返回 `Handle`（`handle.rs:117`），`cas` 在其上可用，故语义可保持；
4. 轮询-重试、100 ms 步进、10 s deadline、`PENDING_ADOPTION` 字符串契约**全部保留**（`NotApplied` 是合法结果不是错误，重试仍需存在）；
5. 收益定性：**安全增益（关闭 TOCTOU 覆写窗口），不是简化**——代码行数大致不变甚至略增，但它消除了「读到 None 后被不同 cluster_id 覆写」的竞态（ora-1 判定这是 cas 在 hydra 中唯一真实价值点）。

**不改变**：`PENDING_ADOPTION` 文本契约、`preflight` 的 10 s 冷启动收养窗口行为（`PREFLIGHT_DEADLINE=10s`）。

### 3.3 明确不改清单（有意设计，非缺失原语的补偿）

- **250 ms 写探测**（`arachne_node.rs:548-557`）：领导权 = 「能否提交写」，KV 原语答不了；已否决 `leader_hint` 方案（见 ADR-0001/D-* 记录）。
- **1 s head 轮询**（`main.rs:467-511`）：零连接状态，且被证明在少数派/丢 quorum 模式下仍可靠（`arachne_store.rs:258-282`）；`get_stale` 而非 `get` 是经 A/B 验证的正确选择。
- **手工代际门**（`arachne_materialize.rs:106`）：重启/重订阅仍需拒绝回滚，`watch` 也删不掉。
- **`reload_lock`**（`admin/mod.rs:221`）：发布串行化既存语义，`multi_put` 不改变本地发布顺序。
- **内容寻址实体键**（`arachne_keys.rs:109-131`）：承重设计，`multi_put` 沿用它（只写变更实体），**不**改整树覆盖。
- **不做重试队列**（`arachne_publish.rs:20-24`）：失败由下次写或 `/reload` 补；`multi_put`/`cas` 不改变该模型。
- **读侧 toc 哈希校验**（`arachne_store.rs:218`）：纵深防御，即便将来用一致快照读降低撕裂概率也保留。
- **`watch` / `get_stale_prefix` / `get_stale_range`**：本次不引入（见 §4 D-4），仅保留为 P2 spike 项。
- **`ApplyOutcome`**：不采纳（见 §4 D-5）。

---

## 4. 决策记录

| | 决策 | 依据 |
|---|---|---|
| **D-1** | `multi_put` 采用**「未命中实体 + toc + head」原子批**，**拒绝**上游 whole-tree-replace 框定 | 内容寻址只写变更实体是承重优化（`arachne_store.rs:342-356` 与 `arachne_keys.rs:14-35`）；整树写 O(tree) 过 raft 回归掉它 |
| **D-2** | `multi_put` 超限 = 整批拒绝、不分片；接 `refused` 计量；实现者不得静默分批 | 原子性承诺；上游前置 fail-stop、不分片。**注意**：把批超限映射为 `refused` **需要新增映射**——当前 `publish` 的 `InvalidArgument` 经 `map_err` 记为 `"error"`、不是 `"refused"`；需新增 `InvalidArgument`→`refused` 判别，并订正 `cluster.md:378` 现有「超 1 MiB ⇒ refused」与代码不符的措辞 |
| **D-3** | 收养用 `without_redirect().cas(NotExists)` | 保持「仅 leader 收养」；`cas` 默认转发会改变失败模型 |
| **D-4** | `watch` / `get_stale_prefix/range` 本次**不引入**，仅 P2 spike | 轮询零连接状态是优点；前缀读与内容寻址历史堆积冲突（需 GC 前置）；代际门删不掉 |
| **D-5** | 不采纳 `ApplyOutcome` | 对外非公共 API（doc-hidden 临时缝），无可消费公共面 |
| **D-6** | 不把 `cas(IndexEquals)` 用于序列化 publish | 与「任意节点可发布、内容寻址后胜」相悖，引入乐观锁争用与新失败分支（伪收益） |

---

## 5. 风险与失败模型变化

| 风险 | 说明 | 缓解 |
|---|---|---|
| 大配置新增「整批拒发」失败类（**回归，如实框定**） | 现状 **per-value ≤1MiB、无批总量上限**：「N 个实体各 <1MiB 但合计 >4MiB」的树**今天能发**（逐实体 put 可部分成功），改造后被**整批拒发**。且 `refused` 映射需新增（当前 `InvalidArgument` 记为 `"error"`） | `refused` 计量（新增 `InvalidArgument`→`refused` 判别 + 订正 `cluster.md:378` 措辞）+ 文档写清上限与「不分片」；「由下次写或 `/reload` 重试」补救**仅在下一版配置重新落入上限内成立**，配置持续超限时 503 会持续（需人工介入改配置） |
| `Err(Timeout)` = 结果未知 | `cas`/`multi_put` 返回 `Err(Timeout)` 时请求**可能已在 leader 上提交**——是「结果未知」，**非「确定未提交」**；据此把收养当未发生并重发 put/cas 不安全 | 收养走 cas（幂等：同值重试仍成功）；文档写明 Timeout 语义；不把 Timeout 归入「已拒绝」类计量 |
| 原子化改变失败语义 | 从「半写垃圾 + head 不动」→「全成或全不成」 | 更新 `arachne_store.rs:7-12,14-17,18-29,351-356` 断言与相关文档（见 §6 T4）；补 A2 测试钉住 |
| `cas` 收养竞态变化 | `NotApplied` 是新合法结果，代码需处理（不能当错误） | B1/B2 竞态测试；保留字符串契约 |
| `without_redirect().cas` 在 follower 的行为 | 需实测确认（不应转发、按原前置语义拒绝）——**已升级为 P1 前置 spike（T3 的 gate 前置，见 §1.2 B4）**：先验证再做 §3.2 | **若实测与预期不符，§3.2 整段要改**（先验证再动工）；兜底回退到「读后 put + 文档记录」并在门禁记录 |

---

## 6. 任务分批

| 批次 | 内容 | 依赖 |
|---|---|---|
| **T1 ✓** | `publish_inner` → `multi_put`（保留跳过 + 容量拒绝路径 + `refused` 计量 + 更新 `arachne_store.rs` 注释断言） | P0 设计定稿 |
| **T2 ✓** | P0 测试组 A1–A4（原子性/不重发/并发/head 完整性） | T1 |
| **T3 ✓** | preflight → `without_redirect().cas(NotExists)` + `NotApplied` 分支 + B 测试组 B1–B4 | T1/T2 绿 **+ §1.2 B4 前置 spike 通过（follower 行为实测符合预期，否则 §3.2 先改）** |
| **T4 ✓** | 文档同步清单（与 §3.1.4 取齐）：`arachne_store.rs:7-12`（三步式提交叙事，**必须重写**）、`:14-17`、`:18-29`（读侧叙事，**必须重写**）、`:351-356`——**四处已随 T1 重写 ✓**；`cluster.md`（publish 指标行 + 收养段口径 + §3.2 写入与提交点）；`ops.md §9.1`（告警表达式 + 转发句）；`arachne_publish.rs` 模块 doc；`dev-docs/aegis/plans/2026-10-05-arachne-control-plane.md` 验收 4（内容哈希=版本，**核对结论：口径不变、无需改**；head-last 叙述 3 处加 dated 勘误）；本计划状态；INDEX —— 全量门禁 | T2/T3 绿 |

> T1–T3 完成证据：arachne 门禁 315/315 绿（lib 284 + adoption 4 + store 10 + materializer + leader_watch 6 + three_nodes 5 等）、`integration/test_arachne_control_plane.py` 三进程 drill PASSED（gates 1/3/4/5，真实 preflight/cas 收养路径）、fmt --check clean、clippy 0 warning。
> **（2026-10-10 复审修复 F1 后注）**：adoption 4 此前是手动命令跑的——两个 arachne 入口都显式枚举 `--test` 目标，`arachne_adoption.rs` 当时缺席两份清单。现已接入 `.acceptance/round10-gate.sh` 与 `.github/workflows/ci.yml` 的 arachne 步骤，并新增守卫 `scripts/check_arachne_test_wiring.cjs`（+自测，进 gate 与 CI）防复发；**此后 315/315 由 round10 门禁自动复现**，不再依赖手动命令。
> T4 收尾证据（2026-10-10）：`bash .acceptance/round10-gate.sh` → **`OVERALL=GREEN`、`entries=99`、`GATE COMPLETE`**（99 条全 exit=0，含 `arachne rust tests (in-process)`、`arachne control plane (CI live-deps)`、`data plane under failover (acc 2)`、`startup knobs (refuse/name)`、`tenant write publish failure`、`cluster rate limits (shared+FO)`、`auth cache layers (L1+L2)`、`tenant Go SDK vs live node`、`admin-ui e2e (browser, 18180)` 等；PRE-FLIGHT `cargo fmt --check && node scripts/check_public_claims.cjs --measure --write` 先行刷新公开页测试计数 792 = 262+530）。

> P2（另立计划）：`get_stale_prefix` 一致快照读路径（前置 GC spike：量一次大配置前缀读的截断率与延迟对比）；`watch` 触发物化（live 集群验证断连重建、丢 quorum 少数派、跨领导权；**保留 1s 轮询作 fallback**，仅当能证明「代际门可删」才考虑）。

---

## 7. 测试计划（要点）

- **P0**：① 原子性/超限整批拒发（对齐 `arachne_materializer.rs:1087-1138` 既有量测，断言在 `:1104-1118`；`:1086` 是 `#[test]` 属性行）；② 未变更实体不重发（断言存储键/raft 日志项数）；③ 并发双发布（证据 = §3.1 推理：批原子 + 内容寻址；`arachne_store.rs:414-442` 降为「键不冲突」旁证，`:422` 既有用例仍可跑）；④ 原子化后 head 指向完整树。
- **P1**：① 两节点竞速不同 cluster_id → 恰一成功、另一 mismatch 拒；② 相同 id → 都 Ok（断言 `Ok(())`，「值一致视为成功」是 hydra 侧 wrapper 约定，夹具 = 同一 raft 组两个 handle 并发 cas）；③ 未收养仍 `PENDING_ADOPTION`；④ `without_redirect().cas` follower 行为——**这是 T3 的前置 spike/gate（§1.2 B4），须先于 §3.2 实施验证**。
- **回归**：`arachne rust tests (in-process)`（`--lib --test arachne_*`）+ 六个 arachne 集群 drill + `.acceptance/round10-gate.sh` 全量。

---

## 8. 参考来源

- arachne-kv 0.4.0 源码：`~/.cargo/registry/src/.../arachne-kv-0.4.0/`（`client/handle.rs`、`state_machine/kv.rs`、`server/mod.rs`）；语义与边限（multi_put/cas/watch/prefix-range/ApplyOutcome、4MiB/4096/10000 上限、watch 断连语义）详见调研记录。
- hydra 现状：`crates/hydra-server/src/cluster/arachne_store.rs`、`arachne_node.rs`、`arachne_materializer.rs`、`arachne_materialize.rs`、`arachne_keys.rs`、`arachne_publish.rs`（行号见正文）。
- 设计评审：ora-1「Hydra × arachne-kv 0.4.0 控制面评审」（六原语逐一定性、三处候选裁决、路线图）。
- 前置文档：`dev-docs/aegis/adr/ADR-0001-arachne-control-plane.md`、`dev-docs/aegis/plans/2026-10-05-arachne-control-plane.md`。
