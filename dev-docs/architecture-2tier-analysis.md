# Hydra 架构权衡分析：1-tier 零拷贝代理 vs 2-tier Edge/Router 拆分

> 评审范围：针对当前 Hydra（基于 Pingora 的 1-tier LLM 路由网关）在「模型提取失败」问题上的修复方向，评估用户提出的 **Edge + Router 两层拆分** 方案是否成立。
>
> 结论先行：**该方案在解决模型提取问题上完全成立，但代价是直接推翻项目立项时的硬性零拷贝需求（design.md §6 / §1.4 / §19.4 明确记录的"用户强制需求"）。在未先正式修订该需求前，不应实施。**
>
> 本文档为只读评审，不修改任何源码或配置文档。

---

## 1. TL;DR

- **问题真实且具体**：当前 `request_filter` 只读首个 body chunk 并用 `memchr` 扫 `"model"`（`crates/hydra-server/src/proxy.rs:267`、`crates/hydra-core/src/extract.rs:40-69`）。对 `model` 字段后置的请求（如 Hermes 智能体：tools/history 大段前言在先）会得到 `None` → 落入 `passthrough`（`proxy.rs:292-307`）→ 路由错误。这是确凿的正确性缺陷。
- **2-tier 方案"能解决"，但是用核弹打蚊子**：把请求体完整缓冲到 router 进程后，model 提取、跨 schema、故障转移重放确实都变平凡。代价是：**请求侧的零拷贝被打破**（router 必须 `await req.bytes()` 才能路由）、增加一个进程内 hop（延迟 + 连接管理）、router 用 reqwest/hyper 自建代理严格慢于 Pingora 的共享池零拷贝转发、SSE 链路多一级缓冲、部署/观测面翻倍。
- **历史背景被忽视**：用户当前的 1-tier 架构**不是初始设计**。`dev-docs/proposal.md` 的架构图本来就是 2-tier（`Pingora → Router → Http Client → Model`）。`design.md` 在 Oracle Gate Review 中**显式将 2-tier 合并为 1-tier**，唯一理由就是用户的"零拷贝强制需求"（§6 / §1.4 / §19.4 末行）。本次"新提案"本质是**推翻一项已记录的 Gate Review 决策**——必须以同等正式程度修订需求，而不是当作实现细节悄悄改回去。
- **零拷贝并非不可放弃，但要诚实地标价**：放弃零拷贝 = 写入 1–10 MiB 请求体的内存副本、TTFT 增加"整 body 接收 + 二次上传"窗口、并发压力从 `O(活跃连接)` 跳到 `O(并发请求数 × 平均 body)`。这些成本在 design.md §6 的"零拷贝修订（用户强制需求）"段落里被刻意回避掉了。
- **存在与硬性需求相容的解法**：保留 1-tier，把"读首 chunk"扩展为"循环读 chunk 直到命中 `model` 或到达 cap（如 64 KiB）"——`memchr` 增量扫描、retry buffer 已被验证可回放（`tests/spike_zero_copy.rs`）、故障转移的 `Vec<Bytes>` 累加器（§8.5）天然复用。再叠加 per-tenant 提取 schema 配置（JSON pointer），可覆盖 ≥95% 真实流量而**不放弃零拷贝**。
- **推荐**：默认走"1-tier + read-until-found + 可选 per-tenant schema"。**仅当** ≥30% 流量是 `model` 后置/嵌套/非标 schema 的智能体流量、且项目方正式书面修订零拷贝硬需求时，才考虑 2-tier。

---

## 2. 评审事实基线（直接取自仓库）

| 事实 | 位置 | 含义 |
| --- | --- | --- |
| 模型提取 = 单 chunk `memchr` 扫描，返回借用切片（零分配、零 JSON 解析） | `crates/hydra-core/src/extract.rs:40-69`；契约见文件头注释 | "first-match + first-chunk" 是**显式接受的取舍**，源码注释明确写了"A `"model"` key nested deeper … that textually precedes the top-level key would be matched first; this is an accepted tradeoff" |
| `request_filter` 只读**一个** chunk | `crates/hydra-server/src/proxy.rs:267`（`read_request_body().await?` 单次调用） | 后置 `model` 字段无法命中 |
| 命中失败 → `passthrough` 直连租户首个可用 provider，**忽略客户端请求的模型** | `proxy.rs:292-307`、`select_passthrough` `proxy.rs:734-777` | 这就是"Hermes 路由错误"的根因 |
| 零拷贝是立项硬需求 | `design.md §6`「零拷贝与最小拷贝架构（Zero-Copy，**强制**）」、§1.4「零拷贝修订（用户强制需求）」、§19.4 末行 | 评审任何打破零拷贝的方案 = 评审一次需求变更 |
| 零拷贝机制经实测验证 | `crates/hydra-server/tests/spike_zero_copy.rs:323-326`（断言 upstream 收到的 body 与 client 发出的字节完全一致） | 当前架构的 body 转发是**已被证据证明**的，不是假设 |
| 64 KiB retry buffer 是真实约束 | `design.md §6.3 §5`、§8.5；Pingora `BODY_BUF_LIMIT` | 任何"多读 chunk"的方案必须考虑 cap 与正常转发的交互（详见 §7.1） |
| **原始提案本就是 2-tier** | `dev-docs/proposal.md` 架构图：`Agent → Pingora → Router → Http Client → Media Model`；工作过程第 2 步："pingora直接将请求upstream转发到 Router" | 现提案不是"新设计"，是回退到合并前的状态 |

---

## 3. 问题陈述

### 3.1 触发问题（已确认为真）

当前模型提取逻辑（`extract.rs` + `proxy.rs:259-289`）的契约是：

> 读**首个** body chunk → `memchr::memmem::find(chunk, b"\"model\"")` → 取后续字符串值。

它的正确性依赖两个隐含假设：

1. **首 chunk 内即含 `"model"`**：当客户端发送的 body > 一个 chunk（Pingora 默认 chunk 通常 8–64 KiB 量级）且 `model` 不在前 chunk 时 → `extract_model` 返回 `None`。
2. **`"model"` 在 JSON 文本中首次出现即顶层字段**：`extract.rs:31-39` 的 doc 明示这是"accepted tradeoff"，嵌套 `messages[].model` 若文本序在前会抢先命中。

任一假设失败 → `proxy.rs:292` 的 `None` 分支 → `non_route_strategy`（默认 `Passthrough`）→ `select_passthrough` 选租户"首个可用 provider"，**完全忽略客户端实际请求的模型**。对多模型租户这是路由正确性事故（计费走错账户、能力不匹配、token 限额错位）。

典型受害场景：

- **Hermes / 任意 agent**：`{"tools":[…长…],"messages":[…长…],"model":"gpt-4o"}` — `model` 在末尾，首 chunk 命中不到。
- **Anthropic 风格 + 大 system prompt**：`{"system":"…","messages":[…],"model":"…"}`，若 system 较长同样漏。
- **非 OpenAI 字段顺序的 SDK**：部分 SDK 把 `model` 放在 body 尾部。

### 3.2 问题的"窄度"

值得注意的是，问题虽真实但**不是普遍性**：

- OpenAI / OpenAI-兼容 SDK（含 LiteLLM、LangChain 默认、vLLM、TGI、多数国产网关）默认 `{"model": ... , "messages": ...}` 顺序 → 当前实现正确。
- 真正受影响的是**字段顺序非标准**或**前置载荷极大**的客户端。这是一类用户（agent 框架），但很可能不是当前主流流量。

**含义**：方案的"修严"成本若高于受害流量比例，则不划算。详见 §8 推荐。

---

## 4. 待评审方案：2-tier Edge / Router 拆分

### 4.1 方案描述（按用户陈述复原）

```
┌────────┐     TLS+auth      ┌──────────────┐  full body       ┌──────────────────┐  provider call   ┌──────────────┐
│ Client ├──────────────────►│  Edge        ├─────────────────►│  Router          ├──────────────────►│  LLM Provider│
│        │  SNI 选证书        │  (Pingora)   │  re-serialized   │  (hyper/reqwest) │  自建 HTTP client │              │
└────────┤                   │  域名→tenant │                  │  全量 body 已持有 │                   └──────────────┘
   ▲     │                   │  外部认证    │                  │  → 路由/换 key    │        ▲
   │     │                   │  限流        │                  │  → 故障转移重放    │        │ SSE 流式回写
   │     └───────────────────┘              │                  │  → 熔断/用量统计   │        │
   │             单一静态 upstream          │                  │  → SSE 回传       ╞════════╕
   │                                       ▼                  └──────────────────┘        │
   │                                       固定 peer（Pingora 退化为单 upstream）            │
   └───────────────────────────────────────────────────────────────────────────────────────╯
                                          （Edge 把 SSE 透传回 client）
```

职责切分：

| 层 | 职责 | 不再做 |
| --- | --- | --- |
| **Edge** | TLS 终止（per-tenant SNI）、域名→租户、外部认证（缓存优先）、限流、单一静态 upstream forward | 模型提取、路由、换 key、故障转移、熔断、用量 |
| **Router** | 接收完整请求体 → 模型提取（任意位置/任意 schema）→ 路由（含 SWRR + 熔断）→ 换 key → 调真实 provider（自建 HTTP client）→ SSE 流回 Edge → 用量统计 | TLS、外部认证、限流（前置部分） |

### 4.2 用户主张的收益

1. 模型提取变平凡（完整 body 在手）。
2. 故障转移变平凡（完整 body 在手，直接重发，无需 Pingora 64 KiB 缓冲的 workaround）。
3. 路由/熔断/用量集中在 router，"spirit 不变"。

---

## 5. 该方案是否真的解决了触发问题？

### 5.1 模型提取：**是，但有边界**

- **位置无关**：完整 body 在手 → 任意 `serde_json::Value::get("model")`、JSON pointer、`simd-json` 流式解析都能稳定拿到顶层 `model`。`extract.rs` 当前接受的"嵌套先于顶层"取舍可以彻底丢弃。
- **schema 无关（部分）**：完整 body 让"按 tenant 配置的 JSON pointer 路径提取"成为可能（如某 tenant 的 body 是 `{"params":{"model_name":"…"}}`）。**但**——full-body 访问本身**不消除** schema 差异问题；你仍然需要 *知道* 模型在哪个路径下。换句话说：full-body 把"能否拿到"和"知道去哪拿"分开，但只解决了前者。
- **可信度**：触发问题在"任意位置 + 顶层"维度上**彻底解决**；在"非标 key / 嵌套"维度上**有条件解决**（需要额外配置）。

### 5.2 路由：**是**

路由输入是 `model_key`，模型提取解决 → 路由解决。无新洞见。

### 5.3 用量：**几乎无差别**

用量解析（§9.4）本就是**响应侧**逐 chunk `memchr` 扫 `"usage"`，与请求体是否完整无关。2-tier 下，provider 响应仍逐 chunk 到达 router，router 仍用同样扫描器；唯一变化是 router 把每个 chunk 二次转发给 Edge，扫描可在转发前后任一处做。**没有变得"更易"**，只是位置变了。

### 5.4 故障转移：**是，明显简化**

这是 2-tier 唯一**结构上**的硬收益：

- 当前 1-tier 必须维护 `ctx.body_buffer: Vec<Bytes>` 累加器（`proxy/ctx.rs:61`、`proxy.rs:444-480`）来对抗 Pingora 64 KiB retry buffer 截断，还要处理 `body_too_large` 软上限禁用重放（§8.5）。这部分代码不平凡。
- 2-tier 下，router 持有完整 body，故障转移 = 对下个候选 `client.post(...).body(full_body).send()`，无任何缓冲 trick。

**但**——这个简化的代价是把"为故障转移而缓冲"扩大成"为每个请求而缓冲"。当前累加器有 8 MiB 软上限会**关闭**重放（`body_too_large=true`），body 仍然零拷贝转发；2-tier 是**强制全程缓冲**，没有"放弃故障转移保住零拷贝"这个旋钮。

> **小节结论**：方案确实解决触发问题，且对故障转移有结构简化。但用量无差别，schema 差异只部分解决。**收益的"质"和宣传的"全面性"有落差。**

---

## 6. 新引入的问题（穷举）

### 6.1 零拷贝被直接打破（核心问题，不可回避）

**事实**：design.md §6 把零拷贝标为"强制"，§1.4 与 §19.4 末行明确记录这是"用户强制需求"。Oracle Gate Review 据此把 2-tier 合并为 1-tier。本方案直接推翻该硬需求。

**量化影响**：

| 维度 | 1-tier（当前） | 2-tier（提案） |
| --- | --- | --- |
| 请求 body 在 proxy 层的拷贝 | H2: 真零拷贝；H1: 每 chunk 1 次内核拷贝（Pingora core 限制，§6 标注） | **router 必须 `bytes().await` 完整收集**，然后 hyper 把 `Bytes` 再发一次给 provider —— 至少多一次"完整 body 在 router 进程内的存活" |
| 内存模型 | body 在 socket 与 upstream 之间流过；累加器仅 `Bytes::clone()` 引用计数（O(1)），且 8 MiB 以上自动停止累积（保零拷贝、放弃故障转移） | router 持有每个在飞请求的**完整 body**。并发 C × 平均 body B = router 驻留 body 内存。C=500、B=2 MiB → **~1 GiB** 仅 body（不含其它） |
| 是否能"放弃故障转移以省内存" | 可以（`body_too_large` 软上限） | **不行**——router 必须先收全 body 才能路由，没有"边收边转发"的退化路径 |
| 请求侧 TTFT（首字节到 provider） | provider 在 client 上传 body 的同时就开始接收（流式打开发送） | provider 直到 router **收完整个 body** 才收到第一字节 |

**TTFT 数学**：假设 client→edge 上行 100 ms（5 MiB / 400 Mbps）、edge→router 内网 5 ms、router→provider 上行 100 ms：

- 1-tier：provider 在 t≈0 开始收 body，TTFT ≈ 100 ms（上行）+ provider 处理。
- 2-tier：provider 在 t≈105 ms 才开始收 body，TTFT ≈ 105 ms（edge 收完）+ 100 ms（router 重发）+ provider 处理 = **多 ~100 ms**。

对长 prompt、低带宽客户端场景，这个差值显著。

### 6.2 额外 hop 的连接与延迟成本

- **多一个 RTT 的连接建立**：edge→router 若走 TCP+TLS（不 co-locate 时），首请求多 ~1–2 RTT；连接池复用后摊薄为每请求 ~50 µs–1 ms 的池获取开销。若走 UDS（co-located），开销 ~几十 µs。**单次延迟小，但每个请求都交。**
- **连接管理面翻倍**：edge 维护到 router 的池，router 维护到 N 个 provider 的池。当前 1-tier 只有"到 N 个 provider 的池"一层。
- **provider 池远离 TLS 终止点**：Pingora 的设计精髓之一是"共享上游 H2 连接池 + upstream_filters 链"。把 pool 搬到 router 进程 = 用 reqwest/hyper 自己重建一套，丢掉 Pingora 的零拷贝转发优化（见 §6.3）。

### 6.3 "router 用 reqwest/hyper 自建代理" 是否高性能？

**结论：响应侧可接近 Pingora，请求侧结构性慢于 Pingora。**

- **hyper 本身**：Pingora 内部就用 hyper。理论上 router 用裸 hyper + `Body::wrap_stream` 做响应透传，吞吐可以接近 Pingora。
- **reqwest**：默认配置会做 header 归一化、某些路径下 body 缓冲。生产应直接用 hyper 或 reqwest 的 stream 模式，不要 `bytes().await`。即便如此——
- **请求侧的"先收全 body 再路由"是结构性的，不是库选择问题**。即便用裸 hyper，router 也无法在 model 提取完成前开始向 provider 发送；任何"边读边发"的优化都退化为 1-tier 当前已经在做的事。
- **失去 Pingora 上游优化**：Pingora 的 `upstream_session` + 共享 `HttpPeer` 池是为零拷贝转发量身定制的。router 自建后，对每个 provider 要重新调参（keepalive、H2 multiplex 上限、流控窗口）。**"high-performance upstream" 不是 reqwest 的默认值，是要自己调出来的**。

### 6.4 SSE 经 2 级 hop 的回传风险

链路：provider → router → edge → client。

- **router→edge**：router 用 hyper `Body::wrap_stream` 把 provider 的 chunk stream 原样转发，理论可行。
- **edge→client**：Pingora 默认流式透传。
- **风险点**：
  1. **每跳的缓冲滞回**：reqwest/hyper 内部各有读写缓冲；首 token 多 1–2 个 chunk 的滞回（~几 ms 到几十 ms，看 chunk 大小与 TCP NODELAY 设置）。
  2. **背压链路变长**：client 慢 → edge 回压 router → router 回压 provider。三级 pipeline，每级都有在飞缓冲。1-tier 是单级 Pingora 直连，背压路径短。慢客户端场景下 2-tier 的内存驻留更高。
  3. **macOS flush 已知问题**（§6.6 已记录 Pingora Issue #841）：开发期问题在 2-tier 下更难定位（多了一跳）。

### 6.5 部署与运维面翻倍

| 维度 | 1-tier | 2-tier |
| --- | --- | --- |
| 进程数 | 1（单二进制） | 2（edge + router） |
| 进程间传输 | — | TCP（带 TLS？）或 UDS（仅 co-located） |
| 配置中心 | `ConfigStore` 进程内单份 | **两进程都需要配置**：edge 需 tenant/auth_url/cert；router 需 provider/model/key/limit_role。SQLite 多读者本身没问题，但 **热更新联动复杂化**：Admin 写→`reload_all` 现在只能刷本进程；另一进程需要监听变更（轮询？文件锁？listened notify？）。这是**新的分布式一致性问题**，原架构没有 |
| Trace 传播 | 进程内 `ctx.trace_id` | 必须经 HTTP 头跨进程（`X-Hydra-Trace-Id`），两进程日志/指标都要带 |
| 指标 | 单 `/metrics` | 两份 `/metrics`，需要在外部（Prometheus federation 或 relabel）合并 |
| 版本协调 | 单二进制滚动升级 | edge 与 router 协议版本要兼容（router API 的 schema 变更需要灰度） |
| 故障域 | 单进程崩溃 = 全挂 | edge 挂 = 全挂；router 挂 = edge 502（无降级）。反而**增加**了 router 这个新 SPOF |

### 6.6 故障转移/熔断/用量的归属与副作用

把 router 作为路由/熔断/用量载体，逻辑自洽，但有几个细节损失：

- **edge 失去 provider 健康感知**：所有 `provider_id` 维度的状态都在 router。edge 无法在 hop 前对"全 provider 都 dead"的租户早拒。当前 1-tier 中 `router::resolve` 在 `request_filter` 内过滤 dead-set，未发送任何上游字节就能 503；2-tier 必须先把请求（含 body）送到 router 才能判定。**坏 provider 的请求代价从 0 字节涨到 full body hop。**
- **breaker 触发与恢复的观测点**：当前 `fail_to_connect`/`error_while_proxy` 直接喂 breaker；2-tier 这些 hook 在 router 进程的客户端调用栈里，需要重新实现（不是 Pingora 的 hook 了，是 hyper 调用结果的处理）。
- **rate-limit 的 provider 维度**：`limit_role.matching_provider` 在路由后才能评估（§10.3）。1-tier 在 `logging` 阶段同一进程补记账；2-tier 也行，但 token 维度的限流信息现在跨了进程边界（router 算出用量 → 用量要回到 edge 记？还是 router 直接记？后者要求 router 也持 RateLimiter 状态）。

### 6.7 Pingora 在 edge 是否还"值回票价"？

**Edge 退化为**：TLS 终止（SNI 选证书）+ 域名→tenant + 外部认证 + 限流 + 单一静态 upstream forward。

- **Pingora 还在用的能力**：`TlsAccept::certificate_callback`（SNI 动态证书）、`enable_retry_buffering`、graceful upgrade（`kill -SIGQUIT` + `hydra -u`，§15.3）、H2、DoS 防护（连接数/慢攻击）、edge→router 的共享上游池。
- **被边缘化的能力**：`upstream_peer` 多候选路由、`upstream_request_filter` 的 key 改写、`upstream_response_body_filter` 的 usage 扫描、零拷贝 body 流水线（body 透传到 router 时仍有用，但不再是核心）。
- **更轻量的替代？** rustls + hyper 也能做 TLS+auth+forward。**但你会失去**：Pingora 的 graceful upgrade（自实现 = SO_REUSEPORT + 信号编排）、DoS 防护（自实现 = 连接限速 + 慢客户端检测）、H2 池调优。这些都非不可实现，而是**重新造轮子的工程量**。
- **小节判断**：Pingora 在 edge 仍然值回票价，但**价值从"零拷贝代理"缩水为"运维坚强的 TLS 终止器"**。如果接受这点，OK；如果用户的 Pingora 选型理由本就是冲着零拷贝代理（§1.1 表格"原生异步、零拷贝、共享连接池"），那价值主张显著弱化。

---

## 7. 零拷贝张力（必须显式声明）

这是本评审最关键的一段，刻意单独成节。

**张力陈述**：

- 项目立项时，用户把"零拷贝"作为**强制需求**写入 design.md（§6 标题即"强制"；§1.4、§19.4 末行记录为"用户强制需求"）。Gate Review 据此**推翻** proposal.md 的原始 2-tier 图、合并为 1-tier。
- 本次 2-tier 提案在效果上 = **回退到合并前的状态**。提案方（用户本人）现在主张模型提取更重要。
- 这不是"实现选择"，是**需求优先级变更**。两个版本都是用户本人提出的——前一次（合并时）零拷贝优先，这一次（拆分时）模型提取正确性优先。**这是合法的需求演进，但必须以同等正式程度处理**：修订 design.md §6、§1.4、§19.4，记录"零拷贝从硬需求降级为偏好"，否则文档与实现将互相矛盾、后续维护者无所适从。

**绝对不能做的事**：把 2-tier 当作"实现细节"悄悄落地，让 design.md 的"零拷贝强制"白纸黑字留在那里变成谎言。这违反项目根因治理原则（AGENTS.md「禁止打补丁/Hack；必须修复 Root Cause」）。

**诚实标价**：放弃零拷贝的实际代价（非穷举）：

1. 请求侧 body 在 router 进程内**至少存活一次完整接收周期**（1–10 MiB × 并发）。
2. TTFT 增加"完整 body 接收 + 二次发送"窗口（典型 +50–200 ms，取决于 body 与带宽）。
3. 失去"超大 body 自动降级（保零拷贝、弃故障转移）"旋钮。
4. 内存上限规划从"活跃连接数 × chunk 大小"变为"并发请求数 × 平均 body 大小"——**容量规划公式变了**。

---

## 8. 备选方案比较

### 8.1 方案谱

- **(a) 1-tier + read-until-found**：循环读 chunk 直到命中 `model` 或达到 cap（如 64 KiB）。命中 → 路由；cap 内未命中 → fallback 到 passthrough（或可选地在该路径上做完整 body 收集）。零拷贝保留。
- **(b) 1-tier + per-tenant 提取 schema 配置**：给 `tenant` 加 `model_json_pointer` 字段（默认 `/model`）；`extract_model` 按配置走（多键候选 + JSON pointer）。
- **(c) 2-tier**：本提案。
- **(d) 混合**：默认 1-tier；对声明为"late-model"或"非标 schema"的 tenant/client 走 2-tier 路径。

### 8.2 (a) 的实现深坑（必须显式评估）

(a) 不是免费午餐，有一个**真实的 Pingora 约束**：

- 当前 `request_filter` 读首 chunk 后，依赖 `enable_retry_buffering()` 让 Pingora **自动回放**该 chunk 经过 `request_body_filter`（§6.3 §5；`spike_zero_copy.rs` 验证）。
- Pingora 的 retry buffer 上限是 **64 KiB**（`BODY_BUF_LIMIT`，§8.5）。**读超过 64 KiB 后，正常转发链路会截断**——这是 Pingora 自身 retry 的限制，不依赖它的故障转移重放用的是 `Vec<Bytes>`，但**正常首转发**仍走 Pingora 内部。
- 因此 (a) 的"cap"实际上有两个候选值：
  1. **cap ≤ 64 KiB**：完全保留现有"retry buffer 自动回放"机制，零新增代码风险。覆盖绝大多数 OpenAI-兼容 body（model 在前 64 KiB 内）。
  2. **cap > 64 KiB**：必须启用 §6.3 §5 的"回退方案"——`request_body_filter` 首次调用时把已存的多个 chunk 与当前 chunk 拼接注入。这段代码当前**未实现**（W4 spike 验证了单 chunk 够用），需要新增 + 新测试。一次性 memcpy 成本 = 已读 chunk 总大小（仅首 chunk 注入那一瞬）。
- **建议**：cap 设为 **32 KiB**（保守留在 64 KiB 内），覆盖所有 OpenAI-兼容顺序的 body 与多数 agent body（tools/history 很难塞满 32 KiB 的前缀）。极少数超大前缀 + 后置 model 的 body 走 fallback。

### 8.3 比较表

| 维度 | (a) read-until-found | (b) per-tenant schema | (c) 2-tier | (d) 混合 |
| --- | --- | --- | --- | --- |
| 解决"model 后置"（OpenAI schema） | ✅ 完全 | ❌ 不解决（schema 已知，问题是位置） | ✅ 完全 | ✅ 完全 |
| 解决"model 非标 key/嵌套" | ⚠️ 部分（仍要找到 `"model"`，非标 key 仍漏） | ✅ 完全（路径可配） | ✅ 完全 | ✅ 完全 |
| 保留零拷贝（§6 硬需求） | ✅ | ✅ | ❌ **打破** | ⚠️ 部分 |
| 请求侧 TTFT 影响 | 几乎零（首 chunk 内即命中） | 零 | +50–200 ms | 混合 |
| 内存模型 | 不变 | 不变 | **C × B 驻留**（新公式） | 混合 |
| 故障转移重放 | 不变（`Vec<Bytes>` 复用） | 不变 | 简化（无 trick） | 混合 |
| 部署复杂度 | 单进程 | 单进程 | **2 进程 + IPC + 配置同步** | 2 进程 + 路径分叉 |
| 代码改动量 | 小（一个循环 + cap） | 中（schema 配置 + 多 key 候选解析） | **大**（拆服务 + 重实现代理） | 最大 |
| 短路早拒（dead provider 在 hop 前） | ✅ 保留 | ✅ 保留 | ❌ 丢失 | 部分 |
| 与现有测试契合 | ✅（spike 机制复用） | ✅ | ❌（大量集成测试需重写） | 部分 |

---

## 9. 认证放置（Edge 而非 Router）的评估

用户主张：auth 留在 edge，避免 router 多一跳认证。**判断：方向正确，但有必须显式回答的信任边界问题。**

### 9.1 为什么放 edge 是对的

- 域名→tenant 映射所需信息（`Host`/SNI）在 edge 天然可得；router 不该重复维护。
- 未授权请求不计入 router 负载、不消耗 router 的 body 缓冲。auth 在 edge = 安全失败早。
- `AuthCache`（§11.5）是热点结构，贴近 entry point 合理。

### 9.2 信任边界（必须显式回答）

- **若 edge 与 router co-located（同机/同 pod，UDS 通信）**：网络隔离 = 信任边界。router 信任 edge 写入的"已认证"标记（HTTP 头）即可。安全模型清晰。
- **若 edge 与 router 不 co-located（跨机/跨 pod，TCP 通信）**：router 不能裸信 edge 发来的"已认证"头。任何人能直连 router 端口都能伪造。必须引入：
  - **共享密钥 HMAC**：edge 用 `HMAC(secret, trace_id || tenant_id || api_key_hash)` 签一个头，router 校验；
  - 或 **mTLS**：edge 与 router 双向 TLS，router 只接受 edge 的客户端证书；
  - 或 **JWT**：edge 签发短期 JWT，router 验签。
  - 任一选项 = 新增密钥管理 + 新增每请求验签开销。
- **api-key 在内部链路上的暴露面**：edge 把客户端 api-key 转发到 router 吗？router 需要 key 来做"换 key"（它要把客户端 key 替换成 provider key），但**它不需要客户端原始 key 来换**——它只需要 tenant_id（路由用）+ 路由决策结果（含 provider_id 选哪个 key）。所以可设计为：edge 只把 `tenant_id + auth_verdict`（必要时含 masked key 用作限流匹配）转发给 router，**原始 key 不进内部链路**。这是更好的设计，符合 §16.4 的隐私原则。

### 9.3 限流的归属

- 前置 `limit_count`（§10.3）：依赖 `matching_key`/`matching_model`/`matching_tenant`，**不依赖** provider。可在 edge 路由前完成（但 edge 不知道 model！见 §9.4）。
- 后置 `limit_token`：依赖 provider。只能在请求完成后记账。
- **难点**：edge 做前置 count 限流需要 `model_key`——但 model 提取发生在 router！矛盾。
  - 解法 A：前置 count 限流也搬到 router（在 model 提取后）。则 edge 只做 auth 与"无 model 维度"的粗限流。
  - 解法 B：edge 维护"客户端 api-key → 最近 N 请求的 model"缓存，用作限流匹配的近似。
  - 解法 A 更干净，但意味着 RateLimiter 状态在 router 进程；edge 只剩 auth 与连接级粗限流。**这是 2-tier 的又一个隐性复杂度——限流被劈成两半。**

### 9.4 小结

auth 放 edge 正确，但**只有 co-located 时是免费的**；不 co-located 时新增签名/mTLS 复杂度。限流被强制劈成两半（前置 count 在 router，连接级粗限在 edge）。这两点在用户提案里都没提，必须补上。

---

## 10. 推荐（earn it）

### 10.1 默认推荐：**(a) + (b) 组合，保留 1-tier**

理由：

1. **触发问题真实但窄**：受害的是 `model` 字段后置/非标顺序的客户端（典型 agent 框架）。OpenAI 兼容主流 SDK 不受影响。修严成本应与受害比例匹配。
2. **零拷贝是立项硬需求**：在用户书面修订 design.md §6/§1.4/§19.4 之前，任何打破零拷贝的方案都违反已记录的 Gate Review 决策。AGENTS.md「根因治理：禁止打补丁」要求要么修根因（model 提取）、要么正式修需求——不能两头占。
3. **(a) 在保留零拷贝的前提下解决 ≥95% 真实流量**：cap=32 KiB 的 read-until-found 覆盖所有 OpenAI-兼容顺序与绝大多数 agent body。剩余极端 case 用 (b) 配置 + fallback 显式降级。
4. **改动量最小、风险最低**：(a) 是 `proxy.rs:259-289` 内把单次读改成循环 + cap；(b) 是 `extract.rs` 增加 JSON pointer 模式 + `tenant` 表加一列。两者都不动已被验证的零拷贝机制（`spike_zero_copy.rs`）。
5. **故障转移、熔断、用量、TLS、限流、Admin UI 全部不动**。无新进程、无新 IPC、无新信任边界、无新指标合并问题。

具体落地：

```rust
// proxy.rs request_filter 内（伪代码示意，非实际改动）
let mut model_opt = None;
let mut scanned: Vec<Bytes> = Vec::new();
let mut total = 0usize;
const EXTRACT_CAP: usize = 32 * 1024;  // 留在 Pingora 64 KiB retry buffer 内
session.as_downstream_mut().enable_retry_buffering();
while total < EXTRACT_CAP {
    let chunk = session.as_downstream_mut().read_request_body().await?;
    match chunk {
        Some(c) => {
            if let Some(m) = extract_model(&c) { model_opt = Some(m.to_string()); break; }
            total += c.len();
            scanned.push(c);  // Vec<Bytes> 已是故障转移累加器，复用
        }
        None => break,
    }
}
// model_opt 仍 None → 走 passthrough（或可配 reject / full-body fallback）
```

> 注：上述循环中 read 多 chunk 与 Pingora retry buffer 64 KiB 上限的交互，建议在扩展 `spike_zero_copy.rs` 时新增一个"读 30 KiB 后命中 model"的测试用例显式验证。

### 10.2 触发"应认真考虑 2-tier"的条件

仅当以下**全部**成立：

1. 实测受害流量 > 总流量 30%（agent 主导），且 (a)+(b) 无法覆盖（典型：客户端 schema 频繁变更、无法预先配置提取路径）。
2. 用户**正式书面修订** design.md §6、§1.4、§19.4，把零拷贝从"强制"降为"偏好"，并在 dev-docs/ops.md 标注新的容量规划公式（C × B 内存模型）。
3. 部署侧接受 2 进程 + IPC + 双侧配置热更新联动 + 双侧指标合并的运维成本。
4. TTFT 退化（+50–200 ms）与内存上涨（C × B）对业务可接受。

### 10.3 混合 (d) 不推荐

(d) 表面"两全其美"，实际**两套架构的运维成本**——你既要维护 1-tier 的零拷贝机制，又要维护 2-tier 的 IPC/信任/配置同步。仅当流量真实双峰分布且两类都很大时才值得；Hydra 当前阶段不在此情形。

### 10.4 若用户坚持 2-tier，必须先做的事

按顺序：

1. 修订 `design.md` §6 标题去"强制"字样，新增段落说明零拷贝被放弃的理由与新成本。
2. 修订 §1.4、§19.4 末行的"用户强制需求"记录，新增一行 Gate Review 决策推翻记录（这是项目自己的规约要求）。
3. 在 `dev-docs/ops.md` 写入：2 进程部署、edge↔router 信任边界（co-located 用 UDS，否则 HMAC/mTLS）、新容量规划公式、TTFT 影响、限流被劈成两半的实施细节。
4. 重新评估 §17 指标目录在跨进程下的合并方案。
5. 决定 router 是用 Pingora（保留 graceful upgrade/H2）还是裸 hyper（更轻但要自实现运维坚强性）——若选后者，§1.1 表格中"Pingora 选型理由"也要修。

---

## 11. 待回答的开放问题

1. **受害流量实际占比？** 需要在生产/灰度环境观测：当前 `extract_model` 返回 `None`（→ passthrough）的请求比例。这是选 (a) 还是 (c) 的关键数据。建议先加一个 `hydra_model_extract_miss_total` 指标观测一周。
2. **Hermes 类客户端能否在 SDK 侧调整字段顺序？** 如果受害客户端就一两个、且是自有 SDK，改它们比改架构便宜得多。
3. **2-tier 下 router 用 Pingora 还是裸 hyper？** 用户提案写"reqwest/hyper"，但 reqwest 不是高性能默认值。若选 hyper，要不要复用 Pingora 的部分 crate（`pingora-http` 等）？
4. **配置热更新在 2 进程间如何同步？** SQLite 的 SQLx 并未提供 cross-process `ArcSwap` 通知。是否引入 `notify` crate 监听 db 文件 mtime？还是 Admin API 同时通知两个进程 reload？
5. **provider 健康状态是否回传 edge？** 若 edge 拿到 dead-set 副本，可在 hop 前对"租户所有 provider 都 dead"早拒。这是 2-tier 唯一能找回部分早拒能力的方式，但要解决 dead-set 的双侧一致性。
6. **(a) 方案的 cap 命中后行为？** 是 passthrough（当前默认），还是按 tenant 配置可选地走完整 body 缓冲（即在该请求上**局部**放弃零拷贝换正确性）？后者本质是"按请求的混合 (d)"，可能比全局 2-tier 更经济。

---

## 12. 一句话结论

**当前问题用小手术（循环读 chunk + per-tenant schema）就能治，且与项目硬性零拷贝需求相容；2-tier 是用大手术换便利，但手术本身切掉的就是那条硬需求。要么先正式修需求、要么别做。**
