# 设计变更：Proxy 终止模式（Terminate-in-Pingora）

> **状态**：待 Oracle 交叉审核  
> **触发**：late-model 客户端（Hermes agent 等）的 model 提取在 first-chunk memchr 下失败 → 路由错误  
> **决策依据**：[`architecture-2tier-analysis.md`](architecture-2tier-analysis.md) + [`architecture-industry-research.md`](architecture-industry-research.md)  
> **范围**：仅 `hydra-server/src/proxy.rs` 重写 + 新增 provider client；`hydra-core` 零改动

---

## 1. 变更概要

将 Hydra 的代理模式从 **零拷贝 stream-through**（Pingora 转发 upstream）改为 **terminate-in-Pingora**（在 `request_filter` 内终止请求，用自己的 HTTP client 调供应商，流式回写响应）。

**业界已验证**：tokenmiser-proxy 在 Pingora 上做了完全相同的事（`request_filter` 返回 `Ok(true)`，不拨号 upstream）。STOA Gateway 评估了 sidecar（2-tier）方案并拒绝，选择 embedded（≈ terminate-in-Pingora）。所有 8 个调研的生产级 LLM 网关都 terminate（无一 stream-through）。

**核心收益**：model 提取从"first-chunk 赌博"变为"全 body 平凡提取"；故障转移从"与 Pingora 重试机制搏斗"变为"简单 for 循环"；删除全部逆框架 hack（enable_retry_buffering / Vec\<Bytes\> / set_retry / 64KB 限制 / passthrough 兜底）。

---

## 2. 背景与触发

### 问题
当前 `request_filter` 只读第一个 body chunk，用 `memchr` 扫描 `"model"`。对于 `model` 字段不在首 chunk 的客户端（Hermes agent：大段 system/tools/history 前置），提取失败 → 落入 passthrough → **不按 model 路由，直连首选 provider** → 路由错误。

### 为什么 first-chunk 无法修复
- `enable_retry_buffering` 的 `BODY_BUF_LIMIT = 64KiB`：即使"读直到找到"，超过 64KB 后 Pingora retry buffer 截断 → 正常转发链路断裂。
- `upstream_peer()` 在 body 读取前运行：Pingora 的生命周期决定了"body 依赖路由"只能在 `request_filter` 内做（读 body → 路由 → 终止或转发），这与 stream-through 零拷贝**结构性冲突**。

### 为什么 terminate 能根治
全 body 在手 → `model` 提取变为 `serde_json::from_slice(&body)?.model`（或全 body memchr）→ 任意位置/嵌套/schema 均可提取。路由、故障转移重放、token 预估全部变为平凡操作。

---

## 3. 新架构

```
                         ┌─────────────────────────────────────────────────┐
  Client ──► Pingora ──►│  request_filter (终止模式)                        │
  (TLS/SNI)              │                                                 │
                         │  ① 域名→租户  ② api-key  ③ 外部认证(缓存优先)      │
                         │  ④ 读全body → model提取(平凡)                       │
                         │  ⑤ 路由 resolve+SWRR  ⑥ 限流                      │
                         │                                                 │
                         │  ⑦ for candidate in candidates:                  │
                         │       构造请求(swap key, /v1 rewrite)              │
                         │       provider_client.send(req) ───────► Provider │
                         │       │                                           │
                         │       ├─ 成功 → 流式写回 session → break            │
                         │       └─ 失败 → breaker.on_failure → continue      │
                         │                                                 │
                         │  ⑧ SSE 逐 chunk 流式回写 + memchr 用量扫描           │
                         │  ⑨ logging: UsageSink + metrics                  │
                         │                                                 │
                         │  return Ok(true)  ← Pingora 不拨号 upstream        │
                         └─────────────────────────────────────────────────┘
```

### 组件角色

| 组件                   | 角色                         | 变化          |
| ---------------------- | ---------------------------- | ------------- |
| Pingora Server         | 监听器 + TLS 终止 + H2 + 优雅升级 | **不变**          |
| ProxyHttp::request_filter | 全功能网关（读 body → 路由 → 调供应商 → 流式回写） | **重写**          |
| ProxyHttp::upstream_peer | trait 要求，返回 sentinel，永不调用 | **空壳**          |
| upstream_*/response_* filters | 不再使用                     | **删除**          |
| **ProviderClient（新增）** | `Arc<reqwest::Client>` 共享池 + 请求构造 + 响应 streaming | **~150 行新增**    |
| AuthChecker            | 外部认证                     | 不变          |
| ConfigStore            | 配置热更新                    | 不变          |
| CircuitBreaker         | 连续失败 → dead-set → 探活    | 不变          |
| RateLimiter            | 滑动窗口限流                  | 不变          |
| UsageSink              | 用量记录                      | 不变          |
| HydraCertStore         | SNI 证书选择                  | 不变          |
| AdminService           | REST + UI + metrics          | 不变          |
| **hydra-core（全部纯逻辑）** | router/swrr/breaker/limit/auth/extract/sse/rewrite | **零改动（83 测试不动）** |

---

## 4. 技术设计

### 4.1 request_filter 新生命周期

```rust
async fn request_filter(&self, session: &mut Session, ctx: &mut RequestContext) -> Result<bool> {
    let started = Instant::now();

    // ── 不变部分（W1-W5 已实现）──
    let tenant = extract::resolve_tenant(&session, &self.store.snapshot())?;        // ① ②
    let api_key = extract::client_api_key(&session);
    let verdict = self.auth.check(&tenant, &api_key).await;                         // ③
    if let AuthVerdict::Denied { status, .. } = &verdict {
        return write_error(session, *status).await; // Ok(true)
    }

    // ── 变更部分 ──
    let body = session.downstream_session.read_body_bytes().await?;                 // ④ 读全 body
    // 也可以用 read_request_body 循环读到 EOS
    let body_bytes = body.unwrap_or_default();

    let model = extract::extract_model(&body_bytes)                                 // ④' model 提取（全 body）
        .ok_or_else(|| write_error(session, 400, "model not found"))?;
    let candidates = router::resolve(&snapshot, &self.breaker, &tenant, model)?;     // ⑤ 路由
    let ordered = swrr::order(candidates, swrr_state);                              // ⑤' SWRR

    if let Err(_) = self.limiter.check_count(&tenant, &model) {                    // ⑥ 限流
        return write_error(session, 429).await;
    }

    // ⑦ 故障转移循环
    let mut last_error = None;
    for (i, candidate) in ordered.iter().enumerate() {
        ctx.cursor = i;
        let provider = &snapshot.providers[&candidate.provider_id];
        let upstream_key = random_key(&snapshot, &candidate.provider_id);

        // 构造上游请求
        let req = self.provider_client.build_request(
            &session.req_header(),
            provider,
            &upstream_key,
            &body_bytes,
        );

        match self.provider_client.send(req).await {
            Ok(resp) if resp.status().is_success() => {
                self.breaker.on_success(&candidate.provider_id);
                ctx.selected = SelectedRoute { provider_id: candidate.provider_id.clone(), .. };

                // ⑧ 流式写回 SSE
                write_response_header(session, resp.headers(), false).await?;
                let mut scanner = UsageScanner::new(provider_kind);
                let mut stream = resp.bytes_stream();
                while let Some(chunk_result) = stream.next().await {
                    let chunk = chunk_result?;
                    scanner.scan_chunk(&chunk);
                    session.write_response_body(Some(chunk), false).await?;
                }
                session.write_response_body(None, true).await?; // EOS
                ctx.usage = scanner.finalize();

                // ⑨ logging
                self.log_request(session, ctx, &started);
                return Ok(true); // 终止，Pingora 不拨号
            }
            Ok(resp) => {
                // 4xx/5xx — 非连接失败，可能是 provider 返回的错误
                self.breaker.on_failure(&candidate.provider_id);
                last_error = Some(format!("provider {} returned {}", candidate.provider_id, resp.status()));
                continue;
            }
            Err(e) => {
                self.breaker.on_failure(&candidate.provider_id);
                last_error = Some(e.to_string());
                continue; // 连接失败 → 下一候选（body 已在手，零成本重放）
            }
        }
    }

    // 全部候选耗尽
    write_error(session, 502, &last_error.unwrap_or_default()).await
}
```

### 4.2 ProviderClient（新增模块）

```rust
// crates/hydra-server/src/proxy/provider_client.rs
pub struct ProviderClient {
    client: reqwest::Client,      // 共享连接池（rustls, H2）
}

impl ProviderClient {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(32)
            .pool_idle_timeout(Duration::from_secs(90))
            .timeout(Duration::from_secs(300))  // LLM 请求长超时
            .build()
            .unwrap();
        Self { client }
    }

    /// 构造上游请求（swap key + /v1 rewrite + Host）
    pub fn build_request(&self, original: &RequestHeader, provider: &Provider,
                         upstream_key: &str, body: &[u8]) -> reqwest::Request {
        let url = rewrite::rewrite_path(original.uri.path(), &EndpointUrl::from(&provider.endpoint));
        reqwest::Request::builder()
            .method(original.method.as_str())
            .url(&url)
            .header("Authorization", format!("Bearer {}", upstream_key))
            .header("Content-Type", "application/json")
            .header("X-Hydra-Trace-Id", &trace_id)
            .body(body.to_vec())     // body clone（零成本重放的前提）
            .build()
            .unwrap()
    }

    pub async fn send(&self, req: reqwest::Request) -> Result<reqwest::Response, reqwest::Error> {
        self.client.execute(req).await
    }
}
```

### 4.3 故障转移（简化）

**当前**（与 Pingora 重试搏斗）：
- `fail_to_connect` → `e.set_retry(true)` → Pingora 重新调用 `upstream_peer` → cursor++ → Vec\<Bytes\> 重放
- `error_while_proxy` → 条件 set_retry（`retry_after_connect` + `upstream_bytes_seen==0` + `body_replayable`）
- 64KB buffer 截断处理 + body_too_large 标志

**新**（简单 for 循环）：
- `for candidate in candidates { try send; on fail continue; }` — body 已在手，clone 给下一候选。
- 无 Pingora 重试机制、无 buffer 截断、无 set_retry、无 cursor 追踪。

### 4.4 用量解析（不变）

`UsageScanner`（hydra-core 纯函数）在 SSE chunk 循环中逐块 memchr 扫描——与当前 `upstream_response_body_filter` 中的逻辑**完全相同**，只是调用位置从 filter 回调移到 request_filter 内的 stream 循环。

### 4.5 被删除的机制

| 机制 | 原因 |
| --- | --- |
| `enable_retry_buffering()` | 不再依赖 Pingora upstream 重试 |
| `Vec<Bytes>` 累加器 | body 已全量读入，直接 clone 给 reqwest |
| `set_retry(true)` / cursor | 故障转移改为 for 循环 |
| `fail_to_connect` / `error_while_proxy` | 合并为 for 循环的 continue |
| `request_body_filter` / `upstream_request_filter` | 请求构造移入 build_request |
| `upstream_response_body_filter` / `response_body_filter` | 响应流式移入 stream 循环 |
| `body_too_large` / BODY_BUF_LIMIT | 不再相关 |
| passthrough 兜底 | 全 body 在手，总能提取 model |
| 首 chunk memchr 限制 | 全 body memchr/serde |

---

## 5. 权衡分析

### 收益
| 维度 | 改善 |
| --- | --- |
| **model 提取** | 从"首 chunk 赌博"→"全 body 平凡提取"；任意位置/嵌套/schema |
| **故障转移** | 从"与 Pingora 搏斗"→"简单 for 循环"；无 64KB 限制 |
| **代码复杂度** | proxy.rs 从 ~800 行逆框架 hack → ~500 行直白逻辑 |
| **可维护性** | terminate 是业界标准模式；stream-through body 路由是 Pingora 的非预期用法 |
| **hydra-core** | 零改动，83 测试全绿 |

### 代价
| 维度 | 影响 | 量化 |
| --- | --- | --- |
| **请求 TTFT** | 等 full body 才调供应商 | 大 prompt (~5MB) +100-500ms；小 prompt 可忽略 |
| **零拷贝请求** | body 全读入内存再通过 reqwest 发出 | 放弃零拷贝（用户原始"强制"需求） |
| **零拷贝响应** | reqwest 读 → session 写（一次 userspace copy） | SSE chunk ~100B，copy 开销可忽略 |
| **供应商连接池** | reqwest per-client 池 < Pingora 共享池 | 高 RPS(>10K) 时有差异；LLM 场景影响小（provider 延迟主导） |
| **内存** | 全 body 缓冲（当前 Vec\<Bytes\> 也缓冲——差异小） | 500 并发 × 2MB avg ≈ 1GB（与当前几乎相同） |

### 零拷贝的"强制"需求审视
用户的零拷贝需求（§6 零拷贝原则）初衷是**"防止 JSON 反复 encode/decode"**（吞吐杀手）。terminate 模式：
- ✅ **仍然不做 JSON encode/decode**（body 原样 bytes 传给 reqwest，response chunk 原样写回 session）。
- ❌ **不再是 kernel-level 零拷贝**（body 经过 userspace buffer）。
- 本质：从"Pingora 零拷贝转发"变为"reqwest userspace 转发"——body 字节未被 serde 处理，但多了一次内存拷贝。

---

## 6. 开发计划

### Phase T1 — ProviderClient + 骨架（0.5d）
- 新增 `proxy/provider_client.rs`（reqwest client + build_request + send）。
- `request_filter` 改为 terminate 骨架：auth + read_body + route（暂不调供应商，返回 501）。
- upstream_peer 改为 sentinel。
- 删除 filter 回调（request_body_filter / upstream_*_filter / response_*_filter）。
- **TDD**：auth pass-through 测试（验证认证仍工作）、body-read 测试。

### Phase T2 — 全功能 request_filter（1d）
- 实现 ④ read full body + ④' model extract（全 body memchr/serde）。
- 实现 ⑦ failover for 循环 + provider_client.send。
- 实现 ⑧ SSE 流式回写 + usage scanner。
- 实现 ⑨ logging（UsageSink + metrics + breaker.on_success）。
- **TDD**：wiremock mock 供应商集成测试（转发 JSON + SSE + failover + breaker + usage 提取 + error codes）。

### Phase T3 — 清理 + 验证（0.5d）
- 删除 Vec\<Bytes\> / enable_retry_buffering / set_retry / body_too_large / passthrough。
- 清理 ctx.rs（移除 cursor/upstream_bytes_seen/body_too_large 等字段）。
- 运行全量 `cargo test --features server`（确保 W2/W3/W4b/W5 不回归）。
- clippy + fmt。
- 集成测试（integration/test_crud.py 对 Docker 容器 — 109/109 应仍绿）。

### Phase T4 — 零拷贝模式 feature flag（0.5d，可选）
- 保留当前 proxy.rs 为 `proxy_zero_copy` 模式（feature flag）。
- 默认 terminate 模式；零拷贝模式可选（仅 OpenAI 兼容 + 不需 late-model 的场景）。
- 文档更新（ops.md + README + design.md §6）。

**估时合计：~2.5d（T1-T3），T4 可选 +0.5d。**

### 与当前实现的关系
- hydra-core：**零改动**（83 测试）。
- W2（db/store）、W3（auth/sink）、W4b（TLS）、W5（admin/metrics）、W6（UI）：**零改动**。
- 仅 W4 的 proxy.rs + ctx.rs：**重写**。
- 集成测试（integration/）：**不变**（admin API 不变，109 测试仍绿）。

---

## 7. 风险与回退

| 风险 | 缓解 |
| --- | --- |
| TTFT 回退（大 prompt） | LLM 请求秒级延迟，body-read 占比小；可监控 `hydra_request_duration_seconds` 对比 |
| reqwest 连接池不如 Pingora | LLM 场景 provider 延迟主导，pool 差异影响小；可调 pool_max_idle |
| reqwest streaming SSE jitter | Rust 无 GC，inter-chunk jitter 可控（AIGatewayBench 方法论证实） |
| 放弃零拷贝"强制"需求 | 本质仍不做 JSON encode/decode；用户需确认是否接受 userspace copy |
| 回退 | feature flag `proxy_zero_copy` 保留当前实现；或 git revert proxy.rs（hydra-core 不受影响） |

---

## 8. 设计文档更新

- **design.md §6**：新增"终止模式（Terminate Mode）"章节，标注为当前默认模式；原"零拷贝 stream-through"标注为可选模式（feature flag）。
- **ops.md**：新增部署配置项（terminate 模式默认 / zero-copy 可选）。
- **README.md / README.zh-CN.md**：更新架构说明。

---

> 本文档待 Oracle 交叉审核。审核通过后按 §6 开发计划执行。
