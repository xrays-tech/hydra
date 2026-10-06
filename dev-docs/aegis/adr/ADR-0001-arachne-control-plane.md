# ADR-0001 — 用 Arachne（raft 线性化 KV）取代 Redis 控制面

- **Status**：`recorded-from-plan`（决策已定稿；实现从 Phase 1 开始，见 §实施计划）
- **Date**：`2026-10-05`
- **Deciders**：用户（架构方向与六条决策的最终裁定）
- **Source Evidence**：`dev-docs/aegis/plans/2026-10-05-arachne-control-plane.md`（实施计划，含 Phase 0 实测与上游修复复核）
- **Owner Surface**：本文件位于本仓既有的 `dev-docs/aegis/` 工作区（Hydra 的 Aegis 工作区约定；**不是** `docs/aegis/`）
- **Boundary**：本 ADR 是 advisory 记录，不授予完成权，也不取代 `dev-docs/cluster.md` / `dev-docs/ops.md` / `dev-docs/design.md` 这些项目权威文档

---

## 1. 背景（Context）

Hydra 的集群协调今天建立在「Redis 是可靠的单点协调者 + 时钟可信」这两个前提上，用**租约的时间栅栏**近似 leader 唯一性。实测与代码事实：

| # | 事实 | 证据 |
|---|---|---|
| C1 | 故障切换窗口 **11–18 s**（租约 15 s + 选举 tick 5 s，两次独立 `SIGKILL` 实测 17.7 s / 18.2 s） | `dev-docs/cluster.md` §5.1 |
| C2 | 「谁是 active」在**三处各存一份**：lease（谁持租约）→ registry（谁是 active URL）→ forward（转发目标） | `cluster/{lease,registry,forward}.rs` |
| C3 | 同一 `HYDRA_NODE_ID` 的两个进程会**同时认为自己是 leader**（该 id 同时是租约身份） | `cluster.md` §3.1 |
| C4 | 管理写被拒时的转发链需要三层防御（注册表实时解析 + 自转发护栏 + forward-once 标记）才不至于递归 | `cluster.md` §5.2 |
| C5 | Redis 断连后曾**一次都不重拨**（`fred` 无重连策略），租约 90 s 拿不回来 | `cluster.md` §3 的 2026-09-30 修正 |

这些不是配置问题，而是**结构缺口**：没有共识层时，「至多一个 leader」只能被近似，无法被保证。

上游 Arachne 提供 raft 线性化 KV（crate `arachne-kv`，`raft-seedable` 内核），它给出的正是这个保证。

## 2. 决策（Decision）

**引入 `arachne-kv`（pin `0.1.2`）作为集群控制面的基座**，具体形态：

1. **leader 身份 = raft 领导权**。`leader_ready` 由「Redis 租约未过期」改为**写探测**（`without_redirect().put(探测键, 本节点id)` 成功即 ready）。
2. **配置权威 = Arachne 的 key-path 结构化存储**：
   - `hydra/ctl/head` = **一个内容哈希**（配置的提交点，最后写）；
   - `hydra/cfg/<toc-hash>/toc` = 目录索引，`hydra/cfg/<toc-hash>/<entity-type>/<id>` = 每实体一键；
   - 读取一律 `get_stale` + content-hash 校验；**全案不使用 `get`**。
3. **节点同构**：集群里每个节点完全一样 —— 都跑数据面、都跑管理面、都是 raft 成员、都能当 leader。**`edge` 角色与「无状态数据面」退役**；配置不再跨节点推送，改为每个节点各自从 Arachne 物化。
4. **写路径交给库转发**：写可打在任意节点，库在 `NotLeader` 时经 `Forward` RPC 转发到 leader（会话表保证幂等）。Hydra 侧**不实现**跨节点转发，`cluster/forward.rs` 与 `x-hydra-forwarded` 护栏退役。
5. **租户自助写例外（D-6）**：那 4 个数据面端点的身份闸唯一在**入口节点**；不重复鉴权（删 leader 侧 `reauth` + 授权绑定），`/api/v1/internal/tenant-config/*` 整族删除，**不留任何 Hydra 转发器**，**不加新鲜度闸**。
6. **Redis 只留数据面热路径**：限流（Lua 滑动窗口）、熔断投票、认证 L2、失效事件流（Streams）**不迁移**。
7. **单节点模式不受影响**：未配集群变量时不启动 Arachne、不需要 Redis，行为与今天一致。

## 3. 六条决策记录（D-1…D-6，含被否决的备选）

| # | 决策 | 备选（真实存在过的） | 裁定 |
|---|---|---|---|
| **D-1** | 只替换控制面；Redis 留在数据面（限流/熔断/认证 L2/失效总线） | ① 全面替换（需自建事件流 + 原子计数器，Arachne 无 Lua/订阅/TTL）；② 只换 lease + registry | 用户裁定 |
| **D-2** | 节点同构：全部是 raft 成员、都跑数据面与管理面；`edge` 与「无状态数据面」退役 | ① edge 做 raft learner（本地读、低延迟，但需 WAL + 成员变更）；② edge 纯 HTTP 客户端 + last-known-good | 用户裁定 |
| **D-3** | 写打在任意节点，由库透明转发到 leader；Hydra 不需要 409/提示/UI 重试层 | ① 非 leader 返回 409 + leader 地址 + admin UI 重发（上游修复前的设计，已被取代）；② 只返回裸错误 | 用户裁定 → 上游 `bce2943`/0.1.2 修复后**该需求消失** |
| **D-4** | 配置 = key-path（版本化目录 + 每实体一键 + head 单一提交点）；`head` 是**内容哈希**不是版本号 | ① 整体 blob + 字节分片（删一个实体导致后续分片全部移位 ⇒ 近乎全量重写）；② 每实体一键但用版本号做提交点（`get_stale` 不保证单调 ⇒ 可能拼出从未存在过的配置） | 用户要求重新设计后定稿 |
| **D-5** | 集群 HA 前提 = **至少 3 台奇数同构节点** | ① 保持 2 台（raft 下无容错）；② 引入外部协调服务 | 用户裁定 |
| **D-6** | 租户自助写：身份闸唯一在入口节点，不重复鉴权，收口到那 4 个端点 | ① 保留三道闸（含 leader 重鉴权）；② 请上游在转发协议里带调用方上下文；③ 乙-lite 先删身份保留转发作过渡（用户否决） | 用户裁定（乙-full） |

## 4. 「为什么引入共识层」的论证

- 「至多一个 leader」是**安全性**属性（不允许两个写者），时间栅栏只能近似它：任何租约、时钟、身份（C3）抖动都直接变成双写窗口；
- 共识给出的是**硬保证**：raft 的 leader 唯一性不依赖时钟质量；
- 收益是可量化的：切换窗口从 **11–18 s**（C1）降到**选举超时量级**（LAN profile 1 s，实测 2 s 内选出 leader）；
- 顺带消掉 C2/C4 两类**重复 owner**：转发目标不再需要「注册表实时解析 + 自转发护栏 + forward-once」三层防御，因为库自己会转发、且非 leader 会拒绝写。

## 5. 「为什么不重复鉴权」（D-6 的核心论证）

三道闸里被删掉的两道是「leader 用同一份 `ConfigStore` 再鉴一次租户」与「leader 侧授权绑定」。可以删的依据是**代码结构本身**：

> 写核心 `admin/sub_tenant_write.rs` 在**一个事务内**（`BEGIN IMMEDIATE`）重读全部活的 DB 行 + 当前 `ConfigData`，然后校验这次写（配额计数含 disabled 行、前缀重叠、validate-then-insert 的 TOCTOU、资源存在性）。归属正确性主要由**事务内校验**承载，而不是靠第二道令牌鉴定。

因此**重复的是「令牌鉴定」而不是「写入校验」**：后者无论如何都会跑，且必须跑。

**如实记录两条放弃项**（不粉饰）：

1. 「执行写的节点按最新配置再确认归属」这道闸没有了。入口节点快照落后（令牌刚轮换、子租户刚改）时，会在旧视图下放行一次写。缓解：事务内校验仍在；配置收敛实测 ≤7 ms + 1 s 轮询。影响面是「基于稍旧视图的一次合法写」，不是越权写。
2. 入口节点被攻破后可冒充任意租户写。**这不是新增能力**：任何集群节点本来就持有 `HYDRA_ENCRYPTION_KEY` 与全量配置快照（可解密 provider key 与证书私钥），节点级攻破今天等于全集群失守。

## 6. 后果（Consequences）

**收益**

- 切换窗口 11–18 s → 选举超时量级；单写者由共识保证；
- Hydra 侧转发层与管理写重试层不再需要（少两层代码 + 少一条对外契约）；
- 配置写入变为「只写变化的实体」（key-path + 内容寻址），小改动不再触发全量重写。

**代价（明确记账）**

- HA 前提变为 **≥3 台奇数同构节点**；2 节点在新方案下**没有容错**（必须写明并可被拒绝/告警）；
- **每节点需要持久卷**（raft WAL + SQLite）；
- **扩容 = raft 成员变更**（`add_learner` → 追平 → `promote`），不再是拉起一个无状态副本；
- **配置物化在 N 个节点各跑一遍**（版本闸门 + 分片复用压低成本，但仍是 N 倍）；
- **租户写限流**移到入口节点后是每节点进程内窗口 ⇒ 集群总量上界 = N × 单窗口（沿用「反 DoS 而非计费」定位，不引 Redis 计数）；
- 依赖一个新引入的第三方共识库（`arachne-kv` v0.1.x）。

## 7. 兼容边界（Compatibility Boundary）

| 面 | 边界 |
|---|---|
| 环境变量 | **新增** `HYDRA_CLUSTER_PEERS` / `HYDRA_ARACHNE_LISTEN` / `HYDRA_ARACHNE_DATA_DIR`；**删除** `HYDRA_ROLE` / `HYDRA_CONTROL_URL` / `HYDRA_PUBLIC_URL` / `HYDRA_LEADER_LEASE_MS` / `HYDRA_CONTROL_POLL_MS` / `HYDRA_REGISTRY_STALE_GRACE_SECS` / `HYDRA_FAILOVER_GRACE_MS`。集群判定改为「有没有配 `HYDRA_CLUSTER_PEERS`」。**无既有生产版本 ⇒ 不留别名、不留双轨**，代码/清单/文档/脚本同一批改齐 |
| 对外 HTTP | `/healthz/leader`（200/503）、`/api/v1/internal/*`（cluster token）保留；**不再引入** 409+leader 地址契约；无 leader 时 503（可重试） |
| 数据面 | 单节点与集群下都在本节点服务；各节点用本地已物化状态服务，控制面故障期间数据面不受影响 |
| 租户侧契约 | `tenant-api-integration.md` 的三端点语义不变；那 4 个写端点的路由与鉴权不变（变的只是"谁执行"） |
| 持久化 | 本地 SQLite **降级为可重建的物化状态**；`head`（Arachne）是权威，不一致时以 Arachne 为准（必须写进 `store.rs` 注释，否则"本机改了 DB 就算数"会变成第二个真相） |
| Arachne 依赖 | pin `0.1.2`；**不可用 0.1.1**（旧行为：follower 写不转发、无运行时启动 panic、新 leader hint 不自指） |
| Arachne 传输 TLS | Arachne 的 tonic 传输加密**不等于**下游租户 TLS（OpenSSL/BoringSSL），两者开关与文档分开 |

## 8. 退役影响（Retirement Impact）

`delete-first`（内部代码，无外部契约；Redis 侧只有 TTL 派生键，**不触及不可重建数据，无需用户确认**）：

| 退役对象 | 替代者 |
|---|---|
| `cluster/lease.rs`（Redis 租约选举） | raft 领导权 |
| `cluster/registry.rs`（节点注册表/心跳/回收） | Arachne 成员关系 + 静态地址表 |
| `cluster/control_client.rs`（轮询/按租约持有者轮换） | 每节点本地物化 |
| 快照推送路径与 `GET /api/v1/internal/snapshot` | 每节点从 Arachne 物化 |
| `NodeRole::Edge` 及其全部分支 | 节点同构 |
| `cluster/forward.rs`、`tenant_config/forward.rs`、`admin/tenant_config_api.rs` | 库内转发（管理写）+ 入口节点就地执行（租户写） |
| `x-hydra-forwarded`、`x-hydra-tenant-token`、`TENANT_TOKEN_HEADER` | 不再需要 |
| 7 个旧环境变量 | 新的三变量 + 集群判定 |

**保留**：`admin/sub_tenant_write.rs`（唯一写核心）、操作员侧管理入口、`content.rs` 的编解码（改为服务 Arachne 值）。

**不可回滚点**：`T4.0`（同构收口）与 `T4.1`（旧代码删除）；在此之前必须先过验收 1–5。回退路径 = revert 提交 + 重新部署旧版本，**配置无损失**（SQLite 与 Arachne 两份状态都在）。

## 9. 基线同步（Baseline Sync）

- **Needed**：yes
- **Target**：`dev-docs/cluster.md`（§1 角色 / §2 环境变量 / §3 共享状态 / §4 拓扑 / §5 演练）、`dev-docs/ops.md` §13、`dev-docs/design.md` §20、`dev-docs/deployment.md`、`README.md` / `README.zh-CN.md`、`environment/*.yml`
- **Action**：update baseline（随实施同步；**当前这些文档仍描述 Redis 租约世界** —— 在 Phase 4 的 T4.3 完成前，它们与 ADR 不一致是**已知的、被跟踪的**状态）
- **Reason**：本决策改变 canonical owner（leader 身份、配置权威）、外部环境变量契约、部署拓扑前提（≥3 同构节点）与退役清单 —— 属基线同步必须覆盖的四类
- **在完成基线写回之前，本 ADR 不得被当作"当前架构已如此"来引用**

## 10. 实施与证据（Evidence）

**实施计划**：`dev-docs/aegis/plans/2026-10-05-arachne-control-plane.md`（Phase 0 已完成 → Phase 1–4）

**Phase 0 实测（已完成，硬门通过）**：

- 依赖图：`pingora 0.8` + `arachne-kv` **可链接进同一二进制**；`cargo tree --duplicates` 无 `tokio`/`hyper`/`rustls`/`tonic` 重复主版本；`protobuf 2.28.0` 两侧相同。证据 `.arachne-research/results/duplicates.txt`
- 行为探针 13 项全部 PASS（含：follower 写被拒且提示精确、1 MiB 上限在 propose 前强制、失多数派时写/线性读失败而 `get_stale` 仍服务、单例生命周期、外部 current-thread 运行时驱动、三节点 2 s 内选出 leader）。证据 `.arachne-research/results/probes.log`
- **三条假设被实测证伪并已改设计**：F-1 `Arachne::start` 无运行时上下文即 panic（→ 必须在自己运行时里引导）；F-2 follower 的 `get` 立即 `QuorumUnavailable`（→ 提交点改内容哈希、全案禁用 `get`）；F-3 新 leader 自己的 `leader_hint` 不指向自己（→ `leader_ready` 改写探测）

**上游修复复核（2026-10-05，`bce2943` → 发布 `0.1.2`）**：

- 五条反馈全部落实；探针切回 **crates.io 的 0.1.2** 重跑 **13/13 PASS**。证据 `.arachne-research/results/probes-after-fix.log`、`probes-0.1.2-published.log`
- 修复带来**三处简化**：写打在任意节点由库转发（D-3 的 409/hint/UI 重试层不再需要）、`cluster/forward.rs` 整文件退役、唯一保留项是租户自助写（即 D-6）
- 一条探针自身的缺陷也记录在案：单节点首写窗口返回的是 `Timeout`（可重试），不是 `NotLeader` —— 曾一度误报为库的回归

## 11. 可逆性（Reversibility）

| 情形 | 动作 |
|---|---|
| Phase 0 硬门失败（依赖冲突等） | 已通过；该门是唯一"整案作废"的触发点 |
| 上游停止维护 / 出现不可解释的一致性故障 | revert 相关提交 → 重新部署旧镜像 → 回到 Redis 租约路径；**配置无损失**（SQLite 与 Arachne 两份状态都在） |
| 仅想回退同构化 | 需 revert `T4.0`（同构收口）；此前应先确认 ≥3 节点前提是否已被部署采用 |
| 想换回 crates.io 更早版本 | **不可**：0.1.1 缺三项本方案依赖的行为（见 §7） |

---

## Boundary

This ADR is an advisory Aegis Method Pack record. It does not grant completion authority or replace project-authoritative architecture sources.
