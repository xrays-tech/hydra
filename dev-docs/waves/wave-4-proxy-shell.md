# Wave 4 — Pingora 代理外壳（Proxy Shell Integration）

> crate：`hydra-server` ｜ 估时：3d ｜ 串行（依赖 W1+W2+W3）
>
> 关键纪律：本波把 **W1 纯函数**接到 **Pingora 生命周期**，外壳只做「session ↔ core 输入/输出」翻译。**绝不 mock 路由/SWRR/解析等内部逻辑**——它们已在 W1 测过，此处直接调用。唯一边界 = 真实上游，用 **真实 HTTP mock upstream server**（进程级 double）测。

---

## 1. 目标与范围

### In-scope
- `proxy::HydraProxy`（`impl ProxyHttp`）+ `RequestContext`（CTX）；
- `request_filter`：域名→租户、api-key 解析、外部认证（接 W3 `AuthChecker`）、**零拷贝 model 提取**（`read_body_bytes` 仅读首 chunk + W1 `extract_model` `memchr` 扫描）、路由（接 W1 `resolve`）、前置限流、非 JSON 路径 passthrough；
- `upstream_peer` + `peer::build`（endpoint→HttpPeer）；
- `upstream_request_filter` + `rewrite`（接 W1）：替换 api-key、`/v1` 重写、Host；
- 响应过滤：`upstream_response_filter`/`response_filter`/`upstream_response_body_filter`（接 W1 `UsageScanner` `memchr` 扫描，body 原样透传）、`logging`（接 W3 Sink + 熔断 on_success/on_failure）；
- 故障转移：`fail_to_connect`（总重试 + on_failure）/ `error_while_proxy`（条件重试，§8.3）；
- 零拷贝重放缓冲：自实现 `ctx.body_buffer: Vec<Bytes>`（`Bytes::clone` O(1)），取代失效的 `enable_retry_buffering`；大请求体策略（§8.5：软上限禁用重试 / 硬上限 413）；
- 熔断器探活后台任务（真实 HTTP/TCP 探测，接 W1 breaker 状态）；
- 多租户 TLS：`tls::HydraCertStore`（`certificate_callback`，证书来自 W2 `ConfigData.certs`，PEM 解析在此波完成）；
- 并发外壳装配（design §5.3/§10.2，B7）：`Arc<CircuitBreaker>`（DashSet dead-set）、限流 `DashMap<LimitKey,SlidingWindow>`、SWRR `DashMap<(tenant,model),SwrrState>` + 各 GC 后台任务——把 W1 纯状态机包成 server 侧可并发结构（core 仅交付纯逻辑 + 读写接口）。

### Out-of-scope
- Admin REST/指标（W5）；
- 内嵌 UI（W6）；
- Playwright E2E（W6，本波用集成测试）。

### 依赖与前置
- W1：`resolve`/`swrr`/`breaker`/`UsageScanner`/`extract_model`/`limit`/`rewrite`/`mask_key`/纯类型。
- W2：`ConfigStore`（snapshot/reload）、`CertMeta`→需在此波解析为 `ResolvedCert`。
- W3：`AuthChecker`、`UsageSink`。

---

## 2. TDD 任务列表

### 2.1 Peer 构造与重写（0.3d）
- T1.1 `peer_build_https_sni`：endpoint `https://api.x.com` → `HttpPeer{addr=api.x.com:443, tls=true, sni=api.x.com}`。
- T1.2 `peer_build_http`：endpoint `http://up:8080` → `tls=false`。
- T1.3 `peer_build_custom_port`：endpoint `https://x.com:8443` → port 8443。
- T1.4 `rewrite_applied_in_upstream_request_filter`：构造 CTX.selected，断言改写后 `Authorization` 为 provider key、`Host`/路径按 W1 `rewrite_path`。

### 2.2 Spike：Pingora 最小集成（0.4d）—— 先验证 API 再铺开
- T2.1 `spike_request_filter_short_circuit`：最小 `ProxyHttp`，`request_filter` 写 200 "ok" → `Ok(true)`；启动真实 Pingora server + reqwest 客户端请求，断言收 200。
- T2.2 `spike_first_chunk_model_extract`：`read_body_bytes().await` 仅读首 chunk + W1 `extract_model`（`memchr`）提取 model；**首 chunk 必须回填 `ctx.body_buffer` 并原样转发**，验证上游收到完整 body（零拷贝，design §6.3）。
- T2.3 `spike_vec_bytes_replay`：`ctx.body_buffer: Vec<Bytes>` 累积（`Bytes::clone` O(1)）+ `request_body_filter` 增量 push；故障转移时遍历重放，验证上游收到等价 body。**【W4 spike 结果】首 chunk 正常转发需 `enable_retry_buffering()`（Pingora 默认，回放已消费首 chunk；手动 `request_body_filter` 再注入失败——小 body 时 `is_body_done` 跳过转发）；64KiB 仅影响 Pingora 自身 retry，故障转移重放用 `Vec<Bytes>`（不受限）。**
- T2.4 `spike_body_too_large_threshold`：累积字节达 `[proxy] max_request_body` 软上限 → `body_too_large=true`（停止累积，body 仍原样转发）；超 `max_request_body_hard` → 413。
> Spike 用例可保留为集成测试。

### 2.3 request_filter 链路（0.7d）—— 接 W1/W3
> 用 mock upstream（wiremock 或自建 HTTP server）+ 真实 Pingora 实例 + reqwest 客户端。
- T3.1 `rf_unknown_domain_404`：Host 不匹配任一租户 → 404。
- T3.2 `rf_disabled_tenant_403`：`enabled=0` → 403。
- T3.3 `rf_auth_denied_401`：AuthChecker 返回 `Denied{401}` → 401，不转发。
- T3.4 `rf_auth_fail_closed_503`：AuthChecker 返回 `Denied{503}` → 503。
- T3.5 `rf_no_auth_url_401`：tenant.auth_url 空 → 401。
- T3.6 `rf_model_not_in_tenant_model_403`：model 不在 `tenant_models` → 403（接 W1 `ModelNotAllowed`）。
- T3.7 `rf_no_provider_403`：交集空/全熔断 → 403/503。
- T3.8 `rf_limit_count_429`：限流超 `limit_count` → 429。
- T3.9 `rf_passthrough_non_v1`：`GET /v1/models` 无 model → passthrough 直连首选 provider（不替换 key）。
- T3.10 `rf_body_too_large_413`：body 超 `max_request_body_hard` → 413，连接关闭。
- T3.11 `rf_happy_path_sets_ctx`：全部通过 → CTX.candidates/selected 正确，`Ok(false)` 继续上游。

### 2.4 转发与流式（0.6d）
- T4.1 `forward_non_stream_json`：mock upstream 返回 JSON → 客户端收到等价 body；usage 解析正确（W1 纯函数驱动）。
- T4.2 `forward_sse_streamed`：mock upstream 返回 `text/event-stream` 分块 → 客户端逐块收到（顺序/内容一致）。
- T4.3 `forward_usage_extracted_on_logging`：SSE 末尾 usage → `UsageSink.record` 被调用一次（用 Spy Sink：真实 Sink trait 的计数实现，非内部逻辑 mock）。
- T4.4 `forward_provider_fingerprint_stripped`：上游 `server`/`via` 头被去除。

### 2.5 故障转移与熔断（0.7d）—— design §8
> mock upstream 配置多个：主返回连接拒绝/超时，备返回 200。
- T5.1 `failover_connect_error_tries_next`：首选上游连接失败 → 自动切下一候选 → 客户端收 200。
- T5.2 `failover_exhausted_502`：全部候选连接失败 → 502，body 含 attempts/trace_id。
- T5.3 `failover_retry_after_connect_off_default`：上游已建连但中断、`retry_after_connect=false`（默认）→ **不重试**，返回错误。
- T5.4 `failover_retry_after_connect_on`：`retry_after_connect=true` 且 `upstream_bytes_seen==0` 且 body 可重放 → 重试下一候选。
- T5.5 `failover_no_retry_mid_stream`：上游已返回字节后中断 → 绝不重试（防重复流）。
- T5.6 `failover_body_too_large_blocks_proxy_retry`：body 超 `max_request_body` → `error_while_proxy` 不重试（body_replayable=false）。
- T5.7 `breaker_marks_dead_after_threshold`：连续 threshold 次失败 → provider 进 dead-set，后续请求候选直接跳过。
- T5.8 `breaker_probe_revives`：探活后台任务（真实 HTTP 探测 mock upstream）成功 → 移出 dead-set。
- T5.9 `breaker_on_success_resets`：上游 2xx 首字节 → 复位计数。

### 2.6 多租户 TLS（0.3d）—— design §12
- T6.1 `tls_sni_selects_tenant_cert`：两个租户两套自签证书，按 SNI 选对（rustls/openssl 客户端连不同域名拿到对应证书）。
- T6.2 `tls_cert_hot_reload`：改证书 + `reload_all` → 新连接用新证书。
- T6.3 `tls_cert_single_source`：`HydraCertStore` 持有的 Arc 与 `ConfigData.certs` 同源（W2 T6.6 已验证引用同一性，本波验证 PEM 解析回填正确）。
- T6.4 `tls_pem_parse_failure_isolated`：单租户坏证书不影响其他租户证书加载（`validate` 告警，其余可用）。

---

## 3. 外部边界与测试方式

| 边界 | double | 性质 |
| --- | --- | --- |
| LLM provider 上游 | **真实 mock upstream HTTP server**（wiremock 或自建） | ✅ 外部第三方上游的网络 double |
| 租户 auth 服务 | W3 `AuthChecker` trait，测试用真实 wiremock 装配的 checker | ✅ 边界 |
| SQLite（usage） | `:memory:` 真实引擎 + Spy Sink（计数实现，trait 真实） | ✅ |

**绝对禁止**：在 `request_filter`/`upstream_peer` 测试里 mock `resolve`/`swrr`/`breaker`/解析。这些是 W1 纯函数，直接用真实 `ConfigData` 喂入。若某集成测试想隔离内部逻辑，说明该逻辑应下沉为 W1 纯函数补测，而非在 W4 mock。

**Pingora 实例化**：集成测试启动真实 `Server`（绑 `127.0.0.1:0` 随机端口），用 reqwest 打它——端到端走真实框架，不 mock Pingora。

---

## 4. 与 design.md 的映射
§6（全生命周期）、§7（路由装配）、§8（故障转移+熔断+大 body）、§9.4（SSE 装配）、§12（TLS）。

---

## 5. 出口准则
- [ ] `cargo test -p hydra-server` 含集成测试全绿（真实 Pingora + mock upstream）；
- [ ] design §6.1 全部 hook 均有至少一个集成测试覆盖；
- [ ] `retry_after_connect` 默认 false 且有专项测试；
- [ ] 熔断 dead-set 经集成测试验证（mark/probe/revive）；
- [ ] 多租户 TLS 端到端选证书 + 热更新验证；
- [ ] 生产代码无内部 mock；Spy Sink 是 `UsageSink` 的真实测试实现（计数），不绕过逻辑；
- [ ] 热路径零拷贝：body 原样转发、`"model"`/`"usage"` 用 `memchr` 提取、故障转移重放用 `Vec<Bytes>`；生产代码 grep 无 `serde_json::from_slice` 作用于完整 body（注：`enable_retry_buffering()` 用于首 chunk 正常转发，W4 验证，允许）。

---

## 6. 风险与注意
- **macOS SSE flush bug**（Pingora #841）：本地开发 SSE 集成测试在 macOS 可能假阴性；CI 用 Linux，本地可跳过或用容器。
- **零拷贝 body 处理**：spike（T2.2/T2.3/T2.4）必须先行，确认首 chunk 读取+回填转发、`Vec<Bytes>` 重放、软/硬上限判定，否则故障转移与零拷贝测试不可信；**禁用 `enable_retry_buffering`**。
- **熔断探活任务**：真实 HTTP 探测需选轻量端点（如 `GET {endpoint}/v1/models`）；对不支持该端点的 provider 降级为 TCP 探活；二者均真实 I/O，非 mock。
- **测试速度**：每集成测试起 Pingora server 较慢；用 `OnceCell` 共享 server 句柄 + 每测独立 mock upstream 端口。
