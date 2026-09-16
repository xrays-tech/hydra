# 2026-09-16 变更归档（当天工作留档）

> **归档时间**：2026-09-16（UTC）
> **提交范围**：`110774c`（09:59）→ `40f5912`（17:32），共 **27 个提交**
> **生产（k3s ns `hydra`）**：4/4 Pod = `172.16.29.88:30800/gpu/dogress2@sha256:94d54df3…`（commit `40f5912`）
> **用途**：这一天"改了什么 / 为什么 / 怎么验证的 / 上线到哪一版 / 还剩什么"的一页索引。
> 每条只写结论与证据，完整论证在各主题自己的文档里（每节末尾"关联文档"）。

---

## 0. 一页速览

| # | 主题 | 类型 | commit | 上生产 | 一句话 |
|---|------|------|--------|--------|--------|
| A | 集群一致性 / 副本保真 / 测试隔离 | fix+test+docs | `110774c` `58fc66e` `f0508f9` `fac2956` `5faeddd` `587f485` `12accda` `7ce718f` `8087ed7` `35e1496` `e865340` | 是（token dev 环境） | 修掉"整体重启后没人能抢到租约"的死锁、wire 保真补齐、AuthCache 自死锁、跨进程端口撞车 |
| B | 监听器语义 / 证书链 / HTTP2 | fix+test+docs | `530ab21` `e537e78` `7186676` `8cbc35d` `4aafe3d` `215abbb` `f4fc588` `7eb3093` `106d6f6` `85601c7` | 是（`f8d5e1b3…` → `49a90210…`） | 明文监听恒定、证书不再改协议；fullchain 上链；edge 会重解析证书；h2 authority 参与租户解析 |
| C | 节点注册表只增不减 | fix+docs | `e909f28` `317377a` `c638ec5` | 是（`6d04e83d…`） | 注册表 113 行/108 离线 → 4 行全活；节点身份改为 Pod 名 |
| D | 管理端 UI | fix+test+ci | `bccccb2` `40f5912` | 是（`ca92a191…` → `94d54df3…`） | 补回被误删的 `invalidateFK`（保存报错但已生效）；会话进 sessionStorage + 401 统一退出 |

四条主线之外的两件横切工作：**CI 新增真实浏览器 e2e 门禁**（`ui-e2e` job），以及**一整套生产运维记录**（端口映射、证书 runbook、上线/事故记录）。

---

## 1. 主线 A：集群一致性、副本保真与测试隔离（09:59–11:28）

### 1.1 D3 代际语义补洞（`110774c`）
三处语义漏洞：① 幂等 `reload` 也会推进代际（无变更却换代际 ⇒ 副本反复重建）；② 写者领先自己的副本被判成"分叉"并打 ERROR；③ 显式 `POST /reload` 推进了本地版本却从不发布集群标记（D3 形同虚设）。
→ 现在"只有真实变化才换代际"、写者领先单独计数（`hydra_control_writer_ahead_total`）、两条自增路径都发布 `hydra:{ctl:version}`。

### 1.2 复核修正批次（`58fc66e`）
第四十七轮 A+B+C 经两个独立 oracle 复验后，修掉本轮引入的 7 项 + 既有 4 项：
代际谓词窄于真实复制内容（离线 provider_model / 原样 join 行不在比较范围）、闸门分支缺区分度测试、无 Redis 时用例静默通过、`WriterAhead` 抹掉可观测信号、显式 reload 锁外读版本；既有缺陷：**租约 `Uncertain` 死状态**（丢 key 的节点永远不能再被提升）、`internal_control` 的租约检查必须提前到 `since>=current` 短路之前。

### 1.3 冷启动租约死锁（`f0508f9` + `fac2956` + `5faeddd`）
**事故**：三容器 Up、`restarts=0`，但 `/healthz/leader` 全 503、`lease_holder=None`、租约键为空；edge 拿不到快照 ⇒ 数据面 404。
**根因**：上一批两处各自正确的改动叠加 —— 产端对任何 `since` 都先校验租约（移除了冷启动时唯一能开 `sync_ok` 的 `200 {snapshot:null}`），而丢 key 的节点又回到抢租路径 ⇒ 没人持租约 ⇒ 没人能开闸 ⇒ **空闲租约永远没人抢**。
**修复**：`LeaseStore::cluster_version()` + `eligible_to_acquire() = sync_ok || 本地版本 ≥ 已提交版本`（读不到 ⇒ fail-closed，落后于已提交版本仍不被提升）。
**配套**：真实 Redis 冷启动回归用例（`fac2956`，含非空洞性反证）、`ops-upgrade` 症状表补两条与"节点重启后 20 秒内全 503 属正常"的说明（`5faeddd`）。

### 1.4 跨进程端口撞车（`587f485`）
`next_free_port` 只保证进程内唯一，且探测 bind 后立刻释放 ⇒ 两个并发测试进程抢同一端口，抢输方的 Pingora 静默 bind 失败，客户端被**别人的 Hydra admin** 应答（`401 missing or invalid admin token`）。改为每进程独占一段端口带（持有 socket 认领，避免依赖可写临时目录的文件锁）。

### 1.5 副本保真（`12accda` + `7ce718f` + `8087ed7`）
- `12accda`：wire 只带 enabled 行而 `restore_config` 全表重建 ⇒ **任何一次物化都会删光副本上的禁用行**；补齐 `limit_roles` / `key_prefix_bindings` 两组全量 fidelity 行 + fail-closed 守卫（老 wire 直接拒绝，保留 last-known-good）。
- `7ce718f`：`provider_key` 的 `id/created_at` 在副本侧被重新生成（纳秒主键）⇒ 副本不逐字节一致、切换后按 id 删除 404、同 provider 多 key 可能 PK 冲突。改为 wire 逐条携带身份；`gen_id_static/now_static` 删除，统一到 `ids::new_id`（进程内单调计数 + 纳秒前缀 + 随机盐）。
- `8087ed7`：第五十二轮审核报告归档。

### 1.6 AuthCache 自死锁（`35e1496` + `e865340`）
`AuthCache::check` 在 L1 未命中时把 DashMap 分片读守卫带过 `await`，L2 命中后对同 key `insert` ⇒ 同 task 自持读锁要写锁 ⇒ **永久自死锁**：Pingora 唯一工作线程 park 在 futex，`:8080` 不再 accept，而 `:8081/healthz` 仍 200 ⇒ k8s 不重启、约一半请求静默挂死。修复为进 L2 前 drop 守卫；配套 edge 数据面探针 + 僵死告警文档。

> **关联文档**：`bug-2026-09-16-auth-cache-guard-deadlock.md`、`edge-dataplane-probe-and-alerts.md`、`ops-upgrade-2026-09-16.md`、`reviews/`（第 47–52 轮）。

---

## 2. 主线 B：监听器语义、证书链与 HTTP/2（12:19–14:59）

### 2.1 监听协议不再由租户证书决定（`530ab21` + `e537e78` + `7186676`）
**现象**：写入租户证书后重启，唯一的 `:8080` 监听器变成 TLS ⇒ 通过明文入口（NodePort 30090）的公网流量整片失败；清空证书才恢复。
**修复**：`HYDRA_LISTEN` 现在**恒定是明文 HTTP**；HTTPS 由**独立的** `HYDRA_TLS_LISTEN`（如 `0.0.0.0:8443`）承担。地址非法/两者相同 ⇒ **拒绝启动**（exit 1），而不是"绑出个意外的东西"。证书链（fullchain）经 `ssl_add_chain_cert` 完整下发。新增 `hydra_proxy_listener_tls` / `hydra_proxy_tenant_certs` 与告警 `HydraHttpsDisabledWithCerts`。
**测试**：`tests/tls.rs` T6.5（双监听并存）/ T6.6（fullchain 真实握手，客户端必须能验到根）+ `tests/boot_listeners.rs`（env 装配 / 地址冲突拒绝 / TLS 端口不可用降级）。

### 2.2 edge 收到快照后重新解析证书（`8cbc35d` + `4aafe3d`）
edge 没有本地库，证书只随控制面快照到达；此前快照里的证书不会重新解析进 TLS 回调 ⇒ **edge 上的 HTTPS 永远握不上手**。现在 `refresh_certs` 同时挂在 admin reload（leader）与轮询钩子 `PollOutcome::Applied`（edge/standby）上。

### 2.3 证书下发 runbook 与上线（`215abbb` + `f4fc588`）
`PUT /api/v1/tenants/{id}` 的证书负载必须是**扁平**结构（`#[serde(flatten)]`，早前写成嵌套 `{"tenant": …}` 会 `missing field id`）；runbook 含 fullchain 下发、验证、清空回滚。上线顺序**硬性**：先滚含修复的镜像，再写证书（反向操作 = 立刻踩回地雷）。

### 2.4 HTTP/2 一律 404（`7eb3093`）+ 端口映射（`106d6f6`）+ 上线与事故记录（`85601c7`）
`request_filter` 只读 `Host` 头解析租户，而 pingora 的 h2 服务端把 authority 留在 `request_header.uri` 且从不合成 `Host` ⇒ 所有 HTTPS+h2 客户端 404 `unknown_domain`。修复为 `Host` → `uri.host()` 依次取。同时落档生产端口映射：

| 入口 | NodePort | Service | 容器 | 协议 |
|------|----------|---------|------|------|
| 公网 80 | 30090 | `hydra-edge` | 8080 | 明文（恒定） |
| 公网 443 | 30443 | `hydra-edge` | 8443 | TLS（SNI 租户证书） |
| 管理端 | 30091 | `hydra-admin` | 8081 | 明文（仅内网；Service 只选 Ready = leader） |

`85601c7` 另记录一次控制面事故：control Pod 的 Redis 长连接失效 → 租约无法续期 → 双 standby → 新 edge 无快照（404 / TLS `NO_CERTIFICATE_SET`）；恢复方式是**删 ordinal 0**（只删 standby 无效：STS `OrderedReady` + readiness=`/healthz/leader` 会形成无 leader 死锁）。

> **关联文档**：`bug-2026-09-16-tenant-cert-flips-listener-to-tls.md`、`bug-2026-09-16-h2-tenant-lookup.md`、`fix-plan-2026-09-16-tls-dual-listener.md`（§13.6 上线 / §13.7 事故 / §14 证书 runbook）、`ops-port-mapping-prod.md`。

---

## 3. 主线 C：节点注册表只增不减（15:46–16:13）

**现象**：反复删/重建 Pod 后，管理端"运行状态"堆出大量离线 edge/controller 节点（生产实测 `hydra:{nodes}` **113 行、存活仅 4 行**）。

**根因（三个各自合理的设计相乘）**：
1. hash 字段本身没有 TTL —— 会过期的只是另一个 key `hydra:{node:hb}:<id>`（心跳 30s），心跳过期只把行标成 `alive=false`，**行没人删**；
2. `NodeRegistry::unregister()`（唯一删除入口）在生产路径上**从未被调用**，而 k8s 杀 Pod 不给进程清理机会；
3. `HYDRA_NODE_ID` 未配置 ⇒ 身份回落到 `node-<随机 hex>`，每次重启都是新面孔 ⇒ HSET 覆盖不了旧行，只能新增。

**修复（`e909f28` + `317377a`）**：
- 注册表值升级为 `v2|role|control_url|last_seen`，新增 `sweep_stale(grace)`：仅当"心跳不存在 **且** `last_seen` 超过 grace（默认 120s）"才回收；历史两段式行直接回收；`saturating_sub` 保证时钟超前的节点不被误杀；批量 HDEL。
- 心跳收敛成一个入口 `register(ttl)` = 注册**或**续期（重写行 + 续 TTL，每 20s 一次，TTL 30s）⇒ 被误回收的活节点一个 tick 内自愈，这是敢设 120s grace 的前提。回收任务 60s 一轮、每个集群节点都跑（判据来自共享 Redis、幂等；leader-only 会在生产出现过的 leaderless 窗口里停摆）。
- `v2` 前缀是给**滚动升级**的：旧版本读者读到 `role="v2"` 会忽略该行，而不是把时间戳粘到 control URL 上把轮询目标写坏。
- 身份优先级改为 `HYDRA_NODE_ID` → `HOSTNAME`（K8s 即 Pod 名）→ 随机；回收后 `node_id` 直接是 Pod 名，能和 `kubectl get pods` 对上。
- 优雅停机后 best-effort `unregister()`；新增 `HYDRA_REGISTRY_STALE_GRACE_SECS`；指标 `hydra_registry_nodes{state}` / `hydra_registry_reaped_total`；告警 `HydraRegistryStaleRows` / `HydraRegistryChurn`。

**上线实测（`6d04e83d…`）**：113 行/4 活/108 离线 → 首轮回收后 **4 行全活、0 离线**；另做真机验证：删一个 edge Pod → 5 行（1 离线，grace 窗口内）→ ~2.5 分钟后被回收回 4 行。

> **关联文档**：`bug-2026-09-16-stale-registry-nodes.md`（含 §8 上线记录）。

---

## 4. 主线 D：管理端 UI（17:09–17:32）

### 4.1 保存报错但实际生效：`invalidateFK is not defined`（`bccccb2`）
**根因**：`8d6b97c`（删已退役的显示明文开关那次）删掉了 `function invalidateFK(kind) { delete FK[kind]; }`，而两处调用点（`submit()` 与 `doDelete()`）留了下来。任何配了 `clearsFK` 的资源（**providers、tenants**）在保存/删除时都会抛 `ReferenceError` —— 而且是在 `await writeAndReload(...)` **之后**才抛，于是：**写入已落库（standby 上已转发给 leader 并生效），UI 却弹"更新/删除失败"、弹窗不关、按钮卡 loading、列表不刷新**。
**澄清**：与 leader/standby **无关**（在 leader 上保存 provider 同样报错），因此"redirect 到 leader"治不了这个问题。
**修复与防回归**：补回该函数；新增 e2e `T2.2b`（UI 编辑 + 删除 provider，覆盖两条 `clearsFK` 路径）；**CI 新增 `ui-e2e` job**（真实二进制 + Chromium 跑整套 Playwright）—— 此前 CI 完全没有 UI 用例，这类回归才会漏到生产。
**验证**：本地整套 12 passed；**反证**：再把定义删掉重建二进制 ⇒ `T2.2`/`T2.2b` 双双失败（失败点正是"modal 仍可见"，与生产现象一致）；生产上线后把 `T2.2b` 直接对生产 admin UI 跑 → passed。

### 4.2 刷新浏览器就掉登录 + 401 半登录（`40f5912`）
**根因**：token 只存在内存变量（`let TOKEN = null`），且 `DOMContentLoaded` 无条件 `showLogin()`（还会清空输入框）；全仓没有任何一处把 token 写进 storage/cookie。旁证：`sessionStorage["hydra-admin-ok"]` **只在签出时被 remove、从没被 set**，是早期"记住登录"设计留下的死键。
**修复（方案 A + 401）**：
- token 存 `sessionStorage["hydra-admin-token"]`：刷新/同标签导航保持登录，关标签即失效，按标签页隔离。**故意不用 localStorage**（admin token 是全舰队根凭证，而 admin 面目前是明文 HTTP NodePort；"关浏览器也保持登录"需先上 HTTPS）。
- 启动 `restoreSession()`：先显示登录框再自动校验；存票被轮换时失败即停在登录框并显示 401（不再进入"处处 401"的半登录界面）；手动输错不会误删已有会话。
- `api()` 统一处理 401：清存票 + 回登录框 + 一次"会话已失效"提示（新增 i18n 键，en/zh/fr/de）。
**验证**：新增 `T2.1c`（刷新保持登录）/`T2.1d`（签出后刷新不复活 + 过期存票 fail-closed 并清键）；`lang.spec` 同步改为"reload 后不应再弹登录框"（旧断言写死了内存态行为）。本地 12 passed；**生产实测** `T2.1c`/`T2.1d` 均 passed（不写任何配置）。

> **关联文档**：`dev-docs/reviews/slices/R9-admin-ui.md`（I8/I9 的历史判定）。

---

## 5. 生产现状快照（当天收尾）

| 项 | 值 |
|----|----|
| 镜像 | `gpu/dogress2@sha256:94d54df3…`（commit `40f5912`） |
| 副本 | `hydra-control-0`（1/1，leader）/ `hydra-control-1`（0/1，standby，设计如此）/ `hydra-edge-659b99cf9b-*` ×2 |
| 节点注册表 | 4 行全活（`node_id` = Pod 名；滚动产生的旧行在 grace 后自动回收） |
| 数据面 | `https://api-test.do.top/v1/models` → **200**（h2）；`http://…` → **200** |
| TLS | `hydra_proxy_listener_tls=1`、`hydra_proxy_tenant_certs=1`、证书链 `*.do.top` → GeoTrust G2 → DigiCert Global Root G2（verify ok） |
| 控制面 | edge `hydra_control_snapshot_version` 正常推进；standby 管理写入经转发返回 200（`HYDRA_CLUSTER_PEERS` 已配） |
| 上游 | **`DeepSeek-V4-Flash`（A100-1）仍会挂住**（连上后不回包，网关 300s 超时；偶发 404），`Qwen3.8-27B` 正常（非流式 ~0.8s、流式首字 ~0.1s）—— 属 GPUStack 侧问题，非网关 |

---

## 6. CI 与测试基础设施（横切）

- **新增 `ui-e2e` job**：构建 `hydra`（`--features server`）→ 起实例（含 `HYDRA_ENCRYPTION_KEY`）→ `seed.sh` → Playwright 跑 `tests/e2e/`。此前 CI 只有 fmt / 迁移守卫 / clippy ×2 / build / core / server / cluster 测试，**没有任何 UI 用例**。
- **新增/更新的用例**：`tests/tls.rs` T6.5/T6.6、`tests/boot_listeners.rs`、`tests/redis_real.rs::real_redis_cold_start_elects_one_leader_without_a_producer`、`tests/fidelity_disabled_rows.rs`、`tests/provider_key_fidelity.rs`、注册表 `sweep_stale` 系列单测、e2e `T2.1c/T2.1d/T2.2b`。
- **当天最后一次门禁实测**：`fmt` 干净；`clippy` workspace 与 `-p hydra-server --features server,cluster-redis,usage-clickhouse --all-targets -D warnings` 均 0 error；`hydra-core` 150 passed；`--features server,cluster-redis` 388 passed；CI 同款命令（+真实 Redis）**402 passed / 0 failed**；e2e **12 passed**。
- **注意**：`--all-features` 在本工作区**本来就不编译**（`tls-boringssl` 与 `tls-openssl` 同开会让 `pingora-core` 重复导入 `ssl_lib`，E0252）——门禁要用 CI 里的显式特性列表。

---

## 7. 未完成 / 待决策

| # | 事项 | 现状 | 建议 |
|---|------|------|------|
| 1 | `DeepSeek-V4-Flash` 在 A100-1 挂住 | 网关侧 300s 才放弃；探针 1.5s 拿到 401 因此熔断不判死 | GPUStack 侧查实例；网关侧可选：把 `FORWARD`/上游超时改成"首字节/单次尝试"超时，并让熔断探测走真实推理路径 |
| 2 | standby 转发的 5s 超时（`forward.rs` `FORWARD_TIMEOUT`）语义 | 超时返回 502 `forward_failed`（"leader 不可达"），但写可能已经落地 | 提为可配 + 返回"结果未知"专用码；这是"报错但已生效"的**唯一 standby 独有**机制（审计 R4/M5） |
| 3 | 写后置步骤失败仍返回错误（`apply_tenant_access_token_write` 400 / `reload_after_write` 500，行已提交） | 与是否 leader 无关 | 把 token/证书校验移到写库前；`reload_failed` 与"写入是否成功"在响应里分开 |
| 4 | K8s `terminationGracePeriodSeconds`(30s) vs Pingora 排空(300s) | 进程在排空完成前被 SIGKILL ⇒ `unregister()` 与 usage sink 收尾 flush 都到不了 | 对齐两个超时（edge 抬长以容纳流式请求，排空上限压到其下） |
| 5 | `/admin` 自动 redirect 到 leader | 已评估：外部 NodePort 只选 Ready（=leader），需要 redirect 的只有 port-forward/集群内入口；且它**治不了** 4.1 的 UI 错误 | 优先级低；更省事的替代是非 leader 页面加横幅 + "跳到 leader"链接 |
| 6 | 管理端"记住我"（localStorage）/ 服务端会话 + HttpOnly Cookie | 方案 A 已落地；B/C 未做 | 先把 admin 面放上 HTTPS/仅内网再考虑 |
| 7 | 快照不含租户 access-token 哈希 | standby 的 `has_access_token` 恒为 false；`POST /tenants/{id}/auth/cache/invalidate` 在 standby 上因本机无哈希而 401 | 评估把哈希纳入快照（或让该端点也走转发） |

---

## 8. 回滚与风险提示

1. **回滚监听器修复的镜像前必须先清空租户证书**：旧镜像"有证书就把唯一监听器切成 TLS"，回滚会立刻让明文入口全挂。
2. **注册表 v2 值格式**：旧读者会把新行当作未知 role 忽略（安全降级）；新读者两种格式都能读。混合版本期间 `list_nodes` 可能短暂显示陌生 role。
3. **会话持久化的边界**：token 现在会在浏览器标签页内驻留（`sessionStorage`）。admin 面依然是**明文 HTTP**、且 token 是根凭证 —— 暴露面没有变化，但"把它放到 HTTPS 后面"应尽快排期。
4. **回收器不是"删库"**：判据是"心跳不存在 + `last_seen` 超过 grace"，且每个节点都会把自己重新注册；生产已验证三次（滚动后旧行消失、被杀 Pod 的行 2.5 分钟后消失）。

---

## 附录 A：当天提交清单（27）

```
17:32  40f5912  feat(ui): 管理端会话放进 sessionStorage —— 刷新不再掉登录 + 401 统一退回登录页
17:09  bccccb2  fix(ui): 补回被误删的 invalidateFK —— 修掉 providers/tenants 保存报错但已生效
16:13  c638ec5  docs(ops): 节点注册表回收上线生产 —— 113 行/108 离线 收敛到 4 行全活
15:49  317377a  fix(cluster): 注册表值加 v2 前缀 —— 让新旧混跑时旧读者忽略该行而不是写坏轮询目标
15:46  e909f28  fix(cluster): 回收节点注册表里的陈旧行 + 节点身份改用 HOSTNAME
14:59  85601c7  docs(ops): h2 修复上线生产 + 记录一次控制面事故与恢复（leaderless → OrderedReady 死锁）
14:46  106d6f6  docs(ops): 生产端口映射表 —— 80 与 443 各映射到哪个 NodePort
14:46  7eb3093  fix(proxy): HTTP/2 的 authority 也要参与租户解析 —— 修掉 TLS 上所有 h2 请求 404
13:47  a6a39a1  docs(ops): 生产已落地数据面探针 + 记录打补丁时的命令替换坑
13:41  f4fc588  docs(ops): 生产上线记录 —— 先滚镜像再写证书，4/4 pod 换新、证书回填、HTTPS 在 :30443 验证通过
12:49  4aafe3d  docs(ops): 记录边节点证书修复的真机复现与前后对照验证
12:39  8cbc35d  fix(cluster): edge 收到快照后重新解析租户证书 —— 否则 edge 上的 HTTPS 永远握不上手
12:20  215abbb  docs(ops): 生产证书写入 runbook（fullchain 下发 + 验证 + 清空回滚）
12:19  7186676  docs(ops): 监听语义反转写进手册 + 修复计划与根因记录
12:19  e537e78  test(proxy): 双监听 / 启动装配 / 证书链回归
12:19  530ab21  fix(proxy): 监听协议不再由租户证书决定 —— 明文恒定 + 可选 TLS 监听
11:28  e865340  docs(ops): edge 数据面探针 + 僵死告警（配套 auth-cache 自死锁的运维加固）
11:25  35e1496  fix(auth): AuthCache::check 不再让 DashMap 守卫跨越 await —— 修掉分片写锁自死锁
11:21  8087ed7  docs(review): 第五十二轮 —— A3 落地（provider_key 副本保真 + 纳秒主键收敛）
11:17  7ce718f  fix(cluster): provider_key 副本保真 —— wire 携带行身份，不再重生成主键
11:00  12accda  fix(cluster): wire fidelity 补齐禁用行 —— 物化不再销毁禁用 limit_role / provider_key_binding
10:56  587f485  test(common): 每个测试进程独占一段端口带 —— 修掉跨进程端口撞车 flake
10:47  5faeddd  docs(ops): 上线自检补齐"恢复路径三项信号"+ 症状表补两条
10:46  fac2956  test(cluster): 冷启动必须能选出 leader —— 真实 Redis 回归用例
10:37  f0508f9  fix(cluster): 修复整体重启后的租约死锁 —— 可提升判据补上"租约无关"的一半
10:26  58fc66e  fix(cluster): 回归复核修正批次 —— fidelity 谓词对齐、闸门/分支补测、租约死状态与产端授权修复
09:59  110774c  fix(cluster): D3 代际语义补洞（A+B+C）
```

## 附录 B：生产镜像时间线（当天）

| 时间 | 镜像 | 对应提交 | 内容 |
|------|------|----------|------|
| ~13:40 | `…@sha256:f8d5e1b3…` | `530ab21`+ | 双监听修复上线（随后回填证书，HTTPS 在 `:30443` 验证通过） |
| ~14:5x | `…@sha256:49a90210…` | `106d6f6` | h2 修复；公网 HTTPS(h2) 由 404 转 200 |
| ~16:0x | `…@sha256:6d04e83d…` | `317377a` | 节点注册表回收；113 行 → 4 行全活 |
| ~17:1x | `…@sha256:ca92a191…` | `bccccb2` | 管理端 `invalidateFK` 修复 |
| ~17:3x | `…@sha256:94d54df3…` | `40f5912` | 管理端会话持久化 + 401（**当前**） |

> 生产镜像走 `:latest` + `Always` 拉取：每次 `git push` 触发构建机重建并推送，随后在生产 `rollout restart`（edge）/ 逐个 `delete pod`（control，`OrderedReady` 下必须按 ordinal 顺序）。**纯文档提交也会重建镜像**，但行为不变、无需再滚生产。

