# Hydra 租户自助 API 对接文档

> **给谁看**：接入 Hydra 的**租户**（及其研发/运维）。这是对外契约，可以照此写代码。
> **不给谁看**：Hydra 自身的实现细节见 `design-tenant-api.md`（内部设计）与 `ops.md`（运维手册）。
>
> 本文的每一条都对应仓库里可运行的代码/测试；凡是"你会看到的行为"，都有测试守着。若本文与线上行为不一致，那是**缺陷**，请找运维报。

---

> **⚠ 2026-11-03 重大契约变化（read this first).** 数据面 client-key **鉴权是 now INTERNAL**:Hydra 把 your client's api-key 对照 its **stored `user_api_key` 里 rows** 下 里 the four-level hierarchy `tenant → user_group → user → api_key` — **no `auth_url` is ever consulted any more**, and the auth cache is gone. Consequences:
> - **§5.2 `auth/cache/invalidate` and §5.4/5.5/5.6 sub-tenant endpoints are RETIRING** — the code still **serves** them (the T10 deletion batch is deferred), but the auth cache they touch is **no longer consulted**, so "invalidate" no longer changes client-key outcomes (revoke / disable / delinquency now flip **immediately**: local node next-request, other nodes ≤5 s via config publish). Do not build new flows on them.
> - **New self-service endpoints (§5.7)** manage your groups / users / api-keys / billing.
> - `whoami` STILL echoes `auth_url` (column still NOT NULL at write time, migration `0015` deferred) **but nothing uses it for auth**.
> - Client api-keys (`hk_…`) and tenant access tokens (`ht_…`) are **hydra-minted, plaintext shown exactly once**; a tenant may hold **multiple** access tokens.
> - A **delinquent** user's key answers **402 `user_delinquent`** on the data plane (body `{"error":{"message":"user_delinquent","type":"insufficient_quota","code":"user_delinquent"}}`).

---

## 1. 你能用它做什么

Current endpoints (2026-11-03) — **ACTIVE** = the internal-auth hierarchy (incl. the new §5.7 groups/users/keys/billing) **+ RETIRING** = auth-cache + sub-tenant (still served, pending the deferred T10)，都跑在**你已经在用的那个数据面地址**上（客户端调 `/v1/*` 的同一个域名/端口），因此不需要运维额外为你开放管理口。

| 端点 | 方法 | 用途 |
|---|---|---|
| `/tenant/{tenant_id}/api/v1/whoami` | `GET` | read your config: domain, `enabled`, `config_version` (`auth_url` is STILL echoed but **no longer used for auth**) |
| `/tenant/{tenant_id}/api/v1/auth/cache/invalidate` | `POST` | **RETIRING** — still served, but the auth cache is **no longer consulted** on the data plane, so it no longer changes client-key outcomes (§5.2; revoke/delinquency are now immediate via §5.7) |
| `/tenant/{tenant_id}/api/v1/usage` | `GET` | 查某个时间窗内你实际产生了多少请求/token |
| `/tenant/{tenant_id}/api/v1/user-groups` | `GET` · `PUT` · `DELETE .../{id}` | list / upsert / delete your user-groups (§5.7; non-empty delete ⇒ 409) |
| `/tenant/{tenant_id}/api/v1/users` | `GET ?group_id=` · `PUT` · `DELETE .../{id}` | list / upsert / delete your users (`user_group_id` required; delete cascades keys) (§5.7) |
| `/tenant/{tenant_id}/api/v1/users/{uid}/api-keys` | `GET` · `POST` · `DELETE .../{key_id}` | list / **mint (plaintext `hk_` shown ONCE)** / revoke a key (§5.7) |
| `/tenant/{tenant_id}/api/v1/users/{uid}/billing` | `PUT` | `active` / `delinquent` (delinquent ⇒ **402** on `/v1/*`) (§5.7) |
| `/tenant/{tenant_id}/api/v1/sub-tenants` | `GET` | **RETIRING** — 只读列出**你自己且启用中的**子租户（前缀、启用状态、配置版本；已停用的不返回） |
| `/tenant/{tenant_id}/api/v1/sub-tenant-routes` | `GET` | **RETIRING** — 只读列出**你自己**的子租户路由（`model → provider`、默认路由、配置版本） |
| `/tenant/{tenant_id}/api/v1/sub-tenants/{name}` | `PUT` | **RETIRING** — **自助创建/更新**一个子租户（按名字 upsert）；见 §5.6 |
| `/tenant/{tenant_id}/api/v1/sub-tenants/{id}` | `DELETE` | **RETIRING** — **自助删除**一个子租户（按不可变 id，幂等）；见 §5.6 |
| `/tenant/{tenant_id}/api/v1/sub-tenant-routes` | `PUT` | **RETIRING** — **自助创建/更新**一条子租户路由（按 `sub_tenant_id`+`model_key` upsert）；见 §5.6 |
| `/tenant/{tenant_id}/api/v1/sub-tenant-routes/{id}` | `DELETE` | **RETIRING** — **自助删除**一条子租户路由（按不可变 id，幂等）；见 §5.6 |

**明确没有的能力**（不是漏做，是刻意不做）：

- **不能改你的租户级配置**。域名、`auth_url`、证书、是否启用，都由运维通过管理 API 设置（本 API 没有这些的写路径，所以没有"字段级越权"的可能）。**Your own groups / users / api-keys / billing are the writable part** (§5.7) — always scoped to only your own resources; **Global Role scheduling is operator-only** (admin `/api/v1/global-roles`), a tenant never repoints providers. The old §5.6 sub-tenant writes are RETIRING.
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

# ② 我充值了，让所有客户端立刻恢复（清空本租户缓存）
curl -s -X POST $HYDRA/tenant/$TID/api/v1/auth/cache/invalidate \
  -H "Authorization: Bearer $TOK" -H "content-type: application/json" -d '{}'

# ③ 我昨天用了多少
curl -s "$HYDRA/tenant/$TID/api/v1/usage?since=2026-09-16T00:00:00Z&until=2026-09-17T00:00:00Z" \
  -H "Authorization: Bearer $TOK"
```

---

## 3. 怎么拿到令牌

1. 令牌由**运维**在你的租户上配置（管理 UI 的 Tenants 表单里的 `Access token`，或管理 API）。
2. 服务端只存 **SHA-256 摘要**，**永远不会回显明文**。所以**丢了只能让运维轮换**（换一个新值即完成轮换）。
3. 最短 16 个字符。建议 `openssl rand -hex 32` 生成。
4. 这是**你的**凭证，不是客户端的 api-key：
    - 它放在 `Authorization: Bearer` 里，只用于调用本文的九个端点（5 个只读 + 4 个写）；
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

**只读端点**方法固定：`whoami`/`usage`/`sub-tenants`/`sub-tenant-routes` 只认 `GET`、`invalidate` 只认 `POST`；用错方法 → `405 method_not_allowed`。**写端点**（§5.6）按 `(方法, 路径)` 解析：`PUT`/`DELETE` 各走各的**路径形状**，形状对不上的组合 → `404 not_found`。保留前缀下**任何**不是这documented set路由的路径 → `404 not_found`（本 API 自己回答，**不会**落到上游）。

> **405 还是 404 的判定（2026-09-30 逐格实测，此前这一段的例子是错的）**：先看**路径**能不能对上那五条只读路由，再看方法 —— 对上了但方法不对就是 **405**；对不上任何 `(方法, 路径)` 组合就是 **404**。所以同一个"用错方法"会得到两种答案，取决于**该路径形状是否也是一条只读路由**：
>
> | 请求 | 结果 | 原因 |
> |---|---|---|
> | `POST /tenant/{tid}/api/v1/sub-tenant-routes` | **405** | 该路径**也是**只读路由 `sub-tenant-routes`（只认 `GET`）⇒ 方法检查先命中 |
> | `GET /tenant/{tid}/api/v1/sub-tenants/QQ` | **404** | `/sub-tenants/{name}` 这个**形状**只有写路由（`PUT`），`GET` 对不上任何组合 |
> | `PUT` / `DELETE /tenant/{tid}/api/v1/sub-tenants` | **404** | 该路径的写路由是 `/sub-tenants/{name}`，没有"不带 name"的写形状 |
> | `DELETE /tenant/{tid}/api/v1/sub-tenant-routes` | **404** | 删除的写形状是 `/sub-tenant-routes/{id}` |
> | `GET /tenant/{tid}/api/v1/auth/cache/invalidate` | **405** | 该路径是只读路由，只认 `POST` |
>
> 换句话说：**旧文档里"`GET`/`POST` 打到写路径一律 404"的例子是错的** —— `POST .../sub-tenant-routes` 恰好打在一条**同时是只读路由**的路径上，实测是 **405**。按 `code` 分支处理（两者都在 §6 里）不会踩坑，但别按 404/405 去猜"路径存不存在"。

### 4.5 幂等性

- 所有 `GET` 端点（`whoami` / `usage` / `sub-tenants` / `sub-tenant-routes`）天然幂等。
- `POST auth/cache/invalidate` **幂等**：重复调用只是重复"清掉本来就已经没有的东西"。所以重试安全。
- `invalidated` 是**本节点实际删掉的条数**，重复调用会变 0，这**不是**失败：那些 key 下次请求本来就会回源。
- 写端点（§5.6）同样**幂等**：重复 `PUT` 按自然键收敛到**同一行**（保留原 id，只更新字段到最新）；`DELETE` 按**不可变 id**，删一个已不存在的 id 是 no-op。所以"重试同一个写请求"是安全的。

### 4.6 就绪

节点刚启动、还没拿到配置快照时会返回 `503 not_ready`。这是**可重试**的（换一个节点、或稍等重试即可），不是你的请求有问题。

---

## 5. 端点

### 5.1 `GET /tenant/{tenant_id}/api/v1/whoami`

读你自己的非机密配置。**只读快照、不查库、不访问你的 `auth_url`**，所以在任何节点（含边缘节点）都是瞬时的。

```json
{
  "tenant_id": "t_acme",
  "name": "Acme Inc.",
  "enabled": true,
  "domain": "api.acme.example",
  "auth_url": "https://auth.acme.example/verify",
  "config_version": 42,
  "base_url": "/tenant/t_acme/api/v1"
}
```

| 字段 | 说明 |
|---|---|
| `tenant_id` / `name` | 你的标识与名称 |
| `enabled` | **你当前是否被停用**。`false` 时你的客户端请求会被拒绝，但你**仍然可以使用本 API 的这些端点**（自救路径，见 §7.1） |
| `domain` | 你现在绑定的域名，即你的客户端实际请求的 `Host`。**只读**：要改找运维 |
| `auth_url` | Hydra 把你的客户端 api-key 发到哪里校验。**只读**：要改找运维 |
| `config_version` | 本次鉴权所依据的**配置快照版本**。你刚让运维改了什么，可以轮询这个字段判断"改动是否已生效"，而不用猜 |
| `base_url` | 本 API 的前缀，即上表 `$HYDRA` 之后的那一段（拼上 `$HYDRA` 就是你实际用的地址） |

**不会返回**：令牌摘要、证书私钥、任何 provider key。

### 5.2 `POST /tenant/{tenant_id}/api/v1/auth/cache/invalidate`

> ⚠ **RETIRING (2026-11-03).** The endpoint is still served and answers like before, but **client-key auth no longer consults an auth cache at all**, so invalidating changes nothing about `/v1/*` outcomes. Revoke with `DELETE …/users/{uid}/api-keys/{key_id}` and set delinquency with `PUT …/users/{uid}/billing` (§5.7) — both flip on the **next** request. This section stays until the deferred T10 deletion batch; the text below describes **historical** behaviour.

**HISTORICAL (pre-2026-11-03).** Hydra 会缓存"某个客户端 key 的鉴权结论"（默认 allow 5 分钟 / deny 30 秒），所以你在自己系统里封禁一个 key 之后，Hydra 侧可能还有几分钟在放行。调它即可让结论立刻失效、下次请求强制回你的 `auth_url` 重新判定。

#### 请求

```http
POST /tenant/{tenant_id}/api/v1/auth/cache/invalidate?wait=converged&timeout_ms=2000
Authorization: Bearer <token>
Content-Type: application/json

{ "api_keys": ["sk-live-aaa", "sk-live-bbb"] }
```

- 请求体**可省略**（或 `{}` / `{"api_keys":[]}`）→ **清空本租户全部缓存项**。
- 给了 `api_keys` → 只清这些 key。

| 查询参数 | 取值 | 默认 | 说明 |
|---|---|---|---|
| `wait` | `converged` \| `none` | `converged` | `none` = 发出去就立刻返回 `202`，不等集群确认（适合批量脚本）。给了别的值 → `400 invalid_wait`。**例外：没有 `cluster-redis` 特性的构建**里不存在 fleet，本节点自己清完就是全部答案 ⇒ 此时**无视** `wait` 与 `timeout_ms`，一律返回 `200 single_node` |
| `timeout_ms` | `1`..`60000` | 服务端配置（默认 2000） | 本次请求等待全集群确认的预算。越界/非数字 → `400 invalid_timeout_ms` |

> 这两个参数**不是同等强度地"校验"**，别以为它们对称：
> - `timeout_ms` 会先 `trim()` 再 `parse::<u64>()`（`crates/hydra-server/src/tenant_api/handlers.rs`，`invalidate()` 里的 `param("timeout_ms")` 分支），所以 `?timeout_ms=%202000%20`（带尾随空格）**被接受**；
> - `wait` 是**逐字节比较**（同函数的 `match param("wait")`：`None | Some("converged")` / `Some("none")` / `Some(other)`），所以 `?wait=none%20` 是 **400 `invalid_wait`**，不会被当成 `none`。
>
> 百分号解码本身是正常的（`tenant_api::time_bound::query_params` 先 `percent_decode` 再取值），所以 `%20` 确实变成了那个空格 —— 差别只在**比较前有没有 `trim()`**。
>
> 也就是说：写错了**不会**静默按默认值执行（这点是真的），但"能容忍空白"这件事只对 `timeout_ms` 成立。

#### 为什么要"等集群确认"

你的数据面通常有多个节点。如果你只清了一个节点的缓存，另一个节点还会继续放行被停用的 key。所以成功清除是**三件事**：本节点内存清掉 + 共享缓存清掉 + **等所有存活节点都确认清掉**。

#### 响应

```json
{
  "invalidated": 2,
  "checked": 2,
  "tenant_id": "t_acme",
  "scope": "keys",
  "fleet": {
    "state": "applied",
    "nodes_total": 3,
    "nodes_applied": 3,
    "lagging": [],
    "event_id": "1737-0",
    "waited_ms": 41
  }
}
```

| 字段 | 说明 |
|---|---|
| `invalidated` | **本节点**实际删掉的条数。不是集群总数 |
| `checked` | 你这次点了几个 key（清空整个租户时为 0）。`invalidated == 0 且 checked > 0` = "这些 key 在本节点本来就没缓存"，**不是失败** |
| `scope` | `keys`（按 key 清）或 `tenant`（清空） |
| `fleet.state` | 见下表 —— **这才是"到底清干净没有"** |
| `fleet.nodes_applied` / `nodes_total` | 已确认 / 存活节点数 |
| `fleet.lagging` | **未确认的节点名 —— 一律读作"没确认"，绝不要读作"已证实落后"**。三种来源：① `state=pending` 且确实检查过节点、发现它们的水位落后；② `wait=none`：**没人被检查**，此处就是**全部存活节点**（服务端自己的集群测试就是这么断言的：*"nobody was checked, so nobody confirmed"*）；③ `nodes_total=0` 的 `pending`（枚举不出 fleet）时才为空。运维可以直接拿它去查 |
| `fleet.event_id` | 本次失效在内部总线上的事件 ID。用 `wait=none` 时拿它做后续对账。**可为 `null`**：`single_node`（无总线）与 **publish 失败**的 `unavailable`（什么都没入队）都是 `null` —— 此时没有可对账的 ID |
| `fleet.waited_ms` | 本次真正等待的毫秒数。**`single_node` 路径上恒为 `0`**（硬编码，不走计时），别把它当"等了多久"的度量 |

#### 状态码与 `fleet.state` 的对应（**必须按这个判断，不要只看 HTTP 200**）

| HTTP | `fleet.state` | 含义 | 你该做什么 |
|---|---|---|---|
| `200` | `applied` | **所有存活节点都已生效** | 完成 |
| `200` | `single_node` | 本节点就是全部数据面（单节点部署） | 完成 |
| `202` | `pending` | 已发布，但**没有全部确认** | **不是错误，也还不是"完成"**。可重试（幂等），或把 `lagging` 交给运维。`lagging` 的语义是**"没确认的节点"**，不是"已证实落后"：用 `wait=none` 时它等于**全部存活节点**（没人被检查），**不代表**那些节点落后。若 `nodes_total=0`，说明本节点此刻**枚举不出存活节点集**（启动窗口 / 注册表行过期），此时 `lagging` 为空、与"有具体未确认节点"不是一回事 —— 请重试或交运维 |
| `503` | `unavailable` | 失效通道存在但**不可用**（**发布**或**水位读取**失败） | **集群没有被通知**。必须重试或找运维。响应体仍是上面的正常结构，**没有** `error.code`。注意两条 `unavailable` 路径的字段不同：**水位读取失败**时 `lagging` = 全部存活节点、`event_id` 有值（事件已入队但读不到水位）；**发布失败**时 `lagging` 为空、`event_id` 为 `null`（什么都没入队） |

> **没有 `cluster-redis` 特性的构建（真正的单节点）：`wait` / `timeout_ms` 完全不生效。**
> 此时根本没有 fleet，本节点自己清完就是全部答案，实现直接返回 **`200` + `single_node`**
> （无论 `wait=none` 还是 `timeout_ms=1`）—— 所以"`wait=none` ⇒ 立刻 `202`"这条**只对带
> `cluster-redis` 的构建成立**。拿 `202`/`event_id` 做对账的脚本请在 `single_node` 下退回
> "本地已完成"分支。判据是响应里的 `fleet.state`，不是你有没有传 `wait`。

#### 请求限制

| 限制 | 值 | 超限 |
|---|---|---|
| 单次请求的 key 数 | 1000 | `400 too_many_keys` |
| 单个 key 长度 | 4096 字节 | `400 invalid_api_key` |
| 请求体大小 | 1 MiB | `413 payload_too_large` |
| 每租户调用频率 | 10 次/分钟 | `429 rate_limited` + `Retry-After` |

> 频率上限是必要的：每次失效都会让所有节点回源你的 `auth_url`，也会在内部总线上扇出。**一个租户的凭证不该能消耗其他租户的可用性。**

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

### 5.4 `GET /tenant/{tenant_id}/api/v1/sub-tenants`
> ⚠ **RETIRING** (still served, T10 deferred): prefix routing is superseded by Global Role; do not build new flows here.

只读列出**你自己**的子租户。**只读快照、不查库、不访问你的 `auth_url`**，所以在任何节点（含边缘节点）都是瞬时的。只返回 `tenant_id` 与你一致的行——你**永远看不到**别的租户的子租户。

**只返回启用中的子租户**（快照本身只载入 enabled 行；已停用的子租户及其路由不会出现在本视图）。如需审计停用的配置，请联系运维用管理 API（`/api/v1/sub-tenants`）查看。

```json
{
  "config_version": 42,
  "sub_tenants": [
    { "id": "st_acme_1", "tenant_id": "t_acme", "name": "Acme 内部",
      "key_prefix": "QQCX_", "enabled": true,
      "created_at": "2026-09-18T08:00:00Z", "updated_at": "2026-09-18T08:00:00Z" }
  ]
}
```

| 字段 | 说明 |
|---|---|
| `config_version` | 本次读取所依据的**配置快照版本**（与 `whoami` 同义，可用来判断你的改动是否已生效） |
| `sub_tenants` | 你本租户**启用中**的子租户，按配置顺序排列 |
| `sub_tenants[].key_prefix` | 该子租户的 client api-key 前缀（**路由选择器**，不是机密，见 §7.6） |
| `sub_tenants[].enabled` | 该子租户是否启用 |

### 5.5 `GET /tenant/{tenant_id}/api/v1/sub-tenant-routes`

只读列出**你自己**的子租户路由（`model → provider`）。**只读快照、不查库**，任何节点（含边缘）瞬时。路由行只带 `sub_tenant_id`，租户作用域是**经其子租户的 `tenant_id` 间接**判定的——别人的子租户下的路由**永远**不会出现。

```json
{
  "config_version": 42,
  "sub_tenant_routes": [
    { "id": "str_acme_1", "sub_tenant_id": "st_acme_1", "model_key": null,
      "provider_id": "p_acme", "enabled": true,
      "created_at": "2026-09-18T08:00:00Z", "updated_at": "2026-09-18T08:00:00Z" }
  ]
}
```

| 字段 | 说明 |
|---|---|
| `config_version` | 本次读取所依据的**配置快照版本** |
| `sub_tenant_routes` | 你本租户子租户**启用中**的路由，按配置顺序排列 |
| `sub_tenant_routes[].model_key` | 非空 = 该 model 专属路由；`null` = 该子租户**默认路由**（两级） |
| `sub_tenant_routes[].provider_id` | 命中该前缀 + 该 model（或默认）时收窄到的 provider |

> 这两个端点是**只读**的：要创建/修改/删除子租户或路由，用你自己的**写端点**（§5.6）；运维也可通过管理 API（`/api/v1/sub-tenants`、`/api/v1/sub-tenant-routes`）代管，见 `ops.md` §5.5。

### 5.6 子租户写端点（v2：自助创建 / 修改 / 删除）
> ⚠ **RETIRING** (still served, T10 deferred): see the top banner and §7.6.

v2 起，你可以**自助管理自己的子租户与其路由**，不用再等运维。四个写端点都挂在**你已在用的那个数据面地址**的保留前缀下（和只读端点同一个域名/端口），**同样只认你的租户访问令牌**（`Authorization: Bearer`，与只读端点同一道闸门——写端点不会成为更弱的门）：

| 端点 | 方法 | 用途 |
|---|---|---|
| `/tenant/{tenant_id}/api/v1/sub-tenants/{name}` | `PUT` | **按名字 upsert** 一个子租户（`name` 在 URL 里）。body `{key_prefix?, enabled}`；`key_prefix` 省略 ⇒ 平台自动生成一个（8 位 `[A-Z0-9]` + `_` 分隔符） |
| `/tenant/{tenant_id}/api/v1/sub-tenants/{id}` | `DELETE` | **按不可变 id 删除** 一个子租户（幂等） |
| `/tenant/{tenant_id}/api/v1/sub-tenant-routes` | `PUT` | **按 `(sub_tenant_id, model_key)` upsert** 一条路由（`model_key` 省略/`null` = 该子租户**默认路由**）。body `{sub_tenant_id, model_key?, provider_id, enabled}` |
| `/tenant/{tenant_id}/api/v1/sub-tenant-routes/{id}` | `DELETE` | **按不可变 id 删除** 一条路由（幂等） |

#### 请求与响应

```http
# ① 创建 / 更新一个子租户（按名字 upsert）
PUT /tenant/{tenant_id}/api/v1/sub-tenants/QQ
Authorization: Bearer <token>
Content-Type: application/json

{ "key_prefix": "QQCX_", "enabled": true }
# key_prefix 可省略（自动生成）；enabled 默认 true；tenant_id 与 name 都来自 URL，不放 body

→ 200
{ "sub_tenant": { "id":"st_…", "tenant_id":"t_acme", "name":"QQ",
                  "key_prefix":"QQCX_", "enabled":true,
                  "created_at":"…", "updated_at":"…" },
  "config_version": 43 }

# ② 写一条路由（按 sub_tenant_id + model_key upsert；model_key 省略 = 默认路由）
PUT /tenant/{tenant_id}/api/v1/sub-tenant-routes
Authorization: Bearer <token>
Content-Type: application/json

{ "sub_tenant_id": "st_…", "provider_id": "p_acme", "model_key": null, "enabled": true }

→ 200
{ "route": { "id":"str_…", "sub_tenant_id":"st_…", "model_key":null,
             "provider_id":"p_acme", "enabled":true,
             "created_at":"…", "updated_at":"…" },
  "config_version": 44 }

# ③ 删除（按不可变 id，幂等）
DELETE /tenant/{tenant_id}/api/v1/sub-tenants/{id}        → 204
DELETE /tenant/{tenant_id}/api/v1/sub-tenant-routes/{id}  → 204
```

`200` 的响应体带被写的那一行（`sub_tenant` / `route`）与它**已生效于**的 `config_version`。`204` 无 body。

#### 幂等性（**重要**）

- **重复 `PUT` 收敛到同一行**：子租户按 `(tenant_id, name)`、路由按 `(sub_tenant_id, model_key)` upsert；同一自然键的重复 `PUT` **保留原行的不可变 id**，只把字段更新到最新值。所以"重试同一个 PUT"是安全的。
- **`DELETE` 按不可变 id，幂等**：删除一个**已不存在**的 id 是 no-op（`204`），不会因为"名字撞了"而误删你后来新建的同名资源。

#### 重试与对账（`config_version`）

- **写已落本节点、但没能发布到集群（`503 config_not_published`）**：见下"传输语义"。集群**没有多数派**时，你的写会在入口节点**本地提交成功**（这一份配置该节点已经在用），但**无法提交到集群的权威提交点**，因此**别的节点永远不会看到它**。这时 API 明确告诉你这件事，而不是回一个 `200` 让你以为全集群都生效了。**怎么办**：① 先**重读资源**（`GET sub-tenants` / `sub-tenant-routes`，用**同一个节点**）确认本地状态；② 等集群恢复多数派后**重发完全相同的请求**——写端点是 upsert/DELETE，**幂等**，而**那次重发才是真正把配置发布出去的动作**（没有后台补发）。响应里的 `config_version` **只在 `200` 时**可以作为对账基线。
- **`config_version` 是你的对账基线**：每次成功写（`200`）的响应带 `config_version`；`whoami` 与两个只读列表端点也带它。轮询它从"你上次看到的版本"前进，即可判断"我的改动是否已生效"，不用猜。

#### 传输语义（为什么你看到的是"正常响应"）

- **写就在你连的那个节点上执行**（ADR-0001 D-6）。没有"把请求转发到 leader"这一层：入口节点用**你的令牌**鉴权、在**本地事务**里执行写核心（事务内重读活行 + 当前配置，校验归属/配额/前缀）、然后把自己的配置**提交到集群的权威提交点**（`head`）。单节点部署下最后一步是本地完成，行为与集群形态对你**完全一致**（`200` 带资源 + `config_version`，或 `204`）。
  > ⚠ **过去的一个约束**已经**不再是约束**：旧拓扑要求"租户写必须经由 edge 节点，打到 leader 自己的数据面会恒定 `503 no_leader`"。**那个约束随转发层一起删除**：现在**任何节点都接受并执行租户写**，把你的 LB 指向哪一台都可以。
- **提交点是集群的 `head`，不是这个节点**。所以存在一种**只有集群部署才有**的失败：本地提交成功、但提交到 `head` 失败（集群**失去多数派**）⇒ **`503 config_not_published`**。它不是"你的请求有问题"，也不是"什么都没发生"——**这一台已经改了**，只是集群还没改。按上一节的步骤处理。
- **分区 / 无多数派时写 fail-closed，但读继续**：`503 config_not_published` 时读仍正常（这个节点用它已经物化的配置服务，包括你刚写的那份）；`503 not_ready` 则是本节点**还没有**物化配置（启动窗口），稍等或换节点即可。
- **你的令牌不会离开这个节点**（没有转发，也就没有 `x-hydra-tenant-token` 这个头）。

#### 错误码（写端点）

| HTTP | code | 含义 | 可重试 |
|---|---|---|---|
| `400` | `invalid_request` | 请求体不是合法 JSON | 否 |
| `400` | `invalid_name` / `empty_key_prefix` / `invalid_key_prefix` / `key_prefix_overlap` | 名字 / 前缀校验失败（前缀须含分隔符、与同租户既有前缀及启用中的 operator binding 不重叠） | 否（改正后重发） |
| `400` | `provider_not_found` / `provider_not_in_tenant` / `model_not_in_tenant` / `model_not_served_by_provider` | 路由的 provider/model 不在你的授权范围内 | 否（核对 provider/model） |
| `400` | `quota_exceeded` | 子租户或路由数量到顶（每租户 64 子租户 / 每子租户 32 路由） | 否（删掉一些） |
| `400` | `prefix_generation_failed` | 自动生成前缀多次尝试仍冲突（罕见） | **是**（重试） |
| `401` | `unauthorized` | 令牌缺失 / 错误 / 该租户未配置令牌（三者文案相同，与只读端点一致） | 否（先检查令牌） |
| `403` | `tenant_id_mismatch` | URL 里的 `tenant_id` 与令牌归属不一致 | 否（检查 base URL） |
| `404` | `not_found` | 资源不存在，**或**属于别的租户（二者刻意不可区分） | 否（核对 id / `sub_tenant_id`） |
| `409` | `name_duplicate` / `prefix_duplicate` | 名字 / 前缀与同租户既有子租户冲突 | 否（换个 name/prefix） |
| `429` | `rate_limited` | 触发每租户请求预算（带 `Retry-After`，见 §7.2）。**写端点没有单独的"配置写预算"**：旧的 `too_many_requests` 那个额度随内部端点一起退役了（集群总量上界 = 节点数 × 单节点窗口） | **是**（等 `Retry-After`） |
| `503` | `not_ready` | 本节点还没有物化配置（写未执行） | **是**（稍等或换节点） |
| `503` | `config_not_published` | **写已在本节点提交，但没能提交到集群**（失去多数派）。别的节点看不到它——**等多数派恢复后重发同一个请求**（幂等，且那次重发才是发布动作） | **是**（重读 + 重发） |

#### 边界（**必读**，与 §7.6 同义）

1. **停用 / 删除 ≠ 吊销。** 删掉一个子租户或路由只是**停止按它的前缀 steering**；那些 client key 仍**照常走默认管线**（认证仍过你的 `auth_url`）。**吊销 key 永远是你 `auth_url` 的职责**，不是 Hydra 删子租户/路由的行为。
2. **前缀是路由选择器，不是身份、也不是秘密。** 任何调用方都能出示任意前缀的 key，但路由永远受 `model ∩ 你被授权的 provider` 约束（运行时 backstop），认证始终在你的 `auth_url`。按子租户归因的可信度 = 你自己发 key 的纪律。
3. **收敛窗口。** 变更写进权威提交点（`head`）之后，其它节点各自**物化**（1 s 一跳 + 版本闸门），所以可能**短暂仍按旧路由**——用 `config_version` 对照确认（`whoami` / 只读端点）。**注意**：`config_version` 在 `503 config_not_published` 的那一次写上是"本节点的版本"，此时它**领先**于集群，所以只把 `200` 响应的 `config_version` 当基线。

---

### 5.7 The four-level user hierarchy: groups / users / api-keys / billing (2026-11-03)

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

Propagation of any of those flips: **on your node the next request**; other nodes within **≤5 s** (config publish + ~1 s materialise). There is **no auth-cache residual window any more** — see §7.4's rewrite.

---

## 6. 错误码总表

| code | HTTP | 含义 | 可重试 |
|---|---|---|---|
| `unauthorized` | 401 | 令牌缺失 / 错误 / 该租户未配置令牌（三者文案相同） | 否（先检查令牌） |
| `tenant_id_mismatch` | 403 | 令牌归属与 URL 里的 `tenant_id` 不一致 | 否（检查 base URL） |
| `not_ready` | 503 | 该节点还没有配置快照 | **是** |
| `not_found` | 404 | 保留前缀下不是这documented set路由的路径；写端点上资源不存在 / 属于别的租户（§5.6） | 否（检查路径拼写 / id） |
| `method_not_allowed` | 405 | 方法用错 | 否 |
| `rate_limited` | 429 | 触发频率上限（见 §7.2），带 `Retry-After` | **是**（等 `Retry-After`） |
| `payload_too_large` | 413 | 请求体超过 1 MiB | 否 |
| `invalid_request` | 400 | 请求体不是合法 JSON，或读取失败 | 否 |
| `too_many_keys` | 400 | 单次请求的 key 超过 1000 个 | 否（分批） |
| `invalid_api_key` | 400 | 某个 key 超过 4096 字节 | 否 |
| `invalid_wait` | 400 | `wait` 不是 `converged`/`none` | 否 |
| `invalid_timeout_ms` | 400 | `timeout_ms` 不在 1..60000 | 否 |
| `invalid_group_by` | 400 | `group_by` 不在白名单 | 否 |
| `invalid_since` | 400 | `since` 缺失 / 写法非法 / 晚于 `until` | 否 |
| `invalid_until` | 400 | `until` 写法非法 | 否 |
| `window_too_large` | 400 | 窗口超过上限（默认 31 天） | 否（缩小窗口） |
| `usage_store_unavailable` | 503 | 计量存储读不到或无法解析 | **是** |
| `config_not_published` | 503 | **写已在本节点提交，但没能提交到集群**（失去多数派；写端点，§5.6）。读照常 | **是**（**先重读、等多数派恢复后重发同一请求** —— 那次重发才是发布动作） |
| `invalid_name` | 400 | 子租户名字非法（写端点，§5.6） | 否 |
| `empty_key_prefix` / `invalid_key_prefix` / `key_prefix_overlap` | 400 | 前缀为空 / 非 ASCII / 无分隔符 / 与同租户前缀或启用中的 operator binding 重叠（写端点，§5.6） | 否 |
| `provider_not_found` / `provider_not_in_tenant` / `model_not_in_tenant` / `model_not_served_by_provider` | 400 | 路由的 provider/model 不在你的授权范围内（写端点，§5.6） | 否 |
| `quota_exceeded` | 400 | 子租户 / 路由数量到顶（写端点，§5.6） | 否 |
| `prefix_generation_failed` | 400 | 自动生成前缀多次尝试仍冲突（写端点，§5.6） | **是** |
| `name_duplicate` / `prefix_duplicate` | 409 | 名字 / 前缀与同租户既有子租户冲突（写端点，§5.6） | 否 |
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
| same tenant's **tenant API** (rest of this doc) | fully usable (all ACTIVE + RETIRING routes, incl. write endpoints) |

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
| 每租户失效调用 | 10 次/分钟 | 见 §5.2 |
| 每租户**配置写** | 60 次/分钟 | 写端点（§5.6）专用预算，由 **leader 侧**施加（`429 too_many_requests`）；运维可调（`ops.md` §5.5） |

几个要点：

- **配置写预算在 leader 侧**：所有配置写最终都落到唯一写者 leader，所以在 leader 上施加一个进程内窗口即覆盖整条数据面；该窗口**在 leader failover 时重置**（有界突发，anti-DoS，与 §7.4 的 allow-TTL 上界同类）。它与上面"每租户已授权请求 60/分钟"是**两个独立**的预算：前者计量"已认证的任意请求"（在边缘闸门），后者专计量"配置写"（在 leader）。

几个要点：

- **"已授权"= 节点接受的、属于你的已认证请求**：凡通过令牌校验且 URL 租户匹配的请求都计数，**包括随后被 API 以 4xx/5xx 拒绝的**（如 `400 invalid_since`、`413 payload_too_large`、`503 usage_store_unavailable`）；**唯一不计数的是 `403`（URL 写错租户）**。
- **失败预算与已授权预算是独立的**：别人拿错令牌试探**不会**消耗你正常调用的额度。
- **失败锁定只针对"鉴权失败"**：拿**对**令牌的请求**不会**被它拒。锁定的作用是把"持续猜错令牌"的来源在窗口内拦在 `429` 上；持有效令牌的正常调用不受影响。锁定期满自动恢复。
- **`403`（URL 写错租户）不消耗已授权额度**：否则一个配错 base URL 的客户端会把整个租户锁在自己的 API 之外。
- 限额是**每节点**计算的，所以 N 个节点的集群整体上限约为 N 倍。运维可以调（见 `ops.md` §1.1）。

### 7.3 Why you cannot clear a cache by key prefix (HISTORICAL - as of 2026-11-03)

There is **no auth cache on the data plane any more**: client-key auth matches stored `user_api_key` rows (§5.7), so there is nothing to "clear". §5.2's endpoint is still *served* (the T10 deletion batch is deferred) and still accepts its old `api_keys` / whole-tenant forms - but it changes nothing about `/v1/*` outcomes. Historical fact, kept for the still-served endpoint's shape: the cache keyed on `(tenant_id, sha256(api_key))`, so a **prefix** never matched - it was treated as one full key (`invalidated: 0`, silently ineffective). Do the real action via §5.7: `DELETE .../api-keys/{key_id}` (revoke) or `PUT .../billing` (delinquency).

### 7.4 How fast does a revoke / ban take effect (was: "residual window")

**There is no auth-cache residual window any more.** A revoke, a disable, or a delinquency flip is decided by published config, not by a TTL:

- **your node**: the **next** request already sees it;
- **other nodes**: within **<=5 s** (config publish + ~1 s replica materialise).

To be sure it landed everywhere, compare `config_version` via `whoami` and use the publish/fleet state the write endpoints return (§5.6 transport semantics) - do **not** rely on a `fleet.state == "applied"` from `invalidate`. A persistently `lagging` node is an ops issue (ops has matching alerts).

### 7.5 用量口径

- 契约里**没有** `tenant_id` 查询参数：多传会被**忽略**，身份只认令牌。这是防止越权的设计（你无法查别人的用量）。
- `requests` 在 ClickHouse 部署下是**近似值**（见 §5.3 边界 2）。
- 计量存储的保留期由运维决定，超出保留期的历史查不到，会表现为"该窗口没有记录"（`as_of: null`），而不是报错。

### 7.6 子租户前缀的语义边界 (**must read** - these endpoints are RETIRING)

1. **Prefix is not identity, and not a secret.** Any caller may present any prefix, but routing stays bounded by `model ∩ your granted providers` (runtime backstop). **Auth no longer happens at your `auth_url`**: the key must match a stored `user_api_key` row of YOUR tenant (§5.7 truth table) - a stranger wearing someone else's prefix gets **401**, there is no pass-through to an external auth service.
2. **Stop / delete a sub-tenant is NOT a revoke.** Deleting one only stops prefix steering; its keys keep flowing through the default pipeline (they always did). **Revoking a key is now YOUR §5.7 endpoint** (`DELETE .../users/{uid}/api-keys/{key_id}`, effective on the next request) - sub-tenant deletion never revoked anything.
3. **Edge staleness.** Route/sub-tenant changes converge via control-plane polling (**~1s** + version no-rewind); you may briefly see the old routing. Same nature as every config change; compare `config_version` from `whoami`.

---

## 8. 排障

| 现象 | 可能原因 | 怎么办 |
|---|---|---|
| `401 unauthorized` | 令牌写错/被轮换/该租户没配令牌 | 核对令牌；确认运维没给你轮换过 |
| `403 tenant_id_mismatch` | URL 里的 `tenant_id` 和令牌不是同一个租户 | 用 `whoami` 返回的 `tenant_id` 拼 URL（`base_url` 字段直接给了前缀） |
| `404 not_found` | 路径拼错（例如少了 `api/v1`，或路径不在这documented set之内）；写端点上资源不存在 / 属于别的租户（§5.6） | 对照 §1 的路径表 |
| `405 method_not_allowed` | 方法用错（例如用 GET 调 invalidate） | `GET` whoami/usage；`POST` invalidate |
| `429 rate_limited` | 触发限流或锁定 | 读 `Retry-After` 再重试。持**有效令牌**时，429 只会来自每租户**已授权**频率（60/分钟）或失效频率（10/分钟）上限；**失败**锁定（按来源 IP / 令牌摘要）只拦**错误**令牌的来源，不会拦对令牌 |
| `503 not_ready` | 该节点还没物化配置（启动窗口） | 重试（会落到其他节点） |
| `503 config_not_published` | 集群**失去多数派**：写已落在这台节点的库上，但没能提交到集群 | **不是你的请求有问题**。等集群恢复后**重发完全相同的请求**（幂等；那次重发才是把改动发布出去的动作）。需要确认时用**同一个节点**重读资源 |
| `503 usage_store_unavailable` | 计量存储读不到或返回无法解析 | 重试；持续失败把 `X-Hydra-Trace-Id` 给运维 |
| `202 pending` 一直出现 | 集群里有节点没确认；若 `fleet.nodes_total == 0` 则是该节点还无法枚举存活节点（启动窗口 / 注册表行过期） | `lagging` 有名字就给运维；`nodes_total == 0` 时先重试或换节点，持续如此把 `X-Hydra-Trace-Id` 给运维 |
| `invalidated: 0` | HISTORICAL: the data plane no longer consults that cache (§5.2 RETIRING) | means nothing for `/v1/*`; revoke via §5.7 instead |
| key still passes after you "invalidated" | invalidation no longer gates the data plane; or you hit a `lagging` node | `DELETE .../api-keys/{key_id}` (§5.7); check `config_version` / `lagging` |
| client answers 402 `user_delinquent` | that user's `billing_status` is `delinquent` (§5.7) | intended gate; restore with `PUT .../users/{uid}/billing {"status":"active"}` |

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
POST {HYDRA}/tenant/{TID}/api/v1/auth/cache/invalidate[...]   (RETIRING: cache no longer consulted)
GET  {HYDRA}/tenant/{TID}/api/v1/usage?since=<必填>[&until=][&group_by=none|model|provider|user|user_group|api_key|day]
GET  {HYDRA}/tenant/{TID}/api/v1/sub-tenants
GET  {HYDRA}/tenant/{TID}/api/v1/sub-tenant-routes
PUT    {HYDRA}/tenant/{TID}/api/v1/sub-tenants/{name}        body {key_prefix?, enabled}      (upsert by name)
DELETE {HYDRA}/tenant/{TID}/api/v1/sub-tenants/{id}                                        (delete by immutable id)
PUT    {HYDRA}/tenant/{TID}/api/v1/sub-tenant-routes            body {sub_tenant_id, model_key?, provider_id, enabled} (upsert by (sub_tenant_id, model_key))
DELETE {HYDRA}/tenant/{TID}/api/v1/sub-tenant-routes/{id}     (delete by immutable id)

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
写端点判定：200 = 已写（带 config_version）｜ 204 = 删除（幂等）｜ 504 forward_result_unknown = 结果未知，先重读再重发
