# Hydra

[English](README.md)

> **受够了 Python LLM 网关——闲置就吃掉 20GB 内存，还在 OpenAI↔Anthropic 的有损转换里默默吞掉你的工具调用？**
>
> **Hydra 是基于 Rust + Pingora 的 LLM 路由网关，OpenAI 与 Anthropic 双协议原生直通——零转换、按租户 TLS、计量级用量分解（缓存 token + TTFT）——65 MiB 单体二进制，零 `unsafe` / `unwrap` / `panic`。**

**高性能 LLM 路由网关。** 同时支持 **OpenAI（`/v1/chat/completions`）与 Anthropic（`/v1/messages`）** 两种客户端协议，格式同构直通（按客户端路径端到端保持同一格式，不做 OpenAI↔Anthropic 转换），路由到上游模型供应商，提供按租户鉴权、加权负载均衡、故障转移、熔断、限流、细粒度用量计量（输入/缓存/输出 token + TTFT）、按租户 TLS。基于 Rust + [Pingora](https://github.com/cloudflare/pingora)。

## 亮点 Highlights

> 以下数据在 10 核机器 + 线程化 mock 上游实测（未触达任何真实付费上游）。完整方法学与 8 核 16G VPS 容量推算见[评测报告](docs/evaluation-report.html)。

| | 指标 | 说明 |
|---|---|---|
| ⚡ | **11,056 RPS** 峰值吞吐 | c=25，p99 = 4.39ms |
| 🪶 | **65 MiB** 满载内存 (RSS) | 18.6 → 65.4 MiB；占 16G 机器 < 0.4% |
| ⏱️ | **~0.3 ms** 单请求网关开销 | 相对 LLM 延迟可忽略 |
| 🛡️ | **0** 处生产 `unwrap`/`panic`/`unsafe` | 两个 crate 均 `#![forbid(unsafe_code)]` |
| 🔐 | provider 密钥 **AES-256-GCM** 落库加密 | fail-closed 启动；管理面永不返回明文 |
| 🧪 | **core 114 + server 173** 测试，`clippy -D warnings` 干净 | CI 硬门禁 |

**生产就绪度：9.2 / 10** —— 完整[评测报告](docs/evaluation-report.html)。

---

## 这是什么

Hydra 部署在你的 Agent/客户端与 LLM 供应商之间。一次请求：按域名解析租户 → 调用租户自有认证端点鉴权 → **读取完整请求体**（model 可从任意位置/schema 提取）→ 路由（模型 × 租户授权供应商，加权轮询）→ 换上供应商 key → 用自有 HTTP client (reqwest) 调真实供应商 → 流式回写响应 → 解析用量 token（含缓存 token）→ 记录。

```
Agent ──► Pingora ──► [解析租户 → 外部认证 → 读全body → 提取model
                        → 路由 → 换key → reqwest调供应商 → 流式回写SSE
                        → 解析用量(输入/缓存/输出+TTFT) → 记录]
```

供应商失败时，Hydra **自动故障转移**到下一个候选（body 已全量缓存，重放零成本 `Bytes::clone` O(1)）。

## 特性

- **终止模式代理（Terminate-in-Pingora）**：在 `request_filter` 内读取完整请求体（model 提取适用于任意位置/schema，不再受首 chunk 限制）；通过专用 reqwest client 调用供应商；SSE 响应经 Pingora session 流式回写。返回 `Ok(true)`，Pingora 不拨号 upstream。
- **路由**：模型名 → 供应商 ∩ 租户授权供应商；平滑加权轮询（Nginx SWRR）。
- **api-key 前缀绑定闸门**：按原始前缀把客户端 api-key 固定到指定供应商（`sk_aaa_*` → Provider A）；最长前缀优先，fail-closed（绑定供应商不可用 ⇒ 503，绝不回落）。
- **外部认证**：每个租户配置自己的 `auth_url`；Hydra 缓存判定 5 分钟，并提供失效接口（欠费/封禁由租户自决）。
- **故障转移 + 熔断**：failover 循环依次尝试每个候选供应商；连续失败触发 dead-set，后台探活恢复。全 body 重放 O(1)。
- **限流**：内存滑动窗口（请求数 + token），按角色，m/h/d 窗口。
- **用量记录**：可插拔 Sink（默认 SQLite，可选 ClickHouse）；**细粒度 token 分解**：`prompt_tokens`/`completion_tokens`/`total_tokens`/`cached_tokens`（OpenAI `prompt_tokens_details` + Anthropic `cache_read_input_tokens`）；**延迟指标**：`forward_latency_ms`（Hydra 自身开销）+ `ttft_ms`（首 token 延迟）。所有数字字段默认 0（无 NULL）。
- **按租户 TLS**：基于 SNI 的证书选择，热更新（BoringSSL/OpenSSL）。
- **管理 REST + UI**：全部配置实体增删改查、Prometheus `/metrics`、内嵌控制台。

## 部署

### Docker（推荐）

```bash
# 1. 交叉编译 linux/amd64 二进制 + 构建镜像
./environment/build.sh

# 2. 启动全栈（hydra + mock-tenant + clickhouse）
cd environment && docker compose up -d

# 3. 注册你的供应商（读取 secure/config.json）
python3 ../environment/init.py
```

### 源码编译

```bash
cargo build --release --features server
HYDRA_ADMIN_TOKEN=<token> ./target/release/hydra
```

## 配置

Hydra 通过**环境变量**启动（运行时），所有路由配置存于 **SQLite**（经管理 API 管理）。

| 环境变量               | 默认值                           | 用途                                                |
| ---------------------- | -------------------------------- | --------------------------------------------------- |
| `HYDRA_DB_URL`         | `sqlite:hydra.db?mode=rwc`       | SQLite 数据库位置                                   |
| `HYDRA_LISTEN`         | `0.0.0.0:8080`                   | 代理监听地址（配证书时用 `:443` 走 TLS）            |
| `HYDRA_ADMIN_ADDR`     | `127.0.0.1:8081`                 | 管理 REST + UI + `/metrics` 监听地址                |
| `HYDRA_ADMIN_TOKEN`    | —                                | 守护 `/api/v1/*` 的 Bearer token（**管理必填**）     |
| `HYDRA_ENCRYPTION_KEY` | —                                | 32 字节的 base64；落库加密 provider api-key（**必填**，缺失即拒启动）。生成：`openssl rand 32 \| base64` |
| `HYDRA_USAGE_SINK`     | `sqlite`                         | `sqlite` 或 `clickhouse`                            |
| `HYDRA_CLICKHOUSE_URL` | —                                | ClickHouse HTTP 端点（sink=clickhouse 时必填）      |
| `RUST_LOG`             | `info`                           | 日志级别                                            |

**端口**：`8080`/`443` 代理 · `8081` 管理（REST + UI + metrics）。

## 使用

### 管理界面

打开 `http://<host>:8081/admin/`，输入管理 token。管理供应商、模型、key、租户、授权、限流角色，查看/失效认证缓存与熔断器。

### 管理 REST

```bash
TOKEN=<你的管理 token>

curl -X POST http://localhost:8081/api/v1/providers \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"id":"openai","key":"openai","name":"OpenAI","endpoint":"https://api.openai.com","weight":1}'

curl -X POST http://localhost:8081/api/v1/tenants \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"id":"acme","name":"ACME","domain":"acme.example.com","auth_url":"https://auth.acme.example.com/v","enabled":true}'

curl -H "Authorization: Bearer $TOKEN" http://localhost:8081/api/v1/providers
curl http://localhost:8081/metrics
```

### 把客户端指向 Hydra

```bash
curl https://acme.example.com/v1/chat/completions \
  -H "Authorization: Bearer <客户端 api-key>" \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"你好"}],"stream":true}'
```

Hydra 按域名解析租户 → 调 `auth_url` 鉴权 → 路由 `gpt-4o` 到授权供应商 → 换 key → 流式回写 → 记录用量。

## 工程结构

```
crates/hydra-core/    纯领域逻辑（路由、SWRR、熔断、SSE 扫描、限流）——零 I/O 依赖
crates/hydra-server/  Pingora 代理外壳（终止模式）、DB、认证、用量 Sink、TLS、管理 API
environment/          Dockerfile + docker-compose + mock-tenant + 初始化脚本
integration/          Python CRUD 测试套件 + e2e 代理测试 + mock LLM/auth
docs/                 design.md、ops.md、dev-plan.md、架构分析
```

## 集群模式（Cluster Mode）

单节点零依赖；集群 = `HYDRA_ROLE=leader|edge` + Redis（唯一外置依赖），
K8s/k3s 无关、自维持（自动选举/故障切换/加入退出/自愈）。

```bash
cargo build --release --features server,cluster-redis,usage-clickhouse
cd environment && docker compose -f docker-compose.cluster.yml up -d --scale hydra-edge=2
```

已通过真实环境验收（双 leader 候选 + 无状态 edge，docker Redis）：故障切换
~11–18s、跨节点限流、共享熔断、认证失效总线、无共享卷证书轮换。
详见 **[`docs/cluster.md`](docs/cluster.md)**（环境变量表、Redis 故障矩阵、
故障切换演练与实测记录）与
[`environment/docker-compose.cluster.yml`](environment/docker-compose.cluster.yml)。

## 更多

- 设计与架构：[`docs/design.md`](docs/design.md)
- 架构变更（终止模式）：[`docs/design-change-terminate-mode.md`](docs/design-change-terminate-mode.md)
- 运维手册：[`docs/ops.md`](docs/ops.md)
- 交互式流程图：[`docs/workflow.html`](docs/workflow.html)

Rust 1.83+ · Pingora 0.8.x
