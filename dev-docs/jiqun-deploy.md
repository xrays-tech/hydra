# Hydra 三节点 K3s 集群部署 —— 配置清单与说明

> **定位**：面向「把 Hydra 以 3 个节点跑进 K3s 集群」的配置向操作文档，覆盖两
> 类配置来源：① 配置中心（defing，`dev` 分支）拉取的共享配置；② 配置中心之外、
> 集群部署必须逐节点提供的配置项，并给出每项的说明。
>
> **不是重复造轮子**：集群**行为与契约**（节点模型 / 键空间 / 部署 / 演练 / 成员变更）
> 见 [`dev-docs/cluster.md`](cluster.md)，完整 K3s/K8s 清单与拓扑见
> [`dev-docs/deployment.md`](deployment.md) §4，运维与调优见 [`dev-docs/ops.md`](ops.md)。
> 本文聚焦**配置项清单 + 逐项说明**，YAML 只给要点并回链原文；配置语义如有出入，
> 以代码与 `cluster.md` §2 为准。
>
> **安全**：本文所有密钥类取值一律脱敏占位（`<…>`），实际值以配置中心
> （defing `dev` 分支）与集群 Secret 为准，**不要把真实 token / 密钥提交进仓库**。

---

## 1. 部署形态速览

- **单镜像、无角色**：`hydra:latest`（`environment/build.sh` 构建，含
  `server,cluster-redis,arachne,usage-clickhouse`）——**集群里三个节点完全一样**：
  都跑数据面、都跑管理面、都是 raft 成员、都有本地 SQLite。**进入集群的唯一开关是
  `HYDRA_CLUSTER_PEERS`**（静态成员表），没设就是单节点模式（零外部依赖）。
  **`edge` 角色与 `HYDRA_ROLE` 已退役**（ADR-0001 D-2）。
- **"3 节点"指 Hydra 节点数**，不要求等于 K3s 机器数；**3 是硬下限**（raft 需要多数派，
  两台互相都容不了故障，少于 3 个条目的成员表会被直接拒绝）。推荐最小拓扑：

  | Pod / 实例 | 状态 | 备注 |
  |---|---|---|
  | `hydra-0` | **有状态** | 独立 PVC（raft WAL + SQLite）；可能被选为 writer |
  | `hydra-1` | **有状态** | 同上 |
  | `hydra-2` | **有状态** | 同上 |
  | `redis`（集群内服务） | — | **唯一必选外置依赖**：共享限流 / 熔断 / 认证缓存 L2 / 失效总线 |
  | `clickhouse`（集群内服务） | — | `HYDRA_USAGE_SINK=clickhouse` 时的用量 sink |

- **没有"无状态副本"可扩**：扩容/缩容 = **raft 成员变更**（必须改所有成员上的成员表并重启；
  流程与一条**必须先读的陷阱**见 `cluster.md` §6.3）。Redis 之外**不调用任何编排 API**。

---

## 2. 配置来源：两类，职责不同

| 来源 | 提供什么 | 特点 | 怎么改 |
|---|---|---|---|
| **① 配置中心（defing）**<br>`projects/dogress/branches/dev/config` | 全节点**共享**的基础配置（数据库/Redis/ClickHouse 地址、令牌、加密主密钥、**成员表**、日志…） | 所有节点同一份值；含 Secret | 在配置中心改（注意密钥以实际值下发） |
| **② 部署清单（节点级）**<br>Pod env / K8s Secret / 环境文件 | **逐 Pod 不同**的项：`HYDRA_NODE_ID`、`HYDRA_ARACHNE_LISTEN`、数据卷路径 | 每节点必须单独给出；**不能只依赖配置中心** | 改 manifest 后滚动重启 |

> **合并规则**：配置中心的值作为共享基底注入每个节点；凡逐节点不同的项必须在
> Pod 清单里以最终值覆盖（或根本不放进共享配置）。`HYDRA_NODE_ID` 与
> `HYDRA_ARACHNE_LISTEN` 在三个节点之间**必然不同**，**不要**试图用共享配置表达它们。
> 而 `HYDRA_CLUSTER_PEERS` 三个节点**必须完全相同**（含顺序——顺序就是 raft id），
> 因此它**适合**放共享配置，但一旦改动就是一次集群身份变更，见 §5 的警告。

取配置（示例，令牌请换成自己的，勿提交）：

```bash
curl -s "https://defing.do.top/v1/projects/dogress/branches/dev/config?format=env" \
  -H "Authorization: Bearer <ACCESS_TOKEN>"
```

> 配置中心按**分支**管理：`dev` 分支示例值即下文 §3；生产部署应取对应分支并
> 保持与发布节奏一致。密钥类项建议改走集群 Secret（见 §5），配置中心仅留非敏感
> 共享项。

---

## 3. 配置中心获取的共享配置项（dev 分支）

> 以下键名与取值形态与 curl 返回一致；密钥类已脱敏，实际值以配置中心为准。

| 配置项 | dev 取值（脱敏） | 说明 |
|---|---|---|
| `HYDRA_CLUSTER_PEERS` | `hydra-0.hydra:8091,hydra-1.hydra:8091,hydra-2.hydra:8091` | **静态成员表，集群的定义**。`name=host:port` 逗号分隔，**三个节点的值必须逐字相同（含顺序）**：顺序就是每个成员的 raft id，改顺序 = 换一个集群。必须 ≥3 项，且**每个成员自己的 `HYDRA_ARACHNE_LISTEN` 必须与表里它那一项一致**，否则拒绝启动（`ListenMismatch`） |
| `HYDRA_CLUSTER_ID` | `dogress-dev` | **强烈建议显式设置**。集群身份。**默认值是成员表内容的哈希** ⇒ 改成员表就会改身份，而每个节点的数据目录记着旧身份 ⇒ **全部拒绝启动**（实测报错见 `cluster.md` §6.3）。因此做一次成员变更的前提就是：**从第一天起**把它设成一个稳定的人类可读名字 |
| `HYDRA_ADMIN_ADDR` | `0.0.0.0:8081` | 管理监听地址（admin REST + UI + `/metrics` + `/healthz` + `/readyz`）。**集群里必须 `0.0.0.0`**（默认 `127.0.0.1:8081` 只允许本机，探针/Service 无法访问） |
| `HYDRA_ADMIN_TOKEN` | `<admin-token>` | 守护 `/api/v1/*` 的 Bearer token。**每个节点都要**（fail-closed，缺失拒启动）——集群里每个节点都提供管理 API，不再有"只有 leader 需要 token"这回事 |
| `HYDRA_CLUSTER_TOKEN` | `<cluster-token>` | **启动要求**它存在且 ≥16 字符（fail-closed）。⚠ **今天它不守任何东西**：`/api/v1/internal/*` 前缀已无路由（快照通道与内部租户写端点都退役了）。保留是刻意的——删它是部署契约变化（ADR-0001 §7.1） |
| `HYDRA_ENCRYPTION_KEY` | `<base64-32B>` | 32 字节的 base64（`openssl rand 32 \| base64`），AES-256-GCM 主密钥：provider api-key 与证书私钥落库/进配置树时密封共用。**全集群必须一致**（任一节点不同则解密失败、fail-closed）。缺失即拒启动；丢失则库不可读 |
| `HYDRA_REDIS_URL` | `redis://:<pass>@redis:6379/0` | Redis 地址（**数据面唯一必选外置依赖**，fail-closed）。按此 URL 必须可连：服务名 `redis`、端口 `6379`、`requirepass`/ACL 与 URL 密码一致。共享限流/熔断/L2/失效总线共用这一个 Redis |
| `HYDRA_REDIS_MODE` | `single` | Redis 部署模式：只接受 `single`；**其它任何值（含拼错）快速失败**。**限定**：该开关**只在集群模式下被读取**——单节点默认下既不校验也不提及（`integration/test_startup_knobs.py` K12） |
| `HYDRA_USAGE_SINK` | `clickhouse` | 用量 sink：`sqlite`（单节点默认）或 `clickhouse`。**集群必须 `clickhouse`**（fail-closed：逐节点 sqlite 用量在集群里无意义）。单二进制同时编入两种 sink，切值无需重编 |
| `HYDRA_CLICKHOUSE_URL` | `http://<user>:<pass>@clickhouse:8123/?database=dogress` | ClickHouse HTTP 端点，`HYDRA_USAGE_SINK=clickhouse` 时必填。支持 `http://user:pass@host:8123`（HTTP Basic）与 `?user=&password=`；其余 query（如 `?database=dogress`）原样透传。要求该 ClickHouse 已建好对应用户/库与 `usage_record` 表 |
| `RUST_LOG` | `info` | `tracing` 日志过滤级别（镜像默认已置 `info`，此项与镜像默认一致即可） |

> **已从本表删除的项**（配置中心若还返回它们，请一并清掉）：`HYDRA_CONTROL_POLL_MS`、
> `HYDRA_LEADER_LEASE_MS`。它们与 `HYDRA_ROLE` / `HYDRA_CONTROL_URL` / `HYDRA_PUBLIC_URL`
> 等一起被 ADR-0001 退役，设了会在启动时被 ERROR **逐个点名**（完整清单见 `ops.md` §13.3b）。

---

## 4. 集群部署补充配置项（不在配置中心 dev 返回中）

### 4.1 必填（逐节点给出，三个节点各不相同）

| 配置项 | 示例 | 说明 |
|---|---|---|
| `HYDRA_NODE_ID` | `hydra-0` / `hydra-1` / `hydra-2` | 本节点在**成员表里的名字**，必须能在表里找到自己。**位置 = raft id**。K8s 里用 `$(POD_NAME)`（StatefulSet 固定名），**不要依赖 `HOSTNAME` 回退**——普通 Deployment 每次重启换名，会把节点挪出它被配置的表 |
| `HYDRA_ARACHNE_LISTEN` | `0.0.0.0:8091` | 本节点 raft 传输绑定的地址。**必须与成员表里自己那一项一致**（`ListenMismatch`）。端口与 `HYDRA_ADMIN_ADDR` **无关**——旧文档"两者必须相同"的约定随 leader 提示的转发用途一起作废 |

### 4.2 用默认即可、按需显式化

| 配置项 | 默认 | 说明 |
|---|---|---|
| `HYDRA_ARACHNE_DATA_DIR` | SQLite 路径所在目录下的 `arachne/`（即 `/app/data/arachne`） | raft 数据目录。**必须落在 PVC 上**：丢它等于丢该节点的 raft 日志与物化状态（节点能用 `head` 重建配置，但要以"新目录"重新认领，见 `cluster.md` §5.6） |
| `HYDRA_DB_URL` | 镜像内 `sqlite:/app/data/hydra.db?mode=rwc` | 本地 SQLite（可重建的物化状态）。数据卷须挂到 `/app/data` |
| `HYDRA_LISTEN` | `0.0.0.0:8080` | 代理**明文**监听地址，**恒定绑定**（即使配了租户证书也不会变成 TLS）。集群内 Service/Ingress 打到 8080 即可 |
| `HYDRA_TLS_LISTEN` | *（未设置）* | 可选的代理 **TLS** 监听地址（如 `0.0.0.0:8443`）。**只有设置它才会创建 HTTPS 监听**；证书随配置树分发到每个节点，无需共享卷。未设而租户有证书 ⇒ 证书不被使用（error 日志 + `hydra_listener_misconfig_total`）。必须与 `HYDRA_LISTEN` 不同端口 |
| `HYDRA_ENCRYPTION_KEY_FILE` | — | `HYDRA_ENCRYPTION_KEY` 的替代：从裸 32 字节文件读主密钥（K8s 用 secret volumeMount 场景）。二者任一即可，同时给优先 `_FILE` |

### 4.3 预留 / 代码内默认（一般不需要设置）

| 配置项 | 状态 | 说明 |
|---|---|---|
| `HYDRA_BREAKER_QUORUM` | 代码内默认 `1` | 熔断投票法定数（任一存活投票即生效），**确实被读取**（`crates/hydra-server/src/main.rs`） |
| `HYDRA_RATE_LIMIT_FAIL_MODE` | **不存在该开关** | Redis 宕机时限流**硬编码 fail-open，NOT configurable**（`grep -rn RATE_LIMIT_FAIL_MODE crates/` 为空） |
| ~~`HYDRA_FAILOVER_GRACE_MS`~~ | **从未接线，已退役** | 切换由 raft 选举决定，没有它可做的事（在 `RETIRED_CLUSTER_ENV` 里） |
| ~~`HYDRA_EDGE_TLS`~~ | **从未实现** | 只存在于旧文档里，代码从未读取过它。TLS 监听用上表的 `HYDRA_TLS_LISTEN` |

---

## 5. 三节点变量矩阵（示例）

共享部分（配置中心 dev，三节点**完全相同**）：§3 全部项。
节点级部分（清单中逐 Pod 覆盖，示意值）：

| Pod | `HYDRA_NODE_ID` | `HYDRA_ARACHNE_LISTEN` | 成员表里的位置（= raft id） | 卷 |
|---|---|---|---|---|
| `hydra-0` | `hydra-0` | `0.0.0.0:8091` | 1 | `/app/data` ← PVC `data`（**必需**） |
| `hydra-1` | `hydra-1` | `0.0.0.0:8091` | 2 | 同上 |
| `hydra-2` | `hydra-2` | `0.0.0.0:8091` | 3 | 同上 |

> ⚠ **做成员变更之前先读这条**：成员表的默认身份哈希会随表变化，所以**没有显式
> `HYDRA_CLUSTER_ID` 的集群改不了成员表**（每个节点都会以
> `META cluster_id mismatch: expected … got …` 拒绝启动，实测 2026-10-05）。
> 完整规避方式与变更步骤见 `cluster.md` §6.3。

### K3s 落地要点（详见 `deployment.md` §4）

- **镜像**必须用 `environment/build.sh` 构建（含 `arachne` feature）。**缺 `arachne` 时
  带成员表的节点会直接拒绝启动**（"this build has no control plane"）——这是实测抓到的
  一类缺陷，不要用更窄的 feature 配方。
- **工作负载形态**：三个节点用**同一个 StatefulSet**（`replicas: 3`）+ **headless Service**
  （`clusterIP: None`）提供稳定 DNS `hydra-N.hydra`。**首次安装时把
  `podManagementPolicy` 设成 `Parallel`**：默认的 `OrderedReady` 会等 pod-0 Ready 才起 pod-1，
  而一个**从未被认领的数据目录**要等多数派（10 s 窗口），于是 pod-0 永远等不到 ⇒ 集群起不来
  （实测 2026-10-05）。**已经认领过的集群**顺序启动也能起（单台重启实测 ~500 ms），
  但 `Parallel` 对两种情况都对，所以推荐一直开着。
- **探针**：`readinessProbe` 用**带 admin token 的管理探针**
  （`/api/v1/health` + `Authorization: Bearer`）。**不要**用旧文档的
  `readinessProbe: /healthz/leader`——它会把 Service 收敛到单一节点，而集群里**每个节点
  都能接受管理写**。
- **Secret**：`kubectl -n hydra create secret generic hydra-cluster --from-literal=admin=<admin-token> --from-literal=cluster=<cluster-token> --from-literal=enc="$(openssl rand 32 | base64)"`。三项都是**启动必填**（`HYDRA_CLUSTER_TOKEN` 虽然今天不守任何东西，仍然是启动要求，见 §3）。若共享配置已含它们，可二选一作为单一来源，避免两处漂移；`HYDRA_ENCRYPTION_KEY` 必须全集群一致。
- **`POD_NAME`**：若在 env 里用 `$(POD_NAME)`，须先用
  `valueFrom: { fieldRef: { fieldPath: metadata.name } }` 定义 `POD_NAME`（K8s 不会自动注入）。
- **入口**：Ingress（k3s 默认 `traefik`）→ StatefulSet 的 Service `:8080`。**不需要单独部署
  数据面 Deployment**：三个成员都在服务。
- **依赖服务名与地址**必须与配置中心 URL 一致：Redis 服务名 `redis`（同 namespace 解析），
  `requirepass`/ACL 匹配 `HYDRA_REDIS_URL` 里的密码；ClickHouse 需按 `HYDRA_CLICKHOUSE_URL`
  备好用户/库（示例 `sh_admin` / `dogress`）并执行过 `environment/clickhouse/init.sql`
  （建 `usage_record` 表）。生产 Redis 建议 ACL + 内网隔离。

---

## 6. 启动 fail-closed 校验（提前对表，缺一项节点拒绝启动）

| 条件 | 影响 |
|---|---|
| 集群模式缺 `HYDRA_REDIS_URL` | 拒绝启动 |
| 集群模式缺 `HYDRA_CLUSTER_TOKEN` | 拒绝启动（⚠ 尽管它今天不守任何东西，见 §3） |
| 任何节点缺 `HYDRA_ADMIN_TOKEN` | 拒绝启动 |
| 集群模式缺 `HYDRA_ENCRYPTION_KEY[_FILE]` | 拒绝启动（库不可读保护） |
| 集群模式 `HYDRA_USAGE_SINK ≠ clickhouse` | 拒绝启动 |
| 镜像未编入 `arachne` feature 而设了成员表 | 拒绝启动（换 `build.sh` 镜像） |
| 成员表 <3 项 | 拒绝启动（`TooFewMembers`） |
| `HYDRA_NODE_ID` 不在成员表里 | 拒绝启动（`SelfNotAMember`） |
| `HYDRA_ARACHNE_LISTEN` 与表里自己那一项不一致 | 拒绝启动（`ListenMismatch`） |
| 设了任何一个已退役的变量 | **启动成功但有一条 ERROR 逐个点名它**（不是拒绝） |
| `HYDRA_REDIS_MODE` 为任何非 `single` 值**且本节点是集群成员** | 快速失败；单节点默认下该值不被读取，因此不会拒绝启动 |
| 新数据目录未被多数派认领（10 s 内） | 拒绝启动（`no member adopted this node's Arachne data directory within 10s`） |

---

## 7. 部署后验证

```bash
NS=hydra
AUTH='-H "Authorization: Bearer <cluster-token>"'   # /healthz/leader 用 CLUSTER token

# 1. 恰一个 writer：三台各问一次，应当恰好一台 200、其余 503
for n in 0 1 2; do echo -n "hydra-$n: "; \
  kubectl -n $NS exec hydra-$n -- sh -c \
  'wget -qO- --header="Authorization: Bearer <cluster-token>" http://127.0.0.1:8081/healthz/leader >/dev/null && echo 200 || echo 503'; done

# 2. 每个节点的数据面在服务（任选一台，Host 指向真实租户）
kubectl -n $NS exec hydra-1 -- sh -c \
  'wget -qO- --header="Host: <tenant-domain>" http://127.0.0.1:8080/healthz'

# 3. 管理面：指向任意一台都行（写就在接收它的节点上应用并发布）
curl -H "Authorization: Bearer <admin-token>" http://hydra-0.hydra:8081/api/v1/tenants

# 4. 故障切换演练：杀掉持写的那台，等新 writer（实测 1.1–1.6 s），
#    数据面全程无感（20 rps × 60 s 实测 1200/1200 全 200）
kubectl -n $NS delete pod hydra-<writer> --grace-period=0 --force
```

更多演练、实测数字与成员变更 SOP 见 [`dev-docs/cluster.md`](cluster.md) §6。

---

## 8. 相关文档

- 集群**行为与契约**（节点模型 / 键空间 / 部署 / 演练 / 成员变更 / 已知限制）：
  [`dev-docs/cluster.md`](cluster.md)
- 决策与论证（为什么用 Arachne、九条决策、被否决的备选、代价）：
  [`dev-docs/aegis/adr/ADR-0001-arachne-control-plane.md`](aegis/adr/ADR-0001-arachne-control-plane.md)
- 部署方案（单节点 / compose / K3s / K8s 完整清单）：[`dev-docs/deployment.md`](deployment.md)
- 运维手册：[`dev-docs/ops.md`](ops.md)（§1.1 环境变量、§9.1 告警、§13 集群运维）
- compose 版集群全栈（对照参考）：[`environment/docker-compose.cluster.yml`](../environment/docker-compose.cluster.yml)
