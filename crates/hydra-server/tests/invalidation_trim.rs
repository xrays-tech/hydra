//! Invalidation-stream trimming, against a REAL Redis.
//!
//! These two cases live in their own binary with their OWN Redis database on
//! purpose. `redis::test_redis::isolated_pool` (used by the lib unit tests)
//! hands out databases round-robin from a 1..=40 range, so two tests running
//! concurrently can land on the same database and operate on the SAME
//! `hydra:{ctl:events}` stream — a slow trim test then observes another test's
//! trim. That is a live instance of the DB-partition weakness the review flagged
//! for the test harness; integration tests get their own database here, exactly
//! as `tests/common/mod.rs` prescribes.
//!
//! Real Redis, never a double: dev-plan 铁律 2. `common::real_redis_pool`
//! PANICS when `HYDRA_TEST_REDIS_URL` is unset rather than skipping.
#![cfg(feature = "cluster-redis")]

use std::sync::Arc;
use std::time::Duration;

use hydra_server::cluster::events::{
    spawn_invalidation_consumer, spawn_trim_task, InvalidationStream,
};
use hydra_server::http::{AuthCache, AuthConfig, HttpAuthChecker};

mod common;

/// The scenario the review described, in the shipped constants' terms: a
/// sustained event rate above `maxlen / trim_interval` (10 000 / 30 s ≈ 333
/// events/s) used to make EVERY trim drop entries and therefore bump — i.e. wipe
/// every node's auth cache (L1+L2) on a 30 s schedule, which one tenant could
/// hold open with two invalidations a minute.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sustained_rate_above_maxlen_per_interval_does_not_bump_when_the_fleet_keeps_up() {
    let s = InvalidationStream::new(common::real_redis_pool(58).await);
    let maxlen = 100u64;
    let live = vec!["n1".to_string()];

    // Four "interval" rounds, each publishing more than maxlen — every round
    // therefore drops a full window, the shape that used to bump every time.
    for round in 0..4 {
        let mut last = String::new();
        for i in 0..(maxlen + 50) {
            last = s
                .publish(Some("t1".into()), vec![format!("sk-{round}-{i}")])
                .await
                .expect("publish");
        }
        // The fleet keeps up: the consumer applied everything published.
        s.mark_applied("n1", &last).await.expect("mark");
        let slowest = s
            .slowest_live_watermark(&live)
            .await
            .expect("watermark known");
        let (removed, bumped) = s
            .trim_and_maybe_bump(maxlen, Some(&slowest))
            .await
            .expect("trim");
        assert!(
            removed > 0,
            "round {round}: the trim must actually drop a window (removed {removed})"
        );
        assert!(
            !bumped,
            "round {round}: a keeping-up fleet must not get its caches wiped"
        );
        // The stream really is back at maxlen (the trim is what shrinks it).
        let remaining = s.read_since("0", maxlen + 1).await.expect("read");
        assert_eq!(
            remaining.len() as u64,
            maxlen,
            "round {round}: the stream is bounded to maxlen after the trim"
        );
    }
    assert_eq!(
        s.generation().await.expect("gen"),
        0,
        "no bump across 600 events at 1.5x the maxlen/interval rate"
    );
}

/// End-to-end: with a live view proving every dropped entry was applied, the
/// consumer's cache SURVIVES the trim (no bump ⇒ no fleet-wide clear).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_proven_safe_trim_leaves_the_consumer_cache_alone() {
    let pool = common::real_redis_pool(59).await;
    let stream = InvalidationStream::new(pool);
    let auth = Arc::new(
        HttpAuthChecker::new(
            AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
            AuthConfig::default(),
        )
        .expect("checker"),
    );
    auth.cache()
        .set("t1", "sk-a", true, Duration::from_secs(300))
        .await;
    assert_eq!(auth.cache().len(), 1, "seeded verdict before trim");

    // A store whose database holds no config and no fidelity rows is fine: the
    // published events target ANOTHER tenant, so they cannot clear the seeded `t1`
    // verdict — only a generation bump can.
    let store = hydra_server::store::ConfigStore::from_data(
        common::setup_pool().await,
        hydra_core::config::ConfigData::default(),
        Arc::new(hydra_server::crypto::StaticKeyProvider::new([1u8; 32], 1)),
    )
    .await
    .expect("from_data");
    spawn_invalidation_consumer(stream.clone(), auth.clone(), store, "test-node".to_string());

    // Publish first, then WAIT until the consumer's watermark covers the last id:
    // at that point every entry a trim can drop has provably been applied by every
    // live node. (Without that wait the consumer races the trim, and a bump is the
    // CORRECT outcome — that race is covered by `trim_bump_clears_consumer_cache`
    // in the lib suite.)
    let mut last_id = String::new();
    for i in 0..5 {
        last_id = stream
            .publish(Some("t9".to_string()), vec![format!("sk-{i}")])
            .await
            .expect("publish");
    }
    let live_id = vec!["test-node".to_string()];
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let marks = stream
                .applied_watermarks(&live_id)
                .await
                .expect("watermarks");
            if marks.get("test-node").map(String::as_str) == Some(last_id.as_str()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the consumer never caught up with the published events");

    let live: Arc<dyn Fn() -> Vec<String> + Send + Sync> =
        Arc::new(|| vec!["test-node".to_string()]);
    spawn_trim_task(stream.clone(), 2, Duration::from_millis(20), Some(live));

    // Several trim intervals' worth of time. The trims DO happen (the stream keeps
    // shrinking to maxlen) — they just need no compensation.
    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(
        auth.cache().len(),
        1,
        "the t1 verdict must SURVIVE: every dropped entry was already applied, so no bump \
         and no fleet-wide clear"
    );
    assert_eq!(
        stream.generation().await.expect("gen"),
        0,
        "no bump happened"
    );
}
