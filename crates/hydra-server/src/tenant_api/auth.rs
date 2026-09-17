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
    /// This node holds no configuration yet (an edge before its first snapshot).
    /// Fail closed — a gate that cannot check must not pass anything.
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
    let guard = store.replication();
    let Some(content) = guard.as_ref() else {
        return Err(AuthError::NotReady);
    };
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

    #[test]
    fn a_store_without_configuration_is_not_ready_not_unauthorized() {
        // The edge shape: `from_snapshot` holds no replication content until the
        // first snapshot lands, so the gate cannot check anything. A gate that
        // cannot check must fail closed AND say so — 401 would claim "your token
        // is wrong", which is a different statement.
        let store = ConfigStore::from_snapshot(ConfigData::default(), kp());
        assert_eq!(
            authenticate(&store, "anything").err(),
            Some(AuthError::NotReady)
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
    /// token hashes, so `authenticate` has a real (non-`None`) snapshot to read.
    ///
    /// Reuses the production `apply_snapshot` path — no test-only `ConfigStore`
    /// constructor.
    fn store_with_tenant_hashes(
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
        };
        let store = ConfigStore::from_snapshot(ConfigData::default(), kp());
        store.apply_snapshot(HydratedWire {
            version: 1,
            cfg,
            fidelity,
        });
        store
    }

    /// Two tenants configured with the same token hash must fail CLOSED (a 401
    /// `Unauthorized`), not silently resolve to whichever row iterates last.
    #[test]
    fn a_duplicate_token_hash_is_unauthorized_not_whichever_row_iterates_last() {
        let hash = sha256_hex_string(b"shared-secret-token");
        let store = store_with_tenant_hashes(
            &[
                ("t1".into(), "acme.example".into()),
                ("t2".into(), "other.example".into()),
            ],
            vec![("t1".into(), hash.clone()), ("t2".into(), hash.clone())],
        );
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
    #[test]
    fn a_single_match_still_resolves_to_its_tenant() {
        let store = store_with_tenant_hashes(
            &[("t1".into(), "acme.example".into())],
            vec![("t1".into(), sha256_hex_string(b"only-this-tenant"))],
        );
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
