# 实施计划：T10 遗留清理（解耦 → 删除 auth cache / sub-tenant / auth_url）

- **日期**：2026-11-04
- **状态**：草案（待实施）
- **前置**：`2026-11-03-user-hierarchy-global-role.md` 的 T1–T9/T11 已落地，全量测试 974 passed / 0 failed；T10 因架构耦合被回滚。
- **目标**：删除新体系已不再使用的旧结构，使“Global Role = 唯一调度界面”“内部鉴权 = 唯一 API-Key 鉴权”在代码层也成立。
- **硬约束**：
  1. **禁止**触碰 `tools/`（Go/Python/TS SDK）、`go.work`、`integration/test_sdk_*`、`integration/mock_llm.py`。
  2. 每一步必须 `cargo check --all-targets` + 全量测试绿；失败即回滚该步。
  3. 删除清单外的文件不得改动；需要越界时先停下报告。
  4. 不做数据迁移；绿地清库。`0015` 只在新代码完全不再引用旧结构后落地。

---

## 1. 为什么上一轮失败

| 失败点 | 事实 | 结论 |
|---|---|---|
| 第一次 T10a1 | 代理自由删除，波及 SDK/tools/`go.work` | 必须白名单 + 每步验证 |
| 第二次 T10a1 | `cluster/events.rs` 直接依赖 `http.rs` 的 `AuthCache`/`HttpAuthChecker`；`tenant_api_cluster.rs` 又依赖 `events::InvalidationStream` | **先解耦 events，再删 auth cache**，不能并行删 |

`cluster/events.rs` 不是纯 auth cache：它承载 Redis Streams 的 generation/trim/consumer 语义。保留事件流、移除“事件→auth cache 应用”这一段，是正确拆分。

---

## 2. 阶段 A：解耦 `cluster/events.rs` 与 auth cache

### A1. 事件流纯化
- 在 `cluster/events.rs` 中删除对 `crate::http::{AuthCache, HttpAuthChecker, AuthConfig}` 的字段/参数/构造依赖。
- 保留：`InvalidationStream`、`Invalidation`、`FleetReport`、generation/trim/applied-key 语义、Redis Streams 读写。
- 将“收到事件后清 L1/L2 auth cache”的逻辑移出（直接删除；新鉴权已无 cache）。
- `spawn_invalidation_consumer` 保留为纯消费者：记录 applied index/generation，不再调用 cache。

### A2. 测试拆分
- `tests/invalidation_trim.rs` 保留，删除 auth-cache 断言，只测 stream/trim/generation。
- `tests/tenant_api_cluster.rs`：保留集群收敛测试；把“跨节点 auth cache 失效”断言替换为“事件 applied watermark 收敛”；若该文件只测 auth cache，则删除（需在提交信息中说明）。
- 新增/保留守卫：`publish -> consumer applied` 的 watermark 单调性。

### A3. 验收
- `cargo test --features "server,cluster-redis,arachne" --test invalidation_trim --test tenant_api_cluster`
- `cargo check --all-targets --features "server,cluster-redis,arachne"`
- 此时 `events.rs` 不再引用 `http::Auth*`，`http.rs` 的 auth 类型才可以进入阶段 B。

---

## 3. 阶段 B：删除外部鉴权

### B1. 删除清单（精确）
- 文件：`crates/hydra-server/src/redis/auth_cache.rs`
- 测试：`tests/auth_cache.rs`、`tests/http_auth.rs`、`tests/admin_auth_test.rs`
- 集成：`integration/test_auth_cache_layers.py`、`integration/test_auth_hop_failmode.py`、`integration/mock_auth.py`

### B2. 编辑清单
- `crates/hydra-server/src/http.rs`：删除 `AuthChecker`、`HttpAuthChecker`、`AuthCache`、`AuthConfig`、`auth_request_body`、auth-cache 指标；保留其余 HTTP 工具。
- `crates/hydra-server/src/redis/mod.rs`：删除 `auth_cache` 声明。
- `AppState.auth` 字段与所有构造点（`main.rs`、`proxy.rs`、tests）：删除 auth 参数。
- Admin：删除 `DELETE /api/v1/auth/cache`、`POST /api/v1/tenants/auth/test`、auth-cache 指标；保留 `AdminState.invalidation`/events。
- Tenant API：删除 `Endpoint::InvalidateAuthCache`/handler、`invalidate_per_min`；保留通用 throttle 与其他端点。
- `hydra-core/src/auth.rs`：删除 `AuthVerdict`/`CacheSource`/`Verdict`/`AuthEntry`/`CacheOp`/`cache_decision`/`apply_upstream`/`decide`/`denial_status_for_reason`/`REASON_INSUFFICIENT_BALANCE`（仅在无引用时）；保留 `sha256_hex`/`sha256_hex_string`/`hmac`。
- 环境变量：删除 `HYDRA_AUTH_*`、`HYDRA_TENANT_API_INVALIDATE_PER_MIN`，同步 `check_documented_env.cjs`/`check_cluster_env.cjs`。

### B3. 验收
- `rg "AuthCache|HttpAuthChecker|AuthConfig|auth/cache/invalidate|tenants/auth/test" crates/ integration/ admin-ui/ scripts/` 仅允许历史文档命中。
- 全量测试绿。

---

## 4. 阶段 C：删除 sub-tenant / key-prefix binding

### C1. 代码删除
- 模型：`SubTenant`、`SubTenantRoute`、`ProviderKeyBinding`。
- `ConfigData`：`sub_tenants`、`sub_tenant_routes`、`key_prefix_bindings`。
- Router：`match_key_binding`、`match_sub_tenant`、`match_sub_tenant_route`、`sub_tenant_id_for_key`、`select_sub_tenant_route` 及管线步骤 3.5/3.6。
- Proxy：`passthrough_candidates` 中的 binding/sub-tenant 分支。
- 租户 API：`Endpoint::{ListSubTenants,ListSubTenantRoutes}`、`TenantWriteRoute::{UpsertSubTenant,DeleteSubTenant,UpsertRoute,DeleteRoute}` 及 handlers。
- Admin：sub-tenant/sub-tenant-route/provider-key-binding 的 handlers/routes/UI/i18n。
- 控制面：`EntityPath::{SubTenant,SubTenantRoute,KeyBinding}`、`FidelityRows`/`FidelityTreeEntity` 对应字段、`db/restore.rs` 插入分支、`TOC_FORMAT` 说明。
- DB：`list_sub_tenants*`/`list_sub_tenant_routes*`/`list_key_bindings*`/CRUD；`admin/sub_tenant_write.rs`；`hydra-core/src/sub_tenant.rs`。
- 测试：`tests/sub_tenant_admin.rs`、`tests/sub_tenant_data_plane_write.rs`、`hydra-core/tests/sub_tenant.rs`、`integration/test_sub_tenant_steering.py`、`integration/test_tenant_api_contract.py` 中对应段。

### C2. 迁移 `0015_drop_legacy.sql`
- `DROP TABLE provider_key_binding;`
- `DROP TABLE sub_tenant_route;`
- `DROP TABLE sub_tenant;`
- `ALTER TABLE tenant DROP COLUMN access_token_hash;`
- `ALTER TABLE tenant DROP COLUMN auth_url;`
- 前提：代码已无引用；SQLite ≥3.35；绿地清库。

### C3. 验收
- `rg "sub_tenant|SubTenant|provider_key_binding|key_prefix_binding|KeyBinding"` 仅历史文档/迁移历史命中。
- 全量测试绿；`TOC_FORMAT=5` 新树可发布/物化/restore。

---

## 5. 阶段 D：删除 `auth_url` / `access_token_hash`

- `Tenant.auth_url`、`Tenant::has_access_token` 字段/响应；admin tenant PUT 的 `access_token` 字段、`apply_tenant_access_token_write`、`validate_access_token_shape`；`whoami.auth_url`。
- `db` 查询/DTO/测试中的 `auth_url`、`access_token_hash`。
- `0015` 的 DROP COLUMN 与 C 阶段同批。
- 验收：`rg "auth_url|access_token_hash"` 仅历史文档/迁移历史命中。

---

## 6. 执行顺序与门禁

| 步 | 内容 | 门禁 |
|---|---|---|
| A | events 解耦 | 全量测试 + clippy + fmt |
| B | auth cache 删除 | 全量测试 + `rg` 销账 |
| C | sub-tenant/binding 删除 + `0015` | 全量测试 + `rg` 销账 + migrate 测试 |
| D | auth_url/access_token_hash 删除（并入 `0015`） | 全量测试 + `rg` 销账 |
| E | 文档/ADR/INDEX | `check_documented_env` / `check_ci_wiring` / `check_tenant_error_codes` |

每步独立 commit，禁止把 A–D 混成一个无法回滚的大提交。

---

## 7. 风险与回滚

| 风险 | 缓解 |
|---|---|
| `events.rs` 解耦破坏集群失效语义 | 先只删 cache 应用，保留 stream/watermark；`tenant_api_cluster` 改测 watermark |
| 删除 sub-tenant 破坏用量归因 | 用量已迁到 user/group/api_key；确认无 `sub_tenant_id` 残留 |
| 旧测试大量删除导致覆盖下降 | 只删“仅测旧行为”的断言；Global Role 并集/回退、内部鉴权、fleet 收敛必须有替代测试 |
| 代理越界删除 | 每一步给出精确文件白名单；`tools/`/SDK/`go.work` 明确禁止 |
| 迁移 0015 与代码不同步 | 先删代码+测试，再落 0015；同批 commit |

**回滚**：每步前 `git stash create` + untracked tar；失败 `git checkout <sha> -- .` 恢复。T10 不应触碰当前绿色工作区以外的功能。
