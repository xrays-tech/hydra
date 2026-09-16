//! The single owner of "what a replica must reproduce" (plan T1/T6).
//!
//! `ConfigData` is the RUNTIME snapshot: by design the loader keeps only ENABLED
//! `limit_role` / `provider_key_binding` rows, because those are what the hot
//! path matches against. That filter is correct for routing and fatally wrong as
//! a REBUILD source: `db::restore_config` wipes both tables and used to rebuild
//! them from `ConfigData`, so every materialization destroyed the replica's
//! disabled rows.
//!
//! This module separates the two roles. [`ReplicationContent`] is the rebuild
//! source (FULL rows), and it is also the object the generation predicate is
//! defined on — so "the version advanced" and "the replicated bytes changed"
//! are the same statement.

use std::sync::Arc;

use hydra_core::config::ConfigData;
use hydra_core::model::{
    LimitRole, ProviderKey, ProviderKeyBinding, ProviderModel, TenantModel, TenantProvider,
};
use sqlx::SqlitePool;

use crate::crypto::KeyProvider;

/// Tenant access-token HASH (`tenant_id` → hash) — never the token itself.
///
/// Carried so a replica can answer `has_access_token` and serve
/// `POST /tenants/{id}/auth/cache/invalidate` without a 401 (plan §7-7).
pub type TenantTokenHashes = Vec<(String, String)>;

/// The rows a replica needs BEYOND `ConfigData` to rebuild itself faithfully.
///
/// Every vector is loaded in a TOTAL order (see [`ReplicationContent::load`]) so
/// that `PartialEq` on the owning struct is a sound "did the replicated bytes
/// change?" predicate.
#[derive(Clone, Debug, PartialEq)]
pub struct FidelityRows {
    /// FULL `limit_role` rows — including `enabled == false`.
    pub limit_roles: Vec<LimitRole>,
    /// FULL `provider_key_binding` rows — including `enabled == false`.
    pub key_prefix_bindings: Vec<ProviderKeyBinding>,
    /// `provider_key` rows WITH their identity (`id`, `created_at`) so the
    /// replica keeps the leader's primary keys instead of minting new ones.
    pub provider_keys: Vec<ProviderKey>,
    /// `tenant_id` → access-token hash.
    pub tenant_token_hashes: TenantTokenHashes,
    /// Offline `provider_model` rows (the derived map drops `status != 1`).
    pub provider_models: Vec<ProviderModel>,
    /// Join rows with their ids preserved.
    pub tenant_providers: Vec<TenantProvider>,
    pub tenant_models: Vec<TenantModel>,
}

/// Everything a replica must reproduce, plus the version it was produced at.
///
/// `version` lives INSIDE this struct so a single atomic load yields a
/// consistent (version, content) pair — otherwise a producer could pair new
/// content with the old version. `cfg` is an `Arc` so the store's hot-path swap
/// and this struct share ONE allocation rather than deep-cloning `ConfigData`.
///
/// `PartialEq` (which includes `version`) is the generation predicate; callers
/// compare a candidate loaded at the CURRENT version, then label it (see
/// `ConfigStore::reload_all_with`).
#[derive(Clone, Debug, PartialEq)]
pub struct ReplicationContent {
    pub version: u64,
    pub cfg: Arc<ConfigData>,
    /// PRIVATE on purpose: the only production constructors are [`Self::load`]
    /// (leader/standby, from the DB) and [`Self::from_hydrated`] (replica, from
    /// a wire that already passed the version check).
    ///
    /// An EMPTY `FidelityRows` would instruct a replica to wipe its fidelity
    /// tables and insert nothing — cluster-wide silent data loss. Do NOT claim
    /// the type system rules that out: `FidelityRows` has `pub` fields and
    /// `from_hydrated` accepts any value, so an empty set IS constructible. The
    /// real guards are three, and all three are required: (1) only `load`
    /// populates `replication`, (2) `ConfigStore::from_snapshot` (edge, no pool)
    /// leaves it `None`, and (3) `internal_control` answers 503 `not_ready` for
    /// `None` — so a default/empty content can never be SERVED as a snapshot.
    fidelity: FidelityRows,
}

impl ReplicationContent {
    /// Read-only accessor (used by `SnapshotWire::build`).
    #[must_use]
    pub fn fidelity(&self) -> &FidelityRows {
        &self.fidelity
    }

    /// Build from an already-verified wire — the REPLICA path, and the
    /// documented escape hatch for tests. Needs no pool, so it is usable from a
    /// synchronous `#[test]`.
    #[must_use]
    pub fn from_hydrated(version: u64, cfg: Arc<ConfigData>, fidelity: FidelityRows) -> Self {
        Self {
            version,
            cfg,
            fidelity,
        }
    }

    /// Load the FULL replication content for `cfg` from the local DB.
    ///
    /// TWO properties make `PartialEq` on this struct a valid change predicate:
    ///
    /// 1. **Total order.** Each query must return a deterministic TOTAL order.
    ///    The pre-existing queries were not sufficient — the access-token-hash
    ///    query had no `ORDER BY` at all, and `list_limit_roles` /
    ///    `list_provider_keys` ordered by second-granularity `created_at`, which
    ///    ties on a batch insert. A phantom inequality would bump the generation
    ///    on every reload and rebuild every replica for nothing. The `, id`
    ///    tie-breakers were added in `db.rs` alongside this module.
    /// 2. **One read for keys.** `cfg.provider_keys` is PROJECTED from the same
    ///    `provider_keys` read used for the fidelity rows, so a provider deleted
    ///    between two reads can never leave the fidelity rows dangling.
    pub async fn load(
        pool: &SqlitePool,
        kp: &dyn KeyProvider,
        cfg: ConfigData,
        version: u64,
    ) -> Result<Self, sqlx::Error> {
        let provider_keys = crate::db::list_provider_keys(pool, kp).await?;
        let mut cfg = cfg;
        cfg.provider_keys = provider_keys.iter().fold(
            std::collections::HashMap::<String, Vec<String>>::new(),
            |mut acc, k| {
                acc.entry(k.provider_id.clone())
                    .or_default()
                    .push(k.api_key.clone());
                acc
            },
        );

        Ok(Self {
            version,
            cfg: Arc::new(cfg),
            fidelity: FidelityRows {
                limit_roles: crate::db::list_limit_roles(pool).await?,
                key_prefix_bindings: crate::db::list_provider_key_bindings(pool).await?,
                provider_keys,
                tenant_token_hashes: crate::db::list_tenant_access_token_hashes(pool).await?,
                provider_models: crate::db::list_provider_models(pool).await?,
                tenant_providers: crate::db::list_tenant_providers(pool).await?,
                tenant_models: crate::db::list_tenant_models(pool).await?,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::StaticKeyProvider;

    async fn pool() -> SqlitePool {
        let pool = crate::db::init_pool("sqlite::memory:")
            .await
            .expect("init_pool");
        crate::db::run_migrate(&pool).await.expect("migrate");
        pool
    }

    fn kp() -> StaticKeyProvider {
        StaticKeyProvider::new([1u8; 32], 1)
    }

    /// Seed one row in EVERY table the replication content reads, so the
    /// equality below is exercised on populated vectors rather than on a set of
    /// empty ones (which would pass even with no `ORDER BY` at all).
    async fn seed(pool: &SqlitePool, kp: &dyn KeyProvider) {
        let provider = hydra_core::model::Provider {
            id: "p1".into(),
            key: "k1".into(),
            name: "P".into(),
            endpoint: "http://127.0.0.1:1/".into(),
            weight: 1,
            created_at: "2026-01-01 00:00:00".into(),
            updated_at: "2026-01-01 00:00:00".into(),
            max_concurrency: None,
            max_queue_depth: None,
            queue_wait_timeout_ms: None,
        };
        crate::db::insert_provider(pool, &provider)
            .await
            .expect("insert provider");
        // Two keys with the SAME `created_at` and two limit roles with the same
        // `created_at`: the tie-breakers (`ORDER BY …, id`) are what make the
        // order — and therefore `PartialEq` — deterministic.
        for (id, secret) in [("k-b", "sk-b"), ("k-a", "sk-a")] {
            crate::db::insert_provider_key(
                pool,
                kp,
                &hydra_core::model::ProviderKey {
                    id: id.into(),
                    provider_id: "p1".into(),
                    api_key: secret.into(),
                    created_at: "2026-01-01 00:00:00".into(),
                },
            )
            .await
            .expect("insert provider key");
        }
        for id in ["r-b", "r-a"] {
            crate::db::insert_limit_role(
                pool,
                &hydra_core::model::LimitRole {
                    id: id.into(),
                    name: id.into(),
                    matching_key: None,
                    matching_model: None,
                    matching_tenant: None,
                    matching_provider: None,
                    limit_count: Some(1),
                    limit_token: Some(1),
                    window: "m".into(),
                    enabled: false, // disabled ⇒ fidelity-only, absent from cfg
                    created_at: "2026-01-01 00:00:00".into(),
                },
            )
            .await
            .expect("insert limit role");
        }
        for id in ["b-b", "b-a"] {
            crate::db::insert_provider_key_binding(
                pool,
                &hydra_core::model::ProviderKeyBinding {
                    id: id.into(),
                    key_prefix: format!("sk-{id}-"),
                    provider_id: "p1".into(),
                    enabled: false,
                    created_at: "2026-01-01 00:00:00".into(),
                    updated_at: "2026-01-01 00:00:00".into(),
                },
            )
            .await
            .expect("insert binding");
        }
        crate::db::insert_tenant(
            pool,
            &hydra_core::model::Tenant {
                id: "t1".into(),
                name: "T".into(),
                domain: "acme.com".into(),
                auth_url: "https://auth.acme.com/v".into(),
                cert_key: None,
                cert_file: None,
                enabled: true,
                created_at: "2026-01-01 00:00:00".into(),
                updated_at: "2026-01-01 00:00:00".into(),
            },
        )
        .await
        .expect("insert tenant");
        crate::db::set_tenant_access_token_hash(pool, "t1", Some("deadbeef"))
            .await
            .expect("set token hash");
        crate::db::insert_provider_model(
            pool,
            &hydra_core::model::ProviderModel {
                id: "m1".into(),
                key: "gpt-4".into(),
                name: "GPT-4".into(),
                provider_id: "p1".into(),
                status: 1,
            },
        )
        .await
        .expect("insert model");
        crate::db::insert_tenant_provider(
            pool,
            &hydra_core::model::TenantProvider {
                id: "tp1".into(),
                tenant_id: "t1".into(),
                provider_id: "p1".into(),
            },
        )
        .await
        .expect("insert tenant provider");
        crate::db::insert_tenant_model(
            pool,
            &hydra_core::model::TenantModel {
                id: "tm1".into(),
                tenant_id: "t1".into(),
                model_key: "gpt-4".into(),
            },
        )
        .await
        .expect("insert tenant model");
    }

    /// The gate for `reload_all`'s idempotence (plan O10): two `load` calls on an
    /// UNCHANGED database must be `==`. `PartialEq` includes `version` and every
    /// fidelity vector, so this is exactly the "did the replicated bytes change?"
    /// predicate the generation is decided on.
    ///
    /// It fails if any of the six queries loses its TOTAL order: `created_at` is
    /// second-granular, so same-second rows would come back in arbitrary order
    /// and a phantom inequality would advance the generation on every reload.
    #[tokio::test]
    async fn replication_content_is_idempotent() {
        let pool = pool().await;
        let kp = kp();
        seed(&pool, &kp).await;

        // Same version label for both: the version rides INSIDE the content, so
        // it must not be what makes the two unequal.
        let cfg = crate::store::build_config(&pool, &kp)
            .await
            .expect("build_config");
        let first = ReplicationContent::load(&pool, &kp, cfg.clone(), 7)
            .await
            .expect("first load");
        let second = ReplicationContent::load(&pool, &kp, cfg, 7)
            .await
            .expect("second load");

        assert_eq!(
            first, second,
            "two loads of an unchanged DB must be EQUAL, or `reload_all` advances \
             the generation on every call and rebuilds every replica for nothing"
        );

        // Non-vacuous: the vectors this compares are actually populated.
        assert_eq!(first.fidelity().provider_keys.len(), 2);
        assert_eq!(first.fidelity().limit_roles.len(), 2);
        assert_eq!(first.fidelity().key_prefix_bindings.len(), 2);
        assert_eq!(first.fidelity().tenant_token_hashes.len(), 1);
        assert_eq!(first.fidelity().provider_models.len(), 1);
        assert_eq!(first.fidelity().tenant_providers.len(), 1);
        assert_eq!(first.fidelity().tenant_models.len(), 1);
        // The DISABLED limit role / binding are in the fidelity rows but NOT in
        // `cfg` — the distinction G1 turns on.
        assert!(
            first.cfg.limit_roles.is_empty(),
            "cfg keeps enabled rows only"
        );
        assert!(
            first.cfg.key_prefix_bindings.is_empty(),
            "cfg keeps enabled rows only"
        );

        // And equality is really equality of the ORDER: reversing the row ids in
        // the DB would change the content (so the assertion above is not
        // comparing two empty/trivially-equal sets).
        crate::db::delete_provider_key(&pool, "k-a")
            .await
            .expect("delete key");
        let after = ReplicationContent::load(
            &pool,
            &kp,
            crate::store::build_config(&pool, &kp)
                .await
                .expect("build_config"),
            7,
        )
        .await
        .expect("third load");
        assert_ne!(after, first, "a real change must break the equality");
    }
}
