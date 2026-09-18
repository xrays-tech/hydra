# 实施计划：子租户 v2 —— 租户自助写（A′）

> 状态：**计划阶段门禁通过（oracle GATE: PASS，2026-09-18）——findings 已并入；实现阶段待启动。**
> 目标设计：`design-sub-tenant.md` §6.3/§6.5（A′）、`design-tenant-api.md` §6.4b（**决策记录 A-2**，已决，oracle GATE: PASS）
> 前置：v1 已实现并提交（`da07610` 起 6 个提交）；本文只覆盖 **v2**。
> 日期：2026-09-18

---

## Goal

让租户在自己数据面入口（可能是 edge）自助 **CRUD 子租户及其路由**，写最终由**唯一写者 leader** 落库，具备：幂等语义、leader failover 重试收敛、`config_version` 对账、分区 fail-closed、以及 A-2 要求的**授权绑定 / 接收侧租约断言 / 独立限流 / 凭据不外泄 / 审计归因**。

成功定义：
1. 租户 `PUT`/`DELETE` 子租户与路由端点，幂等、可重试、跨节点一致。
2. 写请求在 edge 认证后转发；leader **重鉴权并做授权绑定**（写目标 == 已鉴权租户）。
3. leader internal 写 handler **断言本节点持有租约**，非 leader ⇒ 503，绝不本地写。
4. 写校验在**事务内**按 **DB 全量行**进行（修 v1 复审 findings 1/2）。
5. 每租户配置写有**独立限流维度**；租户 Bearer 走专用头且链路禁日志；配置写有审计归因。
6. 分区期间写 fail-closed（503/504），读继续用旧快照。

---

## Architecture

```
edge 数据面 (tenant_api write handler, :8080)
  1. 保留前缀解析 + 租户令牌闸门（快照，零 I/O）
  2. 每租户配置写限流（独立维度）
  3. 若本节点是唯一写者（单节点 leader_ready==None）→ 本地执行
     否则 → TenantConfigForwarder::forward_config_write(leader_url)
         Authorization: Bearer <HYDRA_CLUSTER_TOKEN>
         x-hydra-tenant-token: <tenant access token>
         x-hydra-forwarded: 1, x-hydra-trace-id
                    │
                    ▼
leader admin/control 面  /api/v1/internal/tenant-config/...
  4. internal 闸门已用 cluster token 认节点（admin/mod.rs:631-653）
  5. 接收侧租约断言：is_leader_candidate() else 404；leader_ready Some(false) ⇒ 503 not_leader；None/true 本地执行
  6. 读 x-hydra-tenant-token → tenant_api::authenticate(&state.store, ·) 重鉴权（NotReady ⇒ 503）
  7. 授权绑定：body tenant_id != T ⇒ 403（先于任何查询）；sub_tenant_id 缺失/他租户 ⇒ 404
  8. 事务内：读全量行 → 复验 → upsert/delete → commit → reload → config_version++
  9. 结构化审计日志（tenant_id / trace_id / action / resource / version）
                    │
                    ▼
        返回 200/201/204 或 400/403/404/409/503/504（经 forward 分类）
```

**零新集群机制类别**：复用既有 internal cluster-token 闸门与 `forward.rs` 的超时/确定失败分类。

---

## 关键设计裁定（plan-level decisions）

| # | 决策 | 理由 |
|---|---|---|
| D1 | **数据面转发 plumbing（信任受控）**：新增 `TenantConfigForwarder { cluster_token: Option<String>, leader_url: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>> }`，随 `AppState` 注入；仅租户配置写路径可使用。**不把 `NodeRegistry` 交给数据面** | 现状数据面拿不到 cluster token/registry/control URL（`AppState` 无这些字段；`ControlClient` 只 `.spawn()`）。闭包只暴露"给我 leader URL"，把 peer 解析能力收在 admin/cluster 侧 |
| D2 | **新增专用转发函数**（`forward.rs`）：发送 `Authorization: <cluster token>` + `x-hydra-tenant-token: <tenant bearer>`，**不**中继调用方 `Authorization`（现有 `forward_mutation` 固定中继 `authorization`，无法用于本路径，`:236-238`） | internal 闸门认的是 cluster token，而 edge 上调用方 `Authorization` 是租户令牌 |
| D3 | **edge 先认证、再转发**：数据面先走既有租户令牌闸门（快照）与限流，才转发；leader 再权威重鉴权 | 避免未认证请求放大到 leader；快照认证与 leader 重鉴权同源 |
| D4 | **幂等键**：子租户 `PUT` = upsert by `(tenant_id, name)`；`DELETE` 按**不可变 id**。路由 `PUT` = upsert by `(sub_tenant_id, model_key)`（`model_key` 省略 = 默认路由，走 `uq_sub_tenant_route_default` 部分唯一索引）；`DELETE` 按不可变 id | A-2 理由 4 要求 DELETE 按不可变 id（防迟到重放删同名新资源）；路由天然键由部分唯一索引保证 |
| D5 | **事务内写核心，admin 与租户两面共用**：`validate_sub_tenant_write` 改为接收**事务内读到的全量行快照** + `&ConfigData`（provider/model/operator binding 仍来自快照）；配额按**全量行**计数 | 修 v1 复审 findings 1（enabled-only 配额可绕过）与 2（TOCTOU）。admin 面同步受益 |
| D6 | **每租户配置写限流 = leader 侧单点**：在 leader internal 写 handler 前置一个 per-tenant `Throttle`（固定窗口，进程内）。配额可由常量/环境变量配置（建议默认 60 写/分钟/租户） | 所有写都在 leader 落库，单点限流即可覆盖全集群；`RedisRateLimiter` 目前只在 proxy `AppState`，不引入新 Redis 依赖到 admin |
| D7 | **审计归因**：leader 侧结构化 `tracing`（`tenant_id`、`trace_id`、`action`、`resource`、`resource_id`、`config_version`；node id best-effort）。**不新增审计表** | 现状无审计表；新增表属独立产品决策，v2 先以可检索日志满足追溯，并在本文记为待评估 |
| D8 | **单节点 = 本地执行**：`TenantConfigForwarder` 为 `None`（无集群）或 `leader_ready == None` 时，数据面直接调用同一个写核心 | 与 `maybe_forward_mutation`（`admin/mod.rs:492-498`）语义一致，单节点零回归 |
| D9 | **租户写端点形态**（flat，与只读端点同源）：`PUT /tenant/{tid}/api/v1/sub-tenants/{name}`、`DELETE /tenant/{tid}/api/v1/sub-tenants/{id}`、`PUT /tenant/{tid}/api/v1/sub-tenant-routes`（body 带 `sub_tenant_id,model_key?,provider_id,enabled`）、`DELETE /tenant/{tid}/api/v1/sub-tenant-routes/{id}` | 与 v1 只读端点同命名空间；路由用 body 表达可空 `model_key`，避免路径编码 NULL。**条件（oracle 裁定）**：`name` 必须加字符集规则（见 V4），否则含 `/` 的旧名不可被 URL-keyed PUT 寻址；leader 与 edge 对原始 path segment 的比较必须一致（无 percent-decoding 不对称） |

---

## Baseline / Authority Refs

| 权威 | 路径 | 用途 |
|---|---|---|
| A-2 决策记录 | `design-tenant-api.md` §6.4b | 信任模型、前置条件 1–7 |
| 子租户设计 | `design-sub-tenant.md` §6.3/§6.5 | A′ 与备选裁定 |
| v1 实施计划 | `plans/2026-09-18-sub-tenant.md` | 数据模型/写校验/只读端点/复审 findings |
| 集群 | `dev-docs/cluster.md` | LEADER/EDGE、租约 |
| 纪律 | `dev-plan.md` §1 | TDD、零 mock、真实 Redis |

### 已核实事实（v2 recon，2026-09-18，HEAD `0015706`）

- internal 闸门：`admin/mod.rs:631-653`（cluster token、fail-closed），**早退 `route()`** 在 `:650-652`，先于 admin token 闸门与 `maybe_forward_mutation`；edge 在 `:611-619` 预 404 一切非健康路径。
- 路由注册需在深路径拒绝 `:401-403` **之前**（先例 `/internal/control` `:373-375`、`tenants/auth/test` `:384-386`）。
- 接收侧租约模板：`cluster_api.rs:191-210`（`is_leader_candidate` else 404；`leader_ready` Some(false) ⇒ 503）。
- 转发：`forward_mutation` `forward.rs:189-196`（**不 feature-gated**，但只中继 `authorization`+`content-type`）；`forward_target_from_registry` `:128-147`（`cluster-redis`）；`ForwardError` 分类 `:60-109`；超时 `:214-263`。
- 数据面缺口：`AppState`（`proxy.rs:108-164`）无 cluster token/registry/control URL；`ControlClient` 在 `main.rs:858-878` `.spawn()` 后未存。
- 租户重鉴权：`tenant_api/auth.rs:64` `authenticate(&ConfigStore, bearer)`；`AdminState.store` 与数据面同源，可调用。URL↔token 交叉检查模板 `tenant_api/mod.rs:403-415`（403）。
- v1 写路径可复用：`validate_sub_tenant_write`（`sub_tenant.rs:122`）、`generate_sub_tenant_prefix`（`handlers.rs:1588`）、`is_unique_conflict`（`:1600`）、重试常量（`:1563`）、`reload_best_effort`（`:135`）。
- DB：`list_*_on<E: Executor>`（`db.rs:1482/1561`）可传 `&mut *tx`；事务模板 `upsert_provider_key`（`:574`）、`write_tenant`（`:790`）。**无** count-all、**无** natural-key upsert。
- 限流：`Throttle`（`throttle.rs:29`，DashMap 固定窗口）可用；`TenantApiLimiter`（`tenant_api/limit.rs:71`）是数据面进程内；`RedisRateLimiter` 只在 proxy。
- 审计：无表；仅 `tracing` + `x-hydra-trace-id`（`forward.rs:233` 中继）。

---

## 需求就绪检查 / Requirement Ready Check

- [x] A-2 已决且 oracle GATE: PASS（信任模型/绑定/租约断言/幂等/对账）。
- [x] 数据面 plumbing 缺口已识别并裁定 D1/D2/D3。
- [ ] **待确认**：D9 租户写端点的精确形态（尤其路由 PUT 的自然键表达）——本计划给出建议，oracle 裁定。
- [ ] **待确认**：D6 默认限流值与是否需环境变量可配。

---

## 变更必要性 / Change Necessity

- 租户自助写是 A′ 的目标；v1 只读不满足设计 §6.2 的 v2 目标。
- 不新增机制：复用 internal cluster-token 面与 forward 语义（D1/D2 仅补传输参数）。
- findings 1/2 是 v1 已记录的**必须修**项，v2 一并落地（D5），否则租户面继承可绕过的配额与 TOCTOU。

---

## 既有面复用检查 / Existence Check

| 需求 | 既有可复用 | 结论 |
|---|---|---|
| 节点认证 | internal cluster-token 闸门 `admin/mod.rs:631-653` | 直接复用 |
| 租约断言 | `cluster_api.rs:191-210` 模板 | 复制结构 |
| 租户重鉴权 | `authenticate(&store, bearer)` | 直接复用 |
| 转发分类 | `ForwardError` + 超时 | 复用；仅新增 header 组合 |
| 写校验/生成/错误映射 | v1 helpers | 复用 + D5 事务化改造 |
| 限流 | `Throttle` | 复用为 leader 侧新维度 |
| 对账 | `whoami.config_version` | 直接复用 |

**不存在**：数据面 cluster-token/leader 解析（D1 新增）；natural-key upsert（D4 新增）；count-all（D5 新增）。

---

## 架构完整性视角 / Architecture Integrity Lens

1. **Ownership**：写核心唯一 owner = `hydra-server` 写模块（admin 与 tenant 面共用）；纯校验仍归 `hydra-core`。
2. **Boundaries**：数据面通过受控 `TenantConfigForwarder` 触达 leader，**不**直接持有 registry；internal 写 handler 是唯一接收侧。
3. **Contract changes**：新增 internal 路由 + 4 个租户写端点 + `parse_route` 变体；`AppState` 新增 forwarder 字段。
4. **Cascade**：`parse_route` 增写方法/变体；`validate_sub_tenant_write` 签名变更需同步 v1 admin 调用点（D5）。
5. **Dependency direction**：不变；纯校验仍无 I/O。
6. **Retirement**：v1 只读端点保留；无删除项。
7. **Entropy**：新增 1 传输器、1 internal 路由族、4 端点、1 限流维度——均为 A-2 明确要求。

---

## 兼容性边界 / Compatibility Boundary

- 单节点（forwarder None）与 `leader_ready None`：本地执行，行为等价于 v1 admin 写。
- v1 admin 写路径：D5 后配额改为全量行、TOCTOU 关闭——**行为收紧**（更严格），需在文档记录。
- 读写分离：写 fail-closed 不影响读（读仍快照）。
- `WIRE_VERSION` 不变（无模型变更）；无迁移（如需 count 索引，评估）。

---

## TDD Route

**Strict。** 传输器/写核心/端点先红后绿；集群路测试连**真实 Redis**（`HYDRA_TEST_REDIS_URL`，本机 6380）。

---

## 计划压力测试 / Plan Pressure Test

| 拷问 | 回答 |
|---|---|
| 是否引入新信任边界？ | 数据面获得"请求 leader URL 的闭包 + cluster token"——**受限于配置写路径**，且 leader 重鉴权 + 授权绑定兜底（A-2）。 |
| 能否更小？ | 仅子租户 CRUD 不做路由自助亦可更小；但设计目标含路由，故纳入。可用开关只开子租户。 |
| 依赖 v1 是否充分？ | 是；v2 独立于 v1 已提交状态，只在同一写核心上加事务与转发。 |
| 最大风险 | 数据面 cluster-token 暴露面 + 幂等路由键表达；已由 D1/D4 + oracle 复核约束。 |

---

## 计划期复杂度检查 / Plan-Time Complexity Check

- 新增：1 转发器、1 internal 路由族、4 端点、2 DB count、1 natural-key upsert、1 限流维度、审计日志、测试若干。
- 无新 crate、无新外部依赖（`reqwest`/`fred` 已在）。
- 复杂度集中在"传输 + 事务 + 幂等"三处，逐任务可独立验证。

---

## 执行就绪视图 / Execution Readiness View

- 全部输入已具（A-2 + recon）。硬阻塞仅 D9/D6 待 oracle 裁定；实现可按建议值起步并标注。

---

## 文件改动清单

### 新增
- `crates/hydra-server/src/tenant_config/forward.rs`（`TenantConfigForwarder`，D1/D2）
- `crates/hydra-server/src/admin/tenant_config_api.rs`（internal 写 handler，D3/D5/D6/D7）
- `crates/hydra-server/tests/sub_tenant_tenant_write.rs`（数据面/集群端到端）
- 可能：`crates/hydra-server/src/tenant_config/mod.rs`

### 修改
- `crates/hydra-server/src/cluster/forward.rs` — 新增带自定义头/覆盖 Authorization 的转发函数（D2）
- `crates/hydra-server/src/admin/mod.rs` — internal 路由注册（`:401` 之前）
- `crates/hydra-server/src/admin/handlers.rs` — v1 写路径改走事务化写核心（D5）
- `crates/hydra-server/src/proxy.rs` — `AppState` 增 forwarder 字段
- `crates/hydra-server/src/main.rs` — 装配/注入 forwarder（D1/D8）
- `crates/hydra-core/src/sub_tenant.rs` — 校验签名接收全量行快照（D5）
- `crates/hydra-core/src/tenant_api.rs` + `crates/hydra-server/src/tenant_api/{mod.rs,handlers.rs}` — 4 个写端点（D9）
- `crates/hydra-server/src/db.rs` — count-all + natural-key upsert（D4/D5）
- 文档：`design-sub-tenant.md`（v2 落地注记）、`tenant-api-integration.md`（写端点 + 边界）、`ops.md`（限流/审计/迁移）、`HANDOFF.md`、`aegis/INDEX.md`

---

## 分步任务（bite-sized）

### V1 — 数据面转发传输器（D1/D2）
- `cluster/forward.rs`：新增 `forward_config_write(base_url, method, path_and_query, tenant_bearer, body, trace_id)`：`Authorization: Bearer <cluster token>`、`x-hydra-tenant-token`、`FORWARD_ONCE_HEADER`、`x-hydra-trace-id`、`content-type`；复用超时/classification。
- `tenant_config/forward.rs`：`TenantConfigForwarder`（cluster token + leader-url 闭包）；`leader_url()` 由闭包解析，无则 `None`。
- `main.rs`：从 `ClusterConfig` + registry 装配闭包，注入 `AppState`（单节点为 None）。
- 测试：单元（头组合、无 token/无 leader 的错误）；集群（edge 转发到 leader，`x-hydra-forwarded` 环守卫）。

### V2 — leader internal 写端点 + 接收侧闸门 + 租约断言（A-2 前置 7）
- 注册 `/api/v1/internal/tenant-config/{resource}[/{id}]`（在 `admin/mod.rs:401` 之前）。
- handler：`is_leader_candidate()` else 404；`leader_ready Some(false)` ⇒ 503 `not_leader`；None/true 本地执行（模板 `cluster_api.rs:191-210`）。
- 测试：无 cluster token 401；非候选 404；无租约 503；持有租约执行。

### V3 — leader 重鉴权 + 授权绑定 + 审计（A-2 前置 4/6）
- 读 `x-hydra-tenant-token` → `authenticate(&state.store, ·)`；`NotReady` ⇒ 503；缺失 ⇒ 401。
- 绑定：`body.tenant_id != T` ⇒ 403（纯字符串比较，先于查询）；`sub_tenant_id` 缺失/他租户 ⇒ 404。
- 结构化审计日志（tenant_id/trace_id/action/resource/version）。
- 测试：403 mismatch、404 foreign、401 no token、审计字段存在（tracing 捕获或计数）。

### V4 — 事务化写核心（D5，修 findings 1/2）
- `validate_sub_tenant_write` 改为接收 `existing_sub_tenants: &[SubTenant]` + `existing_routes: &[SubTenantRoute]`（**全量行**）+ `&ConfigData`（provider/model/operator binding）；配额按全量。
- `db.rs`：`count_sub_tenants_by_tenant_on`、`count_routes_by_sub_tenant_on`、natural-key upsert（子租户 `(tenant_id,name)`；路由 `(sub_tenant_id, model_key)`）。
- 写核心：`begin → list_*_on(&mut tx) → validate → insert/update → commit`；admin 与 internal 共用。
- **`name` 字符集规则（D9 条件）**：`validate_sub_tenant_write` 增加 `name` 校验——非空、可打印、无 `/`、长度上限（镜像既有 `key_prefix` ASCII 规则）；否则 v1 旧名可存但无法被 v2 URL-keyed PUT/DELETE 寻址。
- 测试：并发重叠前缀在事务内被拒；停用后再建触发配额；upsert 幂等；DELETE by id 幂等且不误删同名新资源；含 `/` 的 name 被 400 拒绝。

### V5 — 租户写端点（D9）
- `parse_route` 扩展 4 端点（PUT/DELETE 子租户、PUT/DELETE 路由），带方法区分。
- 数据面 handler：先走令牌闸门 + URL↔token 403（模板 `tenant_api/mod.rs:403-415`）；调用写核心或转发。
- 测试：幂等 PUT 两次同结果；DELETE 幂等；跨租户 403/404；无令牌 401。

### V6 — 每租户配置写限流（D6）
- leader internal 写 handler 前置 per-tenant `Throttle`；超限 ⇒ 429。
- 常量/环境变量可配（默认建议 60/min）。
- 测试：第 N+1 次 429；不同租户互不影响。

### V7 — 文档 + 退役检查
- `design-sub-tenant.md` v2 落地注记；`tenant-api-integration.md` 写端点与边界（幂等/重试/`config_version` 对账/分区 503-504/删除≠吊销）；`ops.md`（限流/审计/转发边界/单节点，并写明 **leader failover 后进程内限流窗口重置 → 有界突发**，属 anti-DoS 语义，与 E2 TTL 同类）；`HANDOFF.md`；`INDEX.md`。

### V8 — 门禁 + 证据
- 见下。

---

## 验证与门禁

```bash
cargo test -p hydra-core
cargo test -p hydra-server --features server
HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380 \
  cargo test -p hydra-server --features server,cluster-redis
cargo build --workspace --features server
cargo fmt --all -- --check
RUSTFLAGS=-D warnings cargo clippy -p hydra-core -p hydra-server --features server --all-targets -- -D warnings
cargo tree -p hydra-core | rg 'tokio|pingora|sqlx|reqwest|hyper'   # 空
```
- [ ] 幂等：PUT 两次、DELETE 两次、failover 重试收敛到同一状态。
- [ ] 授权绑定负例：节点 token + 他租户 Bearer 无法写。
- [ ] 租约断言：standby/被罢黜 leader 上不本地写（503）。
- [ ] 分区：写 503/504，读旧快照。
- [ ] findings 1/2：事务内全量配额 + 重叠复验。
- [ ] 限流 429。
- [ ] 已知既有失败（`cluster-redis` 下 `admin_api` 2 例 E2 屏障）仍为基线，不新增失败。

---

## 风险与缓解

| 风险 | 缓解 |
|---|---|
| 数据面获得 cluster token/peer 解析 | D1 闭包最小暴露；仅配置写路径；leader 重鉴权 + 绑定；A-2 前置 7 |
| 路由 natural-key upsert 语义（NULL 默认） | D4 + 部分唯一索引；事务内复验；oracle 裁定 D9 |
| 事务内校验与快照不一致 | 以事务内 DB 行为准；`config::validate` warn 仅兜底 |
| 限流单点 | 所有写在 leader，单点即全覆盖；429 语义明确 |
| 审计无表 | D7 结构化日志；记入待评估（独立产品决策） |

---

## 退役（Retirement）

- 不退役 v1 只读端点与 admin CRUD。
- D5 收紧 admin 配额语义（enabled→全量）：属修正，须在 ops/HANDOFF 说明。

---

## 待确认

| # | 问题 | 建议 | 阻塞 |
|---|---|---|---|
| D9 | 租户写端点形态（路由 PUT 自然键） | 采用 §D9 建议（body 表达 `model_key?`） | V5 |
| D6 | 配置写限流默认值/可配 | 60/min/租户，环境变量可覆盖 | V6 |
| — | 审计是否需持久化表 | v2 用结构化日志，表另立决策 | 否 |

---

## 执行路线 / Execution Route

```
V1 (forwarder) ──┐
V2 (internal gw + lease) ──┤
V3 (re-auth + binding + audit) ──┤
V4 (tx write core + count/upsert) ──> V5 (tenant write endpoints) ──> V6 (rate limit) ──> V7 (docs) ──> V8 (gate)
```
- V1 与 V2/V3 可并行（不同文件）；V4 是写核心，V5 依赖 V1+V4。

---

## 修订记录

| 日期 | 变更 |
|---|---|
| 2026-09-18 | 初稿：v2（A′）任务拆分 V1–V8、D1–D9 设计裁定、recon 事实基线；待 oracle 架构复核。 |
| 2026-09-18 | oracle 计划阶段门禁 **GATE: PASS**。并入：D9 `name` 字符集条件（V4）、ops 的 failover 限流窗口注记（V7）、A-2 前置 7 行号更正 `:646-652`；新增 A-2 前置 8（数据面转发 plumbing 信任受控）。recon 全部引用经核验；A-1 逐字未动。 |
