# 子租户（Sub-Tenant）与 api-key 前缀路由 — 设计文档

> 状态：**设计评审通过（oracle v1 评审 + 事实核查）；实施计划已排（`dev-docs/aegis/plans/2026-09-18-sub-tenant.md`，计划阶段 oracle GATE: PASS）**
> 日期：2026-09-18
> 相关：`design.md` §7.1（路由/白名单）、§7.1b（key-prefix binding）、`design-tenant-api.md`（数据面自助 API 与 A-1 决策）、`cluster.md`（LEADER/EDGE 拓扑）

本文件是**设计文档**，不是实施计划。实施任务拆分见 `dev-docs/aegis/plans/2026-09-18-sub-tenant.md`。

---

## 1. 背景与目标

### 1.1 目标

引入 **子租户（Sub-Tenant）**：租户下的一个分组概念。

- 同一个子租户下用户的 client api-key 共享一个前缀。例如租户为子租户生成前缀 `QQCX`，则该子租户的 key 形如 `QQCX_XXXXX`。
- 租户为子租户配置“路由到模型/上游 provider”的规则，于是**可以按 api-key 前缀控制路由**。
- 该前缀级路由**优先级最低**，作为对既有 model × tenant 路由的收窄/覆盖。
- 它的原理与既有的 **key-prefix binding（§7.1b）** 相同，但改由**租户自主管理**，因此可作为后者的租户自助替代形态（二者共存，见 §4）。

### 1.2 非目标（明确不做，见 §10）

- **Hydra 不铸造、不验证 client api-key 的所有权。** 客户端 key 由租户自己的 `auth_url` 外部验证（`crates/hydra-server/src/http.rs:498,579`），Hydra 只缓存裁决。因此前缀只是**路由选择器**，不是身份边界（见 §5）。
- 子租户不是新的认证边界，不引入独立凭据。
- v1 不涉及多 provider 权重路由行、用户可配 priority、嵌套子租户、Hydra 侧 key 铸造（永久非目标）。

---

## 2. 事实基线（已对 HEAD 代码核实）

评审对以下事实逐行核对，本节是后续决策的地基。

### 2.1 数据面热路径顺序

`crates/hydra-server/src/proxy.rs::request_filter`：

1. （0）租户自助 API 保留前缀拦截（`:496-513`），在 Host→tenant **之前**。
2. （1）Host → tenant（`resolve_tenant`，`:518-532`）；未知域名 404，`localhost`/缺 Host 归 `localhost` 租户。
3. （2）租户 enabled 闸门（`:534-538`）。
4. （2.5）`GET /v1/models` 目录（认证前，可由本地快照应答）。
5. （3）client api-key 解析（`:542-563`）。
6. （4）外部认证（cache-first，`auth_url`）。
7. （5）读全量 body →（6）提取 model →（7）预限流。
8. （8）`router::resolve(cfg, breaker, &tenant, &model_key, Some(api_key))`（`:771-792`）。

**结论：路由发生时，`tenant` 与 `api_key` 两者都已在手**，且 tenant 先于 key 解析。

### 2.2 现有路由管线与优先级

`crates/hydra-core/src/router.rs`：

| 步 | 内容 | 位置 |
|---|---|---|
| (0) | `TenantModel` 白名单（default-open，租户仅可用列出的 model） | `:88-96` |
| (1)(2)(3) | model→providers ∩ 租户授权 providers | `:98-119` |
| (3.5) | **key-prefix binding 闸门**：原始 api-key 命中 `ConfigData::key_prefix_bindings`，**最长前缀**胜出，`retain` 为**单个** provider，空 ⇒ `NoAvailableProvider`（fail-closed） | `:121-132`，`match_key_binding` `:62-70` |
| (4)(5) | 过滤 dead / 无 key / weight=0，再 SWRR 排序 | `:134-152` |

`accessible_models`（`:196-272`）的文档明确要求**与 `resolve` 逐闸门镜像**——任何新路由闸门都必须同步进它与 admin catalog（`crates/hydra-server/src/admin/handlers.rs:1236-1245`），否则目录与实路由 diverge。

### 2.3 现有 key-prefix binding（最近似物）

`crates/hydra-core/src/model.rs:137-155`：`ProviderKeyBinding { key_prefix, provider_id, enabled, ... }`。

- **全局命名空间**（不分租户），`config.rs:77`。
- 键约定形如 `sk_aaa_`（含分隔符）。
- `config::validate` 对其只做 **warn 级**校验（`config.rs:300-313`）。

### 2.4 配置快照与投影

- leader 写 SQLite，构造快照：**全量行源** `FidelityRows`（`crates/hydra-server/src/cluster/content.rs:39`）→ **wire DTO** `SnapshotWire` / `FidelityWireRows`（`crates/hydra-server/src/cluster/snapshot.rs:101,122`；序列化 `SnapshotWire::build :201`、解析 `::hydrate :275`）→ edge 物化 `db/restore.rs::restore_config`（`:68`）。（原文所称 `HydraWire` **在代码中不存在**，为误写。）
- `store.rs` 从 DB 构造 `ConfigData`（`:172-189`）；edge 无本地 SQLite，走 `ConfigStore::from_snapshot`（`main.rs:308-316`），控制客户端轮询 + 版本防倒退。
- 新实体必须新增迁移（模式参照 `migrations/0006_provider_key_binding.sql`，当前下一个空号为 `0010`），并投影进**上述全部四处**：`ConfigData`（enabled-only）、`FidelityRows`、`FidelityWireRows`、`db/restore.rs`（含 `WipedTable` 删除顺序）。
- **wire 版本硬约束**：`SnapshotWire` 与 `FidelityWireRows` 均 `#[serde(deny_unknown_fields)]`，且 `ConfigData` 无 `#[serde(default)]`；因此新增字段必须同步 bump `WIRE_VERSION`（`snapshot.rs:49`），并遵循"先升级 reader/standby、再由 leader 发新格式"的升级顺序，否则会以 serde error 而非版本门禁失败。

### 2.5 集群写入与 EDGE 的约束（头号问题的地基）

- 现有 admin 写入转发：`crates/hydra-server/src/cluster/forward.rs` + `admin/mod.rs::maybe_forward_mutation`（`:472-545+`）——转发到**活跃 leader**，带 `FORWARD_ONCE_HEADER` 环守卫（`forward.rs:115`）、注册表实时解析目标（`:129-147`）、connect/total 双超时与“确定失败 vs 结果未知”分类（`:53-109`）。
- **但**：转发**原样中继调用方的 `Authorization`**（`forward.rs:236-238`），而 admin 闸门**先**验 admin token **再**转发（`admin/mod.rs:655-668`）。edge 刻意不持有 `HYDRA_ADMIN_TOKEN`（`design-tenant-api.md:711`；`main.rs:238-247` 只在 leader 约束，`:278-283` edge 只需 `control_url` + cluster token）。**所以 admin 转发不能原样复用给租户写。**
- **关键利好**：leader 的 admin 面已有 `/api/v1/internal/*` 端点族，用 **`HYDRA_CLUSTER_TOKEN`** 认证（fail-closed、常数时间比较，`admin/mod.rs:619-641`；原文引用 `:582-604` 为陈旧行号，行为不变）；而 edge **本就持有 cluster token** 并在调用 leader 的 control URL。也就是说，“edge→leader + cluster token 认证”的**传输与认证模式已存在**。
- 租户 API 目前只有 3 条路由（`crates/hydra-core/src/tenant_api.rs:172-181`），全部挂在数据面保留前缀下，且按 A-1 决策**不向 leader 转发**（`design-tenant-api.md:674-712`）。

---

## 3. 数据模型

### 3.1 实体

```
SubTenant {
  id, tenant_id, name, key_prefix, enabled, created_at, updated_at
}
UNIQUE(tenant_id, name)
UNIQUE(tenant_id, key_prefix)          -- 前缀租户内唯一（Q2 已决）

SubTenantRoute {
  id, sub_tenant_id, model_key NULLABLE, provider_id, enabled, created_at, updated_at
}
UNIQUE(sub_tenant_id, model_key)       -- NULL = 该子租户默认路由；NULL 唯一性用部分唯一索引
```

- `model_key` 非空 = 该 model 专属路由；`NULL` = 子租户默认路由。二级，无用户可配 `priority`。
- 每行**单 provider**。weight/多 provider 推迟（§10）。
- 投影进 `ConfigData` 的**两个** enabled-only 字段——`sub_tenants`（承载 `key_prefix`，前缀匹配的来源）与 `sub_tenant_routes`。二者必须同时下发 edge：只有路由、没有子租户前缀，无法完成前缀匹配。wire 投影细节见 §2.4。

### 3.2 前缀生成与匹配

- **生成**：创建时由 leader 侧生成。建议 **8 位大写字母数字**（36⁸ ≈ 2.8×10¹²），并**必须带分隔符**（`QQCX_` 而非裸 `QQCX`）——裸 `starts_with("QQCX")` 会捕获 `QQCXWEB_*`。租户示例的 4 位仅示意；4 位在单租户内约 1300 个前缀即生日碰撞，不推荐。
- **唯一性作用域**：**租户内**（Q2，已决）。匹配只发生在本租户的路由表内。
- **与 operator binding 的关系**：创建时若与**已启用**的 operator binding 前缀重叠，**写路径直接拒绝**（死配置不许入库）。运行时的结构性优先级见 §4。

### 3.3 写路径校验（error 级 fail-closed）

对照 operator binding 现状只有 warn（`config.rs:300-313`，operator 输入可人工兜底），**租户输入不可以**。写路径必须 error 级拒绝：

- provider 存在且 ∈ 该租户 `tenant_providers`；
- `model_key` 若填，∈ 该租户 `tenant_models` 且由该 provider 服务；
- 前缀非空、含分隔符、与同租户既有前缀及启用中的 operator binding 无重叠；
- 每租户子租户/路由数量上限（防经租户 API 的配置 DoS）。

快照侧 `config::validate` 追加 warn 级孤儿检查（与 binding 同款），但**不作为唯一防线**。

---

## 4. 路由集成与优先级（裁定）

### 4.1 管线增量

```
(0) TenantModel 白名单（不变）
(1)(2)(3) model ∩ tenant_providers（不变）
(3.5) operator key-prefix binding（不变）：
        命中 ⇒ retain 单 provider，空 ⇒ NoAvailableProvider
(3.6) 【新】若 (3.5) 未命中任何 binding，且原始 key 前缀命中本租户一条启用的
      SubTenantRoute（按「model 专属 > 默认」取一条）：
        candidates ∩= {route.provider_id}；空 ⇒ NoAvailableProvider（fail-closed）
      无命中 ⇒ 行为与今天完全一致（opt-in steering，不是白名单）
(4)(5) 过滤 + 确定性排序（不变）
```

### 4.2 裁定：`(a′)` fail-closed 交集、operator 胜出

“最低优先级”有三种可能的解释，本设计**裁定为 (a′)**：

- **否决 (b) “模型映射为空时由子租户路由兜底”**：模型映射为空意味着 operator 从未授权该 model，让子租户在此兜底 = 租户把流量导到 operator 从未映射的 provider，是**提权**。
- **否决 (c) 软偏好**：不能强制就不满足“替代 key binding”的目标（binding 本是 fail-closed 强制）。
- **采纳 (a′)**：operator binding 命中 ⇒ **整体跳过**子租户路由（两套独立配置求交可能互锁；operator 的显式处置应胜出）；否则把候选集 ∩ 子租户路由的 provider，空 ⇒ fail-closed；无命中 ⇒ 与今天完全一致。

### 4.3 镜像义务（P0 验收项）

前缀闸门在代码中实际出现于**三个热路径点**（不止 `resolve`）。新增 (3.6) 步后，必须同步：

- `router::resolve`（`router.rs:81`）——(3.6) 主闸门；
- `router::accessible_models`（`router.rs:196-272`）——目录与实路由逐闸门一致；
- **`proxy::passthrough_candidates`（`proxy.rs:1475`）**——模型缺失的 passthrough 路径，今日已应用 (3.5) binding 闸门（`:1481`）；**必须同样应用 (3.6)**，否则同一 key 的走向会因请求 body 是否携带 `model` 字段而分叉（可观测的不一致）；
- admin catalog（`admin/handlers.rs:1256` `tenant_model_catalog`）：该视图**无 client key**（key-based 的 (3.5)/(3.6) 结构性不触发），因此**功能上无需改动**，仅需在注释中声明此点。

**Q10（已决）**：模型缺失 passthrough 只可能命中**默认路由**（`model_key IS NULL`）——无 model 可匹配模型专属行；求交为空 ⇒ 503 fail-closed，与 binding 在 passthrough 命中死 provider 的现有行为一致。

不镜像 ⇒ 目录/路由对租户“说谎”（宣称可路由、实际 503，或同一 key 行为不一致）。

---

## 5. 前缀语义与可执行性

- 前缀匹配发生在**认证之后**（`proxy.rs:775` 传入的 key 已经过租户 `auth_url` 裁决），所以“拿别人前缀的假 key”只能被路由到**该租户本就无权**的 provider 集合之内的子集，且仍过不了 auth。
- 因此子租户不是新认证边界。**前缀不是秘密、不是身份**；任何 caller 都可以 Present 任意前缀，但路由永远受 `model ∩ tenant_providers` 约束（运行时 backstop）。即使 edge 快照陈旧（provider 已被 operator 移出），求交使其 fail-closed。
- **归因可信度 = 租户自身发 key 的纪律**，必须写入租户对接文档。
- **停用/删除 ≠ 吊销**：路由消失后 key 照常走默认管线（认证仍在 `auth_url`）。“停用一个子租户”只是停止 steering，不是停用 key——文档必须写明，否则运营者会误以为删除=吊销。吊销永远是 `auth_url` 的职责。

### 全局唯一前缀？—— 已决：不需要

曾评估“前缀全局唯一”的诉求（理由：热路径上或许只有 api-key 可用于判断）。经核实，热路径中 tenant 先于 key 解析（§2.1），且每次路由都同时持有 tenant 与 key，**租户内唯一已足够**；全局唯一只增加生成复杂度与抢占面，无功能收益。故 **Q2 = 租户内唯一**。

---

## 6. 头号问题：EDGE → leader 的租户写路径

### 6.1 问题

租户 API 在数据面（`/tenant/` 保留前缀），可能落在 EDGE。子租户 CRUD 是**写**，而 edge 无本地 SQLite、无 admin token、admin 面只余健康检查——**租户写今天在 edge 上无路可走**。

### 6.2 v1：回避（零新集群机制）

- **operator 代管的子租户 CRUD 走既有 admin 面**：`maybe_forward_mutation` 已解决“从非 leader 写”的鉴权、转发、环守卫、结果未知语义，全部现成且有测试。这是**零新集群机制**即可上线路由价值的形态。
- **租户 API 先做只读**（GET 子租户/路由）：由快照喂养，edge 今天就能服务（`whoami` 已证明该路径可行）。

### 6.3 v2：方案 A′（租户自助写）

复用既有 internal 控制面模式转发到租约持有者：

- 传输/认证：edge 数据面 handler → leader `/api/v1/internal/tenant-config/...`，**cluster token 认证节点**（`admin/mod.rs:619-641`；原文引用 `:582-604` 为陈旧行号）；请求体内携带**租户 Bearer**，leader 侧用**同一 `authenticate` 闸门重鉴权**。cluster token 只证明“这是我方节点”，不证明租户身份；重鉴权是权威快照上的二次确认，也天然防重放中的身份混淆。
- 复用 `forward.rs` 全套语义：注册表实时解析目标、`FORWARD_ONCE_HEADER` 环守卫、connect/total 双超时与“确定失败/结果未知”分类（503/504）。
- **幂等写**：PUT-by-`(tenant_id, name)` upsert + 幂等 DELETE；leader failover 在 apply 后、ack 前发生时，租户重试收敛到同一状态。
- **对账**：租户 API 的 `whoami` 已返回 `config_version`（`auth.rs:54-57`），异步/未知结果的对账不必新建机制。
- 单节点/leader 本机：复用 `maybe_forward_mutation` 结构（`leader_ready` 为 `None` 或 `is_leader()` ⇒ 本地执行，`admin/mod.rs:480-483`）。

### 6.4 必须的决策记录：A-1 修订（A-2）

A-1（`design-tenant-api.md:674-712`）否决的是**失效清除端点（E2）**向 leader 转发，其决定性理由是“扇出已由共享流完成，转发不产生额外效果”。该理由对**配置写不成立**——leader 是唯一写者，转发就是唯一正确的落点；但 A-1 的机制偏好（“数据面经共享 Redis 总线协调、不直连 peer”）需要**显式修订**，按 §6.5 的决策记录流程新增 **A-2**，写明放宽的范围与理由。**不得悄悄绕过 A-1。**

### 6.5 备选方案与裁定

| 方案 | 正确性 | 信任边界 | 运维/拓扑 | 工作量 | 裁定 |
|---|---|---|---|---|---|
| **A′ 既有 internal 面 + forward.rs 语义转发**（cluster token 认节点 + 租户 token 认身份） | 同步 CRUD；失败分类现成；幂等 upsert 兜 failover | 需修订 A-1（数据面首次直连 peer）；leader 重鉴权 | 复用 edge→leader control URL；LB 拓扑零改动 | 中 | **v2 首选** |
| **B Redis 意图流** | 最终一致；需幂等 apply + 版本守卫 + 新建回执/对账 | 完全符合 A-1 字面（总线模式） | Redis 已是集群硬依赖；无新网络路径 | 大 | 仅当 A-2 被否时启用 |
| **C LB 分流到 leader** | 正确 | 无新边界 | **破坏 any-node 自助**；样例拓扑 leader 不暴露数据面 | 小 | **否决** |
| **D 仅 admin 面（operator 代管）** | 正确且零新机制 | 无新边界 | 与一切现有配置写同构 | 最小 | **v1 采纳**（P2 的前置，非替代） |

B 的核心问题：CRUD 是同步语义，纯 fire-and-forget 会“先 202 后冲突”（失效流是单向的，没有 per-request 回执）。

---

## 7. 安全与滥用分析

1. **前缀非秘密、非身份**：归因（P3 用量/限流按子租户）必须在**路由时**从已认证请求的原始 key 派生并存独立列——**不能**事后从存储的 `client_api_key` 重建（该列是掩码的，`sink.rs:12-16,397`）。
2. **前缀重叠**：裸前缀会吞掉更长前缀。缓解：前缀必含分隔符 + 创建时同租户重叠拒绝 + 与启用中 operator binding 重叠拒绝。
3. **提权已封死但依赖两道闸**：运行时求交（router 3.6）+ 写路径校验（提交时）。运行时求交是 backstop。
4. **operator/租户前缀冲突**：结构性消解（operator 胜出、跳过子租户路由）+ 创建时拒绝，而非运行时最长前缀仲裁。
5. **快照膨胀**：无上限子租户/路由 = 配置 DoS。需每租户上限 + 写路径校验。
6. **删除/停用 ≠ 吊销**（见 §5）。
7. **edge 陈旧窗口**：路由变更经控制面轮询收敛（~1s + 版本防倒退），删除后短暂仍按旧路由——与一切配置变更同性质，写入文档。
8. **转发信任（v2）**：重放靠 FORWARD_ONCE + 幂等 upsert + leader 重鉴权；分区时写 fail-closed（503/504）、读继续用旧快照。
9. **目录说谎风险**：catalog/`accessible_models` 不镜像 = 可被租户观测到的谎言（§4.3）。

---

## 8. 分期

### v1（零新集群机制，纯增量，可独立上线）
1. 迁移（两表 + 约束，含 NULL 部分唯一索引）+ `ConfigData`/`store.rs`/`FidelityRows`/`FidelityWireRows`/`restore.rs` 四处投影（`WIRE_VERSION` bump）+ `config::validate` warn 检查。
2. operator admin CRUD（既有 admin 面模式，自动获得 leader 转发）。
3. `router.rs` (3.6) 步 + `accessible_models` 镜像 + `proxy::passthrough_candidates` 默认路由收窄（Q10）+ `hydra-core` 纯函数单测（路由逻辑必须住 core，受依赖防火墙约束）。
4. 租户 API **只读**端点（GET 子租户/路由，快照喂养）。

### v2（租户自助写）
A′ + A-2 决策记录 + 幂等 upsert + leader 重鉴权 + `config_version` 对账 + 每租户配额 + 独立限流维度（防配置写扇出放大，参照 §5.1 对 invalidate 的论证）。

### v3（用量归因）
`usage_record` 新列（路由时派生），ClickHouse/SQLite 双 sink 同步改。

---

## 9. 开放问题与已决裁定

| # | 问题 | 结论 |
|---|---|---|
| Q1 | “最低优先级”精确语义 | **(a′)**：operator binding 命中即整体胜出；否则 ∩ 子租户路由 provider 集；空 ⇒ fail-closed |
| Q2 | 前缀唯一性作用域 | **租户内唯一**（`UNIQUE(tenant_id, key_prefix)`）；创建时额外拒绝与启用中 operator binding 重叠。全局唯一**不需要**（§5） |
| Q3 | 替代还是共存 operator key binding | **共存**，operator 胜出；v1 不做迁移/弃用 |
| Q4 | 路由行粒度 | 每行单 provider；`(model_key, NULL=默认)` 两级；weight 推迟 |
| Q5 | edge→leader 写路径 | v1 回避；v2 采 A′，且**必须先落 A-2** 决策记录 |
| Q6 | failover/重放/分区语义 | PUT-upsert 幂等 + FORWARD_ONCE + leader 重鉴权；“结果未知”（504）→ 以 `whoami.config_version` 对账；分区写 fail-closed、读用旧快照 |
| Q7 | 停用/删除语义 | fall-through 到正常路由（opt-in steering），**不是**吊销 |
| Q8 | Hydra 侧验证 key 前缀归属 | 不需要也不可行；前缀仅路由选择器 |
| Q9 | 前缀长度与字符集 | **8 位大写字母数字 + 分隔符**（如 `QQCX_`）；自动生成与提交校验都必须强制含分隔符 |
| Q10 | (3.6) 在模型缺失 passthrough 的作用域 | **仅默认路由（`model_key IS NULL`）参与收窄**；必改 `proxy::passthrough_candidates`，否则同一 key 因 body 是否带 `model` 而分叉（见 §4.3） |

> **Q11（Q1 的严格解读，已决）**：子租户**默认路由**把该子租户流量钉在单一 provider；该 provider 不服务的 model 对该前缀 **fail-closed 不可路由**。这正是 (a′)。将其放宽为"落回正常路由"即 §4.2 已明确否决的软偏好 (c)，且会让默认路由比同形状的 operator binding 更弱。

---

## 10. 明确不做

1. 不做“(b) 模型映射为空时子租户路由兜底”——提权通道。
2. 不做跨租户/全局前缀匹配。
3. 不在 v1 引入用户可配 `priority` 或多 provider 权重路由行。
4. 不复用 `forward_mutation` 的 admin-token 中继做租户写（edge 无 admin token，闸门顺序根本不同）。
5. 不做纯 fire-and-forget 的 Redis 意图流（无 per-request 回执的 CRUD 不可用）。
6. 不跳过 catalog/`accessible_models` 镜像，也不跳过模型缺失 passthrough（`passthrough_candidates`）的 (3.6) 收窄。
7. 不在认证前用前缀做任何归因/限流。
8. 不删除 `ProviderKeyBinding`（全局 operator 语义，与租户自服务正交）。
9. 不让 `config::validate` 的 warn 级校验成为唯一防线——租户写路径必须 error 级 fail-closed。
10. 不悄悄放宽 A-1——数据面直连 peer 必须伴随 A-2 决策记录。

---

## 附：对既有文档的影响

- 本设计落地后需同步：`design.md` §7.1b（新增租户前缀路由层级）、`design-tenant-api.md`（新增决策记录 A-2）、租户对接文档（前缀非身份/非吊销、归因可信度边界）、`ops.md`（子租户配额/可观测）。

---

## 附：勘误与修订记录

### 2026-09-18 计划阶段对账（oracle 事实核查 + 计划门禁 GATE: PASS）

排期时对 HEAD 逐行核验，修正以下事实性偏差。前四类语义不变（Implementation Drift，文档面），E-4 为规范不完整（Design Defect），已在本文件就地修订：

- **E-1（已修订 §2.4）**：原文所称 `HydraWire` 类型在代码中**不存在**。实际投影链为 `FidelityRows`（`cluster/content.rs:39`）→ `SnapshotWire`/`FidelityWireRows`（`cluster/snapshot.rs:101,122`，`build :201` / `hydrate :275`）→ `db/restore.rs::restore_config`（`:68`）。并补记 `WIRE_VERSION` 硬约束。
- **E-2（已修订 §2.5、§6.3）**：internal 控制面闸门实际位于 `admin/mod.rs:619-641`，原文 `:582-604` 为陈旧行号；行为（`HYDRA_CLUSTER_TOKEN`、常数时间比较、fail-closed）不变。
- **E-3（已修订 §4.3）**：admin catalog（`handlers.rs:1256`）是 keyless 视图，(3.5)/(3.6) 结构性不触发；原文将其列为"必需镜像点"在功能上是 no-op。
- **E-4（Design Defect，已修订 §4.3）**：原文 §4.3 镜像义务清单遗漏热路径第四个闸门点 `proxy::passthrough_candidates`（`proxy.rs:1475`，今日已应用 (3.5)）。已补入，并新增 **Q10** 裁定。
- **E-5（已修订 §3.1）**：原文只列 `sub_tenant_routes`；实际必须**同时**投影 `sub_tenants`（`key_prefix` 所在），否则无法做前缀匹配。
- **E-6（记录）**：`.sqlx/` 离线缓存实为 44 个 query 文件（旧 `HANDOFF.md` 记 33）；与设计逻辑无关，供实施期对齐。
- **Q11 回填**：§9 补记 (a′) 的严格默认路由语义。

实施任务拆分与验收矩阵见 `dev-docs/aegis/plans/2026-09-18-sub-tenant.md`。
