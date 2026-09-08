# 深度调研简报：零改动在 GPUStack 原位替换 Higress 的 Hydra 改造方案

> 调研对象：`/home/alex/Projects/GPUStack/gpustack`（GPUStack v2.2.x）与 `/home/alex/Projects/dogress2`（Hydra）
> 目标：**不改 GPUStack 一行代码**，用 Hydra 原位替换其内嵌/接入的 Higress（apiserver + controller + pilot + envoy gateway）
> 方法：把 GPUStack→Higress 的全部契约（控制面 + 数据面 + 可观测性）逐条盘点，再映射为 Hydra 改动清单
> 日期：2026-02

---

## 0. 结论先行

- **可行性：高。** GPUStack 对 Higress 的依赖全部收敛在两个契约面上，且都是**有限、可枚举**的：
  1. **控制面**：GPUStack 只对"K8s API 子集"做一次性 CRUD（**无 watch、无 namespace 枚举、无 discovery**），共 5 类资源、约 20 个 REST 端点；
  2. **数据面**：一组可枚举的 HTTP 行为（header 路由、路径重写、forward-auth 认证、用量上报、4xx/5xx 回退、透传镜像路由）。
  3. 可观测性（Prometheus/Grafana）可降级或做浅兼容。
- **改动全部集中在 Hydra 新增一个 `gateway-adapter` 模块**：把 GPUStack 写进 CRD 的状态翻译成 Hydra 原生路由表，并实现 GPUStack 特有的数据面语义。
- **推荐路线**：MVP 采用"拓扑 B（external 指向）+ 控制面策略 2（保留 Higress 的嵌入式 apiserver 当纯存储，Hydra 充当控制器+数据面）"，**约 6~8 人周**；完整版（伪 apiserver 全替换 + ai-proxy 全类型 + 可观测性兼容）**约 12~16 人周**。

---

## 1. "零改动"的定义与边界

| 层面 | 是否可动 | 说明 |
|---|---|---|
| GPUStack Python 代码 | ❌ 不动 | 目标约束 |
| GPUStack 环境变量/配置 | ✅ 允许（最小） | external 拓扑需设 `gateway_mode=external` + `gateway_kubeconfig` + `advertise_address`；嵌入式拓扑完全不用动 |
| GPUStack 镜像打包 | ✅ 允许（最小） | 嵌入式拓扑需自建镜像：删掉 s6 的 `controller/pilot/gateway`（及可选 `apiserver`）服务，换成 hydra 进程 |
| GPUStack 数据面端口 | ❌ 不能变 | Hydra 必须占用 80/443（`port`/`tls_port`），嵌入式下还要 15020（Prometheus scrape） |
| GPUStack 写入的 CRD | ❌ 不能变 | Hydra 必须接受并正确解释这些写入 |

**关键洞察**：GPUStack 认为自己在跟"Higress 控制面"对话（写 CRD、读回 CRD）。Hydra 只需**单向往译**——把 GPUStack 的写入翻译成自己的路由表，CRUD 全部返回成功；不做写回（被动存储类如 TLS secret、higress-config 除外）。GPUStack 无从察觉差异，这就是"零改动原位替换"得以成立的原因。

---

## 2. 控制面契约盘点（K8s API 子集）——Hydra 必须实现的输入面

GPUStack 用 `kubernetes_asyncio` 客户端访问（嵌入式：`<data_dir>/higress/kubeconfig` → `https://127.0.0.1:18443`，insecure-skip-tls-verify；external：用户 kubeconfig）。实际调用点：

| # | 资源（group/version） | 方法（代码位置） | 触发场景 |
|---|---|---|---|
| 1 | `GET /api` 或 `/api/v1`（API 资源发现） | `v1.get_api_resources()`（`gateway/__init__.py:100` `wait_for_apiserver_ready`） | server 启动，60s 超时重试 |
| 2 | Secret `kubernetes.io/tls`（core/v1） | read/create/replace（`__init__.py:662` `ensure_tls_secret`） | 配置了 ssl_keyfile/certfile 时写入 `gpustack-tls-<host>` 或 `gpustack-tls-default` |
| 3 | ConfigMap `higress-config`（core/v1） | read/replace（`__init__.py:720` `ensure_gateway_timeout`） | 启动时调 idleTimeout |
| 4 | Ingress（networking.k8s.io/v1） | read/create/replace/delete/**list(label_selector=`gpustack.ai/managed=true`)**（`utils.py:830` `ensure_model_ingress`、`__init__.py:205`、`utils.py:1032` `cleanup_ingresses`） | 启动（镜像 ingress `gpustack`）+ 每个 model-route 增删改 + 孤儿清理 |
| 5 | McpBridge（networking.higress.io/v1） | get/create/replace/list（`utils.py:545` `ensure_mcp_bridge`，`__init__.py:149` `ensure_mcp_resources`） | 启动 + worker/cluster/provider/实例注册变更 |
| 6 | WasmPlugin（extensions.higress.io/v1alpha1） | get/create/replace/list/delete/patch（`utils.py:945` `ensure_wasm_plugin`） | 启动写 8 个插件 + 各 controller 增量 diff |
| 7 | EnvoyFilter（networking.istio.io/v1alpha3） | get/create/replace/delete/list（`utils.py:1230` `ensure_fallback_filter`、`1187` `cleanup_fallback_filters`） | 每个配置了回退的 model-route |

**要点**：
- **无 K8s watch**（GPUStack 侧）；所有 reconcile 由 GPUStack 自己的 DB 事件总线驱动（`ModelRoute.subscribe` 等），对 K8s 只做一次性读改写。
- 客户端对响应格式有要求：`apiVersion/kind/metadata.resourceVersion`、list 返回 `{items: []}`；`get_api_resources` 需返回合法 `APIResourceList`（kubernetes_asyncio 仅要求不抛异常）。
- 命名空间：embedded/external 下 `get_namespace()==gateway_namespace=="higress-system"`，单命名空间，简化；incluster 下模型 ingress 在 server namespace、网关在 higress-system（路由名带 `ns/` 前缀）。

---

## 3. 数据面契约盘点——Hydra 必须复刻的 HTTP 行为

### 3.1 端口与监听

| 端口 | 用途 | 零改动要求 |
|---|---|---|
| 80（`port`）/ 443（`tls_port`） | 数据面入口 | Hydra 必须监听；TLS 用 GPUStack 写入的 secret |
| 15020 | GPUStack 内置 Prometheus scrape `/stats/prometheus`（`prerun.py:302`，仅 embedded） | 最好兼容；否则 scrape 报错+面板空（可接受降级） |
| 15021/15090/15000 | envoy 健康/指标/管理 | 可忽略 |

### 3.2 路由模型（Higress 控制器语义的 Rust 复刻——核心工作）

GPUStack 的"路由表"不存在于单一文件，而是分布在 3 类 CRD + 注解里，Hydra 需要把它们**推导**出来：

**① Ingress → 路由规则**
- 镜像 ingress `gpustack`：`/` 前缀 → McpBridge `default` → GPUStack server（UI/API/认证/用量全部走这里）。
- 每模型路由 ingress `ai-route-route-<id>.internal`（及回退 `.<id>.fallback.internal`），关键注解（`utils.py:609` `generate_model_ingress`）：
  - `higress.io/destination`：**换行分隔的 `"pct% service:port"` 加权列表**（`controllers.py:1142` `calculate_destinations` 计算，权重=目标 weight）；
  - `higress.io/exact-match-header-x-higress-llm-model: <route>`：**header 路由核心**——仅当请求头 `x-higress-llm-model == route 名`（含 org 前缀 effective name，如 `org1/qwen`）才命中；
  - 回退 ingress 额外 `higress.io/exact-header-x-higress-fallback-from: <主ingress名>`；
  - `higress.io/rewrite-target: /$1$3`、`higress.io/ignore-path-case: true`、`higress.io/proxy-next-upstream-tries: 2`、`higress.io/proxy-next-upstream: error,timeout,http_503,http_502,non_idempotent`；
  - `included_proxy_route=true` 时（generic_proxy 路由）追加 `/model/proxy/<id>/` 与 `/model/proxy/` 两条正则路径。

**② 路径匹配与重写语义**（`utils.py:63` `RoutePrefix.regex_prefixes`）——必须逐条复刻：

| 匹配正则 | 重写 `/$1$3` 结果 | 说明 |
|---|---|---|
| `/(v1)(-openai)?(/chat/completions|/completions|/responses|/embeddings|/audio/transcriptions|/audio/speech|/images/generations|/images/edits)` | `/v1<path>`（恒等） | OpenAI 路由，legacy `/v1-openai/...` 归一为 `/v1/...` |
| `/(v1)()(/audio/translations|/images/variations|/moderations|/score)` | 恒等 | OpenAI 路由 |
| `/(v1)()(/rerank)`、`/(v2)()(/rerank)` | 恒等 | rerank 双版本 |
| `/(v1)()(/messages|/messages/count_tokens|/complete)` | 恒等 | Anthropic 路由 |
| `r"/()model/proxy/\d+(/|$)(.*)"` | `/<rest>`（剥掉 `/model/proxy/<id>`） | generic-proxy 别名路径 |
| `"/()model/proxy(/|$)(.*)"` | `/<rest>`（剥掉 `/model/proxy`） | legacy 别名路径 |

（`higress.io/ignore-path-case: true` → 大小写不敏感匹配。）

**③ McpBridge → 后端解析**（`utils.py:252` `model_instance_registry`、`344` `cluster_registry`、`364` `provider_registry`）
- `service:port`（如 `model-1-2.static:80`、`provider-1.dns:443`）→ 查 registry 同名项：`type=dns` → `host=domain, port=port`；`type=static` → `host=domain`（已含 ip:port，port 固定 80）。
- `proxyName` → McpBridgeProxy（`serverAddress:serverPort`、`type=HTTP/HTTPS`、`connectTimeout`）——外部供应商经代理出网。
- 覆盖的注册来源：GPUStack 自身（`gpustack.static`/`.dns`）、每 worker（直连 `worker_ip:port` / WORKER 代理 `worker.advertise_address:port` / **TUNNEL 模式指向 server 端 WebSocket 代理 `127.0.0.1:proxy_port`**）、每 cluster（`cluster-gateway.static`）、每 provider（`provider-<id>.dns/static`）、LoRA 别名（`l<sha256[:8]>`）。
- **实例动态性**：实例 scale up/down → McpBridge registries 增删 → Hydra 路由表必须实时刷新（1s 轮询或 watch 是正确性关键）。

**④ 权重与负载均衡**：destinations 是 `pct%` 权重列表 → Hydra 现有 SWRR 可直接复用（按百分比归一权重）。

### 3.3 认证（ext-auth 等价）——`gateway/__init__.py:294` `ext_auth_plugin`

- 作用域：**仅** `[ns/]ai-route-route-` 前缀的路由（镜像路由不认证）。
- 行为：对命中请求发起 **forward-auth GET** → GPUStack `/token-auth`：
  - 透传头：`X-Real-IP`、`X-Forwarded-For`、`x-higress-llm-model`、`x-api-key`、`cookie`、`x-gpustack-auth-cache`；
  - 附加头：`X-GPUStack-Auth-Token: <derived token>`；
  - 成功响应头回写上游：`X-Mse-Consumer`、`Authorization`、`cookie`、`x-gpustack-auth-cache`；
  - 超时：`GPUSTACK_HIGRESS_EXT_AUTH_TIMEOUT_MS`（默认 30000）；`failStrategy=FAIL_OPEN`。
- GPUStack 侧 `/token-auth`（`routes/token.py`）依赖 `x-higress-llm-model` 头做模型→访问策略判定，并回发 `X-Mse-Consumer` + `Authorization: Bearer <registration token>`。
- Hydra 现状：租户级 `auth_url`（GET/POST? 判定+缓存 5 分钟）——**需新增 forward-auth 模式**：GET 语义、上述头透传/回写、GPUStack token 注入。缓存机制可复用。

### 3.4 model-router 等价（`__init__.py:539` `generic_proxy_router_plugin`）

- 主体 Hydra 已有（读全 body 提取 model，任意位置/schema）。
- 需补齐：
  - 写 `x-higress-llm-model` 头（供 3.2 路由 + 3.3 认证 + 3.7 用量使用）；
  - `/model/proxy/<route_id>/...` 路径 → `aliasNameMapping[str(route_id)]` → 解析 model 名，**并回写 body 的 model 字段**（JSON 或 multipart）；
  - multipart/form-data 提取、`maxBodyBytes` 上限（config 可调）；
  - `enableOnPathSuffix` 白名单（即 §3.2 的 OpenAI/Anthropic 路径集）。

### 3.5 transformer 等价（`__init__.py:471`）——header 变换规则集

按序执行（Hydra `rewrite.rs` 已有部分能力）：
1. remove `X-GPUStack-Auth-Token`、`X-GPUStack-Model-Instance`；
2. rename `x-gpustack-model` → `x-higress-llm-model`；rename `x-gpustack-fallback-path` → `:path`（回退时恢复原路径）；
3. dedupe `x-gpustack-model`/`x-higress-llm-model`（RETAIN_FIRST）、`:path`（RETAIN_LAST）；
4. map `:path` → `x-gpustack-original-path`（备份原路径）；remove `x-gpustack-fallback-path`。

### 3.6 用量上报（token-usage 等价）——`__init__.py:618` + `routes/gateway_metrics.py`

- 上报端点：`POST /v2/usage/gateway-metrics`，认证头 `X-GPUStack-Auth-Token` = **`HMAC-SHA256(jwt_secret_key, "gateway-metrics-push")` 的 hex**（`config.py:359` `_derive_gateway_token`）——零改动下 Hydra 必须能拿到 `jwt_secret_key`（同机读取 GPUStack data dir，或从 config 传入）。
- 载荷（`metrics_collector.py:60` `ModelUsageMetrics`）：`model`、`input_token/output_token/total_token/input_cached_token`、`request_count`、**`completed`（流结束时是否见过 canonical usage chunk）**、`output_chunk_count`、`request_content_bytes`、`started_at/completed_at`（UnixMilli）、`organization_id`（来自 `X-Organization-Id` 头，token-usage 插件可配）等。
- `completed=false` 时 GPUStack 服务器按字节/块数**估算** token（`GPUSTACK_USAGE_ESTIMATED_BYTES_PER_INPUT_TOKEN`）——Hydra 尽量传 `completed=true`（流式下累计 usage chunk，非流式直接解析）。
- 流式（SSE）需增量累计 token 字段；OpenAI `prompt_tokens_details.cached_tokens` 与 Anthropic `cache_read_input_tokens` 均已原生支持（Hydra 现有能力）。
- ⚠️ 精确报文格式（如 `model_route_id` 是否由网关带、`operation` 枚举）需对照 `gpustack-higress-plugins` 的 token-usage 插件源码（本 repo 无该包，只有 server 侧 schema）。

### 3.7 回退（fallback 等价）——`controllers.py:1030` `sync_gateway` + `networking_istio_io_v1alpha3_api.py:225`

GPUStack 语义（Envoy custom_response RedirectPolicy）：
- 主路由上游返回 **4xx/5xx**（retry 用尽后）→ 内部重定向到回退路由（`<ingress>.fallback.internal`，其 destination=回退目标），带 `x-higress-fallback-from: <主ingress名>`（防环，max 10 次）+ `x-gpustack-fallback-path: <原路径>`；
- 回退目标由 `ModelRouteTarget.fallback_status_codes` 决定（`calculate_destinations` 中 fallback destinations 权重归一为 1）。
- Hydra 原生 failover 可**直接等价实现**：候选 = 主 destinations（按 retry 策略重试 2 次）→ 失败后追加 fallback destinations，请求头补 `x-higress-fallback-from` + `x-gpustack-fallback-path`（transformer 负责还原路径）。比 Envoy 内部重定向更简单可靠。

### 3.8 ai-proxy 等价（外部供应商路由）——`ai_proxy_types.py` + `utils.py:414`

- 跨 provider 的 model-route 才启用：WasmPlugin `gpustack-ai-proxy` 的 `providers[]`（`id/apiTokens/type/failover/retryOnFailure/自定义字段`）+ `matchRules[]`（`activeProviderId` + service + ingress 作用域）。
- 语义：命中规则的服务 → 用 provider registry 的 domain/port/protocol + `apiTokens` 换 `Authorization`，并按 `type`（openai/azure/bedrock/claude/… ~30 种）做协议适配。
- **v1 建议只做 openai 兼容子集**（GPUStack 内部实例都是 OpenAI 格式；`claude` 走 Anthropic 直通）；全类型后置。

### 3.9 worker 代理模式契约

- WORKER 代理模式：destination 解析为 `worker.advertise_address:worker.port`，但 worker 侧代理依赖请求头 **`X-GPUStack-Model-Instance`（=`clusterNameHeader`，由 `gpustack-set-model-pre-route` 插件写入，值=所选实例 cluster 名如 `model-1-2.static`）** 来定位实例端口（`routes/worker/proxy.py:158`）。
- TUNNEL 模式：destination = server 端 WebSocket 代理（embedded 下 `127.0.0.1:proxy_port`），同样依赖该头。
- → Hydra 需实现 pre-route 插件等价：**路由决策后把目标实例 cluster 名写入 `X-GPUStack-Model-Instance`**（且 transformer 阶段不得删除——注意 §3.5 里 remove 的是入站头，出站需新增）。

---

## 4. 两种零改动拓扑 × 两种控制面策略

### 4.1 拓扑

- **拓扑 A：嵌入式原位替换**。自建镜像 = GPUStack 官方镜像 + hydra 二进制；s6 服务里删 `controller/pilot/gateway`（及可选 `apiserver`），加 hydra。GPUStack 环境变量**完全不变**（auto → embedded，kubeconfig 自动指向 18443）。最贴合"原位"。
- **拓扑 B：external 指向**。GPUStack 以 docker/裸机跑 `gateway_mode=external`，`gateway_kubeconfig` 指向 Hydra 控制面，`advertise_address` 指向 Hydra 数据面。只改 env，适合快速验证。

### 4.2 控制面策略

| 策略 | 做法 | 优点 | 代价 |
|---|---|---|---|
| **策略 2（推荐 MVP）** | **保留 Higress 嵌入式 apiserver**（纯文件存储，最轻的进程），Hydra 用 kube-rs 以 1s 轮询/watch 消费 5 类 CRD → 构建路由表；Hydra 只做控制器+数据面 | 无需实现 K8s API；真实 apiserver 语义免费获得（list/label-selector/resourceVersion）；工作量最小 | 镜像里仍带 apiserver 二进制（内存小）；依赖其 file 存储 |
| **策略 1（完整版）** | Hydra 自实现"伪 apiserver"（axum + SQLite，绑 18443，实现 §2 约 20 个端点 + APIResourceList） | 彻底移除 apiserver 进程；Hydra 全自持（无 Higress 组件残留） | 需实现并测试 K8s API 子集（响应格式细节多），约 2~3 周 |

两条策略都不改动 GPUStack 代码；策略 2 对"原位"的忠实度稍低（保留一个 Higress 进程），策略 1 是完全替换。

---

## 5. Hydra 改动清单（分级 + 工作量）

> 现状列基于 `crates/hydra-core`（router/extract/sse/breaker/limit/swrr/rewrite/auth）与 `crates/hydra-server`（proxy/provider_client/admission/limiter/sink/tls/admin/cluster）。

| # | 模块 | Hydra 现状 | 需新增 | 工作量 |
|---|---|---|---|---|
| L0-1 | 控制面适配（策略 2） | 无 | kube-rs 客户端 + 1s 轮询/watch + CRD→内部模型 diff | **1~2 周** |
| L0-1' | 控制面适配（策略 1，可选） | 无 | 伪 apiserver（§2 端点全集 + SQLite + APIResourceList） | **2~3 周** |
| L0-2 | **路由引擎（核心）** | router.rs 按"模型×租户→供应商" | 新数据模型：Ingress 规则（regex/header matcher/rewrite/权重）→ registry 解析（dns/static/proxy/tunnel）→ SWRR 后端组 + retry 策略 + 动态刷新 | **3~4 周** |
| L1 | ext-auth 等价 | auth_url 判定+5min 缓存 | forward-auth 模式（GET、头透传/回写、`X-GPUStack-Auth-Token` 注入、FAIL_OPEN） | 3~5 人日 |
| L1 | token-usage 等价 | sink.rs（sqlite/clickhouse） | GPUStack sink：POST `/v2/usage/gateway-metrics` + HMAC token + SSE 累计 + completed 语义 | 3~5 人日 |
| L1 | model-router 等价 | extract.rs 读全 body 提 model | multipart、`/model/proxy/<id>` 别名、body model 字段回写、maxBodyBytes、写 `x-higress-llm-model` | ~1 周 |
| L2 | transformer 等价 | rewrite.rs 部分 | header 规则集（rename/map/dedupe/remove + path 备份恢复） | 3~5 人日 |
| L2 | fallback 等价 | failover + dead-set | 按状态码回退、防环头、主/备 destinations 编排 | 3~5 人日 |
| L2 | ai-proxy 等价（v1） | provider 路由/换 key | GPUStack provider 配置映射（apiTokens/activeProviderId/matchRules 作用域），v1 仅 openai 兼容 | 1~2 周 |
| L2 | 镜像路由透传 + TLS | 无 | catch-all `/` → GPUStack server（UI/API/认证/用量）+ 读 tls secret 建 443 监听 + hostname | 3~5 人日 |
| L2 | worker 代理头 | 无 | 出站写 `X-GPUStack-Model-Instance`（pre-route 等价） | 2~3 人日 |
| L3 | 可观测性 | `/metrics` 原生 | `15020/stats/prometheus` + envoy 指标名浅兼容（或明示降级：GPUStack Prometheus scrape 失败可容忍、Grafana 面板空） | ~1 周 |
| L3 | 集群模式 | leader/edge + Redis | 网关态跨节点同步（多 GPUStack 实例时加分，非必须） | 视需求 |
| — | 测试/回归 | 现有 287 测试 | 对照 `tests/gateway/*`（fallback/plugins/utils）+ GPUStack e2e（认证/流式/回退/用量四链路） | 1~2 周 |

**工作量汇总**：
- **MVP**（策略 2 + 拓扑 B；内部实例路由、OpenAI/Anthropic 直通、ext-auth、token-usage、镜像透传、TLS、无 fallback/ai-proxy/可观测性兼容）：**6~8 人周**。
- **完整版**（策略 1 全替换 + fallback + ai-proxy openai 子集 + worker 代理头 + 可观测性兼容）：**12~16 人周**。

---

## 6. 风险与关键依赖

| 风险 | 说明 | 缓解 |
|---|---|---|
| kubernetes_asyncio 响应格式细节 | 伪 apiserver（策略 1）需精确匹配客户端解析（resourceVersion/items/404 体） | MVP 用策略 2 规避；策略 1 用 GPUStack `tests/gateway` + 手工 curl 对照 |
| token-usage 报文精确格式 | 本 repo 只有 server 侧 schema（`ModelUsageMetrics`），网关侧序列化在 `gpustack-higress-plugins` 包（需另行拉取源码对照） | 以 server 侧字段为准 + 集成 e2e 断言 usage 落库 |
| 注解/CRD 语义未覆盖项 | Higress 控制器对注解的解析细节（rewrite 边界、regex 方言）以代码为准 | 逐条用真实 GPUStack 生成的 CRD 快照做单元测试 |
| GPUStack 版本演进 | 契约（header 名、插件名、CRD 结构）随版本变 | 适配层集中在一个模块，版本参数化 |
| 流式用量 + completed 语义 | 流中断时 token 估算依赖网关报文质量 | 对齐 token-usage 插件行为（completed=true 优先） |
| worker 代理/TUNNEL 模式 | 依赖 `X-GPUStack-Model-Instance` 头链路 | MVP 先支持直连模式；代理模式后置 |
| 认证缓存一致性 | GPUStack 侧也有 auth 缓存头（`x-gpustack-auth-cache` JWT） | Hydra 用自己 5min 缓存即可，头透传保持兼容 |

**验证路径**：GPUStack 自带 `tests/gateway/`（fallback EnvoyFilter、WasmPlugin spec、utils）是行为基准；完整验证 = 部署 GPUStack（docker-compose 官方方式）+ Hydra 替换后跑官方 e2e（建模型→实例→推理→用量查询→回退场景）。

---

## 7. 收益复述（相对现状）

- **资源**：移除 envoy gateway + pilot + controller（+apiserver）四进程，控制节点内存/镜像大幅下降（Higress 全家桶数百 MB~GB 级 vs Hydra 65 MiB）。
- **性能**：~0.3ms 网关开销、11k RPS；无 Wasm 解释层；SSE 流式回写可控。
- **稳定性原生化**：熔断/限流/故障转移/回退是 Rust 内建逻辑，不再依赖 wasm 插件链的 phase/priority 编排。
- **可运维**：管理 REST + UI、Prometheus 指标、单二进制；集群模式（leader/edge + Redis）可作为多 GPUStack 实例的统一入口。
- **零改动红利**：GPUStack 升级不阻塞（契约集中适配），回退方案 = 恢复原 Higress 镜像/配置，迁移风险低。
