# Oracle 复审问题的分批修复（2026-09-29）

**状态**：批次 0 **已完成并验证**；批次 1 **16 项中的 15 项已完成**（余下 1 项 `XTRIM` 相关已做，剩 3 个待决策项，见 §2）；批次 2 **文档部分已完成**（见 §3）
**状态（最新，随每轮更新 —— 上面那行是批次 0–2 时期的历史口径，不要当现状读）**：批次 3–6 的可判定项早已清空；此后每轮产出的是**新一轮对抗性复审**发现的项。**可判定池子已清空**（§2s 那 6 条在 §2t/§2u/§2v/§2w/§2x 全部闭环；§2r 的候选排除 provider 归因在 §2y 落地）；**待决策 0 项（截至 2026-10-09 "全部收敛"轮 —— 上一版这里写的是"4 项仍待取向"，D-4/D-6/D-7/D-10 已在这轮全部按收敛方向拍板，逐条见下面决策表）**。余下两项属从记录中浮出的实现级收紧：**DD-1** `HYDRA_REDIS_MODE` 设置即校验（已随本批代码落地）、**DD-2** breaker `threshold`/`probe_interval` 不可配（记录为固定默认）。
* **已闭环（不再占决策位）**：**D-2**（第八十一轮）、**D-3**（第二百一十轮核实：**随转发层退役消失**，不是选了某条路）、**D-5② / D-13① / D-15② / D-16①③ / D-17①**（第一百零九 / 二百一十轮落地）、**D-8**（第一百零九轮：**前提随 edge 角色退役消失**）、**D-12**（第二百一十轮按"标注，不改写"落地）。最近几轮走的是"**把公开断言/文档里点名的东西真跑一遍**"这条路（§2ah–§2as），每轮都命中真问题：§2as 就是从一个**我自己刚写就过期的公开数字**顺下去，摸到 `[[bin]]` 目标**没有** `forbid(unsafe_code)` 这个真缺口。**第一百一十六轮起的产品代码**（`hydra-core/src/limit.rs`、`hydra-server/src/proxy.rs`）与 drill 前置断言见 §2cw。再往前推进需要至少一项决策 —— 或对**新近改动**（§2t–§2as 的 JS/测试/守卫/文档）再做一轮对抗性复审，历史上每轮都产出真问题。**第一百一十七轮**：`MatchCtx` 的 `Debug` 改为**脱敏**手写实现；并按最初指令**派了两个只读 Oracle 子代理**，独立复现并修掉两条 P1 守卫缺陷（`check_source_purity` 能被一个 `'{'` 永久关掉、`check_ci_wiring` 把**注释**当接线），新增决策项 **D-16**（`matching_key` 明文存活该列是否封存）—— 见 §2cx；**第一百三十八轮**又把"admin 写响应是否回带校验告警"记为 **D-17**（实测：写入路径自己会 reload ⇒ 告警**已经**在写入那一刻进 leader 日志，响应体里没有；`201` 干净不代表角色没问题）—— 见 §2du。最近几轮见 §2s–§2cx；跨会话入口见 `INDEX.md`。
**来源**：本轮 8 路 Oracle 子代理复审（HEAD `8b7b3c6`）+ 维护者自己的门禁实测
**基线 HEAD**：`8b7b3c6`（批次 0/1 的改动尚未提交）

---

## 0. 本轮为什么会有这么多"门禁说绿、实际是红"

一句话：**证据链本身坏了**，不是某一个 bug。三条独立原因：

1. `.acceptance/` 的脚本用 `cmd 2>&1 | tail -N || fail=1` 判定，管道退出码来自 `tail`（恒 0）；
   `grep … && fail=1` 判定的是文本而不是退出码；`t3-baseline.sh` 连 `exit` 都没有。
2. **CI 的 `optional-features` job 自 `e06b477`（2026-09-20，sub-tenant v2）起就是红的**：
   `cargo clippy …--features server,cluster-redis,usage-clickhouse -- -D warnings` 报
   `too_many_arguments (8/7)`（`tenant_api/handlers.rs::forward_write`，exit 101）。
   而 INDEX / v2 / v3 计划都写"两套特性组合 clippy 0 告警、全绿"。
3. 被 4 处文档写成"既有基线失败"的 `cluster-redis` 下 `admin_api` 2 例，**是一次真回归**：
   `git merge-base --is-ancestor c3eaa6f 692655d` = YES —— 用来证明"非本次回归"的那个
   "干净 HEAD worktree" `692655d` **本身就包含** `events.rs` 里 `Applied(200)` → `Pending(202)`
   的语义翻转，而两例测试的症状正是"期望 200，实得 202"。

批次 0 就是修这三条 + 一批当天可做、风险极低的缺陷。

---

## 1. 批次 0（已完成，逐项含验证证据）

| # | 缺陷 | 改动 | 验证（真实执行） |
|---|---|---|---|
| 1 | CI `optional-features` clippy 恒红 | `forward_write` 的 6 个请求参数收进 `ForwardedWrite` 结构体（仍 `#[cfg(feature="cluster-redis")]`）；补文档说明 A-2 前置 5 的令牌契约 | CI 那条命令**逐字复现**：修复前 exit 101 → 修复后 exit 0 |
| 2 | `mask_key` 在 `L == 14` 时返回**完整密钥**（`len-14 == 0` 颗星），且 `L == 15..17` 只藏 1–3 个字符；它是 provider-key 管理面与用量库**唯一**的脱敏手段 | 长档下界 `14 → 20`（隐含中段 ≥6 字符、≥30%）；新增**长度扫描守卫** `mask_key_never_round_trips_at_any_length`（1..=72 逐长度断言"绝不回显原文"）；`HANDOFF.md` 的格式说明同步 | `cargo test -p hydra-core` 全绿（含新守卫）；旧断言 `assert_eq!(mask_key(k14), k14)` 已反转为 `assert_ne!` |
| 3 | 唯一用量 flush 钩子只监听 SIGTERM/SIGINT，而官方 systemd 单元是 `KillSignal=SIGQUIT`、官方升级流程第一步也是 `kill -SIGQUIT` | 两个钩子（`spawn_sink_flush_on_shutdown`、`spawn_registry_unregister_on_shutdown`）都加 SIGQUIT 分支；`ops.md` §13.5b 补"哪些信号会 flush" | **活实例实测**：SIGQUIT 后日志依次出现 `pingora_core::server: SIGQUIT received…`、`hydra: SIGQUIT: flushing usage sinks`、`hydra: usage sinks flushed`（旧代码不可能打印第二行） |
| 4 | `docker-compose.yml`（官方推荐栈）把**无认证**的 ClickHouse 发布到所有网卡（`usage_record` 是计费/用量数据）；`mock-tenant`（对任意 key 返回 allowed）同样全网可达 | 两处端口 + cluster compose 的 CH 端口改 `127.0.0.1:` 绑定，并写明理由与"需要远程就加凭据"的替代路径 | `python3 yaml.safe_load` 解析三个 compose 文件；端口清单逐个打印确认 |
| 5 | `docker-compose.local.yml` 内联的 `usage_record` DDL 停在 v3 之前（缺 `sub_tenant_id`），而 sink 会写该列 → 干净卷上每次 flush 都 `NO_SUCH_COLUMN_IN_TABLE` 并最终丢弃 | 补齐列 + 追加幂等 `ALTER TABLE … ADD COLUMN IF NOT EXISTS`（覆盖既有卷）；注释点明必须与 `environment/clickhouse/init.sql`、迁移 0011 同形 | 程序化比对：`init.sql` 与 compose 内联 DDL 的列名列表**完全一致**（各 15 列） |
| 6 | `integration/run.sh` 与 `e2e_proxy_test.py` 的默认 admin token 都是 14 字符 < `MIN_ADMIN_TOKEN_LEN=16`（服务端 fail-closed）→ 集成门禁**必然启动失败**；e2e 还完全没有 `HYDRA_ENCRYPTION_KEY` | token 默认值改 19 字符；`run.sh` 前置长度断言；e2e 每次运行生成随机 32B 主密钥；`README.md`/`test_crud.py` 默认值同步 | `bash -n` / `py_compile` / AST 校验；长度断言自检 |
| 7 | 三个 SDK 唯一的接口指向 2026-09-17 **已删除**的管理面路由，且配 admin 端口（8081）→ 100% 失败，而它们的 mock 测试复刻了错路径因而永不报警 | 路径改 `/tenant/{tid}/api/v1/auth/cache/invalidate`、端口/示例改数据面 8080；三个 SDK 各加"精确路径 + 不得含 `/api/v1/tenants/` + tenant id 转义"回归断言；README 更正"leader discovery 只是历史优化，每个数据面节点本地应答（design §6.4 A-1）" | Go `go test ./...` 11 PASS / `go vet` 干净；TS `npm test` 13 pass / `tsc --noEmit` 干净；Python `unittest` 12 OK；`grep -rn "api/v1/tenants" tools/ integration/` 无存活代码路径 |
| 8 | `.acceptance/` 门禁**结构上无法失败**（见 §0.1） | `phase-b-gate.sh` / `tenant-api-gate.sh` / `t3-baseline.sh` 全部改为 `run()` 统一以**命令自身退出码**判定 + 累积 `fail` + `exit $fail` + 汇总表；`tenant-api-gate.sh` 的"依赖防火墙"从裸 `cargo tree`（恒 0）改为 grep 判定；`t3-baseline.sh` 加前置检查（活 CH / Redis）并真正执行活实例 `--ignored` 用例；`findings-disposition.py` 声明"只核计划文本"、把失效行号标记换成符号名，并新增 3 条**可执行断言** | 用**真实脚本里的 `run()` 函数**（`sed` 抽取后 source）证明：`exit 101` → `fail=1`、脚本 exit 1；`findings-disposition.py` 27/27 通过、exit 0 |
| 9 | `tests/e2e/seed.sh` 播种失败仍打印 `+ providers` 且 exit 0（CI 的 seed 步骤不是门禁）；`scripts/load_test.sh` 声称的三项校验一项没做、恒 exit 0 | seed.sh：`post()` 校验 2xx 并回显错误体、拒绝 `jq` 产出的 `null`、新增 7 个资源的**读回校验**、缺 `jq` 时明确失败；load_test.sh：撤销"假断言"（`|| true`、匹配任意行的 grep、末尾 `echo`），把分布检查变成 opt-in 且**真断言**（缺 `X-Echo-Instance` 即失败），无 `oha`/`wrk` 时默认失败（`SKIP_LOAD_MEASUREMENT=1` 显式豁免），头部改写为"测量工具 + 确定性门禁在别处" | seed.sh：对不可达 admin → exit 1 且给出原因；对**真实实例**→ 8 个资源全 201、读回校验通过、exit 0。load_test.sh：无负载工具 → exit 1；`SKIP_LOAD_MEASUREMENT=1` → exit 0；开启分布检查而上游不回 header → exit 1；比例判定逻辑用 750/250（PASS）、900/100 与 500/500（FAIL）三组输入自检 |

**批次 0 结束时的全量门禁**（本机，HEAD = `8b7b3c6` + 上述改动）：

```
cargo fmt --check                                              exit 0
cargo clippy --workspace --all-targets --features hydra-server/server -- -D warnings            exit 0
cargo clippy --workspace --all-targets --features hydra-server/server,hydra-server/cluster-redis,hydra-server/usage-clickhouse -- -D warnings   exit 0   ← 修复前 exit 101
cargo test -p hydra-core                                       17 套件全绿（新增长度扫描守卫）
cargo test -p hydra-server --features server                   500 passed / 0 failed / 1 ignored
python3 .acceptance/findings-disposition.py                    27/27，exit 0
活实例：SIGQUIT → usage sinks flushed（见 #3）
活实例：tests/e2e/seed.sh → 8/8 HTTP 201 + 读回校验，exit 0（见 #9）
```

---

## 1b. 批次 1 已完成项（第二轮，逐项含反向证伪）

每一项都做了**反向证伪**（临时把修复去掉，确认测试真的会红），这是本项目对"测试有效"的一贯要求。

| # | 缺陷 | 改动 | 验证 / 反向证伪 |
|---|---|---|---|
| P1-1 | **上游 302 会把真实 provider key 送给第三方**：reqwest 默认跟随 10 跳，跨主机只清 `Authorization`/`Cookie`，而 Anthropic 家族凭据走 `x-api-key`；跟随后的 200 还会被当"成功"计入用量 | `ProviderClient::new()` 加 `.redirect(Policy::none())`（含失败回退路径也带上策略）；3xx 因此回到常规非 2xx 处理（熔断+转移） | 新测试 `a_redirect_is_not_followed_so_the_provider_key_cannot_be_replayed`：起两个 listener，"上游"答 302 指向"收集器"，断言收集器**从未被连接**。证伪：删掉策略 → 测试 FAIL（收集器被访问） |
| P1-4 | **硬编码 300s total timeout 从中间切断长流**：`ClientBuilder::timeout` 覆盖到 body 结束，>300s 的生成拿到 200 + 半截 SSE 却照常计费；还让"首字节超时"告警永不触发 | 删除 client 级 total timeout；新增 body **空闲**期限 `HYDRA_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS`（默认 120，`0` 拒绝）+ 指标 `hydra_upstream_stream_idle_timeout_total`；空闲超时**计入熔断**（唯一无歧义的上游侧 mid-stream 证据），客户端断连仍不计 | 两臂测试：`a_wedged_stream_is_cut_by_the_idle_bound`（1s 窗口、客户端 30s，必须在 <10s 内结束）与 `a_slow_but_alive_stream_is_not_cut_by_the_idle_bound`（每 300ms 一帧共 2.4s，1s 窗口，body 必须完整）。证伪：把窗口改成 3600s → 前者 FAIL 且耗时正好 30.000s（= 客户端自己的超时，说明没有别的东西在兜底） |
| P1-12 | **SNI 查表区分大小写**（证书表键是小写）→ 混合大小写 SNI 既 miss 精确键也 miss 通配键，握手被拒 | `HydraCertStore::lookup` 入口 `to_ascii_lowercase`（与同文件 `sni_matches_host` 已有的折叠一致） | `store_then_lookup_roundtrip` 增加 `ACME.com` / `API.ACME.COM` 命中断言 + 后缀混淆负例。证伪：改回不折叠 → FAIL（"mixed-case exact SNI must match"） |
| P1-7 | **operator `key_prefix_binding` 只校验"非空"且全局生效**：`"s"` 这类裸前缀被接受，命中后跳过子租户路由门，静默改写**所有**租户路由或使其 503 | 抽出唯一前缀形状规则 `hydra_core::sub_tenant::validate_prefix_shape` + `prefixes_collide`；子租户校验器改调它（单所有者）；admin POST/PUT 复用 → 400 `empty_key_prefix`/`invalid_key_prefix`；`config::validate` 增加 binding↔sub_tenant 跨命名空间重叠告警 | core：`prefix_shape_rule_is_shared_and_total`、`prefix_collision_predicate_matches_the_validator`；server：`admin_api` 里裸前缀/非 ASCII 前缀 POST 与 PUT 均 400 且快照未被改动。证伪：让检查恒返回 None → 断言 FAIL |
| P1-6 | **DEFERRED 事务让 `busy_timeout` 形同虚设**：读后写的事务在其他人提交后会**立即** `SQLITE_BUSY`（WAL 快照失效，不进 busy handler）→ 管理写变 500，破坏"配额边界幂等收敛"契约 | 新增 `db::begin_write`（`pool.begin_with("BEGIN IMMEDIATE")`）并用于 12 个读后写事务；`classify_db_err` 把 `SQLITE_BUSY`(5)/`BUSY_SNAPSHOT`(517) 映射为 **503 `storage_busy`**（可重试）而非 500 | 新测试文件 `tests/sqlite_write_lock.rs`：并发写者必须**等待**后成功（而非失败）。证伪：`begin_write` 改回 `begin()` → FAIL，错误正是 `code: 5, "database is locked"` |
| P1-15 | **三个 SDK 把 200/202/503 压成 pass/fail**：`202 pending` 被当成功（body 丢弃），`503 unavailable` 被当**节点故障**而隔离+换节点，丢掉"集群未被通知"这一信号；`wait`/`timeout_ms` 从未发送、trace id 被丢 | 三语言统一：结构化结果（state/invalidated/nodes_applied/nodes_total/lagging/event_id/waited_ms/trace id）+ `waitMode`/`timeout_ms` 客户端校验 + 专用 `InvalidatePendingError`/`InvalidateUnavailableError`；**fleet-aware 应答一律不隔离不轮换**，非 fleet-aware 的 500/503/404/405 保持原行为 | Go 22 PASS + `go vet`、TS 28 pass + `tsc --noEmit`、Python 29 OK（维护者又独立重跑了 Go/Python 两套）；覆盖 200 applied / 200 single_node / 202 pending / 503 unavailable / 参数出现与缺省 / 错误路径 trace id |

**P1-6 顺带发现的约束（已写进代码注释，避免后人再踩）**：`BEGIN IMMEDIATE` **不能**无差别套到只读快照事务上。`ReplicationContent::load` 必须保持 DEFERRED —— 它整个事务就是为了"十份 row-set 来自同一快照"，一拿写锁就会把并发写者堵到 `busy_timeout` 再失败。第一版替换把它一起改了，`a_read_transaction_is_not_torn_by_a_concurrent_writer` 立刻变红；现已回退该处并写明"不要改成 `begin_write`"，该测试就是这条边界的守卫。

**P1-1/P1-4 顺带发现的测试纪律**：新增的集成测试里，`assert_eq!(resp.status(), 200)` 在**全套并行**运行时会偶发失败——租户鉴权那一步是活的 `MockServer`，负载高时会超时。凡是"先拿到 200 再测别的东西"的用例，都要用一个带重试的 helper 去取 200（重试不会削弱被测量的性质：真要坏了每次都会失败），否则就是给别人留一个 flaky。

**第二轮结束时的门禁**：

```
cargo fmt --check                                        exit 0
clippy(server) / clippy(server+cluster-redis+usage-clickhouse)  exit 0 / exit 0
cargo test -p hydra-core                                 239 passed / 0 failed
cargo test -p hydra-server --features server             506 passed / 0 failed / 1 ignored（连跑 3 次一致）
python3 .acceptance/findings-disposition.py              27/27（+3 条可执行断言）
node scripts/check_i18n.js                               338 keys × 4 locales OK
SDK：go 22 PASS / ts 28 pass / py 29 OK
```

---

## 1c. 批次 1 第三轮完成项（含反向证伪）

| # | 缺陷 | 改动 | 验证 / 反向证伪 |
|---|---|---|---|
| P1-2 | **下游请求体读取没有总期限**：pingora 对 HTTP/1 body 只有 per-read 期限（每个字节都重置），HTTP/2 完全没有读超时 ⇒ 客户端发一半就停，能把一个 worker task（连缓冲的 body）永久占住，而**不需要有效 api-key** → 跨租户可用性洞。顺带：读取出错被当作"body 正常结束"，把**截断的** body 当完整请求转发给上游（静默损坏） | 整个读取循环包在**总期限** `HYDRA_REQUEST_BODY_TIMEOUT_SECS`（默认 60，`0` 拒绝）内 → **408 `request_body_timeout`** + 关闭（不排空：这个客户端本来就不发）；读取出错 → **400 `request_body_read_error`** + 关闭（fail-closed） | 新测试 `a_client_that_never_finishes_its_body_is_cut_by_the_deadline`（裸 TCP：完整头 + `content-length: 1000000` + 半个 body + 静默，断言 408 且在 10s 内）。证伪：把期限改成 60s → FAIL，客户端 15s 读超时（"the proxy must answer, not hang"），即旧行为确实是永久挂住 |
| P1-13 | **`config_version` 解析失败被静默降级为"从未设置"** → `replica_is_current(.., 0)` 对空副本返回 true，一个"无法证明自己持有当前配置"的节点可被判定已同步并赢得租约 | 新增 `db::ConfigVersion{Absent,Value,Corrupt}` 三态（保留原文本以供日志）；`replica_is_current` 对 `Corrupt` **fail-closed**（警告 + false）；`materialize` 对 `Corrupt` 照常应用快照并**顺带修复**标记（内容与标记同事务写入）；`ConfigStore::load` 对 `Corrupt` 用保守水位并**大声警告**，不拒绝启动（否则元数据损坏会变成整机不可用） | 在 `version_helpers_roundtrip` 里新增：写 `'not-a-number'` 后断言 `replica_version == Corrupt(...)`、且 `!replica_is_current(&pool, 0)` 与 `!replica_is_current(&pool, 8)`。证伪：把 `Corrupt` 改回 `Absent`（旧语义）→ FAIL |

---

## 1d. 批次 1 第四轮完成项（P1-8，含反向证伪与量化验证）

**缺陷**：裁剪固定 `XTRIM MAXLEN 10000`（每 30 s），且**只要删掉条目就 bump generation** ⇒ 事件速率超过 `10000 / 30 ≈ 333/s` 时每一次裁剪都会删条目 ⇒ **每 30 s 清空全集群 L1+L2 认证缓存**，与消费者是否落后无关；单个租户每分钟 2 次 invalidation 即可永久维持（节流只限速率、不限后果）。

**改动**：
1. Lua 裁剪脚本改为**两个返回值** `{removed, bump}`，并在裁剪**之前**用 `XRANGE … COUNT (XLEN − maxlen)` 取出**本次将被删掉的最新一条**的 id（第一版错误地拿"流里最后一条/最新存活条"去比，几乎必然 bump，测试立刻把它挡下来了）。
2. 新增 `InvalidationStream::slowest_live_watermark(live_nodes)`：读存活节点的 applied watermark（`hydra:{ctl:inv:applied}:<node>`），取**最慢**的一个；**全部存活节点都必须有 watermark** 才返回 `Some` —— 少一个就证明不了（该节点从未应用、位置未知）。id 用 `(ms, seq)` 数值比较（`9-1 > 10-0` 的字典序陷阱）。
3. `trim_and_maybe_bump(maxlen, min_applied)`：仅当"最新被删 id ≤ 最慢 watermark"时**不 bump**（证明所有人都已应用）；其余一切情况（无存活视图、有节点无 watermark、watermark 不可解析）**照旧 bump**，安全方向不变。
4. `spawn_trim_task(...)` 增加存活视图参数，并在 `main.rs` 里**移到共享存活视图构造之后**（此前 spawn 在视图之前，任务根本看不到 fleet）。
5. **零指标的链路补上两个计数**：`hydra_invalidation_trimmed_total`（裁剪条目数，无论是否 bump）与 `hydra_invalidation_generation_bumps_total`（真正"丢了没人读过的条目"的次数）；裁剪失败也记 `hydra_control_poll_total{result="invalidation_trim_error"}`。`cluster.md` §5.1a 与 `ops.md` §13.7 记录新语义与阈值。

**验证**（真实 Redis 6380）：
- 单测：`a_trim_of_applied_entries_does_not_bump`（最慢 watermark 覆盖被删范围 ⇒ 不 bump）、`a_trim_that_drops_an_unapplied_entry_still_bumps`（有人落后 ⇒ 必须 bump）、`an_unprovable_trim_point_disables_the_optimisation`（无视图/有节点无 watermark ⇒ 不做优化）。
- **量化**（新集成测试 `tests/invalidation_trim.rs`，独占 Redis DB 58/59）：4 轮 × 150 事件 = 600 事件、速率 1.5× 阈值，且每个存活节点都跟上 ⇒ `generation == 0`（**零次全集群清缓存**），同时断言流确实被裁剪回 `maxlen`。
- **e2e**：真实消费者 + 真实 20 ms 裁剪任务，等 watermark 追上后 ⇒ 缓存条目**存活**、generation 保持 0。
- **反向证伪**（两个层级各一次）：把 watermark 参数换成空串（等价于旧行为）⇒ 单测 2 例 FAIL、集成 2 例 FAIL（"a keeping-up fleet must not get its caches wiped"）。恢复后全绿。

**顺带修掉一个我自己引入的测试缺陷（并纠正了上一轮的归因）**：上一轮我把"套件对负载敏感"归因为鉴权跳超时，**归因错了**。真实原因是我的测试 helper 把 wiremock `MockServer` 句柄搬进 helper 后**提前 drop** —— 而 `MockServer::drop` 会**关掉服务器本身**，且 wiremock 的实例是**进程内池化共享**的，所以它既杀掉自己的鉴权跳（Hydra 正确 fail-closed 返回 503），也会**连带影响并发运行的其它用例**（这正是上一轮"既有用例也偶发失败"的原因）。修法：helper 返回 `MockServer` 句柄，调用方用命名绑定（`let (_auth, state) = …`）持有到测试结束。此后该套件连跑 5 次全绿、耗时回到 2.5 s（之前会卡到 10 s 重试预算耗尽），重试预算也从 ~10 s 降回 ~2 s（只用于吸收代理异步绑定）。

---

## 1e. 批次 1 第五轮完成项（P1-5：ClickHouse 提交后重试重复计费）

**缺陷**：`INSERT` 的响应丢失（读超时 / 回复在途时连接被重置）被归类为"批次没落地"并**整批重投**，但那次 INSERT 很可能**已经提交**；表是普通 `MergeTree`，没有幂等键 ⇒ 每次重试都追加一份完整的用量行 ⇒ **用量/配额/计费被重复计算**且事后不可见。

**先在活实例上把方案验证清楚（ClickHouse 24.3.18）**：
- `SETTINGS insert_deduplication_token='X'` + 表设置 `non_replicated_deduplication_window` ⇒ 同 token 两次 = **1 行**，换 token = 2 行；
- 表**没有**该设置时：token 被接受但**静默忽略**（不报错，只是不去重）；
- 既有表可 `ALTER TABLE … MODIFY SETTING non_replicated_deduplication_window = 1000` 打开；
- `SETTINGS` 必须在 `FORMAT` **之前**（反过来是语法错误）。

**改动**：
1. `sink.rs` 每次 flush 生成**一个** batch id（`<pid>-<nanos>-<seq>`，无新依赖），重试期间**复用**；`clickhouse_insert_statement()` 把 token 拼进语句（token 过滤到安全字符集，避免拼串注入）。
2. 插入器签名改为 `Fn(&str, Vec<UsageRecord>)`（SQLite 侧忽略该 id 并注明理由：它是单事务提交，不存在整批重投）。
3. DDL：`environment/clickhouse/init.sql` 与 `docker-compose.local.yml` 内联 DDL 都带上该表设置；本机活实例已按文档执行 ALTER。
4. 文档：`ops.md` §5.4 增加"重试幂等迁移"块，**明确写出残余**：批次跨重试窗口后重投、或批次组成变化时会拿到**新** token，那些边界仍可能重复；根治需要**行级**幂等（稳定行键 + `ReplacingMergeTree`，读路径去重）= 一次 schema 迁移，已记录而非默默假设。

**验证**：
- 单测 `one_batch_keeps_its_dedup_token_across_retries`：同一批次 3 次尝试必须**同一个** token，不同批次必须不同；断言精确。**这个测试当场抓到了我自己的实现 bug**：第一版把 `new_batch_id()` 放在重试循环**内部**（每次尝试都换 token ⇒ 去重完全失效），测试立刻 FAIL 并打印三个不同的 id。
- **活实例验收**（把原先只断言"不 panic"的 `clickhouse_sink_writes_batch` 改成真断言）：sink 落库后 `SELECT count()` 必须 +2；随后用**同一 token** 原样重投一批，probe 行数必须仍为 1。
- **反向证伪**：在活实例上把表设置改回 `0` ⇒ 该验收测试 FAIL（`left: "3"` vs `right: "1"`），证明该 DDL 设置是**承重**的，不是装饰。恢复后连跑 3 次全绿。
- 顺带修掉该测试**不可重跑**的问题（固定 token/tenant 会让第二次运行看到第一次的行）：改为 per-run 唯一 token + tenant，连跑 3 次全绿。


---

## 1f. 批次 1 第六轮完成项（P1-9、P1-16）

| # | 缺陷 | 改动 | 验证 / 反向证伪 |
|---|---|---|---|
| P1-9 | **物化失败 3 次后节点永久失去当选资格**：`MaterializationGuard` 每个快照只重试 3 次，用尽即**永不**再试；而控制客户端不会重投"内存水位已越过"的版本 ⇒ 只剩两条回头路：一次**更新的配置写入**（需要一个能工作的 leader —— 恰恰是"没有可当选节点"的集群给不出的）或**重启**。瞬时故障（SQLite 被锁、磁盘一度写满后恢复）会造成**永久**不可当选。 | 改为**次数不限、速率受限**：失败后 1s→2s→4s…（上限 60s）指数退避，新快照立即重置退避与失败计数；成功后清零。`materialize` 幂等、被版本闸门与 `in_flight` 串行保护，所以重试安全。`take_retry()==None` 的日志从"nothing left to retry"改为"deferred (no snapshot yet or still backing off)"（前者读起来像"放弃了"）。新增 `hydra_replica_materialize_retries_total{outcome=attempt\|succeeded\|failed\|throttled}` —— 这个"不能物化 ⇒ 不能当选"的状态以前既永久又不可见。 | 三条测试：`a_failed_materialization_is_always_offered_again_after_a_backoff`（连续 3 次失败后**第 4 次仍被提供**）、`the_retry_backoff_grows_and_is_capped`、以及真实 SQLite 触发器故障注入的 e2e `a_transient_materialization_failure_heals_without_a_new_snapshot`（**不换快照、不重启**，故障移除后闸门自行重开）。**反向证伪**：把旧的"3 次后返回 None"塞回去 ⇒ 两条测试 FAIL，e2e 打印出 **140 个连续的 `false`**（跑满 25s 超时也没重开）——正是"永久不可当选"的直观证据。 |
| P1-16 | 两个"整缓存清除"缺陷：① `del_all_tenants` 循环里用 `?`，**第一个租户失败就早退**，其余租户的 L2 裁决留在 Redis 里 —— 而调用方此时**已经清掉了自己的 L1**，下一次 L1 miss 就会把本该被清掉的裁决**重新回填**（直到各自 allow TTL 到期）；② 紧接着无条件 `record_auth_cache_size(0)`，**谎报缓存为空**，且整条链路零指标。 | `del_all_tenants` 不再早退：逐租户收集成功/失败并返回 `(deleted, failed)`（索引仍会 drop，理由：留着陈旧成员比丢掉更糟，漏掉的租户由 per-key allow TTL 兜底）；`clear_all` 按结果记录 `hydra_auth_cache_clear_total{layer=l1\|l2,result=ok\|partial\|error}`，部分失败时 `warn!` 说明"这些租户会保留裁决直到 allow TTL"；**删掉** `record_auth_cache_size(0)`（它统计的是 L1，L2 部分失败时写 0 就是撒谎）并注明由 `check()` 与新的 clear 指标反映真相。 | `a_failing_tenant_does_not_abort_the_whole_cache_clear`：**真故障注入** —— 把某一个租户的索引键改成字符串，使 `SMEMBERS` 报 WRONGTYPE，断言 `failed == 1`、`cleared == 2`、且另外两个租户的裁决**确实**被清掉。**反向证伪**：把早退的 `?` 放回去 ⇒ 测试 FAIL（整个 clear 直接报错中止）。 |

---

## 1g. 批次 1 第七轮完成项（P1-11、P1-14 + 三处文档纠错）

| # | 缺陷 | 改动 | 验证 / 反向证伪 |
|---|---|---|---|
| P1-14 | **管理面 token 门禁无限速、无锁定、无指标，唯一记录是默认被过滤掉的 `debug!`**（`RUST_LOG=info` 下根本不落盘）。它守的是**唯一**一道能改全部租户/provider/上游密钥的凭据，而隔壁租户面的同类门禁三样俱全。 | 复用既有的 `tenant_api::throttle::Throttle`（固定窗口、进程内）做**按 peer** 的失败预算：`HYDRA_ADMIN_AUTH_FAIL_LIMIT_PER_MIN`（默认 10，`0`/垃圾值回落），超限回 **429 + `Retry-After`**（新增 `err_json_throttled`，与租户面 `respond_json_with_retry_after` 对齐）；未超限仍 401 但**升级为 `warn!`** 并计数 `hydra_admin_auth_failures_total{result=denied\|throttled}`。键用**对端 IP（丢端口）**且**不读 XFF**（XFF 可伪造、会让人轮换桶来爆破）；**成功请求永不被预算拦住**（写进注释与测试），所以不会把运维锁在自己的网关外。 | `repeated_bad_admin_tokens_are_throttled_then_a_valid_token_still_works`：3 次 401 → 第 4 次 **429 且带 `Retry-After`** → 同一 peer 用**正确 token 仍 200**。**反向证伪**：关掉限速 ⇒ FAIL（`left: 401` vs `right: 429`）。 |
| P1-11 | **`/metrics` 的鉴权口径随角色漂移且文档互相矛盾**：edge 上**免鉴权**、leader/all 上要 admin token；随包 UI 的 api-docs 写 "token-free"，而同 crate 的 `static_files.rs` 注释写 "all … and `/metrics` routes remain token-gated"。而官方集群拓扑要求 edge 的 `HYDRA_ADMIN_ADDR` 绑 `0.0.0.0` ⇒ 恰好是最该保护的部署把带 `tenant`/`provider`/`model` 标签（客户标识）的序列公开了。 | **统一为"所有角色都要 admin token"**：edge 分支只保留 `/healthz`、`/readyz` 免鉴权（探针不该需要密钥），`/metrics` 落到与其它角色同一条门禁；`admin-ui/api-docs.js` 的条目改为 `auth: true` 并补 401/429 说明；`ops.md` §9 写明抓取必须带 admin token、给出 Prometheus `credentials_file` 写法，并**明说取舍**（抓取端会持有全域凭据；不接受就绑 loopback 或用注入头的 sidecar，而不是打开端口）。 | `edge_admin_probes_only` 更新为：`/healthz`、`/readyz` 无 token 仍 200；**`/metrics` 无 token 401、带 token 200**。全量套件（含集群）无其它回归。 |

---

## 1h. 批次 1 第八轮完成项（P1-10：主密钥轮换）

**缺陷**：`key_version` 在**唯一**的构造点被硬编码为 1 ⇒ 版本化 AAD 机制是**死代码**；而"换主密钥"没有任何路径：所有密文（provider api-key、租户证书私钥）立刻不可解 → `ConfigStore::load` 失败 → **进程拒绝启动** → 而能重新录入密钥的 admin API 就在这个进程里 ⇒ 只能手工改库。

**改动**：
1. `StaticKeyProvider` 变成**小密钥环**（`version → key`，`BTreeMap`）：`seal` 永远用**当前版本**，`open` 接受环内任意版本；环外版本**照旧 fail-closed**。新增 `KeyProvider::version()`（trait 上，re-seal 的"目标版本"因此可经 `&dyn KeyProvider` 取到）。
2. 新增 `HYDRA_ENCRYPTION_KEY_VERSION`（默认 1；`0`/垃圾值**启动即报错**而不是静默回落 —— 正好是上次 ClickHouse/`config_version` 那类"`parse().ok()` 把错误变成默认值"的坑）与可选 `HYDRA_ENCRYPTION_KEY_PREVIOUS`（+ `_PREVIOUS_VERSION`）构成**轮换窗口**。
3. 新增 `db::reseal_secrets(pool, kp) -> ResealReport`：把 `provider_key` 与 `tenant`（证书私钥列）**在同一个写事务内读完再逐行重封**；打不开的行**如实报告并保持原样**（绝不用垃圾覆盖），报告含 `provider_keys_resealed / tenant_certs_resealed / already_current / failed[]`，`is_complete()` 供退出码使用。
4. 一次性触发：`HYDRA_RESEAL_SECRETS=1` ⇒ 在 **加载配置快照之前**（正是会解密的那一步之前）执行、打印报告、**退出而不服务**；有失败行则退出码 1。选环境变量而非子命令：容器里 `docker run --rm -e ...` 更直接，也不必和 Pingora 的参数解析较劲。
5. `ops.md` §3 写入**可执行的轮换 SOP**（三步：生成新钥+版本号 → 带新钥+旧钥+`HYDRA_RESEAL_SECRETS=1` 跑一次 → 用新钥单独重启并删掉旧钥变量），并明说窗口代价、报告不干净就**不要**删旧钥、以及"丢钥的行不可恢复但会被点名报告"。

**验证**（真实 SQLite + 真实 AES-GCM，断言的是**落库的字节**）：
- `a_rotation_reseals_every_secret_under_the_new_version`：2 个 provider key + 1 个租户证书私钥从 v1 重封到 v2；**新钥单独能读、旧钥单独读不到**、证书公钥 PEM 未被改动；再跑一次幂等（`already_current == 3`）。
- `an_unopenable_row_is_reported_and_left_untouched`：用一个未知版本(7)/未知密钥的行 ⇒ 报告点名该行与版本、`is_complete() == false`、该行**版本仍是 7**（没被改写）。
- `the_key_ring_refuses_versions_it_does_not_hold`：新密文带**新**版本号；只持旧版本的 provider 打不开它。
- **反向证伪**：把 `UPDATE` 里绑定的版本号从新版本改回旧版本 ⇒ `a_rotation...` FAIL（`left: None` vs `right: Some("sk-old-1")`）。
- **顺带抓到我自己的一个真 bug**：第一版 `reseal_secrets` 在**已取得写事务之后**才从 **pool** 读租户行 ⇒ 单连接池（`:memory:`）直接 `PoolTimedOut` 死锁，多连接池则是"读到事务外的快照"。改为**在同一事务内读**（既消除死锁也消除原子性缺口），并把这条理由写进函数文档。

---

## 1i. 第九轮：批次 2 收尾 + 新发现的一个 P1

### 已收尾
1. **T3.3 承诺的归因集成测试补齐**（`terminate_mode.rs::usage_is_attributed_to_the_sub_tenant_that_owns_the_key_prefix`）：断言记录点写入的 `sub_tenant_id` 就是前缀所属子租户、且**原始 key 永不落库**；反向证伪：去掉派生即 FAIL（`left: None` vs `right: Some("st1")`）。v3 计划状态行与本计划批次 2 第 2 条同步改为"已补齐"。
2. **OF-1 用量口径静默丢失**：扫描器按**请求路径**选 schema（`/v1/messages` → Anthropic，同时那条路径也决定凭据头），而字段名来自**上游响应**。`/v1/messages` 打到 OpenAI 字段的上游时，usage 对象解析成功但三个字段全 None ⇒ 记 `Some(Usage{全 None})` ⇒ **200 但 tokens 静默 NULL**。改为两个 schema 都接受两族字段名（并保留"只赋值 Some"语义），双向各加断言；反向证伪：去掉回退 ⇒ FAIL。

### 新发现的 P1（本轮由自己的测试抓出，非清单内）
**集群模式的 token 配额实际上从未生效，而且检查路径会销毁窗口。** `ADD_TOKENS_SCRIPT` 把 **token 数量当作 sorted-set 的 score**（为了 CHECK 好求和），却用 `ZREMRANGEBYSCORE zk '-inf' (now - window_ms)` **按毫秒时间戳**淘汰 —— 真实时钟下 `now - window_ms ≈ 1.79e12` 大于任何 token 数 ⇒ **每次写都清空整个窗口**（`add_tokens` 后窗口只剩最后一条），而 `check_tokens` 在求和**之前**先清空 ⇒ 和恒为 0 ⇒ **永远放行**（且这个"读"路径会破坏它读的窗口）。既有脚本测试看不见它，因为它传的 `now` 是假的 1000/2000（此时 `now - window_ms` 为负，谁都不会被淘汰）。

**修复**：token 窗口改为 **score = 时间、member = `<now>:<tokens>:<salt>`**（计数放进 member，求和时解析；解析不了的旧格式 member 直接跳过，且它们第一次新格式写入就会被淘汰）。`add_tokens` 返回累计值便于断言；`check_tokens` 只做过期清理、不再可能清空活跃窗口。

**验证**（真实 Redis）：新增 `the_token_window_accumulates_and_check_does_not_wipe_it`（两次 60 token ⇒ 窗口 2 条、120>100 ⇒ 连续两次 check 都必须 Denied）；把旧的 `token_accounting_on_real_redis` 从"假 now + 无 token 前缀 member"**改写为真实时钟 + 生产 member 格式**（并写明旧写法正是掩盖该 bug 的形状）；同时修掉 member 碰撞：新增**每实例 nonce**（pid 在容器里可能全是 1，不够），`two_instances_do_not_collapse_the_same_window_member` 用两个 limiter 实例模拟两个节点，断言两条写入**都在**窗口里（旧实现同毫秒会塌成一条 ⇒ 少计，fail-open 方向）。

---

## 1j. 第十轮：核心层 4 修 + 分区/索引/仓库卫生 + CI 补一个从没编译过的特性（并修掉本轮自己的两处自伤）

### A. `hydra-core` 4 项（OF-2 非 SSE 用量尾部、OF-3 SWRR 状态、weight ≤ 0 校验、滑动窗口无界队列）
1. **非 SSE 响应的 usage 尾部**：`UsageScanner` 的"跨 read 拼接"只在 `data:` 分帧行上生效，普通 `application/json` 响应体里 `"usage"` 对象被两个 read 切开时**永不重组** ⇒ 200 但 tokens 全 NULL。改为两条路径共用尾部候选，并**加界**（`MAX_JSON_TAIL = 64 KiB`）：超界即丢弃候选而不是继续攒。新增 `non_sse_usage_split_across_reads_is_reassembled`（切开点选在对象内部，第二个 read **完全不含** `"usage"` 字样）与 `a_huge_incomplete_json_usage_is_not_buffered_forever`（200 KiB 无闭合 ⇒ 必须 `None`）。反向证伪：去掉非 SSE 分支 ⇒ 第一条 FAIL。
2. **SWRR 权重状态溢出**：`current_weight` 用 `i32` 累加，`weight` 同量级时 `i32::MAX` 附近可回绕 ⇒ 选出的"最重"可能变成最轻。改为 `HashMap<String, i64>` + `saturating_add`/`saturating_sub`，并加断言"状态始终落在 i32 范围内"（语义不变、只是不再回绕）。
3. **`weight <= 0` 的配置**：router 只认 `weight > 0`，而校验器只跳过 `weight == 0` ⇒ **负权重 provider 校验全绿但永远选不中**。`validate` 改为 `<= 0` 均报 Warn；同时对负权重额外发一条**显式**警告（是配置错误，不是"软下线"）。新增 `validate_negative_weight_provider_is_reported`。
4. **滑动窗口队列有界**：`limit.rs` 每个 window 的样本队列无上限 ⇒ 高频租户可把内存顶穿。加 `MAX_SAMPLES_PER_WINDOW` / `MAX_TOKEN_SAMPLES_PER_WINDOW = 1_048_576`，到顶时**合并最旧**（保持"总和正确、精度退化"而不是丢新样本）。

### B. OB-3：两个测试文件共用同一个 Redis 库（会互相 `FLUSHDB`）
`registry_reaping.rs` 与 `sub_tenant_data_plane_write.rs` 都写 `real_redis_pool(47)`。每个调用都会 flush 自己的库，所以两者互相删除对方的键，症状是"另一个文件跑过就随机失败"。改 `sub_tenant_data_plane_write.rs` 用 **57**，并**把这条约定变成可执行的守卫**：新增 `crates/hydra-server/tests/redis_db_partition.rs::no_two_test_files_share_a_redis_database`，扫描 `tests/*.rs` 里所有 `real_redis_pool(<n>)` 调用点，任何编号被两个文件声明即失败（无需 Redis）。
反向证伪：把 57 改回 47 ⇒ FAIL 并点名 `db 47: ["registry_reaping.rs", "sub_tenant_data_plane_write.rs"]`；改回 57 ⇒ PASS。

### C. 用量查询的子租户索引（`0012_usage_sub_tenant_index.sql`）
`usage_record` 有 `(tenant_id, created_at)` 索引，而按子租户的查询/聚合要再叠 `sub_tenant_id` ⇒ 大租户下退化为大范围扫描。新增 `idx_usage_record_sub_tenant (tenant_id, sub_tenant_id, created_at)`（`IF NOT EXISTS`，与既有迁移风格一致）。

### D. 仓库卫生（P2；已实测而非推断）
1. `.gitignore`：新增根级 `*.db`/`*.db-shm`/`*.db-wal`/`*.db-journal`、`*.pid`、`__pycache__/`、`*.pyc`。用 `git ls-files -i -c --exclude-standard` 核对新规则**只**命中那 4 个已知残留、没有波及任何正常文件（323 个被跟踪文件里仅 4 个命中）。
2. 取消跟踪 4 个运行期残留：`hydra.pid`、`e2e_test.db-shm`、`e2e_test.db-wal`、`environment/__pycache__/init.cpython-312.pyc`（`git rm --cached`，文件仍在磁盘上；取消跟踪后四者都被新规则正确忽略）。**未提交**。
3. `.dockerignore`：`secure/`（TLS 私钥 + `local-test.env`）与 `**/*.env` 从未被排除 —— 构建上下文是**仓库根**，Dockerfile 只 `COPY bin/hydra`，但**整个上下文都会上传给 builder**。同时 `.acceptance/`(974 MB) 与 `.cargo-cache/`(435 MB) 也没排除。
   实测（`DOCKER_CONFIG=$PWD/.docker`，BuildKit）：修前 `secure/`、`.acceptance/`、`.cargo-cache/` **全部 IN CONTEXT**，上下文 **1.28 GB**；修后三者全部 EXCLUDED，上下文 **32.45 MB**（正好是那个二进制），而正对照 `crates/hydra-server/migrations/`、`environment/Dockerfile`、`bin/hydra` 仍 IN CONTEXT。
（**第十四轮更正**：`bin/hydra` 之所以还在，是因为**没有任何规则排除 `bin/`**，而不是因为文件末尾那两行 `!bin/`、`!bin/hydra` 取反生效 —— 实测把那两行删掉 `bin/hydra` 依然 included。两行保留作防御，但这里的描述原先说反了。）
   （附带教训：**旧版 legacy builder 不认 `**/` 前缀**，用它测这个会得到"全都排除"的假阳性；必须用 BuildKit 验证。）

### E. CI：新增 `alt-tls-backend` 作业（`tls-openssl` 从未被编译器碰过）
`hydra-server/tls-openssl` 是 design §12.1 允许的替代下游 TLS 后端（BoringSSL 不好编时用），但**没有任何作业编译过它** —— 与之前给 `optional-features` 补作业的原因同一形状。它不能并进该作业：`server` 会打开 `tls-boringssl`，两个后端互斥。新作业只做 `cargo clippy --workspace --all-targets --features hydra-server/tls-openssl -- -D warnings`，不需要 Redis。本机已按该命令逐字跑过：**exit 0**。

第十一轮补两处：
1. **该作业确实覆盖了值得覆盖的代码**（不是只验证"特性名能解析"）：`tls.rs` 的整个 `HydraCertStore` / SNI 回调 / 证书解析管线都gated 在 `#[cfg(any(feature = "tls-boringssl", feature = "tls-openssl"))]`（`lib.rs:110`、`tls.rs:33-34`），另有 `listeners.rs:220`、`main.rs:387/1175/1256`、`proxy.rs:349/562` 等。所以这个作业真的会用另一个 TLS 后端编译租户证书路径。
2. **不再依赖运行器镜像里"碰巧装了什么"**：`openssl-sys` 用的是**系统** OpenSSL（`cargo tree -e features -i openssl-sys` 只显示 `openssl-sys feature "default"`，工作区里没有任何地方打开 `vendored`），因此需要头文件 + `pkg-config`。作业里已**显式** `apt-get install -y libssl-dev pkg-config`（几秒钟），而不是赌镜像内容 —— 否则镜像一升级就会以一个语焉不详的 `openssl-sys` 构建错误挂掉。这也是该作业便宜的原因：它**不**构建 vendored 的 BoringSSL，所以不需要 cmake/perl/Go。

### F. 本轮自己的两处自伤（已修，且**都是门禁先被骗过**的）
1. **两个既有测试被我的测试文件手术"掐掉"了**：把新测试插在**既有 `#[test]` 属性与它所属函数之间**，于是 `sse.rs::usage_schema_dispatch_by_provider` 与 `validate.rs::validate_binding_empty_prefix` 变成**无属性的死函数**（`dead_code` + `duplicate_macro_attributes`），**停止运行**。修回属性后两条都通过，`sse` 23 例、`validate` 18 例（净增 2 例=恢复的那两条）。
   教训：`-D warnings` 本身**就是**这类事故的守卫（它确实报了 `dead_code`），所以**每轮收尾必须跑一次 clippy**，而不是只在改产品代码的那轮跑。
   **第十一轮把这条从"断言"升级为"实测"**：在 `crates/hydra-core/tests/limit.rs` 里人工复现该事故（把 `#[test]` 与 `limit_match_all_null_matches_everything` 之间插入一个探针函数）后，`cargo clippy -p hydra-core --all-targets -- -D warnings` **exit 101**，报 `error: function \`limit_match_all_null_matches_everything\` is never used`；还原文件后同一命令 exit 0。也就是说这个守卫是真的，前提是**你得跑它**。
2. **clippy 确实红了，但我的命令报 exit 0**：`cargo clippy ... | tail -15` 的退出码是 `tail` 的。CI `check` 作业的实际结果是 **exit 101**，两处告警都在本轮新增的测试里（`std::iter::repeat(..).take(..)` → `repeat_n`；`(*cw as i64)` → `cw.abs()`，因为 `current_weight()` 已是 `i64`）。
   修：两处改掉；并新增 `.acceptance/round10-gate.sh`，**每个门禁命令用 `"$@"` 直接调用并显式记录 `$?`**，禁止再用管道吞掉状态码。
3. **同一个脚本自己又犯了同一类错（第十一轮自查发现）**：初版脚本最后一句是 `echo "log: $LOG"`，于是**即使汇总打印 `OVERALL=RED`，脚本本身恒 exit 0** —— 谁把它接进 CI / pre-commit / `&&` 都会永远通过。同时它给可选套件的命令**漏了 `--no-fail-fast`**，因此首个失败二进制之后的测试**全部被静默跳过**（汇总只报 "299 passed"，实际总数 644）—— "没跑过的测试"和"通过的测试"看起来一样。
   修：末尾 `exit "$overall"`；汇总循环改成显式 `if`（不再依赖 `[ ] &&`）并只读 `overall`；可选套件命令加 `--no-fail-fast`。
   反向证伪（把脚本的门禁列表换成 `true` / `false` 两个桩命令、并**把 LOG 改到 /tmp 以免截断真实日志**）：带 `exit` 行 ⇒ 退出码 **1**；删掉该行 ⇒ 退出码 **0**。即这个 bug 与它的修复都被实测过。

### G. 本轮验证结果（每条命令**单独**取退出码；脚本 `.acceptance/round10-gate.sh`，日志 `.acceptance/round10-gate.log`）

| 门禁命令 | 退出码 | 说明 |
|---|---|---|
| `cargo fmt --check` | 0 | |
| `cargo clippy --workspace --all-targets --features hydra-server/server -- -D warnings` | **0** | CI `check` 作业逐字命令；修掉 F.2 的两处 lint 后转绿 |
| clippy（`server,cluster-redis,usage-clickhouse`） | 0 | CI `optional-features` 作业 |
| clippy（`tls-openssl`） | 0 | 新 CI 作业的逐字命令（E） |
| `cargo test -p hydra-core` | 0 | **16** 个测试二进制（15 个 `tests/*.rs` + lib 单测），**246 passed / 0 failed / 0 ignored**（含本轮新增的 2 个 `limit` 上限单测） |
| `cargo test -p hydra-server --features server` | 0 | **520 passed / 0 failed / 1 ignored** |
| `cargo test -p hydra-server --features server,cluster-redis,usage-clickhouse`（活 Redis 6380 + 活 CH 8123） | 101 | **644 passed / 2 failed / 3 ignored**（`--no-fail-fast`，含本轮的 migration 索引断言）；2 例即 D-2 的 `admin_api` 决策项，与前几轮完全一致（现在是 `admin_api.rs:1724`、`:3063`，行号随本轮新增测试下移） |
| `python3 .acceptance/findings-disposition.py` | 0 | 27/27 |
| `node scripts/check_i18n.js` | 0 | |

两点口径说明（都是本轮踩出来的）：
- 可选套件必须加 **`--no-fail-fast`** 才能得到 641 这个总数：默认行为是在 `admin_api` 失败后**不再启动后续测试二进制**，会少算一大截。
- **此前记录的 core「239」不再可比**，不要拿它和 246 相减：那是两处测试被 F.1 掐死、且第十轮新用例尚未全部落地时的口径。当前唯一可信数字是上面这条命令在**这个**树上跑出来的数字。
- 三个套件的数字随本轮后续提交面变动：core 239 → 246、`--features server` 517 → 520、可选+活依赖 640 → 644。**净增全部来自新测试**（limit 上限单测 ×2、OC-5 ×1、OC-3 ×1、migration 索引断言 ×1 等），没有测试被删除或改为跳过；其中还含**从死测试恢复的 2 例**（F.1）。

### H. §3b 遗留项的收口核对（本轮逐条回查代码/文档，全部**已关闭**）
§3b 里原先挂着 5 个散点，本轮逐条核对当前树，均已被前几轮的文档修改覆盖，因此从"余"清单中移除：
1. `jiqun-deploy.md:111` 关于 `HYDRA_BREAKER_QUORUM` 的表格项**已改为真话**（"**确实被读取**（`main.rs` 的 `std::env::var`）"），不再是"暂未提供 env 覆盖"。
2. `tenant-api-integration.md` **已补 `forward_failed`**：§6 表格 `:482` 有 `502 forward_failed` 行（与 `admin/mod.rs:687`、`tenant_api/handlers.rs:1098` 的实际取值一致），`:481` 的 `forward_result_unknown` 行也在。
3. `tenant-api-integration.md` §5.2 的 5 处分歧**已按代码订正**：`lagging` 明确写成"未确认"并列出三种来源（含 `wait=none` 时"没人被检查"、`nodes_total=0`）；
   补上了"无 `cluster-redis` 构建一律 `200 single_node`"的例外；补上了 `timeout_ms` 有 `trim()`、`wait` 是逐字节比较的**不对称**；补上了 `event_id` 可为 `null`（`single_node` 与 publish 失败的 `unavailable`）与 `waited_ms` 在 `single_node` 恒为 0。
4. `#[ignore]` 那条"显式记录"的限制**已写进 `ops.md:70`**（planner 把监听地址按**字符串**比较，所以"同端口不同地址"在静态检查阶段查不出来）。
5. `2026-09-18-sub-tenant-v3.md` 的状态行与 T3.3 集成测试：§1i 已处理。

---

## 1k. 第十轮下半：把 Oracle C 丢失的四条（OC-3/4/5/6）找回来并收口

**为什么之前"丢了"**：Oracle C 共报 6 条，其中 **OC-1 → 计划 P1-8**、**OC-2 → 计划 P1-7**（换了编号，所以 `OC-1/OC-2` 这两个标签在计划里零命中），而 **OC-3/OC-4/OC-5/OC-6** 从未被收录 —— 它们在计划全文里一次都没出现。本轮让该复审者（会话仍在）重述全部条目并**逐条在当前树上复核**，结论如下（每条都重新落到 `path:line`）。

| 标号 | 内容 | 本轮结论 |
|---|---|---|
| OC-1 | 失效流 `XTRIM` 每次 trim 都 bump ⇒ 每 30 s 全集群清 L1+L2 | 已修（= P1-8，§1d） |
| OC-2 | operator `key_prefix_binding` 只校验非空 + 全局作用域 | 已修（= P1-7，§1b） |
| OC-3 | 转发链 trace id 不对齐：leader 的审计记录与租户可见 id 断链 | **本轮已修** |
| OC-4 | leader 自身数据面写端点确定性 503，与租户契约"你无需感知/可重试"冲突 | **本轮已修（文档侧）**；行为侧待决策 |
| OC-5 | 写端点被门禁拒绝时 metrics 归因 `endpoint="unrouted"` | **本轮已修** |
| OC-6 | DELETE 幂等 204 与"外来行 404"构成存在性 oracle，与自身"不可区分"声明矛盾 | **待决策**（P3） |

### OC-5（代码，已修 + 反向证伪）
`tenant_api/mod.rs` 的写分支把 `ctx.tenant_api_endpoint = Some(write.label())` 放在 `run_gate` **之后**，而**写 401/403/429/503 响应的正是 `run_gate`**：于是每一次被拒的写都被记到 `endpoint="unrouted"` —— 那个标签的含义是"这不是一条路由"。后果是双向的：按端点告警的运维**看不到写端点的拒绝**，而 `unrouted` 的尖峰（本该意味着路径漂移＝代码/配置 bug）实际是有人在猜写端点的凭据。读路径一直是先把标签设好再进门禁的。
修：把这一行移到 `run_gate` 之前（`write.label()` 只取决于路径，进门禁前就已确定）。`ctx.tenant` **刻意**留在门禁之后并加注释：令牌未验证前，URL 里的租户只是个**未经证实的声明**，拿它当指标标签等于让调用者自选这条失败请求记在哪个租户账上。
新增 `metrics::tenant_api_requests_total(endpoint, status)` getter（沿用既有"`Current value of … (tests)`"模式）与集成测试 `tenant_api.rs::a_refused_write_is_attributed_to_its_route_not_to_unrouted`。
反向证伪：把赋值移回 `run_gate` 之后 ⇒ FAIL（`before=0, after=0`，精确 +1 断言）；移回前 ⇒ PASS。

### OC-3（代码，已修 + 反向证伪）
边缘转发写时带 `x-hydra-trace-id`（就是它回给租户的那个 id，`cluster/forward.rs:361`），而 leader 的 `AdminService::response` **自己新造一个**、全仓没有任何地方读这个头（grep 只有出站写法）。于是 leader 的审计记录 `hydra::tenant_config_write`（`tenant_config_api::audit`）与错误响应体标的是一个**租户永远看不到**的 id，"我的 trace id 是 X，请查"无法与 leader 的审计对上。
修：**采用**中继进来的 id（`relayed_trace_id`），审计记录 / leader 错误体 / 租户响应头从此是同一个字符串 —— 这就是整条修复，不需要把新参数穿透到几十个 handler 签名里。**必须做形状校验**：这一行在任何门禁之前执行，未认证的调用者本可把换行符、ANSI 转义或超长字符串塞进运维可见的日志与响应体；只接受长度 ≤ 64 的 `[A-Za-z0-9._:-]`，否则退回本地新造。
新增 `admin_api.rs::the_leader_adopts_the_relayed_trace_id_and_sanitises_it`（前半断言采用，后半断言超长值与含空格/分号的值**不被回显**、退化为 `hydra-` 本地 id）。
反向证伪：恢复 `let trace_id = crate::proxy::new_trace_id();` ⇒ FAIL（`left: "hydra-18d9c3c0a447d34f-11"` vs `right: "hydra-edge-17f4a2c9"`）；恢复修复 ⇒ PASS。

### OC-4（文档侧已修；行为侧进入决策清单）
契约说"两种形态你拿到的响应形状完全一致…你**无需也不应**感知它在哪个节点落地"、且把 503 标成可重试；代码是 `NoLeader` 的文档自陈"a cluster leader's own data plane has no forward target, so it returns this and does NOT apply locally"（`cluster/forward.rs:35-41`），`tenant_api/handlers.rs:1068` 返回确定性 **503 `no_leader`**。也就是：**LB 把租户流量打到 leader 的 `:8080` 时，读全好、写恒定 503 且重试同一节点永远无效**。`ops.md:730-738` 单独记了这个拓扑约束，但**租户契约里没有** —— 这是文档缺口。
修（本轮）：把该例外搬进 `tenant-api-integration.md` 的"传输语义"（含"必须经由边缘""同节点重试必然失败"），把 `503 not_ready` 与 `503 no_leader` 在两张错误码表里**拆成两行**并分别给可重试判定；同时补上 OC-3 的 trace id 采用说明。
行为侧（让租约持有者走本地写核心）会改动单一写者不变量的实现方式，**留给决策**（`forward.rs:37-39` 已把它列为 refinement）。

### OC-6（待决策，P3）
`admin/tenant_config_api.rs:172-174` 的文档写"Missing and foreign are deliberately indistinguishable (no oracle)"，而 `:252-256`（子租户 DELETE）与 `:294-316`（路由 DELETE）实际是 **missing → `RowNotFound` → 204**、**foreign（属于别的租户）→ `Ok(_)` → 404**。即一个持有合法令牌的租户**可以分辨"这个 id 存在但属于别人"与"这个 id 不存在"**，与自己的声明相反。可利用性低（id 是 `gen_id()` 的纳秒串，不可枚举），故维持 P3。
两条修法都只有几行，但改的是对外可见的 404 语义，**二选一需产品决策**：① 外来行也回 204 `Deleted(id)`，区分只留在审计日志（与文档声明一致）；② 承认可区分，改文档与契约为"foreign 回 404 且这是有意的"。**本轮未改**。

---

## 1l. 第十一轮：对前十轮修复本身做对抗性复审（六路 Oracle）+ 抓到的一个 P1

### A. 抓到的新问题：空闲上限的惩罚**机制上是空的**，但两处文档都说它在工作（P1）

**现象**：provider 只要"先回 `200` 头、再永远不吐 body"，就**永远不会被熔断**，无论截断多少次。它一直留在轮转里，每个请求都拿到 200 + 半截答案（而且按量计费）。

**证据链**：
- 空闲截断路径确实记了一次失败：`proxy.rs` 的 `stream_response` 内 `Err(_elapsed) => { record_upstream_stream_idle_timeout(...); self.state.breaker.on_failure(provider_id); ... }`，注释还专门论证"这是**无歧义**的 provider 侧证据"。
- 但**同一个请求**在拿到 2xx **响应头**时就已经记了一次成功：`if status.is_success() { ...; self.state.breaker.on_success(&cand.provider_id); ... }`（**在同一次 `stream_response` 调用之前**）。
- 熔断器是**连续失败**语义且**一次成功即清零**：`core/breaker.rs` 的 `on_failure` 是 `*count += 1; if *count >= threshold { dead.insert }`，`on_success` 的文档明写 "clears both the counter and the dead flag"。
- 于是每个"回 200 后卡死"的请求都是：清零 → 记 1 次失败。**计数永远停在 1**，阈值为 5 的熔断器**永远不可能被这条路径触发**。而 `ops.md` 的告警表与 §9 的说明都写着这条路径 "also feeds the circuit breaker — it is unambiguous provider-side evidence"。
- 与 `proxy.rs` 前文已经修正过的 5xx 场景是同一类错误（"任何已认证租户都能用 5 个被上游拒绝的请求把该 provider 对其他所有租户打成 503"），但这一条的触发者是**合法的慢上游**，攻击者什么都不用做。

**修**：把"成功"的判定从**收到响应头**改为**响应体完整送达**——删掉头部处的 `on_success`，改到 `stream_response` 返回 `Ok(())` 之后；`Err` 分支**不**记成功（上游沉默已在 `stream_response` 内记失败；客户端自己断连那一类**故意不计**，免得客户端的断连把健康 provider 标死 —— 该注释原样保留）。
副作用已逐个过一遍：客户端中途断连的请求现在既不成功也不失败（此前会记成功），即"既不重置也不惩罚"，对连续失败语义是更保守的一侧；TTFB 指标 `record_upstream_duration` 仍在原处（它测的是首字节，与健康判定无关）；`on_success` 仅此一处被移动，`5xx → on_failure`、连接/首字节失败等路径未动。

**验证**（新增 `terminate_mode.rs::a_provider_that_stalls_after_its_headers_trips_the_breaker`，用既有的 `spawn_stalling_sse_upstream(1, 0, true)` 原始 socket 上游 + `state_for_one_upstream`（1 s 空闲窗口）+ 阈值 5 的熔断器）：
- 五个请求都必须先拿到 **200**（头已发出，所以这是截断而不是可重试失败），前四个之后 `!is_dead`，第五个之后 `fail_count == 5` 且 `is_dead`；
- 第六个请求必须 **503**（唯一 provider 已出局，请求根本不该再打到上游）；
- 测试环境不启动探活任务（`spawn_probe_task` 只在 `main.rs:691` 被调用），所以出局状态不会中途被复活，断言不 flaky。
**反向证伪**：把 `on_success` 放回头部原处 ⇒ 测试 FAIL，`fail_count` 期望 5 实得 **1**（正是"计数被钉在 1"的直接证据）；放回修复版 ⇒ PASS。

### B. 本轮同时把两条"断言"升级为"实测"
1. `-D warnings` 确实是"`#[test]` 与函数被手术断开"这类事故的守卫：人工复现后 `cargo clippy -p hydra-core --all-targets -- -D warnings` **exit 101**（`error: function \`limit_match_all_null_matches_everything\` is never used`），还原后 exit 0。
2. 新 CI 作业覆盖的是**真正的代码**而不是"特性名能解析"：`tls.rs` 的整个 `HydraCertStore`/SNI 管线 gated 在 `any(tls-boringssl, tls-openssl)`；并把 `openssl-sys` 需要的 `libssl-dev`/`pkg-config` 从"赌镜像内容"改成**显式安装**。

### C. 自查抓到的第三处自伤（同属"门禁说谎"）
`.acceptance/round10-gate.sh` 初版**恒 exit 0**（末句是 `echo`），且给可选套件漏了 `--no-fail-fast`（首个失败二进制之后的测试全部被静默跳过，汇总只报 299 / 实际 644）。已修并反向证伪：换成 `true`/`false` 桩命令后，带 `exit` 行 ⇒ 1，删掉该行 ⇒ 0。现在真实运行该脚本返回 **exit 1**（因为 D-2 两例仍红），并且**脚本内部**已经报出完整的 `644 passed / 2 failed / 3 ignored`。

### D. 六路对抗性复审（对前十轮的修复本身）
前十轮的所有结论都是**同一个执行者**写的，没有任何独立验证。本轮按"找问题"的原始要求，派 6 个 Oracle 分别对抗性复审：① `hydra-core` 纯逻辑（§1j.A/§1b/§1i）；② 持久化与集群一致性（`db.rs`/`cluster/*`/`redis/rate_limit.rs`/`sink.rs`/迁移）；③ 鉴权与边界（`admin/*`/`tenant_api/*`/`crypto.rs`/`tls.rs`）；④ **测试诚实性**（每个新测试是否真的在断它声称的东西、有没有"改了修复也照样过"的空测试、全局状态与 flaky 模式、属性被手术断开这一类）；⑤ 计划/文档自身的事实核对（每个数字、每个 `path:line`、每个函数名）；⑥ 工程卫生/CI（新作业在 ubuntu-latest 上是否真能过、`.dockerignore`/`.gitignore` 是否漏项、门禁脚本自身是否有假绿）。
它们的结论（含"已核对为真"与"无法离线验证"两类）合并如下（§1m）。

---

## 1m. 第十一轮下半：六路复审的结论与处置

六路复审**独立确认**了前十轮的大部分结论（其中"测试有效性"那一路逐条推演了 29 个测试的"改回旧实现是否必然变红"，结论是**确实承重**；"文档核对"那一路用程序计数确认了 core 246 / server 520 / 可选 644、`#[ignore]` 恰 3、`findings-disposition` 恰 27 项、`AppState` 字面量恰 3）。但它也抓出了**我自己引入的问题**，其中两条 P1 与两条 P2 是本轮修掉的。

### A. 本轮修掉的问题（每条含反向证伪）

**A1【P1 · 我自己的回归】`environment/clickhouse/init.sql` 里的去重设置只是注释，官方栈的新实例静默失去去重 ⇒ 重复计费。**
两路复审各自独立发现同一条。`dev-docs/ops.md:590` 写"**Fresh** instances get it from `environment/clickhouse/init.sql`"，而该文件 39 行里 `non_replicated_deduplication_window` **只出现在 `--` 注释中**，没有可执行 `SETTINGS`；带 `SETTINGS` 的只有 `docker-compose.local.yml` 的内联 DDL。两个官方 compose（`docker-compose.yml:108`、`docker-compose.cluster.yml:129`）恰恰挂载 `init.sql` 进 `/docker-entrypoint-initdb.d/`，所以**全新生产实例**上 `insert_deduplication_token` 会被接受并**静默忽略** —— 也就是 §1e 声称修掉的那个 P1，只在手工 ALTER 过的实例（含本机活实例）上才是修好的。这正是"修了本地、漏了官方路径"的典型。
修：`init.sql` 补上真正执行的 `SETTINGS non_replicated_deduplication_window = 1000;`，并把注释改成说明它**曾经**只作为注释存在。
守卫（新增 `crates/hydra-server/tests/clickhouse_ddl_parity.rs`，无需 ClickHouse）：程序化解析两份 DDL，断言 ①列集合一致 ②**两份都执行**该 SETTINGS（先剥掉 `--` 注释再判定，**注释不算设置**）③`ORDER BY` 一致。反向证伪：把 `init.sql` 的 SETTINGS 删掉（即它 2026-09-29 之前的真实状态）⇒ FAIL。
（附带教训：第一版解析器用"第一个 `(` 到第一个 `)`"取列，被 `Nullable(String)` 的嵌套括号打穿，且 compose 里有一句注释也含 `CREATE TABLE`；已改为按行解析 + 先剥注释 + 用带表名的锚点。）

**A2【P2 · 我自己的回归】SSE 路径的尾部**完全无界**：`MAX_JSON_TAIL` 只加在非 SSE 分支上。**
§1j.A.1 声称"两条路径共用尾部候选，并加界（64 KiB）"，实际界限只在 `sse.rs` 非 SSE 分支里检查；SSE 分支的 `self.tail.extend_from_slice(trailing)` 没有任何长度判断，而 `trailing` 在缓冲里没有 `\n` 时就是**整个缓冲**。于是"单条未终止的 `data:` 行有多长，尾部就留多长"，并且每次 read 都重扫累积缓冲（brace-match 线性）—— 正是加界要消除的二次开销，却被写成了已加界。
修：把上限判定提到两条路径共用处；超界**丢弃**而不是继续攒。
守卫：`tests/sse.rs::an_oversized_unterminated_sse_tail_is_dropped_rather_than_retained`。反向证伪：删掉该判定 ⇒ FAIL（`left: Some(tokens_in=1, tokens_out=5)` vs `right: None`，即 200 KiB 尾部真的被留下来了）。
**并且这条测试的第一版是空转的**：填充用的 `a` 直接接在数字后面（`1aaaa…`）使 JSON 非法，所以无论有没有上限都解析不出 usage、都返回 `None` —— 第一次反向证伪**没能让它变红**，我据此把它改成在 JSON 字符串内填充才成为真正的判别器。**这正是"测试有效性"那一路警告的形态，我在同一轮里自己踩了一次。**

**A3【P2 · 我自己的回归】OpenAI/Generic 分支会**擦除**已吸到的值（少计 ⇒ fail-open）。**
§1i 的修复只做了一半：Anthropic 分支是"只赋值 `Some`"（带注释说明"缺失字段不能擦掉先前事件带来的值"），而**同一文件**的 OpenAI/Generic 分支是无条件赋值。流里出现第二个 usage 对象且缺字段时（`{"usage":{"completion_tokens":5}}`、`{"usage":{}}`、或网关的 `{"usage":{"total_tokens":…}}`），`tokens_in` 被置回 `None` ⇒ 总 token 变小 ⇒ **配额与计费少算**，且 200 记录里只是 NULL，没有任何指标。而 `Generic` 恰是默认 schema。
修：OpenAI 分支镜像 Anthropic 的写法（三个字段都只在 `Some` 时赋值）。守卫：`tests/sse.rs::a_later_partial_usage_object_does_not_erase_earlier_values`；反向证伪：恢复无条件赋值 ⇒ FAIL（`left: tokens_in=None` vs `right: Some(11)`）。
顺带修同族的一个方向性缺口（P3）：`OpenAiUsageFields` 缺 `cache_read_input_tokens`，于是"Anthropic 风格上游 + OpenAI 风格路由"时缓存命中数静默 NULL（不进配额，但计量少记）。已补字段 + `tests/sse.rs::the_openai_schema_also_reads_the_anthropic_cache_field`。

**A4【P3 · fail-open】`proxy.rs` 的 token 求和未检查溢出，release 下回绕绕过配额。**
`let total = u.tokens_in.unwrap_or(0) + u.tokens_out.unwrap_or(0)`：两个操作数都是**上游 JSON 里的 u64**，而工作区**没有任何 `[profile]` 覆盖**（release 即 `overflow-checks = false`）。上游报 `tokens_in: u64::MAX` 时总和回绕成极小值 ⇒ 该请求几乎不记 token 额度。改为 `saturating_add`（方向：把预算足额计入，安全侧）。
（同一路复审还指出上游数值本身没有合理性上限；记入批次 4。）

**A5【P2 · 同一类洞的第二处】控制面转发客户端不禁重定向。**（P1-1 只修了 provider 上游那一侧。）
`cluster/forward.rs` 的 `client_for` 既没有 `redirect`，兜底还是 `unwrap_or_else(|_| reqwest::Client::new())`（默认跟随 10 跳）；而这些转发带着凭据：admin 转发中继运维的 `Authorization`（admin token），租户配置写转发同时带 **cluster token** 与 `x-hydra-tenant-token`，而 leader URL 来自注册表里**未校验的字符串**。reqwest 跨主机跳会保留自定义头 ⇒ 一个 302 就能把两把凭据送到 `Location` 指定的主机。已加 `Policy::none()`（builder 与兜底同策略），并把"必须能构建"从静默降级改成显式 `expect`（常量设置若无解，说明 TLS 后端不可用，不该悄悄退回默认策略）。
同类还有一处：`proxy/provider_client.rs` 的**最后一个**兜底 `unwrap_or_else(|_| reqwest::Client::new())` 把 P1-1 刚关掉的重定向重新打开，而紧邻注释还称"first fallback 重新应用了策略"（只覆盖了第一次）。已一并改为恒定策略 + 显式 `expect`。
**测试待补**（批次 4）：两处都需要"两个 listener + 302 收集器"形状的测试（`provider_client` 已有同形测试可抄）。

**A6【P3 · 假注释】SWRR 的"被 clamp 进 i32"是旧实现的残留说法。**
`swrr.rs:89-92` 写"so the subtraction is clamped into i32"，实际已是 i64 + `saturating_sub`；而 `tests/swrr.rs` 那句"state stays inside i32"的断言只是**该 fixture 的性质**（复审独立复算：3 个 `i32::MAX` 权重时 `|cw| = 4294967294 > i32::MAX`，真正的界是 `|cw| <= Σweight`）。已改注释。**测试断言的改法记入批次 4**（要改成 `|cw| <= Σweight`，而不是删掉——它是防"被人塞回 i32"的哨兵）。

**A7【P2 · 我的注释是错的】新 CI 作业的"用系统 OpenSSL"前提不成立：`pingora-openssl` **硬开** `vendored`。**
第六路复审指出我写的注释（"nothing in this workspace enables its `vendored` feature — `cargo tree -e features -i openssl-sys` shows only `openssl-sys feature \"default\"`"）把一个**反向树**的读数当成了结论。复核（本轮我亲自跑过）：`pingora-openssl-0.8.1/Cargo.toml:52` 是 `features = ["vendored"]`，`Cargo.lock` 里有 `openssl-src 300.6.0+3.6.2`，`cargo tree -e features --features hydra-server/tls-openssl` 里 `openssl-sys` 下挂 `openssl-src`。也就是说该作业会**从源码编译一遍 OpenSSL**（需要 perl/make/cc，首跑几分钟；`Swatinem/rust-cache` 会缓存），而 `libssl-dev`/`pkg-config` 根本用不到。
修：删掉那个多余的 `apt-get install` 步骤（它是与结论无关的网络失败点），注释改为事实。**"这个作业便宜因为它不编 vendored BoringSSL"是错的**——便宜的是不编 BoringSSL，但它要编 vendored OpenSSL。

**A8【回归 · 我自己引入并当场被门禁抓住】熔断改动弄红了 `breaker_success_clears_failures`。**
该测试在**收到 200 响应头之后立刻**断言 `fail_count == 0`；而我把成功判定移到"响应体送达之后"，于是断言与实现不同步（`terminate_mode.rs:669` FAILED）。
这不是测试太弱，而是**语义变了**：健康证据从"上游回了 2xx 头"改成"一个响应完成了"。修：测试先消费响应体，并在 1 s 预算内轮询 `fail_count == 0`（代理侧的 `on_success` 就在最后一次写之后，需要容忍调度）。注释已写明这次语义变更及其原因，指向新增的 `a_provider_that_stalls_after_its_headers_trips_the_breaker`。
另外复核了 `on_success` 的其它调用点，确认**不影响复活语义**：探针任务自己调 `on_success`（`breaker_wrap.rs:256`），与请求路径无关。

**A9【回归 · 低】新增的 DDL 守卫文件有一个未使用的 import，`clippy -D warnings` 直接把它拦下。**（`clickhouse_ddl_parity.rs:24` 的 `Path`；改为只留 `PathBuf`。）这是本轮 `check` 作业唯一一次真正的编译失败，说明门禁在起作用。

### B. 复审确认**为真**的（无需动作，但值得留档）
- §1b `mask_key` 边界（20）与长度扫描守卫成立；L∈[14,20) 由"暴露 L−4 个字符"变为中档，**严格变好**且无回归（旧测试曾把 L==14 的完全泄漏钉成预期）。
- §1j.A.2 SWRR 常规权重逐位不变；饱和在 `|cw| <= Σweight` 下不可达，不会卡住权重；原缺陷（3×`i32::MAX`）真实存在。
- §1j.A.4 合并保和、保序、方向为**超计**（fail-closed）；count clamp 对 `limit <= cap` 完全等价。
- §1i Anthropic 方向的双族回退与"只赋值 Some"正确。
- OC-5 的标签时序、`ctx.tenant` 留在门禁后（攻击者无法选指标标签）、`/metrics` 全角色 token 化、成功请求不被失败预算拦住、`version_from_env` fail-closed、环外版本 fail-closed 且无解密回退、reseal 单事务读写且失败行不改写、`del_all_tenants` 不再早退、`slowest_live_watermark` 三处"证明不了"都退回保守行为、限流 member 的 per-instance nonce 防塌陷。
- `findings-disposition.py` 27/27、`#[ignore]` 恰 3 条、core/server/可选三套件的计数、`git merge-base --is-ancestor c3eaa6f 692655d` = YES、`AppState` 字面量 = 3 —— 全部经程序计数独立复核。

### C. 本轮新加的**类级**守卫（因为同一类事故已经发生三次）
`crates/hydra-server/tests/test_attribute_integrity.rs::no_function_claims_two_test_attributes`：扫描 `crates/**/*.rs`，任何"一个函数块里出现两个 test 属性"即失败并点名两个行号（它同时意味着**另一个函数已经没有属性、不再运行**）。
反向证伪：再次人工复现该手术（在 `limit.rs` 插入 `#[test]
/// probe
#[test]`）⇒ FAIL 并报 `limit.rs:46 and :48 are two test attributes for one function …`；还原 ⇒ PASS。
**这一轮我又犯了一次同样的错**（把新测试插在既有 `#[test]` 与函数之间），是 `clippy -D warnings` 当场以 **exit 101** 拦住的 —— 与 §1j.F.1 的结论一致，而这次终于把它变成了跑在普通测试套件里的守卫，不再依赖"记得跑 clippy"。

### E. 本轮验证（`.acceptance/round10-gate.sh`，每条命令各自取退出码；脚本现在**返回**判定，见 §1j.F.3）

| 门禁 | 退出码 | 计数 |
|---|---|---|
| `fmt --check` | 0 | |
| clippy（CI `check` 作业逐字） | **0** | 本轮修掉 A9 与两处回归后转绿 |
| clippy（`optional-features` 作业） | 0 | |
| clippy（`tls-openssl`，新作业逐字） | 0 | |
| `test hydra-core` | 0 | **249 passed / 0 failed**（第十轮 246 + 本轮 3 个 sse 测试） |
| `test hydra-server --features server` | 0 | **525 passed / 0 failed / 1 ignored**（520 + terminate_mode 1 + attribute 守卫 1 + DDL 守卫 3） |
| 可选 + 活 Redis/CH（`--no-fail-fast`） | 101 | **649 passed / 2 failed / 3 ignored**，2 例即 D-2（`admin_api.rs:1724`、`:3063`），与前几轮完全一致 |
| findings-disposition | 0 | 27/27 |
| i18n | 0 | |

一处必须写明的过程事实：**我对熔断的改动一次把 `breaker_success_clears_failures` 弄红了**（A8），并有一次 lint（A9）——两者都是**全量门禁**抓到的，不是靠推理发现的。这也是本轮把门禁从"我手工挑命令跑"换成"一个脚本逐条记录退出码"的直接收益。

### D. 记入**批次 4**（复审发现、本轮未修）
1. 【P1】`/api/v1/internal/*` 的 **cluster-token 门禁**零限速、零计数、零日志（`admin/mod.rs:776-798`）—— P1-14 只修了同一文件里的 admin token 门禁；且 `MIN_ADMIN_TOKEN_LEN` 只作用于 admin token，cluster token 只校验"存在"（`main.rs:230-236`）。
2. 【P1】`cluster/content.rs::a_read_transaction_is_not_torn_by_a_concurrent_writer` **从未调用被测入口** `ReplicationContent::load`（自己用 `pool.begin()` 手搓事务）⇒ 被引用为"DEFERRED 边界守卫"不成立：把 `load` 改成 `begin_write` 它照样全绿。
3. 【P2】`tests/sqlite_write_lock.rs::deferred_begin_is_the_hazard_begin_write_removes` 两支都放行（`Ok(_) => {}`）⇒ **永不失败**，而 doc 声称它是防回退的守卫。
4. 【P2】ClickHouse 批级幂等在 CI 里**零覆盖**：唯一端到端测试 `clickhouse_sink_writes_batch` 被 `#[ignore]` 且 CI 从不加 `--ignored`；`clickhouse_insert_statement`（含"`SETTINGS` 必须在 `FORMAT` 之前"）全仓无测试引用。建议加**纯字符串单测**。
5. 【P2】`HYDRA_ENCRYPTION_KEY_VERSION` 的解析（`version_from_env`）在 `crates/` 内**零测试引用**，而 §1h 把它当作防呆卖点。
6. 【P2】`cluster/replica.rs`：`take_retry` 在取走重试时就增加 `fail_streak`，而 `note_attempt_succeeded` 只在成功时清零 ⇒ "重试路径空转"会让退避单调升到 60s 上限（计划只写了"次数不限、速率受限"的一半）。
7. 【P2】`sink.rs`：同一 `insert_deduplication_token` 在重试时**批次内容会变**（`buffer` 只增不减、失败批次 `extend` 回来后继续 `push`），而 ClickHouse 去重键含块内容 ⇒ 内容变了就不再命中去重，**重复行仍在**（与 §1e 的修复方向相反）。
8. 【P2】`sink.rs`：优雅关闭时最后一次 flush 用 30s 重试窗口，而 `shutdown()` 只等 5s ⇒ 后端不可用时缓冲里的用量**既不落库也不报 `shutdown_unflushed`**（那段代码永远来不及执行）。
9. 【P2】`admin/tenant_config_api.rs` 有一份 `classify_db_err` 的**复制品**且缺 BUSY 分支 ⇒ leader 内部写端点/经它转发的数据面写在 `BEGIN IMMEDIATE` 超时后回 **500 `database_error`**，而契约承诺可重试的 **503 `storage_busy`**（唯一所有者原则被破坏）。
10. 【P2】管理面失败预算键用 IPv6 **源地址原文**（未归一到 /64）且是**进程内** `DashMap` ⇒ 预算可乘（/64 内换地址、或 N 个节点各一份）；注释"can only throttle more, never less"只对**头部**伪造成立。
11. 【P2】主密钥轮换**不强制**版本号变革：`reseal_secrets` 用"版本号相等"当作"已用当前钥匙加密"（`db.rs:1972-1975`）⇒ 换钥不改版本号时一行都不重封、报告 `already_current`、**exit 0**，删旧钥后拒绝启动（需要一次决策：`already_current` 是"版本号相同"还是"用当前钥能开"）。
12. 【P2】租户可跨租户**探测** operator `key_prefix_binding` 前缀：租户写路径经 `validate_sub_tenant` 的 binding 重叠检查拿到 `400 key_prefix_overlap` ⇒ 干净的布隆判定（§1k OC-6 只处理了 DELETE 的 204/404）。需决策：折叠成通用 400 还是保留自助排错信息。
13. 【P3】免鉴权 `/healthz` 返回 `ActiveListeners`（实际监听地址、是否配 TLS、证书数）+ 租户/provider 计数；`/healthz/leader` 是匿名**节点角色** oracle。需决策探针可以泄漏多少。
14. 【P3】`SlidingWindow` 的 `token_sum` 在 `saturating_add` 饱和后**永不衰减**（差额留在 `token_sum` 里，`evict_stale` 只清样本）⇒ 该 window 存活期内永久超计（fail-closed，触发需 > u64::MAX 的单条/两条样本）；上限只约束**单个** window，window 数量本身无界（bucket 含客户端 key 的脱敏值）。
15. 【P3】`mask_key` 的输出同时是**限流匹配值与桶身份**（`proxy.rs:830`/`:1402-1408` + `limit.rs:46-48`），而 `design.md:989` 写的是明文匹配 ⇒ 改边界会静默改变限流行为；且短 key 脱敏碰撞可构造（1–5 字符全是 `*`）。需决策：匹配改用原始 key 的哈希 / 明文 / 保持脱敏。
16. 【P3】`extract_usage_object` 只看**第一次** `"usage"` 出现，遇到 `"usage":null` 或字符串值即永久阻断该响应的用量提取；超界丢弃是**静默**的（无日志无指标）。
17. 【P1 · 需要决策】**整套门禁脚本不在版本控制里**：`.gitignore` 的 `/.acceptance/`（既存规则，非本轮引入）使 `git ls-files .acceptance` = **0**，而本轮计划却推荐"直接用 `.acceptance/round10-gate.sh`"，并把 `findings-disposition.py` 的 27/27、`t3-baseline.sh` 的 `--ignored` 验收当作证据；CI 里也没有任何步骤调用它们（`grep -nE "phase-b-gate|t3-baseline|round10-gate|tenant-api-gate" .github/` 为空）。也就是说**证据链只存在于本机一个被忽略的目录**，新克隆没有门禁、CI 无法引用。两条路都要动仓库策略，故列为 **D-5**：①门禁移入受控目录（如 `scripts/gates/`）；②保留路径，把 `/.acceptance/` 改成 `/.acceptance/*` + `!/.acceptance/*.sh` + `!/.acceptance/*.py`（git 需先放开父目录再取反；`.log`/`pw-browsers/` 仍忽略）。
18. 【P2】`scripts/load_test.sh` 的分布判定**读错文件**：`:86` 写入的是**实例名**原始列表，而判定 awk（`:95-96`、`:102` 的 `c[NR]/s`、`:108`）读的也是这个文件，`uniq -c` 只用于打印 ⇒ `min` 变成"最后一个实例名 ÷ 实例名之和"。实测：生产形状 `750×9001 + 250×9002` → **FAIL**（正确分布被误判），`uniq -c` 形状 → PASS，非数字实例名 → awk 除零致命错误。于是该 opt-in 门禁实际不可用，而计划 §1j 声称的"三组输入自检"用的是**另一种输入形状** ⇒ 那个自检不是证据（与计划自己批判的"假 `now` 测真实时钟"同形）。
19. 【P2】CI 同形状空洞仍在（本轮只补了 `tls-openssl` 一个）：(a) `#[ignore]` 用例 CI 从不运行（唯一载体 `t3-baseline.sh` 见第 17 条）；(b) 仍无 `sqlx prepare --check`；(c) 无作业 `docker build` / 跑 compose / 跑 `integration/`，于是本轮 `environment/*`、`.dockerignore`、compose 端口的改动没有 CI 保护；(d) 三个 SDK 与 `tools/hydra-cli` 的测试没有作业运行；(e) `--features proxy`（无 TLS 后端）无人编译，而 manifest 声称它必须能独立构建。
20. 【P2】新作业的覆盖声明有三处够不着：`[[bin]] hydra` 带 `required-features = ["server"]`，而该作业不含 `server` ⇒ `main.rs` **不参与编译**（lib 内的 `tls.rs`/`listeners.rs`/`proxy.rs` 确实编译，主结论成立）。附带的推论更值得记：`main.rs` 里 `#[cfg(not(any(feature = "tls-boringssl", feature = "tls-openssl")))]` 的分支**任何配置下都不可编译**＝永久死代码。另：该作业只有 clippy（不链接），别的作业都有 `cargo build`。
21. 【P3】`.dockerignore` 漏掉 `.gitignore` 里明列 "never commit" 的 AI-agent 目录（`.claude/ .opencode/ .agents/ .cursor/ .windsurf/ .continue/ .codeium/`）；`test-results/`、`playwright-report/` 未进任何 ignore；`!bin/`、`!bin/hydra` 实测是**空操作**（删掉后 `bin/hydra` 仍 included）⇒ 计划把"没被排除"说成"取反生效"。

22. 【P3】文档/坐标类（复审逐条给出正确值）：§4 的可选套件命令**缺 `--no-fail-fast`**；§1j.G 残留数字 `641`（应 644）；§1k OC-4 引用的是 `tenant_config/forward.rs:36` 而非 `cluster/forward.rs:35-41`；错误码是 **27 行/33 code**（文档写 26/32）；`admin_api.rs:1626/:2965` 应为 `:1653/:2987`、`tenant_api_cluster.rs:294-320` 应为 `:186`；`ops.md:70` 应为 `:69`；`.gitignore` 是**根锚定** `/*.db`（子目录 db 不忽略）且"323 文件/4 命中"是 `git rm --cached` **之前**的观测；README 的"core 114 + server 173"早已过期（现 246/520+）。

---

## 1n. 第十二轮：批次 4 的第一批（鉴权面 + 单所有者 + 覆盖缺口）

### A. 已修（每条含反向证伪）

**A1【P1】`/api/v1/internal/*` 的 cluster-token 门禁与 admin 门禁合并为同一条拒绝路径。**
`admin/mod.rs` 原本有两把锁、只给其中一把上锁具：admin token 门禁有"按 peer 的失败预算 + 计数器 + `warn!`"，而**守控制面与跨租户写端点的 cluster token 门禁**只做一次比较就回 401 —— 零限速、零计数、零日志。猜这把令牌既免费又无痕（比第 9 轮修掉的那个洞更险：那个至少还有一行被日志级别过滤掉的 `debug!`）。
修：抽出唯一实现 `refuse_bad_credential(session, path, trace_id, gate, message)`，两把锁共用（同一份预算、同一窗口——在一个端口上浪费掉的尝试就是浪费掉了，只会更早限速、不会更晚）；指标标签带上 gate（`admin_denied|admin_throttled|cluster_denied|cluster_throttled`），于是任一把锁都能单独告警；顺手修掉那句被盲目编辑出的一长串空格的 `warn!` 文案。
同时补上**强度下限**：`HYDRA_CLUSTER_TOKEN` 此前只校验"存在"（1 个字符也能启动），而 admin token 有 `MIN_ADMIN_TOKEN_LEN = 16`。新增 `AdminService::MIN_CLUSTER_TOKEN_LEN = 16` 并在 `main.rs` 拒绝启动；顺带修掉 leader 报错文案里同样多出来的一串空格。

**A2【P2】管理面失败预算的键：IPv6 归一到 /64，且 IPv4-mapped 必须**先解包**。**
原实现用 `s.ip().to_string()` 作桶键。一个被委派 /64 的客户端持有 2^64 个地址，可以随意换桶 ⇒ **预算可乘 2^64**，对"它本来要防的那个攻击者"形同虚设。修：抽出纯函数 `throttle_bucket(IpAddr)`，IPv4 原样、IPv6 折到 /64；**并先把 `::ffff:a.b.c.d` 解包成 IPv4** —— 否则双栈监听器上报的所有 IPv4 客户端会一起折进 `::/64` 这一个桶，一个人十次错 token 就把所有 IPv4 调用者限速了（保守方向，但那是运维自锁）。
守卫：`admin/mod.rs` 内联单测三条（IPv4 独立桶；同 /64 同桶、不同 /64 不同桶；mapped 解包且两个 mapped 地址不同桶）。反向证伪：把折叠改回 `v6.to_string()` ⇒ 两条 FAIL（`left: "2001:db8:1:2:aaaa:…"` vs `right: "2001:db8:1:2::"`）。
另外把"预算仅在失败路径上消费、成功请求永不被拦"和"这是**进程内**计数、N 个可达管理端口 = N 倍预算"这两点写进注释与 `ops.md` 的告警行（原文的"can only throttle more, never less"只对**头部**伪造成立，对源地址不成立）。

**A3【P2】`classify_db_err` 的复制品与单一所有者原则。**
`admin/tenant_config_api.rs` 手抄了一份 `handlers::classify_db_err`，而且**已经漂移**：副本缺 `SQLITE_BUSY (5) / 517` → **503 `storage_busy`** 那一支。于是 `BEGIN IMMEDIATE` 超时经 leader 内部写端点（以及经它转发的数据面写）回的是 **500 `database_error`（"内部 bug"）**，而管理面同一操作承诺的是**可重试的 503**。修：删掉副本，改为调用唯一所有者（`classify_db_err` 提升为 `pub(super)`），并在两处都写明"这个映射只能有一个所有者"。

**A4【P2】ClickHouse 插入语句第一次有了测试（此前全仓零引用）。**
`clickhouse_insert_statement` 是重试幂等修复里**属于我们自己的那一半**：token 必须进到语句里，且 **`SETTINGS` 必须在 `FORMAT` 之前**，否则 ClickHouse 直接拒绝该查询。而 CI 对它的覆盖是 **0**（唯一端到端测试被 `#[ignore]`，没有任何作业加 `--ignored`）。新增纯单测两条：①语句带 token 且 `SETTINGS` 位置 < `FORMAT` 位置、前缀仍是该表的 INSERT；②敌意 batch id 不能改写语句（引号必须被过滤 ⇒ 全文只有一对引号、无换行、token 只剩 `[A-Za-z0-9_-]`）且 token 截断到 128 字符。
反向证伪：把 `FORMAT JSONEachRow` 挪到 `SETTINGS` **之前** ⇒ FAIL，并打印出实测位置（`SETTINGS at 240 / FORMAT at 221`）。
（过程记录：这条测试我第一版写错了断言 —— 我断言"输入里的 `injected`、`--` 不得出现"，但**字母和 `-` 本来就是允许的**、它们在引号内无害，真正要断言的是**结构**（只有一对引号、无换行、token 字母表）。已改成结构断言。同一轮里第二次出现"我自己写的测试不测它声称的东西"。）

**A5【P1 级缺陷的守卫】cluster 门禁的集成测试。**
`tests/admin_api.rs::bad_cluster_tokens_are_throttled_and_counted`：用 `admin_state_with(3, Some("cluster-token-16chars"))`（把测试用的 state 构造器扩了一个 cluster token 参数）打 4 次错令牌 —— 前 3 次 401、第 4 次必须 **429 + `Retry-After`**；并断言两个新标签的计数**都增加**（用"增加"而非精确增量：同一二进制里其它用例也会拿 admin token 打 internal 前缀，而指标是进程全局的）；最后断言**正确**的 cluster token 既不会被 401 也不会被 429（预算只在失败路径上消费，运维不会自锁）。
反向证伪：把 cluster 分支换回裸 `err_json(401, …)` ⇒ FAIL（`left: 401` vs `right: 429`）。

### B2. 本轮验证

| 门禁 | 退出码 | 计数 |
|---|---|---|
| fmt / clippy×3（server、optional、tls-openssl） | 全部 **0** | |
| `test hydra-core` | 0 | **249 / 0** |
| `test hydra-server --features server` | 0 | **529 / 0 / 1 ignored**（525 + bucket 单测 3 + cluster 门禁集成测试 1） |
| 可选 + 活 Redis/CH（`--no-fail-fast`） | 101 | **655 / 2 / 3**（649 + 上述 4 + 仅 `usage-clickhouse` 下编译的语句单测 2）；2 例仍为 D-2（`admin_api.rs:1724`、`:3063`），与前几轮完全一致 |
| findings-disposition / i18n | 0 | 27/27 |

### B. 本轮仍未做（继续留在批次 4）
`cluster/content.rs` 那条从未调用被测入口的测试、`sqlite_write_lock.rs` 那条两支都放行的测试、`sink.rs` 的"重试时批次内容会变导致去重不命中"与"关闭时 5s < 30s 重试窗口 ⇒ 静默丢量"、`version_from_env` 的零覆盖、以及 D-5。

---

## 1o. 第十三轮：批次 4 第二批（用量丢弃路径 + 三条"不承重"的测试）

### A. `sink.rs`：关闭时静默丢量，以及"重试窗口"其实不封顶

**A1【P2】最后一次 flush 用的 30 s 窗口 25 s 长于关闭预算 5 s ⇒ 丢量既不计数也不打日志。**
`shutdown()` 只等 `MAX_SHUTDOWN_WAIT = 5s` 就返回，而 `main.rs` 的 SIGTERM/SIGQUIT 路径紧接着 `process::exit` —— 任务在哪停就死在哪。最终 drain 用的却是常规 `MAX_FLUSH_RETRY_WINDOW = 30s`，于是**后端在关闭时不可用**的情况下：5 s 到点、`shutdown()` 返回、进程退出，`None` 分支里那两句 `note_usage_drop("shutdown_unflushed", …)` 与 `error!(lost = …)` **永远不会执行**。也就是说这批用量记录丢了，而**计数器与日志都没有痕迹** —— 这与计划全程"任何丢弃都有指标"的承诺直接冲突。
修：最终 drain 的窗口按关闭预算封顶（留 1 s 给上报与退出本身）。

**A2【P2 · A1 的根因】`flush_with_backoff` 的"重试窗口"只约束**尝试**，不约束**时长**。**
这是写 A1 的测试时被它**逼出来**的：即使把窗口封到 4 s，任务仍在 6.35 s 才返回 —— 因为循环是在每次失败之后才检查 `elapsed >= retry_window`，然后才 `sleep(delay)`，而 `delay` 最大 10 s（`CAP`）。所以"4 s 的窗口"可以让函数忙 14 s。文档却写着这个常量是"How long ONE `flush_with_backoff` invocation may keep retrying"。
修：`sleep(delay.min(retry_window - elapsed))` —— 现在窗口真的封顶，最终 drain 也才可能在关闭预算内跑完并上报。
守卫：`sink.rs::audit_3_9_tests::the_final_drain_finishes_inside_the_shutdown_budget`（用恒失败的 inserter + `batch_size=16` 让记录只可能被最终 drain 处理，再 `drop(tx)` 关通道；断言 `timeout(MAX_SHUTDOWN_WAIT, join)` **成功返回**）。
反向证伪：去掉 sleep 的钳制 ⇒ FAIL（5.00s 时任务仍在重试）；去掉关闭预算封顶同样 FAIL。修复后实测 **4.00s** 完成（正是被封过的窗口）。

**A3【P3 · 复审的一条断言是错的，已纠正来源而不是"修"它】**：持久化那一路复审认为"重试期间 `buffer` 只增不减 ⇒ 同 token 下批次内容会变 ⇒ ClickHouse 去重不命中 ⇒ 重复行"。**这条不成立**：`flush_with_backoff` 是在 `select!` 的分支里被 `await` 的，重试期间**根本没人往 `buffer` 里 push**；而各 inserter 的 `Err((batch, msg))` 返回的就是它收到的那个 `Vec`（`sink.rs:309/509/998/1066`），内容与顺序都不变。所以"同 token + 同内容"这个去重前提**是成立的**。
真正该改的是**注释**：它把"a batch whose composition changed between attempts"列成现网残余风险，把一个不可能的路径说成活的，反而盖住了真实的残余（批次超出重试窗口后会**换新 id** 重发，跨这条边界的"ack 丢失"仍可重复 —— 需要行级幂等 = schema 迁移）。已改写，并补一条测试把"每次重试都重发同一批、同一顺序"这个**承重不变量**钉住（`every_retry_sends_the_identical_batch_in_the_same_order`）。

### B. 两条"不承重"的测试（复审的 P1/P2，已修）

**B1【P1】`cluster/content.rs` 里被引为"DEFERRED 边界守卫"的那条测试，从未调用被测入口。**
`a_read_transaction_is_not_torn_by_a_concurrent_writer` 自己用 `pool.begin()` 手搓了一个事务，整个函数体里**没有 `ReplicationContent::load`**；因此把 `load` 的 `pool.begin()` 换成 `db::begin_write` 它照样全绿 —— 它证明的是"SQLite 的快照隔离没坏"，不是"`load` 用的是读事务"。`content.rs` 的生产注释还把"这个区分由该测试断言"写成了事实。
修：新增 `load_does_not_block_on_a_concurrent_writer_holding_the_write_lock` —— 真跑生产入口，且在**另一条连接持写锁**（`begin_write` + 一条未提交的 DELETE）时调用它；读事务从快照继续（WAL 下读不被写阻塞），而 `BEGIN IMMEDIATE` 会先在 `busy_timeout` 里干等再失败。同时改正 `content.rs` 里那句错误指向。
反向证伪：把 `load` 改成 `db::begin_write(pool)` ⇒ FAIL（3 s 超时打印 `Elapsed(())`，即真被写锁挡死）。

**B2【P2】`sqlite_write_lock.rs::deferred_begin_is_the_hazard_begin_write_removes` 两支都放行 ⇒ 永不失败。**
`match write { Ok(_) => {} , Err(e) => assert!(…) }`：`Ok` 分支是空块，`Err` 分支只校验错误码 —— 任何实现下都不会红。而 doc 却称它"会在未来某次重构把配置面悄悄改回 `pool.begin()` 时大声失败"（它其实连配置面都没碰，跑的是自己池上的裸 SQL）。
修：改为断言**危险真的发生**：先让 tx2 读出一个快照、tx1 提交、然后 tx2 的升级写必须 `Err`，且错误码必须是 `5`/`517`（正是 `classify_db_err` 映射成可重试 `503 storage_busy` 的那两个码）。doc 也改成实话，并指向真正守卫该选择的那条测试（`a_second_writer_waits_for_the_first_instead_of_failing`）。
实测 3/3 稳定通过；**敏感性证伪**：把 tx2 的读去掉（于是不存在陈旧快照）⇒ 插入**成功**（`changes: 1`）⇒ `expect_err` 失败。这条证明断言是针对"危险条件"本身的，不是"随便一次插入失败"。

### C. 本轮验证

| 门禁 | 退出码 | 计数 |
|---|---|---|
| fmt / clippy×3（server、optional、tls-openssl） | 全部 **0** | |
| `test hydra-core` | 0 | **249 / 0** |
| `test hydra-server --features server` | 0 | **532 / 0 / 1 ignored**（529 + sink 2 + content 1） |
| 可选 + 活 Redis/CH（`--no-fail-fast`） | 101 | **658 / 2 / 3**；2 例仍为 D-2（`admin_api.rs:1724`、`:3063`） |
| findings-disposition / i18n | 0 | 27/27 |

### D. 批次 4 剩余
`version_from_env` 的零覆盖（`HYDRA_ENCRYPTION_KEY_VERSION` 的解析无任何测试引用）；reseal 的 `already_current` 语义（D-6 候选）；租户面 operator 前缀探测（需决策）；`load_test.sh` 的分布判定读错文件；CI 的空洞（`--ignored`、`sqlx prepare --check`、无 docker/SDK 作业）；`.dockerignore` 的 AI-agent 目录与 `!bin/` 空操作；以及 D-5。

---

## 1p. 第十四轮：批次 4 第三批（一个假绿脚本、一个单向门、两处卫生）

### A. `scripts/load_test.sh` 的分布判定**读错了文件**（假红，而且自检是假的）
`$counts_file` 里写的是**实例名**（每行一个），而判定 awk（`{ c[NR]=$1; s+=$1 }` … `min = c[NR]/s`）把 `$1` 当成**计数**：`min` 实际是"最后一个实例名 ÷ 实例名之和"。`uniq -c` 只被用来**打印**。
**独立复现**（本轮我亲自跑过）：把生产形状 `750×9001 + 250×9002`（正确的 3:1）喂给旧逻辑 ⇒ `FAIL minority share 0.001 outside [0.150, 0.350]`（**正确分布被判失败**）；非数字实例名 ⇒ `awk: 致命错误：试图除0`。而计划 §1j 声称的"三组输入自检"用的是 `uniq -c` 形状 —— **与生产输入形状不同**，所以那个自检不是证据（与计划自己批判的"假 `now` 测真实时钟"同形）。
修：①判定输入改成 `uniq -c` 的 `<count> <name>` 表；②判定抽成函数 `distribution_verdict`，并且**少数派份额取所有行里的最小值**（旧的 `c[NR]/s` 只在两实例时恰好等于最小值，三个以上就错）；③新增**真正执行**的自检 `distribution_verdict_selfcheck`，在脚本开头就跑，失败即 `exit 1`（四组：3:1 必须 PASS、9:1 必须 FAIL、1:1 必须 FAIL、**乱序且末行落在带内而最小值落在带外**必须 FAIL）。
**反向证伪**：把判定改回 `share = c[NR] / s` ⇒ 自检 **FAIL** 并打印 `three instances must compare the SMALLEST share, got: PASS minority share 0.320`。
**过程记录**：这条自检第一版的第三组用例**不具鉴别力**（我选的 `100/450/450` 让两种逻辑都落在带外 ⇒ 都 FAIL ⇒ 自检照样通过），是**反向证伪把它暴露出来的**；已改成"末行在带内、最小值在带外"（`100/580/320`），两种逻辑才会给出不同结论。

### B. 主密钥轮换的单向门：`already_current` 从来没有验证过（旧口径：版本号相同就等于已用当前钥加密）
`db.rs::reseal_secrets` 对 `version == current` 的行直接 `already_current += 1; continue;` —— **从不调用 `kp.open()`**。而版本号与**密钥材料**是两个独立维度：换钥但不加 `HYDRA_ENCRYPTION_KEY_VERSION` 时，每一行的版本号仍等于 current，于是报告 `already_current=N, failed=0` ⇒ `is_complete()` 真 ⇒ **`main.rs` 退出码 0**；运维按 `ops.md` §3 的 SOP 删掉旧钥之后，进程再也起不来（`open()` 只按版本号取钥、无回退），**没有回头路**。
修：`already_current` 改为**实证** —— 该行必须真能用当前钥打开，否则进 `failed` 并点名，附一句"很可能换了钥但没bump版本号；在解决之前不要删旧钥"。两条循环（provider_key 与 tenant cert）都已改。
守卫：`tests/key_rotation.rs::a_row_labelled_current_but_sealed_with_other_material_is_reported`（用材料 A 以版本 1 封装，再用**材料 B、版本仍为 1** 跑 reseal）。
反向证伪：把两条循环改回未验证的捷径 ⇒ FAIL，报告正是那个陷阱的样子：`ResealReport { provider_keys_resealed: 0, tenant_certs_resealed: 0, already_current: 1, failed: [] }`。

### C. `version_from_env` 的零覆盖（§1h 把它当防呆卖点，却没有任何测试引用）
`HYDRA_ENCRYPTION_KEY_VERSION` 的整个解析就是 `version_from_env`，而它在 `crates/` 内**没有任何测试引用**。新增单测覆盖：未设置 ⇒ 取默认；空串/纯空白 ⇒ 取默认（manifest 里写空值应等价于不写）；`"2"`、`" 7 "` ⇒ 解析（含 trim）；**`"0"` 与 `"abc"` 必须 Err**（fail-closed，不能悄悄退回默认）；`"-1"` 必须 Err。每个用例用自己的变量名，因此不会与并行测试相互污染。
反向证伪：把它改成"解析失败即返回默认"（fail-open）⇒ FAIL，报 `HYDRA_TEST_KEY_VERSION_ZERO="0" must be REFUSED (fail closed), not defaulted`。

### D. 卫生两处
`.dockerignore` 补上 `.gitignore` 里标着 "never commit" 的 AI-agent 目录（`.claude`/`.opencode`/`.agents`/`.cursor`/`.windsurf`/`.continue`/`.codeium`/`.aider*`，它们常含会话记录）与本地 UI-E2E 产物（`test-results`/`playwright-report`）。
另外**纠正计划里的一处说法**：`.dockerignore` 的 `!bin/`、`!bin/hydra` 实测是**空操作**（没有任何规则排除 `bin/`，删掉这两行 `bin/hydra` 仍然 included），所以 §1j.D 里写的"正对照 `bin/hydra`（`!bin/hydra` 取反）仍 IN CONTEXT"把"没被排除"说成了"取反生效"。保留这两行作为防御（若将来有人加规则排除 `bin/`），但描述已更正。

### E. 本轮验证

| 门禁 | 退出码 | 计数 |
|---|---|---|
| fmt / clippy×3 | 全部 **0** | |
| `test hydra-core` | 0 | **249 / 0** |
| `test hydra-server --features server` | 0 | **534 / 0 / 1 ignored**（532 + crypto 1 + key_rotation 1） |
| 可选 + 活 Redis/CH（`--no-fail-fast`） | 101 | **660 / 2 / 3**；2 例仍为 D-2（`admin_api.rs:1724`、`:3063`） |
| findings-disposition / i18n | 0 | 27/27 |

`scripts/load_test.sh` 的判定不进门禁（需活负载工具），本轮用**独立提取 + 直接调用**的方式验证：自检四例全对、生产形状数据得 `PASS minority share 0.250`。

### F. 批次 4 剩余
CI 的空洞（`#[ignore]` 用例从不运行、无 `sqlx prepare --check`、无 docker/SDK 作业）；租户面 operator 前缀探测（需决策）；以及 **D-5**（门禁脚本不在版本控制里）。

---

## 1q. 第十五轮：批次 4 第四批（CI 空洞：`#[ignore]`、特性矩阵、compose、SDK）

### A. 加一个新门禁，先被它抓出两个真缺陷（`--features proxy` 根本编译不过）
`crates/hydra-server/Cargo.toml` 声称"`proxy` 必须能独立构建（W4b follow-up）"，而 `--features proxy`（**不带任何 TLS 后端**）从未被任何作业编译过。我加这条检查的第一步就是跑它，结果 **exit 101**：
1. `tests/metrics.rs` 整个测试二进制**编译失败**（`error[E0433]: cannot find tls in hydra_server`）—— 该文件的 gate 是 `db+http-client+proxy`，而其中的 `tenant_cert_gauge_tracks_snapshot_changes` 用了只在 TLS 后端下存在的 `hydra_server::tls`。后果不只是这一条：**这个文件里其它所有测试在该配置下也一并停止编译与运行**。修：把 `#[cfg(any(feature = "tls-boringssl", feature = "tls-openssl"))]` 加到那条测试上（而不是加到文件上——那样会牺牲其余测试的覆盖）。
2. `proxy.rs` 的测试助手 `note_for` 只被那条 TLS-gated 测试使用 ⇒ 在无 TLS 构建下是**死代码**，而 CI 风格（`-D warnings`）里死代码是**错误**。修：把助手 gate 成与它服务的测试相同。
修完 `cargo clippy --workspace --all-targets --features hydra-server/proxy -- -D warnings` **exit 0**。

### B. `#[ignore]` 用例：此前**没有任何作业运行**（`grep -c -- --ignored` = 0）
而它们承载着**两件本仓库已经做错过的事**的唯二端到端证据：ClickHouse 重试幂等（重投不得重复计费）与聚合读。唯一载体是 `.acceptance/t3-baseline.sh`（而该目录整个不在版本控制里，见 D-5）。
本轮**先在本机把三条都跑通**（这样才能保证新作业是绿灯可期的），再进 CI：
- `boot_listeners::same_port_on_different_addresses_is_not_detected_statically`（无需外部服务）⇒ ok；
- `clickhouse_sink::clickhouse_sink_writes_batch`（需 `CH_URL`）⇒ ok；
- `usage_query::live_clickhouse_aggregate_matches_a_hand_run_query`（需 `CH_URL`）⇒ ok。
新增：`optional-features` 作业加一步跑 **无需服务**的那条；新作业 **`live-deps`**（GitHub 服务的 ClickHouse 容器 + 真实 Redis）跑需要 `CH_URL` 的两条。

### C. `live-deps` 顺便把第十一轮那个 P1 变成**被检查的事实**
该作业在跑测试前，先用 HTTP 接口把仓库里的 `environment/clickhouse/init.sql` **真正执行一遍**，再断言 `SHOW CREATE TABLE usage_record` 里含 `non_replicated_deduplication_window`。本机已逐条验证该路径可行：`init.sql` 能干净地应用到**全新库**（`?database=ci_probe` 实测通过），且 `SHOW CREATE TABLE` 确实把该设置打印出来（`SETTINGS index_granularity = 8192, non_replicated_deduplication_window = 1000`）。也就是说：第十一轮"全新生产实例静默失去去重"那个 P1，从此由 CI 直接盯着**官方路径**，而不只是由 `tests/clickhouse_ddl_parity.rs` 比对文件。

### D. 另外三处空洞
1. **compose 从未被校验**：三个 `docker-compose*.yml` 是交付物，而 YAML 笔误／插值错／端口写错只会在运维机器上暴露。新增一步 `docker compose -f <file> config -q`（用 dummy secrets 满足 `${VAR:?}`；本机实测三个文件全 VALID）。机器上运行的仍是真脚本；
2. **三个 SDK 的测试没有任何作业运行**：本轮修掉的"SDK 指向已删除的管理面路由 + 错端口"只由 SDK 自己的测试守着，而它们不在 CI 里。新增 `sdks` 作业（Go / Node / Python）。本机实测：Go `go test ./...` ok（需可写的 `GOCACHE`；在本沙箱里默认缓存目录只读，属环境问题）、Python `python3 -m unittest discover -s tests` **29 tests OK**、TypeScript `npm test` **28 pass / 0 fail**。
   **一个被检查纠正的假设**：TS 包**没有提交 lockfile**，所以 `npm ci` 会直接失败 —— 已改用 `npm install`（若将来提交 lockfile，应换回 `npm ci`）。
3. **`alt-tls-backend` 作业只有 clippy、从不链接**，且现在要覆盖两种特性组合；已改名为 **`alt-features`**，并为 `tls-openssl` 补 `cargo build`（本机 exit 0）、为 `proxy`-only 补 clippy。
   **另一个被 YAML 抓到的自作自受**：我给新步骤起的名字里含 `#[ignore]d`，而 YAML 的裸标量里 `#` 是注释起始 ⇒ 步骤名被静默截断成 `…(normally`；已加引号。

### E. 本轮验证

CI 本身跑不了，所以**每一个新增的 CI 步骤都在本机按逐字命令验证过**（这是本轮的主要工作方式）：

| 新步骤 | 本机结果 |
|---|---|
| `clippy --features hydra-server/proxy -- -D warnings` | **exit 0**（修掉 `metrics.rs` 的 TLS 门与 `note_for` 死代码之后；修之前 exit 101） |
| `build --workspace --features hydra-server/tls-openssl` | **exit 0**（11.6s） |
| `cargo test … --test boot_listeners -- --ignored` | ok（3.02s） |
| `cargo test … --test clickhouse_sink --test usage_query -- --ignored`（`CH_URL` 指向活实例） | 两条都 ok |
| `curl --data-binary @environment/clickhouse/init.sql` 到**全新库** | 应用成功；`SHOW CREATE TABLE` 含 `non_replicated_deduplication_window = 1000` |
| `docker compose -f … config -q` ×3 | 三个文件都 VALID |
| Go SDK `go test ./...` | ok（0.019s；需可写 `GOCACHE`，本沙箱默认缓存目录只读属环境问题） |
| TS SDK `npm test` | **28 pass / 0 fail**（`npm install` 而非 `npm ci`——该包没有 lockfile） |
| Python SDK `python3 -m unittest discover -s tests` | **29 tests OK** |

门禁（本机全套，`.acceptance/round10-gate.sh`，**已把三条新 CI 检查并入**）：

| 门禁 | 退出码 | 计数 |
|---|---|---|
| fmt / clippy×4（server、optional、tls-openssl、**proxy-only**） | 全部 **0** | |
| `test hydra-core` | 0 | **249 / 0** |
| `test hydra-server --features server` | 0 | **534 / 0 / 1 ignored** |
| 可选 + 活 Redis/CH（`--no-fail-fast`） | 101 | **660 / 2 / 3**；2 例仍为 D-2（`admin_api.rs:1724`、`:3063`） |
| **ignored: listener limitation** / **compose config** | 0 / 0 | 新增两条 |
| findings-disposition / i18n | 0 | 27/27 |

### F. 批次 4 剩余
`sqlx prepare --check`（**已评估为低价值、暂不加**：离线构建本身就会因 `.sqlx` 缺少匹配条目而编译失败，所以"查询改了但缓存没更新"已被现有作业覆盖；真正需要它的是"只在联网构建时才暴露"的场景，本仓库全用 `SQLX_OFFLINE=true`）；租户面 operator 前缀探测（需决策）；以及 **D-5**。

---

## 1r. 第十六轮：特性矩阵的第二个（也是更大的）洞 —— manifest 声称的"每波可独立编译"是假的

第十五轮修好了 `--features proxy`（无 TLS 后端）。顺着同一条线索往下查，`crates/hydra-server/Cargo.toml` 的开头注释还声称：

> "Granular, composable feature set so each wave can compile its slice of the I/O shell WITHOUT pulling unrelated native deps (**e.g. W2/W3 must build with only sqlx/reqwest and never touch pingora/BoringSSL on macOS**)."

**实测（本轮逐字跑了 clippy，`-D warnings`）**：`runtime`、`db`、`http-client` 三个裸切片**全都 exit 101**；连 `runtime,db`、`runtime,http-client`、`runtime,db,http-client`（也就是注释所指的那个"只有 sqlx/reqwest、不碰 pingora"的 W2/W3 构建）也**全都编译不过**。错误清一色是 E0433，且只有两条耦合边：

| 耦合边 | 站点 | 为什么 |
|---|---|---|
| `http.rs` → `crate::admin::metrics` | **5 处**（277/395/432 是生产 `record_*`；1141/1155 是测试读取 `allow_ttl_capped_total`） | `admin` 模块 gated 在 `proxy` 上（`lib.rs:119-120`） |
| `store.rs`(26-27)、`db/restore.rs`(74) → `crate::cluster::{content,snapshot}` | 4 处 | `cluster` 模块 gated 在 `proxy` 上（`lib.rs:46-47`） |

`runtime,db,http-client` 那次是 **lib 6 个错误 + lib test 3 个错误**。
**处置**：修（a）确实是机械的（`sink.rs:97-105` 已有 `#[cfg(feature = "proxy")]` 的 no-op 先例），但（b）不是机械的 —— 它等于在问"**副本 restore 路径到底属不属于 `db`**"，那是设计问题，不是打字问题；只做（a）而不做（b）会得到"半修"，没有任何可验证的结果（构建照样不过）。因此本轮**先把假话改成实话**：重写该注释，写明两个耦合边的精确站点、实测数字、以及**CI 真正编译的四个组合**（`server` / `server,cluster-redis,usage-clickhouse` / `tls-openssl` / `proxy`），并指出 macOS 开发者要躲 BoringSSL 应该用 `proxy` 或 `tls-openssl`（这条现在是有 CI 检查的、真的能用），而不是那两个裸切片。
把（a）+（b）记为**批次 5 第 1 项**，并把（b）的设计问题写成一句可决策的话（供决策：`db` 是否拥有 restore 路径）。
**过程记录**：这条注释我最初写的是"`http.rs` 调 `crate::admin::metrics::record_*` 5 处" —— 逐行核对后发现其中 2 处其实是测试里读 getter，不是 `record_*`；`store.rs` 也不只用 `content`，还用 `snapshot`。两处都已在注释里改准。**正是在做这种"逐行核对"时抓到上一轮那种假注释的同类错误，说明"写进文档的每个数字都必须现场数一遍"这条纪律还得继续执行。**

---

### G. 本轮的两处**测量**错误（同一个根因）
1. 我先算出"可选套件 661 passed"（比上一轮的 660 多 1），而本轮根本没有新增测试。原因是 `.acceptance/round10-gate.sh` 新加的三条门禁插在了日志里 `test optional` 与 `findings disposition` 之间，而我的 `awk '/test optional/,/findings/'` **把这些新段落一起框进去了**，于是那条 `--ignored` 测试的 `1 passed` 被算进了可选套件。
2. 顺着这条线索核对，又发现计划里"core **17** 个测试二进制"也是错的：实际是 **16**（15 个 `tests/*.rs` + lib 单测）。它同样是"按段落求和"数出来的，同一个根因。
修：改成**按 section 切分日志**再逐段求和（`re.split(r'^########## ')`），并把数对写回计划与 INDEX。实测（本轮）：core **249/0/0（16 个二进制）**、`--features server` **534/0/1（41 个）**、可选+活依赖 **660/2/3（41 个）**、`ignored: listener limitation` 单独 **1/0/0**。
**教训（第三次同类）**：范围型的 `awk`/`grep` 统计会随着日志结构变化而**静默改变含义**。要么按结构化边界切分，要么现场逐项数。

---

## 1s. 第十七轮：把 CI 与本机门禁补齐到"同一套"，并核实 §4 的每一行

### A. ClickHouse 容器的就绪命令：**实测**而不是猜
`live-deps` 作业里我原先写的是 `--health-cmd "wget -qO- http://127.0.0.1:8123/ping"`。直接进正在运行的 24.3 容器里查：
- `wget` **存在**（`/usr/bin/wget`）⇒ 原写法能用；
- `curl` **不存在** ⇒ 作业里那些 `curl` 必须留在 **runner** 上（它们确实在 runner 上）；
- `clickhouse-client` **存在**，而本仓库自己的 compose 用的就是它（`environment/docker-compose.yml:117-118`，注释写着 "clickhouse-client is bundled in the official image"）。
**改**：就绪命令换成仓库已验证过的 `clickhouse-client --query 'SELECT 1' || exit 1`（少依赖一个"碰巧装了 wget"），并在注释里记下上面三个实测事实，免得后人再把 curl 放进容器。

### B. 本机门禁补上 ClickHouse e2e（CI 那边在 `live-deps` 里）
第十五轮我把"无需服务的 `#[ignore]` 用例"并进了本机门禁，但**需要 `CH_URL` 的那两条**只进了 CI —— 也就是本机门禁仍然可以全绿而完全没跑过"重试幂等"这条唯二的端到端证据。补上第 13 条门禁（`clickhouse_sink` + `usage_query`，`--ignored`），实测两条都过（0.52s / 0.04s）。
门禁脚本现在 **13 条**：第十轮 9 条 + 第十五轮并入的 proxy-only clippy / ignored-listener / compose config + 本轮的 CH e2e。

### C. §4 复现清单重写成"与 CI 一一对应"，并且**每一行都真的跑过**
§4 原来只有 8 条命令、且与 CI 的七个作业对不上（`alt-features` 只写了 tls-openssl、没有 proxy-only 与 build；`live-deps`、`sdks`、compose 校验、三条 shell 检查全都不在里面），还缺 `--no-fail-fast`。现已重写为逐行标注对应作业名的完整清单，并把**每一条都在本机跑过**这件事落实：
- `node --test scripts/check_i18n.test.cjs`、`bash scripts/ask_llm.test.sh` 本轮补跑（后者输出 `E3 (ask_llm + dead-code): ALL PASSED`，exit 0）；
- Go 那条**如实标注**：本沙箱里默认 `GOCACHE`（`~/.cache/go-build`）只读，逐字执行会得到 `[setup failed]`（**环境问题，不是仓库问题**），可写缓存下实测 `ok … 0.019s`；
- 并把"门禁脚本每次会**截断自己的日志**（所以上一轮原始日志不可复现）"和"该脚本本身不在版本控制里（D-5）"两条写进去。

### D. 顺带核实的两件小事
`environment/docker-compose.yml` 的健康检查用的是 `clickhouse-client` 而非 curl；本机三个 compose 文件在 dummy secrets 下 `config -q` 全部通过（与 CI 的 `scripts` 作业一致）。

---

## 1t. 第十七轮下半 · 第 12/13 轮改动的对抗性复审结论（第一路：鉴权面 + sink）

复审确认了第 12/13 轮的主要修复（下面"已核实为真"一栏），但抓出 **1 条 P1**、**5 条 P2**、**4 条 P3**。

### A. 【P1】官方集群拓扑下 **edge 的 `/metrics` 任何人都抓不到**（我第 7 轮那次统一门禁漏掉的另一半）
**事实链（我逐条复核过）**：
- edge 分支只白名单 `/healthz`、`/readyz`，**`/metrics` 会落到** admin-token 门禁（`admin/mod.rs:866-873` → `:930`）；
- 而 `check_auth` 在**未配置 token 时 fail-closed 返回 false**（`admin/mod.rs:492-495`：`let Some(token) = &self.state.admin_token else { return false }`），token 的唯一来源是 `HYDRA_ADMIN_TOKEN`；
- 官方 `environment/docker-compose.cluster.yml` 的 `hydra-edge` 只给了 `HYDRA_CLUSTER_TOKEN`（`:97`），**没有** `HYDRA_ADMIN_TOKEN`；
- 文档把这个差异写成规格：`dev-docs/jiqun-deploy.md:69` —— "**leader 必填**……edge 无 CRUD API，**可不设**"；而 `dev-docs/cluster.md:17` 又把 edge 的角色描述成"仅 `/metrics` `/healthz` `/readyz`"。
⇒ **在所有 edge 节点上，`/metrics` 对任何凭据都回 401**，于是数据面节点**完全没有指标**（`hydra_requests_total`、`hydra_breaker_dead`、`hydra_auth_cache_size` 等在该节点的标签下恒为空），而 `ops.md` §9.1 的告警契约假定它们存在。
**为什么没被测出来**：`admin_api.rs` 的 edge 用例（`:1049-1063`）断言的正是"无 token ⇒ 401、带 token ⇒ 200"，但它构造的 state 由 `admin_state_with` 固定传入 `Some(TOKEN)`（`:66`）——**"edge 没有 admin token"这个真实形态从未被构造过**。这是我第 7 轮那次修复的测试盲区：我验的是"edge 的 /metrics 也要 token"，没验"edge 拿不到 token 会怎样"。
**处置**：修复需要安全取舍（见 **D-8**），本轮不单方面改；但**事实与矛盾已在计划里钉死**。三条候选：(a) 给 edge 注入 `HYDRA_ADMIN_TOKEN`（注意：edge 上其它路径在校验 token **之前**就 404 了，所以它只解锁 `/metrics`；代价是数据面节点持有那把"能改全部 provider key"的凭据，被攻陷即外泄）；(b) 恢复 edge `/metrics` 免鉴权，但把 edge 管理口绑到私有地址/网络（官方拓扑把它绑在 `0.0.0.0:8081`）；(c) 引入仅用于抓取的独立 token（新增门禁语义）。

### B. 【P2】我的第 13 轮注释把**机制写错了**（已在本轮修掉）
`main.rs:1504-1506` 在 `sink.shutdown().await` 之后**只打了 `info!("usage sinks flushed")` 然后返回**，**没有** `process::exit` —— 我却在 `sink.rs` 的注释与测试 doc 里把"截断"归因于它。真机制是 Pingora 自己的关闭时序：`graceful_shutdown_timeout_seconds: Some(5)`（`main.rs:1210`）→ `run_forever` 以 `std::process::exit(0)` 结束（`pingora-core-0.8.1/src/server/mod.rs:640`，不跑析构）。
**结论（要封顶窗口）不变、论证链错了** —— 这正是本项目反复出现的同型事故。已改写两处注释；顺带发现 `main.rs:1563-1565` 自己的注释**本来就写对**（"pingora's SIGQUIT/SIGTERM path ends in `process::exit(0)`"），即同一仓库里两处注释互相矛盾。测试重跑：`the_final_drain_finishes_inside_the_shutdown_budget` ok（4.00s，行为未变）。

### C. 记入**批次 5**（复审发现，本轮未修）
2. **两把锁共享同一失败预算 ⇒ 跨闸门连带限速**：`/api/v1/internal/*` 的失败会消耗**同一 IP** 的预算，从而让该 IP 随后**管理面**的失败请求拿到 429（生产上真实场景：standby/edge 的转发失败会连带把同地址运维的 admin 请求打成 429）。计划把共享写成"只会更早限速"，成立但**没写连带影响**，`ops.md` 也没有。修法：键加 gate（`format!("{gate}|{key}")`）或内部前缀用独立 `Throttle`；两者都不影响"成功永不拦"。
3. **`auth_fail_throttle` 表从不 GC**：`Throttle` 是 `DashMap`，租户面三张表都有后台 gc（`tenant_api/limit.rs:258`），而第 12 轮新增的这张**全仓无 `gc()` 调用点**；键由 `peer_ip`（**未认证即可产生**）决定且**永不过期** ⇒ IPv6 客户可用不同源地址各留一条永久记录（≈60–100 B/条），与"/64 折叠堵住预算乘法"的初衷相反（这条是**内存**侧的洞）。修法：接进既有 gc 循环，一行。
4. **`/64` 折叠只覆盖 `::ffff:` 一种嵌入形态**：`to_ipv4_mapped()` 不认 IPv4 兼容地址（`::a.b.c.d`）、6to4（`2002:V4ADDR::/48`）、NAT64 ⇒ 每种形态各得一个桶，双栈客户可把 10/min 变成 20/min、再加形态数倍。修法：`None` 分支先试 `to_ipv4()`，并把 6to4/NAT64 的残余**如实写进注释与 `ops.md:933`**（现在的措辞读起来像已经封死）。
6. **最终 drain 的窗口从 30 s 收到 4 s 是一个未被承认的取舍**：后端在 4–25 s 之间恢复的关闭场景，从"这批成功落库"变成"整批丢弃并记 `shutdown_unflushed`"。注意 Pingora 侧其实给了 ≈20 s（`grace_period_seconds` 默认 20）+5 s，所以 4 s 并非上限。修法（复审建议）：把**上报前置**（drain 之前先 `warn!` 声明"若失败将丢 N 条"），从而窗口可以保留更长；或把窗口设为预算的一部分并把取舍写进 `ops.md`。
7. **标签重命名留下 3 处不一致描述**（本路复审发现，属"我们自己引入的漂移"）：`admin/metrics.rs:60` 的模块目录表仍写 `denied|throttled`、同文件 `:463` 的 help string 同样、`ops.md:1186` 的环境变量表也写 `{result="denied"|"throttled"}`（**照此写规则永远不匹配**）；只有 `ops.md:933` 是对的（`{result=~".+_throttled"}`）。修法：三处统一为 `<gate>_denied|<gate>_throttled`。
8. **`MIN_CLUSTER_TOKEN_LEN` 无测试、无文档**，且门槛是**长度不是熵**（`"aaaaaaaaaaaaaaaa"` 也通过），而注释写着 "must both be unguessable"。修法：文档补"≥16 且必须随机"，并抽 `validate_cluster_token` 纯函数加单测。
9. **睡眠封顶的边界**：`delay.min(remaining)` 在窗口末尾会退化为 0 ⇒ 那个"最后尝试"不再有退避间隔（当前无害，但窗口调小就会变成忙循环）。修法：`remaining` 为零时直接返回 false，或 `.max(1ms)`。
10. **窗口算式与 `MAX_SHUTDOWN_WAIT` 之间没有编译期约束**：若该常量被调到 ≤1 s，`saturating_sub(1s)` 会让窗口变 0，最终 drain 退化为"一次裸尝试后整批丢弃"。修法：`.max(Duration::from_secs(1))` + 注释。
11. **我第 13 轮那条新测试的承重范围小于它的 doc**：`every_retry_sends_the_identical_batch_in_the_same_order` 只把 `Vec` 喂进 `flush_with_backoff`；生产中唯一能破坏该不变量的地方是 `buffer.extend(returned)`，而 `returned` 就是同一个 `Vec` ⇒ **真实回归下它不可能变红**，doc 里那句 falsification（"让重试时先 push 进 buffer"）是测试自己做不到的操作。修法：若要它承重，必须经 `run_channel_sink` 的 ticker 分支在重试期间并发 `tx.send`，断言"重试中到达的记录不会混进本批"。

### D. 复审**确认成立**的（第 12/13 轮修复）
预算只在失败路径消费（`Throttle::allow` 的拒绝分支不计数 ⇒ 带正确 token 永不被拦、也不延长锁定）；`peer_ip` 丢端口且不读 XFF（与注释一致）；`/64` 折叠本身正确（只清 `segments()[4..]`，三条单测与实现一致）；IPv4 与 mapped 各自成桶（"双栈把所有 IPv4 客户端折进 `::/64`"确实被堵住）；`refuse_bad_credential` 是两把锁的唯一 401 出口；`classify_db_err` 已是单所有者且 `5|517` → 503 可达；**"重试期间 buffer 不会变"对所有调用点成立**（`flush_with_backoff` 只在两处以 inline await 调用，全仓 `buffer.push` 仅一处）；`note_usage_drop` 在丢弃时确实可达；`new_batch_id` 一批一个且重试复用，没误发给非 ClickHouse 后端；`clickhouse_insert_statement` 形态正确且两条单测在 `optional-features` 里会跑；集群 token 长度下限不破坏任何文档流程（全部示例是 `openssl rand -hex 32`，且 CI 那个 20 字符假值只用于 `docker compose config -q`）。

---

## 1u. 第十七轮下半 · 第 15/14 轮改动的对抗性复审结论（第二路：CI + 文档）

复审确认了第 15 轮绝大多数 CI 判断（见"已核实为真"），但抓出**两条 P1，都是我自己第 15 轮引入的**，本轮已修。

### A. 【P1 · 已修】`scripts` 作业在**干净检出上必然红**：`docker-compose.local.yml` 引用了被 gitignore 的 `env_file`
`environment/docker-compose.local.yml:43/83/121` 三处 `env_file: [../secure/local-test.env]`，而 `secure/` **不在版本控制里**（`git ls-files secure/` = 0，`.gitignore:49`）—— 那里放的是真的上游 api-key。我第 15 轮把三个 compose 文件都塞进 `docker compose config -q` 循环，而 compose **对缺失的 env_file 不退让**（非零退出）。
**我自己的复现**（非破坏性：把 compose 复制到一个没有 `../secure/` 的临时树里跑）：`config -q` ⇒ **exit 1**，stderr `env file /tmp/cleanco/secure/local-test.env not found`。⇒ 那个作业在每次干净 CI 上都是红的，i18n/ask_llm 之外的这条新门禁永远拿不到绿。计划 §1q.E 写的"三个文件都 VALID"是在**有**这个未跟踪文件的开发机上证的 —— 又一次"用不同输入形状自证"。
**修**：两个**官方**栈无条件校验；本地栈**仅在该 env_file 存在时**校验，缺了就**大声 SKIP**。两分支都实测过：干净树里 ⇒ 两个官方文件过 + `SKIP … (no secure/local-test.env)` + exit 0；本仓库里 ⇒ 三个都过（`local compose: validated`）。本地门禁脚本同步改成同一逻辑。

### B. 【P1 · 已修】新加的 `--ignored` 步骤**永远不会执行**（因为它在仍红的全量测试之后）
`optional-features` 的全量测试步骤排在前，而该配置下 `admin_api` 两例（**D-2**）确定性失败；GitHub Actions 在前一步失败后不再执行后续步骤 ⇒ 我"给已知监听限制补了自动守卫"的说法**当前是假的**（守卫存在，但从不运行）。
**修**：把那条 `--ignored` 步骤**移到全量测试之前**（并在注释里写明"顺序是有意的：套件当前因 D-2 而红，跟在失败步骤后面的门禁等于没跑"）。已在解包后的 YAML 里核对步骤顺序：`… Build workspace → Ignored test … → Test hydra-server (optional features)`。

### C. 记入**批次 5**（复审发现，本轮未修）
- 【P2】`alt-features` 的 `cargo build --workspace --features tls-openssl` **不构建 `hydra` 二进制**：`[[bin]] hydra` 的 `required-features = ["server"]`（`cargo metadata` 实测），而 `tls-openssl` 不含 `server` ⇒ 只链接 lib。同理 `--features proxy` 那步只 lint lib+test。**后果**：`main.rs:409/1190/1308` 三处 `#[cfg(not(any(tls-boringssl, tls-openssl)))]` 分支在 CI 里**从未被编译**，而那正是"proxy 必须能独立构建"要检的东西。修法（需构建决策）：把 `hydra` 的 `required-features` 放宽为 `["proxy"]`（`main.rs` 本来就用 `#[cfg]` 处理无 TLS 的情况），或把注释改成"只链接库"。
- 【P2】`live-deps` 的 `usage_query` 端到端断言在 CI 上是**空表对空表恒真**：该作业只跑 `init.sql` 建表、**不插数据**，而该测试断言的是 `count()==expected` 与 `!rows.is_empty()`。本机之所以过，是因为本机 CH 里有数据 —— 又是"不同输入形状"。修法：断言 `expected.0 > 0`（"fixture 非空否则比较是空洞的"），或在步骤前插几行。
- 【P2】`.dockerignore` 的 `!bin/`、`!bin/hydra` 仍是**空操作**，但文件里的注释还写着 `# KEEP the staged binary`（计划 §1p.D 已自我更正、文件未同步）。
- 【P2】`sdks` 用 `npm install` + `^5.6.3`/`^20.14.0` ⇒ TS 依赖不锁定，与同一份 CI 里 Playwright 的 "PINNED" 策略自相矛盾。修法：提交 `tools/hydra-ts/package-lock.json` 并改回 `npm ci`。
- 【P3】`live-deps` 的 ClickHouse 就绪循环**耗尽不报错**（Redis 那段会 `exit 1`），失败信息退化成 curl 的网络错误。
- 【P3】`crypto.rs:181-184` 的 `HYDRA_ENCRYPTION_KEY_PREVIOUS_VERSION` 缺省仍是 `current-1`，而 `version_from_env` 明确拒绝 0 ⇒ v1 旋转会把旧钥插进**版本 0**槽位（当前无行会带 0，故无害，但两处口径不一致）。
- 【P3】`ci.yml` 无 `XTRIM`/`MINID` 专项断言步骤，P1-8 的 CI 证据同样受 B 条（D-2 致红）牵连。

### D. 复审**确认成立**的（我的第 15 轮 CI 判断）
`live-deps` 的服务容器可用（本机同 tag 镜像 24.3.18.7；`clickhouse-client --query 'SELECT 1'` 实测 exit 0；镜像有 `wget`/`clickhouse-client`、**无 curl** ⇒ 步骤里的 curl 必须留在 runner，与我的实现一致；runner 会等 healthy ⇒ 无竞态）；`curl --data-binary @init.sql` 在 24.3.18.7 上实测 exit 0；`SHOW CREATE TABLE` 含 `non_replicated_deduplication_window = 1000`（断言命中）；`init.sql` 是 `CREATE TABLE IF NOT EXISTS`，可重复执行；`CH_URL`/`HYDRA_TEST_REDIS_URL` 的名字与端口与测试一致；`scripts` 其余步骤本机真绿；三个 SDK 从干净检出可跑（Go 只用标准库+httptest、无 go.sum；TS 的 `test` 脚本自带 tsc 且产物路径一致；Python 只用标准库+临时端口）；`boot_listeners -- --ignored` 不需服务、不会挂死（用 `ephemeral_port()` 分带探测，非固定端口）；第 14 轮 `already_current` 修正**没有**把任何合法流程变成误报（合法 v1→v2 走重封路径、不经过新分支）；`load_test.sh` 的新判定自检真、可达、与生产同形；三个 compose 在变量齐备时全 exit 0，且 `${VAR:?}` 全集只有 3 个名字、无遗漏。

---

## 1v. 第十八轮 · 第三路复审（未覆盖模块）+ 修掉一条 P1

第三路复审专门看**从未被任何复审覆盖过**的模块（limiter/admission、registry/control_client、store、usage_query/clickhouse、sub_tenant_write、http 的鉴权缓存），抓出一条 **P1**，本轮已修。

### A. 【P1 · 已修】认证跳的响应体读取**没有任何期限、也没有大小上限**
**证据**：客户端 builder 只有 `pool_idle_timeout` 与 `tcp_nodelay`（`http.rs:546-550`，**无 `timeout`、无 `read_timeout`**；reqwest 0.12 两者默认都是 `None`）；现场唯一的期限是 `tokio::time::timeout(timeout, send)`（`http.rs:646`）——而 reqwest 的 `send()` **在收到响应头时就 resolve**，所以它只包住响应头；判定判决又**必须**读 body（`http.rs:684` 的 `resp.text().await`，无上限）。
**后果**：租户的 `auth_url` 只要"先回 2xx 头、body 永不结束"（挂死的认证服务，或把 200 当错误页的 WAF/LB），处理该请求的 task 就**永久**挂住（连接、请求上下文、reqwest 连接都不释放）；任何能对该租户发请求的人用**互不相同**的 api-key（每个都是 cache miss）即可无限扇出，既是跨租户可用性洞，也是无上限内存增长（`text()` 没有 `clickhouse.rs` 那种 64 KiB 截断）。
**修（行为保持）**：①客户端加 `.timeout(config.timeout)` 与 `.read_timeout(config.timeout)` —— 这正是 `AuthConfig::timeout` 文档所说的"auth_url 往返的每次调用期限"，而"往返"本就包含 body；②把 `resp.text()` 换成有 64 KiB 上限的 `read_auth_body()`，失败/停滞/超界时返回**已读到的字节**（与旧的 `unwrap_or_default()` 在"没找到显式拒绝标记即为允许"这一设计语义上一致，只是不再挂死、不再无上限分配）。
**测试**：`tests/http_auth.rs::a_stalled_auth_body_does_not_hold_the_request_forever` —— 原始 socket 先发头（`content-length: 4096`）然后 60 s 不说话（**必须用原始 socket**：wiremock 的 `set_delay` 延迟的是整个响应，会打中"响应头期限"这条另一条路径）。断言 200 ms 配置下调用在 5 s 内返回、判决为 fail-closed 的 `503 auth_upstream_unavailable`、且不缓存。
**反向证伪**：把客户端的 `.timeout`/`.read_timeout` 去掉（即修复前的状态）⇒ 测试 **FAILED**，并且实测 `the call took 60.00124867s`（一直挂到那个被卡住的 socket 自己关闭）——这条把"没有任何期限"变成了可复现的数字。恢复后 30 个 auth 测试全过（0.62s）。
（顺带：现有 29 个 auth 测试在改动后全部通过，说明期限与截断没有破坏既有契约，包括既有的超时/失败模式语义。）

### B. 第三路复审的其余发现 → 记入批次 5/6（本轮未修）
- 【P2】**L2 回填绕过 invalidation epoch 守卫**：`set_if_unchanged` 有"快照 epoch 变了就不写"的守卫（`http.rs:161-176`），而 `check` 的 L2 回填路径（`http.rs:244-255`）**既不采样也不比较** epoch，失效路径又是先 bump 再删 L1/L2 ⇒ 一次 `l2.get` 的 await 期间落地的撤销，可能把**已撤销**的 allow 判决用 PTTL 剩余值写回 L1（正是该守卫注释点名要防的形态）；附带该路径的 TTL **未经 `allow_ttl_max` 钳制**。修法：`l2.get` 前采样 epoch、插入前比对，或直接复用 `set_if_unchanged`（一次收口守卫与钳制）。
- 【P2】**`https://` 的 ClickHouse URL 被静默降级为明文 TCP**：`parse_clickhouse_url` 把 `http://`/`https://` 都剥掉只留 `host:port`（`clickhouse.rs:85-89`），凭据以 `Authorization: Basic` 明文写进请求（`:202-208`），连接是裸 `TcpStream::connect` 且**全模块无 TLS**（`:218-220`）——而模块文档把 `https://host:port` 列为"接受形式"。运维若按文档配 `https://`，凭据**明文上网**，且症状（连接成功但请求被拒/超时）会被误判为证书问题。修法：见到 `https://` **启动即 fail-closed**（纯代码可判）；真正支持 TLS 属产品决策。
- 【P2/P3】**db 错误分类器还有第三份，且已漂移**：`tenant_api/handlers.rs::map_core_err` 的 `CoreError::Db(d) => (500, "database_error", d.to_string())`，而其注释自称"The status / code are IDENTICAL to the internal face"——**不成立**：唯一所有者 `admin/handlers.rs:179-189` 把 SQLite 的 5/517 映射成**可重试的 503 `storage_busy`**，而写核心全程 `BEGIN IMMEDIATE`（超时正是 code 5）⇒ 同一个操作在管理面是 503、在**租户数据面是 500**（"这是 bug"），并把原始 sqlite 文本回给租户。**注意：§1n A3 只修了 `tenant_config_api.rs` 那一份**；这说明"单一所有者"当时只做了局部收口。修法：该分支改调 `classify_db_err`、原文进日志；是否给租户承诺 `storage_busy` 需产品决策（`tenant-api-integration.md` 的错误码表未列该码）。
- 【P3】`create_sub_tenant` 自称的"按 `(tenant_id, name)` 幂等 upsert"在**管理面 POST 上不可达**：两个分支都传 `self_id = None`（`sub_tenant_write.rs:173/188`），核心校验器在 `self_id=None` 时先撞 `NameDuplicate`（`core/src/sub_tenant.rs:341-355`），于是重复 POST 必然 409 而**不会**收敛到同一行 —— `db.rs:1730-1737` 那个 `ON CONFLICT … DO UPDATE` 与 A-2 prerequisite 4 在此路径上是死代码。租户面 PUT 不受影响。
- 【P3】admission 的 queue-full 是 **load→fetch_add 的 TOCTOU**（`admission.rs:311-326`）：N 个并发请求可同时读到 0 而全部入队，`max_queue_depth`（含 `0`="无队列"）不构成界，`hydra_queue_depth` 与 `Retry-After` 随之失真。修法：一次 `fetch_update` CAS 占位。

### C. 第三路复审**核对为正常**的（"没有发现"同样有信息量）
`limiter.rs` 的桶构造与 GC（先 evict 再据返回值 retain；`limit_count == 0` 拒绝且不建窗口；`check_tokens` 不建窗口；两个上限方向都 fail-closed）；`admission.rs` **无 permit 泄漏**（三条返回路径都 drop guard、DashMap 守卫不跨 await）；`registry.rs` 的值格式容错/两击回收/租约持有者不回收；`control_client.rs` 的 std 锁不跨 await、退避封顶 30 s、非递增快照被拒；`store.rs`（`PartialEq` 含 cfg 故只改证书也会 bump 并 notify；先落盘 marker 再发布；两条换快照路径都 notify）；`usage_query.rs`/`clickhouse.rs`（全参数绑定、唯一插值是白名单常量、`model_key/provider_id` 为 NOT NULL、分组基数受配置约束故**无**攻击者可控成行爆炸、CH 侧 64 KiB 截断 + 独立读超时）；`sub_tenant_write.rs` 的事务边界与自排除；`http.rs` 其余（L1 判定单次求值、deny/402/fail-mode 的缓存语义、显式 allow 才放行、`expires_in` 被 allow 钳制挡住 `Instant` 溢出）。

### D. 本轮门禁
`clippy -D warnings`（server 全目标）exit 0；`tests/http_auth.rs` **30 passed**（新增 1）；反向证伪实测 60.00124867s（见 A）。

---

## 1w. 第十九轮：`https://` 的 ClickHouse URL 不再被静默降级为明文

### A. 【P2 · 已修】写了 `https://` 却走明文 TCP，Basic 凭据明文上线
**证据**：`parse_clickhouse_url` 把 `http://` 与 `https://` **一样剥掉**只留 `host:port`（`clickhouse.rs:85-89`），凭据被拼进 `Authorization: Basic …`（`:202-208`），连接是裸 `tokio::net::TcpStream::connect`、**全模块无 TLS**（`:218-220`）——而模块文档把 `https://host:port` 列为"接受形式"（`:76-81`）。
**后果**：运维照文档把 `HYDRA_CLICKHOUSE_URL` 写成 `https://user:pass@ch.example.com:8443` 时，网关以**明文**连上去并把用户名口令发出；而对端若是只开 TLS 的端口，症状是"连接成功但请求被拒/超时"，排错方向会被引到证书上去。这是**静默的凭据泄露**：代码承诺 https、实现丢弃 scheme。
**修**：`ClickHouseConfig` 新增 `tls_requested`（**记住**而不是丢掉这个信息），`parse_clickhouse_url` 识别 `https://` 前缀；`clickhouse::send` 在**任何拨号/写包之前**拒绝该配置并给出可执行信息（"本构建没有 ClickHouse 的 TLS 传输，拒绝以明文发送凭据与用量行；请在内网/隧道上用 `http://`，或在数据库前面终止 TLS"）。因为 `send` 是唯一的连接点，一处守卫同时覆盖 writer（`sink.rs`）与 reader（`usage_query.rs`）。文档同步改为"`https://` 会被解析但随后被 `send` 拒绝"。
**测试**：`clickhouse.rs::an_https_url_is_refused_rather_than_sent_in_plaintext` —— 断言 scheme 被记住、`host:port` 与凭据仍被解析（正因如此传输层必须拒绝）、`send` 返回的 Err **同时**包含 `https://` 与 `PLAINTEXT`（即是我们主动拒绝，不是网络错误）；并断言明文 URL 不受影响（这是 scheme 检查，不是新禁令）。
**反向证伪**：删掉 `send` 里的守卫（即修复前状态）⇒ 测试 **FAILED**，实测错误变成 `clickhouse connect ch.example.com:8443: failed to lookup address information` —— 也就是说它**真的会去拨号**，明文凭据会离开进程。
**再加一层：启动即拒**（复审建议的"fail at startup"，本轮也做了）。`build_sink` 在拿到 URL 后就检查 `https://` 前缀并返回新的 `BuildSinkError::ClickHouseTlsUnsupported`（`main.rs:535` 用 `?` 传播 ⇒ **进程拒绝启动**）。否则症状是"节点正常起来、服务流量，直到第一次 flush 才发现每次写入都失败" —— 凭据两种方式都不会外泄，所以早失败严格更好。
守卫：`sink.rs::an_https_clickhouse_url_fails_at_build_time`（断言 offer 被拒且信息含 `https://` 与 `PLAINTEXT`；并断言明文 `http://` **仍能构建**——这是 scheme 检查而非新禁令）。反向证伪：删掉该检查 ⇒ FAIL（`https:// must be refused before any traffic is served`）。
（过程记录：这条测试第一版写成普通 `#[test]`，控制组那半会真的去 spawn 一个 sink 任务 ⇒ `there is no reactor running` 而失败；改为 `#[tokio::test]` 后两条都过。**测试自己的运行前提也是要被验证的东西**。）

**回归**：`--lib clickhouse` 27/0；活实例两条端到端（`clickhouse_sink_writes_batch`、`live_clickhouse_aggregate_matches_a_hand_run_query`）仍过（它们用明文 `http://127.0.0.1:8123`）；`clippy -D warnings` exit 0。

### B. 记入**批次 6**（复审发现、仍待处理）
1. 【P2】**L2 回填绕过 invalidation epoch 守卫**（`http.rs:244-255`）：撤销在途竞态可把**已撤销**的 allow 判决按 PTTL 剩余值写回 L1，且该路径 TTL **未经 `allow_ttl_max` 钳制**。修法：`l2.get` 前采样 epoch、插入前比对，或复用 `set_if_unchanged`。**验证难点已记**：需要"在 `l2.get` 与插入之间落地一次 invalidate"的时序注入。
2. 【P2/P3】**db 错误分类器还有第三份且已漂移**：`tenant_api/handlers.rs::map_core_err` 的 `CoreError::Db` 一律 `(500, "database_error", d.to_string())`，而其注释自称与内部面"The status / code are IDENTICAL"——**不成立**（唯一所有者把 SQLite 5/517 映射为可重试的 `503 storage_busy`），且把原始 sqlite 文本回给租户。**§1n A3 的"单一所有者"当时只做了局部收口**（只改了 `tenant_config_api.rs` 那一份）。修法：该分支改调 `classify_db_err`、原文进日志。**验证难点已记**：`map_core_err` 是私有函数，而构造 `sqlx::Error::Database` 需要真约束/真锁竞争（`sqlite_write_lock.rs` 有现成形状）。
3. 【P3】admission 的 queue-full 是 **load→fetch_add 的 TOCTOU**（`admission.rs:311-326`）：并发到达可让全部请求入队，`max_queue_depth`（含 `0`="无队列"）不构成界；修法：一次 `fetch_update` CAS 占位。
4. 【P3】`create_sub_tenant` 的"(tenant_id, name) 幂等 upsert"在管理面 POST 上不可达（`self_id=None` 先撞 `NameDuplicate`，`ON CONFLICT … DO UPDATE` 是死代码）。
5. 三处**标签描述不一致**（我第 12 轮重命名留下的漂移）：`admin/metrics.rs:60` 的模块目录表、同文件 `:463` 的 help string、`ops.md:1186` 的环境变量表都还写 `denied|throttled`，只有 `ops.md:933` 是新的正确写法 `<gate>_denied|<gate>_throttled` —— 照旧写法写的告警规则**永远不匹配**。
6. `.dockerignore` 的 `!bin/`、`!bin/hydra` 注释仍写 `# KEEP the staged binary`（实测是空操作，§1p.D 已自我更正、文件未同步）。
7. `MIN_CLUSTER_TOKEN_LEN` 无测试无文档，且门槛是**长度不是熵**（`"aaaaaaaaaaaaaaaa"` 也过），而注释写着 "both unguessable"。
8. drain 窗口 30s→4s 是**未被承认的取舍**（后端在 4–25s 之间恢复的关闭场景从"落库"变成"丢弃并计数"）；复审建议把上报**前置**（drain 前先 `warn!` 声明风险），从而窗口可保留更长。
9. 我第 13 轮那条 `every_retry_sends_the_identical_batch_in_the_same_order` **承重不足**（真实回归下不可能变红），doc 里的 falsification 是测试自己做不到的操作。
10. `alt-features` 的两步都没真正编译 `hydra` 二进制（`required-features = ["server"]`），故 `main.rs:409/1190/1308` 的 `#[cfg(not(any(tls-*)))]` 分支在 CI 里从未被编译 —— 正是"proxy 必须能独立构建"要检的东西（**D-7 相关**）。
11. `live-deps` 的 `usage_query` 断言在 CI 上是**空表对空表恒真**（该作业只建表不插数据）。

---

## 1x. 第二十轮：单一所有者收口（第三份 db 映射）+ 我第 12 轮重命名留下的文档漂移

### A. 【P2/P3 · 已修】数据面写路径的 db 错误映射：**第三份**，且已漂移 + 把原始 SQLite 文本回给租户
**证据**：`tenant_api/handlers.rs` 的 `CoreError::Db(d) => (500, "database_error", d.to_string())`（`:1007`），而唯一所有者 `admin/handlers.rs` 把 SQLite 的 5/517 映射成**可重试的 503 `storage_busy`**；写核心全程 `BEGIN IMMEDIATE`（超时正是 code 5）。⇒ 同一个操作在管理面回"稍后重试"、在**租户数据面**回"这是 bug"，并且把**原始 sqlite 文本**（内部实现细节）写进租户可见 body。而 `map_core_err` 的注释自称"The status / code are IDENTICAL to the internal face"。
**这也纠正我自己在 §1n A3 的说法**：当时那次"单一所有者"只把 `tenant_config_api.rs` 那一份改成委托，**没有**扫到数据面这一份 —— 于是"唯一所有者"实际是两份。这是"局部收口被当成全局收口"的典型，值得记下来。
**修**：`classify_db_err` 从 `pub(super)` 提到 `pub(crate)`；`map_core_err` 的 `CoreError::Db` 分支**改成调用它**，原始错误文本改为 `tracing::warn!`（日志里才是有用的地方），租户看到固定措辞"the storage layer could not complete the write"。注释改为"这一致性现在是由构造保证的，而不是靠声明"。
**租户可见的行为变化**（明确记录）：数据面写端点在锁竞争时现在可能回 **503 `storage_busy`**（此前一律 500）——方向是**可重试**、且与管理面一致；`tenant-api-integration.md` 的错误码表尚未列该码（批次 6 第 12 条）。
**测试缺口（如实记录）**：`map_core_err` 是私有函数，而构造 `sqlx::Error::Database` 需要真约束/真锁竞争（`tests/sqlite_write_lock.rs` 有现成形状：文件库 + 一条持写锁的连接）。本轮**没有**补这条端到端测试，已记为批次 6 的明确待办（含形状指引），不假装它已被覆盖。
**回归**：`tenant_api` 46/0、`sub_tenant_internal_write` 8/0、`--features server` 全量 **535/0/1**、`clippy -D warnings`（全目标）exit 0。
（附注：`sub_tenant_data_plane_write.rs` 在纯 `--features server` 下是 0 个测试——该文件 gate 在 `cluster-redis` 上，属预期，不是测试消失。）

### B. 【P3 · 已修】我第 12 轮重命名留下的三处文档漂移
`record_admin_auth_failure` 的标签已从 `denied|throttled` 改为 `<gate>_denied|<gate>_throttled`，但三处描述没跟上，其中两处是**给外部告警仓库看的接口说明**：
- `admin/metrics.rs:60` 的模块目录表（仍写 `result=denied|throttled`）；
- 同文件 `:463` 注册的 **help string**（会出现在 `/metrics` 输出里）；
- `ops.md:1186` 的环境变量表（`{result="denied"|"throttled"}` —— **照此写规则永远不匹配**）。
已全部改为 `<gate>_denied|<gate>_throttled`（gate = admin|cluster），并把 `ops.md` 的环境变量行补上"两个门禁共用一个按 peer 的预算"这一事实（此前只在 §9.1 的告警行里写了）。`ops.md:933`（第十一轮写的告警契约行）本来就是对的，未动。

### C. 【P3 · 已修】`.dockerignore` 的 `!bin/`、`!bin/hydra` 注释与事实不符
第 14 轮已实测这两行是**空操作**（没有任何规则排除 `bin/`，删掉后 `bin/hydra` 依然 included），但文件里的注释仍写着 `# KEEP the staged binary`，读起来像是它们在承重。注释已改成事实（防御性保留 + 实测结论 + 为什么仍留着：将来若有人加规则排除 `bin/`，镜像构建不会静默崩）。

### D. 批次 6 更新（第 1 条**降级为"需要先改抽象"**）
1. **L2 回填绕过 invalidation epoch 守卫**（`http.rs:244-255`）：撤销在途竞态可把已撤销的 allow 判决按 PTTL 剩余值写回 L1，且该路径 TTL 未经 `allow_ttl_max` 钳制。**本轮查明为什么还没做**：`AuthCache::l2` 是**具体类型** `Option<Arc<RedisAuthL2>>`（`http.rs:83`），不是 trait 对象 —— 因此**无法注入一个"在 `get()` 期间 bump epoch"的假 L2**来写确定性回归测试。要么先把 L2 抽象成 trait（可测），要么接受"只能靠真 Redis + 时序"的不可靠验证。**这是一个前置决策，已并入 D-9**。
2. db 映射的端到端测试（见 A）。
3. admission 的 queue-full TOCTOU（load→fetch_add 应为 `fetch_update` CAS）。
4. `create_sub_tenant` 的"幂等 upsert"在管理面 POST 上不可达（`ON CONFLICT … DO UPDATE` 是死代码）。
5. `MIN_CLUSTER_TOKEN_LEN` 无测试无文档，且门槛是长度不是熵。
6. drain 窗口 30s→4s 是未被承认的取舍（建议把上报前置）。
7. 我第 13 轮那条 `every_retry_sends_the_identical_batch_in_the_same_order` 承重不足。
8. `alt-features` 两步都没真正编译 `hydra` 二进制（D-7 相关）。
9. `live-deps` 的 `usage_query` 断言是空表对空表。

---

## 1y. 第二十一轮：`required-features` 挡住的不只是"一个组合"，而是**从未被编译过的二进制**

### A. 起因：复审指出 `alt-features` 的两步**根本没碰过 `hydra` 二进制**
复审（第十七轮第二路）发现：`alt-features` 里我写的 `cargo build --workspace --features hydra-server/tls-openssl` **不构建 `hydra` bin**，因为 `[[bin]] hydra` 的 `required-features = ["server"]`，而 `tls-openssl` 不含 `server` ⇒ `cargo` **静默跳过**该目标，那一步只链接了 lib。同理 proxy-only 那步只 lint 了 lib+test。**后果**：`main.rs` 里三处 `#[cfg(not(any(feature = "tls-boringssl", feature = "tls-openssl")))]` 分支（`main.rs:409/1190/1308` 附近的 `let cert_store: Option<()> = None;`、`let tls_bound: Option<String> = None;`）**在 CI 里从未被编译过** —— 而那正是"`proxy` 必须能独立构建"这句话要检的东西。

### B. 本轮做了什么：**先做实验，再改结论**
把 `required-features` 从 `["server"]` 松到 `["proxy"]`，然后逐条实测：
| 命令 | 结果 |
|---|---|
| `cargo check -p hydra-server --features hydra-server/proxy --bin hydra` | **exit 0** —— 二进制**确实**能在无 TLS 后端下编译（此前无法验证，因为 target 被跳过） |
| `cargo clippy -p hydra-server --features hydra-server/proxy --all-targets -- -D warnings` | **exit 0**（`--all-targets` 现在含该 bin） |
| `cargo check -p hydra-server --features hydra-server/tls-openssl --bin hydra` | **exit 0** |
| `cargo build --workspace --features hydra-server/tls-openssl` | Finished（**现在真的会构建并链接该二进制**） |
| `cargo check -p hydra-server --bin hydra`（无特性） | 仍然拒绝/跳过：`target 'hydra' requires the features: 'proxy'` —— **默认构建行为未变** |
| 标准门禁 `clippy --features hydra-server/server -D warnings` + `cargo test -p hydra-core` | exit 0 / **249/0**（生产组合不受影响） |

**处置**：保留松绑，因为它在四个方向上都更好：①文档承诺的"独立构建"从**没被检查过**变成**被检查**（CI 里 proxy-only 的 `--all-targets` 现在包含 bin，另加了一条**显式**的 `cargo check --bin hydra` 步，因为 `--bin` 在 required-features 未满足时会**报错**而不是静默跳过 —— 这条步是正向证明）；②`alt-features` 的"linking, not just lints"那句注释**终于成立**；③三处 TLS-less 分支进入编译覆盖；④默认（无特性）构建行为**逐字不变**。
`Cargo.toml` 与 `ci.yml` 的注释都改成事实，并把上面那几条实测命令写进 manifest 注释作为依据。

### C. 与 D-7 的关系
D-7（`db` 是否拥有副本 restore 路径）讨论的是 **W2/W3 裸切片**（`db`、`http-client` 单独）——**那些仍然编译不过**，§1r 的诊断（两条耦合边、精确站点）依然成立。本轮解决的是**另一个**组合：`proxy`（含无 TLS 后端）与 `tls-openssl` —— 也就是 macOS 开发者躲 BoringSSL 实际会用的那两个。所以："那条逃逸通道现在**真的**能用了，而且**被 CI 盯着**"，这句话从"部分为真"变成了"逐字为真"。

### D. 本轮验证小结
`clippy(server) -D warnings` exit 0；core **249/0**；上面表格里五条命令逐字跑过。**未重跑**可选特性套件与 UI/活依赖套件（上轮绿：660/2/3）。

---

## 1z. 第二十二轮：让两条"证据"不再是空的

### A. 【P2 · 已修】CI 里那条 ClickHouse 聚合端到端断言是**空表对空表**，恒真
复审指出（第十七轮第二路）：`live-deps` 作业只执行 `init.sql` **建表、不插数据**，而 `usage_query.rs::live_clickhouse_aggregate_matches_a_hand_run_query` 断言的是"读到的聚合 == 手工跑的同一查询"。空表上两边都是 0 ⇒ **比较 0 与 0，测不出任何聚合错误**。本机之所以看着正常，只是因为本机 CH 里**有数据** —— 又一次"不同输入形状"。
**修**：在比较之前断言 fixture **非空**（`expected.0 > 0`），并写明"否则这个测试是空的"。这样 CI 里一旦表是空的就**大声失败**，而不是给出一条看起来很强的绿。本机实测仍过（本机有数据）。

### B. 【P3 · 已修】集群 token 的强度下限：抽成纯函数 + 补测试 + 补文档
第 12 轮给它加了 16 字符下限（此前只校验"存在"），但（复审指出）**没有测试、`ops.md` 也没写**，而且注释写着 "must both be unguessable" —— 而门槛是**长度不是熵**（`"aaaaaaaaaaaaaaaa"` 照样通过）。
**修**：把内联检查抽成纯函数 `validate_cluster_token(&str) -> Result<(), String>`（放在其它纯解析器旁边），启动点改为 `if let Err(e) = validate_cluster_token(..) { return Err(e.into()) }`（保持与其它启动拒绝**完全相同的错误类型转换**）；新增 `#[cfg(test)] mod cluster_token_tests` 覆盖：恰好 16 通过、64 通过、空串/1 字符/15 字符被拒，且错误信息**必须**同时包含变量名与 `openssl rand -hex 32`（可执行建议）。`ops.md` 的环境变量行补上"**至少 16 字符且必须随机**"，并写明"长度是下限、不是保证；启动检查分不出 `aaaaaaaaaaaaaaaa` 和真 token，而这把令牌授权的是内部控制面与跨租户写端点"。
**反向证伪**：把下限判断改成恒假（即修复前的"只看存在"）⇒ 测试 FAILED（`must be refused: ()`）。
**过程记录（两次自伤，都被工具抓住）**：①测试第一次被插进了 `#[cfg(all(test, feature = "cluster-redis"))]` 的模块里 ⇒ 在 `--features server` 下**根本不编译**，`cargo test --bin hydra` 只跑了另外 2 条（文件里另一处注释恰好警告过这个坑："Deliberately NOT inside the `cluster-redis` test module … that module does not compile under the `--features server` gate"）；移到独立的不带 gate 的模块后才真正执行。②搬移时我用 `s.index("    }")` 取函数结尾，匹配到了 `        }` 行**内部**的四个空格 ⇒ 切掉了两个闭合花括号、还把一个文档注释挂到了错误的模块上；靠编译器报的 unclosed delimiter 修复，并把 C3 的文档注释放回 `non_route_strategy_tests`。**教训与第十九轮同型：对源码做文本手术必须用结构化边界，而不是子串搜索。**

### C. 本轮验证
`clippy -D warnings`（server 全目标）exit 0；core **249/0**；`--features server` **536/0/1**（535 + 新增的集群 token 测试）；`cargo test --bin hydra` 3/0（新测试确实在跑）；活 CH 的 `live_clickhouse_aggregate_matches_a_hand_run_query` 仍过（且现在带非空断言）。**未重跑**可选特性套件与 UI 套件（上轮绿：660/2/3）。

---

## 2a. 第二十三轮：`max_queue_depth` 从"提示"变成真正的界

### A. 【P3 · 已修】admission 的 queue-full 是 **load → fetch_add 的 TOCTOU**
**证据**：`acquire` 里先 `gate.queue_depth.load(Ordering::Acquire)` 判断是否满（`admission.rs:311-320`），随后 `WaitGuard::new` 无条件 `fetch_add(1, Ordering::AcqRel)`（`:152-153`）——**检查与占位不是一步**。
**后果**：同时到达的一批请求读到同一个 depth 后会**一起**通过检查：`max_queue_depth = 1`（或 `0`，语义是"不排队"）时真实 waiter 数可达该批的并发数，于是 `hydra_queue_depth` 与据此算出的 `Retry-After` 描述的是一个**门禁从未执行过的界**。
**修**：把检查与占位合成一步 —— 新增 `WaitGuard::try_reserve(queue_depth, provider_id, max_depth)`，用 `compare_exchange_weak` 循环（失败即用最新值重试），返回 `None` 即满。`max_depth` 取 `if max_queue_depth == 0 { 1 } else { max_queue_depth }`，**逐字保留原来的可接受集合**（旧检查在 depth==0 时允许自增 ⇒ "不排队"下最多一个 waiter），所以这不是语义变更，只是把同一个集合变成原子的。`WaitGuard` 仍在 drop 时自减，depth 仍是"当前 waiter 数"。

### B. 测试与**诚实记录的证伪结果**
`tests/admission.rs::the_queue_depth_bound_holds_under_a_simultaneous_burst`：32 个任务过一道 barrier 同时进入；**全程持有唯一 permit**（`max_concurrency = 1`），因此凡是通过检查的请求必然真的排队、并在 100 ms 有界等待后以 `WaitTimeout` 返回。断言的是**精确值**而非时间窗：`admitted (Ok | WaitTimeout) == 1`、其余 31 个必须是 `QueueFull`。

**反向证伪的结果必须如实写**：把 `try_reserve` 换回"`load()` 检查 + `fetch_add`"（即修复前的形状）后连跑 3 次 —— **1 次 FAILED**（`admitted=2, full=30`）、**2 次 PASSED**。也就是说：这个竞态是**间歇性**的，本测试是旧 bug 的**概率性探测器**（约 1/3），而不是确定性证伪。
**但作为 CI 守卫它是可靠的**：修复版连跑 6 次全绿（该文件 10/10，0.19s）——CAS 保证 `depth <= max` 由构造成立，所以它在修复后**不可能**因真实原因变红。这正是"稳定绿 + 真实不变量"该有的样子；我在报告里不会再声称"反向证伪 ⇒ FAIL"。

### C. 本轮验证
`clippy -D warnings`（server 全目标）exit 0；`tests/admission.rs` 10/0 ×6 次。**未重跑**其它套件（上轮绿：core 249/0、server 536/0/1、可选 660/2/3）。

---

## 2b. 第二十四轮：两处"承诺了做不到的事"的注释（都是 docs↔fact 那一类）

### A. 【P3 · 已修】`create_sub_tenant` 自称的"按 `(tenant_id, name)` 幂等 upsert"
**原文**（`sub_tenant_write.rs` 的函数文档）："Create a sub-tenant (idempotent upsert by natural key `(tenant_id, name)`, D4) … on a name conflict the existing row's id is retained, so a retry after a leader failover converges to the same row (A-2 prerequisite 4)."
**事实**：这个保证**属于另一条入口** —— 租户面 `PUT /tenant/{tid}/api/v1/sub-tenants/{name}`（`tenant_config_api.rs:213-231`，我逐行核过：先按自然键查出既有行，再 `update_sub_tenant(..., &st.id, ...)` 收敛）。而 `create_sub_tenant` 是**创建**路径，两个分支都把 `self_id` 传 `None`（`:173`、`:188`），于是**顺序重复**的创建（重试、幂等重放、重复提交）会先撞校验器的 `NameDuplicate` ⇒ **409**，不会收敛。
**修**：把函数文档改成"哪个入口给哪个保证"，并写明 `ON CONFLICT … DO UPDATE` 仍然可达、但**只**对"两个并发创建都没看到既有行"这种竞态有效——它**不**让顺序重复收敛。
**收尾时我自己又差点写下新的假话**：第一版写的是"never reaches the ON CONFLICT"，逐行核对并发路径后改成上面这句（并发竞态确实会走到它）。**这正是本轮的意义：改文档时同样要逐行核实，否则只是把一处假话换成另一处。**
**未动代码**：管理面创建是否也应该收敛（409 → 收敛）是对外语义变更，记为 **D-10**。

### B. 【P3 · 已修】我第 13 轮那条 sink 测试的 doc 声称了一个它做不到的证伪
测试 `every_retry_sends_the_identical_batch_in_the_same_order` 只把 `Vec` 喂给 `flush_with_backoff`，而生产中唯一能破坏该不变量的地方是 `buffer.extend(returned)`；那条路径**从这里不可达**（flush 在 await 期间没人能并发 push，也就没有第二个来源可以打乱顺序）。原 doc 却写着"Falsification: make the retry re-take the batch after pushing anything into `buffer`" —— 那是**测试自己做不到的操作**。
**修**：doc 改成实话——它钉住的是**当前实现**交付同一批同一顺序这一不变量（单 token 去重的前提），而真正能对该回归变红的测试必须驱动 `run_channel_sink` 并在重试期间并发 `tx.send`；那条已记为批次 6，不在这里冒领。

### C. 本轮验证
`clippy -D warnings`（server 全目标）exit 0（改完措辞后再跑一次）；`--lib sink::` 5/0（4.00s，行为未变——本轮只改注释与文档）。**未重跑**其它套件（上轮绿：core 249/0、server 536/0/1、可选 660/2/3）。

---

## 2c. 第二十五轮：把"丢失的用量一定会被报出来"变成**被断言的属性**（并补上风险前置告警）

### A. 【P3 · 已补】第 13 轮那条修复（关闭丢量不再静默）**从来没有测试过"上报"本身**
第 13 轮我修的是：最终 drain 的 30 s 窗口长于 5 s 关闭预算 ⇒ 任务被截断 ⇒ `note_usage_drop("shutdown_unflushed")` 与 `error!` **永不执行**。当时补的测试 `the_final_drain_finishes_inside_the_shutdown_budget` **只断言时长**。也就是说：**任何"保住了时长、丢掉了上报"的回归都不会被发现** —— 而那恰恰是这次修复的全部意义。复审（第十七轮第一路）也点出过这条。
**修**：
- `admin/metrics.rs` 新增 `usage_dropped_total(reason)`（沿用既有 getter 模式），让"丢弃计数"成为**可断言**的东西；
- 新增 `sink.rs::a_failed_final_drain_is_reported_not_silent`：恒失败的 inserter + 两条缓冲记录 + 关通道，等任务结束后断言 `usage_dropped_total("shutdown_unflushed")` **严格增加**（该计数是进程全局的，且姊妹测试也会让一次最终 drain 失败，所以断言"增加"而不是精确增量）。
**反向证伪**：删掉 `None` 分支里的 `note_usage_drop(..)` ⇒ FAIL，报 `before=0, after=0`。

### B. 【P3 · 已加】最终 drain 之前先**声明风险**
复审建议：计数器只能在 drain **失败之后**才敢加（没落地就计数等于谎报），所以给"中途被杀"这种情形留一条痕迹的正确做法是**日志前置**。已在最后一次 drain 前加一条 `warn!`（`at_risk = buffer.len()`、`window_ms`），并注明**为什么这里刻意用日志而不是计数**。

### C. 本轮验证
`clippy -D warnings`（server 全目标）exit 0；`--lib sink::` **6/0**（新增 1 条，4.00s）。反向证伪的两次运行都如实记录在上面。**未重跑**其它套件（上轮绿：core 249/0、server 536/0/1、可选 660/2/3）。

### D. 批次 6 余项
db 映射的端到端测试（形状已指明：文件库 + 持写锁的连接，经 `map_core_err`）｜那条真正能对"重试期间并发 push"变红的 sink 测试（需驱动 `run_channel_sink` 并在重试中 `tx.send`）。到此为止批次 6 的**可判定**项只剩这两条，其余都是决策项（D-1…D-10）。

---

## 2d. 第二十六轮：补上第二十轮留下的那条测试债（真实 SQLITE_BUSY → 503）

第二十轮我把数据面写路径的 db 映射改成委托唯一所有者，**但没补测试**（当时登记为批次 6，并写明了难点：`map_core_err` 私有 + 需要真锁竞争构造 `sqlx::Error::Database`）。本轮把这条债还了，而且是用**真的** SQLITE_BUSY，不是构造出来的假错误。

**测试**（`tenant_api/handlers.rs` 新增 `#[cfg(test)] mod tests`）：
1. 建**文件库**（`:memory:` 池被固定在单连接上，造不出第二个连接；`db::init_pool` 对文件库给 8 个连接 —— 这点我核对过）；
2. 一条连接 `db::begin_write` 拿到**写锁**并写入；
3. 从**池**里发一条写 ⇒ 在 `busy_timeout`（5 s，`db.rs:57`）后真的拿到 SQLITE_BUSY；
4. 把它喂给 `map_core_err(&CoreError::Db(err))`，断言 `(503, "storage_busy")`，**并且**租户可见的消息里不含 `sqlite` / `database is locked`（后半条针对的是"把原始 SQLite 文本回给租户"这个副作用）。
**反向证伪**：把第三份映射（`(500, "database_error", d.to_string())`）放回去 ⇒ **FAILED**，实测 `left: (500, "database_error")` vs `right: (503, "storage_busy")` —— 两条断言都命中。
实测 5.02s（就是 busy_timeout 的长度）。

**本轮验证**：`clippy -D warnings`（server 全目标）exit 0；该测试 1/0。**未重跑**其它套件（上轮绿：core 249/0、server 536/0/1、可选 660/2/3）。

**批次 6 现在只剩 1 条可判定项**：那条真正能对"重试期间并发 push"变红的 sink 测试（需驱动 `run_channel_sink` 并在重试中 `tx.send`）—— 其余全部是决策项（D-1…D-10）。

---

## 2e. 第二十七轮：批次 6 最后一条可判定项（"重试期间到达的记录"由**真实驱动**验证）

### A. 为什么需要它
第 24 轮我承认了我第 13 轮那条测试**承重不足**：它只把 `Vec` 喂给 `flush_with_backoff`，而生产中唯一能破坏"每次重试内容相同"这条不变量的地方是 `buffer.extend(returned)` —— 那条路径从该测试**不可达**。本轮补上真正驱动 `run_channel_sink` 的版本。

### B. 测试
`sink.rs::a_record_arriving_during_a_retry_joins_the_next_batch_not_this_one`：
- `batch_size = 2`（小时级 tick，所以只有"攒够 2 条"会触发 flush）；前两条记录触发一次 flush，inserter **故意失败两次**，于是重试进行中；
- 在重试**期间**从测试侧 `tx.send(rec("late"))`（通道容量 8，不会阻塞）；
- 断言：①首批必须是逐字的 `["a","b"]`；②**任何**一次尝试都不得同时含 `late` 与 `a`/`b`（混批就是"内容变了 ⇒ 去重 token 失效 ⇒ 重复计费"）；③`late` 仍必须被落到后续批次（**不丢**）。
实测 0.15s 通过。

### C. 诚实说明：这不是"反向证伪"，而是**断言活性探针**
这类 bug 的成因是一个**设计变更**（共享缓冲/并发 flush），把某一行改回去**造不出来**。所以我没有声称"反向证伪 ⇒ FAIL"，而是做了两件事：
1. 测试在真实驱动下通过（0.15s）；
2. **活性探针**（一次性、未保留）：让 inserter 在失败时把一条 `late` 追加进退回的批次 —— 即人为制造那条被禁止的条件 —— 断言**立刻命中**，打印出 `[["a","b"], ["a","b","late"], ["a","b","late","late"], ["late"]]`。这证明这三条断言不是空的，而是真的在检查。
**边界**：它证明"断言是活的"，不证明"存在某个已知的实现会把它们弄红"；后者需要有人真的把 flush 改成共享缓冲（那种改动会被这条测试挡住，这正是它的用途）。

### D. 本轮验证
`clippy -D warnings`（server 全目标）exit 0；`--lib sink::` **7/0**（新增 1 条，4.00s）。**未重跑**其它套件（上轮绿：core 249/0、server 536/0/1、可选 660/2/3）。

### E. 批次 6 到此**可判定项已清空**
从第一轮至今一直在"找可判定的事做"，现在这个池子见底了：剩下的**全部是 D-1…D-10**（改对外语义、安全取舍、仓库策略或抽象）。下一轮若仍无取舍，能做的只有"对新近改动再做一轮对抗性复审"（历史上每轮都产出真问题）与更细的守卫；**实质推进需要至少一项决策**，其中 **D-8（P1）**最紧：官方集群拓扑下 edge 的 `/metrics` 恒 401，数据面节点**完全没有指标**，而 `ops.md` §9.1 的告警契约假定它们存在。

---

## 2f. 第二十八轮：全量门禁复核 + 对两处**从未被对抗性复审过**的面派出复审

### A. 全量门禁复核（13 条，按 section 逐段求和）
| 门禁 | 退出码 | 计数 |
|---|---|---|
| fmt / clippy×4（server、optional、tls-openssl、proxy-only） | 全部 **0** | |
| `test hydra-core` | 0 | **249 / 0** |
| `test hydra-server --features server` | 0 | **540 / 0 / 1 ignored**（第十九~二十七轮新增的 4 条测试都在其中） |
| 可选 + 活 Redis/CH（`--no-fail-fast`） | 101 | **668 / 2 / 3** —— 2 例仍是 **D-2**，与前几轮完全一致 |
| ignored: listener limitation / clickhouse e2e | 0 / 0 | 1 / 0 与 2 / 0 |
| compose config / findings-disposition / i18n | 0 | 3 个 compose VALID、27/27 |

也就是说：**除了 D-2 那两例，整棵树是绿的**，而且每一条都有退出码而不是靠肉眼。

### B. 派出的两路复审（因为可判定池子见底，而"复审新面"历史上每轮都产出真问题）
1. **启动与编排**：`main.rs`（角色解析、各角色启动期校验与"静默容忍"、监听接线、TLS/明文监听、sink 构建、`HYDRA_RESEAL_SECRETS` 一次性模式、信号与关闭、生产里的每一处 `expect`/`unwrap`，以及每个 `#[cfg(feature)]` 分支是否真的能被 `environment/` 里文档化的部署走到）、`listeners.rs`（启动规划器与 `probe_bind`、活跃监听 gauge、计划与实际绑定是否会不一致）、`tls.rs`（SNI 证书存储与回调、通配匹配、小写键、默认证书回退）、以及三个 compose + Dockerfile 与 `main.rs` 读的 env/端口是否一致。**要求**：每条发现给 `path:line` + 引用、说明后果方向（拒绝启动/起错角色/绑错地址/无鉴权监听/静默跳过文档化功能），并区分"仅凭代码可判"与"需产品决策"。
2. **代理请求管道整体**（`proxy.rs` + `ctx.rs`/`provider_client.rs`/`limiter.rs`）：准入与路由、failover 循环（一个客户端请求最多能打到上游几次？设计文档的说法是否仍然为真）、鉴权与缓存交互（是否存在"无判决也放行"或"用陈旧判决"的路径）、上游交换（请求构造/凭据替换/首字节界/流式体界/非 2xx）、**记账（logging 阶段）是否恰好一次**（成功、失败、failover、截断流、客户端断连各自记什么，记录的 tenant/sub-tenant/provider/model/status/tokens 与客户端的实际遭遇是否一致）、指标能否重复/漏记/打错标签、限流窗口与上游尝试的相对顺序（是否可以"被放行却超限而无窗口记录"）。**要求**同上，并要求指出方向（多计费/少计费/归错租户/泄漏/可用性损失）。

两路的结果会在后续轮次到达；**本轮不预判它们的结论**。之所以选这两块：它们是整个仓库里体量最大、且二十七轮下来**从未被任何一路对抗性复审覆盖过**的面（前面几路覆盖的是鉴权面、持久化/集群、核心逻辑、文档、CI、测试、以及 limiter/admission/registry/store/usage_query/clickhouse/sub_tenant_write/http）。

### C. 结论：可判定项已清空，实质推进需要决策
批次 0/1/2 早已完成；批次 3–6（三轮复审的产物）里所有可判定项都已修复并各自带验证（详见 §1l–§2e）。**剩下的全部是 D-1…D-10**。最紧的是 **D-8（P1）**：官方集群拓扑下 edge 的 `/metrics` 恒 401、数据面节点完全没有指标，而 `ops.md` §9.1 的告警契约假定它们存在。

---

## 2g. 第三十轮：两项审计（我的测试是否真在跑；我的测试产物是否污染了工作树）

本轮没有修复项——**因为可判定池子已在 §2e 清空**，而两路复审仍在运行。所以做的是两件"验证型"工作，它们针对的都是**本会话已经真实踩过两次的失效模式**。

### A. 我本会话新增的 17 条测试**是否真的在跑**？
本会话里我两次栽在"测试存在但没在跑"上：第 10/11 轮的 `#[test]` 被手术断开（两个既有测试静默停止运行），第 22 轮的集群 token 测试被写进 `#[cfg(all(test, feature = "cluster-redis"))]` 模块 ⇒ 在 `--features server` 下**根本不编译**。所以做了一次全量点名审计：跑可选特性全套（活 Redis + 活 CH，`--no-fail-fast`，715 行 `test` 输出）并逐条点名核对本会话新增的 17 条 —— **全部出现**：
`bad_cluster_tokens_are_throttled_and_counted`、`the_leader_adopts_the_relayed_trace_id…`、`a_refused_write_is_attributed_to_its_route_not_to_unrouted`、`the_sub_tenant_usage_index_exists`、`no_two_test_files_share_a_redis_database`、`no_function_claims_two_test_attributes`、`the_final_drain_finishes_inside_the_shutdown_budget`、`a_failed_final_drain_is_reported_not_silent`、`a_record_arriving_during_a_retry_joins_the_next_batch_not_this_one`、`the_queue_depth_bound_holds_under_a_simultaneous_burst`、`a_busy_write_is_a_retryable_503_without_internals_in_the_message`、`an_https_clickhouse_url_fails_at_build_time`、`the_cluster_token_floor_is_enforced_with_actionable_advice`、`a_stalled_auth_body_does_not_hold_the_request_forever`、`an_https_url_is_refused_rather_than_sent_in_plaintext`、`one_batch_keeps_its_dedup_token_across_retries`、`every_retry_sends_the_identical_batch_in_the_same_order`。
其中 `clickhouse_sink_writes_batch` 与 `same_port_on_different_addresses_is_not_detected_statically` 是 `#[ignore]` 的，在这里显示为 ignored —— 它们由门禁里那两条专用 `--ignored` 步骤执行（此前已各自单独验证通过）。**结论：本会话新增的测试没有一条是"写了但没跑"的。**

### B. 我的测试产物有没有污染工作树？
逐个核对本机跑过 Go/TS/Python/node 之后留下的目录：`tools/hydra-ts/node_modules`、`tools/hydra-ts/dist-test`、`tools/hydra-py/tests/__pycache__` **三者都存在且都被 gitignore 正确忽略**。`git status` 的 8 个 `??` 全部是**本会话的新交付物**（7 个新测试/迁移文件 + 本计划文档），没有垃圾。**结论：工作树是干净的**（101 项改动/未跟踪全部有归属）。

### C. 状态
两路复审（启动/编排/监听；代理请求管道整体）仍在运行，结果到达后并入下一轮。**可判定项已清空**；实质推进需要 D-1…D-10 至少一项的取向，最紧的是 **D-8（P1）**。

---

## 2h. 第三十一轮：核对一处**从未被任何复审碰过**的用户面文档（`docs/index.html`）

前面所有文档核对（第三轮、第十一轮、第十七轮）覆盖的是 `dev-docs/**`、`README*.md`、`HANDOFF.md`、`admin-ui/api-docs.js`。本轮清点剩余文档面时发现还有一个**受版本控制、面向用户**的 52 KB 页面从未被核对：`docs/index.html`（配 `docs/.nojekyll`，显然是 GitHub Pages 的落地页）。

**核对结果（逐条对照代码）**：
| 页面上的具体声明 | 代码 | 结论 |
|---|---|---|
| "一个二进制，两个端口：8080 代理 / 8081 管理" | `environment/Dockerfile` 的 `EXPOSE 8080 8081 8443`；明文数据面 8080、管理面 8081 | **一致** |
| `HYDRA_ROLE=edge` | `cluster/mod.rs:45` "Parse `HYDRA_ROLE` (default `all`)" | **一致** |
| `HYDRA_REDIS_URL=redis://…` | `main.rs:219` 读它 | **一致** |
| `HYDRA_CLUSTER_TOKEN=…` | `main.rs` 读它；第 12 轮起还有 16 字符下限（页面只说"=…"，未承诺强度，不算错） | **一致** |
| 示例请求路径 `/v1/chat/completions`、`/v1/messages` | 分别是 OpenAI 兼容路径与 Anthropic 路径 | **一致** |

页面其余内容是**模拟终端输出**（`[edge] bootstrap → snapshot v42 · routes synced · serving :8080` 之类）与营销文案，不构成可核对的事实声明；我**不**把它记作"已核对无误"的整体，只记"它的具体声明（端口/env/路径）全部与代码一致，没发现漂移"。这也是本仓库文档面里最后一处未核对的具体声明集。

### 状态
两路复审（启动/编排/监听；代理请求管道整体）仍**未返回**。可判定池子仍为空（§2e），实质推进需要 D-1…D-10 至少一项取向；最紧的是 **D-8（P1）**。

---

## 2i. 第三十一轮下半 · 请求管线整体复审（第八路 Oracle）：**2 条 P1、5 条 P2、5 条 P3**

这一路审的是 `proxy.rs` 的**端到端请求生命周期**（此前没人把它当作一个整体看过）。结论：**成功路径是对的**（记账恰好一次、归因到真正出正文的那次尝试、permit 与窗口不错配——见下面"核对为正常"），但**边缘有三个真正承重的洞**。

### P1-1 `matching_provider` 维度的限流角色**整体失效**（供应商维度的窗口"写进去、从不读"）
- 前置检查的 `MatchCtx.provider` **恒为 `None`**（`proxy.rs:831-847`，注释自陈"provider is unknown until routing"），而 `core/limit.rs:55-59` 的 `dim_matches` 对"角色声明了具体 provider、ctx 是 None"判**不匹配** ⇒ 该角色根本不进 `match_roles`，count 与 token 两类检查**全被跳过**；
- 记账侧却带着 provider 写窗口（`proxy.rs:1435-1445` 的 `provider: Some(&sel.provider_id)` + `add_tokens`）⇒ 单向窗口；
- 该维度**可以被配置并落库**（`migrations/0001_init.sql:76`、`db.rs:1289/1346`、`admin/handlers.rs:1438` 的 `POST /limit-roles` **无任何校验**），全仓**没有**任何非 `None` 的 `matching_provider` 测试用法。
- **方向：免费额度（该限流永不生效），且静默**（无警告、无校验、无指标）。修法二选一并显式化：①让检查也能表达该维度（候选集合已知后逐候选构造带 provider 的 `MatchCtx` 再检查）；②若产品上不支持，`config::validate` 对该字段报错/Warn 且 admin 写入口拒绝。
- **已修复（2026-10-09，D-11 决策落定为修法②）**：`config::validate` 对存量行保留具名 Warn（`crates/hydra-core/src/config.rs:356-362`），同时 admin 写入口（POST/PUT `/api/v1/limit-roles`）把**非 NULL `matching_provider` 直接 400 `matching_provider_cannot_match`**（`crates/hydra-server/src/admin/handlers.rs` 新增 `matching_provider_write_error`，先于任何 DB 写）。面向用户的配置表面随之移除：admin-ui 表单字段 + 表格列（`admin-ui/app.js`）、CLI `--matching-provider` 选项（`tools/hydra-cli`）均删除，i18n dead keys 清理。测试：新增 `tests/admin_api.rs::limit_role_write_rejects_the_inert_provider_dimension`（POST→400 不落库、PUT→400 原行不变、干净写入→201/200）；`integration/test_limit_roles_enforcement.py` L6 从「写入 + 断言永不触发 + Warn」改为「POST→400 + GET→404 未落库」。仍接受该维度的**非管理写入路径**（DB restore 恢复 legacy 备份、集群复制同步 legacy 行、文件加载配置）由保留的 Warn 兜底。

### P1-2 **全部候选失败**（以及鉴权拒绝、路由失败）的请求在指标与账务里**归零**
- `logging` 的两条记录分支都以 `ctx.selected` 为前提（`proxy.rs:1321-1323`、`:1371-1372`），而 `selected` 只在 **2xx** 分支写入（`:1063-1067`）⇒ 上游集体 5xx/4xx/429、超时、容量不足、路由失败这些"客户端确实收到了答复"的请求**不产生任何 `hydra_requests_total` 样本**，`status=5xx` 这条曲线**恒为 0**。
- 代码与两处文档直接矛盾：`design.md:1466` 写该指标是"请求总数（**含失败**）"，`metrics.rs:545` 写 "one per proxied request, **including fails**"。
- **方向：计数为零次 ⇒ 告警盲区**（按 `status=~"5.."` 写的规则永不触发）。修法必须保持"租户 API 流量不进这一族"的既有区分（`design-tenant-api.md:961` 的自指防护）——具体往哪个指标族/标签写是**产品决策**。
- **已修复（2026-10-09）**：`logging` 改为两层守卫——外层 `ctx.tenant_api_endpoint.is_none()` 排除租户 API 控制面请求（自指防护保持，`tenant_api` 设 `ctx.tenant_api_endpoint` 标记自己、不依赖 `selected`），内层 `ctx.tenant.is_some()` 才计数；失败路径（全候选失败/路由失败/超时/容量不足）现在也产生一条 `hydra_requests_total` 样本，`provider` 标签在未选中任何 provider 时为空串 `""`（标签基数不变），`status=5xx` 不再恒 0。`design.md`/`metrics.rs` 的"含失败"表述自此与代码一致。新增集成测试 `tests/metrics.rs::all_candidates_failed_produces_empty_provider_sample`。残留观察项：`hydra_requests_total` 是否被外部按营收口径消费（原始修法选择的依据）不变。

### P2-1 非 2xx 时**丢弃上游错误体**且不消费响应（连接无法回池）
`proxy.rs:1119-1146` 的非 2xx 分支既不读也不转发 `resp`，客户端只拿到 Hydra 拼的通用 JSON；注释却自称 "forwarded verbatim when they carry a useful code"。**方向：可用性/可诊断性**（上游的 `x-request-id`、"model not found"、"context length exceeded" 全被替换成 `all_proxies_failed`），并让错误风暴期的连接无法复用（机制推断，未实跑）。修法：有界（如 64 KiB）读取上游 body 并在后置分支作为 detail 回传，顺带让连接可回池。

### P2-2 mid-stream 截断**伪装成 200 成功**（用量与客户端所见不一致）
空闲上限触发截断时客户端已收到部分 SSE，而 `ctx.status_code` 早在响应头阶段就定为 200（`:1062`），`logging` 收到的 `error` 是 `None`（`stream_response` 的错误在 `:1092-1113` 被吞掉后 `return Ok(true)`）⇒ 记录看起来是一次成功、token 只记到截断处。**方向：少计**（上游很可能已按完整回复计费），对账差额无法归因到任何一行。修法：在 ctx 上置截断标记并由 `logging` 写进 `error`（或新字段）与指标——`status_code` 的语义是产品决策。

### P2-3 拿到 count 名额后失败/被拒的请求**已消耗名额但无用量记录**
count 窗口在**路由与上游尝试之前**递增（`proxy.rs:837-847`，`check_and_inc` 只在允许时 push），而用量记录只在 2xx 后产生 ⇒ 限流 429、`no_model_field`、校验 412、路由 404/503、全候选失败**都永久吃掉一个名额却没有记录**。**方向：少计 + 客户侧可用性损失**（"没转发成功为什么配额没了"无法解释）。注：反方向是正确的——被拒的 429 不占窗口。修法需**产品决策**（把 count 语义显式化为"准入尝试"或"已转发到上游"）。

### P2-4 `max_concurrency` 的热加载**是假的**
`AdmissionControl::get_or_create_gate` 首次创建后**不重建信号量**（`admission.rs:415-418`，注释自陈 "the first policy wins"），于是改小/改大 `max_concurrency` **完全无效**；更糟的是 `max_queue_depth` 取**新** policy、信号量取**旧** policy，同一闸门混用两个来源。而 `design-admission-queue.md:197` 承诺 "hot-reloaded via `ArcSwap`"。**方向：运维谎言 + 可用性损失**（"降容量保护弱上游"不生效且无告警）。修法：版本化重建闸门（在途 permit 持旧闩锁自然排空）+ 一条变更计数指标，或删掉文档承诺。

### P2-5 `retry_after_connect` 会重开被文档判定为"已废弃"的重复计费失效转移
唯一决定"发送后是否换候选"的条件只用了该字段（`proxy.rs:1180`），而 `design.md:92/812-813` 两处声称它在 terminate-mode 已废弃、§8.3 承诺的三重闸（+ `upstream_bytes_seen` + `body_replayable`）在代码里**只剩第一个**。今天外部无法把它设为 true（`main.rs` 没有 env 解析），所以现实风险受限，但**一旦有人按文档接线**，`true` 的语义正是文档自己警告的**双份上游账单**。修法：删字段并恒不重放（与文档一致），或重新引入第二道闸并改掉文档的错误声明。

### P3（5 条）
1. 非 2xx 不消费响应 ⇒ 连接池退化（与 P2-1 同源）。
2. `hydra_retries_total` **少计**：计数器只在循环体末尾（`proxy.rs:1199-1205`），而 `:948-970` 的 5 条 `continue` 早退路径与 `:1188` 的 502 早退都不计；注释却自称 "for every candidate we fall through from"。
3. **`ttft_ms`/`forward_latency_ms` 用 `unwrap_or(0)` 冒充 0**（`proxy.rs:1398-1399`），而同一记录的 token 字段刻意保留 `None`（"must not masquerade as a zero count"）——同一条记录两种口径；非流式 2xx 会把"没有 TTFT"记成 `ttft_ms=0`。修法：直接透传 `Option`。
4. 主动跳过候选时不打 `on_failure`（**这是对的**），但也没有任何"跳过"指标 ⇒ "某 provider 的 key 被删空 ⇒ 静默退化为死权重"只在 `warn!` 里出现一次。
5. **陈旧注释/文档**（会误导下一轮）：`provider_client.rs:18-19`、`:219-221` 仍称"client 级 300 s 总超时覆盖整个交换"，而该超时**已在第 2 轮删除**；`proxy.rs:1159-1161` 称 `retry_after_connect` "was declared and documented but read by nothing"（现在被读了）；`proxy.rs:1200` 的 "for every candidate" 与实测不符。

### 核对为**正常**的（20 条，判据见复审原文）
`logging` 一定被调用且**恰好一次**（核过 pingora 0.8.1 的三条出口）；`short_circuit` 的状态码确实落到 `logging`；**失败转移不会给同一 provider 记两次用量**（`ctx.selected` 每轮成功即覆盖写并立即返回，记录里的 provider/endpoint/api-key 与真正送正文的那次严格一致）；用量 sink 调用只有一处且以 `selected.is_some()` 为条件；admission permit 无泄漏（RAII + 第 23 轮的 CAS 保证 `depth ≤ max`）；首字节超时**当场**记指标（B2 修复未回归）；发送后失败**默认不重放**（防重复计费方向正确）；breaker 边界四个方向都对（容量错误不计、`on_success` 移到正文交付后、空闲超时计入、客户端断连不计）；被拒的 429 **不**消耗窗口（不存在"越拒越封"）；`check_count` 在路由之前；token 总和 `saturating_add`；鉴权先于路由且判词必落 ctx；缓存 key 含 tenant（L2 也按租户命名空间）；凭据交换只发一个头且客户端自带凭据头从不透传；302 不跟随（所有 fallback 分支都带策略）；下游 body 读取的 fail-closed 三分支与 408/413/400 正确；三个 `0` 在解析期被拒；`/v1/models` 不记账；子租户归因用**原始** key（落库才是 masked）；`status` 标签不会被伪造。

### 无法静态判定的（复审明示）
把 provider 加进 `MatchCtx` 后 count 语义的连带影响（需跑测试 + 对照 `ops.md:319-330` 的既有角色配方）；非 2xx 丢弃响应是否真的导致连接不复用（hyper 机制推断，未抓包）；mid-stream 截断时上游实际计费的 token 数；`ArcSwap` Guard 跨长流持有（`proxy.rs:554` 跨 SSE 转发持有）在真实负载下是"仅延迟回收"还是"退化为每次 `load_full`"（需压测，**未列为正式缺陷**）；`hydra_requests_total` 是否被外部按营收口径消费（影响 P1-2 的修法选择）。

---

## 2j. 第三十一轮下半 · 启动/角色-拓扑/监听复审（第九路 Oracle）：**2 条 P2、3 条 P3 + 更正我自己的两处说法**

### P2-1 `HYDRA_ROLE` 拼错或写成 `standby` ⇒ **静默降级为单节点**，并让三条"拒绝启动"的集群校验**一起失效**
`cluster/mod.rs:48-61` 的 `from_env` 对未知值只 `warn!` 然后回落 `All`；而 `main.rs:222-301` 的所有集群校验都以 `role.is_cluster()`（只认 `leader|edge`）为条件 —— `HYDRA_REDIS_URL`、`HYDRA_CLUSTER_TOKEN`、`HYDRA_USAGE_SINK=clickhouse`、`HYDRA_CONTROL_URL`（以及 `:247` 的 `HYDRA_ADMIN_TOKEN`）**全部不再检查**；`main.rs:412` 的 `redis_backend` 直接给 `None` ⇒ 进程内 limiter、无 L2、registry 为 `None`（不注册/不参选/不持租约）、`tenant_config_forwarder` 为 `None` ⇒ **数据面写本地落库**。
**后果（以官方 cluster compose 为例）**：把 `HYDRA_ROLE: leader` 写成 `leadre`，该容器**照常启动、healthcheck 仍 200**（它只查 `/api/v1/health`），但它不参选；管理员把 UI 指向它时**写落到它自己的本地 SQLite**，而下一次 `restore_config` 会把这次写入**抹掉**（"先成功后丢失"）；若写进 K8s manifest（`deployment.md:166`），`/healthz/leader` 在 `all` 节点回 404 ⇒ **readiness 探针永不通过、滚动永久卡住且不报错**。
`jiqun-deploy.md:91` 已部分承认"拼错会 WARN 后回落"；**本条新增的事实是**：这条退路同时让三条明确写着 "refusing to start" 的 fail-closed 校验失效。修法：`Err(_)`（未设置）才等于 `All`，`Ok(其他)` 应**拒绝启动**（`from_env` 返回 `Result`），或至少在"配了集群参数却没有合法角色"时拒绝。**仅凭代码可判**；"是否允许拼错退化"需产品决策。

### P2-2 按 `ops.md` 的构建命令起官方 compose ⇒ **节点拒绝启动**
`ops.md:34` 的快速上手写 `cargo build --release --features server`，而 `environment/docker-compose.yml:48` 硬编码 `HYDRA_USAGE_SINK: clickhouse`；`usage-clickhouse` 是**独立**特性（`server` 不含它）⇒ `sink.rs:909-928` 返回 `BuildSinkError::ClickHouseFeatureDisabled`，`main.rs` 用 `?` 传播后 **exit 1**。反向也成立：`deployment.md:12` 的构建命令**含** `usage-clickhouse` —— **ops.md 的构建行与自己指向的 compose 不一致**。修法：把 `ops.md:34` 改成 `--features server,usage-clickhouse`（与 `deployment.md` 对齐）。仅凭代码可判。

### P3-1 契约点 4 与实现相反：TLS 端口被占时是"记录并降级为明文"，不是"启动错误"
`listeners.rs:40-42` 的契约写 "An illegal or conflicting address is a **startup error**, never a silent fallback"，而 `main.rs:1292-1300` 在 TLS bind 失败时只 `error!` + `record_listener_misconfig("tls_bind_failed")` 然后 `None`（**继续只讲明文**）。`ops.md:52` 与 `boot_listeners.rs:181` 把当前行为钉成规格 ⇒ **代码与 ops.md 一致、与 listeners.rs 自己的契约相反**。修法二选一（(a) bind 失败即拒绝启动，与明文同策略；(b) 改写契约点 4 为"降级并计数"）——**(a) 会把"TLS 端口被占"从可用性降级升级为整机不启动，属产品决策**。

### P3-2 `hydra_listener_bound{protocol="tls"}` 在"配置成功但真正 bind 失败"时**永远是 1**
`main.rs:1322` 的 `record_listener_bound("tls", tls_bound.is_some())` 在 `add_service` 之后、`run_forever` **之前**执行，即**早于 Pingora 真正 bind**；明文侧有 `spawn_listener_self_check`（20s 内实测连接、失败会写 `false`）自校正，**TLS 侧没有任何自校正路径**。而 `ops.md:926` 的告警正是 `> 0 certs and bound{tls} == 0` ⇒ **该告警永不触发**。触发窗口是 probe→bind 之间的微秒级竞态（端口被抢），机制上存在。修法：与明文对称，在服务起来后实测一次 TLS 地址的 connect 再写 gauge。

### P3-3 管理监听地址**没有**可绑定探测，失败形态与数据面相反
`main.rs:1381-1384` 的管理服务没有 `probe_bind`（明文数据面在 `:1251` 有）。端口被占时 Pingora 的 `Listeners::build()` 以 `?` 短路、`start_service` 内 `.expect("Failed to build listeners")` panic ⇒ **该 service 的全部地址都不绑**；因为管理面与数据面是两个 service，结果是"**数据面在、管理面无声消失**"，容器 healthcheck 判死并反复重启，运维看到的是重启循环而不是明确的启动错误。修法：`probe_bind(&admin_addr)` 一行（`probe_bind` 已是 `pub`）。仅凭代码可判。

### 更正我自己的两处说法（同一路复审指出）
1. **§1u.C 的"`main.rs:409/1190/1308` 的 `#[cfg(not(any(tls-*)))]` 分支在 CI 里从未被编译"在当前树上已不成立**：第 21 轮把 `required-features` 从 `["server"]` 松到 `["proxy"]`，且 CI 有 proxy 作业 ⇒ 那三个分支**是可编译的**。该句描述的是第 21 轮**之前**的状态，措辞需限定（D-7/批次 5 第 1 项里"`main.rs` 不参与编译"同样需复核）。
2. `ops.md` 的环境变量总表（`:41-64`）**未收录 `HYDRA_RESEAL_SECRETS`**（它只出现在 §3 的轮换 SOP）——表格遗漏，不构成误导，记入批次 6。

### 核对为**正常**的（10 条，判据见复审原文）
`listeners::plan` 的纯函数校验（含"同端口不同地址"不静态拦截这一**已记录**的已知限制，非新发现）；`scan` 顺序无"保护跑在被保护对象之后"（DB→迁移→reseal→`ConfigStore::load`→cert_store→Redis/registry→auth/sink→AppState→控制客户端→选举）；**生产路径只有一处 `expect`**（`main.rs:1269-1272`）且被同一 `#[cfg]` 表达式保护 ⇒ **不可达**，其余 `expect` 全在 `#[cfg(test)]`；`HYDRA_RESEAL_SECRETS` 的位置（迁移之后、真正解密之前）与退出码语义正确；信号/关闭三信号齐备且与 `ops.md` 的 `KillSignal=SIGQUIT` 对齐（**并指出 `Dockerfile:59` 的 "graceful zero-downtime upgrade via SIGQUIT" 在本二进制里不成立** —— Hydra 从不以 `--upgrade` 再启动，属注释口气而非运行时缺陷）；`tls.rs` 的 store/lookup/回调（键只由 `to_lowercase` 产生、`lookup` 入口同样折叠、通配只一级、后缀混淆有负例、无匹配且无 default 时**不回落到别的租户证书**）；三个 compose + Dockerfile 的 `HYDRA_*` 名称全部被代码读取、`HYDRA_DB_URL` 卷路径正确、`HYDRA_CONTROL_URL` 是静态值但转发目标由注册表+租约实时解析且有自转发守卫 ⇒ **不形成转发环**；`probe_bind` 在 Linux 上确实设 `SO_REUSEADDR` ⇒ 不存在"探测被 TIME_WAIT 拒绝而 Pingora 本可绑定"的假拒绝；`record_active` 是一次性 `OnceLock`（与已记录的 P3 相符）。

### 无法静态判定的（复审明示）
probe→bind 竞态的真实概率与容器/systemd 下的最终表现（决定 P3-2 是否该升级）；发现 P2-1 在真实 compose/StatefulSet 上的**可恢复时长**；`tls_settings_failed` 的触发条件；管理面 bind 失败时 Pingora 是否只杀该 service 的 runtime（与 `panic` 策略有关）；两个 gauge 在**所有**特性组合下是否都真的注册可见。

---

## 2k. 第三十二轮：从两路复审里挑出的两条"小而承重"的修复

### A. 【P2 · 已修】按 `ops.md` 的构建命令起官方 compose ⇒ **节点拒绝启动**
`ops.md` §1 的快速上手写 `cargo build --release --features server`，而该节第 2 步指向的 `environment/docker-compose.yml:48` 硬编码 `HYDRA_USAGE_SINK: clickhouse`；`usage-clickhouse` 与 `server` 是**互相独立**的特性（`Cargo.toml`），缺它时 `build_sink` 返回 `BuildSinkError::ClickHouseFeatureDisabled`，`main.rs` 用 `?` 传播后 **exit 1**。也就是说**文档化的单节点快速上手必然起不来**，而 `dev-docs/deployment.md:12` 的构建命令一直是含 `usage-clickhouse` 的 —— 两处文档互相矛盾。
**修**：`ops.md` 改为 `--features server,usage-clickhouse`，并在注释里写明"为什么单单 `server` 看着对却不可能启动"，同时注明与 `deployment.md` 对齐。

### B. 【P1-1 的可见化 · 已修】让"永远不可能匹配"的限流维度**不再静默**
复审查明（§2i P1-1）：`limit_role.matching_provider` 非空时，前置门禁的 `MatchCtx.provider` **恒为 `None`**（门禁在路由之前），而 `dim_matches` 要求相等 ⇒ 该角色被 count 与 token **两类检查同时跳过**；同时记账路径**带着** provider 写窗口 ⇒ 一个"只写不读"的窗口。它却可以被配置、被落库、在管理面被展示（`POST /limit-roles` 无任何校验），全仓没有非 `None` 的测试用法。
**修（本轮做的是"可见化"，不是"实现它"）**：`config::validate` 现在对 `matching_provider.is_some()` 的角色报 **Warn**，文案直接说明"门禁在路由之前 ⇒ 该角色被整体跳过、限流从不生效"。**刻意用 Warn 而不是 Fatal**：既有配置不能因此停止校验，但这个陷阱不能继续静默。
**测试**：`tests/validate.rs::validate_limit_role_with_a_provider_dimension_is_reported_as_dead`（构造带 provider 维度的角色，断言警告里同时出现 `matching_provider` 与角色 id）。
**反向证伪**：删掉该警告 ⇒ FAIL（`got []`）。
**未做（需决策）**：①真正实现该维度（候选集合已知后逐候选构造带 provider 的 `MatchCtx` 再检查）；②在管理面写入口**拒绝**该字段。两条都改对外行为/语义，记为 **D-11**。

### C. 本轮验证
`clippy -D warnings`（core 全目标）exit 0；`cargo test -p hydra-core` **249/0**；`--test validate` **19/0**（新增 1 条）。**未重跑** server/可选套件（两处改动分别是 core 内部的校验与纯文档）。

### D. 复审队列里剩下的可判定项（下一轮）
管理面**没有启动前 `probe_bind`**（一行，与明文同策略）｜`ttft_ms`/`forward_latency_ms` 用 `unwrap_or(0)` 冒充 0（应透传 `Option`）｜`hydra_listener_bound{tls}` **无自校正**（`ops.md:926` 的告警因此永不触发）｜`HYDRA_ROLE` 拼错回落单节点时**三条 "refusing to start" 校验一起失效**（改 `from_env` 返回 `Result` 属行为变更→需决策，但"配了集群参数却没有合法角色"这一条可判定）｜`ops.md` 环境变量表漏收 `HYDRA_RESEAL_SECRETS`｜`retries_total` 少计与陈旧注释。

---

## 2l. 第三十三轮：管理端口的启动前探测 + "未测量的延迟"不再冒充 0

### A. 【P3 · 已修】管理监听地址没有启动前 `probe_bind`，失败形态与数据面**镜像反转**
明文数据面在 `main.rs:1251` 有 `probe_bind`，管理端口**没有**：`admin_service.add_tcp(&admin_addr)` 直接交给 Pingora，而那里的 bind 失败发生在 `Listeners::build()` 的 `?` 短路 + `start_service` 内的 `.expect("Failed to build listeners")`。因为管理面与数据面是**两个 service**，结果是"**数据面在服务、管理端口永远不监听**"——而三个 compose 的 healthcheck 全都打管理端口的 `/api/v1/health`，于是容器进**重启循环且没有任何启动期错误可读**（`ops.md` 记载过的是这个事故的镜像形态）。
**修**：在 `add_tcp` 之前加 `probe_bind(&admin_addr)`，失败即 `refusing to start`（与明文同策略、同 `.into()` 错误转换）。
**测试**：`tests/boot_listeners.rs::a_taken_admin_port_refuses_startup`（占住端口 → 断言进程**退出且非零**、日志能解释原因）。
**反向证伪**：删掉该探测 ⇒ FAIL，实测进程**存活满 20 s**（"must make the process EXIT rather than start half-alive (data plane serving, admin port never listening)"）；修复后该文件 3 passed / 1 ignored、0.23s。

### B. 【P3 · 已修】`ttft_ms` / `forward_latency_ms` 用 `unwrap_or(0)` 冒充 0
`proxy.rs:1398-1399` 把两个**未测量**的延迟写成 `Some(0)`，而同一记录里的 token 字段刻意保留 `None`（注释："a provider that does not report a dimension must not masquerade as a zero count"）——**同一条记录两种口径**。后果：非流式 2xx（没有 chunk）会把"没有 TTFT"记成 `ttft_ms=0`，与"首字节在 1 毫秒内到达"不可区分。
**修**：直接透传 `ctx.ttft_ms` / `ctx.forward_latency_ms`（两者本就是 `Option<u64>`）。
**验证**：`--features server` 全量 **541 / 0 / 1 ignored**（含 TTFT 与用量记录相关套件），无回归——也就是说没有任何测试依赖"未测量 ⇒ 0"这个旧口径。

### C. 本轮验证
`clippy -D warnings`（server 全目标）exit 0；`--features server` **541/0/1**；`boot_listeners` 3/0/1。**未重跑**可选套件（上轮绿 668/2/3）。

### D. 复审队列剩下的可判定项（下一轮）
`hydra_listener_bound{tls}` **无自校正**（`ops.md:926` 的告警因此永不触发——probe 成功但 Pingora bind 竞态失败时 gauge 恒 1）｜`HYDRA_ROLE` 拼错回落单节点时三条 "refusing to start" 校验一起失效（其中"配了集群参数却没有合法角色"可判定）｜`ops.md` 环境变量表漏收 `HYDRA_RESEAL_SECRETS`｜`hydra_retries_total` 每条 `continue` 早退路径少计 + 三处陈旧注释（`provider_client.rs:18/219` 仍称 300 s 总超时、`proxy.rs:1159` 称 `retry_after_connect` 无人读取、`:1200` 的 "for every candidate"）。

---

## 2m. 第三十四轮：三处陈旧/相反的注释 + 一处漏收的环境变量

本轮全是"文档与事实不符"这一类（本计划的主题），都是复审在两路报告里点出的，逐条改并核对。

### A. `provider_client.rs` 两处仍宣称"客户端级 300 s 总超时"
- `:18-19`："sharing a short-timeout client … so `ProviderClient` carries its own long-lived (**300 s timeout**) connection pool"；
- `:219-221`："**the client-level 300s timeout still covers the whole exchange, including body reads**"。
**事实**：那个总超时**在第 2 轮就被删掉了**（它会把每一次超过 5 分钟的正常生成截断成"HTTP 200 + 半截 SSE"），现在只有两个显式界：`upstream_first_byte_timeout_secs`（响应头）与 `upstream_stream_idle_timeout_secs`（body 块间隔）。这两处注释正是"下一轮以为还有兜底"的来源，已改成事实（并写明"为什么这里**不能**用 `RequestBuilder::timeout`：那会掐断流式 body"）。

### B. `proxy.rs:1159` 称 `retry_after_connect` "declared and documented but **read by nothing**"
**事实**：它现在**被读了**（就是下面那个 `never_reached_upstream` 分支）。已改为："它被读；今天没有任何 env 接线（因此保持文档默认 `false`）；一旦接线，`true` 的语义就是下面注释所描述的重复计费场景"——这样下一轮不会因为这句旧话误判。

### C. `proxy.rs:1200` 的 "for every candidate we fall through from"
**事实**：计数器只在循环体末尾递增，而**5 条 `continue` 早退路径**（provider 缺配置 / endpoint 不可解析 / 无 api-key / `keys.choose` 返回 None）与 502 早退**都不计**。已把注释改成实话（说明这些是"被跳过的候选"而非"重试了请求"，所以没有在此计数），并把它**是否该有独立计数器**记为批次 6 的开放问题（属指标口径决策）。

### D. `ops.md` 环境变量总表**漏收 `HYDRA_RESEAL_SECRETS`**
它此前只出现在 §3 的轮换 SOP 与 `HYDRA_ENCRYPTION_KEY_PREVIOUS` 那行的正文里。已补成独立表行，并顺带写清三件容易误用的事：①这是**一次性维护开关**（跑完即退出、不服务流量）；②必须在旧钥仍在环里时跑；③`failed` 非空就**不要删旧钥**，且"already current"现在是**实证**（第 14 轮改成真去 `open` 一次）。

### E. 本轮验证
`clippy -D warnings`（server 全目标）exit 0（三处注释改完后重跑）。**未跑测试**：本轮改动为纯注释与文档，无行为变化（这一点是刻意的——如果一处注释改动需要改代码才成立，那说明我改错了）。

### F. 复审队列剩下的可判定项（下一轮）
`hydra_listener_bound{tls}` **无自校正**（probe 成功但 Pingora bind 竞态失败时 gauge 恒 1 ⇒ `ops.md:926` 的告警永不触发）｜`HYDRA_ROLE` 拼错回落单节点时三条 "refusing to start" 校验一起失效（其中"配了集群参数却没有合法角色"可判定）｜以及上面 C 的"跳过候选是否该有独立计数器"。

---

## 2n. 第三十五轮：一条复审结论**被证伪**（TLS gauge 的"无自校正"），改的是语义而不是加代码

复审（§2j P3-2）报：`hydra_listener_bound{protocol="tls"}` 在 "probe 成功但 Pingora 真正 bind 失败" 时**永远是 1**，而 `ops.md:926` 的告警条件正是它 `== 0` ⇒ 告警永不触发。**其余部分成立，但"这是 liveness 漏洞"这个前提不成立**，我逐行核过：

- 明文与 TLS 是加在**同一个** Pingora service 上的（`main.rs:1260` 的 `proxy_service.add_tcp(&plan.plain)` 与 `:1275` 的 `proxy_service.add_tls_with_settings(&tls_addr, …)`），而 `Listeners::build()` 是**按 service 全有或全无**（`main.rs:1262-1264` 的注释、以及 `probe_bind` 之所以存在的理由都写着这点）；
- 因此"TLS 绑定失败而明文照常服务"这条路径**不存在**：service 构建失败 ⇒ 两个监听器都没有 ⇒ 明文侧的自检（20 s 内实测连接）把 `plain` 改成 0 ⇒ **`plain` 那条告警会响**；
- 剩下的真问题是**语义**：`record_listener_bound("tls", …)` 发布的是**配置决策**且永不被修正，所以它是一个**配置** gauge，而 `protocol="plain"` 才是**存活** gauge。

**处置（不加代码）**：在发布点写了详细注释说明"为什么这里刻意不做第二次拨号"（同一 service、全有或全无、`plain` 已经覆盖），并给 `ops.md` §9.1 那两行告警补上"两个协议要区别读"的说明（`plain` = 存活；`tls` = 配置；`tls == 1` 的含义是"TLS 被配置过"，不是"TLS 正在 accept"）。
**为什么这是本轮的正确动作**：加一个 TLS 拨号是**冗余**的（不会带来新信息），而"gauge 名字读起来像存活、实际是配置"才是会误导运维的那部分——**与第 17 轮那条被证伪的复审结论（sink 批次组成）同类：先核实前提，再决定要不要动代码。**

### 本轮验证
`clippy -D warnings`（server 全目标）exit 0（注释改完后重跑）。改动为注释与文档，无行为变化。

### 复审队列剩下的可判定项（下一轮）
`HYDRA_ROLE` 拼错回落单节点时**三条 "refusing to start" 校验一起失效**（可判定的子集：把 `from_env` 的那句 `warn!` 升级为列出"被忽略的集群接线"的 `error!`，不改回落行为本身）｜`hydra_retries_total` 的跳过候选是否该有独立计数器（指标口径）。

---

## 2o. 第三十六轮：`HYDRA_ROLE` 拼错的回落**不再安静**（并说明它到底丢掉了什么）

复审（§2j P2-1）查明：`cluster/mod.rs` 的 `from_env` 对未知 `HYDRA_ROLE` 只 `warn!` 后回落 `All`，而 `main.rs` 里**每一条集群专属的启动校验都以 `is_cluster()` 为条件** ⇒ 回落之后，`HYDRA_REDIS_URL` / `HYDRA_CLUSTER_TOKEN` / `HYDRA_CONTROL_URL` / 仅对集群角色生效的 `HYDRA_ADMIN_TOKEN` **四条校验一起失效**；该节点**不参选、不注册、无 L2**，数据面写落到**自己的本地 SQLite**（下一次 `restore_config` 覆盖掉），K8s 里 `/healthz/leader` 回 404 ⇒ **滚动永久卡住且不报错**。

**修（本轮：只做"可见化"，不改回落行为）**：`from_env` 把这条日志从 `warn!` 升级为 **`error!`**，并列出**将被忽略的集群连线**（新增纯函数 `ignored_cluster_wiring(redis_url, cluster_token, control_url) -> Option<String>`，只在"确实配了集群参数"时返回清单，否则维持普通 `warn!`——因为那时回落正是文档化的单节点默认）。注释里也写清了四件因此被跳过的事。
**为什么不动回落本身**：`jiqun-deploy.md` 明确把"拼错就退出集群"写成有意行为（"拼错会 WARN 后回落单节点，但会退出集群"），改它会动零配置默认路径 ⇒ 属产品决策；而"静默"才是真正的缺陷（复审的原话也是"陷阱是静默的"）。

**测试**：`cluster/mod.rs::tests::a_role_fallback_names_the_cluster_wiring_it_ignores` —— 覆盖"什么都没配 ⇒ `None`"、"空白值不算配置"、三者齐全时逐个被点名、以及**只配一个**也要报告。
**反向证伪**：把 helper 改成恒返回 `None` ⇒ FAIL（`cluster wiring must be reported`）。

### 本轮验证
`clippy -D warnings`（server 全目标）exit 0；`--features server` **542 / 0 / 1 ignored**（541 + 本条）；`cargo test -p hydra-core` **250 / 0**（249 + 第 32 轮的 `validate` 测试）。

### 复审队列剩下的可判定项（下一轮）
`hydra_retries_total` 的"跳过候选是否该有独立计数器"（指标口径，属决策）｜以及两路复审里那些**需要决策**的项（已归入 D-8/D-11 等）。**可判定池子再次只剩 1 条**。

---

## 2p. 第三十七轮：全量门禁复核（第 32–36 轮改动之后）

第 32–36 轮里我的验证是**分片的**（第 34 轮只跑 core、第 36 轮只跑 core+server），所以本轮把 13 条门禁整套重跑一遍，按 section 逐段求和：

| 门禁 | 退出码 | 计数 |
|---|---|---|
| fmt / clippy×4（server、optional、tls-openssl、proxy-only） | 全部 **0** | |
| `test hydra-core` | 0 | **250 / 0**（249 + 第 32 轮的 `validate` 测试） |
| `test hydra-server --features server` | 0 | **542 / 0 / 1 ignored**（+第 33 轮 admin 探测测试、第 36 轮 role 回落测试） |
| 可选 + 活 Redis/CH（`--no-fail-fast`） | 101 | **670 / 2 / 3** |
| ignored: listener limitation / clickhouse e2e | 0 / 0 | 1 / 0 与 2 / 0 |
| compose config / findings-disposition / i18n | 0 | 3 个 compose VALID、27/27 |

**唯一的两个失败**是：`empty_body_delete_invalidates_all_local` 与 `too_many_invalidation_keys_are_refused_and_publish_nothing` —— 即 **D-2**，自第三轮以来一直如此、没有任何漂移。也就是说：第 32–36 轮（ops.md 构建命令、`matching_provider` 警告、管理端口启动探测、`ttft_ms`/`forward_latency_ms` 透传、三处陈旧注释、`HYDRA_RESEAL_SECRETS` 表行、`HYDRA_ROLE` 可见化、TLS gauge 语义澄清）**没有引入任何回归**，整棵树除 D-2 之外是绿的，且每条都有退出码。

---

## 2q. 第三十九轮：给"被跳过的候选"加计数器 —— 两次实验把这条发现**搬到了另一个位置**

### A. 背景（复审 §2i P3-4）
复审指出：失效转移循环里有 4 条"跳过这个候选"的路径（provider 缺配置 / endpoint 不可解析 / 无 api-key / 取不到可用 key），它们**只打 `warn!`、没有任何计数器**；于是"某 provider 的 key 被删空 ⇒ 它静默退化为永不选中的死权重"这件事在 `/metrics` 里查不到，客户端只看到笼统的 "all candidates failed"。

### B. 本轮做了什么（纯增量）
新增 `hydra_candidate_skipped_total{provider,reason}`（`reason` 取值 `missing_config` / `bad_endpoint` / `no_key` / `no_usable_key`），在 4 条路径各加一次记录，并配 getter 供测试使用。**纯增量**：不改任何既有指标的含义。

### C. 两次实验（这才是本轮真正的收获）
为了给它写测试，我依次构造了两个场景，结果**都没能触发**这些分支，而且原因各不相同：
1. **删光 `provider_key` 行 + `reload_all()`**（这是复审描述的"key 被删空"场景）：reload **成功**，请求也确实失败了 —— 但失败原因是 **"no candidates"**，而不是循环里的 `no_key` 分支。也就是说**没有 key 的 provider 是在"候选选择"阶段就被过滤掉的**，根本走不到逐候选的循环里。第一次实验的实际报错是 `200`（因为请求走的是**内存快照**，删表不发布新快照就没用），补上 `reload_all()` 之后才变成"no candidates" —— 这两步各自都值得记下来。
2. **把 provider 的 endpoint 改成不可解析**：`reload_all()` 直接返回 **`FatalValidation("provider 'p1' … has invalid endpoint 'not-a-url': must be an http:// or https:// URL")`**，快照根本不发布，循环里仍是那个合法 endpoint ⇒ 循环里的 `bad_endpoint` 分支同样**不可达**。

**结论**：在**合法配置**下，这 4 条分支看起来是**防御性/不可达**的；我加的计数器只在"未来某次改动真的让这种候选进入循环"时才会响。**复审这条发现因此被搬到另一个位置**：真正的"provider 静默退出轮转"发生在**候选选择**那一步（`router::resolve` / 候选构造），下一轮应该在那里加计数器——那才是有 `warn!` 或干脆什么都没有的地方。

**处置**：①保留这个增量计数器（无害的防御性埋点），但在 `record_candidate_skipped` 的文档里**如实写明上面两次实验的结论**与"它现在只是防御性的"；②**删掉那条写不出来的测试**（不能通过的测试不能留在树里），把实验结论写进代码注释与本节；③把"候选选择阶段的可观测性"记为下一轮的明确待办（含精确位置）。

### D. 本轮验证
`clippy -D warnings`（server 全目标）exit 0；`--features server` **542 / 0 / 1 ignored**（与第 37 轮一致——本轮没有新增测试，删掉了一条不可通过的）。两次实验都真跑过，报错原文已抄进注释。

---

## 2r. 第四十轮：把"没有任何信号"缩小成"有信号、但归错对象"

顺着 §2q 那条被搬家的发现继续查，本轮把结论再收窄一次——**这次是靠读代码把上一轮的推断纠正掉**。

第 39 轮的结论是"provider 静默退出轮转这件事在 `/metrics` 里查不到"。本轮核到：**信号其实存在**。
`proxy.rs:870-887` 在 `router::resolve(...)` 返回 `Err(RouteError::NoAvailableProvider)` 时会
`crate::admin::metrics::record_route_error(&tenant_id, reason)`，而该指标是
`hydra_route_errors_total{tenant, reason}`（`metrics.rs:37/130/381/909-913`）—— 所以"没有任何可用 provider"**今天就能被计数、也能告警**。

**真正缺的是归因对象**：这个计数器的标签是**租户**，没有 provider。于是运维能看到"租户 X 在 2026-09-29 14:00 出现大量 `no_available_provider`"，但**看不出是哪个 provider 掉出了轮转**（是 key 被删空？权重变 0？熔断？还是 endpoint 笔误被 `FatalValidation` 挡住？）。复审的原话是"某 provider 的 key 被删空 ⇒ 静默退化为死权重"，准确的说法应该是：**退化的结果被计数了，退化者没有被点名**。

因此下一轮的正确动作也随之明确（而不是"在候选选择处加计数器"这种笼统说法）：
1. 在**候选构造**处（`store`/`resolve` 之前那个把 provider 变成候选的环节）记录"谁被排除、为什么"——需要一个**带 provider 标签**的计数器（reasons 与 §2q 的四个一致，最好能复用同一个指标族）；
2. 由于 `router::resolve` 在 `hydra-core`（纯逻辑、拿不到 shell 的 key 表），"没有 key" 这一类排除**只能由 shell 判定**，所以埋点位置必须在 shell 侧的候选构造，而不是 core 的 `resolve`。
3. 第 39 轮那两个实验（删 key ⇒ 走的是 `NoAvailableProvider`；坏 endpoint ⇒ `FatalValidation` 根本不发布快照）说明：**能被观测到的退化只有第一种**，第二种被配置校验挡在发布之前 —— 这也解释了为什么"坏 endpoint"永远到不了循环里的分支。

### 本轮验证
**未改代码**：本轮只更正了上一轮记录的结论（读 `proxy.rs:870-887` + `metrics.rs:37/130/381/909`）。之所以不改，是因为"给候选排除加 provider 归因"需要先定指标族与 reasons 复用方式（**指标口径选择**），而不是随手加一族新指标；把它留给下一轮并带着上面三条明确约束。

---

## 2s. 第四十一轮 · 第十路复审：`admin-ui` 的 JavaScript（**无 XSS**，6 条契约漂移）

这路审的是唯一从未被碰过的面：`admin-ui/{index.html,app.js,api-docs.js,stats.js,i18n.js}` 与它的服务路径（`static_files.rs` + `admin/mod.rs` 的 `/admin/*` 分支）。**结论：注入类没问题，问题全是"UI 与 Rust API 的契约漂移"。**

### P2-1 UI 编辑 provider 会**静默清空**并发字段 ⇒ 准入控制被无声关闭
- `admin-ui/app.js:297-303` 的 providers 表单**只有 5 个字段**（id/key/name/endpoint/weight），没有任何并发字段；`:852-867` 的 `collectBody` 只遍历这些字段，而编辑走的是**整体 PUT**（`:750`）。
- 服务端把它当"缺字段=设 NULL"：`model.rs:34-42` 三个字段是 `Option<u32>` + `#[serde(default)]`，`db.rs:442-459` 的 `UPDATE` **无条件覆盖**这三列。
- 而 `proxy.rs:1002` 正是用 `provider.max_concurrency` 做准入闸 ⇒ **该 provider 变成不限并发**（队列等待上限一并消失）。
- `admin-ui/api-docs.js:114` 还写着这些字段"可改"，**文档与 UI 互相矛盾**；e2e 只创建、从不经 PUT 编辑并发（`tests/e2e/admin.spec.cjs:90-99`），所以结构上抓不到。
- 修法（只改 UI）：①在表单补三个 `type:"number"` 字段使 PUT 回传原值；或②改成"先 GET 再只覆盖表单字段"。

### P2-2 写操作后的 `POST /reload` 失败被**吞进空 catch**
`app.js:157-161` 的 `writeAndReload`：`try { await api("POST","/reload",…) } catch { /* reload best-effort */ }` —— 服务端 `400 reload_failed` 的语义是"**旧快照被保留**"，而 UI 仍显示 "Created/Updated"；返回体的 `changed`/`version` 也全被丢弃。**`snapshot_stale` 在 UI 里是死数据**（全仓只命中 `api-docs.js` 的说明文字）。修法：不再吞异常，明确提示"写入已提交，但运行时重载失败（旧快照保留）"。

### P2-3 "reveal plaintext" 开关是**空操作**
`app.js:604-609` 的开关只写 `STATE.revealKeys`，全仓**只赋值、从不读取**（`:625` 定义）；服务端 `handlers.rs:522-525` 早已把 `?reveal=1` 变成 no-op（"admin API NEVER returns plaintext provider keys"）。安全侧是好消息，坏消息是 **UI 与四语文案仍在承诺一个不可能发生的功能**，运维会误以为"密钥没配"。修法：删掉开关与 `STATE.revealKeys`，或把文案改成"明文永不可见"。

### P3 三条
- `Promise.all` 让 `/cluster/status` 的 502（`cluster_unavailable`）**劫持整个健康页**（`app.js:1114-1125`）：`/health` 已拿到的数据被丢弃，只剩永久骨架 —— 而进程本身是健康的。修法：`allSettled`。
- 非 JSON 错误体使 toast 退化成 **`"429 429: "`**（`app.js:143-153`），且服务端专门下发的 **`Retry-After` 从不被读**。修法：清洗文案 + 读该头。
- tenants PUT 会清掉 UI 里根本不存在因而无法回传的 `cert_file`/`cert_key`（`db.rs:842-853` 整体替换）。影响有限（证书**内容**在 `tenant_cert` 表、未受影响），坏的是迁移前的路径回退配置被抹掉。修法：补两个字段或先 GET 回填。

### **核对为正常**的（这部分的"没有发现"同样有信息量）
1. **无 XSS**（做了实证而非肉眼）：全部 markup sink 只有 4 处，数据渲染一律走 text（`renderCell`/toast/模态）；唯一的 `innerHTML` 风险点 `highlightJson`（`app.js:1182-1191`）被**用 Python 复刻同一算法 + 敌意输入**验证：输出只有 `span`/`class`，无 `on*`/`src`/`href` —— 因为 `esc()` 在正则之前已实体化 `& < > "`。
2. **token 处理干净**：只有 `sessionStorage` + `Authorization` 头 + **相对路径**（`API="/api/v1"`），不进 URL、不进日志、不跨源；登出后 reload 不被复活（e2e 已覆盖）。唯一不一致是 `index.html:13-16` 的注释写 "never persisted"（与 sessionStorage 实现矛盾，四语文案才对）。
3. **17 个 API 调用逐个核对**：路径/方法全部存在且一致，无"UI 读了但服务端不发"或反之。
4. **202/503 的诚实性 UI 做得对**：`doInvalidate` 不把 202 当失败，而是读 `fleet.state` 四态并把 lagging 节点列出来；`unavailable` 的文案明说 "the fleet was NOT told"。
5. **服务路径挡得住**：`/admin/*` 的 asset 分支刻意免鉴权（资源里无秘密）但**先 404**（edge）且在 token 门禁之外**只允许 GET**；路径欺骗三件套（`/ADMIN/`、`..`、`%2f`）逐个被否证（`strip_prefix` + 分隔符检查 + **嵌入文件名的精确等值查找**）。唯一提示是缺 `cache-control`（当前无风险，将来加 CDN 才需要 `no-store`）。

### 未验证（复审明示）
`highlightJson` 的真实 DOM 行为（用 HTMLParser 复刻算法，未在 Chrome/Firefox 实测）；`max_concurrency` 清空后"并发真的不再受限"需活实例 + 负载工具；e2e 断言是否真的会红（未反向证伪）；服务端是否校验 `control_url` 的 scheme（超出本轮范围）。

### 本轮为何不改代码
三条 P2 里两条要动 **JS + 四语 i18n**（新增字段/文案要同步 `i18n.js` 的四个 locale，而 CI 的 `scripts` 作业会跑 `check_i18n.js`），而本轮我已没有足够上下文同时完成"改 JS + 改 4 个 locale + 跑 i18n 检查 + 跑 UI e2e"。**只改一半会正好落进这个仓库反复出现的那类问题**（改了代码没同步文案/校验），所以本轮只做记录，下一轮带着完整上下文一起做。

---

## 2t. 第四十二轮：修掉 UI 静默清空并发上限（P2-1），并用"列覆盖"核对收尾

### A. 问题回顾（§2s P2-1）
`admin-ui/app.js` 的 providers 表单只有 5 个字段，而编辑走的是**整体 PUT**；服务端把"缺字段"当 `None` 写库（`db.rs:447-449` 的 `UPDATE provider SET … max_concurrency = ?, max_queue_depth = ?, queue_wait_timeout_ms = ?`）。于是**在 GUI 里改一个 provider 的名字，就把它配好的并发上限静默清零**——而 `proxy.rs:1002` 正是用 `max_concurrency` 做准入闸，等于把这个 provider 的限流关掉。e2e 只创建、从不经 PUT 编辑这些字段，所以结构上抓不到。

### B. 修法（只改 UI + i18n）
`app.js` 的 providers 表单补三个字段，用**已存在**的 `optint` 映射（`readValue` 里 `"" ⇒ null`，正好是"不限"）；并在注释里写明**为什么这些字段必须出现在表单里**（PUT 是整体替换 + 缺字段即 NULL）。`i18n.js` 补 `field.maxConcurrency` / `field.maxQueueDepth` / `field.queueWaitTimeoutMs` 三个标签到**全部四个 locale**（en/zh/fr/de）。
**先验证再动手的两件事**：①`optint` 映射本来就有（`app.js:840-851`）；②`openForm` 在编辑时确实以 `isEdit ? record[f.name] : f.value` **预填**（`app.js:708`）——否则加了字段也照样提交空值、等于没修。

### C. 验证
1. **i18n 门禁（CI 的 `scripts` 作业会跑）**：`node scripts/check_i18n.js` ⇒ `OK (341 en keys, 4 locales, code↔en consistent)`，exit 0 —— 证明三个新标签在四个 locale 里都存在、且代码引用的键全部可解析。
2. **列覆盖核对**（本轮做的结构性检查）：`db.rs` 的 provider `UPDATE` 写入的列是 `key, name, endpoint, weight, updated_at, max_concurrency, max_queue_depth, queue_wait_timeout_ms`；表单现在提交**全部**这些业务列（`updated_at` 由 `collectBody` 从原记录回填）。**修好之后不存在"服务端会覆盖、而表单不提交"的列**——这正是这条缺陷的判据。
3. **未做（如实记录）**：**浏览器级的往返测试**（设并发 → 编辑 provider → 断言值仍在）没有跑。它属于 `tests/e2e/admin.spec.cjs`，而本地跑 Playwright 需要构建二进制 + 起实例 + seed（`ui-e2e` 作业那一整套），本轮上下文不允许；**下一轮第一件事就是给 e2e 补这条断言并真的跑一次**（这是唯一能证明"UI 编辑不再丢字段"的测试）。

### D. 本轮验证小结
`node scripts/check_i18n.js` exit 0（见上）。**未跑** cargo 测试套件：本轮只改 JS 与 i18n 文案，未触及 Rust（`admin_ui.rs` 只断言静态资源的服务与鉴权，与本改动无关）。

---

## 2u. 第四十三轮：给"UI 编辑不再丢并发字段"补上**浏览器级**回归守卫（T2.2c）

### A. 新增的测试
`tests/e2e/admin.spec.cjs` 新增 **T2.2c a UI edit keeps the provider concurrency limits**：
1. 用 admin API 建一个 provider，显式带上 `max_concurrency: 7 / max_queue_depth: 3 / queue_wait_timeout_ms: 1500`（比走 UI 建更快、且能精确指定这三列）；
2. 登录 → providers 列表 → 打开该行的 Edit；
3. **断言表单真的带着这三个值**（`toHaveValue('7'/'3'/'1500')`）—— 这正是本轮修复的那部分：字段缺失时定位器会超时；
4. 做**当年会清空它们的那个动作**（改 name）并保存；
5. `GET /providers/{id}` 读回，断言三列**原样还在**。
测试里写明了 Falsification：把 `CRUD.providers.fields` 里那三条删掉 ⇒ 第 3 步超时、第 5 步读到 null。

### B. 它在 CI 里会真的跑
`ci.yml` 的 `ui-e2e` 作业第 470 行执行 `npx playwright test --config=playwright.config.cjs`，而该 config 覆盖 `tests/e2e/`；所以这条守卫进的是**每次 CI 都会跑**的套件，不是"写完就算"的孤儿测试。

### C. 本轮验证（如实区分"验过的"与"没验的"）
- ✅ `node --check tests/e2e/admin.spec.cjs` ⇒ 语法通过；
- ✅ `node scripts/check_i18n.js` ⇒ `OK (341 en keys, 4 locales…)`（本轮新标签与四语文案一致）；
- ✅ 第 42 轮那条**列覆盖核对**：`db.rs` 的 provider `UPDATE` 写的 8 列，表单与 `collectBody` 现在全部覆盖；
- ❌ **没有跑 Playwright**。原因有三，且都不是"偷懒"：①它需要浏览器（CI 里由 `npx playwright install --with-deps chromium` 装）；②它需要一个**新建并 seed 过的实例**（`ui-e2e` 作业那套：构建二进制 → 起实例 → `seed.sh`）；③直接拿本机 `hydra-a/b/c` 那套跑，会往你的本地开发库里**写入测试数据**（一个 `pw-conc-*` provider），我不想在没有授权的情况下动它。
- 想手动复现的人只需要：起一个实例（`HYDRA_ADMIN_TOKEN` 已知）→ `HYDRA_BASE=http://127.0.0.1:8081 HYDRA_ADMIN_TOKEN=… npx playwright test --config=playwright.config.cjs -g T2.2c`。

### D. 状态
至此第十路复审（§2s）的 **P2-1 已闭环**（UI 修复 + 列覆盖核对 + 浏览器级守卫 + i18n 门禁）。§2s 里剩下的 5 条（吞掉 reload 失败、reveal 空开关、健康页被 `Promise.all` 劫持、`"429 429:"` 文案与不读 `Retry-After`、tenants PUT 清 `cert_file`）与 §2r 的候选排除归因仍在队列里。

---

## 2v. 第四十四轮：被吞掉的 reload 失败不再静默 —— 并修掉让"静默"无法被发现的**门禁盲区**

### A. P2-2（§2s）：`writeAndReload` 把 `POST /reload` 的失败整个吃掉
原文（`admin-ui/app.js:150-153`）：
```js
async function writeAndReload(op) {
  await op();
  try { await api("POST", "/reload", { body: {} }); } catch { /* reload best-effort */ }
}
```
服务端契约是：运行进程无法采用新快照时，`POST /reload` 返回 **400 `reload_failed` 并且保留 OLD 快照**（读侧对应的标记是 `snapshot_stale`）。所以这里吞掉的是一个具体的状态：**写进库了、运行进程没采用**。编辑的 save 路径紧接着弹 `common.toast.updated`（"Provider updated"），而 UI 之后每个视图读的都是**数据库行**（`loadEntity`）——于是操作者看到新值出现在所有页面上，合理地以为已经生效，而 proxy 仍按旧配置在跑。这比"没有信号"更糟：它给出的是**错误的确认**。

### B. 修法：顺序也要一起修（不只是补一个 toast）
只在 `writeAndReload` 里补一个 `toast(...)` 不够：`toast()` 是**追加**到 `#toast-root`，而两个调用点都在 `writeAndReload` 返回**之后**才弹自己的成功 toast ⇒ 操作者眼睛最后落在"Provider updated"上，警告被埋在它上面。所以改成"**返回错误、由调用点在自己的成功 toast 之后报**"：
- `writeAndReload(op)` 返回 `null`（运行进程已采用）或那个 Error（写落地了、reload 没成）；两个调用点（save 与 `doDelete`）改成 `const reloadErr = await writeAndReload(...)`，在各自成功 toast **之后** `if (reloadErr) toastReloadFailed(reloadErr)`。
- 文案 `common.reloadFailed` 加入**全部四个 locale**，并且本身说清"两件事都成立"：已保存，但运行时重载失败——运行中的进程可能仍在用旧配置（请先重新读取再重试）。
- `catch` 的语义保持只覆盖 `op()` 自身的失败（外层 catch 仍弹 `failedUpdate`/`failedDelete`）；reload 失败**不走**那条路——把它报成"更新失败"同样是错的，因为写**确实**落地了。

### C. 顺带发现的**门禁盲区**（本轮真正的头条）
补 `common.reloadFailed` 之前我先跑了一次 `node scripts/check_i18n.js`，它输出 **`OK (341 en keys, 4 locales, code↔en consistent)`** —— 而那时这个键**在 `i18n.js` 里根本不存在**，代码里却已经有 `t("common.reloadFailed")`。原因在扫描器自己：`scripts/check_i18n.js` 的模板字面量分支把**整个模板跳过**（旧代码扫到反引号就 `break`，然后 `code += "``"`），而 `${…}` 里面是**真实代码**。于是两个方向同时失明：
- 方向 2（代码→en）：`` `${t("a.b")}` `` 里的键**从不被检查** ⇒ 引用缺失键的代码被判 `OK`（**假阴性**，本轮正是踩到它）；
- 方向 3（en→引用 / 死键）：只在模板里被引用的键**被判死键**（**假阳性**）。

征兆出现在加完键之后那句 `DEAD-EN-KEY common.reloadFailed`：一个假阳性反过来暴露了假阴性。仓库里目前只有 2 处模板内含 `t(`（`app.js:171`、`app.js:791`，后者的键是运行时拼的），所以**波及面小**；但门禁的**性质**是"报告一致"而实际漏检，与 §1y 那类"守卫静默通过"同源。

### D. 修法与反向证伪（11 条断言）
`scripts/check_i18n.js`：
- 新增 `extractBraced(src, i)`——按词法规则（嵌套字符串/模板/正则/注释/转义）配平花括号，取出 `${…}` 的内部源码。**只数花括号是不够的**：字符串里的 `}` 会提前闭合，字符串里的 `{` 则永不闭合；
- 模板分支改为对每个 `${…}` **递归 `scanSource()`**，把其字面量并入 `literals`、把其代码并入待扫描的 `code`；
- 模板的**文本块**仍然不算引用（运行时拼出来的键本来就无法验证），这一点写在注释里，避免后人以为"模板全都算"。

`scripts/check_i18n.test.cjs` 新增 3 条断言（8 → 11），并用**旧扫描器反向证伪**（把 `git show HEAD:scripts/check_i18n.js` 换回去跑同一个测试文件）：
- 旧扫描器 **3 条失败**：`FAIL template-interior missing key exits non-zero -> status=0 out=OK (3 en keys, 4 locales, code↔en consistent)`、`FAIL … is reported`、`FAIL template-only reference is not a dead key -> status=1 out=DEAD-EN-KEY stats.tip`；真实仓库上仍旧输出 `DEAD-EN-KEY common.reloadFailed`；
- 新扫描器 **11 条全 PASS**，真实仓库 `OK (342 en keys, 4 locales, code↔en consistent)`。

**如实记录**：6b（嵌套模板 + 字符串内花括号的折磨用例）在**新旧扫描器下都 PASS** —— 旧扫描器的 "naive backtick 配对" 会把嵌套模板的内部**意外暴露**成普通代码，所以它**不是**证伪用例；真正证伪死键方向的是 6a（单个模板、且是唯一引用）。这句话写在测试注释里，防止后人高估 6b 的守卫价值。

### E. 新增的浏览器级守卫（T2.2d）
`tests/e2e/admin.spec.cjs` 新增 **T2.2d a failed runtime reload is surfaced after the success toast**：用 `page.route('**/api/v1/reload', …)` 把 `POST /reload` 打成 `400 {"error":{"code":"reload_failed",…}}`（套件里 `cluster/status` 已有同样的 route 拦截先例，故不需要真的造一个坏 store），然后走"编辑 provider → 保存"，并断言：
1. **`.last()` 的 toast 是 `runtime reload FAILED`** —— 顺序即行为：埋在成功 toast 上面就不算"报出来"；
2. `GET /providers/{id}` 读回新名字 —— 证明**写确实落地**（这正是"把它报成失败"也不对的原因）。

Falsification 写在用例注释里：把 toast 挪回 `writeAndReload` 内部、或恢复空 catch，该断言失败。

### F. 本轮验证（区分"验过的"与"没验的"）
- ✅ `node --check admin-ui/app.js`、`node --check scripts/check_i18n.js`、`node --check tests/e2e/admin.spec.cjs` 全部通过；
- ✅ `node scripts/check_i18n.test.cjs` ⇒ **11/11 PASS**（含新增 3 条）；`node --test scripts/check_i18n.test.cjs` 同样通过（CI 的那条命令）；
- ✅ 反向证伪：旧扫描器 3 条失败、新扫描器 0 条失败（见 D）；
- ✅ `node scripts/check_i18n.js` ⇒ `OK (342 en keys, 4 locales, code↔en consistent)`，exit 0；
- ❌ **没有跑 Playwright**（原因同 §2u C：需要浏览器、需要新建并 seed 过的实例、且不愿往你本地开发库里写测试数据）。T2.2c 与 T2.2d 同属"已进 CI 套件、本地未跑"的状态；
- **未跑 cargo**：本轮只动 JS 与文档，未触及 Rust。

### G. 一处**未改**但记下的东西（避免下轮重复发现）
`admin-ui/app.js:1232` 的空 `catch { return; }`（`refreshLeaderBanner`）是**刻意**的：探测失败时不新建 banner。但它的注释写着"no banner either way"，而代码在探测失败时**不会移除已存在的 banner** ⇒ 一次瞬时失败后，一个"你不在 leader 上"的旧 banner 会一直留着（每 30s 重探，若一直失败就一直是旧状态）。两种改法都成立（保留上次已知状态以免闪烁 / 失败即撤下），属于语义选择而非缺陷，**未擅自改动**；若要改，应连同注释一起改。

---

## 2w. 第四十五轮：UI 契约漂移第二批（reveal 空开关 / 健康页被劫持 / **同名全局被静默遮蔽**）＋ 一套**不需要浏览器**的 UI 回归测试

### A. P2-3：`reveal plaintext` 开关是空操作，而且文案在承诺一个不可能发生的功能
**服务端事实**（不是推断）：provider key 的读取**永远**是掩码 —— `admin/handlers.rs:512` 的注释与 `:532/:536`、`:576-581`、`:601-605`、`:647-651` 四处都走 `hydra_core::rewrite::mask_key`（`core/rewrite.rs:6`：`first10 + *** + last4`），而 `?reveal=1` 在 `handlers.rs:522-525` 是**为兼容而接受、实际 no-op**。`api-docs.js:160` 早就如实写了这一点。

**UI 侧却是三处相反的话**：①`app.js` 的面板开关只写 `STATE.revealKeys`（全仓**只赋值、从不读取**），并且它调用 `loadEntity` 重渲染后复选框自己又变回未勾选；②`crud.provider-keys.desc` 写着 "Masked by default; reveal is audit-logged." —— **两句都假**（从不曾解掩码；也没有任何审计日志）；③`tip.masked`（"stored masked after create"）是四句话里唯一诚实的。

**修法**：删掉开关 + `const STATE` + `providers-keys` 配置里的 `maskedKeys: true`（三者都只服务于这个开关），原地留一段说明为什么**不该**有开关；删掉 `common.form.reveal`（四语）；把 `crud.provider-keys.desc` 与 `tip.masked` 四语改成事实陈述（读取永远返回 `first10 + *** + last4`；要换密钥就输入新的）。
**验证**：`grep -rn "keys-reveal\|revealKeys\|common.form.reveal\|maskedKeys" admin-ui/` ⇒ **空**；`node scripts/check_i18n.js` ⇒ `OK (341 en keys…)`（342−1）；`.toggle-pill` 的 CSS **没有**变成死代码（`stats.js:36` 仍在用）。**反向证伪（实测）**：往 `app.js` 里加回一句 `t("common.form.reveal")` ⇒ `CODE-REF-NO-EN  common.form.reveal`、exit 1 ⇒ "删掉开关"这件事被 i18n 门禁钉住了。

**顺带记一条 docs↔fact**：`dev-docs/changelog-2026-09-16.md:108` 把 `8d6b97c` 描述为"删已退役的显示明文开关那次"，即这个开关**当时就该被删掉**。但 `git cat-file -t 8d6b97c` ⇒ **Not a valid object name**（本仓库 268 个提交里没有它；同文档引用的 `bccccb2`、`40f5912` 同样不存在，而 `c3eaa6f`、`d508daa` 存在）⇒ 这条历史断言**在本仓库无法核对**，且当前树里那个开关确实还在。记为 **D-12**（changelog 引用了本仓库不存在的提交，无法据以核对）。

### B. P3：健康页被 `Promise.all` 劫持 —— **并且更正复审给的机制**
复审的修法建议（`allSettled`）是对的，但它给的**触发条件错了**：它说"单节点部署下 `/cluster/status` 返回 502"。实测不是 —— `admin/cluster_api.rs:51-53`（没有 registry）与 `:92-95`（非 cluster 构建）都返回 **200 `{cluster:false}`**，而**套件里已有的 T9.5 就断言了这一点**（`tests/e2e/admin.spec.cjs:180-181`：`expect(status.code).toBe(200)` + `expect(status.body.cluster).toBe(false)`）。502 `cluster_unavailable` 的真实条件只有一个：**有 registry 但读不到（Redis 不可达）**（`cluster_api.rs:56-63`）。

缺陷本身成立，而且是**不对称**暴露出来的：同一个端点在 `refreshLeaderBanner`（`app.js` 的 `catch { return; }`）里**早就被容忍**，只有健康页让它把整页带走 —— `Promise.all([/health, /cluster/status])` 一个失败就丢掉**已经到达**的 `/health` 数据，状态卡片永远停在骨架屏，而进程本身是健康的。
**修法**：`Promise.allSettled` + 每个面板按**自己**的结果渲染；新增 `renderHealthProbeError` / `renderClusterUnavailable`；两个新键 `custom.health.unavailable` / `custom.health.clusterUnavailable` × 四语；**集群探测失败不弹 toast**（本页自己的探测失败才弹），原始 JSON 视图把两个结果都带上并标注 —— 缺键读起来像"健康且为空"。另外**刻意不复用** `custom.health.clusterNotEnabled` 那句话：它断言"HYDRA_ROLE 未设置/单节点"，与"我们问不到"是两件不同的事。

### C. 本轮新发现：**同名全局函数被静默遮蔽**（`fmtNum`）—— 一类，而不是一例
`admin-ui` 的四个脚本共享**同一个全局作用域**（`index.html:118-121` 四个 `<script>`）：`stats.js:190` 的 `fmtNum` 注释写着 "Compact number formatting: 1234 -> 1.23k, 1500000 -> 1.5M."，而 `app.js:1257` 也声明了 `fmtNum`（`Array.isArray(v) ? v.length : (v ?? "—")`，用于健康页计数）。`app.js` **最后加载** ⇒ 它赢得这个名字 ⇒ **stats.js 的格式化器从未运行过**：统计页把 `1234567` 原样打印，而作者写的是 `1.23M`（`barChart` 的 chart value 同样，`stats.js:169`）。
**修法**：重命名为 `fmtNumCompact`（定义 + 5 个调用点），并在定义处注释里写明**为什么不能叫 `fmtNum`**；同时把"四个脚本的顶层名字必须两两不相交"做成**类级守卫**（见 D）——实测全仓只有这一处冲突，所以守卫今天是零容忍的。
**与既有守卫的关系**：仓库**已经**为脚本顺序写了守卫（`admin/static_files.rs:191` 的 `index_html_loads_api_docs_before_app_js`，起因是 `app.js` 顶层**急切**读取 `CUSTOM["api-docs"].render`），但"顺序正确"完全不妨碍**同名互相覆盖** —— 对 `fmtNum` 来说顺序谁先谁后都错，只是错的函数不同（后加载者赢）。两者是互补的：顺序守卫保"读得到"，名字守卫保"读到的不是别人的实现"。
**证据（先测后修，不是事后补）**：修复前 harness 打印 `FAIL no top-level name is declared by two admin-ui scripts -> fmtNum: stats.js,app.js`，以及 `requests 1234567  tokens 2500000  prompt tokens 1000000 …`；修复后同一断言输出 `requests 1.23M  tokens 2.5M  prompt tokens 1M  completion tokens 1.5M …`。

### D. 新增 `scripts/admin_ui_render.test.cjs`：**不需要浏览器**的 UI 回归测试（21 条断言，含 2 条反向证伪）
动机很直接：这一轮的三条修复里有两条（健康页、格式化器）**只在浏览器里能看见**，而 Playwright 需要 Chromium + 新建并 seed 过的实例 + 不愿污染你的本地开发库 —— 于是它们会落进"已写、没人跑"的坑。但这两条都是**纯粹的控制流**（拿 API 响应 → 写 DOM），不需要浏览器：
- 用 `vm.createContext` + 一个最小 DOM 桩（`createElement`/`appendChild`/`removeChild`/`firstChild`/`classList`/`dataset`/`setAttribute`/`querySelector`），按 `index.html` 的顺序把四个脚本**各自** `runInContext` 进**同一个** context —— 这正是四个 `<script>` 共享全局作用域的忠实模型（也是上面那个遮蔽能被复现的原因；若把两个文件塞进同一个 `new Function` 体，重复 `const` 会变成语法错误，与浏览器行为不符）；
- 用假的 `api` 驱动**真正的**页面渲染器（`renderHealth`、`renderStatsData`），再读回它建出的**节点类名与文本**；
- 加载**真实的 `i18n.js`**（不是桩），所以断言的文案就是发布文案，键被改名/删掉会在这里同时失败。

两条**反向证伪**（都是"把修复原样退回"这一个 token 级改动，并断言替换点**恰好出现一次**）：
1. `Promise.allSettled([` → `Promise.all([` ⇒ `REVERSE: with Promise.all the health cards do NOT render`（`threw` 或卡片数为 0）；
2. `fmtNumCompact` → `fmtNum` ⇒ `REVERSE: without the rename the stats cards print raw digits`（`1234567` 且不含 `1.23M`）。

**它不替代 Playwright，也不假装能**：没有 CSS、没有布局、没有事件派发、没有真实 `fetch`；它断言的是"建出了哪些节点、文本是什么"，不是"长什么样、点起来如何"。交互仍在 `tests/e2e/admin.spec.cjs`。已加进 CI 的 `scripts` 作业（与 `check_i18n` 并列），并实测 `node --test` 会传播失败（故意改坏一条断言 ⇒ `# pass 0 / # fail 1`；探针文件已删除）。

### E. 本轮验证（逐条命令、逐条退出码）
- ✅ `node --check`：`admin-ui/{app,stats,i18n,api-docs}.js`、`scripts/{check_i18n.js,check_i18n.test.cjs,admin_ui_render.test.cjs}`、`tests/e2e/admin.spec.cjs` 全过；
- ✅ `node scripts/check_i18n.js` ⇒ `OK (343 en keys, 4 locales, code↔en consistent)`；
- ✅ `node --test scripts/check_i18n.test.cjs` ⇒ `# pass 1 / # fail 0`（文件内 11/11 断言）；
- ✅ `node --test scripts/admin_ui_render.test.cjs` ⇒ `# pass 1 / # fail 0`（文件内 21/21 断言，含 2 条反向证伪）；
- ✅ `.github/workflows/ci.yml` 可解析，`scripts` 作业 7 步、含新的 render 测试步骤；
- ✅ 先测后修的两处证据见 C、D（修复前后同一 harness 的输出差异）；
- ✅ **全量本机门禁**（`.acceptance/round10-gate.sh`，本轮从 13 条扩到 **15 条**：新增 `i18n tests` 与 `admin-ui render`）：**14 条绿**，唯一红的是那条从第 3 轮就红着的 `test optional + live redis/CH`，逐节解析日志后确认它就是 **670 passed / 2 failed / 3 ignored**，两个失败仍是 **D-2** 那两例（`admin_api.rs:1799` 的 `empty_body_delete_invalidates_all_local`、`:3138` 的 `too_many_invalidation_keys_are_refused_and_publish_nothing`）——与第 37 轮的数字完全一致（**无回归**）；其余各条（fmt/clippy×4/core/server/两个 `#[ignore]`/compose/findings/i18n/i18n tests/admin-ui render）全 exit 0；
- ❌ **未跑 cargo 之外的额外 cargo**：本轮只动 JS / i18n / CI 配置，未触及 Rust（上面那次全量门禁里的 cargo 是既有套件的复核，不是新代码）；
- ❌ **未跑 Playwright**：`T2.2c`、`T2.2d`、`T9.5c` 仍属"已进 CI 套件、本地未跑"（理由同 §2u C）。

### F. 队列剩余
§2s 的 6 条已闭环 4 条（P2-1 §2t/§2u、P2-2 §2v、P2-3 §2w-A、健康页 §2w-B）；**剩 2 条**：P3 非 JSON 错误体使 toast 退化成 `"429 429: "` 且从不读 `Retry-After`；P3 tenants PUT 会清掉 UI 无法回传的 `cert_file`/`cert_key`。另有 §2r 的候选排除 provider 归因，以及本轮新记的 **D-12**。

---

## 2x. 第四十六轮：`api()` 的错误渲染（空体 / HTML / `Retry-After`）＋ 租户表单丢掉旧版证书路径 —— 并给"整体 PUT"补上**跨语言**的列覆盖守卫

§2s 的最后两条 P3 在本轮闭环。两条都属于**"界面把服务端已经说清的事弄丢了"**，所以都用同一个办法验证：把真实函数喂进假输入，先量出坏行为，再修，再把修复退回一次。

### A. P3-a：错误响应渲染（`app.js` 的 `api()`）
**原构造**：`const code = json?.error?.code || resp.status; const message = json?.error?.message || (typeof json === "string" ? json : resp.statusText); new Error(`${resp.status} ${code}: ${message}`)`。
用新的 harness（假 `fetch` 驱动**真实** `api()`）量出三种退化，都是实测字符串：

| 输入 | 修前 | 后果 |
|---|---|---|
| 空体 + `statusText:""`（HTTP/2 不携带 reason phrase） | **`"429 429: "`** | 状态被印两遍、消息为空、末尾悬空冒号 |
| HTML 错误页（ingress 502） | 整页进 toast，实测 **6478 字符** | 噪音淹没信息，且把内部页面细节摊给操作者 |
| `Retry-After: 3` | `err.retryAfterSec === undefined` | 服务端专门为限速算好的头**从不被读**（`admin/mod.rs:181-198` → `handlers.rs:94` 的 `err_json_throttled`） |

**修法**：`errorHead(status, code)` 让 `code` 只在"说了状态没说的话"时才追加；`cleanErrorBody()` 把 HTML 压成 `<title>`（无 title 则去标签）并截断到 160 字符；`parseRetryAfter()` 同时支持 delta-seconds 与 HTTP-date；有 `Retry-After` 时消息追加 `(retry in Ns)`（新键 `common.err.retryAfter`，四语），完全没消息时用 `common.err.noDetails`（四语）；`err.retryAfterSec` 一并暴露。
**反向证伪（各断言替换点恰好 1 次）**：①把 `errorHead` 的去重条件退回"总是印两遍" ⇒ `REVERSE: without the dedup the status is printed twice`；②把 `parseRetryAfter(...)` 换成 `null` ⇒ `REVERSE: without reading the header there is no retry hint`。
**附带发现**：`err.code` / `err.body` 在 `app.js` 里**只写不读**（与 §2w 的 `STATE.revealKeys` 同一类）。本轮**保留**它们（`err.body` 仍是 `err.body = json;` —— 解析后的对象或原始 body 字符串；**清洗只进了 `message`**，这条措辞原先写错，已在 §2aa-F 更正），但 `err.retryAfterSec` 是**真的被读**的——消息里就用它；若 `code`/`body` 长期无人读，正确的处置是删掉而不是继续假装。

### B. P3-b：租户表单丢掉旧版证书路径（`cert_file`/`cert_key`）
**事实链（逐条核对，含对复审结论的收窄）**：
1. `db.rs:771`（`update_tenant`）与 `:843`（`write_tenant`，管理面 PUT 走这条）都是**整体替换**：`UPDATE tenant SET name = ?, domain = ?, auth_url = ?, cert_key = ?, cert_file = ?, enabled = ?, updated_at = ?`；缺字段反序列化为 `None`，`non_empty_path` 再把它变成 NULL。
2. 表单里只有 `cert_pem`/`cert_key_pem`（**内容**，存 `tenant_cert` 表）——它们是**动作语义**：`CertWrite::None` = 不动、`Clear` = 清、`Content` = 替换（`handlers.rs:855-910` 的 `resolved_secret_writes` / `resolve_tenant_cert_write`）。
3. 所以**内容没被清**（复审说"影响有限"是对的）；被清的是**旧版路径列**。
4. **什么时候真有害**：`config.rs:168-186` 的 `CertMeta` 是 **content-first**，"the loader falls back to them when no content is present" ⇒ 只有"有路径、没内容"的行（pre-0007 行，或直接用 SQL 写的行）会因此**丢掉整份证书**（下次 reload 后该域名回落到默认证书），而且**无声**。对已经转换成内容的行，清的只是两个不再被读的列。

**修法**：把 `cert_file`/`cert_key` 放回租户表单（`map:"opt"` ⇒ 留空即 null ⇒ 存 NULL），tip 里写明它们会在保存时**被读取并转换为密封内容**——这正是 pre-0007 行的迁移路径；文件读不到是**响亮的** 400 `cert_file_unreadable`（而不是静默清掉），操作者可以清空字段继续。i18n 新增 `field.certFilePath`/`field.certKeyPath`/`tip.certFilePath`/`tip.certKeyPath` 四语。

**刻意不做的事（并写明原因，免得下一轮把它当 bug"修"掉）**：不让 PEM 内容字段"留空即清空"。GET 从不返回证书内容 ⇒ 编辑时 PEM 框**必然是空的** ⇒ 若"空"表示清空，**每一次 UI 编辑都会抹掉证书**。`map:"opt"`（空 ⇒ null ⇒ 服务端 `CertWrite::None` = 不动）是有意的安全选择，代价是 GUI **无法**删除证书（`cert_pem:""` 只能从 API 走）。这是"能力缺口"，不是缺陷。

**同时修 docs↔fact**：`api-docs.js` 的 POST 条目原写 "empty strings are ignored"、PUT 条目原写 "Blank cert fields keep the current cert" —— 对**内容**成立，对**旧版路径列**不成立（PUT 写什么就是什么，缺了就清）。两句已改成把两种机制分开说清。

**可核的历史链**：`admin_api.rs:896-926` 的 `tenant_create_ui_payload_no_zombie_on_400` 记着**上一代**同一个坑——当年表单**有**这些字段、空值以 `""` 送出，被当成真实路径 ⇒ 400 `cert_file_unreadable`，而租户**已经 INSERT**（"报告失败但实际创建"）。服务端修成"空串不是路径（存 NULL）"。此后字段从表单里消失，于是**同一个列以相反的形状**（缺字段 ⇒ NULL）再次被静默清空。

### C. 新的**跨语言**类级守卫（本轮最有价值的一条）
`scripts/admin_ui_render.test.cjs` 新增检查：从 `crates/hydra-server/src/db.rs` **读**所有 `UPDATE <table> SET ... WHERE`，只看**触碰 `updated_at`** 的那些（= 整体替换；定向单列 UPDATE 如 `access_token_hash`、证书密文由各自路径写，**不该**成为表单字段），然后要求该实体的 `CRUD[key].fields` 覆盖其全部列（`updated_at` 除外——`collectBody` 会从记录回填）。
- 它**当场抓到两个历史实例**：`providers.max_concurrency/max_queue_depth/queue_wait_timeout_ms`（§2t 的 P2-1）与 `tenants.cert_key/cert_file`（本轮 P3-b）。
- 反向证伪对**两者各做一次**：把那条字段从表单里删掉 ⇒ 同一个检查报出缺口（`REVERSE: dropping the tenants cert path field is caught by the coverage check`、`... the providers concurrency field ...`）。
- **判据说清楚**：这个检查读的是**服务端的 SQL**，不是人工维护的清单——所以下次服务端多写一列时它会自己变红，而不是等下一路复审。
- 另补"字段存在但**没预填**等于没修"：用 `openForm(CRUD.tenants, record)` 真跑模态构建器，断言 `[data-field="cert_file"]` 的值就是记录里的路径（`name`/`domain` 同时断言，证明这不是"碰巧读到"）；反向证伪：把 `openForm` 的预填表达式退回"永不预填" ⇒ `REVERSE: without the prefill an edit would submit an empty cert path`。

### D. harness 的三处能力扩展（都是为了能**本地**验证上面这些）
1. `fetchImpl`：可注入假 Response（`ok`/`status`/`statusText`/`headers.get`/`text`），从而驱动**真实** `api()` —— 此前 harness 只能注入假 `api`，等于绕开了要测的函数；
2. `callExpr` + `globals`：入口表达式可引用**上下文内**的值（`CRUD.tenants`、`__record`），因为 `CRUD` 无法从 Node 传进去；
3. DOM 桩补 `data-*` 属性选择器（真实 DOM 里 `dataset.field` **就是** `data-field`）与 `remove()/focus()`。缺前者时"模态里找不到 input"会**伪装成"字段不存在"**——第一次跑出来是 `name=undefined`，看起来像表单坏了；这个坑本身值得记一笔。

### E. 本轮验证
- ✅ `node --check`：`admin-ui/{app,i18n,api-docs}.js`、`scripts/admin_ui_render.test.cjs` 全过；
- ✅ `node scripts/check_i18n.js` ⇒ `OK (349 en keys, 4 locales, code↔en consistent)`（本轮 **+6 键**：`common.err.noDetails`、`common.err.retryAfter`、`field.certFilePath`、`field.certKeyPath`、`tip.certFilePath`、`tip.certKeyPath`）；
- ✅ `node --test scripts/admin_ui_render.test.cjs` ⇒ `# pass 1 / # fail 0`，文件内 **41 条断言**，其中 **7 条反向证伪**（Promise.all 退回 `all`、`fmtNumCompact` 退回 `fmtNum`、去重退回、`parseRetryAfter` 退回、租户证书字段删除、provider 并发字段删除、预填退回）；`node --test scripts/check_i18n.test.cjs` ⇒ `# pass 1 / # fail 0`；
- ✅ 修复前后的同一 harness 输出差异已抄进 A/B（`"429 429: "`、**6478** 字符、`retryAfterSec=undefined`、`tenants.cert_key, tenants.cert_file`、`cert_file=undefined`）；
- ❌ **未跑 cargo**：本轮未触碰 Rust —— `find crates -newermt "-100 minutes"` 的最早命中仍早于本轮全部 JS 改动（改动时间戳：`admin-ui/*.js` 19:56，最新 Rust 19:36），而本会话上一次**全量门禁**（§2w：15 条、14 绿、唯一红的仍是 D-2 两例）已覆盖当时全部 Rust 改动；
- ❌ **未跑 Playwright**：T2.2c/T2.2d/T9.5c 与"租户证书路径的浏览器级往返"仍属"已进 CI、本地未跑"。本轮的处理原则是：**能用不依赖浏览器的方式验证的，就不再堆进那张清单**。

### F. 队列
§2s 的 6 条**全部闭环**（P2-1 §2t/§2u、P2-2 §2v、P2-3 §2w-A、健康页 §2w-B、`"429 429:"`/`Retry-After` §2x-A、租户证书路径 §2x-B）。**可判定池子只剩 1 条**：§2r 的候选排除 provider 归因（三条约束见 §2r）。另有 **12 项待决策**（D-1…D-12，最紧的是 D-8）。

---

## 2y. 第四十七轮：给"掉出轮转的 provider"点名（§2r 落地）—— 并**更正 §2r 的一条前提**

这是**最后一条可判定项**。§2r 把问题定位得很准（"退化的结果被计数了，退化者没有被点名"），但它的第 2 条约束**基于一个错误前提**，本轮先纠正再动手。

### A. 更正 §2r 的前提：`router::resolve` **看得见** key 表
§2r 写的是"`resolve` 在 `hydra-core`（纯逻辑、拿不到 shell 的 key 表），所以'没有 key'只能由 shell 判定"。实测不是：`router.rs` 的第 (4) 步就在过滤 `cfg.provider_keys.get(pid).is_some_and(|k| !k.is_empty())` ——**快照里带着 key 表**，core 完全看得见；它只是**不能碰 metrics**（core 的依赖被刻意限制在 serde/serde_json/memchr/bytes/sha2）。

所以正确做法不是"在 shell 侧重新判定一遍"（那会是第二份真相），而是：**core 负责判定并返回归因数据，shell 负责记录**。

### B. 改动（core 出数据、shell 记录）
1. `hydra-core/src/model.rs` 新增三个类型：`ExclusionReason`（`BreakerDead` / `NoKey` / `ZeroWeight`，带 `as_str()` → `breaker_dead`/`no_key`/`zero_weight`）、`ExcludedCandidate { provider_id, reason }`、`ResolveOutcome { candidates, excluded }`。
2. `router.rs`：`resolve` 的实现搬进 **`resolve_detailed`**，逐 provider 记录第 (4) 步的三种丢弃（原来那串 `filter/filter/filter_map/filter` 改成显式循环，只为能带上"为什么"）；`resolve` 变成**一层薄包装**（`one implementation, two views`），签名与语义对既有调用者/测试**零变化**。
3. **空集不再是 `Err`**：第 (4) 步把候选清空时，`resolve_detailed` 返回 `Ok(ResolveOutcome { candidates: [], excluded })` —— 否则归因会**正好在最需要它的时候**（全部被丢弃）被丢掉。`resolve` 包装层把"空集"还原成历史上的 `Err(NoAvailableProvider)`；shell 侧遇到空集时走**同一条 503 路径**（用同一对 `route_error_status/route_error_reason` 计算，不硬编码），所以对外行为不变。
4. `proxy.rs`：成功 resolve 之后 `record_excluded_candidates(&outcome.excluded)`，逐条 `record_candidate_skipped(provider, reason)`。**成功请求也记录**，因为要钉住的正是那种"别的 provider 还在服务、所以什么都没失败"的静默退化。
5. 指标文档改实话：`hydra_candidate_skipped_total{provider,reason}` 现在有**两族** reason —— **选择阶段**（`breaker_dead`/`no_key`/`zero_weight`，真实可达）与**循环内的防御性**那一族（`missing_config`/`bad_endpoint`/`no_key`/`no_usable_key`，§2q 实测不可达）。`no_key` **刻意同名**：按"某 provider 没有可用 key"告警的人不该去猜是哪条代码路径发现的。模块表与注册时的 help 文本同步更新。

**刻意不记录的地方**：无 `model` 字段的 passthrough 路径（`passthrough_candidates`）。它**在第一家合格 provider 处就返回**，后面的候选根本没被检查 ⇒ "还有谁被排除"在那里**从未被计算**，只记录前几家会给出**误导性的半份答案**。这个理由写在 `proxy.rs` 的注释里。

### C. 验证
- **core 单测**（`crates/hydra-core/tests/router.rs`，+2 例，40 passed）：
  - `resolve_detailed_attributes_every_exclusion`：p_a 删 key、p_b 权重 0、p_c breaker dead ⇒ `excluded == [(p_a,no_key),(p_b,zero_weight),(p_c,breaker_dead)]`（按 provider_id 定序），`candidates` 为空且**返回 `Ok`**；同时断言包装层 `resolve(...)` 仍返回 `Err(NoAvailableProvider)`（历史契约不变）。
  - `resolve_detailed_attributes_nothing_when_all_providers_are_usable`：没丢弃就**不许**有归因（否则计数器是噪音，不是信号）。
- **shell 集成测**（`crates/hydra-server/tests/terminate_mode.rs`，+1 例，47 passed）：真起 Pingora + wiremock，两家 provider 都服务 gpt-4，**在运行中删掉 p_gone 唯一的 key 并 `reload_all()`**，然后发请求：
  1. 基线（两家都有 key）：请求 200，`candidate_skipped("p_gone","no_key") == 0`；
  2. 删 p_gone 的 key 后：请求**仍然 200**（p_live 在服务），而 `("p_gone","no_key") == 1`、`("p_live","no_key") == 0` —— **这正是过去完全无痕的那一幕**；
  3. 再发一次 ⇒ `== 2`（是计数器，不是一次性标志）；
  4. 再把 p_live 的 key 也删掉 ⇒ 请求 **503**（历史契约不变），而 `p_gone == 3`、`p_live == 1` —— **全灭时也点名**，这条正是"空集返回 `Ok` 而不是 `Err`"的价值。
- **反向证伪（实测）**：把 `proxy.rs` 里那行 `record_excluded_candidates(&outcome.excluded);` 去掉（其余不动，只留一句注释占位）⇒ 同一个测试在**预期的那条断言**上失败：`assertion left == right failed: the key-less provider must be named on the request that routed around it / left: 0 / right: 1`。已把探针删除并 `grep FALSIFICATION` 确认无残留，恢复后重跑通过。
- 全量门禁（15 条）结果见 §2y-D（本轮改动触及 core + shell，所以必须重跑）。

### D. 全量门禁（15 条）与**我自己的一次测量错误**
首次跑：**13 条绿 / 2 条红**。两条红各有原因，都已处置：
1. **`cargo fmt --check` exit=1 —— 我自己的**：`resolve_detailed_attributes_nothing_when_all_providers_are_usable` 里一行超宽（python 写入的 Rust 不会自动满足 rustfmt）。`cargo fmt` 后 **`cargo fmt --check` 复跑 clean**，并复跑 `cargo test -p hydra-core --test router` ⇒ 40 passed（格式化只动换行）。
2. **`test optional + live redis/CH` exit=101 —— 仍是 D-2 两例**：section 解析后为 **671 passed / 2 failed / 3 ignored**，失败是 `empty_body_delete_invalidates_all_local`（`admin_api.rs:1799`）与 `too_many_invalidation_keys_are_refused_and_publish_nothing`（`:3138`），与第 37/45 轮一致。

**其余 13 条全绿**（fmt 修好前：clippy×4、core、server、两个 `#[ignore]`、compose、findings、i18n、i18n tests、admin-ui render）。本轮改动逐条对得上：

| 门禁条目 | 本轮之前 | 本轮之后 | 差额 |
|---|---|---|---|
| `test hydra-core` | 250 | **252** | +2（两条新 router 测试） |
| `test hydra-server (server)` | 542 | **543** | +1（terminate_mode 新测试） |
| `test optional + live redis/CH` | 670 / 2 / 3 | **671 / 2 / 3** | +1，红的仍是 D-2 |

**并更正我自己的一次测量错误（第二次同类）**：我最初把 optional 那节数成 **674**（670+4），并差点把它当成"多出来的 3 条测试"去追查。原因是**按范围切日志**（从最后一个 `test optional + live` 匹配切到文件尾），把后面 `ignored:`/`compose`/`findings`/`i18n` 各节的 `test result:` 行一起算了进去 —— 这正是本计划 §2p（第 37 轮）已经写下的那条教训："解析必须**按 section**（`re.split(r'^########## ')`），不能按范围 `awk`"。按 section 解析后：671 / 2 / 3，与 +1 的预期完全一致，**没有无法解释的增量**。教训的处置是**把它写下来**（本条），而不是"这次注意点"。
## 2z. 第四十八轮：把"从不本地运行的 e2e"变成**可静态核对**的契约（新 `check_e2e_contracts.cjs` + 它自己的 5 条证伪）

可判定池子已空，且两条复审仍在飞。本轮的活来自一个**已知负债**：`T2.2c` / `T2.2d` / `T9.5c` 属于"已进 CI 套件、本地从未跑过"的用例（需要 Chromium + 新建并 seed 的实例，且不愿写你的本地开发库）。这类用例有两种失效形态，**都不是"测试失败"**：①选择器 / 导航键被改名 ⇒ CI 变红；②更糟：断言**变成空的**（`toHaveCount(0)` 之类照样绿），于是"有覆盖"是假象。既然不能跑，就先让它**机器可核对**。

### A. 先人工逐条核对这三个用例的全部依赖（这是守卫的判据来源）
- **选择器**：`#modal-root`（`openModal` 的追加目标）、`.modal-overlay`、`.modal-foot`、`button.btn.primary`（`submitBtn`）、`#toast-root`/`.toast`（`toast()`）、`#content table tbody tr`（`renderTable`）、`#health-stats`/`#cluster-stats`/`#cluster-nodes`/`#health-json`（`renderHealth`）、`.stat`/`.skeleton` —— 全部存在。
- **导航键**：`renderNav` 用 `dataset: { key }`（`app.js:605`）⇒ `#nav button.nav-item[data-key="health"]` 成立；`health` 与 `providers` 都在 `NAV` 的 items 里。
- **行内 Edit 按钮**：`iconBtn(name, label, ...)` 把 `title: label`（`app.js:770-773`），en 的 `common.action.edit` 是 **`"Edit"`** ⇒ `button[title="Edit"]` 成立。**这一处最脆**：改一次 i18n 文案而不同步 e2e，断言就静默失效。
- **断言文案**：`/runtime reload FAILED/` ← `common.reloadFailed` 的 en 文案 ✓；`/unavailable/i` ← `custom.health.unavailable` ✓；`#cluster-nodes` **不含** `/HYDRA_ROLE/` ✓（`clusterUnavailable` 的文案里没有那句"单节点"的话）。
- **路由**：`**/api/v1/reload`（`admin/mod.rs` 的 `parts == ["reload"] && method == "POST"`）、`**/api/v1/cluster/status`（`parts == ["cluster","status"]`）✓。

### B. 新守卫 `scripts/check_e2e_contracts.cjs`
读 `tests/e2e/*.spec.cjs`，抽出 `locator/fill/waitForSelector/click/...` 的**字面**选择器与 `navItem(page,'key')` 的键，再到 `admin-ui/{app,stats,api-docs,i18n}.js` + `index.html` + `style.css` 里核对每个 `#id` / `.class` / `[data-field="x"]` / `[data-key="v"]` / `[title="v"]` 真的存在。`data-*` 既接受 HTML 字面量、也接受 `dataset:` 赋值（UI 用的是后者 —— 这正是 `[data-key]` 第一次跑出来的**假阳性**，已修）。
两条自我约束：
- **拒绝空过**：抽到的 token 少于 `MIN_TOKENS = 40` 直接判失败（"抽不到东西还打印 OK"是本仓库反复踩的坑）；
- **明确不覆盖 API 路由**：`admin/mod.rs` 按**路径段**匹配（`parts == ["cluster","status"]`），文本核对只会变成猜谜 —— 路由留给浏览器跑，这一条写在脚本头部。
实测真实仓库：**`OK (224 selector/nav tokens across 3 spec file(s))`**。

### C. 反向证伪（5 个分支，逐个实测）
| 探针（在 spec 的临时副本上做替换） | 结果 |
|---|---|
| id `toast-root` → `toast-rooot` | exit 1：`DRIFT … needs id "toast-rooot"` |
| class `modal-overlay` → `modal-overlai` | exit 1：`DRIFT … needs class` |
| `data-field="max_concurrency"` → `…max_concurreny` | exit 1：`DRIFT … needs a form field named` |
| `navItem(page,'health')` → `'healt'` | exit 1：`DRIFT nav key "healt" does not appear…` |
| **空 spec（一个选择器都没有）** | exit 1：`FAIL only 0 token(s) examined (< 40)` |

并把这些分支固化成 `scripts/check_e2e_contracts.test.cjs`（**11 条断言**，含"真实 spec 通过"与"必须报告检查了多少 token"两条元断言），`node --test` ⇒ pass 1 / fail 0。

### D. 接线
CI 的 `scripts` 作业 **7 → 9 步**（新增 `e2e selector contracts (static)` 与 `e2e contract checker tests`）；本机门禁 **15 → 17 条**；计划 §4 的复现命令同步两条。

### E. 本轮验证
- 5 条 JS 门禁条目**全绿**：`i18n` ⇒ `OK (349 en keys, 4 locales…)`；`i18n tests`、`admin-ui render`、`e2e contracts` ⇒ `OK (224 …)`、`e2e contract tests` ⇒ pass 1 / fail 0；
- **未跑 cargo**：本轮未触碰任何 Rust 文件 —— `crates/**` 的 mtime 全部早于第 47 轮那次全量门禁日志的时间戳（`20:03:58`）；唯一在门禁之后变化的是 `crates/hydra-core/tests/router.rs` 的**格式化**（`cargo fmt`），已单独复验 `cargo fmt --check` clean + 该 target `40 passed`。这一点**如实区分**："门禁覆盖的是格式化前的字节"，而格式化只动换行。

### F. 顺带记一条**代码里的坑**（本轮自己踩到）
给这个脚本写头部注释时我写了 `` `**/api/v1/...` `` —— 其中的 `*/` **提前闭合了块注释**，于是注释后半段变成代码 ⇒ `SyntaxError: Unexpected token '...'`。注释里不要写 glob。已改写措辞。

---

## 2aa. 第十一路复审的处置（`admin-ui` / `scripts` 的 JS 与那套 harness 自身）

第 47 轮末派出的两路复审之一（"UI/JS 与 `scripts/admin_ui_render.test.cjs` 这套 harness 本身"）已返回。约束是只读、不跑构建、必须给 `path:line` + 原文。**它抓到一条真的、我漏掉的东西**，也提出了几条**断言强度**的问题；另有一条**被我用 fixture 证伪**。

### A. 被证伪的一条（P2）："`extractBraced` 遇嵌套模板会截断 interior"
复审断言：`` `${ x ? ` + "`b${y}c`" + ` : "" }`` 这种写法会让 `depth` 失衡、interior 被吞到文件尾，于是其后的 `t("...")` **永不校验**（假阴性）。

**实测证伪**（4 个 fixture，直接跑 `scripts/check_i18n.js`，结果已固化为 `check_i18n.test.cjs` 的第 7 组 **4 条断言**）：
| fixture | 预期 | 实测 |
|---|---|---|
| interior 内含嵌套模板，其后有 `t("missing.key")` | 必须报 `CODE-REF-NO-EN` | ✅ 报了 |
| interior 内含嵌套模板 **+ 带花括号的正则** ` /}/ `，其后有该调用 | 同上 | ✅ 报了 |
| 调用位于**模板文本**里（`${...}` 之外） | **不得**报（那是文本不是调用，报了就成假阳性） | ✅ 没报 |
| 键**只在**嵌套 interior 里被引用 | 不得判死键 | ✅ 没报 |

原因是复审把嵌套模板里的 `${y}` 当成了"普通字符" —— 实际 `check_i18n.js` 的反引号分支对 `${` 是**递归** `extractBraced` 的，`${y}` 在递归里被完整消费，不会影响外层的括号计数。这是本会话**第三条被我实测证伪的复审结论**（前两条：sink 批次组成不变、TLS gauge"无自校正"）。**处置**：不改扫描器；把这条行为**钉成测试**（否则下一个人会再提一次同样的担心）。

### B. 被接受并修掉的一条：**空白输入 = 静默清除**（复审的 P1，机制属实、归因需更正）
复审的归因是"本轮新增的 `cert_file`/`cert_key` 字段让证书**内容**的清空分支第一次可随手触发" —— 这一句**不成立**：`cert_pem`/`cert_key_pem` 早就在表单里（§2x 之前就有），本轮加的是两个**路径**字段。**但它指出的机制是真的，而且比它说的更宽**：
- `readValue` 的 `map:"opt"` 只把 **恰好为 `""`** 当"未填"，**一个空格**就会原样送出（`app.js` 旧代码 `case "opt": return v === "" ? null : v;`）；
- 服务端把"存在但 trim 后为空"读成**显式清除**：`handlers.rs` 的 `resolve_tenant_cert_write` 里 `(Some(""), _) => Ok(CertWrite::Clear)`；access token 走 `resolved_secret_writes` 的 `token.trim().is_empty() => Some(None)`；
- 于是：在 PEM 文本框或 access token 框里**手滑打一个空格再保存**，就会**删掉该租户的证书 / 自助 token**，而字段旁边的四语文案写的是 "leave blank to keep"（`tip.certPem`、`tip.blankKeepToken`）—— 一句当时就**不成立**的话。
- 「**修法**」`case "opt": return v.trim() === "" ? null : v;`（值本身**不** trim：PEM 内容的尾换行必须按字节往返，服务端只为判空而 trim）。**后果**：那句话第一次**成真**；同时 GUI 依旧无法"清除"证书（`cert_pem:""` 只能走 API），与 §2x-B 记的"能力缺口"一致。
- 「**验证**」harness 新增 6 条断言（空⇒null、空格⇒null、换行/制表⇒null、真路径原样透传、**不 trim 值**、textarea 同样）+ 1 条反向证伪（把 `trim()` 退回 ⇒ `REVERSE: without the trim a spacing-only input is sent as a clear`）。

### C. 断言强度（复审的 P2/P3）——三条都接受并改了
1. **`textOf` 双计**：桩里 `textContent` 是普通属性，而 `textOf` 把"自身 + 每个子节点"相加 ⇒ 叶子文本被数两遍，语义与真实 DOM 的 `textContent` getter 不符。已改成"有子节点就只算子节点、叶子才算自身"（桩从不给有子节点的节点设 `textContent`）。
2. **健康卡断言太弱**：原断言只要求 `ok` 与 `3` **出现在网格里某处** ⇒ 五张卡全渲染成 `"—"`（或标签互换）也照样绿。已改为**标签→值配对**：用**发布文案**查标签（`t("custom.health.providers")` 等），再断言同一张 `.stat` 的 `.sv` 是 `3`/`2`/`ok`。这同时补掉了复审说的另一件事：`Promise.all` 反向用例的真实失败形态是"卡片渲染成垃圾值"，而原来的断言只覆盖到"没渲染成卡片"。
3. **`...the chart value uses the compact formatter too` 是空断言**：它断言的是整个 `#content` 含 `1.23M`，而总量卡片（也在 `#content` 里）本来就有 `1.23M` ⇒ 把图表值退回原始格式化器它**仍然通过**。已收到 `byClass(content, "chart-value")` 上，并同时要求 `1.23M`（requests 图）与 `2.5M`（tokens 图）。
4. 复审还指出反向用例的**措辞**与真实失败模式不符（"cards do NOT render" vs "渲染成 `—`"）—— 已通过第 2 条的配对断言消除了这个缺口（值不对现在会被抓住），措辞也在注释里写明。

### D. 能力空白（复审的 P3）：桩补 `prepend()` 与 `crypto`
`app.js` 的 `refreshLeaderBanner` 用 `$(".main").prepend(banner)`，桩没有该方法 ⇒ 一旦有用例驱动到那一行就会 `TypeError`（当前没有用例走到，属"假绿风险"）。桩补 `prepend`/`insertBefore`，并把 `crypto` 从 `{}` 换成 Node 的 `webcrypto`（`generateToken` 需要真字节）。

### E. 死代码一处（接受）

`check_i18n.js` 的 `scanSource` 正则分支里 `depth` **只写不读**（决定正则在何处结束的是 `inClass`，而 `{...}` 里不可能出现 `/`）。已删除并留一句说明。§2v 新增的 `extractBraced` 本来就没有这个变量。

### F. 文档不精确两处（接受）
1. **`err.body`**：我在 §2x-A 里写的"`err.body` 现在装的是清洗后的值"**与代码不符**（代码是 `err.body = json;`，清洗只进 `message`）。已在 §2x-A 原文更正并指向本节。
2. **掩码形状**：`first10 + *** + last4` 只对 `len >= 20` 成立（`rewrite.rs:238-258` 三档：≥20 → first10/last4；≥6 → first2/last2；更短更狠）。已把四语文案（`crud.provider-keys.desc`）+ `app.js` 的注释改成带长度限定；**注意 zh 用的是全角括号**（我第一版按半角替换，`count == 3` 的断言把它抓了出来 —— 顺带说明"断言替换点数量"这个习惯是有用的）。

### G. 复审"核对为正常"的部分
它列了 9 条（7 处证伪目标各只匹配一次、`coverageGaps` 的服务端事实与 `db.rs:447/771/843` 相符、`updated_at` 例外有依据、`data-*` 选择器与 `buildField` 的 `dataset` 相符、`check_i18n.test.cjs:6b` 的"偶然通过"说明诚实、`writeAndReload` 两个调用点不重复报、reveal 无残留、行号锚点可核对、健康页修复方向正确）。我抽检了其中的关键几条（7 处证伪目标、`coverageGaps` 的 SQL、reveal 残留、行号锚点），与我的核对一致；未逐条重跑的部分**不作为已核实**记录。

### H. 本轮验证
- `node scripts/check_i18n.js` ⇒ `OK (349 en keys, 4 locales…)`；`node --test scripts/check_i18n.test.cjs` ⇒ 文件内 **15/15**（新增 4 条嵌套模板断言）；
- `node scripts/admin_ui_render.test.cjs` ⇒ **49 条断言全过**（含 8 条反向证伪）；`node --test` ⇒ pass 1 / fail 0；
- `node --check`：`admin-ui/{app,i18n}.js`、两个测试文件全过；
- **另一路复审（Rust 归因改动 §2y）仍在飞**，结果并入下一轮。

---

## 2ab. 第四十九轮（上）：把"写了但没人跑"变成机制上不可能 —— 新 `check_ci_wiring.cjs`

本会话**两次**犯同一个错：写完一个 checker 却忘了把它接进 CI（`admin_ui_render`、`check_e2e_contracts`），而这个仓库的历史更长 —— 第 15 轮之前 `#[ignore]` 用例没有任何作业运行、三个 SDK 套件完全没有作业、UI e2e 是在一次生产回归之后才补上的。**存在但从不运行的文件比没有文件更糟**：它让覆盖率看起来比实际大。

新守卫 `scripts/check_ci_wiring.cjs`（对着 `.github/workflows/ci.yml` 这个"谁跑什么"的唯一权威）：
1. `scripts/` 里的每个 `*.test.{cjs,js,sh}` 与 `check_*.{js,cjs}` 必须被某个步骤点名；
2. 每个 `tests/e2e` 下的 spec 必须落在 `playwright.config.cjs` 的 `testDir` 内（**落在外面就永不运行**），且 CI 真的跑了 `playwright test`；
3. `crates/<crate>/tests` 下的每个测试文件，其 crate 必须被某条 `cargo test` 选中（`-p <crate>` 或 `--workspace`）；
4. 含**真实** `#[ignore]` 属性的文件，必须由一个带 `--ignored` 的步骤运行。
**下限防空洞**：artifact 数量低于阈值（5/20/1）直接判失败，避免"glob 写错 ⇒ 抽到 0 个 ⇒ 打印 OK"。
**实测输出**：`OK (everything is executed)` + `7 script artifact(s)` / `3 e2e spec(s) under testDir tests/e2e` / `54 crate test file(s) in 2 crate(s)` / `3 file(s) with real #[ignore] tests: boot_listeners, clickhouse_sink, usage_query` —— 最后一项正是第 15 轮那个修复的**持续验证**。

**两条我在写它的过程中实测到的坑（都进了测试）**：
1. **`grep -l '#[ignore]'` 会数错**：它把 `test_attribute_integrity.rs`（只在**注释**里提到 `#[ignore]`）也算成"有 ignored 用例"，却**漏掉** `usage_query.rs` 的 `#[ignore = "needs a live ClickHouse"]`。所以检测用**行首锚定**的正则 `/^\s*#\[ignore(\s*=|\(|\])/`，两个方向都钉了测试。
2. **块注释里的 glob**：我在头部注释里写 `crates/*/tests/*.rs`，其中的 `*/` **提前闭合了块注释** ⇒ `ReferenceError`。这是我**第二次**踩（§2z-F 记过一次）；本轮把它作为个人习惯问题记录，没有再想"造个守卫"——那种守卫会是模糊的。

**自举**：第一次运行时它唯一的发现就是自己（`scripts/check_ci_wiring.cjs is never executed by ci.yml`）—— 正是它要防的那类问题。

**反向证伪**（`scripts/check_ci_wiring.test.cjs`，**8 条断言**，全部通过 `HYDRA_WIRING_ROOT` 指向临时骨架）：
| 骨架 | 期望 | 实测 |
|---|---|---|
| 全部接好 | 通过 | ✅ |
| 多一个未接的 `check_orphan.cjs` | 失败并点名 | ✅ |
| `#[ignore]` 用例但无 `--ignored` 步骤 | 失败并点名 target | ✅ |
| **注释**里提到 `#[ignore]` | **不得**报（假阳性守卫） | ✅ 不报 |
| `#[ignore = "reason"]` 形式且未接 | 必须报（`grep` 会漏的那种） | ✅ 报了 |
| spec 不在 `testDir` 内 | 失败 | ✅ |
| 空骨架 | 触发下限，不空洞通过 | ✅ |
| —— 另外首次运行时"缺少 `scripts/` 目录"曾以 ENOENT 崩溃 | 应给出下限提示 | 已修 + 钉住 |

**接线**：CI 的 `scripts` 作业 **9 → 11 步**；本机门禁 **17 → 19 条**；计划 §4 加两条命令。

---

## 2ac. 第四十九轮（下）：处置第十二路复审（§2y 的 Rust 归因改动）—— **1 条 P2（设计缺陷）** + 6 条 P3

复审（只读、不跑构建）**未发现 P1**，并逐例核对了核心语义：`resolve()` 包装层还原历史 `Err`、空集 503 分支与旧路径**逐行等价**、三个 reason 与旧的四个丢弃条件一一对应、`record_excluded_candidates` 置于判空之前是对的、`excluded` 来自 `HashSet` 不会重复计数、新代码无 `unwrap`/索引/panic。它还独立核实了一件我没查的事：**`accessible_models` 与新的 step-4 落选集合逐例相同**（顺序不同、结果相同）⇒ 不会出现"目录列得出、数据面 503"的新分歧。

### A. P2（接受 —— 这是我引入的设计缺陷，本轮修掉）
**问题**：`zero_weight` 对**每个成功请求**为该 provider 加 1，于是这个序列 ≈ 请求量而不是"掉出轮转的次数"；更糟的是它把两件**运维含义相反**的事压进同一个 label：
- `weight == 0`：`ops.md:1031` 明确是 **soft-disabled**（受支持的运维动作，模型行保留）；
- `weight < 0`：`config.rs:286-296` 明确按**故障**处理并 Warn（`"can never be selected"`）—— 但在指标里与"我就是要关掉它"无法区分。

后果很具体：`ops.md` §9.1 的告警表 7 条里 6 条是 `increase(...) > 0` 形状，照抄一条给这个计数器写规则，会在**完全按设计配置**的健康集群上永久告警。而这族指标自述要回答"which provider went quiet"—— 恰恰在最需要它的那一类（配置写错）上答不出来。

**修法（分层：core 陈述事实，shell 拥有策略）**：
1. `ExclusionReason` 拆为四个：`BreakerDead` / `NoKey` / `InvalidWeight`（`weight < 0`，**故障**）/ `SoftDisabled`（`weight == 0`，**有意动作**）；
2. `router::resolve_detailed` 按 `weight == 0` 与 `weight < 0` 分别归因（路由结果相同，只有原因不同）；
3. `proxy::record_excluded_candidates` **显式跳过 `SoftDisabled`**，注释写明理由（否则序列跟踪流量、告警永久误报）；
4. `ops.md` §9.1 新增该指标一行，并把 `soft_disabled` **明确排除**在告警表达式之外；§10.4 补一句"成功的请求也可能在途中丢掉过 provider"（复审 P3-1：新信号当时**完全没有**进运维契约 —— 全仓 `grep candidate_skipped dev-docs/` 零命中）。

**验证**：core 新增 1 例（`weight == 0` ⇒ `soft_disabled`；`weight < 0` ⇒ `invalid_weight`；两者都仍被排除出候选）；shell 测试新增一个 `weight = 0` 的第三 provider，断言**它的计数器保持 0**（政策被钉住）；**反向证伪实测**：删掉跳过逻辑 ⇒ 该断言失败（`left: 1 / right: 0`，消息正是那条"must NOT be counted"），探针已删除、`grep FALSIFICATION` = 0、恢复后重跑通过。

### B. P3 六条（全部接受并修）
1. **新信号没进运维告警契约** → `ops.md` §9.1 加行 + §10.4 加话（见 A.4）。
2. **doc 注释错位**：我插入 `candidate_skipped_total` 时把 `record_mid_stream_error` 的文档与 `#[allow(dead_code)]` 留在了它上面 ⇒ 那个**公开访问器的 rustdoc 第一句在说另一件事**（"Increment `hydra_mid_stream_errors_total`…"），而被说的函数反而没有文档。已各自归位。
3. **`router.rs` 模块文档与新入口矛盾**：模块文档写第 (4) 步"empty ⇒ `RouteError::NoAvailableProvider`"，对 `resolve_detailed` 已为假，且模块文档**完全没提**这个新入口 —— 而 `resolve_detailed` 自己的文档写着 "See the module docs for the full pipeline"。已分两条写明两个入口的差别，并在开头点名它。
4. **shell 测试的绝对计数依赖进程级全局注册表**（`metrics()` 是 `OnceLock`，整个测试二进制共享），且与同文件另一个测试共用 `p_live` ⇒ 已全部改成**增量断言**（`gone0 + 1`、`live0 + 1`、`gone0 + 3`），并加注释说明为什么不能用绝对值（复审核实"当前不 flaky"，但那是把正确性托付给另一个测试的现状）。
5. **同一段文档自相矛盾**：`"Two families … and both are real today"` 与两段之后的 `UNREACHABLE` 冲突（会让运维以为 `missing_config`/`bad_endpoint` 会产生序列）⇒ 已改为"第一族今天产生序列；第二族是防御性的、实测不可达，**不要**为它写规则"。
6. **"the snapshot is never published" 断言过强**：`ConfigStore::apply_snapshot`（副本 hydrate 路径）发布前**不**跑校验 ⇒ 已限定为"该路径不发布；'永不发布'靠的是上游只发已校验快照这个不变量（`store.rs`）"，并注明复审**未找到**可达反例。
7. **`ExclusionReason` 的 serde 形态与 `as_str()` 不一致**（潜在）：派生 `Serialize` 会给 `"BreakerDead"`，而 label 契约是 `"breaker_dead"`，两者无任何东西钉住 ⇒ 已加 `#[serde(rename_all = "snake_case")]`（仓库先例 `tenant_api.rs:149-150`），并在注释里写明"序列化值与 label 不可能漂移"。

### C. 本轮验证
- `cargo fmt --check` clean（python 写入的 Rust 又一次需要 `cargo fmt`，已跑）；
- `cargo test -p hydra-core` ⇒ **253 passed / 0 failed**（+1 例）；
- `cargo test -p hydra-server --features server --test terminate_mode` ⇒ 含新政策断言的用例通过；
- 本机全量门禁 **19 条**（§2z 起新增 e2e contracts / citests / wiring 三类）结果见 §2ad；
- **反向证伪**：soft-disable 跳过逻辑（实测失败后已恢复）；wiring checker 的 8 条（§2ab）。

---

## 2ad. 第四十九轮（收尾）：全量门禁 **19 条 / 18 绿**（唯一红的仍是 D-2）

| 门禁条目 | 结果 |
|---|---|
| fmt --check、clippy×4、compose config、findings disposition、i18n、i18n tests、admin-ui render、e2e contracts、e2e contract tests、**ci wiring**、**ci wiring tests**、两个 `#[ignore]` 条目 | **全 exit 0** |
| `test hydra-core` | **253 / 0**（上一轮 252，+1 = 本轮新增的 soft-disable 用例） |
| `test hydra-server (server)` | **543 / 0 / 1 ignored**（不变：本轮**改**了一个既有测试、没有新增） |
| `test optional + live redis/CH` | **671 / 2 / 3**，两个失败仍是 **D-2**（`admin_api.rs:1799` 的 `empty_body_delete_invalidates_all_local`、`:3138` 的 `too_many_invalidation_keys_are_refused_and_publish_nothing`） |

增量逐条对得上，**没有无法解释的变动** —— 这次按 **section** 解析（上一轮我按范围切片把 optional 数成 674，教训记在 §2y-D）。`OVERALL=RED` 完全由 D-2 造成；本机门禁从 17 条扩到 **19 条**（新增 `ci wiring` 与 `ci wiring tests`）。

---

## 2ae. 第四十九轮（补）：自查发现并修掉新守卫的两处脆弱点 + 3 处陈旧注释

新守卫是在"低配环境"里自举出来的（它第一次运行时唯一的发现就是**它自己**），所以我按"复审会怎么打它"自查了它的谓词。找到两处，都是真的：

1. **`run: |` 块标量被漏读**：`ci.yml` 里有 **11 处**多行 `run:` 块，而我的提取器是**按行过滤**的，只看得到 `run: |` 这一行 ⇒ 任何"只在块里执行"的 artifact 会被**误判为未接**（假阳性，会诱使人去加一个多余的步骤）。
2. **兜底过宽（更严重）**：我原来还写了 `|| ci.includes(f)`，于是"文件里任何位置出现过这个名字"都算已接 —— **包括步骤名（`name:`）与注释**。也就是说这个守卫当时可能被一条注释满足。第 1 条之所以没在今天暴露，恰恰是因为这条兜底在替它掩盖。

**修法**：新增 `extractRunCommands()`，真正解析块标量（取 `run:` 键之后**缩进更深**的行，遇到同级或更浅缩进即停），**并删除兜底** —— 只有真实命令才算"被执行"。改完复跑真实仓库仍然 `OK (everything is executed)`：这本身就是"8 个 artifact 都真的被执行、不是被提到"的证据。

**两条新测试**（`check_ci_wiring.test.cjs` 8 → **11 条断言**）：
- 只在 `run: |` 块里执行的 artifact **必须算已接**（防假阳性）；
- 只在**步骤名**里被提到的 artifact **必须判未接**（防假阴性 / 防宽松兜底）。

**顺带修 3 处陈旧注释**（复审 P2 改了 reason 名字后的残留）：`crates/hydra-core/tests/router.rs` 的 `// → zero_weight`，以及 `crates/hydra-server/src/proxy.rs` 两处列出旧 reason 集合的注释（现在是 `invalid_weight`，且 `soft_disabled` 被刻意跳过）。

**验证**：`node --check` 两个文件；`ci wiring` ⇒ `OK`（8 artifacts）；`node --test scripts/check_ci_wiring.test.cjs` ⇒ pass 1 / fail 0（11 条断言）；`e2e contracts`（224 tokens）与 `i18n`（349 keys）不受影响、全绿；`cargo check -p hydra-core --all-targets` 通过（只改注释）。**未重跑全量门禁**：本轮只改了 JS 与注释，没有改动可观察行为，而第 49 轮那次 19 条门禁（18 绿、唯一红的仍是 D-2）在本轮之前刚刚跑过。

---

## 2af. 第五十轮：**逐条复核 12 项待决策的"前提"**（不决策，但让决策可下）

可判定池子已空、两条复审要么已处置要么在飞，所以本轮做一件不改行为、但让下一步变便宜的事：把 D-1…D-12 每一条的**事实前提**拿当前代码重测一遍。结论：**没有任何一条因前提变化而失效**；**一条（D-7）的成本清单已经漂移**，**一条（D-8）暴露出文档与代码矛盾并已修**；**两条的行号引用过期**，已更正。

| 决策 | 记录的前提 | 本轮复核（实测/读码） | 结论 |
|---|---|---|---|
| D-1 首字节超时故障转移 | `retry_after_connect` 恒 false | `proxy/config.rs:36` 字段存在、`proxy.rs:1220` **确实被读**（`!retry_after_connect` 才走 failover 分支）、默认值 false（`config.rs:6`） | 成立（"恒 false"须理解为**默认值**，已在表里写清）；决策开放 |
| D-2 两例红测试 | `admin_api.rs:1650`、`:2989` | **行号已漂移**：失败点在 `:1799`/`:3138`（函数定义 `:1728`/`:3062`）；本会话最近一次全量门禁仍是 **2 failed / 671 passed** | 前提成立，**引用已更正** |
| D-3 OC-4 行为侧 | 文档已修；`forward.rs:37-39` 列为 refinement | `forward.rs:38` 的 refinement 注释在位；`NoLeader` 变体在 `forward.rs:41`、fail-closed 使用在 `:109`、对租户的响应在 `tenant_api/handlers.rs:1096` | 成立 |
| D-4 / OC-6 DELETE | doc 说"不可区分"，代码 404 vs 204 | `tenant_config_api.rs:172-174` 仍写 "deliberately indistinguishable (no oracle)"；`:252-256` 外来行 ⇒ `Err(ApplyError::NotFound)`，而 `RowNotFound ⇒ WriteOutcome::Deleted`（204） | 成立（注释与代码确实矛盾） |
| D-5 门禁脚本 | 不在版本控制 | `.gitignore:71` 正是 `/.acceptance/`；`git ls-files .acceptance` ⇒ **0** | 成立 |
| D-6 前缀重叠 oracle | `sub_tenant.rs:357-363` | 检测在 `sub_tenant.rs:353`（变体 `:81`、文案 `:107`）；映射成 400 `key_prefix_overlap` 的地方有 **3 处**（`admin/handlers.rs:1639`、`admin/tenant_config_api.rs:757`、`tenant_api/handlers.rs:1006`） | 成立，**引用更正**（"三处映射"是新信息） |
| D-7 W2/W3 裸切片 | 2 条耦合边（`http.rs→admin::metrics` **5 处**；`store.rs`+`db/restore.rs`→`cluster`） | **实测**：`--features db` ⇒ 3 error（`store.rs:26,27`、`db/restore.rs:74`）；`--features http-client` ⇒ 4 error（`http.rs:277/395/432` 的 `admin::metrics`，是 **3 处不是 5 处**）+ **`sink.rs:647` 的 `mpsc` 未解析**（记录里没有这条：`tokio::sync::mpsc` 的导入被特性门住，而 `drain_on_drop` 没门） | **成本清单已漂移**：现在是 **5 个文件 7 处**；决策开放，但"要不要修"应按这个数来估 |
| D-8 edge `/metrics`（**P1**） | 官方拓扑下恒 401 | `admin/mod.rs:855-870`：edge 模式只放行 `/healthz`/`/readyz`，**`/metrics` 走与别处同一条 token 门禁**，且注释写明这是**有意**的（起因："指标暴露面取决于角色"，而官方拓扑把 edge 管理口绑在 `0.0.0.0`）；`environment/docker-compose.cluster.yml:90-110` 的 `hydra-edge` **只注入 `HYDRA_CLUSTER_TOKEN`**，两个 control 服务都有 admin token（`:55` 内联、`:75` 用 `<<: *control-env`） | 前提**精确成立**（现在有了逐行证据）；并修掉与代码矛盾的 `cluster.md:17`（见下） |
| D-9 L2 抽象 | `AuthCache::l2` 是具体类型 | `http.rs:83` `l2: Option<Arc<crate::redis::auth_cache::RedisAuthL2>>`（非 redis 构建下 `:87` 是 `Option<()>`） | 成立 |
| D-10 sub-tenant 重名 | 管理面 409 | `admin/sub_tenant_write.rs:41-51` 自述 "duplicates are 409" | 成立 |
| D-11 `matching_provider` | 只 Warn | `config.rs:324-339`：`role.matching_provider.is_some()` ⇒ Warn（含"CANNOT match，前置门禁在路由之前"的理由） | 成立 |
| D-12 changelog SHA | 三个 SHA 不在本仓库 | 复测 `git cat-file -t 8d6b97c/bccccb2/40f5912` ⇒ 三者仍 `Not a valid object name` | 成立 |

### 顺带修掉的一处 docs↔fact（可判定，无需决策）
`dev-docs/cluster.md:17` 的角色表把 edge 的管理 API 写成 "❌（仅 `/metrics` `/healthz` `/readyz`）" —— 而按 `admin/mod.rs:855-870`，`/metrics` 与别处一样要 admin token，**而官方 compose 的 edge 没有这个 token**（上面 D-8 的逐行证据），所以那句话读起来像"edge 的指标抓得到"，实际是 401。已改成：**无 token 时 `/metrics` 也是 401；只有 `/healthz`/`/readyz` 免鉴权**，并写清"要让抓取器拿到指标必须先做一次取舍"。这条改的是**当前事实**，不是替 D-8 做决定。

### 本轮验证
- `cargo check -p hydra-server --no-default-features --features db` ⇒ 3 error（清单见 D-7 行）；`--no-default-features --features http-client` ⇒ 4 error；两者都是**测量**，不是推断；
- 其余各行是读码/`git` 实测，命令与输出已抄进上表；
- **未改任何行为**：本轮只改 `cluster.md` 一处文档与计划表格（外加更正引用）。

---

## 2ag. 第五十一轮：处置第十三路复审（wiring 守卫 + reason 策略）—— **1 条 P1（我上一轮只修了一半）** + 1 条 P2 + 5 条 P3

### A. P1（接受 —— 而且它证明我上一轮"以为修好了"只修了一半）
复审给的是 `check_ci_wiring.cjs` 的 `|| ci.includes(f)`（对**整个 ci.yml 文本**做子串搜索）：注释、步骤名都能让某个脚本被判定为"已被执行"。我上一轮已删掉那半条并加了"步骤名里提到不算"的测试 —— **但复审的探针在本轮复现时仍然通过（exit 0）**：它把真实步骤替换成

```yaml
run: echo wiring ok   # scripts/check_ci_wiring.test.cjs is mentioned here only
```

文件名落在 **`run:` 行的行内注释**里，而我的提取器把整行（含注释）当成一个命令字符串，`includes` 照样命中。**修法两层**：①`stripInlineComment()` 去掉行内注释（引号内的 `#` 不算）；②`invokes()` **要求文件名是被 runner 调用的参数**（`node|nodejs|npx|bash|sh|python3|python|deno|bun|tsx` 加可选 flag），于是 `grep -q foo scripts/x.test.cjs` 这种"只是读了文件"也不再算执行。

**证据（同一探针，修前/修后）**：修前 `exit 0` / `OK (everything is executed)`；修后 **`exit 1` / `UNWIRED  scripts/check_ci_wiring.test.cjs is never executed by ci.yml`**。真实仓库仍绿（`OK`，8 个 artifact 都确实是被调用的）。新增 3 条测试（11 → **14 条断言**）：行内注释不算、`grep` 读文件不算、`#[should_panic] #[ignore]` 同行也算 ignored。

### B. P2（接受 —— 本轮唯一改变语义的发现）：`invalid_weight` 在真实系统里**不可达**
我自己核过证据链：`migrations/0001_init.sql:13` 是 `weight INTEGER NOT NULL DEFAULT 1 CHECK (weight >= 0)`；内存中的 provider 全部由这些行构造（`ConfigStore::load`）；`validate_and_log` 的 issue 全是 `Warn`、fatal 只剩 endpoint 一条 ⇒ `weight < 0` **只可能来自手工构造的 `ConfigData`**（单元测试）。而我上一轮却把它写进了 `ops.md` §9.1 的告警表达式 —— 那等于给运维一条**永远不可能触发**的规则，还暗示"存在会被接受的坏配置"。

**修法**：表达式改为 `reason=~"no_key|breaker_dead"`；`invalid_weight` 与 failover 循环那一族一起显式标为"防御性、不可达"；`metrics.rs` 的 help 文本与文档、`model.rs` 的变体文档都注明"schema 禁止、只在手工构造时出现"。**与 ops.md 既有做法对齐**：那里早有一条对 `hydra_retries_total{stage="connect"}` 的"标出哪些系列现实不会产生"，我这条当时没有同样诚实。

### C. P3 五条（全部接受）
1. `validate.rs:363-364` 的注释称"validator 与 router 在 `weight > 0` 上一致" —— 本轮刚把 router 拆成 0/<0 两种原因 ⇒ 已更新，并说明为何 DB 里不会出现负权重。
2. 我新加的 `candidate_skipped("p_soft", "invalid_weight") == 0` **永远无法失败**（`p_soft` 只被赋过 weight 0），而且同批其它断言用的是**增量**、自己违反文件里刚写下的约定 ⇒ 已删除；负权重那一侧的覆盖由 core 测试承担（那里可以构造）。
3. `ops.md:86-89` 说"CI **从不**运行 `--ignored`（`grep -c` = 0）" —— 第 15 轮加了两个步骤后已是**假陈述** ⇒ 改成事实，并指出 `check_ci_wiring` 会持续守住它。
4. 下限太弱：`MIN_SCRIPTS=5 / MIN_CRATE_TESTS=20 / MIN_E2E_SPECS=1` 相对实际的 **8/54/3** 差一个数量级 —— "删掉 2/3 个 spec"都不会报 ⇒ 收到 **6/45/3**（并写明"下调下限要与删除 artifact 同时做"）。**改完立刻暴露了我自己测试骨架的假设**（21 个 crate 文件、1 个 spec）⇒ 骨架同步改成 45/3。
5. `#[ignore]` 检测漏掉"属性不在行首"的合法写法（`#[should_panic] #[ignore]` 同行、`cfg_attr(..., ignore)`）⇒ 改成"任何以 `#[` 开头且含 `ignore` 的行"，两个方向都覆盖、且不把 `//` 注释算进来。

### D. 顺带修的三处标签语义（复审 P3-7）
`router.rs` 的 catalog 注释、`proxy.rs` 的 passthrough 过滤、`admin/handlers.rs` 的 catalog `online` 都用 `weight <= 0` 并称其"soft-disabled" —— 行为与新 step 4 一致（两者都不可路由），但标签语义已经分家（`0` 是**有意**、`< 0` 是**非法**）⇒ 三处都改成"不可路由（故意的 0 或非法的 <0）"。

### E. 本轮验证
- `cargo fmt --check` clean；`cargo test -p hydra-core` ⇒ **253 passed / 0 failed**；`terminate_mode` 目标用例通过（删掉那条空洞断言后仍覆盖 soft-disable 政策）；
- `node scripts/check_ci_wiring.cjs` ⇒ 真实仓库 `OK`；`node --test scripts/check_ci_wiring.test.cjs` ⇒ **14 条断言全过**；
- **P1 探针修前 exit 0 / 修后 exit 1**（见 A）；
- 复审"核对为正常"里我特别记两条：它逐条打印了 40 个 `run:` 匹配行、确认多行块的内容行不会被误当命令；它核对了脚本名之间**不存在子串别名**（`check_i18n.js` 不会被 `check_i18n.test.cjs` 的完成状态满足）—— 正是我担心的另一个方向。

### F. 下一轮的候选（本轮实测发现，未做）
`npm view @playwright/test version` 在本机**能联网**（返回 `1.63.0`），且 `~/.cache/ms-playwright` 里**有 chromium**（1234 + headless shell）⇒ **JS 版 Playwright runner 也许能装起来并真的跑 e2e**（npm 的 cache 是只读的，需把 `npm_config_cache` 指到工作区内、并用 `--no-save` 装进仓库的 `node_modules`，跑完删掉）。若成立，那三个"从未本地运行"的用例（T2.2c/T2.2d/T9.5c）就能真的执行一次，把本项目最大的一笔验证债清掉。**下一轮第一件事就是试它。**

---


## 2ah. 第五十二轮：**第一次真的把那套 e2e 跑起来** —— 两个"从未运行"的用例本来就是红的，外加一个新守卫

### A. 先把"本地跑不了"这个结论推翻
之前每一轮都记着"本地跑不了：需要 Chromium + 新建并 seed 的实例"。本轮实测**推翻了它**：本机有 `~/.cache/ms-playwright/chromium-1234`（含 headless shell），npm **也能联网**（`npm view @playwright/test version` ⇒ `1.63.0`）；唯一的障碍是 npm 的 cache 目录只读（EROFS）⇒ 把 `npm_config_cache` 指到工作区内即可。

版本匹配：CI pin 的是 `@playwright/test@1.55.0`（它要下载 chromium-**1187**），而缓存里是 **1234**；实测 **`1.62.0` 正好对应 1234** ⇒ 一个字节的浏览器都不用下载。runner 装在被 gitignore 的 `.acceptance/tmp-pw`（仓库**没有**也**不**应该有 root `package.json`，而且 `node_modules` 并不在 `.gitignore` 里 ⇒ **绝不能**装到仓库根目录）。用临时 SQLite + 端口 18080/18081（避开你的 a/b/c 栈）起一次性实例，seed 后跑套件，`trap` 收尾。

### B. 头条：那三个"从未运行"的用例里，**有两个本来就是红的**
第一次运行的结果：`T9.5c` **通过**，而 `T2.2c` / `T2.2d` **失败** —— 失败在它们的 API 脚手架：

```
create failed: {"status":400,"json":{"error":{"code":"invalid_json",
  "message":"failed to parse request body: missing field `created_at` at line 1 column 118"}}}
```

即：这两个用例（本会话为 §2t 的 P2-1 与 §2v 的 P2-2 写的回归守卫）用 `api('POST', '/providers', { body: { … } })` 建夹具，而 handler **直接反序列化进实体结构体**，`created_at`/`updated_at` 是**必填**字段（UI 的 `collectBody` 正是因此会在 create 时补 `""`）。**它们第一次进 CI 就会红。**
- **修法**：给两个 body 补 `created_at: ''` / `updated_at: ''`（并在原处留注释说明"这是第一次真的运行才发现的"）。
- **静态守卫抓不到这类问题**：`check_e2e_contracts` 只看选择器/导航键/路由，看不到 payload 契约。这是"选择器正确 ≠ 语义正确"的实证，也解释了为什么"能静态核对"不能替代"真的跑一次"。
- 顺带核查全仓：`api('POST'…)` 只有这 2 处缺时间戳；第三处 `POST /provider-models` **不需要**（`ProviderModel` 结构体没有时间戳字段 ✓ 已核对）。

### C. 全量套件第一次本地运行：**18 / 18 通过**（25.5s）
三个 spec 全绿：既有 15 个用例 + 本轮那 3 个 ⇒ 除了上面那两处，没有别的潜在破损（此前它们"在 CI 里绿"是靠**从不运行**换来的）。

### D. 新能力：`scripts/e2e-local.sh`（可复现的配方）
一次性实例（临时 DB、临时端口、`trap` 收尾）+ runner 装到 `.acceptance/tmp-pw`（可复用）+ 浏览器走缓存；任意 `playwright test` 参数可透传（`-g "T2.2c|T2.2d"`），`KEEP=1` 保留现场。**这就是把"CI-only"变成"本地可复现"**，本轮之前这个仓库最大的一笔验证债：现在一条命令、26 秒。

**而且它可以与 CI 完全一致**：我第一次跑用的是 `1.62.0` + 共享缓存里的 `chromium-1234`（当时没注意到这个 checkout 里**早就**有 `.acceptance/pw-browsers/chromium-1187` —— 正是 CI pin 的 `@playwright/test@1.55.0` 要的那一版）。脚本现在**优先选 CI 的那一对**（1.55.0 + `.acceptance/pw-browsers`），选不到才回退到 1.62.0 + `~/.cache/ms-playwright`；`PW_VERSION`/`PLAYWRIGHT_BROWSERS_PATH` 均可覆盖。整轮 18/18 已在**CI 同一对**下复验通过（脚本自报 `playwright 1.55.0 (matches the CI pin: yes)`）。

### E. 新守卫：把"必填时间戳"变成静态可查的契约
`check_e2e_contracts.cjs` 现在还会：从 `crates/hydra-core/src/model.rs` 解析出**哪些实体结构体把 `created_at`/`updated_at` 声明为非 `Option` 的 `String`**，再把 spec 里 `api('POST'|'PUT', '<resource>', { body: { … } })` 的 body 抽出来（花括号配平），若该资源对应这类结构体而 body 缺字段 ⇒ 报错。**刻意做窄**：只管这两个字段、只对声明了它们的结构体生效（"每个字段都必须发"是错的：很多字段是 `Option`/`#[serde(default)]`），等下一个必填字段咬人时再放宽。

**反向证伪（实测）**：把我刚补上的那两行从 T2.2c 的 body 里拿掉 ⇒ 立刻报两条 `POST /providers omits created_at / updated_at, which Provider requires`。配套 **4 条测试**（含"没有时间戳的结构体不得要求它"这条反向用例）。为让聚焦夹具不被"token 下限"误伤，下限改为可用 `E2E_CONTRACTS_MIN_TOKENS` 覆盖 —— 而**下限自己那条测试显式传值**，否则覆盖会把下限一起关掉（**这个坑我在实现时真踩了一次**，被自己的测试抓出来）。

### F. 顺带修一处仓库卫生
`.gitignore` 补上 `test-results/` 与 `playwright-report/`：`.dockerignore` 早就有它们，而 `.gitignore` 没有 ⇒ **本地跑一次失败的用例就会让 `git status` 变脏**，而"脏了就不容易发现真正的脏东西"。

### G. 本轮验证
- `scripts/e2e-local.sh` 端到端跑通：**18 passed**（25.5s，`1.62.0`+1234 与 **CI 同款 `1.55.0`+1187** 各跑一遍都通过；定向 `-g T9.5c` 亦通过）；
- `node scripts/check_e2e_contracts.cjs` ⇒ `OK (229 selector/nav tokens across 3 spec files)`；其测试 ⇒ **15 条断言全过**；
- `node scripts/check_ci_wiring.cjs` ⇒ 仍 `OK`；`git check-ignore test-results` ⇒ 命中；
- **未跑 cargo 全套**：本轮唯一的 Rust 相关动作是 `cargo build --bin hydra`（为 e2e 起实例），源码未改。

---


## 2ai. 第五十三轮：用新能力把两处**只在静态/脚手架层验证过**的修复补上端到端证据

上一轮让 e2e 能在本地真跑（CI 同款 runner + 浏览器）。本轮就用它，把两个"我改了、但只在静态守卫或 harness 层验证过"的修复做成**端到端**断言，并各自**反向证伪**。

### A. 新增 `T2.3b`：UI 编辑不再丢租户的旧版证书路径（验证 §2x-B）
- **夹具**：用 `openssl` 现场生成一对**真** PEM 到临时目录（服务端在写入时会**读这些文件并封存内容**，所以必须是可解析的真证书），然后**只用路径字段**（不带 `cert_pem`）创建租户 —— 这正是 pre-0007 行的形状。
- **断言链**：①创建成功、`cert_file` 落库；②打开该行 Edit，**表单必须带着这两个路径**（`toHaveValue`）；③改名保存；④读回：`name` 变了，而 `cert_file` / `cert_key` **仍在**（修复前是 `null`）。
- **反向证伪（实测）**：把 `CRUD.tenants.fields` 里那两条字段删掉 ⇒ 测试**在预期的那条断言上失败**（`toHaveValue(cert_file)` 超时，输出见本节 E）。
- 这补上了 `check_e2e_contracts`（只证明"表单里有这两个字段"）与 harness（只证明"编辑时预填"）够不到的那一段：**真实的 PUT 往返**。

### B. 新增 `T2.8`：429 保留 code / 消息**与 Retry-After 提示**（验证 §2x-A）
- 用 `page.route` 把 `/api/v1/stats/usage` 打成 `429` + `{"error":{code,message}}` + `Retry-After: 3`，进 Stats 页，断言最后的 toast 同时含 `too_many_failed_attempts`、服务端消息与 **`retry in 3s`**，且**不含** `429 429`。
- **反向证伪（实测）**：把 `parseRetryAfter(resp.headers && …)` 换成 `null` ⇒ 断言失败，失败信息正是 `unexpected value "…429 too_many_failed_attempts: …"`（**没有** retry 提示）—— 顺带证明 code + message 那半条渲染确实在工作。

### C. 顺带抓到我自己上一轮交付物里的一个**盲区**（已修）
`scripts/e2e-local.sh` 原来判断"要不要重建二进制"只看 `crates/**/*.rs`。但二进制是**编译期嵌入**的：`include_dir!("admin-ui")`（`admin/static_files.rs:30`）+ `sqlx::migrate!("./migrations")`（`db.rs:71`）⇒ **只改 `admin-ui/**` 而不重建，`/admin/*` 服务的仍是旧资源**，e2e 就会去测树里并不存在的代码，而"测试通过"会给出**虚假的信心**。修法：staleness 检查覆盖 `crates/** + admin-ui/** + Cargo.toml` 的 `.rs/.toml/.sql/.js/.html/.css`，并**打印是哪次改动触发了重建**。本轮两处 falsification 都因此真的重建了（各 1.7s）。

### D. 一次自伤（记下来，因为它浪费了整整一次调用）
我第一次写 falsification 探针时，用带**嵌套量词**的正则去删字段条目（`(?: *[^\n]*\n)*?`），在 72KB 的文件上**灾难性回溯**，把那次调用卡到 600s 上限并被 SIGTERM（`app.js` 一度留在探针状态，已确认最终树是干净的：两处探针都恢复、`grep FALSIFICATION` = 0）。改成**逐行**定位（找到含 `{ name: "x"` 的行，删到首个以 `},` 结尾的行为止）后瞬时完成。**教训：会改仓库文件的探针按行做，不要用嵌套量词正则。**

### E. 本轮验证（逐条是实测输出）
- `scripts/e2e-local.sh` 全量 ⇒ **20 passed (29.9s)**（CI 同款 `1.55.0` + `chromium-1187`）；
- `T2.3b` 证伪：`✘ … T2.3b …` + `expect(locator).toHaveValue … > 646 | await expect(page.locator('#modal-root [data-field="cert_file"]')).toHaveValue(certFile);`；恢复后 `✓`；
- `T2.8` 证伪：`✘ … Expected pattern: /retry in 3s/ … unexpected value "Failed to load usage stats429 too_many_failed_attempts: too many failed attempts from this address"`；恢复后 `✓`；
- `check_e2e_contracts` ⇒ `OK (248 selector/nav tokens across 3 spec files)`（新增两个用例把 token 数从 229 抬到 248，且全部能对上 UI 源码）；`check_ci_wiring` ⇒ `OK`；
- **未跑 cargo 全套**：本轮除 `cargo build --bin hydra`（为 e2e 重建嵌入资源）外没有改动 Rust 源码。

---


## 2aj. 第五十四轮：又抓到**三个从未被运行的测试套件** —— 并把守卫的视野扩到全仓

起因是自查：`check_ci_wiring.cjs` 只认识 `scripts/` 与 `crates/<crate>/tests` 两处。于是我列了一遍"这个仓库里测试都在哪"，结果**三个真套件从来没被任何东西跑过**。

### A. 三个套件，全部健康，全部没人跑
| 套件 | 规模 | 被谁跑过 | 首次真跑的结果 |
|---|---|---|---|
| `integration/test_crud.py` | 25KB、stdlib-only，覆盖 7 类实体完整 CRUD + 边界（掩码/必填/409/400/401/404）+ 非 CRUD 端点 | **没有任何东西**（CI 里 `integration` 只出现在一句注释里；`run.sh` 需要 Docker + 预建镜像） | **116/116 通过** |
| `integration/e2e_proxy_test.py` | 自起 mock auth + mock LLM + hydra，走完整代理链路 | **没有任何东西** | **通过**（`SUCCESS ✓ proxy returned mock LLM response`，含 usage 提取） |
| `tools/hydra-cli`（`npm test`） | **已发布**的 npm 包（`hydra-admin` v1.0.2）自己的 typechecked 套件 | **没有任何东西**（`sdks` 作业只跑 go/ts/py） | **16 tests / 0 fail** |

三个都是绿的 —— 这恰恰是重点：**没人知道**它们绿不绿，因为没人跑过。这正是本轮要修的东西。

### B. 顺带发现：集成夹具**在本机根本跑不起来**（也就解释了为什么它从没被跑）
`integration/*.py` 把端口**硬编码**成 8080/8081/9090/9091，而本机这两个端口被你的开发栈占着（`ss` 显示 127.0.0.1:8080/8081 在听）⇒ 第一次运行直接 bind 失败。已把四个端口改成**环境变量可覆盖**（默认值不变，CI 里照旧），`mock_llm.py`/`mock_auth.py` 的监听端口同理。改完：`test_crud.py` 116/116 ✓、`e2e_proxy_test.py` 全链路 ✓。

### C. 我在这一步**自己制造并修掉**的一个 bug（值得记）
给两个 mock 加端口覆盖时，我把 `port = int(os.environ.get(...))` 插到了使用它的 `print` **之后** ⇒ 两个 mock 都以 `NameError: name 'port' is not defined` 退出 ⇒ 代理请求变成 `503 auth_upstream_unavailable`。我一开始以为是产品缺陷，直到把 hydra 自己的日志（`WARN hydra_server::http: auth upstream request failed …`）和 mock 的日志对照才定位到**是自己的补丁**。教训：**`ast.parse`/`node --check` 只是语法检查，不是运行**；这次我随后真的把每个 mock 跑了一遍（`[mock-llm] listening on 0.0.0.0:19190` ✓）才继续。

### D. 交付物
1. **`integration/run-crud-local.sh`**（新，tracked）：一次性实例（临时 DB、可覆盖端口、`trap` 收尾）+ 跑 `test_crud.py`；本地与 CI 用**同一条命令**。与 `scripts/e2e-local.sh` 同形。
2. **CI**：`sdks` 作业新增 `Admin CLI tests`（`npm ci && npm test`，它有 lockfile 所以用 `ci` 而非 `install`）；新增 **`integration` 作业**（build 二进制 → `run-crud-local.sh` → `e2e_proxy_test.py`，**不需要 Docker**），CI 作业数 7 → **8**。
3. **守卫扩视野**：`check_ci_wiring.cjs` 现在按 `TEST_GLOBS`（`*.test.{ts,js,cjs,mjs}`、`test_*.py`/`*_test.py`、`*_test.go`、`*.spec.*`）**全仓发现**测试文件（跳过 `target/.acceptance/node_modules/dist/dist-test/dev-docs/…`），并要求每个都有 runner：被命令点名、或它的**目录**被某个脚本引用、或某个 step 在它的 `working-directory` 里跑了测试运行器（三种形态都接受，各有测试）。实测：真实仓库发现 **6 个**（含 `integration/` 两个、`hydra-cli`、go/py/ts SDK）并全部覆盖 ✓。
   两个实现细节也踩了坑并修掉：step 解析原以为 `working-directory:` 一定在 `run:` **之前**（仓库是那样写，但不该假设）⇒ 改成按 YAML 列表项切块、顺序无关；新加的下限对小骨架过严 ⇒ 改成可由 `CI_WIRING_MIN_OTHER_TESTS` 覆盖，并**给下限自己加了一条显式测试**。
4. **顺手更正一处陈旧文档**：`integration/test_crud.py` 的模块 docstring 还写着"list masks → first4…last4; `?reveal=1` → plaintext"，而同一文件的**测试体**（`:273-301`）早就断言"ALWAYS masked、`?reveal=1` no-op"（P1-5）⇒ docstring 已改成事实。没人发现它，正因为没人跑（也不会去读）这个文件。

### E. 本轮验证
- `integration/test_crud.py` ⇒ **116/116**（经新脚本，一次性实例）；
- `integration/e2e_proxy_test.py` ⇒ **PASSED ✓**（自起 mocks + hydra 全链路）；
- `tools/hydra-cli`：`npm ci && npm test` ⇒ **16 tests / 0 fail**（本轮实测，安装需把 `npm_config_cache` 指到工作区内）；
- `node scripts/check_ci_wiring.cjs` ⇒ `OK`（6 个其它位置的测试全部有 runner）；其测试 ⇒ **18 条断言全过**；
- 本机全量门禁 **21 条**（新增两条 integration 条目），结果见 §2ak。

---


## 2ak. 第五十四轮（收尾）：全量门禁 **21 条 / 20 绿**（唯一红的仍是 D-2）

| 门禁条目 | 结果 |
|---|---|
| fmt --check、clippy×4、compose config、findings disposition、i18n、i18n tests、admin-ui render、e2e contracts、e2e contract tests、ci wiring、ci wiring tests、两个 `#[ignore]` 条目 | **全 exit 0** |
| `test hydra-core` | **253 / 0** |
| `test hydra-server (server)` | **543 / 0 / 1 ignored** |
| `test optional + live redis/CH` | **671 / 2 / 3**，两个失败仍是 **D-2** |
| **`integration crud (disposable)`**（本轮新增） | **`=== 116/116 passed ===`** |
| **`integration proxy e2e`**（本轮新增） | **`[e2e] all stopped. PASSED ✓`** |

即：两个**全新**的门禁条目一次通过，而它们守的是三个此前从不运行的套件。本机门禁 19 → **21 条**，与 CI 的 8 个作业一一对应（除 `ui-e2e`，它由 `scripts/e2e-local.sh` 覆盖，见 §2ah）。

---


## 2al. 第五十五轮：**真的去用那个已发布的 CLI** —— `update` 每个分组都是坏的（打桩测试看不见）

上一轮把 `tools/hydra-cli`（npm 包 `hydra-admin`，**已发布** v1.0.2）的测试接进 CI 并确认 16/16 通过。本轮更进一步：**把 CLI 真的当用户那样用一遍** —— 建一次性实例，对它跑每一条命令。

### A. 调查方法
`npm run build` 出 `dist/cli.js` → 起一次性实例（临时 DB、端口 18100/18101）→ `node dist/cli.js --base-url … --token …` 逐条跑：8 个系统命令 + 7 个实体分组。

### B. 结果：读路径全好，**写路径的 `update` 全坏**
- 8 个系统命令（`health`/`reload`/`metrics`/`concurrency`/`breaker`/`auth-cache`/`stats`/`cluster`）与 7 个分组的 `list` **全部 exit 0** ✓；
- `providers create` ✓、`get` ✓、`delete` ✓（带确认提示）；
- **`providers update p-cli --weight 5` ⇒ HTTP 400**：
  ```
  {"error":{"code":"invalid_json","message":"failed to parse request body: missing field `id` at line 1 column 44"}}
  ```
  原因是**契约错配**（与 §2t/§2x 修 UI 时同一个类）：admin API 的 PUT 是**整体替换**（`UPDATE provider SET <每一列>`，handler 直接把 body 反序列化进实体），而 CLI 只送用户敲的那几个 flag（+ `created_at`/`updated_at`）。所以 **每一个带 `update` 子命令的分组都坏**（`provider-models`/`provider-keys`/`tenants`/`limit-roles` …），而 CLI 自己的 16 条测试**看不见**：它们用打桩 HTTP 只测 `client.ts` 的传输层，没有任何测试构造真实的 update body 再送进真服务端。

### C. 修法：读—改—写（并把"读回来是掩码"的字段挡掉）
1. `mergeForUpdate(def, current, partial)`：先 `GET` 当前记录，把用户给的 flag 覆盖上去，再 PUT 整体。新增纯函数便于测试。
2. **危险点（我先想到再写）**：`provider-keys` 的读回**是掩码**（`first10…last4`）。若把读回的 `api_key` 合并回去，就会**把掩码写成真的 key** —— 比原来的 400 更糟。所以加了 `READBACK_UNSAFE`：这类字段**永不从读回合并**，必须由操作者显式给出；没给就**本地报错**而不是发一个坏 body：
   `Error: provider key update needs --api-key: the server masks that value on read, so it cannot be carried over from the current record.`
3. 顺带剥掉响应专用的装饰字段（`snapshot_stale`、`has_access_token`）—— 今天 serde 会忽略未知字段，但**依赖这一点正是契约漂移的开始**。
4. `package.json` 的 test 脚本原来是硬编码 `dist-test/test/client.test.js`，新增测试文件不会被跑；改成 `dist-test/test/*.test.js` 并新增 `test/merge.test.ts`（4 条：字段保留 / 装饰字段剥除 / **掩码不得覆盖密钥** / 非对象读回不抛错）。

### D. 端到端验证（真实例，修前/修后）
| 动作 | 修前 | 修后 |
|---|---|---|
| `providers update p-cli --weight 5` | `HTTP 400 … missing field \`id\`` | `✓ provider p-cli updated` |
| 更新后其余字段 | —（写没发生） | `{id: p-cli, key: cli, name: CLI Provider, endpoint: http://127.0.0.1:9/, weight: 5}` —— **全部保留，只有 weight 变了** |
| `provider-keys update k1`（不给 `--api-key`） | —（同样 400） | 本地拒绝：`needs --api-key …`（**不会把掩码写进去**） |
| `provider-keys update k1 --api-key sk-brandnew-…-9876` | — | `✓`，读回掩码 `sk-brandne*************9876` —— 新 key 的 last4（旧的 `1234` 不见了）⇒ 真的换了密钥而不是被掩码污染 |
| `tenants update t1 --name "T1 renamed"` | — | `✓`，`domain`/`auth_url`/`enabled` 全部保留 |

`tools/hydra-cli` 的包内测试：**20 tests / 0 fail**（16 + 4）。本机门禁新增 `cli tests (hydra-admin)` 条目（21 → **22 条**）。

### E. 顺带记录两条**能力缺口**（不改，留给决策/后续）
CLI 的 `auth-cache` 与 `reload` **没有任何选项**（`--help` 只有全局选项），因此：
- `DELETE /api/v1/auth/cache` 的**租户级**失效（`{tenant_id}`，管理 UI 里就有这个功能）**无法从 CLI 发起**；
- `POST /api/v1/reload` 的 **`?force=1`**（一个**安全相关**的运维覆盖）**无法从 CLI 发起**。
两条都是"API 支持、CLI 到不了"，属于能力缺口而非缺陷（都是纯增量的 flag）。已如实记录，未擅自扩大已发布包的接口面。

### F. 我自己的两个错报（也记下，免得当成发现）
第一轮探测时我用错了 flag：`auth-cache --tenant`（真名不存在）、`reload --force`（不存在）、`tenants auth-test --url`（本是**位置参数** `<auth-url>`）。三条都是**我的调用错**，不是 CLI 的缺陷 —— 但它们恰好暴露了 E 里那两条真实缺口（前两条 flag 确实不存在）。

---


## 2am. 第五十六轮：把上一轮的 CLI 修复**在每个分组上**验完，并顺手核掉三处"看起来可疑"的地方

上一轮修好了 CLI 的 `update`（读—改—写），但只在 3 个分组上端到端验过。本轮把它**验完**，另外核了三处此前没碰过的面。

### A. `update` 修复：6 个支持更新的分组**全部**通过
| 分组 | 动作 | 结果 |
|---|---|---|
| `providers` | `update p --weight 5` | ✓，其余字段全保留（上一轮） |
| `provider-models` | `update m1 --status 0` | ✓ |
| `provider-keys` | 不给 `--api-key` ⇒ 本地拒绝；给新 key ⇒ 掩码换成新 key 的 last4 | ✓（上一轮） |
| `tenants` | `update t1 --name …` | ✓，`domain`/`auth_url`/`enabled` 保留（上一轮） |
| `limit-roles` | `update r1 --limit-count 42` | ✓，读回 `{window: m, limit_count: 42, matching_tenant: t1, name: R1}` 全部保留 |
| `provider-key-bindings` | `update b1 --disabled` | ✓ |
（`tenant-providers` / `tenant-models` 的 CLI **本来就没有** `update` 子命令 —— 它们与 API 一致：这两个关联表没有 PUT。）

### B. 三个 SDK 的**真实请求路径**核对（"打桩测试藏漂移"的另一半）
上一轮 CLI 的教训是"打桩测试看不见真实契约"。于是本轮检查三个 SDK 源码里（**不含测试**）真正发出的路径：
- `hydra-go` / `hydra-py` 客户端代码只有一处活路径：`POST /tenant/{tenant_id}/api/v1/auth/cache/invalidate`；
- 该路由**存在**（`tenant_api`：`POST /auth/cache/invalidate`，见 `tenant_api/mod.rs:91` 的文档与 `invalidate_per_min` 配置）；
- `hydra-ts` 源码里没有自建路径（只有类型与文档里的示例行）。
⇒ **未发现漂移**（先前一轮已经修过"SDK 指向已删除路由 + 错端口"）。这条记录的价值在于：它是**主动核过**的结论，而不是"没看见就当没有"。

### C. `bin/hydra`：一个真的**脚印坑**（已用文档修掉）
`environment/Dockerfile:39` 是 `COPY bin/hydra /usr/local/bin/hydra` —— 它**原样打包**那个文件，而 `bin/hydra` 由 `build.sh` 的 [2/3] 步从 `~/.cargo/global-target/…/release/hydra` 拷来。本机实测：`bin/hydra` 还是 **9 月 15 日**的 32MB 产物（`.gitignore:63` 已忽略 `/bin/`，所以它不在版本控制里、也不会随克隆带过去）。
后果：**谁在几周前跑过一次构建、今天直接 `docker build`，就会把那个旧二进制打进镜像**。`build.sh` 每次都重新 cross-compile 并覆盖它，所以走 `build.sh`（`deployment.md` 推荐的就是它）是安全的 —— 但文档没有把"别绕开它"写出来。已在 `deployment.md` 的镜像段落补上 ⚠ 警示（含本机实测的日期证据）。
**为什么不加代码守卫**：`build.sh` 已经每次都覆盖，真正需要约束的是"人手敲 docker build"这个动作，文档是合适的位置；而在 Dockerfile 里比时间戳需要把源码塞进构建上下文，代价大于收益。

### D. 我自己的重复错误（第三次了，记下来）
本轮 + 上一轮我**反复用错的 flag**：`auth-cache --tenant`、`reload --force`、`tenants auth-test --url`、`limit-roles --tenant-id`、`limit-roles --count` —— 全是**我的调用错**（真名分别是位置参数 `<auth-url>`、`--matching-tenant`、`--limit-count`，而 `auth-cache`/`reload` 确实没有那些选项）。每次都要多花一次调用才发现。教训：**先读 `--help` 再调用**（CLI 自己的 help 就是权威），别按 API 的字段名猜 flag。

---


## 2an. 第五十七轮：把"文档里点名的东西"核一遍 —— 指标名审计 + 一个**参数名骗人**的等待循环 + `init.py` 首次端到端

### A. 指标名审计（docs ↔ 代码）：**没有真缺陷，但我自己的检查先是错的**
做法：抽代码里出现过的 `hydra_*` 名（**57 个**）与 dev-docs 里点名的名（**65 个**），求差集。

**第一版检查给出假阳性**：`hydra_sni_host_mismatch_total` 被判成"未注册"，实际它**实现了** —— 只是通过**常量**注册（`tls.rs:398` 的 `MISMATCH_METRIC`、`metrics.rs:234` 的 `SNI_MISMATCH_METRIC`），而我只匹配 `register_*!("字面量")` ⇒ **是我的检法错了，不是代码错了**。改成"这个名字在代码里出现过"才拿到可用信号（这正是本会话反复出现的模式：先怀疑代码，再核对是不是自己的测量错了）。

剩余 12 条人工分诊后**全部不是缺陷**：正则截断（`hydra_listener_`、`hydra_request_duration_seconds_`、`hydra_registry_`…）、crate 名（`hydra_core`/`hydra_server`）、**已改名的旧名**（`hydra_proxy_listener_tls` → 现 `hydra_listener_*`）、**明确写"不存在/被否"的提及**（`ops.md` 的 "there is deliberately no `hydra_proxy_listener_*` alias"；`design-tenant-api.md:907` 的 rejected 行）、以及**提案**（`hydra_model_extract_miss_total` 原文是"建议先加一个…观测一周"）。

两条真候选：
1. `dev-docs/aegis/plans/2026-09-09-auth-insufficient-balance-402.md` 把 `hydra_auth_decisions_total` 写成**单数** `hydra_auth_decision_total`，而该文件同时断言"观测面已存在"（确实存在，只是名字错一位）⇒ 已就地更正并注明。
2. changelog 声称的 `hydra_control_writer_ahead_total` 在本仓库不存在 —— 但该 changelog 记录的是**生产仓库**的提交（D-12：`8d6b97c` 等在本仓库不存在）⇒ 属 D-12 家族，不另立条目。

**刻意没做成守卫**：这条检查要能不误报，需要一份 allowlist（截断名 / 旧名 / 否定提及 / 提案），而会腐烂的 allowlist 正是本会话反复批判的东西。理由留在本节，供后人判断该不该做。

### B. 一个"参数名骗人"的等待循环（已修 + 实测前后）
`environment/init.py` 的 `wait_health(timeout=30)` **实际等 60 秒**：循环是 `for _ in range(timeout * 10)` 配 `time.sleep(0.2)` ⇒ 参数其实是"轮询次数"的伪装。**实测**（指向不可达端口）：修前 `elapsed=60s`；改成 `time.monotonic()` 截止时间后 `elapsed=30s`，失败提示不变（`[init] FAIL: Hydra not healthy. Is docker-compose up?`），成功路径复测通过（`done: 6 ok, 0 warnings`）。
同一模式在 `integration/e2e_proxy_test.py:67` 也存在，但它的 `sleep` 是 **0.1** ⇒ 恰好 30s ✓ **不是缺陷**；已在 `init.py` 的注释里写下这个对比，免得后人"统一"成 0.2 又把那个变坏。

### C. `environment/init.py` 首次端到端验证（deployment.md 与三个 compose 都点名它，此前**没有任何东西跑过它**）
- **成功路径**：`13 ok, 0 warnings`，随后逐表核对：`providers 2`、`provider-models 3`、`provider-keys 2`、`tenants 1`、`tenant-providers 2`、`tenant-models 3` —— **全部对上** ✓；
- **失败路径**：配置缺失 ⇒ 明确提示 `cp environment/config.example.json secure/config.json` + exit 1 ✓；token 太短 ⇒ 明确提示最小长度 + exit 1 ✓；
- 顺带一个值得记的对比：**它自己就带了 `{created_at:"", updated_at:""}`** —— 这个"从没人跑过"的部署脚本**知道**时间戳契约，而本会话为 UI 写的两个 e2e 用例（第一次运行就红）与 CLI 的 `update`（每个分组都坏）都不知道。同一个仓库、同一个契约、三处实现，**两处错**。

### D. 把这次验证**变成长久的**（而不是又一次手工跑一遍）
`integration/run-crud-local.sh` 已经有一个一次性实例，于是让它顺带跑**文档化的播种步骤**：生成一份临时 config（一个 provider/model/key/tenant，覆盖 `init.py` 的每一类 POST）→ 跑 `python3 environment/init.py` → 要求它打印 `done: N ok, 0 warnings` 且 exit 0。这样 `init.py` 每次进 CI 的 `integration` 作业（以及每次本机跑这个脚本）都会被真正执行一次 —— **它就是那个"没人跑过"的步骤的守卫**。
**反向证伪（实测）**：把生成的 config 里 endpoint 改成 `not-a-valid-url`（服务端会拒）⇒ 脚本 **exit=1**；恢复后 **exit=0**。

### E. 本轮验证
`init.py` 成功路径与三条失败路径各实测（含 60s→30s 的前后计时）；新加的守卫做了反向证伪（见 D）；计划里那个名字错误更正后 `grep` 复核；本轮**未改 Rust**，未跑 cargo。

---


## 2ao. 第五十八轮：**对正在跑的集群做只读探测**（D-8 的证据从"读码"升级成"实测"）+ 把一处陈旧说法在两个还没改的地方补上

### A. D-8（唯一 P1）的**现场证据**
本机 8080–8084/8090/6380/8123 都在听 ⇒ 你的 dev 集群在跑。用**只读**探测（`/healthz` 在 edge 模式免鉴权 ⇒ **不需要任何凭据**就能分辨角色）：

| 端口 | `/healthz` | `/metrics` | `/api/v1/health`（带仓库文档里的 dev token） |
|---|---|---|---|
| 8081 | 401 | 401 | 401 |
| 8082 | 401 | 401 | 401 |
| **8084** | **200** | **200（无 token！）** | 404 |
| 8080 / 8083 / 8090 | 404 | 404 | 404 |

- 8081/8082 = leader/standby（`/healthz` 401 ⇒ 非 edge；`/metrics` 401 ⇒ 需要 token，**符合现行设计** ✓）；
- **8084 = edge**（`/healthz` 200 免鉴权、无 `/api/v1/*` ⇒ 404 ✓），但它的 **`/metrics` 不带 token 就返回 200**。

这**看起来**与现行代码相反（`admin/mod.rs:855-870` 让 edge 的 `/metrics` 走同一条 token 门禁）。**用指标集合给这个二进制定年**：它的 `/metrics` 只有 **4 个 family**，且缺 `hydra_registry_nodes`（第 16 轮前后才有）、`hydra_listener_tenant_certs`（9-16 改造）、`hydra_candidate_skipped_total`（本会话才有）⇒ **这是一个早于 9-16 改造的旧构建**，行为正是改造前那版（源码注释原话："`/metrics` used to be token-free here…"）✓ 矛盾就此消解：**现行代码是对的，跑着的镜像是旧的**。
暴露面：8081/8082/8084 全绑在 **127.0.0.1** ✓ ⇒ 无 token 的 `/metrics` 也不出本机（否则这条 P1 就有了现成实例）。

**对决策的意义**：D-8 的前提在**现行代码 + 官方编排**上成立（§2y-D 的逐行证据），而**本地 compose 其实给 edge 发了 token**（三个服务都 `env_file: ../secure/local-test.env`，该文件确实定义了 `HYDRA_ADMIN_TOKEN`）⇒ "抓不到 edge 指标"**只发生在没给 token 的官方编排上** —— 方向比上一轮更清楚。

### B. 把同一处陈旧说法在两个还没改的地方补上
第 50 轮我修了 `cluster.md:17`（它暗示 edge 提供 `/metrics`）。同样的说法还在两处：
1. `dev-docs/jiqun-deploy.md:91`（k8s 官方编排的 `HYDRA_ROLE` 行）"edge（…仅 `/metrics` `/healthz` `/readyz`）" ⇒ 已**分开说清**：`/healthz`/`/readyz` 免鉴权、`/metrics` 走同一条 token 门禁，而**官方编排不给 edge 注入 token** ⇒ 按现状恒 401；
2. `environment/docker-compose.local.yml` 的 8084 端口注释 "edge admin: /metrics /healthz /readyz only" ⇒ 已写清 `/healthz`+`/readyz` 免鉴权、`/metrics` **需要 token**（本 compose 通过 env_file 给了 token，所以抓取方必须带上），并**注明旧镜像的行为不同**（附本轮实测）。

### C. 顺手清掉一个每次都会出现的警告
三个 compose 里只有 `docker-compose.yml` 还带 `version: "3.8"`，于是每次 `docker compose` 都打印 "the attribute `version` is obsolete … please remove it"。已删除 ⇒ 三个文件现在 **exit=0 且零警告**（实测）。

### D. 本轮验证
- 只读探测：上表（`/healthz`、`/metrics`、`/api/v1/health` × 6 个端口）—— **没有向你的开发库写入任何数据**；
- `docker compose config -q`：三个文件 **exit=0 且无警告**（删 `version` 前后各测一次）；
- `node scripts/check_ci_wiring.cjs` / `node scripts/check_i18n.js` ⇒ exit 0；
- **未跑 cargo**：本轮只改文档与 compose（无 Rust 改动）。

---


## 2ap. 第五十九轮：现场核 ClickHouse 的**实际表结构** —— 一个真缺口在**写路径的守卫**上，不在产品里

### A. 现场取证（只读）
本机 ClickHouse 24.3 在 8123 可查（compose 的 `clickhouse-init` 会应用仓库里的 `init.sql`）。只读查询结果：
- `default.usage_record`：`MergeTree ORDER BY (created_at, tenant_id, provider_id) SETTINGS index_granularity = 8192, **non_replicated_deduplication_window = 1000**` ⇒ **仓库那份 `init.sql` 真的在真实实例上生效**（含第 11 轮那条 P1 的设置）✓；
- 该表有 **76 行真实用量** ⇒ 用量落库链路在跑 ✓；
- 另有 `dedup_probe` / `dedup_probe2` / `dedup_probe3` 三张探针表，以及一个 `oracle_probe_*` 数据库里的同名 `usage_record` —— 都是早前轮次实验的遗留，位于**用户本机实例**里；**我没有删除它们**（那是他的基础设施，删表是写操作）。

### B. 现场发现：本机这张表**缺 `sub_tenant_id`** —— 是"栈旧"，**不是**产品缺陷
`default.usage_record` 的 15 列里**没有 `sub_tenant_id`**，而现行代码的 INSERT（`sink.rs:671`）**列了它** ⇒ 用现行二进制写这张表，ClickHouse 会拒绝每一次插入（sink 只能计数 `hydra_usage_records_dropped_total`）⇒ **该节点不再计量用量/配额/计费**。
但查过事实后确认是**本地栈旧**：`init.sql` **有** `sub_tenant_id` ✓；本地 compose 的 `clickhouse-init` 也有 `ALTER TABLE usage_record ADD COLUMN IF NOT EXISTS sub_tenant_id Nullable(String)`（`:253`）✓；**升级路径在 `ops.md:587-616` 就写着**（"v3 schema migration — sub-tenant usage attribution" 加那两条 ALTER）✓。
⇒ 与第 58 轮的旧镜像、第 57 轮那个旧 `bin/hydra` 是同一件事：**本机 dev 栈整体早于若干次结构变更**。我先怀疑是产品缺陷，**查文档后才排除**（这条本身值得记：不能停在"现场看起来不对"）。

### C. 真缺口：**写路径没有守卫**（已补 + 已证伪）
`crates/hydra-server/tests/clickhouse_ddl_parity.rs` 一直在守"两份 DDL 不许漂移"（`init.sql` vs 本地 compose 的内联 DDL）—— 但**没有任何东西守"代码 INSERT 的列与 DDL 的列是否一致"**。这正是上面那种状态的**结构成因**：只要有人给 INSERT 加/删一列而不同步 DDL，测试全绿、运行时每一次插入都被拒。
新增 `the_usage_insert_names_exactly_the_ddl_columns`：从 `src/sink.rs` 解析 `INSERT INTO usage_record (…)` 的列名，与 `init.sql` 的列集合**精确比对**，并断言"解析到的列不为空"（否则模式一变就会**静默变绿**）。
**反向证伪（实测）**：把 `sub_tenant_id` 从 INSERT 里删掉 ⇒ 该测试失败并打印两边列集；恢复 ⇒ **4 passed**。
**实现中我自己的一个错**：第一版解析只 `trim_end_matches('\\')`，而跨行续行符在**下一段的开头**，于是产出 `"\\\n     status_code"` 这样的"列名" ⇒ 测试先红。**是解析器错，不是 DDL 错**（两边列集合本来完全一致）；改成"只保留标识符字符"后正确。**新写的守卫第一次失败时，先怀疑守卫自己**。

### D. 本轮验证
- `cargo test -p hydra-server --features server --test clickhouse_ddl_parity` ⇒ **4 passed**；
- 反向证伪见 C（失败 → 恢复 → 通过）；`cargo fmt --check` clean、`clippy -p hydra-server --all-targets --features server -D warnings` clean；
- 只读 CH 查询（**未写入任何数据**，**未删除任何探针表**）。

---


## 2aq. 第六十轮：**产品内的 API 文档**上有 2 处真漂移 —— 并用一条实测守卫把它钉住

### A. 为什么查这里
`admin-ui/api-docs.js` 是运维在**产品内部**读的 API 参考（53 条端点），而且它是**手写**的 —— **从来没有任何东西**把它与路由表对过 ⇒ 它可以宣传服务端根本没有的路径或**方法**，运维要等到调用时才发现。

### B. 第一次探测：规则太窄，差点得出"没有漂移"
首版规则只把 `404 … "unknown path"` 当漂移 ⇒ 53 条全部报 ok。但我注意到 `PUT /api/v1/tenant-models/{id}` 返回的是 **405**（路径在、方法不在）⇒ **那同样是漂移，只是换了个状态码**。把 405 也计入后，得到 2 条真漂移：
- `PUT /api/v1/tenant-providers/{id}` ⇒ **405 method_not_allowed**；
- `PUT /api/v1/tenant-models/{id}` ⇒ **405 method_not_allowed**。
两者与另外两处早已记录的事实一致：CLI **明确不给**这两个分组 `update` 子命令、集成套件的注释也写 "association — PUT skipped, no update endpoint"。**只有产品内的文档在承诺它。**

### C. 修法（删掉"能力"，但把知识留下）
删掉那两条 PUT 条目，并在对应的 `GET {id}` 条目上写明：关联表**没有更新端点**、`PUT` 返回 405 method_not_allowed、要改就 DELETE 再建。⇒ 文档不再承诺做不到的事，而运维仍知道该怎么做。

### D. 守卫自己的连锁反应（意外收获）
删掉 PUT 条目的下一秒，本会话早先建好的 i18n 门禁就报：
```
DEAD-EN-KEY  apidocs.summary.PUT.api.v1.tenant_providers.id
DEAD-EN-KEY  apidocs.summary.PUT.api.v1.tenant_models.id
```
因为 `check_i18n.js` 会**从 `api-docs.js` 反推**该有哪些 `apidocs.summary.*` 键 ⇒ 它把"目录里残留、已无人对应的键"抓了出来（**含另外三个 locale**）。已删 8 行（2 键 × 4 语）⇒ `OK (347 en keys, 4 locales…)`（349−2 ✓ 数字对得上）。**这是本会话里守卫之间第一次互相咬合**，值得记一笔。

### E. 把它做成守卫（而不是又手工跑一次）
新增 `integration/check_api_docs.py`：从 `api-docs.js` 抽取 `(method, path)`，对一次性实例逐条发请求（`{...}` 占位换成假 id，POST/PUT 带 `{}`），**把 `404 unknown path` 与 `405` 都判为漂移**；另有两条**防空转**：解析到的端点少于 20 条即失败（模式一变就会静默变绿）、实例不健康即失败（否则每条都会因为"连不上"而"失败"）。
**接线**：并入 `integration/run-crud-local.sh`（⇒ CI 的 `integration` 作业与本机门禁都会跑）。整链实测：`=== 116/116 passed ===` → `documented endpoints: 51 | drift: 0` → `init.py`（`0 warnings`）✓，脚本 exit 0。
**反向证伪（实测）**：把文档里一条路径改成 `/api/v1/does-not-exist` ⇒ 探针 **exit=1** 并打印 `DRIFT  GET /api/v1/does-not-exist 404 not routed`；恢复 ⇒ 0 漂移。

### F. 本轮验证
见 E（整链 + 反向证伪）；`node --check admin-ui/api-docs.js`、`node scripts/check_i18n.js` ⇒ OK。

---


## 2ar. 第六十一轮：把"文档里的**请求体**"也真发一遍 —— 顺带又踩了一次"先怀疑守卫"

### A. 为什么补这一层
上一轮只验了文档的**路由与方法**（51 条全通）。但 `api-docs.js` 每条写端点还带一个示例 `body:` —— 那正是运维**复制粘贴**的东西，而本会话已经**两次**栽在同一类问题上（两个 e2e 夹具、CLI 的 `update`，都是"少了 handler 必填的字段"）。于是给探针加第二阶段：**把每个文档 body 真的发出去**，若服务端回 `400 invalid_json … missing field …` 就是漂移。

### B. 第一版探测：14 条"漂移"全是**我自己的 bug**
第一版用正则从 `api-docs.js` 抽 `body: {...}` 并把文本**原样**发出，结果 **14/14 全被判漂移**，而且**错误信息完全一样**：`failed to parse request body: key must be a string at line 1 column 3`。**这个"完全一致"本身就是信号** —— 原因是我抽取的是 **JS 对象字面量**（键没有引号：`{ id: "openai", … }`），**不是 JSON** ⇒ 探针发的根本不是合法 JSON。
**修法**：不再用正则猜，改成**用 node 运行这个文件本身**（给它 `localStorage/navigator/document/window` 桩，与 `check_i18n.js` 的做法一致）拿到真实对象再 `JSON.stringify`。顺带两个收益：①拿到了**真实**的 body（我第一版的正则只匹配 POST/PUT，**漏掉了 `DELETE /api/v1/auth/cache` 那条带 body 的**）；②阶段一的 path/method 也来自同一份真实数据。
**教训（第二次同类）**：**一个跨所有输入完全一致的失败，是夹具的 bug，不是 N 个产品缺陷。**（上一轮的同族教训是"新守卫第一次红先怀疑守卫自己"。）

### C. 结果：**0 漂移**，但这次是"核过"而不是"看起来对"
修好后：`documented endpoints: 51 | documented bodies: 15 | drift: 0` —— 15 个文档 body 全部以**正确形状**被接受（POST 得 201、PUT 对不存在的 id 得 404 ⇒ 路由到达并查了库，都不是"缺字段"）。跳过的 1 条在输出里写明理由：`POST /api/v1/tenants/auth/test` 会让**服务端去请求 auth_url**，探它属于另一件事。
**反向证伪（实测）**：把文档里 providers 的示例 body 删掉 `key` 字段 ⇒ 探针 **exit=1** 且只报这一条：`DRIFT POST /api/v1/providers 400 … missing field key` ✓（精确、不误伤其余 14 条）。

### D. 接线与整链
`integration/check_api_docs.py` 现在两阶段都在 `integration/run-crud-local.sh` 里（⇒ CI 的 `integration` 作业 + 本机门禁）。整链实测：
```
=== 116/116 passed ===
documented endpoints: 51 | documented bodies: 15 | drift: 0
[init] done: … ok, 0 warnings
```
脚本 exit 0 ✓。

---

## 2as. 第六十二轮：**公开页面上的两个数字/断言**都没有守卫 —— 其中一个背后藏着一个真缺口（`[[bin]]` 没有 `forbid(unsafe_code)`）

### A. 起点：一个我自己刚写进去就过期的数字
上一轮把 `docs/index.html` 那句公开断言从 `287 tests` 改成 `796`。实测本轮的 Rust 用例数是 **253（hydra-core）+ 544（hydra-server）= 797** —— 第 59 轮给 `clickhouse_ddl_parity.rs` 加的那条测试让 server 从 543 变 544，而**没有任何东西会比对这个数字**：它已经烂过两次（`287` 与真实值差了 2.8 倍；`796` 又差 1）。改对数字不是修复，**加守卫**才是：
```
core=253 server=544 total=797      # cargo test -p hydra-core / -p hydra-server --features server
```

### B. `scripts/check_public_claims.cjs`（新）+ CI 接线（零额外成本）
CI 的 `check` job **本来就在跑**这两个套件，所以它的输出就是现成的"测量值"。两步 `cargo test` 现在 `| tee "$RUNNER_TEMP/{core,server}-tests.log"`（**显式** `set -o pipefail`：`| tee` 会把 cargo 的退出码换成 tee 的，这正是本仓库栽过的那类静默），随后一步 `node scripts/check_public_claims.cjs --core-log=… --server-log=…` 断言**页面上两个语区的数字都等于实测和**，且两语区的 **测量日期**一致。
- 退出码分三级：**0** 一致；**1** 陈旧/语区不一致/缺日期（并打印 `--measure --write` 的刷新命令）；**2** **无法核对**（缺日志、日志里没有 `test result:` 行、套件本身没通过、或实测低于下限 100）。"无法核对"绝不返回 0 —— 这是本会话反复踩的"守卫默认通过"。
- `--measure` 能自己跑两条套件（本机用），`--write` 把数字**与当天日期**一起重写；**18** 条断言见 `scripts/check_public_claims.test.cjs`。
- **反向证伪（实测）**：把真实页面改回 `796` ⇒ `exit=1`，且只报 `advertised 796 but the suites report 797 (hydra-core 253 + hydra-server 544)`；恢复 ⇒ `exit=0` ✓。另有一条测试**直接对仓库里那份 `docs/index.html` 跑检查器**（不是对夹具），所以页面被改坏时套件会红。

### C. 顺着同一行往下看：`unsafe / unwrap / panic = 0` 这条断言**是真的**，但没人守
同一块统计里有 `0 unsafe` + "生产代码无 unwrap / panic / unsafe"。这条更容易变成假话，于是先**测**再**守**。测量方法本身有两个坑，两个都踩了：
1. 第一版剥 `#[cfg(test)]` 只认**裸形式**，于是 `cluster/forward.rs` 的 `#[cfg(all(test, feature = "cluster-redis"))] mod registry_tests` 被当成生产代码 ⇒ 报出 20+ 条"生产 `expect`"。**这正是 `HANDOFF.md:50` 那条手写 grep 配方（"取每个文件第一个 `#[cfg(test)]` 之前的文本"）的同族漏洞**，也是它把 8 数成 6 的原因。
2. 注释与字符串：`admission.rs:102` 的 `/// … no ManuallyDrop / unsafe needed…`、`restore.rs:40`/`proxy.rs:1334` 提到 `unreachable!()` 的注释、以及任何错误信息字符串里的 `panic!()` 都不是违规。

剥干净（`#[cfg(test)]` **两种形式**、注释、字符串内部）后的实测，`crates/*/src` 63 个文件：

| 断言 | 实测 | 说明 |
|---|---|---|
| `unsafe` | **0** | 唯一命中是文档注释 |
| `.unwrap()` | **0** | 成立 |
| `panic!`/`unreachable!`/`todo!`/`unimplemented!` | **0** | 两处命中都是注释 |
| `.expect(…)` | **8** | 公开断言**没提** `expect`；`HANDOFF.md` 说 **6** ⇒ 陈旧（差的是 P1-1 重定向修复后新增的 `cluster/forward.rs:203` 与 `proxy/provider_client.rs:114`，两处都是 `build().or_else(...).expect(...)`） |

### D. **真缺口**：`[[bin]]` 是独立编译单元，`lib.rs` 的 `#![forbid(unsafe_code)]` 管不到它
`HANDOFF.md` 写"both crates: `#![forbid(unsafe_code)]`"，实际只有 `crates/*/src/lib.rs` 有；`crates/hydra-server/src/main.rs`（`[[bin]] name = "hydra"`，**镜像里跑的就是它**）**没有**。`unsafe` 会在那里静默编译通过 —— **实测证明，不是推断**：
```
（去掉属性 + 植入 unsafe 块，模拟修复前的树）cargo check … --bin hydra ⇒ Finished，exit=0   ← 能编译
（加上属性 + 同一段 unsafe）              cargo check … --bin hydra ⇒ error: usage of an unsafe block
```
**修法**：`main.rs` 顶部加 `#![forbid(unsafe_code)]`（放在 `//!` 文档注释之后、首个 item 之前，附注释说明它为什么必须在这儿），并让新守卫**断言每个 crate 根**都带该属性（`src/lib.rs` **与每个 `[[bin]]`** ⇒ 以后新增 bin 忘了加会红）。

### E. `scripts/check_source_purity.cjs`（新）+ 接线
同一套剥离逻辑（注释/**字符串内部**一起 blank、`#[cfg(test)]` 两种形式）扫 `crates/*/src/**/*.rs`，断言 `unsafe`/`.unwrap()`/panic 族 **三者为 0**；同时校验 crate 根属性、以及**页面那句断言仍然存在**（改词就红 ⇒ 逼迫重新核对，而不是让守卫悄悄匹配不到东西）。`expect` 的 8 处**逐行打印**但**不判违规**（公开断言没提它），并在输出里点明这是待决策项（见 F）。**20** 条断言见 `scripts/check_source_purity.test.cjs`，其中两条**故意**覆盖边界：`#[cfg(all(test, feature = …))]` 里的 `unwrap()` **不算**违规、字符串 `"http://a"` 之后的真实 `unwrap()` **必须**被抓到（防止把字符串当注释吞掉半行）。
接线：CI `scripts` job 两条（检查器 + 自测），本机门禁 `.acceptance/round10-gate.sh` 两条；`check_ci_wiring.cjs` 现在识别 **12** 个脚本工件（原 10）。

### F. 新待决策 **D-13**：生产代码里的 `expect` 到底禁不禁？
两份设计文档写着"生产代码不得出现 `unwrap/expect/panic/…`"（`dev-plan.md:197-206`、`design-tenant-api.md:974`），门禁表甚至给了命令并说"必须为空"（`:1084`）。实测：那条命令**从未可执行** —— 不过滤 `#[cfg(test)]` 时 `crates/hydra-server/src` 有 **489 行命中 / 34 个文件**；过滤后 `expect` 仍有 **8** 处。三条路（都改对外/工程约定，故交决策）：①承认现状，把规则改成"禁 `unwrap`/`panic`/`unsafe`；`expect` 仅限不可达不变量并逐个登记"；②真禁 `expect`，把 8 处改成返回 `Result`/`Option`（`control_client` 的 poisoned mutex 是主要难点）；③只对**请求路径**禁（分模块清单）。**本轮已做的**是不擅自定政策：把两份文档的"现状 vs 规则"差异标注清楚（不删原规则），并把可自动化的那部分（前三个断言 + 根属性）变成硬门禁。

### G. 本轮同时完成的（文档↔事实）
- `dev-docs/HANDOFF.md:44-56`：`6` 处 `expect` ⇒ **8** 处并列全（含新增两处的位置与原因），把"手写 grep + 取第一个 `#[cfg(test)]` 之前"的配方替换为 `node scripts/check_source_purity.cjs`，并写明为什么手写配方不可靠（489 行、漏 `cfg(all(test, …))`）。
- `dev-docs/design-tenant-api.md:974` 与 `:1084`：各自加"2026-09-29 复核"标注（实测数字 + 已有自动门禁 + D-13），**保留**原规则文本，不装作它已被满足。
- **已发布模板也被真跑过**：`environment/init.py` 用**仓库里那份** `environment/config.example.json`（只注入 admin_url/token）跑出 `17 ok, 0 warnings`（算术核对：2 provider → 9 行 + 1 租户 + 2 tenant-providers + 5 tenant-models），并且 `integration/run-crud-local.sh` 里那一步已改成**从该模板生成配置**（`[it-crud] init config from the shipped template: 2 provider(s), tenant=default`）；整链 `116/116 passed` → `documented endpoints: 51 | documented bodies: 15 | drift: 0` → init `0 warnings`。

### H. 我这一轮自己犯的三个错（都当场发现）
1. **17/18 全红 ⇒ 先怀疑夹具**：新检查器的测试第一次跑，`SyntaxError: Identifier 'ClaimError' has already been declared` —— 我"去重"那次编辑留了两份同名 class。**"跨所有输入完全一致的失败是夹具 bug"** 这条元教训第二次生效。
2. JS 模板字面量里塞了 Rust 文档注释的**反引号**（`` `.unwrap()` ``）⇒ 把模板字符串提前闭合，测试文件语法错。夹具里的反引号要清掉。
3. 有一条断言我写错了预期（"只有 Cargo.toml、没有根的 crate" 走的是 `no crate roots found`，不是 `no Rust sources under`）—— 这次**守卫是对的、断言是错的**，改断言；同时补了一条真正的"`[[bin]]` 路径不存在"用例。

### I. 本轮的收尾门禁
见 §2at。

---

## 2at. 第六十二轮（收尾）：全量门禁 **26 条 / 25 绿**（唯一红的仍是 D-2，且逐节解析确认）

本机门禁从 22 条扩到 **26 条**（新增 `public claims`、`public claims tests`、`source purity`、`source purity tests`），逐条独立取退出码：

| 条目 | 结果 |
|---|---|
| fmt --check / clippy×4（server、optional、tls-openssl、proxy-only） | 全 exit=0 |
| **test hydra-core** | **253 passed / 0 failed** |
| **test hydra-server (server)** | **544 passed / 0 failed / 1 ignored** |
| **test optional + live redis/CH** | **672 passed / 2 failed / 3 ignored** —— 两个失败仍是 **D-2**（`empty_body_delete_invalidates_all_local`、`too_many_invalidation_keys_are_refused_and_publish_nothing`），自第 3 轮起未变 |
| ignored: listener limitation / ignored: clickhouse e2e | exit=0（真活 CH + 真 Redis） |
| compose config（三份，含本地） | exit=0，零警告 |
| findings disposition / i18n / i18n tests / admin-ui render | exit=0 |
| e2e contracts / e2e contract tests / ci wiring / ci wiring tests | exit=0 |
| **public claims（本轮新增）** | exit=0，输出 `[claims] OK: advertised 797 Rust tests == measured 797 (hydra-core 253 + hydra-server 544), dated 2026-09-29` |
| **source purity（本轮新增）** | exit=0，输出 `[purity] OK: 0 unsafe / 0 unwrap() / 0 panicking macros in production code` + 3 个 crate 根 + 8 处 `expect` 逐行 |
| integration crud (disposable) / integration proxy e2e / cli tests | exit=0 |

**算术自洽核对**（按 section 解析，不按范围切片 —— §2p 那条教训）：core 253 + server 544 = **797**，正是页面现在的数字；`optional` 从第 49 轮的 **671** 变 **672**，与本轮 server **543 → 544** 的 +1 完全对应（同一批新增测试在带可选特性的组合里也会被计入），**不是**新的漂移。
`OVERALL=RED` 唯一来源仍是 D-2 那两例 —— 即"本轮没有引入新的红"，而 D-2 本身**等决策**（第 50 轮已复核其前提）。


---

## 2au. 第六十三轮：公开页面**剩下那几条数字**逐条真测 —— 找到两条真缺陷（一条数值陈旧、一条是**串号**），并更正我自己第一版的错误判断

### A. 起点
上一轮给"测试数量"和"unsafe/unwrap/panic"两条公开断言加了守卫，于是本轮把页面**剩下的数字**逐条列出来核（`docs/index.html` 全文扫描，见 §C 表）：`11056 RPS @ c=25 / p99 4.39ms`、`0.3 ms 网关开销`、`RSS 18.6 → 65.4 MiB（16GB 的 0.4%）`、`114 core + 173 server 测试`、`65 MiB 二进制`。这些数字要么从未被任何东西核对（`scripts/load_test.sh` 需要 `oha`/`wrk`，本机都没装 ⇒ 从没人跑过），要么与上一轮修好的那一处**同一页不同口径**。

### B. 两个测量陷阱，都踩到了（真风险，写进了 harness 头部）
1. **debug 与 release 差一个数量级**：第一次用 `target/debug/hydra` 测得 **2403 RPS / p50 10.6 ms / RSS 39 → 44 MiB**。若就此下结论会得出"页面虚报 4.6 倍"的**错误**结论；而 p50 ≈ 25 × 0.42 ms 正是"排队"的形状（c=25 × 每请求串行成本），不是"网络慢"。换 `cargo build --release` 后同一夹具得 **25,047 RPS / p99 1.55 ms / RSS 21 → 30 MiB**。
2. **仓库自带的 mock 是单线程的**：`integration/mock_llm.py`/`mock_auth.py` 都是 stdlib 单线程 `HTTPServer`。实测：同一 release 二进制、同一机器，上游用 `mock_llm.py` ⇒ ~**2.4k** RPS（那是 mock 的上限），换并发 Go echo 上游 ⇒ **25.0k** RPS。照文档复现的人很容易掉进这个坑。

### C. **先更正我自己第一版的判断（三处都错）**
我第一版写下"页面数字无机器、无方法、无日期"，据此还写进了 INDEX。**读完整个页面后，这三条都不成立**：
- `:424`「10 核机器实测 · c=25 · 无真实付费上游」；`:716`「10 核机器，线程化 mock 上游」；
- `:744` 给出方法并链到 **`dev-docs/evaluation-report.html`**（评测日期 **2026-08-09** · v0.1.0 · macOS 10 核 / 24GB），该报告 `:267` 明写 **release 构建（`cargo build --release --features server`）**、`oha 1.15`、线程化 Python mock、预热鉴权缓存、隔离 mock 基线；`:271` 还自己声明了局限（macOS vs Linux 内核/网络栈差异、Python mock 在高并发区间衰减、真实 LLM 延迟未模拟）。
⇒ 页面**有**机器、**有**方法、**有**日期来源，我最初只读了统计带那 20 行就下了结论。**教训（本轮新增）**：要断言"某文档没写 X"，必须把那份文档**整份**读完 —— 与"新守卫第一次红先怀疑守卫"同族，但方向相反（这次是"我对外部对象的负面断言没核实"）。因此本轮的实际工作是：**跨平台复现 + 把页面其余数字逐条核**，而不是"补缺失的出处"。

### D. 跨平台独立复测（release · Linux/x86 · 14 核 Intel Ultra 5 235 · 8 秒 · 并发 Go echo 上游 · 未触达付费上游）
`.acceptance/round63/{upstream.go,loadgen.go,measure.sh}`（scratch，与其它门禁脚本同类，见 D-5；支持 `BIN=`/`SWEEP=`，采样 `/proc/<pid>/status` 的 `VmRSS`/`VmHWM`）：
```
proxied c=1    rps=  6248  p50=0.148ms  p99=0.396ms
proxied c=4    rps= 20855  p50=0.172ms  p99=0.418ms
proxied c=25   rps= 25047  p50=0.986ms  p99=1.550ms     ← 页面标的就是 c=25
proxied c=64   rps= 22598  p50=2.868ms  p99=4.338ms     ← 过饱和点
baseline(直连上游, c=25) rps=259349  p50=0.063ms  p99=0.520ms
RSS: boot 22504 kB (21 MiB) → 42k 请求预热后 25496 kB (24 MiB) → 满载峰值 31228 kB (30 MiB)
```
| 页面（2026-08-09 macOS 基准） | 本次复测（Linux/x86） | 判定 |
|---|---|---|
| 11,056 RPS @ c=25 | **25,047**（另一轮 25,108） | 页面保守 2.3× |
| p99 4.39 ms | **1.55 ms** | 页面保守 |
| 0.3 ms 网关开销 | c=1：0.148 vs 直连 0.063 ⇒ **≈0.09 ms**；c=4 ⇒ 0.11 ms | 页面保守 |
| RSS 18.6 → 65.4 MiB | **21 → 30 MiB**（VmHWM 30.5 MiB） | 峰值保守 2.2×；空闲同量级 |

两轮 release 测量可复现（25,047 / 25,108 RPS；1.550 / 1.575 ms，抖动 <1%）。**顺带核过出厂路径**：`environment/build.sh:10/36` 走 cross-compile **release** 且只 stage 那一个二进制、`Cargo.toml` 无 `[profile.release]` 覆写 ⇒ 镜像里跑的确实是 release，"页面基于 release"这个隐含前提对**出厂**成立，只对**手敲 debug 二进制做基准**的人不成立（正是我第一版踩的）。

### E. 因此找到的**两条真缺陷**（这才是本轮的主要产出）
1. **`114 core + 173 server 测试`（`:748-749`）陈旧**：实测 **253 core + 544 server**。同一页面另一处（统计带）写着正确的 797 ⇒ **一页两个口径**。已改（含 en），并把新守卫补上：`check_public_claims.cjs` 现在**逐套件**比对这条断言（它本来就同时拿到两份 transcript），任一处不符即 exit 1 并点名是第几处；配套断言 **18 → 22 条**，含一个"**总数对但拆分错**"的用例（200 + 597 = 797 仍必须红）。**反向证伪实测**：把页面改回 `114/173` ⇒
   ```
   [claims] FAIL: docs/index.html
     - correctness-gate claim #1 says 114 core + 173 server but the suites report 253 core + 544 server
     - correctness-gate claim #2 says 114 core + 173 server but the suites report 253 core + 544 server
   ```
   恢复 ⇒ exit 0。
2. **`65 MiB 二进制`是串号**（`:411/414` hero、`:806/809` footer、`:7` `<meta name="description">`）：评测报告 `:26` 写的是 **`65 MiB (RSS)`**，全报告**从未**声称二进制体积——65.4 是**满载 RSS**，被搬到"打包进一个 65 MiB 的二进制"这句里。实测二进制：随镜像发布的 `bin/hydra`（server+cluster-redis+usage-clickhouse，2026-09-15 构建）**31 MiB**、本机 `--features server` release 构建 **27.7 MiB**（debug 是 275 MiB，可解释 65 从何而来）：都不是 65 MiB。已把四处 + meta 改为「约 30 MiB（随镜像发布的 Linux release 构建实测 31 MiB）」，并让 footer 明说"约 30 MiB 二进制"（原文仅"65 MiB"，无法判断指二进制还是 RSS —— 这本身就是那两个数字被混用的后果）。
   **这一类刻意不做自动守卫**：要真判它就得在 CI 里构建**出厂特性集**并 `stat` 产物，而页面数字随平台/特性变化（macOS 与 Linux、`--features` 组合都不同），做出来只会是 flaky 门禁 —— 与性能数字同理，见 F。

### E2. 同类串号还扩散到了两份**对外简报**（已一并修）
全仓 grep `65 MiB` 共 7 个文件（页面本身 + 评测报告 + 计划/INDEX + 2 份 GPUStack/Higress 替代简报 + 3 份文案），逐个看上下文：
- **正确标注的**（本就写的是 RSS/RAM/满载）：`文案/README.md:36`（"满载内存 RSS 65.4 MiB"）、`文案/Twitter-X-社交媒体.md:13/17`（"65 MiB RSS"、"65 MiB RAM"）、`文案/V2EX-掘金-技术社区.md:48`（"**满载** 65 MiB"+方法）。**不动**。
- **同一条串号**（把 RSS 当二进制体积）：`gpustack-higress-replacement-briefing.md:97` 写"65 MiB **单二进制**"、`gpustack-higress-zero-change-replacement-deep-dive.md:220` 写"vs Hydra 65 MiB"（上下文是"内存/镜像"）⇒ 两处都改成"**单二进制 31 MiB** + 满载 RSS 65 MiB（2026-08-09 基准；2026-09-29 Linux 复测 21→30 MiB）"。顺带一提：**这条修改让简报的论点更强**（31 MiB 的单二进制比 65 MiB 更有说服力），而"对比数百 MB~GB 级"的结论不依赖被改掉的那个数字。
- 另核到一条**曾经是假话、上一轮才变真**的宣传句：`文案/V2EX-掘金-技术社区.md:52` 写"两个 crate 全部 `#![forbid(unsafe_code)]`（不是 `deny`，是 `forbid`）"—— 上一轮发现 `[[bin]]`（`src/main.rs`）**没有**该属性并补上，这句话到那时才**完全**成立（lib 一直是真有，bin 是"文件里没有所以不成立"）。无需再改，记录这层因果。

### F. 页面与 harness 的改动
1. `docs/index.html`：①`:748/:749` 计数改对（+「2026-09-29 计数」）；②hero/footer/meta 的二进制体积改成"约 30 MiB 二进制（实测 31 MiB）"；③统计带下方**新增跨平台复测段落**（中英），明确"原始测量条件见下方测量方法与评测报告；以下是 2026-09-29 在 Linux/x86、release 构建、8 秒、并发 Go echo 上游下的独立复测"，并给出 25,047 RPS @ c=25 / p99 1.55 ms / 开销 ≈0.09 ms / RSS 21→30 MiB，附复现入口与**单线程 mock 警告**；④方法学那条链接**补上评测日期**（2026-08-09 · v0.1.0 · macOS 10 核 / 24GB），读者才能判断这些数字的年代。**不替换**页面原有的 11056/4.39/65.4/0.3（它们来自另一台机器与 2026-08-09 的评测，我无从核对那台机器），只把**日期 + 复测值**摆在旁边。
2. `scripts/load_test.sh` 头部加"复现已发布基准"三条：必须 `C=25` 且跑到稳态（`N=1000` 只是热身）；上游必须**并发**（点名两个单线程 mock，附 2.4k vs 25.0k 实测）；新数字必须连**机器**一起写。
3. **性能与体积数字不做硬门禁**，理由写明：它们机器相关（`RPS ≥ 11056` 在共享 runner 上必然 flaky，比没有门禁更糟）；能得到的最强保障是"页面写出处 + 复现入口 + 本记录"。**计数类做硬门禁**（机器无关，已经做了）。

### G. 本轮验证与未覆盖
`node scripts/check_public_claims.cjs …` ⇒ exit 0，输出两行（总数 797 与逐套件 253/544）；`node --test scripts/check_public_claims.test.cjs` ⇒ **22 pass / 0 fail**；`node scripts/check_source_purity.cjs` ⇒ exit 0；`node scripts/check_ci_wiring.cjs` ⇒ OK；`bash -n scripts/load_test.sh` ⇒ OK；HTML 标签配平（div 129/129、p 46/46、span 145/145）。**本轮未改 Rust 代码**（debug/release 两个二进制都来自当前源码）。
**没测到的（如实记录）**：上游返回**单个 JSON 响应体**而非 SSE，"真实 LLM 流式路径（逐 chunk flush、`ttft`、长连接）本轮未覆盖"，页面的 p99 也不含上游真实延迟；"16GB 机器的 0.4%"在本机（30.7 GiB）无法核对，仅与 65.4/16384 = 0.399% 的**内部算术**自洽（与第 61 轮同法核过）。

---
## 2av. 第六十四轮：把"文档承诺的旋钮"逐条核对 —— 一个幽灵环境变量、一个不存在的配置文件，以及**每个容器重启都会丢掉缓冲用量**（实测）

### A. 起点与方法
上一轮修的是"页面上的数字"；这一轮换一类同样可机械核对的承诺：**运维文档告诉你要设的东西，代码里到底读不读**。做法是先做一次**全仓审计**（`crates/**/*.rs` 里的 `HYDRA_*` 字面量 vs 文档里出现的名字），再把候选逐条落到**运行时实测**。

**我第一版审计工具是错的，且错法很典型**：只匹配 `env::var("HYDRA_X")` 字面量 ⇒ 报出 28 个"文档提了、代码没读"的变量，其中包含 `HYDRA_TLS_LISTEN` 这种**显然是核心功能**的名字。原因：本仓库把变量名放在**常量**里（`crates/hydra-server/src/listeners.rs:52` `pub const TLS_LISTEN_ENV: &str = "HYDRA_TLS_LISTEN";`），再用 `env::var(TLS_LISTEN_ENV)` 读。改成"扫描任意 `HYDRA_*` 字符串字面量"后，幽灵从 28 个降到 15 个，再剔除历史计划文档/CI 局部变量后，真正的候选只有 **5 个**，其中 4 个**早已被诚实地标注为"从未实现"**（`HYDRA_EDGE_TLS`、`HYDRA_FAILOVER_GRACE_MS`、`HYDRA_RATE_LIMIT_FAIL_MODE`，以及 HANDOFF 里那条记录 `HYDRA_DATABASE_URL → HYDRA_DB_URL` 的历史改名，见 `jiqun-deploy.md:113`、`cluster.md:289`、`ops.md:1297`）。**教训重复一次：新守卫/新审计第一次报出一大片命中时，先怀疑工具。**

### B. 真缺陷 1：`ops.md` 把 `HYDRA_LOG` 当成一个能用的别名（实测：它是死的）
`dev-docs/ops.md:63` 原文：`| \`RUST_LOG\` / \`HYDRA_LOG\` | \`info\` | \`tracing\` env filter. |`。全仓 grep：`HYDRA_LOG` **只出现在这一行**，`crates/` 里 0 处；日志初始化是 `main.rs:172` 的 `EnvFilter::from_default_env()`（只认 `RUST_LOG`）。**运行时实测**（`.acceptance/round64/env-log-test.sh`，同一二进制、同一 scratch DB，只有环境变量不同；每次都验证实例**活着**：`/api/v1/health` 与 `/api/v1/providers` 都回 200）：
```
A HYDRA_LOG=debug   alive: health=200 providers=200   lines=0    DEBUG=0   INFO=0
B RUST_LOG=debug    alive: health=200 providers=200   lines=132  DEBUG=97  INFO=35
C 两个都不设        alive: health=200 providers=200   lines=0    DEBUG=0   INFO=0
```
⇒ 照文档设 `HYDRA_LOG=debug` 的人**一条日志都拿不到**（这正是"出事故时最需要的那个旋钮"）。**顺带核出第二条**：表格说默认 `info`，但 C 组（两个都不设）**0 行**——`from_default_env()` 的缺省不是 info；`info` 是**部署**默认（`environment/Dockerfile:48` 与三份 compose 都设 `RUST_LOG=info`）。已把该行改成只有 `RUST_LOG`，并写明三点：`HYDRA_LOG` 不被读取（附 grep 与实测）、`info` 是部署默认、裸跑二进制时是静默的。
**这是同一族里第三个"文档提过、代码从未读取"的开关**（前两个是 `reveal plaintext` 与 `HYDRA_EDGE_TLS`），所以本轮不只修字，还加了守卫（见 E）。

### C. 真缺陷 2：`ops.md` 把 `hydra.toml` 画进部署目录树（本仓库没有配置文件加载器）
`:18` 原文"single static binary + `data/` + **an optional `hydra.toml`**"，`:24` 的 `/opt/hydra/` 树里还有 `hydra.toml  # config (NO secrets — token from env)`，`:257` 说升级时"the new process re-reads `hydra.toml`/env on boot"。实测：**任何 `Cargo.toml` 都没有 `toml` 依赖**，`grep -rn 'hydra\.toml' crates/` 为空；`design-tenant-api.md:875` 早就把这条记为**既有差异**（且 `ops.md:94-96` 自己也写了"toml 是 target schema"）——但**部署目录树与升级说明仍在按它写**，运维会照做。已改三处（目录树去掉该文件、`:18` 明说 env-only 并给出证据、`:257` 改成"re-reads **env**"），并在 `design.md` §15.1（`hydra.toml` 目标 schema）加了状态块：**未实现**，附同样两条证据。

### D. 真缺陷 3（本轮最重）：**每个 `docker stop` / `compose down` / 容器重启都会丢掉缓冲的用量**
线索来自 B/C 同类核对时顺手读的关闭路径。事实链：
1. `main.rs:1569-1578` 与 ops.md §13.5b 都写明：进程关闭时会为**在飞请求**排空 `HYDRA_SHUTDOWN_DRAIN_SECS`（默认 **20** 秒），再加 pingora 最后一段运行时的 5 秒；注释还警告"`terminationGracePeriodSeconds` MUST exceed this value plus 5s plus slack"。
2. **实测**（`.acceptance/round64/sigquit-test.sh`，release 二进制，一次性实例 + Go 并发 echo 上游 + 20 条真实代理请求；用量 sink 是**批处理**的 `DEFAULT_BATCH_SIZE=256 / DEFAULT_FLUSH_SECS=5`，所以这 20 条在信号到达时**还在内存里**）：
```
sigquit  signal=QUIT  requests_ok=20  rows_before=0  rows_after=20  exit=0   shutdown=35.02s  flush log lines: 2
sigterm  signal=TERM  requests_ok=20  rows_before=0  rows_after=20  exit=0   shutdown=25.02s  flush log lines: 2
sigkill  signal=KILL  requests_ok=20  rows_before=0  rows_after=0   exit=137 shutdown=0.00s   flush log lines: 0
```
⇒ SIGQUIT（**正是 systemd 单元 `KillSignal=SIGQUIT` 发的那个信号**）35 秒、SIGTERM 25 秒，两者都把 20 条用量落库；**对照组 SIGKILL 一行业都没有** —— 证明"排空"是承重的，不是"反正一直在写"。（此测试也验证了代码注释里那句"pingora 以 `process::exit(0)` 结束、不跑析构"的钩子是有效的；此前**没有任何测试提到 SIGQUIT**，`grep -rln SIGQUIT crates/*/tests` 为空。）
3. **而三份 compose 文件一个都没设 `stop_grace_period`** ⇒ Docker 默认 **10 秒**（SIGTERM → 等 10s → SIGKILL）⇒ **常规 `docker stop` / `compose down` / 重启会在排空完成前把进程 SIGKILL 掉，缓冲用量按上面对照组的方式丢掉**。`ops.md`/`jiqun-deploy.md` 只写了 k8s 的 `terminationGracePeriodSeconds`，**容器这条路径没人写**（`grep -rn stop_grace_period environment/ dev-docs/` 在本轮之前为**空**）。
**修法**：给**全部 7 个 hydra 服务**（`docker-compose.yml` 的 `hydra`；`cluster.yml` 的 `hydra-control-a/b`、`hydra-edge`；`local.yml` 的 `hydra-a/b/c`）加 `stop_grace_period: 30s`（= 20 排空 + 5 最后一步 + 5 余量），附注释写明算术、实测数字、以及"若你同时设 `stop_signal: SIGQUIT` 则要 40s"。三份文件 `docker compose config -q` 全部 exit 0。**注意 `hydra-edge` 没有 `container_name`**，我第一版按 `container_name` 定位只改到 2/3 个服务 —— 渲染后逐个核对才发现，已补齐（这也是"改完要按渲染结果复核"的又一例）。

### E. 两个新守卫（把 B 与 D 变成机制）
1. **`scripts/check_documented_env.cjs`（+ 11 条断言的测试）**：把 `ops.md` 里**表格第一列的每个配置名**拿去和"全仓可执行文件里出现过的字面量"比对，缺一即 exit 1 并打印那一行；**低于下限（行数/检查数）即 exit 2**，绝不空过。**它的头两版都自证成立，两次都被我自己的证伪抓住**：
   - 第一版把 `.md` 也算证据 ⇒ **文档自己满足了断言**（我按修复前的行做证伪，居然 exit 0）；
   - 第二版排除 `.md` 但没排除**注释** ⇒ **检查器自己头部引用的那行**（`scripts/check_documented_env.cjs` 的注释里写着 `HYDRA_LOG`）成了证据 —— 同样证伪为 exit 0。
   现在只采信"可执行/配置文件里、去了注释之后的字面量"，并对"明确写了『未读取/未接线』的行"跳过（承认诚实的说明不是承诺）。**修完后证伪生效**：把 `ops.md` 那行改回 `RUST_LOG / HYDRA_LOG` ⇒ exit 1 并点名 `HYDRA_LOG`；恢复 ⇒ exit 0。另外还派生了 4 条测试（两种自证回归各一条、`RUST_LOG / X` 同格两名字都查、真字面量算证据）。
2. **`scripts/check_compose_grace.cjs`（+ 11 条断言的测试）**：用 `docker compose config --format json` 渲染三份文件，对每个 hydra 镜像服务断言 `stop_grace_period` 存在且 ≥ **从代码读出的预算**（`parse_shutdown_drain_secs` 的 `unwrap_or(20)` + `graceful_shutdown_timeout_seconds: Some(5)` + slack 5 = 30s）。**阈值不是硬编码**：测试里用一个 drain=60 的 fixture `main.rs` 证明它会自动升到 70s（用 `--main-rs=` 注入，**不去改仓库里那份 main.rs** —— 本会话已经有过"探针被杀、文件留在被改状态"的事故）。下限 4 个服务（官方拓扑 1+3），**CI 里本地 compose 因缺 `secure/local-test.env` 会渲染失败**，此时打印 SKIP 理由而不是失败（已用"把该文件临时移走"的方式模拟过 CI：4 个服务、exit 0）。
   两个守卫都已接进 CI 的 `scripts` 作业与本机门禁（`check_ci_wiring.cjs` 现在识别 **16** 个脚本工件）：门禁条目 26 → **30**。

### F. 本轮验证与没做的事
`node scripts/check_compose_grace.cjs` ⇒ exit 0，逐服务打印 7 个 `stop_grace_period=30s`；**证伪**：把 `docker-compose.yml` 的改成 `10s` ⇒ exit 1 并给出 `10s < 30s`，恢复 ⇒ exit 0。`node scripts/check_documented_env.cjs` ⇒ exit 0（41 个名字全有引用）+ 证伪同上。`check_ci_wiring` / `check_i18n` / `check_public_claims` / `check_source_purity` 全绿；两份新测试 **11 + 11 全过**；三份 compose `config -q` 全 exit 0。**未做**：没有真起容器验证 `docker stop` 的端到端行为（那需要构建镜像；本机 dev 栈是裸进程）；`systemd-analyze verify` 对提取出的 unit 只报"`/opt/hydra/hydra` 不存在"（预期，那是部署路径），语法与指令合法；ClickHouse/Redis 服务的 stop grace 未动（本轮只覆盖我们测过的那个排空语义）。

---
## 2aw. 第六十五轮：把**错误契约**也变成被测对象 —— 契约本身是好的，但我的探针错了四次（每次都被自己抓住）

### A. 为什么挑这一条
`admin-ui/app.js` 里长出了 `cleanErrorBody()`（把 HTML 错误页压成 `<title>`）与 `errorHead()`（去掉重复的状态码），这说明**服务端的错误形状从来不是一个被测契约**：UI 是"绕过去"而不是"依赖它"。而 `admin-ui/api-docs.js` **确实**逐端点写了 `errors: [{status, code}]`（51 条里 26 条有），所以这是**文档↔行为**可机械核对的一类。新探针 `integration/check_error_contract.py` 对**每一个**文档化端点发四类"应当失败"的请求（无 token / 畸形 JSON / 未文档化的方法 / 未知路径），并检查：错误体是不是 JSON、有没有 `code`、状态与**该端点自己文档化的码**是否一致。

### B. 我的探针第一版报了 21 条"违规"，四条原因**全是我的 bug**
1. **信封形状**：文档化的是 `{"error":{"code","message","trace_id"}}`（`app.js:176` 读 `json.error`），我却找**顶层** `code` ⇒ 几乎所有响应被判违规。
2. **401 与 429**：我以为"无 token ⇒ 401"，但 admin 失败预算是**按来源**的，几次之后同一条门禁就回 **429 `too_many_failed_attempts`（带 `Retry-After`）**——只有 1 个端点在文档里写了这个码，其余 50 个没写，于是我又把它们当成"未文档化"。
3. **"错误方法"探错了方法**：`/api/v1/providers` **同时文档化 GET 与 POST**，我却把 GET 当"错误方法"去探，于是"GET 返回 provider 列表"被判违规；而且违规信息里打印的是**文档化的**那个方法，输出本身是误导的。改成"取该路径**未文档化**的方法（优先 DELETE/PATCH/PUT/POST/GET）"。
4. **最隐蔽的一条：共享加载器只复制了 `{method, path, body}`** ⇒ `auth`、`resp`、`errors` 全丢，于是"逐端点文档码对比"**是空转的**，却仍在为每一条打印"undocumented"。这是本轮第 4 个"守卫看起来在工作、其实什么都没验证"的例子（前三个在 §2au/§2av：`.md` 自证、注释自证、空日志当测量）。修法是**从源头**改：`integration/check_api_docs.py` 的 `DOCS_LOADER` 现在一并带出 `auth/resp/errors`（两个探针共用一份加载器）。

### C. 结论：**服务端契约是好的**（120 条错误响应，0 违规）
- 每个错误体都是 `application/json` + 形如 `{"error":{code,message,trace_id}}`；**没有** HTML 页、空体或裸文本（120 条错误响应逐条验证）。
- 无 token ⇒ `401 unauthorized`，预算耗尽后 ⇒ `429 too_many_failed_attempts` **且带 `Retry-After`**；**持有正确 token 的请求从不被该预算拦住**（这条断言保住了"暴力猜测不能把管理面从运维手里夺走"）。
- 畸形 JSON ⇒ `400 invalid_json`；路径上未文档化的方法 ⇒ `405 method_not_allowed`；未知路径/未知 id ⇒ `404 not_found`。
- **顺带正向验证了一条"按角色而变"的文档承诺**：`/healthz/leader` 文档写"200 主 / 503 备 / 404 非候选"，实测 **8081→200、8082→503、8084→404、单节点一次性实例→404**，四处全部吻合（只读探测，未写任何数据）。
- 两条**良性但未文档化**的行为：`DELETE /metrics` 与 `DELETE /api/v1/health` 都返回 **200**（两个探针 handler 不看方法）。**刻意不改**：改成 405 会打断那些用 POST/HEAD 做探活的负载均衡与监控；改为写进文档（见 D）。

### D. 产品内文档补了一段"全局错误契约"
既然"统一行为"不该在 51 个端点里重复，就把它们写进**产品自带的 API 参考**的引言：`admin-ui/api-docs.js` 的 `renderApiDocs()` 新增一段，文案键 `apidocs.chrome.introErrors` **四语齐全**（`admin-ui/i18n.js`），内容涵盖：错误信封形状 + `trace_id`（查日志时带上）+ 401/429（含 `Retry-After`、"正确 token 永不被拦"）+ 400 `invalid_json` + 405 + 404 + "`/healthz/leader`、`/metrics`、`/api/v1/health` 对任意方法都应答，请用 GET"。`check_i18n.js` ⇒ `OK (348 en keys, 4 locales, code↔en consistent)`（347 → 348 ✓）；`admin_ui_render.test.cjs` 仍 pass。

### E. 接线（这条探针从此每次 CI 与本机门禁都跑）
- `integration/run-crud-local.sh` 在 `check_api_docs.py` 与 `init.py` 之间插入该探针 ⇒ 进入 CI 的 `integration` 作业与本机门禁；**整链实测**：`116/116 passed` → 错误契约 `51 endpoints / 120 responses / 0 violations` → `init.py 0 warnings`，exit 0。
- **反空转下限**：实例不健康 / 文档端点少于 20 / 良构错误响应少于 40 ⇒ **exit 2**（"无法核对"），绝不返回 0；再次运行同一个实例时"首个无 token 请求应是 401"这条会打印 NOTE 说明预算已被上次跑耗尽，而不是假装验证过。
- 新测试 `integration/test_error_contract.py`（**20 条**，全部针对 B 里那四个 bug 的回归：信封、畸形/空/HTML/数组体、按角色的 `resp` 状态解析、加载器必须带 `auth/resp/errors`、全局注解是否存在），已接进 CI 的 `integration` 作业与本机门禁（`check_ci_wiring.cjs` 现识别 **8** 个仓外测试工件，原 7）。
- 探针输出的 NOTE **与文档耦合**：`uniform_behaviour_is_documented()` 检查 API 参考里那段全局注解是否还在；在 ⇒ 逐端点的省略是"不必重复"，不在 ⇒ NOTE 改口为"确实未文档化"。这样文档被删掉时，输出不会继续说"已被全局注解覆盖"。

### F. 本轮意义与自我更正
这一步把"**文档承诺的错误码**"变成了可执行断言（此前只有路由与方法、请求体两层）。四次探索性错误全部出在我自己的探针上，**没有一条是产品缺陷**——这一点也如实记录：本轮的产出是"契约被证明成立 + 一处文档补全 + 一个新守卫"，而不是"修了一个 bug"。若只看第一版输出（21 条违规）就会得出完全相反的结论，所以**"新守卫/新探针第一次红，先怀疑它自己"**这条元教训在本会话里已经第三次生效（前两次：§2au 的 14 条假漂移、§2av 的 28 个假幽灵变量）。

---
## 2ax. 第六十六轮：文档承诺的**请求体上限与失败模式**逐条实测 —— 三个是真的，第四个"旋钮"根本不存在（已补上）

### A. 被测的承诺（`dev-docs/ops.md` §8 与 §13）
| 承诺 | 出处 |
|---|---|
| `max_request_body_hard` 32 MiB ⇒ **413** + 关连接 | `ops.md:892` |
| `HYDRA_REQUEST_BODY_TIMEOUT_SECS`（默认 60 s）⇒ **408 request_body_timeout**（**总**期限，不是空闲期限） | `ops.md:893` |
| 租户 API 体超 1 MiB ⇒ **413 payload_too_large**，不排空 | `ops.md:531` |
| "如需降低内存峰值，**调低 `max_request_body_hard`**" | `ops.md:896` |

### B. 前三条：**实测全部为真**（`.acceptance/round66/body-limits.sh`，一次性实例 + Go 并发 echo 上游作 tenant auth）
```
A 33 MiB 经代理        -> 413  {"error":{"message":"request_body_too_large","type":"proxy_error"}}   （上传 34603008B 后判定）
B 发一半就不发（raw socket，HYDRA_REQUEST_BODY_TIMEOUT_SECS=2）
                       -> 2.00 秒后 HTTP/1.1 408  {"error":{"message":"request_body_timeout","type":"proxy_error"}}
                          日志：WARN downstream request body did not finish within the deadline; 408 + close tenant=t1 timeout_secs=2
C 2 MiB 打租户 API     -> 401 unauthorized（鉴权先于读体，见 D 的修正）
D 33 MiB 打管理面 API  -> 413  {"error":{"code":"request_body_too_large","message":"request body exceeds 1048576 bytes",…}}
```
**顺手发现一处文档缺口**：管理面的 1 MiB 上限（`MAX_ADMIN_BODY_BYTES`）与它的 413 码**全仓没有文档**（`ops.md:531` 只写了租户 API 那 1 MiB）。已补进 `ops.md` §13 的环境表行，并把 413 行为补进第 65 轮在 API 参考里加的那段**全局错误契约**（`apidocs.chrome.introErrors`，四语：32 MiB 可调 / 1 MiB 不可调 / `request_body_too_large` 与 `payload_too_large` / `408 request_body_timeout`）。

### C. 第四条是**假承诺**：那个"调低它"的旋钮根本不存在
`ops.md:896` 建议运维"调低 `max_request_body_hard`（超过即 413）"来降低内存峰值。实测代码：该字段**只**由 `ProxyConfig::default()`（32 MiB）提供 —— `main.rs:548-576` 是**唯一**的构造点，它只从 env 读三个字段（first-byte / stream-idle / request-body-timeout）然后 `..ProxyConfig::default()`；全仓 `grep -rn HYDRA_MAX_REQUEST_BODY\|REQUEST_BODY_HARD` **为空**，DB/API/UI 也没有该字段。也就是说：**这条建议无法执行**（同一族第 4 例，前 3 例见 §2av：`HYDRA_LOG`、`hydra.toml`、容器 `stop_grace_period`）。
**修法（本轮选择"让承诺成真"，而不是删掉承诺）**：新增 `HYDRA_MAX_REQUEST_BODY_HARD`：
1. `proxy/config.rs` 新增纯函数 `parse_max_request_body_hard`（默认 32 MiB；**拒 `0`** —— 那会让每个带 body 的请求都 413，看起来像整体故障；单位是**字节**）与它的单元测试；
2. `main.rs` 在**同一个**构造点读取（沿用该处注释的原则："Read HERE (the single construction site) or the env var is a ghost"）；
3. **端到端证伪**（`.acceptance/round66/cap-knob.sh`，2 MiB 的真实请求体，三种配置）：
```
default (不设)                  -> HTTP 200   ← 32 MiB 上限下正常通过
HYDRA_MAX_REQUEST_BODY_HARD=1048576 -> HTTP 413 request_body_too_large   ← 旋钮真的到了请求路径
HYDRA_MAX_REQUEST_BODY_HARD=0       -> HTTP 200   ← 0 被拒、回落默认（与单测一致）
```
4. 文档同步：§8 表格行改成 `HYDRA_MAX_REQUEST_BODY_HARD`（并写明字节、`0` 被拒、以及"实测：1048576 时 2 MiB 被 413、未设时通过"），§13 环境表新增一行（含"管理面/租户 API 的 1 MiB 是编译期常量、不可配"）。

### D. 并修正一条**过度断言**的威胁模型（带实测证据）
`ops.md:893` 原文称这个 408 洞"**在不持有有效 api-key 的情况下就能到达**，因此是跨租户可用性问题"。实测与代码顺序不符：`proxy.rs` 的流水线是 `resolve_tenant`(571) → 禁用检查(578) → 缺 key(656) → **认证跳** → **读全 body**(706-754) → 路由/限流。**认证在读取 body 之前**，所以：
- 认证上游不可达时（冷缓存、空认证缓存），33 MiB 的客户端拿到的是 `503 auth_upstream_unavailable`，且 `curl` 报 **`size_upload=0`** —— **一个字节都没上传**就被答复；被拒绝的请求根本走不到读体阶段。
- 但**如果该租户的 `auth_url` 对任意 key 放行**（开发/宽松鉴权常见），任何知道租户域名的调用者都能到达这一步 ⇒ 该风险**依然成立**，只是前提要写清。
- 另外实测发现认证缓存会让**已放行过**的 key 在 auth 服务挂掉后仍能到达读体阶段（这正是缓存的目的，不是缺陷，但它让"上游已挂"这个直觉失效）。
已把 `ops.md:893` 那段改成"前提明确 + 实测数字"的版本（保留风险结论，去掉无限定的断言）。

### E. 一个**守卫自证**的实证：第 62/63 轮那个计数守卫真的抓住了我这一轮
本轮给 `proxy/config.rs` 加的单元测试让 server 套件从 **544 → 545**，于是页面上的 `797` 与 `253 core + 544 server` 立刻过期。用第 62/63 轮建的工具直接跑：
```
$ node scripts/check_public_claims.cjs --measure --write
[claims] rewrote docs/index.html: 798 Rust tests (253 core + 545 server), dated 2026-09-29
[claims]   (was) advertised 797 but the suites report 798 (hydra-core 253 + hydra-server 545)
[claims]   (was) correctness-gate claim #1/#2 says 253 core + 544 server but the suites report 253 core + 545 server
```
**这是本会话第一次由工具（而不是我手改）把页面数字刷新正确**，也证明那个守卫对"真实改动引起的漂移"是敏感的（不是只能靠人为构造的证伪）。门禁里那两份 transcript 也已用新计数重跑刷新（否则本机门禁会（正确地）红）。

### F. 本轮验证
`cargo fmt --check` ok；`cargo clippy -p hydra-server --features server --all-targets -- -D warnings` ok；`cargo test -p hydra-server --features server` 全绿（server **545/0/1**，core 不变 **253/0**）；上面四个 live 实验与 cap-knob 三例；`check_i18n` ⇒ `OK (348 en keys, 4 locales)`；`admin_ui_render` pass；`check_public_claims` 对**刷新后**的 transcript ⇒ exit 0（798 / 253+545）。

---
## 2ay. 第六十七轮：把**主密钥轮换 SOP** 与**备份/恢复**真跑一遍 —— SOP 是对的，但文档里的查看命令**根本执行不了**，而备份流程**整节缺失**

### A. 方法
这一轮把两条**灾难恢复路径**从"读文档"变成"真执行"：`ops.md` §1.3-§1.5 的主密钥轮换（`HYDRA_RESEAL_SECRETS` 一次性重封）与备份/恢复。全部在**一次性实例**上做（scratch SQLite、端口 18080-18085、Go 上游当 auth/LLM double），不碰用户数据。

### B. 轮换 SOP：**文档承诺的五条全部为真**（`.acceptance/round67/rotation-drill.sh`）
| 文档承诺（`ops.md:155-201`） | 实测 |
|---|---|
| "**不要只换密钥**"：只配新钥时启动期解密失败、进程**拒绝启动**（因为能重新录入密钥的管理 API 就在同一个进程里） | 实测**退出码 1**，报错可操作：`fatal startup error error=database error: error occurred while decoding: stored key_version 1 does not match provider key_version 2; re-enter the key or rotate` |
| 一次性重封：打印报告并**退出、不提供服务** | 输出 `reseal: provider_keys=1 tenant_certs=0 already_current=0 failed=0`，exit **0**，且**没有绑定监听端口**（探测 18080 得 `000`）；DB 行 `key_version 1 → 2` |
| 重封后只用新钥即可服务 | 用 KEY_B 单独启动：健康 ✓，且**经代理的真实请求 HTTP 200** ⇒ provider 密钥确实被解开了（不只是"进程起来了"） |
| "**打不开的行只报告、绝不重写**" | 用**错的**旧钥重封：`failed=1` + `reseal FAILED: provider_key pk1: cannot open key_version 1: decryption failed (wrong master key or tampered ciphertext)`，exit **1**，且该行**仍停在 version 1**；随后用**原钥**启动仍能服务并代理 200 ⇒ 数据没被破坏 |
| 两钥窗口是"窗口" | 重封前用原钥启动正常（未重封的行照旧可读） |

**结论：轮换路径是可信的** —— 这是本会话第一次把 DR 路径端到端跑通，值得记一笔（而不是"读了代码觉得没问题"）。

### C. 真缺陷 1：轮换 SOP 第 1 步的查看命令**执行不了**
`ops.md:164/166` 原文让运维这么查"当前在用哪些版本"：
```bash
sqlite3 "$HYDRA_DB_URL" 'SELECT key_version, COUNT(*) FROM provider_key GROUP BY key_version;'
```
但 `HYDRA_DB_URL` 是 **URL**（`sqlite:///…/hydra.db?mode=rwc`），**不是路径**：直接交给 sqlite 客户端会被当成文件名 ⇒ 实测（本机没有 `sqlite3` CLI，用等价的 Python 客户端，路径语义相同）：
```
sqlite3.connect("sqlite:///…/rot.db?mode=rwc") -> OperationalError: unable to open database file
```
而这正是"决定要不要轮换、轮换到哪个版本"的那一步 —— 运维会卡在这里（或更糟：若该字面路径恰好可写，会得到一个**空库**并得出"没有需要轮换的行"的错误结论）。**修法**：文档给出可用的剥壳写法并实测通过：
```bash
DB_PATH="${HYDRA_DB_URL#sqlite://}"; DB_PATH="${DB_PATH%%\?*}"
sqlite3 "$DB_PATH" 'SELECT key_version, COUNT(*) FROM provider_key GROUP BY key_version;'
```
（实测：`DB_PATH=/…/bak-live.db`，随后读到 `provider=1 provider_key=1 tenant=1` ✓；这段剥壳逻辑现在也在 `integration/run-crud-local.sh:55` 里被真跑。）

### D. 真缺陷 2：**整份备份/恢复流程缺失**，而"照直觉 `cp` 一下"会得到**空备份**
全仓 grep：`ops.md`（以及其它运维文档）**没有任何**备份/恢复章节；而这台节点的**全部**配置（providers、**封存**的 provider 密钥、tenants、证书、limit roles、sub-tenants）都在**一个 WAL 模式**的 SQLite 文件里（`db.rs:55`）。实测"直觉做法"的后果（`.acceptance/round67/backup-test.sh`）：
```
A 运行中 cp hydra.db                    → 备份文件 4096 字节，五张表全部不可读
                                          （560 KB 的近期写入还在 -wal 里；活库有 1 provider + 1 key + 1 tenant + 2 关联）
B 运行中 VACUUM INTO 'backup.db'        → provider=1 provider_key=1 tenant=1 tenant_provider=1 tenant_model=1（与活库一致）
C 用该快照起第二个节点（同一主密钥）    → 健康 ✓，经代理请求 HTTP 200（配置**与封存密钥**都活着）
D 用该快照 + 另一个主密钥               → 拒绝服务：fatal startup error … error occurred while decoding …
```
**修法**：新增 `ops.md` §1.4 "Backup & restore"（本节此前不存在）：明确 ❌ `cp` 为什么不行（附实测 4096 字节/表不可读）、✅ `VACUUM INTO`（**无需停机**，附实测行数与"第二个节点从快照起来并代理 200"）、恢复步骤（停节点、放回 `data/hydra.db`、清掉旁边的 `-wal`/`-shm`、**同一个 `HYDRA_ENCRYPTION_KEY`/`_VERSION`**）、以及最关键的警告 —— **没有那把密钥，备份就是废纸**（附实测的拒绝服务报错），并要求把密钥与数据库**分开存放**。同时写明刻意不覆盖的范围：ClickHouse 用量数据（用 CH 自己的工具备）、集群副本（从 leader 重建，`db::restore_config`）、`data/` 权限（§1.3）。

### E. 让文档的备份步骤**被 CI 真跑**
照本会话的做法，"文档化的步骤"要有执行者：`integration/run-crud-local.sh` 现在在 CRUD 套件之后、对**同一个活库**执行 §1.4 的 `VACUUM INTO`，并断言快照的**每张表行数与活库一致**且非空（不一致/为空 ⇒ 脚本 exit 1）。
整链实测（`integration/run-crud-local.sh`）：`116/116 passed` → `[it-crud] backup snapshot {'provider': 2, 'provider_key': 0, 'tenant': 0, 'tenant_provider': 0, 'tenant_model': 0} vs live {…同样…}` + `backup snapshot is complete` → 错误契约 `51 endpoints / 120 responses / 0 violations` → `init.py 0 warnings`，exit 0。（键/租户计数为 0 是 CRUD 套件自己清理的结果；断言的是"快照 == 活库且非空"，不是某个绝对数。）

### F. 本轮验证
轮换 drill 五条断言全中；备份 drill 四组对照全中；`bash -n` 两份脚本 ok；`check_documented_env`（43 个文档化 env 名全有引用，含上一轮新增的 `HYDRA_MAX_REQUEST_BODY_HARD`）exit 0；`check_public_claims` 对刷新后的 transcript ⇒ exit 0（798 / 253+545）；整链 `run-crud-local.sh` exit 0。**未做**：没有验证"活库正在写入时 `VACUUM INTO` 的并发一致性"（只验证了空闲活库；SQLite 文档称其为一读事务快照，但本机没有做高并发下的对照实验）；没有实际跑 `sqlite3` CLI（本机未安装，缺陷与修法都用等价路径语义的 Python 客户端验证，已在文中注明）。

---
## 2az. 第六十八轮：文档承诺的**零停机升级**从来没能工作过 —— 两个独立原因，都已修好并有 CI 断言

### A. 被验证的承诺
`ops.md` §2：「Pingora 内建 socket 交接：`kill -SIGQUIT <pid>`（旧进程优雅排空并把监听 socket 交给新进程）+ `hydra -u`（新进程**继承**监听 socket）；旧进程上的在途请求跑完，新连接进新进程」；§2.3 还断言压测探针「必须看到 0 个由连接重置导致的非 2xx，且新进程 bind 期间只有一次**亚秒级**停顿」。

### B. 实测：**照文档做，等于把节点搞下线**
`.acceptance/round68/upgrade-drill.sh`（一次性实例、端口 18080/18081、50 ms 一次的探针持续打数据端口）：
```
before: 新进程 (-u) → ERROR pingora_core::server::transfer_fd: No incoming socket transfer…
                       ERROR hydra: fatal pingora startup error error=HYDRA_LISTEN=127.0.0.1:18080:
                             cannot bind … Address already in use (os error 98); refusing to start
         旧进程     → INFO pingora_core::server: SIGQUIT received, sending socks / Trying to send socks
         （没有任何东西在听那枚 upgrade socket）
```
**后果**：新进程直接退出；旧进程继续排空，而它一进入 `Graceful shutdown` 就**关掉数据监听**（实测约 SIGQUIT 后 5 秒），于是从那一刻起到排空结束（~31.6 s）**端口是死的** —— 照文档升级会得到一段没有任何监听者的窗口，而不是"零停机"。

### C. 根因之一：`-u` 根本没到 Pingora 手上
`main.rs` 用 `Server::new_with_opt_and_conf(Some(Opt::default()), conf)` 且**从不读 argv**（`grep -n "args()\|parse_args" crates/hydra-server/src/main.rs` 当时为空）。而 `-u` 是被 **Pingora 的 bootstrap** 消费的，不是 `ServerConf`：`Bootstrap::new` 取 `(opt.test, opt.upgrade)`，只有 `upgrade == true` 才 `load_fds(upgrade)` 去继承监听 socket（pingora-core 0.8.1 `server/bootstrap_services.rs:148`；`server/mod.rs:367` 的 `listen_fds()` 在 SIGQUIT 时把 socket 发到 `upgrade_sock`）。
**修法**：新增纯函数 `upgrade_requested(&[String])`（只认 `-u` / `--upgrade`，`--upgrade=1` 与 `-U` 都不认——宁可让打错的人拿到响亮的 "address already in use"，也不要静默降级成"没升级"），在**唯一**构造点传入 `Opt { upgrade, ..Default::default() }`，并在 upgrade 模式下打一行 INFO。**测试**：把它放进**不带特性门**的 `#[cfg(test)] mod upgrade_flag_tests`（原有的 `mod tests` 带 `cluster-redis` 门，放那儿在默认 `--features server` 作业里根本不会跑）。

### D. 根因之二：hydra 自己的**启动前 bind 探测**会掐死交接
修好 `-u` 之后，交接开始了（旧进程终于日志 `listener sockets sent`、新进程 `Bootstrap done`），但新进程紧接着仍然死：
```
ERROR hydra: fatal pingora startup error error=HYDRA_LISTEN=127.0.0.1:18080: cannot bind …
ERROR hydra: fatal pingora startup error error=HYDRA_ADMIN_ADDR=127.0.0.1:18081: cannot bind …
```
这两条不是 Pingora 的 service bind，而是 hydra **自己的 `probe_bind` 预检**（第 62 轮那类"启动前证明能绑"的守卫）—— 它在 upgrade 模式下**必然**失败，因为此时那两个地址**正是**被前任合法占着的，Pingora 会从 FD 表里继承（`listeners/l4.rs:326` 用地址字符串查表，命中则 `from_raw_fd`，不命中才 `bind`）。
**修法**：三处 `probe_bind`（明文入口、TLS、管理口）在 upgrade 模式下**让位**（数据面与管理口各加 `if !upgrade`，TLS 用 `if upgrade { Ok(()) } else { probe_bind(...) }`），并写明理由与实测。
**过程里我自己踩的一个坑值得记**：第一遍 `grep -n "probe_bind\|refusing to start" … | head -12` **把第三处（管理口，行 1464）截掉了**，于是"修好"后新进程只是把致命错误从 18080 换成了 18081。**教训：用 `head` 截 grep 输出等于给自己制造盲区** —— 现在 `grep -n "probe_bind(" crates/…/main.rs` 不带 head，三处全部确认已让位。

### E. 修好后的实测（`.acceptance/round68/upgrade-drill.sh`，release 二进制）
```
624 次探针（50 ms 一次，跨越 SIGQUIT、交接、旧进程完整排空+退出）→ 连接被拒 0 次、非监听者应答 0 次
旧进程退出后数据端口仍被服务（HTTP 200）
旧进程日志：SIGQUIT received, sending socks → listener sockets sent
新进程日志：upgrade mode: inheriting the listening sockets… → Bootstrap done → upgrade mode: not probing …
回归项：不带 -u 的进程仍然响亮拒绝：cannot bind 127.0.0.1:18080: Address already in use (os error 98) … refusing to start
```
**反向证伪（实测）**：把 `upgrade_requested` 改成恒 `false` 后重建 ⇒ 同一套断言立刻红：`FAIL: the data port went dead after the old process exited`、`FAIL: a process started WITHOUT -u stayed alive`、`FAIL: the old process never logged 'listener sockets sent'`、`127 connection refusals`；恢复后 ⇒ `PASSED (293 probes, 0 refusals)`。

### F. 把这条路径变成 CI 断言（`scripts/handover.test.sh`）
新脚本把 §2.1/§2.3 变成可执行契约：起旧进程 → 50 ms 探针压数据端口 → `SIGQUIT` → 0.3 s 后以 `-u` 起新进程 → 等旧进程退出 → 断言数据端口**仍被服务**、旧进程日志有 `listener sockets sent`、**0 次连接被拒**、并且"不带 `-u` 必须响亮拒绝"；用 `HYDRA_SHUTDOWN_DRAIN_SECS=1` 把全程压到 ~15 s（实际 293 次探针 / 0 拒绝）。已接进 CI 的 `integration` 作业（那里已经构建二进制）与本机门禁（脚本工件 16 → **17**）。§2.2 里原来把 `address already in use` 归因于"socket 路径不可写"，现在改成**要读旧进程日志来区分**（有 `listener sockets sent` = 交接发生了；没有 = 没发生），并补上"要尽快起新进程（旧进程等接收者约 1 s，实测 0.3 s 后起、信号后 1.0 s 完成发送）"。§2.3 的"亚秒级停顿"也改成实测口径：**交接模式下根本没有 bind**。

### G. 本轮验证
`cargo fmt --check` ok；`cargo clippy -p hydra-server --features server --all-targets -- -D warnings` ok（无 warning）；`cargo test -p hydra-server --features server` 全绿（含新增的 `upgrade_flag_tests::the_upgrade_flag_is_recognised_exactly`）；`scripts/handover.test.sh` PASSED；`check_ci_wiring` OK（17 个脚本工件全被执行）；`check_public_claims` 对**重新测量**的 transcript exit 0（服务端测试数变了，用第 62 轮那套 `--measure --write` 工具刷新页面，而不是手改）。

---
## 2ba. 第六十九轮：把**吊销延迟**（`ops.md` §5.2）实测 —— 三条承诺全部成立，并把它量化进运维手册

### A. 被测承诺
`ops.md` §5.2「已吊销的 key 还能用多久」给出三条：
1. **正常情形**：等失效广播收敛（响应里 `fleet.state: applied` 表示每个活节点都已应用；`202` 会点名 `lagging` 节点）；
2. **消费者不前进的节点**（consumer 任务死了 / Redis 分区 / 不在 registry）：没有东西清它的 L1，只能等条目自身的 TTL —— 而那个 TTL 过去是**租户自己**通过 `expires_in` 要的，现在由 `HYDRA_AUTH_ALLOW_TTL_MAX_SECS`（默认 **300**）钳住；"调低它能让吊销更快生效"；
3. **`hydra_invalidation_consumer_stalled_seconds`**（>60 s 告警）是第 2 种情形的信号；"这个旋钮**从不**钳 DENY（deny 本来就很短）"。

方法：写了一个**可切换**的租户鉴权 double（`.acceptance/round69/auth_double.py`：从文件读 allow/deny，**不重启**就能翻转，并逐次记录调用），租户请求 `expires_in=3600`（远大于 knobs），逐项对照实测（`.acceptance/round69/revocation-drill.sh`、`caseB.sh`）。

### B. 实测结果（全部与文档一致，并且拿到了具体数字）
| 情形 | 实测 | 结论 |
|---|---|---|
| allow 已缓存、租户要 3600 s、`HYDRA_AUTH_ALLOW_TTL_MAX_SECS=2` | 网关**继续放行 2.07 s** 后开始拒绝；鉴权服务**恰好被再调用 1 次**（1 → 2） | 上限确实按 cap 生效，缓存也确实在工作 |
| 同一流程、cap=3600（对照） | 翻转 deny **4 s 后仍在放行** | 证明上一行是被 cap 钳住的，不是别的原因 |
| 显式失效（`DELETE /api/v1/auth/cache` + `tenant_id`），此前 allow 可缓存一小时 | **下一条请求立即被拒**；响应体 `{"invalidated":1,…,"fleet":{"state":"single_node","nodes_total":1,…}}` | §5.2 第 1 条成立；`single_node` 这一档也与文档一致 |
| **DENY** 已缓存、随后鉴权服务改为放行 | 节点**继续拒绝 30.41 s**（全程只有 2 次鉴权调用） | "deny 本来就很短"= 固定 `deny_ttl` 30 s（`http.rs:495`），**本旋钮不钳它** |

**因此事故口径可以写死**：显式失效**立即**生效；不做失效时，**allow 最多活 `min(租户 expires_in, cap)`，deny 最多活 30 s**。这三条都是可测的，已按实测写进 `ops.md` §5.2（附表格）。

### C. 顺手核到的一处**监测盲区**（文档层面）
§5.2 让运维"盯 `hydra_invalidation_consumer_stalled_seconds`、>60 s 告警"。实测：**单节点实例的 `/metrics` 里根本没有这个族** —— 它是 `IntGaugeVec`，只有集群事件消费者给它设了子项才会被导出（`metrics.rs:349` 注册、`:734` 赋值），单节点没有消费者 ⇒ 族不存在。**这不是缺陷**（第 2 种情形本就只在集群里发生），但文档没写"仅集群模式"，而**对一条永不出现的序列配告警规则等于监控静默失效**（本会话第 12 轮在 `invalid_weight` 上踩过同族问题）。已在 §5.2 补上这句限定与实测依据。
**（补验完成）** 集群模式下该序列**确实出现**且语义正确：构建 `server,cluster-redis,usage-clickhouse` 全特性二进制后，以 `HYDRA_ROLE=leader` + `REDIS_URL=redis://127.0.0.1:6380/9` 起一个控制节点（`HYDRA_CONTROL_URL` 指向自己、`HYDRA_CLUSTER_TOKEN` 就位），`/metrics` 里拿到：
```
# HELP hydra_invalidation_consumer_stalled_seconds How long this node's applied watermark has not advanced (alert > 60)
hydra_invalidation_consumer_stalled_seconds{node="cl-node-1"} 7
hydra_control_poll_total{result="ok"} 14 / {result="error"} 1
hydra_registry_nodes{...}
```
⇒ 文档的"(>60 s 告警)"信号在集群里是真的；单节点里它不存在（见上）。**并且没有碰用户的 ClickHouse**：sink 指向死端口 `http://127.0.0.1:18999` 且全程不发代理请求（日志里 clickhouse 相关 0 行），只在 `/metrics` 上做只读检查。

### D. 本轮改动
- `ops.md` §5.2：新增"实测（2026-09-29）"表格（上面四行）+ 事故口径一句话 + 指标"仅集群模式"的限定与理由。
- 夹具（scratch，与其它门禁脚本同类）：`.acceptance/round69/{auth_double.py,revocation-drill.sh,caseB.sh}`。

### E. 本轮验证
四组对照全部拿到可复现数字（2.07 s / 4 s 对照 / 立即失效 / 30.41 s）；`bash -n` 两份脚本 ok；`python3 -c ast.parse` ok；文档改动后 `check_documented_env`、`check_public_claims` 等守卫全绿（见 §F 的总收尾）。

---
## 2bb. 第七十轮：证书**热更新 + SNI 多租户**（`ops.md` §3）实测 —— 承诺全部成立，并把它变成 CI 断言；证伪过程本身抓到我两个错误

### A. 被测承诺
- §3.1「更新租户证书（**热** —— 无需重启）」：PUT 会触发 `ConfigStore::reload_all()` 与 W4b 证书重载契约；**新**握手用新证书；**已有**连接不受影响（保留协商时的证书）。
- §3.2/§3.3：用 `openssl s_client` 验证；`POST /api/v1/reload` 重新解析证书并回报快照计数。

### B. 做法与结果（`.acceptance/round70/cert_drill.py`，后提升为 `integration/test_cert_reload.py`）
用 `openssl` 生成三张自签证书并以 `O=`（`OLD-CERT`/`NEW-CERT`/`OTHER-TENANT`）与有效期区分；起一个带 `HYDRA_TLS_LISTEN` 的一次性实例；用 Python `ssl` 读每次握手的**对端证书指纹**（比解析 `openssl s_client` 文本更可靠）。实测：
| 检查 | 结果 |
|---|---|
| 建租户（带旧证书路径）后**不调** `/reload` | 新握手就是 `O=OLD-CERT`（写后 reload 生效） |
| **只 PUT** 新路径（不 `/reload`、不重启、pid 不变） | 新握手变成 `O=NEW-CERT` ✔ 热更新 |
| **轮换前建立并保持的连接** | 轮换后仍报 `OLD-CERT`（未被重新协商）✔ "已有连接不受影响" |
| SNI：第二个租户 `other.local` | 拿到 `OTHER-TENANT`；`load.local` 仍拿自己的新证书 |
| `POST /api/v1/reload` | `{"status":"reloaded","changed":false,"version":3,"tenants":2,…, "certs":2}` ✔ §3.3 |

**顺手测到两条运维要点（已写进 §3.1）**：
1. **匹配不到租户的 SNI 在握手阶段就被拒绝** —— 没有默认证书（`hydra::tls` 打 `no cert matched SNI and no default configured; handshake will be rejected`）。⇒ **用 TLS 探数据端口的 LB 健康检查必须带真实 `server_name`**，否则会把健康节点判死；改探免鉴权的 `/healthz` 可完全避开。
2. `HYDRA_TLS_LISTEN` 已设但还没有租户证书时节点照常起，日志明说：`HYDRA_TLS_LISTEN is set but no tenant cert is loaded yet; TLS handshakes will fail until a certificate is written. No restart is needed once one is written.`

### C. 本轮最有价值的部分：**证伪过程抓到我两个错误**
1. **第一次证伪居然"通过"了** —— 我在 `tenant_config_api.rs` 里打补丁停掉"写后 `reload_all`"，把探针门控在 `HYDRA_FALSIFY_NO_RELOAD` 上，重跑测试仍是 PASSED。原因：**租户 PUT 走的是 `handlers.rs`**（全仓有**两份**同名 `reload_best_effort`：`handlers.rs:209` 与 `tenant_config_api.rs:781`，语义完全相同、只有日志 `target` 不同 —— `hydra::admin` vs `hydra::tenant_config_write`）。把探针挪到真正被调用的那份后：**探针 ON ⇒ 7 条 FAIL**（证书完全不生效、每次握手被拒），**探针 OFF ⇒ PASSED**。
2. 因此给探针加了"**证明自己执行**"的输出（`tracing::warn!("FALSIFY(r70): …")` 并统计日志行数）—— 一次"探针没跑却宣布通过"的假阴性就是这样被抓出来的。**教训（与第 68 轮"`head` 截 grep"同族）：证伪探针必须可观测地证明自己执行过。**
3. 顺手记录一处**单一所有者**味道：两份 `reload_best_effort` 语义重复（只有日志 target 不同）。**本轮不改**（属重构，需先决策），仅记录。

### D. 测试自身的稳健性（三处都真踩过）
第一次提升为 CI 测试时连撞三次，都已修：
- **残留实例会顶掉端口并冒充"健康"**：早前崩溃留下的实例（甚至是另一个 profile `target/release` 起的）继续占用端口，健康探测答 200，于是所有测量都在描述**陈旧进程**（表现为"租户已存在 409"、"探针一次都没跑"）。现在：按**进程名** `pkill -x hydra` 清理 + 启动前断言**端口空闲** + 用 `ss -ltnp` 断言**监听端口属于我们自己的 pid** + `/api/v1/tenants` 必须为空 ⇒ 否则拒绝测量。
- **握手被拒从"抛栈"改成"报告为 FAIL"**：否则一次真实的失败看起来像夹具崩溃（我的第一次证伪就长这样）。
- **`pkill -f <路径>` 会杀掉我自己的 shell**（命令行里含该字符串）⇒ 改用 `pkill -x hydra`。

### E. 交付与接线
- 新增 `integration/test_cert_reload.py`（CI `integration` 作业 + 本机门禁；仓外测试工件 8 → **9**）。
- `ops.md` §3.1 增补"实测 2026-09-29"段（含上面两条运维要点）。
- **产品代码净变化为零**：`grep -rn FALSIFY crates/` 为 0，探针已移除并重建验证（恢复后测试 PASSED）。

---
## 2bc. 第七十一轮：§2.3 的"升级后冒烟"被实测改掉一半（**计数器不连续**），并量化 `systemctl restart` 的真实停机

### A. 被测承诺与结果
`ops.md` §2.3 的冒烟清单原文最后一行："After: `GET /api/v1/health` → 200, `/metrics` → **counter continuity**"。

**实测**（`.acceptance/round71/upgrade_metrics.sh`；一次性实例 + 真实租户/路由 + Go 上游；因为 404 的请求根本不进 `hydra_requests_total`，第一版用未配路由的探针量出 0/0 —— 这就是"测量前先确认被测对象真的在被计数"的一次教训）：
```
30 次真实请求后            hydra_requests_total = 31
SIGQUIT + hydra -u 交接后  hydra_requests_total = 0      ← 新进程、新注册表
```
⇒ 交接的终点是**全新进程**，Prometheus 注册表也是新的：**不存在"计数器连续"这件事**。`rate()`/`increase()` 会自己处理 reset（Prometheus 对 counter reset 有专门处理），把 reset 读成"升级丢了流量"是虚惊一场。
（对照：**进程态**会重置，**库态**不会 —— 见 C。）

### B. 顺带量化了另一条升级路径：`systemctl restart` **不是**零停机
§1.2 建议 `KillSignal=SIGQUIT`（让 `systemctl restart` 发 SIGQUIT）。实测这条路径的代价：
```
drain 设为 2 s：端口 313 次探针里 125 次被拒（≈6.25 s 拒绝窗口）
                SIGQUIT → 替代进程开始服务：17.54 s（含脚本 0.5 s 轮询粒度）
默认 20 s drain ⇒ 拒绝窗口 ≈ 25 s（= drain + pingora 最后 5 s 步）
对照：§2.1 的交接 = 624 次探针 0 拒绝（第 68 轮实测，CI 里由 handover.test.sh 守着）
```
原因：旧进程一进入 graceful shutdown 就**关掉数据监听**，而 systemd 是"先停后起"。⇒ `KillSignal=SIGQUIT` **只让排空变优雅，不产生交接**；想零停机就得按 §2.1 的顺序（旧进程还在排空时就把 `hydra -u` 起起来，通常需要一个 wrapper），否则就接受这段间隙、在低峰升级。两条都已写进 §2.2（新增一条 caveat）与 §2.3。

### C. 真正值得断言的"升级后"检查，也补了实测
§2.3 现在写的是：`/health` 200、metrics 端点可达、**配置快照未变**。最后一条实测（库里先建一个 provider，再走完整交接）：
```
before: POST /api/v1/reload -> version 1, providers 1
after : POST /api/v1/reload -> version 1, providers 1
```
（第一次我用空库测出 0→0，等于什么也没测 —— 已改成"先写数据再比"。）

### D. 让 CI 守住这段措辞
`scripts/handover.test.sh` 在交接完成后新增断言：**metrics 端点仍能应答且含 `hydra_*` 族**（实测 24 行），并打印一句"计数器在新进程里从零开始（符合预期）"。这样文档的措辞不会再悄悄漂回"计数器连续"，而"升级后 metrics 挂了"这种真回归会立刻红。

### E. 本轮改动与验证
- `dev-docs/ops.md`：§2.2 新增一条 caveat（含两组实测数字）、§2.3 改写"After"清单（含 reset 说明与快照不变的实测）。
- `scripts/handover.test.sh`：+1 条 metrics 存活断言（PASSED，293 探针 / 0 拒绝）。
- scratch：`.acceptance/round71/upgrade_metrics.sh`。
- **未改产品代码**（`cargo fmt --check` / `clippy -D warnings` / bin 测试均绿；全部守卫与套件绿）。

---
## 2bd. 第七十二轮：**两节点集群 HA 实测**（转发/注册表/故障切换 17.7 s/edge 角色面）+ 官方集群编排**三个服务都没有健康检查**（已补，并加角色感知守卫）

### A. 被测承诺（`dev-docs/cluster.md`）
- §16：`leader` 角色节点**读本地 + 把变更转发给 active**（租约持有者是唯一写者）；
- §22：故障切换 = 租约过期 + 选举 tick，**"实测 ≤ 租约 15s + tick 5s，约 11–18s"**；
- §35-45：`HYDRA_CONTROL_URL` 只是快照轮询端点，**转发目标按租约持有者从注册表实时解析**；
- §115：转发是 **fail-closed**（租约持有者指针消失 ⇒ standby 的管理写**全部 503**，无静态回退）。

### B. 做法与结果（`.acceptance/round72/cluster_drill.sh`，Redis 6380 上的一次性 DB、端口 18092-18097、ClickHouse 指向死端口以免写入你正在运行的实例）
```
PASS  node A 持有租约（/healthz/leader = 200）；PASS  A 管理口健康
PASS  node B 是备用（同一探针返回 503）
PASS  发给 B（standby）的管理写被转发：HTTP 201
PASS  该写入在 **A** 上可见（证明确实转发到了 active，而不是本地写）
PASS  /api/v1/cluster/status: lease_holder=node-a，两个节点都在注册表里（alive=true）
PASS  **硬杀（SIGKILL）A 后 B 在 17.7 s 被提升** —— 落在文档的 11–18 s 窗口内（首次实测这条"实测"宣称）
PASS  被提升的 B 之后**本地接受写入**（201）
```
**edge 角色面（同时也是待决策 D-8 的直接证据）**：
```
/healthz        无 token: 200   带 token: 200
/readyz         无 token: 200   带 token: 200
/metrics        无 token: 401   带 token: 200
/api/v1/health  无 token: 404   带 token: 404      ← edge 不提供管理 API
/api/v1/providers 404 / 404
```
⇒ 与文档"edge 只有 `/metrics /healthz /readyz`"一致；**D-8 的前提得到实测确认**：抓 edge 指标**必须**持 admin token（无 token 恒 401），而官方集群编排**不给 edge admin token**。

### C. 真缺陷：官方集群编排里**三个 hydra 服务都没有 healthcheck**（已修）
用 `docker compose config --format json` 逐个服务核对：
```
docker-compose.yml        hydra            healthcheck=yes（/api/v1/health + token）
docker-compose.cluster.yml hydra-control-a  NONE
                           hydra-control-b  NONE
                           hydra-edge       NONE      ← 官方拓扑零健康监督
docker-compose.local.yml   hydra-a/b        yes（/api/v1/health + token）
                           hydra-c          yes（**/healthz**，免鉴权）
```
后果：官方集群拓扑下**没有任何东西监督节点存活**（启动期的 bind 预检只在启动那一刻负责；运行中监听挂掉、进程假活都不会被发现）。**修法**：给三个服务补上 **按角色** 的 healthcheck —— control-a/b 用 `curl -fsS -H "Authorization: Bearer $HYDRA_ADMIN_TOKEN" http://127.0.0.1:8081/api/v1/health`（不带 token 会 401 而永远 unhealthy），**edge 用 `curl -fsS http://127.0.0.1:8081/healthz`**（因为 `/api/v1/health` 在 edge 上是 **404**，照 control 抄会**把健康的 edge 全判成 unhealthy**）；`docker compose config -q` 通过，渲染后逐服务核对已生效。`dev-docs/cluster.md` 新增一节记录"角色 ↔ 探针路径"的对应与实测依据。

### D. 新守卫 `scripts/check_compose_health.cjs`（+9 条断言的测试）
对**渲染后**的 compose（锚点/`extends` 藏不住东西）断言：①每个 hydra 镜像服务都有 healthcheck；②`HYDRA_ROLE=edge` 的服务必须探 `/healthz` 或 `/readyz` 且**不得**探 `/api/v1/health`；③其余角色必须探 `/api/v1/health` **并带** `Authorization: Bearer`；④hydra 服务少于下限即 exit 2（不空过）。
**反向证伪（真实文件）**：把 cluster.yml 里 edge 的探针改成 `/api/v1/health` ⇒ `exit=1`，报 `hydra-edge (role=edge): /api/v1/health is 404 on an edge (measured) …`；恢复 ⇒ exit 0。已接进 CI `scripts` 作业与本机门禁（脚本工件 17 → **19**）。

### E. 本轮验证与未覆盖
`check_compose_health` 对三份文件 7 个服务全绿；其测试 9 条全过；`check_ci_wiring` OK；本轮**未改产品代码**（只在编排/文档/守卫层面）。**未覆盖**：没有验证"edge 轮询失败自动经注册表旋转到新 active"（文档 §22 称数据面/控制面均无感）—— 那需要在故障切换期间持续压 edge 的数据面并观察其自动切换，属于下一步；也没有做 grace 优雅停机下的切换计时（文档的 11–18 s 是**硬失败**口径，本轮已按其原意用 SIGKILL 验证；优雅 SIGQUIT 会让旧节点在排空期间继续持租约，届时提升更慢，值得下轮补测）。

---
## 2be. 第七十三轮：把上轮记下的缺口补上 —— **edge 在故障切换期间是否真的"无感"**（实测：数据面 355/355 全 200，控制面自己旋转到新 active）

### A. 被测承诺
`dev-docs/cluster.md` §22 原文（上轮只验了前半句）："自动故障切换：active 死亡 → 租约过期 → 合格候选提升（实测 ≤ 租约 15s + 选举 tick 5s，约 11–18s），**edge 数据面与控制面均无感**（edge 轮询失败自动经注册表旋转到新 active；standby 也按租约持有者轮换 —— 即使它的静态 `HYDRA_CONTROL_URL` 指向旧 active）"。

### B. 结果（`.acceptance/round73/edge_failover.sh`：active A + standby B + edge E，E 的 `HYDRA_CONTROL_URL` **故意指向 A**）
```
PASS  A 持有租约；B 为备用；E 的 /healthz 200；E 的数据面代理请求 200（已从 A 取到快照）
PASS  硬杀（SIGKILL）A ⇒ B 在 18.2 s 被提升（上轮 17.7 s；文档带 11–18s 的上沿）
PASS  **edge 控制面自己旋转到新 active**：在新 active 上改配置后，E 自己的
      hydra_control_snapshot_version 6 → 7，hydra_control_poll_total{result="ok"} 2 → 17
      —— 尽管 E 的静态 CONTROL_URL 指向的是已经被杀死的节点
PASS  **edge 数据面"无感"**：整个切换窗口以 20 rps 持续压 E 的数据端口
      ⇒ 355 次探针 **355 次 200**、**0 次连接被拒**、**0 个非 200**
```
两次独立运行的提升耗时：**17.7 s / 18.2 s** ⇒ 文档那个"约 11–18s"的实测口径得到确认（且处于区间上沿，粒度由 15 s 租约 + 选举 tick 决定）。已把这段复核（含数字）写进 `cluster.md` §22 紧邻原句。

### C. 本轮真正的"坑"：共享测试 Redis 不是私有的（我的夹具因此造出一支**从未被初始化**的集群）
第一次运行的结果是：A 从未当选、edge 全部请求 404、主节点被提升失败——**看起来像一堆产品缺陷**。日志给出了真相：
```
INFO hydra: leader election started node_id=node-a lease_ms=15000
INFO cluster::control_client: control poll target rotated to lease holder
     from=http://127.0.0.1:18092 to=http://127.0.0.1:14101      ← 一个不存在的节点
WARN cluster::control_client: control poll failed; keeping last-known-good snapshot
```
即：我随机挑的 Redis DB 里**残留着上一轮（或别的进程）留下的租约/注册表条目**，其中"租约持有者"的 control_url 是 `:14101`。于是 A 忠实地把控制轮询旋转到那个死节点、并且**因为自己不是 active 而把管理写 fail-closed 转走**（正是 cluster.md §115 记录的设计），我的种子请求全部没落地 ⇒ edge 自然 404。
**修法**：两个集群 drill（`round72/cluster_drill.sh`、`round73/edge_failover.sh`）在启动前都**显式清空自己用的 DB**：本机没有 `redis-cli`，于是用 Python 走 RESP 直接 `SELECT`+`FLUSHDB` 并打印 `keys N -> 0`。这既修掉了"随机 DB 索引撞上残留数据"的隐患，也让 drill 的输出里留下了"我用的是哪个 DB、清空到几"的证据。
**教训（与"探针必须证明自己执行过"同族）**：**共享的外部依赖必须有显式的初始化步骤**；否则"环境脏"会被误读成"产品坏"。第一次运行如果我只看 FAIL 行就下结论，本会话就会写下一份错误的缺陷报告。

### D. 未做（明确记录）
- **没有把集群 drill 接进 CI**：故障切换本身受 15 s 租约支配（两次实测 17.7/18.2 s），加上三节点启动与 55 s 的持续压测，单次 ~60 s 且对端口/时序敏感。可行的折中是"把租约降到 5 s，只断言'转发 + 注册表 + 提升'三条"（~25 s），但那是另一轮的事；本轮把 drill 保留为可复现的 scratch 夹具并在此记录。
- 优雅（SIGQUIT）停机下的提升计时仍未测：文档的 11–18s 是**硬失败**口径，两轮都用 SIGKILL 按原意验证。

---
## 2bf. 第七十四轮：把第三、七十三轮的**集群 HA 手工验证**变成 CI 断言（6.4 s 跑完；证伪时又踩了一次"探针打错函数"）

### A. 为什么要这一步
第 72/73 轮用 scratch drill 验证了集群 HA（转发、注册表、故障切换 17.7/18.2 s、edge 无感），但那是**手工夹具**。本会话的纪律是"文档化的步骤要有执行者"，所以这一轮把它做成**能在 CI 里跑的集成测试**。

**关键技巧**：`HYDRA_LEADER_LEASE_MS` 的合法区间是 **1000–600000 ms**（`main.rs:90-94`），所以把租约缩短到 **3 s**、轮询 250 ms，提升就从 ~18 s 变成 **3.8 s** —— 整套 drill（三节点 + 转发 + 提升 + edge 透明性）**6.4 秒跑完**，完全适合 CI。

### B. `integration/test_cluster_ha.py`（已接进 CI 的 `live-deps` 作业与本机门禁）
14 条断言，全部通过（实测输出）：
```
PASS  node A holds the leader lease            (lease 3000 ms)
PASS  node B is up and reports standby         (/healthz/leader=503)
PASS  a write to the STANDBY is accepted (forwarded)          HTTP 201
PASS  the forwarded write is visible on the ACTIVE node       HTTP 200
PASS  cluster/status names the lease holder
PASS  edge /healthz answers token-free
PASS  edge /api/v1/health is 404 ⇒ healthchecks must use /healthz
PASS  edge /metrics is refused without a token and served with one   ← D-8 证据
PASS  control: an unknown domain on the edge is 404            （"没有配置"长什么样）
PASS  the edge serves the data plane (routed from its snapshot)      HTTP 503
PASS  the standby is promoted after the active dies            3.8s
PASS  the edge kept serving across the failover (never 404/refused)  statuses=[503]
PASS  the promoted node accepts writes locally                 HTTP 201
PASS  the edge follows the NEW active by itself (snapshot version 7 → 8)
```
两个设计要点：
- **断言"路由"而不是"上游健康"**：夹具里 tenant 的 `auth_url` 与 provider endpoint **故意指向死端口**，所以"能被路由"的请求回 503/502，而 **404 表示"我的快照里没有这个租户"、0 表示监听没了**；断言写成"非 404 且非 0"，并额外加了一条**对照**（未知域名在 edge 上确实是 404）。第一版我把期望写成 200/502，于是把 503 误判为失败 —— 记录在此以免后人重犯。
- **必须显式清 Redis DB**（第 73 轮的教训）：测试自己 `SELECT`+`FLUSHDB` 并打印 `keys N -> 0`；Redis 不可达时 **exit 2（无法核对）**，不是静默跳过。

### C. 证伪：又踩了一次"探针打错函数"（第二次）
为证明这条测试真能失败，我在**转发路径**上打补丁 `HYDRA_FALSIFY_NO_FORWARD`——**测试仍然 PASSED**。原因：补丁打在 `cluster/forward.rs` 的 `forward_config_write`，而**管理面 standby 路径实际调用的是 `forward_mutation`**（`admin/mod.rs:749` → `maybe_forward_mutation`）。把探针挪到真正被调用的函数后：
```
probe ON  ⇒ FAIL a write to the STANDBY is accepted (forwarded) — HTTP 502
            FAIL the forwarded write is visible on the ACTIVE node
probe OFF ⇒ CLUSTER HA: PASSED
```
**教训（与第 70 轮"两份同名 reload_best_effort"同族，这是第二次）**：证伪探针必须打在**被调用的那个函数**上 —— **先找 call site，不要凭函数名猜**；并且探针要能证明自己执行过（第 70 轮的做法）。本轮探针已移除，`grep -rn FALSIFY crates/` = 0，恢复后测试 PASSED。

### D. 接线与验证
CI：新步骤加在 **`live-deps` 作业**（那里本来就有真 Redis 6379 与真 ClickHouse），先 `cargo build … --features server,cluster-redis,usage-clickhouse --bin hydra` 再 `HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6379 python3 integration/test_cluster_ha.py`；本机门禁新增 `gate "cluster HA"`。`check_ci_wiring.cjs` 现在识别 **10** 个仓外测试工件（原 9），全部有执行者。**本轮产品代码净变化为零**。

---
## 2bg. 第七十五轮：`HYDRA_TRUSTED_PROXIES` 实测 —— 文档（连代码里的告警文案）把 catch-all 的后果**说反了**

### A. 被测承诺（`ops.md` §5 环境变量表 + 部署清单）
`HYDRA_TRUSTED_PROXIES` 是"信任其 `X-Forwarded-For` 的反向代理"的 IP/CIDR 名单，未设置 = 什么都不信；裸 IP 视作 /32；失败预算按**源 IP**（与**令牌摘要**独立）计（`HYDRA_TENANT_API_AUTH_FAIL_LIMIT_PER_MIN` 默认 10，超限锁 `HYDRA_TENANT_API_LOCKOUT_SECS` 默认 900 s）。清单里那条最关键的告警原文是：

> "**Catch-all disables the per-IP dimension.** A `0.0.0.0/0` or `::/0` entry trusts XFF from **any** peer and therefore defeats the per-IP dimension"

代码里对应的 `warn!` 也说 "making the per-IP lockout dimension **forgeable** and effectively disabling it"。

### B. 实测（`.acceptance/round75/xff_probe.sh`，六个案例；每例一个新节点、新 DB，lockout 缩短到 15 s 以便连续跑）
| 案例 | 配置 | 结果 |
|---|---|---|
| A | 未设置 | 14 个**互不相同**的 XFF ⇒ **第 11 个起 429** ⇒ 头被忽略、按 peer 计 |
| B | `=127.0.0.1`（信任本机） | 14 个互不相同的 XFF ⇒ **全程 401，无 429** ⇒ 每个 XFF 各有自己的桶 |
| B2 | 同上 + **固定** XFF | 第 11 个起 429 ⇒ 该"伪客户端"自己的桶被打满 |
| C | `=10.0.0.1`（**不**信任本机） | 旋转 XFF ⇒ 仍然 429 ⇒ **是名单在决定，不是"有头就信"** |
| D | `=0.0.0.0/0`（catch-all） | 旋转 XFF（且**令牌也每请求轮换**，以排除令牌维度的干扰）⇒ **仍然第 11 个起 429** |
| E | `=127.0.0.1` + **固定**坏令牌、旋转 XFF | 429 ⇒ 令牌摘要确实是**独立**维度 |
| F | catch-all，**全新** XFF + **全新** 令牌（在预算已被打满之后） | **仍然 429** ⇒ 桶属于 **peer**，所有客户端**共用** |

**结论：catch-all 并不会让每 IP 维度"可伪造/失效"，恰恰相反** —— 因为 `0.0.0.0/0` 让**每一个**候选都算"可信"，`resolve_client_ip` 于是回退到 **peer**（这条行为在代码里有单测，名字就叫 `an_all_trusted_or_absent_xff_falls_back_to_the_peer`）。后果是**所有客户端共用 peer 那一个桶**：十几个坏令牌就能把该地址后面的**全部客户端**按 900 s 默认值锁在租户 API 之外 —— 正是代码里 B1 那套改动想要消除的跨租户自伤。
**真正"可伪造"的场景是另一条**（清单第一条已写对）：名单**只列了 LB**、而 LB **既不追加也不剥离**入站 XFF ⇒ 调用者可以自己轮换 XFF、每次拿到新桶（实测就是案例 B：14 次旋转、一次都没被限）。所以"让名单安全"的动作是**在 LB 上剥掉入站 XFF**，而不是"别用 catch-all 因为它可伪造"。

### C. 修法（文档 + 运维在日志里看到的那句话）
1. `ops.md` 的清单条目改写为实测口径：catch-all ⇒ **共用 peer 桶 / 锁停放大的风险**（附 F 的实测），并新增一条把"可伪造"归到**名单窄 + LB 不剥离**这个真正场景上（附 B 的实测）。
2. `crates/hydra-server/src/main.rs` 里那句 catch-all 告警（以及它上面的注释）**说反了**，已按实测重写为"所有客户端共用 peer 的失败桶 → 少数坏令牌会把整片客户端锁在外面"；已重建并实测新文案确实出现在日志里（drill 的 D 案例直接断言这句）。

### D. 变成 CI 断言：`integration/test_trusted_proxies.py`（**2.8 s**，7 条断言全过）
把 A/B/B2/C/D/F 的判定固化成测试（无需 Redis，端口 18480-18487，`HYDRA_TENANT_API_LOCKOUT_SECS` 缩短到 15 s；坏令牌**每请求轮换**以免令牌维度成为真正触发者），并且**断言 catch-all 的启动告警用的是"共用桶"而不是"forgeable"** —— 这样文档/文案再漂回旧说法就会红。已接进 CI `integration` 作业与本机门禁（仓外测试工件 10 → **11**）。
**反向证伪**：在 `resolve_client_ip` 里植入 `HYDRA_FALSIFY_IGNORE_TRUSTED`（恒返回 peer）⇒ **只有** B 那条断言失败（`codes=[401×10, 429×3]`），其余照旧通过 ⇒ 证明测试确实在验证"信任名单如何决定客户端 IP"，而不是顺带通过。探针已移除（`grep -rn FALSIFY crates/` = 0），恢复后 PASSED。

### E. 本轮验证
`cargo fmt --check` / clippy / bin 测试全绿；drill 六案例 + CI 测试 7 条断言全过；`check_ci_wiring` OK。**产品代码只剩那句告警文案的更正**（行为未变）。

### F. 顺带修掉的一处"本地跑不起来"（同一轮内发现）
重跑第 74 轮的集群测试时报 `HYDRA_ROLE=leader requires the 'cluster-redis' cargo feature` —— 因为本轮为验证代理逻辑重建了 **server-only** 的 `target/debug/hydra`，把上一轮那个带集群特性的二进制覆盖了（CI 的 `live-deps` 步骤在跑测试前会先 `cargo build … --features server,cluster-redis,usage-clickhouse`，所以 CI 不受影响，本机手跑会）。两处修：①测试现在**识别**这条启动错误并给出 `CANNOT VERIFY` + 精确的重建命令（exit 2），而不是报一个看不懂的集群失败；②本机门禁的 `cluster HA` 条目改成**先构建正确的特性集**再跑（与 CI 同一条命令）。修完实测：server-only 二进制 ⇒ 清晰的 `CANNOT VERIFY`；带集群特性 ⇒ `CLUSTER HA: PASSED`。

### G. 本轮验证（收尾）
`cargo fmt --check` / `clippy -D warnings` / bin 测试全绿；drill 六案例 + CI 测试 7 条断言全过；8 个 checker 全绿；四个端到端套件全过（trusted-proxies 2.8 s、cluster HA、handover、cert reload、error contract）；`check_ci_wiring` 识别 **11** 个仓外测试工件；`grep -rn FALSIFY crates/` = 0。**产品代码只剩那句告警文案的更正**（行为未变）。

---
## 2bh. 第七十六轮：租户 API 的**三套配额**实测 —— 文档承诺全部成立（并固化成 2.8 s 的 CI 断言）

### A. 被测承诺（`ops.md` §5 环境变量表）
- `HYDRA_TENANT_API_RATE_LIMIT_PER_MIN`（默认 **60**）：**已授权**请求的每租户上限，而且"counts **every** authenticated request the node accepts for the tenant, **including ones it then rejects**"；
- `HYDRA_TENANT_API_INVALIDATE_PER_MIN`（默认 **10**）：`auth/cache/invalidate` 的每租户上限（每次都要向全 fleet 扇出并重新打租户的 `auth_url`，所以上限更低）；
- `HYDRA_TENANT_API_AUTH_FAIL_LIMIT_PER_MIN`（10，按源 IP **与**令牌摘要独立 —— 第 75 轮已实测）；
- "0/垃圾值会回落默认"（`env_positive_u32` 的注释明确写着：0 会拒绝所有请求，即"打错一个字符就自我 DoS"）。

要打到**已授权**路径需要一个租户自助令牌：管理面 `access_token` 字段写入（只存 SHA-256 哈希、≥16 字符）。

### B. 实测（`.acceptance/round76/limits_probe.sh`，配额按需缩短，值都打印在行内）
| 案例 | 配置 | 实测 |
|---|---|---|
| A | `RATE_LIMIT_PER_MIN=3` | 6 次带**有效**令牌的 `GET /tenant/t1/api/v1/usage` ⇒ `400 400 400 429 429 429` |
| — | 同上 | **前 3 个 400 也被计入配额** ⇒ **"连被拒绝的已认证请求也计数"这条文档承诺被直接验证**（否则第 4 个不会是 429） |
| — | 同上 | 429 带 **`Retry-After: 59`**；`hydra_tenant_api_throttled_total{scope="tenant"} 4` |
| B | `INVALIDATE_PER_MIN=2` | `POST auth/cache/invalidate` ×5（有效令牌）⇒ `200 200 429 429 429` |
| C2 | 同上 | `DELETE /tenant/t1/api/v1/usage` ×4 ⇒ `404 404 404 404`（方法/路由错误由**租户 API 自己**回答，没有落到代理路径） |
| D | `RATE_LIMIT_PER_MIN=60` + 先用 12 个坏令牌打满**失败**预算 | 之后带有效令牌 `GET /tenant/t1/api/v1/whoami` ⇒ **200** ⇒ 失败锁**不会**波及持有效令牌的调用者（B1 契约） |
| E | `RATE_LIMIT_PER_MIN=0` / `INVALIDATE_PER_MIN=0` | `whoami` ⇒ **200** ⇒ 0 值确实回落默认，而不是"拒绝一切" |

**结论：这一面没有产品缺陷** —— 三套配额、`Retry-After`、指标标签、0 值回落、以及"失败锁与授权链互不干扰"全部与文档一致。

### C. 我自己的两个错误（都当场抓住，记下来）
1. **`/usage` 不带查询参数回 400**，而我第一版把"有效令牌应当 200"当成期望 ⇒ D/E 两条被误判为失败（其实是**处理过了**）。改用 `GET /tenant/t1/api/v1/whoami`（租户 API 里确有的 200 端点）后两条转绿。**顺带**：正是这些 400 让我发现 A 其实一次性验证了"被拒绝的已认证请求也占配额"这条文档承诺。
2. **端口搞混**：D/E 把租户令牌发到了**管理口**（管理口当然 401）⇒ 一度看起来像"有效令牌被锁"。改成数据口后正常。
（两条都不是产品问题；记录在案的目的是提醒：**"有效令牌应当成功"这类断言，先确认那个端点/端口本身会不会先返回 4xx**。）

### D. 固化成 CI 断言：`integration/test_tenant_api_limits.py`（**2.8 s**，7 条断言）
把 A/B/D/E 与 `Retry-After`、指标标签一起固化（无需 Redis，端口 18490-18497，配额缩短、失败锁 15 s），已接进 CI `integration` 作业与本机门禁（仓外测试工件 11 → **12**）。
**反向证伪**：在**真正被调用的**那处（`tenant_api/mod.rs` 里 `check_success(...)` 的调用点——第 70/74 轮两次教训之后，这次先找 call site）植入 `HYDRA_FALSIFY_NO_RATE_LIMIT`（恒放行）⇒ **恰好那 4 条**与授权上限相关的断言失败（`codes=[400×6]`、无 `Retry-After`、无 throttled 指标），而 B/D/E 照旧通过 ⇒ 说明断言确实钉在那条配额逻辑上。探针已移除（`grep -rn FALSIFY crates/` = 0），恢复后 PASSED。

### E. 本轮验证
`cargo fmt` / clippy / bin 测试全绿；drill 七条 + CI 测试 7 条断言全过；8 个 checker 全绿；`check_ci_wiring` 识别 **12** 个仓外测试工件。**本轮未改产品代码**（只加测试 + scratch 夹具）。

---
## 2bi. 第七十七轮：**每 provider 的准入队列**实测 —— 能力与文档一致，但发现一处**静默的"改了不生效"**（新增 D-14）

### A. 被测承诺（design-admission-queue §5/§10、`metrics.rs`、`api-docs.js`）
`provider.max_concurrency` / `max_queue_depth` / `queue_wait_timeout_ms`；`max_concurrency == 0` = 不限流（passthrough）；队列满/等待超时 ⇒ **503 `admission_denied` + `Retry-After`**；指标 `hydra_queue_drops_total{provider,reason}`（reason ∈ full/timeout/closed）、`hydra_admission_decisions_total{outcome}`、gauge `hydra_permit_inflight`/`hydra_permit_available`/`hydra_queue_depth`、histogram `hydra_queue_wait_seconds`；`GET /api/v1/concurrency` 报告各 provider 的 `{max_concurrency,inflight,available,queue_depth}`。

### B. 实测：**能力本身全部成立**（`integration/test_admission_queue.py`，慢上游 1.2 s，**15 条断言全过，~9.7 s**）
| 案例 | 配置 | 实测 |
|---|---|---|
| Q1 饱和 | cap=2 / queue=3 / wait=2s，8 并发 | `200×4, 503×4`；503 体为 `admission_denied`（+`Retry-After`）；**`hydra_queue_depth` 在 32 次采样中峰值恰为 3**（= `max_queue_depth`，即第 21/23 轮 CAS 修复想钉住的那个"界"，这次是**端到端**证明）；`hydra_queue_drops_total{reason="full"}=3`、`{reason="timeout"}=1`；`hydra_admission_decisions_total{outcome="acquired"}=2 / "dropped"=4`；采样期间 in-flight 峰值**恰为 2** |
| Q2 等待超时 | cap=1 / queue=5 / wait=**300ms** | 等待者被 shed，`reason="timeout"` 计数增加 ⇒ "超时"与"队列满"是两个可区分的原因 |
| Q3 passthrough | `max_concurrency=0` | 3 并发全部 200（不限流） |
| Q4 **已知限制** | 见 C | 钉住"改了不生效"的现状（详见 C） |

### C. **真缺陷（本轮头条）：改 provider 的准入上限，返回 200 但运行时**不生效**，而查询端点会"附和"旧值**
`proxy/admission.rs` 自己写着：*"once created, a gate's semaphore is **NOT resized** if a later call passes a different `max_concurrency` — the first policy wins. P0.4 will handle resizing on hot-reload"* —— 也就是说这是**代码里已知、但从未实现**的 P0.4 项。实测（1.2 s 上游）：
```
cap=2 → 打两次并发（gate 在第一次请求时创建，捕获 cap=2）
PUT /api/v1/providers/p1  max_concurrency=1   → HTTP 200
POST /api/v1/reload                            → 200
GET /api/v1/providers/p1                       → max_concurrency=1     ← 库里的值确实变了
GET /api/v1/concurrency                        → max_concurrency=2     ← 运行时 gate 仍是旧值
再打两次并发                                    → 采样到的 in-flight 仍到 2（新上限没有被执行）
```
**危害**：运维在过载现场**调低某 provider 的并发上限**（标准的止血动作）时，管理 API 回 200、库里的行也变了，但**执行侧毫无变化**，直到进程重启；而他会用来"确认生效"的那个端点 (`/api/v1/concurrency`) **报的正是旧值**——于是"配置"与"观测"互相印证了一个错误的结论。调**高**上限同样不生效（provider 会被卡在旧的低上限上）。**代码知道这件事（P0.4 注释），但没有任何面向运维的文档写过**：`ops.md` 没有、`api-docs.js` 还把该端点描述成 "configured cap"（正是那个陈旧值）。
**本轮修法（文档层，不改并发语义）**：①`ops.md` §4 新增一段带实测数字的说明（"上限在 provider **第一次请求**时被捕获，之后不 resize；改完请重启节点"）；②`api-docs.js` 的描述改为"运行时 gate 正在执行的上限"，并写明它可能滞后于配置、以及重启建议；③`integration/test_admission_queue.py` 的 **Q4** 把这个现状**钉住**（断言"gate 保持首次请求时的 cap，无论后来是调高还是调低"），这样将来真做 resize 时会成为一次**有意的、可见的**改动。
**新决策项 D-14**：是"实现热重载时的 semaphore resize"（`tokio::sync::Semaphore` 可增不可减，需要自定义计数 + 与 gauge/在飞请求的一致性设计，属于有并发风险的真功能），还是"维持现状 + 文档 + 重启"（当前做法）。

### D. 本轮我自己的三个夹具错误（都当场抓住）
1. **没送 `Host: load.local`** ⇒ 全部 404（`resolve_tenant` 只看 Host），所有断言都在量空气；
2. **tenant 的 `auth_url` 指向了慢 chat mock**（无 `/auth` 路径）⇒ 每个请求都死在认证跳（503 `auth_upstream_unavailable`）；
3. **`/api/v1/concurrency` 的期望写错两次**：它是**惰性创建**的运行时视图（没有流量过的 provider 根本不出现；有流量的 provider 报的是 gate 的值）。
（另外 Q4 的第一版被追加在 `finally` **之后**，于是它在节点已被杀死的状态下跑，PUT 得到 connection refused —— 结构错位，已移回 try 内。）

### E. 本轮验证
`cargo fmt --check` / clippy / bin 测试全绿（本轮**未改 Rust 逻辑**，只改了文档与 api-docs 描述）；新集成测试 15 条断言全过；8 个 checker 全绿；`check_i18n`/`check_e2e_contracts` 在改 `api-docs.js` 后复跑通过；`check_ci_wiring` 识别 **13** 个仓外测试工件（原 12）。

---

## 2bj. 第七十八轮：D-14 的**安全一半** —— 让"改了不生效"可见（端点双值 + 计数器 + 单次 WARN），并把新增的 4 个 panic 点消掉

上一轮只把"准入上限改了不生效"写进文档（**要人记得去查**）；本轮把它变成**机器可断言的状态**，同时对**是否真的实现 resize**（D-14 的另一半，涉及并发语义）**明确不动**。原则：先把"错误的观测"修成"诚实的观测"，再决定语义。

### A. 实现（`provider` 侧）

| 位置 | 改动 |
|---|---|
| `proxy/admission.rs:224` | `ProviderGate` 新增 `configured: Mutex<ConcurrencyPolicy>`（**当前配置**要的值）与 `warned_configured: Mutex<Option<ConcurrencyPolicy>>`（去重用）；原 `max_concurrency`/`queue_capacity`/`wait_timeout_ms` 字段语义不变 = **执行侧**的值 |
| `proxy/admission.rs:244` `limits_stale()` | 三个字段逐一比对（并发上限 / 队列长度 / 等待预算） |
| `proxy/admission.rs:256` `observe_configured_limits()` | 在 `acquire` 里 `get_or_create_gate` **之后**调用：记下配置值；若与执行值不同 ⇒ 每个**不同**策略只 WARN **一次**（`target: hydra::admission`，日志里**同时**给出 enforced/configured 三对值）并 `record_admission_limits_stale(provider)` |
| `proxy/admission.rs:236` `lock_gate()` | 见 D 段 —— 新增的 4 个 mutex 取锁**不 panic** |
| `admin/metrics.rs` | 新计数器 `hydra_admission_limits_stale_total{provider}`（模块头部指标表同步），记录"在过期上限下仍被放行"的**请求数**（刻意用 counter 而非 gauge：`rate()` 直接回答"还有多少流量正走在旧上限上"） |
| `admin/handlers.rs:2454` | `concurrency_collection` 的 configured 侧改为从**当前快照**解析（`hydra_core::config::resolve_policy(provider.max_concurrency, …, state.default_concurrency_policy)`），而不是"某个早先请求碰巧看到的值" |
| `admin/mod.rs` | 新常量 `DEFAULT_CONCURRENCY_POLICY` + `AdminState::with_default_concurrency_policy()` + 单测 `the_endpoint_default_policy_matches_proxyconfig`（断言该常量 == `ProxyConfig::default().default_concurrency_policy`，防止端点默认值与代理默认值漂移） |
| `ProviderConcurrencyStatus` | 追加 `configured_max_concurrency` / `configured_max_queue_depth` / `configured_queue_wait_timeout_ms` / `limits_stale`；`snapshot_with_configured(impl Fn(&str) -> Option<ConcurrencyPolicy>)` 供管理面注入"配置侧"解析 |

响应形状（实测，非设计稿）：
```json
{"providers":[{"provider_id":"p1","max_concurrency":2,"inflight":0,"available":2,"queue_depth":0,
               "configured_max_concurrency":1,"configured_max_queue_depth":1,
               "configured_queue_wait_timeout_ms":1000,"limits_stale":true}]}
```

### B. 端到端证据（`integration/test_admission_queue.py` Q4 扩展，**整链 PASSED**）
`PUT max_concurrency=1`（库里 2 → 1）+ reload，随后 3 次请求：`configured_max_concurrency=1` 与 `max_concurrency=2` **并列出现**；`limits_stale=true`；`hydra_admission_limits_stale_total{provider="p1"} 3`（= 3 次"在旧上限下被放行"的请求）；节点日志出现**一次**带双侧数值的 WARN；"gate 仍按首次请求的 cap 执行"（调高到 3 / 调低到 1 都不生效）这条**已知限制**照旧被钉住。

### C. 三次反向证伪（每次都先让探针**证明自己执行过**）
1. **Rust 单测**：`tests/admission.rs::a_configured_limit_change_is_reported_as_stale` —— 删掉 `acquire` 里的 `observe_configured_limits(...)` 调用 ⇒ 断言 `left: 4, right: 50` 失败；恢复 ⇒ 过。
2. **端点解析**（本轮最该做的那次）：探针插在 `handlers.rs:2454` **真正被调用的那个闭包**里（`if true { return None; }`，即"假装快照答不出来"的修复前行为）⇒ `configured_max_concurrency=0`、Q4 那条断言 FAIL（`ADMISSION QUEUE: FAILED (1)`）；恢复 ⇒ PASSED。（教训第 70/74 轮已记两次：**先找 call site**；这次第一版的字符串匹配就没对上代码形状，`assert count==1` 直接拦住，没有产生"假绿"。）
3. **锁的毒化恢复**：把 `observe_configured_limits` 里的 `lock_gate(&self.configured)` 换回 `.lock().expect("admission gate mutex")` ⇒ 新单测 `a_poisoned_gate_mutex_does_not_panic_admission` 在同文件 `:269:57` panic（`FAILED`）；恢复 ⇒ 过。
4. **"不误报"这一半**（自觉最容易漏的方向）：光断言"配置改了会计数"是不够的 —— 一个**在该变的时候会变**、但在**不该变的时候也变**的告警比没有告警更坏。于是 Q1 里补上反向断言（cap/queue/wait **一次写入、之后再没改过** ⇒ `hydra_admission_limits_stale_total` 必须**一个样本都不存在**），证伪方式是把计数器**提到** `if !self.limits_stale()` 之前**无条件自增**（即那个误报 bug）⇒ Q1 拿到 `['hydra_admission_limits_stale_total{provider="p1"} 8']` 并 `ADMISSION QUEUE: FAILED (1)`；恢复 ⇒ `[]` / PASSED。集成套件断言数 15 → **16**。

### D. 本轮我自己的四个错误（全部当场抓住，其中两个是**守卫抓我**）
1. `admin/mod.rs` 的 `#[cfg(test)] mod tests` 里直接写了 `DEFAULT_CONCURRENCY_POLICY` ⇒ `error[E0425]: cannot find value ... in this scope`；改成 `super::DEFAULT_CONCURRENCY_POLICY`。
2. 新计数器的注册我写了 `.unwrap()`（其余兄弟全是 `.ok()?`）⇒ **`check_source_purity` 当场红**（`metrics.rs:437`）。已改 `.ok()?`：注册失败应当是"整组指标不可用"，而不是**在生产代码里 panic**。
3. 新函数被插进了 `record_queue_drop` 的文档注释**中间**，把"queue drop"的文档挂到了新函数头上、原函数反而没有文档 ⇒ 已把两段文档各自归位。
4. 我给 `ProviderGate` 新增的 4 处 `Mutex::lock().expect(...)` 让纯净度清单里的生产 `expect` 从 **8 → 12**（D-13 的量化基线当场被我抬高）⇒ 改为 `lock_gate()`（`PoisonError::into_inner` 恢复）：这两个锁只保护**上报用**的 `ConcurrencyPolicy` 副本，没有"写一半的不变量"可破坏，而 gate 是**同一 provider 的所有任务共享**的 —— 一个无关线程的 panic 不该把该 provider 之后的**每次**准入都变成 panic。清单回到 **8**（= 第七十七轮基线；`check_source_purity` 现打印 8 处，D-13 仍是待决策项）。

### E. 本轮验证
`cargo fmt` / `cargo clippy -p hydra-server --features server --all-targets -- -D warnings` clean；`--lib` **190 passed / 0 failed**（+1 = 毒化恢复单测）；`--test admission` **12 passed**；`integration/test_admission_queue.py` **PASSED**（**16 条断言**：Q1 新增"配置一致时计数器必须缺席"的**不误报**断言，Q4 含 5 条可见性断言）；`check_source_purity` exit 0（`expect` 8 处，回到基线）；`check_documented_env` / `check_i18n` / `check_e2e_contracts` / `admin_ui_render` / `check_ci_wiring` 全绿。文档同步：`ops.md` §4（双值 + 计数器 + WARN + 实测数字）、`ops.md` §9.1 新增告警行（"Admission limits accepted but not enforced"）、`admin-ui/api-docs.js`（`/api/v1/concurrency` 的描述与示例响应）、`design-admission-queue.md` §10 指标表新增该计数器行。**D-14 仍未决**（本轮刻意不做 resize 语义）。

---

## 2bk. 第七十八轮的第二个发现（**最重的一个，而且是我的测试基建在破坏用户环境**）：`pkill -x hydra` 一直在杀**用户的本地 dev 栈**

### A. 现场
收尾跑门禁时发现本机 dev 栈异常：`ss` 显示 8080/8081/8082/8084/8090 仍在 LISTEN，但 `curl`/裸 socket 连上去**立刻被 RST**（`ConnectionResetError`）。查 Docker：
```
hydra-a  hydra-local:latest  Up 3 minutes (unhealthy)   RestartCount=14  exit code 0
hydra-b  hydra-local:latest  Up 3 minutes (unhealthy)   RestartCount=14  exit code 0
hydra-c  hydra-local:latest  Up 3 minutes (unhealthy)   RestartCount=14  exit code 0
```
健康检查日志全是 `curl: (7) Failed to connect to 127.0.0.1 port 8081`，`hydra-a` 的应用日志在**每 16 秒**于 hydra-a/hydra-b 之间轮换控制轮询目标（`control poll failed; keeping last-known-good snapshot failures=N`）——**三个节点谁都不在服务**。退出码 **0** 是关键：不是崩溃，是**干净的 SIGTERM 退出**，然后被 `restart: unless-stopped` 拉起。

### B. 根因（我的四个集成测试）
```
integration/test_cert_reload.py:168      subprocess.run(["pkill", "-x", "hydra"], ...)
integration/test_cluster_ha.py:196       subprocess.run(["pkill", "-x", "hydra"], ...)
integration/test_trusted_proxies.py:143  subprocess.run(["pkill", "-x", "hydra"], ...)
integration/test_tenant_api_limits.py:110 subprocess.run(["pkill", "-x", "hydra"], ...)
```
它们启动前"清理残留实例"用的是**按进程名**杀。而 `environment/docker-compose.local.yml` 起的容器里，主进程名**就叫 `hydra`**（`/usr/local/bin/hydra`，**同一个 uid**，所以信号真的能送达 —— 本轮我自己的 `pkill -x hydra` 同样打中了它们）。每次门禁跑 4 个套件，本轮又跑了 3 次门禁：4×3 + 我手上 2 次 = **恰好的 14 次重启**，与 `RestartCount=14` 对齐。也就是说：**过去几轮里我每跑一次门禁，就把用户的 dev 栈打断一次**（套件本身用的是 18xxx 高端口，所以测试结果没被污染，掩盖了这件事）。这正是"第 70/74 轮同族"的第三次变体：**清理动作必须证明自己打的是谁**。

### C. 修法（改"匹配可执行文件"，而不是"匹配名字"）
四个套件各加一个 `kill_our_instances()`：遍历 `/proc/<pid>/exe`，**只**杀 `realpath == BIN`（= 本仓库 `target/debug/hydra`）的进程；返回被杀 pid 列表并打印（清理动作**自证执行过**）。`scripts/handover.test.sh` 的 `pkill -f "$BIN"` 本来就是**路径**匹配，不动。

### D. 新守卫（防止回潮）
`scripts/check_e2e_contracts.cjs` 新增 `nameKillLines()`：扫 `integration/`、`scripts/`、`.github/workflows/`，只认三种**可执行**形态 —— ①argv 列表 `["pkill","-x","hydra"]`；②shell 字符串 `"pkill -x hydra"`；③裸命令（含 `sudo`、含 `; `/`|` 之后的、含数字 flag 如 `-9`）。`pkill -f "$BIN"`（路径）与**文档字符串里解释旧命令的那句话**都不算违规（前者只认三个形态，后者靠注释前缀过滤）。9 条断言进 `check_e2e_contracts.test.cjs`。

### E. 证伪与验证（两个方向都测）
1. **守卫证伪**：把 `subprocess.run(["pkill", "-x", "hydra"], ...)` 塞回 `test_trusted_proxies.py` ⇒ 检查器 exit 1 并点名该行；再换成 `subprocess.run("pkill -x hydra", shell=True)` ⇒ 同样 exit 1；恢复 ⇒ exit 0。**（第一版守卫是假绿的！** 我的正则要求命令行以名字结尾（`\s*$`），而真实代码后面还有 `, capture_output=True)` ⇒ 放进真 bug 却仍然 `OK`。第二版改形态匹配后，**又**被自己的测试抓到两处：数字 flag `-9` 不匹配 `-[a-zA-Z]+`；以及守卫把自己的**测试夹具**（测试文件里必须写那条坏命令的字面量）当成了违规 ⇒ 现在跳过 `*.test.cjs`。）
2. **修复有效（对着用户的环境实测）**：跑 `integration/test_trusted_proxies.py` ⇒ **PASSED**，而三个容器 `RestartCount` **15 → 15 未变**、全部 `healthy`；依次再跑 `test_tenant_api_limits.py` / `test_cert_reload.py` / `test_cluster_ha.py` ⇒ 全部 PASSED，容器仍是 `15 healthy` ×3。
3. **修复没有变成空操作**：用套件自己的 env 起一个**属于我们的**残留实例（`/proc/<pid>/exe` = `target/debug/hydra`，pid 633607），调 `kill_our_instances()` ⇒ 返回 `[633607]`、进程 exit `-9`（真的被杀了），同时 dev 栈 `RestartCount` 依旧 `15 → 15`、状态 `running`。
4. **恢复确认**：停止误杀后三个容器自行回到 `healthy`（此前是 `unhealthy`）—— 这也反向证明"unhealthy 是我造成的"。
5. **门禁尺度复证**：修好后把整套门禁（37 项）完整跑完，结束后三个容器仍是 `RestartCount=15`、`healthy` —— 修复前每跑一次门禁约 **+4** 次重启（4 个套件各一次）。该次门禁 **36 绿 / 1 红**，唯一的红是**待决策 D-2** 的两例（`admin_api.rs`：端点回 202、测试期望 200）。

### F. 教训（写进 INDEX）
*"按名字杀进程"在**共享主机**上等于对**别人的部署**动手*；判据必须落在**可执行文件路径**上。与本会话既有的两条同族：探针要证明自己**执行过**（第 70 轮）、要打在**真正被调用的函数**上（第 74 轮）—— 这条是"**清理**也要证明自己打的是谁"。另外：**我自己的新守卫第一版是假绿的，第二次被自己的测试文件误伤** —— 守卫必须先被证伪，才有资格守别人。

---

## 2bl. 第七十九轮：把 `ops.md` §1.4 的**备份/恢复手册真跑一遍**并钉成 CI 断言 —— 手册里那句"顺手删掉 `-wal`"**是承重的**

§1.4 是本会话早前补写的（四个论断都只是**手工测过一次**、没有任何东西守着）：A 运行中 `cp` 数据库不可用；B `VACUUM INTO` 是活库快照、第二个节点能靠它服务；C 恢复要"停节点、放快照、**删掉旁边的 `-wal`/`-shm`**、用同一把主密钥"；D 换主密钥 ⇒ 拒绝服务。本轮新增 `integration/test_backup_restore.py`（**12 条断言**，已接进 CI `integration` 作业与本机门禁；`check_ci_wiring` 仓外测试工件 **13 → 14**），把这四条**可执行化**，并顺手测出两件手册没写对的事：

| 案例 | 做法 | 实测 |
|---|---|---|
| **A** 运行中裸 `cp` | 301 个 provider + 4.1 MB WAL，然后 `cp hydra.db` | **PASS（危险成立，而且形状更坏）**：副本 **221 184 字节 —— 看起来完全正常**，却只有 **215/301** 个 provider。§1.4 原文记的是"4096 字节、每张表都读不出来"；新的形状**更阴**：从一个正常大小的副本恢复会**开机成功、静默丢掉最新配置** |
| **B1** `VACUUM INTO` | 活库上执行（本机没装 `sqlite3` CLI ⇒ 用 Python 内置 SQLite 发同一条语句；`VACUUM INTO` 是 SQLite ≥ 3.27 的语句，两者等价） | 快照 237 568 字节、**逐表与活库行数完全一致**；执行前后 `/api/v1/health` 都是 200 ⇒ **无需停机** |
| **B2** 从快照起第二个节点 | 快照当 `restored.db` 起节点 | **PASS**：健康 + 代理请求 **HTTP 200** ⇒ 路由配置**与封印的 provider key** 都活着（key 解不开会是 503） |
| **C1** 快照 + 旧 `-wal`/`-shm` | 把快照放好，但**把旧库的 `-wal`/`-shm` 留在旁边** | **读都读不出来**（`DatabaseError`），且**节点直接拒绝启动**：`ERROR hydra: fatal startup error error=error returned from database: (code: 11) database disk image is malformed` |
| **C2** 照手册删掉那两个文件 | 同一个快照，删掉 `*.db-wal` / `*.db-shm` | **PASS**：逐表=活库，节点健康、代理 **200** ⇒ **手册那句"顺手删掉"是承重的，不是整洁步骤** |
| **D** 换 `HYDRA_ENCRYPTION_KEY` | 同一个快照、另一把 32 字节密钥 | **PASS**：exit=1，日志 `fatal startup error error=database error: error occurred while decoding: decryption failed (wrong master key o…` |
| **E** 什么时候会真的遇到 `-wal`？ | `SIGTERM`（drain 缩到 1 s）后看目录 | **与直觉相反**：**每一次停止都留下 `hydra.db-wal` + `hydra.db-shm`** —— 因为进程走 Pingora 的 `process::exit(0)`（**§13.5b 已记**"不跑析构"），SQLite 池从不关闭 ⇒ C1 的陷阱是**常规情形**而非边缘情形；同时 E 也证明**光留着 `-wal` 无害**（同一库重启会重放，停止前写入的行仍可读 ⇒ 200），**只有"底下的库文件被替换"时才致命** |

### A. 本轮我自己被抓到的两个错（同属"断言假绿"这一族）
1. **B1 复用了案例 A 的 `missing`**：快照与活库**逐表完全相等**，却因为 `not missing` 里那个来自案例 A 的 `['provider']` 而报 FAIL —— 测量是对的、**断言引用了别的案例的结果**。
2. **C1 的"节点拒绝启动"是空洞断言**：第一版写成 `stale_died or not wait_healthy(ADMIN2)`，但它在 `stop(node2)` **之后**才求值 ⇒ 节点无论健不健康都通过。是"**故意不留 `-wal`**"那次证伪把它暴露出来的（探针下该条本应变红，却仍然 PASS）。改成先取 `stale_died`/`stale_healthy` 再停节点，重跑证伪 ⇒ 该条如期变红。

### B. 三次反向证伪（每条承重断言的机制都被证实）
1. **A 的机制**：在裸 `cp` 之前先 `PRAGMA wal_checkpoint(TRUNCATE)` ⇒ `missing={}` ⇒ A 断言变红（并顺带把 C1 也带红：WAL 被清空后"陈旧 WAL"不再有害）⇒ 证明 A 测的确实是 **WAL 未 checkpoint** 这件事。
2. **C1 的机制**：不复制那份旧 `-wal` ⇒ C1 三条全红（文件可读、节点健康、无 `malformed`）⇒ 证明 C1 测的是那份陈旧 WAL。
3. **E2 的机制**：重启前把新写的 `-wal` 删掉 ⇒ 停止前写入的 `extra1` 变成 **404**（provider 计数 302 → 215）⇒ 证明 E2 读的是 **WAL 重放**这条路径。

### C. 文档同步（按实测改口径，不按记忆）
`ops.md` §1.4：引言改成"备份两条 + 恢复一条、全部由新测试守着"；裸 `cp` 的危险补上**"正常大小但少 86 行"**这个更坏的形状；`VACUUM INTO` 补 301 行复测数字；新增一块 ⚠️ 说明**"删掉 `-wal`/`-shm` 是承重步骤"**（含节点拒绝启动的**原文错误串**）以及"**每次停止都会留下 `-wal`**、单留无害、替换库文件才致命"。`ops.md` §13.5b 补一句交叉引用：同一条 `process::exit(0)` 既是不跑析构的原因，也是 `-wal` 残留的原因。

### D. 本轮验证
`python3 integration/test_backup_restore.py` ⇒ **PASSED（12 条断言，含 7 条 `....` 原始测量行）**；三次证伪全部如期变红、探针已移除（`grep -c FALSIFY` = 0）；`check_ci_wiring` exit 0（工件 13 → **14**，新测试已在 CI 中）；`check_documented_env` / `check_i18n` / `check_e2e_contracts` / `check_public_claims`（802 = 253 + 549 仍成立，本轮无 Rust 改动）/ `check_source_purity` / `check_compose_*` 全绿。**本轮未改 Rust 代码**（只加测试 + 改文档 + 接线）。

---

## 2bm. 第八十轮：把租户 Python SDK **对着真节点**跑起来（三种 fleet 状态全部实测），以及**我一次把 956 行源码弄丢又靠会话记录救回来**

`tools/hydra-py` 的 README 写着一份很硬的契约（`POST {数据面节点}/tenant/{tid}/api/v1/auth/cache/invalidate`；`200 single_node`/`200 applied` = done、`202 pending` = **不是**成功、`503 unavailable` = 集群没被通知；`X-Hydra-Trace-Id`/`event_id`/`lagging`/`waited_ms` 全都保留给调用方）。它自带 **787 行单测**，但那些跑的全是 **mock**；**从来没有任何东西把 SDK 指向一个真节点**，所以"文档里的用法真能用"和"真节点真会发这些状态"都没被验证过。本轮新增 `integration/test_sdk_live.py`（**12 条单节点断言 + 11 条集群断言**，两种模式；已接进 CI `integration` 与 `live-deps` 两个作业、以及本机门禁；`check_ci_wiring` 仓外测试工件 **14 → 15**）。

### A. 单节点腿（server 特性，无需 Redis）—— 全部实测通过
| 断言 | 实测 |
|---|---|
| 文档里的用法可用 | `HydraClient(token=租户令牌, nodes=[数据面])` + `invalidate_with_result("t1")` ⇒ **HTTP 200 `single_node`**、`res.done is True`、`nodes_total=1`、`trace_id=hydra-…-12` |
| **SDK 调用真的清了缓存**（三态前置条件） | ①mocked auth 允许 ⇒ 请求 200 ②auth 翻成拒绝 ⇒ **仍然 200（命中缓存）** ③**SDK 失效** ⇒ 下一次请求 **401 `denied`** ⇒ 光断言"返回 200"是不够的，这一步才证明它有用；前置条件被显式断言（若②不是 200，说明缓存根本没生效，这条就证不了任何事） |
| 另一个租户的令牌 | **403**，SDK 抛 `HTTPError`（不会伪装成 done） |
| 旧管理面路径 | `/api/v1/tenants/t1/auth/cache/invalidate` 与 `/api/v1/tenant/t1/…` 都是 **404 `not_found`** ⇒ README 那句"2026-09-17 已移出管理面"是真的 |
| **不是管理端口** | 把 SDK 指向 **admin 端口** ⇒ `HTTPError 401` ⇒ README 那句"**It is not the admin port**"是真的 |

### B. 集群腿（cluster 特性 + 真 Redis）—— 三种状态全部由**真节点**产生
| 状态 | 实测 |
|---|---|
| `applied` | 一个持租约的 leader + 一个 edge（都注册进 registry）⇒ **HTTP 200 `applied`，nodes 2/2，`lagging=[]`，`event_id=1790700555077-0`，`waited_ms≈470`** |
| `pending` | `wait=none` ⇒ **HTTP 202 `pending`**、`lagging=['sdk-live-a','sdk-live-edge']`、`nodes=0/2`；`invalidate_with_result` **返回**该结果（`done=False`），`invalidate_tenant_auth_cache` **抛 `InvalidatePendingError`** |
| `unavailable` | edge 的 Redis 走一个我控制的 **TCP 中继**，节点起来后**剪断中继** ⇒ 下一次失效 **HTTP 503 `unavailable`**；两个 API 各自返回/抛出正确结果（`InvalidateUnavailableError`） |
| 顺带证实 | **edge 也本地应答租户 API**（请求走到了 publish 才 503），而它的 `/api/v1/health` 是 **404**、`/healthz` 是 200 ⇒ "每个数据面节点都本地应答"这条对 edge 成立 |

**为什么"unavailable"要用中继**：先试过直接把 `HYDRA_REDIS_URL` 指到死端口 ⇒ 节点**根本起不来**（`redis pool failed to initialise: IO Error: Connection refused`）；而这个状态的定义是"通道存在、但办不成事"，所以必须**在运行中的节点下面把总线剪断**。

### C. 本轮我自己的三个错（含一次**真正的破坏**）
1. **`HYDRA_ROLE=standby` 不是合法角色**：节点打印 `unknown HYDRA_ROLE; falling back to single-node 'all' mode`，于是我那次测的是"单机模式"，租户都不在本地库里 ⇒ 401。合法值只有 `leader` / `edge`（HA 测试里的"standby"节点其实也是 `leader` 角色，靠租约决定主备）。
2. **把"返回值 API"和"抛异常 API"搞混了两次**：README 的第一个示例用 `invalidate_with_result` **返回**三态结果（自己 `if res.done`），而抛异常的是 `invalidate_tenant_auth_cache`。我在 C2/C3 里都先写了"期待返回的 API 抛异常"⇒ 真节点返回 `202 pending` / `503 unavailable`，**SDK 是对的、测试是错的**（又一次"新守卫第一次红，先怀疑守卫自己"）。
3. **★ 我弄丢了 956 行的 `tools/hydra-py/hydra_sdk/client.py`，然后把它救了回来。** 为了回滚一个证伪探针，我用了 `git checkout tools/hydra-py/hydra_sdk/client.py` —— 而该文件是**本会话未提交的工作**（956 行），git 里只有 HEAD 的 298 行旧版 ⇒ **一行命令销毁了 ~660 行成果**，而 `git diff` 之后显示"干净"，看起来像从来没改过。更糟的是我事后才意识到：`grep`/`wc` 当场都还在用 956 行这个数字，`__init__.py` 导入的 `MAX_TIMEOUT_MS` 也只在被删的那版里存在。
   **救回路径（可复用）**：DSH 会把**每个会话（含子代理会话）**的完整对话记录写在 `~/.dsh/sessions/<workspace>/<session-id>/session.jsonl.zstd`（zstd 压缩，`grep` 直接搜不到）。写这个 SDK 的是**子代理会话**，它的 `write`（37 860 字符）+ 15 次 `edit` 都在里面。写了个脚本按**时间戳顺序回放**这些 `write`/`edit`：**15 次 edit 全部精确命中、0 次跳过，产出 956 行** —— 与丢失前的行数完全一致；恢复后 **SDK 自带 29 个单测全过**，本轮新写的 23 条真节点断言（单节点 + 集群）也全过 ⇒ 恢复的是同一份文件。恢复副本另存 `.acceptance/recovered-client.py.keep`。
   **教训（比这一轮的测试更重要）**：①**回滚一个探针永远不要用 `git checkout <path>`**，要用 `cp` 备份 + `cp` 还原（本会话此前一直这么做，这次偷懒了）；②`git checkout` 之后 `git diff` 会"看起来干净"，**这是它最危险的地方**，所以破坏性命令执行后要立刻用**行数/符号存在性**核对，而不是看 diff；③**会话记录（含子代理）是可用的取证与恢复媒介** —— 未提交的工作并非"没有备份"，但要主动去取。

### D. 四次反向证伪
1. **不调用 SDK**（S2 的前置条件仍在）⇒ 失效后的请求仍是 **200 `{"allowed": false}`** ⇒ "清了缓存"这条断言确实依赖那次 SDK 调用。
2. **把 `is_done_state` 改成恒 True**（即"把 202 当成功"这个原始 bug）⇒ C2/C3 的 4 条断言（`done is False`、两个抛异常断言）全部变红 ⇒ 这些断言测的确实是 SDK 的三态语义。**这次用 `cp` 备份+还原，文件恢复后 `cmp` 与备份逐字节一致。**
3. `HYDRA_ROLE=standby` ⇒ 节点日志明确打印回退到单机模式（见 C1），反向证明 B 段的 `2/2 nodes` 来自真正的集群接线。
4. `wait=none` 与 `wait=converged` 的对照（C1 2/2 applied vs C2 0/2 pending）互为对照：同一条总线、同一个租户，**只有等待语义不同** ⇒ 202 不是"总线坏了"。

### E. 文档与接线
`tools/hydra-py/README.md` 新增"How this contract is verified against a REAL node"一节（列明上面每一条实测，包括"死 Redis URL 起不来、所以用中继剪断"这个实现约束，以及 edge 也本地应答这一点）。CI：单节点腿进 `integration` 作业、集群腿进 `live-deps`（那里有真 Redis 且本来就会构建 cluster 特性二进制）；本机门禁新增两条 entry（`tenant SDK vs live node`、`tenant SDK vs live cluster`）。

### F. 本轮验证
`python3 integration/test_sdk_live.py` ⇒ **PASSED**；`--cluster` ⇒ **PASSED**；SDK 自带 29 个单测 ⇒ **OK**；`check_ci_wiring` exit 0（工件 14 → 15）；四次证伪全部如期变红、探针已移除（`grep -c FALSIFY` 在 `crates/`、`integration/`、`tools/` 全为 0）；`check_i18n` / `check_public_claims`（802 = 253 + 549，本轮无 Rust 改动）/ `check_source_purity` / `check_documented_env` / `check_e2e_contracts` 全绿。**本轮未改 Rust 代码。**

---

## 2bn. 第八十一轮：**D-2 结案** —— 唯一让门禁红着的两例，是**测试写死了旧契约**（证据在文档里，不在偏好里）

### A. 先查"谁对"，再谈"怎么办"
D-2 一直被写成"注入单节点 fleet 视图断言 200 / 承认 202 并改断言+文档"的二选一，而计划里建议前者。本轮先把**两条前提**查实：
1. **产品文档早已承诺三态**：`admin-ui/api-docs.js:43-47` 明写 `DELETE /api/v1/auth/cache` 报告 `fleet.state` = `applied`（200）/ `pending`（**202**，并把 lagging 节点点名）/ `single_node`（200），且**刻意**说"没有 `published` 布尔了 —— 它在没有流的时候不可能为 false，等于宣称了一次从未发生的广播"；`ops.md` §5.1 同样三态并强调"只有 200 才叫 done"。
2. **"空 live set ⇒ pending"是已被单测钉住的语义**：`tenant_api_cluster.rs::barrier_reports_pending_not_applied_when_the_live_set_is_empty`。
3. 而这两例的 `AdminState` **从未注入 fleet 视图**（人工构造，`live_nodes` 保持 `None` ⇒ handler 解析出 `live = []`）⇒ 它们必然拿到 `202 pending`，**这是正确答复**；写死 `assert_eq!(r.status(), 200)` 是**对着旧契约的陈断言**（该断言写在 `published: bool` 那个已被删除的字段时代）。

⇒ 结论：这不是"要不要改行为"的取舍，而是**测试落后于已发布且已被文档化的契约**。按批次 1 的既定原则（文档 ↔ 事实对齐）修测试，**不动 handler 一个字节**。

### B. 改动（`crates/hydra-server/tests/admin_api.rs`）
`empty_body_delete_invalidates_all_local`：把 `assert_eq!(r.status(), 200)` 换成**对三态的精确断言** —— 该构造下必须是 `202` + `fleet.state == "pending"` + `nodes_total == 0` + `lagging == []` + `event_id` 是字符串，并原样保留真正承重的两条（`invalidated == 3`、`auth.cache().len() == 0`：**无论 fleet 如何，本地缓存必须清掉**）。`too_many_invalidation_keys_are_refused_and_publish_nothing`：末条改为"**在 cap 上必须被接受**"（`200 | 202`，且 `!= 400`），`XLEN == 1` 不变。两处都加了 `println!("[D-2] …")` 把真实状态打进测试输出（这是本轮证据的来源）。

实测输出（`--nocapture`）：
```
[D-2] DELETE /api/v1/auth/cache -> 202 Accepted fleet={"event_id":"1790701272807-0","lagging":[],"nodes_applied":0,"nodes_total":0,"state":"pending","waited_ms":0}
[D-2] DELETE /api/v1/auth/cache (1000 keys) -> 202 Accepted fleet={"event_id":"1790701272713-0","lagging":[],"nodes_applied":0,"nodes_total":0,"state":"pending","waited_ms":5}
```

### C. 反向证伪（两条新断言的机制都被证实）
1. **把三态打平成 200**（`let report_status = 200u16;`，即 D-2 想"恢复"的旧契约）⇒ `empty_body_delete_invalidates_all_local` 变红，报文里直接给出 `202 pending` 的实际 fleet 对象；恢复后 42 tests ok。
2. **跳过本地清理**（`total += 0`）⇒ `invalidated == 3` 断言变红（`"invalidated":0`）⇒ 这条确实是承重断言，不是陪衬。
探针均以 `cp` 备份 + `cp` 还原（本轮**不再使用** `git checkout`），`cmp` 逐字节一致，`grep -c FALSIFY` 在 `crates/` = 0。

### D. 结果
可选特性套件（`server,cluster-redis,usage-clickhouse`，活 Redis，`--no-fail-fast`）**首次全绿：exit 0、0 failed**（此前每轮都是 `2 failed`）。`admin_api` 单测 42 passed。整套本机门禁随之 **42 项全绿、`OVERALL=GREEN`、exit 0**（`test result: FAILED` 出现 **0** 次；此前每轮都是 41 绿 / 1 红）。同时修掉 `.github/workflows/ci.yml` 里那句**已过期**的注释（它当时说"套件因 D-2 而红，所以 `--ignored` 步骤必须排在全量测试之前"）—— 顺序**保留**（守卫不应依赖套件通过才运行），但理由改成实话，并注明 D-2 于 2026-09-30 结案。计划头部状态行 `待决策 14 项` → **13 项**，决策表 D-2 行标记已解决并写明证据。

**未动**：handler、`events.rs` 的三态实现、任何对外行为。**D-2 是"文档对、测试旧"，不是"测试对、代码错"** —— 这一点在结案记录里写清楚，以免将来有人照着旧行号把断言改回 200。

---

## 2bo. 第八十二轮：把**运维 CLI** 对着真节点跑一遍 —— README 里有**两条命令根本不工作**（一条静默给出错误答案）

`hydra-admin`（`tools/hydra-cli`）是运维天天用的入口，README 声称它 "drives every admin endpoint" 并给了完整命令表。它自带的测试是 **typechecked 单元套件 + 假 fetch**（CI `sdks` 作业跑 `npm test`）—— **从来没有任何东西把 CLI 指向真网关**。本轮新增 `integration/test_cli_live.py`（**62 条断言**，接进 CI `integration` 作业与本机门禁；仓外测试工件 15 → 18，因为另加了 2 个 CLI 单测文件），逐个跑 README 里的命令。**一跑就抓到两个真缺陷**：

### A. 缺陷 1：`--max-concurrency null`（README 明确写的"清空"）根本不工作
`providers update openai --max-concurrency null   # clear (set to null)` 实测：
```
HTTP 400 invalid_json: failed to parse request body: invalid type: string "", expected u32
```
用请求回显服务器抓到真实 body：`"max_concurrency":""` —— **空字符串**，不是 `null`。
根因（两行探针定案）：`parseNumber` 对 `null` 返回 JS `null`，而 **commander 12.1.0 把解析器返回的 `null` 变成 `''`**（不是报错、不是保留），于是"清空"变成了"写一个非法值"。对照：`--max-concurrency 5` 正常。
**修法**：把"清空"意图用**哨兵字符串**（`CLEAR_VALUE = 'null'`）带过 commander，在拼 body 时转成真正的 JSON `null`（`fieldValue()`），并导出 `buildBody` 供测试。影响面：providers 三个字段 + limit-roles 的 `--limit-count`/`--limit-token`。

### B. 缺陷 2：`tenants auth-test <url>` **完全没被执行**，而是静默打印租户列表并 exit 0
README：`hydra-admin tenants auth-test <auth-url> [--tenant-id <id>]`。实测：`--tenant-id` 报 `unknown option`，不带该参数则**打印出一列租户**、exit 0 —— 一个"看起来成功"的错误答案（运维以为探测了 auth URL）。root cause 用 commander 探针钉死：
```
registered subcommands: [ 'list', 'auth-test <url>' ]
```
`new Command('auth-test <auth-url>')` 注册的子命令**名字就是整串 `auth-test <url>`**（构造器不解析参数说明，只有 `.command('get <id>')` 那种写法才解析），于是 `auth-test` 永远匹配不上，**组的默认子命令 `list`（`isDefault: true`）吞掉了这些 token**。
**修法**：`new Command('auth-test').argument('<auth-url>', …)`。

### C. 两个缺陷都补了**单测 + 端到端断言**，并各自反向证伪
- 新 `tools/hydra-cli/test/nullable.test.ts`（6 条）：哨兵→JSON `null`、普通数字不受影响、未传的字段不凭空变 `null`、**哨兵字符串绝不允许漏进 body**。
- 新 `tools/hydra-cli/test/dispatch.test.ts`（3 条，含一个**真起本地 HTTP server** 的用例）：子命令名必须是 `auth-test` 且声明 1 个参数；`parse(['auth-test', url, --tenant-id …])` 必须打到 `/api/v1/tenants/auth/test` 而**不是**默认的 `/api/v1/tenants`。
- **反向证伪 A**：把 `parseNumber` 的哨兵改回返回 `null`（修复前行为）⇒ 端到端 drill 的 `--max-concurrency null` 断言变红，报的正是那句 `400 … invalid type: string ""`。
- **反向证伪 B**：把 `new Command('auth-test').argument(...)` 改回 `new Command('auth-test <auth-url>')` ⇒ drill 的两条 auth-test 断言同时变红（`rc=1`，不再打印租户列表）。单测侧也红（该文件整体失败）。恢复后 29 个 CLI 单测全过。

### D. 端到端 drill 覆盖（`integration/test_cli_live.py`，62 条断言全过）
①**文档化配置**：`HYDRA_BASE_URL`/`HYDRA_ADMIN_TOKEN` 生效；全局选项放在子命令**前/后**都可用（README 明说两种都行）；默认输出是表格、`--json` 是可解析 JSON。②**服务类命令**：`health`/`reload`/`metrics`（裸 Prometheus 文本）/`concurrency`/`breaker`/`stats usage`/`cluster status`/`auth-cache invalidate`/`tenants auth-test` 全部 exit 0。③**8 个实体组的 CRUD**：create→list→get→update→delete 每一步都**用 REST 独立核对**（只断言 CLI 退出码不算证据），两个 mapping-only 组按 README 拒绝 `update`。④**"delete 非空洞"**：先断言删除前 REST 是 200 再删（管理面 DELETE 是幂等的 204，否则"删完 404"对一行**从未存在**的数据同样成立 —— 这个问题在我第一版里真的发生了：tenants 建失败、delete 却 PASS）。⑤**失败路径**：错 token ⇒ 401、未知 id ⇒ 404、未知命令 ⇒ 非 0。

### E. 本轮我自己的三个错误（都是夹具，全部当场抓住）
1. **tenants 建失败（409 域冲突）**：drill 的种子租户已占用 `cli.local`，循环里又用同一个域 ⇒ 修正为 `cli-e1.local`。
2. **`reload` 的断言写错**：CLI 打印 `✓ config reloaded`，我却断言含 "snapshot"（那是 README 对该动作的描述，不是输出）⇒ 改成断言 `reloaded`。
3. **`parseAsync([...])` 少了 `{ from: 'user' }`**：commander 会**切掉前两个 argv 元素**（它假设那是 `node script`），于是 `auth-test` 被丢掉、`--tenant-id` 落到了默认的 `list` 上 ⇒ 单测报 `unknown option '--tenant-id'`。**我差点把它当成 CLI 的第三个缺陷** —— 靠"用 commander 探针打印 `_dispatchSubcommand` 的 name/ops/unknown"才看清 `DISPATCH name= list ops= [] unknown=[…]` 里 `auth-test` 根本不在，是**我的测试**写错了。教训：**"守卫报红"先怀疑守卫**（本会话第四次）。

### F. 文档与接线
`tools/hydra-cli/README.md` 新增两节：*"Two documented behaviours that did NOT work (fixed 2026-09-30)"*（写明两条的症状、根因与修法）与 *"How this CLI is verified against a REAL node"*（列出 drill 覆盖的每一层）。CI：`integration` 作业新增 `actions/setup-node@v4` + `npm ci && npm run build` + 跑 drill；本机门禁新增 entry `admin CLI vs live node`（先 `npm run build`，确保测的是**当前源码**而不是旧 `dist`）。`check_ci_wiring`：仓外测试工件 **15 → 18**（2 个新 CLI 单测 + 1 个新 drill），exit 0。

### G. 本轮验证
CLI 单测 **29 passed / 0 failed**（原 21）：`npm test`；drill **62 PASS / 0 FAIL**；两次反向证伪如期变红并已恢复（`grep -rn FALSIFY tools/hydra-cli/src` = 0）；`check_ci_wiring`/`check_i18n`/`check_e2e_contracts`/`check_source_purity` 全绿。**产品代码只改了 CLI 的两处缺陷**（`src/commands/entities.ts`、`src/commands/system.ts`）+ 两处新测试；Rust 侧未动。

---

## 2bp. 第八十三轮：把**第二份实现**（TypeScript SDK）也对着真节点跑 —— 并加一条**跨 SDK 一致性**断言（两份实现、一份契约）

`tools/hydra-ts` 与 `tools/hydra-py` 文档化的是**同一份租户契约**（`200 applied`/`single_node` = done、`202 pending` **不是**成功、`503 unavailable` = 集群没被通知、trace id 保留给调用方、端点在**数据面**而非管理口）。第八十轮只把 Python 那份指向了真网关；TS 那份**从未离开过 mock 套件** —— 而"两份实现、一份契约"正是**静默漂移**最容易藏身的地方，任何单 SDK 测试都看不见。本轮新增 `integration/test_sdk_ts_live.py`（**单节点 10 条 + 集群 12 条断言**，接进 CI `integration`/`live-deps` 两个作业与本机门禁两条 entry；`check_ci_wiring` 仓外测试工件 18 → 19）。

### A. 做法
TS SDK 通过一个**生成的 Node driver**（`.acceptance/sdk-ts-live/driver.mjs`）驱动：它 `import` 构建产物 `dist/client.js`（按 `file://` 绝对路径），每个 leg 往 stdout 打一个 JSON 对象，Python 侧只做断言。这样测的是**构建产物**（`tsc -p tsconfig.build.json`），而不是源码或 mock。

### B. 实测（全部通过）
| 腿 | 实测 |
|---|---|
| T1 文档用法 | `HydraClient({token, nodes:[数据面]})` + `invalidateWithResult` ⇒ **HTTP 200 `single_node`**、`isDoneState` 为真、`traceId=hydra-…-12`、`nodesApplied/nodesTotal=1/1` |
| T2 **真的清了缓存** | 允许⇒200、翻成拒绝⇒**仍 200（命中缓存）**、TS SDK 失效⇒下一次 **401 `denied`**（前置条件被显式断言） |
| T3 另一租户的令牌 | `HTTPError`（`403`），**不**伪装成 done |
| T4 旧管理面路径 ×2 | 都是 **404** |
| C1 活总线 `wait=converged` | **200 `applied`、2/2（edge 也注册进来了）、`eventId`、`waitedMs≈373`** |
| C2 `waitMode='none'` | **202 `pending`、`lagging=[两个节点]`、0/2**；返回式 API 的 `isDoneState` 为假，抛异常式 API 抛 `InvalidatePendingError` |
| C3 **剪断总线**（edge 的 Redis 走 TCP 中继，起来后剪断） | **503 `unavailable`**（且断言**不是** `pending`）；抛 `InvalidateUnavailableError` |
| **P 跨 SDK 一致性** | 同一个节点：TS `applied 2/2` vs Python `applied 2/2` —— `state` 与 `nodesApplied/nodesTotal` 必须**逐一相同** |

### C. 两次反向证伪（其中第一次**没打中**，当场改对）
1. **`isDoneState` 恒真**（即"把 202 当成功"那个原始 bug）⇒ C2 两条 + C3 一条断言变红（`done=True`、`raised=None`）⇒ 证明这三条断言测的确实是三态语义。
2. **跨 SDK 一致性**：第一版把 `nodesApplied: 0,`（一个**默认/空结果**构造点）改成 `+1` —— **探针静默失效**（那条路径根本不在成功响应上），drill 依旧全绿。用 `grep -n "nodesApplied"` 找到**成功路径**的构造点（`client.ts:440` `nodesApplied: nodesApplied ?? 0,`）后重打 ⇒ **C1 的节点计数断言与 P 的一致性计数断言同时变红**（`ts=3/2 py=2/2`）⇒ 一致性断言确实在比数字。两次探针都以 `cp` 备份 + `cp` 还原（`cmp` 逐字节一致，`grep -c FALSIFY` = 0）。
   （教训：**探针必须证明自己执行过** —— 这是本会话第四次同族；换行的 `grep` 定位比"猜哪个构造点是活的"便宜得多。）

### D. 结论
**两份实现对同一份契约没有发现漂移** —— 这是"跑真节点"这条线第一次以**否定**收场（前几轮都是抓到真缺陷）。同样诚实记录：本轮**未改产品代码**（Rust 与 TS/Python SDK 的 `src` 都没动），产出是**新增覆盖 + 一条新的断言类型（跨实现在位一致性）**。

### E. 文档与接线
`tools/hydra-ts/README.md` 新增 "How this contract is verified against a REAL node"（与 `hydra-py` 那份对称，含"为什么用中继剪断总线"与跨 SDK 一致性一节）。CI：`integration` 作业新增 `npm install && npm run build` + 单节点腿；`live-deps` 作业新增集群腿（同一作业里先跑 Python 版本的集群腿）。本机门禁新增两条 entry（含先 `npm run build` 以保证测当前源码）。`check_ci_wiring` exit 0（工件 **19**）。

### F. 本轮验证
`python3 integration/test_sdk_ts_live.py` ⇒ **PASSED（10 条）**；`--cluster` ⇒ **PASSED（12 条）**；两次证伪如期变红并已恢复；`check_i18n` / `check_ci_wiring` / `check_e2e_contracts` / `check_source_purity` 全绿；`ci.yml` 可解析、门禁脚本语法 OK。

---

## 2bq. 第八十四轮：**第三份实现**（Go SDK）对着真节点 —— 并升级为**三方一致性**断言（三份实现、一份契约）

第八十轮（Python）与第八十三轮（TypeScript）之后，`tools/hydra-go` 是同一份租户契约的**第三份实现**，而它同样**从未离开过 mock**（`go test ./...`，CI `sdks` 作业）。本轮新增 `integration/test_sdk_go_live.py`（**单节点 10 条 + 集群 12 条断言**，接进 CI `integration`/`live-deps` 与本机门禁两条 entry；`check_ci_wiring` 仓外测试工件 19 → 20），并把第八十三轮的"跨 SDK 一致性"升级成**三方**：Go == Python == TypeScript。

### A. 做法（Go 特有的三个坑，都踩到了）
driver 是一个 **sidecar 模块**（`.acceptance/sdk-go-live/driver/`），`go.mod` 里用 `replace github.com/ipconfiger/hydra/tools/hydra-go => <repo>/tools/hydra-go` **把本地 SDK 当普通依赖引入** —— 这样测的是"消费者如何使用这个包"，而不是包内测试。三个真坑：
1. **仓库根有 `go.work`**（`use ./tools/hydra-go`）⇒ 放在仓库里的 driver 会被拉进工作区，而工作区模式下 `-mod=mod` 被拒（`-mod may only be set to readonly or vendor when in workspace mode`）⇒ driver 改用 `GOWORK=off` 构建，成为一个独立的消费方模块。
2. **沙箱的 `/` 是只读的** ⇒ 默认 `~/.cache/go-build` 写不进去，报出的却是**看起来毫不相干**的错误：`package context is not in std (/usr/lib/golang/src/context)`（`context.go` 明明就在那儿，我刚才 `ls` 过）外加一条打不开 cache 文件的错。第一次我先去怀疑工具链/GOROOT，直到看见那条 cache 报错才转向 ⇒ **把 `GOCACHE`/`GOPATH`/`GOMODCACHE` 全部指到工作区内的 `.acceptance/`** 后立刻通过。（教训：**"标准库不见了"这类荒谬报错，先找被写坏的缓存/权限**；这也是本会话"先怀疑环境再看产品"的又一次。）
3. driver 里用 `%T` 打印**错误类型**（`*hydra.InvalidatePendingError` / `*hydra.InvalidateUnavailableError`）并用 `errors.As` 取回附带的 result ⇒ 断言的是"类型正确 + 附带结果完整"，而不只是"出错了"。

### B. 实测（全部通过）
| 腿 | 实测 |
|---|---|
| G1 文档用法 | `200 single_node`、`State.Done()` 真、`traceId=hydra-…-12`、`1/1` |
| G2 **真的清了缓存** | 允许→200、翻成拒绝→**仍 200（命中缓存）**、Go SDK 失效→下一次 **401 `denied`** |
| G3 另一租户令牌 | `*hydra.HTTPError`（不伪装成 done） |
| G4 旧管理面路径 ×2 | **404** |
| C1 活总线 | **200 `applied`、2/2、`eventId`、~486 ms** |
| C2 `WaitNone` | **202 `pending`、`lagging=[两节点]`、2 个 live 节点**；`Done()` 假、返回 **`*hydra.InvalidatePendingError` 且带完整 result** |
| C3 剪断总线 | **503 `unavailable`**（断言**不是** pending）+ **`*hydra.InvalidateUnavailableError`** |
| **P3 三方一致性** | 同一节点：**go `applied 2/2` == py `applied 2/2` == ts `applied 2/2`** |

### C. 两次反向证伪（第二次**第一版又没打中**，第二次同族教训）
1. **`Done()` 恒真** ⇒ C2 两条 + C3 一条断言变红（`done=True`、两个错误类型都不再出现）。
2. **三方一致性**：第一版我把 `res.NodesApplied = fleet.NodesApplied` 的第一处（行 728，**503 非 2xx 分支**）改成 `+1` —— **探针静默失效**（C1/P3 测的是 2xx 分支，行 755）⇒ drill 依旧全绿。读那两段代码（728 在 `if resp.StatusCode < 200 || >= 300` 里）后改打 **2xx 分支** ⇒ **C1 计数断言与 P3 的计数断言同时变红**（`go=3/2 py=2/2 ts=2/2`）。两次探针均 `cp` 备份 + `cp` 还原（`cmp` 逐字节一致，`grep -c FALSIFY` = 0）。
   **教训（与第八十三轮完全同族，第二次）：`grep -n` 只能告诉我"有几处赋值"，不能告诉我"哪一处是活路径" —— 必须读上下文/条件，否则探针就是在测一行死代码。**

### D. 结论
**三份实现对同一契约没有发现漂移** —— 与第八十三轮一致，"跑真节点"这条线连续第二轮以**否定**收场。同样诚实记录：本轮**未改产品代码**（Rust 与三份 SDK 的源码都没动），产出是**新增覆盖 + 把跨实现一致性从两方升级为三方**。

### E. 文档与接线
`tools/hydra-go/README.md` 新增 "How this contract is verified against a REAL node"（与 `hydra-py`/`hydra-ts` 对称，并写明 Go 特有的 `GOWORK=off` + sidecar `replace` 用法与"为什么用中继剪断总线"）。CI：`integration` 作业新增 `actions/setup-go@v5` + 单节点腿；`live-deps` 新增集群腿（先构建 TS dist 供三方一致性使用）；本机门禁新增两条 entry。`check_ci_wiring` exit 0（工件 **20**）。

### F. 本轮验证
`python3 integration/test_sdk_go_live.py` ⇒ **PASSED（10 条）**；`--cluster` ⇒ **PASSED（12 条）**；两次证伪如期变红并已恢复；`check_i18n` / `check_ci_wiring` / `check_e2e_contracts` / `check_source_purity` 全绿；`ci.yml` 可解析、门禁脚本语法 OK。

---

## 2br. 第八十五轮：把**出厂运维路径**真跑一遍 —— 用**发布的 Dockerfile 建镜像 + 启发布的 compose**，在容器里验证 `docker stop` 的排空（第八十二轮以来第一次动"部署件"）

第四轮/第六十四轮留下的"未做"清单里有这么一条：**`docker stop` 端到端从未验证过（需要构建镜像）**。第六十四轮只在**裸二进制**上量过 SIGTERM 25s / SIGQUIT 35s / SIGKILL 丢 20 行，然后据此给 7 个 hydra 服务加了 `stop_grace_period: 30s` —— 但"运维真的按 `docker stop` 会怎样"一直没人测。本轮把它测了。

### A. 做法（绕开三个环境坑）
1. **二进制来源**：文档路径 `environment/build.sh` 第 1 步调用 `rust_build_linux`（`cargo zigbuild` 包装脚本）—— **本机没装这个包装脚本**，所以退化为**等价步骤**：在 `crates/hydra-server` 下 `cargo build --release --features server,cluster-redis,usage-clickhouse --bin hydra`（本机就是 x86_64-linux，原生 release 即是目标产物），再按 build.sh 第 2 步 `cp -f target/release/hydra bin/hydra`。偏离已如实记录（不是"照文档跑通"，是"照文档的等价路径跑通"）。
2. **镜像**：按交付的 `environment/Dockerfile` `docker build -t hydra:e2e .`（**另打 scratch tag，不覆盖用户自己的 `hydra:latest`**）。第一次失败是因为 buildx 要写 `~/.docker/buildx`，而**沙箱根目录只读** ⇒ 用 `DOCKER_CONFIG=$PWD/.acceptance/docker-config` 把 CLI 状态目录移进工作区。
3. **起栈**：交付的 `environment/docker-compose.yml`（单机三件套 hydra + mock-tenant + clickhouse）**端口全部被用户的本地栈占用**（8080-8084/8091?/8123…）⇒ 写了一个 **scratch override**（`.acceptance/compose-e2e-override.yml`）：只改**宿主侧端口**（18080/18081/18123/19000/19091）、改容器名（`hydra-e2e*`）、加一个 compose **没有**的 chat mock（否则代理请求不可能成功，第 5 步的"用量是否被刷盘"就成了 0 行对 0 行的空断言），并用 `-p hydra-e2e` 让命名卷私有。**容器内部端口/健康检查/env/网络一律不动** ⇒ 被测的仍是交付件。

### B. 实测（drill `COMPOSE DRILL: PASSED`，日志 `.acceptance/round85/drill.log`）
| 步骤 | 实测 |
|---|---|
| 启栈 + **交付的健康检查** | 4/4 服务 **healthy**（hydra / clickhouse / mock-tenant / mock-llm） |
| 管理面 | `/api/v1/health` **200**（带 token）；`/healthz` **401**（role=all 下仍受 token 门禁 —— 与 CI 注释一致） |
| 真实流量 | 经容器 **20/20 请求 200**（`Host: e2e.local`） |
| **`docker stop`** | **耗时 25 s**（与第六十四轮裸二进制 SIGTERM 25.02s **完全吻合**），退出后 **usage 行数 0 → 20，20 行全部落库** ⇒ `stop_grace_period: 30s` 这条修复在**运维真正使用的那条路径**上成立；Docker 默认 10s 会在排空完成前 SIGKILL |
| **对照组 `docker kill`** | 0 s、**缓冲的 20 行全部丢失**（行数停在 20）⇒ 上面那条断言不是"总能过"的空话 |

### C. 顺带查出的**环境问题**（不是产品缺陷，但会咬人，已写进交付文件）
ClickHouse 的 schema 由镜像 entrypoint 在**首次启动**时读 `/docker-entrypoint-initdb.d/init.sql` 执行；**本机 `/home/alex` 是 `0700`**，容器用户（甚至 root）无法穿越进仓库 ⇒ 挂载读成 `Permission denied`，CH 起来时**没有 `usage_record` 表**，hydra 的 sink 每次重试都报
`WARN hydra_server::sink: usage sink batch insert failed … Table default.usage_record does not exist`。
用 `docker run -v <repo>/environment/clickhouse/init.sql:/x.sql:ro … head -1` **独立复现**了 `Permission denied`，确认是**宿主权限**而不是 compose 写错（正常部署不会把仓库放在 0700 home 下；CI 的 `live-deps` 作业本来就断言过这张表）。**处理**：在 `environment/docker-compose.yml` 的挂载点旁写清这个前提与自查命令（`SHOW TABLES`）+ 手工补救（`curl --data-binary @clickhouse/init.sql http://127.0.0.1:8123/`），drill 里也照此把 schema 补上再测排空。

### D. 我自己的两个夹具错误
1. **compose 会"合并"序列字段而不是替换**：我的 override 用普通 `ports:` ⇒ 与基础文件的 publish **相加**，于是 `Bind for 127.0.0.1:8123 failed: port is already allocated`（ch 端口撞上用户的实例），随后一整套步骤全在测空气（API 全 000）。正确写法是 **`ports: !override`**（Compose ≥ 2.24）。
2. **把 ClickHouse 的报错文本喂进 `$(( ))`**：`rows()` 直接把响应当数字用 ⇒ `Code: 未绑定的变量`（bash 把它当变量名）。改成"只接受纯数字，否则视为无值"，并给算术加保护。另外 `docker stop hydra-e2e-hydra` 打错对象名（我设了 `container_name: hydra-e2e`）⇒ 改为按**服务名**停（`compose stop hydra`）。
3. drill 第一版**只打印不断言**（全都 000 也 exit 0）—— 典型的"守卫静默通过"；已改成 `check()` 累加失败并 `exit 1`。

### E. 收尾与清洁
scratch 镜像 `hydra:e2e` 已 `docker rmi` 删除；`bin/hydra`（暂存产物）已删除 —— 正是第五十七轮记录过的"几周前构建的 `bin/hydra` 被 `docker build` 打进镜像"陷阱，不留残骸；`compose down -v` 移除了私有命名卷；**用户的 dev 栈全程未被触碰**（`15 healthy` ×3）。交付文件只加了一处**注释**（compose 里 init.sql 挂载点），`check_compose_grace` / `check_compose_health` / `docker compose config -q` 均通过。

### F. 本轮验证
drill **PASSED**（6 条断言：端口空闲 / 管理面 200 / healthz 401 / 20×200 / stop ≥15s 且 20 行全落 / kill 丢缓冲）；`check_compose_grace` / `check_compose_health` / `check_compose_health.test` / `docker compose config -q`（官方单机 + 集群 + 本地三份）全绿；`fmt --check` 不变（无 Rust 改动）。**本轮未改产品逻辑**（唯一成果性改动是交付 compose 里的一段注释）。

---

## 2bs. 第八十六轮：`ops.md` 环境变量表里的**默认值**从没人核过 —— 新增守卫，**18 个默认值逐一对上代码**

`check_documented_env.cjs`（第六十四轮）只证明"表里点名的每个变量**被读过**"，**完全不管值**。而那张表第二列/描述里的数字正是运维据以**推算 `terminationGracePeriodSeconds`、各类超时、内存上限**的东西（例：`HYDRA_SHUTDOWN_DRAIN_SECS` default 20 → 排空预算 20+5+slack）。这些数字全是手写的，**此前没有任何东西与代码比对过**。

### A. 新增 `scripts/check_documented_defaults.cjs`（+ `…test.cjs`）
**方法**（刻意保守 —— 会"编造默认值"的守卫比没有守卫更坏）：
1. 从 ops.md 表格行取**文档侧默认值**：短的第二列（`` `60` ``、`60 s`、`32 MiB (= 33554432 **bytes**)`）或描述里的 `default N`；**`*(unset)*` 与 `host:port` 这类非数字格子一律判为"非数字"**（第一版把 `` `127.0.0.1:8081` `` 读成 127、`` `0.0.0.0:8080` `` 读成 0，已修）。
2. 从**代码**取回退字面量，覆盖本仓库实际存在的四种形态：①`env_positive_u32("VAR", 60)` 这类"把默认值当参数传"；②`unwrap_or(120)` / `unwrap_or_else(|| Duration::from_millis(2_000))`；③**本地解析函数**（`parse_shutdown_drain_secs(...)`、`parse_*_timeout_secs(...)`）—— 顺着调用名找到函数体里的 `unwrap_or`；④**具名常量**（`unwrap_or(DEFAULT_FORWARD_TIMEOUT_SECS)`、`time_bound::DEFAULT_USAGE_MAX_WINDOW_DAYS`）与**结构体默认值**（`unwrap_or(ProxyConfig::default().max_request_body_hard)` → 去 `impl Default for ProxyConfig` 里取该字段的初始化值）。`32 * 1024 * 1024` 会求值。
3. **只在两侧都拿到数字时比较**；取不到的一律打印 `????` 并注明"哪一侧没有数字 + 代码站点"，**绝不算通过**；再加**覆盖率下限**（`MIN_COMPARED = 10`，可用 `CDD_MIN_COMPARED` 覆盖）—— 低于下限直接 exit 1，防止守卫退化成"什么都不比还报 OK"。

**结果**：**18 个默认值全部与代码一致**（含 `SHUTDOWN_DRAIN_SECS=20`、`REGISTRY_STALE_GRACE_SECS=120`、`ADMIN_AUTH_FAIL_LIMIT_PER_MIN=10`、`FORWARD_TIMEOUT_SECS=5`、`AUTH_ALLOW_TTL_MAX_SECS=300`、`MAX_REQUEST_BODY_HARD=33554432`、`REQUEST_BODY_TIMEOUT_SECS=60`、`UPSTREAM_FIRST_BYTE=30`/`STREAM_IDLE=120`、五个 `TENANT_API_*`、`TENANT_CONFIG_WRITE_PER_MIN=60`、`CLICKHOUSE_QUERY_TIMEOUT_MS=5000`、`ENCRYPTION_KEY_VERSION=1`），另有 **14 行**因格子非数字（`*(unset)*`、`0.0.0.0:8080`、`leader/edge`、角色/路径等）被诚实地标为**无法核对**。**本轮没有发现任何漂移** —— 这是"文档 ↔ 事实"这条线又一次以**否定**收场。

### B. 双向反向证伪（都在真树上做，`cp` 备份/还原）
1. **文档动**：把 ops.md 的 `HYDRA_SHUTDOWN_DRAIN_SECS` 描述从 `default 20` 改成 `default 25` ⇒ exit 1，`DRIFT HYDRA_SHUTDOWN_DRAIN_SECS: ops.md says 25 (prose: default 25) but the code falls back to 20 @ crates/hydra-server/src/main.rs:1657 (parse_shutdown_drain_secs().unwrap_or)`。
2. **代码动**：把 `registry_stale_grace_secs()` 的 `unwrap_or(120)` 改成 `unwrap_or(90)` ⇒ exit 1 并点名 `falls back to 90`。恢复后 exit 0。

### C. 我自己的四个错误（全在抽取器里，都是"守卫自己错"这一族）
1. **正则不允许跨行**：`"VAR"[^
]{0,80}?unwrap_or` 永远匹配不到"变量在一行、`unwrap_or` 在几行后"的常见写法 ⇒ 第一版只比出 **5** 个（覆盖率下限当场把我拦住，没让"5 个全对"冒充通过）。
2. **窗口跨越到**别的**旋钮**上：用 `lines.slice(i, i+12)` 时，窗口里可能落进**下一个**变量的 `unwrap_or(5000)`，从而把别的默认值算到这个键头上（**假绿**风险）。现在窗口在**下一个 `HYDRA_*` 字面量处截断**，同时保留"变量前面那几行"（包裹它的调用）——两个窗口，各有用途。
3. **`([^)]{1,60})` 在 `ProxyConfig::default()` 的第一个 `)` 处截断** ⇒ 常量/字段解析全部落空，`fn=` 调试输出 `inner="ProxyConfig::default("` 才露馅 ⇒ 改成"取到行尾再让解析器剥掉标点"。
4. **`impl Default` 找不到时退化成扫全文件** ⇒ 命中**结构体字段声明**（`pub field: u64,`）→ 数字为 `u64` → 报"没有默认值"。改成**只在 `impl` 块内找**。另外 `return Ok(Duration::from_secs(
 const))` 的捕获上限 40 字符把常量名截掉了（放宽到 200）。

### D. 测试与接线
`scripts/check_documented_defaults.test.cjs`（**13 条**，`node --test`）：6 条纯函数用例（数字格子 / 描述里的 default / `32 MiB (= 33554432 bytes)` **取字节数而非 32** / 拒绝 `host:port` / 拒绝 `*(unset)*` / `32*1024*1024` 求值 / 下划线数字），7 条端到端用例（夹具匹配 ⇒ exit 0；**文档漂移 ⇒ exit 1 且同时点名两个数与代码站点**；**代码漂移 ⇒ exit 1**；覆盖率不足 ⇒ exit 1；**表格为空 ⇒ 也 exit 1**）。为此给脚本加了 `CDD_OPS` / `CDD_SRC` / `CDD_MIN_COMPARED` 覆盖（夹具只有一个源文件，所以"至少 10 个源文件"的健全性检查在**显式覆盖时**跳过 —— 那条检查是防"ROOT 弄错"的，不是防夹具的）。CI：`scripts` 作业加两步；本机门禁加两条 entry；`check_ci_wiring` exit 0（脚本工件 19 → **21**）。

### E. 本轮验证
`node scripts/check_documented_defaults.cjs` ⇒ **exit 0（18 比对全等、14 无法核对）**；`node --test scripts/check_documented_defaults.test.cjs` ⇒ **13/13**；两次真树证伪如期变红并已恢复（`cmp` 一致）；`ci.yml` 可解析、门禁脚本语法 OK；`fmt --check` 不变（**本轮无 Rust 改动**）。

---

## 2bt. 第八十七轮：`§9.1 告警表` 的 PromQL **从未与指标注册比对过** —— 新增守卫（指标名 + **标签键**），15 处引用全部解析成功

`dev-docs/ops.md` §9.1 是**写给运维仓库的契约**，它自己就写着"每个表达式用的都是本仓库里真实存在的指标名与标签"。规则文件在**另一个仓库**，所以这里写错一个字母的后果不是文档瑕疵，而是**一条永远不会触发的规则 = 静默的监控空洞**（只会在事故里被发现）。此前没有任何东西把这张表与注册代码对过。

### A. 新增 `scripts/check_alert_expressions.cjs`（+ `…test.cjs`）
**会判失败的两件事**：
1. 表达式里每个 `hydra_*` 名字必须在 `crates/<crate>/src` 里**注册过**（名字由 `const` 拼出来的会解析常量）；找不到时报错并给出**近似候选**（`did you mean: …`）。
2. `{…}` 选择器里的每个**标签键**必须是**该指标自己注册的标签**之一 —— 这是最容易写错、后果最沉默的一类（选择器指向不存在的标签 ⇒ 永远匹配不到任何序列）。报错时打印该指标的真实标签列表与定义文件。
**只报告、不判失败的一件事**：标签**值**（`{reason=~"no_key|breaker_dead"}`）。对字面量做"代码里有没有出现"的检查**既太弱又太强**：它分不清活值与死值（表里那行"**不要用** `stage=\"connect\"`"的警告本身就说明有死值），而 `result=~".+_throttled"` 是运行时拼出来的、字面量当然不在代码里 —— 所以这些只作为 `note` 打印给人工看，**既不算通过也不算失败**，理由写在脚本头部。
**覆盖率下限**：§9.1 里的指标引用少于 10 处即 exit 1（防"提取坏了、什么都没比还报 OK"）；`CAE_OPS`/`CAE_SRC`/`CAE_MIN_REFS` 可覆盖以便夹具测试。

**结果**：**15 处指标引用、5 处标签选择器，全部解析成功**（53 个已注册指标）—— **未发现漂移**。这是"文档↔事实"这条线连续第三轮以**否定**收场（第 83 轮跨 SDK 一致性、第 86 轮默认值、本轮告警表达式）。同时诚实记录一处**已知的活值/死值盲区**：`stage="connect"`（第 65 轮记录的死值）我这套检查**检测不出来**，因为 `"connect"` 这个字面量在代码里确实存在（别的语境）—— 这正是把"值"降级为 observation 的原因，而不是硬判。

### B. 双向反向证伪（真树、`cp` 还原）
1. **标签键写错**：`hydra_listener_bound{protocol="tls"}` → `{protocoll="tls"}` ⇒ exit 1，`DRIFT hydra_listener_bound{protocoll…}: the metric registers labels [protocol] @ crates/hydra-server/src/admin/metrics.rs, so a selector on \`protocoll\` can never match`。
2. **指标名写错**：`hydra_registry_nodes` → `hydra_registry_node` ⇒ exit 1，`DRIFT … is not registered anywhere … (did you mean: hydra_registry_nodes, hydra_registry_reaped_total?)`。恢复后 exit 0（`cmp` 一致）。

### C. 我自己的两个错误（都在守卫自身）
1. **块注释里写了 `crates/*/src`** —— 其中的 `*/` **提前终止了注释**，于是后面的散文变成代码：`SyntaxError: Unexpected token 'const'`。修了头部后**第二处同样的写法**又炸了一次（`Unexpected identifier 'cannot'`）。教训：**在 `/* */` 注释里写通配路径必须避开 `*/`**（改用 `crates/<crate>/src`），而且**同一个错误模式要一次性全量搜出来**，别一处一处修。
2. **"至少 20 个注册"的健全性检查把夹具挡住了**（夹具只有 2 个注册）⇒ 与第八十六轮同样的修法：**显式 `CAE_SRC` 覆盖时跳过下限**（那条下限是防"ROOT 弄错"，不是防夹具），但仍要求 ≥1 个注册。

### D. 测试与接线
`scripts/check_alert_expressions.test.cjs`（8 条）：夹具里注册一个带 `protocol` 标签的 gauge 与一个无标签 counter，然后断言 —— 真实标签的选择器 ⇒ exit 0；**未知标签 ⇒ exit 1 且打印该指标真实标签与文件**；**未注册指标 ⇒ exit 1 且给出近似候选**；**对"无标签指标"加选择器 ⇒ exit 1 且显示 `[none]`**；覆盖率不足 ⇒ exit 1；**缺 §9.1 表 ⇒ exit 2（绝不 0）**。CI：`scripts` 作业加两步；本机门禁加两条 entry；`check_ci_wiring` exit 0（脚本工件 21 → **23**）。

### E. 本轮验证
`node scripts/check_alert_expressions.cjs` ⇒ **exit 0**（15 引用 / 5 选择器全解析 / 53 指标）；`node --test …test.cjs` ⇒ **8/8**；两次真树证伪如期变红并已恢复；`ci.yml` 可解析、门禁脚本语法 OK；**本轮无 Rust 改动**。

---

## 2bu. 第八十八轮：租户 API **错误码 ↔ HTTP 状态**的外部契约从没核过 —— 新增守卫，**33 个错误码全部与代码一致**

`dev-docs/tenant-api-integration.md` §6 是**对外契约**：集成方按 `code` + HTTP 写重试逻辑（`429` ⇒ 等 `Retry-After`；`503 no_leader` ⇒ **必须换边缘节点**；`504 forward_result_unknown` ⇒ **先重读再重试**）。这一列与代码漂移的后果**不是"某个端点没了"**，而是每个照文档实现的客户端**按错误的方式重试** —— 而且**什么都不会失败**，没有编译错误、没有测试红、没有日志异常。此前没有任何东西把这张表与代码对过。

### A. 新增 `scripts/check_tenant_error_codes.cjs`（+ `…test.cjs`）
**做法**：对 §6 表里每个 `code`，找到**真正发出它**的站点，并从同一表达式里读回状态码。本仓库实际有四种形态，全部支持：
1. `respond_error(session, ctx, 400, "invalid_wait", …)` —— 状态是第 3 个参数；
2. `err_json(429, "too_many_requests", …)`（`admin/tenant_config_api.rs` 的写预算节流）—— 相邻字面量；
3. `Err((413, error_body("payload_too_large", …)))` —— 状态在共享 body 构造器**旁边**的元组里；
4. **映射函数**：`BoundError::code()` 把枚举映射成字符串，状态由**调用方**给（`respond_error(session, ctx, 400, e.code(), …)`）—— 此时去调用点取 400。
**只在能读到"文档状态 + 代码状态"时比较**；读不到的一律 `????` 并注明，**绝不算通过**；另有**覆盖率下限**（15，`TEC_MIN_COMPARED` 可覆盖）。`TEC_DOC`/`TEC_SRC` 供夹具测试。

**结果**：**33 个文档化错误码全部与代码发出的状态一致，0 个无法核对**（第一版有 4 个 `????`：`payload_too_large` 与三个 `BoundError` 码 —— 补上形态 3/4 后归零）。**未发现漂移** —— "文档↔事实"这条线**连续第四轮以否定收场**（83 跨 SDK、86 默认值、87 告警表达式、88 错误码）。

### B. 双向反向证伪（真树、`cp` 还原）
1. **表动**：§6 里 `too_many_requests` 的 `429` 改成 `400` ⇒ exit 1：`DRIFT too_many_requests: the table says HTTP 400 but the code emits it with 429 — a client's retry logic follows the table`。
2. **代码动**：`admin/tenant_config_api.rs` 的 `err_json(429, "too_many_requests", …)` 改成 `400` ⇒ exit 1：`… the table says HTTP 429 but the code emits it with 400`。恢复后 exit 0（`cmp` 一致）。
   （第一次尝试从**代码侧**证伪时，我用的正则没匹配上那个跨行的调用（`site found: False`）—— **探针自己没打中**，改为按实际形态定位后才成功；这是"探针必须证明自己执行过"的第 N 次同族。）

### C. 一个自我纠正（探针里的类型 bug）
第一版探索脚本用 `status in found` 比较，而 `status` 是 **int**、`found` 是**字符串列表** ⇒ 27 个码**全部**显示 `MISMATCH`（含明显正确的那些）。**"跨所有输入完全一致的失败 = 夹具/探针 bug"** 这条老教训再次生效；修好类型后信号才有意义。

### D. 测试与接线
`scripts/check_tenant_error_codes.test.cjs`（**9 条**）：四种形态各一条"匹配 ⇒ exit 0"（**形态若哪天不再被识别，覆盖率会掉而不是静默放过**）；**表动 ⇒ exit 1 且同时点名两侧**；**代码动 ⇒ exit 1**；覆盖率不足 ⇒ exit 1；**缺 §6 表 ⇒ exit 2（绝不 0）**。CI：`scripts` 作业加两步；本机门禁加两条 entry；`check_ci_wiring` exit 0（脚本工件 23 → **25**）。

### E. 本轮验证
`node scripts/check_tenant_error_codes.cjs` ⇒ **exit 0（33/0）**；`node --test …test.cjs` ⇒ **9/9**；两次真树证伪如期变红并已恢复；`ci.yml` 可解析、门禁脚本语法 OK；**本轮无 Rust 改动**（只改测试夹具与文档，探针已还原）。

---

## 2bv. 第八十九轮：把**对外租户契约**（`tenant-api-integration.md` §4 + 附录）真跑一遍 —— **找到并修正 §4.4 一个错误的例子**（405 而非 404）

第八十八轮把 §6 的 `code → HTTP` 表**静态**对上了代码；本轮把同一份文档的**行为面**（§4 通用约定 + §5 端点 + 附录的九条路由）**在真节点上跑**。这份文档是**外部集成方**照着写的，附录那九条路由是他们直接复制的对象。新增 `integration/test_tenant_api_contract.py`（**53 条断言**，接进 CI `integration` 作业与本机门禁；`check_ci_wiring` 仓外测试工件 20 → **21**）。

### A. 覆盖的承诺（全部实测通过）
| 段 | 实测断言 |
|---|---|
| §4.2 | 2xx 响应带 `X-Hydra-Trace-Id`；`Content-Type: application/json`；**200 上没有 `Retry-After`** |
| §4.3 | 每个非 2xx 都是 `{"error":{code,message,trace_id}}` 信封，且 **`error.trace_id` 与响应头逐字符相同** |
| §4.1 | **`Host` 不参与身份判定**（换任意 Host 仍 200）；t2 令牌打 t1 的 URL ⇒ **403 `tenant_id_mismatch`**；缺失／错误／该租户未配置令牌 ⇒ **三者都是 401 且 `message` 完全相同**（无"哪些令牌存在"的探测面） |
| §4.4 / §5 | 四条只读路由（`whoami`/`usage`/`sub-tenants`/`sub-tenant-routes`）+ 两条列表可由令牌访问；四条只读路由用错方法 ⇒ **405**；保留前缀下的未知路径 ⇒ **404 `not_found`**（本 API 自己回答） |
| §4.5 / §5.6 | 重复 `invalidate` 安全（两次 200，第二次 `invalidated=0` **不是错误**）；重复 `PUT` **收敛到同一不可变 id** 并更新字段；`DELETE` 两次都 **204**（删已不存在的 id 是 no-op）；写后可读侧一致 |
| §5.3 / §5.6 | 缺 `since` ⇒ 400 `invalid_since`；`group_by` 越界 ⇒ 400 `invalid_group_by`；窗口 >31 天 ⇒ 400 `window_too_large`；非法 JSON ⇒ 400 `invalid_request`；空 `key_prefix` ⇒ 400 `empty_key_prefix` |

### B. ★ 本轮真缺陷：文档 §4.4 的那个例子是**错的**
原文写着"写端点按 `(方法, 路径)` 解析……用错方法（**例如 `GET`/`POST` 打到写路径**）→ `404 not_found`（**不是** 405）"。逐格实测（`GET/POST/PUT/DELETE` × 七条路径）后发现**同一个"用错方法"会得到两种答案**，取决于**该路径形状是否也是一条只读路由**：
```
/sub-tenants                 GET 401(已路由)  POST 405  PUT 404  DELETE 404
/sub-tenants/NAME            GET 404          POST 404  PUT 401(已路由)  DELETE 401
/sub-tenant-routes           GET 401(已路由)  POST 405  PUT 401(已路由)  DELETE 404
/sub-tenant-routes/ID        GET 404          POST 404  PUT 404          DELETE 401
/auth/cache/invalidate       GET 405          POST 401(已路由)  PUT 404  DELETE 404
```
根因（代码）：**方法检查属于路由**（`tenant_api/mod.rs:317-332`）—— 路径先匹配那五条只读路由，方法不对就 **405**；写路由按 `(方法, 路径)` 注册，**形状对不上**才是 **404**。所以 `POST /tenant/{tid}/api/v1/sub-tenant-routes` 恰好打在一条**同时是只读路由**的路径上 ⇒ **405**，而文档那个例子说 404。
**处理**：把 §4.4 改成**实测出来的规则**并附上一张逐格表（405 = 路径是只读路由但方法不对；404 = 没有任何 `(方法, 路径)` 组合），并在文档里明确标注"**旧文档里那个例子是错的**"。客户可见影响：照旧文档猜"404 ⇒ 路径不存在"的集成方会被误导（按 §6 的 `code` 分支处理则不受影响）。

### C. 两次反向证伪（真树、`cp` 还原 + 重新构建）
1. **把 405 改成 404**（即恢复旧文档的说法）⇒ 探针 **5 条**断言变红（四条只读路由 + `POST /sub-tenant-routes` 那条）。
2. **跳过 URL/令牌交叉校验**（403 改成 200）⇒ `§4.1: … the status is 403` 变红。
恢复后 `mod.rs` 与备份 `cmp` 一致、探针 53/53 通过、`grep -c FALSIFY` = 0。

### D. 接线与验证
CI `integration` 作业新增一步（注释写明：§6 由第八十八轮静态守、行为面由本轮守，并且"它上线时就抓到了 §4.4 一个错误例子"）；本机门禁新增一条 entry。**门禁 54 项全绿、`OVERALL=GREEN`、exit 0**。记录：本节 + `INDEX.md` 第八十九轮。

---

## 2bw. 第九十轮：租户 API 的 **§7 边界**黑盒实测 —— 停用租户的**自救路径**成立，但文档漏了"你的客户端会看到什么"，且**两个错误信封不一样**

第九十轮继续同一条线（把对外文档的承诺真跑一遍）。§7 是集成方在**事故时**才读的一段，其中三条是承重的、而**黑盒从未验证过**（`tenant_api.rs` 的进程内测试只覆盖了快照视图，没有覆盖代理/HTTP 面）。新增 `integration/test_tenant_api_boundaries.py`（**20 条断言**，接进 CI `integration` 作业与本机门禁；仓外测试工件 21 → 22）。

### A. 实测（全部通过）
| 段 | 实测 |
|---|---|
| §7.1 自救路径 | 把租户 PUT 成 `enabled=false` 后：**`whoami` 仍 200 且 `enabled=false`**（文档恢复流程第 1 步）；**`POST auth/cache/invalidate` 仍 200**（第 3 步，实测 `invalidated:1`）；四条只读路由 + **写路由（PUT 子租户 200 / DELETE 204）全部照常**；重新启用后 `whoami` 立即变 `true` |
| §7.1 客户端侧 | **被拒**：403，体 `{"error":{"message":"tenant_disabled","type":"proxy_error"}}`（对照：启用时同一请求 200） |
| §7.3 前缀失效 | 先制造缓存（允许→200、翻成拒绝→仍 200），再用**前缀**调 `api_keys` ⇒ 200 且 **`invalidated==0`**，客户端**照常被服务**（文档原话"静默无效"，本轮把"静默"钉住）；改用**完整 key** ⇒ `invalidated==1` 且客户端下一次 **401** ⇒ 文档给的正解确实有效 |
| §7.5 `tenant_id` 参数 | 多传 `&tenant_id=t2` ⇒ **200（不是 400）且答案仍是 `t1`**（返回体 `tenant_id:"t1"` 与不传时逐字相同）⇒ "身份只认令牌、不越权"成立 |
| §7.5 空窗口 | `since/until` 一小时的窗口 ⇒ 200、`as_of: null`、`requests: null`（**"没有记录"不是错误**） |

### B. ★ 本轮的真缺口：文档没说"停用后你的客户端看到什么"，而且**两个错误信封不同**
§7.1 只说"你的客户端请求会被拒绝"，既没给状态码也没给 code；更要紧的是 —— **数据面 `/v1/*` 的错误信封与本文档 §4.3 写的不是同一个**。实测四种数据面错误：
```
401 {"error":{"message":"missing_api_key","type":"proxy_error"}}
403 {"error":{"message":"model_not_allowed","type":"proxy_error"}}
404 {"error":{"message":"unknown_domain","type":"proxy_error"}}
403 {"error":{"message":"tenant_disabled","type":"proxy_error"}}
```
—— **只有 `message` + `type`（恒为 `proxy_error`），没有 `code`、也没有 `trace_id`**（实现：`proxy.rs::short_circuit`）；而租户 API 用的是 `{"error":{code,message,trace_id}}`（`tenant_api` 的 `respond_error`）。一个只读本文档的集成方在客户端侧按 `error.code` 分支会**永远匹配不到**，报障时也拿不到 `trace_id`。
**处理**：§7.1 新增"停用后你的**客户端**看到什么"小节（状态码 + 信封原文）并**明确标注两者的差异**（数据面按 HTTP + `error.message` 判定，`code`/`trace_id` 只存在于租户 API 的信封里）。同时把这条差异**写进探针**：断言代理信封是 `{message,type}` 且**不含** `code`/`trace_id`，而租户 API 信封**必须**含 `code`/`message`/`trace_id`。

### C. 反向证伪（真树、`cp` 还原 + 重构建）
把代理里的停用检查改成死代码（`if false && !tenant.enabled`）⇒ 探针 **3 条**断言变红（拒绝、403 `tenant_disabled`、信封形状 —— 因为那时 200 根本没有错误体）。恢复后 `cmp` 一致、20/20 通过、`FALSIFY` 计数 0。
（另记一处**我自己的夹具 bug**：第一版断言"403 且 `error.code == tenant_disabled`"，而数据面的信封根本没有 `code` ⇒ 我按 §4.3 的信封去找字段。**正是这个红灯**让我发现文档缺的是"两个信封不一样"这件事 —— 夹具错误变成了本轮的真发现。）

### D. 接线与验证
CI `integration` 作业新增一步（注释写明覆盖 §7 的哪些边界）；本机门禁新增一条 entry；`check_ci_wiring` exit 0（工件 21 → **22**）。**门禁 55 项全绿、`OVERALL=GREEN`、exit 0**。记录：本节 + `INDEX.md` 第九十轮。**本轮产品改动只有文档**（`tenant-api-integration.md` §7.1 新增小节），Rust 逻辑未改（探针已还原）。

---

## 2bx. 第九十一轮：补上计划里自己记下的**流式（SSE）路径**缺口 —— §9.1 那两条"告警契约"第一次被亲眼看到触发

第六十三轮在计划里留了这句话："**未测**：上游是单个 JSON 而非 SSE ⇒ 真实流式路径（`ttft`/长连接）本轮未覆盖"。而 `ops.md` §9.1 把流式的两种失败都写成了**告警契约**：
- `hydra_upstream_stream_idle_timeout_total`："上游发了响应头（可能还有部分 body）后 `HYDRA_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS` 内没有字节……客户端已经拿到 200 与已到达的内容，所以这是**截断**的答复、不是可重试失败，且这条路径**同时喂熔断器**"；
- `hydra_mid_stream_errors_total`："在 200 + 首块已经发给客户端**之后**才发生的 chunk 读/写失败（无法故障转移）"。

也就是说：**这两条被写成"要被告警"的信号，此前从未有人见它们真的亮过**。本轮新增 `integration/test_streaming_path.py`（**12 条断言**，接进 CI `integration` 作业与本机门禁；仓外测试工件 22 → 23），用一个可控的 SSE 上游（三档：`incremental` / `wedged` / `dying`）在真节点上跑：

| 案例 | 实测 |
|---|---|
| **S1 增量投递** | 5 块 + `[DONE]`，到达时刻 **0.0 / 0.5 / 1.0 / 1.5 / 2.0 / 2.5 s**（每块间隔恰为上游的 0.5 s 暂停）⇒ **真的边到边发**。断言刻意放在**到达时刻**而不是 body 上——**缓冲后一次性发出的代理同样能通过 body 比对** |
| **S2 卡死的中途** | 上游发完头 + 第一块后沉默 8 s（空闲上界设 2 s）：客户端拿到 **200 + 第一块**，答复**被截断**（永不出现 `[DONE]`），且**连接在 2.04 s 被网关切断**（不是客户端放弃、也不是等上游的 8 s），`hydra_upstream_stream_idle_timeout_total{provider="p1"}` 由**不存在**变为 **1** ⇒ §9.1 的告警契约**首次被看到触发** |
| **S3 上游中途死亡** | 客户端 200 + 第一块后 **0.2 s** 连接结束、正文截断、无终止符；`hydra_mid_stream_errors_total{provider="p1"}` **1 → 2** |
| **S4 流式也计量** | 一次流式请求后 sqlite 里的 `usage_record` 行数增加（**计费不能漏掉流式**） |

### A. 我自己抓到的一个**空洞断言**（本轮最值得记的过程细节）
S2 最初写的是"空闲上界确实生效（连接没有挂过去）"＝`deltas[-1] < 上界+5`。但我的读循环**只在收到非空数据时记录时刻**，EOF 那一刻没有记录 ⇒ `deltas[-1]` 永远是**第一块**的 0.04 s，这条断言**恒真**（第一版输出里那句 `closed_after=0.04s` 就是它的"绿"，看着像证据其实什么都没测）。已改成**记录 EOF 时刻**并断言 `上界-0.7 ≤ eof_at ≤ 上界+3.5`（S3 用 `eof_at < 5`）—— 这才区分得出"网关在上界切了它"与"客户端自己放弃"。**同类教训在本会话第四次出现：先问"这条断言在什么情况下会红"**。

### B. 反向证伪（真树、`cp` 还原 + 重构建）
把网关的流空闲上界换成恒定 3600 s（即"永不触发"）⇒ **3 条**断言变红：`[DONE]` 出现了（不再截断）、连接在 **8.0 s** 才结束（上游沉默结束）、`hydra_upstream_stream_idle_timeout_total` **一次都没亮**。恢复后 `cmp` 一致、12/12 通过、`FALSIFY` 计数 0。

### C. 接线与验证
CI `integration` 作业新增一步（注释写明它补的是计划里记下的缺口、以及 S1 为什么断言到达时刻）；本机门禁新增一条 entry；`check_ci_wiring` exit 0（工件 22 → **23**）。**门禁 56 项全绿、`OVERALL=GREEN`、exit 0**。记录：本节 + `INDEX.md` 第九十一轮。**本轮无产品代码改动**（探针已还原）。

---

## 2by. 第九十二轮：**两层鉴权缓存**首次被实测（含"L2 是共享的"这条从未验证过的设计属性）—— 顺带为 **D-9** 留下行为基线

`design.md` / `design-tenant-api.md` 描述了两层鉴权结论缓存：进程内 L1 + Redis 支撑的 L2，存在的理由是"一个节点学到的结论不该被下一个节点重新学一遍"；`ops.md` §9.1 靠着它（`hydra_auth_cache_clear_total{layer,result}`），租户契约 §7.4 的"残余窗口"论证也靠它，而 **D-9** 问的正是"L2 要不要抽成 trait"。**这条属性从未被黑盒测过**。本轮新增 `integration/test_auth_cache_layers.py`（**11 条断言**，接进 CI **`live-deps`** 作业与本机门禁；仓外测试工件 23 → 24），用**真集群**（leader + edge）+ **真 Redis** + **会记数的 auth 上游**把"有没有再问一次鉴权服务"变成一个数字。

### A. 实测（全部通过）
| 腿 | 实测 |
|---|---|
| A L1 | **冷缓存**下第一次请求：200 且鉴权调用 **1**；同节点第二次：200 且调用**仍为 1**（L1 命中） |
| B L2 真的被写 | Redis 里出现 `hydra:{auth}:t1:<sha256>`，值 **`"1"`**（allow），且**租户索引集**里登记了该 hash（`del_tenant` 靠它做无 SCAN 的整租户清除） |
| **C L2 是共享的** | 同一个客户端 key 打到**第二个节点（edge）**：200，且鉴权调用 **1 → 1 未变** ⇒ 结论来自 **Redis L2**，不是又一次上游调用 |
| D 整集群清除 | 在 **edge** 上调 `invalidate` ⇒ **Redis 里的结论立刻消失（keys `[]`、索引 `[]`）**；随后 leader 上同一请求**重新问鉴权**（1 → 2）并被**拒绝（401）** |
| E deny 也会缓存 | 被拒之后 L2 里重新出现该 key，值为 **`"0"`**（deny，带更短 TTL） |

### B. 我自己的两个夹具错误（都在本轮被发现，也都是"测量口径错了"）
1. **mock 把每一个 POST 都计数**，而它同时扮演租户 `auth_url` 与 provider 上游 ⇒ 我实际在数**上游 chat 调用**，于是 A/C 两腿"失败"（看着像"L1 没命中/L2 不共享"）。改成**只计 `/auth`** 路径后，两条腿立刻转绿。**"跨多条腿一致的失败 = 夹具口径 bug"** 又一次成立。
2. **A 腿在"热缓存"上断言"第一次会问鉴权"** —— 我的预热循环已经把 L1/L2 填好了 ⇒ hits=0 是必然的。改成**先清一次缓存再计数**，A 才真的在测冷路径。
3. 另外 E 腿第一版断言"清除后 Redis 里没有这个 key"—— 但那是**在 D 腿请求之后**看的，而那次请求把 **deny 结论又写了回去**（同一个 key 名）。改成：**清除后立刻**断言 key 消失（D 腿内），D 腿请求之后再断言新写入的是 **deny（`"0"`）**（E 腿）。

### C. 反向证伪（真树、`cp` 还原 + 重构建）
把 L2 的**读**改成恒 miss（`if true { return Ok(None) }`）⇒ **C 腿**变红：edge 上同一 key 的鉴权调用 **1 → 2**（"结论来自共享 L2"不再成立）。恢复后 `cmp` 一致、11/11 通过、`FALSIFY` 计数 0。
**注意这条证伪只打了 C**：A/B/D/E 与 L2 读无关（A 是 L1、B 是写、D 是清、E 是写），所以"只红一条"正是**期望的形状** —— 若当时红了一片，说明腿之间互相污染。

### D. 对 **D-9** 的意义（不替用户决策，但把前提变成实测）
D-9 的选项是"把 `AuthCache::l2` 从具体类型改成 trait 对象 / 保持具体类型并接受 L2 竞态只能靠真 Redis 时序验证"。本轮给出的是**当前行为的基线**：L2 确实在被读写、确实跨节点共享、整集群失效确实同时清两层、deny 也会缓存 —— 也就是说**"抽成 trait"要保住的行为有四条可测的**，改动之后应该跑同一个 drill 复验。已把这一句写进计划 §2by，供决策时引用。

### E. 接线与验证
CI：放进 **`live-deps`** 作业（那里有真 Redis 与 cluster 特性集的构建），与 Python/TS/Go 三个 SDK 的集群腿并列；本机门禁新增一条 entry（自带 cluster 构建）。`check_ci_wiring` exit 0（工件 23 → **24**）。**门禁 57 项全绿、`OVERALL=GREEN`、exit 0**。记录：本节 + `INDEX.md` 第九十二轮。**本轮无产品代码改动**（探针已还原）。

---

## 2bz. 第九十三轮：**熔断器**的运维工作流首次黑盒跑通 —— 顺带纠正我自己对"跳过计数器属于哪个事件"的理解

三个文档承诺了这套工作流，而它**从未被黑盒执行过**（`load_breaker_swrr.rs` 是进程内单测）：
- `admin-ui/api-docs.js`：`GET /api/v1/breaker` = "熔断 dead-set：当前被排除在路由之外的 provider（连续失败）"；`DELETE /api/v1/breaker/{id}` = "强制清除该 provider 的熔断（标记为健康）"；
- `ops.md` §9.1：`hydra_candidate_skipped_total{reason="breaker_dead"}` —— "**本来会服务这个请求**的 provider 在候选集构建时被丢弃"的告警行；
- `design.md` §8.4：失败时 `on_failure`、2xx 首字节到达时 `on_success`。

新增 `integration/test_breaker_lifecycle.py`（**13 条断言**，接进 CI `integration` 作业与本机门禁；仓外测试工件 24 → 25）：一个 provider 指向**关闭的端口**、另一个指向健康 mock。

| 腿 | 实测 |
|---|---|
| B1 | 健康节点上 `GET /api/v1/breaker` ⇒ `{"dead":[]}`，`/api/v1/health` 的 `breaker_dead: 0`（**"不误报"方向**） |
| B2 | 连续失败（实测第 **5** 次后进入 dead-set）⇒ `dead=["p-flaky"]`、`breaker_dead: 1`（两个接口**同一个集合**）；此时**跳过计数器仍然是空的** —— 因为前面那些请求**都真的到达了**该 provider（是"尝试"而非"跳过"） |
| B3 | 带着死 provider 再发一次 ⇒ **`hydra_candidate_skipped_total{provider="p-flaky",reason="breaker_dead"} 1`** 出现（§9.1 的告警行第一次被看到触发），客户端拿 **503**（候选耗尽），而健康 provider 的模型仍 **200** |
| B4 | `DELETE /api/v1/breaker/p-flaky` ⇒ **`was_dead: true` 且 `dead: []`**；把该端口的上游**救活**后再发 ⇒ **200**（**重置不是装饰性的**） |
| B5 | 连续成功之后仍在 dead-set 之外（`on_success` 生效） |

### A. 我自己搞错的一件事（本轮最值得记的）
第一版把"跳过计数器会亮"断言放在 **B2**（刚触发熔断时）⇒ **红**。查下去才明白：**那个计数器属于"跳过"事件，不属于"失败"事件** —— B2 的每一次请求都**到达**了该 provider（作为候选被尝试、然后失败 502），只有 B3 那种"已死之后仍被选中又被丢弃"的请求才会跳过。这不是产品缺陷，是我**把指标挂错了事件**。修法：B2 改成断言**"此时不该有跳过计数"**（顺带得到"不误报"方向），把"会亮"挪到 B3（断言 `provider` 与 `reason` 双标签）。**同类教训（先问"这条断言在什么情况下会红/它测的是哪个事件"）在本会话第五次出现。**

### B. 反向证伪（真树、`cp` 还原 + 重构建）
把 `breaker_reset` 里的 `state.breaker.on_success(id)` 去掉（即"回 200 但不真的清"）⇒ **4 条**断言变红：响应里 `dead:["p-flaky"]`、`GET /api/v1/breaker` 仍列出、再次请求得 **503**（没有真的复活）、B5 仍留在 dead-set。恢复后 `cmp` 一致、13/13 通过、`FALSIFY` 计数 0。

### C. 接线与验证
CI `integration` 作业新增一步；本机门禁新增一条 entry；`check_ci_wiring` exit 0（工件 24 → **25**）。**门禁 58 项全绿、`OVERALL=GREEN`、exit 0**。记录：本节 + `INDEX.md` 第九十三轮。**本轮无产品代码改动**（探针已还原）。

---

## 2ca. 第九十四轮：`limit_roles` **执行**首次黑盒跑通 —— 并修掉一个**文档承诺、代码没做**的 `Retry-After`（产品改动），顺带产出一份 **D-11** 的实测证据

`ops.md` §4 把整套配额功能写清楚了：角色在 `limit_roles` 表里、`matching_*` 任一为 NULL 即"该维度全匹配"、`limit_count`/`limit_token` 上限、`window` = m/h/d、"**matcher 选出角色后，最严格的那个生效**"；§4.2 还承诺 **"A `429` returns `Retry-After` reflecting the remainder of the current window"**；`limit-token` 是"下一次请求"语义；`enabled=false` 软禁用；计数器是每进程的（v1 限制）。CLI 那轮（第八十二轮）只验证过 `limit-roles` 的**实体 CRUD**，**执行侧从未跑过**。

新增 `integration/test_limit_roles_enforcement.py`（**15 条断言**，接进 CI `integration` 作业与本机门禁；仓外测试工件 25 → 26）。

### A. ★ 真缺陷（文档承诺、代码没做）：`limit_roles` 的 429 **没有 `Retry-After`**
第一版 drill 的 L1 直接红：`Retry-After=None`。查代码（`proxy.rs:848`、`:865`）发现两处配额拒绝都走的是 **`short_circuit(session, 429, "rate_limited")`** —— 而 `short_circuit` 只写 body，**不写任何响应头**。而同一份 `ops.md` §4.2 明确承诺这个 429 会带 `Retry-After`（"反映当前窗口的剩余"），且**同文件另一处**（`provider_rate_limited` 分支，`proxy.rs:1270-1285`）**是带 `Retry-After` 的** —— 也就是说：照文档实现的客户端在这个 429 上**没有任何可睡的时长**，会立刻重试、再撞一次 429（正是窗口想阻止的惊群）。
**修法（产品改动，已实现）**：
1. `crates/hydra-core/src/limit.rs` 新增 `SlidingWindow::retry_after(now)` —— "最老的活样本（count 或 token 两条队列里取更老的）还要多久才滑出窗口"，即文档说的"当前窗口剩余"；空窗口返回 `None`。
2. `proxy/limiter.rs`：`CountVerdict::Denied` 增加 `retry_after: Option<Duration>`，**两个门**（count 与 tokens）在拒绝处就地算出来（`limit_count = 0` 的无条件拒绝没有窗口 ⇒ `None`）。
3. `proxy.rs` 新增 `short_circuit_rate_limited(...)`：写 `Content-Type` + **`Retry-After`**（向上取整到秒、**永不为 0** —— `Retry-After: 0` 等于邀请立刻重试）+ `X-Hydra-Trace-Id`，两处配额 429 都改用它。
实测：**`Retry-After: 59`**（60 秒窗口、刚过第一个样本）✓，token 门的 429 同样带上。
**同时**纠正一处会害人的文档空洞：`hydra_limit_rejected_total` 的 `dim` 标签**真实值是 `count` 与 `tokens`（复数）**，此前文档只写"dim"没写取值 —— 已把取值写进 `metrics.rs` 的模块指标表与函数文档（**按 `dim="token"` 写的告警规则永远不会触发**）。

### B. 其余实测（全部通过）
| 腿 | 实测 |
|---|---|
| L1 | `limit_count=2` ⇒ `200 200 429`；体为 `rate_limited`；**`Retry-After: 59`**；`hydra_limit_rejected_total{tenant="t1",role="r-count",dim="count"} 1` |
| L2 | 再叠加一个更紧的角色（`limit_count=1`）⇒ 下一次请求**立刻** 429（"最严格者生效"） |
| L3 | 只匹配 `t1` 的角色**不影响 t2**；只匹配模型 `echo` 的角色对模型 `other` **不生效** |
| L4 | `enabled=false` ⇒ 三次全过，且该角色**仍在列表里**（软禁用而非删除） |
| L5 | `limit_token = 一次响应用量+1` ⇒ `200 200 429`（**"下一次请求"语义**）；`dim="tokens"`；token 门的 429 **也带 `Retry-After`** |
| **L6（D-11 证据）** | 只写 `matching_provider="p1"` 的角色**完全不执行**（三次全过），节点日志确有 `WARN … limit_role 'r-prov' declares matching_provider 'p1', which CANNOT match: the …` ⇒ **D-11 的前提被实测钉住**（前置门禁在**路由之前**跑，那时还没有选中的 provider） |

### C. 反向证伪（真树、`cp` 还原 + 重构建）
把 `short_circuit_rate_limited` 里的 `Retry-After` 插入去掉（即恢复"修复前"行为）⇒ **2 条**断言变红（L1 的 `Retry-After=None`、L5 的 token 门同项）。恢复后 `cmp` 一致、15/15 通过、`FALSIFY` 计数 0。单测侧也补了：`limiter.rs` 文件内 4 处 `CountVerdict::Denied` 断言改成**检查 `retry_after` 的具体值**（计数拒绝 ≈60s、`limit_count=0` ⇒ `None`、token 拒绝 = 60s），这样"头里有数字"变成了"数字是对的"。

### D. ★ 第一次跑门禁 **红了两类**，都是我这次改动引入的（都已修）
这一轮的门禁**首次以 RED 收场**，`OVERALL=RED`、5 条 entry 变红。逐条查下去，两个原因都是**我自己**造成的，而且都属于"只验证了我改的那一条路径"：
1. **集群特性集编译不过**（`auth cache layers` / `cluster HA` / 三个 SDK 集群腿 / Go 腿 exit=2 全部因此变红）：我改的 `CountVerdict::Denied` 新增了必填字段，而 **`redis/rate_limit.rs`（Redis 版限流器，只在 `cluster-redis` 下编译）** 里还有 3 处构造点 → `E0063`。我本地只构建了 `--features server`。**修法**：那 3 处补上 `retry_after` —— `limit_count == 0` 的无条件拒绝无窗口 ⇒ `None`；两处 Redis 脚本拒绝 ⇒ **整个窗口长度作为上界**（Redis 里的样本没法像进程内那样读出"最老样本的年龄"，给上界是**故意保守**的：`Retry-After` 绝不能邀请比窗口更早的重试）。**教训**：本会话早有的"门禁要跑全部特性组合"这条纪律，我在**改共享类型**时又忘了一次；改公共类型必须把 CI/gate 编译的每个组合都构建一遍。
2. **`tenant error codes`（第八十八轮那套守卫）报了假警**：`rate_limited` 被判"文档 429 / 代码 400"。查下去发现是**我第八十八轮埋的**一处全局兜底：处理 `BoundError` 映射函数时，我写了"在**所有源文件**里找 `respond_error(…, N, x.code())`"，于是 `time_bound.rs` 的 `e.code()` 调用点（400）被算到了**任何**走到这个分支的 code 头上。当时 33 个码全绿**纯属巧合**（受影响的码恰好都是 400）；这次 `rate_limited` 的**首次出现位置**移进了 `proxy.rs`，就把它照出来了。**修法**：把兜底**限定在真正定义 `fn code(&self) -> &'static str` 的那个文件**（否则老实报 UNVERIFIED），并新增两条**配对**形态 —— ①字面量是 `short_circuit_rate_limited(…, "rate_limited", …)` 的实参 ⇒ 顺着该函数体里的 `ResponseHeader::build(429…)` 取状态；②字面量在 `serde_json::json!` 手写 body 里 ⇒ 在附近找真正发送它的 `respond_json*(…, 429, …)`。结果：**30 个码与代码一致、3 个（`invalid_since`/`invalid_until`/`window_too_large`）诚实标为 UNVERIFIED** —— **覆盖率从 33 降到 30，但那 3 个原本是"借了别人的状态码"的假绿**。这正是本会话反复出现的形状：**守卫先被证伪，才有资格守别人**；而"全绿"如果是巧合，它就会在下一次无关改动里变成假警。

### E. 接线与验证
CI `integration` 作业新增一步；本机门禁新增一条 entry；`check_ci_wiring` exit 0（工件 25 → **26**）。`cargo test -p hydra-core --lib limit` ⇒ 2 passed；`-p hydra-server --lib` ⇒ **190 passed**；**`--features server,cluster-redis,usage-clickhouse --no-fail-fast` ⇒ 42 个测试二进制全过、0 failed**（这一条是上面第 1 点修复的验证）；`tenant error codes` 检查器 exit 0（30/3）；`clippy --workspace --all-targets`（含 cluster/usage 组合）`-D warnings` clean；`fmt --check` clean。修复后重跑门禁 **59 项全绿、`OVERALL=GREEN`、exit 0**。记录：本节 + `INDEX.md` 第九十四轮。

---

## 2cb. 第九十五轮：数据面**模型目录**（`GET /v1/models`）首次实测 —— 含一条**安全相关**承诺（"出示的 key 不走外部鉴权、只能收窄"）与**两种**收窄机制的区别

`dev-docs/design.md:632` 与 `design-tenant-model-catalog.md` 描述的目录语义很不寻常，且其中一条是安全相关的：**"`GET /v1/models` 不再直通 —— **无条件公开**在本地聚合应答该租户可调用模型目录（跨授权 provider 并集 × 租户模型白名单 × 在线 provider 过滤，OpenAI 兼容形状；带不带 api-key 均可读 —— 2026-09-09 起出示的 key **不再走外部鉴权**，仅按前缀绑定收窄目录；聊天等调用仍须 api-key）"**；代码注释还补了"**完全本地**（不读 body、不过限流门、不路由、不拨上游、不产生用量记录）、只拦截 `GET` 且路径精确匹配、绑定视图 ⊆ 匿名并集"。此前**从未黑盒验证**。

新增 `integration/test_model_catalog.py`（**17 条断言**，接进 CI `integration` 作业与本机门禁；仓外测试工件 26 → 27），用一个**会计数**的 mock 上游，把"本地应答"变成数字。

| 腿 | 实测 |
|---|---|
| C1/C7 | `GET /v1/models` ⇒ 200、`{"object":"list","data":[{"id":…,"object":"model"}]}`、**upstream chat 调用数 0 → 0**（本地应答）、响应带 `X-Hydra-Trace-Id` |
| C2 | **不带任何 api-key** 也 200，目录与带 key 时**完全相同**（文档的"无条件公开"） |
| **C3（安全）** | **无效 key** 照样 200、目录不变，且**鉴权上游调用数 0 → 0** ⇒ "出示的 key 不再走外部鉴权"成立（它只用于前缀收窄） |
| C5 | 租户白名单外的模型（`m2`，属于已授权 provider）**不列出** |
| C6（机制一） | **operator `provider-key-bindings`**（`OPB_` → p1）⇒ 整个目录收窄成 `['m1']`（`m3` 只由 p2 服务） |
| C6（机制二） | **子租户**（`SUB_` 前缀，只给 `m1` 配了路由）⇒ `m1` 保留（按其路由收窄），而**它没有配路由的 `m3` 不受该绑定限制**（文档的"路由是覆盖、否则走租户默认"），且**两种绑定都不可能把匿名并集撑大** |
| **C4** | **在线过滤**用"状态迁移"测：provider 未失败时 `m3` **在**目录里 → 打 6 次把它打进熔断 dead-set（先断言 dead-set 里确有 p2）→ 目录变成 `['m1']`（`m3` 消失） |
| C8 | `POST /v1/models` **不是**目录（走正常管线），`HEAD` 也不是（501） |

### A. 本轮我自己踩的三个坑（两个是"断言消失/探针没生效"，都很值得记）
1. **`updated_at` 漏了 ⇒ 收窄那条腿是空的**：第一次创建 operator binding 时我照 CLI 的字段列表写了 `created_at` 却漏了 `updated_at`，admin 的"整记录 PUT/POST"直接 400 ⇒ 绑定根本没建，于是"收窄"断言在**空绑定**上比较（看着通过其实什么都没测）。修法：**断言前置条件**（创建返回 201 且 `GET /provider-key-bindings` 里能看到该前缀）——这条 precondition 断言就是它自己抓出来的（同一个"整记录必须带齐字段"的契约，第八十二轮在 CLI 上已经踩过一次）。
2. **★ 我的"证伪探针"两次都没生效，最后发现是两个不同的原因**：
   - 先说第一次：我改 `hydra-core/src/router.rs` 后**立刻** `cargo build`，输出只有 `Compiling hydra-server` —— **cargo 的 mtime 分辨率让 core 的重编译被跳过**（`sleep 2` 后同样的命令就出现 `Compiling hydra-core`）。也就是说那次"证伪仍然全绿"**测的是旧二进制**。教训：**改完源码要确认目标 crate 真的被重编译了**（看 `Compiling <crate>` 行），必要时 `sleep 1~2` 再用。
   - 第二次（重建确认后仍全绿）才暴露真问题：**drill 里根本没有 C4 那段**。我之前用"切片重排 + 整块替换"把 C6 挪到 C4 前面，随后替换 C6 区块时把夹在中间的 **C4 整段删掉了**，而套件照样打印 `PASSED` —— **少一条断言是不可见的**。已把 C4 重新加回并在此记录：**断言被删掉时，套件不会报错**（这是"守卫静默变小"的又一变体）。
3. 顺带确认：C6 的两次预期也错过一次（子租户只配了 `m1` 的路由，我却按"整目录收窄成 m1"断言）⇒ 读 `router::accessible_models` 的 (3.5)/(3.6) 两段代码后才写对：**operator 绑定按 provider 收窄全部模型；子租户路由是"逐模型覆盖"，没有路由的模型不受限**。

### B. 有效的反向证伪（真树、`cp` 还原 + `sleep 2` 重建）
把 `accessible_models` 第 (4) 步的 `breaker.is_dead(pid)` 去掉（即关掉在线过滤）⇒ 重建**确认 `Compiling hydra-core`** 后：**只有 C4 那一条**变红（`ids=['m1','m3']` —— 死掉的 provider 的模型仍在目录里），其余 16 条照过（正确形状：只有 C4 依赖这个过滤）。恢复后 `cmp` 一致、17/17 通过、`FALSIFY` 计数 0。

### C. 接线与验证
CI `integration` 作业新增一步；本机门禁新增一条 entry；`check_ci_wiring` exit 0（工件 26 → **27**）。**门禁 60 项全绿、`OVERALL=GREEN`、exit 0**。记录：本节 + `INDEX.md` 第九十五轮。**本轮无产品代码改动**（探针已还原）。

---

## 2cc. 第九十六轮：**子租户 / 前缀绑定**在聊天路径上的引导（steering）首次实测 —— 含 §7.6.2 那条**安全边界**"删除或停用子租户 ≠ 吊销"

第九十五轮只在**目录**一侧测了两种收窄机制；本轮把 **`/v1/chat/completions` 的引导**跑通。被写的承诺来自 `design-sub-tenant.md` §4.1 与租户契约 §7.6：
- 子租户按 `key_prefix` 命中，其路由是**逐模型覆盖**（`model-specific > default`），**没有配路由的模型回落到租户自己的路由**（"routes are overrides; otherwise the tenant default applies"）；
- **operator `provider-key-bindings`** 按前缀命中且**更强势**：把候选集限制到该 provider（fail-closed），命中时**跳过子租户门**（ruling a′）；
- **§7.6.2（安全边界）**：**"停用 / 删除子租户 ≠ 吊销"** —— 那些 key **照常走默认管线**；吊销 key **永远**是租户 `auth_url` 的职责。

新增 `integration/test_sub_tenant_steering.py`（**8 条断言**，接进 CI `integration` 作业与本机门禁；仓外测试工件 27 → 28）。**关键手法**：两个 mock 上游分别回 `chatcmpl-A` / `chatcmpl-B` ⇒ **"是哪个 provider 服务的"变成可观测量**，而不是猜。

| 腿 | 实测 |
|---|---|
| S1 | 普通 key 请求 `shared` ⇒ 200，由 A 或 B 服务（基线） |
| **S2** | 建子租户 `SUB_` 并给 `shared` 配路由 → **p-b** ⇒ 同一 key 连发 4 次**全是 B**（引导生效，SWRR 被覆盖） |
| S3 | 该子租户**没有配路由**的模型 `onlya` ⇒ **回落默认**（只有 A 提供它，实测 tag=A） |
| **S6** | 建 operator 绑定 `OPB_` → p-a ⇒ 连发 4 次**全是 A**（尽管 `shared` 也由 B 提供）⇒ 绑定**确实更强势** |
| S7 | 未授权模型 ⇒ **403 `model_not_allowed`**（与前缀无关，`model ∩ 授权 provider` backstop 生效） |
| **S4** | **删掉子租户**后同一 key 仍 200（走默认管线）⇒ **"删除 ≠ 吊销"**成立 |
| S5 | **停用**子租户（`enabled=false`）后：**对照组**证明默认管线在等权重下**交替**（`plain=['A','B','A','B','A','B']`），而该 key **没有被钉在**子租户路由上（`off=['A','B','A','B','A','B']`）⇒ 停用既不引导也不吊销 |

### A. 我自己在本轮修掉的两个夹具问题
1. `must()` 只接受 200/201，而子租户 **DELETE 的正确答复是 204**（文档写明）⇒ drill 在最后一段 `CANNOT VERIFY` 中止。已接受 204（并把注释写在判定处）。
2. S5 的第一版断言写成 `… and tag_off != "B" or tag_off == "B"` —— **恒真的废话**。改成"用对照组证明默认管线交替，再证明被停用绑定的 key 同样交替（没有被钉住）"，从**一个恒真断言**变成**两条有信息的断言**。

### B. 反向证伪（真树、`cp` 还原 + `sleep 2` 重建 —— 上一轮刚学到的 mtime 坑）
把 `match_sub_tenant_route` 改成**恒返回 `None`**（即关掉聊天路径的子租户引导）⇒ 重建**确认 `Compiling hydra-core`** 后：**只有 S2** 变红（`tags=['A','B','A','B']` —— 不再被钉在 B），其余 7 条照过（S3/S4/S5 本来就测"不该被引导"的情形，S6 走 operator 绑定、S7 走 backstop）。恢复后 `cmp` 一致、8/8 通过、`FALSIFY` 计数 0。

### C. 接线与验证
CI `integration` 作业新增一步；本机门禁新增一条 entry；`check_ci_wiring` exit 0（工件 27 → **28**）。**门禁 61 项全绿、`OVERALL=GREEN`、exit 0**。记录：本节 + `INDEX.md` 第九十六轮。**本轮无产品代码改动**（探针已还原）。

---

## 2cd. 第九十七轮：ClickHouse sink 的**线上报文**（wire format）首次被观测 —— 修掉一个**产品缺陷**（URL 里的 path），并纠正我自己写错的断言（`param_*` 并非写入路径的形态）

前几轮把**读**路径（`usage_query.rs`）与 **CH 的 URL 解析**测过，但 `ops.md` §1.1 对 `HYDRA_CLICKHOUSE_URL` 承诺的三件事（userinfo ⇒ Basic 认证、query string 原样透传、sink 到底发什么）**从来没有人在线上看过**：`clickhouse.rs:186-189` 的 `send` 文档说 `params` 是 `param_*` 绑定，而写入路径的 `insert_batch_clickhouse_http`（`sink.rs:711` 起）传的是 `&[]`。新增 `integration/test_clickhouse_sink_wire.py`（**16 条断言**，mock ClickHouse，不碰真数据库；接进 CI `integration` + 本机门禁，仓外工件 28 → **29**）。

### A. 实测到的报文形态（这是本轮的主要事实产出）
| 观测 | 实测值 |
|---|---|
| 请求行 | `POST /?database=dogress&query=INSERT%20INTO%20usage_record%20(…)%20SETTINGS%20insert_deduplication_token='1377657-18d9e28aae48033f-0'%20FORMAT%20JSONEachRow` |
| 认证 | `Authorization: Basic Y2h1c2VyOmNocGFzcw==` = base64(`chuser:chpass`)，**且**来自 URL userinfo |
| 透传 | `database=dogress` 原样保留（`?user=&password=` 形态则**不**伪造 `Authorization`） |
| **行数据** | **在请求 BODY 里**，一行一个 JSON 对象：`{"tenant_id":"t1","provider_id":"p1","model_key":"echo","client_api_key":"sk*******-1","sub_tenant_id":null,"status_code":200,"tokens_in":5,"tokens_out":8,…}` —— **不是 `param_*`** |
| 敏感字段 | `client_api_key` 是**掩码**（`mask_key` 对 11 字符 key 保留首 2 末 2），原始 key `sk-tenant-1` **不在**报文里 |

⇒ 我最初的断言写成"行数据以 `param_*` 传输"（照抄了 `send` 的文档措辞），**第一次运行就红**。修法是**改为实测形态并把这种形态钉住**（同时断言"没有 `param_*`"），而不是把断言删掉——并把 `clickhouse.rs:217` 那行请求行注释改成 `[&param_k=v …]` + 说明"写入路径不传 params，绑定是**读**路径的形态"。`ops.md` §1.1 补上这段实测报文（含 `sk*******-1` 掩码事实）。

### B. 本轮发现并修掉的**产品缺陷**：URL 里的 path
`parse_clickhouse_url` 注释声称"and any path"被剥掉，实际用的是 `trim_end_matches('/')`：`http://host:8123/clickhouse` 只去掉尾斜杠，**path 还粘在 port 上** ⇒ 连接阶段就失败，日志 `clickhouse connect 127.0.0.1:18897/clickhouse: invalid port value`（一个指向"网络/端口"的错误结论）。改为 `host_port.split('/').next()`，并新增单元测试 `parse_url_trims_a_path`（`#[cfg(feature="usage-clickhouse")]`）。W3b 腿在**线上**复验：现在 `path=/?database=dogress&query=…`。

### C. 夹具（harness）自身的两个问题 —— 都是"绿/红会骗人"的那一类
1. **W3b 与 W3 共用 mock 端口**，而 `ThreadingHTTPServer.shutdown()` **只停循环、不释放监听套接字** ⇒ 第二次 bind 抛 `OSError: Address already in use`，**W3、W4 从未运行过**（前一轮的"未复验"其实就是这个）。改为**每腿独立端口**（18897/18896/18895/18894）+ `close_server()`（`shutdown()` + `server_close()`）。
2. **证据行指错**：W4 用 `next(l for l in log if "usage sink" in l)` 取证据，取到的是 `INFO usage sink built kind=clickhouse`（与断言字符串无关的那一行）。改为取**包含断言字符串的那一行**，现在打印 `WARN hydra_server::sink: usage sink batch insert failed; will retry error=clickhouse insert rejected: status=\`HTTP/1.1 500 Internal Server Error\``。W3b 的 else 分支也不再只说"没到 mock"，而是回显日志里最后一条 clickhouse 行。

### D. 反向证伪（真树、`cp` 还原 + `cmp` 一致 + `sleep 2` 重建并确认 `Compiling hydra-server`）
一次植入两个互不相干的探针，按**标签**归因：
1. `sink.rs` 把 `tokens_in` 写成 JSON **字符串**（`"5"`）⇒ **W2 数字断言变红**（`tokens_in='5'`）。这条有牙齿很重要：`JSONEachRow` 对 UInt64 列拒绝 `"5"` ⇒ 真回归是**INSERT 被拒**，而不是静默 0。
2. `clickhouse.rs` 把 path 裁剪还原成 `trim_end_matches('/')` ⇒ **W3b 变红**（`the sink never reached the mock`）。

结果：`exit=1`、失败清单**恰好**是这两条，其余 14 条照过 ⇒ 断言可归因、非恒真。`cp` 还原后 `cmp` 与备份**逐字节一致**，重建（确认 `Compiling`）后 16/16 通过、`exit=0`。全仓 `grep -rn FALSIFY crates/ integration/ scripts/` = 0。

### E. 接线与验证
CI `integration` 作业新增一步（**该作业的二进制只用 `server` 编译**，而 sink 需要 `usage-clickhouse`；`server`-only 二进制会让 `build_sink` 在启动时失败、drill 报 exit 2 ⇒ 该步内先按生产特性集 `server,cluster-redis,usage-clickhouse` 构建）；本机门禁新增一条 entry；`check_ci_wiring` exit 0（28 → 29）。**产品改动**：`crates/hydra-server/src/clickhouse.rs`（path 裁剪 + 注释）、`dev-docs/ops.md` §1.1。记录：本节 + `INDEX.md` 第九十七轮。

**★门禁第一次跑是 RED，唯一红的是 `fmt --check`，而它是我这轮改 Rust 后没跑 `cargo fmt --check` 造成的**：我把 `host_port` 的 `.split('/').next()` 链和新增单测的 `assert_eq!` 手写成了多行，rustfmt 要的是它们各自的单行/换行形态。**教训（本会话第一次由 fmt 抓到）**：改完 Rust **先 `cargo fmt --check` 再谈门禁**——`cargo build` 与 16/16 全绿的 drill 都不会因为格式而不通过，**构建与测试全绿完全掩盖了格式红**，这也是"你验证过的东西 ≠ 你交付的东西"的又一形态。`cargo fmt` 只动了这一个文件（`find -newermt` 确认），重建（确认 `Compiling hydra-server`）后 drill 仍 16/16，重跑门禁 **62 项全绿、`OVERALL=GREEN`、exit 0**（core 253 + server 549 = 802 与 `docs/index.html` 宣称一致；新增的那条单测在 `usage-clickhouse` 特性集里，故不计入 802）。

---
## 2ce. 第九十八轮：ClickHouse 传输的**另一半** —— 租户 API 的用量**读**路径（§5.3）首次在线上被观测：**"字符串编码的计数器"这条陷阱真被绕过**，四种"读不到"各自被归因

第九十七轮看的是**写**（sink 的 INSERT 报文）；本轮看**读**。`tenant-api-integration.md` §5.3 对集群部署的 `GET /tenant/{tid}/api/v1/usage` 写了完整契约（窗口与归一化 / `as_of` / 分组 / 四种错误），它的事实基础来自 `design-tenant-api.md` §4.3.3 **手工测过一次**的 ClickHouse 24.3 实测（64 位整数被序列化成 JSON **字符串**、空集 `MAX(created_at)` 返回 `""` 而非 null、`UNION ALL` 分支顺序不稳定……），而之后的守卫只有 `#[ignore]` 的、**需要活库**的测试 —— 读路径在 HTTP 层的**线上行为从未被观测过**。这正是本能力要防的那类失败：**语法正确、语义错误的 200**（把字符串计数器读成 0 用量，回报一个看起来诚实的空窗口）。新增 `integration/test_usage_query_wire.py`（**33 条断言**，mock ClickHouse 当存储、不碰真库；接进 CI `integration` + 本机门禁；仓外工件 29 → **30**）。

### A. 实测到的事实（本轮产出）
| 观测 | 实测 |
|---|---|
| **字符串计数器** | mock 按 CH 实测形态答 `{"requests":"3","tokens_in":"100",…}` ⇒ 响应 `{"requests":3,"tokens_in":100,"tokens_out":40,"cache_hit_tokens":10,"errors":1}` —— **真数字，不是 0 窗口** |
| 数字形态 | 同一路径答 JSON **number** 也接受（`parse_lenient_u64` 两种都收）⇒ 存储侧格式变化不会静默归零 |
| `as_of` / 元数据 | 等于窗口内 `MAX(created_at)`；`source: "clickhouse"`、`group_by: "none"`、`rows: []`、窗口按归一化回显，均与文档一致 |
| 绑定 | `WHERE tenant_id = {t:String} … created_at >= {s:String} … < {e:String}`，线上带 `param_t=t1`、`param_s`、`param_e`，且**SQL 文本里没有租户 id**（绑定而非拼接） |
| 分组 | `group_by=model` 是**第二条查询**（`AS key` + `GROUP BY key ORDER BY key` + `count() AS requests`），totals 仍是**总合计**而非第一组行（`UNION ALL` 顺序陷阱的反面） |
| `group_by=sub_tenant` | 用 `coalesce(sub_tenant_id, '')`，未命中前缀的用量以文档写明的**空字符串分组**返回（实测 `keys=['', 'sub-1']`） |
| `group_by=day` | 用 `substr(created_at, 1, 10)`（不引入日期函数 ⇒ 主键裁剪仍在） |
| 空窗口 | CH 答 `{"last_seen":""}` ⇒ 响应 `as_of: null`（**不是空字符串**）、totals 全 0、仍是 200 |
| 读不到 | 四种形态**全部 503 `usage_store_unavailable`、绝不返回零值 200**，并按 `hydra_tenant_api_usage_query_total{result=…}` 分别归因为 `decode_error`（缺 `last_seen` 的形状漂移 / 非 JSON）、`store_unavailable`（CH 5xx）、`result_too_large`（超 64 KiB 响应上限） |
| 报文措辞 | 暂态两类（store/decode）**给租户的 message 逐字相同**（"retry or contact the operator"），而 `result_too_large` **明说重试无用、请缩小窗口** ⇒ 代码里那份"三种失败同码不同言"的意图被实测确认 |
| 计数精度 | 精确计数：`{group_by="none",result="ok"}=3`、`{result="decode_error"}=2` ⇒ 每次读**恰好记一次、只归一类** |
| 读不产生用量 | 全部读操作期间 **INSERT 数 = 0**（§5.3 第 4 条语义边界） |
| 读自己的期限 | 第二个节点 `HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS=1200` + mock 挂 5s ⇒ **1.20s** 返回 503，日志 `clickhouse read: timed out after 1200ms`；而**写**侧默认 15000ms ⇒ 读路径确实有**独立**期限 |

### B. 我自己的两个夹具错误（都当场抓住）
1. **U12 第一版把"没有误报"写成了"样本存在"**：我在失败腿之前读 `decode_error`，断言"样本存在且为 0" —— 而健康状态恰恰是**该样本根本不存在**，于是 33 条里唯一红的就是我自己这条（`sample present=False`）。改成两条有信息的断言：①**六次成功读之后** `decode_error` 总量仍为 0（不误报方向）；②**精确计数**（ok=3 / decode=2 ⇒ 每次读只记一次、只归一类）。
2. **mock 从 SQL 文本反推分组答案，于是证伪的"红"理由有一部分来自夹具**：把 `coalesce(sub_tenant_id,'')` 换成裸列后，第一版 mock 返回**模型**分组 ⇒ 第二条 U5 断言是因为"mock 没按 SQL 答"而红。改为**按真实 CH 行为答**（裸 `sub_tenant_id` 的 SQL NULL 变成 JSON `null`，而解码器要求字符串 `key`）⇒ 证伪时该腿红了**正确的理由**：**HTTP 503**（整个窗口因一行未归因的用量而解码失败），正是 `usage_query.rs:322-325` 注释里写下的后果。

### C. 反向证伪（跨两个 crate 各一枚探针；`cp` 还原 + `cmp` 一致 + `sleep 2` 重建并确认 `Compiling hydra-core`/`Compiling hydra-server`）
1. **F1 `hydra-core`**：`normalize_as_of` 去掉 `Some("") => None`（保留空串）⇒ **只有 U7 变红**，实测 `as_of=''`（文档点名的"空字符串 as_of 看起来像真值"）。
2. **F2 `hydra-server`**：`ch_group_expr(SubTenant)` 改用裸 `sub_tenant_id` ⇒ **U5 两条 + U12 两条变红**（U5 第二条实测 **HTTP 503**；U12 的精确计数变成 `decode_error=3`、且"六次成功读之后"出现一条失败样本）—— 即**精确计数那条断言是一道交叉检查**：被测行为漂移会同时在**归因**上被看见。

共 5 条红标签、**全部可归因**（F1→U7；F2→U5×2+U12×2），其余 28 条照过。`cp` 还原后 `cmp` 逐字节一致、重建后 **33/33 通过、exit=0**；全仓 `grep -rn FALSIFY crates/ integration/ scripts/` = 0。**本轮无产品代码改动**（两枚探针均已还原）。

### D. 接线与验证
CI `integration` 作业新增一步（复用上一步刚构建的特性集二进制：同一作业的 `server`-only 二进制**不能**承载本 drill）；本机门禁新增 1 条 entry；`check_ci_wiring` exit 0（29 → **30**）。**门禁 63 项全绿、`OVERALL=GREEN`、exit 0**（新增 entry `tenant usage read over CH exit=0`；core / server 两份转写里 `test result: FAILED` 均为 0；`fmt --check` 通过——上一轮那条教训已生效）。记录：本节 + `INDEX.md` 第九十八轮。

---

## 2cf. 第九十九轮：集群**共享限流**首次在真集群上实测 —— 顺带挖出一个 **P1**：Redis 连接断了以后**永远不会重连**（fred `Has policy: false`），降级的 leader 与 fail-open 的限流**只能靠重启恢复**

第九十七/九十八轮看的是 ClickHouse 传输的写与读；本轮转到 **Redis 集群面**。`ops.md` §12 写着集群模式提供"multi-instance with **shared rate-limit counters**"，§13.4/`redis/rate_limit.rs` 写着限流是**硬编码 fail-open**（"there is NO env override"），§13.5 与 `cluster.md` §3 写着"数据面不受影响、选举 fail-closed、**无切换直至 Redis 恢复**"—— 这些行为**从未在真集群上跑过**：限流只在单机 drill 里测过（进程内窗口），Redis 限流器只有进程内单测。新增 `integration/test_cluster_limits.py`（**18 条断言**，真 leader + edge + 真 Redis，且 Redis 走**可切断的 TCP 中继**；接进 CI `live-deps` 与本机门禁；仓外工件 30 → **31**）。

### A. 实测到的（正向）
| 腿 | 实测 |
|---|---|
| C1 | 三个角色**一次 reload 同一版本**下发；用 `limit_count = 0`（**无条件拒绝、Redis 调用之前就决定**）当**零副作用收敛探针**：edge 首个 429 即证明它已持有**整个版本**（用 t1 去探会污染被测窗口）；且该角色**没有**在 Redis 建任何键 |
| C2 | **共享计数窗口**：leader 2 次 + edge 1 次填满 `limit_count = 3` ⇒ 之后**两个节点都 429**；Redis 里恰好 3 个成员、**两个不同的 instance 前缀**（成员带 per-instance nonce ⇒ 不会像旧版那样在同毫秒折叠成一条而**少计**，即 fail-open 方向）；`Retry-After: 60`（Redis 限流器给不出最老样本年龄 ⇒ 用整个窗口做上界，刻意保守） |
| C3 | **共享 token 窗口**：`limit_token = 10` < 一次请求的 13 token ⇒ leader 放行（记账）后 **edge 的下一次请求 429**（跨进程的"下一次请求"语义），指标标签为真实值 `dim="tokens"` |
| C4 | **fail-open**：窗口已满时切断中继 ⇒ 请求重新被放行（`[200,200,200]`），日志 `redis rate-limit check failed; failing open`，`hydra_control_poll_total{result="rate_limit_error"}` 上升，且**时间有界**（首个请求 0.50s = 命令超时，数据面不会挂死） |

### B. ★P1：连接被切断后**永不重连**（本轮最重要的产出）
C5 原意只是"恢复后限流重新生效"，结果**一直是红的**：切断 ~2s 再恢复后，**90 秒内节点新建 Redis 连接数 = 0**，每条命令都撞 500ms 命令超时并 fail-open（`rate_limit_error` 从 2 涨到 46 而我在 88.7s 后放弃），**限流永久失效**。停止猜测、改用**只读观测**定位：
1. 先证明**不是我的夹具**——独立客户端经同一中继 `PING` 立刻 `+PONG`（`C5 the relay serves again` 那条断言现在常驻 drill）；
2. 独立探针（`.acceptance/round99/reconnect-probe.py`）**不让任何其它客户端**碰中继，于是计数到的每一次连接都是节点自己的：**`new-conns=0`，`/healthz/leader` 连续 90s 503**（租约永远拿不回来 ⇒ **集群长期没有 leader**），日志每秒刷 `Timeout Error: Request timed out`；
3. 打开 `RUST_LOG=warn,fred=debug` 让 fred 自己说话：`Connection closed` → `Resetting connection` → **`Checking reconnect state. Has policy: false`** —— 根因确定：**fred 没拿到重连策略**。而策略**不是 `Config` 的字段**，是 `Pool::new(config, perf, connection, policy, size)` 的**独立参数**，`redis/mod.rs` 传的是 **`None`**（`ReconnectPolicy` 的 `max_attempts = 0` 语义本来就是"永久重试"）。
   ⇒ 后果：`ops.md` §13.5 承诺的"直至 Redis 恢复"**不成立**；限流 fail-open、认证 L2、熔断同步、注册表续约**全部**从"临时降级"变成"**永久损坏，只能重启**"。

### C. 产品修复（本轮改动）
`crates/hydra-server/src/redis/mod.rs`：新增 `pub const RECONNECT_DELAY_MS: u32 = 1_000;` 与 `pub fn reconnect_policy() -> ReconnectPolicy`（`new_constant(0, 1000)` = **永久重试**、fred 自带 jitter），并把建池逻辑抽成可测的 `fn pool_for(config)` 传入 `Some(reconnect_policy())`；文档注释里写实测（0 次重连 / 90s / `Has policy: false`）。**单测** `the_pool_always_gets_a_reconnect_policy_that_never_gives_up`：不连 Redis 就能断言"每个 client 都**带着**策略 + `max_attempts == 0`"（`None` 这种缺陷在 Redis 真的消失之前完全不可见，所以必须钉在**建池**这一层）。

### D. 修复前后（同一棵树、同一个 drill、同一段探针）
| | 修复前 | 修复后 |
|---|---|---|
| 切断期间的重拨 | **0 次 / 90s** | **每 1s 一次**（实测 `refused=22`，2 个 client 各一次/秒） |
| 恢复后 `/healthz/leader` | **503 持续 90s**（探针放弃） | **~3s 内 200**（探针）；drill 的 C5 **首次尝试就 429**（0.0s） |
| 限流 | 永久 fail-open | 只在故障期间 fail-open |

### E. 反向证伪（`cp` 还原 + `cmp` 一致 + `sleep 2` 重建并确认 `Compiling`）
把 `Some(reconnect_policy())` 改回 **`None`**（即修复前的真实代码）⇒ 重建后 drill：**只有 C5 变红**（`last status=200 after 88.7s of probing`）、其余 **17 条照过** ⇒ C5 就是这道缺陷的守卫，且**红标签可归因**。`cp` 还原后 `cmp` 逐字节一致、重建后 **18/18 通过、exit=0**。断言集合与修复前**逐条比对**（diff 只有 C5 的判定翻转、条数 18 对 18）⇒ **没有断言被静默删掉**。`redis::` 单测 21 passed（含新增 1 条；不设 `HYDRA_TEST_REDIS_URL` 时会因 `isolated_pool` 设计而红——门禁已导出该变量）。

### F. 文档与接线
`ops.md` §13.5 增实测对照表（0 次重连 / 90s / 恢复后 3s，并点明"重启曾是唯一恢复手段"）、§9.1 **新增一行告警** `increase(hydra_control_poll_total{result="rate_limit_error"}[10m]) > 0`（"共享限流已关闭"：每个受影响请求每个匹配角色 +1 ⇒ 同时是"多少流量失去保护"的信号）；`cluster.md` §3 给"选举"行加实测修正块；`check_alert_expressions` 由 15 引用/5 选择器变为 **16/6 全解析**；`check_documented_env` exit 0；`check_ci_wiring` exit 0（30 → 31）；CI `live-deps` 新增一步、门禁新增 1 条 entry。**门禁 64 项全绿、`OVERALL=GREEN`、exit 0**（新 entry `cluster rate limits (shared+FO) exit=0`；两份转写 `FAILED` 计数 0；`grep -rn FALSIFY` = 0）。

**★门禁第一次跑 RED，两条红都是我这次改动引入的**：①`clippy optional` 打在 **`assert!(RECONNECT_DELAY_MS > 0)`** 上（`assertions_on_constants`，常量断言要写进 `const {}` 块）⇒ 改为 `const { assert!(RECONNECT_DELAY_MS > 0) };`；②新门禁 entry 写成 `gate "…" HYDRA_TEST_REDIS_URL=... python3 …` ⇒ `gate()` 直接 `"$@"` 执行，把 `VAR=value` 当命令 ⇒ **exit 127**（其余同类 entry 都用 `bash -c`）。两条都改完、`cargo clippy --features server,cluster-redis,usage-clickhouse --all-targets` clean、`fmt --check` clean（上一轮的教训已生效）后重跑即全绿。

---

## 2cg. 第一百轮：把上一轮那个 P1 的**同类缺陷**清干净 —— 全树 **5 个 `Pool::new` 里有 4 个在用 fred 默认值**（含 lib 测试夹具），并加一道**单一所有者**守卫

第九十九轮修掉的是**生产**那一个建池点。收尾时顺着"还有谁在建池"一问，全树 grep 出 **5 处 `Pool::new`**，其中 **4 处**（lib 测试夹具 `test_redis::isolated_pool` + `tests/common/mod.rs` + `tests/tenant_api_cluster.rs` 三处）把 fred 的**三件套全留默认**：`default_command_timeout = 0`（**永久等待**，正是生产注释里点名的"数据面 stall"）、`unresponsive` 看门狗关闭、`policy = None`（**永不重连**）。`tenant_api_cluster.rs` 那处注释甚至写着"fred 默认 `0` 会一直挂到进程被杀"，于是**手工**造了一个 `PerformanceConfig` 绕过去 —— 这是典型的"第二个所有者"气味：**测试池不能超时也不能重连 ⇒ 测试跑的是另一个程序**，而上一轮那个重连修复恰恰只能在这类池上被验证。

### A. 一个所有者（本轮改动）
`crates/hydra-server/src/redis/mod.rs` 现在导出：
- `pub fn build_pool(url, size)` —— 标准三件套（命令超时 500ms、看门狗、**永久重连**策略）；
- `pub fn build_pool_with(url, size, perf)` —— 只允许覆盖**命令超时**（唯一需要按测试变动的旋钮），看门狗与策略仍由本模块拥有；
- `fn pool_from_config(config, perf, size)` —— **全树唯一的 `Pool::new` 调用**（生产 `pool_for` 与上面两个公开入口都汇到这里）。

迁移的 4 处：`test_redis::isolated_pool`（→ `build_pool(&url, 2)`）、`tests/common/mod.rs`（→ `build_pool(&url, 1)`）、`tests/tenant_api_cluster.rs` 两个"死端口"用例（→ `build_pool(&url, 1)`）、以及那个手工 perf 的用例（→ `build_pool_with(..., 200ms)`）。全部**feature 组合**编译通过（`server` / `proxy` / `server,cluster-redis,usage-clickhouse`）。

### B. 守卫：`scripts/check_redis_pool.cjs`（+ 9 条测试）
两条规则：**R1 单一所有者** —— `Pool::new(` 只允许出现在 `redis/mod.rs`（新调用点应当走共享构造器）；**R2 策略不得为 `None`** —— 该文件里每个 `Pool::new` 的**第 4 个实参**必须是 `Some(`。第 4 个实参用"取整个实参列表、按**顶层**逗号切开"的方式定位（跨行 + 嵌套括号，正则会在两处都错），并**跳过注释里提到的构造器**。缺 owner 文件、或扫描到的调用数为 0 ⇒ **exit 2（绝不 0）**。测试用临时树喂真脚本（`CRP_CRATES`/`CRP_OWNER` 覆盖），**两个方向都测**：正确形态 0；owner 里写 `None` ⇒ 1 且点名行号；**别处**出现 `Pool::new` ⇒ 1 且点名文件；缺第 4 实参 ⇒ 1；注释提及 ⇒ 0；无调用 ⇒ 2；缺文件 ⇒ 2；嵌套括号后仍能定位第 4 实参（`Some` ⇒ 0、`None` ⇒ 1）。

### C. 我自己的两个错误（都由"守卫自己先被证伪"暴露）
1. **`pkill -f 'deps/hydra_server-'` 把发起命令的 shell 自己杀了两次**（模式匹配到了我自己的命令行），第一次直接吞掉了本该执行的源码编辑 ⇒ 改成"列出后排除 `$$`/`$PPID` 再杀"。（`pkill -f` 自杀这条坑本轮第二次踩，和第七十八轮"按名字杀进程会杀用户的 dev 栈"同族。）
2. 守卫**第一版把违规藏在 exit 2 后面**：owner 里没有调用时先判 `ownerCalls === 0` ⇒ 一个"别处有调用、owner 没了"的树会报 CANNOT VERIFY 而**不报违规**。改成**先报违规**再判 CANNOT VERIFY；另有测试夹具把**相对** `CRP_OWNER` 配**绝对** `CRP_CRATES`（脚本用 `path.join` 拼接 ⇒ 相对 owner 指向真仓库）——改为 `path.resolve` 并让夹具传绝对路径。

### D. 反向证伪（`cp` 还原 + `cmp` 一致）
把 `tests/common/mod.rs` 改回 `Pool::new(config, None, None, None, 1)` ⇒ 守卫 **exit 1** 并输出 `VIOLATION crates/hydra-server/tests/common/mod.rs:62: \`Pool::new\` outside the single owner …`；`cp` 还原后 `cmp` 一致、守卫 exit 0、守卫测试 9/9 通过。**Rust 侧**：新增 `the_test_harness_builds_the_same_pool_as_production`（用真夹具池断言"每个 client 都带着策略且 `max_attempts == 0`"，这正是修复前为 `None` 的地方）；`redis::tests` 4 passed、`--test tenant_api_cluster` **10 passed**（含两个死端口用例与那个 200ms 短超时用例）。

### E. 接线与验证
CI `scripts` 作业新增两步（守卫 + 守卫测试），本机门禁新增两条 entry；`check_ci_wiring` exit 0。**门禁 66 项全绿、`OVERALL=GREEN`、exit 0**（新 entry `fred pool single owner exit=0`、`fred pool guard tests exit=0`；两份转写 `FAILED` 计数 0；`fmt --check`/`clippy`（含 cluster/usage 特性）均 clean）。

**一个被我自己否掉的测试**（记录下来以免重犯）：我本想用一个"accept 后永不回话"的黑洞 socket 来区分 `0 = 永久等待`，结果该测试**挂死 60s+** —— `Pool` 未 `init()` 时 `pool.get()` 会一直等连接，**命令超时并不适用**。结论：那条断言在这个 API 下构造不出来，删掉，改为"静态规则（唯一调用点 + 第 4 实参必须 Some）+ 已有的 `pool_config_bounds_every_command` 断言非零超时"这条链来守。

---

## 2ch. 第一百零一轮：上游**连接一直建不起来**（死路由）时 —— 请求**不会故障转移**而是 502；新增 connect 界（产品改动）+ 启动期拒绝"无效的界"

第九十九/一百轮在看 Redis 集群面；本轮换到**上游连接**这一侧。`ops.md` §9.1 把 `hydra_upstream_first_byte_timeout_total` 定义为"上游**已经接受了连接**、然后不回应头"，而 `proxy.rs:1206-1247` 依据**同一个区分**决定要不要故障转移：只有**连接错误**能证明"上游根本没看到请求"（`never_reached_upstream = true`），首字节超时意味着"请求已经写出去了"，重放可能**重复计费** ⇒ 客户端拿 **502 `upstream_transport_error`**，**不换 provider**。但 `ProviderClient::send` 把**整个 `req.send()`（含建连）**包在首字节界里，而客户端**从未设置 `connect_timeout`** ⇒ **一个 SYN 被丢掉的路由（安全组改错、AZ 死掉、IP 被黑洞）会被烧掉整个首字节界，然后被当成"发出去之后的失败"**：网关**拒绝**把请求转给旁边那个健康 provider。

### A. 实测（本轮先测后改，红着进来）
用 **TEST-NET-1 `192.0.2.1`（RFC 5737，保留给文档、路由器必丢）** 当黑洞（drill 先自证"这个地址在本机确实是个黑洞"），两个 provider（`echo` 同时挂在健康 mock 与黑洞上，等权 SWRR）：

| 观测 | 修复前 | 修复后 |
|---|---|---|
| 6 次请求的状态码 | **`[502,200,502,200,502,200]`** | **`[200,200,200,200,200,200]`** |
| `hydra_retries_total`（是否故障转移） | **0** | 4 |
| `hydra_upstream_first_byte_timeout_total{provider="p-dead"}` | **4**（语义错的归因） | **0** |
| 每次死路由尝试的耗时 | 3.00s（= 首字节界） | **2.00s**（= connect 界） |
| 客户端拿到的东西 | `{"error":{"message":"upstream_transport_error",…}}` | 正常上游回复 `chatcmpl-live` |

**对照腿（必须保持不变）**：上游**接受连接后一言不发**仍是首字节超时 + **不故障转移**（`codes=[200,502,200,502]`、计数器 +2）—— 即修复没有把两类混为一谈，双计费策略原样保留。

### B. 产品修复
1. `ProviderClient::with_connect_timeout(secs)` → `.connect_timeout(...)`（`new()` 用默认值），把建连阶段变成**普通连接错误** ⇒ 走 `never_reached_upstream = true` 分支 ⇒ **故障转移**；
2. `ProxyConfig` 新增 `upstream_connect_timeout_secs`（默认 **10**）+ 纯函数 `parse_upstream_connect_timeout_secs`（0/垃圾 → 默认）；
3. **启动期校验** `check_upstream_connect_before_first_byte(connect, first_byte)`：connect **必须严格小于**首字节界，否则**拒绝启动**并同时点名两个环境变量（否则这个界永远不生效，症状只是"状态码不对"，运维无从解释）；
4. `main.rs` 读两个变量 + 校验 + 传入 `ProxyConfig`（否则就是 ghost switch）；`proxy.rs` 用配置值构造客户端。
5. 单测：`parse` 全覆盖 + **默认值对必须可用**（`default.connect < default.first_byte` 且 `check` 通过）+ `check` 两个方向（相等/更大 ⇒ Err 且消息含两个变量名，严格小于 ⇒ Ok）。

### C. 反向证伪（`cp` 还原 + `cmp` 一致 + `sleep 2` 重建）
删掉 `.connect_timeout(connect)`（即修复前形态）⇒ drill **5 条红**：U0/U1（`codes=[502,200,502,200,502,200]`）、U2 两条（首字节计数 4 而 retries 0）、U3（`max=3.00s` 撞在首字节界上）。**U3 是我自己修过的弱断言**：第一版写成 `max < first_byte + 2`，在**没有修复**时也通过（3.0 < 5.0）⇒ 改成"必须**早于**首字节界结束"（`< first_byte - 0.5`），修复后 2.00s 通过、去掉修复 3.00s 变红 ⇒ 这条断言现在真的能区分。U4（对照）与 U5（启动拒绝）与客户端改动无关，两个方向都通过 —— 期望形状。

### D. 文档
`ops.md` §1.2 环境表新增 `HYDRA_UPSTREAM_CONNECT_TIMEOUT_SECS`（默认 10、必须严格小于首字节界、给出实测前后对照与"它买到什么"），并把 `HYDRA_UPSTREAM_FIRST_BYTE_TIMEOUT_SECS` 那行**说清它只管响应头**、建连归 connect 界、body 归 idle 界；§9.1 的告警行补上"**连接从未建立是另一类**，会故障转移且**不**计入本计数器"，并记录修复前的实测数字。`check_documented_defaults` 由 18 → **19** 条默认值核对通过，`check_documented_env`/`check_alert_expressions` exit 0。

### E. 接线与验证（含门禁抓到的**我自己造成的**两处）
CI `integration` 作业新增一步（本 drill 不需要 Redis/CH，黑洞地址是保留网段，CI 里同样成立），本机门禁新增 1 条 entry；`check_ci_wiring` exit 0（仓外工件 31 → **32**）。

**★门禁第一次跑 RED，红的是 `public claims`，而它抓的正是我这次改动**：新增 2 条 Rust 单测后 `--features server` 套件变成 **253 + 551 = 804**，而 `docs/index.html` 仍宣称 **802**（含两处"correctness gates"细目）—— 就是"我刚写就过期的公开数字"这一类（§2as 的老账）。按守卫自己给出的正规路径 `node scripts/check_public_claims.cjs --measure --write` 重新测量并回写（804 = 253 core + 551 server），`check_i18n` 0。**第二次门禁 67 项全绿、`OVERALL=GREEN`、exit 0**（`upstream connect bound exit=0`；两份转写 `FAILED` 计数 0；`grep -rn FALSIFY` = 0）。

---

## 2ci. 第一百零二轮：**死路由到底有多贵**（用出厂默认值实测）—— 并验证熔断器确实止血、手动 reset 是真杠杆

第一百零一轮修的是"死路由**不会**故障转移"；修完之后**它开始等**：每次落在死 provider 上的尝试都要等 `HYDRA_UPSTREAM_CONNECT_TIMEOUT_SECS`（**默认 10s**，即运维真正跑的值）。`ops.md` §9.1 声称熔断器是让 provider"悄悄离开轮转"的那个机制（`hydra_candidate_skipped_total{reason="breaker_dead"}`），`breaker_wrap.rs` 在**连续 5 次**失败后置死；但这**两个事实从来没被放在一起量过** —— `test_breaker_lifecycle.py` 用的是**关闭的端口**（失败瞬间返回），`test_upstream_connect_bound.py` 用的是 2s 界且只看 6 个请求、不碰熔断器。新增 `integration/test_dead_route_cost.py`（**9 条断言**，**故意不设任何界**，即用出厂 10s/30s；死路由仍是 TEST-NET-1，drill 先自证该地址在本机是黑洞）。

### A. 实测（这就是"死路由的价目表"）
| 观测 | 实测 |
|---|---|
| 9 个请求的延迟 | `[10.0, 0.0, 10.0, 0.0, 10.0, 0.0, 10.0, 0.0, 10.0]` —— **每次被选中就付一次 10s**，而请求**全部 200**（健康 provider 接住） |
| 进入死集所需失败数 | **恰好 5**（与文档/`design §8.4` 的阈值一致） |
| 触发后 | `dead=['p-dead']`、`hydra_candidate_skipped_total{reason="breaker_dead"}` **4**（每个被跳过的请求 +1），随后 4 个请求**全部 0.0s** ⇒ **惩罚停止** |
| 手动 reset | `DELETE /api/v1/breaker/p-dead` ⇒ `{"was_dead":true,"dead":[]}`；reset 后**没有修路由**的话，10s 惩罚立刻回来（`[(200,0.0),(200,10.0),(200,0.0)]`）⇒ **reset 是诚实杠杆，不是装饰** |
| 只有一个死 provider 的租户 | 首个请求 **502 `all_proxies_failed`，10.0s**（有界，不挂死）；触发后 **503，0.0s** ⇒ 熔断器把"每次 10s"变成"立刻失败" |

⇒ 两 provider 等权轮转下的最坏情形：**前约 9 个请求里约 5 × 10s 的用户可见延迟**被摊掉，之后恢复。这是出厂默认值的真实代价，已写进 `ops.md` §1.2 的环境表行（含上述数字）。

### B. ★我自己的夹具错误（先红后绿，且**红的是夹具不是产品**）
第一版 D1 循环只跑 `THRESHOLD + 3 = 7` 个请求就断言"熔断器已触发" —— 但等权 SWRR 是**交替**的，7 个请求里只有 **4** 次落在死 provider 上（`10.0, 0.0, 10.0, 0.0, 10.0, 0.0, 10.0`），**差一次**才到阈值 ⇒ 五条红里有四条其实是"断言预算不够"。改成"**驱动到死集真的出现为止**（预算 15 个请求）"，并把"触发所需失败数"做成一条**自己的断言**（`tripped_after == 5`）⇒ 立刻全绿，且顺带测到"恰好 5 次"这个数字。（这正是本会话反复得到的规矩：**守卫变红时先怀疑守卫**；第二、三次跑才把预算与产品行为分开。）

### C. 接线与验证
CI `integration` 新增一步、本机门禁新增 1 条 entry（**不需要** Redis/CH；`check_ci_wiring` exit 0，仓外工件 32 → **33**）；`check_documented_env` exit 0。**门禁 68 项全绿、`OVERALL=GREEN`、exit 0**。本轮**无产品代码改动**（`ops.md` 文档 + 新 drill）。

---

## 2cj. 第一百零三轮：熔断器的**探活/复活**首次实测 —— "自动复活"是真的（9.5s），**404 盲区**也真的会 flap；顺带纠正 `ops.md` §6.3 两处与代码相反的运维说明

**"provider 离开轮转"的另一半**：`design.md` §8.4 承诺"每 `probe_interval`（默认 10s）对 dead provider 做轻量探测（`GET {endpoint}/v1/models` 或 TCP 探活）；成功 → 移出 dead-set"，而 `ops.md` §9.1 把其中一处记成**已知盲区**（探活判定是"任何 `< 500` 都算活"、且探活路径不是真实推理路径，"记录在案而非偷偷修掉"）。这两半**从未在活节点上被观测**：第一百零二轮的死路由 provider **从未恢复**，所以复活路径一次都没走到。新增 `integration/test_breaker_probe_revival.py`（**10 条断言**；单 provider + 可切换应答的 mock，避免被故障转移掩盖）。

### A. 实测
| 情形 | 实测 |
|---|---|
| **真正恢复**的 provider（探活路径重新应答） | **9.5s 后自己离开 dead-set**（探活间隔 10s）—— **不需要任何流量、不需要手动 reset**，随后立刻正常服务 ⇒ design §8.4 的承诺成立 |
| 探活路径 `/v1/models` 答 **404**、而 **chat 路径 100% 坏** | **10.0s 后被复活**，紧接着的 5 个请求再次全败（`[503×5]`）⇒ **trip → revive → trip 的 flap**，与"真恢复"在表面上无法区分 —— §9.1 记的盲区在本轮变成了**可复现的数字** |
| 探活路径答 **5xx** | **保持 dead**（刻意：不然 500-ing 的 provider 会 flap）—— 这正是 §6.3 旧文案说错的那条 |

### B. 产品/文档缺陷：`ops.md` §6.3 与代码相反（本轮修正）
1. 旧文案："The HTTP probe considers **any HTTP response** (even a 401/429) as 'host alive'" —— **5xx 不是**（代码 `resp.status().as_u16() < 500`，注释写着"reviving on any response made a 500-ing provider flap"）。已按实测改成"**任何 `< 500`**"并给出三档实测表。
2. 旧文案让运维"**调** `[breaker] threshold`（例如 3）…`probe_interval` 更短" —— 而这两个旋钮**根本不存在**：`BreakerPolicy { threshold: 5, probe_interval: 10s }` 只来自 `Default::default()`，全仓**没有**读它们的 env 变量（`grep -rn BREAKER_THRESHOLD crates/`、`grep -rn PROBE_INTERVAL crates/` 皆空），而且这个项目**没有任何配置文件 loader**（`design.md` 的 `[breaker]` 段落只是设计草图）。已把 §6.3 改成"**没有可调的 breaker 旋钮**，唯一杠杆是 `DELETE /api/v1/breaker/{id}`"，并把它并入 §13 的"文档化但未接线"清单（与 `HYDRA_FAILOVER_GRACE_MS`、`HYDRA_RATE_LIMIT_FAIL_MODE` 同类）；`design.md` 的 `[breaker]` 草图也加了与 `non_route_strategy` 同款的幽灵开关标注。

### C. 反向证伪（`cp` 还原 + `cmp` 一致 + `sleep 2` 重建）
把探活判定改成 `Ok(_resp) => true`（＝"任何应答都复活"，即 §6.3 旧文案描述的行为）⇒ **只有 P3 变红**（`dead=[]`，500 的 provider 被复活），P1/P2 照过 ⇒ P3 正是那条规则的守卫，也说明本轮对 §6.3 的修正**有测试背书**。还原后 `cmp` 一致、重建后 **10/10 通过、exit=0**。

### D. 接线与验证
CI `integration` 新增一步、门禁新增 1 条 entry（不需 Redis/CH），`check_ci_wiring` exit 0（仓外工件 33 → **34**）；`check_documented_env`/`check_alert_expressions`/`check_documented_defaults` 均 exit 0。 **门禁 69 项全绿、`OVERALL=GREEN`、exit 0**（新 entry `breaker probe revival exit=0`；两份转写 `FAILED` 计数 0）。**本轮无 Rust 逻辑改动**（`ops.md` §6.3/§13 + `design.md` 标注 + 新 drill；证伪探针已还原）。

---

## 2ck. 第一百零四轮：数据面**请求体契约**首次跑通（含"截断请求被转发"这条**真实可达**的静默损坏路径）—— 并顺带修掉一个**不会失败的守卫**：`check_ci_wiring` 对 `integration/**` 是空判

### A. 新 drill：`integration/test_request_body_contract.py`（12 条断言）
`ops.md` §1.2/§6.7 把 terminate-mode 读全 body 的三条客户端可见结果写得很细（硬上限 413 / 总期限 408 / 读错误 400），而 `integration/` 里**一条都没有**（`grep -rl 'request_body_too_large|REQUEST_BODY_TIMEOUT|MAX_REQUEST_BODY_HARD' integration/` 为空）。实测（`HYDRA_MAX_REQUEST_BODY_HARD=65536`、`HYDRA_REQUEST_BODY_TIMEOUT_SECS=2`）：

| 腿 | 实测 |
|---|---|
| B1 **边界** | **恰好 65536 字节** ⇒ **200**（代码是 `buf.len() > cap`，边界正确，不是 413） |
| B2 | 65598 字节 ⇒ **413 `request_body_too_large`** |
| B3 | 声明 `Content-Length: 100000`、发一半就停 ⇒ **2.0s 时 408 `request_body_timeout`**（总期限真的在挡"发一半不发了"） |
| B4 | 声明 5000、只发 222 字节后关闭 ⇒ 上游**一个请求都没收到**，客户端得 **400 `request_body_read_error`** |
| **B4b** | 声明 `len+4096`、**只发一段完整合法 body** 后关闭 ⇒ 同样 **400 + 上游零请求**。**这条才是判别性的**：还原守卫（`Err → 当作正常结束`）后，网关**答 200 并把这段"长短不符"的请求转发给了上游**（`upstream saw [(66, 66)]`）—— 即代码注释里说的"静默损坏路径"**真实可达**；而 B4（截断点落在 JSON 中间）**看不出**这件事，因为它会被 model 提取第二次挡住（只答一个别的 400）。**教训：验证一条声明为"已修"的漏洞，探针必须打在"前缀本身合法"的形态上** |
| B5 | **chunked** 路径发一半卡住 ⇒ 同样 **2.0s 408**（文档说的"pingora H1 读只有 per-read 期限、每字节都会重置它"因此成立） |
| B6/B7 | admin 的 **1 MiB 编译期常量** ⇒ `413 request_body_too_large`；租户 API 的同尺寸体 ⇒ **`413 payload_too_large`（不同的 `code`！）** —— 只按 "413" 分支的客户端会看到两种信封 |

### B. ★守卫缺陷（本轮真正的"问题"）：`check_ci_wiring.cjs` 的覆盖判定对 `integration/**` 是**空判**
`coveredBy()` 里有一条 `if (dir && st.run.includes(dir + "/")) return true;`（注释写着"its directory, via a script"）。而 CI 里有几十条命令包含 `integration/` ⇒ **任何**丢进 `integration/` 的新文件都被判为"已接线"。**实测**：新建一个没人引用的 `integration/test_zzz_dummy_probe.py`，守卫照样打印 **`OK (everything is executed)`** ⇒ 过去若干轮里我写的"仓外工件 N → N+1"**只验证了计数，没有验证接线**（每轮我确实手工接了线，但守卫给不出任何保证，未来丢一个文件进去会**静默不跑**）。

**修法**：把目录前缀那条删掉，改成**真实链条**（不动点迭代）：
1. 某条 CI 命令**点名**该文件；
2. 或 CI 执行的**脚本**（`.sh`/`.py`，从命令里解析出路径）里点名它 —— 覆盖 `run-crud-local.sh → test_crud.py`/`check_error_contract.py`；
3. 或同目录中**已接线的 Python 文件 import 它** —— 覆盖 `e2e_proxy_test.py → mock_auth.py/mock_llm.py` 与 `check_error_contract.py → check_api_docs.py`（两跳）；
4. 仍是"working-directory + 测试 runner"那条（精确到目录相等/前缀）。
另加**工作流形状规则** `stepShapeProblems()`：某个 step 块里出现 **两个 `run:` 或两个 `env:`** ⇒ 报"step 缺少 `- name:`/列表标记、被并进了上一步"。

### C. 由此抓出的**真实 CI 缺陷**：`ci.yml` 里有一个**丢了列表标记的 step 体**
强化的守卫第一次运行就红了，两份文件：
- `integration/test_request_body_contract.py`（本轮新文件，尚未接线 —— 预期）；
- **`integration/test_cluster_limits.py`（第九十九轮那个 drill）** —— 追下去发现根因不在它：它在 CI 里的那一步后面跟着一个**没有 `- name:`/`- ` 标记的 step 体**（`env: CH_URL…` + `run: cargo test … --ignored`），于是两个 step 的映射**合并**、出现**重复的 `env:`/`run:` 键**。用严格 YAML 载入器验证：**`duplicate key: env`**（GitHub 会拒绝这样的 workflow；即便容忍，后一个 `run:` 会覆盖前一个 ⇒ **auth-cache drill 与那两条 `#[ignore]` 套件都静默不跑**）。`git show HEAD:.github/workflows/ci.yml` 干净 ⇒ 这是本会话未提交的 CI 编辑留下的。**修法**：补回 `- name:` 与列表标记（并写明这次踩坑的原因）；严格载入器现在通过（123 个 step / 8 个 job，无重复键）。

### D. 反向证伪（都是"先让探针证明自己能红"）
1. 把 `- name:`/标记**再删掉**（还原缺陷）⇒ 守卫输出**三条**：`has 2 run: keys`、`has 2 env: keys`（新规则），以及 `integration/test_cluster_limits.py is never executed`（强化后的覆盖规则抓到了**后果**）。`cp` 还原后 `cmp` 一致、守卫 `OK`。
2. drill 侧：还原"读错误当作正常结束"⇒ **只有 B4b 红**（`upstream saw [(66, 66)]` + 200），B4 仍绿 —— 证明 B4b 是那条漏洞的判别性探针。
3. 守卫自身的测试：新增 3 个夹具用例（`integration/**` 新文件必须 UNWIRED、脚本/import 链条必须算接线、合并 step 必须报重复键），并把**旧用例 14**（本来是靠"目录被提到"通过的）改成真实脚本点名 —— **旧夹具本身就在编码那条空判**，这是它存在的证据。守卫测试 **22 条全过**。

### E. 我自己的两个夹具错误
1. B1 的"恰好等于上限"体**少了 2 字节**（正文替换掉了 `"hi"` 两个字符）⇒ 断言长度并修正偏移后才是真边界；
2. B7 用**客户端 key** 调租户 API ⇒ 先吃 401（body 上限检查在鉴权之后）⇒ 改为租户 `access_token`。 **验证**：`check_ci_wiring` exit 0（仓外工件 34 → **35**，且这次是**真的**接线检查）；守卫自测 22 条全过；CI `integration` 新增一步、门禁新增 1 条 entry；**门禁 70 项全绿、`OVERALL=GREEN`、exit 0**（`request body contract exit=0`、`ci wiring exit=0`、`ci wiring tests exit=0`；两份转写 `FAILED` 计数 0）。

---

## 2cl. 第一百零五轮：**优雅停机**（`SIGTERM` + 在飞请求）首次实测 —— 在飞请求确实拿到完整 200、`exit = drain + 5s`；但**排空期间没有任何监听器在收**（探针/抓取全部拒连），且文档对 flush 钩子的说法是**过度声明**

`ops.md` §13.5b 把这条写成 Kubernetes 滚动更新契约（"`HYDRA_SHUTDOWN_DRAIN_SECS`（默认 20）是 Pingora `SIGTERM` 后可用于**排空在飞请求**的时间；它映射到 `grace_period_seconds`"，并给出 `terminationGracePeriodSeconds ≥ drain + 5 + slack` 的算术），而且"哪些信号会 flush 用量 sink"也写得很硬。但这些**只有静态守卫**（`check_compose_grace.cjs` 比对 compose 的 `stop_grace_period`；`main.rs` 有单测证明 drain 是显式设置的）—— **从来没有真的给一个带在飞请求的进程发过 `SIGTERM`**。新增 `integration/test_shutdown_drain.py`（**13 条断言**）。

### A. 实测（drain=10s 与 drain=1s 两个节点）
| 观测 | 实测 |
|---|---|
| `SIGTERM` 到达时**在飞**的请求 | **仍然拿到完整的 200**（排空是把已经接受的做完） |
| 进程退出时刻 | **`drain + 5s`**：drain=10 ⇒ **15.0s**；drain=1 ⇒ **6.0s**（+5 是 Pingora 的 `graceful_shutdown_timeout_seconds`，文档里那个"最后一步"）⇒ §13.5b 的算术**第一次有了实测数字** |
| 预算是不是真界限 | drain=1s 时，需要 3s 的请求在 **1.5s 被切断**（不是 3s） |
| **排空期间的新连接** | **数据面与管理面都拒连**：`SIGTERM` 后 ~0.2s，`/healthz`、`/readyz`、`/metrics` **全部停答**（实测 connection refused），`/v1/…` 同样拒连 ⇒ **排空是"只收在飞"，不是"窗口内继续收"**，且这段时间**既没有探针也没有抓取**（最长 `drain+5s`） |
| `SIGTERM` 时尚在 sink 批里的用量行 | 进程消失后**仍在库里** |
| 钩子的可观测 | 日志出现 `SIGTERM: flushing usage sinks`；死 sink 场景下还会打出 `usage sink is shutting down with an un-flushable batch; these usage records are LOST lost=1` |

### B. 两处文档修正（都来自 A 表）
1. **排空期间没有任何监听器在收** ⇒ §13.5b 新增实测表与运维结论：**不要指望排空去服务信号之后到达的流量**。K8s 的 endpoint 摘除是异步的，这段空隙里的请求会被**拒掉**；正确做法是**在 `SIGTERM` 之前**把 readiness 翻掉或加 `preStop` 延迟；同时预期一段最长 `drain+5s` 的 metrics 抓取缺口。宽限期仍然重要，但方向相反：它是防止 `SIGKILL` 落在"正在做完的那个请求"上。
2. **对 flush 钩子的说法是过度声明**（原文："a shutdown signal that is not in that list discards the buffered usage batch silently"、"without an explicit hook every routine restart silently discarded the whole sink buffer"）。**去掉钩子重跑**：缓冲的那行**照样落库**（周期 flush 间隔 5s，而退出窗口 `drain+5 ≥ 6s`，总能插进去），死 sink 场景下 `LOST` 也照样被打出来（sink 循环在 teardown 时收到通道关闭，同样走那条分支）。钩子**独有**的是"在 teardown 抢跑之前**提前**排空"和那条显式日志 —— 并且注意 `shutdown_unflushed` **计数器随进程一起消失**，所以**日志才是唯一的死后证据**。文档已按实测改写。

### C. 我自己的两个夹具错误（都是"断言没有判别力"）
1. N4 第一版把"排空期间探针还活着"当预期（写反了）；改成**按实测断言**：0.2s 之后管理面**必须**拒连、且数据面**必须**拒连（后者正是"排空只收在飞"的证据）。
2. N3 第一版声称"缓冲的行能落库证明钩子承重"—— **它不承重**：去掉钩子后 N3 照样绿。于是补了 **N5**（死 ClickHouse sink + `SIGTERM`）：其中"shutdown drain RUNS"那条**会红**（`SIGTERM: flushing usage sinks` 缺失），而"LOST 报出来"那条**两种情形都绿** ⇒ 计划里逐个标注了哪条有判别力、哪条没有。

### D. 反向证伪
去掉 `spawn_sink_flush_on_shutdown(...)`（＝修复前形态）⇒ drill 红 **2 条**：N1 的钩子日志（`<no line>`）与 N5 的"drain 真的跑了"；`cp` 还原后 `cmp` 一致、重建后 **13/13 通过、exit=0**。 **验证**：CI `integration` 新增一步、门禁新增 1 条 entry（不需 Redis/CH），`check_ci_wiring` exit 0（仓外工件 35 → **36**）；`check_documented_env`/`check_compose_grace` 仍绿；**门禁 71 项全绿、`OVERALL=GREEN`、exit 0**（`shutdown drain exit=0`；两份转写 `FAILED` 计数 0）。**本轮无产品代码改动**（文档 + 新 drill；证伪探针已还原）。

---

## 2cm. 第一百零六轮：**主密钥轮换**（`HYDRA_RESEAL_SECRETS` 一次性维护开关）在真二进制上端到端实测 —— 文档承诺全部成立；并抓出**开关自身的一个静默错**（拼错值 ⇒ 节点照常服务、轮换根本没跑）

`ops.md` §1.2/§3 把轮换写成运维流程（`docker run --rm -e HYDRA_RESEAL_SECRETS=1 …`，读 `reseal: provider_keys=… tenant_certs=… already_current=… failed=…` 这行报告与退出码，"`failed` 非空 ⇒ **不要删旧密钥**"，"版本已等于当前的行走**真的打开验证**"）。而此前的测试只有 `crates/hydra-server/tests/key_rotation.rs`（**库级**：`StaticKeyProvider`，没有进程、没有 env、没有报告行、没有退出码）。新增 `integration/test_key_rotation_live.py`（**24 条断言**，openssl 生成自签证书以覆盖**两类被封存的行**）。

### A. 实测：文档承诺全部成立（端到端）
| 腿 | 实测 |
|---|---|
| K0 | 节点用 A 跑 ⇒ 上游 mock **收到解密后的 provider key** `Bearer sk-provider-secret-42`（端到端确认"封存/解密"链路） |
| K1 | `HYDRA_RESEAL_SECRETS=1` + 新钥 B + `_PREVIOUS=A` ⇒ **exit 0**、报告 `provider_keys=1 tenant_certs=1 already_current=0 failed=0`、**0.0s 内退出且不绑任何监听器**（文档"此模式不服务流量"成立） |
| K2 | 只用 B 启动 ⇒ 行能打开、上游仍收到同一个 key ⇒ **轮换成功** |
| K3 | 再跑一次同钥 ⇒ `already_current=2`、exit 0 |
| K4 | 只用**旧钥 A** ⇒ 启动**直接失败**（exit 1，`stored key_version 2 does not match provider key_version 1`）而不是带着打不开的凭据运行 |
| **K6** | 行标着 **version 2**（＝当前版本）但**材料不是当前密钥**（用 C、同 version、无 previous）⇒ 报告 `failed=2`、exit 1，并打出**逐字**诊断 `labelled key_version 2 (the CURRENT version) but it does NOT open under the current key — the key was most likely rotated without bumping HYDRA_ENCRYPTION_KEY_VERSION. Do NOT delete the previous key until this is resolved` ⇒ 文档那句"already current 意味着**能打开**"**有实测背书**（若只看版本号就会误报成功） |
| K5 | 用 C（无 previous）⇒ exit **1**、`reseal FAILED: …`、`failed=2`，且**行未被改动**（随后用 B 仍能端到端服务） |
| K7 | 在 **edge** 节点上跑该开关 ⇒ 拒绝并给出文档里的原因（`needs a local database`） |

### B. ★开关自身的静默错（本轮产品修复）
`reseal_requested()` 原来是 `matches!(var, Ok("1") | Ok("true") | Ok("yes"))` ⇒ **其它任何值都被当成"照常服务"**。实测（K8 的修复前一轮日志 `.acceptance/round106/keyrot-findings.log`）：`YES`、`on`、`TRUE`、`reseal`、`2` **全部让节点正常起服务**（`served=True`，进程不退出）⇒ 运维按文档跑一次性命令、看到容器起来在服务，**以为轮换完成了**；而 `upgrade_requested` 对同类错误是"**故意严格**"的（文档原话：typo 会得到响亮的拒绝，而不是静默不升级）。
**修法**：`parse_reseal_switch(Option<&str>) -> ResealSwitch{Off,On,Invalid(String)}`（**纯函数**，与 `proxy::config` 的 `parse_*` 同形），大小写不敏感；**ON**=`1|true|yes|on`，**OFF**=`（空）|0|false|no|off`，**其它一律 `Invalid`** ⇒ 启动期 `error!` 打出损坏值并 **exit 1**。单测 `the_reseal_switch_is_strict_about_values_it_does_not_know` 覆盖三态与大小写/空白。文档 §1.2 那行同步改为"接受 `1|true|yes|on`（大小写不敏感）+ 其它值被**拒绝启动**"，并把 K6 的逐字诊断写进去。

### C. 反向证伪（`cp` 还原 + `cmp` 一致 + 重建）
把解析改回"不认识就走 Off"（＝修复前语义）⇒ drill **恰好 6 条红**（K8 的 6 个非法值，全部 `served=True`），其余 18 条照过 ⇒ K8 就是这道静默错的守卫。还原后 `cmp` 一致、重建后 **23/23 通过、exit=0**、`fmt --check` clean。

### D. 我自己的两个夹具错误
1. **K7 的 edge 配置不完整**：先被 `edge mode requires HYDRA_CONTROL_URL` 拦下（说明校验顺序在 reseal 之前），补 `HYDRA_CONTROL_URL` 后又被 `cluster mode requires HYDRA_USAGE_SINK=clickhouse` 拦下 ⇒ 逐个补齐后才拿到文档里那条 `needs a local database`。两次红都**不是产品问题**。
2. **证书不是独立路由**：`POST /api/v1/tenants/t1/cert` 是 404 `unknown path`（实测）；`cert_pem`/`cert_key_pem` 是**租户体自身的字段** ⇒ 改成在创建租户时带上，才有 `tenant_certs=1`。 **验证**：CI `integration` 新增一步、门禁新增 1 条 entry（不需 Redis/CH），`check_ci_wiring` exit 0（仓外工件 36 → **37**）；`check_documented_env` 0；新增的那条 Rust 单测在 `cluster-redis` 特性集里（`server` 不隐含它），门禁的可选套件与 CI 的 optional-features 作业都会跑到；`check_public_claims` 仍为 **804 = 253 + 551**（未受影响，因为它只数 `--features server`）。**门禁 72 项全绿、`OVERALL=GREEN`、exit 0**（`master-key rotation exit=0`；两份转写 `FAILED` 计数 0）。

---

## 2cn. 第一百零七轮：**主密钥的来源**（内联 base64 vs `_FILE` 原始 32 字节）首次端到端实测 —— 两种形式同密钥流、**文件优先**、只吃行尾；并修掉一个"got 44"式无提示报错

第一百零六轮测的是**轮换**（用 `HYDRA_ENCRYPTION_KEY`）；本轮测**密钥材料的来源**。`ops.md` §1.2 写得很简洁但很容易踩："`HYDRA_ENCRYPTION_KEY` = **Base64 of 32 bytes**（`openssl rand 32 | base64`）… 也接受 `HYDRA_ENCRYPTION_KEY_FILE`（**raw 32-byte file**）" —— 内联是 base64、文件是**裸字节**，而 K8s secret 卷/`openssl rand 32 > key` 产出的文件**结尾带换行**。此前**没有任何测试**碰过这两条路径：第一百零六轮只用内联形式，`crypto.rs` 的单测直接 `StaticKeyProvider::new([7u8;32],1)`，既不读环境也不读文件系统。新增 `integration/test_master_key_sources.py`（**11 条断言**）。

### A. 实测
| 腿 | 实测 |
|---|---|
| S1 | 内联 base64 可服务，上游收到**解密后**的 provider key |
| **S2** | 两种形式是**同一个密钥流**，且**双向**成立：内联封存的行用**文件**打开 ✓、文件封存的行用**内联**打开 ✓ |
| S3 | 文件结尾 `\n` **和** `\r\n` 都被接受（`echo`／K8s secret 卷的产物可直接用） |
| S4 | 结尾是**空格**则**不**被静默接受（只裁行尾），错误里给出字节数（`must be 32 bytes, got 33`） |
| **S5** | 两者**同时**设置时**文件优先**（文档 "preferring the file form"），且**双向**证明：A 封存的库在（file=A, inline=B）下能开；B 封存的库在同一对值下**拒绝启动** ⇒ 内联那个确实没被使用 |
| S6 | 都不设置 ⇒ **拒绝启动**（fail-closed）并点名两个变量 |
| S7 | `_FILE` 指向不存在路径 ⇒ 启动失败并**打印该路径** |
| S8 | 文件里放 **base64** 文本 ⇒ 失败（文件形式要裸字节），且错误现在**说明两种形式的区别** |

### B. 产品修复（小但真实）：错误信息不再只说 "got 44"
`CryptoError::KeyLength` 原来只有 `master key must be 32 bytes, got {got}` —— 而最经典的踩坑就是"**把 base64 放进文件**"（或把裸钥放进变量），此时唯一的线索是 `got 44`。改为
`master key must be 32 bytes, got 44: HYDRA_ENCRYPTION_KEY holds the BASE64 of those bytes, while HYDRA_ENCRYPTION_KEY_FILE holds the RAW bytes — base64 text in the file (or a raw key in the variable) produces exactly this`
（与本仓其它"响亮且告诉你改什么"的报错同风格，如 `HYDRA_CLUSTER_TOKEN is too short … openssl rand -hex 32`）。新增单测 `the_length_error_names_both_forms`（断言消息里同时出现两个变量名与 BASE64/RAW）；`ops.md` §1.2 那行同步补上本节实测到的全部事实（同密钥流、文件优先、只裁行尾、错误自解释）。

### C. 反向证伪（`cp` 还原 + `cmp` 一致 + 重建）
同时植入两个互不相干的探针：①`trim_key_bytes` 变成恒等（不裁行尾）⇒ **S3 两条红**（K8s 那条路直接不可用）；②把 `KeyLength` 的消息改回原句 ⇒ **S8 红**。共 3 条红、其余 8 条照过 ⇒ 两条断言都有判别力。还原后 `cmp` 一致、重建后 **11/11 通过、exit=0**。

### D. 我自己的两个夹具错误（都是"探针在错的时刻跑"）
1. `run_once()` 第一版在**停掉节点之后**才把控制权交回调用者 ⇒ S2/S5 的请求打在死端口上（`HTTP 0`），三条腿因此变红 —— 纯夹具问题。改为 `run_once(..., probe=…)`：**在节点活着时**执行探针。
2. 探针第一版回传的是 `proxied()` 的 `(status, body)` **元组**（`st2 == 200` 恒假）⇒ 抽出 `status_of()`。

### E. 验证（含门禁第二次抓到我自己的公开数字）
CI `integration` 新增一步、门禁新增 1 条 entry（不需 Redis/CH），`check_ci_wiring` exit 0（仓外工件 37 → **38**）；`check_documented_env` 0、`fmt --check` clean。**门禁第一次跑 RED，红的是 `public claims`，抓的正是我这轮新增的那条非特性门控单测**：`--features server` 套件变成 **253 + 552 = 805**，而 `docs/index.html` 仍宣称 804（含两处细目）—— 这是**两轮内第二次**（第一百零四轮同样如此）。按守卫自己给出的路径 `node scripts/check_public_claims.cjs --measure --write` 回写（805 = 253 + 552），`check_i18n` 0。**流程教训（已内化）**：只要新增**未被特性门控**的 Rust 单测，就必须**在跑门禁之前**先执行那条 `--measure --write`；否则每轮都会白跑一次全量门禁。**第二次门禁 73 项全绿、`OVERALL=GREEN`、exit 0**（`master key sources exit=0`；两份转写 `FAILED` 计数 0）。

---

## 2co. 第一百零八轮：**认证跳（租户 `auth_url`）面对死路由**首次实测 —— 有界 503、绝不转发、**故障不被缓存**（每次请求都付 2s）；并实测出 `FailMode::Open` **无法被选中**（又一个"文档化了但接不了线"的模式）

轮到最后一个没被"死路由"测过的跳：**租户的 `auth_url`**。`http.rs` 把 401/403 归为**可缓存的拒绝**，而传输失败 / 5xx / 无法解析的 2xx 体走 `fail_mode_verdict(fail_mode)`——注释写着 **"never cache"**，默认 `FailMode::Closed` ⇒ `503 auth_upstream_unavailable`。新增 `integration/test_auth_hop_failmode.py`（**9 条断言**；黑洞用 TEST-NET-1，先自证该地址在本机确实黑洞）。

### A. 实测
| 情形 | 客户端看到 | 代价 |
|---|---|---|
| `auth_url` **不可达**（SYN 被丢） | `503 auth_upstream_unavailable`（`type: auth_error`） | **每次请求 ~2.00s**（＝文档的 2000ms 往返超时），且**provider 一次都没被调用**（`+0`） |
| `auth_url` **拒绝连接**（没人在听） | 同样的 503 | **~0.001s** |
| 认证服务恢复 | **下一个请求立刻 200** | — |
| 归属 | `hydra_auth_upstream_error_total{tenant="t1"}` 增加 | — |

⇒ 两点运维事实：**死认证路由会让每个请求都付出那个超时**（因为刻意不缓存）；而**缓存它会更糟** —— 见下。

### B. ★反向证伪（并且它先纠正了我的一条**没有判别力**的断言）
1. 第一版 A2 用**轮换的 key**（`-0/-1/-2`）发 3 个请求 —— 这样**永远打不到缓存**，所以把"把故障写进缓存"的探针种下去后**照样全绿**：断言没有判别力。改成**同一个 key** 之后才暴露问题。
2. 第二步探针种在 `Err(_)`（外层 `tokio::time::timeout`）分支上 —— 但实测黑洞走的是 **`Ok(Err(e))`** 分支（reqwest 自己 2s 的 `ClientBuilder::timeout` 先触发，日志里是 `auth upstream request failed`，不是 `timed out`），所以仍然全绿。**把探针挪到真正触发的分支**后：
   - A2 变成 `[(503, 2.0), (401, 0.0), (401, 0.0)]` ⇒ 第 2/3 个请求**瞬间**命中缓存拒绝（而且**状态码变成 401**，因为缓存拒绝路径返回 `decide(v, 401, "denied")`）；
   - A4 变成 **HTTP 401** —— 认证服务**已经恢复**，租户却还在被拒（缓存拒绝的 30s TTL）。
   两条腿同时红 ⇒ "不缓存故障"是**真正承重**的，且顺带记录了一个细节：**缓存拒绝回 401，而故障回 503**。
3. `cp` 还原后 `cmp` 一致、重建后 **9/9 通过、exit=0**。

### C. 新发现的"文档化了但接不了线"：`[auth] fail_mode`
`design.md` §11.4 把 `[auth] fail_mode` 写成**配置项**，`http.rs` 里 `FailMode::Open`（"availability-first"：认证服务挂了就放行）**实现完整且有注释**；但 `main.rs` 用 `AuthConfig { ..AuthConfig::default() }`（默认 `Closed`），全仓**没有**读它的 env、也**没有**配置文件 loader ⇒ **实测** `HYDRA_AUTH_FAIL_MODE=open` / `HYDRA_FAIL_MODE=open` / `HYDRA_AUTH_FAILMODE=open` 三者在认证上游不可达时**都仍然返回 503** ⇒ 这个模式**无法被选中**。处理方式沿用第一百零三轮的先例（`[breaker]` 草图）：**不擅自接线**（放行未认证流量是安全决策），而是把它写进 `ops.md` §5.x 的实测表 + §13 的"文档化但未接线"清单，并给 `design.md` §11.4 加同款幽灵开关标注。

### D. 文档
`ops.md` 新增 §5.x（上表 + 两条运维结论 + fail_mode 说明）；§13 清单新增 `[auth] fail_mode`；`design.md` §11.4 加标注。`check_documented_env`/`check_compose_grace` 仍绿。 **验证**：CI `integration` 新增一步、门禁新增 1 条 entry（不需 Redis/CH），`check_ci_wiring` exit 0（仓外工件 38 → **39**）；**门禁 74 项全绿、`OVERALL=GREEN`、exit 0**（`auth hop (dead route) exit=0`；两份转写 `FAILED` 计数 0）。**本轮无产品代码改动**（文档 + 新 drill；证伪探针已还原）。

---

## 2cp. 第一百零九轮：**用量行的丢失计数**（计费相关）第一次被实测并**写进文档** —— 顺带发现 ClickHouse sink 丢掉的行**不带 trace id**（生产那条路缺诊断）

`sink.rs` 的注释写着"丢失计费数据绝不能只体现在一行日志里（audit §3.9）"，并有一个四词表的原因集合（`channel_full`／`channel_closed`／`retention_cap`／`shutdown_unflushed`）。而 `grep -rn usage_dropped_total dev-docs/` **为空**：这个"你在少计费"的信号**在任何运维文档里都不存在** —— §9 的 Key series 没有、§9.1 的告警表也没有，只能读源码才知道。新增 `integration/test_usage_drop_accounting.py`（**10 条断言**）。

### A. 实测（死亡后端 = ClickHouse mock 关掉）
| 腿 | 实测 |
|---|---|
| D0 | 后端健康时 5 个请求全 200、行确实落库，且 `hydra_usage_records_dropped_total` **根本不出现**（不误报） |
| D1 | **600 个请求**打向死后端：**全部 200**（丢遥测绝不破坏代理路径），**第一次丢弃出现在第 600 个请求**，`drops=[('channel_full', 88)]` |
| D2 | 原因取自文档化的词表；**出厂默认下运维最先看到的是 `channel_full`**（sink 先缓冲满 256 一批并进入退避，之后通道才溢出；1 万条的 `retention_cap` 反而很难达到）；每条丢弃都带 `dropped_trace_id` |
| D3 | 后端恢复后：保留的批**落库**（`inserts` 增加）、后续请求照常 200、**计数器停止增长**（88 → 88） |

### B. 发现：ClickHouse sink 的丢弃告警**不带 trace id**
`SqliteSink::record` 的告警有 `dropped_trace_id`（能定位是**哪一行**），而 **`ClickHouseSink::record` 的告警没有** —— 实测日志是 `… dropping usage record error=no available capacity reason="channel_full"`，**只有计数没有行身份**。而 ClickHouse 正是集群部署**被要求**使用的 sink（`ops.md` §12）⇒ **生产那条路缺的恰好是诊断信息**。修法（与 SQLite sink 对齐，记录就在手里）：从 `TrySendError::{Full,Closed}(r)` 取出 `r.trace_id` 并打出 `dropped_trace_id=…`；实测修复后日志变成 `… dropping usage record dropped_trace_id=hydra-18d9ef83680d6fd9-12 error=… reason="channel_full"`。

### C. 文档（本轮另一半产出）
`ops.md` §9 的 Key series 补上 `hydra_usage_records_dropped_total{reason}`（含四个原因的解释与实测阈值/顺序），**§9.1 新增一行告警** `increase(hydra_usage_records_dropped_total[10m]) > 0`（"**计费数据正在丢失**"：请求仍照常 200，正因如此才需要告警而不是靠症状）。`check_alert_expressions` 由 16/6 → **17/6 全解析**。

### D. 反向证伪与我自己的三个错误
1. **第一次探针（400 请求）根本不够**：sink 会缓冲满 256 一批再退避，400 个请求**一次都没溢出** ⇒ 四腿红却全是"夹具没到阈值"。改成**分块驱动直到第一次丢弃出现**，并把阈值本身报出来（600）。
2. **读错了指标名**：我用 `hydra_usage_dropped_total`（metrics.rs 里的**getter 名**），而注册的序列是 **`hydra_usage_records_dropped_total`** ⇒ 在节点已经打了 2488 条丢弃告警的同时，我读到"空序列"，差点把它当成"计数没接上"的 P1。**教训：getter 名 ≠ 序列名，断言前先确认注册名。**
3. **备份取错了时刻**：我在**应用修复之前**做了 `.keep`，于是"证伪后还原"把修复也一起还原了（重新发现后重打补丁，并把**修复后**的文件另存为 `.keep`）。**教训（已在计划里写死）**：一轮里既改代码又做证伪时，备份必须取**证伪探针植入前**的状态，而探针是打在修复后的树上的。

反向证伪本身成立：把 `dropped_trace_id` 去掉 ⇒ **只有 D2 那条红**；还原（正确的 `.keep`）后 **10/10 通过**、`fmt --check` clean。 **验证**：CI `integration` 新增一步、门禁新增 1 条 entry（不需 Redis/CH），`check_ci_wiring` exit 0（仓外工件 39 → **40**）；`check_documented_env` 0、`check_alert_expressions` 17/6 全解析；**门禁 75 项全绿、`OVERALL=GREEN`、exit 0**（`usage drop accounting exit=0`；两份转写 `FAILED` 计数 0）。

---

## 2cq. 第一百一十轮：**两个 1 MiB body 上限"谁排空"**首次实测 —— 租户 API 不排空（留在链路上 ~1 MiB）但**能读到 413**；数据面**先排空**（全消费）—— 并把文档那句"可能遇到连接重置"**按实测改准**

`ops.md` §6.7 记着一条"Known limitation (no fix promised)"：租户 API 的 body 超过 1 MiB 时"回 413 并关闭连接、**不排空剩余 body** —— 仍在上传的客户端**可能**在读到 413 body 之前遇到连接重置"。代码与之相符（`tenant_api/mod.rs::read_body` 直接返回 413 元组，**不**调 `drain_request_body()`），而**数据面**的硬上限路径**会**调 `session.as_downstream_mut().drain_request_body()` 再回 413。两个平面一个对"还在上传的客户端"礼貌、一个（文档说）不礼貌 —— 而"可能遇到重置"正是那种"要么真的在线上成立、要么不成立"的说法。新增 `integration/test_body_cap_drain.py`（**5 条断言**，裸 socket 因为 urllib 不会"滴灌"上传）。

### A. 实测
| 腿 | 实测 |
|---|---|
| T1 | 租户 API + 2 MiB 体（一口气写）⇒ **413 `payload_too_large`** 可读 |
| **T2** | 租户 API + **滴灌** 2 MiB（32 KiB/次、20ms 间隔）⇒ 节点在**只消费了 1 081 344 / 2 097 147 字节**时就回了 **413 Payload Too Large**，且**客户端 `write_error=None`、413 读得到** |
| **T3** | 数据面 + 滴灌超限体（64 KiB 上限）⇒ 节点**先把 131 125 字节全部消费**（排空）再回 **413 `request_body_too_large`**，客户端必然读得到 |
| T4 | 之后新连接在两个平面上都被正常服务（无残留损伤） |

**判别性指标不是"有没有重置"（那本来就是个 race，文档也写的是 "may"），而是"节点在回 413 之前消费了多少字节"**：≈1 MiB ⇒ 没排空；=全部 ⇒ 排空了。T2 与 T3 因此成为一条干净的不对称证据（1081344/2097147 vs 131125/131125）。

### B. 文档按实测改准（§6.7）
原句只说"可能遇到连接重置"，容易让人以为**总是**读不到 413。实测把两个方向都写清：节点在**cap 处停止读取**、留 ~1 MiB 在链路上（"不排空"的事实基础）；而**一旦客户端在响应可读时停止推送，它就能正常读到自己的 `413 payload_too_large`**（`write_error=None`）⇒ 重置需要的是**继续推送**的客户端；数据面则相反（先排空，413 必然可读）。并给写客户端的建议：**看到连接可读就停止写入**。

### C. 反向证伪
在租户 API 的 413 分支前**插入 `drain_request_body()`**（＝让租户 API 变得像数据面）⇒ T2 变成 `wrote 2097147/2097147` ⇒ **只有"没有排空"那条红**，其余照过 ⇒ 该断言正是这条不对称的守卫。`cp` 还原（这次备份取的是**修复/探针植入前**的正确状态：`.keep` = 探针前）后 `cmp` 一致、重建后 **5/5 通过**、`fmt --check` clean。

### D. 我自己的夹具错误
`dribble()` 第一版把请求行写死成 `POST` —— 而**租户 API 的写路由是按 `(方法, 路径)` 注册的**（第八十九轮已记录两种形状）：POST 打在同一条路径上被**在读取 body 之前**就以 **404** 拒绝（实测 `wrote 32768/2097147 ... response='HTTP/1.1 404 Not Found'`）⇒ 该腿根本没走到上限。把方法参数化（T2 用 `PUT`）后即得真值。 **验证**：CI `integration` 新增一步、门禁新增 1 条 entry（不需 Redis/CH），`check_ci_wiring` exit 0（仓外工件 40 → **41**）；`check_documented_env` 0；**门禁 76 项全绿、`OVERALL=GREEN`、exit 0**（`body cap draining exit=0`；两份转写 `FAILED` 计数 0）。**本轮无产品代码改动**（文档 + 新 drill；证伪探针已还原）。

---

## 2cr. 第一百一十一轮：**客户端中途挂断**（流式）首次实测 —— 上游被取消（8 块只发了 3 块）、用量**照样记账**、provider **不被冤枉**；文档里"指标与熔断器对同一事件刻意不一致"得到实测确认

流式路径的其它失败都测过了（第九十一轮：上游卡死、上游中途死亡；第一百零二轮：死路由），**唯一没测过的方向是"客户端自己走了"**。`proxy.rs::stream_response` 每写一块都 `?` 传播下游写失败，其注释写着"下游写失败通常意味着**客户端**走了，绝不能因此把 provider 判为不健康" —— 这条声明当时没有任何测试。新增 `integration/test_client_disconnect.py`（**8 条断言**；mock 用 chunked SSE，**usage 对象放在第一块**，客户端读到头一块后以 `SO_LINGER 0` **RST** 掉）。

### A. 实测
| 观测 | 实测 |
|---|---|
| 上游是否被取消 | mock 只**送达 3/8 块**（网关不再拉取）⇒ 客户端走后不会继续为一个没人要的答案烧上游 |
| 是否仍然记账 | **有**用量行，且带上游已经报过的 token：`tokens_in=5 tokens_out=8` ⇒ 挂断**不会抹掉**已产生的用量 |
| 那一行长什么样 | **与完整回答无法区分**：`status_code=200`、`error` 为 NULL ⇒ 店铺记录的是用量，不是"这次被截断" |
| provider 是否被冤枉 | **没有**：dead-set 仍为 `[]` |
| 指标 | `hydra_mid_stream_errors_total{provider="p1"} 1` ⇒ **同一个事件进了指标、却没进熔断器** |
| 之后 | 节点继续正常服务 |

### B. 我自己的两个错（都是"把预期写反/写错"）
1. **C2 的预期写反了**：我断言"客户端侧的截断不该出现在 `hydra_mid_stream_errors_total`"，实测却是 `1`。查 `ops.md` 同一段：文档明写"the failure is counted in `hydra_mid_stream_errors_total{provider}`"，且**只有** idle-bound（上游沉默）那一路才喂熔断器；"区分所有剩余原因"本身是**已记录的**产品决策 §7-1 ⇒ **文档是对的、我的断言是错的**，已改为断言文档化行为（指标计数 + 熔断器不动，两者对同一事件刻意不一致）。
2. **C1 的探测器不可靠**：我用 mock 的写异常（`broke`）判断"网关关闭了上游连接"，实测 `broke=False` —— 往已关闭的 socket 写在本机是**可以成功**的（进内核缓冲，直到 RST 到达）。改成用**送达块数**（`3 < 8`）作为判别量。
3. 另两处夹具问题：SQLite 的 `usage_record` 用 **`tokens_in/tokens_out`**（migration 0005 起的中性列名）且**没有 `trace_id` 列**（那是 ClickHouse 表才有）；SQLite sink 每 **5s** 才落一批 ⇒ 需要轮询等待行出现。

### C. 反向证伪
把 `session.write_response_body(Some(chunk), false).await?` 改成忽略错误（`let _ = …`）⇒ mock **送达 8/8 块**（网关继续把没人要的答案拉完）⇒ **只有"停止拉取"那条红**，其余照过 ⇒ 该断言正是"取消"行为的守卫。`cp` 还原（备份取的是**探针植入前**＝修复后状态）后 `cmp` 一致、重建后 **8/8 通过**、`fmt --check` clean。

### D. 文档
`ops.md` 的 mid-stream 段落新增实测块：取消（3/8 块）、仍然记账（tokens 5/8）、行长什么样（200 + `error` NULL，**看不出被截断**）、指标与熔断器刻意不一致，并指出"想知道有多少答案被客户端放弃"仍属 §7-1 的待决事项。 **验证**：CI `integration` 新增一步、门禁新增 1 条 entry（不需 Redis/CH），`check_ci_wiring` exit 0（仓外工件 41 → **42**）；**门禁 77 项全绿、`OVERALL=GREEN`、exit 0**（`client disconnect exit=0`；两份转写 `FAILED` 计数 0）。**本轮无产品代码改动**（文档 + 新 drill；证伪探针已还原）。

---

## 2cs. 第一百一十二轮：**监听器/SNI 告警契约**在活抓取上逐条评估（四种配置）—— 三条文档表达式**各自何时触发**全部实测确认；本轮也是"夹具自身三个坑"的一课

`ops.md` §9.1 开头那三行是运维判断"TLS 到底有没有在服务"的唯一信号，而且写明读法（`protocol="plain"` 是**存活**、`protocol="tls"` 是**配置**）。三者都能**只靠配置**触发（有证书没 TLS 端口、有 TLS 端口没证书、健康配对），而**从来没有人对着真实 `/metrics` 求值过**。新增 `integration/test_listener_signals.py`（**9 条断言**）。

### A. 实测（四种配置，每腿都在"启动时库里已有证书"的前提下起节点）
| 配置 | 实测信号 | 文档表达式 |
|---|---|---|
| 无证书、无 TLS 端口 | `plain=1 tls=0 certs=0 misconfig=0` | 三行**全为假**（正确） |
| **启动时即有证书**、无 TLS 端口 | `certs=1 tls=0 plain=1`，`misconfig{certs_without_tls_port}=1` | **第 1 行触发** + **第 3 行触发** |
| 有 TLS 端口、无证书 | `tls=1 certs=0`，`misconfig{tls_port_without_certs}=1` | **第 2 行触发** + 第 3 行触发 |
| TLS 端口 + 证书 | `plain=1 tls=1 certs=1 misconfig=0` | 三行**全为假**；并且**带租户 SNI 的 HTTPS 请求真能 200** |

一个关键的**时序**事实（本轮实测）：**监听器规划器只在启动时跑一次** —— 运行中通过管理面写入证书**不会**重新规划，所以"有证书没端口"的 misconfig 计数必须在**启动前**就把证书写进库才会触发（文档也把它写成 "Startup-time configuration problem"，因此文档自洽，但读的人容易以为任何时候都成立）。drill 因此改成"先用一个临时节点把库种好、再起被测量的节点"。

### B. ★反向证伪（两个方向，一次构建）
①把 `PlanNote::CertsWithoutTlsPort` 的推入删掉 ⇒ **L2 的"misconfig 被点名"变红**；②把 `TlsPortWithoutCerts` 改成**无条件推入**（＝过度告警方向）⇒ **L1 与 L4 的"三行全为假"变红**。共 3 条红、其余照过 ⇒ 两条断言都有判别力（既能抓"该报没报"，也能抓"不该报乱报"）。`cp` 还原（`.keep` 取的是**探针植入前**状态）后 `cmp` 一致、重建后 **9/9 通过**、`fmt --check` clean。

### C. 本轮我踩的三个**夹具**坑（都已写进代码注释）
1. **切片式字符串手术把文件写坏了**：我用 `s[s.index(A):s.index(B)]` 做整函数替换，而 B 出现在 A 之前 ⇒ 生成 **470 386 行**的畸形文件（含 `sdef`/`rdef` 残片），`python3 -m py_compile` 当场报 SyntaxError、**没有静默通过**。改用 `write` 工具整文件重写（内容在会话里可复原）。**教训**：整函数替换用精确匹配编辑，别用下标切片；python 改完文件后**必须**跑 `wc -l` + `py_compile` 核对。
2. **上一次会话遗留的孤儿进程**：pid 742147（`./target/debug/hydra`，已跑 **7 小时 27 分**）一直占着 `127.0.0.1:18740/18741`，本轮第一条腿因此 `Address already in use` 起不来，**看起来像产品故障**。查清它属于**本仓库二进制**（不是用户容器里的 dev 栈：那些 exe 是 `/usr/local/bin/hydra`）后杀掉，并给本 drill 加上 `kill_our_instances()`（按 `/proc/<pid>/exe` 匹配，绝不按名字）—— 这是第七十八轮定下的模式，新 drill 应当一律沿用。
3. **指标竞态**：`/api/v1/health` 由**管理面**服务回答，而它比数据面监听器先起来；因此紧跟 health 通过的抓取**读不到** `bound{protocol="plain"}`（四腿全 `None`，而节点其实在正常服务）。加 `wait_for_plain()`（自检线程每 250ms 探一次自己的入口端口，最长 20s）后即得真值。 **验证**：CI `integration` 新增一步、门禁新增 1 条 entry（不需 Redis/CH），`check_ci_wiring` exit 0（仓外工件 42 → **43**）；**门禁 78 项全绿、`OVERALL=GREEN`、exit 0**（`listener signals exit=0`；两份转写 `FAILED` 计数 0）。**本轮无产品代码改动**（文档 + 新 drill；两枚探针已还原）。

---

## 2ct. 第一百一十三轮：**一次失败的 reload**首次被造出来 —— 顺带修掉一个**告警契约缺陷**：显式 `POST /reload` 从不更新 `config_snapshot_stale`（失败不亮、成功不灭）

`ops.md` §9.1 那一行（`hydra_config_snapshot_stale == 1` ⇒ "A post-write reload failed; the in-memory snapshot is behind the DB"）是运维发现"快照落后于库"的唯一信号，而 `reload_best_effort` 的注释把它写得很重（大写 STALE：写完 reload 失败后**后续写入仍会返回 2xx 却毫无运行时效果**）。它的触发条件**从没被造出来过**：管理面写边界会拒绝那个历史起因（`endpoint` 缺 scheme），所以本轮改为**绕过写边界、直接把坏行写进 SQLite**（＝手工改库/从备份恢复的真实情形）。新增 `integration/test_snapshot_stale.py`（**10 条断言**）。

### A. ★发现的产品缺陷：显式 reload 端点"只共享加载、不共享记录"
`admin::cluster_api::reload` 的注释写着"Explicit reload shares the same best-effort path (reload_all only)" —— 它共享的只有**加载**，**结果从不记录**：既不调 `record_config_snapshot_stale`，也不更新 `AdminState::snapshot_stale`。实测（修复前）：坏行存在时 `POST /api/v1/reload` 返回 **400 `reload_failed`（old snapshot retained）**，而**仪表仍是 0** ⇒ 文档让运维去告警的那个指标**对失败不亮**；随后某次写入触发的失败 reload 把它置 1，等坏行清掉、`/reload` **成功**之后它**仍是 1** ⇒ 按文档"再 reload 一次即可恢复"的运维会看到告警**继续燃烧**。两个方向都错。
**修法**：把"记录结果"抽成**唯一所有者** `handlers::note_reload_outcome(state, stale)`（同时写指标与 `snapshot_stale` 原子），`reload_best_effort` 与显式端点**都**走它（显式端点失败置 1、成功置 0）。这也让 `tenant_config_api` 之外的三条路径口径一致。

### B. 实测（修复后）
| 腿 | 实测 |
|---|---|
| S0 | 健康节点：gauge=0、`/reload` 200 |
| S1 | 坏行 + `POST /reload` ⇒ **400 `reload_failed`** 且 **gauge=1**；`GET /tenants/t1` 带 **`snapshot_stale: true`** |
| S1 | 节点**继续服务**（200，旧快照）——坏的从来不是流量 |
| S1（陷阱） | 陈旧窗口内的写入 **仍返回 200**，但**毫无效果**：`enabled=false` 之后该租户**照常被服务**（200）——代码注释里那句大写警示**实测成立** |
| S2 | 清掉坏行 + `/reload` ⇒ **200 且 gauge 回到 0**、`snapshot_stale: false`，并且**先前那次无效果的写入终于生效**：同一租户变成 **403 `tenant_disabled`** |

### C. 反向证伪
把显式端点的两处 `note_reload_outcome` 拿掉（＝修复前）⇒ **恰好 4 条红**：S1 的 gauge 与 per-tenant 标志、S2 的 gauge 与 per-tenant 标志；其余 6 条照过 ⇒ 断言与修复一一对应。`cp` 还原（`.keep` 取的是**探针植入前**状态）后 `cmp` 一致、重建后 **10/10 通过**、`fmt --check` clean。

### D. 文档与夹具
`ops.md` §9.1 那行按实测重写（400 `reload_failed`、gauge=1、**2xx 无效果**陷阱、per-tenant `snapshot_stale: true`、恢复步骤，以及"此前显式端点不记录 ⇒ 告警既不亮也不灭"的实测）。**我自己的两个夹具错误**：①第一次把"gauge 变 1"的读取放在**触发它的那次写入之前**（显式 `/reload` 修复前不记录，gauge 自然还是 0）⇒ 顺序纠正为"先显式 reload 断言失败+计数，再看写入陷阱"；②`snapshot_stale` 字段在**租户视图**上而不是 health body 上（第一版断言查了 health）⇒ 改为查 `GET /tenants/t1`。另：本轮两次用 heredoc 做整段替换时，一次**转义把引号写坏**导致 `py_compile` 报错（当场发现）⇒ 之后改用 `edit` 工具做精确匹配。 **验证**：CI `integration` 新增一步、门禁新增 1 条 entry（不需 Redis/CH），`check_ci_wiring` exit 0（仓外工件 43 → **44**）、`check_alert_expressions` 17/6 仍全解析；**门禁 79 项全绿、`OVERALL=GREEN`、exit 0**（`snapshot stale exit=0`；两份转写 `FAILED` 计数 0）。

---

## 2cu. 第一百一十四轮：把"**文档里点名的指标**是否真的存在"变成守卫 —— 运维文档 69 个名字全部对得上（**否定结论**），并修掉我自己那次**手审的假阳性**

第一百零九轮我踩过一次"getter 名 ≠ 序列名"（读到空序列，差点误判成"计数没接上"的 P1）。本轮把这一类做成守卫：**运维文档里出现的每个 `hydra_*` 指标名都必须能在代码里注册**。仓库里已有的 `check_alert_expressions.cjs` 只覆盖 §9.1 的**告警表**；而 §9 的 "Key series" 列表、`cluster.md` 的指标表、以及正文里顺口点名的序列**都没有守卫** —— 而这些恰好是运维**手工抄进面板**的名字。

### A. 手工审计：**否定结论 + 我自己的假阳性**
先手写脚本审计（dev-docs 全部 69 个名字 vs 代码注册表），第一版把 **`hydra_sni_host_mismatch_total` 报成"文档里写了、代码没注册"**。查下去发现**错的是我的审计**：该计数器在 `tls.rs` 里通过**常量**注册（`const MISMATCH_METRIC = "hydra_sni_host_mismatch_total";` + `prometheus::register_int_counter!(MISMATCH_METRIC, …)`），而且注册进的是**默认 registry** —— 正是 `/metrics` 用 `prometheus::gather()` 导出的那一个 ⇒ 指标存在且可见。**教训**：审计脚本必须认全三种注册形态（字面量 / 常量 / `Opts::new`），否则会对着**正确的文档**发警报。

### B. 守卫：`scripts/check_documented_metrics.cjs`（+ **10 条测试**）
- 只扫**运维面向**的三份文档（`ops.md`、`cluster.md`、`tenant-api-integration.md`），跳过 crate 名（`hydra_core` 等）与通配前缀（`hydra_listener_*` 这种以 `_` 结尾的）；
- 认全三种注册形态（含**常量解引用**一跳到字面量）；
- **故意不存在**的名字走显式白名单（4 个 `hydra_proxy_listener_*` 家族 —— 文档提到它们是为了说明**不存在**这个别名），白名单**每次运行都打印**，增长立即可见；
- 覆盖率下限 `CDM_MIN_CHECKED`（默认 20）：一个**什么都没查**的扫描必须是 exit 2 而不是 OK；缺文档、代码里一个注册都没有同样 exit 2；
- 报错形态带**文件:行号 + 最接近的候选名**。
**测试**覆盖：字面量/常量/`Opts` 三种形态各一条、拼错（须点名并给建议）、通配与 crate 名被跳过、白名单、缺文档 ⇒ 2、覆盖率不足 ⇒ 2、无注册 ⇒ 2。**其中"常量形态"那条正是本轮假阳性的回归测试**；测试本身也被覆盖率下限抓过一次（我那个只含通配/crate 名的夹具 ⇒ 0 个名字 ⇒ 正确报 CANNOT VERIFY），于是夹具里补了一个真名字。

### C. 真实树上的验证与证伪
真实树：**69 个文档指标名（3 份运维文档）全部对得上 55 个已注册序列**，4 个白名单名 —— 即"文档↔代码"在这一面**没有漂移**（本轮以否定收场）。**证伪**：在 `ops.md` 的 Key series 列表里把 `hydra_sni_host_mismatch_total` 改成 `…_totals`（一个字符）⇒ 守卫 **exit 1** 并打印
`DRIFT dev-docs/ops.md:1152: \`hydra_sni_host_mismatch_totals\` is not registered anywhere in crates/ … (did you mean hydra_sni_host_mismatch_total?)`
；`cp` 还原 + `cmp` 一致后 exit 0。 **验证**：CI `scripts` 作业新增两步、门禁新增 2 条 entry，`check_ci_wiring` exit 0；**门禁 81 项全绿、`OVERALL=GREEN`、exit 0**（`documented metrics exit=0`、`documented metric tests exit=0`；两份转写 `FAILED` 计数 0）。**本轮无产品代码改动**（新守卫 + 文档零漂移的确认）。

---

## 2cv. 第一百一十五轮：**副本保真**（边缘节点该知道什么）首次实测 —— 限流角色/租户令牌哈希/子租户路由**都在边缘且跨重启与 leader 死亡存活**；并抓出 **D-15**：`matching_key` 比的是**掩码**后的客户端 key（文档说原始 key）

`ops.md` §13 的"已修"清单里有一条关于**副本保真**的承诺："Disabled `limit_role` / `provider_key_binding` rows are not carried in config snapshots — after a failover they are lost from replicas. **FIXED**: the snapshot contract carries the full fidelity rows (including disabled ones, `provider_key` identity and tenant access-token hashes), so a promoted replica is byte-faithful." —— "byte-faithful"是一种**节点间线协议**的断言，可观察后果很具体：leader 死后，被提升的节点必须**仍然**执行该租户的限流、仍然认证它的租户令牌、仍然按子租户路由。新增 `integration/test_replica_fidelity.py`（**15 条断言**，真 leader + edge + 6380 Redis）。

### A. 实测（正向：这些都"过河"了）
| 腿 | 实测 |
|---|---|
| F1 | **启用**的限流角色在**边缘**执行：2 次 200 后 429 |
| F2 | 租户 API **访问令牌在边缘可用**（哈希随快照过河） |
| F3 | **子租户 + 路由**在边缘生效（`shared` 被钉到 p-b：4/4 `chatcmpl-b`） |
| F4/F5 | **重启边缘**（全新副本重新物化快照）后：限流仍执行、令牌仍认证、子租户仍引导 |
| F6 | **leader 被杀**后边缘继续服务（last-known-good 快照）且限流仍执行 |
| F7 | 运行中把**停用**角色改为启用（PUT + reload）⇒ leader 与边缘**都**开始执行（无需重启） |

### B. ★D-15：`matching_key` 比的是**掩码**后的 key（文档写成原始 key）
`design.md:989` 写"`matching_key` 为 NULL **或等于**客户端 api-key"，而 `proxy.rs:838`（原引用 `:834` 已陈旧） 把上下文建成 `MatchCtx { api_key: Some(&mask_key(&api_key)), … }`。**实测三向**：
- 角色的 `matching_key` 写**原始** key ⇒ **永不触发**（4 次请求全 200；leader 与 edge 都一样）；
- 写该 key 的**掩码** ⇒ 生效（2 次 200 后 429）；
- **掩码只保留首/尾** ⇒ **两个不同的客户 key 掩码相同就共用一个配额**：`sk-fidelity-limited` 与 `skzzzzzzzzzzzzzzzed` 掩码都是 `sk***************ed`，前者的窗口耗尽后**后者的第一次请求即 429**（实测）。
⇒ 已记入决策表 **D-15**（① 改成比原始 key，但要先把 bucket 变成哈希，否则客户 key 会进 Redis key 名；② 保持掩码并改文档 + 给运维一条拿到掩码的路；③ 现状），并在 `ops.md` §4 补上实测说明（今天怎么用才对）、`design.md` 那句加实测标注；计划头部"待决策 13 → **14** 项"。**不擅自改代码**：两条修法都动对外语义（Redis key 内容/隐私面，或文档与运维流程）。

### C. 我自己的四个夹具错误（都当场纠正，其中三个是"控制腿"救的）
1. `matching_key: "LIM_"` 当**前缀**用 ⇒ 角色匹配不到任何东西（**匹配是精确相等**，`hydra_core::limit::dim_matches`）。因此加了"**leader 也必须执行同一角色**"的控制腿 —— 它当场证明**问题在我的角色**而不是副本保真。
2. 想用"角色 A 写原始 key、角色 B 写掩码"来分辨形态，却让两个角色**指向同一个 key** ⇒ 掩码角色的窗口被前一条腿耗尽，原始角色的 429 其实是**掩码角色**给的，什么都没测出来 ⇒ 改成**每个形态一个独立 key**（加一个"任何角色都不指向"的 key 作对照）。
3. 边缘腿复用同一 key ⇒ 窗口是 Redis 里**跨节点共享**的，边缘第一次请求就已经超限 ⇒ 给边缘腿一个**自己掩码**的 key 与角色。
4. F7 第一版把**原始** key 写进角色 ⇒ 又被 D-15 自己咬了一口（leader 也不执行）⇒ 改为掩码并**先断言 leader**。
5. 另一次证伪探针（把 `fidelity.limit_roles` 置空）**没有变红**，反而教会我机制：**启用**的角色走的是常规 config 载荷（`store.rs` 加载时已过滤 `enabled`），而 `fidelity` 载荷承载的是**含停用行**的全量 —— 所以"停用行是否过河"在 edge 上**没有活的可观测面**（edge 无管理面），那一半由 `cluster/content.rs` 的单元测试（`fidelity().limit_roles.len() == 2`）守着。这条边界已写进 drill 注释与本节。 **验证**：CI `live-deps` 作业新增一步、门禁新增 1 条 entry（需 6380 Redis + 集群特性集），`check_ci_wiring` exit 0（仓外工件 44 → **45**）；**门禁 82 项全绿、`OVERALL=GREEN`、exit 0**（`replica fidelity exit=0`；两份转写 `FAILED` 计数 0）。**本轮无产品代码改动**（文档 + 新 drill + 新决策项 D-15；探针已还原）。

---

## 2cw. 第一百一十六轮：把 D-15 的**文档侧**修掉（`matching_key` 两种形式都认）—— 顺带发现上一轮的"原始形式"腿**根本不可能变红**（drill 里没有任何角色指向那个 key）

承上一轮 §2cv-B：文档说 `matching_key` 等于**客户端 api-key**（原始 key），代码只比**掩码**，写原始 key 的角色永不触发。D-15 的两条修法里 ② 是"保持掩码 + 改文档"，但**改文档等于承认文档撒谎**；而 ① 的真障碍只是"bucket 会进 Redis key 名"。**两者并不冲突**：可以让**匹配**认两种形式、**计数桶继续用掩码** —— 对外语义只增不减（原先能触发的掩码角色照旧触发），原始 key **不进** Redis key 名、也不进指标标签。

### A. 改动（`hydra-core` + `hydra-server`，非破坏性）
- `hydra-core/src/limit.rs`：`MatchCtx` 新增 `pub api_key_raw: Option<&'a str>`（**只用于匹配**；`api_key` 仍是掩码值、仍是 bucket 的键），新增 `fn key_dim_matches(role_dim, ctx)` = `ctx.api_key == Some(wanted) || ctx.api_key_raw == Some(wanted)`；`match_roles` 的 key 维度改走它。`api_key_raw = None` 时行为与修复前**逐位相同**（老调用方无需改）。
- `hydra-server/src/proxy.rs`：两处 `MatchCtx` 都带上原始 key —— 路由前的 count 门 `api_key_raw: Some(&api_key)`（`proxy.rs:655` 的 `api_key` 就是 `extraction.key`，**原始**提交值），token 门 `api_key_raw: ctx.client_api_key.as_deref()`。测试构造点（`proxy/limiter.rs` 6 处、`redis/rate_limit.rs` 1 处）同步。
- 单测 `matching_key_accepts_the_raw_key_or_its_mask`（`crates/hydra-core/tests/limit.rs`）：同一角色，**原始**、**掩码**、**两者都不等** 三向断言（第三种必须不匹配，否则"匹配一切"也能骗过前两条）。`cargo test -p hydra-core --test limit` ⇒ 9 passed。

### B. ★上一轮那 4 个"全 200"里，有 **1 个是我的夹具**（这条比产品缺陷更值得记）
修好后重跑 drill，原始形式那条腿**仍然** `[200,200,200,200]`。我先怀疑二进制没更新（确认 `Compiling hydra-server`）、再怀疑 `api_key` 变量已被掩码（读 `proxy.rs:655`：它是 `extraction.key`，**原始**值）—— 都不成立。最后去读**种子行**：`integration/test_replica_fidelity.py` 的角色是 `r-shared`/`r-masked`/`r-masked-edge`/`r-off`，**没有任何行**的 `matching_key` 等于 `RAW_ONLY_KEY` —— 而腿上方的注释信誓旦旦写着"`r-raw` 在该 key 的原始形式上"、还有一句"`r-shadow` 是……对照角色"。**两个角色都只存在于注释里**。也就是说这条腿**无论产品怎么改都不会红**（在它自己的 key 上没有任何角色），它上一轮"测到 200"是**必然**，不是证据。
⇒ 教训升级（此前记过"守卫不可能失败就不是证据"）：**新写的腿必须能失败**。已修：补上 `r-raw` 角色（`matching_key = RAW_ONLY_KEY`），并**在 F0 增加前置断言** —— `GET /limit-roles` 读回后断言 F0b/F1 用到的三个 `matching_key` **都在且 enabled**（`matching_key` 是**精确**匹配，少一行会**静默**解除一条腿）。这条前置断言当场就把上一轮的缺陷变成**看得见的红**。

### C. 实测（正向 + 反向证伪）
- 原始形式角色（`r-raw` 独占 `sk-raw-only-probe-77`）：`[200, 200, 429, 429]`（limit 2）—— 文档描述的形式**可用**了；
- 掩码形式角色不变：`[200, 200, 429, 429]`；**掩码碰撞**后果仍在（`skzzzzzzzzzzzzzzzed` 首请求即 429）；
- F1–F7（边缘保真、重启、leader 死亡、运行中启用传播）全绿 —— **16 条断言**，exit 0；
- **反向证伪**：把 `key_dim_matches` 改回只认掩码（备份取自**修复后**的树，探针后 `cp` 还原）⇒ 重新 `Compiling hydra-core` + `Compiling hydra-server` 后：单测 `matching_key_accepts_the_raw_key_or_its_mask` **FAILED**（8 passed / 1 failed），drill 的原始形式腿 **FAIL**（`[200,200,200,200]`）而掩码腿/碰撞腿/F1–F7 **仍 PASS** ⇒ 这条腿确实只由该修复驱动；还原后 `grep -rn FALSIFY crates/ integration/ scripts/` 为空、单测回到 9 passed、drill 16/16 绿。
- 文档同步：`ops.md` §4 那段从"只认掩码（今天怎么用才对）"改写为"**两种形式都认**（附实测码串）+ 计数桶仍用掩码 + 掩码碰撞仍共配额"；`design.md` §10.1 那句从"⚠️ 与代码不符"改为"**已修复**：两种形式均可"。
- **D-15 收窄**（仍留在决策表，仍是 14 项）：① 匹配形式**已闭环**（本轮）；② 余下的是**计数桶是否也该按原始 key 计**（要先哈希，否则客户 key 进 Redis key 名 ⇒ 掩码碰撞的两个 key 共享配额这一点仍需运维用"一 key 一角色"绕开）。决策表 D-15 行随之改写。

**验证**：`cargo build --bin hydra --features server,cluster-redis,usage-clickhouse` 成功；`cargo test -p hydra-core --test limit` 9 passed；`HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380 cargo test -p hydra-server --features server,cluster-redis,usage-clickhouse --lib limit` 21 passed；`integration/test_replica_fidelity.py` 16/16（exit 0）。本轮**有产品代码改动**（`hydra-core/src/limit.rs`、`hydra-server/src/proxy.rs` + 7 处测试构造点），drill 增 1 条前置断言 + 种子补 1 行。

### D. 门禁两次 RED，两次都是我（第三次踩同一个坑 + 一次新的操作顺序坑）
1. **`cargo fmt --check`**：新写的 `assert_eq!(…, "raw role must match")` 是 rustfmt 要的多行形态 ⇒ 门禁第一次 RED。**教训第二次出现**（§2cd 记过一次）：`cargo build` 与全绿 drill 都**不会**因格式而红 ⇒ 改完 Rust **先跑 `cargo fmt --check` 再跑门禁**。本轮我确实先跑了 `cargo fmt --check`，但**是在启动门禁之后**才跑的 —— 发现红时门禁已在编译这棵树、且我正要修改源码 ⇒ 只能**杀掉门禁**（避免"边改边编"得到不可复现的结论）、`cargo fmt`、重跑。**新规则：格式检查必须在启动门禁之前完成，且门禁运行期间不改工作树。**
2. **`public claims`**：新增的 `matching_key_accepts_the_raw_key_or_its_mask` 让 `--features server` 下的 core 计数 **253 → 254**，而 `docs/index.html` 仍宣称 **805**（第三轮内第三次：§2ch 804、§2cn 805）⇒ 门禁第二次 RED。按守卫给出的路径 `--measure --write` 回写为 **806（254 core + 552 server）**，两处细目同步；重跑 `check_public_claims` 显示 `OK: advertised 806 == measured 806`。
   ⇒ **流程规则（写进这里以免第四次）**：**只要新增任何非特性门控的 Rust 测试（含 `crates/*/tests/` 里的集成测试），在启动整套门禁之前先跑一次 `node scripts/check_public_claims.cjs --measure --write`**，否则门禁必红在 `public claims` 且红因与改动无关。
3. **最终**：门禁 **82 项全绿、`OVERALL=GREEN`、exit 0**（两份转写 `FAILED` 计数 0；`replica fidelity exit=0`、`public claims exit=0`）；`cargo fmt --check` 干净；`grep -rn FALSIFY crates/ integration/ scripts/` 为空；用户 dev 集群未受影响（`docker ps`：**7 个运行中容器全部 healthy**，其中 `hydra-a/b/c` + 4 个 `hydra-local-*`；3 个 `hydra` 进程各由一个 `tini` 拉起，无一来自本仓库 `target/debug/hydra`）。

---

## 2cx. 第一百一十七轮：**派发两个只读 Oracle 子代理**（回到最初那条指令）+ 自己先修两处；子代理的两条最高危发现**我都独立复现了**——守卫能被**一个字符**永久关掉，以及"注释算接线"

主题分两半。**A 半（自查）**：上一轮的 `api_key_raw` 让 `MatchCtx` 开始承载**活凭据**，而它还 `#[derive(Debug)]` ⇒ 任何一次 `debug!(?ctx)` 都会把客户 key 写进日志（乃至 panic 消息）；改成**手写 `Debug` 并脱敏 `api_key_raw`**，加单测 `debug_never_prints_the_raw_client_key`（断言不出现原始 key、出现 `<redacted>`、且 model/tenant 仍可打印 —— 否则脱敏把诊断价值也一起删了）。同一轮还发现**我上一轮自己写的夹具是错的**：`tests/limit.rs` 里硬编码的 `const MASK = "sk-raw-for********67890"` 是 **23 字符、尾部 5 个字符**，而 `mask_key` 对 22 字符 key 的规则是首 10 后 4（`sk-raw-for********7890`，22 字符）—— 即那个常量**不是产品会产出的任何字符串**，第二条断言退化成"`api_key` 相等能匹配"（与既有的 T6.2 重复），改掩码宽度它照样绿 ⇒ 改为 `let mask = mask_key(RAW)` + `assert_ne!(mask, RAW)`，从此夹具跟着生产规则走（`crates/hydra-core/tests/limit.rs`）。

**B 半（对抗性复审，只读）**：按最初指令"派发 Oracle 子代理从多个角度分析源码"，本轮派了**两个只读**子代理（明确禁止改文件、禁止跑 cargo/docker）：①针对 `matching_key` 改动做安全/语义复审；②专门猎"**不可能变红的守卫**"。两份报告共 5 + 13 条发现。**我对其中最高危的两条各自独立复现**（子代理是分析、不是证据）：

### A. ★`scripts/check_source_purity.cjs`：一个 `'{'` 就能让"生产代码纯净"这条**公开断言**永久变绿（P1，已修）
`stripTestItems` 靠**逐行数花括号**确定 `#[cfg(test)]` 条目的结束位置，而 `stripComments` 只清注释与 `"…"`/`r#"…"#`，**不清字符字面量**。于是测试模块里一个 `const L: char = '{';` 让 depth 永远回不到 0 ⇒ **该 gate 到文件尾全部被抹空**，后面的生产代码**永不扫描**。**我自己数了一遍**（`.acceptance/round117/count-gates.cjs`，复用修复后的脚本函数）：全仓 **58** 个 `#[cfg(test)]` 条目中，**只有 4 个**条目之后仍有**生产**代码 —— `redis/mod.rs:37`（正是唯一 `Pool::new` 宿主）、`http.rs:1022`、`model.rs:355`、`cluster/replica.rs:234`。子代理报的"15 个"把"之后还有**其它测试条目**"的文件也算进来了（`main.rs` ×4、`sink.rs`、`tls.rs` 等），那些位置写 `'{'` 只会让守卫**提前停止扫描测试代码**，并不会掩盖生产代码 ⇒ 采用的是**我实测的 4 处**这个数（这也说明子代理的清单要复核后再写进结论）。
**我的独立复现**（真夹具、子进程跑真脚本，`.acceptance/round117/s1/probe.cjs`）：同一份 `lib.rs`，违规 `.unwrap()` 在测试模块**之后** —— 控制组（无字符字面量）**exit 1 抓到违规**；同一违规 + 测试模块里加一行 `const L: char = '{';` ⇒ **exit 0、stderr 为空**。**修复**：`stripComments` 增加字符字面量分支（`'\n'`/`'\''`/`'\\'`/`'\xNN'`/`'\u{…}'`/`b'{'` 都覆盖），并且**必须**有闭合引号才当作字面量 —— 否则 `&'a str` 这种生命周期会被误吞。**3 条新测试**：`'{'` 不得掩盖其后的生产违规（含控制组）、`'}'` 不得提前结束条目而把**测试代码**报成生产违规、生命周期与转义引号不被误判。**反向证伪**：把字符分支关掉 ⇒ 恰好 **2 条新测试变红**（`pass 21 / fail 2`），其余 21 条照过；还原后 **23/23**、真树 exit 0。

### B. ★`scripts/check_ci_wiring.cjs`：**注释**算接线（P1，已修）—— 104 轮只修了 rule 1
`namedInSteps` 是 `st.run.includes(base)`、rule 4 是 `cmd.includes("--ignored")`，而 `extractSteps`/`extractRunCommands` **保留** YAML 注释与 block scalar 里的 shell 注释。于是 ①新建的 `integration/test_zzz_probe.py` **只在注释里被提到**（`# TODO also run …`）就算"已接线"；②`cargo test … --test usage_query   # TODO re-enable --ignored` 让 `#[ignore]` 目标被判为"有人在跑" ⇒ 三个 `#[ignore]` 套件可以再次全部无人运行而守卫打印 OK —— 正是 104 轮修掉的那类假绿换了个形态。**修复**：新增 `stripComments(text)`（对多行 `run:` 体逐行调用既有的引号感知 `stripInlineComment`），并让 `commands`、`namedInSteps`、`executedScripts` 全部走它。**3 条新测试**：只在注释里被点名的 drill 必须报"never executed"、`--ignored` 只出现在注释里必须报"no step runs … --ignored"、以及**控制组**（同一行去掉注释 ⇒ 通过）。
**★我自己的夹具错误（控制组当场抓住）**：第 20 例第一版用 `boot_listeners` 当靶子，而 `baseCi()` **本来就**有一行真跑 `--ignored` 的步骤 ⇒ 该例"失败"了 —— 不是守卫没修好，而是**靶子已被别处喂饱**（与 §2cv-C 的第 2、3 条同型）。改用一个 base CI 从不跑、只在注释里被点名的目标（`usage_query`）后才真正有判别力。**反向证伪**：把两处 `stripComments` 去掉 ⇒ 恰好 **2 条新测试变红**（`2 CI WIRING TEST(S) FAILED`），真树仍 exit 0（证明没有误伤）。

### C. 子代理的另一条高危（P2，**只做文档缓解**）：`matching_key` 是全仓**唯一明文存活**的客户端凭据列
上一轮我把"原始形式可用"写进 `ops.md`/`design.md`，等于**教运维把活 key 明文贴进配置**。子代理列出四跳，我**逐条只读核对为真**：`db.rs:1288/1311` 是裸列（**没有** `kp.seal`，而同文件 `db.rs:568` 的 provider api-key 是 `kp.seal(...)`）→ `cluster/snapshot.rs:119` 的 `FidelityWireRows.limit_roles` 是**明文** `LimitRole`（同一载荷里 `sealed_provider_keys`/`tenant_token_hashes` 是密封的）→ `db/restore.rs:242` 原样落库到**每个** edge → `GET /api/v1/limit-roles` 原样回显（我这轮新加的前置断言读的就是它）、admin UI 直接渲染。**本轮的处理**：`ops.md` §4 改为**推荐掩码形式**并写明上述四跳（原始形式仍可用但只在接受暴露时用），`design.md` §10.1 同步；是否**封存该列**记为 **D-16**（封存要动"配置加载期匹配"的读路径 + 保真载荷 + restore，属产品决策）。

### D. 顺带修掉一条被掩码论证依赖的错误文档（F6）
`design.md` 两处写"客户端 api-key 仅取**前 4 + 后 4**"（`:979` 与 `:1472`），而 `mask_key`（`rewrite.rs:238`）实际是 **长度 ≥ 20 ⇒ 前 10 + 后 4**（22 字符的 key 会保留 **14** 个字符，不是 8）、6–19 ⇒ 前 2 + 后 2、< 6 ⇒ 全星；`ops.md` §4 本来就写对了。这条差异正好决定"用掩码就安全 / 掩码碰撞有多宽"的判断 ⇒ 两处按代码更正。

### E. 记录但**本轮未修**的子代理发现（供后续轮次排序，均已给 file:line 与复现路径）
`check_ci_wiring` 的 `TEST_GLOBS` 漏掉 `integration/check_*.py`（S3，两个现有 checker 不在发现集合内）；`check_redis_pool` 的行级 `//` 启发式可被 URL 里的 `//` 骗过、且 `Builder::build_pool`/`use … as P; P::new` 两条路径完全绕过 R1+R2（S4，后者是 fred 自己的 API，文本匹配的固有边界）；`check_documented_env` 的谓词是"某处出现过这个名字"而非"被读取"（S5）；`check_documented_defaults` 33 行里 **14 行 `????`** 从不比较、floor 由其余 19 行满足（S6，含 `HYDRA_ADMIN_ADDR`、`HYDRA_LISTEN`、`HYDRA_DB_URL`、`HYDRA_ROLE` 等运维真会照抄的行）；两个 metric 类守卫把**测试内注册**当作已注册（S7，实证 `metrics.rs:1487` 的 `hydra_unused_test_marker` 能被认成"已存在"）；两个 compose 守卫在 CI 上**永远跳过** local stack 而 floor 恰好等于剩下 4 个服务（S8）；`check_e2e_contracts` 的 timestamp 规则只认"字面对象 + 80 字符内"一种形状、pkill 扫描无计数无 floor（S9）；`check_i18n` 只扫 `admin-ui` **顶层**且所有计数无 floor（S10）；`check_public_claims` 的日期只做**两个 locale 互比**、从不与事实比（S11）；`check_alert_expressions` 的 label 选择器计数无 floor（S12）；`check_ci_wiring` 的 `SKIP_DIRS` 让 `admin-ui/`、`bin/`、`tests/**` 的新工件既不检查也不计数（S13）。语义侧：原始形式角色 + `matching_tenant = NULL` ⇒ **跨租户共享窗口**且 `config::validate` 不告警（F2，配置层可加 Warn）；`bucket_for` 只看 `api_key`，一个"只给 raw"的 ctx 会让 bucket 退化成空串（F3，今天不可达但无断言固定）；手工 `Debug` 只脱敏 `api_key_raw`，`api_key` 仍靠"调用方传掩码"的纪律（F4）；`limiter.rs`/`rate_limit.rs` 里 7 处 `api_key_raw: None, // tests: same value…` 的注释与该处 `api_key` 也是 `None` 的事实不符，正掩盖了"limiter 的 key 分支零覆盖"（F5）。

### F. 本轮我自己的一个**编辑事故**（当场修好，值得记）
给决策表插 `D-16` 时，我用 `edit` 把新行**替换掉了 `D-6` 行的开头**（`old_string` 取了 D-6 行的**前缀**），结果 D-6 只剩尾巴、表格当场破损；读回发现后按行号把前缀补齐。**教训**：决策表这类"每行都长得像"的文件，`old_string` **必须整行**（或以行首 `| D-x |` 为锚），绝不能只取一行的前半段 —— 否则"替换"会静默变成"删前缀"。

**验证**：`node scripts/check_source_purity.cjs` 真树 exit 0（63 文件、3 crate root、clean）；`node --test scripts/check_source_purity.test.cjs` **23 passed / 0 failed**；`node scripts/check_ci_wiring.cjs` exit 0（45 仓外工件）；`node scripts/check_ci_wiring.test.cjs` **ALL PASSED**；`cargo test -p hydra-core --test limit` **10 passed**；`HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380 cargo test -p hydra-server --features server,cluster-redis,usage-clickhouse --lib limit` **21 passed**；`cargo build --bin hydra --features server,cluster-redis,usage-clickhouse` 成功；`cargo fmt --check` 干净（**在启动门禁之前**完成，遵 §2cw-D 的新规则）；`check_public_claims` 因新增 core 单测 254 → **255**，按新规则**先** `--measure --write`（807 = 255 core + 552 server）**再**起门禁。**门禁 82 项全绿、`OVERALL=GREEN`、exit 0**（`ci wiring`/`ci wiring tests`/`source purity`/`source purity tests`/`public claims`/`replica fidelity` 全 exit=0；两份转写 `FAILED` 计数 0）。**未提交**（`git status` 约 200 项）。

---

## 2cy. 第一百一十八轮：按 §2cx-E 的清单**继续清"不可能变红的守卫"** —— 修掉 3 条（发现集漏 `check_*.py`、fred 连接池的两条绕过、**测试内注册被当成线上指标**），并把 Rust 文本扫描器收成**单一所有者**

承上一轮：子代理的守卫审计清单有 13 条，本轮挑其中**能立刻闭环**的 3 条（S3/S4/S7）动手，每条都先自己复现。

### A. S3 —— `check_ci_wiring` 的发现集漏掉 `integration/check_*.py`
`TEST_GLOBS` 认 `test_*.py`/`*_test.py`，**不认 `check_*.py`**，而本仓真有两个这样的 checker（`integration/check_api_docs.py`、`integration/check_error_contract.py`）⇒ 它们**从不被发现**，也就从不被检查"有没有人跑"；新加一个同形的 checker 会静默加入这个盲区。**实测**：改前发现集 **45** 个仓外工件、两个名字都不在清单里（`grep` 它们为 0 命中）；加上 `/^check_.*\.py$/` 后 **47** 个、两个都在，且**守卫仍 exit 0** —— 即它们**确实被接线**（`run-crud-local.sh`（CI 的 `integration` 作业跑）→ `check_error_contract.py` → `import check_api_docs`，正是 104 轮那条链的两跳）。**新测试 2 条**：无人跑的 `integration/check_orphan.py` 必须报"never executed"、同文件被某步点名则通过（控制组）。**反向证伪**：把 glob 换回旧集合 ⇒ 第 22 例红（且发现集回到 45，"orphan" 根本没被发现）。

### B. S4 —— `check_redis_pool`：行级 `//` 启发式 + fred 的两条绕过
(i) **复现**（真夹具、`CRP_*` 指向临时树）：owner 之外再建一个池，**同一行**带 URL（`Config::from_url("redis://127.0.0.1:6379")`）⇒ 守卫 **exit 0**；把 `//` 去掉的**同一份文件**⇒ **exit 1**。根因是 `src.slice(lineStart, idx).includes('//')` 这种纯文本、行内、无字符串感知的"是不是注释"判断。**修复**：改为**保偏移**的注释/字符串屏蔽（`blankCommentsAndStrings`），判据变成"这一段的字节在屏蔽后还是不是 `Pool::new`"。
(ii) **补两条绕过规则**（子代理指出，我核对 fred 10.1.0 源码为真）：fred 的 `Builder::build_pool(size)` 内部调 `Pool::new(config, Some(perf), Some(conn), self.policy, size)`，而 `Builder::default()` 的 `policy` 是 `None` ⇒ **`Builder::…build_pool(2)` 能建出一个永不再连的池，且文本里根本没有 `Pool::new`**；`use fred::…::Pool as P;` 则让 `P::new(...)` 对文本搜索彻底隐形。新增 **R1b**（owner 之外的 `\.build_pool\s*\(` 即违规）与 **R1c**（任何 `Pool as <别名>` 即违规）—— 实测真树**没有**这两类站点（`.build_pool(` 只出现在我们自己的自由函数与测试调用里，无 `Pool as` 导入），所以两条规则今天只起"不许再出现"的作用。**新断言 6 条**（15 条总数），含 URL 违规、两个控制组（无 URL 的同形违规 / 注释里的 `Pool::new` 不算调用）、`.build_pool(` owner 外违规 + owner 内通过、别名违规。**反向证伪**：把注释判定换回旧的 `//` 启发式并把两条新规则改成空循环 ⇒ **恰好 3 条新断言红**（其余 12 条照过）。

### C. ★S7 —— 两个指标守卫把**测试内的注册**当成线上序列（`hydra_unused_test_marker`）
`check_documented_metrics`（扫 `crates/**`）与 `check_alert_expressions`（扫 `crate/src`，含 `#[cfg(test)]` 块）都把 `register_*!` 宏当"已注册"，于是 `admin/metrics.rs` 测试模块里的 `register_int_counter!("hydra_unused_test_marker", "test")` **算一个存在的序列** —— 一个"只在测试里存在"的指标名与"根本不存在"一样永远无法 resolve，恰恰是这两个守卫声称要防的事。**修复（顺手把扫描器收成单一所有者）**：新增 `scripts/rust_blank.cjs`，把原先长在 `check_source_purity.cjs` 里的 Rust 文本屏蔽器搬出来，并做成**两种模式**：
- `stripComments(text)`：注释**与**字符串/字符字面量内容一起屏蔽（保偏移、保行号）——给"数括号/找违规"用；
- `stripCommentsOnly(text)`：只屏蔽注释、**保留字面量内容**——给"从字面量里读名字"用；
- `stripCommentsAndTestItems(text)`：在**全屏蔽文本**上算 `#[cfg(test)]` 条目边界，再**按行**施加到"只屏蔽注释"的文本上 ⇒ 注释没了、测试条目没了、**字面量留着**。
`check_source_purity.cjs` 改为 `require('./rust_blank.cjs')`（自带 23 条测试仍全绿）。
**实测（真树）**：`check_documented_metrics` 的"已注册序列" **55 → 54**（正好少掉那个测试专用标记），而**文档里 69 个名字全部仍然解析**；`check_alert_expressions` 仍 17 refs / 6 label selectors 全解析、exit 0。⇒ **负结论（有价值）**：**没有任何被文档点名的指标只存在于测试里**（否则这一改会立刻变红）；那一面今天没有漂移。
**新断言**：metrics 守卫 4 条（测试内注册不算 + 控制组"测试模块外的同一字面量能解析" + `crates/<crate>/tests/` 下的注册也不算 + 只出现在注释里的注册不算）= **14 条**；alert 守卫 2 条（`#[cfg(test)]` 内的告警指标被报出 + 控制组）。**反向证伪**：让两个守卫重新读原文 ⇒ metrics 恰好 **2 条红**、alert **1 条红**，控制组都照过。
**★我在这条改动里自己捅的一个洞（当场抓住）**：第一版让两个守卫直接用"连字面量一起屏蔽"的 `stripComments` ⇒ 注册名 `"hydra_x"` 被抹成空格 ⇒ `check_documented_metrics` 立刻报 `CANNOT VERIFY: no registered metric found`（exit 2）。这不是"修好了"，而是把守卫推成了恒 `CANNOT VERIFY`；两种模式的拆分正是为此。**教训**：屏蔽文本时要区分"给人看代码结构"和"从字面量里取证"两种用途，别用同一份屏蔽结果干两件事。

### D. 我自己的第二个失误：多探针证伪后**没把每个文件都还原**
第 22 例（`check_*.py` 那条）第一次"失败"，我差点当成守卫没修好 —— 实际是**上一批五枚探针里 `check_ci_wiring.cjs` 那一枚还留在树上**（我只还原了 redis_pool/doc_metrics/alerts/rust_blank 四个）。确认方式很便宜：`grep -c FALSIFY <file>` **逐文件**核对，而不是只在整个 `scripts/` 上 grep 一次就放心。**规则**：一次植入 N 枚探针，还原后必须**逐文件**确认 `FALSIFY` 计数为 0，再解释任何红色。

**验证**：五个守卫在真树上全部 exit 0（`check_source_purity` 63 文件/3 crate root/clean、`check_ci_wiring` **47** 仓外工件、`check_documented_metrics` 69 名/54 序列、`check_alert_expressions` 17 refs/6 selectors、`check_redis_pool` 119 文件）；测试：purity **23**、wiring **ALL PASSED**（新增 3 例）、redis-pool **15**、doc-metrics **14**、alert **ALL PASSED**（新增 2 例）；`grep -rn FALSIFY crates/ integration/ scripts/` = 0。**门禁 82 项全绿、`OVERALL=GREEN`、exit 0**（`ci wiring`/`ci wiring tests`/`source purity`/`source purity tests`/`documented metrics`/`alert expressions`/`fred pool single owner`/`fred pool guard tests` 全 exit=0；两份转写 `FAILED` 计数 0；`check_public_claims` 仍 **807**（255 core + 552 server），本轮只加 JS 测试因此无需改计数）。

---

## 2cz. 第一百一十九轮：**S5 + S6 收口** —— "文档承诺的开关真的被读了吗"与"文档承诺的默认值真的对得上吗"，两条守卫各自都有一个"看起来在工作、其实什么都没验证"的洞

承 §2cx-E 的清单，本轮清 **S5**（`check_documented_env`）与 **S6**（`check_documented_defaults`）。两条都是"守卫声称的判据比它实际做到的强"，各自先复现再改。

### A. S5：证据从"某处出现过这个名字"改成"**真的被读**"
原谓词 `collectLiterals` 收集**任何**大写标识符（注释已去、字符串保留）⇒ ① compose 里的 `environment: HYDRA_FOO: "1"`（**写入**）算证据；② `const X_ENV: &str = "HYDRA_X";`（**只是名字**）算证据；③ 甚至 `dev-docs` 之外的任何 yml 都算。**隔离复现**（真实 ops.md 太大不能隔离；做最小夹具：一行表格 + 一个 compose 赋值 + 没有任何读取它的代码）：**旧守卫 exit 0**（打印 OK），**新守卫 exit 1**（点名 `HYDRA_FOO_PROBE`）；控制组：把同一名字改成 `std::env::var("HYDRA_FOO_PROBE")` ⇒ exit 0。
**新的读证据**（三种形态，都是本仓真实用到的）：
1. **直接读**：`env::var("X")`/`env::var_os("X")`/`env!("X")`/`option_env!("X")`、`os.environ["X"]`/`os.environ.get("X")`/`getenv("X")`、`process.env.X`/`process.env["X"]`、`os.Getenv("X")`、shell `${X}`/`$X`；
2. **名字作为完整字面量实参**传给 helper：`env_positive_u32("HYDRA_TENANT_API_LOCKOUT_SECS", 900)`、`version_from_env("HYDRA_ENCRYPTION_KEY_VERSION", 1)` —— 本仓多数开关都走这种"把变量名当 `&str` 传"的 helper，只认内联 `env::var("X")` 会把它们全报成未读（我第一版的窄谓词实测报了 **15 个**假红，其中 9 个是这类 helper 调用）。要求字面量后面紧跟 `,`/`)`，所以 `warn!("HYDRA_X is unset")` 这种"消息里以名字开头"**不算**读；
3. **常量携带 + 一跳**：`const TLS_LISTEN_ENV: &str = "HYDRA_TLS_LISTEN";` 且该常量在某处**作为调用实参**被使用（`validate(TLS_LISTEN_ENV, v)`）—— 只有声明不算读；只用它拼日志消息也不算。
另加 **`READ_BY_DEPENDENCY`**（`RUST_LOG`：由 tracing 的 `EnvFilter::from_default_env()` 读取，本树里没有这个字面量），**每次运行都打印**，因此这张表不能悄悄变长。
**真树负结论**：44 个文档名字**全部**有读证据 ⇒ 这个洞是**潜在的、不是现存的**（今天没有任何"文档承诺但没人读"的开关）。
**测试 11 → 19 条**：新增"常量被使用才算读""常量声明但未使用不算读""名字作为 helper 实参算读""只在日志消息里出现不算读""Python/JS 读算证据"。**反向证伪**：把这套新测试跑在**旧谓词**上 ⇒ 恰好 **3 条红**（含那条 compose 赋值的回归用例）。

### B. S6：默认值守卫的两个洞（非数值默认从不比较；"无法比较"是静默的）
实测改前状态：**19 条比较通过、14 条 `????`**，而 floor 只要求 10 —— 也就是说**14/33 行的默认值根本没被比较**，其中包含 `HYDRA_ADMIN_ADDR`（文档写 `127.0.0.1:8081`、并且明文写着"**Bind loopback only**"）与 `HYDRA_LISTEN`（`0.0.0.0:8080`）：它们是**地址**而守卫只懂数字，所以把 `DEFAULT_ADMIN_LISTEN` 改成 `0.0.0.0:8081`（把管理面暴露到所有网卡）**不会**触发任何红。
**修法一：字符串默认值也参与比较**（新增 `documentedStringDefault` + `codeStringDefault` + `stringConstValue`）：文档单元格里的反引号字面量（`` `127.0.0.1:8081` ``、`` `sqlite:hydra.db?mode=rwc` ``、`` `sqlite` ``）对代码里的 `const …: &str = "…"`（或 `unwrap_or("…")`）比较。**结果：19 → 23 条比较通过、14 → 10 条无法比较**；实测四条新纳入比较：`HYDRA_ADMIN_ADDR`、`HYDRA_LISTEN`、`HYDRA_DB_URL`、`HYDRA_USAGE_SINK`（都核对过代码常量确实存在且相等）。
**修法二：无法比较的行变成"**记录在案的决策**"**：每行必须出现在 `UNVERIFIED_OK`（含理由，每次运行打印）；**没记录 ⇒ FAIL**；**记录已过期**（那行其实已可比较/已不在文档里）⇒ FAIL（只在检查**仓库自带表格**时强制，夹具不会误伤）。剩下 10 行的理由都是"行本身的性质"（密钥无默认值、文档默认是 `unset`、开关是布尔/词法解析），不是"守卫偷懒"。
**反向证伪三枚**：①把文档里 `HYDRA_ADMIN_ADDR` 改成 `0.0.0.0:8081` ⇒ **DRIFT** 并同时打印文档值与 `DEFAULT_ADMIN_LISTEN` 的值与位置；②删掉一条 allowlist 项 ⇒ **DRIFT**“not recorded in UNVERIFIED_OK”；③把字符串通路关掉 ⇒ 两条新字符串用例**红**（且回显旧行为：`code=127` 这种**错误的数字抽取**，正是 S6 的原始症状）。

### C. 我自己这一轮的三个失误（都当场被测试/真树抓住）
1. **staleness 检查第一版无条件执行** ⇒ 夹具文档（1–2 行）里那 10 个 allowlist 名字全被判"过期"，一口气把**两条既有夹具测试**打红。修法：只在检查**仓库自带** `dev-docs/ops.md` 时强制，夹具里降级为 `note`。
2. **`opts.docs` 引用了一个不存在的变量**：`main()` 里根本没有 `opts`（局部变量叫 `ops`，配置常量叫 `OPS`）⇒ 整个守卫在真树上以 `ReferenceError: opts is not defined` 崩掉。**这次是"两处同时报红"救了我**：夹具测试全红 + 真树打印堆栈 ⇒ 教训：**改完守卫要同时跑"守卫本身"和"它的测试文件"**（同一个崩溃在测试里只表现为 exit 1，在真树上是堆栈）。
3. **`stringConstValue` 第一版把整个表达式当常量名**（`DEFAULT_ADMIN_LISTEN.to_string()` 被去掉非字母数字后成了 `DEFAULT_ADMIN_LISTENtostring`）⇒ 两条地址行即使加了字符串通路**仍然 `????`**。修法：先取**前导标识符**再解析常量。

**验证**：`check_documented_env` 真树 exit 0（44 个名字全有读证据，`RUST_LOG` 走依赖豁免并被打印）；测试 **19 passed / 0 failed**；`check_documented_defaults` 真树 exit 0（**23 条比较通过、10 条已记录**）；其测试 **ALL DOCUMENTED-DEFAULT TESTS PASSED**；两枚探针已还原（`grep -c FALSIFY` 逐文件 0）。**门禁 82 项全绿、`OVERALL=GREEN`、exit 0**（`documented env wired`/`documented env tests`/`documented defaults`/`documented default tests`/`ci wiring`/`source purity`/`public claims` 全 exit=0；两份转写 `FAILED` 计数 0）。

---

## 2da. 第一百二十轮：清 Oracle 复审的**语义侧四条**（F2–F5）—— 其中 F3 是一个"桶退化成空串 ⇒ 一个角色的所有客户端共用一个窗口"的**静默错杀**路径，且它此前**零测试覆盖**

§2cx-E 里除了守卫问题，还留着 5 条**产品语义**发现（F2–F5 + 已并入 D-16 的 F1）。本轮把 F2–F5 全部落地，每条都先复现/定性，再改，再反向证伪。

### A. ★F3：`bucket_for` 的 key 分量会退化成 `""`（两个 limiter 各有一份，且**没有任何测试走到那一分支**）
两个 `bucket_for`（`proxy/limiter.rs:298`、`redis/rate_limit.rs:402`）都写 `parts.push(ctx.api_key.unwrap_or(""))`。而上一轮（§2cw）刚给 `MatchCtx` 加了 `api_key_raw`，其文档明确说"只给 raw 也合法" ⇒ 那种 ctx 下 key 分量变成 **空串** ⇒ 该角色的窗口变成 `(role_id, "")`，**所有客户端共用一个配额**：第二个客户端的第一次请求会被第一个客户端耗尽的窗口拒掉（**无日志、无指标**）。子代理还指出这分支在 cargo 测试里**零覆盖**：`limiter.rs`/`rate_limit.rs` 的 7 个夹具**都是** `api_key: None` 且 `api_key_raw: None`，永远走 `unwrap_or("")`。
**修法（单一所有者）**：`hydra-core` 新增 `pub fn bucket_key(ctx) -> String` —— 有掩码就用掩码；**只有 raw 时把 raw 掩码**（因此永不空串，且 raw key 不会进 bucket 名，集群模式下 bucket 就是 Redis key 名）；两者都无才返回空（那条路径只对 `matching_key` 为 NULL 的角色可达，行为与改前一致）。两个 limiter 都改成调用它，`Vec<&str>` → `Vec<String>`。
**新测试（三层）**：core `the_key_bucket_keeps_its_identity_from_the_raw_key_too`（raw-only ⇒ 等于 `mask_key(raw)`、**非空**、**不含 raw**、两个客户端不同桶、有掩码时原样、两者都无时为空）；server `a_raw_only_context_shares_the_window_with_the_masked_form`（raw-only 与掩码形**落同一个窗口**：第一个放行后第二个必须 429 —— 这条同时钉住"掩码不分裂窗口"）与 `a_key_scoped_role_keeps_one_window_per_client`。
**反向证伪**：把 `bucket_key` 的 raw 分支改回返回空串 ⇒ core 测试 **FAILED**、server 测试也 **FAILED**（两层都抓得住）。

### B. F4：手工 `Debug` 只脱敏了 `api_key_raw`，`api_key` 靠"调用方纪律"活着
上一轮我把 `api_key_raw` 脱敏，但 `api_key` 原样打印，理由是"它的契约就是掩码"。子代理指出这正是**纪律而非不变量**：`api_key_raw` 存在的理由就是"调用方手上是原始 key"，将来任何一处把 raw 塞进 `api_key`，一次 `debug!(?ctx)` 或 panic 消息就会打印活凭据 —— 而上一轮的测试**看不见**这条路径（它的夹具用的就是真掩码）。
**修法**：`Debug` 里 `api_key` **只在它是 `mask_key` 的不动点**（即真的是掩码）时原样打印，否则打 `<redacted: api_key is not a mask_key value>`。测试加一段"把 raw 放进 `api_key`"的误用断言（既要**不出现** raw，也要**出现**那句说明 —— 静默脱敏会让下一个人以为是值本身长这样）。
**反向证伪**：改回直接打印 `self.api_key` ⇒ `debug_never_prints_the_raw_client_key` **FAILED**。

### C. F5：7 处测试注释撒了谎，正是它掩盖了 A 的覆盖缺口
`api_key_raw: None, // tests: same value, so matching is unchanged` —— 而这些夹具的 `api_key` **也是 `None`**，根本不是"同一个值"。注释已改为实话（"this fixture exercises key-less roles, so no raw key is needed"，`limiter.rs` 6 处 + `rate_limit.rs` 1 处），覆盖缺口由 A 的两条新 server 测试补上。

### D. F2：key 角色 + `matching_tenant = NULL` ⇒ **跨租户**预算，且 `validate` 对此一言不发
`config::validate` 对**永不匹配**的 `matching_provider` 早就有具名告警（D-11），但对"`matching_key` 有值而 `matching_tenant` 为 NULL"没有任何提示 —— 而窗口属于 `(role_id, mask(key))`、与租户无关 ⇒ 两个租户只要 auth 后端都接受同一个 key 串就**共用一个预算**，一个租户的流量能把另一个租户的第一次请求拒掉。这条在第一百一十六轮之后**从"死的"变成"活的"**（此前该形态角色根本不匹配任何人）。
**修法**：新增**具名 Warning**（不是 Error：共用预算可以是刻意设计，但不能是意外的、更不能不吭声），文案点名 role id 与后果。**测试**：`validate_key_scoped_role_without_a_tenant_scope_is_reported_as_cross_tenant` + **控制组**（把 `matching_tenant` 补上 ⇒ 不得再告警，否则这条告警等于对**所有** key 角色都响）。**反向证伪**：把条件改成 `false` ⇒ 恰好该测试 **FAILED**。文档：`ops.md` §4 新增那条运维规则（"key 角色务必设 `matching_tenant`"）并给出告警原文。

### E. 流程（本轮两次按前几轮写下的规则办事，都省下了一次重跑）
- `cargo fmt --check` 在**启动门禁之前**就红了（新测试的换行形态）⇒ 先 `cargo fmt` 再起门禁（§2cw-D 规则）。
- 新增测试让 `--features server` 的计数 807 → **811**（core 255 → **257**、server 552 → **554**）⇒ **先** `check_public_claims --measure --write` 再起门禁（§2cy-D 规则），门禁的 `public claims` 因此一次通过。

**验证**：`cargo test -p hydra-core --test limit` **11 passed**、`--test validate` **20 passed**；`cargo test -p hydra-server --features server --lib limiter` **9 passed**（含两条新测试）；`HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380 cargo test -p hydra-server --features server,cluster-redis --lib rate_limit` **6 passed**；`cargo fmt --check` 干净；三枚探针逐文件还原（`FALSIFY` = 0）。**门禁 82 项全绿、`OVERALL=GREEN`、exit 0**（`public claims`/`source purity`/`documented defaults`/`fred pool single owner` 等全 exit=0；两份转写 `FAILED` 计数 0；`docs/index.html` 的公开计数已回写为 **811（257 core + 554 server）**）。

---

## 2db. 第一百二十一轮：S8 —— 两个 compose 守卫在 CI 上**从来没检查过本地栈**（跳过 = 不检查），改成"**读文本也要查**"的静态回退

承 §2cx-E 的 S8：`check_compose_grace.cjs` 与 `check_compose_health.cjs` 都用 `docker compose config` 渲染三个 compose 文件；**本地栈**需要 gitignored 的 `secure/local-test.env`，而该文件**在 CI 上不存在**（`.gitignore:49 /secure/`）⇒ 渲染失败被当成"合理跳过"，于是 `hydra-a/b/c` 三个服务**在 CI 上从不被检查**；而 `MIN_SERVICES = 4` 恰好等于**另外两个能渲染的文件**里的 hydra 服务数 ⇒ 把本地栈的 `stop_grace_period`（或 healthcheck）删掉，CI 依旧打印 `OK … 4 hydra service(s) checked`。**这条承诺本身是实测出来的**：Docker 默认 10s 停等会把 ~25s 的 drain **SIGKILL** 掉（实测 20 个请求 ⇒ `usage_record` **0 行** vs 完整 drain 的 **20 行**），所以"跳过"等于把一条会用数据换来的结论静默丢在 CI 之外。

### A. 修法：单一所有者的**文本读取器** + 静态回退，且"查不到东西"必须失败
新增 `scripts/compose_static.cjs`（唯一所有者，两个守卫共用）：只解析顶层 `services:` 下两空格缩进的服务块，取出 `image:`、任意标量键（`stop_grace_period`/`HYDRA_ROLE`/`test`）与 mapping 键是否存在（`healthcheck:`）。**它的边界写在文件头里**：看不见 `extends:`/锚点/变量插值 ⇒ 静态路径**天生弱于**渲染，因此调用方必须①标注结果来自静态路径、②**找到 0 个 hydra 服务就直接 CANNOT VERIFY（exit 2）**，绝不能"什么都没找到 ⇒ 通过"。
两个守卫的 `main()` 里，"文件被跳过"分支改为：**若有文件路径 ⇒ 静态检查该文件**（并把跳过的原因与"已静态检查（较弱）"一起打印）；渲染出来的结果与静态结果**一起计入 floor**。health 守卫同时把**规则集抽成 `evaluateService()`**，两条路径共用同一套规则（edge 必须走免 token 的 `/healthz`、控制节点必须探 `/admin /api/v1/health` 且带 `Authorization: Bearer`），避免"两条路径各写一份规则、日后跑偏"。
**测试驱动用的开关**：两个守卫各加 `--static-file=FILE`（测试直接驱动静态路径，不碰 docker、不依赖环境）。

### B. 实测（真夹具，两个方向）
- **能抓到**：把 `environment/docker-compose.local.yml` 里三处 `stop_grace_period: 30s` 删掉 ⇒ 静态路径 **FAIL** 并逐个点名 `hydra-a`/`hydra-b`/`hydra-c`（"no `stop_grace_period`: docker … waits its 10s default, then SIGKILLs — the drain is cut short and buffered usage is lost"）；把 `hydra-c` 的 `healthcheck:` 子块整块删掉 ⇒ health 守卫 **FAIL**（`no healthcheck (nothing supervises this node)`）。
- **控制组**：未改动的真文件走静态路径 ⇒ 两个守卫都 exit 0（grace 报 `hydra-a (stop_grace_period=30s)`；health 报 `hydra-c (role=edge)`）；只含 `redis` 的假文件 ⇒ **exit 2**（`found no hydra-image service … refusing to pass by finding nothing`）。
- **我自己在写读取器时踩的坑**：`image:` 正则**漏了 `m` 标志** ⇒ 服务块是多行字符串，`$` 锚到整块末尾 ⇒ 只有 `image:` 恰好是**最后一行**时才匹配，三个 shipped 文件**全部报 0 个 hydra 服务**（若没有"找到 0 个就 exit 2"这条，它会静默变绿 —— 正是本轮要修的那类）。

### C. 我这一轮最值得记的失误：**探针备份取在了修复之前**
按前几轮写下的规矩我给探针做了备份，但备份命令与**修复脚本在同一个命令里、且 `cp` 在前** ⇒ 备份的是**修复前**的文件；探针跑完"还原"时，把**静态回退本身**一起还原掉了（测试立刻从 14/14、12/12 变成 11/3、9/3）。发现方式：还原后我照例重跑测试（而不是只看 `grep FALSIFY`），失败数就是线索。**修法**：按会话记录**重放**那两步变换（都带 `assert` 的确定性编辑），确认两条守卫恢复、`FALSIFY` 逐文件为 0、测试回到 14/14 与 12/12；随后**重新取一次备份（这次的时机是"修复后、探针前"）并重跑证伪**，两处各 2 条红、还原后仍全绿。**规则收严**：备份命令**不能**与修复命令写在同一条 shell 里（`&&` 顺序骗过我一次）；探针前必须 `cp` 一次**修复后**的树，并在还原后**跑测试**确认真回来了。

### D. 接线（新工件必须被 CI 执行 —— 这次是守卫自己提醒我的）
新增的 `scripts/compose_static.test.cjs`（5 条用例：块边界、只取匹配镜像、无 hydra 服务返回空、本地栈形状、**三个 shipped 文件合计 7 个 hydra 服务且每个都有 grace/healthcheck**）一落地，`check_ci_wiring` 立刻报 `UNWIRED scripts/compose_static.test.cjs is never executed by ci.yml` ⇒ 补 CI `scripts` 作业一步 + 本机门禁 1 条 entry（**82 → 83 项**）。`check_ci_wiring` 随后 exit 0（47 个仓外工件不变）。

**验证**：两个守卫真树 exit 0；测试 `compose grace` **14 passed**、`compose health` **12 passed**、`compose static reader` **5 passed**；`check_ci_wiring` exit 0；`check_public_claims --measure` 仍 **811**（本轮无 Rust 测试增减）；探针逐文件还原（`FALSIFY` = 0）。**门禁 83 项全绿、`OVERALL=GREEN`、exit 0**（新增 entry `compose static reader tests exit=0`；`compose stop grace`/`compose grace tests`/`compose healthchecks`/`compose health tests` 全 exit=0；两份转写 `FAILED` 计数 0）。

---

## 2dc. 第一百二十二轮：S9 + S12 —— "时间戳必须随 POST 一起发"那条规则只认**一种实参形状**（换个写法就溜过去），pkill 扫描与告警 label 选择器**各自没有 floor**

承 §2cx-E 的清单，本轮清 **S9**（`check_e2e_contracts.cjs`）与 **S12**（`check_alert_expressions.cjs`）。

### A. S9-A：时间戳规则认"实参形状"，不认"调用" ⇒ payload 放进变量就整条溜掉
原正则要求 `api('POST'|'PUT', 'path', … body: {` **80 字符内**出现**内联对象字面量**。**实测复现**（`--spec` 指向夹具；注意 `--spec=FILE` 这种 `=` 写法**不被解析**、会静默退回扫真目录 —— 我第一次的两次探针就是这样白跑的）：控制组（内联 body、删掉 `created_at`/`updated_at`）⇒ **exit 1**；逃逸夹具（`const payload = {…}; api('POST','/providers',{ body: payload, ignored: { created_at: '' } })`，真 payload 里**两个时间戳都没有**）⇒ **exit 0**。
**修法**：改为匹配**调用本身** `api\(\s*['"](POST|PUT)['"]\s*,\s*['"]([^'"]+)['"]`，再取路径后的**花括号配平**选项对象，取其中的 `body:`：
- `body: { … }` ⇒ 内联对象（原行为）；
- `body: <ident>` ⇒ 在**同一文件**里解析 `const|let|var <ident> = { … }`（一跳）；
- **解析不出来 ⇒ 报 DRIFT 并说明原因**（"不是本文件里的对象字面量，无法判断有没有带时间戳 ⇒ 请内联或写成同文件 `const`"）—— 让逃生口**响亮**，而不是静默跳过。
另加两条地板：`payloadsRead == requiring`（**每一个**需要时间戳的调用都必须真被读过）与 `apiSites >= MIN_API_SITES`（默认 5；仓里今天 7 个调用）。**实测三形状**：变量形 ⇒ exit 1 **点名 `omits created_at`**（改前 exit 0）；`body: makeProvider()` 形 ⇒ exit 1（"not an object literal in this file"）；变量形但带时间戳 ⇒ exit 0（控制组）。
**顺带修掉一个把数字撑大的旧账**：原时间戳循环也在 `checked++`，而 `checked` 就是"selector/nav token"计数与 `MIN_TOKENS` 地板用的那个变量 ⇒ 报出来的 248 其实是 242 个 token + 6 个 payload；现在两者分开（`payloadsRead`），真值 **242**。

### B. S9-B：pkill 规则**没有任何计数**（目录一改名就静默空转）
那段 walk 只收集文件、没有计数也没有地板 ⇒ 把 `integration/` 改名或把 harness 搬走，规则会"什么都没扫到"却照样打印 OK。**修法**：`harnessScanned` 计数 + `MIN_HARNESS_FILES`（默认 30；真树今天 **86**）+ FAIL 文案（"the harness directories may have moved, so this rule proves nothing"）；OK 行现在把三个计数都打出来（token / api 调用与已核 payload / 已扫 harness 文件）。

### C. S12：告警守卫对 **label 选择器**没有地板
`MIN_REFS` 只限制"指标引用数"，于是把 §9.1 的表达式改写成不含 `{label="…"}` 的形状（如 `sum by (…)`）后，"label key 必须真实存在"这条规则会**静默变成空转**且仍打印 OK。**修法**：`MIN_LABELS`（默认 4；真树今天 **6**）不满足即 exit 1，并说明是"规则没被行使"而不是"表里没标签"。

### D. 我这一轮的两次手术事故（都是"编辑区域起点找错"）
1. 第一次替换用 `s.index("  for (const file of specFiles) {")` 作起点 —— 但 main() 里**更早**还有一个同名循环（token 规则那个）⇒ 我把 **STRUCT_OF_RESOURCE / modelSrc / requiredTimestamps 的整段解析**连同前面的 token 循环一起删掉了（报错 `STRUCT_OF_RESOURCE is not defined`）。
2. 第二次用**正则字面量本身**当锚点、再 `rindex` 往前找所属的 `for` 循环，写到 `.acceptance/round122/` 里先**打印行号与长度**确认区间（245→278、33 行）后才替换 ⇒ 成功。
**教训（与 §2db-C 同类但更具体）**：**同一个文件里出现多次的循环头不能当锚点**；替换前应把捕获到的区间**落盘并打印行数**，确认它正是要改的那一段。另外本轮所有探针备份都取自**修复后**（`.acceptance/round122/*.postfix`），还原后逐文件 `FALSIFY` = 0、测试全绿。

### E. 接线（新地板必须能失败）
`check_e2e_contracts.test.cjs` 新增 5 条：逃逸形（同文件 const）必须被查、不可读 payload 必须被报、控制组（带时间戳的同形）通过、`E2E_CONTRACTS_MIN_API_SITES=500` 必红、`E2E_CONTRACTS_MIN_HARNESS_FILES=500` 必红。测试夹具把两个新地板**显式置 0**（聚焦夹具只有 1 个调用），并在注释里写明"地板自己有独立用例，所以这里的覆盖不会把它关掉"（沿用 `MIN_TOKENS` 的既有纪律）。

**验证**：`check_e2e_contracts` 真树 exit 0（242 tokens / 7 calls / 6 payloads / 86 harness files）；其测试 **ALL E2E CONTRACT TESTS PASSED**；`check_alert_expressions` 真树 exit 0（17 refs / 6 labels），`CAE_MIN_LABELS=7` 必红（**证伪**：把地板条件改成 `false` ⇒ 同样命令 exit 0 ⇒ 地板确实在承重）；`check_ci_wiring` exit 0；`check_public_claims --measure` 仍 **811**（本轮无 Rust 改动）。**门禁第一次 RED**：`alert expression tests exit=1` —— 新加的 `MIN_LABELS` 把该守卫**自己的聚焦夹具**也判红（夹具里只有 1 条表达式），与我给 e2e 两条地板做的处理同型 ⇒ 在测试 harness 里把 `CAE_MIN_LABELS` 显式置 0，并**补一条控制组用例**（正常表达式满足地板），地板自身的判别力用 `CAE_MIN_LABELS=7` 单独验（必红）。修好后 **门禁 83 项全绿、`OVERALL=GREEN`、exit 0**（两份转写 `FAILED` 计数 0）。

---

## 2dd. 第一百二十三轮：**先查我自己上一轮写的代码** —— 放宽后的时间戳规则能被"字符串里的字段名"骗过（与被修过的 `'{'` 同一类），另清 S10

本轮开头不是接着清清单，而是**回头审 §2dc 自己刚写的东西**，结果第一条就命中了。

### A. ★我自己引入的洞：`created_at`/`updated_at` 只出现在**字符串**里也算"带了"（`note: 'created_at: never sent }'`）
§2dc 放宽后的规则用**朴素正则 + 朴素花括号计数**在**原文**上找字段：`\bcreated_at\s*:` 会命中**字符串内部**，而字符串里的 `}` 又会**提前结束**配平 ⇒ 一个 payload 只要在**提示文案**里提到这两个名字，就被判定为"带了时间戳"。**实测复现**（夹具：`note: 'created_at: never sent, updated_at: never sent }'`，真 payload 里没有这两个字段）⇒ **exit 0**。这与第一百一十八轮 `check_source_purity` 被一个 `'{'` 打哑是**同一类**（守卫读文本时不认字面量）。
**修法**：新增**保偏移**的 `maskJsLiterals()`（注释、单/双引号与模板字符串**内容**屏蔽、引号本身保留），结构搜索与字段判定**全部在屏蔽后的副本上**做（偏移一致，索引运算不受影响）。这带来两个必须同时处理的对称风险：
- **引号内容被屏蔽后 path 读不出来了** ⇒ 方法/路径改从**原文**按**同一偏移**切回：用 `d` 标志拿到 `m.indices`，在原文里切出 `POST`/`/providers`；
- **注释里的调用**若只在原文匹配，就会变成"读不到选项对象"的假 DRIFT ⇒ 现在**调用匹配也走屏蔽副本**（注释内调用**不是调用**），实测：把一行 `// await api('POST', '/providers', { body: { id: '' } });` 放进夹具 ⇒ **exit 0**（正确地忽略）。
**实测四形状**：字符串里提字段 ⇒ **exit 1 点名 `omits created_at`**（改前 0）；注释里的调用 ⇒ 0；变量形（真缺字段）⇒ 1；带时间戳的变量形 ⇒ 0。真树仍 242 tokens / 7 calls / 6 payloads。
**★我自己在写测试时又犯一次同类错**：逃逸夹具第一版在 `ignored:` 后面**又放了一行真的 `created_at: ''`** ⇒ 用例"通过"了但什么都没测（因为真字段确实在 code 里）。把夹具改成与手工探针一致后才真正红 ⇒ 再次印证"**夹具本身要先能失败**"。

### B. S10：i18n 扫描只读 `admin-ui/` **顶层**，且**没有任何 floor**
`fs.readdirSync(adminUiDir)` 非递归 ⇒ 一旦 UI 模块落到子目录，它的 `t("brand.new.key")` 就**完全不被检查**（页面会直接显示原始 key）；同时 `issues.length === 0` 就 exit 0 ⇒ 扫到 0 个文件也会打印 `OK (0 en keys, …)`。
**修法**：递归遍历（`*.js`（除 `i18n.js`）+ `*.html`）+ 三条 floor（`I18N_MIN_FILES`/`I18N_MIN_EN_KEYS`/`I18N_MIN_T_KEYS`，默认 3/100/50；真树 **4 文件 / 348 en key / 139 个 `t()` 字面量**），不满足即 **exit 2 CANNOT VERIFY**；OK 行把文件数与 `t()` 数一并打印。`checkI18n()` 改为返回 `{ issues, enKeys, stats }`（**增量**，既有解构仍可用），floor 判定留在 CLI 层（导出函数不该 `process.exit`）。
**测试 3 条**：子目录里的缺失 key 必须被报（**证伪**：把递归改回不递归 ⇒ 该例 exit 0 且 `1 UI file(s)`；控制组同时变红，说明方向对称）、同布局但 key 齐全 ⇒ 0、`I18N_MIN_FILES=99` ⇒ **exit 2**（**证伪**：把 floor 条件改 `false` ⇒ 同命令 exit 0）。夹具 harness 把三条 floor 显式置 0（沿用既有纪律：floor 自己有独立用例）。

### C. 我这轮第三次"编辑落点"事故（同一个教训换了个样子）
给 `check_i18n.js` 插 floor 常量时，我用 `const adminUiDir` 这行当锚点 —— 而那一行**后面还有续行**（`= argDir ? … : …`）⇒ 我插在了三元表达式中间，直接 `SyntaxError: Unexpected token '?'`。**这次我没有将就**：先 `cp` 还原到修改前，再把常量放到**模块顶层**（`function main() {` 之前），并让 `main()` 从 `checkI18n()` 的返回值里取计数。**教训升级**：**锚点必须是完整语句或独立行，插入前确认它下一行不是续行**（§2dc 是"重复出现的循环头"，这轮是"跨行表达式"）。

### D. 与 §2dc 相同的处置：探针备份都取自**修复后**
本轮所有探针（e2e 屏蔽、i18n 递归、i18n floor）的备份都在**修复之后、探针之前**取（`.acceptance/round123/*.postfix`），还原后逐文件 `FALSIFY` = 0、测试全绿。

**验证**：`check_e2e_contracts` 真树 exit 0、其测试 **ALL PASSED**（新增 2 条）；`check_i18n` 真树 exit 0（348 keys / 4 files / 139 t()）、其测试 **ALL PASSED**（新增 3 条）；`check_ci_wiring` exit 0；`check_public_claims --measure` 仍 **811**（本轮无 Rust 改动）。**门禁 83 项全绿、`OVERALL=GREEN`、exit 0**（`i18n`/`i18n tests`/`e2e contracts`/`e2e contract tests` 全 exit=0；两份转写 `FAILED` 计数 0）。

---

## 2de. 第一百二十四轮：再回头审自己 —— §2cw 的"读证据"规则仍接受**测试夹具里的字符串**当证据（"某处出现过"换成"某个文件里出现过"而已）

续 §2dd 的做法：先审自己上一轮（§2cw/§2db 时期）写的规则。**实测复现**：造一棵**没有任何生产代码读它**的树 —— 只放**守卫自己的测试文件**（`scripts/check_documented_env.test.cjs`，其夹具字符串里含 `env::var("HYDRA_ADMIN_TOKEN")`）+ 两行 ops.md 表格 ⇒ 旧规则 **exit 0** 并打印"all have a READ site in code"。也就是说：**把产品里那行真读取删掉，这个守卫也不会发现**（该名字的证据来自夹具）。这正是 §2cx-S5 的同一类洞，只是从"任何文件"缩小到了"任何文件，包括测试"。

### A. 先量化，再修（避免拍脑袋）
用 `DOC_ENV_DUMP=1` 把**每个名字的全部证据点**（file:line + 形态）打出来，再按"是否测试工件"分类统计：真树 **44 个名字里 0 个**是"只有测试证据" ⇒ 这条洞在**今天**不成立（负结论），但它是**潜在**的、且守卫的措辞（"READ site in code"）在说谎。
**修法两件**：
1. **跳过测试工件**：`*.test.{cjs,js,mjs,ts}`、`*.spec.*`、`test_*.py`、`*_test.go`，以及路径中含 `tests/`/`test/` 段（`crates/*/tests/`、`tools/*/test/`、`tests/e2e/` 都覆盖）；
2. **Rust 文件里 `#[cfg(test)]` 条目整块屏蔽**（复用 `rust_blank.cjs` 的 `stripCommentsAndTestItems`）—— 单测里调 `env_positive_u32("HYDRA_X", 1)` 不是产品在读这个开关。
**结果**：真树仍 exit 0（44 个名字证据充足，与我事先的量化一致）；开头那棵"只有夹具"的树现在 **exit 1**。

### B. 一个被我试了又撤回去的做法（值得记，因为它是"修法自己制造假红"的实例）
我顺手把 JS/TS 也换成**字符串屏蔽**（复用 §2dd 抽出的 `maskJsLiterals`，并把它移到单一所有者 `scripts/js_blank.cjs`）—— 结果**控制组立刻变红**：`process.env["HYDRA_X"]` 与 `os.environ["HYDRA_X"]` **本身就是字符串字面量**，屏蔽字符串内容等于把该守卫要读的证据擦掉。⇒ 回退为"JS/TS 只去注释"，并把这条**残余**写进守卫头部：文档字符串里恰好拼出 `process.env.HYDRA_X` 仍会被算作读（要区分它需要真解析器，代价不成比例）。
**顺带**：`maskJsLiterals` 因此从 `check_e2e_contracts.cjs` 里搬进 `scripts/js_blank.cjs`（单一所有者，e2e 守卫改为 require；它的行为由 e2e 的"字符串里的字段名"用例与 §2dd 的记录共同钉住）。
**教训**：给守卫"加严"时，**先跑控制组**——我这次的加严差点把真证据一起删掉，而控制组是唯一能立刻发现它的东西。

### C. 测试（4 条，含控制组）与证伪
新增：①只在**测试工件**里出现 ⇒ 必须 exit 1（**证伪**：把跳过逻辑改成 `false` ⇒ 该例红）；②控制组：同样的写法落在**非测试**代码里 ⇒ exit 0；③`#[cfg(test)]` 里的 helper 调用 ⇒ exit 1（**证伪**：把 `.rs` 的屏蔽换回只去注释 ⇒ 该例红）、④控制组：同样的调用在测试模块**之外** ⇒ exit 0。
**我写夹具时的又一个小错**：三条用例第一版都把 `HYDRA_ADMIN_TOKEN` 也放进文档表格，而夹具树里没人读它 ⇒ 期望 exit 0 的控制组其实因为**另一个名字**而红。改成**单行表格 + `DOC_ENV_MIN_ROWS=1`** 后，每条用例的成败只由被测机制决定。

**验证**：`check_documented_env` 真树 exit 0；其测试 **22 passed / 0 failed**；开头那棵"只有夹具"的树从 0 变 **1**；`check_e2e_contracts` 真树 exit 0 + 测试 ALL PASSED（改用共享 masker 后无回归）；`check_ci_wiring` exit 0；`check_public_claims --measure` 仍 **811**。**门禁 83 项全绿、`OVERALL=GREEN`、exit 0**（`documented env wired`/`documented env tests` 等全 exit=0；两份转写 `FAILED` 计数 0）。

---

## 2df. 第一百二十五轮：把 §2da 的"**文档承诺**"端到端跑一遍 —— 跨租户告警确实会响，且**真的共用一个预算**（含控制组）

前几轮的收尾办法都是"清守卫清单"（S9/S10/S12 已清）。本轮换一种：**验证自己两轮前改的产品行为 + 同时写进文档的那句话**。§2da（第一百二十轮）给"`matching_key` 有值但 `matching_tenant` 为 NULL"加了具名告警，并在 `ops.md` §4 写下后果（"its window is shared by EVERY tenant that accepts that key"）。当时只有 `validate()` 的**单元测试**，**没有一次端到端**。

### A. L7：在已有的 `integration/test_limit_roles_enforcement.py` 上加一条腿（不新增工件 ⇒ 不用接线）
那个 drill 本来就起一个一次性节点 + 两个租户（`t1`=`limits.local`、`t2`=`limits2.local`）+ 一个 `proxied(model, tenant, key)` 助手，并且**已经有 L6 在节点日志里找 `matching_provider` 告警** —— 形状完全一样，直接复用。新增：
1. 种一个 `matching_key=<raw key>`、`matching_tenant=None`、`limit_count=1` 的角色 ⇒ 断言**节点日志**里出现**点名该角色**的告警（实测打印：`WARN hydra::store: config: limit_role 'r-cross' scopes on matching_key but has matching_tenant NULL: its window is …`）；
2. **把文档写下的后果测出来**：`t1` 用该 key 请求 ⇒ **200**（耗尽窗口），随后 **`t2` 用同一个 key 的第一次请求 ⇒ 429**（实测 `codes=[200, 429]`）；
3. **控制组**：同一个 key 形状的角色改成 `matching_tenant="t1"` ⇒ `[200, 429, 200]`（t2 不受影响）**且日志里没有**关于该角色的跨租户告警 ⇒ 证明"共享"来自**缺失的租户域**，不是夹具里的别的东西。
**反向证伪**：把 `config.rs` 里那条 `if role.matching_key.is_some() && role.matching_tenant.is_none()` 改成 `false`、重建后重跑 ⇒ **恰好那一条**检查变红（`LIMIT ROLES: FAILED (1)`），而"共享确实发生"和两条控制组**照过** ⇒ 归因精准：新腿的告警断言由**告警代码**驱动，后果断言测的是文档描述的行为（该行为在告警之前就存在）。还原后 drill 全绿。

### B. 顺手修掉这个 drill 里一处**会骗人的输出**（L6 也有）
`check("…", cond, "no warning line found in the node log")` 的 detail 是**写死的失败文案**，于是**通过时也打印**"no warning line found in the node log"（本轮 L7 第一次运行时就打出了这句自相矛盾的输出）。已把 L6/L7 的 detail 改成**回显真正找到的那行**（`warning line: …`），控制组的 detail 改成"关于该角色的告警条数"。这类"输出与事实相反"的小坑正是本仓库反复抓的东西，既然是本轮引入 L7 时当场看见的，就一起修掉。

### C. 为什么不做 S11/S13（记录取舍，不是遗漏）
剩下的两条是**潜在**洞（`check_public_claims` 的日期只做 locale 互比；`check_ci_wiring` 的 `SKIP_DIRS` 让 `admin-ui/`、`bin/`、`tests/**` 里的新工件既不被发现也不被计数）——今天真树里那两个位置上**没有任何测试工件**，所以改了不改变任何结论；而"文档承诺 vs 实测"这条线今天**有**可验证的东西（上一轮刚把规则改严，端到端从未跑过），所以先做后者。S11/S13 仍留在 §2cx-E 的清单里。

**验证**：`integration/test_limit_roles_enforcement.py` 全绿（新增 L7 三条检查 + 修好的两处 detail）；`check_ci_wiring` exit 0（未新增工件，仓外工件仍 47）；`cargo fmt --check` 干净（本轮只改 Python 与文档，仍按规则先跑）；`check_public_claims --measure` 仍 **811**（无 Rust 测试增减）。**门禁 83 项全绿、`OVERALL=GREEN`、exit 0**（`limit_roles enforcement exit=0` 即含新增的 L7；两份转写 `FAILED` 计数 0）。

---

## 2dg. 第一百二十六轮：清掉 Oracle 复审清单**最后两条**（S13 与 S11）—— "放在被跳过目录里的工件没人管"与"公开日期只做 zh/en 互比"

§2cx-E 的清单到本轮清空（S3/S4/S5/S6/S7/S8/S9/S10/S12 已在前几轮闭环）。

### A. S13：`check_ci_wiring` 的 `SKIP_DIRS` 让四类目录里的工件**既不被发现也不被计数**
原 `SKIP_DIRS` 含 `admin-ui`/`bin`/`docs`/`environment`，且 walk 显式跳过 `tests/`（只留 `tests/e2e` 给 playwright 规则）⇒ 把一个新的 `*.test.cjs` 放进 `admin-ui/`（或把 spec 放在 `tests/` 而非 `tests/e2e/`）**永远不会被检查**：
**修法**：`SKIP_DIRS` 只保留"不可能有测试工件或必须不读"的目录（`secure`、`dev-docs`、`target`、`node_modules`、`.acceptance`、`dist*`），四个目录与 `tests/` 本体重新纳入遍历；`crates`/`scripts`/`.github`/`tests/e2e` 仍走各自的规则。另加 **walk floor**（`CI_WIRING_MIN_DIRS`，默认 10；真树约 25 个目录）——因为"遍历了什么"此前也**没有计数**。
**实测**：真树仍 **47 个仓外工件**（今天这四个目录里没有任何测试工件 ⇒ 结论不变，属于潜在洞），守卫 exit 0。**证伪**：把 `SKIP_DIRS` 与 `tests` 跳过逻辑改回去 ⇒ 新增的两条用例（`admin-ui/panel.test.cjs`、`tests/stray.spec.cjs` 都必须报 "never executed"）**恰好变红**，控制组照过。
**测试 4 条**：两条"新目录里的未接线工件必须被报"、两条控制组（被某步点名则通过）、外加 walk floor 的独立用例（`CI_WIRING_MIN_DIRS=500` 必红）。夹具 harness 把两个 floor 显式置 0（沿用既有纪律）。

### B. S11：公开计数页的**日期**只与另一个 locale 互比（两个字符串来自同一个文件）
`check_public_claims` 原先只断言 `zhDate === enDate` ⇒ 写上**未来日期**、或写一个**比它被校验时用的转写更旧**的日期，都照样 OK。
**修法（三条独立判据）**：①广告日期**不得晚于今天**（本地日期）；②用 `--core-log/--server-log` 校验时，广告日期**不得早于最新转写的 mtime 日期超过 1 天**（跨时区/午夜的余量），否则报"stale"并提示"re-measure, then update the page with --write"；③`--measure` 是**刚刚**测的 ⇒ 广告日期必须是今天。
**顺手修掉一处"一个值两个来源"**：写回路径用 `new Date().toISOString()`（**UTC**）而校验用本地日期 ⇒ 在 UTC 以东的机器上，傍晚刚 `--write` 出来的页面可能**当场不通过自己的检查**。现在两条路径都用同一个 `localDate()`。
**实测**：真树 `--measure` OK（广告 `2026-09-30` = 今天）。**证伪**：把"今天"改成 `9999-12-31`（等于关掉未来判据）⇒ 只有"未来日期"用例红；把 staleness 判据改 `false` ⇒ 只有"stale 日期"用例红；还原后 **25/25**。
**测试 3 条**：未来日期必报、比转写更旧的日期必报（harness 现场写日志 ⇒ 一个月前的日期天然 stale）、控制组（今天的日期通过）。夹具默认日期是"昨天"，正好落在 1 天余量**边界内**，因此既有 22 条用例不受影响（实测 22 → 25 全绿）。

**验证**：`check_ci_wiring` 真树 exit 0（47 仓外工件、walk floor 满足）；其测试 **ALL PASSED**（新增 4 条）；`check_public_claims` 真树 `--measure` OK；其测试 **25 passed / 0 failed**（新增 3 条）；两枚探针（wiring、claims）均已还原（逐文件 `FALSIFY` = 0）；`cargo fmt --check` 干净（本轮无 Rust 改动，仍按规则先跑）。**门禁 83 项全绿、`OVERALL=GREEN`、exit 0**（`ci wiring`/`ci wiring tests`/`public claims`/`public claims tests` 全 exit=0；两份转写 `FAILED` 计数 0）。

---

## 2dh. 第一百二十七轮：**我上一轮自己引入的假阳性** —— 把"日期旧了"判成失败，会让一张**数字完全正确**的发布页在 CI 上变红；改为 note 后重新派两个只读 Oracle（开启新一轮复审）

§2dg 清空了 §2cx-E 的复审清单，所以本轮按上一轮结尾写下的计划：**派新一轮只读 Oracle 复审最新代码**（两位子代理已在跑，结果留到下一轮处置）。但在派之前，我先按惯例复核上一轮自己的改动，**第一条就命中**。

### A. ★S11 的"stale 日期"判据是错的（我自己把它写进了失败路径）
上一轮我给 `check_public_claims` 加了三条日期判据，其中"广告日期不得早于最新转写 mtime 日期超过 1 天"被写成**失败**。问题：CI 每次运行都**重新生成**转写（`$RUNNER_TEMP/*-tests.log`，就在同一个 job 里），于是**只要测试总数一段时间不变**，一张**计数与 gate 断言全对**的页面就会因为"日期旧"而让 CI 变红 —— 而唯一的"修法"是提交一次**只改日期**的 commit。
**实测复现**（干净夹具：`core.log`=257、`server.log`=554，页面就是仓库里那张 811 的页面，只把日期改成 `2026-08-01`）：**exit 1**，且报错**只有**日期这一条（计数与两条 gate 断言都通过）。⇒ 这是"把**真但旧**的事实当成假"的假阳性，与我在别人守卫里反复修的同类缺陷是同一族。
**修法**：只有**谎言**才失败 —— ①日期不得晚于今天；②`--measure` 必须是今天（因为 `--write` 刚盖的章）；而"日期旧"降级为**打印 note**（新增 `notes` 通道，成功/失败两条路径都打印），文案明确"count 未变则无妨，下次测量时用 `--write` 刷新"。守卫头部同步改写（上一轮那段描述的是错误语义）。
**测试**：把原用例从"必须报错"改成"exit 0 + 必须打印 note"，并**新增一条控制组**（旧日期 + 错计数仍必须红 ⇒ note 不会把真正的检查掩盖掉）。全套 **26 passed**。

### B. 为什么这条值得单独记
这是本会话第一次由**我自己**制造的假阳性（此前都是"守卫不能变红"这一类）。它的形态很典型：**把可观测的时间戳当成不变量**。判据应该是"这个断言失败时，是否**只可能**因为页面说错话"——若还存在"页面没说错、只是没人重新测量"的合法世界，那它只能是 note。这条已经写进 `check_public_claims.cjs` 头部，作为后续加判据时的判据。

### C. 本轮同时派出的两位只读 Oracle（结果下一轮处置）
①**守卫硬化复审**：审 §2dd–§2dg 这十轮给守卫加的东西（`rust_blank.cjs`/`js_blank.cjs`/`compose_static.cjs`、放宽后的 e2e 时间戳规则、各守卫新加的 floor、env 守卫的"证据必须在非测试代码里"规则），专门找**同一族**缺陷（守卫不能失败、证据来路不对、floor 可被别的东西满足、加固本身引入新逃逸口）以及顺序/可达性问题（早退、`continue`、allowlist 先于违规报告）。②**产品复审**：审 §2cw–§2da 的产品改动（`api_key_raw` 匹配、`bucket_key`、`Debug` 脱敏、跨租户告警）在**单元测试看不到的地方**（两个门在不同阶段对"哪些角色匹配/哪个桶被记账"是否会分歧、reload 中途改配置、集群 Redis key 名、`validate` 的所有配置路径、还有没有别的泄 key 出口）。

**验证**：`check_public_claims` 真树 `--measure` OK；其测试 **26 passed / 0 failed**；干净夹具（计数正确、日期旧）从 **exit 1 → exit 0 + note**；`check_ci_wiring` exit 0。

---

## 2di. 第一百二十七轮（续）：两位 Oracle 的 **18 条新发现**，本轮先修其中两条 P0/P1 —— `#[cfg(not(test))]` 被当成测试代码抹掉、未闭合引号让 e2e 时间戳规则静默失效

§2dh 记录了本轮派出的两位只读 Oracle（守卫硬化复审 + 产品改动复审）。两份报告合计 14 + 5 条发现（另有若干"查过、确认无问题"的负结论）。**本轮先修两条**，其余带位置与复现路径记入 §2dj（下一轮的队列）。

### A. ★F1（P0，守卫硬化复审）：`isTestGate` 用子串 `\btest\b` 判门 ⇒ `#[cfg(not(test))]`（**纯生产**代码）被整块抹掉
`rust_blank.cjs:118` 的判定是 `/\btest\b/.test(expr)` —— 不是谓词求值，而是**子串搜索**；于是"这个门说**不是**测试代码"的那一种写法被当成了"这是测试代码"的证据，整个 item 被 blank，里面的生产代码**永不扫描**。
**我的独立复现**（真夹具 + `PURITY_SRC_ROOT`/`PURITY_DOCS` 注入，子进程跑真脚本）：同一个 `.unwrap()` —— 普通函数里 **exit 1**；放进 `#[cfg(not(test))] mod boot` 里 **exit 0**（守卫打印 clean）。影响四个守卫：`check_source_purity`（放行）、`check_documented_env`/`check_documented_metrics`/`check_alert_expressions`（把生产读点/注册点抹掉 ⇒ **假红**）。真树今天**没有** `not(test)`（`grep` 只有 `#[cfg(test)]` 56 处 + `#[cfg(all(test, feature = …))]` 4 处），属潜伏缺陷。
**修法**：把子串搜索换成真正的**蕴含判定** `cfgIsTest(expr)` —— 问的是"这个谓词**只在** cfg(test) 时成立吗"：
- `test` ⇒ true；
- `all(a,…)` ⇒ **任一**合取项蕴含 test（`#[cfg(all(test, feature = "cluster-redis"))]` **是**测试代码 —— 真树里有 4 处，弄错就会开始误报）；
- `any(a,…)` ⇒ **所有**析取项都蕴含 test（`any(test, feature="x")` 在 feature 打开时会进生产 ⇒ 算生产，安全方向）；
- `not(…)` ⇒ false（`not(test)` 只在生产构建里编译）；
- 其余（feature/target/…）⇒ false ⇒ **一律算生产代码，宁可多扫不可漏扫**。
9 条谓词用例逐一核对通过（含 `all/any/not` 嵌套与否）。
**★一条我差点写错的结论**（值得记）：子代理称"`#[cfg(feature = "test-helpers")]` 这类**名字里带 test 的 feature** 也会被吞掉"。我按子串语义以为成立，**实测推翻**：`stripComments` **先**把字符串内容抹空，谓词看到的其实是 `#[cfg(feature = "            ")]`（实测打印），所以旧代码在那个形状上并不会误判。⇒ 已在 `rust_blank.cjs` 的注释里**按实测改写**（并说明这里保留"先字符串屏蔽"只是纵深防御，不是修复本身），对应的测试也改成"锁定这条性质"而不是"F1 的证据"。**教训**：子代理的机制推断也要逐条复现，否则会把错的因果写进代码注释。
**测试**：purity 新增 3 条（`not(test)` 里的违规必须被报 + `test-…` feature 不算测试门 + 控制组 `all(test, feature="x")` 仍算测试代码）。**证伪**：把判定换回原来的 `\btest\b` ⇒ **恰好** `not(test)` 那条红（另两条照过，正因为它们在旧谓词下本来就对）；还原后 **26/26**、真树 exit 0。

### B. ★F2（P1，守卫硬化复审）：`maskJsLiterals` 遇**未闭合引号**会把文件剩余部分抹空 ⇒ 时间戳规则静默失效
`js_blank.cjs` 的引号分支一路扫到"下一个同类引号或 EOF"，于是**一个游离反引号**就让其后所有 `api(` 调用从掩码里消失。**子代理实测**：注入一个游离反引号后 `api()` 站点从 8 变 **0**，而守卫之所以还非零退出，**纯粹靠**与之无关的 `MIN_API_SITES` floor 偶然兜底（在测试 harness 里该 floor 被置 0 ⇒ 完全静默）。
**修法两件**：
1. `"`/`'` **在换行处终止**（JS 字符串不能跨行；未闭合就是语法错误，继续扫到"下一个引号"会把中间全抹掉）；
2. 只有**模板字面量**（唯一能合法跨行的）在未闭合时向上报告 `unterminated`，`check_e2e_contracts` 据此**直接报 DRIFT**（"this file cannot be parsed as JS … the checks below prove nothing"），而不是静默跳过。
**为何只报模板**：把未闭合的 `'`/`"` 也报出来会误伤**正则字面量里含撇号**的合法 spec（`/don't/` 这种扫描器分不清），所以单引号只做"换行终止"，不做"报错"。
**实测**：①游离反引号 ⇒ exit 1 且理由是"unterminated"（**证伪**：只回退"换行终止 + 未闭合上报"这两处 ⇒ 该例变 exit 0 **且** 打印 `0 api call(s) seen` ⇒ 规则完全空转）；②游离单引号 + 其后的真违规 ⇒ 违规仍在**正确行号**被报（`omits `updated_at``）（**证伪**：同一探针下该例 exit 0 ⇒ 违规被吞）。
**测试**：e2e 新增 2 条（未闭合模板必报、游离引号不得藏住后续违规）。
**★第三次"备份取错时机"**：我在开始 F2 时 `cp` 出的 `js_blank.cjs.postfix` 其实是**修复前**的文件（cp 在同行、python 编辑在后），后来"还原"直接把它盖回工作树 ⇒ 真树 `exit 1`（新 API `maskJsLiteralsReport` 不存在）。**这次的规则**：备份文件名里的 postfix/before 都不可信，**还原前后都要 `grep` 一个只有修复后才存在的符号**（本轮用 `grep -c maskJsLiteralsReport`）；已改成 `.postfix.verified`。

### C. 其余发现（记入 §2dj，下一轮按优先级处置）
**产品侧**（产品复审，比我预想的更值得先看）：
- **P2-1 两道门读的是**不同代**的配置快照**：计数门/token 门用请求开始时的 `cfg_guard`（`proxy.rs:557`），而记账门在 `logging` 阶段**重新** `self.state.store.snapshot()`（`proxy.rs:1517`）。任何一次 admin 写入都会换掉这一代（`store.rs:526`）⇒ 请求进行中把角色删掉/停用，计数样本**已消耗**而 token **永不入账**（直接违背 `limiter.rs:181` 写明的契约），反向则 token 入账而从未检查；Redis 路径还会因为两代 `window_ms` 不同而**提前淘汰/滞留**。修法候选：一个请求只用一个配置代（把 `Arc<ConfigData>` 存进 `RequestContext`）。
- **P2-2 跨租户警告的措辞会误导**：`matching_tenant` **不在 bucket 里**（`bucket_for` 只在 role 限定 tenant 时才拼它），所以"设了 tenant 就安全"是错的；掩码碰撞（`mask_key("ab1234") == mask_key("abZZ34")`）在**同租户内**也会共享（这正是 D-15 第二部分）。
- **P3-1** 掩码（首 10 尾 4）直接进 Redis key 名（`hydra:{rl:role:bucket}`），与 `ops.md` 记的"hash-first"待办不一致；**P3-2** 告警只在 `build_config` 路径产生 ⇒ admin 写入的 HTTP 响应里没有、**edge 永不评估**（`apply_snapshot` 不 re-validate）；**P3-3** `Debug` 的"掩码不动点"判定把**全 `*` 串**当掩码（任意长度全 `*` 都是不动点 ⇒ `Some("******")` 会**逐字打印**）；**P3-4** raw 形让 `matching_key` 成为明文活凭据，而 `validate` 里没有对应警告（只有文档 + D-16）。
**守卫侧**：F3（`compose_static` 对 `extends:`／`${VAR}` 镜像**完全失明**，而两个 compose 守卫发现 1 个服务就通过 ⇒ 三个真服务逃避 grace/healthcheck 检查）；F4（`scalar()` 不剥 YAML 行内注释 ⇒ `stop_grace_period: 30s # …` 误判 FAIL）；F5/F6（`check_alert_expressions` 与 `check_e2e_contracts` 的 floor 落在 `problems` 的**早退之后** ⇒ 永不执行）；F7（`check_redis_pool` 的 `splitArgs` 不屏蔽字符串 ⇒ 含逗号的字符串能把 `None` 策略"搬"到第 4 位）；F8（`check_ci_wiring` 不读 `if:`/`continue-on-error` ⇒ `if: false` 的步骤算"已执行"）；F9（`documented_defaults`/`documented_env` 的裸子串与过松的 `carrierUse`）；F10–F14（floor 常数与实际脱节、发现盲区、compose 缩进/嵌套假设、i18n 未闭合模板、`--write` 的自我满足形状）。

**验证**：`check_source_purity` 真树 exit 0、测试 **26 passed**（新增 3）；`check_e2e_contracts` 真树 exit 0（242 tokens / 7 calls / 6 payloads / 88 harness files）、测试 **ALL PASSED**（新增 2）；`check_public_claims` 测试 26 passed（本轮 C 段）；两处探针已还原（逐文件 `FALSIFY` = 0，且本轮改为**验证式备份**）。**门禁 83 项全绿、`OVERALL=GREEN`、exit 0**（两份转写 `FAILED` 计数 0；`public claims` 真树 `--measure` 仍 **811**）。

---

## 2dj. 第一百二十八轮：修掉产品复审里最重的一条 —— **一个请求用了两代配置**（计数门/记账门分裂 ⇒ 用量凭空消失）

上一轮记入队列的 **P2-1**（产品复审最先列出的那条）。**先自查**：`proxy.rs:557` 取 `cfg_guard` 供计数门与 token 门使用，而**记账门**在 `logging` 阶段又 `self.state.store.snapshot()` 取了**新的一代**（`proxy.rs:1517`）。任何一次 admin 写入都会发布新一代（`store.rs:526`），而流式请求的 `logging` 可以晚到秒级甚至分钟级 —— 于是同一个请求的两半可能看着**不同的角色集与不同的窗口长度**。

### A. 后果（子代理逐条给出，我按代码核对为真）
1. 请求进行中把角色**删除/停用** ⇒ 计数样本**已经花掉**，而记账阶段 `match_roles` 因 `enabled &&` 为假直接不匹配 ⇒ 本次请求的 token **永不入账**，直接违背 `limiter.rs:181` 写明的契约"the request is always counted"，且**无日志无指标**；
2. 反向（请求中新增/启用角色）⇒ token 入账但**从未被检查**；
3. 窗口长度也会跨代：进程内 `add_tokens` 用**新**定义的 `window_len` 建/取窗口，而 `check_count` 用的是**旧**定义；Redis 路径把 `window_ms` 作为 Lua 参数发给脚本（`rate_limit.rs:207/320`）⇒ 同一个 `tokens_key` 的淘汰边界按两代给出两个界 ⇒ 旧 `m`、新 `h` 时token 会被**提前淘汰**（fail-open），反向则**滞留**（过度拒绝）。

### B. 修法：**一个请求，一代配置**
`RequestContext` 新增 `pub cfg: Option<Arc<ConfigData>>`（`proxy/ctx.rs`），在取请求级快照处 `ctx.cfg = Some(Arc::clone(&cfg_guard))`（`proxy.rs:557` 之后），记账门改为**优先用这一代**的 `limit_roles`（拿不到才回退到新快照 —— 请求路径上不存在这种情况）。生命周期不增加任何保留：原先那个 `Guard` 本来就要活到 `request_filter` 返回（终止模式下流式响应是在 hook 内写完的）。
**一处顺手说明**：这不是"把新配置藏起来"——请求**已经**按旧一代过了门，记账必须跟它一致，否则两侧各自自洽、合起来不守恒。

### C. 端到端证据（新腿 L8，加在已接线的限流 drill 里）
夹具改造：mock 上游对 `model == "slow"` **睡 1.5 秒**（这样能在请求在途时改配置），并种一个 `slow` 的 provider-model 与 t1 的 tenant-model。
**L8 步骤**：种 `r-inflight`（`matching_tenant=t1`、`limit_count=1000`、`limit_token=1`）→ 线程里发一条 `slow` 请求 → 睡 0.4s 后（请求仍在途）`DELETE /limit-roles/r-inflight` + reload → 等请求 1 结束 → **重建同一角色**（同 id/同形状）+ reload → 发请求 2 → 断言 **429**。
**为什么这条能判别**：token 窗口的键是 `(role_id, bucket)`，重建角色后请求 2 的 token 门读的就是同一个窗口；**只有**请求 1 的用量被记进那一代才会超限。**实测**：请求 1 = **200**、请求 2 = **429** ⇒ 用量确实落在门用过的那一代里。
**反向证伪**：把记账门改回读新快照（`match None::<&ConfigData>`）⇒ **恰好那一条**断言变红（请求 2 变 **200**，用量凭空消失），其余 L1–L7 与 L8 的第一条断言照过 ⇒ 归因精准。还原后用**验证式备份**（`.acceptance/round128/proxy.rs.verified`，`grep -c "ctx.cfg.as_deref()" == 1`）确认修复回来了。

### D. 仍未做的（队列顺延）
产品侧剩 **P2-2**（跨租户警告措辞误导：`matching_tenant` **不在** bucket 里，掩码碰撞在同租户内也共享）、**P3-1**（掩码进 Redis key 名 vs D-15 的 hash-first）、**P3-2**（告警在 edge 不评估、admin 响应无提示）、**P3-3**（"掩码不动点"把全 `*` 串当掩码）、**P3-4**（raw 形明文凭据无 validate 警告）；守卫侧剩 **F3**（compose 静态路径对 `extends:`/`${VAR}` 失明却打印 OK）、**F4**（YAML 行内注释误判）、**F5/F6**（floor 落在早退之后）、**F7**（`splitArgs` 不屏蔽字符串）、**F8**（不读 `if:`）、**F9–F14**。

**验证**：`integration/test_limit_roles_enforcement.py` 全绿（含新 L8 两条断言）；`cargo test -p hydra-server --features server --lib` **195 passed**；`cargo fmt --check` 干净（**启动门禁之前**）；`check_public_claims --measure` 仍 **811**（无 Rust 测试增减）；探针已还原、`FALSIFY` = 0。**门禁 83 项全绿、`OVERALL=GREEN`、exit 0**（`limit_roles enforcement exit=0` 含新 L8；两份转写 `FAILED` 计数 0）。

---

## 2dk. 第一百二十九轮：清守卫队列的四条（**F3** 静态路径对 `extends:`/`${VAR}` 失明却打印 OK、**F4** YAML 行内注释误判、**F5/F6** floor 落在早退之后永不执行）

承接 §2di/§2dj 的队列。

### A. ★F3：`compose_static` 对 `extends:` 与 `${VAR}` 镜像的服务**完全失明**，而守卫照样打印 OK
**先复现**（子代理的夹具）：`hydra-a` 用 `extends: base-hydra`、`hydra-c` 用 `image: ${HYDRA_IMAGE_TAG}` ⇒ grace 守卫只看见 `base-hydra` 一个服务，**exit 0**；而这两个服务是**真的会被部署**的，它们的 `stop_grace_period`/healthcheck 压根没被检查。这正是本模块诞生时要防的形状（"退化为检查得更粗，而不是不检查"被悄悄变成了"不检查"）。
**修法**：`compose_static.cjs` 新增 `unjudgeableServices(text)` —— 点名两类文本读不出来的服务（`extends:`；`image:` 里含 `${…}`），**两个守卫**在静态路径上发现非空即 `ScanError`（**exit 2 CANNOT VERIFY**），文案给出服务名与原因并让运维改用 `docker compose config`。
**实测**：夹具 ⇒ 两个守卫都 **exit 2** 并点名 `hydra-a uses `extends:``、`hydra-c its image is not decided until render time`；**三个 shipped 文件不受影响**（先确认过它们既无 `extends:` 也无 `${` 镜像）⇒ 仍 exit 0。**证伪**：把拒绝条件改 `false` ⇒ 新增的两条用例（grace/health 各一条）**恰好变红**。

### B. F4：`scalar()` 不剥 YAML **行内注释** ⇒ 合法的 `stop_grace_period: 30s # 实测 25s drain` 被判 FAIL
仓库的 compose 文件里本来就有大量解释性注释，把注释挪到同一行是完全正常的编辑，却会让 `durationSecs("30s # …")` 解析失败并打印"unparseable"。**修法**：新增 `stripYamlComment()`（引号感知：`#` 只有在前面是空白且不在引号内时才是注释），`scalar()` 先剥注释再 trim/去引号。**实测**：带行内注释的合法文件 ⇒ grace/health 都 exit 0。**证伪**：去掉 `stripYamlComment` ⇒ 新增用例红。

### C. ★F5/F6：两条 floor 落在 `problems` 的**早退之后**，在它们唯一该起作用的情形里永不执行
- `check_alert_expressions`：`MIN_REFS`/`MIN_LABELS` 的 exit 在 `problems` 的 exit **之后** ⇒ 当"抽取塌陷"和"真漂移"同时发生时，运维只看到 DRIFT，永远看不到"这条检查什么都没证明"。**修法**：两条 floor **并入 `problems`**（而不是简单地前移 —— 我先试了前移，结果是 floor 反过来把漂移遮住；两次都不对的教训写进代码注释），于是**一次运行把两者都报出来**。
- `check_e2e_contracts`：`MIN_TOKENS` 的 `process.exit(1)` 在 name-kill 扫描与 `MIN_HARNESS_FILES` floor **之前** ⇒ 抽取坏掉时那条 floor 与 pkill 规则一起不可达。**修法**：改成 `problems.push(...)`，单次退出在最末。
**实测/证伪**：①alert 探针（把 `MIN_REFS` 条件改 `false`）⇒ **两条**用例红（新加的"漂移 + 塌陷同报"以及既有的"覆盖率不足必须失败"）；②e2e 探针（把 token floor 改回早退）⇒ 新加的"token floor 触发时不得隐藏 harness 扫描与其 floor"红，且输出只打印 token floor（harness 一行都没有）。还原后两套测试全绿（alert ALL PASSED、e2e ALL PASSED）。

### D. 队列剩余
产品侧 **P2-2**（跨租户警告措辞：`matching_tenant` 不在 bucket 里）、**P3-1**（掩码进 Redis key 名）、**P3-2**（edge 不评估告警、admin 响应无提示）、**P3-3**（全 `*` 串被当掩码）、**P3-4**（raw 形明文凭据无 validate 警告）；守卫侧 **F7**（`splitArgs` 不屏蔽字符串）、**F8**（不读 `if:`/`continue-on-error`）、**F9**（裸子串与过松的 `carrierUse`）、**F10–F14**。

**验证**：`compose_static` 5 passed、`compose grace` 17 passed（+3）、`compose health` 13 passed（+1）、alert ALL PASSED（+1）、e2e ALL PASSED（+1）；三个真树守卫 exit 0；`check_ci_wiring` exit 0；`check_public_claims --measure` 仍 **811**；四枚探针已还原（逐文件 `FALSIFY` = 0，全部用**带 assert 的替换**）。

**门禁 83 项全绿、`OVERALL=GREEN`、exit 0**（`compose stop grace`/`compose grace tests`/`compose healthchecks`/`compose health tests`/`compose static reader tests`/`alert expressions`/`alert expression tests`/`e2e contracts`/`e2e contract tests` 全 exit=0；两份转写 `FAILED` 计数 0）。

---

## 2dl. 第一百三十轮：再清两条（**F7** `splitArgs` 不认字符串 ⇒ `None` 策略可被"逗号搬位"藏起来、**F8** `check_ci_wiring` 不读 `if:`/`continue-on-error` ⇒ 永不执行的步骤算"已执行"）

### A. ★F7：`check_redis_pool` 的参数切分不认字符串字面量
**先复现**——但我按子代理给的例子复现**失败**了：他们的例子把逗号放在**嵌套调用** `label("a,b")` 里，而深度计数本来就覆盖括号 ⇒ 那条形状**从来无害**。真正能搬位的是**深度 0 的字符串实参**里的逗号：`Pool::new(cfg, "a,b", Some(conn), None, 2)` —— 旧切分给出
`["cfg","\"a","b\"","Some(conn)","None","2"]` ⇒ `parts[3]` 是 `Some(conn)`，而**真正的第 4 个实参是 `None`** ⇒ R2"策略必须是 `Some(...)`"被一个根本不是策略的实参满足，守卫打印 OK。
**修法**：在**保偏移的屏蔽副本**上定位顶层逗号（复用同文件已有的 `blankCommentsAndStrings`），再按这些位置从**原文**切片 ⇒ 值本身不变。
**实测**：旧切分 `parts[3]="Some(conn)"` ⇒ R2 通过（旁路成立）；新切分 `parts[3]="None"` ⇒ 守卫 **exit 1** 并点名。
**★我这一轮又踩了同一个坑（第二次）**：证伪探针结尾我用 `cp .before …` 还原，而 `.before` 是**修复前**的副本 ⇒ **把修复本身还原掉了**，随后新加的用例"失败"我开始怀疑用例。**发现方式**：直接 `sed` 看文件里的 `splitArgs` 仍是旧实现。⇒ 已改为**验证式备份**（`cp` 后 `grep -c "blankCommentsAndStrings(text)"` 必须为 1），探针一律用**定向替换**而不是整文件回滚。**教训升级**：`cp` 整文件"还原"是危险的；**能定向反转的探针就定向反转**（`X` → `false && X`），并且**每次还原后立刻跑一次被测用例**。
**测试 2 条**：顶层字符串里的逗号不得藏住 `None` 策略（**证伪**：切分改回走原文 ⇒ 该例红）、控制组（第 4 位真是 `Some(policy)` ⇒ 通过）。顺带修掉测试文件里过期的"15 assertions"字样（实为 17）。

### B. F8：`check_ci_wiring` 只看 `run:` 文本，`if: false` 的步骤照样算"已执行"
`extractSteps` 只取 `run:`/`working-directory:` ⇒ `if:`/`continue-on-error:` 完全不读。**实测**（用脚本自身的函数）：带 `if: false` 的步骤与带 `continue-on-error: true` 的步骤都被当成正常步骤。**修法两半**：
1. `extractSteps` 同时取出 `cond`/`lenient`；**`if:` 字面为假**（`false` / `${{ false }}`）的步骤从"执行集"里剔除，并**具名报问题**（"the step `…` is disabled by `if: false` — nothing it names is executed"）；`commands` 也改为**由启用步骤派生**（否则 `scripts/` 那条规则仍会被禁用步骤满足 —— 这是我第一版留下的洞，被新用例当场抓到）；
2. 跑测试却带 `continue-on-error: true` 的步骤**报"不是门禁"**（失败无法阻断），不跑测试的只在 notes 里提一句。
**实测**：真树今天两形状都没有 ⇒ 仍 exit 0；夹具两形状各自 exit 1 并给出对应文案，控制组（去掉这两把键）通过。
**★我这条改动里自己制造的两个 TDZ 崩溃（都被用例抓到）**：①`continue-on-error` 规则写在 `TEST_RUNNER` 定义**之前** ⇒ 每次带该键的运行都 `ReferenceError`；②把 `commands` 改成 `steps.map(...)` 时，声明位置在 `steps` **之前** ⇒ 整个守卫崩。两次都是"新代码引用了后面才定义的 `const`"，修法是把规则/声明移到依赖之后（并在注释里写明**必须留在 `TEST_RUNNER` 之后**）。
**证伪（两半各自独立）**：把 `isLiteralFalse` 变恒假 ⇒ **只有**"禁用步骤不算执行"那条红；把 `continue-on-error` 的匹配变恒假 ⇒ **只有**"不是门禁"那条红；还原后 **ALL CI WIRING TESTS PASSED**、真树 exit 0。
**已知未做**：**作业级 `if:`**（`jobs.<id>.if: false`）仍未建模 —— 已写进代码注释作为"记录在案的缺口"，而不是假装覆盖。

### C. 队列剩余
产品 **P2-2**、**P3-1**…**P3-4**；守卫 **F9**（`documented_defaults`/`documented_env` 的裸子串与过松 `carrierUse`）、**F10–F14**。

**验证**：`check_redis_pool` 真树 exit 0、测试 **17 条全过**（+2）；`check_ci_wiring` 真树 exit 0、测试 **ALL PASSED**（+3）；`check_public_claims --measure` 仍 **811**；三枚探针全部**定向反转**并还原（逐文件 `FALSIFY` = 0）。

**门禁 83 项全绿、`OVERALL=GREEN`、exit 0**（`fred pool single owner`/`fred pool guard tests`/`ci wiring`/`ci wiring tests` 全 exit=0；两份转写 `FAILED` 计数 0）。

---

## 2dm. 第一百三十一轮：**F12**（compose 静态读取器不认缩进层级：嵌套"诱饵键"能冒充服务自己的键，`healthcheck: {disable: true}` 与渲染路径诊断相反）

### A. 复现两半
①**嵌套诱饵**：`x-static-decoy: { healthcheck: { test: … }, stop_grace_period: 30s }` —— 静态路径把**嵌套**里的键当成服务自己的键，于是对一个**根本没有 healthcheck** 的服务给出了**错误诊断**（"a all-role node runs the admin API — probe /api/v1/health"），grace 也照样"通过"；
②**`healthcheck: { disable: true }`**：docker 渲染后这个键**根本不存在**（compose 语义是"移除健康检查"），而静态路径读成"有 healthcheck 但 test 是空的"，于是两条路径对同一处缺陷给出**不同诊断**。

### B. 修法（以及我第一版**过度收紧**被测试当场抓住）
新增 `ownKeys(block)`（服务自己的、缩进最小的那些键）与 `scalarOwn`/`hasOwnKey`（只在这些键里查），并把 `subBlock`/`healthcheckDisabled` 改成**锚定服务自己的那一行**（不再用"第一行写成该名字的"）⇒ `healthcheckTest(block)`（探针命令行）、`envValue(block, 'HYDRA_ROLE')`（服务自己的 `environment:`）都从**正确的子映射**读；`image:` 也改为 `scalarOwn`（嵌套里的 `image:` 不能让一个非 hydra 服务看起来像 hydra）；`healthcheck: { disable: true }` 按 docker 语义算作**没有 healthcheck**。
**★我第一版把"层级感知"一刀切**：让 `scalar` 也只认自己的键 ⇒ **两条既有测试当场红**：`HYDRA_ROLE` 本来就住在 `environment:` 子映射里（于是本地栈的 `hydra-a` 被诊断成 `role=all`），`healthcheck.test` 同理。⇒ 正确做法是**两种读法并存**：`scalar`（嵌套容忍，用于住在子映射里的键）+ `scalarOwn`/`hasOwnKey`（只认服务自己的键）+ `subBlock` 系列（从**正确的**子映射里取）。这条"先收紧再回退一半"的过程已写进 `compose_static.cjs` 的注释，免得后人再收紧一次。
**★我这轮还制造了三个自伤**（都被测试或真树当场抓到）：①`hasKey` 改名 `hasOwnKey` 后**测试文件仍调旧名**（两处 `hasKey is not a function`）；②用 `const {`…`} = require('./compose_static.cjs')` 的区间替换把中间夹着的 **`const { spawnSync } = require('child_process')` 一起删掉**（真树立刻 `ERROR: spawnSync is not defined`）；③`compose_static.cjs` 的两次插入因**锚点文本不符**而静默失败（`assert` 少写在其中一处）⇒ 半个补丁 + 半个补丁叠加。**教训**：区间型替换必须**替换后立即 `node -e` 加载模块 + 跑真树**，而不是等最后一起跑。

### C. 实测与证伪
- 诱饵夹具：health ⇒ `no healthcheck (nothing supervises this node)`（不再是那条错误诊断）；grace ⇒ `no \`stop_grace_period\``、exit 1（嵌套里的 30s 不再算数）。
- `disable: true` 夹具 ⇒ 与渲染路径一致地报 `no healthcheck`。
- **真树**：两个守卫都 exit 0（本地栈静态检查仍报 `hydra-a (role=leader)`、`hydra-c (role=edge)` —— 证明 `HYDRA_ROLE` 仍从 `environment:` 里读到）。
- **证伪**：①`ownKeys` 改回"所有键" ⇒ grace/health 各 **1 条**红（两条诱饵用例）；②`healthcheckDisabled` 恒假 ⇒ **恰好**"disable 语义"那条红。还原后 parser **7/7**、health **15/15**、grace **19/19**、真树双 0、`FALSIFY` = 0。

### D. 队列剩余
产品 **P2-2**、**P3-1**…**P3-4**；守卫 **F9**（`documented_defaults`/`documented_env` 的裸子串与过松 `carrierUse`）、**F10**（floor 常数与实际脱节、`MIN_E2E_SPECS` 零余量）、**F11**（`crates/`、`.github/` 辅助脚本的发现盲区）、**F13**（i18n 未闭合模板）、**F14**（`--write` 的自我满足形状）。

**验证**：`compose_static` 7、`compose grace` 19（+2）、`compose health` 15（+2）全绿；两个守卫真树 exit 0；`check_ci_wiring` exit 0；`check_public_claims --measure` 仍 **811**；两枚探针已还原（逐文件 `FALSIFY` = 0）。

**门禁 83 项全绿、`OVERALL=GREEN`、exit 0**（`compose stop grace`/`compose grace tests`/`compose static reader tests`/`compose healthchecks`/`compose health tests` 全 exit=0；两份转写 `FAILED` 计数 0）。

---

## 2dn. 第一百三十二轮：**F9(a)** 相邻开关的默认值会被当成被测行的默认值（假通过），**F13** i18n 未闭合模板静默缩短扫描

### A. ★F9(a)：`check_documented_defaults` 用 `line.includes(name)` 认开关 ⇒ 邻居"代答"
**复现**（干净夹具，两行 Rust）：
```rust
fn extra() -> u32 { std::env::var("HYDRA_LISTEN_EXTRA").unwrap_or(9999) }
fn real()  -> u32 { std::env::var("HYDRA_LISTEN").unwrap_or(1111) }
```
文档写 `| HYDRA_LISTEN | 9999 |`（**邻居的值**）⇒ 旧守卫 **exit 0** 并打印 `OK HYDRA_LISTEN = 9999 … @ knobs.rs:1 (extra().unwrap_or)` —— 一行**错误**的文档默认值被**另一个开关**的默认值"验证"通过；而同一文件里真正的 `HYDRA_LISTEN` 是 1111。因为 `codeDefault` 在**第一个**产出数值的行上就返回，邻居只要排在前面就赢。
**修法（两处，缺一不可）**：①行筛选从 `line.includes(name)` 改为**词边界**匹配 `\bNAME\b`（新增 `lineMentions`，并把 `HYDRA_LISTEN_EXTRA` 这类相邻名字排除）；②窗口锚点 `wide.indexOf(varName)` 同样改为**词边界搜索**。
**实测**：修好后同一夹具 **exit 1** 且报 `HYDRA_LISTEN: ops.md says 9999 … but the code falls back to 1111 @ knobs.rs:2`（读到**正确那一行**）。**真树**：`23 条比较通过 / 10 条已记录`（与修前一致 ⇒ 真实树里旧行为**恰好**都命中了正确的行，这是**负结论**：这处洞在仓库里没有造成实际错判）。
**★证伪过程本身就是一条教训**：我先只回退①（行筛选）⇒ 假通过**没有**回来，一度以为"新用例不具判别力"；实际是**我的修复有两处独立机制**（②窗口锚点单独就能把窗口移到正确的行上）。把**两处一起**回退后：夹具重新 exit 0 并打印 `9999 … extra().unwrap_or`，且**两条**新用例同时变红（drift 用例 + 控制组）。⇒ 教训：**当"回退一处探针却不变红"时，先怀疑修复有多个独立机制**，别急着否定用例。
**测试 2 条**：邻居默认值不得为文档行背书（+控制组：写真值 1111 时通过）。**★另一个自伤**：我最初把这两条用例**追加在测试文件末尾**，而该文件末尾是 `console.log(...); process.exit(...)` ⇒ 用例**从未运行**（第一次探针因此"没变红"）。已把用例移到 summary **之前**——这类"末尾追加被 exit 截住"的坑值得记。

### B. F13：`check_i18n` 的未闭合模板让扫描**静默缩短**
模板分支扫到 EOF 也不报错 ⇒ 其上/其后的 `t()` 引用消失（症状被 `MIN_T_KEYS` floor 兜住，但没人说出原因）。**修法**：`scanSource` 增加 `unterminated` 标记 → `checkI18n` 的 `stats` 带上它 → CLI 见到即 **exit 2 CANNOT VERIFY**（"a template literal never closes, so the scan stopped early and every t() call after it is invisible"）。**实测对照**（最小夹具：`i18n.js` + 一个含未闭合模板且其后还有 `t()` 的文件）：修后 ⇒ `CANNOT VERIFY` **exit 2**；修前 ⇒ exit 1 并打印 **347 条**无关问题（因为夹具缺其它 UI 文件），**完全没有提**"文件没解析成功"。**证伪**：把上报条件改 `false` ⇒ 该用例变成 exit 0 且只数到 `1 t() literal`（第二个引用**静默消失**）。**测试 1 条**。

### C. 队列剩余
产品 **P2-2**、**P3-1**…**P3-4**；守卫 **F9(b)**（`carrierUse` 过松）、**F10**（floor 常数与实际脱节、`MIN_E2E_SPECS` 零余量）、**F11**（`crates/`、`.github/` 辅助脚本的发现盲区）、**F14**（`--write` 的自我满足形状）。

**验证**：`check_documented_defaults` 真树 exit 0、测试 **ALL PASSED**（+2）；`check_i18n` 真树 exit 0（348 keys / 4 files / 139 t()）、测试 **ALL PASSED**（+1）；`check_ci_wiring` exit 0；`check_public_claims --measure` 仍 **811**；探针逐文件 `FALSIFY` = 0。

**门禁 83 项全绿、`OVERALL=GREEN`、exit 0**（`documented defaults`/`documented default tests`/`i18n`/`i18n tests` 全 exit=0；两份转写 `FAILED` 计数 0）。

---

## 2dp. 第一百三十三轮：**F11** 发现盲区（顺带挖出"按名字跳目录"这个真根因）与 **F10** 三条形同虚设/零余量的 floor

### A. ★F11：`crates/` 与 `.github/` 完全不遍历 —— 而真根因是 **`SKIP_DIRS` 按名字匹配**
`walk` 显式跳过 `crates`/`.github`（前者只有规则 3/4 管 `crates/<crate>/tests/*.rs`，后者只读 ci.yml）⇒ `crates/x/tests/check_y.py`、`.github/scripts/*.sh|*.test.cjs` 这类工件**既不被发现也不被计数**（潜伏：今天树里没有）。
**修法（两步，第二步才是根因）**：①把 `crates`/`.github` 纳入遍历（`.rs` 不匹配 `TEST_GLOBS`，所以 cargo 目标仍归规则 3/4；`crates/` 里的 `*.test.cjs` 之类现在会被发现）；②**真根因**：`SKIP_DIRS.has(ent.name)` 是**按名字、任意深度**匹配的，而 `scripts` 就在那张表里（因为规则 1 拥有**顶层** `scripts/`）⇒ **任何深度的 `scripts/` 目录都被跳过**。**实测**：`.github/scripts/probe.test.cjs` 即使①做完也**照样不被发现**（我用临时 `WALK` 轨迹打印打出来的：只走到 `.github`、`.github/workflows`，没走到 `.github/scripts`）。
⇒ 改为**两张表**：`SKIP_ANYWHERE`（构建缓存/VCS：`target`/`.git`/`node_modules`/… 任意深度）与 `SKIP_TOP_LEVEL`（`scripts`/`secure`/`dev-docs`，**只在顶层**跳过）；点目录除 `.github` 外仍跳过。**"按名字跳过"对一个**因位置**才有规则的目录本来就是错的工具** —— 这句写进了代码注释。
**实测**：夹具里 `crates/hydra-server/tests/check_probe.py` 与 `.github/scripts/probe.test.cjs` 都被发现并报 "never executed"；控制组（两步点名它们）通过；**真树不变**（47 个仓外工件、exit 0）。

### B. F10：三条 floor 各自的问题
- `MIN_SCRIPTS = 6` 而真树 **30** ⇒ 删掉 24 个 checker 都不会红（形同虚设）⇒ 提到 **20**；
- `MIN_E2E_SPECS = 3` 而真树**恰好 3** ⇒ 任何"两个 spec 合并成一个"的正当改动都会红（零余量）⇒ 降到 **2**；
- 规则 5 的**单一全局** floor（4 vs 47）可以被**一个类别**喂饱（真树 47 个里 **40 个**是 `integration/`）⇒ 新增**按类别 floor**（`integration ≥ 25`、`tools ≥ 4`）；
- 所有 floor 都补上**环境变量覆盖**（以前 `MIN_SCRIPTS`/`MIN_CRATE_TESTS`/`MIN_E2E_SPECS` **没有**覆盖口，夹具骨架根本建不起来，与其它守卫不一致）。
**测试 3 条**（脚本 floor 显式值必红、类别 floor 显式值必红、两个新目录里的未接线工件必被报 + 控制组），测试 harness 把六个 floor 全部显式置 0（每条 floor 都有自己的用例）。**顺带修掉一条既有用例**：`an empty skeleton trips the floors` 依赖 `MIN_SCRIPTS` 的默认值，现在改为**显式传值**。
**证伪**：①把类别 floor 的循环改成空数组 ⇒ **恰好**"类别消失"那条红；②在 walk 里重新跳过 `crates`/`.github` ⇒ **两条**"新目录里的工件"红；还原后 **ALL CI WIRING TESTS PASSED**、真树 exit 0、`FALSIFY` = 0。

### C. 队列剩余
产品 **P2-2**、**P3-1**…**P3-4**；守卫 **F9(b)**（`carrierUse` 过松）、**F14**（`--write` 的自我满足形状）。

**验证**：`check_ci_wiring` 真树 exit 0（47 仓外工件、类别 floor 满足）、测试 **ALL PASSED**（+3 用例，另有 1 条既有用例改为显式 floor）；`check_public_claims --measure` 仍 **811**；两枚探针已还原（逐文件 `FALSIFY` = 0，备份已用 `grep -c SKIP_ANYWHERE` 验证）。

**门禁 83 项全绿、`OVERALL=GREEN`、exit 0**（`ci wiring`/`ci wiring tests` 全 exit=0；两份转写 `FAILED` 计数 0）。

---

## 2dq. 第一百三十四轮：**F9(b)** "carrier 常量只要被当参数用过就算读"（两个真实用例其实都不是读）、**F14** `--write` 能用**别人的转写**把页面改成"通过"

### A. ★F9(b)：carrier 跳转必须**落在一次读**上，而不是"被当参数用过"
原判据是 `[(,]\s*IDENT\s*[,)]` —— 任何"作为调用实参出现过"都算"该常量被用来读环境"。**真树上两个用例其实都不是读**：`crates/hydra-server/src/listeners.rs:149` 的 `validate(LISTEN_ENV, plain)`（校验调用）与 `main.rs:1338` 的 `format!("{}={}", LISTEN_ENV, plan.plain)`（错误消息实参）；真正的那次读是 `listeners.rs:256` 的 `std::env::var(TLS_LISTEN_ENV)`。
**修法**：新判据 `carrierRead()` 要求跳转**终点是读**：①`env::var(IDENT)`/`env::var_os(IDENT)`（常量就是被读的变量名）；②`helper(IDENT, …)`，其中 `helper` 是**本仓**那个"通过形参读环境"的函数（由 `envReaderHelpers()` 从源码推导：`fn NAME(params)` 的体里出现 `env::var(参数)`，例如 `env_positive_u32`）。
**实测**：真树仍 44 个名字全有读点、exit 0（说明真实证据够用：`HYDRA_LISTEN` 有直接的 `env::var("HYDRA_LISTEN")`，`HYDRA_TLS_LISTEN` 有 `env::var(TLS_LISTEN_ENV)`）。
**测试 3 条**：①只被 `validate`/`format!` 用过 ⇒ **必须报未读**（改前会通过）；②控制组 `env::var(CONST)` ⇒ 通过；③控制组 `env_positive_u32(CONST, 5)`（同文件、体内 `env::var(key)`）⇒ 通过。
**★顺带纠正上一轮（第一百二十四轮）自己写的一条测试**：那条用例叫"a constant that CARRIES the name and is USED as an argument is evidence"，其夹具正是 `validate(TLS_ENV)` 这个**弱形状** ⇒ 新判据下它红了。已按新语义**改写并改名**（"…and is READ through it"），并在注释里写明"只要求'被当参数用过'也**太弱**"这一课，避免后人再放开。
**证伪**：把 `carrierRead` 换回"任意实参使用即算读" ⇒ **恰好**"只被 validate/打印"那条红；还原后 25/25、真树 exit 0。

### B. F14：`--write` + 别人的转写 = **自我满足**的检查形状
**复现**：页面写 700、转写是 200+500 ⇒ `--write` 把页面改成转写说的数并 **exit 0**（一条"能把自己弄通过"的命令，正是 CI step 绝不该有的形状）。
**修法**：默认**拒绝**"用转写改**计数**"：`--write` 只有在它**自己测量**（`--measure`）时才允许改变广告计数；**刷新日期**仍允许（计数没变 ⇒ 没有"弄通过"）。唯一的逃生口是**显式环境变量** `PUBLIC_CLAIMS_ALLOW_LOG_WRITE=1`（可 grep 的故意行为，测试用它保住改写机制本身的覆盖）。头部已写明这条契约。
**实测**：计数相同的"日期刷新"路径 ⇒ 不拒绝（有 note）；计数变化的 `--write`（无 `--measure`）⇒ **exit 2** 并给出"use `--measure --write`"的指引；`--measure` 正常路径不受影响。
**测试**：新增"`--write` 不得相信别人的转写"（断言 exit 2 **且页面未被修改**）；既有那条"--write refreshes the count and the date"改为显式设置该环境变量（保住机制覆盖），并在注释里说明原因。**证伪**：把 `mayRewriteFromLogs` 恒真 ⇒ **恰好**新用例红。

### C. 队列剩余
产品 **P2-2**（跨租户警告措辞：`matching_tenant` 不在 bucket 里）、**P3-1**（掩码进 Redis key 名 vs D-15 的 hash-first）、**P3-2**（edge 不评估该告警、admin 响应无提示）、**P3-3**（"掩码不动点"把全 `*` 串当掩码）、**P3-4**（raw 形明文凭据无 validate 警告）。守卫队列（S3–S13、F1–F14）**至此全部闭环**。

**验证**：`check_documented_env` 真树 exit 0（44 个名字）、测试 **25 passed**（+3，另改写 1 条）；`check_public_claims` 真树 `--measure` OK、测试 **27 passed**（+1，另 1 条加显式钩子）；`check_ci_wiring` exit 0；两枚探针已还原（逐文件 `FALSIFY` = 0）。

**门禁 83 项全绿、`OVERALL=GREEN`、exit 0**（`documented env wired`/`documented env tests`/`public claims`/`public claims tests` 全 exit=0；两份转写 `FAILED` 计数 0）。

---

## 2dr. 第一百三十五轮：产品复审的三条小项 —— **P3-4**（raw 形凭据只写在文档里，`validate` 不吭声）、**P2-2**（警告措辞把"掩码碰撞"漏掉）、**P3-3**（`Debug` 把全 `*` 串当掩码）

### A. P3-4 + P2-2：`matching_key` 的**值**现在各有具名警告
`config::validate` 原本只有"tenant 域缺失"那条警告（§2da），而两种写法**代价相反**、且都只在文档里提过：
- **非掩码值（即 raw 客户 key）**⇒ 该列是**明文**存储与复制（对比 provider api-key 的封存）、并被 `GET /api/v1/limit-roles` 原样返回 ⇒ 活凭据进每个节点的库、每份备份与每次管理面响应（D-16）。警告**按名**说出这件事并指出"掩码形能匹配同一把 key 而不必存它"。
- **掩码值** ⇒ 窗口是 `(role_id, mask(key))`**与租户无关**，任何**掩码相同**的别的 key 就**静默共用一个预算**（同租户内也一样，这正是 P2-2 指出的、原措辞会让人以为"设了 tenant 就安全"）。
每个角色**恰好一条**（if/else），并且**顺手修掉 §2da 那条警告里被我写坏的空格**（`its window is                  shared` —— 长串空格来自当轮 python heredoc 的续行）。
**实测（端到端）**：既有 drill 的 L7 用一把**原始** key 种角色 ⇒ 节点日志出现 `WARN hydra::store: config: limit_role 'r-cross' stores what looks like a RAW client key in \`matching_key\`: that …`，**已在 L7 里加断言**（同一份日志里同时断言 §2da 的 tenant 警告）；drill 全绿。
**文档**：`ops.md` §4 增加一条运维要点（两种形态各自的代价 + 各自会警告），措辞与代码一致。

### B. P3-3：`Debug` 的"是不是掩码"判定被全 `*` 串绕过
判据是"是不是 `mask_key` 的不动点"，而**任意长度的全 `*` 串都是不动点**（实测）⇒ 一个恰好长成 `******` 的凭据会被**逐字打印**，与该 impl 存在的理由相矛盾。**修法**：再加一条"必须含非 `*` 字符"。代价：长度 < 6 的 key 其**真掩码**也是全星 ⇒ 那种情况会被一起脱敏；这是**安全方向**的误差（少一点诊断信息，不会多打一个凭据）。**测试**：`Some("******")` 必须被脱敏（**证伪**：把判据改回"只看不动点" ⇒ `debug_never_prints_the_raw_client_key` 红）。

### C. 证伪与验证
三枚探针各自**只**打红自己那条用例：①去掉 raw-key 分支 ⇒ `validate_reports_what_the_matching_key_value_costs` 红；②去掉掩码分支 ⇒ 同一条红；③`Debug` 判据改回不动点 ⇒ `debug_never_prints_the_raw_client_key` 红。还原后 `--test validate` **21 passed**、`--test limit` **11 passed**、`FALSIFY` = 0。
**计数刷新**：新增 1 个 core 单测 ⇒ **258 core + 554 server = 812**，按规则**先** `--measure --write` **再**起门禁。

**验证**：`cargo fmt --check` 干净（启动门禁前）；`check_public_claims --measure` OK（812）；`integration/test_limit_roles_enforcement.py` 全绿（L7 现含 4 条检查）；**门禁 83 项全绿、`OVERALL=GREEN`、exit 0**（`limit_roles enforcement exit=0` 含 L7 的 4 条检查；`public claims` 812 一致；两份转写 `FAILED` 计数 0）。

---

## 2ds. 第一百三十六轮：把 F5/F6 那一**类**在**所有**守卫里扫一遍 —— 又找到三处"floor 落在早退之后 / floor 把漂移遮住"

前几轮只修了被点名的两个文件（`check_alert_expressions`、`check_e2e_contracts`）。本轮改成**按类横扫**：列出每个守卫的 `process.exit` 位置与 floor/汇总检查的位置，逐个核对**顺序**。

### A. 扫出三处（都是同一个病）
| 守卫 | 病 | 修法 |
|---|---|---|
| `check_documented_defaults.cjs` | `problems` 的 exit(1) 在 `MIN_COMPARED` floor **之前** ⇒ **抽取坏掉时那条"这条检查什么都没证明"永远不打印** | floor 并入 `problems`（单次退出、一次报全） |
| `check_tenant_error_codes.cjs` | 同上（`MIN_COMPARED`） | 同上 |
| `check_documented_metrics.cjs` | 顺序反过来：floor 的 `exit(2)` 在 `problems` **之前** ⇒ **floor 把漂移遮住**（正是我在第一百二十九轮给 alert 守卫踩过的那个"前移 floor 反而遮住漂移"） | floor 仍保持 exit 2（"没真正跑"比"漂移"更强），但**先把 `problems` 全打印出来**再退出 |
**实测（合并报告）**：defaults 夹具（文档 1 / 代码 2 + `MIN_COMPARED=50`）⇒ 一次运行同时打印 `DRIFT HYDRA_LOUD …` 与 `DRIFT only 1 documented default(s) could be compared (< 50) …`、exit 1；metrics 夹具（缺名 + `MIN_CHECKED=50`）⇒ 同时打印 `DRIFT … hydra_typo_total …` 与 `CANNOT VERIFY … only 2 documented name(s) checked`、**exit 2**（保住语义：没真正跑 > 漂移）。
**测试**：三个套件各加一条"**漂移 + floor 同一次运行都要报**"的用例；**证伪**：把三处条件分别置假 ⇒ 对应新用例（以及原有的 floor 用例）红；还原后三套全绿。

### B. 顺带核对（没问题的部分，记为负结论）
逐个核对了 `check_source_purity`/`check_compose_grace`/`check_compose_health`/`check_public_claims`/`check_redis_pool`/`check_i18n`/`check_documented_env`/`check_ci_wiring` 的 exit 位置：它们要么只有一个末尾 exit，要么 floor 已在 `problems` 之前并以 `problems` 统一退出（`check_ci_wiring` 用 floor→problems→单次 exit 的形态）。`check_i18n` 的 `exit(2)` 在打印 issues 之后、floor 判定之前 —— 属于**诊断顺序**问题（打印了不对的东西再报 CANNOT VERIFY），已在上一轮（§2dn）把"未闭合模板"这条最强的原因放在 floor 之前，其余保持。

**验证**：三个守卫真树 exit 0；三套测试全绿（各 +1 用例）；`check_ci_wiring` exit 0；`check_public_claims --measure` 仍 **812**；三枚探针已还原（逐文件 `FALSIFY` = 0，备份经 `grep -c "round 136"` 验证）。

**门禁 83 项全绿、`OVERALL=GREEN`、exit 0**（`documented defaults`/`documented default tests`/`documented metrics`/`documented metric tests`/`tenant error code tests` 全 exit=0；两份转写 `FAILED` 计数 0）。

---

## 2dt. 第一百三十七轮：**证据来源**这一类在第四个守卫里也成立（`check_tenant_error_codes` 认**注释**与**单测**里的"发射"），并给"case 被追加在 `process.exit` 之后 ⇒ 永远不跑"这个**踩过两次**的坑加了守卫

### A. ★`check_tenant_error_codes`：抽取跑在**原文**上 ⇒ 注释/单测里的出现都算"已发射"
该守卫的承诺是"文档里的每个错误码都真的以该状态码被发射"。**实测复现**（`TEC_DOC`/`TEC_SRC` 夹具）：
- 文档写 `unknown_path = 404`，而源码里**只有一行注释**提到它 ⇒ 守卫打印 `OK unknown_path = 404 (table says 404)`、**exit 0**；
- 同一行放进 `#[cfg(test)] mod tests` ⇒ 也 **exit 0**。
**修法**：抽取前用**单一所有者** `rust_blank.cjs` 的 `stripCommentsAndTestItems()` 屏蔽注释与整个 `#[cfg(test)]` 条目——**但保留字符串内容**（抽取要匹配 `"code_name"` 字面量，而这个函数对"非测试行"正是保留字面量）。**实测**：两种夹具都变成 **exit 1**；**真树仍 `30 个码匹配 / 3 个无法抽取`、exit 0** ⇒ **负结论**：仓库里没有任何被文档点名的错误码依赖注释或单测的"发射"。
**测试 3 条**：注释里的发射不被接受、`#[cfg(test)]` 里的不被接受、控制组（真实 `respond_error(..., 404, "unknown_path", …)` 站点）通过。**证伪**：把抽取改回读原文 ⇒ **恰好那两条**红（输出正是 `status=0 OK unknown_path = 404`）。

### B. ★★我第二次把用例写在 `process.exit` 之后 —— 于是给这个坑加了守卫
本轮 A 的测试块第一次被追加到 `check_tenant_error_codes.test.cjs` **末尾**，而该文件末尾是 `console.log(…); process.exit(…)` ⇒ 三条用例**从未运行**（我第一次探针"没变红"就是这个原因；**与第一百三十二轮 `check_documented_defaults.test.cjs` 完全同一个坑**）。两次都是**偶然**发现的（探针没红），所以本轮把它做成**机械检查**：
新增 `scripts/check_test_tails.cjs`（+ `check_test_tails.test.cjs` 6 条用例）：如果一个 suite 文件存在**顶格**的 `process.exit(` / `sys.exit(`（缩进的 `if (…) process.exit(1)` 是正常写法，不算），则其后**任何非空非注释行**都是**永远不会执行**的代码 ⇒ exit 1 并逐条点名。
**★第一版规则太窄**（只看顶格的 `check(`/`assert(`）——**恰好漏掉我自己那次**（我的用例是缩进在 `{ … }` 块里的）⇒ 改成"顶格 exit 之后的**一切代码**"，并用**真实夹具**（缩进块内的 `assert`）验证能抓到。
**实测**：真树 **63 个 suite 文件、其中 6 个有顶格 exit、没有一个后面还有代码** ⇒ exit 0；夹具（缩进用例 + 顶格 exit）⇒ DEAD 并点名行号；控制组（缩进的 `if (…) process.exit(1)` 后面照常写用例）⇒ exit 0；suite 文件太少 ⇒ **exit 2 CANNOT VERIFY**。**证伪**：让 `deadCases` 不收集任何行 ⇒ JS 与 Python 两条用例红。
**接线**：CI `scripts` 作业 +2 步、本机门禁 +2 条（**83 → 85 项**）；`check_ci_wiring` 在这之前就报 `UNWIRED` 逼我把两件工件接上（这次它先于我注意到）。

**验证**：`check_tenant_error_codes` 真树 exit 0、测试 15 条；`check_test_tails` 真树 exit 0、测试 **6/6**；`check_ci_wiring` exit 0；`check_public_claims --measure` 仍 **812**；两枚探针已还原（逐文件 `FALSIFY` = 0，备份经 `grep -c` 验证）。

**门禁 85 项全绿、`OVERALL=GREEN`、exit 0**（新增两条 `test tails (no dead cases)`/`test tail checker tests` 均 exit=0，`tenant error codes`/`tenant error code tests` exit=0；两份转写 `FAILED` 计数 0）。

---

## 2du. 第一百三十八轮：**P3-2 先测量再下结论** —— "告警只在启动时出现"其实是错的（**写入路径自己就会 reload 并告警**），并把 mask 形告警也纳入端到端

### A. 先读代码、再实测（结论比复审说的窄）
产品复审（P3-2）说：`config::validate` 只在 `build_config` 路径产生告警 ⇒ ① 启动时到达、② `POST /reload` 到达、③ admin 写入只到 leader 日志、④ edge 永不评估、⑤ HTTP 响应体里没有提示。
**核对代码**：`admin/handlers.rs` 的 `limit_role_collection` 在 `insert_limit_role` 之后**自己**调 `reload_best_effort(state, trace_id)`（`handlers.rs:31/32`）——也就是说**写入路径就会重新加载配置**，于是 `validate_and_log` 在**写入那一刻**就会跑。
**端到端实测（新增 L9）**：**故意不发** `POST /reload`，直接 `POST /api/v1/limit-roles` 一个 `matching_key` 为原始 key 的角色 ⇒ `POST -> 201`，且**这次写入新增的日志**里就有 `RAW client key` 告警（断言同时检查 `r-nowarn-probe` 与 `RAW client key` 出现在"写入之后新增的日志切片"里）⇒ 复审的第 ③ 条**不成立**（不是"只在启动时"）。
**同时补齐上一轮（§2dr）只做了单元测试的那一半**：再写一个**掩码形**角色 ⇒ 断言日志出现 `matches on the MASKED form 'sk***************ed': any OTHER client key whose mask is that same string shares this window`（mask 形告警现在也有端到端证据）。

### B. 剩下两条（真实但不值得单方面改）
- **edge 永不评估**：`apply_snapshot` 有意不 re-validate（代码注释与计划里都记过：**配置校验归 leader**）。让每个 edge 都跑一遍校验会在启动/热更新时引入 leader 与 edge 的语义分叉（edge 没有租户域与写入边界），**属产品决策**，因此本轮只**写清事实**。
- **HTTP 响应体里没有提示**：改响应形状（加 `warnings` 字段）是**对外契约变更**，同样不单方面做；但至少在 `ops.md` 里告诉脚本化运维"干净的 201 不等于角色没问题，去看 leader 日志"。
⇒ `ops.md` §4 新增一条"**这些告警在哪里出现**"：启动时、每次配置加载、以及**写入时**（写入路径会 reload，无需显式 `/reload`）；**不在**响应体里；**edge 不会重新校验**收到的快照。
**决策侧**：把"是否让 admin 写响应回带校验告警"记为新的决策项 **D-17**（小项，但不该单方面改对外形状）。

**验证**：drill 全绿（L9 两条新断言 + 上一轮 L7/L8 全在）；`check_documented_env`/`check_alert_expressions`/`check_documented_defaults`/`check_documented_metrics`/`check_test_tails` 真树全部 exit 0（ops.md 改动没有破坏文档守卫）；`check_ci_wiring` exit 0；`check_public_claims --measure` 仍 **812**。

**门禁 85 项全绿、`OVERALL=GREEN`、exit 0**（`limit_roles enforcement exit=0` 含 L9；文档守卫与 `test tails` 均 exit=0；两份转写 `FAILED` 计数 0）。

---

## 2dv. 第一百三十九轮：**审我自己上一轮刚写的守卫** —— `check_test_tails` 会被"写在模板字符串里的 `process.exit`"骗成**假阳性**（第五次同一族）；并把 P3-1 的真实障碍记进 D-15

### A. ★`check_test_tails.cjs` 的字符串盲区（我自己上一轮刚写的代码）
该守卫逐行看**原文**，于是**一行以 `process.exit(0);` 开头的模板字符串内容**被当成"顶格的 exit" ⇒ 其后**真实的用例**被报成**永远不会执行**的代码。**实测复现**（夹具：`const FIXTURE = \`\nprocess.exit(0);\n\`;` 之后跟着真实用例）⇒ **exit 1 + DEAD 点名**。这正是我已经在**四个**守卫里修过的"文本匹配不认字面量"那一族（`'{'`、URL 里的 `//`、消息字符串里的字段名、e2e 的掩码顺序）—— 第五次出现，且这次是**我自己**新写的代码。
**修法**：扫描前先**保偏移地屏蔽字面量与注释** —— JS/TS 复用单一所有者 `scripts/js_blank.cjs` 的 `maskJsLiterals`，Python 用新写的 `maskPythonLiterals`（行注释、单/双引号、三引号；偏移与行号保持不变，所以报告的行号仍然正确）。**实测四向**：①模板字符串里的 exit ⇒ **不再被误判**（exit 0）；②Python docstring 里的 `sys.exit(0)` ⇒ 不再误判；③**真实的** Python 陷阱（顶格 `sys.exit(0)` 后还有代码）⇒ 仍然 DEAD 点名；④真树 ⇒ exit 0 且"6 个文件有顶格 exit、没有一个后面还有代码"。
**测试**：新增两条**假阳性回归**用例（模板字符串、Python docstring），并把它们与既有的"真陷阱必须被抓"用例放在一起 —— **证伪**：把扫描改回读原文 ⇒ **恰好这两条**红（真陷阱用例照过），还原后 **8/8**。

### B. P3-1 并入 D-15，并把它真正的障碍写清楚（**滚动升级**期间会让限流近似翻倍）
产品复审的 P3-1：集群模式下 bucket（=**掩码**，长 key 保留首 10 尾 4）直接进 Redis key 名（`hydra:{rl:<role>:<bucket>}:count|tokens`）⇒ Redis keyspace/慢日志/监控里能看到凭据片段。自然的收敛方向是**哈希**，但**不能单方面做**：key 的**名字**一变，滚动升级期间**旧节点算掩码、新节点算哈希** ⇒ 同一客户端的请求被记进**两个窗口**、限流在那段时间里近似**翻倍**（fail-open 方向），旧窗口成为孤儿（等过期）。⇒ 需要**迁移方案**（双写/双读一段时间，或整批停写窗口）。
已把这段（含"为什么它是决策而不是修复"）**写入决策表 D-15 行**，因此复审队列里最后一条也**有了明确归属**。

**验证**：`check_test_tails` 真树 exit 0、测试 **8/8**（+2 假阳性回归）；`check_ci_wiring` exit 0；`check_public_claims --measure` 仍 **812**；一枚探针已还原（`FALSIFY` = 0，备份经 `grep -c maskPythonLiterals` 验证）。另：本轮按原始指令**再派一个只读 Oracle**（审最近五轮新增的守卫：`check_test_tails`、`check_tenant_error_codes` 的屏蔽、两个 floor 合并、`compose_static` 的层级/子映射、`check_ci_wiring` 的遍历与按类 floor），结果留到下一轮处置。

---

## 2dw. 第一百四十轮：只读 Oracle 复审最近五轮的守卫 —— 21 条发现；本轮先修两条 **P1**（一个**崩溃**、一个**静默丢服务**）

按原始指令又派了一个只读 Oracle 审"最近五轮新增的守卫"（`check_test_tails`、tenant-codes 的屏蔽、两个 floor 合并、`compose_static` 的层级/子映射、`check_ci_wiring` 的遍历与按类 floor）。报告 **21 条**（含大量"查过、确认承重"的负结论）。本轮修其中两条 P1，其余按优先级记入下一轮队列。

### A. ★P1-1：`check_tenant_error_codes.cjs:93` 调用了一个**本文件里不存在**的函数 ⇒ 一命中就**崩**
`const fn = functionBody(sources, helperName);` —— `functionBody` 只定义在 `check_documented_defaults.cjs`，本文件没有；`main()` 无 try/catch。**实测复现**（内存夹具，两个函数：字面量上一行以 `short_circuit_rate_limited(` 结尾）⇒ `ReferenceError: functionBody is not defined`，进程带 stack trace 退出，**exit 1** —— 而该守卫的 header 把 1 定义为"a mismatch"，于是"诚实的 CANNOT VERIFY（exit 2）"出口被绕过。**这形状正是它自己注释里写的 shape 5**；今天潜伏，因为真实树里最接近的站点被更早的 shape 先 `continue` 掉了。
**修法**：把 `functionBody` 补进本文件（与 defaults 版同构：找 `fn <name>`，取到下一个 `
}` 或 1200 字符）；找不到 helper 就返回 null ⇒ 该形状**落回"无法抽取"的诚实路径**，而不是崩。
**实测**：同一夹具不再崩、结果为 `[]`（该行将被报为无法抽取）；真树 exit 0。**测试**新增两条（不崩 + 该行仍被如实报告）；**证伪**：把函数删掉 ⇒ 新用例复现**同一条** `ReferenceError`。

### B. ★P1-2：`compose_static` 把**注释**当代码 —— 两种写法都会**静默丢服务**
- **列零注释**：`services:` 里一行列零 `# …` 被 `/^\S/` 当成"新的顶层 key" ⇒ **mapping 就此结束**，其后的服务**整个消失**（实测 `serviceBlocks` 只剩 `hydra-a`，`hydra-edge` 连块都没有）。
- **行尾注释**：服务名后带注释（`redis:   # local cache`）不被服务名正则识别 ⇒ 该服务自己的键被**并进上一个服务**。**在真实文件上实测**：把 `environment/docker-compose.local.yml` 的 `  redis:` 改成 `  redis:   # local cache` ⇒ `hydraServices` 从 `hydra-a,hydra-b,hydra-c` 变成 **`hydra-a,hydra-b`**（本地 edge 的 grace/healthcheck **一次都不查**），而 `unjudgeableServices` 仍为空（不触发拒绝）、三个文件合计 floor 4（7→6）**照样满足** ⇒ 守卫打印 `OK … 6 hydra service(s) checked`。
**修法**：①跳过"首个非空字符是 `#`"的行（注释不是 key）；②服务名正则允许行尾注释：`/^  ([A-Za-z0-9_.-]+):\s*(?:#.*)?$/`。
**实测**：列零注释后用例恢复 `hydra-a,hydra-edge`；真实文件加行尾注释后仍 `hydra-a,hydra-b,hydra-c`；未改动的真树不变；两个 compose 守卫 exit 0。**测试 3 条**（列零注释、行尾注释、**真实文件回归**：给真文件加行尾注释不得改变可见服务集合）；**证伪**：把两处改回 ⇒ **恰好这 3 条**红（其余 7 条照过）。

### C. 记入下一轮队列（含证据，未在本轮修）
- **P1-3（我的 `check_test_tails` 只认"列零"的 exit）** —— **已在 §2dx 修复（第一百四十一轮）**：现识别三种形状，实测判定 **44/64**（列零 exit 6 + Python `__main__` 且块体 exit 37 + IIFE 1），OK 行如实写明余下 20 个"本规则不建模、**未查**"，并加 `MIN_JUDGED` floor（`CANNOT VERIFY` / exit 2）。**数字更正**：Oracle 报的 41 是"**含**缩进 exit 的文件数"（复测 = 41），而"**最后一行代码**是缩进 exit"的是 **37**；原报告引用的旧 OK 行与 `0 suite file(s) with a top-level exit` 当 PASS 的 CONTROL 用例都已改写。
- **P2**：`check_tenant_error_codes` 的"一致"= 任一站点一致（多站点漂移不可见，`not_found` 33 处等是常态）；S6 的 ±1500 窗口会把**隔壁 handler 的 status** 记到本 code 名下（假 DRIFT）—— **已修（第一百四十八轮，§2ee）**：status 现在绑定到**发送该 body 的那个调用**（不越函数、优先括号区间包含字面量的调用、否则取其后最近者），真树输出与修复前**逐字节相同**；shape 5 在**真实**的分行调用上从不生效（`proxy.rs:861/887` 命中集合为空，`rate_limited` 的 429 证据全来自另一子系统 ⇒ 把 proxy 的 429 改成 418 守卫仍绿）—— **已修（第一百四十七轮，§2ed）**：helper 名沿参数表**向上回溯**（停在语句终结行）⇒ 对**真实 `proxy.rs` 副本**实测：未改动 `OK 429` exit 0、把 `build(429` 改成 `418` ⇒ **DRIFT 且 exit 1**。`compose_static` 仍缺 header 自己点名的 **YAML anchor / merge 键**（真实 `docker-compose.cluster.yml:45` 已有 `&control-env`、`:102` 有 `<<: *control-env` ⇒ `envValue(hydra-control-a,'HYDRA_ROLE')` 读不到那行明写的 `leader`）—— **已在 §2dy 修复（第一百四十二轮）**：键行容忍锚点、`collectAnchors`/`mergeRefs` 解析合并（自己的键优先）、无法解析的合并（锚点不在本文件 / 列表形 / **服务级** `<<:`）改为**具名拒绝**；真文件 control-a/b 由 `null`（消费方静默变成 `all`）变为 `leader`，静态路径与渲染路径逐条一致。`check_ci_wiring`：block scalar 被空格 join ⇒ 其中一行 shell 注释会**吞掉整个 step**（假 UNWIRED）；rule 4 在**同一 step 的另一条命令**里找 `--ignored`（假"已跑"）—— **前两条已修（第一百四十五轮，§2eb）**：块标量改按**行**拼接、规则改用**逐命令**的 `logicalCommands()`；不建模 job 边界（job 级 `env:`/`defaults:` 并入上一步）—— **已修（第一百四十九轮，§2ef）**：job 感知扫描器 + job 级 `if:`/`defaults:` 建模，实测旧解析器会**冤枉** job a 的最后一步（`cond="false"`）又**放过**永不运行的 job b。
- **P3**：`-p <crate>` 子串覆盖、`invokes()` 不认 glob、`extractRunCommands` 死代码、`check_documented_defaults` 的 code 侧仍读原文（**真实输出已把证据行指到注释**：`usage_query.rs:340`，真 fallback 在 343）—— **已修（第一百四十三轮，§2dz）**：改用 `stripCommentsAndTestItems()`，witness 行 340 → **343**，真树 23/10 不变；metrics 的 4 条 allowlist **全是死条目**（文档里只有通配 `hydra_proxy_listener_*`，OK 行却说它们"deliberately absent"）—— **已修（第一百四十四轮，§2ea）**：内建表清空 + **未被咨询的条目 = DRIFT** + OK 行只报"实际被需要"的条数与跳过的通配数；常量解析只认 `: &str`、floor 数 occurrence 而非 distinct、tenant-codes 的不可比行不入 problem 列表、三个 floor 无独立用例、`compose_static.test.cjs` 用 `scalar()` 断言而守卫用 `scalarOwn()` —— **后者已修（第一百四十四轮，§2ea）**：断言改用守卫的读取器（诱饵夹具实测旧断言通过而 grace 守卫 exit 1）。

**验证**：`check_tenant_error_codes` 真树 exit 0、测试全绿（+2）；`compose_static` 10/10；两个 compose 守卫 exit 0；`check_ci_wiring` exit 0；`check_public_claims --measure` 仍 **812**；两枚探针已还原（逐文件 `FALSIFY` = 0，备份经 `grep -c` 验证）。

**门禁 85 项全绿、`OVERALL=GREEN`、exit 0**（`compose config`/`compose stop grace`/`compose grace tests`/`compose static reader tests`/`compose healthchecks`/`compose health tests`/`tenant error code tests` 全 exit=0；两份转写 `FAILED` 计数 0）。

---

## 2dx. 第一百四十一轮：修 §2dw 的 **P1-3** —— `check_test_tails` 只认"列零 exit"，OK 行把"只查了 6 个"与"全都查过"印成同一句

§2dw 的 C 段把 P1-3 记进队列（"64 个 suite 文件里只有 6 个被认出有 program end，而这 6 个全在 `scripts/`，其**余**文件只有**缩进** exit"）。本轮修完，并把两处**数字**与一处**措辞**按实测更正。

### A. 三种"程序结束"形状 + 判定数 floor

`programEnd()` 现识别三种形状（`how` 是**形状名**，用于诊断文案）：
1. 列零 `process.exit(` / `sys.exit(`；
2. Python `if __name__ == "__main__":` 块**且块体内 exit**（仅调 `main()` 不算：其后代码**真的会跑**，`integration/test_crud.py` 就是这样）；
3. 列零 IIFE **且体内 exit**（`scripts/admin_ui_render.test.cjs`）。

**实测（真树 64 个 suite 文件）**：**44 个**被判定 —— 列零 exit **6** + Python `__main__` 且块体 exit **37** + IIFE **1**；余 **20** 个（其中 3 个 Python：`test_crud.py`、`test_error_contract.py`、`tools/hydra-py/tests/test_client.py`，它们只 `main()` 不 exit）是**本规则不建模的形状**。OK 行改为如实分区：`OK (44 of 64 suite file(s) have a recognized program end and none has code after it; 20 end in another shape that this rule does not model and were NOT checked)`。
**★我删掉了半句"想当然"**：初稿写的是 `… 20 end in another shape, where appended code still runs` —— "追加的代码照样会跑"对**逐个文件**都**没有验证过**（完全可能存在"缩进在 `try{}` 里的 exit"这种未建模但同样会终止的形状）⇒ 半句事实（"不建模"）保留、半句**断言**删掉。这正是本计划反复出现的"OK 行必须只说查过的东西"。

**floor**：`MIN_JUDGED`（默认 30，可用 `CTT_MIN_JUDGED` 覆盖）—— 判定数低于它即 **exit 2 CANNOT VERIFY**（"规则没覆盖到它存在的理由"），头部 `Exit:` 说明同步改写。测试 harness 把两个 floor 都置 `'0'`（沿用纪律：floor 自己有独立用例，夹具不该被它干扰），并新增一条"判定数不够必须拒绝"的用例。

### B. 诊断文案不得硬编码形状（我自己的用例抓到的）

DEAD 行原先写死 `… AFTER the top-level exit at line N`，对 Python/IIFE 两种形状会把读者指向一个**不存在的"列零 exit"行** ⇒ 改为 `${d.how}`（汇总行同步改 `with code after their program end`）。**这是我新写的用例 9/11 变红后发现的**（两例都断言形状名出现在诊断里）—— 即"报告文案与判定同源"。

### C. ★探针**没有**变红 ⇒ 发现一条真机制：形状检查是**单一承重件**，而我差点让它变成"两个各查一半"

第一轮证伪里，把 `programEnd` 里的形状前置过滤 `if (!/^if __name__ ==\s*['"]/.test(lines[i])) continue;` 反转成 `false && …` 后 **14 条用例仍全绿**。原因不是探针没生效（已用 `grep` 确认探针文本落入文件），而是 `pythonMainBlock(lines, i)` **只按缩进**算块尾、**从不检查 `lines[i]` 是不是 `if __name__` 行** ⇒ 前置过滤才是唯一的形状检查，而那条用例（真 `__main__` 块）在**两条路径**下都被认出。
**由此暴露一个真实假阳性**：没有任何形状检查时，**任何**"下一行缩进更深"的行只要其块内出现 `sys.exit(` 就被当成"程序结束"。**补的用例**（`def main():\n    sys.exit(1)\n\ntest_appended()`：没人调用 `main`，追加的用例**真的会跑**）在反转后**恰好**红。
**修法（anti-entropy：单一所有者）**：把形状检查**上移进 `pythonMainBlock`**（连同"掩码文本里 `"__main__"` 内容被抹成空格、必须匹配引号形"的注释一起搬过去），`programEnd` 里那份**重复条件删掉** ⇒ 一处判定、一处可证伪。

### D. 掩码细节与数字更正（含 §2dw 里的 41）

- 掩码跑的是 `maskPythonLiterals`（**引号保留、内容抹空**）⇒ 匹配字面 `"__main__"` 会**一个 Python 结束都认不出**。**实测对照**（同一份真树、只改这一处）：**引号形 44/64** vs **字面文本形 7/64**（= 只剩 6 列零 + 1 IIFE）。我此前注释里写的"42"是**估的**，已改为实测 **44**；`MIN_JUDGED` 注释也改为列出 6/37/1 的分解。
- §2dw 引用的 **41**：复测确为 **41**，但那是"**含**任意缩进 exit 的文件数"（含写在函数体里的）；"**最后一行代码**是缩进 exit"的是 **37** —— 正是本轮认出的数目。两个数字都在 §2dw 那一条里写明，避免"同一个事实两个值"。

### E. 证伪（四枚探针，每枚先确认**已应用**）

| 反转对象 | 期望变红 | 实测 |
| --- | --- | --- |
| `pythonMainBlock` 的形状检查（→ `false &&`） | 新假阳性用例 | **红 1 条**（`FALSE POSITIVE guard: a sys.exit inside an indented body (no __main__) is not an end`） |
| `jsIifeEnd` 的返回分支（→ `false &&`） | IIFE 用例 | **红 1 条** |
| 列零 exit 分支（→ `false &&`） | 干净通过 / 真陷阱 / Python 列零 | **红 3 条** |
| 诊断里的 `${d.how}`（→ 硬编码 `the top-level exit`） | Python/IIFE 两条 | **红 2 条** |

每枚探针都用 `grep` 确认替换文本已落入文件（第一轮的"没变红"就是被这里问出来的），还原用**验证式备份** `.acceptance/round141/test_tails.cjs.r141-final`（`md5 = 09bd9fd72a323e2ecb5ac1d6a1a058c8`、`grep -c "were NOT checked"` = 1），还原后 **14/14** 且 `grep -rn FALSIFY crates/ integration/ scripts/` = **0**。

### F. 验证

`node scripts/check_test_tails.cjs` 真树 **exit 0**（`44 of 64 …`）；`node --test scripts/check_test_tails.test.cjs` **14/14**；CI（`.github/workflows/ci.yml:812/815`）与门禁（`.acceptance/round10-gate.sh:59/60`）均已接线，无需改动。**本轮无产品代码改动**（1 个守卫 + 1 个测试文件 + 文档）。

---

## 2dy. 第一百四十二轮：清掉 §2dw 队列里 `compose_static` 的 **YAML anchor / merge 键** 盲区 —— 一个**明写在文件里**的角色被静默读成 `all`

§2dw 的 P2 条目写着"`compose_static` 仍缺 header 自己点名的 YAML anchor / merge 键（真实 `docker-compose.cluster.yml:45` 已有 `&control-env`、`:102` 有 `<<: *control-env`）"。本轮修完。

### A. 复现：明写的 `leader` 读成 `null`，再由消费方**静默替换**成 `all`

- `environment/docker-compose.cluster.yml:45` 是 `    environment: &control-env`，其下 `HYDRA_ROLE: leader` **明写**在 `hydra-control-a` 自己的块里；但 `subBlock()` 的键行判据要求**恰好** `key:` ⇒ 锚点让**整段子映射不可见** ⇒ `envValue(block,'HYDRA_ROLE')` = `null`（同一块里的 `HYDRA_REDIS_URL`/`HYDRA_REDIS_MODE`/`HYDRA_NODE_ID` 一起消失）。
- `hydra-control-b`（`:102`）只写 `<<: *control-env`，合并键根本没被解析 ⇒ 角色同样 `null`。
- **后果由消费方决定**：`check_compose_health.cjs:156` 是 `role: (envValue(block, 'HYDRA_ROLE') || 'all').toLowerCase()` ⇒ **把 `leader` 静默说成 `all`**。两个可观测后果：①**两条提取路径互相矛盾**（`docker compose config` 渲染路径打印 `role=leader`，静态路径打印 `role=all`）——而本模块的整个存在理由（第一百二十一轮）就是"两条路径不能漂移"；②**假 FAIL**：任何**经锚点/合并**拿到 `HYDRA_ROLE: edge` 的服务，会被按 `all` 判，要求它去探**它没有的** admin API（`/api/v1/health` + `Authorization: Bearer`），而 edge 上该路径是 404。

### B. 修法（全部落在单一所有者 `compose_static.cjs`）

1. `subBlock()` 的键行判据允许**锚点**与**行尾注释**：`^\s+KEY:\s*(?:&[\w-]+)?\s*(?:#.*)?$`（此前 `^\s+KEY:\s*$`）。
2. 新增 `collectAnchors(text)`：收集 `key: &name` 及其**子映射行**（`name → { owner, key, lines }`）。
3. 新增 `mergeRefs(lines)` + `envValue(block, name, anchors)`：**服务自己的键优先**（YAML 合并语义），只有自己的映射里没有该名字时才查锚点；`anchors` 非 `Map` 或名字不在表里 ⇒ 返回 `null`（由第 4 点拒绝，而不是默认值）。
4. `unjudgeableServices(text)` 扩展出三类**具名拒绝**（调用方 `CANNOT VERIFY`）：锚点**不在本文件**（含跨文件合并，读不到就是读不到）、**列表形** `<<: [*a, *b]`（不猜谁赢）、**服务级** `<<:`（此时 `image:`/`stop_grace_period:`/`healthcheck:` 都可能来自锚点，文本读法根本管不了这三样）。
5. 顺带 anti-entropy：`ownKeys` 与新的服务级合并判定共用同一个"最小缩进"判定 `ownIndent()`（此前缩进计算散在 `ownKeys` 里）。

### C. 实测（真文件，改前 → 改后）

`hydra-control-a`：`null` → **`leader`**；`hydra-control-b`：`null` → **`leader`**（经 `<<: *control-env`）；`hydra-edge` 仍 `edge`；`unjudgeableServices(cluster)` = **`[]`**（真文件没有无法解析的合并 ⇒ **没有引入假拒绝**）。`node scripts/check_compose_health.cjs --static-file=environment/docker-compose.cluster.yml` 现在对 control-a/control-b 都打印 `(role=leader)`，与**渲染**路径逐条一致（此前静态 `all` vs 渲染 `leader`）。

### D. 测试 +8 与证伪 7 枚

**parser 10 → 16**：①锚点行不隐藏子映射；②合并来的值可读 + **自己的键胜出**（同一夹具里 `hydra-c` 自写 `HYDRA_ROLE: edge` ⇒ `edge`）；③**控制组**：不传锚点表时合并值仍是 `null`（拒绝猜）；④不可解析合并被具名拒绝（含锚点名）+ 列表形被拒绝；⑤服务级合并被拒绝；⑥**真实 cluster 文件回归**（三个角色逐个断言 + `unjudgeable` 为空）。
**health 15 → 17**：⑦合并来的 `edge` 按 edge 判 ⇒ **exit 0**（改前会因"缺 admin 探针"假 FAIL）；⑧不可解析合并 ⇒ **exit 2** 且错误信息含锚点名。

**证伪（每枚先确认替换文本已落入文件）**：①去掉 `subBlock` 的锚点容忍 ⇒ 新用例 + 真文件回归 **2 红**；②`mergeRefs` 置空 ⇒ **2 红**；③"自己的键优先"反转 ⇒ **4 红**；④拒绝逻辑置空 ⇒ **1 红**；⑤服务级合并拒绝置假 ⇒ **1 红**；⑥守卫不传 `anchors`（`envValue(block,'HYDRA_ROLE')`）⇒ guard 级 **1 红**；⑦守卫侧拒绝逻辑置空 ⇒ guard 级 **1 红**。还原用**验证式备份** `.acceptance/round142/*.postfix`（`md5 = ec9ccdbd…/5944e590…`，并经 `grep -c collectAnchors`/`grep -c anchoredFixture` 验证），还原后 16/16、17/17。
**一次探针事故（同类第三回）**：第一版探针用 `sed` 表达式去改模板字符串里的正则，`PROBE[anchor-on-key-line] DID NOT APPLY` —— 因为文件里是 `\\s`（模板字符串里的两个反斜杠）、而 `sed` 表达式里只写了一个；改为**Node 字面量替换 + 应用检查**（并确认片段在文件里唯一）后探针才真正生效 ⇒ **规则：探针必须自证已应用**。

### E. 验证

`compose_static.test` **17/17**、`check_compose_health.test` **18/18**、`check_compose_grace.test` **19/19**（三套合计 **54/54**；**这类计数会随测试增长而变，复核时请以 `node --test scripts/<x>.test.cjs` 的 `# pass` 为准**——第一百五十五轮复审指出本计划里若干 N/N 数字已漂移）；`check_compose_grace`/`check_compose_health`/`check_ci_wiring` 真树 **exit 0**；`check_public_claims --measure` 仍 **812**（本轮只改 JS 守卫与测试，Rust 计数不变）；`grep -rn FALSIFY crates/ integration/ scripts/` = **0**。**本轮无产品代码改动**（1 个共享模块 + 1 个守卫 + 2 个测试文件 + 文档）。

**队列剩余**：§2dw 的 P2 其余三条（tenant-codes 的"任一站点一致"、"±1500 窗口把隔壁 handler 的 status 记到本 code 名下"、shape 5 在真实分行调用上从不生效；`check_ci_wiring` 的 block-scalar 注释吞 step、rule 4 跨命令取 `--ignored`、不建模 job 边界）与 P3 全表（`-p <crate>` 子串、`invokes()` 不认 glob、`extractRunCommands` 死代码、defaults 的 code 侧读原文、metrics 的 4 条死 allowlist、常量只认 `: &str`、floor 数 occurrence、tenant-codes 不可比行不入 problems、三个 floor 无独立用例、`compose_static.test.cjs` 的 `scalar()` vs `scalarOwn()`）。

---

## 2dz. 第一百四十三轮：**我自己的并发命令把门禁弄红了** —— 门禁条目依赖"别人刚好建出来的二进制"；顺带清掉 defaults 守卫的"注释即证据"

本轮两件独立的事，都是"证据来源"这一类：一件是**门禁条目**的证据（跑的是哪个二进制），一件是 **defaults 守卫**的证据（读的是注释还是代码）。

### A. ★门禁 RED 的根因：条目继承了别的条目建出来的二进制

**现象**：本轮跑完 `.acceptance/round10-gate.sh`（85 项）得到 **`OVERALL=RED`**，只有两条 exit=2：`replica fidelity` 与 `cluster rate limits (shared+FO)`，原因都是
`ERROR hydra: fatal startup error error=HYDRA_ROLE=leader requires the 'cluster-redis' cargo feature (rebuild with --features cluster-redis); refusing to start`。
**排查**：两条 drill 本身**不建**二进制（它们的 header 只是**写明**这条前置命令：`integration/test_replica_fidelity.py:29`、`test_cluster_limits.py:35`，运行时只读 `HYDRA_BIN`/`target/debug/hydra`）；门禁里这两条**没有** `cargo build`（对比：`clickhouse sink wire format`/`cluster HA`/SDK 三条**有**）。
**真正的触发者是我自己**：门禁进行到一半时我在同一 checkout 里跑了 `check_public_claims --measure` —— 它是 `cargo test -p hydra-server --features server`，会**重新链接 `target/debug/hydra`（不带 `cluster-redis`/`usage-clickhouse`）**。前一条条目（`auth cache layers`）刚建好的集群二进制被我这次运行覆盖 ⇒ 紧随其后的两条 drill 拿到"缺 feature 的二进制" ⇒ 诚实报 **CANNOT VERIFY(2)** ⇒ 门禁 RED。**串行跑门禁本身不会红**（`--measure` 条目在最前面），是**并发**把它弄红的。
**这是真脆弱性，不只是我的操作失误**：门禁的 35 条 drill 都读同一个 `target/debug/hydra`，其中 3 条读的 drill **自己声明了**构建前置条件却由条目继承了别人的构建结果 ⇒ 任何并行的 `cargo` 命令（开发者手跑一次 `cargo test`）都能让门禁变红，或者更糟——**让 drill 跑在一个与当前源码/特性不符的二进制上**。

**修法（两步）**：
1. **让条目自给自足**：给三条"drill 声明了构建、条目却没建"的条目补上构建（与 CI 同形，`.github/workflows/ci.yml` 每一步都自己 `cargo build … --bin hydra`）：`tenant usage read over CH`、`replica fidelity`、`cluster rate limits (shared+FO)`；并在文件里写下测量经过（"drill 绝不能依赖别的条目刚好建了什么"）。
2. **加守卫 `scripts/check_gate_entries.cjs`（+ 测试 7 条、CI 2 步、门禁 2 条 ⇒ 门禁 85 → 87 项）**：契约由 **drill 自己写明**——凡 drill 源码里写了 `cargo build -p hydra-server … --bin hydra`，跑它的条目必须**在同一条目里**做这次构建；另外守住"门禁脚本必须**返回**自己的判定"（`exit "$overall"`；历史事故：脚本以 `echo` 结尾 ⇒ 摘要写 RED 而 exit 0）。三条 floor：条目数（40，真树 87）、被判定的前置条件数（4，真树 **5**）、脚本不可读/太短 ⇒ `CANNOT VERIFY`；floor 与漂移**同时打印**，floor 保留更强的 exit 2。
   **★floor 先写错再改对**：我按感觉写了 `MIN_JUDGED=10`，真树只有 **5** ⇒ 守卫立刻红并打印 `only 5 … (< 10)` ⇒ 改为 **4**（低于实测、留余量；**等于实测的 floor 是零余量**，第一百三十三轮已吃过一次）。

**证伪（真脚本，每枚先确认已应用）**：①把 `replica fidelity` 条目改回不建构建 ⇒ `DRIFT entry "replica fidelity" (line 198) … does not build it`（exit 1）；②把 `exit "$overall"` 换成 `echo "OVERALL=$overall"` ⇒ `DRIFT the gate script does not end by RETURNING its verdict`（exit 1）；③`CGE_MIN_JUDGED=99` ⇒ **exit 2** 且 `CANNOT VERIFY only 5 …`；还原后守卫 exit 0（`md5 = 391eccfb…`）。
**端到端证伪（最有说服力的一枚）**：先把二进制**故意弄成 feature-poor**（`cargo build -p hydra-server --features server --bin hydra`，正是 `--measure` 的后果）⇒ 旧条目命令 **exit 2**（复现门禁 RED 的那条错误），新条目命令（自己建）**exit 0 + `REPLICA FIDELITY: PASSED`** ⇒ 修的是"依赖"，不是"现象"。

### B. 清掉 defaults 守卫的**注释即证据**（§2dw 队列 P3 的 `usage_query.rs:340` 那条）

**复现**：`check_documented_defaults` 的 code 侧跑在**原文**上 ⇒ `usage_query.rs` 里 `HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS` 的第一处提及是 **340 行注释**（真读取在 **343** 行）⇒ 守卫给出的**witness 行号是注释**；更糟的是**注释能伪造证据**：夹具里只有一行注释写着 `// env_millis("HYDRA_TEST_KNOB_0", 7)`、真实代码 `unwrap_or(9)` ⇒ 旧版 **exit 0 + `OK HYDRA_TEST_KNOB_0 = 7 … @ knobs.rs:1 (helper called with an inline default)`**（把注释当默认值）。
**修法**：code 侧改用单一所有者 `scripts/rust_blank.cjs` 的 `stripCommentsAndTestItems()`（**行号保留**（实测 473 → 473 行）、**字符串内容保留**（要匹配 `"HYDRA_X"` 字面量本身））；`text` 与 `lines` **都**换成屏蔽副本（否则常量/函数体等下游抽取仍走原文）。头部新增"什么才算证据"一节。
**实测**：witness 行 **340 → 343**；真树**不变**（**23 条一致 / 10 条不可比**，exit 0）⇒ **负结论**：真树那 23 条从来没有依赖注释或单测。
**测试 +4**（注释里的 helper 调用不算默认值（旧版会通过）＋ witness 指向代码行 2 而非注释行 1＋控制组"同样的调用在真代码里照读"＋只在 `#[cfg(test)]` 里的默认值不算证据）。
**证伪**：把 `const code = stripCommentsAndTestItems(text)` 改回 `const code = text` ⇒ **恰好这 3 条新用例红**，输出正是那句伪造的 `OK … knobs.rs:1`；还原后全绿（`md5 = f7033954…`）。
**★探针自身的假读数（本轮第二个事故）**：第一版探针只读 `execFileSync` 的 **stdout** 来数 `FAIL` 行——而这个套件的 FAIL 走 **stderr** ⇒ 探针打印 `red=0`（真相是 3 红）⇒ 改用 `spawnSync` 合并两个流后才有可信读数。**同一个"证据来源"的病，这次落在我的探针上。**

### C. 队列：守卫**没有**覆盖的那一半（写下来而不是假装已修）

新守卫只判**drill 自己声明了构建**的条目（真树 **5** 条）。其余 **30** 个读 `target/debug/hydra` 的 drill **没有**声明需要哪种构建（多数也确实不需要 cluster 特性），本规则**不去**把它们叫成漂移——否则就是 30 条假阳性。**残留风险**：那 30 条仍然可能跑在一个"上一次 cargo 命令留下的、特性不对/源码不新"的二进制上。**考虑过并放弃的一刀切**："凡条目跑读二进制的 drill 就必须自己建"——会把 30 条条目的运行特性集**一次性**改掉（可能改变 drill 结果），属于未经测量的大范围行为变更，故记为候选而非本轮实施。**次轮可测的中间形态**：让每条这样的条目至少 `cargo build -p hydra-server --features server --bin hydra`（非集群形态，与 `ci.yml:470` 同形）并逐条重跑比对；或让 drill 在启动前自己断言"二进制对应当前源码"（如比较 `target/debug/hydra` 与最新源文件的 mtime）。

### D. 验证

`check_gate_entries` 真树 exit 0（87 条目 / 5 前置条件）、测试 **7/7**；`check_documented_defaults` 真树 exit 0、测试全绿（+4）；`check_ci_wiring` exit 0（新守卫与新测试都被它发现并接线，CI +2 步）；`check_compose_static`/`compose grace`/`compose health` 全绿（§2dy 的改动无回归）；`check_public_claims --measure` 仍 **812**。**门禁 87 项全绿、`OVERALL=GREEN`、exit 0**（本轮改动后重跑：`grep -E "exit=[^0]"` **无输出**；上一轮 RED 的两条 `replica fidelity`/`cluster rate limits (shared+FO)` 现均 **exit=0**，本轮新修的三条 `tenant usage read over CH`/`replica fidelity`/`cluster rate limits` 与新接线两条 `gate entries build what they run`/`gate entry checker tests` 均 exit=0；两份转写 `FAILED` 计数 **0**）。
**本轮无产品代码改动**（1 个新守卫 + 1 个守卫改动 + 2 个测试文件 + 门禁脚本 3 条目 + CI + 文档）。

---

## 2ea. 第一百四十四轮：清 §2dw 队列 P3 的两条 —— `check_documented_metrics` 的**四条死 allowlist**，以及 `compose_static.test.cjs` 断言用错读取器

### A. ★四条 allowlist 条目**一条都没被用过**，而 OK 行把它们印成"deliberately absent"

**实测**（只读）：`hydra_proxy_listener_bound`、`hydra_proxy_listener_tenant_certs`、`hydra_proxy_tenant_certs`、`hydra_proxy_listener_tls` 四个名字在三份运维文档里的出现次数**全是 0**（`documented-at=[]`，`registered=false`）⇒ `ABSENT_ON_PURPOSE.has(name)` **从未被走到**，即四条都是**死条目**。而 OK 行一直印 `4 name(s) allowlisted as deliberately absent: …` —— **宣称了没有发生的工作**（本计划反复出现的同一族：输出说不准自己做了什么）。
**真正让那条注记通过的是通配符**：`ops.md:1253` 写的是 `` `hydra_proxy_listener_*` ``，抽取出的 token 是 `hydra_proxy_listener_`、以 `_` 结尾 ⇒ 被 `endsWith('_')` **跳过**；allowlist 与它无关。**危险面**：allowlist 正是"能压掉真漂移"的那个机制，而**没人需要的条目 = 没人复核过的条目**。

**修法（五处）**：
1. 记录 `absentUsed`（真正被咨询过的条目）；
2. **从未被咨询的条目 = DRIFT**（与 `check_documented_defaults` 的 `UNVERIFIED_OK` 陈旧规则同族，理由也相同）；
3. 内建表**清空**，并在常量上写明"为什么是空的、什么情况下才该加"；测试用的注入口 `CDM_ABSENT_ON_PURPOSE`（逗号分隔）保留，好让"被使用"和"陈旧"两条路径都能被测；
4. OK 行改为 `N of M allowlisted name(s) were actually needed` + `K wildcard prefix(es) skipped`，并直说"文档那条 deliberate-absence 注记是**通配符**，由尾下划线规则处理"；
5. 诊断**分开计数**：`0 documented-but-missing metric name(s), 1 dead allowlist entr(ies)` —— 之前把死条目叫做"missing metric name"，属"输出说不准自己的内容"。

**测试 +2**（原 allowlist 用例改用注入口；新增"没人需要的条目必须 DRIFT"；新增控制组"通配符注记靠 skip 规则自己通过，不靠 allowlist"），套件 **14 条断言全绿**。
**证伪 3 枚（每枚先自证已应用）**：①关掉陈旧检查（`if (absentUsed.has(name)) continue;` → `if (true) continue;`）⇒ **恰好**"没人需要的条目必须 DRIFT"那条红（`exit=0` 而非 1）；②去掉 `absentUsed.add(name)` ⇒ **恰好**"被咨询的条目不该红"那条红；③**在真文档上把死条目加回去** ⇒ 真树 **exit 1** 且点名该条目（`0 documented-but-missing metric name(s), 1 dead allowlist entr(ies)`），还原后 exit 0。还原用**验证式备份** `.acceptance/round144/check_documented_metrics.cjs.postfix2`（`md5 = ecc8a5a0…`，`grep -c "dead allowlist entr(ies)"` = 1）。
**★本轮第二个"模板字符串"自伤**：OK 行里我写了 `${cond ? \`: ${…}\` : '…'}`——**反引号套在 `${}` 里会终结外层模板** ⇒ `SyntaxError: Invalid or unexpected token`（实测），改为先算成变量再插入；注释里写明了原因。

### B. P3：`compose_static.test.cjs` 用 `scalar()` 断言"静态路径看得见"，而守卫用 `scalarOwn()`

该断言（`compose_static.test.cjs:102`）自称"no `stop_grace_period` **visible to the static path**"，但 `check_compose_grace.cjs:178` 读的是 `scalarOwn(block,'stop_grace_period')`。**实测判别力**：同一夹具里 `stop_grace_period` 只写在诱饵子映射中 ⇒ `scalar` 返回 **`"30s"`**（旧断言**通过**）、`scalarOwn` 返回 **`null`**，而 grace 守卫在该夹具上**真的报** `no \`stop_grace_period\`: docker stops with SIGTERM, waits its 10s default, then SIGKILLs …` 并 **exit 1** ⇒ **旧断言在被测机制失败时仍然通过**。
**修法**：断言改用守卫的读取器（`scalarOwn`），并在注释里写明"这两条断言必须与 `check_compose_grace`/`check_compose_health` 用**同一个**读取器"（`hasOwnKey` 那条本来就一致）。`scalar` 的"嵌套容忍"用途在既有用例（含诱饵那两条）里仍被覆盖，语义没有被删掉。
**实测**：真树 7 个 hydra 服务全部满足（grace 守卫真树 exit 0、诱饵夹具 exit 1）；`compose_static.test` **16/16**、`check_compose_grace.test` 全绿（两套合计 **35/35**）。

### C. 验证

受影响的 8 条门禁条目全部 exit 0（`documented metrics`、`documented metric tests`、`compose stop grace`、`compose grace tests`、`compose static reader tests`、`ci wiring`、`compose healthchecks`、`compose health tests`）；`grep -rn FALSIFY crates/ integration/ scripts/` = **0**；**门禁 87 项全绿、`OVERALL=GREEN`、exit 0**（`grep -E "exit=[^0]"` 无输出；`documented metrics`/`documented metric tests`/`compose static reader tests`/`compose stop grace`/`compose grace tests`/`ci wiring` 全 exit=0；两份转写 `FAILED` 计数 0）。**本轮无产品代码改动**（1 个守卫 + 1 个测试断言修正 + 2 个测试文件 + 文档）。

**队列剩余**：§2dw 的 P2（tenant-codes 的"任一站点一致"、±1500 窗口假 DRIFT、shape 5 在真实分行调用上从不生效；`check_ci_wiring` 的 block-scalar 注释吞 step、rule 4 跨命令取 `--ignored`、不建模 job 边界）与 P3 余项（`-p <crate>` 子串、`invokes()` 不认 glob、`extractRunCommands` 死代码、常量只认 `: &str`、floor 数 occurrence 而非 distinct、tenant-codes 不可比行不入 problems、三个 floor 无独立用例）＋ §2dz 记下的"30 个读二进制却不声明构建的 drill"残留。

---

## 2eb. 第一百四十五轮：清 §2dw 队列 P2 的两条 —— `check_ci_wiring` 用**空格**拼接 `run: |` 块（一个 shell 注释吞掉后面整块；`--ignored` 能跨命令"接线"）

两条 P2 是**同一个根因**的两个症状：`check_ci_wiring.cjs` 的 `extractSteps` 里 `run = body.join(" ")`（原 149 行）。块标量是一段**脚本**，`#` 只注释**它自己那一行**；空格拼接把注释行与后面所有命令行并成一行，随后 `stripComments`（按行去注释）把这一整行抹掉 ⇒ **其后的命令全部消失**。

### A. 实测（真 `ci.yml`）

19 个 `run: |` 块里 **5 个**含 `#` 行，其后最多还有 **18** 条命令（`ci.yml:286/955/1004/1013/1029`）。今天被吞掉的调用只有 `ci.yml:1004` 的 `npm install --save-dev @playwright/test@1.55.0` 与 `npx playwright install --with-deps chromium` —— 都不是本守卫要求的工件 ⇒ **今天没有造成假报**，但规则本身不成立。

### B. 两个症状（都在夹具里复现）

1. **假 UNWIRED**：工件在注释行之后被调用 ⇒ 守卫看不到 ⇒ 报 `UNWIRED scripts/check_d.cjs is never executed by ci.yml`（**实测输出**）。
2. **假通过**：规则 4（"`#[ignore]` 目标被 `--ignored` 跑到"）比较的是**整段 step 文本**，所以 `--test usage_query` 与另一条命令里的 `--ignored` 同时出现就算"已接线" ⇒ `OK (everything is executed)`（**实测输出**）。

### C. 修法

1. `run: |` 块改为 **`join("\n")`**（脚本按脚本读），注释里写下测量经过；
2. 新增 `logicalCommands(run)`：按行拆分并**合并 `\` 续行**，返回"真正的命令"列表；
3. 规则 3（crate 被选中）与规则 4（`--ignored`）改用 `cargoTestInvocations`（**逐命令**），不再用整段 step 文本。

### D. 测试 +4 与证伪 2（每枚先自证已应用）

**测试**：(a) `run: |` 里注释行**不得**隐藏其后的命令（只有 `check_d.cjs` 在该块内、且在注释之后）；(b) **同一 step 的另一条命令**里的 `--ignored` **不算**接线（必须仍报 UNWIRED）；(c) 控制组：同一条命令带 `--test usage_query … --ignored` ⇒ 通过；(d) 控制组：**`\` 续行**仍是**一条**命令（规则逐命令化后，合法的多行写法必须继续算数）。
**证伪**：①把 `join("\n")` 改回 `join(" ")` ⇒ **(a)(b) 两条同时红**，输出正是上面那两句；②只把规则改回"整段文本"匹配（`cargoTestInvocations = cargoTestCmds`）⇒ **恰好 (b) 红**；还原后 **ALL CI WIRING TESTS PASSED**（`md5 = 1c3bada8…`，备份经 `grep -c logicalCommands` / `grep -c "does not hide the commands after it"` 验证）。

### E. 验证

真树 `check_ci_wiring` **exit 0**（`OK (everything is executed)`，47 仓外工件 / 34 脚本工件 / 54 crate 测试文件均与修复前一致 ⇒ **真树结论未变**，说明这两条今天是**潜在**缺陷）；`check_ci_wiring.test` 全绿；`check_test_tails` 现扫 **65** 个 suite 文件（+1 是第一百四十三轮新增的 `check_gate_entries.test.cjs`，它用 `node:test` 所以落在"未建模"的 21 个里）、判定 44、exit 0；`check_gate_entries` exit 0；`grep -rn FALSIFY crates/ integration/ scripts/` = **0**；**门禁 87 项全绿、`OVERALL=GREEN`、`GATE_EXIT=0`**（`grep -E "exit=[^0]"` 无输出；`ci wiring`/`ci wiring tests` 两条条目本次已覆盖该修复）。**本轮无产品代码改动**（1 个守卫 + 1 个测试文件 + 文档）。

**队列剩余**：§2dw 的 P2 里 tenant-codes 三条（"任一站点一致"、±1500 窗口假 DRIFT、shape 5 在真实分行调用上从不生效）与 `check_ci_wiring` 的**第三条**（不建模 job 边界：job 级 `env:`/`defaults:` 会并进上一步）；P3 余项（`-p <crate>` 子串、`invokes()` 不认 glob、`extractRunCommands` 死代码、常量只认 `: &str`、floor 数 occurrence 而非 distinct、tenant-codes 不可比行不入 problems、三个 floor 无独立用例）＋ §2dz 记下的"30 个读二进制却不声明构建的 drill"残留。

---

## 2ec. 第一百四十六轮：清 §2dw 队列 P2 的 tenant-codes 第一条 —— "任一站点一致"就算通过（多站点漂移不可见）

**问题**：`codeStatuses()` 返回**所有**站点抽到的 status，而判据是 `statuses.includes(docStatus)` ⇒ 同一个 code 在 A 处 404、在 B 处 500，只要表里写 404 就**通过**；而且 OK 行会一边打印 `404/500 (table says 404)` 一边说 OK —— **输出与结论自相矛盾**。表格给客户端的是**一个** status 去跟随（重试逻辑照表走），两个 status 意味着"表不全"或"某处用错了 code"。

**先量再改**：真树 33 个文档码里 **0 个**是多 status（**30** 单值 + **3** 不可抽取）⇒ 今天没有错判，这是**潜在**洞（与 §2eb 同类：规则不成立但尚未咬人）。

**修法**：抽到的 status 去重；`distinct.length > 1` ⇒ **DRIFT**（措辞同时点名两个 status、各站点分歧与表值）；只有 `distinct.length === 1 && distinct[0] === docStatus` 才进 `agreed`，OK 行只列**真正一致**的行 —— 旧版会把"不一致但包含表值"的行也印成 `OK`。

**测试 +2**：(a) 两站点 404/500、表写 404 ⇒ 必须 **FAIL**（旧版通过）；(b) **控制组**：两站点都是 404 ⇒ 通过，且 OK 行形如 `OK    not_found = 404 (table says 404)`。
**证伪**：把 `if (distinct.length > 1)` 置假 ⇒ **恰好 (a) 红**，探针输出正是旧版那句自相矛盾的话 `OK    not_found = 404/500 (table says 404)`；同时真树在该探针下仍是 `OK (30 …)`（⇒ 证实"真树无此形态"）；还原后套件 **ALL TENANT-ERROR-CODE TESTS PASSED**（`md5 = 81cb8b54…`，备份经 `grep -c "DIFFERENT statuses"` 验证）。
**真树不变**：30 一致 / 3 不可抽取、exit 0（本改动落在门禁**启动之后**，故按纪律单独重跑其两条条目：`node scripts/check_tenant_error_codes.cjs` exit 0、`node --test scripts/check_tenant_error_codes.test.cjs` **ALL TENANT-ERROR-CODE TESTS PASSED**；同一轮末尾的完整门禁为 **87 项 `OVERALL=GREEN`、exit 0**）。

**队列剩余（tenant-codes 另两条 + 一条）**：①**±1500 字符窗口**（shape 6）会把**隔壁 handler 的** `respond_json(…, status, …)` 记到本 code 名下 ⇒ 假 DRIFT（本轮未动：收紧它需要按**括号配对**把 status 限制在同一个调用内，属独立一轮）；②shape 5 在**真实分行调用**上从不生效（§2dw 实测：`proxy.rs:861/887` 命中集合为空，`rate_limited` 的 429 证据全部来自另一子系统 ⇒ 把 proxy 的 429 改成 418 守卫仍绿）；③`check_ci_wiring` 不建模 **job 边界**（job 级 `env:`/`defaults:` 会并进上一步）。

---
## 2ed. 第一百四十七轮：清 §2dw 队列 P2 的 tenant-codes 第二条 —— shape 5 在**真实分行调用**上从不生效

**问题（复审指出，本轮独立复现）**：`proxy.rs:859`/`:885` 的调用是**多行**的

```
return short_circuit_rate_limited(
    session,
    "rate_limited",
    retry_after,
    &ctx.trace_id.clone(),
)
```

而 shape 5 只看**字面量所在行**与**它上面一行**是否以 `call(` 结尾 ⇒ 两处都不匹配 ⇒ **helper 找不到**。

**后果（实测）**：与真实形状一致的夹具（helper 体内 `ResponseHeader::build(418)`、表写 429）⇒ 旧版打印 `????  rate_limited: documented 429, no status extractable at its emission site`（该行**无法判定**，只要其它行满足覆盖 floor，整轮仍是 `OK`）；真树上 `rate_limited` 的 429 证据**只来自租户 API 子系统** ⇒ **把 proxy 的 429 改成 418，本守卫仍然绿** —— 复审那句话被独立证实。

**修法**：当原有两条正则都没命中时，**沿参数表向上回溯**（最多 8 行）找到"打开这个调用的那一行"，并在遇到**语句终结行**（以 `;` / `{` / `}` 结尾）时**停止** —— 已完成的上一个调用绝不能被当成本次调用的开头。

**决定性验证（对**真实文件副本**做，`TEC_SRC` 指向副本目录，`TEC_DOC` 用真文档）**：未改动的 `proxy.rs` 副本 ⇒ `OK    rate_limited = 429`、**exit 0**；把 helper 里的 `ResponseHeader::build(429` 改成 `418` ⇒ **DRIFT** `rate_limited: the table says HTTP 429 but the code emits it with 418`、**exit 1**（这就是复审说"守卫仍绿"的那一步，现在会红）。

**测试 +3**：① 多行调用被跟进（418 vs 429 ⇒ DRIFT）；② **控制组**：同一多行调用写 429 ⇒ 通过且 OK 行形如 `OK    rate_limited = 429 (table says 429)`；③ **边界用例**：多行调用**之前**有一个以 `;` 结束的**另一个** helper 调用（那一个建 **429**、本次 helper 建 **418**）⇒ 必须读到 **418**（证明回溯停在语句边界，不会误取上面那个调用）。
**证伪**：把 `if (!helperName) {` 置假 ⇒ **恰好这 3 条红**，每条都打印旧版的 `???? … no status extractable`；同一探针下真树仍是 `OK (30 documented error code(s) …)`（⇒ 证实"真树的结论**不依赖** proxy 证据"，正是复审指出的性质）；还原后套件 **ALL TENANT-ERROR-CODE TESTS PASSED**（`md5 = 40ffa3e1…`，备份经 `grep -c "The call may SPAN LINES"` 验证）。
**真树不变**：30 一致 / 3 不可抽取、exit 0。

**队列剩余**：shape 6 的 **±1500 字符窗口**（会把**隔壁 handler 的** `respond_json(…, status, …)` 记到本 code 名下 ⇒ 假 DRIFT；正解同样是"绑定到**最近的同一个**调用"，而不是任意窗口，但需要先造出能让 shape 6 命中的夹具，故单独一轮）；`check_ci_wiring` 的 **job 边界**；P3 余项（`-p <crate>` 子串、`invokes()` 不认 glob、常量只认 `: &str`、floor 数 occurrence 而非 distinct、tenant-codes 不可比行不入 problems、三个 floor 无独立用例）＋ §2dz 的"30 个读二进制却不声明构建的 drill"残留。

---
## 2ee. 第一百四十八轮：清 §2dw 队列 P2 的 tenant-codes 第三条 —— shape 6 的 **±1500 字符窗口**把**隔壁 handler** 的 status 记到本 code 名下（**假 DRIFT**）

**复现（夹具，同文件两个 handler）**：`not_found` 的字面量在 `mine` 里、它自己的调用传 **404**，而**上面**的邻居 handler 发 **418** ⇒ 旧实现（`text.slice(idx - 1500, idx + 1500)` 里取**第一个** `respond_json*`）报
`DRIFT  not_found: the table says HTTP 404 but the code emits it with 418` —— **假 DRIFT**，会让运维去"修"一个**正确**的文件。
**★一个值得记的翻转**：同一个漏窗在**第一百四十六轮之前**是**假通过**（`includes(404)` 为真 ⇒ 静默 OK，真漂移可被掩盖），第一百四十六轮把多状态变成 DRIFT 之后，它**翻成了假 DRIFT**。同一根因、两个方向，两个都会骗人。

**修法：把 status 绑定到"发送这个 body 的那个调用"**，新增 `statusOfSendingCall(text, idx)` + `callSpan(text, open)`（**字符串感知**的括号配对：消息字符串里的 `)` 不能闭合调用）：
- **不越出所在函数**（在最近的 `\nasync fn`/`\nfn` 与下一个 `\n}\n` 之间取作用域）；
- **优先"括号区间包含该字面量"的那个调用**（真实形状 A：`respond_json(session, 404, json!({ … "code": … }))`）；
- 否则取它**之后最近**的一个调用（真实形状 B：`let body = json!(…); … respond_json(session, ctx, 200, &body)`）；
- **绝不**在字面量**之前**取一个"并不包含它"的调用（这正是漏窗的病）。
- **★我第一版的第二个错**：用 `(?:\w+\s*,\s*){2}?(\d{3})` 数"前面有几个实参"来定位 status，而真实两种调用的实参个数不同（`respond_json(session, 404, …)` 与 `respond_json(session, ctx, 200, …)`）⇒ 形状 A 反而**解析不出来**（夹具打出 `????`）⇒ 改为"**第一个三位数字实参**"并在注释里写明为什么不能数实参。

**证据**：
- 漏窗夹具：现在读到**它自己的 404**（`OK    not_found = 404 (table says 404)`）；修复前是 418。
- **真树输出与修复前 `diff` 为空**（逐字节相同：30 一致 / 3 不可抽取、exit 0）⇒ 修的是"归属"，没有丢掉任何原本能解析的行。
- 形状 B 双向实测：发送处 404 ⇒ `OK 404`；发送处 **418** ⇒ **DRIFT**（证明形状 B 确实被跟进）。
- **边界夹具**：邻居在**下面**（自己的 `let body` + 404 在前）⇒ 仍读 **404**（证明不越函数）。

**测试 +4**：(a) 邻居在前 ⇒ 必须读自己的 404（旧版报 418）；(b) 形状 B 被跟进；(c) 形状 B 的漂移被抓；(d) 邻居在后 ⇒ 仍读自己的 404。
**证伪**：把字符窗口搜索**放回**去（`statusOfSendingCall` → 旧的 slice+regex）⇒ **恰好 (a) 红**，输出正是旧版那句 `DRIFT … emits it with 418`；还原后套件全绿（`md5 = 0b42904f…`，备份经 `grep -c statusOfSendingCall` 验证）。

**真树不变**：30 一致 / 3 不可抽取、exit 0（输出与基线逐字节相同）。**门禁 87 项全绿、`OVERALL=GREEN`、`GATE_EXIT=0`**（该轮门禁在 shape-6 改动**之前**启动，故按纪律单独重跑其两条条目：`check_tenant_error_codes.cjs` exit 0、`check_tenant_error_codes.test.cjs` **ALL PASSED**；同轮末尾收尾复核：`test tails`/`gate entries`/`ci wiring`/`documented metrics`/`compose grace`/`compose health` 全 exit 0、`FALSIFY` = 0）。
**队列剩余**：`check_ci_wiring` 的 **job 边界**建模；P3 余项（`-p <crate>` 子串、`invokes()` 不认 glob、`extractRunCommands` 死代码、常量只认 `: &str`、floor 数 occurrence 而非 distinct、tenant-codes 不可比行不入 problems、三个 floor 无独立用例）＋ §2dz 的"30 个读二进制却不声明构建的 drill"残留。

---
## 2ef. 第一百四十九轮：清 §2dw 队列 P2 的最后一条 —— `check_ci_wiring` **不建模 job 边界**（错的不只是漏检：它会**冤枉**上一步、又**放过**一个永不运行的 job）

**旧解析器**（`extractSteps`）在**任意** `- ` 列表项处切块，并把所有非列表行追加到"正在读的那一块" ⇒ 两个方向都错，且**互相独立**。用**原版函数**对同一夹具实测（脚本 `.acceptance/round149/old-parser.cjs`）：

```
OLD run="bash scripts/d.test.sh"   cond="false"   ← job b 的 job 级 `if: false` 被塞进了 job a 的最后一步
OLD run="node scripts/check_d.cjs" cond=null      ← 永不运行的 job b 的步骤被当成"会执行"
```

即：**① 冤枉**——一个**正常**的步骤被判为"被 `if: false` 禁用"（假 UNWIRED）；**② 放过**——一个**永不运行**的 job 里点名的工件被算作"已接线"（**假通过**，比漏检更糟：整个 job 被注释掉也算跑过）。第一个 job 的 job 级键更惨：它前面没有块可追加 ⇒ **完全被丢弃**（同一夹具里 job a 的 `if: false` 从未被读到）。

**修法（job 感知的扫描器）**：`extractSteps` 改为按 **YAML 结构**读——`jobs:` 进入 job 区；2 空格键 = 新 job（**关掉前一步的块**）；4 空格键 = job 级键（**同样是 job 边界**，并识别 job 级 `if:` 与 `defaults:`）；`defaults.run.working-directory` 记为 job 默认目录，**只对没有自己 `working-directory:` 的步骤生效**；步骤仍是 6 空格处的列表项。规则侧：`steps` 现在同时排除 **step 级** `if: false` 与 **job 级** `if: false`（新增一条具名 problem，一次点名整个 job 与其步骤数）。
**★我自己在这条路上先写错了一次，而且守卫照样打印 OK**：job 级分支里我把捕获到的键与 `"if:"` 比较，而正则捕获的是不带冒号的 `"if"` ⇒ `job.disabled` **永远不被赋值**、整段新代码是死代码，而真树与夹具都打印 `OK (everything is executed)`。**抓到它的不是退出码，而是把解析结果 dump 出来看**（`jobDisabled: false`）——这正是本计划反复强调的"绿不等于证据"。

**测试 +5**（fixture 用 `scripts/check_d.cjs` 作为"只被那个 job 点名"的必需工件）：① job 级 `if: false` 的**第一个** job ⇒ 必须报该 job 被禁用**且**该工件 never executed；② 同样情形放在**第二个** job ⇒ 必须点名 job `b` **且不得**冤枉 `bash scripts/d.test.sh`（旧版正是冤枉它）；③ 控制组：同样的形状但 job 启用 ⇒ 通过；④ job 级 `defaults.run.working-directory: tools/sdk` + `go test ./...` ⇒ 目录覆盖规则必须生效（`tools/sdk/client_test.go` 只靠这个 job 默认目录被覆盖）。
**证伪 3 枚（每枚先自证已应用）**：①把 `"if"` 改回 `"if:"`（即我踩过的那个 bug）⇒ **3 条红**，输出正是旧版的 `OK (everything is executed)`；②把 `job.defaultDir` 应用置假 ⇒ **恰好**默认目录那条红（`tools/sdk/client_test.go is never executed`）；③**忠实复原旧解析器**（job 名不关块 + job 级键追加进当前块）⇒ **2 条红**，同样复现假通过；老解析器的"冤枉上一步"由上面那段**原版函数直接 dump** 提供证据（`cond="false"` 落在 `d.test.sh` 上）。还原用**验证式备份** `.acceptance/round149/check_ci_wiring.cjs.postfix`（`md5 = 13ae28a5…`）。

**真树不变**：`check_ci_wiring` exit 0（`OK (everything is executed)`；47 仓外工件 / 34 脚本工件 / 54 crate 测试文件）——真 `ci.yml` 的每个 job 都只有 `runs-on:` + `steps:`，**没有** job 级 `if:`/`defaults:` ⇒ 这两条是**潜在**缺陷（复审的判断正确）。顺带修掉一句被上一轮改动弄过期的注释（"space-joined command" → 现在的**按行**拼接）。
**验证**：`check_ci_wiring` 真树 exit 0、测试全绿；**门禁 87 项全绿、`OVERALL=GREEN`、`exit 0`**（`grep -E "exit=[^0]"` 无输出；`ci wiring`/`ci wiring tests` 两条条目覆盖本轮修复）。**注意**：删死代码那一步落在门禁**启动之后**，故按纪律**单独重跑**其两条条目（`check_ci_wiring.cjs` exit 0、`check_ci_wiring.test.cjs` ALL PASSED）；末尾收尾复核 `tenant error codes`×2、`test tails`、`gate entries`、`documented metrics`、`compose grace`、`compose health`、`source purity`、`documented env` 全 exit 0、`FALSIFY` = 0。**本轮无产品代码改动**（1 个守卫 + 1 个测试文件 + 文档）。

**§2dw 复审队列至此清零**：P1-3（§2dx）、P2 全部（§2dx/§2dy/§2eb/§2ec/§2ed/§2ee/§2ef）、P3 里 `check_documented_defaults` 读原文、metrics 四条死 allowlist、`compose_static.test.cjs` 的读取器错配（§2dz/§2ea）均已闭环；P3 余项见下。

**顺带清掉 P3 的 `extractRunCommands` 死代码**：实测全仓只有**定义**（原 78 行）与一句注释提到它、**没有任何调用点**（所有规则都走 `extractSteps`）—— 它是**同一段块标量解析的第二份副本**（当时还与 `extractSteps` 的空格拼接**不一致**），留下的价值只有"以后有人用它、于是两套解析各自漂移"。已删除，并在原处写下为什么删（`grep -c extractRunCommands` 现为 **1**，仅剩那句注释）；`check_ci_wiring` 真树仍 exit 0、测试全绿。

**队列剩余（P3 全部，均为"守卫自身的精度"而非产品缺陷）**：`-p <crate>` 子串覆盖（`c.includes('-p hydra-core')` 也会被 `-p hydra-core-extra` 满足）、`invokes()` 不认 glob、常量解析只认 `: &str`、floor 数 occurrence 而非 distinct name、tenant-codes 不可比行不入 `problems`、三个 floor 无独立用例；另有 §2dz 的"30 个读二进制却不声明构建的 drill"残留。

---
## 2eg. 第一百五十轮：清 P3 两条 —— tenant-codes 的"读不出来就算了"（`????` 不入 problem 列表）与 `-p <crate>` 的**子串**判据

### A. tenant-codes：三条 `????` 行此前**既不算通过也不算失败**

**问题**：`invalid_since`/`invalid_until`/`window_too_large` 三行在真树上打 `????  … no status extractable at its emission site`，而守卫仍然 **exit 0**（OK 行只说"3 unverifiable by extraction"）。后果：**新增一个"读不出形状"的错误码**、或**把这三行的文档状态改掉**，都不会有任何反应 —— 而这三行恰是"状态写在**另一个文件**里"的那类。
**为什么读不出来（实测）**：`code` 名字面量在 `crates/hydra-server/src/tenant_api/time_bound.rs:51-53` 的 `BoundError::code()` 里，而 400 在**调用点** `crates/hydra-server/src/tenant_api/handlers.rs:467`（`respond_error(session, ctx, 400, e.code(), …)`）。跨文件跟随 `.code()` **正是**本守卫头部记着的那条错路（当年让 `rate_limited` 取到**另一个枚举**的 400）⇒ **不猜**，改为**记录决策**。
**修法**（与 `check_documented_defaults` 的 `UNVERIFIED_OK` 同构）：①不可比的行**必须**在 `UNVERIFIED_OK` 里记着（否则 **DRIFT**：要么教会抽取器，要么写下为什么读不出来）；②**记录本身会过期** —— 当某行其实**能被抽出**时，该记录变成 DRIFT（"delete the entry so the row is compared"）；③OK 行改为 `3 unverifiable by extraction and RECORDED in UNVERIFIED_OK`。
**★作用域必须写对（我第一版就写错）**：把"过期"判据套在**被测文档**上 ⇒ **十个夹具**因为"文档里没有那三个码"而红（夹具的一行表格不是"记录过期"的证据）⇒ 改为**只对随包文档（`dev-docs/tenant-api-integration.md`）判定**，并保留可测性：真文档 + 一棵**能抽出该行**的源码树 ⇒ 过期判据照样触发（实测输出 `DRIFT the UNVERIFIED_OK entry for \`invalid_since\` is no longer needed …`）。
**测试 +2**：①未记录的不可比行 ⇒ 必须 FAIL（旧版打 `????` 且 exit 0）；②**过期记录**（随包文档 + 夹具源码）⇒ 必须 FAIL。
**证伪**：①把"必须记录"的判据置真（不再区分）⇒ **恰好**未记录那条红并复现 `????  brand_new_code … `；②把过期判据置假 ⇒ **恰好**过期那条红（输出 `OK    invalid_since = 400 (table says 400)`）。还原后套件全绿（`md5 = 75dce772…`）。

### B. `check_ci_wiring`：`-p <crate>` 用**子串**判"这个 crate 被选中"

`cargoTestInvocations.some((c) => c.includes(`-p ${t.crate}`) || c.includes("--workspace"))` ⇒ `-p hydra-server-extra` **满足** `hydra-server`（`--features hydra-core-x` 亦然）⇒ 一次"把包名写错/换了包名"的 CI 修改仍然读作"这个 crate 被测试了"。**修法**：新增 `selectsCrate(command, crate)` —— 整词匹配 `-p` / `--package`（含 `-p=<crate>`），`(?![A-Za-z0-9_-])` 拒绝更长名字；`--workspace` 仍覆盖全部。
**测试 +3**：只选 `-p hydra-server-extra` 的 CI ⇒ 必须报该 crate 未被选中；控制组 `--package hydra-server` 通过；控制组 `--workspace` 通过。
**★夹具第二次自伤**：第一版只替换了 `-p hydra-core` 那一行，而夹具里**另一条** `cargo test -p hydra-server --test boot_listeners -- --ignored` 仍然正确选中了 `hydra-server` ⇒ 用例"通过"得毫无意义（实测 `OK (everything is executed)`，我把两条 cargo 行都改掉后才真正变红）。
**证伪**：把子串判据放回去 ⇒ **恰好**该条红并复现 `OK (everything is executed)`。还原后套件全绿（`md5 = e82a6f10…`）。

**验证**：两守卫真树 exit 0（`30 … / 3 … RECORDED`、`OK (everything is executed)`）；`check_test_tails`/`check_gate_entries` exit 0；`FALSIFY` = 0；**门禁 87 项全绿、`OVERALL=GREEN`、`exit 0`**（`grep -E "exit=[^0]"` 无输出；`tenant error codes`/`tenant error code tests`/`ci wiring`/`ci wiring tests` 全 exit=0）。tenant-codes 的头部注释在门禁启动后改过一次（纯注释），故按纪律**单独重跑**其两条条目：`check_tenant_error_codes.cjs` exit 0、`check_tenant_error_codes.test.cjs` ALL PASSED；末尾收尾复核 12 条守卫/套件全 exit 0。**本轮无产品代码改动**（2 个守卫 + 2 个测试文件 + 文档）。

**队列剩余（P3，守卫自身精度）**：`invokes()` 不认 glob、常量解析只认 `: &str`、floor 数 occurrence 而非 distinct name、三个 floor 无独立用例；以及 §2dz 的"30 个读二进制却不声明构建的 drill"残留。

---
## 2eh. 第一百五十一轮：清 P3 两条 —— 覆盖 floor 数的是**出现次数**（一个指标重复十次就够）与 `invokes()` **不认 glob**（一次合理的 CI 合并会造出三十条假 UNWIRED）

### A. `check_alert_expressions`：floor 数 occurrence，不数 distinct

**问题**：`refs++` / `labelsChecked++` 都是**出现次数**，于是 `MIN_REFS=10` 可以被**同一个指标重复十次**满足 —— 一节被改成围绕单条序列的文档仍然打印 OK，而"每个 label key 都必须存在"这条规则实际上什么都没检查。
**先量再改**（真 §9.1）：**出现次数** refs=**17** / labels=**6**；**distinct** 指标=**14** / `metric{label}` 对=**4**。
**修法**：新增 `distinctMetrics` / `distinctLabels` 两个集合，floor 改用 **distinct** 计数；OK 行同时打印两者（`17 metric reference(s) over 14 distinct metric(s) and 6 label selector(s) over 4 distinct pair(s)`）。
**★顺带修掉一个"零余量 floor"**：`MIN_LABELS` 原为 **4**，而 distinct 实测正是 **4** ⇒ 一旦把两条序列合并到同一个 label key（完全合法），一份**正确**的文档就会变红。按第一百三十三轮的教训（`MIN_E2E_SPECS` 曾等于实测值）降为 **3**，并在常量上写明实测值与理由。
**测试 +2**：①**同一个**指标重复 10 次的表达式 ⇒ `MIN_REFS=10` 必须 **FAIL**（输出 `only 1 DISTINCT metric(s)`；旧版通过）；②**控制组**：十个**不同**指标 ⇒ 同一个 floor 通过（证明 floor 不是"对所有人都更严"）。
**证伪**：把判据换回 `refs < MIN_REFS` ⇒ **恰好**那条红，输出正是旧版的 `OK (10 metric reference(s) over 1 distinct metric(s) …)`。还原后套件全绿（`md5 = c0a71340…`）。

### B. `check_ci_wiring`：`invokes()` 不认 glob ⇒ 一次合理的 CI 合并会造出三十条假 UNWIRED

**问题**：`invokes()` 只认"runner + 具体路径"，于是把三十个 per-file 步骤合并成一条 `node --test scripts/*.test.cjs`（很自然的 CI 清理）会让守卫报三十条 `scripts/… is never executed` —— 而它们**确实**在被执行。实测：真 `ci.yml` 今天逐条点名每个文件（所以这是**潜在**缺陷，与 §2ef 同理）。
**修法**：`invokes()` 在直接匹配失败后，再扫描"runner 之后的**含 `*`/`?` 的参数**"，用 `globMatchesBasename()` 把它翻成正则（`*` = `[^/]*`、`?` = `[^/]`，其余转义）与文件名比对；**runner 前置要求保留**，所以 `grep -l TODO scripts/*.test.cjs`（读文件）**仍然不算**执行。
**测试 +3**：①`node --test scripts/*.test.cjs` ⇒ 匹配到的文件算被执行（旧版报 UNWIRED）；②**控制组**：同一个 glob 交给 `grep -l` ⇒ 仍不算；③**控制组**：无关的 glob（`node --test tools/*.spec.mjs`）⇒ 不算。
**证伪 2 枚**：①把 glob 分支置空 ⇒ **恰好**第①条红并复现 `UNWIRED scripts/a.test.cjs is never executed by ci.yml`；②把匹配函数改成恒真（`if (true) return true`）⇒ **恰好**第③条控制组红（`OK (everything is executed)`）—— 证明这个匹配器**承重**而不是橡皮图章。还原后套件全绿（`md5 = 0afc0439…`）。

**验证**：两守卫真树 exit 0（`17 … over 14 …`、`OK (everything is executed)`）、`check_test_tails`/`check_gate_entries` exit 0、`FALSIFY` = 0；**门禁 87 项全绿、`OVERALL=GREEN`、`exit 0`**（`grep -E "exit=[^0]"` 无输出；`alert expressions`/`alert expression tests`/`ci wiring`/`ci wiring tests` 全 exit=0，本轮两处修复**都在门禁启动之前**完成 ⇒ 覆盖完整）；末尾收尾复核 14 条守卫/套件全 exit 0。**本轮无产品代码改动**（2 个守卫 + 2 个测试文件 + 文档）。

**队列剩余（P3，守卫自身精度）**：常量解析只认 `: &str`、三个 floor 无独立用例；以及 §2dz 的"30 个读二进制却不声明构建的 drill"残留。

---
## 2ei. 第一百五十二轮：清 P3 的"常量解析只认 `: &str`" —— 会把**已注册**的指标报成"未注册"（假 DRIFT），外加一处**写死的断言计数**

### A. `check_documented_metrics` 的常量值解析只认 `: &str`

**问题**：常量版注册（`register_int_counter!(MISMATCH_METRIC, …)`）靠 `constValue()` 反查字面量，而它的正则是 `(?:pub )?const NAME\s*:\s*&str\s*=\s*"…"` ⇒ 声明写成 **`&'static str`**、或写成 **`pub(crate) static`**，这个指标就不在 `names` 里，于是**文档里那条被注册的指标**会被报成 `is not registered anywhere in crates/` —— **假 DRIFT**（运维会去"修"一行完全正确的文档）。实测真树：目前所有常量都是 `: &str`（`admin/metrics.rs:238` 等）⇒ **潜在**缺陷。
**修法**：类型注解不再参与匹配（`(?:pub(?:\([^)]*\))?\s+)?(?:const|static)\s+NAME\s*:[^=]+=\s*"([a-z][a-z0-9_]*)"`），值仍必须**长得像指标名**（`[a-z][a-z0-9_]*`）。
**测试 +3**：`&'static str` 常量、`pub(crate) static` ⇒ 都要能解析；**控制组**：常量值不是指标名（`"not_a_metric_name"`）⇒ 仍必须报未注册。
**★我的第一版夹具"对得没道理"（本轮第二次自伤）**：夹具里**只有**那一个注册点 ⇒ 旧正则下 `names.size === 0`，守卫走的是 **`CANNOT VERIFY: no registered metric found (the scan is broken)`（exit 2）**，而不是这条用例要证明的"文档名未注册 DRIFT"。加一个**普通的**字面量注册点后，证伪才打出正确的失败模式：`DRIFT … \`hydra_sni_host_mismatch_total\` is not registered anywhere in crates/`（**2 条红**，正是这条用例声称的"假 DRIFT"）。
**★B（顺带发现）**：该套件的收尾行**写死** `PASSED (14 assertions …)` —— 本轮加了 3 条断言后它**仍然说 14** ⇒ 摘要报告了一个从未被计数的数字。改为**计算**：新增 `checks` 计数器并在收尾行插值，现在正确打印 **19**。

**验证**：真树 exit 0；套件 19/19 全绿；`check_test_tails`/`check_gate_entries` exit 0；`FALSIFY` = 0；**门禁 87 项全绿、`OVERALL=GREEN`、`exit 0`**（`grep -E "exit=[^0]"` 无输出；`documented metrics`/`documented metric tests` 均 exit=0，本轮修复在门禁启动之前完成 ⇒ 覆盖完整）；末尾收尾复核 12 条守卫/套件全 exit 0。**本轮无产品代码改动**（1 个守卫 + 1 个测试文件 + 文档）。

**队列剩余（P3 最后一条）**：三个 floor 没有独立用例（第一百三十三轮立下的纪律："每个 floor 都要有自己的用例"）；以及 §2dz 的"30 个读二进制却不声明构建的 drill"残留。

---
## 2ej. 第一百五十三轮：清 P3 最后一条 —— 四个 floor **没有独立用例**（一个从没被看见触发过的 floor，等于没有 floor）

**先量再改**：把每个守卫的 floor 与它自己的套件逐一对齐，结论是**四个** floor 只有"被 harness 归零"这一种出现方式、**没有任何用例让它们真正触发**：
`CI_WIRING_MIN_CRATE_TESTS`、`CI_WIRING_MIN_E2E_SPECS`、`CI_WIRING_MIN_TOOLS`（第一百三十三轮给六条 floor 补环境变量覆盖时，只做了"可覆盖"而没做"有专门用例"）与 `CAE_MIN_LABELS`（第一百二十二轮只在**手跑探针**里验过 `CAE_MIN_LABELS=7`，自动化用例里一直是归零的）。
**为什么这很危险（本仓已有先例）**：`MIN_E2E_SPECS` 曾经**恰好等于**真树计数 ⇒ 一旦合并两个 spec 就会红，而当时**没有用例**会告诉你这件事（第一百三十三轮）。**一个从没被看见触发过的 floor，与没有 floor 只差在心理安慰。**
**修法（测试侧，4 条新用例 + 1 个 harness 改动）**：
- `check_ci_wiring.test.cjs` +3：把三个 floor 分别设成夹具达不到的值（`500`/`500`/`500`）⇒ 必须各自报出**它自己那句**文案（`crate test file(s) found (< 500)` / `no e2e specs found under` / ``test artifact(s) under `tools/` (< 500)``）；
- `check_alert_expressions.test.cjs` +2：harness 的 `run()` 新增 `labelMin` 参数（原来把 `CAE_MIN_LABELS` **写死**成 `"0"` ⇒ 标签 floor 在自动化里永远关闭）⇒ ①夹具带 1 个 label selector、floor=2 ⇒ 必须报 `DISTINCT label selector(s)`；②**控制组**：同一个夹具 floor=1 ⇒ 通过（证明失败只因 floor 而变）。
**证伪 4 枚（每枚先自证已应用）**：分别把四个 floor 的判据置假 ⇒ ①crate-test floor ⇒ **恰好**那条红；②e2e floor ⇒ **恰好**那条红；③类别 floor ⇒ **2 条红**（新用例 + 既有的"整个类别消失"用例，两者都走同一个循环 ⇒ 说明新用例确实落在这条机制上）；④标签 floor ⇒ **恰好**那条红（输出 `status=0 note value protocol="tls"`，即关掉判据后守卫直接通过）。还原用**验证式备份** `.acceptance/round153/*.postfix`（`md5 = 0afc0439…/60fbb1ed…`），还原后两套全绿。

**验证**：两守卫真树 exit 0；两套件全绿；`check_test_tails`/`check_gate_entries` exit 0；`FALSIFY` = 0；**门禁 87 项全绿、`OVERALL=GREEN`、`exit 0`**（`grep -E "exit=[^0]"` 无输出；`ci wiring`×2、`alert expressions`×2 均 exit=0，本轮改动在门禁启动之前完成 ⇒ 覆盖完整）。**本轮无产品代码改动**（2 个测试文件 + 文档）。

**§2dw 复审队列（P1/P2/P3）至此全部闭环**，含本轮这最后一条。**残余队列只剩两件都不是"复审发现的守卫缺陷"**：§2dz 记下的"30 个读二进制却不声明构建的 drill"（已有一刀切方案与更小的中间形态，留待测量后实施），以及 §2dw 里 tenant-codes 的那条**已记录决策**（三行状态在另一文件，跨文件跟随 `.code()` 是已知错路 ⇒ 保留 `UNVERIFIED_OK`）。

---
## 2ek. 第一百五十四轮：清 §2dz 那件残余 —— 38 条条目跑"会启动 `target/debug/hydra` 的 drill"，而其中 **9 条跑在门禁第一次构建之前**（继承上一次 cargo 命令留下的二进制）

### A. 先量再改（这次量出来的比记录的更严重）

§2dz 记的是"30 个读二进制却不声明构建的 drill"。逐条对齐后实测：门禁 **87** 条条目里 **38** 条跑"会启动 `target/debug/hydra` 的 drill"（`HYDRA_BIN` 默认值 / 源码里写死 `target/debug/hydra`）；其中只有 **9** 条**自己**建二进制（就是那几个声明了前置条件的 cluster drill），而 **9 条跑在门禁第一次构建（原第 140 行）之前**：`trusted-proxy keying`、`tenant API limits`、`tenant API contract`、`tenant API boundaries`、`streaming path (SSE)` 等。它们拿到的是**上一条 cargo 命令刚好留下的那个二进制** —— 第一百四十三轮的事故（feature-poor）就是这么发生的，而在**全新 checkout** 上 `target/debug/hydra` 根本不存在（错误信息会指向别处）。

### B. CI 早就有这一条，本地门禁没有

`ci.yml` 的 `integration` 作业**第一条**就是
`cargo build -p hydra-server --features server --bin hydra`，**然后**才跑 `test_trusted_proxies.py`/`test_tenant_api_limits.py` 等（同一组 drill）。也就是说：**CI 有这道前置构建，本地门禁缺了它** —— 差距不是"CI 更严"，而是"本地门禁依赖一个不该依赖的状态"。

### C. 修法两步（机制 + 机制守卫）

1. **门禁补上同一条构建**：新增条目 `build the binary under test` = `cargo build -p hydra-server --features server --bin hydra`，放在 `cargo test` 之后、第一条 drill 之前（与 CI 同形、同 feature 集）⇒ **87 → 88 项**。
2. **把顺序依赖变成机械规则**（`check_gate_entries.cjs` 新增第 3 条规则）：凡条目跑"**会启动预建二进制**"的 drill（检测 drill 源码里的 `target", "debug", "hydra` 或 `target/debug/hydra`），该条目**必须**自己建它，**或者**其**前面**已有条目建过 —— 否则 DRIFT。同时加第四条 floor `MIN_BINARY_DEPS`（真树 **38**，floor **20**，低于实测留余量），并修正 OK 行措辞（38 条里只有 9 条自建 ⇒ 说"每条都自己建"是**把话说大了**，改为"own entry or preceded by one that does"）。

### D. 证据与证伪（4 枚，每枚先自证已应用）

- **真门禁上的证伪（最有说服力）**：把新加的那条构建改成 `true` ⇒ 守卫 **exit 1** 并点名 `DRIFT entry "trusted-proxy keying" (line 123) runs integration/test_trusted_proxies.py, which starts \`target/debug/hydra\`, but NO entry before it builds that binary` —— 也就是说这条规则**正好会抓住本轮修复前真实存在的状态**（不是假想）。
- 新规则置假 ⇒ 套件里**恰好**"no earlier build ⇒ drift"那条红；floor 置假 ⇒ **恰好** floor 那条红。
- 测试 **+5**（无前置构建 ⇒ drift；+ 三个控制组：前置构建够用、自己建够用、既不建也不启动的 drill 不被判；+ floor 自有用例）；原有两个旧用例的期望串同步改成新的 OK 行措辞（`N drill(s) DOCUMENT a build`）—— 它们断言的是**旧措辞**，改完才反映真实输出（本轮**第三次**遇到"断言写的是旧文案"）。
- 还原用**验证式备份** `.acceptance/round154/*.postfix`（`grep -c readsPrebuiltBinary` = 2、`grep -c "build the binary under test"` = 1），还原后套件 12/12、真树守卫 exit 0。

**验证**：`check_gate_entries` 真树 exit 0（`88 entries; 5 drill(s) DOCUMENT a build …; 38 entry(ies) … each either building it in its own entry or preceded by one that does`）、测试 12/12；`check_ci_wiring` exit 0；`check_test_tails` exit 0；`FALSIFY` = 0；**门禁 88 项**重跑（判定见下）。**本轮无产品代码改动**（1 个守卫 + 1 个测试文件 + 门禁脚本 + 文档）。

**队列状态**：§2dw 复审队列已闭环，§2dz 的残余（本轮）已闭环 ⇒ **残余清零**；只剩两条**需要你决策**的事（16 项决策表，最紧 D-8/D-15/D-16/D-17/D-14/D-13），以及本轮派出的**两个只读 Oracle 复审**（守卫改动 / 验证纪律）正在运行，结果在下一轮处置。

---
## 2el. 第一百五十五轮：处置两个只读 Oracle 复审的 **P1 类**（三条"守卫骗人" + 一条"OK 行说大话"）与**文档/数字失效**

两个只读子代理（第一百五十三轮派出）交回 **A：守卫改动 14 条** / **B：验证纪律 15 条**。本轮**逐条自行复现**后先修 P1 与"文档数字"这一类；P2/P3 记入下一轮队列。**复审期间仓库在被我并发编辑**（B 明确记录了这一事实并区分"当时/此刻"两套数字，处理得当）。

### A. ★脚本链把**注释**当执行证据（`check_ci_wiring`）——"还没接线"这句 TODO 成了"已接线"的证明

**复现（本轮独立）**：runner 脚本 `integration/run.sh` 里只写 `# TODO: integration/test_new.py is still unwired`、**不调用它** ⇒ 旧版 **`OK (everything is executed)`**；**只删掉那行注释**、别的不动 ⇒ `UNWIRED integration/test_new.py is never executed by ci.yml`。这与本文件自己的教义（"A comment is a statement of intent, never evidence of execution"，只应用于 step 的 `run:` 文本）直接冲突。
**修法（1 行）**：`wiredSet()` 里读脚本内容时改用 `stripComments(...)`。**测试 +1**（runner 脚本只在注释里提到工件 ⇒ 必须 UNWIRED）；**证伪**：改回原文读取 ⇒ **恰好**那条红。
**真树**：仍 `OK`（唯一依赖脚本文本链的 `integration/check_error_contract.py`/`test_crud.py` 在去注释后提及仍在 ⇒ 复审的"今天潜伏"结论成立）。

### B. ★文件里**任意**一个 `fn status(&self) -> u16` 会把自己的值塞进这个 code（`check_tenant_error_codes`）⇒ **假 DRIFT**

**复现（本轮独立）**：夹具只有一个发射点 `respond_error(session, ctx, 400, e.code(), …)`，同文件另有一个**无关** `impl QuotaError { fn status(&self) -> u16 { 500 } }` ⇒ 旧版报 `DRIFT quota_exceeded: emitted with DIFFERENT statuses (500/400)`；**删掉那行无关的 `fn status`、其余完全相同** ⇒ `OK quota_exceeded = 400`。即它读的不是"该 code 的发射状态"，而是"文件里任意一个 status 函数里的所有三位数"（`:176-183` 的注释自称已 scoped，实现既不看 `impl` 归属也不看类型）。
**修法**：删除那四行兜底（映射站点的状态已由紧随其后的 `.code()` 调用点提取，读不出来的三行由 `UNVERIFIED_OK` 记录）；**真树输出逐字节相同**（30 一致 / 3 记录）⇒ 删掉的是纯误导路径。
**测试 +1**（无关 `fn status` 不得注入）；**证伪**：把它加回去 ⇒ **恰好**那条红并复现 `DIFFERENT statuses (500/400)`。

### C. ★静态路径把"角色读不出来"默认成 `all`（`check_compose_health`）⇒ 对**正确的 edge** 报 FAIL

**复现（本轮独立）**：同一个 service，`environment:` 写成 **LIST 形态**（`- HYDRA_ROLE=edge`）时：**渲染路径** `OK … hydra-edge (role=edge)`、**静态路径** `hydra-edge (role=all): a all-role node runs the admin API — probe /api/v1/health …` ⇒ 让运维去给一个**没有管理面**的 edge 加 admin 探针。`HYDRA_ROLE: ${NODE_ROLE:-edge}` 同样复现。这正是该守卫头部记录的那个事故，而 `compose_static.cjs` 的"读不出就 REFUSE"纪律此前只覆盖 merge key。
**修法**：`unjudgeableServices()` 新增两类具名拒绝 —— ①`environment:` 为 **LIST** 形态且 `HYDRA_ROLE` 读不出；②角色值含 `${`（渲染期才定）。两者都让静态路径 **CANNOT VERIFY(exit 2)** 而不是 FAIL，并提示"用 `docker compose config` 渲染后再判"。
**测试 +1**（两种形态都必须 exit 2 且**不得**出现 `role=all`）；**证伪**：关掉 LIST 判据 ⇒ **恰好**那条红。**真树**：health/grace 仍 exit 0（三个 shipped 文件都用 mapping 形态 ⇒ 无新拒绝）。

### D. ★`check_test_tails` 的 IIFE 扫描**提前 return**，而 OK 行把"没判"说成"形状不建模"

**复现（本轮独立，且**先证伪了复审给的夹具**）**：复审的夹具用的是**单行** IIFE，其输出其实由"单行形态不被识别"解释；换成**真实的多行形态**后机制才显现 —— 文件里两个列零 IIFE（第一个不 exit、第二个 exit），其后追加的用例**完全不被检查**（`OK … 1 end in another shape that this rule does not model`），而**只留那个会 exit 的 IIFE** 的同一文件立刻报 `DEAD … after the IIFE that exits at line 4`。即：**形状是建模了的，只是扫描在第一个 IIFE 就停了**——与本计划 §2dx 亲手删掉的那半句"想当然"是同一类。
**修法**：`jsIifeEnd` 不 exit 时 `break` 到**下一个**列零 IIFE（不再 `return null`）；OK 行改为只陈述事实：`N were NOT judged by this rule — appended code in those files is not checked here`（不再声称它们属于"未建模的形状"）。
**测试 +1**（两 IIFE 夹具必须报 `DEAD …:9`）；**证伪**：改回 `return … : null` ⇒ **恰好**那条红。

### E. 文档与注释里的**失效数字**（复审 B 的 P1/P2/P3-5，逐条实测）

- `plans/2026-09-29-oracle-remediation.md:5410`（§4，**复现门禁的唯一入口**）写着"它现在有 **13 条门禁**"，而实际 **88** 条（6.8×）⇒ 改为"**88 条条目** —— 数字随文件增长，请用 `grep -c '^gate ' …` 取当前值"，并保留历史加法作为出处说明。
- `check_test_tails.cjs` 的注释仍写 `44 of 64`/`7 of 64`/`6 of 64`（真树 **65**，第 65 个是第一百四十三轮新增的 `check_gate_entries.test.cjs`）⇒ 全部改为 65。
- `check_ci_wiring.cjs`：`MIN_SCRIPTS` 的注释写"30 real artifacts"（真树 **34**）、walk floor 注释写"visits ~25"（真树 **47**）⇒ 改为实测值。
- `check_gate_entries.cjs`：`38 of 87` → **88**，并把"9 more ran BEFORE the gate's first build"改为**过去时**（第一百五十四轮加了前置构建后实测 `beforeFirstBuild = 0`，而门禁自己的注释也同步改成过去时）。

### F. 本轮方法学收获（复审 B 提出，值得记）

**探针"文本已落入文件"不等于"该分支被关掉"**：复审把 `if (A || B)` 改成 `if (false && A || B)` —— 文本确实变了、`grep` 可见，但优先级让条件等价于 `B`，**机制仍生效**、套件全绿。推荐形态是 `if (false) /*probe*/ if (原条件) {`（语义假且可 grep），或探针后打印判定值。这与本计划已记录的两次事故同源，只是更隐蔽。

**验证**：本轮涉及的 13 条守卫/套件全部 exit 0；`FALSIFY` = 0；**门禁 88 项全绿、`OVERALL=GREEN`、`exit 0`**（`grep -E "exit=[^0]"` 无输出；本轮全部改动都在门禁启动之前完成 ⇒ 覆盖完整）。**本轮无产品代码改动**（4 个守卫 + 1 个共享模块 + 4 个测试文件 + 文档/注释）。

**队列（下一轮，来自两份复审的 P2/P3，未在本轮修）**：`check_ci_wiring` 的 `--test <target>` **子串**判据（可被更长的 target 名永久满足）、`run: |` 块体内一行 `- ` 会截断/丢弃整个 step（假红 + 静默假绿）、`stepShapeProblems` 与 `extractSteps` **两份解析器**、死变量 `checked`（注释宣称它是防静默通过的机制）；`check_tenant_error_codes` 的源码路径不可读时**崩溃 exit 1**（应为 2）、`TEC_MIN_COMPARED=0` 可整体关掉覆盖下限、空表 `OK (0 …)`；`check_documented_metrics` 把**出现次数**当"name(s)"报（69 vs **36 distinct**）；`check_compose_grace` 对 `${VAR}` 的 `stop_grace_period` 报 `unparseable`（与 C 同根因）；以及"N/N 断言"计数漂移与两条"名字对、机制错"的测试用例。

---
## 2em. 第一百五十六轮：处置复审 P2/P3 的四条（含**我自己的测试死代码**）——`${VAR}` 的 grace、`--test` 子串、metrics 把出现次数当"名字"、死变量 `checked`

### A. `check_compose_grace`：合法的 `${VAR}` 被报 `unparseable`（与第一百五十五轮 C 同根因）

**复现**：`stop_grace_period: ${HYDRA_STOP_GRACE:-30s}` ⇒ 旧版 `unparseable … stop_grace_period: ${HYDRA_STOP_GRACE:-30s}` 且 **exit 1**（= 漂移判定），而这是合法 compose。
**修法**：`unjudgeableServices()` 再增一条 —— **服务自身层**（own-level）任何键的值含 `${` ⇒ 具名拒绝（渲染期才定，文本读法管不了）⇒ 静态路径 **CANNOT VERIFY(2)**。
**先确认不会误伤真文件**：三个 shipped 文件只在**嵌套**位置插值（`environment:` 的值、healthcheck 里的 `$${HYDRA_ADMIN_TOKEN}` 转义形式）⇒ 实测 `unjudgeableServices()` 对三个文件仍为 `[]`，grace/health 真树仍 exit 0。**测试 +1**（含"三个真文件不得被拒绝"的回归断言）；**证伪**：关掉该判据 ⇒ **恰好**那条红。

### B. `check_ci_wiring` rule 4：`--test <target>` 是**子串**判据

`c.includes("--test " + target)` 被**更长的 target 名**满足（`--test usage_query` 命中 `--test usage_query_wire`）⇒ 重命名后废弃目标的 `#[ignore]` 套件会**永远**显示"已接线"。修法与 `selectsCrate` 同形：整词正则 + `(?![A-Za-z0-9_-])`，并把 `--ignored` 也限定为整词。
**测试 +2**（子串不得满足 + 控制组精确名满足）。

### C. `check_documented_metrics`：把**出现次数**当"名字个数"（69 vs 36 distinct）

OK 行与 floor 都用 `checked`（每次出现 +1）⇒ 一个名字重复 25 次即可满足 `MIN_CHECKED=20`。修法同第一百五十一轮对 alert 守卫做的：新增 `distinctNames` 集合，**floor 按 distinct 计**，OK 行改为 `69 documented mention(s) over 36 distinct metric name(s)`，floor 失败文案同步改为 `only N DISTINCT documented name(s) checked (< M) across K mention(s)`。
**测试 +1**（一个名字重复 25 次 ⇒ `MIN_CHECKED=20` 必须 **exit 2**）；**证伪**：改回 occurrence 判据 ⇒ **恰好**那条红（输出 `OK (25 documented mention(s) over 1 distinct …)`）。

### D. 死变量 `checked`（`check_ci_wiring`）——注释宣称它是"防静默通过"的机制，实际从未被读取

原注释：`// artifacts examined (so a broken glob cannot pass silently)`，而 `grep -n checked` 只有"累加"与"声明"两行，没有任何读取处 ⇒ **注释在描述一个不存在的保护**。修法：OK 行打印它（`OK (everything is executed — 47 artifact(s) examined)`），注释改为实话；**测试 +1** 断言 OK 行含该计数；**证伪**：把打印去掉 ⇒ **恰好**那条红。

### E. ★我自己的测试死代码 —— `check_test_tails` 抓到了我

本轮给 rule 4 补的两条用例是 `cat >>` 追加到 `scripts/check_ci_wiring.test.cjs` **末尾**的，而该文件末尾是 `process.exit(failures ? 1 : 0)` ⇒ **用例从未运行**（探针"没变红"就是这个原因，我一度以为"用例不判别"）。**抓到它的是本计划第一百三十七轮为这个坑写的 `check_test_tails`**：`DEAD scripts/check_ci_wiring.test.cjs:1235 — 62 line(s) of code sit AFTER the top-level exit at line 1230`。把 67 行移到 summary 之前后，用例才真正运行，rule 4 的证伪随即变红。**这是本会话第三次踩同一个坑（第一百三十二轮、第一百三十七轮、本轮），也是第一次由守卫而不是偶然发现。**
顺带修掉两条夹具缺陷：CONTROL 夹具漏了共享 `crateTests` 需要的 `--test boot_listeners -- --ignored` 步骤（导致"控制组"因为**别的**规则变红——复审 A 在 `check_ci_wiring.test.cjs` 里点名过同一类"名字对、机制错"）。

### F. 探针自身的读数也要核对

本轮第一版探针用 `/^(not ok|FAIL)/` 过滤红行，而这几套套件打印的是 `   FAIL  <label>`（**带缩进**）⇒ 一条真红被过滤成 `red=0`（我差点据此判定"用例不判别"）。改为 `/(^|\s)(not ok|FAIL)\b/` 后读数与事实一致。**教训**：探针的**报告**同样需要自检（与第一百四十三轮"探针只读 stdout、而套件把 FAIL 打到 stderr"是同一族）。

**验证**：本轮涉及的 12 条守卫/套件全部 exit 0；`FALSIFY` = 0；**门禁 88 项全绿、`OVERALL=GREEN`、`exit 0`**（`grep -E "exit=[^0]"` 无输出；本轮改动全部在门禁启动之前完成 ⇒ 覆盖完整）。**本轮无产品代码改动**（4 个守卫 + 1 个共享模块 + 4 个测试文件 + 文档）。

**队列剩余（复审 P2/P3 未修项）**：`check_ci_wiring` 的 `run: |` 块体内一行 `- ` 会截断/丢弃整个 step（假红 + 静默假绿）、`stepShapeProblems` 与 `extractSteps` **两份解析器**应合一；`check_tenant_error_codes` 源码路径不可读时**崩溃 exit 1**（应 2）、`TEC_MIN_COMPARED=0` 可整体关掉覆盖下限、空表打印 `OK (0 …)` 且文案把 floor 说成 mismatch；"N/N 断言"计数漂移；两条"名字对、机制错"的用例（`check_ci_wiring.test.cjs` 的 `an empty skeleton trips the floors`）；`check_gate_entries` 的 `CGE_MIN_ENTRIES` 与"脚本过短"两条 floor 缺独立用例。

---
## 2en. 第一百五十七轮：清复审 P2/P3 的 tenant-codes 三条 —— **崩溃被当成"漂移"**、**空表打印 OK(0)**、**floor 失败被说成"status mismatch"**

三条都先复现（同一组夹具）：

| 现象（修复前） | 实测 |
| --- | --- |
| `TEC_SRC` 指向**文件**（而非目录） | 未捕获 `ENOTDIR` + 栈 ⇒ **exit 1**，而头部把 1 定义为"a mismatch"（输入不可读应是 2） |
| §6 表**只有表头**、且 `TEC_MIN_COMPARED=0` | `OK (0 documented error code(s) match the emitted status …)`、**exit 0** |
| 表非空但覆盖 floor 破了（1 条可比 vs floor 15） | `DRIFT only 0 error code(s) could be compared (< 15) …` + **`1 documented-vs-emitted status mismatch(es)`** ⇒ **0 个 mismatch 却报 1 个**，且 **exit 1**（应为 2） |

**修法**：
1. `walk()` 读目录失败 ⇒ 抛 `ScanError`（`code = 2`）；**调用点**包 try/catch ⇒ `CANNOT VERIFY: cannot read the source tree …` + **exit 2**（与同文件读文档失败时的行为一致）。
2. `documented.size === 0` ⇒ **无条件** `CANNOT VERIFY`（"there is nothing to compare"）+ **exit 2**，不再受 `TEC_DOC` 影响；`MIN_COMPARED` 夹到 `Math.max(1, …)`，`TEC_MIN_COMPARED=0` **不能**把唯一的 floor 关掉。
3. floor 与 mismatch **分开计数与措辞**：两者仍在**同一次运行**里全部打印（第一百三十六轮的纪律），但退出码按"更强者"取 —— `floors.length > 0 ? 2 : 1`，汇总行改为 `${problems.length} documented-vs-emitted status mismatch(es), ${floors.length} coverage problem(s)`。

**测试 +4（含 1 组参数化）**：①源码树不可读 ⇒ **exit 2** 且文案正确（不得是栈）；②空表在 `TEC_MIN_COMPARED=0` 与 `1` 两种取值下都 **exit 2**；③**floor 单独失败**（一份完全正确、可比的表 + floor 15）⇒ **exit 2** 且汇总行是 `0 … mismatch(es), 1 coverage problem(s)`（旧版正是"1 mismatch, exit 1"）；④**all-unverifiable 运行 + `TEC_MIN_COMPARED=0`**（用随包表的三条**已记录**行 + 一棵抽不出任何东西的源码树，这样没有未记录项来"顺带"报错）⇒ 必须 **exit 2** —— 这条专门钉住 `Math.max(1, …)` 这个夹子（**探针初版"没变红"就是因为少了它**：夹子被空表判据遮蔽，属于"两道防线只有一道被测"的形态，与第一百四十一轮 `pythonMainBlock` 那次同源）。
**证伪 4 枚（每枚先自证已应用）**：①去掉 walk 的保护 ⇒ 恰好"不可读 ⇒ exit 2"那条红（输出仍是 `CANNOT VERIFY: probe: unguarded`）；②去掉空表判据 ⇒ 恰好两条空表用例红（`it exited 0` / `status=2`）；③把 floor 的退出码改回 1 ⇒ **8 条红**（所有 floor 相关断言 + 依赖"更强者"语义的行）；④把夹子改回 `Number(...)` ⇒ 恰好第④条红。
**顺带修正的既有断言**：6 条旧用例原本断言 floor 失败时 `status === 1`（那是把"这检查什么都没证明"降级成普通漂移），现按新语义改为 **2**，并在断言里同时要求**漂移文案仍被打印**（"一次运行报全"）。

**真树不变**：`30 一致 / 3 记录`、exit 0。**验证**：12 条守卫/套件全 exit 0、`FALSIFY` = 0、**门禁 88 项全绿、`OVERALL=GREEN`、`exit 0`**（`grep -E "exit=[^0]"` 无输出；本轮改动全部在门禁启动之前完成 ⇒ 覆盖完整）。**本轮无产品代码改动**（1 个守卫 + 1 个测试文件 + 文档）。

**队列剩余（复审 P2/P3 未修项）**：`check_ci_wiring` 的 `run: |` 块体内一行 `- ` 会截断/丢弃整个 step（假红 + 静默假绿）与"`stepShapeProblems`/`extractSteps` 两份解析器应合一"；"N/N 断言"计数漂移；`an empty skeleton trips the floors` 这条"名字对、机制错"的用例；`check_gate_entries` 的 `CGE_MIN_ENTRIES` 与"脚本过短"两条 floor 缺独立用例。

---
## 2eo. 第一百五十八轮：清复审 P2/P3 的最后三条 —— `run: |` 块体内一行 `- ` 会**丢掉整段命令**；两条"名字对、机制错/过期"的用例与计数

### A. ★`run: |` 块体内以 `- ` 开头的行被当成"新 step"，其后所有命令**整段消失**

**复现（本轮独立）**：块体里先写 `- printf 1`、再写真正的命令

```yaml
      - run: |
          echo "starting"
          - printf 1
          python3 integration/test_old.py
```
⇒ 旧版报 `UNWIRED integration/test_old.py is never executed by ci.yml` —— **假红**，指向一个**正确**的文件。机制：解析器在**任何** `/^\s*-\s/` 行切块，而切出来的"新 step"没有 `run:` ⇒ `extractSteps` **不发射它** ⇒ 其后的所有命令（含真正的调用）**被静默丢弃**。
**★诚实标注**：复审给的两个方向里，**"假红"我复现了**；"整条 step 消失且无任何 problem（静默假绿）"这个方向我按它给的形状**没有复现出来**（把 `- ` 行放到块体最后时，命令在切分**之前**、仍被看到 ⇒ `OK`）—— 记录为"未能复现，可能有更窄的触发条件"，而不是照抄成结论。
**修法**：给解析器加**块体状态**：遇到 `run: |`/`run: >` 记下它的缩进，其后**更缩进**的行（含以 `- ` 开头的行、以及空行）一律当作**脚本文本**追加到当前 step；缩进回落到该行时块体结束。**这一判定必须放在 job/step 键处理之前**（块体里一行 `    foo: bar` 长得像一个键）。**★实现上的两个坑（都靠实测发现）**：①`      - run: |` 这个形态**同时**是"新 step"与"块标量开头"，只处理不带 `- ` 的形式 ⇒ **修法一开始完全无效**（夹具仍报 UNWIRED）；②把检测补到 `- ` 分支后两个方向才都正确。
**测试 +2**：①块体内 `- ` 之后的真命令**必须**算接线（控制组：把真命令删掉 ⇒ 仍必须 UNWIRED，证明这条用例通过是"命令被看见了"，不是"工件不再被要求"）。
**证伪 2 枚**：①关掉块体消费 ⇒ **恰好**那条红并复现 `UNWIRED …`；②只关掉"`- ` 形态的块标量检测"（保留非 `-` 形态）⇒ **同样恰好**那条红 —— 这枚专门证明第②个坑是承重的。

### B. 两条"名字对、机制错 / 数字过期"的测试与文档

- `check_ci_wiring.test.cjs` 里名叫 **"an empty skeleton trips the floors"** 的用例：它断言的是"空骨架会被拒绝（不是静默通过）"，而**在脚本 floor 被关掉后它依旧会红**（复审实测：靠 `playwright.config.cjs declares no testDir` 这条**别的**规则）⇒ 名字会把读者骗进"floor 有用例"的清单。**改名**为 `an empty skeleton is refused (not a silent pass)`，并在注释里指出"名字带 floor 的用例才是 floor 的用例"，同时点明真正承重的 `the script-artifact floor catches a shrunken scripts/ directory`。
- 计划里的 N/N 计数已漂移（compose_static 16→**17**、health 17→**18**、grace 19）⇒ 改为实测值并在文里写明"**这类计数会随测试增长而变，复核以 `# pass` 为准**"。
- **★一条被证伪的复审发现（负结论）**：复审 P3 称 `check_gate_entries` 的 `CGE_MIN_ENTRIES` 与"脚本过短"两个判据"没有任何用例"。**实测反驳**：把 `if (all.length < MIN_ENTRIES)` 置假后，套件里**恰好** `a gate script too short to be the real one is CANNOT VERIFY, never a pass` 一条红（`# pass 11 / # fail 1`）⇒ 该判据**已被钉住**（那条用例写一个 1 条目的 gate 脚本、走的就是默认 `MIN_ENTRIES=40`）。记录为负结论，不据此改代码。

**验证**：11 条守卫/套件全 exit 0；`FALSIFY` = 0；**门禁 88 项全绿、`OVERALL=GREEN`、`exit 0`**（`grep -E "exit=[^0]"` 无输出；本轮改动全部在门禁启动之前完成 ⇒ 覆盖完整）。**本轮无产品代码改动**（1 个守卫 + 3 个测试文件 + 文档）。

**队列剩余（复审 P2/P3 的最后一件）**：`check_ci_wiring` 有**两份解析器**（`extractSteps` 与 `stepShapeProblems`），后者应当复用前者的状态机 —— 本文件自己的注释就反对"two owners of what is a command"，但合并是一次结构性改动（影响"重复键/丢失 `- name:`"那类判据），留作单独一轮。

---
## 2ep. 第一百五十九轮：清复审 P2/P3 的最后一件 —— `check_ci_wiring` 的**两份解析器**合一（"什么算一个 step 块"从此只有一个所有者）

**问题**：`extractSteps`（解析步骤）与 `stepShapeProblems`（找"丢了 `- name:` 标记导致 `run:`/`env:` 重复键"）**各自实现了一套扫描器**，而且第二套仍按"任何 `- ` 行开新块"切分 —— 于是**块标量体内一行 `- ` 会把块切开**，把本该同属一个 step 的重复键**分到两块里**，重复键就**查不出来了**（GitHub 会直接拒绝这种 workflow，而宽容的解析器让后者赢 ⇒ 另一个 step 被静默丢掉）。本文件自己的注释就反对这种"two owners of what is a command"。

**修法（真正的单一所有者）**：
1. 新增 `blockScalarEnd(lines, i)`：**唯一**回答"这个块标量的体到哪一行结束"（缩进规则 + 空行容忍）。
2. `stepBlocks(text)` 升级为**唯一的扫描器**：它同时负责 ①`jobs:`/job 键/job 级 `if:`/`defaults:`（job 上下文，含 `jobDisabled` 与 `defaultDir`）②块标量体的归属 ③以 `- ` 行切块；每个块带上 `{ lines, startLine, job, jobDisabled, defaultDir }`。
3. `extractSteps(text)` 退化为**纯映射**（从块里读 `dir`/`run`/`cond`/`lenient`，块体范围问 `blockScalarEnd`，job 默认目录在映射里应用）；`stepShapeProblems(text)` 改为遍历**同一批块**，数 `run:`/`env:` 键时用 `blockScalarEnd` **跳过块体**。

**新增测试 3 条**：①**合并 step 的重复键落在块体 `- ` 行之后**时仍必须被抓到（旧版**静默漏掉** —— 这就是本轮要修的方向）；②**块体内写成 YAML 的 `run:` 行不是重复键**（假阳性护栏：shell heredoc 里写 `run: something` 曾被数成第二个 `run:` 键）；③此前两条（块体 `- ` 不隐藏命令、注释不隐藏命令）继续有效。
**证伪 2 枚（每枚先自证已应用）**：①把 `blockScalarEnd` 变成恒 `null`（等价于旧的"任何 `- ` 行切块"）⇒ **5 条红**（块标量相关的 4 条 + 新加的重复键那条）；②把 `stepShapeProblems` 数键时的"跳过块体"去掉 ⇒ **恰好**第②条红 ⇒ 证明这条护栏承重，而不是装饰。
**★夹具自身的一课**：我第一版"重复键"夹具把第二个 `run:` 写在 `run: |` 的**更深缩进**处 —— 在 YAML 里那本来就是**标量内容**、不是重复键，用例"失败"得毫无意义；改成真实的**丢了 `- ` 标记**形态（键与第一个 `run:` 同缩进）后才成立。

**验证**：13 条守卫/套件全 exit 0；`FALSIFY` = 0；**门禁 88 项全绿、`OVERALL=GREEN`、`exit 0`**（`grep -E "exit=[^0]"` 无输出；本轮改动全部在门禁启动之前完成 ⇒ 覆盖完整）。**本轮无产品代码改动**（1 个守卫 + 1 个测试文件 + 文档）。

**队列状态**：**两份复审（A：守卫改动 14 条 / B：验证纪律 15 条）的全部 P1/P2/P3 至此处置完毕**（§2el–§2ep）。剩下的只有**需要你决策**的事（16 项决策表）—— 最紧的仍是 **D-8**（官方拓扑下 edge `/metrics` 恒 401）、**D-15**（bucket 是否哈希化，含滚动升级限流翻倍的迁移方案）、**D-16**（`matching_key` 明文列是否封存）、**D-17**（admin 写响应是否回带校验告警）、**D-14**（准入上限热重载不 resize）、**D-13**（生产 `expect`：第 201 轮后为 **5 处**，见决策表前的复测块）。

---
## 2eq. 第一百六十轮：把"数**出现次数**、报**名字个数**"这一族**横扫**到最后一个守卫（`check_documented_env`），并派出新一轮只读复审

### A. `check_documented_env` 的 floor 数的是出现次数（与 metrics 同族）

第一百五十六轮在 `check_documented_metrics` 修掉了"OK 行把出现次数称作 name(s)、floor 也按出现次数算"，本轮按**同一类**横扫其余守卫，命中 `check_documented_env.cjs`：
`checked += 1` 是**逐出现**累加（一行表格可以点名同一个变量、同一个名字也可以出现在多行），`if (checked < MIN_ROWS)` 因此可被重复行满足，而 OK 行写的是 `44 documented env name(s)`。
**先量再改**：真表 **44 occurrences / 40 distinct**（四个名字出现两次）—— 差距小，但两个数字**本来就会各自漂移**，而 floor 跟的必须是"到底验证了多少"。
**修法**：新增 `checkedNames` 集合；floor 改为 `checkedNames.size < MIN_ROWS`（失败文案 `only N DISTINCT env name(s) checked (< floor M) across K occurrence(s)`）；OK 行改为 `40 distinct documented env name(s) (44 occurrence(s))`。
**★顺手纠正我自己刚写下的错注释**：第一版注释写"实测 44 occurrences, 44 distinct"——**这是我想当然**（真值 40 distinct），写完立刻用守卫自己的输出核对并改正；这正是本计划"注释里的数字必须来自测量"那一条。
**测试 +1**：一个变量重复 25 行 ⇒ `DOC_ENV_MIN_ROWS=20` 必须**拒绝**（旧版通过）；另两条旧断言按新文案更新（`3 distinct documented env name(s)`、`distinct documented env name(s) (… occurrence(s)) in dev-docs/ops.md`）——**本会话第四次遇到"断言写的是旧文案"**。
**证伪**：把判据换回 `checked < MIN_ROWS` ⇒ **恰好**那条新用例红（`not ok 26`）。还原用验证式备份（`md5 = 61ecab4e…`），还原后 **26/26** 全绿。

### B. 派出新一轮只读复审（队列清空后的惯例）

§2dw 与 §2el 的两批复审条目已全部处置，故按计划惯例**再派两个只读 Oracle**，这次打在**尚未被这两批复审覆盖**的两块地面：
1. **drill 的腿能不能变红**：逐个检查 `integration/test_limit_roles_enforcement.py`、`test_replica_fidelity.py`、`test_cluster_limits.py`、`test_auth_cache_layers.py`、`test_admission_queue.py`、`test_body_cap_drain.py`、`test_usage_drop_accounting.py` 里每条断言的**可证伪性**（历史上出现过"只存在于注释里的角色"、"断言用被测实现自己算期望值"、"控制组在被测机制关掉后照样绿"这类），以及 mock 响应/故障注入是不是真的能失败。**明确告知这是静态复审、不跑 drill**，报告里必须标注这一限制。
2. **门禁与 CI 本身会不会吞掉判定**：逐条检查 `.acceptance/round10-gate.sh` 的 `gate` 条目（管道无 `pipefail`、`|| true`、`tee`/`grep` 吞码、子 shell 丢状态）与 `ci.yml` 的 `run:` 块（**同一类从未被审过**）；找"永远退 0 的检查"；核对文档（`ops.md`、计划 §4）对门禁的描述与**当前**文件是否一致（第一百五十五轮刚修过 §4 里"13 条门禁"那个数）；核对 `scripts/`/`tools/` 下**既不在门禁也不在 CI** 里执行的"看起来是检查/测试"的工件。
两个子代理在后台运行，结果在下一轮处置。

**验证**：8 条守卫/套件全 exit 0；`FALSIFY` = 0；**门禁 88 项全绿、`OVERALL=GREEN`、`exit 0`**（判定见下）。**本轮无产品代码改动**（1 个守卫 + 1 个测试文件 + 文档 + 两个只读子代理）。

---
## 2er. 第一百六十一轮：处置新一批复审的 **P1 四条**（三条"腿永远为绿" + 一个**结构非法的 CI step**），并给守卫补上这类判据

第一百六十轮派出的两个只读复审交回：**门禁/CI 审计**（3 个 P1 + 4 个 P2 + 6 个 P3）与 **drill 静态审计**（4 个 P1 + 16 个 P2 + 一批 P3）。本轮**逐条自行复现**后先修 P1，并把新队列（含 P2/P3）记在下面。

### A. ★`.github/workflows/ci.yml:307` 有一个**只有 `name:`、没有 `run:`/`uses:`** 的 step

**复现（YAML 解析，非我的行扫描器）**：`python3 -c "yaml.safe_load(...)"` ⇒ `live-deps: step 6 has NO run/uses: {'name': 'The ClickHouse end-to-end tests (normally ignored: needs CH_URL)'}`。它的工作是**后面**那条 `ClickHouse sink + usage query against a live instance (ignored tests)` 在做 ⇒ 这行是一个**遗留的重名壳**：既让"live-deps 会跑 CH e2e"这句话依赖一个空 step，又让**整份 workflow 不满足 GitHub 的 schema**（每个 step 必须有 `run` 或 `uses`）——而**本地没有任何守卫看得见它**（`stepShapeProblems` 只查**重复键**，不查**缺键**）。
**修法两步**：①删掉这个孤儿 step；②让守卫能抓这一类：`stepShapeProblems` 新增"每个 step 必须有 `run:` 或 `uses:`"的判定。
**★修法里的一个坑（我自己踩的）**：第一版判据在真文件上**误报** `line 251` —— 我的行扫描器把 **`services: clickhouse: ports:` 的列表项 `- 8123:8123`** 也当成了 step（YAML 解析器不会）。⇒ 给扫描器加 `inSteps` 状态（只有 job 的 `steps:` 段里的列表项才算 step）后才正确；**测试 +2**（①无 `run`/`uses` 的 step 必须被报；②**控制组**：`services:` 里的 `ports:` 列表项**不得**被判成 step）。
**证伪**：真文件上删掉 `inSteps` 作用域 ⇒ 立刻在 `- 8123:8123` 上误报（这就是我实测到的现象本身）。

### B. ★`integration/test_limit_roles_enforcement.py:247` 的 `or True`（恒真）

**引用**：`check("L1: the refusal is \`429 rate_limited\` in the data-plane envelope", 'rate_limited' in (headers_last.get("X-Noop") or "") or True, …)` —— 布尔式以 **`or True`** 结尾 ⇒ **恒真**；它读的还是一个**不存在的头** `X-Noop`。产品把 `rate_limited` 从 429 信封里整个删掉，这条腿照绿（它还会被计入 PASSED 统计，让人以为信封验过了）。
**修法**：改为断言**真的在响应里的东西**（状态码取自循环、不看那个不存在的头）：`codes[2] == 429 and "json" in ct.lower()`（`ct` 取 `Content-Type`/`content-type` 两种拼写）。
**★我第一版又写错一次**：改后的判据引用了 `st429`，而 `st429` 是**下一条请求**才赋值的 ⇒ 会 `NameError`；改成用循环里的 `codes[2]` 与已经拿到的 `headers_last`。
**证伪**：把期望的 content-type 改成 `text/plain` ⇒ 该 drill **FAILED(1)** 并点名这条腿，输出 `HTTP 429 content-type='application/json'`。

### C. ★`integration/test_admission_queue.py:268` 的**优先级**错误（"恰好 2" 变成"任意非零"）

**引用**：`check(…, max(inflights) if inflights else 0 == 2, …)` ⇒ Python 里条件表达式优先级低于 `==`，实际是 `max(inflights) if inflights else (0 == 2)` ⇒ **只要采样非空且峰值非零就通过**。**实测验证**：`inflights=[8]` ⇒ 表达式值 `8`（真）而"本意"是 `False`。于是并发上限被放大到 8 时这条腿仍打印 PASS（同一行 detail 还把 `max in-flight=8` 印在 PASS 旁边）。
**修法**：加括号 `(max(inflights) if inflights else 0) == 2`。**证伪**：把期望改成 3 ⇒ drill **FAILED(1)** 并点名该腿（输出 `max in-flight=2`）。

### D. ★同族横扫：**第二个 `or True`**（复审范围外，被本轮扫出来）

`integration/test_model_catalog.py:346`：`check("C8: ...and neither is HEAD", st_head != 200 or True, …)` ⇒ 同样是恒真 ⇒ 改为 `st_head != 200`。**证伪**：改成 `st_head == 200` ⇒ drill **FAILED(1)**，输出 `HEAD -> HTTP 501`（真实值）。**注**：复审的报告只点出第一条 `or True`（它审的 7 个 drill 不含 `test_model_catalog.py`）——这是"按类横扫"再次补上复审盲区的例子。

**验证**：6 条守卫/套件 + 3 个 drill 全部 exit 0（`limit_roles`/`admission`/`catalog` 三份 PASSED，且都带着**修好后真的被断言的数值**：`max in-flight=2`、`content-type='application/json'`、`HEAD -> HTTP 501`）；`FALSIFY` = 0；**门禁 88 项全绿、`OVERALL=GREEN`、`exit 0`**（`grep -E "exit=[^0]"` 无输出；`ci wiring`×2、`limit_roles enforcement`、`model catalog`、`admission queue`、`sub-tenant steering` 均 exit=0 ⇒ 本轮改动全部被门禁覆盖）。**本轮有产品外的工件改动**（CI workflow 1 行删除、1 个守卫 + 1 个测试文件、3 个 drill 的断言）。

### 新队列（两份复审的 P2/P3，留待后续轮次；按"会不会骗人"排序）

- **drill 侧 P1 未修两条**：①`test_replica_fidelity.py:66` 的 `MASKED_KEY = LIMIT_KEY`，而 `r-shared` 的 `matching_key` 正是同一字符串 ⇒ **"mask 形态才生效"这条腿在 mask 匹配坏掉时也绿**（真正锚定它的是 `:417-422` 的 collide 腿）；②`test_admission_queue.py:300-304` 的 Q2 `reason="timeout"` 读的是**进程级累计计数**，Q1 自己就会产生一次 timeout（仓库实测 `{full}=3/{timeout}=1`）⇒ 等待预算失效时这条腿仍绿。修法分别是"给 mask 腿一个没有 raw role 指向的 key"与"取增量"。
- **drill 侧 P2（16 条）**：最重的几条 —— `test_limit_roles_enforcement.py:261` 的 L2 429 可能来自已超限的 `r-count`（无法隔离"取最严"）；`:429` 的 L8 依赖"mock 在 slow 上睡 1.5s"却**从未断言前提**；`test_replica_fidelity.py` 的 F4/F5 **从不做 promotion**（只重启 edge）而 PASS 文案宣称"survive its promotion"，且 §13 的 disabled 行/`provider_key` 身份**没有任何腿观测**；`test_cluster_limits.py:476` 的三次 POST = 三个版本 ⇒ "整个版本收敛"推不出来；`:566` 的"命令超时把黑洞变成普通错误"从未被夹具触发（relay 是**立刻拒绝**而非黑洞）；`:591` 的"恢复"只要求 45 次探针里出现**一次** 429（窗口被清空后由探针自己填满也算 recovered）；`test_auth_cache_layers.py:343` 丢弃了 invalidate 的响应（没断言 fleet 扇出）；`:184` 的 `KEYS hydra:{auth}:*` 会把**索引集**也算进结论键，`keys[0]` 顺序敏感可能让腿**因错误的原因变红**。
- **门禁/CI 侧 P2/P3**：①`npm run build >/dev/null 2>&1;` 用 `;` 而非 `&&`（门禁 4 处 + CI 2 处）⇒ 编译失败被吞、drill 可能对**陈旧 `dist/`** 报绿（复审用假 npm 复现出 `chain exit=0`）；②`findings disposition` 这条 entry 实际只核 `2026-09-17-tenant-api.md` 的**文本字符串**（`grep -c oracle-remediation` = 0）⇒ 与 oracle-remediation 的任何修复无关；③门禁注释与 plan 说"CI 每步都自建二进制"，而 CI 有 3 个 step 不建（注释与环境相反）；④`check_gate_entries` 的真实覆盖是 **5/35**，OK 行应把"自建 N / 继承 M"拆开印；⑤`scripts/e2e-local.sh` 与 `scripts/load_test.sh`（含 4 条自检）**既不在门禁也不在 CI**，而 §2ah 拿前者当 ui-e2e 的覆盖证据；⑥plan §4 声称门禁跑"上面绝大多数命令"（实测约 32/56，缺 Go/Python SDK 测试、`e2e-local.sh`、性能基线等）；⑦`.acceptance/` 整体不进版本控制（**D-5**，且"门禁存在"这件事本身没有守卫）；⑧门禁日志每次被截断、`GATE <name> exit=N` 只进 stdout ⇒ 事后无法判断"跑完绿了还是死在半路"（建议把汇总也写进日志）。

---
## 2es. 第一百六十二轮：清新队列的 drill P1 两条（mask 腿被顶包、Q2 读 Q1 的累计计数）+ 门禁 `npm run build` **吞掉失败**这一类

### A. ★mask 腿被 raw role 顶包 —— 用"同一产品故障下的**新旧夹具对照**"定案

**问题（复审）**：`integration/test_replica_fidelity.py:66` 的 `MASKED_KEY = LIMIT_KEY`，而 `r-shared` 的 `matching_key` **正是**那个字面 key ⇒ "mask 形态才真正生效"这条腿的 4 个请求被 **`r-shared`（raw 精确）与 `r-masked`（mask）同时**匹配 ⇒ **mask 匹配坏掉它也绿**。
**修法**：新增 `MASKED_ONLY_KEY`（19 字符、**掩码与 `MASKED_LIMIT_KEY` 相同**但**没有任何 raw role** 指向它）并加 `assert` 钉住它与 `LIMIT_KEY`/`RAW_ONLY_KEY` 不同；mask 腿改用它 ⇒ 该腿的拒绝只可能来自 `r-masked`。
**★决定性证据（产品故障下的对照实验）**：把 `crates/hydra-core/src/limit.rs` 的 `key_dim_matches` 改成**只比 raw**（等价于把 mask 匹配关掉）、重建后跑同一个 drill：
- **旧夹具**（`MASKED_ONLY_KEY = LIMIT_KEY`）⇒ 该腿 **PASS**（`codes=[200,200,429,429]`）——**复审的断言被实测坐实**；
- **新夹具** ⇒ 该腿 **FAIL**（`codes=[200,200,200,200]`）。
即：这条腿从"无论产品怎么做都绿"变成"被测机制一坏就红"。还原产品码后 `cargo test -p hydra-core --test limit` **11 passed**、drill **PASSED**。
**★探针自身又踩一次坑**：模拟旧夹具时我先被**自己刚加的 `assert`** 挡住（drill 直接 AssertionError、连 F0b 都没跑到）⇒ 旁路该 assert 后才完成对照。

### B. ★Q2 的 `reason="timeout"` 读的是**进程级累计**计数（Q1 自己就产生一次）

**问题（复审）**：`hydra_queue_drops_total` 是进程级 vec、节点全程只启动一次，而 Q1（cap=2/queue=3/wait=2000ms/8 并发）本身就会产生一次 `timeout` 丢弃（仓库自己的实测 `{full}=3 / {timeout}=1`）⇒ Q2 用 `any('reason="timeout"')` 判"等待预算生效"，**Q1 的样本就把它满足了**：等待预算一旦失效、waiter 全按 `full` shed，这条腿照绿。
**修法**：Q2 前快照 `drops_before`，断言**增量** `timeout_now > timeout_before`（detail 打出 `before -> now`）。
**实测**：修后输出 `timeout drops 1 -> 2` ——**Q1 的那 1 次被显式排除**。**证伪**：把判据改成 `timeout_now > timeout_before + 5`（不可能）⇒ drill **FAILED(1)** 并点名该腿 ⇒ 比较是"活的"。

### C. ★门禁 4 条 `npm run build >/dev/null 2>&1;` 用 `;` 收尾 ⇒ 编译失败被吞、drill 可能对**陈旧 `dist/`** 报绿

**问题（复审）**：`tools/hydra-ts`/`hydra-cli` 的 `npm run build`（`tsc`/`tsup`，类型错误即非零）在门禁里由 `;` 分隔 ⇒ 构建失败后**后面的 drill 照样跑**，而 `tools/*/dist` 在当前树里**存在**（未 ignore）⇒ 演练的是**上一次的产物**，编译错误连输出都被 `2>/dev/null` 吃掉。（CI 的四处已是 `&&`，所以这是门禁侧的问题。）
**修法**：4 条 entry 改为 `npm run build >/dev/null &&`（stdout 静音、stderr 保留）。
**★证明（复审的方法，我复现了）**：用一个**必定失败**的 `npm` shim：旧形态 ⇒ 链**继续**（`exit=2`，"drill would run against a stale dist/"）；新形态 ⇒ 链**停止**（不打印 "drill reached"，exit 2）。
**★并加机械判据**：`check_gate_entries.cjs` 新增"构建必须以 `&&` 结尾"的规则（`cargo build … --bin hydra` / `npm run build` 之后的**第一个分隔符**若是 `;` 且其后还有命令 ⇒ DRIFT）。**测试 +2**（`;` 形态必报 + `&&` 控制组）；**证伪**：把真门禁里一条 entry 改回 `;` ⇒ **恰好**该条 DRIFT 并点名它。
**★规则自己的坑**：第一版用字符类写正则，被 `2>&1` 里的 `&` 挡住 ⇒ **规则静默匹配不到任何东西**（探针"没变红"），改成"先定位构建、再看它之后的第一个分隔符"的扫描式实现才对。
**★而它当场抓到了我自己**：我为了做上面那个证伪把门禁脚本从备份"还原"，而**那份备份是修复前取的** ⇒ 4 处 `;` **全被还原回来**；新规则立刻报 **3 条 DRIFT**。这正是本计划反复记的"备份必须取在修复之后并验证"——本次由守卫当场发现。重做修复并取**修复后且经 `grep -c` 验证**的备份（`.postfix2`，`grep -c "npm run build >/dev/null &&"` = 4、`grep -c "2>&1;"` = 0）。

**验证**：4 条守卫/套件 + 4 个 drill 语法检查全过；`FALSIFY` = 0；**门禁 88 项全绿、`OVERALL=GREEN`、`exit 0`**（`grep -E "exit=[^0]"` 无输出；`replica fidelity`、`admission queue`、`tenant TS SDK vs live node/cluster`、`admin CLI vs live node` 均 exit=0 ⇒ 本轮改动全部被门禁覆盖）。**本轮无产品代码改动**（drill 2 个 + 门禁 4 条 + 1 个守卫 + 1 个测试文件 + 文档）。

---
## 2et. 第一百六十三轮：门禁的**证据链**（日志必须有终结标记）、一条**名字骗人**的 entry、以及 fidelity drill 里**说大了的声明**

### A. 门禁日志每次被截断、判定只进 stdout ⇒ "跑完绿了"与"死在半路"事后无法区分

**问题（复审）**：`.acceptance/round10-gate.sh` 开头 `: > "$LOG"`，而 `GATE <name> exit=N` 与 `OVERALL=…` **只打在终端**；一旦运行被打断（kill、终端消失、磁盘满），日志会**停在中间**，而事后看它和"跑完绿了"无法区分 —— 本仓库最在意的那类证据链问题。
**修法**：①每条 entry 的判定改为 `echo "GATE $name exit=$rc" | tee -a "$LOG"`（stdout 与日志都有）并追加 `########## exit=$rc`；②汇总与判定**同时写进日志**，日志最后三行为 `OVERALL=…` / `entries=N` / **`GATE COMPLETE`** ⇒ **"日志有尾且带判定"成为可判定事实**。
**并加机械判据**：`check_gate_entries.cjs` 的"必须返回判定"规则扩展为"**并且**必须把终结标记写进 `$LOG`"。**测试 +2**（无标记 ⇒ DRIFT；控制组有标记 ⇒ 通过）；**证伪**：把真门禁里的 `GATE COMPLETE` 删掉 ⇒ **恰好**该条 DRIFT。
**★又是"备份取在修复之前"（第 N 次）**：我做证伪后从 `.postfix` 还原门禁，而那份备份取在**标记改动之前** ⇒ 标记被还原掉、新判据**当场报 DRIFT 抓到我**（与上一轮同一坑，同样由守卫当场发现）；随后重做并取**验证式备份** `.acceptance/round163-gate.sh.verified`（`grep -c "GATE COMPLETE"`=1、`npm run build >/dev/null &&`=4、`tenant-api plan markers`=1、`2>&1;`=0）。

### B. 名为 `findings disposition` 的 entry 与它名字暗示的对象**无关**

**问题（复审）**：该 entry 跑 `.acceptance/findings-disposition.py`，而脚本自陈"核对的是**计划正文里的字符串**"，指向的是 **`2026-09-17-tenant-api.md`**（`grep -c oracle-remediation` = **0**）。⇒ 它的 `exit=0` 很容易被读成"oracle-remediation 的修复都在"。
**修法**：entry 改名为 **`tenant-api plan markers (text only)`**，并在文件里写上它到底核什么、不核什么（"名字必须说清自己在核什么"）。

### C. ★fidelity drill 的声明"survive its promotion"在**两个地方**都没有支撑

**问题（复审）**：`test_replica_fidelity.py` 的 docstring F4/F5 与 PASS 文案都写"promoted node still…"，但 **F4/F5 实际是 `stop(edge)` 后按 `edge` 角色重启**（没有任何角色提升）；而 `test_cluster_ha.py` 虽然真的提升 standby，却**从不**在提升后的节点上复查"限流角色 / 令牌哈希 / 子租户路由"。⇒ **"提升后仍然生效"这句话在全仓没有任何腿能证伪**。
**修法（本轮只做"把话说对"）**：①docstring F4/F5 改为 "a RESTARTED edge…"，并写明**提升覆盖在 `test_cluster_ha.py`、而它不复查这三类行**；②PASS 文案改为 "survive a RESTART of that replica"；③在文件头部用一段"**本 drill 究竟观测到什么、没有观测到什么**"取代原先那句笼统的 §13 转述：观测到的是**启用**角色的限流/令牌哈希/子租户路由（F1–F3、重启后 F4/F5、leader 被杀后 F6）、**代理观测**的是"先禁用后启用"的角色能在下一次快照到达运行中的 edge（F7）、**未观测**的是 promotion 本身（并把缺口写清）。
**为什么先只改说法**：补齐"提升后仍执行限流"需要在 HA drill 里种 `limit-roles`/`tenant-tokens`/`sub-tenants`+`sub-tenant-routes` 并在提升后断言 —— 那会改动一个当前全绿的 drill 的夹具与断言，**属于独立一轮的工作**；本轮把声明先改成事实（"false claims are worse than no claims"），并把这个缺口连同**具体做法**记进队列。

**验证**：`check_gate_entries` 真树 exit 0、测试 **16/16**；`check_ci_wiring` exit 0；`FALSIFY` = 0；`test_replica_fidelity.py` 在改动后仍 **PASSED**（exit 0）；**门禁 88 项全绿、`OVERALL=GREEN`、`exit 0`**，且**新行为当场被证实**：`.acceptance/round10-gate.log` 的最后三行是 `OVERALL=GREEN` / `entries=88` / **`GATE COMPLETE`**（此后"日志有尾且带判定"可事后审计）。**本轮无产品代码改动**（门禁脚本 + 2 个测试文件 + 1 个 drill 的注释/文案 + 文档）。

---
## 2eu. 第一百六十四轮：补上"**提升后仍然生效**"这条**全仓无人验证**的缺口（fidelity drill 新增 P1–P4）

**背景**：第一百六十三轮把 `test_replica_fidelity.py` 的声明从"survive its promotion"改成了事实（F4/F5 只是重启 edge），并把真正的缺口记进队列 —— `test_cluster_ha.py` 虽真的提升 standby，却**从不**在提升后的节点上复查"限流角色 / 令牌哈希 / 子租户路由"。本轮把这条缺口**补成真实覆盖**。

**做法（在 fidelity drill 里加第三个节点 + 四条腿）**：
1. 新增 `S_ADMIN, S_DATA = 18740, 18741`，并在**播种之前**启动一个 `leader` 角色节点 `fid-standby`（它取租约的 stand-by 侧 ⇒ leader 被杀后它被**提升**）；收尾一并 `stop`。
2. leader 被杀（既有 F6 之后）新增 **P1–P4**：
   - **P1**：`leader_probe(S_ADMIN)` 变 200 ⇒ standby 确实被提升（503 = 仍是 standby）；
   - **P2**：在**被提升节点**上发请求 ⇒ **429**（429 = 角色行到了且配额生效；**200 = 租户在但角色行缺失**（请求被路由走）；**404 = 该节点根本没有这个租户的配置**）——三种结果可区分，正是这条腿是证据的原因；
   - **P3**：租户 API 令牌在**被提升节点**上仍 **200** 且 `tenant_id=t1`（否则 401：哈希没过去）；
   - **P4**：子租户前缀在**被提升节点**上仍被钉到 p-b（`tags` 全为 `b`）。

**★决定性证伪（产品零改动、只动夹具）**：把 standby 的 `HYDRA_CONTROL_URL` 指向一个**死端口**（它因此永远物化不出快照）后重跑：**P1 仍然 PASS**（提升是租约的事），而 **P2 FAIL（HTTP 404）→ P3 FAIL（401 unauthorized）→ P4 FAIL（`tags=['?','?','?','?']`）** ⇒ 这三条腿**确实**在测"被提升的节点知不知道这些行"，而不是在测租约。
**★加这条腿时踩的坑**：readiness 我最初用免鉴权的 `/healthz` 轮询，结果在 leader 角色节点上那是一串**未认证的管理面请求** ⇒ 触发 `HYDRA_ADMIN_AUTH_FAIL_LIMIT_PER_MIN` 节流，节点开始回 **429**，drill 报"the standby never became healthy"（**是节流，不是节点没起来**）；改成带 token 的 `/api/v1/health`（与 HA drill 同形）后正常。顺带把 standby 的日志尾部也打印出来（leader 路径本来就有，standby 路径没有）——**诊断缺失本身就是排查成本**。

**验证**：`test_replica_fidelity.py` 全绿（**含 P1–P4**，输出 `HTTP 429` / `HTTP 200 {"tenant_id":"t1"…}` / `tags=['b','b','b','b']`）；`check_test_tails`/`check_gate_entries`/`check_ci_wiring` exit 0；`FALSIFY` = 0；**门禁 88 项全绿、`OVERALL=GREEN`、`exit 0`**（`replica fidelity` exit=0，且**新腿确实在门禁里跑过**：门禁日志里 `P4: ...and still steers the sub-tenant` 出现 1 次；日志尾部仍是 `OVERALL=GREEN` / `entries=88` / `GATE COMPLETE`）。**本轮无产品代码改动**（1 个 drill + 文档）。

---
## 2ev. 第一百六十五轮：**实测推翻了 L2 的因果**（它是被"已经用完额度的 r-count"判绿的）并给 L8 补上前提断言

### A. ★L2「更严的重叠角色生效」其实一直在测别的东西

**复审怀疑**：`test_limit_roles_enforcement.py` 的 L2 只断言"下一个请求 429"，而 L1 已经把 `r-count` 的窗口用完 ⇒ 429 可能来自 `r-count`，于是"只取第一个匹配角色/只取最宽角色"的实现同样会绿。
**我按"先归因再下结论"改法去测**：断言这条 429 被记在 `r-tight` 名下（`hydra_limit_rejected_total{role=…}` 的增量）—— **它立刻失败**，并且带着决定性数字：

```
put r-tight 之后（等了 0.5s 让 reload 落地）: ['hydra_limit_rejected_total{dim="count",role="r-count",tenant="t1"} 2']
发出请求之后:                              ['… role="r-count" …} 3']
⇒ r-count 2 -> 3，r-tight 0 -> 0
```

即：**这条腿的 429 完全来自 `r-count`**（它自己的窗口在 L1 就被花光了），`r-tight` 一次都没参与 —— 复审的怀疑成立，而且**不是竞态**（加了 settle 等待仍然如此）。

**重新设计（真正隔离"两条都匹配时取更严的那条"）**：
- 换到 model **`other`**（`r-count` 只钉 model `echo`，因此不匹配）；
- 同时放两条都匹配的角色：**`r-wide`（limit 1000，本来会放行全部）** 与 **`r-tight`（limit 1）**；
- ①第 1 个请求必须 **200**（wide 会放行；若这里就 429，说明是顺序/运气而不是"取严"）；②第 2 个必须 **429**（wide 本来会放行 —— 这才是"取严"的判别点）；③**归因**必须是 `r-tight`（增量 >0）；④**控制组**：删掉 `r-tight` 后同一个请求又变 **200**（证明这条 429 确实是它的）。
- 实测四项全过（`HTTP 200` / `HTTP 429` / `r-tight 0 -> 1` / `HTTP 200`）；跑完把 `r-wide` 也删掉，保持后续腿的夹具与原先一致。
**证伪**：把 `r-tight` 的 `matching_tenant` 改成 `t9`（等于让它**不匹配**）⇒ **恰好 L2 的两条新断言红**（`HTTP 200`；`r-tight 0 -> 0`）—— 而**旧形态**在同一次实验里是绿的（因为它读的是 `r-count` 的 429），这正是"改写前后判别力"的直接对照。

### B. L8 的前提从未被断言（前提失效时这条腿会静默失去意义）

**问题（复审）**：L8 的全部推理建立在"删除角色时请求 1 **仍在飞行中**"（mock 对 `slow` 睡 1.5s），但这个前提**没有任何断言**：一旦延时不再发生（模型名改了、种子行被删、上游改写 model 字段），请求 1 会在 DELETE **之前**结束，记账发生在角色仍存在时 ⇒ 请求 2 照样 429 ⇒ **即使"一个请求用了两代配置"的 bug 回来了，这条腿仍然绿**。
**修法**：加两条**前提断言** —— ①0.4s 时线程仍存活（`t.is_alive()`）；②请求 1 的耗时 ≥ 1.0s（真的是慢上游，不是快路径）。实测：`thread alive after 0.4s = True`、`request 1 took 1.50s`。
**证伪**：把 mock 的 `time.sleep(1.5)` 改成 0（"慢上游不慢了"）⇒ **恰好这两条前提断言红**（`alive=False`、`0.41s`），而 L8 的结论断言依旧会"过"—— 正好证明补前提之前这条腿会**静默失去意义**。

**验证**：`test_limit_roles_enforcement.py` 全绿（含新 L2 四条与 L8 两条前提）；`check_test_tails`/`check_gate_entries` exit 0；`FALSIFY` = 0；**门禁 88 项**重跑（判定见下）。**本轮无产品代码改动**（1 个 drill + 文档）。

---
## 2ew. 第一百六十五轮（续）：门禁 RED 的**真正原因**是一条**时间炸弹测试**（UTC vs 本地日期）——顺带把"今天是哪天"收成单一所有者

**现象**：本轮跑门禁得到 **`OVERALL=RED`**，唯一非零条目是 `public claims tests exit=1` —— 而**不是**我刚改的 drill（门禁日志证实新腿都跑了：`L2: ...and the refusal is ATTRIBUTED` 1 次、`L8 PREMISE` 2 次，`limit_roles enforcement exit=0`）。
**根因（与我的改动无关）**：`scripts/check_public_claims.test.cjs` 的 `--write refreshes the count and the date` 用例用 `new Date().toISOString().slice(0,10)`（**UTC**）算"今天"，而守卫在第一百二十六轮已经统一为**本地**日期（`localDate()`）。当时本地 `2026-10-01`、UTC 仍是 `2026-09-30` ⇒ 页面被写入**今天（本地）**，而断言要求**昨天（UTC）** ⇒ **在 UTC 以东的时区、每天傍晚之后这条用例必红**。这是一条**时间炸弹**：写完当天绿、第二天开始红，与第一百二十七轮那条"日期旧了不算失败"是同一族问题的另一面。
**修法（单一所有者，而不是再抄一遍）**：
1. `check_public_claims.cjs` 的 CLI 改为 `if (require.main === module) { … }` 并 `module.exports = { localDate, addDays, main }` —— 此前它**在 require 时就执行 `main()`**，所以测试根本无法 import（这正是测试另抄一份日期的原因）；
2. 测试改成 `const { localDate } = require(CHECKER)`，并把**两处**（`:103` 的 IIFE 与 `:278` 的 `p2` 版本）都换成 `localDate(new Date())` ⇒ "今天是哪天"**只有一个实现**（真表里原先有两份：UTC 那份与本地那份）。
**实测**：套件 **27/27**（改前 `not ok 23 - --write refreshes the count and the date in both locales`，输出正是 `（2026-10-01）` vs 期望 `（2026-09-30）`）；`check_public_claims.cjs` 真树行为不变（`CANNOT VERIFY: no measurement…` 与日志模式的 OK 都与改前一致）。
**顺带核对页面没被我改坏**：`--write`（日志模式）对 `docs/index.html` **零改动**（`diff` 空）⇒ 页面仍是 `2026-09-30`/812，守卫按第一百二十七轮的规则把它当 **note**（在 1 天余量内）而不是失败；**没有**借这次机会改对外数字。
**验证**：`check_public_claims` 真树 + 套件 27/27、`check_ci_wiring`、`check_test_tails` 全 exit 0；`FALSIFY` = 0；**门禁重跑 88 项全绿、`OVERALL=GREEN`、`exit 0`**（`public claims`/`public claims tests`/`limit_roles enforcement` 均 exit=0，日志尾部 `OVERALL=GREEN` / `entries=88` / `GATE COMPLETE`）。**本轮无产品代码改动**（1 个守卫的可 require 化 + 1 个测试文件 + drill + 文档）。

---
## 2ex. 第一百六十六轮：`test_cluster_limits.py` 的三条"测了别的东西"——C1 版本、C4 黑洞、C5 恢复

### A. C1「边缘持有**整个**版本」推不出来（三次 POST = 三个版本）

**问题（复审）**：`r-deny`/`r-count`/`r-token` 由**三次独立 POST** 写入，而写路径每次都 `reload_best_effort` ⇒ **三个版本**；把**探针角色写在最前**时，边缘只拿到 v1（仅 `r-deny`）也对 t3 回 429 ⇒ "边缘持有整个版本（含 `r-count`）"**推不出来**。
**修法**：①把**探针角色 `r-deny` 写在最后**（于是"边缘看见了 r-deny"蕴含"它是包含另两条角色的那一代"）；②再给**直接证据**：断言边缘**实际应用**的快照版本 == 这次写入产生的版本（`POST /reload` 的响应体里带 `"version":15`，`hydra_control_snapshot_version` 是节点**应用**快照时记的）。
**★量出来的一个反直觉事实**：该 gauge 在 **leader 上是 0**（它是发布者，不应用自己的快照）而**边缘是 15** —— 我第一版断言"两个节点版本相等"因此失败（`leader=0.0 edge=15.0`）；改成与**写入版本**比较后才成立：`reload version=15.0 edge applied=15.0`。

### B. ★C4 的黑洞腿**从未被触发**（那条"命令超时"的声明一直是空话）

**问题（复审）**：`relay.set_blocked(True)` 是**立刻拒绝并关闭**连接（`return`），节点看到的是一次瞬时错误；而 `HYDRA_REDIS_COMMAND_TIMEOUT_MS` 是为"**接受连接然后沉默**"准备的。于是 `elapsed_cut < 2.5` 这条腿**与命令超时无关**：把超时删掉它也绿。
**修法**：给 relay 加**第二种故障模式** `set_blackhole(True)`（接受连接、**永不回包**，并先切断池里的老连接，使下一条命令真的落在沉默的 socket 上），新增 **C4b** 三条断言：①仍回落 200（fail-open 而不是挂死）；②耗时 **≥0.3s**（拒绝会是 ~0s ⇒ 证明 socket 真的沉默）；③**<3.0s**（有界）。
**证伪（把 5× 的超时喂进去）**：把节点的 `HYDRA_REDIS_COMMAND_TIMEOUT_MS` 设成 **2500** ⇒ 黑洞请求耗时从 **0.50s → 2.50s**，与超时**同比例**变化 ⇒ 这条腿测的确实是命令超时，不是别的东西。
**★顺带发现一个 helper 陷阱并写进注释**：`redis_cmd(sock, …)` **不 SELECT**，用的是该 socket 当前所在的库；窗口在 `REDIS_DB`(3)，所以"直接对共享 socket 发 `DEL <窗口键>`"**什么也删不掉**（返回 `:0`）—— 我第一版 C5 探针就是这样**静默什么都没做**。已在 helper 上写明这条 hazard。

### C. ★C5「恢复」只要求 45 次探针里出现**一次** 429（无法区分"窗口被保留"与"窗口被清空后由探针重新填满"）

**修法**：记录**总线恢复后的第一个响应**，断言它**已经是 429**（切断前窗口里就有 `limit_count` 个样本 ⇒ 恢复后第一个请求就应被拒）；原来那条"某次探针 429"保留为辅助。
**★决定性证伪**：用一条 **SELECT 过的连接**把 `hydra:{rl:r-count:t1}:count` 真的删掉（`DEL → :1`）后重跑 ⇒ **新断言 FAIL**（`first status after the bus returned = 200`），而**旧断言仍然 PASS**（`last status=429 after 4.6s of probing`，正是探针把窗口重新填满）—— 复审的判断被端到端证实，且新断言确实抓住了旧断言抓不到的那一类。

**验证**：drill 全绿（新增 C1 版本断言、C4b 三条、C5 首响应断言）；`check_test_tails`/`check_gate_entries`/`check_ci_wiring` exit 0；`FALSIFY` = 0；**门禁 88 项全绿、`OVERALL=GREEN`、`exit 0`**（`cluster rate limits (shared+FO)` exit=0，且**新腿在门禁里确实跑过**：门禁日志中 `C4b: ...and the COMMAND TIMEOUT` 与 `C5: ...and the FIRST request after recovery` 各出现 1 次）。**本轮无产品代码改动**（1 个 drill + 文档）。

---
## 2ey. 第一百六十七轮：`test_auth_cache_layers.py` 的两条（前提被丢弃 + `KEYS` 把索引键算进结论键）——顺带修掉 `SMEMBERS` 的 RESP 帧

### A. 冷清空的响应被丢弃 ⇒ "清空扇出到**每个**节点"是假设，而不是检查

**问题（复审）**：`tenant_invalidate(A_DATA)` 的返回值**没接**（对比 D 腿接了并断言 `st_inv == 200`）。而紧随其后的 **C 腿**（"第二个节点**不再问** auth 服务 ⇒ 结论来自共享 L2"）**只有在那次清空真的把另一个节点的 L1 也清掉**时才是证据：否则被清空的那个节点用自己的**热 L1** 回答，L2 这条路根本没被走到，而 C 腿照样绿。
**修法**：接住响应体（`InvalidateView.fleet` 就是 `FleetReport`）并断言 **`nodes_applied == nodes_total >= 2`**，作为 C 腿的**前提**；实测 `{'state': 'applied', 'nodes_total': 2, 'nodes_applied': 2, 'lagging': [], 'waited_ms': 501}`。
**证伪**：把期望改成 `nodes_total >= 3`（超过真实节点数）⇒ **恰好这条前提断言红**（证明它是"活的"；真正的机制级证伪需要"一个节点故意不响应事件"，属另一件事，本轮不做并如实标注）。

### B. `KEYS hydra:{auth}:*` 会把**索引键**算进"结论键"

**问题（复审）**：索引集叫 `hydra:{auth:idx}:<tenant>`，它也匹配 `hydra:{auth}:*` ⇒ `cache_keys()` 返回的列表里混着索引键 ⇒ ①`len(keys) >= 1` 无法区分"L2 里真的写了结论"与"只有索引集存在"；②`keys[0]` 恰好是索引（SET）时 `GET` 会返回 **WRONGTYPE** ⇒ 这条腿**因错误原因变红**。
**修法**：`cache_keys(tenant="t1")` 只按 `hydra:{auth}:<tenant>:*` 取、并显式排除索引前缀 ⇒ 返回的每个键都是结论键（实测 `['hydra:{auth}:t1:e28e2ec7…664']`）。

### C. ★顺带：`index_members()` 把 RESP 帧当成成员（"≥1 个成员"被帧满足）

**实测**：`SMEMBERS` 的响应是 `*1\r\n$64\r\n<hash>\r\n`，而该 helper 只滤掉了 `*` 头、没滤 `$<len>` 头 ⇒ **单成员集合被报成两个成员**（`['$64', '<hash>']`）。于是所有"索引 ≥1 个成员"的断言**被帧本身满足**。
**修法**：再滤掉 `$<len>` 行；并把 B 腿的判据从"`len(index_members()) >= 1`"升级为**索引必须正好等于这条结论的 key hash**（租户清空是**走索引**的，索引没指向该键就会漏清）——实测 `index == [key hash]` 完全相等。
**证伪**：把 `$`/数字过滤去掉 ⇒ **恰好**这条新断言红（输出 `index=['$64', 'e28e2ec7…664']`）⇒ 既证明修法承重，也证明**旧判据在"只有帧"时也会通过**。

**验证**：drill 全绿（新增 1 条前提断言、`cache_keys`/`index_members` 修正、B 腿判据升级）；`check_test_tails`/`check_gate_entries`/`check_ci_wiring` exit 0；`FALSIFY` = 0；**门禁 88 项**重跑（判定见下）。**本轮无产品代码改动**（1 个 drill + 文档）。

---
## 2ez. 第一百六十八轮：三条「判据证明的不是它自己说的话」＋一类「文档里的脚本谁都不跑」

### A. 门禁条目守卫的 OK 行：把 29/38 的"信任前序条目"藏起来了

`check_gate_entries.cjs` 的 OK 行原话是「38 entry(ies) run a drill that starts the prebuilt binary,
each **either building it in its own entry or preceded by one that does**」——真话，但读者看不出**有多少条是靠前序条目**。实测 2026-10-01 的真实拆分：**SELF-BUILD 9 / INHERITED 29**（38 条里只有 9 条自己构建）。现在这两个数字**打印在 OK 行里**，并写明 inherited 的含义（"测的是本次运行中**更早的条目**构建的那个二进制"）。**刻意不为这个拆分加 floor**：不变量是"没有条目信任本次运行没构建过的二进制"（规则 3），而拆分是条目排布的属性，加 floor 会在一次合法合并上误红（`MIN_E2E_SPECS` 的教训）——理由写在守卫头部。
**测试 +3**（inheriting ⇒ `SELF-BUILD 0 / INHERITED 1`；self-building ⇒ `1 / 0`；两者都有 ⇒ `1 / 1`），**证伪**：把计数反过来（`if (!selfBuilds)`）⇒ 两条非对称用例同时红，真实 OK 行变成 `SELF-BUILD 29 / INHERITED 9`；还原后 19/19 全过。
**本轮我自己差点造的假**：我第一版探针把 `judged = 5`（"文档化构建前提"那条规则的计数）当成了 self-build 数，于是准备把注释里**正确的**「only 9 build the binary in the same entry」改成 5——**实测两次（9/29 与 35 个读二进制的 drill 中 5 个声明构建前提）之后才发现两者是不同规则的数**。教训与既有规则同形：**注释里的数字也要先量再改**；改对了数字、改错了事实，和说谎没区别。

### B. `test_body_cap_drain.py` T2：旧判据在「客户端中途消失、从未收到任何回复」时也成立

**问题（复审）**：T2 的第一条检查号称「MEASURED — the tenant API answers WITHOUT draining」，判据只有 `written < total`。**`written < total` 在"连接断了、根本没有任何回复"时同样为真** ⇒ 这条检查可以打印"租户面不提前回答"，而它证明的恰好是反面。
**线上证伪（`wc`/RST 探针，`.acceptance/round168-reset-probe.py`）**：起同一个节点，把一个 2 MiB body 只发 6×32 KiB（**低于 1 MiB 上限**，节点此刻无从回答）后销毁连接 ⇒ 记录 `wrote 196608/2097147, reply='', write_error=None`：**旧判据 True / 新判据 False**。
**修法**：判据改为「**真的读到 `413 payload_too_large`** 且 `TENANT_CAP <= written < total`」。下界不是测量而是**代码的推论**：`tenant_api/mod.rs::read_body` 只在 `buf.len() + chunk.len() > MAX_BODY(1 MiB)` 时返回 413 ⇒ 节点**必然已经读掉 1 MiB 以上**，而客户端写出的字节不可能少于节点读到的。**门禁条目实测**：`wrote 1081344/2097147`＝1 MiB＋32 KiB，正好是 `1048576 + chunk(32 KiB)` —— 推论与实测一致。第二条检查另外加上 `write_error is None`（客户端不掉线），与"回复可读"区分开。

### C. `test_usage_drop_accounting.py` D2：「第一个原因」是用 `any(...)` 验的存在性

**问题（复审）**：标签写「at the shipped defaults the **first** reason an operator sees is the bounded channel (`channel_full`)」，判据却是 `any(r == "channel_full" for r, _ in samples1)` —— 在**burst 结束时**的样本里找存在性。于是"先出现 retention_cap、后来才出现 channel_full"的运行同样通过，而它证明的是另一件事。
**修法**：在**第一次 drop 的那一刻**取样，并断言**集合恰好等于** `["channel_full"]`。实测（本机 + 门禁）：600 请求后第一次 drop，样本 `[('channel_full', 88.0)]`，burst 结束时仍是 `[('channel_full', 88.0)]`。
**证伪两条**：①把期望改成 `["retention_cap"]` ⇒ 该断言红、drill `exit 1`（证明断言在跑，不是摆设）；②谓词级探针（`.acceptance/round168-d2-probe.py`）：合成记录（首 drop=`retention_cap`，末尾样本含 `channel_full`）⇒ **旧判据 True / 新判据 False**。**如实标注**：出厂常量下线上造不出这个形状（`retention_cap` 要缓冲区先堆到 10 000 条，`channel_closed` 要通道关闭），所以②是谓词级证伪，线上验证靠 drill 本身。

### D. 一类问题：**文档里的脚本，谁都不跑**（`scripts/e2e-local.sh`、`scripts/load_test.sh`）

**实测 2026-10-01**：`scripts/e2e-local.sh` 与 `scripts/load_test.sh` 在门禁里出现 **0** 次、在 `ci.yml` 里出现 **0** 次 —— 两个**能跑、也确实有用**的产物只被"人手"跑过（前者是浏览器套件；后者的四组判官自检在第 6 轮是**手工提取 + 直接调用**验证的）。计划 §4 把它们当成"本机命令"列着，而"没有任何自动入口跑它们"这件事从未被写下来，也没有任何守卫看得见（规则 1 只管 `*.test.*` / `check_*.cjs`）。
**做了一个必须先做的安全改造**：`scripts/e2e-local.sh` 自己的构建是 `cargo build -p hydra-server --features server`（第 92 行），而门禁第 11 条构建的是 `server,cluster-redis,usage-clickhouse` 且其后的集群 drill 依赖那个二进制 —— **直接把它接进门禁，它可能在门禁中途重链出一个缺 cluster 特性的二进制**，正是**第 143 轮那次整门禁 RED** 的成因。因此新增 `E2E_SKIP_BUILD=1`（跳过新鲜度探测与重建，二进制不存在时明确 `die`；**实测缺二进制 ⇒ exit 1 且给出一句话原因**）。另给 `load_test.sh` 加 `SELFTEST_ONLY=1`（把判官自检提到健康检查**之前**，且不再强制 `HYDRA_ADMIN_TOKEN`；实测 `SELFTEST_ONLY=1` ⇒ exit 0，不带开关且无 token ⇒ 仍 exit 1）。
**接线**：门禁新增 `load harness selftest` 与 `admin-ui e2e (browser, 18180)`（后者用显式端口，因为 `integration/run-crud-local.sh` 默认就是 18080/18081）；`load_test.sh` 的自检另接进 CI `scripts` 作业。**证伪（自检承重）**：把自检里"必须 PASS"的那组输入改成偏斜（900/100）⇒ `SELFTEST_ONLY=1` **exit 1** 并打印 `selfcheck: 3:1 split must PASS, got: FAIL minority share 0.100 …`；还原后 exit 0。
**守卫（把这一类钉住）**：`check_ci_wiring.cjs` 新增**规则 1b** —— `scripts/*.sh`（`*.test.sh` 归规则 1）必须被 `ci.yml` **或**本地门禁**调用**，否则 DRIFT；例外只有"被**已接线**文件当 helper 调用"（`ask_llm.sh <- scripts/ask_llm.test.sh`），且**必须打印**"N reached only as a helper"。**实测**：真实树 3 个这样的脚本、1 个 helper 例外；把门禁路径指向不存在的文件 ⇒ **恰好 `scripts/e2e-local.sh` 报 UNWIRED、exit 1**（即修前状态）。
**这一步我自己先写错了一版**：第一版把"注释里提到"也算接线，于是**这条规则自己的注释**写着 `scripts/e2e-local.sh`，把本该报出来的孤儿洗成了"已接线"——用空门禁验证时才暴露（守卫自己的注释成了它宣称已关闭的洞的证据）。修法是按文件类型去注释后再匹配（shell 的 `#`、JS 的 `//` 与 `/* */`），并把两种形状钉成反向用例（shell 注释、JS 块注释各一条）。测试 **+6**（孤儿 / 门禁控制组 / helper 控制组 / shell 注释 / JS 块注释 / floor），全套 `ALL CI WIRING TESTS PASSED`。

### E. §4 的「绝大多数」变成了数字（顺带两版测量都说谎的记录）

见 §4 正文：44 条命令行中 35 条被门禁调用、33 条被 CI 调用，只剩 3 条不在任何自动入口（其中 2 条是 `.acceptance/` 草稿目录里的脚本 —— **接不进去，因为 `.acceptance/` 不在版本控制里（D-5）**）。测量脚本留在 `.acceptance/round168/sec4-coverage.py`；§4 里同时记下**头两版测量的两种说谎形状**（把行尾注释当命令的一部分；按整行匹配忽略了"同一产物、不同参数"）。

### F. 本轮验证

门禁 **90 条条目**（本轮 +2：`load harness selftest`、`admin-ui e2e (browser, 18180)`）；B 段的新判据在门禁条目 `body cap draining` 里**实测通过**（`wrote 1081344/2097147`）。`check_gate_entries`（含拆分的打印）/`check_test_tails`/`check_ci_wiring`（含新规则 1b）出口 0；`check_ci_wiring.test.cjs` 与 `check_gate_entries.test.cjs` 全过；`findings-disposition` 27/27；`grep -rn FALSIFY crates/ integration/ scripts/` = 0。**本轮无产品 Rust 代码改动**（2 个 drill + 2 个 `scripts/*.sh` + 1 个守卫 + 文档）。

**门禁判定（第一百六十八轮）**：`bash .acceptance/round10-gate.sh` ⇒ **90 条条目全部 `exit=0`**、`OVERALL=GREEN`、`GATE_EXIT=0`，日志尾部 `OVERALL=GREEN` / `entries=90` / `GATE COMPLETE`（日志 `.acceptance/round10-gate.log`，stdout 存 `.acceptance/round168-gate.out`）。**本轮改动确实在门禁内被执行**（逐条可在日志里查）：`load harness selftest` 打印 `SELFTEST ONLY: the SWRR distribution judge passed all four known-input cases`；`admin-ui e2e (browser, 18180)` 打印 `20 passed (29.9s)`；`body cap draining` 打印新判据的 `wrote 1081344/2097147 (need 1048576 <= written < 2097147)`；`usage drop accounting` 打印新判据的 `reasons at the first drop (after 600 request(s))=` 后跟 `['channel_full']`；`ci wiring` / `ci wiring tests` / `gate entries build what they run` / `gate entry checker tests` 均 `exit=0`。`FALSIFY` = **0**；`git status --short` **210 项（未提交）**。


## 2fa. 第一百六十九轮：`ops.md` §13 那句「disabled 行也随快照走」——从「没有观测点」变成**在发布者自己的线上读到**

### A. 缺口是什么：整条 drill 的头部写着这半边**无法观测**，而它其实有一个 HTTP 观测点

`test_replica_fidelity.py` 的头部（第一百六十二轮复审后如实改写）写着：§13 的「disabled 行是否随快照走」这半边 **no live observable from an edge** —— 因为 **edge 没有管理面**。这句话本身没错，但**被声称的东西不是 edge 的行为，而是发布者（leader）的 wire 契约**，而它有一个直接的 HTTP 观测点：leader 上的 **`GET /api/v1/internal/control?since=0`**（cluster token 保护）返回 **`SnapshotWire` 的 JSON**（`crates/hydra-server/src/admin/cluster_api.rs::internal_control` → `ok_json(snapshot)`），里面有**两份**载荷：`snapshot.cfg`（运行时配置，密钥被剥掉）与 `snapshot.fidelity`（保真行集）。

**先量后写（探针 `.acceptance/round169-fidelity-wire-probe.py`，实测 2026-10-01）**：
```
cluster token -> HTTP 200 ; admin token -> HTTP 401 {"code":"unauthorized","message":"invalid cluster token"}
wire_version=3 version=18
fidelity.limit_roles: 5  ['r-masked','r-masked-edge','r-off','r-raw','r-shared']
cfg.limit_roles     : 4  ['r-masked','r-masked-edge','r-raw','r-shared']      ← 少了 disabled 的 r-off
fidelity.key_prefix_bindings: 1 ['b-off'] ; cfg.key_prefix_bindings: 0 []     ← 少了 disabled 的 b-off
fidelity.tenant_token_hashes: 1 ['t1']  (sealed fields: ciphertext/key_version/nonce)
```
⇒ **两份载荷的差集正好就是 §13 那句话里的 disabled 行**。这既证明了修法，也说明了一条腿**不能读错子对象**（读 `cfg` 会得出相反结论）。

### B. 新增腿 F8（6 条断言，全绿；插在 F7 **之前**，理由见下）

①控制面**只认 cluster token**（admin token 与无 token 都 401）；②**disabled 的 `r-off` 在 `fidelity.limit_roles` 里且 `enabled == False`**；③**它不在 `cfg.limit_roles` 里**（两条一起构成"两份载荷差集 = disabled 行"的机制证明）；④**disabled 的 `b-off` 同理**；⑤**租户 token 是密封的**：整份响应体里**不出现明文 token**，且密封记录是三段式（`ciphertext`/`nonce`/`key_version`）；⑥其余保真字段非空（`provider_models` / `tenant_providers` / `sub_tenant_routes`），保证上面那些腿不是读了一份空载荷。
**为什么必须插在 F7 之前**：**F7 会把 `r-off` 改成 enabled**（`PUT /limit-roles/r-off` + reload），插在它后面就没有"disabled 行"可指了 —— 这条顺序依赖已写进注释，否则下一轮有人挪动段落就会静默失去被测对象。
**两条反向证伪（都是机制级）**：
1. **读错子对象**：把 `roles_fid`/`roles_cfg` 的来源对调 ⇒ **恰好** ②③ 两条同时红（`roles=['r-masked','r-masked-edge','r-raw','r-shared']`、`fidelity has r-off=False, cfg has r-off=True`），drill `exit 1` ⇒ 证明这条腿**能区分两份载荷**，读 `cfg` 那版是**假通过**；
2. **把"disabled"这一半去掉**：`enabled is False` 改成 `is True` ⇒ ②④ 两条同时红 ⇒ 证明它断言的是"**disabled** 行也在"，而不只是"有这个 id"。
两次证伪后从**修复后备份**恢复（`diff -q` 一致、`grep -c FALSIFY` = 0），drill 复跑 `REPLICA FIDELITY: PASSED`。

### C. 同时改掉了**我自己前一版过头的悲观结论**（诚实性双向）

头部原来只写「observed by proxy … no live observable」；现在写清**两条**：F8 观测到的是**发布侧 wire**；**仍然不可观测的是"disabled 行在 edge 上的行为"**——因为 disabled 行**根本没有行为**。F7 的注释也被改写（它原先用"no live observable"解释为什么只测"disabled→enabled"这一方向）。`r-off` / `b-off` 两个夹具的定位从"保持载荷形状"变成**证据**。这方向上一次修的是"文档说了比证据多"；这次修的是"文档说了比证据少"——**同一种病的另一面**。

### D. 顺带把另一条队列项**量化并降级**（`check_test_tails` 的 21 个未判文件）

队列里写的是「65 个 suite 只有 44 个被判 ⇒ 那 21 个上没有任何守卫」。本轮**实测把它们分了类**（脚本见下），结论比原话**小得多**：
- **17 个**（如 `integration/test_snapshot_stale.py`）文件里**根本没有 exit 调用** ⇒ 没有"程序终点"，**附加代码照样会跑** ⇒ **不判它们是正确行为，不是缺口**；
- **3 个** node 自建 harness（`check_documented_metrics.test.cjs` / `check_redis_pool.test.cjs` / `check_test_tails.test.cjs`）的 exit 在 **`if (failures.length) { … }`** 里，是**条件出口** ⇒ 附加代码在成功路径上仍会执行 ⇒ 也**正确地不判**；
- **1 个** 真缺口：`integration/test_crud.py` 的 `main()` 在**所有路径**上都 `sys.exit`（`:592` `sys.exit(1)` / `:593` `sys.exit(0)`），而 `__main__` 块是 `main()` 调用形式 ⇒ 块之后追加的代码是**死代码**，而守卫的模型（要求 `sys.exit(` 出现在 `__main__` 块**内部**）看不见它。
**只修这 1 个的话，守卫要新增"一跳"模型（`__main__: main()` 且被调函数在每条路径上都 exit）** —— 而这正是**容易产生假阳性**的地方：如果被调函数在成功路径上 `return`，块之后的代码**会**执行，模型若只看"函数里含 `sys.exit`"就会误报。因此**本轮只量化、不改守卫**，并把假阳性风险写在这里（下一轮要做就得先写"能 return 的计数器用例"当控制组）。

### E. 本轮验证（门禁判定）

`bash .acceptance/round10-gate.sh` ⇒ **90 条条目全部 `exit=0`**、`OVERALL=GREEN`、`GATE_EXIT=0`，日志尾部 `entries=90` / `GATE COMPLETE`（stdout 存 `.acceptance/round169-gate.out`）。**F8 是在门禁内被执行的**（日志可查）：`F8 PREMISE … — GET /limit-roles -> 200 r-off enabled=[False]; GET /provider-key-bindings -> 200 b-off enabled=[False]`、`F8 the leader's published snapshot — HTTP 200 wire_version=3 version=18 fidelity.roles=[…'r-off'…] cfg.roles=[…] fidelity.bindings=['b-off'] cfg.bindings=[]`、`F8: the DISABLED limit role (r-off) IS in the published fidelity rows — PASS`；`replica fidelity exit=0`、`auth cache layers (L1+L2) exit=0`、`cluster rate limits (shared+FO) exit=0`。`check_test_tails` / `check_gate_entries` / `check_ci_wiring` 出口 0；`FALSIFY` = **0**；`git status --short` 210 项（**未提交**）。
**注**：先跑的两次证伪（读错子对象、去掉 disabled 判定）都如期把 ②③ / ②④ 两条打红、drill `exit 1`，之后从**修复后备份**恢复并 `diff -q` 校验一致；F8 的**前提断言**是在这轮门禁运行**之前**加入的，因此它也在这份判定里被执行（`F8 PREMISE … PASS`）。

## 2fb. 第一百七十轮：`check_test_tails` 的**第 4 种程序终点**（`__main__: main()` 且 `main()` 每条路径都 exit）——以及那条注释里的**错误事实**

### A. 缺口：`integration/test_crud.py` 的块后代码**确实**是死代码，而守卫看不见它

上一轮（§2fa.D）把 21 个未判文件量化成「17 个没有 exit + 3 个条件出口 + **1 个真缺口**」。本轮把那 1 个补上：`integration/test_crud.py` 的 `main()`（**实测：第 567–595 行**）**通篇没有 `return`**，最后一句是 `    sys.exit(0)`，而文件末尾是

```python
if __name__ == "__main__":
    main()
```

⇒ 在这个块之后追加的用例**永远不会执行**，而旧模型要求 `sys.exit(` 出现在 `__main__` **块体内部**（`sys.exit(main())` 那条形状），所以这个文件**从未被判过**。
**顺带发现一条错的事实**：`check_test_tails.test.cjs` 里那条既有控制组的注释写着「**`test_crud.py` is written this way**, and code after the block really does run」——**实测这句话是错的**（该文件的 `main()` 不是 `return 0` 的形状）。夹具本身仍是正确的控制组（"能 return 的 main() 之后代码会跑"），但那句"某某文件就是这样写的"必须删掉，否则**下一个人会照它去理解 `test_crud.py`**。已改成：说明该 CONTROL 保的是**假阳性边界**，并写明 `test_crud.py` 的真实形状（含行号）。

### B. 新形状（shape 4）与**保守判据**

`programEnd` 现在认第四种终点：**`__main__` 块调用一个"每条路径都退出"的函数**。判据刻意保守，只在**两个条件同时成立**时成立：
1. 该函数体（按缩进界定）的**最后一条非空语句**是同缩进的 `sys.exit(...)`；
2. 该函数体里**任何位置都没有 `return`**（任何分支上的 `return` 都是一条"正常返回"的路径，会让调用者之后那行**重新变活**）。
两个条件都写成了**可被反向证伪**的东西（见 D），而不是"看起来对"。
**实测（真树）**：判定数 **44 → 45**，多出来的**正好一个**文件：`integration/test_crud.py`（`block calling \`main()\`, whose last statement is \`sys.exit(…)\` and which has no \`return\``）。其余 20 个未判文件**不变**（实测构成：18 个 JS/CJS、2 个 Python；其中**只有 3 个**含 exit，且都在 `if (failures.length) { … process.exit(1); }` 这类**条件出口**里 ⇒ 块后代码仍会执行 ⇒ **不判它们是对的**）。

### C. 顺手把守卫里**三处**过期数字一次扫干净
守卫头部、`MIN_JUDGED` 旁、`pythonMainBlock` 的形态说明**都**写着「44 of 65」（其中一处还是我上一轮刚写的），按"注释里的数字必须先量再改"全部改为 **45 of 65**，并把形状拆分（6 列零 exit + 37 `__main__` 内 exit + **1 形状 4** + 1 IIFE）与"20 个未判"的构成写清。**这次扫的目的是**：改完一个计数后**必须 grep 全仓同形数字**——只在头脑里记住"我改了注释"是不够的（三处里我第一遍只改了两处，第三处是靠 grep 才发现的）。

### D. 反向证伪（两条，都是把新机制的一条前提拆掉）

1. **删掉 `return` 检查** ⇒ 恰好新增的控制组 `CONTROL (shape 4): a \`return\` ANYWHERE in main() …` **变红**（真树仍 exit 0：没有第二个这样的文件）⇒ 证明"任何 `return` 都取消判定"这一条是承重的；
2. **把 `defAlwaysExits` 的收尾判据改成"任何被调函数都算退出"**（`return last + 1;`）⇒ 恰好两条控制组（`no sys.exit` / `EMPTY main()`）**同时变红** ⇒ 证明"最后一句必须是 `sys.exit`"这一条也是承重的。
两次都从**修复后备份**恢复（`diff -q` 一致、`FALSIFY` = 0），测试 **19 passed / 0 failed**（新增 4 条），真树 `check_test_tails` exit 0。

### E. 本轮验证（门禁判定）

`bash .acceptance/round10-gate.sh` ⇒ **90 条条目全部 `exit=0`**、`OVERALL=GREEN`、`GATE_EXIT=0`，日志尾部 `entries=90` / `GATE COMPLETE`（stdout 存 `.acceptance/round170-gate.out`）。**本轮改动在门禁内被执行**（日志可查）：`test tails (no dead cases)` 行的输出正是 **`[test-tails] OK (45 of 65 suite file(s) have a recognized program end …)`** —— 即 44 → 45 的新判定在门禁里生效；`test tail checker tests`（`node --test scripts/check_test_tails.test.cjs`，**19 tests / 0 fail**）在该条目里跑完并 exit=0。`check_gate_entries` / `check_ci_wiring` / `findings-disposition` 出口 0；`FALSIFY` = **0**；`git status --short` 210 项（**未提交**）。

## 2fc. 第一百七十一轮：`test_snapshot_stale.py` 的「写进去但没生效」腿——**断言是"缺席"，而前提只是"PUT 返回 2xx"**

### A. 问题：缺席型断言与"什么都没发生"天然兼容

S1 那条腿的标签是「an admin write during the stale window still answers 2xx」＋「…and has NO runtime effect: the disabled tenant is **STILL SERVED**」，判据是 `st_after == 200`。**"仍然被服务"这个断言，对"改了但没生效"和"根本没改"两种世界同样成立** —— 如果那次 `PUT /tenants/t1` 实际什么都没改（body 被忽略、字段名写错、写路径静默 no-op），这条腿**照样绿**，而它对 staleness 什么都没证明。这正是本会话反复出现的那一类：**判据与它自己声称的东西不是同一个命题**（第 167 轮 `tenant_invalidate` 丢弃响应、第 168 轮 `written < total`、第 169 轮读错子对象，同一家族）。
**修法（读回提交态作为前提）**：在 PUT 之后、`proxied()` 之前加一条**前提断言**：通过 **admin API** 读回租户行，要求 `"enabled":false` **已提交**，且同一响应体里 `"snapshot_stale":true`（admin 读的是**数据库行**而不是运行中的快照 —— 这恰好是它在这里能做证人的原因）。实测输出：
```
PASS S1 PREMISE: … — GET /tenants/t1 -> HTTP 200 enabled=false present=True snapshot_stale=true present=True;
     … "enabled":false,"created_at":"…","updated_at":"…","has_access_token":false,"snapshot_stale":true}
PASS S1: …and has NO runtime effect: the tenant whose row now says `enabled=false` is STILL SERVED
```
标签也据此改写（从「the disabled tenant」改成「**the tenant whose row now says `enabled=false`**」），把前提与结论绑在一句里。

### B. 反向证伪（一次运行同时证明"旧判据是假通过"与"新前提承重"）

把 PUT 的 body 改成 `"enabled": True`（即**什么都不改**）后跑同一条 drill：
```
FAIL S1 PREMISE: … — GET /tenants/t1 -> HTTP 200 enabled=false present=False …
PASS S1: an admin write during the stale window still answers 2xx      ← 旧判据照样通过
PASS S1: …and has NO runtime effect: … is STILL SERVED                ← 旧判据照样通过（假通过，实锤）
FAIL S2: …and the write that had no effect now DOES take effect: the disabled tenant is refused
```
⇒ ①**旧的那条腿在"根本没改"的世界里通过**（假通过被实测复现）；②新前提断言**红**，于是失败信息第一次**指向真因**（"那次写根本没提交"），而不是只在 S2 报一个下游症状。恢复方式与前几轮一致：从**修复后备份**恢复、`diff -q` 一致、`FALSIFY` = 0，复跑 `SNAPSHOT STALE: PASSED`。

### C. 顺带做的**全仓清点**：drill 里"丢弃返回值"的调用共 **85 处 / 31 个文件**（实测分类）

| 类别 | 数量 | 备注 |
|---|---|---|
| `POST /reload`（配置重载） | **53** | 作为 setup；**本次未逐条审计**（见下） |
| `redis_cmd`（SELECT/FLUSHDB 等 socket 准备） | 15 | 失败会直接抛异常，不是静默 |
| admin **写**调用（POST/PUT/DELETE） | 9 | 例：`test_limit_roles_enforcement.py` 的两处 `DELETE`（L2 控制腿）——失败会让后续腿看到 429 而不是 200，属**失败可见** |
| `proxied()`/其他流量 | 6 | 多为"制造一次流量"的显式行为 |
| admin `GET`（多在 f-string 里） | 2 | 只用于打印 |

**为什么这次只修 1 处、而不是 85 处**：绝大多数丢弃是"setup 失败 → 后续腿看到 404/401/200 而**判据要求 429/403**"的**失败可见**形状（重载没生效 ⇒ 配置为空 ⇒ 腿红，不会绿）。真正危险的是**断言为"缺席/未生效"的那些腿**——只有它们能在"setup 静默没做"时**变绿**。本轮按这条判据逐个看了同族的三处（`test_snapshot_stale.py` 的 trap 腿、`test_sdk_live.py` 的「still served = cached」腿、`test_limit_roles_enforcement.py` 的 L2 控制腿）：后两处**已有下游正向断言兜底**（缓存腿后面那条"失效后必须 401"只有在翻转真的生效时才可能通过；L2 控制腿在角色没删掉时会看到 429）⇒ 只有第一处是真缺口。**余下的 53 处重载丢弃未逐条审计**，这一点如实记在此处，并进入队列（见 §2ez.G）。

### D. 本轮验证（门禁判定）

`bash .acceptance/round10-gate.sh` ⇒ **90 条条目全部 `exit=0`**、`OVERALL=GREEN`、`GATE_EXIT=0`（`entries=90` / `GATE COMPLETE`；stdout 存 `.acceptance/round171-gate.out`）。**新前提在门禁内执行并通过**（日志可查）：`snapshot stale` 条目里 `PASS S1 PREMISE: the PUT really COMMITTED \`enabled=false\` … — GET /tenants/t1 -> HTTP 200 enabled=false present=True snapshot_stale=true present=True`；该条目 `exit=0`。`check_test_tails` / `check_gate_entries` / `check_ci_wiring` / `findings-disposition` 出口 0；`FALSIFY` = **0**；`git status --short` 210 项（**未提交**）。

## 2fd. 第一百七十二轮：`test_model_catalog.py` 的 C6 子租户那一半**根本无法失败** —— 重新构造出能分辨的夹具（C6b）

### A. 问题（比"前提未验"更重）：那两条腿的断言对**任何**包含 m1/m3 的目录都成立

C6 子租户那半的判据是 `"m1" in ids_sub`（"routed model is kept (narrowed to its route's provider)"）与 `"m3" in ids_sub`（"a model with NO route is not restricted"）。而**匿名目录本身就是 `['m1','m3']`** ⇒ 这两条 `in` 断言对**匿名目录本身**同样成立，也就是**在那两条腿的整个生命周期里，无论子租户/路由是否存在、是否进了快照，它们都不可能失败**。换句话说：标签写着"收窄到路由的 provider"，判据却连"有子租户"都证明不了 —— 这是本会话那条"判据证明的不是它自己说的话"的最严重一档（**恒真**）。
**顺带量到两件事实**（都写进了 drill 标签）：
* **未知前缀的 key 也被服务**（`NOPE_abc123` → 200，列表与匿名完全相同）：与 `design.md:632`「**带不带 api-key 均可读**、出示的 key **仅按前缀绑定收窄目录**」一致 —— 端点**不从 key 认租户**，所以"子租户 key 的目录"与"任意 key 的目录"在**只有 operator binding 能收窄**这件事上等价。这条现在是 C6 的**控制组**（没有它，"路由收窄了"与"谁都看得见全部"无法区分）。
* **写边界是 fail-closed 的**：把 `m3` 路由到一个**不服务它的 provider** ⇒ `400 model_not_served_by_provider`（实测）⇒ **最直观的那个"空交集"夹具根本写不进去**。

### B. 修法：保留前提、补控制组，并**构造出能分辨的夹具 C6b**

* **前提断言**（子租户与它的路由真的存在）：`GET /sub-tenant-routes` **读回** `str1`，而不是从 201 推断（operator 那半早就有同款前提检查，子租户那半没有）；
* **控制组**：未知前缀 key 必须看到**完整匿名目录**；
* **非分辨态如实标注**：只有 `m1` 被路由时子租户目录 == 匿名目录，这是**文档化的"无命中 ⇒ 行为与今天完全一致"**（不是白名单）——但它**单独不能**证明门在工作，注释里写明，由 C6b 补上分辨力；
* **C6b（关键）**：`router::accessible_models` 只在"子租户门把候选集**掏空**"时丢掉模型，而直接写这种路由会被拒（A 段实测 400）。于是**按生产里真实发生的路径构造**：给 `m3` **加第二个 provider**（`pm3b` → p1，201），再把子租户路由 `str2` 钉到**另一个** provider（p2，201 —— **可以**钉，因为 p2 在**配置**里确实服务 m3，breaker 是**运行态**），而 p2 此时**已因 C4 被熔断**。结果：
```
C6b … provider-model=201 route=201 sub=['m1'] unknown=['m1', 'm3'] tenant=['m1', 'm3']
    call m3 with the sub-tenant key -> HTTP 503 '{"message":"no_available_provider"}'
```
⇒ 同一时刻、同一模型：**子租户 key 丢掉 m3**（provider 集被掏空），**匿名与未知前缀 key 仍通过 p1 看到 m3**。这条断言**可以失败**，且它直接验证 `design.md` §7.1c 承诺的"`accessible_models` **目录镜像**"。

### C. 反向证伪（改**产品代码**、重建、跑同一条 drill）

把 `router.rs` 里 (3.6) 的目录镜像关掉（`if false && operator_binding.is_none() {`），**重建**后跑 drill：
```
FAIL  C6b: the mirror is REAL …  — sub=['m1', 'm3'] unknown=['m1', 'm3'] tenant=['m1', 'm3']
MODEL CATALOG: FAILED (1)   (drill exit 1)
```
⇒ **恰好这一条**红（`sub` 保住了 m3），而"调用路径 503"那条仍绿 —— 与机制一致：503 来自 `resolve`，目录丢弃来自 `accessible_models` 的镜像，两者是**不同的代码路径**，一条腿红了另一条不红正好把它们分开。恢复：从**原文件备份**还原、`diff -q` 一致、`grep -c "false && operator_binding"` = 0，**重建**并复跑 ⇒ `sub=['m1']`、`MODEL CATALOG: PASSED`。

### D. 本轮改动与验证

drill 头部把 C6 的描述从「a presented sub-tenant-prefixed key NARROWS the listing」改成**实测口径**（operator binding 才是精确收窄；子租户前缀只在**有路由覆盖**时收窄；写边界拒绝"provider 不服务该模型"的路由），并新增 C6b 一条。守卫：`check_test_tails` / `check_gate_entries` / `check_ci_wiring` 出口 0；`FALSIFY` = **0**。**产品代码未变**（临时证伪已还原并重建）。

### E. 顺带把上一轮开的"丢弃重载"队列**收口（负结论，带方法）**

上一轮留下的问题是「53 处 `POST /reload` 丢弃未逐条审计」。本轮用机械筛法把它做成**可复核的分类**：对每个 drill 里丢弃的 reload，向后 40 行取**紧随其后的 `check` 标签与其判据表达式的头三行**，按判据形状分类（期望 200/2xx/429 = **正向**；期望 4xx/0/`is False`/`not in`/`== []` = **缺席**）。实测 **38 组**：**22 组正向**、13 组其它（多数是**已经**加了前提断言的腿，如 `test_limit_roles_enforcement.py` 的 `L8 PREMISE`）、**3 组**被判成"缺席"：
* `test_auth_hop_failmode.py` A1 / A3（"unreachable auth service gives the documented **503**"）—— **实测上是失败可见的**：这两条腿的**前提就是"重载把租户的 auth_url 指向了死端口"**，若重载静默失败，租户仍指向**可用**的上游 ⇒ 请求返回 200 ⇒ 断言（503）**红**。也就是说"缺席型断言"在这里并未与"setup 没做"兼容；
* `test_sub_tenant_steering.py` S5（control）—— 被正则误判（它是控制组，断言 `set(tags) == {"A","B"}`，属正向）。
⇒ **结论：38 组里唯一真正"断言与 setup 兼容"的就是本轮修掉的 C6 子租户那一半**（而且它不是 C6b 那种"前提未验"，是**恒真**）。方法论与计数都写在这里，便于后人复核或推翻（判据形状的启发式，不是语义分析）。

### F. 本轮验证（门禁判定）

`bash .acceptance/round10-gate.sh` ⇒ **90 条条目全部 `exit=0`**、`OVERALL=GREEN`、`GATE_EXIT=0`（`entries=90` / `GATE COMPLETE`；stdout 存 `.acceptance/round172-gate.out`）。**C6b 在门禁内被执行并通过**（日志可查）：`C6b (premise): both writes were accepted … provider-model -> 201 … route -> 201`、`C6b: the mirror is REAL … — sub=['m1'] unknown=['m1', 'm3'] tenant=['m1', 'm3']`、`C6b: ...and the CALL path … HTTP 503 no_available_provider`；`model catalog (/v1/models) exit=0`。`check_test_tails` / `check_gate_entries` / `check_ci_wiring` / `findings-disposition` 出口 0；`FALSIFY` = **0**（含产品代码的临时反转已还原并 `diff -q` 校验、重建）；`git status --short` 210 项（**未提交**）。

## 2fe. 第一百七十三轮：两条「标签比证据强」的腿 —— `N4` 用 200 ms 里的一次采样说了整段 15 s 排空期，`C2` 只证明"这条序列存在"

### A. 机械筛选（本轮方法）与它筛出的候选

把「**标签里有强断言词**（exactly/always/never/only/every/identical/narrowed/none/zero…）**而判据里没有任何比较或集合操作**（`==`/`!=`/`all(`/`set(`/`sorted(`/`len(`）」做成一枚扫描器（`.acceptance/round173-label-sweep.py`，**它明说自己是启发式**）：全仓 **29 处**命中，收窄到"完整性/精确性"词后剩 **24 处**；逐条读过之后，**其余 22 处都是合理的**（如 `max_depth <= 3`、`set(tags) == {"A","B"}`、时间窗上下界、`not snap_missing` 这类由穷举比较算出的布尔）。真正站不住的是下面两条。

### B. `test_shutdown_drain.py` N4：**一次采样**支撑"整段排空期没有任何探测/抓取"

**问题**：探测循环写成"四个探测全被拒 ⇒ `break`"，而实测**在 t=0.2s 就满足了** ⇒ `probe_trace` 只有 **2 个样本**（`[(0.0, 404, 0, 0), (0.2, 0, 0, 0)]`），而进程随后又活了约 **15 s**。N4 的标签却是「during the drain there is no probe and no scrape at all」「refuses **every** connection」——**关于整段窗口的断言，证据是窗口前 200 ms 里的一次采样**。更要紧的是**它挡不住真正的回归方向**：如果管理面监听器在排空中途又被拉起来，这条腿**看不见**。
**修法**：（1）原有早退循环**保留不动**（`alive_right_after` 的语义依赖它），在其**之后**新增一个"探到进程消失为止"的阶段（上限 40 s，0.2 s 一次），样本并入同一份 trace；（2）新增 **N4 PREMISE**：样本数 `>= 5`、且**最后一个样本落在退出前 1 s 内**、退出跨度 `> 1 s` ⇒ "没有回应"才是关于**整段窗口**的陈述。
**实测（修复后）**：`samples at t>=0.2s: 75; last sample at 14.9s; exit at 15.1s`（trace 覆盖 0.0→14.9 s），两条拒绝断言仍全绿。
**反向证伪（用旧代码路径）**：把新阶段改成 `while False and …` ⇒ **恰好 N4 PREMISE 红**：`samples at t>=0.2s: 1; last sample at 0.2s; exit at 15.1s`，而**拒绝断言照样绿** —— 这正是旧版的缺口被量化出来的样子。

### C. `test_client_disconnect.py` C2：标签说"**被计数了**"，判据只是"这条序列出现了"

**问题**：判据 `any('provider="p1"' in l for l in mid)` 只证明 `hydra_mid_stream_errors_total{provider="p1"}` **这一行存在**，既不检查数值，也无法把增量**归因于**这次 abort（同一逻辑在本会话已经修过一次：`test_admission_queue.py` 的 `not l.endswith(" 0")`）。若将来有别的腿先把它推到非零，这条断言会**静默变成同义反复**。
**修法**：新增 `mid_stream_count()`（序列不存在记 **0.0** —— 与实测一致：首次 abort 前它根本不出现；能解析失败时返回 **None**，绝不悄悄当 0），在 C1 的 abort **之前**取快照，判据改成 **`mid_before is not None and mid_after is not None and mid_after > mid_before`**。实测：`counter 0.0 before the abort -> 1.0 after`。
**反向证伪**：把"before"快照挪到**读取之前那一刻**（即 `before == after == 1`）⇒ **该条红**（`counter 1.0 before the abort -> 1.0 after`），而**旧的"存在性"判据在同一个输出下是 True**（`['…{provider="p1"} 1']` 就在 detail 里）⇒ 假通过被如实复现。

### D. ★本轮我自己的一次自伤（第三次同型，已记录）

做 C2 的证伪时我敲了 `cp .acceptance/round173-bak/test_client_disconnect.PRE-FIX.py integration/...` —— **把 PRE-FIX 备份盖回了已修好的文件**，于是 `mid_stream_count` 这个**修复的一部分**一起被撤回，drill 直接 `NameError` 崩掉。这正是本会话已经犯过两次的错（"备份必须在修复**之后**取，恢复前先 `diff`"）：**这次不是守卫先发现，而是运行时崩了**。补救：重新施加全部三处修改、跑绿、再取 **POST-FIX** 备份（`grep -c 'mid_before is not None and mid_after'` = 1 校验），**之后**才做真证伪，最后从 POST-FIX 备份恢复并 `diff -q` 确认。

### E. 本轮验证（门禁判定）

`bash .acceptance/round10-gate.sh` ⇒ **90 条条目全部 `exit=0`**、`OVERALL=GREEN`、`GATE_EXIT=0`（`entries=90` / `GATE COMPLETE`；stdout 存 `.acceptance/round173-gate.out`）。**两条改动都在门禁内执行并通过**（日志可查）：`N4 PREMISE … — samples at t>=0.2s: 75; last sample at 14.9s; exit at 15.1s`、`C2: a client-caused mid-stream abort IS counted … — counter 0.0 before the abort -> 1.0 after`；`shutdown drain exit=0`、`client disconnect exit=0`。`check_test_tails` / `check_gate_entries` / `check_ci_wiring` / `findings-disposition` 出口 0；`FALSIFY` = **0**（两处证伪均已从 POST-FIX 备份恢复并 `diff -q` 校验）；`git status --short` 210 项（**未提交**）。

## 2ff. 第一百七十四轮：换维度继续筛「判据只是"存在"、标签说"被计数"」——`test_admission_queue` Q1 与 `test_limit_roles_enforcement` L5

### A. 机械筛选（第二种判据形状）与结果

按上一轮记下的方向换维度：**标签含 counted / measured / attributed / increment / recorded / metered** 而**判据只是"存在"**（`any(` / `in` / `is not None`，且无可比较/可计数的算子）。扫描器 `.acceptance/round174-any-sweep.py` 在真树命中 **6 处**；逐条读过后 **4 处合理**：
* `test_admission_queue.py:396`（Q4）**已经**是强形式（`not l.endswith(" 0")`）；
* `test_breaker_lifecycle.py:260` 的标签本身就是**"还没有计数"**（缺席是正确形状）；
* `test_crud.py:490` 是"响应里有这个字段"（字段存在即断言内容）；`test_master_key_sources.py:296` 是启动失败用例。
⇒ **2 处真缺口**，都是"`any('…' in l …)` 当'被计数了'"：**Q1** 与 **L5**。

### B. 两处修法（与第 173 轮 C2 同款：改成**可归因的增量**）

两处都新增同一个 4 行 helper `counter_value(lines, needle)`（**序列不存在记 0.0** —— Prometheus 计数器首次使用才出现，缺席即零是诚实读法；**行在但值解析不出来返回 None**，绝不悄悄当 0），并在**触发之前**取快照：
* **`test_admission_queue.py` Q1**（`hydra_queue_drops_total{reason="full"}`）：判据由"这一行存在"改成 **`full_after > full_before`**；并**顺带补一条"账要对得上"的断言** —— `(full 增量 + timeout 增量) >= 本轮的 503 数`（实测 **4 shed = full +3、timeout +1**，等式成立；用 `>=` 是因为还有本 drill 没驱动的第三种文档化原因）。
* **`test_limit_roles_enforcement.py` L5**（`hydra_limit_rejected_total{dim="tokens",role="r-token"}`）：保留"标签值确实是 `tokens`"这一半（那是这条腿的原意：写 `dim="token"` 永不触发），并加上 **`r-token` 序列的增量** ⇒ 标签值与计数**同时**被钉住。
**实测**：Q1 `full 0.0 -> 3.0`、`4 shed; full +3.0, timeout +1.0`；L5 `r-token series 0.0 -> 1.0`（且 `dim="tokens"` 在场）。两条 drill 全绿。

### C. 反向证伪（两条，都把"之前"快照挪到**触发之后**）

* Q1：把 before 快照移到"读之后"那一刻 ⇒ **两条新断言同时红**：`full 3.0 -> 3.0`、`4 shed; full +0.0, timeout +0.0`，**而旧的"存在性"判据在同一输出里是 True**（detail 里就印着 `['…reason="full"} 3', '…reason="timeout"} 1']`）⇒ 假通过被如实复现；
* L5：同样处理 ⇒ **该条红**（`r-token series 1.0 -> 1.0`），旧判据同样为 True。
两次都**先从 POST-FIX 备份恢复**（本轮吸取上一轮的教训：**先取 POST-FIX 备份并 `grep -c counter_value` 校验**，再动证伪），最后 `diff -q` 确认一致、`FALSIFY` = 0，并把两条 drill 复跑到全绿。

### D. 本轮验证（门禁判定）

`bash .acceptance/round10-gate.sh` ⇒ **90 条条目全部 `exit=0`**、`OVERALL=GREEN`、`GATE_EXIT=0`（`entries=90` / `GATE COMPLETE`；stdout 存 `.acceptance/round174-gate.out`）。**两条改动都在门禁内执行并通过**（日志可查）：`Q1: hydra_queue_drops_total{"reason="full"} was counted … — full 0.0 -> 3.0`、`Q1: ...and the increments account for every shed request … — 4 shed; full +3.0, timeout +1.0`、`L5: ...and it is counted … — r-token series 0.0 -> 1.0`；`admission queue exit=0`、`limit_roles enforcement exit=0`。守卫出口 0；`FALSIFY` = **0**；`git status --short` 210 项（**未提交**）。

## 2fg. 第一百七十五轮：`check_test_tails` 的**第 5 种终点**（`__main__: unittest.main()`）——以及两轮机械筛选的**负结论**

### A. 先做了两个维度的机械筛选，都**基本是噪音**（如实记录）

* **维度 (b)「前置断言只读状态码」**（`.acceptance/round175-setup-sweep.py`：`must(...)`/`if st not in (200, 201)` 之后 30 行内无 `GET`/`sql(`/`metric(` 读回）：命中 **38/46**，但逐条看下来**绝大多数是 `seed()` 循环里的写入**（`raise SystemExit` 那类），它们的后续腿本来就是**行为性**验证，不需要读回；真正值得看的只有 `test_sub_tenant_steering.py` 的 6 处 `must(...)`，而其中"缺席型"的那条（S5：**停用的**子租户不转向）**前提已由 `must`（201）与 S2 的正向腿兜底**（第 172 轮 C6 那种"恒真"不成立）。⇒ 该维度**没有新缺口**，规则需要"`must` 之后紧跟缺席型 check"这类双条件才不至于淹没在噪音里。
* **维度 (a)「标签谈时间/顺序、判据只比最终状态」**（`.acceptance/round175-order-sweep.py`）：命中 **53 处**，同样基本是噪音 —— 因为 `still` 一词把大量"动作之后仍然 200"的**正常最终态断言**拉了进来。逐个读过 20 余条，未发现第 168 轮 D2 那种"用 `any` 验第一个"的同类（T3「先排空再回答」有 `written == total` 兜底；`test_auth_cache_layers.py` A 腿用的是**逐请求计数**；Q4 的 `reported == 2` 对"每次重读"确实能分辨）。
**结论**：这两条机械线到此为止（三个维度合计：29 + 6 + 38 + 53 处命中，真缺口 2 + 2 处，已全部修掉），**没有发现新的假 PASS**。方法论与产物脚本都留在 `.acceptance/`，便于复核或推翻。

### B. 换个落点：`check_test_tails` 少认了**两种真实存在的程序终点**（本轮修）

做筛选时顺手核对"还有哪些 suite 未被判"，发现 **2 个 Python suite 仍未判**：`integration/test_error_contract.py` 与 `tools/hydra-py/tests/test_client.py` —— 两者的末尾都是
```python
if __name__ == "__main__":
    unittest.main(verbosity=2)     # 另一个是 unittest.main()
```
而 **`unittest.main` 默认 `exit=True`**（⇒ `sys.exit(not result.wasSuccessful())`）⇒ **块之后追加的用例永远不会执行**。第 170 轮加的"形状 4"只认**本文件内定义**的、每条路径都 exit 的函数，所以这两个文件一直落在规则之外 —— **同一类缺口，只是被调函数来自标准库**。
**形状 5 的判据（含刻意的边界）**：
* 命中：`__main__` 块里调用 `unittest.main(...)`，**且没有 `exit=False`**（这是 unittest 文档化的"不退出"开关）；
* **刻意不**命中 `pytest.main(...)`：它只**返回**退出码，不抛 `SystemExit`（除非调用者自己包 `sys.exit(pytest.main(...))` —— 那种写法形状 2 已经认了）。
**实测**：判定数 **45 → 47 of 65**（多出来的正是那两个文件：`block calling \`unittest.main()\`, which exits by default`），真树仍 exit 0（两者块后无代码）；守卫里**三处**过期数字（45 of 65）一次扫干净，形状拆分更新为 **6 列零 exit + 37 `__main__` 内 exit + 1 形状 4 + 2 形状 5 + 1 IIFE = 47**。
**测试 +3**（形状 5 命中；控制组 `unittest.main(exit=False)` 不判；控制组 `pytest.main([...])` 不判），**22 passed / 0 failed**。
**两条反向证伪**：①把识别改成 `unittestZZ.main(` ⇒ **恰好形状 5 那条红**，真树掉回 **45 of 65**（＝本轮之前的状态）；②去掉 `exit=False` 的豁免 ⇒ **恰好该控制组红** ⇒ 两条边界都是承重的。

### C. 本轮验证（门禁判定）

`bash .acceptance/round10-gate.sh` ⇒ **90 条条目全部 `exit=0`**、`OVERALL=GREEN`、`GATE_EXIT=0`（`entries=90` / `GATE COMPLETE`；stdout 存 `.acceptance/round175-gate.out`）。**本轮改动在门禁内生效**（日志可查）：`test tails (no dead cases)` 行的输出正是 **`[test-tails] OK (47 of 65 suite file(s) …)`**（即 45 → 47 的新判定在门禁内成立），`test tail checker tests` 条目内 `node --test scripts/check_test_tails.test.cjs` = **22 tests / 0 fail**。`check_gate_entries` / `check_ci_wiring` / `findings-disposition` 出口 0；`FALSIFY` = **0**；`git status --short` 210 项（**未提交**）。

## 2fh. 第一百七十六轮：★**我自己的新检查抓出了一个共享 helper 的 bug** —— `maskJsLiterals` 把**正则里的反引号**当成模板字面量开头，**一份真实守卫 suite 的 74% 被抹掉**

### A. 起因：本轮先加的"未判但有 exit ⇒ 必须记录"规则，立刻把**我写的那条记录**判成过期

本轮前半段给 `check_test_tails` 加了一条义务：**未被判定的文件若含 `process.exit(`/`sys.exit(`，要么建模、要么记录在 `UNJUDGED_WITH_EXIT_OK` 里并写明理由；记录不再适用也算 DRIFT**（缘由：第 170/175 轮各有一个这样的文件是**靠人眼**发现的，而 OK 行只报了一个数字）。加完后，真实树立刻报：
```
UNJUDGED_WITH_EXIT_OK records scripts/check_redis_pool.test.cjs, but that no longer applies
```
但那个文件**确实**含 `process.exit(1);`（第 217 行，在 `if (failures.length) {` 里）—— 于是追进去，发现**不是记录写错，而是守卫"看不见"那个文件**。

### B. 根因：`scripts/js_blank.cjs` 的正则字面量处理（共享 helper，两个守卫在用）

`scripts/check_redis_pool.test.cjs:190`：
```js
  r.code === 1 && /\.build_pool\(` outside the single owner/.test(r.out),
```
**正则字面量里有一个反引号**（JS 合法）。掩码器把它当成**模板字面量开头**，而模板可以跨行 ⇒ **再也没闭合** ⇒ 实测后果三条：
1. 该文件 **8231 → 2167 个非空白字符**（**74% 的文件内容被抹掉**）；
2. `maskJsLiteralsReport` 对一个**完全合法**的文件报 **`unterminated: true`** —— 而 `check_e2e_contracts.cjs` 会把这个标志当成"**该文件无法作为 JS 解析**"；
3. `check_test_tails` **根本看不到该文件的 `process.exit(`** ⇒ 它被算成"未判且无 exit"，于是我的记录显得"过期"。**这比没有守卫更坏**：守卫照常打印 OK。
（掩护：`js_blank.cjs` 的注释作者早就知道"正则里可能有引号"—— 例如 `/don't/` —— 但他只为**非模板**引号做了"不跨行 ⇒ 损失只限一行"的处理；**反引号是跨行的**，损失无界，这条推论当时没做。）

### C. 修法：掩码器**跳过**正则字面量（不抹其内容）

新增 `regexCanStartHere()`（保守启发式：前一个非空白字符不能结束表达式 ⇒ 可能是正则；**关键字** `return/typeof/case/in/of/do/else/yield/await/…` 也算）与 `endOfRegex()`（转义、字符类 `[…]`、**不跨行**：找不到闭合就当除法），命中则 `i = 闭括号+1` **跳过而不抹除** —— 调用方（如 `check_e2e_contracts`）要从正则里读模式，抹内容会改变它们的语义，跳过即可消除失步。
**实测（修复后）**：`check_redis_pool.test.cjs` 的 `unterminated` **true → false**，第 217 行 `process.exit(1);` **在掩码文本里可见**，`check_test_tails` 因此**看得到**它、并认出那条记录**有效**（OK 行新增 `2 recorded exception(s) of the kind that hid rounds 170/175`）。
**新测试文件** `scripts/js_blank.test.cjs`（**8 条**，接进 CI `scripts` 作业与本机门禁 ⇒ 门禁 **90 → 91 条**）：①正则里的反引号不再吞掉后续代码；②**实测用例钉死**（拿真实 `check_redis_pool.test.cjs`：`unterminated === false`、`process.exit(1);` 可见、非空白字符 floor > 1500）；③控制组：除法 `a / b / c` 不被当正则；④控制组：`return /…/` 关键字位置；⑤控制组：正则**字符类**里的 `/` 不提前结束；⑥控制组：**未闭合模板仍报 `unterminated: true`**（修法不能把警报一起关掉）；⑦控制组：含 `/` 的**字符串**仍按字符串抹内容；⑧**全树钉子**：扫**全部 63 个 JS/TS 文件**，断言**没有任何文件**被误读成 `unterminated`（修复前正是这一条会报出 `check_redis_pool.test.cjs`）。**类别清剿（实测）**：修复前全树 **1 个**文件报 `unterminated`（就是这个），修复后 **0 个** —— 即这个失步在同一时刻只影响了一份文件。

### E. 本轮验证（门禁判定）

`bash .acceptance/round10-gate.sh` ⇒ **91 条条目全部 `exit=0`**（本轮 **+1**：`js masker (regex desync)`）、`OVERALL=GREEN`、`GATE_EXIT=0`（`entries=91` / `GATE COMPLETE`；stdout 存 `.acceptance/round176-gate.out`）。**本轮改动在门禁内执行并通过**（日志可查）：`js masker (regex desync)` 条目内 `node --test scripts/js_blank.test.cjs` = **8 tests / 0 fail**；`test tails (no dead cases)` 的输出含 `[test-tails] OK (47 of 66 suite file(s) … ; 2 recorded exception(s) of the kind that hid rounds 170/175)`；`test tail checker tests` = 26 tests / 0 fail（本轮 +4）。`check_gate_entries`（91 条）/ `check_ci_wiring` / `findings-disposition` 出口 0；`FALSIFY` = **0**（掩码器与守卫的临时反转均已从 POST-FIX 备份恢复并 `diff -q` 逐一校验）；`git status --short` 211 项（**未提交**）。
**反向证伪**：把跳过改成 `if (false && …)` ⇒ **`js_blank` 4 条红**（含"实测用例"），**且 `check_test_tails` 立刻把 `check_redis_pool.test.cjs` 那条记录报成过期、exit 1** —— 即 bug 与"记录过期检查"两头都被实测到；恢复后 `diff -q` 三文件全等、`FALSIFY` = 0、两套测试 **7/7 与 26/26** 全过。

### D. 顺带：新义务本身的测试与夹具

`check_test_tails` 的新规则需要夹具可以**替换**记录表（否则每个 fixture 都会因"仓库里那两条记录用不上"而红 —— 实测 15 条测试同时红）。加 `CTT_UNJUDGED_WITH_EXIT_OK`（**替换**而非合并，明文写在守卫里），测试骨架默认设为 `{}`，并按"每条 floor/记录都有自己的用例"的既有纪律补 **4 条**：未记录⇒DRIFT、已记录⇒OK 且打印、**记录过期⇒DRIFT**、无 exit 的未判文件⇒不报。另外三条**既有控制组**（缩进条件 exit、可达 `return`、未被调用的 `main()`）现在必须**写明记录**——它们的断言本来说的就是"这个 exit 是条件的/不可达的"，现在以机器可读的形式写下来了。

## 2fi. 第一百七十七轮：把上一轮的方法用在**另一个共享 blanker** 上 —— `rust_blank.cjs` 有 **7 个守卫**依赖它，却**没有自己的测试文件**

### A. 先量：这个 helper 现在是**对的**，而且它的不变量可以全树检验

`scripts/rust_blank.cjs`（249 行）被 **7 个守卫**使用（`check_alert_expressions` / `check_ci_wiring` / `check_documented_defaults` / `check_documented_env` / `check_documented_metrics` / `check_source_purity` / `check_tenant_error_codes`），而**直接覆盖它的只有一个附带用例**（`check_source_purity.test.cjs` 里那条关于字符字面量的注释/断言）—— 它自己的头部记录了**三个分别实测过的 bug**（`'{'` 字符字面量把 `stripTestItems` 的花括号配对带偏、`#[cfg(test)]` 里的 `register_int_counter!` 被当成"活着"、嵌套块注释）。**一个决定守卫能看见什么文本的共享 helper，没有专属测试，正是"静默过度抹除 ⇒ 守卫变橡皮图章"的温床。**
**全树实测（119 个 `crates/**/*.rs`）**：`stripComments` **零**长度偏移（填充契约成立）；`stripTestItems(stripComments(…))` 花括号**零**失衡；**零**文件被抹成空。
**★我自己的第一版探针说谎了**：我先用 `stripCommentsAndTestItems` 的输出数花括号，得到 **7 个文件"失衡"** —— 但那正是**有意保留字符串内容**的函数（指标守卫要从中读名字），字面量里的 `{}` 当然会被计入；换成正确的组合（`stripTestItems(stripComments(…))`）后失衡为 **0**。**探针必须和被测函数的契约对齐**，否则"发现"的是自己的 bug。（这一条与第 168 轮 `judged=5` 误当成 self-build 数是同一类。）

### B. 新测试文件 `scripts/rust_blank.test.cjs`（**11 条**，接进 CI `scripts` 作业与本机门禁 ⇒ 门禁 **91 → 92 条**）

①`stripComments` 保持长度与行数（含中文注释的多字节情形）；②**字符字面量里的花括号**（`'{'` / `'}'`）不让花括号配对失步 —— 且断言**测试项之后的生成代码仍然可见**；③转义字符字面量 `'\''`、`'\\'`；④raw 字符串 `r#"…"#` / `r##"…#"##`（含**内容里有 `"#`** 的情形）；⑤字符串里的 `//` 不是注释、注释里的 `"` 不是字符串；⑥**嵌套块注释**整体抹除；⑦`stripTestItems` 的判定方向：`cfg(test)`、**`cfg(all(test, …))`**（隐含 test ⇒ 抹）、**`cfg(any(test, …))`**（可以随特性发布 ⇒ **必须扫描、不能隐藏**）、`cfg(not(test))`、`cfg(feature = "test")`（**字符串里的 test 不算 test 谓词**）、以及 **`mod tests;` 分号形式**（必须停在 `;` 而不是跑到下一个花括号块）；⑧`stripCommentsOnly` 保留字面量内容、`stripComments` 抹掉；⑨`stripCommentsAndTestItems` 抹测试项但保留生产行的字面量；⑩**全树钉子**（119 文件：偏移零漂移、花括号零失衡、零文件被抹空）；⑪**真实文件钉子**（`crates/hydra-server/src/admin/metrics.rs`：原始 **5** 个 `register_int_counter!`，抹掉测试项后**剩 4**，且 `#[cfg(test)]` 消失）。

### C. 反向证伪（两条，都把 helper 的一条承重分支拆掉）

1. **不再抹字符字面量**（`if (false && blankLiterals && c === "'")`）⇒ **全树钉子那条红**；更值得记的是同一反转下的全树实测：**3 个真实文件**的花括号配对立刻失步（`crates/hydra-core/src/sse.rs`、`crates/hydra-server/src/sink.rs`、`crates/hydra-server/tests/clickhouse_ddl_parity.rs`）—— 也就是说这条分支**不是理论上的**：今天就有 3 份文件会在它失效时把后面的代码藏起来；②**把 `any(test, …)` 也算成 test**（`if (m[1] === 'all' || m[1] === 'any')`）⇒ **恰好第 ⑦ 条红** ⇒ "不确定就按生产处理（扫描而不是隐藏）"这个安全方向是承重的。两次都从 **POST-FIX 备份**恢复（`diff -q` 一致、`FALSIFY` = 0），并复跑**全部 7 个消费者**（`check_source_purity` 及其 26 条测试、`check_documented_metrics`/`_env`、`check_tenant_error_codes`、`check_alert_expressions`）全部出口 0。

### D. 顺带：把"会随测试文件增长而变"的计数写法改掉

`check_test_tails` 里那句计数**连续三轮都要改**（65 → 66 → 67：每加一个 suite 文件分母就动）。本轮改成**只钉"被判定的数量"（47）**，并写明**分母会随每个新 suite 文件增长**、列出这三轮的 65→66→67 —— 数字仍然是实测的，但不再需要每轮返工。

### E. 本轮验证（门禁判定）

`bash .acceptance/round10-gate.sh` ⇒ **92 条条目全部 `exit=0`**（本轮 **+1**：`rust blanker (offsets/items)`）、`OVERALL=GREEN`、`GATE_EXIT=0`（`entries=92` / `GATE COMPLETE`；stdout 存 `.acceptance/round177-gate.out`）。**本轮改动在门禁内执行并通过**（日志可查）：`rust blanker (offsets/items)` 条目内 `node --test scripts/rust_blank.test.cjs` = **11 tests / 0 fail**；`js masker (regex desync)` = 8 tests / 0 fail；`test tails (no dead cases)` 输出 `[test-tails] OK (47 of 67 suite file(s) … ; 2 recorded exception(s) …)`。`check_gate_entries`（92 条）/ `check_ci_wiring` / `findings-disposition` 出口 0；`FALSIFY` = **0**（两条反转均从 POST-FIX 备份恢复并 `diff -q` 校验）；`git status --short` 212 项（**未提交**）。

## 2fj. 第一百七十八轮：把上一轮的掩码修复**追到它的下游** —— `check_e2e_contracts` 会在"读不了的文件"上**整条规则静默**

### A. 上一轮的修复对下游是什么影响？先量，别猜

`check_e2e_contracts.cjs` 用的是同一个掩码器，而且它对 `unterminated` 的处理比"计数不准"严重得多：命中就把该文件报成 **"cannot be parsed as JS"** 并 **`continue`** —— 也就是说**那个 spec 里的每一条 `api()`/时间戳检查全部跳过**。实测（本轮）：
* **真实 spec 集合**（`tests/e2e/*.spec.cjs`）在**修复前后都是 `exit 0`** ⇒ 对今天的仓库这处影响是**潜在的、不是现存的**（没有任何真实 spec 含"正则里的反引号"）；
* 但只要有一个，后果就是"**规则在一份它读不了的文件上彻底沉默**"。

### B. 于是在**它自己的测试套件**里把这条钉死（而不是只写进文档）

`scripts/check_e2e_contracts.test.cjs` 新增一个夹具（+2 条断言）：一份 spec 里同时有
```js
const CODE_RE = /`[^`]*`/;          // 正则里的反引号：合法 JS，却是旧掩码器的"模板起点"
await api('POST', '/providers', { body: { id: '', key: 'k' } });   // 故意漏掉时间戳
```
断言**两半**：①**不得**报 `unterminated string or template literal`（不许把合法文件说成不可解析）；②**并且** `api()` 的载荷检查**仍然执行**（仍要抓到 `omits \`created_at\``）—— 只有两半一起，才证明"这份文件是可读的"而不仅是"没有触发那条例外"。

### C. 反向证伪（把掩码器换回修复前那一版）

把 `scripts/js_blank.cjs` 临时换成**第 176 轮的 PRE-FIX 版本**（`.acceptance/round176-bak/js_blank.PRE-FIX.cjs`）：
```
FAIL  a regex containing a backtick is NOT reported as an unterminated template
      -> DRIFT … an unterminated string or template literal — this file cannot be parsed as JS …
FAIL  ...and the api() payload check still runs on that file (the missing timestamps are caught)
      -> status=1 DRIFT …（同上）
```
⇒ **两条断言同时红**，且失败信息**逐字复现**"规则在这份文件上沉默"的症状；同一时刻真实 spec 集合仍是 `exit 0`（再次确认影响是潜在的）。恢复方式：用本轮实验前另存的 POST-FIX 掩码器覆盖，并**额外与第 176 轮的 POST-FIX 备份 `diff -q`**（两者必须逐字节一致）⇒ 通过，`FALSIFY` = 0，`js_blank.test.cjs` 8/8 与 `rust_blank.test.cjs` 11/11 复跑全过，`check_e2e_contracts` 真树 `exit 0`。

### D. 同一类问题的另一半：**门禁解析器不能"吃掉"条目**

顺着"解析器吃掉文本"这条线查了 `check_gate_entries.cjs` 的 `entries()`：它只认 `^gate\s+"…"\s+…`。**bash 接受而它不接受的写法**（`gate 'x' …` 单引号、` gate "x" …` 前导空格）会让那条目**照常运行**，却**对本守卫的每一条规则都不可见** —— 包括"跑的 drill 必须自己构建二进制"这条（它存在的原因是第 143 轮那次整门禁 RED）。
**实测**：真门禁脚本 **92 条条目、0 条**这种行 ⇒ 今天是**潜在洞**，正因如此要把它变成**断言**而不是假设。新增 **完整性检查**：任何"看起来在调用 `gate` 却没能被解析"的行 ⇒ DRIFT，并给出正确写法。**测试 +3**（单引号 ⇒ DRIFT、前导空格 ⇒ DRIFT、控制组列零双引号 ⇒ 无 finding；控制组刻意用**普通 drill**，否则会同时踩到规则 1，一个"因两个原因失败"的用例两个都不证明）；**22 passed / 0 failed**。**反向证伪**：把检测正则改成永不匹配（`gateZZ`）⇒ **恰好那两条新用例红**、其余 20 条全绿（我第一版把 `continue` 一起反转，结果 11 条控制组同时红 —— 那不是机制证伪，是把守卫弄坏了；**证伪也要最小化改动**）。恢复后 `diff -q` 一致、`FALSIFY` = 0、真树 `check_gate_entries` 出口 0。
**★门禁重跑的原因**：这一改动落在本轮门禁**已经跑过** `gate entries build what they run` 与 `gate entry checker tests` 之后 ⇒ 那份判定**不覆盖**最终修订，于是**杀掉旧运行、用最终修订重跑完整门禁**（宁可多花一次 40 分钟，也不把"判定覆盖的不是最终状态"记成通过）。

### E. 本轮验证（门禁判定）

以**最终修订**重跑（旧运行在改动前已跑过相关条目，那份判定不覆盖本轮）：`bash .acceptance/round10-gate.sh` ⇒ **92 条条目全部 `exit=0`**、`OVERALL=GREEN`、`GATE_EXIT=0`（`entries=92` / `GATE COMPLETE`；stdout 存 `.acceptance/round178-gate.out`）。**两处改动都在门禁内执行并通过**（日志可查）：`e2e contract tests` 条目里有 `PASS  a regex containing a backtick is NOT reported as an unterminated template` + `ALL E2E CONTRACT TESTS PASSED`；`gate entry checker tests` 条目 = **22 tests / 0 fail**；`e2e contracts` / `gate entries build what they run` 均 `exit=0`。`check_test_tails` / `check_ci_wiring` / `findings-disposition` 出口 0；`FALSIFY` = **0**；`git status --short` 212 项（**未提交**）。

## 2fk. 第一百七十九轮：把"**门禁判定必须覆盖最终修订**"从人的自觉变成**机器可查**

### A. 动机就是上一轮的实测：GREEN 的判定**不覆盖最终修订**，而只有人会看出来

第一百七十八轮的门禁在改动落地后被证明**判定的是改动前的修订**（两处改动所在的条目已经跑过），我**只能靠人读日志**才发现，然后手动杀掉重跑。这类"判定与修订不一致"在整场会话里反复出现（每轮都在门禁运行期间改文件），而**没有任何东西记录"这一跑判的是哪一版树"**。

### B. 新工具 `scripts/tree_manifest.cjs`（两个模式，一次运行各用一次）

* `--write <file>`：把**覆盖范围内的每个文件**的 sha256 写成 `hash␣␣相对路径` 的排序清单；
* `--check <file>`：重新计算并与清单比对，分成三类并**分级**：
  * **CODE 改动（`scripts/` `integration/` `crates/` `tools/` `tests/` `environment/` `.github/` 与根文件）⇒ exit 1**，逐条打印 `CHANGED modified|added|removed <path>`，并说明"更早的条目判的是一个已经不存在的修订"；
  * **仅 DOCS 改动（`docs/` `dev-docs/`）⇒ exit 0 + NOTE**（没有代码移动，但**读文档的条目** —— `findings-disposition`、`public claims` —— 可能不与最终文本一致）；
  * 清单缺失/格式坏 ⇒ **exit 2（CANNOT VERIFY）**，绝不猜。
* **被排除的生成/草稿区**（`target`、`.acceptance`、`node_modules`、`dist`、`dist-test`、各类 cache、`*.log`）——否则每次运行都会"看起来不稳定"（尤其 `.acceptance` 里有门禁自己的日志与每个 drill 的 scratch）。

### C. 接进门禁：**第一条**记录、**最后一条**核验（⇒ 92 → **94 条条目**）

`gate "tree manifest (record)"` 放在最前（`fmt --check` 之前），`gate "tree manifest (verify)"` 放在最后（`cli tests (hydra-admin)` 之后）——两条都在 GATE SUMMARY 里出现，退出码照常计入判定。**实测首跑**：第一条 `exit=0` 并打印 `recorded 352 file(s)`。
**新测试文件** `scripts/tree_manifest.test.cjs`（**7 条**，已接进 CI `scripts` 作业 —— 这里顺带被 `check_ci_wiring` **当场抓到**："`scripts/tree_manifest.test.cjs` is never executed by ci.yml"，守卫按设计工作）：①不变 ⇒ clean；②**代码**改动 ⇒ exit 1 且**点名文件**；③**控制组**：仅文档改动 ⇒ exit 0 + `NOTE: 2 doc file(s) changed`；④新增代码文件 ⇒ 1（并点名 `added`）；⑤删除代码文件 ⇒ 1（`removed`）；⑥**生成区被忽略**（改 `target/`、`.acceptance/<drill>/state.db`、`scripts/run.log` 后仍 clean）；⑦清单缺失 ⇒ 2；⑧清单格式坏 ⇒ 2（不猜）。
**反向证伪**：把 `isDoc` 改成恒 `false` ⇒ **恰好第③条（docs-only 控制组）红**，其余全绿 ⇒ 两级分类是承重的。恢复后 `diff -q` 一致、`FALSIFY` = 0、7/7 通过。
**★这一轮还有一个"自证"**：因为新工具的核验条目就在门禁**内部**，我**在本次运行期间只允许改 `dev-docs/`**（那会走 NOTE 分支）——这一轮写文档的动作本身就是对机制的第一次真实使用。

### D. 本轮验证（门禁判定）——★**机制在第一次真实运行里就抓到了本轮我自己的文档改动**

`bash .acceptance/round10-gate.sh` ⇒ **94 条条目全部 `exit=0`**（本轮 **+2**：`tree manifest (record)` / `tree manifest (verify)`）、`OVERALL=GREEN`、`GATE_EXIT=0`（`entries=94` / `GATE COMPLETE`；stdout 存 `.acceptance/round179-gate.out`）。**日志里三条实测证据**：
```
[tree-manifest] recorded 352 file(s) in .acceptance/gate-manifest.txt
[tree-manifest] NOTE: 1 doc file(s) changed during the run (modified dev-docs/aegis/plans/2026-09-29-oracle-remediation.md) — no code moved, but the doc-dependent entries may not match the final text
[tree-manifest] OK  (the source tree is unchanged: 352 file(s), docs-only changes: 1)
```
⇒ 即：**我在门禁运行期间写 §2fk 这个动作被新机制自己检出**，并**被正确判为"仅文档"**（exit 0 + NOTE）；如果这一轮我改的是任何**代码**路径（`scripts/` `integration/` …），最后那条条目会 **exit 1**、整门禁 **RED**，从而强制"判定必须覆盖最终修订"。这就是这一轮想解决的问题的**闭环证据**，而且是它第一次投入使用的真实运行。`ci wiring` / `ci wiring tests` 出口 0（顺带证明新测试文件已接进 CI）；`FALSIFY` = **0**；`git status --short` 214 项（**未提交**）。

## 2fl. 第一百八十轮：`check_documented_env` 只认**表格第一列的名字** —— 于是**散文里承诺的开关它看不见**

### A. 先量：把"文档里的名字"和"被检查的名字"两个集合算出来

`check_documented_env.cjs` 的契约是"**文档承诺的开关必须在代码里有读取点**"，而它的解析器只取**表格行的第一列**里的反引号名字。本轮把三个集合算清楚（实测 2026-10-01）：
* ops.md 里**第一列**出现的名字：**42**（含 `RUST_LOG`、`SIGTERM` 这类非 `HYDRA_` 的）；
* ops.md 里**任何位置**出现的 `HYDRA_*` 名字：**50**；
* **差集 10 个**：`HYDRA_LOG`、`HYDRA_FAIL_MODE`、`HYDRA_AUTH_FAIL_MODE`、`HYDRA_AUTH_FAILMODE`、`HYDRA_FAILOVER_GRACE_MS`、`HYDRA_RATE_LIMIT_FAIL_MODE`、`HYDRA_ENCRYPTION_KEY_FILE`、`HYDRA_CLICKHOUSE_IO_TIMEOUT_MS`、`HYDRA_ENCRYPTION_KEY_PREVIOUS_VERSION`、`HYDRA_BREAKER_QUORUM`。
**逐个查证（每一个都读了上下文 + 代码）**：前六个**本来就不该有读取点**——`HYDRA_LOG` 在 `RUST_LOG` 那一行里被明确写成「**NOT read**」；三个 fail-mode 拼写出现在一条"测得它们**仍然回 503**"的注记里；`HYDRA_FAILOVER_GRACE_MS` 与 `HYDRA_RATE_LIMIT_FAIL_MODE` 在 ops.md 的**"文档写了但没接线"清单**里（第 1668–1670 行）；后四个是**真开关**，但写在**相邻行的描述里**（`HYDRA_ENCRYPTION_KEY_FILE` 在 `HYDRA_ENCRYPTION_KEY` 行内、`HYDRA_CLICKHOUSE_IO_TIMEOUT_MS` 在 `..._QUERY_TIMEOUT_MS` 行内等），代码里**确有读取点**（`crypto.rs:142`、`clickhouse.rs:135`、`crypto.rs:186`、`main.rs:73`，本轮逐个 grep 核对）。
⇒ **今天没有"承诺了却没人读"的开关**（负结论，但这次是**逐条查证**的负结论）。

### B. 但缺口是真的：**新**在散文里承诺的开关会**无声地**绕过这条规则

所以本轮把"散文名"这条路径**从"看不见"变成"必须记账"**：新增 `PROSE_ONLY_OK`（名字 → **理由**），把上面 10 个逐个记录在案；规则是「**任何出现在第一列之外的 `HYDRA_*` 名，未记录 ⇒ FAIL**」，并且**记录也会过期**：某名字**后来有了表格行**、或**从文档里消失** ⇒ FAIL（"不能过期的记录就是陈旧断言"——`UNVERIFIED_OK` 与 `UNJUDGED_WITH_EXIT_OK` 的同一课）。
**OK 行**同时把两个数都打出来 ⇒ 覆盖范围可见：`40 distinct documented env name(s) (10 more appear in prose and are recorded as needing no read site) (44 occurrence(s)) …`。
**测试 +4**（未记录的散文名 ⇒ FAIL 且点名并要求"给行或记账"；控制组：同一名字**已记录** ⇒ exit 0 且 OK 行报 `1 more appear in prose…`；**记录过期**（名字不在文档里）⇒ FAIL；**记录过期**（名字后来有了表格行）⇒ FAIL），**30 passed / 0 failed**。夹具需要能**替换**记录表，故加 `DOC_ENV_PROSE_OK`（**替换**而非合并），测试骨架默认设为 `{}`（否则 16 条既有用例会被"仓库里那 10 条记录在本夹具里用不上"打红——这次又是**先被自己的规则打到**）。**真实 ops.md 那条用例刻意*不*设覆盖**，因为它断言的正是"仓库自己的记录足够"。

### C. 反向证伪（两条，分别拆掉一个机制；并且**第二次不再叠加在第一次之上**）

1. 让 `unrecorded` 恒空（散文规则不触发）⇒ **恰好"未记录的散文名"那条红**，真树仍 exit 0；
2. 让 `staleRecords` 恒空（过期检查不触发）⇒ **恰好那两条过期用例红**、第①条通过。
**★过程失误（记下）**：我第二版证伪**直接叠在第一版之上**（忘了先恢复），于是三条测试同时红、看起来像"过期检查也承重"，其实是两个机制都被拆掉了 —— 与第 178 轮"把 `continue` 一起反转"同型：**证伪必须建立在干净状态上，且只改一处**。重做后才得到上面那份干净结论。恢复后 `diff -q` 一致、`FALSIFY` = 0、30/30 通过、真树 exit 0。

### D. ★本轮另一次自伤：`check_ci_wiring` 的"`run:` 行数"完整性检查**被门禁抓成 RED**（已修）

同一轮里我还给 `check_ci_wiring.cjs` 加了另一条完整性检查（"工作流里有 N 行 `run:`，解析器就必须产出 N 个 step；差集对**本文件每条规则**都不可见"）。**首跑门禁直接 RED**：`ci wiring tests exit=1` ⇒ 那条用例是第 150 轮写的 `a JOB-level \`defaults.run.working-directory\` covers the tests in that directory` —— 它的夹具里有
```yaml
    defaults:
      run:
        working-directory: tools/sdk
```
而 **YAML 里 `run:` 键只有两个家：step 与 job 级 `defaults.run`**。后者不是命令，我的计数却把它算进去了 ⇒ **假阳性**（14 行 vs 13 个 step）。**修法**：按缩进算出 `defaults:` 块的行区间并从计数里排除；**实测**：真实工作流仍是 **115 行 = 115 step**、`check_ci_wiring` 真树 `OK (everything is executed — 47 artifact(s) examined)`、其 73 条 PASS 全过。
**两条同族的教训**（都在本轮）：①**新的完整性断言必须先跑整套守卫的用例**——真实树绿不代表夹具绿（夹具刻意构造畸形输入，正是新规则最容易误伤的地方）；②**门禁这一次真的起作用了**：它把"我自己新加的检查误伤既有夹具"在一轮内变成 RED，而不是让一条假阳性悄悄留在守卫里。

### E. 本轮验证（门禁判定）

**第一次运行 RED、修好后重跑 GREEN**（RED 的原因与修法见 D 段）：
* **RED 那次**：`GATE ci wiring tests exit=1` / `OVERALL=RED` / `GATE_EXIT=1` —— 我的新完整性检查误伤第 150 轮的 job-`defaults.run` 夹具；
* **修好后重跑**：`bash .acceptance/round10-gate.sh` ⇒ **94 条条目全部 `exit=0`**、`OVERALL=GREEN`、`GATE_EXIT=0`（`entries=94` / `GATE COMPLETE`；stdout 存 `.acceptance/round180-gate.out`）。
**门禁内证据**：`documented env tests` 条目 = **30 tests / 0 fail**（本轮 +4）；`documented env wired` 输出含 `40 distinct documented env name(s) (10 more appear in prose and are recorded as needing no read site) …`；`ci wiring` / `ci wiring tests` 出口 0（73 条 PASS）；`tree manifest (verify)` ⇒ `[tree-manifest] OK (the source tree is unchanged: 352 file(s), docs-only changes: 0)` —— **最后一次运行时整棵树都被判定覆盖**（这次我在启动前就把文档写完，所以连 NOTE 都没有）。`FALSIFY` = **0**；`git status --short` 214 项（**未提交**）。

## 2fm. 第一百八十一轮：`check_documented_metrics` 的**排除表**里有**没人用、也没人证明**的条目

### A. 先量：文档里的指标名 vs 守卫实际检查的名字

`check_documented_metrics.cjs` 的契约是「**运维文档提到的每个 `hydra_*` 序列都必须真的注册过**」。把两个集合算清楚（实测 2026-10-01）：
* 三份运维文档（`ops.md` / `cluster.md` / `tenant-api-integration.md`）里出现的 `hydra_*` 名字：**39 个**；
* 其中**以 `_` 结尾的通配前缀**（`hydra_proxy_listener_*` 之类）：**2 个**（守卫打印"2 wildcard prefix(es) skipped"）；
* ⇒ 非通配 **37 个**，而守卫报的是 **36 distinct**。差的**1 个**来自 `NOT_A_METRIC` 排除表（crate/工具名，不是指标）—— 本轮核对：三份文档里只有 **`hydra_core`** 真的被提到过（1 次）。

### B. 问题：排除表里 **8/9 条目今天什么都没"排除"**，其中 **2 条连理由都找不到**

逐个核对 `NOT_A_METRIC` 的 9 个条目（在**真实目录/包名**里找依据）：
* **`hydra_core`** → `crates/hydra-core` ✓（**且**文档里确实提到 ⇒ 真的跳过了一个词）；
* **`hydra_server`** → `crates/hydra-server` ✓；**`hydra_py`/`hydra_ts`/`hydra_go`** → `tools/hydra-py|ts|go` ✓；**`hydra_sdk`** → `tools/hydra-py/hydra_sdk/`（Python 包目录）✓；**`hydra_admin`** → `tools/hydra-cli/package.json` 的 `name`/`bin` ✓；
* **`hydra_dev`、`hydra_ui`** → **除这张表本身以外，全仓（docs/crates/tools/scripts/tests/environment/.github）零出现、也没有同名目录** ⇒ **死条目**。
**为什么死条目危险**：排除表在**注册查找之前**生效，所以它正是"文档承诺了、代码里不存在"的指标**藏身之处**——一条不用的排除项就是一处**静默豁免**。**修法**：**删掉 `hydra_dev`/`hydra_ui`**，并把"条目必须**挣得**自己的位置"写成断言：**每个条目要么在扫描到的文档里真的跳过了某个名字，要么对应一个真实存在的 crate/工具/包目录或包名**；否则 **DRIFT**。OK 行同时**打印跳过计数与名字**（此前完全静默）：`… 1 crate/tool name(s) skipped by NOT_A_METRIC (hydra_core); 2 wildcard prefix(es) skipped; …`。
**测试 +3**（未被使用且无依据的排除项 ⇒ FAIL 且点名；控制组：被文档提到的排除项 ⇒ 通过**且**打印计数；控制组：由**真实目录**支撑的排除项 ⇒ 通过），夹具需要能**替换**排除表 ⇒ 加 `CDM_NOT_A_METRIC`（**替换**而非合并），测试骨架默认 `[]`（否则既有夹具里 `hydra_core`/`hydra_server` 会因"临时目录里没有那两个 crate 目录"而被判无依据 —— 本轮又是**先被自己的规则打到**：改完首跑 **9 条既有断言同时红**）。
**两条反向证伪**：①把 `hydra_ui` 加回内置表 ⇒ **真实树 exit 1** 并点名 `hydra_ui … skips NOTHING …`（同时夹具 23 条仍全过，因为夹具不继承内置表）；②让"依据检查"永不触发 ⇒ **恰好那条新用例红**。恢复后 `diff -q` 一致、`FALSIFY` = 0、真树 `OK`、套件 **23 passed**。

### C. 本轮验证（门禁判定）

`bash .acceptance/round10-gate.sh` ⇒ **94 条条目全部 `exit=0`**、`OVERALL=GREEN`、`GATE_EXIT=0`（`entries=94` / `GATE COMPLETE`；stdout 存 `.acceptance/round181-gate.out`）。**门禁内证据**：`documented metric tests` 条目 = `documented-metrics guard tests: PASSED (23 assertions …)`（本轮 +3）；`documented metrics` 条目的 OK 行含 `1 crate/tool name(s) skipped by NOT_A_METRIC (hydra_core); 2 wildcard prefix(es) skipped`；`tree manifest (verify)` ⇒ `OK (the source tree is unchanged: 352 file(s), docs-only changes: 1)`，那条 NOTE 又一次**如实记录了我本轮在门禁运行期间写 §2fm 的文档改动**（仅文档 ⇒ exit 0）。`FALSIFY` = **0**；`git status --short` 214 项（**未提交**）。

## 2fn. 第一百八十二轮：继续查"**豁免必须还在用**" —— `READ_BY_DEPENDENCY` 的死豁免方向，以及一处**连续十三轮没人动过的过期计数**

### A. `check_documented_env.READ_BY_DEPENDENCY`：豁免的**危险方向**没有被检查

这条豁免的含义是「这个文档化的开关**不需要字面读取点**，因为它由**依赖**读取」——今天只有一条：`RUST_LOG`（`EnvFilter::from_default_env()`，`main.rs`）。**危险的方向是"死豁免"**：如果树里**长出了**一个字面读取点，那条豁免就不再豁免任何东西，而且**静默藏起那次读取**（守卫先看 `sites`，再看这张表；一旦 `sites` 命中就 `continue`，表里的条目**不会再被检查**）。
**实测（本轮）**：`RUST_LOG` 在仓库里的出现只有**注释**（`admin/mod.rs:77`、`metrics.rs:185`）和**测试里设置它**（`.env("RUST_LOG","info")` —— 那是"写"不是"读"），所以豁免**仍然需要** ⇒ 今天没有死豁免（负结论，但现在是**被断言**的）。
**修法**：新增检查 —— **豁免的名字若在本树里有了字面读取点 ⇒ FAIL**（"豁免已死且藏起那次读取，删掉它"），并打印记录的理由；**另一个方向**（名字离开了文档表格）**只作为 NOTE**，因为**定向夹具文档只有三行**，否则夹具里每条豁免都会被报"不再是文档化名字"（这是本轮**刻意**选择的取舍，写进了注释）。测试 +3（豁免名字**长出**字面读取点 ⇒ FAIL 且点名并要求删除；控制组：无字面读取点 ⇒ exit 0 且打印那条 NOTE；控制组：**真实表格**仍保留 `RUST_LOG` 豁免并打印）。为了可测，`READ_BY_DEPENDENCY` 增加**替换式**覆盖 `DOC_ENV_READ_BY_DEPENDENCY`（默认仍是仓库那一条，因为**既有夹具本来就依赖它** ⇒ 这次不需要动夹具，与 180/181 轮"先被自己的规则打到"形成对照）。**证伪**：让死豁免检查永不触发 ⇒ **恰好那条新用例红**，其余 32 条全绿；恢复后 `diff -q` 一致、`FALSIFY` = 0、**33 passed**。

### B. 顺带抓到一处**连续十三轮没人动过的过期计数**（同类里最典型的一例）

`check_documented_defaults.cjs` 的 `UNVERIFIED_OK` 文档注释写着「**12 rows here, 21 compared**」（第一百一十九轮实测），而**今天实测是 10 rows / 23 compared**（该守卫自己的 OK 行就打印这一对：`OK (23 documented default(s) match the code, 10 unverifiable by extraction)`）。⇒ 注释里的两个数字**都过期**，而且它在一个**专门用来记录不可比行的清单**旁边 —— 正是"注释里的数字要先量再改"最该命中的地方。已改成**实测值 + 复测方法**（跑一下守卫读 OK 行），并保留第一百一十九轮的历史注记。

### C. 本轮验证（门禁判定）

`bash .acceptance/round10-gate.sh` ⇒ **94 条条目全部 `exit=0`**、`OVERALL=GREEN`、`GATE_EXIT=0`（`entries=94` / `GATE COMPLETE`；stdout 存 `.acceptance/round182-gate.out`）。**门禁内证据**：`documented env tests` = **33 tests / 0 fail**（本轮 +3）；`documented env wired` 打印 `note: RUST_LOG is read by a DEPENDENCY …`（豁免仍在用）；`documented defaults` 的 OK 行 = `OK (23 documented default(s) match the code, 10 unverifiable by extraction)` —— **与改正后的注释一致**（`12/21` → `10/23`）；`tree manifest (verify)` ⇒ `OK (… docs-only changes: 1)`（NOTE 如实记录本轮运行期间的文档改动）。`FALSIFY` = **0**；`git status --short` 214 项（**未提交**）。

## 2fo. 第一百八十三轮：把「记录表 + 过期检查 + 依据检查」这套模式**收敛成单一所有者**（`recorded_exceptions.cjs`）

### A. 动机：同一套不变量已经被**手写了四遍**，每遍都被审出不同的洞

"记录下来的豁免"（guard 故意不判的东西，各带理由）在四个守卫里各写了一份：
| 守卫 | 记录表 | 本轮之前的实现 |
|---|---|---|
| `check_test_tails.cjs` | `UNJUDGED_WITH_EXIT_OK` | 自己的 `undefined ? new Map([...]) : new Map(Object.entries(JSON.parse(...)))` |
| `check_documented_env.cjs` | `PROSE_ONLY_OK`、`READ_BY_DEPENDENCY` | 同上（两份） |
| `check_documented_defaults.cjs` | `UNVERIFIED_OK` | **没有**覆盖口；过期检查只对"随包表格"生效 |
| `check_tenant_error_codes.cjs` | `UNVERIFIED_OK` | **没有**覆盖口；过期检查只对"随包表格"生效 |
三条不变量是**同一套**：①**覆盖是"替换"不是"合并"**（夹具不能继承仓库的记录，否则会因"关于不存在文件的记录"整体变红——这条在 180/181 轮各撞了一次）；②**未记录 ⇒ finding**；③**记录过期 ⇒ finding**（"不能过期的记录就是陈旧断言"）。
**修法**：新增 `scripts/recorded_exceptions.cjs` + 8 条测试，导出 `records(envRaw, builtin)`（替换语义、并**拒绝空理由/非对象**覆盖）与 `audit({records, needed, applies})`（返回 `{unrecorded, stale, used}`）。**刻意不放进模块的**：消息措辞与退出码——每个守卫有自己的语气、自己的测试断言自己的措辞，所以模块**只返回结构**，由调用方说话。
四个守卫改为调用它（行为保持不变；`documented_defaults`/`tenant_error_codes` 顺带获得此前**没有的**覆盖口 `CDD_UNVERIFIED_OK`/`CTEC_UNVERIFIED_OK`），新测试文件接进 CI `scripts` 作业与门禁 ⇒ **门禁 94 → 95 条**。

### B. 跨守卫证伪：把共享的"过期方向"关掉 ⇒ **五个套件同时红**

把 `audit` 的 `stale` 恒置空（`const stale = []`）后逐套件跑：
`recorded_exceptions` **2 条红**、`check_test_tails` **1 条**、`check_documented_env` **2 条**、`check_tenant_error_codes` **1 条**、`check_documented_defaults` **1 条**（本轮新增）⇒ 共享所有者**对五个守卫都是承重的**，而不是"抽出来好看"。恢复后 `diff -q` 一致、`FALSIFY` = 0。

### C. 本轮我自己的三个失误（都由既有守卫当场抓到，逐条记下）

1. **TDZ**：`require('./recorded_exceptions.cjs')` 第一版插在**使用点之后**（锚点选在了文件中部）⇒ `ReferenceError: Cannot access 'records' before initialization`，`check_documented_env` 的 **28 条**测试同时红。修法：require 放到文件顶部其它 require 旁。
2. **极性反转**：`check_tenant_error_codes` 的 `applies` 我写成 `!documented.has(code) || !unverifiedUsed.has(code)`，而它应当是 `!documented.has(code) || unverifiedUsed.has(code)`（**`applies` 回答"这条记录要保留吗"**，不是"它过期了吗"）⇒ 守卫立刻报**三条记录全部过期**（3 DRIFT）。修法：改正极性，并把这条**极性陷阱写进模块头**（"共享谓词最容易邀请这种反转"）。
3. **死代码**：给 `check_documented_defaults.test.cjs` 追加用例时**加在了 `process.exit` 之后** ⇒ `check_test_tails` 直接报 `DEAD scripts/check_documented_defaults.test.cjs:309 — 19 line(s) of code sit AFTER the top-level exit`（**正是第 170/175 轮修的那个形状**，这次是它自己抓到我）⇒ 把用例移到摘要之前，并**重新做证伪**确认它真的活着（关掉共享过期方向 ⇒ 该用例红）。

### D. 本轮验证（门禁判定）

`bash .acceptance/round10-gate.sh` ⇒ **95 条条目全部 `exit=0`**（本轮 **+1**：`recorded exceptions`）、`OVERALL=GREEN`、`GATE_EXIT=0`（`entries=95` / `GATE COMPLETE`；stdout 存 `.acceptance/round183-gate.out`）。**门禁内证据**：`recorded exceptions` 条目 = **8 tests / 0 fail**；四个重构过的守卫条目 `test tails (no dead cases)` / `documented env wired` / `documented default tests` / `tenant error codes` **全部 `exit=0`**（`documented env tests` 33、`check_test_tails` 26 条用例均在门禁内跑过）；`tree manifest (verify)` ⇒ `OK (the source tree is unchanged: 354 file(s), docs-only changes: 1)`（文件数 352 → 354 = 新增模块 + 其测试；NOTE 如实记录本轮运行期间的文档改动）。`FALSIFY` = **0**；`git status --short` 214 项（**未提交**）。

## 2fp. 第一百八十四轮：把最后一个守卫也接到共享的"记录表"上 —— `check_documented_metrics` 的两张表

### A. 收口：`NOT_A_METRIC`（Set → **带理由的 Map**）与 `ABSENT_ON_PURPOSE`

上一轮把四个守卫的记录表收敛到 `recorded_exceptions.cjs`，剩下的是 `check_documented_metrics.cjs` 的两张：
* **`NOT_A_METRIC`** 此前是**一组名字**（`new Set([...])`，环境覆盖是 JSON **数组**），第 181 轮给它加了"必须挣得位置"的检查（要么文档里真的跳过某名字、要么对应真实 crate/工具/包）。本轮把它改成**名字 → 理由的 Map**（每条现在都写明它代表哪个 crate/工具/包：`hydra_sdk` → `tools/hydra-py/hydra_sdk` 包目录、`hydra_admin` → `tools/hydra-cli` 的包名与 `hydra-admin` 二进制 ……），加载交给共享模块（覆盖 = JSON **对象**），并把第 181 轮那条规则**表达成共享代数的 STALE 方向**：`applies(name) = 文档真的跳过了它 ∨ 真实 crate/工具/包承载它`。⇒ 语义没变（行为逐字保持，消息不变），但"记录表"现在**只有一个所有者**，"过期"也**只有一种定义**。
* **`ABSENT_ON_PURPOSE`**（文档明确写"故意不存在"的名字，今天为空）的过期检查同样改为共享 `audit(...)`；它保留**自己的逗号列表**环境语法（`CDM_ABSENT_ON_PURPOSE=a,b`），因为**它的夹具就是这么写的** —— 这一点写进计划以免后人误以为漏了。
**测试**：`check_documented_metrics.test.cjs` 的 5 处覆盖从数组改为对象（`{"hydra_sdk":"the Python package"}` 等），**23 assertions 全部通过**；真树 `OK`，OK 行仍打印 `1 crate/tool name(s) skipped by NOT_A_METRIC (hydra_core)`。
**反向证伪（针对重构后的规则）**：把一条**死记录**加回内置表（`['hydra_ui', …]`）⇒ **真实树 exit 1** 并以**同样的措辞**点名 `hydra_ui is in NOT_A_METRIC but skips NOTHING …`，夹具套件不受影响（它们不继承内置表）；恢复后 `diff -q` 一致、`FALSIFY` = 0。

### B. 至此"记录表"的账（可复核）

| 守卫 | 记录表 | 是否接共享模块 |
|---|---|---|
| `check_test_tails.cjs` | `UNJUDGED_WITH_EXIT_OK` | ✅ |
| `check_documented_env.cjs` | `PROSE_ONLY_OK`、`READ_BY_DEPENDENCY`（加载） | ✅ |
| `check_documented_defaults.cjs` | `UNVERIFIED_OK`（+ 新增覆盖口 `CDD_UNVERIFIED_OK`） | ✅ |
| `check_tenant_error_codes.cjs` | `UNVERIFIED_OK`（+ 新增覆盖口 `CTEC_UNVERIFIED_OK`） | ✅ |
| `check_documented_metrics.cjs` | `NOT_A_METRIC`、`ABSENT_ON_PURPOSE` | ✅（本轮） |
⇒ **五处全部单一所有者**；`check_documented_metrics` 的 `NOT_A_METRIC` 还顺带从"一组名字"升级成"名字 + 理由"，与其它四处一致。

### C. 本轮验证（门禁判定）

`bash .acceptance/round10-gate.sh` ⇒ **95 条条目全部 `exit=0`**、`OVERALL=GREEN`、`GATE_EXIT=0`（`entries=95` / `GATE COMPLETE`；stdout 存 `.acceptance/round184-gate.out`）。**门禁内证据**：`documented metric tests` 条目 = `documented-metrics guard tests: PASSED (23 assertions …)`；`documented metrics` 条目的 OK 行仍含 `… 1 crate/tool name(s) skipped by NOT_A_METRIC (hydra_core) …`；`recorded exceptions` = 8 tests / 0 fail；`tree manifest (verify)` ⇒ `OK (the source tree is unchanged: 354 file(s), docs-only changes: 1)`（NOTE 如实记录本轮运行期间的文档改动）。`FALSIFY` = **0**；`git status --short` 216 项（**未提交**）。

## 2fq. 第一百八十五轮：回到"解析器不能吃掉文本" —— 两个 compose 守卫的**镜像过滤器会按名字吃掉一个服务**

### A. 先量三处候选，两处是**负结论**（如实记录）

| 候选 | 实测 | 结论 |
|---|---|---|
| `check_tenant_error_codes` 的 `status()` 调用点解析 | 第 155 轮已把"按文件里任意 `fn status` 解析"删除；今天未解析的形状走 `unverified` 路径（要么记账要么 DRIFT）⇒ **失败方向是可见的**，不是被吃掉 | 无需动作 |
| `check_alert_expressions` 的 PromQL 解析 | §9.1 表 **14 行全部含 `hydra_`**；手工抓"表达式单元格"得到 **12** 个名字，而守卫报 **14 distinct**（比手工抓的更多）⇒ 覆盖 ⊇ 手工抓取 | 无需动作 |
| **`check_compose_grace` / `check_compose_health` 的服务过滤** | 三个 compose 文件里的 hydra 服务**今天都匹配** `IMAGE_RE`（`hydra:latest` / `hydra-local:latest`，且 `docker-compose.yml` 的 `hydra` 同时有 `build:` 与 `image:`）⇒ 今天没有服务被跳过 | **但过滤本身按"镜像"取服务、按"名字"才该取**，见 B |

### B. 真缺口：过滤器**按镜像**选服务，于是一个**叫 `hydra-*` 但镜像不匹配**的服务会被**静默跳过**

两个守卫都用 `if (!IMAGE_RE.test(image)) continue;` 选服务，而 OK 行照样打印"**N hydra service(s) checked**"。也就是说：一个名字像 hydra 节点、镜像却写成（例如）`someone-elses/hydra-fork:latest`、或者**根本没写 `image:`** 的服务，它的 `stop_grace_period` / role / healthcheck **一个都不检查**，而输出读起来像"都查过了"——正是本会话反复修的那一类（**过滤器吃掉文本**，第 178 轮的 `gate` 条目、第 181 轮的 `NOT_A_METRIC` 同型）。
**修法**：
* **渲染路径**（`checkDoc` / `services` 循环）：**名字像 hydra 而镜像不匹配 ⇒ 直接产出一条 finding**（`ok:false` / `probs:[…]`，措辞写明"镜像过滤器跳过了它，所以它的 stop_grace_period / role / healthcheck 未被检查"）；
* **静态回退路径**：`check_compose_grace` 用**同款 finding**（`unfiltered.concat(...)`），`check_compose_health` 用该守卫**既有的哲学**——**拒绝继续**（`ScanError`，"the static fallback would silently skip N hydra-NAMED service(s) … render the file instead"），因为它对"读不出来的形状"本来就是拒绝而不是猜测；
* 两处都新增**只扫 `services:` 块**的文本扫描器 `hydraNamedServices()`：**我的第一版扫了整个文件**，于是 `volumes:` 下同缩进的 `hydra-a-data:` / `hydra-b-data:` 被当成"没有镜像的服务"⇒ **两个假 finding** —— 被 `check_compose_grace.test.cjs` 里既有的"未改动的本地栈应当通过"**控制组当场抓住**（这是本轮第一个自伤，见下）。
**测试 +5**（grace +2：名字像 hydra 且镜像不匹配 ⇒ finding；控制组：非 hydra 名 + 外来镜像 ⇒ 不吭声。health +2：同名用例 + 同名控制组；另有既有用例构成回归网），**grace 21/21**、**health 20/20** 全过，真树两个守卫 `exit 0`。
**两条反向证伪**：把渲染路径那条 `if (/hydra/i.test(name))` 改成永不触发 ⇒ **grace 恰好 1 条红、health 恰好 1 条红**（各自新用例），其余全绿；逐个恢复（`diff -q` 校验）后 `FALSIFY` = 0、两套复跑全绿。

### C. 本轮验证（门禁判定）

`bash .acceptance/round10-gate.sh` ⇒ **95 条条目全部 `exit=0`**、`OVERALL=GREEN`、`GATE_EXIT=0`（`entries=95` / `GATE COMPLETE`；stdout 存 `.acceptance/round185-gate.out`）。**门禁内证据**：`compose stop grace` 条目打印 `[compose-grace] drain 20s + final 5s + slack 5s = 30s required; 7 hydra service(s) checked: OK`；`compose healthchecks` 打印 `[compose-health] 7 hydra service(s) checked: OK`；`compose grace tests` / `compose health tests` / `compose static reader tests` 全部 `exit=0`（本轮各 +2 条用例）；`tree manifest (verify)` ⇒ `OK (the source tree is unchanged: 354 file(s), docs-only changes: 1)`。`FALSIFY` = **0**；`git status --short` 216 项（**未提交**）。

## 2fr. 第一百八十六轮：`check_e2e_contracts` 的 **UI 源清单**：**列了却不存在的文件**会让整条规则报几十条假 DRIFT

### A. 先量：清单与磁盘**今天完全一致**，但两个方向都**没有任何断言**

`check_e2e_contracts.cjs` 用 `UI_FILES`（6 个 `admin-ui/*`）当"UI 源全集"，然后逐条检查 e2e spec 里的 selector / nav key 是否在这些源里出现。实测（本轮）：
* 列出的 6 个**全部存在**，`admin-ui/` 下的**6 个源文件也全部在清单里**（`listed == on disk`，双向都为空）。
* 但代码是 `fs.existsSync(p) ? readFileSync(p) : ""` —— **文件不在就把空串当源**，且**没有任何 finding**。
⇒ **两个方向的后果都是静默的**：①**列了却不存在**：所有 selector 检查都去读空源 ⇒ 报出**几十条假 DRIFT**（"no admin-ui source declares id …"），而**真因（文件不见了）一个字都不提**；②**存在却没列**：只在该文件里声明的 selector / nav key **读起来像不存在** ⇒ 同样假 DRIFT。两条都属本轮主题："**清单/过滤器吃掉对象**"。

### B. 修法：把清单本身变成**前置条件**（并用共享的记录表）

1. **列出的文件必须存在** ⇒ 否则一条 finding 点名："`admin-ui/i18n.js` is listed in UI_FILES but does not exist: every selector/nav check below would read an empty source and report false drift instead of naming the missing file"；
2. **`admin-ui/` 下的每个源文件（`.js/.cjs/.mjs/.ts/.html/.css`）要么在 `UI_FILES`、要么记录在 `UNSCANNED_UI_OK`（名字 → 理由）** ⇒ 未记录即 finding；**记录过期**（文件没了、或已被列进 `UI_FILES`）也 finding —— 这里**直接复用第 183 轮的共享模块** `recorded_exceptions.cjs`（`records()`/`audit()`），所以"未记录"与"过期"两个方向与另外五个守卫**同一套定义**；
3. 为了可测，新增根覆盖 `E2E_CONTRACTS_ROOT`（与其它守卫一致）。
**测试 +4**（未列且未记录 ⇒ 报；控制组：**已记录** ⇒ 不报且记录不算过期；**列了却缺失** ⇒ 报出"那一个真事实"；**记录过期**（文件已在清单里）⇒ 报），套件 `ALL E2E CONTRACT TESTS PASSED`（36 条 PASS）；真树 `exit 0`。
**反向证伪**：把两条规则同时关掉 ⇒ **恰好那两条新用例红**，而且失败信息**逐字复现旧行为**：`DRIFT … selector "\#login-overlay" needs id "login-overlay", which no admin-ui source declares` —— 即"文件缺失 → 几十条假 DRIFT"就是我修掉的症状本身。恢复后 `diff -q` 一致、`FALSIFY` = 0、套件复跑全过。

### C. ★同一个自伤**连犯两轮**（第 183 轮一次、本轮又一次）

两次都是**给自定义 harness 的套件追加用例时，把代码加在 `process.exit` 之后** ⇒ 两次都被 `check_test_tails` 当场抓住（本轮：`DEAD scripts/check_e2e_contracts.test.cjs:396 — 66 line(s) of code sit AFTER the top-level exit at line 390`）。**纪律**（写进队列）：**给这类套件追加用例后，第一件事是跑 `node scripts/check_test_tails.cjs`**，而不是先跑套件本身 —— 套件会打印 `ALL … PASSED`（它的摘要只看 `failures` 计数），**死代码在它眼里是不存在的**，这正是一条"守卫抓守卫"的必需链路。

### D. 本轮验证（门禁判定）

`bash .acceptance/round10-gate.sh` ⇒ **95 条条目全部 `exit=0`**、`OVERALL=GREEN`、`GATE_EXIT=0`（`entries=95` / `GATE COMPLETE`；stdout 存 `.acceptance/round186-gate.out`）。**门禁内证据**：`e2e contract tests` 条目打印 **40 条 PASS**（本轮 +4）并收尾 `ALL E2E CONTRACT TESTS PASSED`；`e2e contracts` / `recorded exceptions` 均 `exit=0`；`tree manifest (verify)` ⇒ `OK (the source tree is unchanged: 354 file(s), docs-only changes: 1)`（NOTE 如实记录本轮运行期间的文档改动）。`FALSIFY` = **0**；`git status --short` 216 项（**未提交**）。

## 2fs. 第一百八十七轮：`check_source_purity` 的**crate root 清单**漏掉 cargo **自动发现**的二进制

### A. 缺口：`src/bin/<name>.rs` 会被扫违规，却**不要求**带 `#![forbid(unsafe_code)]`

这个守卫管两件事：①扫 `crates/*/src` 里的生产代码（`unsafe` / `unwrap()` / panic 宏）；②**每个 crate root 都必须带 `#![forbid(unsafe_code)]`**。第 ② 条的"root 全集"是这么算的：`src/lib.rs` + `Cargo.toml` 里 `[[bin]]` 的 `path`，然后 OK 行打印 `#![forbid(unsafe_code)] present on every crate root (…3 个…)`。
**漏的是什么**：cargo 会**自动发现** `src/bin/<name>.rs`（以及 `src/bin/<name>/main.rs`）为二进制目标——**不需要任何 `[[bin]]` 段**。这样的文件当然在 ① 的扫描范围内（它在 `src/` 下），但 ② **不把它当 root** ⇒ **一个新增的自动发现二进制可以不带这条 lint 就进仓库，而 OK 行照样宣称"每个 crate root 都有"**。
**实测**：本树今天**没有** `src/bin/`（唯一的 bin 是显式 `[[bin]] path = "src/main.rs"`，且三个 root 都带 lint）⇒ **洞是潜在的**——正因如此，本轮**按构造**关掉它，而不是靠"记得更新一张清单"。

### B. 修法（按构造，不靠清单）

`crateRoots()` 里在解析 `[[bin]]` 之后补上 cargo 的自动发现：枚举 `src/bin/*.rs`（文件）与 `src/bin/<name>/main.rs`（目录形式），**按名字排序**（OK 行的列表因此稳定），非 `.rs` 文件不算（控制组覆盖）。**测试 +4**：自动发现的二进制**不带 lint ⇒ FAIL 且点名 `src/bin/extra.rs`**；控制组：**带 lint ⇒ 通过**；`src/bin/<name>/main.rs` 嵌套形式 ⇒ 同样被要求；控制组：`src/bin/README.md` ⇒ **不算 root**（不误报）。套件 **30/30**（26 → 30），真树 `[purity] … 3 crate root(s): clean` 与 `OK: … every crate root (…)` 不变。
**反向证伪**：把自动发现那段改成永不执行 ⇒ **恰好那两条"必须带 lint"的用例红**（控制组仍绿）；恢复后 `diff -q` 一致、`FALSIFY` = 0、30/30 复跑全过。

### C. ★纪律执行到了（与上一轮的自伤对照）

上一轮记录了"给自定义 harness 套件追加用例后会落在 `process.exit` 之后"，本轮**追加完先跑 `check_test_tails`** 再跑套件 —— 它绿，且套件 30/30 证明**追加的用例是活的**（这套是 `node:test` 型，无 `process.exit`，所以末尾追加本就安全；纪律的价值在于**不必每次重新判断是哪种套件**）。
**另一个本轮踩到的小坑（第三次同型，但换了介质）**：注释里写了 `` `src/bin/*/main.rs` `` —— 其中的 **`*/` 直接终结了 JS 块注释**，于是 `node --check` 报 `SyntaxError: Unexpected identifier 'src'`（第 176 轮是 Python 文档字符串里的 `/* /* */ */` 触发过同类）。**教训**：注释里**不要出现 `*/` 序列**（写成 `src/bin/<name>/main.rs`），改完**先 `node --check`**。

### D. 本轮验证（门禁判定）

`bash .acceptance/round10-gate.sh` ⇒ **95 条条目全部 `exit=0`**、`OVERALL=GREEN`、`GATE_EXIT=0`（`entries=95` / `GATE COMPLETE`；stdout 存 `.acceptance/round187-gate.out`）。**门禁内证据**：`source purity tests` 条目 = **30 tests / 0 fail**（本轮 +4）；`source purity` 条目打印 `[purity] scanned 63 file(s) under crates/*/src …; 3 crate root(s): clean` 与 `OK: #![forbid(unsafe_code)] present on every crate root (…)`（列表与修前一致 ⇒ 真实树的行为未变，新增的是**构造上的覆盖**）；`tree manifest (verify)` ⇒ `OK (the source tree is unchanged: 354 file(s), docs-only changes: 1)`。`FALSIFY` = **0**；`git status --short` 216 项（**未提交**）。

## 2ft. 第一百八十八轮：`check_i18n.js` 的**语言清单是硬编码的** —— 新增一门语言会被**静默不检查**

### A. 缺口：`for (const lang of ["zh", "fr", "de"])`，而 UI 自己声明 `LANGUAGES`

`admin-ui/i18n.js` 自己声明了 `LANGUAGES = [{code:"zh"},{code:"en"},{code:"fr"},{code:"de"}]`（UI 用**对象数组**），而 `scripts/check_i18n.js` 校验"en 的每个 key 在其它语言里都存在"时用的是**写死的 `["zh","fr","de"]`**。⇒ **UI 里新增一门语言（例如 `es`）不会被检查**，而 OK 行还打印「**4 locales**」——又是本会话反复修的"**硬编码清单吃掉对象**"（第 187 轮 cargo 自动发现的二进制同型）。实测：今天 UI 恰好 4 门（en/zh/fr/de），所以洞是**潜在**的。

### B. 修法：语言集**从 UI 发现**，并把三种集合关系变成 finding

1. `loadI18n()` 的沙箱返回值加上 `LANGUAGES`（UI 的声明）；
2. **检查集改为** `Object.keys(I18N)` 去掉源语言 `en`（并把 `SOURCE_LANG = "en"` 提成常量）⇒ **任何**新增语言**自动**进入检查；
3. 新增两条集合关系（都是本轮同类缺口的自然补集）：**`LANGUAGES` 声明了却没有字典** ⇒ `LANGUAGE-DECLARED-NO-DICTIONARY`；**有字典但 `LANGUAGES` 从不提供** ⇒ `DICTIONARY-NOT-SELECTABLE`；
4. OK 行的「N locales」改为**算出来**（并列出语言代码），不再写死。
**实测**：真树 `OK (348 en keys, 4 locales (en, zh, fr, de), code↔en consistent; 4 UI file(s) scanned, 139 t() literal(s) checked)`。
**测试 +4**（**新增且完整**的第五门语言 ⇒ 被检查且计数为 5；**新增语言的某个 key 缺失** ⇒ `MISSING es  a.b` 报出来（**修前它会静默通过**）；`LANGUAGES` 声明无字典 ⇒ 报；有字典但 UI 不提供 ⇒ 报）；夹具需要一个可变语言表的构造器 `makeLocaleFixture()`（既有 `makeFixture` 固定四门）；套件 `ALL CHECK_I18N TESTS PASSED`，真树 `exit 0`。
**反向证伪**：把检查集改回写死的 `["zh","fr","de"]` ⇒ **恰好那两条"第五门语言"用例红**，而且失败输出**逐字复现旧症状**：对一个真的声明了 5 门（含不完整的 `es`）的树打印 `OK (1 en keys, **4 locales (en, zh, fr, de)**, …)` —— 即"静默通过 + 谎报语言数"。恢复后 `diff -q` 一致、`FALSIFY` = 0、套件复跑全过。

### C. 本轮验证（门禁判定）

`bash .acceptance/round10-gate.sh` ⇒ **95 条条目全部 `exit=0`**、`OVERALL=GREEN`、`GATE_EXIT=0`（`entries=95` / `GATE COMPLETE`；stdout 存 `.acceptance/round188-gate.out`）。**门禁内证据**：`i18n` 条目打印 `OK (348 en keys, 4 locales (en, zh, fr, de), code↔en consistent; 4 UI file(s) scanned, 139 t() literal(s) checked)`（**语言集现在是算出来的**）；`i18n tests` 条目 = `ALL CHECK_I18N TESTS PASSED`（本轮 +4 条用例）；`tree manifest (verify)` ⇒ `OK (the source tree is unchanged: 354 file(s), docs-only changes: 1)`。`FALSIFY` = **0**；`git status --short` 216 项（**未提交**）。

## 2fu. 第一百八十九轮：`check_i18n` 的**扫描文件类型是按名字写死的**（`*.js` + `*.html`）—— 同一族的第二个洞

### A. 缺口：**语言**换了，扫描就看不见了

上一轮修掉"语言清单硬编码"，本轮把同一条判据（**清单/过滤器不得吃掉对象**）用在**扫描的文件类型**上：那段的规则是 `*.js`（除 `i18n.js`）+ `*.html`，**按扩展名写死**。⇒ 一个用别的源语言写的 UI 模块（`.mjs`/`.cjs`/`.jsx`/`.ts`/`.tsx`，或任何本仓库将来采用的形式）**完全不被读**：它里面 `t("brand.new.key")` 既不参与"key 必须在 en 里"，也不参与"en 里每个 key 必须被引用"——**静默漏检**。（该段注释已经记着上一代同类洞：早期只读顶层目录，子目录里的模块同样不可见。）实测：今天 `admin-ui/` 下只有 `.js`/`.html`/`.css`，所以洞是**潜在**的。

### B. 修法：扩展名集合**显式化** + **目录审计**（未扫描的文件必须**记录**）

1. `SCANNED_EXTENSIONS = /\.(js|mjs|cjs|jsx|ts|tsx|html)$/`（**显式**列出 UI 模块可能的源语言，而不是靠"后缀恰好是 .js"）；
2. **目录审计**：`admin-ui/` 下**每个文件**必须是 **①被扫描** 或 **②记录在 `UNSCANNED_UI_OK`（名字 → 理由）**；未记录 ⇒ `UNSCANNED-UI-FILE` finding，**记录过期**（文件没了、或已变成可扫描）⇒ `STALE-UNSCANNED-RECORD` finding —— 复用第 183 轮的 **`recorded_exceptions.cjs`**（又是"未记录/过期"同一套定义）；
3. 内置记录今天只有一条：`style.css` ——「样式表：只承载类名与布局，不含 `t()` 调用」；`i18n.js` 按**名字**豁免（它是字典本身）；
4. 两个新 issue 类型**加打印分支**（第一版只把 issue 计数带进退出码、**不打印** ⇒ 我的新用例因此看不到文件名，**被自己的用例当场抓到**）。
**实测**：真树 `OK (348 en keys, 4 locales (en, zh, fr, de), code↔en consistent; 4 UI file(s) scanned, 139 t() literal(s) checked)` 不变。
**测试 +5**：**不可扫描的语言**（`.vue`）未记录 ⇒ `UNSCANNED-UI-FILE widget.vue`；控制组：**已记录** ⇒ 通过且记录不算过期；**记录过期**（文件不存在）⇒ `STALE-UNSCANNED-RECORD gone.vue`；**`.tsx` 现在确实被扫**（其 key 缺失 ⇒ `CODE-REF-NO-EN brand.new.key`）；控制组：普通形状（`.js` + `.html` + `i18n.js`）**无需任何记录**。夹具需要能**替换**记录表 ⇒ `I18N_UNSCANNED_UI`，测试骨架默认 `{}`（否则夹具里那条 `style.css` 记录会被判过期 —— 这个坑本轮又撞了一次，5 条既有用例同时红）。套件 `ALL CHECK_I18N TESTS PASSED`，真树 `exit 0`。
**反向证伪**：把目录审计的 `needed` 置空 ⇒ **恰好那条"不可扫描语言"用例红**，输出**逐字复现旧行为**：`OK (1 en keys, 4 locales (en, zh, fr, de), …, 1 UI file(s) scanned, …)` —— 一个含 `.vue` 模块的树被判"OK"。恢复后 `diff -q` 一致、`FALSIFY` = 0、套件复跑全过。


### C. 本轮验证（门禁判定）

`bash .acceptance/round10-gate.sh` ⇒ **95 条条目全部 `exit=0`**、`OVERALL=GREEN`、`GATE_EXIT=0`（`entries=95` / `GATE COMPLETE`；stdout 存 `.acceptance/round189-gate.out`）。**门禁内证据**：`i18n` 条目打印 `OK (348 en keys, 4 locales (en, zh, fr, de), code↔en consistent; 4 UI file(s) scanned, 139 t() literal(s) checked)`（真树结论未变 ⇒ 新增的是**构造上的覆盖**）；`i18n tests` 条目 = `ALL CHECK_I18N TESTS PASSED`（本轮 +5 条用例）；`tree manifest (verify)` ⇒ `OK (the source tree is unchanged: 354 file(s), docs-only changes: 1)`。`FALSIFY` = **0**；`git status --short` 216 项（**未提交**）。

## 2fv. 第一百九十轮：一个**从未被调用**的 helper —— 它想做的事**不可靠**，于是**删掉并留下实测理由**（顺带两处负结论）

### A. 先量两个候选，都是负结论（如实记录）

* **`check_alert_expressions` 的章节锚点**（`### 9.1 Alerting`）：找不到该表时**已经**是显式失败（`cannot find the \`### 9.1 Alerting\` table in …`）⇒ 标题被改写不会静默通过；
* **`check_compose_grace`/`health` 的 `COMPOSE_FILES`**：缺文件会让 `docker compose config` 失败并走静态回退，缺 `secure/local-test.env` 有专门分支 ⇒ 也不是静默；
* **`check_public_claims` 的日期纪律**：我怀疑又是一枚"时间炸弹"（页面写着 `2026-09-30`，今天 `2026-10-01`），读代码后确认**不是**：**旧日期只打印 NOTE**（第 126/134 轮的结论：日期是"最后一次测量"的记录，陈旧值得刷新但不是谎言；只有**未来日期**与 **`--measure` 当天不符**才是硬失败）⇒ 守卫的行为与它头部的说明一致，**无需改动**。

### B. 真发现：`assertedStringsIn()` 是**死代码**，而它想支撑的规则**经不起实测**

按"守卫自己的常量/函数是否有死件"扫了一遍 `scripts/*.cjs`：**常量零死件**，但**函数有一个**：`check_e2e_contracts.cjs` 里的 `assertedStringsIn()`（收集 spec 里 `toContainText('…')` 的字面量）**全仓只有定义、没有任何调用**。看起来它本该支撑一条"spec 断言的可见文本必须存在于 UI 源里"的规则 —— **实测这条规则不成立**：三个 spec 共 **12 条**字面断言，其中 **4 条**是**spec 自己在运行时创建的值**（`node-b`、`pw-upstream.example.com`、`auth.pw.example.com`），它们**既不在 UI 源、也不在 `tests/e2e/seed-data.json`** ⇒ 任何静态版本都会在**正确的套件上报假 DRIFT**。
**处置**：**删掉这个 helper**，并在原处留下**实测数字与理由**（"12 条断言、4 条运行时值 ⇒ 静态规则会假报，故不实现"），再在 `check_e2e_contracts.test.cjs` 里加一条**决定钉子**：一份含"运行时创建的断言值"的 spec **不得**被判 DRIFT —— 将来若有人把这个诱人的规则接上，**先红的是这条用例**。
**反向证伪**：临时把那条朴素规则按正确作用域加回去（`problems` 声明处）⇒ 输出**当场复现假 DRIFT**：`DRIFT admin.spec.cjs: asserts text no UI source carries: node-b`（并连带打红"真实 spec 通过"等用例）；删除后 `diff -q` 一致、`FALSIFY` = 0、套件 `ALL E2E CONTRACT TESTS PASSED`。
**方法论**：这是本会话第一次**主动删掉**一个"看起来有价值"的守卫能力 —— 判据与前面几轮同源：**一条规则的价值取决于它能否在不正确的输入上保持沉默**；实测证明它不能，于是它不该存在，而且**它不在的理由必须留下**（否则下一个人会重新写一遍）。


### C. 本轮验证（门禁判定）

`bash .acceptance/round10-gate.sh` ⇒ **95 条条目全部 `exit=0`**、`OVERALL=GREEN`、`GATE_EXIT=0`（`entries=95` / `GATE COMPLETE`；stdout 存 `.acceptance/round190-gate.out`）。**门禁内证据**：`e2e contract tests` 条目打印 **41 条 PASS**（本轮 +1：那条"运行时值不是 drift"的决定钉子）并收尾 `ALL E2E CONTRACT TESTS PASSED`；`e2e contracts` = `exit=0`；`tree manifest (verify)` ⇒ `OK (the source tree is unchanged: 354 file(s), docs-only changes: 1)`。`FALSIFY` = **0**；`git status --short` 216 项（**未提交**）。

## 2fw. 第一百九十一轮：★**一个拓扑开关拼错就静默降级** —— `HYDRA_REDIS_MODE=clustr` 让 edge 以 single 模式起来了

### A. 起点：把 `ops.md` §13.7「已知限制」逐条对照**是否有测试钉住**

§13.7 是一份"已知限制"清单，每条都是**对外承诺**（"fail fast 而不是静默乱来"）。逐条量：`BREAKER_THRESHOLD` → `test_breaker_lifecycle.py` ✓、`PROBE_INTERVAL` → `test_breaker_probe_revival.py` ✓、`fail_mode` → `test_auth_hop_failmode.py` ✓、`hydra_replica_materialize_retries_total`/`hydra_invalidation_generation_bumps_total` 两条"看这个指标"的告警指示 → 指标都**已注册**（`metrics.rs:493/504`，前者我第一次 grep 用错了 token 才以为找不到）✓；**`sentinel` 一词在测试里零出现** ⇒ 「Redis sentinel/cluster 部署模式 **fail fast**（只接线了 single）」这条**没有任何测试**。
顺着它读代码，发现**文档与代码不一致**：`RedisMode::from_env()` 是
```rust
match std::env::var("HYDRA_REDIS_MODE").as_deref() {
    Ok("sentinel") => Self::Sentinel,
    Ok("cluster") => Self::Cluster,
    _ => Self::Single,          // ← 任何别的值（含拼错）都静默变成 single
}
```
而**同一个枚举的文档注释**写着 "they currently fail fast at startup **rather than silently misbehaving**"。

### B. **线上实测两个方向**（用真实二进制 + 真实 Redis URL，逐条看日志）

* `HYDRA_REDIS_MODE=clustr`（少一个 e）⇒ **节点照常启动**：`provider-key encryption enabled` → `config store loaded` → `node registered in the cluster registry node_id=…` → `auth checker initialised`；**没有一条日志提到这个开关**。一个**拓扑**开关被拼错，节点以 single 模式在集群里注册了自己。
* `HYDRA_REDIS_MODE=sentinel` ⇒ `fatal startup error error=unsupported HYDRA_REDIS_MODE 'sentinel' (supported: single)`（这一半是文档承诺的，也是**没有测试**的那一半）。

### C. 修法 + 钉住（两侧都有）

1. **把解析拆成纯函数** `RedisMode::parse(Option<&str>) -> Result<Self, RedisError>`，语义：`None`/空/`single`（**大小写不敏感**，`SINGLE`/` single ` 也接受）⇒ `Ok(Single)`；**其它任何非空值** ⇒ `Err(UnsupportedMode { mode: <操作者原样写的字符串> })` —— 错误信息引用**原输入**而不是折叠后的副本（第一版返回折叠值，被我自己那条断言当场抓到）。
2. `from_env()` 变成 `parse(std::env::var(…).ok().as_deref())`，`main.rs` 的调用点加 `?` ⇒ **解析即启动失败**（不再等到连 Redis 才失败）。
3. **单元测试 +2**（用纯函数，不碰进程环境、不会有并行竞态）：已知值 6 种（含大小写/空白）⇒ `Single`；**6 个非已知值**（`sentinel`/`cluster`/`clustr`/`SENTINEL`/`snetinel`/`banana`）⇒ `Err(UnsupportedMode)` 且 `mode` 等于原输入。**实测（本地 6380 上的真 Redis）**：`cargo test -p hydra-server --features server,cluster-redis --lib redis::` ⇒ **24 passed / 0 failed**（未设 `HYDRA_TEST_REDIS_URL` 时那 17 个环境门控用例会失败，与本改动无关 —— 门禁的 optional-features 条目会带上该变量）。
4. **文档同步**：`ops.md` 的环境表行与 §13.7 那条改成"**sentinel/cluster 与任何其它值都 fail fast**，`single`/`SINGLE`/未设置才被接受"，并写明这就是原来那句"fail fast rather than silently misbehaving"该有的样子；`cluster.md` 的表格同步。
5. **线上复测**（修复后，同样三条）：`clustr` ⇒ `fatal startup error error=unsupported HYDRA_REDIS_MODE 'clustr' (supported: single)`（**从"静默启动"变成"拒绝启动"**）；`sentinel` ⇒ 同样的拒绝（未变）；`single` ⇒ 正常启动（控制组）。

### D. 反向证伪

把 `parse` 的兜底改回 `Ok(Self::Single)`（即修复前的行为）⇒ **恰好那条"拒绝一切未知值（含拼错）"的用例红**，而"已知值可接受"的控制组仍绿；恢复后 `diff -q` 一致、`FALSIFY` = 0。

### E. 队列（下一个自然落点）

**把这条行为也钉到"线上"**：一个新 drill 腿（或现有 drill 的一条腿）用真二进制 + `HYDRA_REDIS_MODE=clustr` 断言**进程非零退出**且日志含 `unsupported HYDRA_REDIS_MODE 'clustr'` —— 本轮已用**手工实测**给出这三个方向的证据（`clustr` / `sentinel` / `single`），但还没有自动化入口；做成 drill 要新增门禁条目与 CI 接线，故记入队列（判据：`must()` 式的"启动必须失败 + 日志点名开关"）。


### F. ★门禁又一次抓到我自己：第一次跑 RED，原因是 `cargo fmt --check`

本轮第一次门禁 **RED**，唯一非零是 **`fmt --check exit=1`** —— 我新写的两条单元测试里 `for value in [ … ]` 单行超长，`rustfmt` 要拆行。**修法**：`cargo fmt` 后 `fmt --check` 干净、两条测试复跑通过、`FALSIFY` = 0，然后**以最终修订重跑门禁**。
两点值得记：①门禁的第一条就是 `fmt`，所以它**在任何测试之前**就把问题拦住了（"先便宜后昂贵"的排序起作用）；②这再次证明**改完 Rust 必须自己先 `cargo fmt --check`**，而不是等门禁跑 40 分钟才发现（本轮代价就是多跑一次门禁）。

### G. 本轮验证（门禁判定）

以**最终修订**（`cargo fmt` 之后）重跑整条门禁：`.acceptance/round191b-gate.out`。

- 条目数 **95**，`grep -c '^GATE '` = 95，`grep 'exit=[1-9]'` **零命中**（即**没有一条**非零），末行 `OVERALL=GREEN` / `GATE_EXIT=0`。
- 首条 `tree manifest (record) exit=0`、末条 `tree manifest (verify) exit=0`；本轮**只有文档**在跑门禁期间被改（§2fw 的 A–F），所以 verify 打印的是 `NOTE: 1 doc file(s) changed during the run … no code moved`（`OK (the source tree is unchanged: 354 file(s), docs-only changes: 1)`）——**代码在跑门禁期间零改动**，这正是上一轮那句"改完代码不要再动"的反面证据。
- 关键三条：`fmt --check exit=0`（上一轮 RED 的那条，本轮转绿）、`test optional + live redis/CH exit=0`、`admin-ui e2e (browser, 18180) exit=0`。
- **新增用例确实在门禁里跑了**（不是"本地跑过"）：`test optional + live redis/CH` 的日志里出现
  `test redis::tests::redis_mode_parses_the_known_values ... ok` 与
  `test redis::tests::redis_mode_refuses_everything_else_including_typos ... ok` —— 这一条是**这个门禁最重要的性质**：门禁再绿，如果它没跑到新用例，就只是"没坏"而不是"被验"。

⇒ 本轮判定：**GREEN（95/95）**，且**修复被门禁内的用例钉住**。

### H. 下一轮队列（本轮新发现，含可行性已探明的一条）

1. ✅ **已做（第一百九十二轮，见 §2fx）**：`HYDRA_REDIS_MODE` 的 fail-fast 现在有**自动化入口** —— `integration/test_startup_knobs.py` 的 **K1/K2**（真二进制 + 真 Redis：非零退出 + 日志点名 `'clustr'`/`'sentinel'`），并有 **K3 控制组**（`single` 时同一个 edge 正常启动）。原队列文字保留如下供追溯（第一百九十一轮 §2fw.E）：**把这条行为也钉到"线上"**：一个新 drill 腿（或现有 drill 的一条腿）用真二进制 + `HYDRA_REDIS_MODE=clustr` 断言**进程非零退出**且日志含 `unsupported HYDRA_REDIS_MODE 'clustr'`；做成 drill 要新增门禁条目与 CI 接线，判据是 `must()` 式的"启动必须失败 + 日志点名开关"。
1. ✅ **已做（第一百六十九轮，见 §2fa）**：`test_replica_fidelity.py` 的 §13「disabled 行」半边现在有真正的观测腿（F8 读 **leader 发布侧**的 wire）。原队列文字保留如下供追溯：**`test_replica_fidelity.py`：§13「disabled 行 / `provider_key` 身份」那一半可以做成真正的观测腿**。现状（头部如实标注「no live observable from an edge」）是因为 **edge 没有管理面**；但被声称的东西是**发布侧的 wire 契约**，而它有直接的 HTTP 观测点：**leader** 上的 `GET /api/v1/internal/control?since=0`（cluster token 保护）返回 `SnapshotWire` 的 JSON，其中
   `snapshot.fidelity.limit_roles`（`cluster/snapshot.rs` 明确写 **FULL `limit_role` rows, disabled ones included**）、
   `fidelity.key_prefix_bindings`（**FULL `provider_key_binding` rows, disabled ones included**，携带 `provider_key` 身份）、
   `fidelity.tenant_token_hashes`（密封哈希）、`fidelity.provider_models`（**含 offline 模型**，`status != 1`）。
   ⇒ 一条腿即可断言「首启为 **disabled** 的角色/绑定**确实出现在 leader 发布的载荷里**」（drill 里 `r-off` / `b-off` 夹具现成，今天只作形状用途）。注意 `SnapshotWire` 是 `deny_unknown_fields`，形状稳定；实现时**不得打印密封材料**（只断言行数/身份字段）。
2. ✅ **已做（第一百七十轮，见 §2fb）**：新增「形状 4」（`__main__: main()` 且 `main()` 每条路径都 exit），判定数 **44 → 45**（多出来的正是 `integration/test_crud.py`），并顺带改掉测试注释里「`test_crud.py` 就是这样写的」这条**错误事实**。原队列文字保留如下供追溯：**`check_test_tails`：65 个 suite 里只有 44 个被判** ⇒ **第一百六十九轮已量化并降级（见 §2fa.D）**：21 个未判文件里 **17 个根本没有 exit 调用**（不判是对的）、**3 个** node 自建 harness 的 exit 是**条件出口**（不判也是对的）、**真缺口只有 1 个**（`integration/test_crud.py` 的 `__main__: main()` 一跳形状）。修它要新增「一跳」模型，且有**假阳性风险**（被调函数能正常 `return` 时块后代码会执行），需先写控制组用例。
3. **`.acceptance/` 不在版本控制（D-5）的具体代价本轮又增加两条**：§4 里 3 条不被任何自动入口执行的命令中，有 2 条住在 `.acceptance/`（`round63/measure.sh`、`phase-b-gate.sh`）——**它们没法变成别人能跑的门禁条目**。
4. **`ci.yml` 未提交**（实测：工作树 8 个作业 / `HEAD` 4 个）⇒ 本文件所有「CI 会跑 X」的说法都**只在提交后成立**；`check_ci_wiring` 只保证「工作树里的接线正确」。未提交是用户的要求，记在此处以免误读。
6. ✅ **已收口（第一百七十二轮，见 §2fd.E）**：38 组分类，唯一真缺口是 `test_model_catalog.py` C6 的子租户那一半（而且它是**恒真**，不是「前提未验」），已修；`test_sub_tenant_steering.py` S3 由 S2 的正向腿兜底。原队列文字保留如下供追溯：**drill 里「丢弃返回值」的调用共 85 处 / 31 个文件（第一百七十一轮实测分类，见 §2fc.C），第一百七十一轮只修了其中唯一一处"缺席型断言 + 前提未验"的真缺口**；**53 处 `POST /reload` 丢弃未逐条审计**。下一轮若继续这条线，判据是：**断言为"缺席/未生效/仍然 200"的腿，其 setup 写必须读回提交态**（本轮已修的那一处就是样板：`test_snapshot_stale.py` 的 S1 PREMISE）。不要改成"禁止丢弃返回值"的守卫 —— 85 处里绝大多数是**失败可见**的（重载失败 ⇒ 配置为空 ⇒ 腿红），一律要求断言只会制造噪音。**下一轮的现成清单**（本轮已用机械筛法产出：丢弃的重载之后 40 行内出现「缺席/否定」味道的 check 标签，共 **28 组**）。**最值得先看的两组**（因为"配置为空"正好满足它们的断言）：
   * `test_sub_tenant_steering.py` S3「a model the sub-tenant has NO route for **falls back to the tenant default**」（reload@238 → check@247）—— 子租户/路由行**根本没加载**时，回退同样发生 ⇒ 断言照样绿；
   * `test_model_catalog.py` C6「a model the sub-tenant has NO route for **is not restricted** by it」（reload@292/307 → check@314）—— 同上：限制行不存在时"不受限"也成立。
   共同判据：**这类腿的前提应是「被否定的那条行确实已加载」**（可由同 drill 的一条**正向**腿兜底，也可读回配置或 `GET` 端点）。若同 drill 已有正向腿覆盖同一批行，则只需在注释里点明依赖，不必新增断言。
7. **「标签比判据强」的机械筛选已完成一轮（第一百七十三轮，见 §2fe.A）**：扫描器 `.acceptance/round173-label-sweep.py`（强断言词 ⨯ 无比较/集合操作的判据）在真树命中 **29 处**，收窄到完整性词后 **24 处**，逐条读过 —— **2 处真缺陷已修**（N4 / C2），**其余 22 处合理**（`max_depth <= 3`、`set(tags) == {...}`、时间窗上下界、穷举比较算出的布尔如 `not snap_missing`）。**下一轮若要继续这条线**：换个维度扫 —— 「判据里出现 `any(`/存在性 而标签含"counted/measured/attributed"」这一类（本轮 C2 就是它，已修；同类可继续机械筛），以及「drill 的**前置断言**只读了 HTTP 状态码而没读**运行中的状态**」。
9. ✅ **两个"下一轮维度"已在第一百七十五轮跑完并收口（负结论，见 §2fg.A）**：维度 (b)「`must`/状态码之后无读回」命中 38/46 **基本是 `seed()` 噪音**（真缺口 0：`test_sub_tenant_steering.py` S5 的前提由 `must` 与 S2 正向腿兜底）；维度 (a)「标签谈时间/顺序」命中 53 处**基本是 `still` 带来的噪音**（真缺口 0）。**三个判据维度合计命中 126 处、真缺口 4 处（Q1/L5/C2/N4/C6 同族），已全部修掉。** 若还要继续这类审计，判据必须**双条件**（例如"`must` 之后 30 行内紧跟**缺席型** check 且无读回"），否则信噪比过低 —— 这句话本身就是本轮两次筛选的实测结论。
8. **「存在性判据冒充'被计数'」这一维度也筛过一轮（第一百七十四轮，见 §2ff.A）**：`.acceptance/round174-any-sweep.py` 命中 **6 处**，**2 处真缺口已修**（Q1 / L5），**4 处合理**（Q4 已是强形式；`breaker_lifecycle` 的标签本身是"还没有计数"；`test_crud` 是字段存在；`master_key_sources` 是启动失败）。**下一轮可换这两个维度**：(a) 标签含**时间/顺序**词（first / before / then / mid-request）而判据只比较**最终状态** —— 与第 168 轮 D2「first reason」同型；(b) drill 的**前置断言只读 HTTP 状态码**（`must()` / `if st not in (200,201)`）而后续腿依赖**运行中状态**（快照/配置/DB 行）—— 第 171/172 轮各修过一例（`test_snapshot_stale` S1、`test_model_catalog` C6），可用「`must(` 之后 30 行内是否存在**读回**语句」机械筛。
5. **一条考虑过但决定不做的守卫（附实测理由）**：drill 之间的**端口复用**（42 处共享端口，例：`test_body_cap_drain.py` 与 `test_sdk_ts_live.py` 共用 18760/18761）**不加守卫** —— 门禁是**串行**的，多数共享是**同一个 mock 上游端口**的有意复用（`18999` 被 8 个 drill 用），而 `check_api_docs.py`/`check_error_contract.py` 这类**静态检查器根本没有服务器**（数字只出现在文本里）⇒ 这类守卫的信噪比太低。（真正需要它的是「并行跑多个 drill」，本项目没有这个用法；本轮 e2e 条目改用显式 18180/18181 属于**顺手**，不是因为存在冲突。）
---
## 2fx. 第一百九十二轮：★**同一族、方向相反** —— `HYDRA_ROLE` 未设 + 配了集群接线 ⇒ **一句日志都没有**；顺带把上一轮的 fail-fast 钉成 drill

### A. 起点：把「本轮修好的那条」变成自动入口，却先量到了它的孪生兄弟

§2fw.E 的队列项说：`HYDRA_REDIS_MODE` 的 fail-fast 只有**手工实测**，要做成 drill。按纪律**先量再写**：直接读日志的**分类结构**（`cluster/mod.rs::from_env`），而不是去抄上一轮的手工命令。这一读就发现同一段 20 行里还有一条**同族、方向相反**的缺口。

先把上一轮的行为复测一遍（真二进制 `target/debug/hydra`，`HYDRA_ROLE=edge` + 真 Redis 6380）：

* `HYDRA_REDIS_MODE=clustr` ⇒ `exit=1`，`fatal startup error error=unsupported HYDRA_REDIS_MODE 'clustr' (supported: single)`（**第一百九十一轮的修复在线上成立**）；
* `HYDRA_REDIS_MODE=sentinel` ⇒ 同样拒绝（文档一直承诺的那一半）；
* `HYDRA_REDIS_MODE=single` ⇒ 正常启动（**控制组**）。
* 附带一条**边界事实**：`HYDRA_REDIS_MODE` **只在 `role.is_cluster()` 时才被读取**（`main.rs`：`if role.is_cluster() { RedisBackend::connect(redis_url…, RedisMode::from_env()?) }`）⇒ 单节点部署里 `HYDRA_REDIS_MODE=clustr` **不校验也不报错**。这是**有意**的（该开关在单节点下无意义，且"配了集群接线却没设角色"这一情形已由 §2fx.B 的修复变成 ERROR），因此**不改**：写成负结论而不是顺手加一条无条件校验（那会让一个无关的遗留变量**拒绝启动**）。

### B. 真发现：诊断挂在**其中一条路径**上，而不是挂在**条件**上

`NodeRole::from_env()`（`crates/hydra-server/src/cluster/mod.rs:61`）的结构是：

```rust
match std::env::var("HYDRA_ROLE").as_deref() {
    Ok("leader") => Self::Leader,
    Ok("edge") => Self::Edge,
    Ok(other) => { /* 查 ignored_cluster_wiring ⇒ Some ⇒ error! 点名被丢弃的变量；None ⇒ warn! */ Self::All }
    Err(_) => Self::All,          // ★ 完全没有日志
}
```

而它自己的文档注释（第 48–59 行）写的是：回退**允许**发生，但**不允许安静**，因为 `main.rs` 里每一条集群专属校验都挂在 `is_cluster()` 上，一个**本想当 leader** 的节点回退成 `all` 会"启动成功"却：跳过那四条校验、把租户写进**自己的**本地 SQLite（下一次 `restore_config` 会覆盖它）、永不参与选举、`/healthz/leader` 恒 404（K8s 滚动更新会**永远挂住**）。这段话**逐字**适用于 `Err(_)` 分支 —— 但诊断只挂在 `Ok(other)` 上：

**线上实测（修复前，逐条看日志）**：
* `HYDRA_ROLE=ledge` + 三个接线变量 ⇒ 启动成功 + `ERROR … cluster wiring listed in 'ignored' is NOT used … role="ledge" ignored=HYDRA_REDIS_URL, HYDRA_CLUSTER_TOKEN, HYDRA_CONTROL_URL`（**这条一直是对的**）；
* **`HYDRA_ROLE` 根本不设** + 同样三个接线变量 ⇒ 启动为 `all`，`grep HYDRA_ROLE` 与 `grep 'cluster wiring'` **零命中** —— **一句都没有**。也就是说：**"忘了设这个变量"这种最可能的运维错误，恰好是诊断到不了的那条路径**（拼错有 ERROR，忘配有静默）。
* 第三处**假陈述**：`HYDRA_ROLE=all` + 接线 ⇒ `ERROR … unknown HYDRA_ROLE … role="all"` —— 而 `all` 是 `lib.rs:43`（`Node role (HYDRA_ROLE: all/leader/edge)`）、`cluster.md:38`（默认 `all`）、`jiqun-deploy.md:91`（取值 `all`）三处**都写着的合法值**。把一个**文档化的值**报成 unknown，是一条**不实的日志**（日志是运维唯一的证据来源，说错比不说更糟）。

### C. 修法：把诊断挂到**条件**上，并把整个判定做成**纯值**

1. **抽出纯函数** `role_from_raw(Option<&str>) -> NodeRole`（无环境访问 ⇒ 测试可并行），并**折叠首尾空白**：manifest 里的 `" leader "` 是**空格事故**，不是另一种角色 —— 不折叠就会把一个**配好了集群身份**的节点静默降级成单节点（与 `ignored_cluster_wiring` 把空白视作"未配置"同一条纪律）。
2. **新增纯值** `RoleNotice`（`Quiet` / `Unknown{raw}` / `WiringIgnored{why, ignored}`）+ 纯函数 `role_notice(raw, wiring)`，判据是**条件**而**不是**到达条件的路径：

   | `HYDRA_ROLE` | 集群接线 | 结果 |
   |---|---|---|
   | `leader` / `edge` | 任意 | `Quiet`（接线**正在被使用**） |
   | 未设 / 空 / `all` | 无 | `Quiet`（**就是**文档化的单节点默认；一条正确的输入不该被警告） |
   | 拼错的值 | 无 | `Unknown` ⇒ **WARN** 点名该值 |
   | **未设** / **空白** / **`all`** / **拼错** | **有** | **`WiringIgnored` ⇒ ERROR**，`ignored=` 逐个点名被丢弃的变量；`why` 区分四种运维错误（`HYDRA_ROLE is not set` / `… is blank` / `HYDRA_ROLE="all" is not a cluster role` / `HYDRA_ROLE="ledge" is not a known role`） |

   `from_env()` 退化成"读环境 → `role_from_raw` → `role_notice` → 按值记日志"，四条路径共用同一个判定。
3. **测试**：原先 `parses_roles` 跑的是**测试模块里的一份 `match` 副本**（生产逻辑改了它照样绿 —— 一份**长得像覆盖率的重复实现**）⇒ 改为调用**生产** `role_from_raw`，并补上空白折叠/空白即未设两行；新增 `role_notice_covers_every_way_to_be_a_non_cluster_node` 覆盖上表**每一行**（含 `all` 那行"不得说 unknown"的断言）。`cluster::` 全模块 **69 passed / 0 failed**（`HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380`）。
4. **文档同步三处**（`ops.md` §13.3 的变量表、`cluster.md:38`、`jiqun-deploy.md:91`）：三处原本只说"拼错会 WARN 后回落"，现在写清 **ERROR + `ignored=` 点名**、**未设也报**、**空白被折叠**。`jiqun-deploy.md` 那句"拼写错误会 WARN 后回落单节点"本身也不准（有接线时是 ERROR），一并改掉。

### D. 把两条都钉成**真入口**：`integration/test_startup_knobs.py`（11 条断言）

上一轮 §2fw.E 的队列项（`HYDRA_REDIS_MODE` 的 fail-fast 只有手工实测）与本轮的新缺口**同属一个主题**（"启动开关：拒绝，或者大声说出它正在被丢弃"），所以合成**一个** drill，观测全是**进程级**的（退出码 + 日志文本），没有一条读 helper：

* **K1/K2** `HYDRA_REDIS_MODE=clustr` / `sentinel`（`HYDRA_ROLE=edge` + 真 Redis）⇒ **非零退出** 且日志点名**开关与运维输入的那个值**（`unsupported HYDRA_REDIS_MODE 'clustr'`）；
* **K3 控制组** 同一个 edge 用 `single` 真的起来了 ⇒ K1/K2 证明的是**开关**，不是"集群模式坏了"；
* **K4/K5/K6** 三条接线路径（拼错 / **未设** / `all`）都**继续服务**，且 ERROR 行**逐个点名**三个被丢弃的变量；K6 另断言措辞是 `not a cluster role`、**不得**出现 unknown；
* **K7 沉默控制组**（未设、`HYDRA_ROLE=all`，都**不配**接线）⇒ **零** role/wiring 噪音 —— 这是"规则必须在正确输入上闭嘴"的正面证据；
* **K8** `HYDRA_ROLE=" edge "` ⇒ 真的以 edge 身份启动，且**不**报"丢弃接线"；
* **K9** `HYDRA_NON_ROUTE_STRATEGY=rejekt` ⇒ 非零退出并**引用**该值（这一条此前只有单元测试，现在钉在运维实际看得到的地方）。

配套：drill **只**观测真二进制，前置检查**二进制存在**与 **Redis 可达**（否则 `exit 2`，不是 `exit 1`）；`observe()` 的"启动成功"判据是 **HTTP 健康端点 200 / 进程仍在**，拒绝腿的判据是**退出码非零**（"没挂"与"被拒"必须分开，否则一个**慢失败**的进程会假通过）；继承环境里所有 `HYDRA_*` **先清掉**（开发者 shell 里导出的 `HYDRA_ROLE` 会把 K7 的沉默断言改成"因为别的原因绿"）。门禁条目 + CI 接线同步：**95 → 96** 条。

### E. 反向证伪（两次，各自最小、单变量、在干净状态上）

1. **把"未设 + 有接线"那一臂改回 `Quiet`**（即修复前的行为，单行）⇒ 重编译后 drill **恰好 K5 一条红**，失败信息逐字复现旧症状 `<SILENT: no line named the wiring>`；其余 10 条全绿。恢复后 `diff -q` 与备份一致、`FALSIFY`=0。
2. **删掉 `all` 那一臂**（让它落进"unknown role"分支，单行）⇒ drill **恰好 K6 的措辞断言红**，输出逐字复现旧假陈述 `cluster wiring is configured but HYDRA_ROLE="all" is not a known role`；其余全绿。恢复、`diff -q` 一致、`FALSIFY`=0。

### F. ★门禁抓到的第二件事（**不是**我的新用例）：刷新路径"计数改两处、日期只改一处"

本轮第一次门禁跑完 **96/96 条**，唯一非零是 **`public claims exit=1`**：

```
- advertised 812 but the suites report 813 (hydra-core 258 + hydra-server 555)
- correctness-gate claim #1 says 258 core + 554 server but the suites report 258 core + 555 server
```

诊断：我在 §2fx.C 里**净增了 1 条单元测试**（`role_notice_covers_every_way_to_be_a_non_cluster_role`；`parses_roles` 是改造不是新增）⇒ `hydra-server` **554 → 555**，而 `docs/index.html` 广告的是 **812**。**这不是守卫误报，是公共页面说了旧数字** —— 守卫不但报了，还直接给出刷新命令（`--measure --write`）。

**但刷新之后出现了更值得记的一条**：`--write` 把**计数**（`CLAIM_ZH`/`CLAIM_EN`/`CLAIM_GATE`）和**头条日期**都改了，**却把"正确性门禁"那条声明自己的日期留在原地** ⇒ 页面变成

```
正确性门禁：258 core + 555 server 测试（2026-09-30 计数）   /   (counted 2026-09-30)
```

—— **555 是 2026-10-01 测出来的**，而这一行说它是 09-30 数的：**一条关于"这两个数字是哪天测的"的假陈述**，而且**守卫看不见它**（`CLAIM_GATE` 只匹配数字，`DATE_ZH`/`DATE_EN` 只匹配头条日期）。这正是本会话反复出现的那一族：**同一个测量被写在两处，刷新路径只更新了其中一处**（第 182 轮的"注释里的实测数字过期"同型）。

**修法（守卫 + 页面，一次做完）**：`scripts/check_public_claims.cjs` 新增两条**声明式**日期模式 `CLAIM_GATE_DATE_ZH = /（(\d{4}-\d{2}-\d{2})\s*计数）/` 与 `CLAIM_GATE_DATE_EN = /\(counted\s+(\d{4}-\d{2}-\d{2})\)/i`，并规定：

1. **每条（zh/en）必须带日期**，缺了就 FAIL 并写"re-verify the gate wording, then update this checker so it keeps matching something" —— 否则有人改写措辞就能**静默退休**这条规则（与既有的"gate claim 消失"规则同款措辞）；
2. **日期必须等于广告的测量日期**（两个语言各比各的头条日期）。这里**刻意选"相等"而不是"不更新"**：这条声明与头条是**同一次测量**的两处陈述，而"早于头条"**恰好就是**要抓的那个漂移状态；刷新只要一条命令；
3. **`--write` 同步刷新这两处日期**（与头条日期同一趟、同一个本地日期）。

页面用守卫自己的命令修好：`node scripts/check_public_claims.cjs --measure --write` ⇒ `813 Rust tests (258 core + 555 server), dated 2026-10-01`，两处声明日期都变成 `2026-10-01`。

**测试 +2（该套件 27 → 29，全过）**：①`the correctness-gate claim date must be the advertised measurement date`（**逐字复现线上漂移**：计数对、声明日期早一天 ⇒ FAIL 并点名 zh/en；**外加**"改写后不带日期" ⇒ FAIL 而不是静默退休；控制组：一致 ⇒ OK 且 OK 行明说 "carries the same measurement date"）；②`--write refreshes the correctness-gate claim date too (the 2026-10-01 drift)`（写完后**两个**日期都是今天，并且**用同一套检查复跑刷新后的页面必须 exit 0** —— 防止"写完自己就算通过"的自我满足形状）。夹具 `docsWith()` 的两条声明也补上日期（否则新规则会把**全部**既有夹具打红 —— 本族第 N 次"自己的规则先打到自己"，这次**在写测试之前**就给夹具补上了）。

**★这条规则第一次运行就打在真页面上**：`node --test scripts/check_public_claims.test.cjs` 里那条 `the shipped docs/index.html stays parseable and self-consistent` 当场红，报的正是 `docs/index.html` 的 `claim (zh) is dated 2026-09-30 while the page advertises 2026-10-01` —— 也就是**它先抓住了那个真实缺陷，再让我去修**。

**两条反向证伪（各自单变量、干净状态）**：①把日期比较关掉（`false && advertisedDate && …`）⇒ **恰好 2 条新用例红**（第 22 条也红，因为"没问题 ⇒ 不写"这一耦合是真实的）；②去掉 `--write` 里那两行日期刷新 ⇒ **新用例红 + 既有的 `--write refreshes the count and the date in both locales` 也红**（失败信息逐字是 `the correctness-gate claim (zh) is dated 2026-01-01 while the page advertises 2026-10-01`）⇒ 刷新侧**承重**。两次恢复后 `diff -q` 一致、`FALSIFY`=0、套件 29/29、`--measure` OK。

**代价与纪律**：这一轮的净效果是"加一条测试"这件事**会推动一个公共声明**，而**只有门禁会告诉你**。刷新命令是守卫自己给的，所以没有猜测成分；但**刷新之后必须再读一遍 diff**，否则就会像本轮一样把"日期只改一半"的假陈述带进页面。

### G. 本轮验证（门禁判定）

以**最终修订**（含 §2fx.F 的守卫修复与页面刷新）重跑整条门禁：`.acceptance/round192c-gate.out`。

- 条目数 **96**（本轮 +1：`startup knobs (refuse/name)`），`grep -c '^GATE '` = 96，`grep 'exit=[1-9]'` **零命中**，末行 `OVERALL=GREEN` / `GATE_EXIT=0`。
- 首条 `tree manifest (record) exit=0`、末条 `tree manifest (verify) exit=0` ⇒ `OK (the source tree is unchanged: 355 file(s), docs-only changes: 1)`（本轮只有本计划文件在门禁期间被改；**代码零改动**）。
- **新条目在门禁内真的跑了 11 条断言**（不是"本地跑过"）：门禁日志里 `K1`…`K9`（含 K6/K7 各两条）**全部 PASS**，末行 `STARTUP KNOBS: PASSED (refusal + named-value + ignored-wiring + silence control)`。
- **新增/修改的单元测试在门禁里跑过**：`test optional + live redis/CH` 段里 `cluster::tests::role_notice_covers_every_way_to_be_a_non_cluster_node ... ok`、`cluster::tests::parses_roles ... ok`、`cluster::tests::a_role_fallback_names_the_cluster_wiring_it_ignores ... ok`（两组 feature 各跑一遍）。
- **`public claims` 由红转绿、且携带新规则**：`[claims] OK: … 258 core + 555 server … and carries the same measurement date`；`public claims tests` = **29 tests / 29 pass / 0 fail**（+2）。
- 其余本轮触及的条目：`gate entries` 打印 **96 条**、`test tails`、`ci wiring`、`source purity`、`documented env/defaults/metrics`、`compose grace/health`、`i18n`、`e2e contracts`、`tree manifest` 全部 `exit=0`。

⇒ 本轮判定：**GREEN（96/96）**。**两条缺陷（静默的角色回退路径 + 刷新路径只改一半的日期）都被门禁内的入口钉住**：第一条由 drill 的 K5/K6 断言、第二条由 `public claims` 自己的新规则（该规则**第一次运行就抓到了盘子上的真实漂移**）。

**★一条本轮的方法论**：门禁"红"的时候我先怀疑的是自己的新用例有没有写错；实际原因是**我的改动本身推动了公共声明**（测试数 812→813）。守卫不但报对了，还给出刷新命令 —— 而**刷新之后必须再读一遍 diff**：正是那一步暴露出 `--write` 只改了一半日期，也就找到了第二处缺陷。

### H. 下一轮队列（含可行性已探明的一条）

1. ✅ **已做（第一百九十三轮，见 §2fy）**：`CLUSTER_ONLY_ENV` 现在是**唯一所有者**（10 个名字），`from_env` 从它构造 `ignored` 清单，`scripts/check_cluster_env.cjs` 双向检查（`src/cluster/` 下的读必须被列出或记录；列出的名字必须有读者），drill 的 **K10** 在线上断言"十个都点名"。原队列文字保留如下供追溯（第一百九十二轮 §2fx.H）：**★`ignored_cluster_wiring` 是一份"手工枚举"，它自己也会吃掉对象**（实测）：那条 ERROR 的措辞是"the cluster wiring listed in **`ignored`** is NOT used"，而 `ignored` **只列三个变量**（`HYDRA_REDIS_URL` / `HYDRA_CLUSTER_TOKEN` / `HYDRA_CONTROL_URL`）。但同一段代码里**还有至少六个只在集群模式下才有意义**的开关，它们在 `HYDRA_ROLE` 不是集群角色时**同样被丢弃、且一个字都不提**（读数点已逐个量出）：
   * `HYDRA_NODE_ID`（`cluster/mod.rs:154`）、`HYDRA_CONTROL_POLL_MS`（`cluster/mod.rs:148`）—— 都在 `ClusterConfig::from_env` 里读，而该函数**无条件**被调用（`main.rs:226`），值只被集群机器使用；
   * `HYDRA_REDIS_MODE`（本轮实测：`role=all` 时根本不校验）；
   * `HYDRA_PUBLIC_URL`（`main.rs:454`）、`HYDRA_LEADER_LEASE_MS`（`main.rs:102`）、`HYDRA_REGISTRY_STALE_GRACE_SECS`（`main.rs:1711`）、`HYDRA_FORWARD_TIMEOUT_SECS`（`cluster/forward.rs:40`）。
   判据与第 185–189 轮同族（**清单不得吃掉对象**）：一个读到"这三条被忽略"的运维，会合理推断"其余的我配了就会生效"。下一轮的落点是**按构造**给出这份清单（"哪些名字是 cluster-only"由一处表/判定而不是手写字面量决定），并配一条守卫：**cluster-gated 代码里读到的 cluster-only 名字必须都在表里**（否则 DRIFT）。**先量后改**：哪些名字真的 cluster-only 需要逐个确认（例：`HYDRA_RESEAL_SECRETS` 是一次性维护开关、与集群无关；`HYDRA_TENANT_API_CONVERGE_TIMEOUT_MS` 在单节点也生效）—— 这份"候选 7 个、需确认边界"的清单就是本项可行性已探明的部分。
2. **两条上一轮**记录下来的负结论（**不要重做**）：①`HYDRA_REDIS_MODE` 只在 `role.is_cluster()` 时被读取 ⇒ 单节点部署里不校验也不报错，**有意保留**（该开关在单节点下无意义，而"配了接线却没设角色"已由第一百九十二轮修复变成 ERROR；加无条件校验会让无关的遗留变量拒绝启动）；②`HYDRA_ROLE` 回退成 `all` **本身**有意保留（拼错不得让节点无法代理），修的是"**回退必须出声**"，不是取消回退。
3. **本轮记录的一条负结论（不要重做）**：`NOT_CLUSTER_ONLY` 里那条 `HYDRA_ROLE` 记录**必须留**（它是选择器本身，`src/cluster/mod.rs` 里读它、但它在任何模式下都读）—— 我一度以为"把 `HYDRA_ROLE` 的读取挪出 `src/cluster/`"可以让守卫更纯，但那只是把同一个事实搬到另一个文件，且会让 `NodeRole::from_env` 与其模块分离；记录 + 过期检查是更诚实的做法（记录一旦不再适用，守卫会报）。
4. **下一轮候选（本轮顺手量到，未做）**：`check_documented_env.cjs` 的"死豁免"方向只覆盖 `READ_BY_DEPENDENCY`；本轮新增的 `NOT_CLUSTER_ONLY` 已经自带过期检查，但**别的**"手工枚举"里可能仍有不带过期检查的表 —— 机械筛法：`grep -n 'new Set(\[' scripts/*.cjs` 找出一切**仍在用裸 Set**（无理由、无过期检查）的记录表，逐个按"条目是否挣得位置"判。这是第 181/183/184 轮那条线的收尾（`NOT_A_METRIC` 已改 Map + 理由，其余待查）。

---
## 2fy. 第一百九十三轮：同一族的收尾 —— **清单只点三个名字，另外七个被静默丢弃**；按构造给清单唯一所有者 + 双向守卫 + 线上 K10

### A. 起点：上一轮我就把这条写进了队列，本轮**先量边界再改**

上一轮（§2fx.H）量到：那条"你配了集群接线但这个节点不是集群节点"的 ERROR 只点名 3 个变量。本轮先把"到底哪些是 cluster-only"逐个量清楚（**先量后改**）：

| 变量 | 读点 | 为什么算 cluster-only |
|---|---|---|
| `HYDRA_REDIS_URL` | `main.rs:227`（集群分支）、`redis` 连接 | backbone 只在集群用 |
| `HYDRA_REDIS_MODE` | `redis/mod.rs`（仅在 `role.is_cluster()` 分支被调用） | 拓扑开关 |
| `HYDRA_CLUSTER_TOKEN` | `main.rs:238`（集群校验）+ 控制面 | 控制通道共享 token |
| `HYDRA_CONTROL_URL` | `main.rs:1053`（edge 轮询） | 快照轮询端点 |
| `HYDRA_PUBLIC_URL` | `main.rs:454`，**在 `#[cfg(feature="cluster-redis")]` 的 registry 块内** | 注册到注册表的可达地址 |
| `HYDRA_NODE_ID` | `cluster/mod.rs:157` | 注册表/租约身份 |
| `HYDRA_CONTROL_POLL_MS` | `cluster/mod.rs:151` | 轮询间隔（控制客户端） |
| `HYDRA_LEADER_LEASE_MS` | `main.rs:102`（**唯一的调用点**在 `role == Leader` 的分支里） | 选举租约长度 |
| `HYDRA_REGISTRY_STALE_GRACE_SECS` | `main.rs:476`（registry 块内） | 静默节点判死的时间窗 |
| `HYDRA_FORWARD_TIMEOUT_SECS` | `cluster/forward.rs:40`（转发只在 registry 存在时可达） | standby→active 写转发 |

**明确排除**（写进代码注释，避免下次又被"顺手加进去"）：`HYDRA_USAGE_SINK`/`HYDRA_CLICKHOUSE_URL`（集群必填但单节点同样有效）、`HYDRA_TENANT_API_CONVERGE_TIMEOUT_MS`（租户面在所有角色都存在）、`HYDRA_RESEAL_SECRETS`（一次性维护开关，与拓扑无关）。

### B. 修法：**清单唯一所有者**，不再写在函数体里的三个字面量

1. `crates/hydra-server/src/cluster/mod.rs` 新增 **`const CLUSTER_ONLY_ENV: [&str; 10]`**：每个名字带一句"它是干什么的"注释，并在文档注释里写明**排除名单与理由**（上面那段）。这是"什么算集群接线"的**唯一所有者**。
2. `ignored_cluster_wiring(present: &[(&str, Option<&str>)])` 改为**纯函数接一对列表**（不再自己读环境），`NodeRole::from_env` 用 `CLUSTER_ONLY_ENV` 构造这张表 ⇒ **加一个集群开关 = 往表里加一行**，而不是"记得同时改三处字面量"。空白值仍不算配置（与角色侧同一条规则）。
3. 单元测试：`a_role_fallback_names_the_cluster_wiring_it_ignores` 改为**六项**（含 `HYDRA_NODE_ID`/`HYDRA_LEADER_LEASE_MS`/`HYDRA_REDIS_MODE`）并断言"未设的**不出现**在清单里"；新增 `the_cluster_only_env_table_is_exactly_the_cluster_topology` 断言**表的精确内容**（这条在类型上就带长度：`[&str; 3] != [&str; 10]` **编译期**就会红，见 §2fy.D）+ 无重复 + 每个名字形如 `HYDRA_*`。`cluster::` 全模块 **70 passed / 0 failed**。

### C. 守卫：`scripts/check_cluster_env.cjs`（双向），外加**共享记录表**

只有"表 + 测试"仍然是**手工枚举**：没人保证下一个集群开关会被加进去。所以新增守卫，两条机械规则：

* **R1 COMPLETE**：`crates/hydra-server/src/cluster/` 下**任何** `std::env::var("HYDRA_*")` 字面读取，必须在 `CLUSTER_ONLY_ENV` 里 —— 该目录下每个模块都是为集群而存在，所以"在这里读"就**按构造**等于 cluster-only。**这正是出错的那一半**：住在 `src/cluster/` 的三个读（`HYDRA_NODE_ID`、`HYDRA_CONTROL_POLL_MS`、`HYDRA_FORWARD_TIMEOUT_SECS`）**恰好都被旧的三字面量清单漏掉**。
* **R2 EARNED**：表里每个名字，在 `crates/hydra-server/src/` 下**必须有至少一个字面读点** —— 没有读者的名字是"关于一个不存在的设置的主张"（与第 181 轮 `NOT_A_METRIC`、第 183/184 轮的"必须挣得位置"同一条规则）。

**记录表用共享代数**：`src/cluster/mod.rs` 里读 `HYDRA_ROLE`，而它**显然不是** cluster-only（它是选择器本身，任何模式都读）⇒ 这条例外记为 `NOT_CLUSTER_ONLY`（名字→理由，**替换式**覆盖 `CLUSTER_ENV_NOT_CLUSTER_ONLY`），并交给 `recorded_exceptions.cjs` 的 `audit()` —— 于是它**自带过期检查**（一旦 `src/cluster/` 不再读它，守卫会报"这条记录不再适用"）。**★写测试时又踩了一次同一个坑**：夹具默认继承内置记录 ⇒ 控制组当场红（`NOT_CLUSTER_ONLY records HYDRA_ROLE, but that no longer applies`）⇒ 与第 180/181/189 轮同样处理：夹具 harness **默认注入 `'{}'`**（替换而非合并），需要记录表的用例显式给值。

**守卫自身**：`exit 0/1/2`；**表解析不出来 ⇒ 2**（不是 1）；**`src/cluster/` 里一个字面读都没有 ⇒ 2**（"没有主语的规则不是通行证"）；`--help`；OK 行打印 `10 cluster-only name(s); 6 literal read(s) …; 1 recorded non-cluster read(s)` **并把十个名字列出来**（运维看到的就是这份清单）。测试 **13/13**：完整表通过并打印清单、缺名字 ⇒ 点名 `file:line`、同一读被记录 ⇒ 通过且不算过期、记录过期 ⇒ 报、无读者的表项 ⇒ 报、重复 ⇒ 报、四种 `CANNOT VERIFY`（无表声明／空表／无读点／owner 文件缺失）、未知参数、**真树**通过、以及"**出厂表与 Rust 里那条精确断言一致**"。

### D. 线上 K10：drill 断言**十个都被点名**

`integration/test_startup_knobs.py` 新增 **K10**：把**全部十个** cluster-only 变量配上、`HYDRA_ROLE=all` ⇒ 节点照常启动，但那条 ERROR 必须**一个不漏**地点名十个（判据：`missing=[]`）。分工写进注释：R1 负责"住在 `src/cluster/` 的读"，`main.rs`/`redis` 里的四个（`PUBLIC_URL`/`LEADER_LEASE_MS`/`REGISTRY_STALE_GRACE_SECS`/`REDIS_MODE`）**无法靠路径机械判定**——它们由 K10 在线上兜住。drill 现 **12 条断言**，门禁条目 **98** 条。

### E. 反向证伪（三次，各自最小、单变量、干净状态）

1. **把表缩回修复前的三个名字**（即重现线上缺陷）：`check_cluster_env` **红**并逐条点名 `HYDRA_FORWARD_TIMEOUT_SECS`（`forward.rs:40`）、`HYDRA_CONTROL_POLL_MS`、`HYDRA_NODE_ID`；drill **K10 红**且 `missing=` 正好是**那七个**；单元测试**编译期**红（`[&str; 3] == [&str; 10]` 不成立 —— 长度在类型里，这比运行期断言更早）。
2. **同长度换名**（`HYDRA_FORWARD_TIMEOUT_SECS` → `HYDRA_RESEAL_SECRETS`，一步）：单元测试**运行期**红且 `left/right` 逐字给出两份清单；`check_cluster_env` **红**点名 `forward.rs:40` 的读没被列出；drill **K10 红** `missing=['HYDRA_FORWARD_TIMEOUT_SECS']`。**这条同时量出 R2 的边界**：`HYDRA_RESEAL_SECRETS` **有**读点（`main.rs:1680`），所以 R2 不报它 —— R2 抓的是"没有读者的名字"，不是"不是 cluster-only 的名字"，后者由精确断言与 R1 负责。
3. **关掉守卫的 R2**（`false &&`，一步）⇒ 该套件**恰好第 5 条用例红**（`a table entry that nothing reads is a finding`），其余 12 条绿。
   三次恢复后 `diff -q` 与备份一致、`FALSIFY`=0、守卫 13/13、drill PASSED、`cluster::` 70/70。

### F. 本轮验证（门禁判定）

以**最终修订**重跑整条门禁（第二次运行，第一次因已知的计数漂移在第 29 条红、已停）：`.acceptance/round193b-gate.out`。

- 条目数 **98**（本轮 +2：`cluster-only env table`、`cluster-only env guard tests`），`grep -c '^GATE '` = 98，`grep 'exit=[1-9]'` **零命中**，`OVERALL=GREEN` / `GATE_EXIT=0`。
- **新条目在门禁内真的跑了**：`[cluster-env] 10 cluster-only name(s); 6 literal read(s) under crates/hydra-server/src/cluster/ (9 file(s)); 1 recorded non-cluster read(s): OK`，并**打印出十个名字**；守卫套件在门禁内 `# tests 13 / # pass 13 / # fail 0`。
- **drill 的 K10 在门禁内 PASS**（`missing=none`），该 drill 段共 **12 条断言**全 PASS，末行 `STARTUP KNOBS: PASSED`。
- **单元测试**：`cluster::tests::the_cluster_only_env_table_is_exactly_the_cluster_topology ... ok` 在两组 feature 里各出现一次（`grep -c` = 2）。
- **公共声明在位**：`[claims] OK: … 258 core + 556 server … and carries the same measurement date`（刷新后 814 = 258 + 556）。
- 首条 `tree manifest (record)`、末条 `tree manifest (verify)` 均 `exit=0` ⇒ `OK (the source tree is unchanged: 357 file(s), docs-only changes: 1)`（门禁期间只改了本计划文件；**代码零改动**）。

⇒ 本轮判定：**GREEN（98/98）**。**缺陷（清单只点三个名字、另外七个静默丢弃）被四层钉住**：精确表断言（单元测试）、R1 机械完整性（守卫）、R2 挣得位置（守卫）、以及**线上 K10**（十个都被点名）。

**★门禁后的收尾**：把 §2fy.G 的教训落进门禁脚本**头部的前置说明**（"加过 Rust 测试 ⇒ 先 `cargo fmt --check && node scripts/check_public_claims.cjs --measure --write` 再开跑；停门禁要用作业句柄，不要 `pkill -f cargo`"）。改动只在注释里，`bash -n` 通过、`check_gate_entries` 仍报 **98 条** ⇒ 条目声明未被影响（如实记录：这条注释本身没有经过门禁运行，它不改变任何条目）。

### G. ★同一个 RED **又一次**出现，这次处置不同：先停、刷新、只跑一遍

本轮第一次门禁在第 29 条 `public claims` 就 **RED**，原因与上一轮**同一条**：我新增了 1 条单元测试（`the_cluster_only_env_table_is_exactly_the_cluster_topology`）⇒ `hydra-server` **555 → 556**，而 `docs/index.html` 广告 813：

```
[claims] FAIL: docs/index.html
  - advertised 813 but the suites report 814 (hydra-core 258 + hydra-server 556)
  - correctness-gate claim #1 says 258 core + 555 server but the suites report 258 core + 556 server
```

**处置（与上一轮的差别）**：上一轮我等整条门禁跑完（40 分钟）才处理；这一轮**先确认失败原因**（读 `.acceptance/round10-gate.log` 里 `public claims` 段），确认是"已知的计数漂移"而不是新缺陷后，**停掉这次运行**（日志留作 `.acceptance/round193-gate-stopped.out`），用守卫自己的命令 `--measure --write` 刷新到 `814 Rust tests (258 core + 556 server), dated 2026-10-01`（**两处声明日期都在位**，第一百九十二轮的新规则当场验证了这一点），然后重跑整条门禁 **一遍**。

**教训（写进下一轮的落地项）**：`加过 Rust 测试 ⇒ 公共计数必然漂移` 是可以**在开跑门禁之前**用一条命令消掉的，而门禁的结构决定了它只能在"两组 cargo 测试跑完、拿到 transcript"之后才判定（第 29 条，约 8 分钟）—— 所以正确做法是**改完 Rust 先跑 `node scripts/check_public_claims.cjs --measure --write`，再开跑门禁**。这一条要落到门禁脚本**头部的前置说明**里（注释级，下一轮做；门禁脚本正在运行时不得编辑它 —— bash 是边读边执行的）。

**另一条自伤，记在这里**：停门禁时我写了 `pkill -f 'cargo'`，而这个模式**匹配到了我自己那条命令行**（命令文本里就含 "cargo"）⇒ 我自己的 shell 被 SIGTERM 掉，`mv` 没执行。**教训**：`pkill -f <模式>` 的模式不得是**自己命令行里出现过的字符串**；要停就按 pid/作业句柄停（本轮正确的一步是先用 `job_kill` 停作业，再用 `pgrep -af` 确认只剩自己的自匹配）。

### H. 下一轮队列（含本轮量出的**负结论**）

1. **`recorded_exceptions.cjs` 那条线的收尾：扫完 `new Set([` 之后是负结论**（本轮实测，**不要重做**）。机械筛 `grep -n 'new Set(\[' scripts/*.cjs`（排除测试）只剩四处，逐条读过：
   * `check_documented_env.cjs` 的 `CODE_EXT` / `CODE_NAMES` —— **正分类**（"什么算代码"），不是"记录表"族；
   * `check_ci_wiring.cjs` 的 `SKIP_TOP_LEVEL = {scripts, secure, dev-docs}` 与 `tree_manifest.cjs` 的 `SKIP_DIRS = {target, node_modules, dist, …}` —— 是"**有意不扫**"的跳过集合，而且**理由就写在旁边的注释里**（`scripts` 是规则 1 自己的目录、`secure` 不能读、`dev-docs` 是散文；`target`/`node_modules`/`.acceptance` 是生成物与草稿）。与"记录表吃掉对象"的关键差别是**后果**：这两处跳过的整棵目录**本来就不该被扫**，条目过期不会让任何真实对象**静默逃过检查**（OK 行仍打印扫描到的文件数）。
   ⇒ **不为它们建守卫**（信噪比低）。这与第 168/171/174/175 轮的负结论同型：**审计的产出可以是"这里不需要规则"，但必须把判据和实测数字留下**。
2. ✅ **门禁脚本头部的前置说明（第一百九十四轮前落地，见 §2fy.G 与 §2fz）**：已把"加过 Rust 测试 ⇒ 先 `cargo fmt --check && node scripts/check_public_claims.cjs --measure --write` 再开跑门禁"与"停门禁用作业句柄，不要 `pkill -f` 一个出现在自己命令行里的模式"写进 `.acceptance/round10-gate.sh` 头部注释（`bash -n` 通过、`check_gate_entries` 仍报 **98** 条）。
3. **本轮（第一百九十四轮）派出的两路只读 Oracle 复审**（守卫生态本身 / 启动 drill 与文档一致性）**仍在飞**，结果到达后并入下一轮处置 —— 这是本会话第十几次用"外部对抗性复审"给见底的池子补充可判定项，历史上每轮都产出真问题。

---
## 2fz. 第一百九十四轮：**本轮修的那个守卫自己长了同一个病** —— `HYDRA_` 前缀过滤器把 `HOSTNAME` 吃掉了（外加"看不见的读取"）

### A. 起点：不是新一轮复审，而是**回头量自己上一轮刚建的守卫**

§2fy.C 里我写的 `check_cluster_env.cjs` 的 R1 用的是：

```js
/env::var(?:_os)?\(\s*"(HYDRA_[A-Z0-9_]+)"/g
```

也就是说：**R1 只看得见 `HYDRA_` 开头、且写成字面量的读取**。这两个限定各自都是一个"过滤器吃掉对象"的形状 —— 而**这个守卫的全部存在理由**就是抓那种形状（§2fy 的发现就是"一份手写清单吃掉了七个对象"）。先量真相（`crates/hydra-server/src/cluster/` 下共 8 处 `env::var`）：

```
cluster/forward.rs:40   std::env::var("HYDRA_FORWARD_TIMEOUT_SECS")   ← 字面量、HYDRA_
cluster/mod.rs:72      std::env::var("HYDRA_ROLE")                    ← 字面量、HYDRA_（已记录）
cluster/mod.rs:76      std::env::var(name)                            ← ★ 非字面量：R1 完全看不见
cluster/mod.rs:145     std::env::var("HYDRA_CONTROL_URL")             ← 在表里
cluster/mod.rs:148     std::env::var("HYDRA_CLUSTER_TOKEN")           ← 在表里
cluster/mod.rs:151     std::env::var("HYDRA_CONTROL_POLL_MS")         ← 在表里
cluster/mod.rs:157     std::env::var("HYDRA_NODE_ID")                 ← 在表里
cluster/mod.rs:158     std::env::var("HOSTNAME")                      ← ★ 字面量但**不是 HYDRA_**：R1 看不见
```

* **`HOSTNAME`（`cluster/mod.rs:158`）**是 `node_id_from` 的兜底层：一个**真实存在于本树的**、住在 `src/cluster/` 里的环境读取，而 R1 因为**只认 `HYDRA_` 前缀**从来没看过它一眼。它确实不该进 `CLUSTER_ONLY_ENV`（操作系统总会设它，不是运维**为集群**配置的东西，所以它不可能成为"被丢弃的接线"）——但它**必须是一条被记录的决定**，而不是被过滤器顺手吃掉。
* **`env::var(name)`（`cluster/mod.rs:76`）** 是我自己上一轮写的表循环；今天无害，但**任何**将来用间接方式写的 cluster-only 读取都会**完全不可见**（R1 的字面量规则够不着，R2 只管"表里的名字有没有读者"，方向相反）。

### B. 修法：把两个"看不见"变成**必须记录的决定**

`scripts/check_cluster_env.cjs`：

1. **读取器重写**：`envReads(files)` 返回两类 —— `literals`（**任意名字**，不再限前缀）与 `nonLiteral`（按**文件**聚合，因为守卫说不出名字）。`hydraLiteralReads()` 保留给 R2 用（只判 `HYDRA_*` 是否有读者）。
2. **R1 的判据不变**（`src/cluster/` 下的字面量读取必须在表里），但**覆盖面变大**：`HOSTNAME` 现在真的被看见了。
3. **新增 R3 NO INVISIBLE READS**：`src/cluster/` 下**每个**含非字面量读取的文件必须在 **`NON_LITERAL_READ_OK`**（文件→理由，替换式覆盖 `CLUSTER_ENV_NON_LITERAL_OK`）里记录；未记录 ⇒ finding，**记录过期**（该文件不再有非字面量读取）⇒ 也是 finding。今天内置记录只有一条：`cluster/mod.rs` —— "`NodeRole::from_env` 里遍历 `CLUSTER_ONLY_ENV` 的循环，它**按构造**只读那张表"。
4. **`NOT_CLUSTER_ONLY` 加一条**：`HOSTNAME` → "操作系统主机名（node-id 兜底层）；总是被设置，不是运维为集群配置的东西"。
5. OK 行因此多了一个数字：`10 cluster-only name(s); 7 literal read(s) … (9 file(s)); 1 file(s) with a non-literal read; 2 recorded non-cluster name(s)` —— **7 而不是 6**，正是被吃掉的 `HOSTNAME`。
6. **头部注释**里把"KNOWN LIMIT"写明而不是暗示：本守卫**不把测试模块切出去**（`check_source_purity.cjs` 有区域解析器，这里没有），所以 `src/cluster/` 里一个 `#[cfg(test)]` 块读环境变量会需要一条记录；**实测今天没有这样的读取**（9 文件 / 7 字面量 / 1 非字面量）。

**测试 13 → 15**：新增①非 `HYDRA_` 字面量读取（`HOSTNAME`）未记录 ⇒ FAIL 并点名，**控制组**记录后 ⇒ 通过且 OK 行给出记录数；②非字面量读取未记录 ⇒ FAIL 并点名**文件**，记录后 ⇒ 通过（OK 行打印 `1 file(s) with a non-literal read`），**记录过期**（去掉间接读取）⇒ FAIL。两条既有断言因措辞变化同步更新（`recorded non-cluster read(s)` → `recorded non-cluster name(s)`、`env::var("HYDRA_*")` → `env::var("NAME")`）—— 这是**我自己的断言跟着我自己的输出改**，改的只是字符串，判据没动。

### C. 反向证伪（两次，各自最小、单变量、干净状态）

1. **把读取器的正则改回 `HYDRA_` 前缀**（一步，即重现本轮修掉的缺陷）⇒ ①新增的"非 HYDRA 字面量"用例**红**（夹具里的 `HOSTNAME` 又隐形了，守卫报 0 而期望 1）；②**真树用例红**，而且失败原因是**记录过期方向**：`NOT_CLUSTER_ONLY records HOSTNAME, but that no longer applies` —— 也就是说**这条记录让"前缀过滤器"再也不可能被安静地装回去**（实测输出里字面量计数从 7 掉回 6）。
2. **关掉 R3**（`needed: false ? nonLiteralNow : []`，一步）⇒ **恰好那条"非字面量必须记录"用例红**，其余 14 条绿。
   两次恢复后 `diff -q` 与备份一致、`FALSIFY`=0、守卫 OK、套件 **15/15**。

### D. ★两路 Oracle 复审回来了（同一轮内处置）：**守卫自己的"声称多于验证"**

§2fz.A–C 是我**自己**回头量守卫；同时派出的两路只读复审也返回了，两条 P1 都**复现成立**，另有一条**经复测不成立**（如实记录）。

1. **P1（成立，已修）：`NON_LITERAL_READ_OK` 以「文件」为键，吞掉同文件里任意数量的隐藏读取。** 复审用 /tmp 夹具证明：`mod.rs` 里 `env::var(SECRET_KNOB)`（`HYDRA_SECRET_KNOB` **不在表里**）因为"该文件已有一条记录"而**静默通过**，OK 行还宣称"every read … is listed or recorded"。**我自己复现了**（`.acceptance/fx194` 夹具：先只有一条非字面量读取 ⇒ 通过；补上被记录的表循环后再加一条隐藏读取 ⇒ 修复前静默、修复后点名 `mod.rs:19`）。**修法**：记录键从 `file` 改为 **`file#ordinal`**（`parseSiteKey`），`applies` 判"该文件仍有 ≥ ordinal 个非字面量读取" ⇒ **一条记录只覆盖一个 site**（顺序键，不用行号，避免普通编辑churn）。测试 +2（两条 site 只记录一条 ⇒ FAIL 并点名；两条都记录 ⇒ OK；控制组）。
2. **P2（成立，已修）：OK 行声称"回退 ERROR 会列出这十个名字"，而守卫从不读那条链路。** 复审的夹具证明：`CLUSTER_ONLY_ENV` **声明后完全没被使用**（无 `from_env`、无 ERROR）时，OK 行照样打印十个名字。**修法**：(a) OK 行改为陈述**表本身**（`CLUSTER_ONLY_ENV is <十个名字>`），并**明说**"该表是否被 ERROR 打印**静态扫描看不见**，由 `integration/test_startup_knobs.py`（K10）在线上断言"；(b) **把 K10 的手抄清单换成读源码**：drill 现在从 `crates/hydra-server/src/cluster/mod.rs` 解析 `CLUSTER_ONLY_ENV`（`cluster_only_names()`，带 `[&str; N]` 长度校验），表里**新增一个名字就自动进入 K10**，而 drill 没有对应取值时 K10 **直接红**（`no-value-for=…`）—— 于是"表 → ERROR"这条链路第一次有了自动 owner。
3. **P2（成立，已修）：`check_public_claims --write` 在**修不了的问题**上返回 0。** 复审实测：页面**完全没有** correctness-gate 那一行时，`--write` 打印 `rewrote …` + `(was) …` 并 **exit 0**，而下一次检查同一个文件**红**。**修法**：新增 `unfixable` 分类（页面**形状**问题：gate claim 整行消失、缺头条日期、缺 claim 日期），`--write` 写完**可修的那一半**之后，若仍有不可修项 ⇒ 打印 `FAIL: N problem(s) are NOT fixable by --write` 并 **exit 1**；头部退出码说明同步。测试 +1（夹具删掉 gate claim 行 ⇒ `--write` exit 1 且**可修的一半确实被写入**；控制组不加 `--write` ⇒ exit 1 且不写）。
4. **P2/P3（成立，已修，都是"输出比判定宽"）**：①`check_ci_wiring` 头条 `48 artifact(s) examined` 只数了 `discovered` 一类，而同一轮 notes 里另有 40+3+3+54+3 ⇒ 改为**点名类别**并打印**判定总数**（实测 `48 test artifact(s) outside scripts/ and crates/<crate>/tests, 149 artifact(s) judged in total across 6 classes`），它的测试同步断言"类别 + 总数"两半（原断言只认 `artifact(s) examined`，措辞一变就红 —— 已改成同时钉住分类名与总数）；②`check_test_tails` 头部"20 个未判定文件 / 18 JS / 2 Python / 只有 3 个含 exit"实测是 **23 / 23 / 0 / 2**（47 judged 也已是 48）—— 已改成"以 OK 行为准 + 实测数字"，并改掉"它们没有 program end"这句**与代码矛盾**的理由（`:351-355` 明确拒绝假设这一点）；③`check_gate_entries` 同一文件里三个互不相同的、全部过时的 entry 计数（85/88/92，实测 **98**；38→39、9→10、35→36）已全部按实测改正。
5. **负结论（复审提出、经复测**不成立**，不要照它改）**：复审称 `check_documented_env` 的 OK 行"all have a READ site in code"对 `RUST_LOG` 是假陈述（它走 `READ_BY_DEPENDENCY`）。**复测方法（决定性）**：把豁免表**清空**跑 `DOC_ENV_READ_BY_DEPENDENCY='{}' node scripts/check_documented_env.cjs` —— 仍然 `exit 0` ⇒ `RUST_LOG` **不在**那 40 个"表格里的名字"之中，那句断言对它**为真**（它下面的 note 是独立说明，不是这 40 个的一部分）。**处置**：不改判据，只把 OK 行改成**算术式**（`viaDependency > 0` 时才说"all but N …"）—— **该分支今天是潜伏的**（实测 `viaDependency = 0`），代码注释里如实写明"这是潜伏分支，且复审的说法已被上述清空实验否证"。**这就是"先复测再动手"的价值**：照抄复审会让一处**正确的**输出被改成废话。
6. **未做（记入队列，附实测）**：①`redis/mod.rs` 的 `RedisMode::Sentinel`/`Cluster` 两臂**不可达**（`parse` 只放行 `single`）⇒ K2 的标签已改成"经由 catch-all 拒绝"，而**枚举臂本身是否该删**留待下一轮（要动 `RedisMode` 的形状）；②`check_i18n.js` / `check_e2e_contracts.cjs` 的 OK 行不提它们跑过的 `UNSCANNED_UI_OK` 审计（失败路径会打印，故只是"成功时不可审计"）；③`check_cluster_env.cjs` 的 R3 看不见 `env::vars()`（遍历整个环境；真实树 `grep` 为空，潜伏）。

### D2. 本轮验证（门禁判定）

以最终修订重跑整条门禁：`.acceptance/round194b-gate.out` ⇒ **98 条条目全部 `exit=0`、`OVERALL=GREEN`、`GATE_EXIT=0`**。门禁内证据：`[cluster-env] 10 cluster-only name(s); 7 literal read(s) …; 1 non-literal read(s) (all recorded); 2 recorded non-cluster name(s): OK` + 新的"静态扫描看不见"note；`cluster-only env guard tests` 17/17；drill **14 条断言**全 PASS（含 `K10 … parsed=10/10 missing=none`、`K11`（空白角色）、`K12`（单节点边界：`rejected=False wiring-names-value=False`））；`OK (everything is executed — 48 test artifact(s) outside scripts/ and crates/<crate>/tests, 149 artifact(s) judged in total across 6 classes)`；`public claims tests` 30/30；`tree manifest (verify)` ⇒ `OK (the source tree is unchanged: 357 file(s), docs-only changes: 2)`。**反向证伪三次**：R3 改回文件键 ⇒ 守卫 3 条红（含真树，由**记录过期**方向抓到）；`--write` 改回无条件 `return 0` ⇒ 恰好新用例红；K10 改读子集 ⇒ **修复后**红（`parsed=3/10`），修复前不红（这一条是我自己的证伪抓到我自己的）—— 全部恢复后 `diff -q` 一致、`FALSIFY`=0。

---
## 2ga. 第一百九十五轮：队列里的两条"潜伏项"收口 —— 守卫看不见**整环境遍历**，而我上一轮的注释**说过头了**

### A. `env::vars()` 对 `check_cluster_env.cjs` 完全隐形（潜伏漏洞，本轮实测并修）

§2fz 末的队列项③：R3 只匹配 `env::var(_os)?\(`，而 **`env::vars()` / `env::vars_os()` 遍历整个环境**，在守卫里**一个字符都不匹配** ⇒ 一个未记录的整环境遍历对**全部三条规则**隐形（R1 看不见它读的名字，R3 也不认为它是"非字面量 site"）。真实树今天没有这种读取（`grep -rn 'env::vars()' crates/` 为空）⇒ 潜伏，但正是本会话反复修的那一族。**修法**：正则加一支 `env::vars(?:_os)?\(\s*\)`，于是整环境遍历与 `env::var(<表达式>)` **同一种 site**，走同一套 `file#ordinal` 记录与过期检查。**证伪（.acceptance/fx195 夹具）**：未记录 ⇒ `FAIL … mod.rs:16 reads the environment through a NON-literal argument … (1 UNRECORDED)`；记录后 ⇒ `OK … 1 non-literal read(s) (all recorded)`。测试 +1（→ **18/18**）。

### B. 我上一轮的注释**比类型系统更强**：`RedisMode::Sentinel`/`Cluster` 不是"不可达"

§2fz.D-6 的队列项①说这两臂"不可达、留待下一轮决定是否删"。本轮先量再改，结论是**我的话说过头了**：这两个变体是 `pub enum` 的 `pub` 变体 ⇒ **任何直接调用者**都能构造 `RedisMode::Sentinel` 并传给 `pub async fn connect(...)`；只有"经由环境变量的那条路"被 `parse` 堵死（`parse` 除 `single` 外一律报错）。所以**它们不是死代码**，而是**面向直接 API 调用者的防御分支** —— 删掉变体会砍掉公开 API 形状，而保留并说清"为什么它们必须继续返回错误"才是对的。**处置（文档级，不删代码）**：`redis/mod.rs` 的 `connect()` 注释改为"**没有环境路径**能产生这两个变体（`parse` 在构造之前就拒绝），但**直接构造枚举的人**仍能到达 ⇒ 因此它们必须继续返回错误而不是穿透"；drill 的 K2 标签同步收窄为"经由 catch-all 拒绝；那条臂只对直接 API 调用者可达"。**教训（写进本会话的常备清单）**：`pub` 变体**永远**不是"不可达"——"不可达"只能针对某一条构造路径说，而这一点应当在**同一句话里**限定清楚。

### C. 本轮验证（门禁判定）

以最终修订重跑整条门禁：`.acceptance/round195-gate.out` ⇒ **98 条条目全部 `exit=0`、`OVERALL=GREEN`、`GATE_EXIT=0`**。门禁内证据：`[cluster-env] 10 cluster-only name(s); 7 literal read(s) …; 1 non-literal read(s) (all recorded); 2 recorded non-cluster name(s): OK`（整环境遍历现在计入并须记录）；`cluster-only env guard tests` **18/18**；`tree manifest (verify)` ⇒ `OK`。反向证伪：夹具里加一处**未记录的** `env::vars()` ⇒ 守卫 FAIL 并点名 site；记录后 ⇒ OK。`FALSIFY` = **0**。

---
## 2gb. 第一百九十六轮：两条"成功路径不可审计"的 OK 行 —— **算出来却不打印的审计**

### A. 缺口：两份守卫**跑了** UI 源清单审计，**成功时一个字都不说**

`check_i18n.js` 与 `check_e2e_contracts.cjs` 都跑同一条不变量（`admin-ui/` 下每个源文件要么被扫描、要么**记录在 `UNSCANNED_UI_OK` 里并带理由**）——这条不变量在**失败**时会打印（`UNSCANNED-UI-FILE` / `STALE-UNSCANNED-RECORD` / `UI_FILES` 缺失），但**成功**时 OK 行只报"扫了几个文件、查了几条 t() 字面量"，**完全看不出这次运行有多少结论是"靠记录豁免"得来的**。第 189 轮把这条审计建起来时，`UNSCANNED_UI_OK` 的条目数与"判定集合的大小"都算了出来，**却没进任何输出**（`check_i18n.js` 只把 `files/tKeys/unterminated` 放进 `stats`）。判据同族：**"运行过什么"必须在成功路径上可见**，否则读者无法区分"全部扫过"与"一半靠豁免"。

### B. 修法：把审计数字放进 OK 行（两个守卫，同一处口径）

* `check_i18n.js`：`stats` 增加 `unscannedJudged`（按 `SCANNED_EXTENSIONS` 判定的未扫描文件数）与 `unscannedRecorded`（`UNSCANNED_UI_OK.size`），OK 行插入
  `… 4 UI file(s) scanned, 1 file(s) deliberately unscanned and recorded (style.css), 139 t() literal(s) checked`（**并列出被记录的文件名**）。
* `check_e2e_contracts.cjs`：OK 行尾部加 `6 UI source(s) judged against the list, 0 recorded as deliberately unscanned (none)`。
* **★既有断言随之要改（这是好事）**：`check_i18n.test.cjs` 那条控制组原本断言 `!/widget\.vue/`（"被记录的文件名**不得出现**在输出里"）—— 新 OK 行**就是要**打印它。断言收窄为"**不得出现 finding**"（`STALE-UNSCANNED|UNSCANNED-UI-FILE` 都不许有）**并且**"审计行必须点名它"（`deliberately unscanned and recorded (widget.vue)`）⇒ 既保住原判据的牙齿，又把新行为钉住。**反向证伪**：把审计那两行从 OK 行里删掉 ⇒ 该套件 **2 处红**（控制组 + 真树用例），恢复后 `diff -q` 一致、`FALSIFY`=0。

### C. 本轮验证（门禁判定）

以最终修订重跑整条门禁：`.acceptance/round196-gate.out` ⇒ **98 条条目全部 `exit=0`、`OVERALL=GREEN`、`GATE_EXIT=0`**。门禁内证据：`i18n` 条目打印 `OK (348 en keys, 4 locales …, 4 UI file(s) scanned, 1 file(s) deliberately unscanned and recorded (style.css), 139 t() literal(s) checked)`（**成功路径现在报出审计**）、`i18n tests` = `ALL CHECK_I18N TESTS PASSED`、`e2e contract tests` = `ALL E2E CONTRACT TESTS PASSED`（OK 行含 "6 UI source(s) judged against the list, 0 recorded …"）、`tree manifest (verify)` ⇒ `OK`。反向证伪：删掉 OK 行里的审计 ⇒ `check_i18n.test.cjs` **2 处红**，恢复后 `diff -q` 一致、`FALSIFY` = **0**。

---
## 2gc. 第一百九十七轮：守卫把**注释**当成了证据 —— "读过这个名字"可以由一行 `///` 满足

### A. 缺口（本轮实测，且是这条守卫**自己声称**的规则的反面）

`scripts/check_cluster_env.cjs` 的 R2 写的是"表里每个名字必须有一个**字面读取点**"，但两个读取器都在**原始行**上做正则，**从不剔除注释**。用夹具实测（`.acceptance/fx196`）：表里列出 `HYDRA_NOTE_ONLY`，而它在整个树里的**唯一出现**是一行文档注释 —— `/// A COMMENT that mentions std::env::var("HYDRA_NOTE_ONLY") — never executed.` ⇒ 守卫打印 **OK**（`10 cluster-only name(s); 2 literal read(s) …`）。也就是说：**一行永远不会执行的注释，可以为一个"没人读的开关"背书**，而这条守卫的存在理由正是"名字必须挣得它的位置"。这正是本会话自己反复写下的那条判据 —— **注释/字符串永远不是执行的证据**（第 186 轮记录在"追加用例落在 `process.exit` 之后"那次自伤旁边）。

### B. 修法：跳过**整行注释**（并写明剩下的边界）

新增 `isCommentLine(line)`：行首（允许空白）以 `//`、`/*` 或 `*` 开头 ⇒ 跳过。两个读取器（`envReads` 的字面量/非字面量两支、`hydraLiteralReads`）都先用它过滤。
**为什么不是"从行内 `//` 起截断"**：那会制造**假阴性** —— `let u = "redis://x"; let _ = env::var("A");` 会丢掉一个**真实**读取，比原缺陷更糟。
**如实写明的边界**（进代码注释）：**行尾注释**里的名字、以及**字符串字面量**里的名字仍然算数；要正确处理它们需要一个 Rust 词法器，本轮不做。真实树今天没有 `env::var` 出现在注释里（`grep` 逐一确认），所以这是**潜在**缺陷。

### C. 测试与反向证伪

测试 +1（→ **19/19**）：表里列一个**只在注释里出现**的名字 ⇒ FAIL 并点名 `HYDRA_NOTE_ONLY is listed in CLUSTER_ONLY_ENV but nothing in crates/hydra-server/src reads it`；**控制组**：把同一句挪进**代码**里 ⇒ 通过。
**反向证伪**：让 `isCommentLine` 恒返回 `false`（即把注释重新算作读取）⇒ **恰好那条新用例红**（18 绿 1 红）；恢复后 `diff -q` 一致、`FALSIFY` = 0。

### D. 本轮验证（门禁判定）

以最终修订重跑整条门禁：`.acceptance/round197-gate.out` ⇒ **98 条条目全部 `exit=0`、`OVERALL=GREEN`、`GATE_EXIT=0`**。门禁内证据：`[cluster-env] 10 cluster-only name(s); 7 literal read(s) …; 1 non-literal read(s) (all recorded); 2 recorded non-cluster name(s): OK`、`cluster-only env guard tests` **19/19**、`tree manifest (verify)` ⇒ `OK`。`FALSIFY` = **0**。

---
## 2gd. 第一百九十八轮：**同一类缺陷在本会话里已有单一所有者** —— 该守卫却自己手搓了一份（并因此带着一个"已知限制"）

### A. 两处负结论先量出来

本轮本来要查"兄弟守卫是不是也把注释当证据"，量了**两处**，结论是**不必改**（如实记录）：

* `check_documented_env.cjs`：`directRead()` 确实用 `line.includes(...)`，但**行在更早的地方就被处理过** —— `.rs` 走 `stripCommentsAndTestItems(raw)`（同时把 `#[cfg(test)]` 项也清掉），其余语言走按语言区分的 `stripComments(raw, ext)`；它甚至把残留（"文档行里拼出 `process.env.HYDRA_X` 的字符串仍算数"）**写在头部**而不是装作没有。
* `check_documented_defaults.cjs`：同样共享 `rust_blank.cjs` 的 `stripCommentsAndTestItems`，而且头部**记着实测**（第 143 轮：`HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS` 曾被报在注释行 `usage_query.rs:340`，真实读取在 343；并且"唯一'默认值'来自注释的夹具当时能通过"）。

⇒ **"注释不是证据"在本会话里早已有单一所有者**（`rust_blank.cjs`），我在第 197 轮发现的问题**不是生态缺口，而是我自己那个新守卫没接上它**。

### B. 修法：接上单一所有者，顺手删掉一条"已知限制"

`scripts/check_cluster_env.cjs` 删掉手搓的 `isCommentLine()` 行首启发式，两个读取器（`envReads` 的两支、`hydraLiteralReads`）改为在 **`stripCommentsAndTestItems()`** 的输出上匹配。收益是**三件**、且第 197 轮文档里那一堆"边界"随之消失：

1. **行尾注释**也被剥掉（启发式只看行首 ⇒ 第 197 轮如实写下"行尾注释里的名字仍算数"这个边界，现在不需要了）；
2. **`#[cfg(test)]` 项**被清掉 ⇒ 头部那条 **KNOWN LIMIT**（"`src/cluster/` 里 `#[cfg(test)]` 读环境变量会需要一条记录"）**按构造消失**，不必再靠文字承认；
3. "注释即证据"不再由本守卫自己解释，而是与**两个兄弟守卫同一套语义**（第 197 轮我写的是一个只此一家的启发式 —— 单一所有者被复制成三份，正是本会话一直在收敛的那件事）。

### C. 测试与反向证伪

测试 +1（→ **20/20**），且新用例把两条**更强的**性质钉住：①**行尾**注释里的名字**不是**读者（比第 197 轮的行首注释强）；②`#[cfg(test)]` 模块里 `env::var("HYDRA_FROM_A_TEST")`（一个**不在表里**的名字）**不产生 finding** —— 这条曾经是"已知限制"，现在是规则。
**反向证伪**：把两处 `stripCommentsAndTestItems(...)` 换回裸 `readFileSync(...)`（即回到"注释算证据"）⇒ **恰好那两条注释用例红**（18 绿 2 红）；恢复后 `diff -q` 一致、`FALSIFY` = 0。

### D. 本轮验证（门禁判定）

以最终修订重跑整条门禁：`.acceptance/round198-gate.out` ⇒ **98 条条目全部 `exit=0`、`OVERALL=GREEN`、`GATE_EXIT=0`**。门禁内证据：`[cluster-env] 10 cluster-only name(s); 7 literal read(s) …; 1 non-literal read(s) (all recorded); 2 recorded non-cluster name(s): OK`、`cluster-only env guard tests` **20/20**、`tree manifest (verify)` ⇒ `OK`。`FALSIFY` = **0**。

---
## 2ge. 第一百九十九轮：把"注释/实测数字"这条线**量到底**（三个负结论）+ 对**耐久记录本身**做一次数字核对

### A. 同一族的收尾：还有谁在读"原始行"？

第 197/198 轮修的是 `check_cluster_env.cjs`（唯一一个把注释当证据的守卫）。本轮把**其余读 Rust 的守卫**逐个量过，结论三条**负结论**（不必改，但留下测量）：

| 守卫 | 读代码的方式 | 结论 |
|---|---|---|
| `check_tenant_error_codes.cjs:357-360` | `stripCommentsAndTestItems(readFileSync(f))` | 已共享单一所有者（注释与 `#[cfg(test)]` 项都清掉） |
| `check_source_purity.cjs:87,147` | `stripTestItems(stripComments(...))` | 同上，注释甚至写明"三个守卫依赖它" |
| `check_alert_expressions.cjs:31,127` | `stripCommentsAndTestItems(...)` | 同上 |
| `check_documented_env.cjs` / `check_documented_defaults.cjs` | 见 §2gd.A | 同上（前者还把残留写在头部） |

⇒ **`rust_blank.cjs` 是"什么算代码"的单一所有者，第 197 轮那个缺陷只是"我这个新守卫没接上它"**；本轮之后接上的守卫是**第四个**（`rust_blank.cjs:85` 的注释仍写"three guards now depend on it"——**该注释本身已过期**，见 §2ge.B）。

### B. 对**耐久记录本身**做数字核对（本轮唯一"输出"是一次审计，结果：全部对得上）

"注释里引用的实测数字必须与事实一致"这条纪律，本会话一直只用在**代码注释**上。本轮把它掉转过来用在**计划文件**上：把最近几节（§2gb–§2gd）与 `INDEX.md` 末尾的数字逐条与实测对齐。

| 记录里的说法 | 实测 | 结论 |
|---|---|---|
| 门禁 **98** 条条目 | `grep -c '^gate ' .acceptance/round10-gate.sh` = **98** | ✅ |
| `cluster-only env guard tests` **20/20** | `node --test …` = **20 ok** | ✅ |
| OK 行 `10 cluster-only name(s); 7 literal read(s); 1 non-literal read(s) (all recorded); 2 recorded non-cluster name(s)` | 逐字一致 | ✅ |
| 页面 `advertised 814 == measured 814` | 逐字一致 | ✅ |
| `tree manifest` 的 **357 file(s)** | `--check` 报 357 | ✅ |

**唯一"可疑"的两处 `88 条条目`**：都不是陈旧的断言，而是**历史记录 + 取数指引** —— `plans/…:5254` 是"当时把 13 改成 88"的处置记录；`plans/…:6911`（§4）写着"它现在有 **88 条条目** —— **数字随文件增长，请用 `grep -c '^gate ' …` 取当前值**"。后者正是**正确**写法（把"会漂移的数字"换成"取数方法"），因此**不改**。

**顺手量到一条真·过期注释（本轮唯一的代码改动）**：`scripts/check_source_purity.cjs:85` 写着 "a single owner, because **three** guards now depend on it"，而实测依赖方是 **7 个**（`grep -rln "rust_blank.cjs" scripts/*.cjs`）。⇒ 改成**实测数字 + 取数方法**，而不是再写一个会过期的数字。★**我第一次写这段记录时把文件名写成了 `rust_blank.cjs:85`** —— 本轮审计的那类错误（引用与事实不符）发生在**记录自己**身上：核对时才发现注释在 `check_source_purity.cjs`，已改正。**"先 grep 路径再引用"对记录同样适用**。

### C. 本轮验证（门禁判定）

**★本轮不重跑门禁，理由要写准（第一次我写错了）**：改动是 **`check_source_purity.cjs` 的一处注释** + 两处记录。`tree_manifest --check` 对当前树报 **"1 code file(s) changed"** —— 也就是说 **`.cjs` 属于"代码"**，只有 `docs/`、`dev-docs/` 才算 docs-only；**"注释级改动不使门禁失效"这句话对 `.cjs` 是错的**（我第一次就是这么写的，核对 manifest 时改正）。
因此本轮的证据是**成比例的、而不是全门禁**：**①** 上一轮 **98/98 GREEN** 的完整门禁（`.acceptance/round198-gate.out`）仍然成立（它的树与本轮的差别**只有这一行注释**）；**②** 该守卫**自己的**入口复跑：`node scripts/check_source_purity.cjs` = `exit 0`（`63 file(s) scanned; 3 crate root(s): clean`）、`node --test scripts/check_source_purity.test.cjs` = **0 红**；**③** `check_cluster_env` / `check_test_tails` / `check_gate_entries` / `check_ci_wiring` 复跑全 `exit=0`，`FALSIFY` = 0。
**★限度已在下一轮（第二百轮）消除**：`.acceptance/round199-gate.out` ⇒ **98 条条目全部 `exit=0`、`OVERALL=GREEN`、`GATE_EXIT=0`**（首条 `tree manifest (record)`、末条 `tree manifest (verify)` 均 `exit=0`，`OK (the source tree is unchanged …)`）—— 即**这一行注释的树现在有完整门禁覆盖**，本节的取舍没有留下未覆盖的改动。若要求"每一轮都以全门禁收尾"，则本轮应当重跑 —— 这里按**代价/收益**做了取舍，并把取舍写在这里而不是藏起来。

---
## 2ge2. 第二百轮：补上上一轮**自己声明的限度** —— 对当前树重跑完整门禁

### A. 为什么单列一轮

§2ge.C 记着一个**明确的限度**：第 199 轮的改动是 `check_source_purity.cjs` 的**一行注释**，而 `.cjs` 被 `tree_manifest` 视作**代码**（实测报 `1 code file(s) changed`）⇒ 那一行注释**没有**经过完整门禁，我当时声明"下一个代码改动会重新覆盖它"。把一个**已知未覆盖的改动**留在树上不管，正是本会话反复教训的那件事（"能失败才算证据"），所以本轮不做新事、只把它兑现。

### B. 做法与结果

对**当前树**（与上次 GREEN 的差别**仅这一行注释**）重跑整条门禁：`.acceptance/round199-gate.out` ⇒ **98 条条目全部 `exit=0`、`OVERALL=GREEN`、`GATE_EXIT=0`**；首条 `tree manifest (record)` 与末条 `tree manifest (verify)` 均 `exit=0`，verify 报 **`OK (the source tree is unchanged: 357 file(s), docs-only changes: 0)`**（**零漂移** —— 这个树被完整判定过）。开跑前照门禁头部的**前置说明**做了 `cargo fmt --check` 与 `check_public_claims --measure`（均干净），因此**一次通过、无 RED**。

### C. 收口

§2ge.C 的限度**已消除**：`.cjs` 的注释改动现在由一次真实运行兑现，而不是靠"文档级改动不使门禁失效"的推理 —— 何况那句推理对 `.cjs` 本来就**是错的**（第 199 轮写错、核对 manifest 时改正）。

## 2gf. 第二百零一轮：把**已经决定过的**策略补齐到剩下的调用点 —— 集群控制客户端的 `lock().expect(…)` 与 `lock_gate` 先例不一致

### A. 起点：为 D-13 备料时量出的**内部不一致**（不是新政策）

D-13 记的是"生产代码里的 `expect` 到底禁不禁"（实测 **8 处**，守卫 `check_source_purity.cjs` 每次运行都会逐条打印）。本轮先把这 8 处的**理由**逐条读出来备料，其中三类：

* `reqwest` 客户端构造（`control_client.rs:97`、`forward.rs:203`、`provider_client.rs:131`）—— 常量配置、消息自己论证 "infallible"；
* 不变量式解包（`admin/mod.rs:432` 的 "admin SQLite pool (leader mode only)"、`main.rs:1367` 的 "cert store is built whenever a TLS listener is configured"）；
* **`Mutex::lock()`（`control_client.rs:142`、`:176`、`:243`）** —— 而这三处**与本仓库已经决定过的策略相矛盾**：`proxy/admission.rs` 里 `lock_gate()` 的文档写着"一个**无关线程**的 panic 不该让经过该锁的**每一次**后续请求都 panic"，并有一个**故意毒化锁**的测试钉着（`admission.rs:600-610`：`panic!("poison the gate mutex on purpose")` 之后仍必须正常工作）。

⇒ 这**不是**要替 D-13 做决定，而是**同一策略在两个模块里不一致**：控制客户端的 `url` 锁与 admission 的两个锁是**同一类值**（一份整体替换的快照），没有"写一半的不变量"。

### B. 修法：把 `lock_gate` 提成**单一所有者**，三处调用点接上

1. `crates/hydra-server/src/lib.rs` 新增 `pub(crate) fn lock_gate<T>(m: &Mutex<T>)`（文档记下来源与判据：**守卫值是整体替换的快照 ⇒ 恢复；真的要代表"写一半坏了不变量"时才保留 `.expect`**）；
2. `proxy/admission.rs` 删掉**私有副本**、改用 `use crate::lock_gate;`（消除"同一语义两处实现"）；
3. `cluster/control_client.rs` 的**三处生产**调用点（`:142`/`:176`/`:243`，均为 `self.url.lock().expect("control url mutex")`）改为 `crate::lock_gate(&self.url)`。

### C. 验证（本轮证据）

* `HYDRA_TEST_REDIS_URL=… cargo test -p hydra-server --features server,cluster-redis,usage-clickhouse --lib 'proxy::admission'` ⇒ **1 passed / 0 failed**（`lock_gate` 的毒化锁测试仍在，即**共享函数的契约有测试**）；`--lib 'cluster::'` ⇒ **70 passed / 0 failed**；`cargo fmt --check` 干净。
* **机械可观测的收据**：`check_source_purity.cjs` 的生产 `.expect(` 站点从 **8 → 5**（逐条打印，若有人把调用点改回 `.expect(...)`，这个数字会立刻回到 8 —— 可见，但不是失败；把它变成**失败**属于 D-13 的政策决定，本轮不做）。
* **测试覆盖的诚实说明**：三处新调用点**没有各自的毒化锁测试**（构造 `ControlClient` 需要 store/key provider，代价高于收益）；它们复用**同一个函数**，而该函数的契约由 `admission` 的毒化测试钉住 —— 这是"单一所有者"的直接推论，不是"看起来像有覆盖"。

### D. 本轮验证（门禁判定）

以最终修订重跑整条门禁：`.acceptance/round201-gate.out` ⇒ **98 条条目全部 `exit=0`、`OVERALL=GREEN`、`GATE_EXIT=0`**。门禁内证据：`source purity` 条目的 info 行现在打印 **5 production .expect(...) site(s)**（本轮 8 → 5）、`source purity tests` 与 `cluster::`/`admission` 套件全绿、`tree manifest (verify)` ⇒ `OK`。`FALSIFY` = **0**（本轮无反向证伪：改的是"用哪个函数"，其契约由既有毒化测试钉住，见 §2gf.C 的诚实说明）。

---
## 2gg. 第二百零二轮：把上一条线**收口**（负结论）—— 生产代码里已无 `lock().expect(…)`

### A. 本轮是一次核对，结论是"这一类已经清零"

上一轮把 `lock_gate` 提成单一所有者、并接上控制客户端的三处**生产**调用点。本轮核对"还有没有别处"：

* `grep -rn 'lock()\.expect\|lock()\.unwrap' crates/hydra-server/src` ⇒ **33 处**，逐条看**全部落在 `#[cfg(test)]` 代码里**（测试时钟 `lease.rs`、调用记录器 `replica.rs`、`forward.rs`、以及 `control_client.rs` 自己的测试）。
* 权威判据是守卫自己的**生产视图**（它对每个文件剥掉注释与 `#[cfg(test)]` 项）：`check_source_purity.cjs` 现在打印 **5 production .expect(...) site(s)**，逐条读出为
  `admin/mod.rs:432`（"admin SQLite pool (leader mode only)"）、`main.rs:1367`（"the cert store is built whenever a TLS listener is configured"）、`control_client.rs:97` / `forward.rs:203` / `provider_client.rs:131`（三处 reqwest 常量构造，消息自述 "infallible"）——
  **其中含 `lock()` 的：0 条**。⇒ **生产代码里已经没有任何"锁中毒即 panic"的调用点**，第 201 轮那条线收口。

### B. 为什么"测试里的 `.expect`"不算（并且**不改**）

测试里的 `lock().expect("test clock")` 等是**有意为之**：测试要的是"中毒就是测试写错了"这一强断言（被测对象是本机 mutex，没有跨线程 panic 的现实风险），而"无关线程 panic 不该连坐"这条判据针对的是**共享给所有请求的运行时锁**。**改为 `lock_gate` 反而会削弱测试**（把"测试自身出错"变成静默继续）。判据写在这里，避免下一轮把 33 处误当缺口重做。

### C. 本轮无代码改动 ⇒ 不重跑门禁（判据此时是**成立**的）

本轮只读、只写记录 ⇒ 无代码移动。`tree_manifest --check` 报当前树相对上次记录**只有 docs 变化**（下一步核对），因此**第 201 轮的 98/98 GREEN 仍然覆盖这棵树**。这与 §2ge.C 的情形不同（那次改的是 `.cjs`，被 manifest 视作**代码**，故补跑了一次完整门禁）—— 判据按 manifest 的分类走，而不是按"我以为的性质"走。

---
## 2gh. 第二百零三轮 + 第二百零四轮：两轮**只写记录**的轮次（结构核对 / 再派复审）

这两轮没有代码改动，按第 203 轮立下的判据（"记录的两半必须同构：每轮在计划里有章节、在 INDEX 里有段落"）补记于此 —— **否则第 203 轮那条判据会立刻在自己身上失效**。

### 第二百零三轮：对**记录自己**做结构核对，补上一处真缺口

把两半**机械对齐**核对（`grep -o '^## 2[a-z]*\. 第…轮'` vs INDEX 的轮次标记）：第 199、201、202 轮都有计划章节，**唯独第 200 轮只有 INDEX 段落** ⇒ 按时间顺序补 `§2ge2`。**判据**：只有一半意味着"读计划的人看不到某一轮，或读 INDEX 的人看不到其证据"，与一直在修的"记录与事实不符"同族（这次不一致发生在**记录内部**）。★附记一次**方法自伤**：我第一次用 `tail -12` 取窗口做核对，一度误判 201/202 也缺失；逐条定位后才发现真正的缺口是第 200 轮 —— **核对方法本身也需要"先确认取数方式"**，与本轮判据是同一件事。

### 第二百零四轮：池子空时的既定做法 —— 再派两路**只读**对抗性复审

连续九轮（195–203）产出都是自我一致性/记录完整性，故不再找同类小事，而把复审派到**尚未审过的角度**：①**对本会话最近五轮（195–203）自己的改动**做对抗性复审（`check_cluster_env` 的 site 键与 `rust_blank` 剥离是否可能**藏掉真实读取**、两个 OK 行的审计数字是否与判定集合一致、`lock_gate` 提升在**各特性组合**下是否行为保持、新测试是否会"因错误原因通过"），判据写明是"**这五轮有没有引入比修掉的更糟的缺陷**"；②**对 D-8/D-13/D-15/D-16/D-17/D-5 六项决策的事实前提重新测量**（上次第 176 轮，此后树变化很大），目的是让**答复的那一刻依据就是最新的**。两路均只读，结果并入下一轮处置。

**门禁判定**：两轮均无代码改动 ⇒ `tree_manifest --check` 报 **`OK (357 file(s), docs-only changes: 2)`** ⇒ **第 201 轮的 98/98 GREEN 仍覆盖这棵树**。

## 2gi. 第二百零七轮：第 204 轮派出的**第二路**复审（对本会话最近五轮自我改动的对抗性复审）返回

**判定：P1 = 无**；结论句是"rounds 195–203 没有引入比它修掉的东西更严重的缺陷"，唯一值得当缺陷处理的是 P2#1（且它是**第 198 轮那次 site 键修法的残余**，不是这九轮新增的洞）。以下逐条记录，**本轮不改代码**（理由见 §2gi.D）。

### A. P2#1（真缺陷，未修）：`NON_LITERAL_READ_OK` 的记录只按**序号存在性**背书，不按**身份**

`scripts/check_cluster_env.cjs:281-284` 的裁决是"该文件第 N 个非字面量读取**存在**"，而记录值是**理由文本**，理由是就**某一个具体 site** 写的（"the `CLUSTER_ONLY_ENV` loop in `NodeRole::from_env`: it reads exactly the table, by construction"）⇒ **A 的理由可以背书 B site**。
复审用 /tmp 夹具实测：文件里 site #1 = 新增的 `std::env::var(name)`（名字 `HYDRA_NEW` 不在表里）、site #2 = 表循环，记录键仍是 `mod.rs#1` ⇒ `2 non-literal read(s) (all recorded) … OK exit=0`；**把两条记录的理由对调后同样 exit=0**。反向对照（只 1 条记录／无记录）确实红 ⇒ 不是 `applies` 失效，而是**身份未被校验**。
第 198 轮那条注释（"Ordinals instead of line numbers so ordinary edits above a site do not churn the record"）**恰好把这性质写成了优点** —— 在某个 site 之上插入/删除一个非字面量读取，记录会**静默改绑**。
**候选修法（复审给出）**：记录值带上**期望源码锚点**（或让 `applies` 比对 `nonLiteralFiles.get(file)[ordinal-1]` 所在行的文本/变量名），失败信息里打印被接受的 site 那一行。**我另想到一个更便宜的中间版**：把键写成 `file#i_of_N`（序号 + 该文件 site 总数）—— 能挡住"在上方插入/删除"这一被实测的场景（N 变了 ⇒ 旧键不再匹配），但**挡不住"替换成另一个 site"**（形状不变）；要完全挡住必须存内容锚点。

### B. P2#2（真，小）：第 199 轮那条"修正"仍不准确

`scripts/check_source_purity.cjs:85` 现在写 "shared by **SEVEN guards** (re-measure with `grep -rln \"rust_blank.cjs\" scripts/*.cjs`)"。复审实测：`grep -rln … | wc -l` = **9**（其中 `check_cluster_env.test.cjs`、`rust_blank.test.cjs` 只是提到文件名）；`require("./rust_blank.cjs")` 的真消费者是 **7 个**，且其中一个（`rust_blank.test.cjs`）是**测试**而非守卫 ⇒ "SEVEN guards" 在"守卫"这个词上仍不成立。**修法**：只留取数命令（或写"7 个消费者，其中含 1 个测试"），不要写会再次漂移的数字 —— 这正是我第 199 轮想修的那件事，**改了一次仍不对**。

### C. P3（两条，都是最近五轮我自己引入的）

* `scripts/check_i18n.js:400`：`unscannedRecorded: UNSCANNED_UI_OK.size` **算了但从未打印**（OK 行用的是 `stats.unscannedJudged` 与 `UNSCANNED_UI_OK.keys()`）⇒ 第 196 轮"把算出来的审计打出来"漏了这一项，删掉即可（无行为影响）。
* `crates/hydra-server/src/lib.rs:30-34`：新增的 `lock_gate` 插在 `crypto` 的**文档注释与其 `#[cfg(feature = "db")]` 之间**，把两者隔开（rustdoc 归属正确，但读起来是一个悬空注释）⇒ 把该函数移到文件末尾或 `crypto` 段之后。

### C2. 复审判为 CLEAN 的部分（负结论，附测量）

`env::vars()` 支（round 195）双向断言且套件 **20/20**；两个 OK 行的审计数字/名单与判定集合一致（`style.css` 1 条对 1 条、e2e `6/0` 与 `UI_FILES.length` 一致）；**共享 stripper 不会遮住真实读取** —— `stripTestItems` 按整行置空、行号不漂移，复审用独立掩码器对比两棵 crate 共 **58 个 test item、0 行过度掩码**，并用"消字面量/留字面量"双跑对齐；`check_documented_defaults` 打印的行号与磁盘逐条一致；**`lock_gate` 三处调用点行为等价**（同一把 `Arc<Mutex<String>>`、同样的 `let mut guard`/`.clone()` 语义，无跨 `.await` 持锁；`pub(crate)` 足够）；**特性组合**无 `dead_code`/悬挂引用（`without cluster-redis` 时三处随之消失）；**新测试不是空断言**（用的是旧实现必然失败的行尾注释与 `#[cfg(test)]` 形状）。**复审自陈的限度**：`docs/index.html` 的 814=258+556 与完整门禁需跑 cargo，本轮未测。

### D. 为什么本轮**不改代码**（如实说明，而不是"已修"）

以上四项都不需要任何决策，且都不大 —— 但本轮我的可用上下文已接近耗尽，而**任何代码改动都会让"当前树已被 98/98 门禁覆盖"这个结论失效**（`.cjs` 被 `tree_manifest` 视作**代码**），需要重跑约 40 分钟门禁才能重新成立。在**无法把"改 + 测 + 反向证伪 + 门禁"四件事完整做完并验证**的情况下动代码，正是本会话反复记录的失误模式（改一半、留下未覆盖的改动、把"我以为"当证据）。因此本轮选择：**把发现精确记录下来（含复审的实测与它自陈的限度），保持树处于已验证状态**，等下一轮（或你的指令）带着完整预算执行。

---
## 2. 批次 1（剩余清单）

按"先止血、后一致性"排序；✅ 的六项已在 §1b 完成并逐项反向证伪。

- ✅ 1. 上游自动重定向未禁用 → 随 302 外泄 Anthropic 的 `x-api-key`（§1b P1-1）
- ✅ 2. 下游请求体读取无总期限（§1c P1-2）→ 已完成，原描述保留如下供追溯：**下游请求体读取无总期限**（h2 连 per-read 期限都没有）→ 匿名客户端可永久占住 worker。
  修：Hydra 层加总期限/无进展期限 + 408 + 断连。判定：h1 每秒发 1 字节的 slow-loris 必须在期限内被 408 掐断。
  （注：上游**响应体**那一半已在 §1b P1-4 修好；这里指的是**下游请求体**，仍未做。）
- **3. 首字节超时直接 502、不再故障转移**（`retry_after_connect` 恒 false 的死配置把它锁住）。
  ⚠ **这一条需要产品决策**：代码把它当"可能已计费"处理是**有意的**（请求已发出，上游可能已生成/计费），
  而复审视角认为"上游一个应用层字节都没产出 ⇒ 可以安全重试一次"。两者都成立，取哪个是成本 vs 可用性的取舍，
  不能由实现者单方面改。判定（若决定允许）：第一个 provider 静默不答、第二个 200 时报 200，而非 502。
- ✅ 4. 硬编码 300s 的 client 级 total timeout 截断长流（§1b P1-4）
- ✅ 5. ClickHouse 提交后重试导致重复计费（§1e P1-5）→ 批次级已修，行级残余已记录；原描述：**ClickHouse 提交后重试导致重复计费**（既有未收口，CODE_REVIEW §19）。修：批次幂等键 +
  `insert_deduplication_token` / 去重引擎。判定：注入"已提交但响应丢失"后 `count()` 必须为 1。
- ✅ 6. DEFERRED 事务下 `busy_timeout` 失效（§1b P1-6，含只读快照必须保持 DEFERRED 的边界）
- ✅ 7. `key_prefix_binding` 前缀只校验非空、全局作用域（§1b P1-7）
- ✅ 8. 失效事件流：`XTRIM MAXLEN` 使每次 trim 都 bump（§1d P1-8）→ 已完成；原描述：**`XTRIM MAXLEN` 使每次 trim 都 bump** → >333 事件/秒时全集群每 30 s 清空 L1+L2；
  单个租户 2 次 invalidation/分钟即可永久维持。修：按最慢存活节点水位改 `XTRIM MINID` 裁剪。
  ⚠ 需要真实 Redis 才能验证（dev-plan 铁律 2 禁止 mock），本轮环境没有，留到有 Redis 的一轮。
- ✅ 9. 副本物化重试预算耗尽后永久不可当选（§1f P1-9）→ 已完成；原描述：**副本物化重试预算（每快照 3 次）耗尽后节点永久不可当选**，且自愈所需的快照本身要靠一次管理写。
  修：改为按时间限速重试；预算耗尽加 gauge。
- ✅ 10. 主密钥无轮换路径（§1h P1-10）→ 已完成；原描述：**主密钥无轮换路径**：`key_version` 硬编码 1，换 key = 全部密文不可解 + 拒绝启动 + 无恢复工具。
  修：`HYDRA_ENCRYPTION_KEY_VERSION` + 离线 re-seal；工具就绪前先把恢复边界写进 `ops.md`。
- ✅ 11. `/metrics` 鉴权随角色漂移（§1g P1-11）→ 已完成（统一为所有角色都需 token）；原描述：**`/metrics` 鉴权随角色漂移且与随包 UI 文档相反**（leader 上抓取须把根凭据写进 Prometheus 配置；
  edge 上免鉴权且文档要求绑 `0.0.0.0`）。修：统一口径 + 文档一致 + 明确抓取凭据。
- ✅ 12. SNI 查表区分大小写（§1b P1-12）
- ✅ 13. `config_version` 解析失败静默降级为 0（§1c P1-13）→ 已完成；原描述：**`config_version` 解析失败静默降级为 0** → 内容为空的副本可能被误判"已同步"并可当选。修：三态化 + fail-closed。
- ✅ 14. 管理面认证无限速/无锁定/无指标（§1g P1-14）→ 已完成；原描述：**管理面认证无限速/无锁定/无指标**（唯一记录是默认被过滤掉的 `debug!`）。修：复用租户面 `Throttle` + 计数器 + `warn!`。
- ✅ 15. SDK 把 200/202/503 三态压成 pass/fail、把 503 当节点故障（§1b P1-15）
- ✅ 16. 失效/缓存链路指标 + `del_all_tenants` 早退 + 谎报 cache_size 0（§1d 补 trimmed/bumps，§1f 补齐其余）→ 已完成：bump 次数、L2 清失败、`hydra_auth_cache_size` 谎报 0。修：加
  `hydra_invalidation_generation_bumps_total`、`hydra_auth_cache_clear_total{layer,result}`，
  L2 清失败时不要写 0，`del_all_tenants` 不要早退。

### 需决策项（实现者不单方面改；每轮重复列出直到有结论）

| # | 决策 | 两个选项 | 建议 / 影响 |
|---|---|---|---|
| D-1 | 首字节超时是否允许故障转移（§2 第 3 条） | 允许一次重试 / 保持 502 | 成本 vs 可用性；`retry_after_connect` 现恒 false。**第二百一十轮补测（2026-10-09）**：该字段确实**被读**（`proxy.rs:1214`：`!never_reached_upstream && !retry_after_connect` 才走 502 分支），但**没有任何 env / 配置入口能把它打开**（全仓只有 `proxy/config.rs:36` 的字段与 `ProxyConfig::default()` 的 false）⇒ 现实里它是一个**打不开的开关**。~~建议：**保持 502**（重放已发出的请求可能让客户被上游计费两次），并把该字段要么接上明确入口、要么删掉，别让它继续看起来可配~~ ★**已落地（2026-10-09，P3-1）**：选择"删掉"——字段连同空壳的 `FailoverConfig` 一起从 `proxy/config.rs` 删除（首字节超时一律 502、不重放，防双计费的 fail-safe 方向），`tests/terminate_mode.rs` 回归测试同步改名；`ops.md` §7 的"已删除"标注补上字段级确认 |
| ~~D-2~~ | ✅ **已解决（第八十一轮，按"文档既有契约"这条路走）**：`admin_api` 两例改为断言**已发布的 200/202/503 三态**而不是写死的 200 | —— | 决策依据不是偏好而是证据：`admin-ui/api-docs.js:43-47` 与 `ops.md` §5.1 **早就写明**该端点是三态（`200 applied` / `202 pending`＋lagging / `503 unavailable`，"只有 200 才叫 done"），而"空 live set ⇒ pending"更已被 `tenant_api_cluster.rs::barrier_reports_pending_not_applied_when_the_live_set_is_empty` 钉成**有意语义**；这两例的 `AdminState` 从未注入 fleet 视图（人工构造）⇒ 它拿到的 202 是**正确答案**，写死 200 才是陈旧的。修法见 §2bn（附两条反向证伪）。可选套件由此**首次全绿**（`--no-fail-fast`：0 failed） |
| ~~D-3~~ | OC-4 行为侧：leader 数据面写端点 | —— | ✅ **已随退役消失，不需要决策（第二百一十轮核实，2026-10-09）**：转发层整体删除（commit `37f0bc3` "the entry node applies its own config write"，T3.5 / D-6 乙-full）⇒ `crates/hydra-server/src/cluster/forward.rs` **文件已不存在**，全仓 `grep -rn "no_leader"` = **0 命中**，而租户契约文档 `tenant-api-integration.md:442` 已写明"**任何节点都接受并执行租户写**，LB 指向哪台都可以"。本轮只清掉两处退役残留：本行 + `tenant_api/handlers.rs` 里那段还在讲已不存在的 `TenantConfigForwarder` 的注释块（以及同一函数里指向已删除模块 `admin::tenant_config_api::throttle_gate` 的 (2) 段） |
| D-4 | OC-6：DELETE 外来行 | 也回 204（区分只进审计）/ 承认可区分并改文档契约 | **第二百一十轮已按"行为为准"落地一半（2026-10-09）**：`admin/sub_tenant_write.rs` 的枚举级注释原称"缺失与外来不可区分（无 oracle）"，而代码一直是**缺失 → 幂等 204、外来 → 404**；该注释已改成实话并写明这是**有意**的（对"什么都没删"的请求回 204 是更大的谎，而 id 是 `gen_id()` 的纳秒串、不可枚举 ⇒ 这个 oracle 不可利用）。 **★已决（2026-10-09，收敛：保持 404）**：404 是对"你无权/不存在"的准确信号，把外来行与缺失折叠成 204 会让语义更糟；该决策项随注释修正关闭，不再占决策位。 |
> **前提复测（第 204 轮派出的只读复审，结果在目标第 196 轮到达）** —— 目的：让决策**在被答复的那一刻依据就是最新的**：
> * **D-8 成立**：`admin/mod.rs:905-910`（edge 分支只放行 healthz/readyz）、`:529-532`（无 token ⇒ false）、`:967-968`（门禁拒绝）、`:206`（401）；官方拓扑 `environment/docker-compose.cluster.yml:138-149` 的 edge **只有** `HYDRA_CLUSTER_TOKEN`；本地 compose 三个服务都带 token。计划内 `cluster.md:17`、`ops.md` §9.1 引用**未失效**。**未实测端口**（未起服务），结论由静态门禁链支撑。
> * **D-13 已变化**：生产 `.expect(...)` 为 **5 处**而非 8 处（第 201 轮把三处 `lock().expect(…)` 接上 `lock_gate` 后的收据；守卫逐条打印 `admin/mod.rs:432`、`cluster/control_client.rs:97`、`cluster/forward.rs:203`、`main.rs:1367`、`proxy/provider_client.rs:131`）。正文里出现的「8 处」是**当时**的数字。
> * **D-15 成立，行号陈旧**：`proxy.rs:834` 现为 **`proxy.rs:838`**（另一构造点 `:1513-1519`；bucket 所有权 `hydra-core/src/limit.rs:100-106`；两实现 `proxy/limiter.rs:298-314`、`redis/rate_limit.rs:402-408`；Redis 键 `rate_limit.rs:331`）。掩码碰撞由复审**独立复算**，非引用。
> * **D-16 成立**（`db.rs:1288-1293`/`:1311`/`:1333`/`:1345-1349` 裸列 vs `db.rs:568`/`:604` 的 `kp.seal`；跨节点明文 `cluster/snapshot.rs:117-123` + `db/restore.rs:242-249`）。★复审**如实声明限度**：「全仓唯一明文凭据列」只做到**近邻核对**（同 schema 内未见别的明文凭据列）＝"未找到反例"，**不是穷举证明**；要把该命题写进结论需另做专门取证。
> * **D-17 成立**（`admin/handlers.rs:1462-1467`、`:1496-1503` 无 `warnings` 字段；reload 只 `tracing::warn!`，文案 `hydra-core/src/config.rs:318-380`；`ops.md:566-573` 明写 not in the HTTP response body）。**未起服务实测响应体**，结论由返回处代码形状支撑。
> * **D-5 成立**：`.gitignore:71` 是 `/.acceptance/`（**整目录**）⇒ 取反**只能**是目录级白名单（先放开父目录再 `!` 白名单）；原措辞易被读成"按扩展名忽略"。

| D-5 | 门禁脚本放哪（第十一轮 P1） | 移入 `scripts/gates/` / 保留 `.acceptance/` 但改 `.gitignore` 取反其 `.sh`+`.py` | ✅ **已落地（第一百零九轮，走②）**：`.gitignore` 由 `/.acceptance/`（整目录）改为目录级白名单（`/.acceptance/*` + `!*.sh` + `!*.py`，并排除草稿），**证据链 15 个脚本入库**；嵌套 `roundNN/` 仍忽略。详见 §2gk。 |
| D-16 | `limit_role.matching_key` 是**全仓唯一以明文存活的客户端凭据列**吗、要不要封存（第一百一十七轮，对抗性复审 F1 实测核对） | ① 用 `kp.seal` 封存该列（与 provider api-key 对齐）——但匹配发生在**配置加载时**，需要一个"读出来解封"的路径，且 `SnapshotWire` 的保真载荷与 `db/restore.rs` 也要跟着改；② 不封存，但**文档只推荐掩码形式**（第一百一十七轮已做，`ops.md` §4 + `design.md` §10.1/§9.5）并接受"写原始 key = 明文活凭据"；③ 让 `matching_key` 只接受**摘要**（presented key 求同一摘要再比），彻底不存密钥形态 | **实测（第一百一十七轮只读核对）**：`db.rs:1288`/`db.rs:1311` 是裸列（**没有** `kp.seal`，而同一文件的 provider api-key 在 `db.rs:568` 是 `kp.seal(...)`）；`cluster/snapshot.rs:119` 的 `FidelityWireRows.limit_roles` 是**明文** `LimitRole`（同载荷的 `sealed_provider_keys`/`tenant_token_hashes` 是密封的）；`db/restore.rs:242` 原样落库到**每个** edge；`GET /api/v1/limit-roles` 原样回显（drill 的前置断言就读的它）、admin UI 直接渲染。即：写原始 key ⇒ 活凭据明文进每个节点的库、每份备份与每次管理面响应。 ★**已落地（2026-10-09，第二百一十轮）**：用户裁定 ①（全封存 + 存量行读时自动封存），本轮实施完毕 —— 封存形态是**单列文本信封** `sealed:v1:<版本>:<base64(nonce‖密文)>`（`crypto::Sealed::{to_text,from_text,open_text}`），三条写路径（`db::{insert,update}_limit_role`、`db/restore.rs`、配置树编码器 `sealed_limit_role`）全部走它，三条读路径（`get_limit_role`/`list_limit_roles[_on]`、树解码 `opened_limit_role`）解封；存量明文行由加载器 `db::seal_legacy_limit_keys`（读 + CAS 写）自动重封存，主密钥轮换 `reseal_secrets` 增加第三块 + `limit_keys_resealed`。**未选 ③**：摘要形式作为**形式**继续存在（D-16③ 已在第一百零九轮落地），但它与"封存"是两件事：封存保护库/备份/副本，摘要连持主密钥者也无法还原。详见 §2gl。**仍待你取向的一个附带决定（同一件事的第二半）**：`GET /api/v1/limit-roles` 现在**仍明文回显** `matching_key`（管理 UI 列表直接渲染、编辑表单预填）。选项：① 保持现状；② 回显**掩码或摘要**（库里是明文则回 `mask_key(它)`；掩码形式能匹配同一把 key，所以复制回显值再 PUT 回去**不改变限流行为**，只改变存储形态）。★**已落地（2026-10-09，P3-4，附带决定选中"回显掩码/摘要"）**：`GET /api/v1/limit-roles` 与 `GET /limit-roles/:id`（admin 三个回显出口）把 `matching_key` 改为**掩码回显**——digest（`sha256:`）原样保留（不可还原），其余（raw/mask）走 `mask_key`；写端点响应体（`limit_role_body`）同样掩码。管理面从此不再携带可还原的客户端凭据，"全仓唯一明文回显例外"关闭。参考既有政策：provider api-key 永不回明文 |

| D-17 | **admin 写入的 HTTP 响应要不要回带该角色的校验告警**（第一百三十八轮实测：写入路径会 reload ⇒ 告警**已经**在写入那一刻进 leader 日志，但响应体里没有；`201` 干净不代表角色没问题） | ① 在 `POST/PUT /api/v1/limit-roles` 的响应里加一个 `warnings` 数组（对外形状**增量**变更，需同步 `ops.md`/admin 文档与 `check_e2e_contracts` 面）；② 只保留日志通道（现状），靠 `ops.md` §4 告诉脚本化运维去哪看；③ 让 `validate` 的错误也阻止写入（对既有配置是破坏性变更） | ✅ **已落地（第一百零九轮，走①）**：`ValidationIssue` 加 `subject`，5 条 limit-role 告警带主体，`POST/PUT /api/v1/limit-roles` 的响应带**恒存在**的 `warnings` 数组（`admin_api.rs::limit_role_write_returns_the_warnings_for_that_role` 钉住非空与空两态）。详见 §2gk |
| D-6 | 租户面能否区分"与 operator 全局前缀重叠"（第十二轮复审 P2） | 折叠成通用 400 / 保留现状并写进契约 / 只在租户命名空间内给具体原因 | 现状是可被租户当布隆过滤器用来**枚举 operator 的全局前缀命名空间**（`sub_tenant.rs:357-363` → `400 key_prefix_overlap`）；改法都动租户可见语义。 **★已决（2026-10-09，收敛：接受现状，标注非缺陷）**：泄漏的只是"operator 是否有某前缀"这一个布尔位，而 operator 前缀本是运维自管公开配置，不是秘密；折成通用 400 会牺牲"我自己配的前缀冲突"这一常用诊断。该决策项关闭。 |
| D-7 | `db` 是否拥有副本 restore 路径（第十六轮） | 把 `cluster::{content,snapshot}` 从 `proxy` 门里挪出来 / 承认 W2/W3 裸切片不再支持 | 决定批次 5 第 1 项是"真的把切片修好"还是"注释就是最终结论"。 **★已决（2026-10-09，收敛：承认切片失效，文档撤回 standalone 声称）**：真修需把 `cluster::{content,snapshot}` 从 `proxy` 门挪出并重排 feature 依赖，纯工程量、无用户可见收益（无人用裸 `db` 切片）；`Cargo.toml`/`HANDOFF` 已改为"`--features db` 需配合 `server`"。该决策项关闭。 |
| D-8 | **官方拓扑下 edge 的 `/metrics` 怎么抓**（第十七轮 P1，我第 7 轮的回归） | 给 edge 注入 admin token / 恢复免鉴权但把 edge 管理口绑私有地址 / 新增仅抓取用的 token | 事实已钉死：edge 没有 token ⇒ `check_auth` fail-closed ⇒ `/metrics` 恒 401，而 `cluster.md:17` 与 `ops.md` §9.1 都假定它可用。三条路都是安全取舍（数据面节点是否该持有那把"能改全部 provider key"的凭据）。**第五十八轮补现场证据**：本机 dev 集群实测 —— 8081/8082（leader/standby）`/metrics` 401、8084（edge，用免鉴权的 `/healthz` 认出）`/metrics` **200 无 token**，但该容器是**早于 9-16 改造的旧镜像**（指标集合只有 4 个 family，缺 `hydra_registry_nodes` 等）⇒ 现行代码不受影响；且**本地 compose 给三个服务都发了 token**（`env_file`），所以"抓不到 edge 指标"只发生在**官方编排**（`docker-compose.cluster.yml` 的 edge 无 token、`jiqun-deploy.md` 的 k8s 同理）。三条路都仍待选。 ★**已关闭（2026-10-08）：前提随角色退役消失** —— `environment/docker-compose.cluster.yml` 现在没有任何 `hydra-edge` 服务（三成员 `hydra-a/b/c` 同构，文件自述 "There is no leader/edge split"，每个成员的 healthcheck 都带 `HYDRA_ADMIN_TOKEN` ⇒ **都拿到 admin token**）；代码侧 `admin/mod.rs:679-681` 明写 edge 的免 token 探针块**已删**，`/metrics` 与其他路由走**同一条门禁**（`admin/mod.rs:476`）。⇒ 无需在三条安全取舍里选；`cluster.md:258` 的口径（"`/metrics` 走同一条门禁"）**实测已正确**，无需订正。 |
| D-9 | L2 缓存要不要抽象成 trait（第二十轮） | 把 `AuthCache::l2` 从具体 `Arc<RedisAuthL2>` 改成 trait 对象 / 保持具体类型并接受"撤销在途竞态只能靠真 Redis 时序验证" | 直接决定批次 6 第 1 条（L2 回填绕过 epoch 守卫，安全相关）能否有**确定性**回归测试；改抽象会影响 `redis/auth_cache.rs` 与若干测试的构造方式。 ★**已落地（2026-10-09，P3-3）**：`AuthCache::l2` 改为 `Option<Arc<dyn L2>>`（`redis/auth_cache.rs` 定义 `trait L2`，`RedisAuthL2` 实现），并用假 L2（`GatedL2`，`get` 内阻塞可注入时序）加了**两个确定性单测**——`l2_backfill_refuses_when_invalidated_while_the_read_is_in_flight`（N1：读飞行中失效 ⇒ L2 回填被拒、L1 保持空）与对照组 `l2_backfill_succeeds_when_no_invalidation_lands_during_the_read`；真实 Redis 的 L2 测试全部保留（dev-plan 铁律 2） |
| D-15 | `limit_role.matching_key` **比的是什么形式**：`design.md:989` 写"NULL **或等于**客户端 api-key"（原始 key），而 `proxy.rs:838`（原引用 `:834` 已陈旧） 把上下文建成 `MatchCtx { api_key: Some(&mask_key(&api_key)) }`（**掩码**形式）（第一百一十五轮实测）—— **① 已闭环（第一百一十六轮）**：匹配改为认**两种形式**（`MatchCtx::api_key_raw` + `key_dim_matches`），bucket 仍用掩码，实测原始形式 `[200,200,429,429]` | **余下②**：计数桶是否也按**原始** key 计 —— 若改，必须先把 bucket 改成**哈希**（集群模式 `hydra:{rl:role:bucket}:count` 会进 Redis keyspace，客户 key 不能明文进去；也就不能进指标标签）。不动的话：**掩码只保留首/尾字符 ⇒ 两个不同的客户 key 掩码相同就共用一个配额**（实测：`sk-fidelity-limited` 与 `skzzzzzzzzzzzzzzzed` 掩码相同，前者窗口耗尽后后者第一次请求即 429）—— 运维侧绕法（今天可行）：**一 key 一角色**，且角色的 `matching_key` 写该 key | **实测（第一百一十五轮）**：修复前 `matching_key` 写**原始** key 的角色**永不触发**（4 次请求全 200，leader/edge 都一样）；写该 key 的**掩码**才生效。**第一百一十六轮**：`MatchCtx.api_key_raw` + `key_dim_matches` 后原始形式也生效（drill 从 `[200,200,200,200]` 变 `[200,200,429,429]`），掩码形式不变、碰撞后果仍在；反向证伪（改回只认掩码）⇒ 单测 FAILED + 该腿 FAIL 而其余全绿 |

> **补充（第一百三十九轮，P3-1 并入本项）**：集群模式下 bucket 直接进 Redis key 名（`hydra:{rl:<role>:<bucket>}:count|tokens`），而 bucket 就是**掩码**（长 key 保留首 10 尾 4）⇒ Redis keyspace/慢日志/监控里能看到凭据的片段。把它改成**哈希**是自然的收敛方向，但**不能单方面做**：`count_key`/`tokens_key` 的**名字**变了，滚动升级期间旧节点算"掩码"、新节点算"哈希" ⇒ 同一个客户端的请求被**记进两个窗口**，限流在那段时间里近似**翻倍**（fail-open 方向），且旧窗口成为孤儿（只能等过期）。⇒ 需要**迁移方案**（双写/双读一段时间，或整批停写窗口）才动手；因此 P3-1 并入 D-15 一并决策。| D-11 | `matching_provider` 这个"永远不匹配"的限流维度怎么收（第三十二轮） | 真正实现（候选已知后逐候选检查）/ 管理面写入口拒绝该字段 / 只保留 Warn（现状） | 现状只会 Warn（本轮加）；实现它要动前置门禁的顺序，拒绝它会让既有配置的写入从 201 变 400 —— 两条都动对外行为 |
| D-10 | 管理面 `POST /api/v1/sub-tenants` 同名重复该怎样（第二十四轮） | 收敛（幂等，返回既有行）/ 保持 409 | 现状是 409（函数文档曾承诺无条件收敛，**已改成实话**）。**第二百一十轮补正一处此前记错的判断**：`db::upsert_sub_tenant_by_name` 的 `ON CONFLICT DO UPDATE` 在这条路径上**不是死代码** —— 顺序重复会被校验器拦成 409，但**并发**创建（两者都基于同一份插入前快照校验通过）时，那个分支正是"输家不因自己看不见的约束而失败"的保险；已把这一点写进 `create_sub_tenant` 的文档。 **★已决（2026-10-09，收敛：保持 409）**：管理面 POST=创建操作，重名是明确错误，409 正确；租户面 PUT=upsert 幂等本就不同——两条入口不一致是语义正确而非缺陷，`ON CONFLICT` 兜并发。该决策项关闭。 |
| D-12 | `dev-docs/changelog-2026-09-16.md` 引用了**本仓库不存在**的提交（第四十五轮，读该文档以核对"显示明文开关是否已删"时发现） | 把三条 SHA 改成现有提交（若确实对应某个改动）/ 标注"来自生产仓库历史，本仓库无法核对" / 保留现状并接受这些历史断言不可验证 | 实测 `git cat-file -t`：`8d6b97c`、`bccccb2`、`40f5912` 全部 `Not a valid object name`，而同文档引用的 `c3eaa6f`、`d508daa` 存在 ⇒ 这是**文档与仓库历史对不上**，不是"提交过但被 rebase"。它的实际危害已经出现过一次：`changelog` 说那个开关"已删"，而当前树里它还在（且是空操作）。✅ **已落地（第二百一十轮，走②"标注，不改写"，2026-10-09）**：文档头部加免责声明并给出**逐个数**的实测（本文引用的 29 个 7 位 SHA 中 **27 个** `Not a valid object name`，只有 `c3eaa6f`/`d508daa` 存在 ⇒ 这份归档写于**另一段历史**）；**原文一律不改**（把 SHA 换成"看起来像"的提交等于编造对应关系）。同时注明"显示明文开关已删"与现状的差异（`?reveal=1` 残骸仍在，但**行为上**确实永不回明文）。若你确认这些 SHA 来自生产仓库，可再决定是否补一份对照表 |
| D-13 | 生产代码禁 `expect`：规则与现状哪边改（第十一轮起） | ① 允许，但每处剩余站点必须**逐条登记理由** / ② 把设计文档那句"必须为空"改成真实政策 | ✅ **已落地（第一百零九轮，走① + ②合体）**：守卫学会"声明处带 `#[cfg(test)]` 的模块不算生产代码"（并去父模块**验证**那条属性），TDengine 5 处接 `lock_gate`，其余 2 处用 `EXPECT_SITE_OK`（键=文件@行指纹）逐条登记理由 ⇒ 生产 **17 → 2**；设计文档那句"必须为空"改成真实政策。详见 §2gk |
| D-14 | 准入上限**热重载不 resize**（第七十七轮） | ① 实现 resize（`tokio::sync::Semaphore` 只能加不能减 ⇒ 需自写计数内核 + 与 in-flight/gauge 的一致性设计） / ② 维持"改完重启"，靠已有可见性兜住 | ★**已落地（2026-10-09，P3-2，选① 真修）**：`ProviderGate` 改为**版本化闸门**——`ArcSwap<GateGeneration>`（不可变的一代 = semaphore + 三个上限），配置变化在下一个 `acquire` **换代**、在途 permit 持旧代 `Arc` 自然排空，新请求立即用新上限；混用永远不可能（同一请求全从一代取）。指标改名 `hydra_admission_resizes_total{provider}`（原 `hydra_admission_limits_stale_total` 的语义随 resize 落地被替代；`limits_stale` 字段保留，语义收窄为"配置已改但尚未被请求应用 / provider 已从配置删除"）。`integration/test_admission_queue.py` Q4 从"钉住不 resize"改写为"断言 resize 生效 + 生效前窗口可见 + 换代计数 + `limits_stale` 翻转"（drill 日志级别提到 `info` 以观察换代 INFO）。详见 §2bj |

## 3. 批次 2（文档 ↔ 事实对齐）—— 10 项**已完成**（第三轮，见下），余项见 §3b

1. `admin_api` 2 例改回"由 `c3eaa6f` 引入的回归、待修"（4 处：`2026-09-18-sub-tenant.md`、
   `-v2.md`、`INDEX.md` 两处），并**决定**语义：是给测试注入 fleet 视图断言 200，还是承认 202 并同步
   更新断言 + `admin-ui/api-docs.js` + `tenant-api-integration.md` + 运维告警。
2. ✅ v3 状态行与 T3.3 集成测试：**均已处理** —— 状态行改为如实描述，缺失的 `terminate_mode.rs` 归因测试已补齐（`usage_is_attributed_to_the_sub_tenant_that_owns_the_key_prefix`，含反向证伪）。
3. README / `HANDOFF.md` 的"production `src/` 零 unwrap/panic" → 实际是**6 处生产 `expect(`**
   （`main.rs` 的 `cert_store`、`admin/mod.rs`、`cluster/control_client.rs` ×4）。改成可核口径。
4. `HANDOFF.md` 的 v2 状态（"未提交、V8 gate pending"）与 `INDEX.md`（已提交 + 复审 PASS）矛盾；
   v3 计划把 v1+v2 的提交区间错误地写成只有 v2 的 `9adbea1..d799756`（v1 是 `da07610~1..1007dea`）。
5. `cluster.md:71` / `design.md:1676` 的 `HYDRA_RATE_LIMIT_FAIL_MODE` "可配 closed" —— 代码零命中。
6. `INDEX.md` 的"17 个错误码" → 实际 35 个。
7. `#[ignore]` 计数（文档记 2、实际 3）且 CI 从不跑 `--ignored`；其中
   `boot_listeners.rs::same_port_on_different_addresses_is_not_detected_statically` 想"显式记录"
   的限制在 `dev-docs/`/`README` 零命中 —— 写进 `ops.md` 或删掉该测试。
8. `tenant-api-integration.md` 的管理员面 `DELETE /api/v1/auth/cache` 缺 200/202/503 三态说明
   （`nodes_total=0` 时确定性 202，运维会误判成失败）；`admin-ui/api-docs.js` 同。
9. 计划里"剩余 `AppState` 构造点 = 2"已过期（实际 3：`sub_tenant_data_plane_write.rs` 手搓了一个）；
   要么改回 `for_tests()`，要么更新计数。
10. 从未在 CI 编译的 `tls-openssl` 特性；无 `sqlx prepare --check`；`.dockerignore` 未排除 `secure/`；
    被跟踪的运行期残留（`hydra.pid`、`e2e_test.db-*`、`environment/__pycache__/*.pyc`）。

11. **`dev-docs/tenant-api-integration.md` §5.2 与代码有 5 处分歧**（第三轮修 SDK 三态时逐条核对发现，均以**代码**为准）：
    - `:202` 写"`lagging` 仅当 `state=pending` **且确实检查过节点**时非空"，但 `events.rs:212-223` 的 `wait=none` 分支把
      `lagging` 填成**全部存活节点**（服务端自己的集群测试 `tenant_api_cluster.rs:294-320` 就是**故意**这么断言的：
      "nobody was checked, so nobody confirmed"）→ **该行文字是缺陷**，不是代码。
    - `:202` 还排除了 `unavailable`，但 `events.rs:252-263` 的 `unavailable` 也带 `lagging: live_nodes`
      （只有 publish 失败那一种 `unavailable` 是空，`events.rs:200-209`）。
    - `:167` 写 `wait=none` ⇒ "立刻返回 202"，但**无 `cluster-redis` 的构建**根本没有 fleet，
      `handlers.rs:356-368`（`:367`）无论 `wait`/`timeout_ms` 都返回 **200 `single_node`**。
    - §5.2 把两个参数写成同等"校验、不忽略"，实际只对 `timeout_ms` 做 `trim()`（`handlers.rs:241`），
      `wait` 是逐字节比较（`:225-238`），所以 `?timeout_ms=%202000%20` 被接受而 `?wait=none%20` 是 400。
    - `event_id` 在 `single_node` 与 publish 失败的 `unavailable` 下为 `null`（`events.rs:159-168`、`:200-209`），
      `waited_ms` 在 `single_node` 路径硬编码 0（`FleetReport::single_node()` 从不调 `.measured()`，`:148-151`）——
      两者都可空/常量，文档未记。
    修：按代码订正文档措辞（`lagging` 一律表述为"未确认"而非"已证实落后"），并补上参数解析的不对称与可空性说明。

---

## 3b. 第三轮新发现（含对本计划自身数字的更正）

> **2026-09-29 第十轮收口**：下面第 4、5、6、7、11 条所记的文档缺口**已逐条回查关闭**，证据见 §1j.H —— 它们从本轮起不再是"余项"。

1. **本机已跑着真实依赖栈，验证面因此扩大**：`hydra-local-redis-test`（127.0.0.1:6380，64 库）、`hydra-local-clickhouse`（127.0.0.1:8123）、`hydra-a/b/c`。第三轮据此跑了此前无法运行的套件：`cluster-redis` lib **220 passed / 0 failed**；`server,cluster-redis,usage-clickhouse` 全套 **623 passed / 2 failed / 3 ignored**（2 个失败见下条）。**这也意味着批次 1 中依赖 Redis 的项（P1-8 `XTRIM MINID`、P1-16 失效链路指标）下一轮可以真实验证**，不必再延期。
2. **`admin_api` 那 2 例失败已由维护者独立复现**（真实 Redis 在位）：`empty_body_delete_invalidates_all_local`（`admin_api.rs:1626`）与 `too_many_invalidation_keys_are_refused_and_publish_nothing`（`:2965`）**确定性失败**，症状即"期望 200、实得 202"。至此 OG-5 的静态推断升级为实测：这是 `c3eaa6f` 引入的回归，不是"环境/既有基线"。**语义决策仍未定**（见 §2 第 10 条与下面第 5 条的建议）。
3. **该套件在 `optional-features` 配置下对机器负载敏感（测试基建脆弱，非产品缺陷）**：44 个用例同时启动代理 + wiremock 鉴权跳，突发期内鉴权请求超时 → Hydra **正确地** fail-closed 返回 **503** → 那些"断言首个响应就是 200"的用例随机失败（实测报错：`a_wedged_stream_is_cut_by_the_idle_bound ... expected 200 from the proxy, saw Some(503)`；同轮还带倒 `every_header_credential_transport_authenticates`、`error_402_when_auth_denied_insufficient_balance` 等既有用例）。证据链：把新用例的"取 200"重试预算提到 ~10 s 后连跑 4 次全绿；**跳过我的 3 个新用例连跑 5 次也全绿**，说明既有用例同样脆弱、只是原始突发没那么大。建议：给该套件加一个"鉴权跳就绪/退避"策略，或把数据面用例拆到独立二进制以降低突发。**未做**（属测试基建改造，需单独一批）。
4. **`dev-docs/jiqun-deploy.md:111` 仍称 `HYDRA_BREAKER_QUORUM` "暂未提供 env 覆盖"，但 `main.rs:65` 确实读它** → 又一处 doc↔code 缺陷（本轮文档负责人已把真话写进 `cluster.md`/`ops.md`，但未改该表项）。
5. **`tenant-api-integration.md` §6 缺 `forward_failed`**（代码确实会发，与已列的 `forward_result_unknown` 不对称）→ 文档缺口。
6. **本计划自身的两个数字已更正**：错误码不是 35 而是 **32 个不同 code / 26 行**（由本轮文档核对逐条计数并回查代码）；`#[ignore]` 是 **3** 条（计划里曾记 2）。
7. **`2026-09-18-sub-tenant-v3.md:3` 状态行仍写 `已实现（T3.1–T3.6…）`**，与本计划"T3.3 集成测试待补"冲突 → 待改（并补 T3.3 的集成测试）。
8. **对 §2 第 10 条（admin_api 语义）的建议**（供决策，未实施）：`tenant_api_cluster.rs::barrier_reports_pending_not_applied_when_the_live_set_is_empty` 已把"空 fleet 视图必须回 `pending`，绝不可回 `applied`"固定为**有意语义**；那两例 admin_api 测试的 `AdminState` 从未注入 fleet 视图，属人工构造。因此推荐：**给这两例注入单节点 fleet 视图（走真实路径，期望 200），空 fleet 的 202 由既有集群测试覆盖**，并同步 `admin-ui/api-docs.js` 的样例。另一种选择（承认 202 并改断言）也不算错，但会削弱"管理面能确认清理完成"这一信号。

---

## 4. 复现命令（批次 0/1 之后的门禁）

**下面每一条都已经在本机逐字跑过**（第十五~十七轮），并且与 `.github/workflows/ci.yml` 的作业对应；括号里写的是对应的 CI 作业名。第 12 条起是本轮新并入门禁的。

**★「七个作业」这句话本身有两个坑（实测 2026-10-01）**：①工作树里 `ci.yml` 有 **8** 个作业（`check` / `optional-features` / `alt-features` / `live-deps` / `sdks` / `integration` / `scripts` / `ui-e2e`），这里原先写「七个」，已过期；②**`HEAD` 里的 `ci.yml` 只有 4 个**（`check` / `optional-features` / `scripts` / `ui-e2e`）——`alt-features`、`live-deps`、`sdks`、`integration` 四个作业**只存在于未提交的工作树**（`git status --short .github/workflows/ci.yml` = `M`）。因此本文件里所有「CI 会跑 X」的说法，**只在工作树这份文件被提交之后**才成立；`check_ci_wiring` 解析的也是工作树（守卫管的是「仓库里的接线是否正确」，管不了「提交与否」）。这是**未提交状态**（用户未要求提交）的直接代价：**守卫绿了不等于 CI 真的会跑**。

```bash
cd /home/alex/Projects/hydra
export SQLX_OFFLINE=true
export HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380   # hydra-local-redis-test（64 库）
export CH_URL=http://127.0.0.1:8123                  # hydra-local-clickhouse 24.3

cargo fmt --check                                                                       # check
cargo clippy --workspace --all-targets --features hydra-server/server -- -D warnings     # check
cargo clippy --workspace --all-targets \
  --features hydra-server/server,hydra-server/cluster-redis,hydra-server/usage-clickhouse -- -D warnings   # optional-features
cargo test -p hydra-core                                                                 # check
cargo test -p hydra-server --features server                                             # check
cargo test -p hydra-server --features server,cluster-redis,usage-clickhouse --no-fail-fast   # optional-features
                                                                                         # ^ `--no-fail-fast` 不是可选项：否则首个失败二进制之后的测试全部被静默跳过
cargo clippy --workspace --all-targets --features hydra-server/tls-openssl -- -D warnings    # alt-features
cargo build  --workspace --features hydra-server/tls-openssl                                 # alt-features（只 clippy 不链接 = 从未链接过）
cargo clippy --workspace --all-targets --features hydra-server/proxy -- -D warnings          # alt-features（无 TLS 后端）
cargo check  -p hydra-server --features hydra-server/proxy --bin hydra                       # alt-features（第 21 轮新增：`--bin` 在特性未满足时会报错，所以这条是"文档承诺的独立构建"的正向证明；`--all-targets` 在 required-features 未满足时只会静默跳过）
cargo test -p hydra-server --features server,cluster-redis,usage-clickhouse \
  --test boot_listeners -- --ignored                                                         # optional-features（无需服务）
cargo test -p hydra-server --features server,cluster-redis,usage-clickhouse \
  --test clickhouse_sink --test usage_query -- --ignored                                      # live-deps（需 $CH_URL）
# live-deps 还会先验证官方路径：把仓库里的 init.sql 真正执行到全新实例上，并断言表设置存在
curl -sS --fail --data-binary @environment/clickhouse/init.sql "$CH_URL/"
curl -sS --data-binary "SHOW CREATE TABLE usage_record" "$CH_URL/" | grep -o 'non_replicated_deduplication_window[^,]*'
# 交付物/脚本类
for f in environment/docker-compose.yml environment/docker-compose.cluster.yml environment/docker-compose.local.yml; do
  HYDRA_ADMIN_TOKEN=dummy-admin-token HYDRA_CLUSTER_TOKEN=dummy-cluster-token \
  HYDRA_ENCRYPTION_KEY=ZHVtbXkta2V5LWZvci12YWxpZGF0aW9uLW9ubHkAMDE= docker compose -f "$f" config -q   # scripts
done
(cd tools/hydra-go && go test ./...)                       # sdks
#   ↳ 本机默认的 GOCACHE（~/.cache/go-build）在本沙箱里是只读的，会得到
#     "[setup failed]"；这不是仓库问题。可写缓存下实测 `ok … 0.019s`：
#     (cd tools/hydra-go && GOCACHE=/tmp/gocache go test ./...)
(cd tools/hydra-ts && npm install && npm test)             # sdks（该包没有 lockfile ⇒ 不能用 npm ci；实测 28 pass）
(cd tools/hydra-py && python3 -m unittest discover -s tests -v)   # sdks（实测 29 tests OK）
node scripts/check_i18n.js                                 # scripts
node --test scripts/check_i18n.test.cjs                    # scripts
node --test scripts/admin_ui_render.test.cjs               # scripts（不需要浏览器；21 条断言含 2 条反向证伪）
node scripts/check_e2e_contracts.cjs                       # scripts（e2e 选择器/导航键 ↔ admin-ui 源码）
node --test scripts/check_e2e_contracts.test.cjs           # scripts（checker 自己的 5 条证伪）
node scripts/check_ci_wiring.cjs                           # scripts（每个测试/检查器都必须有 runner）
node --test scripts/check_ci_wiring.test.cjs               # scripts（wiring checker 自己的 7 条证伪，含注释误报与 testDir 越界）
# 公开断言 ←→ 事实（第 62 轮新增；CI 的 check 作业用两条 `cargo test | tee` 的 transcript 当"测量值"）
cargo test -p hydra-core 2>&1 | tee /tmp/core.log; cargo test -p hydra-server --features server 2>&1 | tee /tmp/server.log
node scripts/check_public_claims.cjs --core-log=/tmp/core.log --server-log=/tmp/server.log   # 数字/日期不符 ⇒ exit 1；缺日志/套件没过 ⇒ exit 2
node scripts/check_public_claims.cjs --measure --write      # 自测 + 把数字与当天日期写回 docs/index.html
node --test scripts/check_public_claims.test.cjs            # 18 条断言（含对仓库里那份 index.html 直接校验）
node scripts/check_source_purity.cjs                        # scripts（生产代码 0 unsafe/unwrap/panic + 每个 crate 根带 forbid(unsafe_code)）
node --test scripts/check_source_purity.test.cjs            # 20 条断言（含 cfg(all(test,…)) 不算违规、字符串里的 // 不得吞行）
integration/run-crud-local.sh                              # integration（一次性实例 + 116 条 admin REST 断言）
env npm_config_cache="$PWD/.acceptance/tmp-npm-cache" bash -c "cd tools/hydra-cli && npm test"   # sdks 作业的 CLI 部分
MOCK_LLM_PORT=19190 MOCK_AUTH_PORT=19191 HYDRA_PROXY_PORT=18092 HYDRA_ADMIN_PORT=18093 \
  python3 integration/e2e_proxy_test.py                     # integration（自起 mock auth/llm + 代理全链路）
scripts/e2e-local.sh                                      # 一次性实例 + 真跑 Playwright 套件（**实测 2026-10-01：20 passed / 30.2s**；不需下载浏览器）
#   ↳ 上面那个「18 passed / ~26s」是过期数字：套件涨了 2 条而文档没跟。**它现在是门禁条目**
#     `admin-ui e2e (browser, 18180)`（`E2E_SKIP_BUILD=1` + 显式端口，理由见 §2ez）。
SELFTEST_ONLY=1 bash scripts/load_test.sh                 # 负载工具的**确定性一半**：SWRR 分布判定的 4 组已知输入自检（无需实例/无需 token，实测 exit 0）
#   ↳ `scripts/load_test.sh` 的**测量一半**（RPS/P99/分布采样）仍需活实例 + 并发 echo 上游，
#     因此**不进任何自动化**：这条自检是能被自动化的那部分，也已接进门禁与 CI `scripts` 作业。
bash scripts/ask_llm.test.sh                               # scripts（实测 "E3 (ask_llm + dead-code): ALL PASSED"）
# 性能数字（第 63 轮）：发布页那四个数字怎么复现 / 怎么再测
#   .acceptance/round63/measure.sh  （一次性实例 + Go 并发 echo 上游 + Go 压测器，支持 BIN=/SWEEP=；scratch，见 D-5）
BIN=./target/release/hydra DUR=8s SWEEP="1 4 25 64" bash .acceptance/round63/measure.sh
#   ↳ 必须用 release（debug 会低一个数量级）且上游必须并发（仓库自带 mock 是单线程，见 §2au.B）
python3 .acceptance/findings-disposition.py
bash .acceptance/phase-b-gate.sh          # 全量门禁（需要 6380 上的真实 Redis；浏览器 leg 需要 Playwright 浏览器）
CH_URL=http://127.0.0.1:8123 bash .acceptance/t3-baseline.sh   # 活 CH 门禁（含 --ignored 用例）
```

需要真实 Redis/CH 时的可选特性全套（本机：`hydra-local-redis-test` 在 6380、`hydra-local-clickhouse` 在 8123）：

```bash
HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380 CH_URL=http://127.0.0.1:8123 \
  cargo test -p hydra-server --features server,cluster-redis,usage-clickhouse
```

**推荐直接用** `.acceptance/round10-gate.sh`：它把上面绝大多数命令**逐条独立执行并显式打印各自的退出码**（末尾给 GATE SUMMARY），并**返回**判定（`exit "$overall"`）。

**「绝大多数」是多少（实测 2026-10-01，方法见下）**：上面这个代码块可解析出 **44 条命令行**，其中 **35 条被门禁条目调用**、**33 条被 `ci.yml` 调用**（两者有重叠），只剩 **3 条**不在任何自动入口里：
① `scripts/e2e-local.sh` —— **本轮修好**（已接进门禁；CI 侧由 `ui-e2e` 作业覆盖同一套浏览器测试，故不重复接）；
② `.acceptance/round63/measure.sh`（性能数字复现）与 ③ `.acceptance/phase-b-gate.sh` —— 这两条**接不进去**，因为 `.acceptance/` **不在版本控制里**（**D-5**）：一个只存在于本机草稿目录的脚本没法成为别人能跑的门禁条目。这是 D-5 的具体代价，不是遗漏。
测量方法：`python3 .acceptance/round168/sec4-coverage.py`（把代码块的续行拼起来、按"产物"而非整行匹配，因为同一个产物在门禁与 CI 里带的参数不同）。**头两版测量都说了谎**，记在这里以免后人重犯：第一版把行尾 `# 说明` 当成命令的一部分，于是 `node scripts/check_i18n.js` 被判成"哪里都没执行"（门禁里逐字就有这一行）；第二版按整行匹配，又把 `check_public_claims --core-log=/tmp/core.log` 这类**参数不同、产物相同**的判成缺失。它现在有 **88 条条目** —— **数字随文件增长，请用 `grep -c '^gate ' .acceptance/round10-gate.sh` 取当前值**（原先这里写的是「13 条」，那是第十轮 9 条 + 第十五轮并入的 proxy-only clippy / ignored-listener / compose config + 当轮的 CH e2e；第一百五十五轮复审指出该数字已过期约 6.8 倍，会把读者引向「只要跑十来条」的错误印象）。
注意：**它每次会截断自己的日志**（`.acceptance/round10-gate.log`），所以"上一轮的原始日志"不可复现——要看历史数字就得在跑之前另存一份。
**该脚本本身不在版本控制里**（`/.acceptance/` 被忽略）——这是 **D-5**，尚未决策。
之所以要专门做这件事：第十轮里 `cargo clippy ... | tail -15` **把 exit 101 报成了 0**，两处 lint 就这样漏过了门禁，而 CI 的 `check` 作业是红的。**任何门禁命令都不要接管道后再看 `$?`。**

## 2gj. 第二百零八轮：清 P2/P3 四条（记录按**身份**而非序号背书；外加两条同族小缺陷）

**触发**：第 207 轮把 2 个 P2 + 2 个 P3 逐条记录在案，并**明确说明当轮不改代码的理由**（改 `.cjs` 会让"当前树被 98/98 覆盖"失效，需重跑约 40 分钟门禁）。本轮预算完整，四条一起清。

**★P2#1（真缺陷，已修）：`check_cluster_env.cjs` 的记录按**序号存在性**背书，不按身份。** `applies(key)` 只问"该文件是否仍有 ≥ ordinal 个非字面站点"，于是**人在某一行写下并签字的理由，可以被同一文件里的任意一行满足**。**自己复现（不采信复审转述）**：夹具里唯一的非字面站点是 `let _ = std::env::var(name);`、其上一行是 `let name = "HYDRA_ANYTHING";`；把上一行换成 `let name = format!("HYDRA_{}", "CLUSTER_PEERS");`（一个**可能藏住 cluster-only 名字**的间接写法——正是这条规则存在的理由），守卫仍打印 `1 non-literal read(s) (all recorded) … OK`、exit 0。

**修法**：键改为**站点自身文本的指纹** `<file>@<sha1-8>`，指纹覆盖 `env::var` 那一行**加其上最多 3 行非空上下文**（参数是在那里被构造的）；文本完全相同的站点用 `#2`/`#3` 区分，保持"一条记录只背书一个站点"（第 194 轮的成果）。消息给出**可直接粘贴的键**（未记录时）与该文件**当前**的键（记录过期时），所以真改动之后重录是机械动作。

**为什么必须含上下文（这一版之前我写错过一次）**：只取 `env::var` 那一行的指纹时，**上面那次替换仍然是绿的**——两版夹具的该行逐字节相同。加上下文后实测三条性质：站点未动 **GREEN**、内容被换 **RED（修前 GREEN）**、上方插入无关代码（窗口之外）**GREEN**（**不 churn**，即当初选序号想换来的性质，被内容身份保留）。**机械证伪**：去掉上下文窗口 ⇒ 新用例**红**，且**真实树用例也红**（树的记录不再匹配任何站点）；恢复后 23/23、真实树 exit 0。**如实标注的限度**：这是"站点 + 近邻上下文"的指纹，不是数据流分析——若改动发生在窗口之外（例如更上方重新绑定同一标识符），键不变。已写进代码注释。

**★P2#2（真缺陷，已修）：`check_source_purity.cjs` 注释里那个会漂移的数字。** 原话是"shared by SEVEN guards（用 `grep -rln "rust_blank.cjs" scripts/*.cjs` 重新测量；本轮之前写的是 three）"。**实测三种问法三个答案**：`grep -rln rust_blank` 命中 12 个文件（含只在注释里提到它的）、`require('./rust_blank.cjs')` 命中 8（含 1 个测试文件）、非测试守卫 **7**。⇒ 同一个数字可以有三种"正确"，所以修法是**只写命令、不写数字**，并把这个教训留在注释里。

**★P3#1（真缺陷，已修）：`check_i18n.js` 的成功行把一个"算了没打印"的数与一个"标签不符"的数并排。** `unscannedRecorded`（第 196 轮就是为这句话算的）从未被打印；被打印的 `unscannedJudged` 却顶着 "and recorded" 的标签，而括号里列的是**记录**的名字。现在**两个数都打印**（`N file(s) not scanned, M recorded (…)`），把那层**隐含**变成明示；它的自测同步钉住新措辞（旧断言立刻变红，这正是"消息被测试钉住"的意义）。**顺带更正我自己**：动手前我在草稿里写过"这两个数在真实树上系统性差 1"——**错的**。字典文件是 `.js`，本就属于被扫描的扩展名，所以 `unscannedJudged` 不含它；门禁通过时两者相等（审计会在未记录/过期时先失败）。真正的缺陷只是"算了没打印 + 标签指着另一个集合"。

**★P3#2（不只是排版，已修）：`crates/hydra-server/src/lib.rs` 的 `pub mod crypto;` 掉出了 `#[cfg(feature = "db")]`。** 第 201 轮插入 `lock_gate` 时把它插在 crypto 的文档注释与自己的条目之间，**顺手带走了那条 attribute** ⇒ `crypto` 在**所有**特性组合下都被编译，而它的依赖（`aes-gcm`/`base64`/`rand`）只由 `db` 拉入。**为什么没暴露**：CI 构建的每种组合都隐含 `db`（`proxy` → `db`、`server` → `db`），所以是**潜在**缺陷——任何 db-less 切片构建会在 `crypto.rs` 内部报缺失依赖，而不是干脆不编译该模块。已把 attribute 还给 `crypto`，`lock_gate`（文档 + 自己的 attribute）紧随其后，并在注释里写明这次改动与"为什么是潜在的"。

**验证**：`check_cluster_env` 自测 **23/23**、真实树 exit 0；`check_source_purity` **30/30**、exit 0；`check_i18n` **1/1**、exit 0；`check_test_tails` 26/26；共享 `rust_blank.cjs` 的其余守卫（documented env / defaults / metrics / tenant error codes / alert expressions / e2e contracts）全 exit 0；**本地门禁 99 条目 `OVERALL=GREEN`**。

## 2gk. 第二百零九轮：用户一次性裁定六项决策（D-5/D-8/D-13/D-15②/D-16③/D-17），本轮全部落地

**裁定的内容**（用户："按照你的建议一次性先全部改完了再来最后一次性验证"）：D-5 走②（.gitignore 目录级白名单）、D-13 走①（含 TDengine 5 处接 `lock_gate`）、D-8 **关闭**、D-17 走①（响应带 `warnings`）、D-15② 与 D-16③ **合并做**（计数桶身份改为摘要、`matching_key` 支持摘要形式）。

**★D-5（门禁脚本进版本控制）**：`.gitignore` 从 `/.acceptance/`（整目录）改成目录级白名单：`/.acceptance/*` + `!/.acceptance/*.sh` + `!/.acceptance/*.py`，并**排除草稿**（`bak-*.py`、`extract_*.py`、`tmp-*.py/sh`、`round*-probe*`）——按扩展名放行会把 12 个死脚本塞进每个克隆，而这条决策的目的（"新克隆必须有门禁、CI 能引用"）由**证据链**满足即可。入库 **15 个**：`round10-gate.sh`（门禁本身）、它调用的 `findings-disposition.py`、以及记录引用过的 `hunt-flake.sh`/`ab-index-order.sh`/`phase-b-gate.sh`/`t3-baseline.sh`/`tenant-api-gate.sh`/`round89-90/173/174/175` 的扫查脚本等。**未做**：嵌套 `roundNN/` 目录仍忽略（历史测量的一次性脚本，价值已在计划里落档）；要搬需要单独决策。

**★D-8（edge `/metrics` 恒 401）—— 由退役关闭，不是选了一条路**：`environment/docker-compose.cluster.yml` 现在**没有** `hydra-edge` 服务（三成员 `hydra-a/b/c` 同构，自述 "There is no leader/edge split"，healthcheck 一律带 `HYDRA_ADMIN_TOKEN`）；代码侧 `admin/mod.rs:679-681` 写明 edge 的免 token 探针块**已删**，`/metrics` 与其它路由走同一条门禁（`:476`）。`cluster.md:258` 的口径实测已正确，无需订正。**这条以后再被提起时，先读这一行**：三条安全取舍都不需要选。

**★D-13（`expect` 禁不禁）—— 走①，且先把数字测准**：守卫原先打印的"17 处生产 `.expect(...)`"**不是它说的东西**——其中 10 处在 `usage/testing.rs`，而该模块的声明处就是 `#[cfg(test)]`（`usage/mod.rs:332`），只被测试用。落地三件：
1. **TDengine 后端 5 处**互斥锁 `lock().expect(...)` 接上 `lock_gate`（第 201 轮同一先例：一个无关线程 panic 不该让之后每次用量写入都 panic）；`lock_gate` 的特性门随之放宽为 `any(feature = "db", feature = "usage-tdengine")`（后者不隐含任何数据库特性，只门 `db` 会让那个组合编译不过）。实测 `cargo check --features server,usage-tdengine` exit 0。
2. **守卫学会"声明处带 `#[cfg(test)]` 的模块不是生产代码"**，而且**不是照单信任**：它去父模块里**找到** `mod <stem>;` 并**验证**其上方的属性块确实含 `cfg(...test...)`；模块一旦丢掉门，下一次运行就会重新算作生产代码。回落 10 处，并在 OK 行**点名**哪些文件被排除、依据是哪一行。
3. **其余生产站点逐条登记**（`EXPECT_SITE_OK`，键 = `文件@该行 sha1-8`，未登记即判红、登记失效也判红），两条理由都**读了代码才写**：`main.rs:1166` 的 cert store 在 TLS 特性下**无条件构建**（`main.rs:510-517`）且该分支只在配了 TLS 监听时到达、且这是 bootstrap 没有可返回错误的上游；`provider_client.rs:131` 的 reqwest client 用**常量设置**构建、失败会**用同一个 builder 重试一次**，能活过重试的失败是常量集合的属性而非请求数据的属性。
4. **我这次自己造过一个"不会失败"的检查并当场抓到**：新登记一开始只把发现推进 `problems`，而 OK 分支只判 `violations` ⇒ 守卫在"两个真实键都还没登记"的情况下打印 `every one REGISTERED` 并 exit 0。改为 `problems.length === 0` 是承重的一行，注释里写明它为什么在。**净结果**：生产站点 **17 → 2**（且 2 条都登记在案），设计文档里那句"必须为空"改成真实政策（`design-tenant-api.md`）。
5. 自测：**31 条**（含"未登记必红 / 登记后通过 / 登记失效是 stale"与"声明处 `#[cfg(test)]` 被排除、去掉门就重新算生产"两向）；`PURITY_EXPECT_OK` 在夹具里默认替换为 `{}`（否则每个夹具都会把仓库的两条记录报成 stale——`recorded_exceptions.cjs` 明文记过这个坑）。

**★D-17（写入响应回带该角色的告警）**：`ValidationIssue` 增加 `subject: Option<String>`（"这条告警说的是哪个实体"），limit-role 的 **5 条**告警改用 `warn_about(role.id, …)`；`POST/PUT /api/v1/limit-roles` 的成功响应现在带 `warnings` 数组——**恒存在、可为空**——内容是 `validate(snapshot)` 里 subject 等于该角色的那几条。整体性告警（无 subject）仍只进日志。**测试**：`admin_api.rs::limit_role_write_returns_the_warnings_for_that_role` 断言 inert 维度（`matching_provider`）的告警**在响应体里且点名该角色**，并断言干净角色的 `"warnings":[]`。`ops.md` §4 的"不在响应体里"改写为现状。

**★D-15② + D-16③（合并做：计数桶身份 + 摘要匹配形式）**：
* `bucket_key` 的 KEY 维度从**掩码**改为**原始 key 的摘要**（`key_bucket_id` = sha256 前 32 hex）。这同时修掉两件实测过的事：**泄漏**（集群模式下这段字符串会进 Redis 键名、派生标签）与**碰撞**（掩码只保留前 10/后 4 ⇒ `sk-fidelity-limited` 与 `skzzzzzzzzzzzzzzzed` 同掩码，前者窗口打满后**后者首个请求即 429**——一个客户吃另一个的配额，无日志无指标）。
* `matching_key` 新增**摘要形式** `sha256:<64 小写 hex>`（`key_digest`/`is_key_digest`），匹配时对 presented key 求同摘要比较；原始与掩码两种历史形式不动（三条腿都在测试里）。
* **`config::validate` 里那条"掩码不是唯一身份 ⇒ 共享预算"的告警被删除**——它的前提被这条修复消灭了，而**不会触发的告警是训练运维跳过清单的噪音**；`hydra-core/tests/validate.rs` 反过来**钉住它的缺席**，防止有人照旧笔记加回来。
* **测试当场抓到我一个真错**：新的"摘要形式"用例第一次运行就红——`matching_key` 的 raw-判定是 `mask_key(k) != k`，于是**摘要形式被当成 RAW key 告警**（"写摘要"的告警文本去警告那个摘要值本身）。判据补上 `&& !is_key_digest(key)` 后才对。这正是"先写断言再信自己"的价值。
* 文档：`ops.md` §4 两处（窗口身份 + 推荐形式与如何算摘要：`printf %s "$KEY" | sha256sum` 加 `sha256:` 前缀）、`design.md` §10.1/§9.5 的匹配形式与"桶里不再有客户 key 的任何字符、升级会重置一次窗口"。
* **drill**：`integration/test_limit_roles_enforcement.py` 的 L9 原来断言"掩码形式会告警"，改为**断言它不再告警**（附理由），并把"掩码碰撞会共享窗口"的说法改成历史事实 + 已修。
* **未做（如实标注）**：D-16 的**封存该列**（用 `kp.seal` 加密 `limit_role.matching_key`）没有做。③（摘要形式）让运维**可以**不存密钥形态，但**列本身仍是明文**——封存需要配置加载期的"读出来解封"路径 + 保真载荷 + `db/restore.rs` 三处联动，属独立一件事；D-16 因此**保持开**，只是从"要不要给一条不泄漏的路"降级为"要不要连历史遗留的明文行也加密"。

**本轮门禁（实测）**：`.acceptance/round10-gate.sh` **99 条目、98 绿、1 红**，红的是 **`public claims`**——因为我新增了 3 条 Rust 测试（hydra-core 的桶身份与摘要匹配、hydra-server 的写入告警），而公开页的计数还是 774。**这是我的操作失误，且是门禁头部明写过的 pre-flight**（"if you added or removed a Rust test … run this FIRST: `cargo fmt --check && node scripts/check_public_claims.cjs --measure --write`"），我漏跑了。按 pre-flight 修：`--measure --write` 报 **777（262 core + 515 server）**并改写 `docs/index.html`，`--measure` 复核 **exit 0**。**其余 98 条的判定仍覆盖这份代码树（机械证明，不是推断）**：`node scripts/tree_manifest.cjs --check .acceptance/gate-manifest.txt` 报 **`OK (the source tree is unchanged: 379 file(s), docs-only changes: 3)`** —— 相对门禁收据只有 3 个文档（本轮计划、INDEX、`docs/index.html`）变化，**没有代码移动**（这正是第 201/207 轮用过的同一条判据）。CI 待推。

**★D-16 裁定（2026-10-08，用户）：「① 全封存，存量行读时自动封存」** —— 即用 `kp.seal` 加密 `limit_role.matching_key`，并且**读到明文形态的存量行时自动重新封存**（迁移无需人工步骤）。实施未在本轮完成（本轮的预算是六项决策那批），**下一轮第一件事**；实施前的现场事实与落点（都已核对过）：
* **写入点**：`crates/hydra-server/src/admin/handlers.rs` 的 POST/PUT 走 `crate::db::{insert,update}_limit_role(state.db(), &r)` —— 这两个函数**只拿到 `&SqlitePool`**，`key_provider` 不在那一层（`ConfigStore::load(pool, kp)` 才有）⇒ **第一步是让 key provider 到达写读这一层**（改签名 + 全部调用点 + 测试），否则封存只能做在 store 侧、而 admin 写入会绕过它（那正是"两个所有者"的老毛病）。
* **读取点**：`db.rs:1311`（list）、`:1333`（get）、`list_limit_roles_on`（`cluster/content.rs:178` 用它建树）——**读路径同时服务"内存配置（匹配要用明文）"与"跨节点保真载荷"**，所以两者要么都解封后在树里再封一次、要么让树专用一个"取封存形态"的读取；`sealed_provider_keys` 就是树里自己封的先例（`cluster/arachne_entities.rs`），照它做即可。
* **自动封存存量行**：读时若该值**不是**合法封存块（`Sealed` 解析失败）⇒ 按明文使用 + `UPDATE limit_role SET matching_key = <sealed>` 写回。要防的两个坑：①**每次读都尝试写**会把读变成写（用"只在解析失败时写"钉住）；②多节点并发写回同一行是幂等的（同样的明文 ⇒ `seal` 每次随机 nonce ⇒ 值不同但语义相同），日志要能看出发生过一次迁移。
* **restore 路径**：`db/restore.rs:242/248` 落库时**绑定的是树里的值** ⇒ 树若带封存形态，这里直接落库即可（不解封），这也是"全封存"必须让树也带封存形态的原因。
* **回显**：`GET /api/v1/limit-roles` 把整行序列化（`handlers.rs:1532-1534`）⇒ 解封后仍会**明文回显**。用户选的是①（封存），回显策略是**同一件事的第二个决定**：封存只保护"库/备份/replica"，不保护 admin API 的读数；我在实施时会同时给出"回显只显示掩码/摘要"的选项，默认**保持现状**（因为改回显是对外形状变更，需要你点头）。
* **测试计划**：①新写入的行在库里**不是明文**（直接读裸列断言）；②存量明文行可读**且被重新封存**（读一次后裸列变封存块）；③端到端匹配不变（`admin_api` 的 limit_role 用例 + `hydra-core` 的三形态测试）；④restore 落库后成员库同样不存明文。

## 2gl. 第二百一十轮：D-16 落地 —— `limit_role.matching_key` 全封存 + 存量行由加载器自动重封存（并补上"arachne 门控下的 `--lib` 测试无人执行"这个洞）

**裁定**（第一百零九轮，用户原话）：「① 全封存，存量行读时自动封存」。本轮把 §2gk 末尾那份"实施前的现场事实与落点"逐条兑现，并把全仓所有"该列是明文"的公开说法改掉。**本轮有产品代码改动**（`crypto.rs`/`db.rs`/`db/restore.rs`/`store.rs`/`cluster/arachne_entities.rs`/`cluster/content.rs`/`admin/handlers.rs`/`main.rs` + `hydra-core` 的警告措辞与文档注释 + 门禁脚本与 CI 各一行）。

### 2gl-A. 一个设计决定：封存形态是"值自己说自己"（单列文本信封），不是三列

`provider_key` 把一把密钥摊在 `api_key_ciphertext`/`api_key_nonce`/`key_version` **三列**上，"这一行是不是密文"是**表结构**的属性。`limit_role.matching_key` 只有**一列 TEXT**，且从第一个版本起存的就是明文 ⇒ 加三列会让**同一把密钥有两个载体**（还可能两列同时有值），并且把"这行封存了吗"变成一个需要迁移才能回答的问题。所以封存形态做成**值自述**：

* `crypto::Sealed::to_text`：`sealed:v1:<key_version>:<base64(nonce ‖ ciphertext)>`（`SEALED_TEXT_PREFIX` 是格式标签，将来换布局＝换标签，**不是**对既有字节的重新解释）；
* `Sealed::from_text`：**只做结构解析**（前缀、版本、base64、长度 > nonce）——"这值像不像封存块"与"它能不能被打开"是**两个不同的问题**，故意分开；
* `Sealed::open_text`：**唯一**执行"封存/明文"策略的地方 —— 是信封就 `kp.open`（**失败即报错**，绝不把密文当 key 返回）；不是信封就**按明文原样返回**（D-16 之前的行）。
  * ★**这一条的限度是被"证伪"量出来的，不是推出来的**（见 2gl-F 的 ★②）：配置**加载器**在读之前就把列重封存了，所以"把非信封当错误"**不会**让加载器测试变红 —— 真正的承重者是**其它读者**：还没跑过迁移的节点上的 `GET /api/v1/limit-roles`、任何直接调 `db::list_limit_roles` 的代码、以及**旧版本写下的配置树**（滚动升级）。注释里原先写的"否则升级后读不了自己的配置"对**加载器这条路径**是**把话说大了**，已按实测改写。

**为什么同一个文本形态也用在配置树上**：树里那一列改用兄弟字段（`SealedLimitRole{role, sealed}`）会立刻产生"树的风味"这个第二问题，而它只会与数据库那份**漂移**。用同一形态 ⇒ "它封存了吗"在全仓**只有一个所有者**（`Sealed::from_text`），旧树/旧行都按同一条规则读。

### 2gl-B. 三条写路径、三条读路径（表）

| 路径 | 位置 | 动作 |
| --- | --- | --- |
| admin POST | `db::insert_limit_role(pool, kp, r)` | `seal_limit_key`（`kp.seal` + `to_text`） |
| admin PUT | `db::update_limit_role(pool, kp, r)` | 同上（**顺手证伪**：只封 insert 不封 update ⇒ P2 变红） |
| 副本重建 | `db/restore.rs` 的 `INSERT INTO limit_role` | 同上（与 provider api-key 同一先例：**在写边界封**） |
| 配置树（每角色实体 + 保真实体两处） | `cluster/arachne_entities.rs::sealed_limit_role` | `kp.seal_deterministic`（**必须**确定性：树按字节内容寻址，随机 nonce 会让**没变的配置**每次发布换个树名） |
| 读（repo） | `db::open_limit_key` → `Sealed::open_text` | 信封开、明文过、打不开＝错误 |
| 读（树） | `opened_limit_role` | **严格**：非信封＝**按名字报错**（理由见 2gl-G：树的兼容性由 `TOC_FORMAT` 负责，字段级 fallback 是 fail-open 方向） |
| 读（HTTP） | `admin/handlers.rs` 四处调用点补 `state.key_provider.as_ref()` | 回显形态**不变**（仍明文回显 —— 见 2gl-E 的限度） |

签名改动波及全部调用点：`store.rs`、`cluster/content.rs`、`cluster/arachne_materializer.rs`、以及 8 个测试文件（`repo`/`loader`/`metrics`/`anthropic_passthrough`/`streaming_usage_persistence`/`terminate_mode`/`fidelity_disabled_rows`/`arachne_derivation_fidelity`）。

**与上一轮那份落点清单的一处**有意偏离**（记档）**：§2gk 末尾写的是"`restore` 落库时绑定的是树里的值 ⇒ 树若带封存形态，这里直接落库即可（不解封）"。本轮**没有**那样做：`FidelityRows.limit_roles` 在内存里保持**明文**（匹配引擎要用，而且副本的 `build_config_with_fidelity` 正是从它建 `cfg.limit_roles`），落库时由 `seal_limit_key` **重新封存** —— 这与同文件里 provider api-key 的先例一致（`restore.rs` 一直在用 `kp.seal` 重封，而不是搬运密文），于是"一行怎么被写进库"只有**一条规则**（都走 `seal_limit_key`），而树的封存形态只服务于"树这件载体"自身。代价是同一把 key 在树里与在库里是**两个不同的信封**（nonce 不同），语义相同、可读性相同；收益是不必让"树的值"和"库的值"必须同源，也就不会出现"只有经由树才封存"的第二条写路径。

### 2gl-C. 自动迁移：为什么**不**放在 reader 里（这是"把读变成写"的坑，且有一个既有测试会抓）

`seal_legacy_limit_keys(pool, kp)` 由**加载器**（`store::build_config`，启动与每次 reload 都跑）在**读之前**调用 ⇒ 无需人工步骤。它的形状是三步，每一步都有理由：

1. `legacy_limit_key_rows`：**先纯读**（不加锁）⇒ 已封存的库上**连写事务都不开**（否则"加载器跑过"就等于"加载器每次都拿写锁"）；
2. 有存量行才 `begin_write`；
3. 每行用 `reseal_limit_key_row` 做 **CAS**（`WHERE id = ? AND matching_key = ?`，绑的是**读到的明文**）⇒ 读与写之间被 admin 写入（或另一节点先封存）改掉的行**不覆盖**，且**只统计真正改写的行数**（日志数字才不是谎话）。

★**为什么不放进 reader**：`list_limit_roles_on` 是在 `ReplicationContent::load` 的**普通（deferred）读事务**里被调用的，在那种事务里写会去拿写锁、撞上并发写者就 `busy_timeout` 后失败 —— 而门禁里正有一条测试专门钉这件事（`load_does_not_block_on_a_concurrent_writer_holding_the_write_lock`）。所以"读"保持只读，迁移是**独立的一次写**。

### 2gl-D. 主密钥轮换（`hydra --reseal`）必须覆盖这一列，否则轮换是单向门

第三块 `limit_role`：与 provider/tenant 两块同一条规则 —— **"已是最新版本"要被 `open` 验证过**（版本号与密钥材料是两个独立旋钮）；不同之处是它**没有** `key_version` 列，版本在信封里，所以先在 `from_text` 里解析。另加一种情形：**明文行**（迁移与轮换谁先到都行）。`ResealReport` 增 `limit_keys_resealed`，`main.rs` 的 `reseal:` 行增 `limit_keys=N`。

### 2gl-E. 文档订正（含一处**同一段里的自相矛盾**，是第 209 轮留下的）

* `design.md` §10.1 与 schema 注释：`matching_key` 由"明文列"改为"**封存存放**（`sealed:v1:…`），NULL 仍表示匹配全部"，并写清**封存覆盖什么/不覆盖什么**。
* `ops.md` §4 整段重写。★**发现**：第 209 轮改写了"计数桶已按摘要计（D-15②）"这条，却漏了同一列表里更早的那条"**计数桶仍用掩码**（未变）⇒ 掩码相同的两个 key 共用一个配额" —— **同一屏内两条互相否定**的话。已合并为一条（D-15② 生效 + 历史实测留档）。
* 同一段还引用了**已被退役删除的 drill**（`integration/test_replica_fidelity.py`，仓库里已无此文件 —— `ls` 实测）作为"原始形式可用"的实测出处 ⇒ 改为**活着的**那条 `integration/test_limit_roles_enforcement.py`（同一形式、同一实测 `[200,200,429,429]`），并写明旧引用为何消失。
* `ops.md` 的 `reseal` 报告行清单**多了 `limit_keys=`**（`main.rs` 打印 + `ResealReport` 新字段）⇒ 文档两处字段清单同步；`integration/test_key_rotation_live.py` 用的是**子串**断言（`"provider_keys=1" in report`），不受影响（未改 drill，故其覆盖仍止于"两类封存行"）。
* `hydra-core/src/limit.rs`：`KEY_DIGEST_PREFIX` 的说明由"该列是明文列"改为"该列已封存；摘要买的是**封存买不到的那一半**——不存可还原物"。
* `hydra-core/src/config.rs` 的 raw-key 告警**改写**（★这条不只是措辞）：原文说"该列 kept and replicated **in PLAINTEXT**"，D-16 之后**这句是假的**，而"把话说大了的告警＝训练运维跳过清单"；改为点名**仍然成立**的暴露（持主密钥者可还原 + `GET /api/v1/limit-roles` 明文回显），并保留"写摘要"的建议。`hydra-core/tests/validate.rs` 同步**两向**断言：必须出现新措辞、**不得**再出现 `PLAINTEXT`。
* **限度（如实标注）**：admin API 仍**明文回显**该值（`GET /api/v1/limit-roles` + admin UI）。第 209 轮记的是"改回显是对外形状变更，需要你点头"，本轮**保持现状**，未改对外形状。

### 2gl-F. 测试与证伪（13 条新增、14 枚探针全红；另有两处★自我发现）

**新增**：`crypto` 2 条（信封往返且不含 key／非信封按明文过、信封打不开必须报错）、`db::tests` 2 条（CAS 拒绝被改过的行／CAS 正常改写且幂等）、`arachne_entities` 1 条（**每条** blob 都不含客户 key + 两条载体都往返 + 旧树明文可读）、`tests/limit_key_sealing.rs` 6 条（库内非明文且仍匹配／UPDATE 也封／存量行可读且被加载器重封存且幂等／副本重建后也封／信封打不开即拒绝（且用对密钥能读）／NULL 仍是 NULL）、`key_rotation` 1 条（轮换顺手封存发现的明文行）。夹具同步加强：配置树的两处 limit-role 夹具**真的带了一把客户 key**（否则"树里没有明文"是在空集上断言）。

**证伪（14 枚探针，逐枚只改生产代码一处、跑完立即还原；收据 `.acceptance/round210-probe.out`，驱动脚本 `.acceptance/round210-probe.py`）**：P1 insert 不封 / P2 update 不封 / P3 reader 不解封 / P4 加载器不迁移 / P5 CAS 去掉 `AND matching_key = ?` / P6 restore 明文落库 / P7 非信封当错误 / P8 每角色实体不封 / P9 保真实体不封 / P10 树解码不解封 / P11 轮换跳过该列 / P12 信封直接放明文 / **P13 树解码按旧规则把明文当 key / P14 树格式静默退回 3**。**14/14 全部让指名的那条测试变红**（其中 P3/P10/P12 顺带打红同族兄弟，已如实记录在收据里）。

★**自我发现①（"不会失败的检查"，我自己造的）**：CAS 那条测试**第一版把 UPDATE 语句抄在测试里**，于是**删掉生产代码里的 CAS 子句它照样绿** —— 它断言的是**代码的副本**。修法是让**两半都走生产代码**：把迁移拆成 `legacy_limit_key_rows`（读）与 `reseal_limit_key_row`（CAS 写），测试按**竞态发生的顺序**调用它们（读 → 并发写 → 写回陈旧信封），因此它现在**只能**因生产 CAS 存在而绿。这也是它必须放在 `db.rs` 的 `#[cfg(test)]` 里（集成测试够不到 `pub(crate)`）。
★**自我发现②（承重点测错了）**：`open_text` 的"明文直通"这条分支，我原先的测试**只能**通过 `build_config` 走到它，而加载器**先封存再读** ⇒ 把该分支改成错误时那条测试**仍然绿**（探针 P7 报 NO-RED 才发现）。改成**先**断言 reader（尚未迁移时）能读出明文，再跑加载器看迁移；探针 P7 随即变红（`.acceptance/round210-probe.out`）。

### 2gl-F2. ★★本轮最重的一条：封存**改变了树实体字节的语义** ⇒ 必须 bump `TOC_FORMAT`（3 → 4），否则混版本集群**静默 fail-open**

这是我自己在收尾自查时问出来的问题，不在上一轮那份"落点清单"里：`limit_role/<id>` 实体里那个字段**字节形状没变**（还是 `Option<String>`），但**语义变了**（明文 key → `sealed:v1:…` 信封）。而 `arachne_keys.rs:58` 的 `TOC_FORMAT` 文档写得很清楚：「A toc carrying any other value is refused rather than guessed at: **a mixed-version cluster must fail loudly, because the alternative is one node writing a tree the others decode differently**」，历史两次 bump（1→2 cert 私钥载荷、2→3 实体键加内容哈希）都属这一类。

**不 bump 的后果是"限流悄悄不再生效"**：一个还说着 3 的旧节点解同一份字节时，会把 `sealed:v1:…` **整个字符串当成要匹配的 key** ⇒ 所有按 `matching_key` 限流的角色**永不命中**（fail-open，无日志、无指标）。于是：

* `TOC_FORMAT` **3 → 4**（`arachne_keys.rs`，文档里补第 4 条理由），混版本双方**按名字拒绝**对方的树（`the table of contents declares format 3; this build speaks 4`），拒不物化的节点继续服务**上一份已知良好配置** ⇒ 方向是**大声拒绝**而不是**静静失效**。
* **树侧因此不再需要字段级 fallback**：`opened_limit_role` 由"非信封按明文过"改为**严格**（按名字报 `EntityCodecError::Crypto` 并点名角色）——树的兼容性由 `TOC_FORMAT` 负责，字段级 fallback 反而会让**被误解码的树**活下来（这正是"旧节点的明文"与"新节点的信封"共用同一字段的后果）。**数据库侧保留 fallback**（那一列有过真实存量行，且加载器迁移在读之前跑），两侧**故意不对称**，理由写在两处注释与 2gl-A。
* **订正前一轮的措辞与本轮早先的措辞**：我先前在 `crypto.rs`/`db.rs`/测试文档里写的"旧树按明文读（滚动升级）"**是错的**（bump 之后它根本到不了解码器），已按实测改写；`cluster.md` 的键空间表把 `TOC_FORMAT = 3` 更正为 4 并写明原因。
* **加一枚防遗忘的钉子**：`arachne_keys` 新增测试 `the_toc_format_is_pinned_with_its_reason`（断言 `TOC_FORMAT == 4`，失败信息要求"改了语义就 bump 并在上方文档补一条；没改语义的话，这条测试就是提醒你刚才发生了一次 bump、旧版本从此拒收这棵树"）——全仓**没有别的地方**会因为忘记 bump 而变红。
* **运维面**：`ops.md` §4 与 `design.md` §10.1 各加一条 **UPGRADE NOTE**（升完所有节点再发布配置变更；拒绝是刻意的安全方向）。
* **证伪**：P13（树解码按旧规则读明文）与 P14（格式静默退回 3）各自**恰好**打红自己那条测试（`.acceptance/round210-probe.out`）。

**一处诚实的限度**：这一条**没有**做"两个真实不同版本的节点在同一个集群里互相拒绝"的端到端实验（那需要构建一个旧格式的二进制并起真集群）；支撑它的是 `Toc::decode` 的格式检查（既有测试 `malformed_tocs_are_refused` 覆盖 `UnsupportedFormat`）+ 新增的 pin 测试 + 静态读取。记在这里，而不是让它看起来像实测过。

### 2gl-G. ★顺带发现的真洞：arachne 门控模块的 `--lib` 测试（278 条）**谁都不跑**

新加的树测试落在 `cluster/arachne_entities.rs` 的 `#[cfg(test)] mod tests` 里，而它在 `--features server` 下**整个模块被编译掉** ⇒ 我查它到底有没有被执行时发现：门禁那条 `arachne rust tests (in-process)` 用的是 `--test` 目标列表，CI 同一步骤一样 ⇒ **`crates/hydra-server/src/cluster/` 自己的单元测试（278 条，`cargo test -p hydra-server --features server,cluster-redis,arachne --lib` = 278 passed / 0 failed，实测）在本地门禁与 CI 里都不执行**。这不是本轮引入的，但**本轮的证据落在了里面**，所以就地补上：门禁那一条 entry 与 CI 同名 step 各加 `--lib`（同一次 `cargo test` 调用里加目标选择器，不加 entry、不改计数）。（`check_ci_wiring` / `check_gate_entries` 两条守卫随后都 exit 0。）

**门禁（实测）**：`.acceptance/round10-gate.sh` ⇒ **99 条目、全部 `exit=0`、`OVERALL=GREEN`、`GATE_EXIT=0`**（收据 `.acceptance/round210-gate.out`，日志 `.acceptance/round10-gate.log`；`entries=99`、`GATE COMPLETE`）。末条 `tree manifest (verify)` 报 **`OK (the source tree is unchanged: 380 file(s), docs-only changes: 1)`**（那 1 个 doc 是本记录 —— 它在跑动期间被补写过），⇒ **没有代码移动，99 条判定覆盖这份代码树**（与第 201/207/209 轮同一条判据）。★本轮**新加的 `--lib` 目标真的被执行了**：`GATE arachne rust tests (in-process) exit=0` 里现在既有 279 条 lib 测试、也有原来的 7 个 `--test` 目标。**开跑前**按 pre-flight 做了 `cargo fmt --check`（clean）与 `check_public_claims --measure --write`（788 = 262 core + 526 server，复核 exit 0）；两条守卫 `check_gate_entries` / `check_ci_wiring` 也各自 exit 0。

**计数**：新增 Rust 测试 ⇒ 公开页按 pre-flight 规则先 `cargo fmt --check && node scripts/check_public_claims.cjs --measure --write` ⇒ 报 **788（262 core + 526 server）**，`--measure` 复核 exit 0（`--features server` 下 arachne 模块不参与计数，所以 12 条新增里只有 11 条进这个数字；第 12 条由 2gl-G 新补的 `--lib` 入口执行）。

## 2gm. 第二百一十一轮（批次 A）：把"待决策清单"本身变成事实 —— 清退役残留、订正决策表、给两处误报的事实归位

**这一轮的输入是用户的一句要求**："给我具体的当前状态，不要给中间态避免歧义"。于是我先把**清单本身**量了一遍，结果发现四条与事实不符的东西 —— 它们都不是代码缺陷，而是**记录缺陷**，而记录缺陷恰恰是"歧义"的来源。

**★发现①：D-3（OC-4 行为侧）根本不是待决策项 —— 它随退役消失了，和 D-8 同一形态。** 它描述的是"租户写必须绕边缘节点、打到 leader 自己的数据面恒 503 `no_leader`"。实测：转发层已随 ADR-0001 D-6 / 计划 T3.5 整体删除（commit `37f0bc3` "the entry node applies its own config write"）⇒ `crates/hydra-server/src/cluster/forward.rs` **文件不存在**；全仓 `grep -rn "no_leader"` = **0 命中**；租户契约 `tenant-api-integration.md:442` 早已写明"**任何节点都接受并执行租户写**"。**它却在决策表里挂了两轮**（第一百九十九轮的"前提复测"甚至还在引用 `forward.rs:38/41/109` —— 那三个坐标指向一个已删除的文件，而这正是"复测"应当抓到却漏掉的一类：**引用的文件是否还存在**）。本轮把该行改成"已随退役消失"，并把 `tenant_api/handlers.rs` 里残留的三处退役叙述清掉：①V5 文件块注释仍在讲"cluster 节点转发给 leader + 单节点本地写"两分支与已不存在的 `TenantConfigForwarder`；②`write()` 的 doc 仍写"(forward | local write)"；③第 (2) 段把"每租户配置写节流"归给已删除的模块 `admin::tenant_config_api::throttle_gate`（该模块**不存在**）—— 三处都改成现状，并各自注明"曾经是什么、为什么删"（这段历史有价值，因为它解释了为什么这个路径**没有**独立节流器）。

**★发现②：D-4 比表里写的窄 —— 代码是有意的，矛盾的只有一条注释。** 枚举级注释称"缺失与外来不可区分（无 oracle）"，而 DELETE 分支的行为一直是 **缺失 → 幂等 204、外来 → 404**，且分支内的注释写了实话。本轮按"**行为为准**"改枚举级注释，并把理由写进去（对"什么都没删"的请求回 204 是更大的谎；id 是 `gen_id()` 的纳秒串、不可枚举 ⇒ 这个 oracle 不可利用）。**对外语义那一半（要不要为了一致让外来行也回 204）仍留给你**，表里如实标注。

**★发现③：我在上一轮报告里对 D-10 说错了一句，本轮订正。** 我说"管理面 POST 路径上 `upsert_sub_tenant_by_name` 不可达 ⇒ 清不可达代码"。核对代码后：**顺序重复**确实被校验器拦成 409（走不到 update 分支），但**并发**创建（两者都基于同一份插入前快照校验通过）时，`ON CONFLICT (tenant_id, name) DO UPDATE` 正是"输家不因自己看不见的约束而失败"的保险 ⇒ **它不是死代码**，本轮**没有删它**，而是把这层区分写进 `create_sub_tenant` 的文档（原来只有"幂等 upsert"一句，读者会以为第二次 POST 会成功）。**结论：D-10 只剩"409 还是收敛"这一个产品选择。**

**★发现④：D-12 按选项②落地（标注，不改写）**：`dev-docs/changelog-2026-09-16.md` 头部加免责声明，并给出**逐个数**的实测 —— 该文档引用的 **29 个 7 位 SHA 里 27 个** `Not a valid object name`，只有 `c3eaa6f`/`d508daa` 存在 ⇒ 这份归档写于**另一段历史**（生产仓库那天），**原文一律不改**（把 SHA 换成"看起来像"的提交等于编造对应关系）。同时注明"显示明文开关已删"与现状的差异（`?reveal=1` 残骸仍在，但**行为上**确实永不回明文）。

**决策表与状态行同步（本轮的机械部分）**：D-5② / D-13① / D-15② / D-16①③ / D-17① 五行标为**已落地**（各带 §2gk/§2gl 指针）、**新增 D-13 与 D-14 两行**（此前这两项只在正文里，表里没有行 ⇒ "待决策 N 项"根本数不准）、D-1 行补上本轮实测（**该开关没有任何入口能打开**）、D-16 行补上**仍待决定的那一半**（admin API 是否继续明文回显 `matching_key`）、**计划头部状态行**由"待决策 16 项"改为**真实的 9 项**（8 项待取向 + 1 项附带），并逐条列出。

**清单的真实状态（本轮结束时的定稿）**：
* **待你取向 8 项**：D-1 / D-4（仅剩对外语义）/ D-6 / D-7 / D-9 / D-10 / D-11 / D-14。
* **附带 1 项**：D-16 第二半（`GET /api/v1/limit-roles` 是否继续明文回显）。
* **已闭环**：D-2、D-3（退役消失）、D-5②、D-8（退役消失）、D-12、D-13①、D-15②、D-16①③、D-17①。

**本轮不改行为**：全部是注释/文档 + 决策表，`cargo fmt --check` clean、`clippy -D warnings` exit 0；门禁照跑（判定见收据）。
