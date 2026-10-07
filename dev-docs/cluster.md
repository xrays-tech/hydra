# Hydra 集群模式（Cluster Mode）

> **集群是 opt-in，而且只有一个开关**：设了 `HYDRA_CLUSTER_PEERS`（静态成员表）就是集群成员，
> 没设就是单节点——行为与之前完全一致、零外部依赖。**成员表就是决策本身**：没有可拼错的角色变量，
> 也就没有"拼错后静默回落到单节点"这条路径（ADR-0001 退役了 `HYDRA_ROLE`）。
>
> **集群里每个节点完全一样**：都跑数据面、都跑管理面、都是 raft 成员、都能当 leader。不存在
> `edge` 角色，也不存在"无状态数据面"（那是 D-2 撤回的产品承诺）。
>
> **控制面权威是 Arachne（raft 线性化 KV）**；**Redis 只承载数据面的近似状态**（限流 / 熔断 /
> 认证缓存 L2 / 失效总线，D-1 明确"不搬"）。因此 Arachne 失去多数派时数据面继续服务，而 Redis
> 断连时控制面完全不受影响。
>
> 权威文档：[`ADR-0001`](aegis/adr/ADR-0001-arachne-control-plane.md)（决策与论证）、
> [`计划`](aegis/plans/2026-10-05-arachne-control-plane.md)（实现与实测记录）、
> [`ops.md`](ops.md) §13（运维动作）。本文是集群的**行为与契约**说明。

---

## 1. 节点模型

| 状态 | 判定 | 数据面 | 管理面 | 本地 SQLite | raft |
|---|---|---|---|---|---|
| 单节点（默认） | 没设 `HYDRA_CLUSTER_PEERS` | ✅ | ✅ 全部 | ✅ 权威 | ❌ 完全不启动 |
| 集群成员 | 设了 `HYDRA_CLUSTER_PEERS` | ✅ | ✅ 全部 | ✅ **可重建的物化状态**（权威是 `head`） | ✅ 成员表里的一个节点 |

**成员表就是集群的身份**，三点必须一起理解：

1. **顺序 = raft id**：每个成员的数字 id 是它在表里的**位置**（1-based）。**改顺序 = 换一个集群**，
   不是"重排一下"。
2. **至少 3 台**：`MINIMUM_MEMBERS = 3`，少于 3 个条目**在解析成员表时就拒绝启动**
   （`TooFewMembers`）——两台互相都容不了故障，比一台更不可用。
3. **每个成员必须能在表里找到自己**：`HYDRA_NODE_ID` 必须出现在表中，且
   `HYDRA_ARACHNE_LISTEN` 必须与**自己那一项**的地址一致（`SelfNotAMember` /
   `ListenMismatch`），否则拒绝启动——"peers 会拨一个地址而本节点绑另一个"是必须当场说出来的错。

**为什么没有 edge**：一个不持有配置数据库的节点无法物化配置树，而"所有节点配置相同"正是集群行为
可预测的前提。代价如实记账：**每台都需要持久卷**（raft WAL + SQLite），**扩容 = raft 成员变更**
（见 §6.3），不再是把副本数 +1。

---

## 2. 环境变量

运维侧的完整表与默认值以 [`ops.md`](ops.md) §1.1 / §13.3 为权威；这里是集群**特有**的部分。

| 变量 | 必填 | 说明 |
|---|---|---|
| `HYDRA_CLUSTER_PEERS` | **是** | 静态成员表，`name=host:port` 逗号分隔。**每个成员的值必须完全相同**（含顺序）。≥3 项 |
| `HYDRA_NODE_ID` | **是** | 本节点在表里的名字。**集群下不要依赖 `HOSTNAME` 回退**：普通 Deployment 每次重启换 Pod 名，身份跟着漂 |
| `HYDRA_ARACHNE_LISTEN` | **是** | 本节点 raft 传输绑定的地址，**必须等于表里自己那一项** |
| `HYDRA_ARACHNE_DATA_DIR` | 否 | raft 数据目录。默认 = SQLite 路径所在目录下的 `arachne/`（`sqlite:/app/data/hydra.db` ⇒ `/app/data/arachne`） |
| `HYDRA_CLUSTER_ID` | **建议显式** | 集群身份。默认 = **成员表内容的哈希** ⇒ **改成员表就会改身份**，而每个节点的数据目录记录着旧身份，于是全部拒绝启动。**要在将来做成员变更，就从第一天起显式设一个人类可读的名字**（见 §6.3，含实测报错） |
| `HYDRA_REDIS_URL` | **是** | 数据面骨干（fail-closed，集群模式缺它就拒绝启动） |
| `HYDRA_REDIS_MODE` | 否（默认 `single`） | 只接受 `single`；其它任何值**快速失败**。**限定**：这个开关**只在集群模式下被读取**，单节点默认下既不校验也不提及（由 `integration/test_startup_knobs.py` 的 K12 钉住） |
| `HYDRA_USAGE_SINK` | **是** | 必填、无默认：`clickhouse`（共享存储）或 `none`（显式不计量）。集群下逐节点用量必然错，所以前者是实际选择 |
| `HYDRA_ADMIN_TOKEN` | **是** | 每个节点都要，因为**每个节点都提供管理 API** |
| `HYDRA_ENCRYPTION_KEY` | **是** | **全集群必须一致**（provider key 与证书私钥共用同一主密钥） |

### 2.1 已退役的变量（设了会在启动时被点名）

`cluster/mod.rs` 的 `RETIRED_CLUSTER_ENV` 是这张表的**唯一所有者**，启动时若有任何一个被设置，
会打一条 ERROR **逐个点名**它们（"these variables were retired … and are IGNORED"）：

| 退役的名字 | 被谁取代 |
|---|---|
| `HYDRA_ROLE` / `HYDRA_EDGE` | **成员表**（有没有配 `HYDRA_CLUSTER_PEERS`） |
| `HYDRA_CLUSTER_TOKEN` | **无需替代**（2026-10-05 删除）：它守的是 `/api/v1/internal/*`，而那一族的两条路由都已退役。**它不再是启动要求**，集群现在只需要 `HYDRA_ADMIN_TOKEN` |
| `HYDRA_CONTROL_URL` | 无需替代：节点之间不再互相轮询；配置从 Arachne 物化 |
| `HYDRA_PUBLIC_URL` | 无需替代：`leader_hint` 提供 leader 的 **raft 地址**，而它只用于显示 |
| `HYDRA_LEADER_LEASE_MS` | **raft 选举**（LAN profile，选举超时量级） |
| `HYDRA_CONTROL_POLL_MS` | 每个节点自己的物化循环（1 s 一跳，版本闸门挡住空转） |
| `HYDRA_REGISTRY_STALE_GRACE_SECS` | 无需替代：没有注册表行可回收 |
| `HYDRA_FAILOVER_GRACE_MS`（从未接线） | 无需替代（切换由选举决定） |
| `HYDRA_FORWARD_TIMEOUT_SECS` | 无需替代：Hydra 侧零转发 |

**集群启动 fail-closed 检查**：缺 `HYDRA_REDIS_URL` / `HYDRA_ADMIN_TOKEN`、未设 `HYDRA_USAGE_SINK`、
成员表 <3 项或自身不在表里、二进制没编 `arachne` feature ⇒ **拒绝启动**。
（单节点模式只受 `HYDRA_ADMIN_TOKEN` 与用量 sink 的约束。）

---

## 3. 控制面：Arachne

### 3.1 键空间（权威定义在 `crates/hydra-server/src/cluster/arachne_keys.rs`）

| 键 | 内容 |
|---|---|
| `hydra/ctl/head` | **当前配置树的内容哈希** —— **提交点**，最后写 |
| `hydra/cfg/toc/<toc-hash>` | 该树的目录（每个实体的路径 + 内容哈希 + 长度） |
| `hydra/cfg/e/<path>/<content-hash>` | **每实体一键，键自带内容哈希** |
| `hydra/ctl/cluster_id` | 本节点数据目录记录下来的集群身份（"认领"机制，见 §5.6） |
| `hydra/ctl/format` | 键空间格式版本（`TOC_FORMAT = 3`） |

实体键**内容寻址**是刻意的：同一个实体的两个版本各有各的键，于是"并发发布互相撕裂"在键层面就
不可能发生，而"这个实体是否已经存过"由**键自身**回答（对自己副本查一次，不需要基线、不需要跨节点
协商）。**读取一律 `get_stale`**：线性读 `get` 在 follower 上会立刻返回 `QuorumUnavailable`，
所以"顺序严格"不是靠线性读保证的，而是靠**单写者 + 内容哈希 + 逐实体校验**。

### 3.2 写入与提交点

管理写打在**任意节点**都成立，序列是：

1. 入口节点在**本地 SQLite 事务**里提交（`BEGIN IMMEDIATE`，事务内重读活行 + 当前配置，校验归属/配额/前缀）；
2. 本地重建 `ConfigData` → 编码成分片 → 写实体（键已存在则跳过）→ 写 toc；
3. **最后写 `hydra/ctl/head`** —— 这一步才是提交点，也是**唯一**被库转发给 raft leader 的操作。

因此：**head 不前进 ⇒ 集群里没有任何节点会看到半份配置**。第 3 步失败时，本地事务**已经提交**，
所以错误是 `StoreError::NotPublished`（"已提交到本节点库、**未发布**到集群"），管理 API 答
**503 `config_not_published`**，而节点继续用自己那份新配置服务——文案不是"写失败"，因为回滚没有发生。

### 3.3 每个节点自己物化

每个节点一个循环（1 s 一跳）：读 `head` → 与本地已物化的哈希比较（**版本闸门**，没变就空转）→
变了才读 toc 与实体 → 解码 → 写回**本地** SQLite → 换入内存 `ConfigData`。失败按 1s→2s→4s…（上限
60s）**无限次**退避重试，新树立刻重置退避（`hydra_replica_materialize_retries_total{outcome}` 观测；
历史上曾是"3 次后永久放弃"，那会让一个节点在**没有任何回头路**的情况下停在旧配置上）。

**配置权威是 `head`，本地 SQLite 是可重建的物化状态。** 不一致时以 Arachne 为准——否则"本机改了
数据库就算数"会变成第二个真相。

### 3.4 领导权（写探测）与 `leader_hint`（只用于显示）

- **"本节点能否写"由写探测回答**：`without_redirect().put(探测键, 本节点id)` 成功即 `leader_ready`。
  不能用 `leader_hint` 自判——**刚当选的 leader 自己的 hint 在很长一个窗口内不指向自己**（实测），
  用它自判会让新 leader 拒绝写。
- **`leader_hint` 只用于显示**：舰队视图与 admin UI 的"谁是 leader"横幅由它填充（控制面每 250 ms
  缓存一次）。它**不参与任何写路径**，也不构成任何重定向契约（响应头与 409 契约随 D-3 一起不存在）。

### 3.5 失去多数派会怎样

| 面 | 行为 |
|---|---|
| 管理写 | **立刻** 503 `config_not_published`（不挂起、不静默接受；实测 0.0 s 返回） |
| `/healthz/leader` | 503（本节点无法提交 ⇒ 不许自称 leader） |
| 数据面 | **继续用已物化的配置服务**（真请求闸门→路由→上游实测 200） |
| 配置更新 | 停滞（没人能提交），恢复多数派后自动跟上 |

---

## 4. 数据面：一个 Redis，四个用途

控制面搬走之后，Redis 的职责只剩下面四项（D-1"不搬"）：

| 子系统 | Key | 说明 |
|---|---|---|
| 共享限流 | `hydra:{rl:role:bucket}:count\|tokens` | Lua 滑动窗口（同 `{rl:...}` tag 同槽） |
| 共享熔断 | `hydra:{br}:dead:{p}` + `hydra:{br}:alldead` | 投票 + 心跳 TTL + 本地 1s 同步 |
| 认证缓存 L2 | `hydra:{auth}:{tenant}:{keyhash}` + 索引 | L1 miss 才访问；租户索引免 SCAN |
| 失效总线 | `hydra:{ctl:events}` + `hydra:{ctl:gen}` | Streams 持久可重放 + generation 兜底 |
| — | 命名空间规则 | 多 key 操作必须同 hash tag；禁 SCAN/MATCH（Cluster 安全） |

### 4.1 失效事件流的裁剪语义

**旧行为（缺陷）**：裁剪固定 `XTRIM MAXLEN 10000`（每 30 s），且**只要删了条目就 bump
generation** ⇒ 事件速率超过 `10000/30 ≈ 333/s` 时**每次裁剪都会清空全集群的 L1+L2 认证缓存**，
与消费者是否落后无关。

**现行为**：裁剪仍按 `MAXLEN`（内存上界不变），但**只有当被删掉的条目里有人尚未应用时才 bump**。
判据是"最慢存活消费者的 applied watermark"（`hydra:{ctl:inv:applied}:<node>`）：只有**每个存活节点
都有 watermark** 且被删的最新条目 ≤ 最慢 watermark，才证明"所有人都读过了"。任何一环无法证明就
退回旧行为（bump），所以安全方向不变。

指标：`hydra_invalidation_trimmed_total`（裁剪条目数）、
`hydra_invalidation_generation_bumps_total`（**真正丢了没人读过的条目** ⇒ 非零即真实收敛损失）。

### 4.2 Redis 故障行为

| 子系统 | Redis 宕机行为 |
|---|---|
| 配置 / 控制面 | **不受影响**：配置走 Arachne，与 Redis 无关 |
| 限流 | fail-open，**硬编码、不可配置**（`HYDRA_RATE_LIMIT_FAIL_MODE` 从未实现）+ 告警指标 `hydra_control_poll_total{result="rate_limit_error"}` |
| 熔断 | 退回本地 trip（投票不同步，本地死集仍生效） |
| 认证 L2 | 退回纯 L1（失效传播暂停，条目按 TTL 过期） |
| 领导权 | **不受影响**：领导权来自 raft |

> **2026-09-30 实测修正（已修）**：连接池曾用 `policy: None` 建，而 **fred 没有默认重连策略**，
> 于是连接被切断后**一次都不重拨**：实测切断 ~2s 后 90 秒内新建连接数 = 0，限流的 fail-open 从
> "临时"变成"永久"，只有重启能恢复。修法：`redis::reconnect_policy()`（`max_attempts = 0`，1s 间隔
> 带 jitter）交给 `Pool::new`。运维说明见 `ops.md` §13.5。

---

## 5. 部署

### 5.1 docker compose（推荐起步）

```bash
cd environment
export HYDRA_ADMIN_TOKEN="$(openssl rand -hex 32)"          # 每个节点都用它
export HYDRA_ENCRYPTION_KEY="$(openssl rand 32 | base64)"   # 全集群必须一致
./environment/build.sh                                      # 镜像含 server,cluster-redis,arachne
docker compose -f docker-compose.cluster.yml up -d
python3 environment/init.py                                 # 指向任一节点的管理口即可
```

`docker-compose.cluster.yml` 里三个服务**只有身份、端口与卷不同**（同一 environment 锚点）；
`HYDRA_CLUSTER_PEERS` 三处完全相同。

### 5.2 k3s / k8s

**必须是 StatefulSet（固定身份 + 独立 PVC），而且必须改一项默认值**：

```yaml
apiVersion: apps/v1
kind: StatefulSet
metadata: { name: hydra }
spec:
  serviceName: hydra
  replicas: 3
  # ↓↓↓ 首次安装（PVC 全新）必须这样：默认的 OrderedReady 会"等 pod-0 Ready 才起 pod-1"，
  #     而一个从未被认领的数据目录要等多数派（§5.6），于是 pod-0 永远等不到 ⇒ 集群起不来。
  #     已认领过的集群顺序启动也能起（实测：单台重启走本地读，~500 ms），但 Parallel 对两种情况都对。
  podManagementPolicy: Parallel
  template:
    spec:
      containers:
        - name: hydra
          image: hydra:latest
          env:
            - { name: HYDRA_CLUSTER_PEERS, value: "hydra-0.hydra:8091,hydra-1.hydra:8091,hydra-2.hydra:8091" }
            - { name: HYDRA_NODE_ID, value: "$(POD_NAME)" }            # StatefulSet 保证稳定
            - { name: HYDRA_ARACHNE_LISTEN, value: "0.0.0.0:8091" }
            - { name: HYDRA_CLUSTER_ID, value: "hydra-prod" }          # 显式！见 §6.3
            - { name: HYDRA_REDIS_URL, value: redis://redis:6379 }
            - { name: HYDRA_ADMIN_TOKEN, valueFrom: { secretKeyRef: { name: hydra, key: admin } } }
            - { name: HYDRA_ENCRYPTION_KEY, valueFrom: { secretKeyRef: { name: hydra, key: enc } } }
            - { name: HYDRA_USAGE_SINK, value: clickhouse }
            - { name: HYDRA_CLICKHOUSE_URL, value: http://clickhouse:8123 }
          ports: [{ containerPort: 8080 }, { containerPort: 8081 }, { containerPort: 8091 }]
          readinessProbe:
            # 每个节点都提供管理 API，探针同一条规则（带 token 探 /api/v1/health）
            httpGet: { path: /api/v1/health, port: 8081 }
            httpHeaders: [{ name: Authorization, value: "Bearer $(HYDRA_ADMIN_TOKEN)" }]
  volumeClaimTemplates:
    - metadata: { name: data }
      spec: { accessModes: [ReadWriteOnce], resources: { requests: { storage: 2Gi } } }
```

**不要**用 `readinessProbe: /healthz/leader` 做 Service 路由（那是旧 leader/edge 拓扑的做法）：
集群里**每个节点都能接受管理写**，把流量引到单一节点只会浪费另外两台。

### 5.3 裸机 / VM

同一二进制 + systemd；每个节点需要**持久卷**（raft WAL + SQLite）。三台之间必须能互相访问
`HYDRA_ARACHNE_LISTEN` 的端口（raft 传输），该端口只应对内网暴露（见 §8）。

### 5.4 探针与存活监督

**一条规则**：每个 hydra 服务都用带 token 的管理探针
（`curl -fsS -H "Authorization: Bearer $HYDRA_ADMIN_TOKEN" http://127.0.0.1:8081/api/v1/health`）。
`/metrics` 走同一条门禁。**探测路径只有这一条**：`/healthz` 与 `/readyz` 是旧 `edge` 角色提供的
免 token 探针，**随角色一起删除**（实测 2026-10-05：在管理口与数据口都返回 404）。由
`scripts/check_compose_health.cjs` 在 CI 里守住。

### 5.5 持久卷与数据目录

每个节点自带 `/app/data`（SQLite + `arachne/` raft 目录）。**卷是必需的，不是优化**：丢失它等于
丢掉该节点的 raft 日志与物化状态（节点能用 `head` 重建配置，但要以"新目录"重新认领，见 §5.6）。

### 5.6 数据目录的"认领"（**从全停恢复的关键**）

**一个从未被认领过的数据目录**必须由**多数派**先认领才能服务。节点启动时：先**本地读**目录里记录的
集群身份（`hydra/ctl/cluster_id`）——已经记着且与配置一致 ⇒ 立刻通过；**没记着** ⇒ 只能由 **leader**
写进去（这一步刻意**不走转发**），而 leader 需要多数派在场。重试窗口 `PREFLIGHT_DEADLINE` = **10 秒**。

四种情形，都已实测（2026-10-05）：

| 情形 | 结果 |
|---|---|
| **三台一起冷启动**（PVC 全新） | ✅ **1.5 s** 全部起来——三台在场 ⇒ 秒级选出 leader ⇒ leader 写身份 ⇒ 复制到每台本地目录 |
| **换盘 / 重挂卷**（另两台在跑） | ✅ **1.3 s**——新节点一加入 raft 就收到已有的身份，本地读随即通过 |
| **单台重启**（该目录认领过，哪怕另两台都不在） | ✅ **~500 ms**——认领是**本地读**，不需要多数派 |
| **只起一台**（组内 3 台，目录从未认领） | ❌ **10 s 后退出**：`no member adopted this node's Arachne data directory within 10s`——一台既选不出 leader 也写不了 |

所以实际规则是：**让"多数派"在同一个 10 秒窗口内互相可见**（最省事的做法就是同时启动全部节点）。
一旦认领成功，掉队的节点随时可以后来加入。

#### 从"全停"恢复：步骤（**先读，再动手**）

1. **先确认这是哪种情况**：三台的 `HYDRA_ARACHNE_DATA_DIR`（默认 `/app/data/arachne`）里**有没有**
   东西。**有** ⇒ 目录已被认领过，按 §5.1/§6.1 正常启动即可，**顺序起也没问题**（每台低头读自己那份）。
2. **目录全新（首次安装 / PVC 被清掉）** ⇒ 必须让**多数派同时可用**：
   - compose：`docker compose -f docker-compose.cluster.yml up -d`（所有服务几乎同时起，天然满足）；
   - K8s：`podManagementPolicy: Parallel`（见 §5.2）；
   - 裸机/systemd：**同时**（或把前两台的启动压在彼此的 10 秒窗口内）起 `hydra`，第三台事后加入即可。
3. **只有一台能起、其它暂时起不来**：这台会在 **10 秒**后退出，日志是
   `no member adopted this node's Arachne data directory within 10s …`，并且**会告诉你下一步该做什么**
   （多数派一起起 / `Parallel`）。**不要反复重启它**——重启不解决"没有多数派"；先把第二台弄起来。
4. **别把"认领失败"当成"卷坏了"**：它是**顺序问题**，不是数据问题。反过来，如果日志报的是
   `belongs to a different cluster` / `cluster_id mismatch`，那才是**配置问题**（成员表被改过、或卷挂错了），
   见 §6.3 的陷阱。
5. 恢复后按 §6.1 第 4 步确认：三台里**恰好一台**答 `/healthz/leader` 200，且各节点物化哈希 == `head`。

---

## 6. 故障切换演练

### 6.1 步骤

```bash
# 1. 看谁在写（三台各问一次；恰好一台 200）。
#    `/healthz/leader` 是唯一免 token 的路由（LB 要能不带密钥就路由到 writer）；
#    单节点节点上它答 404（"没有控制面"）。
for p in 8081 8082 8083; do echo -n "$p: "; \
  curl -s -o /dev/null -w '%{http_code}\n' localhost:$p/healthz/leader; done

# 2. 硬杀当前 leader（不优雅退出，演练的是最坏情况）
docker compose -f docker-compose.cluster.yml kill hydra-a     # 换成上一步里 200 的那台

# 3. 等新 leader（选举超时量级；实测 1.1–1.6 s）
for p in 8082 8083; do echo -n "$p: "; \
  curl -s -o /dev/null -w '%{http_code}\n' localhost:$p/healthz/leader; done

# 4. 数据面必须全程无感（这是验收 2 的口径，见下）；管理写打任意幸存节点都应成功，
#    失去多数派时应立刻 503 config_not_published

# 5. 恢复旧节点：它已有认领过的目录 ⇒ 直接以成员身份回来并自动跟上 head
docker compose -f docker-compose.cluster.yml start hydra-a
```

### 6.2 实测记录（2026-10-05，三个真进程）

| 项 | 结果 | 证据 |
|---|---|---|
| 选举耗时 | **1.08 s / 1.63 s**（两次独立运行，`kill -9`；预算 3 s） | `integration/test_arachne_control_plane.py` gate 1 |
| 双 leader | **切换全过程从未出现两台同时 200**（连续轮询） | 同上，gate 3 |
| 配置收敛 | 三个节点服务同一份配置（内容层）；`materialized() == head`（哈希层） | 同上 gate 4 + `tests/arachne_three_nodes.rs` |
| 任意节点写 | 打在三台上的管理写**全部 201**（入口节点就地应用并发布） | 同上，gate 1 |
| 失去多数派 | 管理写 **0.0 s** 返回 503 `config_not_published`；`/healthz/leader` 503；**真代理请求仍 200** | 同上，gate 5 |
| **数据面跨故障切换** | **20 rps × 60 s = 1200 次请求全部 200**（p50 2 ms / p99 3 ms / max 3 ms），第 15 s `kill -9` leader；被杀前后 ±3 s 的 120 次也全 200 | `integration/test_data_plane_failover_load.py`（验收 2） |

旧 leader/edge 拓扑下的历史数字（切换 11–18 s、edge 355/355）**不再适用**：那套机制已删除，且旧
证据成立的原因完全不同（无状态 edge 没有本地库）。历史记录保留在计划与 ADR 里。

### 6.3 成员变更 SOP（**先读 `HYDRA_CLUSTER_ID` 那条陷阱**）

> ⚠ **陷阱（实测 2026-10-05）**：`HYDRA_CLUSTER_ID` 的默认值是**成员表内容的哈希**。于是
> **只要改动成员表，默认身份就变了**，而每个节点自己的数据目录记录着旧身份 ⇒ 启动时直接失败：
>
> ```
> fatal startup error error=arachne control plane refused to start: Arachne::start: unrecoverable:
> facade init: cannot open WAL: unrecoverable storage error:
> META cluster_id mismatch: expected "hydra-0718fec1d91bd94b", got "hydra-47b808fb0733c41f"
> ```
>
> （复现方式：拿一个被认领过的数据目录，把成员表从 3 项改成 4 项，不设 `HYDRA_CLUSTER_ID`。
> 这是库自己的身份校验，**不是** Hydra 的 preflight。）
>
> **规避方式只有一个：从第一天起显式设 `HYDRA_CLUSTER_ID`**（一个人类可读的稳定名字）。之后改成员
> 表就不再改身份。**已经上线的集群如果当初没设**，要做成员变更就得先按新表算出默认哈希、把它显式写
> 进配置——否则三台会同时拒绝启动。

变更步骤（**本仓没有把这条流程包成命令**，必须人工执行）：

1. 改**所有**成员上的 `HYDRA_CLUSTER_PEERS`（含顺序）并重启——顺序即 raft id，改顺序等于换集群；
2. 新成员第一次起来时需要多数派在 10 s 内一起可用（§5.6），否则它只是"认领不到目录"；
3. 用 raft 的成员变更把新节点加入并追平（`add_learner` → 等追平 → `promote`），**移除成员同理**；
   集群在变更期间**不能失去多数派**，所以每次只动一台；
4. 变更后确认三台里**恰好一台**答 `/healthz/leader` 200，且各节点物化哈希 == `head`。

---

## 7. 可观测性与告警

控制面的信号（`ops.md` §9.1 有对应的告警表达式）：

| 指标 | 含义 |
|---|---|
| `hydra_arachne_this_node_leader{node}` | 1 = 本节点是 writer。**`sum() == 0` 就是"集群没有写者"**（该序列只在集群节点上存在，单节点部署不会误报） |
| `hydra_arachne_leader_flips_total` | 本节点"我是不是 leader"的答案变了几次（采样 gauge 看不到抖动） |
| `hydra_arachne_publish_total{result}` | `ok` / `not_leader` / `quorum_unavailable` / `error` / `refused`（编码期拒绝：超 1 MiB、无法成键、密封失败 ⇒ **容量告警**） |
| `hydra_arachne_config_bytes` | 最近一次成功发布的树字节数（增长曲线） |
| `hydra_arachne_quorum_unavailable_total{op}` | 因无多数派而被拒的操作（`publish` / `read`） |
| `hydra_replica_materialize_retries_total{outcome}` | 物化循环：`attempt` / `succeeded` / `failed` / `throttled`（稳态不计） |
| `hydra_control_poll_total{result="rate_limit_error"}` | 限流因 Redis 不可达而 fail-open（**数据面**信号） |

数据面本身的指标（请求、令牌、熔断、限流、失效流）见 `ops.md` §9。

---

## 8. 安全说明

- **明文永不跨节点**：provider key 与证书私钥在配置树里都是**密封**的（AES-256-GCM，密钥来自
  `HYDRA_ENCRYPTION_KEY`，全集群一致）。密封是**确定性**的（nonce 由主密钥 + 域分隔 + 密钥版本 +
  明文派生），所以同一份逻辑配置在任何节点发布都得到同一棵树名——这是内容寻址"没变就不重写"成立
  的前提，代价（同一树内相同明文 ⇒ 相同密文、不是 SIV）写在 `crypto.rs` 里。
- **管理面永不返回明文私钥**（单节点语义延续）。
- **每个节点都有自己的管理口与 admin token**：不再有"只有 leader 需要 token"这回事；每个节点的管理口
  都应像以前一样只绑内网/回环。
- **只有 `HYDRA_ADMIN_TOKEN` 一个 token**（§2）：`HYDRA_CLUSTER_TOKEN` 守的是 `/api/v1/internal/*`，
  而那一族已无路由；它在 2026-10-05 随启动要求一起删除，因为让每个部署去生成、分发、轮换一个
  **什么都不守的秘密**比删掉那个契约更贵。
- **raft 传输（tonic）与下游租户 TLS 是两件事**：前者是集群内部通道，后者是租户 SNI 证书，开关与
  文档分开。raft 端口必须只对内网暴露。
- **Redis 建议开启 ACL + 内网隔离**；生产用托管多 AZ。

---

## 9. 已知限制 / 未闭合项（如实记账）

- **成员变更是人工流程**（§6.3），没有命令封装；且**默认身份会随成员表变化**（那条陷阱）。
- **配置树的旧分片没有 GC**：内容寻址意味着每次发布都留下上一版实体键，回收（只回收没有任何存活
  toc 引用的键）**尚未实现**，目前只有一条 `format` 版本号。
- **2 节点部署被拒绝**（不是告警）：`MINIMUM_MEMBERS = 3`。
- **每节点都需要持久卷**，且**配置物化在 N 个节点各跑一遍**（版本闸门 + 分片复用压低成本，仍是 N 倍）。
- **租户写限流是每节点进程内窗口** ⇒ 集群总量上界 = N × 单窗口（沿用"反 DoS 而非计费"定位）。
- **`HYDRA_ARACHNE_LISTEN` 的端口与 `HYDRA_ADMIN_ADDR` 无关**（旧文档要求两者相同，那条约定随
  leader 提示的转发用途一起作废）。
- **失去多数派期间管理写全不可用**（fail-closed）：这是设计，不是缺陷，但意味着"多数派恢复"必须是
  运维的第一优先级。
- 旧文档曾要求 `HYDRA_FAILOVER_GRACE_MS` / `HYDRA_RATE_LIMIT_FAIL_MODE`：前者**从未接线**、后者
  **代码里根本不存在**（限流 fail-open 是硬编码）。

---

## 10. 退役对照（历史，便于查阅旧文档与旧工单）

| 旧机制 | 替代者 | 决策 |
|---|---|---|
| Redis 租约选举（`cluster/lease.rs`） | raft 领导权（写探测） | D-1 / D-3 |
| 节点注册表 + 心跳 + 回收（`cluster/registry.rs`） | 静态成员表；对端存活**不可知**（UI 显示"未知"） | D-2 |
| 配置快照 HTTP 推送 / 轮询（`cluster/control_client.rs`、`snapshot.rs` 的线上路径） | 每节点自己从 Arachne 物化 | D-4 |
| 管理写转发 `cluster/forward.rs` + `x-hydra-forwarded` 护栏 | 库内转发 + 入口节点就地执行 | D-3 / D-6 |
| 租户写内部端点 `/api/v1/internal/tenant-config/*` | 入口节点自己跑写核心 | D-6（乙-full） |
| `edge` 角色与"无状态数据面" | 节点同构；**扩缩容 = raft 成员变更** | D-2 / D-5 |
| `LeaderElection` / `NodeRegistry` / `LEASE_KEY` 等 | 全部删除（计划的退役 grep 归零） | T4.1 |
