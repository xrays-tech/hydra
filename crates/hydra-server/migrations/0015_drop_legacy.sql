-- ===========================================================================
-- 0015 — DROP the legacy key-prefix binding plane (T10 Stage C)
-- ===========================================================================
--
-- READ THIS BEFORE UPGRADING: **this migration is destructive and irreversible.**
--
-- It removes the three legacy control-plane tables that carried the old
-- api-key-prefix routing model:
--
--   * `provider_key_binding`
--   * `sub_tenant_route`
--   * `sub_tenant`
--
-- The tables are dropped in dependency order: the child rows that reference
-- `sub_tenant` go first, then the parent itself. `provider_key_binding` has
-- no child tables left in the current schema, but it is dropped first so the
-- operator-visible outcome is independent of the order of the two `sub_*`
-- statements.
--
-- This migration does NOT touch `tenant`. The `auth_url` and `access_token_hash`
-- columns on `tenant` are deferred to Stage D and stay present here.
--
-- Historical migrations that created or extended these tables are untouched;
-- `sqlx::migrate!` checksums them, so a later deletion is the only safe way to
-- remove the schema.
DROP TABLE IF EXISTS provider_key_binding;
DROP TABLE IF EXISTS sub_tenant_route;
DROP TABLE IF EXISTS sub_tenant;
