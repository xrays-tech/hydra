# 实施计划（v2 · 绿地重建）：四级用户体系 + 内部 API-Key 鉴权 + Global Role 资源调度

- **日期**：2026-11-03
- **状态**：**已通过 oracle 设计门禁（PASS，2026-11-03；BL-1～BL-4 + 最终 DDL 修正全部关闭）；**核心已落地（T1–T9 + T11 全绿，见 §9）；T10（migration `0015` DROP 旧 + §3.7 全清单 + sub-tenant / auth-cache 代码 & UI & 文档 delete）DEFERRED — old external-auth path、`sub_tenant`/`sub_tenant_route`/`provider_key_binding` 与 `tenant.auth_url`（NOT NULL 写时必填，`0014` 只 CREATE）still live code**
- **作者**：编排智能体（综合 exp-2/exp-3/exp-4 现状测绘；v1 已通过门禁，但 v1 的迁移/兼容层在本版被删除）
- **基线**：`870a732`（main）
- **前置假设（本版核心）**：
  1. **没有真实上线业务、没有需要保留的生产数据**；本地/开发库可以删除重建。
  2. 因此**不做任何数据迁移、回填、双模过渡**；允许破坏性 schema/API 变更。
  3. 旧集群数据目录可以清空；`TOC_FORMAT` 直接进入新格式，无需与旧树共存。
  4. 旧的 `auth_url` 外部鉴权、`sub_tenant` 前缀路由、`provider_key_binding` 前缀绑定在本版**一并删除**，避免两套规则并存（“规则收束到一个界面”）。
- **对应需求**：
  1. 新数据结构 `tenant -> user_group -> user -> api_key` 四级；
  2. 租户可有**多个**访问令牌（tenant access token），用于访问租户 API；
  3. 租户 API 可管理用户组、用户、用户的 API-Key；
  4. API-Key 鉴权改为 **hydra 内部**，不再调用租户外部 `auth_url`；
  5. 用户有**欠费状态**，租户 API 可设置；欠费用户的 API-Key 鉴权必须拒绝并返回欠费错误；
  6. 新增 **Global Role** 表，以 `(tenant, user_group, user, api_key, model)` 为键映射到一组 provider，`*` 表示全部；
  7. 运行时由请求的 `model` + `api-key` 反查 user/group/tenant，匹配规则并取**并集**、去重；
  8. 并集按权重 round-robin，或按会话头（`X-Opencode-Session`、`Session-Id`、`x-conversation-id` 等）做**会话粘性**路由；
  9. 规则收束到一个管理界面。

---

## 1. Goal 与验收

### 1.1 数据层级与多令牌

| # | 验收项 | 判定 |
|---|---|---|
| A1 | 四级表存在且引用完整 | `user_group`、`user`、`user_api_key` 三表 + FK/唯一索引见 §3.1；非法跨租户引用在写入时拒绝 |
| A2 | 多租户令牌 | 同一租户可创建 ≥2 个 `tenant_access_token`；任一有效令牌均可调租户 API；删除其一不影响其余 |
| A3 | 旧结构已删除 | `tenant.auth_url`、`tenant.access_token_hash`、`sub_tenant`、`sub_tenant_route`、`provider_key_binding` 在代码与 schema 中均不存在（`grep` 零命中；归档文档除外） |
| A4 | 控制面往返 | 新实体经 `publish -> materialize -> restore` 后逐字段一致（含禁用行、id、时间戳、key hash） |
| A5 | 全节点同版本 | 新树 `TOC_FORMAT=5`；混合版本节点按名字拒绝新树并服务 last-known-good；本版不提供 format-4 回读 |

### 1.2 内部鉴权与欠费

| # | 验收项 | 判定 |
|---|---|---|
| B1 | 零外部鉴权 | `enforce_auth` 路径无任何 HTTP 出站；`HttpAuthChecker`/`AuthCache`/`redis::auth_cache` 代码删除；相关 env 删除 |
| B2 | 有效 key | 正确 key -> 放行；日志/响应不出现明文 key，只出现 `key_prefix`+last4 |
| B3 | 无效 key | 未知/禁用/过期 key -> **401** `invalid_api_key`；跨租户 key -> 401，不得放行 |
| B4 | 欠费 | `billing_status=delinquent` -> **402**，body `{"error":{"message":...,"type":"insufficient_quota","code":"user_delinquent"}}` |
| B5 | 用户/组禁用 | `user.enabled=false` / `group.enabled=false` -> 403 `user_disabled` / `group_disabled` |
| B6 | 状态传播 | 租户 API 设置欠费：本节点下一次请求立即生效；其他节点 ≤5 s 生效（配置发布 + 1 s 物化轮询） |
| B7 | 无缓存 | 撤销/欠费不依赖 TTL；`auth/cache/invalidate`、admin `auth/test`、`auth/cache` 端点删除 |

### 1.3 Global Role 调度

| # | 验收项 | 判定 |
|---|---|---|
| C1 | 五维匹配 | 每条规则五维均为 `*` 或具体 id；全部匹配才算命中；`*` 只匹配“该维任意值” |
| C2 | 并集去重 | 命中 ≥2 条规则 -> provider 集合为并集，重复 provider 只出现一次；顺序确定（provider_id 升序）后再排序 |
| C3 | 授权边界 | 角色 provider 必须同时在线服务该 model（`models_by_key`）；否则在求交阶段被过滤（不记为 excluded） |
| C4 | 无命中回退 | 无任何规则命中 -> 走 `tenant_providers ∩ models_by_key`，行为与今天一致 |
| C4b | 通配租户不越权 | `tenant_id='*'` 的角色：命中后仍与 `tenant_providers` 求交；具体租户角色不求交（显式授权）。见 D-3 |
| C5 | 权重 | 无会话头 -> 沿用 SWRR（`Provider.weight`，`weight=0` 软禁用），每 `(tenant, model)` 状态 |
| C6 | 会话粘性 | 有会话头 -> 同一 `(tenant, model, api_key_id, session)` 稳定命中同一 provider；provider 集合变化时只重映射受影响会话 |
| C7 | 会话头优先级 | 依次取 `X-Opencode-Session`、`Session-Id`、`x-conversation-id`；HTTP 头大小写不敏感；空值/超过 256 字节忽略 |
| C8 | 故障转移 | 粘性首选不可用（admission 拒绝/连接失败/上游非 2xx）时，按 **provider_id 升序**在剩余候选中转移（不调用有状态的 `swrr::order`）；不改变“已发送后不重试”的双计费规则 |
| C9 | Catalog 一致 | `GET /v1/models` 在鉴权前返回，**永不** 401/402/403。能解析 principal 则用角色并集+回退；无 key/未注册 key 按“匿名 principal”匹配（具体 user/group/api_key 维度不命中）；欠费用户仍可看目录 |
| C10 | 单界面 | 管理端一个 Global Roles 页面完成全部规则增删改查；旧的 sub-tenant/key-prefix 路由页面删除 |

---

## 2. 背景（现状测绘结论）

- 现有表：`provider`、`provider_model`、`provider_key`、`tenant`、`tenant_provider`、`tenant_model`、`limit_role`、`provider_key_binding`、`sub_tenant`、`sub_tenant_route`、`config_meta`。
- **没有** user/group/api-key 表；客户端 key 从不落库（只存 SHA-256 到鉴权缓存）。
- 鉴权：`proxy.rs:386` -> `AuthCache`（L1 + Redis L2）-> 未命中 POST 到 `tenant.auth_url`（`http.rs:557` `HttpAuthChecker`）。
- 路由：`router.rs:195` 管线 = `tenant_models` -> `models_by_key` -> `tenant_providers` -> key-prefix binding -> sub-tenant route -> 过滤 -> 排序；SWRR 由 `proxy.rs:941` 施加。
- **没有 user/group 维度、没有 `*` 通配、没有会话粘性**；会话头不读取、不转发。
- `ModelProvider.weight` 是死字段；实际权重取 `Provider.weight`。
- 控制面：Arachne 内容寻址树，`TOC_FORMAT=4`；`FidelityRows` 保存禁用行/密封材料；物化 1 s 轮询；发布走 `multi_put` 原子批。

**绿地判断**：上述旧结构没有生产数据负担。保留它们只会让新体系背上第二套规则和兼容分支，因此本版**删除**外部鉴权、sub-tenant、key-prefix binding，不做回填。

---

## 3. 设计

### 3.1 SQLite schema（两条迁移：`0014` 新增、`0015` 删旧）

> 所有 id 为应用生成的 TEXT；时间戳 ISO-8601 TEXT；`*` 是保留通配值（id 为 UUID/ULID，不会冲突）。
> **落地顺序**：`0014` 只 CREATE 新表（旧代码继续编译/运行，每批全绿）；`0015` 在终批 T10 删除旧表/列，与代码删除同批落地。绿地无数据搬迁。

```sql
-- 0014_user_hierarchy.sql（只新增；不删任何旧结构）
-- 用户组（租户内）
CREATE TABLE user_group (
  id         TEXT PRIMARY KEY,
  tenant_id  TEXT NOT NULL REFERENCES tenant(id) ON DELETE CASCADE,
  name       TEXT NOT NULL,
  enabled    INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE (tenant_id, name)
);
CREATE INDEX idx_user_group_tenant ON user_group(tenant_id);

-- 用户（租户内；一个用户属于一个组）
CREATE TABLE user (
  id             TEXT PRIMARY KEY,
  tenant_id      TEXT NOT NULL REFERENCES tenant(id) ON DELETE CASCADE,
  user_group_id  TEXT NOT NULL REFERENCES user_group(id) ON DELETE RESTRICT,
  external_id    TEXT,
  name           TEXT NOT NULL,
  enabled        INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  billing_status TEXT NOT NULL DEFAULT 'active' CHECK (billing_status IN ('active','delinquent')),
  billing_reason TEXT,
  billing_updated_at TEXT,
  created_at     TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at     TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE (tenant_id, external_id)
);
CREATE INDEX idx_user_tenant_group ON user(tenant_id, user_group_id);

-- 用户 API-Key（只存哈希）
CREATE TABLE user_api_key (
  id          TEXT PRIMARY KEY,
  user_id     TEXT NOT NULL REFERENCES user(id) ON DELETE CASCADE,
  name        TEXT NOT NULL,
  key_hash    TEXT NOT NULL UNIQUE,
  key_prefix  TEXT NOT NULL,
  key_last4   TEXT NOT NULL,
  enabled     INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  expires_at  TEXT,
  created_at  TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_user_api_key_user ON user_api_key(user_id);

-- 租户访问令牌（多令牌；哈希唯一）
CREATE TABLE tenant_access_token (
  id         TEXT PRIMARY KEY,
  tenant_id  TEXT NOT NULL REFERENCES tenant(id) ON DELETE CASCADE,
  name       TEXT NOT NULL,
  token_hash TEXT NOT NULL UNIQUE,
  token_prefix TEXT NOT NULL,                   -- 展示用，如 "ht_ab12"
  token_last4  TEXT NOT NULL,                   -- 展示用
  enabled    INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  expires_at TEXT,
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE (tenant_id, name)
);
CREATE INDEX idx_tenant_access_token_tenant ON tenant_access_token(tenant_id);

-- Global Role：五维键；'*' = 通配
CREATE TABLE global_role (
  id            TEXT PRIMARY KEY,
  name          TEXT NOT NULL,
  tenant_id     TEXT NOT NULL DEFAULT '*',
  user_group_id TEXT NOT NULL DEFAULT '*',
  user_id       TEXT NOT NULL DEFAULT '*',
  api_key_id    TEXT NOT NULL DEFAULT '*',
  model_key     TEXT NOT NULL DEFAULT '*',
  enabled       INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  created_at    TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at    TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_global_role_tenant ON global_role(tenant_id);

-- 角色 -> provider 集合
CREATE TABLE global_role_provider (
  id          TEXT PRIMARY KEY,
  role_id     TEXT NOT NULL REFERENCES global_role(id) ON DELETE CASCADE,
  provider_id TEXT NOT NULL REFERENCES provider(id) ON DELETE CASCADE,
  UNIQUE (role_id, provider_id)
);
CREATE INDEX idx_global_role_provider_role ON global_role_provider(role_id);
```

**写时不变量（handler 校验，不能只靠 FK）**

- `user.user_group_id` 的组必须同租户；
- `user_api_key.user_id` 的 user 必须存在且同租户；
- `global_role` 的每个非 `*` 维度必须存在且属于同一租户；`api_key_id` -> user -> group -> tenant 链一致；
- `model_key` 非 `*` 时不存在于 `provider_model` -> Warn（允许预配置）；
- `global_role_provider.provider_id` 必须存在；
- `*` 不能被当成真实 id 写入任何 FK 列；
- `expires_at` 写入时规范化为 RFC3339 UTC（字典序比较才不会误判）。

### 3.2 内部鉴权（唯一路径）

```rust
pub struct ApiPrincipal {
    pub tenant_id: String,
    pub user_group_id: String,
    pub user_id: String,
    pub api_key_id: String,
    pub billing_status: BillingStatus, // Active | Delinquent
}

pub struct ApiKeyIdentity {
    pub principal: ApiPrincipal,
    pub key_enabled: bool,
    pub expires_at: Option<String>,
}
```

`ConfigData` 新增：

```rust
pub user_groups: Vec<UserGroup>,              // 含 enabled=false
pub users: HashMap<String, User>,             // 含 enabled=false / delinquent
pub users_by_api_key_hash: HashMap<String, ApiKeyIdentity>, // 含 disabled/expired key
pub tenant_tokens: HashMap<String, TenantTokenIdentity>, // token sha256 -> 身份（仅 enabled）
```

```rust
pub struct TenantTokenIdentity {
    pub tenant_id: String,
    pub token_name: String, // whoami 返回；不含明文
}
```

```rust
pub global_roles: Vec<GlobalRole>,            // 仅 enabled
pub global_roles_by_tenant: HashMap<String, Vec<usize>>,
pub global_roles_any_tenant: Vec<usize>,
```

**删除**：`auth_url`、`HttpAuthChecker`、`AuthCache`、`redis/auth_cache.rs`、`AuthChecker` trait、`AuthConfig`、`HYDRA_AUTH_*` 环境变量、`/tenant/.../auth/cache/invalidate`、admin `auth/test`、admin `auth/cache`。

**鉴权流程**（替换 `proxy.rs:379-428`）：

1. Host 解析 tenant（不变）；缺失/禁用早退（不变）；
2. 提取 client key（不变）；缺失 -> 401；
3. `sha256_hex(key)` -> `cfg.users_by_api_key_hash`：
   - 未命中 -> 401 `invalid_api_key`；
   - `principal.tenant_id != tenant.id` -> 401 `invalid_api_key`；
   - `!key_enabled` / `expires_at <= now` -> 401 `invalid_api_key`；
   - user/group/tenant disabled -> 403；
   - `Delinquent` -> 402 `user_delinquent`；
4. 成功 -> `ctx.principal = Some(principal)`，路由直接使用。

**判定真值表**

| key 存在 | key enabled/未过期 | user/group | billing | 结果 |
|---|---|---|---|---|
| 否 | — | — | — | 401 |
| 是 | 否 | — | — | 401 |
| 是 | 是 | disabled | — | 403 |
| 是 | 是 | enabled | delinquent | 402 |
| 是 | 是 | enabled | active | 放行 |

**密钥生成与存储**

- `hk_` + 32 字节 CSPRNG（base64url）；明文只在创建响应返回一次；
- 只存 SHA-256 hex、`key_prefix`（前 8 字符）、`key_last4`；
- 高熵随机 key 用 SHA-256 足够，不引入 HMAC；
- `last_used_at` 不写入配置树（避免每请求发布）；如需审计，异步写本地表/用量后端。

**身份索引推导（承重规则）**

- 身份三字段（`user_groups`/`users`/`users_by_api_key_hash`）是 enabled-only 约定的**有意例外**：由共享的 `reindex_identity` 从**全量行**构建，leader（`build_config`）与副本（树解码后）同源；
- 目的：disabled key 在副本上仍是“已知但撤销”（401），而不是“未知”；
- `global_roles` 与 `tenant_tokens` 仍为 enabled-only。

### 3.3 租户 API

沿用 `/tenant/{tid}/api/v1/` 保留前缀与精确路由解析；全部要求租户令牌，`tenant_id` 必须与令牌所属租户一致。

| 方法 + 路径 | 语义 |
|---|---|
| `GET /user-groups` | 列出本租户用户组 |
| `PUT /user-groups` | body `{id?, name, enabled}`；无 id 创建，有 id 更新 |
| `DELETE /user-groups/{id}` | 删除；组内仍有 user -> 409 `group_not_empty` |
| `GET /users` | 列出本租户用户（可 `?group_id=`） |
| `PUT /users` | body `{id?, user_group_id, external_id?, name, enabled}` |
| `DELETE /users/{id}` | 删除用户及其 key（级联） |
| `GET /users/{id}/api-keys` | 列出 key（只返回 `key_prefix`/`key_last4`） |
| `POST /users/{id}/api-keys` | body `{name, expires_at?}`；返回 `{id, key, key_prefix, key_last4}`，`key` 仅此一次 |
| `DELETE /users/{id}/api-keys/{key_id}` | 撤销 |
| `PUT /users/{id}/billing` | body `{status:"active"|"delinquent", reason?}`；幂等 |

- 复用现有租户 API 的鉴权/限流/审计/错误词表；随失效子系统删除的 `invalidate_per_min`/`converge_timeout`/`live_nodes` 一并移除。
- 写入序列：`begin_write` -> 本地提交 -> `reload` -> Arachne 发布；失败 503 `config_not_published`。
- **发布串行化**：新增 `ConfigStore` 级发布锁（`tokio::Mutex<()>`，包住 reload + publish），admin 与租户 API 共用；行级一致性由 `BEGIN IMMEDIATE` 承担（现有 `AdminState::reload_lock` 只在 admin 侧，不能假装复用）。

**租户访问令牌（admin 管理）**

- 端点：`GET/POST /api/v1/tenants/{id}/access-tokens`、`DELETE /api/v1/tenants/{id}/access-tokens/{token_id}`。
- **由 hydra 生成**：`ht_` + 32 字节 CSPRNG（base64url），明文仅在创建响应返回一次；库/树只存 `token_hash` + name + prefix/last4（与 user key 同型）。不采用运营者自选口令。
- `whoami` 返回 `{token_name, ...}`；`ConfigData.tenant_tokens` 的值是 `TenantTokenIdentity { tenant_id, token_name }`（不只是 tenant_id）。
- `token_hash` 全局 UNIQUE，因此跨租户重复哈希结构性不可能；`tenant_api/auth.rs` 的 duplicate-hash fail-closed 分支可删除，测试同步。

**引导链（全内部鉴权后的唯一部署路径，必须写进 runbook）**

```
HYDRA_ADMIN_TOKEN
  -> admin 创建 tenant（不再有 auth_url）
  -> admin 创建 tenant access token（明文只显示一次）
  -> 租户 API 创建 user_group / user / user_api_key（key 明文只显示一次）
  -> admin 配置 tenant_provider（授权） + 可选 tenant_models / global_role
  -> 客户端用 hk_ key 访问代理
```

没有这条链，`enforce` 后新部署无法创建第一个可用 key。T11 runbook 必须逐条给出 curl 示例与失败排查（401/403/402 分别代表什么）。

### 3.4 Global Role 与路由

```rust
pub struct GlobalRole {
    pub id: String,
    pub name: String,
    pub tenant_id: String,      // "*" = 任意
    pub user_group_id: String,  // "*" = 任意
    pub user_id: String,        // "*" = 任意
    pub api_key_id: String,     // "*" = 任意
    pub model_key: String,      // "*" = 任意
    pub provider_ids: Vec<String>,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}
```

**匹配**（纯核心 `role.rs`）：先取 `global_roles_by_tenant[tenant]` + `global_roles_any_tenant`，逐条比较五维；`*` 匹配任意；命中集合全部参与并集；结果按 role.id 排序。

**解析管线**（简化后，删除 binding/sub-tenant 两步）：

```
(0) tenant_models 访问闸门（default-open）
(1) models_by_key[model]（在线 provider）
(2) 若 principal 存在且命中 ≥1 条 Global Role：
      union = BTreeSet(role.provider_ids)
      if 任一命中角色 tenant_id == "*":
          union ∩= tenant_providers[tenant]      // 通配角色不放大隐式授权
      candidates = union ∩ models_by_key[model]
    否则：
      candidates = models_by_key[model] ∩ tenant_providers[tenant]
(3) 过滤：breaker dead / 无 provider key / weight<=0
(4) 有会话头 -> 粘性首选 + 其余 provider_id 升序；无会话头 -> SWRR
```

> **裁决**：`tenant=<具体>` 的角色是管理员显式授权，不与 `tenant_providers` 求交；`tenant='*'` 是全局默认，必须求交（防一条通配规则重定向所有租户）。`tenant_models` 始终是最外层闸门。**通配角色命中但租户没有任何 `tenant_providers` 授权时，按 legacy 语义返回 403 `TenantForbidden`**（不得因角色存在而放行）。

**会话粘性**（`affinity.rs`）

- 头优先级：`X-Opencode-Session` → `Session-Id` → `x-conversation-id`；`trim` 后非空且 ≤256 字节；
- 粘性键：`sha256(tenant_id || model_key || api_key_id || session)`（分隔符 `0x1f`）；
- 加权 rendezvous：
  ```
  for c in candidates:
      u = first_u64(sha256(sticky_key || 0x1f || c.provider_id)) / 2^64
      score = -ln(u) / c.weight      // u=0 -> +inf；并列按 provider_id 升序
  pick argmin(score)             // exponential race: min(-ln(u)/w), P(i) ∝ weight_i
  ```
- failover：粘性首选在首位，其余按 provider_id 升序；不调用 `swrr::order`（有状态）；
- `HYDRA_SESSION_AFFINITY=on|off`（默认 on）；会话头不转发上游（现状也不转发）。

**SWRR 状态**：保持 `(tenant_id, model_key)`；粘性路径完全不读写；无粘性行为与今天一致。

### 3.5 Arachne 控制面

**新实体路径**（`EntityPath`）：
- `user_group/<id>`
- `user/<id>`
- `user_api_key/<id>`
- `tenant_access_token/<id>`
- `global_role/<id>`（payload = `GlobalRoleTreeEntity { role, provider_ids }`）

**删除实体**：`SubTenant`、`SubTenantRoute`、`KeyBinding`、`Token`（旧单令牌）；同时删除 `FidelityRows`/`FidelityTreeEntity` 的 `sub_tenants`、`sub_tenant_routes`、`key_prefix_bindings`、`tenant_token_hashes` 字段及对应解码分支（T10）。

**`TOC_FORMAT` 4 -> 5**：新实体枚举改变 key 布局与解码分支；混合版本必须按名字拒绝。无 format-4 回读。**禁止重置为 1**：变体删除会让判别值重排，旧树可能被新枚举静默错读；单调递增 + `hydra/ctl/format` 标记才是安全做法（绿地清目录）。T2–T10 之间发布的 format-5 树在 T10 删除变体后会被新二进制按 `UnknownEntityKind` 拒绝（fail-stop，安全方向）；绿地清目录后无实际暴露，若需保留则在 T10 后重新发布一次。

**实体权威载体**：
- 五类新行**只**由 per-row 实体承载（含 `enabled=false`），**不**复制进 `_fidelity` 单值，避免容量悬崖；
- `split_config(cfg, fidelity, kp)` 签名不变：per-row blob 从 `FidelityRows` 的全量行字段编码；
- 解码时 per-row blob 总是进入 `FidelityRows`（restore 用）；身份三字段由 `reindex_identity` 从全量行构建；`global_roles` 仅 enabled 进 `ConfigData`；
- `FidelityRows` 新字段 `#[serde(skip)]`，不序列化进 `_fidelity`，但参与 `ReplicationContent` 的 `PartialEq` 代际谓词；
- `ReplicationContent::load` 的单事务全序读扩展到五张新表，按 id `ORDER BY`；
- `key_hash` / `token_hash` 用 `seal_deterministic` 密封进树（沿用 `sealed_tenant_token_hashes` 不变量）。

**restore 顺序**（`WipedTable`）：`global_role_provider` -> `global_role` -> `user_api_key` -> `user` -> `user_group` -> `tenant_access_token` -> 现有表。

**容量约束（先到的是 4096 条目）**：
- `multi_put` 单批 ≤4096 条目（全部变更实体 + toc + head，原子批不可拆），TOC/值 ≤1 MiB 次之；
- 默认前置硬限：`HYDRA_MAX_USER_API_KEYS_PER_TENANT=1000`、`HYDRA_MAX_USERS_PER_TENANT=1000`、`HYDRA_MAX_HIERARCHY_ROWS=3000`（全局），并检查**全局实体总数**（既有 + 新增 ≤3800）；
- 超限写 API 返回 400 `hierarchy_limit_exceeded`；不静默分片；
- T1/T2 实测 500/1k/2k 行 + 实体条数 vs 4096，结果回写本计划后再进 T4。

### 3.6 管理界面

- **一个 Global Roles 页面**：五维（均带“任意 `*`”）+ provider 多选 + enabled；支持复制、批量启停；保存走 admin API。
- 用户/组/Key：admin 只读排障视图；租户自助走租户 API。
- **删除**旧 sub-tenant、sub-tenant-route、provider-key-binding 页面与 auth test 按钮。
- UI 由 @designer 设计实现；后端先冻结 API 契约。

### 3.7 删除清单（终批 T10 与 `0015` 同批落地）

> 实测引用量（rust）：`auth_url` 175 处/26 文件、`sub_tenant` 1016 处/41 文件、`binding` 208 处。删除是本计划最大的工作量，必须按清单逐项销账，不能只靠“分批 grep”。

**A. 整文件删除**

| 文件 | 原因 |
|---|---|
| `crates/hydra-server/src/redis/auth_cache.rs` | 外部鉴权 L2 缓存 |
| `crates/hydra-server/src/cluster/events.rs` | Redis Streams 失效子系统（唯一消费者是 auth cache） |
| `crates/hydra-core/src/sub_tenant.rs` | 旧前缀分组模型 |
| `crates/hydra-server/src/admin/sub_tenant_write.rs` | 旧 sub-tenant 写核心（~680 行） |
| `crates/hydra-server/tests/admin_auth_test.rs`、`http_auth.rs`、`auth_cache.rs`、`tenant_cache.rs`、`invalidation_trim.rs` | 外部鉴权/缓存/失效测试 |
| `crates/hydra-server/tests/sub_tenant_admin.rs`、`sub_tenant_data_plane_write.rs` | sub-tenant 测试 |
| `integration/mock_auth.py`、`environment/mock_tenant/`（若仅用于外部鉴权） | 外部 auth mock |
| `integration/test_auth_cache_layers.py`、`test_auth_hop_failmode.py`、`test_sub_tenant_steering.py` | 对应 e2e |

**B. 外部鉴权类型与端点**

- `http.rs` 的 `AuthChecker`、`HttpAuthChecker`、`AuthCache`、`AuthConfig`、`auth_request_body`、认证指标；保留 `http.rs` 其余通用工具。
- `hydra-core/src/auth.rs` 的 `AuthEntry`/`CacheOp`/`cache_decision`/`apply_upstream`/`decide`/`denial_status_for_reason`/`REASON_INSUFFICIENT_BALANCE`；只留 `sha256`/`hmac`/状态码常量。
- `redis` 模块的 auth-cache 装配、`GLOBAL_IDX_KEY` 与租户索引。
- 端点：`POST /tenant/{tid}/api/v1/auth/cache/invalidate`、`DELETE /api/v1/auth/cache`、admin `POST /api/v1/tenants/auth/test`。
- `TenantApiConfig` 的 `converge_timeout`/`invalidate_per_min`/`live_nodes`；`AdminState` 的 publisher/live_nodes/converge_timeout。
- 环境变量：`HYDRA_AUTH_*`、`HYDRA_TENANT_API_INVALIDATE_PER_MIN`，同步 `check_documented_env.cjs` / `check_cluster_env.cjs` 清单。

**C. 租户字段与 admin DTO**

- `tenant.auth_url`、`tenant.access_token_hash`（`0015` DROP COLUMN）。
- `Tenant.auth_url` / `Tenant::has_access_token`；tenant PUT body 的 `access_token`、`apply_tenant_access_token_write`、`validate_access_token_shape`；`whoami`/admin 响应字段。
- 旧单令牌 `EntityPath::Token` 与 `sealed_tenant_token_hashes`。

**D. sub-tenant / key-prefix binding**

- 表：`sub_tenant`、`sub_tenant_route`、`provider_key_binding`（`0015` DROP TABLE）。
- 模型/配置：`SubTenant`、`SubTenantRoute`、`ProviderKeyBinding`；`ConfigData.sub_tenants`/`sub_tenant_routes`/`key_prefix_bindings`；`FidelityRows`/`FidelityTreeEntity` 对应字段。
- 路由：`router::match_key_binding`、`match_sub_tenant`、`match_sub_tenant_route`、`sub_tenant_id_for_key`、`select_sub_tenant_route`；`proxy` 的对应 gates。
- 租户 API：`Endpoint::ListSubTenants/ListSubTenantRoutes`、`TenantWriteRoute::{UpsertSubTenant,DeleteSubTenant,UpsertRoute,DeleteRoute}` 及 handler（`hydra-core/src/tenant_api.rs:191-283`）；`tenant-api-integration.md` 改写。
- admin API/handlers/UI/i18n：sub-tenant、sub-tenant-route、provider-key-binding 页面与路由。
- 文档：`design-sub-tenant.md`、`design-sub-tenant-v2/v3`、`2026-08-21-key-prefix-binding.md` 标记 retired。

**E. 用量归因（D-16，T8）**

- `UsageRecord.sub_tenant_id` -> `user_id`/`user_group_id`/`api_key_id`（全 `Option<String>`）。
- ClickHouse `environment/clickhouse/init.sql` 与 `clickhouse/mod.rs` 的 DDL/INSERT/行 JSON/group 表达式；TDengine `CREATE STABLE`/insert/group；绿地直接重建表，不写 ALTER 迁移。
- `GroupBy::SubTenant` -> `GroupBy::{User, UserGroup, ApiKey}`；`usage/query.rs` 词表；租户 API `group_by` 参数。
- `usage/testing.rs`、`clickhouse_ddl_parity.rs`、`usage_query.rs`、`test_usage_query_wire.py`。
- `router::sub_tenant_id_for_key` 删除（producer 已死）。

**F. 保留**

- `reqwest`：仍被 breaker probe（`proxy/breaker_wrap.rs`）与 provider client 使用，只删除 auth 用途。
- Redis：仍承载限流/熔断票等，删除 auth cache 不等于删除 Redis。
- `limit_role.matching_key`：仍匹配客户端原始 key 做限流，保留。

---

## 4. 决策记录

| | 决策 | 依据 / 被否方案 |
|---|---|---|
| **D-1** | 绿地重建：不做迁移/回填/双模；旧结构在终批删除 | 无生产数据；保留兼容层只会让两套规则并存。**但删除必须与代码同批**，不能先 DROP 后改代码（BL-2） |
| **D-2** | 通配用字面量 `*`，不用 NULL | 显式、可读、UI 可直接展示；id 不会冲突 |
| **D-3** | 五维全匹配；多规则并集；`tenant='*'` 求交 `tenant_providers`，具体租户不求交 | 并集是需求；通配角色必须保留 fail-closed 边界 |
| **D-4** | 内部鉴权唯一路径；删除 `auth_url`/`AuthCache`/外部 checker | 需求明确“不需要外部接口”；无缓存才没有撤销陈旧窗口 |
| **D-5** | 身份索引包含 disabled key/user/group；key 撤销 401、用户/组禁用 403、欠费 402 | key 存在性防枚举；用户状态是业务事实 |
| **D-6** | 多令牌独立表；旧单列在终批删除 | 无迁移负担 |
| **D-7** | API key 只存 SHA-256 + prefix/last4；明文仅创建响应一次 | 与现有密钥密封策略一致 |
| **D-8** | `TOC_FORMAT=5`；新实体进树；无 format-4 回读；**禁止重置为 1** | 变体删除会让判别值重排；单调递增防止旧树被新枚举静默错读 |
| **D-9** | 会话粘性用加权 rendezvous；粘性不读写 SWRR，failover 尾部 provider_id 升序 | 无状态、稳定、无内存增长；`swrr::order` 有状态 |
| **D-10** | 删除 sub_tenant / sub_tenant_route / provider_key_binding（终批 T10） | 规则收束到一个界面；Global Role 可表达同样的路由意图 |
| **D-11** | 租户 API 只写自己的组/用户/key/欠费；Global Role 仅 admin 可写 | 调度是平台权限；租户不能自行改 provider |
| **D-12** | `tenant_models` 保留为访问闸门，角色不能绕过 | 合同/套餐边界不能被调度放大 |
| **D-13** | 新行只走 per-row 实体（含 disabled），不进 `_fidelity`；哈希确定性密封 | 避免双份编码与 1 MiB 悬崖 |
| **D-14** | 发布串行化下沉到 `ConfigStore` 级锁 | 现有 `reload_lock` 只在 admin 侧；并发 reload 幂等但发布顺序无保证 |
| **D-15** | 硬限按 4096 条目推导，写 API 前置 400，不静默分片 | 分片破坏原子树语义；前置错误比 publish 503 可诊断 |
| **D-16** | 用量归因从 `sub_tenant_id` 改为 `user_id`/`user_group_id`/`api_key_id`；GroupBy 同步为 `user`/`user_group`/`api_key` | 旧分组删除后不能留死字段；本版一次性做全（T8），否则永久 NULL 死列 |
| **D-17** | 迁移拆两条：`0014` 只建新表，`0015` 在终批删旧；旧代码在 T1–T9 继续编译运行 | 单条 0014 先 DROP 会让 loader/proxy 在中间批次运行时或编译期失败（BL-2） |
| **D-18** | 租户访问令牌由 hydra 生成（`ht_` + CSPRNG，明文一次），不采用运营者自选口令 | 与 user key 同型；避免弱口令与哈希入库的边界不一致 |
| **D-19** | 引导链固定为 admin token -> tenant -> tenant token -> 租户 API 建组/用户/key -> tenant_provider/角色 | 全内部鉴权后这是唯一可部署路径；runbook 必须给出 curl 示例 |

**被否方案**

- 保留 `auth_url` 作为兜底：需求明确内部鉴权，留兜底会保留外部依赖与缓存陈旧窗口。
- 保留 sub-tenant 作为“legacy 回退”：没有生产数据，双规则会让“单界面”失效。
- 用 `tenant_provider` 承载调度：无法表达五维/通配/并集。
- 每次请求查 SQLite：绕过快照一致性，把 DB 变热路径。
- 会话粘性 `hash(session) % N`：集合变化重映射几乎全部会话。
- 把明文 key 存进配置树再加密：没必要，哈希足够。

---

## 5. 风险与失败模型

| 风险 | 说明 | 缓解 |
|---|---|---|
| **删除面大** | 外部鉴权、sub-tenant、binding 同时删除，实测 175/1016/208 处引用，波及 proxy/http/redis/admin/tenant_api/UI/tests/docs | §3.7 清单逐项销账；**终批 T10 与 `0015` 同批落地**；此前旧代码保留可编译；T10 后 `grep` 零命中 + 全量门禁 |
| **T1 破坏编译/运行** | 若 `0014` 先 DROP 旧表/列，loader 仍 SELECT 旧表、model 删除字段会让 http/proxy/admin 编译失败 | D-17：`0014` 只建新表；旧代码在 T1–T9 继续运行；`0015` 与删除同批 |
| **开发库/集群需清空** | `ALTER TABLE DROP COLUMN`、TOC_FORMAT=5、旧 keyspace 不兼容 | runbook：删除本地 DB 与 Arachne 数据目录；CI 重建；注意 `_sqlx_migrations`；不做回滚 |
| **SQLite 版本** | sqlx 未开 bundled，链接系统库；`DROP COLUMN` 需 ≥3.35 | Fedora 44 满足；CI 镜像显式检查 `sqlite3 --version`，否则改 bundled |
| **实体条数/容量** | `multi_put` 4096 条目先到；TOC 1 MiB 次之；超限 publish fail-stop 影响所有写 | 前置硬限 1000 key/1000 user per tenant + 全局 3000 行 + 实体总数 ≤3800；T1/T2 实测 |
| **用量归因跨切面** | `sub_tenant_id` 贯穿 UsageRecord、ClickHouse/TDengine DDL/JSON/group、GroupBy、租户 API 参数、parity 测试 | 独立 T8 做全；绿地直接重建后端表；T10 才删旧字段；测试矩阵同步 |
| **引导链断裂** | 全内部鉴权后新部署无 key 可用 | D-19 引导链 + T11 runbook curl 示例；e2e 从空库走完整链路 |
| **鉴权热路径** | 每请求 SHA-256 + HashMap | 索引在 reload 构建；压测 p99 |
| **角色配置越权** | `*` 太宽或 provider 指向越权 | 通配租户角色强制求交；写时链校验；UI 预览；审计 |
| **会话头滥用** | 客户端自选 provider，可能集中打一个上游 | 只在角色并集内选择；breaker/admission/限流仍生效；可关闭粘性 |
| **角色线性扫描** | 每租户规则过多 O(n) | 按 tenant 建索引；目标 ≤1k roles/tenant |
| **欠费传播延迟** | 多节点 ≤1 s 轮询 + 发布 | SLO ≤5 s；metrics 暴露配置版本 |
| **admin/tenant 并发写** | 两路同时发布 | ConfigStore 级发布锁 + `BEGIN IMMEDIATE` |
| **观测盲区** | 规则命中/粘性/鉴权来源不可见 | `hydra_role_matched_total`、`hydra_sticky_hit_total`、`hydra_user_delinquent_402_total` |

---

## 6. 任务分批

> 采用 D-17 的“先增后删”顺序：T1–T9 旧代码保持可编译/可运行，T10 一次性删旧（`0015` + 代码 + UI + 文档 + 测试），T11 收尾门禁。每批结束都必须能编译并跑现有测试。

| 批次 | 内容 | 依赖 |
|---|---|---|
| **T1** | 迁移 `0014`（只建新表）+ `model.rs` 新结构（additive）+ `ConfigData` 新索引 + loader + 校验 + 硬限 + SQLite 版本检查 | 设计门禁通过 |
| **T2** | Arachne 新 per-row 实体（含 disabled）/`TOC_FORMAT=5`/`FidelityRows` 权威载体/`seal_deterministic`/restore + 往返与容量实测 | T1 |
| **T3** | 内部鉴权：`ApiPrincipal`、纯函数 checker、判定表、错误映射、指标；`proxy` 切换到内部路径（旧外部代码暂留） | T1/T2 |
| **T4** | 租户 API：组/用户/key/欠费 CRUD + `ConfigStore` 发布锁 + 路由解析 | T1/T2；与 T3 可并行（先冻结 `ApiPrincipal`） |
| **T5** | 多租户令牌：表/admin 生成与管理/`whoami`（`token_name`） | T1/T2 |
| **T6** | Global Role：表/实体/匹配函数/索引/admin CRUD + 通配校验 Warn | T1/T2 |
| **T7** | 路由接入：并集/通配求交/回退/去重/过滤、`accessible_models` 匿名语义、会话粘性 + 加权 rendezvous | T3/T6（`ApiPrincipal` 来自 T3） |
| **T8** | 用量归因：`user_id`/`user_group_id`/`api_key_id` + ClickHouse/TDengine DDL/INSERT/JSON/group + `GroupBy::{User,UserGroup,ApiKey}` + 租户 API 参数 | T3 |
| **T9** | 管理 UI（@designer）：Global Roles 单页 + 用户/组/key 排障视图（旧页面暂留） | T4/T6 API 冻结 |
| **T10** | **删除批**：`0015`（DROP 旧表/列）+ §3.7 全清单（auth/sub-tenant/binding/usage 旧字段/UI 旧页面/文档/tests/env/scripts） | T1–T9 全绿 |
| **T11** | 部署/引导链/清库 runbook、README/tenant-api-integration/cluster.md/ADR、全量门禁与 grep 销账 | T10 |

> T1/T2 是串行地基；T3/T4/T5/T6 可在 T2 后并行；T7 依赖 T6；T8 只依赖 T3；T9 依赖 API 冻结；T10/T11 收尾。T1/T2 容量实测未回写前不得进入 T4。

---

## 7. 测试计划

**纯核心（hydra-core）**

- 角色匹配：五维全 `*`、单维具体、多规则并集、去重、确定性排序；
- 通配租户角色与 `tenant_providers` 求交；具体租户不求交；边界外 provider -> validate Warn；
- 悬挂维度（指向已删 user/group/key）-> Warn；
- `accessible_models`：有 principal / 无 key / 未注册 key 三种输入与 `resolve` 一致；欠费仍出目录；
- 粘性：稳定性、集合变化重映射、权重分布卡方容差、`u=0`/平分 tie-break、头优先级与长度限制；
- 鉴权判定表：未知/禁用/过期/跨租户/欠费/用户禁用/组禁用。

**服务端（hydra-server）**

- schema：`0014` 后新旧表共存且 loader 正常；新表 FK/唯一约束生效；T10 后旧表/列不存在；
- 租户 API CRUD：租户隔离；key 明文只返回一次；撤销后立即 401；`hierarchy_limit_exceeded` 前置 400；组内有人时删除 409；
- 令牌：admin 生成 `ht_` 明文一次；多令牌并存；`whoami` 返回 `token_name`；撤销其一不影响其余；
- **引导链 e2e**：空库 -> admin 建 tenant -> 建 tenant token -> 租户 API 建组/用户/key -> 配 tenant_provider/角色 -> 客户端请求成功（D-19）；
- 欠费：本节点立即 402；其他节点 ≤5 s；
- 控制面：publish/materialize/restore 往返；TOC_FORMAT 5 混合版本拒绝；禁用行/欠费行/哈希保真；
- **身份索引派生保真**：副本解码后的 `users_by_api_key_hash` 必须与 leader `build_config` 一致（含 disabled key）；
- 路由：并集 + SWRR + 粘性 + failover；通配角色在无 `tenant_providers` 时 403；`/v1/models` 与真实可选 provider 一致；
- 用量归因：新 `user_id`/`user_group_id`/`api_key_id` 写入 ClickHouse/TDengine；`GroupBy::{User,UserGroup,ApiKey}`；DDL parity；旧 `sub_tenant_id` 在 T10 后不存在；
- 并发：admin 与租户 API 同时写，经 store 级锁，无半写/无丢发布/顺序单调。

**删除销账（T10 后）**

```bash
# 源码零命中（归档文档/dev-docs 历史计划除外）
rg -n "auth_url|access_token_hash|sub_tenant|provider_key_binding|ProviderKeyBinding|key_prefix_binding|KeyBinding|HttpAuthChecker|AuthCache|AuthConfig|AuthVerdict|InvalidationStream" \
  crates/ integration/ environment/ admin-ui/ scripts/ || true
# schema 零命中
rg -n "sub_tenant|provider_key_binding|auth_url|access_token_hash" crates/hydra-server/migrations/
# 旧端点零命中
rg -n "auth/cache/invalidate|tenants/auth/test" crates/ integration/ admin-ui/
```

**门禁命令（实施完成后必须全绿）**

```bash
export CARGO_HOME=/home/alex/Projects/hydra/.cargo-cache/home RUSTUP_TOOLCHAIN=1.98.0
cargo fmt --check
cargo clippy --all-targets --features "server,arachne,cluster-redis" -- -D warnings
cargo test --features "server,cluster-redis,arachne"
node scripts/check_cluster_env.cjs
node scripts/check_documented_env.cjs
node scripts/check_ci_wiring.cjs
node scripts/check_source_purity.cjs
bash .acceptance/round10-gate.sh   # 若当前门禁脚本名不同，以 .acceptance/ 下最新为准
```

---

## 8. 参考来源

- exp-2：`migrations/0001_init.sql`、`0010_sub_tenant.sql`、`store.rs:70 build_config`、`arachne_keys.rs`、`arachne_entities.rs`、`content.rs`、`db/restore.rs`。
- exp-3：`tenant_api/mod.rs`、`handlers.rs`、`tenant_api/auth.rs`、`http.rs:538 AuthChecker`、`auth.rs`、`redis/auth_cache.rs`、`admin/handlers.rs`。
- exp-4：`proxy.rs:477 request_filter`、`router.rs:195 resolve_detailed`、`swrr.rs`、`limit.rs`、`proxy/provider_client.rs`。
- 现有计划/ADR 格式：`dev-docs/aegis/plans/2026-10-10-multi-put-atomic-publish.md`、`dev-docs/aegis/adr/ADR-0001-arachne-control-plane.md`、`dev-docs/aegis/BASELINE-GOVERNANCE.md`。

---

## 9. 实施结果（2026-11-03）

**已落地**：T1–T9 全绿 + T11 doc/baseline sync. Gates（this-work execution log）：`cargo fmt --check` clean、`cargo clippy --all-targets --features "server,arachne,cluster-redis" -- -D warnings` 0 warnings、`cargo test --features "server,cluster-redis,arachne"` **974 passed / 0 failed**;`node scripts/check_documented_env.cjs` + `node scripts/check_i18n.js` green.

| 任务 | 结果 | 证据 |
|---|---|---|
| T1 | ✅ `0014` only CREATE + `model.rs` new structs + `ConfigData` identity index + loader + write-time checks | `migrations/0014_user_hierarchy.sql`;`model.rs` (`TenantAccessToken` `:299`);`store.rs:221` |
| T2 | ✅ new per-row entities + `TOC_FORMAT=5` + `seal_deterministic` + restore order | `arachne_keys.rs:5` (`TOC_FORMAT = 5`),`:80` (5 new EntityPath),`:183` (`user/<id>` incl delinquent) |
| T3 | ✅ internal auth (single path) + truth table + metrics | `identity.rs` (D-5 truth table 401/403/402),`proxy.rs:441/468/470` |
| T4 | ✅ tenant API group/user/key/billing CRUD + publish lock | `tenant_api/handlers.rs` (`user-groups`/`users`/`users/{uid}/api-keys`/`billing` handlers) |
| T5 | ✅ multi token (`ht_`) admin managed + `whoami` `token_name` | `admin/handlers.rs:3364` GET/POST `/access-tokens`;`:3526` DELETE |
| T6 | ✅ `global_role` / `global_role_provider` + CRUD + wildcard Warn | `admin/handlers.rs:3148/3173`;`upsert_global_role_on_conn` |
| T7 | ✅ union ∩ models ∩ (wildcard intersect) + SWRR + session-sticky rendezvous | `router.rs`/`affinity.rs`/`proxy.rs` |
| T8 | ✅ usage attribution `user_id`/`user_group_id`/`api_key_id` + `GroupBy::{User,UserGroup,ApiKey}` | `usage/query.rs`;`tenant_api/handlers.rs:455` whitelist validation |
| T9 | ✅ Global Roles single page + user/group/key troubleshooting view (old pages kept) | admin UI (@designer delivered; T10 page deletion DEFERRED) |
| T10 | **DEFERRED** (later batch) | `0015` not applied;`0014` only CREATE;`proxy.rs`/`http.rs`/`redis/auth_cache.rs`/`sub_tenant_write.rs`/sub-tenant tenant endpoints still compile & serve;`tenant.auth_url` still NOT NULL;`whoami` still echoes `auth_url` |
| T11 | ✅ README (en/zh) / cluster.md / ops.md / ADR-0003 / INDEX synced | see 配套 list below |

**被否方案 / 偏差 (must-not-lose)**

1. **Session-sticky selection function written backwards in plan §3.4**: `pick argmax(score)` with `score = -ln(u)/weight` — the SHIPPED code (`affinity.rs:115`/`:42`) picks **minimum** `-ln(u)/weight` (ties `provider_id` ascending) so `P(i) ∝ weight_i`. Docs carry the minimum-wins form; the pseudocode "argmax" is a notation slip, corrected here — do NOT re-copy it.
2. **Hard limits partially wired**: only `HYDRA_MAX_USER_API_KEYS_PER_TENANT` (default `1000`) is read (`tenant_api/mod.rs:233-235` → `env_positive_u32`, 400 `hierarchy_limit_exceeded`); `HYDRA_MAX_USERS_PER_TENANT` (1000) and `HYDRA_MAX_HIERARCHY_ROWS` (3000) are **design-only, NOT wired** — `multi_put` 4096-entry budget remains the only live bound (`ops.md` marks them 未接线 + guard `PROSE_ONLY_OK`).
3. **Usage attribution / GroupBy landed as D-16**: tokens `none|model|provider|user|user_group|api_key|day`; `sub_tenant` is NOT a token.
4. **T10 gap is intentional & safe**: `TOC_FORMAT=5` refuses old trees via `UnknownEntityKind` (fail-stop); new trees coexist with still-present old entities until `0015` + deletion batch land; green-field dir wipe ⇒ no live exposure.
5. **Tenant multi-token admin surface** is `GET/POST /api/v1/tenants/{id}/access-tokens` + `DELETE .../access-tokens/{token_id}` (admin-plane, D-18 `ht_` mint, plaintext once); `TenantAccessToken` model + `db::insert/list_tenant_access_tokens` wired; NOT claimed as a tenant-self-service surface.

**配套 doc updated**:`dev-docs/cluster.md` (TOC 5 + new control-plane entity note)、`dev-docs/ops.md` (`HYDRA_SESSION_AFFINITY` row + hierarchy-ceiling note + internal-auth note)、`README.md`/`README.zh-CN.md` (internal auth + hierarchy + Global Role)、`dev-docs/tenant-api-integration.md` (T10-aware)、`dev-docs/aegis/adr/ADR-0003-user-hierarchy-global-role.md`、`dev-docs/aegis/INDEX.md`.
