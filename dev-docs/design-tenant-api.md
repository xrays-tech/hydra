# 设计：租户自助 API（Tenant API）——数据面 `/tenant/{tenant_id}/api`

> **对外契约（给租户）在 [`tenant-api-integration.md`](tenant-api-integration.md)**：快速开始、字段表、错误码总表、边界与排障。本文件是**内部设计**：为什么这样设计、备选方案与取舍。
> 两者冲突时**以代码与测试为准**，并视为缺陷（修复方式：先改代码或先改契约，再把两份文档一起对齐）。

- **日期**：2026-09-17
- **状态**：**已实现**（T1–T10 全部落地；门禁与交叉审核记录见 `aegis/plans/2026-09-17-tenant-api.md`）
- **作者**：编码智能体（DeepSeek）
- **对应需求**：现状 admin API 与 tenant API 混在同一端口/同一路由树/同一 `/api/v1` 命名空间；需要提供**专门的 tenant API**，以**租户访问令牌（tenant access token）**为唯一鉴权手段，**挂在数据面端点**、以 `/tenant/{tenant_id}/api` 为 base URL，把租户对接所需的全部接口收敛到一处入口。
- **本轮范围**（v2，2026-09-17 需求变更后）：① 强制清除一个或多个 api-key 的认证缓存；② 获取某个时间戳之后的 token 用量；③ 只读的本租户状态自述（`whoami`）。**已移除**："通过接口自动设置绑定域名与 Auth URL"（见 §2.3 需求变更记录）。
- **前置研究**：四路并行代码勘察（数据面资源管线与集群转发 / 用量存储与查询路径 / 认证缓存失效 / 测试与门禁约定），事实均带 `file:line` 证据；结论回填于 §1、§4、§6、§10。

---

## 0. TL;DR

1. **"混在一起"是四重耦合，不是一个端口问题**：同一端口（8081）、同一路由树（`/api/v1/*` 平铺）、同一鉴权闸门代码、同一份文档与 UI。租户侧唯一的接口 `POST /api/v1/tenants/{tenant_id}/auth/cache/invalidate` **物理上住在管理面**（`admin/mod.rs:616-668`），这就是要收敛的对象。
2. **新入口 = 数据面保留前缀 `/tenant/{tenant_id}/api/v1/*`**，在 `request_filter` 的**第 0 步**拦截，**早于** Host→tenant 解析（`proxy.rs:365-379`）、早于客户端 api-key 解析（`proxy.rs:396-410`）。晚一步，租户令牌就会被当成客户端 api-key 走外部鉴权、甚至进用量记录（§3.2）。
3. **身份只有一个权威来源：租户令牌。** URL 里的 `{tenant_id}` 必须与令牌归属一致（不一致 → 403）；`Host` 不参与鉴权（§3.3）。
4. **令牌校验不再读 DB。** 今天 `tenant_id_for_token` 每次尝试都做全表 `SELECT id, access_token_hash FROM tenant`（`admin/handlers.rs:1716-1728`、`db.rs:991-1019`），零限流零锁定——放到公网端口就是 DB 读放大器。新路径改读**集群快照里已经存在的** `FidelityRows::tenant_token_hashes`（`cluster/content.rs:29/46`，wire 上密封、hydrate 解封 `snapshot.rs:325-329`）：**零 DB I/O、leader/standby/edge 三种角色都能本地判定、随快照天然一致**（§3.4）。
5. **三个端点**：`GET /whoami`（**纯快照，零 DB，全角色可用**）、`POST /auth/cache/invalidate`（**零 DB；全集群清除且可确认**）、`GET /usage`（**唯一需要 DB**）。
6. **去除写需求带来的三处结构性简化**（§2.3）：
   - **两个高危威胁整体消失**——不再有"租户可写域名"（Host→租户映射被不可信方控制 → 客户端原始 api-key 被截获）与"租户可写 `auth_url`"（网关出站目标被不可信方指定 → SSRF）。§5 从 5 个威胁缩到 3 个。
   - **不再有任何 DB 写路径** → 不需要窄写 DB 函数、不需要 `key_provider`、不需要 `reload_all` 与跨服务共享的 `reload_lock`/`snapshot_stale`、不存在域名唯一性冲突、不存在"停用租户还能改数据面行为"的闸门不对称。
   - **不再需要 leader 转发** → 不需要 `cluster_registry`/`leader_ready`/`forward_mutation`、不需要在管理口挂第二个执行点、不引入任何新信任边界、不碰 `HYDRA_PUBLIC_URL` 的语义。**租户 API 变成"只读 + 一次幂等的缓存删除"**。
7. **E2 的语义是"在全部数据面节点上清除"，而且调用方能知道是否真的做到了**（§4.2）。今天做不到这一点：远端节点的 L1 命中项**不会**因为发布方删了 L2 而消失（L1 命中根本不查 L2），而消费者的已读位置 `last_id` 只存在于 `spawn_invalidation_consumer` 的循环局部变量里（`events.rs:293` 声明 / `:311` 推进），**从不对外发布**——所以 `published: true` 只表示"进了流"，与"已生效"无关。设计补上三层：**L1 扇出**（既有失效流+消费者）+ **L2 权威**（发布方同步 `DEL` 共享 Redis）+ **L3 收敛屏障**（每节点把"已应用到的事件 ID"写进一个 Redis HASH，发布方等待**注册表里全部存活节点**确认）。返回码因此是 **200 = 已全量生效 / 202 = 已接受未收敛（带 `lagging` 列表）/ 503 = 集群成员但失效通道不可用**（今天这种情况会静默报 `true`）。
8. **必须给残余窗口一个"不由租户决定"的硬上界**（§4.2.5）。活着但不消费失效事件的节点（消费者挂掉、与 Redis 分区、未注册）只能靠 L1 的 allow TTL 兜底——而**今天的 allow TTL 可以被租户用 `expires_in` 抬到任意长**（`http.rs:661-674`；`clear_all` 的注释自己承认 "can raise well beyond the 300 s default"，`http.rs:309-311`）。也就是说：今天"某节点上封禁何时生效"部分取决于**被停用的那个租户**。新增 `HYDRA_AUTH_ALLOW_TTL_MAX_SECS`（默认 300 = 现有默认值，**对不设 `expires_in` 的租户零变化**）把该窗口封顶。
9. **`AppState` 只增 2 个字段**：`invalidation: Option<InvalidationStream>`（E2 广播）与 `usage: Arc<dyn UsageQuery>`（E3 的读能力，按 sink kind 在启动时注入唯一实现）。DB 池**不新增字段**——直接用 `ConfigStore::pool()`（`store.rs:374-379`，同一个 store 实例的公开访问器，避免第二个"本节点有没有本地库"的 owner）。
10. **两个后端都在 v1，集群首次可用**（需求确定：生产必跑集群，用量首发可用）。集群强制 `HYDRA_USAGE_SINK=clickhouse`（`main.rs:263-269`），而 CH 侧今天只有 INSERT、没有读路径 → 本设计**新增 CH 读通道**。关键事实全部对仓库自带的活实例**实测**得到（ClickHouse 24.3，§4.3.3）：**64 位整数默认被序列化成 JSON 字符串**（`{"n":"12345"}`）、无匹配行时 `MAX(created_at)` 返回 `""` 而不是 null、`{name:String}` + `param_*` 参数绑定实测可安全承载 `x' OR 1=1 --`、主键 `ORDER BY (created_at, …)` 对范围和等值谓词**确实裁剪**（`Granules: 1/2`）。**CH 侧最危险的陷阱是"整数被引号包裹"**：解析器若只接受 JSON number，就会静默返回 0 用量——正是那个"语法正确、语义错误的 200"（§4.3.3 CH-A）。
11. **用量查询是净新增读面**：`usage_record` 今天在生产代码里**只写不读**（`src/` 内无任何 `SELECT`），索引 `idx_usage_record_tenant(tenant_id, created_at)` 正好匹配 `WHERE tenant_id=? AND created_at>=?`（`migrations/0001_init.sql:101`）；`created_at` 是应用显式绑定的 `%Y-%m-%dT%H:%M:%SZ`（`proxy.rs:1390-1392`），**不是** SQLite 的 `datetime('now')`——用错格式比较会静默多算一整天（§4.3.2）。
12. **不做"按前缀清缓存"**：`AuthCache` 只存 `(tenant_id, sha256(api_key))`（`http.rs:59/192-194`），摘要不可前缀匹配（§4.2.3）。

**Verdict: DO IT，风险面已降到"只读 + 幂等删除"，且三个端点在全部角色（含集群 edge）上都可用。** 剩下的全部风险集中在三处：令牌爆破/放大（用快照校验 + 限流解决）、跨租户越权（身份只来自令牌）、以及**用量端点的三个解析陷阱**（CH 整数被引号包裹 / CH 空集返回空串 / 两侧时间格式不一致）——三者都会以"返回 0"的形式静默失败，因此 §10 的测试矩阵把它们各自列为独立负例。

---

## 1. 背景与现状

### 1.1 今天的两条入口

| 端口 | 默认绑定 | 鉴权 | 命名空间 | 实际服务对象 |
|---|---|---|---|---|
| 数据面 `HYDRA_LISTEN` | `0.0.0.0:8080`（`main.rs:54`） | 客户端 api-key → 租户 `auth_url` 外部鉴权（缓存优先） | `/v1/*` 任意路径（透传） | **租户的终端客户** |
| 数据面 TLS `HYDRA_TLS_LISTEN` | 未设则不存在（`listeners.rs:34-42`） | 同上（SNI 选租户证书，`design.md` §12.3） | 同上 | 同上 |
| 管理面 `HYDRA_ADMIN_ADDR` | `127.0.0.1:8081`（`main.rs:55`） | `HYDRA_ADMIN_TOKEN`（`admin/mod.rs:263-275`）**或**租户令牌（唯一例外，`admin/mod.rs:616-668`）**或**集群令牌（`/api/v1/internal/*`，`admin/mod.rs:582-604`） | `/api/v1/*` 平铺 | **运维**，外加**一个租户接口** |

租户流量被劈成两半：数据面走业务，管理面走自助运维。而管理面**默认绑回环**，租户物理上够不着；运维要开放它，就得把整个管理 API（providers / provider-keys / tenants / limit-roles 全套 CRUD）一起端到同一监听器上，靠一个共享 token 兜底。

### 1.2 四重耦合（"混在一起"的精确形态）

| # | 耦合面 | 证据 | 后果 |
|---|---|---|---|
| C1 | **端口耦合** | 租户自助端点在 `AdminService`（`admin/mod.rs:616-668`），与运维 CRUD 同一个 `ServeHttp` | 开放租户自助 = 同时开放运维面；管理口绑回环 = 租户自助不可用 |
| C2 | **路由树耦合** | 租户端点塞进 `/api/v1/tenants/{id}/...`，与运维资源树同层（`admin/mod.rs:343-418`） | `/api/v1/tenants` 既是运维资源又是租户入口；深路径要专门插在"depth 拒绝"之前（`admin/mod.rs:350-358` 的注释就是这个坑） |
| C3 | **鉴权闸门耦合** | 租户闸门 = 在 admin 闸门**之前**插一段 if（`:616-668` 对比 `:670-679`） | 每加一个租户接口就重复一遍前缀匹配 + 令牌校验 + URL/令牌一致性校验；顺序错一次就 401，而**没有测试能覆盖"顺序"这一属性** |
| C4 | **文档/UI 耦合** | `design.md` §13.2「管理 Web API」端点表里列着租户自助端点；`admin-ui/api-docs.js` 同页展示 | 对接方读"管理 API 文档"才知道租户接口；权限模型在文档上就是错的 |

### 1.3 现状的具体缺陷（可证伪，全部带证据；标注本设计是否处置）

| # | 缺陷 | 证据 | 本设计 |
|---|---|---|---|
| D1 | 租户令牌校验**每次请求全表读** | `admin/handlers.rs:1717-1721` → `db.rs:991-1019`（无 `WHERE` 摘要、无 `LIMIT`、无缓存） | ✅ §3.4 改读快照 |
| D2 | 令牌闸门**零限流、零锁定、零失败计数** | 全仓库 NOT FOUND；代码自认同类问题：`admin/mod.rs:233-240` | ✅ §5.1 |
| D3 | `published` 字段会**说谎** | 初值 `true`，仅当 `invalidation` 为 `Some` 且 publish 返回 `Err` 才置 `false`（`admin/handlers.rs:1640-1645`）；而它可为 `None`（`main.rs:621-644`） | ✅ §4.2.2 改 `broadcast` 三态 |
| D4 | `invalidated` 只统计 **L1** 删除 | `http.rs:250-259`（L2 删除成败不入账） | ✅ §4.2.2 并列返回 `checked` |
| D5 | `invalidate_tenant` 是**全表 `retain`** | `http.rs:291-300`，O(全部缓存条目) 且持分片写锁 | ⚠️ 沿用（既有语义），但 §5.1 限流约束其调用频率 |
| D6 | 单个租户令牌可诱发**全集群清缓存** | 失效流裁剪**只要删除任何条目**就 bump generation（`removed > 0`；裁剪无法区分已读/未读）→ 每节点 `clear_all()`（`events.rs:204-214/320-332`）；`MAX_INVALIDATION_KEYS=1000` 只约束单请求键数（`admin/handlers.rs:1557`） | ✅ §5.1 每租户失效频率上限 |
| D7 | 用量表**只写不读** | `src/` 内无 `SELECT ... FROM usage_record`；`UsageSink` 只有 `record`/`shutdown`（`sink.rs:46-63`） | ✅ §4.3 净新增读面 |
| D8 | 现有"用量统计"读的是**进程内计数器** | `admin/handlers.rs:2106-2108` → `admin/metrics.rs:760-800`；重启归零、无时间维度 | ✅ §4.3.4 明确两者口径分工 |
| D9 | 集群强制 ClickHouse sink，而 CH **没有读路径** | `main.rs:263-269`；CH 侧只有硬编码 INSERT（`sink.rs:683-688`），无 SELECT、无响应体解析，config 已被 move 进私有 flush task（`sink.rs:559-568`） | ✅ §4.3.3 新增 CH 读通道（v1 必做） |
| D10 | `HYDRA_ADMIN_ADDR` 非回环绑定**无任何告警** | `main.rs:977-978`；全仓库 NOT FOUND loopback 断言 | 本次不动（**去写需求后管理口不再承载租户 API，紧迫性下降**；仍建议单列一个小改动） |

> 与 v1 相比，D-类缺陷的处置面**没有缩小**（租户关心的仍是 D1–D4、D6–D8），缩小的是**新增风险面**——见 §2.3。

---

## 2. 需求与范围

### 2.1 必须交付

| # | 需求 | 验收面 |
|---|---|---|
| R1 | 专门的租户 API，只认租户访问令牌 | 管理 token / 集群 token / 客户端 api-key 在本入口一律 401 |
| R2 | 挂在数据面，`/tenant/{tenant_id}/api` 为 base URL，可直接访问 | 明文与 TLS 监听器上都可达；不需额外监听器、不需运维开放管理口 |
| R3 | 强制清除一个或多个 api-key 的认证缓存，**在全部数据面节点上生效** | 精确 key 列表；空 = 全租户；**全集群清除 + 收敛确认**（200/202/503，见 §4.2）；返回**可信**的生效语义 |
| R4 | 获取某个时间戳之后的 token 用量，**单节点与集群部署下都必须可用** | 时间窗聚合；租户维度**只**由令牌决定；SQLite 与 ClickHouse 两个后端各自实现（§4.3.3） |
| R5 | 租户可自证身份与状态（只读） | `whoami` 返回租户 id / 名称 / 绑定域名 / 启用状态 / 配置版本 |
| R6 | 收敛：租户对接只需读这一份契约 | `design.md` §13.2 端点表分离；`api-docs.js` 拆分 |

### 2.2 显式非目标（v1 不做，理由随附）

| 非目标 | 理由 |
|---|---|
| 通过接口修改绑定域名 / Auth URL | **需求已移除**（§2.3），能力保留在运维管理 API |
| 租户自助**轮换自己的令牌** | 被窃令牌可反向锁死真租户；且需要新的令牌下发面 → 独立特性 |
| 租户自助配置 supplier/model 授权 | 商务授权归运维；开放即等于租户自行扩权 |
| 按 **api-key 前缀**清缓存 | 摘要不可前缀匹配（§4.2.3） |
| **用量明细**（逐条记录）查询 | v1 只做聚合；明细要游标分页 + 脱敏字段语义（骨架见 §4.3.5） |
| 计费/账单语义 | 只出 token 与请求数；金额由租户侧自算 |

### 2.3 需求变更记录（2026-09-17：移除"自动设置域名与 Auth URL"）

**移除项**：原先设计的 `PUT /settings/domain` 与 `PUT /settings/auth-url`。

**保留能力的方式**：域名与 `auth_url` 的修改继续由**运维管理 API** 承担——`PUT /api/v1/tenants/{id}`（`admin/handlers.rs:989-1063`，admin token 鉴权，整行更新 + 证书 + 令牌摘要同一事务 `db.rs:740-812`），Admin UI 的 Tenants 页继续提供该表单（`admin-ui/app.js`）与 `auth_url` 探活按钮（`POST /api/v1/tenants/auth/test`，`admin/mod.rs:347-349`）。**功能没有丢失，只是从"租户自助"回到"运维代改"。**

**随之删除的设计面（反熵：不再需要的机制必须真删，不能留悬空零件）**：

| 删除项 | 原设计中的作用 | 为什么现在不需要 |
|---|---|---|
| 平台后缀白名单（`DOMAIN_MODE`/`DOMAIN_ZONE`/`DOMAIN_RESERVED`） | 限制租户能绑到哪些域名 | 没有任何租户可控的域名写入路径 |
| 域名 `UNIQUE` 冲突处理（409 `domain_taken`） | 防抢占 | 同上 |
| `auth_url` 形态与网段校验 + 允许列表 + **请求期出口校验** | 防 SSRF（写时 TOCTOU/DNS 重绑定） | 租户无法指定出站目标 |
| 窄写 DB 函数 `update_tenant_settings` | 只碰 `domain`/`auth_url` 两列，避免 `write_tenant` 的整行替换越权 | 没有租户侧写路径 |
| `key_provider` 进入 `AppState` | 原为 `write_tenant` 写密文列所需 | 窄写函数一并删除，写路径整体消失 |
| 跨服务共享的 `reload_lock` / `snapshot_stale` | 租户写后 `reload_all` 的串行化与陈旧上报 | 租户 API 不再触发配置重载 |
| `enabled` 对写接口的闸门 | 防"被停用租户抢域名后求恢复" | 只剩读与缓存删除，停用租户自救路径必须保持畅通 |
| 管理口挂载租户 API（第二执行点） | 作为 edge 转发的落地端 | 没有需要 leader 权威的写 |
| `cluster_registry` / `leader_ready` 进入 `AppState`、`forward_mutation` 复用、`x-hydra-forwarded` 循环守卫 | 把写送到 active leader | **读**不需要权威节点：`whoami` 读快照、`invalidate` 本地清 + 广播、`usage` 读**共享**的计量存储 |
| `host_mismatch` 指标 | 记录 Host 与令牌租户不一致（域名抢占的审计信号） | 不存在域名抢占面，该指标沦为噪声 |
| 6 个环境变量 | —— | 配置项从 11 个降到 5 个（§7.3） |

**代价（必须承认）**：租户不能自助修正绑定域名或认证地址。运维侧因此承担一条新的工单路径；`dev-docs/ops.md` 需补一节"租户改域名/auth_url 的运维流程"（含改域名后证书需同步、`reload_all` 由管理 API 自动触发）。这是**用运维工时换掉一整类凭证截获与 SSRF 风险**，是本轮变更的核心收益。

**安全性态的变化（一句话）**：租户 API 从"可写控制面"变成"**只读 + 一次幂等的缓存删除**"。它再也无法改变配置快照、无法改变路由、无法改变出站目标。

---

## 3. 总体设计

### 3.0 三平面模型

| 平面 | 监听器 | 凭证 | 权限主体 | 读哪份文档 |
|---|---|---|---|---|
| **数据面** | `HYDRA_LISTEN` / `HYDRA_TLS_LISTEN` | 客户端 api-key（外部鉴权） | 租户的**终端客户** | 租户 |
| **租户自助面**（本设计新增） | **同数据面监听器**（`/tenant/{tenant_id}/api/v1/*`） | **租户访问令牌** | **租户本身** | 租户 |
| **运维控制面** | `HYDRA_ADMIN_ADDR` | `HYDRA_ADMIN_TOKEN`；`/api/v1/internal/*` 用 `HYDRA_CLUSTER_TOKEN` | 运维 / 集群内部 | 运维 |

三条硬规则：

1. **凭证不跨平面，且没有任何豁免**：管理 token 在租户自助面一律 401；租户令牌在运维控制面一律 401。**运维控制面上不再存在任何租户令牌路由**（migration 0009 的旧路径按 §8.1 M1 直接删除）——管理面因此回到"只有运维与管理面凭证"的单一语义。
2. **租户自助面不复用管理面的路由树**：路径挂在数据面保留前缀 `/tenant/{tenant_id}/api/v1/*` 之下，不再挂 `/api/v1/tenants/{id}/...`。
3. **一个行为只有一个 owner**：三个 handler 只实现一次（新模块 `tenant_api`），旧路径删除而非保留别名（§8.1 M1）。
4. **只读优先**：租户 API 不产生配置写入、不触发 `reload_all`、不改变数据面行为。唯一的副作用是删除本节点内存里的缓存项并广播一条失效事件。

### 3.1 挂载点与 base URL

```
明文：http://<数据面主机>:8080/tenant/{tenant_id}/api/v1/...
TLS ：https://<任意能连到数据面的主机名>/tenant/{tenant_id}/api/v1/...
```

- `/tenant/` 是**保留前缀**：从此**不再进入代理管线**（不会被透传给上游、不会被 `/v1/models` 目录或 passthrough 抢走）。

  **拦截条件是前缀而不是三个字面路由**（实现期补记，T4）：若只匹配"解析成功的路由"，`/tenant/{tid}/api/v1/typo` 这类近似路径会漏进管线，而租户令牌走的就是 `Authorization: Bearer`——它同时是客户端 api-key 的合法载体，于是该令牌会被 POST 给租户自己的 `auth_url` 并脱敏进用量记录。**裸 `/tenant`（无尾斜杠）同样算保留**：把 base URL 写错的调用方照样会带着令牌，而保留名没有任何合法理由到达上游。
- 前缀采用需求给定的字面形态（非 `/-/tenant/...`）；形态取舍见 §11 Q1。
- **路径里带 `{tenant_id}` 的作用**：让 base URL 自描述 + 作为令牌的**交叉校验位**（客户端把 base URL 配错时立刻 403，而不是静默读到别人的数据）。它不是身份来源。
- 其后必须是 `{tenant_id}/api/v1/...`；`{tenant_id}` 不得为空、不得含 `/`（对齐 `admin/mod.rs:626` 的既有校验风格）。

### 3.2 拦截位置：为什么必须是"第 0 步"

`request_filter` 今天的步骤序（`proxy.rs:354-457`）：

```
(1) Host → tenant                      :365-379   ← 未知域名 404 unknown_domain
(2) tenant.enabled 闸门                 :382-385   ← 停用租户 403 tenant_disabled
(3) 客户端 api-key 抽取（6 种载体）      :396-410   ← **物理上先于 (2.5) 执行**
(2.5) GET /v1/models 目录拦截            :439-457   ← 消费 (3) 的结果（出示的 key 仅用于按前缀绑定收窄）
(3') 缺 key → 401 missing_api_key        :460-463
(4) 外部鉴权（缓存优先 → 租户 auth_url）  :467
(5) 读全量 body → (6) 抽 model → (7) 限流 → (8) 路由 → (9) 故障转移
```

租户自助面必须在 **(1) 之前**（记为第 0 步）拦截。四个独立理由：

| 理由 | 若不提前会发生什么 |
|---|---|
| A. Host 不在鉴权链上 | 第 (1) 步先按 `Host` 判租户：用 IP 直连、用平台自身域名调租户 API，全部落到 `404 unknown_domain`。租户的 base URL 不该依赖"某个域名必须已被某个租户占用" |
| B. 停用租户必须还能自救 | 第 (2) 步的 `enabled` 闸门会挡住停用租户；而欠费停机场景下，**租户正是被停用方**，必须仍能清缓存、查用量、看状态 |
| C. 令牌不能被当成客户端 api-key | 第 (3) 步的 `Authorization: Bearer` 是**客户端 api-key 的合法载体之一**（`hydra_core::apikey` 六种载体）。晚于它，租户令牌会被当成客户端 key：送往租户 `auth_url`（`http.rs:558-566` 把**原始 key**放进 body 与 `Authorization`）、并被 `mask_key` 后写进用量记录（`proxy.rs:1105/1111`） |
| D. 免于业务限流/路由副作用 | 第 (7) 步 count/token 限额与第 (8) 步 SWRR 选路都不适用于控制面请求；晚拦截会白吃一次限流判定与一次路由计算 |

**落点（必须早于 (3) 的 `:396`）**：`request_filter` 开头新增一次前缀判定，命中则 `return tenant_api::dispatch(...).await`（`Ok(true)` 短路，Pingora 不再拨上游）；未命中则现有流程**零改动**。

**短路后的日志/指标语义**（容易踩）：`logging`（`proxy.rs:1029-1157`）只在 `ctx.tenant.is_some() && ctx.selected.is_some()` 时写用量记录与 `hydra_requests_total`。租户 API 请求要**设 `ctx.tenant`（便于归因）但绝不设 `ctx.selected`** → 不产生用量记录、不污染业务指标；状态码写 `ctx.status_code`。计费口径因此天然干净：**租户自助请求不进 `usage_record`，也不会被计入自己的 token 用量**（否则租户查用量会把查询本身算进去，形成自指噪声）。

### 3.3 身份模型：两个来源的裁决

| 来源 | 角色 | 裁决 |
|---|---|---|
| `Authorization: Bearer <tenant-token>` | **唯一权威** | 摘要 → 租户 id；未知/缺失 → `401 unauthorized`（fail-closed） |
| URL `/tenant/{tenant_id}/...` | 交叉校验位 | 与令牌归属不一致 → `403 tenant_id_mismatch` |
| `Host` 头 | **完全不参与** | 被忽略：不鉴权、不阻断、不计数、不记录（去写需求后不存在域名抢占面，Host 一致性指标沦为噪声，故**不引入**该指标——见 §2.3） |

补充规则：

1. **`tenant.enabled` 不作为任何闸门**。租户 API 只剩读与缓存删除，两者都不改变数据面行为；而停用租户的自救路径（清缓存、看状态、查用量）必须畅通——这正是 migration 0009 的既有语义（闸门在 admin token 之前、与 `enabled` 无关）。**这是去写需求后新规则变简单的一个例子**：v1 需要"读不闸、写闸"的不对称规则，现在只剩一条。
2. **令牌未配置**（`access_token_hash IS NULL`）→ 该租户永远 401（fail-closed，与 `admin/handlers.rs:1716-1719` 的既有注释一致）。
3. **`{tenant_id}` 存在但令牌不属于它** → 403 而非 404：租户 id 不是秘密（它就在租户自己的 base URL 里），403 不泄漏任何东西，而 404 只会让排障变难。
4. **令牌有效但租户在快照里不存在** → 403 `tenant_not_found`。理论上不该发生（令牌来自快照），但 revision 竞态下可能：令牌索引与 `tenants_by_id` 来自**同一次快照读**，因此实现上要**在同一次 `snapshot()` / `replication()` guard 内**完成两件事，不允许分两次读（否则会读到跨版本的数据）。

### 3.4 令牌索引：从 DB 移到快照

**问题**：`tenant_id_for_token` 今天每次尝试都全表读（D1），且只存在于 `AdminState`（需要 `pool`）。数据面 `AppState` **没有任何 DB 字段**（`proxy.rs:108-128`），edge 节点 `pool = None`（`main.rs:285-293`）。

**方案**：租户令牌摘要**已经**在集群快照里：

| 环节 | 证据 |
|---|---|
| 类型 | `cluster/content.rs:29` `pub type TenantTokenHashes = Vec<(String, String)>;`（`tenant_id → SHA-256 hex`） |
| 承载 | `cluster/content.rs:46` `FidelityRows::tenant_token_hashes` |
| 可达性 | `cluster/content.rs:86` `ReplicationContent::fidelity()`；`store.rs:357-359` `ConfigStore::replication()` |
| leader 侧构建 | `store.rs:318-325`（`load`）／`store.rs:468-482`（`reload_all_with` 每次重建，先落盘后发布） |
| edge/standby 侧填充 | `store.rs:424-434`（`apply_snapshot` 用 wire 的 fidelity 重建 content） |
| 明文可用性 | wire 上**密封**（`snapshot.rs:238-243`），hydrate 时**解封**（`snapshot.rs:325-329`） |

于是 `store.replication().fidelity().tenant_token_hashes` 就是每个角色本地都有的"令牌表"。校验算法：对 presented token 算一次 SHA-256 摘要（64 hex），对表中每行做**常数时间比较**，命中即返回其 `tenant_id`。

| 性质 | 结论 |
|---|---|
| DB I/O | **0**（原来：每请求一次全表 SELECT） |
| 角色覆盖 | leader / standby / edge **全部**可用。edge 在首次 `apply_snapshot` 之前 `replication()` 为 `None` → `503 not_ready`（fail-closed，绝不放行） |
| 一致性 | 与配置快照同一版本；管理口写令牌 → `reload_all` → 新 content → 所有节点随快照对齐。响应带 `config_version` 供租户判断"平台看到的是哪一版" |
| 无失效窗口 | 直接读 content，不引入第二份索引、不需要快照 hook 排序、不存在"索引落后于快照"的窗口（刻意：**不新增第二个 owner**） |
| 成本 | O(有令牌的租户数) 次 64 字节比较；1000 租户 ≈ 64 KB 比较，微秒级。若将来实测成热点，可在 `ConfigStore` 内挂 `Arc<HashMap<[u8;32], String>>`（同一 content 派生、同一 hook 重建）——属优化，非 v1 需要 |
| 时序 | 对**所有**候选行做常数时间比较，成功路径也不因提前返回而泄漏"哪个租户匹配" |

**这是本设计里最重要的一处简化**：令牌闸门从「`&AdminState` + DB」变成「`&ConfigStore` + 内存」，因此数据面可直接调用它，**且 edge 也能鉴权**——这正是 §6 能取消全部转发的前提。

### 3.5 端点清单

base URL：`{数据面}/tenant/{tenant_id}/api/v1`

| # | 方法 + 路径 | 用途 | 数据来源 | 需要 DB | 需要在 leader 执行 |
|---|---|---|---|---|---|
| E1 | `GET /whoami` | 只读自述：我是谁、平台当前怎么看我 | 配置快照 | ❌ | ❌ |
| E2 | `POST /auth/cache/invalidate` | 强制清除一个或多个 api-key 的认证缓存（空 = 全租户），**全集群生效并可确认** | 本地 `AuthCache` + 失效流 + 收敛屏障 | ❌ | ❌ |
| E3 | `GET /usage` | 查询某时间戳之后的 token 用量 | 计量存储（SQLite / ClickHouse） | ✅ | ❌ |

**两个端点的最后两列都是 ❌，但原因不同、都很关键**：

- **E2 不需要 leader**：缓存**本来就是每节点各一份**的，"清全集群"从来不是靠把请求送到某个权威节点，而是靠**扇出**。它需要的是"让每个节点都执行一次本地删除，并确认它们都执行了"——由失效流 + 收敛屏障完成（§4.2.2/§4.2.3），**不向对端发任何请求**。
- **E3 不需要 leader**：它是**读**。集群下计量存储是**共享的 ClickHouse**，任何节点查到的都是同一份数据；单节点下 SQLite 就在本地。

因此 v1 **不存在任何转发路径**（§6.2）——也就没有集群令牌、内部端点、循环守卫与新的信任边界。

统一约定：

- `Content-Type: application/json`；所有响应带 `X-Hydra-Trace-Id`（复用 `proxy::new_trace_id`，`proxy.rs:310-320`）。
- 错误体对齐 `design.md` §13.4 的既有信封 `{"error":{"code","message","trace_id"}}`。**不复用** `proxy.rs:1302-1309` 的 `short_circuit`（它产出 `{"error":{"message","type":"proxy_error"}}`，字段不同）——用新的 `respond_json`（形制照抄 `respond_catalog`，`proxy.rs:1280-1297`）。
- 只有 E2 有请求体；上限复用管理面量级（`MAX_ADMIN_BODY_BYTES = 1 MiB`，`admin/handlers.rs:208`）。
- 全部端点幂等（E1/E3 天然幂等，E2 重复执行结果相同）。

---

## 4. 端点详设

### 4.1 E1 `GET /whoami`（只读，纯快照）

```jsonc
// 200
{
  "tenant_id": "t_abc",
  "name": "Acme",
  "enabled": true,
  "domain": "acme.example.com",      // 平台当前把哪个 Host 归给我（只读）
  "auth_url": "https://auth.acme.example/verify",  // 平台向哪个地址校验我的客户端 key（只读）
  "config_version": 42,
  "base_url": "/tenant/t_abc/api/v1"
}
```

- **数据来源：配置快照，零 DB I/O。** 因此 leader / standby / edge 三种角色**行为完全一致**，不存在 `not_ready`（除 edge 首次快照前 `replication()==None` → 503）。
- 需要"tenant_id → Tenant"的索引：今天 `ConfigData` 只有 `tenants_by_domain: HashMap<String, Tenant>`（`hydra-core/src/config.rs:38-39`，以 **domain** 为键）。**新增派生索引 `tenants_by_id: HashMap<String, Tenant>`**，由**同一个 loader** 从同一批行构建（与 `models_by_key` 之于 `provider_models` 同构：派生索引不是第二个 owner，它是同一数据源的第二种访问路径）。替代方案是线性扫描 `tenants_by_domain.values()`，O(N) 也可接受；推荐派生索引，顺带让 `whoami` 与"租户存在性"判定都是 O(1)。
- **绝不返回**：`access_token_hash`、证书私钥、任何 provider key。
- `auth_url` 与 `domain` 是**租户自己的、非机密的配置**，返回它们是支持排障的关键（"平台认为我在哪个域名上？"是最常见的工单问题）；它们是**只读展示**，不是可写资源。
- `config_version` 用于跨节点/跨快照的一致性判断（edge 未同步时该值落后）；`version()` 在 edge 首帧前返回 0（`store.rs:381-385`），语义即"本节点还没有任何内容"。
- **与 E2/E3 的一致性**：E1 幂等且便宜，文档建议客户端在排查时先调它。

### 4.2 E2 `POST /auth/cache/invalidate`（**全集群清除，且可确认**）

```text
POST /tenant/{tid}/api/v1/auth/cache/invalidate?wait=converged&timeout_ms=2000
{ "api_keys": ["sk-live-aaa", "sk-live-bbb"] }   // 可选；缺省 / 空数组 = 清空本租户全部缓存

→ 200  （已在**全部数据面节点**生效）
{
  "invalidated": 2,          // 本节点 L1 实际删除条目数
  "checked": 2,              // 本次处理的 key 数
  "tenant_id": "t_abc",
  "scope": "keys",           // keys | tenant
  "fleet": {
    "state": "applied",      // applied | pending | single_node | unavailable
    "nodes_total": 5,        // 注册表中的**存活**数据面节点数
    "nodes_applied": 5,
    "lagging": [],           // 尚未确认的 node_id（state=pending 且有落后节点时非空；nodes_total=0 时为空）
    "event_id": "1758096000123-0",
    "waited_ms": 37
  }
}

→ 202  （已发布，但**未在超时内**全量生效 —— 语义是"已接受，未完成"）
{ ...同上..., "fleet": { "state":"pending", "nodes_total":5, "nodes_applied":4,
                        "lagging":["edge-3"], "waited_ms":2000 } }

→ 200  （单节点部署：本地即全部）      "fleet": {"state":"single_node","nodes_total":1,"nodes_applied":1}
→ 503 + fleet.state = "unavailable"  失效通道不可用（今天会静默报 true —— D3）。**注意：没有独立的 error.code**——响应体仍是正常的 InvalidateView，失败由 fleet.state 表达（实现即如此；早期草稿写的 `fleet_invalidation_unavailable` 这个码从未存在）
→ 400 too_many_keys                  超过 1000 个
→ 400 invalid_api_key                单个 key 超 4096 字节
→ 429 rate_limited                   见 §5.1
```

**为什么状态码是 200/202/503 而不是一个布尔字段**：HTTP 已经有"已接受但未完成"的语义（202）。用一个永远为 `true` 的 `published` 字段表达同一件事，就是 D3 的成因。**默认 `wait=converged`**，因为本端点的全部意义就是"让封禁立刻在整条数据面生效"；调用方若要 fire-and-forget 可传 `wait=none`（立即 202，带 `event_id` 供后续对账）。

#### 4.2.1 本节点侧（全部复用，无新逻辑）

| 环节 | 复用对象 | 证据 |
|---|---|---|
| 校验/上限 | `MAX_INVALIDATION_KEYS=1000`、`MAX_API_KEY_LEN=4096`、`invalidate_shape_error` | `admin/handlers.rs:1551-1587` |
| 精确 key 失效 | `HttpAuthChecker::invalidate(tenant_id, keys)` → L1 删除 + **L2 `DEL`** | `http.rs:700-702` → `http.rs:245-259`、`redis/auth_cache.rs:102-106` |
| 全租户失效 | `HttpAuthChecker::invalidate_tenant(tenant_id)` → L1 `retain` + L2 `del_tenant` | `http.rs:704-706` → `http.rs:291-300`、`redis/auth_cache.rs:109-117` |
| 发布（**发布边界自动哈希**，线上只有摘要） | `InvalidationStream::publish` | `events.rs:63-86` |
| 在途判定竞态 | `AuthCache::epoch` 守卫 | `http.rs:125-142` |
| 指标钩子 | `record_auth_cache_size(auth.cache().len())` | `admin/handlers.rs:1793` |

**本节点的 L2 删除是同步的**，所以"本节点 + 任何寒冷节点"在 E2 返回时已确定失效（L1 未命中会去 L2，而 L2 已空 → 回源）。**唯一的缺口是其他节点内存里的 L1 命中项**——这正是 §4.2.4 要解决的。

#### 4.2.2 全量生效需要三层（**这是本端点的核心设计**）

远端节点的 L1 命中项**不会**因为本节点删了 L2 而消失（L1 命中根本不查 L2）。所以"在所有数据面端点上都清掉"必须由三层共同保证：

| 层 | 机制 | 保证 | 延迟 |
|---|---|---|---|
| **L1 扇出** | 失效流事件 + 既有消费者 `apply_invalidation`（`events.rs:233-265`）→ 每个节点 `invalidate_hashes(tid, keyhashes)` 删本地 L1 | 事件被消费的节点全部清除 | 消费者空闲轮询 ≤500ms（`events.rs:272`） |
| **L2 权威** | 发布方同步 `DEL` 共享 Redis 的 L2 项（`redis/auth_cache.rs:102-106`） | 所有**寒冷**节点（无 L1 项）下次请求必然回源；且 L1 过期后不会从 L2 复活陈旧判定 | 立即 |
| **L3 收敛屏障**（**新增**） | 每个节点把自己的"已应用到的事件 ID"发布到共享 Redis；发布方等待**注册表中全部存活节点**确认 | 调用方**知道**是否真的全量生效，而不是"我发出去了" | 典型 <100ms，上限 = `timeout_ms` |

**三层缺一不可**：
- 只有 L1+L2（今天的设计）：调用方拿到的 `published: true` 只表示"进了流"，远端可能几分钟内仍在放行被停用的 key。
- 只有 L3 而没有 L2：寒冷节点会从 L2 复活陈旧 allow（`http.rs:309-311` 的注释记录了这个 B2 缺陷的成因）。
- 只有 L2 而没有 L1 扇出：持有 L1 命中的节点完全不受影响。

#### 4.2.3 收敛屏障（L3）的机制

**问题（今天的具体缺陷）**：消费者的 `last_id` 只存在于 `spawn_invalidation_consumer` 的循环局部变量里（`events.rs:293` 声明 / `:311` 推进），**从不对外发布**。因此整个系统里没有任何一处能回答"集群现在清到哪了"——`published: true` 是唯一信号，而它与"已生效"无关。

**机制（全部放进既有的 `InvalidationStream`，不新增子系统）**：

| 元素 | Redis 键 | 写入方 | 读取方 |
|---|---|---|---|
| 每节点已应用水位 | `hydra:{ctl:inv:applied}`（**一个 HASH**：`node_id → last_applied_event_id`） | 每个节点的失效消费者（**每批一次 HSET**，不是每事件） | E2 的等待循环 |
| （沿用）失效流 | `hydra:{ctl:events}` | 发布方 `XADD` | 各节点消费者 |
| （沿用）generation | `hydra:{ctl:gen}` | 裁剪任务 | 各节点消费者 |

- **屏障令牌 = `XADD` 返回的流 ID**（如 `1758096000123-0`）。用它而不是自增计数器，是因为它**由 Redis 在写入时分配**，因此"事件存在"与"令牌已分配"是同一个原子动作，不存在"令牌可见但事件还没落地"的窗口。比较按 `(ms, seq)` 数值对，不按字符串。
- 消费者在**成功应用一批之后**，把该批的最大 ID 一次性 HSET 上去；顺序是**先 apply 后 ack**（绝不允许先 ack——那会让屏障说谎）。
- **generation bump 路径（裁剪删除任何条目即触发）的 ack 规则**：`clear_all()` 会清掉**所有**缓存项，因此它**取代**了任何在它之前已发布的事件（注意：触发条件是"裁剪删除了任何条目"，不是"丢了未读条目"）；但节点**不能**为它没读到的事件 ID 记账。因此该路径只做两件事：① `clear_all()`；② **不推进水位**。后果是相关事件会以 `lagging` 出现（`state: pending`），而**不**是假报 `applied`。这是有意的保守选择：**宁可报未收敛，也不谎报已收敛**。§4.2.5 给出该情形下的硬上界。
- 等待循环：`publish` → 取 event_id → 读注册表存活节点列表（`registry.list_nodes`）→ 轮询 HASH，直到 `∀ live node: applied >= event_id` 或超时。**只等存活节点**：已死节点不在注册表里，也不在给它们送流量的负载均衡池里。**空集不是"已收敛"**：`list_nodes` 读的是本节点自己注册进注册表的那个 hash，空结果意味着视图陈旧/尚未填充（启动窗口、或注册表行已过期但 Redis 仍可连），而不是"没有对端"——因此空存活节点集**不**判为 `applied`，屏障返回 `pending` 且 `nodes_total: 0`（`lagging: []`）。
- **不是转发**：整条屏障只读共享 Redis 的 HASH，**不向对端发任何请求**，因此 §6.2 "本设计不需要任何转发"的结论不变（也就不需要集群令牌、内部端点或新信任边界）。

#### 4.2.4 契约里必须写清的四条语义

1. **调用顺序：先封禁，后失效。** 失效只删除缓存项；下一次请求会**立即回源**（`http.rs:537-547` → `:559-566`）。若租户尚未在自己的 auth 服务里封禁该 key，回源返回 allow 并**重新缓存**——等于"越清越糟"。正确顺序：**租户侧封禁 → 调 E2 → 确认 `state: applied`**。
2. **`invalidated: 0` 不等于失败**：它只统计**本节点 L1 命中数**（`http.rs:250-259`）；目标 key 可能只在 L2、只在别的节点的 L1、或从未被缓存——这些情况下"下次请求本来就会回源"。并列返回 `checked`（D4）。
3. **`state` 四态取代会说谎的 `published`**（D3）：`applied`（全部存活节点已确认，200；空存活节点集不判 `applied`，返回 `pending`，见 §4.2.3）/ `pending`（超时未收敛，202，`lagging` 列出未确认节点）/ `single_node`（本节点即全部数据面，200）/ **`unavailable`（通道存在但发布或水位读取失败，503）**。**"无失效流"归 `single_node` 而不是 503**：`main` 只在没有 Redis 主干时把流留成 `None`，而 leader/edge 缺它**拒绝启动**，所以"无流"只可能是单节点 `all` 角色；"集群成员但无通道"在运行期不可构造（早期的 C2 措辞已按此修订，见 §10.3）。
4. **覆盖范围 = 本集群**（已确认部署形态：**单一集群，半年内无多集群/多地域计划**）。一次 E2 覆盖**共享同一失效主干（同一个 Redis）**的全部数据面节点；`nodes_total`/`nodes_applied`/`lagging` 三个字段共同回答"清干净没有、还差谁"。

   > 因此 **v5 删除了 `HYDRA_CLUSTER_ID` 与 `fleet.cluster` 字段**：只有一个集群时，"刚才清的是哪一片"没有可回答的对象，留下就是一个永远为空的字段（反熵：不为假想需求保留机制）。
   >
   > **重新评估的触发条件**（与 §6.4 A-1 同款写法）：一旦引入第二个数据面集群/地域且各自使用独立 Redis，必须重新评估 —(a) 响应是否需要一个范围声明字段、(b) 是否要求多集群共享失效主干（顺带共享 L2，收益叠加）、(c) 租户契约是否要写明"逐集群调用"。该约束在设计期已识别，不是被遗忘。

#### 4.2.5 残余窗口：必须给出**有限的、且不由租户决定**的上界

L1 扇出 + L3 屏障覆盖"存活 + 在消费"的节点。仍然存在一类节点：**活着、在收流量，但它的消费者没在推进**（消费者任务 panic 后未重启、与 Redis 网络分区、节点未注册因而不在 `nodes_total` 里）。这类节点的 L1 命中项不会被清除，其陈旧窗口**只由 L1 的 allow TTL 决定**。

而今天的 allow TTL **可以被租户抬高到任意值**：`expires_in` 覆盖默认 TTL（`http.rs:661-674`），`clear_all` 的注释自己承认"a tenant auth service can raise well beyond the 300 s default via `expires_in`"（`http.rs:309-311`）。也就是说：**在今天的系统里，"某个数据面节点上这条 key 的封禁何时生效"是由被停用的租户自己决定的。** 这显然不是可以接受的语义。

因此新增一个不做可选项的硬上界：

| 配置 | 默认 | 语义 |
|---|---|---|
| `HYDRA_AUTH_ALLOW_TTL_MAX_SECS` | `300` | L1/L2 的 **allow** 缓存项 TTL 上限。`expires_in` 请求更长的 TTL 时按此封顶（`deny` TTL 不受影响，它本来就短）。**默认值已确认**（§11 已决）：300s = 现状值 ⇒ 对不设 `expires_in` 的租户零行为变化，残余窗口上界 ≤300s |

- **默认值 = 现有默认 allow TTL 300s**（`http.rs:414-423`）→ **对不设 `expires_in` 的租户零行为变化**。
- 代价：设了长 `expires_in` 的租户会产生更多 `auth_url` 回源流量。这是**运维显式做出的取舍**，必须在 `ops.md` 写明并给出调参建议（`HYDRA_AUTH_ALLOW_TTL_MAX_SECS` 越小时封禁生效越快、认证服务压力越大）。
- 收益：**任何**节点上的陈旧 allow 都有一个与"租户是否配合"无关的上界。E2 的 `state: applied` 覆盖正常情形，这个上限覆盖所有异常情形。

#### 4.2.6 可观测性（"到底清干净没有"必须可回答）

新增指标：`hydra_invalidation_consumer_applied_id`（per-node gauge，消费者水位）、`hydra_invalidation_consumer_lag_events`（流尾与本节点水位之间的事件数）、`hydra_invalidation_consumer_stalled_seconds`（水位多久没推进 → 这就是上面那类"活着但不消费"的节点的告警口径）、`hydra_tenant_api_invalidate_pending_total`（返回 202 的次数）、`hydra_tenant_api_invalidate_converge_seconds`（收敛耗时直方图）。

`ops.md` 要加一条告警规则：**`consumer_stalled_seconds > 60` 即告警**——今天这个故障是完全不可见的（消费者只是 `warn!` 后重试，`events.rs:336-338`）。

#### 4.2.7 为什么不做"按前缀清"

`AuthCache` 的键是 `(tenant_id, sha256(api_key))`（`http.rs:59`、`:192-194`），**内存与 Redis 里都只有摘要**（L2 key 为 `hydra:{auth}:{tenant}:{keyhash}`，`redis/auth_cache.rs:19-21`）。摘要不可前缀匹配，因此"清掉所有 `sk-proj-abc*` 的缓存"**在当前数据结构下不可实现**（与上面 L1 扇出无关：源数据里就没有前缀）。

| 路径 | 内容 | 代价 |
|---|---|---|
| P1（推荐 v1） | 不支持；契约明说"只接受完整 key 或全租户" | 0 |
| P2（独立特性） | 鉴权时额外维护 `tenant → DashSet<(prefix, hash)>` 前缀索引；E2 接受 `api_key_prefixes` 做有界扫描 | 新索引的同步失效负担 + 每请求一次插入 + 扫描配额 |

注意：`provider_key_binding.key_prefix`（`migrations/0006_provider_key_binding.sql`）证明平台**在路由侧**确实做前缀匹配——那是因为路由侧看得到原始 key；缓存侧看不到。这个区别必须写进文档，否则会被反复提问。

### 4.3 E3 `GET /usage`（某时间戳之后的 token 用量）

**v1 在两种部署形态下都可用**：单节点读 SQLite、集群读 ClickHouse（§4.3.3）。这是**首发的硬要求**，不是"第二刀"。

```text
GET /tenant/{tid}/api/v1/usage?since=2026-09-16T00:00:00Z&until=2026-09-17T00:00:00Z&group_by=model
→ 200 {
  "tenant_id": "t_abc",
  "since": "2026-09-16T00:00:00Z",
  "until": "2026-09-17T00:00:00Z",
  "as_of": "2026-09-16T23:59:57Z",     // 本次查询窗口内最新一条记录时间（= 窗口内 MAX(created_at)；默认 until=now 时即存储最新）；无记录时为 null
  "totals": {"requests":1234,"tokens_in":456789,"tokens_out":12345,
             "cache_hit_tokens":9999,"errors":7},
  "rows": [{"key":"gpt-4o","requests":800,"tokens_in":300000,"tokens_out":9000,
            "cache_hit_tokens":5000,"errors":2}, ...],
  "source": "clickhouse"               // sqlite | clickhouse（本次应答来自哪一个计量存储）
}
→ 400 invalid_since / invalid_until    形态非法或 since > until
→ 400 invalid_group_by                 group_by 不在白名单
→ 400 window_too_large                 窗口超上限（默认 31 天）
→ 503 usage_store_unavailable          计量存储不可达，或响应无法解码，或结果超过响应体上限（CH 连接失败/超时/形状漂移；本节点无本地库；或结果 > ~64 KiB 被截断——后者应缩小窗口/分组，重试无效）
```

**入参约定**（T8 落地，均为对外契约的一部分）：
- `since` **必填**；`until` **可省略，省略即"服务端当前时刻"** —— 需求原文是"获取某个时间戳以后的用量"，因此单参数调用必须可用。
- 时间戳接受 RFC3339（`Z` 或带偏移）、**无时区的 `T` 分隔形态**、**空格分隔形态**（历史遗留输入）、epoch 秒与 epoch 毫秒；一律归一化为规范形态后再入查询，应答里回显的是**归一化后**的值。
- `group_by ∈ {none(默认), model, provider, day}`；未知值 400，**绝不插值**（列名来自白名单）。
- **契约里不存在 `tenant_id` 参数**：身份只来自令牌，多传该参数被**忽略**（不是报错，因为它是无害的多余输入）。
- 解码失败、存储不可达、结果超限对租户是**同一个 503**，但**指标标签不同**（`result=decode_error` / `store_unavailable` / `result_too_large`）——运维据此发现"存储答了但答的不是我们要的形状"，或"结果太大被截断"（后者让调用方缩小窗口/分组，重试无效）。

> 与 v3 相比：**`501 usage_query_unavailable` 分支被删除**（CH 读路径进 v1）。原来的"防御分支 `Sqlite + pool=None`"也改由同一码 `usage_store_unavailable` 表达——它分不清"配置里没有 store"与"store 连不上"，但对租户而言两者的正确反应相同（重试/找运维），无需暴露内部拓扑。

#### 4.3.1 两个后端的查询（**ClickHouse 版本已对活实例实测**）

| 项 | SQLite | ClickHouse |
|---|---|---|
| 表 | `usage_record`（`0001_init.sql:85-99`，`0002`/`0005` 改列） | `usage_record`（`environment/clickhouse/init.sql:15-31`） |
| 索引 | `idx_usage_record_tenant(tenant_id, created_at)`（`0001_init.sql:101`）—— 等值+范围，正好匹配 | 主键 `ORDER BY (created_at, tenant_id, provider_id)` —— **`created_at` 是前导键** |
| 索引是否被用 | 是（等值前导 + 范围次列） | 是。`EXPLAIN indexes=1` 实测输出 `PrimaryKey Keys: created_at, tenant_id` / `Condition: and((created_at in [...]), (tenant_id in ['x','x']))` / `Granules: 1/2` |
| 时间列 | `TEXT`，应用显式绑定 `%Y-%m-%dT%H:%M:%SZ` | `String`，同一个值（实测库内真实行为 `"2026-09-15T07:32:06Z"`） |
| 空集语义 | `COUNT(*)=0`、`SUM`→`NULL`（需 `COALESCE`）、`MAX(created_at)`→`NULL` | `count()`→`"0"`、`COALESCE(sum(..),0)`→`"0"`、**`MAX(created_at)`→`""`（空字符串，不是 null）** |
| 参数绑定 | `?` 占位 | **`{name:String}` + `param_<name>` URL 参数**（见下） |

**SQLite 参考 SQL**（用 `sqlx::query` 运行时校验风格，与 `sink.rs:383-384` 一致 → **无需重跑 `cargo sqlx prepare`**）：

```sql
SELECT COUNT(*)                           AS requests,
       COALESCE(SUM(tokens_in), 0)        AS tokens_in,
       COALESCE(SUM(tokens_out), 0)       AS tokens_out,
       COALESCE(SUM(cache_hit_tokens), 0) AS cache_hit_tokens,
       COALESCE(SUM(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END), 0) AS errors,
       MAX(created_at)                    AS last_seen
FROM usage_record
WHERE tenant_id = ? AND created_at >= ? AND created_at < ?;
```

**ClickHouse 参考 SQL（实测可跑，返回形状已确认为 JSONEachRow 单行）**：

```sql
SELECT count()                                    AS requests,
       COALESCE(sum(tokens_in), 0)                AS tokens_in,
       COALESCE(sum(tokens_out), 0)               AS tokens_out,
       COALESCE(sum(cache_hit_tokens), 0)         AS cache_hit_tokens,
       COALESCE(sum(if(status_code >= 400,1,0)),0) AS errors,
       MAX(created_at)                            AS last_seen
FROM usage_record
WHERE tenant_id = {t:String} AND created_at >= {s:String} AND created_at < {e:String}
FORMAT JSONEachRow
```

实测输出（真实 8 行数据）：`{"requests":"8","tokens_in":"247","tokens_out":"18","cache_hit_tokens":"0","errors":"0","last_seen":"2026-09-15T07:32:06Z"}`

**分组查询是第二条独立语句**（**不是**一条 `UNION ALL`，理由见 CH-G）：

```sql
SELECT model_key AS key, count() AS requests,
       COALESCE(sum(tokens_in),0) AS tokens_in, COALESCE(sum(tokens_out),0) AS tokens_out,
       COALESCE(sum(cache_hit_tokens),0) AS cache_hit_tokens,
       COALESCE(sum(if(status_code >= 400,1,0)),0) AS errors
FROM usage_record
WHERE tenant_id = {t:String} AND created_at >= {s:String} AND created_at < {e:String}
GROUP BY key ORDER BY key FORMAT JSONEachRow
```

实测输出 `{"key":"Qwen/Qwen2.5-7B-Instruct","requests":"8","tokens_in":"247","tokens_out":"18","cache_hit_tokens":"0","errors":"0"}`（**没有 `last_seen` 列**，因此由"仅行"解码器解析）；无匹配行时实测返回**空体**。

`group_by=provider` 用 `provider_id`，`group_by=day` 用 `substr(created_at,1,10)`（定宽格式下是安全切片，两侧通用，且两侧都不对列套函数——套函数会同时改变语义并放弃主键裁剪）。

其他写入路径事实（两侧共享）：批量 flush，`batch_size` 默认 256 / `flush_secs` 默认 5（`sink.rs:941-943`）；`record()` 非阻塞、丢弃即计数（`sink.rs:316-335`）；**一行 = 一次成功选中 provider 的请求**（`proxy.rs:1101-1103`），选路前失败不进表；**无任何裁剪**（`MAX_RETAINED = 10_000` 只拒绝尚未写入的记录，`sink.rs:90-93`）；token 三列可空。

#### 4.3.2 必须写进契约的格式/一致性约束（四个，均已实测验证）

1. **入参归一化**：接受 RFC3339（`Z` / 带偏移）与 epoch 秒/毫秒；**服务端统一归一化为 `%Y-%m-%dT%H:%M:%SZ`（UTC）** 再作为查询参数。**归一化的 owner 在 shell**（`tenant_api/time_bound.rs`，用 `chrono`）：`hydra-core` 无 `chrono` 且依赖白名单不允许引入，因此 core 只做**规范形态**的字形/范围校验与顺序比较，词法解析与窗口长度判定留在 server 侧（§7.1）。
2. **绝不在两侧对 `created_at` 套函数**。SQLite 的 `datetime('now')` 产出空格分隔形式，而库内是 `T…Z`；`'T'(0x54) > ' '(0x20)`，故用空格形式比较会对**当天任意时刻**的行都成立 → 静默多算一整天。CH 侧同理（`toDateTime(created_at)` 不仅改变语义，还会**放弃主键裁剪**）。唯一正解：把边界格式化成与列相同的定宽 UTC 字符串再比较（同长度、同 `T`/`Z`、全 UTC ⇒ 字典序 == 时间序）。**实测确认**库内真实值就是 `"2026-09-15T07:32:06Z"`。
3. **`as_of` 的两侧差异**：CH 在无匹配行时 `MAX(created_at)` 返回 **`""` 而不是 null**（实测 `{"last_seen":""}`）。解析器必须把 `""` 映射为 `null`，否则租户会看到一个空的 `as_of` 字符串。SQLite 侧则天然是 `NULL`。
4. **一致性窗口自述**：批量 flush（≤256 条或 ≤5s）意味着最近 ≤5s 的记录可能尚未落库；后端故障时最多 `MAX_RETAINED` 条滞留内存。响应必须带 `as_of`，并让 `hydra_usage_records_dropped_total{reason=...}` 的四种丢弃（`channel_full`/`channel_closed`/`retention_cap`/`shutdown_unflushed`，`sink.rs:143/166/311/325`）成为**欠算下界**的可观测声明，而不是让租户以为数字完备。

#### 4.3.3 ClickHouse 读路径（**v1 必做**）：实测事实与三个必须避开的陷阱

**实测环境**：仓库自带的 `hydra-local-clickhouse`（ClickHouse 24.3.18.7，`127.0.0.1:8123`，`environment/clickhouse/init.sql` 建表，表内 8 行真实数据）。以下全部是**跑出来的**，不是从文档推断的：

| # | 实测事实 | 如果不处理的后果 |
|---|---|---|
| **CH-A** | `SELECT toUInt64(12345) FORMAT JSONEachRow` → **`{"n":"12345"}`**。**64 位整数默认被序列化成 JSON 字符串**（`output_format_json_quote_64bit_integers=1`）；加 `SETTINGS output_format_json_quote_64bit_integers=0` 才得到 `{"n":12345}`。`count()` 同样是 `"1"` | **这是本端点最危险的陷阱**：`tokens_in`/`count()` 全是 UInt64。若解析器按 JSON number 解，或在字符串上取 `as_u64()` 得 `None` 再默认成 0，就会返回 `{"requests":0,...}`——**一个语法正确、语义错误的 200**，租户据此认为"这段时间没有用量"。解析器**必须同时接受字符串与数字两种形态**（并按 `output_format_json_quote_64bit_integers` 两种取值写测试） |
| **CH-B** | 无匹配行时 `MAX(created_at)` → **`""`**（空字符串），而非 null | `as_of` 会变成空串而非 `null`（§4.3.2 第 3 条） |
| **CH-C** | 参数绑定：`{t:String}` + URL `?param_t=…` 实测把 `x' OR 1=1 --` **原样**返回为字面量 `x' OR 1=1 --` | 若改为手工拼接 SQL 字面量，就是把**租户可控输入**拼进外部数据库的查询里（SQL 注入）。**必须用 `param_*` 绑定，禁止手工转义** |
| **CH-D** | 查询失败 → **HTTP 404 + 纯文本 `Code: 60. DB::Exception: … (UNKNOWN_TABLE)`**（与写路径 `sink.rs:1207` 已断言的形态一致） | 只按 HTTP 状态判失败会把"业务错误"与"5xx 不可达"混为一谈；错误分类要读状态码 + `Code: N` |
| **CH-E** | 主键 `ORDER BY (created_at, tenant_id, provider_id)`；`EXPLAIN indexes=1` 显示谓词同时进 `PrimaryKey`，`Granules: 1/2`（**裁剪生效**）。但 `tenant_id` 是**第二**列 | 窄窗口裁剪很好；**31 天窗口**仍会扫描该窗口内全部租户的行，再按 `tenant_id` 过滤。因此窗口上限不是可选优化，而是必需（§5.1 反放大）。运维若要在规模上做大量按租户查询，可另开任务把 CH 表改成 `ORDER BY (tenant_id, created_at)`（需重建表 + 回填，**本次不做**） |
| **CH-F** | 写路径的 INSERT 重试会在"响应读超时"后**重发整批**（`sink.rs:207-231`），而 CH INSERT 不去重 | 存在**重复行** → `COUNT(*)` 高估 requests。且 CH 表**没有 `trace_id` 列**（列清单见 `init.sql:15-31`），无法按 trace 去重。契约必须把 `requests` 标注为"近似值"，并把这件事写进 `ops.md` |
| **CH-G**（T8 实测新增，**推翻了本节原先"一次查询、行在前总计在后"的假设**） | `UNION ALL` 的**分支顺序不保证**：同一语句（`SELECT … GROUP BY key UNION ALL SELECT …`）6 次里有 **1 次把总计行排在了最前**；补 `ORDER BY is_total` **无效**（`UNION ALL` 之后的裸 `ORDER BY` 只作用于最后一个分支）。对照实验：把两条语句**分开**执行，各自 6/6、3/3 稳定 | 按"最后一行是总计"做**位置解码**会读到**某一行分组的数字**当成整体总计——即"语法正确、语义错误"，且量级看起来正常（单分组时甚至完全一样）。**因此读侧必须发两次查询**（总计 + 分组，与 SQLite 臂同构），core 侧的"仅行"解码器 `decode_usage_rows_json_each_row` 因此新增 |
| **CH-H**（T8 实测新增，**只有活实例能发现**） | CH 的 HTTP 响应是 **`Transfer-Encoding: chunked`、且无 `Content-Length`**（实测响应头）。按 `\r\n\r\n` 切出"body"得到的是 `7C\r\n{…}`，JSON 解析报 `trailing characters at line 1 column 2` | 用 `Content-Length` 的替身（wiremock）**测不出来**：T8 的 26 条替身测试全绿，只有 `--ignored` 的活 CH 用例失败。传输层必须解 chunked（`clickhouse::response_body`），且这条事实必须由**活实例用例**守住 |

**传输选型（新决策 Q14，见 §11）**：CH 侧今天只有一条**裸 TCP 手写 HTTP** 的写通道（`sink.rs:711-734`，刻意不用 `clickhouse` crate，理由见 `sink.rs:428-437`）。读路径需要**真正的响应体读取 + JSONEachRow 解码**，而现有写通道只把响应体当错误文本丢弃（`sink.rs:801-811`）。两个选项：

| 选项 | 内容 | 取舍 |
|---|---|---|
| **T1（推荐）** | 把"CH 寻址 + 请求行/Basic 认证 + 状态行分类"抽成一个共享的 `clickhouse` 传输原语，写路径与新读路径共用；读路径在其上加 SQL 构造 + JSONEachRow 解码 | 一个 owner 管"怎么跟 CH 说话"，无新依赖，期限约定一致。代价：**要动正在工作的写通道**（回归风险），靠既有 `tests/clickhouse_sink.rs` 兜底 |
| T2 | 读路径直接用 `reqwest`（已是依赖，带超时/连接池） | 不碰写路径；但"怎么跟 CH 说话"就有**两套实现**（裸 TCP 写 + reqwest 读），URL/凭据/协议的解释会分叉 |

无论选哪个，**CH 的 URL/凭据解析必须只有一个 owner**：今天 `ClickHouseConfig` 是私有的、且在 `sink.rs:559-568` 被 move 进 flush task 闭包，读侧够不着 → 必须把它提成可复用的解析函数（同一次解析、两处使用），而不是在读侧再写一份 `parse_url`。

**读侧配置可达性**：`build_sink` 返回类型抹除的 `Box<dyn UsageSink>`（`sink.rs:963`），读侧无法下钻。因此读能力必须**单独注入**（见 §4.3.4），而不是给 `UsageSink` 加查询方法——`UsageSink` 的语义是"fire-and-forget 写入通道"（`sink.rs:39-63`），让它同时当查询服务会改变它的含义。

**分组查询的空结果**：CH 在无匹配行时对分组查询返回**空体**（实测 `''`），而不是空行或 null → "仅行"解码器必须把空体解成**空集合**（`Ok(vec![])`），而总计查询的空体则仍是形状漂移（它永远投影 `last_seen`）。两者语义不同，故分成两个解码函数。

**分组查询的确定性**：分组查询用 `GROUP BY key ORDER BY key`，分组行**顺序稳定**（6/6 实测）；但仍按既有 `rows_by_key` 做**顺序无关**比较，不把 SQL 排序当成契约。

#### 4.3.4 读能力如何进入 `AppState`（**取代 v3 的 `usage_backend` 枚举**）

v3 的做法是往 `AppState` 放一个 `usage_backend: UsageBackend { Sqlite, ClickHouse }` 枚举，E3 里按它分支，CH 分支回 501。**两个后端都进 v1 之后，枚举 + 分支就没有必要了**，而且它比"注入能力"更容易出错——分支写错就是那个"假 0"。

**改为注入一个能力对象**：

```
main.rs 启动时按 sink kind 选择唯一实现，注入 AppState：
  sink_kind == "sqlite"      → Arc<SqliteUsageQuery>      (pool: SqlitePool)
  sink_kind == "clickhouse"  → Arc<ClickHouseUsageQuery>  (cfg: 共享解析出的 CH 配置)
AppState.usage: Arc<dyn UsageQuery>
```

- E3 里**没有分支**：只有一个 `self.state.usage.aggregate(tenant_id, since, until, group_by)` 调用。
- "返回假 0"的风险从"运行时分支写错"降级为"启动时注入错实现"——单点、可被一条启动断言测试覆盖（§10.2 T14：断言注入的实现与 `sink_kind` 一致，两种组合各一条）。
- **为什么不能省掉这个字段、只按"有没有 pool"判断**（v3 的论据仍然成立）：集群下 leader 也**有**本地 SQLite（`main.rs:285-293` 只按 `role == Edge` 决定 pool），而那份 `usage_record` **一行都没写过**（sink 是 ClickHouse）→ 只按 pool 判断会让 leader 拿空表返回 `{"requests":0}`。
- **集群下也不需要转发**：ClickHouse 是**共享外部存储**，任何节点（含 edge）查到的都是同一份数据 → E3 在全部角色上可用，且与 §6.2"本设计不需要任何转发"的结论一致。
- `source` 字段由实现自身填写（`"sqlite"` / `"clickhouse"`），而不是由调用方推断——同一份能力对象同时回答"数据从哪来"。

#### 4.3.5 非目标：明细查询（留给第二刀）

若将来要 `/usage/records`：SQLite 侧用 `id INTEGER PRIMARY KEY AUTOINCREMENT`（`0001_init.sql:86`）做**游标分页**（`WHERE tenant_id=? AND id > ?`），**不要**用 `created_at` 做游标——列只有秒级精度，同一秒多行会漏/重。**CH 侧没有等价的单调自增列**（`init.sql:15-31` 无 `id`），因此 CH 的明细分页需要另一套游标（例如 `(created_at, provider_id)` 复合游标 + 去重语义），这也是把它列为独立特性的原因之一。同时要对 `client_api_key`（已脱敏，`proxy.rs:1105` 用 `mask_key`）是否需要回显做显式决策。

#### 4.3.6 与既有 `GET /api/v1/stats/usage` 的关系

运维看的是**进程内 prometheus 累计值**（`admin/handlers.rs:2106-2108` → `metrics.rs:760-800`），重启归零、无时间维度；租户看的是**计量存储里的持久行**（时间窗、可重复查询）。两者**必然对不上**（各自漏算不同：计数器丢"重启前"，行丢"选路前失败"与四种丢弃 + CH 重复行）。

**结论**：两者都保留，但职责写清并交叉引用——`/api/v1/stats/usage` = **进程健康度**（文档明确"非计费口径、重启归零"）；`/tenant/{tid}/api/v1/usage` = **对账口径**（持久行、时间窗，且 `requests` 在 CH 部署下是近似值）。**不改任何一方的口径**，避免制造第二套账。

### 4.4 端点与资源可用性矩阵

| 端点 | 单节点 `all`（sqlite） | 集群 `leader` active | 集群 `leader` standby | 集群 `edge` | `all` + `cluster-redis` 但无 Redis |
|---|---|---|---|---|---|
| E1 `whoami` | ✅ 快照 | ✅ 快照 | ✅ 快照 | ✅ 快照 | ✅ 快照 |
| E2 `invalidate` | ✅ 本地 | ✅ 本地 + 广播 | ✅ 本地 + 广播 | ✅ 本地 + 广播 | ✅ 本地（`broadcast: local_only`） |
| E3 `usage` | ✅ SQLite | ✅ **ClickHouse** | ✅ **ClickHouse** | ✅ **ClickHouse** | ✅ SQLite |
| 鉴权 | ✅ 快照 | ✅ 快照 | ✅ 快照 | ✅ 快照 | ✅ 快照 |
| **转发** | 无 | 无 | 无 | **无** | 无 |

> **三个端点在全部角色上都可用，且没有任何转发。** E3 在集群下读**共享的 ClickHouse**（每个节点都能直连，含 edge），在单节点下读本地 SQLite——这是"CH 读路径进 v1"（§4.3.3）的直接结果，也消除了 v3 唯一一处"集群弱于单节点"。最后一列的"无 Redis 集群"不是可运行形态（leader/edge 缺 `HYDRA_REDIS_URL` 直接拒绝启动，`main.rs:195-201`），指的是单节点 + 编译了 `cluster-redis` 但未配 Redis。

---

## 5. 安全设计

去写需求后，威胁模型从 5 个缩到 3 个。**下面明确列出被移除的两个，以及它们为什么真的消失了**（不是为了简洁而省略）：

| 已移除的威胁 | 原攻击链 | 为什么现在不存在 |
|---|---|---|
| 域名抢占 → 客户端凭证截获 | 租户调 API 抢占"空位"域名（平台自身主机名 / `localhost` / 运维尚未绑定的客户域名）→ 真实客户端按该 Host 发来请求 → Hydra 用该租户的 `auth_url` 鉴权 → **该租户收到这些客户端的原始 api-key**（`http.rs:558-566`） | 租户 API **没有任何**写 `tenant.domain` 的路径。`Host→租户` 映射只由运维的 `PUT /api/v1/tenants/{id}`（admin token）改变 |
| `auth_url` 写入 → SSRF | 租户把 `auth_url` 指向集群内服务/元数据地址，使网关成为被指定的 HTTP 客户端；或指向外部主机外泄凭证 | 同上：租户无法指定网关的出站目标 |

**残余风险**：管理 API 自身仍允许运维把 `auth_url` 指向任意地址（含内网）。这是**既有**的、由可信方操作的边界，不在本次变更内，但值得单列一个小改动（写时网段校验 + `POST /api/v1/tenants/auth/test` 的探活同样受益）。

### 5.1 威胁 T1：令牌爆破与资源放大

今天的令牌闸门零限流（D2），且每次尝试全表读（D1）。搬到公网数据面后两者都升级为可利用项。

| 措施 | 内容 |
|---|---|
| 索引化校验 | §3.4：0 DB I/O ⇒ 移除"未授权请求即 DB 放大器"这一条 |
| 失败限流 | 按**源 IP** 与**令牌摘要**两个维度固定窗口：默认 10 次失败/分钟 → `429` + `Retry-After`；持续超限 → 15 分钟锁定该维度（`HYDRA_TENANT_API_AUTH_FAIL_LIMIT_PER_MIN` / `_LOCKOUT_SECS`）。**B1：锁定只在鉴权失败路径生效，持有效令牌的请求永不被它拒绝**——代价是锁定窗内猜者可用 `429`（错令牌）/ `200`（对令牌）之差判断令牌真伪，即放弃了"锁定期间无有效性预言机"这一性质。接受此权衡的理由：≥16 字符令牌的边际猜测价值 ≈ 0，而 LB 后**跨租户 DoS**（同出口 IP 约 11 个坏令牌即可拖垮整条租户 API，且窗口滚动后可续）是具体威胁；且 `403`（URL 写错租户，§5.2）**今天已**暴露令牌有效性，故这并非新增的预言机面 |
| 已授权限流 | 按租户：默认 60 次/分钟（`HYDRA_TENANT_API_RATE_LIMIT_PER_MIN`），**计数所有通过的已认证请求**——包括随后被 API 以 4xx/5xx 拒绝的，唯一不计的是 `403`（URL 写错租户）；防止租户 API 被当作免费的 CPU/DB 消耗面 |
| 反放大 | E3 窗口上限（默认 31 天）+ `group_by` 白名单，避免一次请求全表扫描 |
| **失效风暴防护** | 针对 D6：E2 追加"每租户失效频率上限"（默认 10 次/分钟）；超限 429。三个理由：① 失效流裁剪**只要删除任何条目**就会 bump generation（≈ 每 30s 里事件数超过 `maxlen=10_000`，即约 **>333 事件/秒**持续 30s 即触发），进而让**每个节点清空整个缓存**（L1+L2）——与消费者是否落后**无关**（`events.rs:204-214/320-332`）；② 每次失效会让**所有**相关节点回源 `auth_url`，是租户自己认证服务的流量放大器；③ 收敛屏障本身要读 Redis（发布 + 轮询 HASH），高频调用会把共享主干变成热点——单个租户的凭证不得成为跨租户可用性武器 |
| 实现位置 | 独立小组件（`DashMap` 固定窗口；`cluster-redis` 下用 Redis 计数，使 edge 水平扩展时限额不被放大 N 倍，参照 `redis/rate_limit.rs` 的窗口原语）。**不复用** `proxy::limiter::Limiter`：它是 `LimitRole` 驱动的业务配额（`limiter.rs:25-63`），语义不同 |
| 常数时间 | 对所有候选摘要做常数时间比较（复用 `constant_time_eq` 的写法，`admin/handlers.rs:662-675`） |
| 令牌强度 | 沿用最短 16 字符（`admin/handlers.rs:240/853-860`）；文档给出 `openssl rand -hex 32` 的生成指引（既有话术） |

### 5.2 威胁 T2：跨租户越权

| 面 | 对策 |
|---|---|
| 身份 | 令牌 → 租户，客户端不可自述；URL `{tenant_id}` 必须一致（403） |
| 数据 | E1/E3 的 `tenant_id` **只**来自令牌，**绝不**接受 query/body 里的 tenant 参数（契约里不存在该参数） |
| 缓存 | E2 只能清自己：`invalidate(自有 tenant_id, keys)`；**不存在**"不带 tenant_id 就跨租户匹配"的分支（该分支只在管理端，`admin/handlers.rs:1617-1626`） |
| 写能力 | 租户 API **没有**任何配置写路径（§2.3）→ 不存在字段级越权（改 `enabled`/`name`/证书/令牌）的可能 |
| 快照一致性 | 令牌索引与 `tenants_by_id` 必须在**同一次快照读**内取（§3.3 规则 4），避免跨版本组合 |

### 5.3 威胁 T3：凭证与响应泄漏面

| 面 | 要求 |
|---|---|
| 日志 | 令牌明文**永不**入日志/指标标签/trace；只记 `tenant_id`（未知时无）+ 可选摘要前 8 位 |
| 用量记录 | 租户 API 请求不写 `usage_record`（§3.2 的短路序保证 `ctx.selected` 为空）——**同时避免自指噪声**：查用量不会污染用量 |
| 错误响应 | 401 不回显令牌；不区分"令牌不存在"与"该租户未配置令牌"（同码同文案） |
| E1 响应 | 不回显 `access_token_hash`、证书私钥、provider key；`domain`/`auth_url` 是租户自己的非机密配置，可回显（§4.1） |
| 失效流 | 既有实现已在发布边界哈希（`events.rs:63-86`），线上只有摘要；**新端点沿用，不新增明文通道**。既有提醒：v=1 的 `legacy_keys` 明文读取分支仍在（`events.rs:133-140`）——写 Redis 的能力等价于能注入明文 key，属既有边界，不在本次变更内 |
| 前端/文档 | `api-docs.js` 的租户页不得要求输入管理 token；示例不出现真实令牌形态 |

---

## 6. 集群与拓扑

### 6.1 三种角色的资源差异（逐条对齐）

| 资源 | `all`（单节点） | `leader` | `edge` | 证据 |
|---|---|---|---|---|
| 本地 SQLite | ✅ | ✅ | ❌ `pool = None` | `main.rs:285-293` |
| 数据面监听器 | ✅ | ✅ | ✅（**入口在这里**） | `main.rs:855-858`；`cluster.md` §4.2「代理入口指向 edge 的 8080」 |
| 管理面 CRUD | ✅ | ✅ | ❌（只留探活端点） | `admin/mod.rs:562-570` |
| `leader_ready` 句柄 | `None` | `Some(f)`（**仅 leader 构建**） | `None` | `main.rs:691-766` |
| 集群注册表 | `None` | ✅ | ✅ | `main.rs:359-433` |
| 失效流发布器 | `None` | ✅ | ✅ | `main.rs:621-644` |
| usage sink | `sqlite` 默认 | **必须 clickhouse** | 必须 clickhouse | `main.rs:263-269` |
| 快照 fidelity（含令牌摘要） | ✅ | ✅ | ✅（首次 `apply_snapshot` 后） | `store.rs:424-434`、`snapshot.rs:325-329` |

### 6.2 为什么本设计**不需要**任何转发

v1 需要转发，是因为"写配置"必须落到**持有租约的权威节点**（standby 也持有 pool，但那是副本库，写进去就是分裂脑——这正是管理面 `maybe_forward_mutation`，`admin/mod.rs:435-548` 要防的事）。

去写需求后，三个端点分别落在三种**不需要权威**的资源上：

| 端点 | 落在哪 | 为什么不需要权威节点 |
|---|---|---|
| E1 `whoami` | 配置快照 | 快照是**到处都有一份**的复制内容；任一节点的读都等价 |
| E2 `invalidate` | 本节点内存 + 失效流 | 缓存**是每节点各一份**的；"清全集群"本来就是靠广播实现的（`events.rs:233-265`），不是靠把请求送到某个节点 |
| E3 `usage` | 计量存储 | 单节点下 SQLite 在本地；集群下 ClickHouse 是**共享外部存储**——任何节点（含 edge）直连查到的都是同一份数据。**因为 v1 就实现了 CH 读通道，E3 在集群下天然全节点可用，不需要任何转发**（§4.3.3） |

因此**删除**：`cluster_registry` / `leader_ready` 进入 `AppState`、`forward_mutation` 复用、`x-hydra-forwarded` 循环守卫、`ForwardError` 的 502/504 三分类语义、管理口的租户 API 第二挂载点、`HYDRA_PUBLIC_URL` 语义问题、以及"转发目标陈旧 → `503 forward_loop`"这个风险项。**这是本次需求变更带来的最大结构性收益。**

> 一个必然会被重提的替代方案是"把 E2 转发到 leader 的 admin API，复用既有的租户端点"。该方案**技术上可行**，但**不产生全集群清除**（扇出由共享失效流完成，与哪个节点发布无关），且会清错节点、把入口节点推到异步路径上、并重新引入整套转发机制。完整论证见 **§6.4 决策记录 A-1**。

### 6.3 E2 的集群语义：扇出 + 屏障 + 硬上界

| 机制 | 覆盖 | 延迟 | 证据/归属 |
|---|---|---|---|
| 本节点 L1 删除 + 同步 L2 `DEL` | 本节点 + 所有**寒冷**节点（无 L1 项） | 立即 | `http.rs:245-259`、`redis/auth_cache.rs:102-106` |
| 失效流事件 → 各节点消费者 `invalidate_hashes` | **全部在消费的存活节点**的 L1 | ≤500ms（空闲轮询 `events.rs:272`） | `events.rs:63-86`、`:233-265` |
| 收敛屏障（新增）：每节点水位 HASH + 发布方等待 | **可证明**：`nodes_applied == nodes_total` | 典型 <100ms | §4.2.3 |
| `HYDRA_AUTH_ALLOW_TTL_MAX_SECS`（新增） | 消费者停摆/未注册节点的**残余窗口上界** | ≤ 该值 | §4.2.5 |
| generation bump → `clear_all()` | 裁剪**删除任何条目**即触发（非"仅当丢了未读条目"）的**兜底**（全清 L1+L2，代价高） | 同扇出 | `events.rs:204-213`（Lua `removed > 0`）、`:320-332` |

**关键的诚实声明（写进 `ops.md` 与 E2 的响应语义）**：

1. `state: applied` 的含义是"**注册表中全部存活节点的消费者都已确认应用**"。它**不**覆盖未注册的节点、消费者停摆的节点、以及与失效主干网络分区的节点——这些由 `HYDRA_AUTH_ALLOW_TTL_MAX_SECS` 兜底。**空存活节点集也不判 `applied`**：`list_nodes` 读的是本节点自注册的注册表 hash，空结果 = 视图陈旧/未填充（而非"没有对端"），空集上的"∀ 已确认"是空真——屏障此时返回 `pending`（`nodes_total: 0`）而非 `applied`。
2. `state: pending` 是**诚实的未收敛**，不是错误：`lagging` 列出未确认的 node_id，运维可据此定位（配合 `hydra_invalidation_consumer_stalled_seconds` 告警）。**不因为裁剪丢弃的事件 ID 已不可知就报 `applied`**——这是刻意的保守选择（§4.2.3）。
3. 一次 E2 覆盖**本集群共享同一失效主干**的全部数据面节点（部署形态已确认为单一集群；多集群场景的重新评估触发条件见 §4.2.4 第 4 条）。

**同时记录一个既有的放大路径（D6，且比 v5.1 的描述更宽）**：失效流裁剪（`maxlen=10_000`，`main.rs:632-639`，每 30s 一次）**只要删除任何条目**就 bump generation —— `trim_and_maybe_bump` 用的是 `XTRIM MAXLEN` 的 `removed > 0`（`events.rs:204-213` 的 Lua），**裁剪在原理上无法区分已读与未读**。所以"只裁掉已读条目就无害"这个假设**不成立**：只要持续 >333 事件/秒，每个 trim 周期都会让**全部节点清空整个缓存（L1+L2）**，与任何消费者是否落后无关。因此 §5.1 的"每租户失效频率上限"不是可选项。


### 6.4 决策记录 A-1：E2 **不**转发到 leader 的 `admin` API

- **编号**：A-1（正文内决策记录；本项目无 `docs/adr/` 体系，决策记录随设计文档走——见 §6.5）
- **日期**：2026-09-17 ｜ **状态**：**已决**（实现后回填"已执行"）
- **决策候选**：新端点的实现形态——直连本节点 + 三层保证（§4.2），还是把请求转发到 leader 的 `admin` API 上既有的 `POST /api/v1/tenants/{id}/auth/cache/invalidate`。

#### 背景与证据

既有的租户自助端点在 leader 的 admin 面**确实能正确执行精确清除**：

| 环节 | 事实 | 证据 |
|---|---|---|
| 精确删除（本节点） | 逐 key `map.remove((tenant, sha256(key)))` + L2 `DEL` | `http.rs:245-259` |
| L2 键与流载荷同源 | 同一 `hex_digest` 转换点；大小写已规范化（审计 L-4） | `http.rs:882-890`、`:276-284` |
| 在途判定不被撤销 | 每条失效路径第一步 `epoch++` + `set_if_unchanged` 拒写 | `http.rs:246`、`:125-142` |
| 集群侧精确删除 | 事件带 `tenant_id` + 摘要 → 各节点 `invalidate_hashes` | `events.rs:233-265` |
| **转发链路技术上可行** | 旧路由在 admin token 闸门**之前**、只认租户令牌；`forward_mutation` **原样中继调用方 `Authorization`**（即租户令牌）；leader 有 `pool` 与 `invalidation` | `admin/mod.rs:616-668` vs `:670-679`、`forward.rs:236-241` |

**所以本决策不是因为"转发会失败"，而是因为"转发没有意义、并且更差"。**

#### 选项

| 选项 | 内容 |
|---|---|
| **A（采纳）** | 数据面本节点直连：令牌走快照校验（§3.4）→ 本节点同步清 L1+L2 → 发布失效流 → **收敛屏障**等待全存活节点确认（§4.2.3） |
| **B（否决）** | 数据面把请求转发到 leader 的 admin API，复用既有租户端点 |
| C（否决） | 让 leader 主动逐节点推送清除（放弃共享总线模型） |

#### 决策

**采纳 A。**

#### 理由

1. **B 不产生全集群清除——这是决定性的。** 扇出由**共享失效流**完成，与"哪个节点发布"无关：无论 edge 自己发布还是 leader 代发，各节点消费的都是同一条总线事件，**对其他节点的效果逐字节相同**。B 唯一改变的是"同步被清的是哪个节点的缓存"。
2. **B 把"同步清空"给了错误的节点。** 文档拓扑是 **LB/Ingress → edge:8080**（`dev-docs/cluster.md` §4.2 明写）、`environment/docker-compose.cluster.yml` 里 leader 候选只暴露 `8081`、edge 才暴露 `8080`。**租户流量根本不打 leader 的数据面** → B 等于花一跳去清一个可能不承载任何租户请求的节点。
3. **B 反而把入口节点推到异步路径上。** 租户下一次请求极可能仍打到同一入口（LB 亲和/DNS）。A 让入口节点**同步**清空（下一次请求立即回源）；B 让入口节点等总线（≤500ms）。A 让掉的正是这个端点唯一"立刻见效"的部分。
4. **B 把已删除的机制整套加回来**：`cluster_registry`/`leader_ready` 进 `AppState`、`forward_mutation` + `x-hydra-forwarded` 循环守卫、**502/504「结果未知」的歧义语义**，以及"管理口必须对所有数据面节点可达"这一新耦合（edge 刻意没有 `HYDRA_ADMIN_TOKEN`）。A 的依赖只有"共享 Redis（集群本来必需）+ 本节点内存"——**没有对端可失败**。
5. **B 会让 `invalidated` 变得没有意义**：该字段在**接收请求的节点**上算出（`handlers.rs:1660-1667`）；转发后它统计的是 leader 的 L1 命中数，对租户更无用。
6. **B 省不下任何代码**：A 已经调用**同一套原语**（`HttpAuthChecker::invalidate`/`invalidate_tenant`、`InvalidationStream::publish`、`apply_invalidation`），而旧端点按 §8.1 **M1 直接删除**（项目尚未上线）——它的 handler 主体原样搬进新模块，是**搬迁**而不是重写。要复用，复用的是 **handler 与底层原语**，不是**网络路径**。
7. **B 也没有解决真正的缺口。** 真正的缺口是"**没有任何 owner 负责全集群已收敛**"：leader 处理完同样只回 `published: true`（`handlers.rs:1660-1667`），消费者水位 `last_id` 同样从不对外发布（`events.rs:293/311`：`last_id` 在循环内声明与推进）。**B 只是把"谁在骗你"从 edge 换成 leader。** 该缺口由收敛屏障（§4.2.3）在**失效流**上补齐，而不是由请求路径上多一跳补齐。

#### 后果

| 类型 | 内容 |
|---|---|
| 正面 | 零额外网络跳；入口节点同步生效；无对端失败模式；不引入新信任边界；复用同一 handler 与原语（零重复实现） |
| 已接受的代价 | 收敛确认需要一次共享 Redis 读（发布方 + 各节点水位 HASH），由 E2 的每租户频率上限约束（§5.1） |
| 已接受的限制 | 一次 E2 只覆盖共享同一失效主干的数据面节点（= 本集群，部署形态已确认为单一集群）。引入第二个集群时的重新评估触发条件见 §4.2.4 第 4 条 |

#### 重新评估的触发条件（reconsider if）

1. 失效主干从"共享总线"改成"由某节点主动推送到对端"（即选项 C 成立）——此时"权威节点"才真正存在，本决策需重评；
2. 出现需要**同步阻塞直到物理确认**、且不能容忍"读共享 Redis 得知水位"这一机制的合规要求（例如要求逐节点签名回执）；
3. 共享 Redis 被移出架构（届时 L2 与失效流同时消失，整个 §4.2 的三层模型都要重做）；
4. **引入第二个数据面集群/地域**（Q13 已确认半年内不会）——届时 E2 的覆盖范围、响应是否需要范围声明字段、是否强制共享失效主干，都要重新评估（§4.2.4 第 4 条）。

> **2026-09-18 更新**：A-1 的机制偏好（"数据面经共享 Redis 总线协调、不直连 peer"）在**租户配置写**这一新场景下被 **§6.4b 决策记录 A-2 显式、限定地修订**。A-1 本体（E2 不转发到 leader 的 admin API）**不受影响、继续有效**——两者的判别标准见 A-2「与 A-1 的关系」。

### 6.4b 决策记录 A-2：租户配置写经 internal 控制面转发到 leader

- **编号**：A-2（正文内决策记录；无 ADR 体系，理由同 §6.5）
- **日期**：2026-09-18 ｜ **状态**：**已决（待实现落地后回填"已执行"）**
- **决策候选**：子租户自助写的实现形态——数据面节点（可能是 edge）收到租户写请求后，如何到达唯一写者 leader。
- **依赖设计**：`dev-docs/design-sub-tenant.md` §6（头号问题）、v1 已实现（operator 代管写走既有 admin 面转发）；本决策是 v2（租户自助写）的前置。

#### 背景与证据

1. **A-1 的决定性理由对配置写不成立。** A-1 否决转发的第 1 条理由是"扇出已由共享失效流完成，转发不产生额外效果"（§6.4 理由 1）。该理由针对的是**单向失效事件**；而子租户 CRUD 是**对权威配置的同步写**，leader 是**唯一写者**（`store.rs`/SQLite 在 leader），所以"把写送到 leader"不是"没意义的一跳"，而是**唯一正确的落点**。A-1 该条不能外推到本场景。
2. **既有的"edge→leader + cluster token"传输/认证模式已存在。** leader 的 admin 面有 `/api/v1/internal/*` 端点族，用 **`HYDRA_CLUSTER_TOKEN`** 认证（fail-closed、常数时间比较，`crates/hydra-server/src/admin/mod.rs:619-641`；当前唯一路由 `/internal/control`，`:373-374`）；edge **本就持有 cluster token** 并在轮询 leader 的 control URL。
3. **admin 面转发不能原样复用给租户写。** `forward_mutation` 原样中继调用方 `Authorization`（`cluster/forward.rs:236-238`），而 admin 闸门**先**验 admin token **再**转发（`admin/mod.rs:653-662` → `:664-670`）；edge 刻意不持有 `HYDRA_ADMIN_TOKEN`（`main.rs:238-247`；`design-tenant-api.md:711`）。信任边界与闸门顺序根本不同。
4. **可复用的转发语义全套现成**：注册表实时解析目标、`FORWARD_ONCE_HEADER` 环守卫（`forward.rs:115`）、connect/total 双超时与"确定失败 vs 结果未知"分类（`forward.rs:53-109`）。
5. **对账基线现成**：租户 API 的 `whoami` 已返回 `config_version`（`tenant_api/auth.rs:54-57`）。

#### 选项

| 选项 | 内容 |
|---|---|
| **A′（采纳）** | 数据面 handler → leader `/api/v1/internal/tenant-config/...`；**cluster token 认节点**（`admin/mod.rs:619-641`），**专用请求头 `x-hydra-tenant-token`** 携带租户 Bearer（**不放在 body**），leader 侧用**同一 `authenticate` 重鉴权**并**做授权绑定**（写目标 == 已鉴权租户，见前置条件 4）；复用 `forward.rs` 语义；幂等 PUT-by-`(tenant_id,name)` upsert + 按**不可变 id** 的幂等 DELETE；`whoami.config_version` 对账。 |
| B（仅当 A-2 被否时启用） | Redis 意图流：最终一致，需幂等 apply + 版本守卫 + 新建 per-request 回执/对账。 |
| C（否决） | LB 把写流量分流到 leader。 |

#### 决策

**采纳 A′**，并**显式、限定地**放宽 A-1 的机制偏好：**允许数据面节点为"租户配置写"直连 leader 的 internal 控制面**。该放宽**不涉及 E2**、不改动失效流模型。

#### 理由

1. **配置写必须有权威落点。** 与 E2 不同，写不通过共享总线"扇出"；leader 是唯一写者，转发是唯一正确路径。
2. **不新建信任边界类别。** 复用既有 internal 控制面 + `HYDRA_CLUSTER_TOKEN`（节点身份），叠加 leader 侧对**租户 Bearer 的二次鉴权**（租户身份）。cluster token 只证明"这是我方节点"，不证明租户身份；重鉴权在权威快照上执行，也天然防重放中的身份混淆。**更重要的是，重鉴权之外必须做授权绑定**：写目标必须等于已鉴权租户（见前置条件 4）——即 **cluster token 证明节点、Bearer 证明身份、绑定规则证明权限**，三者缺一不可。
3. **不新建网络路径类别。** edge 已在调用 leader 的 control URL（快照轮询），写只是复用同一传输。
4. **幂等使 failover 安全。** PUT-by-`(tenant_id,name)` upsert 收敛；**DELETE 必须按服务端生成的不可变 id**（重放落到已不存在的 id = no-op；**不得**按 `(tenant_id,name)` 删——否则迟到的 DELETE-X 重放会删掉租户后来新建的同名 X，收敛到的是"旧意图"而非最新状态）。leader 在 apply 后、ack 前失败时，租户重试收敛到同一状态。**结果未知（504）** 以 `whoami.config_version` 对账，不新建机制。
5. **分区时 fail-closed。** 写 503/504；读继续用旧快照（数据面只读端点 §6.2 本就快照喂养）。
6. **否决 C**：破坏 any-node 自助；样例拓扑 leader 不暴露数据面（`design-sub-tenant.md` §6.5）。
7. **B 的核心问题**：CRUD 是同步语义，纯 fire-and-forget 会"先 202 后冲突"（失效流是单向的，没有 per-request 回执）。

#### 实现前置条件（必须随 v2 一并满足，来自 v1 实现后复审 findings）

1. **配额必须按 DB 全量行计数**（在写事务内），而不是按 enabled-only 快照计数——否则"停用→再建"可无限绕过 `MAX_SUB_TENANTS_PER_TENANT`/`MAX_ROUTES_PER_SUB_TENANT`，使 DB 与全量行无界增长（v1 复审 finding 1）。v1 仅 operator 面，故不阻塞；v2 必须修。
2. **写事务内复验前缀重叠**——v1 的 validate-then-insert 存在 TOCTOU：并发两个"不等但重叠"的前缀都能过（DB unique 只挡相等），只有 warn 级 `config::validate` 兜底（v1 复审 finding 2）。
3. **为配置写引入独立限流维度**（防经租户 API 的写扇出放大，参照 `design-tenant-api.md` §5.1 对 invalidate 的论证）。
4. **租户绑定（授权，非仅鉴权）**：leader 重鉴权得到租户 T 后，写目标必须强制等于 T。判定映射（不得含糊）：
   - 请求体 `tenant_id != T`（含该 id 不存在）⇒ **403**，且用**纯字符串比较、在任何存在性查询之前**判定——镜像 `tenant_api/mod.rs:403-405` 的"租户 id 就在调用方自己的 base URL 里，所以是 403 而非 404"理由，避免把端点变成租户存在性 oracle；
   - 路由写的 `sub_tenant_id` **缺失，或属于他租户 ⇒ 统一 404**（资源域内不泄露存在性）。
   内部端点**绕过了数据面的 URL↔token 交叉检查**（`tenant_api/mod.rs:405` 的 `tenant_id_mismatch`/403），所以这条绑定是**唯一防线**：cluster token 持有者（每个 edge/standby）或一个可重放的租户 Bearer 都不能借此写**别的租户**的数据。
5. **凭据放置与日志约束**：租户 Bearer 走专用请求头 `x-hydra-tenant-token`（**不放 body**，避免转发路径上的 body 日志/诊断泄露活凭据）；转发链路**禁止记录**该头与请求体。
6. **审计与归因**：经 internal 路径的每次配置写必须记录**发起租户**与**发起节点**（trace id 已在 `forward.rs:233` 中继，租户归因需新增），供事后追溯。
7. **接收侧必须断言本节点是租约持有者**：internal 写 handler 在落库前必须检查本节点**当前持有租约**（`state.leader_ready` / `is_leader()`，同 `maybe_forward_mutation` 的 `admin/mod.rs:480-486`）：单节点（`leader_ready == None`）本地执行；**非 leader ⇒ `503 not_leader`，绝不本地执行**。理由：internal 闸门在 `admin/mod.rs:646-652` **早退 `route()`**，**不经过**既有 admin 转发路径的 leader 检查（那里的 sender 侧机制——注册表解析、FORWARD_ONCE、超时分类——**都不覆盖接收侧**）。若缺此断言，一个 standby / 被罢黜 leader 会写自己的副本 DB 并返回 200，而下一次 `restore_config` 会把它清掉——这正是理由 7 谴责的"先成功后丢失"（幻影 200）。检查—写入之间被罢黜的竞态（TOCTOU）与既有 504"结果未知"属同一**已接受**的歧义类。
8. **数据面转发 plumbing（信任受控）**：数据面今天**拿不到** cluster token / registry / control URL（`AppState` 无这些字段；`ControlClient` 未存入数据面）。A′ 必须向 `AppState` 注入**最小能力**——一个"解析 leader URL"的闭包 + cluster token，且**仅租户配置写路径**可用；**不得**把 `NodeRegistry` 交给数据面。转发须用**新函数**（现有 `forward_mutation` 固定中继调用方 `Authorization`，无法发送 cluster token + `x-hydra-tenant-token`，`forward.rs:236-238`）。详见 v2 计划 `plans/2026-09-18-sub-tenant-v2.md` D1/D2。

#### 后果

| 类型 | 内容 |
|---|---|
| 正面 | any-node 自助写；零新集群机制类别；复用既有 internal 认证与 `forward.rs` 全套语义；幂等 + `config_version` 对账 |
| 已接受的代价 | **数据面首次为写直连 peer**（仅指向 leader 的 control 面）；cluster token 的作用域从"取快照"扩到"提交配置写"——由 leader 重鉴权 + 租户 Bearer 约束 |
| 已接受的限制 | 分区期间写不可用（fail-closed）；跨集群场景不覆盖（见下） |

#### 重新评估的触发条件（reconsider if）

1. leader 租约/选举语义改变，使"活跃 leader 是唯一写者"不再成立；
2. 引入第二个数据面集群/地域（届时需明确写路由与一致性边界）；
3. `HYDRA_CLUSTER_TOKEN` 被共享给非受信节点（节点身份前提被破坏）。

#### 与 A-1 的关系（不得混淆）

- A-1 否决的是**把 E2 失效请求转发到 leader**，其决定性理由是"扇出已由共享流完成"——**A-1 继续完整有效**。
- A-2 只放宽"A-1 的机制偏好"，且**仅限租户配置写**这一"leader 是唯一写者"的场景。判别口诀：**请求是否需要落到唯一权威写者？** 需要 → A′；只是让各节点生效 → 共享总线（A-1）。**缓存没有权威写者**（每节点 L1 自治、L2 共享），因此 **E2 永远落在口诀的第二支**——这条是防止把 E2 重新解释为"需要到达 leader"的护栏。

### 6.5 本项目的决策记录归属（ADR 门禁结论）

本项目**没有 ADR 体系**（无 `docs/adr/`、无 `dev-docs/aegis/adr/`、无 baseline 目录；`dev-docs/aegis/` 只有 `plans/`）。按 `dev-docs/aegis/README.md:8-10`，该目录下的记录是**咨询性方法包产物**，不授予完成权限。

因此决策记录的归属是：**`dev-docs/design-*.md` 的正文决策记录节**（先例：`dev-docs/design-tenant-model-catalog.md` 的 `## 6. 风险与决策记录`）+ `dev-docs/aegis/plans/*.md` 的「设计决策」表。

另外，按 ADR 门禁的 **Retro/Memory Filter**：本决策在本文写作时**尚未执行**（设计状态为待评审），属于"unexecuted idea"，**不应**进入被接受的架构记忆。

> **2026-09-17 更新（实现落地后的基线同步）**：**前提已变** —— A-1 已随 T1–T10 实现并进入 CI 门禁，不再是 unexecuted idea，因此它**已具备**进入被接受的架构记忆的资格。归属结论**不变，但理由换了**：本项目没有 ADR 体系（上一段已核实），决策记录就住在 `design-*.md` 的正文决策记录节，所以 A-1 仍然是 §6.4 这一节本身，**不新建独立 ADR 文件**；同时它已进入 `dev-docs/ops.md` 与对外契约，成为运维与租户都看得到的既有事实。**"不建 ADR 文件"现在是因为项目没有那套体系，而不是因为决策还没执行** —— 这两者的区别在下一次有人问"该不该为架构决策补 ADR"时是决定性的。

**本决策**同时**已**被 §11 收口为"已决项"，不在待决策清单内。

### 6.6 部署拓扑：可信反向代理与 `X-Forwarded-For` 解析

失败限流的每 IP 维度默认以 **socket peer IP** 为键、**绝不**读 `X-Forwarded-For`（调用方可控头不得自选桶）。置于 LB 之后时，所有请求共享 LB 的出口 IP，每 IP 维度退化为"整条租户舰队级"（保守，但会把同 LB 下合法客户端与攻击者一起计数——正是 §5.1 里 B1 权衡所针对的跨租户 DoS 场景）。`HYDRA_TRUSTED_PROXIES` 解决这一点：

- **语义**：逗号分隔的 **IP 或 CIDR** 白名单（IPv4/IPv6；裸 IP 视为 /32 或 /128），列出"其 `X-Forwarded-For` 值得信任"的反向代理。
- **未设/空 = 不信任任何人**：回落到 socket peer IP（即此前的保守行为，零变化）。
- **解析**：当 peer 本身在信任列表内时，**先把所有 `X-Forwarded-For` 头行按顺序拼接成单个列表**，再取其中**最右侧一个"不是可信代理"的地址**作为限流键；没有任何 XFF 行 / 全为可信代理 / 含非法项时回落到 peer。
- **非法条目 → 启动失败**（fail-closed，不静默忽略）。
- **catch-all 告警**：`0.0.0.0/0` 或 `::/0` 会信任来自**任意 peer** 的 XFF，等效于让每 IP 维度失效；节点**启动时大声告警**但**不拒绝启动**（区别于"非法条目 → 启动失败"：catch-all 是合法但危险的配置）。
- **误配风险**：信任了一个**不剥离/不覆盖入站 XFF** 的代理时，客户端可伪造 XFF 轮换自己的每 IP 桶，等效于让每 IP 维度失效。只在信任"会剥离/覆盖入站 XFF"的代理时才设此变量。

---

## 7. 实现落点与文件改动

### 7.1 新增

**纯逻辑放 `hydra-core`，I/O 外壳放 `hydra-server`**——`dev-plan.md:26-37` 铁律 2 的强制要求（内部逻辑必须重构为纯函数，直接以真实输入/输出单测，不 mock 任何东西），且 `hydra-core` 有依赖防火墙（CI `cargo tree` 校验，`ci.yml:86`）与 ≥90% 行覆盖率门槛（`dev-plan.md:19-24`）。

| 文件 | 内容 |
|---|---|
| `crates/hydra-core/src/tenant_api.rs`（**新增，纯**） | ① **规范形态校验**：`%Y-%m-%dT%H:%M:%SZ` 的定宽字形校验（含月/日/时/分/秒的**数值范围**检查，纯字符串运算，不做日历计算）；② **规范形态下的顺序比较**（同长度定宽 ⇒ 字典序 == 时间序）；③ 路径解析：`/tenant/{tid}/api/v1/{endpoint}` → `(tenant_id, Endpoint)`；④ 用量 DTO 与聚合行的纯计算（含 `COALESCE` 语义的零值）。**零 I/O、零 HTTP 类型、零 `chrono`** |
| `crates/hydra-core/tests/tenant_api.rs`（新增） | 上述纯函数穷举单测（形制照 `crates/hydra-core/tests/router.rs` / `validate.rs`） |
| `crates/hydra-server/src/tenant_api/time_bound.rs`（**新增**） | **时间入参的词法解析与归一化住在 shell，不在 core**——因为 RFC3339（带偏移）与 epoch 的解析需要日历运算，而 **`hydra-core` 没有也不允许有 `chrono`**（依赖白名单见 `crates/hydra-core/Cargo.toml:9-16`；`model.rs:299-300` 亦自述 "no `chrono` in core"）。本模块用 `chrono` 把用户的 `since`/`until`（RFC3339 / epoch 秒 / epoch 毫秒）归一化为规范字符串，并在这里做**窗口长度**判定（需要日历运算）；随后把规范字符串交给 core 做字形校验与顺序比较 |
| `crates/hydra-server/src/tenant_api/mod.rs` | 路由（轻量段匹配，形制照 `admin/mod.rs:277-419`）、前缀判定与 `request_filter` 入口、`respond_json`（形制照 `proxy.rs:1280-1297`）、`TenantApiConfig::from_env` |
| `crates/hydra-server/src/tenant_api/auth.rs` | 令牌闸门：`tenant_from_token(&ConfigStore, &str)`（读 `store.replication().fidelity().tenant_token_hashes`）+ 常数时间比较 + 与 `tenants_by_id` 的**同一快照读** |
| `crates/hydra-server/src/tenant_api/handlers.rs` | E1–E3 三个 handler（`Resp = http::Response<Vec<u8>>`） |
| `crates/hydra-server/src/clickhouse.rs`（**新增，Q14=T1**） | CH 的**唯一传输 owner**，由写路径与读路径共用：① `ClickHouseConfig` 与其 URL/凭据解析（从 `sink.rs:446-469` 提取，今天它是私有且被 move 进 flush task 闭包）；② 裸 TCP 手写 HTTP 的请求原语 `send(cfg, body, timeout) -> (status, body)`（从 `sink.rs:711-797` 提取，**超时改为入参**：写路径用它测过的期限，读路径用 `HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS`）；③ 状态行 + `Code: N` 分类（从 `sink.rs:801-811` 提取并扩展到读侧需要）；④ `url_encode` / Basic 认证头（从 `sink.rs:899-914` 提取）。**关键：提取必须逐行保持写路径已有的期限与重试语义不变**（既有 `tests/clickhouse_sink.rs` 是回归网） | "怎么跟 CH 说话"只能有一个 owner；若读侧自己再写一份 `parse_url`/状态分类，URL、凭据与协议的解释就会分叉成两套 |
| `crates/hydra-server/src/usage_query.rs`（**新增**） | E3 的读能力：`trait UsageQuery { async fn aggregate(...) -> Result<UsageAggregate, UsageQueryError>; fn source(&self) -> &'static str; }`，两个实现 `SqliteUsageQuery`（走 `store.pool()`）与 `ClickHouseUsageQuery`（走共享 CH 传输）。**JSONEachRow 解码器同时接受 `"123"` 与 `123`**（§4.3.3 CH-A），并把 `""` 归一为 `None`（CH-B） |
| `crates/hydra-server/src/tenant_api/throttle.rs` | 固定窗口限额（单节点 `DashMap`；`cluster-redis` 下走 Redis） |
| `crates/hydra-server/tests/tenant_api.rs` | 数据面集成测试（`#![cfg(all(db, http-client, proxy))]`；真实 `HydraProxy` + 真实 Pingora `Server` + `:memory:` SQLite + wiremock 假上游） |
| `crates/hydra-server/tests/tenant_api_cluster.rs` | E2 广播与收敛屏障、快照鉴权、E3 走 CH（`#![cfg(feature = "cluster-redis")]`；**真实 Redis**，§10.3） |
| `dev-docs/aegis/plans/<date>-tenant-api.md` | 实施计划（本文通过评审后产出，须登记进 `dev-docs/aegis/INDEX.md`，`kind=plan`） |

> 与 v1 相比，`policy.rs`（域名/URL 校验）整个消失，纯模块只剩归一化 + 解析。

### 7.2 修改

| 文件 | 改动 | 关键点 |
|---|---|---|
| `crates/hydra-core/src/lib.rs` | `pub mod tenant_api;` | |
| `crates/hydra-core/src/config.rs` | `ConfigData` 新增派生索引 `tenants_by_id: HashMap<String, Tenant>`（`config.rs:37-69`） | 由**同一个 loader** 从同一批行构建，与 `tenants_by_domain` 同源 → 不是第二个 owner。需同步 `ConfigData::default`、loader（`store.rs:build_config`）、以及 `entities.rs`/`config_data.rs` 的既有形状测试 |
| `crates/hydra-server/src/proxy.rs` | `request_filter` 第 0 步插入前缀判定；`AppState` 增 2 字段 + `AppState::for_tests()` | 新增字段会**打断 12 处测试构造点**（`tests/terminate_mode.rs:206-233` 的 helper——它**内含** `:224`——加 `:600/718/1089/1320/2871`、`streaming_usage_persistence.rs:271`、`tls.rs:166`、`anthropic_passthrough.rs:302/392/506`、`metrics.rs:282`；**不重复计 `:224`**）→ **必须同批**提供 `AppState::for_tests(...)` 并迁移这 12 处。仓库内 `AppState { .. }` 字面量共 **14** 处：1 处结构体定义（`proxy.rs:108`）、1 处生产构造（`main.rs:571`，随本方案的构造后移处理、不迁移到 `for_tests()`）、12 处测试 |
| `AppState`（`proxy.rs:108-128`） | **只加两个字段**：`invalidation: Option<InvalidationStream>`（`not(cluster-redis)` 下设 `Option<()>` 占位，与 `AdminState` 同款，`admin/mod.rs:108-113`）、`usage: Arc<dyn UsageQuery>` | 收敛屏障没有增加字段（长在 `InvalidationStream` 上，见下）。**DB 池也不新增字段**——用 `ConfigStore::pool()`（`store.rs:374-379`）：同一个 store 实例的公开访问器，"本节点有没有本地库"只有一个 owner，避免 `AppState.pool` 与之漂移。**不需要** `key_provider`/`admin_token`/`cluster_registry`/`leader_ready`/`reload_lock`/`snapshot_stale`。E2 的屏障还需要"存活节点列表"：`registry.list_nodes()` 已存在（`cluster_api::cluster_status` 在用），但**不把 registry 放进 `AppState`**——改为在 `main.rs` 把一个 `Arc<dyn Fn() -> Vec<String>>` 注入 `tenant_api` 配置（与 `AdminState.leader_ready` 同一闭包注入手法，`admin/mod.rs:104`）。这样数据面**仍然拿不到注册表**，也就仍然没有任何转发能力 |
| `crates/hydra-server/src/main.rs` | ① `AppState` 构造**后移到 `invalidation_stream` 之后**（`:621-644` 之后）；`:594` 的 `state.sink.clone()` 改为先克隆 `sink`。② `usage: Arc<dyn UsageQuery>` 由 `:261` 已读的 `sink_kind` 选择唯一实现。③ `allow_ttl_max` 从 env 接进 `AuthConfig`。④ `cluster.node_id` 接进 E2 响应构造（`HYDRA_CLUSTER_ID` 已随 Q13 删除） | **只有 1 个迟到资源**（`invalidation`），所以后移是小改动。替代方案：`ArcSwapOption`/`OnceLock` 后置填充；**推荐后移**，不留半初始化状态。`node_id` 已存在（注册表/租约/熔断投票都用它） |
| `crates/hydra-server/src/sink.rs` | 把 CH 的传输与寻址**下沉到新模块 `clickhouse.rs`**（Q14=T1，见 §7.1），`ClickHouseSink` 改为调用它；**行为与期限必须逐行不变** | 这是本次唯一动到"正在工作的生产代码"的改动（§12 已列风险）：既有 4 条 `clickhouse_sink` 测试即回归网，且提取前后本应**零行为差异** |
| `crates/hydra-server/src/http.rs` | `AuthCache::set` / `set_if_unchanged` 对 **allow** 项套用 `allow_ttl_max` 上限（§4.2.5，默认 300s = 现状值） | "消费者停摆的节点"唯一的硬上界来源；`deny` TTL 不受影响。需同步 `AuthConfig::default`（`http.rs:414-423`）与既有 TTL 单测 |
| `crates/hydra-server/src/cluster/events.rs` | **`InvalidationStream` 扩为"扇出 + 收敛"的单一 owner**（不新增子系统）：① 消费者每批 apply 之后把该批最大事件 ID 写进 `hydra:{ctl:inv:applied}` HASH（**先 apply 后 ack**）；② 新增 `applied_watermarks()` 与 `await_applied(event_id, live_nodes, timeout)`；③ generation bump 路径 `clear_all()` 后**不推进水位**（§4.2.3 的保守规则） | 收敛屏障必须与失效流**同一 owner**：共用 `hydra:{ctl:*}` 命名空间与同一个 Redis 连接池；放别处就是第二个"谁负责让全集群失效"的答案 |
| `crates/hydra-server/src/admin/mod.rs` | **删除** migration 0009 的租户令牌路由整块（`:616-668`）；管理端 `DELETE /api/v1/auth/cache` 的响应体由 `published: bool` 换成新的 `fleet` 对象 | M1（§8.1）：管理面回到单一凭证语义，不再有"排在 admin 闸门之前"的特殊分支 |
| `crates/hydra-server/src/admin/handlers.rs` | 把 `Resp`/`err_json`/`ok_json`/`read_body`/`constant_time_eq`/`invalidate_shape_error` 提为 `pub(crate)`（或搬进新模块）；`tenant_auth_cache_invalidate` 主体搬进新模块；**删除** `tenant_id_for_token`（由 `tenant_from_token(&ConfigStore, ..)` 取代）；管理端 `DELETE /api/v1/auth/cache` 改用新的 `fleet` 响应形态 | 现状是 `pub(super)` = 仅 `crate::admin` 可见（`handlers.rs:27/47/59/219/666`），proxy 模块够不着。旧实现删除，**不留双实现** |
| `crates/hydra-server/src/db.rs` | **只加** SQLite 侧的用量聚合查询（`sqlx::query` 运行时风格 → **不改 `.sqlx/`**） | **不新增任何租户写函数**（v1 的窄写函数随需求一起删除） |
| `crates/hydra-server/src/admin/metrics.rs` | 新增租户 API 与用量查询指标族（§9） | CH 解码失败必须可观测为 `decode_error`，否则那类陷阱表现为静默 0 |
| `dev-docs/design.md` | §13.2 端点表：租户自助端点移出「管理 Web API」；新增「租户自助 API（数据面）」一节（**明说只读 + 缓存删除，域名/auth_url 修改仍在管理 API**）；§11.7 表格指向新路径；§9.3 CH 侧补一句"读路径由租户 API 使用" | |
| `dev-docs/ops.md` | §5.1 改写为新路径；新增"租户 API 开通/关闭"节；**新增"租户改域名/auth_url 的运维流程"节**；**新增 CH 用量查询的运维节**（`requests` 为近似值/重复行成因、宽窗口的代价、`ORDER BY (tenant_id, created_at)` 的规模建议） | §2.3 与 §4.3.3 CH-F/CH-E 明确承认的代价 |
| `admin-ui/app.js`、`admin-ui/api-docs.js` | 租户页展示 base URL 与令牌状态；API 文档拆分租户/运维两页 | 改前端 ⇒ **Playwright 浏览器腿强制**（`dev-docs/waves/wave-6-ui-hardening.md:86`） |
| `dev-docs/aegis/INDEX.md` | 本设计文档已登记（`kind=doc`） | 既有缺口提醒：`2026-08-27-tenant-access-token.md` 至今未登记 |
| `.sqlx/` | **无需改动**（采用运行时 `query` 风格） | 若改用 `query!` 宏则必须按 `dev-docs/HANDOFF.md:167-190` 重生成并提交 |

### 7.3 配置项（**全部为环境变量，10 个**）

**事实前提：本仓库没有任何配置文件加载器**（`Cargo.toml` 无 `toml` 依赖；全仓库无 `hydra.toml` 读取代码）——`design.md` §15.1 的 toml 示例与代码不一致是既有已记录问题。所以新开关只能走环境变量（+ DB 行）。

| 变量 | 默认 | 说明 |
|---|---|---|
| `HYDRA_TENANT_API` | `on` | 总开关；`off` 时前缀整体不拦截（回到基线行为） |
| `HYDRA_TENANT_API_RATE_LIMIT_PER_MIN` | `60` | 每租户**已授权**请求上限 |
| `HYDRA_TENANT_API_AUTH_FAIL_LIMIT_PER_MIN` | `10` | 每 IP / 每摘要失败上限 |
| `HYDRA_TENANT_API_LOCKOUT_SECS` | `900` | 持续超限锁定时长 |
| `HYDRA_TRUSTED_PROXIES` | *(未设)* | 逗号分隔的**可信反向代理** IP / CIDR 白名单（IPv4/IPv6；裸 IP 视为 /32 或 /128）。**未设/空 = 不信任任何人**（回落到 socket peer IP，即既有的保守行为，零变化）；当 peer 本身在信任列表内时，**先把所有 `X-Forwarded-For` 头行按顺序拼接**，再取**最右侧一个不是可信代理的地址**作为每 IP 维度键，没有任何 XFF 行/全为可信代理/含非法项时回落到 peer。**非法条目 → 启动失败**（fail-closed）；**catch-all（`0.0.0.0/0` / `::/0`）→ 启动时大声告警但不拒绝启动**（合法但等效于禁用每 IP 维度）。误配风险：信任了一个**不剥离入站 XFF** 的代理会让客户端可伪造 XFF 轮换每 IP 桶，等效于让每 IP 维度失效（详见 §6.6） |
| `HYDRA_TENANT_API_INVALIDATE_PER_MIN` | `10` | 每租户失效频率上限（防 D6 放大） |
| `HYDRA_TENANT_API_USAGE_MAX_WINDOW_DAYS` | `31` | E3 窗口上限（§5.1 反放大；也是 CH 主键前导为 `created_at` 时控制扫描量的手段） |
| `HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS` | `5000` | E3 的 CH 查询超时（与写路径的推送超时相互独立） |
| `HYDRA_TENANT_API_CONVERGE_TIMEOUT_MS` | `2000` | E2 等待全集群确认的预算；超时 → 202 + `lagging`。`wait=none` 可跳过等待 |
| `HYDRA_AUTH_ALLOW_TTL_MAX_SECS` | `300` | **allow 缓存项 TTL 上限**（§4.2.5）。默认等于现有默认 allow TTL ⇒ 对不设 `expires_in` 的租户零变化；设小的副作用是认证回源流量上升 |

> v1 的 6 个域名/URL 相关开关（`DOMAIN_MODE`/`DOMAIN_ZONE`/`DOMAIN_RESERVED`/`ALLOW_AUTH_URL_WRITE`/`AUTH_URL_ALLOWLIST` 等）**全部删除**；v5 又删除了 `HYDRA_CLUSTER_ID`（Q13：单一集群，无范围可声明）。
>
> **总开关默认 `on` 的决定现在几乎没有争议**：整个表面是只读 + 一次幂等的缓存删除，不再有"新暴露的高危写接口"。v1 需要默认关闭两个写端点，是因为它们能把不可信方放进 `Host→租户` 映射与出站目标——那两个位置已不存在。

---

## 8. 与既有接口的关系

### 8.1 旧路径：**直接删除**（M1）

**前提已确认：项目尚未正式上线，无需考虑破坏性变更。**

因此 migration 0009 的 `POST /api/v1/tenants/{tenant_id}/auth/cache/invalidate` —— 连同 `admin/mod.rs:616-668` 那段"在 admin 闸门之前插一段租户令牌闸门"的整个代码块 —— **直接删除**：

| 删除项 | 收益 |
|---|---|
| 管理口的租户令牌路由 | 运维控制面**回到单一凭证语义**（admin token / cluster token），§3.0 规则 1 不再需要任何豁免 |
| `Deprecation`/`Sunset` 头与 `hydra_tenant_api_legacy_route_total` | 少一套只在过渡期存在的机制（反熵：不留悬空零件） |
| C3 那一类"闸门顺序即安全属性"的隐患 | 管理面路由器不再有一条"排在 admin 闸门之前"的特殊分支——那正是 §1.2 C3 记录的耦合形态 |
| `published` 字段的兼容垫片 | 响应体可以直接换成新的 `fleet` 对象，无需保留一个会说谎的旧字段 |

被否决的两个选项（记录备查）：

| 选项 | 否决理由 |
|---|---|
| M2 双挂载 + 硬弃用 | 只在"有存量调用方"时才有价值；前提已确认不成立，留下就是纯负担 |
| M3 永久双挂载 | 同一能力两个入口，长期维护成本 + 违反"一个行为一个 owner" |

**唯一保留的衔接物是数据，不是接口**：`tenant.access_token_hash`（migration 0009 加的列）继续使用——租户令牌就是新端点的凭证，运维在 Admin UI 上的设置/轮换流程不变。

### 8.2 顺带收敛的既有不一致

| 项 | 处理 |
|---|---|
| `published: true` 会说谎（D3） | 管理端 `DELETE /api/v1/auth/cache` 与新的 E2 **都**改用 `fleet` 对象 + `state` 三态；`published` **直接删除**（无存量调用方，§8.1）。两个入口共用同一个收敛屏障实现 |
| `invalidated` 只有 L1 口径（D4） | 响应增 `checked`；文档明确 `invalidated` = "本节点 L1 实际删除数" |
| 租户令牌闸门无任何防护（D2） | 新模块的闸门取代旧实现（旧实现随路由一起删除），一次修复到位 |
| `tenant_id_for_token` 全表读（D1） | 由 `tenant_from_token(&ConfigStore, ..)` 取代；旧函数**删除**，不留双实现 |
| `HydraProxy` 侧的 `GET /v1/models` 目录与租户 API 的路径边界 | 两条拦截都在 `request_filter` 前段，顺序写死在注释与测试里（§10.2 T22） |

---

## 9. 可观测性

### 9.1 新指标

| 指标 | 类型 | 标签 | 说明 |
|---|---|---|---|
| `hydra_tenant_api_requests_total` | counter | `endpoint`(whoami/invalidate/usage/**unrouted**), `status` | 入口流量与结果分布。`unrouted` = 保留前缀下不是三条路由的路径（404 在路由解析**之前**返回，因此归属只能是它）；它正是"租户打错路径"的信号 |
| `hydra_tenant_api_auth_failures_total` | counter | `reason`(missing/unknown/mismatch/**locked**) | 令牌失败分类（**不含**令牌本身）。被锁定而拒绝时记 `locked`（`throttled` 回答"拒了什么"，这个标签回答"闸门为什么没跑"）。**`tenant_gone` 不可达**：快照里有令牌摘要却没有对应租户行时，实现返回 `not_ready`(503) 而非鉴权失败——那是配置快照换代的中间态，fail-closed 更诚实（见 §3.3），故该值与代码不符已删除 |
| `hydra_tenant_api_throttled_total` | counter | `scope`(ip/token/tenant/invalidate) | 限流触发。四个值现在都可产生（E2 的每租户失效限流记 `invalidate`） |
| `hydra_tenant_api_usage_query_total` | counter | `source`(sqlite/clickhouse), `group_by`, `result`(ok/store_unavailable/decode_error/**result_too_large**) | **`decode_error` 专指 CH 响应解析失败**——它是"整数被引号包裹"那类陷阱的唯一外部信号，否则表现为静默 0。**`too_large` 已删除**：窗口超限在查询之前就返回 400 `window_too_large`，该请求根本不会到达查询，因此这个值不可达。但**结果体超限**（CH 结果超过 ~64 KiB 响应上限）现在**可达**：内部变体 `ResultTooLarge`、指标标签 `result_too_large`，对外仍是 503 `usage_store_unavailable` 并附"缩小窗口/分组"的 actionable 消息——重试无用，须窄化 `since`/`until` 或降低 `group_by` 基数 |
| `hydra_tenant_api_usage_query_seconds` | histogram | `source` | CH 查询延迟；主键以 `created_at` 前导 ⇒ 宽窗口的代价随窗口增长，必须可观测 |
| `hydra_tenant_api_auth_latency_seconds` | histogram | — | 令牌校验耗时（回归 §3.4 的"零 DB I/O"声明） |
| `hydra_tenant_api_invalidate_pending_total` | counter | — | E2 返回 202 的次数。**持续 >0 说明集群里有节点清不掉** |
| `hydra_tenant_api_invalidate_converge_seconds` | histogram | `result`(applied/pending/single_node/unavailable) | 全集群收敛耗时（典型 <100ms；这是"清干净了没有"的量化口径）。`result` 就是 `fleet.state`，因此四态全部可能出现 |
| `hydra_invalidation_consumer_applied_id` | gauge | `node` | 各节点消费者的已应用事件水位（屏障的数据源） |
| `hydra_invalidation_consumer_lag_events` | gauge | `node` | 流尾与本节点水位之间的事件数 |
| `hydra_invalidation_consumer_stalled_seconds` | gauge | `node` | 水位多久没推进。**> 60s 即告警**——今天消费者只是 `warn!` 后重试（`events.rs:336-338`），"活着但不消费"完全不可见 |
| `hydra_auth_allow_ttl_capped_total` | counter | `tenant` | allow TTL 被 `HYDRA_AUTH_ALLOW_TTL_MAX_SECS` 封顶的次数（= 有多少租户在用超长 `expires_in`，是调参依据） |

> v1 的 `settings_write_total`（敏感写审计）、`host_mismatch_total`（域名抢占审计）与 `legacy_route_total`（过渡期计数）随需求与决策删除。**没有写，就没有写审计；没有域名抢占面，Host 一致性就是噪声；旧路径直接删除，就没有过渡期可计数。**

### 9.2 日志

- 每次鉴权失败：`reason`、`tenant_id?`、`source_ip`、`trace_id`（**不含令牌**）。
- 每次 E2：`tenant_id`、`scope`、`checked`、`invalidated`、`broadcast`、`trace_id`（运维排障"为什么没生效"的唯一证据）。
- **不新增配置写审计日志**（无写操作）。

### 9.3 与现有指标的隔离

租户 API 请求**不得**进入 `hydra_requests_total` / `hydra_tokens_total`（按 `tenant`/`provider`/`model` 打标签，`metrics.rs:179-184`，且**目前没有任何基数上限**——不要往这个已无界的族里再加维度）。租户自助流量单独一族，标签全部低基数枚举。

---

## 10. 验证与门禁

### 10.0 纪律前提（本仓库铁律，直接约束测试形态）

| 铁律 | 出处 | 要求 |
|---|---|---|
| **铁律 1 TDD 优先**：先红后绿；`hydra-core` 覆盖率 ≥90%、`hydra-server` ≥60% | `dev-plan.md:19-24` | 纯函数（归一化/解析/聚合）先写穷举单测；handler 由集成测试驱动 |
| **铁律 2 本体零 Mock/零桩**：唯一允许 mock 的是**真实外部边界**，且优先**进程级真实 double**（HTTP → wiremock 真 server；SQLite → `:memory:` 真引擎） | `dev-plan.md:26-37` | 不得为租户 API mock `ConfigStore`/`AuthCache`/DB；`auth_url` 用 wiremock |
| **铁律 2 补充**：Redis 必须连**真实实例**，未设 `HYDRA_TEST_REDIS_URL` 时必须**明确失败**、绝不静默跳过 | `dev-plan.md:39-48`、`tests/common/mod.rs:45-52` | §10.3 集群用例用真实 Redis |
| 生产代码不得出现 `unwrap/expect/panic/unimplemented/todo`、mock/stub、`#[cfg(test)]` 分支 | `dev-plan.md:197-206`、`dev-docs/waves/wave-6-ui-hardening.md:64/90` | E3 的 `UsageQuery` 两个实现的**每一个解析失败路径**都必须显式返回错误，绝不 `unwrap_or(0)`（那正是"假 0"的成因） |
| CI 全程 `RUSTFLAGS="-D warnings"` + `SQLX_OFFLINE=true` | `.github/workflows/ci.yml:8-16` | 本地门禁带同样环境变量 |

### 10.1 单元（`cargo test -p hydra-core`，无 I/O、无特性、无网络）

| 对象 | 用例 |
|---|---|
| 时间戳归一化 | RFC3339（`Z` / 带偏移）/ epoch 秒 / epoch 毫秒 / 非法串；**断言输出恒为定宽 `%Y-%m-%dT%H:%M:%SZ`**——这是 §4.3.2 那个"多算一整天"缺陷的回归锚点 |
| 时间窗校验 | `since > until` 拒绝；窗口超上限拒绝；边界值（恰好等于上限）通过；零宽窗口允许 |
| 路径解析 | 合法；缺 `tenant_id`；`tenant_id` 含 `/`；多余段；尾斜杠；`/tenant/{tid}/api/v1/../x` |
| 聚合零值 | 全 NULL 列 → 0（不是 NULL、不是 panic）；无行 → `requests: 0` + `as_of: null` |

### 10.2 数据面集成（`cargo test -p hydra-server --features server`）

**形态沿用 `tests/terminate_mode.rs`**（仓库里唯一无特性门禁的数据面套件）：

| 要素 | 既有做法（照抄） |
|---|---|
| DB | `common::setup_pool()` → 真实 `sqlite::memory:` + `run_migrate`（`tests/common/mod.rs:15-23`；每测试独立库，真引擎不 mock SQL） |
| 状态构造 | `ConfigStore::load(pool, key_provider)` + `HttpAuthChecker::new(AuthCache::new(300s,30s), AuthConfig::default())` + `CircuitBreaker` + `RateLimiter` + `NoopSink` → `Arc<AppState>`（`tests/terminate_mode.rs:206-233`） |
| 起服务 | 真实 Pingora `Server` + `http_proxy_service` + `add_tcp(ephemeral_port)` + `std::thread::spawn(run_forever)`（`terminate_mode.rs:236-249`） |
| 就绪 | **重试循环，不是 sleep**（`send_until_ready`/`get_until_ready`，`terminate_mode.rs:261-283`/`:1554-1574`） |
| 外部边界 | wiremock 真 HTTP server（`MockServer::start()`） |
| 断言 | 状态码 + 精确整串 body（本地应答用全等断言）+ 头（`content-type`、`x-hydra-trace-id`）+ **零上游负例**（`MockServer::received_requests()` 为空） |

**用例矩阵**（`E1`–`E3` 见 §3.5）：

| # | 用例 | 期望 |
|---|---|---|
| T1 | 正确令牌 + 正确 `{tenant_id}` → E1 | 200，字段齐全，**响应无任何摘要/密钥** |
| T2 | 无 `Authorization` | 401，**且未触碰 DB**（断言池无查询/无写入） |
| T3 | 错误的令牌 | 401，文案与 T2 一致 |
| T4 | 令牌属于 B，URL 写 A | 403 `tenant_id_mismatch`（**不是** 404） |
| T5 | 管理 token 调 E1 | 401（凭证不跨平面） |
| T6 | 租户令牌调 `/api/v1/providers`（管理面） | 401 |
| T7 | **停用**租户 + E1/E2/E3 | 全部 200（自救路径必须通） |
| T8 | 令牌 = 客户端 api-key（模拟误用） | 仍按租户令牌语义判定；**断言没有向 `auth_url` 发出任何请求**（外部鉴权从未运行） |
| T9 | 租户 API 请求后查 `usage_record` | **0 行新增**；`hydra_requests_total` 不增（含 E3 自身的自指防护） |
| T10 | `HYDRA_TENANT_API=off` | 前缀不再拦截（回到基线），断言基线一致 |
| T11 | E2 精确 key（先用一次请求制造缓存项） | `invalidated >= 1`；随后同一 key 的请求**必须回源**（wiremock 断言 `auth_url` 被再次调用） |
| T12 | E2 空 body | `scope:"tenant"`，全租户缓存清空 |
| T13 | E2 1001 个 key / 单个 4097 字节 | 400 `too_many_keys` / `invalid_api_key` |
| T14 | E3 的**实现注入**与 sink kind 一致 | `sink=sqlite` → 注入 `SqliteUsageQuery`；`sink=clickhouse` → 注入 `ClickHouseUsageQuery`（单点启动断言，两种组合各一条） |
| T14a | **CH 整数被引号包裹**（CH-A，**最危险**） | 喂两种响应体：`{"requests":"8"}` 与 `{"requests":8}` → **都必须解出 8**；喂 `{"requests":"abc"}` → **必须 `decode_error`（503）而不是 0** |
| T14b | CH 空集 → `last_seen` 为空串（CH-B） | `{"requests":"0","last_seen":""}` → `as_of: null`（**不是空字符串**） |
| T14c | CH 查询失败分类（CH-D） | HTTP 404 + `Code: 60. DB::Exception: …` → `usage_store_unavailable`（503）+ 指标 `result=store_unavailable`；HTTP 5xx/连接超时 → 同码 |
| T15 | E3 时间窗与手写 SQL 对照 | 逐字段相等（含 NULL 语义与 `errors`） |
| T16 | E3 `since` 为空格分隔格式（历史遗留输入） | 归一化后正确；**对照"若不归一化会多算整天"的反例断言** |
| T17 | E3 窗口超上限 / `since > until` | 400 `window_too_large` / `invalid_since` |
| T18 | E3 的 `tenant_id` 只来自令牌 | 构造 `?tenant_id=other` 被忽略（契约无此参数 → 断言响应仍是自己的数据） |
| T19 | 令牌在**快照**（非 DB）：只改快照不改 DB | 鉴权仍按快照判定（**证明不读 DB**） |
| T20 | edge 语义：`pool=None` 且 `replication()==None` | E1/E2/E3 → 503 `not_ready`（fail-closed，绝不放行） |
| T21 | 失败限流 | 连续错误令牌 → 429 + 锁定；恢复后正常 |
| T22 | 前缀与业务路径边界 | `POST /v1/chat/completions` 仍**原样透传**（wiremock 收到 1 次）；`/tenant/t/api/v1/usage` 不落到上游（`received_requests` 为空） |
| T23 | E1 的 `base_url` 字段 | 与实际可用前缀一致（自述不自相矛盾） |

> ⚠️ **`AppState` 加字段会打断 12 处测试构造点**（另 1 处是生产构造 `main.rs:571`；见 §7.2）→ **必须同批**提供 `AppState::for_tests()` 并迁移。

### 10.3 集群（`--features server,cluster-redis,usage-clickhouse`，**真实 Redis**）

文件首行 `#![cfg(feature = "cluster-redis")]`；连接用 `common::real_redis_pool(<db>)`（形制见 `tests/admin_api.rs:1562`、`tests/redis_real.rs`，db 41–63 分区）。`HYDRA_TEST_REDIS_URL` 未设时**必须 panic**（`tests/common/mod.rs:45-52`），**不得**退化成 mock 或静默跳过。

| # | 用例 | 期望 |
|---|---|---|
| C1 | edge 上的 E2（单集群，1 个数据面节点） | 本节点 L1 清空 + 失效流出现一条 **v=2** 记录，且**断言载荷里只有摘要、无明文 key**（对齐 `tests/admin_api.rs:1550-1645` 的既有流载荷断言） |
| C2 | **有失效流但通道失败**（原始措辞"集群成员但无 Redis 后端"**运行期不可构造**：leader/edge 缺 `HYDRA_REDIS_URL` 会拒绝启动 → 已修订） | 用指向**已关闭端口**的 Redis 构造流 → E2 → **503 + `fleet.state:"unavailable"`**（**不是** `single_node`、**不是**假的 `applied`；D3 的回归） |
| C3 | 单节点 `all` | E2 → `fleet.state = "single_node"`、200（本地清即全部） |
| C4 | **收敛屏障：两节点 + 消费者都在跑** | 发布后两节点水位均前进；E2 返回 **200** + `state:"applied"` + `nodes_applied == nodes_total` |
| C5 | **收敛屏障：远端消费者停摆** | E2 在 `timeout_ms` 后返回 **202** + `state:"pending"` + `lagging` **精确列出该节点**；`consumer_stalled_seconds` 上升 |
| C6 | **先 apply 后 ack 的顺序（关键负例）** | 构造"消费者读到事件但 apply 前中止" → 水位**不得**前进（若实现成先 ack 后 apply，此用例必须失败） |
| C7 | generation bump 路径 | 让流里的事件数超过 `maxlen` 使 `XTRIM` 删除条目（**不依赖"未读"条件**，裁剪无法区分）→ 各节点 `clear_all()` 且水位**不推进**；若同时有在途事件，E2 报 `pending` 而**不是** `applied` |
| C8 | 屏障**不是转发** | 抓取 E2 期间的出站：本节点未向任何对端发起 HTTP（无 `x-hydra-forwarded`）；只读写共享 Redis |
| C9 | 存活节点集合 | 心跳过期的节点**不**出现在 `nodes_total`，也不阻塞收敛 |
| C10 | 失效主干的边界 | 两个独立 Redis 的实例互不影响：在 A 上发 E2，B 的节点水位**不动**（把"覆盖范围 = 本集群"这条契约的边界钉成可执行断言；Q13 确认单集群，但边界本身不能靠假设） |
| C11 | `HYDRA_AUTH_ALLOW_TTL_MAX_SECS` 生效 | wiremock 认证应答带 `expires_in=86400` → 缓存项 TTL 被封到上限，`hydra_auth_allow_ttl_capped_total` 增加 |
| C12 | edge（无 DB）上的 E1 | 200 且来自快照（**证明 E1 不需要 DB、不需要转发**） |
| C13 | 集群下的 E3（edge 节点） | **200 且 `source: "clickhouse"`**，数字与直连 CH 手跑 SQL 一致（**证明集群下用量端点可用、且不需要转发**） |
| C13a | CH 不可达 | CH 停掉/端口指错 → 503 `usage_store_unavailable` + `usage_query_seconds` 有值；**不是 200 假 0、不是 panic** |
| C14 | 令牌索引随快照 | leader 改令牌 → `reload_all` → 远端（含 edge）用**新令牌**可鉴权、旧令牌 401 |
| C16 | 旧路径已删除 | `POST /api/v1/tenants/{id}/auth/cache/invalidate` 在管理口返回 404（**且不是 401**——证明路由整块消失，而不是被闸门挡住） |
| C17 | 失效事件重复消费 | 幂等，不报错 |
| C18 | 数据面不得暴露内部面 | `/api/v1/internal/*` 在 8080 上不是内部路由；断言内部面只在 8081 |

### 10.4 既有门禁命令（逐条，CI 同款）

```bash
export RUSTFLAGS="-D warnings" SQLX_OFFLINE=true          # ci.yml:8-16
export HYDRA_TEST_REDIS_URL="redis://127.0.0.1:6379"      # 集群用例必需（tests/common/mod.rs:45-52）

cargo update --workspace --locked --dry-run                                                  # ci.yml:58
cargo fmt --check                                                                            # ci.yml:61
cargo clippy --workspace --all-targets --features hydra-server/server -- -D warnings         # ci.yml:69
cargo build --workspace --features hydra-server/server                                       # ci.yml:72
cargo test -p hydra-core                                                                     # ci.yml:77
cargo tree -p hydra-core --no-default-features                                               # ci.yml:86（依赖防火墙）
cargo test -p hydra-server --features server                                                 # ci.yml:100

# 三特性矩阵（environment/build.sh 出镜像用的正是这组，破了就是出坏二进制）
cargo clippy --workspace --all-targets --features hydra-server/server,hydra-server/cluster-redis,hydra-server/usage-clickhouse -- -D warnings  # ci.yml:140
cargo build  --workspace --features hydra-server/server,hydra-server/cluster-redis,hydra-server/usage-clickhouse                              # ci.yml:143
cargo test   -p hydra-server --features server,cluster-redis,usage-clickhouse                                                                 # ci.yml:146

node scripts/check_i18n.js && node --test scripts/check_i18n.test.cjs && bash scripts/ask_llm.test.sh   # ci.yml:162-168
```

**本改动特有的额外门禁**：

| 门禁 | 命令 / 内容 | 理由 |
|---|---|---|
| 浏览器腿（**强制**） | 起 release 二进制 → `./tests/e2e/seed.sh` → `npx playwright test --config=playwright.config.cjs` | 本设计改 `admin-ui/app.js` 与 `api-docs.js`；仓库纪律：Web 应用必须经 Playwright 端到端验证（`dev-docs/waves/wave-6-ui-hardening.md:5/86`），CI 的 `ui-e2e` job 即如此（`ci.yml:188-253`） |
| 生产代码 grep 门禁 | `rg 'unwrap\(\)\|expect\(|panic!\|unimplemented!\|todo!' crates/hydra-server/src` 必须为空；人工确认无 mock/stub/`#[cfg(test)]` 分支 | `dev-plan.md:197-206` |
| 反熵核对 | 三个 handler **只有一份实现**（旧路由按 M1 删除，不是保留别名）；`tenant_id_for_token` 旧实现删除、不留双实现；管理口的租户令牌路由块整块消失（C16 断言 404 而非 401）；v1 的域名/URL 校验模块**不存在**（不是留着不用）；`usage_backend` 枚举**不存在**（被 `UsageQuery` 注入取代） | §2.3、§3.0 规则 1/3、§8.1 |
| `.sqlx/` | 本设计用运行时 `sqlx::query` → **无需重生成**。若改用 `query!`/`query_as!` 宏则必须按 `dev-docs/HANDOFF.md:167-190` 重生成并提交（三个坑：`--features db` 不能单独编译、特性要放在 `--` **之后**、`prepare` 写到 crate 目录必须搬回仓库根） | `SQLX_OFFLINE=true` 是全局编译前提（`ci.yml:11`） |
| **CH 传输提取的零行为差异**（Q14=T1 的回归门禁） | 提取 `clickhouse.rs` 的那一次 commit **必须**让既有 4 条 `clickhouse_sink` 测试（含 `#[ignore]` 的那条在本机活实例上手工跑）**全部原样通过**；该 commit 只做搬迁，不夹带读路径代码 | 这是本次唯一触碰生产写通道的改动；把"搬迁"与"新增"分成两个 commit，是为了让回归失败时能立刻定位到"是搬迁搬错了"而不是"新代码有问题" |
| CH 读路径的活实例门禁 | 依既有约定（`tests/clickhouse_sink.rs:10-13/173`）：**纯函数**（SQL 构造、JSONEachRow 解码、`""`→`None`、引号整数）用确定性单测覆盖；**真连 ClickHouse 的那条**标记 `#[ignore]`，手工跑：`CH_URL=http://127.0.0.1:8123 cargo test -p hydra-server --features server,usage-clickhouse --test usage_query -- --ignored`。本机已有 `hydra-local-clickhouse`（`environment/docker-compose.local.yml`），本次设计中的 6 条 CH 事实即由它实测得出（§4.3.3） | 与仓库既有 CH 测试策略一致；`decode_error` 那类必须能用喂固定响应体的方式确定性地测 |
| Python e2e（可选） | `python3 integration/e2e_proxy_test.py`（数据面冒烟）。注意 `integration/` **不在 CI 里**，且既有套件**从未覆盖**租户令牌端点（`grep "auth/cache/invalidate" integration/` = 0 命中）→ 租户 API 的端到端覆盖**以 Rust 集成测试为准** | `dev-docs/design-tenant-model-catalog.md:221-234` 的同款措辞 |

### 10.5 验收证据的落档方式（**不是** `.acceptance/`）

`.acceptance/` 是**被 gitignore 的本机工具链暂存目录**（`.gitignore:58-60`），**不是**验收机制。仓库约定是**写回受版本控制的 markdown**：

1. 实施计划 `dev-docs/aegis/plans/<date>-tenant-api.md` 末尾回填 `## 实施记录`（状态 + `| 项 | 结果 |` 表 + 精确 pass 计数 + `执行期修正` + `遗留`）；
2. 新增/更新权威文档章节：`dev-docs/design.md`（租户 API 章节 + §13.2 拆分）、`dev-docs/ops.md`（§5.1 改路径 + 新增两节）；
3. `dev-docs/aegis/INDEX.md` 追加一行（`Date | Kind | Path | Title`，`kind=doc`）。

**验收判定**：上述命令全绿 + fmt/clippy 零告警 + 两种特性组合测试通过 + Playwright 全绿 + **code review 无 P0/P1 遗留**（沿用 `dev-docs/design-tenant-model-catalog.md:234` 的判定句式）。

---

## 11. 待决策项

> **已答/已决项不在本表。** 表后 §11.2 列出本轮已收口的决策与答案。

| # | 决策 | 选项 | 建议 |
|---|---|---|---|
| Q1 | 前缀形态 | 需求给定的 `/tenant/{tenant_id}/api` ↔ 行业惯例的 `/-/tenant/{tenant_id}/api` | **前者**（需求即契约）。两者都把该前缀变成保留路径；后者唯一优势是不与"上游可能存在的 `/tenant/*`"冲突，而 LLM provider 不会提供这类路径 |
| Q2 | 是否保留只读的 `whoami` | 保留 ↔ 只做 E2+E3（两件事都能间接证明令牌有效） | **保留**。成本近零（纯快照、无 DB）、且它是唯一的"平台认为我是谁/我在哪个域名上"的自述入口，能消化大量支持工单 |
| Q5 | 是否提供用量明细 | v1 不做 ↔ 一并做 | **v1 不做**（§4.3.5）。补充理由（v4）：CH 侧**没有等价的单调自增列**，明细分页需要另一套游标语义 |
| Q6 | `ConfigData` 派生索引 | 新增 `tenants_by_id` ↔ `whoami` 线性扫描 `tenants_by_domain.values()` | **新增派生索引**（同源构建，非第二 owner；顺带让存在性判定 O(1)） |
| Q7 | `enabled` 是否作为闸门 | 全不闸（推荐）↔ 对 E3 也闸 | **全不闸**：只剩读与缓存删除，停用租户的自救必须畅通 |
| Q8 | `AppState` 迟到资源 | 构造后移（推荐）↔ `ArcSwapOption`/`OnceLock` 后置填充 | **后移**（只 1 个迟到资源，改动小）；顺带提供 `AppState::for_tests()` 收敛 12 处测试构造点 |
| Q9 | 管理口 `auth_url` 写入的网段校验（**既有边界，非本次需求**） | 本次顺带做 ↔ 单列小改动 | **单列**：本次变更不去碰管理 API 的既有行为；但它值得一个独立小任务（含 `POST /api/v1/tenants/auth/test` 探活） |
| Q10 | E2 默认是否等待收敛 | 默认 `wait=converged`（预算 2s）↔ 默认立即返回 + 客户端自行轮询 | **默认等待**。端点的意义就是"让封禁立刻在整条数据面生效"，同步确认才是正确默认；不接受长延迟的调用方可传 `wait=none` |
| Q11 | 收敛屏障的位置 | 扩 `InvalidationStream`（推荐）↔ 新建独立 `InvalidationBarrier` ↔ 放进 `tenant_api` | **扩 `InvalidationStream`**：水位与失效流共用 `hydra:{ctl:*}` 命名空间与同一个 Redis 池，拆开就是第二套"谁负责让全集群失效"的答案 |
| **Q15** | **CH 表的排序键是否改**（v4 新增） | 保持 `ORDER BY (created_at, tenant_id, provider_id)` ↔ 改为 `ORDER BY (tenant_id, created_at)`（需重建表 + 回填） | **保持现状**。实测当前谓词**确实用到主键**（`Granules: 1/2`）；只在"按租户大量查询且窗口很宽"成为真实瓶颈时才值得改，属独立任务（§4.3.3 CH-E） |

### 11.1 已答项（本轮，2026-09-17）

| # | 问题 | 答案 | 落到设计哪里 |
|---|---|---|---|
| Q3 | ClickHouse 部署下的用量端点 | **生产必跑集群，用量走 ClickHouse，首发可用** → CH 读路径从"第二刀"提升为 **v1 必做** | §4.3.3（含 6 条实测事实）、§4.3.4（`AppState.usage` 能力注入取代 `usage_backend` 枚举）、§7.2（`sink.rs` 与新增 `usage_query.rs`）、§10.2 T14a–T14c、§10.3 C13/C13a |
| Q4 | 旧路径（migration 0009） | **尚未正式上线，无需考虑破坏性** → **M1 直接删除** | §8.1（含删除项收益与两个被否决选项）、§3.0 规则 1 去掉豁免、§7.2 `admin/mod.rs` 行 |
| Q12 | allow TTL 上限 | **默认 300 秒刚好** → 确认 | §4.2.5（残余窗口上界 ≤300s）、§7.3 配置表 |
| Q13 | 是否有多集群/多地域 | **不会** → 删除 `HYDRA_CLUSTER_ID` 与 `fleet.cluster`，覆盖范围直接定义为"本集群"，并把多集群列为重新评估触发条件 | §4.2.4 第 4 条、§6.4 A-1 的 reconsider 第 4 条、§7.3（配置项 10 → 9） |
| Q14 | CH 读通道的传输选型 | **按建议 = T1**：抽共享传输原语，写/读路径同用一个 owner | §4.3.3、§7.1 新增 `clickhouse.rs`、§7.2 `sink.rs` 行、§10.4 的"零行为差异"回归门禁 |

> 五项回答（Q3/Q4/Q12/Q13/Q14）带来的净效果：**删掉**一套弃用机制 + 一个响应字段 + 一个 `HYDRA_CLUSTER_ID` + 一个能力枚举 + 一条 501 分支；**新增**一条 CH 读通道（共享传输、JSONEachRow 解码器、超时、`decode_error`/`usage_query_seconds` 指标与 5 条测试）。三个端点在全部角色（含集群 edge）上可用，且**仍然没有任何转发**。
>
> **剩余 9 项（Q1/Q2/Q5–Q11）全部是工程内部选择或有明确建议值**，没有需要业务方再拍板的开放问题——设计已具备进入实施计划的条件。

## 12. 风险与回滚

| 风险 | 级别 | 缓解 | 回滚 |
|---|---|---|---|
| 租户令牌在公网端口被爆破 | 中 | 快照校验（去掉 DB 放大）+ 双维度**失败**锁定（B1：仅作用于鉴权失败路径，持有效令牌的请求永不被它拒；锁定窗内放弃无有效性预言机，理由见 §5.1）；LB 后配 `HYDRA_TRUSTED_PROXIES` 使每 IP 维度按真实客户端计数 | 总开关 `HYDRA_TENANT_API=off`；或只在可信网络开放租户 API |
| 失效风暴诱发全集群清缓存（D6） | 中 | 每租户失效频率上限 | 临时下调 `INVALIDATE_PER_MIN` |
| **E2 返回 202（未收敛）被误读为成功** | 中 | 状态码用 202 而非 200；`state`/`lagging` 显式；`invalidate_pending_total` 可告警；`api-docs.js` 与 `ops.md` 给出 202 的处置流程 | 无（语义即如此）；运维侧对 pending 告警 |
| **消费者停摆的节点上陈旧 allow** | 中 | `HYDRA_AUTH_ALLOW_TTL_MAX_SECS` 给出与租户无关的硬上界；`consumer_stalled_seconds > 60s` 告警 | 调小 `ALLOW_TTL_MAX_SECS` 以更快收敛（代价是认证回源上升） |
| 收敛屏障把共享 Redis 变成热点 | 低 | 水位**按批** HSET（不是按事件）；等待循环有 `timeout_ms` 上限与短轮询间隔；E2 本身有每租户频率上限 | 调大轮询间隔或关闭等待（`wait=none`） |
| allow TTL 封顶导致租户认证服务压力上升 | 中 | 默认值 = 现状（零变化）；`hydra_auth_allow_ttl_capped_total{tenant}` 量化影响面；`ops.md` 给出按容量调参的建议 | 调大 `ALLOW_TTL_MAX_SECS`（代价是封禁生效变慢，二者只能取一） |
| 租户查用量把查询本身算进用量 | 低 | §3.2 短路序保证 `ctx.selected` 为空（T9 覆盖） | 无（设计保证） |
| **CH 响应里 64 位整数被引号包裹，解析器只认 number** | **高** | §4.3.3 CH-A 实测记录；解码器两种形态都接受；T14a 用 `{"requests":"8"}` / `{"requests":8}` / `{"requests":"abc"}` 三条断言；`decode_error` 指标可告警 | 无（必须实现期覆盖；这是唯一会以"静默 0 用量"形式失败的缺陷） |
| CH 查询宽窗口的扫描代价（主键前导为 `created_at`） | 中 | 窗口上限（默认 31 天）+ `usage_query_seconds` 直方图 + CH-E 记录的 `ORDER BY` 建议 | 调小 `USAGE_MAX_WINDOW_DAYS`；规模化需求走 Q15 |
| CH INSERT 重试导致重复行 ⇒ `requests` 高估 | 中 | §4.3.3 CH-F；CH 表无 `trace_id` 列无法去重 → 契约把 `requests` 标注为**近似值**，`ops.md` 写明成因 | 无（已知偏差，需在文档中声明而非隐藏） |
| **改动正在工作的 CH 写通道**（Q14 已选 T1） | 中 | **把改动拆成两个 commit**：commit 1 只做"传输下沉到 `clickhouse.rs`"（零行为差异，靠既有 4 条 `clickhouse_sink` 测试 + 本机活实例的 `#[ignore]` 那条作回归网）；commit 2 才新增读路径。回归失败时能立刻区分"搬迁搬错"与"新代码有问题"（§10.4） | 回滚 commit 1 即回到现状；读路径随之推迟 |
| **注入错 `UsageQuery` 实现导致返回假的 0** | 中 | 启动时单点注入（取代运行时分支）；T14 断言注入实现与 `sink_kind` 一致（两种组合各一条）；`source` 字段由实现自报，租户可自行核对 | 无（必须在实现期覆盖） |
| `AppState` 加字段打断测试编译 | 低 | `for_tests()` 收敛 12 处测试构造点（同批；生产构造在 `main.rs:571` 另行后移） | 无（纯机械） |
| 需求移除后**运维工单量上升**（改域名/auth_url） | 中 | `ops.md` 新增运维流程节；Admin UI 表单与探活按钮已存在 | 若要恢复自助写，**必须重新做 T1/T2 的全部对策**（§5 的已移除威胁表就是那份清单） |
| 前缀保留导致既有透传行为变化 | 低 | 前缀几乎不可能与 provider 路径冲突（T22 覆盖）；总开关可完全回到基线 | 总开关 |
| 派生索引 `tenants_by_id` 与 `tenants_by_domain` 漂移 | 低 | 同源构建于同一 loader 函数；`config_data.rs`/`entities.rs` 既有形状测试覆盖两者 | 无 |

---

## 附录 A：现状认证缓存的结构、TTL 语义与失效流程（事实基线）

> 本附录记录**代码现状**（不是设计意图），作为 §4.2「三层保证」与 §4.2.5「残余窗口上界」的事实基础。所有断言带 `file:line`。

### A.1 结构图

```
                     ┌───────────────────────────────────────────────┐
                     │ main.rs / AppState / AdminState               │
                     │   auth: Arc<HttpAuthChecker>  ← 两侧同一个 Arc │
                     │   （main.rs:451 建，:573/:984 分发）           │
                     └───────────────────────┬───────────────────────┘
                                             │
        ┌────────────────────────────────────┴─────────────────────────────────┐
        │ HttpAuthChecker                                   http.rs:451        │
        │   cache:  AuthCache            ← 判定与缓存                    │
        │   client: reqwest（独立连接池，与上游通道隔离）                  │
        │   config: AuthConfig { allow_ttl 300s, deny_ttl 30s,             │
        │                        timeout 2000ms, fail_mode Closed }        │
        │                        http.rs:414-423（默认值）                  │
        └───────┬───────────────────────────────────────────┬──────────────┘
                │ check(tenant, key)              invalidate / invalidate_tenant
                ▼                                                 │
   ┌────────────────────────────────────────┐                     │
   │ AuthCache                      http.rs:58│◄────────────────────┘
   │  map: DashMap<(String,[u8;32]), AuthEntry>   ← L1，进程内，零网络
   │  epoch: AtomicU64         ← 失效代际（N1 竞态守卫）
   │  allow_ttl / deny_ttl     ← 只用于「写」时算 expires_at
   │  now: Clock               ← 可注入，便于确定性测试
   │  l2: Option<Arc<RedisAuthL2>>  ← 仅 cluster-redis 构建存在
   └───────────────┬────────────────────────┘
                   │ 只有 L1 **未命中**（含"存在但已过期"）才往下
                   ▼
   ┌────────────────────────────────────────┐
   │ RedisAuthL2            redis/auth_cache.rs:39
   │  pool: fred::Pool       ← 与租约/注册表/失效流共用同一个 Redis
   └────────────────────────────────────────┘
                   │ 只有 L2 也未命中才往下
                   ▼
   ┌────────────────────────────────────────┐
   │ 租户 auth_url（外部 HTTP POST，2s 超时）│  ← 真源；返回 allow/deny
   └────────────────────────────────────────┘
```

**判定逻辑在纯核心里**：`cache_decision(entry, now)`（`crates/hydra-core/src/auth.rs:123-128`）——`now < expires_at` 即 `Hit(allowed)`，否则 `Miss`；`AuthCache::l1_decision`（`http.rs:183-190`）只做 map 取值 + 调用它，**同步函数**（刻意如此：`DashMap::get` 返回的 `Ref` 是分片读锁，跨 `.await` 会自死锁 —— `bug-2026-09-16-auth-cache-guard-deadlock`）。

### A.2 数据结构与键空间

| 层 | 存储 | 键 | 值 | TTL 载体 |
|---|---|---|---|---|
| **L1** | 进程内 `DashMap` | `(tenant_id, sha256(api_key) 原始 32 字节)` | `AuthEntry { allowed: bool, expires_at: Instant }`（`auth.rs:96-103`） | **值里的绝对 `Instant`**，不是 Redis 式 TTL |
| **L2** | 共享 Redis | `hydra:{auth}:{tenant_id}:{keyhash_hex}` | 字符串 `"1"` / `"0"` | **Redis 原生 TTL**（`SET … PX(ttl)`） |
| L2 租户索引 | 共享 Redis | `hydra:{auth:idx}:{tenant_id}`（SET） | 该租户的 keyhash 集合 | 无独立 TTL（随 `del_tenant` 整体删） |
| L2 全局索引 | 共享 Redis | `hydra:{auth:idx}`（SET） | 有 L2 判定的 tenant_id 集合 | 无（随 `del_all_tenants` 删） |

- **明文 api-key 从不入内存、不入 Redis**：键是 `sha256(api_key)` 的**原始字节**（L1）或**小写 hex**（L2 / 失效流载荷），`http.rs:55-57`、`hex_digest` 是唯一转换点（`http.rs:882-890`，审计 L-1）。
- L2 索引存在的唯一原因：命名空间规则**禁止 `SCAN`**，而"按租户清空"和"全清"必须能枚举 → 用 SET 索引代替扫描（`redis/auth_cache.rs:28-35`）。
- 写入顺序 **索引先、值后**（N2，`redis/auth_cache.rs:70-77`）：反之则"有值无索引"的项会躲过按租户的失效；悬空索引成员无害（只是多删一次不存在的键）。

### A.3 请求路径流程图（`check`，含 TTL 行为标注）

```
 request_filter (proxy.rs:467)
        │
        ▼
 HttpAuthChecker::check(tenant, api_key)                    http.rs:511
        │
        ├─(1) tenant.auth_url 为空？ ──是──► 401 no_auth_url（Local，不缓存）:528
        │
        ├─(2) AuthCache::check(tenant_id, api_key)           http.rs:192
        │        │
        │        ├─ L1 查表（同步，一次 now()）              http.rs:183-190
        │        │     ├─ Hit(allowed) ──► 返回 ⚑ TTL 不动、不写、不碰 Redis
        │        │     └─ Miss（缺失 **或 已过期但未 GC**）
        │        │
        │        ├─ L2 查（仅 cluster-redis）                http.rs:208-221
        │        │     ├─ GET hydra:{auth}:{t}:{h} + PTTL
        │        │     ├─ 命中 ──► 回填 L1：expires_at = now + 剩余PTTL ⚑ 重写 L1，不动 L2
        │        │     │            返回 Hit(allowed)
        │        │     └─ 未命中/不可解析 ──► Miss
        │        └─ 返回 Miss
        │
        ├─(3) epoch_before = cache.epoch()                   http.rs:556  ← 采样必须在往返之前
        │
        ├─(4) POST auth_url（Bearer <明文key> + X-Hydra-Tenant）  2s 超时  :559-579
        │        ├─ 连接失败/超时 ──► fail_mode（默认 closed → 503，**不缓存**）:570-578
        │        └─ 有响应
        │              └─ apply_upstream(status, allow_ttl, deny_ttl)  auth.rs:140
        │                    ├─ 2xx  ──► Set{allowed:true,  ttl = expires_in ?? allow_ttl}
        │                    ├─ 401/403 ──► Set{allowed:false, ttl = deny_ttl}
        │                    ├─ 402  ──► 直接透传，**永不缓存**（余额瞬息万变）:583-596
        │                    └─ 5xx/其他 ──► None（不缓存，走 fail_mode）
        │
        └─(5) set_if_unchanged(epoch_before, …)              http.rs:125-142
                 ├─ epoch 变了（期间发生过失效）──► **不写缓存**，但本次请求仍用该判定
                 └─ epoch 未变 ──► set()：L1 写入 now+ttl；L2 SET … PX(ttl)   :227-241
```

### A.4 **有没有请求递增 TTL？——没有。** 逐路径对照

| 请求情形 | L1 TTL | L2 TTL | 结论 |
|---|---|---|---|
| **L1 命中** | **完全不动** —— `l1_decision` 只读不写（`http.rs:183-190`），`check` 命中即 `return`（`:200-202`） | 根本未访问 Redis | **不回滑** |
| **L1 未命中 → L2 命中** | 条目被**重写**，`expires_at = now + PTTL(剩余)`（`http.rs:210-219`） | **不动** —— `get` 只做 `GET` + `PTTL`（`redis/auth_cache.rs:50-68`），没有 `EXPIRE`/`PEXPIRE` | **不是续期**：重建出的绝对截止点仍锚在 L2 的原截止点上，因此 **L1 永远不会活得比 L2 更久**（这条性质正是回填安全的原因） |
| **两层都未命中 → 上游放行/拒绝** | 全新 `now + ttl`（`:229`） | 全新 `SET … PX(ttl)`（`redis/auth_cache.rs:88-97`），**整段新 TTL** | **只有"过期后重新判定"才拿到全新 TTL** |
| 失效期间在途的判定 | 不写（epoch 守卫） | 不写 | **不存在"竞态请求把 TTL 续上"** |
| GC 扫描（60s 一次，`main.rs:617`） | 删除已过期项（`http.rs:333-338`） | 不涉及 | 纯内存回收；过期项在语义上**早已是 Miss**，GC 不影响判定 |

**所以 TTL 的语义是**：`expires_at` 在**写入那一刻**定死，此后**无论来多少次请求都不会延长**——是 absolute / non-sliding TTL，不是 rolling TTL。

**但由此产生一个必须知道的**涌现行为**：

```
       t=0            t=300s         t=600s         t=900s
        │               │              │              │
   [写 L1+L2, TTL=300s]  │              │              │
        │               │              │              │
   请求 × N 次命中 ───────┤ TTL 到期，两层同时失效      │
        │               │              │              │
        │           第 1 个请求 Miss ──► 回源 auth_url ──► 重新写入"整段 300s"
        │               │              │              │
        └───────────────┴──────────────┴──────────────┘
        只要"每 300s 内至少被用一次"，该 key 就**永远**留在缓存里，
        但每 300s 会**重新向租户 auth_url 求证一次**。
```

即：**每个 TTL 窗口一次回源**（不是"一次缓存、永久有效"），这正是 §4.2.4 第 1 条「先封禁后失效」之所以关键的原因——只要租户侧先改判，下一个窗口就会自然收敛；反过来若租户先清缓存再封禁，回源会立刻把 allow 重新写回整段 TTL。

**两个附带事实（同样是现状，不是设计）**：

1. **没有 single-flight / 防击穿**：TTL 边界上并发的 N 个同 key 请求会**全部** Miss 并**各自**发一次 `auth_url`（`check` 返回 `Miss` 后无任何去重，`http.rs:537-547` → `:559-566`）。热 key 的 TTL 越短，这个尖峰越频繁——这是 §4.2.5 把 allow TTL 上限默认值取"现状值"而不是更小值的原因之一。
2. **`len()` 含未清扫的过期项**：`map.len()`（`http.rs:157-160`）是物理条目数，GC 每 60s 才跑一次，所以 `hydra_auth_cache_size` 最多有 60s 的滞后。

### A.5 失效接口的流程（现状）

**入口有两个**（同一套底层原语，两条独立闸门）：

| 入口 | 路径 | 鉴权 |
|---|---|---|
| 管理端 | `DELETE /api/v1/auth/cache`（`admin/mod.rs:316-318` → `handlers.rs:1589`） | `HYDRA_ADMIN_TOKEN` |
| 租户自助 | `POST /api/v1/tenants/{tenant_id}/auth/cache/invalidate`（`admin/mod.rs:616-668` → `handlers.rs:1747`） | 租户 access token（且 URL tenant_id 必须与令牌归属一致） |

**本节点执行流程**（`handlers.rs:1589-1668`）：

```
① 闸门（admin token / 租户令牌 + URL 交叉校验）
        │
② 读 body → InvalidateRequest { tenant_id?, api_keys? }      :1534-1538
   （空 body 视作 {None, None} = 全清，curl -X DELETE 无 body 也要能用）
        │
③ invalidate_shape_error：keys ≤ 1000、单 key ≤ 4096 字节    :1565-1587
        │
④ 四分支派发                                                  :1614-1639
   ┌────────────────────────────┬──────────────────────────────────────────┐
   │ (Some(t), Some(keys))      │ AuthCache::invalidate(t, keys)           │
   │                            │   epoch++ → 逐 key: L1 remove + L2 DEL   │
   │                            │            （http.rs:245-259）            │
   ├────────────────────────────┼──────────────────────────────────────────┤
   │ (Some(t), None)            │ AuthCache::invalidate_tenant(t)          │
   │                            │   epoch++ → L1 retain(全表) + L2 del_tenant│
   │                            │            （http.rs:291-300）            │
   ├────────────────────────────┼──────────────────────────────────────────┤
   │ (None, Some(keys))         │ 遍历**配置快照里全部租户**，逐租户 invalidate│
   │                            │   （跨租户按 key 匹配，:1617-1626）        │
   ├────────────────────────────┼──────────────────────────────────────────┤
   │ (None, None)               │ 遍历全部租户，逐租户 invalidate_tenant     │
   │                            │   （= 清空本节点整个 L1，:1627-1638）      │
   └────────────────────────────┴──────────────────────────────────────────┘
   注：租户自助入口**没有** (None, *) 分支——tenant_id 恒由令牌决定（:1769-1772）
        │
⑤ InvalidationStream::publish(tenant_id, api_keys)            :1645-1657
   → 在**发布边界**把明文 key 哈希成 SHA-256 hex（v=2 载荷）
   → XADD hydra:{ctl:events}（events.rs:63-86）
        │
⑥ record_auth_cache_size(cache.len())                        :1659
        │
⑦ 200 {"invalidated":L1删除数, "tenant_id":…, "published":bool}  :1660-1667
```

**集群侧（其他节点如何清）**：

```
每个节点：spawn_invalidation_consumer(stream, auth, store)     events.rs:287
   loop {
     XREAD hydra:{ctl:events} since last_id （批量 100）        :268
     对每条事件 apply_invalidation(cache, inv, known_tenants)   :233-265
        ├─ (Some(t), keyhashes 非空) → invalidate_hashes(t, hashes)  ← 不再哈希，直接用摘要
        ├─ (Some(t), 空)             → invalidate_tenant(t)
        ├─ (None,    空)             → clear_all()
        └─ (None,    keyhashes 非空) → 遍历 known_tenants 逐个 invalidate_hashes
     last_id = 该条 ID                     ← ⚠ 只存在局部变量，从不对外发布
     检查 generation（hydra:{ctl:gen}）     :320-332
        └─ 变了 → clear_all()  ← 裁剪掉了未读事件时的兜底；清 L1 **和** L2
     空闲 sleep ≤500ms                                                   :272
   }
裁剪任务：trim_and_maybe_bump(maxlen=10_000, 30s)  main.rs:632-639
   └─ 裁剪删除任何条目（XTRIM removed>0，无法区分已读/未读）→ generation++ → 触发所有节点 clear_all()
```

### A.6 三条最容易踩的不变量（直接决定 §4.2 的设计）

| # | 不变量 | 证据 | 推论 |
|---|---|---|---|
| I1 | **L1 命中时根本不查 L2** | `check` 命中即 `return`（`http.rs:200-202`） | 只删 L2 对**持有 L1 命中的热节点毫无作用**——这就是 "E2 只清本节点" 不能成立的根因（§4.2.2） |
| I2 | **`clear_all`（含全部失效路径）必须同时清 L2** | `http.rs:302-328`，注释记录了 B2 缺陷：只清 L1 时，下一次 L1 miss 会把陈旧判定从 L2 **原样复活** | 任何"清缓存"实现漏掉 L2 都是无效实现 |
| I3 | **`epoch++` 是每条失效路径的第一步** | `http.rs:246/267/292/313` | 失效期间在途的判定会被 `set_if_unchanged` 拒写（N1）——所以失效**不会**被竞态请求撤销 |

### A.7 现状与 §4.2 的对应关系

| 现状事实 | 对应设计条目 |
|---|---|
| I1（L1 不查 L2） | §4.2.2 第 1 层「L1 扇出」**不可省** |
| I2（清必须清两层） | §4.2.2 第 2 层「L2 权威」 |
| `last_id` 从不发布（`events.rs:293/311`：`last_id` 在循环内声明与推进） | §4.2.3 第 3 层「收敛屏障」——这是"清干净没有"唯一可回答的方式 |
| TTL 绝对不过期回滑 + `expires_in` 可抬高 allow TTL | §4.2.5 `HYDRA_AUTH_ALLOW_TTL_MAX_SECS`（把残余窗口从"租户决定"改成"运维决定"） |
| 消费者停摆只 `warn!`（`events.rs:336-338`） | §4.2.6 `consumer_stalled_seconds` 告警 |
| 无 single-flight | §4.2.5 默认值取现状值（不取更小）的理由之一 |

---

## 13. 修订记录

| 日期 | 版本 | 变更 |
|---|---|---|
| 2026-09-17 | v1 | 初稿：现状分析（D1–D10）、三平面模型、数据面保留前缀拦截、快照令牌索引、五个端点（含域名/auth_url 自助设置）、T1/T2 安全论证、集群转发方案（管理口同路径挂载）、待决策 Q1–Q8 |
| 2026-09-17 | **v5.2** | **第 1 轮 oracle 复审（逐条事实证伪）findings 全部处置**：**[F-1]（FALSE）** `AppState` 构造点计数错误——写作"13 个测试构造点"，实为 **12 处测试**（`terminate_mode.rs:224` **内含于** `:206-233` 的 helper，被重复计了一次）；仓库内 `AppState { .. }` 字面量共 **14** 处 = 1 结构体定义（`proxy.rs:108`）+ 1 生产构造（`main.rs:571`）+ 12 测试。两文档的计数、列举与两条用它做完整性校验的语句全部改正，并加上可执行的计数校验命令（**唯一有效形态**：`grep -rn "AppState {" crates/ --include=*.rs | grep -v "impl AppState" | wc -l` → T4 之后为 **2**；裸 `grep -c "AppState {"` 会数到 `impl AppState {` 给出 3；**14 是改造前的历史值**）。**[I-1]（IMPRECISE）** `events.rs:291` 是签名收尾行，`last_id` 实际在 `:293` 声明、`:311` 推进——两文档 7 处引用改为 `events.rs:293/311`。**[I-2]（IMPRECISE，**实质上把风险说轻了**）** 失效流裁剪的 generation bump **不是**"仅当丢掉未读条目"：`trim_and_maybe_bump` 用的是 `XTRIM MAXLEN` 的 `removed > 0`（`events.rs:204-213` 的 Lua），**裁剪在原理上无法区分已读/未读** → 只要持续 **>333 事件/秒**（`maxlen=10_000` ÷ 30s 间隔）每个 trim 周期都会让**全部节点清空整个 L1+L2**，**与消费者是否落后无关**。D6 / §5.1 / §6.3 / C7 与附录 A.5 的措辞全部改正，"只裁已读条目就无害"的错误假设已显式否证。另采纳复审的两处精度修正：`hydra-core` 依赖白名单行号 `Cargo.toml:9-16`、以及 §3.2 的步骤列表原把 (2.5) 画在 (3) 之上（代码里 `:396` 先执行并喂给 `:454`）已修正为物理顺序并加注"第 0 步必须早于 `:396`"。其余 27/30 条断言（含 6 条 ClickHouse 事实被复审用 curl **独立复测**、逐字节一致）确认为真 |
| 2026-09-17 | **v5.1** | **修正一处事实错误**：§7.1 原把"RFC3339（带偏移）/ epoch → `%Y-%m-%dT%H:%M:%SZ`"放在 `hydra-core`，但 **`hydra-core` 没有 `chrono`、且依赖白名单不允许引入**（`crates/hydra-core/Cargo.toml:7-11`；`model.rs:299-300` 自述 "no `chrono` in core"）。改为**职责切分**：词法解析与归一化、窗口长度判定放 shell 的 `tenant_api/time_bound.rs`（用 `chrono`）；core 只做规范形态的字形+数值范围校验与顺序比较（纯字符串，零日历运算）。§4.3.2 第 1 条同步注明归一化 owner |
| 2026-09-17 | **v5** | **Q14 与 Q13 收口**：① **Q14 = T1** → CH 传输下沉为共享模块 `clickhouse.rs`（"怎么跟 CH 说话"的唯一 owner），写路径与读路径共用；§7.1 新增该模块条目，§7.2 `sink.rs` 改为"调用新模块、行为逐行不变"，§10.4 新增**"零行为差异"回归门禁**（把改动拆成"只搬迁"与"新增读路径"两个 commit），§12 相应风险从条件风险改为已选定风险并给出回滚点。② **Q13 = 不会（单一集群）** → **删除 `HYDRA_CLUSTER_ID` 与 `fleet.cluster` 字段**（反熵：只有一片数据面时，"清的是哪一片"没有可回答的对象），E2 的覆盖范围直接定义为"本集群"，并把"引入第二个集群"写成 §4.2.4 与 §6.4 A-1 的**重新评估触发条件**（约束被显式记录，而非遗失）；配置项 10 → 9。§11 待决策表收敛为 **Q1/Q2/Q5–Q11 共 9 项**（全部为工程内部选择或有建议值），已答项 5 项存档于 §11.1。**设计已具备进入实施计划的条件** |
| 2026-09-17 | **v4** | **三项待决已答，据其收口设计**：① **生产必跑集群、用量走 ClickHouse、首发可用** → CH 读路径从"第二刀"提升为 **v1 必做**：新增 §4.3.3（对仓库自带活实例 ClickHouse 24.3 实测得到 6 条硬事实：**64 位整数默认被序列化成 JSON 字符串**、空集 `MAX(created_at)` 返回 `""`、`{name:String}`+`param_*` 绑定实测抗注入、主键对范围+等值谓词确实裁剪 `Granules: 1/2`、错误响应为 404+`Code: N`、INSERT 重试可产生重复行且 CH 表无 `trace_id` 无法去重）、§4.3.4（**`AppState.usage: Arc<dyn UsageQuery>` 取代 `usage_backend` 枚举**——两个后端都在 v1 后分支已无必要，且分支写错正是"假 0"的成因）、§4.3.1 双后端 SQL（CH 版实测可跑）、新增 `usage_query.rs` 与 `HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS`、T14a–T14c 与 C13/C13a 共 5 条新测试、`decode_error` 指标；删除 `501 usage_query_unavailable` 分支。② **尚未上线** → 旧路径 **M1 直接删除**：§8.1 重写（管理面回到单一凭证语义、§3.0 规则 1 去掉唯一豁免、删除 `Deprecation`/`Sunset`/`legacy_route_total`/`published` 兼容字段），新增 C16 断言旧路由 404。③ **allow TTL 默认 300 确认**（§4.2.5）。§11 重构为「待决策 Q1/Q2/Q5–Q11/Q13 + 新增 Q14 CH 传输选型/Q15 CH 排序键」+「§11.1 已答项」；§12 风险表相应重写；**三个端点在全部角色（含集群 edge）上可用，仍然没有任何转发** |
| 2026-09-17 | **v3.2** | 新增 **§6.4 决策记录 A-1：E2 不转发到 leader 的 admin API**——含既有端点"能精确清除"的证据表、转发链路**技术可行**的确认、选项 A/B/C 对照、7 条否决理由（第 1 条决定性：扇出由共享失效流完成，转发不产生全集群清除；第 2 条：LB 指向 edge，转发等于清一个不承载租户流量的节点）、已接受代价与**重新评估的触发条件**；新增 **§6.5** 说明本项目的决策记录归属（无 ADR 体系 → 正文决策记录节）与 ADR 门禁的 Retro/Memory Filter 结论（未执行决策不建独立 ADR 文件）。§6.2 加交叉引用，§11 声明已决项不在待决策表内 |
| 2026-09-17 | **v3.1** | 新增 **附录 A：现状认证缓存的结构、TTL 语义与失效流程**（结构图 / 键空间 / 请求流程图 / "**没有请求递增 TTL**"的逐路径对照 / 失效接口的双入口流程与集群扇出图 / 三条不变量 I1–I3 及其与 §4.2 的对应表）。纯事实基线补充，无设计变更 |
| 2026-09-17 | **v3** | **需求收紧：E2 必须在「全部数据面节点」上清除，而不是只清本节点**（§4.2）。原设计把"发布到失效流"当成"已生效"，这是错的：远端节点的 **L1 命中项不会**因为发布方删了 L2 而消失（L1 命中不查 L2），而消费者的 `last_id` 只存在于局部变量里（`events.rs:293/311`：`last_id` 在循环内声明与推进）**从不对外发布** → `published: true` 与"已生效"无关。新增 **L3 收敛屏障**（每节点已应用水位 HASH + 发布方等待全部存活节点确认，长在既有 `InvalidationStream` 上，**不新增字段、不是转发**）、**200/202/503 三态**（取代会说谎的布尔）、**残余窗口硬上界** `HYDRA_AUTH_ALLOW_TTL_MAX_SECS`（默认 300 = 现状；今天该窗口由被停用租户的 `expires_in` 决定）、以及 `consumer_stalled_seconds` 等 7 个指标（"活着但不消费"今天完全不可见）。集群测试从 9 条扩到 17 条，新增"先 apply 后 ack""generation bump 不得假报 applied""跨集群隔离""屏障不是转发"等关键负例。配置项 6 → 9 |
| 2026-09-17 | **v2** | **需求变更：移除"通过接口自动设置绑定域名与 Auth URL"**（§2.3）。随之**删除**：域名 zone 白名单与 409 冲突处理、`auth_url` 形态/网段校验与请求期出口校验、窄写 DB 函数、`key_provider`/`reload_lock`/`snapshot_stale`、`enabled` 写闸门、管理口第二挂载点与全部 leader 转发机制、`host_mismatch` 指标、6 个环境变量。**后果**：威胁从 5 个缩到 3 个；`AppState` 增字段从 8 个缩到 2 个；端点从 5 个缩到 3 个（`whoami` 改为纯快照只读）；**不再存在任何转发路径**（§6.2）；唯一能力缺口收敛为"集群下用量端点 501"（§4.3.3）。配置项 11 → 6。运维代偿：`ops.md` 新增"租户改域名/auth_url"流程节 |

---

