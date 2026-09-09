# 实施计划：认证拒绝原因透传 —— insufficient_balance ⇒ HTTP 402（Payment Required）

- **日期**：2026-09-09
- **状态**：待 oracle 架构复核（未复核，P0/P1/P2 未定）→ 复核通过后启动开发；上游契约已源码实证（见 §0.7 / §9）
- **作者**：编码智能体（DeepSeek）｜发起：用户产品决策（欠费应以 402 透出，而非笼统 401）
- **对应需求**：租户认证服务拒绝调用方 api-key 时若原因为「欠费」（`reason:"insufficient_balance"`），Hydra 应向客户端返回 HTTP **402 Payment Required**，错误体 `type:"insufficient_quota"`（主流网关对齐）并透传原因——而不是现在的统一 `401 {"error":{"message":"denied","type":"auth_error"}}`。客户端无法区分「key 无效」与「余额不足」。

---

## 0. 根因（Symptom → Root Cause，证据链闭合）

1. **现象**：认证回源（POST `tenant.auth_url`）判定为拒绝时，无论上游以何种方式表达「欠费」，客户端收到的都是 `401`（`message:"denied"`）；仓库内不存在任何 `402` / `insufficient_balance` / balance 处理（`grep -rn 402|insufficient_balance|balance crates dev-docs` 无有效命中）。
2. **in-band 原因被丢弃**：2xx 拒绝体只被扫描两个布尔字段——`body_says_denied` 仅看 `"status":false` / `"allowed":false`（http.rs:592-596）；body 里 `"reason":` 字段从未读取（http.rs 测试夹具出现过 `{"status":false,"reason":"invalid_key"}`，但只断言布尔）。判定分支（http.rs:434-442）写死 `Denied{status:401, reason:"denied"}`。
3. **判定结果只有「静态标签」**：`AuthVerdict::Denied{status, reason: &str}`（hydra-core/src/auth.rs:55-58）仅携带编译期固定标签（`denied`/`no_auth_url`/`auth_upstream_unavailable`），无任何业务原因通道；`proxy.rs enforce_auth` 把标签原样写进 `{"error":{"message":…,"type":"auth_error"}}`。
4. **out-of-band 402 也透传不过去**：`apply_upstream`（hydra-core/src/auth.rs:127-139）只把 401/403 映射为拒绝；**402 落在「其它状态」→ `CacheOp::None` → 按 §11.4 fail_mode（默认 closed）返回 503**，更不会以 402 回给客户端。上游 403 同样被压平成 401（http.rs:464-475 写死 `status:401`）。
5. **契约缺定义**：design.md §11.3（1044-1090）响应契约只有 `status/allowed/expires_in`；`reason` 仅在「可选精细化响应体」示例出现且纯为信息性；无 reason→HTTP 状态映射定义。
6. **结论**：认证边界只保留「过/不过」一个比特，拒绝原因与精确语义状态码在网关上丢失。根因 owner = hydra-core auth 判定层 + http.rs `HttpAuthChecker::check` 两个拒绝分支（in-band / out-of-band）+ 契约文档 §11.3。
7. **上游契约源码实证（2026-09-09，`/home/alex/Projects/api`）**：`crates/api/src/auth/handler.rs` `auth_api_key`（378-511 行）——**所有分支恒返回 HTTP 200**（`StatusCode::OK`），允许/拒绝一律经 body `AuthApiKeyResponse{status, reason, user_id}`（86-91 行）表达：欠费（arrears check false，435 / 451-465 行）⇒ `status:false, reason:"insufficient_balance"`（小写精确）；key 空/未找到 ⇒ `reason:"invalid_key"`；内部错误 ⇒ `reason:"internal_error"`；放行 ⇒ `status:true, reason:""`。该端点**从不回 HTTP 402**（grep 402 仅命中 `crates/api/src/error.rs:47/103` 的 `AppError::InsufficientBalance ⇒ PAYMENT_REQUIRED`，用于钱包/充值等公开 REST 错误层，不用于本端点）——即生产路径 = **in-band reason**；out-of-band 402 仅作其它租户/未来兼容保留。

## 1. 方案（Goal）

把「欠费 ⇒ 402」接入认证判定层，其余语义不动：

- 新增**纯函数** `reason → denial HTTP status` 映射（hydra-core::auth，延续「判定逻辑在纯核心、shell 只做 I/O」分层）；
- in-band（**生产主路径**，上游 Dogress 实证）：2xx 拒绝体携带 `reason` 且大小写不敏感命中 `insufficient_balance` ⇒ 客户端 **402**，错误体 `{"error":{"message":"insufficient_balance","type":"insufficient_quota"}}`，**不写 deny 缓存**；
- out-of-band（兼容保留）：auth_url 直接返回 HTTP 402 ⇒ 客户端 **402**（不再落入 `CacheOp::None` → 503），不缓存；
- 未知 / 缺失 reason 与 401/403 上游 ⇒ 维持现状（401 `denied`，缓存语义不变）——向后兼容，上游未变更前行为零变化；
- admin `tenant_auth_test` 的 402 归类从「unexpected_status/Fail」改为拒绝 PASS（并展示细分 verdict）；
- 测试：纯函数单测 + auth 判定集成（wiremock）+ terminate_mode 端到端 402 pin + admin 归类用例；全套门禁。

## 2. 语义决策表（「不能影响任何其他地方的功能」）

| # | 决策 | 理由 / 影响面 |
|---|---|---|
| S1 | 双信号源均映射 402：① 2xx 拒绝体 reason 命中 `insufficient_balance`（in-band，Dogress 生产路径）；② auth_url 直回 HTTP 402（out-of-band） | Dogress 恒 200 语义（§11.3，源码实证）与标准 HTTP 语义都要接住；两端实现同一判定出口 |
| S2 | reason 匹配 = trim 后**大小写不敏感**（`eq_ignore_ascii_case`，兼容 `Insufficient_Balance` 等变体）；未知/缺失 ⇒ 401 `denied` 兜底（现状） | **用户决策（2026-09-09）**：大小写不敏感；未知 reason 仍 401 fail-safe 防误判；未来扩展 reason 表只需加行（映射纯函数集中管理） |
| S3 | **402 拒绝不写 AuthCache**（L1，cluster 下亦不写 Redis L2） | 余额是快速变化状态：写入 deny_ttl(30s) 会把 402 在 TTL 内降级成 401，客户端误判「key 无效」、重试策略错误；每次请求实时回源由租户侧决定（其自有余额缓存承担压力） |
| S4 | 客户端错误体：HTTP **402** + `{"error":{"message":<reason标签>,"type":"insufficient_quota"}}`（in-band ⇒ `message:"insufficient_balance"`；out-of-band 402 ⇒ `message:"denied"`）；非 402 拒绝维持 `type:"auth_error"` | **用户决策（2026-09-09）**：402 时 `type:"insufficient_quota"` 与主流 OpenAI 兼容网关对齐；`proxy.rs enforce_auth` 按 `Denied.status` 选 type |
| S5 | reason→status 映射放 **hydra-core::auth 纯函数**（表驱动 + 单测）；http.rs 只做「读 reason / 调映射 / 决定缓存否」 | 延续 `http.rs` 模块头铁律（判定语义可单测、可复用，admin test 与 proxy 共用同一口径的映射） |
| S6 | 影响面收敛：只改**拒绝分支**；allow 路径、cache Hit(allow)、路由、catalog、fail_mode（503）一律不变；上游 403 仍压平为 401（403→403 透传属另案，本次不展开） | 单一行为面 = 「欠费类拒绝 401→402」；全量回归兜底 |
| S7 | admin `tenant_auth_test`（admin/handlers.rs:1567-1631）：HTTP 402 从 `other`（Fail）改为**拒绝 PASS**；2xx 拒绝带欠费 reason 时 `verdict` 细分为 `insufficient_balance` | 诊断工具与代理语义一致；fake-key 探测 402 = 端点可用（它拒绝了一切 key） |
| S8 | metrics：`hydra_auth_decision_total{verdict="denied"}` 自然涵盖 402；如需按 status 维度区分属 P2 | 观测面已存在，无需新计数器 |

## 3. 代码改动清单

### 3.1 hydra-core/src/auth.rs（owner：纯映射）

- 新增常量 `pub const REASON_INSUFFICIENT_BALANCE: &str = "insufficient_balance";`
- 新增纯函数（表驱动，未来扩展点）：
```rust
/// Map a tenant auth denial reason to the downstream HTTP status.
/// Currently only insufficient_balance is specialised (402, case-insensitive
/// after trim); any unknown / missing reason stays 401 (legacy behaviour).
pub fn denial_status_for_reason(reason: Option<&str>) -> u16
```
  （trim 后 `eq_ignore_ascii_case("insufficient_balance")` ⇒ 402——用户决策大小写不敏感；其余 / None ⇒ 401）
- 模块 doc 注明：402 语义 + 调用方（http.rs in-band / out-of-band 两端）+ 「402 拒绝不缓存」由调用方执行。

### 3.2 crates/hydra-server/src/http.rs（owner：HttpAuthChecker::check）

- **in-band 分支**（现 2xx+denied 的 401 拒绝块 http.rs:434-442 之前）：新增轻量 `json_string_field`（风格对齐 `parse_expires_in`/扁平扫描）读取 `"reason"`；trim 后大小写不敏感命中 `insufficient_balance` ⇒ `return AuthVerdict::Denied { status: 402, reason: REASON_INSUFFICIENT_BALANCE, source: CacheSource::Miss }`（**不 set 缓存**）；否则走现有 401+缓存逻辑。
- **out-of-band 分支**：在 status→`apply_upstream` 判定（http.rs:424-425）之前特判 `status == 402` ⇒ `return Denied { status: 402, reason: "denied", source: Miss }`（不缓存）——402 永不落入 `CacheOp::None` → 503 路径。
- 复用 `denial_status_for_reason` 统一 in-band 映射（S5）；缓存与否的规则（status==402 ⇒ 不 cache）在调用点用同一常量判断，注释写明 S3 理由。
- 单测（http.rs mod tests）：`json_string_field` 正常/缺失/空白/引号转义。

### 3.3 crates/hydra-server/src/admin/handlers.rs（owner：tenant_auth_test 归类）

- match（1567-1631）：新增 `402` 分支 ⇒ `(true, true, "denied", …)`；2xx 拒绝分支在 `body_says_denied` 为真时读 reason，命中欠费 ⇒ `verdict:"insufficient_balance"`、detail 文案说明「欠费拒绝 = 端点可用」。

### 3.4 其它

- `proxy.rs enforce_auth`（222-229 行错误体写法）：按 `Denied.status` 选 `type`——`402` ⇒ `"insufficient_quota"`（S4），其余（401/503）维持 `"auth_error"`；`message` = `verdict.reason`（in-band 欠费 ⇒ `"insufficient_balance"`）。函数 doc 注释同步（status 可为 402，message 即原因标签）。
- 无 schema / migration / 配置项 / admin 路由 / hydra-cli 改动。

## 4. 测试计划

- **hydra-core（auth.rs 纯函数单测）**：`denial_status_for_reason` 表驱动——`insufficient_balance`→402；**大小写变体**（`Insufficient_Balance` / `INSUFFICIENT_BALANCE`）→402（S2 钉住）；`invalid_key`/空/None→401；前后空格 trim。
- **hydra-server http_auth.rs（wiremock 判定集成）**：
  1. 200 `{"status":false,"reason":"insufficient_balance"}` ⇒ 客户端 402；**不写缓存**：同 key 连发两次，断言 auth_url 被调 2 次且两次均 402（钉住 S3）；
  2. auth_url 直回 HTTP 402 ⇒ 客户端 402（且不缓存，同上）；
  3. 200 `{"status":false,"reason":"invalid_key"}` / 无 reason ⇒ 401（现状回归）；
  4. auth_url 401 / 403 ⇒ 401 + 写缓存（现状回归）；
  5. 200 `{"status":true}` ⇒ allow（回归）；5xx ⇒ 503 fail-closed（回归）；
  6. 200 `{"status":false,"reason":"Insufficient_Balance"}`（大小写变体）⇒ 402（S2 钉住）。
- **terminate_mode.rs（端到端）**：参照 `error_401_when_auth_denied` 新增 chat 用例——mock auth 返回 `reason:"insufficient_balance"` ⇒ HTTP **402** + body `{"error":{"message":"insufficient_balance","type":"insufficient_quota"}}`（S4 钉住 type）；另 401 拒绝 body 维持 `type:"auth_error"`（回归）。
- **admin_auth_test.rs**（`tenant_auth_test` 探针套件，非 admin_api.rs）：auth_url 回 402 ⇒ `ok:true` verdict `denied`（原 unexpected_status/Fail）；2xx 拒绝 + 欠费 reason ⇒ verdict `insufficient_balance`。
- **门禁**：`cargo fmt --check`、`cargo clippy --workspace --all-targets --features hydra-server/server -- -D warnings`、`cargo test -p hydra-core`、`cargo test -p hydra-server --features server`（http_auth / admin_auth_test / admin_api / terminate_mode 等全量回归 + 新增用例）。

## 5. 发布/部署顺序

- **Hydra 侧可独立先行**：上游（Dogress）尚未变更前，本改动行为零变化（未知 reason ⇒ 401 现状）——向后兼容，无发布阻塞。
- **端到端见效无需上游改动**（§9 结论 1）：Dogress `crates/api` `/auth/api_key` 已对欠费输出 `status:false, reason:"insufficient_balance"`（HTTP 200），Hydra in-band 映射即接住；out-of-band 402 支持为其它租户/未来兼容，Dogress 本端点不会触发。
- 无 DB/配置/schema 变更；随常规 release 发布即可。

## 6. 文档同步清单（实施时一并更新）

- **dev-docs/design.md**：
  - §11.3 响应契约表新增 `402 | 拒绝（欠费，余额不足） | 透传 402，不缓存` 行；
  - §11.3 「响应体判定」补：2xx 拒绝体可携带 `reason`，大小写不敏感命中 `insufficient_balance` ⇒ 402（不缓存）；其余 reason ⇒ 401；
  - §11.3 客户端错误契约补：402 响应 `{"error":{"message":…,"type":"insufficient_quota"}}`（主流网关对齐）；
  - §11.2 拒绝缓存说明补「402 类拒绝不缓存（余额为快速变化状态）」；
  - §6.3 错误状态叙述（含 401/503 处，若成表则补行）说明拒绝可携带 402 及原因标签；
  - 1166 行「欠费/阻断全由租户自决」措辞补一句「欠费拒绝以 402 + `insufficient_balance` 向客户端透出」。
- **dev-docs/ops.md**：auth 端点排查/错误码叙述核查，补 402 一行（如该处有成表）。
- **README.zh-CN.md**：客户端错误段若列 401/403/503 则补 402 语义说明（核查后按实际）。
- **本计划 + dev-docs/aegis/INDEX.md** 登记；实施后回填 §10 实施记录与 §8 复核记录。

## 7. 风险与决策记录

| # | 风险 / 决策 | 处理 |
|---|---|---|
| R1 | 缓存降级：402 若写 deny 缓存，30s 内被降级成 401，客户端误判 key 无效 | **S3 决策**：402 拒绝不缓存（含 L2）；同 key 每次回源实时判定 |
| R2 | 欠费期每请求回源 auth_url | 有界成本（一次 POST/key）；AuthCache 不缓存属有意取舍，auth 服务侧通常有余额缓存/高可用；以 `hydra_auth_decision_total` / `hydra_auth_upstream_error_total` 观测；如需按 key 短时去重属后续项（P2） |
| R3 | reason 契约漂移（值变化 / 第三方语义） | S2 大小写不敏感（`eq_ignore_ascii_case`）+ 未知 reason ⇒ 401 兜底（fail-safe，宁可保守不误判 402）；上游 `internal_error` 维持 401 现状不变；契约变更需显式更新 §11.3 与映射表 |
| R4 | 402 对 OpenAI 兼容客户端的影响 | 非 2xx 即报错路径；401→402 变化仅发生在欠费期且语义更准确（客户端可提示充值而非换 key）；`type:"insufficient_quota"` 与主流网关一致 |
| R5 | 影响面扩散 | S6 收敛：仅拒绝分支；403 透传 / 其它 reason 特判等明确列为另案或 P2，避免连带改动 |
| R6 | admin auth test 语义翻转（402 Fail→PASS） | S7 同步，避免运维误判「端点异常」 |
| R7 | fail_mode=open 下上游裸 402 此前落入 `CacheOp::None` → fail-open 会**放行**；新增 out-of-band 特判改为拒绝 402 | 显式 402 是拒绝而非可用性异常——修复后 open 模式亦拒绝（oracle P2-8 记录；行为面收敛于欠费拒绝） |
| R8 | 对合规 Dogress 上游，admin 探针的 `insufficient_balance` 细分 verdict 不可达（欠费检查在 key 查中之后，fake-key 得 `invalid_key`） | 保留为诊断用途（仅非合规上游触发），detail 文案已说明（oracle P2-8） |

## 8. oracle 复核修订记录

| 轮次 | 结论 | 修订 |
|---|---|---|
| v1（2026-09-09 oracle 复核） | **GATE: PASS**（P0=0，P1=0；8×P2 按序吸收，均不阻塞） | P2-1→§4 测试套件更正为 admin_auth_test.rs；P2-2→core 单测落 tests/auth.rs（无内联 cfg(test)）；P2-3→Redis L2 不变式：402 分支不调 cache::set ⇒ L1/L2 均不写（结构保证 + 代码注释）；P2-4→既有 invalid_key deny 缓存用例（http_auth auth_upstream_200_body_status_false_denies_cached）作回归护栏；P2-5→文档同步追加 ops.md/源码注释（auth.rs:55、proxy.rs enforce_auth doc、handlers 探针 doc）；P2-6→out-of-band 402 由 http_auth 层钉住（proxy type 分支经 in-band e2e 全路径覆盖）；P2-7→reason 扫描沿用扁平契约风格并补 json_string_field 单测；P2-8→§7 新增 R7/R8 行为说明 |

## 9. 契约对齐结论与开放项（2026-09-09 调查后更新）

1. **[已实证，无需 Dogress 改动] 欠费输出形态**：`/home/alex/Projects/api/crates/api/src/auth/handler.rs:451-465`——`/auth/api_key` 恒 HTTP 200 + `{"status":false,"reason":"insufficient_balance"}`（小写）。hydra in-band 路径即生产主路径；out-of-band 402 保留为其它租户/未来兼容（Dogress 本端点不会触发）。
2. **[已决策] reason 匹配**：大小写不敏感（S2）；`insufficient_balance` / `invalid_key` / `internal_error` 词汇写入 §11.3 契约供两端参考（Hydra 只特判 `insufficient_balance`）。
3. **[已决策] 客户端错误体**：402 ⇒ `{"error":{"message":…,"type":"insufficient_quota"}}`（S4），与主流 OpenAI 兼容网关对齐；本计划不再留 P2。

---

## 10. 实施记录（2026-09-09，已完成）

- **代码**：
  - hydra-core/src/auth.rs：`REASON_INSUFFICIENT_BALANCE` + 纯函数 `denial_status_for_reason`（trim + 大小写不敏感），Denied doc 补 402；
  - crates/hydra-server/src/http.rs：新增 `json_string_field`（带引号 key、扁平扫描）；in-band 2xx 拒绝分支 reason 命中欠费 ⇒ 402 且不缓存（其余拒绝 401+缓存不变）；out-of-band 上游直回 402 ⇒ 402 不缓存（先于 apply_upstream / fail_mode）；
  - crates/hydra-server/src/proxy.rs：`enforce_auth` 按 status 选 type——402 ⇒ `insufficient_quota`，其余维持 `auth_error`；
  - crates/hydra-server/src/admin/handlers.rs：`tenant_auth_test` 新增 402=PASS 分支、2xx 拒绝 reason 细分 `insufficient_balance`。
- **测试**：hydra-core tests/auth.rs T7.8 表驱动（含大小写变体）；http.rs `json_string_field` 单测；http_auth.rs 新增 in-band 402 不缓存（两次回源）/ 大小写变体 / 裸 402 三用例；terminate_mode.rs 新增 `error_402_when_auth_denied_insufficient_balance`（402 + body `type:"insufficient_quota"` + 零上游）；admin_auth_test.rs 新增 402 PASS 与 `insufficient_balance` 细分两用例；既有 invalid_key deny 缓存回归用例原样通过。
- **门禁（全绿）**：`cargo fmt --check` OK；`cargo clippy --workspace --all-targets --features hydra-server/server -- -D warnings` exit 0；`cargo test -p hydra-core` exit 0；`cargo test -p hydra-server --features server` exit 0（terminate_mode 26 含新用例、http_auth / admin_auth_test / admin_api / cluster 等全量）。
- **文档**：dev-docs/design.md §11.2 / §11.3 / §6.3 / §11.7（402 契约 + `type:"insufficient_quota"` + 不缓存语义）；dev-docs/ops.md §5 补「402 拒绝不缓存」；本计划 §8 / §10；aegis INDEX.md 已登记。
