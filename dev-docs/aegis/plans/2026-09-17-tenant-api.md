# 实施计划：租户自助 API（Tenant API）——数据面 `/tenant/{tenant_id}/api`

- **日期**：2026-09-17
- **状态**：**待 oracle 架构复核**（复核通过后启动开发）
- **作者**：编码智能体（DeepSeek）｜深度复核：Architecture review (@oracle)
- **对应需求**：admin API 与 tenant API 混在同一端口/路由树/命名空间；需要**专门的 tenant API**，以**租户访问令牌**为唯一鉴权，挂在**数据面**、以 `/tenant/{tenant_id}/api` 为 base URL，收敛租户对接所需全部接口。首发能力：强制清除一个或多个 api-key 的认证缓存（**必须在全部数据面节点生效**）、查询某时间戳之后的 token 用量（**单节点与集群都可用**）、只读自述。
- **前置设计**：`dev-docs/design-tenant-api.md` **v5.1**（本文所有"设计 §x"均指该文档）

---

## Goal

1. 数据面新增保留前缀 `/tenant/{tenant_id}/api/v1/*`，与既有业务路径（`/v1/*`）、目录拦截（`GET /v1/models`）互不干扰；
2. 三个端点：`GET /whoami`（纯快照、零 DB）、`POST /auth/cache/invalidate`（**全集群清除并可确认收敛**）、`GET /usage`（SQLite + ClickHouse 双后端）；
3. 令牌校验**不再读 DB**（改读集群快照里已有的 `FidelityRows::tenant_token_hashes`），从而 leader/standby/edge 三种角色都能本地鉴权；
4. **零转发**：不引入 `leader_ready`/`cluster_registry`/`forward_mutation`，不新增信任边界；
5. 删除 migration 0009 在管理口的租户路由（尚未上线），管理面回到单一凭证语义；
6. 全程门禁：`fmt` / `clippy -D warnings` / `hydra-core` / `--features server` / 三特性矩阵 / Playwright。

**非目标**（设计 §2.2）：域名与 `auth_url` 自助设置（需求已移除）、按前缀清缓存、用量明细、租户自助轮换令牌、计费语义。

## 架构 / Architecture

```
数据面监听器（HYDRA_LISTEN / HYDRA_TLS_LISTEN，同一 Pingora Service）
  request_filter 第 0 步：命中 /tenant/{tid}/api/v1/* → tenant_api::dispatch → Ok(true) 短路
       │
       ├─ 令牌闸门 tenant_from_token(&ConfigStore, bearer)
       │     └─ 读 store.replication().fidelity().tenant_token_hashes（零 DB I/O）
       │        与 tenants_by_id 同一次快照读（禁止分两次读）
       │
       ├─ E1 whoami   → ConfigData（快照）
       ├─ E2 invalidate → AuthCache::invalidate[_tenant]（本节点 L1+L2）
       │                  + InvalidationStream::publish
       │                  + await_applied(event_id, live_node_ids, timeout)   ← L3 收敛屏障
       └─ E3 usage    → Arc<dyn UsageQuery>（启动时按 sink kind 注入唯一实现）
                          ├─ SqliteUsageQuery     → store.pool()
                          └─ ClickHouseUsageQuery → clickhouse::send()（与写路径同源）
```

## 技术栈 / Tech Stack

Rust 1.98（`rust-toolchain.toml`）；Pingora 0.8（`ServeHttp` / `ProxyHttp`）；`sqlx` 0.8（SQLite，离线缓存 `SQLX_OFFLINE=true`）；`fred` 10（Redis，`cluster-redis`）；`chrono` 0.4（**仅 `hydra-server`**）；`serde_json`；`prometheus`；wiremock（测试）；Playwright（前端）。

## Baseline / Authority Refs

| 类型 | 引用 |
|---|---|
| 前置设计（本计划的权威输入） | `dev-docs/design-tenant-api.md` v5.1（含 §6.4 决策记录 A-1、附录 A 认证缓存事实基线） |
| 项目铁律 | `dev-docs/dev-plan.md:17-81`（铁律 1 TDD / 铁律 2 零 mock / 铁律 3 terminate-in-Pingora） |
| 主设计 | `dev-docs/design.md` §6 / §11 / §13 / §17 |
| 集群 | `dev-docs/cluster.md` §1 角色、§2 环境变量、§3 共享状态、§4.2 拓扑 |
| 运维 | `dev-docs/ops.md` §5 认证缓存失效、§9 可观测性、§13 集群运维 |
| CI 门禁 | `.github/workflows/ci.yml`（`RUSTFLAGS=-D warnings`、`SQLX_OFFLINE=true`、四个 job） |
| sqlx 离线缓存流程 | `dev-docs/HANDOFF.md:167-190`（三条已记录的坑） |
| 既有计划格式先例 | `dev-docs/aegis/plans/2026-08-27-tenant-access-token.md`、`.../2026-09-16-changelog-gap-remediation.md` |

```text
BaselineUsageDraft:
- Required baseline refs: design-tenant-api.md v5.1；dev-plan.md 铁律；cluster.md §1-4；ops.md §5/§9；ci.yml
- Delivered context refs: 本次会话已直读 proxy.rs / admin/mod.rs / admin/handlers.rs / http.rs /
  redis/auth_cache.rs / cluster/events.rs / cluster/content.rs / cluster/snapshot.rs / store.rs /
  sink.rs / listeners.rs / main.rs / migrations 0001-0009 / 五个测试套件头部 / 活 ClickHouse 实测
- Acknowledged before plan refs: 全部 required refs 已在写作前直读
- Cited in plan refs: 见各任务 Verification 与文件改动清单
- Missing refs: 无
- Decision: continue
```

### ADR / 决策信号携带（完成期需交付给 ADR Backfill）

本工作流有一条**已决**的架构决策，必须在完成时原样传递给 ADR/基线回填，而不是届时机重新发现：

| 决策 | 状态 | 来源 | 备选（真实存在过的） | 兼容边界 | 完成期需回答的基线同步问题 |
|---|---|---|---|---|---|
| **A-1：E2 不转发到 leader 的 admin API** | 设计期已决（未执行） | 设计 §6.4（7 条否决理由 + 4 条重新评估触发条件） | ① 转发到 leader 的 admin API（技术可行，被否）；② leader 主动逐节点推送（被否）；③ 直连本节点 + 三层保证（采纳） | 数据面不得获得任何转发能力（`AppState` 不含 registry/leader_ready） | 实现落地后：架构基线里"租户控制面不需要权威节点"这一条是否已成立？失效流的 owner 是否已包含收敛语义（而非另起一套）？ |

其余决策（Q3 CH 读进 v1、Q4 旧路径 M1 删除、Q12 TTL 上限、Q13 单集群、Q14 CH 传输下沉）均为**设计期已答的需求/工程决策**，已在设计 §11.1 存档；若实现期产生新的持久架构决策，按同样格式追加到本表。

## 需求就绪检查 / Requirement Ready Check

```text
Requirement Ready Check:
- Requirement source refs: 用户 2026-09-17 四轮需求陈述（新增 tenant API / 移除域名与 auth_url 自助设置 /
  E2 必须清到全部数据面节点 / 生产必跑集群且用量首发可用）；用户对 Q3/Q4/Q12/Q13/Q14 的答复
- Goals and scope refs: design-tenant-api.md §2.1（R1-R6）、§2.2（非目标）、§2.3（需求变更记录）
- User / scenario refs: 租户对接方（欠费停机/付费恢复、对账、按 key 封禁）；运维（代改域名/auth_url）
- Requirement item refs: R1 专用入口+令牌 / R2 数据面可达 / R3 全集群清除 / R4 两形态可用用量 /
  R5 只读自述 / R6 文档收敛
- Acceptance / verification criteria refs: 设计 §10.1-10.5（单元 / 数据面集成 T1-T23 / 集群 C1-C18 /
  门禁命令 / 验收证据落档）
- Open blocker questions: 无（Q1/Q2/Q5-Q11 均有建议值且非阻塞，见 §待确认）
- Decision: ready
```

## 变更必要性 / Change Necessity

```text
Change Necessity:
- User-visible need: 租户需要一个只用自己令牌、走数据面、能在全部数据面节点上真正清掉指定 key 缓存的入口；
  集群部署下必须能查用量。今天这三点都不成立：租户端点住在管理口（默认回环，租户够不着）、
  清缓存只同步清本节点（远端靠异步事件、且无任何收敛证明）、CH 侧没有读路径。
- No-change / non-code option: 不可行。文档/配置改动无法改变"端点挂在哪个监听器""扇出是否可确认"
  "CH 能否被查询"这三件事；这三者都是代码结构问题。
- Why code change is necessary: ① 前缀拦截必须进 request_filter 的第 0 步（晚于 Host→tenant 会 404、
  晚于 api-key 抽取会把租户令牌当客户端 key 送去 auth_url）；② 收敛屏障必须由消费者发布水位
  （今天 last_id 只存在局部变量里，events.rs:291）；③ CH 读通道今天完全不存在。
- Minimum change boundary: 新模块 tenant_api/（3 文件）+ clickhouse.rs（传输下沉，读写成同源）
  + usage_query.rs（双后端读）+ hydra-core 纯函数模块 + ConfigData 派生索引；AppState 只增 2 字段；
  改 reach 点：proxy.rs（1 处拦截）、main.rs（构造后移 + 注入）、cluster/events.rs（水位）、
  http.rs（TTL 上限）、sink.rs（改为调用新传输）、admin/mod.rs（删旧路由）。
- Decision: code-change
```

## 既有面复用检查 / Existence Check

| 拟新增面 | 既有 owner / 复用候选 | 为何既有面不足 | 创建证明（验证信号） | 熵/退役影响 | 决策 |
|---|---|---|---|---|---|
| `tenant_api/` 模块 | `admin/handlers.rs` 的 `tenant_auth_cache_invalidate` + `tenant_from_token` | 它们的闸门依赖 `AdminState.pool`（edge 无 DB），且注册在管理面（租户默认够不着） | §10.2 T1-T23 全绿；§10.3 C12/C17 证 edge 可达 | 旧实现随旧路由**删除**（T9），不留双实现 | **add-with-proof** |
| `clickhouse.rs` | `sink.rs` 内的私有 CH 传输 | 私有的 `ClickHouseConfig` 已被 move 进 flush task 闭包（`sink.rs:559-568`），读侧拿不到；且响应体被当错误文本丢弃（`sink.rs:801-811`） | 搬迁 commit 让既有 4 条 `clickhouse_sink` 测试**零差异通过**（§10.4） | 搬迁而非复制：`sink.rs` 改为**调用**新模块，不保留第二份传输 | **add-with-proof**（T3） |
| `usage_query.rs` | `UsageSink` trait | `UsageSink` 语义是 fire-and-forget 写入通道（`sink.rs:39-63`），且以 `Box<dyn>` 类型抹除；给它加查询方法会改变它的含义与生命周期 | §10.2 T14/T14a-c、§10.3 C13/C13a | 无退役负担（纯新增读面） | **add-with-proof** |
| `ConfigData.tenants_by_id` | `tenants_by_domain` | 键是 domain，无法由 tenant_id O(1) 取行 | `config_data.rs` 形状测试 + `loader.rs` | 不是第二个 source of truth：与 `tenants_by_domain` **同一 loader 同批行**构建（与 `models_by_key` 之于 `provider_models` 同构） | **add-with-proof**（T2） |
| 收敛屏障 | 新建独立 `InvalidationBarrier` | 与失效流共用 `hydra:{ctl:*}` 命名空间与同一个 Redis 池 | §10.3 C4-C8 | 若拆成独立类型，就是第二套"谁负责让全集群失效"的答案 | **reuse-existing**（扩 `InvalidationStream`，T6） |
| `HYDRA_CLUSTER_ID` / `fleet.cluster` | —— | 单一集群下无对象可声明 | —— | —— | **reject**（设计 v5 已删） |

## 架构完整性视角 / Architecture Integrity Lens

| 维度 | 结论 |
|---|---|
| **不变量** | ① 租户令牌只在数据面被接受；② 缓存清除必须同时作用于 L1 与 L2（`http.rs:302-328` 的 B2 教训）；③ 时间边界必须与库内列同格式定宽比较；④ 生产代码零 `unwrap/expect`/零 mock/零 `#[cfg(test)]` 分支 |
| **规范 owner** | 令牌→租户：`ConfigStore` 的快照（唯一）；失效扇出+收敛：`InvalidationStream`（唯一）；"怎么跟 CH 说话"：`clickhouse.rs`（唯一）；用量解析：`hydra-core` 纯函数（唯一）；"本节点有没有本地库"：`ConfigStore::pool()`（唯一，不在 `AppState` 再放一个） |
| **职责重叠检查** | 未发现。`tenant_api` 只做路由/闸门/响应；业务逻辑全部委托既有原语。**特别检查**：E2 不做自己的缓存删除逻辑（调用 `AuthCache`），E3 不做自己的 JSON 解析（调用 `clickhouse.rs` + core 纯函数） |
| **更高层简化** | 已采纳两处：① 需求去掉域名/auth_url 写 → 整条写路径与全部 leader 转发消失（设计 §6.2）；② Q14=T1 → CH 传输下沉为共享模块而非读侧再写一份。**拒绝**的简化：把 `UsageQuery` 塞进 `UsageSink`（改变既有 trait 语义） |
| **退役/可证伪** | T9 删除旧路由整块 + `tenant_id_for_token` + `published` 兼容字段；证伪信号 = §10.3 C16 断言旧路径返回 **404 而非 401**（证明路由消失，不是被闸门挡住） |
| **判定** | proceed |

## 兼容性边界 / Compatibility Boundary

| 面 | 边界 |
|---|---|
| 数据面既有路径 | `POST /v1/chat/completions`、`/v1/messages`、`GET /v1/models`、passthrough 行为**逐字节不变**（§10.2 T22 断言上游 `received_requests()` 计数一致） |
| 新增保留前缀 | `/tenant/` 不再进入代理管线。这是**唯一的行为变更面**（几乎不可能与 LLM provider 路径冲突）；`HYDRA_TENANT_API=off` 可完全回到基线 |
| 管理面 | **破坏性**：删除 `POST /api/v1/tenants/{id}/auth/cache/invalidate`（用户已确认尚未上线）。`DELETE /api/v1/auth/cache` 保留，但响应体由 `published: bool` 换成 `fleet` 对象 |
| 认证判定语义 | 除 `HYDRA_AUTH_ALLOW_TTL_MAX_SECS`（默认 = 现状值）外**不改**；不设 `expires_in` 的租户**零行为变化** |
| 配置 | 新增 4 个 env（`HYDRA_TENANT_API` 等），全部有安全默认；删除 `HYDRA_CLUSTER_ID`（v5，从未存在过） |
| `.sqlx/` | 不改（采用运行时 `sqlx::query` 风格）；若改用 `query!` 宏则必须重生成 |
| 数据库 schema | **零迁移**（不新增列/表；`ConfigData` 是内存派生索引） |

## TDD Route

```text
TDD Route:
- Mode: auto
- Decision: strict
- Strict authority: 显式项目规则 —— dev-docs/dev-plan.md:19-24「铁律 1：TDD 优先 —— 先写测试，后写实现。
  任何生产代码都必须由一个先失败的测试驱动。交付顺序：红灯测试 → 最小实现 → 绿灯 → 重构。」
- Test posture: strict RED test（纯函数与 handler）；**重构类任务（T3/T9 搬迁）以既有测试为回归网**，
  不新增 RED 测试（重构步不改变行为，无法写出会失败的"新行为"测试）
- Reason: 项目铁律显式要求；且本功能的失败模式（假 0 用量、清错节点的缓存、比较错时间格式）
  全都是"静默错误"，必须有先失败的红灯把行为钉住
- Verification: 每个任务给出精确命令与期望输出；红灯步骤要求命令**因断言失败而失败**（而非编译失败）
```

## 计划压力测试 / Plan Pressure Test

```text
Plan Pressure Test:
- Owner / contract / retirement: 三个新 owner 各有唯一性论证（Existence Check）；退役路径明确（T9 删旧路由，
  证伪信号 404-not-401）；无 fallback/适配层残留
- Architecture integrity / higher-level path: 已采纳"删写需求"与"CH 传输下沉"两条更高层简化；拒绝了
  改变 UsageSink 语义的伪简化
- Verification scope: 单元（core 覆盖 ≥90%）+ 数据面集成 23 条 + 集群 18 条 + 门禁命令 + Playwright +
  活 CH 实测；每条失败模式（CH 引号整数、CH 空集空串、时间格式、假 0、未收敛、屏障假报）都有独立负例
- Task executability: 每个任务含精确文件路径、锚点、命令与期望；无 TBD/TODO
- Pressure result: proceed
```

## 计划期复杂度检查 / Plan-Time Complexity Check

| 目标文件 | 现有规模/形状 | owner 契合 | 就地增长风险 | 更优边界 | 建议 |
|---|---|---|---|---|---|
| `proxy.rs` | 1603 行 | 已是代理生命周期 owner | 高（再塞 handler 会失控） | **只加 1 次转发调用**（第 0 步前缀判定 → `tenant_api::dispatch`），全部逻辑在新模块 | edit-in-place（1 处） |
| `admin/handlers.rs` | 2130 行 | 运维 handler owner | 高 | 租户逻辑**搬出**，只留 `pub(crate)` 化的公共小工具 | split task（T6 搬出 + T9 删除） |
| `admin/mod.rs` | 692 行 | 管理面路由器 | 中 | 删除租户路由块（净减） | edit-in-place（净减） |
| `sink.rs` | 1247 行 | usage 写入 owner | 中（CH 传输混在里面） | **把 CH 传输下沉 `clickhouse.rs`**，sink 只留批量/flush/重试策略 | extract helper（T3） |
| `cluster/events.rs` | 约 560 行 | 失效总线 owner | 中 | 水位与等待循环放这里（同命名空间同池） | edit-in-place |
| 新增 `tenant_api/*`、`clickhouse.rs`、`usage_query.rs`、core `tenant_api.rs` | 0 | 新 owner | —— | 按职责拆文件（路由/闸门/handler/限流/时间边界） | add owner file |

```text
Complexity Budget:
- Artifact class: 多模块功能（新 owner 文件 + 4 处 edit-in-place + 2 处净减）
- Target files / artifacts: 见上表
- Current pressure: proxy.rs 1603 / handlers.rs 2130 已偏大
- Projected post-change pressure: proxy.rs +约 10 行；handlers.rs **净减**；sink.rs **净减**；新增模块各自 <400 行
- Budget result: within-budget
- Planned governance: 大文件只做"转发调用"或"净减"；新逻辑一律进新文件
```

## 执行就绪视图 / Execution Readiness View

```text
Execution Readiness View:
- Intent Lock: 交付设计 §2.1 的 R1-R6；不扩到 §2.2 的非目标
- Scope Fence: 只动"文件改动清单"列出的文件；不改 provider/model/限流/熔断/证书/租约/注册表逻辑
- Baseline Lock: 设计 v5.1 + 本文；任何与两者冲突的发现必须回到设计（走 §Drift 规则）
- Approved Behavior:
    E1 快照只读；E2 本节点同步清 + 全集群扇出 + 收敛确认（200/202/503）；
    E3 双后端、时间窗聚合、requests 在 CH 下为近似值
- Owner / Contract Constraints: 令牌→租户只来自快照；失效扇出+收敛只在 InvalidationStream；
    CH 寻址只在 clickhouse.rs；用量解析只在 core 纯函数
- Compatibility Boundary: 数据面既有路径逐字节不变；保留前缀是唯一行为变更面
- Retirement Boundary: T9 删除管理口租户路由 + tenant_id_for_token + published 字段；
    证伪 = 旧路径 404（非 401）
- Task Batches:
    Batch A（无依赖，可并行）: T1 core 纯函数 | T2 ConfigData 索引 | T3 CH 传输下沉
    Batch B（依赖 A）: T4 骨架+拦截+闸门 → T5 E1 → T6 E2+屏障 → T7 TTL 上限
    Batch C（依赖 T3）: T8 usage_query 双后端
    Batch D（依赖 B+C）: T9 删除旧路+文档+UI → T10 门禁+验收记录
- Test Obligations: 每个 TDD 任务先红后绿；T14a/T14b/T14c 三条"静默 0"负例为阻塞项
- Review Gates: ① 计划 oracle 复审；② Batch A 后一次自审；③ T8 后一次自审；
    ④ 全部完成后对抗式交叉复审（实现 vs 设计一致性 + 完整性）
- Drift / Rewind Rules: 发现"设计与代码事实不符"→ 停下，先改设计并记修订行，再继续；
    发现新 owner/新 fallback → 回 Existence Check；门禁失败 → 修到绿，不降级断言
- Evidence Required Before Completion: 全部门禁命令输出 + 新增测试计数 +
    活 CH 实测记录 + Playwright 结果 + 计划文档 ## 实施记录 回填
- Advisory Boundary: method-pack execution guidance only; not GateDecision, PolicySnapshot,
    or completion authority
```

---

## 文件改动清单

### 新增

| 文件 | 内容 |
|---|---|
| `crates/hydra-core/src/tenant_api.rs` | 纯函数：规范时间戳字形+数值范围校验、规范形态顺序比较、路径解析、CH 宽松整数解析、`as_of` 归一化、用量 DTO |
| `crates/hydra-core/tests/tenant_api.rs` | 上述纯函数穷举单测 |
| `crates/hydra-server/src/tenant_api/mod.rs` | 路由（轻量段匹配）、前缀判定入口 `dispatch`、`respond_json`、`TenantApiConfig::from_env` |
| `crates/hydra-server/src/tenant_api/auth.rs` | `tenant_from_token(&ConfigStore, &str)` + 常数时间比较 + 与 `tenants_by_id` 的同快照读 |
| `crates/hydra-server/src/tenant_api/time_bound.rs` | `chrono` 侧：RFC3339/epoch → 规范字符串 + 窗口长度判定 |
| `crates/hydra-server/src/tenant_api/handlers.rs` | E1/E2/E3 三个 handler |
| `crates/hydra-server/src/tenant_api/throttle.rs` | 固定窗口限额（本地 DashMap；cluster-redis 下走 Redis） |
| `crates/hydra-server/src/clickhouse.rs` | CH 唯一传输 owner（从 `sink.rs` 下沉） |
| `crates/hydra-server/src/usage_query.rs` | `trait UsageQuery` + `SqliteUsageQuery` + `ClickHouseUsageQuery` |
| `crates/hydra-server/tests/tenant_api.rs` | 数据面集成（`#![cfg(all(db, http-client, proxy))]`） |
| `crates/hydra-server/tests/tenant_api_cluster.rs` | 集群用例（`#![cfg(feature = "cluster-redis")]`，真实 Redis） |
| `crates/hydra-server/tests/usage_query.rs` | 用量读路径（含 `#[cfg(feature="usage-clickhouse")]` 与一条 `#[ignore]` 的活 CH 用例） |

### 修改

| 文件 | 改动 |
|---|---|
| `crates/hydra-core/src/lib.rs` | `pub mod tenant_api;` |
| `crates/hydra-core/src/config.rs` | `ConfigData.tenants_by_id` + `Default` |
| `crates/hydra-server/src/store.rs` | `build_config` 同批构建 `tenants_by_id` |
| `crates/hydra-server/src/proxy.rs` | `request_filter` 第 0 步拦截；`AppState` +2 字段；`AppState::for_tests()` |
| `crates/hydra-server/src/main.rs` | `AppState` 构造后移；注入 `usage`、`invalidation`、存活节点列表闭包、`allow_ttl_max` |
| `crates/hydra-server/src/sink.rs` | CH 传输改为调用 `clickhouse.rs`（行为逐行不变） |
| `crates/hydra-server/src/cluster/events.rs` | 水位 HASH 写入（先 apply 后 ack）、`applied_watermarks()`、`await_applied()`、generation bump 不推进水位 |
| `crates/hydra-server/src/http.rs` | allow TTL 上限 |
| `crates/hydra-server/src/admin/mod.rs` | 删除租户路由块（`:616-668`） |
| `crates/hydra-server/src/admin/handlers.rs` | 公共小工具 `pub(crate)` 化；租户逻辑搬出；`tenant_id_for_token` 删除；管理端响应改 `fleet` |
| `crates/hydra-server/src/db.rs` | 仅新增 SQLite 用量聚合查询 |
| `crates/hydra-server/src/admin/metrics.rs` | 新增指标族 |
| `crates/hydra-server/tests/{terminate_mode,streaming_usage_persistence,tls,anthropic_passthrough,metrics}.rs` | 迁移 13 处 `AppState` 构造点到 `for_tests()` |
| `dev-docs/design.md` | §13.2 拆分、§11.7 指向新路径、§9.3 补 CH 读 |
| `dev-docs/ops.md` | §5.1 改路径；新增"租户 API 开通/关闭"、"租户改域名/auth_url 流程"、"CH 用量查询运维"三节 |
| `admin-ui/app.js`、`admin-ui/api-docs.js` | 租户页展示 base URL 与令牌状态；API 文档拆分 |
| `dev-docs/aegis/INDEX.md` | 登记本计划（`kind=plan`） |

---

## 分步任务（bite-sized）

> **执行纪律**：每个任务改完即跑该任务的 Verification 并提交**一个** commit。Coordinator 在第一个写操作前记录 `TaskStartSnapshot`（`git rev-parse HEAD` + `git status --porcelain`）。

### T1 — `hydra-core` 纯函数模块

**Files**：`crates/hydra-core/src/tenant_api.rs`（新建）、`crates/hydra-core/src/lib.rs`（`pub mod tenant_api;`）、`crates/hydra-core/tests/tenant_api.rs`（新建）
**Why**：端点契约里最容易被静默写错的三件事（时间格式、CH 整数形态、空集语义）都是纯字符串/JSON 判定，必须零 I/O 穷举测试。
**Change Necessity**：铁律 2 要求内部逻辑重构为纯函数并直测；这些判定若留在 shell 里就要靠集成测试间接覆盖，无法穷举。
**Impact/Compat**：`hydra-core` 无新依赖（**不得引入 chrono**，见设计 §7.1 v5.1 修正）。

**Steps**

1. **红灯**：写 `crates/hydra-core/tests/tenant_api.rs`，包含以下断言（节选，全部要写全）：

```rust
use hydra_core::tenant_api::*;

#[test]
fn canonical_timestamp_accepts_only_the_fixed_width_utc_form() {
    assert!(is_canonical_timestamp("2026-09-16T00:00:00Z"));
    for bad in [
        "2026-09-16 00:00:00",     // 空格分隔（SQLite datetime('now') 形态）——必须拒绝
        "2026-09-16T00:00:00",     // 缺 Z
        "2026-09-16T00:00:00+08:00", // 带偏移（应在 shell 被归一化，core 只认规范形态）
        "2026-09-16T00:00:00.123Z",  // 小数秒
        "2026-9-16T00:00:00Z",     // 非定宽
        "2026-13-16T00:00:00Z",    // 月越界
        "2026-09-32T00:00:00Z",    // 日越界
        "2026-09-16T24:00:00Z",    // 时越界
        "2026-09-16T00:60:00Z",    // 分越界
        "2026-09-16T00:00:60Z",    // 秒越界
        "",
    ] {
        assert!(!is_canonical_timestamp(bad), "{bad:?} must be rejected");
    }
}

#[test]
fn ordering_on_the_canonical_form_equals_chronological_ordering() {
    // 定宽 + 全 UTC + 同 T/Z 分隔 ⇒ 字典序 == 时间序。这是设计 §4.3.2 第 2 条的机制。
    assert!(canonical_le("2026-09-16T00:00:00Z", "2026-09-16T00:00:01Z"));
    assert!(canonical_le("2026-09-16T23:59:59Z", "2026-09-17T00:00:00Z"));
    assert!(!canonical_le("2026-09-17T00:00:00Z", "2026-09-16T23:59:59Z"));
    assert!(canonical_le("2026-09-16T00:00:00Z", "2026-09-16T00:00:00Z"));
    // 同一日期内：'T'(0x54) > ' '(0x20)，所以空格形态会"永远大于"——这正是要防的坑
    assert!(!canonical_le("2026-09-16T00:00:00Z", "2026-09-16 23:59:59"));
}

#[test]
fn clickhouse_quoted_int64_and_plain_number_are_both_accepted() {
    // 活实例实测：SELECT toUInt64(12345) FORMAT JSONEachRow → {"n":"12345"}
    assert_eq!(parse_lenient_u64(&serde_json::json!("12345")), Some(12345));
    assert_eq!(parse_lenient_u64(&serde_json::json!(12345)), Some(12345));
    assert_eq!(parse_lenient_u64(&serde_json::json!("")), None);
    assert_eq!(parse_lenient_u64(&serde_json::json!("abc")), None);
    assert_eq!(parse_lenient_u64(&serde_json::json!(null)), None);
    assert_eq!(parse_lenient_u64(&serde_json::json!(-1)), None);
    assert_eq!(parse_lenient_u64(&serde_json::json!(-1.0)), None);
}

#[test]
fn as_of_maps_empty_string_and_null_to_none() {
    // SQLite: NULL → None；ClickHouse: MAX(created_at) 空集 → "" （实测）→ 必须也是 None
    assert_eq!(normalize_as_of(None), None);
    assert_eq!(normalize_as_of(Some("")), None);
    assert_eq!(normalize_as_of(Some("2026-09-16T00:00:00Z")),
               Some("2026-09-16T00:00:00Z".to_string()));
}

#[test]
fn route_parsing_is_exact_and_rejects_near_misses() {
    assert_eq!(parse_route("/tenant/t1/api/v1/whoami"),
               Some(TenantApiRoute { tenant_id: "t1", endpoint: Endpoint::Whoami }));
    assert_eq!(parse_route("/tenant/t1/api/v1/auth/cache/invalidate"),
               Some(TenantApiRoute { tenant_id: "t1", endpoint: Endpoint::InvalidateAuthCache }));
    assert_eq!(parse_route("/tenant/t1/api/v1/usage"),
               Some(TenantApiRoute { tenant_id: "t1", endpoint: Endpoint::Usage }));
    for bad in [
        "/tenant//api/v1/whoami",           // 空 tenant_id
        "/tenant/a/b/api/v1/whoami",        // tenant_id 含 '/'
        "/tenant/t1/api/v1",                // 缺 endpoint
        "/tenant/t1/api/v1/usage/",         // 尾斜杠
        "/tenant/t1/api/v1/whoami/extra",   // 多余段
        "/tenant/t1/api/v2/whoami",         // 版本不符
        "/tenantapi/v1/whoami",             // 前缀粘连
        "/tenant/t1/api/v1/models",         // 不是本 API 的端点
    ] {
        assert!(parse_route(bad).is_none(), "{bad:?} must not parse");
    }
}
```

2. **Verify RED**：`cargo test -p hydra-core --test tenant_api` → 期望**编译失败**（模块不存在）。补最小骨架（签名 + `todo!()` 之外的空实现）使编译通过后重跑 → 期望断言失败。

   > 注：本仓库 wave-6 grep 门禁禁止生产代码出现 `unimplemented!`/`todo!`。红灯阶段的骨架可以用返回 `false`/`None` 的**真实最小实现**（不是占位宏），这样断言失败才是我们要的红灯。

3. **GREEN**：实现 `crates/hydra-core/src/tenant_api.rs`。要点：
   - `is_canonical_timestamp`：长度必须 20；分隔符位置必须为 `-`,`-`,`T`,`:`,`:`,`Z`；其余位必须为 ASCII 数字；再检查 `MM∈01..=12`、`DD∈01..=31`、`HH∈00..=23`、`MI∈00..=59`、`SS∈00..=59`。**不做日历运算**（不判闰年/月长），因为那需要日期库，而 core 不允许有。
   - `canonical_le`：`a <= b` 字典序；doc 注释必须写明"仅当两者都通过 `is_canonical_timestamp` 时等于时间序"。
   - `parse_lenient_u64`：接受 `Value::Number`（`as_u64`，并用 `as_f64` 排除负数/小数）与 `Value::String`（`parse::<u64>()`，空串→None）。
   - `normalize_as_of`：`None`/空串 → `None`。
   - `parse_route`：以 `/tenant/` 严格前缀开头，取到下一个 `/` 前的 `tenant_id`（非空且不含 `/`），其余必须**精确等于** `api/v1/whoami`、`api/v1/auth/cache/invalidate`、`api/v1/usage` 三者之一。
   - DTO：`UsageTotals { requests, tokens_in, tokens_out, cache_hit_tokens, errors: u64 }` + `Default`（全 0，即 `COALESCE` 语义）、`UsageRow { key: String, totals: UsageTotals }`、`UsageAggregate { totals, rows, as_of: Option<String> }`。

4. **Verify GREEN**：`cargo test -p hydra-core --test tenant_api` → 全绿；`cargo test -p hydra-core` → 15 套件全绿（无回归）。

5. **Commit**：`feat(core): tenant API pure functions (timestamp form, route parsing, CH-lenient numbers)`

**Verification**：
```bash
cargo fmt --check
cargo clippy -p hydra-core --all-targets -- -D warnings
cargo test -p hydra-core
cargo test -p hydra-core --test tenant_api
cargo tree -p hydra-core --no-default-features   # 防火墙：仍无 tokio/pingora/sqlx/reqwest/hyper
```

### T2 — `ConfigData.tenants_by_id` 派生索引

**Files**：`crates/hydra-core/src/config.rs`、`crates/hydra-server/src/store.rs`（`build_config`）、`crates/hydra-core/tests/config_data.rs`
**Why**：E1 要按 `tenant_id` O(1) 取行；今天只有 `tenants_by_domain`（键是域名）。
**Change Necessity**：备选是线性扫描 `tenants_by_domain.values()`。选派生索引是因为**存在性判定**（令牌有效但租户行不存在）也走同一索引，且它与 `models_by_key` 之于 `provider_models` 同构——**同批行、同 loader**，不是第二个 source of truth。
**Impact/Compat**：`ConfigData` 是 `pub` 字段结构体，但**穷举字面量只有 2 处**（已实测核对：`grep -rn -A1 "ConfigData {" crates/` 后逐处读过）：
- `crates/hydra-server/src/store.rs:178` —— `build_config` 的返回字面量，**正是本任务要改的地方**；
- `crates/hydra-core/tests/validate.rs:275` —— 一个测试字面量，补一个 `HashMap::new()` 即可。

其余 7 处（`hydra-core/tests/{router,validate,admission}.rs`、`hydra-server/src/{proxy,cluster/snapshot}.rs`、`hydra-server/tests/load_breaker_swrr.rs`）都用 `ConfigData::default()` + 字段赋值，**新增字段不会打断它们**（另有 38 处 `ConfigData::default()` 调用点同样不受影响）。所以本任务是"1 处生产 + 1 处测试 + `Default` + loader 构建"，不是"修一片"。

**Steps**
1. **红灯**：在 `crates/hydra-core/tests/config_data.rs` 加一条断言：手工构造两个租户（domain 与 id 不同序），断言 `tenants_by_id` 能按 id 取到、且 `len()` 与 `tenants_by_domain` 一致。
2. **Verify RED**：`cargo test -p hydra-core --test config_data` → 编译失败（字段不存在）。
3. **GREEN**：
   - `config.rs`：`pub tenants_by_id: HashMap<String, Tenant>,` 加在 `tenants_by_domain` 之后，doc 注释写明"派生索引，与 `tenants_by_domain` 同源；不是第二份真相"；
   - `ConfigData::default()` 补 `HashMap::new()`；
   - `store.rs::build_config`：在填 `tenants_by_domain` 的同一循环里同时 `insert(t.id.clone(), t.clone())`（**同一批行、同一循环**，禁止第二次查库）。
4. **Verify GREEN**：`cargo test -p hydra-core` + `cargo test -p hydra-server --features server --test loader` + `cargo test -p hydra-server --features server --test repo`。
5. 编译并修掉因新增字段而失败的构造点：**预期只有 `crates/hydra-core/tests/validate.rs:275` 一处**（`store.rs:178` 已在步骤 3 改过）。若编译器报出第三处，说明有未预期的穷举字面量，**必须停下报告**而不是顺手补上——那意味着 `ConfigData` 的构造分散程度超出本计划已知的事实。
6. **Commit**：`feat(core,server): derive ConfigData.tenants_by_id alongside tenants_by_domain`

**Verification**：
```bash
cargo fmt --check && cargo clippy --workspace --all-targets --features hydra-server/server -- -D warnings
cargo test -p hydra-core
cargo test -p hydra-server --features server --test loader --test repo --test config_store
```

### T3 — **commit 1**：CH 传输下沉到 `clickhouse.rs`（零行为差异）

**Files**：`crates/hydra-server/src/clickhouse.rs`（新建）、`crates/hydra-server/src/lib.rs`（`pub mod clickhouse;`）、`crates/hydra-server/src/sink.rs`
**Why**：Q14=T1 要求"怎么跟 CH 说话"只有一个 owner；读路径需要真正的响应体读取，而现有写通道把响应体当错误文本丢弃（`sink.rs:801-811`）。
**Change Necessity**：备选是读侧再写一份 `parse_url`/状态分类 —— 那会让 URL/凭据/协议的解释分叉成两套（设计 §4.3.3 T2 已论证否决）。
**Impact/Compat**：**这是本次唯一动到正在工作的生产代码的改动**。纪律：本 commit **只做搬迁**，不夹带任何读路径代码；行为与期限**逐行不变**。
**Risk**：回归网 = 既有 4 条 `clickhouse_sink` 测试（含 1 条 `#[ignore]`），外加本机活实例手工跑那条 ignored。

**Steps**
1. **先建回归基线**：`cargo test -p hydra-server --features server,usage-clickhouse --test clickhouse_sink` → 记录输出（3 passed, 1 ignored）。
2. 新建 `clickhouse.rs`，**逐行**搬运以下四块（不改逻辑）：
   - `ClickHouseConfig` 与其 URL/凭据解析（原 `sink.rs:446-469`，含 `user:pass@` → Basic 头）；
   - `url_encode`（原 `sink.rs:899-914`）；
   - 裸 TCP HTTP 原语（原 `sink.rs:711-797`）：连接、请求行、`Content-Length`、`Connection: close`、connect/IO deadlines。**签名加一个显式 `timeout: Duration` 入参**（写路径传它原来硬编码的值，保证零行为差异；读路径将传 `HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS`）；
   - 状态行分类（原 `sink.rs:801-811`）：保留既有 `status_line.contains(" 200 ")` 语义，**并额外**提供 `parse_exception_code(body) -> Option<u32>`（识别 `Code: N. DB::Exception:`，供读侧分类用；写路径暂不使用）。
3. `sink.rs` 改为调用新模块：删掉搬走的实现，保留批量/flush/重试/丢弃计数逻辑不变。
4. **Verify 零差异**：
```bash
cargo fmt --check
cargo clippy --workspace --all-targets --features hydra-server/server,hydra-server/usage-clickhouse -- -D warnings
cargo test -p hydra-server --features server,usage-clickhouse --test clickhouse_sink   # 期望与步骤 1 完全一致
# 活实例（本机有 hydra-local-clickhouse）：
CH_URL=http://127.0.0.1:8123 cargo test -p hydra-server --features server,usage-clickhouse \
  --test clickhouse_sink -- --ignored --nocapture
# 落库验证（应能看到本测试写入的行数增加）
curl -s --data-binary "SELECT count() FROM usage_record" 'http://127.0.0.1:8123/'
```
5. 全特性回归：`cargo test -p hydra-server --features server,cluster-redis,usage-clickhouse`。
6. **Commit（只搬迁）**：`refactor(server): extract the ClickHouse transport into clickhouse.rs (no behaviour change)`

### T4 — `tenant_api` 骨架 + 数据面前缀拦截 + 令牌闸门

**Files**：`crates/hydra-server/src/tenant_api/{mod,auth,time_bound}.rs`（新建）、`crates/hydra-server/src/proxy.rs`、`crates/hydra-server/src/main.rs`、13 处测试构造点
**Why**：R1+R2 的落地：专用入口、数据面可达、只认租户令牌。令牌闸门读快照是"edge 也能鉴权 / 零转发"的前提。
**Change Necessity**：拦截必须进 `request_filter` 第 0 步（设计 §3.2 的 A/B/C/D 四条理由，C 条最硬：晚于 api-key 抽取会把租户令牌送去 `auth_url`）。
**Impact/Compat**：`AppState` 加 2 字段 → **打断 13 个测试构造点**，必须同批提供 `for_tests()` 并迁移；`main.rs` 的 `AppState` 构造后移。

**Steps**
1. **红灯**：新建 `crates/hydra-server/tests/tenant_api.rs`，写 T2/T3/T4/T5/T6/T10/T19/T20/T22（见设计 §10.2），只保留**令牌与闸门相关**的断言：
   - 无 token → 401，且断言**未触碰 DB**（用 `:memory:` 池 + 计数断言或只读池）；
   - 错 token → 401，文案与上一条一致；
   - token 属于 B、URL 写 A → 403 `tenant_id_mismatch`；
   - 管理 token 调 `/tenant/.../whoami` → 401；
   - 租户 token 调 `/api/v1/providers` → 401；
   - **T19/令牌只在快照**：只改 store 的快照（`apply_snapshot`）不改 DB，鉴权仍按快照判定；
   - **T22/边界**：`POST /v1/chat/completions` 仍原样透传（wiremock 收到 1 次），`/tenant/...` 不落到上游；
   - **T8/令牌不得被当成客户端 api-key**：用租户令牌调 `/tenant/{tid}/api/v1/whoami` 时，断言 **wiremock 上的 `auth_url` 收到 0 次请求**（外部鉴权从未运行）、且响应头/日志里不出现该令牌。这条钉住的是"拦截必须早于 api-key 抽取"（设计 §3.2 理由 C）；
   - **T9/不计费**：调用任意租户端点后断言 `usage_record` **新增 0 行**、`hydra_requests_total` **未增加**（设计 §3.2 的短路序保证 `ctx.selected` 为空）；
   - **C14/令牌索引随快照**（需 `cluster-redis`，放集群套件）：leader 改某租户令牌 → `reload_all` → 远端节点（含 edge）用**新令牌**可鉴权、**旧令牌 401**（证明闸门读的是快照而不是 DB）；
   - **C18/内部面不外泄**：`/api/v1/internal/*` 在数据面（8080）上**不是**内部路由——它在管理口才存在（`admin/mod.rs:582-604`），断言数据面对该前缀按普通租户路径处理（既不返回内部数据、也不出现 `cluster_token` 语义）。
2. **Verify RED**：`cargo test -p hydra-server --features server --test tenant_api` → 失败（前缀未拦截，请求落到业务管线 → 断言不符）。
3. **GREEN**（按此顺序）：
   a. `AppState`（`proxy.rs:108-128`）加 `invalidation: Option<InvalidationStream>`（含 `not(cluster-redis)` 的 `Option<()>` 占位，对齐 `admin/mod.rs:108-113`）与 `usage: Arc<dyn UsageQuery>`；提供 `pub fn for_tests(pool, store, auth, breaker, limiter, sink, proxy) -> Arc<AppState>`；
   b. 迁移 13 处构造点到 `for_tests()`：`tests/terminate_mode.rs:{224,600,718,1089,1320,2871}` 与 `:206-233` 的 helper、`tests/streaming_usage_persistence.rs:271`、`tests/tls.rs:166`、`tests/anthropic_passthrough.rs:{302,392,506}`、`tests/metrics.rs:282`；
   c. `main.rs`：把 `AppState` 构造**移到 `invalidation_stream` 之后**（`:621-644` 之后）；`:594` 的 `state.sink.clone()` 改为先克隆 `sink`；
   d. `tenant_api/mod.rs`：`pub async fn dispatch(state: &AppState, session, ctx) -> PingoraResult<bool>`；轻量段匹配（形制照 `admin/mod.rs:277-419`）；`respond_json`（形制照 `respond_catalog`，`proxy.rs:1280-1297`）；`TenantApiConfig::from_env()`（`HYDRA_TENANT_API=on|off`，默认 on）；
   e. `tenant_api/auth.rs`：`pub fn tenant_from_token(store: &ConfigStore, bearer: &str) -> Option<String>`。**必须在同一次 `replication()` guard 内**同时取令牌摘要表与 `tenants_by_id`（设计 §3.3 规则 4）；常数时间比较；`replication()` 为 `None` → `NotReady`（503）；
   f. `proxy.rs::request_filter` 开头（`let cfg_guard` 之前）插入：
```rust
// (0) Tenant self-service API — data-plane reserved prefix. Intercepted BEFORE
//     Host→tenant (otherwise an IP/own-hostname call 404s and "set my domain"
//     could never bootstrap), before the api-key extraction (Authorization:
//     Bearer is ALSO a client api-key transport — a tenant token must never be
//     forwarded to auth_url or masked into a usage record), and before the
//     /v1/models catalog intercept.
if let Some(route) = hydra_core::tenant_api::parse_route(session.req_header().uri.path()) {
    if self.state.tenant_api.enabled {
        return crate::tenant_api::dispatch(&self.state, session, ctx, route).await;
    }
}
```
   g. `dispatch` 内：令牌闸门 → URL `tenant_id` 交叉校验（403）→ 端点分派。**增量接线规则（避免任何占位实现）**：
      - T1 的 `parse_route` **一次写全**三个端点的匹配（它是纯匹配器，完备性由 T1 的单测钉住）；
      - T4 的 `dispatch` 只把 `Endpoint::Whoami` 接上真实 handler；**未接线**的端点返回 `404 unknown path` ——
        这与今天的系统行为**完全一致**（那些路径本来就不存在），因此不是占位，而是"该路由尚未提供"的诚实状态；
      - T5 把 `Endpoint::InvalidateAuthCache` 接上，T7 把 `Endpoint::Usage` 接上；
      - 每个任务的 RED 只覆盖它本次接线的端点；**没有任何一个任务的产物是"桩"**。
4. **Verify GREEN**：`cargo test -p hydra-server --features server --test tenant_api`（仅闸门用例全绿）；`cargo test -p hydra-server --features server` 全绿（证明 13 处迁移无回归）。
5. **Commit**：`feat(server): tenant API skeleton — data-plane prefix interception and snapshot token gate`

**Verification**：
```bash
cargo fmt --check && cargo clippy --workspace --all-targets --features hydra-server/server -- -D warnings
cargo test -p hydra-server --features server
```

### T5 — E1 `GET /whoami`

**Files**：`crates/hydra-server/src/tenant_api/handlers.rs`（新建）、`tenant_api/mod.rs`
**Why**：R5。它是唯一的"平台认为我是谁"的自述入口，成本近零（纯快照）。
**Change Necessity**：E1 全部数据来自快照，**不需要 DB**，因此它在 edge 上也成立——这正是"零转发"的关键一环。
**Impact/Compat**：无。

**Steps**
1. **红灯**：追加 T1（正确 token → 200，字段齐全）、**T2 的加强版**（响应体里不得出现任何摘要/密钥：断言 body 不含 `access_token`、不含 `cert_key`）、T7（**停用租户仍 200**）、T20（`replication()==None` → 503）。
2. **Verify RED** → 失败。补充断言：
   - **T23/自述不自相矛盾**：响应里的 `base_url` 必须与实际可用前缀一致（用它去拼一次请求，断言得到 200，而不是 404）；
   - **C12/edge 上 E1 可用**（集群套件）：edge 节点（`pool=None`）调 E1 → 200 且来自快照 —— 这条是"E1 不需要 DB、不需要转发"的证据；
   - **T7 的 E1 部分**：**停用**租户调 E1 → 200（自救路径必须畅通；E2/E3 的部分在 T6/T8 各自覆盖）。
3. **GREEN**：`handlers::whoami(state) -> Resp`，返回 `{tenant_id,name,enabled,domain,auth_url,config_version,base_url}`；`config_version` 用 `store.version()`（`store.rs:381-385`）；`base_url = format!("/tenant/{tid}/api/v1")`。
4. **Verify GREEN**。
5. **Commit**：`feat(server): tenant API E1 GET /whoami (snapshot-only)`

### T6 — E2 `POST /auth/cache/invalidate` + 收敛屏障（L1 扇出 + L2 权威 + L3 确认）

**Files**：`crates/hydra-server/src/tenant_api/handlers.rs`、`tenant_api/throttle.rs`（新建）、`tenant_api/mod.rs`、`crates/hydra-server/src/cluster/events.rs`、`crates/hydra-server/src/admin/handlers.rs`（管理端响应改 `fleet`）
**Why**：R3 —— **必须在全部数据面节点生效，且调用方能确知是否生效**。这是本次最核心的交付。
**Change Necessity**：今天远端节点的 L1 命中项不会因发布方删了 L2 而消失（L1 命中不查 L2），且消费者的 `last_id` 只存在局部变量里（`events.rs:291`）从不发布 → 系统里没有任何一处能回答"清干净没有"。
**Impact/Compat**：`DELETE /api/v1/auth/cache`（运维用）响应体由 `published: bool` 换成 `fleet` 对象；`tenant_api` 复用既有 `AuthCache`/`InvalidationStream` 原语，不新增失效机制。

**Steps**
1. **红灯**（`tests/tenant_api.rs`）：T11（精确 key → `invalidated>=1`，随后同 key 请求**必须回源**，wiremock 断言 `auth_url` 被再次调用）、T12（空 body → `scope:"tenant"`）、T13（1001 key → 400 `too_many_keys`；单个 4097 字节 → 400 `invalid_api_key`）、T21（失败限流 → 429）、**C3（单节点 `all` → `state:"single_node"`）**。
   **红灯**（`tests/tenant_api_cluster.rs`，新文件，真实 Redis）：C1（edge 上 E2：本节点 L1 清空 + 流里一条 v=2 记录且**载荷只有摘要无明文**）、C2（集群成员但无 Redis 后端 → **503 `fleet_invalidation_unavailable`**，不是 `single_node`、不是假 `applied`）、C4（两节点收敛 → 200 + `nodes_applied==nodes_total`）、**C5（远端消费者停摆 → 202 + `state:"pending"` + `lagging` 精确列出该节点，且 `consumer_stalled_seconds` 上升）**、**C6（先 apply 后 ack 的关键负例）**、C7（generation bump 后水位不推进，在途事件报 `pending` 而非 `applied`）、C8（屏障**不是转发**：出站无 `x-hydra-forwarded`）、C9（心跳过期节点不入 `nodes_total` 且不阻塞）、C10（两个独立 Redis 互不影响）、**C17（同一失效事件重复消费幂等，不报错）**。
2. **Verify RED** → 失败（`fleet` 字段不存在 / 水位不存在）。
3. **GREEN**：
   a. **先钉住 cfg 形状**（已实测核对）：`cluster::events` 与 `cluster::registry` 都是 `#[cfg(feature = "cluster-redis")]` 门控的（`cluster/mod.rs:23-24`、`:27-28`），因此 `InvalidationStream` 在无该特性时**根本不存在**。于是：
      - `AppState.invalidation` 必须**镜像 `AdminState` 的双字段写法**（`admin/mod.rs:108-113`）：有特性时 `Option<InvalidationStream>`，无特性时 `#[allow(dead_code)] Option<()>`；
      - `handlers::invalidate` 必须有 cfg 分支：有特性 → `publish` + `await_applied`；无特性 → **只做本地清除并回 `state: "single_node"`**；
      - **`"single_node"` 是可推导的、不是猜测**：无 `cluster-redis` 特性时 `HYDRA_ROLE=leader|edge` 会被启动检查拒绝（`main.rs:241-243` "requires the 'cluster-redis' cargo feature"），所以"无特性 ⇒ 不是集群 ⇒ 本地清除即全部"。
      - 因此**不存在**"集群成员但没有失效通道"这一状态在无特性构建里的对应物；该状态只可能出现在"编译了 cluster-redis 但没有 Redis 后端"（`main.rs:621-644` 下 `invalidation_stream = None`）→ 回 **503**（C2）。

   b. `cluster/events.rs`：新增键 `hydra:{ctl:inv:applied}`（**一个 HASH**：`node_id → last_applied_event_id`）。消费者在**成功 apply 一批之后**把该批最大事件 ID 一次性 HSET（**先 apply 后 ack**，顺序不可颠倒）；新增 `applied_watermarks() -> HashMap<String,String>` 与 `await_applied(event_id, live_nodes, timeout) -> AppliedOutcome {Applied{nodes}, Pending{nodes_applied, nodes_total, lagging}, SingleNode, Unavailable}`；**generation bump 路径 `clear_all()` 之后不推进水位**（被裁剪的事件 ID 已不可知，不得为没读到的事件记账）。
   c. `tenant_api/handlers.rs::invalidate`：校验/上限复用（把 `admin/handlers.rs` 的 `invalidate_shape_error`、`MAX_INVALIDATION_KEYS`、`MAX_API_KEY_LEN` 提为 `pub(crate)`）；本节点调 `state.auth.invalidate[_tenant]`；`publish`；然后 `await_applied`；按结果回 **200 / 202 / 503**；响应体按设计 §4.2 的 `fleet` 对象；`checked` 与 `invalidated` 并列。
   d. `tenant_api/throttle.rs`：固定窗口限额（源 IP / 令牌摘要 / 租户成功 / **每租户失效频率**），`cluster-redis` 下走 Redis 计数（参照 `redis/rate_limit.rs` 的窗口原语），其余用 `DashMap`。
   e. `admin/handlers.rs::auth_cache_invalidate` 响应改用同一 `fleet` 结构（**共用同一个 `await_applied` 实现**，不复制）。
   f. 存活节点列表：`main.rs` 注入 `Arc<dyn Fn() -> Vec<String> + Send + Sync>`（返回存活 node_id），与 `AdminState.leader_ready` 同一闭包注入手法（`admin/mod.rs:104`）——**不把 registry 放进 `AppState`**。
4. **Verify GREEN**：数据面 + 集群两套测试全绿。
5. **Commit**：`feat(server,cluster): tenant API E2 fleet-wide cache invalidation with a convergence barrier`

**Verification**：
```bash
export HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6379
cargo test -p hydra-server --features server --test tenant_api
cargo test -p hydra-server --features server,cluster-redis --test tenant_api_cluster
cargo test -p hydra-server --features server,cluster-redis
```

### T7 — allow TTL 硬上界

**Files**：`crates/hydra-server/src/http.rs`、`crates/hydra-server/src/main.rs`、`crates/hydra-server/src/admin/metrics.rs`
**Why**：E2 的收敛确认覆盖"在消费的存活节点"；**消费者停摆的节点**只能靠 L1 的 allow TTL 兜底——而今天该 TTL 可被租户用 `expires_in` 抬到任意长（`http.rs:661-674`；`clear_all` 的注释自己承认，`http.rs:309-311`）。即"某节点上封禁多久生效"部分由被停用租户决定。
**Change Necessity**：这是让残余窗口**有限且不由租户决定**的唯一手段；备选（不封顶）就是保留该语义缺陷。
**Impact/Compat**：默认 300s = 现有默认值 ⇒ **对不设 `expires_in` 的租户零行为变化**；设了长 `expires_in` 的租户会增加认证回源（`ops.md` 必须写明这一取舍）。

**Steps**
1. **红灯**：`tests/http_auth.rs` 加一条：wiremock 认证应答带 `expires_in=86400`，断言缓存项 TTL 被封到上限（用注入 `Clock` 的确定性写法，避免 sleep）。
2. **Verify RED** → 失败（TTL 为 86400s）。补充：**C11（`HYDRA_AUTH_ALLOW_TTL_MAX_SECS` 端到端生效）**——同一输出下断言 `hydra_auth_allow_ttl_capped_total` 增加。
3. **GREEN**：`AuthConfig` 加 `allow_ttl_max: Duration`（`default()` = 300s）；`AuthCache::new/with_clock` 接收并保存；`set`/`set_if_unchanged` 对 **allow** 项套 `ttl.min(allow_ttl_max)`（**deny 不受影响**）；`main.rs` 从 `HYDRA_AUTH_ALLOW_TTL_MAX_SECS` 读取并接线；新增计数 `hydra_auth_allow_ttl_capped_total{tenant}`（`metrics.rs`）。
4. **Verify GREEN** + `cargo test -p hydra-server --features server --test http_auth --test auth_cache`。
5. **Commit**：`fix(auth): cap the allow-cache TTL so a stalled node's stale window is operator-bounded`

### T8 — **commit 2**：`usage_query.rs` 双后端读路径（含 CH）

**Files**：`crates/hydra-server/src/usage_query.rs`（新建）、`crates/hydra-server/src/db.rs`、`crates/hydra-server/src/tenant_api/handlers.rs`、`crates/hydra-server/src/main.rs`、`crates/hydra-server/src/admin/metrics.rs`、`crates/hydra-server/tests/usage_query.rs`（新建）
**Why**：R4 —— **生产必跑集群，用量走 ClickHouse，首发可用**。今天 CH 只有 INSERT，没有读路径。
**Change Necessity**：不做 CH 读，集群部署下这个端点就是空的（设计 v3 曾以 501 搪塞，用户已否决）。
**Impact/Compat**：纯新增读面；`UsageSink` trait **不改**（它的语义是 fire-and-forget 写入）。

**Steps**
1. **红灯**（`tests/usage_query.rs`，纯函数部分确定性、不需要真 CH）：
   - **T14**：断言注入的实现与 `sink_kind` 一致（`sqlite`→`SqliteUsageQuery`、`clickhouse`→`ClickHouseUsageQuery`）；
   - **T14a（最危险）**：喂 `{"requests":"8"}` 与 `{"requests":8}` **都必须解出 8**；喂 `{"requests":"abc"}` **必须 `decode_error` 而不是 0**；
   - **T14b**：`{"requests":"0","last_seen":""}` → `as_of: null`（**不是空串**）；
   - **T14c**：HTTP 404 + `Code: 60. DB::Exception: …` → `usage_store_unavailable`；
   - **T15/T16/T17**：SQLite 时间窗与手写 SQL 对照（含 NULL 语义与 `errors`）；`since` 传空格分隔形态 → 归一化后正确，且**对照"若不归一化会多算整天"的反例**；窗口超上限 / `since > until` → 400；
   - **T18**：`?tenant_id=other` 被忽略（契约无此参数）。
   - **C13（集群下 E3 在 edge 上可用）**：`sink=clickhouse` 时 edge 调 E3 → **200 且 `source:"clickhouse"`**，数字与直连 CH 手跑 SQL 一致（= "集群下可用且不需要转发"的证据）；
   - **C13a（CH 不可达不是假 0）**：把 CH 端口指错 → **503 `usage_store_unavailable`**，且 `usage_query_seconds` 有观测值；断言**不是 200 + 全 0、不是 panic**；
   - **`#[ignore]` 活 CH 用例**（形制照 `tests/clickhouse_sink.rs:172-180`）：真连本机 CH，断言与手跑 SQL 一致。
2. **Verify RED** → 失败。
3. **GREEN**：
   a. `usage_query.rs`：
```rust
pub enum UsageQueryError { StoreUnavailable(String), Decode(String), WindowTooLarge { max_days: u32 } }

/// 与同区域的 `UsageSink`（`sink.rs:46-63`）用**同一种风格**：手写 desugar 的
/// `Pin<Box<dyn Future>>` 而不是 `#[async_trait]`。理由是 dyn 兼容性与既有兄弟 trait
/// 保持一致（同目录同风格），而不是引入第二种异步 trait 写法。
pub trait UsageQuery: Send + Sync {
    fn aggregate<'a>(
        &'a self,
        tenant_id: &'a str,
        since: &'a str,
        until: &'a str,
        group_by: GroupBy,
    ) -> Pin<Box<dyn Future<Output = Result<UsageAggregate, UsageQueryError>> + Send + 'a>>;

    /// 本次应答的数据来源，由实现自报（"sqlite" | "clickhouse"）。
    /// 由实现回答而不是由调用方推断：同一个能力对象同时回答"数据从哪来"。
    fn source(&self) -> &'static str;
}
```
   b. `SqliteUsageQuery { pool: SqlitePool }`：查询用**运行时校验的 `sqlx::query`**（与 `sink.rs:383-384` 同风格 ⇒ **不改 `.sqlx/`**）；SQL 见设计 §4.3.1；`as_of` 走 `normalize_as_of`。
   c. `ClickHouseUsageQuery { cfg: clickhouse::ClickHouseConfig, client_timeout: Duration }`：SQL 见设计 §4.3.1，**必须用 `{t:String}`/`{s:String}`/`{e:String}` + `param_*` 绑定**（实测抗注入），`FORMAT JSONEachRow`；`group_by` 是**白名单**映射到列名（绝不插值）；解码用 core 的 `parse_lenient_u64` 与 `normalize_as_of`（CH-A/CH-B）；错误按 `Code: N` 分类（CH-D）。
   d. `db.rs`：仅新增 SQLite 聚合查询函数。
   e. `main.rs`：按 `sink_kind` 注入 `Arc<dyn UsageQuery>`；CH 分支复用 `clickhouse.rs` 的解析（与写路径**同一次解析函数**）。
   f. 指标：`hydra_tenant_api_usage_query_total{source,group_by,result}`（含 `decode_error`）与 `hydra_tenant_api_usage_query_seconds{source}`。
4. **Verify GREEN** + 活 CH：
```bash
cargo test -p hydra-server --features server --test usage_query
CH_URL=http://127.0.0.1:8123 cargo test -p hydra-server --features server,usage-clickhouse \
  --test usage_query -- --ignored --nocapture
curl -s --data-binary "SELECT tenant_id, count() FROM usage_record GROUP BY tenant_id FORMAT JSONEachRow" \
  'http://127.0.0.1:8123/'   # 与端点输出人工对照一次
```
5. **Commit**：`feat(server): tenant API E3 usage query over SQLite and ClickHouse`

### T9 — 删除旧路由 + 文档 + 前端

**Files**：`crates/hydra-server/src/admin/mod.rs`、`admin/handlers.rs`、`dev-docs/design.md`、`dev-docs/ops.md`、`admin-ui/app.js`、`admin-ui/api-docs.js`、`dev-docs/aegis/INDEX.md`
**Why**：R6 + 反熵：不留双实现、不留悬空零件。
**Change Necessity**：旧路由与 `tenant_id_for_token` 与新实现**同一个行为两个 owner**；用户已确认尚未上线，无需兼容层。
**Impact/Compat**：**破坏性**（用户已确认）。管理面回到单一凭证语义，不再有"排在 admin 闸门之前"的特殊分支。

**Steps**
1. 删除 `admin/mod.rs:616-668` 整块（租户令牌路由）；`handlers.rs` 删除 `tenant_id_for_token`；`admin-ui/api-docs.js` 移除旧条目。
2. **红灯（C16）**：先写断言再删除路由 —— 删除前它会**红**（因为删除前租户令牌返回 200，而断言要求 401）。

   **Verify 旧路径确实消失**：加 C16 断言。**注意：断言必须用两种凭证分别验证，单一断言会写成错的**（本节已实测追踪过 admin 路由器的顺序）：

   | 凭证 | 删除**前** | 删除**后** | 说明 |
   |---|---|---|---|
   | 租户令牌 | 200（旧路由在 admin 闸门之前） | **401** `missing or invalid admin token` | 旧路由消失后请求继续下落到 admin 闸门（`admin/mod.rs:670-679`）→ 被拒 |
   | 管理令牌 | 404（`parts.len()>2` 的深路径拒绝，`admin/mod.rs:363-366`） | **404** | 与删除前相同，**不能单独作为证伪信号** |

   因此 C16 必须断言**两条同时成立**：
   - 租户令牌打该路径 → **401**（**关键**：删除前是 200，所以 200→401 就是"租户路由已消失"的证伪信号）；
   - 管理令牌打该路径 → **404**（证明该路径从未是运维资源）。

   > 只断言"404"是错的：租户令牌在删除后返回的是 401，写成 404 会让这个测试**永远失败**——
   > 这正是"证伪信号必须先用真实代码走一遍"的例子。
3. **反熵核对**：`grep -rn "tenant_id_for_token\|auth/cache/invalidate" crates/` → 只剩新模块与测试。
4. 文档：`design.md` §13.2 端点表拆分（租户端点移出「管理 Web API」）、新增「租户自助 API（数据面）」节、§11.7 表格指向新路径、§9.3 补 CH 读；`ops.md` §5.1 改路径 + 新增三节（租户 API 开通/关闭、**租户改域名/auth_url 的运维流程**、**CH 用量查询运维**：`requests` 为近似值的成因、宽窗口代价、`ORDER BY` 建议）。
5. 前端：`app.js` 租户页展示 base URL 与令牌状态；`api-docs.js` 拆分为"租户 API / 运维 API"两页。
6. **Playwright（强制）**：见 T10。
7. **Commit**：`refactor(admin,docs,ui)!: delete the admin-plane tenant route; document the data-plane tenant API`

### T10 — 门禁 + 验收证据回填

**Files**：本计划文档（`## 实施记录`）、`dev-docs/aegis/INDEX.md`
**Why**：仓库约定：验收证据写回受版本控制的 markdown（`.acceptance/` 是 gitignore 的本机工具链暂存目录，**不是**验收机制）。
**Steps**
1. 跑全套门禁（命令见下）。
2. Playwright（真实二进制 + 真实 Chromium）：
```bash
cargo build --release --workspace --features hydra-server/server
HYDRA_ADMIN_TOKEN=dev-admin-token-2026 HYDRA_ADMIN_ADDR=127.0.0.1:8081 \
  HYDRA_DB_URL="sqlite://$PWD/e2e-ci.db?mode=rwc" HYDRA_ENCRYPTION_KEY=... \
  nohup ./target/release/hydra >/tmp/hydra-e2e.log 2>&1 &
./tests/e2e/seed.sh && npx playwright test --config=playwright.config.cjs
```
3. 在本计划末尾回填 `## 实施记录`：状态 + `| 项 | 结果 |` 表（精确 pass 计数）+ `执行期修正` + `遗留`。
4. **Commit**：`docs(aegis): tenant API implementation record and gate evidence`

**Verification（全量门禁，CI 同款）**：
```bash
export RUSTFLAGS="-D warnings" SQLX_OFFLINE=true
export HYDRA_TEST_REDIS_URL="redis://127.0.0.1:6379"

cargo update --workspace --locked --dry-run                                   # ci.yml:58
cargo fmt --check                                                             # ci.yml:61
cargo clippy --workspace --all-targets --features hydra-server/server -- -D warnings   # ci.yml:69
cargo build --workspace --features hydra-server/server                        # ci.yml:72
cargo test -p hydra-core                                                      # ci.yml:77
cargo tree -p hydra-core --no-default-features                                # ci.yml:86 防火墙
cargo test -p hydra-server --features server                                  # ci.yml:100

cargo clippy --workspace --all-targets --features hydra-server/server,hydra-server/cluster-redis,hydra-server/usage-clickhouse -- -D warnings  # ci.yml:140
cargo build  --workspace --features hydra-server/server,hydra-server/cluster-redis,hydra-server/usage-clickhouse                              # ci.yml:143
cargo test   -p hydra-server --features server,cluster-redis,usage-clickhouse                                                                 # ci.yml:146

node scripts/check_i18n.js && node --test scripts/check_i18n.test.cjs && bash scripts/ask_llm.test.sh   # ci.yml:162-168

# 生产代码 grep 门禁（dev-plan.md:197-206 / wave-6:64）
rg 'unwrap\(\)|expect\(|panic!|unimplemented!|todo!' crates/hydra-server/src crates/hydra-core/src
```

---

## 设计用例覆盖矩阵（44/44，可审计）

> 本表是"设计 §10 的每一条用例都被某个任务的红灯覆盖"的证据。**生成方式**：从设计 §10.2/§10.3 的表格里机械抽取用例 ID，再逐个回到本计划的任务红灯段落里匹配。第一轮机械检查曾发现 **13 条未被覆盖**（`T8 T9 T23 C3 C5 C11 C12 C13 C13a C14 C16 C17 C18`），其中 **C5（远端消费者停摆 → 202+lagging）是收敛屏障最核心的负例**；已全部补入对应任务。复核命令：

```bash
python3 - <<'PY'
import re
plan=open('dev-docs/aegis/plans/2026-09-17-tenant-api.md',encoding='utf-8').read()
design=open('dev-docs/design-tenant-api.md',encoding='utf-8').read()
d_t=re.findall(r'^### 10\.2 数据面集成.*?(?=^### 10\.3)',design,re.S|re.M)[0]
d_c=re.findall(r'^### 10\.3 集群.*?(?=^### 10\.4)',design,re.S|re.M)[0]
ids=[x for x in re.findall(r'^\| (T\d+[a-c]?) \|',d_t,re.M)]+[x for x in re.findall(r'^\| (C\d+[a-c]?) \|',d_c,re.M)]
parts=re.split(r'^### (T\d+) — ',plan,flags=re.M); tasks={parts[i]:parts[i+1] for i in range(1,len(parts),2)}
miss=[cid for cid in ids if not any(re.search(r'\b'+cid+r'\b', body[m.start():m.start()+1600])
      for body in tasks.values() for m in re.finditer(r'红灯', body))]
print("用例总数", len(ids), "未分配", miss)
PY
```

| 设计用例 | 覆盖任务 | 备注 |
|---|---|---|
| T1 | T5 | E1 正常路径 |
| T2 | T4 | 401 且**未触碰 DB** |
| T3 | T4 | 错令牌，文案一致 |
| T4 | T4 | 跨租户 → 403（非 404） |
| T5 | T4 | 管理 token 不跨平面 |
| T6 | T4 | 租户 token 不进管理面 |
| T7 | T5（E1）/ T6（E2）/ T8（E3） | 停用租户自救路径畅通 |
| T8 | T4 | 令牌未被当成客户端 key（`auth_url` 收 0 次） |
| T9 | T4 | 不计费、不进业务指标（含 E3 自指防护） |
| T10 | T4 | `HYDRA_TENANT_API=off` 回基线 |
| T11 | T6 | 精确 key → 强制回源 |
| T12 | T6 | 空 body = 全租户 |
| T13 | T6 | 上限 400 |
| T14 | T8 | 注入实现与 sink kind 一致 |
| T14a | T8 | **CH 引号整数（最危险）** |
| T14b | T8 | CH 空集 → `as_of: null` |
| T14c | T8 | CH 错误分类 |
| T15 | T8 | 与手写 SQL 对照 |
| T16 | T8 | 空格分隔输入归一化 + 反例 |
| T17 | T8 | 窗口上限 |
| T18 | T8 | `?tenant_id=` 被忽略 |
| T19 | T4 | 令牌只在快照 |
| T20 | T4（闸门）/ T5（E1） | `replication()==None` → 503 |
| T21 | T6 | 失败限流 429 |
| T22 | T4 | 前缀与业务路径边界 |
| T23 | T5 | `base_url` 自述一致 |
| C1 | T6 | edge E2 + 流载荷只有摘要 |
| C2 | T6 | 集群成员无 Redis → 503 |
| C3 | T6 | 单节点 → `single_node` |
| C4 | T6 | 收敛 200 |
| **C5** | **T6** | **远端消费者停摆 → 202 + lagging** |
| C6 | T6 | 先 apply 后 ack |
| C7 | T6 | generation bump 不得假报 applied |
| C8 | T6 | 屏障不是转发 |
| C9 | T6 | 死节点不阻塞收敛 |
| C10 | T6 | 失效主干边界 |
| C11 | T7 | allow TTL 封顶端到端 |
| C12 | T5 | edge E1 来自快照 |
| C13 | T8 | 集群 E3 在 edge 可用 |
| C13a | T8 | CH 不可达 → 503 非假 0 |
| C14 | T4 | 令牌索引随快照 |
| C16 | T9 | 旧路径消失（双凭证断言） |
| C17 | T6 | 重复消费幂等 |
| C18 | T4 | 数据面不暴露内部面 |

## 兼容性与风险

| 风险 | 级别 | 缓解 | 回滚 |
|---|---|---|---|
| **CH 整数被引号包裹 → 静默 0 用量** | **高** | T14a 三条断言（`"8"`/`8`/`"abc"`）；`decode_error` 指标可告警；core 的 `parse_lenient_u64` 承担唯一解析点 | 无（必须实现期覆盖） |
| **改动正在工作的 CH 写通道**（T3） | 中 | 只搬迁、零行为差异、独立 commit、既有 4 条测试 + 活实例为回归网 | 撤 T3 的 commit 即回到现状（T8 随之推迟） |
| **收敛屏障把共享 Redis 变成热点** | 低 | 水位**按批** HSET（非按事件）；等待有 `timeout_ms` 上限；E2 有每租户频率上限 | 调大轮询间隔或 `wait=none` |
| **allow TTL 封顶抬高租户认证压力** | 中 | 默认 = 现状（零变化）；`hydra_auth_allow_ttl_capped_total{tenant}` 量化影响面；`ops.md` 给出调参建议 | 调大 `ALLOW_TTL_MAX_SECS`（代价是封禁生效变慢，二者只能取一） |
| E2 返回 202 被误读为成功 | 中 | 用 202 而非 200；`state`/`lagging` 显式；`invalidate_pending_total` 可告警；文档给处置流程 | 无（语义即如此） |
| 消费者停摆节点上的陈旧 allow | 中 | T7 的硬上界 + `consumer_stalled_seconds>60s` 告警 | 调小上限 |
| `AppState` 加字段打断 13 处测试编译 | 低 | `for_tests()` 同批收敛 | 无（纯机械） |
| 前缀保留改变既有透传行为 | 低 | T22 断言业务路径逐字节不变；`HYDRA_TENANT_API=off` 完全回基线 | 总开关 |
| CH 宽窗口扫描代价 | 中 | 窗口上限（默认 31 天）+ `usage_query_seconds` | 调小上限；规模化需求走设计 Q15 |

## 退役（Retirement）

| 退役对象 | 当前状态 | 处理 | 证伪信号 |
|---|---|---|---|
| `admin/mod.rs:616-668` 租户令牌路由 | 活跃 | **T9 删除** | 旧路径返回 **404**（不是 401） |
| `admin/handlers.rs::tenant_id_for_token` | 活跃 | **T9 删除**（由 `tenant_from_token(&ConfigStore,..)` 取代） | `grep` 无残留调用；且 C16 的双凭证断言成立（租户令牌 401 / 管理令牌 404） |
| `admin/handlers.rs::tenant_auth_cache_invalidate` | 活跃 | **T6 搬进新模块**，T9 删除原位置 | 同一行为只有一个实现 |
| `InvalidateResponse::published` | 活跃 | **T6 删除**（无存量调用方） | 响应体只有 `fleet` 对象 |
| `sink.rs` 内的私有 CH 传输 | 活跃 | **T3 下沉**（搬迁，非复制） | `sink.rs` 不再含 URL 解析/请求构造 |
| `HYDRA_SETTINGS_*` 域名/URL 校验模块 | **从未存在** | 设计 v2 已移除需求 | 无（不是"留着不用"） |
| `usage_backend` 枚举 | **从未存在** | 设计 v4 已改为能力注入 | 无 |
| `HYDRA_CLUSTER_ID` | **从未存在** | 设计 v5 已删（单一集群） | 无 |

## 待确认

以下 9 项在设计 §11 给出建议值，**均非阻塞**（工程内部选择或有安全默认）；若评审要求改动，改的是本计划的对应任务而不是设计契约：

| # | 项 | 本计划采用的建议值 |
|---|---|---|
| Q1 | 前缀形态 | `/tenant/{tenant_id}/api/v1/*`（需求给定） |
| Q2 | 保留只读 `whoami` | T5 保留 |
| Q5 | 用量明细 | 不做 |
| Q6 | `tenants_by_id` 派生索引 | T2 新增 |
| Q7 | `enabled` 是否闸门 | 全不闸（只有读与缓存删除） |
| Q8 | `AppState` 迟到资源 | T4 构造后移 + `for_tests()` |
| Q9 | 管理口 `auth_url` 网段校验 | **本次不做**，建议单列小任务 |
| Q10 | E2 默认等待收敛 | `wait=converged`，预算 2s |
| Q11 | 收敛屏障位置 | 扩 `InvalidationStream`（T6） |

### 开发前基线（2026-09-17，本机实测）

开发启动前先确认"门禁当前是全绿的"，否则开发期的失败无法归因。实测结果（`RUSTFLAGS=-D warnings`、`SQLX_OFFLINE=true`）：

| 门禁 | 结果 |
|---|---|
| `cargo fmt --check` | ✅ OK |
| `cargo clippy --workspace --all-targets --features hydra-server/server -- -D warnings` | ✅ OK |
| `cargo test -p hydra-core` | ✅ OK（15 套件全绿） |
| `cargo tree -p hydra-core --no-default-features` | ✅ OK（依赖防火墙仍成立） |
| `cargo test -p hydra-server --features server` | ✅ OK |

**本地环境与 CI 的差异（已实测，必须写进每条验证命令）**：

| 依赖 | CI | 本机 | 证据 |
|---|---|---|---|
| Redis | `redis://127.0.0.1:6379` | **`redis://127.0.0.1:6380`** | `hydra-local-redis-test 127.0.0.1:6380->6379/tcp`；6379 在宿主上 `Connection refused`（`hydra-local-redis` 只暴露容器内 6379，未映射到宿主）。`tests/common/mod.rs:45-52` 对未设/不可达是**明确失败**而非跳过，所以用错端口会让集群套件直接红 |
| ClickHouse | 未在 CI 中跑 | **`http://127.0.0.1:8123`**（`hydra-local-clickhouse`，24.3.18.7，表内已有真实数据） | `curl 'http://127.0.0.1:8123/?query=SELECT%201'` → `1` |

> 因此：本文中所有照抄 `ci.yml` 的 `HYDRA_TEST_REDIS_URL=...6379` 一律以 **6380** 在本机执行；活 CH 用例用 `CH_URL=http://127.0.0.1:8123`。

## 验证与门禁

**门禁 = 上述 T10 全套命令全绿 + Playwright 全绿 + oracle 架构复核无 P0/P1 遗留 + 实现交叉审核无 P0/P1 遗留。**

```text
Execution Route:
- Decision: inline
- Evidence: 任务间有强顺序依赖（T4 依赖 T1-T3；T6 依赖 T4/T5；T8 依赖 T3；T9 依赖 T6/T8），
  且大量改动集中在同一组文件（proxy.rs / main.rs / cluster/events.rs / admin/*），
  并行子代理会在同一批文件上冲突；协调成本高于收益
- Fallback: 若某个任务被证实独立（例如 T1 与 T3 之间无文件重叠），可把该任务派给子代理
- User confirmation required: no
```
