//! Phase 3c — prove the shared-DB config fanout against a REAL Redis.
//!
//! What these tests are for (contract §7.5): a write on node A advances the
//! shared `config_revision`, A publishes `{version, origin}`, and node B's
//! watcher reloads its in-process snapshot WITHOUT B writing anything. A missed
//! message is repaired by the poller. Subscriber/poll reloads never publish, so
//! the fleet cannot amplify a change into a broadcast storm. A message that
//! predates B's load cannot roll the snapshot back.
//!
//! Test isolation notes:
//!   * the relational side uses one isolated `TestDb` per test (`db::testing`);
//!   * Redis is real (never a mock, dev-plan 铁律 2); each test gets its own
//!     database via `common::real_redis_pool`;
//!   * **Redis pub/sub is server-global, not database-scoped**, so databases do
//!     NOT isolate subscribers. Each test therefore uses its OWN channel through
//!     the explicitly test-support `*_on`/`with_channel` helpers, otherwise a
//!     parallel test's message could trigger a reload here.
//!
//! Every positive assertion is a bounded poll, and the two "must not move"
//! assertions use a bounded window — no un-checked fixed sleeps.

#![cfg(feature = "cluster-redis")]

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hydra_core::model::Provider;
use hydra_server::cluster::config_bus::{spawn_config_watch_on, RedisConfigNotifier};
use hydra_server::crypto::{KeyProvider, StaticKeyProvider};
use hydra_server::db;
use hydra_server::redis::build_subscriber;
use hydra_server::store::{ConfigNotifier, ConfigStore};

// Redis databases reserved for this test binary (see `tests/common/mod.rs`:
// integration tests hand-assign 41..=63, and `tests/redis_db_partition.rs`
// scans the literal `real_redis_pool(<n>)` calls below for collisions).
//   pub/sub convergence = 41, poll fallback = 42, no-relay = 43,
//   startup race = 44/45.

fn kp() -> Arc<dyn KeyProvider> {
    Arc::new(StaticKeyProvider::new([1u8; 32], 1))
}

fn provider(id: &str) -> Provider {
    Provider {
        id: id.into(),
        key: format!("key-{id}"),
        name: format!("{id} name"),
        endpoint: format!("https://{id}.example.com"),
        weight: 1,
        created_at: "2026-01-01 00:00:00".into(),
        updated_at: "2026-01-01 00:00:00".into(),
        max_concurrency: None,
        max_queue_depth: None,
        queue_wait_timeout_ms: None,
    }
}

/// URL for a specific database of the test Redis instance.
fn redis_url(database: u8) -> String {
    let base = std::env::var("HYDRA_TEST_REDIS_URL")
        .expect("HYDRA_TEST_REDIS_URL is set by every path that calls this");
    hydra_server::redis::database_url(&base, database)
}

/// A per-test pub/sub namespace (Redis channels are NOT database-scoped).
fn channel(name: &str) -> String {
    format!("hydra:test:config:changed:{name}")
}

/// Counts every publish, optionally forwarding it to the real Redis transport.
///
/// Used twice: on node A to assert "exactly one announcement per local write"
/// while still delivering it, and on node B to assert "a subscriber reload is
/// quiet" (no inner transport at all).
struct CountingNotifier {
    inner: Option<RedisConfigNotifier>,
    calls: AtomicU64,
    last_version: AtomicU64,
}

impl CountingNotifier {
    fn with_redis(inner: RedisConfigNotifier) -> Arc<Self> {
        Arc::new(Self {
            inner: Some(inner),
            calls: AtomicU64::new(0),
            last_version: AtomicU64::new(0),
        })
    }

    fn silent() -> Arc<Self> {
        Arc::new(Self {
            inner: None,
            calls: AtomicU64::new(0),
            last_version: AtomicU64::new(0),
        })
    }

    fn calls(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }

    fn last_version(&self) -> u64 {
        self.last_version.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ConfigNotifier for CountingNotifier {
    async fn publish(&self, version: u64) -> Result<(), String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.last_version.store(version, Ordering::SeqCst);
        match &self.inner {
            Some(inner) => inner.publish(version).await,
            None => Ok(()),
        }
    }
}

/// Poll until `predicate` holds, with a hard deadline. The terminal condition is
/// part of the assertion; a timeout names what never happened.
async fn wait_until<F>(label: &str, timeout: Duration, mut predicate: F)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if predicate() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out after {timeout:?} waiting for {label}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Assert `predicate` keeps holding for a whole window. This is the negative
/// half of the startup-race test: "it did not change" is only meaningful over an
/// interval in which a delivery could have arrived.
async fn assert_unchanged_for<F>(label: &str, window: Duration, mut predicate: F)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + window;
    while tokio::time::Instant::now() < deadline {
        assert!(predicate(), "{label}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// 1. The fast path: A writes + publishes, B converges without writing.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_stores_converge_via_pubsub() {
    let db = db::testing::isolated().await;
    let channel = channel("pubsub");

    let store_a = ConfigStore::load(db.pool.clone(), kp())
        .await
        .expect("load A");
    let store_b = ConfigStore::load(db.pool.clone(), kp())
        .await
        .expect("load B");
    let before = store_b.version();

    // One Redis database, one dedicated subscriber on the same database.
    let pool = common::real_redis_pool(41).await;
    let subscriber = build_subscriber(&redis_url(41)).expect("subscriber builds");
    let store_a = store_a.with_notifier(Arc::new(RedisConfigNotifier::with_channel(
        pool,
        "node-a".to_string(),
        channel.clone(),
    )));
    // A 60 s poll interval makes pub/sub the ONLY path to convergence inside the
    // assertion window. A 200 ms poller (the old value) let a BROKEN pub/sub path
    // still pass, because the fallback could repair the change well within 3 s.
    spawn_config_watch_on(
        store_b.clone(),
        subscriber,
        Duration::from_secs(60),
        &channel,
    )
    .await
    .expect("watcher starts");

    // A writes the shared DB (the insert bumps `config_revision`), then reloads.
    // B is never written to: convergence must come from the announcement.
    db::insert_provider(&db.pool, &provider("p-pubsub"))
        .await
        .expect("insert provider");
    assert!(
        store_a.reload_local(false).await.expect("A reload"),
        "the committed write must advance A's revision"
    );
    let a_version = store_a.version();

    // ~1 s is hundreds of times the normal delivery latency and far below one
    // 60 s poll tick, so success here PROVES the pub/sub path carried the change.
    wait_until(
        "B to reach A's version through pub/sub (the poller cannot run for 60 s)",
        Duration::from_secs(1),
        || store_b.version() == a_version && store_b.snapshot().providers.contains_key("p-pubsub"),
    )
    .await;
    assert!(
        store_b.version() > before,
        "B advanced from the announcement, without its own write"
    );
    assert_eq!(
        store_b.snapshot().providers.len(),
        1,
        "B's snapshot carries exactly the entity A wrote"
    );
}

// ---------------------------------------------------------------------------
// 2. The correctness backstop: no announcement reaches B, the poller repairs it.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poll_fallback_converges_without_pubsub() {
    let db = db::testing::isolated().await;
    let channel = channel("fallback");

    let store_a = ConfigStore::load(db.pool.clone(), kp())
        .await
        .expect("load A");
    let store_b = ConfigStore::load(db.pool.clone(), kp())
        .await
        .expect("load B");
    let before = store_b.version();

    // The message never leaves A: a silent notifier simulates the outage /
    // missed-message case. (A different Redis DATABASE would NOT do this: Redis
    // pub/sub is server-global, so the subscriber would still receive it.)
    let publisher = CountingNotifier::silent();
    let store_a = store_a.with_notifier(publisher.clone());

    // B's subscriber is REAL and live, but no message is ever published on this
    // test's channel. Convergence can only come from the 200 ms revision probe.
    let subscriber = build_subscriber(&redis_url(42)).expect("subscriber builds");
    spawn_config_watch_on(
        store_b.clone(),
        subscriber,
        Duration::from_millis(200),
        &channel,
    )
    .await
    .expect("watcher starts");

    db::insert_provider(&db.pool, &provider("p-fallback"))
        .await
        .expect("insert provider");
    assert!(store_a.reload_local(false).await.expect("A reload"));
    assert_eq!(
        publisher.calls(),
        1,
        "A attempted exactly one announcement (suppressed at the transport)"
    );
    let a_version = store_a.version();

    wait_until(
        "B to converge through the poll fallback",
        Duration::from_secs(3),
        || {
            store_b.version() == a_version
                && store_b.snapshot().providers.contains_key("p-fallback")
        },
    )
    .await;
    assert!(
        store_b.version() > before,
        "the DB revision probe advanced B without any pub/sub message"
    );
}

// ---------------------------------------------------------------------------
// 3. No broadcast storm: consuming a message must never relay it.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscriber_reload_does_not_relay() {
    let db = db::testing::isolated().await;
    let channel = channel("no-relay");

    let store_b = ConfigStore::load(db.pool.clone(), kp())
        .await
        .expect("load B");
    // B's notifier counts any attempt to re-announce the change.
    let b_notifier = CountingNotifier::silent();
    let store_b = store_b.with_notifier(b_notifier.clone());

    let pool = common::real_redis_pool(43).await;
    let store_a = ConfigStore::load(db.pool.clone(), kp())
        .await
        .expect("load A");
    let a_notifier = CountingNotifier::with_redis(RedisConfigNotifier::with_channel(
        pool,
        "node-a".to_string(),
        channel.clone(),
    ));
    let store_a = store_a.with_notifier(a_notifier.clone());

    let subscriber = build_subscriber(&redis_url(43)).expect("subscriber builds");
    // A 60 s poll interval makes the subscriber the ONLY path to convergence;
    // otherwise the poller could mask a broken pub/sub path.
    spawn_config_watch_on(
        store_b.clone(),
        subscriber,
        Duration::from_secs(60),
        &channel,
    )
    .await
    .expect("watcher starts");

    db::insert_provider(&db.pool, &provider("p-no-relay"))
        .await
        .expect("insert provider");
    assert!(store_a.reload_local(false).await.expect("A reload"));
    let a_version = store_a.version();

    wait_until(
        "B to reload from the pub/sub message (not the 60 s poller)",
        Duration::from_secs(2),
        || {
            store_b.version() == a_version
                && store_b.snapshot().providers.contains_key("p-no-relay")
        },
    )
    .await;
    assert_eq!(
        a_notifier.calls(),
        1,
        "A announced its one local change exactly once"
    );
    assert_eq!(
        a_notifier.last_version(),
        a_version,
        "the announcement carried the newly applied revision"
    );
    assert_eq!(
        b_notifier.calls(),
        0,
        "B's subscriber-driven reload must never publish (no broadcast storm)"
    );
}

// ---------------------------------------------------------------------------
// 4. Startup race: stale/pre-load messages cannot roll the snapshot backwards.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_race_does_not_roll_back() {
    let db = db::testing::isolated().await;

    // --- (a) A stale message arriving while the watcher starts is ignored. ---
    let channel_a = channel("race-live");
    let pool_a = common::real_redis_pool(44).await;
    let stale =
        RedisConfigNotifier::with_channel(pool_a.clone(), "stale".to_string(), channel_a.clone());
    let store_a = ConfigStore::load(db.pool.clone(), kp())
        .await
        .expect("load A")
        .with_notifier(Arc::new(RedisConfigNotifier::with_channel(
            pool_a,
            "node-a".to_string(),
            channel_a.clone(),
        )));
    let store_b = ConfigStore::load(db.pool.clone(), kp())
        .await
        .expect("load B");
    let b_before = store_b.version();

    let subscriber = build_subscriber(&redis_url(44)).expect("subscriber builds");
    spawn_config_watch_on(
        store_b.clone(),
        subscriber,
        Duration::from_secs(60),
        &channel_a,
    )
    .await
    .expect("watcher starts");

    // `b_before` is exactly B's applied revision: the message is not ahead, so
    // the store's gate must ignore it.
    stale
        .publish(b_before)
        .await
        .expect("publish a stale version");
    assert_unchanged_for(
        "a stale announcement moved (or rolled back) B's version",
        Duration::from_millis(200),
        || store_b.version() == b_before,
    )
    .await;
    assert!(
        !store_b.snapshot().providers.contains_key("p-race"),
        "the stale message carried no content and changed no snapshot"
    );

    // A valid newer message still converges: the watcher was alive all along,
    // and the ignored message did not wedge it.
    db::insert_provider(&db.pool, &provider("p-race"))
        .await
        .expect("insert provider");
    assert!(store_a.reload_local(false).await.expect("A reload"));
    let a_version = store_a.version();
    wait_until(
        "B to apply the valid message after ignoring the stale one",
        Duration::from_secs(2),
        || store_b.version() == a_version && store_b.snapshot().providers.contains_key("p-race"),
    )
    .await;
    assert!(
        store_b.version() >= b_before,
        "the ignored stale message never rolled the version back"
    );

    // --- (b) A message published BEFORE B's initial load cannot regress it. ---
    let channel_b = channel("race-preload");
    let pool_b = common::real_redis_pool(45).await;
    // A loads, writes, reloads and announces — all before B even exists.
    let store_a2 = ConfigStore::load(db.pool.clone(), kp())
        .await
        .expect("load A2")
        .with_notifier(Arc::new(RedisConfigNotifier::with_channel(
            pool_b.clone(),
            "node-a2".to_string(),
            channel_b.clone(),
        )));
    db::insert_provider(&db.pool, &provider("p-preload"))
        .await
        .expect("insert provider");
    assert!(store_a2.reload_local(false).await.expect("A2 reload"));
    let preload_version = store_a2.version();

    // B now loads and must resume the LATEST watermark, not the published one.
    let store_b2 = ConfigStore::load(db.pool.clone(), kp())
        .await
        .expect("load B2");
    assert_eq!(
        store_b2.version(),
        preload_version,
        "B's initial load reads the committed revision"
    );
    assert!(
        store_b2.snapshot().providers.contains_key("p-preload"),
        "B's initial snapshot already contains the pre-load write"
    );

    let subscriber = build_subscriber(&redis_url(45)).expect("subscriber builds");
    spawn_config_watch_on(
        store_b2.clone(),
        subscriber,
        Duration::from_secs(60),
        &channel_b,
    )
    .await
    .expect("watcher starts");

    // A stale announcement after the watcher starts still cannot roll it back.
    let stale = RedisConfigNotifier::with_channel(pool_b, "stale".to_string(), channel_b.clone());
    stale
        .publish(preload_version.saturating_sub(1))
        .await
        .expect("publish a stale version");
    assert_unchanged_for(
        "B's post-load snapshot regressed",
        Duration::from_millis(200),
        || {
            store_b2.version() == preload_version
                && store_b2.snapshot().providers.contains_key("p-preload")
        },
    )
    .await;

    // Positive sentinel: the watcher is live and only moves FORWARD.
    db::insert_provider(&db.pool, &provider("p-postload"))
        .await
        .expect("insert provider");
    assert!(store_a2.reload_local(false).await.expect("A2 reload"));
    let postload_version = store_a2.version();
    wait_until(
        "B to apply the newer post-load write",
        Duration::from_secs(2),
        || {
            store_b2.version() == postload_version
                && store_b2.snapshot().providers.contains_key("p-postload")
        },
    )
    .await;
    assert!(
        store_b2.version() >= preload_version,
        "B never regressed below the revision read at load"
    );
    assert!(
        store_b2.snapshot().providers.contains_key("p-preload"),
        "the pre-load entity survived the later reload"
    );
}
