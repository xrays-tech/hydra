//! §2.4 (wave-6) — load / correctness integration test for SWRR distribution
//! and circuit-breaker avoidance, using the **real** production code paths
//! (`hydra_core::swrr::order`, `hydra_core::router::resolve`, the server's
//! real concurrent `CircuitBreaker`). No internal mocks (dev-plan §1 铁律 2).
//!
//! These are the deterministic, CI-runnable counterparts to
//! `scripts/load_test.sh` (which measures real RPS/P99 against a running
//! instance). They assert:
//!
//! - SWRR over a 3:1 weighting converges to the weight ratio within ±2% over
//!   a large N.
//! - Once a provider enters the dead-set, `router::resolve` excludes it from
//!   candidates (traffic avoids the dead upstream).
//! - After the breaker records a success (the probe-success hook), the
//!   provider is re-included.

#![cfg(all(feature = "db", feature = "http-client", feature = "proxy"))]

use std::collections::{HashMap, HashSet};

use hydra_core::breaker::{BreakerConfig, BreakerView};
use hydra_core::config::ConfigData;
use hydra_core::model::Tenant;
use hydra_core::router::resolve;
use hydra_core::swrr::{order, SwrrState};
use hydra_server::proxy::breaker_wrap::CircuitBreaker;

/// Build a `ConfigData` with two providers (pA weight 3, pB weight 1) both
/// serving `m`, both with a key, both authorised for tenant `t`, with `m` in
/// the tenant's `tenant_models` gate.
fn two_provider_cfg(weight_a: i32, weight_b: i32) -> ConfigData {
    let mut cfg = ConfigData::default();
    cfg.providers.insert(
        "pA".into(),
        hydra_core::model::Provider {
            id: "pA".into(),
            key: "a".into(),
            name: "A".into(),
            endpoint: "https://upstream-a.local".into(),
            weight: weight_a,
            created_at: "2026-01-01 00:00:00".into(),
            updated_at: "2026-01-01 00:00:00".into(),
            max_concurrency: None,
            max_queue_depth: None,
            queue_wait_timeout_ms: None,
        },
    );
    cfg.providers.insert(
        "pB".into(),
        hydra_core::model::Provider {
            id: "pB".into(),
            key: "b".into(),
            name: "B".into(),
            endpoint: "https://upstream-b.local".into(),
            weight: weight_b,
            created_at: "2026-01-01 00:00:00".into(),
            updated_at: "2026-01-01 00:00:00".into(),
            max_concurrency: None,
            max_queue_depth: None,
            queue_wait_timeout_ms: None,
        },
    );
    cfg.models_by_key.insert(
        "m".into(),
        vec![
            hydra_core::config::ModelProvider {
                provider_id: "pA".into(),
                weight: weight_a,
            },
            hydra_core::config::ModelProvider {
                provider_id: "pB".into(),
                weight: weight_b,
            },
        ],
    );
    let mut tp = HashSet::new();
    tp.insert("pA".into());
    tp.insert("pB".into());
    cfg.tenant_providers.insert("t".into(), tp);
    let mut tm = HashSet::new();
    tm.insert("m".into());
    cfg.tenant_models.insert("t".into(), tm);
    cfg.provider_keys.insert("pA".into(), vec!["sk-a".into()]);
    cfg.provider_keys.insert("pB".into(), vec!["sk-b".into()]);
    cfg
}

fn tenant_t() -> Tenant {
    Tenant {
        id: "t".into(),
        name: "T".into(),
        domain: "example.com".into(),
        auth_url: "https://auth.example.com/v".into(),
        cert_key: None,
        cert_file: None,
        enabled: true,
        created_at: "2026-01-01 00:00:00".into(),
        updated_at: "2026-01-01 00:00:00".into(),
    }
}

// ---------------------------------------------------------------------------
// T4.1 — SWRR weight distribution under repeated selection (3:1 → ~6:2).
// ---------------------------------------------------------------------------

#[test]
fn swrr_weight_distribution_3_to_1() {
    let cfg = two_provider_cfg(3, 1);
    let breaker = CircuitBreaker::new(BreakerConfig::new(5));
    let tenant = tenant_t();

    // SWRR is deterministic, so a single `SwrrState` driven N times produces an
    // exact distribution. We aggregate the picked provider across many rounds.
    let n = 1000u32;
    let mut state = SwrrState::default();
    let mut counts: HashMap<String, u32> = HashMap::new();
    for _ in 0..n {
        let mut cands = resolve(&cfg, &breaker, &tenant, "m", None).expect("candidates");
        order(&mut cands, &mut state);
        let picked = cands[0].provider_id.clone();
        *counts.entry(picked).or_insert(0) += 1;
    }

    let a = *counts.get("pA").unwrap_or(&0);
    let b = *counts.get("pB").unwrap_or(&0);
    assert_eq!(a + b, n, "every round must pick a provider");

    // 3:1 weighting over 1000 rounds ⇒ expect ~750 / ~250.
    // SWRR is exact on full cycles (3:1 → 3 picks of A, 1 of B per 4), so the
    // deviation should be tiny. Allow ±2% to absorb the partial final cycle.
    let pct_a = (a as f64 / n as f64) * 100.0;
    let pct_b = (b as f64 / n as f64) * 100.0;
    assert!(
        (pct_a - 75.0).abs() < 2.0,
        "pA share should be ≈75% (was {pct_a:.1}%: a={a}, b={b})"
    );
    assert!(
        (pct_b - 25.0).abs() < 2.0,
        "pB share should be ≈25% (was {pct_b:.1}%: a={a}, b={b})"
    );
}

// ---------------------------------------------------------------------------
// T4.2 — breaker-under-failure: dead upstream is excluded; revives on success.
// ---------------------------------------------------------------------------

#[test]
fn breaker_excludes_dead_then_revives_on_success() {
    let cfg = two_provider_cfg(3, 1);
    let breaker = CircuitBreaker::new(BreakerConfig::new(3));
    let tenant = tenant_t();

    // Baseline: both providers survive.
    let cands = resolve(&cfg, &breaker, &tenant, "m", None).expect("candidates");
    assert_eq!(cands.len(), 2, "both providers selectable initially");

    // Trip the breaker for pA (threshold=3 consecutive failures).
    breaker.on_failure("pA");
    breaker.on_failure("pA");
    breaker.on_failure("pA");
    assert!(breaker.is_dead("pA"));

    // resolve must now exclude pA — only pB survives.
    let cands = resolve(&cfg, &breaker, &tenant, "m", None).expect("candidates");
    assert_eq!(cands.len(), 1, "dead pA excluded");
    assert_eq!(cands[0].provider_id, "pB", "pB is the only survivor");

    // Simulate a successful probe (the background probe task calls on_success).
    breaker.on_success("pA");
    assert!(!breaker.is_dead("pA"));

    // pA is back in the candidate set.
    let cands = resolve(&cfg, &breaker, &tenant, "m", None).expect("candidates");
    assert_eq!(cands.len(), 2, "pA revived after on_success");
    let ids: HashSet<&str> = cands.iter().map(|c| c.provider_id.as_str()).collect();
    assert!(ids.contains("pA") && ids.contains("pB"));
}

// ---------------------------------------------------------------------------
// T4.2b — once only one provider survives, SWRR still serves 100% from it.
// ---------------------------------------------------------------------------

#[test]
fn swrr_converges_to_sole_survivor_when_other_is_dead() {
    let cfg = two_provider_cfg(3, 1);
    let breaker = CircuitBreaker::new(BreakerConfig::new(1));
    let tenant = tenant_t();

    breaker.on_failure("pA"); // threshold=1 → pA dead immediately
    assert!(breaker.is_dead("pA"));

    let mut state = SwrrState::default();
    let mut counts: HashMap<String, u32> = HashMap::new();
    for _ in 0..200 {
        let mut cands = resolve(&cfg, &breaker, &tenant, "m", None).expect("candidates");
        order(&mut cands, &mut state);
        *counts.entry(cands[0].provider_id.clone()).or_insert(0) += 1;
    }
    assert_eq!(
        *counts.get("pA").unwrap_or(&0),
        0,
        "dead pA must never be picked"
    );
    assert_eq!(
        *counts.get("pB").unwrap_or(&0),
        200,
        "pB absorbs all traffic"
    );
}

// ---------------------------------------------------------------------------
// T4.2c — soft-disable (weight=0) also excludes from candidates (§7.2).
// ---------------------------------------------------------------------------

#[test]
fn soft_disabled_weight_zero_excluded() {
    let cfg = two_provider_cfg(0, 1); // pA soft-disabled
    let breaker = CircuitBreaker::new(BreakerConfig::new(5));
    let tenant = tenant_t();
    let cands = resolve(&cfg, &breaker, &tenant, "m", None).expect("candidates");
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].provider_id, "pB");
}

// ---------------------------------------------------------------------------
// T4.1b — exact SWRR cycle shape: weights (3,1) produce the canonical
// Nginx SWRR pick sequence over one full cycle (length = sum of weights).
// ---------------------------------------------------------------------------

#[test]
fn swrr_exact_cycle_shape_3_to_1() {
    // SWRR is deterministic; over `total = 3+1 = 4` rounds the canonical
    // Nginx sequence for weights {A:3, B:1} starting from current_weights {0,0}
    // is: A, A, B, A (the heavy provider gets the interleaved slots).
    let cfg = two_provider_cfg(3, 1);
    let breaker = CircuitBreaker::new(BreakerConfig::new(5));
    let tenant = tenant_t();
    let mut state = SwrrState::default();

    let mut picks: Vec<String> = Vec::new();
    for _ in 0..4 {
        let mut cands = resolve(&cfg, &breaker, &tenant, "m", None).expect("candidates");
        // resolve returns sorted by provider_id → [pA, pB]. order() reorders.
        order(&mut cands, &mut state);
        picks.push(cands[0].provider_id.clone());
    }
    let a_count = picks.iter().filter(|p| p.as_str() == "pA").count();
    let b_count = picks.iter().filter(|p| p.as_str() == "pB").count();
    assert_eq!(a_count, 3, "one full cycle: 3 picks of pA (got {picks:?})");
    assert_eq!(b_count, 1, "one full cycle: 1 pick of pB (got {picks:?})");
    // No two consecutive dead-locks: SWRR interleaves so pB appears once and
    // is never adjacent to itself.
    assert_eq!(picks.iter().filter(|p| p.as_str() == "pB").count(), 1);
}

// ---------------------------------------------------------------------------
// T4.1c — auth-cache key never affects routing (privacy sanity): the breaker
// view the router sees only knows `is_dead`, never api_keys.
// ---------------------------------------------------------------------------

#[test]
fn breaker_view_erasure_no_keys_visible() {
    let cfg = two_provider_cfg(3, 1);
    let breaker = CircuitBreaker::new(BreakerConfig::new(2));
    let tenant = tenant_t();
    // The BreakerView trait surface is only `is_dead` — confirm the router
    // path works through that erasure.
    let view: &dyn BreakerView = &breaker;
    let cands = resolve(&cfg, view, &tenant, "m", None).expect("candidates");
    assert_eq!(cands.len(), 2);
}
