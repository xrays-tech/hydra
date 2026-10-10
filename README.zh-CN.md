# Hydra

[English](README.md) · [🌐 项目主页](https://xrays-tech.github.io/hydra/)

> **受够了 Python LLM 网关——闲置就吃掉 20GB 内存，还在 OpenAI↔Anthropic 的有损转换里默默吞掉你的工具调用？**
>
> **Hydra 是基于 Rust + Pingora 的 LLM 路由网关，OpenAI 与 Anthropic 双协议原生直通——零转换、按租户 TLS、计量级用量分解（缓存 token + TTFT）——满载运行约 65 MiB RSS，零 `unsafe`、零生产 `unwrap`/`panic!`；唯一的 panic 来源是 2 处断言不可达不变量的 `expect()`（均已登记，决策 D-13）。**

**高性能 LLM 路由网关。** 同时支持 **OpenAI（`/v1/chat/completions`）与 Anthropic（`/v1/messages`）** 两种客户端协议，格式同构直通（按客户端路径端到端保持同一格式，不做 OpenAI↔Anthropic 转换），路由到上游模型供应商，提供按租户鉴权、加权负载均衡、故障转移、熔断、限流、细粒度用量计量（输入/缓存/输出 token + TTFT）、按租户 TLS。基于 Rust + [Pingora](https://github.com/cloudflare/pingora)。

## 亮点 Highlights

> 以下数据在 10 核机器 + 线程化 mock 上游实测（未触达任何真实付费上游）。完整方法学与 8 核 16G VPS 容量推算见[评测报告](dev-docs/evaluation-report.html)。

| | 指标 | 说明 |
|---|---|---|
| ⚡ | **11,056 RPS** 峰值吞吐 | c=25，p99 = 4.39ms |
| 🪶 | **65 MiB** 满载内存 (RSS) | 18.6 → 65.4 MiB；占 16G 机器 < 0.4% |
| ⏱️ | **~0.3 ms** 单请求网关开销 | 相对 LLM 延迟可忽略 |
| 🛡️ | **0** 处生产 `unwrap`/`panic!`，**2** 处 `expect()` 不变量断言 | 两个 crate 均 `#![forbid(unsafe_code)]`；2 处 panic 来源均为「不可达不变量」断言——`main.rs`（cert store）、`proxy/provider_client.rs`。**计数 = `check_source_purity.cjs` 实测值；是可用守卫验证的声明，不是文案。** |
| 🔐 | provider 密钥 **AES-256-GCM** 落库加密 | fail-closed 启动；管理面永不返回明文 |
| 🧪 | **core + server** 测试（精确实测数见 [`docs/index.html`](docs/index.html)），`clippy -D warnings` 干净 | CI 硬门禁 |

**生产就绪度：9.2 / 10** —— 完整[评测报告](dev-docs/evaluation-report.html)。

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
- **用量记录**：可插拔 Sink（**`HYDRA_USAGE_SINK` 必填、无默认**：`clickhouse` 共享存储 或 `none` 显式不计量；旧 SQLite sink 已于 2026-10-07 退役并按名字拒启）；**细粒度 token 分解**：`prompt_tokens`/`completion_tokens`/`total_tokens`/`cached_tokens`（OpenAI `prompt_tokens_details` + Anthropic `cache_read_input_tokens`）；**延迟指标**：`forward_latency_ms`（Hydra 自身开销）+ `ttft_ms`（首 token 延迟）。所有数字字段默认 0（无 NULL）。
- **按租户 TLS**：基于 SNI 的证书选择，热更新（BoringSSL/OpenSSL）。
- **管理 REST + UI**：全部配置实体增删改查、Prometheus `/metrics`、内嵌控制台。

## 部署

### Docker（推荐）

```bash
# 两把 key 均必填——不 export 则 compose 直接失败
export HYDRA_ENCRYPTION_KEY="$(openssl rand 32 | base64)"
export HYDRA_ADMIN_TOKEN="$(openssl rand -hex 32)"

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
# HYDRA_ENCRYPTION_KEY 与 HYDRA_USAGE_SINK 均必填、无默认
HYDRA_ADMIN_TOKEN=<token> \
HYDRA_ENCRYPTION_KEY="$(openssl rand 32 | base64)" \
HYDRA_USAGE_SINK=none \
./target/release/hydra
```

## 配置

Hydra 通过**环境变量**启动（运行时），所有路由配置存于 **SQLite**（经管理 API 管理）。

| 环境变量               | 默认值                           | 用途                                                |
| ---------------------- | -------------------------------- | --------------------------------------------------- |
| `HYDRA_DB_URL`         | `sqlite:hydra.db?mode=rwc`       | SQLite 数据库位置                                   |
| `HYDRA_LISTEN`         | `0.0.0.0:8080`                   | 代理**明文**监听地址（恒定绑定，不受证书影响）      |
| `HYDRA_TLS_LISTEN`     | *（未设置）*                      | 可选的代理 **TLS** 监听地址，如 `0.0.0.0:8443`。**设置它**（而不是租户是否配了证书）才会创建 HTTPS 监听；租户证书按 SNI 选择。 |
| `HYDRA_ADMIN_ADDR`     | `127.0.0.1:8081`                 | 管理 REST + UI + `/metrics` 监听地址                |
| `HYDRA_ADMIN_TOKEN`    | —                                | 守护 `/api/v1/*` 的 Bearer token（**管理必填**）     |
| `HYDRA_ENCRYPTION_KEY` | —                                | 32 字节的 base64；落库加密 provider api-key（**必填**，缺失即拒启动）。生成：`openssl rand 32 \| base64` |
| `HYDRA_USAGE_SINK`     | —（**必填**）                     | `clickhouse`（共享用量存储）或 `none`（显式不计量）。`sqlite`（旧的单节点默认）已于 2026-10-07 退役并**按名字拒绝启动** |
| `HYDRA_CLICKHOUSE_URL` | —                                | ClickHouse HTTP 端点（`HYDRA_USAGE_SINK=clickhouse` 时必填） |
| `RUST_LOG` | *未设* | 日志级别；未设时进程内默认按 tracing 默认 `error`——`info` 为部署推荐值 |

> **单节点 vs 集群的唯一开关是 `HYDRA_CLUSTER_PEERS`**：不设置 = 单节点（上表即可）；设置（≥3 项、三节点逐字相同）= 进入 raft 集群，此时还需：

| 环境变量 | 默认 | 用途 |
|---|---|---|
| `HYDRA_CLUSTER_PEERS` | —（**集群必填**） | 静态成员表，`name=host:port` 逗号分隔；**顺序即 raft id**；≥3 项；所有节点逐字相同 |
| `HYDRA_NODE_ID` | —（**必填**） | 本节点在成员表里的名字，各节点唯一 |
| `HYDRA_ARACHNE_LISTEN` | —（**必填**） | 本节点 raft 传输地址，须等于成员表里自己那一项 |
| `HYDRA_CLUSTER_ID` | 成员表哈希（**建议显式设置**） | 集群身份；显式设置后日后可安全改成员表 |
| `HYDRA_REDIS_URL` | —（**必填**） | 数据面共享状态骨干（Redis 是集群的依赖之一，另一为用量 sink） |
| `HYDRA_REDIS_MODE` | `single` | 仅集群读取，只接受 `single` |

**端口**：`8080` 代理（明文，恒定绑定）· `8443` 代理（TLS，仅当设置 `HYDRA_TLS_LISTEN`）· `8081` 管理（REST + UI + metrics）。

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
curl -H "Authorization: Bearer $TOKEN" http://localhost:8081/metrics
```

### 把客户端指向 Hydra

```bash
curl https://acme.example.com/v1/chat/completions \
  -H "Authorization: Bearer <客户端 api-key>" \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"你好"}],"stream":true}'
```

Hydra 按域名解析租户 → 调 `auth_url` 鉴权 → 路由 `gpt-4o` 到授权供应商 → 换 key → 流式回写 → 记录用量。

**模型目录**：`GET /v1/models` **无条件公开可读**（带不带 api-key 均可访问——出示的 key 不会触发鉴权，仅按 key 前缀绑定收窄目录），返回**该租户当前可调用模型的目录**——Hydra 本地聚合（跨授权 provider 并集、按租户模型白名单过滤、剔除熔断/软禁用/无 key 的 provider），不再直通单一上游；聊天等调用仍须 api-key。管理端另有只读聚合端点 `GET /api/v1/tenants/{tenant_id}/models`（admin token；含每个 provider 的在线状态，供运维诊断）。详见 `dev-docs/design-tenant-model-catalog.md` 与 `dev-docs/aegis/plans/2026-09-08-public-models-catalog.md`。

## 工程结构

```
crates/hydra-core/    纯领域逻辑（路由、SWRR、熔断、SSE 扫描、限流）——零 I/O 依赖
crates/hydra-server/  Pingora 代理外壳（终止模式）、DB、认证、用量 Sink、TLS、管理 API
environment/          Dockerfile + docker-compose + mock-tenant + 初始化脚本
integration/          Python CRUD 测试套件 + e2e 代理测试 + mock LLM/auth
dev-docs/                 design.md、ops.md、dev-plan.md、架构分析
```

## 集群模式（Cluster Mode）

单节点零外置依赖；**集群唯一开关是 `HYDRA_CLUSTER_PEERS`**（静态成员表，`name=host:port` 逗号分隔）——
设了它（≥3 项）本进程就是 raft 集群成员，不设即单节点、行为零变化。成员表**顺序即 raft id**，
所有节点的值必须逐字相同。**最少 3 个成员**（`MINIMUM_MEMBERS=3`，少于 3 项在解析成员表时就拒绝启动）。

集群是 3 个**完全同构**的 raft 成员（都跑数据面/管理面/本地 SQLite/raft），**没有 edge、没有可
`--scale` 的角色**；任意时刻**恰好一个 writer**，由 raft 写探测决定。控制面是 Arachne（raft 线性化
KV），Redis 只承载数据面近似状态。

集群构建/运行：

```bash
# 需含 arachne feature，否则设了 HYDRA_CLUSTER_PEERS 会拒启
cargo build --release --features server,cluster-redis,arachne,usage-clickhouse
export HYDRA_ADMIN_TOKEN="$(openssl rand -hex 32)"          # 每个节点都用
export HYDRA_ENCRYPTION_KEY="$(openssl rand 32 | base64)"   # 全集群一致
cd environment && docker compose -f docker-compose.cluster.yml up -d   # 3 个同构成员，勿 --scale
```

每节点必填：`HYDRA_CLUSTER_PEERS`（三处相同）、各自不同的 `HYDRA_NODE_ID` 与
`HYDRA_ARACHNE_LISTEN`（且后者须等于成员表里自己那一项）、`HYDRA_REDIS_URL`、`HYDRA_ADMIN_TOKEN`、
`HYDRA_ENCRYPTION_KEY`（全集群一致）、`HYDRA_USAGE_SINK`；**强烈建议显式设 `HYDRA_CLUSTER_ID`**
（否则默认=成员表哈希，改成员表会全集群拒启）。

实测（3 个真进程，docker Redis）：选举/故障切换 **~1.1–1.6 s**、双 writer 从未出现、失去多数派时
管理写立即 503 而数据面继续服务。详见 [`dev-docs/cluster.md`](dev-docs/cluster.md)（成员表、故障矩阵、
成员变更 SOP）与 [`environment/docker-compose.cluster.yml`](environment/docker-compose.cluster.yml)。

## 更多

- 设计与架构：[`dev-docs/design.md`](dev-docs/design.md)
- 架构变更（终止模式）：[`dev-docs/design-change-terminate-mode.md`](dev-docs/design-change-terminate-mode.md)
- 运维手册：[`dev-docs/ops.md`](dev-docs/ops.md)
- 部署方案（单节点 / compose / K3s / K8s）：[`dev-docs/deployment.md`](dev-docs/deployment.md)
- 交互式流程图：[`dev-docs/workflow.html`](dev-docs/workflow.html)

Rust 1.83+ · Pingora 0.8.x
