//! Auth identity types for the INTERNAL api-key auth path (plan §3.2).
//!
//! Pure data (no I/O, no async) — the zero-I/O firewall of `hydra-core` applies:
//! the SHA-256 lookup that produces these lives in the server (`store.rs` /
//! `proxy.rs`), the decision table that consumes them is the pure checker landing
//! in T3.
//!
//! ## Why these live apart from [`crate::auth`]
//!
//! `auth.rs` is the EXTERNAL-auth vocabulary (`auth_url` verdicts, the cache
//! decision core) that plan §3.7 deletes wholesale in T10. These types are its
//! REPLACEMENT, and welding them into the module that is scheduled for deletion
//! would force the deletion batch to cherry-pick them back out. One new module,
//! deleted-by-nothing.
//!
//! ## The identity-index contract (plan §3.2 承重规则 / decision D-5)
//!
//! [`ApiKeyIdentity`] is the value of `ConfigData::users_by_api_key_hash`, and
//! that index is built from FULL rows — disabled keys, disabled users/groups,
//! delinquent users ALL present. A revoked key on a replica therefore reads
//! "known but revoked → 401", never "unknown → 401": the anti-enumeration property
//! survives replication. The enabled/disabled/expired/billing distinction is NOT
//! baked in here (except `billing_status`, which the principal must carry to
//! answer 402 without a second hop); the T3 checker applies the truth table:
//!
//! | key present | key enabled / not expired | user/group | billing | result |
//! |---|---|---|---|---|
//! | no  | — | — | — | 401 |
//! | yes | no | — | — | 401 |
//! | yes | yes | disabled | — | 403 |
//! | yes | yes | enabled | delinquent | 402 |
//! | yes | yes | enabled | active | allow |

use serde::{Deserialize, Serialize};

use crate::model::BillingStatus;

/// Who a successfully-resolved client api-key IS: the four-level identity
/// (tenant → group → user → key) plus the billing state.
///
/// Built by the identity index (`ConfigData::reindex_identity`) from the JOIN of
/// the key row onto its user row, so `tenant_id` / `user_group_id` are the USER's
/// dimensions — a cross-tenant key is detected by comparing
/// `principal.tenant_id` against the request's tenant (plan §3.2 step 3: mismatch
/// ⇒ 401 `invalid_api_key`, never a pass).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiPrincipal {
    pub tenant_id: String,
    pub user_group_id: String,
    pub user_id: String,
    pub api_key_id: String,
    pub billing_status: BillingStatus,
}

/// One client api-key's identity record: the principal it resolves to, plus the
/// key's OWN state (revocation/expiry are key-level facts, distinct from the
/// user's enable/billing state and mapped to different statuses: revoked/expired
/// key ⇒ 401, disabled user ⇒ 403, delinquent user ⇒ 402).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKeyIdentity {
    pub principal: ApiPrincipal,
    /// `false` = revoked key: known (so 401, not "unknown") but never serves.
    pub key_enabled: bool,
    /// User-row enablement at identity-index build time.
    #[serde(default)]
    pub user_enabled: bool,
    /// User-group enablement at identity-index build time. A missing group is
    /// treated as disabled by [`ConfigData::reindex_identity`].
    #[serde(default)]
    pub group_enabled: bool,
    /// Expiry (RFC3339 UTC, normalized at write time so lexicographic compare is
    /// correct — plan §3.1). `None` = never expires. The checker treats
    /// `expires_at <= now` as 401 `invalid_api_key`.
    pub expires_at: Option<String>,
}

/// One tenant access token's identity record: what `whoami` may return (plan
/// §3.3: `token_name`, never the plaintext). The value of
/// `ConfigData::tenant_tokens` (`token_hash` → this), which the loader fills
/// with ENABLED rows ONLY — unlike the api-key identity index, a revoked tenant
/// token is simply GONE (tenant tokens are not an enumeration surface).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantTokenIdentity {
    pub tenant_id: String,
    pub token_name: String,
    /// Expiry (RFC3339 UTC); `None` = never expires. The tenant-token gate treats
    /// `expires_at <= now` as `Unauthorized`; revoked tokens remain absent because
    /// `ConfigData.tenant_tokens` loads ENABLED rows only.
    #[serde(default)]
    pub expires_at: Option<String>,
}

/// Internal api-key auth result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthDecision {
    Allow,
    InvalidKey,
    UserDisabled,
    GroupDisabled,
    TenantDisabled,
    Delinquent,
}

/// Pure internal api-key decision table.
pub fn decide_api_key(
    tenant_id: &str,
    tenant_enabled: bool,
    identity: Option<&ApiKeyIdentity>,
    now_rfc3339: &str,
) -> AuthDecision {
    let Some(identity) = identity else {
        return AuthDecision::InvalidKey;
    };
    if identity.principal.tenant_id.as_str() != tenant_id {
        return AuthDecision::InvalidKey;
    }
    if !identity.key_enabled {
        return AuthDecision::InvalidKey;
    }
    if let Some(expires_at) = identity.expires_at.as_ref() {
        if expires_at.as_str() <= now_rfc3339 {
            return AuthDecision::InvalidKey;
        }
    }
    if !identity.user_enabled {
        return AuthDecision::UserDisabled;
    }
    if !identity.group_enabled {
        return AuthDecision::GroupDisabled;
    }
    if !tenant_enabled {
        return AuthDecision::TenantDisabled;
    }
    if identity.principal.billing_status == BillingStatus::Delinquent {
        return AuthDecision::Delinquent;
    }
    AuthDecision::Allow
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip symmetry: the index value must survive the JSON hop the
    /// cluster wire takes (`SnapshotWire` carries `ConfigData`).
    #[test]
    fn identity_round_trips_through_json() {
        let id = ApiKeyIdentity {
            principal: ApiPrincipal {
                tenant_id: "t1".into(),
                user_group_id: "g1".into(),
                user_id: "u1".into(),
                api_key_id: "k1".into(),
                billing_status: BillingStatus::Delinquent,
            },
            key_enabled: false,
            user_enabled: true,
            group_enabled: false,
            expires_at: Some("2026-12-01T00:00:00Z".into()),
        };
        let json = serde_json::to_string(&id).expect("serialize");
        // `billing_status` must serialize snake_case (the SQLite CHECK domain).
        assert!(json.contains("\"delinquent\""), "json was {json}");
        let back: ApiKeyIdentity = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, id, "round-trip must be exact");
    }

    /// The `TenantTokenIdentity` value must survive the hop exactly. It carries
    /// `expires_at` so the auth-time gate can refuse an enabled-but-expired token
    /// without a DB read; revoked tokens remain absent because the index loads
    /// ENABLED rows only.
    #[test]
    fn token_identity_round_trips() {
        let id = TenantTokenIdentity {
            tenant_id: "t1".into(),
            token_name: "ci".into(),
            expires_at: Some("2026-12-01T00:00:00Z".into()),
        };
        let json = serde_json::to_string(&id).expect("serialize");
        let back: TenantTokenIdentity = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, id);
    }

    #[test]
    fn decide_api_key_truth_table() {
        let now = "2026-01-01T00:00:00Z";
        let active = ApiPrincipal {
            tenant_id: "t1".into(),
            user_group_id: "g1".into(),
            user_id: "u1".into(),
            api_key_id: "k1".into(),
            billing_status: BillingStatus::Active,
        };
        let delinquent = ApiPrincipal {
            tenant_id: "t1".into(),
            user_group_id: "g1".into(),
            user_id: "u1".into(),
            api_key_id: "k1".into(),
            billing_status: BillingStatus::Delinquent,
        };
        let key = |p: ApiPrincipal,
                   key_enabled: bool,
                   user_enabled: bool,
                   group_enabled: bool,
                   expires_at: Option<String>| {
            ApiKeyIdentity {
                principal: p,
                key_enabled,
                user_enabled,
                group_enabled,
                expires_at,
            }
        };

        assert_eq!(
            decide_api_key("t1", true, None, now),
            AuthDecision::InvalidKey
        );
        assert_eq!(
            decide_api_key(
                "t2",
                true,
                Some(&key(active.clone(), true, true, true, None)),
                now,
            ),
            AuthDecision::InvalidKey,
        );
        assert_eq!(
            decide_api_key(
                "t1",
                true,
                Some(&key(active.clone(), false, true, true, None)),
                now,
            ),
            AuthDecision::InvalidKey,
        );
        assert_eq!(
            decide_api_key(
                "t1",
                true,
                Some(&key(
                    active.clone(),
                    true,
                    true,
                    true,
                    Some("2026-01-01T00:00:00Z".into())
                )),
                now,
            ),
            AuthDecision::InvalidKey,
        );
        assert_eq!(
            decide_api_key(
                "t1",
                true,
                Some(&key(active.clone(), true, false, false, None)),
                now,
            ),
            AuthDecision::UserDisabled,
        );
        assert_eq!(
            decide_api_key(
                "t1",
                true,
                Some(&key(active.clone(), true, true, false, None)),
                now,
            ),
            AuthDecision::GroupDisabled,
        );
        assert_eq!(
            decide_api_key(
                "t1",
                false,
                Some(&key(active.clone(), true, true, true, None)),
                now,
            ),
            AuthDecision::TenantDisabled,
        );
        assert_eq!(
            decide_api_key(
                "t1",
                true,
                Some(&key(delinquent.clone(), true, true, true, None)),
                now,
            ),
            AuthDecision::Delinquent,
        );
        assert_eq!(
            decide_api_key(
                "t1",
                true,
                Some(&key(
                    active.clone(),
                    true,
                    true,
                    true,
                    Some("2026-01-02T00:00:00Z".into())
                )),
                now,
            ),
            AuthDecision::Allow,
        );
    }
}
