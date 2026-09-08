# 简报：GPUStack × Higress 集成剖析，以及用 Hydra 替代 Higress 的可行性

> 调研对象：`/home/alex/Projects/GPUStack/gpustack`（GPUStack v2.2.x 代码库）
> 候选替代品：`/home/alex/Projects/dogress2`（Hydra，Rust + Pingora LLM 路由网关）
> 日期：2026-02

---

## 一、GPUStack 是如何集成 Higress 的

### 1.1 定位

GPUStack 官方架构文档明确：**"GPUStack uses Higress for API routing and load balancing"**（`docs/architecture.md`）。Higress 在 GPUStack 中不是可选的旁路，而是默认数据面的核心——**AI Gateway**：

- 承接所有客户端请求（UI/API/推理 API，默认 80/443）
- 按请求体里的 `model` 字段把推理请求路由到正确的模型实例（vLLM/SGLang 等）
- 对外部模型供应商（OpenAI/Anthropic/DeepSeek 等 ~30 家）做代理与 key 替换
- 执行认证、用量统计、故障转移、限流等 LLM 专用逻辑（Wasm 插件实现）

### 1.2 部署形态：四种 gateway mode（`gpustack/schemas/config.py` GatewayModeEnum）

| 模式 | 说明 |
|---|---|
| `embedded`（默认） | Higress 以 **standalone 模式内嵌在 GPUStack 容器里**：镜像内多阶段拷贝 4 个二进制——轻量 kube-apiserver（文件存储，`--storage file`，监听 127.0.0.1:18443）+ higress controller + pilot（Istio）+ envoy gateway，由 s6-overlay 拉起（`pack/rootfs/etc/s6-overlay/s6-rc.d/{apiserver,controller,pilot,gateway}`）。GPUStack server 通过 `<data_dir>/higress/kubeconfig` 访问这个内嵌 apiserver（`gpustack/cmd/prerun.py`） |
| `incluster` | Helm 部署在 K8s 上，Higress 作为集群 ingress controller（chart 自带 `higress-core` sub-chart，或复用已有 Higress，`gateway.ingressClassname=higress`）。**官方标注 experimental**（`docs/installation/helm.md`） |
| `external` | 指向用户自管的 Higress K8s 集群（`gateway_kubeconfig`），McpBridge 以 static registry 指向 GPUStack 的 advertise 地址 |
| `disabled` | 关掉网关 |

### 1.3 控制面集成：CRD 驱动的 reconcile（核心机制）

GPUStack server 用 `kubernetes_asyncio` 把自身状态**翻译成 Higress 的 CRD**，控制器（`gpustack/server/controllers.py` + `gpustack/gateway/utils.py` + `gpustack/gateway/__init__.py`）持续 diff + reconcile 三类资源：

**① Ingress（networking.k8s.io/v1，class=higress）**
- 镜像 Ingress `gpustack-mirror`：`/` 前缀 → 默认 McpBridge registry（即 GPUStack server 自身），承载 UI/API/`/token-auth`/用量上报。
- 每模型路由一个 Ingress：`ai-route-route-<id>.internal`，核心注解：
  - `higress.io/destination`：目标集群名（由 worker/实例地址拼出）
  - `higress.io/rewrite-target: /$1$3`：剥掉 `/v1/chat/completions` 等路径段
  - `higress.io/proxy-next-upstream(-tries)`：重试策略
  - `higress.io/exact-header-x-higress-llm-model: <route>`：**按 header 路由的关键**——路由表由 `x-higress-llm-model` header 决定
  - 泛代理路由额外含 `/model/proxy/<id>/` 路径规则（generic-proxy 别名）

**② McpBridge（networking.higress.io/v1）——Higress 的服务发现桥**
GPUStack 把自己和所有后端注册进 `default` McpBridge：
- `gpustack` registry（dns/static → server:port，供 ext-auth、用量上报用）
- 每个 worker 的 registry：直连模式（worker_ip:port）、WORKER 代理模式、TUNNEL 模式（WebSocket 隧道，指向 server 端 HTTP 代理地址）
- 每个 cluster / 每个 model provider 的 registry 与 proxy
- LoRA 别名 registry（hash 命名，`l<sha256[:8]>`）

**③ WasmPlugin（extensions.higress.io/v1alpha1）——LLM 专用插件链**
按 phase + priority 组成处理链（`gateway/__init__.py::initialize_gateway`）：

| 插件 | phase/优先级 | 职责 |
|---|---|---|
| `gpustack-model-router`（generic-proxy-router） | AUTHN / 900 | 从 JSON/multipart body 读 `model` → 写入 `x-higress-llm-model` header；`/model/proxy/<id>/` 路径驱动别名映射 |
| `transformer` | AUTHN / 810 | header 变换：`x-gpustack-model`→`x-higress-llm-model`、path 备份/去重 |
| `gpustack-llm-ext-auth` | AUTHN / 360 | forward-auth：把请求转发到 GPUStack `/token-auth`，校验 api-key/模型访问策略，回写 `X-Mse-Consumer` + `Authorization`（注册 token），带 5 分钟缓存 |
| `gpustack-token-usage` | — / 400 | 解析响应 token 用量 → POST 到 GPUStack `/v2/usage/gateway-metrics` |
| `ai-proxy` | — / 100 | 外部供应商路由：~30 家 provider 的 endpoint/格式/api-key 替换/failover 配置 |
| `ai-statistics` | — / 900 | 访问日志/遥测属性（consumer 等） |

**插件分发**：Wasm 二进制由独立 Python 包 `gpustack-higress-plugins` 提供，GPUStack server 自身 HTTP 托管（`/v1/plugins/...`，`gateway/plugins.py`），Higress 启动时下载；Helm 模式下有独立 `higress-plugins` Deployment（`charts/gpustack-chart/templates/higress-plugins-deployment.yaml`）。

### 1.4 数据面请求流（以推理请求为例）

```
client → envoy gateway(80/443)
  → model-router wasm: 读 body 提取 model → x-higress-llm-model
  → ext-auth wasm: forward-auth → GPUStack /token-auth
      （校验 api-key + model 访问策略，回写 X-Mse-Consumer/Authorization）
  → Ingress header matcher → 命中 ai-route-route-<id>.internal
  → higress.io/destination → 实例集群（直连 worker / worker 代理 / websocket 隧道）
  → 响应回写时 token-usage wasm 解析用量 → GPUStack 用量 API 入库
  → ai-statistics 写日志/遥测（Prometheus 抓 /stats/prometheus + Grafana higress 面板）
```

### 1.5 集成代价小结

- 镜像内置 **4 个常驻进程**（apiserver + controller + pilot + envoy gateway）+ Istio mesh 配置 + Prometheus/Grafana 集成，控制节点资源开销大
- GPUStack 侧存在**一整层 CRD 适配代码**（gateway 模块约 1600 行 utils + client 层 + 多个 controller）
- 插件链是 Wasm 生态：新增/改逻辑要写 Go/Rust wasm 插件、发版、重新分发

---

## 二、Hydra（本项目）与 Higress 在 GPUStack 中职责的对照

| 能力 | Higress（GPUStack 内） | Hydra（dogress2） | 替代难度 |
|---|---|---|---|
| body 提取 model → 路由 | model-router wasm（读 body，受首 chunk 限制，需 enableOnPathSuffix 白名单） | 原生终止式：读全 body，model 任意位置/schema 提取 | 直接替代 ✅ |
| 认证 | ext-auth wasm → GPUStack /token-auth（api-key + 模型策略 + 缓存） | 按租户 `auth_url` 外部认证 + 5 分钟缓存 + 失效接口；api-key 前缀绑定 | 小改：auth_url 指向 /token-auth，透传 `x-higress-llm-model`，回写 `X-Mse-Consumer`/`Authorization` |
| 路由到内部模型实例 | McpBridge 注册 + Ingress header matcher（动态实例发现） | 目前按**静态供应商 URL** 路由（SWRR 加权轮询） | **主要工程**：需新增"动态后端发现" |
| 外部供应商代理 | ai-proxy wasm（~30 家，key 替换/failover） | 原生：多供应商、换 key、加权路由、故障转移、熔断 | 直接替代 ✅（功能更强） |
| 用量计量 | token-usage wasm 解析 → GPUStack 用量 API | 原生细粒度：prompt/cached/completion + TTFT，SQLite/ClickHouse sink | 小改：新增 GPUStack 用量 API sink |
| 限流 / 熔断 / 故障转移 | 需 wasm 插件 + CRD 配置 | 原生内存滑动窗口限流、熔断 dead-set、全 body 重放 failover | 直接替代 ✅ |
| 按租户 TLS | 单证书（Ingress TLS） | 按 SNI 多租户证书、热更新 | 更强 ✅ |
| 管理面 | GPUStack UI（网关配置需 CRD/annotations） | 管理 REST + UI + Prometheus /metrics | 直接替代 ✅ |
| 通用 Ingress / K8s 生态 | 完整（MCP server、其他 wasm 插件、云原生生态） | 无（LLM 网关定位） | **失去** ⚠️ |
| 资源占用 | 4 进程 + Istio，数百 MB~GB 级 | 65 MiB 单二进制 | ✅ |

---

## 三、替代方案与成本评估

### 方案 A：前置叠加（最低成本，零 GPUStack 改动）✅ 推荐起步

把 Hydra 部署在 GPUStack **前面**：`client → Hydra → GPUStack(Higress) → 模型实例`。
Hydra 承担多租户入口（域名解析租户、按租户 TLS、auth_url、限流、api-key 前缀绑定、跨 GPUStack 实例的故障转移、统一用量聚合）；GPUStack 原样不动。
- 成本：**约 0~2 人日**（部署 + 把 GPUStack 当做一个 upstream 配置）
- 收益：多租户隔离、按租户 TLS、统一计量、多 GPUStack 集群路由
- 缺点：Higress 还在（不省资源）；多一跳（~0.3ms，相对 LLM 延迟可忽略）

### 方案 B：仅替换"外部供应商"路径（低-中成本）

GPUStack 的 ModelProvider/generic-proxy（走 Higress ai-proxy wasm）换成 Hydra 管理外部供应商。
- 成本：约 3~5 人日（供应商/路由配置双向同步桥）
- 收益有限：GPUStack 本身已具备此能力，且会割裂网关；**不推荐单独做**，除非需要 Hydra 独有的多租户 key 前缀绑定与更细计量。

### 方案 C：深度替换——用 Hydra 完全取代 Higress（中-高成本）

Hydra 直接终止客户端流量并路由到**模型实例本身**。GPUStack 内嵌的 apiserver/controller/pilot/gateway 全部退役。

需要新增/修改（按工程量大到小）：

1. **实例发现同步桥（最大头，约 2~4 人周）**：GPUStack 的 model-route/model-instance controller 目前把状态写进 Higress CRD；改为（或并行）把 `route → backend 端点列表` 推到 Hydra 管理 API（或 Hydra 定时拉 GPUStack API）。要覆盖：直连 worker、WORKER 代理、WebSocket TUNNEL 隧道、K8s cluster 服务、多副本负载均衡（Hydra SWRR 可复用）、LoRA 别名。
2. **incluster（K8s）模式**：Higress 同时是集群 ingress controller（承载 GPUStack UI/API 的 ingress）。要么砍掉该模式，要么混合方案（保留 Higress 仅做 control-plane ingress，推理路径走 Hydra）——**混合方案工程更小**。
3. **控制面 API/UI 转发（中）**：Hydra 目前只路由 LLM 路径；需增加"非 LLM 路径透传"（/token-auth、UI、/v2/usage 等），或混合方案下交给保留的 Higress。
4. **认证兼容（小）**：Hydra tenant `auth_url` → GPUStack `/token-auth`，透传 `x-higress-llm-model`，并把响应里的 `X-Mse-Consumer`/`Authorization` 回写上游请求。
5. **用量上报（小）**：Hydra 新增 GPUStack sink，把解析好的用量 POST 到 `/v2/usage/gateway-metrics`（带 gateway token），保持 GPUStack 用量页面/计费不变。
6. **可观测性迁移（中）**：Prometheus scrape 从 envoy `/stats/prometheus` 换到 Hydra `/metrics`；Grafana 面板（higress-* 两个 dashboard）需重做或换 Hydra 指标。
7. **回归验证（中）**：双协议（OpenAI/Anthropic）流式、多模态/audio、multipart、MCP 路径等兼容性测试。

总量估算：**主干 3~6 人周**（不含长期维护）。

---

## 四、用 Hydra 替代 Higress 的正向收益

1. **资源断崖式下降**：去掉 4 个内嵌进程 + Istio 网格后，控制节点内存/镜像体积显著下降（Higress 全家桶通常数百 MB~GB 级 vs Hydra 满载 65 MiB RSS）；对 GPUStack 的"单机嵌入式"部署形态（本来就是为了低配机器）收益尤其大。
2. **性能**：网关开销 ~0.3ms/请求、实测峰值 11k RPS；Rust 数据面无 wasm 解释层，SSE 流式回写更可控。
3. **多租户能力（Higress 方案没有的）**：按域名解析租户、按租户 TLS（SNI 证书热更新）、租户级 `auth_url`、api-key 前缀绑定（`sk_aaa_*` → 固定供应商，fail-closed）、租户级限流角色。
4. **稳定性原生化**：故障转移（全 body O(1) 重放）、熔断 + 后台探活恢复、限流都是内建逻辑，不再依赖 wasm 插件链的优先级/阶段编排。
5. **计量更细**：原生解析缓存 token（OpenAI `prompt_tokens_details` / Anthropic `cache_read_input_tokens`）+ TTFT，无需 wasm 解析；落库加密（AES-256-GCM）。
6. **运维面**：管理 REST + UI、Prometheus 指标、集群模式（leader/edge + Redis，K8s 无关）、单二进制易部署升级。

## 五、风险与代价（需正视）

- GPUStack 控制面对 Higress 的耦合是**写进代码的**（CRD reconcile、`x-higress-llm-model` 约定、`/token-auth` 契约），替代 = 新增一层适配，不是删配置；`incluster`/K8s 模式与 Higress 生态（通用 Ingress、MCP server、其他 wasm 插件）会丢失或需混合保留。
- 双同步链路（既有 CRD + 新 Hydra push）的一致性维护成本。
- 非 OpenAI/Anthropic 协议路径（GPUStack 的其他推理后端/协议）需逐一验证 Hydra 透传兼容性。

## 六、结论

- **低成本替代的真实路径是方案 A（前置叠加）**：零 GPUStack 代码改动，立刻获得多租户/按租户 TLS/统一计量/跨实例路由，适合把 Hydra 定位为"GPUStack 之上的多租户 LLM 入口层"。
- **方案 C（完全替换）** 收益最大（资源 + 性能 + 稳定性），工程大头集中在"模型实例动态发现同步桥"与"混合模式下保留控制面 ingress"两点；建议按 A → C 分阶段演进，先验证兼容性再退役 Higress。
- 与"替代"相比，更自然的定位是**互补**：Hydra 管多租户入口 + 外部供应商路由，GPUStack(Higress) 管内部模型实例编排；两者以"GPUStack 用量 API + 实例端点"为契约衔接，改动面最小。
