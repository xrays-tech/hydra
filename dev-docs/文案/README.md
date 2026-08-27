# Hydra 推广文案集

围绕 **Hydra**（Rust + Cloudflare Pingora LLM 路由网关）的推广文案，基于 2026 年 8 月的行业痛点调研产出。

## 文件清单

| 文件 | 用途 |
|------|------|
| [`行业痛点诊断报告.md`](行业痛点诊断报告.md) | 文案前置分析：行业现状 / 公认痛点 / 破局点。三套文案的共同弹药库 |
| [`V2EX-掘金-技术社区.md`](V2EX-掘金-技术社区.md) | 中文技术社区长文，含 2 个备选标题（痛点型 + 干货型） |
| [`Twitter-X-社交媒体.md`](Twitter-X-社交媒体.md) | X / 程序员群转发短文案，3 个版本（暴击型 / 中文极简 / 技术炫耀型） |

> README 顶部 Pitch Banner 已直接写入仓库根目录的 `README.md` 与 `README.zh-CN.md`，本目录不重复维护。

## 核心叙事主脊（所有文案的共同骨架）

> **LiteLLM**（市占第一、56K star）在 2026 年 6 月官宣全面 Rust 重写，官方给出的理由就是 Python 版的内存泄漏（生产用户原话 *"20GB of RAM… and that's being idle"*）、`input: {}` 翻译 bug（6 个 PR 反复修）、延迟毛刺与 OOM。
>
> **Hydra 出厂就是 Rust + Pingora**——龙头花一整年重写要变成的样子，我们今天就是。
>
> 一句话：*"The incumbent's roadmap is our launch day."*

## 投放节奏建议

1. **先发中文社区**（V2EX `/go/programmer` + 掘金同步），标题用「痛点控诉型」转化更高。掘金务必配 `dev-docs/evaluation-report.html` 的 benchmark 截图——量化对比是性能向项目的命脉。
2. **隔 12 小时再发 X**（英文圈），配两张图：一张 11K RPS 的 `oha` 输出，一张 65MB RSS 的 `top` 截图。两张图比一万字管用。
3. **README Banner** 已就位，老外一看就懂痛点（LiteLLM 的怨气全球通用）。

## 可信度边界（文案作者必读）

下列数字的可辩护性分级，写新文案时请遵守：

| 主张 | 数值 | 可辩护性 |
|------|------|----------|
| 单请求网关开销 | ~0.3 ms | **强**（基线相减测得，与供应商延迟无关） |
| 满载内存 RSS | 65.4 MiB | **强**（环境可移植，数字极小） |
| 0 unwrap/unsafe/panic + `forbid(unsafe_code)` | 两个 crate | **强**（静态事实，可秒验） |
| AES-256-GCM + fail-closed | — | **强**（静态事实） |
| 峰值 RPS | 11,056 @ c=25, p99=4.39ms | **中**（mock 上游 + 本地 10 核，必须带 caveat） |
| 稳定吞吐 | 3,500–4,000 RPS | **中**（"悬崖前的安全区"） |
| 8C16G VPS 容量推算 | 2,000–3,000 并发流 / 400–600 RPS 真实 LLM | **弱**（推算非实测，谨慎使用） |
| 生产就绪度 9.2/10 | — | **弱**（自评，对外请写「内部评测」） |

**铁律**：对外引用 11K RPS 时，必须带 *"mock 上游、本地 10 核机器实测"* 的限定，不要拿它对标厂商 SLA。诚实写在 `dev-docs/evaluation-report.html` 里了。
