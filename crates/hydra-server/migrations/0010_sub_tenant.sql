-- 子租户（Sub-Tenant）与 api-key 前缀路由（design-sub-tenant.md §3.1）
--
-- 租户下的分组：同一子租户的 client api-key 共享一个 key_prefix。租户为该子
-- 租户配置 model → provider 路由，从而按 api-key 前缀做最低优先级路由。
--
-- key_prefix 租户内唯一（Q2）；route 每行单 provider，model_key 可空（NULL =
-- 该子租户的默认路由）。NULL 的唯一性用部分唯一索引表达——SQLite 把 NULL 视为
-- 彼此不同，表级 UNIQUE(sub_tenant_id, model_key) 会放过多条默认行，故**故意
-- 不**再加表级约束（plan F5）。
CREATE TABLE sub_tenant (
  id          TEXT PRIMARY KEY,
  tenant_id   TEXT NOT NULL REFERENCES tenant(id) ON DELETE CASCADE,
  name        TEXT NOT NULL,
  key_prefix  TEXT NOT NULL,
  enabled     INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  created_at  TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE(tenant_id, name),
  UNIQUE(tenant_id, key_prefix)
);
CREATE INDEX idx_sub_tenant_tenant ON sub_tenant(tenant_id);

CREATE TABLE sub_tenant_route (
  id            TEXT PRIMARY KEY,
  sub_tenant_id TEXT NOT NULL REFERENCES sub_tenant(id) ON DELETE CASCADE,
  model_key     TEXT,                                   -- NULL = 子租户默认路由
  provider_id   TEXT NOT NULL REFERENCES provider(id) ON DELETE CASCADE,
  enabled       INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  created_at    TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at    TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_sub_tenant_route_st ON sub_tenant_route(sub_tenant_id);
CREATE UNIQUE INDEX uq_sub_tenant_route_model
  ON sub_tenant_route(sub_tenant_id, model_key) WHERE model_key IS NOT NULL;
CREATE UNIQUE INDEX uq_sub_tenant_route_default
  ON sub_tenant_route(sub_tenant_id) WHERE model_key IS NULL;
