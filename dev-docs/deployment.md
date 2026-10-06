# Hydra 部署方案

> 覆盖三种形态：**单节点**（默认，零外部依赖）、**docker-compose 多节点**（三个同构 raft 成员）、
> **K3s / K8s 多节点**（同一镜像，零编排 API 依赖）。设计细节见 `dev-docs/cluster.md`，运维见 `dev-docs/ops.md`。

---

## 1. 构建

```bash
# 二进制（单节点即可用；集群模式也编译进同一二进制）
cargo build --release --features server,cluster-redis,usage-clickhouse
#   → target/release/hydra

# Docker 镜像（一键：cross-compile + 打入 bin/hydra + docker build）
./environment/build.sh
#   → hydra:latest（单节点与集群模式共用同一镜像；没设 HYDRA_CLUSTER_PEERS = 单节点，行为零变化）
#
# ⚠ 不要直接 `docker build`：`environment/Dockerfile` 是
#   `COPY bin/hydra /usr/local/bin/hydra` —— 它**原样打包** `bin/hydra`，
#   而这个文件由 `build.sh` 第 [2/3] 步从 `~/.cargo/global-target/…/release/hydra`
#   拷过来。谁在几周前跑过一次构建、今天直接 docker build，就会**把那个旧二进制
#   打进镜像**（本机实测：`bin/hydra` 还是 9 月 15 日的 32MB 产物，而源码早已前进）。
#   `build.sh` 每次都会重新 cross-compile 并覆盖它，所以走它就是安全的。
```

---

## 2. 单节点部署

### 2.1 二进制 + systemd

```bash
install -m0755 target/release/hydra /opt/hydra/hydra
mkdir -p /opt/hydra/data
```

```ini
# /etc/systemd/system/hydra.service
[Unit]
Description=Hydra LLM gateway
After=network-online.target

[Service]
ExecStart=/opt/hydra/hydra
WorkingDirectory=/opt/hydra
Restart=on-failure
User=hydra
Environment=HYDRA_ADMIN_TOKEN=<token>
Environment=HYDRA_ENCRYPTION_KEY=<base64-32B>   # 必填，fail-closed；丢失则 DB 不可读
Environment=HYDRA_DB_URL=sqlite:/opt/hydra/data/hydra.db?mode=rwc
Environment=HYDRA_LISTEN=0.0.0.0:8080           # 代理（明文，恒定）
# Environment=HYDRA_TLS_LISTEN=0.0.0.0:8443     # 可选：设置它才启用 HTTPS（按 SNI 选租户证书）
Environment=HYDRA_ADMIN_ADDR=127.0.0.1:8081     # 管理 REST + UI + /metrics

[Install]
WantedBy=multi-user.target
```

```bash
systemctl daemon-reload && systemctl enable --now hydra
curl -H "Authorization: Bearer <token>" http://127.0.0.1:8081/api/v1/health   # → 200
```

### 2.2 docker-compose（单节点全栈：hydra + mock-tenant + clickhouse）

```bash
cp environment/config.example.json secure/config.json   # 填入真实 provider api-key
export HYDRA_ENCRYPTION_KEY="$(openssl rand 32 | base64)"
export HYDRA_ADMIN_TOKEN="$(openssl rand -hex 32)"     # 必填（无默认值，>= 16 字符）
                                                       # 同时写进 secure/config.json 的 "admin_token"
cd environment && docker compose up -d
python3 environment/init.py                            # 播种 provider/tenant/模型
curl -H "Authorization: Bearer $HYDRA_ADMIN_TOKEN" http://localhost:8081/api/v1/tenants
```

端口：`8080` 代理（HTTP）、`8081` admin、`9091` mock-tenant、`8123` ClickHouse。

---

## 3. docker-compose 多节点（集群）

拓扑：`redis` + **三个完全同构的 Hydra 成员**（`hydra-a`/`b`/`c`，各自独立数据卷）+ `clickhouse`。
每个成员都跑数据面、管理面、本地 SQLite 与一个 raft 节点；**没有 edge，也没有可 `--scale` 的角色**
（ADR-0001 D-2）。

```bash
export HYDRA_ADMIN_TOKEN="$(openssl rand -hex 32)"          # 必填，>= 16 字符（每个节点都用）
export HYDRA_CLUSTER_TOKEN="$(openssl rand -hex 32)"        # 启动必填（见 cluster.md §2 的说明）
export HYDRA_ENCRYPTION_KEY="$(openssl rand 32 | base64)"    # 全集群必须一致
docker compose -f environment/docker-compose.cluster.yml up -d
```

验证：

```bash
# 互斥：恰好一台 writer 返回 200，其余 503（/healthz/leader 用 CLUSTER token）
for p in 8081 8082 8083; do echo -n "port $p: "; \
  curl -s -o /dev/null -w "%{http_code}\n" localhost:$p/healthz/leader; done

# 数据面（Host 头路由到 tenant）——三台的 8080 都在服务
curl -s -H "Host: <tenant-domain>" -H "Authorization: Bearer <key>" \
     -d '{"model":"gpt-4","messages":[{"role":"user","content":"hi"}]}' \
     http://localhost:8080/v1/chat/completions
```

故障切换演练（**实测 1.08 s / 1.63 s** 选出新 writer；数据面全程无感，见
`integration/test_data_plane_failover_load.py`）：

```bash
docker compose -f environment/docker-compose.cluster.yml kill hydra-a   # 上一步里 200 的那台
curl -s -H "Authorization: Bearer $HYDRA_CLUSTER_TOKEN" localhost:8082/healthz/leader   # → 200
docker compose -f environment/docker-compose.cluster.yml start hydra-a  # 以成员身份回归，自动跟上 head
```

> 集群必须项：`HYDRA_CLUSTER_PEERS`（三个节点的值逐字相同）、**每台各自的** `HYDRA_NODE_ID` 与
> `HYDRA_ARACHNE_LISTEN`（且后者必须与成员表里自己那一项一致）、`HYDRA_USAGE_SINK=clickhouse`
> （fail-closed）、`HYDRA_REDIS_URL`、`HYDRA_ADMIN_TOKEN`（每个节点都要）、
> `HYDRA_ENCRYPTION_KEY`（全集群一致）。**建议显式设 `HYDRA_CLUSTER_ID`**，否则将来无法改成员表
> （原因见 `cluster.md` §6.3）。

---

## 4. K3s / K8s 多节点部署

零 K8s API 依赖（不调用编排 API，纯容器清单）。先建 Secret，再 `kubectl apply`。

### 4.1 前置

```bash
kubectl create ns hydra
kubectl -n hydra create secret generic hydra-cluster \
  --from-literal=token=<cluster-token> \
  --from-literal=admin=<admin-token> \
  --from-literal=enc="$(openssl rand 32 | base64)"   # 全集群一致
```

### 4.2 Redis

```yaml
# redis.yaml
apiVersion: apps/v1
kind: Deployment
metadata: { name: redis, namespace: hydra }
spec:
  replicas: 1
  selector: { matchLabels: { app: redis } }
  template:
    metadata: { labels: { app: redis } }
    spec:
      containers:
        - { name: redis, image: redis:7, ports: [{ containerPort: 6379 }] }
---
apiVersion: v1
kind: Service
metadata: { name: redis, namespace: hydra }
spec: { selector: { app: redis }, ports: [{ port: 6379 }] }
```

### 4.3 三个同构成员（StatefulSet，固定身份 + 独立 PVC）

```yaml
# hydra.yaml
apiVersion: apps/v1
kind: StatefulSet
metadata: { name: hydra, namespace: hydra }
spec:
  serviceName: hydra
  replicas: 3
  # ↓↓↓ 首次安装（PVC 全新）必须这样：默认的 OrderedReady 会"等 pod-0 Ready 才起 pod-1"，而一个
  #     【从未被认领的数据目录】要等多数派（10 s 窗口，cluster.md §5.6），pod-0 永远等不到 ⇒ 起不来。
  #     已认领过的集群顺序启动也能起（实测单台重启 ~500 ms），但 Parallel 对两种情况都对。
  podManagementPolicy: Parallel
  selector: { matchLabels: { app: hydra } }
  template:
    metadata: { labels: { app: hydra } }
    spec:
      containers:
        - name: hydra
          image: hydra:latest          # 必须用 build.sh 构建（含 cluster-redis + arachne）
          ports:
            - { containerPort: 8080 }  # 数据面（明文，恒定绑定）
            - { containerPort: 8081 }  # 管理面 + /metrics + /healthz/leader
            - { containerPort: 8091 }  # raft 传输（只对内网）
          readinessProbe:
            # 每个节点都提供管理 API ⇒ 一条规则：带 admin token 的管理探针。
            # 不要用 /healthz/leader：那会把 Service 收敛到单一节点，而任何节点都能接受管理写。
            httpGet:
              path: /api/v1/health
              port: 8081
              httpHeaders: [{ name: Authorization, value: "Bearer $(HYDRA_ADMIN_TOKEN)" }]
          env:
            - { name: POD_NAME, valueFrom: { fieldRef: { fieldPath: metadata.name } } }
            # 成员表：三个节点【逐字相同】，顺序 = raft id
            - { name: HYDRA_CLUSTER_PEERS, value: "hydra-0.hydra:8091,hydra-1.hydra:8091,hydra-2.hydra:8091" }
            # 集群身份：显式给出，否则默认值 = 成员表哈希 ⇒ 改成员表就全集群拒绝启动
            - { name: HYDRA_CLUSTER_ID, value: dogress-prod }
            # 逐节点不同：来自 StatefulSet 的稳定 Pod 名
            - { name: HYDRA_NODE_ID, value: "$(POD_NAME)" }
            - { name: HYDRA_ARACHNE_LISTEN, value: "0.0.0.0:8091" }
            - { name: HYDRA_ADMIN_ADDR, value: 0.0.0.0:8081 }
            - { name: HYDRA_REDIS_URL, value: redis://redis:6379 }
            - { name: HYDRA_REDIS_MODE, value: single }
            - { name: HYDRA_USAGE_SINK, value: clickhouse }
            - { name: HYDRA_CLICKHOUSE_URL, value: http://clickhouse:8123 }
            - { name: HYDRA_CLUSTER_TOKEN, valueFrom: { secretKeyRef: { name: hydra-cluster, key: token } } }
            - { name: HYDRA_ADMIN_TOKEN, valueFrom: { secretKeyRef: { name: hydra-cluster, key: admin } } }
            - { name: HYDRA_ENCRYPTION_KEY, valueFrom: { secretKeyRef: { name: hydra-cluster, key: enc } } }
          volumeMounts: [{ name: data, mountPath: /app/data }]
  volumeClaimTemplates:
    - metadata: { name: data }
      spec:
        # 每个成员都要持久卷：raft WAL + SQLite。1Gi 只是起步，按配置总量规划（ADR-0001 R9）
        accessModes: [ReadWriteOnce]
        resources: { requests: { storage: 2Gi } }
---
apiVersion: v1
kind: Service
metadata: { name: hydra, namespace: hydra }
spec:
  clusterIP: None                     # headless：hydra-N.hydra 直连（raft 传输按名字互相访问）
  selector: { app: hydra }
  ports:
    - { name: raft, port: 8091, targetPort: 8091 }
---
apiVersion: v1
kind: Service
metadata: { name: hydra-data, namespace: hydra }
spec:
  selector: { app: hydra }
  ports: [{ name: http, port: 8080, targetPort: 8080 }]
```

> **`$(POD_NAME)`** 使每个成员以自己的稳定名字进成员表——StatefulSet 保证它跨重启不变，
> 而普通 Deployment 的 Pod 名每次重启都变（那会让节点挪出它被配置的表）。
> **三个成员都由同一个 StatefulSet 管理，配置完全相同**，唯一逐节点不同的就是
> `HYDRA_NODE_ID` / `HYDRA_ARACHNE_LISTEN`。

### 4.4 数据面就是这三个成员（**没有 edge，也没有 HPA**）

旧拓扑用一个无状态 `Deployment` + `HPA` 承接数据面流量。**那个角色已随 ADR-0001 D-2 退役**，原因
不是省事：一个不持有配置数据库的节点无法物化配置树，而"所有节点配置相同"正是集群行为可预测的前提。

因此：

- **入口指向同一组成员**：Ingress / LB 的 backend 用 `hydra-data:8080`（上面那个 Service），
  三个 Pod 都在服务，流量自然分摊；
- **不要给这个 StatefulSet 配 HPA**：成员数是 raft 的成员表决定的，**自动加减 Pod 只会造出
  "不在成员表里的进程"**（它会因为不在表里而拒绝启动）。加容量有两条路：纵向扩容（每台更大），
  或者一次**计划的成员变更**（`cluster.md` §6.3，必须先有显式 `HYDRA_CLUSTER_ID`）；
- **探针**：`/api/v1/health`（带 admin token）。旧文档的 `/readyz` 是 edge 角色的端点，
  集群成员一律走管理探针。

### 4.5 ClickHouse（用量 sink）

```yaml
# clickhouse.yaml（或部署独立 ClickHouse，仅需 HTTP :8123 + usage_record 表）
apiVersion: apps/v1
kind: Deployment
metadata: { name: clickhouse, namespace: hydra }
spec:
  replicas: 1
  selector: { matchLabels: { app: clickhouse } }
  template:
    metadata: { labels: { app: clickhouse } }
    spec:
      containers:
        - name: clickhouse
          image: clickhouse/clickhouse-server:24-alpine
          ports: [{ containerPort: 8123 }]
---
apiVersion: v1
kind: Service
metadata: { name: clickhouse, namespace: hydra }
spec: { selector: { app: clickhouse }, ports: [{ port: 8123 }] }
```

建表（首次）：

```bash
kubectl -n hydra exec deploy/clickhouse -- clickhouse-client --multiquery < environment/clickhouse/init.sql
```

### 4.6 入口（Ingress）

```yaml
# ingress.yaml
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: hydra
  namespace: hydra
  annotations:
    ingressClassName: traefik          # k3s 默认；k8s 换 nginx/其他
spec:
  rules:
    - host: llm.example.com             # 按域名路由到 tenant
      http:
        paths:
          - path: /
            pathType: Prefix
            backend: { service: { name: hydra-data, port: { number: 8080 } } }
```

### 4.7 部署与验证

```bash
kubectl apply -f redis.yaml -f hydra.yaml -f clickhouse.yaml -f ingress.yaml
kubectl -n hydra rollout status sts/hydra

# 恰一个 writer（在集群内执行；/healthz/leader 免 token）
for n in 0 1 2; do echo -n "hydra-$n: "; kubectl -n hydra exec hydra-$n -- sh -c \
  'wget -qO- http://127.0.0.1:8081/healthz/leader >/dev/null && echo 200 || echo 503'; done

# 故障切换：杀持写的那台，新 writer 实测 1–2 s 选出，数据面全程无感
kubectl -n hydra delete pod hydra-<writer> --grace-period=0 --force
```

---

## 5. 环境变量速查

| 变量 | 单节点 | 集群 | 说明 |
|---|---|---|---|
| `HYDRA_CLUSTER_PEERS` | 不设置 | **必填** | 进入集群的**唯一**开关 = 静态成员表，三个节点逐字相同，顺序即 raft id，≥3 项 |
| `HYDRA_NODE_ID` | 自动 | **每台必填且不同** | 必须出现在成员表里；**不要依赖 `HOSTNAME` 回退**（普通 Deployment 的名会变） |
| `HYDRA_ARACHNE_LISTEN` | — | **每台必填** | 本节点 raft 传输地址，**必须等于表里自己那一项**；端口与 `HYDRA_ADMIN_ADDR` 无关 |
| `HYDRA_CLUSTER_ID` | — | **强烈建议显式** | 集群身份；**默认 = 成员表哈希 ⇒ 改成员表就全集群拒绝启动**（`cluster.md` §6.3） |
| `HYDRA_REDIS_URL` | — | 必填 | 唯一外置依赖（**数据面**的共享状态） |
| `HYDRA_REDIS_MODE` | — | 只接受 `single` | 其它值（含拼错）快速失败；单节点默认下不读取 |
| `HYDRA_CLUSTER_TOKEN` | — | 必填（启动要求） | ⚠ 今天不守任何东西（`/api/v1/internal/*` 已无路由），删它是契约变化 |
| `HYDRA_ADMIN_TOKEN` | 必填 | **每台必填** | 每个节点都有自己的管理 API |
| `HYDRA_ENCRYPTION_KEY` | 必填 | 必填，全集群一致 | 主密钥（provider key/证书私钥） |
| `HYDRA_USAGE_SINK` | `sqlite` 默认 | **必须 `clickhouse`** | fail-closed（逐节点 sqlite 用量在集群里无意义） |
| `HYDRA_DB_URL` | `sqlite:hydra.db?mode=rwc` | 每台独立数据卷 | 本地 SQLite 是可重建的物化状态 |
| `HYDRA_ARACHNE_DATA_DIR` | — | 默认 `/app/data/arachne` | raft 数据目录，**必须在持久卷上** |
| `HYDRA_LISTEN` / `HYDRA_ADMIN_ADDR` | `0.0.0.0:8080` / `127.0.0.1:8081` | 同左（集群里管理口要 `0.0.0.0` 才能被探针访问） | 代理（明文，恒定）/ 管理端口 |
| `HYDRA_TLS_LISTEN` | *（未设置）* | 同左（如 `0.0.0.0:8443`） | 可选 HTTPS 端口；**设置它**才启用 TLS，与租户是否有证书无关 |

**已退役（设了会被启动 ERROR 点名）**：`HYDRA_ROLE`、`HYDRA_EDGE`、`HYDRA_CONTROL_URL`、
`HYDRA_PUBLIC_URL`、`HYDRA_CONTROL_POLL_MS`、`HYDRA_LEADER_LEASE_MS`、
`HYDRA_REGISTRY_STALE_GRACE_SECS`、`HYDRA_FAILOVER_GRACE_MS`、`HYDRA_FORWARD_TIMEOUT_SECS`
（完整清单与替代者见 `ops.md` §13.3b）。

健康端点：`/healthz/leader`（200 = 本节点是 writer；503 = 有闸门但没拿到领导权；404 = 根本没有
控制面，即单节点）、`/api/v1/health`（**需 admin token**，探针用这个）、`/metrics`（同一个 admin
token 门禁）。**`/healthz` 与 `/readyz` 随 edge 角色一起退役**——实测 2026-10-05：两者在管理口与
数据口都返回 **404**，所以探针只能是 `/api/v1/health`（带 admin token）；`/healthz/leader` 是唯一
免 token 的路由，但它答的是"谁是 writer"，不是"这个节点健康吗"。
