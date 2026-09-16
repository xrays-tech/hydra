# BUG：租户证书把**唯一的**监听器从明文切成 TLS —— 明文入口（80）在下一次重启后 100% 失效

> 记录时间：2026-09-16（UTC）
> 现象环境：dogress 生产 k3s（172.16.39.182 / .171 / .185，ns `hydra`）
> 代码定位：`crates/hydra-server/src/main.rs:755-805`（`run_server` 的 (3b) 段）
> 设计对照：`dev-docs/design.md` 架构图（ProxyService 应为 `:443 (TLS, SNI)` + `:80 (HTTP, dev)` **两个监听**）
> 状态：**已修复**（监听器拓扑改为**只由配置决定**：明文 `HYDRA_LISTEN` 恒定绑定、`HYDRA_TLS_LISTEN` 才新增 TLS 监听；见提交 `a3eb73b` 与 `dev-docs/ops.md` §9.1 的 `hydra_listener_*` 指标。**本文档记录的是事故当时的分析，状态行已过期故更正。**）

---

## 0. 一句话结论

ProxyService 只绑**一个** downstream 监听器，协议由"快照里是否存在租户证书"二选一：

```rust
let listen_addr = env("HYDRA_LISTEN").unwrap_or("0.0.0.0:8080");
if snapshot.certs.is_empty() {
    proxy_service.add_tcp(&listen_addr);                     // 明文
} else {
    proxy_service.add_tls_with_settings(&listen_addr, None, settings); // TLS(SNI)
}
```

于是"**给租户配一张证书**"这个纯配置动作，等价于"把数据面协议静默切成 TLS"。
而现网入口只映射了 **80（明文）** ⇒ **任何一次重启**（升级、驱逐、崩溃恢复）都会让该副本对明文请求直接 RST：
接口全挂、进程却健康、`/healthz` `/readyz` 全绿 —— 属于"升级/重启即炸"的地雷，**不是概率问题**。

---

## 1. 本次生产实录（症状与证据）

### 1.1 时间线

| 时间（UTC） | 事件 |
|---|---|
| 09-15 06:53 | 旧版 edge 副本启动。此时**还没有租户**（租户 07:04 才创建）→ 绑定**明文**监听 |
| 09-15 07:04 | 新建租户 `api`（domain `api-test.do.top`）并写入 `*.do.top` 证书（`tenant.cert_pem`） |
| 09-15 07:35 / 07:39 | 控制节点（旧版）重启 → 它们**当时就是 TLS 监听**；唯一的明文副本仍是那个 06:53 的老 pod |
| 09-16 03:31–03:37 | 本次升级把所有 pod 换成新镜像 → 新 edge 读到快照里有证书 → **全部绑 TLS** |
| 09-16 03:33 | 公网明文请求全部 `Empty reply from server`；直连 pod 8080 在 1–3ms 内 RST（`code=000`） |
| 09-16 03:35 | 清空租户证书（`cert_pem: ""`）→ 重启 edge → 恢复 `plain-TCP listener bound` → 数据面恢复 |

> 关键点：**问题一直被"启动顺序"掩盖**。只要那个在租户创建之前启动的老副本不重启，明文就一直能跑；
> 一旦重启（本次升级就是），协议立刻翻转。

### 1.2 直接证据

- 同一二进制、同一配置，仅因快照里有无证书而打印不同日志：
  - 有证书：`INFO hydra: proxy TLS listener bound (per-tenant SNI cert callback) listen=0.0.0.0:8080`
  - 无证书：`INFO hydra: proxy plain-TCP listener bound (no tenant certs configured) listen=0.0.0.0:8080`
- 崩坏时的探针盲区（放大影响面）：
  - `:8080` 明文请求 → 1–3ms `code=000`（RST）
  - `:8081` `/healthz`、`/readyz` → **200**（探针只覆盖 admin 端口，k8s 判定副本健康、继续留在 Service endpoints 里）
- 入口侧：`curl -v http://api-test.do.top/...` → `Empty reply from server`；`https://api-test.do.top`（443）→ 连接失败（现网只映射了 80）。

### 1.3 影响面

- 单副本：该副本 100% 明文请求失效（不是部分失败）。
- 全副本：**所有副本一起重启 = 数据面 100% 不可用**（本次即如此，公网所有请求失败）。
- 隐蔽性：进程不崩、日志不报错、探针全绿、指标正常（`hydra_request_duration_seconds_*` 只是不再增长），
  排查者很容易误判为"升级把服务搞坏了"。

---

## 2. 根因分析

### 2.1 实现只有一个监听器，且协议随数据隐式翻转

- `HYDRA_LISTEN` 默认 `0.0.0.0:8080`（`main.rs:54`），全进程**仅此一个** downstream 地址；
- 没有任何"强制明文 / 强制 TLS"开关：运维文档已注明 `HYDRA_EDGE_TLS` **代码从不读取**；
- 协议选择的输入是**业务数据**（租户是否配了证书），而不是**部署配置** ⇒ 一次租户配置变更会改变整个数据面的传输层行为，且重启才生效（最隐蔽的生效时机）。

### 2.2 与设计不符：设计本来要的是双监听

`dev-docs/design.md` 的架构图（ProxyService 一节）：

```
│  │  ProxyService       │
│  │  :443 (TLS, SNI)    │   ← 生产：按 SNI 选租户证书
│  │  :80  (HTTP, dev)   │   ← dev/明文
```

即：**设计的意图是"两个监听并存"**；实现只做了"一个端口二选一"。因此本缺陷可定性为
**实现与设计的偏离（design-conformance gap）**，而不是"设计如此、现网配错"。

### 2.3 为什么"现网只映射 80"是致命的组合

- 现网入口：`api-test.do.top` → `125.67.215.16:80` →（集群外 NAT，本次未定位到具体设备）→ 节点 `:30090`（`hydra-edge` NodePort）→ pod `:8080`；
- 443 未映射；且集群节点上的 `:80/:443` 已被 `svclb-traefik`（klipper-lb）接管，**不能**寄望"节点 80/443 直连 pod"；
- 于是"有证书 → 监听变 TLS"必然导致入口 100% 失配。

---

## 3. 提问回答：同时支持 80 与 443（raw HTTP + HTTPS）能不能解决？

**结论：方向正确，但"只把 80/443 在网络上同时开放"不够；必须让 hydra 同时具备两种监听，或把 TLS 终结挪到更外层。**

理由（按重要性）：

1. **单监听器与端口数量无关**：即使公网 80 与 443 都映射进来，只要都落到同一个 `:8080`，80 上的明文仍然会被 TLS 监听器 RST。协议由"证书是否存在"决定，多开端口不改变这一点。
2. **Pingora 本身支持一个 Service 挂多个监听**：`pingora-core 0.8.1` 的 `Listeners { stacks: Vec<TransportStackBuilder> }`，`add_tcp` / `add_tls_with_settings` 可重复调用并各自带独立传输栈。所以"明文 + TLS 并存"是**小改动**，不是架构改造。
3. **这正是 design.md 原本的形态**（§2.2），补上它属于"回归设计"，而不是新增能力。
4. **一旦双监听落地**：证书的存在不再影响明文通路 ⇒ 重启不再翻转协议 ⇒ 地雷消除，同时 HTTPS/SNI/per-tenant 证书能力保留。

因此答案是"**能解决，但解法是"hydra 双监听（或外层 TLS 终结）"，而不是"入口多开一个端口"**"。下面给出三条路径。

---

## 4. 解决路径

### 路径 A（推荐）：hydra 双监听 —— 明文常量保留 + 证书存在时额外挂 TLS

**代码改动（`main.rs` (3b) 段，示意）**

```rust
let listen_addr = env("HYDRA_LISTEN").unwrap_or("0.0.0.0:8080".into());
let tls_listen  = std::env::var("HYDRA_TLS_LISTEN").ok();     // 例: 0.0.0.0:8443

// 明文通路永远在（保持 80 入口可用，且行为与"有无证书"解耦）
proxy_service.add_tcp(&listen_addr);

// 有证书且配置了 TLS 端口时，额外挂一个 TLS 监听（SNI 选证能力不变）
if !snapshot.certs.is_empty() {
    match tls_listen {
        Some(addr) => { proxy_service.add_tls_with_settings(&addr, None, settings); }
        None => error!("tenants have certs but HYDRA_TLS_LISTEN is unset; HTTPS listener DISABLED"),
    }
}
```

要点：
- **明文与 TLS 决定性分离**：`snapshot.certs` 只影响"是否有 HTTPS 端口"，不再影响明文端口；
- 反例告警：有证书却没配 TLS 端口（以及反之）→ `error!` + 指标，禁止再出现"静默切协议"；
- 证书热更新路径不变（Admin 改证书 → `reload_all` → `HydraCertStore` 立即生效，design §12.1）。

**部署侧改动**

| 项 | 改动 |
|---|---|
| Deployment | 容器新增 `containerPort: 8443`；env `HYDRA_TLS_LISTEN=0.0.0.0:8443` |
| Service `hydra-edge` | 新增 `port 8443 → NodePort 30443`（30443 当前空闲） |
| 入口（集群外） | 把 `api-test.do.top:443` 映射到任一节点 `:30443`。⚠️ 节点 `:80/:443` 已被 traefik 的 klipper-lb 占用，**必须走独立 NodePort**；本次未能定位 `125.67.215.16:80 → 30090` 的映射设备，需要入口侧确认后再加 443 |
| 探针 | 给 **8080** 补 readiness/liveness（配套 `e865340` 的运维加固） |
| 文档 | `ops-upgrade-2026-09-16.md` 的配置表补 `HYDRA_LISTEN` / `HYDRA_TLS_LISTEN` |

**验收（含回归判据）**

1. 有证书时：`curl http://api-test.do.top/v1/models -H 'Authorization: Bearer <key>'` → 200；
2. 有证书时：`curl https://api-test.do.top/v1/models -H 'Authorization: Bearer <key>'` → 200（证书链可信；`*.do.top` 通配已含 `api-test.do.top`）；
3. **重启任意 edge 副本后，上述两条都仍可用** ← 这就是本次事故的回归判据；
4. SNI 与 Host 不一致时：按 `Host` 解析租户、按 SNI 选证书，仅记 `hydra_sni_host_mismatch_total`，不阻断（design §12.3）；
5. 无证书部署（单机/dev）：仍是纯明文，行为与今天一致。

**证书轮换提醒**：现网租户证书为 `*.do.top`（DigiCert / GeoTrust，SAN `*.do.top, do.top`），**2027-01-10 到期**；
轮换走 Admin API 写 `cert_pem` + `cert_key_pem`（内容模式，私钥入库加密），不再依赖文件路径（`HYDRA_CERT_DIR`）。

### 路径 B：TLS 终结放到外层（不改代码）

- Traefik / nginx 在 443 终结 TLS，明文回源 hydra `:8080`；80 可直接 301 到 443 或继续明文回源；
- **前提：租户不要再存证书**（存了就会把唯一监听器切成 TLS，又回到本 bug）；
- 优点：不动代码、可立刻做，证书由成熟组件管理；
- 缺点：**丢掉 per-tenant SNI 能力**（hydra 的多租户证书特性失效），且 "证书一存就炸" 的雷还在，
  只能靠路径 A 的告警兜底或加配置校验。

### 路径 C：维持明文（本次的止血方式，仅临时）

- 租户不配证书（本次已执行：`cert_pem: ""`），数据面恢复明文；
- 只能作为过渡：将来任何一次"给租户配证书"都会再次引爆，除非同时落地 A 或 B。

### 三路径对比

| | A 双监听（推荐） | B 外层终结 | C 维持明文 |
|---|---|---|---|
| 代码改动 | 小（十几行 + 1 个 env） | 无 | 无 |
| 入口侧配合 | 需要（443 → 30443） | 需要（配 Traefik/nginx 与证书） | 不需要 |
| 保留 per-tenant 证书/SNI | ✅ | ❌ | ❌（等于放弃） |
| 重启是否会翻转协议 | 不会（已解耦） | 不会 | 不会（但没证书能力） |
| 与 design.md 一致 | ✅ 回归设计 | 偏离（能力闲置） | 偏离 |
| 风险 | 低（明文通路不变，仅新增监听） | 中（依赖外层组件与证书运维） | 高（地雷保留） |

---

## 5. 无论选哪条路径都建议同时做

1. **显式告警/指标**：证书存在但无 TLS 监听端口（或反向）→ `error!` + 指标，杜绝静默切协议；
2. **edge 8080 数据面探针**（配套提交 `e865340`）——本次"进程健康、探针全绿、数据面全挂"的盲区；
3. **部署模板 `$(VAR)` 顺序**（本次另一独立故障，与本 bug 无关但同为"重启即炸"）：
   `POD_NAME`（`fieldRef`）必须定义在引用它的 `HYDRA_PUBLIC_URL` **之前**，否则 K8s 不展开、
   控制面会把字面量 `http://$(POD_NAME).hydra-control:8081` 注册成自己的对外地址，edge 轮询永久失败；
4. **入口拓扑入文档**：80/443 → 两个 NodePort 的映射关系写清楚，避免"只有 80"成为隐性约束；
5. **启动自检**：启动日志里明确打印"明文监听 = X（恒定）/ TLS 监听 = Y 或 disabled"，
   让每次重启都能一眼看出协议形态。

---

## 6. 复现步骤（回归用，5 分钟）

1. Admin API 给租户写入证书（`cert_pem` + `cert_key_pem`）；
2. 重启任意 edge 副本（`kubectl -n hydra rollout restart deploy/hydra-edge`）；
3. 观察该副本日志出现 `proxy TLS listener bound …`；对 pod `:8080` 发明文请求 → `code=000`（RST）；
   同时 `:8081/readyz` 仍 200（证明探针抓不到）；
4. 修复后（路径 A）：同样步骤下明文与 HTTPS 都仍正常，且日志同时出现**两条**监听绑定记录。

---

## 7. 附录：本次事故的处置记录

| 时间（UTC） | 动作 | 备注 |
|---|---|---|
| 03:31–03:32 | control 全量换镜像（scale 0 → 2，leader 候选先起） | 控制库 v4 校验值预检通过 |
| 03:35:20 | 清空租户证书（`cert_pem: ""`，保留 `access_token`） | 止血；证书材料备份见下 |
| 03:36 | 修 sts env 顺序 + 补 `HYDRA_CLUSTER_PEERS`，重启 control | 解决轮询永久失败 |
| 03:37 | 修 deploy env（`POD_NAME`/`POD_IP` 前置），rollout edge | 同时应用"无证书→明文监听" |
| 03:38 | 验收：4 pod 同镜像、快照 v24、poll 失败 0、明文监听、认证通过、模型目录正常 | 数据面恢复 |
| 03:39 | 定位剩余失败为**上游** GPUStack 返回 `Model not found or no running instances available` | 与本次升级无关 |

备份（可复现证书恢复）：
- `ru_deployer/.tmp/hydra-control-0-after.db`（含 `cert_pem` 与私钥密文/nonce/version）
- `/tmp/tenant-cert.pem`（证书 PEM）
- `ru_deployer/.tmp/hydra-control-0-preupgrade.db`（升级前控制库）

回滚"清空证书"：用 Admin API 重新写入 `cert_pem` + `cert_key_pem`（私钥明文需从原证书文件取；
若已无明文私钥，可用上述 DB 备份中的密文 + `HYDRA_ENCRYPTION_KEY` 重新封装）。
