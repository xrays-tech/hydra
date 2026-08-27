# Twitter / X · 程序员群转发短文案

> 高密度、带极客幽默、善用代码块与对比符号。三个版本按场景选用。

---

## 版本 A · 暴击型（适合 X 英文圈，配 benchmark 截图）

> LiteLLM ate 20GB of RAM doing nothing. The `input:{}` translation bug burned $400/night in retry loops. So they announced a full Rust rewrite for 2026.
>
> We skipped the wait. Hydra is Rust + Cloudflare Pingora, born native.
>
> 65 MiB RSS. ~0.3ms overhead. Zero protocol conversion — OpenAI in→OpenAI out, Anthropic in→Anthropic out. Per-tenant TLS. Cached-token billing that doesn't quietly bill cache hits as fresh input.
>
> ```
> 11,056 RPS @ p99=4.39ms  ·  0 unwrap/unsafe/panic
> 65 MiB RAM              ·  forbid(unsafe_code) on both crates
> ```
>
> The incumbent's roadmap is our launch day.
>
> 🔗 `<repo>`

---

## 版本 B · 中文群转发（极简毒舌）

> LiteLLM 闲置吃 20GB、`input:{}` 祖传 bug 一晚烧 400 刀，官方已认怂宣布 Rust 重写。
>
> Hydra 出厂就是 Rust + Pingora：65MB 内存、0.3ms 开销、双协议原生直通零转换、按租户 TLS、缓存 token 单独计费。龙头花一年重写要变成的样子，我们今天就是。
>
> ```
> 11,056 RPS · p99 4.39ms · 0 unsafe/unwrap/panic
> ```
>
> 🔗 `<repo>`

---

## 版本 C · 技术炫耀型（适合配架构图 / profiler 输出发）

> Hot take: your LLM gateway shouldn't parse your prompt.
>
> Hydra scans request/response bytes with SIMD `memchr` at memory-bandwidth speed — it only deserializes the ~50 bytes of `usage` metadata, and only when they actually show up. Zero JSON parsing on the hot path.
>
> Failover is a `for` loop. Body is reference-counted `Bytes`, so replaying to the next provider is a refcount bump, not a memcpy.
>
> ```rust
> for candidate in candidates {
>     if provider.send(req).await.is_ok() { break }
>     // body replay = Bytes::clone() = O(1), zero-copy
> }
> ```
>
> Built on the same framework Cloudflare uses for 40 trillion requests/day.
>
> 🔗 `<repo>` · benchmarks in README

---

## 配图建议

- **版本 A**：两张图并排——左边 `oha` 的 11K RPS 输出，右边 `top` 里 65MB RSS 的进程行。两张图胜过千言。
- **版本 C**：一张 `memchr` 扫描 vs `serde_json::from_slice` 的火焰图对比，或者 profiler 显示 99% chunk skip 的截图。
- **避免**：架构流程图（社交媒体上没人看）、纯文字长截图（X 折叠后等于没发）。
