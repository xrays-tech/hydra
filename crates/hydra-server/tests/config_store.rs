//! §2.4 — ConfigStore ArcSwap shell: load / reload / swrr-clear / validate-fail.
//!
//! T6.4 (`reload_all` keeps breaker dead-set) and T6.6 (shared cert Arc) are
//! deferred to W4 — they depend on the `CircuitBreaker` and `HydraCertStore`
//! that wave introduces.

mod common;

use hydra_core::model::{Provider, ProviderModel, Tenant};
use hydra_core::swrr::SwrrState;
use hydra_server::crypto::{KeyProvider, StaticKeyProvider};
use hydra_server::{db as repo, store::ConfigStore};

fn now() -> &'static str {
    "2026-01-01 00:00:00"
}

/// Deterministic test key provider (never reads from the environment).
fn kp() -> std::sync::Arc<dyn KeyProvider> {
    std::sync::Arc::new(StaticKeyProvider::new([1u8; 32], 1))
}

fn provider(id: &str, key: &str) -> Provider {
    Provider {
        id: id.into(),
        key: key.into(),
        name: format!("{key} name"),
        endpoint: format!("https://{key}.example.com"),
        weight: 1,
        created_at: now().into(),
        updated_at: now().into(),
        max_concurrency: None,
        max_queue_depth: None,
        queue_wait_timeout_ms: None,
    }
}

fn tenant(id: &str, domain: &str) -> Tenant {
    Tenant {
        id: id.into(),
        name: format!("{id}-tenant"),
        domain: domain.into(),
        auth_url: format!("https://auth.{domain}/verify"),
        cert_key: None,
        cert_file: None,
        enabled: true,
        created_at: now().into(),
        updated_at: now().into(),
    }
}

async fn seed_basic(pool: &sqlx::SqlitePool) {
    repo::insert_provider(pool, &provider("p1", "openai"))
        .await
        .expect("p1");
    repo::insert_provider_model(
        pool,
        &ProviderModel {
            id: "m1".into(),
            key: "gpt-4".into(),
            name: "gpt-4".into(),
            provider_id: "p1".into(),
            status: 1,
        },
    )
    .await
    .expect("m1");
    repo::insert_tenant(pool, &tenant("t1", "acme.com"))
        .await
        .expect("t1");
}

/// T6.1 — `load()` populates the ArcSwap; `snapshot()` reflects the DB.
#[tokio::test]
async fn store_load_populates_arcswap() {
    let pool = common::setup_pool().await;
    seed_basic(&pool).await;

    let store = ConfigStore::load(pool, kp()).await.expect("load");
    let snap = store.snapshot();

    assert!(snap.providers.contains_key("p1"));
    assert!(snap.models_by_key.contains_key("gpt-4"));
    assert!(snap.tenants_by_domain.contains_key("acme.com"));
}

/// T6.2 — after a DB change, `reload_all()` swaps in the new snapshot; the
/// returned guard is a single consistent view (ArcSwap load semantics).
#[tokio::test]
async fn store_reload_all_replaces_atomically() {
    let pool = common::setup_pool().await;
    seed_basic(&pool).await;

    let store = ConfigStore::load(pool.clone(), kp()).await.expect("load");
    let before = store.snapshot();
    assert_eq!(before.providers.len(), 1);

    // Add a second provider + model.
    repo::insert_provider(&pool, &provider("p2", "azure"))
        .await
        .expect("p2");

    store.reload_all().await.expect("reload");

    let after = store.snapshot();
    assert_eq!(
        after.providers.len(),
        2,
        "snapshot must reflect the newly added provider"
    );
    // The old guard is unaffected (ArcSwap keeps the prior Arc alive).
    assert_eq!(before.providers.len(), 1);
}

/// T6.3 — `reload_all()` clears the SWRR DashMap (design §5.3 P1-B2).
#[tokio::test]
async fn store_reload_clears_swrr() {
    let pool = common::setup_pool().await;
    seed_basic(&pool).await;

    let store = ConfigStore::load(pool.clone(), kp()).await.expect("load");

    // Inject SWRR state as if requests had been served.
    store.swrr().insert(
        ("t1".into(), "gpt-4".into()),
        SwrrState {
            current_weights: [("p1".to_string(), 3)].into(),
        },
    );
    store
        .swrr()
        .insert(("t1".into(), "other".into()), SwrrState::default());
    assert_eq!(store.swrr().len(), 2, "precondition: two swrr entries");

    // Make a REAL change before reloading. `swrr.clear()` is part of the
    // "content changed" branch: an unchanged reload keeps the routing cache
    // (there is nothing to invalidate). Without this the test would assert that
    // a no-op reload clears SWRR — the opposite of the new contract (plan T6
    // step 7: make a real change, do NOT move `swrr.clear()` out of the branch).
    repo::insert_provider(&pool, &provider("p-extra", "extra"))
        .await
        .expect("seed a second provider");

    assert!(
        store.reload_all().await.expect("reload"),
        "a real DB change must advance the replication content"
    );
    assert!(
        store.swrr().is_empty(),
        "swrr must be cleared when the reload actually changed the content"
    );
}

/// T6 (the OTHER direction) — a no-op `reload_all()` must leave BOTH the
/// routing cache and the generation alone.
///
/// `store_reload_clears_swrr` above proves the changed branch clears SWRR; on
/// its own that would also pass if the "changed" check were inverted (always
/// true), so the no-op direction has to be pinned as well: an unchanged reload
/// keeps the SWRR state (nothing to invalidate) and does not advance the
/// version.
#[tokio::test]
async fn store_noop_reload_keeps_swrr_and_the_version() {
    let pool = common::setup_pool().await;
    seed_basic(&pool).await;

    let store = ConfigStore::load(pool.clone(), kp()).await.expect("load");
    // Settle the baseline: the first reload publishes the content the store was
    // constructed from (its version label may differ), so everything after this
    // must be a strict no-op.
    store.reload_all().await.expect("baseline reload");
    let version = store.version();

    store.swrr().insert(
        ("t1".into(), "gpt-4".into()),
        SwrrState {
            current_weights: [("p1".to_string(), 3)].into(),
        },
    );
    assert_eq!(store.swrr().len(), 1, "precondition: one swrr entry");

    // No DB write in between ⇒ the replicated bytes are identical.
    assert!(
        !store.reload_all().await.expect("no-op reload"),
        "an unchanged reload must report `changed == false`"
    );
    assert_eq!(store.version(), version, "and must not advance the version");
    assert_eq!(
        store.swrr().len(),
        1,
        "an unchanged reload keeps the routing cache — there is nothing to \
         invalidate, and clearing it would reset every tenant's smooth-weighted \
         rotation for no reason"
    );
}

/// T6 — a no-op reload must NOT notify snapshot followers.
///
/// `store::tests::every_snapshot_swap_notifies_followers` covers the changed
/// direction (a real change notifies); the plan's acceptance also requires the
/// negative half, because notifying on an unchanged reload is what made every
/// replica rebuild for nothing.
#[tokio::test]
async fn store_noop_reload_does_not_notify_followers() {
    let pool = common::setup_pool().await;
    seed_basic(&pool).await;
    let store = ConfigStore::load(pool.clone(), kp()).await.expect("load");

    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sink = calls.clone();
    store.on_snapshot_change(std::sync::Arc::new(move |_cfg| {
        sink.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));

    // Settle: publish the content the store was built from.
    store.reload_all().await.expect("baseline reload");
    let after_baseline = calls.load(std::sync::atomic::Ordering::SeqCst);

    // No DB write ⇒ identical content ⇒ no notify.
    assert!(!store.reload_all().await.expect("no-op reload"));
    assert!(!store.reload_all().await.expect("no-op reload again"));
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        after_baseline,
        "an unchanged reload must not notify: every follower would rebuild its \
         replica for nothing"
    );

    // ...and a REAL change still notifies (the hook is not simply broken).
    repo::insert_provider(&pool, &provider("p-notify", "notify"))
        .await
        .expect("seed a provider");
    assert!(store.reload_all().await.expect("changed reload"));
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        after_baseline + 1,
        "a changed reload notifies exactly once"
    );
}

/// T6.5 — a fatal validation issue makes `reload_all()` return `Err` and the
/// previous snapshot is kept (design §5.3).
#[tokio::test]
async fn store_reload_validate_fail_keeps_old() {
    let pool = common::setup_pool().await;
    seed_basic(&pool).await;

    let store = ConfigStore::load(pool.clone(), kp()).await.expect("load");
    let old_providers = store.snapshot().providers.len();

    // Inject a provider with an unusable endpoint → fatal endpoint check.
    let bad = Provider {
        id: "pbad".into(),
        key: "bad".into(),
        name: "bad".into(),
        endpoint: "not-a-url".into(),
        weight: 1,
        created_at: now().into(),
        updated_at: now().into(),
        max_concurrency: None,
        max_queue_depth: None,
        queue_wait_timeout_ms: None,
    };
    repo::insert_provider(&pool, &bad)
        .await
        .expect("insert bad provider");

    let err = store
        .reload_all()
        .await
        .expect_err("reload must fail on fatal");
    let msg = format!("{err}");
    assert!(
        msg.contains("fatal") && msg.contains("pbad"),
        "expected fatal validation naming the bad provider, got: {msg}"
    );

    // Snapshot unchanged: the bad provider is NOT visible.
    let snap = store.snapshot();
    assert_eq!(
        snap.providers.len(),
        old_providers,
        "old snapshot must be kept when validation fails"
    );
    assert!(!snap.providers.contains_key("pbad"));
}
