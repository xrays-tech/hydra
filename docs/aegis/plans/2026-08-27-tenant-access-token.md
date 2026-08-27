# 实施计划：租户 Access Token → 自助清除本租户认证缓存（欠费停机 / 付费恢复）

- 日期：2026-08-27
- 状态：待审核（审核通过后启动开发）
- 作者：编码智能体（DeepSeek）
- 对应需求：为 Tenant 增加 AccessToken 配置；租户凭该令牌通过 HTTP REST 端点清除自己名下 api-key 的认证缓存，强制重新认证（欠费停机、付费恢复访问等场景）

## Goal

1. Tenant 增加 access_token 配置（admin REST + admin-UI 可设置/轮换，永不回显）；
2. 新增租户自助端点 DELETE /api/v1/tenants/auth/cache：凭 Authorization: Bearer <access_token> 清除**该租户自己**的 auth 缓存（与管理员 DELETE /api/v1/auth/cache 语义一致，但租户身份由令牌决定，客户端无法越权清别人）；
3. 集群模式下沿用现有 P4 invalidation stream（全节点广播）+ standby 转发语义；
4. 全套测试 + 文档（design §11.7/§13.2、API Docs 页、ops runbook）。

## 设计决策

| 决策点 | 方案 | 理由 |
|---|---|---|
| 端点 | `POST /api/v1/tenants/{tenant_id}/auth/cache/invalidate`：URL 带 tenant_id，body 带 api-key 列表（可选，缺省/空 = 清该租户全部），Bearer 带令牌 | 已确认：URL 显式 tenant_id（租户自知身份），body 可选精确失效指定 key（对齐管理端 auth/cache 语义）；POST 带 body 最自然 |
| 鉴权 | 独立于 admin token 的租户令牌闸门；admin token **不能**调此端点（权限分离）；未配置令牌的租户一律 401（fail-closed） | 租户自助能力必须与运维能力隔离 |
| 身份来源 | 服务端由令牌反查租户 id（sha256 比对），并校验 == URL 的 tenant_id，不一致 → 403（fail-closed） | URL 的 tenant_id 是自述身份，令牌是凭证；两者必须一致才放行 → 防越权 |
| 令牌存储 | tenant.access_token_hash TEXT（SHA-256 十六进制），明文不落库、响应不回显、不可逆（丢失即轮换） | 与 AuthCache 对 api-key 的 sha256 做法一致；令牌只用于比对、从不用于出站调用，单向哈希是正确原语；不引入新依赖（constant-time 比较手写 ~10 行） |
| 更新语义 | create：非空→设置；空→无令牌。update：null/缺省→保留现值；非空→轮换；显式 ""→清除（同 cert_pem 的 clear 约定） | 与现有 cert 字段语义一致；admin-UI blank=keep |
| 集群 | 端点注册在 mutation 路径：standby 走现有 lease-holder 转发；执行节点沿用现有 P4 广播（invalidation stream）清全集群 L1/L2 | 与管理员 auth-cache 端点行为一致，零新机制 |
| edge 节点 | 不提供（无 DB 无 token 表）→ 404 | 与 edge 只提供探活端点的既有约定一致 |
| 频率控制 | 不自带限流：端点只能清自己的缓存，滥用只伤害该租户自己（其 auth_url 流量） | 保持最小面；如需可在后续加 per-tenant 限流 |
| 令牌生命周期 | 无 TTL（已确认）；仅手动轮换（改值即换）；UI 提供「生成随机令牌」按钮（已确认） | 简单、够用 |

## 端点契约

```text
POST /api/v1/tenants/{tenant_id}/auth/cache/invalidate
Authorization: Bearer <tenant-access-token>          # 租户令牌（非 admin token）
Body: {"api_keys": ["sk-aaa", ...]}                 # 可选；缺省/空 = 清该租户全部
→ 200 {"invalidated":N,"tenant_id":"<id>"}
→ 401 令牌缺失/未知/该租户未配置令牌（fail-closed）
→ 403 令牌归属租户 != URL 的 tenant_id（越权防护）
→ 404 edge 节点（无 admin API）/ 租户不存在
```

## 文件改动清单

### 新增
- `crates/hydra-server/migrations/0009_tenant_access_token.sql` — ALTER TABLE tenant ADD COLUMN access_token_hash TEXT;（可空，向后兼容）
- `crates/hydra-server/tests/tenant_cache.rs` — 端点集成测试（含生成按钮对应 API 语义的断言）
- `docs/aegis/plans/2026-08-27-tenant-access-token.md` — 本计划

### 修改
- `crates/hydra-server/src/db.rs` — 新增 set_tenant_access_token（写哈希/NULL）+ list_tenant_access_token_hashes（读 (tenant_id, hash) 供比对）+ tenant_has_access_token（供响应视图）
- `crates/hydra-server/src/admin/handlers.rs` — TenantUpsert 加 access_token: Option<String>；create/update 写哈希；新增 TenantView（flatten Tenant + has_access_token: bool）供 list/get 响应；新增 tenant_auth_cache_invalidate handler
- `crates/hydra-server/src/admin/mod.rs` — 注册 DELETE /api/v1/tenants/auth/cache（在 admin-token 闸门前走租户令牌闸门）；抽出 maybe_forward_mutation 复用 standby 转发逻辑
- `.sqlx/` — 重新 cargo sqlx prepare 生成新 query 的离线缓存（CI SQLX_OFFLINE=true）
- `admin-ui/app.js` — tenants fields 加 access_token（type password, map opt, blank=keep）；columns 加 Token 列（set/—）
- `admin-ui/api-docs.js` — 新端点文档条目（租户令牌鉴权说明）
- `docs/design.md`（§11.7 / §13.2 端点表）、`docs/ops.md`（新增节：租户自助失效）

## 分步任务（bite-sized）

### Task 1：迁移 + db 层
- 建 0009_tenant_access_token.sql；在 db.rs 加三个函数（sqlx::query!，沿用 cert 模式：None→NULL）。
- 验证：cargo sqlx prepare --workspace（本地 SQLite 应用全部迁移后生成 .sqlx 缓存）；SQLX_OFFLINE=true cargo test -p hydra-server --test migrate --features server。

### Task 2：handler 层（写路径）
- TenantUpsert 加字段；create：access_token 非空 → set_tenant_access_token(Some(sha256_hex))，空/缺省跳过；update：非空→轮换，Some("")→清除，None→保留；TenantView 包装 list/get 响应（flatten + has_access_token）。
- 复用 hydra_core::auth::sha256_hex；新增 constant_time_eq 辅助（长度校验 + XOR 累加）。
- 验证：cargo test -p hydra-server --lib --features server（含 db/handler 单测）。

### Task 3：路由 + 租户令牌闸门
- mod.rs：抽出 maybe_forward_mutation(...) -> Option<Resp>（把现有 leader 转发块搬进方法）；新增 tenant_from_bearer(session) -> Option<String>（sha256 比对，constant-time）；在 admin-token 闸门**之前**插入：命中 DELETE /api/v1/tenants/auth/cache → 租户令牌校验（失败 401）→ maybe_forward_mutation（standby 转发）→ tenant_auth_cache_invalidate。
- 验证：cargo test -p hydra-server --test tenant_cache --features server。

### Task 4：租户端点 handler
- tenant_auth_cache_invalidate(state, tenant_id, trace_id)：state.auth.invalidate_tenant(tenant_id) + 刷新 auth_cache_size 指标 + 集群 P4 广播（沿用 auth_cache_invalidate 的 publish 片段）；返回 {"invalidated":N,"tenant_id"}。

### Task 5：前端 + API 文档
- app.js：tenants fields 加 { name: "access_token", label: "Access token", type: "password", map: "opt", full: true, placeholder: "blank = keep current", tip: "租户自助令牌：DELETE /api/v1/tenants/auth/cache 清除本租户认证缓存；编辑留空=保留，改值=轮换" }；columns 加 { key: "has_access_token", label: "Token", render: (v) => v ? "set" : "—" }。
- api-docs.js：新端点条目（auth: "tenant token"，注明非 admin token）。
- 验证：无头 Chrome 本地起服务 → Tenants 表单出现 Token 字段；API Docs 页渲染正常。

### Task 6：集成测试 + 文档 + 全量回归
- tests/tenant_cache.rs（wiremock 无上游需求）：创建带令牌租户→admin API 断言响应含 has_access_token:true 且**不含** token/hash 字段；预置一条 auth 缓存（wiremock 认证流）→ 租户令牌调端点 → 200 + invalidated:1；错误/缺失令牌 → 401；admin token 调此端点 → 401；未配置令牌租户 → 401。
- 文档：design §11.7/§13.2、ops.md、API Docs。
- 全量：SQLX_OFFLINE=true cargo test --features server --workspace + cargo clippy --features server --workspace --all-targets -- -D warnings。

## 兼容性 & 风险

- **向后兼容**：新列可空；响应仅新增字段；端点全新无行为变更；租户模型本身不加字段（哈希在 DB 边界管理，同 cert 私钥模式）。
- **安全**：令牌不可逆存储、constant-time 比对、fail-closed；管理员在 UI 设置后只能轮换不能查看（与 provider api-key 的"永不出现在响应"一致）。
- **风险/未知**：租户忘记令牌 → 只能管理员轮换（文档写明）；集群 standby 转发依赖既有 lease 机制（已测试路径）；.sqlx 缓存必须随新 query 提交，否则 CI 挂。
- **Retirement**：无旧路径退役；纯增量功能。

## 待确认

## 已确认（2026-08-27）

1. 端点：`POST /api/v1/tenants/{tenant_id}/auth/cache/invalidate`，URL 传 tenant_id、body 传 api-key 列表（可选）。
2. admin-UI 提供「生成随机令牌」按钮。
3. 无 TTL，仅手动轮换。

## 待确认

（无 —— 设计已定稿，可启动开发）
