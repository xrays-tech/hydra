# Wave 3 — 外部边界适配器（External Boundary Adapters）

> crate：`hydra-server` ｜ 估时：2d ｜ 与 W2 可并行（不同模块，无写冲突）
>
> 关键纪律：**外部边界是唯一允许「double」的地方**。`AuthChecker`/`UsageSink` 是面向**真实第三方服务**的 trait；测试用 **wiremock（真实 HTTP server）** 与 **in-memory SQLite**，**绝不 mock 内部逻辑**。

---

## 1. 目标与范围

### In-scope
- `http::AuthChecker` trait + `HttpAuthChecker`（reqwest 异步回源 `tenant.auth_url`）；
- `http::auth_cache::AuthCache`（`DashMap<(tenant_id, sha256(key)), AuthEntry>` + TTL + GC）——把 W1 的纯 `cache_decision`/`apply_upstream` 装配成可并发结构；
- `sink::UsageSink` trait + `SqliteSink`（channel 批量写 `usage_record`）+ `ClickHouseSink`（feature flag）；
- 强制失效入口 `invalidate(keys)` / `invalidate_tenant(tenant)`。

### Out-of-scope
- 把边界接到 Pingora 请求生命周期（W4）；
- Admin HTTP 暴露失效接口（W5）；
- 任何路由/解析逻辑（已在 W1）。

### 依赖与前置
- W1：`auth::cache_decision`、`auth::apply_upstream`、`auth::AuthVerdict`（携带状态码）、`UsageRecord`、`ProviderKind`、`sha256` 工具。
- W2：`SqlitePool`（SqliteSink 用）、`ConfigStore`（持有 tenant.auth_url 与 TTL 配置）。

---

## 2. TDD 任务列表

### 2.1 AuthCache 并发装配（0.3d）—— 把 W1 纯逻辑包成并发结构
- T1.1 `cache_decision_delegates_to_pure`：构造 `AuthCache`，并发插入条目，`decision()` 调用 W1 纯函数，结果一致（命中/过期语义复用 W1，本波只测并发外壳）。
- T1.2 `cache_set_and_invalidate_keys`：`invalidate(["k1","k2"])` 删除命中项，返回实际删除数；不存在项忽略。
- T1.3 `cache_invalidate_tenant`：按 tenant 清空其全部条目。
- T1.4 `cache_gc_evicts_expired`：注入过期条目，跑 GC 后被清。
- T1.5 `cache_key_is_sha256_not_plaintext`：断言 DashMap key 为 sha256，内存无明文（grep/反射不可行 → 通过 `Debug` 输出不含明文验证）。

### 2.2 HttpAuthChecker（reqwest 边界）（0.7d）—— design §11.2/§11.3
> 全部用 **wiremock** 起真实 HTTP server 模拟租户 auth 服务。

- T2.1 `auth_upstream_2xx_caches_allowed`：wiremock 返回 200 → `check()` 返回 `Allowed{Miss}`，且 AuthCache 写入 `allowed=true, allow_ttl`；二次调用命中缓存（不再打 wiremock，用 `Mock::expect(1)`）。
- T2.2 `auth_upstream_401_caches_denied`：401 → `Denied{status:401, Miss}`，写 `allowed=false, deny_ttl`；二次命中。
- T2.3 `auth_upstream_403_denied_no_200_cache`：403 同 401 处理。
- T2.4 `auth_upstream_5xx_fail_closed_no_cache`：500 → `Denied{status:503, Local}`，**不写缓存**；`fail_mode=closed`。
- T2.5 `auth_upstream_timeout_fail_closed`：wiremock delay > `timeout_ms` → 503，不缓存。
- T2.6 `auth_fail_open_config`：`fail_mode=open` + 500 → `Allowed{Local}`（放行不缓存）。
- T2.7 `auth_request_contract`：捕获 wiremock 收到的请求，断言：`POST`、`Authorization: Bearer <key>`、`X-Hydra-Tenant`、`X-Hydra-Trace-Id`、JSON body 含 `api_key/tenant_id`（design §11.3）。
- T2.8 `auth_response_expires_in_override`：返回体 `{"allowed":true,"expires_in":60}` → 缓存 TTL 用 60 而非全局默认。
- T2.9 `auth_independent_client_pool`：`HttpAuthChecker` 用独立 reqwest client，与上游通道隔离（通过 client 配置/连接池参数断言）。
- T2.10 `auth_concurrent_same_key_single_flight_optional`：（可选优化）并发同 key 首次请求避免重复回源——若实现则测，否则文档标注 v2。

### 2.3 SqliteSink（0.5d）—— design §9.2
- T3.1 `sink_records_to_usage_table`：`record(UsageRecord)` ×N → 后台 flush 后 `usage_record` 表有对应行。
- T3.2 `sink_batches_by_size`：发 `batch_size` 条 → 立即一次批量 INSERT。
- T3.3 `sink_batches_by_time`：不足 batch_size 但超 `flush_secs` → 仍 flush。
- T3.4 `sink_backoff_on_db_error`：DB 临时不可用 → 重试 + 指数退避，恢复后写入；期间不阻塞调用方（`record` 立即返回）。
- T3.5 `sink_mask_key_stored`：存入的 `client_api_key` 字段为脱敏值（由 W1 `mask_key` 生成，sink 仅搬运）。
- T3.6 `sink_drop_drains`：Drop 时 flush 剩余（优雅关闭）。

### 2.4 ClickHouseSink（feature flag）（0.3d）
- T4.1 `clickhouse_sink_writes_batch`：用 `clickhouse` testcontainer（或 CI 提供 CH 实例）验证批量 INSERT；**无 CH 环境时 `#[ignore]`，文档注明手动跑**。
- T4.2 `clickhouse_schema_matches`：表结构与 `usage_record` 字段对齐。
- T4.3 `sink_trait_swap_by_config`：`sink=sqlite`/`sink=clickhouse` 启动装配出对应实现，trait 对外统一。

### 2.5 失效与 fail_mode 装配（0.2d）
- T5.1 `invalidate_forces_reauth`：先 `check` 缓存 allowed → `invalidate` → 再 `check` → wiremock `expect(2)`（确认回源）。
- T5.2 `config_drives_ttls`：`[auth] allow_ttl_secs/deny_ttl_secs/timeout_ms/fail_mode` 解析后正确注入。

---

## 3. 外部边界与测试方式

| 边界 | 测试 double | 性质 |
| --- | --- | --- |
| 租户 `auth_url`（HTTP） | **wiremock** 真实 HTTP server | ✅ 外部第三方服务的网络层 double |
| SQLite（SqliteSink） | `:memory:` 真实引擎 | ✅ 真实 DB |
| ClickHouse | testcontainer 真实 CH（或 `#[ignore]` 手动） | ✅ 真实 DB |

**禁止**：在 `HttpAuthChecker` 内部 mock「缓存判定逻辑」——判定逻辑是 W1 纯函数，直接调用；本波只测「reqwest 回源 + 缓存回填装配」。若发现需要 mock 内部判定，说明抽象泄漏，应重构而非 mock。

---

## 4. 与 design.md 的映射
§9.1-§9.3（UsageSink）、§11.2-§11.6（认证全链路）、§15.1 `[auth]`/`[usage]` 配置。

---

## 5. 出口准则
- [ ] `cargo test -p hydra-server --features usage-clickhouse`（或默认 sqlite）全绿（wiremock 随测随起）；feature 名以 design §1.1 为准（`usage-clickhouse`），无未声明的 `auth`/`usage` feature；
- [ ] `HttpAuthChecker`/`UsageSink` 为 trait，生产装配真实实现，测试装配真实 double；
- [ ] 无 `mockall`/手写内部 mock；wiremock 仅模拟外部 HTTP 服务；
- [ ] AuthCache 内存 key 为 sha256，无明文；
- [ ] `ClickHouseSink` feature 隔离，默认编译不含 CH 依赖。

---

## 6. 风险与注意
- **wiremock 与 tokio**：wiremock 是 async test helper，仅在 `#[tokio::test]` 测试中用，不进生产依赖树（dev-dependency）。
- **single-flight 缓存击穿**：高并发失效后首请求可能重复回源；v1 接受，记录为 v2 优化（T2.10 可选）。
- **ClickHouse 测试稳定性**：CI 无 CH 时全部 `#[ignore]`，避免误红；提供 `just test-clickhouse` 手动入口。
- **reqwest blocking 防护**：`Cargo.toml` 排除 `reqwest` 的 `blocking` feature（design §1.1），防额外 runtime panic。
