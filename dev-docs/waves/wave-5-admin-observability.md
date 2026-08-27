# Wave 5 — 管理服务与可观测性（Admin & Observability）

> crate：`hydra-server` ｜ 估时：2d ｜ 串行（依赖 W1–W4）
>
> 关键纪律：AdminService 是真实 HTTP 服务（Pingora `ServeHttp`），测试用**真实 HTTP 请求**打它 + **`:memory:` 真 SQLite**。指标用 `prometheus` crate 自注册 registry 自托管 `/metrics`（design §1.1 已弃用 `pingora-prometheus`）。

---

## 1. 目标与范围

### In-scope
- `admin::AdminService`（Pingora `Service` + `ServeHttp`），独立端口；
- 轻量路由分发（不引入 axum，避免双 runtime）；
- REST CRUD：`/api/v1/{providers,provider-models,provider-keys,tenants,tenant-providers,tenant-models,limit-roles}` 全资源；
- `DELETE /api/v1/auth/cache`（认证缓存失效，接 W3）；
- `GET/DELETE /api/v1/breaker[/]`（熔断 dead-set 查看 / 手动复位）；
- `GET /api/v1/health`、`POST /api/v1/reload`；
- 写后 `ConfigStore::reload_all()`（联动 SWRR 清空，W2 已实现）；
- `admin::metrics`：自托管 `/metrics`，注册 §17 全部指标；
- Proxy 侧把指标埋点接入（counter/histogram 记录点）。

### Out-of-scope
- 内嵌 UI（W6）；
- 优雅升级验证（W6）；
- Playwright E2E（W6）。

### 依赖与前置
- W1：纯类型（实体用于 JSON 序列化）。
- W2：`db::repo`、`ConfigStore::reload_all`。
- W3：`AuthChecker::invalidate*`、`UsageSink`（用量计数可观测）。
- W4：`CircuitBreaker`（dead-set 读写）、`HydraProxy`（埋点回调）。

---

## 2. TDD 任务列表

### 2.1 AdminService 骨架与鉴权（0.3d）—— design §13.1/§13.3
- T1.1 `admin_requires_token`：无/错 `Authorization` → 401。
- T1.2 `admin_token_from_env`：`HYDRA_ADMIN_TOKEN` 注入后通过。
- T1.3 `admin_unknown_path_404`：未匹配路由 → 404 JSON。
- T1.4 `admin_binds_isolated_port`：AdminService 与 ProxyService 不同端口（装配断言）。

### 2.2 REST CRUD（0.7d）—— design §13.2
> 每资源一组：create/list/get/update/delete，全用真实 HTTP（reqwest 打 AdminService）+ `:memory:` SQLite。
- T2.1 `provider_crud_http`：POST→201+body；GET list→200；GET id；PUT→200；DELETE→204；重复 key→409。
- T2.2 `provider_model_crud_http`：含 status 校验；FK 不存在 provider→400。
- T2.3 `provider_key_crud_http`：默认列表**掩码**返回；`?reveal=1` 返回原文。
- T2.4 `tenant_crud_http`：`auth_url` 必填→插空 400；domain 唯一冲突 409。
- T2.5 `tenant_provider_crud_http`：UNIQUE 冲突 409。
- T2.6 `tenant_model_crud_http`：UNIQUE 冲突 409。
- T2.7 `limit_role_crud_http`：window CHECK；count/token 同 NULL→400（或告警）。
- T2.8 `write_triggers_reload_all`：任一 POST/PUT/DELETE 后，`ConfigStore.snapshot()` 反映新值（通过随后 proxy 请求行为或 `/health` 快照断言）。
- T2.9 `reload_returns_latest_snapshot`：写后响应体含最新该资源快照。

### 2.3 认证缓存失效（0.3d）—— design §11.7/§13.2
- T3.1 `auth_cache_invalidate_by_keys`：`DELETE /api/v1/auth/cache {api_keys:[...]}` → 返回 `invalidated:n`，对应缓存被删（随后 proxy 请求触发回源，用 mock auth server `expect` 验证）。
- T3.2 `auth_cache_invalidate_by_tenant`：`{tenant_id}` 清空该租户全部。
- T3.3 `auth_cache_invalidate_unknown_returns_zero`：不存在的 key → 0，不报错。

### 2.4 熔断查看/复位（0.2d）
- T4.1 `breaker_list_dead`：`GET /api/v1/breaker` 返回当前 dead-set。
- T4.2 `breaker_reset_provider`：`DELETE /api/v1/breaker/:id` 手动复位某 provider → 随后候选重新包含它。

### 2.5 健康与重载（0.2d）
- T5.1 `health_returns_ok`：`GET /api/v1/health` → 200 `{status:ok,...}`（含 DB/pool 状态）。
- T5.2 `reload_endpoint_triggers_reload_all`：`POST /api/v1/reload` → 返回 200 + 新快照；校验失败→400 保留旧。

### 2.6 指标自托管与埋点（0.3d）—— design §17
- T6.1 `metrics_endpoint_exposes_default_registry`：`GET /metrics` → 200，文本含 `# HELP`/`# TYPE`。
- T6.2 `metric_requests_total_incremented`：发一个 proxy 请求后 `/metrics` 中 `hydra_requests_total` 增长（按 tenant/provider/model/status 标签）。
- T6.3 `metric_auth_decisions`：认证 allow/deny/hit/miss 标签正确累加。
- T6.4 `metric_breaker_dead`：熔断后 `hydra_breaker_dead{provider=...}` 为 1。
- T6.5 `metric_tokens_total`：带 usage 的请求后 token 计数增长。
- T6.6 `metric_histogram_latency`：`hydra_request_duration_seconds` 有 bucket 样本。

---

## 3. 外部边界与测试方式
- **无新增外部边界**。Admin 测试 = 真实 AdminService HTTP + `:memory:` SQLite + 真实 ConfigStore/AuthCache/Breaker 装配。
- 认证失效测试需联动 mock auth server（wiremock，W3 已有）验证「失效后回源」。
- 指标测试读 `/metrics` 文本断言，不 mock prometheus。

---

## 4. 与 design.md 的映射
§13（Admin 全节）、§11.7（失效）、§8.4（breaker 复位）、§17（指标目录）、§15.1 配置。

---

## 5. 出口准则
- [ ] `cargo test -p hydra-server` Admin 套件全绿；
- [ ] §13.2 全部端点有 HTTP 级测试；
- [ ] §17 全部指标有埋点 + 至少一项增长断言；
- [ ] 写操作一律触发 `reload_all` 并返回最新快照；
- [ ] Admin 端口鉴权（Bearer Token）强制；
- [ ] 无内部 mock；AdminService 为真实 Pingora `ServeHttp` 服务。

---

## 6. 风险与注意
- **不引入 axum**：自写轻量路由匹配（方法+路径前缀+id）；保持单 Tokio runtime（design §13.1）。
- **指标 label 基数**：tenant/provider/model 组合可能膨胀；model 限定为配置内已知值，避免任意 path 进 label。
- **`?reveal=1` 安全**：明文 key 接口需 Admin Token + 审计日志（记录访问）。
- **reload 并发**：并发写触发多次 `reload_all` 可能竞争；用 `tokio::sync::Mutex` 串行化 reload 调用（最后一次为准），测试覆盖并发写场景。
