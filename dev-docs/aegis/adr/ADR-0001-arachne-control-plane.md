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

## 3. 九条决策记录（D-1…D-9，含被否决的备选）

> D-1…D-6 为方案定稿时的裁定；**D-7、D-8 是实现期追加的**，**D-9 取代了 D-8**（D-8 的「已存密文」前提核对后不成立）。三条都不是"顺手加的"：D-7 不加就重建不出同一份副本；D-8 不遵守就无限重新物化，而 D-9 是把这条约束真正满足掉的做法。

| # | 决策 | 备选（真实存在过的） | 裁定 |
|---|---|---|---|
| **D-1** | 只替换控制面；Redis 留在数据面（限流/熔断/认证 L2/失效总线） | ① 全面替换（需自建事件流 + 原子计数器，Arachne 无 Lua/订阅/TTL）；② 只换 lease + registry | 用户裁定 |
| **D-2** | 节点同构：全部是 raft 成员、都跑数据面与管理面；`edge` 与「无状态数据面」退役 | ① edge 做 raft learner（本地读、低延迟，但需 WAL + 成员变更）；② edge 纯 HTTP 客户端 + last-known-good | 用户裁定 |
| **D-3** | 写打在任意节点，由库透明转发到 leader；Hydra 不需要 409/提示/UI 重试层 | ① 非 leader 返回 409 + leader 地址 + admin UI 重发（上游修复前的设计，已被取代）；② 只返回裸错误 | 用户裁定 → 上游 `bce2943`/0.1.2 修复后**该需求消失** |
| **D-4** | 配置 = key-path（版本化目录 + 每实体一键 + head 单一提交点）；`head` 是**内容哈希**不是版本号 | ① 整体 blob + 字节分片（删一个实体导致后续分片全部移位 ⇒ 近乎全量重写）；② 每实体一键但用版本号做提交点（`get_stale` 不保证单调 ⇒ 可能拼出从未存在过的配置） | 用户要求重新设计后定稿 |
| **D-5** | 集群 HA 前提 = **至少 3 台奇数同构节点** | ① 保持 2 台（raft 下无容错）；② 引入外部协调服务 | 用户裁定 |
| **D-6** | 租户自助写：身份闸唯一在入口节点，不重复鉴权，收口到那 4 个端点 | ① 保留三道闸（含 leader 重鉴权）；② 请上游在转发协议里带调用方上下文；③ 乙-lite 先删身份保留转发作过渡（用户否决） | 用户裁定（乙-full） |
| **D-7** | 配置树的**内容范围**：除 `ConfigData` 外，还包含（a）它推导时丢弃的行（`EntityPath::Fidelity`）与（b）`skip_serializing` 丢掉的**秘密**（cert 私钥，随 `cert` 实体密封；此前只有 provider key 与令牌哈希） | ① 只复制 `ConfigData`（原计划的字面做法）：副本重建不出同一份 SQLite —— 丢 `enabled=0` 的 `limit_role`、`status != 1` 的 `provider_model`，且 provider key 的 `id`/`created_at` 会**被重铸**；② 副本"尽力而为"重建、缺的行当不存在：节点间表内容合法地不同，而 `head` 只声明"配置是这一份"；③ cert 私钥不进树（第一版的真实状态，被 `tests/arachne_cert_fidelity.rs` 判红）：`restore_config` 不是"少写一个字段"而是**往 `cert_key_ciphertext` 写 NULL** ⇒ 物化一次就**删掉**该节点已有的租户 TLS 私钥 | 实现期裁定（用户选 A）；cert 部分是**实测抓出的缺陷修复**，非新增选项 |
| **D-8** | **密封必须在编码之外**：发布方传入已封好的材料（`SealedMaterial`），编码器不持有主密钥。推论（同一类，各自实测抓出一处）：**编码结果只能依赖逻辑内容，不能依赖进程状态** —— ② `HashSet` 按迭代序序列化（`RandomState` 每实例随机）同样让"没变的配置"换树名 | ① 由编码器在 `split_config` 里现封：AES-GCM 每次新 nonce ⇒ **同一个没变的配置每次发布都换一个树名**（第一版就是这样，被 `the_same_inputs_name_the_same_tree` 抓出）；② 直接 `encode(&path, &HashSet)`：同一逻辑集合两次读库得到不同字节序（被 `two_independent_builds_of_the_same_config_name_the_same_tree` 抓出；该用例**故意分两次构建配置**——单元素 fixture 与"同一对象切两次"都抓不到） | ~~实测强制~~ **已被 D-9 取代（2026-10-05）**：D-8 依赖的「已存密文」前提经核对不成立，见 D-9 |
| **D-9** | **确定性密封**：`KeyProvider::seal_deterministic`，nonce = `HMAC-SHA256(主密钥, 域分隔‖key_version‖明文)[..12]`；**编码器自己封**（调用方供料的 `SealedMaterial` 整个删掉），树名成为**逻辑配置**的纯函数，任意节点发布字节相同 | ① D-8 原方案「读库里已存的密文」：只对 2/3 的秘密成立 —— provider key 的实体是「整供应商的 `Vec<String>` 再封一次」，与逐行密文形状不同；**租户令牌哈希在库里根本没有密文**（`tenant.access_token_hash` 是明文 hex）；② 为令牌哈希补一列密文 + 实体改成逐行携带密文 + 让物化节点把树里的密文原样写入：一个迁移 + 实体重做 + restore 签名扩展，且仍依赖各节点密文一致；③ 保持随机 nonce 并接受重命名：每次发布换树名，内容寻址的「没变就不重写」作废 | **用户裁定 A**（2026-10-05）。代价写进 `crypto.rs`：相同明文封出相同密文（同一棵树内相等可见）、**不是 SIV**、不提供误用容忍；换来的是不同明文不可能共用 nonce、轮换改变所有 nonce，以及不再需要任何跨节点密文保存 |


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

**实施进展（2026-10-05，T1.3 / T2 / T3.1）**：

| 已落地 | 提交 | 说明 |
| --- | --- | --- |
| 集群装配与同构启动 | `a421580`、`0dca1c6`…`c41185d`、`4ca5ee5` | `arachne` feature 下挂载 `cluster::arachne_*`；`Arachne::start` 在自己的运行时里引导（F-1 的直接后果）；`leader_ready` 由写探测回答（F-3） |
| 成员表取代 `HYDRA_ROLE` | `b87c289` | 集群判定 = 有没有配 `HYDRA_CLUSTER_PEERS`；`NodeRole::Edge` 退役 |
| 物化循环端到端 | `2176f14` | 真 raft + 真 store：`head` → toc → 逐实体 → 解码 → 安装 → **成功后才推水位**；退避 1s→2s→…→60s；未物化 = 不可当选 |
| fidelity 实体（D-7）与编码期密封（D-8） | `8ef3692` | `EntityPath::Fidelity`；`SealedMaterial` 由调用方传入 |
| cert 私钥随树走（D-7 的 (b)） | 本轮 | `cert` 实体改为 `CertTreeEntity{meta, sealed_key}`；解码时回填 `cert_key_pem`；缺少密封材料**拒绝发布**而不是丢掉密钥；`TOC_FORMAT` 1→2（同一批字节两种解法的版本必须互相拒绝） |
| 集合的编码序（D-8 推论②） | 本轮 | `tenant_providers` / `tenant_models` 是 `HashSet`，改为**排序后**编码（`sorted_members`），否则同一配置每次读库换树名 |
| 解码必须复现 loader 的**推导规则** | 本轮 | ① 四个 `Vec` 的排序原先一律按 `id`，而 loader 用的是 SQL `ORDER BY`（`limit_role.created_at,id` / `binding.key_prefix` / `sub_tenant.tenant_id,name` / `route.sub_tenant_id,model_key`）⇒ 副本持有的顺序与 leader 不同；② `tenants_by_domain` 的键在 loader 侧是**小写域**，解码侧却拿值里的原样域名当键 ⇒ 存储域名为混合大小写时，副本上的租户落在**没人会去查的键**下（`proxy::resolve_tenant` 用小写 `Host` 查表），于是同一租户在 leader 上解析得到、在副本上解析不到。守卫：`tests/arachne_derivation_fidelity.rs`（两处都先红后绿，fixture 让"loader 序"与"id 序"反向、让域名非小写） |
| 真实 target（`ReplicaTarget`） | 本轮 | 先 `db::restore_config`（一个事务重建本地 SQLite）**再** `ConfigStore::apply_snapshot`（换入内存态）：反了会让节点服务一份自己库里没有的配置；`MaterializeTarget` 相应改为 `async`。守卫用例连"把副本自己的库用 loader 读回来 == 发布的那份配置"都钉住了 |
| toc 解码器缺判别值 12/13 | 本轮 | `TenantProvider`/`TenantModel` **能写不能读** ⇒ 配置里只要有一条授权，`read` 在 toc 处失败、物化无限重试、**全集群零物化**。躲过既有用例的原因：没有 fixture 带授权走过真实 toc。补 12/13，并加 `every_entity_kind_survives_a_toc_round_trip` 把这一类堵死（能当键用 ⇒ 必须能解码） |

**实现期抓出的一个缺陷（如实记账）**：`8ef3692` 的提交信息写「树携带副本所需的、`ConfigData` 推导时丢掉的行」，**这句话当时是过强的**——它漏掉了 cert 私钥。`CertMeta::cert_key_pem` 带 `skip_serializing`，Redis 时代的 wire 用一个**单独的**密封字段（`SnapshotWire::sealed_certs`）补偿这个 skip，而树没有对应字段。后果不是"少一个字段"：`restore_config` 见到 `cert_key_pem == None` 会往 `cert_key_ciphertext` 写 NULL，于是**物化一次就删掉该节点已有的租户 TLS 私钥**（与 G1 丢禁用行、G3 重铸 provider-key 主键同一类）。已由 `tests/arachne_cert_fidelity.rs` 判红后修复。

**`SealedMaterial` 的构造来源（一处必须说清的现状）**：目前唯一的构造函数是 `seal_plaintext`（**现封**，因此不可复现，只适合测试与一次性发布）。生产用的"读库里**已存的密文**（`provider_key.api_key_ciphertext`、`tenant.cert_key_ciphertext`）"这一路径**还没写**，属 T3.2 的发布切换；现在**没有任何 `db` 读取函数返回已存密文**（`list_provider_keys` 等一律解密后返回）。顺带纠正 `8ef3692` 提交信息里的第二处措辞：`SnapshotWire::build` **不是**"已存密文的来源"，它每次 build 都用 `kp.seal(..)` 重新封——这对"版本号命名"的 wire 是对的，对**内容寻址**的树是致命的。

**尚未落地（引用本 ADR 时不得当作已完成）**：

1. **发布侧仍走 `SnapshotWire`**（属 T3.2），而且**启动时没有人拉起物化循环**（没有 spawn）。`ReplicaTarget` 本身已实现并端到端验证（真 raft + 真 store + 真 SQLite），但**节点今天仍不从 Arachne 服务配置** —— 只做一半会得到一个「没有人写 head、节点却自称已物化」的进程。T3.1 的「真实 3 节点」验收因此未执行；已执行的是 1 节点 + 真实 store 的端到端与纯函数层的等值/确定性/保密用例。
2. `SealedMaterial` 缺"从已存密文构造"的入口（见上），发布切换时必须先补 `db` 侧的读取函数。
3. T3.5（D-6 乙-full）未开始：`admin/tenant_config_api.rs`、`x-hydra-tenant-token`、`/api/v1/internal/tenant-config/*` 仍在。
4. `RETIRED_CLUSTER_ENV` **刻意为空**：计划要退役的 7 个变量目前仍被 Redis 路径读取，表里填名字等于宣称已退役（有用例钉住这张表为空）。
5. §9 基线同步未做：`dev-docs/cluster.md` / `ops.md` 仍描述 Redis 租约世界。

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
