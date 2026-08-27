//! P0.1 — admission-queue policy resolution + validation tests (pure core).
//!
//! Design `dev-docs/design-admission-queue.md` §5 (config schema), §11 P0.1.
//!
//! These cover the pure half of the admission queue:
//! - [`resolve_policy`] field-by-field override resolution.
//! - [`validate`] rejecting meaningless queue-without-cap configs.
//! - serde defaults: a `Provider` parsed from JSON without the three new
//!   fields deserialises to `None` (opt-in, zero-regression rollout).

use std::collections::HashSet;

use hydra_core::config::{
    resolve_policy, validate, ConcurrencyPolicy, ConfigData, ModelProvider, Severity,
};
use hydra_core::model::Provider;

// --- ConcurrencyPolicy / resolve_policy -------------------------------------

const DEFAULTS: ConcurrencyPolicy = ConcurrencyPolicy {
    max_concurrency: 8,
    max_queue_depth: 16,
    queue_wait_timeout_ms: 2000,
};

/// All-`None` overrides ⇒ the resolved policy equals the defaults verbatim.
#[test]
fn resolve_policy_all_none_returns_defaults() {
    let p = resolve_policy(None, None, None, DEFAULTS);
    assert_eq!(p, DEFAULTS);
}

/// Each override wins field-by-field; un-overridden fields keep the default.
#[test]
fn resolve_policy_overrides_win_field_by_field() {
    // Override only concurrency.
    let p = resolve_policy(Some(32), None, None, DEFAULTS);
    assert_eq!(p.max_concurrency, 32);
    assert_eq!(p.max_queue_depth, DEFAULTS.max_queue_depth);
    assert_eq!(p.queue_wait_timeout_ms, DEFAULTS.queue_wait_timeout_ms);

    // Override only queue depth.
    let p = resolve_policy(None, Some(64), None, DEFAULTS);
    assert_eq!(p.max_concurrency, DEFAULTS.max_concurrency);
    assert_eq!(p.max_queue_depth, 64);
    assert_eq!(p.queue_wait_timeout_ms, DEFAULTS.queue_wait_timeout_ms);

    // Override only timeout.
    let p = resolve_policy(None, None, Some(5000), DEFAULTS);
    assert_eq!(p.max_concurrency, DEFAULTS.max_concurrency);
    assert_eq!(p.max_queue_depth, DEFAULTS.max_queue_depth);
    assert_eq!(p.queue_wait_timeout_ms, 5000);

    // Override all three.
    let p = resolve_policy(Some(4), Some(8), Some(1000), DEFAULTS);
    assert_eq!(
        p,
        ConcurrencyPolicy {
            max_concurrency: 4,
            max_queue_depth: 8,
            queue_wait_timeout_ms: 1000,
        }
    );
}

/// `max_concurrency = Some(0)` is a legitimate explicit override meaning
/// "unlimited" — it must propagate through (NOT be replaced by the default).
#[test]
fn resolve_policy_explicit_zero_concurrency_is_unlimited_override() {
    let p = resolve_policy(Some(0), None, None, DEFAULTS);
    assert_eq!(p.max_concurrency, 0, "explicit 0 = unlimited opt-out");
}

// --- validate ---------------------------------------------------------------

fn provider(id: &str) -> Provider {
    Provider {
        id: id.into(),
        key: format!("k_{id}"),
        name: format!("{id} name"),
        endpoint: format!("https://{id}.example.com"),
        weight: 1,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        max_concurrency: None,
        max_queue_depth: None,
        queue_wait_timeout_ms: None,
    }
}

/// Minimal clean config with one provider + key so the existing checks stay
/// silent; we only mutate the provider's concurrency fields per-test.
fn clean_config_with(p: Provider) -> ConfigData {
    let mut cfg = ConfigData::default();
    cfg.providers.insert(p.id.clone(), p);
    cfg.provider_keys.insert("p1".into(), vec!["sk-1".into()]);
    cfg.tenants_by_domain.insert(
        "acme.com".into(),
        hydra_core::model::Tenant {
            id: "t1".into(),
            name: "t1".into(),
            domain: "acme.com".into(),
            auth_url: "https://auth.acme.com/verify".into(),
            cert_key: None,
            cert_file: None,
            enabled: true,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        },
    );
    let mut tps = HashSet::new();
    tps.insert("p1".to_string());
    cfg.tenant_providers.insert("t1".into(), tps);
    cfg.models_by_key.insert(
        "gpt-4o".into(),
        vec![ModelProvider {
            provider_id: "p1".into(),
            weight: 1,
        }],
    );
    let mut tms = HashSet::new();
    tms.insert("gpt-4o".to_string());
    cfg.tenant_models.insert("t1".into(), tms);
    cfg
}

/// A provider with a valid cap + queue validates cleanly (no concurrency warn).
#[test]
fn validate_accepts_queue_with_concurrency_cap() {
    let mut p = provider("p1");
    p.max_concurrency = Some(8);
    p.max_queue_depth = Some(16);
    p.queue_wait_timeout_ms = Some(2000);
    let cfg = clean_config_with(p);

    let issues = validate(&cfg);
    let concurrency_warns: Vec<_> = issues
        .iter()
        .filter(|i| i.message.contains("max_queue_depth") || i.message.contains("max_concurrency"))
        .collect();
    assert!(
        concurrency_warns.is_empty(),
        "valid cap+queue must not warn, got {concurrency_warns:?}"
    );
}

/// `max_queue_depth = Some(8)` with `max_concurrency = None` ⇒ warn.
#[test]
fn validate_rejects_queue_without_concurrency_cap_none() {
    let mut p = provider("p1");
    p.max_queue_depth = Some(8);
    // max_concurrency stays None.
    let cfg = clean_config_with(p);

    let issues = validate(&cfg);
    assert!(
        issues.iter().any(|i| {
            i.severity == Severity::Warn
                && i.message.contains("p1")
                && i.message.contains("max_queue_depth")
                && i.message.contains("max_concurrency")
        }),
        "expected a queue-without-cap warning, got {issues:?}"
    );
}

/// `max_queue_depth = Some(8)` with `max_concurrency = Some(0)` (unlimited) ⇒ warn.
#[test]
fn validate_rejects_queue_with_unlimited_concurrency() {
    let mut p = provider("p1");
    p.max_concurrency = Some(0);
    p.max_queue_depth = Some(8);
    let cfg = clean_config_with(p);

    let issues = validate(&cfg);
    assert!(
        issues.iter().any(|i| {
            i.severity == Severity::Warn
                && i.message.contains("p1")
                && i.message.contains("max_concurrency=0")
        }),
        "expected a queue-with-unlimited-cap warning, got {issues:?}"
    );
}

/// `max_queue_depth = Some(0)` (fail-fast) is valid even without a cap: a
/// fail-fast policy never queues, so "no cap" is not meaningless.
#[test]
fn validate_allows_fail_fast_zero_queue_without_cap() {
    let mut p = provider("p1");
    p.max_queue_depth = Some(0);
    // max_concurrency stays None — fine, because depth==0 means no queueing.
    let cfg = clean_config_with(p);

    let issues = validate(&cfg);
    assert!(
        !issues
            .iter()
            .any(|i| i.message.contains("max_queue_depth") && i.message.contains("p1")),
        "fail-fast (depth=0) without a cap should NOT warn, got {issues:?}"
    );
}

/// `queue_wait_timeout_ms = Some(0)` ⇒ warn (a zero timeout is a misconfig).
#[test]
fn validate_rejects_zero_wait_timeout() {
    let mut p = provider("p1");
    p.queue_wait_timeout_ms = Some(0);
    let cfg = clean_config_with(p);

    let issues = validate(&cfg);
    assert!(
        issues
            .iter()
            .any(|i| i.severity == Severity::Warn && i.message.contains("queue_wait_timeout_ms=0")),
        "expected a zero-timeout warning, got {issues:?}"
    );
}

/// A provider with all-`None` concurrency fields validates cleanly (opt-out).
#[test]
fn validate_all_none_concurrency_is_clean() {
    let cfg = clean_config_with(provider("p1"));
    let issues = validate(&cfg);
    assert!(
        !issues.iter().any(|i| i.message.contains("max_concurrency")
            || i.message.contains("max_queue_depth")
            || i.message.contains("queue_wait_timeout_ms")),
        "all-None concurrency fields must not warn, got {issues:?}"
    );
}

// --- serde defaults ---------------------------------------------------------

/// A `Provider` deserialised from JSON WITHOUT the three new fields parses,
/// with each new field defaulting to `None` (zero-regression opt-in rollout).
#[test]
fn serde_provider_without_concurrency_fields_defaults_none() {
    let json = serde_json::json!({
        "id": "p1",
        "key": "openai",
        "name": "OpenAI",
        "endpoint": "https://api.openai.com",
        "weight": 3,
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
    });
    let p: Provider = serde_json::from_value(json).expect("deserialize provider");
    assert_eq!(p.id, "p1");
    assert_eq!(p.max_concurrency, None);
    assert_eq!(p.max_queue_depth, None);
    assert_eq!(p.queue_wait_timeout_ms, None);
}

/// A `Provider` WITH the three new fields round-trips through serde.
#[test]
fn serde_provider_with_concurrency_fields_roundtrips() {
    let p = Provider {
        id: "p1".into(),
        key: "openai".into(),
        name: "OpenAI".into(),
        endpoint: "https://api.openai.com".into(),
        weight: 3,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        max_concurrency: Some(8),
        max_queue_depth: Some(16),
        queue_wait_timeout_ms: Some(2000),
    };
    let json = serde_json::to_string(&p).expect("serialize");
    let back: Provider = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(p, back);
}
