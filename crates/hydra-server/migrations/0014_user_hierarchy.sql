-- ===========================================================================
-- 0014 — 四级用户体系 + 多租户访问令牌 + Global Role（plan §3.1, 只新增）
-- ===========================================================================
--
-- D-17: THIS migration CREATEs ONLY. It deletes NOTHING — `auth_url`,
-- `access_token_hash`, `sub_tenant`, `sub_tenant_route` and `provider_key_binding`
-- stay (schema AND code) until the final batch T10 lands `0015` together with the
-- code deletion. Every binary built on T1–T9 keeps compiling and running against
-- the old tables; the loader additionally reads the new ones. Greenfield: no data
-- migration, no backfill, no dual-mode.
--
-- All ids are application-generated TEXT; timestamps ISO-8601 TEXT.
-- `*` is the reserved wildcard value for the Global Role dimensions (decision
-- D-2) — real ids are UUID/ULID and can never collide with it.

-- 用户组（租户内）
CREATE TABLE user_group (
  id         TEXT PRIMARY KEY,
  tenant_id  TEXT NOT NULL REFERENCES tenant(id) ON DELETE CASCADE,
  name       TEXT NOT NULL,
  enabled    INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE (tenant_id, name)
);
CREATE INDEX idx_user_group_tenant ON user_group(tenant_id);

-- 用户（租户内；一个用户属于一个组）
CREATE TABLE user (
  id             TEXT PRIMARY KEY,
  tenant_id      TEXT NOT NULL REFERENCES tenant(id) ON DELETE CASCADE,
  user_group_id  TEXT NOT NULL REFERENCES user_group(id) ON DELETE RESTRICT,
  external_id    TEXT,
  name           TEXT NOT NULL,
  enabled        INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  billing_status TEXT NOT NULL DEFAULT 'active' CHECK (billing_status IN ('active','delinquent')),
  billing_reason TEXT,
  billing_updated_at TEXT,
  created_at     TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at     TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE (tenant_id, external_id)
);
CREATE INDEX idx_user_tenant_group ON user(tenant_id, user_group_id);

-- 用户 API-Key（只存哈希）
CREATE TABLE user_api_key (
  id          TEXT PRIMARY KEY,
  user_id     TEXT NOT NULL REFERENCES user(id) ON DELETE CASCADE,
  name        TEXT NOT NULL,
  key_hash    TEXT NOT NULL UNIQUE,
  key_prefix  TEXT NOT NULL,
  key_last4   TEXT NOT NULL,
  enabled     INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  expires_at  TEXT,
  created_at  TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_user_api_key_user ON user_api_key(user_id);

-- 租户访问令牌（多令牌；哈希唯一）
CREATE TABLE tenant_access_token (
  id         TEXT PRIMARY KEY,
  tenant_id  TEXT NOT NULL REFERENCES tenant(id) ON DELETE CASCADE,
  name       TEXT NOT NULL,
  token_hash TEXT NOT NULL UNIQUE,
  token_prefix TEXT NOT NULL,                   -- 展示用，如 "ht_ab12"
  token_last4  TEXT NOT NULL,                   -- 展示用
  enabled    INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  expires_at TEXT,
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE (tenant_id, name)
);
CREATE INDEX idx_tenant_access_token_tenant ON tenant_access_token(tenant_id);

-- Global Role：五维键；'*' = 通配
CREATE TABLE global_role (
  id            TEXT PRIMARY KEY,
  name          TEXT NOT NULL,
  tenant_id     TEXT NOT NULL DEFAULT '*',
  user_group_id TEXT NOT NULL DEFAULT '*',
  user_id       TEXT NOT NULL DEFAULT '*',
  api_key_id    TEXT NOT NULL DEFAULT '*',
  model_key     TEXT NOT NULL DEFAULT '*',
  enabled       INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  created_at    TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at    TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_global_role_tenant ON global_role(tenant_id);

-- 角色 -> provider 集合
CREATE TABLE global_role_provider (
  id          TEXT PRIMARY KEY,
  role_id     TEXT NOT NULL REFERENCES global_role(id) ON DELETE CASCADE,
  provider_id TEXT NOT NULL REFERENCES provider(id) ON DELETE CASCADE,
  UNIQUE (role_id, provider_id)
);
CREATE INDEX idx_global_role_provider_role ON global_role_provider(role_id);
