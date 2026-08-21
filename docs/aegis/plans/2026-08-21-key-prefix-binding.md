# 实施计划：api-key 前缀 → Provider 绑定路由闸门（Key Prefix Binding Gate）

- 日期：2026-08-21
- 状态：待审核（审核通过后启动开发）
- 作者：编码智能体（DeepSeek）
- 对应需求：在「候选集之前的门」阶段新增一组配置，将固定前缀的客户端 api-key 与指定 Provider 绑定（如 `sk_aaa_` → Provider A、`hk_bbb_` → Provider B）

## Goal

在 Hydra 网关中新增 **api-key 前缀 → provider 绑定** 能力：

1. 新增配置表 `provider_key_binding`（迁移 0006）+ 全链路 CRUD（db 层 / admin REST / admin-UI / 可选 hydra-cli）；
2. 在纯核心 `router::resolve` 的候选集计算中加入**绑定闸门**：客户端 api-key 命中绑定前缀时，候选集被限制为该 provider（fail-closed）；
3. 无 model 的 passthrough 路径同样遵守绑定闸门；
4. 全套测试（core 纯函数 / db CRUD / loader / admin HTTP）+ 文档更新。

## Architecture

```
客户端请求 (Authorization: Bearer sk_aaa_xxx)
  → proxy.rs request_filter
      → ① 域名→租户  ② api-key 解析（原始值）  ③ 外部认证  ④ 读 body  ⑤ 提取 model
      → ⑦ 前置限流（matching_key，用脱敏 key —— 不变）
      → ⑧ router::resolve(cfg, breaker, tenant, model_key, Some(&api_key))
          step 0   TenantModel 闸门（default-open）
          step 1-2 models_by_key ∩ tenant_providers
          step 3   交集（fail-closed）
          step 3.5 ★ 新增：key-prefix 绑定闸门
              match_key_binding(cfg.key_prefix_bindings, api_key)
              命中 → 候选集 retain 为绑定 provider；为空 → NoAvailableProvider(503)
              未命中 → 不限制
          step 4   过滤（dead / keyless / weight≤0）—— 不变
      → swrr::order → failover loop
  → 无 model 时 passthrough_candidates(cfg, tenant_id, Some(&api_key))
      ★ 同样先查绑定：命中则只允许绑定的 provider
```

数据流：`provider_key_binding` 表 → `db::list_provider_key_bindings`（只取 enabled）→ `ConfigData.key_prefix_bindings: Vec<ProviderKeyBinding>`（ArcSwap 热加载）→ 纯函数 `match_key_binding`（最长前缀优先）→ `resolve` step 3.5 闸门。

## Tech Stack

- Rust workspace（hydra-core 纯核心 / hydra-server I/O 壳，Pingora + sqlx + tokio）
- SQLite（`sqlx::query!` / `query_as!` 编译期校验 + `.sqlx/` 离线缓存，CI `SQLX_OFFLINE=true`）
- admin REST：Pingora `ServeHttp` 手写路由（`admin/mod.rs`）+ 处理器（`admin/handlers.rs`）
- admin-UI：无构建步骤的静态 HTML/JS（`include_dir!` 编译期嵌入），CRUD 声明式配置
- 测试：cargo test（core 纯函数 + server 集成测试 `:memory:` SQLite + 真实 HTTP）

## Baseline / Authority Refs

- `docs/design.md` §4.1（schema）、§5.2/§5.3（loader / ConfigData）、§7.1（候选计算）、§10.1（limit_role matching 语义，绑定表沿用其 enabled 过滤约定）、§13（admin REST）、§14（admin-UI）
- `docs/waves/wave-1-pure-core.md`（纯核心约定：resolve 候选管线）、`wave-4-proxy-shell.md`（request_filter 生命周期）
- `crates/hydra-core/src/router.rs`、`config.rs`、`model.rs`；`crates/hydra-server/src/{db,store,proxy,admin}/*`
- `docs/HANDOFF.md`（sqlx prepare 流程、测试基数：hydra-core 101 / hydra-server 159）
- 需求来源：用户本轮对话（全局绑定 / 最长前缀优先 / fail-closed / 1:1，四项均已确认）

### Requirement Ready Check

- Requirement source refs：用户本轮需求 + 4 项设计决策确认（ask_user_question 结果）
- Goals and scope refs：上表 Goal
- Acceptance / verification criteria refs：Task 各自的 Verification 命令 + 最终全量 `cargo test`
- Open blocker questions：无
- Decision: **ready**

### Change Necessity

- User-visible need：不同前缀的客户端 api-key 必须路由到指定后端
- No-change / non-code option：无——现有路由管线（tenant × model 交集）没有 key 维度，仅靠现有表无法表达该约束
- Why code change is necessary：需要新表（持久化）、新 ConfigData 维度（热加载）、resolve 新闸门（纯核心）、CRUD + UI（管理面）
- Minimum change boundary：`hydra-core`（model/config/router）+ `hydra-server`（migration/db/store/proxy/admin）+ `admin-ui/app.js` + 测试 + 文档
- Decision: **code-change**

### Existence Check

- Proposed new surface：`provider_key_binding` 表 / `ProviderKeyBinding` 模型 / `/api/v1/provider-key-bindings` 资源 / admin-UI 新 section
- Existing owner / reuse candidate：复用现有 CRUD 工厂模式（db.rs Row 结构、handlers collection/item、app.js CRUD 声明、hydra-cli ENTITY_DEFS）；不新建框架
- Why existing surface is insufficient：现有 `tenant_provider` 是租户维度授权，`limit_role.matching_key` 只做限流不改变路由，均无法表达「key 前缀 → 指定后端」
- Creation proof：见 Task 1–8 的完整代码
- Entropy / retirement impact：纯增量；若未来升级为按租户绑定，本表保留为全局层（兼容）
- Decision: **add-with-proof**

### Architecture Integrity Lens

- Invariant：路由候选集 = 模型在线 ∩ 租户授权 ∩ 绑定闸门 ∩ 存活过滤；绑定闸门是**约束**而非偏好（fail-closed）
- Canonical owner / contract：候选集唯一入口 `router::resolve`（纯核心）；绑定匹配唯一入口 `router::match_key_binding`
- Responsibility overlap：无——限流（limit.rs）管 429，路由（router.rs）管选后端，认证（http.rs）管放行，三者保持独立
- Higher-level simplification：绑定表与 limit_role 共用「只加载 enabled」的 loader 约定与 Row 转换模式，无重复实现
- Retirement / falsifier：若某天需要「同前缀不同租户不同后端」，全局 1:1 模型不满足 → 触发表结构升级（加 tenant_id），届时本表按 anti-entropy 规则迁移
- Verdict：通过

### Plan Pressure Test

- Owner / contract / retirement：新 owner = 绑定表 + 匹配函数，owner 清晰；`resolve` 签名变化有全量调用点清单
- Architecture integrity：见上，无更高层路径被跳过
- Verification scope：每 Task 有精确命令；最终全量 `cargo test` 两个 crate + fmt + clippy
- Task executability：每步含完整代码，可 2–5 分钟执行
- Pressure result: **proceed**

### Complexity Budget

- Artifact class：新增实体（表 + 模型 + CRUD 5 函数 + 2 handler + 1 路由段 + 1 UI section）
- Target files：见 File Map（约 16 个文件，含测试）
- Current pressure：低（现有 CRUD 均为同一工厂模式）
- Projected post-change pressure：低（完全镜像 limit_role/tenant_provider 模式）
- Budget result: **within-budget**
- Planned governance：不新建模块文件；绑定匹配函数放 `router.rs`（与闸门同 owner）

## Compatibility Boundary

- `router::resolve` 公开签名增加第 5 参 `client_api_key: Option<&str>` —— 仅 workspace 内部使用，无外部消费者；所有调用点（proxy.rs + core/server 测试）在本计划内一次性更新
- `ConfigData` 新增字段 `key_prefix_bindings` —— 派生 `Default` 自动覆盖；唯一显式字面量 `tests/validate.rs:275` 同步补字段
- 迁移 0006 纯增量（`CREATE TABLE`），存量 DB 原地升级
- 客户端侧协议零变化（不新增/不修改任何客户端请求头或路径）
- admin REST 新增资源段（增量）；admin-UI 新增导航 section（增量）
- 不做：per-tenant 绑定、前缀 → 多 provider、按前缀加权

## TDD Route

```text
TDD Route:
- Mode: off
- Decision: skipped
- Strict authority: not applicable（用户未要求 strict TDD）
- Test posture: post-change regression（每个 Task 附精确验证命令；核心 gate 用纯函数测试）
- Reason: 仓库已有完备测试套件与 CI 门禁（fmt/clippy -D warnings/双 crate 全量测试）；按 Task 增量补回归即可
- Verification: 见各 Task Verification + 最终全量命令
```

## File Map

| 文件 | 操作 | 归属 Task |
|---|---|---|
| `crates/hydra-server/migrations/0006_provider_key_binding.sql` | 新建 | 1 |
| `crates/hydra-core/src/model.rs` | 修改（+ProviderKeyBinding） | 1 |
| `crates/hydra-core/src/config.rs` | 修改（ConfigData 字段 + validate 检查） | 1 |
| `crates/hydra-core/tests/validate.rs` | 修改（字面量补字段 + 2 新测试） | 1 |
| `crates/hydra-core/src/router.rs` | 修改（match_key_binding + resolve step 3.5 + 签名） | 2 |
| `crates/hydra-core/tests/router.rs` | 修改（既有调用点 + 5 新测试） | 2 |
| `crates/hydra-core/tests/breaker.rs` | 修改（调用点补参） | 2 |
| `crates/hydra-server/src/proxy.rs` | 修改（resolve 调用 + passthrough 闸门） | 2 |
| `crates/hydra-server/tests/load_breaker_swrr.rs` | 修改（调用点补参） | 2 |
| `crates/hydra-server/src/db.rs` | 修改（Row 结构 + 5 CRUD 函数 + 导入） | 3 |
| `.sqlx/` | 刷新（cargo sqlx prepare） | 3 |
| `crates/hydra-server/src/store.rs` | 修改（build_config 加载） | 4 |
| `crates/hydra-server/src/admin/handlers.rs` | 修改（2 handler + 导入） | 5 |
| `crates/hydra-server/src/admin/mod.rs` | 修改（路由段） | 5 |
| `crates/hydra-server/tests/repo.rs` | 修改（CRUD round-trip） | 7 |
| `crates/hydra-server/tests/loader.rs` | 修改（enabled-only 加载） | 7 |
| `crates/hydra-server/tests/admin_api.rs` | 修改（HTTP CRUD + snapshot 断言） | 7 |
| `admin-ui/app.js` | 修改（CRUD section + NAV + 注释） | 6 |
| `docs/design.md` / `README.md` / `README.zh-CN.md` / `docs/HANDOFF.md` | 修改 | 8 |
| `tools/hydra-cli/src/types.ts` | 可选修改（ENTITY_DEF） | 8（可选） |

---

# Task 1 — 迁移 + 模型 + ConfigData + 校验

**Files**
- 新建 `crates/hydra-server/migrations/0006_provider_key_binding.sql`
- 修改 `crates/hydra-core/src/model.rs`、`crates/hydra-core/src/config.rs`
- 修改 `crates/hydra-core/tests/validate.rs`

**Why**：绑定配置的持久化载体与内存模型，是所有后续步骤的地基。

**Change Necessity**：无既有表能表达「key 前缀 → provider」；需新表 + 新模型 + ConfigData 新维度。

**Impact / Compatibility**：`ConfigData` 新字段由 `Default` 覆盖；`tests/validate.rs:275` 显式字面量需补字段（否则编译失败）。

**Verification**：`SQLX_OFFLINE=true cargo test -p hydra-core`（validate 全套绿）。

## 步骤 1.1 — 新建迁移文件

`crates/hydra-server/migrations/0006_provider_key_binding.sql`：

```sql
-- api-key 前缀 → provider 绑定（路由闸门，design.md §7.1b）
--
-- 客户端 api-key（Authorization: Bearer / x-api-key 的原始值）以 key_prefix
-- 开头时，路由候选集被限制为该 provider（fail-closed）。多条前缀同时命中时
-- 取最长前缀（最具体）。enabled=0 的绑定不参与匹配（loader 只加载 enabled）。
CREATE TABLE provider_key_binding (
    id          TEXT PRIMARY KEY,
    key_prefix  TEXT NOT NULL UNIQUE,          -- 客户端 api-key 前缀，如 'sk_aaa_'
    provider_id TEXT NOT NULL REFERENCES provider(id) ON DELETE CASCADE,
    enabled     INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_provider_key_binding_provider ON provider_key_binding(provider_id);
```

## 步骤 1.2 — model.rs 增加 `ProviderKeyBinding`

在 `crates/hydra-core/src/model.rs` 的 Tenant family 之后（`TenantModel` 下方）追加：

```rust
// ---------------------------------------------------------------------------
// Key-prefix binding（路由闸门, design §7.1b）
// ---------------------------------------------------------------------------

/// An api-key-prefix → provider binding (routing gate, design §7.1b).
///
/// When a client api-key's raw value starts with `key_prefix`, the routing
/// candidate set is restricted to `provider_id` (fail-closed; longest prefix
/// wins when several prefixes match). Only `enabled == true` rows are loaded
/// into `ConfigData::key_prefix_bindings` (mirrors `LimitRole`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderKeyBinding {
    pub id: String,
    /// Client api-key prefix, e.g. `sk_aaa_`. Empty prefixes are invalid
    /// (rejected at the admin handler, warned by `config::validate`).
    pub key_prefix: String,
    pub provider_id: String,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}
```

## 步骤 1.3 — config.rs：ConfigData 新字段 + validate 检查

(a) 顶部导入改为：

```rust
use crate::model::{LimitRole, Provider, ProviderKeyBinding, Tenant};
```

(b) `ConfigData` 在 `limit_roles` 字段后追加：

```rust
    /// Enabled api-key-prefix → provider bindings (design §7.1b; only
    /// `enabled == true` rows, like `limit_roles`). Matching is longest-prefix
    /// wins; see [`crate::router::match_key_binding`].
    pub key_prefix_bindings: Vec<ProviderKeyBinding>,
```

(c) `validate()` 在 limit-role 检查块后追加：

```rust
    // provider_key_bindings → prefix non-empty + provider must exist.
    for b in &cfg.key_prefix_bindings {
        if b.key_prefix.is_empty() {
            issues.push(ValidationIssue::warn(format!(
                "provider_key_binding '{}' has an empty key_prefix; it can never match",
                b.id
            )));
        }
        if !cfg.providers.contains_key(&b.provider_id) {
            issues.push(ValidationIssue::warn(format!(
                "provider_key_binding '{}' references unknown provider_id '{}'",
                b.id, b.provider_id
            )));
        }
    }
```

## 步骤 1.4 — validate.rs：字面量补字段 + 2 个新测试

(a) `validate_empty_config_is_clean`（约 275 行）字面量补 `key_prefix_bindings: Vec::new(),`：

```rust
    let cfg = ConfigData {
        tenants_by_domain: HashMap::new(),
        models_by_key: HashMap::new(),
        tenant_providers: HashMap::new(),
        tenant_models: HashMap::new(),
        providers: HashMap::new(),
        provider_keys: HashMap::new(),
        limit_roles: Vec::new(),
        key_prefix_bindings: Vec::new(),
        certs: HashMap::new(),
    };
```

(b) 文件末尾追加 helper + 2 测试（复用现有 `clean_config()` / `warn_messages()`）：

```rust
fn binding(id: &str, prefix: &str, provider_id: &str) -> hydra_core::model::ProviderKeyBinding {
    hydra_core::model::ProviderKeyBinding {
        id: id.into(),
        key_prefix: prefix.into(),
        provider_id: provider_id.into(),
        enabled: true,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    }
}

/// T9.8 — provider_key_binding references an unknown provider ⇒ Warn.
#[test]
fn validate_binding_unknown_provider() {
    let mut cfg = clean_config();
    cfg.key_prefix_bindings.push(binding("b1", "sk_", "ghost"));
    let warns = warn_messages(&validate(&cfg));
    assert!(
        warns
            .iter()
            .any(|m| m.contains("ghost") && m.contains("provider_key_binding")),
        "expected a dangling-provider warning, got {warns:?}"
    );
}

/// T9.9 — provider_key_binding with an empty prefix ⇒ Warn.
#[test]
fn validate_binding_empty_prefix() {
    let mut cfg = clean_config();
    cfg.key_prefix_bindings.push(binding("b1", "", "p1"));
    let warns = warn_messages(&validate(&cfg));
    assert!(
        warns.iter().any(|m| m.contains("empty key_prefix")),
        "expected an empty-prefix warning, got {warns:?}"
    );
}
```

**验证（Task 1 完成点）**：`SQLX_OFFLINE=true cargo test -p hydra-core` 全绿（validate 套件含新 2 测试）。

---

# Task 2 — 纯核心绑定闸门（router）+ 全调用点更新

**Files**
- 修改 `crates/hydra-core/src/router.rs`、`crates/hydra-core/tests/router.rs`、`crates/hydra-core/tests/breaker.rs`
- 修改 `crates/hydra-server/src/proxy.rs`、`crates/hydra-server/tests/load_breaker_swrr.rs`

**Why**：核心交付物——绑定闸门进入候选集计算，是本次需求的本质。

**Change Necessity**：候选集管线（step 0–5）没有 key 维度，必须在 `resolve` 内新增 step 3.5；`resolve` 签名因此增加 `client_api_key` 参数，所有调用点同步更新。

**Impact / Compatibility**：`resolve` 第 5 参为 `Option`，`None` 时行为与现状完全一致；既有测试调用点机械补 `, None`。

**Verification**：`SQLX_OFFLINE=true cargo test -p hydra-core`；`SQLX_OFFLINE=true cargo test -p hydra-server --features server --test load_breaker_swrr`。

## 步骤 2.1 — router.rs：`match_key_binding` + `resolve` 闸门

(a) 导入改为：

```rust
use crate::model::{ProviderKeyBinding, Tenant};
```

(b) 在 `resolve` 函数前新增纯匹配函数：

```rust
/// Longest-prefix match of a client api-key against the enabled prefix
/// bindings (design §7.1b). Returns the binding with the longest `key_prefix`
/// that `api_key` starts with; `None` when no enabled binding matches
/// (⇒ no routing restriction).
pub fn match_key_binding<'a>(
    bindings: &'a [ProviderKeyBinding],
    api_key: &str,
) -> Option<&'a ProviderKeyBinding> {
    bindings
        .iter()
        .filter(|b| b.enabled && api_key.starts_with(&b.key_prefix))
        .max_by_key(|b| b.key_prefix.len())
}
```

(c) `resolve` 签名与 step 3 改造：

```rust
pub fn resolve(
    cfg: &ConfigData,
    breaker: &dyn BreakerView,
    tenant: &Tenant,
    model_key: &str,
    client_api_key: Option<&str>,
) -> Result<Vec<Candidate>, RouteError> {
```

将

```rust
    let intersection: Vec<String> = by_model.intersection(tenant_ok).cloned().collect();
    if intersection.is_empty() {
        return Err(RouteError::NoAvailableProvider);
    }
```

替换为

```rust
    let mut intersection: Vec<String> = by_model.intersection(tenant_ok).cloned().collect();
    if intersection.is_empty() {
        return Err(RouteError::NoAvailableProvider);
    }

    // (3.5) Key-prefix binding gate (design §7.1b): a client api-key whose raw
    // value matches an enabled prefix binding restricts the candidate set to
    // the bound provider — fail-closed (never falls back to unbound
    // providers). Longest prefix wins; no match ⇒ no restriction.
    if let Some(api_key) = client_api_key {
        if let Some(binding) = match_key_binding(&cfg.key_prefix_bindings, api_key) {
            intersection.retain(|pid| pid == &binding.provider_id);
            if intersection.is_empty() {
                return Err(RouteError::NoAvailableProvider);
            }
        }
    }
```

（step 4/5 的 `.filter(...)` 链不变，`intersection.into_iter()` 照旧。）

同时更新模块文档头部的 pipeline 描述（步骤 3.5）：

```rust
//! 3. **Intersection** of (1) and (2); empty ⇒ [`RouteError::NoAvailableProvider`].
//! 3.5 **Key-prefix binding gate** — an api-key matching an enabled prefix
//!     binding restricts the set to the bound provider (fail-closed; longest
//!     prefix wins; no match ⇒ no restriction).
```

## 步骤 2.2 — proxy.rs：resolve 调用 + passthrough 闸门

(a) `request_filter` 中路由调用（约 374 行）改为：

```rust
                    match router::resolve(
                        cfg,
                        self.state.breaker.as_ref(),
                        &tenant,
                        &model_key,
                        Some(api_key.as_str()),
                    ) {
```

(b) passthrough 调用（约 396 行）改为：

```rust
                        match passthrough_candidates(cfg, &tenant_id, Some(api_key.as_str())) {
```

(c) `passthrough_candidates`（约 957 行）改为：

```rust
/// Build a degenerate single-candidate list for **passthrough** requests (no
/// `model` field, `NonRouteStrategy::Passthrough`): the tenant's first live,
/// non-dead provider with weight > 0 and at least one api-key. In terminate
/// mode passthrough is just a one-element failover loop — no upstream_peer /
/// retry-buffer machinery.
///
/// The §7.1b binding gate applies: an api-key matching an enabled prefix
/// binding may only pass through to the bound provider (fail-closed; no match
/// ⇒ unrestricted).
///
/// Returns `None` when no live provider exists (caller maps to 503).
fn passthrough_candidates(
    cfg: &ConfigData,
    tenant_id: &str,
    client_api_key: Option<&str>,
) -> Option<Vec<Candidate>> {
    let providers = cfg.tenant_providers.get(tenant_id)?;
    let bound = client_api_key.and_then(|k| router::match_key_binding(&cfg.key_prefix_bindings, k));
    let mut pids: Vec<&String> = providers.iter().collect();
    pids.sort(); // deterministic ordering
    for pid in pids {
        if let Some(b) = bound {
            if pid != &b.provider_id {
                continue;
            }
        }
        let Some(provider) = cfg.providers.get(pid) else {
            continue;
        };
        if provider.weight <= 0 {
            continue;
        }
        let Some(keys) = cfg.provider_keys.get(pid) else {
            continue;
        };
        if keys.is_empty() {
            continue;
        }
        return Some(vec![Candidate {
            provider_id: pid.clone(),
            endpoint: provider.endpoint.clone(),
            weight: provider.weight,
        }]);
    }
    None
}
```

（`use hydra_core::router;` 已存在，无需新导入。）

## 步骤 2.3 — 既有测试调用点补参（机械替换）

- `crates/hydra-core/tests/router.rs`：全部 `resolve(&cfg, &b, &tenant, "...")` → 末尾加 `, None`（约 15 处：106/128/133/136/146/162/173/187/202/219/229/240/254/273/306 行）
- `crates/hydra-core/tests/breaker.rs`：143 行 `resolve(&cfg, view, &tenant, "gpt-4o")` → `resolve(&cfg, view, &tenant, "gpt-4o", None)`
- `crates/hydra-server/tests/load_breaker_swrr.rs`：全部 `resolve(&cfg, ..., "m")` → 末尾加 `, None`（约 8 处：118/154/164/173/195/220/242/269 行）

## 步骤 2.4 — router.rs 新增 T2.12–T2.16 测试

文件末尾追加（复用 `base_cfg()` / `tenant()` / `alive_breaker()` / `resolve_set()`）：

```rust
fn binding(id: &str, prefix: &str, provider_id: &str, enabled: bool) -> hydra_core::model::ProviderKeyBinding {
    hydra_core::model::ProviderKeyBinding {
        id: id.into(),
        key_prefix: prefix.into(),
        provider_id: provider_id.into(),
        enabled,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    }
}

/// T2.12 — an api-key matching an enabled prefix binding restricts the
/// candidate set to the bound provider.
#[test]
fn resolve_key_binding_restricts() {
    let mut cfg = base_cfg();
    cfg.key_prefix_bindings.push(binding("b1", "sk_aaa_", "p_a", true));
    let tenant = tenant();
    let b = alive_breaker();
    let cands = resolve(&cfg, &b, &tenant, "gpt-4o", Some("sk_aaa_123")).expect("bound provider");
    assert_eq!(resolve_set(&cands), HashSet::from(["p_a".into()]));
}

/// T2.13 — longest prefix wins when several enabled bindings match.
#[test]
fn resolve_key_binding_longest_prefix_wins() {
    let mut cfg = base_cfg();
    cfg.key_prefix_bindings.push(binding("b1", "sk_", "p_a", true));
    cfg.key_prefix_bindings.push(binding("b2", "sk_aaa_", "p_b", true));
    let tenant = tenant();
    let b = alive_breaker();
    let cands = resolve(&cfg, &b, &tenant, "gpt-4o", Some("sk_aaa_123")).expect("longest prefix p_b");
    assert_eq!(resolve_set(&cands), HashSet::from(["p_b".into()]));
}

/// T2.14 — disabled bindings never match (no restriction).
#[test]
fn resolve_key_binding_disabled_ignored() {
    let mut cfg = base_cfg();
    cfg.key_prefix_bindings.push(binding("b1", "sk_aaa_", "p_a", false));
    let tenant = tenant();
    let b = alive_breaker();
    let cands = resolve(&cfg, &b, &tenant, "gpt-4o", Some("sk_aaa_123")).expect("no restriction");
    assert_eq!(
        resolve_set(&cands),
        HashSet::from(["p_a".into(), "p_b".into(), "p_c".into()])
    );
}

/// T2.15 — fail-closed: the bound provider is not in the eligible set ⇒ error.
#[test]
fn resolve_key_binding_bound_provider_ineligible() {
    let mut cfg = base_cfg();
    // p_a serves gpt-4o but is NOT in the tenant's authorised provider set.
    cfg.tenant_providers.insert(
        "t_acme".into(),
        HashSet::from(["p_b".into(), "p_c".into()]),
    );
    cfg.key_prefix_bindings.push(binding("b1", "sk_aaa_", "p_a", true));
    let tenant = tenant();
    let b = alive_breaker();
    let err = resolve(&cfg, &b, &tenant, "gpt-4o", Some("sk_aaa_123")).unwrap_err();
    assert_eq!(err, RouteError::NoAvailableProvider);
}

/// T2.16 — no matching prefix (or None api-key) ⇒ no restriction.
#[test]
fn resolve_key_binding_no_match_no_restriction() {
    let mut cfg = base_cfg();
    cfg.key_prefix_bindings.push(binding("b1", "sk_aaa_", "p_a", true));
    let tenant = tenant();
    let b = alive_breaker();
    let cands = resolve(&cfg, &b, &tenant, "gpt-4o", Some("hk_bbb_1")).expect("no match");
    assert_eq!(
        resolve_set(&cands),
        HashSet::from(["p_a".into(), "p_b".into(), "p_c".into()])
    );
    let cands2 = resolve(&cfg, &b, &tenant, "gpt-4o", None).expect("None → no restriction");
    assert_eq!(
        resolve_set(&cands2),
        HashSet::from(["p_a".into(), "p_b".into(), "p_c".into()])
    );
}
```

**验证（Task 2 完成点）**：

```bash
SQLX_OFFLINE=true cargo test -p hydra-core
SQLX_OFFLINE=true cargo test -p hydra-server --features server --test load_breaker_swrr
```

---

# Task 3 — db.rs CRUD 5 函数 + `.sqlx/` 离线缓存刷新

**Files**
- 修改 `crates/hydra-server/src/db.rs`
- 刷新 `.sqlx/`（提交新 query 元数据）

**Why**：绑定表的持久化读写层；同时解锁 store 加载与 admin CRUD。

**Change Necessity**：现有 db.rs 无绑定表函数；新增 5 个与 limit_role 完全同构的函数。

**Impact / Compatibility**：纯增量；`query!` 宏需要 `.sqlx/` 新条目（CI `SQLX_OFFLINE=true`）。

**Verification**：`cargo sqlx prepare` 成功 + `SQLX_OFFLINE=true cargo build --workspace --features hydra-server/server`。

## 步骤 3.1 — 安装 sqlx-cli（如未安装）

```bash
cargo install sqlx-cli --no-default-features --features sqlite,rustls
```

> 若无法联网安装：退路是临时用 live DB 编译（`DATABASE_URL=sqlite:///tmp/hydra-prepare.db` 且不设 SQLX_OFFLINE），但提交前必须完成 `cargo sqlx prepare` 刷新 `.sqlx/`，否则 CI 编译失败。

## 步骤 3.2 — db.rs：导入 + Row 结构 + CRUD 函数

(a) 导入改为：

```rust
use hydra_core::model::{
    LimitRole, Provider, ProviderKey, ProviderKeyBinding, ProviderModel, Tenant, TenantModel,
    TenantProvider,
};
```

(b) `LimitRoleRow` 的 `From` 实现之后追加：

```rust
#[derive(sqlx::FromRow, Debug, Clone)]
struct ProviderKeyBindingRow {
    id: String,
    key_prefix: String,
    provider_id: String,
    enabled: i64,
    created_at: String,
    updated_at: String,
}

impl From<ProviderKeyBindingRow> for ProviderKeyBinding {
    fn from(r: ProviderKeyBindingRow) -> Self {
        ProviderKeyBinding {
            id: r.id,
            key_prefix: r.key_prefix,
            provider_id: r.provider_id,
            enabled: r.enabled != 0,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}
```

(c) 文件末尾（`delete_limit_role` 之后）追加 CRUD 段：

```rust
// ---------------------------------------------------------------------------
// CRUD — provider_key_binding (design §7.1b)
// ---------------------------------------------------------------------------

/// Insert a binding. Violating the UNIQUE `key_prefix` constraint returns a
/// sqlx UNIQUE violation (→ 409 by the admin layer).
pub async fn insert_provider_key_binding(
    pool: &SqlitePool,
    b: &ProviderKeyBinding,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "INSERT INTO provider_key_binding (id, key_prefix, provider_id, enabled, created_at, \
         updated_at) VALUES (?, ?, ?, ?, ?, ?)",
        b.id,
        b.key_prefix,
        b.provider_id,
        b.enabled,
        b.created_at,
        b.updated_at
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_provider_key_binding(
    pool: &SqlitePool,
    id: &str,
) -> Result<ProviderKeyBinding, sqlx::Error> {
    let row = sqlx::query_as!(
        ProviderKeyBindingRow,
        r#"SELECT id as "id!", key_prefix, provider_id, enabled, created_at, updated_at
           FROM provider_key_binding WHERE id = ?"#,
        id
    )
    .fetch_one(pool)
    .await?;
    Ok(row.into())
}

pub async fn list_provider_key_bindings(
    pool: &SqlitePool,
) -> Result<Vec<ProviderKeyBinding>, sqlx::Error> {
    let rows = sqlx::query_as!(
        ProviderKeyBindingRow,
        r#"SELECT id as "id!", key_prefix, provider_id, enabled, created_at, updated_at
           FROM provider_key_binding ORDER BY key_prefix"#
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Update a binding's mutable fields (prefix / provider / enabled).
pub async fn update_provider_key_binding(
    pool: &SqlitePool,
    b: &ProviderKeyBinding,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE provider_key_binding SET key_prefix = ?, provider_id = ?, enabled = ?, \
         updated_at = ? WHERE id = ?",
        b.key_prefix,
        b.provider_id,
        b.enabled,
        b.updated_at,
        b.id
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_provider_key_binding(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query!("DELETE FROM provider_key_binding WHERE id = ?", id)
        .execute(pool)
        .await?;
    Ok(())
}
```

## 步骤 3.3 — 刷新 `.sqlx/` 并验证编译

```bash
# 1) 建一个已 migrate 的临时库（迁移目录含新的 0006）
DATABASE_URL=sqlite:///tmp/hydra-prepare.db cargo sqlx db create
DATABASE_URL=sqlite:///tmp/hydra-prepare.db cargo sqlx migrate run --source crates/hydra-server/migrations
# 2) 重新生成离线缓存（写入仓库根 .sqlx/）
DATABASE_URL=sqlite:///tmp/hydra-prepare.db cargo sqlx prepare --workspace --features db
# 3) 离线编译验证（CI 同款）
SQLX_OFFLINE=true cargo build --workspace --features hydra-server/server
```

确认 `git status` 出现新增的 `query-*.json` 文件（本次应新增 5 条：insert/get/list/update/delete）。

**验证（Task 3 完成点）**：步骤 3.3 的 build 成功；`git status` 可见 `.sqlx/` 新文件。

---

# Task 4 — store.rs 加载绑定

**Files**
- 修改 `crates/hydra-server/src/store.rs`

**Why**：把绑定表接入热加载快照，路由热路径才能读到。

**Change Necessity**：`build_config` 必须把 enabled 绑定装入 `ConfigData`。

**Impact / Compatibility**：镜像 limit_roles 的加载约定（只取 enabled）。

**Verification**：`SQLX_OFFLINE=true cargo build --workspace --features hydra-server/server`（loader 测试在 Task 7 补齐）。

## 步骤 4.1 — store.rs

(a) 导入改为：

```rust
use hydra_core::model::{LimitRole, ProviderKeyBinding};
```

(b) `build_config` 在 limit_roles 块之后追加：

```rust
    // provider_key_bindings: only enabled bindings participate (design §7.1b).
    let key_prefix_bindings: Vec<ProviderKeyBinding> = db::list_provider_key_bindings(pool)
        .await?
        .into_iter()
        .filter(|b| b.enabled)
        .collect();
```

(c) `ConfigData` 字面量（约 137 行）补字段：

```rust
    let cfg = ConfigData {
        tenants_by_domain,
        models_by_key,
        tenant_providers,
        tenant_models,
        providers,
        provider_keys,
        limit_roles,
        key_prefix_bindings,
        certs,
    };
```

**验证（Task 4 完成点）**：build 成功。

---

# Task 5 — admin REST CRUD（handlers + 路由）

**Files**
- 修改 `crates/hydra-server/src/admin/handlers.rs`、`crates/hydra-server/src/admin/mod.rs`

**Why**：管理面 API——配置表的增删改查。

**Change Necessity**：新资源段 `/api/v1/provider-key-bindings`，镜像 limit_role 的 collection/item 模式。

**Impact / Compatibility**：新增资源段，路由为增量；写后 `reload_best_effort` 热生效。

**Verification**：`SQLX_OFFLINE=true cargo test -p hydra-server --features server --test admin_api`（含既有套件全绿）。

## 步骤 5.1 — handlers.rs

(a) 导入改为：

```rust
use hydra_core::model::{
    LimitRole, Provider, ProviderKey, ProviderKeyBinding, ProviderModel, Tenant, TenantModel,
    TenantProvider,
};
```

(b) Limit roles 段之后（`limit_role_item` 之后）追加：

```rust
// ===========================================================================
// Provider key bindings (design §7.1b)
// ===========================================================================

pub(super) async fn provider_key_binding_collection(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    trace_id: &str,
) -> Resp {
    if method == "GET" {
        match crate::db::list_provider_key_bindings(&state.pool).await {
            Ok(rows) => ok_json(200, &rows),
            Err(e) => db_err_resp(e, trace_id),
        }
    } else if method == "POST" {
        let body = read_body(session).await;
        let mut b: ProviderKeyBinding = match parse_body(&body, trace_id) {
            Ok(b) => b,
            Err(r) => return r,
        };
        if b.key_prefix.trim().is_empty() {
            return err_json(
                400,
                "empty_key_prefix",
                "key_prefix must be a non-empty string",
                trace_id,
            );
        }
        if b.id.is_empty() {
            b.id = gen_id();
        }
        let ts = now_ts();
        if b.created_at.is_empty() {
            b.created_at = ts.clone();
        }
        if b.updated_at.is_empty() {
            b.updated_at = ts;
        }
        match crate::db::insert_provider_key_binding(&state.pool, &b).await {
            Ok(()) => {}
            Err(e) => return db_err_resp(e, trace_id),
        }
        reload_best_effort(state, trace_id).await;
        ok_json(201, &b)
    } else {
        method_not_allowed(trace_id)
    }
}

pub(super) async fn provider_key_binding_item(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    id: &str,
    trace_id: &str,
) -> Resp {
    match method {
        "GET" => match crate::db::get_provider_key_binding(&state.pool, id).await {
            Ok(b) => ok_json(200, &b),
            Err(e) if is_not_found(&e) => {
                err_json(404, "not_found", "provider_key_binding not found", trace_id)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        "PUT" => {
            let body = read_body(session).await;
            let mut b: ProviderKeyBinding = match parse_body(&body, trace_id) {
                Ok(b) => b,
                Err(resp) => return resp,
            };
            if b.key_prefix.trim().is_empty() {
                return err_json(
                    400,
                    "empty_key_prefix",
                    "key_prefix must be a non-empty string",
                    trace_id,
                );
            }
            b.id = id.to_string();
            b.updated_at = now_ts();
            match crate::db::update_provider_key_binding(&state.pool, &b).await {
                Ok(()) => {}
                Err(e) => return db_err_resp(e, trace_id),
            }
            match crate::db::get_provider_key_binding(&state.pool, id).await {
                Ok(b) => {
                    reload_best_effort(state, trace_id).await;
                    ok_json(200, &b)
                }
                Err(_) => err_json(404, "not_found", "provider_key_binding not found", trace_id),
            }
        }
        "DELETE" => match crate::db::delete_provider_key_binding(&state.pool, id).await {
            Ok(()) => {
                reload_best_effort(state, trace_id).await;
                empty(204)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        _ => method_not_allowed(trace_id),
    }
}
```

## 步骤 5.2 — admin/mod.rs 路由段

在 `("limit-roles", Some(id))` 分支之后追加：

```rust
            ("provider-key-bindings", None) => {
                handlers::provider_key_binding_collection(state, session, method, trace_id).await
            }
            ("provider-key-bindings", Some(id)) => {
                handlers::provider_key_binding_item(state, session, method, id, trace_id).await
            }
```

**验证（Task 5 完成点）**：admin_api 既有套件全绿 + build 成功。

---

# Task 6 — admin-UI 编辑界面

**Files**
- 修改 `admin-ui/app.js`（编译期嵌入，改完重新 build hydra-server 即生效）

**Why**：管理界面可视化编辑绑定配置。

**Change Necessity**：UI 的 CRUD 是声明式配置，新增一个 section 即可。

**Impact / Compatibility**：新增导航项 + 资源页；无既有 UI 行为变更。

**Verification**：`SQLX_OFFLINE=true cargo test -p hydra-server --features server --test admin_ui` + 手工冒烟（见步骤 6.2）。

## 步骤 6.1 — app.js 修改

(a) 头部注释（约 231 行）`the 7 CRUD entities` → `the 8 CRUD entities`。

(b) `CRUD` 对象末尾（`"limit-roles"` 之后）追加：

```js
  "provider-key-bindings": {
    title: "Key Prefix Bindings", nav: "Key Bindings", icon: "key2", path: "/provider-key-bindings", singular: "binding",
    desc: "Route gate — client api-keys whose raw value starts with a prefix are pinned to one provider (longest prefix wins, fail-closed).",
    columns: [
      { key: "id", label: "ID", mono: true },
      { key: "key_prefix", label: "Prefix", mono: true },
      { key: "provider_id", label: "Provider", fk: "providers" },
      { key: "enabled", label: "Enabled", render: (v) => boolPill(v) },
    ],
    fields: [
      { name: "id", label: "ID", placeholder: "auto if blank" },
      { name: "key_prefix", label: "Key prefix", required: true, placeholder: "sk_aaa_",
        tip: "client api-key prefix; e.g. sk_aaa_ → keys starting with sk_aaa_ use this provider" },
      { name: "provider_id", label: "Provider", type: "select", fk: "providers", required: true },
      { name: "enabled", label: "Enabled", type: "checkbox", map: "bool", value: true },
    ],
  },
```

(c) `NAV` 的 Configuration 组末尾追加 `"provider-key-bindings"`：

```js
  { label: "Configuration", items: ["providers", "provider-models", "provider-keys", "tenants", "tenant-providers", "tenant-models", "limit-roles", "provider-key-bindings"] },
```

## 步骤 6.2 — 手工冒烟（可选，需运行环境）

启动 `environment/docker-compose.yml` 或本地 `cargo run --features server` 后，登录 `/admin`：
1. 侧栏出现 "Key Bindings"；
2. 新建绑定 `sk_aaa_` → 某 provider，保存后列表出现且 reload 成功；
3. 编辑改 prefix，删除绑定，均正常。

**验证（Task 6 完成点）**：`admin_ui` 测试绿（嵌入式资源自检）+ build 成功。

---

# Task 7 — 集成测试（repo / loader / admin_api）

**Files**
- 修改 `crates/hydra-server/tests/repo.rs`、`crates/hydra-server/tests/loader.rs`、`crates/hydra-server/tests/admin_api.rs`

**Why**：覆盖 db CRUD、loader 加载、HTTP CRUD + 热加载生效三条链路。

**Change Necessity**：新功能需回归证据（仓库铁律：无内部逻辑 mock，真实 `:memory:` SQLite + 真实 HTTP）。

**Impact / Compatibility**：纯测试增量。

**Verification**：`SQLX_OFFLINE=true cargo test -p hydra-server --features server`。

## 步骤 7.1 — repo.rs：CRUD round-trip + CASCADE

文件末尾追加（复用既有 `provider()` / `now()` helper）：

```rust
/// T4.10 — provider_key_binding CRUD round-trip + CASCADE with its provider.
#[tokio::test]
async fn provider_key_binding_crud() {
    let pool = common::setup_pool().await;
    repo::insert_provider(&pool, &provider("p1", "openai", 1))
        .await
        .expect("seed provider");

    let b = hydra_core::model::ProviderKeyBinding {
        id: "b1".into(),
        key_prefix: "sk_aaa_".into(),
        provider_id: "p1".into(),
        enabled: true,
        created_at: now().into(),
        updated_at: now().into(),
    };
    repo::insert_provider_key_binding(&pool, &b).await.expect("insert");
    assert_eq!(repo::get_provider_key_binding(&pool, "b1").await.unwrap(), b);

    let mut upd = b.clone();
    upd.key_prefix = "hk_bbb_".into();
    upd.enabled = false;
    repo::update_provider_key_binding(&pool, &upd)
        .await
        .expect("update");
    assert_eq!(repo::get_provider_key_binding(&pool, "b1").await.unwrap(), upd);

    let all = repo::list_provider_key_bindings(&pool).await.expect("list");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].key_prefix, "hk_bbb_");

    // CASCADE: deleting the provider removes its binding.
    repo::delete_provider(&pool, "p1").await.expect("delete p1");
    assert!(
        repo::get_provider_key_binding(&pool, "b1").await.is_err(),
        "binding must be CASCADE-deleted with its provider"
    );
}
```

## 步骤 7.2 — loader.rs：只加载 enabled 绑定

文件末尾追加（复用 `seed()` / `kp()`）：

```rust
#[tokio::test]
async fn load_key_prefix_bindings_enabled_only() {
    let pool = common::setup_pool().await;
    seed(&pool).await;
    let mk = |id: &str, prefix: &str, provider_id: &str, enabled: bool| {
        hydra_core::model::ProviderKeyBinding {
            id: id.into(),
            key_prefix: prefix.into(),
            provider_id: provider_id.into(),
            enabled,
            created_at: now().into(),
            updated_at: now().into(),
        }
    };
    repo::insert_provider_key_binding(&pool, &mk("b1", "sk_aaa_", "p1", true))
        .await
        .expect("b1");
    repo::insert_provider_key_binding(&pool, &mk("b2", "hk_", "p2", false))
        .await
        .expect("b2");

    let cfg = build_config(&pool, &kp()).await.expect("build_config");
    assert_eq!(cfg.key_prefix_bindings.len(), 1, "only enabled rows load");
    assert_eq!(cfg.key_prefix_bindings[0].id, "b1");
    assert_eq!(cfg.key_prefix_bindings[0].provider_id, "p1");
}
```

> loader.rs 现有测试文件名与 `build_config` 导入（`use hydra_server::store::build_config`）已具备；确认 `now()` helper 已存在（是）。

## 步骤 7.3 — admin_api.rs：HTTP CRUD + 热加载生效

在 provider-keys CRUD 测试之后追加：

```rust
#[tokio::test]
async fn provider_key_bindings_crud_http() {
    let state = admin_state().await;
    let port = start_admin(state.clone());

    // Seed a provider so the FK holds.
    let p = r#"{"id":"p1","key":"openai","name":"O","endpoint":"https://api.openai.com","weight":1,"created_at":"","updated_at":""}"#;
    let _ = req(port, reqwest::Method::POST, "/api/v1/providers", Some(TOKEN), Some(p)).await;

    // Create → 201.
    let b = r#"{"id":"b1","key_prefix":"sk_aaa_","provider_id":"p1","enabled":true,"created_at":"","updated_at":""}"#;
    let r = req(port, reqwest::Method::POST, "/api/v1/provider-key-bindings", Some(TOKEN), Some(b)).await;
    assert_eq!(r.status(), 201);
    let created: serde_json::Value = r.json().await.expect("json");
    assert_eq!(created["key_prefix"], "sk_aaa_");

    // Hot reload: the in-memory snapshot now carries the enabled binding.
    let snap = state.store.snapshot();
    assert_eq!(snap.key_prefix_bindings.len(), 1);
    assert_eq!(snap.key_prefix_bindings[0].provider_id, "p1");
    drop(snap);

    // Duplicate prefix → 409 (UNIQUE).
    let dup = r#"{"id":"b2","key_prefix":"sk_aaa_","provider_id":"p1","enabled":true,"created_at":"","updated_at":""}"#;
    let r = req(port, reqwest::Method::POST, "/api/v1/provider-key-bindings", Some(TOKEN), Some(dup)).await;
    assert_eq!(r.status(), 409);

    // Empty prefix → 400 (handler guard).
    let empty = r#"{"id":"b3","key_prefix":"","provider_id":"p1","enabled":true,"created_at":"","updated_at":""}"#;
    let r = req(port, reqwest::Method::POST, "/api/v1/provider-key-bindings", Some(TOKEN), Some(empty)).await;
    assert_eq!(r.status(), 400);

    // Unknown provider → 400 (FK violation).
    let ghost = r#"{"id":"b4","key_prefix":"hk_","provider_id":"ghost","enabled":true,"created_at":"","updated_at":""}"#;
    let r = req(port, reqwest::Method::POST, "/api/v1/provider-key-bindings", Some(TOKEN), Some(ghost)).await;
    assert_eq!(r.status(), 400);

    // List → 1 row.
    let r = req(port, reqwest::Method::GET, "/api/v1/provider-key-bindings", Some(TOKEN), None).await;
    assert_eq!(r.status(), 200);
    let list: serde_json::Value = r.json().await.expect("json");
    assert_eq!(list.as_array().unwrap().len(), 1);

    // Update (PUT) → 200, disabled reflected.
    let upd = r#"{"id":"b1","key_prefix":"sk_aaa_v2","provider_id":"p1","enabled":false,"created_at":"","updated_at":""}"#;
    let r = req(port, reqwest::Method::PUT, "/api/v1/provider-key-bindings/b1", Some(TOKEN), Some(upd)).await;
    assert_eq!(r.status(), 200);
    let item: serde_json::Value = r.json().await.expect("json");
    assert_eq!(item["enabled"], serde_json::Value::Bool(false));

    // Disabled binding leaves the hot snapshot.
    let snap2 = state.store.snapshot();
    assert_eq!(snap2.key_prefix_bindings.len(), 0, "disabled binding not loaded");
    drop(snap2);

    // Single GET → 200; unknown id → 404.
    let r = req(port, reqwest::Method::GET, "/api/v1/provider-key-bindings/b1", Some(TOKEN), None).await;
    assert_eq!(r.status(), 200);
    let r = req(port, reqwest::Method::GET, "/api/v1/provider-key-bindings/nope", Some(TOKEN), None).await;
    assert_eq!(r.status(), 404);

    // DELETE → 204.
    let r = req(port, reqwest::Method::DELETE, "/api/v1/provider-key-bindings/b1", Some(TOKEN), None).await;
    assert_eq!(r.status(), 204);
}
```

> `start_admin(state)` 在原测试中直接 move state；本测试用 `start_admin(state.clone())` 以便之后读 `state.store.snapshot()`（`AdminState` 可 Clone，见 admin/mod.rs 用法）。

**验证（Task 7 完成点）**：`SQLX_OFFLINE=true cargo test -p hydra-server --features server` 全绿。

---

# Task 8 — 文档 +（可选）hydra-cli

**Files**
- 修改 `docs/design.md`、`README.md`、`README.zh-CN.md`、`docs/HANDOFF.md`
- 可选修改 `tools/hydra-cli/src/types.ts`

**Why**：仓库文档即权威（design.md 是 schema/语义来源）；README 与 HANDOFF 保持同步。

**Change Necessity**：新增 schema + 路由闸门语义必须入 design.md，否则后续 wave 会失同步。

**Impact / Compatibility**：纯文档；hydra-cli 为可选增量（现有 CLI 已缺 limit-roles，非必须对齐）。

**Verification**：无编译门禁；文档 diff 自查。

## 步骤 8.1 — docs/design.md

(a) §4.1 schema 列表追加：

```markdown
-- api-key 前缀 → provider 绑定（§7.1b 路由闸门）
CREATE TABLE provider_key_binding (
    id          TEXT PRIMARY KEY,
    key_prefix  TEXT NOT NULL UNIQUE,          -- 客户端 api-key 前缀，如 'sk_aaa_'
    provider_id TEXT NOT NULL REFERENCES provider(id) ON DELETE CASCADE,
    enabled     INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
```

(b) §7.1 之后新增 §7.1b：

```markdown
### 7.1b api-key 前缀绑定闸门（provider_key_binding）

新增 `provider_key_binding` 表（§4.1）：`key_prefix`（UNIQUE）→ `provider_id`。

- **匹配**：客户端 api-key（Authorization Bearer / x-api-key 的**原始值**）以某条
  `enabled=1` 的 `key_prefix` 开头 ⇒ 候选集被限制为该 provider；
- **最长前缀优先**：多条前缀同时命中时取 `key_prefix` 最长者（最具体）；
- **fail-closed**：绑定的 provider 不在候选集（不提供该模型 / 未被租户授权 /
  熔断 / 软禁用）⇒ `503 NoAvailableProvider`，绝不回落其他后端；
- **无命中** ⇒ 不限制（保持 §7.1 现有语义）；
- **passthrough**（无 model 字段）同样受闸门约束，只允许命中绑定的 provider；
- **管理面**：`/api/v1/provider-key-bindings` CRUD（§13.2 模式），写后热加载；
- **隐私**：仅对原始 key 做前缀比较，key 明文不落库、不进日志（与 §16.4 一致）；
- loader 只加载 `enabled=1` 的行（与 limit_role 同约定）；`config::validate`
  对空前缀 / 未知 provider 告警（Warn）。
```

## 步骤 8.2 — README.md / README.zh-CN.md

功能特性列表各追加一条：

```markdown
- api-key 前缀绑定路由闸门（key_prefix → provider，最长前缀优先，fail-closed）
```

## 步骤 8.3 — docs/HANDOFF.md

按最终 `cargo test` 实际输出更新两处测试基数（原 101 / 159）。

## 步骤 8.4 —（可选）hydra-cli：`tools/hydra-cli/src/types.ts`

`ENTITY_DEFS` 末尾追加：

```ts
  {
    route: 'provider-key-bindings',
    command: 'provider-key-bindings',
    label: 'key-prefix binding',
    labelPlural: 'key-prefix bindings',
    columns: [
      { field: 'id', header: 'ID', width: 18 },
      { field: 'key_prefix', header: 'PREFIX', width: 24 },
      { field: 'provider_id', header: 'PROVIDER', width: 18 },
      { field: 'enabled', header: 'ENABLED', width: 8 },
    ],
    fields: [
      { field: 'id', kind: 'string', flag: '--id <id>', help: 'Binding id', required: true },
      { field: 'key_prefix', kind: 'string', flag: '--key-prefix <prefix>', help: 'Client api-key prefix', required: true },
      { field: 'provider_id', kind: 'string', flag: '--provider-id <id>', help: 'Provider id', required: true },
      { field: 'enabled', kind: 'boolean', flag: '--enabled', falseFlag: '--disabled', help: 'Enable (--enabled) or disable (--disabled)', default: true },
    ],
  },
```

验证：`cd tools/hydra-cli && npm test`（如选择执行该可选任务）。

---

# 最终验证（全部 Task 完成后）

```bash
# CI 同款门禁
cargo fmt --check
cargo clippy --workspace --all-targets --features hydra-server/server -- -D warnings
cargo build --workspace --features hydra-server/server
SQLX_OFFLINE=true cargo test -p hydra-core
SQLX_OFFLINE=true cargo test -p hydra-server --features server
# 依赖防火墙（hydra-core 零 I/O 依赖，新增字段/函数不得引入依赖）
cargo tree -p hydra-core --no-default-features | grep -E ' (tokio|pingora|sqlx|reqwest|hyper)(-[^ ]+)? v' || echo OK
```

预期测试数变化：hydra-core 101 → ~108（router +5、validate +2）；hydra-server 159 → ~162（repo +1、loader +1、admin_api +1）。

# Risks

| 风险 | 影响 | 缓解 |
|---|---|---|
| sqlx-cli 未安装 / 无法联网安装 | `.sqlx/` 无法刷新，CI 编译失败 | Task 3 前置 `cargo install sqlx-cli`；退路 live-DB 编译 + 提交前必须 prepare |
| CI `-D warnings` | 任何 warning 直接失败 | 每 Task 验证命令含 build/clippy；注意未用导入、`unused_mut` 等 |
| `resolve` 签名变更波及 ~24 个调用点 | 编译错误 | 步骤 2.3 给出完整行号清单，机械替换 |
| 空前缀会匹配一切 key（`starts_with("")` 恒真） | 误绑定全部流量 | handler 400 拒绝 + validate Warn + 步骤 2.1 匹配仅对 enabled 绑定 |
| 删 provider 级联删绑定 | 配置静默消失 | ON DELETE CASCADE 为既有约定（provider_model 同款）；UI 有确认弹窗 |
| 隐私：绑定匹配用原始 key | 前缀即信息暴露 | 仅内存比较，不落库/不进日志；日志仍走 `mask_key`（§16.4） |

# Retirement

- 本特性为**纯增量**，无旧逻辑退役；`resolve` 签名变化是一次性内部 API 演进（无外部消费者）。
- 若未来升级为按租户绑定（表加 `tenant_id`），本全局表保留为兼容层，按 anti-entropy 规则迁移；届时 `match_key_binding` 增加租户维度参数。
- `RouteError::NoAvailableKey` 仍保持现状（未被 resolve 使用），不因本特性改动。

---

# Execution Readiness View

```text
Execution Readiness View:
- Intent Lock: api-key 前缀 → provider 绑定闸门（全局 / 最长前缀优先 / fail-closed / 1:1），四项语义已由用户确认
- Scope Fence: 不含 per-tenant 绑定、前缀→多 provider、按前缀加权；不含客户端协议变更
- Baseline Lock: design.md §4.1/§5.2/§5.3/§7.1/§10.1/§13/§14；wave-1/4 纯核心与 request_filter 约定；.sqlx 离线缓存必须随 SQL 变更刷新
- Approved Behavior: 命中绑定→候选集=绑定 provider（不可用→503）；未命中→原语义；passthrough 同闸门
- Owner / Contract Constraints: 候选集唯一入口 router::resolve；匹配唯一入口 router::match_key_binding；db Row→模型转换复用既有模式
- Compatibility Boundary: resolve 第 5 参 Option（None=现状）；ConfigData 新字段 Default 覆盖；迁移 0006 增量；admin REST/UI 增量
- Retirement Boundary: 纯增量；未来按租户升级时全局表留作兼容层
- Task Batches: T1(模型/迁移/校验) → T2(核心闸门+调用点) → T3(db+sqlx) → T4(store) → T5(admin API) → T6(UI) → T7(集成测试) → T8(文档/可选 cli)
- Test Obligations: 每 Task 精确命令；最终 CI 同款 5 命令全绿；测试数 101→~108 / 159→~162
- Review Gates: 本计划文档审核通过后启动（用户要求）；每 Task 完成后自验证再进入下一 Task
- Drift / Rewind Rules: 任一 Task 验证失败即停，修复后再继续；.sqlx 未刷新不提交
- Evidence Required Before Completion: fmt/clippy/build/两 crate 全量测试绿 + .sqlx 新文件入 git + design.md 文档同步
- Advisory Boundary: 方法包执行指引；非 GateDecision/PolicySnapshot/完成授权
```

# Execution Route

```text
Execution Route:
- Decision: inline
- Evidence: 8 个 Task 强顺序耦合（T2 签名变更波及 T7 测试文件、T3 解锁 T4/T5），子代理并行收益低且易冲突；调研上下文（调用点清单、模式细节）已在本人会话内
- Fallback: 若单 Task 卡住，可派子代理执行隔离切片（如 T6 UI 或 T8 文档）
- User confirmation required: no（按本计划文档审核通过即启动）
```
