# Wave 1 — 纯领域核心（Pure Domain Core）

> crate：`hydra-core` ｜ 依赖：**无 I/O**（禁 `tokio`/`pingora`/`sqlx`/`reqwest`/`hyper`；`memchr`/`bytes`/`sha2` 为纯计算/引用计数/密码库，**允许且必需**用于零拷贝提取与 api-key 哈希）｜ 估时：4d
>
> 这是整套系统的地基。**全部为纯函数**，可在无网络、无文件、无运行时下穷尽测试，**零 mock**。

---

## 1. 目标与范围

### In-scope
- 建立 Cargo workspace 与 `hydra-core` crate（强制零 I/O 依赖）；
- 全部领域实体（纯数据结构）；
- 全部「内部逻辑」以纯函数实现并穷尽单测：
  - 路由候选计算（`TenantModel` 闸门 + 交集 + 权重/熔断过滤）
  - 加权 Round Robin（SWRR）
  - 熔断状态机（事件 → dead-set 纯转移）
  - usage 零拷贝 `memchr` 扫描解析（OpenAI / Anthropic / 通用）
  - `"model"` 零拷贝 `memchr` 提取（返回 `&[u8]` 借用，不解完整 JSON）
  - 限流匹配 + 滑动窗口计数
  - 认证缓存命中/过期判定
  - `/v1` 路径重写、api-key 脱敏
  - 配置加载期校验（纯：输入 row 集合 → 校验结果）

### Out-of-scope
- 任何 DB 读写、HTTP 调用、Pingora 集成（留给 W2/W3/W4）；
- 任何 `async`（core 全同步纯函数；异步仅在 server 外壳）。

### 依赖与前置
- 无（W1 是第一波）。唯一输入是 `design.md`。

---

## 2. 纯函数清单（= 本波次全部交付物）

| 模块 | 纯函数签名（要点） | design.md |
| --- | --- | --- |
| `model` | `Provider/ProviderModel/ProviderKey/Tenant/TenantModel/LimitRole/Candidate/...`（derive Clone/Debug/serde，无逻辑） | §4 |
| `config::ConfigData` | 内存聚合结构（含 `certs` 字段占位，证书对象在本波次用纯 `CertMeta{domain,path}`，真实 PEM 解析留 W4） | §5.2 |
| `router::resolve` | `(&ConfigData, &BreakerView, &Tenant, &str model_key) -> Result<Vec<Candidate>, RouteError>` | §7.1 |
| `swrr::order` / `swrr::pick` | `(&mut Vec<Candidate>, &mut SwrrState) -> ()`（原地排序 + 状态更新） | §7.2 |
| `swrr::SwrrState` | `{ current_weights: HashMap<ProviderId,i32> }` 纯转移 | §7.2 |
| `breaker` | `Breaker::on_failure/on_success/is_dead`（纯状态机：事件 → 新状态 + dead-set 变更） | §8.4 |
| `sse::UsageScanner` | `scan_chunk(&mut State, &[u8], ProviderKind) -> ScanResult`；`memchr` 扫描 `"usage"`，命中处仅反序列化 ~50 字节；跨 chunk 边界小缓冲；`finalize() -> Option<Usage>` | §9.4 |
| `extract::extract_model` | `(&[u8]) -> Option<&[u8]>`；`memchr` 扫描 `"model"` 早退（~20 字节），返回**借用切片**（零 JSON 解析、零分配） | §6.3 |
| `limit::match_roles` | `(&[LimitRole], &MatchCtx) -> Vec<&LimitRole>` | §10.1 |
| `limit::SlidingWindow` | `check_and_inc(now,limit) -> bool`、`add(now,tokens)`（纯：传入 `now: Instant`，便于测试） | §10.2 |
| `auth::cache_decision` | `(&AuthCache, tenant_id, &api_key_hash, now) -> Verdict{Hit(allowed)/Miss}` | §11.2/§11.5 |
| `auth::apply_upstream` | `(verdict, ttl_cfg) -> CacheOp{Set(allowed,ttl)/None}`（回源结果如何回填缓存） | §11.2 |
| `rewrite::rewrite_path` | `(req_path, &EndpointUrl) -> String`（首个 `/v1` 切分拼接） | §6.5 |
| `rewrite::mask_key` | `(&str) -> String`（前4+后4） | §9.5 |
| `config::validate` | `(&ConfigData) -> Vec<ValidationIssue>` | §5.4 |

> 「熔断」「滑动窗口」「认证缓存」在 core 里是**纯状态机/纯数据结构**：接收事件与「当前时刻」作输入，返回新状态。真实时间 (`Instant::now()`) 由外壳注入，测试传入可控时刻。

---

## 3. TDD 任务列表（红 → 绿 → 重构，按序）

> 命名约定：`#[test] fn <模块>_<场景>()`。每个任务先写测试（失败）再实现。仅列关键用例，实现时按等价类补全。

### 3.1 实体与 ConfigData（0.3d）
- T1.1 `entities_derive_roundtrip`：各实体 serde 序列化/反序列化对称（构造 → JSON → 还原相等）。
- T1.2 `configdata_construct_and_index`：手工拼装 `ConfigData`，断言 `tenants_by_domain`/`models_by_key`/`tenant_providers`/`tenant_models`/`provider_keys` 索引正确。

**foundation lane 共享类型与决策（必先合并，供各 lane 引用，避免重复合并冲突）：**

- `AuthVerdict{Allowed{source}, Denied{status,reason,source}}` + `CacheSource{Hit,Miss,Local}`：**归 core `auth` 模块**为纯类型；`cache_decision` 返回低层 `Verdict{Hit/Miss}`，由 core 的 `decide()` 映射为携带状态码的 `AuthVerdict`（design §11.6）。
- `UsageRecord` / `Usage` / `ProviderKind`：归 core（`UsageRecord` 放 `model`/`usage` 模块，供 W3/W4 引用）。
- `sha256(api_key) -> [u8;32]`：归 core `auth` 模块；**新增纯依赖 `sha2`**（纯密码学、无 I/O）→ core 允许依赖白名单更新为 `memchr`/`bytes`/`sha2`（同步更新 §3.10 T10.1 与出口准则）。
- 路由/解析辅助类型：`BreakerView`（breaker 模块）、`EndpointUrl`（rewrite 模块）、`MatchCtx`（limit 模块）、`RouteError`（router 模块）——各由所属模块定义并 rustdoc 标注，供其他 lane 引用。
- **并发外壳（`Arc<CircuitBreaker>`、限流 `DashMap<LimitKey,SlidingWindow>`、SWRR `DashMap`、各 GC 后台任务）不在 core**，由 **W4 server 侧**装配（design §5.3/§10.2）；core 仅交付纯状态机 + 读写接口。

### 3.2 路由 resolve（0.8d）—— design §7.1
- T2.1 `resolve_tenant_model_gate_reject`：model 不在租户 `tenant_models` → `Err(ModelNotAllowed)`（闸门优先）。
- T2.2 `resolve_tenant_model_gate_pass`：在列表内 → 继续。
- T2.3 `resolve_model_not_found`：`models_by_key` 无该 key → `Err(ModelNotFound)`。
- T2.4 `resolve_tenant_no_providers`：租户无 `tenant_providers` → `Err(TenantForbidden)`。
- T2.5 `resolve_intersection_empty`：交集为空 → `Err(NoAvailableProvider)`。
- T2.6 `resolve_intersection_subset`：模型由 [A,B,C]、租户授权 [B,C,D] → 候选 {B,C}。
- T2.7 `resolve_filter_no_keys`：候选 provider 无 api_key → 被过滤；全无 → `Err`。
- T2.8 `resolve_filter_weight_zero`：`weight=0` 软禁用，被过滤。
- T2.9 `resolve_filter_breaker_dead`：`BreakerView` 标 dead 的 provider 被过滤。
- T2.10 `resolve_all_filtered`：全部被熔断/软禁用/无 key → `Err(NoAvailableProvider)`。
- T2.11 `resolve_ok_returns_candidates_with_weight`：成功返回带 weight 的候选，顺序由后续 swrr 决定（此处仅校验集合）。

### 3.3 SWRR（0.6d）—— design §7.2
- T3.1 `swrr_single_candidate_always_picked`：唯一候选每次都中。
- T3.2 `swrr_proportional_distribution`：权重 3:1，8 次选取分布 = 6:2（Nginx SWRR 平滑序列）。
- T3.3 `swrr_state_advances`：连续调用 `current_weight` 正确累加与扣减。
- T3.4 `swrr_state_keyed_by_tenant_model`：不同 `(tenant,model)` 状态独立。
- T3.5 `swrr_order_preserves_set`：`order()` 仅重排，不丢候选。
- T3.6 `swrr_failover_does_not_reuse`：故障转移按数组顺序遍历，不调用 swrr（契约注释 + 用例验证 cursor 递增）。

### 3.4 熔断状态机（0.4d）—— design §8.4
- T4.1 `breaker_below_threshold_stays_alive`：失败 < threshold 不进 dead。
- T4.2 `breaker_threshold_marks_dead`：连续 threshold 次 `on_failure` → `is_dead=true`。
- T4.3 `breaker_success_resets`：`on_success` 清零计数并移出 dead。
- T4.4 `breaker_non_consecutive_reset`：失败中间夹一次成功 → 计数归零（连续性语义）。
- T4.5 `breaker_dead_view_filtered_by_resolve`：与 T2.9 联动确认。
- T4.6 `breaker_deadset_is_additive_until_probe`：core 不做探活（探活是 IO，留 W4），仅暴露 dead-set 读写接口供外壳探活任务调用。

### 3.5 零拷贝扫描：`"model"` 提取 + usage 解析（0.8d）—— design §6.3/§9.4

> 全部基于 `memchr` SIMD 字节扫描，**默认零 JSON 解析、零分配**；命中处仅反序列化小切片。返回值优先借用 `&[u8]`。

**`extract::extract_model`（请求侧）：**
- T5.1 `extract_model_standard`：`{"model":"gpt-4o",...}` → `Some(b"gpt-4o")`（早退，不扫整 body）。
- T5.2 `extract_model_whitespace_tolerant`：`{ "model" : "gpt-4o" }` 各处空白 → 正确。
- T5.3 `extract_model_not_first_field`：`{"a":1,"model":"x"}` 仍命中（`memchr::find` 定位）。
- T5.4 `extract_model_missing_returns_none`：无 `"model"` → `None`。
- T5.5 `extract_model_no_allocation`：返回 `&[u8]` 必为入参的子区间（断言指针落在 input 边界内）；零分配由借用返回类型在编译期保证。
- T5.6 `extract_model_nested_false_match_avoided`：body 含 `{"messages":[{"model":"x"}]}` 这类嵌套 `"model"` → 用对象起始锚定/首匹配策略返回顶层 `model`，文档化取舍。
- T5.7 `extract_model_short_input_no_panic`：空/极短输入不 panic。
- T5.8 `extract_model_first_chunk_only`：仅首 chunk 即可（模拟首 chunk 含完整 model 字段），无需读完整 10MB。

**`sse::UsageScanner`（响应侧）：**
- T5.9 `usage_scan_no_usage_skips_zero_alloc`：无 `"usage"` chunk → `ScanResult::Skip`，零反序列化、零分配。
- T5.10 `usage_scan_finds_usage_memchr`：含 `"usage"` chunk → `ScanResult::Found`，仅此 chunk 反序列化。
- T5.11 `usage_openai_final_chunk`：末尾 `data: {"usage":{prompt/completion/total}}` → `finalize` 得用量。
- T5.12 `usage_anthropic_message_delta`：`event: message_delta` + `usage:{input_tokens,output_tokens}` → 归一。
- T5.13 `usage_anthropic_incremental_accumulate`：多 delta 的 output_tokens 增量累加。
- T5.14 `usage_done_marker`：`data: [DONE]` 终止，后续字节忽略。
- T5.15 `usage_cross_chunk_boundary`：`"usage"` 被拆在两 chunk → 尾部小缓冲拼接，仍命中（常态零分配）。
- T5.16 `usage_non_stream_json`：单次完整 JSON（非 SSE）一次扫描+反序列化。
- T5.17 `usage_schema_dispatch_by_provider`：`ProviderKind` 驱动归一分支。
- T5.18 `usage_malformed_chunk_skipped`：非法 JSON 行不 panic，跳过。
- T5.19 `usage_openai_no_include_usage`：流中无 usage → `finalize` 得 `None`（已知限制）。

### 3.6 限流（0.5d）—— design §10
- T6.1 `limit_match_all_null_matches_everything`：全 NULL 的 role 匹配任意 `MatchCtx`。
- T6.2 `limit_match_specific_dimensions`：`matching_key/model/tenant/provider` 各维度精确匹配与通配。
- T6.3 `limit_match_multiple_overlay`：多 role 命中，返回全部（取严由调用方）。
- T6.4 `window_count_within_limit`：`check_and_inc` 窗口内 < limit 返回 true 并入队。
- T6.5 `window_count_exceeds`：达上限返回 false。
- T6.6 `window_sliding_eviction`：传入 `now` 推进，过期样本被淘汰后重新放行。
- T6.7 `window_token_dimension`：`add(tokens)` 累计，token 维度独立窗口。

### 3.7 认证缓存判定（0.4d）—— design §11.2/§11.5/§11.6
- T7.1 `auth_cache_hit_allowed`：命中且 allowed=true 且未过期 → `Verdict::Hit(true)`。
- T7.2 `auth_cache_hit_denied`：命中 allowed=false 且未过期 → `Verdict::Hit(false)`。
- T7.3 `auth_cache_expired_is_miss`：`now > expires_at` → `Miss`。
- T7.4 `auth_apply_upstream_2xx_sets_allow`：回源 2xx → `CacheOp::Set(true, allow_ttl)`。
- T7.5 `auth_apply_upstream_401_sets_deny`：401/403 → `Set(false, deny_ttl)`。
- T7.6 `auth_apply_upstream_5xx_no_cache`：5xx/超时 → `None`（不缓存，按 fail_mode 由外壳决定响应）。
- T7.7 `auth_verdict_carries_status`：`Denied{status, reason}` 携带 401/503，供外壳直接写响应（§11.6）。

### 3.8 重写与脱敏（0.3d）—— design §6.5/§9.5
- T8.1 `rewrite_first_v1_split`：`/foo/v1/chat` + endpoint `https://api.x.com` → `/v1/chat`、Host=api.x.com。
- T8.2 `rewrite_endpoint_with_prefix`：endpoint `https://gw.x.com/llm` → `/llm/v1/chat`。
- T8.3 `rewrite_no_v1_passthrough`：path 无 `/v1` → 整 path 拼接。
- T8.4 `rewrite_multiple_v1_uses_first`：`/v1/a/v1/b` → 首个 `/v1` 起 = `/v1/a/v1/b`（与设计一致）。
- T8.5 `mask_key_short_input`：过短 key 全掩码（不越界 panic）。
- T8.6 `mask_key_normal`：`sk-abcdef…wxyz` 格式。

### 3.9 配置加载期校验（0.3d）—— design §5.4
- T9.1 `validate_dangling_tenant_provider`：`tenant_provider.provider_id` 不存在 → 报 issue。
- T9.2 `validate_tenant_model_orphan`：`tenant_model.model_key` 无任何在线 provider_model 提供 → 报 issue。
- T9.3 `validate_bad_endpoint_url`：endpoint 不可解析/scheme 非法 → 报 issue。
- T9.4 `validate_provider_without_key`：在线 provider 无 api_key → 告警级 issue（非致命）。
- T9.5 `validate_limit_role_both_null`：count 与 token 同 NULL → 告警。
- T9.6 `validate_clean_config_no_issues`：合法配置返回空。
- T9.7 `validate_severity_split`：致命项（如全租户证书缺失，证书在本波以 `CertMeta` 占位故用「路径空」模拟）vs 告警项可区分。

### 3.10 workspace 与依赖焊死（0.3d）
- T10.1 `cargo_tree_core_has_no_io`：CI 脚本断言 `cargo tree -p hydra-core` 输出不含 `tokio/pingora/sqlx/reqwest/hyper`（`memchr`/`bytes`/`sha2` 为纯库除外；`tests/compile_gate.rs` 或独立 xtask）。
- T10.2 `core_is_sync`：`fn _assert_sync<T: Sync>(_: T)` 对所有公开结构体编译期断言。

---

## 4. 外部边界与测试方式

**本波次无外部边界。** 全部为纯函数单测：构造输入 → 断言输出。不使用 `wiremock`、不使用 in-memory SQLite、不引入任何 mock 库。`#[cfg(test)]` 仅用于测试模块本身，**绝不在生产代码内**制造条件分支。

唯一「可变状态」（熔断计数、SWRR current_weight、滑动窗口样本、认证缓存条目）均以**显式传入的 `now: Instant`** 驱动，测试时间可控。

---

## 5. 与 design.md 的映射

纯逻辑来源：§4（实体）、§5.2/§5.4（ConfigData+校验）、§7（路由+SWRR）、§8.4（熔断纯部）、§9.4（解析）、§10（限流）、§11.2/§11.5/§11.6（认证缓存+verdict）、§6.5/§9.5（重写+脱敏）。

---

## 6. 出口准则

- [ ] `cargo test -p hydra-core` 全绿，行覆盖率 ≥ 90%；
- [ ] `cargo tree -p hydra-core` 无任何 I/O 依赖（`memchr`/`bytes`/`sha2` 纯库除外，CI 校验）；
- [ ] 生产代码 grep 无 `mock`/`stub`/`#[cfg(test)]`/`unimplemented!`/`todo!`/`panic!`；
- [ ] 所有可变状态由显式 `now` 驱动，无隐藏时间依赖；
- [ ] 公开类型 `Send + Sync` 编译期断言通过；
- [ ] 各模块 rustdoc（`///`）注释说明纯性与时间注入点。

---

## 7. 风险与注意

- **熔断探活不在本波**：core 只暴露 `is_dead`/状态转移与 dead-set 读写接口；探活（真实 HTTP/TCP）是 IO，留 W4。务必把接口设计成「外壳可注入探活结果」。
- **usage schema 扩展**：以 `ProviderKind` 枚举驱动，预留未知 provider 走「通用 JSON 兜底」分支，避免每加一家就改解析器。
- **SWRR 状态体积**：`HashMap<ProviderId,i32>` 跨 (tenant,model) 可能膨胀；core 仅提供单 key 状态，外层 `DashMap` 装配与清空由 W2 store 负责。
- **时间类型**：core 用 `std::time::Instant`（同步、纯）；不要引入 chrono/tokio-time。
