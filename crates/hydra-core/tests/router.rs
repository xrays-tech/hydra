//! T2.1–T2.11 — pure `router::resolve`.
//!
//! `resolve` computes the candidate set for one `(tenant, model_key)` against a
//! config snapshot and a breaker view. Pipeline (design §7.1):
//! TenantModel gate → online model-providers ∩ tenant-providers → filter
//! (dead / no-key / soft-disabled). It returns the **set**; SWRR ordering is a
//! *subsequent* step applied by the caller (T2.11 — only the set is verified
//! here). To keep that set deterministic regardless of `HashSet` iteration
//! order, `resolve` returns candidates sorted by `provider_id`.

use std::collections::HashSet;

use hydra_core::breaker::{Breaker, BreakerConfig};
use hydra_core::config::{ConfigData, ModelProvider};
use hydra_core::model::{Provider, RouteError, Tenant};
use hydra_core::router::resolve;

fn provider(id: &str, weight: i32) -> Provider {
    Provider {
        id: id.into(),
        key: format!("k_{id}"),
        name: format!("{id} name"),
        endpoint: format!("https://{id}.example.com"),
        weight,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        max_concurrency: None,
        max_queue_depth: None,
        queue_wait_timeout_ms: None,
    }
}

fn tenant() -> Tenant {
    Tenant {
        id: "t_acme".into(),
        name: "Acme".into(),
        domain: "acme.com".into(),
        auth_url: "https://auth.acme.com".into(),
        cert_key: None,
        cert_file: None,
        enabled: true,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    }
}

/// Base config: tenant `t_acme` is allowed models `{gpt-4o}`, providers
/// `{p_a,p_b,p_c}`; all three providers serve `gpt-4o` (online, weight 1) and
/// each owns one api-key. A clone of this is mutated per-test.
fn base_cfg() -> ConfigData {
    let mut cfg = ConfigData::default();
    cfg.tenant_models
        .insert("t_acme".into(), HashSet::from(["gpt-4o".into()]));
    cfg.models_by_key.insert(
        "gpt-4o".into(),
        vec![
            ModelProvider {
                provider_id: "p_a".into(),
                weight: 1,
            },
            ModelProvider {
                provider_id: "p_b".into(),
                weight: 1,
            },
            ModelProvider {
                provider_id: "p_c".into(),
                weight: 1,
            },
        ],
    );
    cfg.tenant_providers.insert(
        "t_acme".into(),
        HashSet::from(["p_a".into(), "p_b".into(), "p_c".into()]),
    );
    cfg.providers.insert("p_a".into(), provider("p_a", 1));
    cfg.providers.insert("p_b".into(), provider("p_b", 1));
    cfg.providers.insert("p_c".into(), provider("p_c", 1));
    cfg.provider_keys.insert("p_a".into(), vec!["sk-a".into()]);
    cfg.provider_keys.insert("p_b".into(), vec!["sk-b".into()]);
    cfg.provider_keys.insert("p_c".into(), vec!["sk-c".into()]);
    cfg
}

/// A breaker with no dead providers.
fn alive_breaker() -> Breaker {
    Breaker::default()
}

/// A breaker that considers exactly `pid` dead.
fn breaker_with_dead(pid: &str) -> Breaker {
    let mut b = Breaker::new(BreakerConfig { threshold: 1 });
    b.on_failure(pid);
    b
}

fn resolve_set(cands: &[hydra_core::model::Candidate]) -> HashSet<String> {
    cands.iter().map(|c| c.provider_id.clone()).collect()
}

/// T2.1 — model not in the tenant's `tenant_models` gate ⇒ `ModelNotAllowed`.
#[test]
fn resolve_tenant_model_gate_reject() {
    let cfg = base_cfg();
    let tenant = tenant();
    let b = alive_breaker();
    let err = resolve(&cfg, &b, &tenant, "claude", None).unwrap_err();
    assert_eq!(err, RouteError::ModelNotAllowed);
}

/// T2.1b — tenant has NO `tenant_models` mapping ⇒ default-open: every model
/// is allowed (revised §7.1 semantics: empty/unset = unrestricted).
#[test]
fn resolve_tenant_model_gate_default_open() {
    let mut cfg = base_cfg();
    // `gpt-5` is served by an online provider but is NOT in the tenant's
    // (former) whitelist {gpt-4o} — with a mapping present it would 403.
    cfg.models_by_key.insert(
        "gpt-5".into(),
        vec![ModelProvider {
            provider_id: "p_a".into(),
            weight: 1,
        }],
    );
    // Sanity: with the mapping present the gate rejects gpt-5.
    let tenant = tenant();
    let b = alive_breaker();
    assert_eq!(
        resolve(&cfg, &b, &tenant, "gpt-5", None).unwrap_err(),
        RouteError::ModelNotAllowed
    );
    // Drop the mapping → default-open: gpt-5 now resolves.
    cfg.tenant_models.remove("t_acme");
    let cands = resolve(&cfg, &b, &tenant, "gpt-5", None).expect("no mapping → all models allowed");
    assert!(!cands.is_empty());
    // And the previously-whitelisted model still resolves.
    let cands2 =
        resolve(&cfg, &b, &tenant, "gpt-4o", None).expect("no mapping → all models allowed");
    assert!(!cands2.is_empty());
}

/// T2.2 — model in the gate ⇒ proceeds (and succeeds here).
#[test]
fn resolve_tenant_model_gate_pass() {
    let cfg = base_cfg();
    let tenant = tenant();
    let b = alive_breaker();
    let cands = resolve(&cfg, &b, &tenant, "gpt-4o", None).expect("gate passes → resolves");
    assert!(!cands.is_empty());
}

/// T2.3 — gate passes but the model is served by no online provider ⇒
/// `ModelNotFound`.
#[test]
fn resolve_model_not_found() {
    let mut cfg = base_cfg();
    // Allow gpt-5 at the gate, but no provider serves it.
    cfg.tenant_models
        .get_mut("t_acme")
        .unwrap()
        .insert("gpt-5".into());
    let tenant = tenant();
    let b = alive_breaker();
    let err = resolve(&cfg, &b, &tenant, "gpt-5", None).unwrap_err();
    assert_eq!(err, RouteError::ModelNotFound);
}

/// T2.4 — tenant has no `tenant_providers` entry ⇒ `TenantForbidden`.
#[test]
fn resolve_tenant_no_providers() {
    let mut cfg = base_cfg();
    cfg.tenant_providers.remove("t_acme");
    let tenant = tenant();
    let b = alive_breaker();
    let err = resolve(&cfg, &b, &tenant, "gpt-4o", None).unwrap_err();
    assert_eq!(err, RouteError::TenantForbidden);
}

/// T2.5 — model-providers and tenant-providers are disjoint ⇒
/// `NoAvailableProvider`.
#[test]
fn resolve_intersection_empty() {
    let mut cfg = base_cfg();
    // Tenant is only authorised for an unrelated provider.
    cfg.tenant_providers
        .insert("t_acme".into(), HashSet::from(["p_d".into()]));
    let tenant = tenant();
    let b = alive_breaker();
    let err = resolve(&cfg, &b, &tenant, "gpt-4o", None).unwrap_err();
    assert_eq!(err, RouteError::NoAvailableProvider);
}

/// T2.6 — intersection of {model providers} and {tenant providers} is the
/// candidate subset.
#[test]
fn resolve_intersection_subset() {
    let mut cfg = base_cfg();
    cfg.tenant_providers.insert(
        "t_acme".into(),
        HashSet::from(["p_b".into(), "p_c".into(), "p_d".into()]),
    );
    let tenant = tenant();
    let b = alive_breaker();
    let cands = resolve(&cfg, &b, &tenant, "gpt-4o", None).expect("non-empty intersection");
    // model ∈ {a,b,c} ∩ tenant {b,c,d} = {b,c}
    assert_eq!(
        resolve_set(&cands),
        HashSet::from(["p_b".into(), "p_c".into()])
    );
}

/// T2.7 — a candidate without any api-key is filtered out; if all are keyless
/// the result is an error.
#[test]
fn resolve_filter_no_keys() {
    // (a) one keyless survivor is dropped, the other remains.
    let mut cfg = base_cfg();
    cfg.provider_keys.remove("p_a"); // p_a now keyless
    let tenant = tenant();
    let b = alive_breaker();
    let cands = resolve(&cfg, &b, &tenant, "gpt-4o", None).expect("p_b,p_c still have keys");
    assert_eq!(
        resolve_set(&cands),
        HashSet::from(["p_b".into(), "p_c".into()]),
        "keyless p_a filtered out"
    );

    // (b) every candidate keyless ⇒ error.
    let mut cfg2 = base_cfg();
    cfg2.provider_keys.clear();
    let err = resolve(&cfg2, &b, &tenant, "gpt-4o", None).unwrap_err();
    assert_eq!(err, RouteError::NoAvailableProvider);
}

/// T2.8 — `weight = 0` (soft-disabled) candidates are filtered out.
#[test]
fn resolve_filter_weight_zero() {
    let mut cfg = base_cfg();
    cfg.providers.insert("p_a".into(), provider("p_a", 0)); // soft-disabled
    let tenant = tenant();
    let b = alive_breaker();
    let cands = resolve(&cfg, &b, &tenant, "gpt-4o", None).expect("p_b,p_c remain");
    assert_eq!(
        resolve_set(&cands),
        HashSet::from(["p_b".into(), "p_c".into()]),
        "weight=0 p_a filtered out"
    );
}

/// T2.9 — a provider marked dead by the breaker is filtered out.
#[test]
fn resolve_filter_breaker_dead() {
    let cfg = base_cfg();
    let tenant = tenant();
    let b = breaker_with_dead("p_a");
    let cands = resolve(&cfg, &b, &tenant, "gpt-4o", None).expect("p_b,p_c remain");
    assert_eq!(
        resolve_set(&cands),
        HashSet::from(["p_b".into(), "p_c".into()]),
        "dead p_a filtered out"
    );
}

/// T2.10 — every candidate filtered (dead / soft-disabled / keyless) ⇒
/// `NoAvailableProvider`.
#[test]
fn resolve_all_filtered() {
    let cfg = base_cfg();
    let tenant = tenant();
    // Mark every provider dead.
    let mut b = Breaker::new(BreakerConfig { threshold: 1 });
    b.on_failure("p_a");
    b.on_failure("p_b");
    b.on_failure("p_c");
    let err = resolve(&cfg, &b, &tenant, "gpt-4o", None).unwrap_err();
    assert_eq!(err, RouteError::NoAvailableProvider);
}

/// T2.11 — a successful resolve returns candidates carrying their weights; the
/// returned set is verified (order is finalised by subsequent SWRR, not here).
#[test]
fn resolve_ok_returns_candidates_with_weight() {
    let mut cfg = base_cfg();
    // Distinct weights so we can assert per-provider.
    cfg.providers.insert("p_a".into(), provider("p_a", 5));
    cfg.providers.insert("p_b".into(), provider("p_b", 2));
    cfg.providers.insert("p_c".into(), provider("p_c", 1));
    // Keep models_by_key weights aligned with provider weights for the check.
    cfg.models_by_key.insert(
        "gpt-4o".into(),
        vec![
            ModelProvider {
                provider_id: "p_a".into(),
                weight: 5,
            },
            ModelProvider {
                provider_id: "p_b".into(),
                weight: 2,
            },
            ModelProvider {
                provider_id: "p_c".into(),
                weight: 1,
            },
        ],
    );
    let tenant = tenant();
    let b = alive_breaker();
    let cands = resolve(&cfg, &b, &tenant, "gpt-4o", None).expect("full resolve");

    assert_eq!(cands.len(), 3, "all three survive");
    // The returned slice is sorted by provider_id for determinism.
    assert_eq!(cands[0].provider_id, "p_a");
    assert_eq!(cands[1].provider_id, "p_b");
    assert_eq!(cands[2].provider_id, "p_c");
    let by_id: std::collections::HashMap<&str, i32> = cands
        .iter()
        .map(|c| (c.provider_id.as_str(), c.weight))
        .collect();
    assert_eq!(by_id.get("p_a").copied(), Some(5));
    assert_eq!(by_id.get("p_b").copied(), Some(2));
    assert_eq!(by_id.get("p_c").copied(), Some(1));
    // Endpoints carried through from the provider snapshot.
    assert_eq!(cands[0].endpoint, "https://p_a.example.com");
}

fn binding(
    id: &str,
    prefix: &str,
    provider_id: &str,
    enabled: bool,
) -> hydra_core::model::ProviderKeyBinding {
    hydra_core::model::ProviderKeyBinding {
        id: id.into(),
        key_prefix: prefix.into(),
        provider_id: provider_id.into(),
        enabled,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    }
}

/// T2.12 — an api-key matching an enabled prefix binding restricts the
/// candidate set to the bound provider.
#[test]
fn resolve_key_binding_restricts() {
    let mut cfg = base_cfg();
    cfg.key_prefix_bindings
        .push(binding("b1", "sk_aaa_", "p_a", true));
    let tenant = tenant();
    let b = alive_breaker();
    let cands = resolve(&cfg, &b, &tenant, "gpt-4o", Some("sk_aaa_123")).expect("bound provider");
    assert_eq!(resolve_set(&cands), HashSet::from(["p_a".into()]));
}

/// T2.13 — longest prefix wins when several enabled bindings match.
#[test]
fn resolve_key_binding_longest_prefix_wins() {
    let mut cfg = base_cfg();
    cfg.key_prefix_bindings
        .push(binding("b1", "sk_", "p_a", true));
    cfg.key_prefix_bindings
        .push(binding("b2", "sk_aaa_", "p_b", true));
    let tenant = tenant();
    let b = alive_breaker();
    let cands =
        resolve(&cfg, &b, &tenant, "gpt-4o", Some("sk_aaa_123")).expect("longest prefix p_b");
    assert_eq!(resolve_set(&cands), HashSet::from(["p_b".into()]));
}

/// T2.14 — disabled bindings never match (no restriction).
#[test]
fn resolve_key_binding_disabled_ignored() {
    let mut cfg = base_cfg();
    cfg.key_prefix_bindings
        .push(binding("b1", "sk_aaa_", "p_a", false));
    let tenant = tenant();
    let b = alive_breaker();
    let cands = resolve(&cfg, &b, &tenant, "gpt-4o", Some("sk_aaa_123")).expect("no restriction");
    assert_eq!(
        resolve_set(&cands),
        HashSet::from(["p_a".into(), "p_b".into(), "p_c".into()])
    );
}

/// T2.15 — fail-closed: the bound provider is not in the eligible set ⇒ error.
#[test]
fn resolve_key_binding_bound_provider_ineligible() {
    let mut cfg = base_cfg();
    // p_a serves gpt-4o but is NOT in the tenant's authorised provider set.
    cfg.tenant_providers
        .insert("t_acme".into(), HashSet::from(["p_b".into(), "p_c".into()]));
    cfg.key_prefix_bindings
        .push(binding("b1", "sk_aaa_", "p_a", true));
    let tenant = tenant();
    let b = alive_breaker();
    let err = resolve(&cfg, &b, &tenant, "gpt-4o", Some("sk_aaa_123")).unwrap_err();
    assert_eq!(err, RouteError::NoAvailableProvider);
}

/// T2.16 — no matching prefix (or None api-key) ⇒ no restriction.
#[test]
fn resolve_key_binding_no_match_no_restriction() {
    let mut cfg = base_cfg();
    cfg.key_prefix_bindings
        .push(binding("b1", "sk_aaa_", "p_a", true));
    let tenant = tenant();
    let b = alive_breaker();
    let cands = resolve(&cfg, &b, &tenant, "gpt-4o", Some("hk_bbb_1")).expect("no match");
    assert_eq!(
        resolve_set(&cands),
        HashSet::from(["p_a".into(), "p_b".into(), "p_c".into()])
    );
    let cands2 = resolve(&cfg, &b, &tenant, "gpt-4o", None).expect("None → no restriction");
    assert_eq!(
        resolve_set(&cands2),
        HashSet::from(["p_a".into(), "p_b".into(), "p_c".into()])
    );
}
