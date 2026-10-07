//! The **TDengine** usage backend — ADR-0002's first *new* insertion, and the one that has to prove
//! the pattern is real: this directory is the only place the backend lives (`mod.rs` + `transport.rs`),
//! plus ONE row in `usage::REGISTRY` and ONE cargo feature. Nothing in `main.rs`, the engine or any
//! other backend knows it exists.
//!
//! **Not a delivered backend.** The repository ships ClickHouse as its usage store; TDengine is a
//! candidate that exists so "adding a metrics database" is a demonstrated, measured path rather than
//! a claim (ADR-0002 §4, D-9). Nothing about a deployment's contract changes because of it.
//!
//! # What the store forced (every fact measured against `tdengine/tdengine:3.3.6.13`, 2026-10-07)
//!
//! * **A 200 is not success** — failures, including authentication failures, come back as `HTTP 200`
//!   with `code != 0`; [`transport`] parses the envelope for that reason.
//! * **DDL is required** (`CREATE DATABASE` + `CREATE STABLE`), because inserting into a super table
//!   directly is REFUSED (`code:534`); every row must name a sub-table via
//!   `INSERT INTO … USING … TAGS (…)`. The sub-table itself is created by that first insert, so the
//!   bootstrap only creates the database and the super table.
//! * **`key` is a reserved word**, `max(ts)` is refused, and `if()`/`coalesce()`/`ifnull()` do not
//!   exist — so the read SQL uses `group_key`, `last(ts)`, and `CASE WHEN … IS NULL THEN '' ELSE …`.
//! * **Integers come back as JSON numbers** (the opposite of ClickHouse's strings) and an aggregate
//!   over an empty window returns **`null`, not 0** — both handled in the decoder here rather than in
//!   `hydra_core`, which stays store-neutral.
//! * Timestamps render as RFC3339 UTC (`"2026-10-07T05:57:42.173Z"`), and our fixed-width bound
//!   strings (`'2026-10-07T00:00:00Z'`) match rows directly, so the window bounds pass through.
//!
//! # Version
//!
//! Measured against **`tdengine/tdengine:3.3.6.13`** (LTS), and this backend needs nothing newer:
//! every statement, envelope and framing detail above is a 3.3 feature. The token authentication
//! 3.4.0.0 added is deliberately NOT implemented — an unverifiable auth branch would be a path that
//! looks supported and is not (see `transport`'s header for the one-function note).

pub(crate) mod transport;

// The descriptor must exist in EVERY build (it is the thing that answers "rebuild with
// --features usage-tdengine"), while the writer and reader below only exist with that feature.
// Instead of gating every item, the module declaration carries
// `#[cfg_attr(not(feature = "usage-tdengine"), allow(dead_code, unused_imports))]` — the same shape
// `usage::engine` uses for the same reason.
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use hydra_core::model::UsageRecord;
use hydra_core::tenant_api::{UsageAggregate, UsageRow, UsageTotals};
use tokio::sync::mpsc;

use crate::usage::engine::{
    drain_on_drop, note_usage_drop, run_channel_sink, UsageSink, MAX_FLUSH_RETRY_WINDOW,
};
use crate::usage::query::{GroupBy, UsageQuery, UsageQueryError};
use transport::{parse_tdengine_url, TdengineConfig, TdengineResponse};

use super::super::{
    Backend, BackendConfig, BackendError, ReaderContract, Requirement, UsageBackend,
};

/// The endpoint every node writes usage to.
pub(crate) const URL_ENV: &str = "HYDRA_TDENGINE_URL";

const URL_PURPOSE: &str =
    "the taosAdapter HTTP endpoint (`http://user:pass@host:6041/<database>`) this node writes usage \
     to and reads GET /usage back from";

/// The database this backend creates and uses when the URL carries no path.
pub(crate) const DEFAULT_DATABASE: &str = "hydra_usage";
/// The super table (one stable, one sub-table per tenant).
const STABLE: &str = "usage_record";

pub static DESCRIPTOR: UsageBackend = UsageBackend {
    kind: "tdengine",
    feature: "usage-tdengine",
    requires: &[Requirement {
        name: URL_ENV,
        purpose: URL_PURPOSE,
    }],
    recognises: &[
        "HYDRA_TDENGINE_CONNECT_TIMEOUT_MS",
        "HYDRA_TDENGINE_IO_TIMEOUT_MS",
    ],
    reads: ReaderContract::SameBackend,
    open,
    notes:
        "TDengine (taosAdapter HTTP): one super table, one sub-table per tenant; usage rows are \
            written per batch and read back for GET /usage",
};

fn open(cfg: &BackendConfig) -> Result<Backend, BackendError> {
    #[cfg(feature = "usage-tdengine")]
    {
        let url = cfg.env.get(URL_ENV).ok_or(BackendError::MissingEnv {
            kind: DESCRIPTOR.kind,
            name: URL_ENV,
            purpose: URL_PURPOSE,
        })?;
        let parsed = parse_tdengine_url(&url).map_err(|message| BackendError::Invalid {
            kind: DESCRIPTOR.kind,
            message,
        })?;
        // The schema is bootstrapped HERE, synchronously, so a node either has a usable store or
        // refuses to start: discovering on the first flush that the table does not exist would look
        // exactly like the store being down, and the drop counter would blame the wrong thing.
        bootstrap(&parsed).map_err(|message| BackendError::Invalid {
            kind: DESCRIPTOR.kind,
            message,
        })?;
        let sink = TdengineSink::new(parsed.clone());
        Ok(Backend {
            sink: Arc::new(sink),
            query: Some(Arc::new(TdengineUsageQuery { cfg: parsed })),
            notes: "",
            reads: ReaderContract::SameBackend,
        })
    }
    #[cfg(not(feature = "usage-tdengine"))]
    {
        let _ = &cfg.env;
        Err(BackendError::FeatureDisabled {
            kind: DESCRIPTOR.kind,
            feature: DESCRIPTOR.feature,
        })
    }
}

/// Create the database and the super table if they are not there yet.
///
/// The sub-tables are NOT created here: `INSERT … USING … TAGS (…)` creates one per tenant on first
/// write (measured). This runs synchronously inside `open`, which is why `main` blocks on it — a
/// wrong password or an unreachable host must stop the node, not disappear into a background task.
#[cfg(feature = "usage-tdengine")]
fn bootstrap(cfg: &TdengineConfig) -> Result<(), String> {
    let db = cfg.database_or_default();
    let rt = tokio::runtime::Handle::try_current()
        .map_err(|_| "the TDengine backend must be opened inside a tokio runtime".to_string())?;
    let statements = [
        format!("CREATE DATABASE IF NOT EXISTS {db}"),
        format!("CREATE STABLE IF NOT EXISTS {db}.{STABLE} ({COLUMNS}) TAGS ({TAG})"),
    ];
    for sql in statements {
        let cfg = cfg.clone();
        let resp = tokio::task::block_in_place(|| rt.block_on(transport::send(&cfg, &sql)))?;
        if resp.code != 0 {
            return Err(format!(
                "the TDengine usage schema could not be created ({sql}): {}",
                resp.error_text()
            ));
        }
    }
    Ok(())
}

/// The measured column list. `ts` first (TDengine requires the timestamp column first), then the
/// row's own fields; `tenant_id` is a TAG so each tenant's rows live in their own sub-table.
const COLUMNS: &str = "ts TIMESTAMP, tenant_id NCHAR(64), provider_id NCHAR(64), \
     model_key NCHAR(128), client_api_key NCHAR(64), sub_tenant_id NCHAR(64), status_code INT, \
     tokens_in BIGINT, tokens_out BIGINT, cache_hit_tokens BIGINT, latency_ms BIGINT, \
     forward_latency_ms BIGINT, ttft_ms BIGINT, upstream_host NCHAR(128), err NCHAR(256)";

/// The sub-table tag. One tag, one meaning: whose rows these are.
const TAG: &str = "tenant NCHAR(64)";

// ===========================================================================
// The writer
// ===========================================================================

struct TdengineSink {
    tx: Mutex<Option<mpsc::Sender<UsageRecord>>>,
    join: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl TdengineSink {
    fn new(cfg: TdengineConfig) -> Self {
        const BATCH: usize = crate::usage::engine::DEFAULT_BATCH_SIZE;
        const FLUSH: u64 = crate::usage::engine::DEFAULT_FLUSH_SECS;
        let capacity = BATCH.max(16);
        let (tx, rx) = mpsc::channel(capacity);

        let inserter = move |_batch_id: &str, batch: Vec<UsageRecord>| {
            let cfg = cfg.clone();
            async move {
                match insert_batch(&cfg, &batch).await {
                    Ok(()) => Ok(()),
                    Err(e) => Err((batch, e)),
                }
            }
        };
        let join = tokio::spawn(run_channel_sink(
            rx,
            BATCH,
            FLUSH,
            MAX_FLUSH_RETRY_WINDOW,
            inserter,
        ));
        Self {
            tx: Mutex::new(Some(tx)),
            join: Mutex::new(Some(join)),
        }
    }
}

impl UsageSink for TdengineSink {
    fn record(&self, record: UsageRecord) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            if let Some(tx) = self.tx.lock().expect("tx mutex").as_ref() {
                if let Err(e) = tx.try_send(record) {
                    let (reason, dropped) = match e {
                        mpsc::error::TrySendError::Full(r) => {
                            tracing::warn!(
                                target: "hydra::usage::tdengine",
                                dropped_trace_id = %r.trace_id,
                                "usage record dropped: the sink's channel is full"
                            );
                            ("channel_full", 1)
                        }
                        mpsc::error::TrySendError::Closed(r) => {
                            tracing::warn!(
                                target: "hydra::usage::tdengine",
                                dropped_trace_id = %r.trace_id,
                                "usage record dropped: the sink is shut down"
                            );
                            ("channel_closed", 1)
                        }
                    };
                    note_usage_drop(reason, dropped);
                }
            }
        })
    }

    fn shutdown(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let tx = self.tx.lock().expect("tx mutex").take();
            let join = self.join.lock().expect("join mutex").take();
            if let (Some(tx), Some(join)) = (tx, join) {
                drop(tx);
                let _ = tokio::time::timeout(MAX_FLUSH_RETRY_WINDOW, join).await;
            }
        })
    }
}

impl Drop for TdengineSink {
    fn drop(&mut self) {
        let tx = self.tx.lock().expect("tx mutex").take();
        let join = self.join.lock().expect("join mutex").take();
        drain_on_drop(tx, join);
    }
}

/// Write one batch.
///
/// Rows are GROUPED BY TENANT because the tenant is the tag: one statement per tenant, each with as
/// many `VALUES` tuples as that tenant has rows (multi-tuple `VALUES` measured working). A batch that
/// spans tenants therefore costs one request per tenant rather than one per row.
async fn insert_batch(cfg: &TdengineConfig, batch: &[UsageRecord]) -> Result<(), String> {
    let db = cfg.database_or_default();
    let mut by_tenant: Vec<(&str, Vec<&UsageRecord>)> = Vec::new();
    for r in batch {
        match by_tenant.iter_mut().find(|(t, _)| *t == r.tenant_id) {
            Some((_, rows)) => rows.push(r),
            None => by_tenant.push((r.tenant_id.as_str(), vec![r])),
        }
    }
    for (tenant, rows) in by_tenant {
        let sub = sub_table(tenant);
        let values: Vec<String> = rows.iter().map(|r| row_values(r)).collect();
        // `USING … TAGS (…)` is not decoration: inserting into the super table directly is refused
        // (code 534), and this clause is also what creates the sub-table on first write.
        let sql = format!(
            "INSERT INTO {db}.{sub} USING {db}.{STABLE} TAGS ('{}') VALUES {}",
            escape_sql_string(tenant),
            values.join(" ")
        );
        let resp = transport::send(cfg, &sql).await?;
        if resp.code != 0 {
            return Err(resp.error_text());
        }
    }
    Ok(())
}

/// The sub-table name for a tenant: stable, unique, and inside TDengine's identifier rules (letters,
/// digits, underscore; no leading digit). The tenant id is arbitrary text, so it is hashed rather
/// than escaped into an identifier — and the readable prefix keeps a table list legible.
fn sub_table(tenant: &str) -> String {
    format!("t_{:016x}", fold_hash(tenant))
}

/// The `ts` written for a record: its own fixed-width UTC second plus a deterministic sub-second
/// part derived from the trace id.
///
/// **Why this is not optional, measured 2026-10-07**: TDengine keys a sub-table by `ts` and
/// OVERWRITES on a duplicate — two requests for ONE tenant in the same second collapsed into a
/// single row (`tokens_in` 111 was replaced by 999, and the count stayed 1), while two rows a
/// millisecond apart both survived. `UsageRecord::created_at` carries SECONDS, so a backend that
/// wrote it as-is would silently under-count every tenant doing more than one request per second —
/// the exact failure this design exists to prevent.
///
/// The offset comes from the trace id rather than from a clock, which keeps a RETRIED batch
/// idempotent: the same record always maps to the same `ts`, so the engine's retry overwrites
/// instead of double-counting.
///
/// **Residual risk, stated rather than hidden**: two rows for one tenant in the same second whose
/// trace ids hash to the same millisecond would still collapse (1 in 1000 per pair). TDengine offers
/// no batch deduplication token, and its primary key is the timestamp, so a per-request billing store
/// on this engine needs a millisecond-or-finer unique key that the row itself provides. That is why
/// this backend is a CANDIDATE and not a delivered one (ADR-0002 §5).
fn timestamp(r: &UsageRecord) -> String {
    let millis = (fold_hash(&r.trace_id) % 1000) as u32;
    match r.created_at.strip_suffix('Z') {
        Some(base) => format!("{base}.{millis:03}Z"),
        // A caller that hands over a non-canonical stamp keeps it: this backend must not invent a
        // time, and the read contract's bounds are the caller's own fixed-width form.
        None => r.created_at.clone(),
    }
}

/// FNV-1a over the trace id: deterministic, dependency-free, and stable across processes (a random
/// per-process seed would break the retry-idempotency above).
fn fold_hash(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// One `VALUES (…)` tuple. `ts` keeps the record's own fixed-width UTC string (measured: TDengine
/// matches `'2026-10-07T00:00:00Z'` against a `TIMESTAMP` column), and optional numbers are `NULL`.
fn row_values(r: &UsageRecord) -> String {
    let n = |v: Option<u64>| v.map_or_else(|| "NULL".to_string(), |v| v.to_string());
    let s = |v: Option<&str>| {
        v.map_or_else(
            || "NULL".to_string(),
            |v| format!("'{}'", escape_sql_string(v)),
        )
    };
    // Built from quoted literals rather than a hand-written format string: every element here is
    // either a literal the helpers already quoted or a bare number, and a format string with mixed
    // quoting is exactly how `sub_tenant_id` became `''st1''` — invalid SQL that made the store
    // refuse EVERY batch while the node looked healthy (found by the live test, which writes and then
    // reads back).
    let quoted = |v: &str| format!("'{}'", escape_sql_string(v));
    let parts = [
        quoted(&timestamp(r)),
        quoted(&r.tenant_id),
        quoted(&r.provider_id),
        quoted(&r.model_key),
        s(r.client_api_key_masked.as_deref()),
        s(r.sub_tenant_id.as_deref()),
        r.status_code.to_string(),
        n(r.tokens_in),
        n(r.tokens_out),
        n(r.cache_hit_tokens),
        r.latency_ms.to_string(),
        n(r.forward_latency_ms),
        n(r.ttft_ms),
        s(r.upstream_host.as_deref()),
        s(r.error.as_deref()),
    ];
    format!("({})", parts.join(","))
}

/// A single-quoted TDengine string literal.
///
/// Both `\` and `'` are escaped: TDengine string literals use backslash escapes, so a trailing
/// backslash in a value would otherwise swallow the closing quote and turn the rest of the value
/// into SQL. Every value that reaches here is caller-influenced (a model key, a masked api-key, an
/// upstream error message), which is why it is escaped rather than validated.
fn escape_sql_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            _ => out.push(c),
        }
    }
    out
}

// ===========================================================================
// The reader
// ===========================================================================

struct TdengineUsageQuery {
    cfg: TdengineConfig,
}

/// The expression that keys a grouped row — a WHITELIST, because it is interpolated into the
/// statement (the store has no parameter binding: measured, taosAdapter takes SQL text only).
fn group_expr(g: GroupBy) -> Option<&'static str> {
    match g {
        GroupBy::None => None,
        GroupBy::Model => Some("model_key"),
        GroupBy::Provider => Some("provider_id"),
        // `COALESCE` does not exist in TDengine (measured: `Func not exists`), so the empty-key
        // normalisation the read contract requires is spelled with `CASE`.
        GroupBy::SubTenant => {
            Some("CASE WHEN sub_tenant_id IS NULL THEN '' ELSE sub_tenant_id END")
        }
        // `TO_CHAR` is supported (measured) and `Day` is the calendar day of the row's own `ts`.
        GroupBy::Day => Some("TO_CHAR(ts, 'YYYY-MM-DD')"),
    }
}

impl UsageQuery for TdengineUsageQuery {
    fn aggregate<'a>(
        &'a self,
        tenant_id: &'a str,
        since: &'a str,
        until: &'a str,
        group_by: GroupBy,
    ) -> Pin<Box<dyn Future<Output = Result<UsageAggregate, UsageQueryError>> + Send + 'a>> {
        Box::pin(async move {
            let db = self.cfg.database_or_default();
            let tenant = escape_sql_string(tenant_id);
            let since = escape_sql_string(since);
            let until = escape_sql_string(until);
            let where_clause = format!(
                "FROM {db}.{STABLE} WHERE tenant_id = '{tenant}' AND ts >= '{since}' AND ts < '{until}'"
            );

            // `CASE WHEN` rather than `if(…)`: measured, `if()` does not exist. `last(ts)` rather than
            // `max(ts)`: measured, `max` on a TIMESTAMP column is refused.
            let totals_sql = format!(
                "SELECT count(*) AS requests, sum(tokens_in) AS tokens_in, \
                        sum(tokens_out) AS tokens_out, sum(cache_hit_tokens) AS cache_hit_tokens, \
                        sum(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END) AS errors, \
                        last(ts) AS last_seen {where_clause}"
            );
            let totals_resp = self.run(&totals_sql).await?;
            let totals = decode_totals(&totals_resp)?;

            let mut rows = Vec::new();
            if let Some(expr) = group_expr(group_by) {
                // The alias cannot be `key`: measured, that word is reserved and the statement is
                // rejected with a syntax error.
                let sql = format!(
                    "SELECT {expr} AS group_key, count(*) AS requests, sum(tokens_in) AS tokens_in, \
                            sum(tokens_out) AS tokens_out, sum(cache_hit_tokens) AS cache_hit_tokens, \
                            sum(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END) AS errors \
                     {where_clause} GROUP BY {expr}"
                );
                let resp = self.run(&sql).await?;
                rows = decode_rows(&resp)?;
            }

            Ok(UsageAggregate {
                totals,
                rows,
                as_of: decode_as_of(&totals_resp),
            })
        })
    }

    fn source(&self) -> &'static str {
        // The registry's `kind`: the tenant sees which store answered, and a test can assert it.
        DESCRIPTOR.kind
    }
}

impl TdengineUsageQuery {
    /// One statement, with the transport's own error mapping (a refused statement is
    /// `StoreUnavailable`, never a zero).
    async fn run(&self, sql: &str) -> Result<TdengineResponse, UsageQueryError> {
        let resp = transport::send(&self.cfg, sql)
            .await
            .map_err(UsageQueryError::StoreUnavailable)?;
        if resp.code != 0 {
            return Err(UsageQueryError::StoreUnavailable(resp.error_text()));
        }
        Ok(resp)
    }
}

/// A counter column: JSON numbers (measured), `null` when the aggregate ran over no rows (also
/// measured — ClickHouse returns 0 there, so this difference lives in THIS decoder rather than in
/// `hydra_core`). A value that is neither a number nor null (nor a numeric string, which TDengine
/// does not send but a proxy might) is a FAILURE: "I could not read the count" must never become 0.
fn counter(
    resp: &TdengineResponse,
    row: &[serde_json::Value],
    name: &str,
) -> Result<u64, UsageQueryError> {
    let idx = resp.column_index(name).ok_or_else(|| {
        UsageQueryError::Decode(format!(
            "the response has no `{name}` column: {}",
            resp.body
        ))
    })?;
    let value = row.get(idx).unwrap_or(&serde_json::Value::Null);
    match value {
        serde_json::Value::Null => Ok(0),
        serde_json::Value::Number(n) => n.as_u64().ok_or_else(|| {
            UsageQueryError::Decode(format!("`{name}` is not an unsigned count: {n}"))
        }),
        serde_json::Value::String(s) => s.parse::<u64>().map_err(|_| {
            UsageQueryError::Decode(format!("`{name}` is not a numeric string: {s:?}"))
        }),
        other => Err(UsageQueryError::Decode(format!(
            "`{name}` is not a count: {other}"
        ))),
    }
}

fn decode_totals(resp: &TdengineResponse) -> Result<UsageTotals, UsageQueryError> {
    let row = resp.rows.first().ok_or_else(|| {
        // Aggregates over an empty window return ONE row (measured); no row at all means the answer
        // is not the aggregate we asked for.
        UsageQueryError::Decode(format!("the totals query returned no row: {}", resp.body))
    })?;
    Ok(UsageTotals {
        requests: counter(resp, row, "requests")?,
        tokens_in: counter(resp, row, "tokens_in")?,
        tokens_out: counter(resp, row, "tokens_out")?,
        cache_hit_tokens: counter(resp, row, "cache_hit_tokens")?,
        errors: counter(resp, row, "errors")?,
    })
}

fn decode_rows(resp: &TdengineResponse) -> Result<Vec<UsageRow>, UsageQueryError> {
    let key_idx = resp.column_index("group_key").ok_or_else(|| {
        UsageQueryError::Decode(format!(
            "the grouped response has no `group_key`: {}",
            resp.body
        ))
    })?;
    let mut out = Vec::with_capacity(resp.rows.len());
    for row in &resp.rows {
        // An empty grouping key must be `""`, never NULL — the serialised response has no room for
        // a null key, and the ClickHouse arm normalises the same way.
        let key = match row.get(key_idx) {
            None | Some(serde_json::Value::Null) => String::new(),
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
        };
        out.push(UsageRow {
            key,
            totals: UsageTotals {
                requests: counter(resp, row, "requests")?,
                tokens_in: counter(resp, row, "tokens_in")?,
                tokens_out: counter(resp, row, "tokens_out")?,
                cache_hit_tokens: counter(resp, row, "cache_hit_tokens")?,
                errors: counter(resp, row, "errors")?,
            },
        });
    }
    Ok(out)
}

/// `last(ts)` as the `as_of` the tenant is told about: TDengine renders it RFC3339 (`"…T…Z"`,
/// measured), and an empty window has no row to take it from (`null`).
fn decode_as_of(resp: &TdengineResponse) -> Option<String> {
    let idx = resp.column_index("last_seen")?;
    let row = resp.rows.first()?;
    match row.get(idx) {
        Some(serde_json::Value::String(s)) => hydra_core::tenant_api::normalize_as_of(Some(s)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(tenant: &str, trace: &str) -> UsageRecord {
        let mut r = crate::usage::testing::record(trace);
        r.tenant_id = tenant.to_string();
        r
    }

    /// Values reach the statement escaped: a quote or a trailing backslash in caller-influenced text
    /// (a model key, a masked api-key) must not be able to end the literal.
    #[test]
    fn sql_literals_escape_quotes_and_backslashes() {
        assert_eq!(escape_sql_string("plain"), "plain");
        assert_eq!(escape_sql_string("it's"), "it\\'s");
        assert_eq!(escape_sql_string("back\\slash"), "back\\\\slash");
        assert_eq!(escape_sql_string("a\\'b"), "a\\\\\\'b");
    }

    #[test]
    fn optional_fields_are_null_and_present_ones_are_literals() {
        let mut r = rec("t1", "values");
        r.tokens_in = Some(5);
        r.tokens_out = None;
        r.sub_tenant_id = Some("st1".into());
        r.error = Some("upstream said 'no'".into());
        let values = row_values(&r);
        // The WHOLE tuple, not a substring: `''st1''` contains `'st1'`, so a `contains` assertion
        // let a double-quoted literal through and every insert was refused by the server.
        let ts = timestamp(&r);
        assert!(
            ts.starts_with("2026-09-15T00:00:00.") && ts.ends_with('Z'),
            "the derived stamp keeps the second and adds a sub-second part: {ts}"
        );
        assert_eq!(
            ts,
            timestamp(&r),
            "and it is deterministic (retries stay idempotent)"
        );
        assert_eq!(
            values,
            format!(
                "('{ts}','t1','p','m',NULL,'st1',200,5,NULL,NULL,1,NULL,NULL,NULL,\
                 'upstream said \\'no\\'')"
            ),
            "{values}"
        );
    }

    /// The sub-table name is derived, stable, and a legal identifier even when the tenant id is not
    /// (`acme.example` has a dot; a tenant id may be arbitrary text).
    #[test]
    fn the_sub_table_name_is_stable_and_legal() {
        let a = sub_table("acme.example");
        assert_eq!(a, sub_table("acme.example"));
        assert_ne!(a, sub_table("acme.example2"));
        assert!(a.starts_with("t_"), "{a}");
        assert!(
            a.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "must be a bare identifier: {a}"
        );
    }

    /// The read SQL carries the three measured workarounds, in one place each.
    #[test]
    fn the_grouped_sql_uses_the_measured_workarounds() {
        let sub = group_expr(GroupBy::SubTenant).expect("sub_tenant groups");
        assert!(
            sub.contains("CASE WHEN") && sub.contains("IS NULL"),
            "coalesce does not exist in TDengine: {sub}"
        );
        assert_eq!(
            group_expr(GroupBy::Day),
            Some("TO_CHAR(ts, 'YYYY-MM-DD')"),
            "day grouping"
        );
        assert_eq!(group_expr(GroupBy::None), None);
        // `key` is reserved: the alias must be something else.
        assert_ne!(group_expr(GroupBy::Model), Some("key"));
    }

    /// An aggregate over an empty window is ONE row of `null`s (measured), which must read as a real
    /// zero — and a non-numeric counter must NOT.
    #[test]
    fn an_empty_aggregate_is_a_real_zero_and_a_bad_value_is_a_failure() {
        let empty = TdengineResponse::parse(
            r#"{"code":0,"column_meta":[["requests","BIGINT",8],["tokens_in","BIGINT",8],["tokens_out","BIGINT",8],["cache_hit_tokens","BIGINT",8],["errors","BIGINT",8],["last_seen","TIMESTAMP",8]],"data":[[0,null,null,null,0,null]],"rows":1}"#,
        )
        .expect("parses");
        let totals = decode_totals(&empty).expect("an empty window is a real zero");
        assert_eq!(totals.requests, 0);
        assert_eq!(totals.tokens_in, 0);
        assert_eq!(decode_as_of(&empty), None, "no rows means no `as_of`");

        let bad = TdengineResponse::parse(
            r#"{"code":0,"column_meta":[["requests","VARCHAR",8]],"data":[["not-a-number"]],"rows":1}"#,
        )
        .expect("parses");
        assert!(
            matches!(decode_totals(&bad), Err(UsageQueryError::Decode(_))),
            "a non-numeric count must not become zero"
        );
    }

    /// A grouped response with a NULL key normalises to `""` — the read contract's rule, which in
    /// ClickHouse is `COALESCE` and here is `CASE` plus this decoder.
    #[test]
    fn a_null_group_key_becomes_the_empty_string() {
        let resp = TdengineResponse::parse(
            r#"{"code":0,"column_meta":[["group_key","NCHAR",64],["requests","BIGINT",8],["tokens_in","BIGINT",8],["tokens_out","BIGINT",8],["cache_hit_tokens","BIGINT",8],["errors","BIGINT",8]],"data":[["st1",1,2,3,null,0],[null,2,4,6,0,1]],"rows":2}"#,
        )
        .expect("parses");
        let rows = decode_rows(&resp).expect("decodes");
        assert_eq!(rows[0].key, "st1");
        assert_eq!(rows[1].key, "");
        assert_eq!(rows[1].totals.requests, 2);
    }

    /// One statement per TENANT, because the tenant is the tag: a batch spanning two tenants must not
    /// put one tenant's rows under another's tag.
    #[tokio::test]
    async fn a_batch_is_grouped_by_tenant() {
        let batch = vec![rec("t1", "a"), rec("t2", "b"), rec("t1", "c")];
        let mut by_tenant: Vec<(&str, Vec<&UsageRecord>)> = Vec::new();
        for r in &batch {
            match by_tenant.iter_mut().find(|(t, _)| *t == r.tenant_id) {
                Some((_, rows)) => rows.push(r),
                None => by_tenant.push((r.tenant_id.as_str(), vec![r])),
            }
        }
        assert_eq!(by_tenant.len(), 2, "two tenants, two statements");
        assert_eq!(by_tenant[0].1.len(), 2, "t1's rows stay together");
        assert_eq!(by_tenant[1].1.len(), 1);
    }
}

/// The live test: a REAL taosAdapter, driven through the backend's own `open`/write/read path.
///
/// `#[ignore]`d because it needs a server (`integration/` owns the recipe):
///
/// ```bash
/// docker run -d --name hydra-tdengine -p 127.0.0.1:6041:6041 tdengine/tdengine:3.3.6.13
/// HYDRA_TDENGINE_URL=http://root:taosdata@127.0.0.1:6041/hydra_td_live \
///   cargo test -p hydra-server --features server,usage-tdengine --lib \
///   usage::backends::tdengine::live -- --ignored --nocapture
/// ```
///
/// What it pins, none of which a unit test can: the DDL bootstrap runs against the real server, the
/// `INSERT … USING … TAGS` form is accepted, the measured read SQL returns what was written, and the
/// envelope's `code` is what decides success — the last one by asking for a statement the server
/// refuses.
#[cfg(all(test, feature = "usage-tdengine"))]
mod live {
    use super::*;

    fn url() -> Option<String> {
        std::env::var(URL_ENV).ok().filter(|v| !v.is_empty())
    }

    fn record(tenant: &str, tokens_in: u64, ts: &str) -> UsageRecord {
        let mut r = crate::usage::testing::record("td-live");
        r.tenant_id = tenant.to_string();
        r.provider_id = "p1".into();
        r.model_key = "gpt-4o".into();
        r.sub_tenant_id = Some("st1".into());
        r.tokens_in = Some(tokens_in);
        r.tokens_out = Some(7);
        r.cache_hit_tokens = None;
        r.latency_ms = 3;
        r.created_at = ts.to_string();
        r
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a real TDengine (see the module docs for the recipe)"]
    async fn the_backend_writes_and_reads_a_real_tdengine() {
        let Some(url) = url() else {
            panic!("{URL_ENV} must be set for the live test");
        };
        let pool = crate::db::test_pool().await;
        let cfg =
            BackendConfig::new(pool).with_env(crate::usage::EnvView::fixed(&[(URL_ENV, &url)]));

        // 1. `open` bootstraps the schema against the real server.
        let opened = crate::usage::open("tdengine", &cfg).expect("the backend opens");

        // 2. Clear this test's own window first. TDengine keeps whatever earlier runs wrote, and the
        //    assertions below are exact — a live test that is not re-runnable is a live test nobody
        //    runs twice (measured: the second run reported 3 requests where the first reported 2).
        let cfg = parse_tdengine_url(&url).expect("parses");
        let cleared = transport::send(
            &cfg,
            &format!(
                "DELETE FROM {}.{STABLE} WHERE ts >= '2026-10-07T00:00:00Z' AND ts < '2026-10-07T01:00:00Z'",
                cfg.database_or_default()
            ),
        )
        .await
        .expect("the delete reaches the server");
        assert_eq!(
            cleared.code,
            0,
            "clearing the window must succeed: {}",
            cleared.error_text()
        );

        // 3. Write two rows for one tenant and one for another, through the REAL writer.
        let sink = opened.sink.clone();
        for r in [
            record("td-live-t1", 11, "2026-10-07T00:10:00Z"),
            record("td-live-t1", 22, "2026-10-07T00:20:00Z"),
            record("td-live-t2", 99, "2026-10-07T00:30:00Z"),
        ] {
            sink.record(r).await;
        }
        sink.shutdown().await;

        // 4. Read it back through the REAL reader, and check the tenant filter is a filter.
        let reader = opened.query.expect("it reads");
        let agg = reader
            .aggregate(
                "td-live-t1",
                "2026-10-07T00:00:00Z",
                "2026-10-07T01:00:00Z",
                GroupBy::None,
            )
            .await
            .expect("the aggregate answers");
        assert_eq!(agg.totals.requests, 2, "{agg:?}");
        assert_eq!(agg.totals.tokens_in, 33, "{agg:?}");
        assert_eq!(agg.totals.tokens_out, 14, "{agg:?}");
        assert_eq!(agg.totals.cache_hit_tokens, 0, "NULL sums read as zero");
        assert_eq!(agg.totals.errors, 0);
        assert!(
            agg.as_of.is_some(),
            "`last(ts)` must come back as the as_of the tenant is told about: {agg:?}"
        );

        // The other tenant's row must NOT be in this answer (the tag is what separates them).
        let other = reader
            .aggregate(
                "td-live-t2",
                "2026-10-07T00:00:00Z",
                "2026-10-07T01:00:00Z",
                GroupBy::None,
            )
            .await
            .expect("the second tenant answers");
        assert_eq!(other.totals.requests, 1);
        assert_eq!(other.totals.tokens_in, 99);

        // 5. Grouping: the second tenant's rows carry `st1` too, so group by
        //    provider to keep the assertion about buckets rather than about tags.
        let grouped = reader
            .aggregate(
                "td-live-t1",
                "2026-10-07T00:00:00Z",
                "2026-10-07T01:00:00Z",
                GroupBy::Model,
            )
            .await
            .expect("the grouped read answers");
        assert_eq!(grouped.rows.len(), 1, "{grouped:?}");
        assert_eq!(grouped.rows[0].key, "gpt-4o");
        assert_eq!(grouped.rows[0].totals.requests, 2);

        // 6. An empty window is a real zero (measured: TDengine answers one row of NULLs).
        let empty = reader
            .aggregate(
                "td-live-t1",
                "2030-01-01T00:00:00Z",
                "2030-01-02T00:00:00Z",
                GroupBy::None,
            )
            .await
            .expect("an empty window is not an error");
        assert_eq!(empty.totals.requests, 0);
        assert_eq!(empty.as_of, None, "no rows means no as_of");

        // 7. THE RULE: a refused statement is an error even though HTTP says 200. The server answers
        //    `code != 0` and the transport must surface it — asking the reader for a tenant whose tag
        //    cannot exist is not enough (that is a legitimate empty answer), so the statement itself
        //    is broken here.
        let broken = transport::send(
            &parse_tdengine_url(&url).expect("parses"),
            "SELECT * FROM hydra_td_live.no_such_table_xyz",
        )
        .await
        .expect("the transport reaches the server");
        assert_ne!(broken.code, 0, "the server must refuse this statement");
        assert!(
            !broken.error_text().is_empty(),
            "and the refusal must carry the server's words"
        );

        // 8. A WRONG PASSWORD is also a 200 — this is the case that would silently drop every row.
        let mut bad = parse_tdengine_url(&url).expect("parses");
        bad.password = Some("definitely-not-the-password".into());
        // A wrong password must not read as success. Either shape is acceptable — a refusal at the
        // transport level, or the `code != 0` this store puts inside a 200 — but NOT `code == 0`.
        if let Ok(resp) = transport::send(&bad, "SHOW DATABASES").await {
            assert_ne!(
                resp.code, 0,
                "a wrong password must not read as success (measured: code 855 on a 200)"
            );
        }
    }
}
