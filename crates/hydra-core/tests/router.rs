//! T2.1–T2.16 — pure `router::resolve`; C1–C7 — `accessible_models`
//! (tenant model catalog, design §2.1 — the resolve gates as an all-set).
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
use hydra_core::router::CatalogEntry;
use hydra_core::router::{accessible_models, resolve, resolve_detailed};

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
    let err = resolve(&cfg, &b, &tenant, "claude").unwrap_err();
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
        resolve(&cfg, &b, &tenant, "gpt-5").unwrap_err(),
        RouteError::ModelNotAllowed
    );
    // Drop the mapping → default-open: gpt-5 now resolves.
    cfg.tenant_models.remove("t_acme");
    let cands = resolve(&cfg, &b, &tenant, "gpt-5").expect("no mapping → all models allowed");
    assert!(!cands.is_empty());
    // And the previously-whitelisted model still resolves.
    let cands2 = resolve(&cfg, &b, &tenant, "gpt-4o").expect("no mapping → all models allowed");
    assert!(!cands2.is_empty());
}

/// T2.2 — model in the gate ⇒ proceeds (and succeeds here).
#[test]
fn resolve_tenant_model_gate_pass() {
    let cfg = base_cfg();
    let tenant = tenant();
    let b = alive_breaker();
    let cands = resolve(&cfg, &b, &tenant, "gpt-4o").expect("gate passes → resolves");
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
    let err = resolve(&cfg, &b, &tenant, "gpt-5").unwrap_err();
    assert_eq!(err, RouteError::ModelNotFound);
}

/// T2.4 — tenant has no `tenant_providers` entry ⇒ `TenantForbidden`.
#[test]
fn resolve_tenant_no_providers() {
    let mut cfg = base_cfg();
    cfg.tenant_providers.remove("t_acme");
    let tenant = tenant();
    let b = alive_breaker();
    let err = resolve(&cfg, &b, &tenant, "gpt-4o").unwrap_err();
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
    let err = resolve(&cfg, &b, &tenant, "gpt-4o").unwrap_err();
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
    let cands = resolve(&cfg, &b, &tenant, "gpt-4o").expect("non-empty intersection");
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
    let cands = resolve(&cfg, &b, &tenant, "gpt-4o").expect("p_b,p_c still have keys");
    assert_eq!(
        resolve_set(&cands),
        HashSet::from(["p_b".into(), "p_c".into()]),
        "keyless p_a filtered out"
    );

    // (b) every candidate keyless ⇒ error.
    let mut cfg2 = base_cfg();
    cfg2.provider_keys.clear();
    let err = resolve(&cfg2, &b, &tenant, "gpt-4o").unwrap_err();
    assert_eq!(err, RouteError::NoAvailableProvider);
}

/// T2.8 — `weight = 0` (soft-disabled) candidates are filtered out.
#[test]
fn resolve_filter_weight_zero() {
    let mut cfg = base_cfg();
    cfg.providers.insert("p_a".into(), provider("p_a", 0)); // soft-disabled
    let tenant = tenant();
    let b = alive_breaker();
    let cands = resolve(&cfg, &b, &tenant, "gpt-4o").expect("p_b,p_c remain");
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
    let cands = resolve(&cfg, &b, &tenant, "gpt-4o").expect("p_b,p_c remain");
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
    let err = resolve(&cfg, &b, &tenant, "gpt-4o").unwrap_err();
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
    let cands = resolve(&cfg, &b, &tenant, "gpt-4o").expect("full resolve");

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

// ---------------------------------------------------------------------------
// C1–C7 — pure `accessible_models` (design §2.1: tenant model catalog).
// Same fixtures/builders as the resolve tests above; `tenant_id` = "t_acme".
// ---------------------------------------------------------------------------

/// C1 — tenant without a `tenant_providers` entry (or unknown tenant) ⇒ empty
/// catalog. No error: per-request resolve would 403 TenantForbidden, but the
/// catalog semantics is an empty list.
#[test]
fn catalog_tenant_without_providers_empty() {
    let mut cfg = base_cfg();
    cfg.tenant_providers.remove("t_acme");
    let b = alive_breaker();
    assert!(
        accessible_models(&cfg, &b, "t_acme").is_empty(),
        "no tenant_providers → empty catalog"
    );
    let cfg2 = base_cfg();
    assert!(
        accessible_models(&cfg2, &b, "t_unknown").is_empty(),
        "unknown tenant id → empty catalog"
    );
}

/// C2 — tenant_models whitelist filters models out; default-open (no mapping)
/// admits every served model.
#[test]
fn catalog_whitelist_filter_and_default_open() {
    let mut cfg = base_cfg();
    cfg.models_by_key.insert(
        "gpt-5".into(),
        vec![ModelProvider {
            provider_id: "p_a".into(),
            weight: 1,
        }],
    );
    let b = alive_breaker();
    // Whitelist {gpt-4o} present ⇒ gpt-5 is skipped.
    let entries = accessible_models(&cfg, &b, "t_acme");
    assert_eq!(entries.len(), 1, "whitelist keeps only gpt-4o");
    assert_eq!(entries[0].model, "gpt-4o");
    // Drop the mapping ⇒ default-open: both served models are listed.
    cfg.tenant_models.remove("t_acme");
    let entries = accessible_models(&cfg, &b, "t_acme");
    let models: Vec<&str> = entries.iter().map(|e| e.model.as_str()).collect();
    assert_eq!(
        models,
        vec!["gpt-4o", "gpt-5"],
        "default-open lists both, sorted"
    );
}

/// C3 — several providers serve one model ⇒ a single entry whose provider list
/// is deduplicated and sorted (duplicate serving rows collapse).
#[test]
fn catalog_multi_provider_single_entry() {
    let mut cfg = base_cfg();
    // Duplicate serving row for p_a — must collapse, not duplicate.
    cfg.models_by_key
        .get_mut("gpt-4o")
        .unwrap()
        .push(ModelProvider {
            provider_id: "p_a".into(),
            weight: 1,
        });
    let b = alive_breaker();
    let entries = accessible_models(&cfg, &b, "t_acme");
    assert_eq!(entries.len(), 1, "one model → one entry");
    assert_eq!(entries[0].model, "gpt-4o");
    assert_eq!(
        entries[0].providers,
        vec!["p_a".to_string(), "p_b".to_string(), "p_c".to_string()],
        "deduped + sorted provider ids"
    );
}

/// C5a — a breaker-dead provider is dropped from its model's entry.
#[test]
fn catalog_filter_dead_provider() {
    let cfg = base_cfg();
    let b = breaker_with_dead("p_a");
    let entries = accessible_models(&cfg, &b, "t_acme");
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].providers,
        vec!["p_b".to_string(), "p_c".to_string()],
        "dead p_a filtered out"
    );
}

/// C5b — a soft-disabled (weight = 0) provider is dropped.
#[test]
fn catalog_filter_weight_zero() {
    let mut cfg = base_cfg();
    cfg.providers.insert("p_a".into(), provider("p_a", 0));
    let b = alive_breaker();
    let entries = accessible_models(&cfg, &b, "t_acme");
    assert_eq!(
        entries[0].providers,
        vec!["p_b".to_string(), "p_c".to_string()],
        "weight=0 p_a filtered out"
    );
}

/// C5c — keyless providers are dropped; when none survive the entry (and the
/// whole catalog) is empty rather than an error.
#[test]
fn catalog_filter_no_key() {
    // (a) p_a keyless ⇒ dropped, p_b/p_c remain.
    let mut cfg = base_cfg();
    cfg.provider_keys.remove("p_a");
    let b = alive_breaker();
    let entries = accessible_models(&cfg, &b, "t_acme");
    assert_eq!(
        entries[0].providers,
        vec!["p_b".to_string(), "p_c".to_string()],
        "keyless p_a filtered out"
    );
    // (b) every provider keyless ⇒ entry disappears, empty catalog (no error).
    let mut cfg2 = base_cfg();
    cfg2.provider_keys.clear();
    let entries = accessible_models(&cfg2, &b, "t_acme");
    assert!(entries.is_empty(), "all keyless → empty catalog");
}

/// C5d — weight is read from the `cfg.providers` snapshot (`Provider.weight`),
/// not from the load-time mirror row in `models_by_key`.
#[test]
fn catalog_weight_from_provider_not_row() {
    let mut cfg = base_cfg();
    // The serving row still claims weight 5 for p_a, but the provider snapshot
    // is soft-disabled (weight 0) — the snapshot must win.
    cfg.models_by_key.get_mut("gpt-4o").unwrap()[0] = ModelProvider {
        provider_id: "p_a".into(),
        weight: 5,
    };
    cfg.providers.insert("p_a".into(), provider("p_a", 0));
    let b = alive_breaker();
    let entries = accessible_models(&cfg, &b, "t_acme");
    assert_eq!(
        entries[0].providers,
        vec!["p_b".to_string(), "p_c".to_string()],
        "provider snapshot weight 0 filters p_a despite row weight 5"
    );
}

/// C6 — a serving row referencing a provider absent from `cfg.providers` (an
/// orphan) is dropped without panicking — even when that provider is in the
/// tenant's authorised set (config validate only Warns on orphan references).
#[test]
fn catalog_orphan_provider_dropped_no_panic() {
    let mut cfg = base_cfg();
    cfg.tenant_providers
        .get_mut("t_acme")
        .unwrap()
        .insert("ghost".into());
    // ghost appears in gpt-4o's serving rows and is the sole provider of a
    // whitelisted model — only the existence guard can stop it.
    cfg.models_by_key
        .get_mut("gpt-4o")
        .unwrap()
        .push(ModelProvider {
            provider_id: "ghost".into(),
            weight: 7,
        });
    cfg.models_by_key.insert(
        "m-ghost".into(),
        vec![ModelProvider {
            provider_id: "ghost".into(),
            weight: 7,
        }],
    );
    cfg.tenant_models
        .get_mut("t_acme")
        .unwrap()
        .insert("m-ghost".into());
    let b = alive_breaker();
    let entries = accessible_models(&cfg, &b, "t_acme");
    assert_eq!(entries.len(), 1, "orphan-only m-ghost has no entry");
    assert_eq!(entries[0].model, "gpt-4o");
    assert_eq!(
        entries[0].providers,
        vec!["p_a".to_string(), "p_b".to_string(), "p_c".to_string()],
        "orphan ghost row dropped"
    );
}

/// C7 — deterministic output regardless of map iteration order: entries sorted
/// by model; each provider list sorted by provider id and deduplicated.
#[test]
fn catalog_deterministic_sort() {
    let mut cfg = ConfigData::default();
    cfg.tenant_providers.insert(
        "t_acme".into(),
        HashSet::from([
            "p_a".into(),
            "p_b".into(),
            "p_c".into(),
            "p_m".into(),
            "p_z".into(),
        ]),
    );
    for id in ["p_a", "p_b", "p_c", "p_m", "p_z"] {
        cfg.providers.insert(id.into(), provider(id, 1));
        cfg.provider_keys
            .insert(id.into(), vec![format!("sk-{id}")]);
    }
    // Deliberately scrambled insertion order; providers out of order too.
    cfg.models_by_key.insert(
        "charlie".into(),
        vec![
            ModelProvider {
                provider_id: "p_z".into(),
                weight: 1,
            },
            ModelProvider {
                provider_id: "p_a".into(),
                weight: 1,
            },
            ModelProvider {
                provider_id: "p_m".into(),
                weight: 1,
            },
        ],
    );
    cfg.models_by_key.insert(
        "bravo".into(),
        vec![ModelProvider {
            provider_id: "p_c".into(),
            weight: 1,
        }],
    );
    cfg.models_by_key.insert(
        "alpha".into(),
        vec![ModelProvider {
            provider_id: "p_b".into(),
            weight: 1,
        }],
    );
    // No tenant_models mapping ⇒ default-open.
    let b = alive_breaker();
    let entries = accessible_models(&cfg, &b, "t_acme");
    let expected = vec![
        CatalogEntry {
            model: "alpha".into(),
            providers: vec!["p_b".to_string()],
        },
        CatalogEntry {
            model: "bravo".into(),
            providers: vec!["p_c".to_string()],
        },
        CatalogEntry {
            model: "charlie".into(),
            providers: vec!["p_a".to_string(), "p_m".to_string(), "p_z".to_string()],
        },
    ];
    assert_eq!(
        entries, expected,
        "entries by model asc, providers by id asc"
    );
}

// ---------------------------------------------------------------------------
// T4 — sub-tenant route gate (3.6) + catalog mirror (design-sub-tenant.md §4.1,
// ruling a′: operator binding wins; otherwise intersect with the sub-tenant
// route's provider, fail-closed; no match ⇒ unchanged).
// Same fixtures/builders as above; tenant_id = "t_acme".
// ---------------------------------------------------------------------------

/// Round 46 (plan §2r) — the three step-4 drops are attributed PER PROVIDER and
/// per reason, not just as "the set came out empty".
///
/// T2.7–T2.9 above assert the same drops happen; this asserts the filter also
/// says WHO and WHY, which is what the proxy records as
/// `hydra_candidate_skipped_total{provider,reason}`. Without it, a provider whose
/// last api-key was deleted left rotation silently: while other providers kept
/// serving, nothing failed and `hydra_route_errors_total` (tenant-labelled,
/// failures only) stayed flat.
///
/// Note the contract: an empty candidate set is `Ok` here, so the attribution
/// survives exactly the case where it matters most — `resolve()` restores the
/// historical `Err(NoAvailableProvider)` for everyone else.
#[test]
fn resolve_detailed_attributes_every_exclusion() {
    let mut cfg = base_cfg();
    cfg.provider_keys.remove("p_a"); // → no_key
    cfg.providers.insert("p_b".into(), provider("p_b", 0)); // → soft_disabled
    let tenant = tenant();
    let b = breaker_with_dead("p_c"); // → breaker_dead

    let out = resolve_detailed(&cfg, &b, &tenant, "gpt-4o")
        .expect("gates 0-3 pass; attribution is data, not an error");
    assert!(
        out.candidates.is_empty(),
        "all three were dropped: {:?}",
        out.candidates
    );
    let got = excluded_reasons(&out);
    assert_eq!(
        got,
        vec![
            ("p_a".to_string(), "no_key"),
            ("p_b".to_string(), "soft_disabled"),
            ("p_c".to_string(), "breaker_dead"),
        ],
        "one entry per dropped provider, in provider_id order, with the reason"
    );

    // The thin wrapper keeps the historical contract for callers (and tests) that
    // only want the candidate set.
    assert_eq!(
        resolve(&cfg, &b, &tenant, "gpt-4o").unwrap_err(),
        RouteError::NoAvailableProvider
    );
}

/// The other half of the contract: nothing dropped ⇒ nothing attributed. A
/// counter that fires on healthy requests would be noise, not a signal.
#[test]
fn resolve_detailed_attributes_nothing_when_all_providers_are_usable() {
    let cfg = base_cfg();
    let out = resolve_detailed(&cfg, &alive_breaker(), &tenant(), "gpt-4o").expect("resolves");
    assert!(
        out.excluded.is_empty(),
        "no drops ⇒ no attribution, got {:?}",
        out.excluded
    );
    assert_eq!(
        resolve_set(&out.candidates),
        HashSet::from(["p_a".into(), "p_b".into(), "p_c".into()])
    );
}

/// `(provider_id, reason label)` for each exclusion, in reported order.
fn excluded_reasons(out: &hydra_core::model::ResolveOutcome) -> Vec<(String, &'static str)> {
    out.excluded
        .iter()
        .map(|e| (e.provider_id.clone(), e.reason.as_str()))
        .collect()
}

/// Round 48 (review) — `weight == 0` and `weight < 0` mean OPPOSITE things and
/// must be told apart:
///
/// * `weight == 0` is a SUPPORTED, deliberate soft-disable (`ops.md`), so the
///   proxy does not count it at all: a per-request counter for an intentional
///   state would track traffic, and an `increase(...) > 0` alert rule would fire
///   forever on a fleet configured exactly as intended;
/// * `weight < 0` is a configuration ERROR (`config::validate` warns the provider
///   "can never be selected"), so it is counted as the fault it is.
///
/// The routing outcome is identical (both are dropped from `candidates`); only the
/// reported reason differs.
#[test]
fn resolve_detailed_separates_a_deliberate_disable_from_a_bad_weight() {
    let tenant = tenant();
    let b = alive_breaker();

    let mut soft = base_cfg();
    soft.providers.insert("p_b".into(), provider("p_b", 0));
    let out = resolve_detailed(&soft, &b, &tenant, "gpt-4o").expect("resolves");
    assert_eq!(
        excluded_reasons(&out),
        vec![("p_b".to_string(), "soft_disabled")],
        "a deliberate disable is still reported — just named as a choice"
    );
    assert_eq!(
        resolve_set(&out.candidates),
        HashSet::from(["p_a".to_string(), "p_c".to_string()]),
        "and it is still not a candidate"
    );

    let mut bad = base_cfg();
    bad.providers.insert("p_b".into(), provider("p_b", -3));
    let out = resolve_detailed(&bad, &b, &tenant, "gpt-4o").expect("resolves");
    assert_eq!(
        excluded_reasons(&out),
        vec![("p_b".to_string(), "invalid_weight")],
        "a negative weight is a fault, not a choice"
    );
}
