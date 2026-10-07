//! The **ClickHouse** usage backend: the cluster's shared usage store (ADR-0002's only delivered
//! backend).
//!
//! The writer and the reader are the same store, so both halves open together: the write path posts
//! one JSON object per row to `/?query=INSERT … FORMAT JSONEachRow` with a per-batch
//! `insert_deduplication_token`, and `GET /usage` reads the same table back with bound parameters.
//! The transport, the URL parsing and the TLS policy live in [`transport`] — one owner for "what a
//! ClickHouse URL means".

// The transport is compiled only with the feature that serves this backend: it is ClickHouse's wire
// protocol, and in a build without `usage-clickhouse` nothing can reach it. (Before the move the
// gate was on `pub mod clickhouse;` in `lib.rs`; a module does not inherit the gate of the module
// it moved out of, so it has to say so itself — this was letting 25 transport tests run in builds
// that cannot use the backend.)
#[cfg(feature = "usage-clickhouse")]
pub(crate) mod transport;

// Everything below the descriptor exists only with the feature that compiles this backend (the
// descriptor itself does not: it must be able to say "rebuild with --features usage-clickhouse").
#[cfg(feature = "usage-clickhouse")]
use std::future::Future;
#[cfg(feature = "usage-clickhouse")]
use std::pin::Pin;

#[cfg(feature = "usage-clickhouse")]
use hydra_core::model::UsageRecord;

#[cfg(feature = "usage-clickhouse")]
use crate::usage::engine::{
    drain_on_drop, note_usage_drop, run_channel_sink, UsageSink, MAX_FLUSH_RETRY_WINDOW,
    MAX_SHUTDOWN_WAIT,
};

use super::super::{
    Backend, BackendConfig, BackendError, ReaderContract, Requirement, UsageBackend,
};

/// The endpoint every node writes usage to.
///
/// A constant so the descriptor's `requires`, the read that uses it and the guards that check the
/// documentation all name the same string.
pub(crate) const CLICKHOUSE_URL_ENV: &str = "HYDRA_CLICKHOUSE_URL";

const URL_PURPOSE: &str =
    "the ClickHouse HTTP endpoint that every node writes usage to (it is the cluster's shared \
     usage store, and the store GET /usage reads back)";

pub static DESCRIPTOR: UsageBackend = UsageBackend {
    kind: "clickhouse",
    feature: "usage-clickhouse",
    requires: &[Requirement {
        name: CLICKHOUSE_URL_ENV,
        purpose: URL_PURPOSE,
    }],
    // Recognised with defaults. Listed so an operator finds every knob in one place (`ops.md`), and
    // so the guard can insist that each of them is documented somewhere.
    recognises: &[
        "HYDRA_CLICKHOUSE_CONNECT_TIMEOUT_MS",
        "HYDRA_CLICKHOUSE_IO_TIMEOUT_MS",
        "HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS",
    ],
    reads: ReaderContract::SameBackend,
    open,
    notes: "HTTP INSERT of one JSON row per request (masked key, per-batch dedup token); the same \
            table answers GET /usage",
};

// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The writer (moved verbatim from `sink.rs`, ADR-0002 T1.3)
// ---------------------------------------------------------------------------

// ===========================================================================
// ClickHouseSink (design §9.3) — feature `usage-clickhouse`
// ===========================================================================
//
// NOTE on transport: the W1 `Cargo.toml` pins `clickhouse = "0.1"`, which on
// crates.io is an empty 65-byte placeholder package (no `Client`/`insert` API
// — verified from the vendored source). The real ClickHouse driver by the same
// author lives at `0.11.x`. `Cargo.toml` is owned by another lane, so rather
// than block on a dep bump this sink talks to ClickHouse over its **native,
// first-class HTTP interface** (port 8123, `INSERT … FORMAT JSONEachRow`) using
// only `tokio` (already a `runtime` dep). This is a real, production-grade
// implementation (no test doubles, no placeholders). Switching to the
// `clickhouse` crate later is a mechanical change confined to
// `insert_batch_clickhouse_http`.

#[cfg(feature = "usage-clickhouse")]
use tokio::sync::mpsc as ch_mpsc;

// The transport lives in this backend's `transport` module — the single owner of "how to talk to
// ClickHouse" (URL/credential interpretation, request shape, deadlines, status classification). The
// writer below only builds the `FORMAT JSONEachRow` payload and interprets the answer.
#[cfg(feature = "usage-clickhouse")]
use transport::{is_ok_status, parse_clickhouse_url, response_body, send, ClickHouseConfig};

/// Optional ClickHouse `UsageSink`. Same batching/backoff/Drop semantics as the SQLite one; only the
/// insert transport differs.
#[cfg(feature = "usage-clickhouse")]
pub struct ClickHouseSink {
    /// `None` once the sink has been shut down (explicitly or via `Drop`).
    tx: std::sync::Mutex<Option<ch_mpsc::Sender<UsageRecord>>>,
    join: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[cfg(feature = "usage-clickhouse")]
impl ClickHouseSink {
    /// Spawn the sink against the ClickHouse HTTP endpoint at `url`
    /// (e.g. `http://127.0.0.1:8123`). v1 is HTTP-only (CH's default 8123).
    #[must_use]
    pub fn new(url: &str, batch_size: usize, flush_secs: u64) -> Self {
        let batch_size = batch_size.max(1);
        let capacity = batch_size.max(16);
        let (tx, rx) = ch_mpsc::channel(capacity);

        let cfg = parse_clickhouse_url(url);
        let inserter = move |batch_id: &str, batch: Vec<UsageRecord>| {
            let cfg = cfg.clone();
            let batch_id = batch_id.to_string();
            async move {
                match insert_batch_clickhouse_http(&cfg, &batch, &batch_id).await {
                    Ok(()) => Ok(()),
                    Err(msg) => Err((batch, msg)),
                }
            }
        };

        let join = tokio::spawn(run_channel_sink(
            rx,
            batch_size,
            flush_secs,
            MAX_FLUSH_RETRY_WINDOW,
            inserter,
        ));
        Self {
            tx: std::sync::Mutex::new(Some(tx)),
            join: std::sync::Mutex::new(Some(join)),
        }
    }
}

#[cfg(feature = "usage-clickhouse")]
impl UsageSink for ClickHouseSink {
    fn record(&self, record: UsageRecord) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let tx = {
                let guard = self
                    .tx
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match guard.as_ref() {
                    Some(tx) => tx.clone(),
                    None => {
                        // The sink is already shut down: this record cannot be
                        // delivered. Count it instead of dropping it silently.
                        note_usage_drop("channel_closed", 1);
                        return;
                    }
                }
            };
            if let Err(err) = tx.try_send(record) {
                // Keep the record's trace id, exactly as the SQLite sink above does: the counter
                // says HOW MANY rows were lost, and this field is the only thing that says WHICH.
                // Measured 2026-09-30 (`integration/test_usage_drop_accounting.py`): the
                // ClickHouse sink used to drop with `error=no available capacity reason=...` and no
                // trace id at all — while ClickHouse is the sink cluster deployments are REQUIRED
                // to run (ops.md §12), i.e. the production path was the one without the diagnostic.
                let dropped_trace = match &err {
                    ch_mpsc::error::TrySendError::Full(r)
                    | ch_mpsc::error::TrySendError::Closed(r) => r.trace_id.clone(),
                };
                let (reason, dropped) = match &err {
                    ch_mpsc::error::TrySendError::Full(_) => ("channel_full", 1u64),
                    ch_mpsc::error::TrySendError::Closed(_) => ("channel_closed", 1u64),
                };
                note_usage_drop(reason, dropped);
                tracing::warn!(
                    dropped_trace_id = %dropped_trace,
                    error = %err,
                    reason,
                    "clickhouse usage sink channel full/closed; dropping usage record"
                );
            }
        })
    }

    fn shutdown(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let tx = self
                .tx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            let join = self
                .join
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            drop(tx); // the bg task observes closure and does its final drain
            if let Some(join) = join {
                let _ = tokio::time::timeout(MAX_SHUTDOWN_WAIT, join).await;
            }
        })
    }
}

#[cfg(feature = "usage-clickhouse")]
impl Drop for ClickHouseSink {
    fn drop(&mut self) {
        let tx = self
            .tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let join = self
            .join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drain_on_drop(tx, join);
    }
}

/// The `INSERT` statement. Column list matches the ClickHouse `usage_record`
/// schema (environment/clickhouse/init.sql) — provider-neutral token columns.
#[cfg(feature = "usage-clickhouse")]
const CLICKHOUSE_INSERT_PREFIX: &str =
    "INSERT INTO usage_record (tenant_id, provider_id, model_key, client_api_key, sub_tenant_id, \
     status_code, tokens_in, tokens_out, cache_hit_tokens, latency_ms, \
     forward_latency_ms, ttft_ms, upstream_host, error, created_at)";

/// The insert statement for one batch, carrying ClickHouse's
/// `insert_deduplication_token` so a RETRY of the same batch cannot double-count.
///
/// Why this exists: an `INSERT` whose response is lost (read timeout, connection
/// reset while the reply is in flight) is classified as "the batch did not land"
/// and retried — but the insert may well have COMMITTED, and the table is a plain
/// `MergeTree`, so every retry added a duplicate set of usage rows. Duplicates
/// inflate tenant usage/quota/billing and are invisible after the fact.
///
/// Two halves are required, and BOTH are verified against ClickHouse 24.3:
/// 1. the target table must carry `non_replicated_deduplication_window`
///    (`environment/clickhouse/init.sql`; existing instances need
///    `ALTER TABLE usage_record MODIFY SETTING non_replicated_deduplication_window = 1000`,
///    documented in `ops.md`). WITHOUT it the token is accepted and silently
///    ignored — no error, just no deduplication;
/// 2. this token, stable across the retries of one batch.
///
/// `SETTINGS` must precede `FORMAT` (verified: the reverse is a syntax error).
#[cfg(feature = "usage-clickhouse")]
fn clickhouse_insert_statement(batch_id: &str) -> String {
    // The id is generated by `new_batch_id`, but this is a query built by
    // string formatting, so keep the token to a safe alphabet rather than
    // trusting the caller (a quote here would rewrite the statement).
    let token: String = batch_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(128)
        .collect();
    format!(
        "{CLICKHOUSE_INSERT_PREFIX} SETTINGS insert_deduplication_token='{token}' FORMAT JSONEachRow"
    )
}

/// Insert a batch into ClickHouse over HTTP. On failure returns the error
/// message (the batch is retained by the caller for retry).
///
/// The exchange itself lives in [`crate::clickhouse::send`]; what stays here is
/// the payload (`FORMAT JSONEachRow` rows) and the *classification* of the
/// answer — a 200 means the batch landed, anything else carries ClickHouse's
/// error text, which the caller logs and retries.
#[cfg(feature = "usage-clickhouse")]
async fn insert_batch_clickhouse_http(
    cfg: &ClickHouseConfig,
    records: &[UsageRecord],
    batch_id: &str,
) -> Result<(), String> {
    if records.is_empty() {
        return Ok(());
    }

    // Build the JSONEachRow body (one JSON object per line).
    let mut body = String::with_capacity(records.len() * 256);
    for r in records {
        build_clickhouse_json_row_into(&mut body, r);
        body.push('\n');
    }

    let stmt = clickhouse_insert_statement(batch_id);
    let result = send(cfg, &stmt, &[], body.as_bytes()).await?;

    // ClickHouse returns HTTP 200 + empty body on a successful INSERT; any other
    // status carries the error text in the body. The writer does not need to
    // distinguish truncation from a normal error: both mean "the batch did not
    // land" and are retried.
    if is_ok_status(&result.status_line) {
        Ok(())
    } else {
        let body_text = response_body(&result.body);
        Err(format!(
            "clickhouse insert rejected: status=`{}` body={body_text}",
            result.status_line
        ))
    }
}

/// Append one `UsageRecord` as a single JSON object (`{...}`) to `out`, with no
/// trailing newline. Exposed for deterministic schema testing (T4.2).
#[cfg(feature = "usage-clickhouse")]
pub fn build_clickhouse_json_row(record: &UsageRecord) -> String {
    let mut out = String::with_capacity(256);
    build_clickhouse_json_row_into(&mut out, record);
    out
}

/// In-place variant used by the inserter to avoid an allocation per row.
#[cfg(feature = "usage-clickhouse")]
fn build_clickhouse_json_row_into(out: &mut String, r: &UsageRecord) {
    out.push_str("{\"tenant_id\":");
    json_string_into(out, &r.tenant_id);
    out.push_str(",\"provider_id\":");
    json_string_into(out, &r.provider_id);
    out.push_str(",\"model_key\":");
    json_string_into(out, &r.model_key);
    out.push_str(",\"client_api_key\":");
    match &r.client_api_key_masked {
        Some(v) => json_string_into(out, v),
        None => out.push_str("null"),
    }
    out.push_str(",\"sub_tenant_id\":");
    match &r.sub_tenant_id {
        Some(v) => json_string_into(out, v),
        None => out.push_str("null"),
    }
    out.push_str(",\"status_code\":");
    out.push_str(&r.status_code.to_string());
    out.push_str(",\"tokens_in\":");
    json_opt_u64_into(out, r.tokens_in);
    out.push_str(",\"tokens_out\":");
    json_opt_u64_into(out, r.tokens_out);
    out.push_str(",\"cache_hit_tokens\":");
    json_opt_u64_into(out, r.cache_hit_tokens);
    out.push_str(",\"latency_ms\":");
    out.push_str(&r.latency_ms.to_string());
    out.push_str(",\"forward_latency_ms\":");
    json_opt_u64_into(out, r.forward_latency_ms);
    out.push_str(",\"ttft_ms\":");
    json_opt_u64_into(out, r.ttft_ms);
    out.push_str(",\"upstream_host\":");
    match &r.upstream_host {
        Some(v) => json_string_into(out, v),
        None => out.push_str("null"),
    }
    out.push_str(",\"error\":");
    match &r.error {
        Some(v) => json_string_into(out, v),
        None => out.push_str("null"),
    }
    out.push_str(",\"created_at\":");
    json_string_into(out, &r.created_at);
    out.push('}');
}

/// Append a JSON string literal (with full RFC-8259 escaping) to `out`.
#[cfg(feature = "usage-clickhouse")]
fn json_string_into(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Append a JSON number or `null` for an `Option<u64>`.
#[cfg(feature = "usage-clickhouse")]
fn json_opt_u64_into(out: &mut String, v: Option<u64>) {
    match v {
        Some(n) => out.push_str(&n.to_string()),
        None => out.push_str("null"),
    }
}

// The reader (moved verbatim from `usage_query.rs`, ADR-0002 T1.3)
// ---------------------------------------------------------------------------

#[cfg(feature = "usage-clickhouse")]
use hydra_core::tenant_api::UsageAggregate;

#[cfg(feature = "usage-clickhouse")]
use crate::usage::query::{GroupBy, UsageQuery, UsageQueryError};

/// The ClickHouse-backed reader.
///
/// ## Two queries, not one
///
/// The SQLite arm issues the totals query and (when grouping) the group query
/// separately, and this arm does the same. A single query with a `UNION ALL` of
/// the two would be fewer round-trips, but it **cannot be decoded
/// positionally**: measured on the bundled ClickHouse 24.3, the same statement
/// returned the totals row FIRST in 1 of 6 runs, and adding `ORDER BY is_total`
/// did not stabilise it (a bare `ORDER BY` after a `UNION ALL` binds to the last
/// branch only). A positional decoder would then read a *group* row as the
/// overall totals — one model's numbers reported as the tenant's whole usage.
/// Two queries have no ordering assumption to get wrong.
///
/// ## Binding, never interpolation
///
/// `tenant_id` and the bounds travel as `{name:String}` placeholders bound
/// through `param_*`, and both the SQL and every bound value are percent-encoded
/// by [`transport::send`]. Measured: a bound value of `x' OR 1=1 --`
/// arrives as a literal string, so a tenant id — attacker-influenced text — can
/// never become SQL. `group_by` is a **whitelist** mapped to a column name, so
/// it is the one fragment that is interpolated, and only ever with a constant.
#[cfg(feature = "usage-clickhouse")]
pub struct ClickHouseUsageQuery {
    cfg: transport::ClickHouseConfig,
}

/// Totals row. `MAX(created_at)` is `""` (not NULL) for an empty set on
/// ClickHouse, which `normalize_as_of` maps to `None`.
#[cfg(feature = "usage-clickhouse")]
const CH_TOTALS_SELECT: &str = "\
SELECT count()                                     AS requests, \
       COALESCE(sum(tokens_in), 0)                 AS tokens_in, \
       COALESCE(sum(tokens_out), 0)                AS tokens_out, \
       COALESCE(sum(cache_hit_tokens), 0)          AS cache_hit_tokens, \
       COALESCE(sum(if(status_code >= 400, 1, 0)), 0) AS errors, \
       MAX(created_at)                             AS last_seen \
FROM usage_record \
WHERE tenant_id = {t:String} AND created_at >= {s:String} AND created_at < {e:String} \
FORMAT JSONEachRow";

/// `group_by` → the ClickHouse expression that keys a row. A whitelist: the
/// string is interpolated into the query, so it must never come from input.
#[cfg(feature = "usage-clickhouse")]
fn ch_group_expr(g: GroupBy) -> Option<&'static str> {
    match g {
        GroupBy::None => None,
        GroupBy::Model => Some("model_key"),
        GroupBy::Provider => Some("provider_id"),
        // ClickHouse emits SQL NULL as JSON `null` — measured on the bundled
        // instance over real rows, where the bare column answers
        // `{"key":null,…}` — and the decoder requires a string `key`, so one
        // unattributed row would fail the whole window as a decode error.
        GroupBy::SubTenant => Some("coalesce(sub_tenant_id, '')"),
        // `created_at` is a fixed-width `YYYY-MM-DDTHH:MM:SSZ` string on both
        // backends, so the date is a safe 10-byte slice. No date function: it
        // would both change semantics and drop the primary-key pruning.
        GroupBy::Day => Some("substr(created_at, 1, 10)"),
    }
}

#[cfg(feature = "usage-clickhouse")]
impl ClickHouseUsageQuery {
    /// Built from the configured URL. Parsing stays in
    /// [`crate::clickhouse`], the one owner of what a ClickHouse URL means.
    #[must_use]
    pub(crate) fn new(url: &str) -> Self {
        let mut cfg = transport::parse_clickhouse_url(url);
        // The reader has its own deadline (`HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS`),
        // independent of the writer's: a tenant waiting on a slow query must not
        // inherit the flush task's much longer allowance, and vice versa.
        cfg.io_timeout = transport::env_millis("HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS", 5_000);
        Self { cfg }
    }

    /// Run one statement and return its response body.
    ///
    /// A non-2xx is a *failure*, not a body to decode: ClickHouse answers a
    /// failed query with HTTP 404 and a `Code: N. DB::Exception: …` text
    /// (measured), so the status and the body are both kept for the operator.
    async fn run(&self, sql: &str, params: &[(&str, &str)]) -> Result<String, UsageQueryError> {
        let result = transport::send(&self.cfg, sql, params, b"")
            .await
            .map_err(UsageQueryError::StoreUnavailable)?;
        if !transport::is_ok_status(&result.status_line) {
            return Err(UsageQueryError::StoreUnavailable(format!(
                "clickhouse said {:?}: {}",
                result.status_line.trim(),
                transport::response_body(&result.body)
            )));
        }
        // A body that filled the transport's cap was cut off mid-stream: it is a
        // TOO-BIG answer, not a malformed one, and the two must not be conflated.
        // Retrying a too-big answer returns the same truncated bytes, so this is
        // the one 503 the caller fixes by narrowing the window or shrinking the
        // grouping — never by calling again.
        if result.truncated {
            return Err(UsageQueryError::ResultTooLarge(
                transport::MAX_CLICKHOUSE_RESPONSE,
            ));
        }
        Ok(transport::response_body(&result.body))
    }
}

#[cfg(feature = "usage-clickhouse")]
impl UsageQuery for ClickHouseUsageQuery {
    fn aggregate<'a>(
        &'a self,
        tenant_id: &'a str,
        since: &'a str,
        until: &'a str,
        group_by: GroupBy,
    ) -> Pin<Box<dyn Future<Output = Result<UsageAggregate, UsageQueryError>> + Send + 'a>> {
        Box::pin(async move {
            let params: [(&str, &str); 3] = [("t", tenant_id), ("s", since), ("e", until)];
            let totals_body = self.run(CH_TOTALS_SELECT, &params).await?;
            // The totals query always projects `last_seen`, so a decode failure
            // here is a shape drift or a non-numeric counter — either way it must
            // surface as a failure, never as a zeroed `totals`.
            let totals = hydra_core::tenant_api::decode_usage_json_each_row(&totals_body)
                .map_err(|e| UsageQueryError::Decode(format!("totals: {e:?}")))?;

            let mut rows = Vec::new();
            if let Some(expr) = ch_group_expr(group_by) {
                let sql = format!(
                    "SELECT {expr} AS key, count() AS requests, \
                            COALESCE(sum(tokens_in), 0) AS tokens_in, \
                            COALESCE(sum(tokens_out), 0) AS tokens_out, \
                            COALESCE(sum(cache_hit_tokens), 0) AS cache_hit_tokens, \
                            COALESCE(sum(if(status_code >= 400, 1, 0)), 0) AS errors \
                     FROM usage_record \
                     WHERE tenant_id = {{t:String}} AND created_at >= {{s:String}} \
                       AND created_at < {{e:String}} \
                     GROUP BY key ORDER BY key FORMAT JSONEachRow"
                );
                let body = self.run(&sql, &params).await?;
                rows = hydra_core::tenant_api::decode_usage_rows_json_each_row(&body)
                    .map_err(|e| UsageQueryError::Decode(format!("rows: {e:?}")))?;
            }

            Ok(UsageAggregate {
                totals: totals.totals,
                rows,
                as_of: totals.as_of,
            })
        })
    }

    fn source(&self) -> &'static str {
        "clickhouse"
    }
}

fn open(cfg: &BackendConfig) -> Result<Backend, BackendError> {
    #[cfg(feature = "usage-clickhouse")]
    {
        use std::sync::Arc;

        let url = cfg
            .env
            .get(CLICKHOUSE_URL_ENV)
            .ok_or(BackendError::MissingEnv {
                kind: DESCRIPTOR.kind,
                name: CLICKHOUSE_URL_ENV,
                purpose: URL_PURPOSE,
            })?;
        // Refused at STARTUP, not on the first flush: the alternative is a node that boots, serves
        // traffic and only then fails every usage flush. The credential never leaves either way, so
        // failing early is strictly better than failing late (the transport refuses `https://` too —
        // it has no TLS path, and silently connecting in the clear while the URL promised TLS would
        // leak the credentials).
        if url.trim().starts_with("https://") {
            return Err(BackendError::Invalid {
                kind: DESCRIPTOR.kind,
                message: "HYDRA_CLICKHOUSE_URL uses https:// but this build has no TLS transport \
                          for ClickHouse; refusing to start rather than send credentials and usage \
                          rows in PLAINTEXT — use http:// on a private network or through a tunnel, \
                          or terminate TLS in front of the database"
                    .to_string(),
            });
        }
        Ok(Backend {
            sink: Arc::new(crate::usage::backends::clickhouse::ClickHouseSink::new(
                &url,
                crate::usage::engine::DEFAULT_BATCH_SIZE,
                crate::usage::engine::DEFAULT_FLUSH_SECS,
            )),
            query: Some(Arc::new(ClickHouseUsageQuery::new(&url))),
            notes: "",
        })
    }
    #[cfg(not(feature = "usage-clickhouse"))]
    {
        // The kind is KNOWN (so the error can say which feature to build with) even though this
        // binary cannot serve it — `usage-clickhouse` is not implied by `server`.
        let _ = &cfg.env;
        Err(BackendError::FeatureDisabled {
            kind: DESCRIPTOR.kind,
            feature: DESCRIPTOR.feature,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests: the statement this backend writes, and the transport it writes it with
// ---------------------------------------------------------------------------

#[cfg(feature = "usage-clickhouse")]
#[cfg(test)]
mod statement_tests {
    use super::*;

    #[test]
    fn the_insert_statement_carries_the_dedup_token_before_format() {
        let sql = clickhouse_insert_statement("batch-abc123");
        assert!(
            sql.contains("insert_deduplication_token='batch-abc123'"),
            "the token must reach the statement: {sql}"
        );
        let settings = sql.find("SETTINGS").expect("SETTINGS present");
        let format = sql.find("FORMAT JSONEachRow").expect("FORMAT present");
        assert!(
            settings < format,
            "`SETTINGS` must precede `FORMAT` (ClickHouse rejects the query otherwise), \
             got SETTINGS at {settings} and FORMAT at {format}: {sql}"
        );
        assert!(
            sql.starts_with("INSERT INTO usage_record"),
            "the prefix must stay the table's insert: {sql}"
        );
    }

    #[test]
    fn a_hostile_batch_id_cannot_rewrite_the_statement() {
        // What matters is STRUCTURE, not vocabulary: letters surviving inside the
        // literal is the point (the id is a word), while a quote would END the
        // literal and let a caller append SQL, and a newline would split the
        // statement. Neither can arrive from `new_batch_id`, but this is a
        // string-formatted query, so the filter is load-bearing.
        let sql = clickhouse_insert_statement("a' FORMAT CSV --\ninjected");
        assert_eq!(
            sql.matches('\'').count(),
            2,
            "exactly one quoted literal: {sql}"
        );
        assert!(!sql.contains('\n'), "no newline may survive: {sql}");
        let token = sql
            .split("insert_deduplication_token='")
            .nth(1)
            .and_then(|rest| rest.split('\'').next())
            .expect("token literal");
        assert!(
            !token.is_empty()
                && token
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "only [A-Za-z0-9_-] may survive into the statement, got {token:?}: {sql}"
        );

        // And the token is bounded, so no caller can build a huge statement.
        let long = "x".repeat(500);
        let sql = clickhouse_insert_statement(&long);
        assert_eq!(
            sql.matches('x').count(),
            128,
            "the token must be truncated to 128 chars"
        );
    }
}

/// The transport's contract, driven against a real TCP server that answers with the bytes we choose
/// (no ClickHouse needed): a minimal 200 is accepted, a 5xx keeps the error body, and a server that
/// never answers trips the reader's own deadline instead of hanging the reader forever.
#[cfg(feature = "usage-clickhouse")]
#[cfg(test)]
mod transport_tests {
    use super::*;
    use std::time::Duration;

    fn rec(trace: &str) -> hydra_core::model::UsageRecord {
        crate::usage::testing::record(trace)
    }

    #[cfg(feature = "usage-clickhouse")]
    fn cfg_for(addr: std::net::SocketAddr, connect_ms: u64, io_ms: u64) -> ClickHouseConfig {
        ClickHouseConfig {
            host_port: addr.to_string(),
            auth: None,
            query_params: String::new(),
            tls_requested: false,
            connect_timeout: Duration::from_millis(connect_ms),
            io_timeout: Duration::from_millis(io_ms),
        }
    }

    /// A TCP server that answers every connection with `response` verbatim.
    #[cfg(feature = "usage-clickhouse")]
    async fn spawn_responder(response: &'static str) -> std::net::SocketAddr {
        use tokio::io::AsyncWriteExt as _;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        addr
    }

    /// The bounded reader must still recognise a normal `200` (the pre-fix code
    /// used `read_to_end`; this pins the replacement loop).
    #[cfg(feature = "usage-clickhouse")]
    #[tokio::test]
    async fn a_minimal_200_response_is_accepted() {
        let addr =
            spawn_responder("HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await;
        let cfg = cfg_for(addr, 500, 500);
        insert_batch_clickhouse_http(&cfg, &[rec("a")], "test-batch")
            .await
            .expect("a 200 must be a success");
    }

    /// A non-200 carries ClickHouse's error text and must surface it.
    #[cfg(feature = "usage-clickhouse")]
    #[tokio::test]
    async fn a_5xx_response_surfaces_the_error_body() {
        let addr = spawn_responder(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 60\r\nConnection: close\r\n\r\nCode: 60. DB::Exception: Table usage_record does not exist",
        )
        .await;
        let cfg = cfg_for(addr, 500, 500);
        let msg = insert_batch_clickhouse_http(&cfg, &[rec("a")], "test-batch")
            .await
            .expect_err("5xx must not be treated as success");
        assert!(msg.contains("500"), "{msg}");
        assert!(
            msg.contains("DB::Exception"),
            "error body must reach the caller: {msg}"
        );
    }

    /// A ClickHouse that accepts the connection and then never answers used to
    /// pin the SINGLE flush task forever (`read_to_end` has no deadline), which
    /// silently filled the channel and dropped every subsequent usage record.
    #[cfg(feature = "usage-clickhouse")]
    #[tokio::test]
    async fn a_clickhouse_that_never_answers_times_out() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            if let Ok((sock, _)) = listener.accept().await {
                // Hold the connection open and stay silent.
                tokio::time::sleep(Duration::from_secs(30)).await;
                drop(sock);
            }
        });
        let cfg = cfg_for(addr, 500, 300);
        let started = tokio::time::Instant::now();
        let out = tokio::time::timeout(
            Duration::from_secs(5),
            insert_batch_clickhouse_http(&cfg, &[rec("a")], "test-batch"),
        )
        .await
        .expect("the insert must give up: one stuck flush pinned ALL usage metering on the node");
        let msg = out.expect_err("a black-holed ClickHouse must not report success");
        assert!(msg.contains("timed out"), "unexpected error: {msg}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "must fail fast on its own deadline, took {:?}",
            started.elapsed()
        );
    }
}
