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
    created_at         String
) ENGINE = MergeTree()
ORDER BY (created_at, tenant_id, provider_id);
