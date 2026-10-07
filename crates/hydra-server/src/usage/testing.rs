//! Test-only harness: a **backend-shaped** sink that records instead of writing (ADR-0002 D-8).
//!
//! Why this exists at all: the batching engine's contract (a bounded channel that never blocks the
//! proxy, a flush on size OR time, bounded retry with backoff, a bounded wait on shutdown) used to
//! be tested through `SqliteSink`, because that was the only sink a test could point at a real
//! store. When SQLite is retired, testing the engine through a *store* would mean testing it through
//! whatever backend happens to exist — and the engine would lose its coverage the moment that
//! backend was swapped.
//!
//! So the engine is tested through a sink that keeps what it was handed. It is deliberately a
//! faithful copy of how the real backends are wired — [`run_channel_sink`] plus [`drain_on_drop`],
//! the same two shared pieces `SqliteSink` and `ClickHouseSink` use — so what these tests exercise
//! is the shared wiring, not a test-only reimplementation of it.
//!
//! **It is NOT in `REGISTRY`**, so no `HYDRA_USAGE_SINK` value can select it: a deployment cannot
//! "look configured" while silently dropping its usage into memory. That is the point of the
//! separation, and `usage::tests::the_registry_lists_every_backend_once` plus the guard keep it true.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use hydra_core::model::UsageRecord;
use tokio::sync::mpsc;

use crate::usage::engine::{drain_on_drop, run_channel_sink, UsageSink, MAX_FLUSH_RETRY_WINDOW};
use crate::usage::query::{GroupBy, UsageQuery, UsageQueryError};
use hydra_core::tenant_api::UsageAggregate;

/// The batches a recording sink has delivered: `(batch id, records)`.
pub(crate) type DeliveredBatches = Arc<Mutex<Vec<(String, Vec<UsageRecord>)>>>;

/// One usage record with values that make it identifiable.
#[must_use]
pub(crate) fn record(trace: &str) -> UsageRecord {
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

/// A sink that records every batch the engine delivers, and can be told to fail a few times first.
pub(crate) struct RecordingSink {
    tx: Mutex<Option<mpsc::Sender<UsageRecord>>>,
    join: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Batches, as the engine handed them over (batch id + records).
    batches: DeliveredBatches,
    /// How many insert attempts the harness refuses before accepting.
    failures_left: Arc<AtomicUsize>,
    /// The batch id of EVERY attempt, refused ones included. A retry must reuse the id of the flush
    /// it belongs to (that is the whole point of `insert_deduplication_token`), and only an
    /// all-attempts log can show it.
    attempts: Arc<Mutex<Vec<String>>>,
}

impl RecordingSink {
    /// A sink that accepts everything.
    pub(crate) fn new(batch_size: usize, flush_secs: u64) -> Self {
        Self::with_failures(0, batch_size, flush_secs)
    }

    /// A sink whose first `failures` insert attempts fail (each hands the batch back, so the engine
    /// must retry it with the SAME records and the same batch id).
    pub(crate) fn with_failures(failures: usize, batch_size: usize, flush_secs: u64) -> Self {
        let batch_size = batch_size.max(1);
        let capacity = batch_size.max(16);
        let (tx, rx) = mpsc::channel(capacity);

        let batches = Arc::new(Mutex::new(Vec::new()));
        let failures_left = Arc::new(AtomicUsize::new(failures));
        let attempts = Arc::new(Mutex::new(Vec::new()));

        let batches_for_task = batches.clone();
        let failures_for_task = failures_left.clone();
        let attempts_for_task = attempts.clone();
        let inserter = move |batch_id: &str, batch: Vec<UsageRecord>| {
            let batches = batches_for_task.clone();
            let failures = failures_for_task.clone();
            let attempts = attempts_for_task.clone();
            let batch_id = batch_id.to_string();
            async move {
                attempts
                    .lock()
                    .expect("attempts mutex")
                    .push(batch_id.clone());
                if failures.load(Ordering::SeqCst) > 0 {
                    failures.fetch_sub(1, Ordering::SeqCst);
                    return Err((batch, "the harness refused this attempt".to_string()));
                }
                batches
                    .lock()
                    .expect("batches mutex")
                    .push((batch_id, batch));
                Ok(())
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
            tx: Mutex::new(Some(tx)),
            join: Mutex::new(Some(join)),
            batches,
            failures_left,
            attempts,
        }
    }

    /// Every record the engine has delivered so far, in delivery order.
    pub(crate) fn delivered(&self) -> Vec<UsageRecord> {
        self.batches
            .lock()
            .expect("batches mutex")
            .iter()
            .flat_map(|(_, b)| b.iter().cloned())
            .collect()
    }

    /// How many batches were delivered.
    pub(crate) fn batch_count(&self) -> usize {
        self.batches.lock().expect("batches mutex").len()
    }

    /// The batch id of every attempt so far, refused ones included — the engine's contract is one
    /// id per FLUSH, reused by every retry of that flush.
    pub(crate) fn attempt_batch_ids(&self) -> Vec<String> {
        self.attempts.lock().expect("attempts mutex").clone()
    }

    /// The delivered batches, shareable so a test can still read them AFTER the sink is dropped
    /// (the two tests about `Drop` need exactly that).
    pub(crate) fn batches_handle(&self) -> DeliveredBatches {
        self.batches.clone()
    }

    /// How many insert attempts were refused (i.e. how many retries the engine performed).
    pub(crate) fn attempts_refused(&self, of: usize) -> usize {
        of - self.failures_left.load(Ordering::SeqCst)
    }
}

impl UsageSink for RecordingSink {
    fn record(
        &self,
        record: UsageRecord,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        if let Some(tx) = self.tx.lock().expect("tx mutex").as_ref() {
            let _ = tx.try_send(record);
        }
        Box::pin(async {})
    }

    fn shutdown(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let tx = self.tx.lock().expect("tx mutex").take();
            let join = self.join.lock().expect("join mutex").take();
            if let (Some(tx), Some(join)) = (tx, join) {
                drop(tx);
                let _ = tokio::time::timeout(std::time::Duration::from_secs(5), join).await;
            }
        })
    }
}

impl Drop for RecordingSink {
    fn drop(&mut self) {
        let tx = self.tx.lock().expect("tx mutex").take();
        let join = self.join.lock().expect("join mutex").take();
        drain_on_drop(tx, join);
    }
}

/// A reader that answers with a fixed aggregate — the seam the tenant-API tests use when the
/// property under test is the HTTP layer rather than a store.
pub(crate) struct FixedQuery {
    aggregate: UsageAggregate,
}

impl FixedQuery {
    pub(crate) fn new(aggregate: UsageAggregate) -> Self {
        Self { aggregate }
    }

    /// The shape the tenant API serves when everything is empty: a real zero, with `as_of: None`.
    pub(crate) fn empty() -> Self {
        Self::new(UsageAggregate {
            rows: Vec::new(),
            totals: hydra_core::tenant_api::UsageTotals::default(),
            as_of: None,
        })
    }
}

impl UsageQuery for FixedQuery {
    fn aggregate<'a>(
        &'a self,
        _tenant_id: &'a str,
        _since: &'a str,
        _until: &'a str,
        _group_by: GroupBy,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<UsageAggregate, UsageQueryError>> + Send + 'a>,
    > {
        let agg = self.aggregate.clone();
        Box::pin(async move { Ok(agg) })
    }

    fn source(&self) -> &'static str {
        "testing"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The harness answers the shape the tenant-API tests read: a real zero, no `as_of`, and its own
    /// source label (`"testing"` — never a production backend's).
    #[tokio::test]
    async fn the_fixed_reader_answers_a_real_zero() {
        let q = FixedQuery::empty();
        let agg = q
            .aggregate(
                "t1",
                "2026-09-15T00:00:00Z",
                "2026-09-16T00:00:00Z",
                GroupBy::None,
            )
            .await
            .expect("the harness always answers");
        assert_eq!(agg.totals.requests, 0);
        assert!(agg.rows.is_empty());
        assert_eq!(agg.as_of, None);
        assert_eq!(q.source(), "testing");
    }

    /// ...and the recording sink delivers what it is handed, so a test that sees nothing knows the
    /// sink was not fed rather than that the harness is broken.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_recording_sink_delivers_what_it_is_handed() {
        let sink = RecordingSink::new(1, 3600);
        sink.record(record("harness")).await;
        for _ in 0..50 {
            if !sink.delivered().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let got = sink.delivered();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].trace_id, "harness");
        sink.shutdown().await;
    }
}
