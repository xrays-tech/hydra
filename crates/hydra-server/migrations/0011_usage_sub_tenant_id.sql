-- v3: sub-tenant usage attribution (design-sub-tenant.md §7.1, §8 v3).
--
-- The sub-tenant a request belonged to, derived at RECORD time from the RAW
-- api-key prefix (never reconstructed from the masked `client_api_key` column).
--
-- Nullable on purpose: rows written before this migration (no backfill) and
-- requests whose key matched no enabled sub-tenant prefix stay NULL.
ALTER TABLE usage_record ADD COLUMN sub_tenant_id TEXT;
