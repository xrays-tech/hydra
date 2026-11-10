# Hydra 三节点 K3s 集群部署 —— 配置清单与说明

> **定位**：面向「把 Hydra 以 3 个节点跑进 K3s」的配置向操作文档。集群行为与契约见
> [`dev-docs/cluster.md`](cluster.md)，完整 K8s 清单见 [`dev-docs/deployment.md`](deployment.md) §4，
> 运维见 [`dev-docs/ops.md`](ops.md)。本文聚焦**配置项清单 + 逐项说明**。
>
> **安全**：所有密钥类取值一律脱敏占位（`<…>`），实际值以配置中心与集群 Secret 为准，
> **不要把真实 token / 密钥提交进仓库**。

---

## 1. 部署形态速览

- **单镜像、无角色**：`hydra:postgres`（或 `hydra:mysql`），由 `environment/build.sh` 按
  `DB_BACKEND=postgres|mysql` 构建。集群里三个节点**完全一样**：都跑数据面、都提供管理面、
  都直接写**同一个共享数据库**。
- **集群开关只有 `HYDRA_REDIS_URL`**（数据面共享状态 + 配置扇出）。没有 raft、没有 leader、
  没有静态成员表、没有 `edge` 角色、没有 raft 传输端口。
- **“3 节点”只是容量/冗余选择，不是 quorum 下限**。两个节点也能跑；一个节点就是单节点。
  推荐最小拓扑：

  | Pod / 实例 | 状态 | 备注 |
  |---|---|---|
  | `hydra-0` | 无状态 | 无 PVC；配置在共享库 |
  | `hydra-1` | 无状态 | 同上 |
  | `hydra-2` | 无状态 | 同上 |
  | `postgres` / `mysql` | 共享数据库 | **配置权威**；建议托管或 HA 实例 |
  | `redis` | 集群内服务 | 共享限流/熔断/失效 + 配置通知（非权威） |
  | `clickhouse` | 集群内服务 | `HYDRA_USAGE_SINK=clickhouse` 时的用量 sink |

- **扩容/缩容 = 改副本数**，不受成员表约束。任意节点都能接受管理写；杀掉一个节点只减少容量，
  不中断写（幸存节点继续写库）。

---

## 2. 配置来源：两类，职责不同

| 来源 | 提供什么 | 特点 | 怎么改 |
|---|---|---|---|
| **① 配置中心（defing）**<br>`projects/dogress/branches/dev/config` | 全节点**共享**的基础配置（数据库/Redis/ClickHouse 地址、令牌、加密主密钥、日志…） | 所有节点同一份值；含 Secret | 在配置中心改 |
| **② 部署清单（节点级）**<br>Pod env / K8s Secret / 环境文件 | 建议逐 Pod 不同的 `HYDRA_NODE_ID`；数据库/Redis URL 若不用共享 Secret 则在此覆盖 | 每节点可单独给出 | 改 manifest 后滚动重启 |

> **合并规则**：配置中心的值作为共享基底注入每个节点。`HYDRA_DB_URL`、`HYDRA_REDIS_URL`、
> `HYDRA_ADMIN_TOKEN`、`HYDRA_ENCRYPTION_KEY` 三个节点必须一致；`HYDRA_NODE_ID` 建议不同
> （默认 hostname 也可）。**没有** `HYDRA_CLUSTER_PEERS` / `HYDRA_CLUSTER_ID` /
> `HYDRA_ARACHNE_LISTEN` 这类成员表变量了。

取配置（示例，令牌请换成自己的，勿提交）：

```bash
curl -s "https://defing.do.top/v1/projects/dogress/branches/dev/config?format=env" \
  -H "Authorization: Bearer <ACCESS_TOKEN>"
```

---

## 3. 配置中心获取的共享配置项（dev 分支）

| 配置项 | dev 取值（脱敏） | 说明 |
|---|---|---|
| `HYDRA_DB_URL` | `postgres://hydra:<pass>@postgres:5432/hydra` | **配置权威**。三个节点逐字相同；scheme 必须匹配镜像（postgres 镜像 ⇒ `postgres://`，mysql 镜像 ⇒ `mysql://`） |
| `HYDRA_REDIS_URL` | `redis://:<pass>@redis:6379/0` | **集群开关** + 数据面共享状态 + 配置扇出。缺失即不是集群节点 |
| `HYDRA_REDIS_MODE` | `single` | 只接受 `single`；其它值（含拼错）快速失败 |
| `HYDRA_CONFIG_POLL_MS` | `5000` | 配置 revision 探测兜底；未设/`0`/垃圾值回退 5000 |
| `HYDRA_ADMIN_TOKEN` | `<admin-token>` | 守护 `/api/v1/*`。**每个节点都要**（每个节点都提供管理 API），缺失拒启动 |
| `HYDRA_ENCRYPTION_KEY` | `<base64-32B>` | AES-256-GCM 主密钥：provider api-key 与证书私钥共用。**全集群一致**，缺失拒启动，丢失则库不可读 |
| `HYDRA_USAGE_SINK` | `clickhouse` | **必填、无默认**：`clickhouse` 或 `none`。旧 `sqlite` 已退役并按名字拒绝 |
| `HYDRA_CLICKHOUSE_URL` | `http://<user>:<pass>@clickhouse:8123/?database=dogress` | `HYDRA_USAGE_SINK=clickhouse` 时必填；支持 Basic auth 与 query 透传 |
| `HYDRA_ADMIN_ADDR` | `0.0.0.0:8081` | 管理监听地址。集群里必须 `0.0.0.0`（默认只绑回环，探针/Service 访问不到） |
| `RUST_LOG` | `info` | `tracing` 日志过滤级别 |

> **已从本表删除的项**（配置中心若还返回它们，请一并清掉）：`HYDRA_CLUSTER_PEERS`、
> `HYDRA_CLUSTER_ID`、`HYDRA_ARACHNE_LISTEN`、`HYDRA_ARACHNE_DATA_DIR`、
> `HYDRA_ARACHNE_SNAPSHOT_THRESHOLD_MB`，以及更早的 `HYDRA_ROLE` / `HYDRA_EDGE` /
> `HYDRA_CLUSTER_TOKEN` / `HYDRA_CONTROL_URL` / `HYDRA_PUBLIC_URL` / `HYDRA_CONTROL_POLL_MS` /
> `HYDRA_LEADER_LEASE_MS` / `HYDRA_REGISTRY_STALE_GRACE_SECS` / `HYDRA_FAILOVER_GRACE_MS` /
> `HYDRA_FORWARD_TIMEOUT_SECS`。设了会在启动时被 ERROR **逐个点名**（完整清单见
> `ops.md` §13.3b）。集群现在只需要 `HYDRA_ADMIN_TOKEN` 一个 token。

---

## 4. 集群部署补充配置项（不在配置中心 dev 返回中）

### 4.1 建议逐节点给出

| 配置项 | 示例 | 说明 |
|---|---|---|
| `HYDRA_NODE_ID` | `hydra-0` / `hydra-1` / `hydra-2` | 稳定节点身份（日志、配置扇出 origin、熔断投票）。默认 hostname，其次随机；普通 Deployment 的 Pod 名会变，但**不再影响正确性** |

### 4.2 用默认即可、按需显式化

| 配置项 | 默认 | 说明 |
|---|---|---|
| `HYDRA_LISTEN` | `0.0.0.0:8080` | 代理**明文**监听地址，**恒定绑定**。Ingress/Service 打到 8080 |
| `HYDRA_TLS_LISTEN` | *（未设置）* | 可选代理 **TLS** 监听地址（如 `0.0.0.0:8443`）。**只有设置它才会创建 HTTPS 监听**；证书内容随配置库到每个节点，无需共享卷 |
| `HYDRA_ENCRYPTION_KEY_FILE` | — | `HYDRA_ENCRYPTION_KEY` 的替代：从裸 32 字节文件读主密钥（K8s secret volumeMount）。二者任一即可，同时给优先 `_FILE` |
| `HYDRA_MAX_REQUEST_BODY_HARD` | 32 MiB | 单请求体硬上限 |
| `HYDRA_UPSTREAM_CONNECT_TIMEOUT_SECS` 等 | 见 `ops.md` | 上游超时/限流等通用旋钮，与集群无关 |

### 4.3 预留 / 代码内默认（一般不需要设置）

| 配置项 | 状态 | 说明 |
|---|---|---|
| `HYDRA_BREAKER_QUORUM` | 代码内默认 `1` | 熔断投票法定数（任一存活投票即生效），**确实被读取** |
| `HYDRA_RATE_LIMIT_FAIL_MODE` | **不存在该开关** | Redis 宕机时限流**硬编码 fail-open** |
| ~~`HYDRA_FAILOVER_GRACE_MS`~~ | **已退役** | 没有选举/切换可做；在 `RETIRED_CLUSTER_ENV` 里 |

---

## 5. 三节点变量矩阵（示例）

共享部分（配置中心 dev，三节点**完全相同**）：§3 全部项。
节点级部分（清单中逐 Pod 覆盖，示意值）：

| Pod | `HYDRA_NODE_ID` | 数据卷 | 端口 |
|---|---|---|---|
| `hydra-0` | `hydra-0` | 无（配置在共享库） | 8080 / 8081 |
| `hydra-1` | `hydra-1` | 无 | 8080 / 8081 |
| `hydra-2` | `hydra-2` | 无 | 8080 / 8081 |

> ⚠ **没有“成员变更”流程了**：集群 = 设置 `HYDRA_REDIS_URL` 的一组同构节点；改副本数即可。
> 数据库/Redis 的地址变更需要滚动更新所有节点。

### K3s 落地要点（详见 `deployment.md` §4）

- **镜像**必须用 `environment/build.sh` 构建（含一个 DB 后端 feature）。mysql 变体用
  `DB_BACKEND=mysql ./environment/build.sh`，并让所有节点指向 `mysql://…`。
- **工作负载形态**：普通 `Deployment`（`replicas: 3`）即可；不需要 StatefulSet、headless
  Service、PVC 或 `podManagementPolicy: Parallel`。StatefulSet 仍可用，但只是为了固定
  `HYDRA_NODE_ID`，不再是正确性要求。
- **探针**：`readinessProbe` 用带 admin token 的 `/api/v1/health`。**不要**用
  `/healthz/leader`（已删除，404），也不要把流量收敛到某一台。
- **Secret**：`kubectl -n hydra create secret generic hydra --from-literal=admin=<admin-token>
  --from-literal=enc="$(openssl rand 32 | base64)" --from-literal=db-url='postgres://…'`。
- **入口**：Ingress（k3s 默认 `traefik`）→ Service `:8080`。三个节点都在服务。
- **依赖服务名与地址**必须与配置中心 URL 一致：Redis 服务名 `redis`、
  `requirepass`/ACL 匹配 URL；ClickHouse 需按 `HYDRA_CLICKHOUSE_URL` 备好用户/库并执行过
  `environment/clickhouse/init.sql`。生产 Redis 建议 ACL + 内网隔离；数据库建议托管/多 AZ。

---

## 6. 启动 fail-closed 校验（提前对表）

| 条件 | 影响 |
|---|---|
| 任何节点缺 `HYDRA_ADMIN_TOKEN` | 拒绝启动 |
| 缺 `HYDRA_ENCRYPTION_KEY[_FILE]` | 拒绝启动（库不可读保护） |
| 未设 `HYDRA_USAGE_SINK` | 拒绝启动（无默认，ADR-0002 D-1） |
| 编译时没有 DB 后端 feature | **无法编译**；运行时 URL scheme 不匹配有明确错误 |
| 数据库不可达 | 启动失败（迁移需要数据库） |
| 设了任何一个已退役的变量 | **启动成功但有一条 ERROR 逐个点名它**（不是拒绝） |
| `HYDRA_REDIS_MODE` 为任何非 `single` 值且被设置 | 快速失败 |

---

## 7. 部署后验证

```bash
NS=hydra

# 1. 每个节点管理面健康（任意节点都能接受管理写）
for n in 0 1 2; do echo -n "hydra-$n: "; \
  kubectl -n $NS exec hydra-$n -- sh -c \
  'wget -qO- --header="Authorization: Bearer $HYDRA_ADMIN_TOKEN" http://127.0.0.1:8081/api/v1/health >/dev/null && echo 200 || echo fail'; done

# 2. 本节点视角的集群状态
kubectl -n $NS exec hydra-0 -- sh -c \
  'wget -qO- --header="Authorization: Bearer $HYDRA_ADMIN_TOKEN" http://127.0.0.1:8081/api/v1/cluster/status'

# 3. 管理面：指向任意一台都行（写就在接收它的节点上提交到共享库）
curl -H "Authorization: Bearer <admin-token>" http://hydra-0.hydra:8081/api/v1/tenants

# 4. 节点 kill 演练：删任意一台；幸存节点继续写库，缓存由 Redis 通知/轮询收敛
kubectl -n $NS delete pod hydra-<any> --grace-period=0 --force
```

更多行为、故障矩阵与限制见 [`dev-docs/cluster.md`](cluster.md)。

#### 从“全停”恢复

- 数据库在、Redis 在 ⇒ 按任意顺序启动即可；每个节点从共享库加载配置。
- Redis 暂不可用 ⇒ 配置仍可从数据库加载（单节点/启动后本地写均不依赖 Redis）；
  共享限流降级、配置扇出退化为轮询。先把 Redis 恢复，无需“凑多数派”。
- 数据库损坏/回退 ⇒ 按 `deployment.md` §6 恢复；若 `config_revision` 低于某节点本地版本，
  用 `POST /api/v1/reload?force=1` 强制重读。

---

## 8. 相关文档

- 集群**行为与契约**（节点模型 / 写协议 / 缓存扇出 / 故障模式 / 演练 / 限制）：
  [`dev-docs/cluster.md`](cluster.md)
- 决策与论证（为什么共享库、为何退役 ADR-0001、被否决的备选、代价）：
  [`dev-docs/aegis/adr/ADR-0004-shared-database-control-plane.md`](aegis/adr/ADR-0004-shared-database-control-plane.md)
- 部署方案（单节点 / compose / K3s / K8s 完整清单）：[`dev-docs/deployment.md`](deployment.md)
- 运维手册：[`dev-docs/ops.md`](ops.md)（§1.1 环境变量、§9.1 告警、§13 集群运维）
- compose 版集群全栈（对照参考）：[`environment/docker-compose.cluster.yml`](../environment/docker-compose.cluster.yml)
