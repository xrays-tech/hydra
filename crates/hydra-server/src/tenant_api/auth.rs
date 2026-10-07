//! Token gate for the tenant self-service API.
//!
//! ## Why it reads the config snapshot and not the database
//!
//! The previous tenant endpoint lived on the admin service and resolved a token
//! by running `SELECT id, access_token_hash FROM tenant WHERE access_token_hash
//! IS NOT NULL` on **every attempt** — an unbounded, unordered full-table read,
//! with no rate limit and no lockout, driven by an unauthenticated caller. Moving
//! the endpoint to the internet-facing data-plane listener without changing that
//! would have turned it into a cheap remote amplifier against the leader's
//! SQLite.
//!
//! The snapshot already carries what is needed: the cluster replication content
//! holds `FidelityRows::tenant_token_hashes` (`tenant_id -> SHA-256 hex`), which
//! the wire seals and `hydrate` opens. So the gate is **zero I/O**, works on
//! every role (leader, standby and edge all hold a replication content), and
//! agrees with the rest of the cluster by construction rather than by expiry.
//!
//! ## One guard, both halves
//!
//! The token hashes and the tenant rows are read from the **same**
//! `replication()` guard. `ReplicationContent` holds both (`fidelity()` and the
//! `cfg` it was built with), so a request can never be authenticated against one
//! generation of the snapshot and answered from another.

use hydra_core::auth::sha256_hex_string;

use constant_time_eq as ct_eq;
use hydra_core::config::ConfigData;
use hydra_core::model::Tenant;

use crate::store::ConfigStore;

/// Why a presented bearer could not be attributed to a tenant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthError {
    /// No token was presented, or no tenant owns it.
    ///
    /// The two are deliberately indistinguishable: telling a caller which half
    /// failed turns the gate into an oracle for tokens that exist.
    Unauthorized,
    /// The gate has nothing to judge the request against, and fails closed rather than passing it
    /// or calling the token wrong. Two states say it:
    ///
    /// - the node holds no configuration at all (version 0): a node that has just started or joined,
    ///   before the materializer applies the head. The tenant contract documents this as a retryable
    ///   503 — see `tenant-api-integration.md`;
    /// - the snapshot names a token digest with no matching tenant row: a config that cannot be
    ///   trusted, so nothing is authenticated against it.
    NotReady,
}

/// The tenant a request's token belongs to, plus the row and the snapshot version
/// the decision was made against.
///
/// Both come from the **same** guard as the token comparison, so a response can
/// never be attributed to a newer configuration than the one that authorised it —
/// which is what lets a tenant use `config_version` to tell whether a change it
/// made is live on the node that answered.
pub struct AuthenticatedTenant {
    pub tenant: Tenant,
    pub config_version: u64,
}

/// Resolve a presented bearer to a tenant, using only the config snapshot.
///
/// Comparison is constant-time over the stored digests and does **not** return
/// early on a match order that could be timed: every candidate is compared, and
/// the running result is folded rather than branched away.
pub fn authenticate(store: &ConfigStore, bearer: &str) -> Result<AuthenticatedTenant, AuthError> {
    let present = sha256_hex_string(bearer.as_bytes());
    // ONE atomic read: the hashes, the tenant rows and the version all come from the same
    // generation (see `AuthenticatedTenant`).
    let content = store.replication();

    // "This node holds no configuration yet" is version 0 — the watermark for "nothing has ever been
    // applied" (`ConfigStore::version`, F-4). This guard used to read `let Some(content) = … else
    // { NotReady }`, because the only empty store was the pool-less one built by the deleted
    // `from_snapshot`; the CONDITION did not go away with the constructor, only its old spelling
    // did. It is reachable for real: a node that has just started, or just joined, serves this
    // window (bounded by one materializer tick) until the head is applied, and the tenant contract
    // promises it 503 `not_ready` — retryable — rather than 401, which would say "your token is
    // wrong" about a node that has no configuration to judge it by.
    if content.version == 0 {
        return Err(AuthError::NotReady);
    }
    let hashes = &content.fidelity().tenant_token_hashes;

    // Fold over every row, so the work does not depend on where a match is.
    //
    // The count is what makes a DUPLICATE hash fail closed instead of silently
    // resolving to whichever row happens to iterate last: two tenants configured
    // with the same token hash is a misconfiguration, and the answer for it must
    // be indistinguishable from a wrong token (a 401), never a quiet "you are the
    // other tenant". The fold keeps the no-early-return property — every
    // candidate is still compared and the loop never branches out on a match.
    let mut matched: Option<&str> = None;
    let mut match_count: u32 = 0;
    for (tenant_id, stored) in hashes.iter() {
        if ct_eq(&present, stored) {
            matched = Some(tenant_id.as_str());
            match_count = match_count.wrapping_add(1);
        }
    }
    let Some(tenant_id) = matched else {
        return Err(AuthError::Unauthorized);
    };
    // More than one tenant claimed the same hash: fail closed, indistinguishable
    // from a wrong token (a 401, never a 403 or a silent "you are the other one").
    if match_count > 1 {
        return Err(AuthError::Unauthorized);
    }

    // Same guard as the hashes: the row AND the version cannot come from another
    // generation.
    let cfg: &ConfigData = &content.cfg;
    let config_version = content.version;
    match cfg.tenants_by_id.get(tenant_id) {
        Some(t) => Ok(AuthenticatedTenant {
            tenant: t.clone(),
            config_version,
        }),
        // A hash with no row is a snapshot in transition; it cannot be trusted,
        // and "not ready" is the honest answer (never a 403, which would look
        // like a valid-but-wrong tenant).
        None => Err(AuthError::NotReady),
    }
}

/// Constant-time string comparison — the SAME implementation the admin
/// token gate and the tenant-token gate on the admin plane use.
///
/// Re-exported rather than copied: two constant-time comparators are two places
/// for a timing bug to hide, and the repository already had one. Its reasoning
/// lives with it (`admin::handlers`): a plain `==` on `&str` stops at the first
/// differing byte, which is a (weak, remote) timing oracle over the token.
pub(crate) use crate::admin::handlers::constant_time_eq;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::content::FidelityRows;
    use crate::cluster::snapshot::HydratedWire;
    use hydra_core::config::ConfigData;
    use hydra_core::model::Tenant;
    use std::sync::Arc;

    fn kp() -> Arc<dyn crate::crypto::KeyProvider> {
        Arc::new(crate::crypto::StaticKeyProvider::new([3u8; 32], 1))
    }

    #[test]
    fn constant_time_eq_is_exact() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(!constant_time_eq("", "a"));
        assert!(constant_time_eq("", ""));
        // Real digests are always 64 hex chars, which is why the length
        // early-return leaks nothing useful here.
        let d = sha256_hex_string(b"sk-token");
        assert_eq!(d.len(), 64);
        assert!(constant_time_eq(&d, &d));
        assert!(!constant_time_eq(&d, &d[..63]));
    }

    /// A node that has materialized NO config yet is `not ready`, not `unauthorized` — asserted on
    /// the constructor production actually uses.
    ///
    /// This replaces `a_store_without_configuration_is_not_ready_not_unauthorized`, which built its
    /// store with `from_snapshot`: that constructor is gone (a store always has a database), so the
    /// test now takes its empty store from [`ConfigStore::load`] over an empty database — the state
    /// a real node is in before its first materialization. The EXPECTATION is unchanged, because the
    /// contract it protects is unchanged; what changed is that the premise is now reachable.
    #[tokio::test]
    async fn a_node_that_has_materialized_nothing_is_not_ready_not_unauthorized() {
        let store = ConfigStore::load(crate::db::test_pool().await, kp())
            .await
            .expect("load");
        assert_eq!(
            store.version(),
            0,
            "an empty database carries no watermark: this is what 'nothing materialized' looks like"
        );
        assert_eq!(
            authenticate(&store, "anything").err(),
            Some(AuthError::NotReady),
            "a gate with no configuration to judge by says so (503, retryable) instead of calling              the token wrong (401)"
        );
    }

    // NOTE: the BROADER positive and negative resolution paths (a token for
    // another tenant, a tenant with no token, and the snapshot-vs-database
    // property) are covered end-to-end in `crates/hydra-server/tests/tenant_api.rs`.
    // The single-match and duplicate-hash cases below need a REAL replication
    // content; they build one through the production `apply_snapshot` path
    // (`ReplicationContent::from_hydrated` is the documented test escape hatch),
    // so no test-only constructor is added to `ConfigStore`.

    /// Build a store whose replication content holds the given tenant rows and
    /// token hashes, so `authenticate` has a real snapshot to read.
    ///
    /// Reuses the production `apply_snapshot` path to install the content, and
    /// `ConfigStore::from_data` only for the initial (empty) store — which now needs a pool, so this
    /// helper is async like every other store construction in the codebase.
    async fn store_with_tenant_hashes(
        tenants: &[(String, String)],
        hashes: Vec<(String, String)>,
    ) -> ConfigStore {
        let mut cfg = ConfigData::default();
        for (id, domain) in tenants {
            cfg.tenants_by_domain.insert(
                domain.clone(),
                Tenant {
                    id: id.clone(),
                    name: id.clone(),
                    domain: domain.clone(),
                    auth_url: "https://auth.example/v".into(),
                    cert_key: None,
                    cert_file: None,
                    enabled: true,
                    created_at: "2026-01-01 00:00:00".into(),
                    updated_at: "2026-01-01 00:00:00".into(),
                },
            );
        }
        cfg.reindex_tenants();
        let fidelity = FidelityRows {
            limit_roles: Vec::new(),
            key_prefix_bindings: Vec::new(),
            provider_keys: Vec::new(),
            tenant_token_hashes: hashes,
            provider_models: Vec::new(),
            tenant_providers: Vec::new(),
            tenant_models: Vec::new(),
            sub_tenants: Vec::new(),
            sub_tenant_routes: Vec::new(),
        };
        let store =
            ConfigStore::from_data(crate::db::test_pool().await, ConfigData::default(), kp())
                .await
                .expect("from_data");
        store.apply_snapshot(HydratedWire {
            version: 1,
            cfg,
            fidelity,
        });
        store
    }

    /// Two tenants configured with the same token hash must fail CLOSED (a 401
    /// `Unauthorized`), not silently resolve to whichever row iterates last.
    #[tokio::test]
    async fn a_duplicate_token_hash_is_unauthorized_not_whichever_row_iterates_last() {
        let hash = sha256_hex_string(b"shared-secret-token");
        let store = store_with_tenant_hashes(
            &[
                ("t1".into(), "acme.example".into()),
                ("t2".into(), "other.example".into()),
            ],
            vec![("t1".into(), hash.clone()), ("t2".into(), hash.clone())],
        )
        .await;
        // The token matches two rows: the gate must refuse it, indistinguishable
        // from a wrong token.
        assert_eq!(
            authenticate(&store, "shared-secret-token").err(),
            Some(AuthError::Unauthorized),
            "a duplicate hash must fail closed, never resolve to one tenant"
        );
        // And an unrelated token is still `Unauthorized` (the normal negative path).
        assert_eq!(
            authenticate(&store, "a-different-token").err(),
            Some(AuthError::Unauthorized)
        );
    }

    /// The ordinary single-match path still succeeds: exactly one tenant owns the
    /// hash, the gate resolves it and reports the version it was decided on.
    #[tokio::test]
    async fn a_single_match_still_resolves_to_its_tenant() {
        let store = store_with_tenant_hashes(
            &[("t1".into(), "acme.example".into())],
            vec![("t1".into(), sha256_hex_string(b"only-this-tenant"))],
        )
        .await;
        let authed = authenticate(&store, "only-this-tenant").expect("must resolve");
        assert_eq!(authed.tenant.id, "t1");
        assert_eq!(authed.config_version, 1);
        // The wrong token is still refused (the positive case must not have
        // masked the negative one).
        assert_eq!(
            authenticate(&store, "some-other-token").err(),
            Some(AuthError::Unauthorized)
        );
    }
}
