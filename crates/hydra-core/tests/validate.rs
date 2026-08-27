//! T9.1–T9.6 — config-load validation (pure data-graph invariants only).
//!
//! Design §5.4 lists several load-time checks. The **pure** ones (no I/O) live
//! here in `config::validate`; the I/O-dependent ones are explicitly the W2
//! loader's responsibility (see module docs in `config.rs`).
//!
//! See `dev-docs/waves/wave-1-pure-core.md` §3.9.

use std::collections::{HashMap, HashSet};

use hydra_core::config::{validate, ConfigData, ModelProvider, Severity, ValidationIssue};
use hydra_core::model::{LimitRole, Provider, Tenant};
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
        models_by_key: HashMap::new(),
        tenant_providers: HashMap::new(),
        tenant_models: HashMap::new(),
        providers: HashMap::new(),
        provider_keys: HashMap::new(),
        limit_roles: Vec::new(),
        key_prefix_bindings: Vec::new(),
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
