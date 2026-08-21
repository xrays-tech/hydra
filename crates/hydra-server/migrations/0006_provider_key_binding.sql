-- api-key 前缀 → provider 绑定（路由闸门，design.md §7.1b）
--
-- 客户端 api-key（Authorization: Bearer / x-api-key 的原始值）以 key_prefix
-- 开头时，路由候选集被限制为该 provider（fail-closed）。多条前缀同时命中时
-- 取最长前缀（最具体）。enabled=0 的绑定不参与匹配（loader 只加载 enabled）。
CREATE TABLE provider_key_binding (
    id          TEXT PRIMARY KEY,
    key_prefix  TEXT NOT NULL UNIQUE,          -- 客户端 api-key 前缀，如 'sk_aaa_'
    provider_id TEXT NOT NULL REFERENCES provider(id) ON DELETE CASCADE,
    enabled     INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_provider_key_binding_provider ON provider_key_binding(provider_id);
