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
