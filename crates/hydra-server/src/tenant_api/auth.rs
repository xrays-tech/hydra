//! Token gate for the tenant self-service API.
//!
//! ## Why it reads the config snapshot and not the database
//!
//! The previous tenant endpoint lived on the admin service and resolved a token
//! by running a full-table tenant-token read on **every attempt** — an unbounded,
//! unordered read with no rate limit and no lockout, driven by an unauthenticated caller. Moving
//! the endpoint to the internet-facing data-plane listener without changing that
//! would have turned it into a cheap remote amplifier against the leader's
//! SQLite.
//!
//! The snapshot already carries what is needed: the cluster replication content
//! holds `FidelityRows::tenant_access_tokens`, and `ConfigData.tenant_tokens` is
//! derived from those rows during snapshot hydration. So the gate is **zero I/O**, works
//! on every role (leader, standby and edge all hold a replication content), and agrees
//! with the rest of the cluster by construction rather than by expiry.
//!
//! ## One guard, both halves
//!
//! The token identities and the tenant rows are read from the **same**
//! `replication()` guard. `ReplicationContent` holds the data and config behind both (`fidelity()` and the
//! `cfg` it was built with), so a request can never be authenticated against one
//! generation of the snapshot and answered from another.

use hydra_core::auth::sha256_hex_string;

#[allow(unused_imports)]
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
    pub token_name: String,
}

/// Resolve a presented bearer to a tenant, using only the config snapshot.
///
/// The lookup key is the SHA-256 digest of the presented token. `tenant_access_token.token_hash`
/// is globally UNIQUE in the schema, so a digest collision cannot represent two tenants; no duplicate
/// fold is needed here.
pub fn authenticate(store: &ConfigStore, bearer: &str) -> Result<AuthenticatedTenant, AuthError> {
    let present = sha256_hex_string(bearer.as_bytes());
    // ONE atomic read: the token identities, tenant rows and version all come from the same
    // generation (see `AuthenticatedTenant`).
    let content = store.replication();

    // "This node holds no configuration yet" is version 0 — the watermark for "nothing has ever been
    // applied" (`ConfigStore::version`, F-4).
    if content.version == 0 {
        return Err(AuthError::NotReady);
    }

    let cfg: &ConfigData = &content.cfg;
    let config_version = content.version;
    let identity = match cfg.tenant_tokens.get(&present) {
        Some(identity) => identity,
        None => return Err(AuthError::Unauthorized),
    };

    let now = now_rfc3339();
    if let Some(expires_at) = identity.expires_at.as_deref() {
        if expires_at.as_bytes() <= now.as_bytes() {
            return Err(AuthError::Unauthorized);
        }
    }

    match cfg.tenants_by_id.get(&identity.tenant_id) {
        Some(t) => Ok(AuthenticatedTenant {
            tenant: t.clone(),
            config_version,
            token_name: identity.token_name.clone(),
        }),
        // A token identity with no tenant row is a snapshot in transition; it cannot be trusted,
        // and "not ready" is the honest answer (never a 403, which would look like a valid-but-wrong
        // tenant).
        None => Err(AuthError::NotReady),
    }
}

/// Current time as an RFC 3339 UTC string (e.g. `2026-08-09T12:34:56Z`).
///
/// Lexicographic RFC3339 comparison is safe because these strings are normalized at write time.
fn now_rfc3339() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
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
    use hydra_core::identity::TenantTokenIdentity;
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
    // The cases below build a REAL replication content through the production `apply_snapshot`
    // path, so no test-only constructor is added to `ConfigStore`.

    async fn store_with_tenant_tokens(
        tenants: &[(String, String)],
        tokens: Vec<(String, String, String, Option<String>)>,
    ) -> ConfigStore {
        let mut cfg = ConfigData::default();
        for (id, domain) in tenants {
            cfg.tenants_by_domain.insert(
                domain.clone(),
                Tenant {
                    id: id.clone(),
                    name: id.clone(),
                    domain: domain.clone(),
                    cert_key: None,
                    cert_file: None,
                    enabled: true,
                    created_at: "2026-01-01 00:00:00".into(),
                    updated_at: "2026-01-01 00:00:00".into(),
                },
            );
        }
        cfg.reindex_tenants();
        for (token, tenant_id, token_name, expires_at) in tokens {
            cfg.tenant_tokens.insert(
                sha256_hex_string(token.as_bytes()),
                TenantTokenIdentity {
                    tenant_id: tenant_id.clone(),
                    token_name: token_name.clone(),
                    expires_at,
                },
            );
        }

        let store =
            ConfigStore::from_data(crate::db::test_pool().await, ConfigData::default(), kp())
                .await
                .expect("from_data");
        store.apply_snapshot(HydratedWire {
            version: 1,
            cfg,
            fidelity: FidelityRows::default(),
        });
        store
    }

    #[tokio::test]
    async fn a_single_match_still_resolves_to_its_tenant_token_name() {
        let store = store_with_tenant_tokens(
            &[("t1".into(), "acme.example".into())],
            vec![("only-this-tenant".into(), "t1".into(), "ci".into(), None)],
        )
        .await;
        let authed = authenticate(&store, "only-this-tenant").expect("must resolve");
        assert_eq!(authed.tenant.id, "t1");
        assert_eq!(authed.token_name, "ci");
        assert_eq!(authed.config_version, 1);
        // The wrong token is still refused.
        assert_eq!(
            authenticate(&store, "some-other-token").err(),
            Some(AuthError::Unauthorized)
        );
    }

    #[tokio::test]
    async fn an_expired_token_is_unauthorized() {
        let store = store_with_tenant_tokens(
            &[("t1".into(), "acme.example".into())],
            vec![(
                "expired-token".into(),
                "t1".into(),
                "expired".into(),
                Some("1999-01-01T00:00:00Z".into()),
            )],
        )
        .await;
        assert_eq!(
            authenticate(&store, "expired-token").err(),
            Some(AuthError::Unauthorized)
        );
    }

    #[tokio::test]
    async fn a_token_identity_without_tenant_row_is_not_ready() {
        let store = store_with_tenant_tokens(
            &[],
            vec![(
                "orphan-token".into(),
                "missing-tenant".into(),
                "orphan".into(),
                None,
            )],
        )
        .await;
        assert_eq!(
            authenticate(&store, "orphan-token").err(),
            Some(AuthError::NotReady)
        );
    }
}
