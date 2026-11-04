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
-- The `tenant` columns below are removed after the legacy tables. The columns
-- are dropped after the table drops so a partial migration leaves the legacy
-- tables gone before a `tenant` row/column change can fail on an older DB.
--
-- Historical migrations that created or extended these tables are untouched;
-- `sqlx::migrate!` checksums them, so a later deletion is the only safe way to
-- remove the schema.
DROP TABLE IF EXISTS provider_key_binding;
DROP TABLE IF EXISTS sub_tenant_route;
DROP TABLE IF EXISTS sub_tenant;

ALTER TABLE tenant DROP COLUMN access_token_hash;
ALTER TABLE tenant DROP COLUMN auth_url;
