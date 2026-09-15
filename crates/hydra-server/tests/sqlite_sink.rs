//! Wave-3 §2.3 — `SqliteSink` (design §9.2).
//!
//! Real `:memory:` SQLite engine via `db::init_pool` — NO mocks. Exercises the
//! `UsageSink` trait, channel batching (size + time), exponential-backoff retry
//! on transient DB errors, key-masking pass-through, and Drop-drain.

#![cfg(feature = "db")]

mod common;

use std::time::Duration;

use hydra_core::model::UsageRecord;
use hydra_core::rewrite::mask_key;
use hydra_server::sink::{build_sink, BuildSinkError, SqliteSink, UsageSink};
use sqlx::Row;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A sample `UsageRecord` with distinguishing fields driven by `i`.
fn rec(i: u32) -> UsageRecord {
    UsageRecord {
        tenant_id: format!("tenant-{i}"),
        provider_id: format!("provider-{i}"),
        model_key: "gpt-test".to_string(),
        // The sink must NOT mask — it persists whatever masked string the caller
        // hands it. Here we use a real masked value produced by core `mask_key`
        // (design §9.5) to prove the contract end-to-end.
        client_api_key_masked: Some(mask_key("sk-abcd1234wxyz0987")),
        status_code: 200,
        tokens_in: Some(10 + i as u64),
        tokens_out: Some(20 + i as u64),
        cache_hit_tokens: Some(i as u64),
        latency_ms: 100 + i as u64,
        forward_latency_ms: Some(5 + i as u64),
        ttft_ms: Some(50 + i as u64),
        upstream_host: Some("upstream.example".to_string()),
        error: None,
        trace_id: format!("trace-{i}"),
        created_at: "2026-08-08T00:00:00Z".to_string(),
    }
}

async fn count_usage(pool: &sqlx::SqlitePool) -> i64 {
    let row = sqlx::query("SELECT COUNT(*) AS c FROM usage_record")
        .fetch_one(pool)
        .await
        .expect("count usage_record");
    row.get::<i64, _>("c")
}

async fn stored_keys(pool: &sqlx::SqlitePool) -> Vec<Option<String>> {
    let rows = sqlx::query("SELECT client_api_key FROM usage_record ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("select client_api_key");
    rows.into_iter()
        .map(|r| r.get::<Option<String>, _>("client_api_key"))
        .collect()
}

/// Push `n` records into `sink` (awaiting each non-blocking `record()`).
async fn push_n(sink: &SqliteSink, n: u32) {
    for i in 0..n {
        sink.record(rec(i)).await;
    }
}

// ---------------------------------------------------------------------------
// T3.1 — records land in usage_record after a flush
// ---------------------------------------------------------------------------

/// `record(UsageRecord)` ×N → after a flush the table has the corresponding rows.
/// Triggers the time-based flush (flush_secs=1, fewer than batch_size).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sink_records_to_usage_table() {
    let pool = common::setup_pool().await;

    let sink = SqliteSink::new(pool.clone(), 100, 1);
    push_n(&sink, 5).await;

    // flush_secs=1 → a time-flush fires within ~1s even though batch_size=100
    // was never reached.
    for _ in 0..50 {
        if count_usage(&pool).await == 5 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(count_usage(&pool).await, 5, "5 records should have flushed");
}

// ---------------------------------------------------------------------------
// T3.2 — batch-by-size triggers an immediate flush at N
// ---------------------------------------------------------------------------

/// Sending exactly `batch_size` records triggers one immediate batch INSERT,
/// without waiting for the time-flush.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sink_batches_by_size() {
    let pool = common::setup_pool().await;

    // Large flush_secs so only the size threshold can fire within the test.
    let sink = SqliteSink::new(pool.clone(), 4, 3600);
    push_n(&sink, 4).await;

    // The size-flush is asynchronous; poll briefly for it to land.
    for _ in 0..50 {
        if count_usage(&pool).await == 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        count_usage(&pool).await,
        4,
        "batch_size reached → immediate flush"
    );
}

// ---------------------------------------------------------------------------
// T3.3 — batch-by-time triggers at flush_secs even if < N
// ---------------------------------------------------------------------------

/// Fewer than `batch_size` but exceeding `flush_secs` still flushes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sink_batches_by_time() {
    let pool = common::setup_pool().await;

    let sink = SqliteSink::new(pool.clone(), 1000, 1);
    push_n(&sink, 3).await;

    // No size flush possible (3 < 1000); only the 1s ticker can flush.
    for _ in 0..50 {
        if count_usage(&pool).await == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        count_usage(&pool).await,
        3,
        "time-based flush must fire even below batch_size"
    );
}

// ---------------------------------------------------------------------------
// T3.4 — backoff on a transient DB error, then succeed (never blocking callers)
// ---------------------------------------------------------------------------

/// Simulate a transient DB outage (DROP TABLE), let the sink retry with
/// exponential backoff, then recover (re-migrate) and confirm the buffered
/// batch lands. `record()` never blocks the caller throughout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sink_backoff_on_db_error() {
    let pool = common::setup_pool().await;

    // Break the schema: INSERTs will now fail with "no such table".
    sqlx::query("DROP TABLE usage_record")
        .execute(&pool)
        .await
        .expect("drop usage_record");

    // batch_size=2 so the first two records trigger a size-flush immediately
    // (which fails on the missing table and enters the backoff loop).
    let sink = SqliteSink::new(pool.clone(), 2, 3600);

    // record() must return immediately even while the DB is broken.
    let t0 = std::time::Instant::now();
    push_n(&sink, 2).await;
    let push_elapsed = t0.elapsed();
    assert!(
        push_elapsed < Duration::from_millis(500),
        "record() must be non-blocking even during DB outage (took {:?})",
        push_elapsed
    );

    // Let the bg task hit the broken DB a few times (backoff retries). While the
    // table is missing nothing can be persisted — we don't SELECT from the
    // (dropped) table here; the later post-recovery count proves nothing leaked
    // through during the outage (it equals exactly the batch size).
    tokio::time::sleep(Duration::from_millis(250)).await;

    // Recover: recreate the table. (We can't re-run `run_migrate` — its
    // `_sqlx_migrations` bookkeeping still records 0001_init as applied, so it
    // would be a no-op. Recreate the table directly via DDL instead.)
    sqlx::query(
        "CREATE TABLE usage_record (\
         id INTEGER PRIMARY KEY AUTOINCREMENT, \
         tenant_id TEXT NOT NULL, provider_id TEXT NOT NULL, model_key TEXT NOT NULL, \
         client_api_key TEXT, status_code INTEGER NOT NULL, \
         tokens_in INTEGER, tokens_out INTEGER, cache_hit_tokens INTEGER, \
         latency_ms INTEGER NOT NULL, forward_latency_ms INTEGER, ttft_ms INTEGER, \
         upstream_host TEXT, error TEXT, \
         created_at TEXT NOT NULL DEFAULT (datetime('now')))",
    )
    .execute(&pool)
    .await
    .expect("recreate usage_record");

    // The bg task's backoff retry should now succeed. Poll generously to allow
    // the current backoff interval to elapse (initial 50ms, doubling).
    for _ in 0..100 {
        if count_usage(&pool).await == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        count_usage(&pool).await,
        2,
        "after recovery the backoff retry must flush the buffered batch"
    );
}

// ---------------------------------------------------------------------------
// T3.5 — stored client_api_key is whatever masked string was passed (sink does
//         NOT mask; caller produces it via core mask_key)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sink_mask_key_stored() {
    let pool = common::setup_pool().await;

    let masked = mask_key("sk-abcd1234wxyz0987"); // produced by the CALLER
    let mut r = rec(0);
    r.client_api_key_masked = Some(masked.clone());

    let sink = SqliteSink::new(pool.clone(), 1, 3600);
    sink.record(r).await;

    for _ in 0..50 {
        if count_usage(&pool).await == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let keys = stored_keys(&pool).await;
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].as_deref(), Some(masked.as_str()));
    // Sanity: the stored value is the masked form, never the plaintext.
    assert_ne!(keys[0].as_deref(), Some("sk-abcd1234wxyz0987"));
}

// ---------------------------------------------------------------------------
// T3.6 — Drop flushes remaining (graceful shutdown)
// ---------------------------------------------------------------------------

/// Records buffered but not yet flushed (< batch_size, before flush_secs) are
/// flushed when the sink is dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sink_drop_drains() {
    let pool = common::setup_pool().await;

    let count = {
        let sink = SqliteSink::new(pool.clone(), 1000, 3600);
        push_n(&sink, 5).await;
        // Nothing flushed yet: below batch_size, well within flush_secs.
        assert_eq!(count_usage(&pool).await, 0, "precondition: nothing flushed");
        // Drop here → the impl's Drop closes the channel and best-effort
        // synchronously drains + final-flushes the buffer.
        drop(sink);
        // (block_in_place in Drop makes this synchronous on a multi-thread rt.)
        count_usage(&pool).await
    };
    assert_eq!(count, 5, "Drop must flush the 5 remaining buffered records");
}

// ---------------------------------------------------------------------------
// T3.7 — new metrics dimensions (cached_tokens / forward_latency_ms / ttft_ms)
//         are persisted by the SqliteSink INSERT.
// ---------------------------------------------------------------------------

/// The 0002 migration columns are written by the sink. Verifies the full
/// INSERT path round-trips the three new (nullable) dimensions into SQLite.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sink_persists_new_metrics_columns() {
    let pool = common::setup_pool().await;

    let sink = SqliteSink::new(pool.clone(), 1, 3600);
    sink.record(rec(2)).await;

    for _ in 0..50 {
        if count_usage(&pool).await == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(count_usage(&pool).await, 1);

    // rec(2) ⇒ cache_hit_tokens=2, forward_latency_ms=7, ttft_ms=52.
    let row = sqlx::query(
        "SELECT cache_hit_tokens, forward_latency_ms, ttft_ms FROM usage_record \
         WHERE tenant_id = ?",
    )
    .bind("tenant-2")
    .fetch_one(&pool)
    .await
    .expect("select new metrics");
    let cached: Option<i64> = row.get("cache_hit_tokens");
    let fwd: Option<i64> = row.get("forward_latency_ms");
    let ttft: Option<i64> = row.get("ttft_ms");
    assert_eq!(cached, Some(2), "cache_hit_tokens persisted");
    assert_eq!(fwd, Some(7), "forward_latency_ms persisted");
    assert_eq!(ttft, Some(52), "ttft_ms persisted");
}

/// When the new dimensions are `None` the columns persist as SQL NULL
/// (nullable schema — pre-existing rows and providers that omit the fields
/// keep working).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sink_new_metrics_null_when_absent() {
    let pool = common::setup_pool().await;

    let mut r = rec(0);
    r.cache_hit_tokens = None;
    r.forward_latency_ms = None;
    r.ttft_ms = None;

    let sink = SqliteSink::new(pool.clone(), 1, 3600);
    sink.record(r).await;
    drop(sink);

    for _ in 0..50 {
        if count_usage(&pool).await == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let row = sqlx::query(
        "SELECT cache_hit_tokens, forward_latency_ms, ttft_ms FROM usage_record \
         WHERE tenant_id = ?",
    )
    .bind("tenant-0")
    .fetch_one(&pool)
    .await
    .expect("select null metrics");
    let cached: Option<i64> = row.get("cache_hit_tokens");
    let fwd: Option<i64> = row.get("forward_latency_ms");
    let ttft: Option<i64> = row.get("ttft_ms");
    assert_eq!(cached, None, "cache_hit_tokens None ⇒ NULL");
    assert_eq!(fwd, None, "forward_latency_ms None ⇒ NULL");
    assert_eq!(ttft, None, "ttft_ms None ⇒ NULL");
}

// ---------------------------------------------------------------------------
// T4.3 — config-driven sink selection (trait swap by config)
// ---------------------------------------------------------------------------

/// `build_sink("sqlite", pool, _)` → a usable `SqliteSink` behind `dyn UsageSink`.
/// Unknown kinds and missing required args return typed errors (no panic).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sink_trait_swap_by_config() {
    let pool = common::setup_pool().await;

    // sqlite selection → SqliteSink behind the trait object.
    let boxed = build_sink("sqlite", Some(pool.clone()), None).expect("sqlite sink builds");
    boxed.record(rec(0)).await;
    drop(boxed);

    for _ in 0..50 {
        if count_usage(&pool).await == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(count_usage(&pool).await, 1);

    // Missing required pool → typed error (not a panic). (`matches!` avoids the
    // `Debug` bound that `unwrap_err` would require on `dyn UsageSink`.)
    assert!(
        matches!(
            build_sink("sqlite", None, None),
            Err(BuildSinkError::MissingPool)
        ),
        "sqlite without a pool should be MissingPool"
    );

    // Unknown kind → typed error.
    assert!(
        matches!(
            build_sink("flat-file", None, None),
            Err(BuildSinkError::UnknownKind { .. })
        ),
        "unknown kind should be UnknownKind"
    );
}

/// The explicit `shutdown()` must flush the buffer WITHOUT relying on `Drop`,
/// and must be idempotent (a later `Drop` is a no-op).
///
/// `Drop` alone is not enough: `main` runs pingora's `run_forever`, which ends
/// in `std::process::exit(0)` — no destructors run — so on SIGTERM the
/// in-flight batch, the records queued in the channel and up to `flush_secs` of
/// traffic were discarded. This is the path the SIGTERM handler drives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sink_shutdown_flushes_without_drop() {
    let pool = common::setup_pool().await;
    // batch_size far above N and flush_secs far in the future: only an
    // explicit shutdown can persist these.
    let sink = SqliteSink::new(pool.clone(), 1000, 3600);
    push_n(&sink, 4).await;
    assert_eq!(
        count_usage(&pool).await,
        0,
        "precondition: nothing flushed yet"
    );

    // Explicit shutdown with the sink still alive — no Drop involved.
    sink.shutdown().await;
    assert_eq!(
        count_usage(&pool).await,
        4,
        "shutdown() must drain and flush the buffered records"
    );

    // Idempotent: a second call and the eventual Drop must neither hang nor
    // re-insert anything.
    sink.shutdown().await;
    assert_eq!(count_usage(&pool).await, 4);
    drop(sink);
    assert_eq!(
        count_usage(&pool).await,
        4,
        "Drop after shutdown is a no-op"
    );
}
