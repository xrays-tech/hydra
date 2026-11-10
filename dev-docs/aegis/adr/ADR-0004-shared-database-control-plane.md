# ADR-0004 — 共享关系数据库作为控制面（无 raft / 无 leader）

- **Status**：`accepted`
- **Date**：`2026-11-10`
- **Supersedes**：[`ADR-0001-arachne-control-plane.md`](ADR-0001-arachne-control-plane.md)（Arachne / raft 控制面）
- **Deciders**：用户（架构方向与破坏性重写裁定）
- **Owner Surface**：`dev-docs/aegis/` 工作区；运行行为以 `dev-docs/cluster.md` / `ops.md` 为准
- **Boundary**：本 ADR 是 advisory 记录，不取代项目权威文档

---

## 1. 背景（Context）

ADR-0001 用 Arachne（raft 线性化 KV）取代了 Redis 租约控制面。它解决了租约时代的双 leader 与
切换窗口，但把系统带进另一组成本：

1. **控制面数据本来就在关系数据库里。** tenants、user groups、users、api-keys、global roles、
   limit roles、providers/models/keys、证书内容都由数据库承载。raft 树只是把这些行再复制成
   第二份权威，并需要一套编码/物化/保真/GC 机制。
2. **依赖与运维成本高。** 每个节点需要持久卷（raft WAL + 本地物化库）、raft 传输端口、静态
   成员表、集群身份、成员变更流程；升级/扩容/恢复都受多数派约束。一个上游共识库的缺陷会直接
   变成产品缺陷。
3. **“单写者”不是本系统的需求。** 写是低频管理操作；正确性只需要“写与写串行化、读看到已提交
   状态”。数据库事务 + 行锁已经提供这一点，不需要共识。
4. **关系数据库已经是运维资产。** PostgreSQL/MySQL 的 HA、备份、监控、访问控制是成熟能力；
   把可用性边界放在数据库上，比同时维护数据库和 raft 更简单。

ADR-0001 的实测记录（切换窗口、同一 node id 双 leader、转发链、Redis 不重连等）保留在该 ADR
与 git 历史中；它们解释了为什么租约时代结束，但不再为 raft 辩护。

---

## 2. 决策（Decision）

**共享关系数据库是唯一控制面权威。没有 raft、没有 leader、没有静态成员表。**

### 2.1 数据与后端

- PostgreSQL 与 MySQL 是仅有的两个后端，**编译期**选择：`DB_BACKEND=postgres|mysql` →
  `db-postgres` / `db-mysql`，恰好一个；无运行时驱动切换、无 `sqlx::Any`。
- SQLite **删除**（包括测试/开发）。迁移是绿地基线：`migrations/postgres/`、`migrations/mysql/`。
- 共享库保存 tenants、user groups、users、api-keys、global roles、limit roles、
  providers/models/keys，以及证书内容（密文）。

### 2.2 集群判定与节点模型

- **集群 = 配置了 `HYDRA_REDIS_URL`**。没有成员表、`HYDRA_CLUSTER_ID`、raft 端口或每节点配置卷。
- 每个节点同构：都跑数据面、都提供管理面、都**直接写共享库**。没有 `edge`、只读副本或 leader。
- `HYDRA_NODE_ID` 只用于日志、通知 origin 与熔断投票；不参与正确性。

### 2.3 写协议

- 每个写事务先取 `hydra_lock(id=1)` 行（`SELECT … FOR UPDATE`），做数据变更，并在**同一事务**里
  递增 `config_revision.version`，然后提交。提交后才返回 2xx。
- 锁行在启动/首次使用时幂等播种，避免两个首写者死锁。
- 租户面写入带租户范围并做行数检查（0 行 → 404，防枚举）；管理面是全局的。
- 没有 leader、转发层、发布/物化两阶段，也没有 `503 config_not_published`。

### 2.4 缓存与跨节点收敛

- 每个进程持有 `ArcSwap<ConfigData>`；reload 在**一个读事务**里同时读取 revision 与全部配置行。
- 本地写提交且 reload 后版本前进，才在 Redis `hydra:config:changed` 发布 `{version, origin}`。
- 对端收到通知后**安静 reload**，绝不重新发布（避免广播风暴）。
- `HYDRA_CONFIG_POLL_MS`（默认 5000）做 revision 探测，修复丢消息或 Redis 故障。
- 外部 reload 永不发布；单个节点也会在版本前进时发布自己的本地写。
- 单节点不启动订阅者/轮询，也不需要 Redis。

### 2.5 健康与状态

- `/healthz/leader` 删除（404）。
- `GET /api/v1/cluster/status` 只报告本节点：`node_id`、`role`、`config_version`、
  `db_reachable`、`redis_configured`、`redis_reachable`（当前为 `null`）。没有 peer/leader/term。

---

## 3. 后果（Consequences）

**正面**

- 控制面只有一份权威，不再需要树编码、物化、保真、GC、raft 成员变更或多数派恢复。
- 节点无状态：没有 PVC、没有 raft 端口、没有成员表；扩容就是加副本，节点 kill 只减少容量。
- 写路径更短：数据库事务提交即成功；reload 失败只影响本节点快照（`snapshot_stale`），不失败写。
- 数据库的 HA/备份/监控/访问控制可以直接复用。

**代价与约束**

- **数据库是可用性边界。** 数据库不可达时所有节点都不能写配置；没有“少数派继续写”。
- **必须有 PostgreSQL 或 MySQL**（单节点也是），且 URL scheme 必须匹配编译后端。
- **写串行化是全局的**：`hydra_lock` 是单行，所有写串行。当前管理写频率下可接受；高频批量写会
  排队。
- **每次写做一次完整 reload**（当前规模毫秒级）。`changed=false` 的 no-op 很便宜。
- **Redis 通知是 best-effort**：漏消息/Redis 故障靠轮询收敛，收敛窗口受 `HYDRA_CONFIG_POLL_MS`
  约束；很小会打库，很大则兜底变慢。
- **MySQL 使用可移植子集**：布尔 `SMALLINT`、`expires_at` 保持 RFC3339 文本（按字符串比较）；
  reserved-word 列名改为 `provider_code` / `model_key` / `limit_window`；`delete_tenant` 使用
  带范围的文本协议 delete 避免 MySQL 1615。
- **`config_revision` 被回退时需要强制 reload**（`POST /api/v1/reload?force=1`），否则本地版本
  更高的节点会因“未前进”跳过普通 reload。

---

## 4. 被否决的备选

| 备选 | 为什么否决 |
|---|---|
| **继续用 raft / Arachne** | 控制面数据已在关系数据库；raft 是第二份权威 + 每节点持久卷 + 成员变更 + 上游依赖。单写者不是本系统需求 |
| **Redis 作为唯一权威** | Redis 无事务性多表约束/外键/持久保证；配置与身份数据需要关系约束、备份与可查询性。Redis 仍只用于数据面近似状态与通知 |
| **SQLite（本地或 NFS）** | 单写者/文件锁不适合多节点共享；NFS 上的 SQLite 不是可靠共享存储。SQLite 已删除 |
| **每个节点一份配置副本 + 快照分发** | 这就是 ADR-0001 之前的复杂度；需要版本水位、保真、重建与失败恢复，仍不如直接读共享库 |
| **运行时驱动切换 / `sqlx::Any`** | 占位符、DDL、错误码差异大；编译期一个后端让“忘选后端”成为编译错误，兼容层可以针对一个方言做对 |

---

## 5. 迁移边界

- **绿地**：没有 SQLite→PostgreSQL/MySQL 迁移工具，也没有旧 raft 树的迁移。旧表/旧路径/旧
  env 直接删除；部署从空数据库 + 绿地基线迁移开始。
- **退役环境变量**（设了启动 ERROR 点名）：`HYDRA_CLUSTER_PEERS`、`HYDRA_CLUSTER_ID`、
  `HYDRA_ARACHNE_LISTEN`、`HYDRA_ARACHNE_DATA_DIR`、`HYDRA_ARACHNE_SNAPSHOT_THRESHOLD_MB`、
  `HYDRA_ROLE`、`HYDRA_EDGE`、`HYDRA_CLUSTER_TOKEN`、`HYDRA_CONTROL_URL`、`HYDRA_PUBLIC_URL`、
  `HYDRA_CONTROL_POLL_MS`、`HYDRA_LEADER_LEASE_MS`、`HYDRA_REGISTRY_STALE_GRACE_SECS`、
  `HYDRA_FAILOVER_GRACE_MS`、`HYDRA_FORWARD_TIMEOUT_SECS`。
- **运维迁移**：备份共享库；用匹配后端的新二进制对空库执行迁移；恢复配置数据；所有节点使用
  同一 `HYDRA_ENCRYPTION_KEY`、admin token 与数据库 URL。没有成员表/集群身份需要保留。
- **回滚**：恢复旧数据库备份与匹配的旧二进制；旧 raft 目录与成员表不再有意义。ADR-0001 的
  历史实现可从 git 历史取回，但不作为当前回滚路径。

---

## 6. 证据与权威指针

| 主题 | 位置 |
|---|---|
| 写/缓存协议、故障模式、节点 kill | [`cluster.md`](../../cluster.md) |
| 环境变量、指标/告警、备份/恢复、故障排查 | [`ops.md`](../../ops.md) §1 / §9 / §13 |
| 部署（compose / K8s） | [`deployment.md`](../../deployment.md) |
| 被取代的决策与历史实施记录 | [`ADR-0001`](ADR-0001-arachne-control-plane.md) |
