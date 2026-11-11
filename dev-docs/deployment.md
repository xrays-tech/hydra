# Hydra 部署方案

> 覆盖四种形态：**单节点**（一个 Hydra + 一个共享数据库）、**docker-compose 单节点全栈**、
> **docker-compose 多节点集群**（3 个同构节点共享一个数据库）、**K3s / K8s**。
> 架构行为见 [`cluster.md`](cluster.md)，运维见 [`ops.md`](ops.md)。
>
> 集群开关只有 `HYDRA_REDIS_URL`。没有 raft、没有 leader、没有成员表、没有逐节点配置卷。

---

## 0. 生产安全检查清单（最低要求）

- **数据库传输必须加密并校验服务端身份**：PostgreSQL 连接串带 `sslmode=verify-full`，
  MySQL 连接串带 `ssl-mode=VERIFY_IDENTITY`。`require`/`REQUIRED`/`prefer`/`PREFERRED`/
  `disable` 不够——它们要么不加密，要么不验证证书里的主机名，不能作为生产配置。
- **共享库属于信任边界本身**：它持有**所有租户**的身份与配置、全部 client api-key 摘要，
  以及用 `HYDRA_ENCRYPTION_KEY` 封装的 provider key / 证书私钥。DB 链路的机密性与身份校验
  不是“可选加固”，泄露或篡改它等同于泄露整个控制面。
- **compose 清单里的 DB 用户名/口令/URL 只用于本地开发与演练**，不是生产凭据。生产必须用
  独立的最小权限账号，并通过 secret 管理分发。
- **Redis 只在内网/回环可达**，`hydra:config:changed` 的 `PUBLISH` 必须用 Redis ACL 限制到
  Hydra 节点（见 `cluster.md` §8、`ops.md` §13.5a）。

---

## 1. 构建

Hydra 的数据库后端在**编译期**选择，恰好一个：

```bash
# PostgreSQL（默认）
./environment/build.sh
# → 镜像 hydra:postgres 与 hydra:latest

# MySQL
DB_BACKEND=mysql ./environment/build.sh
# → 镜像 hydra:mysql

# 只交叉编译、不构建 Docker
./environment/build.sh --no-build
```

`build.sh` 内部的 feature 组合是
`server,cluster-redis,usage-clickhouse,db-postgres`（或 `db-mysql`）。缺任一个后端 feature
时二进制**拒绝编译**；SQLite 已删除。

直接构建二进制时同样必须带一个后端 feature：

```bash
cargo build --release --features server,db-postgres
# 或 --features server,db-mysql
```

> ⚠ 不要直接 `docker build`：`environment/Dockerfile` 只 `COPY bin/hydra`，而 `bin/hydra`
> 由 `build.sh` 第 [2/3] 步从交叉编译产物拷入。走 `build.sh` 才不会把旧二进制打进镜像。

---

## 2. 单节点部署

单节点只需要一个 PostgreSQL/MySQL 数据库；不需要 Redis。所有配置（tenants、users、api-keys、
providers/models/keys、证书内容）都在这个共享库里。

### 2.1 二进制 + systemd

```bash
install -m0755 target/release/hydra /opt/hydra/hydra
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
Environment=HYDRA_ENCRYPTION_KEY=<base64-32B>   # 必填，fail-closed；丢失则库不可读
Environment=HYDRA_DB_URL=postgres://hydra:<pass>@127.0.0.1:5432/hydra
Environment=HYDRA_LISTEN=0.0.0.0:8080           # 代理（明文，恒定）
# Environment=HYDRA_TLS_LISTEN=0.0.0.0:8443     # 可选：设置它才启用 HTTPS（按 SNI 选租户证书）
Environment=HYDRA_ADMIN_ADDR=127.0.0.1:8081     # 管理 REST + UI + /metrics
Environment=HYDRA_USAGE_SINK=clickhouse
Environment=HYDRA_CLICKHOUSE_URL=http://127.0.0.1:8123

[Install]
WantedBy=multi-user.target
```

```bash
systemctl daemon-reload && systemctl enable --now hydra
curl -H "Authorization: Bearer <token>" http://127.0.0.1:8081/api/v1/health   # → 200
```

迁移在启动时执行。数据库凭据用独立账号，并限制网络访问；生产连接串必须按 §0 启用
`sslmode=verify-full` / `ssl-mode=VERIFY_IDENTITY`。备份/恢复见 §6。

### 2.2 docker-compose（单节点全栈）

```bash
export HYDRA_ENCRYPTION_KEY="$(openssl rand 32 | base64)"
export HYDRA_ADMIN_TOKEN="$(openssl rand -hex 32)"     # 必填（无默认值，>= 16 字符）
cd environment && docker compose up -d
python3 environment/init.py                            # 播种 provider/tenant/模型
curl -H "Authorization: Bearer $HYDRA_ADMIN_TOKEN" http://localhost:8081/api/v1/tenants
```

拓扑：`hydra` + `postgres` + `mock-tenant` + `clickhouse`。端口：`8080` 代理、`8081` admin、
`9091` mock-tenant、`8123` ClickHouse、`5432` Postgres。

MySQL 变体：用 `DB_BACKEND=mysql` 构建，把 `HYDRA_DB_URL` 改成 `mysql://…`，并加
`--profile mysql`（compose 自带 MySQL 8 服务）。

**本地开发 / 测试**：`environment/docker-compose.local.yml` 起真实 Postgres（以及 `--profile
mysql` 的 MySQL 8）+ Redis + ClickHouse，供 `cargo test`/集成演练使用；测试通过
`HYDRA_TEST_DATABASE_URL` 指向它，每个测试创建独立的 schema/database。

---

## 3. docker-compose 多节点（集群）

拓扑：`postgres` + `redis` + **三个完全同构的 Hydra 节点**（`hydra-a`/`b`/`c`）+ `clickhouse`。
三个节点的 `HYDRA_DB_URL` **逐字相同**；没有逐节点 `node-*-data` 配置卷，也没有 raft 端口。

```bash
export HYDRA_ADMIN_TOKEN="$(openssl rand -hex 32)"          # 每个节点都用
export HYDRA_ENCRYPTION_KEY="$(openssl rand 32 | base64)"    # 全集群必须一致
./environment/build.sh                                       # postgres（默认）
docker compose -f environment/docker-compose.cluster.yml up -d
```

验证：

```bash
# 每个节点都能读管理面；任意节点都能接受管理写（没有 leader）
for p in 8081 8082 8083; do echo -n "port $p: "; \
  curl -s -o /dev/null -w "%{http_code}\n" \
    -H "Authorization: Bearer $HYDRA_ADMIN_TOKEN" http://localhost:$p/api/v1/health; done

# 本节点视角的集群状态（node_id / role / config_version / db_reachable / redis_*）
curl -s -H "Authorization: Bearer $HYDRA_ADMIN_TOKEN" localhost:8081/api/v1/cluster/status

# 数据面（Host 头路由到 tenant）——三台的 8080 都在服务
curl -s -H "Host: <tenant-domain>" -H "Authorization: Bearer <key>" \
     -d '{"model":"gpt-4","messages":[{"role":"user","content":"hi"}]}' \
     http://localhost:8080/v1/chat/completions
```

节点 kill 演练：

```bash
docker compose -f environment/docker-compose.cluster.yml kill hydra-a
# 幸存节点继续写共享库；缓存由 Redis 通知或 5s 轮询收敛
curl -s -H "Authorization: Bearer $HYDRA_ADMIN_TOKEN" localhost:8082/api/v1/cluster/status
docker compose -f environment/docker-compose.cluster.yml start hydra-a
```

> 集群必须项：**相同的** `HYDRA_DB_URL`、`HYDRA_REDIS_URL`、`HYDRA_ADMIN_TOKEN`、
> `HYDRA_ENCRYPTION_KEY`、`HYDRA_USAGE_SINK`；建议每台不同的 `HYDRA_NODE_ID`。
> `HYDRA_CONFIG_POLL_MS` 默认 5000。已退役的 `HYDRA_CLUSTER_PEERS` / `HYDRA_CLUSTER_ID` /
> `HYDRA_ARACHNE_*` 若仍设置会被启动 ERROR 点名，请删除。

MySQL 变体：`DB_BACKEND=mysql ./environment/build.sh`，所有节点的 `HYDRA_DB_URL` 指向
`mysql://…`；集群 compose 只自带 Postgres，需要额外加一个 MySQL 服务（或指向托管 MySQL）。

---

## 4. K3s / K8s 多节点部署

零 K8s API 依赖（纯容器清单）。数据库建议使用**托管 PostgreSQL/MySQL**（或集群内 HA 实例）；
Hydra 节点本身是**无状态**的，不持有配置数据。

### 4.1 前置

```bash
kubectl create ns hydra
kubectl -n hydra create secret generic hydra \
  --from-literal=admin=<admin-token> \
  --from-literal=enc="$(openssl rand 32 | base64)" \
  --from-literal=db-url='postgres://hydra:<pass>@db.example:5432/hydra'
```

如果数据库密码不适合放清单，使用 External Secrets / sealed-secrets；Hydra 只读取环境变量。

### 4.2 Hydra（Deployment；StatefulSet 也可，但不再必需）

```yaml
apiVersion: apps/v1
kind: Deployment
metadata: { name: hydra, namespace: hydra }
spec:
  replicas: 3
  selector: { matchLabels: { app: hydra } }
  template:
    metadata: { labels: { app: hydra } }
    spec:
      containers:
        - name: hydra
          image: hydra:postgres            # mysql 变体用 hydra:mysql
          ports:
            - { containerPort: 8080 }      # 数据面
            - { containerPort: 8081 }      # 管理面 + /metrics
          readinessProbe:
            httpGet:
              path: /api/v1/health
              port: 8081
              httpHeaders: [{ name: Authorization, value: "Bearer $(HYDRA_ADMIN_TOKEN)" }]
          env:
            - { name: POD_NAME, valueFrom: { fieldRef: { fieldPath: metadata.name } } }
            - { name: HYDRA_NODE_ID, value: "$(POD_NAME)" }
            - { name: HYDRA_ADMIN_ADDR, value: 0.0.0.0:8081 }
            - { name: HYDRA_DB_URL, valueFrom: { secretKeyRef: { name: hydra, key: db-url } } }
            - { name: HYDRA_REDIS_URL, value: redis://redis:6379 }
            - { name: HYDRA_REDIS_MODE, value: single }
            - { name: HYDRA_CONFIG_POLL_MS, value: "5000" }
            - { name: HYDRA_USAGE_SINK, value: clickhouse }
            - { name: HYDRA_CLICKHOUSE_URL, value: http://clickhouse:8123 }
            - { name: HYDRA_ADMIN_TOKEN, valueFrom: { secretKeyRef: { name: hydra, key: admin } } }
            - { name: HYDRA_ENCRYPTION_KEY, valueFrom: { secretKeyRef: { name: hydra, key: enc } } }
---
apiVersion: v1
kind: Service
metadata: { name: hydra-data, namespace: hydra }
spec:
  selector: { app: hydra }
  ports: [{ name: http, port: 8080, targetPort: 8080 }]
```

要点：

- **没有 PVC**：配置在共享数据库里，证书内容也在库里（密文）。丢失 Pod 不丢配置。
- **没有 raft 端口**，也没有 headless Service 的成员解析需求。
- **探针只用 `/api/v1/health`（带 admin token）**。`/healthz/leader` 已删除（404），不要写它。
- **任意 Pod 都能接受管理写**；Ingress/LB 指向全部（或 `hydra-data` Service），无需把流量
  收敛到某个“writer”。
- 扩容/缩容就是改 `replicas`；受数据库连接数与 Redis 容量的约束，不受成员表约束。

### 4.3 Redis 与 ClickHouse

Redis：单个 Deployment/Service 即可（生产建议托管多 AZ）。它承载共享限流、熔断投票、失效总线
与配置通知；**不是配置权威**。

```yaml
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

ClickHouse（`HYDRA_USAGE_SINK=clickhouse` 时）：

```yaml
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

首次建表：

```bash
kubectl -n hydra exec deploy/clickhouse -- clickhouse-client --multiquery < environment/clickhouse/init.sql
```

### 4.4 入口（Ingress）

```yaml
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

### 4.5 部署与验证

```bash
kubectl apply -f hydra.yaml -f redis.yaml -f clickhouse.yaml -f ingress.yaml
kubectl -n hydra rollout status deploy/hydra

kubectl -n hydra get pods -l app=hydra
kubectl -n hydra exec deploy/hydra -- sh -c \
  'wget -qO- --header="Authorization: Bearer $HYDRA_ADMIN_TOKEN" http://127.0.0.1:8081/api/v1/cluster/status'
```

节点 kill 演练：删除任意一个 Pod，幸存 Pod 继续写库；缓存通过 Redis 通知或轮询收敛。
不需要等“新 leader”，也没有多数派。

---

## 5. 环境变量速查

| 变量 | 单节点 | 集群 | 说明 |
|---|---|---|---|
| `HYDRA_DB_URL` | **必填** | **必填，所有节点相同** | PostgreSQL/MySQL URL；scheme 必须匹配编译后端。生产必须带 `sslmode=verify-full`（PG）或 `ssl-mode=VERIFY_IDENTITY`（MySQL），见 §0 |
| `HYDRA_REDIS_URL` | 不设置 | **必填** | 集群开关；数据面共享状态 + 配置扇出 |
| `HYDRA_CONFIG_POLL_MS` | — | 默认 `5000` | revision 探测兜底；`0`/垃圾值回退 5000 |
| `HYDRA_NODE_ID` | 自动 | 建议每台不同 | 默认 hostname，其次随机 |
| `HYDRA_REDIS_MODE` | — | 只接受 `single` | 设置时校验 |
| `HYDRA_ADMIN_TOKEN` | 必填 | 每台必填 | 每个节点都有自己的管理 API |
| `HYDRA_ENCRYPTION_KEY` | 必填 | 必填，全集群一致 | provider key/证书私钥主密钥 |
| `HYDRA_USAGE_SINK` | **必填，无默认** | 同左 | `clickhouse` 或 `none` |
| `HYDRA_LISTEN` / `HYDRA_ADMIN_ADDR` | `0.0.0.0:8080` / `127.0.0.1:8081` | 管理口建议 `0.0.0.0:8081` | 代理（明文）/ 管理端口 |
| `HYDRA_TLS_LISTEN` | *（未设置）* | 同左 | 可选 HTTPS；设置它才启用 TLS |

**已退役（设了会被启动 ERROR 点名）**：`HYDRA_CLUSTER_PEERS`、`HYDRA_CLUSTER_ID`、
`HYDRA_ARACHNE_LISTEN`、`HYDRA_ARACHNE_DATA_DIR`、`HYDRA_ARACHNE_SNAPSHOT_THRESHOLD_MB`、
`HYDRA_ROLE`、`HYDRA_EDGE`、`HYDRA_CLUSTER_TOKEN`、`HYDRA_CONTROL_URL`、`HYDRA_PUBLIC_URL`、
`HYDRA_CONTROL_POLL_MS`、`HYDRA_LEADER_LEASE_MS`、`HYDRA_REGISTRY_STALE_GRACE_SECS`、
`HYDRA_FAILOVER_GRACE_MS`、`HYDRA_FORWARD_TIMEOUT_SECS`（完整清单见 `ops.md` §13.3b）。

健康端点：`/api/v1/health`（**需 admin token**，探针用这个）、`/metrics`（同一 token 门禁）。
`/healthz/leader` 已删除（404）。`/healthz` 与 `/readyz` 是已退役 edge 角色的端点，同样 404。

---

## 6. 数据库备份与恢复

数据库就是配置权威；备份它就是备份整个控制面（密文凭据也在里面，所以还需要
`HYDRA_ENCRYPTION_KEY`）。

```bash
# PostgreSQL：一致快照，无需停 Hydra
pg_dump --format=custom --file=hydra-$(date +%F).dump "$HYDRA_DB_URL"

# MySQL
mysqldump --single-transaction --routines --triggers hydra > hydra-$(date +%F).sql
```

恢复：

```bash
# 停掉所有 Hydra 节点 → 恢复数据库 → 用同一个 HYDRA_ENCRYPTION_KEY 启动
pg_restore --clean --if-exists --dbname "$HYDRA_DB_URL" hydra-2026-11-10.dump
# 或
mysql hydra < hydra-2026-11-10.sql
```

要点：

- **迁移在启动时执行**，恢复旧备份后不要用更新的二进制做不可逆的手工迁移；保留与备份匹配的
  二进制，或先在副本上验证。
- **主密钥必须与备份一起妥善保管，但不要和备份放在同一个桶**：没有它，密文凭据不可读，
  节点会拒绝服务。
- 恢复后若 `config_revision` 被回退到低于某个节点的本地版本，用
  `POST /api/v1/reload?force=1` 强制该节点重读收敛（见 `ops.md` §10.5）。
- ClickHouse 用量数据用 ClickHouse 自己的备份工具；它不在配置库内。
