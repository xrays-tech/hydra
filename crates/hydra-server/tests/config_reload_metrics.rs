//! Full-reload cost metrics: duration is exposed for every attempt, and the
//! rows gauge moves only when a new snapshot is actually swapped in.
#![cfg(feature = "proxy")]

mod common;

use std::sync::Arc;

use hydra_server::admin::metrics;
use hydra_server::crypto::{KeyProvider, StaticKeyProvider};
use hydra_server::store::ConfigStore;

fn kp() -> Arc<dyn KeyProvider> {
    Arc::new(StaticKeyProvider::new([1u8; 32], 1))
}

async fn seed_provider(pool: &hydra_server::db::Pool, id: &str) {
    hydra_server::db::insert_provider(
        pool,
        &hydra_core::model::Provider {
            id: id.to_string(),
            key: format!("k-{id}"),
            name: id.to_string(),
            endpoint: "http://127.0.0.1:1/".to_string(),
            weight: 1,
            created_at: "2026-01-01 00:00:00".to_string(),
            updated_at: "2026-01-01 00:00:00".to_string(),
            max_concurrency: None,
            max_queue_depth: None,
            queue_wait_timeout_ms: None,
        },
    )
    .await
    .expect("seed provider");
}

#[tokio::test]
async fn successful_swap_sets_rows_and_noop_leaves_it_untouched() {
    let pool = common::setup_pool().await;
    let store = ConfigStore::load(pool.clone(), kp()).await.expect("load");

    // A fresh DB has no newer revision: this is a reload attempt but NOT a swap.
    let before_noop = metrics::config_rows_loaded();
    assert!(
        !store.reload_local(false).await.expect("no-op reload"),
        "fresh DB must be a no-op"
    );
    assert_eq!(
        metrics::config_rows_loaded(),
        before_noop,
        "a no-op reload must leave the rows gauge unchanged"
    );
    assert!(
        metrics::render().contains("hydra_config_reload_duration_seconds_count"),
        "the no-op attempt still records duration"
    );

    seed_provider(&pool, "p-metrics-rows").await;
    assert!(
        store.reload_local(false).await.expect("changed reload"),
        "the seeded provider advances the revision"
    );
    let after_swap = metrics::config_rows_loaded();
    assert!(
        after_swap >= 1.0,
        "a successful swap sets rows from the new snapshot: {after_swap}"
    );

    // The next reload has nothing new to apply: the gauge must stay where the
    // successful swap put it.
    assert!(
        !store.reload_local(false).await.expect("no-op reload"),
        "nothing advanced since the swap"
    );
    assert_eq!(
        metrics::config_rows_loaded(),
        after_swap,
        "a no-op reload must not rewrite the rows gauge"
    );
}
