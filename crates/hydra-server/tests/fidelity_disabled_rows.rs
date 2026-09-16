//! Plan T1 / audit G1 — a replica must NOT lose the rows the runtime snapshot
//! filters out.
//!
//! THE DEFECT THIS GUARDS. `ConfigData` keeps only ENABLED `limit_role` /
//! `provider_key_binding` rows (that is what the hot path matches against), and
//! `db::restore_config` wipes both tables before rebuilding them. While the
//! rebuild source was `ConfigData`, every materialization on a replica deleted
//! its DISABLED rows — silently, and cluster-wide after a failover.
//!
//! So this test is written to FAIL against the pre-fix code: it seeds one
//! ENABLED and one DISABLED row of each kind, materializes a snapshot onto a
//! fresh replica, and asserts both rows survived.

mod common;

use std::sync::Arc;

use hydra_core::config::ConfigData;
use hydra_core::model::{LimitRole, Provider, ProviderKeyBinding};
use hydra_server::cluster::content::ReplicationContent;
use hydra_server::cluster::snapshot::SnapshotWire;
use hydra_server::crypto::{KeyProvider, StaticKeyProvider};
use hydra_server::{db as repo, store::ConfigStore};

fn now() -> &'static str {
    "2026-01-01 00:00:00"
}

fn kp() -> Arc<dyn KeyProvider> {
    Arc::new(StaticKeyProvider::new([1u8; 32], 1))
}

fn provider(id: &str) -> Provider {
    Provider {
        id: id.into(),
        key: format!("k-{id}"),
        name: id.into(),
        endpoint: "https://api.example.com".into(),
        weight: 1,
        created_at: now().into(),
        updated_at: now().into(),
        max_concurrency: None,
        max_queue_depth: None,
        queue_wait_timeout_ms: None,
    }
}

fn role(id: &str, enabled: bool) -> LimitRole {
    LimitRole {
        id: id.into(),
        name: id.into(),
        matching_key: None,
        matching_model: None,
        matching_tenant: None,
        matching_provider: None,
        limit_count: Some(10),
        limit_token: Some(1000),
        window: "m".into(),
        enabled,
        created_at: now().into(),
    }
}

fn binding(id: &str, prefix: &str, provider_id: &str, enabled: bool) -> ProviderKeyBinding {
    ProviderKeyBinding {
        id: id.into(),
        key_prefix: prefix.into(),
        provider_id: provider_id.into(),
        enabled,
        created_at: now().into(),
        updated_at: now().into(),
    }
}

/// Seed a leader whose config contains BOTH enabled and disabled rows.
async fn seed_leader(pool: &sqlx::SqlitePool) {
    repo::insert_provider(pool, &provider("p1"))
        .await
        .expect("insert provider");
    repo::insert_limit_role(pool, &role("r-on", true))
        .await
        .expect("insert enabled role");
    repo::insert_limit_role(pool, &role("r-off", false))
        .await
        .expect("insert disabled role");
    repo::insert_provider_key_binding(pool, &binding("b-on", "sk_on_", "p1", true))
        .await
        .expect("insert enabled binding");
    repo::insert_provider_key_binding(pool, &binding("b-off", "sk_off_", "p1", false))
        .await
        .expect("insert disabled binding");
}

#[tokio::test]
async fn replica_materialization_keeps_disabled_limit_roles_and_bindings() {
    let leader_pool = common::setup_pool().await;
    seed_leader(&leader_pool).await;
    let key_provider = kp();
    let leader_store = ConfigStore::load(leader_pool.clone(), key_provider.clone())
        .await
        .expect("ConfigStore::load");

    // Precondition: the RUNTIME snapshot really does drop the disabled rows —
    // otherwise this test would not be testing the defect it documents.
    let runtime: ConfigData = leader_store.snapshot().as_ref().clone();
    assert_eq!(
        runtime.limit_roles.len(),
        1,
        "the runtime snapshot keeps only the ENABLED role"
    );
    assert_eq!(
        runtime.key_prefix_bindings.len(),
        1,
        "the runtime snapshot keeps only the ENABLED binding"
    );

    // The wire is built from the leader's replication content (which carries the
    // FULL rows) and hydrated as a replica would.
    let content = leader_store
        .replication()
        .as_deref()
        .cloned()
        .expect("the leader has replication content");
    assert_eq!(
        content.fidelity().limit_roles.len(),
        2,
        "the replication content keeps BOTH roles (enabled and disabled)"
    );
    assert_eq!(
        content.fidelity().key_prefix_bindings.len(),
        2,
        "the replication content keeps BOTH bindings"
    );

    let wire = SnapshotWire::build(&content, key_provider.as_ref())
        .await
        .expect("build wire");
    // The wire is what the replica sees, so it must carry the full sets too.
    assert_eq!(wire.fidelity.limit_roles.len(), 2);
    assert_eq!(wire.fidelity.key_prefix_bindings.len(), 2);

    let hydrated = wire.hydrate(key_provider.as_ref()).expect("hydrate");

    let replica_pool = common::setup_pool().await;
    repo::restore_config(
        &replica_pool,
        key_provider.as_ref(),
        &hydrated.cfg,
        &hydrated.fidelity,
        hydrated.version,
    )
    .await
    .expect("restore_config");

    // THE ASSERTION THAT FAILS PRE-FIX: the replica holds every row.
    let roles = repo::list_limit_roles(&replica_pool).await.expect("roles");
    assert_eq!(roles.len(), 2, "the replica must keep BOTH limit roles");
    assert!(
        roles.iter().any(|r| r.id == "r-off" && !r.enabled),
        "the DISABLED limit role must survive materialization"
    );

    let bindings = repo::list_provider_key_bindings(&replica_pool)
        .await
        .expect("bindings");
    assert_eq!(bindings.len(), 2, "the replica must keep BOTH bindings");
    assert!(
        bindings.iter().any(|b| b.id == "b-off" && !b.enabled),
        "the DISABLED provider_key_binding must survive materialization"
    );
}

/// A second materialization must be a no-op for identical content — the guard
/// that keeps a replica from churning rows it already has right.
#[tokio::test]
async fn rematerializing_the_same_content_is_stable() {
    let leader_pool = common::setup_pool().await;
    seed_leader(&leader_pool).await;
    let key_provider = kp();
    let leader_store = ConfigStore::load(leader_pool.clone(), key_provider.clone())
        .await
        .expect("load");

    let content = leader_store
        .replication()
        .as_deref()
        .cloned()
        .expect("content");
    let wire = SnapshotWire::build(&content, key_provider.as_ref())
        .await
        .expect("build");

    let replica_pool = common::setup_pool().await;
    for _ in 0..2 {
        let hydrated = wire
            .clone()
            .hydrate(key_provider.as_ref())
            .expect("hydrate");
        repo::restore_config(
            &replica_pool,
            key_provider.as_ref(),
            &hydrated.cfg,
            &hydrated.fidelity,
            hydrated.version,
        )
        .await
        .expect("restore_config");
    }

    let roles = repo::list_limit_roles(&replica_pool).await.expect("roles");
    let bindings = repo::list_provider_key_bindings(&replica_pool)
        .await
        .expect("bindings");
    assert_eq!(roles.len(), 2, "no duplication and no loss on re-apply");
    assert_eq!(bindings.len(), 2, "no duplication and no loss on re-apply");
}

/// T6 acceptance — an edit to a DISABLED row must ADVANCE the generation.
///
/// This is the property that only the T1+T6 pair could break: `ConfigData`
/// cannot see disabled rows, so a generation predicate defined on the RUNTIME
/// config would treat "someone disabled a limit role" (or changed a disabled
/// role's window) as no change at all — the edit would then never replicate.
/// The predicate is defined on `ReplicationContent`, whose `PartialEq` includes
/// the full fidelity rows, so the edit must bump the version.
#[tokio::test]
async fn editing_a_disabled_row_advances_the_generation() {
    let pool = common::setup_pool().await;
    seed_leader(&pool).await;
    let key_provider = kp();
    let store = ConfigStore::load(pool.clone(), key_provider.clone())
        .await
        .expect("load");
    // Settle the baseline so the next reload is a strict comparison.
    store.reload_all().await.expect("baseline reload");
    let before = store.version();

    // A no-op reload first: unchanged ⇒ no advance (the control for this test).
    assert!(!store.reload_all().await.expect("no-op reload"));
    assert_eq!(store.version(), before);

    // Now change ONLY a disabled row: flip its `window` while it stays disabled.
    // This column is invisible to `ConfigData` (the row is filtered out of the
    // runtime view entirely), so a `ConfigData`-based predicate would miss it.
    let mut role = role("r-off", false);
    role.window = "h".into();
    repo::update_limit_role(&pool, &role)
        .await
        .expect("update the disabled role");
    assert!(
        !store.snapshot().limit_roles.iter().any(|r| r.id == "r-off"),
        "precondition: the disabled row is absent from the RUNTIME snapshot"
    );

    assert!(
        store.reload_all().await.expect("reload after the edit"),
        "a disabled-row edit IS a replicated change and must advance the generation"
    );
    assert_eq!(
        store.version(),
        before + 1,
        "exactly one generation for one edit"
    );
}

/// Byte-fidelity of the fidelity rows themselves: two loads of an UNCHANGED
/// database must be equal, or the generation predicate would bump on every
/// reload (the O10 idempotency property, asserted from the outside).
#[tokio::test]
async fn replication_content_is_idempotent_across_loads() {
    let pool = common::setup_pool().await;
    seed_leader(&pool).await;
    let key_provider = kp();

    let cfg: ConfigData = ConfigStore::load(pool.clone(), key_provider.clone())
        .await
        .expect("load")
        .snapshot()
        .as_ref()
        .clone();
    let a = ReplicationContent::load(&pool, key_provider.as_ref(), cfg.clone(), 1)
        .await
        .expect("first load");
    let b = ReplicationContent::load(&pool, key_provider.as_ref(), cfg, 1)
        .await
        .expect("second load");

    assert_eq!(
        a, b,
        "two loads of an unchanged DB must be equal — otherwise `reload_all` \
         advances the generation on every call and rebuilds every replica"
    );
}
