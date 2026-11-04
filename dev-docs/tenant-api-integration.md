# Hydra 租户自助 API 对接文档

> **给谁看**：接入 Hydra 的**租户**（及其研发/运维）。这是对外契约，可以照此写代码。
> **不给谁看**：Hydra 自身的实现细节见 `design-tenant-api.md`（内部设计）与 `ops.md`（运维手册）。
>
> 本文的每一条都对应仓库里可运行的代码/测试；凡是"你会看到的行为"，都有测试守着。若本文与线上行为不一致，那是**缺陷**，请找运维报。

---

> **⚠ 2026-11-04 重大契约变化（T10 已完成，read this first).** 数据面 client-key **鉴权是 INTERNAL**:Hydra 把 your client's api-key 对照 its **stored `user_api_key` rows** under the four-level hierarchy `tenant → user_group → user → api_key` — **`auth_url` is gone entirely** (the column was dropped, migration `0015`), and so is the auth cache. Consequences:
> - **The old auth-cache and sub-tenant endpoints are REMOVED** — `auth/cache/invalidate`, `/sub-tenants*` and `/sub-tenant-routes*` (read and write) no longer exist, and the error codes they answered (`key_prefix_overlap`, `name_duplicate`, …) are gone from §6. Revoke / disable / delinquency now flip **immediately**: local node next-request, other nodes ≤5 s via config publish. Do not build flows on the removed endpoints.
> - **The self-service endpoints (§5.7)** manage your groups / users / api-keys / billing.
> - `whoami` NO LONGER echoes `auth_url` (the column does not exist any more).
> - Client api-keys (`hk_…`) and tenant access tokens (`ht_…`) are **hydra-minted, plaintext shown exactly once**; a tenant may hold **multiple** access tokens.
> - A **delinquent** user's key answers **402 `user_delinquent`** on the data plane (body `{"error":{"message":"user_delinquent","type":"insufficient_quota","code":"user_delinquent"}}`).

---

## 1. 你能用它做什么

Current endpoints (2026-11-04) — the internal-auth hierarchy (incl. the §5.7 groups/users/keys/billing), all **ACTIVE** and run in **你已经在用的那个数据面地址**上（客户端调 `/v1/*` 的同一个域名/端口），因此不需要运维额外为你开放管理口。The old `auth/cache/invalidate` and `sub-tenant*` endpoints are REMOVED (T10 complete) — calls to them answer `404 not_found` like any other unknown path.

| 端点 | 方法 | 用途 |
|---|---|---|
| `/tenant/{tenant_id}/api/v1/whoami` | `GET` | read your config: domain, `enabled`, `config_version` |
| `/tenant/{tenant_id}/api/v1/usage` | `GET` | 查某个时间窗内你实际产生了多少请求/token |
| `/tenant/{tenant_id}/api/v1/user-groups` | `GET` · `PUT` · `DELETE .../{id}` | list / upsert / delete your user-groups (§5.7; non-empty delete ⇒ 409) |
| `/tenant/{tenant_id}/api/v1/users` | `GET ?group_id=` · `PUT` · `DELETE .../{id}` | list / upsert / delete your users (`user_group_id` required; delete cascades keys) (§5.7) |
| `/tenant/{tenant_id}/api/v1/users/{uid}/api-keys` | `GET` · `POST` · `DELETE .../{key_id}` | list / **mint (plaintext `hk_` shown ONCE)** / revoke a key (§5.7) |
| `/tenant/{tenant_id}/api/v1/users/{uid}/billing` | `PUT` | `active` / `delinquent` (delinquent ⇒ **402** on `/v1/*`) (§5.7) |

**明确没有的能力**（不是漏做，是刻意不做）：

- **不能改你的租户级配置**。域名、证书、是否启用，都由运维通过管理 API 设置（本 API 没有这些的写路径，所以没有"字段级越权"的可能）。**Your own groups / users / api-keys / billing are the writable part** (§5.7) — always scoped to only your own resources; **Global Role scheduling is operator-only** (admin `/api/v1/global-roles`), a tenant never repoints providers. The old sub-tenant writes are REMOVED.
- **不能查明细**。`/usage` 只给聚合数，不给逐条请求记录。
- **No auth cache any more.** Client-key auth is internal (§4 + the §5.7 truth table); revoke / disable / delinquency flip on the **next** request (no 5-minute window to wait out). MINTING is capped on three axes and always answered `400 hierarchy_limit_exceeded`, never silently sharded: per-tenant api-keys (`HYDRA_MAX_USER_API_KEYS_PER_TENANT`, default 1000), per-tenant users (`HYDRA_MAX_USERS_PER_TENANT`, default 1000), and the total row count of the five identity tables (`HYDRA_MAX_HIERARCHY_ROWS`, default 3000). A cap binds a CREATE only — editing an existing row is never refused (§5.7's error table says which endpoint answers which).

---

## 2. 快速开始

```bash
# 把这三个变量换成你实际的值
export HYDRA=https://gateway.example.com      # 数据面地址
export TID=t_acme                             # 你的 tenant_id
export TOK=sk-tenant-xxxxxxxxxxxxxxxxxxxxxxxx # 你的租户访问令牌

# ① 我是谁 / 我的配置是什么
curl -s $HYDRA/tenant/$TID/api/v1/whoami -H "Authorization: Bearer $TOK"

# ② 我昨天用了多少
curl -s "$HYDRA/tenant/$TID/api/v1/usage?since=2026-09-16T00:00:00Z&until=2026-09-17T00:00:00Z" \
  -H "Authorization: Bearer $TOK"
```

---

## 3. 怎么拿到令牌

1. 令牌由**运维**在你的租户上配置（管理 UI 的 Tenants 表单里的 `Access token`，或管理 API）。
2. 服务端只存 **SHA-256 摘要**，**永远不会回显明文**。所以**丢了只能让运维轮换**（换一个新值即完成轮换）。
3. 最短 16 个字符。建议 `openssl rand -hex 32` 生成。
4. 这是**你的**凭证，不是客户端的 api-key：
    - 它放在 `Authorization: Bearer` 里，只用于调用本文的§1 表里端点（the read + write endpoints of §5.x）；
   - 它**不会**被当成客户端 key 去鉴权，也不会写进用量记录（见 §8.4）；
   - **不要**把它发给你的终端用户或用在前端代码里。

---

## 4. 通用约定

### 4.1 鉴权

```
Authorization: Bearer <tenant access token>
```

- 身份**只**来自令牌。URL 里的 `{tenant_id}` 是**一致性交叉校验**：与令牌归属不一致 → `403 tenant_id_mismatch`。
- `Host` 头**不参与**身份判定。
- 令牌缺失 / 错误 / 该租户未配置令牌 → 一律 `401 unauthorized`，**文案完全相同**（刻意如此：不能让它变成"哪些令牌存在"的探测器）。

### 4.2 响应头

| 头 | 说明 |
|---|---|
| `X-Hydra-Trace-Id` | 每次请求的唯一追踪号。**排障时请提供它**：日志里用它定位。响应体里的 `error.trace_id` 与它一致 |
| `Retry-After` | 只在 `429` 出现（秒） |
| `Content-Type` | `application/json` |

### 4.3 错误信封（所有非 2xx 都是这个形状）

```json
{
  "error": {
    "code": "invalid_since",
    "message": "`since` must be RFC3339 (2026-09-16T00:00:00Z), epoch seconds or epoch milliseconds, and must not be later than `until`",
    "trace_id": "hydra-18d5fdcf0b0c9154-13"
  }
}
```

请**按 `code` 分支**，不要匹配 `message`（文案可能改进）。完整 code 清单见 §6。

### 4.4 方法约定

**只读端点**方法固定：`whoami`/`usage`/`user-groups`/`users`/`users/{uid}/api-keys` 只认 `GET`；用错方法 → `405 method_not_allowed`。**写端点**（§5.7）按 `(方法, 路径)` 解析：`PUT`/`DELETE` 各走各的**路径形状**，形状对不上的组合 → `404 not_found`。保留前缀下**任何**不是这documented set路由的路径 → `404 not_found`（本 API 自己回答，**不会**落到上游）。

> **405 还是 404 的判定（2026-11-04 逐格实测，按当前 router）**：先看**路径**能不能对上那只读路由，再看方法 —— 对上了但方法不对就是 **405**；对不上任何 `(方法, 路径)` 组合就是 **404**。所以同一个"用错方法"会得到两种答案，取决于**该路径形状是否也是一条只读路由**：
>
> | 请求 | 结果 | 原因 |
> |---|---|---|
> | `POST /tenant/{tid}/api/v1/user-groups` | **405** | 该路径**也是**只读路由 `user-groups`（只认 `GET`）⇒ 方法检查先命中 |
> | `GET /tenant/{tid}/api/v1/user-groups/QQ` | **404** | `/user-groups/{id}` 这个**形状**只有写路由（`DELETE`），`GET` 对不上任何组合 |
> | `DELETE /tenant/{tid}/api/v1/user-groups` | **404** | 该路径的写路由是 `/user-groups/{id}`，没有"不带 id"的写形状 |
> | `GET /tenant/{tid}/api/v1/users/{uid}/billing` | **404** | `/users/{uid}/billing` 只有写形状（`PUT`) |
>
> 按 `code` 分支处理（两者都在 §6 里）不会踩坑，但别按 404/405 去猜"路径存不存在"。

### 4.5 幂等性

- 所有 `GET` 端点（`whoami` / `usage` / `user-groups` / `users` / `users/{uid}/api-keys`）天然幂等。
- 写端点（§5.7）同样**幂等**：重复 `PUT` 按自然键收敛到**同一行**（保留原 id，只更新字段到最新）；`DELETE` 按**不可变 id**，删一个已不存在的 id 是 no-op。所以"重试同一个写请求"是安全的。唯一例外：`POST .../api-keys` 每次调用**mint 一个新 key**（retries create duplicates — send it once).

### 4.6 就绪

节点刚启动、还没拿到配置快照时会返回 `503 not_ready`。这是**可重试**的（换一个节点、或稍等重试即可），不是你的请求有问题。

---

## 5. 端点

### 5.1 `GET /tenant/{tenant_id}/api/v1/whoami`

读你自己的非机密配置。**只读快照、不查库**，所以在任何节点（含边缘节点）都是瞬时的。

```json
{
  "tenant_id": "t_acme",
  "name": "Acme Inc.",
  "enabled": true,
  "domain": "api.acme.example",
  "config_version": 42,
  "base_url": "/tenant/t_acme/api/v1"
}
```

| 字段 | 说明 |
|---|---|
| `tenant_id` / `name` | 你的标识与名称 |
| `enabled` | **你当前是否被停用**。`false` 时你的客户端请求会被拒绝，但你**仍然可以使用本 API 的这些端点**（自救路径，见 §7.1） |
| `domain` | 你现在绑定的域名，即你的客户端实际请求的 `Host`。**只读**：要改找运维 |
| `config_version` | 本次鉴权所依据的**配置快照版本**。你刚让运维改了什么，可以轮询这个字段判断"改动是否已生效"，而不用猜 |
| `base_url` | 本 API 的前缀，即上表 `$HYDRA` 之后的那一段（拼上 `$HYDRA` 就是你实际用的地址） |

**不会返回**：令牌摘要、证书私钥、任何 provider key。

### 5.3 `GET /tenant/{tenant_id}/api/v1/usage`

查某个时间窗内的用量。数据来自**计量存储**（单节点部署读本地 SQLite；集群部署读共享 ClickHouse），所以是**持久、可重复查询**的账；集群下任何节点查到的都是同一份数据。

#### 请求

```
GET /tenant/{tenant_id}/api/v1/usage?since=2026-09-16T00:00:00Z&until=2026-09-17T00:00:00Z&group_by=model
```

| 参数 | 必填 | 默认 | 说明 |
|---|---|---|---|
| `since` | **是** | — | 窗口起点（**含**） |
| `until` | 否 | 服务端当前时刻 | 窗口终点（**不含**） |
| `group_by` | 否 | `none` | `none` \| `model` \| `provider` \| `user` \| `user_group` \| `api_key` \| `day`；其他值 → `400 invalid_group_by` |

**`since` 接受的写法**（一律按 UTC 理解，除自带偏移的）：

| 写法 | 例 |
|---|---|
| RFC3339（`Z` 或带偏移） | `2026-09-16T00:00:00Z`、`2026-09-16T08:00:00+08:00` |
| 无时区的 `T` 分隔 | `2026-09-16T00:00:00` |
| 空格分隔（历史遗留输入） | `2026-09-16 00:00:00` |
| epoch 秒 / 毫秒 | `1789516800`、`1789516800000` |

> 服务端会**归一化**后回显在响应里（见 `since`/`until` 字段）。**请以回显的值为准** —— 它才是真正被用来查询的边界。
> 非法写法不会被"猜测"成别的时间窗，而是 `400`。

#### 响应

```json
{
  "tenant_id": "t_acme",
  "since": "2026-09-16T00:00:00Z",
  "until": "2026-09-17T00:00:00Z",
  "as_of": "2026-09-16T23:59:57Z",
  "totals": {
    "requests": 1234,
    "tokens_in": 456789,
    "tokens_out": 12345,
    "cache_hit_tokens": 9999,
    "errors": 7
  },
  "rows": [
    { "key": "gpt-4o", "requests": 800, "tokens_in": 300000, "tokens_out": 9000, "cache_hit_tokens": 5000, "errors": 2 }
  ],
  "group_by": "model",
  "source": "clickhouse"
}
```

| 字段 | 说明 |
|---|---|
| `since` / `until` | **归一化后**的实际窗口（`until` 不含） |
| `as_of` | 你在**本次查询的窗口内**最新一条记录的写入时刻（即窗口内 `MAX(created_at)`）；窗口内没有任何记录时为 `null`。默认 `until=now` 时它恰好等于你在存储里的最新一条写入，查历史窗口时约等于窗口上界。用它判断"数据到齐了没有" |
| `totals` | 整个窗口的合计 |
| `rows` | 按 `group_by` 分组的行；`group_by=none` 时为空数组。`key` 是分组值（模型名 / provider id / **user id** / **user_group id** / **api_key id** / `YYYY-MM-DD`）。`group_by=user|user_group|api_key` 时，用量缺该归属维度（无 principal，或 principal 的维度为空）归入空字符串 `""` 分组 |
| `tokens_in` | 请求**发出**的 token（含命中缓存的） |
| `tokens_out` | 模型**返回**的 token |
| `cache_hit_tokens` | 命中提示词缓存的 token（是 `tokens_in` 的子集） |
| `errors` | HTTP 状态码 **≥400** 的请求数 |
| `source` | 本次数据来自哪个存储：`sqlite` \| `clickhouse` |

#### 语义边界（**请读完再拿去对账**）

1. **一行 = 一次成功选中了上游 provider 的请求。** 选路之前就失败的请求**不进表**。
2. **集群部署下 `requests` 是近似值。** 原因：写入 ClickHouse 超时后会重发整批，而 CH 表里没有可去重的唯一列，所以可能重复计数（`COUNT(*)` 偏高）。`tokens_*` 与 `errors` 由同一批行相加，同样可能偏高。**趋势与对账可以，计费不要直接用。**
3. **与运维看的"用量"必然对不上，这不是 bug。** 管理面 `/api/v1/stats/usage` 读的是**进程内存计数器**（重启归零、无时间维度），你这里读的是**持久计量行**。各自漏算不同：计数器丢"重启前"，持久行丢"选路前失败"。
4. **查用量不会产生用量。** 你调本 API 的行为不写进你自己的用量表，也不计入 `requests`。

#### 错误

```
400 invalid_since / invalid_until   写法非法，或 since > until
400 window_too_large                窗口超过上限（默认 31 天）
400 invalid_group_by                group_by 不在白名单
503 usage_store_unavailable         计量存储读不到，或返回的内容无法解析（重试 / 找运维）
```

> 窗口上限不是官僚规定：ClickHouse 表的排序键**以时间开头**，窗口越宽扫描的行越多（包括其他租户在该窗口内的行）。需要长跨度统计请分段查，或找运维评估。

> **结果过大会返回 `503 usage_store_unavailable`，且重试无效。** 当聚合结果过大（很宽的 `group_by` 叠加很长的窗口）超过网关的响应体上限时，读会失败并返回 `503 usage_store_unavailable`——这不是"重试就好"的故障：缩小 `since`/`until`，或降低 `group_by` 的基数（例如用 `group_by=day` 代替高基数的 `group_by=model`）才有效。

> ⚠ The old §5.2 `auth/cache/invalidate` and the §5.4/5.5/5.6 sub-tenant endpoints are REMOVED (T10 complete): calls to them answer `404 not_found` like any other unknown path.

---

### 5.7 The four-level user hierarchy: groups / users / api-keys / billing (2026-11-04, T10 complete)

These endpoints manage **your own** identity rows under `tenant → user_group → user → api_key`. Same access-token gate as everything else (401 / 403 tenant_id_mismatch / 429 / 503 not_ready / config_not_published apply identically). **The four-level rows you create here are exactly what the data plane authenticates against** — no `auth_url`, no auth cache.

#### 5.7.1 Groups

- `GET /user-groups` → `{ "user_groups":[ {id,tenant_id,name,enabled,created_at,updated_at}, … ] }` (incl. `enabled:false`).
- `PUT /user-groups` body `{"id?":"g…","name":"Internal","enabled":true}` — no `id` creates, `id` updates; an `id` of another tenant reads 404 `not_found`. Names are unique per tenant.
- `DELETE /user-groups/{id}` — deleting a group that still has users answers **409 group_not_empty** (schema keeps it referenced via `ON DELETE RESTRICT`); the explicit check and the DB race give the same answer.

#### 5.7.2 Users

- `GET /users` lists **all** your users, including disabled ones and those with `billing_status:"delinquent"`. `?group_id=` filters; a `group_id` that is not yours → 400 `invalid_group`.
- `PUT /users` body `{"id?":"u…","user_group_id":"g…","external_id?":"e…","name":"Alice","enabled":true}` — `user_group_id` is **required** (no group-less users); `external_id` unique per tenant (collision → 409 `duplicate_external_id`).
- `DELETE /users/{id}` — deleting a user **cascades** its api-keys away (`ON DELETE CASCADE`).

#### 5.7.3 Api-keys — mint once, store only the digest

- `GET /users/{uid}/api-keys` returns only non-secret info per key: `key_prefix`, `key_last4`, `enabled`, `expires_at`. **Never the plaintext** (only a SHA-256 digest is stored).
- `POST /users/{uid}/api-keys` body `{"name":"ci","expires_at":null|"2026-…"}`. The response carries the plaintext **exactly once**:

  ```json
  { "key":"hk_…","id":"ak…","key_prefix":"hk_ab12…","key_last4":"9f2c","expires_at":null,"config_version":N }
  ```

  - `name` empty → 400 `invalid_request`.
  - `expires_at` unparseable → 400 `invalid_request` (it is **never** silently treated as "no expiry").
  - At the per-tenant ceiling → 400 `hierarchy_limit_exceeded` (ceiling = `HYDRA_MAX_USER_API_KEYS_PER_TENANT`, default **1000**).
- `DELETE /users/{uid}/api-keys/{key_id}` revokes it: **immediately** — the next data-plane request with that key answers **401 invalid_api_key** (no TTL to wait out).

#### 5.7.4 Billing (`402` on the data plane)

`PUT /users/{uid}/billing` body `{"status":"active"|"delinquent","reason?":"…"}` (idempotent; unknown user → 404). A user set to `delinquent` answers **402 user_delinquent** on `/v1/*`, body:

```json
{"error":{"message":"user_delinquent","type":"insufficient_quota","code":"user_delinquent"}}
```

Back to `active` clears it. **This replaces the old `auth_url`-based "欠费停机" flow** for per-user billing.

#### 5.7.5 Data-plane truth table (what a `hk_` key hitting `/v1/*` gets)

| condition | answer |
|---|---|
| key unknown / revoked / expired / disabled | **401** `invalid_api_key` |
| user, group or tenant `enabled:false` | **403** |
| key live, user `billing_status:"delinquent"` | **402** `user_delinquent` (body above) |
| everything enabled & active | passes to the model |

Propagation of any of those flips: **on your node the next request**; other nodes within **≤5 s** (config publish + ~1 s replica materialise). There is **no auth-cache residual window any more** — see §7.4's rewrite.

#### 5.7.6 传输语义与对账（写端点，与运维手册 `ops.md` 同义）

- **写就在你连的那个节点上执行**（ADR-0001 D-6）：没有"转发到 leader"层，**任何节点都接受并执行租户写**，LB 指向哪一台都可以。入口节点用**你的令牌**鉴权、在**本地事务**里执行写核心，然后把自己的配置**提交到集群的权威提交点**（`head`)。
- **提交点是集群的 `head`，不是这个节点**，所以存在一种集群-only 失败：本地提交成功、但提交到 `head` 失败（集群**失去多数派**)⇒ **`503 config_not_published`**。这一台**已经改**了，别的节点看不到它——**先重读资源（同一个节点），等恢复多数后重发完全相同的请求**（幂等，且那次重发才是真正把配置发布出去的动作；没有后台补发)。读照常，分区时读继续。
- **`config_version` 是你的对账基线**：每次成功写（`200`/写响应带它，`404` 幂 `DELETE` 时无 body)；`whoami` 也带它。轮询从"你上次看到的版本"前进即可判断"我的改动是否已生效"。只在把 **`200` 的** `config_version` 当基线（`503 config_not_published` 时本节点的版本**领先**于集群）。

---

## 6. 错误码总表

| code | HTTP | 含义 | 可重试 |
|---|---|---|---|
| `unauthorized` | 401 | 令牌缺失 / 错误 / 该租户未配置令牌（三者文案相同） | 否（先检查令牌） |
| `tenant_id_mismatch` | 403 | 令牌归属与 URL 里的 `tenant_id` 不一致 | 否（检查 base URL） |
| `not_ready` | 503 | 该节点还没有配置快照 | **是** |
| `not_found` | 404 | 保留前缀下不是这documented set路由的路径；写端点上资源不存在 / 属于别的租户（§5.7） | 否（检查路径拼写 / id） |
| `method_not_allowed` | 405 | 方法用错 | 否 |
| `rate_limited` | 429 | 触发频率上限（见 §7.2），带 `Retry-After` | **是**（等 `Retry-After`） |
| `payload_too_large` | 413 | 请求体超过 1 MiB | 否 |
| `invalid_request` | 400 | 请求体不是合法 JSON，或读取失败 | 否 |
| `invalid_api_key` | 400 | 某个 key 超过 4096 字节 | 否 |
| `invalid_group_by` | 400 | `group_by` 不在白名单 | 否 |
| `invalid_since` | 400 | `since` 缺失 / 写法非法 / 晚于 `until` | 否 |
| `invalid_until` | 400 | `until` 写法非法 | 否 |
| `window_too_large` | 400 | 窗口超过上限（默认 31 天） | 否（缩小窗口） |
| `usage_store_unavailable` | 503 | 计量存储读不到或无法解析 | **是** |
| `config_not_published` | 503 | **写已在本节点提交，但没能提交到集群**（失去多数派；写端点，§5.7）。读照常 | **是**（**先重读、等多数派恢复后重发同一请求** —— 那次重发才是发布动作） |
| `invalid_group` | 400 | `?group_id=` filter, or a `PUT` user's `user_group_id`, names a group that is not one of yours (hierarchy endpoints, §5.7) | 否 (create/fix the group first) |
| `duplicate_name` | 409 | `PUT /user-groups`: another of your groups already uses that name (unique per tenant, §5.7) | 否 |
| `group_not_empty` | 409 | `DELETE /user-groups/{id}` while the group still has users (`ON DELETE RESTRICT`, §5.7) | 否 (move/delete the users first) |
| `duplicate_external_id` | 409 | `PUT /users`: another user of this tenant already carries that `external_id` (unique per tenant, §5.7) | 否 |
| `hierarchy_limit_exceeded` | 400 | A CREATE past a ceiling: `POST .../api-keys` past `HYDRA_MAX_USER_API_KEYS_PER_TENANT` (per tenant, default 1000) **or** `PUT .../users` past `HYDRA_MAX_USERS_PER_TENANT` (per tenant, default 1000) **or** any of `PUT .../user-groups` / `PUT .../users` / `POST .../api-keys` past `HYDRA_MAX_HIERARCHY_ROWS` (global over the five identity tables, default 3000) | 否 (delete a row first — an UPDATE of an existing row is never refused) |

---

## 7. 你需要知道的边界

### 7.1 被停用（欠费停机）之后还能做什么

`whoami`'s `enabled` becomes `false` ⇒ your **client requests** are rejected, but the tenant API stays fully usable (a test guards this). Deliberate: if the self-service path were also closed, you could never recover. Tenant-level `enabled` is ops-set via the admin API; **per-user 欠费 is now `billing` (§5.7)** — the old `auth_url`-expressed stop no longer gates auth.

典型恢复流程：

```bash
# 1) 确认被停用/欠费: whoami + the users' billing_status
curl -s $HYDRA/tenant/$TID/api/v1/whoami -H "Authorization: Bearer $TOK" | grep enabled
curl -s $HYDRA/tenant/$TID/api/v1/users -H "Authorization: Bearer $TOK" | grep billing_status

# 2) 恢复 a delinquent user: flip its billing back (§5.7) — this REPLACES
#    the old auth_url / invalidate dance
curl -s -X PUT $HYDRA/tenant/$TID/api/v1/users/$UID/billing \
  -H "Authorization: Bearer $TOK" -H "content-type: application/json" \
  -d '{"status":"active"}'

# 3) NO invalidate any more: the billing flip takes effect on the NEXT
#    request (local node immediately, other nodes ≤5 s via config publish).
#    A *tenant-level* disable (`enabled:false`) is still ops-set via the admin
#    API — this API cannot write tenant-level config.
```

#### 停用后你的**客户端**看到什么（2026-09-30 实测）

| 请求 | 响应 |
|---|---|
| 客户端请求（`/v1/*`）打到**被停用**的租户 | **403**，体 `{"error":{"message":"tenant_disabled","type":"proxy_error"}}` |
| key of a `delinquent` user | **402**, body `{"error":{"message":"user_delinquent","type":"insufficient_quota","code":"user_delinquent"}}` |
| same tenant's **tenant API** (rest of this doc) | fully usable (every route in §1's table, incl. the write endpoints) |

> ⚠ **The data-plane `/v1/*` error envelope is NOT the §4.3 tenant-API envelope.** Two shapes live there:
> - `short_circuit` / `respond_auth_denial` (`proxy.rs:1847` / `:1856`) send only `message` (reason string) and
>   `type` (`proxy_error` / `auth_error`) — **no `code`, no `trace_id`**. Measured:
>   `401 missing_api_key`, `403 model_not_allowed`, `404 unknown_domain`, `403 tenant_disabled`, and (2026-11-03)
>   **`401 invalid_api_key`** for an unknown / revoked / expired client key (§5.7 truth table).
> - The delinquency gate is hand-built and DOES carry a `code`: `402` with body
>   `{"error":{"message":"user_delinquent","type":"insufficient_quota","code":"user_delinquent"}}`.
> So on the data plane branch by **HTTP status + `error.message`**, and treat `error.code` as present ONLY for
> `user_delinquent` there. (`error.code` as a stable identifier is the TENANT-API contract — §4.3/§6.)

### 7.2 限流与锁定

| 维度 | 默认上限 | 说明 |
|---|---|---|
| 每租户**已授权**请求 | 60 次/分钟 | 防"把本 API 当免费 CPU/DB" |
| 每**来源 IP** 鉴权失败 | 10 次/分钟 | 防一个来源猜令牌 |
| 每**令牌摘要**鉴权失败 | 10 次/分钟 | 防一个令牌被多来源试探 |
| 持续超限后的锁定 | 900 秒 | 被锁定的维度对**失败鉴权**返回 **`429`（而不是 401）**；持有效令牌的请求不受锁定影响 |

几个要点：

- **"已授权"= 节点接受的、属于你的已认证请求**：凡通过令牌校验且 URL 租户匹配的请求都计数，**包括随后被 API 以 4xx/5xx 拒绝的**（如 `400 invalid_since`、`413 payload_too_large`、`503 usage_store_unavailable`）；**唯一不计数的是 `403`（URL 写错租户）**。
- **失败预算与已授权预算是独立的**：别人拿错令牌试探**不会**消耗你正常调用的额度。
- **失败锁定只针对"鉴权失败"**：拿**对**令牌的请求**不会**被它拒。锁定的作用是把"持续猜错令牌"的来源在窗口内拦在 `429` 上；持有效令牌的正常调用不受影响。锁定期满自动恢复。
- **`403`（URL 写错租户）不消耗已授权额度**：否则一个配错 base URL 的客户端会把整个租户锁在自己的 API 之外。
- 限额是**每节点**计算的，所以 N 个节点的集群整体上限约为 N 倍。运维可以调（见 `ops.md` §1.1）。

### 7.3 Why there is no cache to clear

There is **no auth cache on the data plane at all**: client-key auth matches stored `user_api_key` rows (§5.7), so there is nothing to "clear", and the old `auth/cache/invalidate` endpoint is REMOVED (a call to it is a plain `404 not_found`). Do the real action via §5.7: `DELETE .../api-keys/{key_id}` (revoke) or `PUT .../billing` (delinquency) — both take effect on the **next** request.

### 7.4 How fast does a revoke / ban take effect

**There is no auth-cache residual window any more.** A revoke, a disable, or a delinquency flip is decided by published config, not by a TTL:

- **your node**: the **next** request already sees it;
- **other nodes**: within **≤5 s** (config publish + ~1 s replica materialise).

To be sure it landed everywhere, compare `config_version` via `whoami` and use the write endpoints' transport semantics (§5.7.6) — `503 config_not_published` is the only "the cluster has NOT got it yet" answer. A persistently `lagging` node is an ops issue (ops has matching alerts).

### 7.5 用量口径

- 契约里**没有** `tenant_id` 查询参数：多传会被**忽略**，身份只认令牌。这是防止越权的设计（你无法查别人的用量）。
- `requests` 在 ClickHouse 部署下是**近似值**（见 §5.3 边界 2）。
- 计量存储的保留期由运维决定，超出保留期的历史查不到，会表现为"该窗口没有记录"（`as_of: null`），而不是报错。

---

## 8. 排障

| 现象 | 可能原因 | 怎么办 |
|---|---|---|
| `401 unauthorized` | 令牌写错/被轮换/该租户没配令牌 | 核对令牌；确认运维没给你轮换过 |
| `403 tenant_id_mismatch` | URL 里的 `tenant_id` 和令牌不是同一个租户 | 用 `whoami` 返回的 `tenant_id` 拼 URL（`base_url` 字段直接给了前缀） |
| `404 not_found` | 路径拼错（例如少了 `api/v1`，或路径不在这documented set之内，**包括 old auth-cache / sub-tenant paths**）；写端点上资源不存在 / 属于别的租户（§5.7） | 对照 §1 的路径表 |
| `405 method_not_allowed` | 方法用错（例如 `POST` 调 `user-groups` list） | `GET` 读端点；`PUT`/`POST`/`DELETE` 按 §5.7 |
| `429 rate_limited` | 触发限流或锁定 | 读 `Retry-After` 再重试。持**有效令牌**时，429 只会来自每租户**已授权**频率（60/分钟）上限；**失败**锁定（按来源 IP / 令牌摘要）只拦**错误**令牌的来源，不会拦对令牌 |
| `503 not_ready` | 该节点还没物化配置（启动窗口） | 重试（会落到其他节点） |
| `503 config_not_published` | 集群**失去多数派**：写已落在这台节点的库上，但没能提交到集群 | **不是你的请求有问题**。等集群恢复后**重发完全相同的请求**（幂等；那次重发才是把改动发布出去的动作）。需要确认时用**同一个节点**重读资源 |
| `503 usage_store_unavailable` | 计量存储读不到或返回无法解析 | 重试；持续失败把 `X-Hydra-Trace-Id` 给运维 |
| client answers 402 `user_delinquent` | that user's `billing_status` is `delinquent` (§5.7) | intended gate; restore with `PUT .../users/{uid}/billing {"status":"active"}` |
| key still passes after you "revoked" it | you hit a `lagging` node, or the revoke write answered `503 config_not_published` | re-read on the same node, re-send the write (§5.7.6); check `config_version` |

**报障时请附上**：`X-Hydra-Trace-Id`（或响应体里的 `error.trace_id`）、请求的完整 URL（令牌请打码）、时间点。

---

## 9. 变更策略

- 本 API 的**路径、字段名、错误码**是对外契约。新增字段是兼容变更（请忽略你不认识的字段，不要严格校验 JSON）；删字段/改语义会另行通知。
- **`code` 是稳定标识，`message` 不是**。请按 `code` 分支。
- 时间一律 UTC，且响应里回显的是归一化后的规范形态（`YYYY-MM-DDTHH:MM:SSZ`）。请按字符串/时间类型解析，不要依赖服务端本地时区。

---

## 附录：一页速查

```
GET  {HYDRA}/tenant/{TID}/api/v1/whoami
GET  {HYDRA}/tenant/{TID}/api/v1/usage?since=<必填>[&until=][&group_by=none|model|provider|user|user_group|api_key|day]

GET  {HYDRA}/tenant/{TID}/api/v1/user-groups                 (list)
PUT    {HYDRA}/tenant/{TID}/api/v1/user-groups                     body {id?,name,enabled?}   (upsert)
DELETE {HYDRA}/tenant/{TID}/api/v1/user-groups/{id}                 (409 group_not_empty if it still has users)
GET  {HYDRA}/tenant/{TID}/api/v1/users[?group_id=]                (list; filter must be YOURS)
PUT    {HYDRA}/tenant/{TID}/api/v1/users                            body {id?,user_group_id,external_id?,name,enabled?}
DELETE {HYDRA}/tenant/{TID}/api/v1/users/{id}                     (cascades its api-keys)
GET  {HYDRA}/tenant/{TID}/api/v1/users/{uid}/api-keys             (prefix/last4 only, never plaintext)
POST   {HYDRA}/tenant/{TID}/api/v1/users/{uid}/api-keys            body {name,expires_at?}    (plaintext shown ONCE; ceiling 1000 by default)
DELETE {HYDRA}/tenant/{TID}/api/v1/users/{uid}/api-keys/{key_id}  (revoke; next request 401)
PUT    {HYDRA}/tenant/{TID}/api/v1/users/{uid}/billing             body {status:"active"|"delinquent",reason?} (delinquent => 402 on /v1/*)

Header: Authorization: Bearer <tenant access token>
Trace:  响应头 X-Hydra-Trace-Id，报障必带

E2 判定顺序：先看 HTTP，再看 fleet.state
  200 applied/single_node = 完成 ｜ 202 pending = 未确认（可重试）｜ 503 unavailable = 集群没收到
E3 判定：as_of 为 null 表示窗口内无记录（不是报错）；集群下 requests 是近似值
写端点判定：200 = 已写（带 config_version）｜ 204 = 删除（幂等）｜ 503 config_not_published = 已本地写、未集群提交：重读（`whoami` 同节点）再重发（§5.7.6）
