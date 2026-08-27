# Wave 2 — 持久化与配置加载（Persistence & Config Loading）

> crate：`hydra-server`（仓储/装配）+ 复用 `hydra-core`（ConfigData/校验纯类型）｜ 估时：1.5d
>
> 关键纪律：DB 测试用 **`:memory:` 真实 SQLite 引擎**（sqlx 原生），**绝不 mock SQL**。加载/校验的纯逻辑已在 W1 测过，本波只做「row → 纯类型」搬运 + ArcSwap 装配。

---

## 1. 目标与范围

### In-scope
- `migrations/0001_init.sql`（design §4.1 全表，含 `tenant.auth_url NOT NULL`）+ `sqlx::migrate!` 嵌入；
- `SqlitePool` 初始化 + PRAGMA（design §15.2）；
- `db::repo`：各实体 CRUD（sqlx 编译期校验，`query!`/`query_as!`）；
- `store::ConfigStore`：`load()`（全量加载 → `core::ConfigData` → ArcSwap）、`reload_all()`（COW 替换 + `swrr.clear()` + 证书 map 替换联动）、`snapshot()`；
- `store::loader`：`build(&pool) -> ConfigData`（row → core 类型）+ 调用 `core::config::validate`；
- Admin 写操作的 DB 落库（handler 留 W5，本波只提供 repo 能力）。

### Out-of-scope
- Admin HTTP 服务（W5）；
- 任何上游/认证 HTTP（W3/W4）；
- Pingora 集成（W4）。

### 依赖与前置（W1 产出契约）
- `hydra-core` 提供：`ConfigData`、`Provider/...` 实体、`config::validate(&ConfigData)->Vec<Issue>`、`SwrrState`、`CertMeta`（证书路径占位，真实 PEM 解析在 W4）。
- 本波把 DB row 翻译为这些类型，再交给 core 校验。

---

## 2. TDD 任务列表

> 数据库测试统一用 `sqlx::SqlitePool::connect("sqlite::memory:").await` + `migrate!`，每测独立库。**真实引擎，非 mock。**

### 2.1 迁移与连接（0.2d）
- T1.1 `migrate_creates_all_tables`：跑 migrate 后 `sqlite_master` 含 8 张业务表 + `_sqlx_migrations`。
- T2.1 `pragma_wal_mode_applied`：初始化后 `journal_mode=WAL`、`foreign_keys=ON`。
- T3.1 `migrate_idempotent`：重复 migrate 不报错、不改 schema。

### 2.2 repo CRUD（0.6d）—— 每实体一组
- T4.1 `provider_crud`：insert / get / list / update / delete；唯一约束 `key` 冲突 → `Err`。
- T4.2 `provider_model_crud`：含 `status` CHECK 约束（1/0/-1）；`UNIQUE(key,provider_id)`；级联删 provider 时 model 被 CASCADE 删除。
- T4.3 `provider_key_crud`：按 provider 列表；CASCADE。
- T4.4 `tenant_crud`：`auth_url NOT NULL` → 插空报错；`domain UNIQUE`；`enabled` CHECK。
- T4.5 `tenant_provider_crud`：`UNIQUE(tenant_id,provider_id)`；FK 级联。
- T4.6 `tenant_model_crud`：`UNIQUE(tenant_id,model_key)`。
- T4.7 `limit_role_crud`：`window` CHECK（m/h/d）；`enabled` CHECK。
- T4.8 `repo_insert_then_query_roundtrip`：复杂场景——1 tenant + 2 provider + models + keys + 关联表，查询回的图结构与写入一致。

### 2.3 loader（0.4d）—— design §5.3
- T5.1 `loader_build_indexes_correct`：写入一组配置 → `build()` 得到的 `ConfigData` 各索引（`tenants_by_domain`/`models_by_key`/`tenant_providers`/`tenant_models`/`provider_keys`/`providers`）正确。
- T5.2 `loader_filters_offline_models`：`provider_model.status ∈ {0,-1}` 不进 `models_by_key`。
- T5.3 `loader_lowercase_domain`：写入 `Domain="Foo.COM"` → 索引 key 为 `foo.com`。
- T5.4 `loader_localhost_tenant`：`domain="localhost"` 正常进 `tenants_by_domain`。
- T5.5 `loader_runs_core_validate`：故意写脏数据（悬空 FK / 坏 endpoint）→ `build()` 返回的 `ConfigData` 经 `core::validate` 报对应 issue。
- T5.6 `loader_cert_meta_from_paths`：`cert_file/cert_key` 非空 → `CertMeta{domain,path}` 占位（PEM 解析留 W4，本波只搬路径）。

### 2.4 ConfigStore 装配（0.3d）—— design §5.3
- T6.1 `store_load_populates_arcswap`：`load()` 后 `snapshot()` 反映 DB 内容。
- T6.2 `store_reload_all_replaces_atomically`：改 DB 后 `reload_all()` → `snapshot()` 更新；并发读期间不返回半成品（用 `ArcSwap::load` 语义验证）。
- T6.3 `store_reload_clears_swrr`：`reload_all()` 后注入的 SWRR `DashMap` 被清空（design §5.3 P1-B2）。
- T6.4 `store_reload_keeps_breaker_deadset`：`reload_all()` 不清熔断 dead-set（design §5.3）；provider 被删时移除其 breaker 条目。
- T6.5 `store_reload_validate_fail_keeps_old`：注入导致 `validate` 致命项的脏数据 → `reload_all()` 返回 `Err` 且 `snapshot()` 仍为旧值（保留旧快照）。
- T6.6 `store_certs_arc_shared`：`HydraCertStore`（W4 才实现，本波用 stub trait 占位仅测 Arc 引用同一性）持有的 `Arc<ArcSwap<..>>` 与 `ConfigData.certs` 是同一引用 → `reload_all` 后 hydra 证书视图同步可见。

---

## 3. 外部边界与测试方式

- **唯一边界 = SQLite 文件/内存引擎**。用 `:memory:` 真实引擎，无 mock。
- 不引入 `mockall` 等；`sqlx` 的 `query!` 编译期已校验 SQL 正确性，运行期仅测行为。
- ClickHouse **不在本波**（W3）。

---

## 4. 与 design.md 的映射
§4.1 schema、§5.2 ConfigData、§5.3 ConfigStore、§5.4 校验、§15.2 PRAGMA。

---

## 5. 出口准则
- [ ] `cargo test -p hydra-server` 中持久化相关全绿；
- [ ] 所有 DB 测试用 `:memory:`，CI 无文件残留；
- [ ] `migrate!` 嵌入二进制，启动自动迁移；
- [ ] 生产代码无 mock；repo 仅薄包装 sqlx；
- [ ] `reload_all` 的「校验失败保留旧快照 + 清 SWRR + 证书联动」三项行为有专门测试。

---

## 6. 风险与注意
- **编译期 SQL 校验**需要 `DATABASE_URL` 指向一个已 migrate 的库（CI 用 `:memory:` + offline cache `sqlx prepare`）；先 `cargo sqlx prepare` 生成 `.sqlx/` 提交，CI 离线校验。
- **WAL on :memory:**：WAL 需要文件；`:memory:` 下 WAL 自动降级，行为仍正确，但生产文件库需显式 WAL（T2.1 在临时文件库上验证）。
- **证书真实解析延后**：`loader` 本波只产 `CertMeta` 占位；W4 的 `tls` 模块负责把 `CertMeta` → `ResolvedCert`（PEM 解析）并回填 `ConfigData.certs`。明确交接点，避免 W4 重复实现加载。
