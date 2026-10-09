//! T9.1–T9.6 — config-load validation (pure data-graph invariants only).
//!
//! Design §5.4 lists several load-time checks. The **pure** ones (no I/O) live
//! here in `config::validate`; the I/O-dependent ones are explicitly the W2
//! loader's responsibility (see module docs in `config.rs`).
//!
//! See `dev-docs/waves/wave-1-pure-core.md` §3.9.

use std::collections::{HashMap, HashSet};

use hydra_core::config::{validate, ConfigData, ModelProvider, Severity, ValidationIssue};
use hydra_core::model::{LimitRole, Provider, SubTenant, SubTenantRoute, Tenant};
use pretty_assertions::assert_eq;

// --- fixtures ---------------------------------------------------------------

fn tenant(id: &str, domain: &str) -> Tenant {
    Tenant {
        id: id.into(),
        name: format!("{id} name"),
        domain: domain.into(),
        auth_url: format!("https://auth.{domain}/verify"),
        cert_key: None,
        cert_file: None,
        enabled: true,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    }
}

fn provider(id: &str, endpoint: &str, weight: i32) -> Provider {
    Provider {
        id: id.into(),
        key: format!("key_{id}"),
        name: format!("{id} name"),
        endpoint: endpoint.into(),
        weight,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        max_concurrency: None,
        max_queue_depth: None,
        queue_wait_timeout_ms: None,
    }
}

/// A `limit_role` that declares a PROVIDER dimension can never match, and that
/// must be reported rather than silently ignored.
///
/// The pre-gate builds its `MatchCtx` before routing (`proxy.rs`), so `provider`
/// is `None` there, while `limit::dim_matches` requires equality with the
/// configured value — the role is skipped by BOTH the count and the token check.
/// It is still listed, persisted and editable in the admin API, so a silent skip
/// means an operator believes a provider is capped when nothing is enforced. (The
/// accounting path DOES pass a provider, so such a role also creates a window that
/// is written and never read.)
///
/// Falsification: delete the `matching_provider.is_some()` warning in `config.rs`
/// and this fails.
#[test]
fn validate_limit_role_with_a_provider_dimension_is_reported_as_dead() {
    let mut cfg = clean_config();
    let mut role = limit_role("r-provider", Some(600), None);
    role.matching_provider = Some("p1".to_string());
    cfg.limit_roles.push(role);

    let warns = warn_messages(&validate(&cfg));
    assert!(
        warns
            .iter()
            .any(|m| m.contains("matching_provider") && m.contains("r-provider")),
        "a role whose provider dimension cannot match must be named, got {warns:?}"
    );
}

/// A key-scoped role with no tenant scope is a CROSS-TENANT budget, and it must be named.
///
/// Measured 2026-09-30 (round 116 made the raw-key form match, so roles that used to be silently
/// inert became live): the window belongs to `(role_id, digest(key))`, so two tenants whose auth
/// backends accept the same key string share one budget — one tenant's traffic can exhaust the
/// other's. `validate` warned about the dead `matching_provider` dimension and said nothing here.
///
/// Falsification: delete the `matching_key.is_some() && matching_tenant.is_none()` warning in
/// `config.rs` and this fails.
#[test]
fn validate_key_scoped_role_without_a_tenant_scope_is_reported_as_cross_tenant() {
    let mut cfg = clean_config();
    let mut role = limit_role("r-key", Some(600), None);
    role.matching_key = Some("sk-shared-probe".to_string());
    cfg.limit_roles.push(role);

    let warns = warn_messages(&validate(&cfg));
    assert!(
        warns
            .iter()
            .any(|m| m.contains("matching_tenant NULL") && m.contains("r-key")),
        "a key-scoped role with no tenant scope must be named, got {warns:?}"
    );

    // CONTROL: adding the tenant scope silences it — otherwise the warning could be firing on
    // every key-scoped role, including the safe ones.
    let mut cfg2 = clean_config();
    let mut scoped = limit_role("r-key", Some(600), None);
    scoped.matching_key = Some("sk-shared-probe".to_string());
    scoped.matching_tenant = Some("t1".to_string());
    cfg2.limit_roles.push(scoped);
    let warns2 = warn_messages(&validate(&cfg2));
    assert!(
        !warns2.iter().any(|m| m.contains("matching_tenant NULL")),
        "a tenant-scoped role must not warn, got {warns2:?}"
    );
}

/// The VALUE of `matching_key` also gets a warning, because the forms have different costs and
/// only the documentation mentioned any of them (round 135, product-review P3-4 / P2-2; the raw-key
/// wording was corrected in round 210 when decision D-16 sealed the column).
///
/// Falsification: delete the `mask_key(key) != key` branch in `config.rs` and the first case fails;
/// delete the `else` branch and the second does.
#[test]
fn validate_reports_what_the_matching_key_value_costs() {
    // (a) a RAW key: recoverable by whoever holds the master key, and echoed by the admin API. The
    // warning NO LONGER says the column is plaintext — decision D-16 sealed it (2026-10-08), and a
    // warning that overstates its case is one operators learn to skip. Both halves are asserted, so
    // the text cannot drift back to the stale claim.
    let mut cfg = clean_config();
    let mut raw = limit_role("r-raw", Some(600), None);
    raw.matching_key = Some("sk-live-customer-key-0001".to_string());
    raw.matching_tenant = Some("t1".to_string());
    cfg.limit_roles.push(raw);
    let warns = warn_messages(&validate(&cfg));
    assert!(
        warns.iter().any(|m| m.contains("r-raw")
            && m.contains("RAW client key")
            && m.contains("GET /api/v1/limit-roles")),
        "a raw key in `matching_key` must be named, with the exposure that survives D-16, got {warns:?}"
    );
    assert!(
        !warns.iter().any(|m| m.contains("PLAINTEXT")),
        "the raw-key warning must not claim the column is PLAINTEXT any more (D-16 sealed it), got \
         {warns:?}"
    );

    // (b) the MASK form: no warning any more. The warning that used to sit here said any other key
    // whose mask is the same string SHARES the window, because the window was `(role_id, mask(key))`.
    // Decision D-15② keyed the window by a DIGEST of the raw key, so two keys that share a mask have
    // two windows — the condition cannot occur, and the warning was deleted with its premise. This
    // assertion is what keeps it from being re-added by someone reading an old note.
    let mut cfg2 = clean_config();
    let mut masked = limit_role("r-mask", Some(600), None);
    masked.matching_key = Some("sk***************ed".to_string());
    masked.matching_tenant = Some("t1".to_string());
    cfg2.limit_roles.push(masked);
    let warns2 = warn_messages(&validate(&cfg2));
    assert!(
        !warns2.iter().any(|m| m.contains("r-mask")),
        "the masked form must NOT be warned about any more (D-15② fixed what it described), got {warns2:?}"
    );

    // (c) the DIGEST form (decision D-16③): the recommended one — nothing recoverable is stored.
    let mut cfg4 = clean_config();
    let mut digest = limit_role("r-digest", Some(600), None);
    digest.matching_key = Some(::hydra_core::limit::key_digest("sk-live-customer-key-0001"));
    digest.matching_tenant = Some("t1".to_string());
    cfg4.limit_roles.push(digest);
    let warns4 = warn_messages(&validate(&cfg4));
    assert!(
        !warns4.iter().any(|m| m.contains("r-digest")),
        "the digest form stores nothing recoverable and must not be warned about, got {warns4:?}"
    );

    // CONTROL: no `matching_key` ⇒ no warning either (so the raw warning fires on the value, not on
    // every role).
    let mut cfg3 = clean_config();
    cfg3.limit_roles
        .push(limit_role("r-plain", Some(600), None));
    let warns3 = warn_messages(&validate(&cfg3));
    assert!(
        !warns3.iter().any(|m| m.contains("RAW client key")),
        "a role without `matching_key` must not produce the raw-key warning, got {warns3:?}"
    );
}

fn limit_role(id: &str, count: Option<i64>, token: Option<i64>) -> LimitRole {
    LimitRole {
        id: id.into(),
        name: format!("role_{id}"),
        matching_key: None,
        matching_model: None,
        matching_tenant: None,
        matching_provider: None,
        limit_count: count,
        limit_token: token,
        window: "m".into(),
        enabled: true,
        created_at: "2026-01-01T00:00:00Z".into(),
    }
}

/// A well-formed, minimal config: every reference resolves, every online
/// provider has a key, every role has a limit. `validate` ⇒ empty vec.
fn clean_config() -> ConfigData {
    let mut cfg = ConfigData::default();

    let t = tenant("t1", "acme.com");
    cfg.tenants_by_domain.insert(t.domain.clone(), t.clone());

    cfg.providers
        .insert("p1".into(), provider("p1", "https://a.io", 3));
    cfg.provider_keys.insert("p1".into(), vec!["sk-1".into()]);

    cfg.models_by_key.insert(
        "gpt-4o".into(),
        vec![ModelProvider {
            provider_id: "p1".into(),
            weight: 3,
        }],
    );

    let mut tps = HashSet::new();
    tps.insert("p1".to_string());
    cfg.tenant_providers.insert("t1".into(), tps);

    let mut tms = HashSet::new();
    tms.insert("gpt-4o".to_string());
    cfg.tenant_models.insert("t1".into(), tms);

    cfg.limit_roles.push(limit_role("lr1", Some(60), None));
    cfg
}

fn warn_messages(issues: &[ValidationIssue]) -> Vec<String> {
    issues
        .iter()
        .filter(|i| i.severity == Severity::Warn)
        .map(|i| i.message.clone())
        .collect()
}

// --- tests ------------------------------------------------------------------

/// T9.1 — `tenant_provider.provider_id` not present in `providers` ⇒ Warn.
#[test]
fn validate_dangling_tenant_provider() {
    let mut cfg = clean_config();
    // Reference a provider that does not exist.
    cfg.tenant_providers
        .get_mut("t1")
        .unwrap()
        .insert("ghost".to_string());

    let issues = validate(&cfg);
    let warns = warn_messages(&issues);
    assert!(
        warns
            .iter()
            .any(|m| m.contains("ghost") && m.contains("t1")),
        "expected a dangling-provider warning, got {warns:?}"
    );
}

/// T9.2 — `tenant_model.model_key` with no online provider offering it ⇒ Warn.
#[test]
fn validate_tenant_model_orphan() {
    let mut cfg = clean_config();
    cfg.tenant_models
        .get_mut("t1")
        .unwrap()
        .insert("orphan-model".to_string());

    let issues = validate(&cfg);
    let warns = warn_messages(&issues);
    assert!(
        warns
            .iter()
            .any(|m| m.contains("orphan-model") && m.contains("t1")),
        "expected an orphan-model warning, got {warns:?}"
    );
}

/// T9.4 — an online provider (weight != 0) without any api_key ⇒ Warn.
#[test]
fn validate_provider_without_key() {
    let mut cfg = clean_config();
    cfg.providers
        .insert("p2".into(), provider("p2", "https://b.io", 1));
    // No entry in provider_keys for p2.

    let issues = validate(&cfg);
    let warns = warn_messages(&issues);
    assert!(
        warns.iter().any(|m| m.contains("p2")),
        "expected a missing-keys warning for p2, got {warns:?}"
    );
}

/// A soft-disabled provider (weight == 0) without keys is NOT flagged — it is
/// never a candidate, so a missing key is harmless.
#[test]
fn validate_softdisabled_provider_without_key_is_silent() {
    let mut cfg = clean_config();
    cfg.providers
        .insert("p_off".into(), provider("p_off", "https://off.io", 0));
    // No keys for p_off, but weight == 0 ⇒ no warning.

    let issues = validate(&cfg);
    assert!(
        !issues.iter().any(|i| i.message.contains("p_off")),
        "soft-disabled provider should not be flagged, got {issues:?}"
    );
}

/// An empty key *list* is treated the same as a missing entry.
#[test]
fn validate_provider_with_empty_key_list() {
    let mut cfg = clean_config();
    cfg.providers
        .insert("p3".into(), provider("p3", "https://c.io", 2));
    cfg.provider_keys.insert("p3".into(), vec![]);

    let issues = validate(&cfg);
    assert!(
        issues.iter().any(|i| i.message.contains("p3")),
        "expected warning for empty key list, got {issues:?}"
    );
}

/// T9.5 — a `limit_role` with BOTH `limit_count` and `limit_token` NULL ⇒ Warn.
#[test]
fn validate_limit_role_both_null() {
    let mut cfg = clean_config();
    cfg.limit_roles.push(limit_role("lr_bad", None, None));

    let issues = validate(&cfg);
    let warns = warn_messages(&issues);
    assert!(
        warns.iter().any(|m| m.contains("lr_bad")),
        "expected a both-null limit-role warning, got {warns:?}"
    );
    // A role with only one dimension set stays clean.
    cfg.limit_roles
        .push(limit_role("lr_ok_count", Some(10), None));
    cfg.limit_roles
        .push(limit_role("lr_ok_token", None, Some(100)));
    let issues = validate(&cfg);
    assert!(
        !issues.iter().any(|i| i.message.contains("lr_ok_")),
        "single-dimension roles should not be flagged, got {issues:?}"
    );
}

/// T9.6 — a clean config yields NO issues.
#[test]
fn validate_clean_config_no_issues() {
    let issues = validate(&clean_config());
    assert_eq!(
        issues,
        Vec::<ValidationIssue>::new(),
        "clean config must validate with zero issues"
    );
}

/// T9.7 (adapted to pure-only) — every issue the pure validator can emit is
/// `Warn`; `Fatal` is reserved for I/O-dependent checks (endpoint-URL parsing,
/// cert-file readability) which belong to the W2 loader.
#[test]
fn validate_pure_issues_are_all_warn() {
    let mut cfg = clean_config();
    // Stir in one of every pure defect.
    cfg.tenant_providers
        .get_mut("t1")
        .unwrap()
        .insert("g1".into());
    cfg.tenant_models.get_mut("t1").unwrap().insert("m1".into());
    cfg.providers
        .insert("p9".into(), provider("p9", "https://d.io", 1));
    cfg.limit_roles.push(limit_role("lr_bad", None, None));

    let issues = validate(&cfg);
    assert!(
        !issues.is_empty(),
        "expected multiple issues from a polluted config"
    );
    assert!(
        issues.iter().all(|i| i.severity == Severity::Warn),
        "pure validator must only emit Warn; got {issues:?}"
    );
}

/// Determinism: `validate` returns a stable order regardless of HashMap
/// iteration randomness.
#[test]
fn validate_output_is_deterministic() {
    let mut cfg = clean_config();
    cfg.tenant_providers
        .get_mut("t1")
        .unwrap()
        .insert("zeta".into());
    cfg.tenant_providers
        .get_mut("t1")
        .unwrap()
        .insert("alpha".into());

    let a = validate(&cfg);
    let b = validate(&cfg);
    assert_eq!(a, b, "validate output must be stable across calls");
}

/// A default (fully empty) config validates cleanly: nothing references
/// anything, so there is nothing dangling.
#[test]
fn validate_empty_config_is_clean() {
    let cfg = ConfigData {
        tenants_by_domain: HashMap::new(),
        tenants_by_id: HashMap::new(),
        models_by_key: HashMap::new(),
        tenant_providers: HashMap::new(),
        tenant_models: HashMap::new(),
        providers: HashMap::new(),
        provider_keys: HashMap::new(),
        limit_roles: Vec::new(),
        key_prefix_bindings: Vec::new(),
        sub_tenants: Vec::new(),
        sub_tenant_routes: Vec::new(),
        certs: HashMap::new(),
    };
    assert!(validate(&cfg).is_empty());
}

fn binding(id: &str, prefix: &str, provider_id: &str) -> hydra_core::model::ProviderKeyBinding {
    hydra_core::model::ProviderKeyBinding {
        id: id.into(),
        key_prefix: prefix.into(),
        provider_id: provider_id.into(),
        enabled: true,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    }
}

/// T9.8 — provider_key_binding references an unknown provider ⇒ Warn.
#[test]
fn validate_binding_unknown_provider() {
    let mut cfg = clean_config();
    cfg.key_prefix_bindings.push(binding("b1", "sk_", "ghost"));
    let warns = warn_messages(&validate(&cfg));
    assert!(
        warns
            .iter()
            .any(|m| m.contains("ghost") && m.contains("provider_key_binding")),
        "expected a dangling-provider warning, got {warns:?}"
    );
}

/// A provider with a NEGATIVE weight is discarded by the router (`weight > 0` is
/// required) but used to pass validation untouched, because this check skipped
/// only `weight == 0`. It is exactly the "validated clean, can never serve" case
/// the validator exists to catch.
#[test]
fn validate_negative_weight_provider_is_reported() {
    let mut cfg = clean_config();
    if let Some(p) = cfg.providers.values_mut().next() {
        p.weight = -1;
    }
    let warns = warn_messages(&validate(&cfg));
    assert!(
        warns.iter().any(|m| m.contains("weight")),
        "a provider the router will always drop must be reported, got {warns:?}"
    );
}

/// The predicate itself: `weight > 0` is "online" everywhere, so this test uses 0
/// — the deliberate soft-disable. The ROUTER now distinguishes the two cases in
/// its attribution (`weight == 0` ⇒ `soft_disabled`, `weight < 0` ⇒
/// `invalid_weight`, `model.rs`), but both are equally unroutable here, and a
/// negative weight cannot come from the DB anyway (`CHECK (weight >= 0)`).
#[test]
fn validate_soft_disabled_provider_without_key_is_silent() {
    let mut cfg = clean_config();
    let ids: Vec<String> = cfg.providers.keys().cloned().collect();
    for id in &ids {
        if let Some(p) = cfg.providers.get_mut(id) {
            p.weight = 0;
        }
    }
    cfg.provider_keys.clear();
    let warns = warn_messages(&validate(&cfg));
    assert!(
        !warns.iter().any(|m| m.contains("api_key")),
        "a soft-disabled provider needs no key, got {warns:?}"
    );
}

/// T9.9 — provider_key_binding with an empty prefix ⇒ Warn.
#[test]
fn validate_binding_empty_prefix() {
    let mut cfg = clean_config();
    cfg.key_prefix_bindings.push(binding("b1", "", "p1"));
    let warns = warn_messages(&validate(&cfg));
    assert!(
        warns.iter().any(|m| m.contains("empty key_prefix")),
        "expected an empty-prefix warning, got {warns:?}"
    );
}

// --- sub-tenant fixtures (design-sub-tenant.md) -----------------------------

fn sub_tenant(id: &str, tenant_id: &str, key_prefix: &str) -> SubTenant {
    SubTenant {
        id: id.into(),
        tenant_id: tenant_id.into(),
        name: format!("{id} name"),
        key_prefix: key_prefix.into(),
        enabled: true,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    }
}

fn sub_tenant_route(
    id: &str,
    sub_tenant_id: &str,
    model_key: Option<&str>,
    provider_id: &str,
) -> SubTenantRoute {
    SubTenantRoute {
        id: id.into(),
        sub_tenant_id: sub_tenant_id.into(),
        model_key: model_key.map(str::to_string),
        provider_id: provider_id.into(),
        enabled: true,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    }
}

/// Sub-tenant route whose `sub_tenant_id` is not present in `sub_tenants` ⇒
/// Warn (orphan), not Fatal.
#[test]
fn validate_sub_tenant_route_orphan_sub_tenant_warns() {
    let mut cfg = clean_config();
    // Route points at a sub-tenant that does not exist; provider p1 is known.
    cfg.sub_tenant_routes
        .push(sub_tenant_route("r1", "st_ghost", Some("gpt-4o"), "p1"));

    let issues = validate(&cfg);
    let warns = warn_messages(&issues);
    assert!(
        warns
            .iter()
            .any(|m| m.contains("r1") && m.contains("st_ghost")),
        "expected an orphan-sub-tenant warning, got {warns:?}"
    );
    assert!(
        issues.iter().all(|i| i.severity != Severity::Fatal),
        "orphan sub-tenant must be Warn, not Fatal; got {issues:?}"
    );
}

/// Sub-tenant route whose `provider_id` is not present in `providers` ⇒ Warn
/// (unknown provider), not Fatal.
#[test]
fn validate_sub_tenant_route_unknown_provider_warns() {
    let mut cfg = clean_config();
    // Valid sub-tenant, but the route references a provider that does not exist.
    cfg.sub_tenants.push(sub_tenant("st1", "t1", "QQCX_"));
    cfg.sub_tenant_routes
        .push(sub_tenant_route("r1", "st1", Some("gpt-4o"), "ghost"));

    let issues = validate(&cfg);
    let warns = warn_messages(&issues);
    assert!(
        warns
            .iter()
            .any(|m| m.contains("r1") && m.contains("ghost")),
        "expected an unknown-provider warning, got {warns:?}"
    );
    assert!(
        issues.iter().all(|i| i.severity != Severity::Fatal),
        "unknown provider must be Warn, not Fatal; got {issues:?}"
    );
}

/// Sub-tenant whose `key_prefix` has no separator (`_` or `-`) ⇒ Warn (invalid
/// prefix), not Fatal.
#[test]
fn validate_sub_tenant_prefix_without_separator_warns() {
    let mut cfg = clean_config();
    // Bare prefix: no `_` / `-`, so a `starts_with` match would swallow longer
    // unrelated prefixes (e.g. `QQCXWEB_`).
    cfg.sub_tenants.push(sub_tenant("st1", "t1", "QQCX"));

    let issues = validate(&cfg);
    let warns = warn_messages(&issues);
    assert!(
        warns.iter().any(|m| m.contains("st1")),
        "expected an invalid-key_prefix warning, got {warns:?}"
    );
    assert!(
        issues.iter().all(|i| i.severity != Severity::Fatal),
        "invalid prefix must be Warn, not Fatal; got {issues:?}"
    );
}

/// Two sub-tenants in the SAME tenant with overlapping prefixes (either
/// direction `starts_with`) ⇒ Warn (overlap), not Fatal.
#[test]
fn validate_sub_tenant_prefix_overlap_within_tenant_warns() {
    let mut cfg = clean_config();
    // Both prefixes are individually valid (contain a separator), but
    // `QQCX_W` is a superstring of `QQCX_` ⇒ overlap within tenant t1.
    cfg.sub_tenants.push(sub_tenant("st1", "t1", "QQCX_"));
    cfg.sub_tenants.push(sub_tenant("st2", "t1", "QQCX_W"));

    let issues = validate(&cfg);
    let warns = warn_messages(&issues);
    assert!(
        warns.iter().any(|m| m.contains("st1") && m.contains("st2")),
        "expected a prefix-overlap warning, got {warns:?}"
    );
    assert!(
        issues.iter().all(|i| i.severity != Severity::Fatal),
        "prefix overlap must be Warn, not Fatal; got {issues:?}"
    );
}
