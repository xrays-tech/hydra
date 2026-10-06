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
| `edge` | 无状态数据面 | ❌ | ❌（**无 token 时 `/metrics` 也是 401**；仅 `/healthz` `/readyz` 免鉴权） | 配置随快照分发，可任意扩缩。`/metrics` 走的是与别处**同一条** token 门禁（早期它在这里免鉴权，于是「指标暴露面取决于角色」——而官方拓扑把 edge 的管理口绑在 `0.0.0.0`，那正是唯一要紧的部署）。官方 compose 的 `hydra-edge` 只注入 `HYDRA_CLUSTER_TOKEN`，**没有** `HYDRA_ADMIN_TOKEN`，所以按现状抓取必然 401；要让抓取器拿到指标，必须先做一次取舍（给 edge 注入 admin token / 把 edge 管理口绑到私有地址后另行免鉴权 / 新增仅抓取用的 token），见 `ops.md` §9 |

**自维持**（集群在任何编排环境下自我管理）：
1. **自举**：节点只需 `HYDRA_REDIS_URL` + `HYDRA_CLUSTER_TOKEN` → 注册表发现 leader → 拉全量快照（含证书）→ 开始服务；
2. **自动选举**：leader 候选经 Redis 租约竞争，恰一个 active；
3. **自动故障切换**：active 死亡 → 租约过期 → 合格候选提升（实测 ≤ 租约 15s + 选举 tick 5s，约 11–18s），edge 数据面与控制面均无感（edge 轮询失败自动经注册表旋转到新 active；standby 也按**租约持有者**轮换 —— 即使它的静态 `HYDRA_CONTROL_URL` 指向自己，见 §5.1）；**管理变更同样按租约持有者转发**：standby 的管理写入目标在转发时从注册表实时解析（绝不使用静态 `HYDRA_CONTROL_URL`，它可能指向节点自身），且每个转发请求带 once 标记，任何自转发/互转循环都会立即 fail-closed 503 而不是超时递归（见 §5.2）；

   > **⚠ 下面这段复核记录的是 ADR-0001 之前的 leader/edge 拓扑（2026-09-29 实测）**：`HYDRA_ROLE`、
   > `HYDRA_CONTROL_URL`、Redis 租约与注册表**全部退役**，其中点名的 `hydra_control_snapshot_version`
   > 与 `hydra_control_poll_total{result="ok"}` **已不再注册**（前者的记录器随轮询客户端删除，后者
   > 的 `result="ok"` 从来没有被写过）。今天的对应信号是 `hydra_arachne_this_node_leader`（谁在写）、
   > `hydra_arachne_publish_total{result}`（发布是否成功）与 `hydra_arachne_leader_flips_total`（切主频率），
   > 见 `ops.md` §9.1。**这段数字仍然是当时那份测量的忠实记录**，不是现在的行为描述。

   **复核（2026-09-29，两节点 + edge 实测）**：
   - **提升耗时 17.7 s / 18.2 s**（两次独立运行，硬杀 `SIGKILL` 口径）——正好落在上面那个 11–18s 带的上沿；粒度为 `HYDRA_LEADER_LEASE_MS` 15s + 选举 tick。
   - **edge 数据面确实"无感"**：整个故障切换期间以 20 rps 持续压 edge 的数据面（`Host:` 指向真实租户）⇒ **355/355 全是 200，0 次连接被拒、0 个非 200**；旧 active 死亡后新 active 就位、`standby` 被提升，edge 侧没有任何一次失败。
   - **edge 控制面会自己旋转到新 active**（即使它的静态 `HYDRA_CONTROL_URL` 指向已被杀死的节点）：在新 active 上改一次配置后，edge 自己的 `hydra_control_snapshot_version` 从 **6 → 7** 前进，`hydra_control_poll_total{result="ok"}` 从 **2 → 17** —— 即"按租约持有者从注册表实时解析"这条链路是活的。
   - **实测夹具的坑（值得写下来）**：跑这套 drill 必须**显式清空自己用的 Redis DB**。共享的测试 Redis 不是私有的：上一次运行（或别的进程）留下的租约/注册表条目会让新的 leader 候选"忠实地"把控制轮询目标旋转到那个**已经不存在的节点**上并 fail-closed —— 我第一次运行就是这样得到一支从未被正确初始化的集群（所有请求 404、主节点从未当选），却看起来像产品缺陷。两个 drill 现在都先用 RESP 显式 `SELECT`+`FLUSHDB`（本机没有 `redis-cli`）。
4. **自动加入/退出**：edge 无状态任意增删；leader 候选可加可减；
5. **自愈**：租约时间栅栏杜绝双写、快照版本冲突由胜方覆盖、熔断投票 TTL 自清理、失效事件流跨故障切换不丢（幂等重放）。

---

## 2. 环境变量

| 变量 | 默认 | 说明 |
|---|---|---|
> **`HYDRA_ROLE` 已退役（ADR-0001）**：集群判定改为「有没有配 `HYDRA_CLUSTER_PEERS`」，成员表就是决策本身，不再有可拼错的角色变量与静默回落。代码里已无读取点，设了不生效，因此本表已删除该行（`scripts/check_documented_env.cjs` 把配置表当承诺：要么接线，要么移出表）。取代它的名字见下方新行；本节其余内容仍描述 Redis 租约世界，随 ADR-0001 同步的排期在计划的 T4.3。
| `HYDRA_REDIS_URL` | — | **集群必填**（fail-closed）；如 `redis://redis:6379` |
| `HYDRA_REDIS_MODE` | `single` | `single`（默认，大小写不敏感）/ `sentinel` / `cluster`（后两者接线中，**fail-fast**）；**其它任何值也 fail-fast**（拼错不得静默降级为 `single`）。**限定**：该开关**只在集群角色分支里被读取**（`if role.is_cluster()`，`main.rs`）⇒ `HYDRA_ROLE` 未设/`all` 时**根本不校验**，此时最多只会被那条 "cluster wiring is configured but …" 的 ERROR 提到**变量名**、**不会**出现拼错的值（第一百九十四轮实测，由 drill 的 K12 钉住） |
| `HYDRA_CLUSTER_TOKEN` | — | **集群必填**：控制通道共享 token（leader 服务、edge/standby 调用） |
| `HYDRA_CONTROL_URL` | — | leader/edge 必填：**控制面快照轮询**端点（active leader 的管理端点，如 `http://hydra-control:8081`）。注意它不是管理变更的转发目标 —— 转发目标在转发时按**租约持有者**从注册表实时解析（见 §5.2），因此候选节点把该变量指向自己也是安全的 |
| `HYDRA_PUBLIC_URL` | — | 本节点注册到注册表的可达管理端点（如 `http://hydra-control-a:8081`）；leader 建议必填 |
| `HYDRA_CONTROL_POLL_MS` | `1000` | 控制快照轮询间隔（standby 副本同步可收紧至 200） |
| `HYDRA_LEADER_LEASE_MS` | `15000` | 租约时长（续约每 lease/3） |
| `HYDRA_FAILOVER_GRACE_MS` | `5000` | **预留，尚未接线**（见 §5.1 实测：故障切换 ≈ 租约过期 + 轮换 + 选举 tick） |
| `HYDRA_NODE_ID` | 自动生成 | 节点标识（租约持有者/熔断投票者/注册表条目）；回退顺序 `HYDRA_NODE_ID` → `HOSTNAME` → 随机，回退前提见 §3.1 |
| `HYDRA_REGISTRY_STALE_GRACE_SECS` | 120 | 回收见证键的 TTL（= 宽限窗口）；`<= 0` 视为未设。过小会缩短"仅一时静默"的节点的保护窗口 |
| `HYDRA_USAGE_SINK` | `sqlite` | **集群必须 `clickhouse`**（fail-closed） |
| `HYDRA_CLICKHOUSE_URL` | — | sink=clickhouse 时必填 |
| `HYDRA_ADMIN_TOKEN` | — | **leader 必须**：全集群共享（standby 转发管理变更时沿用） |
| `HYDRA_ENCRYPTION_KEY` | — | **全集群一致**（provider key 与证书私钥共用同一主密钥） |

> **集群启动 fail-closed 检查**：leader/edge 缺 `HYDRA_REDIS_URL`、`HYDRA_CLUSTER_TOKEN`、
> `HYDRA_CONTROL_URL`，或 `HYDRA_USAGE_SINK≠clickhouse`，或 leader 缺
> `HYDRA_ADMIN_TOKEN` / `cluster-redis` feature → 拒绝启动。

---

## 3. 共享状态（一个 Redis，七个用途）

> **控制面权威 = Arachne；Redis 只承载数据面的近似状态**（ADR-0001，2026-10-05）。
> 本节以下的表格与行为描述里，**leader 租约 / 节点注册表 / 配置快照 HTTP 推送**这三项已由 raft 领导权
> 与 Arachne 键路径配置树取代，属于**已退役**的机制；Redis 剩下的四项（共享限流 / 熔断投票 / 认证缓存 L2 /
> 失效总线）仍然是它的职责，D-1 明确「不搬」。
>
> | 子系统 | 现在由谁承载 |
> |---|---|
> | leader 身份 | **raft 领导权**（`ArachneControl` 的写探测回答「本节点能否提交」） |
> | 配置权威 | **Arachne `ctl/head` 内容哈希**（每节点各自物化，见 `dev-docs/aegis/plans/2026-10-05-arachne-control-plane.md`） |
> | 共享限流 / 熔断 / 认证 L2 / 失效总线 | **Redis**（不变） |
>
> 本节其余内容作为**历史与数据面细节**保留；控制面部分在 T4.3 基线同步时逐条改写。


| 子系统 | Key | 说明 |
|---|---|---|
| leader 租约 | `hydra:{lease:leader}` | `SET NX PX` + Lua 原子续约（只续自己的） |
| 节点注册表 | `hydra:{nodes}` + `hydra:{node:hb}:<id>` + `hydra:{node:seen}:<id>` | 注册/心跳（TTL 30s）/leader 发现；第三个键是**回收见证**（TTL = grace，见 §3.1） |
| 失效总线 | `hydra:{ctl:events}` + `hydra:{ctl:gen}` | Streams 持久可重放 + generation 兜底 |

### 5.1a 失效事件流的裁剪语义（2026-09-29 修正）

**旧行为（缺陷）**：裁剪固定用 `XTRIM MAXLEN 10000`（每 30 s 一次），且**只要删掉了条目就 bump generation** ⇒ 超过 `10000 / 30 ≈ 333 事件/秒` 时**每一次裁剪都会删条目**，于是**每个节点每 30 s 清空整个 L1+L2 认证缓存**——与消费者是否落后无关。一个租户每分钟 2 次 invalidation 就能把全集群缓存永久维持在冷态（节流只限速率、不限后果）。

**现行为**：裁剪仍按 `MAXLEN` 执行（内存上界不变），但 bump 只在**删掉的条目中有人尚未应用**时才发生。判定依据是"最慢存活消费者的 applied watermark"（`hydra:{ctl:inv:applied}:<node>`，`InvalidationStream::slowest_live_watermark`）：只有当**每一个存活节点都有 watermark**、且被删的最新条目 ≤ 最慢 watermark 时，才证明"这些条目所有人都已应用"⇒ 不 bump、不清缓存。任何一环无法证明（没有存活视图、某个存活节点从未发布 watermark、watermark 不可解析）都退回旧行为（bump），所以安全方向不变。

**指标**（原先整条链路零指标）：
- `hydra_invalidation_trimmed_total` —— 裁剪掉的条目数（无论是否需要 bump）；
- `hydra_invalidation_generation_bumps_total` —— 真正"丢了没人读过的条目"的次数；**非零表示真实的收敛损失**（或事件速率超过 `maxlen / 裁剪间隔` 且有消费者跟不上）。
| 共享限流 | `hydra:{rl:role:bucket}:count|tokens` | Lua 滑动窗口（同 `{rl:...}` tag 同槽） |
| 共享熔断 | `hydra:{br}:dead:{p}` + `hydra:{br}:alldead` | 投票 + 心跳 TTL + 本地 1s 同步 |
| 认证缓存 L2 | `hydra:{auth}:{tenant}:{keyhash}` + 索引 | L1 miss 才访问；租户索引免 SCAN |
| — | 命名空间规则 | 多 key 操作必须同 hash tag；禁 SCAN/MATCH（Cluster 安全） |

**Redis 故障行为**（数据面永不受影响——edge 持 last-known-good 快照与本地缓存）：

| 子系统 | Redis 宕机行为 |
|---|---|
| 配置快照 | ~~暂停更新（快照走 leader HTTP）~~ 已退役：配置走 Arachne，失 quorum 时各节点继续用本地已物化状态服务（ADR-0001 验收 5） |
| 限流 | fail-open（**硬编码，NOT configurable**：`HYDRA_RATE_LIMIT_FAIL_MODE` 从未实现 —— `grep -rn RATE_LIMIT_FAIL_MODE crates/` 为空）+ 告警指标（`hydra_control_poll_total{result="rate_limit_error"}`） |
| 熔断 | 退回本地 trip（投票不同步，本地死集仍生效） |
| 认证 L2 | 退回纯 L1（失效传播暂停，条目按 TTL 过期） |
| 选举 | ~~续约失败 → 立即降级停写~~ 已退役：领导权来自 raft，与 Redis 无关；Redis 断连只影响上面四项数据面状态 |

> **2026-09-30 实测修正（P1，已修）**：上表最后一行原先并不成立。连接池是用
> `Pool::new(…, policy: None, …)` 建的 —— **fred 根本没有重连策略**（策略不是 `Config` 的字段，
> 别处也补不上），所以"连接被切断"之后**一次都不会重拨**：实测切断 ~2s 再恢复后，**90 秒内新建连接
> 数 = 0**，每条命令都撞 500ms 命令超时，`/healthz/leader` **整整 90 秒 503**（租约永远拿不回来 ⇒
> 集群长期没有 leader），限流的 fail-open 从"临时"变成"永久"，**只有重启能恢复**。fred 自己的
> debug 日志点名了原因：`Checking reconnect state. Has policy: false`。修法：`redis::reconnect_policy()`
> （`max_attempts = 0` = 永久重试，间隔 1s、带 jitter）交给 `Pool::new`，单测 + 集成 drill
> `integration/test_cluster_limits.py` 双侧钉住；修复后切断期间每秒重拨、恢复后 ~3s 重新当上 leader、
> `hydra_control_poll_total{result="rate_limit_error"}` 立刻停止增长。运维侧说明见 `ops.md` §13.5 / §9.1。

### 3.1 注册表：值格式、回收判据与身份前提

本节是注册表契约的权威说明；运维侧的取值要求见 `ops.md` §13.5b/§13.6。

**值格式是冻结的**：`hydra:{nodes}` 的字段值恒为 `role|control_url`（如
`leader|http://hydra-control-a:8081`）。**不得**往里追加时间戳或版本前缀——
追加会让时间戳粘进 `control_url`（把对端的转发目标写坏），加 `v2|` 前缀会让
`role` 变成 `"v2"`，而 `active_leader_url()` 按 `role == "leader"` 判断 ⇒ 返回
`None` ⇒ **每一次 standby 管理写都 503**。因此"最后可见时间"放在**独立键**里：

| 键 | 语义 |
|---|---|
| `hydra:{nodes}` | `node_id → "role|control_url"`（值格式不变） |
| `hydra:{node:hb}:<id>` | 心跳，TTL 30s；缺失即视为离线 |
| `hydra:{node:seen}:<id>` | 回收**见证**，TTL = `HYDRA_REGISTRY_STALE_GRACE_SECS`（默认 120） |
| `hydra:{node:reap}:<id>` | 回收器自己的"一击"标记（无 TTL；见下） |

**续期与注册是同一个入口**（`NodeRegistry::register(ttl, seen_ttl)`，每 20s 调用）：
它同时**重写行**、续心跳、续见证。历史实现把两者分开（启动注册一次、20s 只续心跳），
结果是节点启动后 `role`/`control_url` 变化时行值**永远停在启动值**却一直"活着"。

**回收判据（两击）**：一次扫描同时满足"心跳缺失 **且** 见证缺失"时**不立即删除**，
而是写一个 `hydra:{node:reap}:<id>` 标记并跳过；只有**下一次**扫描仍处于同一状态
（≥60s 连续静默，即三个心跳周期）才真正删除，任何生命迹象（心跳或见证键）都会
**清除**该标记。**当前租约持有者永不回收**（`active_leader_url()` 不看心跳，删掉它的行
会让所有备用节点失去唯一的转发指针）。

> 两击规则不是保守，而是必要：**未升级节点只在启动时写一次行**（其 20s 循环只续心跳），
> 所以一次 ≥30s 的心跳中断若导致行被删，该节点会**永远**从 `list_nodes`/
> `leader_control_urls` 消失；若它正持租约，所有 standby 的管理写会**永久 503**
> （转发是 fail-closed，无静态回退）。历史积压仍会被清理，只晚一个 tick。

**身份前提（控制器要求）**：节点身份取 `HYDRA_NODE_ID` → `HOSTNAME` → 随机。
`HOSTNAME` 这一层只在 **StatefulSet**（或固定 Pod 名的 Deployment）下稳定；普通
Deployment 每次重启都换 Pod 名，该层回退**无收益**。**两个节点绝不能共用同一个
`HOSTNAME`**：它们会共用一行注册，任一方的停机 `unregister()` 会删掉对方的注册；
更严重的是——**该 id 同时是租约身份**（`GET hydra:lease == 本节点 id` 即续租）——
两个进程会**同时认为自己是 leader**（脑裂），而两个"leader"会各自接受管理写并
各自发布快照。生产上用显式 `HYDRA_NODE_ID` 或 StatefulSet 固定名。

---

## 4. 部署

### 4.1 docker-compose（推荐起步）

```bash
cd environment
export HYDRA_ADMIN_TOKEN="$(openssl rand -hex 32)"      # 必填，>= 16 字符
export HYDRA_CLUSTER_TOKEN="$(openssl rand -hex 32)"    # 必填（控制通道）
export HYDRA_ENCRYPTION_KEY="$(openssl rand 32 | base64)"   # 全集群必须一致
docker compose -f docker-compose.cluster.yml up -d --scale hydra-edge=2
# 管理面：指向任一 leader 候选（standby 自动转发到 active）
curl -H "Authorization: Bearer $HYDRA_ADMIN_TOKEN" http://localhost:8081/api/v1/tenants
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
            - { name: HYDRA_ADMIN_ADDR, value: 0.0.0.0:8081 }   # 探针/Service 需从 Pod 外访问 admin
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
            - { name: HYDRA_ADMIN_ADDR, value: 0.0.0.0:8081 }
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

> **edge TLS**：用 `HYDRA_TLS_LISTEN=0.0.0.0:8443` 让 edge 额外绑定 TLS 监听器
> （证书随快照分发，无需共享卷；证书写入后由快照交换自动重解析，**无需重启**）。
> 明文 `HYDRA_LISTEN` 恒定存在——曾有过的 `HYDRA_EDGE_TLS` 开关文档提过、代码从未读取，
> 已删除；而“有证书就把唯一监听器切成 TLS”的做法已按事故报告修掉
> （`dev-docs/bug-2026-09-16-tenant-cert-flips-listener-to-tls.md`）。

### 4.3 裸机 / VM

同一二进制 + systemd；`HYDRA_CONTROL_URL`/`HYDRA_PUBLIC_URL` 用主机名或 VIP；
leader 用 `HYDRA_NODE_ID` 固定标识。

---

### 4.x Liveness supervision (added 2026-09-29)

每个 hydra 服务都带 **按角色** 的 container healthcheck，且路径必须随角色而变：

| 角色 | 探针 | 原因（2026-09-29 在真实两节点集群上实测） |
|---|---|---|
| `leader`（control-a/b） | `curl -fsS -H "Authorization: Bearer $HYDRA_ADMIN_TOKEN" http://127.0.0.1:8081/api/v1/health` | 管理口需 token；不带 token 会 401 而永远 unhealthy |
| `edge` | `curl -fsS http://127.0.0.1:8081/healthz` | **edge 不提供管理 API**：`/api/v1/health` 返回 **404**（实测），`/healthz`/`/readyz` 免鉴权 200；`/metrics` 无 token 401、带 token 200 |

官方 `environment/docker-compose.cluster.yml` 此前**三个服务都没有 healthcheck**（单机与本地栈都有）—— 现已补上，并由 `scripts/check_compose_health.cjs`（CI `scripts` 作业 + 本机门禁）守住"角色 ↔ 探针路径"的匹配。

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
- **物化失败的重试不再有次数上限（2026-09-29）**：`MaterializationGuard` 曾对每个快照只重试 **3 次**，用尽后**永久**不再尝试——而控制客户端不会重投"内存水位已越过"的版本，于是节点只有两条回头路：一次**更新的配置写入**（需要一个能工作的 leader，恰恰是"没有可当选节点"的集群给不出的）或重启。现在改为**次数不限、速率受限**：失败后按 1s→2s→4s…（上限 60s）退避，新快照立即重置退避与失败计数；`materialize` 本身幂等且被版本闸门与 `in_flight` 串行保护，所以重试是安全的。可观测：`hydra_replica_materialize_retries_total{outcome="attempt|succeeded|failed|throttled"}`——`failed` 持续增长说明节点物化不了（因而不能当选），这个状态以前既**永久**又**不可见**。
- 新鲜度闸门基于"最近一次轮询成功"：重启后立刻提升的极端场景（active 同时死亡）仍可能以旧副本上任（租约感知轮换只覆盖"active 存活时回归"的常规路径）。
- `HYDRA_FAILOVER_GRACE_MS` 已文档化但未接线（`grep -rn HYDRA_FAILOVER_GRACE_MS crates/` 为空）；`HYDRA_RATE_LIMIT_FAIL_MODE` **代码里根本不存在**（`grep -rn RATE_LIMIT_FAIL_MODE crates/` 为空），Redis 宕机时限流是**硬编码 fail-open、NOT configurable**（`crates/hydra-server/src/redis/rate_limit.rs` 的 `warn!("redis rate-limit check failed; failing open")` + 注释 "there is NO env override"）。`HYDRA_BREAKER_QUORUM` 有代码内默认值 `1`，且**确实被读取**（`crates/hydra-server/src/main.rs` 的 `std::env::var("HYDRA_BREAKER_QUORUM")`）。

### 5.2 管理变更转发（standby → active）与循环防护

**转发目标 = 实际租约持有者（绝不使用静态 URL）**：standby 收到管理变更
（POST/PUT/DELETE）时，转发目标在转发时刻从节点注册表实时解析
（`NodeRegistry::active_leader_url()`，即 Redis 租约持有者注册的
`HYDRA_PUBLIC_URL`）。静态 `HYDRA_CONTROL_URL` **只**用于控制面快照轮询，
不作为转发目标——它可能指向节点自身（主候选的常见配置），且无法跨故障切换
跟踪租约。

**三层防护**（缺一不可，防御纵深）：
1. **注册表解析**（主修复）：目标始终跟随实际租约持有者，故障切换后无需
   重配置；
2. **自转发护栏**：若解析出的目标恰是本节点自己的注册 URL（误注册/共用
   URL），拒绝转发并 fail-closed 503，绝不自转发；
3. **forward-once 标记**（`x-hydra-forwarded`）：每个转发请求携带标记，
   收到带标记变更的非 active 节点直接 503，不再二次转发——任何自转发或
   节点间互转循环都会立即终止，而不是 5s 超时递归。

**验证**：standby 上 `POST /api/v1/providers` → 成功落盘到 active 并回 201；
向 standby 直接构造带 `x-hydra-forwarded` 的变更 → 503 `forward_loop`；
目标不可达/无租约 → 503 `not_leader`（fail-closed，绝不本地代写）。

---

## 6. 安全说明

- **明文永不跨节点**：provider key 与证书私钥在快照中均为 AES-256-GCM 密封，
  节点用 `HYDRA_ENCRYPTION_KEY` 本地解密（全集群一致）；
- **管理面永不返回明文私钥**（单节点语义延续）；
- **集群共享 `HYDRA_ADMIN_TOKEN`**（standby 转发管理变更的前提）；
- **Redis 建议开启 ACL + 内网隔离**；生产用托管多 AZ（故障切换窗口最小化）。
