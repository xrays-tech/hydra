-- Per-provider concurrency admission policy (design dev-docs/design-admission-queue.md §5).
-- All nullable: NULL ⇒ use ProxyConfig defaults (opt-in).
ALTER TABLE provider ADD COLUMN max_concurrency INTEGER;
ALTER TABLE provider ADD COLUMN max_queue_depth INTEGER;
ALTER TABLE provider ADD COLUMN queue_wait_timeout_ms INTEGER;
