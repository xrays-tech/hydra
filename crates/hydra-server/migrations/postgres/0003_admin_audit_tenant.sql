-- Tenant-scoped audit rows (F2).
--
-- `tenant_id` is the AUTHENTICATED tenant for a tenant self-service write and
-- NULL for an admin-plane mutation. It is nullable on purpose: every row written
-- before this migration has no tenant identity, and an admin row is not
-- tenant-scoped. The tenant id is never taken from the URL claim — the tenant
-- audit writer records the identity the access token resolved to.
--
-- The column carries no secret and no request body (see 0002_admin_audit.sql).

ALTER TABLE admin_audit ADD COLUMN tenant_id TEXT;
