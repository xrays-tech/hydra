# 实施计划：用 Arachne 取代 Redis 控制面（Arachne Control Plane）

- **日期**：2026-10-05
- **状态**：**Phase 0 硬门已通过**；**上游按反馈修复后已复核通过**（`bce2943`，见 §「上游修复复核」，五条全落实并带来三处简化）；六条决策 + 新增 D-6 待 oracle 复核；下一步 = **ADR + oracle 架构复核**，随后按 Phase 1 开工
- **作者**：编码智能体（DeepSeek）
- **基线**：`8b7b3c6`（main，工作区有未提交改动，见 §BaselineUsageDraft）
- **对应需求**：把 Hydra 集群的控制面（leader 选举 / 节点注册表 / 配置分发 / 管理写路由）从「Redis 租约 + 注册表 + HTTP 快照轮询」换成 [Arachne](https://xrays-tech.github.io/arachne/quick-start.html)（raft 线性化 KV，[仓库](https://github.com/xrays-tech/arachne)，crate `arachne-kv` 0.1.1）。数据面热路径（限流 / 熔断 / 认证 L2 / 失效总线）**留在 Redis**。
- **本文档的结构**：§决策已定稿 是用户已裁定的六条（实现不得偏离）；§决策记录 是需要在 ADR 里固化的架构决策；§任务分批 是实施工序。
- **权威输入**：Arachne 文档（`quick-start` / `tutorial` / `api-reference`，本机副本见 §附录 A）、Arachne 源码 checkout（`xrays-tech/arachne` main，`dev-docs/propsol-v0.2.md`）、`dev-docs/cluster.md`、`dev-docs/design.md` §20。

---

## Goal

1. **唯一权威**：控制面状态（配置版本、配置内容、集群成员）的权威从「每个节点本地 SQLite + Redis 租约」收敛到 **Arachne 的 raft 日志**；
2. **消灭租约时间栅栏**：`leader_ready` 从「Redis 租约未过期」变成「本节点是 raft leader」，故障切换窗口从实测 **11–18 s** 降到选举超时（LAN profile `1 s`）；
3. **单写者由共识保证，转发由库负责**：写可以打在**任意节点**，库自己把请求转发到 leader（2026-10-05 上游 `bce2943` 起，实测 p11/p12）；Hydra 侧**不做**跨节点转发，`cluster/forward.rs`、注册表解析、`x-hydra-forwarded` 护栏整体退役，admin UI 也**不需要**重试层；**唯一例外**是租户自助写（见 D-6：它需要 leader 侧重鉴权，因此保留 Hydra 内部转发这一条窄路径）；
4. **节点同构**：集群里**每个节点完全一样** —— 都跑数据面、都跑管理面、都是 raft 成员、都能当 leader。**`edge` 角色与「无状态数据面」一起退役**；配置不再跨节点推送，改为每个节点各自从 Arachne 物化本地状态；
5. **opt-in 不变**：单节点模式（未配集群环境变量）**零外部依赖零行为变化** —— 不启动 Arachne，也不需要 Redis；
6. **留 Redis 在数据面**：限流（Lua 滑动窗口）、熔断投票、认证 L2、失效事件流**不迁移**（用户已裁定）。

**验收口径（可测）**：

| 序号 | 验收项 | 判定 |
|---|---|---|
| 验收 1 | 3 节点集群 `SIGKILL` 当前 leader | 新 leader ≤ 3 s，`/healthz/leader` 在旧节点 503、新节点 200；管理写**打在任意节点**都能成功（库内转发），无 leader 窗口内返回 503 且不挂起 |
| 验收 2 | 故障切换期数据面 | 任取一个存活节点持续 20 rps ≥ 60 s：0 个非 200、0 次连接被拒（沿用既有 HA 演练口径） |
| 验收 3 | 双写不可能 | 任意时刻 `/healthz/leader` 恰好一台 200（分区测试：多数派侧 200、少数派侧 503） |
| 验收 4 | 配置收敛 | 管理写后所有节点物化出**同一内容哈希**的 `ConfigData`（哈希就是版本，见 F-2）；且 `head` 指向该哈希 |
| 验收 5 | Arachne 失多数派 | 管理写 503 fail-closed；**数据面不受影响**（各节点用本地已物化状态继续服务） |
| 验收 6 | 单节点回归 | 不配集群变量时：无 Arachne、无 Redis、既有全量测试不变绿 |
| 验收 7 | 退役 | `grep -rn "LEASE_KEY\|hydra:{nodes}\|NodeRole::Edge" crates/` 零命中；`grep -rn "forward_mutation\|forward_config_write" crates/` 零命中（服务端零转发） |

---

## 架构 / Architecture

```
Hydra 集群 = N 个同构节点（N ≥ 3 且为奇数；每个节点跑全部三种角色）

│  节点 A（完全相同）            │  节点 B（完全相同）            │  节点 C（完全相同）        │
│  ├ 数据面   :8080              │  ├ 数据面   :8080              │  ├ 数据面   :8080          │
│  ├ 管理面   :8081              │  ├ 管理面   :8081              │  ├ 管理面   :8081          │
│  ├ 本地 SQLite + 已物化 Config │  ├ 本地 SQLite + 已物化 Config │  ├ …                       │
│  ├ Arachne raft 成员（WAL 落盘）│  ├ Arachne raft 成员           │  ├ Arachne raft 成员       │
│  ├ 非 leader：管理写回 409+提示 │  │     ↓ 多数派提交             │  │                          │
│  └ 读：本地已物化状态（零往返） │  └ 读：本地已物化状态          │  └ 读：本地已物化状态      │
└───────────────┬────────────────┴───────────────┬───────────────┴────────────┬──────────────┘
                └──────────── raft / tonic（选举 + 日志 + 提交点）─────────────┘
                                  配置物化路径（每个节点各自完成，无推送）
                     Arachne ctl/head + 分片 → 解码 → 本地 SQLite → 内存 ConfigData

Redis（只是数据面的加速器与近似计数器，不是任何权威）
  限流 Lua 窗口 · 熔断投票 · 认证 L2 · 失效事件流（Streams）
```

**权威归属（每个状态只有一个 owner）**：

| 状态 | 旧 owner | 新 owner | 备注 |
|---|---|---|---|
| leader 身份 | Redis 键 `hydra:{lease:leader}` | **raft 领导权**（Arachne） | 租约续约 / 时间栅栏整体退役 |
| 集群成员 | Redis 注册表 + 心跳 TTL | **Arachne `membership()` + 静态地址表** | 成员变更是显式运维操作（`add_learner`/`promote`），不是自动扩缩 |
| 配置版本 | leader SQLite `config_meta.config_version` | **Arachne `ctl/head`（version）** | SQLite 降级为「本地物化状态」 |
| 配置内容 | 各节点 SQLite → HTTP 快照推送 | **Arachne 分片 blob（每节点自读自物化）** | 密文原样存放，密钥仍是 `HYDRA_ENCRYPTION_KEY` |
| 写路径的 leader 路由 | 注册表 `active_leader_url()` + 服务端转发 | **库内转发**（`Handle` 在 `NotLeader` 时经 Forward RPC 转发，会话表保证幂等） | 注册表解析、`cluster/forward.rs`、forward-once 护栏全部退役；**唯一保留**：租户自助写的内部转发（D-6，为 leader 侧重鉴权） |
| 失效广播 | Redis Streams | Redis Streams（**不变**，用户已裁定） | 见 §决策记录 D-1 |
| 限流/熔断/L2 | Redis | Redis（**不变**） | 见 §决策记录 D-1 |

### Arachne 键空间（`crates/hydra-server/src/cluster/arachne_keys.rs`，新增）

**用 key-path（版本化目录 + 每实体一键）而不是把配置切成字节分片** —— 原因见下方「为什么不是字节分片」。

| 键（key-path） | 值 | 写入时机 |
|---|---|---|
| `hydra/ctl/cluster_id` | `hydra:<cluster_id>` | 首启一次性；不匹配即 fail-fast |
| `hydra/ctl/format` | `1`（u32 LE） | 首启；读侧不匹配即拒绝启动（防混版） |
| **`hydra/ctl/head`** | **一个 toc-hash**（`[u8;32]` 的十六进制）—— **没有版本号字段** | 每次配置变更**最后**写（**唯一提交点**） |
| `hydra/cfg/<toc-hash>/toc` | 目录索引：`format` + 实体键列表（`[<path>, <content-hash>, <len>]`） | 提交前写；**一次写一个值**（见下方「目录索引的规模」） |
| `hydra/cfg/<toc-hash>/tenant/<tenant_id>` | 该租户的编码字节（密文原样） | 该实体变化时写 |
| `hydra/cfg/<toc-hash>/provider/<provider_id>` | 同上 | 同上 |
| `hydra/cfg/<toc-hash>/provider_key/<provider_id>` | 同上 | 同上 |
| `hydra/cfg/<toc-hash>/model/<model_key>` | 同上 | 同上 |
| `hydra/cfg/<toc-hash>/limit_role/<id>` | 同上 | 同上 |
| `hydra/cfg/<toc-hash>/key_binding/<id>` | 同上 | 同上 |
| `hydra/cfg/<toc-hash>/sub_tenant/<id>` · `.../sub_tenant_route/<id>` | 同上 | 同上 |
| `hydra/cfg/<toc-hash>/token/<tenant_id>` | 同上（租户访问令牌哈希索引） | 同上 |
| `hydra/cfg/e/cert/<domain>` | `CertTreeEntity`：`CertMeta` + **密封的私钥**（`skip_serializing` 的那个字段必须有人补） | 同上 |
| `hydra/cfg/<toc-hash>/meta` | 单例：`config_version`、`writer_node`、`created_ms`、`entity_count` | 每次提交 |
| `hydra/ctl/node/<node_id>` | `{listen_addr, raft_id, admin_addr, heartbeat_ms}` | 每节点每 10 s（**仅诊断 / 管理 UI 展示**，不是发现权威 —— 地址的权威是 `HYDRA_CLUSTER_PEERS`） |

> **键布局在 T2.2 改过一次，以上表格是方案定稿时的写法**：实际落地为
> **toc 按哈希取名**（`hydra/cfg/toc/<toc-hash>`）、**实体按「路径 + 内容哈希」寻址**（`hydra/cfg/e/<path>/<content-hash>`；2026-10-05 从纯路径改成这样，理由见下方 T3.2 记录），
> 因为实体只在内容变化时才写——「没变的实体」留在旧树哈希下，新树就找不到它（实测报错
> `tree <h> is missing entity tenant/acme`）。`EntityPath` 段名见 `arachne_keys.rs`。
> 另外 `meta` 与 `token` 两个实体**至今没有生产者**（`grep EntityPath::Meta|EntityPath::Token` 在
> `arachne_keys.rs` 之外零命中）：`config_version` 的权威在 T3.2 才切换，令牌哈希则走了 fidelity 实体。
> 二者在解码侧是**拒绝**而不是忽略（未知路径 ⇒ 报错），所以树里一旦出现它们就说明读到了别的版本的树。
> 表里未逐行改写以免掩盖差异，改的是上面这两处注释与下方 T3.1 记录。

**读取顺序**：`get_stale(head)` → `get_stale(<hash>/toc)` → 逐个 `get_stale(<hash>/<path>)` 并**按目录里的 content-hash 校验**；任何一项对不上 ⇒ 整轮重试（**不**使用 `get`，见 F-2）。

### 为什么不是字节分片（设计理由，回应「用 key-path 定义存储结构」）

| 方案 | 一次小改动的写入量 | 问题 |
|---|---|---|
| **整体 blob + 字节分片**（本计划上一版） | 因为分片边界按字节切，插入/删除一个实体后面**所有分片的内容都移位** ⇒ 哈希全变 ⇒ 几乎全量重写 + 全量写日志；配置越大越糟 | 写入放大与配置规模成正比 |
| **每实体一键 + 版本化目录**（本版） | **只写真正变化的实体**（其余实体在新目录里复用同一内容，键路径换了但内容相同；连值都不用重发——实现上按 content-hash 跳过 `put`） | 需要一份目录索引；需要 GC 旧目录 |

**「写入后不通知其它节点」这一点怎么处理**：Arachne 确实没有 Watch/订阅（上游把 Watch 排在 v2），所以**发现变更只能靠轮询**。本方案的模型正是为此设计的：

1. 每个节点以 1 s 周期读 `hydra/ctl/head`（一次 `get_stale`，本地读，无网络往返）；
2. head 未变 ⇒ 立即返回（稳态零成本）；**这就是取代订阅的全部机制**；
3. head 变了 ⇒ 读 `<hash>/toc` 与实体键，**全部是键路径寻址的独立小值**，不存在"读半份配置"；
4. 提交点仍然是 head 的**单键写入** ⇒ 结构化的 key-path 没有牺牲原子性；
5. 传播延迟实测 ≤7 ms（p13），轮询周期 1 s 足够。

**目录索引的规模与边界**：toc 是**一个值**，Arachne 单值上限 1 MiB。按每条实体索引 ~40 字节算，1 MiB 可容纳约 **2 万个实体**（每类合起来算）；超过时按 `toc/<shard>` 拆成固定几段（例如按实体类型分段，类型数固定、无需扫描），head 仍指向同一个 hash。**启动时必须校验 `entity_count` 与 toc 分段一致性**，不匹配即拒绝服务管理写。

**GC（旧目录）**：新 head 提交成功后，异步任务遍历**上一个** toc 并删除其中不在新 toc 里的键（未变化的实体键在两个目录里 hash 相同、内容相同，可以不删）。策略：1 s 后延迟执行（让慢节点读完旧版本），失败重试；`hydra_arachne_gc_deleted_total` 计数。**不需要用户确认**（纯派生数据，删错也只是让落后节点重读一份）。

> **节点名是字符串，raft id 由位置派生**：`HYDRA_NODE_ID` 是任意字符串；Arachne 用它在 `initial_cluster` 里的**下标 + 1** 得到数字 raft id（`assemble_cluster` 实测如此）。因此 `HYDRA_CLUSTER_PEERS` 的**顺序在集群生命周期内不可变**（改顺序 = 换集群身份），这一条要写进 `ops.md` 的「禁止事项」。

**为什么 head 是哈希而不是版本号（实测 F-2 的后果）**：`get` 在 follower 上不可用（p7），所以所有读取都是 `get_stale`（可能陈旧）。用「版本号」就需要一条"先读版本、再按版本读分片"的序列，而 `get_stale` 不保证单调 ⇒ 两个读之间可能读到**不同版本**的分片，拼出一份从未存在过的配置。改成**内容哈希**后：head 与分片的对应关系由哈希自证，节点读到哪一版就是完整的一版；读到旧版只会**延迟**一次物化，不会拼错。

**提交点语义**：先写分片（同 hash 幂等），最后写 head。head 指向新哈希 ⇒ 该配置对全集群可见；head 写失败 ⇒ 没有任何节点会看到半份配置。

**leader 唯一性**：Arachne 没有 CAS，head 的写入由 raft 的 leader 单点执行（`put` 在非 leader 上返回 `NotLeader`）；因此**任何绕过 leader 的写路径都必须被禁掉**。

### leader 提示（**读**用，不是重定向机制）

> **本节已按 D-3 定稿改写（2026-10-05）**：初稿把 `leader_hint()` 当成「admin UI 自动重试」的地基
> （响应头 `x-hydra-leader-hint` + 409 错误体里的 `leader=<id> addr=<host:port>`）。上游 0.1.2 之后
> **写可以打在任意节点、库自己转发**，于是那套机制的需求本身消失了：响应头、409 契约、UI 重发层
> **都不存在**（核实：全仓 grep `leader-hint` / `x-hydra-forwarded` 在服务端与 UI 均无实现）。

- Arachne 的 `leader_hint() -> Option<(NodeId, SocketAddr)>` 返回的是**该节点监听地址**上的 leader；实测（p1/p2）：**follower 上的提示是可靠的**，交接后 ≤1 s 跟上，且与「谁真的能写」一致；
- 实测（p1）：**新 leader 自己的提示不指向自己**（40 s 窗口内始终不是），所以提示**只用于 follower 找 leader**，不用于自判（见 T1.3 的写探测）；
- **今天它只用于「显示」**：`/api/v1/cluster/status` 的 `lease_holder`（舰队视图与 admin UI 的"你不在 leader 上"横幅）由控制面 250 ms 巡检缓存下来的提示提供。它**不参与任何写路径**；
- ~~约定：`HYDRA_ARACHNE_LISTEN` 的端口 = 管理口端口~~ **该约定已作废**：它当时存在只是为了让"提示里的地址"能直接当管理端点用，而既然没有东西要用它当管理端点，就不需要这条约束了——`integration/test_arachne_control_plane.py` 用的就是**不同**端口（raft 与 admin 各自独立），三个清单同理。

### 配置分发：改成「每个节点自己物化」（不再有跨节点推送）

所有节点同构且都在 raft 里，于是旧的「leader 组装快照 → HTTP 推给 edge → edge hydrate」这条链整体退役：

1. leader 提交管理写：本地 SQL 事务 → 重建 `ConfigData` → 编码 → 写 Arachne 分片 → 最后写 `ctl/head`（提交点）；
2. 每个节点（含 leader）一个物化任务：读 `ctl/head` → 版本前进则读 toc → 逐实体读 → 解码 → 写回**本地** SQLite → 换入内存 `ConfigData`；
   - **实现时的实测修正（2026-10-05，0.1.2）**：原文写的是「顺序检查必须走 `get` 线性读」，**这条已被证伪**——follower 上的 `get` **立刻**返回 `QuorumUnavailable`（`bce2943` 的转发只对进程内 `register_peer` 生效），所以**任何节点都不能用 `get`**，只有 `put` + `get_stale`。见 ADR-0001 §10 F-2 与 `arachne_store.rs` 的模块注释（`read` 全程 `get_stale`）。
   - 由此产生的第二个结论：**`head` 不是靠线性读来保证的，而是靠「单写者 + 内容哈希」**——`ctl/head` 只由 `ArachneConfigStore::publish` 写，且 head 指向的 toc 里每个实体都带内容哈希、逐个校验；读到「旧 head」是允许的（下一轮再读），读到「head 与 toc 不一致」是重试而不是修复。
3. 每个节点对自身数据面读内存态 `ConfigData`，零网络往返；Arachne 失多数派时继续用本地已物化状态服务（验收 5）。

> **退役**：`GET /api/v1/internal/snapshot`、`cluster/control_client.rs`（轮询 / 轮换）、`cluster/snapshot.rs` 的线上推送路径。`content.rs` 的编解码**保留**（它现在服务于「编码成 Arachne 值」与「从 Arachne 值解码」）。

---

## 技术栈 / Tech Stack

| 组件 | 版本 / 事实 | 影响 |
|---|---|---|
| Rust 工具链 | Hydra pin `1.98.0`；Arachne `rust-version = 1.93`、edition 2024 | ✅ 兼容，**无需升级工具链** |
| `arachne-kv` | crates.io **0.1.1**；默认 feature `transport-tonic`；`default-features = false` 时为「零外部 crate」精简核心 | 依赖图新增：tonic / hyper / tokio / rustls / protobuf / slog / `raft-seedable` 0.7 |
| Arachne 状态机 | **全内存 + WAL + 快照**（`propsol-v0.2.md` D1） | 全部配置常驻**每个**节点内存（同构后没有「只读副本」这条路可退）；受 `max_value_bytes`/`max_key_bytes`/容量上限约束 |
| Arachne 上限（LAN profile） | `max_value_bytes = 1 MiB`、`max_key_bytes = 4 KiB`、`proposal_queue_bytes = 64 MiB`、`snapshot_threshold_bytes = 64 MiB`、`session_ttl = 60 s` | 分片 256 KiB 取 1/4 余量；**启动时校验配置总量 ≤ 内存预算**，并且每个节点都要过这一关（同构的代价） |
| tonic / protobuf 版本 | 与 Hydra 现有依赖（pingora 0.8 / reqwest 0.12 / rustls）是否重复条目 | 待 Phase 0 实测（体积 + 编译时间 + TLS crate 重复） |
| redis 依赖 | 不动（`fred` 10，feature `cluster-redis`） | 数据面保留 |

---

## Baseline / Authority Refs

| 类型 | 引用 |
|---|---|
| 本次会话已直读 | `dev-docs/cluster.md`（全文）、`crates/hydra-server/Cargo.toml`、`crates/hydra-server/src/cluster/{mod,lease,control_client}.rs` 头部、`cluster/{content,snapshot}.rs` 关键结构、`admin/mod.rs:265-290`（`leader_ready` seam）、`redis/mod.rs` + `cluster/{events,forward,registry}.rs` 调用点清单 |
| Arachne 事实 | 文档三页（quick-start / tutorial / api-reference）+ 源码 checkout 的 `arachne/src/{server/mod.rs,client/handle.rs,runtime/mod.rs,state_machine/kv.rs}` 与 `dev-docs/propsol-v0.2.md` |
| 项目铁律 | `dev-docs/dev-plan.md`（铁律 1 TDD / 铁律 2 零 mock / 铁律 3 terminate-in-Pingora） |
| 主设计 | `dev-docs/design.md` §20（集群模式章节） |
| 运维 | `dev-docs/ops.md` §13（集群运维）、§9（可观测性） |
| 既有计划格式先例 | `dev-docs/aegis/plans/2026-09-18-sub-tenant-v2.md`、`2026-09-29-oracle-remediation.md` |

```text
BaselineUsageDraft:
- Required baseline refs: dev-docs/cluster.md（§1 角色 / §2 环境变量 / §3 共享状态 / §5 故障切换实测）;
  crates/hydra-server/src/{cluster/*,redis/*,admin/mod.rs}; dev-docs/ops.md §13; Cargo.lock（依赖图）
- Delivered context refs: 本次会话已直读 cluster.md 全文、Cargo.toml（两 crate）、cluster 模块全量文件清单与行数、
  lease/control_client 公共 API、admin/mod.rs 的 leader_ready 契约、forward/registry/events 的调用点
- Acknowledged before plan refs: Arachne 文档三页 + 源码 checkout（server/client/runtime/state_machine 四文件）+ propsol D1/D3
- Cited in plan refs: 见每个 Task 的 Verification 与 §附录 A（Arachne 事实表）
- Missing refs: crates/hydra-server/tests/ 下集群相关用例清单的逐文件确认（由 T0.3 产出）;
  `integration/test_cluster_ha.py` / `test_cluster_limits.py` 的逐行口径（只做口径复用，不改断言语义）;
  `tenant_config/forward.rs` 的租户写路径逐行确认（D-6 的前置，取代原先"admin-ui 请求层"这一条 —— 上游修复后 UI 不再需要改动）
- Decision: continue（缺项不阻塞 Phase 0；T0.3 与 T3.3 各自的前置清单必须在对应 Phase 开始前落地）
```

---

## 兼容边界 / Compatibility Boundary

**没有既有生产版本**（用户确认），因此不保留任何兼容层、别名或双轨期 —— 直接改成一致的一套，代价是文档/清单/脚本必须**同步改干净**（由 `scripts/check_cluster_env.cjs` 双向核对兜底）。

| 面 | 边界 | 处理 |
|---|---|---|
| 环境变量 | 名字统一成一套，旧名**直接删除**（不留别名、不静默忽略） | **新增**：`HYDRA_CLUSTER_PEERS`、`HYDRA_ARACHNE_LISTEN`、`HYDRA_ARACHNE_DATA_DIR`；**改名**：`HYDRA_NODE_ID` = 集群唯一字符串 id，在 `HYDRA_CLUSTER_PEERS` 里必须恰好出现一次；**删除**：`HYDRA_ROLE`、`HYDRA_CONTROL_URL`、`HYDRA_PUBLIC_URL`、`HYDRA_LEADER_LEASE_MS`、`HYDRA_CONTROL_POLL_MS`、`HYDRA_REGISTRY_STALE_GRACE_SECS`、`HYDRA_FAILOVER_GRACE_MS` |
| 集群判定 | 从 `HYDRA_ROLE` 改为 **`HYDRA_CLUSTER_PEERS` 是否存在** | 未配 ⇒ 单节点模式（不启动 Arachne、不要求 Redis）；配了 ⇒ 集群模式（全部变量 fail-closed 校验） |
| 角色语义 | `all` / `leader` / `edge` 三值**删除** | 所有节点同构；拒绝任何残留的角色变量（启动即报错并点名） |
| HTTP 契约 | `/healthz/leader`（200/503）保留；`/api/v1/internal/*`（cluster token）保留 | 新增 `x-hydra-leader-hint` 响应头 + 409 错误体里的 `leader=<node> addr=<host:port>`（给 admin UI 自动重试用） |
| 数据面 | 单节点与集群下都在本节点服务 | 退役「edge 无 DB / 无管理 CRUD / 无 raft」这一整套分支 |
| 管理面 | 单节点行为零变化 | 集群下写可打在任意节点（库内转发）；无 leader 时 503（`QuorumUnavailable`/`Timeout`）。**admin UI 与 CLI / SDK 都不需要重试层**（这是 `bce2943` 之后相对原设计的简化）；错误码表补一行「集群不可用 ⇒ 重试」 |
| 持久化 | SQLite **降级为本地物化状态**（不是删除，schema 与 CRUD 全部保留） | 每个节点比对本地记录的「已物化内容哈希」与 `head`：不一致则重放物化（这是**正常恢复路径**，不再是异常）；物化反复失败则拒绝服务管理写并报 ERROR |
| 依赖来源 | ✅ **已解决（2026-10-05）**：上游发布 **`arachne-kv 0.1.2`**（crates.io，稀疏索引确认），携带全部修复 | T1.1 直接 pin `arachne-kv = "0.1.2"`；**不要**用 0.1.1（旧行为：follower 写不转发、无运行时启动 panic、新 leader hint 不自指） |
| 单节点 | 完全不受影响 | feature `arachne` 不开、`HYDRA_CLUSTER_PEERS` 未设 ⇒ 无 Arachne 代码路径被执行 |

---

## TDD Route

```text
TDD Route:
- Mode: auto
- Decision: strict（Phase 1–3 的核心语义面）；light（Phase 0 探针与文档）
- Strict authority: 项目铁律 1（dev-plan.md「TDD：先写测试（真实断言）再实现」）
- Test posture: strict RED test（选举/单写者/收敛）+ diagnostic reproduction（Phase 0 实测）+ post-change regression（门禁）
- Reason: 本工作改动的是「谁是权威」与「谁能写」，错误会以脑裂/丢配置的形式出现，
  只能由真实多节点集成用例（非 mock）证明；Aegis 铁律 2 禁止 mock 替代。
- Verification: 每 Task 的 Verification 段；最终 §验收门禁（验收 1–7）
```

---

## 前置检查 / Gates

```text
Change Necessity:
- User-visible need: 集群故障切换窗口 11–18 s、租约/注册表/转发三层护栏、脑裂面（同 HYDRA_NODE_ID）
  都是运维可见的缺陷类；共识层可以把它们整体消掉。
- No-change / non-code option: 不成立 —— 现状不是配置问题，而是「没有共识层」这一结构缺口。
- Why code change is necessary: leader 唯一性目前由 Redis 租约的时间栅栏近似，任何租约/时钟/身份
  抖动都直接变成双写窗口；只有共识（raft）能给出「至多一个 leader」的硬保证。
- Minimum change boundary: `crates/hydra-server/src/cluster/*` + `main.rs` 引导分支 +
  `admin/mod.rs` 的 `leader_ready` 注入点 + admin UI 的请求层 + 部署清单；不改 proxy/tenant_api/sink 的业务逻辑。
- Decision: code-change
```

```text
Existence Check:
- Proposed new surface: Arachne raft 节点 + 控制面键空间 + 3 个环境变量（`HYDRA_CLUSTER_PEERS` / `HYDRA_ARACHNE_LISTEN` / `HYDRA_ARACHNE_DATA_DIR`）
- Existing owner / reuse candidate: Redis 租约（`cluster/lease.rs`）+ 注册表（`cluster/registry.rs`）
  + 快照轮询（`cluster/control_client.rs`）
- Why existing surface is insufficient: 三者都建立在「Redis 是单点可靠协调者 + 时钟可信」之上，
  给不出 leader 唯一性；且 lease/registry/forward 三处各自维护「谁是 active」，是重复 owner。
- Creation proof: 验收 1/3/5 只有共识层能满足（验收 3 尤其：分区下唯一 leader）。
- Entropy / retirement impact: 净减少 owner 数（lease + registry + control-rotation + 快照推送 + edge 角色
  → 1 个 raft 领导权 + 1 个本地物化路径）；退役清单见 §Retirement。新增面限定在 `cluster/` 内，不新增跨模块抽象。
- Decision: add-with-proof（Arachne 引入）+ reuse-existing（本地 SQLite 与 `ConfigData` 物化复用既有
  `content.rs` 编解码与 admin CRUD，不新建第二套数据模型）
```

```text
Architecture Integrity Lens:
- Invariant: 「至多一个节点接受管理写」必须由共识保证，而不是由时间/租约近似。
- Canonical owner / contract: leader 身份 = raft 领导权（Arachne）；配置内容 = Arachne 分片 + head 提交点。
- Responsibility overlap: 现状 lease（谁持租约）→ registry（谁是 active URL）→ forward（转发目标）
  是三份同一事实的副本；本计划收敛为一份。
- Higher-level simplification: 转发不再需要「按租约持有者实时解析 + 自转发护栏 + forward-once」三层，
  因为目标由对端返回的 leader hint 决定，且对端自己会拒绝非 leader 写；同构之后连「跨节点推送配置」
  这条链也一起消失（每个节点自己从 Arachne 物化）。
- Retirement / falsifier: 若 Phase 0 实测 Arachne 无法在 Hydra 依赖图里共存（tonic/protobuf 冲突、
  与 Pingora 运行时冲突、或 `Arachne::start` 单例约束不可满足），本计划的 Phase 2+ 全部作废，
  只保留 Phase 1 的发现（这就是 falsifier）。
- Verdict: proceed（带 Phase 0 硬门）
```

```text
Plan-Time Complexity Check:
- Target files: crates/hydra-server/src/cluster/{mod,lease,control_client,registry,forward}.rs（均有 300–770 行）、
  redis/mod.rs（610 行）、main.rs（2028 行）
- Existing size / shape signals: `cluster/` 已是 9 文件 / 36353 行规模；`main.rs` 已是 2028 行
- Owner fit: 新 owner 落在 `cluster/arachne*.rs`（新增文件），**不往 `main.rs` 里堆逻辑**
- Add-in-place risk: 直接改 `lease.rs`/`registry.rs` 会让「新旧两套选举」在同文件共存 ⇒ 正是本计划要消灭的重复 owner
- Better file boundary: 新增 `cluster/arachne_node.rs`（引导/身份/句柄）+ `cluster/arachne_keys.rs`（键空间）
  + `cluster/arachne_head.rs`（head 哈希与分片编解码）；旧文件在 Phase 3 整文件退役
- Recommendation: add owner file（新增）+ split task（Phase 3 一次性退役 lease/registry/control 轮询）
```

```text
Plan Pressure Test:
- Owner / contract / retirement: 单一 leader owner（raft）+ 单一配置 owner（Arachne head），退役路径明确
- Architecture integrity / higher-level path: 已做（见 Lens）
- Verification scope: 验收 1–7 全部可在本机 `docker redis` + 3 个同构节点复现；验收 5 需分区注入
- Task executability: 每个 Task 有文件、命令、判定；Phase 0 有硬门
- Pressure result: proceed
```

---

## 决策记录 / Decision Records（D-1…D-5 定稿，落地时追加 D-7/D-8；均需写入 ADR）

### 决策已定稿（2026-10-05 用户裁定，实现不得偏离）

| 问题 | 裁定 | 对方案的影响 |
|---|---|---|
| 失效广播要不要从 Redis 搬进 Arachne | **不搬** | Redis Streams + 游标 + 裁剪判定保持现状；控制面与数据面彻底解耦 |
| 节点是否区分 edge / 控制面 | **不区分：全部节点必须一致** | `edge` 角色退役、`NodeRole` 只剩两种状态（单节点 / 集群同构）、配置分发改为每节点自行物化 |
| 管理写被拒时怎么办 | **在 admin UI 做自动重试，把这个错误消解掉** | 服务端返回结构化的 leader 提示；UI 自动转发一次并在界面上说明 |
| 配置怎么存 | 见 §决策记录 D-4（已重新设计为分片 + 内容寻址 + 提交点） | 键空间、GC、任务分解按 D-4 执行 |
| 节点数 | **至少 3 台**，manifest 与文档**一起改** | compose 与 k8s 清单改 3 副本（StatefulSet 固定身份）；2 节点等于没有容错 |
| 旧环境变量 | 没有既有版本 ⇒ **可以全改**，但必须保证修改一致 | 删除旧名、统一新名、文档/清单/脚本一次改齐，由脚本双向核对 |
| 是否为 Arachne 出 ADR | **要** | 单出一份 ADR 固化 D-1…D-6（实现期追加 D-7/D-8，见 ADR §3），本文档保留实现细节 |

### 决策记录（需写入 ADR）

| # | 决策 | 状态 | 备选（真实存在过的） | 兼容边界 | ADR 需回答 |
|---|---|---|---|---|---|
| **D-1** | **只替换控制面，Redis 留在数据面**（限流 / 熔断 / 认证 L2 / 失效总线） | 用户已裁定 | ① 全面替换（自建事件流 + 计数器，最终去 Redis）；② 只换 lease + registry | 数据面不得依赖 Arachne；限流仍 fail-open、熔断仍本地兜底 | 「Redis 只是数据面的加速器与近似计数器，不是任何权威」这条基线成立吗？`cluster.md` §3 是否已这样写？ |
| **D-2** | **节点同构：全部节点都是 raft 成员、都跑数据面与管理面；`edge` 与「无状态数据面」退役** | 用户已裁定 | ① edge 做 raft learner（本地读、低延迟，但要 WAL + 成员变更）；② edge 纯 HTTP 客户端 + last-known-good（保留无状态，但扩缩容与延迟二选一） | 集群规模 ≥3 且扩容 = raft 成员变更；每节点需要持久卷（WAL + SQLite）；`NodeRole` 简化为「单节点 / 集群」 | 「数据面可无状态横向扩缩」这条产品承诺是否正式撤回？若撤回，`ops.md` 与部署文档里的 HPA 说明怎么改？ |
| **D-3** | **写打在任意节点，由库透明转发到 leader**；Hydra 不需 409/提示/UI 重试层 | 用户裁定「在 admin UI 消解」→ 上游 `bce2943` 修复后**问题本身消失**（复核 p11/p12 实测透明转发成立） | ① 非 leader 返回 409 + hint + UI 重发（原设计，现已被上游修复取代）；② 只返回裸错误 | 对外契约里**不再出现** 409+leader 地址；`x-hydra-leader-hint` 不再需要；无 leader 时仍 503（可重试） | 上游这版修复是**未发布**的（crates.io 仍是 0.1.1 旧行为）；Hydra 采用时是吃 crates.io 还是吃某个新版本/自建分支？这决定了要不要保留 409 兜底 |
| **D-6** | **租户自助写采用方案乙 · 乙-full 定稿**：不重复鉴权（删 leader 侧 `reauth` + 授权绑定），收口到那 4 个数据面端点（`/api/v1/internal/tenant-config/*` 整族删除），**不留任何 Hydra 转发器**、**不加新鲜度闸** | **用户已裁定**（2026-10-05） | ① 甲：保留三道闸（含 leader 重鉴权）；② 丙：请上游在转发协议里带调用方上下文；③ 乙-lite：先删身份、保留转发作过渡（用户否决） | 身份闸唯一在入口节点；写核心事务内校验仍在；限流与审计归属移到入口节点（记账：集群总量 = N × 单窗口）；`head` 是权威、本地 SQLite 是可重建物化 | 无（已定稿）；ADR 需记录两条放弃项与「为何不重复鉴权」的论证 |
| **D-4** | **配置 = 内容寻址分片 + `ctl/head` 提交点；head 写成功才算提交** | 本计划设计（用户要求「再想想」，此版为重新设计的结果） | ① 单值整体写（超 1 MiB 直接 `InvalidArgument`）；② 每实体一个键 + 索引（细粒度，但 Arachne 无范围扫描，索引要自建，且跨实体改动无原子提交点） | 分片上限 256 KiB（Arachne 单值上限 1 MiB）；编码格式带 `format` 版本号；旧分片按引用计数回收 | 分片大小是否需要可配？GC 保留窗口（版本数 vs 时间）怎么定？ |
| **D-5** | **集群 HA 前提 = 至少 3 台（奇数）同构节点** | 用户已裁定 | ① 保持 2 台（2 节点下 raft 无容错）；② 引入外部协调服务 | 部署清单、`ops.md` 演练步骤、容量规划一起改；2 节点部署必须被明确拒绝或至少响亮警告 | 2 节点配置是「拒绝启动」还是「启动但告警」？ |

---

---

## Phase 0 结果（2026-10-05 实测，硬门结论）

**结论：硬门通过。** 依赖图可共存（同二进制链接成功）、六条行为假设 5 条成立，3 条被实测证伪并已在本文档改正（F-1/F-2/F-3）。

- 上游固定提交：`befa3d2`（本机 checkout `.arachne-research/arachne-src`），锁定的 crate 版本：`arachne-kv 0.1.1` + `raft-seedable 0.7.0`
- 探针源码：`.arachne-research/probe/`（六条行为）+ `.arachne-research/dep-probe/`（依赖图）；原始输出：`.arachne-research/results/probes.log`、`.arachne-research/results/duplicates.txt`
- 工具链：**`RUSTUP_TOOLCHAIN=1.98.0`**（Arachne 的 `rust-toolchain.toml` 要 1.93.1，那个 channel 本机未装；用 1.98.0 编译全程无警告/错误）

### 依赖图（T0.1）

| 检查 | 结果 |
|---|---|
| 精简核心（`--no-default-features`） | ✅ 可编译（首次尝试因 `CARGO_HOME` 只读失败，改用工作区内 `.cargo-cache/home` 后通过） |
| **同一个二进制里同时链接 pingora 0.8 + arachne-kv** | ✅ **构建成功**（`dep-probe` 通过；探针二进制 10.4 MB）—— 这是本次硬门的关键一条 |
| `cargo tree --duplicates` | 436 行输出，**无 `tokio` / `hyper` / `rustls` / `tonic` 的重复主版本**（只有 `socket2 0.5+0.6` 这类无害重复） |
| 版本对照（Hydra ↔ 探针） | `protofbuf 2.28.0` 两侧相同；`tokio 1.53.1/1.53.2`、`hyper 1.11.0/1.11.1`、`rustls 0.23.43/0.23.45` 同主版本；`pingora 0.8.1`、`fred 10.1.0`、`reqwest 0.12.28` 相同 |
| 新增依赖 | `tonic 0.12.3`、`protobuf 2.28.0`（Hydra 已有）、`slog`、`raft-seedable 0.7`、`protoc-bin-vendored`（构建期带 protoc；**构建需要网络或预热缓存**，CI 要加缓存步骤） |

### 行为探针（T0.2）

| 探针 | 问的问题 | 实测结果 |
|---|---|---|
| p2 | `without_redirect().put` 在 follower 上是否返回 `NotLeader{hint}` | ✅ **成立**：`NotLeader{leader_hint: Some((n1, 127.0.0.1:17101))}`，且提示**精确指向真 leader** |
| p3 | 单值上限是否在 propose 前强制 | ✅ **成立**：`put(1 MiB)` = Ok；`put(1 MiB + 1)` = `InvalidArgument("value of 1048577 bytes exceeds max_value_bytes (1048576)")` |
| p4 | 失多数派：写/线性读失败、`get_stale` 仍可服务 | ✅ **成立**：存活节点 `put` → `NotLeader{hint: None}`、`get` → 同；`get_stale` 仍返回旧值 |
| p5 | facade 单例生命周期 | ✅ **成立**：`NotInitialized` → 首次 `start` Ok → 二次 `AlreadyInitialized` → `shutdown` Ok → `NotInitialized`（**前提：调用在 tokio 运行时上下文里**，见下） |
| p6 | 多节点 facade 能否在**外部 current-thread 运行时**里驱动（Pingora 的形态） | ✅ **成立**：在别人的 current-thread 运行时里 `start(member)` + `set`/`get` 全通 |
| p1 | 交接后 `leader_hint` 的语义与收敛速度 | ⚠️ **部分证伪**：**其他**节点的提示 1 s 内更新到新 leader（`n1 says leader = n3`）；但**新 leader 自己**的 `leader_hint()` 在 40 s 窗口内**始终不指向自己** |
| p7 | follower 上的线性读 `get` 行为 | ❌ **证伪文档**：follower 的 `get` **立即**返回 `QuorumUnavailable`（0 ms），`without_redirect().get` 返回 `NotLeader{hint}` —— 重定向只在**进程内** `register_peer` 过的对端之间工作 |
| p8 / p10 | **无 tokio 运行时上下文**时调用 `Arachne::start` | ❌ **崩溃**：`tokio::time::interval` panic——“there is no reactor running, must be called from the context of a Tokio 1.x runtime”。**两条路径都崩**（peerless 与 member 都走 `Runtime::new`） |
| p9 | 在运行时上下文里启动 peerless facade | ✅ 成立（这是 p8 的规避方式） |
| p0 | 三节点选举耗时（诊断） | ✅ 2 s 内选出 leader，之后 20 s 稳定不变 |
| **p11** | **「任意节点 `set`，其它节点 `get` 都能得到一致结果」是否成立** | ❌ **不成立**：follower 上 `put()` **立即** `QuorumUnavailable`（0 ms，**不转发**）；follower 上 `get()` 同样立即 `QuorumUnavailable`；只有 leader 自己能读写。`get_stale` 则收敛很快（见 p13） |
| **p12** | 拿到 `NotLeader` 提示后，客户端改投提示节点是否可行 | ✅ **可行**：`follower.put()` → `QuorumUnavailable`，`follower.leader_hint()` → `n2 @ 127.0.0.1:17102`，在该节点重发 → `Ok(())`，值可读回 |
| **p13** | 提交后 follower 本地读多久能看到 | ✅ **7 ms 以内**（5 轮：0/7/7/0/6 ms）⇒ 轮询式物化完全可行，但**不能把一次 `get_stale == None` 当作权威否证** |

### 由实测强制修正的五条设计

> **状态更新（2026-10-05 第二轮）**：F-1/F-2（部分）/F-3 **已由上游 `bce2943` 修复**，并在下方 §「上游修复复核」逐条复核通过。
> 保留本节是因为 **F-4 的一条后果仍然成立**（转发由库负责 ⇒ Hydra 侧不实现转发），而 F-2 的结论（**不使用 `get`，用 `get_stale` + 哈希校验**）在新版下**依然成立**：
> `get` 现在能用了，但它把每次本地读变成一次跨节点往返（实测 41 ms），而配置物化是每节点每秒一次的本地轮询 —— 用 `get_stale` 才是对的。

| 编号 | 原计划假设 | 实测 | 修正 |
|---|---|---|---|
| **F-1** | 「`Arachne::start` 不需要运行时上下文」（Arachne README 的说法） | **启动即 panic**（p8/p10），两条路径都是 | Hydra 必须在**自己的运行时**里引导 Arachne：单节点模式在启动阶段建一个 current-thread 运行时并 `block_on(start)`；集群模式同理（不能从纯同步 `main()` 直接调）。Phase 1 的 T1.2 要把这条写成显式步骤 + 一条回归测试 |
| **F-2** | 「配置物化用 `get`（ReadIndex 线性读）读 `ctl/head`，用 `get_stale` 读分片」 | follower 的 `get` **立即** `QuorumUnavailable`（p7）—— 在健康集群上永远失败 | `ctl/head` 不再是有版本号的清单，改为**单个内容哈希**；节点用 `get_stale` 读 head 与分片，**按内容哈希校验**（哈希即版本，天然单调，不依赖"读到最新"）。写入侧仍用 leader 的 `put`（线性写，天然可用）。**全案不再使用 `get`** |
| **F-3** | 「`leader_ready` = `leader_hint()` 指向自己」 | 新 leader 自己的提示在 40 s 内不指向自己（p1） | `leader_ready` 改为**写探测**：`without_redirect().put(<探测键>, <本节点id>)` 成功即 ready（这是唯一权威判据，且失败方向安全）。`leader_hint()` 只用于**follower 找 leader**（这条被 p1/p2 证实可靠） |
| **F-4** | 「`set`/`get` 在任意节点都能用，客户端重定向是 Arachne 的事」 | **两个 API 都不转发**（p11/p12）：follower 上 `set` 与 `get` 都立即 `QuorumUnavailable`；**重定向只对进程内 `register_peer` 过的对端生效**，真实多进程集群没有这条通道 | Hydra **不做服务端跨节点转发**（写只在 leader 被接受），但**必须**给客户端一个可用的 leader 地址：409/503 响应体 + `x-hydra-leader-hint`。这与「不专门实现 transfer 到 leader」并不矛盾 —— Arachne 内部不做，Hydra 侧只做「告知地址 + 客户端重发」，没有第二套选举或领导者解析 |
| **F-5** | 「`get_stale` 的 `None` 是权威的」 | follower 的本地读**存在毫秒级滞后**（p13 实测 ≤7 ms；p11 中紧跟提交后的读会读到 `None`） | 物化任务与读路径**不得**把一次 `None`/旧值当作权威：以「head 哈希是否变化」为唯一触发条件，并按哈希校验内容；读不到就下一轮再读 |

## 上游修复复核（2026-10-05 第二轮，commit `bce2943`）

用户按本文档 §「Phase 0 结果」的缺陷清单修改了 Arachne 并提交（`befa3d2..bce2943`，25 文件 / +1647 −98）。
**复核结论：五条全部落实，其中 3 条改变了本方案的设计。**

复核方式：把探针依赖从 crates.io 的 `arachne-kv 0.1.1` 换成**本地 checkout**（`path = "../arachne-src/arachne"`，edition 2024 与本机 1.98 工具链），全部探针重跑；另跑上游新增回归测试。
原始输出：`.arachne-research/results/probes-after-fix.log`。

| 原缺陷 | 上游修复 | 复核结果 |
|---|---|---|
| **① 无运行时上下文启动即 panic** | `Runtime::new` 不再构造期建 `tokio::time::interval`（改存 `Duration`，`run()` 内惰性构建） | ✅ **`p8` PASS**（peerless 无运行时启动成功）、**`p10` PASS**（member 无运行时启动成功）。上游回归 `facade_no_runtime.rs` 也通过 |
| **② follower 的 `set`/`get` 不转发** | seam 新增 `ForwardCommand/CommandSink/RemoteForwarder/ForwardTransport`，transport-tonic 新增 `Forward` RPC，handle 在 `NotLeader` 时经 tonic 向 hint 地址转发，leader 侧注入本地 Runtime（会话表保证跨转发幂等） | ✅ **`p11` PASS**：**三个节点都能写**（每个 `put()` 都 `Ok`，1 ms），且**每个值在三个节点上都能线性读回**（follower 读 0–42 ms，走转发）；`get_stale` 三节点一致。**`p7` PASS**：follower 的 `get` 现在 `Ok`（41 ms），`without_redirect().get` 仍如设计返回 `NotLeader{hint}`（0 ms） |
| **③ 新 leader 自指 `leader_hint`** | `raft_to_node` 补 self entry | ✅ **`p1` PASS**：`transfer_leader` 后目标节点的 hint **0 ms 即指向自己**（旧版 40 s 窗口内始终不指向） |
| **④ 双编号身份未写死** | api-reference / README / dev-docs 写死 `RaftId`（1 基序号）↔ `NodeId`（地址身份）映射 | ✅ 文档已更新（`docs/api-reference.html`、README 各 +若干行） |
| **⑤ `get_stale` 滞后语义 / protoc 构建依赖** | 文档补两处注记 | ✅ 已补（实测值与本方案 `p13` 一致：≤7 ms） |

**发布版复核（`arachne-kv 0.1.2`，crates.io）**：上游随后把版本号从 0.1.1 提到 **0.1.2**（提交 `e4c5272`）并发布。探针依赖从本地 checkout **切回 crates.io 的 0.1.2** 重跑 —— **13 个探针全部 PASS**（p1/p2/p3/p4/p6/p7/p8/p9/p10/p11/p12/p13/p14），
即"库级跨节点写转发""无运行时上下文启动""新 leader 自指 hint"三项在**发布产物**上同样成立。证据：`.arachne-research/results/probes-0.1.2-published.log`。
⇒ 遗留 1 关闭：Hydra 直接依赖 crates.io 的 `0.1.2`，不需要 git rev、不需要自建分支。

**回归确认**：上游 `facade_no_runtime` / `leader_hint_self` / `server_facade_cluster`（含 leader 故障切换）三个套件在本机全绿。

> **一条探针自身的缺陷（已修，记在这里以免误判库）**：单节点 facade 首次写要等自选主，期间 `set` 返回的是 **`Timeout`** 而不是 `NotLeader`/`QuorumUnavailable`。
> 我最初的探针只把后两者当"可重试"，于是把 `Timeout` 当成致命错误 ⇒ 一度误报 `p9`/`p10`/`p6` 失败。
> 修法：探针把 `Timeout` 与 `Busy` 一并视为可重试。**这条对 Hydra 也成立**：`Timeout` 是"稍后重试"，不是"不可能"。

### 复核带来的三处方案简化（比原设计更少代码）

| 原设计 | 复核后 | 影响 |
|---|---|---|
| 非 leader 的管理写返回 409 + leader 地址，**admin UI 自动重发**（D-3） | **写打在任意节点都会被库透明转发到 leader** ⇒ 不需要 409、不需要响应头、不需要 UI 重试层 | **admin UI 的请求层不用改**（原 T3.3 的 UI 工作取消）；`x-hydra-leader-hint` 响应头也不再需要 |
| Hydra 侧"给客户端一个可用 leader 地址"是写路径的前提（F-4） | 写路径**不再需要**任何 leader 地址 | 转发层（早已决定退役）彻底没有替代品需求 |
| `leader_ready` 用**写探测**（因新 leader hint 不自指，F-3） | 仍然用写探测（`without_redirect().put`）—— 写探测同时覆盖"我是 leader"与"我能提交"两件事，比 hint 更强 | 不变；但 hint 现在也可作为辅助观测 |
| 租户自助写（A-2）依赖"转发到 leader 后 leader 重鉴权 + 授权绑定" | **需要新决策**：若把写交给库转发，leader 侧执行时**没有 Hydra 的租户 Bearer 上下文** ⇒ 接受侧重鉴权与"写目标 == 已鉴权租户"的绑定就断了 | 见 §决策记录 D-6（新增） |

---

---

## D-6：租户自助写（**已裁定：方案乙 —— 不重复鉴权，收口这 4 个端点**）

用户裁定（2026-10-05）：**采用方案乙** —— "既然已经鉴权过的请求，没有必要反复做重复的鉴权，只需要收口紧一下，只有租户写这几个接口才放行"。

### 乙是什么（按代码事实精确化）

"租户写" = 数据面 4 个自助端点（`hydra_core::tenant_api::parse_write_route`）：`PUT /tenant/{tid}/api/v1/sub-tenants/{name}`、`DELETE …/sub-tenants/{id}`、`PUT …/sub-tenant-routes`、`DELETE …/sub-tenant-routes/{id}`，凭租户访问令牌，写 `sub_tenant` / `sub_tenant_route`。

**"重复鉴权"精确指哪两个东西被删掉**：

| 今天的三道 | 乙之后 | 为什么可以删 |
|---|---|---|
| ① 入口节点 `run_gate`（令牌 + URL 交叉核对 + 失败限速 + 成功配额） | **保留**（唯一一道身份闸） | 这是真正在做鉴权的那一道 |
| ② leader `reauth`（用 leader 的 ConfigStore 用同一份逻辑再鉴一次租户） | **删除** | 同一个令牌、同一份快照语义；令牌在①已被验证 |
| ③ leader 授权绑定（写目标必须 == 已鉴权租户） | **删除**（身份由①产生） | **关键事实**：写核心 `admin::sub_tenant_write` 在**同一个事务内**已经重读活的 DB 行 + 当前 `ConfigData` 做校验（配额、前缀重叠、TOCTOU、存在性 404）—— 归属正确性主要由此承载，而不是靠③ |

### "收口"必须落在入口节点，不能落在 leader（一处不能照字面实现）

若按字面在 leader 上"只放行这几个接口"：**做不到**。交给库转发之后到达 leader 的不是 HTTP 请求，而是一条**不透明的已编码 KV 命令**（`ForwardRequest.cmd`，proto 只有 `kind/cmd/key/client_id/seq_no/hello`），leader 没有"路径"可看。

正确的收口是**机械的、而且比今天更紧**：

- 只有那 4 个数据面端点能构造这类写命令；
- **`/api/v1/internal/tenant-config/*` 整个端点族删除**（它存在的唯一理由就是"从别的节点转发进来"）。今天它的危害面很清楚：任何持集群 token 的调用方都能构造跨租户写；删掉之后**这类请求根本不存在**；
- `x-hydra-tenant-token` 头与 `TENANT_TOKEN_HEADER` 一并退役；
- 操作员侧入口（`admin/handlers.rs` → 同一个写核心）**不受影响**——它是另一条路，不在这 4 个端点里。

### 乙的两个版本（差别在"转发器要不要真的删掉"）

一个机械约束必须先讲：**写完之后要 `ConfigStore::reload_all()`，而配置的提交点 `head` 是 leader-only 的**（非 leader 的 `put` 由库转发，但"写核心跑在谁身上"决定了 SQLite 事务与 reload 发生在哪个节点）。

| | **乙-lite（过渡态）** | **乙-full（目标态）** |
|---|---|---|
| 谁跑写核心 | leader（入口节点仍把请求转过去） | **任意节点**（入口节点自己跑事务 + reload） |
| 转发器 | 保留，但**剥掉租户身份**（不再有 `x-hydra-tenant-token`、不再重鉴权/绑定） | **完全删除**（`tenant_config/forward.rs`、`cluster/forward.rs`） |
| 跨节点机制 | Hydra 的转发（目标 = `leader_hint()` 地址） | **库的透明转发**：入口节点写新版本的实体键 + toc，最后 `put(head)` 那一次由库转到 leader 提交（正好是 §键空间 的 key-path 模型） |
| 前提 | 无（可立即做） | 需要"配置发布走 Arachne"先落地（Phase 2/3） |
| 工作量 | 小 | 中，但**不新增代码**——它是 Phase 2/3 的自然结果 |

**建议**：目标态选 **乙-full**（用户要的"一个转发器都不留"）；若希望"删身份"这件事尽早落地，可以先做 乙-lite 作为过渡，Phase 3 收尾时再删转发器。

### 三处语义的归属（乙-full）

| 语义 | 今天 | 乙-full | 备注 |
|---|---|---|---|
| 写限流（V6/D6） | leader 内网端点，进程内窗口 | **入口节点**，进程内窗口（身份在这里） | **诚实记账**：多节点部署下每个节点各有窗口 ⇒ 集群总量上界 = N × 单窗口。沿用今天的"反 DoS 而非计费"定位，并把这句话写进 `ops.md`；不引入 Redis 计数（D-1） |
| 审计（A-2 前置 6） | leader 记录（tenant / trace_id / action / resource / version） | **入口节点**记录：那正是"验证过身份的节点"。`x-hydra-trace-id` 中继机制（OC-3 已实现）保留，保证一条写在各节点日志里是同一个 trace | 响应头/错误体里的 trace id 与审计记录仍然一致 |
| `config_version` 对账 | 比对 leader 版本 | 写核心在事务内用**本节点当前 `ConfigData`** 校验；提交后由 `head` 物化统一版本 | 见下方待答 2 |

### 明确放弃的东西（如实记账，不粉饰）

1. **"执行写的节点按最新配置重新确认归属"这道闸没有了**。若入口节点快照落后于 leader（令牌刚轮换、子租户刚被改），它会在旧视图下放行一次写。**缓解**：写核心的事务内校验仍在（对着本节点 DB 与 `ConfigData`），且配置收敛实测 ≤7 ms（p13）+ 1 s 轮询；影响面是"基于稍旧视图的一次合法写"，不是越权写。
2. **入口节点被攻破后可以冒充任意租户写**。注意这**不是新增能力**：任何集群节点本来就持有 `HYDRA_ENCRYPTION_KEY` 与全量配置快照（可解密 provider key 与证书私钥），节点级攻破今天就已经是全集群失守，而不是"从租户 A 变成租户 B"。这条要写进 `ops.md` 的威胁模型说明。
3. 4 个端点本身的收口**没有变弱**：仍与读端点共用同一个 `run_gate`（含 URL 交叉核对），且 near-miss 一律本地 404、绝不落到代理管线（否则租户令牌会被当成 api-key 去 POST `auth_url`）。

### 裁定结果（2026-10-05，用户）

1. **目标态 = 乙-full**：不保留任何 Hydra 转发器、不做 乙-lite 过渡。租户写在**入口节点就地完成**（写核心事务 + 本地物化 + 发布），跨节点的只有配置发布里的 `put(head)` 一次，由库透明转发到 leader 提交。
2. **不加新鲜度闸**：不引入「我的配置版本 vs 权威 `head`」的提交前比对。身份在入口节点一次判定；正确性由写核心的事务内校验承载（见上表第 3 行）。

### 乙-full 的写入序列（实现依据，避免「就地完成」被理解成「本地质疑」）

1. 入口节点 `run_gate` 通过（唯一身份闸）→ 得到已鉴权租户 `T`；
2. 入口节点把 4 个端点之一映射成 `TenantConfigWrite`，调**同一个**写核心 `admin::sub_tenant_write`（事务内校验 + upsert/delete，`BEGIN IMMEDIATE`）；
3. `ConfigStore::reload_all()` 重建本节点内存 `ConfigData`（含新行）；
4. 按 §键空间 的 key-path 模型发布：`put(<new-toc-hash>/<entity>/<id>)` … → `put(<new-toc-hash>/toc)` → **`put(head) = new-toc-hash`**（这一次在非 leader 上会由库转发到 leader 提交）；
5. 所有节点（含入口节点自己）在下一轮物化里看到新 `head` 并物化 —— **本节点也走同一条物化路径**，不给自己开后门；
6. 失败语义：步骤 2 失败 ⇒ 事务回滚、无副作用；步骤 4 失败 ⇒ **本地 SQLite 已改但未发布**，此时 Arachne 才是权威，入口节点必须**退避重试发布**（沿用 `hydra_replica_materialize_retries_total` 口径）并报 ERROR —— 与今天 leader 的 `reload_best_effort` 失败对称；
7. **权威与缓存的分工写死**：`head`（Arachne）是权威，本地 SQLite 是**可重建的物化状态**；两者不一致时以 Arachne 为准（物化会覆盖本地行）。这条必须写进 `store.rs` 注释，否则「本机改了 DB 就算数」会变成第二个真相。

## 任务分批 / Task Batches

> 每个 Task 结束后：`cargo fmt` → `cargo clippy --all-targets --features server -D warnings` → 该 Task 的聚焦用例 → **一个可独立回滚的提交**。
> 真实多节点用例一律照铁律 2：**真 docker redis + 真二进制**，不用 mock。

### Phase 0 — 可行性硬门（不合并产品代码）— ✅ **已完成，见上节结果**

**T0.1 依赖图与工具链探针**
- **Files**：`dev-docs/aegis/plans/2026-10-05-arachne-control-plane.md`（本文件 §Phase 0 结果表追加一行）；不改 `Cargo.toml`。
- **Why**：在把 `arachne-kv` 写进 Hydra 之前，先证明它与 `pingora 0.8` / `reqwest 0.12` / `fred 10` 的依赖图能共存。
- **Steps**：
  1. `git -C /home/alex/Projects/hydra/.arachne-research/arachne-src rev-parse --short HEAD` 记录 Arachne 版本 hash；
  2. `cd .arachne-research/arachne-src && cargo build -p arachne-kv --no-default-features` ⇒ 期望 exit 0（零外部 crate 精简核心）；
  3. `cd .arachne-research/arachne-src && cargo build -p arachne-kv-node` ⇒ 期望 exit 0（tonic 传输）；
  4. 在 `.arachne-research/dep-probe/` 建临时 crate（**不加入 Hydra workspace**），依赖 `arachne-kv = "0.1.1"` + `pingora = "0.8"`，`cargo tree --duplicates` 记录重复条目（重点：`tokio`/`hyper`/`rustls`/`protobuf`/`tonic`）。
- **Verification**：命令 exit 0；重复依赖清单写入本文件 §Phase 0 结果表；**判定**：若出现 `rustls` 或 `tokio` 主版本冲突（不可同时链接），本计划的 Phase 2+ 作废并回到设计（falsifier）。
- **Failure path**：如 tonic 传输不可用，退路是用 `default-features = false` 的零依赖核心 + 自写 transport（**本计划显式不选**，代价另立计划）。

**T0.2 API 行为探针（写清 6 条待证事实）**
- **Files**：`.arachne-research/probe/`（临时 crate，不并入 workspace）。
- **Why**：文档给了 API，但 6 条行为必须实测，否则计划里的时序假设会错。
- **Steps**（每条一个最小用例，`cargo run -p probe --bin <name>`）：
  1. `leader_hint()` 在 follower 上返回 leader 的 `(NodeId, SocketAddr)`，且 `transfer_leader` 后 ≤1 s 变化；
  2. `Handle::without_redirect()` 的 `put` 在 follower 上返回 `NotLeader{hint}`（**这就是 leader 判定与写保护的原语**）；
  3. 单值上限：`put` 1 MiB+1 字节 ⇒ `InvalidArgument`；`put` 512 KiB ⇒ Ok（确认上限可配且校验在 propose 前）；
  4. `get_stale` 在 minority 分区侧仍可用（返回旧值），`put`/`get` 返回 `QuorumUnavailable`；
  5. `Arachne::start` 二次调用 ⇒ `AlreadyInitialized`；`Arachne::shutdown` 后静态读 ⇒ `NotInitialized`（**单例约束**）；
  6. 从 Pingora 的 bg runtime（非 `#[tokio::main]`）里 `block_on`/`await` facade 调用可行（Pingora runtime 上不 panic）。
- **Verification**：6 条全部记录实测输出到本文件 §Phase 0 结果表；任一为否 ⇒ 对应设计假设作废并在此表登记替代方案。
- **Failure path**：第 6 条为否时，改为「Arachne 专用 OS 线程 + mpsc 到 Pingora」的装配（Phase 1 T1.2 内解决，不改变外部语义）。

**T0.3 退役清单与真实 Redis 用例盘点**
- **Files**：`dev-docs/aegis/plans/2026-10-05-arachne-control-plane.md`（§Retirement 表格填实际行号）。
- **Steps**：
  1. `grep -rn "LEASE_KEY\|hydra:{nodes}\|node:hb\|node:seen\|node:reap\|XTRIM\|XADD" crates/ integration/ scripts/ environment/` ⇒ 逐条登记（控制面 vs 数据面）；
  2. `ls crates/hydra-server/tests/ | sort` ⇒ 标出依赖 Redis 租约/注册表的用例（Phase 3 必须逐个迁移或退役）；
  3. `grep -rn "MemoryLeaseStore" crates/` ⇒ 单测侧的替身清单。
- **Verification**：清单落表；**判定**：无法在 Phase 3 迁移的用例必须显式列入 §Retirement 的「保留理由」，不允许「先留着再说」。

### Phase 1 — 装配与同构启动（默认不启用）

**T1.1 依赖与 feature**
- **Files**：`crates/hydra-server/Cargo.toml`、`Cargo.lock`。
- **Impact/Compatibility**：`arachne` feature **不在 `cluster-redis`、也不在 `server` 的默认集合里**；`server` 保持现状 ⇒ 默认二进制零变化。
- **Steps**：
  1. 加 `arachne-kv = { version = "0.1.1", optional = true }`；
  2. 加 feature：`arachne = ["runtime", "db", "proxy", "dep:arachne-kv"]`（与 `cluster-redis` 可组合，不互相 imply）；
  3. `cargo tree -e features --features "server,arachne"` 记录；`cargo build --no-default-features --features "runtime,db,http-client"` 仍必须与今天同样成功/失败（不引入新耦合）。
- **Verification**：`cargo build --features "server,arachne,cluster-redis"` exit 0；`cargo build --features server` 产物 hash 与改动前**一致或差异可解释**（记录二进制体积差）。
- **Retirement**：无（纯增量）。

**T1.2 集群身份与成员表（`cluster/arachne_node.rs`，新增）**
- **Files**：新建 `crates/hydra-server/src/cluster/arachne_node.rs`；改 `crates/hydra-server/src/cluster/mod.rs`（挂载模块 + 重写集群判定）。
- **Why**：把「进程级单例 + 静态成员表 + 身份」收在一个 owner 文件里，避免散进 `main.rs`（2028 行）。
- **Steps**（strict TDD）：
  1. 写单测：`ClusterPeers::parse("a=10.0.0.1:7001,b=10.0.0.2:7001,c=10.0.0.3:7001")` ⇒ `initial_cluster = [a,b,c]` + `addresses{a,b,c}`；**自身必须在表里**且**恰好一次**；空表 / 重复 id / 缺地址 / 非 `host:port` / 节点数 < 3 ⇒ 各自一个 `Err`（**fail-closed**，绝无「HOSTNAME → 随机」回退）；
  2. `Verify RED`：`cargo test -p hydra-server --features arachne cluster_peers` 失败；
  3. 实现解析 + `pub fn cluster_enabled() -> bool`（判定 = `HYDRA_CLUSTER_PEERS` 是否存在），以及 `main.rs` 里 `NodeRole` 的替换：集群判定不再读 `HYDRA_ROLE`；
  4. `Verify GREEN`；
  5. 装配函数 `pub fn start(...) -> Result<ArachneControl, String>`：`Arachne::start` + 首次握手键校验（`ctl/cluster_id` + `ctl/format`；不匹配 ⇒ `Err`，文案必须说清「数据目录属于另一个集群」）；
  6. 集成用例（`crates/hydra-server/tests/arachne_cluster.rs`，3 个**进程内**节点、`127.0.0.1:0` 端口）：用 `assemble_cluster`（它不占用那唯一单例，单进程可起多个节点）⇒ 恰好一个 `without_redirect().put` 成功；生产路径仍是 `Arachne::start`。
- **Verification**：`cargo test -p hydra-server --features "arachne" --test arachne_cluster`；反向证伪：把随机 id 回退加回去 ⇒ 第一条单测变红（证伪过程记进提交信息）。
- **Retirement**：无。

**T1.3 leader 判定与 `/healthz/leader`**
- **Files**：`crates/hydra-server/src/main.rs`、`crates/hydra-server/src/admin/cluster_api.rs`、`crates/hydra-server/src/admin/mod.rs`（`leader_ready` 注入点注释）。
- **Steps**：
  1. 新增 `ArachneControl::leader_watch()`：后台任务每 250 ms 发一次**写探测** —— `without_redirect().put(b"hydra/probe/<node_id>", <now_ms>)`，成功 ⇒ `is_leader = true`。**不得用 `leader_hint()` 判断自己是否 leader**（实测 p1：新 leader 自己的提示在 40 s 内不指向自己）；
  2. 用该闭包提供 `leader_ready`（**替换** `LeaderElection` 的产出，不做双轨）；探测失败时置 false 并计数（`hydra_arachne_leader_flips_total`）；
  3. **一致性纪律（必须写进代码注释）**：探测缓存只用于 `/healthz/leader`、管理面转发判定与可观测展示；**配置写永远不信任它** —— 写的判据是 `put` 的返回值（缓存说「我是 leader」而实际不是时，写会失败，不会脑裂）。反向也安全：探测失败 ⇒ 该节点不当 leader，最多多一次 409 重试；
  4. **永远不要用 `get`**（F-2）：`head`、分片、探测全部只用 `put`（线性写）与 `get_stale`（本地读）。这条写成 T1.3 的 grep 断言；
  4. `cluster/mod.rs` 的 `CLUSTER_ONLY_ENV` 换成新变量表；`scripts/check_cluster_env.cjs` 同步。
- **Verification**：`node scripts/check_cluster_env.cjs` exit 0；3 节点集群上 `/healthz/leader` 恰好一台 200；`transfer_leader` 后 ≤1 s 状态翻转。
- **Retirement**：无（此刻 `LeaderElection` 已无调用点，在 T4.1 删除）。

### Phase 2 — 键空间与提交点

**T2.1 键空间与目录编解码（纯函数，`cluster/arachne_keys.rs` + `cluster/arachne_toc.rs`）**
- **Files**：新建两个文件；`cluster/mod.rs` 挂载。
- **Steps**（strict TDD）：
  1. 写单测：`Toc::encode/decode` 往返；**条目顺序敏感**（顺序变化 ⇒ 不同 toc-hash）；截断 / 多余字节 ⇒ `Err`；`format` 不匹配 ⇒ `Err`；实体路径非法（空 id / 含 `/` / 超长）⇒ `Err`（路径由实体 id 拼成，**必须校验**，否则一个带 `/` 的 id 就能写到别的实体命名空间）；
  2. `Verify RED`；
  3. 实现：`Toc { format: u32, entities: Vec<TocEntry> }`，`TocEntry { path: EntityPath, content_hash: [u8;32], len: u32 }`；`EntityPath` 是枚举（`Tenant(id)` / `Provider(id)` / …，**不是裸字符串**，从类型上杜绝路径注入）；键常量 `ctl_head() / cfg_toc(&toc_hash) / cfg_entity(&toc_hash, &path) / ctl_cluster_id() / ctl_format()`；**全部以 `hydra/ctl/` 与 `hydra/cfg/` 前缀**，禁止其它模块裸拼字符串；
  4. `toc_hash = H(Toc::encode())` 的往返与**稳定性**单测（同一 Toc 两次编码哈希相同；条目顺序不同 ⇒ 哈希不同；实体内容变一位 ⇒ 哈希变）；
  5. `Verify GREEN`。
- **Verification**：`cargo test -p hydra-server --features arachne arachne_keys arachne_head`。
- **Retirement**：无。

**T2.2 配置提交点（`ArachneConfigStore`，唯一允许写 `hydra/ctl/head` 的地方）**
- **Files**：新建 `crates/hydra-server/src/cluster/arachne_store.rs`。
- **Steps**（strict TDD）：
  1. 写集成用例（真实 3 节点 Arachne）：① `publish(config)` 返回新 toc-hash，`read_config()` 能完整重建出等价的 `ConfigData`；② **只写变化的实体**：改一个租户 ⇒ 断言旧 toc 里**内容未变**的实体键**没有被重新 `put`**（用 Arachne 指标或键存在性判定），新 toc 里它们的 content-hash 与旧 toc 相同；③ `publish` 中途失败（注入：某个实体 `put` 返回 `QuorumUnavailable`）⇒ **head 不前进**，`read_config()` 仍返回旧哈希与旧内容 —— 这是「head 是唯一提交点」的证明；④ **篡改注入**：把某个实体键的存储值改掉 ⇒ `read_config()` 必须因目录里的 **content-hash 校验失败**而拒绝整轮（不允许拼出错误配置）；⑤ **删除语义**：删掉一个租户 ⇒ 新 toc 里不再有该路径，`read_config()` 得到的 `ConfigData` 里也没有它（结构删除是"不在目录里"，不是"写空值"）；
  2. `Verify RED`；
  3. 实现 `publish(&self, cfg: &ConfigData) -> Result<[u8;32], String>`（① 逐实体编码并算 content-hash；② 与上一份 toc 比对，**只 `put` 变化或新增的实体**；③ `put` toc；④ 最后 `put` head = toc-hash。`NotLeader` ⇒ `Err`，**不自己找别处重试**）、`read_config()`（head → toc → 实体，**全部走 `get_stale`** 且逐项校验 content-hash；不一致整轮重试，**绝不使用 `get`**，见 F-2）、`current_hash()`；
  4. 指标：`hydra_arachne_publish_total{result}`、`hydra_arachne_config_bytes`（gauge）；
  5. `Verify GREEN`；反向证伪：把 head 写在分片之前 ⇒ 用例③ 变红。
- **Retirement**：无。

### Phase 3 — 控制面切换（本轮的主要退役动作）

**T3.1 每节点本地物化（取代跨节点快照推送）**
- **Files**：新建 `crates/hydra-server/src/cluster/materialize.rs`；删 `crates/hydra-server/src/cluster/control_client.rs`；改 `crates/hydra-server/src/store.rs`（物化回调）。
- **Steps**（strict TDD）：
  1. 写用例（真实 3 节点）：① 管理写后 **3 个节点**物化出同一 toc-hash 的 `ConfigData`；② **传播延迟**：提交后 1 个轮询周期内（≤1 s）所有节点完成物化；③ **滞后不是否证**（F-5）：在提交后**立即**读一次 `get_stale(head)`（可能仍是旧值）不得导致物化任务把该节点标成"配置不存在"或抛出错误，只允许下一轮再读；④ 某节点物化失败（注入解码错误）⇒ 按 1s→2s→…→60s 退避重试（沿用既有 `hydra_replica_materialize_retries_total` 口径），且该节点**不能当 leader**（未物化 = 不可当选，防止用旧配置接受管理写）；⑤ Arachne 失多数派 ⇒ 各节点继续用本地已物化状态服务，`hydra_arachne_quorum_unavailable_total` 增加；
  2. `Verify RED`；
  3. 实现物化任务：`get_stale(head)` → 与上次哈希相同则空转（稳态：一次本地读/秒）→ 否则 `get_stale(toc)` → 逐实体 `get_stale` 并校验 → 解码 → 写本地 SQLite → `ArcSwap` 换入 `ConfigData`；
  4. `Verify GREEN`。
- **Verification**：`cargo test -p hydra-server --features "server,arachne" --test arachne_materialize`；`integration/test_snapshot_stale.py` 改为「本地物化陈旧」口径。
- **Retirement Track**：`ControlClient`（轮询 / 按租约持有者轮换）与 `GET /api/v1/internal/snapshot` 同 Task 删除。

#### T3.1 实际落地形状（2026-10-05 实现记录；与上面的 Files 不一致，以这里为准）

原来的 `materialize.rs` 一个文件，落地时拆成三个，因为边界比预想的清楚：

| 文件 | 是干什么的 | 为什么单独一个文件 |
| --- | --- | --- |
| `cluster/arachne_keys.rs` | 键空间 + `Toc` + `content_hash` | 已有（T2.1），本轮只加了 `EntityPath::Fidelity` |
| `cluster/arachne_entities.rs` | `ConfigData`(+fidelity) ⇄ 每实体一值 | **纯函数、无集群无数据库**，可以脱离 raft 单测 |
| `cluster/arachne_materialize.rs` | 门（gate）：head → 决策 / 退避 / 是否可当选 | 决策表是纯函数，`cluster_decision()` 复用 |
| `cluster/arachne_materializer.rs` | 循环：store 读 → 解码 → 交给 target → 推进水位 | 唯一有 I/O 的一半 |
| `cluster/arachne_store.rs` | 提交点（head 的唯一写者）与读路径 | 已有（T2.2） |

**新增的决定：fidelity 实体（`EntityPath::Fidelity`）——树里不只有 `ConfigData`。**

- **问题**：`ConfigData` 是从 SQLite 表**推导**出来的视图，推导过程是有损的：`limit_role` 里 `enabled=0` 的行、`provider_model` 里 `status != 1` 的行、以及所有行的**主键 `id` 与 `created_at`**，都不在 `ConfigData` 里。只复制 `ConfigData` 的副本无法重建出同一张表（会丢掉禁用行，并且重建时会给 provider key **重新生成 id**）。
- **做法**：树里多一个 `Fidelity` 实体，携带这些「推导丢掉的行」。键按**路径**而不是按树哈希（`hydra/cfg/e/fidelity`），与其它实体同一规则，所以配置没变时它不会被重写。
- **秘密怎么走**：`provider_keys`（API key）与 `tenant_token_hashes`（租户令牌哈希）**必须是密文**。旧的 snapshot 线就封了这两样，树里若走明文等于把已有的保护悄悄撤掉。所以：非秘密的行走普通 JSON，这两样走 `Sealed`，只有持主密钥的一方能打开（`FidelityTreeEntity::rows`）。有针对性用例 `the_fidelity_entity_carries_no_plaintext_secrets`（含「换一把主密钥必须被拒绝，而不是交出垃圾」）。
- **一个被实测逼出来的约束（重要）**：树是**内容寻址**的——名字就是字节的哈希。而 AES-GCM 每次封都用**新的随机 nonce**，所以**在编码过程里封密文 = 同一个逻辑配置每次发布都换一个树名**，于是 head 前进、所有节点无限重新物化。因此密封被移到编码**之外**：发布方把已封好的材料作为参数传进来（`SealedMaterial`）。守卫用例：`the_same_inputs_name_the_same_tree`（这条用例是红的抓出来的：第一版把密封写在 `split_config` 里，一个没变的配置命名出了两棵不同的树）。
  - **现状要说清**：目前唯一的构造函数是 `seal_plaintext`（**现封**，因此不可复现，只适合测试/一次性发布）。生产用的「读库里**已存的密文**」这一路径**还没写**，属 T3.2；现在**没有任何 `db` 读取函数返回已存密文**（`list_provider_keys` 等都解密后返回）。原文写的「与 `SnapshotWire::build` 的取值口径一致」是**错的**：`SnapshotWire::build` 每次 build 都用 `kp.seal(..)` 重新封（对版本号命名的 wire 正确，对内容寻址的树致命）。已回写在 ADR §10。
- **不是「顺便加的」**：这是本轮唯一一处**扩大**树的内容的地方，理由不是「将来可能有用」，而是「不加就重建不出同一份副本」。

**同一批工作里抓出的第二个缺陷：cert 私钥根本没进树（已修）。**

- `CertMeta::cert_key_pem` 带 `#[serde(skip_serializing)]`（私钥不进任何序列化形式），而 `Cert` 实体直接序列化 `CertMeta` ⇒ **私钥不在树里**。Redis 时代的 wire 用**单独的**密封字段 `SnapshotWire::sealed_certs` 补偿这个 skip，树没有对应字段。
- 后果不是「少一个字段」：`restore_config` 见到 `cert_key_pem == None` 会**往 `cert_key_ciphertext` 写 NULL** ⇒ 物化一次就**删掉**该节点已有的租户 TLS 私钥。与 G1（丢禁用行）、G3（重铸 provider-key 主键）同一类。
- 修法：`Cert` 实体的值改成 `CertTreeEntity{meta, sealed_key}`，密钥**随该租户自己的 cert 实体**走（不放进 fidelity 单例：按域一实体才能让"这个租户的证书变了"不重写其它证书）；解码时回填 `cert_key_pem`；`cert_key_pem` 存在但密封材料缺失 ⇒ **拒绝发布**（不是丢掉）。`TOC_FORMAT` 1→2：同一批字节两种解法的版本必须互相拒绝。
- 守卫：`tests/arachne_cert_fidelity.rs`（先红后绿，含「树里不得出现明文 `PRIVATE KEY`」）、`a_cert_key_without_sealed_material_is_refused`。

**同一轮里的第三处：集合按迭代序编码（已修）。** `cfg.tenant_providers` / `cfg.tenant_models` 是 `HashSet<String>`，而 serde 按**迭代序**序列化集合、`RandomState` 每实例随机 ⇒ 同样的行两次读库（= 两次发布）得到**不同字节** ⇒ 树名变化、head 前进、全节点重新物化。这是 D-8 的同一失效模式换了个来源（那边是随机 nonce，这边是随机哈希种子）。修法：`sorted_members()` 排序后编码（解码侧仍是集合，顺序不影响相等性）。守卫：`two_independent_builds_of_the_same_config_name_the_same_tree`——它**故意分两次构建配置**，因为原有那条 `the_same_inputs_name_the_same_tree` 对同一个对象切两次、且 fixture 的集合只有一个元素，两种情况下顺序都不构成问题（这就是它一直绿的原因）。

**第四处：解码不能自己发明 loader 的推导规则（已修）。** `ConfigData` 是 loader 从 SQLite **推导**出来的，解码侧必须复现**同一套规则**，而不是另立一套"看起来更整齐"的：

1. **四个 `Vec` 的顺序**：原本一律按 `id` 排，而 loader 用的是 SQL `ORDER BY`（`limit_role` 按 `created_at,id`、`provider_key_binding` 按 `key_prefix`、`sub_tenant` 按 `tenant_id,name`、`sub_tenant_route` 按 `sub_tenant_id,model_key`）⇒ 副本持有的是**另一种顺序的**配置。今天这些顺序恰好不影响行为（所有匹配的限流角色都会被独立执行；绑定/子租户匹配取**最长前缀**而非第一个；路由按唯一键选），所以它逃过了评审——但"这棵树就等于这份配置"只能靠**相等**来验证，偷偷重排会让这类比较全部失去意义。
2. **`tenants_by_domain` 的键**：loader 用**小写域**做键、值里保留库里原样的域名；解码却拿值里的原样域名当键。于是存储域名为 `Acme.Example` 时，副本把租户放在键 `Acme.Example` 下，而 `proxy::resolve_tenant` 用**小写 `Host`** 查表 ⇒ **同一租户在 leader 上解析得到、在每个副本上都解析不到**。这条是真实的行为差异，不只是整齐问题。
- 守卫：`tests/arachne_derivation_fidelity.rs`（两条用例都先红后绿；fixture 故意让"loader 序"与"id 序"方向相反、让域名非小写，并在断言前先证明 fixture 确实如此）。

**T3.1 明确没做完的部分（如实记账，不算完成）**：

1. ~~`MaterializeTarget` 目前只有测试里的记录型实现~~ → **已完成**：真实 target = `ReplicaTarget`（先 `db::restore_config` 一个事务重建本地 SQLite，再 `ConfigStore::apply_snapshot` 换入内存态；顺序是**先库后内存**，反了会让节点服务一份自己库里没有的配置）。守卫用例 `the_real_target_rebuilds_the_replica_and_serves_the_published_config`（真 raft + 真 store，含"把副本**自己的库**用 loader 读回来必须等于发布的那份配置"这一条）。`MaterializeTarget` 因此改成 `async`（`async-trait`），因为失败必须在事务提交前被看见。
   - **仍未做**：**没有任何东西在启动时拉起这个循环**（没有 spawn），所以节点今天还是不从 Arachne 服务配置。这一条与发布侧切换同属 T3.2 —— 只做一半会得到一个"head 永远没有人写、节点却自称已物化"的进程。
2. 发布侧仍然走 `SnapshotWire`（`arachne_entities::encode_config` 已经能产出树，但管理写路径还没改用它）——这是 **T3.2** 的内容。
3. 因此 T3.1 的 Verification 里那条「真实 3 节点、3 个节点物化出同一 toc-hash」**尚未执行**；已执行的是 1 节点 + 真实 store 的端到端与纯函数层的等值性/确定性/保密性用例。

**这一轮由真实 target 用例抓出的两个缺陷（已修）**：

- **toc 解码器缺 `TenantProvider`/`TenantModel`（判别值 12/13）**：两个实体**能写**（`id()`/`discriminant()` 一直产出这两个值）但**读不回来**，于是只要配置里有一条"租户→供应商"或"租户→模型"授权，`read` 就在 toc 处失败、物化无限重试、**全集群一份配置都物化不出来**。躲过所有既有用例的原因很朴素：**没有一个 fixture 带着授权走过真实 toc**（`arachne_entities` 的单测直接 `split_config`+`tree_of`，绕过了 toc 编解码）。现在补上 12/13，并且加了一条把这一类**堵死**的用例：`every_entity_kind_survives_a_toc_round_trip`——凡是本 build 能当键用的实体种类，都必须能解码；它与 `every_entity_kind_has_its_own_segment` 共用同一份 `all_entity_kinds()` 清单。
- **fixture 的 `window: "1m"` 违反 schema 的 CHECK**：记录型 target 什么都接受，所以这条只有真的把行写进 SQLite 才暴露。改 fixture 时把原因写在旁边，免得下次又被"简化"回去。

**T3.2 配置版本权威 = `head` 内容哈希**
- **Files**：`crates/hydra-server/src/store.rs`、`crates/hydra-server/src/db.rs`、`crates/hydra-server/src/admin/*`。
- **Steps**（strict TDD）：
  1. 写用例：① leader 一次管理写 ⇒ `head` 变成新内容的哈希；② 写事务提交但 `publish` 失败 ⇒ 管理写 503 `config_not_published`，**并点名**「本地 SQLite 已提交但未发布」，不静默继续；③ 启动时本地记录的物化哈希 ≠ `head` ⇒ 走**正常重放物化**（不是拒绝启动）；④ 提交哈希与物化哈希不一致且重放持续失败 ⇒ ERROR + 拒绝服务管理写；
  2. `Verify RED`；
  3. 实现：版本的权威值是 `head` 哈希；`config_meta` 记「本节点已物化到哪个哈希」，`config_version`（自增整数）降级为**仅供既有 API 展示/水位比较**的派生子（由物化次数或哈希序号维护，不再是权威）；
  4. `Verify GREEN`。
- **Verification**：`cargo test -p hydra-server --features "server,arachne"`；故障注入用例 ②③④ 各一次。
- **Retirement Track**：`config_meta` 的「版本权威」语义退役（表与列保留）。

#### 残留风险已消除：实体键改为内容寻址（2026-10-05，用户裁定 A）

`cfg_entity(path, content_hash)` ⇒ `hydra/cfg/e/<path>/<content-hash>`（原来是 `hydra/cfg/e/<path>`）。`TOC_FORMAT` 2→3（toc 字节没变，但**键**变了，说 2 的 build 会去错误的位置找实体，两者必须互相拒绝）。

- **为什么**：路径寻址下，两个并发发布者写**同一个键**；head 最终只指向一棵树，但那个路径上可能是**另一个发布者的字节** ⇒ toc 的 content-hash 与实存不符 ⇒ 读侧拒绝（「a mismatch is a retry」）⇒ **全集群停在 last-known-good，直到有人再发布一次**。内容寻址后两个版本各自成键、互不覆盖，head 指向哪棵树，那棵树就是完整的。
- **连带简化**：`plan_publish`/`PublishPlan` **删掉**了。它靠「读已提交的 toc 做基线」来决定哪些实体不用重写，而**滞后的节点拿不到可靠基线**（而且拿旧基线跳过写入时，那个键可能已经被回收）。现在「是否已存」由**键本身**回答（键就是内容哈希），对着**本节点自己的副本**查一次即可，不需要基线、不需要 round trip。`toc_for(tree)` 保留（校验必须发生在任何写入之前）。
- **守卫**：`tests/arachne_store.rs::an_interleaved_publish_cannot_tear_the_tree_named_by_the_head`——**确定性地**构造交错（A 发布 → B 发布 → 把 head 写回 A → 读；再写回 B → 读）。**已反向证伪**：把 `cfg_entity` 临时改回路径寻址，这条用例立刻变红，报错正是 `entity tenant/acme in tree <h> does not match the hash its table of contents records; refusing to serve a mixed tree`。
- **未做**：旧键的 **GC 仍未实现**（内容寻址后旧版本会累积）。计划里 GC 本来就是独立任务；在 GC 落地前，「跳过写入」是安全的（键不会被删），一旦实现 GC 必须保证：**只回收不被任何 live toc 引用的键**，且要考虑滞后发布者可能引用旧 toc。

#### T3.1 的验收：真实 3 节点已执行（2026-10-05）

`crates/hydra-server/tests/arachne_three_nodes.rs`——三个真 raft 成员（真端口）、三个 `ConfigStore`（三个 SQLite）、三个发布者与三个物化器，即 `main.rs` 的那套装配。两条用例：

1. `three_nodes_converge_on_the_same_head_and_each_can_rebuild_itself`：节点 1 写库 → `reload_all` 发布 → **三个节点物化出同一个 toc-hash**、都服务该配置、**各自的库里都有那一行**（重启后能自己重建）；并断言收敛是**轮询而不是重试循环**（每节点允许的 pass 数有上界，超出即失败）。
2. `a_write_on_a_non_leader_node_reaches_every_node`：写到**最后一个节点**（三成员里最多只有一个是 leader，所以大概率是 follower）→ 发布成功 → head 前进 → 三个节点都收敛到它、都落盘。

**这一跑立刻抓出一条与既定裁定冲突的代码事实（已改）**：

- `ArachneConfigStore::publish` 里用的是 **`handle.without_redirect()`**。这行是 **T2.2 时代**的决定，当时 follower 的 `put` 返回 `NotLeader`，而那个拒绝是**唯一可用的写保护原语**（计划 §Phase 0 第 2 条原文：「这就是 leader 判定与写保护的原语」）。
- 但**上游 0.1.2 把这个前提改掉了**（探针 p11：follower 的 `put` 会被转发），而你本轮的裁定（乙：任意节点就地执行 + 就地发布）正建立在这个前提上。保留 `without_redirect()` 的后果是**发布退化成 leader 独占**：三个真节点上一跑，3 次写里有 2 次直接 `NotPublished{NotLeader}`。
- 改法：`publish` 改用**可重定向的 handle**，让库把逐条 `put`（实体 → toc → head）转发给 leader。改完两条用例都过。

**由此产生的残留风险（用户裁定 A 后已消除，见上节）**：实体键是**按路径**寻址（`hydra/cfg/e/<path>`）而内容按**哈希**校验，所以两个节点**并发**发布时可能交错，输者的实体写落在赢者 toc 用另一个哈希描述的那个路径上 ⇒ 读侧哈希不匹配、**每个节点都拒绝这棵树**（退化为继续服务 last-known-good，直到下一次发布）。彻底的做法是把实体键改成**内容寻址**（`hydra/cfg/e/<path>/<content-hash>`），交错就变得无害（输者的树只是无人引用）。**待用户裁定是否现在做。**

#### T3.2 实现记录（2026-10-05；与上面的 Steps 不一致处以这里为准）

**已落地**：

| 部件 | 位置 | 说明 |
| --- | --- | --- |
| `ConfigPublisher` | `cluster/arachne_publish.rs` | 编码 + 提交 head。只做这两件事，编码是纯函数（可脱离集群测），commit 是 `ArachneConfigStore::publish`（head 的唯一写者） |
| 发布时机 | `ConfigStore::reload_all_with` | 本地置性判定的**唯一漏斗**：内容变了才发布，且是在**本地置换之后**。顺序的理由：SQL 事务早就提交了，所以本节点状态必须与自己的库一致；发布失败则返回 `StoreError::NotPublished` |
| 失败语义 | `StoreError::NotPublished` → admin 层 503 `config_not_published` | 文案是「已提交到本节点库、**未发布**到集群」，不是「写失败」——本地事务没有回滚。28 + 4 个调用点改为 `if let Err(r) = reload_best_effort(..).await { return r; }` |
| 物化循环 | `main.rs` bootstrap (2c-arachne) | 每秒一次 `converge()`；**与发布同时落地**——只发布不物化会得到一个「head 没人读、集群静默分叉」的进程 |

**与 Steps 的偏差（如实记账）**：

1. **没有把版本权威换成 head 哈希**：`config_meta.config_version` 仍是自增计数器，`ConfigStore::version()` 仍从 `ReplicationContent` 派生。理由是它与本轮的落地点无关且会牵动多处 API；**权威事实上已经是 head**（节点服务的配置由 head 决定、物化以 head 为准），只是那个**整数水位**还没被降级。属 T3.2 残余。
2. **没有把物化门接到 `leader_ready`**：`leader_ready` 仍是 Arachne 写探测（`ArachneControl::is_leader`）。计划里「未物化 = 不可当选」的接线还没做，因为 `Materializer` 的门不是可共享的（需要 `Arc` 化或把门拆出来独立持有）。属 T3.2 残余。
3. **`POST /api/v1/reload`（force）与 tenant 写路径**都走同一漏斗；tenant 侧那份 `reload_best_effort` 副本也做了同样的 503 映射（该文件按 D-6 乙-full 会在 T3.5 整文件删除，所以只做了最小改动）。

**这一轮抓出的东西**：

- **发布失败的触发条件是真的**：`a_failed_publish_names_itself_and_keeps_serving_locally` 用一个超过库 1 MiB 单值上限的 provider 触发；它证明了「本地照常服务 + 明确的未发布错误」这条路径，也说明**配置大到某个程度会全体写 503**——这不是 bug，但要在 `ops.md` 里点名（fidelity 单例是**一个值**，它的大小上界就是 1 MiB）。
- **两个 `StoreError` 同名**（`store::StoreError` 与 `cluster::arachne_store::StoreError`）：新变体属于前者，写测试时踩了一次。属于命名债，未动。

**T3.3 写路径的转发行为（验证任务；上游 `bce2943` 后**本任务只验证不实现**）**
- **Files**：`crates/hydra-server/src/cluster/forward.rs`（**删除**）、`crates/hydra-server/src/tenant_config/forward.rs`（**保留**，见 D-6）、`crates/hydra-server/src/admin/mod.rs`（无 leader 时的 503 文案）。
- **Why**：原设计要在 Hydra 侧做「409 + leader 地址 + UI 自动重发」；复核 `bce2943` 后**这个需求消失了** —— 库已把 follower 上的写透明转发到 leader（p11/p12 实测）。因此本任务从"实现一层"变成"删掉一层并证明它不需要"。
- **Steps**：
  1. 写集成用例（3 节点 Hydra）：① 把管理写**分别打到三个节点**，三者都 2xx，且**收敛到同一个 `head` 哈希**；② 杀掉 leader 后（无 leader 窗口内）写返回 **503**（不是 409、不是挂起），新 leader 选出后写恢复；③ 断言 `grep` 层面：仓库里不再有 `forward_mutation` / `forward_config_write` 的**操作员**路径调用；④ **反向证伪**：把库换成旧行为（follower 写返回 `QuorumUnavailable`）时用例 ① 必须变红 —— 证明这条简化**依赖上游修复**；
     > **① 的原文已按用户裁定改齐（2026-10-05）**：原文是「三者都 2xx，且**落盘在同一份配置上**」，但每个节点是**各自**提交再发布的（裁定乙），三次不同的管理写是**后写者胜**，不是三次改动合并 ⇒ 能断言的是「每一次写都被接受」+「最后三个节点收敛到同一个 head、服务同一份配置」，不是「三次改动都在」。原措辞与 R5「管理写必须经 leader」也互相矛盾（R5 那条是对"无 CAS"的对策，与本裁定不兼容，已标注作废）。
  2. 删除 `cluster/forward.rs` 与 `x-hydra-forwarded` 护栏；确认 `AppState` 里对应字段一并移除；
  3. 租户自助写的转发路径**保留**（D-6），并在代码注释里写明"为什么只有它保留"；
  4. 上游修复**未发布**（crates.io 仍 0.1.1 旧行为）⇒ 在 `Cargo.toml` 里把依赖固定到可用该修复的版本/来源，并在此写清（这是本任务的一部分，不是运维事项）。
- **Verification**：`integration/test_cluster_ha.py`；新增 `integration/test_write_any_node.py`（①②④）；`cargo test -p hydra-server --features "server,cluster-redis,arachne"`。
- **Retirement Track**：`cluster/forward.rs` 整文件 delete-first（库接管）；`tenant_config/forward.rs` **保留**并在 D-6 记录理由。

**T3.4 数据面 Redis 边界（反证任务）**
- **Files**：无代码改动。
- **Steps**：
  1. `grep -rn "arachne" crates/hydra-server/src/redis/ crates/hydra-server/src/proxy/ crates/hydra-server/src/tenant_api/ crates/hydra-server/src/sink.rs` ⇒ 期望**零命中**；
  2. 跑 `integration/test_auth_cache_layers.py`、`test_cluster_limits.py`、`test_breaker_lifecycle.py` ⇒ 全绿；
  3. `dev-docs/cluster.md` §3 增补一句「控制面权威 = Arachne；Redis 只承载数据面近似状态」。
- **Verification**：第 1 条命中数必须为 0（否则 D-1 边界被破坏，回到复核）。
- **Retirement**：无。

**T3.5 租户自助写按 D-6 收口（**乙-full 定稿**；不做 lite 过渡）**
- **Files**：`crates/hydra-server/src/tenant_api/handlers.rs`（写分支）、`crates/hydra-server/src/tenant_config/forward.rs`（**删除**，乙-full）、`crates/hydra-server/src/cluster/forward.rs`（`forward_config_write` / `TENANT_TOKEN_HEADER` 删除）、`crates/hydra-server/src/admin/tenant_config_api.rs`（乙-full 整文件删除）、`crates/hydra-server/src/admin/sub_tenant_write.rs`（**保留**，仍是唯一写核心）、`crates/hydra-server/src/{proxy,main}.rs`（`tenant_config_forwarder` 字段与装配）。
- **Why**（D-6，用户裁定）：不重复鉴权；收口到 4 个数据面端点；身份闸唯一在入口节点。
- **Steps**（strict TDD）：
  1. **先钉边界用例**（与实现无关，先写先红）：① 4 个端点在**任意节点**都能完成（集群下 3 节点各发一次，都 2xx 且落盘一致）；② 无令牌 ⇒ 401、令牌与 URL 租户不符 ⇒ 403、near-miss 路径 ⇒ 本地 404 且**不落到代理管线**（断言租户令牌没有被当成 api-key 去 POST `auth_url`）；③ **越权写必须失败**：拿租户 A 的令牌写租户 B 的子租户 ⇒ 既不能 2xx，也不能在 B 名下落下任何行（断言 DB 里 B 的行数不变）；④ `grep` 断言：`internal/tenant-config`、`x-hydra-tenant-token`、`TENANT_TOKEN_HEADER` 零命中；
  2. `Verify RED`：③ 在"身份由入口节点产生但**没有**任何绑定校验"的中间态下必然能红 → 证明该用例承重；
  3. 删除转发器与内网端点族；入口节点自己跑写核心 + `reload_all`，配置发布按 §键空间 的 key-path 模型与 §D-6 的「乙-full 写入序列」（`put(head)` 那一次由库转发到 leader 提交）；
  4. **发布失败路径**必须实现并测试：本地 SQLite 已改而 `head` 未前进 ⇒ 报 ERROR + 退避重试发布（不得把「本机改了」当成已提交）；
  5. **记账**：`ops.md` 写明两条放弃项（新鲜度闸、入口节点被攻破的威胁模型）与一条量化事实（集群写限流总量上界 = N × 单窗口）。
- **Verification**：`cargo test -p hydra-server --features "server,cluster-redis,arachne"`；`integration/test_sub_tenant_data_plane_write.py`（既有套件，含越权写用例）；新增 `integration/test_tenant_write_any_node.py`（①③）；`grep` 三条零命中断言；`node scripts/check_ci_wiring.cjs`。
- **Retirement Track**：`x-hydra-tenant-token` / `TENANT_TOKEN_HEADER` / `admin/tenant_config_api.rs` / `tenant_config/forward.rs` 全部 delete-first；**保留** `admin/sub_tenant_write.rs`（唯一写核心）与操作员侧入口。

#### T3.3 实现记录（2026-10-05）

**做了什么**：

| 动作 | 位置 | 说明 |
| --- | --- | --- |
| 删除操作员转发的整个闸门 | `admin/mod.rs` | `maybe_forward_mutation`（含 forward-once 循环护栏）、它的调用点、`resolve_forward_target` 全部删除。管理写现在**就地执行 + 就地发布** |
| 删除操作员转发器 | `cluster/forward.rs` | `forward_mutation` / `forward_mutation_with_timeout` / `FORWARD_ONCE_HEADER` 删除；模块头改写为「只剩租户配置写这一条路径」，并写明它在 T3.5 整块消失 |
| 保留租户转发 | `tenant_config/forward.rs` | 按 D-6 保留，直到 T3.5（乙-full）把 4 个端点搬到入口节点 |
| 退役旧用例 | `tests/cluster.rs` | 删除 4 条断言**旧契约**的用例（`leader_health_and_write_gate`、`a_silent_leader_produces_504_*`、`a_refused_leader_produces_502_*`、`standby_forwards_mutations_to_active`）——它们断言「standby 绝不本地写」，而乙正好相反 |
| 新契约的用例 | `tests/arachne_three_nodes.rs` | ① `a_management_write_on_any_node_is_accepted_and_the_cluster_converges`（三节点各发一次，都被接受，最终收敛到同一 head、服务同一份配置）；② `a_write_that_cannot_be_published_is_refused_rather_than_accepted`（三成员集群只起一个 ⇒ 永远没有 quorum ⇒ 发布失败 ⇒ `NotPublished`，本地照样提交并服务）——这是已删的「502 forward_failed」的继任者，且**确定性**（不靠"杀 leader 抢窗口"） |

**与计划 Files 一行的偏差**：T3.3 的 Files 写「`cluster/forward.rs`（删除）」，但同一份计划的 **T3.5 退役清单**又写 `cluster/forward.rs`（`forward_config_write` / `TENANT_TOKEN_HEADER` 删除）——即该文件在 T3.5 时仍在。以 T3.5 那行为准：**T3.3 退役这个文件的"操作员一半"**，租户那一半随 T3.5 一起消失（否则要把 `forward_config_write` 搬进一个下次就被删掉的文件里）。

**一条测试自身的缺陷（已修，值得记）**：新用例第一次跑是红的，报「三个节点 10 s 内没收敛」，而**实际上三个节点已经收敛到同一个 head**——是我的断言错了：`wait_for_head` 返回了**第一个可见**的 head，而这个节点自己的 `get_stale` 视图当时还停在前一个 head 上（三次发布，头动了三次）。改成**等一个稳定的 head**（连续两次读到相同值），并把失败信息改成带「各节点最后一次结果 + 各自已物化的哈希」，否则下次再红还是只能靠猜。

**②（无 leader 窗口写 503）的诚实边界**：这条现在由「无 quorum ⇒ 发布失败 ⇒ NotPublished」覆盖，走的是**同一条** 503 映射；"杀掉在任 leader 然后抢窗口"那种版本**没有**写成用例（时序脆弱），没有做的原因写在这里而不是含糊过去。

#### T3.4 实现记录（2026-10-05）——一条反证、两条**无法执行**

**① 边界 grep（D-1 是否被破坏）：零命中。**

```
grep -rni "arachne" crates/hydra-server/src/redis/ crates/hydra-server/src/proxy/ \
                     crates/hydra-server/src/tenant_api/ crates/hydra-server/src/sink.rs
⇒ 0
```

即：数据面热路径（限流/熔断/认证 L2/失效总线/用量 sink）**没有任何一处**引用控制面。这是 D-1「只换控制面」的直接证据。

**② 三套数据面集成套件：1 套真跑并 PASS，另外 2 套对本模型**无法执行**（如实记账）。**

用**当前源码**构建的二进制（`cargo build --features "server,cluster-redis,usage-clickhouse"`，debug）实跑：

| 套件 | 结果 |
| --- | --- |
| `integration/test_breaker_lifecycle.py` | **PASSED**（B1–B5：死集进出、§9.1 skip 计数、路由被跳过、DELETE 复位真的恢复、成功复位）。它不使用任何已退役变量 |
| `integration/test_cluster_limits.py` | **CANNOT VERIFY**（exit 2）。它用 `HYDRA_ROLE=leader` + `edge`、`HYDRA_CONTROL_URL`、`HYDRA_PUBLIC_URL`、`HYDRA_LEADER_LEASE_MS`、`HYDRA_CONTROL_POLL_MS` 起两个节点 —— **全是已退役的租约世界变量**。节点自己的日志把话说清了：`cluster wiring is configured but HYDRA_CLUSTER_PEERS is not set; this node is standalone — the wiring listed in ignored is NOT used`，于是 `/healthz/leader` 拿不到租约，套件按其自带守卫报 CANNOT VERIFY |
| `integration/test_auth_cache_layers.py` | 同上，**CANNOT VERIFY**（同一条日志、同一个原因） |

**要点**：这两套**不会假绿**——它们自带 `CANNOT VERIFY` 守卫（exit 2），这正是它们该有的行为。但计划里「三套全绿」这条验收**在本模型下不成立**，原因是测试脚本本身描述的是已退役的拓扑，不是实现回归。

**要做的后续（属 T4.3 基线同步，不是 T3.4）**：把这两套脚本从「leader/edge + 控制面 HTTP」改成「同构 raft 成员 + `HYDRA_CLUSTER_PEERS`」——它们是**数据面**的验证（共享限流窗口、L1/L2 分层、失效总线），价值仍在，只是拓扑描述过时了。在改好之前，②这条验收应记作**未完成**，不得引用为「已通过」。

**③ 基线**：`dev-docs/cluster.md` §3 顶部加了「控制面权威 = Arachne；Redis 只承载数据面近似状态」的说明与三行归属表，并把该节的「配置快照 / 选举」两行标注为已退役（其余逐条改写留 T4.3）。

#### T3.5 实现记录（2026-10-05，D-6 乙-full）

**做了什么**：

| 动作 | 位置 | 说明 |
| --- | --- | --- |
| 数据面写路径改为**就地执行** | `tenant_api/handlers.rs` | 删掉整段转发分支（含 `internal_request` 的四端点映射、`ForwardedWrite`、`forward_write`、`ForwardError`/`TenantConfigForwarderError` 的 502/503/504 分类）⇒ 入口节点跑共享写核心 + `reload_all`（后者会**发布**，T3.2） |
| **写核心搬家** | `admin/sub_tenant_write.rs` | `TenantConfigWrite` / `WriteOutcome` / `ApplyError` / `apply_config_write` 从 `admin/tenant_config_api.rs` 移入——它们本来就是单一写点，且从不依赖 `AdminState`/`Session`/HTTP |
| 内网端点族**整族删除** | `admin/mod.rs`、`admin/tenant_config_api.rs` | `/api/v1/internal/tenant-config/*` 的路由与 960 行实现删除（现在落到普通 404）；`x-hydra-tenant-token` / `TENANT_TOKEN_HEADER` 零命中 |
| 转发器与模块删除 | `tenant_config/`、`cluster/forward.rs`、`proxy.rs`、`main.rs`、`lib.rs` | `TenantConfigForwarder` 及其 `AppState` 字段/访问器/装配、整个 `tenant_config` 模块、`cluster/forward.rs`（操作员半边已在 T3.3 删） |
| `WriteOutcome::Deleted` 去掉 id | `admin/sub_tenant_write.rs` | 唯一读者是已删的内网响应封装；编译器立刻报了「字段从未被读」——留着一个没人读的值就是留一个假接口 |
| **限流旋钮退役** | `admin/mod.rs`、`ops.md` | `config_write_throttle` / `config_write_per_min` / `HYDRA_TENANT_CONFIG_WRITE_PER_MIN` 的唯一执行点是已删的内网端点 ⇒ 一并删除（运维事实写进 `ops.md` §5.5a：集群写总量上界 = **N × 单窗口**） |
| **环境变量退役机制第一次真正启用** | `cluster/mod.rs` | `HYDRA_FORWARD_TIMEOUT_SECS` 在 T3.3 失去最后一个读者，**是 `check_documented_env.cjs` 抓出来的**（它要求「文档里的变量必须有读点」）⇒ 移入 `RETIRED_CLUSTER_ENV`，`CLUSTER_ONLY_ENV` 10→9，并把「表是空的」那条断言改成**精确钉住表内容** |

**用例**：删除 `tests/sub_tenant_internal_write.rs`（整族内网端点，893 行）与 `tests/sub_tenant_data_plane_write.rs` 的两条转发用例（`edge_write_forwards_...`、`edge_write_forward_timeout_is_504`）；后者重写为 `a_data_plane_write_is_applied_by_the_node_that_received_it`（本节点应用 + 落**本节点自己的库** + `config_version` 前进 + 幂等 PUT + DELETE 真的删掉）。跨节点那一半由 `tests/arachne_three_nodes.rs` 覆盖。

**grep 断言（代码零命中，注释里的历史说明保留）**：`internal/tenant-config`、`x-hydra-tenant-token`、`TENANT_TOKEN_HEADER`、`tenant_config_api` 在 `crates/hydra-server/src/` 内的**代码**引用为 0（剩余出现全部是「这里曾是 X，已退役」的注释）。

**记账（已写进 `ops.md` §5.5a）**：① **无新鲜度闸**（入口节点按自己持有的快照鉴权，可能略滞后于 head；事务内仍会对活的 DB 行重校）；② **入口节点被攻破可冒充任意租户**（并说明这不是新增能力：任何节点本来就持主密钥与全量快照）；③ 写限流总量上界 = **N × 单窗口**。

#### 验收 1–5 的执行结果（2026-10-05，T4.1 的前置门）

前置门原文：「T4.0/T4.1 是不可回滚点，**执行前必须先过验收 1–5**」。但验收门禁点名的两个脚本**此前并不存在**：
`integration/test_arachne_control_plane.py`（验收 1/3/4/5）与 `test_admin_write_redirect.py`（验收 1 + UI
重试）。第二个**已作废**（乙裁定没有 409/重试层），第一个**本次补上并跑通**：

| 验收 | 结果 |
| --- | --- |
| 1 切换 + 任意节点写 | **PASS**：三个真进程，写到任意节点都 201；`kill -9` leader 后新 leader **1.2–1.3 s**（预算 3 s）；在**非 leader 的存活节点**上写也成功 |
| 2 故障切换期数据面 20 rps / 60 s | **PASS（2026-10-05 补上，T4.3）**：`integration/test_data_plane_failover_load.py` —— 三个真进程 + 真 mock upstream，**20 rps 压一个跟随者的数据口 60 s**，在第 15 s `kill -9` 掉 leader。实测 **1200 次请求全部 200**（p50 2 ms / p99 3 ms / max 3 ms），杀掉的那个节点确实是当时的 leader、之后由另一台接任，被杀窗口 **±3 s 内的 120 次请求全部 200**。**证伪**：把负载改成打在**将被杀死的那台**上 ⇒ C/D/F 立刻红（139 次失败，第一次就在 t+0.0s，HTTP 0 = 连接被拒），所以"零非 200"不是一条无论如何都会通过的判据 |
| 3 双写不可能 | **PASS**：静止时恰好一台 200；切换**全过程**连续轮询从未出现两台同时 200 |
| 4 配置收敛 | **PASS（内容层）**：三次写后三个节点服务同一份配置。**哈希层**由 `tests/arachne_three_nodes.rs`（三个真 raft 节点 + `materialized() == head`）断言 |
| 5 失多数派 | **PASS**：多数派死后写 **0.0 s 返回 503 `config_not_published`**（不挂起、不静默接受）；`/healthz/leader` 503；存活节点继续服务其已物化配置；**真实代理请求**（闸门→路由→mock upstream）仍 200 |
| 6 单节点回归 | 既有全量测试（纯 `server` 792 通过） |
| 7 退役 grep | T3.3/T3.5 已执行的 grep 断言；`LeaseStore`/`NodeRegistry` 等属 T4.1 |

**这次执行抓出两个此前不可能被发现的缺陷（都已修，见下）**，因此前置门从「无凭据」变成「1/3/4/5 有凭据、2 未移植」。

**缺陷 A：一个 raft 成员竟然必须先配一个已退役的变量才能启动。** `HYDRA_CLUSTER_PEERS` 已配、`HYDRA_CONTROL_URL` 未配时，三个节点**全部**以
`leader mode requires HYDRA_CONTROL_URL` 退出 ⇒ **验收 1/3/4/5 根本无法执行**。修法（T4.1 的启动半边）：Redis 控制路径（租约选举 / 快照轮询客户端 / 备用物化器）**仅当 Arachne 控制面没有在承载领导权时**才启动，其变量也仅在那时被要求。

**缺陷 B：默认 cluster id 按**每个节点自己的数据目录**派生，导致多节点集群根本起不来。** 于是每个成员算出的身份互不相同：第一个抢占 `hydra/ctl/cluster_id` 的取胜，其余两个拒绝加入（`belongs to a different cluster`）。**不显式设 `HYDRA_CLUSTER_ID` 的三节点集群 100% 无法启动**，而四个 in-process 三节点用例都没走到这条路径（它们直接构造 `ClusterConfig::member`）。修法：默认改为对**成员表**取哈希——它是每个成员唯一共享的东西，且 ADR-0001 本来就把它称为集群身份（**顺序属于身份**；用例同时钉住「空白不影响、重排则改变」）。`cluster_id_env()` 成为唯一派生点，raft 组名与握手共用它。

**`integration/test_cluster_ha.py` 已退役**（连 CI 步骤一起）：它驱动 `HYDRA_ROLE=leader|edge` + `HYDRA_CONTROL_URL`，只能报 CANNOT VERIFY。新步骤取代它；演练文档里写明旧覆盖里**如今无人覆盖**的两项（standby 转发管理写——已被乙裁定取代；edge 透明性——edge 角色随 T4.0 退役，且已无轮询可重新指向）。

### Phase 4 — 同构收口、退役与验收

**T4.0 节点同构收口（删除 edge 分支）**
- **Files**：`crates/hydra-server/src/cluster/mod.rs`（`NodeRole`）、`crates/hydra-server/src/main.rs`、`crates/hydra-server/src/admin/{mod,cluster_api}.rs`、`crates/hydra-server/src/tenant_api/*`、`environment/docker-compose.cluster.yml`、`dev-docs/{cluster,ops,deployment}.md`、`README*.md`。
- **Steps**：
  1. `grep -rn "NodeRole\|is_cluster\|has_admin_crud\|HYDRA_ROLE\|role == \|Role::Edge" crates/ scripts/ environment/ admin-ui/` ⇒ 列出全部角色分支；
  2. 逐条改：`NodeRole` 简化为「单节点 / 集群」；`has_admin_crud` 恒真；`edge` 专属分支（无 DB、无管理 CRUD、`/metrics` 免鉴权、快照拉取）全部删除；
  3. 任何残留的 `HYDRA_ROLE` 引用 ⇒ 启动时报错并点名（**不留静默忽略**）；
  4. 部署清单改 3 副本 + StatefulSet 固定身份 + 每节点持久卷；`scripts/check_cluster_env.cjs`、`check_compose_health.cjs` 同步。
- **Verification**：`grep -rn "Role::Edge\|HYDRA_ROLE" crates/ scripts/ environment/ admin-ui/` ⇒ 零命中；`node scripts/check_cluster_env.cjs`、`node scripts/check_compose_health.cjs` exit 0；3 节点 compose 起得来且三个节点行为一致（逐个跑同一组探针）。
- **Retirement Track**：`NodeRole::Edge` 及其全部分支 delete-first。

**T4.1 删除旧选举与注册表代码**
- **Files**：删 `crates/hydra-server/src/cluster/lease.rs`、`crates/hydra-server/src/cluster/registry.rs`、`crates/hydra-server/src/cluster/snapshot.rs` 的线上路径；改 `crates/hydra-server/src/redis/mod.rs`（移除 `RedisLeaseStore` 与相关键常量）。
- **Preconditions**：T3.1–T3.4 全绿 **且** 验收 1–5 通过。
- **Steps**：`delete-first`（内部代码，无外部契约）；`grep` 残留引用（含 `tests/`、`integration/`、`scripts/`、文档）。
- **Verification**：`cargo clippy --all-targets --features "server,cluster-redis,arachne" -D warnings`；`grep -rn "LeaseStore\|LeaderElection\|NodeRegistry\|LEASE_KEY\|MemoryLeaseStore" crates/ integration/ scripts/` ⇒ 零命中。
- **Retirement Track**：见 §Retirement（**不需要**用户 scoped 确认：Redis 侧只有 TTL 派生键，本方案不执行任何针对现网的清理命令）。

**T4.2 环境变量一次改齐**
- **Files**：`crates/hydra-server/src/cluster/mod.rs`、`scripts/check_cluster_env.cjs`、`environment/*.yml`、`dev-docs/{cluster,ops,deployment}.md`、`README*.md`、`LOCAL_TEST_ENV.md`。
- **Steps**：按 §兼容边界 的表改：新增 3 个、删除 7 个、`HYDRA_NODE_ID` 语义收紧；**同一提交内**改齐代码 / 清单 / 文档 / 脚本。
- **Verification**：`node scripts/check_cluster_env.cjs`；`node scripts/check_compose_health.cjs`；`grep -rn "HYDRA_ROLE\|HYDRA_CONTROL_URL\|HYDRA_PUBLIC_URL\|HYDRA_LEADER_LEASE_MS\|HYDRA_CONTROL_POLL_MS\|HYDRA_REGISTRY_STALE_GRACE_SECS\|HYDRA_FAILOVER_GRACE_MS" .`（排除 `.git`/`target`）⇒ 零命中。

**T4.3 验收与运维文档**
- **Files**：`dev-docs/cluster.md`（§1/§2/§3/§4/§5 重写）、`dev-docs/ops.md` §13（成员变更、force-recovery、WAL 目录与容量、内存上限、告警规则）、`dev-docs/design.md` §20（改为指针）、`dev-docs/aegis/INDEX.md`、**新增 ADR**。
- **Steps**：验收 1–7 逐条执行并留证据（命令 + 原始输出）；告警表新增 `hydra_arachne_leader_flips_total`、`hydra_arachne_publish_total{result}`、`hydra_arachne_config_bytes`、`hydra_arachne_quorum_unavailable_total`。
- **Verification**：证据落 `.acceptance/`；`.acceptance/*gate*.sh` 全绿（逐条记录退出码，禁止管道吞码）。

---

## Phase 4 执行记录（2026-10-05）

| Task | 状态 | 实际落地 |
| --- | --- | --- |
| **T4.1 前半（启动半边）** | ✅ 已完成（`6043441`） | 租约选举 / 快照轮询客户端 / 备用物化器不再启动；`control_client.rs`、`replica.rs` 删除；`snapshot.rs` 只剩 `HydratedWire`（线缆整个退役）；`/api/v1/internal/control` 删除；`leader_ready` 只剩一个来源 |
| **T4.1 后半** | ✅ 已完成（`f4f83e9`） | `lease.rs`（715）、`registry.rs`（743）、`RedisLeaseStore`、`LEASE_KEY`、关闭时的注销钩子、`registry_stale_grace_secs`、三套对应测试全部删除。**计划的退役 grep 归零**（`LeaseStore|LeaderElection|NodeRegistry|LEASE_KEY|MemoryLeaseStore`） |
| **T4.0 代码半边** | ✅ 已完成（`f4f83e9`） | `NodeRole` 收敛为 `All \| Cluster`；`Edge` 删除；`AdminState::edge_mode`、`is_leader_candidate()`、admin 路由的 edge 404 分支、`main.rs` 四处 edge 分支删除；`ClusterConfig` 去掉 `control_url` / `poll_interval` |
| **T4.0 清单半边** | ✅ 已完成 | `docker-compose.cluster.yml` 改为**三个同构成员**（同一 environment 锚点，只有 node id / raft 地址 / 发布端口 / 卷不同）；`docker-compose.local.yml` 去掉 `HYDRA_ROLE` / `CONTROL_URL` / `PUBLIC_URL`、补上 `HYDRA_CLUSTER_PEERS` + `HYDRA_ARACHNE_LISTEN`；`scripts/check_compose_health.cjs` 的角色分支删除（一条规则：每个节点都用 `Authorization: Bearer` 探 `/api/v1/health`）；`scripts/compose_static.cjs` 的三条角色拒绝规则删除；`admin-ui` 的 `alive` 改为三态渲染；`environment/{build.sh,release.sh}` 的特征集补齐 |
| **T4.2 环境变量** | ✅ 已完成（代码/文档/清单） | `CLUSTER_ONLY_ENV` 9→7；`RETIRED_CLUSTER_ENV` 2→**9**（`HYDRA_ROLE` / `CONTROL_URL` / `PUBLIC_URL` / `CONTROL_POLL_MS` / `LEADER_LEASE_MS` / `REGISTRY_STALE_GRACE_SECS` / `FAILOVER_GRACE_MS` / `FORWARD_TIMEOUT_SECS` + 一个哨兵名），并有一个启动 ERROR 点名；`ops.md` §13.3b 记录它们与被谁取代；两个守卫脚本的记录同步更新。**清单里残留的三处已清除**（见下） |
| **T4.3 验收与运维文档** | ⏳ 进行中 | ✅ **验收 2 已执行**（见上表行 2，20 rps / 60 s / 1200 次全 200，含证伪）。 ✅ **已移植 `integration/test_startup_knobs.py`**（12 条腿，13 项断言，全绿；CI 那步的特征集补上 `arachne`——没有它，带成员表的节点会**拒绝启动**，腿会因别的原因红）。✅ **集群可观测面已落地**：见下节"退役后的可观测面"。**尚未做**：`cluster.md` / `design.md` 逐条改写（`cluster.md` 只加了"该段描述已退役拓扑"的横幅）；**`dev-docs/jiqun-deploy.md` 整份仍是 leader/edge 时代**（已在文首加"已退役"横幅 + 改正日志字段名；**它此前不在任何 Task 的清单里**，本轮补进 T4.3） |

### 清单半边实际改出来的三个缺陷（都不是"文案问题"）

清单半边不是格式活：这三处都是**声明与事实不符**，而且当时全部门禁是绿的。

1. **`docker-compose.local.yml` 仍在设置三个已退役变量**：`HYDRA_LEADER_LEASE_MS`（a/b 两个节点）与 `HYDRA_CONTROL_POLL_MS`（a/b/c 三个节点）。它们在 `RETIRED_CLUSTER_ENV` 里，所以这套"本地集群"每次启动都会打 ERROR 说这些设置被忽略——文件本身却在告诉读者它们有用。**修法**：删掉这三处；**守卫**：`scripts/check_compose_env.cjs` 规则 1（表从源码读，不在这里复制第二份）。
2. **镜像配方缺 `arachne` 特征**：`environment/build.sh` 是产出 `environment/` 下每个清单所跑镜像的配方，它只写了 `server,cluster-redis,usage-clickhouse`。而设置 `HYDRA_CLUSTER_PEERS` 却没有 `arachne` 时二进制**拒绝启动**（`main.rs`），所以那个镜像**根本跑不起 `docker-compose.cluster.yml`**。CI 一直是绿的，因为 CI 自己的构建步骤单独带了 `arachne`。**修法**：`build.sh` / `release.sh` 补齐；**守卫**：`check_compose_env.cjs` 规则 2，所需特征**从清单推导**（清单开始用 raft 就把配方一起拽上）。
3. **`hydra-c` 没有卷**：本地栈的第三个成员没有任何 `/app/data` 挂载，于是它的 raft 日志与 SQLite 全在容器层——重启即重置，这不叫"三个同构成员"。**修法**：补 `hydra-c-data:/app/data` 与顶层卷声明。

### 退役后的可观测面（2026-10-05 修完，实测）

控制面换代之后，**最该能告警的两件事一件都告不了**，而且三处告警行指向**已经没有记录器的序列**——规则写得出来、Prometheus 收得下、**永远不会触发**：

| 发现的缺陷 | 事实 | 处置 |
|---|---|---|
| 三条告警行指向**死指标** | `hydra_registry_nodes{state="dead"} > 5`、`increase(hydra_registry_reaped_total[1h]) > 20`（回收器随注册表 T4.1 删除，两个序列**注册着但没有任何调用点**）、`changes(hydra_control_snapshot_version[10m]) == 0 and hydra_control_poll_total{result="ok"} > 0`（前者的记录器随轮询客户端删除、**仍以 0 导出**；`result="ok"` **从来没有被写过**，实际只有 `rate_limit_error` 与 `invalidation_trim_error`） | 三个序列与三条告警行**退役**，并在 §9.1 留 **RETIRED 行点名**（operator 拿着老 dashboard 里的名字来 grep，答案必须在他看的地方）；`check_documented_metrics` 的 `ABSENT_ON_PURPOSE` 记下三个名字与理由 |
| 文档让人盯一个**从不移动**的指标 | `ops.md` §13 让运维"Watch `hydra_replica_materialize_retries_total{outcome="failed"}`"，R7 也把它当作物化放大的对冲手段——而这个计数器**注册了但零调用点** | **接线**（`main.rs` 的物化循环：`attempt` / `succeeded` / `failed` / `throttled`，稳态 `NoChange` **不计**，否则每秒一次会淹掉信号）。实测：一次管理写后三节点各自 `attempt=1, succeeded=1` |
| 集群**没有任何指标** | `grep hydra_arachne` 在代码与文档里**零命中**：切主、发布失败、失去多数派——ADR-0001 引入的三种失效模式没有一种可告警 | 新增五个家族并接线（见下）+ §9.1 四条新告警行 |

**新增的五个家族（实测值来自一次真实三节点 + 一次管理写）**：

| 指标 | 类型/标签 | 实测 |
|---|---|---|
| `hydra_arachne_this_node_leader` | gauge `{node}` | `{node="m-b"} 1`，另两节点 `0` ⇒ `sum() == 0` 就是"集群没有写者"的告警 |
| `hydra_arachne_leader_flips_total` | counter | 当选的那台 `1`，全程没变过的 `0`（采样 gauge 看不到抖动，所以单独计数） |
| `hydra_arachne_publish_total` | counter `{result}` | `{result="ok"} 1`／每节点一次；`not_leader`、`quorum_unavailable`、`error`、`refused`（**编码期拒绝**：超 1 MiB、无法成键、密封失败——这是 R2 要的容量告警，比一个自己编的字节阈值诚实） |
| `hydra_arachne_config_bytes` | gauge | `430`（最近一次成功发布的树字节数，R2 的增长曲线） |
| `hydra_arachne_quorum_unavailable_total` | counter `{op}` | `publish` / `read`；新增 `StoreError::QuorumUnavailable` 变体，让"失去多数派"不再混在 `Arachne(String)` 里 |

`hydra_arachne_this_node_leader` **故意是 `IntGaugeVec{node}` 而不是裸 gauge**：裸 `IntGauge` 一注册就以 0 导出，于是**单节点部署**上 `sum(...) == 0` 的规则会**永久误报**（那个 0 的含义是"这里没有 raft"，不是"没有写者"）。`Vec` 在用到标签前不导出任何序列，序列只在问题有意义的地方存在——与 `hydra_invalidation_consumer_*{node}` 同一个理由。

**顺带修好的一处守卫缺陷**：`check_documented_metrics` 的 `ABSENT_ON_PURPOSE` 把环境覆盖**合并**进内建表，违反 `recorded_exceptions.cjs` 的第一条规则（**REPLACE, never merge**）。它一直不可见，因为内建表此前是空的；本轮的三个名字一进去，**11 条既有自测同时变红**（fixture 的文档当然不会拼出本仓的名字）。已改为走 `records()`，自测的公共环境补 `CDM_ABSENT_ON_PURPOSE: '{}'`。

### 本轮新发现（尚未处理，登记在案）

* **一个集群节点无法单独启动：新数据目录必须由多数派先"认领"**。`await_cluster_preflight` 的
  第一步是**本地读**（`get_stale` 读 `HANDSHAKE_CLUSTER_KEY`）：目录里**已经记着**正确的 cluster id
  ⇒ 立刻通过；**没记着** ⇒ 只能由 **leader** 写（`without_redirect()`，不转发），于是**单飞的成员
  等满 10 s（`PREFLIGHT_DEADLINE`）后拒绝启动**。**实测（2026-10-05，真二进制）**：三成员表 + 全新
  目录 + 只有一个进程 ⇒ `exit=1`，`no member adopted this node's Arachne data directory within 10s`；
  而**同一目录被认领过之后，单独重启只需 ~500 ms**（同样实测）。三个后果，都还没有对冲：
  1. **从全停恢复不能"先起一台"**：必须让 ≥2 台在 10 s 窗口内一起起来（compose 的 `up -d` 天然满足）；
  2. **K8s StatefulSet 的默认顺序启动起不来**：`podManagementPolicy: OrderedReady` 会等 pod-0 Ready
     才起 pod-1，而 pod-0 永远等不到多数派 ⇒ **必须 `Parallel`**（`deployment.md` / `cluster.md` 里的
     StatefulSet 片段尚未写这条，已随本文档的 T4.3 登记）；
  3. 失败信息只说了"没人认领你的目录"，**没说"让多数派一起起来"**——可操作性可以更好。
  **为什么不在本轮改行为**：那条检查正是"目录属于别的集群就拒绝加入"的守卫，放宽它需要独立裁定；
  本轮只把**事实、两个后果、以及演练已按此改写**记录在案。
* **`scripts/check_tenant_error_codes.cjs` 也是红的，而且从 T3.5 起就红**（CI 的 `scripts` 作业跑它）。
  它不是本地噪声：那 4 条被记为 DRIFT 的错误码正是**随转发层一起消失的对外契约**——
  `too_many_requests`(429)、`no_leader`(503)、`forward_result_unknown`(504)、`forward_failed`(502)；
  它们的发出站点在 `cluster/forward.rs` 与 `tenant_config/forward.rs` 里，而那两个文件在 `37f0bc3`
  （T3.5，D-6 乙-full）被删除。守卫读的是 `dev-docs/tenant-api-integration.md` §6 那张**对外契约表**，
  所以真正的问题不是守卫，而是**那张表仍在告诉集成方去按 `code` 重试**：`504 forward_result_unknown`
  ⇒ "先重读再重试"、`503 no_leader` ⇒ "必须换边缘节点"——**这两个码再也不会出现**，而
  §"传输语义"的 OC-4 例外（"写端点必须经由边缘，leader 自己返回 503"）描述的是已经删掉的转发世界。
  集成方照它实现的**重试逻辑会永远等不到那个分支**（不是崩溃，是静默失效）。**本轮未改**：这是
  **对外契约**的退役，需要独立裁定（改 §6 表 + §5.6 + OC-4 + 那张 retry 表），且要与
  `tenant-api-integration.md` 的其他读者一起看。

* **`integration/test_startup_knobs.py` 是红的：11 条失败**（实测 2026-10-05，`.github/workflows/ci.yml` 有它的步骤）。这份演练整份是围绕 `HYDRA_ROLE` 写的：K1/K2/K3 用 `HYDRA_ROLE=edge` 造"集群节点"、K4/K5/K6/K11 断言"角色写错但配了 wiring ⇒ 报出被丢弃的变量"、K8 用 `" edge "`。角色退役后这些前提到处不成立。**它自己的 K9/K12 仍绿，K7 的一半仍绿**——也就是说：不是整份作废，是**地基换了**。移植方案与 `test_cluster_limits.py` / `test_auth_cache_layers.py` 同一批（T4.3 的"移植"项），口径都是 `HYDRA_CLUSTER_PEERS`。
* **`HYDRA_CLUSTER_TOKEN` 现在是一个没有消费者的启动要求**：`/api/v1/internal/*` 这个路由族已经**一条都不存在**（随快照通道与转发的管理写一起退役），闸门代码还在（`admin/mod.rs:717`，任何该前缀的请求现在得到 401 而不是 404），而 `main.rs` 仍**要求**它存在且够强，三个清单也都用 `${HYDRA_CLUSTER_TOKEN:?…}` 强制它。也就是说：运维必须为一件没人读的东西准备一个秘密，而删掉它是**部署契约变化**（要连同清单、`CLUSTER_ONLY_ENV`、启动拒绝、`test_startup_knobs.py` K10 一起动），所以本轮只把它**写进启动点注释**（`main.rs` 那段契约里）并登记在此，**不改行为**。
* **`main.rs` 的集群模式拒绝文案曾点名退役变量**：原文 `cluster mode (HYDRA_ROLE=leader|edge) requires HYDRA_REDIS_URL`——运维照着这句话去设一个本产品会报"已忽略"的变量，真因就此丢掉，而**没有任何测试读过这个字符串**。已修，并加守卫 `cluster::tests::no_operator_facing_message_names_a_retired_variable`（扫 `main.rs` 的字符串字面量，**跳过注释**——第一版把注释里引用的历史文案误报成字面量，这本身也是个教训）。

**这一轮的一个额外发现（社区面）**：舰队视图 `/api/v1/cluster/status` 以前从注册表读「成员 + 每个成员是否存活 + 谁持租约」。注册表没了之后，**单个节点再也无法知道对端是否存活**（没有心跳表，也没有节点间 RPC），所以 `alive` 变成**三态**（本节点 `true`、对端 `null`），Admin UI 把 `null` 渲染为「未知」——把健康对端显示成「下线」是**没人测量过的断言**。

## 风险 / Risks

| # | 风险 | 概率 | 影响 | 对冲 |
|---|---|---|---|---|
| R1 | Arachne 是 v0.1.x、小团队项目，没有 Hydra 这类生产负载的运行史 | 高 | 高 | Phase 0 硬门；D-1 让数据面不依赖它（Arachne 挂了数据面照常服务）；代码级回退 = revert 提交并部署旧版本（无数据丢失，SQLite 与 Arachne 两份状态可重建） |
| R2 | 全内存状态机 + 容量硬上限；大配置触顶后**拒写** | 中 | 高 | 分片（256 KiB）+ 启动校验配置总量 + `hydra_arachne_config_bytes` 告警阈值；**同构后每个节点都吃这份内存**，容量要按最大值规划 |
| R3 | 依赖图冲突（tonic / rustls / protobuf 重复或版本不兼容） | 中 | 中 | Phase 0 T0.1 实测；不可接受则本计划作废（falsifier） |
| R4 | `Arachne::start` 每进程单例 | 低 | 中 | 同构设计下每进程正好一个 raft 节点，天然吻合；探针 T0.2-5 钉住 `AlreadyInitialized` |
| R5 | 无 CAS ⇒ 版本分配依赖「单写者」前提 | 中 | 高 | ~~管理写必须经 leader~~ **已被用户裁定（乙）取代**：任意节点都可就地写 + 就地发布，并发管理写**后写者胜、先写者被物化覆盖**（这是自带的、已知的暴露面，与 T3.5 租户写同一性质）；T2.2 用例③ 证明「head 不前进即无部分提交」；T3.3 用例③ 证明不读注册表 |
| R6 | 同构化是**产品语义变化**（撤回无状态数据面 + 2 节点不再容错） | 高 | 高 | 用户已裁定（D-2 / D-5）；T4.0 与文档/清单一并改，并在 `cluster.md` 顶部写明这条不变量 |
| R7 | 配置物化在每个节点各跑一遍 ⇒ 配置越大，全集群 CPU/IO 放大 N 倍 | 中 | 中 | 版本闸门（未变则空转）+ 分片复用（内容未变不重写）+ `hydra_replica_materialize_retries_total` 观测 |
| R8 | 集群扩容 = raft 成员变更（不再是拉起一个无状态副本） | 中 | 中 | `ops.md` 写明成员变更 SOP（`add_learner` → 追平 → `promote`）；扩容需人工一步，不能只改副本数 |
| R9 | WAL 落盘要求：每个节点需要持久卷（现仅 PVC 1Gi） | 低 | 中 | 容量估算与 `snapshot_threshold_bytes = 64 MiB` 的关系写进 `ops.md`；PVC 建议 ≥2Gi |
| R10 | ~~admin UI 自动重试隐藏了「当前 leader 正在切换」这一事实~~ **已随 D-3 消失**：没有 UI 重发层，就没有"重试成功但没告诉操作者"这件事 | — | — | 无需对冲。**仍然成立的相关事实**：管理写在切主瞬间失败时，操作者看到的是一个普通错误码，UI 不做静默重发——这是 D-3 的选择，不是缺口。舰队视图里"谁是 leader"由 `leader_hint` 显示（见 §leader 提示） |
| R11 | Arachne 的 tonic 传输默认带 rustls，与 Hydra 下游租户 TLS（OpenSSL/BoringSSL）是两件事 | 低 | 低 | 文档分开写：**Arachne 传输加密 ≠ 租户 TLS** |
| R12 | ~~服务端零转发 ⇒ 重试责任全在调用方，切主窗口内会看到 409~~ **已随 D-3 消失**：库自己把写转发给 leader，Hydra 侧零转发**不等于**调用方要重发——切主窗口内写会由库重试/由 raft 拒绝，**不产生 409** | — | — | 无需对冲。**D-3 的待答项已关闭**（"是否给 CLI/SDK 内置重发"），因为不需要重发层：`leader-hint` 响应头与 409 leader 契约都不存在 |
| R13 | 目录索引（toc）是单值，实体数很大时可能触到 1 MiB 上限 | 低 | 中 | T2.1 用「实体路径 + content-hash + len」的紧凑编码；启动时校验 `entity_count` 与 toc 大小；超过阈值（建议 >512 KiB）按实体类型分段，类型集合固定、无需扫描 |

---

## Retirement / 退役

```text
Anti-Entropy Declaration:
- Deletion Class: code-retirement（内部代码）+ live-state mutation surface（Redis 派生键，只删写入者）
- Old Path/Object: cluster/lease.rs 的 Redis 租约选举；cluster/registry.rs 的节点注册表与心跳；
  cluster/control_client.rs 的轮询与「按租约持有者轮换」；cluster/snapshot.rs 的线上推送路径；
  GET /api/v1/internal/snapshot；NodeRole::Edge 及其全部分支（无 DB / 无管理 CRUD / /metrics 免鉴权 /
  快照拉取）；redis/mod.rs 的 RedisLeaseStore 与 hydra:{lease|nodes|node:*|ctl:gen} 写入者
- New Canonical Owner: Arachne raft 领导权（leader 身份）+ ctl/head + 分片（配置版本与内容）
  + 每节点本地物化（读取路径）
- Expected Preserved Behavior: /healthz/leader 的 200/503；管理写最终落盘在 leader；
  数据面在控制面故障期间继续服务（改为「本地已物化状态」而非「edge 的 last-known-good」）；
  单节点零变化；租户侧契约（tenant-api-integration.md）不变
- Expected Retired Behavior: 租约续约与时间栅栏、注册表心跳/TTL/两击回收、转发目标注册表解析、
  跨节点配置推送、edge 角色与「无状态数据面」、以及 7 个旧环境变量
- External Boundary Touched: yes（部署 manifest 的环境变量、admin UI 的请求层与文案、运维演练步骤）
- Source-of-Truth Data Risk: none —— Redis 侧全是 TTL 派生键（租约 15 s / 心跳 30 s / 见证 120 s），
  配置权威在 SQLite（保留）与 Arachne（新增）；本计划**不执行**任何针对现网 Redis 或数据库的清理命令
- User Confirmation Required: no（无既有生产版本；删除目标均为进程内可达的内部代码；
  唯一的数据面动作是「不再写」那些键，它们会自行过期）
```

```text
Retirement Decision:
- Path: delete-first（内部代码：lease / registry / control-rotation / snapshot 推送 / edge 分支 /
  7 个旧环境变量）
- Why: 没有既有生产版本，也没有外部契约依赖这些 Rust 模块；新 owner（raft + 本地物化）已覆盖同一行为；
  删除与前缀替换在同一批提交内完成，不留双轨期
- Path: 无需 confirmation-first
- Why: 退役不触及不可重建数据 —— Redis 侧只有 TTL 派生键，停止写入即自然过期；
  SQLite 与 Arachne 两份配置状态都保留
- Non-edits: dev-docs/design.md §20 不复写（T4.3 改为指向本计划与新 ADR 的指针），避免同一事实两处维护
```

## 验收门禁 / Verification Gates

```bash
# 1) 构建与静态门禁
cargo fmt --check
cargo clippy --all-targets --features server -D warnings
cargo clippy --all-targets --features "server,cluster-redis,arachne" -D warnings
cargo build --release --features "server,cluster-redis,arachne"

# 2) 单元/集成
cargo test -p hydra-core
cargo test -p hydra-server --features server
cargo test -p hydra-server --features "server,cluster-redis,arachne"

# 3) 真实多节点验收（真 redis + 3 个同构节点）
python3 integration/test_arachne_control_plane.py      # 验收 1/3/4/5
python3 integration/test_cluster_ha.py                 # 验收 2（口径不变）
python3 integration/test_admin_write_redirect.py       # 验收 1 + D-3 的 UI 自动重试
python3 integration/test_cluster_limits.py             # 数据面未受影响（D-1 边界）
node --test scripts/admin_ui_render.test.cjs           # UI 重试层断言
node scripts/check_cluster_env.cjs
node scripts/check_compose_health.cjs

# 3b) Phase 0 回归（探针可重跑；F-1/F-2/F-3 的守卫）
grep -rn "\.get(" crates/hydra-server/src/cluster/arachne_*.rs || echo "OK: no linearizable get on the arachne path"
grep -rn "leader_hint()" crates/hydra-server/src/ ; # 只允许出现在「follower 找 leader」的转发路径上

# 4) 退役证据（全部必须零命中）
grep -rn "LeaseStore\|LeaderElection\|NodeRegistry\|LEASE_KEY\|hydra:{nodes}" crates/ integration/ scripts/
grep -rn "Role::Edge\|HYDRA_ROLE" crates/ scripts/ environment/ admin-ui/
grep -rn "HYDRA_CONTROL_URL\|HYDRA_PUBLIC_URL\|HYDRA_LEADER_LEASE_MS\|HYDRA_CONTROL_POLL_MS\|HYDRA_REGISTRY_STALE_GRACE_SECS\|HYDRA_FAILOVER_GRACE_MS" crates/ integration/ scripts/ environment/ admin-ui/ dev-docs/ README.md README.zh-CN.md
```

## 回滚 / Rollback

- **阶段内**：每个 Task 一个提交，独立可 revert；Phase 0 不产生产品代码，最坏情况零影响；
- **代码级回退**（唯一路径，因为不留双轨）：revert 相关提交 → 重新部署旧镜像 → 旧集群按老方式工作（Redis 租约）；
- **数据**：Arachne 的 `ctl/*` 是新增键空间，停用即回滚；SQLite 从头到尾都是配置的实际存放处，**回滚不丢配置**；
  Redis 侧的租约 / 注册表键会自行过期（≤120 s），无需人工清理；
- **不可回滚点**：T4.0 与 T4.1（角色分支与旧代码删除）。在这两个 Task 之后回退需要 revert 提交并重新部署旧版本；
  由于配置两份状态都在，数据无损失；因此在执行 T4.0/T4.1 前必须先过验收 1–5。

---

## 附录 A — Arachne 事实表（本次会话实测/直读，供复核核对）

| 事实 | 值 | 来源 |
|---|---|---|
| crate / 版本 | `arachne-kv` **0.1.1**（crates.io 可达，docs.rs 可达）；下游 raft 为 **fork `raft-seedable` 0.7** | `arachne/Cargo.toml` + crates.io/docs.rs HTTP 200 |
| 工具链 | `rust-version = 1.93`、edition 2024、resolver v3 | Arachne `Cargo.toml` |
| 客户端 API | `put/get/get_stale/delete`、`add_learner/promote_learner/remove_member/membership/transfer_leader`、`leader_hint`、`without_redirect`、`register_peer`、`node_id` | `arachne/src/client/handle.rs` |
| facade | `Arachne::start(ClusterConfig)` / `set/get/get_stale/delete/handle/shutdown`；**每进程至多一个节点** | `arachne/src/server/mod.rs` |
| 多节点装配（测试用） | `assemble_cluster(ClusterConfig) -> AssembledClusterNode{handle, thread, tonic}` **不占用那个单例** ⇒ 单进程可以起多个节点（Hydra 的集成用例靠它）；生产路径仍走 `Arachne::start` | `arachne/src/server/mod.rs:588-680` |
| 一致性 | `get` = ReadIndex 线性读（需 quorum）；`get_stale` = 本地读，**不保证单调**；CP 系统 | README + `lib.rs` |
| 错误 | 16 变体；关键：`NotLeader{hint}`(409)、`QuorumUnavailable`/`Busy`/`Timeout`/`ShuttingDown`(503)、`InvalidArgument`(400)、`LearnerNotCaughtUp`(412)、`ConfChangePending`(409)、`AlreadyInitialized`/`NotInitialized` | api-reference §5 |
| 上限 | `max_value_bytes = 1 MiB`、`max_key_bytes = 4 KiB`（LAN profile，`handle.rs` 在 propose 前校验） | `ProfileConfig::lan()` + `handle.rs:589-605` |
| 存储 | 全内存状态机 + 段式 WAL（`fdatasync` + `fallocate`）+ 快照；`snapshot_threshold_bytes = 64 MiB` | propsol D1 + README 持久化节 |
| 缺失能力 | **无 CAS/条件写、无范围扫描、无 Watch/订阅、无 TTL（TTL 列为 v1.5、Watch 列为 v2）** | propsol D3 + `handle.rs` 全量 API |
| 分片/传输 | `tonic`（gRPC/HTTP2 + rustls），`SnapshotChunk` 流式 + 令牌桶限速 | propsol T1 |
| 已知未打通项（上游自述） | 快照「失败重试」路径的状态回报仍未打通（`MsgSnapStatus` 被 raft 更早分派阶段丢弃，第 15 轮定位） | propsol §「仍未打通」 |
| 运维面 | `arachne-node`：`/kv/*`、`/members*`、`/readyz`、`/metrics`（Prometheus）、`force-recovery` 子命令 | README HTTP 表 |
| 许可 | `MIT OR Apache-2.0`（Hydra 是 Apache-2.0） | Arachne `Cargo.toml` |

> 本机副本：`/home/alex/Projects/hydra/.arachne-research/`（文档 HTML + `arachne-src/` main checkout）。
> **本次评估固定在上游 commit `befa3d2`**（2026-10-05）—— 本表所有 Arachne 事实出自该 commit；
> 实现期若上游前进，需重新核对本表（尤其 §上限 / §缺失能力 / §已知未打通项 三行）。
> 该目录**目前未纳入 `.gitignore`**，在 `git status` 中显示为 `??`（4.4 MB）；建议实现期顺手加入忽略，避免误提交第三方副本。

## 附录 B — 需要出 ADR 的问题清单

ADR 的正文范围（其余实现细节留在本文档）：

1. **为什么要引入共识层**：现状 leader 唯一性由 Redis 租约的时间栅栏近似；具体缺陷（11–18 s 切换窗口、
   同 `HYDRA_NODE_ID` 双主、三层转发护栏）与「只有共识能给出至多一个 leader」的论证。
2. **为什么只换控制面**（D-1）：Arachne 无原子计数器 / 无 Lua / 无订阅 / 无 TTL，
   限流与熔断留在 Redis 的**技术**理由，以及「Redis 不是权威」这条基线的边界。
3. **为什么所有节点必须同构**（D-2）：撤回「无状态数据面」的代价（每节点要有持久卷、
   扩容变成 raft 成员变更、配置物化在 N 个节点各跑一遍）与收益（无角色分支、读零往返、控制面故障不影响数据面）。
4. **提交点与分片**（D-4）：为什么用「内容寻址分片 + head 清单」而不是单值或每实体一键；
   1 MiB 单值上限的硬约束、GC 与保留窗口、分片大小是否可配。
5. **管理写被拒的消解位置**（D-3）：服务端返回结构化 leader 提示、admin UI 自动重试一次；
   这条对外契约（409 错误体 + `x-hydra-leader-hint`）的稳定性承诺。
6. **HA 规模前提**（D-5）：至少 3 台奇数节点；2 节点是「拒绝启动」还是「启动但响亮告警」。
7. **可逆性**：什么情况下应当回退（Phase 0 硬门失败 / 上游 Arachne 停止维护 / 生产出现不可解释的
   一致性故障），以及回退路径（revert + 重新部署，配置无损失）。
