//! T4.1–T4.6 — pure circuit-breaker state machine.
//!
//! The `Breaker` here is the PURE state machine only: consecutive-failure
//! counter + dead-set, driven by explicit `on_failure` / `on_success` events.
//! There is no `DashSet`, no background probe, no `Instant::now()` — those are
//! the W4 server shell's concern. The shell's probe task recovers a provider by
//! calling `on_success` (the read/write interface the core exposes).

use std::collections::HashSet;

use hydra_core::breaker::{Breaker, BreakerConfig, BreakerView};
use hydra_core::config::{ConfigData, ModelProvider};
use hydra_core::model::{Provider, Tenant};
use hydra_core::router::resolve;

fn breaker(threshold: u32) -> Breaker {
    Breaker::new(BreakerConfig { threshold })
}

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

/// Two providers (p_a, p_b) both serving `gpt-4o`, both authorised for t_acme,
/// each with one api-key and weight 1.
fn cfg_two_providers() -> (ConfigData, Tenant) {
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
        ],
    );
    cfg.tenant_providers
        .insert("t_acme".into(), HashSet::from(["p_a".into(), "p_b".into()]));
    cfg.providers.insert("p_a".into(), provider("p_a", 1));
    cfg.providers.insert("p_b".into(), provider("p_b", 1));
    cfg.provider_keys.insert("p_a".into(), vec!["sk-a".into()]);
    cfg.provider_keys.insert("p_b".into(), vec!["sk-b".into()]);
    (cfg, tenant())
}

/// T4.1 — failures below the threshold keep a provider alive.
#[test]
fn breaker_below_threshold_stays_alive() {
    let mut b = breaker(3);
    b.on_failure("p_a");
    b.on_failure("p_a");
    assert!(
        !b.is_dead("p_a"),
        "2 consecutive fails < threshold 3 ⇒ alive"
    );
}

/// T4.2 — `threshold` consecutive `on_failure` calls mark the provider dead.
#[test]
fn breaker_threshold_marks_dead() {
    let mut b = breaker(3);
    b.on_failure("p_a");
    b.on_failure("p_a");
    assert!(!b.is_dead("p_a"), "still below threshold");
    b.on_failure("p_a"); // 3rd consecutive ⇒ dead
    assert!(b.is_dead("p_a"));
}

/// T4.3 — `on_success` clears both the failure counter and the dead flag.
#[test]
fn breaker_success_resets() {
    let mut b = breaker(3);
    for _ in 0..3 {
        b.on_failure("p_a");
    }
    assert!(b.is_dead("p_a"));
    b.on_success("p_a");
    assert!(!b.is_dead("p_a"), "on_success removes from dead-set");
    // Counter reset: 2 fresh failures must NOT be enough anymore.
    b.on_failure("p_a");
    b.on_failure("p_a");
    assert!(
        !b.is_dead("p_a"),
        "counter was reset by success; 2 fresh fails < threshold 3"
    );
}

/// T4.4 — a success between failures resets the *consecutive* counter.
#[test]
fn breaker_non_consecutive_reset() {
    let mut b = breaker(3);
    b.on_failure("p_a");
    b.on_failure("p_a"); // count = 2
    b.on_success("p_a"); // reset → 0
    b.on_failure("p_a");
    b.on_failure("p_a"); // count = 2 again (NOT 4) ⇒ alive
    assert!(
        !b.is_dead("p_a"),
        "a success in the middle resets the consecutive-failure count"
    );
}

/// T4.5 — a `Breaker` used as `&dyn BreakerView` is honoured by `resolve`.
#[test]
fn breaker_dead_view_filtered_by_resolve() {
    let (cfg, tenant) = cfg_two_providers();
    let mut b = breaker(1);
    b.on_failure("p_a"); // threshold 1 ⇒ dead immediately
    assert!(b.is_dead("p_a"));

    let view: &dyn BreakerView = &b;
    let cands = resolve(&cfg, view, &tenant, "gpt-4o").expect("p_b still alive");
    let ids: HashSet<&str> = cands.iter().map(|c| c.provider_id.as_str()).collect();
    assert_eq!(
        ids,
        HashSet::from(["p_b"]),
        "dead p_a filtered out, only p_b remains"
    );
}

/// T4.6 — the dead-set is additive across providers and only clears on
/// `on_success` (the probe-success hook). Core performs no time-based probing.
#[test]
fn breaker_deadset_is_additive_until_probe() {
    let mut b = breaker(2);
    b.on_failure("p_a");
    b.on_failure("p_a"); // p_a dead
    b.on_failure("p_b");
    b.on_failure("p_b"); // p_b dead
    assert!(b.is_dead("p_a"));
    assert!(b.is_dead("p_b"));

    // Probe success (shell-driven) recovers exactly one provider.
    b.on_success("p_a");
    assert!(
        !b.is_dead("p_a"),
        "on_success (probe) removes p_a from dead-set"
    );
    assert!(
        b.is_dead("p_b"),
        "untouched provider stays dead — core has no auto-recovery"
    );
}

/// A dead provider that is later reset re-enters the normal failure path.
#[test]
fn breaker_can_re_die_after_recovery() {
    let mut b = breaker(2);
    b.on_failure("p_a");
    b.on_failure("p_a");
    assert!(b.is_dead("p_a"));
    b.on_success("p_a");
    assert!(!b.is_dead("p_a"));
    b.on_failure("p_a");
    b.on_failure("p_a");
    assert!(
        b.is_dead("p_a"),
        "after recovery the counter restarts from 0"
    );
}

/// Unknown providers are simply not dead (read view is total).
#[test]
fn breaker_unknown_provider_not_dead() {
    let b = breaker(3);
    assert!(!b.is_dead("never-seen"));
}
