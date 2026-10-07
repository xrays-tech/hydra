# 用量后端：插入模式（唯一所有者）

> 这份文档是**"怎么接一个新的用量后端"的唯一所有者**。ADR：`dev-docs/aegis/adr/ADR-0002-usage-backends.md`；
> 实施计划：`dev-docs/aegis/plans/2026-10-07-usage-backend-registry.md`。
> 守卫：`scripts/check_usage_backends.cjs`（它读 `REGISTRY` 的源码，检查本文件、`Cargo.toml` 与 `ops.md` 是否跟得上）。

## 0. 硬规则（先于一切）

1. **用量只进 ClickHouse。** SQLite 是**已退役**的取值：`HYDRA_USAGE_SINK=sqlite` 启动即拒绝并点名替代。
   不要"顺手"把某个库接成"本地也行"——用量在集群里必须是**全局可读**的一份。
2. **`HYDRA_USAGE_SINK` 没有默认值。** 用量去哪必须被显式说出来；未设 ⇒ 拒启（唯一例外是被明确授权的 `none`）。
3. **一个后端必须同时能写、能读。** 只会收数据、不会按窗口聚合（见 §2 读契约）的库**不是**用量后端。
4. **`main.rs` 不认识任何具体后端。** 它只调 `usage::open(kind)`。

---

## 1. 一个后端是什么（描述符）

```rust
// crates/hydra-server/src/usage/mod.rs —— 唯一所有者：有哪些后端
pub struct UsageBackend {
    /// `HYDRA_USAGE_SINK` 的取值。全局唯一。
    pub kind: &'static str,
    /// 编译它的 cargo feature（`usage-<kind>`）。没有对应 feature 时 `open` 报 `FeatureDisabled`。
    pub feature: &'static str,
    /// 这个后端**需要**哪些环境变量。启动校验是通用的：缺谁就点名谁，并说清它的用途。
    pub requires: &'static [Requirement],
    /// 这个后端**认识但非必需**的变量（有默认值）：同样进文档；守卫要求它们有文档或一条记录在案的例外。
    pub recognises: &'static [&'static str],
    /// 一次性给出**写**与**读**。两者同源，因此不可能"写侧注册了、读侧忘了"。
    pub open: fn(&BackendConfig) -> Result<Backend, BackendError>,
    /// 读是否可用。`Unavailable { why }` 是**显式声明**（例：本部署 `none`），且 `why` 必须点名该设哪个变量。
    pub reads: ReaderContract,
    /// 一行运维向说明（进 `open` 的 info 日志与本文档的矩阵）。
    pub notes: &'static str,
}
```

`Backend { sink: Arc<dyn UsageSink>, query: Option<Arc<dyn UsageQuery>> }` 由 `open` 返回：

- `sink` **必填**——用量写不进去的后端不该被选中；
- `query` 与 `reads` 必须一致：`ReaderContract::SameBackend ⇒ Some`，`Unavailable{..} ⇒ None`（单测钉住这条对应关系）。

## 2. 两个契约（后端必须满足的语义）

### 2.1 写契约 —— `UsageSink`

| 要求 | 为什么 |
|---|---|
| `record()` **立即返回**（fire-and-forget），内部走有界通道 + 后台 flush | 代理主流程绝不为遥测阻塞；通道满时**丢并计数**，不反压 |
| `shutdown()` 有界排空（`run_forever` 结束于 `process::exit`，`Drop` 不会跑） | SIGTERM 滚动升级不丢在飞批次 |
| 每一次丢弃都进 `hydra_usage_records_dropped_total{reason}`，日志带 `dropped_trace_id` | 丢计费数据必须可告警、可定位到**哪一行** |
| 批量/退避/保留上限用**共享引擎**（`usage::engine`），不自造 | 一套策略、一套测试 |

### 2.2 读契约 —— `UsageQuery`

```rust
fn aggregate(&self, tenant_id: &str, since: &str, until: &str, group_by: GroupBy)
    -> Result<UsageAggregate, UsageQueryError>;
fn source(&self) -> &'static str;   // 必须等于描述符的 kind
```

| 要求 | 为什么 |
|---|---|
| 窗口是**左闭右开**，`since`/`until` 由调用方归一成 `%Y-%m-%dT%H:%M:%SZ` 定宽字符串；后端**不得**自己宽松解析 | 字符串比较一旦宽松就静默多算一整天 |
| "读不到" **必须是 `Err`**（HTTP 层映射 503），**绝不能**是"零" | `{"requests":0}` 与"你没用量"不可区分——这是本端点唯一不许编造的答案 |
| 空窗口可以是真的零，但响应必须带 `as_of`（批量的自述一致性窗口） | 租户要知道最近 ≤N 秒可能还没落库 |
| 分组是白名单 `none|model|provider|sub_tenant|day`；**空分组键归一为 `""`，不是 NULL** | 可空列裸渲染成 JSON `null`，解码器要字符串 ⇒ 整窗 503 |
| 整数计数可能被序列化成**带引号的字符串**（实测：ClickHouse 24.3）⇒ 解码器同时接受 `"123"` 与 `123`；**非数字是失败，不是 0** | 静默 0 就是少计费 |
| `tenant_id` 只来自**已鉴权会话**，绝不来自 URL 参数 | 否则是一个越权读面 |

## 3. 插入模式（七步，每步都有落点）

> 目标形状：**只新增文件 + 一行注册**。任何一步需要改到"另一个后端的文件"或 `main.rs`，说明模式还没做到位（见 §6 的 Phase 4 反证）。

| 步 | 做什么 | 落点 |
|---|---|---|
| 1 | **判定资格**：该库能不能满足 §2.2 的读契约（定宽字符串窗口 + 5 个分组维度的聚合）？不能 ⇒ 不是用量后端 | 本文档 §5 矩阵记一行"不适用 + 理由" |
| 2 | **一个模块**：`src/usage/backends/<kind>/mod.rs` 导出 `pub static DESCRIPTOR: UsageBackend`；自带线协议时加 `transport.rs` | 新目录 |
| 3 | **一个 feature**：`usage-<kind> = ["dep:…"]`（客户端依赖只在这个 feature 下可见） | `crates/hydra-server/Cargo.toml` |
| 4 | **一份 env 声明**：`requires` 里写 `Requirement { name, purpose, secret }`。**不要**自己写"缺少变量"的错误文案——那是通用的 | 描述符内 |
| 5 | **一行注册**：`REGISTRY` 加一行 | `src/usage/mod.rs` |
| 6 | **测试**：共享一致性套件（§4）+ 自己的线缆测试（mock server；有容器时加 `--ignored` 活实例测试） | `tests/<kind>_*.rs` · `integration/test_<kind>_*_wire.py` |
| 7 | **文档与守卫**：本文档矩阵加一行、`ops.md` 环境表加 `requires` 的变量、必要时进 compose/k8s、**并把变量加进演练环境的唯一所有者**（`integration/_usage_env.py`，见实施计划 T2.7——否则要挨个演练去补，今天 `HYDRA_USAGE_SINK` 就是这么散了 27 处的）；跑 `check_usage_backends.cjs` | 文档 + `integration/` |

## 3.5 守卫（`scripts/check_usage_backends.cjs`）

它读 `REGISTRY` 的**源码**并检查（每条都要有反向证伪）：

| 规则 | 为什么 |
|---|---|
| `kind` 唯一、非空 | 重复 kind 的行为取决于查表顺序，是没人会发现的错误 |
| `feature` 在 `crates/hydra-server/Cargo.toml` 的 `[features]` 里存在 | 一个拼错的 feature 名会让后端在某个构建里静默「不存在」 |
| 每个 `requires.name` 在 `ops.md` 的环境表里出现 | 变量声明与运维文档是两处，必须对齐 |
| 每个 `recognises` 名字**要么**有文档、**要么**在守卫的例外表里带理由 | 今天 `HYDRA_CLICKHOUSE_CONNECT_TIMEOUT_MS` / `_IO_TIMEOUT_MS` 无文档——这类洞要被照出来，而不是继承下去 |
| `sqlite` 必须出现在退役取值表里 | 强制定义 1 的可执行形式 |
| 每个后端模块被一致性套件点名 | 加了后端不写测试 ⇒ 红 |
| 解析出的后端数 `== 0` ⇒ 失败 | 防空转（模式一变就「静默全绿」是这类守卫最常见的死法） |


## 4. 一致性套件（每个后端都要过）

`usage::conformance` —— 在 `usage::testing` 的记录型后端上跑引擎契约，在描述符上跑注册契约；**线缆**部分由各后端自己的 mock 测试承担。

| # | 判据 | 反向证伪 |
|---|---|---|
| C1 | 描述符自洽：`kind` 与注册键一致、`feature` 在 `Cargo.toml` 存在、`notes` 非空 | 改一个 feature 名 ⇒ 红 |
| C2 | 空 env 调 `open` ⇒ `MissingEnv`，**逐一点名**每个缺失变量（不 panic、不半开） | 删一个 `requires` 项 ⇒ 红 |
| C3 | `record()` 在通道满时仍**立即返回**，并计入 `channel_full` | 改成阻塞 ⇒ 红 |
| C4 | `shutdown()` 有界排空；`Drop` 不是唯一路径 | 删 `shutdown` 调用 ⇒ 红 |
| C5 | 四种丢弃原因 + `sink_disabled` 都可达且带 `dropped_trace_id` | 去掉 trace id ⇒ 红 |
| C6 | `source() == kind`（读侧标签不许自造） | 改标签 ⇒ 红 |
| C7 | 读失败 ⇒ `Err`，**绝不**返回零 | 把错误吞成 `Default::default()` ⇒ 红 |
| C8 | `reads` 与 `query` 的 `Option` 一致 | 造一个不一致的描述符 ⇒ 红 |

## 5. 后端矩阵

| kind | 状态 | feature | env | 写 | 读 | 读契约可行性 |
|---|---|---|---|---|---|---|
| `clickhouse` | **交付（唯一）** | `usage-clickhouse` | `HYDRA_CLICKHOUSE_URL`（+ `recognises`：`_CONNECT_TIMEOUT_MS` / `_IO_TIMEOUT_MS` / `_QUERY_TIMEOUT_MS`，**前两个今天在 `ops.md` 没有文档**） | HTTP `POST /?query=INSERT…FORMAT JSONEachRow`，行在 body（`client_api_key` 已掩码），带 `insert_deduplication_token` | HTTP `POST /?query=SELECT…FORMAT JSONEachRow`，`param_*` 绑定 | ✅ SQL + 真 `GROUP BY` + 参数绑定；三个 compose 都已硬编码它，DDL 见 `environment/clickhouse/init.sql` |
| `none` | **交付（D-2 已裁定保留）** | 无（总是编译） | 无 | 丢弃并计数 `sink_disabled` | `ReaderContract::Unavailable{why}` ⇒ `/usage` 回 503 `usage_store_unavailable` | ✅（故意没有存储）必须**显式**选择，启动打 WARN |
| `tdengine` | 候选（模式的首个新插入，D-9） | `usage-tdengine` | `HYDRA_TDENGINE_URL`（taosAdapter REST，默认端口 **6041**） | **请求体是 SQL 文本**（不是 JSON/行协议）：`INSERT INTO db.tb VALUES(…)`；或用超级表自动建子表 `INSERT INTO t USING st TAGS(…) VALUES(…)`；或走 schemaless `/influxdb/v1/write?db=` | 同一个 `POST /rest/sql[/db_name]`，**TDengine SQL**；窗口用 `INTERVAL(10s)` | ⚠️ **能用，但有一个必须处理的陷阱**：taosAdapter **默认对多数错误仍回 HTTP 200、错误在 body 里**（要 `httpCodeServerError` 才回非 200）⇒ "读失败必须是 Err 而不是零"这条**必须靠解析 body 的 code 实现**，否则正好落进本设计最反对的形状。另有：URL 里的 `db_name` 只是 SQL 的默认库前缀；OSS 下 REST 的 TLS 是否可用 **UNVERIFIED**（文档把 taosAdapter 的 `[ssl]` 标为 Enterprise）；AGPL-3.0 是**网络服务条款**型 copyleft，选型前必须过法务 |
| `influxdb3` | 候选 | `usage-influxdb3` | 数据库令牌（`Authorization: Bearer …`，端口 **8181**） | `POST /api/v3/write_lp?db=…`，行协议（表**自动创建**，数据库要先建） | `POST /api/v3/query_sql?db=…`（SQL：`DATE_BIN(INTERVAL '1 day', time)`）或 `/api/v3/query_influxql` | ✅ SQL 可用、有 schema-on-write；注意 2026-09-15 起其 Docker `latest` 指向 3 Core（**部署要钉版本 tag**）；docs 页面**未标许可证**（仓库为 Apache-2.0 + MIT） |
| `influxdb2` | 候选（老线） | `usage-influxdb2` | `org` + `bucket` + API token | `POST /api/v2/write?org=…&bucket=…`，行协议（bucket 必须先存在） | `POST /api/v2/query`，**Flux**（`aggregateWindow(every:1d)` + `group()`） | ⚠️ 读是 Flux，与本仓"窗口是字符串边界"的模型是**翻译**关系；OSS v2 的集群能力 **UNVERIFIED** |
| `victoriametrics` | 候选（**需先做取舍**） | `usage-victoriametrics` | `HYDRA_…_URL`（单机 **8428**；集群插入 **8480** / 查询 **8481**，且路径要带 `/insert/<accountID>/`、`/select/<accountID>/`） | `POST /api/v1/import/prometheus`（文本）或 `/api/v1/write`（remote write v1）或 `/api/v1/import`（JSON lines）；无 DDL，metric+label 即 series | `GET/POST /api/v1/query` / `query_range`，**MetricsQL** | ❌/⚠️ **它没有 `GROUP BY`**：窗口是选择器里的 `[5m]` + 输出网格 `step`。要承载本仓的 5 个分组维度就得把它们做成 label，而"每请求一行、15 个字段"与时序标签模型是**不同的东西**（租户维度还会带来基数问题）。要接就必须先写下这个取舍，而不是假装它是同一件事。另：默认**无鉴权**（建议前置代理） |
| `timescaledb` | 候选 | `usage-timescaledb` | PG 连接串（端口 **5432**） | **没有 HTTP 数据接口**，走 PG 线协议：参数化 `INSERT`（或 `COPY`） | SQL：`time_bucket('1 day', ts)` + 真 `GROUP BY` | ✅ 语义最贴合；但代价是 (a) 需要一个 PG 客户端依赖（`tokio-postgres`/`sqlx`），(b) **必须由运维先建表**（`create_hypertable(…, by_range('ts'))`）⇒ 插入模式第 7 步要额外产出一份 DDL/迁移工件；(c) 许可为 Apache-2.0 **加** TSL 两半，连续聚合/压缩在 TSL 半侧；分布式 hypertable 已 sunset |
| `sqlite` | **退役（永不再入）** | — | — | — | — | ❌ `HYDRA_USAGE_SINK=sqlite` 启动即拒绝并点名（`none` 也不是它的替代品：`none` 是"本部署不计量"，不是"用量进本地库"）。**D-3 已裁定：表被删**（迁移 `0013`，不可逆；升级前导出/备份见 `ops.md`）——所以这条路的终点不是"不支持"，而是"不存在" |

**矩阵里的事实出处**：每个候选的端点/端口/鉴权/载荷/窗口语法均取自各自官方文档（TDengine REST API、VictoriaMetrics single-server/cluster、InfluxDB v2 与 v3 的 write/query API、PostgreSQL 协议 + TimescaleDB `time_bucket`/`create_hypertable`），
**标注 `UNVERIFIED` 的项在实现该后端前必须实测掉**（本仓的规矩：候选事实不许当既成事实写进设计）。

## 6. 已知弱点（每次都得手写的部分）

- **注册那一行**：静态表要求手动加一行（备选 `inventory`/`linkme` 自动注册被否，因为它把控制流藏起来、`grep` 不出"有哪些后端"）。守卫是补偿：**没有注册就红**。
- **env 文档**：`requires` 与 `ops.md` 的表是两处，守卫比对它们。
- **一致性套件只覆盖共享语义**：协议的线缆形状只能由各后端自己的 mock 测试守，没有通用办法。
