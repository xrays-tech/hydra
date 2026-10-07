# 用量后端：只进 ClickHouse + 描述符表（实施计划）

- **Status**：设计已落档，**待 ADR-0002 §3 的 D-1/D-2/D-3/D-9 裁定**后进入 Phase 1
- **Date**：`2026-10-07`
- **ADR**：`dev-docs/aegis/adr/ADR-0002-usage-backends.md`
- **插入模式唯一所有者**：`dev-docs/usage-backends.md`（Phase 1 的 T1.5 建立）
- **一句话**：把"用量有哪些后端"从**两处平行 `match` 的巧合一致**，变成一个**有所有者、有插入模式、有守卫**的描述符表；同时按用户裁定把 SQLite 这条路**彻底封死**。

---

## 目标 / 非目标

**目标**

1. `HYDRA_USAGE_SINK=sqlite` 不可能生效（拒启并点名），SQLite 的用量写入与读取代码删除。
2. 一个后端 = **一个模块 + 一行注册 + 一个 feature + 一份 env 声明**；`main.rs` 不再认识任何具体后端。
3. 写契约与读契约**同源**：不再存在"写侧注册了、读侧忘了写一个臂"的可能。
4. 加第二个后端（TDengine 之类）**只新增文件**，不改核心 —— 并且这条判据要被**实测**一次。

**非目标（明确不做）**

- 不动 `GET /api/v1/stats/usage`（进程内 Prometheus 计数器，与持久化用量是**两个有意的答案**；ADR-0002 §1 C6）。
- 不动 ClickHouse 的 `init.sql` / `usage_record` 列语义 / 去重设置（`non_replicated_deduplication_window`）与它们的守卫。
- ~~不删 SQLite 的 `usage_record` 表~~ → **已裁定删除（D-3）**：见 T2.8 与 ADR §3.1（含 Data Destruction Guard 记账与回滚说明）。
- 不改任何迁移文件（`sqlx::migrate!` 是 checksum 强制的：**连注释都不能改**；`0001_init.sql:84` 那句「默认 SQLite Sink」将永久留错，ADR C9 已记账）。
- 不做旧数据的自动迁移（D-4：给一次性导出说明）。
- 不改 `hydra_usage_records_dropped_total{reason}` 的四词词表（`none` 会**新增**一个 `sink_disabled`，属追加而非改名）。

---

## Phase 0 — 裁定门（**已关闭，2026-10-07**）

| 项 | 需要裁定 | 推荐 |
|---|---|---|
| D-1 | `HYDRA_USAGE_SINK` 无默认值 + 未设即拒启 | **裁定：是**（**成本已知**：27 个演练 + `ui-e2e` + 2 个脚本今天都没设该变量 ⇒ 必须同时做 T2.7 的共享 helper） |
| D-2 | 是否保留显式 `none` | **裁定：保留**（绝不作为默认） |
| D-3 | SQLite `usage_record` 表保留还是删 | **裁定：删表，不保留** ⇒ 新增迁移 `0013_drop_usage_record.sql`（ADR §3.1 的执行设计 + Data Destruction Guard 记账）；**不可逆**，升级前导出/备份写进 `ops.md` |
| D-9 | TDengine 本轮实现还是另开 | **裁定：另开**（模式先行） |

---

## Phase 1 — 描述符表（**行为完全不变**，可独立 revert）— **T1.1/T1.2/T1.3 已完成（2026-10-07）**

这一阶段不删任何后端、不改任何线上行为；它的价值是让 Phase 2 变成"删一行"而不是"改两处 match"。

| # | 落点 | 判据 |
|---|---|---|
| **T1.1 ✅(2026-10-07)** | 新增 `crates/hydra-server/src/usage/mod.rs`：`UsageBackend { kind, feature, requires, open, reads, notes }`、`REGISTRY: &[UsageBackend]`、`open(kind, &BackendConfig) -> Result<Backend, BackendError>`、`BackendError::{UnknownKind, MissingEnv, FeatureDisabled, Retired, OpenFailed}`、`ReaderContract::{SameBackend, Unavailable{why}}`。错误文案**通用**（禁止各后端自己拼） | 单测：查表命中/未知 kind 列出已知集合/缺 env 点名变量与用途/退役 kind 点名替代/`REGISTRY` 无重复 kind；`main.rs` 两处调用改为 `usage::open` 且**行为不变**（sqlite 仍在、仍是默认） |
| **T1.2 ✅(2026-10-07)** | 引擎搬迁：`sink.rs` 的 channel/批量/退避/`MAX_RETAINED`/丢弃计数 → `src/usage/engine.rs`（连同 `UsageSink` trait）。**不保留 `sink.rs` 的转发影子** | `src/usage/engine.rs` 的引擎测试原样通过；`grep -rn "crate::sink::\|hydra_server::sink::"` 归零（约 22 处引用逐个改，编译器点名） |
| **T1.3 ✅(2026-10-07)** | ClickHouse 收成一个后端模块：`src/usage/backends/clickhouse/{mod.rs,transport.rs}`（`sink.rs` 的 CH 写 + `usage_query.rs` 的 CH 读 + `clickhouse.rs` 的传输与 URL 解析），导出 `DESCRIPTOR` | `tests/clickhouse_sink.rs`、`tests/clickhouse_ddl_parity.rs`、`integration/test_clickhouse_sink_wire.py`（16 条）、`tests/usage_query.rs` 的 12 条 CH 腿全绿；**线上报文形状零变化**（wire 演练是这条的判据） |
| **T1.4 ✅(2026-10-07)** | `scripts/check_usage_backends.cjs` + `.test.cjs`：解析 `REGISTRY` 源码。规则：① kind 唯一且非空；② 每个 `feature` 在 `Cargo.toml [features]` 存在；③ 每个 `requires.name` 在 `ops.md` 环境表出现；④ `sqlite` 必须出现在退役取值表；⑤ 每个后端模块被一致性套件点名；⑥ 解析出 0 行 ⇒ **失败**（防空转） | 反向证伪逐条实测（删一行注册/改错 feature 名/删文档行 ⇒ 红并点名）；CI `scripts` 作业接入 |
| **T1.5** | `dev-docs/usage-backends.md`：插入模式七步 + 矩阵（此时只有 ClickHouse 一行 + 候选若干）；ADR-0002 状态随裁定推进 | 文档与 `REGISTRY` 逐行一致（T1.4 的守卫检查"矩阵里有的 kind，注册表里也有"，反之亦然） |

**搬家（T1.2/T1.3）实测抓到的两件事（都已修，值得记）**：

1. **`clickhouse_ddl_parity.rs` 如预期变红**：它硬编码 `crates/hydra-server/src/sink.rs` 来解析 CH 的
   INSERT 列名，文件一搬就 `cannot read …`。这正是 ADR §8 写下的"搬家传感器"，已改为一个具名常量
   `INSERT_SOURCE` 指向新位置（并被三处失败文案共用）。**没有它，一个"纯搬家"会让一条纯 ClickHouse
   的守卫静默失去对象**。
2. **`transport` 模块的 feature 门在搬家后丢了**：原来门在 `lib.rs` 的 `pub mod clickhouse;` 上，
   而**搬进子模块不会继承那道门** ⇒ 一次 `--features server`（不含 `usage-clickhouse`）的测试里
   多跑了 **25 条 transport 测试**（`server` 档 497 → 522）。修法：`transport` 自己声明
   `#[cfg(feature = "usage-clickhouse")]`，连同该模块里只在特性下使用的 import 一起门控。
   **这条正是"数字会说话"的例子**：`--features server` 的测试数必须回到 497，多出来的 25 条就是 bug。

### 顺序更正（2026-10-07，实测得出）

计划原先按"Phase 2 删代码 → Phase 3 改测试/演练"排。**实际做不到**：删掉 `SqliteSink`/`SqliteUsageQuery`
的同一次提交必须把**所有**引用它们的测试一起处理，否则树是红的；而把 `HYDRA_USAGE_SINK` 变成必填，
会同时打断 **27 个演练 + `ui-e2e` 作业 + `e2e-local.sh` + `handover.test.sh`**（它们今天都不设这个变量）。
所以真正的分界不是 Phase 号，而是**加性 vs 破坏性**：

| 已做完（加性，每步绿） | 下一步（破坏性，必须一次做完） |
|---|---|
| T1.4 描述符守卫（并当场照出两个没文档的超时变量） | **T2.7 演练 env helper**（必须先做：27 个演练 + 2 个脚本 + `ui-e2e` 改从它取值，此时 helper 仍返回今天的 `sqlite`，行为不变） |
| T2.5 `usage::testing` 测试基座（记录型 sink + 定值 reader，不进 REGISTRY） | T2.1–T2.3 删 SQLite 后端、删 `DEFAULT_USAGE_SINK`、`HYDRA_USAGE_SINK` 必填、`sqlite` 进退役表 |
| T3.1 引擎契约测试改靶（10 条 SQLite 测试 → 7 条改靶 + 2 条随表消失 + 1 条并入注册表测试） | T3.2/T3.3 租户 API 与流式计量的腿改靶（`usage::testing` / mock CH）；**T3.10 三个演练的计量腿必须同步**（它们读 SQLite 行，`sqlite` 退役后无路可走） |
| T2.4 `none` 后端（显式、响亮、第五个丢弃原因已文档化） | T2.8 `0013` 删表迁移 + `migrate.rs`；T3.12 `ops.md` 破坏性升级流程；T3.5/T3.6/T3.11 文档与守卫同步 |

**为什么 T2.7 必须在 T2.1–T2.3 之前**：helper 先落地时它返回今天的值（`sqlite`），27 个演练行为不变、
全部仍绿；等 helper 成为唯一所有者之后再改"必填 + 无默认"，改动就只剩 helper 一行 + 三个真读用量行的
演练。反过来做，就会在同一次提交里同时面对"删后端"和"27 个文件各自补变量"两件事。


**Phase 1 的验收**：全套测试/守卫/演练与改动前**同一组数字**（只有新增测试变多），且 `git diff` 不含任何行为分支变化。

---

## Phase 2 — 强制定义：SQLite 退出（**T2.4/T2.5 已完成；T2.1–T2.3/T2.7/T2.8 待做**）

| # | 落点 | 判据 |
|---|---|---|
| **T2.1** | 删 `SqliteSink`（`src/usage/backends/sqlite*` —— Phase 1 若按 D-6 重排，则它此时是一个目录）：连带 `INSERT INTO usage_record` 与 SQLite 的列绑定 | 写路径 `grep -rn "INTO usage_record" crates/hydra-server/src/` 零命中 |
| **T2.2** | 删 `SqliteUsageQuery` 与 4 条 `SELECT`；删 `DEFAULT_USAGE_SINK`；`HYDRA_USAGE_SINK` **必填**（未设 ⇒ 拒启，文案列出可接受值并说明"为什么必须显式"） | 真二进制实测：不设该变量 ⇒ exit 1 且文案列出取值；`check_documented_defaults.cjs` 的 `UNVERIFIED_OK` 增加该行（"必填、无默认"）并**删掉** `sqlite` 那条字符串比较 |
| **T2.3** | `RETIRED_USAGE_SINKS = ["sqlite"]`（含理由与替代）+ `BackendError::Retired` | 真二进制实测：`HYDRA_USAGE_SINK=sqlite` ⇒ exit 1，文案点名 `sqlite` 已退役、并给出 `clickhouse`（若 D-2 通过也列出 `none`） |
| **T2.4 ✅(2026-10-07)** | （D-2 通过时）`src/usage/backends/none.rs`：丢弃型 sink（把每条丢弃计入 `hydra_usage_records_dropped_total{reason="sink_disabled"}`）+ `ReaderContract::Unavailable{why}`；启动 **WARN** 一行说明"本节点不计量用量" | 单测 + 演练：`none` 下节点起得来、`/usage` 回 503 `usage_store_unavailable`、丢弃计数增长、`/health` 与数据面不受影响 |
| **T2.5 ✅(2026-10-07)** | `src/usage/testing.rs`：记录型 sink（收集批次）+ 定值 reader。**不进 `REGISTRY`** | `cfg` 断言：`REGISTRY` 里没有 `memory`/`testing` 这类 kind（守卫 + 单测各一条） |
| **T2.6** | 删集群模式那条"必须 clickhouse"的特例校验（只剩一个后端时它是同义反复） | 演练：集群起得来（三个成员 + CH）；`grep` 该文案零命中 |
| **T2.7** | **演练环境的唯一所有者**：新增 `integration/_usage_env.py`（`usage_env() -> dict` 给出 `HYDRA_USAGE_SINK` 与后端所需的 URL），把 **27 个**今天不设该变量的演练 + CI 的 `ui-e2e` 作业（`ci.yml:1162-1171`）+ `scripts/e2e-local.sh:113-118` + `scripts/handover.test.sh:39-45` 全部改为从它取 | 反向证伪：把 `usage_env()` 改成一个非法值 ⇒ 27 个演练同时红（说明它们**真的**在用它，而不是各自还留着一份内联值）；`grep -c HYDRA_USAGE_SINK integration/*.py` 只剩 helper 一处 |
| **T2.8** | **删表的迁移（唯一删法）**：新增 `crates/hydra-server/migrations/0013_drop_usage_record.sql`（`DROP TABLE IF EXISTS usage_record;`）；同步 `tests/migrate.rs`（`EIGHT_BUSINESS_TABLES` 去掉该表并**新增一条"它不存在"的断言**；`the_sub_tenant_usage_index_exists` 退役）。**不得改既有迁移**（checksum） | 全新库 `.tables` 无该表；升级既有库后它消失；`migrate.rs` 逐条绿；**反向证伪**：把 `0013` 从列表里拿掉 ⇒ 断言变红（证明"删表真的发生"是被测的，不是被假设的） |

---

## Phase 3 — 覆盖迁移 + 文档/守卫同步（**T3.1 已完成；其余待做**）

| # | 落点 | 判据 / 移交 |
|---|---|---|
| **T3.1 ✅(2026-10-07)** | `tests/sqlite_sink.rs`（10 条）改靶到 `usage::testing` 的记录型后端，成为**引擎一致性套件** `usage::conformance` | 10 条断言的语义逐条保留（批量按大小/按时间、退避重试、关闭排空、`Drop` 不可依赖、掩码 key 等） |
| **T3.2** | `tests/usage_query.rs`（31 条 = 14 条 SQLite 腿 + 12 条 CH 腿 + 5 条 kind 分派腿）拆：① 存储无关的腿（窗口边界归一、`group_by` 白名单、`tenant_id` 参数不可改归属、响应信封与 `as_of`、无 reader ⇒ 503）改由 `usage::testing` 的 reader 驱动；② 与 SQLite SQL 绑定的（`sqlite_*` 对手写 SQL 的逐字段比对）**删除**并在文档记账 | CH 侧的 12 条已有；T3.5 核对 `live_clickhouse_aggregate_matches_a_hand_run_query`（`--ignored`）是否逐字段覆盖了被删的那一类，缺则补 |
| **T3.3** | `tests/streaming_usage_persistence.rs`（2 条，整份文件的前提）改靶：断言"流式路径确实调用了 `record`"，落库形状交给 CH 线缆测试 | 流式仍计量这一条**不能丢**（它曾是 P1） |
| **T3.4** | `AppState::for_tests()`：去掉默认注入的 SQLite reader，改为 `None`；需要 reader 的用例显式用 `for_tests_with_usage(…, Some(usage::testing::reader()))`（20 处站点逐个看，不批量替换） | 每个站点要么无 reader（503 契约），要么显式注入 |
| **T3.5** | 文档同步（ADR-0002 §8 的清单）：`ops.md`（§1.1 默认列 → 必填无默认、§9/§9.1、§12）、`design.md`（§9.2/§9.3/SQLite DDL 段/模块图）、`deployment.md`、`cluster.md`、`jiqun-deploy.md`、`design-tenant-api.md`（§547 的论据改挂读契约）、`admin-ui/api-docs.js`、`README*`；`usage-backends.md` 补 SQLite 的退役行 | 逐处 grep 校对：不再有"默认 sqlite"、"集群必须 clickhouse"（该措辞在只剩一个后端后失效）这类句子 |
| **T3.6** | 守卫同步：`check_documented_defaults`（移入"必填无默认"名单）、`check_documented_env`、`check_compose_env`、`check_e2e_contracts`（若演练用到 sink 值）、`check_usage_backends`（新增 `sqlite` 退役行） | 全部守卫绿；`check_usage_backends` 的反向证伪逐条实测 |
| **T3.7** | `ops.md` 补"历史 SQLite 用量"的一次性导出说明（D-4），并写明升级后 `/usage` 只回答 CH 里的量 | 文档可执行（给具体命令），不假装有自动迁移 |
| **T3.8** | `tests/clickhouse_ddl_parity.rs:136` **硬编码 `src/sink.rs`** 的路径随 D-6 搬家改写（它解析那里的 CH INSERT 列名） | 该测试仍 4/4 绿；**且它是"搬家"这件事的传感器**：若搬家时忘了改它，一个纯 ClickHouse 的守卫会先红 |
| **T3.9** | `ci.yml:479` 的 `--test clickhouse_sink --test usage_query --ignored`：CH 套件若改名/拆分，必须同步（`check_ci_wiring.cjs:666-674` 是目前**唯一**保持它接线的东西） | 改名后 `check_ci_wiring` 仍绿（否则会静默脱线：live-CH 覆盖消失而 CI 全绿） |
| **T3.10** | 三个演练的计量腿改靶（`test_streaming_path.py` S4、`test_shutdown_drain.py` N3、`test_client_disconnect.py` C0/C1）：存储从 SQLite 换成 mock CH（照 `test_clickhouse_sink_wire.py`） | 三条判据**语义**不动：流式也计量、排空不丢在飞批次、客户端断开仍计量且与完整回答不可区分；**并且** `test_streaming_path.py:212` 那句 `HYDRA_USAGE_SINK: "sqlite"` 必须消失（`grep` 零命中） |
| **T3.11** | `check_compose_grace.cjs` 的**证据出处**重挂：结论（SIGKILL 会丢在飞用量 ⇒ drain 必须够长）保留，出处从"SQLite `usage_record` 0 行 vs 20 行"改到 CH 侧演练（丢弃计数 + mock CH 行数） | 该守卫的注释与记录里不再引用一个**已无人测**的数字；新出处指向的断言**确实存在**（逐条点名） |
| **T3.12** | `ops.md` 的**破坏性升级**流程：升级前备份 + 导出 `usage_record`（`VACUUM INTO` / `-csv` 两条命令）+ 升级后"该表已不存在、`/usage` 只答 ClickHouse"；**回滚只有两条路**（换回备份文件 / 手工删掉 `_sqlx_migrations` 里的 `0013` 行）；发行说明点名这是破坏性迁移 | 文档命令**可执行**（真跑一次导出）；回滚两条路各实际操作一次并记下结果 |

---

## Phase 4 — 用"第二个后端"证伪模式（D-9 裁定后另开一案）

**先写下预期触碰清单，再动手**（否则事后总能说"那是顺手改的"）：

预期**只新增**：`src/usage/backends/<kind>/{mod.rs,transport.rs}`、`tests/<kind>_sink.rs`(+ wire)、`Cargo.toml` 的 1 行 feature + 1 行 dep、`usage/mod.rs` 的 **1 行注册**、`usage-backends.md` 的 1 行矩阵、`ops.md` 的 env 行。
预期**不得改动**：任何既有后端文件、`usage/engine.rs`、`main.rs`、`proxy.rs`、`tenant_api/**`。

| # | 落点 | 判据 |
|---|---|---|
| **T4.1** | 把上述清单写进任务书（含 `git diff --stat` 的预期形状） | 清单先落档 |
| **T4.2** | 按 `usage-backends.md` §插入模式七步实现该后端 | `git diff --stat` 与清单一致（越界即失败）；共享一致性套件绿；wire 演练（mock）绿；有容器时 `--ignored` 活实例绿 |
| **T4.3** | 反证：故意越界（在 `main.rs` 里加一句 `if kind == "<kind>"`）——看是否有守卫/一致性判据能发现 | 若不能发现 ⇒ **模式还不够硬**，把缺口记入 `usage-backends.md` 的"已知弱点"并新增一条守卫 |

**Phase 4 是本方案的真正验收**：强制定义（Phase 2）证明"这条路被封死"，Phase 4 证明"下一条路很好接"。

---

## 验收门禁（每个 Phase 收口都跑）

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --features hydra-server/server -- -D warnings
cargo clippy --workspace --all-targets --features hydra-server/server,hydra-server/cluster-redis,hydra-server/usage-clickhouse -- -D warnings
cargo test -p hydra-core
cargo test -p hydra-server --features server
cargo test -p hydra-server --features server,cluster-redis,arachne
cargo test -p hydra-server --features server,cluster-redis,usage-clickhouse   # CH 线缆/DDL 一致性
for f in scripts/check_*.cjs scripts/check_i18n.js; do node "$f"; done
node --test scripts/check_usage_backends.test.cjs
python3 integration/test_clickhouse_sink_wire.py          # 报文形状零变化
python3 integration/test_arachne_control_plane.py         # 验收 1/3/4/5
python3 integration/test_data_plane_failover_load.py      # 验收 2
python3 integration/test_startup_knobs.py                 # 启动开关（新增"未设 HYDRA_USAGE_SINK ⇒ 拒启"一腿）
python3 integration/test_tenant_write_publish_failure.py
```

**真二进制的两条新判据**（Phase 2）：不设 `HYDRA_USAGE_SINK` ⇒ exit 1 并列出取值；`HYDRA_USAGE_SINK=sqlite` ⇒ exit 1 并点名退役与替代。

## 回滚

- Phase 1（描述符表）**可独立 revert**：它不改行为。
- Phase 2 的 revert = 恢复 `SqliteSink`/`SqliteUsageQuery`/`DEFAULT_USAGE_SINK` 三个提交；**数据从未被删**（表保留、迁移不动）。
- Phase 4 的一个后端可以单独 revert（它只新增文件 + 一行注册）。

## 未覆盖 / 风险（不粉饰）

1. **SQLite 行级写入形状**从此无人断言（那张表不再被写）——ADR-0002 §7 已记账。
2. **旧数据不迁移**：升级后老用量在 SQLite 文件里，Hydra 不再读它（D-4 给导出说明，不给自动迁移）。
3. **两个答案仍在**：`/api/v1/stats/usage`（进程内计数器）与租户 `/usage`（持久化存储）口径不同，本方案只写明边界、不合并。
4. **`none` 之下用量不可追回**：只计数丢弃，不落任何地方（这也是它必须**显式**且**响亮**的原因）。
4b. **`none` 与「没有 reader」是两件事**：ADR D-7 要求读契约由描述符显式声明；实现时必须钉住「`ReaderContract::SameBackend` ⇒ `query.is_some()`」，否则会出现「配了后端但 `/usage` 静默 503」。
4c. **演练环境的默认值**：T2.7 之后演练用哪个 sink 由 helper 决定；若 helper 给 `none`，那些「读回用量」的演练就永远测不到落库 ⇒ helper 的默认值必须是**能落库的后端 + mock URL**，并在文件头写明。
5. **候选后端的线协议事实需在实现前复核**：`usage-backends.md` 的矩阵逐条标出"已验证/UNVERIFIED"，实现该后端时必须先把 UNVERIFIED 的项测掉。
