# Hydra 租户自助 API 对接文档

> **给谁看**：接入 Hydra 的**租户**（及其研发/运维）。这是对外契约，可以照此写代码。
> **不给谁看**：Hydra 自身的实现细节见 `design-tenant-api.md`（内部设计）与 `ops.md`（运维手册）。
>
> 本文的每一条都对应仓库里可运行的代码/测试；凡是"你会看到的行为"，都有测试守着。若本文与线上行为不一致，那是**缺陷**，请找运维报。

---

## 1. 你能用它做什么

五个端点，都跑在**你已经在用的那个数据面地址**上（客户端调 `/v1/*` 的同一个域名/端口），因此不需要运维额外为你开放管理口。

| 端点 | 方法 | 用途 |
|---|---|---|
| `/tenant/{tenant_id}/api/v1/whoami` | `GET` | 读你自己当前的配置：绑定域名、`auth_url`、是否被停用、配置版本 |
| `/tenant/{tenant_id}/api/v1/auth/cache/invalidate` | `POST` | **让某个/全部客户端 key 立刻重新鉴权**（欠费停机、付费恢复、封禁某个 key） |
| `/tenant/{tenant_id}/api/v1/usage` | `GET` | 查某个时间窗内你实际产生了多少请求/token |
| `/tenant/{tenant_id}/api/v1/sub-tenants` | `GET` | 只读列出**你自己且启用中的**子租户（前缀、启用状态、配置版本；已停用的不返回） |
| `/tenant/{tenant_id}/api/v1/sub-tenant-routes` | `GET` | 只读列出**你自己**的子租户路由（`model → provider`、默认路由、配置版本） |

**明确没有的能力**（不是漏做，是刻意不做）：

- **不能改任何配置**。域名、`auth_url`、证书、是否启用，都由运维通过管理 API 设置。所以本 API 没有任何"字段级越权"的可能。
- **不能查明细**。`/usage` 只给聚合数，不给逐条请求记录。
- **不能按 key 前缀清缓存**。见 §7.3：缓存里存的是 key 的 SHA-256 摘要，前缀不可匹配。只接受**完整 key** 或**整个租户**。

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
    - 它放在 `Authorization: Bearer` 里，只用于调用本文的五个端点；
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

五个端点方法固定；用错方法 → `405 method_not_allowed`。保留前缀下**任何**不是这五条路由的路径 → `404 not_found`（本 API 自己回答，**不会**落到上游）。

### 4.5 幂等性

- 所有 `GET` 端点（`whoami` / `usage` / `sub-tenants` / `sub-tenant-routes`）天然幂等。
- `POST auth/cache/invalidate` **幂等**：重复调用只是重复"清掉本来就已经没有的东西"。所以重试安全。
- `invalidated` 是**本节点实际删掉的条数**，重复调用会变 0，这**不是**失败：那些 key 下次请求本来就会回源。

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

**本 API 最重要的一个端点。** Hydra 会缓存"某个客户端 key 的鉴权结论"（默认 allow 5 分钟 / deny 30 秒），所以你在自己系统里封禁一个 key 之后，Hydra 侧可能还有几分钟在放行。调它即可让结论立刻失效、下次请求强制回你的 `auth_url` 重新判定。

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
| `wait` | `converged` \| `none` | `converged` | `none` = 发出去就立刻返回 `202`，不等集群确认（适合批量脚本）。给了别的值 → `400 invalid_wait` |
| `timeout_ms` | `1`..`60000` | 服务端配置（默认 2000） | 本次请求等待全集群确认的预算。越界/非数字 → `400 invalid_timeout_ms` |

> 这两个参数是**校验**而不是"忽略"：写错了会明确报错，不会静默按默认值执行 —— 否则你会以为自己要了（或跳过了）一次等待。

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
| `fleet.lagging` | 未确认的节点名。仅当 `state=pending` **且确实检查过节点并发现落后**时非空；`nodes_total=0`（无人可查）时为空。运维可以直接拿它去查 |
| `fleet.event_id` | 本次失效在内部总线上的事件 ID。用 `wait=none` 时拿它做后续对账 |
| `fleet.waited_ms` | 本次真正等待的毫秒数 |

#### 状态码与 `fleet.state` 的对应（**必须按这个判断，不要只看 HTTP 200**）

| HTTP | `fleet.state` | 含义 | 你该做什么 |
|---|---|---|---|
| `200` | `applied` | **所有存活节点都已生效** | 完成 |
| `200` | `single_node` | 本节点就是全部数据面（单节点部署） | 完成 |
| `202` | `pending` | 已发布，但**没有全部确认** | **不是错误，也还不是"完成"**。可重试（幂等），或把 `lagging` 交给运维。用 `wait=none` 时这里**不代表**那些节点落后 —— 只是没人去看。若 `nodes_total=0`，说明本节点此刻**枚举不出存活节点集**（启动窗口 / 注册表行过期），`lagging` 因此为空、与"有具体落后节点"不是一回事 —— 请重试或交运维 |
| `503` | `unavailable` | 失效通道存在但**不可用**（发布或水位读取失败） | **集群没有被通知**。必须重试或找运维。响应体仍是上面的正常结构，**没有** `error.code` |

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
| `group_by` | 否 | `none` | `none` \| `model` \| `provider` \| `day`；其他值 → `400 invalid_group_by` |

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
| `rows` | 按 `group_by` 分组的行；`group_by=none` 时为空数组。`key` 是分组值（模型名 / provider id / `YYYY-MM-DD`） |
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

> 这两个端点是**只读**的：要创建/修改/删除子租户或路由，由运维通过管理 API（`/api/v1/sub-tenants`、`/api/v1/sub-tenant-routes`）操作，见 `ops.md` §5.5。

---

## 6. 错误码总表

| code | HTTP | 含义 | 可重试 |
|---|---|---|---|
| `unauthorized` | 401 | 令牌缺失 / 错误 / 该租户未配置令牌（三者文案相同） | 否（先检查令牌） |
| `tenant_id_mismatch` | 403 | 令牌归属与 URL 里的 `tenant_id` 不一致 | 否（检查 base URL） |
| `not_ready` | 503 | 该节点还没有配置快照 | **是** |
| `not_found` | 404 | 保留前缀下不是这五条路由的路径 | 否（检查路径拼写） |
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

---

## 7. 你需要知道的边界

### 7.1 被停用（欠费停机）之后还能做什么

`whoami` 里的 `enabled` 变成 `false` 之后，你的**客户端请求**会被拒绝，但**这些端点全部照常可用**（有测试守着这一点）。这是刻意的：停用是**你的**策略（通过你的 `auth_url` 表达），如果你连恢复路径都进不去，就无法自助恢复。

典型恢复流程：

```bash
# 1) 确认自己被停用了
curl -s $HYDRA/tenant/$TID/api/v1/whoami -H "Authorization: Bearer $TOK" | grep enabled

# 2) 充值、在你的鉴权系统里放行

# 3) 让所有客户端立刻恢复（否则最多要等 5 分钟的缓存过期）
curl -s -X POST $HYDRA/tenant/$TID/api/v1/auth/cache/invalidate \
  -H "Authorization: Bearer $TOK" -H "content-type: application/json" -d '{}'
```

### 7.2 限流与锁定

| 维度 | 默认上限 | 说明 |
|---|---|---|
| 每租户**已授权**请求 | 60 次/分钟 | 防"把本 API 当免费 CPU/DB" |
| 每**来源 IP** 鉴权失败 | 10 次/分钟 | 防一个来源猜令牌 |
| 每**令牌摘要**鉴权失败 | 10 次/分钟 | 防一个令牌被多来源试探 |
| 持续超限后的锁定 | 900 秒 | 被锁定的维度对**失败鉴权**返回 **`429`（而不是 401）**；持有效令牌的请求不受锁定影响 |
| 每租户失效调用 | 10 次/分钟 | 见 §5.2 |

几个要点：

- **"已授权"= 节点接受的、属于你的已认证请求**：凡通过令牌校验且 URL 租户匹配的请求都计数，**包括随后被 API 以 4xx/5xx 拒绝的**（如 `400 invalid_since`、`413 payload_too_large`、`503 usage_store_unavailable`）；**唯一不计数的是 `403`（URL 写错租户）**。
- **失败预算与已授权预算是独立的**：别人拿错令牌试探**不会**消耗你正常调用的额度。
- **失败锁定只针对"鉴权失败"**：拿**对**令牌的请求**不会**被它拒。锁定的作用是把"持续猜错令牌"的来源在窗口内拦在 `429` 上；持有效令牌的正常调用不受影响。锁定期满自动恢复。
- **`403`（URL 写错租户）不消耗已授权额度**：否则一个配错 base URL 的客户端会把整个租户锁在自己的 API 之外。
- 限额是**每节点**计算的，所以 N 个节点的集群整体上限约为 N 倍。运维可以调（见 `ops.md` §1.1）。

### 7.3 为什么不能按 key 前缀清缓存

Hydra 的鉴权缓存键是 `(tenant_id, sha256(api_key))`，**内存和 Redis 里都只有摘要**。摘要不可做前缀匹配，所以"清掉所有 `sk-proj-abc*` 的缓存"在当前数据结构下**无法实现**。

不要试图用前缀调 `api_keys`：它会被当成**一个完整的 key**（于是匹配不到、`invalidated` 为 0，静默无效）。请传完整 key，或清空整个租户。

### 7.4 封禁多久真正生效（"残余窗口"）

你调 `invalidate` 之后，**正常情况**下等到 `fleet.state == "applied"` 就已经全集群生效。

但有一种情况需要你知道：某个节点**活着、在收流量，却没能消费到失效事件**（消费者任务异常、与内部总线断连等）。这种节点的兜底就是缓存项**自己的 TTL** —— 而这个 TTL 的上限由运维设定（默认 **300 秒**），也就是说：

> **最坏情况下，一个被停用的 key 可能在被封禁后继续工作到约 5 分钟。**

这不是"没生效"，而是刻意的上限（默认值等于系统原有的 allow TTL，所以对没设长 TTL 的租户零变化）。**要更快收敛**：确认 `fleet.state == "applied"`；持续拿到 `202` 就把 `lagging` 里的节点交给运维（运维有对应告警）。

### 7.5 用量口径

- 契约里**没有** `tenant_id` 查询参数：多传会被**忽略**，身份只认令牌。这是防止越权的设计（你无法查别人的用量）。
- `requests` 在 ClickHouse 部署下是**近似值**（见 §5.3 边界 2）。
- 计量存储的保留期由运维决定，超出保留期的历史查不到，会表现为"该窗口没有记录"（`as_of: null`），而不是报错。

### 7.6 子租户前缀的语义边界（**必读**）

你看到 `sub-tenants` / `sub-tenant-routes` 返回的前缀和路由后，请理解这三条边界：

1. **前缀不是身份、也不是秘密。** 任何调用方都可以出示任意前缀的 key，但路由**永远**受
   `model ∩ 你被授权的 provider` 约束（运行时 backstop），且**认证始终发生在你的 `auth_url`**——
   拿别人前缀的假 key 仍要过你的鉴权。因此**按子租户的归因可信度 = 你自己发 key 的纪律**
   （你给哪个子租户发了什么前缀的 key，是只有你知道的）。Hydra 不铸造、不验证 key 的归属。
2. **停用 / 删除子租户 ≠ 吊销。** 删掉一个子租户只是**停止按它的前缀 steering**；那些 key 仍
   **照常走默认管线**（认证仍过你的 `auth_url`）。**吊销 key 永远是你 `auth_url` 的职责**，
   不是 Hydra 删子租户的行为。不要以为"删了子租户 = 这些 key 失效了"。
3. **edge 陈旧窗口。** 路由/子租户变更经控制面轮询收敛（**~1s** + 版本防倒退）；变更之后可能
   **短暂仍按旧路由**——与一切配置变更同性质。需要立即生效时，确认快照版本已推进
   （用 `whoami` / 上述端点的 `config_version` 对照）。

---

## 8. 排障

| 现象 | 可能原因 | 怎么办 |
|---|---|---|
| `401 unauthorized` | 令牌写错/被轮换/该租户没配令牌 | 核对令牌；确认运维没给你轮换过 |
| `403 tenant_id_mismatch` | URL 里的 `tenant_id` 和令牌不是同一个租户 | 用 `whoami` 返回的 `tenant_id` 拼 URL（`base_url` 字段直接给了前缀） |
| `404 not_found` | 路径拼错（例如少了 `api/v1`，或路径不在这五条之内） | 对照 §1 的路径表 |
| `405 method_not_allowed` | 方法用错（例如用 GET 调 invalidate） | `GET` whoami/usage；`POST` invalidate |
| `429 rate_limited` | 触发限流或锁定 | 读 `Retry-After` 再重试。持**有效令牌**时，429 只会来自每租户**已授权**频率（60/分钟）或失效频率（10/分钟）上限；**失败**锁定（按来源 IP / 令牌摘要）只拦**错误**令牌的来源，不会拦对令牌 |
| `503 not_ready` | 该节点还没拿到配置快照 | 重试（会落到其他节点） |
| `503 usage_store_unavailable` | 计量存储读不到或返回无法解析 | 重试；持续失败把 `X-Hydra-Trace-Id` 给运维 |
| `202 pending` 一直出现 | 集群里有节点没确认；若 `fleet.nodes_total == 0` 则是该节点还无法枚举存活节点（启动窗口 / 注册表行过期） | `lagging` 有名字就给运维；`nodes_total == 0` 时先重试或换节点，持续如此把 `X-Hydra-Trace-Id` 给运维 |
| `invalidated: 0` | 那些 key 在本节点本来就没有缓存项 | 正常。下次请求会回源；要确认请用一次请求制造缓存后再调 |
| 清缓存后客户端仍被放行 | 还在 §7.4 的残余窗口内，或有节点 `lagging` | 看 `fleet.state`；持续如此找运维 |

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
POST {HYDRA}/tenant/{TID}/api/v1/auth/cache/invalidate[?wait=converged|none][&timeout_ms=1..60000]
GET  {HYDRA}/tenant/{TID}/api/v1/usage?since=<必填>[&until=][&group_by=none|model|provider|day]
GET  {HYDRA}/tenant/{TID}/api/v1/sub-tenants
GET  {HYDRA}/tenant/{TID}/api/v1/sub-tenant-routes

Header: Authorization: Bearer <tenant access token>
Trace:  响应头 X-Hydra-Trace-Id，报障必带

E2 判定顺序：先看 HTTP，再看 fleet.state
  200 applied/single_node = 完成 ｜ 202 pending = 未确认（可重试）｜ 503 unavailable = 集群没收到
E3 判定：as_of 为 null 表示窗口内无记录（不是报错）；集群下 requests 是近似值
```
