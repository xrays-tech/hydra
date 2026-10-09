-- ClickHouse init: create usage_record table for Hydra's ClickHouseSink.
-- Auto-run on first ClickHouse start via /docker-entrypoint-initdb.d/.
--
-- Provider-NEUTRAL token columns (design §9.5): the metering table records
-- tokens sent / cache hits / tokens returned regardless of upstream schema.
--   tokens_in        — tokens SENT in the request (all input, cache included;
--                       OpenAI prompt_tokens / Anthropic input_tokens)
--   cache_hit_tokens — tokens that hit the prompt cache (subset of tokens_in;
--                       OpenAI prompt_tokens_details.cached_tokens / Anthropic
--                       cache_read_input_tokens)
--   tokens_out       — tokens RETURNED (OpenAI completion_tokens / Anthropic
--                       output_tokens)
-- There is NO total_tokens column: it is derivable (tokens_in + tokens_out)
-- and carries no billing meaning.
CREATE TABLE IF NOT EXISTS usage_record (
    tenant_id          String,
    provider_id        String,
    model_key          String,
    client_api_key     Nullable(String),
    sub_tenant_id      Nullable(String),
    status_code        UInt16,
    tokens_in          Nullable(UInt64),
    tokens_out         Nullable(UInt64),
    cache_hit_tokens   Nullable(UInt64),
    latency_ms         UInt32,
    forward_latency_ms Nullable(UInt32),
    ttft_ms            Nullable(UInt32),
    upstream_host      Nullable(String),
    error              Nullable(String),
    created_at         String,
    -- The ROW-level idempotency key. Derived by the sink from the per-request
    -- `trace_id` (one usage record per request; a re-sent copy is the SAME
    -- record, so it recomputes the SAME key even in a different batch). See the
    -- block below the table for the full derivation and the uniqueness argument.
    dedup_key          String COMMENT 'row-level idempotency key (per-request trace id), re-sends of one logical event share it and are collapsed by ReplacingMergeTree'
) ENGINE = ReplacingMergeTree()
ORDER BY (dedup_key, tenant_id, provider_id)
-- Row-level idempotency (2026-10-09): the batch-level dedup below
-- (`insert_deduplication_token` + `non_replicated_deduplication_window`) keys on
-- the BATCH. A batch re-sent after the 1000-insert window expires, or a batch
-- whose composition changes on retry, is assigned a NEW token — so the batch-level
-- dedup no longer matches, and the event double-counts usage/quota/billing.
--
-- The `dedup_key` column keys on the EVENT instead:
--   * derivation — the sink stores the record's `trace_id` as `dedup_key`
--     (`clickhouse_dedup_key` in crates/hydra-server/src/usage/backends/clickhouse/
--     mod.rs). `trace_id` is per-request (a fresh `hydra-…` id at context init,
--     proxy/ctx.rs), there is exactly one usage record per request (proxy.rs
--     builds it once per response), and a retry re-sends the SAME in-memory
--     record — so an equivalent re-send, even in a differently-composed batch,
--     computes the SAME key;
--   * convergence — `ReplacingMergeTree()` collapses rows sharing a key, so a
--     re-sent event is counted once. NOTE (review N2, 2026-10-09): the collapse
--     happens on background merge, so DIRECT `clickhouse-client` queries see
--     duplicates until a merge runs; Hydra's own `/usage` reads use
--     `FROM usage_record FINAL` to make "counted once" immediate (crates/hydra-server/src/
--     usage/backends/clickhouse/mod.rs); an operator querying the table directly must add
--     `FINAL` too if deduplication matters;
--   * NO version column is needed (and `created_at` CANNOT be one — it is a
--     `String`, and ClickHouse rejects a String version column: it must be
--     Int*/UInt*/Date/DateTime/DateTime64). The reason no version is needed is
--     that every copy of the same `dedup_key` is BYTE-IDENTICAL: a retry re-sends
--     the very same in-memory `UsageRecord` (the engine retains the original batch
--     and the inserter is called again with it), so all duplicates of one event
--     carry identical fields and "keep whichever row" is a no-op choice.
--     IF some day the same `dedup_key` could carry DIFFERENT content (a re-recorded
--     event, not a re-send), add an integer/Date/DateTime version column — e.g. a
--     monotonic write counter — and switch to `ReplacingMergeTree(version)` so the
--     newest write wins (it keeps the max version per key);
--   * the key also carries `tenant_id`/`provider_id` in the ORDER BY, so a
--     (rare) trace-id collision could only fold two events that share the same
--     tenant AND provider — effectively impossible.
--
-- The batch-level dedup is KEPT on purpose: it no-ops an exact re-send of a batch
-- inside the window (cheaper than a merge), while `dedup_key` covers the
-- cross-window / re-composed cases the token cannot.
--
-- EXISTING MergeTree tables are NOT touched by this file: `CREATE TABLE IF NOT
-- EXISTS` only builds a FRESH table, and ClickHouse cannot ALTER a table's engine
-- or ORDER BY, so a pre-existing table needs a manual rebuild (dev-docs/ops.md).
-- This schema takes effect only on a NEW database.
--
-- The setting is EXECUTED (not merely documented): `non_replicated_deduplication_window`
-- is what makes the per-batch token effective on a non-replicated table; without it
-- the token is accepted and silently ignored (verified on 24.3). `tests/
-- clickhouse_ddl_parity.rs` fails if this file and the inline DDL in
-- `docker-compose.local.yml` ever disagree again.
SETTINGS non_replicated_deduplication_window = 1000;
