//! Arachne control plane, T3.1 — a node materializing the tree must hold the SAME
//! config value the leader holds — the loader's `Vec` ORDERS and its map KEYS
//! included — not merely an equivalent one.
//!
//! THE DEFECTS THIS GUARDS. `ConfigData` is DERIVED from the SQLite rows by
//! `store::build_config`, and that derivation is lossy in two ways the tree has to reproduce
//! rather than re-invent:
//!
//! 1. **`Vec` order.** Four vectors take their order from the loader's `ORDER BY`. The tree stores
//!    one entity per row and rebuilds them by re-collecting, and the first version sorted all four
//!    BY `id` — a rule the loader never used, so a replica held a differently-ordered config than
//!    the leader.
//! 2. **Map keys.** `tenants_by_domain` is keyed by the tenant's LOWERCASED domain, while the
//!    `Tenant` value keeps the domain verbatim. The decode re-derived the key from the value, so a
//!    tenant whose stored domain is not lowercase landed under a DIFFERENT key on a replica.
//!
//! Today the orders happen not to be behaviourally load-bearing — every matching limit role is
//! enforced (they are independent keys, not first-match-wins), `match_key_binding` /
//! `match_sub_tenant` take the LONGEST prefix rather than the first, and route selection is by
//! unique `(sub_tenant_id, model_key)` — but the domain KEY is: `proxy::resolve_tenant` looks up
//! by the lowercase `Host`, so a mixed-case row resolves on the leader and not on a replica.
//!
//! All of it matters for a second reason too: "this tree names exactly this config" is only
//! checkable by EQUALITY. A decode that returns a differently-derived lookalike makes every such
//! comparison worthless.
//!
//! The fixtures below are chosen so that the loader's order and `id` order DISAGREE, and so that
//! the stored domain is not lowercase: with the first version's rules every assertion here fails
//! while the code looks perfectly reasonable in review.

#![cfg(feature = "arachne")]

mod common;

use std::sync::Arc;

use hydra_core::config::ConfigData;
use hydra_core::model::{
    LimitRole, Provider, ProviderKeyBinding, SubTenant, SubTenantRoute, Tenant,
};
use hydra_server::cluster::arachne_entities::{
    build_config, split_config, tree_of, SealedMaterial,
};
use hydra_server::crypto::{KeyProvider, StaticKeyProvider};
use hydra_server::{db as repo, store::ConfigStore};

/// The tenant's domain as STORED: not lowercase. The loader lowercases it for the map key only.
const MIXED_CASE_DOMAIN: &str = "Acme.Example";
const LOWERCASE_DOMAIN: &str = "acme.example";

fn kp() -> Arc<dyn KeyProvider> {
    Arc::new(StaticKeyProvider::new([7u8; 32], 1))
}

fn tenant(id: &str, domain: &str) -> Tenant {
    Tenant {
        id: id.into(),
        name: id.into(),
        domain: domain.into(),
        auth_url: format!("https://auth.{LOWERCASE_DOMAIN}/v1"),
        cert_key: None,
        cert_file: None,
        enabled: true,
        created_at: "2026-01-01 00:00:00".into(),
        updated_at: "2026-01-01 00:00:00".into(),
    }
}

/// One tenant + one provider: the FK targets every row below needs.
async fn seed_fk_targets(pool: &sqlx::SqlitePool, domain: &str) {
    repo::insert_tenant(pool, &tenant("t1", domain))
        .await
        .expect("insert tenant");
    repo::insert_provider(
        pool,
        &Provider {
            id: "p1".into(),
            key: "k1".into(),
            name: "P1".into(),
            endpoint: "https://api.example.com".into(),
            weight: 1,
            created_at: "2026-01-01 00:00:00".into(),
            updated_at: "2026-01-01 00:00:00".into(),
            max_concurrency: None,
            max_queue_depth: None,
            queue_wait_timeout_ms: None,
        },
    )
    .await
    .expect("insert provider");
}

fn role(id: &str, created_at: &str) -> LimitRole {
    LimitRole {
        id: id.into(),
        name: id.into(),
        matching_key: None,
        matching_model: None,
        matching_tenant: None,
        matching_provider: None,
        limit_count: Some(10),
        limit_token: None,
        // The schema CHECKs the window vocabulary; "1m" is not in it.
        window: "m".into(),
        enabled: true,
        created_at: created_at.into(),
    }
}

fn binding(id: &str, key_prefix: &str) -> ProviderKeyBinding {
    ProviderKeyBinding {
        id: id.into(),
        key_prefix: key_prefix.into(),
        provider_id: "p1".into(),
        enabled: true,
        created_at: "2026-01-01 00:00:00".into(),
        updated_at: "2026-01-01 00:00:00".into(),
    }
}

fn sub_tenant(id: &str, name: &str, key_prefix: &str) -> SubTenant {
    SubTenant {
        id: id.into(),
        tenant_id: "t1".into(),
        name: name.into(),
        key_prefix: key_prefix.into(),
        enabled: true,
        created_at: "2026-01-01 00:00:00".into(),
        updated_at: "2026-01-01 00:00:00".into(),
    }
}

fn route(id: &str, sub_tenant_id: &str, model_key: &str) -> SubTenantRoute {
    SubTenantRoute {
        id: id.into(),
        sub_tenant_id: sub_tenant_id.into(),
        model_key: Some(model_key.into()),
        provider_id: "p1".into(),
        enabled: true,
        created_at: "2026-01-01 00:00:00".into(),
        updated_at: "2026-01-01 00:00:00".into(),
    }
}

/// Every row is seeded so that the loader's ORDER BY key and the row `id` DISAGREE in direction:
/// the row the loader puts FIRST has the id that sorts LAST.
async fn seed_disagreeing_orders(pool: &sqlx::SqlitePool) {
    seed_fk_targets(pool, LOWERCASE_DOMAIN).await;
    // `ORDER BY created_at, id`: the EARLIER row has the LATER id.
    for (id, created_at) in [("r-z", "2026-01-01"), ("r-a", "2026-06-01")] {
        repo::insert_limit_role(pool, &role(id, created_at))
            .await
            .expect("insert limit role");
    }
    // `ORDER BY key_prefix`: "sk-aaa-" sorts before "sk-zzz-" but its id sorts later.
    for (id, prefix) in [("b-z", "sk-aaa-"), ("b-a", "sk-zzz-")] {
        repo::insert_provider_key_binding(pool, &binding(id, prefix))
            .await
            .expect("insert binding");
    }
    // `ORDER BY tenant_id, name`: "alpha" before "zeta", ids the other way round.
    for (id, name, prefix) in [("s-z", "alpha", "AA_"), ("s-a", "zeta", "ZZ_")] {
        repo::insert_sub_tenant(pool, &sub_tenant(id, name, prefix))
            .await
            .expect("insert sub tenant");
    }
    // `ORDER BY sub_tenant_id, model_key`: same trick on the route rows.
    for (id, model) in [("sr-z", "aaa"), ("sr-a", "zzz")] {
        repo::insert_sub_tenant_route(pool, &route(id, "s-z", model))
            .await
            .expect("insert route");
    }
}

fn ids<E: std::fmt::Debug>(v: &[E], f: impl Fn(&E) -> String) -> Vec<String> {
    v.iter().map(f).collect()
}

/// Publish the leader's config through the tree and hand back what a replica would decode.
async fn round_trip(leader: &ConfigData, key_provider: &dyn KeyProvider) -> ConfigData {
    let rows = hydra_server::cluster::content::FidelityRows::default();
    let sealed = SealedMaterial::seal_plaintext(leader, &rows, key_provider).expect("seal");
    let tree = tree_of(&split_config(leader, &rows, sealed).expect("split")).expect("tree");
    build_config(&tree, key_provider).expect("rebuild")
}

#[tokio::test]
async fn a_replica_reproduces_the_loader_order_not_an_invented_one() {
    let key_provider = kp();
    let pool = common::setup_pool().await;
    seed_disagreeing_orders(&pool).await;

    let leader = ConfigStore::load(pool, key_provider.clone())
        .await
        .expect("load");
    let cfg = leader.snapshot();

    // Non-vacuous: the loader really does order these differently from their ids, so a decode
    // that sorted by id would be caught (and so the fixture is not passing by accident).
    assert_eq!(
        ids(&cfg.limit_roles, |r| r.id.clone()),
        vec!["r-z", "r-a"],
        "fixture: the loader orders limit roles by created_at, so the later id comes first"
    );
    assert_eq!(
        ids(&cfg.key_prefix_bindings, |b| b.id.clone()),
        vec!["b-z", "b-a"],
        "fixture: the loader orders bindings by key_prefix"
    );
    assert_eq!(
        ids(&cfg.sub_tenants, |s| s.id.clone()),
        vec!["s-z", "s-a"],
        "fixture: the loader orders sub-tenants by (tenant_id, name)"
    );
    assert_eq!(
        ids(&cfg.sub_tenant_routes, |r| r.id.clone()),
        vec!["sr-z", "sr-a"],
        "fixture: the loader orders routes by (sub_tenant_id, model_key)"
    );

    let replica = round_trip(&cfg, key_provider.as_ref()).await;

    // THE ASSERTIONS THAT FAIL PRE-FIX: the replica holds the leader's order.
    assert_eq!(
        ids(&replica.limit_roles, |r| r.id.clone()),
        ids(&cfg.limit_roles, |r| r.id.clone()),
        "the replica must hold limit roles in the LOADER's order (created_at, id); sorting them \
         by id is a rule the loader does not use, so the two nodes would read the same config \
         differently the day a first-match rule is introduced"
    );
    assert_eq!(
        ids(&replica.key_prefix_bindings, |b| b.id.clone()),
        ids(&cfg.key_prefix_bindings, |b| b.id.clone()),
        "the replica must hold key-prefix bindings in the loader's order"
    );
    assert_eq!(
        ids(&replica.sub_tenants, |s| s.id.clone()),
        ids(&cfg.sub_tenants, |s| s.id.clone()),
        "the replica must hold sub-tenants in the loader's order"
    );
    assert_eq!(
        ids(&replica.sub_tenant_routes, |r| r.id.clone()),
        ids(&cfg.sub_tenant_routes, |r| r.id.clone()),
        "the replica must hold sub-tenant routes in the loader's order"
    );

    // ...and therefore the whole value is equal, which is the property the tree claims.
    assert_eq!(
        replica,
        (**cfg).clone(),
        "a materialized config must EQUAL the leader's, or 'this tree names this config' cannot \
         be checked by comparing them"
    );
}

/// The tenant map is keyed by the LOWERCASED domain on both sides.
///
/// `store::build_config` lowercases the domain for the map KEY and keeps the row's own spelling in
/// the `Tenant` value; `proxy::resolve_tenant` looks the map up with the lowercased `Host`. So the
/// key is what the data plane matches on, and a replica that re-derived it from the value put a
/// mixed-case tenant under a key no request would ever look up: the tenant resolves on the leader
/// and 404s (or is refused) on every replica.
///
/// Falsification: decode with `insert(tenant.domain.clone(), tenant)` and this fails.
#[tokio::test]
async fn a_mixed_case_domain_lands_under_the_same_key_on_both_sides() {
    let key_provider = kp();
    let pool = common::setup_pool().await;
    seed_fk_targets(&pool, MIXED_CASE_DOMAIN).await;

    let leader = ConfigStore::load(pool, key_provider.clone())
        .await
        .expect("load");
    let cfg = leader.snapshot();

    // The loader's rule, stated as an assertion so the fixture cannot silently stop exercising it.
    assert!(
        cfg.tenants_by_domain.contains_key(LOWERCASE_DOMAIN),
        "fixture: the loader keys the tenant by its lowercased domain"
    );
    assert!(
        !cfg.tenants_by_domain.contains_key(MIXED_CASE_DOMAIN),
        "fixture: the verbatim domain is NOT a key on the leader"
    );
    assert_eq!(
        cfg.tenants_by_domain[LOWERCASE_DOMAIN].domain, MIXED_CASE_DOMAIN,
        "fixture: the VALUE keeps the stored spelling, so key and value genuinely differ"
    );

    let replica = round_trip(&cfg, key_provider.as_ref()).await;

    assert!(
        replica.tenants_by_domain.contains_key(LOWERCASE_DOMAIN),
        "the replica must key the tenant the way the leader does, or `resolve_tenant` (which \
         lowercases the Host) finds the tenant on one node and not on another"
    );
    assert_eq!(
        replica.tenants_by_domain.keys().collect::<Vec<_>>(),
        cfg.tenants_by_domain.keys().collect::<Vec<_>>(),
        "the two maps must have the same keys"
    );
    assert_eq!(
        replica,
        (**cfg).clone(),
        "and therefore the materialized config EQUALS the leader's"
    );
}
