# BUG：`AuthCache::check` 持 DashMap 读守卫跨 `await` ⇒ 分片写锁自死锁，单副本静默僵死

> 记录时间：2026-09-16（UTC）
> 状态：**未修复 —— 仓库 HEAD（`110774c`）仍然存在该缺陷**
> 影响版本：至少 `gpu/dogress2@sha256:4bbcc923…`（线上旧镜像，2026-09-16 03:01 之前）
> 发现环境：dogress 生产 k3s 集群（172.16.39.182 / .171 / .185，ns `hydra`）
> 代码定位：`crates/hydra-server/src/http.rs:170-191`（`AuthCache::check`）

---

## 0. 一句话结论

`AuthCache::check()` 在 **L1 未命中** 时先把 DashMap 的读守卫（`Ref`）绑成局部变量 `entry`，
随后在**仍然持有该守卫**的情况下 `await` 了 Redis L2，并在 L2 命中的分支里对**同一个 key** 执行
`self.map.insert(...)`。同一个 task 自己占着分片读锁、又去要同一分片的写锁 ⇒ 永久自死锁，
把 Pingora proxy 服务的**唯一工作线程**挂死，该副本 8080 端口从此不再 accept 任何连接，
进程却仍活着、`/healthz` `/readyz`（8081）依旧 200 ⇒ **k8s 永远不会重启它**。

---

## 1. 现象（线上实测）

| 观测项 | 僵死副本 | 健康副本 |
|---|---|---|
| 直连 pod IP `:8080`（任意路径/任意 Host） | 4–8s 无响应（`code=000`） | 1ms 内答（404） |
| `:8080` LISTEN 全连接队列 | 采样时 18（无人 accept） | 0 |
| `CLOSE_WAIT` 连接数 | 18（对端已关，进程没读没关） | 0 |
| `/metrics` 里 `hydra_request_duration_seconds_*` | **完全不存在**（一个请求都没走完） | 正常 |
| `:8081` `/healthz` `/readyz` | **200 / 200（探针认为它健康）** | 200 / 200 |
| Service endpoints | **仍被收录**（≈50% 流量被判给它） | 收录 |

- 对 `api-test.do.top` 连打 10 次：`404 000 000 000 404 000 000 404 404 404` ⇒ **5/10 请求 6s 无响应**。
- 该状态**不会自愈**：观测时已持续 ≥ 3h（最后一条 proxy 日志 `2026-09-15T23:59:54Z`，
  首次探测到僵死 `2026-09-16T02:16Z`）。
- 删除 pod 重建后立刻恢复（10/10 正常）。

## 2. 现场证据

### 2.1 线程状态（宿主 `/proc/<pid>/task`，pid=120705 = 容器 `hydra-edge-…-mk4tx`）

```
120724 Hydra admin API    ep_poll            ← admin 服务正常（独立线程）
120725 Pingora HTTP Pr    futex_wait_queue   ← 唯一 proxy worker，卡在锁上
120726 Server             ep_poll
120705 hydra(main)        futex_wait_queue   ← pingora run_forever 的正常 park
120707..120713 tokio-rt-worker  futex_wait_queue  ← 后台 runtime 的 worker，正常 park
```

### 2.2 用户态栈（gdb `thread apply all bt`，符号来自容器内 `/usr/local/bin/hydra`）

```
Thread 4 (LWP 120725 "Pingora HTTP Pr"):
#0  syscall ()
#1  <dashmap::lock::RawRwLock>::lock_exclusive_slow ()
#2  <dashmap::DashMap<(String, [u8; 32]), hydra_core::auth::AuthEntry>>::insert ()
#3  <hydra_server::http::HttpAuthChecker as hydra_server::auth::AuthChecker>::check::{closure#0} ()
#4  <hydra_server::proxy::HydraProxy as pingora_proxy::proxy_trait::ProxyHttp>::request_filter::{closure#0} ()
#5  <pingora_proxy::HttpProxy<HydraProxy>>::process_request::{closure#0} ()
#6  ... <pingora_core::services::listening::Service<HttpProxy<HydraProxy>>>::run_endpoint::{closure#0}
#11 <tokio::runtime::scheduler::multi_thread::worker::Context>::run_task ()
```

栈本身就是结论：**唯一一次 `DashMap::insert` 出现在 `check` 内，就是第 180 行的 L2 回填**
（见下），而它阻塞在 `lock_exclusive_slow`（等写锁）。

> 取证方法备注：宿主机 gdb 直接 attach 时 `/proc/<pid>/exe` 跨 mount namespace 读不到（EIO），
> 需 `set sysroot /proc/<pid>/root`，并把容器内二进制拷出后用
> `add-symbol-file <binary> <ELF基址+.text偏移>` 手工加载符号。完整栈见
> `ru_deployer/.tmp/evidence/mk4tx-backtraces-20260916.txt`。

## 3. 根因

```rust
// crates/hydra-server/src/http.rs（HEAD 现状）
pub async fn check(&self, tenant_id: &str, api_key: &str) -> Verdict {
    let hash = sha256_hex(api_key.as_bytes());
    let entry = self.map.get(&(tenant_id.to_string(), hash));   // ① 拿分片读守卫 Ref（活到函数结束）
    if let Verdict::Hit(_) = cache_decision(entry.as_deref(), (self.now)()) {
        return cache_decision(entry.as_deref(), (self.now)());
    }
    #[cfg(feature = "cluster-redis")]
    if let Some(l2) = &self.l2 {
        if let Ok(Some((allowed, ttl))) = l2.get(tenant_id, &hex_digest(&hash)).await {  // ② 持守卫 await
            let expires_at = (self.now)() + ttl;
            self.map.insert(                                       // ③ 同一分片要写锁 ⇒ 自等自
                (tenant_id.to_string(), hash),
                AuthEntry { allowed, expires_at },
            );
            return Verdict::Hit(allowed);
        }
    }
    Verdict::Miss
}
```

- `DashMap::get()` 返回的 `Ref` 是**分片级读锁守卫**；`insert()` 走 `lock_exclusive_slow`，
  必须等该分片**所有**读守卫释放。
- ① 的 `entry` 绑成了具名变量，生命周期到函数结束；②处 `.await` 期间它仍然活着；
  ③要的还是**同一个 key → 同一个分片** ⇒ 该 task 自己持读锁、又申请写锁，**永远等不到**。
- 这是"同步锁跨 await"在少线程 runtime 上的经典表现：任务永不完成，
  承载它的 worker 线程 park 在 futex 上。
- 旁证：`check` 里唯一属于它自己的 `insert` 就是 L2 回填那处，所以**栈能出现 `insert`
  就证明部署构建启用了 `cluster-redis` 特性并挂上了 L2**
  （`HYDRA_REDIS_MODE=single` + `HYDRA_REDIS_URL` 指向 172.16.39.142:6379）。

### 触发条件

1. L1 未命中（首次请求该 key，或 L1 条目已过 TTL / 被 GC / 被失效）；
2. Redis L2 命中（L2 的 TTL 通常长于 L1，或本节点刚重启、刚被 invalidate）；
3. 即：**"L1 冷、L2 热"**。这也是它为什么不是一上线就炸、而是运行一段时间后偶发。

### 影响面为什么被放大

- Pingora 每个 service 只有一个 worker 线程 → 该线程一死，**整个 8080 数据面停摆**，
  而 8081 的 admin 服务是另一线程，所以 `/healthz` `/readyz` 全部继续 200；
- 部署里的 `readinessProbe` / `livenessProbe` **都只探 8081**，因此 k8s 判定副本健康、
  继续把它留在 Service endpoints 里 ⇒ **约一半请求静默挂死**（对端只会看到连接超时）；
- 没有自愈路径，只能重启 pod。

## 4. 修复方案

### 4.1 最小改动（推荐）

```rust
pub async fn check(&self, tenant_id: &str, api_key: &str) -> Verdict {
    let hash = sha256_hex(api_key.as_bytes());
    let key = (tenant_id.to_string(), hash);

    // (1) L1：取值后立刻释放守卫 —— 任何守卫都不得跨越 await
    let l1 = {
        let entry = self.map.get(&key);
        cache_decision(entry.as_deref(), (self.now)())   // 纯计算，不 await
    };                                                    // ← Ref 在此 drop
    if let Verdict::Hit(v) = l1 {
        return v;                                        // Verdict 是 Copy
    }

    // (2) L2 回填：此处已无任何守卫
    #[cfg(feature = "cluster-redis")]
    if let Some(l2) = &self.l2 {
        if let Ok(Some((allowed, ttl))) = l2.get(tenant_id, &hex_digest(&hash)).await {
            let expires_at = (self.now)() + ttl;
            self.map.insert(key, AuthEntry { allowed, expires_at });
            return Verdict::Hit(allowed);
        }
    }
    Verdict::Miss
}
```

要点：
- 把 L1 判定放进一个 `{ }` 作用域，或显式 `drop(entry)` **再**进入 L2 分支；
- 顺手把 `cache_decision` 的重复调用（原代码调了两次）收敛成一次；
- `Verdict` 已 `Copy`（`crates/hydra-core/src/auth.rs:41`），无克隆开销。

### 4.2 回归测试（建议随修复一起入库）

```rust
#[tokio::test]
async fn l2_hit_does_not_deadlock_when_hydrating_l1() {
    // 造一个"L2 命中、L1 为空"的场景，直接检验 check 能在超时内返回。
    let cache = AuthCache::new(allow_ttl, deny_ttl).with_l2(l2_hit_mock());
    let v = tokio::time::timeout(Duration::from_secs(1), cache.check("1", "k"))
        .await
        .expect("check() deadlocked holding a DashMap guard across the L2 await");
    assert_eq!(v, Verdict::Hit(true));
}
```

要点：**必须带 `timeout`**。死锁的表现是"任务永不返回"，没有超时的测试只会把 CI 挂住。
建议同时覆盖 `Hit(false)`（deny 回填）与"同 key 并发 `check` + `set`"两个变体。

### 4.3 防御性建议（可选，但值得）

- 给 auth cache 增加一个**纯同步**的取值接口（如 `fn peek(&self, key) -> Verdict`，
  内部自带作用域），热路径只调它，从 API 形状上杜绝"守卫逃逸到 await 之后"；
- 在 review checklist / lint 里加一条：**任何 `DashMap::get/entry` 的返回值都不得跨越 `.await`**
  （`clippy::await_holding_lock` 只覆盖 std/parking_lot 锁，覆盖不到 DashMap 的 `Ref`）。

## 5. 同类风险扫描结论（本次已核查）

| 位置 | 结论 |
|---|---|
| `http.rs` `AuthCache::set` (195-229) | `insert` 是同步语句，其守卫在语句末释放；随后的 `l2.set().await` 不再持守卫 ✅ |
| `http.rs` `invalidate` (233-246) | `remove` 同步、随即释放；`l2.del().await` 不持守卫 ✅ |
| `http.rs` `invalidate_tenant` (250-258) | `retain` 同步完成后才 `l2.del_tenant().await` ✅ |
| `proxy/limiter.rs` `check_and_inc` / `add_tokens` | `.entry(key).or_insert_with(..)` 守卫是表达式临时值，整条语句同步完成 ✅ |
| `proxy/admission.rs` `get_or_create_gate` | `get` 后立即 `Arc::clone` 返回，同步函数、无 await ✅ |
| `proxy/breaker_wrap.rs` | `fails` / `dead` 的读写均为同步函数；集群投票走 spawn ✅ |

⇒ **当前仅 `AuthCache::check` 一处**。但同类模式很容易在新代码里复发，建议保留 4.3 的检查项。

## 6. 运维侧加固（与代码修复同等重要）

本次之所以"静默吃掉一半流量"，是**探针覆盖不到数据面**：

1. 给 edge 增加**作用于 8080 的 readiness/liveness**（例如 httpGet `/` 期望 404，
   或新增一个专用的 proxy 健康端点），使"proxy 线程卡死"能被 kubelet 发现并重启；
2. 告警建议：
   - `hydra_request_duration_seconds_*` 在该副本**停止增长**（或 `hydra_auth_cache_size` 冻结）
     而 admin `/healthz` 仍 200 ⇒ 典型的僵死信号；
   - 连接层：该 pod 上 `CLOSE_WAIT` 持续 >0 且 `:8080` LISTEN 队列堆积。

## 7. 复现与验证步骤（供修复后回归）

1. 造"L1 冷 / L2 热"：清掉 L1（`DELETE` auth cache）后用同一把 key 打一次请求让该 key 写入 L2，
   再清一次 L1，然后打第二次；
2. 观察该 edge 副本：若 `:8080` 不再应答且 `/proc/<pid>/task/*/wchan` 出现
   `Pingora HTTP Pr → futex_wait_queue`，即命中本 bug；
3. 修复后同样步骤应正常返回，且 `hydra_request_duration_seconds_*` 持续增长。

## 8. 时间线（UTC）

| 时间 | 事件 |
|---|---|
| 2026-09-15 06:53:47 | 僵死副本启动（旧镜像 `4bbcc923…`） |
| 2026-09-15 07:39–07:40 | 控制面租约/快照抖动（该轮已由 09-16 的集群修复覆盖） |
| 2026-09-15 23:59:54 | 该副本最后一条 proxy 日志（此后静默） |
| 2026-09-16 02:16 | 首次探测到 8080 完全无响应 |
| 2026-09-16 03:12–03:15 | 线程/栈取证，确认 `lock_exclusive_slow` 自死锁 |
| 2026-09-16 03:14 | 删除僵死 pod 重建，挂起率 5/10 → 0/10 |

## 9. 与本次版本升级的关系（重要）

- 本缺陷**不在** `dev-docs/ops-upgrade-2026-09-16.md` 覆盖的修复范围内，HEAD 未修；
  即"发布一个含本修复的新镜像"是**额外**的一次构建。
- 该升级文档 §1.2 明确**不支持新旧混跑**（新节点拒绝无 `epoch` 的旧快照）。
  因此含本修复的镜像上线时，必须**一次性**把 `hydra-control` / `hydra-edge` 全部节点
  换到同一版本（先起 leader 候选，再起 edge），不能逐个滚动。
