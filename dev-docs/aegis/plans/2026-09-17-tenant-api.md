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
  （今天 `last_id` 只存在于循环局部变量里，`events.rs:293/311`）；③ CH 读通道今天完全不存在。
- Minimum change boundary: 新模块 tenant_api/（3 文件）+ clickhouse.rs（传输下沉，读写成同源）
  + usage_query.rs（双后端读）+ hydra-core 纯函数模块 + ConfigData 派生索引；**AppState 只增 3 字段**（`invalidation` / `usage` / `tenant_api`，见 T4）；
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
| 新增保留前缀 | `path.starts_with("/tenant/")` 之后**整段**不再进入代理管线（**是前缀而非三个字面路径** —— P1-2：否则 `/tenant/…/typo` 会落到 api-key 抽取与外部鉴权，把租户令牌送去 `auth_url`）。前缀下未匹配的路由本地 404。这是**唯一的行为变更面**（几乎不可能与 LLM provider 路径冲突）；`HYDRA_TENANT_API=off` 可完全回到基线 |
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
| `crates/hydra-core/src/lib.rs` | `pub mod tenant_api;`（core 无特性门控，永远编译） |
| `crates/hydra-server/src/lib.rs` | **三个新模块各自门控（P2-2）**：`#[cfg(feature = "proxy")] pub mod tenant_api;`（引用 `crate::proxy::AppState`）、`#[cfg(feature = "usage-clickhouse")] pub mod clickhouse;`（用 `tokio::io`）、`#[cfg(feature = "db")] pub mod usage_query;`（其中的 CH 半再单独 `#[cfg(feature = "usage-clickhouse")]`）。实测 `lib.rs:37-86` 里**每一个**模块都是门控的，且 `Cargo.toml` 的 `default` 为空、tokio 为 optional ⇒ 不加门控会让**无特性**的 `cargo build -p hydra-server` 编译失败，而 `dev-plan.md:201` 把 `cargo build --all` 列为出口准则 |
| `crates/hydra-core/src/config.rs` | `ConfigData.tenants_by_id` + `Default` |
| `crates/hydra-server/src/store.rs` | `build_config` 经 `reindex_tenants` 构建 `tenants_by_id` |
| `crates/hydra-server/src/cluster/snapshot.rs` | `hydrate` 之后调用同一个 `reindex_tenants` 重建派生索引（P2-3） |
| `crates/hydra-server/src/proxy.rs` | `request_filter` 第 0 步拦截；**`AppState` +3 字段**（`invalidation` / `usage` / `tenant_api` —— P1-12：v5.2 说"+2"是错的，拦截片段里的 `self.state.tenant_api.enabled` 就是第三个）；`AppState::for_tests()` |
| `crates/hydra-server/src/main.rs` | `AppState` 构造后移；注入 `usage`、`invalidation`、存活节点列表闭包、`allow_ttl_max` |
| `crates/hydra-server/src/sink.rs` | CH 传输改为调用 `clickhouse.rs`（行为逐行不变） |
| `crates/hydra-server/src/cluster/events.rs` | 水位 HASH 写入（先 apply 后 ack）、`applied_watermarks()`、`await_applied()`、generation bump 不推进水位 |
| `crates/hydra-server/src/http.rs` | allow TTL 上限 |
| `crates/hydra-server/src/admin/mod.rs` | 删除租户路由块（`:616-668`） |
| `crates/hydra-server/src/admin/handlers.rs` | 公共小工具 `pub(crate)` 化；租户逻辑搬出；`tenant_id_for_token` 删除；管理端响应改 `fleet` |
| `crates/hydra-server/src/db.rs` | 仅新增 SQLite 用量聚合查询 |
| `crates/hydra-server/src/admin/metrics.rs` | 新增指标族 |
| `crates/hydra-server/tests/{terminate_mode,streaming_usage_persistence,tls,anthropic_passthrough,metrics}.rs` | 迁移 **12 处**测试 `AppState` 构造点到 `for_tests()`（`terminate_mode.rs:206-233` 的 helper **内含** `:224`，不重复计） |
| `dev-docs/design.md` | §13.2 拆分、§11.7 指向新路径、§9.3 补 CH 读 |
| `dev-docs/ops.md` | §5.1 改路径；新增"租户 API 开通/关闭"、"租户改域名/auth_url 流程"、"CH 用量查询运维"三节 |
| `admin-ui/app.js`、`admin-ui/api-docs.js` | 租户页展示 **base URL 的路径部分**（`/tenant/{id}/api/v1`）与令牌状态（`has_access_token`）；API 文档拆分租户/运维两页。**P2-9：只能展示路径** —— 租户 API 在**数据面监听器**（8080/8443），而 admin UI 由管理口（8081）提供、管理 API 不暴露数据面主机名，故"完整 base URL"不可知；写成路径才能被 Playwright 稳定断言 |
| `crates/hydra-server/tests/tenant_cache.rs` | **部分删除（P0-2）**：删掉 5 个打旧路由的测试，保留 3 个测管理 API 令牌生命周期的测试 —— 详见 T9 步骤 0 |
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
fn decode_json_each_row_accepts_quoted_and_plain_int64() {
    // 活实例实测：{"requests":"8",...} —— 64 位整数默认被序列化成 JSON 字符串。
    // 解码器若只认 number，就会静默返回 0 用量（设计 §4.3.3 CH-A，最危险的陷阱）。
    let quoted = r#"{"requests":"8","tokens_in":"247","tokens_out":"18","cache_hit_tokens":"0","errors":"0","last_seen":"2026-09-15T07:32:06Z"}"#;
    let plain  = r#"{"requests":8,"tokens_in":247,"tokens_out":18,"cache_hit_tokens":0,"errors":0,"last_seen":"2026-09-15T07:32:06Z"}"#;
    for body in [quoted, plain] {
        let agg = decode_usage_json_each_row(body).expect("decodable");
        assert_eq!(agg.totals.requests, 8, "body={body}");
        assert_eq!(agg.totals.tokens_in, 247, "body={body}");
        assert_eq!(agg.as_of.as_deref(), Some("2026-09-15T07:32:06Z"));
    }
    // 不可解析的数字必须报错，绝不静默成 0
    assert!(decode_usage_json_each_row(r#"{"requests":"abc"}"#).is_err());
    assert!(decode_usage_json_each_row(r#"{"requests":-1}"#).is_err());
    // 缺字段也必须报错（不是 0）
    assert!(decode_usage_json_each_row(r#"{}"#).is_err());
    // 空集：CH 返回 "" 而不是 null
    let empty = decode_usage_json_each_row(r#"{"requests":"0","tokens_in":"0","tokens_out":"0","cache_hit_tokens":"0","errors":"0","last_seen":""}"#).expect("decodable");
    assert_eq!(empty.totals.requests, 0);
    assert_eq!(empty.as_of, None, "CH 的空集是 \"\" 而非 null，必须归一为 None");
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
   - **`decode_usage_json_each_row(body: &str) -> Result<UsageAggregate, DecodeError>`（本次新增，是本模块的核心交付之一）**：把 CH 的 `FORMAT JSONEachRow` 响应体解码成 `UsageAggregate`。职责边界：**逐行 `serde_json` 解析 + 用 `parse_lenient_u64` 取数 + 用 `normalize_as_of` 归一 `last_seen`**；`totals` 行是最后一行（聚合查询只返回一行；`group_by` 时前 N 行是分组行、末行是总量）。任何字段缺失/类型不可解析 → **`Err(DecodeError)`，绝不 `unwrap_or(0)`**。

   **为什么解码器必须在 core 而不是 `usage_query.rs`**（这是本任务被重新划定范围的原因，两个理由都成立）：
   1. **铁律 2**：它是纯字符串/JSON 判定，零 I/O，本该在 core 直测；
   2. **它决定"静默 0"陷阱的测试是否会在主门禁里跑**。`clickhouse.rs` 必须被 `#[cfg(feature = "usage-clickhouse")]` 门控（见 T3 的编译级缺口），而 `usage-clickhouse` **不被 `server` 隐含**。若解码器住在 `usage_query.rs`/`clickhouse.rs`，那么 T14a/T14b（引号整数、空串 `as_of`）这两条最危险的负例就**只在次级的三特性矩阵里运行**，而主门禁 `cargo test -p hydra-server --features server` 看不见它们。放进 core 之后，它们在 `cargo test -p hydra-core`（无任何特性、CI 第一个 job）里**总是**运行。

4. **Verify GREEN**：`cargo test -p hydra-core --test tenant_api` → 全绿；`cargo test -p hydra-core` → **14 套件**全绿（P2-5 实测：`ls crates/hydra-core/tests/*.rs` 今日为 **13** 个文件，本任务新增 `tenant_api.rs` 后为 14；v5.2 写的"15 套件"是错的）。

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
**⚠️ 必须先做的 serde 决策（P2-3）**：`ConfigData` **会随集群线缆传输**（`cluster/snapshot.rs:129` `pub cfg: ConfigData`，wire DTO 上带 `#[serde(deny_unknown_fields)]`）。所以朴素的"加一个 `pub` 字段"有两个后果：① 快照把租户行**传两遍**；② 混版本时要么反序列化失败，要么（加 `#[serde(default)]` 后）**静默得到空索引** → 令牌有效却被判 `403 tenant_not_found`。

**本任务采用：`#[serde(skip, default)]` + 在 hydrate 处重建**（派生索引应当被**重建**而不是被传输）：

```rust
// config.rs
/// 派生索引（与 tenants_by_domain 同源）。**不上线缆**：hydrate 时由
/// tenants_by_domain 重建 —— 既不让快照体积翻倍，也不引入混版本反序列化风险。
#[serde(skip, default)]
pub tenants_by_id: HashMap<String, Tenant>,
```

- 把构建逻辑抽成 `pub(crate) fn reindex_tenants(cfg: &mut ConfigData)`，**两个调用点共用一个实现**：
  - `store.rs::build_config`（loader 侧）；
  - **`cluster/snapshot.rs::hydrate` 内部**：函数体已有 `let mut cfg = self.cfg;`（**`snapshot.rs:283`**），就在这一行之后、构造 `HydratedWire` 之前调用 `reindex_tenants(&mut cfg)`。**必须在这个位置**：`hydrate` 返回的 `HydratedWire.cfg` 随即被 `store.rs:424-434` 的 `apply_snapshot` 交给 `ReplicationContent::from_hydrated`，所以重建必须发生在**交付之前**，而不是"apply 之后"。
- **一个必须写进代码注释的副作用**：`ReplicationContent` `derive(PartialEq)` 且持有 `Arc<ConfigData>`，而 `ConfigData` 也 derive `PartialEq` ⇒ **`tenants_by_id` 会参与"复制内容是否变化"的代际判定**。因为两侧都由同一个 `reindex_tenants` 从 `tenants_by_domain` 派生，判定仍然正确（派生字段不携带额外信息）；但它意味着实现者若在别处**手改** `tenants_by_id`（例如测试里直接 insert 却不改 domain 索引），会让代际判定出现"无源变化"。**因此：禁止在 `reindex_tenants` 之外写这个字段** —— 它只有一个写入口。
- 因此 **`cluster/snapshot.rs` 与 `store.rs` 都要进 T2 的 Files**；
- 同函数两处调用 ⇒ 不需要 `WIRE_VERSION` 变更；空索引只可能出现在"尚未收到第一帧快照"的 edge 上，而那时 `replication()` 为 `None` → 闸门回 503，语义正确。

**Impact/Compat**：`ConfigData` 是 `pub` 字段结构体，但**穷举字面量只有 2 处**（已实测核对：`grep -rn -A1 "ConfigData {" crates/` 后逐处读过）：
- `crates/hydra-server/src/store.rs:178` —— `build_config` 的返回字面量，**正是本任务要改的地方**；
- `crates/hydra-core/tests/validate.rs:275` —— 一个测试字面量，补一个 `HashMap::new()` 即可。

其余 7 处（`hydra-core/tests/{router,validate,admission}.rs`、`hydra-server/src/{proxy,cluster/snapshot}.rs`、`hydra-server/tests/load_breaker_swrr.rs`）都用 `ConfigData::default()` + 字段赋值，**新增字段不会打断它们**（另有 38 处 `ConfigData::default()` 调用点同样不受影响）。所以本任务是"1 处生产 + 1 处测试 + `Default` + loader 构建"，不是"修一片"。

**Steps**
1. **红灯（P1-11 修订：必须在 loader 上，不能在 core 里手工构造）**：

   v5.2 把这个红灯放在 `crates/hydra-core/tests/config_data.rs` 并"手工构造两个租户"——**那是无效的**：派生逻辑住在 `crates/hydra-server/src/store.rs` 的 `build_config`（唯一的 I/O 侧构建点），一个手工字面量的 core 测试只在自己断言自己，**没有任何生产改动能让它变绿**，只能靠改测试。而且 v5.2 写的"Verify RED：编译失败"**违反本计划自己的 TDD 规则**（红灯必须**因断言失败**而失败，不是因编译失败；T1 有"两段式"的说明，T2 没有）。

   正确做法 —— 红灯打在**真的拥有该派生逻辑的地方**，并且**先加字段再加断言**：

   - 步骤 1a：**先**在 `ConfigData` 上加字段（含 `Default`）→ 此时 `cargo test -p hydra-server --features server` 因 `store.rs:178` 的字面量缺字段而**编译失败**；补上 `tenants_by_id: HashMap::new()` 使其**编译通过但不填**（这是"骨架"，不是桩：它返回当下正确的行为——空索引）；
   - 步骤 1b：在 `crates/hydra-server/tests/loader.rs` 加红灯：seed 一个租户 → `ConfigStore::load` → 断言 `snap.tenants_by_id.get("<id>")` 命中且 `len()` 与 `tenants_by_domain` 一致（同时断言反向一致：每个 domain 行的 id 都在 id 索引里）。此时**因断言失败而红**（loader 还没填）；
   - 步骤 1c：`crates/hydra-core/tests/config_data.rs` 只保留**形状**断言（`Default` 出来是空索引、`serde(skip)` 不期望它在 wire 上）——不试图在那里验派生。

2. **Verify RED**：`cargo test -p hydra-server --features server --test loader` → **因断言失败而红**（不是编译失败）。
3. **GREEN**：
   - `config.rs`：`pub tenants_by_id: HashMap<String, Tenant>,` 加在 `tenants_by_domain` 之后，doc 注释写明"派生索引，与 `tenants_by_domain` 同源；不是第二份真相"；
   - `ConfigData::default()` 补 `HashMap::new()`；
   - `store.rs::build_config`：在填 `tenants_by_domain` 的同一循环里同时 `insert(t.id.clone(), t.clone())`（**同一批行、同一循环**，禁止第二次查库）。
4. **Verify GREEN**：`cargo test -p hydra-core` + `cargo test -p hydra-server --features server --test loader`（红灯转绿）+ `cargo test -p hydra-server --features server --test repo`。
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

**编译级缺口（v5.2 补，已核对 `Cargo.toml`）**：CH 侧全部代码今天受 `#[cfg(feature = "usage-clickhouse")]` 门控（`sink.rs:441-444` 等），而 **`usage-clickhouse` 不被 `server` 隐含**（`crates/hydra-server/Cargo.toml:84` 的 `server = ["db","http-client","proxy","tls-boringssl"]`；CI 的第二个 job 正因如此显式加它，见 `ci.yml:102-108`）。因此：

- `clickhouse.rs` 整体必须 `#[cfg(feature = "usage-clickhouse")]` 门控，`lib.rs` 的 `pub mod clickhouse;` 同门控；
- **无该特性时 `sink_kind` 永远不可能是 `"clickhouse"`**（`build_sink` 对不可用 kind 直接报错，启动即失败）→ 所以这条 cfg 分支是**编译期问题而不是运行期分支**；
- `usage_query.rs` 的 `ClickHouseUsageQuery` 与 `main.rs` 的选择逻辑都要有 cfg 分支（无特性时只可能注入 `SqliteUsageQuery`）；
- 验证矩阵里的 `--features server,cluster-redis,usage-clickhouse` 与 `--features server`（**无** usage-clickhouse）**两种都要编译过**——后者是 T3 最容易漏的门。
**Risk / 回归网的真相（P1-3 修订，已实测核对）**：v5.2 声称"回归网 = 既有 4 条 `clickhouse_sink` 测试"，**这是不够的**：那 4 条里 3 条非 ignored 的只调用 `build_clickhouse_json_row`（`tests/clickhouse_sink.rs:65/118/143`）——而这个函数**留在 `sink.rs` 不动**；第 4 条是 `#[ignore]` 且**不断言任何东西**（`:180` 原文 "assert no panic + graceful drop"）。

**真正覆盖本次搬迁对象的测试住在 `sink.rs` 的内联模块里，而 `--test clickhouse_sink` 根本不会运行库单测**：

| 测试 | 位置 | 覆盖什么 |
|---|---|---|
| `parse_clickhouse_url` 系列（约 7 条） | `sink.rs:996` 起的 `mod tests` | URL/凭据/查询串解析 —— **整块要搬** |
| 传输层测试（含 `a_clickhouse_that_never_answers_times_out`） | `sink.rs:1054` 起的 `#[cfg(test)]`，见 `:1226` | 连接/写/flush/读的期限语义 —— **整块要搬** |
| 活实例写库 | `tests/clickhouse_sink.rs:172-181`（`#[ignore]`） | 端到端能写进真 CH |

因此 T3 的纪律是：**内联测试模块与实现一起搬进 `clickhouse.rs`**（否则 `sink.rs` 在 `usage-clickhouse` 下**根本编译不过**——它们引用了被搬走的 `ClickHouseConfig`/`insert_batch_clickhouse_http`），并且 **Verification 必须包含 `--lib`**。

**Steps**
1. **先建回归基线**（**本计划已在 2026-09-17 预采集，搬迁后必须逐字复现**）：

```text
$ cargo test -p hydra-server --features server,usage-clickhouse --test clickhouse_sink
running 4 tests
test clickhouse_sink_writes_batch ... ignored, needs a real ClickHouse at CH_URL (e.g. http://127.0.0.1:8123)
test clickhouse_json_row_matches_usage_record_schema ... ok
test clickhouse_json_row_escapes_special_chars ... ok
test clickhouse_json_row_new_metrics_null_when_absent ... ok
test result: ok. 3 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out
```

```text
$ curl -s --data-binary "SELECT count() FROM usage_record" http://127.0.0.1:8123/     # 8
$ CH_URL=http://127.0.0.1:8123 cargo test -p hydra-server --features server,usage-clickhouse \
    --test clickhouse_sink -- --ignored --nocapture
test clickhouse_sink_writes_batch ... ok
test result: ok. 1 passed; 0 failed; 0 ignored
$ curl -s --data-binary "SELECT count() FROM usage_record" http://127.0.0.1:8123/     # 10  ← +2，证明确实写进了活 CH
```

**这条 ignored 用例不是摆设**：它真的往 `hydra-local-clickhouse` 里写了 2 行。所以搬迁的回归网是"3 条纯 JSON 形状测试 + 1 条真实写库"，覆盖面比"只有 3 条"强——复审若质疑回归网是否足够，这就是答案。

同时确认两条特性组合可编译（搬迁前基线）：`--features server,usage-clickhouse --all-targets` ✅、`--features server,cluster-redis,usage-clickhouse --all-targets` ✅、`--features server,cluster-redis --all-targets` ✅（`cargo check` 全绿）。
2. 新建 `clickhouse.rs`，**逐行**搬运以下四块（不改逻辑）。**下列事实已逐处读过源码，按它们搬运即可**：

   - **`ClickHouseConfig` 与其 URL/凭据解析**（原 `sink.rs:446-469` 定义、`:529-530` 读 env、Basic 头在请求构造处）。它有 **4 个字段**，其中两个是**已经存在的期限**：`connect_timeout`（`HYDRA_CLICKHOUSE_CONNECT_TIMEOUT_MS`，默认 3000ms）与 `io_timeout`（`HYDRA_CLICKHOUSE_IO_TIMEOUT_MS`，默认 15000ms，用于**每一次** write/flush/read）。**因此不要发明新参数**：原语直接沿用这两个字段（写路径行为逐字不变）；读路径的 `HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS` 在**构造读侧 config 时**填进这两个字段即可（例如 `connect_timeout = min(query_timeout, 3000ms)`、`io_timeout = query_timeout`）——把差异放在 config 构造处，原语本身保持唯一形态。
   - **`url_encode`**（原 `sink.rs:899-914`）。
   - **裸 TCP HTTP 原语**（原 `sink.rs:711-797`）：`POST /?<query_params>&query=<url_encode(sql)>&<param_k=v_k...>`、`Host`、可选 `Authorization: Basic base64(user:pass)`、`Content-Length`、`Connection: close`、connect/IO 期限、**以及一个已有的有界响应读取循环**。

     **签名（P1-4 修订）**：v5.2 写的 `send(cfg, body, timeout)` **两头都不成立** —— ① 有**两个独立期限**（`connect_timeout` 3000ms / `io_timeout` 15000ms，都来自 `env_millis`，`sink.rs:529-530`），一个 `Duration` 表达不了，所以"零行为差异"当场破坏；② 读路径的 SQL 与 `param_*` 绑定没有入参，读侧就得自己拼请求行 —— 那正是设计禁止的第二个 owner。正确签名是：

     ```rust
     pub(crate) async fn send(
         cfg: &ClickHouseConfig,   // 已含 connect_timeout 与 io_timeout
         query: &str,              // INSERT 或 SELECT
         params: &[(String, String)], // 读路径的 {t:String} 绑定；写路径传空切片
         body: &[u8],              // INSERT 的 JSONEachRow 载荷；SELECT 传空
     ) -> Result<(String /*status_line*/, Vec<u8> /*body*/), String>
     ```

     两个期限**留在 `ClickHouseConfig` 里**（写路径逐字不变）；读路径在**构造自己的 config 时**把 `connect_timeout`/`io_timeout` 填成 `HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS` 派生的值 —— 差异只出现在 config 构造处，原语保持唯一形态。**每个 `param_*` 的值也必须 `url_encode`**（见 T8）。
   - **状态行分类**（原 `sink.rs:801-811`）。

   **一处必须纠正的旧描述**：本计划 v5.1 曾称写通道"把响应体当错误文本丢弃"。读了源码后准确的说法是：**它已经把响应体读进来了**（`resp`，带 `MAX_CLICKHOUSE_RESPONSE = 64 KiB` 上限，`sink.rs:475`，且**不等 EOF** 就停止读取），只是**把这段 body 当作错误文本使用**。所以读路径新增的负担比原先估计的**小**：**读取与限长已存在且被测试覆盖，真正新增的只有 `FORMAT JSONEachRow` 的解码**。原语应返回 `(status_line, body)`，写路径继续把 body 当错误文本，读路径去解码它。
3. `sink.rs` 改为调用新模块：删掉搬走的实现，保留批量/flush/重试/丢弃计数逻辑不变。
4. **Verify 零差异**：
```bash
cargo fmt --check
cargo clippy --workspace --all-targets --features hydra-server/server,hydra-server/usage-clickhouse -- -D warnings
# ★ 关键：--lib 才会跑被搬走的内联测试（parse_clickhouse_url 系列 + 传输期限测试）。
#   只跑 --test clickhouse_sink 的话，本 commit 真正搬走的东西一条都没测到（P1-3）。
cargo test -p hydra-server --features server,usage-clickhouse --lib
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

**Files**：`crates/hydra-server/src/tenant_api/{mod,auth,time_bound}.rs`（新建）、`crates/hydra-server/src/proxy.rs`、`crates/hydra-server/src/main.rs`、12 处测试构造点
**本任务不交付任何端点**（P0-1）：只交付"闸门 + 路由骨架"，三个端点一律 `404 unknown path`。
**Why**：R1+R2 的落地：专用入口、数据面可达、只认租户令牌。令牌闸门读快照是"edge 也能鉴权 / 零转发"的前提。
**Change Necessity**：拦截必须进 `request_filter` 第 0 步（设计 §3.2 的 A/B/C/D 四条理由，C 条最硬：晚于 api-key 抽取会把租户令牌送去 `auth_url`）。
**Impact/Compat**：`AppState` 加 **3 字段** → **打断 12 处测试构造点**（`grep -rn "AppState {" crates/ --include=*.rs` 共 14 处 = 1 结构体定义 + 1 生产构造 + 12 测试），必须同批提供 `for_tests()` 并迁移这 12 处；`main.rs:571` 的生产构造另行后移。

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
   - **T20 在本任务的收窄形式（P0-1 修订）**：设计原文的 T20 是"`replication()==None` → E1/**E2**/**E3** 均 503"。但 T4 阶段只有闸门、三个端点都还是 404，所以 T4 **只断言闸门层**：`replication()==None` 时**令牌校验失败 → 503 `not_ready`**（而不是放行、也不是 401）。E1/E2/E3 各自的 503 形式分别在 T5/T6/T8 追加，并在其中明确写"这是 T20 在该端点的落地"。
   - **C18/内部面不外泄**：`/api/v1/internal/*` 在数据面（8080）上**不是**内部路由——它在管理口才存在（`admin/mod.rs:582-604`），断言数据面对该前缀按普通租户路径处理（既不返回内部数据、也不出现 `cluster_token` 语义）。
2. **Verify RED**：`cargo test -p hydra-server --features server --test tenant_api` → 失败（前缀未拦截，请求落到业务管线 → 断言不符）。
3. **GREEN**（按此顺序）：
   a. `AppState`（`proxy.rs:108-128`）加 **3 个字段**：

      | 字段 | 类型 | 用途 |
      |---|---|---|
      | `invalidation` | `Option<InvalidationStream>`（`not(cluster-redis)` 下设 `Option<()>` 占位，对齐 `admin/mod.rs:108-113`） | E2 的扇出与收敛屏障 |
      | `usage` | `Arc<dyn UsageQuery>` | E3 的读能力（T4 阶段先用 `SqliteUsageQuery`，T8 起按 sink kind 选择） |
      | `tenant_api` | `TenantApiConfig` | `enabled`（`HYDRA_TENANT_API`）、限流参数、**T6 的存活节点列表闭包** |

      **`for_tests` 必须能表达新测试所需的值**（P1-12）：`pub fn for_tests(pool, store, auth, breaker, limiter, sink, proxy, usage: Arc<dyn UsageQuery>, invalidation: Option<InvalidationStream>, tenant_api: TenantApiConfig) -> Arc<AppState>`。四个新增参数都**必填**而不是默认值 —— 因为 C1/C4/C6/C7 需要 `invalidation: Some(stream)`、C4/C5/C9/C10 需要真实的存活节点闭包、T10 需要 `enabled: false`；若给它们默认值，这些测试就得改回结构体字面量，`for_tests()` 的收敛意义就没了。**新测试同样只能用 `for_tests()`**（禁止新增 `AppState { .. }` 字面量，否则 §7.2 的计数校验失去意义）。
   b. 迁移 **12 处**测试构造点到 `for_tests()`：`tests/terminate_mode.rs:206-233` 的 helper（**它内含 `:224`，不要重复计**）＋ `:600/:718/:1089/:1320/:2871`、`tests/streaming_usage_persistence.rs:271`、`tests/tls.rs:166`、`tests/anthropic_passthrough.rs:302/392/506`、`tests/metrics.rs:282`。**计数校验（命令已按 T4 实测修正）**：`grep -rn "AppState {" crates/ --include=*.rs | grep -v "impl AppState" | wc -l` —— 迁移前 **14**（1 定义 + 1 生产 + 12 测试），T4 之后应为 **2**（只剩定义与 `main.rs` 的生产构造）。**注意**：裸的 `grep -c "AppState {"` 在 T4 之后会数到 `impl AppState {` 而给出 3，所以必须排除它；
   c. `main.rs`：把 `AppState` 构造**移到 `invalidation_stream` 之后**（`:621-644` 之后）；`:594` 的 `state.sink.clone()` 改为先克隆 `sink`；
   d. `tenant_api/mod.rs`：`pub async fn dispatch(state: &AppState, session, ctx) -> PingoraResult<bool>`；轻量段匹配（形制照 `admin/mod.rs:277-419`）；`respond_json`（形制照 `respond_catalog`，`proxy.rs:1280-1297`）；`TenantApiConfig::from_env()`（`HYDRA_TENANT_API=on|off`，默认 on）；
   e. `tenant_api/auth.rs`：`pub fn tenant_from_token(store: &ConfigStore, bearer: &str) -> Option<String>`。**必须在同一次 `replication()` guard 内**同时取令牌摘要表与 `tenants_by_id`（设计 §3.3 规则 4）；常数时间比较；`replication()` 为 `None` → `NotReady`（503）；
   f. `proxy.rs::request_filter` 开头（`let cfg_guard` 之前）插入：
```rust
// (0) Tenant self-service API. Intercepted BEFORE Host→tenant (otherwise an
//     IP/own-hostname call 404s), before the api-key extraction (Authorization:
//     Bearer is ALSO a client api-key transport — a tenant token must never be
//     forwarded to auth_url at http.rs:558-566 or masked into a usage record at
//     proxy.rs:1105), and before the /v1/models catalog intercept.
//
//     THE PREFIX IS RESERVED, NOT THE THREE PATHS (P1-2): gating on
//     `parse_route(..).is_some()` would let `/tenant/t1/api/v1/models`,
//     `/tenant/t1/api/v2/whoami` and every typo fall through into the normal
//     pipeline — Host→tenant, api-key extraction, external auth — which is
//     exactly the path the security invariant above forbids. So: any path under
//     `/tenant/` is answered locally (404 for a non-route), and the proxy
//     pipeline never sees that prefix.
//
//     The path is copied into an owned String on purpose (P1-1): `parse_route`
//     returns a route borrowing the path, and `dispatch` takes `&mut Session`,
//     so holding the borrow across the call is E0502.
if self.state.tenant_api.enabled
    && session.req_header().uri.path().starts_with("/tenant/")
{
    // 只有命中的请求才分配：上面那次前缀判定是**零分配**的，所以
    // 99.99% 的业务请求在这条 if 上付出的代价是一次字节比较，
    // 而不是一次 String 分配。数据面热路径不得为控制面买单。
    let path = session.req_header().uri.path().to_string();
    return crate::tenant_api::dispatch(&self.state, session, ctx, &path).await;
}
```

> **为什么不是"先取 owned path 再判前缀"**（P1-1 的第一版就是这么写的）：那样每个请求都会分配一个 `String`，把控制面路径的复杂度分摊到全部业务流量上。`str::starts_with` 在借用上完成判定、不产生所有权，因此这里的顺序是"**零分配判定 → 命中才 owned**"。

`dispatch` 内部：`parse_route(&path)` → `Some(route)` 走下面的流程；`None` → **本地 `404 unknown path`**（并且**绝不**把该前缀交给代理管线）。
   g. `dispatch` 内：令牌闸门 → URL `tenant_id` 交叉校验（403）→ 端点分派。**接线规则（P0-1 修订，v5.3）**：

      | 任务 | 本任务接线的端点 | 未接线端点的行为 |
      |---|---|---|
      | **T4** | **一个都不接** —— 只做闸门 + 路由骨架 | 三个端点全部 `404 unknown path` |
      | T5 | `Endpoint::Whoami` | 其余两个 404 |
      | T6 | `Endpoint::InvalidateAuthCache` | 剩下的 404 |
      | T8 | `Endpoint::Usage` | 无（全部接完） |

      **这条规则是被一条 P0 逼出来的，理由必须写在代码注释里**：v5.2 曾写成"T4 接上 `Whoami`"，但那让 **T5（本身就是 E1 whoami 任务）的红灯永远无法变红** —— T4 一接线，设计 T1 用例就通过了，T5 的"先红后绿"变成假的，直接违反本计划的 `TDD Route: strict` 与铁律 1。因此 **T4 不接任何端点**：它的产物是一个**诚实的路由骨架**（`match` 的三个分支都返回 `404 unknown path`），并且**每接线一个端点就删掉一个 404 分支**，到 T8 时骨架里不再有任何 404。

      **关于"这算不算桩"**：算"骨架"而不是"桩"，区别在于——骨架的每一个分支都返回**当前正确**的行为（这三个路径今天在数据面上本来就是 404），而桩返回的是"假装成功"或占位值。本计划的产物始终没有"假装成功"的分支；且这条区分已写入 §反熵核对供评审复核。T1 的 `parse_route` 仍然**一次写全**三端点（纯匹配器，完备性由 T1 单测钉住），所以 T4 的 404 是"路由已知但未提供"，不是"路由未知"。
4. **GREEN 补充：入口与闸门指标**（设计 §9.1 的前三个，`admin/metrics.rs`）：
   - `hydra_tenant_api_requests_total{endpoint,status}` —— 在 `dispatch` 出口处记一次（**标签必须低基数**：`endpoint` 是三个枚举值，`status` 是 HTTP 码）；
   - `hydra_tenant_api_auth_failures_total{reason}` —— 实现后的可达值是 **`missing|unknown|mismatch|locked`**（`locked` = 被锁定而拒绝，`throttled` 回答"拒了什么"、这个标签回答"闸门为什么没跑"）。**`tenant_gone` 不可达**：快照里有摘要却没有租户行时返回 `not_ready`(503)，那是换代中间态、不是鉴权失败；`not_ready` 也不计失败（否则可让未取到快照的 edge 烧掉调用方自己的失败预算）；
   - `hydra_tenant_api_auth_latency_seconds` —— 包住令牌校验（**这是 §3.4"零 DB I/O"断言的回归锚点**：若有人把闸门改回查库，这个直方图会立刻变粗）。

   **禁止**：这些请求不得进入 `hydra_requests_total` / `hydra_tokens_total`（那两个族按 `tenant/provider/model` 打标签且目前**无基数上限**，见设计 §9.3）。

5. **Verify GREEN**：`cargo test -p hydra-server --features server --test tenant_api`（仅闸门用例全绿）；`cargo test -p hydra-server --features server` 全绿（证明 12 处迁移无回归）。加一条断言：调用任一端点后 `hydra_tenant_api_requests_total` 的样本数增长，而 `hydra_requests_total` **不变**。
6. **Commit**：`feat(server): tenant API skeleton — data-plane prefix interception and snapshot token gate`

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
3. **GREEN**：**先把 `dispatch` 里 `Endpoint::Whoami` 的 404 分支替换为真实调用**（这是本任务红灯能变红的必要动作），再实现 `handlers::whoami(state) -> Resp`，返回 `{tenant_id,name,enabled,domain,auth_url,config_version,base_url}`；`config_version` 用 `store.version()`（`store.rs:381-385`）；`base_url = format!("/tenant/{tid}/api/v1")`。
4. **Verify GREEN**。
5. **Verify GREEN**：

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --features hydra-server/server -- -D warnings
cargo test -p hydra-server --features server --test tenant_api                        # E1 全部用例
cargo test -p hydra-server --features server,cluster-redis --test tenant_api_cluster  # C12
cargo test -p hydra-server --features server                                          # 无回归
```

6. **Commit**：`feat(server): tenant API E1 GET /whoami (snapshot-only)`

### T6 — E2 `POST /auth/cache/invalidate` + 收敛屏障（L1 扇出 + L2 权威 + L3 确认）

**Files**：`crates/hydra-server/src/tenant_api/handlers.rs`、`tenant_api/throttle.rs`（新建）、`tenant_api/mod.rs`、`crates/hydra-server/src/cluster/events.rs`、`crates/hydra-server/src/admin/handlers.rs`（管理端响应改 `fleet`）、**`crates/hydra-server/src/main.rs`**（P1-12：`spawn_invalidation_consumer(stream, auth, store)`（`events.rs:287`，调用点 `main.rs:622-630`）必须**新增 `node_id` 入参**，否则消费者无法把水位写进 `hydra:{ctl:inv:applied}` 的 HASH —— 签名与调用点都要改，`main.rs` 因此在本任务的 scope fence 内）、**`crates/hydra-server/src/admin/metrics.rs`**（6 个屏障/消费者指标）
**Why**：R3 —— **必须在全部数据面节点生效，且调用方能确知是否生效**。这是本次最核心的交付。
**Change Necessity**：今天远端节点的 L1 命中项不会因发布方删了 L2 而消失（L1 命中不查 L2），且消费者的 `last_id` 只存在局部变量里（`events.rs:293/311`：`last_id` 在循环内声明与推进）从不发布 → 系统里没有任何一处能回答"清干净没有"。
**Impact/Compat**：`DELETE /api/v1/auth/cache`（运维用）响应体由 `published: bool` 换成 `fleet` 对象；`tenant_api` 复用既有 `AuthCache`/`InvalidationStream` 原语，不新增失效机制。

**Steps**
1. **红灯**（`tests/tenant_api.rs`）：T11（精确 key → `invalidated>=1`，随后同 key 请求**必须回源**，wiremock 断言 `auth_url` 被再次调用）、T12（空 body → `scope:"tenant"`）、T13（1001 key → 400 `too_many_keys`；单个 4097 字节 → 400 `invalid_api_key`）、T21（失败限流 → 429）、**C3（单节点 `all` → `state:"single_node"`）**。
   **红灯**（`tests/tenant_api_cluster.rs`，新文件，真实 Redis）：C1（edge 上 E2：本节点 L1 清空 + 流里一条 v=2 记录且**载荷只有摘要无明文**）、**C2（P1-5 修订：原"C2 = 集群成员但无 Redis 后端"**运行期不可构造**——leader/edge 缺 `HYDRA_REDIS_URL` 会拒绝启动（`main.rs:196-201`）。改为：**有失效流但发布失败** —— 用指向**已关闭端口**的 Redis 构造流 → **503 + `fleet.state:"unavailable"`**（**没有**独立的 `error.code`：响应体仍是正常 InvalidateView，失败由 `fleet.state` 表达——早期草稿的 `fleet_invalidation_unavailable` 这个码从未实现，设计与计划均已改正），不是 `single_node`、不是假 `applied`）**、C4（两节点收敛 → 200 + `nodes_applied==nodes_total`）、**C5（远端消费者停摆 → 202 + `state:"pending"` + `lagging` 精确列出该节点，且 `consumer_stalled_seconds` 上升）**、**C6（先 apply 后 ack 的关键负例；P2-7 修订：必须确定性构造）** —— "杀掉运行中的消费者、卡在 apply 与 ack 之间"**没有确定性路径**（禁止 `#[cfg(test)]` 缝子，也禁止 mock Redis）。改为**直接测批处理步骤**：把该批的"apply 后 ack"提成 `pub(crate) fn apply_batch_then_ack(cache, events, node_id)`（`events.rs:431-478` 已有"直接测 `apply_invalidation`"的既有先例），断言 **apply 失败时不得 HSET**；反向：apply 成功时水位必须 HSET 到该批最大事件 ID。、C7（generation bump 后水位不推进，在途事件报 `pending` 而非 `applied`）、C8（屏障**不是转发**：出站无 `x-hydra-forwarded`）、C9（心跳过期节点不入 `nodes_total` 且不阻塞）、C10（两个独立 Redis 互不影响）、**C17（同一失效事件重复消费幂等，不报错）**。
2. **Verify RED** → 失败（`fleet` 字段不存在 / 水位不存在）。
3. **GREEN**：
   a. **先钉住 cfg 形状**（已实测核对）：`cluster::events` 与 `cluster::registry` 都是 `#[cfg(feature = "cluster-redis")]` 门控的（`cluster/mod.rs:23-24`、`:27-28`），因此 `InvalidationStream` 在无该特性时**根本不存在**。于是：
      - `AppState.invalidation` 必须**镜像 `AdminState` 的双字段写法**（`admin/mod.rs:108-113`）：有特性时 `Option<InvalidationStream>`，无特性时 `#[allow(dead_code)] Option<()>`；
      - `handlers::invalidate` 必须有 cfg 分支：有特性 → `publish` + `await_applied`；无特性 → **只做本地清除并回 `state: "single_node"`**；
      - **`"single_node"` 是可推导的、不是猜测**：无 `cluster-redis` 特性时 `HYDRA_ROLE=leader|edge` 会被启动检查拒绝（`main.rs:241-243` "requires the 'cluster-redis' cargo feature"），所以"无特性 ⇒ 不是集群 ⇒ 本地清除即全部"。
      - 因此**不存在**"集群成员但没有失效通道"这一状态在无特性构建里的对应物；该状态只可能出现在"编译了 cluster-redis 但没有 Redis 后端"（`main.rs:621-644` 下 `invalidation_stream = None`）→ 回 **503**（C2）。

   b. `cluster/events.rs`：新增键 `hydra:{ctl:inv:applied}`（**一个 HASH**：`node_id → last_applied_event_id`）。消费者在**成功 apply 一批之后**把该批最大事件 ID 一次性 HSET（**先 apply 后 ack**，顺序不可颠倒）；**generation bump 路径 `clear_all()` 之后不推进水位**（被裁剪的事件 ID 已不可知，不得为没读到的事件记账）。

      **接口定义（P1-5 修订 —— v5.2 的 `AppliedOutcome` 有三处不可实现，逐条给出正确形态）**：

      | # | v5.2 的问题 | 事实（file:line） | 修订后的定义 |
      |---|---|---|---|
      | ① | `SingleNode` 不可能从 `await_applied` 返回 | 它是 `InvalidationStream` 上的方法，而"没有流"恰恰意味着**没有这个对象** | `AppliedOutcome { Applied { nodes_applied, nodes_total }, Pending { nodes_applied, nodes_total, lagging: Vec<String> }, Unavailable(String) }` —— **删除 `SingleNode`**；它由**调用点**按 `state.invalidation == None` 决定（回 `state: "single_node"`） |
      | ② | `Unavailable` 的触发条件**运行期不可构造** | leader/edge 缺 `HYDRA_REDIS_URL` 直接拒绝启动（`main.rs:196-201`），故"集群成员 + 无失效通道"不存在 | `Unavailable` 重定义为**可达**的：**有流，但 `publish` 失败或水位轮询失败**（Redis 中途不可达 / 命令报错）。这也是 C2 唯一可写的构造方式 |
      | ③ | 同步闭包取不到存活节点 | `NodeRegistry::list_nodes` 是 **`async`**（`registry.rs:245`，返回 `Result<Vec<NodeStatus>, RedisError>`），且**含死节点**（`alive: false`） | 注入 `Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Vec<String>> + Send>> + Send + Sync>`（内部 `list_nodes().await`，**只保留 `alive == true`**），或注入后台任务刷新的 `Arc<ArcSwap<Vec<String>>>`。**必须显式过滤 `.alive`**，否则 C9 会静默失效 |
   c. **接线**：把 `dispatch` 里 `Endpoint::InvalidateAuthCache` 的 404 分支替换为真实调用（本任务的红灯在接线前会因 404 而失败，这就是红灯的原因）；并在本任务追加 **T20 的 E2 形式**（`replication()==None` → 503 `not_ready`）。

   d. `tenant_api/handlers.rs::invalidate`：校验/上限复用（把 `admin/handlers.rs` 的 `invalidate_shape_error`、`MAX_INVALIDATION_KEYS`、`MAX_API_KEY_LEN` 提为 `pub(crate)`）；本节点调 `state.auth.invalidate[_tenant]`；`publish`；然后 `await_applied`；按结果回 **200 / 202 / 503**；响应体按设计 §4.2 的 `fleet` 对象；`checked` 与 `invalidated` 并列。
   e. `tenant_api/throttle.rs`：固定窗口限额（源 IP / 令牌摘要 / 租户成功 / **每租户失效频率**），`cluster-redis` 下走 Redis 计数（参照 `redis/rate_limit.rs` 的窗口原语），其余用 `DashMap`。
   f. `admin/handlers.rs::auth_cache_invalidate` 响应改用同一 `fleet` 结构（**共用同一个 `await_applied` 实现**，不复制）。
   g. 存活节点列表：`main.rs` 注入 `Arc<dyn Fn() -> Vec<String> + Send + Sync>`（返回存活 node_id），与 `AdminState.leader_ready` 同一闭包注入手法（`admin/mod.rs:104`）——**不把 registry 放进 `AppState`**。
4. **GREEN 补充：屏障与消费者健康指标**（设计 §9.1 的中段六个，**这是"清干净没有"唯一可被外部观测的方式**）：

   | 指标 | 类型 | 标签 | 为什么必须有 |
   |---|---|---|---|
   | `hydra_tenant_api_throttled_total` | counter | `scope`(ip/token/tenant/invalidate) | 限流触发面；`invalidate` 那一档对应 D6 的放大防护 |
   | `hydra_tenant_api_invalidate_pending_total` | counter | — | **持续 >0 说明集群里真有节点清不掉**——202 的告警口径 |
   | `hydra_tenant_api_invalidate_converge_seconds` | histogram | `result`(applied/pending) | 收敛耗时的量化口径（典型 <100ms） |
   | `hydra_invalidation_consumer_applied_id` | gauge | `node` | 各节点已应用水位（**屏障的数据源本身**，用于交叉验证响应里的 `nodes_applied` 不是编的） |
   | `hydra_invalidation_consumer_lag_events` | gauge | `node` | 流尾与本节点水位之间的事件数 |
   | `hydra_invalidation_consumer_stalled_seconds` | gauge | `node` | **水位多久没推进**。今天这个故障完全不可见（消费者只是 `warn!` 后重试，`events.rs:336-338`） |

   `hydra_invalidation_*` 三个由消费者/屏障代码更新（而非请求路径），因此它们的更新点在 `cluster/events.rs`；`node` 标签用 `cluster.node_id`。

5. **Verify GREEN**：数据面 + 集群两套测试全绿。集群套件里加一条**交叉断言**：E2 返回 `nodes_applied: N` 时，`hydra_invalidation_consumer_applied_id` 的样本中至少 N 个节点的水位 ≥ `event_id`（**防止响应里的数字与实际水位脱钩**）。
6. **Commit**：`feat(server,cluster): tenant API E2 fleet-wide cache invalidation with a convergence barrier`

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
3. **GREEN**（**P1-6 修订：不得给 `AuthCache::new`/`with_clock` 加参数**）：

   **实测**：`AuthCache::new` / `AuthCache::with_clock` 在仓库里有 **56 个调用点、分布在 16 个文件**（约 50 个在测试里）。给它们加参数会让 T7 的"一个提交、可验证"边界当场不可能，而改动量在跑编译器之前完全看不见。

   正确做法 —— **保持 2 参数构造函数不变，用 builder 式 setter 注入上限**：

   ```rust
   // http.rs
   pub struct AuthCache { /* ... */ allow_ttl_max: Duration, /* ... */ }

   impl AuthCache {
       /// 默认上限 = allow_ttl ⇒ **所有既有调用点的语义逐字不变**。
       pub fn new(allow_ttl: Duration, deny_ttl: Duration) -> Self { /* allow_ttl_max = allow_ttl */ }
       pub fn with_clock(allow_ttl: Duration, deny_ttl: Duration, now: Clock) -> Self { /* 同上 */ }
       /// 唯一新增入口：只由 `main` 读取 `HYDRA_AUTH_ALLOW_TTL_MAX_SECS` 后调用。
       #[must_use]
       pub fn with_allow_ttl_max(mut self, max: Duration) -> Self { self.allow_ttl_max = max; self }
   }
   ```

   - `set` / `set_if_unchanged`：**仅 allow 项**套 `ttl.min(self.allow_ttl_max)`（deny 不受影响）；
   - `AuthConfig` 加 `allow_ttl_max: Duration`（`default()` = 300s），**只由 `main.rs` 使用**；
   - 新增计数 `hydra_auth_allow_ttl_capped_total{tenant}`（`admin/metrics.rs`，**该文件已在 T7 的 Files 里**）；
   - **T7 的 Files 因此只有 `http.rs` / `main.rs` / `admin/metrics.rs` + 测试，零外部改动** —— 这就是"最小改动面"的证据。

   **何时才允许改签名**：仅当该上限是**每个缓存实例必填**时才值得付 56 个调用点的代价；这里它有安全默认（= 现状值），所以不该付。
4. **Verify GREEN**：

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --features hydra-server/server -- -D warnings
cargo test -p hydra-server --features server --test http_auth      # 含 C11：expires_in=86400 被封顶
cargo test -p hydra-server --features server --test auth_cache     # 既有 TTL 语义无回归
cargo test -p hydra-server --features server --lib                 # AuthCache 内联单测（无回归）
cargo test -p hydra-server --features server                       # 主门禁全绿
# 计数校验（P1-6 的证据）：构造函数调用点数量必须与改动前一致
grep -rc "AuthCache::new\|AuthCache::with_clock" crates/ --include=*.rs | awk -F: '{s+=$2} END {print s" 个调用点（改动前基线为 56）"}'
```
5. **Commit**：`fix(auth): cap the allow-cache TTL so a stalled node's stale window is operator-bounded`

### T8 — **commit 2**：`usage_query.rs` 双后端读路径（含 CH）

**Files**：`crates/hydra-server/src/usage_query.rs`（新建）、`crates/hydra-server/src/db.rs`、`crates/hydra-server/src/tenant_api/handlers.rs`、`crates/hydra-server/src/main.rs`、`crates/hydra-server/src/admin/metrics.rs`、`crates/hydra-server/tests/usage_query.rs`（新建）
**Why**：R4 —— **生产必跑集群，用量走 ClickHouse，首发可用**。今天 CH 只有 INSERT，没有读路径。
**Change Necessity**：不做 CH 读，集群部署下这个端点就是空的（设计 v3 曾以 501 搪塞，用户已否决）。
**Impact/Compat**：纯新增读面；`UsageSink` trait **不改**（它的语义是 fire-and-forget 写入）。

**Steps**
1. **红灯**（`tests/usage_query.rs`）。**注意职责边界**：JSONEachRow 的**解码**已在 T1 交付并在 `hydra-core` 单测里覆盖（T14a/T14b 的核心断言在那里，因此总在 CI 第一个 job 运行）；本任务的红灯只覆盖**传输与接线**：
   - **T14（P1-9 修订：原写法不可写）**：注入点原本在 `main.rs` 的 `bootstrap()` 里，而它是**二进制目标** —— `tests/usage_query.rs` 根本调不到。因此**把选择逻辑提取成库函数**（本任务的一个明确交付物）：

     **签名必须 cfg 配对**（自查补：`clickhouse::ClickHouseConfig` 本身被 `usage-clickhouse` 门控，所以一个"总是存在"的 `select` 拿不到那个类型）：

     ```rust
     // usage_query.rs
     #[cfg(feature = "usage-clickhouse")]
     pub fn select(
         sink_kind: &str,
         pool: Option<&sqlx::SqlitePool>,
         ch_cfg: Option<&clickhouse::ClickHouseConfig>,
     ) -> Result<Box<dyn UsageQuery>, SelectError>;

     /// 无 CH 特性时 `sink_kind == "clickhouse"` 不可能出现（`build_sink` 会
     /// 在启动期失败），所以这个变体只可能返回 Sqlite 实现或 Err。
     #[cfg(not(feature = "usage-clickhouse"))]
     pub fn select(
         sink_kind: &str,
         pool: Option<&sqlx::SqlitePool>,
     ) -> Result<Box<dyn UsageQuery>, SelectError>;
     ```

     T14 于是断言（**按特性分两组**）：

     | 断言 | 需要的特性 |
     |---|---|
     | `select("sqlite", Some(pool), …)` → `source()=="sqlite"` | 无 |
     | `select("sqlite", None, …)` → `Err` | 无 |
     | `select("nonsense", …)` → `Err` | 无 |
     | `select("clickhouse", None, Some(cfg))` → `source()=="clickhouse"` | `usage-clickhouse` |
     | `select("clickhouse", …)` 在**无特性**构建里不可构造 → 用 `#[cfg(not(...))]` 断言"该 kind 无法被选中"（`Err`）即可 | 反向 |

     **"返回假 0"的唯一守卫因此变成一条可运行的库单测**，而不是一段测不到的 `main.rs` 分支；且它在**两种特性组合下都被编译并断言**。
     **T8 的测试命令必须带 `--features server,usage-clickhouse`**，否则 CH 那一半连编译都进不去（v5.2 的命令漏了这点）。
   - **T16 也属阻塞项**（P2-7）：它是设计 §4.3.2 第 2 条"多算一整天"陷阱的回归锚点，级别与 T14a/T14b 相同，必须与它们一起在 T8 的红灯清单里作为**阻塞**存在。
   - **T14a/T14b（端到端复验）**：用**假 CH 传输**（一个返回固定 `JSONEachRow` 字节的替身——它是**真实外部边界**，符合铁律 2 允许的 wiremock/进程级替身范畴）喂 `{"requests":"8"}` → 端点返回 8；喂 `{"requests":"abc"}` → **拒绝**（`decode_error`，**不是 0**）；喂 `last_seen:""` → `as_of: null`。纯函数层的等价断言在 T1，这里验的是"解码器确实被接上了"；
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
   c. `ClickHouseUsageQuery { cfg: clickhouse::ClickHouseConfig }`：SQL 见设计 §4.3.1，**必须用 `{t:String}`/`{s:String}`/`{e:String}` + `param_*` 绑定**（实测抗注入），`FORMAT JSONEachRow`。**两个 url 编码点都要做**（读源码得到）：① SQL 本身经 `url_encode` 进 `query=`；② **每个 `param_*` 的值同样必须 `url_encode`** —— 它们也在查询串里，而 `tenant_id`/时间戳里出现 `&`、`+`、`%` 会静默改变绑定值（`+` 在查询串里还可能被解成空格）。漏掉 ② 是"绑定了但仍被篡改"的隐蔽缺陷；`group_by` 是**白名单**映射到列名（绝不插值）；解码用 core 的 `parse_lenient_u64` 与 `normalize_as_of`（CH-A/CH-B）；错误按 `Code: N` 分类（CH-D）。
   d. `db.rs`：仅新增 SQLite 聚合查询函数。
   e. `main.rs`：按 `sink_kind` 注入 `Arc<dyn UsageQuery>`；CH 分支复用 `clickhouse.rs` 的解析（与写路径**同一次解析函数**）。
   f. 指标：`hydra_tenant_api_usage_query_total{source,group_by,result}`（含 `decode_error`）与 `hydra_tenant_api_usage_query_seconds{source}`。
4. **Verify GREEN** + 活 CH：
```bash
cargo test -p hydra-server --features server,usage-clickhouse --test usage_query
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
0. **先清掉旧路由的测试消费者（P0-2，必须在删除路由之前做）**。`crates/hydra-server/tests/tenant_cache.rs` 是旧路由在仓库里**真正的调用方**，而且它 `#![cfg(all(feature = "db", feature = "http-client", feature = "proxy"))]`（`tests/tenant_cache.rs:7`）——即它跑在**主 CI job**（`ci.yml:100` `cargo test -p hydra-server --features server`）里。直接删路由会让主门禁变红，并且**静默丢掉 5 条测试的覆盖面**。

   逐条判定（已读过该文件）：

   | 测试 | 行 | 打哪个端点 | 处置 |
   |---|---|---|---|
   | `tenant_token_set_never_echoed_and_has_flag` | `:144` | 管理 API（令牌写入/不回显/`has_access_token`） | **保留** |
   | `tenant_token_blank_keeps_and_empty_clears` | `:175` | 管理 API（blank=保留、`""`=清除） | **保留** |
   | `tenant_token_too_short_rejected_400` | `:218` | 管理 API（最短长度 400） | **保留** |
   | `tenant_token_invalidates_own_cache` | `:244` | **旧路由**（期望 200） | **迁移 → 删除** |
   | `tenant_token_invalidates_selected_keys` | `:261` | **旧路由**（期望 200） | **迁移 → 删除** |
   | `tenant_token_wrong_or_missing_rejected` | `:287` | **旧路由**（期望 401） | **迁移 → 删除** |
   | `tenant_token_mismatched_url_tenant_is_403` | `:302` | **旧路由**（期望 403） | **迁移 → 删除** |
   | `tenant_without_token_cannot_use_endpoint` | `:312` | **旧路由**（未配置令牌 → 401） | **迁移 → 删除** |

   **"迁移"不是丢覆盖面，而是换承载文件** —— 这 5 条断言的意图**已经**写在 T4/T5/T6 的红灯清单里（401/403/200 矩阵、精确 key、配额上限），只是从管理口换成数据面：

   | 被删的旧测试 | 新承载（必须逐条对应） |
   |---|---|
   | `tenant_token_invalidates_own_cache`（200） | T6 的 T11/T12 |
   | `tenant_token_invalidates_selected_keys`（200） | T6 的 T11 |
   | `tenant_token_wrong_or_missing_rejected`（401） | T4 的 T3 |
   | `tenant_token_mismatched_url_tenant_is_403`（403） | T4 的 T4 |
   | `tenant_without_token_cannot_use_endpoint`（401） | T4 的 T2（无令牌 401）＋ T5 的"令牌有效但租户行不存在/未配置 → 401" |

   **辅助函数**：`invalidate()` 助手（`:122-141`，打旧路径）随 5 条测试一起删除；`admin_state()`/`start_admin()`/`create_tenant()`/`client()`/`wait_ready()` **保留**（3 条管理 API 测试与后续管理面测试仍需要）。
   **文件头部模块文档**（`:1-5`）也要改：它现在描述的是"租户令牌路由 + 管理侧令牌生命周期"，应改为只描述后者，并指向新位置（数据面租户 API 的测试在 `tests/tenant_api.rs`／文档在 `design-tenant-api.md`）。

   **验证本步**：`cargo test -p hydra-server --features server --test tenant_cache` → 期望 **3 passed / 0 failed**（删 5 条后；**3 是步骤 0 的中间态**——步骤 2 加的 C16 双凭证断言会让终态变成 **4 passed**，不要把它当回归）；此时旧路由仍在，主门禁仍绿。

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
3. **反熵核对（P2-1 修订：原断言写得太满，会误报）**：`grep -rn "tenant_id_for_token\|auth/cache/invalidate" crates/` 的真实结果**不是空的**——除新模块与测试外还有：

   | 残留位置 | 性质 | 处置 |
   |---|---|---|
   | `crates/hydra-server/src/cluster/content.rs:28` | **注释**里提到旧路径 | 改成指向新路径或删掉该句 |
   | `crates/hydra-server/tests/cluster.rs:799` | **测试注释** | 同上 |
   | `crates/hydra-server/tests/tenant_cache.rs:2,131` | 本任务步骤 0 已删 | 步骤 0 完成后自然消失 |

   因此**正确断言是"无生产调用点"而非"零匹配"**：`rg -n "tenant_id_for_token" crates/` **必须为空**（该函数被删）；`auth/cache/invalidate` 只允许出现在注释与测试里。

   **另外三处 UI 文档腐化必须一并处理**（`check_i18n.js` 只校验键集合、不校验文案，抓不到）：`admin-ui/api-docs.js` 的旧条目，以及 **`admin-ui/i18n.js` 4 个语言里引用旧路径的 `tip.blankKeep` 文案**（约 `:203/:539/:875/:1211`）。
4. **`ops.md` 必须新增的一条告警规则**（设计 §4.2.6 要求，属本任务的交付物）：`hydra_invalidation_consumer_stalled_seconds > 60` 即告警 —— 这条规则是"活着但不消费的节点"唯一的外部信号，而它依赖 T6 交付的 gauge；若 T6 没交付该 gauge，本步**必须发现并回退到 T6**，不得只写文档。
5. 文档：`design.md` §13.2 端点表拆分（租户端点移出「管理 Web API」）、新增「租户自助 API（数据面）」节、§11.7 表格指向新路径、§9.3 补 CH 读；`ops.md` §5.1 改路径 + 新增三节（租户 API 开通/关闭、**租户改域名/auth_url 的运维流程**、**CH 用量查询运维**：`requests` 为近似值的成因、宽窗口代价、`ORDER BY` 建议）。
5. 前端：`app.js` 租户页展示 base URL 与令牌状态；`api-docs.js` 拆分为"租户 API / 运维 API"两页。
6. **Playwright（强制）**：见 T10。
7. **Verify + Commit**：

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --features hydra-server/server -- -D warnings
cargo test -p hydra-server --features server                       # 主门禁：删路由后必须仍全绿（P0-2 的验收形式）
cargo test -p hydra-server --features server --test tenant_cache   # 4 passed / 0 failed（3 条令牌生命周期 + C16 双凭证）
cargo test -p hydra-server --features server --test tenant_api     # C16 双凭证断言
rg -n "tenant_id_for_token" crates/                                # 期望为空
node scripts/check_i18n.js && node --test scripts/check_i18n.test.cjs
npx playwright test --config=playwright.config.cjs                 # 浏览器腿
```

8. **Commit**：`refactor(admin,docs,ui)!: delete the admin-plane tenant route; document the data-plane tenant API`

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

# 生产代码 grep 门禁 —— 必须限定到本次新增/改动的文件（P1-10）
# 实测：对 crates/hydra-server/src + crates/hydra-core/src 全量跑该模式，**今日即命中 358 行**
# （含 main.rs:909 一处真实的生产 expect，以及大量位于 src/ 内联 #[cfg(test)] 模块里的断言）。
# 全量要求"为空"是**永远无法变绿**的门禁，会逼开发者删掉合法断言或干脆不再信任它。
rg 'unwrap\(\)|expect\(|panic!|unimplemented!|todo!' \
   crates/hydra-core/src/tenant_api.rs crates/hydra-server/src/tenant_api/ \
   crates/hydra-server/src/usage_query.rs crates/hydra-server/src/clickhouse.rs
# 期望为空。**基线例外（已记录）**：main.rs:909 的
#   .expect("the cert store is built whenever a TLS listener is configured")
# 是既有生产代码、不属本次改动，**不得**为过门禁而改它。
# 对本次改动的既有文件另用 diff 复核新增行：
#   git diff -U0 <base> -- crates/hydra-server/src/proxy.rs crates/hydra-server/src/main.rs \
#       crates/hydra-server/src/http.rs crates/hydra-server/src/sink.rs \
#       crates/hydra-server/src/cluster/events.rs crates/hydra-server/src/admin/mod.rs \
#       crates/hydra-server/src/admin/handlers.rs | rg '^\+.*(unwrap\(\)|expect\(|panic!)'   # 期望为空
```

---

## 设计用例覆盖矩阵（44/44，可审计）

> **编号说明（P2-6）**：设计 §10.3 的编号**跳过 C15**（实际是 C1–C14、C13a、C16–C18，共 **17** 行），§10.2 是 T1–T23 加 T14a/b/c（共 **26** 行）。本文因此不再笼统写"集群 18 条 / 数据面 23 条"，而按实测行数表述；**单节点的用例编号是 C3**（v5.2 一度写作"T24"，设计里不存在该编号，已改正）。

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
| C2 | T6 | **有流但发布失败** → 503（原措辞不可构造，见 T6） |
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

## 指标交付矩阵（12/12）

> 设计的 §9.1 声明 12 个新指标。第一轮机械核对发现 **9 个在计划里没有任何归属任务**（含 6 个收敛屏障/消费者健康指标），已全部补入任务步骤。核对方式：抽取设计 §9.1 表格第一列的指标名，逐个回查计划是否出现。
>
> ⚠️ **本矩阵第一版核的是"有没有归属任务"，不是"有没有真的实现"—— 2026-09-17 的 T10 交付核对因此发现 9 个指标在代码里根本不存在**（相同的核对方式、不同的对象：`grep '"<metric>"' crates/hydra-server/src/`）。补实现见 `## 指标交付核对（交付期，可重跑）`；**矩阵本身也加了交付期核对命令**，否则"归属 ≠ 交付"这个缺口会再次静默复发。

| 指标 | 交付任务 | 更新点 |
|---|---|---|
| `hydra_tenant_api_requests_total` | T4 | `dispatch` 出口 |
| `hydra_tenant_api_auth_failures_total` | T4 | 闸门失败点 |
| `hydra_tenant_api_auth_latency_seconds` | T4 | 包住令牌校验 |
| **响应字段 `checked`** | T6 | 断言它与 `invalidated` **并列返回**（P2-7：v5.2 只把它写进契约示例，**没有任何测试断言它存在** —— "契约里有、测试里没有"的字段） |
| `hydra_tenant_api_throttled_total` | T6 | 限流器 |
| `hydra_tenant_api_invalidate_pending_total` | T6 | E2 返回 202 的分支 |
| `hydra_tenant_api_invalidate_converge_seconds` | T6 | `await_applied` 返回后 |
| `hydra_invalidation_consumer_applied_id` | T6 | 消费者 ack 后（**屏障数据源**） |
| `hydra_invalidation_consumer_lag_events` | T6 | 消费者每批 |
| `hydra_invalidation_consumer_stalled_seconds` | T6 | 消费者每批（**ops.md 告警依据**） |
| `hydra_auth_allow_ttl_capped_total` | T7 | `AuthCache::set` 封顶分支 |
| `hydra_tenant_api_usage_query_total` | T8 | E3 出口（含 `decode_error`） |
| `hydra_tenant_api_usage_query_seconds` | T8 | E3 查询包裹 |

## 兼容性与风险

| 风险 | 级别 | 缓解 | 回滚 |
|---|---|---|---|
| **CH 整数被引号包裹 → 静默 0 用量** | **高** | T14a 三条断言（`"8"`/`8`/`"abc"`）；`decode_error` 指标可告警；core 的 `parse_lenient_u64` 承担唯一解析点 | 无（必须实现期覆盖） |
| **改动正在工作的 CH 写通道**（T3） | 中 | 只搬迁、零行为差异、独立 commit、既有 4 条测试 + 活实例为回归网 | 撤 T3 的 commit 即回到现状（T8 随之推迟） |
| **收敛屏障把共享 Redis 变成热点** | 低 | 水位**按批** HSET（非按事件）；等待有 `timeout_ms` 上限；E2 有每租户频率上限 | 调大轮询间隔或 `wait=none` |
| **allow TTL 封顶抬高租户认证压力** | 中 | 默认 = 现状（零变化）；`hydra_auth_allow_ttl_capped_total{tenant}` 量化影响面；`ops.md` 给出调参建议 | 调大 `ALLOW_TTL_MAX_SECS`（代价是封禁生效变慢，二者只能取一） |
| E2 返回 202 被误读为成功 | 中 | 用 202 而非 200；`state`/`lagging` 显式；`invalidate_pending_total` 可告警；文档给处置流程 | 无（语义即如此） |
| 消费者停摆节点上的陈旧 allow | 中 | T7 的硬上界 + `consumer_stalled_seconds>60s` 告警 | 调小上限 |
| `AppState` 加字段打断 **12 处**测试编译（另 1 处是 `main.rs` 的生产构造） | 低 | `for_tests()` 同批收敛；**校验命令（唯一有效形态）**：`grep -rn "AppState {" crates/ --include=*.rs \| grep -v "impl AppState" \| wc -l` → 期望 **2**（结构体定义 + `main.rs` 生产构造）。裸 `grep -c "AppState {"` 会数到 `impl AppState {` 给出 3；**14 是改造前的历史值**，不是任何阶段的期望终态 | 无（纯机械） |
| 前缀保留改变既有透传行为 | 低 | T22 断言业务路径逐字节不变；`HYDRA_TENANT_API=off` 完全回基线 | 总开关 |
| CH 宽窗口扫描代价 | 中 | 窗口上限（默认 31 天）+ `usage_query_seconds` | 调小上限；规模化需求走设计 Q15 |

## 退役（Retirement）

| 退役对象 | 当前状态 | 处理 | 证伪信号 |
|---|---|---|---|
| `admin/mod.rs:616-668` 租户令牌路由 | 活跃 | **T9 删除** | 旧路径返回 **404**（不是 401） |
| `admin/handlers.rs::tenant_id_for_token` | 活跃 | **T9 删除**（由 `tenant_from_token(&ConfigStore,..)` 取代） | `grep` 无残留调用；且 C16 的双凭证断言成立（租户令牌 401 / 管理令牌 404） |
| `admin/handlers.rs::tenant_auth_cache_invalidate` | 活跃 | **T6 搬进新模块**，T9 删除原位置 | 同一行为只有一个实现 |
| **`tests/tenant_cache.rs` 的 5 条旧路由测试** | **活跃**（跑在主 CI job 里） | **T9 步骤 0 删除**，断言意图迁到 `tests/tenant_api.rs`（映射表见 T9 步骤 0） | 终态 `--test tenant_cache` 为 **4 passed**（3 条令牌生命周期 + C16；删除后、加 C16 前的**中间态是 3**）；且 5 条意图在 `tenant_api` 套件里各有对应断言（否则覆盖面静默下降） |
| `tests/tenant_cache.rs` 的 `invalidate()` 助手 | 活跃 | 随 5 条测试一起删除 | 编译通过即证明没有残留调用 |
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
| `cargo test -p hydra-core` | ✅ OK（**13 个测试文件**全绿；T1 之后为 14） |
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

---


### T5 — E1 `GET /whoami`（`8b5395a`）

| 项 | 结果 |
|---|---|
| 交付 | `tenant_api/handlers.rs`（`whoami`）；闸门现在同时返回它所依据的快照版本（`AuthenticatedTenant.config_version`），响应报告**同一个值**——取一次新鲜的 `store.version()` 会让响应归属于比授权它更新的一版配置 |
| 暴露字段 | 租户自己的 id/name/enabled + 绑定的域名与 auth_url（只读；修改仍是运维动作）+ config_version + base_url |
| **TDD 证据** | **RED 7 failed**：5 条新 E1 测试 + 2 条 T4 里用 404 当"已鉴权"标记的断言，失败原因全部是**我们自己的骨架 404**（正确原因，非 setup 错误）→ **GREEN 15 passed** |
| 亮点 | C12（edge 无 DB）：测试构造**无 pool 的 `ConfigStore`** 并喂一帧 `HydratedWire`，走的是真实副本路径（`apply_snapshot`）而非数据库；版本 7 从线缆进到响应，证明"E1 不需要 DB、不需要转发" |

### T6 — E2 全集群清除 + 收敛屏障（`0a81aa3`）

| 项 | 结果 |
|---|---|
| 交付 | 屏障（`events.rs`：每节点水位键 + 单次 MGET、数值化流 ID 比较、apply-then-ack、generation bump 不推进水位、只等存活节点）；E2 handler（三层语义 + 200/202/503 + `fleet` 报告）；限流器（`throttle.rs`，4 条单测）；routing 层的方法契约（GET → 405）；`main.rs` 注入存活节点闭包（后台刷新，避免在异步上下文里 `block_on`） |
| **测试抓到的两个真 bug** | ① **`await_applied` 会挂死租户请求**：截止检查只在两次轮询之间，Redis 客户端对死连接重试时单次 `await` 永不返回 → 每次轮询改为受剩余预算约束，未完成的轮询报 `Unavailable`（fail-closed）。由"指向已关闭端口"的测试**把整个测试二进制跑超时**发现。② **`invalidation == None` 的语义反了**：原实现报 `unavailable`(503)，导致**编译了 cluster-redis 的单节点部署**对一次实际已全局成功的清除回 503。"无流"只可能是单节点 `all` 角色（`main` 只在没有 Redis 主干时为 `None`，而 leader/edge 缺它就拒绝启动）→ 改为 `single_node`，`Unavailable` 保留给"有流但失败"。**只有三特性矩阵能暴露它**——`--features server` 两种实现都通过，这正是计划强制跑两者的原因 |
| 顺带删除 | `constant_time_eq` 被写了两份（admin 一份、我一份）→ 合并为一份，租户闸门复用 admin 的实现 |
| **TDD 证据** | E2 接线被改回 404 后重跑：4 条新测试中 **3 条失败**（第 4 条 `invalidate_is_post_only` 测的是 routing 的方法契约，删掉方法校验它才变红——已记录为"与接线无关但非空断言"） |
| 已声明的边界 | 限流窗口是**每进程**的，N 节点舰队允许 N 倍速率；它仍然约束了每个节点的发布速率（驱动 generation bump 的那个量），共享窗口是明确的后续项 |
| 门禁 | fmt clean；两种 clippy 组合 clean；三特性 **32 target 全绿、0 失败**；core 16；数据面新套件 19 条；集群新套件 7 条（**真实 Redis**，helper 在未设 `HYDRA_TEST_REDIS_URL` 时 panic，因此永不静默跳过） |

### T7 — allow TTL 硬上界 `HYDRA_AUTH_ALLOW_TTL_MAX_SECS`（本提交）

| 项 | 结果 |
|---|---|
| 交付 | `AuthCache.allow_ttl_max` + builder `with_allow_ttl_max`（**不加构造参数**：默认 = `allow_ttl`，56 个调用点全部照旧编译）+ `allow_ttl_max()`/`capped_allows()`；上限在 `set` 里应用（**唯一写入口**，`set_if_unchanged` 委托它 ⇒ 两条路径不可能漂移）；`AuthConfig.allow_ttl_max`（默认 300s）+ `DEFAULT_ALLOW_TTL_MAX_SECS`；`main.rs` 单点构造 + `allow_ttl_max_from_env()`；指标 `hydra_auth_allow_ttl_capped_total{tenant}` |
| **TDD 证据（严格断言级 RED）** | 先放"能编译但错误"的骨架（存下 cap 但无人读、计数器恒 0、指标恒 0.0）⇒ **2 passed / 3 failed，全部是断言失败**（非编译失败）。两条通过的是**负向对照**（deny 不被封顶、默认 cap 不改变正常 allow），它们本来就该通过 |
| **L2 是这条上界的真正战场** | 设计写的是"L1/**L2** 的 allow 项上限"，而 `--lib` 单测够不到 L2。补一条真 Redis 测试 `a_capped_allow_has_a_capped_l2_ttl`，并用**注入缺陷**验证其判别力：把 `set` 改成"L1 用 clamp 后的 ttl、L2 收原始 ttl"（只封一半）⇒ 该测试**失败并打印 `got 86400s`**，而 L1 侧 5 条**全绿**。即：**若只有 L1 测试，"cap 只在单节点成立、集群里 L2 会把长 TTL 喂回来"这个缺陷会整个漏过** |
| 测试里抓到的一处**假隔离** | `a_capped_write_is_counted` 原断言 `allow_ttl_capped_total("t1") == 1.0`：单跑通过、**在模块内失败**。原因是 prometheus 计数器是**进程全局且单调**的，同模块其它测试也在为 `t1` 记账。已改为唯一 tenant + **增量断言**（`after - before`）。这不是计数器坏了，是我的断言假设了不存在的隔离 |
| 为什么**不**在 L2 回填路径上也 clamp | 回填的 TTL 来自 Redis 的 `PTTL`，而**写入 L2 的就是 clamp 后的值**（`PX(ttl)`，已由上面那条测试钉住）。在回填处再 clamp 只会让 L1 条目过期后立刻从 L2 重新填回，**不改变实际生效窗口**，却制造"已经过期了"的错觉。真正的上界由"写入时封顶"保证：`stalled` 节点的 L1 与从 L2 回填的 L1 都不超过 cap |
| 已声明边界 | 调低该开关**不会**追溯缩短已写入 Redis 的长 TTL 项（TTL 变更的固有语义）；要立刻生效用 E2 失效。`expires_in` 更长的租户会产生更多 `auth_url` 回源（§4.2.5 已写的取舍） |
| 门禁 | fmt clean；`clippy -D warnings` **两种**特性组合 clean；core 16 target 全绿；`--features server` **31 target 全绿**；三特性矩阵 **33 target 全绿、0 失败** |

### T8 — 双后端读路径（含 ClickHouse）+ E3 `GET /usage`（本提交）

| 项 | 结果 |
|---|---|
| 交付 | `usage_query.rs`：`ClickHouseUsageQuery`（两次查询、`{t:String}` 绑定、chunked 解帧）+ `select`（**签名在两种特性组合下一致**）；`tenant_api/time_bound.rs`（新建：RFC3339/epoch/空格形态的词法解析 + 归一化 + 窗口判定 + 百分号解码 + 查询串解析）；E3 handler + dispatch 接线；`TenantApiConfig.usage_max_window_days`（`HYDRA_TENANT_API_USAGE_MAX_WINDOW_DAYS`）；指标 `hydra_tenant_api_usage_query_total{source,group_by,result}` + `_seconds{source}`；`AppState::for_tests_with_usage`；`main.rs` 用 `select` 注入（不再是临时 `.ok()`）；core 新增"仅行"解码器 `decode_usage_rows_json_each_row`；`tests/usage_query.rs`（**28 条**，含 1 条活 CH `--ignored`） |
| **实测新事实 CH-G（推翻了设计原先的假设）** | 设计 §4.3.1 假定"分组时一条查询返回：分组行在前、总计行在后"。**实测不成立**：`UNION ALL` 的分支顺序不保证 —— 同一语句 6 次里 **1 次把总计行排在最先**；补 `ORDER BY is_total` **无效**（`UNION ALL` 之后的裸 `ORDER BY` 只绑定到最后一个分支）。对照实验：两条语句**分开**执行时 6/6 与 3/3 全稳定。**若按位置解码，会把某个分组的数字当成整体总计**（单分组时数字完全相同，肉眼不可辨）。→ 读侧改为**两次查询**（与 SQLite 臂同构），core 新增"仅行"解码器；分组查询无 `last_seen` 列，且无匹配行时实测返回**空体**（与总计查询的"必有 `last_seen`"语义不同，故为两个函数） |
| **实测新事实 CH-H（只有活实例能发现）** | CH 的响应是 **`Transfer-Encoding: chunked` 且无 `Content-Length`**（实测响应头）。按 `\r\n\r\n` 切出的"body"是 `7C\r\n{…}` → JSON 报 `trailing characters at line 1 column 2`。**26 条 wiremock 替身测试全绿，只有 `--ignored` 的活 CH 用例失败** —— 这是"替身必然掩盖传输层事实"的教科书例子。→ 传输层新增 `clickhouse::response_body`（解 chunked，容忍被 64 KiB 上限截断），并让写路径的报错文本一并受益；新增 5 条 `--lib` 单测钉住它 |
| 分组查询的其它实测 | 分组行顺序稳定（6/6），但仍用既有的 `rows_by_key` 做**顺序无关**比较，不把 SQL 排序当契约；`MAX(created_at)` 空集为 `""`（CH-B 复现）；绑定值 `x' OR 1=1 --` 原样返回（CH-C 复现） |
| **TDD 证据（注入 A：E3 摘回 404 骨架）** | **9 passed / 17 failed**。仍然通过的 9 条**恰好是**不需要端点的能力断言（5 条 `select_*`、3 条 SQLite reader 级、1 条 CH 绑定 reader 级）；**每一条端点契约测试都失败**。这正是"失败原因正确"的形态 |
| **TDD 证据（注入 B：把读失败变成"假 0 的 200"）** | **22 passed / 4 failed**，失败的 4 条**全部**是"禁止假 0"断言（非数字计数、缺 `last_seen`、404+`Code:60`、端口指错）。即：本设计的最高价值属性有 4 条独立测试守着 |
| 计划外新增测试 | `clickhouse_a_5xx_is_a_store_failure_too`（T14c 原文要求"HTTP 5xx/连接超时 → 同码"，原先只覆盖了 404 与连接失败） |

### T8 执行期对计划的修正（4 处）

| # | 计划所写 | 实际 | 处理 |
|---|---|---|---|
| 1 | `select` 用 **cfg 配对的两个签名**，CH 版收 `Option<&clickhouse::ClickHouseConfig>` | 该类型是 `pub(crate)`，`pub fn` 拿不到（计划只发现了"特性门控"这一半问题，没发现**可见性**这一半）；而且两个 arity 会让 `main` 与测试各自 cfg 分叉 | **偏离计划**：签名**统一**为 `select(kind, pool, ch_url: Option<&str>)`，只门控 `"clickhouse"` 分支。传 **URL 字符串**同时解决可见性与 arity，且不让本函数成为第二个"CH URL 怎么解析"的 owner（解析仍在 `clickhouse.rs`）。无 CH 特性时该 kind 落到 `UnknownKind` ⇒ T14 的"反向断言"用同一行代码表达 |
| 2 | `UsageQueryError::WindowTooLarge { max_days }` | 窗口长度判定需要日历运算，设计 §7.1 已明确它属于 `time_bound`（shell），而窗口上限是 `TenantApiConfig` 的配置 | **偏离计划**：`WindowTooLarge` 只存在于 `time_bound::BoundError`，handler 在**触碰存储之前**就回 400；`UsageQueryError` 保持 `StoreUnavailable` / `Decode` 两态 |
| 3 | 解码用 core 的 `parse_lenient_u64` 与 `normalize_as_of` | 这两者已被 `decode_usage_json_each_row` 封装，而该函数要求"最后一行是总计"——CH 上**不可靠**（CH-G） | **偏离计划（更优）**：分组体走**新增的** core 函数 `decode_usage_rows_json_each_row`，解码逻辑仍由 core 单一持有（不是把解析逻辑抄进 server），并给原函数补了"**不要喂它 `UNION ALL`**"的文档警告 |
| 4 | 测试里有"喂 `{"requests":"8"}` → 端点返回 8" | 端点级注入需要一个能塞进 CH reader 的构造点，`for_tests` 只会从 `store.pool()` 派生 | 新增 `AppState::for_tests_with_usage`（`for_tests` 委托它）。**它不是第二条选择路径**：调用方必须用 `select` 拿到 reader，即与 `main` 同一个函数 |

### T8 里我自己的 3 处测试错误（均由测试自己暴露）

| # | 我写错的地方 | 真相 | 修正 |
|---|---|---|---|
| 1 | T16 的"反例"断言：空格形态的 `since` 会多算一整天 | **只在边界不是午夜时成立**：午夜的 `2026-09-16 00:00:00` 与规范形态逐字符比较结果相同（日位先于分隔符），所以我的 fixture 恰好选了唯一不暴露问题的时刻 —— **反例本身是断言不成立的**。实测确认后改用 `12:00:00` | 反例改为非午夜边界，并**同时断言两个方向**（端点 1 行 / 朴素比较 2 行） |
| 2 | T18 忘了给主租户配令牌 → 401 | 断言 200 却得到 401，读消息才看出是 fixture 缺令牌 | 补 `seed_tenant(T, …, TOKEN)` |
| 3 | 绑定断言走**端点**，用百分号编码的租户 id 拼路径 | 路由器不做路径解码，`route.tenant_id` ≠ 认证租户 → 403，请求根本没到 CH（测的是路由器，不是绑定） | 断言下移到 **reader 级**（SQL 真正被构造的地方），并加"`&` 不得作为分隔符存活"的断言 |

> 另记一条**操作教训**：一次 `python replace` 未命中且**没有断言**，导致"请求已改、断言未改"的半成品状态 —— 表现为**单跑通过、并行偶发失败**（进程全局计数器 + 标签组合不唯一）。凡 `replace` 必断言出现次数，与既有"多 rep 脚本失败即整体不写盘"的教训同类。
>
> 另记一条**门禁教训**：`clickhouse.rs` 被 `usage-clickhouse` 门控，因此 `clippy --features server` **根本看不到它**；`while_let_loop` 只在**三特性 clippy** 下报出。矩阵不是形式主义。

### T9 — 删除旧路由 + 管理面 `fleet` 报告 + 文档 + 前端（本提交）

| 项 | 结果 |
|---|---|
| **步骤 0（先做，P0-2）** | `tests/tenant_cache.rs` 是旧路由在仓库里**唯一真正的调用方**，且跑在**主 CI job**。按计划先删 5 条旧路由测试 + `invalidate()` 辅助函数、改写模块文档：**8 → 3 条**（与计划预测一致），此时旧路由仍在、主门禁仍绿 |
| **C16 双凭证断言（先红后绿）** | 删除**前**实测：租户令牌打旧路径 → **200** + `{"invalidated":0,"tenant_id":"t-gone","published":true}`（**`published: true` 是谎报**，D3 的现场）；管理令牌 → 404。删除**后**：租户令牌 → **401**（落到 admin 闸门），管理令牌 → **404**。测试**同时断言两条**并写明：只有"租户令牌 200→401"是证伪信号，"404"两边相同、单独断言就是永远测不出东西的测试 |
| 删除 | `admin/mod.rs` 的租户令牌路由整块（2372 字符）；`handlers.rs` 的 `tenant_id_for_token` + `tenant_auth_cache_invalidate` + `TenantCacheInvalidateRequest`（92 行）。**保留**了 `invalidate_shape_error` / `MAX_INVALIDATION_KEYS` / `MAX_API_KEY_LEN`（数据面复用），并显式核对未被误删 |
| **管理面响应改 `fleet` 三态** | `published: bool` **删除**。它初值 `true`、仅当"有失效流且发布失败"才为 `false` ⇒ **没有失效流时它会断言一次并未发生的广播**。改为与租户面**同一个** `cluster::events::broadcast_and_confirm` 产出的 `FleetReport`，状态码因此也有 200/202/503 三态。实测响应：`{"invalidated":0,"checked":0,"tenant_id":"t-e2e","scope":"tenant","fleet":{"state":"single_node","nodes_total":1,"nodes_applied":1,"lagging":[],"event_id":null,"waited_ms":0}}` |
| `AdminState` 的注入方式 | 新增 `live_nodes` + `converge_timeout`，**用 builder（`with_fleet`）而不是给 `new` 加参数**：该构造函数有 **14 个调用点**（与本计划 P1-6 对 `AuthCache` 的判定同一理由）。`main` 把**同一个**存活节点视图同时给租户面与管理面——两份视图会对"舰队是谁"给出不同答案，而屏障的全部意义就是两个调用方拿到同一个答案 |
| 前端 | `api-docs.js`：删除旧租户条目 + 管理端条目补 `fleet` 三态与新错误码；`app.js`：`published` 布尔改为展示 `fleet` 状态（`applied`/`single_node` 成功样式，`pending`/`unavailable` 错误样式 + 落后节点列表）；`i18n.js`：**4 种语言**各 5 条新文案，并**删除 4 条已失效的 `apidocs.summary.POST.api.v1.tenants…` key**；`tip.blankKeep` 的示例路径改为数据面 |
| 一处"检查器看不见的 key" | 我最初用模板字符串 `t(\`common.token.fleet.${state}\`)` 拼 key —— `scripts/check_i18n.js` **看不到**运行时拼出的 key，于是报了 5 个 `DEAD-EN-KEY`。改为**显式映射表**（`FLEET_KEYS`）：既是检查器可见的，也避免"某个 key 在另外三种语言里缺失而无人发现" |
| 文档 | `design.md` §13.2（管理端响应 + 租户自助段整体改写为数据面三端点）、§11.7 表格、§9.3/§1500 附近的列表；`ops.md` §1.1 补 **6 个新环境变量**、§5 响应体更新、**§5.1 重写为数据面自述**、**新增 §5.2（残余窗口上界）/ §5.3（两种用量口径及其各自漏算）/ §5.4（E3 排障表）** |
| **Playwright（T10 的一部分，本提交一并取得）** | 用 release 二进制 + 真 SQLite + `tests/e2e/seed.sh` 起真服务，**15/15 通过**，含 **T2.4 auth-cache invalidate** —— 它真实走到改写后的 `app.js` 与新的 `fleet` 响应体（原断言 `toContainText(/Invalidated 0/)` 仍成立，因为失效条数文案未变） |
| 手工验收证据（活实例） | `DELETE /api/v1/auth/cache` 返回上面的 `fleet` 体；旧路径 **管理令牌 → 404 / 租户令牌 → 401**；`git status` 无残留（Playwright 装到工作区后被清理，未污染仓库根） |
| 门禁 | fmt clean；`clippy --workspace --all-targets` **两种组合** clean；`-p hydra-core` 16 target；`--features server` **全绿**；三特性矩阵 **全绿**；`check_i18n.js` `OK (338 en keys, 4 locales, code↔en consistent)`；`check_i18n.test.cjs` 通过；`ask_llm.test.sh` 通过；依赖防火墙按 CI 原样命令通过 |

> **留档的操作事实**：本地 `8080/8081` 已被开发实例占用，e2e 必须换端口（`HYDRA_LISTEN=127.0.0.1:18080 HYDRA_ADMIN_ADDR=127.0.0.1:18081`）；Playwright 浏览器缓存目录默认在 `~/.cache`（只读）→ 需 `PLAYWRIGHT_BROWSERS_PATH` 指到工作区内。

## 指标交付核对（交付期，可重跑）

第一版矩阵核的是"归属"，不是"交付"——T10 的交付核对因此查出 **9 个指标在代码里根本不存在**（`requests_total` / `auth_failures_total` / `auth_latency_seconds` / `throttled_total` / `invalidate_pending_total` / `invalidate_converge_seconds` / 三个消费者 gauge），而设计与 `ops.md` 都已在让运维按 `hydra_invalidation_consumer_stalled_seconds > 60` 告警。同一轮还查出 **3 个幽灵开关**（`HYDRA_TENANT_API_RATE_LIMIT_PER_MIN` / `_AUTH_FAIL_LIMIT_PER_MIN` / `_LOCKOUT_SECS`：文档有、代码从不读取）。全部补齐（提交 `6023661`）。

```bash
# 指标：设计的 §9.1 表格 ↔ 代码里是否真的注册（不是"有没有任务"）
python3 - <<'PY'
import re, subprocess
design = open('dev-docs/design-tenant-api.md', encoding='utf-8').read()
sec = re.findall(r'^### 9\.1 新指标(.*?)(?=^### 9\.2)', design, re.S | re.M)[0]
names = re.findall(r'^\| `([a-z_0-9]+)` \|', sec, re.M)
missing = [n for n in names if not subprocess.run(
    ['grep', '-rl', f'"{n}"', 'crates/hydra-server/src/'],
    capture_output=True, text=True).stdout.strip()]
print(f"{len(names)} 个声明指标，缺失：", missing or "无")
PY

# 环境变量：文档 ↔ 代码是否真的读取
for v in HYDRA_TENANT_API HYDRA_TENANT_API_RATE_LIMIT_PER_MIN \
         HYDRA_TENANT_API_AUTH_FAIL_LIMIT_PER_MIN HYDRA_TENANT_API_LOCKOUT_SECS \
         HYDRA_TENANT_API_INVALIDATE_PER_MIN HYDRA_TENANT_API_USAGE_MAX_WINDOW_DAYS \
         HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS HYDRA_TENANT_API_CONVERGE_TIMEOUT_MS \
         HYDRA_AUTH_ALLOW_TTL_MAX_SECS; do
  printf '%-46s %s\n' "$v" "$(grep -rq "\"$v\"" crates/hydra-server/src/ && echo ok || echo MISSING)"
done
```

**当前结果：12/12 指标已注册、9/9 环境变量被读取。**

同样地，`hydra_tenant_api_requests_total` 的 `endpoint` 标签必须在**每一条出口**上都被设置——包括在路由解析之前就返回的 404/405。实现方式是把标签放在 `RequestContext` 上（而不是穿进四个响应写出函数，那会改掉约 20 个调用点，而"只覆盖记得改的路径"的指标比没有指标更糟）。

### T10 — 全门禁 + 交付核对 + Playwright（本提交）

| 项 | 结果 |
|---|---|
| **core** | 16 target、**187 passed / 0 failed** |
| **server 单特性** | **33 target、435 passed / 0 failed** |
| **server 三特性矩阵** | **33 target、538 passed / 0 failed** |
| **活 ClickHouse `--ignored`** | 1 passed（读者数字与手跑 SQL 逐字段一致：`requests=8` / `tokens_in=247`，分组行也一致） |
| **clippy（workspace，CI 的两种组合）** | 各 **0 error**（`--features hydra-server/server`；`server,cluster-redis,usage-clickhouse`） |
| **依赖防火墙（按 CI 原样命令）** | OK（`hydra-core` 无 tokio/pingora/sqlx/reqwest/hyper） |
| **脚本门禁** | `check_i18n.js` → `OK (338 en keys, 4 locales, code↔en consistent)`；`check_i18n.test.cjs` → pass 1 / fail 0；`ask_llm.test.sh` → ALL PASSED |
| **Playwright（Chromium，真 release 二进制 + 真 SQLite + `tests/e2e/seed.sh`）** | **15/15 passed**，含 **T2.4 auth-cache invalidate**（真实走到改写后的 `app.js` 与新的 `fleet` 响应体） |
| **findings 处置核对** | **24/24 通过**（可重跑 `.acceptance/findings-disposition.py`） |
| **设计用例覆盖矩阵** | **44/44 已分配**（矩阵内嵌脚本重跑：`用例总数 44 未分配 []`） |
| **`AppState` 字面量计数** | **2**（定义 + `main.rs` 生产构造；命令 `grep -rn "AppState {" crates/ --include=*.rs | grep -v "impl AppState" \| wc -l`） |
| **指标交付核对（本轮新增）** | **12/12 指标已注册、9/9 环境变量被读取** —— 这一轮查出并补掉了前述 9 个未实现指标 + 3 个幽灵开关（提交 `6023661`） |
| **限流窗口 GC（本轮新增）** | 交付核对还查出 `TenantApiLimiter::gc` / `Throttle::gc` **在生产代码里从未被调用**：失败维度按**源 IP**（调用方可选）建键 ⇒ 无界内存增长，限流器自己成了它要防的那种攻击面。已按既有 `spawn_gc_task` 形制接线（提交 `e99eca4`） |

> **交付期修正**：计划里"矩阵即可复跑证据"的说法不完整 —— 归属矩阵**不能**替代交付核对。已在 `## 指标交付核对（交付期，可重跑）` 补上可重跑的核对脚本，并在矩阵抬头加了警示。这正是"文档说交付了、代码里没有"这类缺口能被抓住的唯一方式。

## 实现后对抗式交叉审核（2026-09-17）

**形式**：两路独立对抗复审（① 文档声明 vs 代码的逐条证伪；② 实现缺陷/空断言/一致性对抗）。② 在**修改工作树做变异测试**时被中止 —— 见下面的"流程事故"。

### ① 逐条事实证伪：7 条 BLOCKING，全部处置

| # | finding | 判定 | 处置 |
|---|---|---|---|
| 1 | 设计在 **6 处**（含已答的 Q10）文档化了 E2 的 `?wait=converged\|none` 与 `?timeout_ms=`，**代码零解析** | **真** | **实现**（提交 `a415a38`）：`wait=none` → 发布后立即 202 + `event_id`；`timeout_ms` 限幅 `1..=60000`；两者**都校验**（拼错必须 400，而不是静默等于默认值）。**`budget` 用 `Option<Duration>` 而不是零超时**：零超时会把每个节点报成 `lagging`，那是对"没人看过"的节点下断言 |
| 2 | `waited_ms` 在 HEAD 里是编造常量 `888_888` / `777_777` | **假 —— 但暴露了真事故** | 这两个字面量**确实被提交过**（`bce24ac` 引入、`9d6bc47` 移除），当前 HEAD 与工作树**都没有**（`git show HEAD:` 与 `grep` 均证实）；实际值是 `started.elapsed()` 与单节点的 `0`。**根因是流程事故**（见下），已定位到具体提交 |
| 3 | 设计 §10.3 的 C2 与实现、与计划自身的 P1-5 修订互相矛盾；`fleet_invalidation_unavailable` 这个码在 `crates/` 下零命中 | **真** | 改设计：C2 按 P1-5 重写为"有失效流但通道失败"；503 的真实形态是"正常 InvalidateView + `fleet.state:"unavailable"`，**没有**独立 error.code"，并注明该码从未存在 |
| 4 | `crates/hydra-server/tests/zz_adv_probe.rs` 被 git 跟踪，首行自述"用完即删" | **真** | 见"流程事故"。**已删除**（`7aea6e1`）；其中**有价值的断言被保留下来**（见第 8 条） |
| 5 | 5 处指标标签枚举与代码不符（`too_large`/`tenant_gone` 不可达；`unrouted`/`locked`/`single_node`/`unavailable` 缺失） | **真** | 四处改文档 + **一处补代码**：`led` 不应删而应可达 → 被锁定拒绝时记 `reason="locked"`；E2 的 429 补记 `throttled{scope="invalidate"}`（原本是个没有生产者的文档化维度） |
| 6 | `--test tenant_cache` 实测 **4 passed**，计划两处写 3 | **真** | 改计划：注明 **3 是步骤 0 的中间态、4 是终态**（3 条令牌生命周期 + C16），避免下一个人把 4 当回归 |
| 7 | `grep -c "AppState {"` 应为 14 已失效（裸命令 3、排除 impl 后 2） | **真** | 改两处：统一为排除 `impl AppState {` 的形态、期望 **2**，并注明 **14 是改造前的历史值** |
| 8 | （由被删探针留下）413/读失败的 body **不含** `trace_id` | **真，且原实现确有此缺陷** | **改实现**：两处静态字节串改由同一个 `error_body` 构造（带 `trace_id`）。新增测试断言 header 与 body 的 trace id 一致，并**用回退注入验证其判别力**（回退后该测试失败并打印无 trace_id 的 body）。这两条路径（1 MiB 上限、读体失败）此前**完全没有测试** |

### 流程事故（必须记录，不是粉饰）

**`git add -A` 把复审子代理的变异注入扫进了两个提交。** 具体：子代理为验证"测试是否空断言"而临时把 `waited_ms` 改成 `888_888`/`777_777` 并新建了 `zz_adv_probe.rs`；我在它**仍在运行**时执行 `git add -A`，于是 `bce24ac` 记录了注入、`9d6bc47` 记录了它的还原，探针文件则一直被跟踪到 `7aea6e1` 才删除。

| 项 | 事实 |
|---|---|
| 影响面 | 两个**中间提交**（`bce24ac`、`9d6bc47`）短暂含有编造值；两提交之间的树状态不正确 |
| 当前状态 | **HEAD 与工作树干净**（`git show HEAD:…\|grep -n waited_ms` 与 `grep -rn "888_888\|777_777"` 均无命中；探针已删）。`9d6bc47` 已把注入还原，第 8 条的修复也已落地 |
| 为什么不改写历史 | 改写需要为受影响提交重建"当时能编译"的内容，风险高于收益；这些提交**未推送**，推送前 squash 即可得到干净历史（**建议**） |
| 教训 | ① 在**写权限可能被其他代理持有**时禁止 `git add -A`，改为显式列出路径或先 `git status` 逐项确认；② 变异测试必须由**不共享工作树**的执行者做，或先让所有改动方停下；③ "复审代理不许改文件"这条指令**没有被遵守**，因此不能只靠指令 —— 提交前必须自己看 `git status` |

### ② 实现对抗复审：被中止（诚实声明）

该路在**修改工作树做变异测试**时仍在运行，我因上述事故已将其**中止**，因此它**没有交付报告**。它唯一的产出（`zz_adv_probe.rs` 的 413 断言）已被我人工复核、保留为正式测试并**验证了判别力**（第 8 条）。

**因此本门禁的判定依据是**：① 一路完整的逐条证伪（7/7 已处置，且修复本身由我逐条机械复核，证据见上表）+ ② 我自己按同一清单做的对抗式自查（本轮共查出 **5** 个真问题：限流器 GC 从未被调用、成功预算被 403 消耗、`client_ip` 带端口致 IP 维度失效、断言弱形态、两个重复的 fleet 响应类型）+ ③ 全部可复跑门禁（core 187 / 单特性 440 / 三特性 544 / 活 CH 1 / clippy 两种组合 0 / 防火墙 / 脚本 / findings 24-24 / 指标 12-12 / 环境变量 9-9）。**不是**"多路都干净" —— 被中止的那一路没有交付报告，且我没有第三路独立复证的可交付结论。

### 复审修复的机械复核（我自己跑，不等复审）

7 条全部落地，每条都有可复跑证据：

| # | 复核命令 | 结果 |
|---|---|---|
| 1 | `grep -n "invalid_wait\|invalid_timeout_ms\|MAX_CONVERGE_MS" crates/hydra-server/src/tenant_api/handlers.rs` | 常量 + 两个 400 分支齐备；另有 3 条端到端测试（`wait=none` 202、拼错 400、越界/非数/负数 400 + 边界 1 通过） |
| 2 | `grep -rn "fleet_invalidation_unavailable" dev-docs/ crates/` | 仅剩"该码从未存在"的说明性文字，无任何地方再声称它存在 |
| 3 | `grep -n "state\` 四态" dev-docs/design-tenant-api.md` | 已改为四态并写明"无流归 single_node"的理由 |
| 4 | `grep -n 'record_tenant_api_throttled("invalidate")\|record_tenant_api_auth_failure("locked"'` | 两个此前不可达的标签现在都有生产者 |
| 5 | `cargo test … --test tenant_cache` | **4 passed**（计划已改为 4，并注明 3 是中间态） |
| 6 | `grep -rn "AppState {" crates/ --include=*.rs \| grep -v "impl AppState" \| wc -l` | **2**（两文档已改用这条命令） |
| 7 | `grep -n 'error_body("payload_too_large"'` | 413 body 经统一构造器，带 `trace_id`；测试注入验证过判别力 |

### 我自己在自查中查出的 5 个真问题（均已修 + 均有测试）

| 问题 | 性质 | 处置 |
|---|---|---|
| `TenantApiLimiter::gc` / `Throttle::gc` **在生产代码里从未被调用** | 失败维度按**源 IP**（调用方可选）建键 ⇒ 无界内存增长；限流器自己成了它要防的攻击面 | 按既有 `spawn_gc_task` 形制接线（`main.rs`），边界写进 `GC_IDLE` 的文档（"最近 120s 内出现过的失败地址数"，与攻击速率成正比、与总量无关） |
| 成功预算在 URL 交叉校验**之前**扣减 ⇒ 客户端配错 URL 的 403 会烧掉**自己租户**的配额 | 与设计"成功限流"的语义不符，且一个配错的客户端可把租户锁在自己的 API 之外 | 移到交叉校验之后，并加测试**双向**钉住（连打 5 次 403 后配额仍完整） |
| `client_ip` 用了 `session.client_addr().to_string()`，**带端口** ⇒ 每条连接一个桶 ⇒ 源 IP 维度等于失效 | 端到端锁定测试抓到（拿到 200 而不是 429） | 改用 `as_inet().ip()`；并写明**不读 `X-Forwarded-For`**（调用方可控的头不能决定自己的桶），LB 后的共享桶后果写进 `ops.md` |
| `assert_ne!(v["totals"]["requests"], 0)` 无法区分"没有 totals"与"totals 非零"（缺字段索引出 `Null`，而 `Null != 0` 为真） | 弱断言，与同文件兄弟测试的 `is_null()` 不一致 | 改为 `is_null()`；并注入验证（回退后该测试失败） |
| **`FleetView`（租户面）与 `Fleet`（管理面）是两个字段完全相同、各自手工映射的结构** | 同一个响应形状有两个 owner；"必须保持一致的两份形状终将不一致"，且映射处有三处可以漏字段 | **收敛为一个类型**：`waited_ms` 移进 `FleetReport`（屏障自己测量"发布+等待"，两个入口的 `waited_ms` 与 `invalidate_converge_seconds` 因此不可能互相矛盾），两个入口**直接序列化同一个 report**；`cluster-redis` 缺席时各自保留一个 cfg 配对的 `single_node` 占位（因类型在 cluster 模块内），两者都注明"同形同名"。删掉 2 个结构、2 处手工映射 |

## 实施记录（开发期回填）

### T4 — 骨架 + 数据面前缀拦截 + 令牌闸门（`d7f289b`）

| 项 | 结果 |
|---|---|
| 新增 | `tenant_api/mod.rs`（路由 + 闸门 + 响应信封 + `RESERVED_PREFIX` + `TenantApiConfig`）、`tenant_api/auth.rs`（快照闸门 + 常数时间比较）、`usage_query.rs`（`UsageQuery` trait + `SqliteUsageQuery` + `select_sqlite`）、`tests/tenant_api.rs`（10 条） |
| 修改 | `proxy.rs`（`AppState` +3 字段、`for_tests`、拦截点）、`main.rs`（构造后移 + 注入）、`lib.rs`（两个模块声明）、12 处测试构造点 |
| **TDD 证据** | **RED（摘除拦截）：2 passed / 8 failed** —— 失败形态正是设计预测的：保留前缀的请求返回了**管线自己的信封** `{"error":{"message":"unknown_domain","type":"proxy_error"}}`，证明租户路径当时被当成普通数据面请求处理。**GREEN：10 passed** |
| 门禁 | fmt clean；`clippy -D warnings` 在**两种**特性组合下 clean；core 16 target；三特性 **31 target** 全绿；限域生产 grep 为空 |
| 计数校验 | `AppState` 字面量 12 处测试 → `for_tests()`；剩余 **2** 处（定义 + 生产构造，命令见 T4） |

### T4 执行期对计划的修正（3 处）

| # | 计划所写 | 实际 | 处理 |
|---|---|---|---|
| 1 | `for_tests(pool, store, auth, …)` | `pool` 参数**多余**：`ConfigStore::pool()` 已是"本节点有没有本地库"的唯一 owner，再放一份到 `AppState` 就有漂移风险 | **去掉 `pool` 参数**，`usage` 由 `store.pool()` 派生。副作用是 12 处站点的字段映射变成 1:1，迁移可机械完成 |
| 2 | `usage` 字段与 `AppState` 同批（T4），`usage_query.rs` 在 T8 | `AppState` 加字段会打断 12 处站点，所以**一次加完**才是对的；因此 `usage_query.rs` 的 **SQLite 臂**随 T4 落地（CH 臂仍留 T8） | 采纳。`SqliteUsageQuery` 是真实可用的读实现（T8 接上 E3 handler），不是桩；集群部署下 `usage` 暂为 `None`，端点回 503 而**不是**零 —— T8 收口，已在提交信息里标明 |
| 3 | 拦截条件 `starts_with(RESERVED_PREFIX)` | `/tenant`（无尾斜杠）不匹配，会漏进管线 —— 而它同样可能带着租户令牌 | **裸 `/tenant` 也算保留**（`p == "/tenant" || p.starts_with("/tenant/")`）。这是被保留前缀的最后一条缝隙 |

### T4 里被测试抓到的两处**假覆盖**（我的断言写错了）

RED 阶段有 2 条测试**通过**，查"哪 2 条"才发现是我的断言写错：我读的是 `error.code`，而**数据面短路信封把原因放在 `error.message`、根本没有 `code`**，于是 `!matches!(code, "unknown_domain"|…)` 恒真。两条已改为**对信封形状的差分断言**（我们的信封必有 `code` + `trace_id`、且绝无 `type: proxy_error`）。

另 2 条在 RED 下仍通过，**属正当**并已记录：一条是"关闭开关 == 基线"的等价性断言的另一半（其配对断言证明开启时行为不同），一条是"数据面永不暴露内部路由"的回归守卫——两者本来就不是功能测试，强行让它们变红才是错的。

## Batch A 自审记录（T1 / T2 / T3，2026-09-17）

### 状态

| 任务 | 提交 | 门禁 | TDD 证据 |
|---|---|---|---|
| **T1** `hydra-core` 纯函数 | `c312880` | fmt / clippy / core 16 target / 防火墙 全绿 | **真红**：骨架版（每个函数编码一个真实缺陷类）跑出 `5 passed; 12 failed`，失败均为**断言**失败（`parse_route` 把 `t1/api/v1/whoami` 当 tenant_id、解码器对空 body 回 `Ok(全 0)`、宽松整数拒绝 `"12345"`）；换真实现后 `17 passed` |
| **T2** `ConfigData.tenants_by_id` | `af7b88a` | fmt / 两种特性 clippy / core 16 / 三特性 **30 target** 全绿 | **真红**：字段先留空 → `left: 0, right: 1`（`tenants_by_id` 必须与 `tenants_by_domain` **同源**，断言的是"同源"而非"非空"）；GREEN 后 11 passed |
| **T3** CH 传输下沉（只搬迁） | `7cce5ba` | fmt / 两种特性 clippy / 三特性 30 target 全绿 | 重构类：以既有测试为回归网。**零行为差异**已证：`clickhouse_sink` 与钉住的基线**逐字一致**（3 passed / 1 ignored）；活 ClickHouse 行数 **10 → 12**；无 `usage-clickhouse` 时仍能编译 |

### 漂移检查

| 项 | 结论 |
|---|---|
| 是否仍在原任务意图内 | ✅ 三个提交各自只做一件事（纯函数 / 派生索引 / 搬迁） |
| 是否仍在兼容边界内 | ✅ 数据面路径未动（`proxy.rs` 尚未修改）；`ConfigData` 新字段不上 wire（有测试钉住） |
| 是否出现新 owner / fallback / adapter / 分支 | ✅ 无。T3 反而**删掉**了两个本可留下的读路径辅助函数（无人调用 → `-D warnings` 拒绝死代码），把它们留给 T8 |
| 退役轨道是否仍明确 | ✅ T9 的退役清单未变（`tenant_cache.rs` 的 3 留 5 迁、旧路由、`tenant_id_for_token`） |
| 证据是否支撑下一步声明 | ✅ 每一提交都有可复跑的门禁输出 |

### 执行期对计划的修正（3 处，均已在提交信息里记录）

| # | 计划所写 | 实际 | 处理 |
|---|---|---|---|
| 1 | "内联测试必须与实现一起搬进 `clickhouse.rs`，否则 `sink.rs` 在 `usage-clickhouse` 下编译不过" | **不成立**：那些测试引用的是 `ClickHouseConfig`（搬走）与 `insert_batch_clickhouse_http`（留下并改为委托），加一行 `use` 即可编译 | **偏离计划**：测试**留在原处**，继续断言它们本来要断言的 **sink 级**行为；`clickhouse.rs` 另加原语级测试（含从真实 socket 读回请求行、证明 SQL 与每个 `param_*` 都被百分号编码）。覆盖面上升，且没有测试离开 `--lib` |
| 2 | `reindex_tenants` 写成 `pub(crate) fn reindex_tenants(cfg: &mut ConfigData)` | 实现为 **`ConfigData` 的方法**（在 core 里） | **更优**：纯逻辑归 core，自动受 core 覆盖率门槛约束，且"唯一写入口"由类型自身承载 |
| 3 | 预期 `ConfigData` 穷举字面量改动 2 处 | **恰好 2 处**（`store.rs:178`、`hydra-core/tests/validate.rs:275`），无第三处 | ✅ 预测准确。**但第 2 处差点漏掉**：`cargo check -p hydra-server --all-targets` **不会**编译 `hydra-core` 的测试目标，只在**工作区级** clippy 下才暴露 → 已作为操作教训记录：改 `hydra-core` 的公开结构体后必须跑工作区级门禁 |

### TDD 抓到的真缺陷（1 处）

T1 的测试在"真实现"下仍有 **1 条失败**，而它是**实现的真缺陷**：`last_seen` **缺失**时我把它当成了"无记录"。聚合 SQL 永远投影该列（`MAX(created_at) AS last_seen`），所以"缺失"意味着响应形状漂移、不是空结果集。区分「存在但为空」（合法空集）与「缺失」（形状漂移）正是设计禁止"静默成 0"的那条原则。已修实现而非改测试。

## 门禁记录：计划阶段（2026-09-17）

### 判定

**计划门禁：通过（无 P0/P1 遗留）。** 判定基于**两轮已完成的对抗式复审 + 其 findings 的机械处置核对 + 对修复本身的正确性复核**。

### 依据（可核验，不依赖任何一方口头声称）

| 轮次 | 范围 | 结果 | 处置 |
|---|---|---|---|
| 逐条事实证伪 | 30 条带 `file:line` 的断言；6 条 ClickHouse 事实由复审者用 `curl` **独立复测** | BLOCK：1 FALSE / 2 IMPRECISE（27 真） | 全部处置 → 设计 v5.2 |
| 架构对抗 | 任务序与验证可达性、设计保真、owner 唯一性、退役、缺项 | BLOCK：**2 P0 / 12 P1 / 9 P2** | 全部处置（见下） |
| 对"修复本身"的正确性复核 | 自查修复是否引入新错误 | 3 处需收紧 | 全部处置 |

```bash
# findings 处置核对（可重跑，24 项）：
python3 .acceptance/findings-disposition.py
#   -> 24/24 通过；门禁：全部 findings 已在计划文本中落实

# 两张矩阵（可重跑）：
#   设计用例覆盖 44/44、指标交付 12/12、AppState 字面量计数 14（文档声称 14）
# 见「设计用例覆盖矩阵」与「指标交付矩阵」两节内嵌的复核脚本
```

### 对"修复本身"的复核（第三项依据，3 处收紧）

| # | 自查发现的修复缺陷 | 收紧 |
|---|---|---|
| ① | 拦截片段"先取 owned path 再判前缀"会让**每个业务请求**都分配一个 `String` —— 把控制面路径的复杂度摊到数据面热路径上 | 改为**零分配判定前缀 → 命中才 owned**（`str::starts_with` 在借用上完成） |
| ② | `reindex_tenants` 在 `hydrate` 里的插入点只写了"之后"，不精确 | 钉到 **`snapshot.rs:283` 的 `let mut cfg = self.cfg;` 之后、构造 `HydratedWire` 之前**（重建必须在**交付**给 `apply_snapshot` 之前）；并补一条副作用说明：`tenants_by_id` 会**参与 `ReplicationContent` 的 `PartialEq` 代际判定**（`content.rs:64-80` 持 `Arc<ConfigData>`，`ConfigData` 也 derive `PartialEq`）⇒ **该字段只有一个写入口**，禁止在 `reindex_tenants` 之外写它 |
| ③ | `usage_query::select` 的参数类型 `&clickhouse::ClickHouseConfig` **本身被 `usage-clickhouse` 门控**，所以一个"总是存在"的 `select` 拿不到该类型 | 改为 **cfg 配对的两个定义**；T14 的断言按特性分两组，保证它在**两种特性组合下都被编译并断言** |

### 局限（必须记录，不得粉饰）

1. **第三路复审（有界架构复审，在 fixes 之后重跑）未返回结果。** 本会话共派出三路架构/证伪复审，其中两路完成并交付；另两路（第一路架构复审、以及为它另起的有界复审）**跑时病态长跑**，前者被中断后仍交付了报告，后者被中断且无报告。**因此"通过"是相对两轮已完成复审的判定，不是相对"任意多轮都干净"的判定。**
2. 缓解措施：把"修复是否真的落实"变成了**机械可重跑**的核对（`findings-disposition.py`，24 项）与两张矩阵的自校验脚本，而不是依赖复审者的二次确认。任何后续复审只需重跑这三段脚本即可看到当前状态。
3. **实现阶段仍有一道独立的对抗式交叉审核**（本计划最后一步），它会以**代码**为对象重新检查一致性 —— 计划阶段未返回的那一路，其覆盖面在实现阶段被再次覆盖。

## 修订记录（复审驱动）

| 轮次 | 触发 | 处置 |
|---|---|---|
| **初稿** | 依设计 v5.1（需求四轮变更后） | 9 个任务、TDD Route strict、覆盖矩阵与指标矩阵 |
| **自审** | 计划期机械核对 | ① 修掉 T4 "增量接线"的含糊表述（原文出现"折中/占位不成立"这类自相矛盾措辞）；② `UsageQuery` 改为与兄弟 trait `UsageSink` **同一种手写 desugar 风格**；③ 补 ADR/决策信号携带节（A-1 连同真实备选与基线同步问题）；④ **实测**发现 44 条设计用例中 **13 条无任务覆盖**（含 C5 这一收敛屏障的核心负例）→ 全部补齐并落成可重跑的覆盖矩阵；⑤ **实测**发现 12 个指标中 **9 个无归属任务** → 落成指标交付矩阵；⑥ 实测 T2 的 `ConfigData` 穷举字面量只有 **2 处**（非"一片"）；⑦ 实测 `cluster::events` 为特性门控 → 补 `AppState.invalidation` 的双字段 cfg 形状；⑧ **自证伪**：T9 原写的"旧路径 → 404"是错的（租户令牌会落到 admin 闸门 → **401**）→ 改为双凭证断言 |
| **复审第 1 路：逐条事实证伪**（30 条断言：27 真 / 1 假 / 2 不准） | 对抗式证伪，含 6 条 ClickHouse 事实用 curl **独立复测** | **[F-1 假]** `AppState` 构造点从"13 个测试"改正为 **12 处测试**（`terminate_mode.rs:224` 内含于 `:206-233` 的 helper 被重复计）＋计数校验命令；**[I-1 不准]** `events.rs:291` → `:293/311`（7 处引用）；**[I-2 不准，且把风险说轻了]** 裁剪的 generation bump **不是**"仅当丢掉未读条目"——`XTRIM removed>0` **无法区分已读/未读**，故 >333 事件/秒持续 30s 就会让全部节点清空 L1+L2，**与消费者是否落后无关**；D6/§5.1/§6.3/C7/附录 A.5 措辞全部改正 |
| **复审第 2 路：架构对抗**（2 P0 / 12 P1 / 9 P2；该路因跑时病态被中断后另起有界复审） | 与作者独立的事实核对 | **[P0-1]** v5.2 的"T4 接 whoami"与 T5（本身就是 whoami 任务）自相矛盾 → **T4 不接任何端点**（只交付闸门 + 诚实的路由骨架），T5/T6/T8 各替换一个 404 分支；T20 按端点分段落地。**[P0-2]** `tests/tenant_cache.rs` 是旧路由**真正的调用方**且跑在**主 CI job** → T9 新增步骤 0（实测 8 条 → 留 3 删 5 + 断言意图映射表）。**[P1-1]** 拦截片段借用冲突（E0502）→ 先取 owned path。**[P1-2]** `/tenant/` 必须是**前缀**而非三个字面路径（否则 near-miss 会落到外部鉴权，把租户令牌送去 `auth_url`）。**[P1-3]** T3 的回归网真相：真正覆盖搬迁对象的是 `sink.rs` 的**内联模块**，`--test clickhouse_sink` 不会跑它 → 内联测试随实现一起搬 + Verification 加 `--lib`。**[P1-4]** CH 原语签名两头不成立（两个期限 + 读侧 SQL/params 无入参）→ 给出正确签名。**[P1-5]** 屏障接口三处不可实现（`SingleNode` 不该由流方法返回、`Unavailable` 原定义运行期不可构造、`list_nodes` 是 async 且含死节点）→ 逐条重定义。**[P1-6]** 不得给 `AuthCache::new/with_clock` 加参数（**实测 56 个调用点 / 16 个文件**）→ 改用 builder。**[P1-9]** T14 不可写（注入点在二进制目标）→ 提取 `usage_query::select` 库函数。**[P1-10]** 生产 grep 门禁全量跑**今日已命中 358 行**、永远无法变绿 → 限定到新增/改动文件 + 记录基线例外。**[P1-11]** T2 的红灯必须落在 **loader** 上（core 里手工构造的断言无法由生产改动转绿）。**[P1-12]** `AppState` 实为**加 3 字段**（含 `tenant_api`），`for_tests` 必须能表达新测试所需值，T6 的 Files 缺 `main.rs`（`node_id` 穿线）。**9 条 P2** 全部处置：反熵 grep 断言限域 + 4 语言 i18n 文案、三个新模块的**特性门控**、`ConfigData` 上 wire 的 **serde 决策**（`skip, default` + hydrate 重建）、core 套件计数（13→14）、用例编号（无 C15、无 T24）、C6 的确定性构造 + `checked` 断言 + T16 进阻塞项、T5/T9 补 Verification 块、admin UI 只能展示路径 |
