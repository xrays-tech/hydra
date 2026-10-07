//! Arachne control plane, D-7 — a replica materialized from the config TREE must
//! keep every tenant's certificate PRIVATE KEY.
//!
//! THE DEFECT THIS GUARDS. `CertMeta::cert_key_pem` carries
//! `#[serde(skip_serializing)]` on purpose: the private key must never reach any
//! serialised form, and `db::restore_config` re-seals it at the DB boundary from
//! the in-memory value. The Redis-era wire compensates for the skip with a
//! SEPARATE sealed field (`SnapshotWire::sealed_certs`, built in
//! `snapshot.rs`). The config tree had no such field: a `Cert` entity carried the
//! domain, the public PEM and the legacy path fields, and the private key was
//! simply absent.
//!
//! What that costs is not a subtle degradation. `restore_config` writes
//! `cert_pem` and then, seeing `cert_key_pem == None`, writes NULL into
//! `cert_key_ciphertext` / `_nonce` / `_version` — so a node materializing a tree
//! DESTROYS the tenant's TLS private key it had, replacing it with a public
//! certificate and no key. It is the same class of defect as the two this test
//! sits beside: `fidelity_disabled_rows.rs` (disabled rows wiped by a rebuild
//! that read the derived config) and `provider_key_fidelity.rs` (row identity
//! re-minted per materialization).
//!
//! This test is written to FAIL against the pre-fix code: it publishes one tenant
//! with cert content into a tree, materializes that tree onto a fresh replica,
//! and asserts the replica can hand back the SAME private key PEM.

#![cfg(feature = "arachne")]

mod common;

use std::sync::Arc;

use hydra_core::model::Tenant;
use hydra_server::cluster::arachne_entities::build_config_with_fidelity;
use hydra_server::cluster::arachne_materializer::encode_config;
use hydra_server::crypto::{KeyProvider, StaticKeyProvider};
use hydra_server::{db as repo, store::ConfigStore};

const CERT_PEM: &str = "-----BEGIN CERTIFICATE-----\nMIIBacme\n-----END CERTIFICATE-----\n";
/// The value whose absence is the whole point: a private key PEM.
const KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMIIEvQacme\n-----END PRIVATE KEY-----\n";

fn kp() -> Arc<dyn KeyProvider> {
    Arc::new(StaticKeyProvider::new([7u8; 32], 1))
}

fn tenant(id: &str, domain: &str) -> Tenant {
    Tenant {
        id: id.into(),
        name: id.into(),
        domain: domain.into(),
        auth_url: format!("https://auth.{domain}/v1"),
        cert_key: None,
        cert_file: None,
        enabled: true,
        created_at: "2026-01-01 00:00:00".into(),
        updated_at: "2026-01-01 00:00:00".into(),
    }
}

/// A leader with ONE tenant whose certificate is content-mode (migration 0007):
/// `cert_pem` plus a sealed private key in the tenant row.
async fn seed_leader(pool: &sqlx::SqlitePool, kp: &dyn KeyProvider) {
    let t = tenant("t1", "acme.example");
    repo::insert_tenant(pool, &t).await.expect("insert tenant");
    repo::update_tenant_cert(
        pool,
        kp,
        &repo::TenantCert {
            tenant_id: "t1".into(),
            cert_pem: Some(CERT_PEM.into()),
            cert_key_pem: Some(KEY_PEM.into()),
        },
    )
    .await
    .expect("store cert content");
    repo::set_tenant_access_token_hash(pool, "t1", Some(&"a".repeat(64)))
        .await
        .expect("token hash");
}

#[tokio::test]
async fn a_replica_materialized_from_the_tree_keeps_the_cert_private_key() {
    let key_provider = kp();
    let leader_pool = common::setup_pool().await;
    seed_leader(&leader_pool, key_provider.as_ref()).await;

    let leader = ConfigStore::load(leader_pool.clone(), key_provider.clone())
        .await
        .expect("load the leader store");
    let content = Arc::clone(&leader.replication());

    // Fixture check, so a failure below cannot be blamed on the seed: the leader
    // itself holds the private key in memory.
    let leader_cert = content
        .cfg
        .certs
        .get("acme.example")
        .expect("the leader's config carries a CertMeta for the tenant domain");
    assert_eq!(
        leader_cert.cert_key_pem.as_deref(),
        Some(KEY_PEM),
        "fixture: the leader's in-memory config holds the plaintext private key"
    );

    // Publish the tree the way a management write will: `encode_config` seals the secrets itself,
    // deterministically, so this is the tree ANY node produces for this config.
    let tree = encode_config(&content.cfg, content.fidelity(), key_provider.as_ref())
        .expect("encode the tree");

    // ...and materialize it on a node that has never seen this config.
    let (cfg, fidelity) =
        build_config_with_fidelity(&tree, key_provider.as_ref()).expect("decode the tree");
    let replica_pool = common::setup_pool().await;
    repo::restore_config(&replica_pool, key_provider.as_ref(), &cfg, &fidelity, 1)
        .await
        .expect("restore_config");

    // THE ASSERTION THAT FAILS PRE-FIX: the replica can hand the key back.
    let replica_cert = repo::get_tenant_cert(&replica_pool, key_provider.as_ref(), "t1")
        .await
        .expect("read the replica's tenant cert")
        .expect("the tenant row exists on the replica");
    assert_eq!(
        replica_cert.cert_pem.as_deref(),
        Some(CERT_PEM),
        "the public certificate must survive materialization"
    );
    assert_eq!(
        replica_cert.cert_key_pem.as_deref(),
        Some(KEY_PEM),
        "the replica must keep the tenant's TLS PRIVATE KEY: `restore_config` writes NULL into \
         cert_key_ciphertext when the decoded config has `cert_key_pem == None`, so a tree \
         without the sealed key does not merely omit the key — it DELETES the one the node had"
    );

    // And the decoded config is the leader's config, not a near-equivalent one:
    // anything that reads `cfg.certs` must see the key too.
    assert_eq!(
        cfg.certs
            .get("acme.example")
            .and_then(|c| c.cert_key_pem.clone()),
        Some(KEY_PEM.to_string()),
        "the tree must decode to a config that still carries the private key, so every consumer \
         of the decoded config sees what the leader saw"
    );
}

/// The private key must not travel in the CLEAR.
///
/// The skip on `cert_key_pem` exists to keep private keys out of serialised forms;
/// a fix that put the PEM into the entity as plain JSON would undo that protection
/// while making the test above pass.
#[tokio::test]
async fn the_cert_entity_carries_no_plaintext_private_key() {
    let key_provider = kp();
    let leader_pool = common::setup_pool().await;
    seed_leader(&leader_pool, key_provider.as_ref()).await;
    let leader = ConfigStore::load(leader_pool, key_provider.clone())
        .await
        .expect("load");
    let content = Arc::clone(&leader.replication());

    let tree =
        encode_config(&content.cfg, content.fidelity(), key_provider.as_ref()).expect("encode");

    for (path, bytes) in &tree {
        let text = String::from_utf8_lossy(bytes);
        assert!(
            !text.contains("PRIVATE KEY"),
            "entity {} carries the private key PEM in the clear; the key must be SEALED, as the \
             snapshot wire seals it",
            path.to_key_segment()
        );
    }

    // Non-vacuous: the tree really does contain the tenant's cert entity (the
    // assertion above would pass trivially on a tree that dropped certs entirely).
    assert!(
        tree.iter()
            .any(|(path, _)| path.to_key_segment().starts_with("cert/")),
        "the tree must carry a cert entity, or the 'no plaintext' assertion above is vacuous"
    );
}
