# Hydra 详细设计方案

> 基于 `docs/proposal.md` 的 LLM 大模型路由中转服务系统设计与实现方案。
>
> 技术栈：Rust + Pingora + SQLite(sqlx)
>
> 状态：设计阶段（已通过 Oracle Gate Review，P0/P1 已修订，可进入实现）

---

## 目录

1. [技术选型与决策](#1-技术选型与决策)
2. [系统架构](#2-系统架构)
3. [工程结构](#3-工程结构)
4. [数据模型](#4-数据模型)
5. [内存配置中心](#5-内存配置中心)
6. [代理请求生命周期](#6-代理请求生命周期)
7. [路由算法](#7-路由算法)
8. [故障转移与熔断](#8-故障转移与熔断)
9. [用量记录（可插拔 Sink）](#9-用量记录可插拔-sink)
10. [访问限流](#10-访问限流)
11. [外部认证（External Auth）](#11-外部认证external-auth)
12. [多租户 TLS](#12-多租户-tls)
13. [管理 Web API](#13-管理-web-api)
14. [轻量内建 UI](#14-轻量内建-ui)
15. [配置与部署](#15-配置与部署)
16. [安全说明与开放问题](#16-安全说明与开放问题)
17. [可观测性指标目录](#17-可观测性指标目录)
18. [分阶段实施计划](#18-分阶段实施计划)
19. [附录](#19-附录)
20. [集群模式（Cluster Mode）](#20-集群模式cluster-mode)

---

## 1. 技术选型与决策

### 1.1 核心依赖

| 关注点 | 选型 | 版本 | 说明 |
| --- | --- | --- | --- |
| 代理框架 | `pingora` + `pingora-proxy` + `pingora-core` | 0.8.x | Cloudflare 生产级反向代理框架，原生异步、零拷贝、共享连接池 |
| 数据库 | SQLite + `sqlx` | sqlx 0.8 | 编译期 SQL 校验 + `sqlx::migrate!` 内建迁移；启动读一次、偶发管理写的最佳匹配 |
| 异步运行时 | `tokio` | 1.x | Pingora 底层运行时 |
| Web 框架（管理 API） | Pingora `Service` + `ServeHttp` trait | — | 同进程、同运行时、独立端口；避免额外 Tokio runtime |
| 序列化 | `serde` + `serde_json` | 1.x | 实体与 API JSON |
| 无锁热路径 | `arc-swap` | 1.x | 配置热更新，读路径零锁 |
| 并发字典 | `dashmap` | 6.x | 限流计数器、认证缓存、熔断状态、SWRR 状态 |
| HTTP 客户端（认证回源） | `reqwest` | 0.12 | 仅用于调用租户 `auth_url`；禁用 `default-features` 或显式排除 `blocking`（防额外 runtime） |
| 日志 | `tracing` + `tracing-subscriber` | 0.1 | 结构化日志 |
| 指标 | `prometheus` | 0.13 | 自注册默认 registry；由 AdminService 的 `ServeHttp` handler 自托管 `/metrics`（**`pingora-prometheus` 不在 crates.io，弃用**，见 §1.4） |
| ClickHouse 客户端（可选） | `clickhouse` | behind feature flag | 用量记录的可选 Sink |
| 字节扫描（零拷贝提取） | `memchr` | 2.7 | SIMD 加速子串扫描，零分配；提取 `"model"`/`"usage"` 等少量元数据，**避免整体 JSON 反序列化**（见 §6 零拷贝原则） |
| 引用计数缓冲 | `bytes` | 1.x（随 Pingora） | `Bytes::clone()`/`slice()` 为 O(1)（原子计数，无 memcpy）；body filter 借用零拷贝 |
| api-key 哈希 | `sha2` | 0.10 | 纯密码库（无 I/O），归 `hydra-core`；生成认证缓存 key 的 `sha256`（§11.5） |

> **不使用 `pingora-load-balancing`**：其内建 `LoadBalancer<RoundRobin>` 是**无权重**实现，无 SWRR；本系统候选集合动态（每请求按 model×tenant 计算），故自实现 SWRR（§7.2）。健康检查改由自实现熔断器承担（§8.4）。

### 1.2 数据库选型说明（回应提案的 Turso/libSQL 评估）

提案问：**Turso (libSQL) 本地存储效率行不行？**

**结论：效率完全够用（libSQL 嵌入式本地约 ~190ns/查询），但其核心价值（云同步 / 嵌入式副本）对本系统「单进程 + 启动时全量加载到内存」的场景毫无用处。** 经评估采用 **SQLite + sqlx**：

- 热路径是内存而非数据库，DB 选型对运行时性能几乎无影响；
- sqlx 提供编译期 SQL 校验与 `sqlx::migrate!` 零摩擦迁移，libSQL 二者均缺失（refinery/sqlx 未正式支持）；
- libSQL 的 `query()` 多语句陷阱（只执行首条）是真实 footgun；
- 单二进制部署，零外部依赖；
- libSQL 不能与 rusqlite/sqlx 共存于同一依赖树（`libsqlite3-sys` links 冲突）。

> 若未来需要多实例云同步，可后续将 `sqlx` SQLite 切换为 Turso 云端，迁移成本可控。
> （集群模式未走这条路径：多节点配置同步由「快照分发 + 每节点本地 SQLite 副本」实现，
> 见 §20.3，无需共享/云端 DB。）

### 1.3 关键决策记录（已确认）

| 决策点 | 选择 |
| --- | --- |
| 数据库 | **SQLite + sqlx** |
| 管理界面范围 | **REST API + 轻量内建 UI**（同二进制内嵌） |
| 用量记录 | **可插拔 Sink，默认 SQLite，ClickHouse 为可选适配器** |
| 限流计数器 | **内存滑动窗口**（单实例，重启丢失，对窗口限流可接受） |
| 客户端鉴权 | **外部认证**：每租户**必填** `auth_url`（NOT NULL），缓存优先（默认 5 分钟 TTL），缺失则一律拒绝；提供 Admin 接口强制失效（详见 §11） |
| 模型访问授权 | **`TenantModel` 缺省放行闸门**：未配置映射的租户默认可用**全部模型**；一旦配置映射，则仅能访问列表内模型（详见 §7.1） |
| 供应商健康 | **v1 内置内存熔断器**：N 次连续失败 → 内存标记 dead，候选选择跳过，后台探活恢复（详见 §8.4） |

### 1.4 Oracle Gate Review 修订说明

本文档已通过 Oracle 高精度 Gate Review。修订要点（详见各节）：

- **P0**：~~`session.peek_body()` 在 Pingora 0.8.1 不存在（PR #907 关闭未合并）→ 用 `read_body_bytes().await` 读首 chunk + `memchr` 提取 model。**W4 spike 验证**：`request_filter` 中 `read_body_bytes` 前需 `enable_retry_buffering()`（Pingora 默认）以回放首 chunk 正常转发；故障转移重放用自实现 `Vec<Bytes>` 累加器（不依赖 Pingora 的 64KiB retry buffer，大 body 安全）。详见 §6.3、§8.5。~~ **（已废弃，见 terminate-mode）**：当前 `request_filter` 读**全 body**（`read_request_body()` 循环到 EOS）后用 `memchr` 提取 model，不再依赖首 chunk / `enable_retry_buffering` / `Vec<Bytes>` 累加器。详见 `docs/design-change-terminate-mode.md` §4。
- **P1**：SWRR 状态重置（§5.3、§7.2）；`TenantModel` 闸门接入路由（§7.1）；`pingora-prometheus` 改自托管（§1.1）；熔断器（§8.4）；~~故障转移计费竞态对齐 + `retry_after_connect` 配置（§8.1、§8.3）~~ **（§8.1/§8.3 已废弃：terminate-mode 用简单 for 循环，无 `retry_after_connect`）**；Anthropic usage schema（§9.4）。
- **P2**：`weight=0` 语义（§7.2）、配置加载校验（§5.3）、`AuthVerdict` 携带状态码（§11.6）、证书单一数据源（§5.2、§12.1）、metrics 目录（§17）、非 JSON 路径（§6.3）、`/v1` 重写边界（§6.5）、短路面 body 处理（§6.3）。
- **~~零拷贝修订（用户强制需求）~~ → 终止模式修订（已实施）**：禁止在热路径对主体载荷做 JSON 反复 encode/decode（**仍成立**）。~~请求/响应 body 原样转发~~（已更新为 terminate-mode：body 原样传给 reqwest，响应 chunk 原样写回 session）；`"model"` 提取与 `"usage"` 扫描用 `memchr` SIMD 字节扫描（零分配、零 JSON 解析，**仍成立**）；~~首 chunk 正常转发经 `enable_retry_buffering()` 回放~~ **（已删除）**；~~故障转移重放用自实现 `Vec<Bytes>` 累加器~~ **（已删除：terminate-mode 读全 body，`Bytes::clone` O(1) 重放）**。详见 `docs/design-change-terminate-mode.md` §4/§5（原 §6 零拷贝原则、§6.3、§6.6、§8.5、§9.4 描述的是已废弃的 stream-through 架构）。

---

## 2. 系统架构

### 2.1 进程视图

单个二进制内运行 **两个 Pingora `Service`**，共享同一 Tokio 运行时与配置中心：

```
                       Hydra Server (单进程)
 ┌───────────────────────────────────────────────────────────────┐
 │                                                               │
 │  ┌─────────────────────┐        ┌──────────────────────────┐  │
 │  │  ProxyService       │        │  AdminService            │  │
 │  │  (Pingora HttpProxy)│        │  (Pingora ServeHttp)     │  │
 │  │  :443 (TLS, SNI)    │        │  :8081 (内网)            │  │
 │  │  :80  (HTTP, dev)   │        │                          │  │
 │  │                     │        │  ├─ /api/*  REST CRUD    │  │
 │  │  ProxyHttp impl     │        │  ├─ /api/auth/cache 失效 │  │
 │  │  ├─ request_filter  │        │  ├─ /admin/* 内嵌 UI     │  │
 │  │  │  └─ 外部认证 ◄──┐│        │  └─ /metrics Prometheus  │  │
 │  │  ├─ upstream_peer   ││       │     (自托管 handler)     │  │
 │  │  ├─ upstream_*      ││       │                          │  │
 │  │  ├─ response_*      │└──────►│  ┌────────────────────┐  │  │
 │  │  └─ logging         │        │  │ ConfigStore        │  │  │
 │  │                     │        │  │ (ArcSwap<Config>)  │  │  │
 │  │  ┌───────────────┐  │        │  └────────────────────┘  │  │
 │  │  │ CircuitBreaker│  │        │          ▲               │  │
 │  │  │ (内存 dead-set)│  │        │          │ 写后热更新      │  │
 │  │  └───────────────┘  │        │  ┌───────┴────────────┐   │  │
 │  └──────────┬──────────┘        │  │ sqlx::SqlitePool   │   │  │
 │             │ upstream          │  │ + migrations       │   │  │
 │             ▼                   │  └────────────────────┘   │  │
 │  ┌─────────────────────┐        │  ┌────────────────────┐   │  │
 │  │ Provider (LLM/Media)│        │  │ UsageSink (trait)  │   │  │
 │  └─────────────────────┘        │  │ ├─ SqliteSink      │   │  │
 │                                 │  │ └─ ClickHouseSink  │   │  │
 │  ┌─────────────────────┐  缓存  │  ┌────────────────────┐   │  │
 │  │ AuthChecker         │◄──────►│  │ RateLimiter (内存) │   │  │
 │  │  └ AuthCache(5min)  │ 回源   │  └────────────────────┘   │  │
 │  └──────────┬──────────┘────────│                          │  │
 │             ▼                   │                          │  │
 │  ┌─────────────────────┐        │                          │  │
 │  │ RateLimiter (内存)  │        │                          │  │
 │  └─────────────────────┘        │                          │  │
 └─────────────────────────────────┴──────────────────────────┘  │
                ↑                ↑
          Agent / Client     租户认证服务 (auth_url)
```

### 2.2 组件职责

| 组件 | 职责 |
| --- | --- |
| **ProxyService** | Pingora 反向代理，承载所有 LLM 流量；实现 `ProxyHttp` 完成认证、路由、改写、转发、流式回写、故障转移 |
| **AdminService** | 独立端口的 Pingora HTTP 服务，承载 REST API + 内嵌 UI + 自托管 Prometheus 指标；所有 DB 写入经此 |
| **ConfigStore** | 内存配置中心，`ArcSwap` 无锁热路径；启动全量加载；Admin 写后整体热更新（并联动重置运行时状态） |
| **SqlitePool** | sqlx 连接池；迁移；Admin CRUD 持久化 |
| **AuthChecker** | 外部认证：缓存优先（`AuthCache` 5 分钟 TTL）→ 未命中则回源 `tenant.auth_url`；提供强制失效入口 |
| **CircuitBreaker** | 内存熔断器：按 `provider_id` 记录连续失败，标记 dead-set；候选选择跳过 dead，后台探活恢复（§8.4） |
| **UsageSink** | 用量记录可插拔抽象，默认 SQLite，可选 ClickHouse |
| **RateLimiter** | 内存滑动窗口限流器，按 LimitRole 匹配 |

### 2.3 启动流程

严格遵循提案「一次性读取数据库所有的配置内容加载到内存后，启动 Pingora」：

1. 解析 CLI（`pingora_core::server::configuration::Opt`）+ 应用配置文件 `hydra.toml`；
2. 建立 `SqlitePool`，执行 `sqlx::migrate!`；
3. `ConfigStore::load()` 全量加载到 `ArcSwap<ConfigData>`（含引用完整性/URL/证书路径校验，见 §5.4）；
4. 初始化 `AuthChecker`、`CircuitBreaker`、`UsageSink`、`RateLimiter`；
5. 构建 `ProxyService`（含 TLS 监听器，证书来自 `ConfigStore` 单一来源）+ `AdminService`，注册到 `Server`；
6. `server.run_forever()`。

---

## 3. 工程结构

> **crate 划分以 `dev-plan.md` §2 为准**（双 crate：`crates/hydra-core` 纯 + `crates/hydra-server` IO 外壳）。下文目录树按「逻辑模块」展示，落地时 core 模块归 `hydra-core/src/`、IO 模块归 `hydra-server/src/`。

```
hydra/
├── Cargo.toml
├── docs/
│   ├── proposal.md
│   └── design.md              ← 本文件
├── migrations/                ← sqlx 迁移（编译期嵌入二进制）
│   └── 0001_init.sql
├── admin-ui/                  ← 轻量内建 UI 源文件（构建期内嵌）
│   ├── index.html
│   ├── app.js
│   └── style.css
└── src/
    ├── main.rs                ← 入口：Server bootstrap、服务装配
    ├── app.rs                 ← AppState：所有共享组件的持有者
    ├── config.rs              ← hydra.toml + env 配置解析
    ├── error.rs               ← 统一错误类型（thiserror + Pingora Error）
    ├── model/                 ← 领域实体（与提案一一对应）
    │   ├── provider.rs
    │   ├── provider_model.rs
    │   ├── provider_key.rs
    │   ├── tenant.rs          ← 含 auth_url 字段
    │   ├── tenant_provider.rs
    │   ├── tenant_model.rs
    │   └── limit_role.rs
    ├── db/
    │   ├── mod.rs             ← SqlitePool 初始化、PRAGMA、migrate
    │   └── repo.rs            ← 各实体 CRUD（编译期校验 SQL）
    ├── store/
    │   ├── mod.rs             ← ConfigStore（ArcSwap 外壳 + reload_all 联动重置）
    │   ├── config_data.rs     ← ConfigData 内存结构
    │   └── loader.rs          ← DB → 内存加载 + 加载期校验（§5.4）
    ├── auth/
    │   ├── mod.rs             ← AuthChecker：缓存优先 + 回源
    │   ├── cache.rs           ← AuthCache（DashMap + TTL）
    │   ├── client.rs          ← 调用 tenant.auth_url（reqwest）
    │   └── contract.rs        ← 认证契约（请求/响应模型）
    ├── proxy/
    │   ├── mod.rs             ← HydraProxy: ProxyHttp impl
    │   ├── ctx.rs             ← RequestContext（per-request CTX）
    │   ├── extract.rs         ← 从请求提取 domain/api-key/model_key
    │   ├── router.rs          ← 路由算法（TenantModel 闸门 + 交集 + 加权 RR + 熔断过滤）
    │   ├── swrr.rs            ← Smooth Weighted Round Robin
    │   ├── peer.rs            ← 构造 HttpPeer（endpoint/TLS/SNI）
    │   ├── rewrite.rs         ← upstream_request_filter 改写 key/url
    │   ├── sse.rs             ← SSE 流式 chunk 解析、多 provider 用量累加
    │   ├── breaker.rs         ← CircuitBreaker（接 §8.4）
    │   └── tls.rs             ← 动态 SNI 证书选择（TlsAccept），证书来自 ConfigStore
    ├── admin/
    │   ├── mod.rs             ← AdminService 装配（ServeHttp）
    │   ├── router.rs          ← 路由分发
    │   ├── handlers/          ← 各实体 REST handler
    │   ├── metrics.rs         ← 自托管 /metrics handler（prometheus crate）
    │   └── ui.rs              ← 内嵌静态资源（include_dir）
    ├── usage/
    │   ├── mod.rs             ← UsageSink trait + UsageRecord
    │   ├── sqlite.rs          ← 默认 SQLite Sink
    │   └── clickhouse.rs      ← 可选 ClickHouse Sink (feature flag)
    └── limit/
        ├── mod.rs             ← RateLimiter（内存滑动窗口）
        ├── matcher.rs         ← LimitRole 匹配逻辑
        └── window.rs          ← 滑动窗口计数器
```

---

## 4. 数据模型

### 4.1 数据库 Schema（`migrations/0001_init.sql`）

所有表使用 `TEXT` 主键（UUID/ULID 由应用生成），保持与提案字段一一对应。`sqlx::migrate!` 在编译期嵌入并校验。

```sql
-- 供应商
CREATE TABLE provider (
    id        TEXT PRIMARY KEY,
    key       TEXT NOT NULL UNIQUE,        -- 供应商关键字
    name      TEXT NOT NULL,
    endpoint  TEXT NOT NULL,               -- 后端基地址，如 https://api.openai.com
    weight    INTEGER NOT NULL DEFAULT 1 CHECK (weight >= 0),  -- 0=软禁用（不参与 RR，详见 §7.2）
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- 供应商提供的模型
CREATE TABLE provider_model (
    id          TEXT PRIMARY KEY,
    key         TEXT NOT NULL,             -- 模型英文关键字（路由依据）
    name        TEXT NOT NULL,             -- 模型名称
    provider_id TEXT NOT NULL REFERENCES provider(id) ON DELETE CASCADE,
    status      INTEGER NOT NULL DEFAULT 1 -- 1 在线 / 0 手动离线 / -1 探活离线（由后台健康任务写）
                  CHECK (status IN (1, 0, -1)),
    UNIQUE (key, provider_id)
);
CREATE INDEX idx_provider_model_key ON provider_model(key);
CREATE INDEX idx_provider_model_provider ON provider_model(provider_id);

-- 供应商 api-key
CREATE TABLE provider_key (
    id          TEXT PRIMARY KEY,
    provider_id TEXT NOT NULL REFERENCES provider(id) ON DELETE CASCADE,
    api_key     TEXT NOT NULL,             -- 明文存储（见 §16.2）
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_provider_key_provider ON provider_key(provider_id);

-- 租户
CREATE TABLE tenant (
    id        TEXT PRIMARY KEY,
    name      TEXT NOT NULL,
    domain    TEXT NOT NULL UNIQUE,        -- 关联域名（localhost 亦可）
    auth_url  TEXT NOT NULL,               -- 外部认证地址，必填（缺失则该租户请求一律拒绝）
    cert_key  TEXT,                        -- 证书密钥路径
    cert_file TEXT,                        -- 证书文件路径
    enabled   INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- 租户可访问的供应商
CREATE TABLE tenant_provider (
    id          TEXT PRIMARY KEY,
    tenant_id   TEXT NOT NULL REFERENCES tenant(id) ON DELETE CASCADE,
    provider_id TEXT NOT NULL REFERENCES provider(id) ON DELETE CASCADE,
    UNIQUE (tenant_id, provider_id)
);

-- 租户可访问的模型（model_key 引用 provider_model.key；作为访问闸门，详见 §7.1）
CREATE TABLE tenant_model (
    id        TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenant(id) ON DELETE CASCADE,
    model_key TEXT NOT NULL,
    UNIQUE (tenant_id, model_key)
);

-- 访问限制角色（任一 matching_* 为 NULL 表示匹配全部）
CREATE TABLE limit_role (
    id               TEXT PRIMARY KEY,
    name             TEXT NOT NULL,
    matching_key     TEXT,                 -- 匹配客户端 api-key（NULL=全部）
    matching_model   TEXT,                 -- 匹配 model（NULL=全部）
    matching_tenant  TEXT,                 -- 匹配租户 id（NULL=全部）
    matching_provider TEXT,                -- 匹配供应商 id（NULL=全部）
    limit_count      INTEGER,              -- 限额请求数（NULL=不限）
    limit_token      INTEGER,              -- 限额 token（NULL=不限）
    window           TEXT NOT NULL CHECK (window IN ('m', 'h', 'd')),
    enabled          INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- api-key 前缀 → provider 绑定（§7.1b 路由闸门）
CREATE TABLE provider_key_binding (
    id          TEXT PRIMARY KEY,
    key_prefix  TEXT NOT NULL UNIQUE,          -- 客户端 api-key 前缀，如 'sk_aaa_'
    provider_id TEXT NOT NULL REFERENCES provider(id) ON DELETE CASCADE,
    enabled     INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

-- 用量记录（默认 SQLite Sink）。token 列为 provider-中性命名（§9.5）：
--   tokens_in        请求发送的 token 数（含缓存命中；OpenAI prompt_tokens /
--                    Anthropic input_tokens）
--   tokens_out       模型返回的 token 数（OpenAI completion_tokens /
--                    Anthropic output_tokens）
--   cache_hit_tokens 命中缓存的 token 数，⊆ tokens_in（OpenAI
--                    prompt_tokens_details.cached_tokens / Anthropic
--                    cache_read_input_tokens）
-- 不存 total_tokens：它是派生值（tokens_in + tokens_out），无计费意义。
CREATE TABLE usage_record (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    tenant_id         TEXT NOT NULL,
    provider_id       TEXT NOT NULL,
    model_key         TEXT NOT NULL,
    client_api_key    TEXT,                 -- 脱敏后的客户端 key（见 §9.5）
    status_code       INTEGER NOT NULL,
    tokens_in         INTEGER,
    tokens_out        INTEGER,
    cache_hit_tokens  INTEGER,
    latency_ms        INTEGER NOT NULL,
    forward_latency_ms INTEGER,
    ttft_ms           INTEGER,
    upstream_host     TEXT,
    error             TEXT,
    created_at        TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_usage_record_created ON usage_record(created_at);
CREATE INDEX idx_usage_record_tenant ON usage_record(tenant_id, created_at);

-- 内部 schema 版本表由 sqlx::migrate! 自动管理
```

### 4.2 关于 `provider_model.status` 的语义

- `1` 在线：正常参与候选；
- `0` 手动离线：管理员显式停用（不参与候选）；
- `-1` 探活离线：**由后台慢周期健康任务写入**（每 N 秒对所有 `status=1` 的模型探活，连续失败则置 -1，恢复则回 1）。
- **热路径的实时熔断**由内存 `CircuitBreaker` 承担（§8.4），**不**在每次请求失败时写 DB（避免写放大）。两者协作：DB `status∈{0,-1}` 在 `load_all` 时直接排除；`CircuitBreaker` 在运行时叠加排除瞬时故障的 provider。

### 4.3 关系图（文本）

```
tenant ──< tenant_provider >── provider ──< provider_model (key, status)
   │   │                          │            │
   │   │ auth_url ──(外部)──> 租户认证服务     │
   │   │                          └──< provider_key (api_key)
   │   │
   │   └──< tenant_model (model_key)  ──(访问闸门，引用 provider_model.key)
   │
   └─ (cert_key/cert_file) ──> 本地证书文件

limit_role  ──(matching_*)──> 任意实体（软引用，NULL=通配）

(运行时) CircuitBreaker ──> provider_id（内存 dead-set，叠加排除）
```

---

## 5. 内存配置中心

### 5.1 设计目标

- **热路径零锁**：代理转发高频读，使用 `ArcSwap<ConfigData>`，读为原子 load，无 mutex；
- **启动全量加载**：单次 `load()` 构建完整索引；
- **写后热更新**：Admin 每次写 DB 后调用 `store.reload_all()`，整体替换 `ArcSwap` 内的 `ConfigData`（COW 整体替换最简单且原子），并**联动重置相关运行时状态**（SWRR、熔断器，见 §5.3）。

> 注：`AuthCache`、`RateLimiter` 计数器、`CircuitBreaker` dead-set、SWRR 状态属于**运行时状态**（非配置），独立存放于各自的并发结构中，不进入 `ConfigData`。

### 5.2 `ConfigData` 结构（证书为单一数据源）

```rust
// src/store/config_data.rs
#[derive(Clone)]
pub struct ConfigData {
    /// domain(小写) -> 租户（含 localhost 特例；含 auth_url）
    pub tenants_by_domain: HashMap<String, Tenant>,

    /// model_key -> 提供该模型且 status==1 的在线候选（provider_id + weight）
    pub models_by_key: HashMap<String, Vec<ModelProvider>>,

    /// tenant_id -> 允许的 provider_id 集合
    pub tenant_providers: HashMap<String, HashSet<String>>,

    /// tenant_id -> 允许的 model_key 集合（访问闸门）
    pub tenant_models: HashMap<String, HashSet<String>>,

    /// provider_id -> Provider（含 endpoint/weight）
    pub providers: HashMap<String, Provider>,

    /// provider_id -> api_key 列表（运行时随机取）
    pub provider_keys: HashMap<String, Vec<String>>,

    /// 启用的限流角色（按优先级排序）
    pub limit_roles: Vec<LimitRole>,

    /// 域名 -> 已解析证书（PEM）。证书单一数据源归 ConfigStore 所有；
    /// HydraCertStore（§12.1）持有对此 ArcSwap 的 Arc 引用，不再独立存储。
    pub certs: Arc<ArcSwap<HashMap<String, ResolvedCert>>>,  // 类型演进：W1-W2 用 CertMeta{domain,path} 占位 → W4 解析 PEM 为 ResolvedCert
}

pub struct ModelProvider {
    pub provider_id: String,
    pub weight: i32,
}
```

> **单一数据来源**（修订 P2-C7）：证书只存于 `ConfigData.certs`；`HydraCertStore` 通过 `Arc<ArcSwap<...>>` 共享同一引用，热更新时 `ConfigStore` 替换该 `ArcSwap` 内的 map，`HydraCertStore` 立即生效，杜绝双存储漂移。

### 5.3 `ConfigStore` 外壳

```rust
// src/store/mod.rs
#[derive(Clone)]
pub struct ConfigStore {
    inner: Arc<ArcSwap<ConfigData>>,
    pool: SqlitePool,
    // 运行时状态句柄（reload_all 时联动重置）
    swrr: Arc<DashMap<(String, String), SwrrState>>,
    breaker: Arc<CircuitBreaker>,
}

impl ConfigStore {
    pub async fn load(pool: SqlitePool, swrr: Arc<DashMap<_,_>>, breaker: Arc<CircuitBreaker>)
        -> Result<Self> { /* load + 校验（§5.4） */ }

    /// 热路径读（零锁）
    pub fn snapshot(&self) -> arc_swap::Guard<Arc<ConfigData>> { self.inner.load() }

    /// Admin 写后整体替换（COW）+ 联动重置运行时状态
    pub async fn reload_all(&self) -> Result<()> {
        let data = loader::build(&self.pool).await?;   // SELECT * → ConfigData
        loader::validate(&data)?;                       // §5.4 校验
        self.inner.store(Arc::new(data));
        // 修订 P1-B2：候选集合可能变化（权重调整/增删 provider），SWRR 旧状态与新候选不一致 → 清空，下次请求惰性重建。
        self.swrr.clear();
        // 证书 map 已在 ConfigData.certs 内被整体替换，HydraCertStore 自动生效。
        Ok(())
    }
}
```

> **策略**：v1 采用「写后 `reload_all()` 整体 COW 替换 + SWRR 清空」；配置规模小（< 数千行），全量重载毫秒级。`CircuitBreaker` dead-set **不清空**（保持对真实故障的判断），仅在对应 provider 被 Admin 删除时移除其条目。

### 5.4 加载期校验（修订 P2-C5）

`loader::build` 完成、`store` 前执行 `validate`，发现致命问题则 `reload_all` 失败并保留旧快照（启动时则拒绝启动）：

- **引用完整性**：`tenant_provider.provider_id` 存在于 `providers`；`tenant_model.model_key` 至少被一个在线 `provider_model` 提供；
- **endpoint 合法性**：`provider.endpoint` 可解析为 `{scheme, host, port}`，scheme ∈ {http, https}；
- **api_key 非空**：每个在线 provider 至少有一条 `provider_key`（否则该 provider 标记并告警，候选时被过滤）；
- **证书路径**：`tenant.cert_file/cert_key` 文件存在且可读、PEM 合法、公私钥匹配；
- **限流角色**：`limit_count`/`limit_token` 不同时为 NULL（否则无意义，告警忽略）。

校验失败仅告警/跳过非致命项；致命项（如全部租户证书缺失）则失败。

---

## 6. 代理请求生命周期

> **✅ 架构变更（已实施）**：已从"零拷贝 stream-through"切换到**"终止模式（Terminate-in-Pingora）"**（已实施 + Oracle 审核 + e2e 验证通过）——在 `request_filter` 内终止请求（读全 body → 用自有 HTTP client 调供应商 → 流式回写），根治 late-model 客户端的 model 提取问题。详见 [`docs/design-change-terminate-mode.md`](design-change-terminate-mode.md) §4 的完整生命周期描述。
>
> **以下 §6.1–6.7 描述的是已废弃的 stream-through 架构（W4 初版）。** 当前实现已切换到 terminate-mode（在 `request_filter` 内终止请求，读全 body → 路由 → 用 reqwest 调供应商 → 流式回写 → `Ok(true)`）。以下内容保留作为历史参考，不再反映当前代码。终止模式的关键差异：
> - model 提取从"首 chunk memchr 赌博"变为"全 body memchr"（任意位置/schema）；
> - 故障转移从"`set_retry` / `fail_to_connect` / `error_while_proxy`"变为"简单 `for` 循环"（全 body 已缓存，`Bytes::clone` O(1) 重放）；
> - 删除全部逆框架 hack（`enable_retry_buffering` / `Vec<Bytes>` 累加器 / `upstream_*_filter` / `response_*_filter` / `body_too_large`）。
>
> 仍成立的"零拷贝"语义：body 字节**原样传给 reqwest**（不做 JSON encode/decode）；`"model"`/`"usage"` 仍用 `memchr` SIMD 扫描提取。

#### 零拷贝与最小拷贝架构（Zero-Copy，**已废弃** — 见上方 terminate-mode 说明）

**目标**：请求/响应的**主体载荷**（prompt、token 流）从下游 socket 到上游 socket（及反向）全程**不做 JSON 反复 encode/decode**，仅在必要处对**少量元数据**做扫描式提取，最大化 IO 吞吐。

**强制原则**（生产代码，对应 dev-plan 铁律）：

- **请求体**：**禁止**整体 `serde_json::from_slice`。仅用 `memchr` SIMD 字节扫描从**首 chunk**提取 `"model"` 字段（约 ~20 字节即早退，零分配）；body 字节**原样转发**上游，不反序列化、不再序列化。
- **响应体（SSE）**：逐 chunk 用 `memchr` 扫描 `"usage"`（零分配、内存带宽级，~10 GB/s）；**仅**命中时反序列化该 chunk（~50 字节）提取用量，99%+ chunk 零 JSON 开销；body **原样透传**客户端，不重组、不再编码。
- **改写**：api-key 替换、`/v1` 重写、Host 改写**只动 header**（`upstream_request_filter` 只收 `&mut RequestHeader`，物理上碰不到 body）。
- **故障转移重放**（双机制，W4 spike 验证）：(a) **首 chunk 正常转发**——`request_filter` 中 `read_body_bytes` 前调用 `enable_retry_buffering()`（Pingora 默认行为），由 Pingora 回放已消费首 chunk → 上游收到完整 body；(b) Pingora 的 64KiB `BODY_BUF_LIMIT` 仅影响其**自身 retry**（截断后 Pingora 内部 retry 失败），本系统**不依赖**它，故障转移重放改用**自实现 `Vec<Bytes>` 累加器**：每 chunk `Bytes::clone()` = O(1) 原子引用计数自增，**零 memcpy、不受 64KiB 限制、大 body 安全**；重放时按序 `write_body(&chunk)`。

**最小不可避免拷贝**（诚实标注，非"绝对零拷贝"）：

- HTTP/1.1 路径每 chunk 有 1 次 `Bytes::copy_from_slice`（Pingora core 的 `BodyReader` 复用内部缓冲区所致，**不改 core 无法消除**）；
- HTTP/2 路径**真正零拷贝**（`h2` 的 `Bytes` 本身引用计数；filter 中 `as_ref()`/`slice()` 借用零拷贝）；
- 故障转移重放：每 chunk 1 次引用计数自增（非 memcpy）。

**`bytes::Bytes` 语义**：`Bytes::clone()`/`slice()` 为 O(1)（原子引用计数，无分配无拷贝）；body filter 中只读 `as_ref()` 即零拷贝借用，仅当 `*body = Some(new)` 替换时才触发旧分配释放。内核级 `splice`/`sendfile` 非本框架能力，本"零拷贝"指**应用层零 JSON 往返 + 引用计数式搬运**。

### 6.1 Pingora Hook 映射

> **⚠️ 以下 §6.1-6.7 描述的是已废弃的 stream-through 架构（W4 初版）。
> 当前实现已切换到 terminate-mode（在 `request_filter` 内终止请求，读全 body → 路由 → 用 reqwest 调供应商 → 流式回写 → `Ok(true)`）。
> 详见 [`docs/design-change-terminate-mode.md`](design-change-terminate-mode.md) §4 的完整生命周期描述。
> 以下内容保留作为历史参考，不再反映当前代码。**

```
请求进入
   │
   ▼
[early_request_filter]   预留：可插入早期过滤
   │
   ▼
[request_filter]         ──┐ ① 域名→租户  ② 解析 api-key
   │  return Ok(true) 短路  │ ③ 外部认证（缓存优先，见 §11）   ← 失败 401/503
   │  return Ok(false) 继续 │ ④ memchr 扫描首 chunk→model_key ← 零拷贝，非路由直通（§6.3a）
   ▼                      ─┘ ⑤ 路由(含 TenantModel 闸门+熔断) ⑥ 前置限流 ← 403/429/413
[upstream_peer]            返回当前候选 HttpPeer（请求体已由 ④ 缓存，可重放）
   │
   ▼
[connected_to_upstream]
   │
   ▼
[upstream_request_filter]  改写：替换 api-key、重写 Host/路径前缀
   │
   ▼
[读取上游响应]
   │  连接失败 ─► [fail_to_connect]  ─► set_retry(true) + 喂熔断器 ─► 回 [upstream_peer]
   │  代理中错 ─► [error_while_proxy] ─► 条件 set_retry(true)（见 §8.3）
   ▼
[upstream_response_filter]  改写响应头（去 provider 指纹、注入 trace-id）
   │
   ▼
[response_filter]
   │
   ▼  （流式：逐 chunk 触发）
[upstream_response_body_filter] ─► 多 schema 解析 SSE/JSON，累加 usage
   │
   ▼
[response_body_filter]      透传（必要时去重/脱敏）
   │
   ▼
[logging]                   计算 latency/usage → UsageSink.record + 指标(§17) + 访问日志 + 喂熔断器(成功)
```

> 注：④ 用 `memchr` 扫描首 chunk 提取 model（零 JSON 解析）；首 chunk 正常转发经 `enable_retry_buffering()` 回放（Pingora 默认），故障转移重放用自实现 `Vec<Bytes>` 累加器（§8.5，不依赖 64KiB retry buffer）。

### 6.2 `RequestContext`（per-request CTX）

```rust
// src/proxy/ctx.rs
pub struct RequestContext {
    pub started_at: Instant,
    pub tenant: Option<Tenant>,            // 由 domain 解析
    pub client_api_key: Option<String>,    // 客户端原始 key（认证/限流匹配/脱敏记录）
    pub auth_verdict: AuthVerdict,         // 认证判定（携带 HTTP 状态，见 §11.6）
    pub model_key: Option<String>,         // 从请求体解析；None=非路由路径（直通）
    pub passthrough: bool,                 // true=非路由路径，跳过路由直连（见 §6.3a）
    pub candidates: Vec<Candidate>,        // 交集候选（加权 RR 排序，已滤 dead-set）
    pub cursor: usize,                     // 当前尝试索引
    pub selected: Option<SelectedRoute>,   // 当前选中 provider + api_key
    pub upstream_bytes_seen: u64,          // 是否已收到上游字节（影响重试安全性）
    pub body_too_large: bool,              // 请求体超限标志（禁用故障转移）
    pub usage: UsageAccumulator,           // SSE 解析累加器
    pub route_error: Option<RouteError>,   // 路由失败原因（写错误响应用）
    pub trace_id: String,                  // 链路追踪 id
}

pub struct Candidate {
    pub provider_id: String,
    pub endpoint: String,                  // 解析后的 host:port + scheme
    pub weight: i32,
}

pub struct SelectedRoute {
    pub provider_id: String,
    pub endpoint_host: String,             // SNI / Host
    pub upstream_api_key: String,          // 随机选取的 provider key
}
```

### 6.3 `request_filter`：认证 + 解析 + 路由 + 前置限流

职责（按顺序）：

1. **解析域名**：`Host` 头 → 小写；若缺省或为 `localhost` → 用 `localhost` 匹配租户；匹配失败 → 写 404 错误响应，`return Ok(true)`。
2. **租户校验**：租户存在且 `enabled=1`，否则 403。
3. **解析客户端 api-key**：`Authorization: Bearer xxx` 或 `x-api-key`。
4. **外部认证**：调用 `AuthChecker::check(&tenant, &api_key)`（缓存优先，详见 §11）；按 `AuthVerdict` 携带的状态码写错误响应（401/503），`return Ok(true)`。`auth_url` 缺失 → 一律拒绝（401）。
5. **解析 model_key（零拷贝 memchr 扫描，见 §6 零拷贝原则）**：
   - **不**整体读取/反序列化请求体。`request_filter` 内 `read_body_bytes().await` 仅读**首个** chunk，用 `memchr::memmem::find(chunk, b"\"model\"")` SIMD 扫描（约 ~20 字节早退、零分配）得 `model_key`；
   - 该首 chunk 存入 `ctx.body_buffer`（`Vec<Bytes>`）；**首 chunk 转发机制**（W4 spike 验证）：`read_body_bytes` 消耗首 chunk 后，Pingora 自动转发只处理后续 chunk，故须在 `connected_to_upstream`/`upstream_request_filter` 阶段手动 `upstream_session.write_body(&first_chunk)` 预写首 chunk，再让自动转发接管后续；**回退方案**：若手动预写无法与自动转发干净交错，则 `request_body_filter` 首次调用时把存的首 chunk 与当前 chunk 拼接注入（一次小 memcpy，仅首 chunk 大小）；
   - 其余 chunk 由 `request_body_filter` 增量 `push(chunk.clone())` 到 `ctx.body_buffer` + 原样转发上游；
   - `request_filter` 中 `read_body_bytes` 前调用 `enable_retry_buffering()`（Pingora 默认），由 Pingora 回放首 chunk 正常转发（W4 spike 验证）；故障转移重放用 `ctx.body_buffer`（§8.5，不依赖 64KiB retry buffer）；
   - 首 chunk 即含 `model`（OpenAI 兼容格式 `"model"` 在最前），无需读完整 1–10MB body；
   - 解析失败/非 JSON/无 model → 见 §6.3a 处理。
6. **路由**：调用 `router::resolve(&store, &breaker, tenant, model_key)` → 得 `candidates`；为空 → 404/403。
7. **前置限流**：`RateLimiter::check_count(...)`；超限 → 429，`return Ok(true)`。
8. 写入 CTX，`return Ok(false)` 进入 `upstream_peer`。

> **顺序理由**：认证先于 body 读取与路由，未授权请求不计入限流、不消耗 body 读取成本，快速失败。

#### 6.3a 非 JSON / 无 `model` 字段路径的处理（修订 P2-C2）

- **路径前缀非 `/v1/`**（如 `GET /v1/models`、健康检查、webhook 回调）或 body 无 `model` 字段：
  - 默认策略 **`passthrough`**：不走路由算法，**按域名→租户→该租户任一可用 provider 直连**（首选权重最高且非 dead），仅做认证与限流；
  - 由配置 `[proxy] non_route_strategy = "passthrough" | "reject"` 控制；`reject` → 400；
  - 用例：OpenAI 兼容客户端的 `GET /v1/models` 探测、`/health` 等无需 model 路由的请求。

### 6.4 `upstream_peer`：选择当前候选

```rust
async fn upstream_peer(&self, session: &mut Session, ctx: &mut RequestContext)
    -> Result<Box<HttpPeer>>
{
    if ctx.passthrough {
        return peer::build_passthrough(&snapshot, &ctx.tenant);  // 直连，不替换 key
    }
    let sel = ctx.candidates.get(ctx.cursor)
        .ok_or_else(|| perr("no_candidate"))?;
    let provider = &snapshot.providers[&sel.provider_id];
    let peer = peer::build(provider.endpoint)?;        // 解析 scheme/host/port/sni
    let keys = &snapshot.provider_keys[&sel.provider_id];
    ctx.selected = Some(SelectedRoute { ... keys.choose(&mut rng) ... });
    Ok(Box::new(peer))
}
```

### 6.5 `upstream_request_filter`：改写

- **替换鉴权**（仅路由路径）：移除客户端原始 `Authorization`/`x-api-key`，写入 `Authorization: Bearer <provider_api_key>`；
- **重写 Host/路径**（修订 P2-C9，明确边界规则）：提案要求「`/v1` 前面的部分替换成供应商 endpoint」。
  - 规则：定位请求 path 中**首个** `/v1`，保留其（含）之后的尾部，拼接至供应商 endpoint 的 base；例：endpoint=`https://api.openai.com`，path=`/foo/v1/chat/completions` → 上游 path=`/v1/chat/completions`，Host=`api.openai.com`；
  - endpoint 含路径前缀（如 `https://gateway.provider.com/llm`）→ 拼接为 `https://gateway.provider.com/llm/v1/chat/completions`；
  - 无 `/v1` → 整 path 拼接 endpoint（passthrough 类除外）；
  - `Host` / `:authority` 一律改为供应商 endpoint 的 host。
- **注入 `X-Request-Id` / `X-Hydra-Trace-Id`** 便于上游排障；
- **去除可能暴露架构的头**（如客户端自定的内部头）。

### 6.6 响应过滤与流式回写

- `upstream_response_filter`：去除上游可能泄露 provider 指纹的头（如 `server`、`via`）；注入 `X-Hydra-Provider` 供排障（可选，默认关）。
- **流式透传是 Pingora 默认行为，无需额外缓冲**。每个上游 chunk 触发 `upstream_response_body_filter`：
  - 累加 `ctx.upstream_bytes_seen += chunk.len()`；
  - `memchr::memmem::find(chunk, b"\"usage\"")` 零分配扫描；**仅命中**时反序列化该 chunk（~50 字节）提取用量（§9.4）；
  - chunk **原样透传**给客户端（`&mut Option<Bytes>` 不改写 → `as_ref()` 借用零拷贝）。
- **非流式**（`Content-Type: application/json`）：逐 chunk 累积进 `ctx.json_buf`，`end_of_stream` 时**一次** `memchr` 扫描 + 反序列化 `usage`（不重组、不再编码其余字段）。
- **成功响应**（2xx 首字节到达）→ `breaker::on_success(provider_id)` 复位熔断计数。

> **macOS 已知问题**：Pingora Issue #841，SSE 在 macOS 本地开发可能不实时 flush；Linux 生产无影响。开发建议用 Linux 容器。

#### 6.7 短路面 body 处理（修订 P2-C11）

`request_filter` 中写 401/403/413/429 时，若客户端仍在发送大 body：Pingora 在 `respond_error_with_body` 后会丢弃下游剩余 body；为避免连接挂起，写错误响应后调用 `session.downstream_session.set_keepalive(None)` 关闭连接（而非 keep-alive 复用）。对 413（body 超限，§8.5）尤其重要。

---

## 7. 路由算法

严格实现提案 §3.1，并补全 `TenantModel` 闸门与熔断过滤。

### 7.1 候选计算（交集 + TenantModel 闸门 + 熔断过滤）

```rust
// src/proxy/router.rs
pub fn resolve(
    cfg: &ConfigData,
    breaker: &CircuitBreaker,
    tenant: &Tenant,
    model_key: &str,
) -> Result<Vec<Candidate>, RouteError> {
    // (0) TenantModel 访问闸门（缺省放行）：租户【未配置】tenant_models 映射
    //     时默认放行所有模型；【一旦配置】映射即为白名单，映射外的模型拒绝
    //     （ModelNotAllowed → 403）。配置为空集合与缺失条目同义。
    if let Some(allowed_models) = cfg.tenant_models.get(&tenant.id) {
        if !allowed_models.contains(model_key) {
            return Err(RouteError::ModelNotAllowed);   // → 403
        }
    }

    // (1) model_key -> 提供该模型且 status==1 的供应商
    let by_model: HashSet<&str> = cfg.models_by_key.get(model_key)
        .map(|v| v.iter().map(|m| m.provider_id.as_str()).collect())
        .unwrap_or_default();
    if by_model.is_empty() { return Err(RouteError::ModelNotFound); }

    // (2) 租户允许的供应商（fail-closed：无 provider 映射 → TenantForbidden）
    let tenant_ok = cfg.tenant_providers.get(&tenant.id)
        .ok_or(RouteError::TenantForbidden)?;

    // (3) 交集
    let inter: Vec<&str> = by_model.intersection(tenant_ok).copied().collect();
    if inter.is_empty() { return Err(RouteError::NoAvailableProvider); }

    // (4) 过滤：有 api_key；weight>0（§7.2）；未被熔断器标记 dead（§8.4）
    let mut cands: Vec<Candidate> = inter.into_iter()
        .filter(|pid| !breaker.is_dead(*pid))                       // 熔断过滤
        .filter(|pid| cfg.provider_keys.get(*pid).map_or(false, |k| !k.is_empty()))
        .filter_map(|pid| cfg.providers.get(pid).map(|p| Candidate {
            provider_id: pid.into(),
            endpoint: p.endpoint.clone(),
            weight: p.weight,
        }))
        .filter(|c| c.weight > 0)                                   // weight=0 软禁用
        .collect();
    if cands.is_empty() { return Err(RouteError::NoAvailableProvider); }

    // (5) 加权 RR 排序（见 §7.2）
    swrr::order(&mut cands, &tenant.id, model_key);
    Ok(cands)
}
```

### 7.1b api-key 前缀绑定闸门（provider_key_binding）

新增 `provider_key_binding` 表（§4.1）：`key_prefix`（UNIQUE）→ `provider_id`。

- **匹配**：客户端 api-key（Authorization Bearer / x-api-key 的**原始值**）以某条
  `enabled=1` 的 `key_prefix` 开头 ⇒ 候选集被限制为该 provider；
- **最长前缀优先**：多条前缀同时命中时取 `key_prefix` 最长者（最具体）；
- **fail-closed**：绑定的 provider 不在候选集（不提供该模型 / 未被租户授权 /
  熔断 / 软禁用）⇒ `503 NoAvailableProvider`，绝不回落其他后端；
- **无命中** ⇒ 不限制（保持 §7.1 现有语义）；
- **passthrough**（无 model 字段）同样受闸门约束，只允许命中绑定的 provider；
- **管理面**：`/api/v1/provider-key-bindings` CRUD（§13.2 模式），写后热加载；
- **隐私**：仅对原始 key 做前缀比较，key 明文不落库、不进日志（与 §16.4 一致）；
- loader 只加载 `enabled=1` 的行（与 limit_role 同约定）；`config::validate`
  对空前缀 / 未知 provider 告警（Warn）。

对应纯实现位于 `crates/hydra-core/src/router.rs`：`match_key_binding`（最长前缀
匹配）+ `resolve` 候选计算 step 3.5（交集之后、过滤之前）。

### 7.2 加权 Round Robin（Nginx SWRR）

采用 **Smooth Weighted Round-Robin**（与 Nginx 算法一致，分布平滑、无突刺）：

- 每个候选有 `weight`（有效权重，>0）与 `current_weight`（运行时权重）；
- 全局状态按 `(tenant_id, model_key)` 维护，存于 `DashMap<(String,String), SwrrState>`，`SwrrState` 内 `HashMap<provider_id, i32>`；
- 每次请求：对所有候选 `current_weight += weight`，选 `current_weight` 最大者，其 `current_weight -= total_weight`，该候选作为本次首选；
- 故障转移时按已排序候选序列依次取下一个（不再走 SWRR，保证一次请求内不重复选）。

**`weight=0` 语义**（修订 P2-C4）：`weight=0` 表示**软禁用**——provider 保留在配置中但完全不参与候选（`resolve()` 步骤 4 过滤）。全候选 `weight` 恒 >0，SWRR 的 `total_weight` 不可能为 0，无除零风险。

**SWRR 状态一致性**（修订 P1-B2）：

- 候选集合以 `provider_id` 标识，`order()` 内对候选**稳定排序**后与 `SwrrState` 对齐；
- **`reload_all()` 显式 `swrr.clear()`**（§5.3）：因配置变更（权重调整/增删 provider/key）时 `(tenant, model)` 键不变，旧状态会命中复用陈旧权重向量 → 必须清空，下次请求惰性重建；
- 单请求内故障转移不复用 SWRR（按候选数组顺序遍历），无状态污染。

**并发说明**（P2-C13）：`DashMap` 按 key 分桶，单 `(tenant, model)` 热点会串行化其 SWRR 更新；单实例 v1 可接受（LLM 请求本身是长连接、QPS 有限）。

### 7.3 特殊规则（遵循提案）

- **域名缺失 / localhost**：用 `localhost` 匹配租户（仍需 `TenantProvider` 授权；`TenantModel` 缺省放行）。
- **租户未匹配**：直接报错（404）。
- **`TenantModel` 已配置且不含该 model**：报错（403 `ModelNotAllowed`）。**未配置 `TenantModel` 映射的租户默认放行全部模型**（§7.1）。
- **交集为空 / 全部熔断**：报错（403/503 `NoAvailableProvider`）。

---

## 8. 故障转移与熔断

> **⚠️ 以下故障转移机制（Pingora 的 `set_retry` / `fail_to_connect` / `error_while_proxy` / `upstream_bytes_seen`）已在 terminate-mode 重写中删除。
> 当前故障转移是一个简单的 `for candidate in candidates { try send; on fail continue; }` 循环（全 body 已缓存，重放零成本 `Bytes::clone` O(1)）。每次失败 `breaker.on_failure` + `record_retry("terminate_loop")`，成功则 `breaker.on_success`。
> 熔断器（§8.4）不变。详见 [`docs/design-change-terminate-mode.md`](design-change-terminate-mode.md) §4.3。
> 以下 §8.1-8.3 保留作为历史参考，不再反映当前代码。**

### 8.1 触发点

| Hook | 触发时机 | 是否重试 | 处理 |
| --- | --- | --- | --- |
| `fail_to_connect` | TCP/TLS 连接失败 | ✅ 总是重试（未发送任何字节） | `ctx.cursor += 1`；`breaker::on_failure(pid)`；若仍有候选 → `e.set_retry(true)` → 回 `upstream_peer`；否则返回错误 |
| `error_while_proxy` | 连接建立后出错（中断/超时/重置） | ⚠️ **条件**（见 §8.3） | 受 `retry_after_connect` 配置 + `upstream_bytes_seen==0` + body 可重放 三重约束 |

### 8.2 关键安全约束（修订 P1-B6）

LLM 请求为 **POST、非幂等**，且可能已产生计费。两类失败本质不同：

- **连接阶段失败**（`fail_to_connect`）：请求体尚未送达上游，重试零成本，**总是重试**；
- **代理阶段失败**（`error_while_proxy`）：上游已收到请求。流式 LLM 从「建连」到「首字节」常达**数秒**（prompt 处理），此窗口内网络中断会让 `upstream_bytes_seen==0` 但上游**可能已计费**。这不是极端竞态，是常规失败模式。

因此默认**不在代理阶段重试**，由配置显式 opt-in。

### 8.3 `error_while_proxy` 重试条件（对齐代码与文档）

删除原文档中「且请求幂等」的死条件（代码从未判幂等，LLM 也非幂等）。实际条件：

```rust
fn error_while_proxy(&self, _p: &HttpPeer, _s: &mut Session,
                     mut e: Box<Error>, ctx: &mut RequestContext, _reused: bool) -> Box<Error> {
    breaker::on_failure(&ctx.selected().provider_id);
    let cfg = self.cfg.failover;
    let body_replayable = !ctx.body_too_large;        // body 超自实现缓冲上限时不可重放（§8.5；非 Pingora BODY_BUF_LIMIT）
    let first_byte_not_seen = ctx.upstream_bytes_seen == 0;
    let more_candidates = ctx.cursor + 1 < ctx.candidates.len();

    if cfg.retry_after_connect && first_byte_not_seen && body_replayable && more_candidates {
        ctx.cursor += 1;
        e.set_retry(true);                            // opt-in 的代理阶段重试
    }
    e
}
```

- `retry_after_connect` 默认 **`false`**（安全优先）；运维明确接受重复计费风险时置 `true`；
- `upstream_bytes_seen == 0` 是**第二道闸**，防止流式已开始后的灾难性重试；
- `body_replayable`：body 超过自实现缓冲上限（`[proxy] max_request_body`）时不再累积，重放会送出残缺 body → 禁止重试（§8.5）。

> **ops 文档须显著标注**：`retry_after_connect=true` 在上游已处理但首字节未返回的窗口内重试，**会产生重复计费**。

### 8.4 熔断器（CircuitBreaker，修订 P1-B5）

实现提案 `status=-1`（无法访问导致离线）的实时检测，**仅存内存**（不写 DB，避免写放大）：

```rust
// src/proxy/breaker.rs
pub struct CircuitBreaker {
    dead: DashSet<String>,                          // provider_id → dead
    fails: DashMap<String, u32>,                    // provider_id → 连续失败计数
    cfg: BreakerConfig,                             // threshold / cooldown / probe_interval
}

impl CircuitBreaker {
    pub fn on_failure(&self, provider_id: &str) {   // fail_to_connect / error_while_proxy 调用
        let c = self.fails.entry(pid).and_modify(|c| *c += 1).or_insert(1);
        if *c >= self.cfg.threshold { self.dead.insert(pid.to_string()); }
    }
    pub fn on_success(&self, provider_id: &str) {   // upstream 首字节 2xx 调用
        self.fails.remove(pid); self.dead.remove(pid);
    }
    pub fn is_dead(&self, provider_id: &str) -> bool { self.dead.contains(provider_id) }
}
```

- **触发**：连续 `threshold`（默认 5）次 `on_failure` → 进 dead-set，候选选择跳过（§7.1 步骤 4）；
- **恢复**：后台探活任务每 `probe_interval`（默认 10s）对 dead provider 做轻量探测（如 `GET {endpoint}/v1/models` 或 TCP 探活）；成功 → 移出 dead-set 并清零计数；
- **联动 DB `status=-1`**（可选，慢周期）：另设一个低频（如每 60s）任务，对长期 dead 的 provider 将其 `provider_model.status` 写 -1，供 Admin 可见；恢复时回 1。热路径不依赖此写。
- **reload_all 不清 dead-set**：保留对真实故障的判断；provider 被 Admin 删除时同步移除其 dead/fails 条目。

### 8.5 大请求体、零拷贝重放与故障转移（修订 P0-B1 / P1-C3 / 零拷贝）

**双机制（W4 spike 验证）**：(a) **首 chunk 正常转发**——`request_filter` 中 `read_body_bytes` 前调用 `enable_retry_buffering()`（Pingora 默认行为），由 Pingora 回放已消费首 chunk，上游收到完整 body；(b) **故障转移重放**——Pingora 的 64KiB `BODY_BUF_LIMIT` 仅影响其自身 retry，本系统不依赖，改用 `ctx.body_buffer: Vec<Bytes>` 累积请求体：

- **累积**：`request_filter` 读首 chunk（提取 model）+ `request_body_filter` 逐 chunk `push(chunk.clone())`（`Bytes::clone()` = O(1) 引用计数自增，**零 memcpy**）；
- **转发**：body 同时原样转发上游（不缓冲阻塞、不重组、不再编码）；
- **重放**：故障转移时遍历 `ctx.body_buffer` 逐个 `write_body(&chunk)`（`&[u8]` 写 socket，零拷贝）。

**上限策略**：

- **软上限** `[proxy] max_request_body`（默认 8 MiB）：累积字节达此值 → 停止累积（`ctx.body_too_large = true`），但 body 仍原样转发；后果：该请求**代理阶段故障转移被禁用**（`error_while_proxy` 的 `body_replayable=false`，§8.3），连接阶段重试因无完整缓冲同样降级为「首次失败即终止」；
- **硬上限** `[proxy] max_request_body_hard`（默认 32 MiB）：直接 413，`set_keepalive(None)` 关连接（§6.7）；
- **ops 文档**：标注「超大 prompt 禁用故障转移，应控制 `[proxy] max_request_body`；H2 路径零拷贝、H1 路径每 chunk 1 次内核拷贝（Pingora core 限制）」。

### 8.6 最终失败

所有候选耗尽 → `fail_to_proxy` 写错误响应（502 Bad Gateway，body 含 JSON `{error, trace_id, attempts}`）→ `logging` 记录失败用量与指标。

---

## 9. 用量记录（可插拔 Sink）

### 9.1 抽象

```rust
// src/usage/mod.rs
#[async_trait]
pub trait UsageSink: Send + Sync {
    async fn record(&self, record: UsageRecord);
}

pub struct UsageRecord {
    pub tenant_id: String,
    pub provider_id: String,
    pub model_key: String,
    pub client_api_key_masked: Option<String>,   // 见 §9.5 脱敏
    pub status_code: u16,
    pub tokens_in: Option<u64>,          // 请求发送的 token 数（含缓存命中）
    pub tokens_out: Option<u64>,         // 模型返回的 token 数
    pub cache_hit_tokens: Option<u64>,   // 命中缓存的 token 数（⊆ tokens_in）
    pub latency_ms: u64,
    pub upstream_host: Option<String>,
    pub error: Option<String>,
    pub trace_id: String,
    pub created_at: DateTime<Utc>,
}
```

> **token 字段为 provider-中性命名**（§9.5）：不存 `total_tokens`——它是派生值
> （`tokens_in + tokens_out`），无计费意义。

### 9.2 默认实现：`SqliteSink`

- 写入 `usage_record` 表；
- **批量化降负载**：用 `tokio::sync::mpsc` channel 缓冲，后台任务按「每 N 条或每 T 秒」批量 `INSERT`；
- 失败重试 + 指数退避，避免阻塞代理主流程。

### 9.3 可选实现：`ClickHouseSink`（feature flag）

- `Cargo.toml` feature `usage-clickhouse`（依赖极少：sink 走 ClickHouse 原生 HTTP 接口 `INSERT … FORMAT JSONEachRow`，不使用 `clickhouse` crate；`base64` 仅用于 URL userinfo 转 Basic Auth）；
- 配置 `HYDRA_CLICKHOUSE_URL`：支持匿名 `http://host:8123`、**带凭据 `http://user:pass@host:8123`（转 Basic Auth）**、以及查询参数透传（`?database=dogress`、`?user=&password=`）；
- 同样经 channel 异步批量写入；
- 二者实现同一 `UsageSink` trait，启动按配置选择。**feature 开启时同一二进制内同时编译 `SqliteSink` 与 `ClickHouseSink`**，运行时由 `HYDRA_USAGE_SINK=sqlite|clickhouse` 切换，无需重编。

### 9.4 用量解析（零拷贝 memchr 扫描 + 多 provider schema）

`UsageScanner`（`src/proxy/sse.rs`，属 `hydra-core` 纯函数）**默认零 JSON 解析**：

- **逐 chunk**：`memchr::memmem::find(chunk, b"\"usage\"")` 零分配扫描；不命中即跳过（99%+ chunk），body 原样透传；
- **命中时**：从该 chunk 抽取 `data: ` 行的 JSON 切片（`memchr` 定位边界，返回 `&[u8]` 借用零拷贝），**仅**对该 ~50 字节做一次 `serde_json::from_slice` 提取用量；
- **跨 chunk 边界**：`"usage"` 可能被拆在两个 chunk → 维护尾部小缓冲（仅当扫描到不完整 `data:` 行时拼接，常态零分配）。

**多 provider schema**（命中后按 schema 归一为中性字段 tokens_in / cache_hit_tokens / tokens_out）：

| Provider | 流式 usage 位置 | 字段 |
| --- | --- | --- |
| OpenAI 兼容 | 末尾 chunk 的 `usage`（需 `stream_options.include_usage`） | `prompt_tokens / completion_tokens / prompt_tokens_details.cached_tokens` |
| Anthropic | `event: message_delta` 的 `usage` | `input_tokens / output_tokens / cache_read_input_tokens`（累计需累加增量） |
| 通用 JSON | `"usage"` 兜底 | 归一为 tokens_in/tokens_out/cache_hit_tokens |

- 归一：`tokens_in = usage.prompt_tokens ?? usage.input_tokens`；`tokens_out = usage.completion_tokens ?? usage.output_tokens`；`cache_hit_tokens = usage.prompt_tokens_details.cached_tokens ?? usage.cache_read_input_tokens ?? usage.cached_tokens`；**不计算 total**；
- `data: [DONE]` 终止：`memchr` 扫描即可识别；
- 非流式 JSON：逐 chunk 累积进 `ctx.json_buf`，`end_of_stream` 时**一次** `memchr` + 反序列化；
- schema 由 `selected.provider` 推断（路由时记入 CTX），scanner 按之选归一分支；
- **纯函数测试**（W1）：喂入构造的 SSE 字节序列，断言扫描命中/跨边界/未命中/`[DONE]` 行为，零 IO、零 mock。

> **已知限制**：部分 provider 流式不返回 usage（如未设 `include_usage`）。此时三个 token 字段均为 `None`，限流 token 维度对本次请求不计；记录中标注 `tokens=null`。

### 9.5 脱敏

客户端 api-key **仅取前 4 + 后 4 字符**存档（`sk-abcd…wxyz`），原始 key 不落用量表，避免泄露。

---

## 10. 访问限流

### 10.1 匹配规则

对每个请求，从 `ConfigData.limit_roles`（仅 `enabled=1`）中找出所有匹配项：

- `matching_key` 为 NULL **或** 等于客户端 api-key；
- `matching_model` 为 NULL **或** 等于请求 model_key；
- `matching_tenant` 为 NULL **或** 等于租户 id；
- `matching_provider` 为 NULL **或** 等于选中 provider id（注：provider 在路由后确定，故 provider 维度的 token 限流在 `logging` 阶段二次检查/记账）。

多个匹配项**叠加生效**（取最严）。

### 10.2 滑动窗口计数器（内存）

```rust
// src/limit/window.rs
pub struct SlidingWindow {
    window: Duration,                 // m=60s,h=3600s,d=86400s
    samples: Mutex<VecDeque<Instant>>,// 滑动日志（按时间淘汰）
}

impl SlidingWindow {
    pub fn check_and_inc(&self) -> bool { /* 淘汰过期 → 若 < limit 则 push now, true */ }
    pub fn add(&self, tokens: u64) { /* token 维度记账 */ }
}
```

- 计数器存储：`DashMap<LimitKey, SlidingWindow>`，`LimitKey = (role_id, bucket)`，bucket 由该 role 的匹配维度组合确定；
- 周期 GC：后台任务清理空窗口条目，防内存膨胀。

### 10.3 执行时机

| 维度 | 检查时机 | 说明 |
| --- | --- | --- |
| 请求计数 `limit_count` | `request_filter`（路由后） | 前置门控，超限直接 429 |
| Token `limit_token` | `logging`（用量已知后） | 本次请求**已计入**窗口；超额时记录告警，下次请求被拒（最多多放行一次请求，可接受） |

### 10.4 单实例语义

内存实现天然单实例；**集群模式下限流已迁移到 Redis 共享计数**（`RedisRateLimiter`，
见 §20.4，Lua 滑动窗口 + hash tag 键）。默认单节点模式仍为内存实现，行为不变。

---

## 11. 外部认证（External Auth）

### 11.1 设计目标与职责划分

将客户端 api-key 的「是否允许通过」决策**完全委托给租户系统**，Hydra 不持有 api-key 注册表：

- **租户系统负责**：api-key 的发放、欠费、封禁、额度自决；
- **Hydra 负责**：缓存优先校验、回源调用、提供强制失效入口；
- 鉴权点位于 **Pingora `upstream_peer` 之前**（`request_filter` 内），先取 api-key 与域名（→ 租户），再判定。

**强制语义**：`tenant.auth_url` **必须配置**（schema `NOT NULL`）。未配置/为空 → 该租户所有请求一律拒绝（401）。不存在「放行跳过」路径，杜绝裸域名被未授权访问的风险。

### 11.2 认证流程（缓存优先）

```
请求进入 request_filter
   │
   ├─ 解析 domain → tenant
   ├─ 解析 client api_key
   │
   ▼
 tenant.auth_url 存在且非空?
   ├─ 否 ─► 拒绝 401（verdict = Denied{status:401, reason:"no_auth_url"}）
   └─ 是
        ▼
   AuthCache 查 (tenant_id, sha256(api_key))
        │
        ├─ 命中且未过期 (allowed=true)  ─► 放行 (verdict=Allowed{Hit})
        ├─ 命中且未过期 (allowed=false) ─► 拒绝 401 (verdict=Denied{Hit,401})
        └─ 未命中 / 已过期
             │
             ▼
        回源 POST tenant.auth_url （带 Authorization: Bearer <api_key>）
             │
             ├─ 2xx 允许 ─► 写缓存 {allowed=true, TTL=5min} ─► 放行 (Allowed{Miss})
             ├─ 401/403 ─► 写缓存 {allowed=false, TTL=deny_ttl} ─► 拒绝 401 (Denied{Miss,401})
             └─ 超时/5xx/网络错 ─► 按策略 (默认 fail-closed 503，可配 fail-open)
```

> **拒绝结果同样缓存**：避免被拒绝的 key 持续打满 `auth_url`；但拒绝 TTL 可配置得更短（如 30s），便于租户侧解封后较快恢复。

### 11.3 认证契约（通用接口）

Hydra 与租户认证服务之间的 HTTP 契约（`src/auth/contract.rs`）：

**请求**（Hydra → auth_url）：

```http
POST {tenant.auth_url}
Content-Type: application/json
Authorization: Bearer <client_api_key>
X-Hydra-Tenant: <tenant_id>
X-Hydra-Trace-Id: <trace_id>

{"api_key": "<client_api_key>", "key": "<client_api_key>", "tenant_id": "<tenant_id>"}
```

- `api_key` 同时放入 `Authorization` 头与 JSON body，租户侧可任选一种读取；
- `key` 为 **Dogress `crates/api` `/auth/api_key`（`AuthApiKeyRequest`）的字段名别名**，与 `api_key` 同值；双方服务均忽略未知 JSON 字段，超集 body 对 §11.3 契约与 Dogress 契约同时成立；
- `tenant_id`、`trace_id` 便于租户侧日志关联与多租户路由；
- **不含 model**：认证先于 body 解析，此时 model 未知；额度/模型限制由租户系统按其自身规则决定（Dogress 侧 `model_name` 为可选字段，缺省按全部套餐检查）。

**响应**（auth_url → Hydra）：

| HTTP 状态 | 含义 | Hydra 动作 |
| --- | --- | --- |
| `200` | 允许/拒绝由响应体判定 | 见下方「响应体判定」；写缓存后放行或拒绝 |
| `401` / `403` | 拒绝 | 写缓存 `allowed=false`（默认 deny_ttl），返回 401 |
| 其他 / 超时 / 连接错 | 服务异常 | 按策略（见 §11.4） |

**响应体判定（2xx 时 Hydra 读取 body）**：

- **`{"status": false}`**（Dogress `AuthApiKeyResponse.status`）→ 拒绝：写缓存 `allowed=false`（deny_ttl），返回 401。Dogress 认证服务**恒返回 HTTP 200**，拒绝仅通过 `status` 字段表达，必须读 body 而非仅看状态码；
- **`{"allowed": false}`**（本契约可选细化）→ 同样视为拒绝；
- 其余 **合法 JSON 对象** body（`{"status":true}`、`{"allowed":true,...}`）→ 允许；
- **非 JSON 对象**（空、HTML 网页、WAF/登录页、JSON 数组/标量、不可解析）→ **不是有效判定**，视为服务异常，按 §11.4 `fail_mode` 处理（默认 `closed` → 503 拒绝，**不缓存**）—— 防止 auth_url 误指向网页时静默放行所有 key。

**可选精细化响应体**（租户侧可选返回，Hydra 向后兼容）：

```json
{
  "allowed": true,
  "expires_in": 300,
  "reason": "active"
}
```

> 2xx 且 `status=false` 或 `allowed=false` 视为拒绝；2xx 必须是**可解析的 JSON 对象**才可能是允许（否则按 §11.4 服务异常处理，默认拒绝 503）；非 2xx（404/5xx 等）按 HTTP 状态判定（`CacheOp::None` → §11.4）。

### 11.4 `auth_url` 不可用 / 超时的策略

认证服务故障时的处理由配置 `[auth] fail_mode` 决定：

| 策略 | 行为 | 适用 |
| --- | --- | --- |
| `closed`（**默认**） | 返回 `503`，**不缓存**，不转发 | 安全优先，防越权放行 |
| `open` | 放行，**不缓存**，记告警 | 可用性优先，仅用于认证服务高可用的场景 |

- 回源超时默认 **2 秒**（可配 `[auth] timeout_ms`）；
- 回源使用独立连接池（`reqwest`），与 Pingora 上游通道隔离，避免相互影响；
- 故障期间计入指标 `hydra_auth_upstream_error_total`（§17）。

### 11.5 `AuthCache` 结构

```rust
// src/auth/cache.rs
pub struct AuthCache {
    map: DashMap<AuthCacheKey, AuthEntry>,
    default_allow_ttl: Duration,   // 默认 5min
    default_deny_ttl: Duration,    // 默认 30s
}

struct AuthCacheKey {
    tenant_id: String,
    api_key_hash: [u8; 32],        // sha256(api_key)，不存明文
}

struct AuthEntry {
    allowed: bool,
    expires_at: Instant,
    // （可选）来源 auth 服务返回的 expires_in
}
```

- key 使用 `sha256(api_key)`，**内存不存原始 key**（安全/隐私，见 §16.4）；
- 周期 GC 后台任务清理过期项；
- 缓存命中是 O(1)，零网络。

### 11.6 `AuthChecker` 抽象（携带状态码，修订 P2-C6）

```rust
// src/auth/mod.rs
/// verdict 同时携带最终要写回客户端的 HTTP 状态码，避免 request_filter 二次推断
pub enum AuthVerdict {
    Allowed { source: CacheSource },                 // 放行，200 路径继续
    Denied  { status: u16, reason: &'static str, source: CacheSource },
}
pub enum CacheSource { Hit, Miss, Local }            // Local = fail-open 放行 / no_auth_url 拒绝

#[async_trait]
pub trait AuthChecker: Send + Sync {
    /// request_filter 中调用
    async fn check(&self, tenant: &Tenant, api_key: &str) -> AuthVerdict;
    /// Admin 强制失效（见 §13.2）
    fn invalidate(&self, tenant_id: &str, api_keys: &[String]) -> usize;
    fn invalidate_tenant(&self, tenant_id: &str) -> usize;
}
```

- 生产实现 `HttpAuthChecker { cache, http_client, config }`；
- 测试用 `MockAuthChecker`（外部边界 Mock，符合规约）；
- `request_filter` 直接用 `verdict.status` 写错误响应，不再把 401/503 混在同一个 `Denied` 里。

### 11.7 缓存失效（强制重新认证）

提供两种失效粒度，均 **O(1)~O(k)** 删除缓存项，使对应 api-key 下次请求**强制回源**：

| 场景 | 手段 |
| --- | --- |
| 租户系统判定某些 key 欠费/封禁 | 调 Admin 接口删除这些 key 的缓存（见 §13.2），租户侧 auth 服务随后返回拒绝 |
| 某租户整体策略变更 | 调 Admin 接口按 `tenant_id` 清空该租户全部缓存 |
| **租户自助**（欠费停机 / 付费恢复） | 租户持自己的 **Access Token** 调 `POST /api/v1/tenants/{tenant_id}/auth/cache/invalidate` 清除自己名下缓存（见 §13.2；令牌→租户身份由服务端校验，URL 的 tenant_id 必须与令牌归属一致，防越权） |

失效后：缓存内允许项被删 → 下次请求 `Miss` → 回源 → 由租户 `auth_url` 重新决定（是否欠费、是否阻断全由租户自决）。

---

## 12. 多租户 TLS

### 12.1 方案：BoringSSL/OpenSSL + `TlsAccept::certificate_callback`（证书单一来源）

Pingora 默认 BoringSSL，支持运行时 SNI 证书选择（rustls 不支持 `certificate_callback`，故选 BoringSSL/OpenSSL 衍生）：

```rust
// src/proxy/tls.rs
/// 证书不再独立持有 ArcSwap，而是 Arc 引用 ConfigStore 的单一来源（修订 P2-C7）
pub struct HydraCertStore {
    certs: Arc<ArcSwap<HashMap<String, ResolvedCert>>>,  // == ConfigData.certs 的同一引用
    default: Option<ResolvedCert>,
}

#[async_trait]
impl TlsAccept for HydraCertStore {
    async fn certificate_callback(&self, ssl: &mut SslRef) {
        let sni = ssl.servername(NameType::HOST_NAME);
        let cert = sni.and_then(|d| self.certs.load().get(d).cloned())
            .or_else(|| self.default.clone());
        if let Some(c) = cert {
            ext::ssl_use_certificate(ssl, &c.cert).ok();
            ext::ssl_use_private_key(ssl, &c.key).ok();
        }
    }
}
```

- `ResolvedCert` 在 `ConfigStore::load` 时由 `tenant.cert_file` / `cert_key`（路径）读取 PEM 解析得到，存入 `ConfigData.certs`（单一来源）；
- 证书热更新：Admin 修改租户证书 → `reload_all` → `ConfigData.certs` 整体替换 → `HydraCertStore`（同一 Arc 引用）立即生效；
- 监听器：`TlsSettings::with_callbacks(cert_store)` + `enable_h2()`，绑定 `:443`；
- 同时监听 `:80`（明文）用于 `localhost` / 开发，由配置开关。

### 12.2 上游 TLS

供应商 endpoint 普遍为 HTTPS，`HttpPeer::new(addr, true /*tls*/, sni)`，默认 `verify_cert=true`、`alpn=H1H2`；如某 provider 需 mTLS，扩展 `Provider` 增加客户端证书字段（v1 不做）。

### 12.3 SNI 与 Host 一致性（修订 P2-C10）

- 证书按 TLS 握手 SNI 选择，租户按 HTTP `Host` 头解析；正常客户端二者一致；
- 不一致（SNI=A，Host=B）时：**以 `Host` 头解析租户为准**做路由/认证，证书用 SNI 的；若 SNI 域名无证书则回落 default cert；
- 此为低概率场景，仅记录告警指标 `hydra_sni_host_mismatch_total`，不阻断。

---

## 13. 管理 Web API

### 13.1 形态

独立 `AdminService`（Pingora `Service` + `ServeHttp`），监听内网端口（默认 `127.0.0.1:8081`）。同进程、同运行时，避免额外 Tokio runtime（提案架构图右侧的 Web API / Web Admin）。

> 路由分发自行实现轻量匹配（避免引入 axum 造成的双 runtime 风险）；handler 内部共享 `AppState { ConfigStore, SqlitePool, AuthChecker, CircuitBreaker, UsageSink, RateLimiter }`。`/metrics` 由 `metrics.rs` 的自托管 handler 暴露 `prometheus` 默认 registry。

### 13.2 REST 端点

统一前缀 `/api/v1`，JSON。

| 资源 | 方法 | 路径 |
| --- | --- | --- |
| Provider | GET/POST | `/api/v1/providers` |
| | GET/PUT/DELETE | `/api/v1/providers/:id` |
| ProviderModel | GET/POST | `/api/v1/provider-models` |
| | GET/PUT/DELETE | `/api/v1/provider-models/:id` |
| ProviderKey | GET/POST | `/api/v1/provider-keys` |
| | GET/PUT/DELETE | `/api/v1/provider-keys/:id` |
| Tenant | GET/POST | `/api/v1/tenants` |
| | GET/PUT/DELETE | `/api/v1/tenants/:id` |
| TenantProvider | GET/POST | `/api/v1/tenant-providers` |
| | DELETE | `/api/v1/tenant-providers/:id` |
| TenantModel | GET/POST | `/api/v1/tenant-models` |
| | DELETE | `/api/v1/tenant-models/:id` |
| LimitRole | GET/POST | `/api/v1/limit-roles` |
| | GET/PUT/DELETE | `/api/v1/limit-roles/:id` |
| 认证缓存 | **DELETE** | **`/api/v1/auth/cache`**（强制失效，详见下方） |
| 熔断器 | GET / DELETE | `/api/v1/breaker`（查看 dead-set）/ `/api/v1/breaker/:provider_id`（手动复位） |
| 系统 | GET | `/api/v1/health` |
| | POST | `/api/v1/reload`（手动触发 `reload_all`） |

**认证缓存失效接口**（`DELETE /api/v1/auth/cache`）：

```jsonc
// 请求体（二选一或组合）
{
  "tenant_id": "t_xxx",          // 可选：限定租户
  "api_keys": ["sk-aaa", "sk-bbb"]  // 可选：精确失效的 key 列表
}
```

- 语义：从 `AuthCache` 中**尝试删除**匹配项（不存在则忽略），删除后这些 key 下次请求将强制回源 `auth_url` 重新认证；
- `tenant_id` 缺省时仅按 `api_keys` 跨租户匹配（因 key 在缓存中以 `sha256(api_key)` 为值，可定位）；
- `tenant_id` 提供且 `api_keys` 缺省 → 清空该租户全部缓存项；
- 返回：`{ "invalidated": <n>, "tenant_id": "..." }`。

**租户自助失效接口**（`POST /api/v1/tenants/{tenant_id}/auth/cache/invalidate`，迁移 0009）：

```jsonc
// 鉴权：Authorization: Bearer <tenant-access-token>  （租户令牌，非 admin token）
// 请求体（可选；缺省/空 = 清空该租户全部缓存）
{ "api_keys": ["sk-aaa", "sk-bbb"] }
```

- 租户令牌在 admin-UI / admin API 为租户配置（`tenant.access_token_hash`，SHA-256 单向存储，永不回显；编辑留空=保留，改值=轮换，显式 `""`=清除）；
- 服务端以令牌反查租户 id 并校验 == URL 的 tenant_id（不一致 → 403）；未配置令牌/令牌无效 → 401（fail-closed）；
- 语义同管理端 `DELETE /api/v1/auth/cache`：清除该租户缓存项 → 下次请求强制回源；集群模式广播全节点（P4），standby 转发至活跃 leader；
- 返回：`{ "invalidated": <n>, "tenant_id": "..." }`；edge 节点不提供（无 DB）。

**写后一致性**：除认证缓存失效/熔断复位外，每个配置写 handler 成功后立即 `store.reload_all()`，确保内存与 DB 一致；返回最新快照给调用方。

### 13.3 鉴权（v1 最小实现）

Admin 端口仅绑内网 + 单一 Admin Token（`Authorization: Bearer <ADMIN_TOKEN>`，来自环境变量）。后续可扩展多用户/RBAC（见 §16.6）。

### 13.4 错误模型

统一 JSON：`{ "error": { "code": "...", "message": "...", "trace_id": "..." } }`，HTTP 状态码语义化（400 校验/404 不存在/409 唯一冲突/500 内部）。

---

## 14. 轻量内建 UI

### 14.1 目标

无构建步骤、零前端工具链依赖；内嵌于二进制，开箱即用；覆盖配置 CRUD 与状态查看。

### 14.2 实现

- 纯静态资源 `admin-ui/{index.html,app.js,style.css}`（vanilla JS + `<table>` 渲染）；
- 编译期内嵌：`include_dir` 宏打包进二进制；
- `AdminService` 在 `/admin/*` 路径提供静态资源，`/api/*` 提供数据；UI 用 `fetch` 调用同源 `/api/v1/*`；
- 页面分区：Providers / Models / Keys / Tenants（含 auth_url 编辑） / TenantAccess / TenantModels / LimitRoles / AuthCache（失效操作）/ Breaker（dead-set 查看+复位） / Health / Stats（用量统计：`GET /api/v1/stats/usage` 按 tenant/provider 聚合 `hydra_requests_total` 与 `hydra_tokens_total`，图表对比 token 总量与请求次数）。

> 复杂可视化（用量趋势）v1 不做，由外部 BI 直查 SQLite/ClickHouse；Stats 页展示的是进程启动以来的累计快照，非时间窗口趋势。

---

## 15. 配置与部署

### 15.1 `hydra.toml`

```toml
[server]
proxy_tls_addr = "0.0.0.0:443"
proxy_http_addr = "0.0.0.0:80"     # 可选，开发/localhost
admin_addr     = "127.0.0.1:8081"
threads        = 0                 # 0 = CPU 核数

[proxy]
max_request_body       = "8MiB"    # 软上限：超此停止 Vec<Bytes> 重放累积 → 禁用该请求故障转移（body 仍零拷贝转发）
max_request_body_hard  = "32MiB"   # 硬上限：超此直接 413
non_route_strategy     = "passthrough"  # passthrough | reject

[failover]
retry_after_connect = false        # 默认 false（安全）；true 接受重复计费风险

[breaker]
threshold       = 5               # 连续失败阈值
probe_interval  = "10s"           # dead provider 探活间隔

[database]
url = "sqlite://./data/hydra.db?mode=rwc"

[auth]
allow_ttl_secs = 300            # 认证通过缓存 TTL（默认 5 分钟）
deny_ttl_secs  = 30             # 认证拒绝缓存 TTL
timeout_ms     = 2000           # 回源 auth_url 超时
fail_mode      = "closed"       # closed（默认，拒绝）| open（放行不缓存）

[usage]
sink        = "sqlite"             # sqlite | clickhouse
batch_size  = 200
flush_secs  = 5
# clickhouse_url = "..."            # sink=clickhouse 时必填

[admin]
# token 从环境变量 HYDRA_ADMIN_TOKEN 读取（不入文件）

[log]
level = "info"
```

> `/metrics` 端点由 AdminService 自托管（无独立 metrics_addr）。

### 15.2 PRAGMA（SQLite 优化）

初始化时执行：

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 5000;
PRAGMA foreign_keys = ON;
PRAGMA mmap_size = 134217728;
```

### 15.3 部署

- 单二进制 + `data/` 目录（SQLite 文件）+ `hydra.toml`；
- 证书路径在 `tenant` 表中配置（绝对路径或相对 `data/`）；
- 优雅零停机升级：`kill -SIGQUIT <pid>` 后用 `hydra -u` 启新进程（Pingora 内建）。

---

## 16. 安全说明与开放问题

### 16.1 客户端鉴权（已定方案：外部认证）

**问题回顾**：提案字面下租户仅由域名解析，无客户端 api-key 校验，任何命中域名者即可消耗额度。

**已定方案**：采用 §11 外部认证。每租户必填 `auth_url`，Hydra 缓存优先（5 分钟 TTL）回源校验；欠费/封禁等策略由租户系统自决，Hydra 通过 `DELETE /api/v1/auth/cache` 接受租户系统的失效指令强制重新认证。

**遗留权衡**（供评审知悉，不阻塞）：

- `auth_url` 强制必填，未配置的租户所有请求一律 401，不存在裸域名开放放行路径；
- 默认 `fail_mode=closed`：认证服务故障时全部 503，安全优先；若租户认证服务可用性不足，需评估切 `open` 的越权风险；
- 缓存窗口内（默认 5 分钟）已通过认证的 key 即使被租户侧封禁，仍可继续访问，**直到缓存失效或 Admin 主动失效**——这是「缓存换性能」的固有取舍，租户侧可通过缩短 `expires_in` 或主动调用失效接口收敛窗口。

### 16.2 provider api-key 明文存储

`provider_key.api_key` 为供应商真实密钥，必须明文（运行时要原文注入上游）。**缓解**：

- 数据库文件权限 `0600`，仅服务用户可读；
- 生产建议整库加密（SQLCipher / 磁盘加密）；
- Admin API **永远掩码返回** provider key（`first10 + *** + last4`）；`?reveal=1` 接受但已为 no-op（P1-5：admin token 泄露不应暴露所有上游 key）。

### 16.3 内部边界面（外部依赖，允许 Mock）

- 供应商上游（必 Mock 用于故障转移/熔断测试）；
- 租户 `auth_url` 认证服务（必 Mock 用于认证/缓存/失效测试）；
- ClickHouse（可选）；
- 均封装为 trait（`UsageSink`、`AuthChecker`），符合「外部边界允许 Mock」规约。

### 16.4 api-key 隐私

- `AuthCache` 仅存 `sha256(api_key)`，内存无明文；
- 认证回源请求携带明文 key（租户侧已信任，且 auth_url 应为租户自有 HTTPS 端点）；
- 用量记录、访问日志一律脱敏（前 4 + 后 4），永不记录完整 key。

### 16.5 内部逻辑禁 Mock

路由、加权 RR、限流、SSE 解析、认证缓存命中/过期判定、熔断计数等纯函数化，TDD 覆盖，不依赖 IO。

### 16.6 待办（v2 候选，不阻塞 v1）

- Admin 鉴权升级：多用户 + RBAC + Token 轮换（当前单一静态 Token，§13.3）；
- ~~限流计数器 Redis 化以支持多实例~~ —— **已实现**（共享限流/熔断/认证 L2/失效总线，见 §20 集群模式）；
- 用量记录多 Sink 并行（同时写 SQLite + ClickHouse）。

---

## 17. 可观测性指标目录（修订 P2-C8）

所有指标经 `prometheus` 默认 registry，由 AdminService `/metrics` 暴露。

| 指标 | 类型 | 标签 | 说明 |
| --- | --- | --- | --- |
| `hydra_requests_total` | counter | tenant, provider, model, status | 请求总数（含失败） |
| `hydra_request_duration_seconds` | histogram | tenant, provider, model | 端到端延迟 |
| `hydra_upstream_duration_seconds` | histogram | provider, model | 上游耗时 |
| `hydra_retries_total` | counter | tenant, model, stage(connect/proxy) | 故障转移重试次数 |
| `hydra_tokens_total` | counter | tenant, provider, model, kind(prompt/completion) | token 用量（usage 已知时） |
| `hydra_auth_decisions_total` | counter | tenant, verdict(allowed/denied), source(hit/miss/local) | 认证判定 |
| `hydra_auth_upstream_error_total` | counter | tenant | 认证回源故障 |
| `hydra_auth_cache_size` | gauge | — | 认证缓存条目数 |
| `hydra_breaker_dead` | gauge | provider | 当前 dead-set 大小（按 provider） |
| `hydra_breaker_state_transitions_total` | counter | provider, to(dead/alive) | 熔断状态翻转 |
| `hydra_limit_rejected_total` | counter | tenant, role, dim(count/token) | 限流拒绝 |
| `hydra_sni_host_mismatch_total` | counter | — | SNI/Host 不一致（§12.3） |
| `hydra_route_errors_total` | counter | tenant, reason | 路由失败（ModelNotFound/NotAllowed/NoProvider） |

---

## 18. 分阶段实施计划

> 每阶段含 TDD：先写测试再实现，编译/测试通过方算完成。

### Phase 0 — 工程骨架（0.5d）
- Cargo workspace、依赖（`pingora`/`sqlx`/`reqwest`/`prometheus`/`dashmap`/`arc-swap`）、`hydra.toml`、日志、错误类型；
- Pingora 最小 Hello proxy 跑通 `:8080`；
- SQLite 连接 + PRAGMA + 空 migrate；自托管 `/metrics`。

### Phase 1 — 数据层与配置中心（1.5d）
- `0001_init.sql` 全部表（含 `tenant.auth_url`）+ sqlx 编译期校验；
- `db::repo` CRUD；
- `ConfigStore::load` + `ArcSwap` 外壳 + §5.4 加载期校验；
- 单测：DB → 内存映射一致性、校验失败保留旧快照。

### Phase 2 — 外部认证（1.5d）
- `AuthCache`（DashMap + TTL + GC）；
- `HttpAuthChecker`（reqwest 回源，契约见 §11.3）；
- `request_filter` 接入认证（缓存优先 + fail-closed + 携带状态码的 AuthVerdict）；
- `MockAuthChecker` 单测/集成测试。

### Phase 3 — 路由与代理核心（2.5d）
- `request_filter`：域名/tenant/`enable_retry_buffering()` + 首 chunk `read_body_bytes` + `memchr` 提取 model_key；故障转移重放用 `Vec<Bytes>` 累加器（§8.5）；
- `router::resolve`：**TenantModel 闸门** + 交集 + 熔断过滤（纯函数单测覆盖）；
- `swrr::order`（SWRR + `reload_all` 清空）；
- `upstream_peer` + `upstream_request_filter` 改写（含 `/v1` 重写规则）；
- 非 JSON 路径 passthrough（§6.3a）；
- 基本转发联调（Mock 上游）。

### Phase 4 — 故障转移与熔断（2d）
- `fail_to_connect` 总是重试；`error_while_proxy` 按 `retry_after_connect` + `upstream_bytes_seen` + `body_replayable` 条件重试（§8.3）；
- `CircuitBreaker`（on_failure/on_success/is_dead + 后台探活）；
- 大请求体策略 + 413（§8.5）；
- 多上游 Mock 故障注入测试（连接失败/中断/超时/超大 body）。

### Phase 5 — 流式与用量（2d）
- `upstream_response_body_filter` + 多 schema `UsageAccumulator`（OpenAI + Anthropic + 通用）；
- `UsageSink` trait + `SqliteSink`（channel 批量）；
- `ClickHouseSink` feature；
- 真实 OpenAI/Anthropic 流式联调。

### Phase 6 — 限流（1d）
- LimitRole 匹配 + 滑动窗口 + GC；
- 前置 count / 后置 token 双阶段。

### Phase 7 — 管理 API + 热更新 + 认证失效 + 指标（1.5d）
- `AdminService` + REST 全资源 CRUD；
- `DELETE /api/v1/auth/cache` 强制失效；熔断器查看/复位；
- 写后 `reload_all`（联动 SWRR 清空）；`/metrics`、`/health`；
- §17 指标全部接入。

### Phase 8 — 多租户 TLS（1d）
- `HydraCertStore` + `certificate_callback`（证书单一来源）；
- 证书热更新；SNI/Host 一致性告警。

### Phase 9 — 内建 UI + 加固（1.5d）
- `admin-ui` 静态资源内嵌（含认证缓存失效、熔断复位操作）；
- Admin 鉴权、key 掩码、错误模型统一；
- 优雅升级验证、压测、文档（含 `retry_after_connect` 计费风险 ops 说明）。

**预估合计：约 14.5 人日**（与 `dev-plan.md` 波次计划一致；§18 Phase 划分已被波次计划取代，仅作阶段映射参考）。

---

## 19. 附录

### 19.1 关键 Pingora API 速查（已校验 0.8.1）

> **注**：以下 API 表反映 stream-through 架构（已废弃）。Terminate-mode 仅使用 `request_filter`（返回 `Ok(true)`）+ `upstream_peer`（sentinel，返回 `HttpPeer::new("127.0.0.1:0", false, String::new())`）+ `logging`；请求构造与响应流式回写都在 `request_filter` 内完成（用自有 `ProviderClient`（reqwest）调供应商，`session.write_response_header`/`write_response_body` 流式回写）。详见 `docs/design-change-terminate-mode.md`。

| 需求 | API | 备注 |
| --- | --- | --- |
| 选上游 | `upstream_peer()` 返回 `HttpPeer` | 必需 |
| 鉴权/短路 | `request_filter()` → `Ok(true)` | |
| 改写请求头 | `upstream_request_filter(&mut RequestHeader)` | |
| 逐 chunk 观察流 | `upstream_response_body_filter(&mut Option<Bytes>)` | |
| 自写响应 | `session.write_response_header()` + `write_response_body()` | 短路面 + `set_keepalive(None)` |
| 首 chunk 正常转发 | `enable_retry_buffering()`（Pingora 默认，回放首 chunk） | W4 spike 验证；64KiB 仅影响 Pingora 内部 retry |
| 故障转移重放 | **自实现** `Vec<Bytes>` 累加器（`Bytes::clone` O(1)，不受 64KiB 限制） | 大 body 安全 |
| 读取请求体（首 chunk） | `session.downstream_session.read_body_bytes().await` | 仅读首 chunk 提取 model，其余增量转发 |
| 零拷贝元数据提取 | `memchr::memmem::find(&[u8], needle)` | SIMD、零分配；扫描 model/usage |
| 加权 RR | 自实现 SWRR | 内建 LB 无权重 |
| 故障转移 | `fail_to_connect`/`error_while_proxy` → `e.set_retry(true)` | |
| 熔断 | 自实现 `CircuitBreaker`（DashSet） | 非 Pingora 能力 |
| 动态 SNI 证书 | `TlsAccept::certificate_callback`（BoringSSL/OpenSSL） | rustls 不支持 |
| 管理 API / 指标 | 独立 `Service` + `ServeHttp` trait + `prometheus` crate | `pingora-prometheus` 不在 crates.io |
| 模型名路由 | `memchr` 扫描首 chunk 的 `"model"` | 零 JSON 解析、零拷贝借用 |

### 19.2 命名约定

- 实体 id：ULID（字符串，时间有序，避免暴露 UUIDv4 随机性）；
- 配置 `key` 字段：`[a-z0-9-]+`，全局唯一；
- 环境变量前缀 `HYDRA_`。

### 19.3 测试策略

| 层 | 方式 |
| --- | --- |
| 路由/SWRR/限流/解析/认证缓存判定/熔断计数 | 纯函数单测（无 IO，禁 Mock） |
| DB 映射 + 加载期校验 | sqlx in-memory SQLite（`:memory:`） |
| 代理转发/故障转移/熔断/流式/大 body | 集成测试，Mock 上游（`wiremock` 或自建） |
| 外部认证回源 | `MockAuthChecker`（trait Mock，覆盖命中/未命中/拒绝/故障） |
| TLS | 本地自签证书 + curl 验证 |
| 端到端 | Playwright 验证内建 UI（符合规约：web 应用必经浏览器验证） |

### 19.4 Oracle Gate Review 处置汇总

| 项 | 级别 | 处置 |
| --- | --- | --- |
| B1 `peek_body` 不存在 | P0 | ✅ §6.3/§8.5/§19.1 改 `read_body_bytes` + 大 body 策略 |
| B2 SWRR 状态失效 | P1 | ✅ §5.3 `reload_all` 清空 + §7.2 重写 |
| B3 `TenantModel` 闸门 | P1 | ✅ §7.1 接入路由（用户确认=闸门） |
| B4 `pingora-prometheus` | P1 | ✅ §1.1/§13.1/§19.1 自托管 `prometheus` crate |
| B5 `status=-1` 机制 | P1 | ✅ §8.4 熔断器（用户确认=v1 内置） |
| B6 故障转移计费竞态 | P1 | ✅ §8.1/§8.3 对齐 + `retry_after_connect` 配置默认 false |
| C1 多 provider usage schema | P1 | ✅ §9.4 OpenAI+Anthropic+通用 |
| C2 非 JSON 路径 | P2 | ✅ §6.3a passthrough/reject |
| C3 大 body+重试 | P1 | ✅ §8.5 |
| C4 `weight=0` 语义 | P2 | ✅ §4.1/§7.2 软禁用 |
| C5 加载期校验 | P2 | ✅ §5.4 |
| C6 AuthVerdict 状态码 | P2 | ✅ §11.6 |
| C7 证书单一来源 | P2 | ✅ §5.2/§12.1 |
| C8 metrics 目录 | P2 | ✅ §17 |
| C9 `/v1` 重写边界 | P2 | ✅ §6.5 |
| C10 SNI/Host 一致 | P2 | ✅ §12.3 |
| C11 短路面 body 处理 | P2 | ✅ §6.7 |
| C12 Admin 鉴权 | P2 | ✅ §16.6 v2 待办 |
| C13 SWRR 并发 | P2 | ✅ §7.2 注 |
| 零拷贝（用户强制需求） | — | ✅ §6 原则 + §6.3/§6.6/§8.5/§9.4 改 `memchr` 扫描 + `Vec<Bytes>` 重放，禁 body JSON 反复编解码 |

**结论**：P0 已修复，P1 全部处置，P2 已记录/接入，零拷贝架构已贯穿热路径。文档具备进入实现阶段的完成度。

---

## 20. 集群模式（Cluster Mode）

> 本文是集群模式的**概述与设计要点**；完整的角色/环境变量/部署/故障矩阵/实测记录见
> **[`docs/cluster.md`](cluster.md)**（单文档权威源，本节只做索引与设计定位）。

### 20.1 定位

集群模式是 **opt-in**：不设置任何集群环境变量时，Hydra 以单节点模式运行（本设计
§1–§19 全部保持，零外部依赖、零行为变化）。设置 `HYDRA_ROLE=leader|edge` 即进入集群：

- **Redis 是唯一必选外置依赖**，一个 Redis 承载七个用途（见 §20.4）；
- **K8s/k3s 完全无关**：不调用任何编排 API，compose / k3s / k8s / 裸机同一镜像同一行为；
- **自维持**：自举（注册表发现）、自动选举（租约）、自动故障切换、自动加入/退出、自愈
  （时间栅栏杜绝双写、投票 TTL 自清理、失效事件流跨故障切换幂等重放）。

### 20.2 角色模型

| 角色（`HYDRA_ROLE`） | 本地 SQLite | 管理 API | 说明 |
|---|---|---|---|
| 未设置 / `all` | ✅ | ✅ 全部 | 单节点（默认），§1–§19 行为 |
| `leader` | ✅（副本随快照重建） | ✅ 读本地 + 变更转发给 active（P3） | leader 候选，租约竞争，恰一个 active |
| `edge` | ❌ | ❌（仅 `/metrics` `/healthz` `/readyz`） | 无状态数据面，任意扩缩 |

**单写多读**：租约持有者（active）是唯一配置写者；standby 的管理变更透明转发到 active
（P3，保留 Authorization/content-type/trace-id，5s 超时），无目标 503 / 不可达 502 / 绝不本地写。

### 20.3 控制面

- **配置快照**（`SnapshotWire`）：`{version, cfg(秘密剥离), sealed_provider_keys, sealed_certs,
  provider_models, tenant_providers, tenant_models}`。秘密（provider key、证书私钥）AES-256-GCM
  密封（`HYDRA_ENCRYPTION_KEY` 全集群一致），节点本地解密；hydrate fail-closed（任一解密失败
  拒绝整快照）。edge 无本地 DB，配置随快照分发 —— **无共享卷**（证书内嵌快照，migration 0007）。
- **租约**（`hydra:{lease:leader}`）：`SET NX PX` 抢租 + Lua compare-and-renew 续约（只续自己的），
  时间栅栏（续约失败立即降级 Uncertain，fail-closed），新鲜度闸门（最近 3×poll 内成功同步才可参选）。
  实测故障切换约 11–18s（租约 15s + 选举 tick ≤5s，见 cluster.md §5.1）。
- **节点注册表**（`hydra:{nodes}` + 心跳 TTL 30s）：edge/standby 轮询失败 ≥2 次经
  `rotate_from_registry` 旋转；**租约感知轮换**（`active_leader_url`）让回归的旧 leader 即使
  `HYDRA_CONTROL_URL` 指向自己也会跟随当前 active 重建副本。
- **版本持久化**：`config_meta` 表持久化 config version，重启后 `since` 水位单调（不重置为 1）。

### 20.4 共享状态（一个 Redis，七个用途）

| 子系统 | Key | 说明 |
|---|---|---|
| leader 租约 | `hydra:{lease:leader}` | 抢租 + Lua 原子续约 |
| 节点注册表 | `hydra:{nodes}` / `hydra:{node:hb}:<id>` | 注册/心跳/leader 发现 |
| 失效总线 | `hydra:{ctl:events}` + `hydra:{ctl:gen}` | Streams 持久可重放 + generation 兜底 |
| 共享限流 | `hydra:{rl:role:bucket}:count|tokens` | Lua 滑动窗口（同 hash tag） |
| 共享熔断 | `hydra:{br}:dead:{p}` + `hydra:{br}:alldead` | 投票 + 心跳 TTL + 本地 1s 同步 |
| 认证缓存 L2 | `hydra:{auth}:{tenant}:{keyhash}` + 租户索引 | L1 miss 才访问，租户索引免 SCAN |
| — | 命名空间规则 | 多 key 操作同 hash tag；禁 SCAN/MATCH（Cluster 安全） |

### 20.5 Redis 故障行为（数据面永不受影响）

| 子系统 | Redis 宕机行为 |
|---|---|
| 配置快照 | 暂停更新（快照走 leader HTTP，edge 持 last-known-good） |
| 限流 | fail-open（`HYDRA_RATE_LIMIT_FAIL_MODE` 可配 closed）+ 告警指标 |
| 熔断 | 退回本地 trip（投票不同步，本地死集仍生效） |
| 认证 L2 | 退回纯 L1（失效传播暂停，条目按 TTL 过期） |
| 选举 | 续约失败 → 立即降级停写（fail-closed）；无切换直至 Redis 恢复 |

### 20.6 构建与部署

```bash
cargo build --release --features server,cluster-redis,usage-clickhouse   # 集群二进制
```

- compose：`environment/docker-compose.cluster.yml`（redis + 双 leader + `--scale hydra-edge=N` + clickhouse）
- k3s / k8s：纯容器清单（`docs/cluster.md` §4.2），StatefulSet 稳定 leader 身份，readiness 探针
  `/healthz/leader` 路由到 active；edge 无状态 HPA 扩缩
- 裸机：同一二进制 + systemd；`HYDRA_PUBLIC_URL` 用主机名或 VIP

### 20.7 验收与已知限制

live 验收（真实 Redis + 双 leader + 双 edge）的实测记录、验收中发现并修复的缺陷、
以及**已知限制**（禁用 limit_role 不进快照、新鲜度闸门极端场景、`HYDRA_FAILOVER_GRACE_MS`
未接线等）见 **[`docs/cluster.md`](cluster.md) §5.1**，不在此重复。
