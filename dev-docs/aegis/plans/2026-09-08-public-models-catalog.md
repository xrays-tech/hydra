# 实施计划：数据面 GET /v1/models 公开目录（免认证读取）

- **日期**：2026-09-08
- **状态**：已通过 oracle 架构复核（v1 GATE: PASS）→ 已落档 → **2026-09-08 已实施**（proxy.rs (2.5) 上移 + `enforce_auth` 共享 helper + 匿名正/负向钉住测试 + 文档同步）→ 门禁见 §9 实施记录
- **作者**：编码智能体（DeepSeek）｜深度复核：Architecture review (@oracle)
- **对应需求**：生产环境 https://api-dev.do.top/v1/models（数据面，无任何认证头）实测返回 `{"error":{"message":"missing_api_key","type":"proxy_error"}}`；产品结论：OpenAI 兼容的 `GET /v1/models` 目录必须**免认证可读**，聊天调用等其余路径鉴权语义不得受影响。
- **前置设计**：dev-docs/design-tenant-model-catalog.md（2026-09-03，oracle v3 GATE: PASS，开发已合入工作树，未提交）；本计划**修订**其 §2.2「目录访问与聊天访问同权限边界」决策。

---

## 0. 根因（Symptom → Root Cause，证据链闭合）

1. **现象**：匿名 `GET https://api-dev.do.top/v1/models` → 401 `missing_api_key`（`"type":"proxy_error"`）。
2. **直接原因**：数据面 `request_filter`（crates/hydra-server/src/proxy.rs）在**任何按路径分发之前**强制客户端 api-key：
   - ① Host→租户（未知域 404，238-240）；② 租户 enabled（禁用 403，243-246）；
   - ③ 必填 api-key 解析（251-255）：无 key ⇒ 254 行立即 `short_circuit(401,"missing_api_key")`；`short_circuit` 固定输出 `{"error":{"message":"…","type":"proxy_error"}}`（1029-1036）——与线上报错逐字节吻合。
   - ④ 外部鉴权 + 指标（258-291）。
3. **目录拦截位置缺陷**：工作树 catalog 拦截位于 **④ 鉴权通过之后**（293-323，(4.5) 节；320 行固定 `Some(api_key.as_str())`），匿名请求在 254 行已被拦下，到不了目录逻辑。
4. **非目录功能引入的问题**：提交态 HEAD 同点位同样 401（HEAD proxy.rs 254 行；HEAD 无任何 catalog 代码）——`/v1/models` 在数据面**从未匿名可达**。
5. **根因结论**：强制认证闸门（③）先于路径分发 + catalog 设计决策「目录与聊天访问同权限边界」（design-tenant-model-catalog §2.2 要点 121 行）与生产要求冲突。修复 owner = proxy.rs request_filter 的步骤排序 + 目录鉴权语义修订。

## 1. 方案（Goal）

把目录拦截从 (4.5)（④ 后）**上移到必填 key 闸门之前**，并删除原 (4.5) 块：

```text
② 租户 enabled（保留：disabled ⇒ 403，匿名同样执行 —— 目录 = “该租户现在能路由什么”）
(2.5) 若 GET && req_header.uri.path()=="/v1/models"（精确路径、忽略 query）：
    api_key = Self::extract_api_key(session)      // Option，不再必填
    if Some(k):
        执行现有外部鉴权语义（cache-first auth.check + 原有指标；Denied ⇒ 401/403 原样回写）
        —— 出示了凭据就必须有效；目录对绑定 key 收窄（与现状逐字一致）
    if None:
        跳过鉴权（匿名读目录）
    entries = router::accessible_models(cfg, breaker, &tenant.id, api_key.as_deref())
              // hydra-core 纯函数；key=None ⇒ key-prefix 绑定闸门不触发（router.rs 229-233），
              // 匿名 = 租户 tenant_providers ∩ tenant_models 白名单(default-open) ∩ 熔断/weight/有key 过滤的并集
    crate::admin::metrics::record_catalog(&tenant.id)   // 每个被服务的目录请求恰好一次
    return respond_catalog(session, ctx, &entries)      // 200 本地 JSON，复用现有实现；无上游、无 usage 记录
③ 非目录路径：必填 api-key → 401 missing_api_key；④ 外部鉴权 —— 逐字不变
```

不改 hydra-core / DB / schema / 配置项；`ctx.client_api_key` 保持 Option（匿名 None），logging/metrics 全链 Option-safe（已核：`ctx.auth_verdict` 无读取点；logging 的 usage/指标/sink 全挂 `selected=Some` 分支，匿名 selected=None 全跳过；Pingora logging 钩子仍会执行，`respond_catalog` 已写 `ctx.status_code=200`，无 response_written 兜底风险）。

## 2. 语义决策表（“不能影响任何其他地方的功能”）

| # | 决策 | 理由 / 影响面 |
|---|---|---|
| S1 | 未知域 404、disabled 租户 403 保留（目录也执行 ②，匿名不跳过） | 目录语义 = “该租户现在可路由的模型”；disabled 租户不广告 |
| S2 | 出示 key ⇒ 无效 401/403 不变；有效 key + 前缀绑定 ⇒ 目录收窄到绑定 provider（不变） | 出示凭据必须有效；绑定是“哪家 provider 服务该 key 的流量” |
| S3 | 匿名 ⇒ 跳过鉴权，返回租户级模型 id 并集（无 provider 拓扑） | **有意为之**（P2-5 记录）：模型 id 非机密；绑定 key 视图 ⊆ 匿名并集是设计结果，非缺陷 |
| S4 | 仅 GET 精确 /v1/models 免认证；HEAD /v1/models、/v1/models/{id}、其它方法/路径不变（匿名仍 401，keyed 仍直通/受策略约束） | 行为面唯一变更格 = 「匿名 GET /v1/models」401→200（oracle 全组合核验） |
| S5 | 不加配置开关（默认公开） | 仓库无请求语义类开关惯例（env 均为部署配置）；生产要求绝对公开；未来如某租户需 401 再按租户 opt-out（P2-5） |
| S6 | 目录（keyed 与匿名）仍不计 count 配额（R9 延续）；匿名无角色可匹配 ⇒ 无配额 | 本地只读、CPU 有界（models_by_key × tenant_providers 遍历）、无上游、响应极小；以 hydra_catalog_requests_total 观测匿名 QPS（P2-4）；如需限频属后续独立项 |
| S7 | NonRouteStrategy 对精确目录路径的旁路 = catalog 功能既有 R8 语义（keyed 现状即如此） | (2.5) 未新增旁路面 |
| S8 | cluster/edge 共用同一 request_filter ⇒ edge 匿名目录基于 last-known-good 快照（R10 延续） | 行为一致 |

## 3. 代码改动清单（owner: crates/hydra-server/src/proxy.rs）

- 删除 (4.5) 块（293-323）；在 ② 之后插入 (2.5) 块（上移）。
- **共享 helper（P2-2 吸收）**：把 ④ 的「auth.check + 指标（record_auth_decision / record_auth_cache_size / auth_upstream_unavailable 计数）+ Denied 状态体回写」抽为私有 helper，供 (2.5)-Some(k) 与 (4) 两处调用，避免逐行复制漂移。
- `record_catalog` 恰好在每个被服务的目录请求调用一次（两子路径各一次）。
- 模块头注释（8-9 行、203-212 行步骤清单）随实现同步刷新（含 (2.5) 目录分支与 enabled 闸门）。
- 无其它文件改动（respond_catalog / catalog_json / metrics record_catalog / hydra-core accessible_models 复用）。

## 4. 测试计划（crates/hydra-server/tests/terminate_mode.rs，目录用例区 1438+）

新增（匿名 pins，把“唯一行为面变更”钉死，P2-1 吸收）：
- 匿名 GET /v1/models（不带任何认证头）⇒ 200 + 白名单并集 + **零上游断言**（wiremock received_requests 判空）；
- 匿名 GET /v1/models?x=1 ⇒ 200（query 忽略）；
- 匿名 HEAD /v1/models ⇒ 401（方法边界，负向钉住——仓库现无 HEAD 用例，此为新增基准）；
- 匿名 GET /v1/models/{id} ⇒ 401（负向钉住——现有 /{id} 用例只覆盖 keyed）；
- disabled 租户匿名 GET /v1/models ⇒ 403；未知域 ⇒ 404（负向钉住）。

钉住既有语义（不改不破）：无效 key ⇒ 401（现有 `catalog_get_v1_models_unauthorised_key_returns_401`）；有效 key + 绑定收窄（现有）；keyed /{id} 直通（现有）；keyed union/零上游/无 providers 200 空（现有）。回归：POST chat 全流程、限流、熔断、admin_api 全套既有用例。

门禁命令（对齐 ci.yml）：`cargo fmt --check`、`cargo clippy --workspace --all-targets --features hydra-server/server -- -D warnings`、`cargo test -p hydra-core`、`cargo test -p hydra-server --features server`。

## 5. 发布/部署顺序（P1-1，oracle 阻塞级要求，已吸收）

- **本修复不是可在 HEAD 独立热修的补丁**：`(2.5)` 依赖的 `accessible_models`/catalog_json/respond_catalog/record_catalog/hydra_catalog_requests_total 全部只存在于工作树（HEAD 无 catalog 代码，git grep 可证）。若把「闸门上移」单独 cherry-pick 到 HEAD 将无法编译。
- **部署顺序**：catalog 功能（设计文档 Task 1-3，含本计划的 (2.5) 修订）作为**单一 release** 合入并发布；不存在先于 catalog 合入的 HEAD 独立热修路径。
- 若确需在 catalog 合入前缓解线上（例如客户端健康检查依赖 200），需另走一条**最小降级实现**（在 ③ 前对匿名 GET /v1/models 早退 200 空/占位目录，不计算白名单并集、无 provider 语义）——属降级方案，本次**不采纳**，仅记录取舍：其语义达不到“租户可调用目录”，且会引入第二个临时路径（违反 minimality），等 catalog release 一到即被取代。

## 6. 文档同步清单（P2-3，oracle 全量核对）

实施时一并更新，避免 doc-code 矛盾：
- dev-docs/design-tenant-model-catalog.md：§2.2 拦截点（“auth 判定通过后”→(2.5) 免认证）+ 要点 121 行（“同权限边界，未配置/失效 key 依旧 401/403” → 拆为 匿名→200 / 出示失效 key→401/403）+ 错误契约表 130-137 行（“缺 key/auth 拒绝 | 401/403”行修订）+ §6 R9（247 行，匿名下“auth 缓存”防线失效措辞）+ TL;DR（13-14 行）与开头“对应需求”的“租户 token”前提改写 + §8 修订记录追加本缺陷轮次；
- dev-docs/design.md：632 行例外叙述“租户鉴权通过后在本地聚合应答” → “（免认证）本地聚合应答租户模型目录；调用仍须鉴权”；
- README.zh-CN.md：125 行“用同一客户端 api-key 请求 GET /v1/models” → 注明目录**免认证可读**（列出本租户可调用模型），调用/聊天仍须 api-key；
- dev-docs/ops.md：数据面探针叙述（326-360，直连 provider 端点）不需改动；如需可在端点表中注明目录匿名可读；
- crates/hydra-server/src/proxy.rs 模块头注释（见 §3）；
- 本计划 + dev-docs/aegis/INDEX.md 登记。

## 7. 风险与决策记录

| # | 风险 / 决策 | 处理 |
|---|---|---|
| R1 | 行为变更面 | 全组合核验仅「匿名 GET /v1/models」401→200；出示 key 的全部语义逐字不变 |
| R2 | 匿名滥用 / 无配额 | 本地只读 + CPU 有界 + 无上游；以 hydra_catalog_requests_total 观测；租户级目录限频 = 后续独立项（P2-4） |
| R3 | 与既有设计冲突 | 修订 catalog §2.2「同权限边界」决策；本计划为权威记录（GATE 通过） |
| R4 | HEAD / 匿名 /{id} 无既有基准 | 新增负向钉住测试（§4） |
| R5 | (2.5) 与 (4) 鉴权代码重复 | 抽共享 helper（P2-2） |
| R6 | 匿名并集 ⊇ 绑定 key 收窄视图 | 有意为之（S3 / P2-5）；文档记录接受面 |
| R7 | 发布顺序 | 与 catalog 同一 release（P1-1 / §5），无 HEAD 独立热修路径 |

## 8. oracle 复核修订记录

| 轮次 | 结论 | 修订 |
|---|---|---|
| v1（本会话） | **GATE: PASS**（P0=0；P1-1 补发布/部署顺序声明后实施；P2-1..P2-5 按序吸收，均不阻塞） | P1-1→§5 发布/部署顺序（HEAD 无独立热修路径；catalog 同 release；最小降级方案不采纳仅记录）；P2-1→§4 匿名负向钉住（匿名 HEAD 401 / 匿名 /{id} 401 / query 忽略 / disabled 403 / 未知域 404）；P2-2→§3 抽共享鉴权 helper + record_catalog 恰一次；P2-3→§6 文档清单补 TL;DR/要点 121/R9/错误契约/§8/模块头注释；P2-4→§7 R2（匿名观测面）；P2-5→§2 S3+S5（默认公开不加开关成立；未来按租户 opt-out） |

---

## 9. 实施记录（2026-09-08，已完成）

- **代码**（`crates/hydra-server/src/proxy.rs`）：目录拦截上移为 **(2.5)**（② enabled 之后、③ 必填 key 之前）；新增共享 `enforce_auth` helper（cache-first 外部鉴权 + §17 指标 + Denied 401/403 回写），供 (2.5)-出示 key 子路径与 (4) 共用；删除原 (4.5) 块；(3)/(3')/(4) 重新编号；模块头与 request_filter 步骤注释同步刷新。
- **语义**：匿名 GET /v1/models ⇒ 200 本地目录（跳过外部鉴权，零 auth 调用、零上游）；出示 key ⇒ 外部鉴权原样（失效 401/403）并按前缀绑定收窄；HEAD / `/v1/models/{id}` / 其它路径与 chat、管理面、限流、熔断、cluster/edge 全部不变——唯一行为面 = 缺失 key 的 GET /v1/models 401→200。
- **测试**（`crates/hydra-server/tests/terminate_mode.rs`，新增 6 用例全绿）：匿名 200 白名单并集 + 零上游 + 零 auth 调用；query 忽略；匿名 HEAD ⇒ 401；匿名 /{id} ⇒ 401；disabled 租户匿名 ⇒ 403；未知域匿名 ⇒ 404。
- **文档**：README.zh-CN（模型目录免认证说明）、dev-docs/design.md §6.3a、dev-docs/design-tenant-model-catalog.md（§2.2 拦截点/伪码/要点、错误契约、R9、状态、§8 v4）、本计划、dev-docs/aegis/INDEX.md（2026-09-08 行）。
- **门禁（全绿）**：`cargo fmt --check` OK；`cargo clippy --workspace --all-targets --features hydra-server/server -- -D warnings` OK；`cargo test -p hydra-core` 全绿（router 28 / extract 9 / swrr 8 / …）；`cargo test -p hydra-server --features server` 全绿（terminate_mode 25 含 6 新用例、admin_api 26、cluster 6、http_auth 22、tls 4、…）。
- **部署顺序（P1-1）**：与租户模型目录 catalog 功能（Task 1-3）**同一 release** 合入并发布；HEAD 无独立热修路径。
