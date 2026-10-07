# ADR-0002 — 用量后端：只进 ClickHouse，且由一张描述符表拥有"有哪些后端"

- **Status**：`accepted`（**2026-10-07 用户裁定 D-1/D-2/D-3/D-9**，见 §3；实施从 Phase 1 开始）
- **Date**：`2026-10-07`
- **Deciders**：用户（三条强制定义）；实施者为执行方
- **Source Evidence**：用户原话「1，用量只进 clickhouse，绝对不进sqlite，完全屏蔽掉进入sqlite这条路。2，既然使用了 sink 模式来引入 clickhouse，那么实际上还可以扩展出 TDEngine 之类的其它指标用数据库。3，既然如此，这里就要用标准的设计模式来隔离变化，并且给每一个可能的选项一个标准的插入模式。」＋ 本次会话对代码/文档/守卫的逐条直读（下表 C1–C6 均给出证据）
- **Owner Surface**：本仓既有的 `dev-docs/aegis/` 工作区（与 ADR-0001 同）；**不是** `docs/aegis/`
- **Boundary**：advisory 记录，不授予完成权，也不取代 `dev-docs/design.md` / `ops.md` / `deployment.md` / `tenant-api-integration.md` 这些项目权威文档
- **配套**：实施计划 `dev-docs/aegis/plans/2026-10-07-usage-backend-registry.md`；插入模式的唯一所有者 `dev-docs/usage-backends.md`

---

## 1. 背景（Context）

用量（per-request metering row）今天有**两个**可选后端，而"选谁"这件事被写在**两处平行的 `match`** 里：

| # | 事实 | 证据（本次直读） |
|---|---|---|
| C1 | **写**侧由 `sink::build_sink(kind, pool, ch_url)` 选择、**读**侧由 `usage_query::select(kind, pool, ch_url)` 选择——两个函数、两处 `match`、各自 `cfg`、各自错误枚举（`BuildSinkError::{UnknownKind,MissingClickHouseUrl,ClickHouseFeatureDisabled,ClickHouseTlsUnsupported}` 与 `SelectError::{UnknownKind,MissingUrl}`）。写侧必填、读侧可缺（缺了 `GET /usage` 回 503） | `src/sink.rs:909-946`、`src/usage_query.rs:442-456`；`main.rs` 把**同一个** `sink_kind` 喂给两边（`:317`、`:889`） |
| C2 | 漂移风险**已被代码注释自己承认**：`usage_query.rs` 里"Reporting the kind as unknown keeps the two guards from drifting apart"、`tests/usage_query.rs` 里"Without the feature, `build_sink` already refuses … — asserting that keeps the two guards from drifting apart"。**靠注释维持一致**，没有任何构造保证 | `src/usage_query.rs:118-122`、`tests/usage_query.rs:255` |
| C3 | 代码默认值是 **`sqlite`**（`const DEFAULT_USAGE_SINK: &str = "sqlite"`），而**三个 compose 文件全部硬编码 `clickhouse`**；`ops.md` §1.1 的"默认"列也写 `sqlite` ⇒「文档化的默认」与「交付栈实配」不一致，且**没人比较过**（`check_documented_defaults` 比较的是文档值 vs 代码常量，两边都是 `sqlite`，所以它是绿的） | `main.rs:65`；`environment/docker-compose.yml:56`、`docker-compose.cluster.yml:73`、`docker-compose.local.yml:66/112/156`；`dev-docs/ops.md:63` |
| C4 | 集群下 SQLite 用量**只能是错答案**：集群模式拒绝 `HYDRA_USAGE_SINK≠clickhouse`，而 leader **也有**本地 SQLite（那份 `usage_record` 一行都没写过）⇒ 读侧因此必须按"能力"而不是"有没有 pool"判断。这条论据是 3 处注释/文档的共同依据，也正是 `GET /usage` 那个 `usage_store_unavailable` 503 的来源 | `main.rs:318`；`src/tenant_api/handlers.rs:406`；`dev-docs/design-tenant-api.md:547` |
| C5 | SQLite 用量表由 **5 个迁移**建立/扩展（`0001_init` 建表、`0002`/`0005` 改列、`0011` 加 `sub_tenant_id`、`0012` 加索引），当前**有写**（`SqliteSink` → `INSERT INTO usage_record`）**有读**（`SqliteUsageQuery` → 4 条 `SELECT`） | `migrations/0001,0002,0005,0011,0012`；`src/sink.rs`、`src/usage_query.rs` |
| C6 | 管理面 `GET /api/v1/stats/usage` **不是**同一个量：它读**进程内 Prometheus 计数器**（自启动累计，非时间窗），与持久化用量是**两个答案**。这是既有且有意的边界，本 ADR 只把它写明，**不改**（否则会把"两个答案"改成"一个没人验证过的答案"） | `src/admin/handlers.rs:2627` → `src/admin/metrics.rs:1396` |
| C7 | 今天**增加一个后端**要动 4 个文件、7 处：`sink.rs`（引擎旁的分支 + 实现）、`usage_query.rs`（分支 + 实现）、一个 transport（照 `clickhouse.rs` 的形状）、`main.rs`（两处调用 + 集群校验 + 日志）。没有任何东西保证"写侧注册了某 kind ⇔ 读侧也注册了" | 直读；`crate::clickhouse::` 11 处、`hydra_server::sink::` 22 处、`usage_query` 相关 28 处引用 |
| C8 | **「没有默认值」不是一个局部改动**：今天**没有**设 `HYDRA_USAGE_SINK` 的地方包括 **27 个 `integration/*.py` 演练**（每个演练各自内联 `env.update({…})`，没有共享所有者）、CI 的 `ui-e2e` 作业（`ci.yml:1162-1171`）、`scripts/e2e-local.sh:113-118`、`scripts/handover.test.sh:39-45`。它们今天全部默默依赖编译进去的 `sqlite` 默认值 | 逐文件核对（清单见实施计划 T2.7） |
| C9 | **迁移文件是 checksum 强制的**（`sqlx::migrate!("./migrations")`）：不仅 SQL 不能改，**连注释都不能改**。于是 `migrations/0001_init.sql:84` 那句 `-- 用量记录（默认 SQLite Sink）` 在本决策之后**永久是一句错话，且无法修正** | `src/db.rs:71`；`0001_init.sql:84-101` |
| C10 | 三处「证据出处」挂在 SQLite 用量上：`check_compose_grace.cjs` 的 `stop_grace_period` 依据是实测「20 个请求在 SIGKILL 下 `usage_record` 0 行、完整 drain 20 行」；`integration/test_streaming_path.py:212` **是唯一显式钉 `HYDRA_USAGE_SINK=sqlite` 的演练**；`tests/tenant_api.rs:1653` 断言 `source == "sqlite"` | 逐处直读 |

一句话：**"有哪些后端"这件事没有所有者**，它是两处 `match` 的巧合一致；而"用量进哪里"的默认值是 `sqlite`，一个在集群里必然给出错答案、且在交付栈里从未被使用的选项。

## 2. 决策（Decision）

**三条强制定义，落成四个可执行结果：**

1. **用量只进 ClickHouse。** `HYDRA_USAGE_SINK=sqlite` 不再是可选项：启动期**拒绝**并点名（不是 warn、不是回落）；`SqliteSink` 与 `SqliteUsageQuery` **删除**；`DEFAULT_USAGE_SINK` **删除**——用量去哪**必须被显式说出来**（§3 D-1）。
2. **"有哪些后端"由唯一一张描述符表拥有。** 新增 `usage` 模块持有 `UsageBackend` 描述符与 `REGISTRY`；一个后端 = **一行**。`main.rs` 只调用 `usage::open(kind)`，不再认识任何具体后端。
3. **一个后端同时提供写与读。** 描述符的 `open()` 一次性给出 `Arc<dyn UsageSink>` 与读能力；"读不了"只能是**显式声明**（`ReaderContract::Unavailable { why }`，例如"本部署关闭了用量"），不能是"忘了写一个 `match` 臂"。
4. **每个候选后端按同一份插入模式接入**（§4），其唯一所有者是 `dev-docs/usage-backends.md`，并由 `scripts/check_usage_backends.cjs` 给它牙齿（新增后端若不登记 env/feature/文档/一致性测试，守卫变红）。

## 3. 决策记录（D-1…D-9，含真实备选）

| # | 决策 | 备选（真实存在的） | 状态 |
|---|---|---|---|
| **D-1** | `HYDRA_USAGE_SINK` **没有默认值**：未设 ⇒ 启动**拒绝**，错误体列出可接受值并要求显式选择。**成本已量**：27 个演练 + `ui-e2e` + 2 个脚本今天都没设它（C8）⇒ 必须同时给「演练环境的 sink 配置」一个**唯一所有者**（共享 helper），否则下次再加一个变量又要改 27 处 | ① 默认 `clickhouse`（那需要一个 URL，缺 URL 时拒启——等于把「没说」变成「猜 clickhouse」，且同样要改那 27 处）；② 默认 `none`（**零改动、零破坏**，但生产会静默不计量——与「丢计费数据必须响亮」的既有立场直接冲突，已否决）；③ 保留 `sqlite` 作默认（**违反强制定义 1，已否决**） | **已裁定：是**（无默认 + 未设即拒启 + T2.7 的共享 helper） |
| **D-2** | 保留一个**显式**的 `none`：不写任何后端、`/usage` 回既有的 503 `usage_store_unavailable`，启动打 WARN 并导出"未启用"的可见信号 | ① 不保留 `none`——用量必须落库，本地/实验部署也得起一个 ClickHouse；② 用退役的 `sqlite` 顶这个位置（**直接违反强制定义 1，已否决**） | **已裁定：保留**（显式 `none`，绝不作为默认） |
| **D-3** | **已裁定：删表，不保留** —— 新增一条迁移 `0013_drop_usage_record.sql` 把 SQLite 的 `usage_record` 表**删掉**（§3.1 给出完整设计：迁移是唯一的删法、顺序、升级前导出的强制步骤、回滚只有一条路）。备选（① 保留表标注退役：被否决；② 半屏蔽=保留写入禁止读取：直接否决） | **已裁定（用户）** |
| **D-4** | 旧数据**不做自动迁移**：升级后 Hydra 不再读 SQLite 用量，`ops.md` 给一次性导出说明（`sqlite3` → CSV/CH `INSERT`），并写明"升级后 `GET /usage` 只回答 ClickHouse 里的量" | ① 启动时自动把 SQLite 用量灌进 CH（一次性、不可测、可能重复计数）；② 双读合并（两个窗口口径不同，会造出没人验证过的数字） | 采纳（推荐，需在 §8 文档同步中落地） |
| **D-5** | 描述符 = **静态表 + 函数指针**（`&'static [UsageBackend]`，`open: fn(&BackendConfig) -> Result<Backend, BackendError>`） | ① `inventory`/`linkme` 自动注册（少一行注册，多一个依赖 + 隐式控制流，`grep` 不出"有哪些后端"）；② 继续用 `match`（就是 C1/C7 的病） | 采纳（推荐） |
| **D-6** | 目录重排为 `src/usage/{mod,engine}.rs` + `src/usage/backends/<kind>/{mod,transport}.rs`；`sink.rs`/`usage_query.rs`/`clickhouse.rs` 的内容迁入 | ① 原地保留三个文件，只在其上加一层描述符（改动小，但"一个后端一个模块"的插入模式就只是口号） | 采纳（推荐；迁移由编译器逐个点名，约 60 处引用） |
| **D-7** | 读契约与写契约**同源**：读能力由同一个描述符给出；`none` 用 `ReaderContract::Unavailable{why}` 显式声明，`why` 必须点名要设哪个变量 | ① 读侧继续允许"某个 kind 没实现"（现状，静默 503 的原因不可见） | 采纳（推荐） |
| **D-8** | 测试替身放 `usage::testing`（内存 sink/reader），**不进 `REGISTRY`** ⇒ env 无法选中它 | ① 注册成 `memory`（生产可以"看起来配好了"却把用量丢进内存）；② 每个测试各写自己的假实现（重复实现，且一致性套件无法复用） | 采纳（推荐） |
| **D-9** | 本轮**只落**"强制定义 + 描述符表 + 插入模式 + 候选矩阵"；**TDengine 的实现另开一案**（用它作为模式的第一个"新插入"来证伪模式） | ① 本轮就把 TDengine 实现掉（范围翻倍，且会在模式还没被审过时就被一个后端塑形） | **已裁定：另开**（模式先行；TDengine 作为模式的首个新插入，独立任务） |

### 3.1 D-3 的执行设计（**不可逆**，按 Data Destruction Guard 记账）

```text
Data Destruction Guard:
- Target Class        : persistent-state（实时库里的表）
- Exact Target(s)     : SQLite 文件里的表 `usage_record`（含索引 `idx_usage_record_created`、
                        `idx_usage_record_tenant`、`idx_usage_record_sub_tenant`）——
                        **只此一张**；ClickHouse 里同名的 `usage_record` 是另一个库，不动
- Environment        : 每个节点的本地 SQLite（`HYDRA_DB_URL`）
- Why Irreversible    : `DROP TABLE` 之后行数据只存在于**升级前的备份**里。sqlx 的版本表
                        `_sqlx_migrations` 会把该迁移记为"已应用"，而 revert 代码会让它变成
                        "已应用但文件不存在" ⇒ sqlx 报错（VersionMissing），**不是**自动回滚
- Backup / Rollback Note: 见下面的"升级前"三步；回滚 = 用升级前的备份换回文件，或手工删掉
                        `_sqlx_migrations` 里那一行再换回旧二进制（两条都写进 ops.md）
- Allowed Read-Only Next Steps: 导出/统计该表现有行数、检查它是否为空（决定是否需要导出）
- Blocked Destructive Steps    : 未经裁定的任何其他删表/改表动作（本 ADR 只授权这一张表）
- Confirmation Required: **已获得**（2026-10-07 用户裁定「D-3：删表不保留」）
- Status              : confirmed → 进入实施（T2.8）
```

**四条执行要求**（缺一条就不许动）：

1. **删法**：**只能**新增 `crates/hydra-server/migrations/0013_drop_usage_record.sql`（`DROP TABLE IF EXISTS usage_record;`）。
   **绝不能改既有迁移**（checksum 强制，C9）。CH 侧同名表由 `environment/clickhouse/init.sql` 管，属性完全不同，**不动**。
2. **顺序**：迁移在启动时于 sink 构建**之前**执行 ⇒ 删表与"删掉 SQLite 读写代码"**必须同一次发布**。任何"先删表、后撤代码"或反之的组合都会造出一个写向不存在表的二进制。
3. **升级前必须做的事**（写进 `ops.md`，且在发行说明里点名）：这是一条**破坏性迁移**。
   ① 先备份整个 DB 文件（`sqlite3 hydra.db "VACUUM INTO 'usage-backup.db'"`，`ops.md` §backup 已有此步）；
   ② 需要历史用量就先导出（`sqlite3 -header -csv hydra.db "SELECT * FROM usage_record" > usage.csv`）；
   ③ 升级后 `usage_record` **在 SQLite 里不存在**，`GET /usage` 只回答 ClickHouse 的量。
4. **计数与测试同步**：`tests/migrate.rs` 的 `EIGHT_BUSINESS_TABLES`（含 `usage_record`）与
   `the_sub_tenant_usage_index_exists` 必须跟着改（前者变 7 张表 + 一条"该表**不存在**"的断言，
   后者退役——索引随表一起消失）。这条断言正是"删表**真的发生了**"的可执行证据。


## 4. 插入模式（每个候选后端的标准接入方式）

唯一所有者：`dev-docs/usage-backends.md`。七步，每一步都有明确的落点，**没有任何一步需要改另一个后端的文件**：

1. **判定资格**：该库必须能回答**读契约**（按 tenant / model / provider / key / sub_tenant / day 分组、按 `created_at` 字符串窗口求请求数与 token 数的聚合），不只是能收数据；做不到就不是用量后端（可以是别的 trait 的候选，但那不是本 ADR 的范围）。
2. **一个模块**：`src/usage/backends/<kind>/mod.rs` 导出 `pub static DESCRIPTOR: UsageBackend`（`kind` / `feature` / `requires` / `recognises` / `open` / `reads` / `notes`），需要自带线协议时再加 `transport.rs`。 `recognises` 是这个后端**认识但非必需**的变量（今天就有三个：`_CONNECT_TIMEOUT_MS` / `_IO_TIMEOUT_MS` / `_QUERY_TIMEOUT_MS`，其中前两个在 `ops.md` 里**没有文档**——新守卫会立刻把这类洞照出来）。
3. **一个 feature**：`usage-<kind> = ["dep:…"]`；后端自己的客户端依赖只在这个 feature 下可见（`dep:` 语法，与既有 feature 一致）。
4. **一份 env 声明**：`requires: &[Requirement { name, purpose, secret }]`。启动校验是**通用的**（缺变量 ⇒ 报错点名该变量与它要干什么），所以后端自己**不写**错误文案 ⇒ 文案一致、可操作。
5. **一行注册**：`usage::REGISTRY` 加一行。
6. **一致性 + 线缆测试**：调用共享一致性套件（§5 的判据），加自己的 mock-server 线缆测试（真容器测试按仓库既有 `--ignored` 风格）。
7. **文档与守卫**：`usage-backends.md` 的矩阵加一行、`ops.md` 环境表加 `requires` 的变量、需要时进 compose/k8s；跑 `check_usage_backends.cjs` 与 `check_documented_env.cjs`。

## 5. 后端矩阵

完整表（含每个候选的写/读协议、env、feature、待验证事实与不适用理由）在 `dev-docs/usage-backends.md`。此处只记类别与立场：

| 后端 | 立场 |
|---|---|
| **ClickHouse** | **唯一交付后端**（今天就是这样，三个 compose 都已硬编码它） |
| **TDengine** | 首选候选，用作模式的第一个新插入（§3 D-9） |
| VictoriaMetrics / InfluxDB / TimescaleDB / 其他 | 候选，按 §4 接入；矩阵里逐条写明"读契约能不能满足"（例如以 label 集合为模型的时序库，对"5 个分组维度 + 15 个字段的行"要做取舍，必须显式写出来） |
| **SQLite** | **退役，永不再入**：`sqlite` 是被拒绝的 kind，不是被遗忘的选项 |

## 6. 退役影响（Retirement Impact）

`delete-first`（内部代码，无外部集成契约）**＋ 两处需要用户确认的持久状态**（§3 D-3 的表、D-4 的旧数据）：

| 退役对象 | 替代者 |
|---|---|
| `SqliteSink`（`sink.rs`）与其 `INSERT INTO usage_record` | 无（用量只进 CH） |
| `SqliteUsageQuery`（`usage_query.rs`）与其 4 条 `SELECT` | `ClickHouseUsageQuery`（已在集群下使用） |
| `DEFAULT_USAGE_SINK` 与 `HYDRA_USAGE_SINK=sqlite` 这个取值 | **无默认值 + 拒启并点名**（D-1） |
| `sink::build_sink` / `usage_query::select` 两个平行 `match` | `usage::REGISTRY` + `usage::open(kind)`（一行一个后端） |
| 集群模式那条"必须 clickhouse"的特例校验 | 不再需要：只剩一个后端，规则由"有哪些后端"表示 |
| `tests/sqlite_sink.rs`、`tests/usage_query.rs` 的 sqlite 腿 | 见 §7 的覆盖迁移（逐一交代，不假装无损） |
| `for_tests()` 里默认注入的 `SqliteUsageQuery` | `usage::testing` 的显式注入（`for_tests_with_usage`） |

**保留**：`usage_record` 的 5 个迁移（不改历史）、CH 的 `init.sql` 与 `tests/clickhouse_ddl_parity.rs`、`hydra_usage_records_dropped_total` 及其告警行、`GET /api/v1/stats/usage`（进程内计数器，C6）、租户 `/usage` 的 503 `usage_store_unavailable` 契约行。

## 7. 覆盖迁移（Coverage Migration，如实记账）

| 今日覆盖 | 处置 | 之后由谁承担 |
|---|---|---|
| `sqlite_sink.rs` 10 条：批量/退避/关闭排空/行内容 | **改靶**：引擎是共享的（`run_channel_sink`），把靶子从 `SqliteSink` 换成 `usage::testing` 的记录型后端 | 同一批断言 + `usage::conformance` 套件 |
| `usage_query.rs` 14 条 sqlite 腿（该文件 31 条的其余部分为 12 条 CH 腿 + 5 条 kind 分派腿）：窗口口径/分组/边界归一/响应形状 | **拆**：与存储无关的（边界校验、`group_by` 白名单、`tenant_id` 参数、响应信封、`since` 归一）改用 `usage::testing` 的 reader 驱动；**与 SQLite SQL 有关的（T15"逐字段对手写 SQL"）随存储一起消失** | 前者：同一批 HTTP 断言；后者：CH 侧已有 12 条，**并需补 1 条"CH 聚合 == 手跑 SQL"的活实例测试**（`--ignored`，CLI 基线本就有） |
| `streaming_usage_persistence.rs` 2 条：流式请求也记账 | 改靶到记录型后端（断言"流式路径调到 `record`"），落库侧交由 CH 线缆测试 | 同上 |
| 引擎的"通道满/保留上限/关闭未排空"四种丢弃计数 | 与 sink 无关，**保留**；靶子换成记录型后端 | 不变 |
| `for_tests()` 默认可用 reader | 改为**显式注入**（没有 reader 就是"这个节点没有可读的用量存储"，即真实的 503 契约） | `usage::testing` |

**演练里挂 SQLite 用量的腿**（本次调研新发现，必须一并改靶，否则 CI 静默变红）：

| 演练 | 挂在哪 | 改靶 |
|---|---|---|
| `integration/test_streaming_path.py:212` | **唯一显式钉 `HYDRA_USAGE_SINK=sqlite`** 的演练；`:261/:358` 读 `usage_record` 断言行数增加 | 改为 CH mock（照 `test_clickhouse_sink_wire.py` 的形态）；**「流式也计量」这条 P1 判据不能丢** |
| `integration/test_shutdown_drain.py:181-189,392-410` | `usage_rows()` 读 SQLite；N3 断言 SIGTERM 时缓冲行落库 | 同上（排空判据保留，存储换成 mock CH） |
| `integration/test_client_disconnect.py:243-258,357,385,393` | 读 SQLite 断言「客户端断开仍计量、且与完整回答不可区分」 | 同上 |
| `tests/tenant_api.rs:1653` | 断言 `source == "sqlite"` | 改为注入的 reader 标签（`usage::testing`） |

**证据出处需要重挂的守卫**：`scripts/check_compose_grace.cjs` 的 `stop_grace_period` 结论（「SIGKILL 会丢在飞用量 ⇒ drain 必须够长」）**仍然成立**，但它引用的**测量介质**（SQLite `usage_record` 的 0 行 vs 20 行）随本次退役消失 ⇒ 必须把出处改挂到 CH 侧演练（丢弃计数 + mock CH 行数），否则这条守卫会引用一个不再被任何东西测过的数字。

**明确不再覆盖的一项**（不粉饰）：`usage_record` 表在 SQLite 上的**行级写入形状**（列名、掩码 key、`tokens_in/out` 的中性列）没有替代者——那张表不再被写。它的 DDL 与 CH 侧列一致性仍由 `clickhouse_ddl_parity.rs`（对照 CH 的 `init.sql`）与迁移文件的既有测试守着，但"SQLite 行长得对不对"从此无人断言，因为**没有 SQLite 行**。

## 8. 基线同步（Baseline Sync）

- **Needed**：yes（在实施阶段执行；完成前**不得**把本 ADR 当作"当前架构已如此"引用）
- **Target**：`dev-docs/ops.md`（§1.1 环境表 `HYDRA_USAGE_SINK` 的默认列 → "必填、无默认"，§9/§9.1 用量相关序列与告警的措辞，§12 集群段）、`dev-docs/design.md`（§9.2 默认实现、§9.3、§5.3 的 SQLite DDL 段、§15 模块图）、`dev-docs/deployment.md`、`dev-docs/cluster.md`、`dev-docs/jiqun-deploy.md`（三处的环境表与"集群必须 clickhouse"的措辞失效——只剩一个后端）、`dev-docs/design-tenant-api.md`（§547 的"为什么不能只按有没有 pool 判断"要改挂到描述符的读契约上）、`README.md` / `README.zh-CN.md`（若提及 sink 选择）、`admin-ui/api-docs.js`（`/usage` 与 `/stats/usage` 两个端点的来源说明）
- **守卫**：`scripts/check_documented_defaults.cjs`（`HYDRA_USAGE_SINK` 移入 `UNVERIFIED_OK` 的「必填、无默认」名单，删掉 `sqlite` 那条字符串比较；**同时重写** `HYDRA_CLICKHOUSE_URL` 那条记录的理由——「required only when the sink is clickhouse」在本决策后为假，而记录一旦失效是**硬失败**）、新增 `scripts/check_usage_backends.cjs`、`check_documented_env.cjs`（新 env 名）、`check_compose_env.cjs`（它按清单派生「该编哪些 feature」，`usage-clickhouse` 目前**不在派生规则里**——正是把「ClickHouse 是必选」编码进去的位置）、`check_compose_grace.cjs`（证据出处，见 §7）、`check_tenant_error_codes.cjs`（`usage_store_unavailable`→503 的配对今天躺在 `tenant_api/handlers.rs:525/:540`，**必须保持可机械提取**，设计不得把它藏进描述符表）、`check_ci_wiring.cjs`（`:666-674` 是**唯一**保持 CH `--ignored` 套件接线的东西：`ci.yml:479` 的 `--test clickhouse_sink --test usage_query --ignored` 一改名就静默脱线）
- **代码侧被守卫钉住的路径**：`crates/hydra-server/tests/clickhouse_ddl_parity.rs:136` **硬编码 `crates/hydra-server/src/sink.rs`** 来解析 CH 的 INSERT 列 ⇒ D-6 的搬家必须同步改它（否则一个纯 ClickHouse 的守卫会因搬家而变红——而它本来就该在搬家时被看见）
- **Reason**：本决策改变 canonical owner（"有哪些后端"）、外部配置契约（`HYDRA_USAGE_SINK` 的取值与默认）、数据面持久化边界（用量不再落本地库）与退役清单 —— 属基线同步必须覆盖的四类

## 9. 可逆性（Reversibility）

| 情形 | 动作 |
|---|---|
| 想让某个部署"不计量" | 设 `HYDRA_USAGE_SINK=none`（若 D-2 通过），纯配置、可随时切回 |
| 想恢复 SQLite 用量 | revert 相关提交（`SqliteSink`/`SqliteUsageQuery`/表都还在仓库与库里）——**数据从未被删**（D-3 保留表） |
| 想把 CH 换成别的库 | 按 §4 加一行 + 一个模块；**不动核心**（这正是本 ADR 要买的东西） |
| **表已被删（D-3 已裁定执行）** | 这是**单向**的：sqlx 的版本表会把 `0013` 记为已应用，revert 代码只会得到 `VersionMissing`。回滚 = ① 用升级前的备份换回 DB 文件，或 ② 手工删掉 `_sqlx_migrations` 里 `0013` 那一行再换回旧二进制（两条都写进 `ops.md`） |

## 10. 证据与验收（本方案的判据）

1. **强制定义可证伪**：`HYDRA_USAGE_SINK=sqlite` 实测**拒启**并点名；`grep -rn "usage_record" crates/hydra-server/src/` 在**写路径**零命中（只剩迁移与测试的 DDL 断言）；`INSERT INTO usage_record` 在 SQLite 侧零命中。
2. **描述符表是唯一所有者**：`grep -rn "HYDRA_USAGE_SINK" crates/` 只命中 `usage/mod.rs`（描述符/校验）与测试；`main.rs` 不再出现任何具体后端的名字。
3. **插入模式有效（本 ADR 的核心验收）**：第二个后端（TDengine）的接入**只新增**文件与一行注册，**不改动**任何既有后端的文件、不改 `main.rs`、不改引擎；具体判据写进实施计划的 Phase 4（触碰文件清单必须全部是新增 + 该后端的测试/文档）。
4. **守卫**：`check_usage_backends.cjs` 在"有人加了后端但没登记 env/feature/文档"时变红（反向证伪：故意删一行登记 ⇒ 红）。
5. **删表确实发生（D-3 的可执行证据）**：`tests/migrate.rs` 断言全新库**没有** `usage_record` 表、且既有库升级后它**消失**；真二进制对既有库跑一次，`sqlite3 hydra.db ".tables"` 里不再出现该表，而 CH 侧同名表仍在（`SHOW CREATE TABLE usage_record` 照旧）。

5. **既有契约不退化**：`GET /usage` 在 `none` 下的 503 `usage_store_unavailable`、CH 不可达时的 503（不是零）、`hydra_usage_records_dropped_total{reason}` 四词词表、`clickhouse_ddl_parity.rs`、四个真进程演练全部保持绿。

---

## Boundary

This ADR is an advisory Aegis Method Pack record. It does not grant completion authority or replace project-authoritative architecture sources.
