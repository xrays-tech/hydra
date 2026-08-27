# Hydra 部署方案

> 覆盖三种形态：**单节点**（默认，零外部依赖）、**docker-compose 多节点**（双 leader + 无状态 edge）、
> **K3s / K8s 多节点**（同一镜像，零编排 API 依赖）。设计细节见 `dev-docs/cluster.md`，运维见 `dev-docs/ops.md`。

---

## 1. 构建

```bash
# 二进制（单节点即可用；集群模式也编译进同一二进制）
cargo build --release --features server,cluster-redis,usage-clickhouse
#   → target/release/hydra

# Docker 镜像（一键：cross-compile + 打入 bin/hydra + docker build）
./environment/build.sh
#   → hydra:latest（单节点与集群模式共用同一镜像；HYDRA_ROLE 未设置 = 单节点，行为零变化）
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
Environment=HYDRA_LISTEN=0.0.0.0:8080           # 代理
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
cd environment && docker compose up -d
python3 environment/init.py                            # 播种 provider/tenant/模型
curl -H "Authorization: Bearer hydra-admin" http://localhost:8081/api/v1/tenants
```

端口：`8080` 代理（HTTP）、`8081` admin、`9091` mock-tenant、`8123` ClickHouse。

---

## 3. docker-compose 多节点（集群）

拓扑：`redis`（仲裁） + `hydra-control-a/b`（双 leader 候选，独立数据卷） + `hydra-edge`（无状态，可 scale）+ `clickhouse`。

```bash
export HYDRA_ADMIN_TOKEN=admin-secret
export HYDRA_ENCRYPTION_KEY="$(openssl rand 32 | base64)"    # 全集群必须一致
docker compose -f environment/docker-compose.cluster.yml up -d --scale hydra-edge=2
```

验证：

```bash
# 互斥：恰一个 leader 返回 200，另一个 503
for p in 8081 8082; do echo -n "port $p: "; curl -s -o /dev/null -w "%{http_code}\n" localhost:$p/healthz/leader; done
# 数据面（Host 头路由到 tenant）
curl -s -H "Host: localhost" -H "Authorization: Bearer <key>" \
     -d '{"model":"gpt-4","messages":[{"role":"user","content":"hi"}]}' \
     http://localhost:8080/v1/chat/completions
```

故障切换演练（≤~20s 提升，edge 自动跟随新 active）：

```bash
docker compose -f environment/docker-compose.cluster.yml stop hydra-control-a
curl -s localhost:8082/healthz/leader          # → 200（standby 提升）
docker compose -f environment/docker-compose.cluster.yml start hydra-control-a   # 以 standby 回归，自动重建副本
```

> 集群必须项：`HYDRA_USAGE_SINK=clickhouse`（fail-closed）、`HYDRA_CLUSTER_TOKEN`、
> `HYDRA_CONTROL_URL`、`HYDRA_ADMIN_TOKEN`（leader 必填）、`HYDRA_ENCRYPTION_KEY`（全集群一致）。

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

### 4.3 leader 候选 ×2（StatefulSet，稳定身份 + 独立 PVC）

```yaml
# hydra-control.yaml
apiVersion: apps/v1
kind: StatefulSet
metadata: { name: hydra-control, namespace: hydra }
spec:
  serviceName: hydra-control
  replicas: 2
  selector: { matchLabels: { app: hydra-control } }
  template:
    metadata: { labels: { app: hydra-control } }
    spec:
      containers:
        - name: hydra
          image: hydra:latest          # 必须用 build.sh 构建（含 cluster-redis feature）
          ports: [{ containerPort: 8080 }, { containerPort: 8081 }]
          readinessProbe:              # 仅 active 就绪 → Service 只路由到 active
            httpGet: { path: /healthz/leader, port: 8081 }
          env:
            - { name: HYDRA_ROLE, value: leader }
            - { name: HYDRA_ADMIN_ADDR, value: 0.0.0.0:8081 }   # 探针/Service 需从 Pod 外访问 admin
            - { name: HYDRA_REDIS_URL, value: redis://redis:6379 }
            - { name: HYDRA_REDIS_MODE, value: single }
            - { name: HYDRA_CONTROL_URL, value: http://hydra-control-0.hydra-control:8081 }
            - { name: HYDRA_PUBLIC_URL, value: "http://$(POD_NAME).hydra-control:8081" }
            - { name: HYDRA_USAGE_SINK, value: clickhouse }
            - { name: HYDRA_CLICKHOUSE_URL, value: http://clickhouse:8123 }
            - { name: HYDRA_CLUSTER_TOKEN, valueFrom: { secretKeyRef: { name: hydra-cluster, key: token } } }
            - { name: HYDRA_ADMIN_TOKEN, valueFrom: { secretKeyRef: { name: hydra-cluster, key: admin } } }
            - { name: HYDRA_ENCRYPTION_KEY, valueFrom: { secretKeyRef: { name: hydra-cluster, key: enc } } }
          volumeMounts: [{ name: data, mountPath: /app/data }]
  volumeClaimTemplates:
    - metadata: { name: data }
      spec: { accessModes: [ReadWriteOnce], resources: { requests: { storage: 1Gi } } }
---
apiVersion: v1
kind: Service
metadata: { name: hydra-control, namespace: hydra }
spec:
  clusterIP: None                     # headless：POD_NAME.hydra-control 直连
  selector: { app: hydra-control }
  ports: [{ port: 8081, targetPort: 8081 }]
```

> 说明：`$(POD_NAME)` 是 K8s 注入的环境变量，使每个 leader 以自身地址注册进注册表；
> readiness 探针 `503` 时从 Service 摘除 → 管理流量只到 active。

### 4.4 edge 数据面（无状态 Deployment + HPA）

```yaml
# hydra-edge.yaml
apiVersion: apps/v1
kind: Deployment
metadata: { name: hydra-edge, namespace: hydra }
spec:
  replicas: 3
  selector: { matchLabels: { app: hydra-edge } }
  template:
    metadata: { labels: { app: hydra-edge } }
    spec:
      containers:
        - name: hydra
          image: hydra:latest
          ports: [{ containerPort: 8080 }, { containerPort: 8081 }]
          readinessProbe: { httpGet: { path: /readyz, port: 8081 } }   # admin 端口
          env:
            - { name: HYDRA_ROLE, value: edge }
            - { name: HYDRA_ADMIN_ADDR, value: 0.0.0.0:8081 }
            - { name: HYDRA_REDIS_URL, value: redis://redis:6379 }
            - { name: HYDRA_CONTROL_URL, value: http://hydra-control-0.hydra-control:8081 }
            - { name: HYDRA_PUBLIC_URL, value: "http://$(POD_NAME):8081" }
            - { name: HYDRA_USAGE_SINK, value: clickhouse }
            - { name: HYDRA_CLICKHOUSE_URL, value: http://clickhouse:8123 }
            - { name: HYDRA_CLUSTER_TOKEN, valueFrom: { secretKeyRef: { name: hydra-cluster, key: token } } }
            - { name: HYDRA_ENCRYPTION_KEY, valueFrom: { secretKeyRef: { name: hydra-cluster, key: enc } } }
---
apiVersion: v1
kind: Service
metadata: { name: hydra-edge, namespace: hydra }
spec: { selector: { app: hydra-edge }, ports: [{ port: 8080, targetPort: 8080 }] }
---
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata: { name: hydra-edge, namespace: hydra }
spec:
  scaleTargetRef: { apiVersion: apps/v1, kind: Deployment, name: hydra-edge }
  minReplicas: 2
  maxReplicas: 20
  metrics: [{ type: Resource, resource: { name: cpu, target: { type: Utilization, averageUtilization: 70 } } }]
```

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
            backend: { service: { name: hydra-edge, port: { number: 8080 } } }
```

### 4.7 部署与验证

```bash
kubectl apply -f redis.yaml -f hydra-control.yaml -f hydra-edge.yaml -f clickhouse.yaml -f ingress.yaml
kubectl -n hydra rollout status sts/hydra-control deploy/hydra-edge

# 互斥 + 故障切换（在集群内执行）
kubectl -n hydra exec deploy/hydra-edge -- sh -c 'wget -qO- http://hydra-control-0.hydra-control:8081/healthz/leader'
kubectl -n hydra delete pod hydra-control-0     # 杀 active → hydra-control-1 ≤~20s 提升
kubectl -n hydra exec deploy/hydra-edge -- sh -c 'wget -qO- http://hydra-control-1.hydra-control:8081/healthz/leader'   # → 200
```

---

## 5. 环境变量速查

| 变量 | 单节点 | 集群 | 说明 |
|---|---|---|---|
| `HYDRA_ROLE` | 不设置 | `leader` / `edge` | 进入集群的开关 |
| `HYDRA_REDIS_URL` | — | 必填 | 唯一外置依赖（仲裁/共享状态） |
| `HYDRA_CLUSTER_TOKEN` | — | 必填 | 控制通道共享 token |
| `HYDRA_CONTROL_URL` | — | 必填 | active leader 管理端点 |
| `HYDRA_PUBLIC_URL` | — | leader 建议 | 本节点注册地址（注册表发现） |
| `HYDRA_ADMIN_TOKEN` | 必填 | leader 必填 | 集群共享 |
| `HYDRA_ENCRYPTION_KEY` | 必填 | 必填，全集群一致 | 主密钥（provider key/证书私钥） |
| `HYDRA_USAGE_SINK` | `sqlite` 默认 | **必须 `clickhouse`** | fail-closed |
| `HYDRA_DB_URL` | `sqlite:hydra.db?mode=rwc` | leader 用独立数据卷 | edge 无本地 DB |
| `HYDRA_LISTEN` / `HYDRA_ADMIN_ADDR` | `0.0.0.0:8080` / `127.0.0.1:8081` | 同左 | 代理 / 管理端口 |

健康端点：`/healthz/leader`（200 active / 503 standby / 404 非候选）、`/readyz`（edge）、
`/metrics`（Prometheus）、`/api/v1/health`（需 admin token）。
