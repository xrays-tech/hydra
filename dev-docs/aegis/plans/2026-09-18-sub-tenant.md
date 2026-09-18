# 实施计划：子租户（Sub-Tenant）与 api-key 前缀路由

> 状态：**计划阶段门禁通过（oracle GATE: PASS，2026-09-18）——F1 强制修订已落，无需复审；F2–F7 已并入。实现阶段待启动。**
> 目标设计：`dev-docs/design-sub-tenant.md`（oracle 事实核查 + 评审通过）
> 依赖设计：`design.md` §7.1 / §7.1b、`design-tenant-api.md`（A-1 决策）、`cluster.md`
> 日期：2026-09-18
> 范围：**v1（零新集群机制，可独立上线）详细拆分；v2/v3 路线图**。v1 不含租户自助写。

---

## Goal

在租户之下引入**子租户**分组：同一子租户的 client api-key 共享一个前缀（如 `QQCX_`），租户为该子租户配置 `model → provider` 路由，从而**按 api-key 前缀做最低优先级路由**。

成功定义（v1）：

1. operator 可通过既有 admin 面 CRUD 子租户及其路由，写路径 **error 级 fail-closed** 校验，坏配置不允许入库。
2. 前缀路由在数据面热路径生效：位于既有 operator key-prefix binding **之后**、作为其**收窄**，无命中时行为与今天完全一致。
3. `accessible_models` 与 admin 目录与实际路由**逐闸门一致**（不"说谎"）。
4. 租户 API 提供**只读**的子租户/路由视图，由快照喂养，edge 可服务。
5. hydra-core 纯逻辑受依赖防火墙约束；路由逻辑全部住 core 并以纯函数穷尽单测。

---

## 架构 / Architecture

### 路由管线增量（唯一行为改动点）

```
(0) TenantModel 白名单                         不变
(1)(2)(3) model ∩ tenant_providers             不变
(3.5) operator key-prefix binding              不变：命中 ⇒ retain 单 provider，空 ⇒ NoAvailableProvider
(3.6)【新】仅当 (3.5) 未命中：
      若原始 key 命中本租户一条启用的 SubTenantRoute（model 专属 > 默认）：
        candidates ∩= {route.provider_id}；空 ⇒ NoAvailableProvider（fail-closed）
      无命中 ⇒ 与今天完全一致（opt-in steering，不是白名单）
(4)(5) 过滤 + 确定性排序                        不变
```

**裁定 (a′)**（设计 §4.2）：operator binding 命中 ⇒ 整体跳过早租户路由（operator 显式处置胜出）；否则求交；无命中不改变行为。**否决**模型映射为空时兜底（提权）与软偏好（无法替代 binding 的强制语义）。

### 数据流

- leader 写 SQLite（迁移 `0010`），`build_config` 构造 `ConfigData`（新增 `sub_tenants` + `sub_tenant_routes`，**enabled-only**）。
- leader 构造快照：`FidelityRows`（全量行，`cluster/content.rs`）→ `SnapshotWire`/`FidelityWireRows`（`cluster/snapshot.rs`）→ edge `hydrate` → `restore_config`（`db/restore.rs`）。
- edge 无 SQLite：`ConfigStore::from_snapshot` 喂数据面；租户只读端点直接读快照。

---

## 技术栈 / Tech Stack

- Rust workspace；`hydra-core`（纯领域，零 I/O）+ `hydra-server`（Pingora/sqlx/reqwest 外壳）。
- SQLite + sqlx（离线缓存 `.sqlx/`，CI `SQLX_OFFLINE=true`）。
- 测试：hydra-core 纯单测；hydra-server 集成测试（真实 SQLite `:memory:`、真实 Redis、wiremock 外部边界）。

---

## Baseline / Authority Refs

| 权威 | 路径 | 用途 |
|---|---|---|
| 设计文档（本计划依据） | `dev-docs/design-sub-tenant.md` | 全部决策 Q1–Q9、§4.1 管线、§6 分期 |
| 主设计 | `dev-docs/design.md` §7.1 / §7.1b | 路由与 key-prefix binding 现状 |
| 租户 API 设计 | `dev-docs/design-tenant-api.md` §A-1 | 数据面不转发；v2 需 A-2 修订 |
| 集群拓扑 | `dev-docs/cluster.md` | LEADER/EDGE、快照控制面 |
| 开发纪律 | `dev-docs/dev-plan.md` §1 铁律 1/2/3 | TDD、零 mock、terminate-mode |
| 基线治理 | `dev-docs/aegis/BASELINE-GOVERNANCE.md` | Design Defect vs Implementation Drift |
| 既有近似计划 | `dev-docs/aegis/plans/2026-08-21-key-prefix-binding.md` | 迁移/CRUD/闸门/目录镜像的落地样式 |

### 关键既有事实（已对 HEAD 逐行核实，2026-09-18）

- 路由：`crates/hydra-core/src/router.rs::resolve(cfg, breaker, tenant, model_key, client_api_key) -> Result<Vec<Candidate>, RouteError>`（`:81`）；(3.5) 在 `:121-132`；`match_key_binding` 在 `:62-70`（最长前缀）。
- 目录：`accessible_models(cfg, breaker, tenant_id, client_api_key) -> Vec<CatalogEntry>`（`:196`），**已持有 key**，镜像点在 (3.5) 之后 `:233-235`。
- 热路径：`proxy.rs::request_filter`（`:474`）；3.5 的**第四个调用点** `passthrough_candidates`（`:1475`，模型缺失 passthrough）；`resolve` 调用点 `:775`；`accessible_models` 调用点 `:603`。
- 写路径：admin 面在 `admin/mod.rs:653-662` 验 admin token → `:664-670` `maybe_forward_mutation` → `:672` `route()`；`forward.rs` 中继调用方 `Authorization`（`:236-238`）。
- internal 控制面：`/api/v1/internal/*` 由 `HYDRA_CLUSTER_TOKEN` 认证，实际在 `admin/mod.rs:619-641`；当前仅 `/internal/control`。
- 租户面：`hydra-core/src/tenant_api.rs::parse_route`（`:172-188`，3 端点）；server 侧保留前缀 `/tenant/` 拦截、快照令牌闸门、`whoami` 返回 `config_version`（`tenant_api/auth.rs:54-57`）。
- 数据面持有**原始** api-key 用于路由（`proxy.rs:617`），用量 sink 只存**掩码**（`sink.rs:397`）⇒ 事后无法据此归因（设计 §7.1）。

---

## ADR / 决策信号携带（完成期需交付给 ADR Backfill）

1. **A-2（v2 前置）**：显式修订 `design-tenant-api.md` 的 A-1，放宽"数据面不直连 peer"用于**租户配置写**（理由：leader 是唯一写者，转发是唯一正确落点；A-1 的决定性理由"扇出已由共享流完成"对配置写不成立）。**v1 不触发 A-2，也不得悄悄放宽 A-1。**
2. **(a′) 路由优先级裁定**（设计 §4.2）——本计划以 T4 测试固化。
3. **Q10 前缀路由在模型缺失 passthrough 上的作用域**（本计划新增）——oracle 已裁定=是（仅默认路由参与收窄，见 T4.5 / 待决与裁定表）。

---

## 需求就绪检查 / Requirement Ready Check

- [x] 目标与非目标明确（设计 §1、§10）。
- [x] 数据模型与约束明确（设计 §3.1，Q2 租户内唯一，NULL 用部分唯一索引）。
- [x] 路由语义明确（Q1=(a′)，Q7=fall-through，非吊销）。
- [x] v1/v2/v3 分期明确（设计 §8），v1 零新集群机制。
- [x] 写路径校验清单明确（设计 §3.3）。
- [x] 安全边界明确（前缀非身份/非秘密，设计 §5）。
- [ ] **待决**：Q10（passthrough 3.6 作用域）、Q11（默认路由对"默认 provider 不服务的 model"是否严格 fail-closed）、Q12（admin 路由形状）、Q13（前缀生成细节确认）。
- [ ] **设计文档 errata**（下表 E-1..E-6）需随本计划一并归档。

---

## 变更必要性 / Change Necessity

- 既有 `ProviderKeyBinding` 是**全局 operator** 命名空间，租户不可自助，无法表达"同租户下按前缀分组"；设计目标正是其**租户自助替代形态**，故必须新增实体。
- 不新增则无法按前缀收窄路由；不镜像目录则租户可见目录与实际 503 不一致（设计 §4.3，可观测的谎言）。
- **未使用既有替代**：A-1 已否决数据面转发失效端点；本设计 v1 复用 admin 面写路径，**不新建集群机制**（D 方案），因此变更面最小。

---

## 既有面复用检查 / Existence Check

| 需求 | 既有可复用 | 结论 |
|---|---|---|
| operator CRUD + leader 转发 | `maybe_forward_mutation` + `provider_key_binding_*` handler 样式 | **直接复用**（v1 写路径零新机制） |
| 写路径校验 | `config::validate`（仅 warn） | **不可直接复用**，租户/实体写必须 error 级，需新校验纯函数 |
| 路由闸门 | `match_key_binding` 样式 + `resolve` (3.5) | 新增 `match_sub_tenant_route` 纯函数，结构镜像 |
| 目录镜像 | `accessible_models` (3.5) 分支 | 新增 (3.6) 分支，**已在函数的 key 上下文中** |
| 快照投影 | `FidelityRows`/`SnapshotWire`/`restore_config` | 按既有实体逐处新增字段 |
| 租户只读 | `whoami` 快照只读路径 + 令牌闸门 | 新增只读端点，复用同一 `authenticate` |
| v2 节点间传输 | `/api/v1/internal/*` cluster-token 闸门 + `forward.rs` 语义 | v2 复用，v1 不做 |

**不存在项**（grep 全仓零命中的纯新增）：`SubTenant`、`SubTenantRoute`、`/internal/tenant-config`。

---

## 架构完整性视角 / Architecture Integrity Lens

1. **Ownership**：`SubTenant`/`SubTenantRoute` 领域类型归 `hydra-core`；SQLite/快照/HTTP 归 `hydra-server`。唯一 owner，无重复。
2. **Module boundaries**：路由判定只住 `router.rs` 纯函数；handler 只做校验编排 + DB/快照搬运，不内联路由算法。
3. **Contract changes**：`ConfigData` 增字段（快照 wire 契约，需 `WIRE_VERSION` 评估）；`parse_route` 增 `Endpoint` 变体；admin 路由表增条目；tenant API 增只读端点。逐项在 T3/T6/T7 文档化。
4. **Cascade**：新增字段会级联到所有 `ConfigData`/`FidelityRows`/`FidelityWireRows` 字面量构造点（见文件清单），属**机械**级联，不引入新依赖链。
5. **Dependency direction**：方向保持 `hydra-server → hydra-core`；前缀匹配纯函数不下沉 I/O。CI `cargo tree` 防火墙校验不得被破坏。
6. **Retirement**：v1 **不删除** `ProviderKeyBinding`（Q3 共存，operator 胜出）；无遗留路径。
7. **Entropy**：净增 2 实体 + 1 闸门步 + 1 镜像分支 + 4 投影点，均为设计必需，无未论证新实体。

---

## 兼容性边界 / Compatibility Boundary

- **无命中即零行为变化**：未命中子租户前缀的请求，路由结果与今天逐字节一致（T4 回归测试锁定）。
- **operator 优先**：已存在启用 operator binding 的 key，行为完全不变（3.6 被跳过）。
- **快照 wire**：新增字段随 `SnapshotWire` 下发。旧 edge 遇未知字段？`deny_unknown_fields` 已存在——**必须评估 `WIRE_VERSION` 提升**，见 T3 步骤。
- **租户可见性**：`accessible_models` 在 key 命中子租户路由时**收窄**目录，属有意行为（目录与实路由一致），非兼容性破坏。
- **停用/删除 ≠ 吊销**：删除子租户后 key 回到默认管线（认证仍在 `auth_url`）。文档必须显式写明。

---

## TDD Route

**Route: strict.** 每个任务先写失败测试，再最小实现。红→绿→重构；提交信息保留 `test:` → `feat:` 节奏。

- `hydra-core` 纯函数（T1、T4）必须先有单测；覆盖率门槛沿用 dev-plan §1 铁律 1。
- `hydra-server`（T2、T3、T5、T6、T7）以真实 SQLite/真实快照/真实 HTTP 集成测试为主，**零进程内 mock 内部逻辑**。
- 外部边界（`auth_url`、provider upstream）仅在必要时用 wiremock。
- 路由算法不得下沉到 server 侧做"集成测"绕过 core 单测。

---

## 计划压力测试 / Plan Pressure Test

| 拷问 | 回答 |
|---|---|
| 是否只是新增实体换皮？ | 否。核心增量是 (3.6) 闸门 + 目录镜像，有真实行为与 fail-closed 语义。 |
| 能否更小？ | v1 已是最小可上线形态（operator 代管写 + 只读租户面）；路由门可独立交付。 |
| 是否依赖 v2 才能验证？ | 否。v1 全部可在单节点/`--features server` 下验证；v2 才需 `cluster-redis`。 |
| 最大风险 | 设计文档类型名/行号偏差 + 未记录的 passthrough 镜像点（Q10）；已在 errata 中显式暴露并由 Q10 裁定消解。 |
| 验证是否可重跑？ | 是。门禁命令、矩阵脚本、测试名均在本文可复算。 |

---

## 计划期复杂度检查 / Plan-Time Complexity Check

- 新增实体 2；新增 `ConfigData` 字段 2；新增路由闸门 1；新增目录镜像分支 1；新增 admin 资源 2；新增租户只读端点 1。
- 无新增 crate、无新增外部依赖、无新增网络路径（v1）。
- 复杂度增量与设计目标线性对应；**v2 的复杂度（转发/幂等/对账）已隔离在 v2 路线图**，不污染 v1。

---

## 执行就绪视图 / Execution Readiness View

- 全部 v1 任务的输入（设计决策、既有代码点、测试样式）已具。**Q10/Q11 已由 oracle 裁定（均=是），无剩余语义阻塞**；计划阶段门禁已 PASS，实现阶段可启动。
- v1 不依赖 Redis/集群；单机 `--features server` 即可端到端验证。
- v2 依赖 A-2 决策记录先行，已列为 v2 第 0 步。

---

## 设计与事实偏差清单（errata，oracle 需核验）

| # | 设计文档陈述 | HEAD 事实 | 处置 |
|---|---|---|---|
| E-1 | §2.4 "leader 构造 `HydraWire`（`content.rs:43,150,163`）" | **无 `HydraWire`**。wire 类型是 `SnapshotWire`（`cluster/snapshot.rs:122`）；`content.rs` 持有 `FidelityRows`（`:39`）与 `ReplicationContent`（`:67`）；实际序列化/解析在 `snapshot.rs::build`（`:201`）/`hydrate`（`:275`），DB 物化在 `db/restore.rs` | 计划按真实类型/位置落地（T3），并把 errata 归档 |
| E-2 | §2.5 internal 闸门在 `admin/mod.rs:582-604` | 实际 `:619-641`（代码增长） | 仅行号更正；行为一致 |
| E-3 | §4.3 admin catalog（`handlers.rs:1236-1245`）为必需镜像点 | `tenant_model_catalog` **keyless**，3.5/3.6 均不触发；功能上无需改动 | 仅更新其文档注释，声明 3.6 亦 key-scoped；不虚报工作量 |
| E-4 | §4.3 镜像点仅列 `accessible_models` + admin catalog | 存在**未记录的第四调用点** `proxy.rs::passthrough_candidates`（`:1475`）应用 3.5 | **Design Defect（scope: requirements）**——§4.3 镜像义务清单在规范上不完整。oracle 裁定 Q10=是；设计文档 §4.3 修订**与 T4/T5 同一次交付**落地，不拖到 T8 |
| E-5 | §3.1 投影进 `ConfigData`"新字段，如 `sub_tenant_routes`" | 路由需 `key_prefix`，该字段在 `SubTenant` 上；必须**同时**投影 `sub_tenants` | 计划增 `ConfigData.sub_tenants` + `sub_tenant_routes` 两字段 |
| E-6 | `.sqlx` 离线缓存计数（HANDOFF 记 33） | 实为 **44** 个 query 文件 | 文档陈旧，T2/T3 刷新后复核 |

---

## 文件改动清单

### 新增
- `crates/hydra-server/migrations/0010_sub_tenant.sql`
- `crates/hydra-core/tests/sub_tenant.rs`（纯函数：validate + gate；或并入 `tests/router.rs`/`tests/validate.rs`，见任务）
- `crates/hydra-server/tests/sub_tenant_admin.rs`（admin HTTP CRUD + 热加载，`--features server`）
- v2 决策记录 **A-2**：写在 `dev-docs/design-tenant-api.md` §6.4b（**不新建 ADR 文件**——本项目无 ADR 体系，见其 §6.5）

### 修改（核心）
- `crates/hydra-core/src/model.rs` — 新增 `SubTenant`、`SubTenantRoute`
- `crates/hydra-core/src/config.rs` — `ConfigData` 两字段 + `validate` warn 孤儿检查
- `crates/hydra-core/src/router.rs` — `match_sub_tenant_route` + `resolve` (3.6) + `accessible_models` 镜像
- `crates/hydra-core/src/tenant_api.rs` — `parse_route` 新增只读端点变体
- `crates/hydra-server/src/db.rs` — `SubTenantRow`/`SubTenantRouteRow` + `From` + 8 个 CRUD/on-fn
- `crates/hydra-server/src/store.rs` — `build_config` 加载 enabled-only
- `crates/hydra-server/src/cluster/content.rs` — `FidelityRows` 两字段 + `load()` 两 `list_*_on`
- `crates/hydra-server/src/cluster/snapshot.rs` — `FidelityWireRows` + `build`/`hydrate`
- `crates/hydra-server/src/db/restore.rs` — `WipedTable` 变体 + wipe 顺序 + INSERT
- `crates/hydra-server/src/admin/handlers.rs` — CRUD + 写校验 + catalog 注释
- `crates/hydra-server/src/admin/mod.rs` — 4 条路由注册
- `crates/hydra-server/src/proxy.rs` — passthrough 默认路由收窄（Q10 已裁定=是）
- `crates/hydra-server/src/tenant_api/{handlers.rs,mod.rs}` — 只读端点
- `.sqlx/` — 若用编译期宏则刷新
- 文档：`dev-docs/design.md` §7.1b、`dev-docs/design-sub-tenant.md`（errata/决策注记）、`dev-docs/tenant-api-integration.md`、`dev-docs/ops.md`、`dev-docs/HANDOFF.md`、`dev-docs/aegis/INDEX.md`

### 机械级联点（oracle F2 更正；新增字段后 `cargo build` 会报，逐个补 `..Default::default()` 或显式字段）
- **Production 显式字面量**：`store.rs:178-191`（`ConfigData`）、`content.rs:161-169`（`FidelityRows`）、`snapshot.rs::build`/`hydrate` 内构造点。
- **Test fixture 显式字面量（会 break，必须补）**：`crates/hydra-server/tests/common/mod.rs:140`、`crates/hydra-server/tests/tenant_api.rs:735`、`crates/hydra-core/tests/validate.rs:275`；以及 `store.rs:515`、`replica.rs:375/459`、`snapshot.rs:436`、`tenant_api/auth.rs:198` 处的 `FidelityRows`/`FidelityWireRows` fixture。
- **无需改动**：多数 `ConfigData` 站点用 `ConfigData::default()` + 字段赋值（如 `replica.rs:372/437`、`snapshot.rs:363`、`auth.rs:180`、`router.rs:52`），不要为它们手工加字段。
- **总原则**：以上清单仅为提前预警，**最终以编译器报错为准**逐个收敛。

---

## 分步任务（bite-sized）

### T1 — 迁移 0010 + `hydra-core` 实体 + `ConfigData` + `validate`（纯核心）

**T1.1 迁移** `crates/hydra-server/migrations/0010_sub_tenant.sql`

```sql
CREATE TABLE sub_tenant (
  id          TEXT PRIMARY KEY,
  tenant_id   TEXT NOT NULL REFERENCES tenant(id) ON DELETE CASCADE,
  name        TEXT NOT NULL,
  key_prefix  TEXT NOT NULL,
  enabled     INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  created_at  TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE(tenant_id, name),
  UNIQUE(tenant_id, key_prefix)
);
CREATE INDEX idx_sub_tenant_tenant ON sub_tenant(tenant_id);

CREATE TABLE sub_tenant_route (
  id            TEXT PRIMARY KEY,
  sub_tenant_id TEXT NOT NULL REFERENCES sub_tenant(id) ON DELETE CASCADE,
  model_key     TEXT,                                   -- NULL = 子租户默认路由
  provider_id   TEXT NOT NULL REFERENCES provider(id) ON DELETE CASCADE,
  enabled       INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  created_at    TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at    TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_sub_tenant_route_st ON sub_tenant_route(sub_tenant_id);
CREATE UNIQUE INDEX uq_sub_tenant_route_model
  ON sub_tenant_route(sub_tenant_id, model_key) WHERE model_key IS NOT NULL;
CREATE UNIQUE INDEX uq_sub_tenant_route_default
  ON sub_tenant_route(sub_tenant_id) WHERE model_key IS NULL;   -- NULL 部分唯一
```

> **F5**：**故意不**再声明表级 `UNIQUE(sub_tenant_id, model_key)`。SQLite 把 NULL 视为彼此不同，表级约束会放过多条默认行；上面两个部分索引已分别覆盖"非 NULL 唯一"与"默认唯一"，是**必要且充分**的最小集合，不要再叠加冗余约束。

**T1.2 `model.rs`** 新增两个 banner section（Tenant family 之后、Key-prefix binding 之前）：

```rust
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubTenant {
    pub id: String,
    pub tenant_id: String,
    pub name: String,
    pub key_prefix: String,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubTenantRoute {
    pub id: String,
    pub sub_tenant_id: String,
    pub model_key: Option<String>,   // None = 默认路由
    pub provider_id: String,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}
```

**T1.3 `config.rs`** `ConfigData`（`:37-83`）新增 enabled-only 字段（镜像 `key_prefix_bindings :77`）：

```rust
/// 启用的子租户（`key_prefix` 的来源）。
pub sub_tenants: Vec<SubTenant>,
/// 启用的子租户路由。
pub sub_tenant_routes: Vec<SubTenantRoute>,
```

`validate`（`:241`，纯、warn-only）在 binding 检查块（`:300-314`）之后追加 warn 级孤儿检查：
- route 的 `sub_tenant_id` 不在 `sub_tenants` 中；
- route 的 `provider_id` 不在 `providers` 中；
- `sub_tenant.key_prefix` 为空或不含分隔符；
- 同租户前缀重叠（运行时兜底，因为 store 只加载启用行）。

**T1.4 测试** `crates/hydra-core/tests/validate.rs` 追加：
- `validate_sub_tenant_route_orphan_sub_tenant_warns`
- `validate_sub_tenant_route_unknown_provider_warns`
- `validate_sub_tenant_prefix_without_separator_warns`
- `validate_sub_tenant_prefix_overlap_within_tenant_warns`

**验证**：`cargo test -p hydra-core` 全绿；`cargo build -p hydra-core`；变更后 `ConfigData` 字面量级联点先补 `..Default::default()` 或显式字段。

---

### T2 — `db.rs` CRUD（仓储层）

镜像 `provider_key_binding` 函数族（`db.rs:1302-1378`）：
- `SubTenantRow` / `SubTenantRouteRow` + `From<Row>`。
- `insert_sub_tenant` / `get_sub_tenant` / `list_sub_tenants` / `list_sub_tenants_on` / `update_sub_tenant` / `delete_sub_tenant`。
- `insert_sub_tenant_route` / `get_sub_tenant_route` / `list_sub_tenant_routes` / `list_sub_tenant_routes_on` / `update_sub_tenant_route` / `delete_sub_tenant_route`。

约定：
- 优先用**运行时** `sqlx::query`/`query_as`（`restore.rs` 风格）以避免 `.sqlx` 缓存膨胀；若必须用编译期宏，则走刷新流程（T2 尾部）。
- `enabled` 在 DB 边界映射 `i64`↔`bool`。
- `updated_at` 更新走 SQL `datetime('now')`。

**测试**：`crates/hydra-server/tests/repo.rs` 追加 `sub_tenant_crud`（round-trip + `UNIQUE(tenant_id,name)`/`UNIQUE(tenant_id,key_prefix)` 冲突 + route 的 NULL 部分唯一冲突 + `ON DELETE CASCADE`）。

**验证**：`cargo test -p hydra-server --features server --test repo`。
> **执行期修正（T2 实测）**：`--features db` **单独不能编译**本 crate——`store.rs` 在 `db` 下 `use crate::cluster::*`，而 `cluster` 门控在 `proxy`（`db/restore.rs` 亦然）。这是本仓既有特性门控事实（`changelog-gap-remediation` 计划亦记录过），CI 用 `--features server`。原文与 T9 配方里的 `--features db` 已一并更正。

---

### T3 — 快照投影：store / content / snapshot / restore

这是 E-1 的落地，**必须四处都改**，只改一处会让 edge 拿到空路由或丢全量行。

- **T3.1 `store.rs::build_config`（`:64-199`）**：加载 enabled-only `list_sub_tenants(pool)` / `list_sub_tenant_routes(pool)`，填入 `ConfigData` 字面量（`:178-191`）。
- **T3.2 `cluster/content.rs`**：`FidelityRows`（`:39-54`）新增 `sub_tenants` / `sub_tenant_routes` 全量字段；`ReplicationContent::load`（`:137-154`）新增两条 `list_*_on(&mut *tx)`。
- **T3.3 `cluster/snapshot.rs`**：`FidelityWireRows`（`:101-115`）新增字段；`build`（`:259-266`）与 `hydrate`（`:340-348`）成对投影/回填。新增字段非密钥，无需 sealing。
  - **`WIRE_VERSION` 2→3 bump 是强制项（oracle F1）**：`ConfigData` 无 `#[serde(default)]`（`config.rs:36-83`），且 `FidelityWireRows`/`SnapshotWire` 均 `#[serde(deny_unknown_fields)]`（`snapshot.rs:99-100,120`）。旧读←新写会因未知字段失败、新读←旧写会因缺字段失败，**两个方向在同版本号下都会以 serde error 暴露，而不是预期的 `WireVersion` 门禁**（其文档 `snapshot.rs:157-161` 明确要求"先升级所有节点再由 leader 发新格式"）。**不存在"不 bump 的合法分支"**；同版本 round-trip 测试也抓不到该问题。落地：bump `WIRE_VERSION` 至 3，并在升级/文档中写明**先升级 reader/standby、再让 leader 发新格式**的顺序。
- **T3.4 `db/restore.rs`**：`WipedTable`（`:20-53`）加两项、`wipe` 顺序（`:78-89`）把 `sub_tenant_route` 排在**最前**——它同时引用 `sub_tenant` 与 `provider`，必须早于二者删除（FK 已启用，`db.rs:40` `foreign_keys=ON`）；per-table INSERT 循环（镜像 `:254-267`）。
- **T3.5** 以编译器驱动补全所有 `ConfigData`/`FidelityRows`/`FidelityWireRows` 字面量级联点（见"机械级联点"更正清单）。

**测试**：
- `tests/loader.rs`：`load_sub_tenants_enabled_only`、`load_sub_tenant_routes_enabled_only`。
- `tests/fidelity_disabled_rows.rs`：停用行仍随全量保真物化。
- `tests/cluster.rs`：快照 round-trip（build→hydrate）含新字段；`--features server,cluster-redis`。

**验证**：`cargo test -p hydra-server --features server`；`--features server,cluster-redis`（集群路）。

---

### T4 — 路由闸门 (3.6) + 目录镜像 + 纯单测（**P0 核心**）

**T4.1 `router.rs` 纯函数**：

```rust
/// 租户内按原始 api-key 前缀匹配一条启用的子租户路由。
/// 前缀租户内唯一且不重叠，故至多命中一个子租户；同子租户内 `model_key` 精确 > 默认(`None`)。
/// `model_key = None` 表示"仅默认路由"（模型缺失的 passthrough 路径，oracle F4：用 Option 而非 `""` 哨兵）。
pub fn match_sub_tenant_route<'a>(
    cfg: &'a ConfigData,
    tenant_id: &str,
    model_key: Option<&str>,
    api_key: &str,
) -> Option<&'a SubTenantRoute>;
```

实现：先在 `cfg.sub_tenants` 中按 `tenant_id` + `key_prefix` 前缀命中（`max_by_key(prefix.len())` 防御性最长匹配）；再在该 `sub_tenant_id` 的行中取 `model_key == model_key`（当 `Some`），否则 `route.model_key.is_none()`（默认路由）。`model_key=None` 时只匹配 `route.model_key.is_none()` 的默认行。

**T4.2 `resolve` (3.6)**：把现 (3.5) 改为记录 `binding_matched: bool`；仅当未命中时：

```rust
if !binding_matched {
    if let Some(route) = match_sub_tenant_route(cfg, &tenant.id, Some(model_key), api_key) {
        candidates.retain(|p| p == &route.provider_id);
        if candidates.is_empty() {
            return Err(RouteError::NoAvailableProvider); // fail-closed
        }
    }
}
```

**T4.3 `accessible_models` (3.6) 镜像**（`:233-235` 之间，per-model 循环内）：同样在 (3.5) 未命中时求交；若交集为空则**该 model 从目录剔除**（与 resolve 的 fail-closed 一致）。注意先计算 key 是否命中 operator binding（per-key，循环外一次），避免逐 model 重算。

**T4.4 单测** `crates/hydra-core/tests/router.rs`（沿用 `base_cfg`/`binding` 等 fixture，命名照既有 T2.x/C4x）：
- `resolve_sub_tenant_model_route_restricts`
- `resolve_sub_tenant_default_route_restricts`
- `resolve_sub_tenant_model_overrides_default`
- `resolve_sub_tenant_operator_binding_wins`
- `resolve_sub_tenant_prefix_mismatch_unchanged`
- `resolve_sub_tenant_route_provider_not_serving_fails_closed`
- `resolve_sub_tenant_disabled_ignored`
- `catalog_sub_tenant_restricts`
- `catalog_sub_tenant_no_match_unchanged`
- `catalog_sub_tenant_fail_closed_drops_model`

**T4.5 Q10/Q11 裁定（oracle 已决）**：
- **Q10 = 是（ruled）**：默认路由也作用于模型缺失 passthrough（模型专属行不适用）。`proxy.rs::passthrough_candidates` 在 (3.5) 未命中时以 `model_key = None` 走同款收窄；交集空 ⇒ 503，fail-closed。若不这么做，同一 key 的走向会因 body 是否带 `model` 字段而不同，是可观测的不一致。**对应的设计文档 §4.3 修订须与 T4/T5 同一次交付落地**（不是拖到 T8）。
- **Q11 = 是（ruled，严格 a′）**：默认路由的 provider 不服务某 model 时该 model 对该前缀 fail-closed。放宽即是对 §4.2 明确否决的"软偏好 (c)"，且会让默认路由比同形状的 operator binding 更弱。写路径规则 2 在提交时挡住模型专属行；残留漂移（operator 事后解除映射）在 (3)/(3.6) fail-closed，与今日 binding 行为一致。以测试 `resolve_sub_tenant_route_provider_not_serving_fails_closed` 固化，并写入 T8 运维文档：**"默认路由把子租户流量钉在单一 provider；该 provider 不服务的 model 对该前缀不可路由"**。

**验证**：`cargo test -p hydra-core --test router`。

---

### T5 — `proxy.rs` 接线（模型缺失 passthrough，Q10 已裁定）

- `passthrough_candidates`（`:1475`，operator binding 在 `:1481`）在 (3.5) 未命中时调用 `match_sub_tenant_route(..., None, api_key)`，以**默认路由**收窄到单个 provider；交集空 ⇒ 返回 `None` ⇒ 503（与 binding 命中死 provider 的现有行为一致）。
- **同步修订 `dev-docs/design-sub-tenant.md` §4.3**，把 `passthrough_candidates` 列入镜像/闸门面并记录 Q10 裁定（Design Defect，须与本次代码同交付）。
- 正常路由路径：`resolve` 签名不变，**无需改动调用点**，仅新增集成覆盖。

**测试**：`tests/terminate_mode.rs` 追加模型缺失 + 子租户默认前缀收窄用例，以及默认 provider 不可用 ⇒ 503 的 fail-closed 用例。

---

### T6 — operator admin CRUD + 写路径 error 级校验 + 配额

**T6.1 路由注册**（`admin/mod.rs` flat 资源，沿用 `provider-key-bindings` 样式；Q12 已裁定接受）：
- `("sub-tenants", None)` / `("sub-tenants", Some(id))`
- `("sub-tenant-routes", None)` / `("sub-tenant-routes", Some(id))`

**T6.2 handlers**（`admin/handlers.rs`，镜像 `provider_key_binding_collection/item :1430/:1479`）：
- 子租户：GET 列表（可按 `tenant_id` 过滤）、POST 创建、GET/PUT/DELETE 单项。
- 路由：同构。
- 创建子租户时：若 `key_prefix` 缺省，**leader 侧生成** 8 位 `[A-Z0-9]` + 分隔符 `_`（如 `QQCX_`）。**生成→校验→插入必须包在同一个重试循环里**（5 次）：既覆盖 DB `UNIQUE(tenant_id,key_prefix)` 冲突，也覆盖规则 3 的**重叠拒绝**——自动生成的 9 字符前缀可能是既有更短前缀的超串（既有 `QQCX_` vs 生成 `QQCX_WXYZ`），否则会把一个未提供前缀的请求以 400 拒绝（oracle F3）。
- 提交前缀同样强制"含分隔符"校验，与 T1.3 的快照侧检查保持一致，不得分叉。
- 变更成功后 `reload_best_effort`（与 binding 一致），获得 `maybe_forward_mutation` 的 leader 转发。
- **适用范围一句话（oracle F6）**：该"自动转发"只在**提供 admin 面的节点（leader/standby）**成立；edge 在鉴权前就 404 所有 admin 路径（`admin/mod.rs:596-607`），因此从 edge 调 admin CRUD 与所有既有 admin 资源一样不可用——这是 v1 已知边界，写入 ops 文档。

**T6.3 写路径校验（新建纯函数，error 级 fail-closed；住 core）**：
建议 `crates/hydra-core/src/config.rs` 或新纯模块：

```rust
pub enum SubTenantWriteError { ... }
pub fn validate_sub_tenant_write(cfg: &ConfigData, ...) -> Result<(), SubTenantWriteError>;
```

规则（设计 §3.3）：
1. provider 存在且 ∈ 该租户 `tenant_providers`；
2. `model_key` 若填，∈ 该租户 `tenant_models` 且该 provider 服务该 model；
3. 前缀非空、含分隔符、ASCII，与**同租户**既有前缀及**启用中 operator binding** 无重叠（`new.starts_with(existing) || existing.starts_with(new)`）；
4. **每租户配额**：`MAX_SUB_TENANTS_PER_TENANT`、`MAX_ROUTES_PER_SUB_TENANT` 常量（防经 API 的配置 DoS）。
5. 所有失败返回 400/409 语义化错误码，不入库。

`config::validate` 的 warn 仅作快照侧孤儿兜底，**不作为唯一防线**。

**测试**：
- core：`crates/hydra-core/tests/sub_tenant.rs` 写校验矩阵（每规则一条 + 重叠 + 配额）。
- server：`tests/sub_tenant_admin.rs`（`--features server`，镜像 `admin_api.rs:1843`）：201/409 重名/400 空前缀/400 非本租户 provider/400 model 不匹配/400 与 operator binding 重叠/400 超配额/200 列表/200 PUT/404/204；并断言热加载后 `ConfigData.sub_*` 生效。

---

### T7 — 租户 API 只读端点（快照喂养）

- **T7.1 `hydra-core/src/tenant_api.rs::parse_route`**（`:172-188`）新增 `Endpoint::ListSubTenants`（`api/v1/sub-tenants`）与 `Endpoint::ListSubTenantRoutes`（`api/v1/sub-tenants/{sub_tenant_id}/routes` 或 `api/v1/sub-tenant-routes`，与 T6 形状一致）。
- **T7.2 server handler**（`tenant_api/handlers.rs`）：复用既有令牌闸门 `authenticate` 与 `store.replication()` 快照；**不查 DB**；只返回属于该租户的项；edge 可服务。默认只读，不含写。
- 响应含 `config_version`，供 v2 对账基线。

**测试**：`crates/hydra-server/tests/tenant_api.rs` 追加只读端点 200/401/跨租户不可见/edge 快照路径；core `crates/hydra-core/tests/tenant_api.rs` 追加 `parse_route` 正/负例（near-miss 仍本地 404）。

---

### T8 — 文档同步

- `dev-docs/design.md` §7.1b：新增租户前缀路由层级（(3.6)）与 (a′) 裁定。
- `dev-docs/design-sub-tenant.md`：回填 errata（E-1、E-2、E-5、E-6 为**Implementation Drift（文档）**）与 Q10/Q11 裁定注记（升级为"已排实施计划"）。**注意**：§4.3 的修订（E-4，Design Defect）已在 T4/T5 交付中提前落地，不在此处补。
- `dev-docs/tenant-api-integration.md`：**必须写明**——前缀非身份/非秘密；归因可信度=租户自身发 key 纪律；**停用/删除 ≠ 吊销**（吊销永远在 `auth_url`）；edge 陈旧窗口（~1s + 版本防倒退）。
- `dev-docs/ops.md`：子租户配额、可观测（路由命中指标）。
- `dev-docs/HANDOFF.md`：迁移 0010；`.sqlx` 计数按核实值更正（oracle 实测 44，文档旧记 33；T2/T3 后还会再变，建议写成"以 commit X 时点为准"或直接去掉具体数字）。
- `dev-docs/aegis/INDEX.md`：新增本计划条目。
- v2 前：创建 A-2 决策记录文件（**不在 v1 T8 创建**，仅登记）。

---

### T9 — 门禁 + 验收证据回填

见"验证与门禁"。所有门禁带 `RUSTFLAGS=-D warnings`。

---

## v2 / v3 路线图（不在本轮实现）

### v2 — 租户自助写（A′）
**第 0 步（前置，硬性，已完成）**：**A-2 决策记录已落**于 `dev-docs/design-tenant-api.md` §6.4b（本项目无 ADR 体系，决策记录随设计文档走，见其 §6.5——**不新建独立 ADR 文件**，原计划中的 `aegis/adr/A-2-*.md` 路径作废）。A-2 显式、限定地修订 A-1 的机制偏好（仅配置写），并把实现前置条件写入 A-2（含 **4 授权绑定、5 凭据放置/禁日志、6 审计归因、7 接收侧租约断言**，以及 v1 复审 findings 1/2）。**A-2 已经 oracle 复核 GATE: PASS。**
1. `hydra-server` internal 面新增 `/api/v1/internal/tenant-config/...`（挂在既有 `HYDRA_CLUSTER_TOKEN` 闸门 `admin/mod.rs:619-641` 下）。
2. edge 数据面 handler → forward 到活跃 leader；复用 `forward.rs` 注册表实时解析、`FORWARD_ONCE_HEADER` 环守卫、connect/total 双超时与确定失败/结果未知分类。
3. **专用请求头 `x-hydra-tenant-token` 携带租户 Bearer（不在 body）**；leader 侧用**同一 `authenticate`** 重鉴权，并做**授权绑定**（写目标 == 已鉴权租户；A-2 前置条件 4）。
4. **幂等写**：PUT-by-`(tenant_id, name)` upsert + **按不可变 id** 的幂等 DELETE；接收侧须断言本节点持有租约（A-2 前置条件 7），非 leader ⇒ 503；leader failover 在 apply 后/ack 前发生时租户重试收敛。
5. 对账：`whoami.config_version`（`auth.rs:54-57`）。
6. 每租户配额 + **独立限流维度**（防配置写扇出放大）。
7. 分区语义：写 fail-closed（503/504），读继续用旧快照。

### v3 — 用量归因
`usage_record` 新增"子租户 id"列，**路由时**从已认证请求原始前缀派生并存储（禁止事后从掩码 key 重建，`sink.rs`）；SQLite + ClickHouse 双 sink 同步改。

---

## 设计用例覆盖矩阵（Q1–Q9 → 测试）

| 设计决策 | 内容 | 覆盖任务 / 测试 | 状态 |
|---|---|---|---|
| Q1 (a′) | operator 胜出、否则求交、空 fail-closed | T4.2/T4.4 `resolve_sub_tenant_operator_binding_wins`、`..._fails_closed` | 计划 |
| Q2 租户内唯一 | `UNIQUE(tenant_id,key_prefix)` | T1.1 + T2 `sub_tenant_crud` | 计划 |
| Q3 共存 | v1 不删 binding | 兼容性边界 + T4 | 计划 |
| Q4 单 provider 两级 | model 专属 > 默认 | T1.1 部分唯一 + T4.1/T4.4 | 计划 |
| Q5 edge 写 | v1 回避、v2 A′+A-2 | v2 路线图 | 计划 |
| Q6 failover/分区 | 幂等 + FORWARD_ONCE + 对账 | v2 路线图 | 计划 |
| Q7 停用≠吊销 | fall-through，非吊销 | T4 `..._prefix_mismatch_unchanged` + T8 文档 | 计划 |
| Q8 不验前缀归属 | 仅路由选择器 | 设计 §5 + T7 文档 | 计划 |
| Q9 前缀长度/字符集 | 8 位大写字母数字 + 分隔符 | T6.2 + T6.3 + Q13 | 计划 |
| §4.3 目录镜像 | 逐闸门一致 | T4.3 `catalog_sub_tenant_*` | 计划 |
| §7 安全 | 前缀非身份、写路径 error 级 | T6.3 校验矩阵 | 计划 |

---

## 指标交付矩阵

| 指标 | 定义 | 交付点 |
|---|---|---|
| 子租户路由命中 | key 命中子租户路由的请求数 | T5/T9（若设计要求指标，登记到 metrics 注册表；否则显式标注"v1 不加指标"） |
| 目录一致性 | 目录 model 数 == resolve 可路由 model 数（同 key/tenant） | T4.4 一致性测试 |
| 写校验拒绝 | 各类 400/409 错误码 | T6.3 测试矩阵断言 |

> 注：若设计未要求新增 Prometheus 指标，v1 以**测试断言**作为目录一致性的可核验证据，不虚报指标产出。

---

## 兼容性与风险

| 风险 | 影响 | 缓解 |
|---|---|---|
| 只改 `content.rs` 漏掉 `snapshot.rs`/`restore.rs` | edge 路由为空或全量行丢失 | E-1 + T3 明确四处清单 |
| `WIRE_VERSION` 未提升导致同版本互操作失败 | 新旧混合集群 serde error（非预期的版本门禁） | **强制 bump 2→3**（F1），并写明"先升级 reader/standby 再由 leader 发新格式" |
| 未记录 passthrough 镜像点（Q10） | 模型缺失请求绕过子租户路由 | T4.5/T5 显式决策 + 测试 |
| 默认路由对未服务 model fail-closed（Q11） | 子租户部分 model 突然 503 | 严格 (a′) + 写路径校验 + 文档；oracle 裁定 |
| `.sqlx` 缓存失配 | CI `SQLX_OFFLINE` 编译失败 | 优先运行时 query；否则走刷新流程并提交 |
| 快照膨胀/配置 DoS | 快照/内存增长 | T6.3 每租户配额 + 前缀/条目上限 |
| 租户误以为删除=吊销 | 安全事故 | T8 文档强制写明 |

---

## 退役（Retirement）

- v1 **不退役** `ProviderKeyBinding`（Q3）。
- 无旧路由/旧端点被本计划替换。
- v2 若落地 A′，需在 A-2 中记录 `maybe_forward_mutation` 对租户写**不适用**（不得复用其 admin-token 中继），避免后续误用。

---

## 待决与裁定

**全部已由 oracle 裁定（2026-09-18），无阻塞。**

| # | 问题 | 裁定 | 落地 |
|---|---|---|---|
| Q10 | 3.6 是否作用于模型缺失 passthrough | **是**：仅默认路由（`match_sub_tenant_route(..., None, ..)`）参与收窄；不做则同一 key 因 body 是否带 `model` 而行为分叉 | T4.1/T5 + 同步改 design §4.3 |
| Q11 | 默认路由对"默认 provider 不服务的 model"是否严格 fail-closed | **是**（a′ 严格解读）；放宽即被 §4.2 否决的软偏好 (c) | T4.4 测试 + T8 文档 |
| Q12 | admin 路由形状 | **flat**：`sub-tenants` / `sub-tenant-routes`（同既有资源样式） | T6.1 |
| Q13 | 前缀生成细节 | 8 位 `[A-Z0-9]` + `_`；**重试循环须同时覆盖 DB 冲突与重叠拒绝**（F3） | T6.2 |
| E-1、E-2、E-5、E-6 | 文档陈旧 | Implementation Drift（文档），语义不变 | T8 归档 |
| E-3 | admin catalog 镜像 | 无缺陷：keyless 视图，3.6 结构性 no-op | 仅注释 |
| E-4 | design §4.3 清单不完整 | **Design Defect**，Q10 裁定已消除歧义 | 与 T4/T5 同交付修订 design |

---

## 验证与门禁

### 计划阶段门禁（oracle 交叉审核，2026-09-18）
- [x] oracle 逐条事实核验：E-1..E-6 全部 Verified（`HydraWire` 不存在、internal 闸门实为 `mod.rs:619-641`、catalog keyless、passthrough 第四调用点、双 ConfigData 字段、`.sqlx`=44）。
- [x] oracle 裁定 Q10/Q11（均=是）与 Q12/Q13（接受，Q13 加 F3 收紧）。
- [x] 架构完整性 7 维审查通过（BASELINE-GOVERNANCE §6）。
- [x] Design Defect / Implementation Drift 全部归类并处置（E-1/2/5/6 文档 drift；E-3 无缺陷；E-4 Design Defect → 与 T4/T5 同交付修 design §4.3）。
- [x] 判定：**GATE: PASS**（条件式：F1 强制计划文本修订已落，无需复审；F2–F7 已并入）。
- Findings 处置：**F1 已落（强制 WIRE_VERSION bump）**；F2 级联清单已更正；F3 已并入 T6.2；F4 已改 `Option<&str>`；F5 已在 T1.1 显式说明；F6 已加 edge 边界句；F7 wipe 顺序已明确。

### 实现阶段门禁（后续）
```bash
cargo fmt --all -- --check
RUSTFLAGS=-D warnings cargo clippy --workspace --all-targets --features server -- -D warnings
cargo build --workspace --features server
cargo test -p hydra-core
cargo test -p hydra-server --features server --test repo --test loader
cargo test -p hydra-server --features server
cargo test -p hydra-server --features server,cluster-redis   # 集群路（T3/T5）
# 依赖防火墙
cargo tree -p hydra-core | rg 'tokio|pingora|sqlx|reqwest|hyper' && exit 1 || true
# 生产代码 mock/桩门禁（限定本次新增/改动文件）
git diff -U0 <base> -- <本次文件…> | rg '^\+.*(#\[cfg\(test\)\]|mock|Mock|stub|unwrap\(\)|expect\(|panic!)'   # 期望为空（测试文件除外）
```
- [ ] 目录一致性：新增 core 测试证明 `accessible_models` 与 `resolve` 在 3.6 上一致。
- [ ] 零行为回归：未命中前缀的请求路由结果不变。
- [ ] 快照 round-trip 含新字段（build→hydrate）测试通过。
- [ ] 全部 v1 测试绿；工作区无新增 clippy 告警。

---

## 执行路线 / Execution Route

```
T1 (core: 迁移模型/配置/校验)
 └─> T2 (db CRUD)
      └─> T3 (store→content→snapshot→restore 投影)
T1 ─> T4 (router 3.6 + 目录镜像 + 单测)        [可与 T2/T3 并行，写不同文件]
T4 ─> T5 (proxy passthrough 默认路由收窄)
T2/T3 ─> T6 (admin CRUD + 写校验)
T3 ─> T7 (租户只读端点)
T1..T7 ─> T8 (文档) ─> T9 (门禁 + 证据)
```

- **可并行**：T4（`router.rs`）与 T2/T3（`db.rs`/投影）写不同文件，无冲突。
- **写者隔离**：T1 先落地 `ConfigData` 字段，避免 T3 与 T6 争抢 `config.rs`。
- **T6 是 v1 的收敛点**：operator 代管写 + 自动 leader 转发，零新集群机制。

---

## 修订记录

| 日期 | 变更 |
|---|---|
| 2026-09-18 | 初稿：v1 详细拆分（T1–T9）+ v2/v3 路线图；登记设计 errata E-1..E-6 与待确认 Q10–Q13。待 oracle 交叉审核。 |
| 2026-09-18 | oracle 计划阶段门禁：**GATE: PASS**。F1 强制落（WIRE_VERSION 2→3 为必选，无 no-bump 分支）；F2 级联清单更正；F3 T6.2 重试覆盖重叠；F4 `Option<&str>`；F5 T1.1 只留部分唯一索引；F6 edge admin 边界；F7 wipe 顺序。Q10/Q11 裁定=是，Q12/Q13 接受。E-4 定性 Design Defect，design §4.3 修订并入 T4/T5。 |
| 2026-09-18 | 实现阶段：**T1–T8 全部完成**，T9 门禁执行（证据见下节）。代码未提交。 |

---

## 实施记录与验收证据（T9，2026-09-18）

### 交付状态
**T1–T8 全部实现**；v1 范围完成：operator admin CRUD + error 级写校验 + 路由 (3.6) + 目录镜像 + 模型缺失 passthrough 默认路由收窄 + 租户只读端点 + 文档同步。**v1 已提交**（`da07610` 起 6 个逻辑提交，`da07610~1..1007dea`）。

### 任务 → 关键产物
| 任务 | 产物 | 状态 |
|---|---|---|
| T1 | `migrations/0010_sub_tenant.sql`、`model.rs` 两实体、`config.rs` 两字段 + warn 校验 | ✅ |
| T2 | `db.rs` 12 个 CRUD/on-fn + `tests/repo.rs::sub_tenant_crud` | ✅ |
| T3 | `store.rs` enabled-only 加载、`content.rs` `FidelityRows`、`snapshot.rs` `FidelityWireRows` + **WIRE_VERSION 3**、`db/restore.rs` wipe/INSERT | ✅ |
| T4 | `router.rs::match_sub_tenant_route` + (3.6) 闸门 + `accessible_models` 镜像 + 10 单测 | ✅ |
| T5 | `proxy.rs::passthrough_candidates` 默认路由收窄 + 2 集成测试 | ✅ |
| T6 | `hydra-core/src/sub_tenant.rs` 校验器（12 错误变体 + 配额）+ admin 4 路由 + 19 core + 4 集成测试 | ✅ |
| T7 | `tenant_api.rs` 两只读端点 + 快照 handler + 6 集成 + 2 core 测试 | ✅ |
| T8 | `design.md §7.1c`、`tenant-api-integration.md`、`ops.md §5.5`、`HANDOFF.md`、`INDEX.md` | ✅ |

### 门禁证据（可重跑）
```bash
cargo test -p hydra-core                                    # 17/17 套件全绿，0 failed
cargo test -p hydra-server --features server                # 全量套件 0 failed
HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380 \
  cargo test -p hydra-server --features server,cluster-redis # 仅 admin_api 2 例既有失败（见下）
cargo build --workspace --features server                   # ok
cargo fmt --all -- --check                                  # ok
RUSTFLAGS=-D warnings cargo clippy -p hydra-core -p hydra-server --features server --all-targets -- -D warnings   # ok
cargo tree -p hydra-core | rg 'tokio|pingora|sqlx|reqwest|hyper'   # 空（依赖防火墙成立）
```

### 新增测试（实现交付）
- core `validate.rs`：4（T1）
- core `router.rs`：10（T4）
- core `sub_tenant.rs`：19（T6 写校验矩阵）
- core `tenant_api.rs`：`parse_route` 正/负例（T7）
- server `repo.rs`：`sub_tenant_crud`（T2）
- server `loader.rs` / `fidelity_disabled_rows.rs` / `cluster.rs`：3（T3，含 `WIRE_VERSION==3` round-trip）
- server `sub_tenant_admin.rs`：4（T6 HTTP CRUD + 热加载）
- server `tenant_api.rs`：6（T7 只读 + 跨租户隔离 + edge 快照）
- server `terminate_mode.rs`：2（T5 passthrough 收窄 + fail-closed 503）

### 已知既有失败（**非本次回归，已证**）
`--features server,cluster-redis --test admin_api` 中 2 例失败（期望 200，实得 202）：
- `empty_body_delete_invalidates_all_local`
- `too_many_invalidation_keys_are_refused_and_publish_nothing`

二者均为 E2 收敛屏障 `broadcast_and_confirm` 的 `Pending(202)` 路径，与子租户无关；在**干净 HEAD worktree（692655d）**上同样 2 失败，故为既有基线/环境问题。`--features server` 下不受影响（全绿）。证据命令：`git worktree add --detach <tmp> HEAD` 后原样复跑，`37 passed; 2 failed`。

### 执行期修正 / 偏离
- **WIRE_VERSION 2→3 强制**（oracle F1），升级顺序：先 reader/standby，再由 leader 发新格式。
- **`--features db` 单独不可编译**，门禁改用 `--features server`（T2 实测，计划与 T9 配方已更正）。
- T6 引入请求 DTO 以区分"前缀省略（自动生成）"与"空前缀（400）"；配额 `MAX_SUB_TENANTS_PER_TENANT=64`、`MAX_ROUTES_PER_SUB_TENANT=32`。
- T7 采用 flat 只读端点 `api/v1/sub-tenants`、`api/v1/sub-tenant-routes`（与 admin 资源形状一致）。
- 过程事件（须记录）：并行 lane 期间一次 workspace 级 `cargo fmt --all` 触碰了在途文件（仅格式化、无逻辑损失）；后续改为各 lane 只格式化自有文件。

### 未做（明确不属于 v1）
- v2 租户自助写（A-2 决策记录前置）、v3 用量归因；v1 不新增 Prometheus 指标；不退役 `ProviderKeyBinding`。

### 实现后 oracle 对抗式复审（2026-09-18）
**GATE: PASS**（无阻塞项）。7 项 mandate 全部核验通过：闸门 (3.6) 无提权路径、operator 权威、目录镜像与 `resolve` 逐闸门一致、写校验 error 级完整（F3 重试成立）、四处投影 + WIRE_VERSION 3 完整且双向版本门禁、租户只读无跨租户泄漏、无生产 unwrap/mock、测试无假覆盖。

非阻塞 findings 与处置：
| # | 内容 | 级别 | 处置 |
|---|---|---|---|
| 1 | 配额按 **enabled-only** 快照计数；"停用→再建"可绕过 64/32 上限（DB 与全量行无界增长） | NON-BLOCKING | **记为 v2 A-2 前置项**：v2 写路径必须在事务内按 DB 全量行计数（v1 仅 operator 面，危害有限） |
| 2 | 校验→插入 TOCTOU：并发两个"不等但重叠"前缀都能过（DB unique 只挡相等） | OBSERVATION | 已在 `admin/handlers.rs` 写路径注释登记；v2 必须在插入事务内复验 |
| 3 | 重叠防护单向：子租户写拒绝与 operator binding 重叠，但 operator binding 写不回查子租户前缀 | OBSERVATION | 已在 `ops.md §5.5` 写明；(a′) 下 operator 胜出，行为确定、目录不撒谎 |
| 4 | 租户只读视图为 enabled-only，原文档措辞易被读成含停用行 | OBSERVATION | 已改 `tenant-api-integration.md`（明确"仅启用中"） |
| 5 | admin GET 列表跨租户（无 `tenant_id` 时返回全租户） | OBSERVATION | 与既有 admin 资源一致、token 门控；v2 不得复用该 handler 做租户面 |

建议的后续（非阻塞）：`cluster.rs` 增加一条"v2 帧被 v3 reader 拒绝"的混合版本测试，以钉住升级顺序契约。
