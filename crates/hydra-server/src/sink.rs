//! Pluggable usage-record sink (design §9.1–§9.3, §9.5).
//!
//! [`UsageSink`] is the trait every backend implements; [`SqliteSink`] is the
//! default (batched writes into the W2 `usage_record` table) and
//! [`ClickHouseSink`] is the optional ClickHouse backend gated on the
//! `usage-clickhouse` feature. [`build_sink`] selects one at startup from
//! configuration.
//!
//! # Key masking (§9.5)
//!
//! The sink **never** masks — it persists whatever masked string the caller
//! places in [`UsageRecord::client_api_key_masked`]. The caller (the proxy
//! lifecycle) is responsible for producing that value via the pure core
//! [`hydra_core::rewrite::mask_key`]. The SQLite column is `client_api_key`
//! (see `migrations/0001_init.sql`); the field name on the core struct is
//! `client_api_key_masked` — the two refer to the same value.
//!
//! # Why manual `Pin<Box<dyn Future>>` instead of `#[async_trait]`
//!
//! The design sketch (§9.1) writes `#[async_trait]`. That proc-macro desugars
//! to exactly `fn record(&self, ..) -> Pin<Box<dyn Future<Output = ..> + Send
//! + '_>>`. We write the desugared form directly: native `async fn` in traits
//! is stable since Rust 1.75 but **not object-safe**, and [`build_sink`]
//! returns `Box<dyn UsageSink>`, so a dyn-compatible signature is required.
//! No `async-trait` dependency is needed; the two are semantically identical.

#![cfg_attr(not(feature = "db"), allow(unused_imports, unused_variables))]

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use hydra_core::model::UsageRecord;

// ===========================================================================
// Trait (design §9.1) — available whenever the `sink` module is (`runtime` on).
// ===========================================================================

/// Pluggable usage-record sink. Implementations write [`UsageRecord`]s to a
/// backend (SQLite by default, ClickHouse optionally).
///
/// `record` is **fire-and-forget**: it MUST return immediately —
/// implementations buffer internally and flush asynchronously. A bounded
/// internal channel may drop records under extreme backpressure (logged), never
/// blocking the caller (design §9.2: "avoid blocking the proxy main flow").
pub trait UsageSink: Send + Sync {
    /// Buffer one usage record (non-blocking).
    fn record(&self, record: UsageRecord) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

    /// Stop the background flusher after draining what is already buffered,
    /// waiting (bounded) for the final flush to land.
    ///
    /// `Drop` is NOT enough: `main` runs pingora's `run_forever`, which ends in
    /// `std::process::exit(0)` — no destructors run — so on SIGTERM (a k8s
    /// rolling update, `docker stop`) the in-flight batch, everything queued in
    /// the sink channel and up to `flush_secs` of traffic were discarded, which
    /// is exactly what the sinks' `Drop` was written to prevent.
    ///
    /// Default: no-op, for sinks that hold nothing (the no-op test sink).
    fn shutdown(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}

// ===========================================================================
// Shared batching / backoff engine
// ===========================================================================
//
// Both concrete sinks share identical behaviour: an mpsc buffer drained by a
// background task that flushes on `batch_size` OR every `flush_secs`, retrying
// failed batches with exponential backoff. Only the per-backend insert differs,
// so it is supplied as a callable. (Honours the "duplicate twice → extract"
// rule from AGENTS.md; the retry/flush policy is the root-cause-shared logic.)

/// Result of an insert attempt: `Ok` on success, or `Err((batch, message))`
/// returning the un-written batch so the caller can retry it.
type InsertResult = Result<(), (Vec<UsageRecord>, String)>;

/// How long ONE `flush_with_backoff` invocation may keep retrying before it
/// hands the batch back to the channel loop.
///
/// Retrying forever looks safe ("never lose a record") but is not: while the
/// flush future is awaiting, `rx.recv()` is never polled, so the bounded
/// channel fills up and `record()` starts dropping usage — silently, at exactly
/// the moment the sink is already unhealthy (audit §3.9). Returning to the loop
/// after this window keeps live records flowing into the retained buffer.
const MAX_FLUSH_RETRY_WINDOW: Duration = Duration::from_secs(30);

/// Hard cap on records the flush task retains while the backend is down. Beyond
/// it incoming records are dropped (counted on
/// `hydra_usage_records_dropped_total`, never silently) so a long outage cannot
/// grow the gateway's memory without bound.
const MAX_RETAINED: usize = 10_000;

/// Count a dropped usage record. The metric is the point: losing billing data
/// must never be visible only as a log line (audit §3.9).
fn note_usage_drop(reason: &str, n: u64) {
    #[cfg(feature = "proxy")]
    crate::admin::metrics::record_usage_drop(reason, n);
    #[cfg(not(feature = "proxy"))]
    {
        // `admin` (and therefore its metric registry) only exists under `proxy`.
        let _ = (reason, n);
    }
}

/// Drive a background sink loop over `rx`. Flushes when the buffer reaches
/// `batch_size`, on the `flush_secs` interval (if non-empty), and a final flush
/// when the channel closes.
async fn run_channel_sink<F, Fut>(
    mut rx: tokio::sync::mpsc::Receiver<UsageRecord>,
    batch_size: usize,
    flush_secs: u64,
    retry_window: Duration,
    inserter: F,
) where
    F: Fn(Vec<UsageRecord>) -> Fut + Send + 'static,
    Fut: Future<Output = InsertResult> + Send + 'static,
{
    let batch_size = batch_size.max(1);
    let mut buffer: Vec<UsageRecord> = Vec::with_capacity(batch_size);

    let flush_dur = Duration::from_secs(flush_secs.max(1));
    let mut ticker = tokio::time::interval(flush_dur);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Consume the immediate first tick so a size-below-batch buffer only flushes
    // after a real `flush_dur` has elapsed, not instantly.
    ticker.tick().await;

    // Retention-cap drop counter, used only to throttle the warning.
    let mut retained_drops: u64 = 0;

    loop {
        tokio::select! {
            recv = rx.recv() => match recv {
                Some(record) => {
                    // Retention cap: the backend has been down long enough that
                    // the buffer hit its ceiling. Keep DRAINING the channel (so
                    // the bounded channel never fills and never drops behind our
                    // back) but refuse the excess here, counted and warned.
                    if buffer.len() >= MAX_RETAINED {
                        retained_drops += 1;
                        note_usage_drop("retention_cap", 1);
                        if retained_drops == 1 || retained_drops % 1000 == 0 {
                            tracing::warn!(
                                dropped_total = retained_drops,
                                retained = buffer.len(),
                                cap = MAX_RETAINED,
                                "usage sink retention cap reached (backend still down); dropping incoming usage records"
                            );
                        }
                        continue;
                    }
                    buffer.push(record);
                    if buffer.len() >= batch_size {
                        flush_with_backoff(&mut buffer, &inserter, retry_window).await;
                    }
                }
                None => {
                    // Channel closed: best-effort final drain + flush, then exit.
                    // A FAILED final flush means this batch is genuinely lost, so
                    // it is counted like every other drop (never silent).
                    if !buffer.is_empty()
                        && !flush_with_backoff(&mut buffer, &inserter, retry_window).await
                    {
                        note_usage_drop("shutdown_unflushed", buffer.len() as u64);
                        tracing::error!(
                            lost = buffer.len(),
                            "usage sink is shutting down with an un-flushable batch; \
                             these usage records are LOST"
                        );
                    }
                    return;
                }
            },
            _tick = ticker.tick(), if !buffer.is_empty() => {
                flush_with_backoff(&mut buffer, &inserter, retry_window).await;
            }
        }
    }
}

/// Flush `buffer` with exponential backoff (50ms → 100ms → … capped at 10s).
/// On insert failure the batch is returned to `buffer` and retried; this never
/// gives up (usage records are best-effort telemetry, but losing them silently
/// is worse than bounded retry). Never blocks `record()` callers (runs only in
/// the background task).
async fn flush_with_backoff<F, Fut>(
    buffer: &mut Vec<UsageRecord>,
    inserter: &F,
    retry_window: Duration,
) -> bool
where
    F: Fn(Vec<UsageRecord>) -> Fut,
    Fut: Future<Output = InsertResult>,
{
    const INITIAL: Duration = Duration::from_millis(50);
    const CAP: Duration = Duration::from_secs(10);

    let started = tokio::time::Instant::now();
    let mut delay = INITIAL;
    loop {
        if buffer.is_empty() {
            return true;
        }
        let batch = std::mem::take(buffer);
        match inserter(batch).await {
            Ok(()) => return true,
            Err((returned, msg)) => {
                tracing::warn!(
                    error = %msg,
                    backoff_ms = delay.as_millis() as u64,
                    "usage sink batch insert failed; will retry"
                );
                buffer.extend(returned);
                // Give the channel loop a turn once the retry window is spent:
                // the batch is retained in `buffer` and retried on the next
                // flush tick, but `rx.recv()` gets polled again in the meantime
                // so live traffic keeps being accepted (audit §3.9).
                if started.elapsed() >= retry_window {
                    tracing::warn!(
                        retained = buffer.len(),
                        window_ms = retry_window.as_millis() as u64,
                        "usage sink still failing after the retry window; returning to the                          channel loop with the batch retained (retried on the next flush)"
                    );
                    return false;
                }
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2).min(CAP);
            }
        }
    }
}

// ===========================================================================
// SqliteSink (design §9.2) — feature `db`
// ===========================================================================

#[cfg(feature = "db")]
use sqlx::SqlitePool;

#[cfg(any(feature = "db", feature = "usage-clickhouse"))]
use tokio::sync::mpsc;

/// Default `UsageSink` — batched INSERTs into the W2 `usage_record` table.
///
/// `record()` pushes onto a bounded mpsc channel (non-blocking; drops + logs on
/// overflow). A background task flushes on `batch_size` reached OR every
/// `flush_secs`, retrying transient DB errors with exponential backoff. On
/// `Drop` the channel closes and the sink best-effort synchronously drains +
/// final-flushes (requires a multi-threaded tokio runtime; on a current-thread
/// runtime it detaches, still completing the flush asynchronously).
#[cfg(feature = "db")]
pub struct SqliteSink {
    /// `None` once the sink has been shut down (explicitly or via `Drop`).
    tx: std::sync::Mutex<Option<mpsc::Sender<UsageRecord>>>,
    join: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[cfg(feature = "db")]
impl SqliteSink {
    /// Spawn the sink: creates a bounded channel and a background flush task on
    /// the current tokio runtime. Panics (via tokio) if called outside a
    /// runtime — bring the runtime up first (design: sink is constructed during
    /// server startup, after the runtime exists).
    #[must_use]
    pub fn new(pool: SqlitePool, batch_size: usize, flush_secs: u64) -> Self {
        let batch_size = batch_size.max(1);
        let capacity = batch_size.max(16);
        let (tx, rx) = mpsc::channel(capacity);

        let pool_for_task = pool.clone();
        let inserter = move |batch: Vec<UsageRecord>| {
            let pool = pool_for_task.clone();
            async move {
                match insert_batch_sqlite(&pool, &batch).await {
                    Ok(()) => Ok(()),
                    Err(e) => Err((batch, e.to_string())),
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

#[cfg(feature = "db")]
impl UsageSink for SqliteSink {
    fn record(&self, record: UsageRecord) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let tx = {
                let guard = self.tx.lock().expect("sink tx mutex");
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
                // Non-blocking: channel either full (backpressure) or closed
                // (shutting down). Either way never block the caller.
                let dropped_trace = match &err {
                    mpsc::error::TrySendError::Full(r) | mpsc::error::TrySendError::Closed(r) => {
                        r.trace_id.clone()
                    }
                };
                let (reason, dropped) = match &err {
                    mpsc::error::TrySendError::Full(_) => ("channel_full", 1u64),
                    mpsc::error::TrySendError::Closed(_) => ("channel_closed", 1u64),
                };
                note_usage_drop(reason, dropped);
                tracing::warn!(
                    dropped_trace_id = %dropped_trace,
                    error = %err,
                    reason,
                    "usage sink channel full/closed; dropping usage record"
                );
            }
        })
    }

    fn shutdown(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let tx = self.tx.lock().expect("sink tx mutex").take();
            let join = self.join.lock().expect("sink join mutex").take();
            drop(tx); // the bg task observes closure and does its final drain
            if let Some(join) = join {
                let _ = tokio::time::timeout(MAX_SHUTDOWN_WAIT, join).await;
            }
        })
    }
}

#[cfg(feature = "db")]
impl Drop for SqliteSink {
    fn drop(&mut self) {
        // Take both out BEFORE the blocking wait: the guards must not be held
        // across drain_on_drop's block_in_place/block_on.
        let tx = self.tx.lock().expect("sink tx mutex").take();
        let join = self.join.lock().expect("sink join mutex").take();
        drain_on_drop(tx, join);
    }
}

/// Insert a batch inside a single transaction (atomicity + speed). The core
/// struct's `client_api_key_masked` maps to the `client_api_key` column; the
/// `trace_id` field has no corresponding column in the W2 schema and is
/// intentionally not persisted here (it lives in logs/metrics, design §4.1).
///
/// Uses runtime-checked `query` (see `db.rs` header: in-memory pools can't be
/// introspected at compile time).
#[cfg(feature = "db")]
async fn insert_batch_sqlite(
    pool: &SqlitePool,
    records: &[UsageRecord],
) -> Result<(), sqlx::Error> {
    if records.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for r in records {
        sqlx::query(
            "INSERT INTO usage_record \
             (tenant_id, provider_id, model_key, client_api_key, status_code, \
              tokens_in, tokens_out, cache_hit_tokens, \
              latency_ms, forward_latency_ms, ttft_ms, \
              upstream_host, error, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&r.tenant_id)
        .bind(&r.provider_id)
        .bind(&r.model_key)
        .bind(&r.client_api_key_masked)
        .bind(i64::from(r.status_code))
        .bind(r.tokens_in.map(|v| v as i64))
        .bind(r.tokens_out.map(|v| v as i64))
        .bind(r.cache_hit_tokens.map(|v| v as i64))
        .bind(r.latency_ms as i64)
        .bind(r.forward_latency_ms.map(|v| v as i64))
        .bind(r.ttft_ms.map(|v| v as i64))
        .bind(&r.upstream_host)
        .bind(&r.error)
        .bind(&r.created_at)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

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
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(feature = "usage-clickhouse")]
use tokio::sync::mpsc as ch_mpsc;

/// Configuration parsed once from the ClickHouse URL.
#[cfg(feature = "usage-clickhouse")]
#[derive(Clone)]
struct ClickHouseConfig {
    /// `host:port` for the TCP connection (e.g. `127.0.0.1:8123`).
    host_port: String,
    /// HTTP Basic credentials from `user:pass@host` URL userinfo, if present.
    /// Sent as `Authorization: Basic <base64(user:pass)>` — ClickHouse's HTTP
    /// interface accepts Basic auth natively.
    auth: Option<(String, String)>,
    /// Any query string from the URL (e.g. `?database=dogress` or
    /// `?user=x&password=y`), WITHOUT the leading `?`. Appended to the POST
    /// request's query string; empty when the URL had none.
    query_params: String,
    /// Deadline for the TCP connect
    /// (`HYDRA_CLICKHOUSE_CONNECT_TIMEOUT_MS`, default 3000).
    connect_timeout: Duration,
    /// Deadline for EACH of write / flush / response read
    /// (`HYDRA_CLICKHOUSE_IO_TIMEOUT_MS`, default 15000).
    ///
    /// Without these, a ClickHouse that accepts the connection but never answers
    /// — or a black-holed route — pinned the single flush task forever, the
    /// bounded channel filled and usage records were dropped (audit §3.9).
    io_timeout: Duration,
}

/// Largest response we buffer from ClickHouse. A successful `INSERT` answers
/// `200` with an empty body; only error text is ever sent, so anything past
/// this is truncation-safe and keeps a misbehaving server from growing our heap.
#[cfg(feature = "usage-clickhouse")]
const MAX_CLICKHOUSE_RESPONSE: usize = 64 * 1024;

/// Read `KEY` as a positive millisecond count, falling back to `default_ms`.
#[cfg(feature = "usage-clickhouse")]
fn env_millis(key: &str, default_ms: u64) -> Duration {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_millis(default_ms))
}

/// Parse a ClickHouse URL into transport + credentials. Accepted forms:
///
/// - `http://host:port` / `https://host:port` / bare `host:port` (anonymous);
/// - `http://user:pass@host:port` — userinfo becomes HTTP Basic auth;
/// - any of the above plus a query string (`?database=dogress`,
///   `?user=x&password=y`), which is passed through verbatim.
///
/// CR/LF in credentials are stripped (header-injection guard); the password is
/// otherwise sent as-is inside the Basic auth header.
#[cfg(feature = "usage-clickhouse")]
fn parse_clickhouse_url(url: &str) -> ClickHouseConfig {
    let stripped = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    // Split off the query string first (kept for passthrough).
    let (authority, query) = match stripped.split_once('?') {
        Some((a, q)) => (a, Some(q)),
        None => (stripped, None),
    };
    // Split off userinfo (`user[:pass]@`).
    let (userinfo, host_port) = match authority.split_once('@') {
        Some((ui, hp)) => (Some(ui), hp),
        None => (None, authority),
    };
    let auth = userinfo.and_then(|ui| {
        let (user, pass) = match ui.split_once(':') {
            Some((u, p)) => (u, p),
            None => (ui, ""),
        };
        if user.is_empty() {
            None
        } else {
            Some((strip_crlf(user), strip_crlf(pass)))
        }
    });
    ClickHouseConfig {
        // Trim a trailing '/' (and any path): the sink only dials host:port.
        host_port: host_port.trim_end_matches('/').to_string(),
        auth,
        query_params: query.unwrap_or("").to_string(),
        connect_timeout: env_millis("HYDRA_CLICKHOUSE_CONNECT_TIMEOUT_MS", 3_000),
        io_timeout: env_millis("HYDRA_CLICKHOUSE_IO_TIMEOUT_MS", 15_000),
    }
}

/// Strip CR/LF so user-supplied URL credentials can never inject HTTP headers.
#[cfg(feature = "usage-clickhouse")]
fn strip_crlf(s: &str) -> String {
    s.chars().filter(|&c| c != '\r' && c != '\n').collect()
}

/// Optional ClickHouse `UsageSink`. Same batching/backoff/Drop semantics as
/// [`SqliteSink`]; only the insert transport differs.
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
        let inserter = move |batch: Vec<UsageRecord>| {
            let cfg = cfg.clone();
            async move {
                match insert_batch_clickhouse_http(&cfg, &batch).await {
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
                let guard = self.tx.lock().expect("sink tx mutex");
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
                let (reason, dropped) = match &err {
                    ch_mpsc::error::TrySendError::Full(_) => ("channel_full", 1u64),
                    ch_mpsc::error::TrySendError::Closed(_) => ("channel_closed", 1u64),
                };
                note_usage_drop(reason, dropped);
                tracing::warn!(
                    error = %err,
                    reason,
                    "clickhouse usage sink channel full/closed; dropping usage record"
                );
            }
        })
    }

    fn shutdown(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let tx = self.tx.lock().expect("sink tx mutex").take();
            let join = self.join.lock().expect("sink join mutex").take();
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
        let tx = self.tx.lock().expect("sink tx mutex").take();
        let join = self.join.lock().expect("sink join mutex").take();
        drain_on_drop(tx, join);
    }
}

/// Shared `Drop` body: close the channel, then best-effort synchronously wait
/// (bounded by [`MAX_SHUTDOWN_WAIT`]) for the background task to finish its
/// final drain + flush. Requires a multi-threaded tokio runtime
/// (`block_in_place`); on a current-thread runtime or no runtime we silently
/// detach — the bg task still completes the flush asynchronously when the
/// backend recovers, and is aborted when the runtime shuts down.
fn drain_on_drop(tx: Option<mpsc::Sender<UsageRecord>>, join: Option<tokio::task::JoinHandle<()>>) {
    // 1. Close the channel first so the bg task observes closure and does its
    //    final drain + flush.
    drop(tx);
    let join = match join {
        Some(j) => j,
        None => return,
    };
    // 2. Bounded synchronous wait. The `catch_unwind` guards against
    //    `block_in_place` panicking on a current-thread runtime.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let waited = async {
                let _ = tokio::time::timeout(MAX_SHUTDOWN_WAIT, join).await;
            };
            tokio::task::block_in_place(|| handle.block_on(waited));
        }
    }));
}

/// The `INSERT` statement. Column list matches the ClickHouse `usage_record`
/// schema (environment/clickhouse/init.sql) — provider-neutral token columns.
#[cfg(feature = "usage-clickhouse")]
const CLICKHOUSE_INSERT: &str =
    "INSERT INTO usage_record (tenant_id, provider_id, model_key, client_api_key, status_code, \
     tokens_in, tokens_out, cache_hit_tokens, latency_ms, \
     forward_latency_ms, ttft_ms, upstream_host, error, created_at) \
     FORMAT JSONEachRow";

/// Insert a batch into ClickHouse over HTTP. On failure returns the error
/// message (the batch is retained by the caller for retry).
#[cfg(feature = "usage-clickhouse")]
async fn insert_batch_clickhouse_http(
    cfg: &ClickHouseConfig,
    records: &[UsageRecord],
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

    // POST /?query=<url-encoded INSERT>& … with the rows in the body.
    // URL query params (e.g. `?database=dogress`, or `?user=&password=`) are
    // passed through verbatim; `user:pass@` userinfo becomes Basic auth.
    let mut request = String::with_capacity(body.len() + 256);
    request.push_str("POST /?");
    if !cfg.query_params.is_empty() {
        request.push_str(&cfg.query_params);
        request.push('&');
    }
    request.push_str("query=");
    request.push_str(&url_encode(CLICKHOUSE_INSERT));
    request.push_str(" HTTP/1.1\r\n");
    request.push_str("Host: ");
    request.push_str(&cfg.host_port);
    request.push_str("\r\n");
    if let Some((user, pass)) = &cfg.auth {
        use base64::engine::general_purpose::STANDARD as B64;
        use base64::Engine as _;
        let token = B64.encode(format!("{user}:{pass}"));
        request.push_str("Authorization: Basic ");
        request.push_str(&token);
        request.push_str("\r\n");
    }
    request.push_str("Content-Length: ");
    request.push_str(&body.len().to_string());
    request.push_str("\r\nConnection: close\r\n\r\n");
    request.push_str(&body);

    // Every step is deadline-bound (audit §3.9): this runs on the single sink
    // flush task, so one blocked await would stop ALL usage metering for the
    // whole node and silently overflow the channel behind it.
    let hp = cfg.host_port.clone();
    let mut stream = tokio::time::timeout(
        cfg.connect_timeout,
        tokio::net::TcpStream::connect(&cfg.host_port),
    )
    .await
    .map_err(|_| {
        format!(
            "clickhouse connect {hp}: timed out after {}ms",
            cfg.connect_timeout.as_millis()
        )
    })?
    .map_err(|e| format!("clickhouse connect {hp}: {e}"))?;

    tokio::time::timeout(cfg.io_timeout, stream.write_all(request.as_bytes()))
        .await
        .map_err(|_| {
            format!(
                "clickhouse write: timed out after {}ms",
                cfg.io_timeout.as_millis()
            )
        })?
        .map_err(|e| format!("clickhouse write: {e}"))?;
    tokio::time::timeout(cfg.io_timeout, stream.flush())
        .await
        .map_err(|_| {
            format!(
                "clickhouse flush: timed out after {}ms",
                cfg.io_timeout.as_millis()
            )
        })?
        .map_err(|e| format!("clickhouse flush: {e}"))?;

    // Read the response with BOTH a deadline and a size cap. `read_to_end`
    // alone would wait for EOF: a server that sends a status line and then keeps
    // the connection open would hang us until the deadline, and a chatty server
    // could grow `resp` without bound.
    let mut resp = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = tokio::time::timeout(cfg.io_timeout, stream.read(&mut chunk))
            .await
            .map_err(|_| {
                format!(
                    "clickhouse read: timed out after {}ms",
                    cfg.io_timeout.as_millis()
                )
            })?
            .map_err(|e| format!("clickhouse read: {e}"))?;
        if n == 0 {
            break;
        }
        let room = MAX_CLICKHOUSE_RESPONSE.saturating_sub(resp.len());
        if room == 0 {
            // Enough to classify the status/error; stop waiting for EOF.
            break;
        }
        resp.extend_from_slice(&chunk[..n.min(room)]);
    }

    // ClickHouse returns HTTP 200 + empty body on a successful INSERT; any other
    // status carries the error text in the body.
    let resp_text = String::from_utf8_lossy(&resp);
    let status_line = resp_text.lines().next().unwrap_or("");
    if status_line.contains(" 200 ") {
        Ok(())
    } else {
        // Trim the body to a reasonable size for the error message.
        let body_text = resp_text.split("\r\n\r\n").nth(1).unwrap_or("").trim();
        Err(format!(
            "clickhouse insert rejected: status=`{status_line}` body={body_text}"
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

/// Percent-encode a string for use in a `?query=` URL segment (RFC 3986
/// unreserved characters kept; everything else `%HH`).
#[cfg(feature = "usage-clickhouse")]
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

// ===========================================================================
// Config-driven selection (design §9.3)
// ===========================================================================

/// Errors returned by [`build_sink`] when the requested configuration is
/// unsatisfiable. Surfaced as a typed `Result` (never a panic) so startup code
/// can report config problems cleanly.
#[derive(Debug, thiserror::Error)]
pub enum BuildSinkError {
    /// `cfg_kind` did not match `"sqlite"` or `"clickhouse"`.
    #[error("unknown sink kind '{kind}'")]
    UnknownKind { kind: String },
    /// `"sqlite"` was requested but no `SqlitePool` was supplied.
    #[error("sink kind 'sqlite' requires a SqlitePool")]
    MissingPool,
    /// `"clickhouse"` was requested but no URL was supplied.
    #[error("sink kind 'clickhouse' requires a url")]
    MissingClickHouseUrl,
    /// `"clickhouse"` was requested but the `usage-clickhouse` cargo feature is
    /// not enabled.
    #[error("sink kind 'clickhouse' requires the 'usage-clickhouse' cargo feature")]
    ClickHouseFeatureDisabled,
}

/// Default batch size for sinks constructed via [`build_sink`].
const DEFAULT_BATCH_SIZE: usize = 256;
/// Default flush interval (seconds) for sinks constructed via [`build_sink`].
const DEFAULT_FLUSH_SECS: u64 = 5;
/// Maximum time [`SqliteSink`] / [`ClickHouseSink`] `Drop` will wait for the
/// background task to finish its final flush. Bounds graceful shutdown so a
/// permanently-broken backend cannot hang shutdown indefinitely; if the wait
/// elapses the task is detached (it may still complete the flush if the backend
/// recovers, and is aborted when the runtime shuts down).
const MAX_SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

/// Select a [`UsageSink`] implementation from a configuration kind.
///
/// Returns a [`Result`] so invalid configuration is a handled startup error
/// rather than a panic (validates external input per AGENTS.md; the sketch in
/// the design doc returned `Box<dyn UsageSink>` directly, but a missing
/// `pool`/`url` cannot be recovered from without inventing a no-op placeholder
/// sink, which 铁律 2 forbids — hence the typed error).
#[cfg(feature = "db")]
pub fn build_sink(
    cfg_kind: &str,
    pool: Option<SqlitePool>,
    ch_url: Option<&str>,
) -> Result<Box<dyn UsageSink>, BuildSinkError> {
    match cfg_kind {
        "sqlite" => {
            let pool = pool.ok_or(BuildSinkError::MissingPool)?;
            Ok(Box::new(SqliteSink::new(
                pool,
                DEFAULT_BATCH_SIZE,
                DEFAULT_FLUSH_SECS,
            )))
        }
        "clickhouse" => {
            #[cfg(feature = "usage-clickhouse")]
            {
                let url = ch_url.ok_or(BuildSinkError::MissingClickHouseUrl)?;
                Ok(Box::new(ClickHouseSink::new(
                    url,
                    DEFAULT_BATCH_SIZE,
                    DEFAULT_FLUSH_SECS,
                )))
            }
            #[cfg(not(feature = "usage-clickhouse"))]
            {
                let _ = ch_url;
                Err(BuildSinkError::ClickHouseFeatureDisabled)
            }
        }
        other => Err(BuildSinkError::UnknownKind {
            kind: other.to_string(),
        }),
    }
}

#[cfg(all(test, feature = "usage-clickhouse"))]
mod tests {
    use super::*;

    #[test]
    fn parse_url_anonymous() {
        let cfg = parse_clickhouse_url("http://127.0.0.1:8123");
        assert_eq!(cfg.host_port, "127.0.0.1:8123");
        assert!(cfg.auth.is_none());
        assert_eq!(cfg.query_params, "");
    }

    #[test]
    fn parse_url_bare_host() {
        let cfg = parse_clickhouse_url("clickhouse:8123");
        assert_eq!(cfg.host_port, "clickhouse:8123");
        assert!(cfg.auth.is_none());
    }

    #[test]
    fn parse_url_userinfo_becomes_basic_auth() {
        let cfg = parse_clickhouse_url("http://sh_admin:sH_9527!@clickhouse:8123");
        assert_eq!(cfg.host_port, "clickhouse:8123");
        assert_eq!(cfg.auth, Some(("sh_admin".into(), "sH_9527!".into())));
        assert_eq!(cfg.query_params, "");
    }

    #[test]
    fn parse_url_user_only() {
        let cfg = parse_clickhouse_url("http://alice@clickhouse:8123");
        assert_eq!(cfg.auth, Some(("alice".into(), "".into())));
    }

    #[test]
    fn parse_url_query_passthrough() {
        let cfg = parse_clickhouse_url("http://clickhouse:8123/?database=dogress&user=x");
        assert_eq!(cfg.host_port, "clickhouse:8123");
        assert!(cfg.auth.is_none(), "query user must NOT become Basic auth");
        assert_eq!(cfg.query_params, "database=dogress&user=x");
    }

    #[test]
    fn parse_url_userinfo_and_query() {
        let cfg = parse_clickhouse_url("http://u:p@clickhouse:8123/?database=dogress");
        assert_eq!(cfg.auth, Some(("u".into(), "p".into())));
        assert_eq!(cfg.query_params, "database=dogress");
    }

    #[test]
    fn parse_url_strips_crlf_from_credentials() {
        let cfg = parse_clickhouse_url("http://u\r\n:pa\r\nss@clickhouse:8123");
        assert_eq!(cfg.auth, Some(("u".into(), "pass".into())));
    }
}

// ===========================================================================
// Audit §3.9 — the batching engine must never stop draining its channel, and
// the ClickHouse transport must never await an answer without a deadline.
// ===========================================================================
#[cfg(test)]
mod audit_3_9_tests {
    use super::*;

    fn rec(trace: &str) -> UsageRecord {
        UsageRecord {
            tenant_id: "t".into(),
            provider_id: "p".into(),
            model_key: "m".into(),
            client_api_key_masked: None,
            status_code: 200,
            tokens_in: Some(1),
            tokens_out: Some(1),
            cache_hit_tokens: None,
            latency_ms: 1,
            forward_latency_ms: None,
            ttft_ms: None,
            upstream_host: None,
            error: None,
            trace_id: trace.into(),
            created_at: "2026-09-15T00:00:00Z".into(),
        }
    }

    /// The retry loop must hand control back once its window is spent. Retrying
    /// a single batch FOREVER looks like "never lose a record", but while the
    /// flush future waits, `rx.recv()` is never polled: the bounded channel
    /// fills up and `record()` silently drops usage exactly when the sink is
    /// already unhealthy. Pre-fix this test timed out (flush never returned).
    #[tokio::test]
    async fn flush_gives_up_after_the_retry_window_and_keeps_the_batch() {
        let mut buffer = vec![rec("a"), rec("b")];
        let inserter = |batch: Vec<UsageRecord>| async move {
            Err::<(), _>((batch, "clickhouse down".to_string()))
        };
        let gave_up = tokio::time::timeout(
            Duration::from_secs(5),
            flush_with_backoff(&mut buffer, &inserter, Duration::from_millis(150)),
        )
        .await
        .expect(
            "flush_with_backoff must RETURN once the retry window is spent; retrying forever starves the channel-draining select loop"
        );
        assert!(!gave_up, "window exhausted => the batch was not flushed");
        assert_eq!(
            buffer.len(),
            2,
            "the un-flushed batch must be retained for the next flush tick"
        );
    }

    /// While the backend is down the loop keeps accepting new records into its
    /// retained buffer instead of leaving them in the bounded channel to be
    /// dropped. The sender deliberately uses `send().await` (real backpressure,
    /// not `try_send`): every record must still be delivered once the backend
    /// recovers — nothing may be lost across a temporary outage.
    #[tokio::test]
    async fn channel_sink_keeps_draining_across_a_temporary_outage() {
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        let delivered: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (delivered_in, attempts_in) = (delivered.clone(), attempts.clone());
        let inserter = move |batch: Vec<UsageRecord>| {
            let delivered = delivered_in.clone();
            let attempts = attempts_in.clone();
            async move {
                // Fail the first two attempts, then recover.
                if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2 {
                    return Err((batch, "backend down".to_string()));
                }
                delivered
                    .lock()
                    .expect("delivered mutex")
                    .extend(batch.iter().map(|r| r.trace_id.clone()));
                Ok(())
            }
        };
        let handle = tokio::spawn(run_channel_sink(
            rx,
            1,
            1,
            Duration::from_millis(50),
            inserter,
        ));
        for i in 0..6 {
            tx.send(rec(&format!("t{i}")))
                .await
                .expect("channel must stay open");
        }
        drop(tx);
        tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("the sink loop must finish after the channel closes")
            .expect("join");
        let got = delivered.lock().expect("delivered mutex");
        assert_eq!(
            got.len(),
            6,
            "every record must survive a temporary backend outage: {got:?}"
        );
    }

    // -----------------------------------------------------------------------
    // ClickHouse transport deadlines
    // -----------------------------------------------------------------------
    #[cfg(feature = "usage-clickhouse")]
    fn cfg_for(addr: std::net::SocketAddr, connect_ms: u64, io_ms: u64) -> ClickHouseConfig {
        ClickHouseConfig {
            host_port: addr.to_string(),
            auth: None,
            query_params: String::new(),
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
        insert_batch_clickhouse_http(&cfg, &[rec("a")])
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
        let msg = insert_batch_clickhouse_http(&cfg, &[rec("a")])
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
            insert_batch_clickhouse_http(&cfg, &[rec("a")]),
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
