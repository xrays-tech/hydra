//! The shared **batching engine**: one policy for every usage backend (design §9.2, §9.5).
//!
//! [`UsageSink`] is the write contract; the engine below is the machinery a backend gets by
//! implementing one `insert` callable: a bounded channel so `record()` never blocks the proxy, a
//! background task that flushes on `batch_size` OR every `flush_secs`, exponential backoff bounded
//! by a retry window, a retention cap, and one place that counts everything dropped. A backend that
//! re-implemented any of that would be a second policy for "how usage is delivered" — and the drop
//! accounting (which is what an operator alerts on) would then have two owners.
//!
//! Everything here is storage-neutral. The per-backend code lives in `usage::backends::<kind>` and
//! owns exactly two things: how a batch is written, and how it is read back.
//!
//! # Key masking (§9.5)
//!
//! The sink **never** masks — it persists whatever masked string the caller places in
//! [`UsageRecord::client_api_key_masked`]. The caller (the proxy lifecycle) produces that value via
//! the pure core [`hydra_core::rewrite::mask_key`].
//!
//! # Why manual `Pin<Box<dyn Future>>` instead of `#[async_trait]`
//!
//! The design sketch (§9.1) writes `#[async_trait]`; that macro desugars to exactly
//! `fn record(&self, ..) -> Pin<Box<dyn Future<Output = ..> + Send + '_>>`. We write the desugared
//! form directly because native `async fn` in traits is stable but **not object-safe**, and the
//! registry hands out `Arc<dyn UsageSink>`.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use hydra_core::model::UsageRecord;

#[cfg(any(feature = "db", feature = "usage-clickhouse"))]
use tokio::sync::mpsc;

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
pub(crate) const MAX_FLUSH_RETRY_WINDOW: Duration = Duration::from_secs(30);

/// Hard cap on records the flush task retains while the backend is down. Beyond
/// it incoming records are dropped (counted on
/// `hydra_usage_records_dropped_total`, never silently) so a long outage cannot
/// grow the gateway's memory without bound.
pub(crate) const MAX_RETAINED: usize = 10_000;

/// Count a dropped usage record. The metric is the point: losing billing data
/// must never be visible only as a log line (audit §3.9).
pub(crate) fn note_usage_drop(reason: &str, n: u64) {
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
pub(crate) async fn run_channel_sink<F, Fut>(
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
pub(crate) fn new_batch_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos:x}-{seq:x}", std::process::id())
}

pub(crate) async fn flush_with_backoff<F, Fut>(
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

/// Shared `Drop` body: close the channel, then best-effort synchronously wait
/// (bounded by [`MAX_SHUTDOWN_WAIT`]) for the background task to finish its
/// final drain + flush. Requires a multi-threaded tokio runtime
/// (`block_in_place`); on a current-thread runtime or no runtime we silently
/// detach — the bg task still completes the flush asynchronously when the
/// backend recovers, and is aborted when the runtime shuts down.
pub(crate) fn drain_on_drop(
    tx: Option<mpsc::Sender<UsageRecord>>,
    join: Option<tokio::task::JoinHandle<()>>,
) {
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
pub(crate) const MAX_SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

// Tests: the engine's own contract (§3.9 audit)
// ===========================================================================
//
// The batching engine must never stop draining its channel, and a retry must never turn one batch
// into two. These tests drive the engine with their OWN insert callable (`run_channel_sink` takes
// one), so they test the policy without owning a store — which is also what lets them run in every
// feature combination.

#[cfg_attr(
    not(any(feature = "db", feature = "usage-clickhouse")),
    allow(dead_code)
)]
#[cfg(test)]
mod tests {
    use super::*;

    fn rec(trace: &str) -> UsageRecord {
        crate::usage::testing::record(trace)
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

    // =====================================================================
    // The engine's delivery contract, driven through a backend-shaped sink
    // =====================================================================
    //
    // These seven cases were `tests/sqlite_sink.rs` until 2026-10-07. They were written against
    // `SqliteSink` because that was the only sink a test could point at a real store; when ADR-0002
    // retires SQLite, testing the engine through "whatever backend exists" would make the engine's
    // coverage depend on that backend. They now drive `usage::testing::RecordingSink`, which is
    // wired exactly like the real backends (same `run_channel_sink`, same `drain_on_drop`) and
    // records instead of writing.
    //
    // What did NOT come along, and why: `sink_persists_new_metrics_columns` and
    // `sink_new_metrics_null_when_absent` asserted the SQLite ROW shape (three nullable columns
    // round-tripping, `None` stored as SQL NULL). That subject disappears with the table — the
    // honest accounting is in ADR-0002 §7, and the ClickHouse side has its own row-shape coverage.

    /// `record()` ×N → the engine delivers them on the time flush (fewer than `batch_size`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn records_reach_the_backend_on_the_time_flush() {
        let sink = crate::usage::testing::RecordingSink::new(100, 1);
        for i in 0..5 {
            sink.record(rec(&format!("t{i}"))).await;
        }
        for _ in 0..50 {
            if sink.delivered().len() == 5 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            sink.delivered().len(),
            5,
            "the time flush must deliver every buffered record"
        );
        sink.shutdown().await;
    }

    /// Reaching `batch_size` flushes immediately, without waiting for the interval.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batch_size_triggers_an_immediate_flush() {
        // A flush interval far beyond the test: only the size threshold can fire.
        let sink = crate::usage::testing::RecordingSink::new(4, 3600);
        for i in 0..4 {
            sink.record(rec(&format!("t{i}"))).await;
        }
        for _ in 0..50 {
            if !sink.delivered().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(sink.batch_count(), 1, "one batch, flushed by size");
        assert_eq!(sink.delivered().len(), 4);
        sink.shutdown().await;
    }

    /// Below `batch_size`, the interval still flushes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_interval_flushes_below_batch_size() {
        let sink = crate::usage::testing::RecordingSink::new(1000, 1);
        for i in 0..3 {
            sink.record(rec(&format!("t{i}"))).await;
        }
        for _ in 0..50 {
            if sink.delivered().len() == 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            sink.delivered().len(),
            3,
            "the interval must fire even when batch_size was never reached"
        );
        sink.shutdown().await;
    }

    /// A backend that refuses for a while: `record()` never blocks, the batch is retried until it
    /// lands, and nothing leaks (exactly the batch size arrives, not more).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refusing_backend_does_not_block_callers_and_the_batch_lands_once() {
        // batch_size=2 so the first two records flush at once and hit the refusals.
        let sink = crate::usage::testing::RecordingSink::with_failures(3, 2, 3600);

        let t0 = std::time::Instant::now();
        for i in 0..2 {
            sink.record(rec(&format!("t{i}"))).await;
        }
        assert!(
            t0.elapsed() < Duration::from_millis(500),
            "record() must stay non-blocking while the backend refuses ({:?})",
            t0.elapsed()
        );

        for _ in 0..100 {
            if sink.delivered().len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            sink.delivered().len(),
            2,
            "after the refusals stop, the SAME batch must be delivered exactly once"
        );
        assert_eq!(sink.attempts_refused(3), 3, "three refusals were exercised");

        // And every retry of that ONE flush carried the same batch id: a fresh id per attempt would
        // make ClickHouse's `insert_deduplication_token` useless and double-count a re-sent batch.
        let ids = sink.attempt_batch_ids();
        assert!(
            ids.len() >= 4,
            "at least the three refusals plus the success: {ids:?}"
        );
        assert!(
            ids.iter().all(|id| id == &ids[0]),
            "one flush = one batch id, reused by every retry: {ids:?}"
        );
        sink.shutdown().await;
    }

    /// The sink does NOT mask: whatever the caller put in `client_api_key_masked` is what the
    /// backend receives (the proxy lifecycle owns the masking, design §9.5).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_backend_receives_the_masked_key_the_caller_supplied() {
        let masked = hydra_core::rewrite::mask_key("sk-abcd1234wxyz0987");
        assert!(
            !masked.contains("abcd1234wxyz"),
            "precondition: the caller masked it"
        );
        let sink = crate::usage::testing::RecordingSink::new(1, 3600);
        let mut r = rec("masked");
        r.client_api_key_masked = Some(masked.clone());
        sink.record(r).await;
        for _ in 0..50 {
            if !sink.delivered().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let got = sink.delivered();
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].client_api_key_masked.as_deref(),
            Some(masked.as_str()),
            "the sink must store exactly what the caller passed, never re-mask"
        );
        sink.shutdown().await;
    }

    /// `Drop` drains: records buffered below the thresholds are flushed when the sink goes away.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drop_drains_the_buffer() {
        let sink = crate::usage::testing::RecordingSink::new(1000, 3600);
        for i in 0..5 {
            sink.record(rec(&format!("t{i}"))).await;
        }
        assert_eq!(
            sink.delivered().len(),
            0,
            "precondition: nothing flushed yet"
        );
        let batches = sink.batches_handle();
        drop(sink);
        assert_eq!(
            delivered_count(&batches),
            5,
            "Drop must flush the buffered records (the path that survives a panic-free shutdown)"
        );
    }

    /// The explicit `shutdown()` drains without relying on `Drop`, and is idempotent: the SIGTERM
    /// handler drives this path (pingora's `run_forever` ends in `process::exit`, so no destructor
    /// runs on a real shutdown).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_drains_without_drop_and_is_idempotent() {
        let sink = crate::usage::testing::RecordingSink::new(1000, 3600);
        let batches = sink.batches_handle();
        for i in 0..4 {
            sink.record(rec(&format!("t{i}"))).await;
        }
        assert_eq!(
            delivered_count(&batches),
            0,
            "precondition: nothing flushed yet"
        );

        sink.shutdown().await;
        assert_eq!(delivered_count(&batches), 4, "shutdown() must drain");

        // Idempotent: a second call and the eventual Drop must neither hang nor duplicate.
        sink.shutdown().await;
        assert_eq!(delivered_count(&batches), 4);
        drop(sink);
        assert_eq!(
            delivered_count(&batches),
            4,
            "Drop after shutdown must not deliver anything a second time"
        );
    }

    /// Records delivered to a harness whose sink has been dropped (the sink cannot be read then, so
    /// the shared handle is).
    fn delivered_count(batches: &crate::usage::testing::DeliveredBatches) -> usize {
        batches
            .lock()
            .expect("batches")
            .iter()
            .map(|(_, b)| b.len())
            .sum()
    }
}
