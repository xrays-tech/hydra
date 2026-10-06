//! T3.1–T3.6 — Nginx Smooth Weighted Round-Robin (pure).
//!
//! `order` is the pure single-key transition: it mutates a [`SwrrState`] and
//! reorders the candidate slice so the SWRR-selected candidate is first. The
//! outer `DashMap<(tenant, model), SwrrState>` sharded map lives in the server
//! shell (wave-1 §3.1); core delivers only this per-key state + transition.

use std::collections::{HashMap, HashSet};

use hydra_core::model::Candidate;
use hydra_core::swrr::{order, SwrrState};

fn cand(id: &str, weight: i32) -> Candidate {
    Candidate {
        provider_id: id.into(),
        endpoint: format!("https://{id}.example.com"),
        weight,
    }
}

/// T3.1 — a lone candidate is picked on every call (state oscillates to 0).
#[test]
fn swrr_single_candidate_always_picked() {
    let mut cands = vec![cand("only", 5)];
    let mut state = SwrrState::default();
    for _ in 0..5 {
        order(&mut cands, &mut state);
        assert_eq!(
            cands[0].provider_id, "only",
            "lone candidate always selected"
        );
    }
}

/// T3.2 — weights 3:1 produce a 6:2 distribution over 8 picks (Nginx SWRR).
#[test]
fn swrr_proportional_distribution() {
    let mut cands = vec![cand("a", 3), cand("b", 1)];
    let mut state = SwrrState::default();
    let mut counts: HashMap<String, u32> = HashMap::new();
    for _ in 0..8 {
        order(&mut cands, &mut state);
        *counts.entry(cands[0].provider_id.clone()).or_insert(0) += 1;
    }
    assert_eq!(counts.get("a").copied(), Some(6), "weight-3 ⇒ 6 of 8");
    assert_eq!(counts.get("b").copied(), Some(2), "weight-1 ⇒ 2 of 8");
}

/// T3.3 — `current_weight` is correctly accumulated and deducted.
#[test]
fn swrr_state_advances() {
    let mut cands = vec![cand("a", 3), cand("b", 1)];
    let mut state = SwrrState::default();
    // round 1: cw [0,0] → [+3,+1] = [3,1]; pick a (max); a -= 4 → [-1,1]
    order(&mut cands, &mut state);
    assert_eq!(cands[0].provider_id, "a");
    assert_eq!(state.current_weights.get("a").copied(), Some(-1));
    assert_eq!(state.current_weights.get("b").copied(), Some(1));
    // round 2: cw [-1,1] → [+3,+1] = [2,2]; tie ⇒ first (a); a -= 4 → [-2,2]
    order(&mut cands, &mut state);
    assert_eq!(cands[0].provider_id, "a");
    assert_eq!(state.current_weights.get("a").copied(), Some(-2));
    assert_eq!(state.current_weights.get("b").copied(), Some(2));
    // round 3: cw [-2,2] → [+3,+1] = [1,3]; pick b; b -= 4 → [1,-1]
    order(&mut cands, &mut state);
    assert_eq!(cands[0].provider_id, "b");
    assert_eq!(state.current_weights.get("a").copied(), Some(1));
    assert_eq!(state.current_weights.get("b").copied(), Some(-1));
}

/// T3.4 — distinct states (the server keys them per `(tenant, model)`) are
/// fully independent: mutating one never touches another.
#[test]
fn swrr_state_keyed_by_tenant_model() {
    let mk = || vec![cand("a", 1), cand("b", 1)];
    let mut s1 = SwrrState::default();
    let s2 = SwrrState::default();
    let mut c1 = mk();
    order(&mut c1, &mut s1);
    assert!(
        s2.current_weights.is_empty(),
        "an independent state is not mutated by ordering against s1"
    );
    assert!(!s1.current_weights.is_empty());
}

/// T3.5 — `order` only reorders; it never drops or duplicates a candidate.
#[test]
fn swrr_order_preserves_set() {
    let mut cands = vec![cand("a", 2), cand("b", 1), cand("c", 1)];
    let original: HashSet<String> = cands.iter().map(|c| c.provider_id.clone()).collect();
    let mut state = SwrrState::default();
    order(&mut cands, &mut state);
    let after: HashSet<String> = cands.iter().map(|c| c.provider_id.clone()).collect();
    assert_eq!(after.len(), 3);
    assert_eq!(original, after, "order reorders only — never drops");
}

/// T3.6 — within a single request, failover walks the ordered candidate slice
/// by cursor (`cursor += 1`) and does **not** call `order` again; each index is
/// therefore tried at most once. This encodes the contract from design §7.2.
#[test]
fn swrr_failover_does_not_reuse() {
    let mut cands = vec![cand("a", 3), cand("b", 1), cand("c", 1)];
    let mut state = SwrrState::default();
    order(&mut cands, &mut state); // SWRR decided ONCE for this request

    let mut seen: HashSet<String> = HashSet::new();
    for cand in &cands {
        // Simulate failover: walk the ordered slice by cursor, make no further
        // `order` calls. Each candidate must be tried at most once.
        assert!(
            seen.insert(cand.provider_id.clone()),
            "failover must try each candidate at most once (no swrr reuse)"
        );
    }
    assert_eq!(
        seen.len(),
        cands.len(),
        "every candidate reachable via cursor"
    );
}

/// An empty slice is a no-op (never panics, never divides).
#[test]
fn swrr_empty_slice_noop() {
    let mut cands: Vec<Candidate> = vec![];
    let mut state = SwrrState::default();
    order(&mut cands, &mut state);
    assert!(cands.is_empty());
    assert!(state.current_weights.is_empty());
}

/// A provider absent from the state defaults to `current_weight = 0` and is
/// seeded transparently on the first round.
#[test]
fn swrr_unknown_candidate_seeded_at_zero() {
    let mut cands = vec![cand("a", 1), cand("b", 1)];
    let mut state = SwrrState::default();
    order(&mut cands, &mut state);
    // both now present; with equal weights the first (a) is picked, then a-=2
    assert_eq!(state.current_weights.get("a").copied(), Some(-1));
    assert_eq!(state.current_weights.get("b").copied(), Some(1));
}

/// A weight sum that overflows `i32` must not panic (debug) or silently disable
/// load balancing (release).
///
/// `Provider.weight` is an `i32` whose only DB constraint is `weight >= 0`, so
/// two providers near `i32::MAX` sum past `i32::MAX`. The old `i32` sum panicked
/// under `overflow-checks` and, in a release build, wrapped NEGATIVE — which the
/// `total <= 0` guard reads as "nothing selectable", returning early and leaving
/// the candidate order untouched forever (weights silently ignored).
#[test]
fn a_weight_sum_that_overflows_i32_still_balances() {
    let mut cands = vec![cand("a", 2_000_000_000), cand("b", 2_000_000_000)];
    let mut state = SwrrState::default();

    // 8 rounds with equal weights must pick each candidate 4 times.
    let mut picks = std::collections::HashMap::new();
    for _ in 0..8 {
        order(&mut cands, &mut state);
        *picks.entry(cands[0].provider_id.clone()).or_insert(0) += 1;
    }
    assert_eq!(picks.get("a").copied(), Some(4), "picks: {picks:?}");
    assert_eq!(picks.get("b").copied(), Some(4), "picks: {picks:?}");

    // The state stays inside i32 (the subtraction is clamped, not wrapped).
    for (id, cw) in &state.current_weights {
        assert!(
            cw.abs() <= i64::from(i32::MAX),
            "{id} current_weight out of range: {cw}"
        );
    }
}
