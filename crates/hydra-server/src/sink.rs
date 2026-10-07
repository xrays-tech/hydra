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
    F: Fn(&str, Vec<UsageRecord>) -> Fut + Send + 'static,
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
                    //
                    // The window is CAPPED BY THE SHUTDOWN BUDGET, and that cap is
                    // what makes the reporting below reachable at all.
                    //
                    // The mechanism, stated correctly (an earlier version of this
                    // comment blamed a `process::exit` in `main.rs` right after
                    // `sink.shutdown()`, which does not exist — `main.rs` only logs
                    // "usage sinks flushed" and returns, see
                    // `spawn_sink_flush_on_shutdown`): `shutdown()` waits at most
                    // `MAX_SHUTDOWN_WAIT` and then gives up on the join, and the
                    // PROCESS is torn down by Pingora's own shutdown sequence —
                    // `graceful_shutdown_timeout_seconds` (5 s, set in `main.rs`
                    // next to `grace_period_seconds`) and `run_forever` ending in
                    // `std::process::exit(0)` (pingora-core 0.8.1
                    // `src/server/mod.rs:640`), which runs no destructors. So this
                    // task is cut off wherever it stands, with no chance to react.
                    //
                    // The ordinary 30 s retry window therefore outlived the 5 s
                    // budget by 25 s, and a backend that was down at shutdown lost
                    // the whole batch with NO `note_usage_drop` and NO log line: the
                    // two statements below never ran. One second of the budget is
                    // left for them.
                    let shutdown_window = retry_window
                        .min(MAX_SHUTDOWN_WAIT.saturating_sub(Duration::from_secs(1)));
                    if !buffer.is_empty()
                        && !flush_with_backoff(&mut buffer, &inserter, shutdown_window).await
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
/// A fresh, process-unique batch id (`<pid>-<nanos>-<seq>`).
///
/// Used as ClickHouse's `insert_deduplication_token`, so it must be stable for
/// one batch across retries and different for distinct batches. Built from the
/// pid, a monotonic nanosecond clock and an atomic counter — no dependency, and
/// no collision between two nodes' flushes.
fn new_batch_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos:x}-{seq:x}", std::process::id())
}

async fn flush_with_backoff<F, Fut>(
    buffer: &mut Vec<UsageRecord>,
    inserter: &F,
    retry_window: Duration,
) -> bool
where
    F: Fn(&str, Vec<UsageRecord>) -> Fut,
    Fut: Future<Output = InsertResult>,
{
    const INITIAL: Duration = Duration::from_millis(50);
    const CAP: Duration = Duration::from_secs(10);

    let started = tokio::time::Instant::now();
    let mut delay = INITIAL;
    // ONE id per flush, reused by every retry below. ClickHouse's
    // `insert_deduplication_token` (see `clickhouse_insert_statement`) makes a
    // re-sent batch a no-op, which is what stops "the response was lost after the
    // insert committed" — the retry loop's case — from being billed twice. A
    // fresh id per ATTEMPT would defeat it entirely (the first cut did exactly
    // that, and the unit test caught it).
    //
    // EVERY attempt below re-sends the SAME records, in the same order, and that
    // is what makes the single token correct: ClickHouse deduplicates on
    // (token, block), so a retry whose content differed would NOT be recognised
    // and the rows the first attempt inserted would be written again. The
    // invariant holds because `buffer` cannot change while this loop awaits — the
    // select loop in `run_channel_sink` awaits this call inline, so nothing pushes
    // while a retry is in flight — and because every inserter returns the very
    // `Vec` it was handed (`Err((batch, msg))`). An earlier version of this comment
    // claimed "a batch whose composition changed between attempts" as a residual;
    // that cannot happen, and stating it as a live risk hid the real one below.
    //
    // RESIDUAL, stated plainly: a batch that outlives the retry window is put
    // back in the buffer and re-flushed on a later tick under a NEW id, so a lost
    // ack spanning that boundary can still duplicate rows. Closing that needs
    // ROW-level idempotency (a stable per-row key + `ReplacingMergeTree`), which is
    // a schema migration — see dev-docs/ops.md.
    let batch_id = new_batch_id();
    loop {
        if buffer.is_empty() {
            return true;
        }
        let batch = std::mem::take(buffer);
        match inserter(&batch_id, batch).await {
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
                // The sleep is clamped by what is LEFT of the window. Without
                // this clamp the window bounded only the ATTEMPTS: after the last
                // one inside the window the loop still slept the full backoff (up
                // to CAP = 10 s) before noticing, so a "4 s" window could keep the
                // task busy for 14 s. That overshoot is precisely what made the
                // shutdown path lose records silently — the final drain has to
                // finish inside `MAX_SHUTDOWN_WAIT` for its own loss report to run.
                let remaining = retry_window.saturating_sub(started.elapsed());
                tokio::time::sleep(delay.min(remaining)).await;
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
        let inserter = move |_batch_id: &str, batch: Vec<UsageRecord>| {
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

#[cfg(feature = "db")]
impl Drop for SqliteSink {
    fn drop(&mut self) {
        // Take both out BEFORE the blocking wait: the guards must not be held
        // across drain_on_drop's block_in_place/block_on.
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
    let mut tx = crate::db::begin_write(pool).await?;
    for r in records {
        sqlx::query(
            "INSERT INTO usage_record \
             (tenant_id, provider_id, model_key, client_api_key, sub_tenant_id, status_code, \
              tokens_in, tokens_out, cache_hit_tokens, \
              latency_ms, forward_latency_ms, ttft_ms, \
              upstream_host, error, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&r.tenant_id)
        .bind(&r.provider_id)
        .bind(&r.model_key)
        .bind(&r.client_api_key_masked)
        .bind(&r.sub_tenant_id)
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
use tokio::sync::mpsc as ch_mpsc;

// The ClickHouse transport lives in `crate::clickhouse` — the single owner of
// "how to talk to ClickHouse" (URL/credential interpretation, request shape,
// deadlines, status classification). The writer below only builds the
// `FORMAT JSONEachRow` payload and interprets the answer.
#[cfg(feature = "usage-clickhouse")]
use crate::clickhouse::{
    is_ok_status, parse_clickhouse_url, response_body, send, ClickHouseConfig,
};

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

// ===========================================================================
// Batching defaults (shared by every backend, design §9.2)
// ===========================================================================

/// Records buffered before a flush is forced (whichever comes first with the interval below).
pub(crate) const DEFAULT_BATCH_SIZE: usize = 256;
/// Seconds between time-driven flushes when a batch never fills.
pub(crate) const DEFAULT_FLUSH_SECS: u64 = 5;

/// Maximum time [`SqliteSink`] / [`ClickHouseSink`] `Drop` will wait for the
/// background task to finish its final flush. Bounds graceful shutdown so a
/// permanently-broken backend cannot hang shutdown indefinitely; if the wait
/// elapses the task is detached (it may still complete the flush if the backend
/// recovers, and is aborted when the runtime shuts down).
const MAX_SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

// `build_sink` / `BuildSinkError` stood here until 2026-10-07. Selection is now owned by
// `crate::usage::REGISTRY` (ADR-0002): one descriptor per backend, one `open()` that returns the
// writer AND the reader, so the write side and the read side cannot drift apart (the previous two
// parallel `match`es were kept consistent by a comment saying they had to be). The two defaults
// above are still the single owner of "how big a batch is and how often it flushes".

// ===========================================================================
// Audit §3.9 — the batching engine must never stop draining its channel, and
// the ClickHouse transport must never await an answer without a deadline.
// ===========================================================================
/// `clickhouse_insert_statement` had NO test anywhere in the repo, while it is
/// the half of the retry-idempotency fix that lives in our own code: the token has
/// to reach a syntactically valid statement, and `SETTINGS` must come BEFORE
/// `FORMAT` **or ClickHouse rejects the query**. Neither was guarded, so an edit
/// here could only ever fail on a live instance — which CI never runs for this
/// (the end-to-end test is `#[ignore]`d and no job passes `--ignored`).
///
/// Falsification: put `FORMAT JSONEachRow` before `SETTINGS` and the ordering
/// assertion fails; drop the alphabet filter and the sanitisation assertion fails.
#[cfg(feature = "usage-clickhouse")]
#[cfg(test)]
mod clickhouse_statement_tests {
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

#[cfg(test)]
mod audit_3_9_tests {
    use super::*;

    fn rec(trace: &str) -> UsageRecord {
        UsageRecord {
            tenant_id: "t".into(),
            provider_id: "p".into(),
            model_key: "m".into(),
            client_api_key_masked: None,
            sub_tenant_id: None,
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
    /// The retry-idempotency invariant: every retry of ONE batch carries the
    /// SAME `insert_deduplication_token`, and a different batch gets a different
    /// one. A fresh id per attempt would defeat ClickHouse's deduplication
    /// entirely (the retry would be a new insert, i.e. double-counted usage).
    #[tokio::test]
    async fn one_batch_keeps_its_dedup_token_across_retries() {
        let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (seen_in, attempts_in) = (seen.clone(), attempts.clone());
        let inserter = move |batch_id: &str, batch: Vec<UsageRecord>| {
            let seen = seen_in.clone();
            let attempts = attempts_in.clone();
            let batch_id = batch_id.to_string();
            async move {
                seen.lock().expect("lock").push(batch_id);
                if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2 {
                    return Err((batch, "lost the ack".to_string()));
                }
                Ok(())
            }
        };

        let mut buffer = vec![rec("a")];
        let flushed = flush_with_backoff(&mut buffer, &inserter, Duration::from_millis(500)).await;
        assert!(flushed, "the third attempt succeeds");
        let ids = seen.lock().expect("lock").clone();
        assert_eq!(ids.len(), 3, "three attempts, one batch");
        assert!(
            ids.iter().all(|id| id == &ids[0]),
            "every retry of one batch must reuse the same dedup token, saw {ids:?}"
        );

        // A NEW batch must not reuse the token, or its rows would be deduped away.
        let mut second = vec![rec("b")];
        let flushed = flush_with_backoff(&mut second, &inserter, Duration::from_millis(500)).await;
        assert!(flushed);
        let ids = seen.lock().expect("lock").clone();
        assert_ne!(
            ids.last().expect("an id"),
            &ids[0],
            "a different batch needs a different token"
        );
    }

    /// The single dedup token is only sound if every retry re-sends the SAME
    /// records in the SAME order: ClickHouse deduplicates on (token, block), so a
    /// retry whose content differed would not be recognised and the rows the first
    /// attempt inserted would be written a second time. Nothing tested that
    /// invariant — the comment above `flush_with_backoff` even listed "a batch
    /// whose composition changed between attempts" as a live residual, which cannot
    /// happen: the select loop awaits the flush inline (so nothing pushes while a
    /// retry is in flight) and every inserter returns the very `Vec` it was handed.
    ///
    /// WHAT THIS TEST IS AND IS NOT. It pins the behaviour of the implementation as
    /// written — `flush_with_backoff` hands the SAME `Vec` back on every attempt —
    /// which is the invariant the single dedup token depends on. It is NOT a
    /// detector for the realistic regression: the only place production could break
    /// the invariant is the `buffer.extend(returned)` that puts a failed batch back,
    /// and that path cannot be reached from here (nothing else can run while the
    /// flush awaits, so there is no second source of records to reorder against).
    /// A test that CAN fail on that would have to drive `run_channel_sink` and
    /// `tx.send` concurrently during a retry — recorded in the plan as batch 6,
    /// not claimed here. An earlier version of this comment promised a
    /// falsification this test is unable to perform.
    // The `https://` refusal test moved to `crate::usage::backends::clickhouse`'s descriptor: the
    // rule belongs to the backend (it knows its transport has no TLS path), and it is now the
    // descriptor's `open` that enforces it. Leaving a copy here would test a function that no longer
    // exists — see `crate::usage::tests::an_https_clickhouse_url_is_refused_at_open`.

    #[tokio::test]
    async fn every_retry_sends_the_identical_batch_in_the_same_order() {
        let seen: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_in = seen.clone();
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_in = attempts.clone();
        let inserter = move |_batch_id: &str, batch: Vec<UsageRecord>| {
            let seen = seen_in.clone();
            let attempts = attempts_in.clone();
            async move {
                seen.lock()
                    .expect("lock")
                    .push(batch.iter().map(|r| r.trace_id.clone()).collect());
                // Fail three times, then succeed: the batch is re-sent unchanged
                // each time and is still the same batch on the successful attempt.
                if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 3 {
                    return Err((batch, "lost the ack".to_string()));
                }
                Ok(())
            }
        };

        let mut buffer = vec![rec("a"), rec("b"), rec("c")];
        let flushed = flush_with_backoff(&mut buffer, &inserter, Duration::from_millis(500)).await;
        assert!(flushed, "the fourth attempt succeeds");

        let batches = seen.lock().expect("lock").clone();
        assert_eq!(batches.len(), 4, "one batch, four attempts");
        for (i, batch) in batches.iter().enumerate() {
            assert_eq!(
                batch,
                &vec!["a".to_string(), "b".to_string(), "c".to_string()],
                "attempt {i} must carry the same records in the same order"
            );
        }
    }

    /// A record that arrives WHILE a batch is retrying must join the NEXT batch,
    /// never the one in flight.
    ///
    /// This is the invariant the single dedup token rests on, driven through the
    /// REAL channel loop — the sibling test
    /// (`every_retry_sends_the_identical_batch_in_the_same_order`) only feeds
    /// `flush_with_backoff` a `Vec` and therefore cannot reach the one production
    /// path that could break it: the `buffer.extend(returned)` that puts a failed
    /// batch back while the select loop keeps accepting new records. If a late
    /// record were merged into the in-flight batch, the retry's content would differ
    /// from the first attempt's, ClickHouse's (token, block) dedup would not
    /// recognise it, and the rows the first attempt inserted would be billed twice.
    ///
    /// Falsification: make `flush_with_backoff` re-take from a shared buffer on each
    /// attempt (instead of the batch it was handed) and the "mixed" assertion fires.
    #[tokio::test]
    async fn a_record_arriving_during_a_retry_joins_the_next_batch_not_this_one() {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let seen: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_in = seen.clone();
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_in = attempts.clone();
        let inserter = move |_batch_id: &str, batch: Vec<UsageRecord>| {
            let seen = seen_in.clone();
            let attempts = attempts_in.clone();
            async move {
                seen.lock()
                    .expect("lock")
                    .push(batch.iter().map(|r| r.trace_id.clone()).collect());
                // Fail twice so the batch is retried while we push a new record.
                if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2 {
                    return Err((batch, "backend down".to_string()));
                }
                Ok(())
            }
        };

        // batch_size 2 ⇒ the first two records start a flush; the hour-long tick
        // means the ticker cannot flush anything on its own.
        let task = tokio::spawn(run_channel_sink(
            rx,
            2,
            3600,
            MAX_FLUSH_RETRY_WINDOW,
            inserter,
        ));
        tx.send(rec("a")).await.expect("send a");
        tx.send(rec("b")).await.expect("send b");
        // Let the first attempt fail and the retry backoff begin.
        tokio::time::sleep(Duration::from_millis(120)).await;
        tx.send(rec("late")).await.expect("send late");
        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(20), task).await;

        let batches = seen.lock().expect("lock").clone();
        assert!(
            batches
                .iter()
                .any(|b| b == &vec!["a".to_string(), "b".to_string()]),
            "the first batch must be attempted verbatim: {batches:?}"
        );
        for b in &batches {
            let has_late = b.iter().any(|t| t == "late");
            let has_first = b.iter().any(|t| t == "a" || t == "b");
            assert!(
                !(has_late && has_first),
                "a record that arrived during the retry was MIXED into the in-flight \
                 batch, which changes its content and defeats the dedup token: {b:?} \
                 (all attempts: {batches:?})"
            );
        }
        assert!(
            batches.iter().any(|b| b.iter().any(|t| t == "late")),
            "the late record must still be flushed in a later batch (not dropped): {batches:?}"
        );
    }

    /// A final drain that CANNOT succeed must still be REPORTED.
    ///
    /// Round 13 fixed the silent loss (the 30 s drain outlived the 5 s shutdown
    /// budget, so `note_usage_drop("shutdown_unflushed")` and the `error!` never
    /// ran), but nothing asserted the reporting itself — only the timing. A
    /// regression that kept the timing and dropped the reporting would have gone
    /// unnoticed. The counter is process-global and a sibling test also fails a final
    /// drain, so assert a strict INCREASE plus the exact count on the last event.
    ///
    /// Falsification: delete the `note_usage_drop(..)` call in the `None` arm and
    /// this fails (the counter does not move).
    #[tokio::test]
    async fn a_failed_final_drain_is_reported_not_silent() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let inserter = |_batch_id: &str, batch: Vec<UsageRecord>| async move {
            Err::<(), _>((batch, "backend down".to_string()))
        };
        let before = crate::admin::metrics::usage_dropped_total("shutdown_unflushed");

        // Same shape as the timing test: a batch-sized buffer that only the final
        // drain can flush, and an inserter that always fails.
        let task = tokio::spawn(run_channel_sink(
            rx,
            16,
            3600,
            MAX_FLUSH_RETRY_WINDOW,
            inserter,
        ));
        tx.send(rec("lost-1")).await.expect("send");
        tx.send(rec("lost-2")).await.expect("send");
        drop(tx);
        task.await.expect("sink task");

        let after = crate::admin::metrics::usage_dropped_total("shutdown_unflushed");
        assert!(
            after > before,
            "an un-flushable final batch must be COUNTED as a drop (before={before}, \
             after={after}); losing usage silently is the bug this reporting exists for"
        );
    }

    /// The final drain must finish INSIDE the shutdown budget, because that is the
    /// only way its own loss report can run.
    ///
    /// `shutdown()` waits at most `MAX_SHUTDOWN_WAIT` and then stops waiting, and
    /// Pingora's shutdown sequence then ends the PROCESS (`graceful_shutdown_timeout_seconds`
    /// in `main.rs` → `run_forever` → `std::process::exit(0)`), cutting the sink
    /// task off wherever it stands. The final drain used the ordinary 30 s retry
    /// window, which outlived the 5 s budget by 25 s, so with a backend that was
    /// down at shutdown the batch was lost with NO `note_usage_drop` and NO log
    /// line: the reporting statements never ran.
    ///
    /// Falsification: pass `retry_window` (30 s) instead of the capped
    /// `shutdown_window` and the task is still retrying when the budget expires, so
    /// the join times out and this fails.
    #[tokio::test]
    async fn the_final_drain_finishes_inside_the_shutdown_budget() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        // Always fails: what is under test is that the drain STOPS in time.
        let inserter = |_batch_id: &str, batch: Vec<UsageRecord>| async move {
            Err::<(), _>((batch, "backend down".to_string()))
        };
        // A batch size of 16 keeps the record buffered (no inline flush on
        // arrival), and the hour-long tick means only the FINAL DRAIN can flush.
        let task = tokio::spawn(run_channel_sink(
            rx,
            16,
            3600,
            MAX_FLUSH_RETRY_WINDOW,
            inserter,
        ));
        tx.send(rec("a")).await.expect("send");
        drop(tx); // channel closed ⇒ the None arm runs the final drain

        let joined = tokio::time::timeout(MAX_SHUTDOWN_WAIT, task).await;
        assert!(
            joined.is_ok(),
            "the sink task must finish its final drain within MAX_SHUTDOWN_WAIT \
             ({MAX_SHUTDOWN_WAIT:?}); otherwise `shutdown()` returns first, \
             `process::exit` kills the task, and the un-flushed batch is lost with \
             no counter and no log line"
        );
        joined
            .expect("checked")
            .expect("the sink task must not panic");
    }

    #[tokio::test]
    async fn flush_gives_up_after_the_retry_window_and_keeps_the_batch() {
        let mut buffer = vec![rec("a"), rec("b")];
        let inserter = |_batch_id: &str, batch: Vec<UsageRecord>| async move {
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
        let inserter = move |_batch_id: &str, batch: Vec<UsageRecord>| {
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
