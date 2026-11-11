# Hydra 集群模式（Cluster Mode）

> **集群开关只有一个：`HYDRA_REDIS_URL`**。设了它就是集群节点（共享数据面状态 + 配置扇出），
> 没设就是单节点，不需要 Redis、行为不变。
>
> **没有 raft、没有 leader、没有静态成员表**。配置权威是**一个共享的关系数据库**
> （PostgreSQL 或 MySQL，编译期选定）。每个节点同构：跑数据面、提供管理面、直接写共享库。
> 任意节点都能接受管理写；正确性来自数据库事务/约束，不来自共识。
>
> 权威文档：[`ADR-0004`](aegis/adr/ADR-0004-shared-database-control-plane.md)（决策）、
> [`deployment.md`](deployment.md)（部署）、[`ops.md`](ops.md)（运维）。本文是行为与契约。

---

## 1. 节点模型

| 状态 | 判定 | 数据面 | 管理面 | 配置权威 |
|---|---|---|---|---|
| 单节点（默认） | 没设 `HYDRA_REDIS_URL` | ✅ | ✅ 全部 | 共享关系数据库 |
| 集群节点 | 设了 `HYDRA_REDIS_URL` | ✅ | ✅ 全部 | 同一个共享关系数据库 |

- **同构**：每个节点都跑数据面、都提供管理 API、都直接写共享库，也都能写。没有 `edge`，
  没有只读副本角色。
- **数据库是唯一权威**：tenants、user groups、users、api-keys、global roles、limit roles、
  providers/models/keys、证书内容都在共享库里。
- **单节点也离不开数据库**：单节点只是不需要 Redis；它同样使用 PostgreSQL/MySQL。
  SQLite 已删除（包括测试/开发用途）。
- **可用性边界是数据库**（以及共享数据面状态所需的 Redis），不是“多数派”。杀掉一个节点
  只减少容量，幸存节点继续写库、继续服务。

后端在编译期选择：`DB_BACKEND=postgres|mysql` → Cargo feature `db-postgres` / `db-mysql`，
必须恰好选一个。迁移是 `migrations/postgres/`、`migrations/mysql/` 的绿地基线，启动时执行。

---

## 2. 环境变量

运维完整表以 [`ops.md`](ops.md) §1.1 / §13 为准；集群相关的只有这些：

| 变量 | 必填 | 说明 |
|---|---|---|
| `HYDRA_DB_URL` | **是** | 共享库 URL；scheme 必须匹配编译后端（`postgres://…` 或 `mysql://…`） |
| `HYDRA_REDIS_URL` | 集群**是** | **集群开关**；同时承载共享限流/熔断/失效状态与 `hydra:config:changed` 扇出 |
| `HYDRA_NODE_ID` | 否 | 稳定节点身份；默认 hostname，其次随机。用于日志、扇出 origin、熔断投票 |
| `HYDRA_CONFIG_POLL_MS` | 否 | revision 探测间隔，默认 `5000`。未设、`0`、垃圾值都回退 5000 |
| `HYDRA_REDIS_MODE` | 否 | 设置时读取；只接受 `single` |
| `HYDRA_ADMIN_TOKEN` | **是** | 每个节点都有自己的管理 API，所以每个节点都要 |
| `HYDRA_ENCRYPTION_KEY` | **是** | 全集群一致；provider key 与证书私钥共用同一主密钥 |
| `HYDRA_USAGE_SINK` | **是** | `clickhouse` 或 `none`，无默认 |

### 2.1 已退役的变量

设了会被启动时 ERROR 点名（`RETIRED_CLUSTER_ENV` 是唯一所有者）：

`HYDRA_CLUSTER_PEERS`、`HYDRA_CLUSTER_ID`、`HYDRA_ARACHNE_LISTEN`、`HYDRA_ARACHNE_DATA_DIR`、
`HYDRA_ARACHNE_SNAPSHOT_THRESHOLD_MB`、`HYDRA_ROLE`、`HYDRA_EDGE`、`HYDRA_CLUSTER_TOKEN`、
`HYDRA_CONTROL_URL`、`HYDRA_PUBLIC_URL`、`HYDRA_CONTROL_POLL_MS`、`HYDRA_LEADER_LEASE_MS`、
`HYDRA_REGISTRY_STALE_GRACE_SECS`、`HYDRA_FAILOVER_GRACE_MS`、`HYDRA_FORWARD_TIMEOUT_SECS`。

它们不读、不生效；raft 端口、成员表、`HYDRA_CLUSTER_ID` 与每节点 raft/SQLite 数据目录都已经
不存在。清掉清单里的这些名字即可，不要试图通过设置它们改变集群行为。

---

## 3. 写入协议（共享库直接写）

每个写请求在**接收它的节点**上执行：

1. 进入数据库写事务，先 `SELECT id FROM <lock table> WHERE id = 1 FOR UPDATE`。
   共享锁行在启动/首次使用时幂等播种（PG `ON CONFLICT DO NOTHING` /
   MySQL `ON DUPLICATE KEY UPDATE id=id`），避免两个首写者互相死锁。
2. 在同一个事务里做数据变更；需要读-改-写的存在性检查也用 `FOR UPDATE`（MySQL 的
   REPEATABLE READ 下普通 `SELECT` 会读事务开始时的快照）。
3. **同一事务**里 `UPDATE config_revision SET version = version + 1 WHERE id = 1`。
   没有递增 revision 的写语句不合法；bulk 写每个事务只递增一次。
4. 提交。**提交前不返回 2xx**。

因此：任一节点都能写，写与写之间由共享锁行串行化；先提交者胜出，后提交者看到已提交的
状态。没有 leader 选举、没有转发层、没有“已本地提交但未发布”的中间态。

### 3.1 租户/用户写隔离

- 租户面（user groups / users / api-keys / billing）的 SQL **必须带租户范围**，例如
  `WHERE id = ? AND tenant_id = ?`；api-key 的 owner 用
  `user_id IN (SELECT id FROM <user table> WHERE tenant_id = ?)`。
- 0 行受影响（`RowNotFound`）→ **404**，与“不存在”和“属于别的租户”同一答案，避免枚举。
- 以另一个租户的 id 做 upsert 必须**拒绝**，绝不 `UPDATE tenant_id` 把行改归属。
- **管理面是全局的**：admin 可以读写任意租户的行（受 admin token 保护）。

### 3.2 MySQL 注意事项

- 列名避免了 MySQL 保留字：`provider.key` → `provider_code`、
  `provider_model.key` → `model_key`、`limit_role.window` → `limit_window`。
- `delete_tenant` 在写事务里使用**带租户范围的文本协议 DELETE**（`.persistent(false)`），
  避免 MySQL 1615 `Prepared statement needs to be re-prepared`。这是实现细节，运维无需动作；
  备份/ restore 与普通 SQL 不受影响。

---

## 4. 缓存与扇出协议

每个进程持有一份 `ArcSwap<ConfigData>`；热路径读无锁。配置变更通过两步传播：

1. **本地写路径**：事务提交后调用 reload。reload 在**一个读事务**里同时读取
   `config_revision.version` 与 `load_config_rows`（同一快照，避免 revision 与内容错配），
   构建并校验 `ConfigData`，然后在 reload 锁内比较版本，只有 revision 前进才替换快照。
2. **扇出**：只有本地写/管理 reload（`ReloadSource::Local`）且本地版本**确实前进**时，才在
   Redis `hydra:config:changed` 发布 `{"version": N, "origin": "<node-id>"}`。
3. **对端**：订阅者收到消息后走**安静 reload**（不复读、不转发、不再发布）；版本不前进则忽略。
   `ReloadSource::{PubSub,Poll}` 永不再发布，因此不会出现 O(N²) 广播风暴。
   - **去抖合并**：对端**不逐条消息 reload**。`version > 本地` 的合格消息进入 50 ms 尾随去抖窗口；
     连续消息会重置窗口，但从**第一条**消息起最多延迟 250 ms。到点先做一次
     `has_newer_revision()` 探测，再一次安静 reload（一次重建即覆盖整批消息）。
   - 去抖状态在 reload **开始前**清除：reload 进行期间到达的消息会在下一轮重新计时，不会被
     pending 标志吞掉；reload 失败时状态也已清除（不会卡住），由轮询兜底重试。
     `hydra_config_reload_coalesced_total` 统计被合并的消息数（比例信号，不配置告警）。
4. **轮询兜底**：每个集群节点每 `HYDRA_CONFIG_POLL_MS` 做一次廉价探测
   （只读 `config_revision.version`）。发现比本地新才做完整配对读 reload。这是丢消息或
   Redis 故障时的正确性兜底。

启动顺序是承重的：先完成初始加载，再注入 notifier 并启动订阅/轮询。启动期间到达的消息会被
版本闸门忽略，不会把快照回滚。

**外部 reload 绝不重新发布。** 否则一个节点的通知会在集群里反复回响。

### 4.1 `HYDRA_CONFIG_POLL_MS` 边界

- 未设、`0`、无法解析的值 → **5000 ms**（拼错不能关闭兜底）。
- 很小的值（如 1 ms）会让每个节点高频打数据库；轮询只做一次版本探测，但频率仍应保守。
- 很大的值等于**实质关闭**丢消息/Redis 故障的兜底；只在你能接受更长收敛窗口时这样做。

---

## 5. 故障模式

| 故障 | 行为 |
|---|---|
| **杀掉一个节点** | 容量减少；幸存节点继续写共享库、继续服务。该节点的缓存消失，重启后从库重建 |
| **数据库不可达** | 所有节点无法写配置；数据面继续用**已加载的快照**服务。没有 leader 可以“接管”，恢复数据库是唯一修复 |
| **Redis 不可达** | 数据面共享限流 fail-open、熔断投票不同步、失效传播暂停（按 TTL 过期）；**配置正确性不受影响**：扇出失效后由轮询兜底收敛。单个节点的本地写仍会提交 |
| **通知丢失** | 对端在下一个 poll 周期发现 revision 前进并收敛；不是数据丢失 |
| **快照 stale** | 本地 reload 失败会置 `hydra_config_snapshot_stale=1`；写已经提交，响应仍成功。修复导致 reload 失败的行后，`POST /api/v1/reload?force=1` 或下一次成功 reload 会恢复 |
| **`config_revision` 被回退**（误恢复/手工 SQL） | 节点的本地版本可能已经高于共享 revision，普通 reload 会因“未前进”跳过。在该节点执行 `POST /api/v1/reload?force=1`：先递增共享 revision（让对端也看到变化），再重读并**采纳** DB revision（丢弃本地更高的私有水位），恢复正常版本闸门。这是**逐节点**恢复，不是集群一次性动作：本地水位仍高于回退后 revision 的对端，要等共享 revision 越过它（或也在该节点执行 force）才会重新收敛 |
| **单节点** | 不需要 Redis；本地写后 reload 即可，没有订阅者/轮询 |

`/healthz/leader` **已删除**（404）。`GET /api/v1/cluster/status` 只报告本节点事实：
`node_id`、`role`、`config_version`、`db_reachable`、`redis_configured`、`redis_reachable`
（当前该字段为 `null`，健康探测不在这一层）。没有任何 peer/leader/term 字段。

---

## 6. 部署

### 6.1 docker compose（推荐起步）

```bash
cd environment
export HYDRA_ADMIN_TOKEN="$(openssl rand -hex 32)"          # 每个节点都用
export HYDRA_ENCRYPTION_KEY="$(openssl rand 32 | base64)"   # 全集群一致
./environment/build.sh                                      # DB_BACKEND=postgres（默认）
docker compose -f docker-compose.cluster.yml up -d
python3 environment/init.py                                 # 指向任一节点的管理口即可
```

拓扑：**3 个相同的 Hydra 节点 + 1 个 Postgres + Redis + ClickHouse**。三个节点的
`HYDRA_DB_URL` **逐字相同**；没有逐节点 `node-*-data` 配置卷，没有 raft 端口。
MySQL 变体：`DB_BACKEND=mysql ./environment/build.sh`，把每个节点的 `HYDRA_DB_URL` 改成
`mysql://…`，并使用 compose 的 `mysql` profile。

### 6.2 k3s / k8s

不再需要 StatefulSet 的固定身份、PVC 或 `podManagementPolicy: Parallel`；用普通
Deployment/StatefulSet 指向**托管数据库**即可（StatefulSet 仍可用，但只是为了固定 `HYDRA_NODE_ID`）。
每个 Pod 需要相同的数据库 URL、Redis URL、admin token、加密主密钥；探针用
`/api/v1/health` + admin token。**不要**配置 `/healthz/leader` 探针，它已是 404。

入口 Ingres/LB 指向所有 Pod（任意节点都能服务数据面与管理写）。没有 HPA 需要避开的成员表
约束；按数据库连接数和负载扩容即可。

### 6.3 裸机 / VM

同一二进制 + systemd；每台只需要能访问共享数据库和 Redis。没有 raft 传输端口，也没有必须
落在持久卷上的本地配置数据。证书内容存在共享库里（密文），不需要共享证书文件系统。

---

## 7. 可观测性与告警

| 指标 | 含义 |
|---|---|
| `hydra_config_reload_total{source="local\|pubsub\|poll", changed="true\|false"}` | reload 次数；`changed` 区分真实换快照与 no-op |
| `hydra_config_reload_duration_seconds` | 每次 reload 的墙钟耗时（含 no-op/失败；不含等待 reload 锁的时间）；用于衡量 15 万行全量 reload 的代价 |
| `hydra_config_rows_loaded` | 最近一次成功换入快照的实体数；no-op/失败不改变该值 |
| `hydra_config_notify_total{result="ok\|error"}` | 本地变更扇出成功/失败。失败只告警，不失败写；轮询会兜底 |
| `hydra_config_version` | 本节点最后应用的 revision |
| `hydra_config_snapshot_stale` | 1 = 上次 reload 失败，内存快照落后于数据库 |
| `hydra_config_bus_lagged_total` | 广播订阅者落后而跳过的消息数；随后立即探测 revision 并安静 reload，不等下一轮轮询 |
| `hydra_config_reload_coalesced_total` | 被合并进“已排队的 reload”的合格 pub/sub 消息数（突发写的比例信号，不是数据丢失：排队的 reload 会读取最新 revision；不配置告警） |

Watcher 存活时间指标 `hydra_config_watch_last_ok_unixtime` 记录最近一次成功订阅/轮询的
Unix 秒；配合 `hydra_config_version` 与 `snapshot_stale` 判断是否收敛。

建议告警：`snapshot_stale == 1`、`notify_total{result="error"}` 持续增长、
`config_version` 长时间不前进而数据库 revision 在前进（节点没有收敛）。
历史 raft/Arachne 指标已删除，见 [`ops.md`](ops.md) §9.2 的历史表。

---

## 8. 安全说明

- **明文永不落库/跨节点**：provider key 与证书私钥用 `HYDRA_ENCRYPTION_KEY`（AES-256-GCM）
  加密后存共享库；所有节点必须用同一主密钥。
- **数据库本身是高价值资产**：它持有租户、用户、api-key 摘要与密文凭据。限制网络访问、
  使用独立凭据、加密备份。
- **每个节点都有自己的管理口与 admin token**；管理口仍应只绑内网/回环。
- **Redis 建议 ACL + 内网隔离**；生产用托管多 AZ。Redis 只承载数据面近似状态与配置通知，
  不是配置权威。

---

## 9. 已知限制（如实记账）

- **数据库 HA 就是集群 HA**：共享库不可用时所有节点都不能写配置。没有 raft 的“少数派继续
  写”语义，因为本来就不需要共识。
- **每次写仍会在写入节点做一次完整 reload**（当前规模可接受）；`changed=false` 的 no-op 仍然很便宜。
  对端的 pub/sub 路径会把突发消息合并成一次 reload（50 ms 去抖、250 ms 上限），因此扇出侧的
  重建次数不再与消息条数一一对应。
- **轮询间隔有代价**：很小会打库，很大则兜底变慢（§4.1）。
- **租户写限流仍是每节点进程内窗口**：集群总量上界 = N × 单窗口（反 DoS，不是计费）。
- **没有 peer 存活视图**：`cluster/status` 只报告本节点；Redis 通知失败不代表节点宕机。
- **MySQL 使用整数/文本列的可移植子集**：布尔用 `SMALLINT`，过期时间保持 RFC3339 文本
  （按字符串比较）。见 `migrations/mysql/0001_init.sql`。

---

## 10. 退役对照（历史，便于查阅旧文档与旧工单）

| 旧机制 | 替代者 |
|---|---|
| raft 领导权 / 写探测 | 删除；任意节点直接写共享库 |
| 静态成员表 / `HYDRA_CLUSTER_ID` / raft 传输端口 | 删除；集群 = `HYDRA_REDIS_URL` |
| Arachne `head` / 内容寻址配置树 / 每节点物化 | 共享库是权威；内存 `ArcSwap` + Redis 通知/轮询 |
| `multi_put` 原子发布 / 发布失败 503 `config_not_published` | 数据库事务提交即成功；reload 失败只置 stale |
| `/healthz/leader` / leader 横幅 / `lease_holder` | 删除；本节点 `cluster/status` |
| 每节点 SQLite 物化状态 / `db::restore_config` | 删除；每节点从共享库加载 |
| `edge` 角色 / 无状态数据面 | 节点同构；任意节点都服务 |
