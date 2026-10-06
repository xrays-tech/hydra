-- Sub-tenant usage reads (`GET /tenant/{tid}/api/v1/usage?group_by=sub_tenant`)
-- filter and group on `sub_tenant_id`, which arrived in 0011 WITHOUT an index.
-- Every other "new column/table" migration (0002, 0006, 0010) added its index;
-- 0011 was the only one that did not, so the v3 read dimension scanned the whole
-- per-tenant window and grouped it through a temp B-tree.
--
-- Column order matches the existing usage indexes: (tenant_id, created_at) is
-- the range predicate, and `sub_tenant_id` is included so the GROUP BY can be
-- satisfied from the index.
CREATE INDEX IF NOT EXISTS idx_usage_record_sub_tenant
    ON usage_record(tenant_id, sub_tenant_id, created_at);
