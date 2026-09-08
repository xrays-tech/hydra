# 设计：租户模型目录接口（Tenant Model Catalog）

- **日期**：2026-09-03
- **状态**：已通过 oracle 深度复核（v3 GATE: PASS）→ 开发完成（Task 1-3 合入工作树）→ 代码 review APPROVE（无 P0/P1）→ 全套 CI 门禁（fmt/clippy/core/server/cluster-redis）全绿；**2026-09-08 缺陷修订**：数据面目录改为免认证读取（匿名 200；出示 key 仍校验并收窄），见 §8 v4 与 `dev-docs/aegis/plans/2026-09-08-public-models-catalog.md`
- **作者**：编码智能体（DeepSeek）｜深度复核：Architecture review (@oracle)（首轮 GATE: BLOCK，见 §8 修订记录）
- **对应需求**：研究结论落地——用租户侧 token 访问 Hydra 时，需要一个接口返回“该租户当前可访问的所有 provider 下、所有可路由模型”的聚合列表（现状无任何此类接口）。
- **前置研究简报**：本会话《租户模型目录接口可行性》结论：数据面 GET /v1/models 目前是单上游直通；权限模型为逐请求判定、无物化目录；建议数据面本地聚合 + 管理口只读聚合。

---

## 0. TL;DR

1. **数据面新增本地目录**：在 proxy 终止模式里拦截 `GET /v1/models`，改为**本地计算**该租户可路由模型的并集，返回 OpenAI 兼容 `{"object":"list","data":[...]}`（不再直通单一上游）。
2. **计算口径 = 路由准入语义的全集化**（`router::resolve` 的五道闸门逐一镜像）：模型 ∈ models_by_key（仅 status==1）∧ provider ∈ tenant_providers ∧（key 前缀绑定命中则只留绑定 provider）∧ provider 非熔断 / weight>0 / 有 key，再套 tenant_models 白名单（default-open）。
3. **管理面只读聚合端点** `GET /api/v1/tenants/{tenant_id}/models`（admin token）：同一纯函数 + 每模型带 provider 明细与在线状态，供 UI/运维核对“配置视图”。
4. **零新增配置项、零 DB/迁移、纯函数放 hydra-core**；行为变更仅限 `GET /v1/models` 一个路径（熔断探针直连 provider 端点，不受影响）。
5. 全套测试：core 纯函数单测 + 数据面终止模式集成测试 + admin 端点集成测试 + e2e python 断言。

**Verdict: DO IT.** 改动面小、权限口径与现有路由完全一致、无 schema 变更；唯一行为变更（/v1/models 语义）正是需求本身。

---

## 1. 背景与现状（研究结论回填）

### 1.1 “租户 token”与可访问入口（研究简报 §1）

| 入口 | token | 能访问的接口 | 与模型目录 |
|---|---|---|---|
| 数据面 8080/443 | 客户端 api-key（经租户 `auth_url` 外部鉴权） | `POST /v1/chat/completions`、`/v1/messages`；无 model 的 GET 默认 passthrough（proxy/config.rs:88-106） | 唯一“半个”入口：`GET /v1/models` 直通，见 §1.3 |
| 管理面 8081 | 租户自助 access token（migration 0009） | 仅 `POST /api/v1/tenants/{id}/auth/cache/invalidate`（admin/mod.rs:505-557） | 无 |
| 管理面 8081 | `HYDRA_ADMIN_TOKEN` | 平铺 CRUD（providers/provider-models/…，admin/mod.rs:309-358） | 原料可拼、无聚合端点 |

### 1.2 “租户可访问模型”的精确定义（现状 = 逐请求判定）

路由判定链 `crates/hydra-core/src/router.rs:66-141`（`resolve`），五道闸门：

1. **tenant_models 白名单**（default-open）：无映射 ⇒ 全放行；有映射 ⇒ 白名单外 403 `ModelNotAllowed`（router.rs:73-81）；
2. **模型须被 ≥1 个 `status==1` 的 provider 提供**（`models_by_key` 索引；loader 只收录在线行，store.rs:72-86）（router.rs:85-89）；
3. **provider 须在该租户 tenant_providers 授权集内**（fail-closed；缺 ⇒ `TenantForbidden`）（router.rs:94-98）；
4. **key-prefix 绑定闸门**：客户端 key 命中启用绑定 ⇒ 只留绑定 provider（最长前缀优先，无匹配不限制）（router.rs:106-117）；
5. **运行时可路由过滤**：非熔断（breaker dead 剔除，含集群共享 dead 集）、`weight>0`、有 ≥1 个 api-key、provider 必须存在于 cfg.providers（resolve 用 filter_map 静默剔除，router.rs:124-131）（router.rs:119-133）。

> 该集合只存在于“每个请求 resolve 一次”的瞬间：**没有物化、没有枚举接口**——这是不存在目录接口的根因。本设计新增一个纯“枚举”变体，口径与 resolve 完全一致。

### 1.3 现状 `GET /v1/models`（数据面直通）的缺陷

流程（proxy.rs:215-416）：Host→租户 → api-key 外部鉴权 → 无 body/model → `NonRouteStrategy::Passthrough`（默认）→ `passthrough_candidates`（proxy.rs:968-1002）取**第一个**满足条件的 provider（tenant_providers ∩ key 绑定 ∩ weight>0 ∩ 有 key，字典序第一）→ 换 provider key 直通上游（proxy/provider_client.rs:85-118）。

| # | 缺陷 | 后果 |
|---|---|---|
| D1 | 只回**第一个** provider | 多 provider 时拿不到“所有 provider”的并集 |
| D2 | **不过滤 tenant_models 白名单** | 白名单只放 1 个模型时仍列出上游全部模型，调用其中多数将 403 `ModelNotAllowed` |
| D3 | 模型口径是**上游**的 /v1/models | 与 Hydra 配置的 `provider-models.key` 是两套命名；不含 Hydra 的熔断/下线/绑定语义 |
| D4 | 换用 provider key 出站 | 目录请求实际打到上游（依赖上游可用性、多一跳） |

---

## 2. 设计

### 2.0 范围决策

| 决策点 | 方案 | 理由 |
|---|---|---|
| P0 数据面目录 | 拦截 `GET /v1/models`，本地计算返回 | 需求本体：租户 token 能拿自己的可调用模型列表；跨 provider 并集 + 白名单过滤是研究结论 |
| P1 管理口只读聚合 | `GET /api/v1/tenants/{tenant_id}/models`（admin token） | 运维核对“配置视图”（含离线 provider），与数据面同一闸门口径，改动很小 |
| 配置开关 | **不加**（P0 默认开启） | 需求即行为变更；仓库无“特性开关”惯例；熔断探针直连 provider 端点（breaker_wrap.rs:266-267），不受影响 |
| 新依赖 / DB / 迁移 | 无 | 目录全部由 ConfigStore 快照（含 edge 节点快照副本）计算 |
| 响应体泄露面 | 数据面只回模型 id（OpenAI 兼容）；provider 明细仅 P1 管理端点 | 不向租户暴露内部 provider 拓扑 |

### 2.1 纯函数（hydra-core，新增）

**位置**：`crates/hydra-core/src/router.rs` 追加（或新增 `catalog.rs` 子模块并从 `lib.rs` 导出——由实现按模块注释约定取舍；默认追加进 router.rs，与其测试同文件风格一致）。

```rust
/// 租户目录条目：模型 key + 当前可路由的 provider id 列表（确定性排序）。
pub struct CatalogEntry {
    pub model: String,
    pub providers: Vec<String>,
}

/// 枚举租户当前可路由模型（resolve 语义的全集化）。
/// 不可失败：租户不存在 / 无授权 / 全部被过滤 ⇒ 一律返回空 Vec（目录语义）。
/// 返回按 model 排序、providers 排序后的列表。
pub fn accessible_models(
    cfg: &ConfigData,
    breaker: &dyn BreakerView,
    tenant_id: &str,
    client_api_key: Option<&str>,
) -> Vec<CatalogEntry>
```

**算法**（与 router.rs:66-141 逐闸门镜像，返回类型不可失败 = F9 修订）：

1. `tenant_providers.get(tenant_id)` 缺 ⇒ 返回空 Vec（目录语义：无授权 = 无模型；GET 不当模型调用返回 403）；
2. 遍历 `cfg.models_by_key` 的每个 `(model, serving)`（serving 已保证 status==1）：
   - 若 `tenant_models` 有该租户映射且 model 不在白名单 ⇒ 跳过（闸门 1）；
   - providers = serving[].provider_id ∩ tenant_providers[T]；空 ⇒ 跳过（闸门 2+3）；
   - 若 `client_api_key` 命中 `match_key_binding` ⇒ 只留绑定 provider（闸门 4）；
   - **存在性守卫**：对每个候选 P 用 `cfg.providers.get(P)` 取引用，缺失 ⇒ 剔除（对齐 resolve 的 filter_map 语义，router.rs:124-131；该状态可达——validate 对孤儿 tenant_providers 仅 Warn（config.rs:207-216），loader 对引用缺失 provider 的 model 行仍入 models_by_key 且 weight 记 0（store.rs:78-85））；**严禁对 `cfg.providers` 裸索引**（会 panic）；
   - **weight 取值**：一律取存在性守卫 `get(P)` 所得引用上的 `weight` 字段（`Provider.weight`，定义于 model.rs:21-29）——不要再裸索引 cfg.providers，也不要取 `models_by_key` 行内 `ModelProvider.weight`（config.rs:73-76，加载期镜像、缺失时恒 0）；
   - 再过滤：`breaker.is_dead(P)` / weight<=0 / `provider_keys[P]` 空 ⇒ 剔除（闸门 5）；
   - 剩余非空 ⇒ 产出 {model, providers}；
3. 按 model、providers 字典序排序（确定性，延续 resolve 的排序约定，router.rs:140）。

> **实现备注**：`models_by_key` 是“model→providers”倒排索引，天然支持目录枚举；无需给 ConfigData 增加新索引。

### 2.2 数据面拦截（P0，hydra-server）

**拦截点**（2026-09-08 修订：目录免认证读取，见 §8 v4）：`proxy.rs request_filter` 在租户 enabled 检查（proxy.rs:243-246）之后、**必填 api-key 闸门之前**插入（原设计为 auth 判定通过后（proxy.rs:284-291）插入——修订后目录分支上移，匿名请求不再被 401 `missing_api_key` 拦下；keyed 请求仍先过外部鉴权再应答）：

```text
GET && path == "/v1/models"（path 取 req_header.uri.path()，忽略 query）
   ⇒ tenant resolve（未知域 404）+ tenant enabled（禁用 403）—— 不变，匿名同样执行
   ⇒ api-key 可选：无 key ⇒ 匿名读取（跳过外部鉴权，公开目录）；
      有 key ⇒ 走与聊天相同的 cache-first 外部鉴权（Denied ⇒ 401/403 原样回写）
   ⇒ 计算 accessible_models(cfg, breaker, &tenant.id, api_key: Option<&str>)
      （None ⇒ key-prefix 绑定闸门不触发 = 租户级并集；Some ⇒ 绑定收窄；不可失败）
   ⇒ 写本地 JSON 200：{"object":"list","data":[{"id":<model>,"object":"model"}, ...]}
   ⇒ return Ok(true)（不再进入 body 读取 / 限流 / 路由 / 直通）
```

**要点**：

- **鉴权语义（2026-09-08 修订，§8 v4）**：Host→租户（proxy.rs:226-240）、租户 enabled 检查（242-246）不变；api-key **可选**——未携带 ⇒ 匿名 200 读取该租户目录（公开只读）；携带 ⇒ auth_url 外部鉴权 + 缓存（proxy.rs:258-291）原样复用：未配置/失效 key 依旧 401/403，有效 key + 前缀绑定 ⇒ 目录收窄到绑定 provider。匿名并集 ⊇ 绑定 key 收窄视图为有意语义（模型 id 非机密，R12）。
- **响应写法（对齐仓库本地响应惯例）**：参照 short_circuit（proxy.rs:948-955）`session.set_keepalive(None)` 短响应惯例 + stream_response 的 header/EOS 写法（proxy.rs:905-936）；响应头带 `X-Hydra-Trace-Id`；写响应后把状态码记入 `ctx.status_code`（ctx.rs:59 默认 0，避免日志阶段依赖 response_written 兜底）；Content-Type: application/json。新增辅助 `respond_local_json`。
- **可观测性**：目录 GET 不产生 provider 用量/请求记录（logging 仅在有 selected provider 时记录，proxy.rs:750、800-825）——沿用现状、不新增指标（P2，如需可后续加目录计数）。
- **与 non_route_strategy 的关系（F4）**：拦截发生在策略消费（proxy.rs:397-400）之前 ⇒ 配置 `NonRouteStrategy::Reject` 的部署中，`GET /v1/models` 由 400 `no_model_field` 变为 200 目录——策略对该**精确路径**被静默旁路（HEAD/其他 GET 仍受策略约束），记录于 R8。
- **配额口径（F5）**：目录 GET 跳过 pre-limit count gate。注意变更前 GET /v1/models 直通同样消耗按 key/tenant 匹配的 count 配额（proxy.rs:346-368，model=None 也可命中角色）；变更后目录查询不计额，剩余防线 = auth 缓存（防滥用取舍见 R9）。
- **方法边界（F6）**：仅拦截 `GET` 且精确路径 `/v1/models`；`HEAD /v1/models` 与 `/v1/models/{id}` 等其它方法/路径继续直通（差异记录于 R11）。
- **其余 GET 不变**：健康检查、webhook 等非 /v1/models 路径继续 passthrough。
- **cluster/edge 兼容**：edge 节点同样持有 ConfigStore 快照（main.rs from_snapshot 分支）与 breaker/auth 状态 ⇒ 数据面目录在 edge 上工作；控制面中断时目录基于 last-known-good 快照（与同节点聊天口径一致，可能短暂落后 leader 最新配置，见 R10）。

**错误契约**（目录 GET；纯函数不可失败，500 行删除 = F9）：

| 场景 | 状态码 | 说明 |
|---|---|---|
| unknown_domain / 租户不存在 | 404 | 沿用现状（proxy.rs:238-240） |
| 租户 disabled | 403 tenant_disabled | 沿用现状 |
| 缺 key（匿名 GET /v1/models） | 200 目录 | 免认证读取（2026-09-08 修订，§8 v4）；仅 GET 精确 /v1/models；HEAD 与 /v1/models/{id} 匿名仍 401 |
| 出示失效 key | 401/403（上游 verdict） | 沿用现状，缓存语义不变；出示即须有效 |
| 租户无 tenant_providers | 200 空列表 | 目录语义；聊天路径仍 403 TenantForbidden，两者不冲突 |

**响应示例**：

```json
{"object":"list","data":[{"id":"gpt-4o","object":"model"},{"id":"gpt-4o-mini","object":"model"}]}
```

### 2.3 管理口只读聚合端点（P1，hydra-server）

**端点**：`GET /api/v1/tenants/{tenant_id}/models`（admin token 门禁；只读、集群下 standby 读本地副本即可，GET 不走转发（admin/mod.rs:387-390））。

- 路由注册：在 admin/mod.rs 路由表（309-358）之前加专用分支（避开“path 深度 >2 即 404”限制，mod.rs:304-307），位于 admin-token 门禁（mod.rs:560-568）之后、CRUD 路由之前，仅 GET；edge 节点在门禁前即 404（mod.rs:465-473，edge 仅探活端点）；
  - 该分支的端点注册同时必须处理：路由解析在 `/api/v1/` strip 后 parts 为 `["tenants", id, "models"]`（长度 3）——在 `parts.len() > 2` 拒绝（mod.rs:305）之前单独匹配。
- **视图口径（与数据面同源、但为“配置全集”）**：`accessible_models` 会按闸门 5 剔除 dead/weight<=0/无 key 的 provider（仅在线视图，数据面用）；P1 要展示离线项供诊断，故在 handler 层做**静态全集枚举**——同一批闸门（tenant_models 白名单、tenant_providers、models_by_key、cfg.providers 存在性守卫、**跳过 key-prefix 绑定**——client_api_key=None 时 `match_key_binding` 不触发（router.rs:110-117）），但**不剔除** dead/weight0/无 key，仅对每个 provider 打 `online` 标志（`!breaker.is_dead && cfg.providers.get(P) 的 weight>0 && provider_keys[P] 非空`；存在性守卫 get 不裸索引）。模型只要仍有 ≥1 个已授权且存在的 provider 即列出（即使全部离线，便于诊断）；输出按 model、provider_id 排序；
- **租户存在性判定（F8）**：ConfigData 无 tenant-id 索引（config.rs:36-69，仅 tenants_by_domain）——404 判定 = 遍历 `tenants_by_domain` 的值匹配 `id`（域为必填、每租户恰一行，model.rs:79-80 + store.rs:103-105）；**不得**用 `tenant_providers` 有无判定存在性；
- 响应：`{"tenant_id":"<id>","models":[{"model":"gpt-4o","providers":[{"provider_id":"openai","online":true}]}]}`；
- 租户不存在 ⇒ 404；edge 节点无 admin API ⇒ 404（沿用 edge 约定）。

### 2.4 文档更新

- `README.zh-CN.md` 使用节：说明 `GET /v1/models` 返回**本租户可调用模型目录**（本地聚合，非上游目录）；
- `admin-ui/api-docs.js`：新增 P1 端点条目（method/path/summary 结构，api-docs.js:226-247 同款）；
- `admin-ui/i18n.js`：4 语（en/zh/fr/de）同步新增 `apidocs.summary.GET.api.v1.tenants.tenant_id.models` 键（**键名无花括号**：apiSummaryKey 渲染查找时把 path 归一化为去前导 /、/ → .、- → _、并删除 { }（api-docs.js:417-420）；API_DOCS 条目 path 仍写 /api/v1/tenants/{tenant_id}/models（api-docs.js:226-247 同款）；i18n.js:18-23 四语均须存在，缺键出裸键/回退 EN——F2/R1）；
- `dev-docs/ops.md` 端点表登记（如 P1 采纳）；`dev-docs/design.md`：改写 §6.3a 中以 GET /v1/models 作为 passthrough 用例的叙述（该路径不再直通——R5），并登记新端点；
- 本设计文档 + aegis INDEX 登记。

---

## 3. 文件改动清单

### 新增
- `crates/hydra-core/src/router.rs`（追加 accessible_models + CatalogEntry；或新增 catalog.rs——默认追加 router.rs）
- `crates/hydra-core/tests/router.rs`（追加目录纯函数测试；或新增 tests/catalog.rs）
- `dev-docs/design-tenant-model-catalog.md`（本设计）

### 修改
- `crates/hydra-server/src/proxy.rs` — 拦截 GET /v1/models + respond_local_json 辅助（set_keepalive(None)/X-Hydra-Trace-Id/写 ctx.status_code）
- `crates/hydra-server/tests/terminate_mode.rs` — 数据面目录集成测试（复用 seed_*/start_proxy/wiremock 夹具）
- （P1）`crates/hydra-server/src/admin/mod.rs`（专用路由分支）+ `admin/handlers.rs`（handler）+ `crates/hydra-server/tests/admin_api.rs`（用例）
- （P1）`admin-ui/api-docs.js`（端点条目）+ **`admin-ui/i18n.js`（4 语 apidocs key，F2）**
- `README.zh-CN.md`、`dev-docs/ops.md`、`dev-docs/design.md`（§6.3a 叙述改写 + 端点登记）、`dev-docs/aegis/INDEX.md`

### 不改
- DB / migrations / `.sqlx`（无新 query）
- ConfigData 结构、ConfigStore、admin CRUD 资源、环境变量

---

## 4. 分步任务（bite-sized，供开发子任务切分）

### Task 1（core：纯函数 + 单测）
- router.rs 追加 CatalogEntry 与 accessible_models（返回 Vec，不可失败）；tests/router.rs 追加用例：
  1. 无 tenant_providers ⇒ 空；
  2. tenant_models 白名单过滤 + default-open（无映射 ⇒ 全量）；
  3. 多 provider 同模型 ⇒ 单条目多 provider（去重 + 排序）；
  4. key-prefix 绑定收窄（命中 / 未命中 / disabled 忽略，对齐既有 binding 测试）；
  5. 过滤：breaker dead / weight<=0 / 无 key；
  6. **存在性守卫（F3）**：models_by_key 引用缺失 provider（孤儿行）⇒ 剔除不 panic；
  7. 输出字典序确定性。

### Task 2（server 数据面 P0 + 集成测试）
- proxy.rs 拦截 + respond_local_json；terminate_mode.rs 新增用例（夹具：双 MockServer——auth 独立 + provider wiremock）：
  - 白名单过滤 + 跨 provider 并集（两个 mock provider）；
  - **零上游断言（F10）**：不挂任何 provider mock + `upstream.received_requests()` 判空（先例 terminate_mode.rs:321；auth 走独立 MockServer 不计入）；
  - **非拦截 GET 钉住测试（F6）**：`/v1/models/{id}` 仍走上游（wiremock expect(1)）；
  - 未授权租户 key ⇒ 401（外部鉴权路径不变）；
  - 无 tenant_providers ⇒ 200 空 data；
  - key-prefix 绑定 ⇒ 只列绑定 provider 的模型；
  - 既有 POST 路由测试全绿（回归；现仓无既有 GET /v1/models 直通用例被破坏——F6 佐证）。

### Task 3（P1 管理端点 + 集成测试 + API docs）
- admin/mod.rs 专用路由分支（admin-token 门禁内、parts==["tenants",id,"models"]）+ handlers.rs handler（含 tenant 存在性判定 = 扫 tenants_by_domain）；
- admin_api.rs 用例：200 结构 / 租户不存在 404 / 无 token 401 / edge 404（更新既有 edge_admin_probes_only 兼容性）；
- api-docs.js 端点条目 + i18n.js 4 语 apidocs key（F2）。

### Task 4（文档 + 收尾）
- README.zh-CN.md / ops.md / INDEX.md；全仓 fmt + clippy（对齐 ci.yml）+ 双 crate 全量 test（含额外 cluster-redis 编译门禁）；（可选）integration/e2e_proxy_test.py 追加 GET /v1/models 断言。

---

## 5. 验证与门禁（与 .github/workflows/ci.yml 逐字对齐，F1）

```bash
cargo fmt --check                                  # ci.yml:39 同款
cargo clippy --workspace --all-targets --features hydra-server/server -- -D warnings   # ci.yml:47 同款
cargo test -p hydra-core                            # ci.yml:55 同款
cargo test -p hydra-server --features server        # ci.yml:76 同款
# 额外本地门禁（CI 未覆盖——tests/cluster.rs 需 cluster-redis feature）：
cargo test -p hydra-server --features server,cluster-redis
# e2e（可选）：python3 integration/e2e_proxy_test.py
```

注：仓库 CI 环境另设 `RUSTFLAGS="-D warnings"` 与 `SQLX_OFFLINE="true"`（ci.yml:10-11）；本地跑新 query/迁移后需 `cargo sqlx prepare` 刷新离线缓存（本项目本次无新 query，免）。
门禁 = 上述命令全绿 + oracle/design review 无 P0/P1 遗留。

---

## 6. 风险与决策记录

| # | 风险 / 决策 | 处理 |
|---|---|---|
| R1 | 行为变更：GET /v1/models 从“上游直通目录”变为“本地聚合目录”（需求本体，但仍属破坏性变更） | README/变更说明标注；改写 design.md §6.3a 对 GET /v1/models 直通的叙述（R5）；**熔断探针不受影响的反证**：探针直连 `{endpoint}/v1/models`（breaker_wrap.rs:266-267），不经 Hydra 数据面 |
| R2 | Anthropic 协议客户端若也 GET /v1/models，将收到 OpenAI 形状响应 | 数据面无法区分协议族；接受并在 README 注明；低概率路径 |
| R3 | 熔断/软禁用/断 key 会动态缩小目录 | 与聊天路由口径一致（“现在能调什么”）；P1 用 online 标志暴露离线原因 |
| R4 | 目录不含上游新增但 Hydra 未配置的模型 | 口径正确：Hydra 只路由配置过的模型（D3 的正面澄清） |
| R5 | 拦截点与限流顺序 | 目录 GET 跳过 count gate（§2.2）；如需按角色限制目录查询频率属后续项 |
| R6 | provider 明细泄漏 | 数据面只回 model id（§2.0 决策） |
| R7 | tenant_models 白名单里“不在 models_by_key”的 key | 目录天然不出现（无人提供）；聊天路径本就 404 ModelNotFound，无新语义 |
| R8 | **Reject 策略旁路（F4）**：NonRouteStrategy::Reject 部署下 GET /v1/models 由 400 变 200 | 显式记录：目录语义对该精确路径优先于 no_model_field 策略；HEAD/其他 GET 仍受策略约束 |
| R9 | **配额口径变更（F5）**：变更前 GET /v1/models 直通也消耗按 key/tenant 匹配的 count 配额；变更后目录查询不计额 | 防滥用仅剩 auth 缓存；**2026-09-08 修订：匿名目录无 key ⇒ auth 缓存防线也不存在**，仅剩本地只读 + CPU 有界（models_by_key × tenant_providers 遍历）+ 无上游；以 `hydra_catalog_requests_total` 观测匿名 QPS，如需按租户限频属后续独立项 |
| R10 | **edge 快照时效（F10）**：控制面中断时目录基于 last-known-good 快照，可能短暂落后 leader 最新配置 | 与同节点聊天路径口径一致（同为该快照路由）；文档注明即可 |
| R11 | **方法边界（F6）**：仅 GET 精确 /v1/models 被拦截；HEAD /v1/models、/v1/models/{id} 等继续直通 | 明确记录差异；钉住“非拦截 GET 仍走上游”测试；如需统一可后续扩展 |

---

## 7. Out of scope（后续可选）

- 租户自助 token（migration 0009）扩展为可查目录（需在 admin/mod.rs:505 自助闸门处扩展）；
- 目录响应携带 provider 明细（面向租户）或 per-provider 上游健康聚合；
- `GET /v1/models/{model}` 单模型查询的本地化；
- 目录结果缓存 / ETag / 目录计数指标。

---

## 8. oracle 复核修订记录

| 轮次 | 结论 | 修订 |
|---|---|---|
| v1（本会话） | GATE: BLOCK（3×P1） | F1 §5 命令逐字对齐 ci.yml（workspace + pkg/feat + `--` 分隔）；F2 改动清单补 admin-ui/i18n.js 4 语 apidocs key；F3 §2.1 补 provider 存在性守卫（get 不裸索引）并明确 weight 取 cfg.providers[P].weight。另吸收 P2：F4→R8、F5→R9、F6→R11+Task2 钉住测试、F7→§2.2 响应写法、F8→§2.3 存在性判定、F9→签名返回 Vec、F10→R10+Task2 零上游断言机制 |
| v2（oracle 复审） | GATE: BLOCK（1×P1: R1） | R1 i18n 键名去花括号（apiSummaryKey 归一化删 { }，api-docs.js:417-420）；R2 weight 出处改引 model.rs:21-29；R3 统一“get(P) 所得引用取值、不再裸索引”表述；R4 伪码参数改 &tenant.id；R5 补 design.md §6.3a 叙述改写项 |
| v3（oracle 复审） | GATE: PASS（残留 2×P2 不阻塞，已吸收） | P2-F1 §3 清单补 dev-docs/design.md；P2-F2 §2.3 online 措辞改 cfg.providers.get(P) |
| v4（2026-09-08 缺陷修订） | 缺陷修复方案独立 oracle 复核 **GATE: PASS**（P0=0；P1-1 发布顺序声明 + P2-1..5 吸收，见计划 §8） | 数据面 GET /v1/models 改为**免认证读取**：目录拦截上移到必填 key 闸门之前（proxy.rs (2.5) 分支 + 共享 `enforce_auth` helper）；匿名 ⇒ 200 本地目录（跳过外部鉴权）；出示 key ⇒ 仍校验（401/403）并按前缀绑定收窄；HEAD/`/{id}`/其它路径与 chat/管理面全部不变。实现落档：`dev-docs/aegis/plans/2026-09-08-public-models-catalog.md`；配套匿名正/负向钉住测试（200 并集零上游 / query 忽略 / HEAD 401 / /{id} 401 / disabled 403 / 未知域 404）与本文档 §2.2/错误契约/要点/R9 同步修订 |
| 开发后 | 实现代码 review VERDICT: APPROVE（P0/P1=0，4×P2 已按下列吸收） | 代码 review P2 吸收：①proxy/config.rs NonRouteStrategy 文档与 design.md §6.3a 改写（GET /v1/models 不再为直通示例）；②新增 hydra_catalog_requests_total 计数器（metrics.rs + proxy 调用）；③README 模型目录说明 + aegis INDEX 登记；④CatalogEntry 保留 serde derive（对齐实体约定，无在库消费点已注明） |

---