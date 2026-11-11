//! The **GreptimeDB** usage backend — ADR-0002's second measured insertion, the same
//! directory-one-module shape as `tdengine` and `clickhouse`: this directory is the only place the
//! backend lives (`mod.rs` + `transport.rs`), plus ONE row in `usage::REGISTRY` and ONE cargo
//! feature. Nothing in `main.rs`, the engine or any other backend knows it exists.
//!
//! **Not implied by `server`.** The repository's default build does not gain the feature; the
//! optional-features job is where this runs. The store is a real GreptimeDB HTTP SQL endpoint.
//!
//! # What the store forced (every fact measured against `greptime/greptimedb:v1.3.0`, 2026-11-11)
//!
//! * **DDL is required and is bootstrapped here**, synchronously, so a node either has a usable
//!   store or refuses to start (`open`). The canonical GreptimeDB shape is accepted by the measured
//!   server: `CREATE DATABASE IF NOT EXISTS <db>`, a `usage_record` table with
//!   `ts TIMESTAMP TIME INDEX` and `PRIMARY KEY (tenant_id, trace_id)`, `append_mode='false'`,
//!   `ttl='90d'`. **No runtime DDL fallback is implemented** because none is needed (a live server
//!   accepts exactly this).
//! * **Retry dedup is the primary key.** Two identical `(tenant_id, trace_id, ts)` inserts collapse
//!   to one row (measured: `count(*)` stays 1 and `sum(tokens_in)` keeps its value), so a re-sent
//!   batch — the "response was lost after the commit" case — cannot double-count usage.
//! * **`trace_id` is load-bearing for that dedup**: if it were empty for two same-tenant rows at the
//!   same `ts`, they would fold into one. The writer derives a deterministic FNV-1a fallback key
//!   over the row when `trace_id` is empty, and writes it, so the dedup identity is never blank.
//! * **There is no HTTP bind parameter**, so `tenant_id` is an escaped literal, the read window is
//!   the `ts` time index expressed as bare epoch-millisecond bounds, and every grouping expression is
//!   a whitelist.
//!
//! # The write path
//!
//! The shared [`run_channel_sink`]/[`drain_on_drop`] engine, exactly like the other backends; only
//! the insert differs — a multi-row `INSERT … VALUES (…),(…)`, chunked at 512 rows or when the final
//! URL-encoded body would exceed 3.5 MiB, whichever comes first.
//!
//! # The read path
//!
//! `GET /usage` is answered from the same table. The window is
//! `ts >= <since_epoch_ms> AND ts < <until_epoch_ms>` — a range predicate on the `ts` TIME INDEX —
//! while `tenant_id` stays an escaped literal and the groupings stay whitelisted; [`GreptimedbResponse`]
//! is decoded by this module (schema-name → index), so a shape drift or a non-numeric counter is a
//! FAILURE, never a zero.
//!
//! **Why the `ts` window is equivalent to the old `created_at` string window.** Both `created_at` and
//! `ts` come from the same row and the same canonical formatter: `created_at` is the fixed-width,
//! zero-padded all-UTC `YYYY-MM-DDTHH:MM:SSZ` string, and `ts` is exactly `epoch_millis(created_at)`.
//! On canonical rows the epoch-millisecond map is strictly order-preserving, so
//! `created_at >= s AND created_at < u` selects the same set as `ts >= ms(s) AND ts < ms(u)` (the
//! window contract already normalises `since`/`until` to that canonical form, so no lenient parsing
//! is implied). The only row class that differs is the writer's non-canonical fallback: when
//! `created_at` cannot be parsed, the writer stores `ts = 0`. Such a row is **invisible to every
//! real window** under the `ts` predicate (its epoch is 1970-01-01T00:00:00Z) — the same class of
//! "not in the measured window" as the old string predicate, which also excluded junk that did not
//! sort inside `[s, u)`. That fallback is a write-side degradation, not a read-side one, and it is
//! recorded in the module's live-test notes rather than silently widened away.

pub(crate) mod transport;

// The descriptor must exist in EVERY build (it is the thing that answers "rebuild with
// --features usage-greptimedb"), while the writer and reader below only exist with that feature.
// Instead of gating every item, the module declaration in `backends/mod.rs` carries
// `#[cfg_attr(not(feature = "usage-greptimedb"), allow(dead_code, unused_imports))]` — the same
// shape `usage::backends::tdengine` uses for the same reason.
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
use transport::{parse_greptimedb_url, GreptimedbConfig, GreptimedbResponse};

use super::super::{
    Backend, BackendConfig, BackendError, ReaderContract, Requirement, UsageBackend,
};

/// The endpoint every node writes usage to.
pub(crate) const URL_ENV: &str = "HYDRA_GREPTIMEDB_URL";

const URL_PURPOSE: &str =
    "the GreptimeDB HTTP SQL endpoint (`http://user:pass@host:4000/<database>`) this node writes \
     usage to and reads GET /usage back from";

/// The database this backend creates and uses when the URL carries no path.
pub(crate) const DEFAULT_DATABASE: &str = "hydra_usage";
/// The table that holds the usage rows.
const TABLE: &str = "usage_record";

pub static DESCRIPTOR: UsageBackend = UsageBackend {
    kind: "greptimedb",
    feature: "usage-greptimedb",
    requires: &[Requirement {
        name: URL_ENV,
        purpose: URL_PURPOSE,
    }],
    recognises: &[
        "HYDRA_GREPTIMEDB_CONNECT_TIMEOUT_MS",
        "HYDRA_GREPTIMEDB_IO_TIMEOUT_MS",
        "HYDRA_GREPTIMEDB_QUERY_TIMEOUT_MS",
    ],
    reads: ReaderContract::SameBackend,
    open,
    notes:
        "GreptimeDB HTTP SQL API: one usage_record table (primary key on tenant + trace id, \
            merge-on-retry, 90d TTL); usage rows are written per batch and read back for GET /usage",
};

fn open(cfg: &BackendConfig) -> Result<Backend, BackendError> {
    #[cfg(feature = "usage-greptimedb")]
    {
        let url = cfg.env.get(URL_ENV).ok_or(BackendError::MissingEnv {
            kind: DESCRIPTOR.kind,
            name: URL_ENV,
            purpose: URL_PURPOSE,
        })?;
        // Refused at STARTUP, not on the first flush. The transport has no TLS path, so an
        // `https://` URL would otherwise send credentials and usage rows in the clear.
        let parsed = parse_greptimedb_url(&url).map_err(|message| BackendError::Invalid {
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
        let sink = GreptimedbSink::new(parsed.clone());
        // The reader has its own deadline, independent of the writer's: a tenant waiting on a slow
        // query must not inherit the flush task's allowance, and vice versa.
        let mut qcfg = parsed;
        qcfg.io_timeout = transport::env_millis("HYDRA_GREPTIMEDB_QUERY_TIMEOUT_MS", 5_000);
        Ok(Backend {
            sink: Arc::new(sink),
            query: Some(Arc::new(GreptimedbUsageQuery { cfg: qcfg })),
            notes: "",
            reads: ReaderContract::SameBackend,
        })
    }
    #[cfg(not(feature = "usage-greptimedb"))]
    {
        let _ = &cfg.env;
        Err(BackendError::FeatureDisabled {
            kind: DESCRIPTOR.kind,
            feature: DESCRIPTOR.feature,
        })
    }
}

/// Create the database and the table if they are not there yet.
///
/// Runs synchronously inside `open`, which is why `main` blocks on it — a wrong password or an
/// unreachable host must stop the node, not disappear into a background task.
#[cfg(feature = "usage-greptimedb")]
fn bootstrap(cfg: &GreptimedbConfig) -> Result<(), String> {
    let db = cfg.database_or_default();
    let rt = tokio::runtime::Handle::try_current().map_err(|_| {
        "the GreptimeDB backend must be opened inside a tokio runtime (it bootstraps its schema \
         synchronously in `open`)"
            .to_string()
    })?;
    // `block_in_place` panics on a current-thread runtime. Refuse with an actionable message rather
    // than let the node abort on a runtime detail it could have been told about: the schema bootstrap
    // is synchronous by design (see above), and only the multi-thread runtime can host it.
    if !matches!(
        rt.runtime_flavor(),
        tokio::runtime::RuntimeFlavor::MultiThread
    ) {
        return Err(
            "the GreptimeDB backend bootstraps its schema synchronously in `open`, which needs the \
             multi-thread tokio runtime; this node runs on a current-thread runtime, so starting it \
             would panic inside `block_in_place` — use a multi-thread runtime (or create the \
             GreptimeDB schema out of band)"
                .to_string(),
        );
    }
    // `CREATE DATABASE` must NOT be addressed to the database it is creating. `transport::send`
    // always builds `/v1/sql?db=<database_or_default>`, so sending the statement with the configured
    // database asks GreptimeDB to run it INSIDE a database that does not exist yet — measured against
    // `greptime/greptimedb:v1.3.0` as:
    //   greptimedb refused the statement (status HTTP/1.1 400 Bad Request):
    //   {"code":4004,"error":"Database not found: hydra_usage"}
    // (It appeared to work in the first tests only because a previous run had already created the
    // database.) The database is therefore created in GreptimeDB's default `public` database; the
    // table DDL then targets the configured one.
    let create_database = format!("CREATE DATABASE IF NOT EXISTS {db}");
    let mut create_db_cfg = cfg.clone();
    create_db_cfg.database = Some("public".to_string());
    let statements: [(&GreptimedbConfig, String); 2] = [
        (&create_db_cfg, create_database),
        (cfg, CREATE_TABLE.to_string()),
    ];
    for (target, sql) in statements {
        let target = target.clone();
        let resp = tokio::task::block_in_place(|| rt.block_on(transport::send(&target, &sql)))?;
        if resp.is_failure() {
            return Err(format!(
                "the GreptimeDB usage schema could not be created ({sql}): {}",
                resp.error_text()
            ));
        }
        if resp.truncated {
            return Err(format!(
                "the GreptimeDB schema DDL answer exceeded {} bytes",
                transport::MAX_GREPTIMEDB_RESPONSE
            ));
        }
        // A DDL reply carries `output[0].affectedrows` (measured). Its absence means the body is not
        // the acknowledgement we asked for — refuse rather than assume an empty schema was created.
        if resp.affected_rows.is_none() {
            return Err(format!(
                "the GreptimeDB DDL answer for `{sql}` carried no affectedrows: {}",
                resp.body
            ));
        }
    }
    Ok(())
}

/// The canonical measured DDL. Held as one constant so the integration parity test and the writer's
/// column list can be compared against a single owner.
const CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS usage_record (\
     tenant_id STRING, provider_id STRING, model_key STRING, client_api_key STRING, \
     user_id STRING, user_group_id STRING, api_key_id STRING, status_code UINT32, \
     tokens_in UINT64, tokens_out UINT64, cache_hit_tokens UINT64, latency_ms UINT64, \
     forward_latency_ms UINT64, ttft_ms UINT64, upstream_host STRING, err STRING, \
     trace_id STRING, created_at STRING, ts TIMESTAMP TIME INDEX, \
     PRIMARY KEY (tenant_id, trace_id)) WITH ('append_mode'='false','ttl'='90d')";

/// The columns the INSERT names, in declaration order. `ts` is the time index derived from
/// `created_at`; `trace_id` is the dedup identity (see [`dedup_key`]).
const INSERT_COLUMNS: &str = "tenant_id, provider_id, model_key, client_api_key, user_id, \
     user_group_id, api_key_id, status_code, tokens_in, tokens_out, cache_hit_tokens, latency_ms, \
     forward_latency_ms, ttft_ms, upstream_host, err, trace_id, created_at, ts";

// ===========================================================================
// The writer
// ===========================================================================

struct GreptimedbSink {
    tx: Mutex<Option<mpsc::Sender<UsageRecord>>>,
    join: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl GreptimedbSink {
    fn new(cfg: GreptimedbConfig) -> Self {
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

impl UsageSink for GreptimedbSink {
    fn record(&self, record: UsageRecord) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            if let Some(tx) = crate::lock_gate(&self.tx).as_ref() {
                if let Err(e) = tx.try_send(record) {
                    let (reason, dropped) = match e {
                        mpsc::error::TrySendError::Full(r) => {
                            tracing::warn!(
                                target: "hydra::usage::greptimedb",
                                dropped_trace_id = %r.trace_id,
                                "usage record dropped: the sink's channel is full"
                            );
                            ("channel_full", 1)
                        }
                        mpsc::error::TrySendError::Closed(r) => {
                            tracing::warn!(
                                target: "hydra::usage::greptimedb",
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
            let tx = crate::lock_gate(&self.tx).take();
            let join = crate::lock_gate(&self.join).take();
            if let (Some(tx), Some(join)) = (tx, join) {
                drop(tx);
                let _ = tokio::time::timeout(MAX_FLUSH_RETRY_WINDOW, join).await;
            }
        })
    }
}

impl Drop for GreptimedbSink {
    fn drop(&mut self) {
        let tx = crate::lock_gate(&self.tx).take();
        let join = crate::lock_gate(&self.join).take();
        drain_on_drop(tx, join);
    }
}

/// The largest final URL-encoded request body the writer will send in one INSERT: ~3.5 MiB.
const MAX_INSERT_BODY_BYTES: usize = 3_670_016;
/// The largest number of rows in one INSERT.
const MAX_INSERT_ROWS: usize = 512;

/// Write one batch, split into chunks of at most [`MAX_INSERT_ROWS`] rows and at most
/// [`MAX_INSERT_BODY_BYTES`] of URL-encoded request body.
async fn insert_batch(cfg: &GreptimedbConfig, batch: &[UsageRecord]) -> Result<(), String> {
    for chunk in chunk_batches(batch, MAX_INSERT_BODY_BYTES, MAX_INSERT_ROWS) {
        let sql = build_insert(&chunk);
        let resp = transport::send(cfg, &sql).await?;
        if resp.truncated {
            return Err(format!(
                "the GreptimeDB INSERT answer exceeded {} bytes",
                transport::MAX_GREPTIMEDB_RESPONSE
            ));
        }
        if resp.is_failure() {
            return Err(resp.error_text());
        }
        // An INSERT reply carries `output[0].affectedrows` (measured); a body without it is not the
        // acknowledgement we asked for, so the batch is retried rather than assumed written.
        if resp.affected_rows.is_none() {
            return Err(format!(
                "the GreptimeDB INSERT answer carried no affectedrows: {}",
                resp.body
            ));
        }
    }
    Ok(())
}

/// Split `rows` into chunks of at most `max_rows` rows, further split whenever adding the next row
/// would make the final URL-encoded body exceed `max_body`.
///
/// The `max_body` measure is the FINAL encoded body (`sql=<encoded>`), not the raw SQL: an INSERT
/// with quotes and commas in it grows when encoded, and GreptimeDB's own request limit applies to
/// what is actually sent.
///
/// **Single pass, and exact.** `url_encode` maps each byte of its input independently, so the
/// encoded length of `prefix + tuple_1 + "," + … + tuple_n` is EXACTLY
/// `encode(prefix).len() + Σ encode(tuple_i).len() + (n-1)·encode(",").len()`. Each tuple is encoded
/// once (not per candidate), the fixed prefix and separator lengths are computed once, and the
/// running total is advanced by that exact arithmetic — O(n) work, no per-row rebuild and no
/// full-statement re-encode, and byte-identical split decisions to the previous implementation.
fn chunk_batches(rows: &[UsageRecord], max_body: usize, max_rows: usize) -> Vec<Vec<&UsageRecord>> {
    // The fixed statement head and the tuple separator, both encoded once. The separator is not
    // hardcoded to one byte: `url_encode` is the single owner of what a byte costs, and comma is
    // NOT in its unreserved set — asking the encoder keeps this exact even if that set changes.
    let prefix = format!("INSERT INTO {TABLE} ({INSERT_COLUMNS}) VALUES ");
    let prefix_len = "sql=".len() + transport::url_encode(&prefix).len();
    let separator_len = transport::url_encode(",").len();

    let tuple_lens: Vec<usize> = rows
        .iter()
        .map(|r| transport::url_encode(&row_values(r)).len())
        .collect();

    let mut out: Vec<Vec<&UsageRecord>> = Vec::new();
    let mut current: Vec<&UsageRecord> = Vec::new();
    let mut current_len = 0usize;
    for (row, tuple_len) in rows.iter().zip(tuple_lens) {
        let candidate_len = if current.is_empty() {
            prefix_len + tuple_len
        } else {
            current_len + separator_len + tuple_len
        };
        if !current.is_empty() && (current.len() >= max_rows || candidate_len > max_body) {
            out.push(std::mem::take(&mut current));
            current_len = prefix_len + tuple_len;
            current.push(row);
        } else {
            current_len = candidate_len;
            current.push(row);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// One `INSERT INTO usage_record (…) VALUES (…),(…)` statement.
fn build_insert(rows: &[&UsageRecord]) -> String {
    let values: Vec<String> = rows.iter().map(|r| row_values(r)).collect();
    format!(
        "INSERT INTO {TABLE} ({INSERT_COLUMNS}) VALUES {}",
        values.join(",")
    )
}

/// One `VALUES (…)` tuple. Numeric counters are bare integers; absent optionals are `NULL` (never
/// `''`); every string is single-quoted with `'` doubled.
fn row_values(r: &UsageRecord) -> String {
    let n = |v: Option<u64>| v.map_or_else(|| "NULL".to_string(), |v| v.to_string());
    let s = |v: Option<&str>| {
        v.map_or_else(
            || "NULL".to_string(),
            |v| format!("'{}'", escape_sql_string(v)),
        )
    };
    let quoted = |v: &str| format!("'{}'", escape_sql_string(v));
    let parts = [
        quoted(&r.tenant_id),
        quoted(&r.provider_id),
        quoted(&r.model_key),
        s(r.client_api_key_masked.as_deref()),
        s(r.user_id.as_deref()),
        s(r.user_group_id.as_deref()),
        s(r.api_key_id.as_deref()),
        r.status_code.to_string(),
        n(r.tokens_in),
        n(r.tokens_out),
        n(r.cache_hit_tokens),
        r.latency_ms.to_string(),
        n(r.forward_latency_ms),
        n(r.ttft_ms),
        s(r.upstream_host.as_deref()),
        s(r.error.as_deref()),
        quoted(&dedup_key(r)),
        quoted(&r.created_at),
        epoch_millis(&r.created_at).unwrap_or(0).to_string(),
    ];
    format!("({})", parts.join(","))
}

/// A single-quoted GreptimeDB string literal: `'` is doubled, which is the whole rule for standard
/// SQL string literals. Every value that reaches here is caller-influenced (a model key, a masked
/// api-key, an upstream error message), which is why it is escaped rather than validated.
fn escape_sql_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '\'' => out.push_str("''"),
            _ => out.push(c),
        }
    }
    out
}

/// The dedup identity written to `trace_id`.
///
/// A non-empty `trace_id` IS the key, verbatim: one request = one record = one stable key, and a
/// retry re-sends the same record so the key is byte-for-byte unchanged. An empty `trace_id` would
/// otherwise make two same-tenant rows at the same `ts` fold into one, so it falls back to a
/// deterministic FNV-1a hex over the row's identifying fields — a retry of the same row recomputes
/// the same key, while two different events differ in at least one field.
fn dedup_key(r: &UsageRecord) -> String {
    if !r.trace_id.is_empty() {
        return r.trace_id.clone();
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |s: &str| {
        for b in s.as_bytes() {
            hash ^= u64::from(*b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    feed(&r.tenant_id);
    feed("|");
    feed(&r.created_at);
    feed("|");
    feed(&r.status_code.to_string());
    feed("|");
    feed(&r.tokens_in.unwrap_or(0).to_string());
    feed("|");
    feed(&r.tokens_out.unwrap_or(0).to_string());
    feed("|");
    feed(&r.cache_hit_tokens.unwrap_or(0).to_string());
    feed("|");
    feed(&r.latency_ms.to_string());
    format!("{hash:016x}")
}

/// Parse the canonical fixed-width `YYYY-MM-DDTHH:MM:SSZ` timestamp to epoch milliseconds.
///
/// Pure Rust (no clock, no dependency): the row's own string is the only input, so the derived `ts`
/// is deterministic and a retry writes the same value. A fractional part, when present, is folded to
/// milliseconds; a value that is not the canonical form yields `None` (the writer then writes `0`).
fn epoch_millis(created_at: &str) -> Option<i64> {
    let b = created_at.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |s: &[u8]| -> Option<i64> {
        let mut v: i64 = 0;
        for &d in s {
            if !d.is_ascii_digit() {
                return None;
            }
            v = v * 10 + i64::from(d - b'0');
        }
        Some(v)
    };
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let year = num(&b[0..4])?;
    let month = num(&b[5..7])?;
    let day = num(&b[8..10])?;
    let hour = num(&b[11..13])?;
    let minute = num(&b[14..16])?;
    let second = num(&b[17..19])?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // A fractional part (`…SS.123Z`) is folded to milliseconds.
    let mut fraction_ms = 0i64;
    if let Some(frac) = created_at[19..].strip_prefix('.') {
        let digits: String = frac
            .chars()
            .take_while(char::is_ascii_digit)
            .take(3)
            .collect();
        if !digits.is_empty() {
            let mut v = 0i64;
            for c in digits.chars() {
                v = v * 10 + i64::from(c as u8 - b'0');
            }
            for _ in digits.len()..3 {
                v *= 10;
            }
            fraction_ms = v;
        }
    }
    let days = days_from_civil(year, month, day);
    Some((days * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000 + fraction_ms)
}

/// Days from 1970-01-01 to `y-m-d` (Howard Hinnant's civil-date algorithm). `m` in 1..=12.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ===========================================================================
// The reader
// ===========================================================================

struct GreptimedbUsageQuery {
    cfg: GreptimedbConfig,
}

/// The expression that keys a grouped row — a WHITELIST, because it is interpolated into the
/// statement (the store has no parameter binding).
fn group_expr(g: GroupBy) -> Option<&'static str> {
    match g {
        GroupBy::None => None,
        GroupBy::Model => Some("model_key"),
        GroupBy::Provider => Some("provider_id"),
        GroupBy::User => Some("coalesce(user_id, '')"),
        GroupBy::UserGroup => Some("coalesce(user_group_id, '')"),
        GroupBy::ApiKey => Some("coalesce(api_key_id, '')"),
        GroupBy::Day => Some("substr(created_at, 1, 10)"),
    }
}

impl UsageQuery for GreptimedbUsageQuery {
    fn aggregate<'a>(
        &'a self,
        tenant_id: &'a str,
        since: &'a str,
        until: &'a str,
        group_by: GroupBy,
    ) -> Pin<Box<dyn Future<Output = Result<UsageAggregate, UsageQueryError>> + Send + 'a>> {
        Box::pin(async move {
            // The window bounds arrive in the canonical `%Y-%m-%dT%H:%M:%SZ` form (the `UsageQuery`
            // contract normalises them, see `tenant_api::time_bound`), so `epoch_millis` is exact and
            // this `ts` window is equivalent to the old `created_at` string window on canonical rows
            // (see the module docs for the equivalence and the `ts=0` fallback note). A bound that is
            // NOT canonical is a refusal, never a silently widened or zeroed window.
            let since_ms = epoch_millis(since).ok_or_else(|| {
                UsageQueryError::Decode(format!(
                    "the usage window start {since:?} is not the canonical `%Y-%m-%dT%H:%M:%SZ` form"
                ))
            })?;
            let until_ms = epoch_millis(until).ok_or_else(|| {
                UsageQueryError::Decode(format!(
                    "the usage window end {until:?} is not the canonical `%Y-%m-%dT%H:%M:%SZ` form"
                ))
            })?;
            let tenant = escape_sql_string(tenant_id);
            let where_clause = format!(
                "FROM {TABLE} WHERE tenant_id = '{tenant}' AND ts >= {since_ms} AND ts < {until_ms}"
            );

            let totals_sql = format!(
                "SELECT count(*) AS requests, sum(tokens_in) AS tokens_in, \
                        sum(tokens_out) AS tokens_out, sum(cache_hit_tokens) AS cache_hit_tokens, \
                        sum(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END) AS errors, \
                        max(created_at) AS last_seen {where_clause}"
            );
            let totals_resp = self.run(&totals_sql).await?;
            let totals = decode_totals(&totals_resp)?;

            let mut rows = Vec::new();
            if let Some(expr) = group_expr(group_by) {
                let sql = format!(
                    "SELECT {expr} AS group_key, count(*) AS requests, sum(tokens_in) AS tokens_in, \
                            sum(tokens_out) AS tokens_out, \
                            sum(cache_hit_tokens) AS cache_hit_tokens, \
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

impl GreptimedbUsageQuery {
    /// One statement, with the transport's own error mapping (a refused statement is
    /// `StoreUnavailable`, never a zero; a truncated answer is `ResultTooLarge`, never a decode
    /// failure).
    async fn run(&self, sql: &str) -> Result<GreptimedbResponse, UsageQueryError> {
        let resp = transport::send(&self.cfg, sql)
            .await
            .map_err(UsageQueryError::StoreUnavailable)?;
        if resp.truncated {
            return Err(UsageQueryError::ResultTooLarge(
                transport::MAX_GREPTIMEDB_RESPONSE,
            ));
        }
        if resp.is_failure() {
            return Err(UsageQueryError::StoreUnavailable(resp.error_text()));
        }
        Ok(resp)
    }
}

/// A counter column: JSON numbers or numeric strings, `null` when the aggregate ran over no rows
/// (GreptimeDB returns `null` for an empty `sum`, which must read as a real zero). A value that is
/// neither is a FAILURE: "I could not read the count" must never become 0.
fn counter(
    resp: &GreptimedbResponse,
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

fn decode_totals(resp: &GreptimedbResponse) -> Result<UsageTotals, UsageQueryError> {
    let row = resp.rows.first().ok_or_else(|| {
        // A bare aggregate returns ONE row; no row at all means the answer is not what we asked for.
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

fn decode_rows(resp: &GreptimedbResponse) -> Result<Vec<UsageRow>, UsageQueryError> {
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

/// `max(created_at)` as the `as_of` the tenant is told about, normalised so an empty set (`null` or
/// `""`) becomes `None`.
fn decode_as_of(resp: &GreptimedbResponse) -> Option<String> {
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

    /// Values reach the statement escaped: a quote in caller-influenced text must not be able to end
    /// the literal.
    #[test]
    fn sql_literals_escape_quotes() {
        assert_eq!(escape_sql_string("plain"), "plain");
        assert_eq!(escape_sql_string("it's"), "it''s");
        assert_eq!(escape_sql_string("a'b'c"), "a''b''c");
    }

    #[test]
    fn optional_fields_are_null_and_present_ones_are_literals() {
        let mut r = rec("t1", "values");
        r.tokens_in = Some(5);
        r.tokens_out = None;
        r.user_id = Some("u1".into());
        r.error = Some("upstream said 'no'".into());
        let values = row_values(&r);
        let ts = epoch_millis(&r.created_at).expect("the harness stamp parses");
        // The WHOLE tuple, not a substring: `''u1''` contains `'u1'`, so a `contains` assertion
        // would let a double-quoted literal through.
        assert_eq!(
            values,
            format!(
                "('t1','p','m',NULL,'u1',NULL,NULL,200,5,NULL,NULL,1,NULL,NULL,NULL,\
                 'upstream said ''no''','values','{}',{ts})",
                r.created_at
            ),
            "{values}"
        );
    }

    /// A non-empty trace id IS the dedup key; an empty one becomes a deterministic, non-empty hex.
    #[test]
    fn an_empty_trace_id_gets_a_deterministic_fallback_key() {
        let mut a = rec("t1", "");
        a.created_at = "2026-09-15T00:00:00Z".into();
        let b = a.clone();
        assert_eq!(
            dedup_key(&a),
            dedup_key(&b),
            "a retry must compute the same key"
        );
        assert!(!dedup_key(&a).is_empty());
        assert!(
            dedup_key(&a).chars().all(|c| c.is_ascii_hexdigit()),
            "the fallback is hex: {}",
            dedup_key(&a)
        );
        let mut other = a.clone();
        other.tokens_in = Some(999);
        assert_ne!(
            dedup_key(&a),
            dedup_key(&other),
            "a different event must not fold"
        );
        assert_eq!(dedup_key(&rec("t1", "hydra-x")), "hydra-x");
    }

    #[test]
    fn the_epoch_parser_matches_known_timestamps() {
        assert_eq!(epoch_millis("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch_millis("2000-01-01T00:00:00Z"), Some(946_684_800_000));
        // A leap day is a real date.
        assert_eq!(
            epoch_millis("2024-02-29T00:00:00Z"),
            Some(1_709_164_800_000)
        );
        assert_eq!(
            epoch_millis("2026-09-16T00:00:00Z"),
            Some(1_789_516_800_000)
        );
        // Fractional seconds fold to milliseconds.
        assert_eq!(epoch_millis("1970-01-01T00:00:00.250Z"), Some(250));
        // Junk is None, never a guessed time.
        assert_eq!(epoch_millis("not-a-date"), None);
        assert_eq!(epoch_millis("2026-13-01T00:00:00Z"), None);
    }

    /// The grouped SQL carries the whitelist expression and a non-`key` alias.
    #[test]
    fn the_grouped_sql_is_a_whitelist() {
        for (group, column) in [
            (GroupBy::User, "coalesce(user_id, '')"),
            (GroupBy::UserGroup, "coalesce(user_group_id, '')"),
            (GroupBy::ApiKey, "coalesce(api_key_id, '')"),
        ] {
            assert_eq!(group_expr(group), Some(column));
        }
        assert_eq!(group_expr(GroupBy::Model), Some("model_key"));
        assert_eq!(group_expr(GroupBy::Provider), Some("provider_id"));
        assert_eq!(group_expr(GroupBy::Day), Some("substr(created_at, 1, 10)"));
        assert_eq!(group_expr(GroupBy::None), None);
        assert_ne!(group_expr(GroupBy::Model), Some("key"));
    }

    /// An aggregate over an empty window is ONE row of `null`s (measured), which must read as a real
    /// zero — and a non-numeric counter must NOT.
    #[test]
    fn an_empty_aggregate_is_a_real_zero_and_a_bad_value_is_a_failure() {
        let empty = GreptimedbResponse::parse(
            r#"{"output":[{"records":{"schema":{"column_schemas":[{"name":"requests"},{"name":"tokens_in"},{"name":"tokens_out"},{"name":"cache_hit_tokens"},{"name":"errors"},{"name":"last_seen"}]},"rows":[[0,null,null,null,0,null]]}}]}"#,
        )
        .expect("parses");
        let totals = decode_totals(&empty).expect("an empty window is a real zero");
        assert_eq!(totals.requests, 0);
        assert_eq!(totals.tokens_in, 0);
        assert_eq!(decode_as_of(&empty), None, "no stamp means no `as_of`");

        let bad = GreptimedbResponse::parse(
            r#"{"output":[{"records":{"schema":{"column_schemas":[{"name":"requests","data_type":"String"}]},"rows":[["not-a-number"]]}}]}"#,
        )
        .expect("parses");
        assert!(
            matches!(decode_totals(&bad), Err(UsageQueryError::Decode(_))),
            "a non-numeric count must not become zero"
        );
    }

    /// A numeric string is accepted (a proxy or a setting may quote it); a null key is `""`.
    #[test]
    fn a_quoted_counter_decodes_and_a_null_group_key_becomes_empty() {
        let resp = GreptimedbResponse::parse(
            r#"{"output":[{"records":{"schema":{"column_schemas":[{"name":"group_key"},{"name":"requests"},{"name":"tokens_in"},{"name":"tokens_out"},{"name":"cache_hit_tokens"},{"name":"errors"}]},"rows":[["st1","1","2","3",null,"0"],[null,"2","4","6","0","1"]]}}]}"#,
        )
        .expect("parses");
        let rows = decode_rows(&resp).expect("decodes");
        assert_eq!(rows[0].key, "st1");
        assert_eq!(rows[0].totals.tokens_in, 2);
        assert_eq!(rows[1].key, "");
        assert_eq!(rows[1].totals.requests, 2);
    }

    /// A response missing a counter column is shape drift, never a zero.
    #[test]
    fn a_missing_counter_column_is_a_decode_failure() {
        let resp = GreptimedbResponse::parse(
            r#"{"output":[{"records":{"schema":{"column_schemas":[{"name":"requests"}]},"rows":[[7]]}}]}"#,
        )
        .expect("parses");
        assert!(matches!(
            decode_totals(&resp),
            Err(UsageQueryError::Decode(_))
        ));
    }

    /// A batch is split at 512 rows.
    #[test]
    fn a_batch_is_chunked_at_the_row_limit() {
        let rows: Vec<UsageRecord> = (0..1300)
            .map(|i| rec("t1", &format!("trace-{i}")))
            .collect();
        let chunks = chunk_batches(&rows, usize::MAX, MAX_INSERT_ROWS);
        assert_eq!(chunks.len(), 3, "1300 rows / 512 => 3 chunks");
        assert_eq!(chunks[0].len(), 512);
        assert_eq!(chunks[1].len(), 512);
        assert_eq!(chunks[2].len(), 276);
        let total: usize = chunks.iter().map(Vec::len).sum();
        assert_eq!(total, 1300, "no row is dropped by chunking");
    }

    /// A body over the size limit is split even when the row count is small.
    #[test]
    fn a_batch_is_chunked_at_the_body_limit() {
        let mut big = rec("t1", "big");
        big.upstream_host = Some("x".repeat(400));
        let rows: Vec<UsageRecord> = (0..50).map(|_| big.clone()).collect();
        // A limit a bit above one row's encoded body but well below fifty.
        let one = build_insert(&[&big]);
        let one_len = "sql=".len() + transport::url_encode(&one).len();
        let chunks = chunk_batches(&rows, one_len * 5, MAX_INSERT_ROWS);
        assert!(chunks.len() > 1, "the body limit must split the batch");
        for c in &chunks {
            let sql = build_insert(c);
            let len = "sql=".len() + transport::url_encode(&sql).len();
            assert!(len <= one_len * 5 || c.len() == 1, "chunk too large: {len}");
        }
    }

    /// The single-pass running length is EXACTLY the naive `sql=` + full-statement encoding for
    /// every prefix, so the split boundary is byte-identical to the old per-candidate re-encode.
    ///
    /// This is the property the O(n) rewrite rests on: `url_encode` maps each byte independently,
    /// so the length of a concatenation is the sum of the encoded lengths.
    #[test]
    fn the_single_pass_length_matches_the_naive_encoding() {
        let mut rows = Vec::new();
        for i in 0..40u64 {
            let mut r = rec("t1", &format!("trace-{i}"));
            r.model_key = format!("m-{i}'s");
            r.tokens_in = if i % 3 == 0 { None } else { Some(i) };
            r.error = if i % 5 == 0 { Some("e'".into()) } else { None };
            rows.push(r);
        }
        let prefix = format!("INSERT INTO {TABLE} ({INSERT_COLUMNS}) VALUES ");
        let prefix_len = "sql=".len() + transport::url_encode(&prefix).len();
        let sep_len = transport::url_encode(",").len();
        let mut running = 0usize;
        for k in 0..rows.len() {
            let tuple_len = transport::url_encode(&row_values(&rows[k])).len();
            running = if k == 0 {
                prefix_len + tuple_len
            } else {
                running + sep_len + tuple_len
            };
            let refs: Vec<&UsageRecord> = rows[..=k].iter().collect();
            let naive = "sql=".len() + transport::url_encode(&build_insert(&refs)).len();
            assert_eq!(running, naive, "length drift at prefix of {} rows", k + 1);
        }
    }
}

/// The live test: a REAL GreptimeDB, driven through the backend's own `open`/write/read path.
///
/// `#[ignore]`d because it needs a server. The measured recipe:
///
/// ```bash
/// docker run -d --name hydra-greptime -p 127.0.0.1:4400:4000 greptime/greptimedb:v1.3.0 standalone start
/// HYDRA_TEST_GREPTIMEDB_URL=http://127.0.0.1:4400 \
///   cargo test -p hydra-server --features server,usage-greptimedb --lib \
///   usage::backends::greptimedb::live -- --ignored --nocapture
/// ```
///
/// What it pins, none of which a unit test can: the canonical DDL runs against the real server, the
/// multi-row INSERT is accepted, the primary key folds a retried row instead of double-counting, and
/// the measured read SQL returns what was written.
#[cfg(all(test, feature = "usage-greptimedb"))]
mod live {
    use super::*;

    fn url() -> Option<String> {
        std::env::var("HYDRA_TEST_GREPTIMEDB_URL")
            .ok()
            .filter(|v| !v.is_empty())
    }

    fn record(tenant: &str, trace: &str, tokens_in: u64, ts: &str) -> UsageRecord {
        let mut r = crate::usage::testing::record(trace);
        r.tenant_id = tenant.to_string();
        r.provider_id = "p1".into();
        r.model_key = "gpt-4o".into();
        r.user_id = Some("u1".into());
        r.user_group_id = Some("g1".into());
        r.api_key_id = Some("k1".into());
        r.tokens_in = Some(tokens_in);
        r.tokens_out = Some(7);
        r.cache_hit_tokens = None;
        r.latency_ms = 3;
        r.created_at = ts.to_string();
        r
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a real GreptimeDB (see the module docs for the recipe)"]
    async fn the_backend_writes_and_reads_a_real_greptimedb() {
        let Some(url) = url() else {
            panic!("HYDRA_TEST_GREPTIMEDB_URL must be set for the live test");
        };
        let pool = crate::db::test_pool().await;
        let cfg =
            BackendConfig::new(pool).with_env(crate::usage::EnvView::fixed(&[(URL_ENV, &url)]));

        // 1. `open` bootstraps the schema against the real server.
        let opened = crate::usage::open(DESCRIPTOR.kind, &cfg).expect("the backend opens");

        // Unique tenants per run: the window assertions are exact, and a re-run must not see the
        // previous run's rows (a live test that only passes once is a live test nobody runs twice).
        let run = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let t1 = format!("gt-live-t1-{run}");
        let t2 = format!("gt-live-t2-{run}");
        let since = "2026-11-11T00:00:00Z";
        let until = "2026-11-11T01:00:00Z";

        // 2. Write two rows for one tenant and one for another, through the REAL writer.
        let sink = opened.sink.clone();
        for r in [
            record(&t1, "gt-live-a", 11, "2026-11-11T00:10:00Z"),
            record(&t1, "gt-live-b", 22, "2026-11-11T00:20:00Z"),
            record(&t2, "gt-live-c", 99, "2026-11-11T00:30:00Z"),
        ] {
            sink.record(r).await;
        }
        sink.shutdown().await;

        // 3. Read it back through the REAL reader, and check the tenant filter is a filter.
        let reader = opened.query.expect("it reads");
        let agg = reader
            .aggregate(&t1, since, until, GroupBy::None)
            .await
            .expect("the aggregate answers");
        assert_eq!(agg.totals.requests, 2, "{agg:?}");
        assert_eq!(agg.totals.tokens_in, 33, "{agg:?}");
        assert_eq!(agg.totals.errors, 0);
        assert!(
            agg.as_of.is_some(),
            "max(created_at) must be the as_of: {agg:?}"
        );

        let other = reader
            .aggregate(&t2, since, until, GroupBy::None)
            .await
            .expect("the second tenant answers");
        assert_eq!(other.totals.requests, 1);
        assert_eq!(other.totals.tokens_in, 99);

        // 4. Grouping: every row written here carries model `gpt-4o`.
        let grouped = reader
            .aggregate(&t1, since, until, GroupBy::Model)
            .await
            .expect("the grouped read answers");
        assert_eq!(grouped.rows.len(), 1, "{grouped:?}");
        assert_eq!(grouped.rows[0].key, "gpt-4o");
        assert_eq!(grouped.rows[0].totals.requests, 2);

        // 5. Retry dedup: the SAME (tenant_id, trace_id, ts) inserted twice is ONE row (measured:
        //    count 1, sum(tokens_in) 10). This is the property the primary key exists for.
        let dedup = format!("gt-dedup-{run}");
        let row = record(&dedup, "gt-dedup-trace", 10, "2026-11-11T00:40:00Z");
        let stmt = build_insert(&[&row]);
        let parsed = parse_greptimedb_url(&url).expect("parses");
        for attempt in 1..=2 {
            let resp = transport::send(&parsed, &stmt)
                .await
                .unwrap_or_else(|e| panic!("dedup insert attempt {attempt} failed: {e}"));
            assert!(
                !resp.is_failure(),
                "attempt {attempt}: {}",
                resp.error_text()
            );
        }
        let deduped = reader
            .aggregate(&dedup, since, until, GroupBy::None)
            .await
            .expect("the dedup tenant answers");
        assert_eq!(
            deduped.totals.requests, 1,
            "a retried row must fold, not double-count: {deduped:?}"
        );
        assert_eq!(deduped.totals.tokens_in, 10, "{deduped:?}");

        // 6. An empty window is a real zero (measured: one row of NULLs).
        let empty = reader
            .aggregate(
                &t1,
                "2030-01-01T00:00:00Z",
                "2030-01-02T00:00:00Z",
                GroupBy::None,
            )
            .await
            .expect("an empty window is not an error");
        assert_eq!(empty.totals.requests, 0);
        assert_eq!(empty.as_of, None, "no rows means no as_of");

        // 7. THE RULE: a refused statement is a failure whether the server frames it as a non-2xx
        //    (the transport returns `Err`) or as a 2xx body with a top-level `code`/`error` — asking
        //    for a table that cannot exist must never read as a successful empty answer.
        match transport::send(&parsed, "SELECT * FROM hydra_usage.no_such_table_xyz").await {
            Ok(broken) => {
                assert!(broken.is_failure(), "a missing table must be a failure");
                assert!(
                    !broken.error_text().is_empty(),
                    "and the refusal must carry the server's words"
                );
            }
            Err(message) => assert!(!message.is_empty(), "the refusal must be actionable"),
        }
    }
}
