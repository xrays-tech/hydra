# V2EX / 掘金 / 开发者头条 · 技术社区长文

> 中文技术社区投放用。建议标题用「痛点控诉型」；掘金务必配 `dev-docs/evaluation-report.html` 的 benchmark 截图。

---

## 标题（二选一）

- **痛点控诉型**：
  > 被 LiteLLM 的 20GB 内存泄漏和 input:{} 祖传 bug 搞破防，我用 Pingora 造了个 Rust 网关，满载 65MB
- **技术干货型**：
  > 用 Cloudflare Pingora 造 LLM 网关：双协议原生直通、O(1) 故障转移、SIMD 扫 usage 零 JSON 解析

---

## 正文

### 【起因：说多了都是泪】

老哥们，这玩意儿是被逼出来的。

去年我们给一个多租户的 AI 平台选网关，闭眼选了 LiteLLM——毕竟 56K star，文档厚得像字典，没道理出问题吧？结果上线第二周，监控就开始告警：一个啥流量都没有的凌晨，**Pod 内存干到了 9.6GB**，还在以 0.7GB/h 的速度往上爬。去看 issue，好家伙，#12685 里全是我们这种难兄难弟——*"20GB of RAM… and that's being idle"*，官方回复翻译过来就是「4GB 是地板，glibc 不还给系统，你们定时重启吧」。

定时重启我忍了。真正让我破防的是那个 `input: {}` bug：Claude Code 通过 LiteLLM 调 GPT，**每次工具调用 `command` 参数被吞成空字典**，然后客户端进入无限重试，一晚上烧了 400 刀 token。去翻 issue tracker，#12158 / #24134 / #25321 / #25561 / #27469……横跨 Gemini / Ollama / Vertex / Bedrock，**同一个 bug 在不同代码路径里修了 6 次**，每次「fixed」，下次发版又回来。本质问题就一句话：**OpenAI↔Anthropic 翻译这层，天生就是 bug 工厂。**

更阴的是——你以为只是工具调用坏？不，**Anthropic 的 `cache_control` 在转成 OpenAI 线格式时被默默剥光**，缓存直接失效，整轮对话按全价 input 计费，账单能贵 10 倍，你还毫不知情。Anthropic 自己官方文档都写了 *"for the best experience… use the native Claude API"*。潜台词：别用那帮转换代理。

Python、GIL、GC、翻译层——这哪是网关，这是给每个请求配了个收费站。HN 上有人总结得最到位：*"I'd sooner completely rewrite it in Golang/Rust or otherwise."*

巧了，LiteLLM 自己也这么想——**他们 2026 年 6 月官宣全面 Rust 重写**，目标 32MB、sub-1ms。

那我直接从 Rust 起步不行吗？于是就有了 Hydra。

### 【现状：造了个什么玩具】

Hydra，Rust + Cloudflare Pingora 写的 LLM 路由网关。一句话：**部署在你的 Agent 和供应商之间，干 LiteLLM 该干的那点活，但只吃 65MB 内存，还把转换层整个干掉了。**

```
Agent ──► Pingora ──► [解析租户 → 外部认证 → 读全body → 提取model
                       → 路由 → 换key → reqwest调供应商 → 流式回写SSE
                       → 解析用量(输入/缓存/输出+TTFT) → 记录]
```

核心设计决策：**不走 Pingora 默认的流式透传，而是在 `request_filter` 里终止请求、自己用 reqwest 调上游**。这不是我瞎搞——我们调研了 8 个生产级 LLM 网关，**没有一个用透传，全都是终止模式**。原因很简单：你要从 body 里提 `model` 字段做路由，而真实客户端（尤其那种塞一堆 system/tools 前缀的 agent）会把 `model` 挤到很后面。透传只能看首 chunk，看不全就提不到，路由就错。终止模式一把读全 body，问题消失。

### 【硬核亮点】

- **🪶 满载 65 MiB，~0.3ms 开销**：10 核机器 + 线程化 mock 上游实测（没打真实付费上游），c=25 峰值 **11,056 RPS，p99=4.39ms**。单请求网关开销 ~0.3ms，相对 LLM 延迟约等于不存在。作为对比——LiteLLM 的 Rust 重写目标是 32MB，我们今天就在这儿了。*（注：11K 是 mock 上游的合成峰值，真实 LLM 延迟主导时大约 400-600 RPS，别拿这个去对标厂商 SLA，老老实实写在 [评测报告](../../dev-docs/evaluation-report.html) 里了。）*

- **🔀 双协议原生直通，零转换**：OpenAI `/v1/chat/completions` 和 Anthropic `/v1/messages` **都是一等公民，端到端同一格式，绝不互转**。没有 `input: {}`，没有 `cache_control` 被剥，没有 thinking 被拍平。你的 Anthropic SDK 客户端打过来，body 字节级透传到 Anthropic-compatible 上游，响应字节级回来。

- **🛡️ 0 行 `unsafe` / `unwrap` / `panic`**：两个 crate 全部 `#![forbid(unsafe_code)]`（不是 `deny`，是 `forbid`，连局部覆盖都不行）。在代理这种「一次 panic = 一次客户可见故障」的位置，这不是洁癖，是基本盘。core 106 + server 163 个测试，`clippy -D warnings` 是 CI 硬门禁。

- **⚡ O(1) 故障转移 + Nginx 同款 SWRR**：供应商挂了？`for candidate in candidates` 直接重试下一个。因为 body 已经在内存里，`Bytes::clone` 是引用计数 +1，**零 memcpy**，重放免费。负载均衡直接手撸了 Nginx 的平滑加权轮询（Pingora 自带的那个不带权重）。外加正经熔断器：连续失败 → dead-set，后台探活恢复，不会出现「供应商一恢复，所有重试同时砸过去又把它打挂」的死亡螺旋。

- **🔎 SIMD 扫 usage，从不解析你的 prompt**：`memchr` 内存带宽速度扫 `"model"` 和 `"usage"` 关键字，99% 的 SSE chunk 直接跳过；**只在命中那 ~50 字节的 usage 切片时才反序列化**。你的 prompt 全程不被 parse。usage 拆 `prompt_tokens` / `completion_tokens` / `total_tokens` / `cached_tokens`（OpenAI `prompt_tokens_details` + Anthropic `cache_read_input_tokens` 都吃），延迟记 `forward_latency_ms`（网关自身）+ `ttft_ms`（首 token）。缓存 token 不会被当新 input 偷偷计费。

- **🔐 按租户 TLS + AES-256-GCM**：基于 SNI 的证书选择，热更新，为了这个专门选了 BoringSSL 而不是 rustls（rustls 不支持运行时 cert callback）。供应商 key 落库 AES-256-GCM 加密，**主 key 没配直接拒启动**（fail-closed），管理面永不返回明文。DB 文件被偷走，没主 key 也是废铁。

### 【谁可以白嫖】

- **被 LiteLLM 内存泄漏 / `input:{}` bug 折磨的运维老哥**——这就是给你写的
- **做多租户 AI 平台的团队**（需要按租户鉴权、按租户 TLS、按租户计量的）
- **重度用 Claude Code / Anthropic SDK 的**——你的工具调用和缓存终于不会再被转换层吃了
- **想在 8C16G 小机器上扛几千并发流式的**——RSS 占不到 200MB，内存够你跑别的
- **就是想要个不依赖 Postgres / K8s / SaaS 账号、单二进制能跑的网关的洁癖患者**

### 【传送门】

GitHub: `<你的仓库地址>`
评测报告（怎么测的、8C16G VPS 怎么推算的，全摊开了）：`dev-docs/evaluation-report.html`

---

> 如果觉得帮您省了哪怕一次半夜被 OOM 告警吵醒，求个免费的 Star 支持一下。开源不易，头发告急 🍵。也欢迎来 issue 区拍砖、提需求、贡献代码——尤其欢迎来现场表演把 LiteLLM 的 YAML 翻译成 Hydra 的管理 API。
