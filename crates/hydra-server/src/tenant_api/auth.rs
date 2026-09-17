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
    let mut matched: Option<&str> = None;
    for (tenant_id, stored) in hashes.iter() {
        if constant_time_eq(&present, stored) {
            matched = Some(tenant_id.as_str());
        }
    }
    let Some(tenant_id) = matched else {
        return Err(AuthError::Unauthorized);
    };

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

/// Constant-time string comparison.
///
/// Length is compared first (an unavoidable early return, but both operands here
/// are always 64-character hex digests, so it leaks nothing useful); the content
/// comparison then accumulates every byte difference instead of stopping at the
/// first one. A plain `==` on `&str` stops at the first differing byte, which is
/// a (weak, remote) timing oracle over the token — the repo already made this
/// choice for its other token gates.
#[must_use]
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use hydra_core::config::ConfigData;
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

    // NOTE: the positive and negative resolution paths (a known token, an
    // unknown one, a token for another tenant, a tenant with no token, and the
    // snapshot-vs-database property) are covered end-to-end in
    // `crates/hydra-server/tests/tenant_api.rs`. They need a REAL replication
    // content, which only a loaded store has; building one here would mean
    // adding a test-only constructor to `ConfigStore`, and a production type
    // should not grow a door that only tests walk through.
}
