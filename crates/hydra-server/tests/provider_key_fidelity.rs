//! Plan T1 / audit G3 — a replica must reproduce `provider_key` rows WITH their
//! identity.
//!
//! THE DEFECT THIS GUARDS. `restore_config` used to rebuild `provider_key` from
//! `cfg.provider_keys` (the plaintext map, which carries no identity) and mint a
//! FRESH `id` / `created_at` per row on every materialization — the id came from
//! a bare nanosecond counter with no monotonic component and no salt (both
//! minting helpers are now DELETED; the plan's T1 step-6 retirement criterion is
//! `grep -rn` over `crates/` returning nothing).
//! Consequences: the replica was not byte-faithful, `DELETE
//! /provider-keys/{id}` returned 404 on a promoted replica (the id the admin UI
//! held no longer existed), and multiple keys of one provider could collide on
//! the primary key within the same nanosecond.
//!
//! This test is written to FAIL against the pre-fix code: it seeds three keys on
//! one provider with hand-picked ids, materializes a snapshot onto a fresh
//! replica, and asserts the replica holds exactly those ids.

mod common;

use std::sync::Arc;

use hydra_core::config::ConfigData;
use hydra_core::model::{Provider, ProviderKey};
use hydra_server::cluster::snapshot::SnapshotWire;
use hydra_server::crypto::{KeyProvider, StaticKeyProvider};
use hydra_server::{db as repo, store::ConfigStore};

fn kp() -> Arc<dyn KeyProvider> {
    Arc::new(StaticKeyProvider::new([7u8; 32], 1))
}

fn provider(id: &str) -> Provider {
    Provider {
        id: id.into(),
        key: id.into(),
        name: id.into(),
        endpoint: "https://api.example.com".into(),
        weight: 1,
        created_at: "2026-01-01 00:00:00".into(),
        updated_at: "2026-01-01 00:00:00".into(),
        max_concurrency: None,
        max_queue_depth: None,
        queue_wait_timeout_ms: None,
    }
}

fn key(id: &str, provider_id: &str, secret: &str, created_at: &str) -> ProviderKey {
    ProviderKey {
        id: id.into(),
        provider_id: provider_id.into(),
        api_key: secret.into(),
        created_at: created_at.into(),
    }
}

/// Seed two providers; `p1` gets three keys (the collision-prone case), `p2` one.
async fn seed_leader(pool: &sqlx::SqlitePool, kp: &dyn KeyProvider) {
    repo::insert_provider(pool, &provider("p1"))
        .await
        .expect("insert p1");
    repo::insert_provider(pool, &provider("p2"))
        .await
        .expect("insert p2");
    for (id, secret, at) in [
        ("pk-aaa", "sk-p1-first", "2026-01-01 00:00:01"),
        ("pk-bbb", "sk-p1-second", "2026-01-01 00:00:02"),
        ("pk-ccc", "sk-p1-third", "2026-01-01 00:00:03"),
    ] {
        repo::insert_provider_key(pool, kp, &key(id, "p1", secret, at))
            .await
            .expect("insert p1 key");
    }
    repo::insert_provider_key(
        pool,
        kp,
        &key("pk-ddd", "p2", "sk-p2-only", "2026-01-01 00:00:04"),
    )
    .await
    .expect("insert p2 key");
}

#[tokio::test]
async fn replica_keeps_leader_provider_key_identity() {
    let leader_pool = common::setup_pool().await;
    let key_provider = kp();
    seed_leader(&leader_pool, key_provider.as_ref()).await;

    let leader_store = ConfigStore::load(leader_pool.clone(), key_provider.clone())
        .await
        .expect("load");
    let content = leader_store
        .replication()
        .as_deref()
        .cloned()
        .expect("replication content");

    let mut leader_ids: Vec<String> = content
        .fidelity()
        .provider_keys
        .iter()
        .map(|k| k.id.clone())
        .collect();
    leader_ids.sort();
    assert_eq!(
        leader_ids,
        vec!["pk-aaa", "pk-bbb", "pk-ccc", "pk-ddd"],
        "the replication content carries every provider_key WITH its identity"
    );

    let wire = SnapshotWire::build(&content, key_provider.as_ref())
        .await
        .expect("build");
    assert!(
        wire.cfg.provider_keys.is_empty(),
        "the wire clears the plaintext map; the sealed rows carry identity"
    );
    assert_eq!(
        wire.sealed_provider_keys["p1"]
            .iter()
            .map(|k| k.id.clone())
            .collect::<Vec<_>>(),
        vec!["pk-aaa", "pk-bbb", "pk-ccc"],
        "identity rides the wire"
    );

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

    // THE ASSERTION THAT FAILS PRE-FIX: the replica holds the leader's ids.
    let replica_keys = repo::list_provider_keys(&replica_pool, key_provider.as_ref())
        .await
        .expect("list provider keys");

    let mut replica_ids: Vec<String> = replica_keys.iter().map(|k| k.id.clone()).collect();
    replica_ids.sort();
    assert_eq!(
        replica_ids, leader_ids,
        "the replica must keep the leader's provider_key ids — regenerating them \
         breaks every admin delete after failover"
    );

    for leader_key in &content.fidelity().provider_keys {
        let replica_key = replica_keys
            .iter()
            .find(|k| k.id == leader_key.id)
            .unwrap_or_else(|| panic!("replica lost {}", leader_key.id));
        assert_eq!(
            replica_key.provider_id, leader_key.provider_id,
            "provider assignment is preserved"
        );
        assert_eq!(
            replica_key.created_at, leader_key.created_at,
            "created_at is preserved verbatim"
        );
        assert_eq!(
            replica_key.api_key, leader_key.api_key,
            "the re-sealed credential still opens to the leader's plaintext"
        );
    }

    // Three keys on ONE provider: distinct primary keys, none collapsed.
    let p1_ids: Vec<&str> = replica_keys
        .iter()
        .filter(|k| k.provider_id == "p1")
        .map(|k| k.id.as_str())
        .collect();
    assert_eq!(p1_ids.len(), 3, "all three p1 keys survived");
    let mut deduped = p1_ids.clone();
    deduped.sort_unstable();
    deduped.dedup();
    assert_eq!(deduped.len(), 3, "no primary-key collision between keys");
}

/// Re-materializing the SAME wire must not renumber the rows (pre-fix, every
/// materialization minted new ids, so this is the second half of the guard).
#[tokio::test]
async fn rematerializing_does_not_renumber_provider_keys() {
    let leader_pool = common::setup_pool().await;
    let key_provider = kp();
    seed_leader(&leader_pool, key_provider.as_ref()).await;

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
    let mut seen: Vec<Vec<String>> = Vec::new();
    for _ in 0..3 {
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

        let mut ids: Vec<String> = repo::list_provider_keys(&replica_pool, key_provider.as_ref())
            .await
            .expect("list")
            .into_iter()
            .map(|k| k.id)
            .collect();
        ids.sort();
        seen.push(ids);
    }

    assert_eq!(
        seen[0], seen[1],
        "the second materialization must not renumber provider keys"
    );
    assert_eq!(
        seen[1], seen[2],
        "nor the third — identity is a function of the wire, not of the clock"
    );
}

/// The replica's RUNTIME config must also see the keys, or the data plane would
/// route to a provider with no credentials after a failover.
#[tokio::test]
async fn hydrated_runtime_config_exposes_the_provider_keys() {
    let leader_pool = common::setup_pool().await;
    let key_provider = kp();
    seed_leader(&leader_pool, key_provider.as_ref()).await;

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
    let hydrated = wire.hydrate(key_provider.as_ref()).expect("hydrate");

    let cfg: &ConfigData = &hydrated.cfg;
    assert_eq!(
        cfg.provider_keys.get("p1").map(Vec::len),
        Some(3),
        "the hydrated runtime config carries all three p1 keys"
    );
    assert_eq!(
        cfg.provider_keys.get("p2").map(Vec::len),
        Some(1),
        "and the p2 key"
    );
    assert_eq!(
        hydrated.fidelity.provider_keys.len(),
        4,
        "and the fidelity rows carry all four with identity"
    );
}
