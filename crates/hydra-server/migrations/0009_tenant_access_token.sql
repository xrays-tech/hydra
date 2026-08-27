-- Tenant self-service access token (欠费停机 / 付费恢复): a per-tenant
-- bearer credential the TENANT (not the operator) uses to invalidate its own
-- api-key auth cache via POST /api/v1/tenants/{tenant_id}/auth/cache/invalidate.
-- Stored as a SHA-256 hex hash only (one-way): the token is used solely for
-- comparison, never echoed, never recoverable (lost token => rotate).
-- Additive + nullable => fully backward compatible.

ALTER TABLE tenant ADD COLUMN access_token_hash TEXT;