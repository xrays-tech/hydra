# Hydra 集群模式（Cluster Mode）

> 集群模式是 **opt-in**：不设置任何集群环境变量时，Hydra 以单节点模式运行，
> 行为与之前完全一致（零外部依赖）。设置 `HYDRA_ROLE=leader|edge` 即进入集群：
> **Redis 是唯一必选外置依赖**（限流/熔断/认证缓存 L2/失效总线/租约/注册表共用
> 同一个 Redis），K8s/k3s 完全无关——Hydra 不调用任何编排 API，compose、k3s、
> k8s、裸机同一镜像同一行为。

---

## 1. 角色模型

| 角色（`HYDRA_ROLE`） | 职责 | 本地 SQLite | 管理 API | 说明 |
|---|---|---|---|---|
| 未设置 / `all` | 单节点（默认） | ✅ | ✅ 全部 | 现状零变化 |
| `leader` | leader 候选（租约竞争） | ✅（副本随快照重建） | ✅ 读本地 + **变更转发给 active**（P3） | 持有租约者 = active（唯一写者） |
| `edge` | 无状态数据面 | ❌ | ❌（仅 `/metrics` `/healthz` `/readyz`） | 配置随快照分发，可任意扩缩 |

**自维持**（集群在任何编排环境下自我管理）：
1. **自举**：节点只需 `HYDRA_REDIS_URL` + `HYDRA_CLUSTER_TOKEN` → 注册表发现 leader → 拉全量快照（含证书）→ 开始服务；
2. **自动选举**：leader 候选经 Redis 租约竞争，恰一个 active；
3. **自动故障切换**：active 死亡 → 租约过期 → 合格候选提升（实测 ≤ 租约 15s + 选举 tick 5s，约 11–18s），edge 数据面与控制面均无感（edge 轮询失败自动经注册表旋转到新 active；standby 也按**租约持有者**轮换 —— 即使它的静态 `HYDRA_CONTROL_URL` 指向自己，见 §5.1）；
4. **自动加入/退出**：edge 无状态任意增删；leader 候选可加可减；
5. **自愈**：租约时间栅栏杜绝双写、快照版本冲突由胜方覆盖、熔断投票 TTL 自清理、失效事件流跨故障切换不丢（幂等重放）。

---

## 2. 环境变量

| 变量 | 默认 | 说明 |
|---|---|---|
| `HYDRA_ROLE` | `all` | `leader` / `edge` 进入集群 |
| `HYDRA_REDIS_URL` | — | **集群必填**（fail-closed）；如 `redis://redis:6379` |
| `HYDRA_REDIS_MODE` | `single` | `single`（默认）/ `sentinel` / `cluster`（后两者接线中，fail-fast） |
| `HYDRA_CLUSTER_TOKEN` | — | **集群必填**：控制通道共享 token（leader 服务、edge/standby 调用） |
| `HYDRA_CONTROL_URL` | — | leader/edge 必填：active leader 的管理端点（如 `http://hydra-control:8081`） |
| `HYDRA_PUBLIC_URL` | — | 本节点注册到注册表的可达管理端点（如 `http://hydra-control-a:8081`）；leader 建议必填 |
| `HYDRA_CONTROL_POLL_MS` | `1000` | 控制快照轮询间隔（standby 副本同步可收紧至 200） |
| `HYDRA_LEADER_LEASE_MS` | `15000` | 租约时长（续约每 lease/3） |
| `HYDRA_FAILOVER_GRACE_MS` | `5000` | **预留，尚未接线**（见 §5.1 实测：故障切换 ≈ 租约过期 + 轮换 + 选举 tick） |
| `HYDRA_NODE_ID` | 自动生成 | 节点标识（租约持有者/熔断投票者/注册表条目） |
| `HYDRA_USAGE_SINK` | `sqlite` | **集群必须 `clickhouse`**（fail-closed） |
| `HYDRA_CLICKHOUSE_URL` | — | sink=clickhouse 时必填 |
| `HYDRA_ADMIN_TOKEN` | — | **leader 必须**：全集群共享（standby 转发管理变更时沿用） |
| `HYDRA_ENCRYPTION_KEY` | — | **全集群一致**（provider key 与证书私钥共用同一主密钥） |

> **集群启动 fail-closed 检查**：leader/edge 缺 `HYDRA_REDIS_URL`、`HYDRA_CLUSTER_TOKEN`、
> `HYDRA_CONTROL_URL`，或 `HYDRA_USAGE_SINK≠clickhouse`，或 leader 缺
> `HYDRA_ADMIN_TOKEN` / `cluster-redis` feature → 拒绝启动。

---

## 3. 共享状态（一个 Redis，七个用途）

| 子系统 | Key | 说明 |
|---|---|---|
| leader 租约 | `hydra:{lease:leader}` | `SET NX PX` + Lua 原子续约（只续自己的） |
| 节点注册表 | `hydra:{nodes}` + `hydra:{node:hb}:<id>` | 注册/心跳（TTL 30s）/leader 发现 |
| 失效总线 | `hydra:{ctl:events}` + `hydra:{ctl:gen}` | Streams 持久可重放 + generation 兜底 |
| 共享限流 | `hydra:{rl:role:bucket}:count|tokens` | Lua 滑动窗口（同 `{rl:...}` tag 同槽） |
| 共享熔断 | `hydra:{br}:dead:{p}` + `hydra:{br}:alldead` | 投票 + 心跳 TTL + 本地 1s 同步 |
| 认证缓存 L2 | `hydra:{auth}:{tenant}:{keyhash}` + 索引 | L1 miss 才访问；租户索引免 SCAN |
| — | 命名空间规则 | 多 key 操作必须同 hash tag；禁 SCAN/MATCH（Cluster 安全） |

**Redis 故障行为**（数据面永不受影响——edge 持 last-known-good 快照与本地缓存）：

| 子系统 | Redis 宕机行为 |
|---|---|
| 配置快照 | 暂停更新（快照走 leader HTTP） |
| 限流 | fail-open（`HYDRA_RATE_LIMIT_FAIL_MODE` 可配 closed）+ 告警指标 |
| 熔断 | 退回本地 trip（投票不同步，本地死集仍生效） |
| 认证 L2 | 退回纯 L1（失效传播暂停，条目按 TTL 过期） |
| 选举 | 续约失败 → **立即降级停写**（fail-closed）；无切换直至 Redis 恢复 |

---

## 4. 部署

### 4.1 docker-compose（推荐起步）

```bash
cd environment
HYDRA_ADMIN_TOKEN=admin-secret HYDRA_ENCRYPTION_KEY="$(openssl rand 32 | base64)" \
  docker compose -f docker-compose.cluster.yml up -d --scale hydra-edge=2
# 管理面：指向任一 leader 候选（standby 自动转发到 active）
curl -H "Authorization: Bearer admin-secret" http://localhost:8081/api/v1/tenants
```

### 4.2 k3s / k8s（纯容器清单，零 K8s API 依赖）

```yaml
# redis（或托管 Redis）
apiVersion: apps/v1
kind: Deployment
metadata: { name: redis }
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
metadata: { name: redis }
spec: { selector: { app: redis }, ports: [{ port: 6379 }] }
---
# leader 候选 ×2（独立 PVC；StatefulSet 保证稳定身份）
apiVersion: apps/v1
kind: StatefulSet
metadata: { name: hydra-control }
spec:
  serviceName: hydra-control
  replicas: 2
  selector: { matchLabels: { app: hydra-control } }
  template:
    metadata: { labels: { app: hydra-control } }
    spec:
      containers:
        - name: hydra
          image: hydra:latest
          args: ["--features=server,cluster-redis"]   # 构建时启用
          env:
            - { name: HYDRA_ROLE, value: leader }
            - { name: HYDRA_REDIS_URL, value: redis://redis:6379 }
            - { name: HYDRA_CLUSTER_TOKEN, valueFrom: { secretKeyRef: { name: hydra-cluster, key: token } } }
            - { name: HYDRA_CONTROL_URL, value: http://hydra-control-0.hydra-control:8081 }
            - { name: HYDRA_PUBLIC_URL, value: "http://$(POD_NAME).hydra-control:8081" }
            - { name: HYDRA_USAGE_SINK, value: clickhouse }
            - { name: HYDRA_CLICKHOUSE_URL, value: http://clickhouse:8123 }
            - { name: HYDRA_ADMIN_TOKEN, valueFrom: { secretKeyRef: { name: hydra-cluster, key: admin } } }
            - { name: HYDRA_ENCRYPTION_KEY, valueFrom: { secretKeyRef: { name: hydra-cluster, key: enc } } }
          ports: [{ containerPort: 8080 }, { containerPort: 8081 }]
          readinessProbe:
            httpGet: { path: /healthz/leader, port: 8081 }   # 仅 active 就绪 → Service 路由到 active
  volumeClaimTemplates:
    - metadata: { name: data }
      spec: { accessModes: [ReadWriteOnce], resources: { requests: { storage: 1Gi } } }
---
apiVersion: v1
kind: Service
metadata: { name: hydra-control }
spec:
  selector: { app: hydra-control }
  ports: [{ port: 8081, targetPort: 8081 }]
---
# edge ×N（无状态，HPA 扩缩）
apiVersion: apps/v1
kind: Deployment
metadata: { name: hydra-edge }
spec:
  replicas: 3
  selector: { matchLabels: { app: hydra-edge } }
  template:
    metadata: { labels: { app: hydra-edge } }
    spec:
      containers:
        - name: hydra
          image: hydra:latest
          env:
            - { name: HYDRA_ROLE, value: edge }
            - { name: HYDRA_REDIS_URL, value: redis://redis:6379 }
            - { name: HYDRA_CLUSTER_TOKEN, valueFrom: { secretKeyRef: { name: hydra-cluster, key: token } } }
            - { name: HYDRA_CONTROL_URL, value: http://hydra-control-0.hydra-control:8081 }
            - { name: HYDRA_PUBLIC_URL, value: "http://$(POD_NAME):8081" }
            - { name: HYDRA_USAGE_SINK, value: clickhouse }
            - { name: HYDRA_CLICKHOUSE_URL, value: http://clickhouse:8123 }
            - { name: HYDRA_ENCRYPTION_KEY, valueFrom: { secretKeyRef: { name: hydra-cluster, key: enc } } }
          ports: [{ containerPort: 8080 }, { containerPort: 8081 }]
          readinessProbe:
            httpGet: { path: /readyz, port: 8081 }
---
# 代理入口（Ingress / LB 指向 edge 的 8080）
```

> **edge TLS**：`HYDRA_EDGE_TLS=1` 使 edge 绑定 TLS 监听器（证书随快照分发，
> 无需共享卷）。默认 plain TCP（本地开发）。

### 4.3 裸机 / VM

同一二进制 + systemd；`HYDRA_CONTROL_URL`/`HYDRA_PUBLIC_URL` 用主机名或 VIP；
leader 用 `HYDRA_NODE_ID` 固定标识。

---

## 5. 故障切换演练

```bash
# 1. 观察当前 active（两个 leader 的 /healthz/leader：一个 200 一个 503）
for p in 8081 8082; do echo -n "port $p: "; curl -s -o /dev/null -w "%{http_code}\n" localhost:$p/healthz/leader; done

# 2. 杀掉 active（如 port 8081 的容器）
docker compose -f docker-compose.cluster.yml stop hydra-control-a

# 3. ≤ 宽限+租约（~20s）后，standby 提升：
curl -s localhost:8082/healthz/leader          # → 200（新 active）
#    管理变更经 standby 转发到新 active，无需重定向

# 4. edge 轮询失败 → 注册表旋转 → 继续同步（数据面无感）
# 5. 恢复旧节点：以 standby 身份加入（自动降级 + 重建副本）
docker compose -f docker-compose.cluster.yml start hydra-control-a
```

**验证清单**：
- [x] 单节点模式（`HYDRA_ROLE` 未设置）零行为变化
- [x] active 宕机 → standby 自动提升，管理写入恢复，edge 继续服务
- [x] 跨节点限流：两节点合计超 `limit_count` 即 429
- [x] 熔断：A 节点 trip → B 节点 ≤1s 收敛排除
- [x] 认证失效：`DELETE /api/v1/auth/cache` → 全节点 ≤0.5s 清本地缓存
- [x] 证书轮换：admin PUT 新 PEM → 全节点 ≤poll 生效（无共享卷、无文件操作）

### 5.1 实测记录（2025-08，本地 docker redis + 双 leader + 双 edge）

| 项 | 结果 |
|---|---|
| 故障切换（两次） | ~11s / ~18s（租约 15s + tick ≤5s，注册表轮换失效修正后） |
| 旧 leader 回归 | 租约感知轮换 → 自动跟随新 active 重建副本（物化新版本 + 证书） |
| 跨节点限流 | 5 次 200 后第 6 次 429（两 edge 合计计数） |
| 共享熔断 | trip → 投票落 Redis → 对端 ≤1s 收敛 503；probe 仅 <500 才复活（防振荡）；上游恢复后自动撤销 |
| 认证失效 | DELETE 后请求变 MISS（L1 确实被清），事件流 500ms 内消费 |
| 版本持久化 | 重启后版本从 `config_meta` 恢复（不再重置为 1），`since` 水位跨重启单调 |

**验收中发现并修复**：① 轮换锁定字典序第一的死节点（→ 跳过当前失败目标）；② 事件消费者对空 stream 的 nil 回复解析失败死循环（→ 原始 `Value` 判空）；③ 熔断投票任务在 Pingora runtime 上 `tokio::spawn` 不执行（→ 改投 bg runtime `Handle::spawn`）；④ `SharedBreaker` 包裹了与路由无关的独立 breaker（→ 必须包裹路由用的同一实例）；⑤ probe 对任意 HTTP 响应判活导致熔断振荡（→ 仅 <500 复活）；⑥ 版本号重启重置导致 `since` 水位失效（→ 持久化到 `config_meta`）。

**已知限制**（未在本轮修复）：
- 禁用的 `limit_role`/`provider_key_binding` 只存在于 active 本地 DB，**不进快照**（`build_config` 只带 enabled 行，契约见 T5.7）—— 故障切换后该行在副本/新 active 上丢失，需重新创建。修复方向：快照单独携带禁用行。
- 新鲜度闸门基于"最近一次轮询成功"：重启后立刻提升的极端场景（active 同时死亡）仍可能以旧副本上任（租约感知轮换只覆盖"active 存活时回归"的常规路径）。
- `HYDRA_FAILOVER_GRACE_MS` 已文档化但未接线；`HYDRA_BREAKER_QUORUM`/`HYDRA_RATE_LIMIT_FAIL_MODE` 为代码内默认值。

---

## 6. 安全说明

- **明文永不跨节点**：provider key 与证书私钥在快照中均为 AES-256-GCM 密封，
  节点用 `HYDRA_ENCRYPTION_KEY` 本地解密（全集群一致）；
- **管理面永不返回明文私钥**（单节点语义延续）；
- **集群共享 `HYDRA_ADMIN_TOKEN`**（standby 转发管理变更的前提）；
- **Redis 建议开启 ACL + 内网隔离**；生产用托管多 AZ（故障切换窗口最小化）。
