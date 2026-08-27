# 行业实证：LLM 网关的代理架构（补充 Oracle 分析报告）

> 本文是 [`architecture-2tier-analysis.md`](architecture-2tier-analysis.md) 的行业调研补充。Oracle 报告从第一性原理推导了 2-tier 的得失；本文用**真实生产级 LLM 网关的做法**来佐证，回答一个关键问题：**业界到底怎么做？**

---

## 核心发现：所有生产级 LLM 网关都 terminate（没有一个 stream-through）

| 网关 | 语言/框架 | 架构 | model 提取方式 |
| --- | --- | --- | --- |
| LiteLLM | Python→Rust | terminate | 全 body 反序列化 |
| Portkey | Hono/Node | terminate | header/body 路由 + full body |
| TensorZero | Rust/Axum | terminate | 强类型解析全 body |
| one-api / new-api | Go/Gin | terminate | `UnmarshalBodyReusable` 全 body |
| Helicone | CF Workers | terminate（full buffer） | `JSON.parse(bufferedBody).model` |
| tokenmiser-proxy | **Rust/Pingora** | **terminate in Pingora** | 全 body（`request_filter` 返回 `Ok(true)`） |
| Glide | Rust/Axum | terminate | axum extractor 全 body |
| Vercel AI Gateway | Fluid Compute | terminate | 全 body |
| STOA Gateway | Rust/Axum + **Pingora connector** | **embedded**（不是 sidecar） | axum 全 body + Pingora 连接池调上游 |

**没有一个做 zero-copy stream-through。** 这不是偶然，是结构性的——见下。

> 来源：LiteLLM `ARCHITECTURE.md`；Portkey `streamHandler.ts`；TensorZero `endpoints/inference.rs`；one-api `relay/controller/helper.go`；Helicone `RequestBodyBuffer_InMemory.ts`；tokenmiser `docs.rs`；STOA `ADR-058`；Vercel `ai-gateway-architecture-reference-patterns`。

## 为什么 terminate 是结构性必需（不只是 model 提取）

LLM 路由网关需要 **full request body** 才能做的事，远不止提取 model：

1. **按 model 路由** — 需 `model` 字段选供应商/凭证。
2. **token 预估** — 需 messages 估算输入 token（计费/预扣）。
3. **缓存键** — 需全 body hash（精确缓存）或 embedding（语义缓存）。
4. **供应商格式转换** — OpenAI → Anthropic/Gemini/Bedrock 格式需全 body。
5. **故障转移重放** — 需全 body 才能重发到另一供应商（new-api 用 "body storage" 就是为这个）。
6. **用量/成本核算** — 需解析响应 usage。
7. **敏感内容审核** — 需消息内容扫描。

**结论**：对 LLM 网关来说，"需要 full body" 不是 model 提取一个问题——它是一个**类别**的需求。这正是业界一致选择 terminate 的根因。

## STOA 先例：评估了 2-tier sidecar 并明确拒绝

STOA 的 ADR-058 评估了四种 Pingora 集成方式：

| 方案 | 描述 | 结论 |
| --- | --- | --- |
| A 全迁移 | 用 Pingora server 替换 axum | 拒绝（重写 400+ 路由） |
| **B Sidecar（= 用户提案的 2-tier）** | **Pingora 前置 → stoa-gateway** | **拒绝："extra hop, thin value"** |
| **C Embedded** | Pingora connector 嵌入 axum 内 | **采纳** |
| D 仅抄模式 | 不用 crate | 被 C 取代 |

> STOA 拒绝 sidecar（2-tier）的原话："One binary — no sidecar, no extra hop, no deployment complexity"。

**这与你（用户）提的 2-tier 方案是同一个东西。** STOA 评估后拒绝了它。

## tokenmiser 模式：1-tier terminate-in-Pingora（业界已验证的解法）

tokenmiser-proxy 基于 Pingora，但**不做反向代理**——在 `request_filter` 里终止请求：

```rust
async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
    // 读全 body → 路由 → 调供应商（自己的 HTTP client）→ 流式回写 → Ok(true)
    Ok(true) // Pingora 永远不拨号 sentinel upstream
}
```

> 来源：tokenmiser crate 文档——"Requests terminate in the gateway rather than reverse-proxying: cache, cost accounting, shadow A/B and the judge all need the full response in hand."

**这就是"拿到完整 request 后所有分析都好做"的正确实现方式**——但它在一个进程内，不需要独立的 router 上游。STOA 的 embedded（Option C）本质相同。

## 性能数据（terminate 的代价到底多大）

| 网关 | 语言 | 架构 | 每请求开销 | 吞吐 |
| --- | --- | --- | --- | --- |
| LiteLLM Rust | Rust | terminate | ~0.05 ms | 6,782 req/s |
| TensorZero | Rust | terminate | 0.37 ms mean, 0.94 ms p99 | 10,000 QPS |
| Ferro Labs | Rust | terminate | 0.002 ms（bare） | 13,925 req/s |
| LiteLLM Python | Python | terminate | ~7.5 ms | 453 req/s |
| Pingora（Cloudflare） | Rust | **stream-through** | sub-ms | 1T req/day |

**关键数据（Vercel AI Gateway）**：16,000 总运行小时中，仅 1,200 小时是 CPU 工作，**14,800 小时在等供应商响应**。即 terminate 的 CPU 开销占 wall time ~7.5%——**在 LLM 请求的秒级延迟面前几乎不可感知**。

**但 streaming tax 是真实的**（AIGatewayBench 方法论）：
- inter-chunk jitter 的 p99 比 mean 更重要——agent 流应平滑滴入，每几 chunk 卡 40ms 会让编辑器卡顿。
- chunk 重新分块/合并（coalescing）会破坏 token-by-token 体验。
- Rust（无 GC、无缓冲）的 streaming path 能让 p99 inter-chunk gap 随并发上升几乎不增宽——这是 Rust 网关（terminate）的优势。

> 来源：`litellm-rust-launch`；`tensorzero benchmarks.mdx`；`ferro-labs benchmarks`；`vercel.com/blog/how-ai-gateway-runs-on-fluid-compute`；`ai-gateway-bench WHAT_THE_BENCH_TESTS.md`。

## 对你的 2-tier 提案的直接含义

1. **你的直觉是对的**：LLM 路由需要 full body（业界共识）。当前 first-chunk memchr 确实不够（Oracle 报告已确认）。
2. **但"独立成高性能上游"（2-tier）不是业界做法**：STOA 评估了完全相同的 sidecar 方案并拒绝（"extra hop, thin value"）。业界用 **embedded**（单进程内 terminate）。
3. **Pingora 的价值在 2-tier 下被浪费**：Pingora 的核心优势（零拷贝 stream-through + 跨 worker 共享连接池）在 2-tier 下毫无意义（只有一个内部 upstream；router terminate 了 body）。Pingora 退化成 TLS-term + auth + 静态转发——用 rustls+hyper 就能做到。
4. **如果决定放弃零拷贝**（业界常态），更好的选择是 **1-tier terminate-in-Pingora（tokenmiser 模式）**：单进程、无 hop、保 Pingora TLS/H2/优雅升级、full body 访问、用 embedded hyper 调供应商。比 2-tier 少一个进程、少一个 hop、不浪费 Pingora。

## 三条路（综合 Oracle 报告 + 行业实证）

| 方案 | 零拷贝 | 解决 late-model | 复杂度 | 代价 | 业界先例 |
| --- | --- | --- | --- | --- | --- |
| **A. 1-tier 零拷贝 + read-until-found** | ✅ 保留 | ✅（64KB 内） | 低 | 仅改一个 loop | 无（Hydra 独有折中） |
| **B. 1-tier terminate-in-Pingora（tokenmiser）** | ❌ 放弃 | ✅ 彻底 | 中 | full body 缓冲 + 放弃零拷贝 | tokenmiser / STOA embedded |
| **C. 2-tier（你的提案：edge + 独立 router）** | ❌ 放弃 | ✅ 彻底 | 高 | 额外 hop + 浪费 Pingora + 双进程 | STOA **拒绝** |

**推荐**：
- 若**零拷贝是硬需求**（你曾"强制"要求）→ **方案 A**（read-until-found，32KiB cap，先加 `hydra_model_extract_miss_total` 指标量实际 victim 流量比例再定）。
- 若**可以放弃零拷贝**（接受业界常态）→ **方案 B**（1-tier terminate-in-Pingora），**不是方案 C**。

**方案 C（2-tier）在任何维度上都不优于方案 B**——它解决的问题一样（full body），但多了 hop、多了进程、浪费了 Pingora。唯一适用场景：edge 与 router 物理分离部署（edge 在 CDN 边缘，router 在单 region）——但这不是当前场景。

---

> 完整的第一性原理分析（零拷贝代价量化、auth 放置、failover/usage 迁移、代码引用）见 Oracle 报告 [`architecture-2tier-analysis.md`](architecture-2tier-analysis.md)。
