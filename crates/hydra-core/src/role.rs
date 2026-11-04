//! Global Role matching — the five-dimensional rule selector (plan §3.4,
//! acceptance C1–C5).
//!
//! Two pure functions over plain data:
//!
//! - [`match_global_roles`] takes the candidate rules for a request
//!   (`global_roles_by_tenant[tenant_id]` **plus** `global_roles_any_tenant`, per
//!   plan §3.4 "先取 … + …, 逐条比较五维") and keeps the rules whose FIVE
//!   dimensions all match. A rule is a hit only when `tenant`, `user_group`,
//!   `user`, `api_key` and `model` each match — an incomplete match contributes
//!   NOTHING (not even a partial provider set).
//! - [`union_role_providers`] turns the matched rules into the provider SET the
//!   router would consume (the T6/T7 routing lane composes that with the
//!   `tenant_providers` intersection — that integration is NOT done here).
//!
//! The literal `*` on a rule dimension is "any value on that dimension" (decision
//! D-2). The request side has no wildcard: `principal = None` (anonymous / legacy
//! request with no api-key identity resolved) therefore matches ONLY the `*` rules
//! on the group/user/key dimensions — never a concrete id.
//!
//! Determinism: the two index lists are `Vec<usize>` in load order, but a snapshot
//! with several matching rules must hand the SAME order to the leader and to every
//! replica (plan §3.4 "节点一致": same data ⇒ same provider chosen). The result is
//! therefore sorted by `role.id` before it leaves [`match_global_roles`], and
//! [`union_role_providers`] deduplicates + sorts through a `BTreeSet` rather than
//! a hash set.

use std::collections::BTreeSet;

use crate::config::ConfigData;
use crate::identity::ApiPrincipal;
use crate::model::GlobalRole;

/// Wildcard literal accepted on a rule dimension (decision D-2).
const ANY: &str = "*";

/// One-dimension comparison, shared by all five dimensions of a [`GlobalRole`].
///
/// `rule_value == "*"` ⇒ matches any request (anonymous included).
/// Otherwise the request must actually CARRY that dimension: `None` never matches
/// a concrete rule value.
#[must_use]
fn dim_matches(rule_value: &str, presented: Option<&str>) -> bool {
    rule_value == ANY || matches!(presented, Some(v) if v == rule_value)
}

/// All ENABLED [`GlobalRole`] rules matching `(tenant_id, principal, model_key)`.
///
/// Candidate set = `cfg.global_roles_by_tenant[tenant_id]` ∪
/// `cfg.global_roles_any_tenant` (rules whose `tenant_id` is the `*` wildcard),
/// read through the indices [`ConfigData::reindex_global_roles`] builds — the
/// matcher reads rule BODIES from the single `global_roles` vector, it never
/// copies them.
///
/// A candidate survives when all five dimensions match:
/// - `tenant`: `rule.tenant_id == "*"` **or** `== tenant_id`;
/// - `user_group` / `user` / `api_key`: the rule's dimension is `"*"` **or**
///   `principal` is `Some` and the dimension equals the principal's value
///   (`principal == None` ⇒ only the `"*"` rules survive);
/// - `model`: `rule.model_key == "*"` **or** `== model_key`.
///
/// `role.enabled` is re-checked here even though `reindex_global_roles` only
/// indexes ENABLED rows: an empty provider union must never silently route, so a
/// hand-built / stale index cannot smuggle a disabled rule into the candidate set.
/// A stale index whose `idx` no longer resolves is likewise SKIPPED (fail closed)
/// rather than panicking — core has no logger; the server-side loader is the place
/// that would report it.
///
/// The result is sorted by `role.id` (the PK of `global_role`) so the leader and
/// every replica — and every run of the leader — see one order.
#[must_use]
pub fn match_global_roles<'a>(
    cfg: &'a ConfigData,
    tenant_id: &str,
    principal: Option<&ApiPrincipal>,
    model_key: &str,
) -> Vec<&'a GlobalRole> {
    // An EMPTY provider set still MATCHES (it just contributes nothing to the union),
    // so emptiness is deliberately NOT a match filter here.
    let consider = |idx: usize| -> Option<&'a GlobalRole> {
        let Some(role) = cfg.global_roles.get(idx) else {
            return None; // stale index: fail closed, never panic
        };
        if !role.enabled {
            return None;
        }
        if !dim_matches(&role.tenant_id, Some(tenant_id)) {
            return None;
        }
        if !dim_matches(&role.model_key, Some(model_key)) {
            return None;
        }
        let identity_ok = match principal {
            // Anonymous request: a concrete group/user/key value can NEVER match.
            None => role.user_group_id == ANY && role.user_id == ANY && role.api_key_id == ANY,
            Some(p) => {
                dim_matches(&role.user_group_id, Some(&p.user_group_id))
                    && dim_matches(&role.user_id, Some(&p.user_id))
                    && dim_matches(&role.api_key_id, Some(&p.api_key_id))
            }
        };
        if !identity_ok {
            return None;
        }
        Some(role)
    };

    let mut matched: Vec<&'a GlobalRole> = Vec::new();
    // The two index lists are DISJOINT by construction (`reindex_global_roles` routes
    // every rule to exactly one of them), so no de-duplication is needed here.
    if let Some(idxs) = cfg.global_roles_by_tenant.get(tenant_id) {
        for idx in idxs {
            if let Some(role) = consider(*idx) {
                matched.push(role);
            }
        }
    }
    for idx in &cfg.global_roles_any_tenant {
        if let Some(role) = consider(*idx) {
            matched.push(role);
        }
    }
    // Determinism: sort by the PK, not by load order.
    matched.sort_by(|a, b| a.id.cmp(&b.id));
    matched
}

/// The UNION of the matched rules' provider sets: deduplicated and sorted
/// (a [`BTreeSet`], so the order is byte order of `provider_id` — identical on
/// leader and replicas no matter which rule supplied a provider first).
///
/// An EMPTY result is meaningful, not an error: either nothing matched, or every
/// matching rule has an empty `provider_ids`. The router lane treats both as
/// "no Global Role candidate" and falls back to the legacy path (plan §3.4).
#[must_use]
pub fn union_role_providers(roles: &[&GlobalRole]) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    for role in roles {
        for provider_id in &role.provider_ids {
            set.insert(provider_id.clone());
        }
    }
    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::{match_global_roles, union_role_providers};
    use crate::config::ConfigData;
    use crate::identity::ApiPrincipal;
    use crate::model::{BillingStatus, GlobalRole};

    /// A test rule: every dimension `*` and no providers, narrowed per-test by
    /// the mutators below and turned into a real [`GlobalRole`] by [`cfg_of`].
    struct Rule {
        id: String,
        tenant_id: String,
        user_group_id: String,
        user_id: String,
        api_key_id: String,
        model_key: String,
        provider_ids: Vec<String>,
        enabled: bool,
    }

    fn rule(id: &str, provider_ids: Vec<String>) -> Rule {
        Rule {
            id: id.to_string(),
            tenant_id: "*".to_string(),
            user_group_id: "*".to_string(),
            user_id: "*".to_string(),
            api_key_id: "*".to_string(),
            model_key: "*".to_string(),
            provider_ids,
            enabled: true,
        }
    }

    // Dimension narrowers: mutate the moved copy and hand it back, so a test
    // reads as "the rule pinned to <dim> = <value>".
    fn on_tenant(mut r: Rule, v: &str) -> Rule {
        r.tenant_id = v.to_string();
        r
    }
    fn on_group(mut r: Rule, v: &str) -> Rule {
        r.user_group_id = v.to_string();
        r
    }
    fn on_user(mut r: Rule, v: &str) -> Rule {
        r.user_id = v.to_string();
        r
    }
    fn on_key(mut r: Rule, v: &str) -> Rule {
        r.api_key_id = v.to_string();
        r
    }
    fn on_model(mut r: Rule, v: &str) -> Rule {
        r.model_key = v.to_string();
        r
    }
    fn off(mut r: Rule) -> Rule {
        r.enabled = false;
        r
    }

    fn to_role(r: Rule) -> GlobalRole {
        GlobalRole {
            name: format!("name-{id}", id = r.id),
            id: r.id,
            tenant_id: r.tenant_id,
            user_group_id: r.user_group_id,
            user_id: r.user_id,
            api_key_id: r.api_key_id,
            model_key: r.model_key,
            provider_ids: r.provider_ids,
            enabled: r.enabled,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    /// Snapshot carrying `rules`, indexes rebuilt through the ONE sanctioned
    /// entry point ([`ConfigData::reindex_global_roles`]).
    fn cfg_of(rules: Vec<Rule>) -> ConfigData {
        let mut cfg = ConfigData::default();
        for r in rules {
            cfg.global_roles.push(to_role(r));
        }
        cfg.reindex_global_roles();
        cfg
    }

    /// The request's identity: tenant `t_acme` / group `g_1` / user `u_1` / key `k_1`.
    fn principal() -> ApiPrincipal {
        ApiPrincipal {
            tenant_id: "t_acme".to_string(),
            user_group_id: "g_1".to_string(),
            user_id: "u_1".to_string(),
            api_key_id: "k_1".to_string(),
            billing_status: BillingStatus::Active,
        }
    }

    /// Matched rules rendered as their ids, for readable assertions.
    fn ids(roles: &[&GlobalRole]) -> Vec<String> {
        roles.iter().map(|r| r.id.clone()).collect()
    }

    // -------------------------------------------------------------------------
    // C1 — an all-`*` rule matches any request; C2–C5 — a rule may pin each of
    // the five dimensions to the request's exact value instead.
    // -------------------------------------------------------------------------

    #[test]
    fn wildcard_rule_matches_anonymous_and_identified_requests() {
        let cfg = cfg_of(vec![rule("r_any", vec!["p_a".to_string()])]);

        let anon = match_global_roles(&cfg, "t_acme", None, "gpt-4o");
        assert_eq!(ids(&anon), vec!["r_any".to_string()]);

        let p = principal();
        let identified = match_global_roles(&cfg, "t_acme", Some(&p), "gpt-4o");
        assert_eq!(ids(&identified), vec!["r_any".to_string()]);
        // The element is the rule body from the config vector, not a copy.
        assert_eq!(identified[0].provider_ids, vec!["p_a".to_string()]);
    }

    #[test]
    fn every_dimension_can_be_pinned_to_the_request() {
        // tenant / group / user / key / model each pinned to the request's own
        // value ⇒ all FIVE rules match (five separate rules, one pin each).
        let rules = vec![
            on_tenant(rule("r_tenant", vec!["p_1".to_string()]), "t_acme"),
            on_group(rule("r_group", vec!["p_2".to_string()]), "g_1"),
            on_user(rule("r_user", vec!["p_3".to_string()]), "u_1"),
            on_key(rule("r_key", vec!["p_4".to_string()]), "k_1"),
            on_model(rule("r_model", vec!["p_5".to_string()]), "gpt-4o"),
        ];
        let cfg = cfg_of(rules);
        let p = principal();

        let matched = match_global_roles(&cfg, "t_acme", Some(&p), "gpt-4o");
        assert_eq!(
            ids(&matched),
            vec![
                "r_group".to_string(),
                "r_key".to_string(),
                "r_model".to_string(),
                "r_tenant".to_string(),
                "r_user".to_string(),
            ],
        );
    }

    #[test]
    fn a_pinned_dimension_that_disagrees_excludes_the_rule() {
        let rules = vec![
            on_tenant(rule("r_tenant", vec!["p_1".to_string()]), "t_other"),
            on_group(rule("r_group", vec!["p_2".to_string()]), "g_other"),
            on_user(rule("r_user", vec!["p_3".to_string()]), "u_other"),
            on_key(rule("r_key", vec!["p_4".to_string()]), "k_other"),
            on_model(rule("r_model", vec!["p_5".to_string()]), "gpt-5"),
            rule("r_all_wild", vec!["p_9".to_string()]),
        ];
        let cfg = cfg_of(rules);
        let p = principal();

        let matched = match_global_roles(&cfg, "t_acme", Some(&p), "gpt-4o");
        // Only the untouched all-`*` rule survives — one mismatched dimension is
        // enough to drop a rule (never a partial contribution).
        assert_eq!(ids(&matched), vec!["r_all_wild".to_string()]);
    }

    #[test]
    fn anonymous_identity_dims_need_wildcards() {
        let rules = vec![
            // identity all-`*`, but tenant/model PINNED to the request ⇒ still
            // matches an anonymous request (those two dims ARE known per-request).
            on_tenant(
                on_model(rule("r_ident_ok", vec!["p_1".to_string()]), "gpt-4o"),
                "t_acme",
            ),
            on_group(rule("r_group", vec!["p_2".to_string()]), "g_1"),
            on_user(rule("r_user", vec!["p_3".to_string()]), "u_1"),
            on_key(rule("r_key", vec!["p_4".to_string()]), "k_1"),
        ];
        let cfg = cfg_of(rules);

        // principal = None: ONLY the `*` rules on group/user/key may match.
        let anon = match_global_roles(&cfg, "t_acme", None, "gpt-4o");
        assert_eq!(ids(&anon), vec!["r_ident_ok".to_string()]);

        // Same snapshot, identified request: the three pinned rules now match too.
        let p = principal();
        let identified = match_global_roles(&cfg, "t_acme", Some(&p), "gpt-4o");
        assert_eq!(
            ids(&identified),
            vec![
                "r_group".to_string(),
                "r_ident_ok".to_string(),
                "r_key".to_string(),
                "r_user".to_string()
            ],
        );
    }

    // -------------------------------------------------------------------------
    // Union: several matching rules fold into ONE provider set — deduplicated
    // and sorted (`BTreeSet`), so leader and replicas agree byte-for-byte.
    // -------------------------------------------------------------------------

    #[test]
    fn union_dedups_overlapping_rules_and_sorts() {
        let rules = vec![
            // `r_b` is listed FIRST on purpose: the union must not follow rule
            // order, it must follow provider_id order.
            on_group(
                rule("r_b", vec!["p_3".to_string(), "p_1".to_string()]),
                "g_1",
            ),
            on_user(
                rule("r_a", vec!["p_1".to_string(), "p_2".to_string()]),
                "u_1",
            ),
            // An empty provider set MATCHES and contributes NOTHING.
            on_key(rule("r_empty", Vec::<String>::new()), "k_1"),
        ];
        let cfg = cfg_of(rules);
        let p = principal();

        let matched = match_global_roles(&cfg, "t_acme", Some(&p), "gpt-4o");
        assert_eq!(
            ids(&matched),
            vec!["r_a".to_string(), "r_b".to_string(), "r_empty".to_string()]
        );
        assert_eq!(
            union_role_providers(&matched),
            vec!["p_1".to_string(), "p_2".to_string(), "p_3".to_string()],
        );
    }

    #[test]
    fn union_of_nothing_is_empty() {
        // No matched rules at all — the union of an empty rule list is empty.
        let none_matched: Vec<&GlobalRole> = Vec::new();
        let empty: Vec<String> = Vec::new();
        assert_eq!(union_role_providers(&none_matched), empty);

        // Matching rules whose provider sets are all EMPTY ⇒ empty union, which
        // the router lane reads as "no Global Role candidate" (legacy fallback).
        let cfg = cfg_of(vec![
            rule("r_empty_a", Vec::<String>::new()),
            rule("r_empty_b", Vec::<String>::new()),
        ]);
        let matched = match_global_roles(&cfg, "t_acme", None, "gpt-4o");
        assert_eq!(
            ids(&matched),
            vec!["r_empty_a".to_string(), "r_empty_b".to_string()]
        );
        assert_eq!(union_role_providers(&matched), empty);
    }

    #[test]
    fn matching_is_deterministic_regardless_of_load_order() {
        // Same three rules, pushed in two different load orders: both the rule
        // list and the provider union come out identical (sorted), so two nodes
        // with the same data cannot pick different providers.
        let order_a = vec![
            rule("z_role", vec!["p_z".to_string(), "p_shared".to_string()]),
            rule("a_role", vec!["p_a".to_string(), "p_shared".to_string()]),
            rule("m_role", vec!["p_m".to_string()]),
        ];
        let order_b = vec![
            rule("m_role", vec!["p_m".to_string()]),
            rule("a_role", vec!["p_shared".to_string(), "p_a".to_string()]),
            rule("z_role", vec!["p_shared".to_string(), "p_z".to_string()]),
        ];
        let cfg_a = cfg_of(order_a);
        let cfg_b = cfg_of(order_b);

        let matched_a = match_global_roles(&cfg_a, "t_acme", None, "gpt-4o");
        let matched_b = match_global_roles(&cfg_b, "t_acme", None, "gpt-4o");
        assert_eq!(
            ids(&matched_a),
            vec![
                "a_role".to_string(),
                "m_role".to_string(),
                "z_role".to_string()
            ]
        );
        assert_eq!(ids(&matched_a), ids(&matched_b));
        assert_eq!(
            union_role_providers(&matched_a),
            vec![
                "p_a".to_string(),
                "p_m".to_string(),
                "p_shared".to_string(),
                "p_z".to_string()
            ],
        );
        assert_eq!(
            union_role_providers(&matched_a),
            union_role_providers(&matched_b)
        );
    }

    #[test]
    fn wildcard_tenant_and_concrete_tenant_rules_both_match() {
        // `r_any_t` lands in `global_roles_any_tenant`, `r_tn` in
        // `global_roles_by_tenant["t_acme"]`, `r_other` in
        // `global_roles_by_tenant["t_other"]` — the first TWO are candidates for
        // this request, the third is never even read.
        let rules = vec![
            rule("r_any_t", vec!["p_any".to_string()]),
            on_tenant(rule("r_tn", vec!["p_tn".to_string()]), "t_acme"),
            on_tenant(rule("r_other", vec!["p_other".to_string()]), "t_other"),
        ];
        let cfg = cfg_of(rules);
        assert_eq!(cfg.global_roles_any_tenant.len(), 1);
        assert!(cfg.global_roles_by_tenant.contains_key("t_other"));

        let matched = match_global_roles(&cfg, "t_acme", None, "gpt-4o");
        assert_eq!(
            ids(&matched),
            vec!["r_any_t".to_string(), "r_tn".to_string()]
        );
        assert_eq!(
            union_role_providers(&matched),
            vec!["p_any".to_string(), "p_tn".to_string()]
        );
    }

    #[test]
    fn disabled_and_stale_index_entries_never_match() {
        let mut cfg = ConfigData::default();
        cfg.global_roles
            .push(to_role(off(rule("r_off", vec!["p_x".to_string()]))));
        cfg.reindex_global_roles();
        // `reindex_global_roles` excludes DISABLED rows, so a saner snapshot than
        // this one cannot reach the matcher; the hand-built index below proves
        // `match_global_roles` refuses them anyway (fail closed, no panic).
        cfg.global_roles_by_tenant
            .insert("t_acme".to_string(), vec![0, 99]);

        assert_eq!(
            ids(&match_global_roles(&cfg, "t_acme", None, "gpt-4o")),
            Vec::<String>::new(),
        );
        let p = principal();
        assert_eq!(
            ids(&match_global_roles(&cfg, "t_acme", Some(&p), "gpt-4o")),
            Vec::<String>::new(),
        );
    }
}
