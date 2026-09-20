# 实施计划：子租户 v3 —— 用量按子租户归因

> 状态：**计划待 oracle 架构交叉审核**
> 目标设计：`design-sub-tenant.md` §8 v3、§7.1（归因必须在**路由时**从原始 key 派生，不得事后从掩码 key 重建）
> 前置：v1（路由）+ v2（租户自助写）已实现、复审通过并提交（`9adbea1..d799756`）
> 日期：2026-09-18

---

## Goal

把**已认证请求的子租户归属**写入用量记录：`UsageRecord` 新增 `sub_tenant_id: Option<String>`，在**路由/记录时**从**原始 client api-key 前缀**派生（租户作用域），经 **SQLite 与 ClickHouse 双 sink** 落库，并可被租户 `/usage` 以 `group_by=sub_tenant` 读取。

**非目标**：不改动掩码 key 存储（原始 key 永不落库）；不做事后从 `client_api_key`（掩码）重建；不引入新的别名字段到 `/usage` 响应行（用分组维度暴露）。

成功定义：
1. 命中启用子租户前缀的请求，用量记录带该 `sub_tenant_id`；未命中/已停用 ⇒ `NULL`，绝不猜测。
2. 归因**独立于**路由闸门 (3.6) 与 operator key-prefix binding（两者都不影响"属于哪个子租户"）。
3. SQLite 与 ClickHouse 两条写入路径都带该列；CH 既有实例有明确的 `ALTER TABLE` 迁移步骤。
4. 租户可用 `group_by=sub_tenant` 读取自家分片；旧请求/响应完全兼容。
5. hydra-core 保持零 I/O；原始 key 不被新增存储。

---

## Architecture

```
已认证请求 (proxy)
  request_filter: ctx.client_api_key = 原始 key; tenant resolved
        │
        ▼
  logging hook (proxy.rs:1298)  ← 在此派生归因
    cfg = state.store.snapshot()          (已有 :1341)
    sub_tenant_id = router::sub_tenant_id_for_key(cfg, tenant.id, raw_key)  // 纯前缀匹配，model-free
    UsageRecord { ..., sub_tenant_id }
        │
        ├── SqliteSink  → usage_record.sub_tenant_id  (migration 0011)
        └── ClickHouseSink → usage_record.sub_tenant_id (init.sql + 既有实例 ALTER)
```

读取：`GET /tenant/{tid}/api/v1/usage?group_by=sub_tenant` → 复用既有 `key` 字段承载子租户 id（**无响应形状变更**）。

---

## 关键设计裁定（plan-level）

| # | 决策 | 理由 |
|---|---|---|
| D1 | **归因 = 前缀匹配，model-free、binding-free**：命中**启用中**子租户的前缀即归因；无匹配/已停用 ⇒ `None` | design §7.1"路由时派生"；停用即停止归因，与 enabled-only 快照一致 |
| D2 | **新增 `pub fn sub_tenant_id_for_key(cfg, tenant_id, api_key) -> Option<&str>`**（`router.rs`，封装现有私有 `match_sub_tenant`，返回 `st.id`） | 现有 `match_sub_tenant` 私有且正好是所需语义；`match_sub_tenant_route` 需要 model 且在无路由时返回 None（会漏归因），不可用 |
| D3 | **在 `logging` 记录点派生**（`proxy.rs:1298` 附近），不新增 `RequestContext` 字段 | `ctx.client_api_key`（原始 key）+ `ctx.tenant` + `state.store.snapshot()`（:`1341` 已在用）三者都在手；加 ctx 字段无额外收益 |
| D4 | **存储两侧同加列**：SQLite 迁移 `0011`；ClickHouse 改 `environment/clickhouse/init.sql`（新实例）**并**提供既有实例的 `ALTER TABLE usage_record ADD COLUMN sub_tenant_id Nullable(String)` | `CREATE TABLE IF NOT EXISTS` 不会给既有表加列；两条路径都必须覆盖 |
| D5 | **读取用 `group_by=sub_tenant` 维度**（`group_expr`/`ch_group_expr` 增 `sub_tenant_id`；handler 白名单 + label + 文档） | 完全向后兼容：旧响应不变；`rows[].key` 本就是这个维度的取值。避免改 `UsageRow`/decoder/响应形状 |
| D6 | **原始 key 不落库**；sink 只存掩码 | design §7.1；归因在内存中从原始 key 派生后仅写 id |

---

## Baseline / Authority Refs

| 权威 | 路径 | 用途 |
|---|---|---|
| 设计 | `design-sub-tenant.md` §8 v3 / §7.1 | 归因时机与禁止事后重建 |
| 用量读取 | `design-tenant-api.md`（usage 端点） | `/usage` schema 与 group_by |
| 纪律 | `dev-plan.md` §1 | TDD、零 mock、真实 CH（`CH_URL`）|

### 已核实事实（v3 recon，HEAD）

- `UsageRecord` 在 **core**：`crates/hydra-core/src/model.rs:359-381`（14 字段，**无 `Default`**）。
- 原始 key：`proxy.rs:651/655` → `ctx.client_api_key`（`ctx.rs:43`）；掩码 `proxy.rs:1296/1302`；记录点 `proxy.rs:1298-1318`；`state.store.snapshot()` 已在 `logging :1341`。
- 私有纯函数：`router.rs:83-92 match_sub_tenant(cfg, tenant_id, api_key) -> Option<&SubTenant>`（enabled + 同租户 + `starts_with`，最长前缀）。
- SQLite INSERT：`sink.rs:395-418`（nullable 镜像 `upstream_host` `:414-415`）；CH INSERT 常量 `sink.rs:595-599`；CH JSON 行 `sink.rs:652-691`（nullable 镜像 `:679-682`）。
- SQLite 迁移：`0001_init.sql:85-99`、`0002`、`0005` 重命名；**下一个空号 = `0011`**。
- ClickHouse DDL：`environment/clickhouse/init.sql:15-31`（仅首启执行）；`clickhouse.rs` 只做传输，无 DDL。
- 既有 CH 实例：容器 `hydra-local-clickhouse`（`docker-compose.local.yml:191`，`127.0.0.1:8123`）；live 测试 `#[ignore]` 需 `CH_URL`。
- 读取 SELECT 均为**显式列**（`usage_query.rs:160-168/204-235/293-302/382-402`），加存储列不破坏它们；分组白名单 `group_expr :172-182` / `ch_group_expr :307-317`；handler 白名单 `tenant_api/handlers.rs:441-456`。
- 字面量构造点（无 Default，必须补字段）：`proxy.rs:1298`、`sink.rs:811-829`、`tests/sqlite_sink.rs:24`、`tests/clickhouse_sink.rs:19`、`crates/hydra-core/tests/entities.rs:171`。
- CH JSON 形状测试：`tests/clickhouse_sink.rs:41-56`（`USAGE_COLUMNS`）与 `:75-80`（分隔符计数）——加列必须同步更新。

---

## 需求就绪检查 / Requirement Ready Check

- [x] 归因时机（路由/记录时）与"禁止事后重建"明确（design §7.1）。
- [x] 派生原语已识别（`match_sub_tenant`，需 pub 包装，D2）。
- [x] 双 sink + 两种 schema 迁移路径已识别（D4）。
- [x] 读取暴露方式（D5，additive）。
- [ ] **待确认**：D5 是否纳入 v3（还是仅"写列"）；本计划建议纳入，否则归因不可见。
- [ ] **待确认**：CH 既有实例的 `ALTER TABLE` 归属（运维手动步骤 vs 迁移脚本）——本计划按"文档化运维步骤 + 更新 init.sql"处理。

---

## 变更必要性 / Change Necessity

- 归因是设计 P3/v3 目标；不写列则无法按子租户统计/限流。
- 不复用掩码 key：掩码不可做前缀匹配（design §7.1）。
- 不新增实体：仅在既有 `UsageRecord` 与两 sink 上加一列 + 一个只读分组维度。

---

## 既有面复用检查 / Existence Check

| 需求 | 既有可复用 | 结论 |
|---|---|---|
| 前缀归因 | `router.rs::match_sub_tenant` | 包一层 pub 返回 id（D2） |
| 记录点 | `proxy.rs::logging` | 就地派生（D3） |
| 双 sink | `sink.rs` INSERT/JSON | 各加一列 |
| 分组读取 | `usage_query.rs` group 机制 | 加 `sub_tenant` 维度（D5） |
| 纯测试 | `tests/sub_tenant.rs` | 新增纯函数测试 |

**不存在**：`UsageRecord.sub_tenant_id`、SQLite 的该列、CH 的该列、`group_by=sub_tenant`。

---

## 架构完整性视角 / Architecture Integrity Lens

1. **Ownership**：`UsageRecord` 仍归 core；派生纯函数归 `router.rs`（core）；sink/读取归 server。
2. **Boundaries**：core 无 I/O；派生在 server 记录点调用 core 纯函数。
3. **Contract changes**：`UsageRecord` 增字段（core 模型）；`/usage` 增一个白名单分组值（additive）；SQLite/CH schema 各加一列。
4. **Cascade**：5 处 `UsageRecord` 字面量 + CH JSON 形状测试；均为机械。
5. **Dependency direction**：不变。
6. **Retirement**：无删除项；不退役掩码 key。
7. **Entropy**：1 字段 + 1 纯函数 + 1 列（两存储）+ 1 分组值——均为设计目标所需。

---

## 兼容性边界 / Compatibility Boundary

- `/usage`：旧请求/响应不变；`group_by=sub_tenant` 为新增值。未知 `group_by` 仍 400。
- 存储：SQLite `ALTER ADD COLUMN`（旧行 `NULL`）；CH 需既有实例 `ALTER`（旧行 `NULL`）。
- 归因独立于 (3.6)/binding：不影响路由行为。
- 停用子租户 ⇒ 后续请求 `NULL`（与 enabled-only 快照一致）。

---

## TDD Route

**Strict。** 纯函数/派生先红后绿；CH 现场测试用 `CH_URL`（本机 `hydra-local-clickhouse`），不可用时 `#[ignore]` 并说明。

---

## 计划压力测试 / Plan Pressure Test

| 拷问 | 回答 |
|---|---|
| 会不会把原始 key 落库？ | 不会：仅写 `sub_tenant_id`；掩码不变。 |
| 会不会改路由行为？ | 不会：只读派生，独立于 (3.6)/binding。 |
| 能否更小？ | 最小 = 写列 + 双 sink；D5 使归因可见，建议纳入。 |
| 最大风险 | CH 既有实例的无回填迁移（D4）；已显式文档化。 |

---

## 计划期复杂度检查 / Plan-Time Complexity Check

- 新增：1 core 字段、1 core 纯函数、1 迁移、2 处 sink 写入、1 分组维度、若干测试。
- 无新 crate、无新依赖；CH 改动仅 init.sql + 文档化 ALTER。
- 复杂度集中在"双存储 schema 同步"，逐任务可验证。

---

## 执行就绪视图 / Execution Readiness View

- 输入齐备（recon + design）。硬阻塞仅 D5 是否纳入需确认；实现按建议纳入并标注。

---

## 文件改动清单

### 新增
- `crates/hydra-server/migrations/0011_usage_sub_tenant_id.sql`（`ALTER TABLE usage_record ADD COLUMN sub_tenant_id TEXT;`）

### 修改
- `crates/hydra-core/src/model.rs` — `UsageRecord.sub_tenant_id: Option<String>`
- `crates/hydra-core/src/router.rs` — `pub fn sub_tenant_id_for_key(...)`（封装 `match_sub_tenant`）
- `crates/hydra-server/src/sink.rs` — SQLite INSERT + CH INSERT + CH JSON 行 + 测试 `rec()`
- `crates/hydra-server/src/proxy.rs` — `logging` 记录点派生并赋值
- `crates/hydra-server/src/usage_query.rs` — `group_expr` / `ch_group_expr` 加 `sub_tenant_id`
- `crates/hydra-server/src/tenant_api/handlers.rs` — `group_by` 白名单 + 错误文案 + label
- `environment/clickhouse/init.sql` — 新列（新实例）
- tests：`hydra-core/tests/{entities,sub_tenant}.rs`、`crates/hydra-server/tests/{sqlite_sink,clickhouse_sink,terminate_mode,usage_query}.rs`
- 文档：`design-sub-tenant.md`（v3 落地）、`tenant-api-integration.md`（usage group_by 表 + 变更策略）、`ops.md`（CH 迁移步骤）、`HANDOFF.md`、`aegis/INDEX.md`

---

## 分步任务（bite-sized）

### T3.1 — core：字段 + 公开派生函数（纯）
- `model.rs`：`pub sub_tenant_id: Option<String>`（置于 `client_api_key_masked` 之后）。
- `router.rs`：`pub fn sub_tenant_id_for_key<'a>(cfg: &'a ConfigData, tenant_id: &str, api_key: &str) -> Option<&'a str>`，委托私有 `match_sub_tenant` 返回 `st.id`。
- 测试：`crates/hydra-core/tests/sub_tenant.rs` 新增：命中返回 id、最长前缀、非本租户不匹配、已停用不匹配、无前缀 ⇒ None；更新 `entities.rs` 的 `UsageRecord` 字面量（round-trip）。

### T3.2 — 存储：SQLite 迁移 + 双 sink + CH init.sql
- 迁移 `0011`；`sink.rs` SQLite INSERT 加列+绑定（镜像 `upstream_host`）、CH INSERT 常量、CH JSON 行（`match Some→json / None→null`）。
- `environment/clickhouse/init.sql` 加列。
- 更新字面量：`sink.rs` 测试 `rec()`、`tests/sqlite_sink.rs:24` 及其本地 `CREATE TABLE :190-201`、`tests/clickhouse_sink.rs:19` 及其 `USAGE_COLUMNS :41-56` 与分隔符计数 `:75-80`。
- 测试：SQLite sink 落库含该列；CH JSON 形状测试通过；`#[ignore]` 现场 CH 写测试（`CH_URL`）可选跑。

### T3.3 — proxy 记录点派生
- `proxy.rs` `logging`：取/复用 `cfg`（`:1341` 的 snapshot），`sub_tenant_id = ctx.client_api_key.as_deref().and_then(|k| router::sub_tenant_id_for_key(&cfg, &tenant.id, k)).map(str::to_string)`，赋给 `record.sub_tenant_id`。
- 测试：`tests/terminate_mode.rs` 用现有 `Vec<UsageRecord>` 记录器：命中子租户前缀的请求记录该 id；operator binding 同时命中 ⇒ 仍按前缀归因；已停用 ⇒ None；无前缀 ⇒ None。

### T3.4 — 读取：`group_by=sub_tenant`
- `usage_query.rs` `group_expr`/`ch_group_expr` 加 `"sub_tenant_id"`（`GroupBy::SubTenant` 或等价）；`handlers.rs` 白名单与错误文案加 `sub_tenant`；`label`。
- 测试：`tests/usage_query.rs` SQLite + CH(wiremock) 分组；handler 白名单接受/拒绝；旧 group_by 不变。
- 文档：`tenant-api-integration.md` usage 的 group_by 表与说明（子租户 id 不是秘密，与 §5.4 一致）。

### T3.5 — 运维与迁移文档
- `ops.md`：CH 既有实例 `ALTER TABLE usage_record ADD COLUMN sub_tenant_id Nullable(String)` 的一次性步骤 + 无回填说明；SQLite 由迁移 `0011` 自动完成。
- `HANDOFF.md`、`design-sub-tenant.md` v3 落地注记、`aegis/INDEX.md`。

### T3.6 — 门禁 + 证据
- 见下。

---

## 验证与门禁

```bash
cargo test -p hydra-core
cargo test -p hydra-server --features server
cargo test -p hydra-server --features usage-clickhouse        # CH sink/usage_query（wiremock 确定性）
# 可选现场 CH（本机 hydra-local-clickhouse）：
CH_URL=http://127.0.0.1:8123 cargo test -p hydra-server --features server,usage-clickhouse -- --ignored
HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380 cargo test -p hydra-server --features server,cluster-redis
cargo build --workspace --features server
cargo fmt --all -- --check
RUSTFLAGS=-D warnings cargo clippy -p hydra-core -p hydra-server --features server --all-targets -- -D warnings
cargo tree -p hydra-core | rg 'tokio|pingora|sqlx|reqwest|hyper'   # 空
```
- [ ] 命中前缀 ⇒ 记录 id；停用/无前缀 ⇒ NULL；归因独立于 (3.6)/binding。
- [ ] SQLite 落库含列；CH JSON 形状含列；两 sink 不串列。
- [ ] `group_by=sub_tenant` 返回以子租户 id 为 key 的分组；旧行为不变。
- [ ] CH 既有实例迁移步骤文档化。
- [ ] 无新增失败（`cluster-redis` 仅 v1 既有 `admin_api` 2 例基线）。

---

## 风险与缓解

| 风险 | 缓解 |
|---|---|
| CH `CREATE IF NOT EXISTS` 不回填既有表 | D4：init.sql + 文档化 `ALTER`；测试用 wiremock，现场 CH 可选 |
| 归因与 (3.6) 语义混淆 | D1/D2：前缀归因独立于闸门；测试覆盖 operator binding 同时命中 |
| 字面量遗漏导致编译失败 | 以编译器驱动补 5 处；CH JSON 形状测试同步 |
| 原始 key 泄漏到存储 | 仅写派生 id；掩码逻辑不动；review 核对 |

---

## 退役（Retirement）

- 不删除任何字段/路径；不退役掩码 key。
- 旧行的 `sub_tenant_id` 为 NULL（无回填）——文档写明。

---

## 待确认

| # | 问题 | 建议 | 阻塞 |
|---|---|---|---|
| D5 | 是否在 v3 暴露 `group_by=sub_tenant` | **纳入**（否则归因不可见） | T3.4 |
| D4 | CH 既有实例迁移归属 | 文档化运维步骤 + 更新 init.sql | T3.5 |

---

## 执行路线 / Execution Route

```
T3.1 (core 字段+纯函数) ──> T3.2 (迁移 + 双 sink)
T3.1 ──> T3.3 (proxy 派生)
T3.2 ──> T3.4 (group_by 读取)
T3.2/T3.3/T3.4 ──> T3.5 (文档) ──> T3.6 (门禁 + 证据)
```

---

## 修订记录

| 日期 | 变更 |
|---|---|
| 2026-09-18 | 初稿：v3 任务拆分 T3.1–T3.6、D1–D6 设计裁定、recon 事实基线；待 oracle 架构复核。 |
| 2026-09-18 | 实现：写路径 T3.1/T3.2/T3.3/T3.5 落地并绿；T3.4 与归因集成测试显式延后。计划/实现后 oracle 复审因 provider 故障未跑（待补）。 |

---

## 实施记录与验收证据（T3.6，2026-09-18）

### 交付状态
v3 **写路径**已实现：`UsageRecord.sub_tenant_id` + 路由时从原始 key 派生 + 双 sink 落库。**独立 oracle 复审未跑**（specialist provider 连续失败）；**T3.4（`group_by=sub_tenant` 读取维度）与 terminate_mode 归因集成测试显式延后**（设计 §8 v3 只要求列 + 双 sink）。

### 任务 → 产物
| 任务 | 产物 | 状态 |
|---|---|---|
| T3.1 | `model.rs::UsageRecord.sub_tenant_id`；`router::sub_tenant_id_for_key`；core 纯测试 `sub_tenant_id_for_key_attributes_by_prefix` | ✅ |
| T3.2 | 迁移 `0011`；SQLite INSERT + ClickHouse INSERT/JSON 行 + `init.sql`；5 处字面量 + CH 形状测试 `USAGE_COLUMNS` | ✅ |
| T3.3 | `proxy.rs::logging` 从原始 key 派生并写入记录 | ✅ |
| T3.4 | `group_by=sub_tenant` 读取维度 | ⏭ 延后（非设计 v3 必需） |
| T3.5 | 文档（design-sub-tenant / ops / HANDOFF / INDEX） | ✅ |
| T3.6 | 门禁 + 证据 | ✅ |

### 门禁证据
```bash
cargo build --workspace --features server                              # ok
cargo test -p hydra-core                                               # 17/17 套件全绿
cargo test -p hydra-server --features server --test sqlite_sink --test terminate_mode
cargo test -p hydra-server --features server,usage-clickhouse --test clickhouse_sink
cargo test -p hydra-server --features server                          # 全量 0 failed
cargo fmt --all -- --check
RUSTFLAGS=-D warnings cargo clippy -p hydra-core -p hydra-server --features server --all-targets -- -D warnings
cargo tree -p hydra-core | rg 'tokio|pingora|sqlx|reqwest|hyper'      # 空
```
> 注：`--features usage-clickhouse` **单独不可编译**（`clickhouse.rs` 需 tokio，既有特性门控事实）；测试用 `server,usage-clickhouse`。

### 延后 / 待办
- **T3.4** `group_by=sub_tenant`（additive 读取维度；旧请求/响应不变）。
- **terminate_mode 归因集成测试**：纯派生已由 core 测试覆盖；proxy 记录点接线（4 行）尚无端到端断言。
- **独立 oracle 复审**（计划阶段与实现后）因 provider 故障未跑；provider 恢复后补跑，且不得以自审替代。
- **ClickHouse 既有实例迁移**：一次性 `ALTER TABLE usage_record ADD COLUMN IF NOT EXISTS sub_tenant_id Nullable(String)`（无回填）；新实例由 `init.sql` 覆盖；SQLite 由迁移 `0011` 自动完成。
