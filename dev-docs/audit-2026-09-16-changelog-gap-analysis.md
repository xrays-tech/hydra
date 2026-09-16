# 审核文档：`changelog-2026-09-16.md` 工作项 vs 当前代码 交叉对比

- 日期：2026-09-16
- 状态：**待人工裁定**（范围与契约决策见 §7；裁定后产出修复计划）
- 作者：编码智能体（DeepSeek）
- 审核对象：`dev-docs/changelog-2026-09-16.md`（27 个提交、4 条主线 + 2 项横切工作的归档）
- 被审代码：本仓库工作树 HEAD = `21d8066`（本地 `main`，领先 `origin/main`=`30011a8` 共 22 个提交、落后 0）
- 审核方法：以文档中每一条**可验证断言**为单位，逐条在代码中寻找实现点；**只接受实现点作为证据，不接受文档/注释自述**。5 路独立子代理分区核查（各主线 + §7 未决项 + 文档清单），随后对全部 P0 结论与关键 P1 结论**逐条人工复验**（复验命令见 §6）。

---

## 0. 执行摘要

1. **文档与代码库不是同一条历史。** 文档附录 A 的 27 个 commit 在本仓库**全部不存在**（`git cat-file -e` 27/27 失败，`--all` 亦无）。本地 `main` 最后一个提交是 **09-16 12:26**，而文档覆盖到 **17:32**。本仓库是同一批主题的**并行实现**（不同 commit、不同机制），其 09:59–12:26 段与文档区间重叠。
2. 因此缺口分两类：**(甲) 代码里确实没做**；**(乙) 文档/证据链在本仓库不存在**（可能属于运维/部署仓库，需裁定，见 §7-Q2）。
3. **4 项 P0 缺口在生产上会真实复现**，其中 2 项是数据/身份完整性问题（副本物化销毁禁用行、副本主键重生成），1 项是文档声称已修但完全未实现的完整主题（节点注册表回收），1 项是 HTTPS+h2 全量 404。
4. **文档引用的 8 篇文档在本仓库从未存在过**；文档点名的 4 个测试文件与 3 个 e2e 用例 id 亦不存在；文档中的指标名/告警名/端口映射表全部只在 changelog 自身出现。
5. 同时，**文档的部分主题在本仓库已实现且实现质量不低于文档描述**（监听器语义、edge 证书热加载、AuthCache 死锁、`invalidateFK`、租约 `Uncertain`），这部分**不应重复开发**（见 §3）。

---

## 1. 结论一：历史不一致（所有后续结论的前提）

| 检查项 | 结果 |
|---|---|
| 文档附录 A 的 27 个 commit（`110774c`…`40f5912`） | **27/27 不存在**（`git cat-file -e <c>^{commit}` 全失败） |
| 文档引用的 `dev-docs/reviews/`（第 47–52 轮） | **目录从未存在**；`git log --all --diff-filter=A -- 'dev-docs/reviews/*'` 为空 |
| 文档引用的 `dev-docs/reviews/slices/R9-admin-ui.md` | 从未存在 |
| 本地 `main` HEAD 时间 | **09-16 12:26**（`21d8066`） |
| 文档覆盖时间区间 | **09-16 09:59 → 17:32** |
| 文档覆盖但本仓库无实现的区间 | **12:39 → 17:32**（主线 C 全部、主线 D 全部、主线 B 后半） |
| `origin/main` | `30011a8`（含在本地历史中，本地领先 22 个提交，落后 0） |

**判定**：文档描述的是一条外部历史。其 12:39 之后的全部工作（主线 C、D 及 B 的后半）在本仓库**没有任何对应实现**。文档中的统计数字亦非本仓库测得——例如"e2e 12 passed"恰好 = 本仓库现存 8 个 admin 用例 + 1 个 lang 用例 + 3 个本仓库不存在的用例 id（`T2.1c`/`T2.1d`/`T2.2b`），说明该数字是从另一条历史带过来的。

**注意**：这不等于文档"错了"——文档可能准确描述了另一个部署/分支的真实工作。但**它不能作为本仓库的上线、回滚或"已完成"依据**。第 8 节"回滚与风险提示"尤其危险：例如"回滚监听器修复的镜像前必须先清空租户证书"这条，对本仓库当前代码就不成立（见 §3 监听器条目）。

---

## 2. 缺口清单

严重度定义：**P0** = 生产症状会真实复现或存在数据/身份完整性风险；**P1** = 功能或可观测性缺口，影响运维与验证能力；**P2** = 测试/文档缺口，影响回归保护与交接。

### 2.1 P0 — 生产症状会真实复现

#### G1 副本物化会永久删光"禁用行"（数据丢失）

| 项 | 内容 |
|---|---|
| 文档断言 | `12accda`：wire 只带 enabled 行而 `restore_config` 全表重建 ⇒ 任何一次物化都会删光副本上的禁用行；已补齐 `limit_roles`/`key_prefix_bindings` 两组全量 fidelity 行 + fail-closed 守卫 |
| 实际 | **未修，缺陷存活** |
| 证据 | ① `crates/hydra-server/src/db.rs:1323-1334` 物化前全表 `DELETE FROM ... limit_role / provider_key_binding`（`WipedTable` 列表含 `ProviderKeyBinding`、`LimitRole`）；② 重建只从**已过滤**配置来：`crates/hydra-server/src/store.rs:162-175` `list_limit_roles(...).filter(\|r\| r.enabled)`、`list_provider_key_bindings(...).filter(\|b\| b.enabled)`；③ `crates/hydra-server/src/cluster/snapshot.rs:61-69` 的 `SnapshotWire` 只带 `provider_models` / `tenant_providers` / `tenant_models` 三组 fidelity 行，**不含** `limit_roles` / `key_prefix_bindings`；④ 全库无 wire 版本号、无 `#[serde(deny_unknown_fields)]`、无 legacy wire 拒绝逻辑 |
| 后果 | 每次快照物化，副本上的**禁用** limit_role / key_prefix_binding 全部消失；主备切换后配置静默缺失（数据丢失而非报错） |
| 附注 | "保留 last-known-good"**偶然成立**：`control_client.rs:262-282` 在 hydrate/解析失败时返回 `Err` 且不应用。但这是必需 serde 字段的副产品，**不是** fidelity 守卫 |

#### G2 节点注册表回收完全未实现（文档整条主线缺失）

| 项 | 内容 |
|---|---|
| 文档断言 | `e909f28`+`317377a`：注册表值升级 `v2\|role\|control_url\|last_seen`；`sweep_stale(grace)`；心跳收敛为 `register(ttl)`；60s 回收任务跑在每个集群节点；身份优先级 `HYDRA_NODE_ID`→`HOSTNAME`→随机；优雅停机 `unregister()`；新增 `HYDRA_REGISTRY_STALE_GRACE_SECS` + 2 指标 + 2 告警。上线实测 113 行/108 离线 → 4 行全活 |
| 实际 | **完全未实现**：14 项断言中 **12 项 ABSENT、2 项 PARTIAL** |
| 证据 | ① `crates/hydra-server/src/cluster/registry.rs:86` 值仍是旧两段式 `format!("{}|{}", self.role, self.control_url)`；② 全库无 `sweep_stale`、无 `last_seen`、无 `v2` 前缀、无 grace 常量/环境变量（仅 changelog 出现）；③ `registry.rs:105` `unregister()` 是唯一删除入口（`registry.rs:106` 是全库唯一 HDEL），**无任何生产调用点**（`grep -rn unregister crates/` 仅自身定义 + 自身单测 + `forward.rs:251` 一句断言字符串）；④ `crates/hydra-server/src/cluster/mod.rs:118-121` 身份仍是 `HYDRA_NODE_ID` → 随机 `node-{:x}`，**全库不读 `HOSTNAME`**；⑤ `registry.rs:313` 单测把"心跳过期后行仍可见"写成期望值（`assert!(!b_entry.alive, "expired heartbeat ⇒ down but still visible")`），即缺陷被测试固化；⑥ `hydra:{nodes}` 哈希键无 `EXPIRE`（`registry.rs:89`） |
| 次级缺陷 | `refresh_heartbeat`（`registry.rs:112-124`）只 `SET` 心跳、**不重写行**，而 `register()` 只在启动调一次（`main.rs:374`，随后 `main.rs:377` 每 20s 调 `refresh_heartbeat`）。⇒ 节点 `role`/`control_url` 启动后变化时，行值**永远停在启动时**却一直"活着"；文档所述"续期即重写行"的设计下不可能发生 |
| 后果 | 文档 §3 症状（113 行/108 离线）**必然复现**；管理端"运行状态"会堆出无上限的离线节点（`admin/handlers.rs:2007-2017` 不过滤 `alive`，`admin-ui/app.js:1099-1109` 全部渲染） |

#### G3 `provider_key` 副本身份被重新生成（主键可能撞车）

| 项 | 内容 |
|---|---|
| 文档断言 | `7ce718f`：wire 逐条携带身份；`gen_id_static`/`now_static` 删除，统一到 `ids::new_id`（进程内单调计数 + 纳秒前缀 + 随机盐） |
| 实际 | **未修，缺陷存活** |
| 证据 | ① `snapshot.rs:58` wire 只有 `HashMap<String, Vec<SealedDto>>`，`SealedDto` = `{ciphertext, nonce, key_version}`（`snapshot.rs:31-35`），**无 id/created_at**；② `db.rs:1373-1390` 插入时 `.bind(gen_id_static())` / `.bind(now_static())`；③ `db.rs:1510-1517` `gen_id_static()` = 纯纳秒 `format!("id-{nanos:x}")`——**无单调计数、无随机盐**；④ 全库无 `ids` 模块、无 `new_id` |
| 后果 | 副本非逐字节一致；切换后按 id 删除返回 404；**同一 provider 多个 key 在纳秒粒度下可能主键冲突**（文档所述失败模式仍可达） |

#### G4 HTTPS + HTTP/2 请求全部 404（数据面挂）

| 项 | 内容 |
|---|---|
| 文档断言 | `7eb3093`：`request_filter` 只读 `Host` 头，而 pingora h2 服务端把 authority 留在 `request_header.uri` 且从不合成 `Host` ⇒ 所有 HTTPS+h2 客户端 404 `unknown_domain`；修复为 `Host` → `uri.host()` 依次取 |
| 实际 | **未修** |
| 证据 | `crates/hydra-server/src/proxy.rs:302-307` 只读 `host` 头（`.unwrap_or("")`）；`resolve_tenant(cfg, host: &str)`（`proxy.rs:153-161`）签名中无 uri；全库无 `uri.host()`/authority 回退（`grep -rn "uri\.host()\|\.authority" crates/` 仅命中上游 endpoint 的 `peer.rs:44,124,135`、`rewrite.rs:196`）；`git log --all -S"uri.host"` **为空**（两条历史都没有）；无 h2 测试 |
| 后果 | 文档 §5 生产现状称数据面走 h2（`https://api-test.do.top/v1/models` 200），若生产确为 h2，则该入口在本代码下解析为空 host ⇒ 落到 `localhost` 租户或 404 |

### 2.2 P1 — 功能与可观测性缺口

| # | 缺口 | 证据 |
|---|---|---|
| **G5** | **管理端会话持久化 + 401 统一处理（文档 4.2 整条）未实现** | `admin-ui/app.js:79` `let TOKEN = null;`（纯内存）；`app.js:1229` 在 `DOMContentLoaded` 中**无条件** `showLogin()`；`api()`（`app.js:99-124`）无任何 401 分支；`sessionStorage` / `hydra-admin-token` / `restoreSession` 在 `admin-ui/` **0 命中**（唯一 `localStorage` 用法是 i18n 语言 `i18n.js:1360,1395`）。⇒ 文档所述"刷新浏览器就掉登录"的原始 bug **至今存在**。产品文案反而与代码一致：四语帮助文本均写"kept in memory only"（`i18n.js:30/362/694/1026`） |
| **G6** | **幂等 reload 仍推进代际** | `crates/hydra-server/src/store.rs:430-431` `let next = self.version.load(...) + 1; db::set_config_version(pool, next)` ——无条件自增，无内容比对；`ConfigData`（`hydra-core/src/config.rs:36`）未派生 `PartialEq`；`store.rs:553-554` 的单测还断言了该自增。⇒ 文档 bug ① 仍潜伏（无变更也换代际 ⇒ 副本反复重建） |
| **G7** | **文档声称的指标名与告警名均不存在；全库无任何告警规则** | 仅出现在 changelog：`hydra_control_writer_ahead_total`、`hydra:{ctl:version}`、`hydra_proxy_listener_tls`、`hydra_proxy_tenant_certs`、`HydraHttpsDisabledWithCerts`、`hydra_registry_nodes`、`hydra_registry_reaped_total`、`HydraRegistryStaleRows`、`HydraRegistryChurn`。真实指标：`hydra_listener_bound`（`admin/metrics.rs:288`）、`hydra_listener_misconfig_total`（`:294`）、`hydra_control_poll_total`（`:276`）、`hydra_control_snapshot_version`（`:282`）。`grep -rln "alert:" --include=*.y*ml .` **为空**；无 prometheus/grafana/alertmanager 资产 |
| **G8** | **CI 无 `ui-e2e` job** | `.github/workflows/ci.yml` 仅 3 个 job：`check`(L19)、`optional-features`(L109)、`scripts`(L152)；无 Playwright/`seed.sh`/`HYDRA_ENCRYPTION_KEY` 步骤；`git log -S'ui-e2e'` 为空。仓库**无 `package.json`/`node_modules`**，`npx playwright test` 现状无法在 CI 跑。现有 UI 相关门禁只有 `scripts` job 的 i18n 校验（L161/164/167） |
| **G9** | **`internal_control` 完全没有租约校验** | `crates/hydra-server/src/admin/handlers.rs:2172-2178` 先做 `since >= current` 短路；全函数只受 cluster token 保护（`admin/mod.rs:506-523`），无任何 leader/租约判断。文档所述"租约检查必须提前到短路之前"在此设计中**连被排序的对象都不存在**（同时也意味着文档描述的冷启动死锁在本设计下不会发生——见 §3 更正） |
| **G10** | **§7 七项待决策：6 项仍开环，仅 1 项部分改善** | 详见 §4 |

### 2.3 P2 — 测试与文档缺口

**测试缺口（文档点名但不存在）**

| 文档声称 | 实际 |
|---|---|
| `crates/hydra-server/tests/boot_listeners.rs` | **不存在** |
| `crates/hydra-server/tests/redis_real.rs`（含冷启动选主用例） | **不存在** |
| `crates/hydra-server/tests/fidelity_disabled_rows.rs` | **不存在** |
| `crates/hydra-server/tests/provider_key_fidelity.rs` | **不存在** |
| 注册表 `sweep_stale` 系列单测 | **不存在**（且 `sweep_stale` 本身不存在） |
| e2e `T2.1c` / `T2.1d` / `T2.2b` | **均不存在**（现存 9 个用例：T2.1、T2.1b、T2.2、T2.3、T2.4、T2.5、T2.6、T2.7 + lang 1 个） |
| `lang.spec.cjs` 已改为"reload 后不再弹登录框" | **恰好相反**：`tests/e2e/lang.spec.cjs:35-40` 仍断言 reload 后登录框**必须**出现（`#login-overlay` visible） |
| `tests/tls.rs` T6.5 双监听 / T6.6 fullchain 且"客户端必须能验到根" | 文件存在但语义不同（`t6_5_fullchain_pem_presents_the_intermediates`、`t6_6_single_cert_pem_has_an_empty_chain`）；**"验到根"实际未验证**：`tls.rs:453` 设 `SslVerifyMode::NONE`，fixture 全为自签叶子（`acme.crt` issuer=CN=acme.com；"中间证书"用的是无关的 `beta.crt` issuer=CN=beta.io），无 CA/root fixture |

**孤立的测试资产**：`tests/e2e/stats_autorefresh.cjs` 是自建 server 的独立 node 脚本，文件名不匹配 Playwright 默认 `testMatch`（`playwright.config.cjs` 未覆盖 `testMatch`），CI 也不引用 ⇒ **不被任何门禁收集**。

**文档缺口（8 篇从未存在于本仓库）**

`dev-docs/edge-dataplane-probe-and-alerts.md`、`dev-docs/ops-upgrade-2026-09-16.md`、`dev-docs/ops-port-mapping-prod.md`、`dev-docs/bug-2026-09-16-h2-tenant-lookup.md`、`dev-docs/bug-2026-09-16-stale-registry-nodes.md`、`dev-docs/fix-plan-2026-09-16-tls-dual-listener.md`、`dev-docs/reviews/`（含 `slices/R9-admin-ui.md`）。

**仅 2 篇存在，但状态头已过期**：
- `dev-docs/bug-2026-09-16-auth-cache-guard-deadlock.md`（229 行，头部写"未修复"，而 `http.rs:183-190` 已修）
- `dev-docs/bug-2026-09-16-tenant-cert-flips-listener-to-tls.md`（223 行，头部写"未修复"，而 `listeners.rs:130` 已修）

**文档漂移（注释与代码不符，建议单独修）**
- `crates/hydra-server/tests/common/mod.rs:71-83` 声称端口"已消除 bind-then-release 竞争"，但 `:94` 就是 `TcpListener::bind(...).is_ok()` 后立即释放；且 `:91` 的 `pid % 100` 会让相隔 100 的 PID 撞带。
- `crates/hydra-server/src/main.rs:6` 仍写"plain `add_tcp` listener is used for the localhost/dev case (no certs)"——正是 B1 已移除的"证书决定协议"耦合。

---

## 3. 已实现（**不要重复开发**）

以下主题在本仓库已实现，实现质量**不低于**文档描述，部分优于文档（机制更干净）：

| 主题 | 文档主张 | 本仓库实际 |
|---|---|---|
| 监听器语义（文档 2.1） | `HYDRA_LISTEN` 恒定明文 + 独立 `HYDRA_TLS_LISTEN` + 非法/同址拒绝启动 + fullchain | **已实现，且是独立模块**：`crates/hydra-server/src/listeners.rs` 把拓扑做成"配置的纯函数"并文档化契约；`main.rs:832` 无条件 `add_tcp(&plan.plain)`；`main.rs:847` 条件 `add_tls_with_settings`；`listeners.rs:145` `SameAddress` / `:104-120` `BadAddress`/`NoTlsBackend`；`main.rs:167-169` `exit(1)`；`tls.rs:339` `ssl_add_chain_cert`。回归测试 `listeners.rs:260` `the_cert_count_never_changes_the_topology`（0/1/3/99 证书下拓扑必须相同） |
| edge 证书热加载（文档 2.2） | `refresh_certs` 挂在 admin reload + `PollOutcome::Applied` | **目标已达成，机制更干净**：`tls.rs:309-313` `follow_snapshot` 注册 `config.on_snapshot_change`，一处覆盖全部节点角色（admin reload 走 `store.rs:439` `notify`，edge 走 `control_client.rs:283` `apply_snapshot` → `store.rs:406` `notify`）；测试 `tests/tls.rs:606`。文档点名的 `refresh_certs` 符号不存在，但**行为等价** |
| 租户证书扁平负载（文档 2.3） | `#[serde(flatten)]` | **已实现**：`admin/handlers.rs:597-600` `struct TenantUpsert { #[serde(flatten)] tenant: Tenant, ... }` |
| AuthCache 自死锁（文档 1.6） | 进 L2 前 drop 守卫 + GC 接线 | **已实现**：`http.rs:183-190` 守卫被限制在同步函数 `l1_decision` 内，`check` 的 `await` 无守卫（`http.rs:200/210/212`）；GC 已接线 `main.rs:567` `spawn_gc_task(...60s)`，回归测试 `src/redis/auth_cache.rs:356-400` |
| `invalidateFK`（文档 4.1） | 被误删后补回 | **存在且从未被删**：`admin-ui/app.js:95`，调用点 `:723`（`submit()`）、`:902`（`doDelete()`）；`git log -S'invalidateFK'` 仅一个引入提交 `d508daa`。⇒ 文档 4.1 的根因叙述在本仓库**无对应事实** |
| 租约 `Uncertain` 非死状态（文档 1.2 既有缺陷） | 丢 key 的节点不能再被提升 | **已修**：`cluster/lease.rs:326-332` → 回 `Standby`；错误预算 `lease.rs:339-349`（`UNCERTAIN_ERR_BUDGET=3`）；回归测试 `lease.rs:596` |
| 无 Redis 时测试不静默通过（文档 1.2） | 补区分度 | **已实现**：`tests/common/mod.rs:45-52` 与 `src/redis/mod.rs:65-72` 缺 `HYDRA_TEST_REDIS_URL` 时 `panic!`；旧逃生门 `test_redis::configured()` 零调用点 |

**另外两点"文档描述在此设计中不成立"**，可避免误改：
- 文档 1.3 的冷启动租约死锁：本仓库 `internal_control` 无条件保留 `200 {snapshot:null}` 冷启动逃逸（`handlers.rs:2172-2178`），且租约与快照产端解耦（`replica.rs:244-252` 证据门 + `lease.rs:235-249` `sync_is_fresh`），该死锁组合不存在。
- 文档 1.1/1.5 的 Redis 集群标记 `hydra:{ctl:version}`：本设计把版本标记放在 **SQLite 的 `config_meta.config_version`**（`db.rs:1247`）随内容同事务提交（`906abd5` 的"marker 属于内容"），副本按 `?since=<version>` 轮询（`control_client.rs:237-240`）。⇒ 该键**在本设计下不必要**，不应为了对齐文档而新增。

---

## 4. 文档 §7"未完成/待决策"七项现状

| # | 事项 | 现状 | 精确缺口 | 证据 |
|---|---|---|---|---|
| 1 | `DeepSeek-V4-Flash` 挂住 / 300s 才放弃 / 熔断不判死 | **仍开环** | 上游只有 300s **整体**超时（`proxy/provider_client.rs:59`），`proxy.rs:738` 是裸 `send()`，**无首字节/单次尝试超时**；熔断探针 1.5s 打 `GET {endpoint}/v1/models`（非推理路径，`breaker_wrap.rs:228,267`）且 `<500` 即复活（`:273`），每 10s 一次 ⇒ **结构上无法保持跳闸** | 同左 |
| 2 | standby 转发 5s 超时语义 | **仍开环** | `forward.rs:34` 硬编码 `FORWARD_TIMEOUT = 5s`（不可配），仍返回 **502 `forward_failed`** 且消息断言 `"no local write"`（`admin/mod.rs:465-469`）——超时无法知道写是否已落地 | 同左 |
| 3 | 写后置步骤失败仍返回错误（行已提交） | **部分改善** | ✅ token/证书**形状**校验已前移到写库前（`handlers.rs:838-852`，调用点 `:927`/`:996`）；`reload_after_write` 已不存在，改为 `reload_best_effort`（`:135-152`，**从不返回错误**，只记 ERROR + 置 `hydra_config_snapshot_stale` 指标）。❌ 证书/token 的 **DB 错误**仍可落在提交之后（`handlers.rs:934`/`:1003` → `db_err_resp`），响应里仍**没有**区分"写入成功/reload 失败"的字段 | 同左 |
| 4 | `terminationGracePeriodSeconds` 30s vs Pingora 排空 300s | **仍开环（且前提不成立）** | **无排空配置**：`main.rs:783-784` `Server::new(Some(Opt::default()))`，全库未设 `graceful_shutdown_timeout_seconds` ⇒ Pingora 默认 **5s**（`pingora-core-0.8.1/src/server/mod.rs:787`），**不存在 300s 排空**；仓库**无 k8s manifest**（仅 compose），30s 无法从本库验证；`unregister()` 无生产调用点 | 同左 |
| 5 | `/admin` 自动 redirect 到 leader / 横幅替代 | **仍开环** | admin 代码零 `301/302/Location/redirect`；UI 无横幅、无"跳到 leader"链接，且**从不调用 `/healthz/leader`**（该路径只在 `api-docs.js:24` 与 i18n 文案里） | 同左 |
| 6 | "记住我" / 服务端会话 + HttpOnly Cookie | **仍开环（方案 A 也未落地）** | 内存态 token（`app.js:79`）+ 无条件 `showLogin()`（`:1229`）；无 `sessionStorage`；`grep "Set-Cookie\|HttpOnly\|/session\|/login" crates/` **0 命中** | 见 G5 |
| 7 | 快照不含租户 access-token 哈希 | **仍开环** | `SnapshotWire` 无哈希字段（`snapshot.rs:52-79`），`Tenant` 模型也无该字段；`db.rs:1388-1392` 的 tenant INSERT 不写 `access_token_hash`，而表每次物化被清空重建；standby `has_access_token` 恒 false（`handlers.rs:942/962/1013` → `db.rs:876-886`）；**401 发生在转发之前**（`admin/mod.rs:559-567` 早于 `:578-583`），故该端点从不转发 | 同左 |

---

## 5. 文档引用清单核对

| 文档 | 状态 | 行数 | 备注 |
|---|---|---|---|
| `dev-docs/bug-2026-09-16-auth-cache-guard-deadlock.md` | 存在 | 229 | 内容充实，但状态头"未修复"已过期 |
| `dev-docs/bug-2026-09-16-tenant-cert-flips-listener-to-tls.md` | 存在 | 223 | 同上 |
| `dev-docs/edge-dataplane-probe-and-alerts.md` | **缺失** | — | 从未存在 |
| `dev-docs/ops-upgrade-2026-09-16.md` | **缺失** | — | 从未存在 |
| `dev-docs/ops-port-mapping-prod.md` | **缺失** | — | 从未存在 |
| `dev-docs/bug-2026-09-16-h2-tenant-lookup.md` | **缺失** | — | 从未存在 |
| `dev-docs/bug-2026-09-16-stale-registry-nodes.md` | **缺失** | — | 从未存在 |
| `dev-docs/fix-plan-2026-09-16-tls-dual-listener.md` | **缺失** | — | 从未存在；相近内容见根 `CODE_REVIEW.md` §24（不同编号体系） |
| `dev-docs/reviews/`（第 47–52 轮）+ `slices/R9-admin-ui.md` | **缺失** | — | 目录从未存在 |

**生产端口映射表（文档 §2.4）**：`30090→8080`（`bug-2026-09-16-…to-tls.md:88`、`listener_topology.rs:7`）与 `30443→8443`（`bug-…to-tls.md:141`、`CODE_REVIEW.md:1663`）在仓库内有部分记载；**`30091→8081`（admin）只在 changelog 自身出现**；三行完整表仅存在于 changelog。

**未被文档引用但属当天真实产物的**：根 `CODE_REVIEW.md`（189 KB，mtime 09-16 12:26，采用"审核一/二/三/四 §1–§24"编号体系，§24 标"已全部完成"）——这才是**本仓库**当天的评审产物，文档完全未引用。

---

## 6. 审核方法与可复现命令

所有 P0 结论均已人工复验，关键命令：

```bash
# 0. 历史一致性
for c in 110774c 40f5912 ...; do git cat-file -e "$c^{commit}" 2>/dev/null || echo "MISSING $c"; done
git rev-list --left-right --count origin/main...HEAD; git reflog --date=short | head
git log --all --diff-filter=A -- 'dev-docs/reviews/*'          # 空

# G1 副本保真
sed -n '1311,1340p' crates/hydra-server/src/db.rs             # WipedTable 全表删除
sed -n '158,176p'   crates/hydra-server/src/store.rs          # enabled 过滤
sed -n '40,80p'     crates/hydra-server/src/cluster/snapshot.rs # wire 字段

# G2 注册表（全库仅 changelog 命中）
grep -rn --exclude-dir=target --exclude-dir=.git \
  -e sweep_stale -e last_seen -e HYDRA_REGISTRY_STALE_GRACE_SECS -e hydra_registry_nodes .
sed -n '78,130p' crates/hydra-server/src/cluster/registry.rs
grep -rn "unregister" --include=*.rs crates/                   # 无生产调用点
sed -n '1030,1052p' crates/hydra-server/src/main.rs            # 停机只 flush sink

# G3 provider_key 身份
sed -n '1373,1390p' crates/hydra-server/src/db.rs              # gen_id_static()/now_static()
sed -n '1504,1522p' crates/hydra-server/src/db.rs              # 纯纳秒实现
grep -rn "fn new_id\|mod ids" --include=*.rs crates/           # 空

# G4 h2 authority
sed -n '300,315p' crates/hydra-server/src/proxy.rs
grep -rn "uri\.host()" --include=*.rs crates/ ; git log --all -S"uri.host"   # 均空

# G5 会话持久化
sed -n '75,105p' admin-ui/app.js; sed -n '1215,1232p' admin-ui/app.js
grep -rn "sessionStorage\|hydra-admin-token\|restoreSession" admin-ui/       # 空

# G7 指标/告警
grep -rhoE '"hydra_[a-z_]+"' --include=*.rs crates/ | sort -u
grep -rln "alert:" --include=*.y*ml .                                        # 空

# G8 CI
grep -nE "^  [a-z0-9_-]+:" .github/workflows/ci.yml
```

---

## 7. 未决问题（需人工裁定后才能产出修复计划）

| # | 问题 | 为什么必须裁定 |
|---|---|---|
| **Q1** | **修复范围**：只做 P0（G1–G4），还是 P0+P1（含 G5–G10）？ | P0 是生产风险；P1 中的 G5（会话持久化）是用户可见缺陷，G7/G8 是运维/门禁资产，性质不同；范围直接决定计划规模与上线批次 |
| **Q2** | **G7/G8 及 k8s manifest 是否属于本仓库**？ | 本仓库无任何告警规则、无 prometheus/grafana 资产、无 k8s manifest（只有 docker-compose）。若这些在运维仓库，则本仓库只补指标名与 CI job；否则需新建资产目录 |
| **Q3** | **G1/G3 的 wire 兼容策略**：采纳文档的"fail-closed 拒绝旧 wire 格式"（滚动升级期间混版会停摆），还是在 wire 里加版本号做双读？ | 这是**快照契约**变更，影响滚动升级窗口；两方案对可用性的影响相反，必须由你定 |
| **Q4** | **G2（注册表回收）采纳文档设计还是重设计**？ | 文档的 `v2|` 前缀专为滚动升级兼容而设；本仓库 `list_nodes` 会把未知 role 显示成垃圾（`registry.rs:153`）。沿用文档设计需同时修 `list_nodes` 的版本处理 |
| **Q5** | **G4 是否需先确认生产真在跑 h2**？ | 若生产数据面是明文或 h1，则 G4 严重度下降；但文档 §5 明确写 h2，需以生产实测为准 |
| **Q6** | 是否顺带修 §2.3 的文档漂移与两份过期状态头？ | 属小改动，但会扩大提交面 |

---

## 8. 结论

1. 文档描述的是一条约 **09:59–17:32** 的外部历史；本仓库是并行实现，**12:39 之后无任何对应实现**。文档不能作为本仓库的上线/回滚/完成依据。
2. **4 项 P0 缺口是真实且可复现的**，其中 G1（副本删除禁用行）与 G3（副本主键重生成）属数据/身份完整性，G2（注册表回收）是文档自称已完成却完全未实现的整条主线，G4（h2 authority）会命中数据面。
3. **7 项主题本仓库已实现且质量不低于文档**，不得重复开发；另有 2 处"文档描述在本设计下不成立"（冷启动死锁、Redis 版本标记），不应为对齐文档而改动。
4. §7 七项待决策中 6 项仍开环，仅第 3 项部分改善。
5. 建议顺序：**先裁定 §7-Q1~Q6 → 产出修复计划 → oracle 严格审查 → 门禁通过 → 开发**。

---

## 附：本次审核未做的事

- 未修改任何代码或文档（本文件为新增）。
- 未运行测试套件、未执行 `cargo` 构建（审核为静态交叉对比；门禁在计划通过后执行）。
- 未验证生产集群实际状态（无集群访问）；文档中所有"上生产/上线实测"主张一律**未采信**。
