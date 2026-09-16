# Hydra 开发计划（波次总览）

> 本文件是 Hydra 实现阶段的总纲，定义**开发纪律**、**架构分层**、**波次划分**与**出入准则**。
>
> 配套详档：`dev-docs/waves/wave-{1..6}-*.md`。设计依据：`dev-docs/design.md`。

---

## 0. 阅读顺序

1. 本文件（纪律 + 分层 + 波次地图）
2. `dev-docs/design.md`（系统设计，权威）
3. `dev-docs/waves/wave-N-*.md`（当前波次的具体 TDD 任务）

---

## 1. 不可动摇的三条铁律

### 铁律 1：TDD 优先

- **先写测试，后写实现**。任何生产代码都必须由一个先失败的测试驱动。
- 每个功能单元的交付顺序：红灯测试 → 最小实现 → 绿灯 → 重构。
- 测试代码豁免：允许 `unwrap`/`expect`/宽松断言（AGENTS.md）。
- **覆盖率门槛**：`hydra-core` 纯逻辑行覆盖率 ≥ 90%；`hydra-server` IO 外壳 ≥ 60%（集成测试为主）。

### 铁律 2：本体程序逻辑零 Mock / 零桩

- **生产代码（本体）中禁止任何形式的 mock、stub、占位实现、`#[cfg(test)]` 分支**。每一行业务逻辑都是真实实现。
- **内部逻辑必须重构为纯函数**（无 I/O、无全局可变状态、确定输入→确定输出），直接以真实输入/输出做单元测试，**不 mock 任何东西**。
- **唯一允许 mock 的地方 = 真实的外部系统边界**，且**仅在测试中**：
  - LLM provider 上游（真实第三方 HTTP）；
  - 租户 `auth_url` 认证服务（真实第三方 HTTP）；
  - ClickHouse（真实第三方数据库）。
- 即使是边界，测试也优先用**进程级真实 double**，而非进程内 mock：
  - HTTP 边界 → `wiremock` 起一个**真实 HTTP server** 返回预设响应（这是网络层 double，不是 mock 内部逻辑）；
  - SQLite → 用 `:memory:` **真实 SQLite 引擎**（sqlx 原生支持），绝不 mock SQL。
- **判定口诀**：「我 mock 的是别人的服务」✅ 允许；「我 mock 的是自己的函数」❌ 禁止。

#### 铁律 2 补充（2026-09 起生效）：Redis 必须连真实实例，**禁止进程内 mock Redis**

Redis 是外部系统边界（租约、集群限流、熔断投票、鉴权 L2、失效事件流都跑在它上面），而本机与 CI 都能提供**真实 Redis**，因此 `crates/hydra-server/src/redis/mock.rs` 这个 in-process double 属于历史遗留，**不得再作为新测试的依托**：

- **新写的 Redis 相关测试一律连真实 Redis**，端点由环境变量 `HYDRA_TEST_REDIS_URL` 指定（如 `redis://127.0.0.1:6379`）。
- **未设置该变量时必须明确失败**，并在信息里给出启动指引（本机 compose / CI service）；**不得**静默回退到 mock，也不得静默跳过——否则"用真实实例"这条规则会悄悄失效。
- **`MockRedis` 已全量迁移并删除**（2026-09）：`redis/mock.rs`（673 行）与 `redis/mod.rs` 的 `pub mod mock;` 都已移除，`fred` 的 `mocks` feature 也一并摘掉。注意它此前**没有** `#[cfg(test)]` 门控，即那 673 行测试替身是被编进生产二进制的。
- **本机端点（已核实）**：Docker 容器 `hydra-local-redis`（`redis:7-alpine`，属三节点本地环境）在 docker 网络 `environment_default` 内为 `172.22.0.4:6379`，**宿主机可达**；但 `environment/docker-compose.local.yml` 与 `docker-compose.cluster.yml` 只声明了容器内地址 `redis://redis:6379`，**未把 6379 发布到宿主**，所以宿主上跑 `cargo test` 目前拿不到稳定端点。要落地本规则，需要：① 在 compose 的 `redis` 服务上加 `127.0.0.1:6379:6379`（一行）；② CI 侧给作业加 `redis:7-alpine` service 容器（Actions 的 service 会映射到 localhost）并设置 `HYDRA_TEST_REDIS_URL`；③ 本地 `export HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6379`。
- **为什么这件事要紧（不是洁癖）**：in-process double 会掩盖真实 Redis 的语义与失败模式，直接导致测试假绿。已知三例：`MockRedis` 的 `INCR` 永不失败（于是"trim 成功但 bump 失败"这条路径**无法被测到**）、没有 `XLEN`、`XTRIM` 是自己重写的（不校验 MAXLEN/MINID 语义）；另有 `SET ... NX PX 0` 在真 Redis 上会报错、在 double 上却成功。这些差异正是集群侧最重要的失败路径。
- **对当前任务的影响**：`B3(a)`（裁剪与代际自增的原子性）**必须用真实 Redis 验证**——它需要的正是真实 `INCR` 失败/命令时序与 `XLEN`，因此不再给 mock 加"故障注入接缝"。

### 并发约定（2026-09-16 起生效）：`DashMap` 守卫不得跨越 `.await`

来源：`dev-docs/bug-2026-09-16-auth-cache-guard-deadlock.md` —— 生产上一个边缘副本的**唯一** Pingora worker 线程被自死锁挂死，8080 数据面完全停 accept，而 8081 的 `/healthz` 仍 200，k8s 因此继续把流量路由给它（约一半请求静默超时，且**不会自愈**）。

- **根因形状**：`DashMap::get()` 返回的 `Ref` 是**分片读锁**，它是有 `Drop` 的类型 ⇒ **活到作用域结束，而不是最后一次使用**。带着它 `.await`，再对**同一个 key**（⇒ 同一分片）`insert()`，就等于"自己持读锁、又等自己放读锁" ⇒ 永久自死锁。
- **为什么 lint 帮不上忙**：`clippy::await_holding_lock` 只覆盖 std / parking_lot 的锁，**覆盖不到 DashMap 的 `Ref`/`RefMut`**。
- **规则**：
  1. 任何 `DashMap` 的 `get` / `entry` 结果**必须在同一个同步函数或同一个语句内释放**，绝不允许越过 `.await`；
  2. 需要"读一下再决定是否走异步路径"时，把这个读抽成一个**同步**函数（返回 `Copy` 的判定值），例如 `AuthCache::l1_decision(&key) -> Verdict`——守卫在它返回前就已释放，从 **API 形状上**杜绝守卫逃逸；
  3. 新增/审查任何 async 函数时，先看它是否持有 `DashMap` 守卫、`std::sync::MutexGuard` 或任何带 `Drop` 的锁值再 await。
- **回归测试的形状（重要）**：这类缺陷"永不返回"，因此**不能用 `tokio::time::timeout` 包住被测调用**——阻塞发生在**同一次 poll 内**的同步 futex 等待里，计时器即使触发也没人再 poll 这个任务。正确做法：把被测调用放到**独立线程 + 独立 runtime**，由测试线程用**通道超时**等待；并 `std::mem::forget(rt)` 以免 drop 时等待被挂死的 worker（见 `redis::auth_cache::tests::an_expired_l1_entry_does_not_deadlock_on_l2_backfill`）。
- **触发条件的隐蔽性**：`DashMap::get` 在 key **不存在**时会立刻释放守卫，所以"L1 冷"（条目不存在）不会触发；真正的触发是"**条目存在但已过期**"（`cache_decision` 判 Miss，却仍持有分片读锁）**且 L2 命中**——这也是它为什么不是一上线就炸。写这类回归测试时必须构造"present-but-expired"，只清空缓存是不够的。

### 监听拓扑与启动约定（2026-09-16 起生效）

来源：`dev-docs/bug-2026-09-16-tenant-cert-flips-listener-to-tls.md` —— 一个**纯配置动作**（给租户配证书）把唯一的下游监听器从明文静默切成 TLS，于是任何一次重启（升级/驱逐/崩溃恢复）都让该副本的明文入口 100% RST：接口全挂、进程健康、`/healthz` `/readyz` 全绿。

- **规则 1：监听拓扑的唯一来源是部署配置。** "哪个端口用什么协议听"只能由环境变量/配置文件决定；**快照、DB 行、业务数据一律不得影响传输协议**。判断口诀：如果"改一条业务数据 + 重启"能改变某个端口的协议，就是本缺陷。
- **规则 2：「绑定成功」必须有可验证的证据。** 自己打印的 `listener bound` 日志**不算证据**：`main.rs` 的日志发生在 Pingora 真正 `bind()` 之前，而 `Listeners::build()` 是"整个 service 一起成败"——端口被占用时 Pingora 会重试 30 次（每秒一条 `WARN … is in use`），然后在其 service 任务里 `panic!("Failed to build listeners")`，**进程继续存活、admin 端口照常应答、代理端口没有任何监听**（2026-09-16 实测）。因此：启动后必须做一次真实的连接自检并暴露指标；绑定失败**不得**以"进程活着 + 探针绿"收场。
- **规则 3：证书存储的重解析必须挂在快照交换的唯一边界上。** `HydraCertStore` 的内容要跟随 `ConfigStore` 的每一次快照替换（`reload_all` / `apply_snapshot`），**禁止在每个写入点分别接线**——edge 节点就是这么漏掉的：控制面下发的新证书进了快照，但 TLS 回调永远看不到，必须重启（`main.rs` 里那句 `on_poll: None, // edge TLS cert re-resolution lands with the edge TLS wiring` 就是这个漏点的化石）。
- **规则 4：多监听的服务边界。** 一个 Pingora `Service` 可以挂多个监听（`Listeners{stacks}`），但**绑定失败是全 service 级的**：把可用性关键端口（明文入口）和可选端口（HTTPS）放在同一个 service 里，等于让可选端口的配置错误连坐关键端口。落地时要么分 service，要么在 Pingora 启动前预检绑定。

> 设计文档中出现的 `MockAuthChecker` 等字样，在实现阶段一律替换为：纯缓存判定逻辑直接测（无需 mock）+ `HttpAuthChecker` 用 wiremock 测。trait 仍保留用于「生产配置 vs 测试配置」的装配，但测试用真实 double。

### 铁律 3：终止模式（Terminate-in-Pingora）

- **不再做零拷贝 stream-through**（已废弃）：当前架构在 `request_filter` 内终止请求，读取完整请求体后用自有 HTTP client (reqwest) 调用供应商，SSE 响应经 Pingora session 流式回写，返回 `Ok(true)`。
- 请求体**完整读取**（`read_request_body()` 循环到 EOS），model 提取适用于**任意位置/schema**（不再受首 chunk 限制）。body 字节**原样传给 reqwest**（不做 JSON encode/decode）。
- 故障转移是**简单 for 循环**（`Bytes::clone` O(1) 重放）；不再依赖 Pingora 的 `enable_retry_buffering` / `set_retry` / `fail_to_connect` / `error_while_proxy`。
- 少量元数据（`"model"`、`"usage"`）仍一律用 **`memchr` SIMD 字节扫描**提取（零分配、早退），命中处仅反序列化该小切片。
- 诚实边界：放弃 kernel-level 零拷贝（body 经过 userspace buffer），但保留"零 JSON 往返"的核心语义（body 字节未被 serde 处理）。
- 详见 [`dev-docs/design-change-terminate-mode.md`](design-change-terminate-mode.md)（原 §6 零拷贝原则、§6.3、§6.6、§8.5、§9.4 描述的是已废弃的 stream-through 架构）。

---

## 2. 架构分层：从构建层面强制铁律 2

采用 **Cargo workspace 双 crate**，让编译器替我们守住边界：

```
hydra/
├── Cargo.toml                 # [workspace]
├── crates/
│   ├── hydra-core/            # 纯领域核心：零 I/O、零 mock、可穷尽单测
│   │   ├── Cargo.toml         # 不依赖 pingora/tokio/sqlx/reqwest
│   │   └── src/
│   │       ├── model/         # 实体（纯数据结构）
│   │       ├── router/        # resolve / swrr（纯）
│   │       ├── breaker/       # 熔断状态机（纯：输入事件 → 输出 dead-set）
│   │   ├── sse/           # usage 零拷贝 memchr 扫描（纯：&[u8] → Option<Usage>）
│   │   ├── extract/       # 零拷贝元数据提取（纯：memchr 扫描 model，返回 &[u8] 借用）
│   │   ├── limit/         # 匹配 + 滑动窗口（纯）
│   │       ├── auth/          # 缓存命中/过期判定（纯：cache + 时刻 → verdict）
│   │       ├── rewrite/       # /v1 重写、key 掩码（纯）
│   │       └── config/        # ConfigData 内存模型 + 加载期校验（纯）
│   └── hydra-server/          # I/O 外壳：Pingora/sqlx/reqwest，薄适配层
│       ├── Cargo.toml         # 依赖 hydra-core + pingora + sqlx + reqwest
│       └── src/
│           ├── proxy/         # ProxyHttp impl（把纯函数接到 Pingora 生命周期）
│           ├── store/         # sqlx 仓储 + ArcSwap 装配 + ConfigStore
│           ├── http/          # HttpAuthChecker(reqwest)、ServeHttp admin
│           ├── sink/          # SqliteSink / ClickHouseSink
│           ├── tls/           # certificate_callback
│           └── main.rs
├── migrations/
├── admin-ui/
└── dev-docs/
```

**强制点**：`hydra-core` 的 `Cargo.toml` **不得**出现 `tokio`/`pingora`/`sqlx`/`reqwest`/`hyper` 任何 I/O 依赖（`memchr`/`bytes`/`sha2` 为纯计算/引用计数/密码库，**允许且必需**，用于零拷贝提取与 api-key 哈希）。这样：

- 所有「内部逻辑」物理上无法做 I/O，只能纯；
- 纯逻辑 100% 可在无网络、无文件、无运行时下穷尽测试；
- `hydra-server` 只负责「搬数据」：把 Pingora session / sqlx row / reqwest response 翻译成 core 的纯输入，调用 core，再把 core 输出翻译回 I/O 动作。

> 这是「内部逻辑禁 Mock」最彻底的实现方式——边界由 crate 依赖图焊死。

---

## 3. 波次总览

把 `design.md` 的 Phase 0–9 **按 TDD 友好度重排为 6 个波次**：先纯核心（Wave 1，无 I/O 即可全测），再向外逐层装配 I/O 外壳。

| 波次 | 名称 | crate | 关键产出 | 依赖 | design.md Phase | 估时 |
| --- | --- | --- | --- | --- | --- | --- |
| **W1** | 纯领域核心 | `hydra-core` | 路由/SWRR/熔断/解析/限流/认证判定/重写，全纯 + 穷尽单测 | — | P0(骨架)+P3(路由纯部)+P4(熔断纯部)+P5(解析纯部)+P6(限流纯部)+P2(缓存纯部) | 4d |
| **W2** | 持久化与配置加载 | both | sqlx schema/migrate、仓储、`ConfigStore` 加载+校验+ArcSwap | W1 | P1 | 1.5d |
| **W3** | 外部边界适配器 | `hydra-server` | `AuthChecker`(reqwest)、`UsageSink`(sqlite/clickhouse) trait+实现，wiremock 测 | W1,W2 | P2(回源)+P5(sink) | 2d |
| **W4** | Pingora 代理外壳 | `hydra-server` | **初版 stream-through，后重写为 terminate-mode**（proxy.rs 855 行重写 + 新增 provider_client.rs 237 行）：`ProxyHttp` `request_filter` 终止模式全生命周期、自有 reqwest client 调供应商、SSE 流式回写、简单 for 循环故障转移、TLS 证书回调 | W1,W2,W3 | P3(代理部)+P4(故障转移)+P8(TLS)+大body | 3d |
| **W5** | 管理服务与可观测性 | `hydra-server` | AdminService(REST)、自托管 metrics、热更新、认证失效/熔断复位 | W1–W4 | P7 | 2d |
| **W6** | UI、TLS 与加固 | both | 内嵌 UI、多租户 TLS 端到端、Playwright E2E、压测、ops 文档 | W1–W5 | P9 | 2d |

**合计 ≈ 14.5 人日**（已与 design.md §18 对齐为 14.5 人日；波次化把 Phase 重排、骨架并入 W1）。

### 依赖图

```
            W1 (pure core)
           ╱            ╲
          ╱              ╲
   W2 (pool/store)   W3-auth (AuthCache/HttpAuthChecker)
          │           ╱
          │          ╱
          └──→ W3-sink (SqliteSink/ClickHouseSink)   ← 需 W2 的 SqlitePool
                  │
                  ▼
                 W4
                  │
                  W5
                  │
                  W6
```

- **W1 无任何依赖**，最先启动，是后续一切的地基；
- **W2 与 W3-auth 可在 W1 完成后并行**（W2 写 sqlx 仓储/加载/ConfigStore；W3-auth 写 AuthCache + HttpAuthChecker；写不同模块，无冲突）；
- **W3-sink 依赖 W2 的 `SqlitePool`**（design §9.2），故在 W2 交付 pool 后才能编译；W3-auth 不依赖 W2，可与 W2 完全并行；
- **W4 串行**（需要 W1 core + W2 store + W3 边界齐全）；
- **W5 → W6 串行**。

---

## 4. 全局工程实践

- **分支**：每波次一个 `wave/N-xxx` 长分支，内部按 TDD 任务短分支提交；波次完成合主。
- **提交**：测试先行提交可见历史（`test: ...` → `feat: ...`），便于审查 TDD 节奏。
- **CI**（每 PR）：`cargo fmt --check`、`cargo clippy -- -D warnings`、`cargo test -p hydra-core`、`cargo test -p hydra-server`、`cargo deny check`（依赖审计）、覆盖率报告。
- **编译验证**：任何改动后必须 `cargo build --all` + 相关测试通过（AGENTS.md）。
- **Lock 文件**：`Cargo.lock` 必须提交。
- **规约**：遵守 `~/.config/opencode/AGENTS.md` 全部红线（禁 `unwrap` 于生产、禁裸 `except`、禁 `var`/`==`、`set -euo pipefail` 等）。

---

## 5. 每波次统一模板（详档结构）

每个 `dev-docs/waves/wave-N-*.md` 必含：

1. **目标与范围**（in-scope / out-of-scope）
2. **依赖与前置**（上一波次的产出契约）
3. **纯函数清单**（W1 重；后续波次列出装配点）
4. **TDD 任务列表**（编号、测试名 → 行为 → 实现点，按此顺序红→绿）
5. **外部边界与测试方式**（明确哪里用 wiremock / in-memory SQLite）
6. **与 design.md 的映射**（章节引用）
7. **出口准则**（可观测、可验证的完成条件）
8. **风险与注意**

---

## 6. 出口准则（全局，每波次叠加）

所有波次共同满足：

- [ ] `cargo build --all` 通过；`hydra-core` 无任何 I/O 依赖（CI 用 `cargo tree` 校验）；
- [ ] `cargo clippy -- -D warnings`、`cargo fmt --check` 通过；
- [ ] 本波次 TDD 任务全绿；
- [ ] 生产代码零 mock/零桩/零 `#[cfg(test)]` 分支（code review + grep 校验）；
- [ ] 热路径零 JSON 反复编解码：body 原样传给 reqwest（terminate-mode），`"model"`/`"usage"` 用 `memchr` 提取，故障转移重放用 `Bytes::clone`（O(1)）（grep 校验：无 `serde_json::from_slice` 作用于完整 body；生产代码无 `enable_retry_buffering`——已删除）；
- [ ] 凡外部边界，测试用真实进程级 double，并在 PR 注明。

每波次额外出口准则见各自详档。

---

## 7. 风险与缓解

| 风险 | 缓解 |
| --- | --- |
| ~~Pingora 0.8.1 body 转发机制（`read_body_bytes` 消费首 chunk 后需 `enable_retry_buffering` 回放）~~ | ✅ **已废弃**：terminate-mode 在 `request_filter` 内读全 body（`read_request_body()` 循环）后用自有 reqwest client 调供应商，不再依赖 Pingora 的首 chunk 回放 / retry buffer / `Vec<Bytes>` 累加器。详见 `dev-docs/design-change-terminate-mode.md`。 |
| 纯/外壳切分导致类型在 crate 间频繁搬运 | core 拥有领域类型；外壳只做 `Into/From` 转换，约定边界转换集中在 `bridge` 模块 |
| wiremock 与 Pingora 上游集成测试复杂 | 外壳集成测试用独立 mock upstream server（真实 HTTP），不通过 Pingora mock 内部路由 |
| 覆盖率门槛卡进度 | 仅对 `hydra-core` 设硬门槛；外壳以集成测试覆盖关键路径 |

---

下一步：进入 **Wave 1**（`dev-docs/waves/wave-1-pure-core.md`）。
