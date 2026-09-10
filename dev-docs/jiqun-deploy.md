# Hydra 三节点 K3s 集群部署 —— 配置清单与说明

> **定位**：面向「把 Hydra 以 3 个节点跑进 K3s 集群」的配置向操作文档，覆盖两
> 类配置来源：① 配置中心（defing，`dev` 分支）拉取的共享配置；② 配置中心之外、
> 集群部署必须逐节点提供的配置项，并给出每项的说明。
>
> **不是重复造轮子**：集群设计见 [`dev-docs/cluster.md`](cluster.md)，完整
> K3s/K8s 清单与拓扑见 [`dev-docs/deployment.md`](deployment.md) §4，运维与
> 调优见 [`dev-docs/ops.md`](ops.md)。本文聚焦**配置项清单 + 逐项说明**，YAML
> 只给要点并回链原文；配置语义如有出入，以代码与 `cluster.md` §2 环境变量表为准。
>
> **安全**：本文所有密钥类取值一律脱敏占位（`<…>`），实际值以配置中心
> （defing `dev` 分支）与集群 Secret 为准，**不要把真实 token / 密钥提交进仓库**。

---

## 1. 部署形态速览

- **单镜像多角色**：`hydra:latest`（`environment/build.sh` 构建，含
  `cluster-redis` + `usage-clickhouse` feature）按 `HYDRA_ROLE` 区分角色——
  不设置 = 单节点（`all`），设置 `leader` / `edge` 即进入集群；K8s/k3s 无关，
  同一镜像同一行为。
- **“3 节点”指 Hydra 节点数**，不要求等于 K3s 机器数，推荐最小拓扑：

  | Pod / 实例 | 角色 | 状态 | 备注 |
  |---|---|---|---|
  | `hydra-control-0` | `leader`（候选） | 有状态 | 独立 PVC；通常先持有租约 = active |
  | `hydra-control-1` | `leader`（候选） | 有状态 | standby，active 宕机后 ≤ 租约+选举窗口提升 |
  | `hydra-edge-*` | `edge` | 无状态 | 数据面入口，可横向扩缩（示例 1 个起步） |
  | `redis`（集群内服务） | — | — | **唯一必选外置依赖**：租约/注册表/失效总线/共享限流/熔断/认证 L2 |
  | `clickhouse`（集群内服务） | — | — | `HYDRA_USAGE_SINK=clickhouse` 时的用量 sink |

- **进入集群的唯一开关**是 `HYDRA_ROLE`；Redis 之外**不调用任何编排 API**。

---

## 2. 配置来源：两类，职责不同

| 来源 | 提供什么 | 特点 | 怎么改 |
|---|---|---|---|
| **① 配置中心（defing）**<br>`projects/dogress/branches/dev/config` | 全节点**共享**的基础配置（数据库/Redis/ClickHouse 地址、令牌、加密主密钥、日志…） | 所有节点同一份值；含 Secret | 在配置中心改（注意密钥以实际值下发） |
| **② 部署清单（节点级）**<br>Pod env / K8s Secret / 环境文件 | **逐 Pod 不同**或**角色专属**的项：`HYDRA_ROLE`、`HYDRA_NODE_ID`、`HYDRA_CONTROL_URL`、`HYDRA_PUBLIC_URL`、数据卷路径等 | 每节点必须单独给出；**不能只依赖配置中心** | 改 manifest 后滚动重启 |

> **合并规则**：配置中心的值作为共享基底注入每个节点；凡逐节点不同的项必须在
> Pod 清单里以最终值覆盖（或根本不放进共享配置）。`HYDRA_ROLE` /
> `HYDRA_CONTROL_URL` / `HYDRA_PUBLIC_URL` / `HYDRA_NODE_ID` 在三个节点之间
> 必然不同，**不要**试图用共享配置表达它们。

取配置（示例，令牌请换成自己的，勿提交）：

```bash
curl -s "https://defing.do.top/v1/projects/dogress/branches/dev/config?format=env" \
  -H "Authorization: Bearer <ACCESS_TOKEN>"
```

> 配置中心按**分支**管理：`dev` 分支示例值即下文 §3；生产部署应取对应分支并
> 保持与发布节奏一致。密钥类项建议改走集群 Secret（见 §5），配置中心仅留非敏感
> 共享项。

---

## 3. 配置中心获取的共享配置项（dev 分支，共 11 项）

> 以下键名与取值形态与 curl 返回一致；密钥类已脱敏，实际值以配置中心为准。

| 配置项 | dev 取值（脱敏） | 说明 |
|---|---|---|
| `HYDRA_ADMIN_ADDR` | `0.0.0.0:8081` | 管理监听地址（admin REST + UI + `/metrics` + `/healthz` + `/readyz`）。**集群里必须 `0.0.0.0`**（默认 `127.0.0.1:8081` 只允许本机，探针/Service 无法访问）。 |
| `HYDRA_ADMIN_TOKEN` | `<admin-token>` | 守护 `/api/v1/*` 的 Bearer token。**leader 必填**（fail-closed，缺失拒启动）；集群内共享——standby 转发管理变更时沿用同一 token。edge 无 CRUD API，可不设。 |
| `HYDRA_CLICKHOUSE_URL` | `http://<user>:<pass>@clickhouse:8123/?database=dogress` | ClickHouse HTTP 端点，`HYDRA_USAGE_SINK=clickhouse` 时必填。支持 `http://user:pass@host:8123`（HTTP Basic）与 `?user=&password=`；其余 query（如 `?database=dogress`）原样透传。要求该 ClickHouse 已建好对应用户/库与 `usage_record` 表。 |
| `HYDRA_CLUSTER_TOKEN` | `<cluster-token>` | 控制通道共享 token：leader 校验、edge/standby 调用均用它（**不是** admin token）。集群模式必填（fail-closed）。 |
| `HYDRA_CONTROL_POLL_MS` | `500` | 控制面快照轮询间隔（代码默认 `1000`）。standby/edge 用它同步配置与证书；越小故障切换后收敛越快，越大越省 Redis/网络。 |
| `HYDRA_ENCRYPTION_KEY` | `<base64-32B>` | 32 字节的 base64（`openssl rand 32 \| base64`），AES-256-GCM 主密钥：provider api-key 与证书私钥落库/快照密封共用。**全集群必须一致**（任一节点不同则解密失败、fail-closed）。缺失即拒启动；丢失则库不可读。 |
| `HYDRA_LEADER_LEASE_MS` | `15000` | leader 租约时长（代码默认 `15000`，续约每 lease/3 ≈ 5s）。决定故障切换窗口上限（实测 ~11–18s）。 |
| `HYDRA_REDIS_MODE` | `single` | Redis 部署模式：`single`（默认，已接线）；`sentinel`/`cluster` 接线中，遇到即 fail-fast。 |
| `HYDRA_REDIS_URL` | `redis://:<pass>@redis:6379/0` | Redis 地址（集群**唯一必选外置依赖**，fail-closed）。按此 URL 必须可连：服务名 `redis`、端口 `6379`、`requirepass`/ACL 与 URL 密码一致。所有集群功能共用这一个 Redis。 |
| `HYDRA_USAGE_SINK` | `clickhouse` | 用量 sink：`sqlite`（单节点默认）或 `clickhouse`。**集群必须 `clickhouse`**（fail-closed，逐节点 sqlite 用量无意义）。单二进制同时编入两种 sink，切值无需重编。 |
| `RUST_LOG` | `info` | `tracing` 日志过滤级别（镜像默认已置 `info`，此项与镜像默认一致即可）。 |

> dev 分支返回中**没有**、但集群启动即校验（fail-closed）缺一不可的项：
> `HYDRA_ROLE`、`HYDRA_CONTROL_URL`——见下节。

---

## 4. 集群部署补充配置项（不在配置中心 dev 返回中）

### 4.1 必填 / 强烈建议（逐节点给出）

| 配置项 | 默认 | 适用 | 说明 |
|---|---|---|---|
| `HYDRA_ROLE` | `all`（单节点） | 每个集群节点 | 集群开关：`leader`（候选，竞争租约、本地 SQLite 随快照重建、写操作转发给 active）或 `edge`（无状态数据面，无本地库，仅 `/metrics` `/healthz` `/readyz`）。拼写错误会 WARN 后回落单节点，不会静默关代理——但会**退出集群**，务必核对。 |
| `HYDRA_CONTROL_URL` | — | leader、edge **必填** | active leader 的**控制面快照轮询**端点（如 `http://hydra-control-0.hydra-control:8081`）。注意它**不是**管理变更的转发目标——转发按实际租约持有者从注册表实时解析，故把候选指向自己（或都指向 `sts-0`）都安全。 |
| `HYDRA_PUBLIC_URL` | — | leader 建议必填 | 本节点注册进注册表的**可达管理端点**（K8s 里用 `http://$(POD_NAME).hydra-control:8081` / `http://$(POD_NAME):8081`）。leader 不设则注册为“不可轮询”，edge 无法经注册表发现它。 |
| `HYDRA_NODE_ID` | `node-<随机hex>` | 建议固定 | 节点标识（租约持有者 / 熔断投票者 / 注册表条目）。StatefulSet 场景建议固定为 `hydra-control-0` / `hydra-control-1` 便于排查。 |

### 4.2 用默认即可、按需显式化

| 配置项 | 默认 | 说明 |
|---|---|---|
| `HYDRA_DB_URL` | 镜像内 `sqlite:/app/data/hydra.db?mode=rwc`（代码默认 `sqlite:hydra.db?mode=rwc`） | 本地 SQLite：仅 `leader`/`all` 使用，`edge` 忽略（配置来自快照）。leader 数据卷须挂到 `/app/data`，保证 db 落盘 PVC。 |
| `HYDRA_LISTEN` | `0.0.0.0:8080` | 代理监听地址（有租户证书时自动走 TLS/443 语义）。集群内 Service/Ingress 打到 8080 即可，通常无需显式设。 |
| `HYDRA_ADMIN_ADDR` | `127.0.0.1:8081` | 见 §3——配置中心已给 `0.0.0.0:8081`，Pod 内必须保持 0.0.0.0 以便探针/Service 访问。 |
| `HYDRA_ENCRYPTION_KEY_FILE` | — | `HYDRA_ENCRYPTION_KEY` 的替代：从裸 32 字节文件读主密钥（K8s 用 secret volumeMount 场景）。二者任一即可，同时给优先 `_FILE`。 |

### 4.3 预留 / 代码内默认（一般不需要设置）

| 配置项 | 状态 | 说明 |
|---|---|---|
| `HYDRA_FAILOVER_GRACE_MS` | 预留，**未接线** | 文档里描述过宽限窗口；实测故障切换 = 租约过期 + 轮换 + 选举 tick，不依赖本项。 |
| `HYDRA_BREAKER_QUORUM` | 代码内默认 `1` | 熔断投票法定数（任一存活投票即生效），暂未提供 env 覆盖。 |
| `HYDRA_RATE_LIMIT_FAIL_MODE` | 代码内默认 `open` | Redis 宕机时限流 fail-open（可配 closed），暂未提供 env 覆盖。 |
| `HYDRA_EDGE_TLS` | 文档提及，代码未见接线 | `cluster.md` §4.2 注记“=1 使 edge 绑定 TLS 监听器（证书随快照分发）”；当前代码未见读取，生产如需 edge TLS 请先核实实现状态再依赖。 |

---

## 5. 三节点变量矩阵（示例）

共享部分（配置中心 dev，三节点**完全相同**）：§3 全部 11 项。
节点级部分（清单中逐 Pod 覆盖，示意值）：

| Pod | `HYDRA_ROLE` | `HYDRA_CONTROL_URL` | `HYDRA_PUBLIC_URL` | `HYDRA_NODE_ID` | `HYDRA_DB_URL` / 卷 |
|---|---|---|---|---|---|
| `hydra-control-0` | `leader` | `http://hydra-control-0.hydra-control:8081` | `http://hydra-control-0.hydra-control:8081` | `hydra-control-0` | 默认（`/app/data` ← PVC `data`） |
| `hydra-control-1` | `leader` | `http://hydra-control-0.hydra-control:8081` | `http://hydra-control-1.hydra-control:8081` | `hydra-control-1` | 默认（`/app/data` ← PVC `data`） |
| `hydra-edge-*` | `edge` | `http://hydra-control-0.hydra-control:8081` | `http://$(POD_NAME):8081` | 自动即可 | 不需要（无本地库） |

> 关键语义再强调一遍：`HYDRA_CONTROL_URL` 只服务**快照轮询**，三个节点都可以
> 指向 `sts-0`（部署文档即如此）；真正谁是写者由 **Redis 租约 + 注册表**决定，
> 管理变更在转发时刻按租约持有者解析——所以 standby 收到的写入也会正确落到
> active，故障切换后无需改配置。

### K3s 落地要点（详见 `deployment.md` §4）

- **镜像**必须用 `environment/build.sh` 构建（含 `cluster-redis` feature），否则
  `HYDRA_ROLE=leader` 启动即拒。
- **Secret**：`kubectl -n hydra create secret generic hydra-cluster --from-literal=token=… --from-literal=admin=… --from-literal=enc="$(openssl rand 32 | base64)"`——若共享配置已含 token/admin/enc（§3），可二选一作为单一来源，避免两处漂移；`HYDRA_ENCRYPTION_KEY` 必须全集群一致。
- **leader**：`StatefulSet` ×2 + **headless Service**（`clusterIP: None`）提供稳定 DNS `hydra-control-N.hydra-control`；`readinessProbe: /healthz/leader`（仅 active 返回 200 → Service 只路由到 active）；独立 `ReadWriteOnce` PVC（`volumeClaimTemplates`，挂 `/app/data`）。若 `HYDRA_PUBLIC_URL` 用了 `$(POD_NAME)`，须先在容器 env 用
  `valueFrom: { fieldRef: { fieldPath: metadata.name } }` 定义 `POD_NAME`（K8s 不会自动注入），否则会原样拼出不可解析的 host。
- **edge**：无状态 `Deployment`，`readinessProbe: /readyz`；Ingress（k3s 默认 `traefik`）→ `hydra-edge:8080`。
- **依赖服务名与地址**必须与配置中心 URL 一致：Redis 服务名 `redis`（同 namespace 解析），`requirepass`/ACL 匹配 `HYDRA_REDIS_URL` 里的密码；ClickHouse 需按 `HYDRA_CLICKHOUSE_URL` 备好用户/库（示例 `sh_admin` / `dogress`）并执行过 `environment/clickhouse/init.sql`（建 `usage_record` 表）。生产 Redis 建议 ACL + 内网隔离，ClickHouse 独立部署时仅需 HTTP `:8123` + 上述表。

---

## 6. 启动 fail-closed 校验（提前对表，缺一项节点拒绝启动）

| 条件 | 影响 |
|---|---|
| 集群模式缺 `HYDRA_REDIS_URL` | 拒绝启动 |
| 集群模式缺 `HYDRA_CLUSTER_TOKEN` | 拒绝启动 |
| `leader` 缺 `HYDRA_ADMIN_TOKEN` | 拒绝启动 |
| `leader` 但镜像未编入 `cluster-redis` feature | 拒绝启动（换 build.sh 镜像） |
| `leader` / `edge` 缺 `HYDRA_CONTROL_URL` | 拒绝启动 |
| 集群模式 `HYDRA_USAGE_SINK ≠ clickhouse` | 拒绝启动 |
| 任何角色缺 `HYDRA_ENCRYPTION_KEY[_FILE]` | 拒绝启动（库不可读保护） |
| `HYDRA_REDIS_MODE` 为 `sentinel`/`cluster` | fail-fast（接线中） |

---

## 7. 部署后验证

```bash
# 互斥：恰一个 leader 200，其余 503
kubectl -n hydra exec deploy/hydra-edge -- sh -c \
  'wget -qO- http://hydra-control-0.hydra-control:8081/healthz/leader; echo'
kubectl -n hydra exec deploy/hydra-edge -- sh -c \
  'wget -qO- http://hydra-control-1.hydra-control:8081/healthz/leader; echo'

# 故障切换演练：杀 active → standby ≤~20s 提升，edge 自动跟随
kubectl -n hydra delete pod hydra-control-0

# 管理面（指向任一 leader，standby 自动转发给 active）
curl -H "Authorization: Bearer <admin-token>" http://<hydra-control-svc>:8081/api/v1/tenants
```

更多演练与实测记录见 [`dev-docs/cluster.md`](cluster.md) §5。

---

## 8. 相关文档

- 集群设计与环境变量权威表：[`dev-docs/cluster.md`](cluster.md)（§2）
- 部署方案（单节点 / compose / K3s / K8s 完整清单）：[`dev-docs/deployment.md`](deployment.md)
- 运维手册：[`dev-docs/ops.md`](ops.md)（§1.1 环境变量、§4 集群运维）
- compose 版集群全栈（对照参考）：[`environment/docker-compose.cluster.yml`](../environment/docker-compose.cluster.yml)
