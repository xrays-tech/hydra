//! Wave-3 §2.4 — `ClickHouseSink` (design §9.3).
//!
//! The sink talks to ClickHouse over its native HTTP interface (port 8123,
//! `INSERT ... FORMAT JSONEachRow`) — see `sink.rs` for why the `clickhouse`
//! crate is not used. T4.2 (schema alignment) is a real, deterministic test of
//! the pure JSON-row builder; T4.1 (batch insert) needs a live ClickHouse and
//! is therefore `#[ignore]`.
//!
//! Manual run (with a real ClickHouse reachable at `CH_URL`):
//!   CH_URL=http://127.0.0.1:8123 \
//!     cargo test -p hydra-server --features server,usage-clickhouse \
//!       --test clickhouse_sink -- --ignored

#![cfg(feature = "usage-clickhouse")]

use hydra_core::model::UsageRecord;
use hydra_server::sink::{build_clickhouse_json_row, ClickHouseSink, UsageSink};

fn sample() -> UsageRecord {
    UsageRecord {
        tenant_id: "tenant-1".to_string(),
        provider_id: "provider-1".to_string(),
        model_key: "gpt-test".to_string(),
        client_api_key_masked: None,
        status_code: 200,
        tokens_in: Some(12),
        tokens_out: Some(34),
        cache_hit_tokens: Some(5),
        latency_ms: 250,
        forward_latency_ms: Some(8),
        ttft_ms: Some(180),
        upstream_host: Some("upstream.example".to_string()),
        error: None,
        trace_id: "trace-1".to_string(),
        created_at: "2026-08-08T00:00:00Z".to_string(),
    }
}

// The `usage_record` columns, in declaration order
// (environment/clickhouse/init.sql — provider-neutral token columns).
const USAGE_COLUMNS: &[&str] = &[
    "tenant_id",
    "provider_id",
    "model_key",
    "client_api_key",
    "status_code",
    "tokens_in",
    "tokens_out",
    "cache_hit_tokens",
    "latency_ms",
    "forward_latency_ms",
    "ttft_ms",
    "upstream_host",
    "error",
    "created_at",
];

// ---------------------------------------------------------------------------
// T4.2 — the JSON row emitted for ClickHouse mirrors usage_record columns
// ---------------------------------------------------------------------------

#[test]
fn clickhouse_json_row_matches_usage_record_schema() {
    let json = build_clickhouse_json_row(&sample());

    // Every usage_record column is present, by name.
    for col in USAGE_COLUMNS {
        assert!(
            json.contains(&format!("\"{col}\":")),
            "JSONEachRow row missing column `{col}`: {json}"
        );
    }
    // Exactly the right number of fields: none of the sample's values contain a
    // comma, so the field-separator count must be (columns - 1).
    assert_eq!(
        json.matches(',').count(),
        USAGE_COLUMNS.len() - 1,
        "expected {} field separators in {json}",
        USAGE_COLUMNS.len() - 1
    );

    // NULL handling: client_api_key is None → JSON null, not a quoted string.
    assert!(
        json.contains("\"client_api_key\":null"),
        "Option::None must serialise to null: {json}"
    );
    // Numeric columns must NOT be quoted.
    assert!(
        json.contains("\"status_code\":200"),
        "u16 must be a bare number: {json}"
    );
    assert!(
        json.contains("\"tokens_in\":12"),
        "Option<u64>::Some must be a bare number: {json}"
    );
    // New metrics dimensions are bare numbers (Some) — not quoted, not null.
    assert!(
        json.contains("\"cache_hit_tokens\":5"),
        "cache_hit_tokens Some must be a bare number: {json}"
    );
    assert!(
        json.contains("\"forward_latency_ms\":8"),
        "forward_latency_ms Some must be a bare number: {json}"
    );
    assert!(
        json.contains("\"ttft_ms\":180"),
        "ttft_ms Some must be a bare number: {json}"
    );
}

// ---------------------------------------------------------------------------
// T4.2c — new latency/token dimensions render as JSON null when None
// ---------------------------------------------------------------------------

#[test]
fn clickhouse_json_row_new_metrics_null_when_absent() {
    let mut r = sample();
    r.cache_hit_tokens = None;
    r.forward_latency_ms = None;
    r.ttft_ms = None;
    let json = build_clickhouse_json_row(&r);
    assert!(
        json.contains("\"cache_hit_tokens\":null"),
        "cache_hit_tokens None must be null: {json}"
    );
    assert!(
        json.contains("\"forward_latency_ms\":null"),
        "forward_latency_ms None must be null: {json}"
    );
    assert!(
        json.contains("\"ttft_ms\":null"),
        "ttft_ms None must be null: {json}"
    );
}

// ---------------------------------------------------------------------------
// T4.2b — JSON string escaping is correct (real encoder, no serde_json)
// ---------------------------------------------------------------------------

#[test]
fn clickhouse_json_row_escapes_special_chars() {
    let mut r = sample();
    // value contains: a, double-quote, b, backslash, c, newline, d, tab, e
    r.error = Some("a\"b\\c\nd\te".to_string());
    let json = build_clickhouse_json_row(&r);
    assert!(
        json.contains('\\'),
        "escaped output should contain backslashes: {json}"
    );
    // backslash from input is doubled in JSON:
    assert!(
        json.contains("\\\\"),
        "backslash must be escaped to \\\\: {json}"
    );
    // double-quote is backslash-escaped:
    assert!(
        json.contains("\\\""),
        "double-quote must be escaped: {json}"
    );
    // newline/tab become the two-char escapes \n / \t:
    assert!(json.contains("\\n"), "newline escaped: {json}");
    assert!(json.contains("\\t"), "tab escaped: {json}");
}

// ---------------------------------------------------------------------------
// T4.1 — batch insert into a REAL ClickHouse (manual / CI only)
// ---------------------------------------------------------------------------

/// Constructs the sink, pushes a batch (batch_size=2 → immediate flush), and
/// drops it. Verify manually with:
///   `clickhouse-client -q "SELECT count(), sum(latency_ms) FROM usage_record"`
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real ClickHouse at CH_URL (e.g. http://127.0.0.1:8123)"]
async fn clickhouse_sink_writes_batch() {
    let url = std::env::var("CH_URL").unwrap_or_else(|_| "http://127.0.0.1:8123".to_string());
    let sink = ClickHouseSink::new(&url, 2, 1);
    sink.record(sample()).await;
    sink.record(sample()).await;
    drop(sink);
    // No ClickHouse query client available here; assert no panic + graceful drop.
}
