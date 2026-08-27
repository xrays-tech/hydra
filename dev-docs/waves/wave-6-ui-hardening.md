# Wave 6 — UI、TLS 端到端与加固（UI, TLS & Hardening）

> crate：both ｜ 估时：2d ｜ 收官波次（依赖 W1–W5）
>
> 关键纪律：Web 应用必须经 **Playwright** 端到端验证（AGENTS.md）；压测/优雅升级用真实进程；无内部 mock。

---

## 1. 目标与范围

### In-scope
- 内嵌轻量 UI：`admin-ui/{index.html,app.js,style.css}` + `include_dir` 打包进二进制；
- UI 页面分区覆盖：Providers / Models / Keys / Tenants(含 auth_url) / TenantAccess / TenantModels / LimitRoles / AuthCache(失效) / Breaker(查看+复位) / Health；
- 多租户 TLS **端到端**（W4 已做选证书单测，本波做多域名真实证书全链路）；
- Playwright E2E（UI CRUD 流程）；
- 压测（真实上游 mock，验证 SWRR 分布、限流、熔断、SSE 并发）；
- 优雅零停机升级（SIGQUIT + `-u`）验证；
- ops 文档：部署、配置、`retry_after_connect` 计费风险说明、熔断探活策略、排障。

### Out-of-scope
- 新功能开发（全部功能已在 W1–W5 交付）；
- 多实例 / Redis 限流（v2，design §16.6）。

### 依赖与前置
- W1–W5 全部就绪（可独立运行的完整服务）。

---

## 2. TDD / 验证任务列表

### 2.1 内嵌 UI（0.5d）—— design §14
- T1.1 `ui_served_from_admin`：`GET /admin/` 返回 index.html（200，Content-Type text/html）。
- T1.2 `ui_assets_embedded`：`include_dir!` 宏编译期打包；二进制无外部文件依赖（断言资源在二进制内）。
- T1.3 `ui_cors_same_origin`：UI 与 `/api` 同源，`fetch('/api/v1/...')` 可达。
- T1.4 `ui_no_build_step`：源文件为纯静态 HTML/JS/CSS，无 npm/构建（CI 校验 `admin-ui/` 无 `package.json`）。

### 2.2 Playwright E2E（0.7d）—— design §19.3 / AGENTS.md
> 起完整服务（真实 Pingora + `:memory:` 或临时 SQLite + mock upstream + mock auth）+ Playwright 驱动浏览器打 `/admin/`。
- T2.1 `e2e_login_with_admin_token`：输入 Token → 进入管理界面。
- T2.2 `e2e_create_provider_flow`：UI 新建 Provider → 列表出现 → DB 落库 → `ConfigStore` 热更新（随后 proxy 可路由到它）。
- T2.3 `e2e_tenant_with_auth_url`：新建租户（含 auth_url）→ 关联 provider/model → proxy 请求走通真实 mock auth。
- T2.4 `e2e_invalidate_auth_cache`：UI 触发失效某 key → 随后该 key 请求重新回源（mock auth `expect` +1）。
- T2.5 `e2e_breaker_reset`：UI 复位 dead provider → 候选重新包含。
- T2.6 `e2e_limit_role_effect`：配置限流 → UI 触发多次请求 → 超限请求 429 在界面可见（或日志/指标）。
- T2.7 `e2e_streaming_response_displayed`：（可选）UI 触发一次流式测试请求，验证 SSE 渐进显示。

### 2.3 多租户 TLS 端到端（0.3d）—— design §12
- T3.1 `tls_multi_domain_end_to_end`：两个租户域名 + 两套真实自签证书，curl 分别访问各自域名拿到对应证书（验证完整握手 + 路由 + 认证链路）。
- T3.2 `tls_hot_reload_live`：运行中替换某租户证书 + `reload_all` → 新连接用新证书，旧连接不受影响。
- T3.3 `tls_wildcard_or_default_fallback`：未知 SNI 回落 default cert（若配置）。

### 2.4 压测（0.3d）
> 用真实 mock upstream（多实例模拟多 provider）+ 压测客户端（如 `oha`/`wrk` + 自写 SSE 客户端）。
- T4.1 `load_swrr_distribution`：权重 3:1 持续请求，实测分布 ≈ 6:2（验证 W1 SWRR 在真实并发下成立）。
- T4.2 `load_breaker_under_failure`：注入一个上游持续失败 → 熔断后流量自动避开，恢复后回流。
- T4.3 `load_sse_concurrency`：并发 SSE 流，验证无串行化瓶颈、内存稳定（AuthCache/限流窗口 GC 生效）。
- T4.4 `load_baseline_rps`：记录基线 RPS / P99 延迟作为后续回归参考（写入 ops 文档）。

### 2.5 优雅升级（0.2d）—— design §15.3
- T5.1 `upgrade_zero_downtime`：运行中 `kill -SIGQUIT` + 新进程 `-u` → 进行中的请求不中断，新连接由新进程接管。
- T5.2 `upgrade_socket_handover`：监听 socket 正确移交（无端口占用错误）。

### 2.6 加固与 ops 文档（0.3d）—— design §16/§15
- T6.1 `no_unwrap_in_server_release`：`rg 'unwrap\(\)|expect\(|panic!|unimplemented!|todo!' crates/hydra-server/src` 在**生产代码**为空（测试代码豁免）。
- T6.2 `secrets_not_in_config_file`：`hydra.toml` 模板无明文密钥（Token 走 env）；DB 文件权限 0600 文档化。
- T6.3 `ops_doc_retry_billing_warning`：ops 文档显著标注 `retry_after_connect=true` 的重复计费风险（design §8.3）。
- T6.4 `ops_doc_breaker_probe_strategy`：探活端点选择、降级 TCP、`status=-1` 语义说明。
- T6.5 `ops_doc_graceful_ops`：部署、升级、证书更新、限流调整、缓存失效操作 runbook。

---

## 3. 外部边界与测试方式
- E2E：真实服务进程 + Playwright 真实浏览器 + wiremock（mock upstream / mock auth）。
- TLS：真实自签证书 + curl/openssl 真实握手。
- 压测：真实进程 + 真实网络。
- **无内部 mock**。

---

## 4. 与 design.md 的映射
§14（UI）、§12（TLS）、§15（部署/升级）、§16（安全/ops）、§19.3（E2E 策略）。

---

## 5. 出口准则（= 全项目交付门槛）
- [ ] Playwright E2E 全绿（AGENTS.md 强制：web 应用必经浏览器验证）；
- [ ] 多租户 TLS 端到端 + 热更新验证；
- [ ] 压测基线记录入 ops 文档，无内存泄漏/性能塌陷；
- [ ] 优雅升级验证通过；
- [ ] 生产代码（两 crate）grep 无 `unwrap/expect/panic/unimplemented/todo`、无 mock/stub/`#[cfg(test)]` 分支；
- [ ] ops 文档完整（部署/升级/证书/限流/缓存失效/计费风险/熔断探活/排障）；
- [ ] `cargo build --all --release` 产出单二进制，`hydra.toml` + `data/` 即可部署。

---

## 6. 风险与注意
- **Playwright 环境**：CI 需 headless 浏览器；服务需可被浏览器访问（同源 `/admin`+`/api`）。
- **TLS 测试证书**：用 `rcgen` 测试期生成自签证书，不依赖外部 CA；证书有效期设短。
- **压测真实性**：mock upstream 要能承受压测吞吐（wiremock 可能成为瓶颈）；必要时用更轻量的自建 TCP/HTTP echo。
- **优雅升级状态**：Pingora 的 socket handover 在某些容器环境需特殊配置（`upgrade_sock` 路径权限），ops 文档标注。
- **收尾回归**：本波结束跑一次全量 `cargo test --all` + E2E + smoke，作为 v1 发布门槛。
