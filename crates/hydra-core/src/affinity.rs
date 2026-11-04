//! Session affinity — stateless weighted rendezvous (HRW) provider choice
//! (plan §3.4 step 4: "有会话头 → 粘性首选 + 其余 provider_id 升序").
//!
//! Three pure functions over plain data:
//!
//! - [`sticky_key`] folds the four request-identity fields — `tenant_id`,
//!   `model_key`, `api_key_id`, `session` — into ONE opaque hex key
//!   (lower-case sha256 hex of the fields joined by the `0x1f` separator);
//! - [`pick_sticky`] maps that key onto a candidate provider with a
//!   **weighted rendezvous** — an exponential race: `score = -ln(u) / weight`
//!   is a draw from `Exp(rate = weight)`, so the **minimum** score wins
//!   (`P(i) ∝ weight_i`), ties broken by `provider_id` ascending — and returns
//!   the winning **index**;
//! - [`order_with_affinity`] applies that choice to a candidate list, moving
//!   the winner to the front and leaving the tail order untouched — which is
//!   exactly what `router::resolve` already guarantees (`provider_id`
//!   ascending), so failover keeps walking the tail.
//!
//! The server-side wiring (session-header precedence and the ≤256-byte limit,
//! `HYDRA_SESSION_AFFINITY`, the `hydra_sticky_hit_total` counter) lives in
//! `hydra-server` — this module owns only the pure selection. It does NOT call
//! [`crate::swrr`] and mutates no [`crate::swrr`] state: the sticky path never
//! touches the per-`(tenant, model)` SWRR accumulator (plan §3.4/§3.5), and
//! `swrr::order` is still reached when no session key is supplied.
//!
//! ## Purity
//!
//! No I/O, no time, no global state, no randomness: every decision is a
//! deterministic function of the inputs, which is the whole point. The leader
//! and every replica MUST resolve the same session to the same provider from
//! the same data, so the choice is derived from a digest rather than from per-node
//! mutable state.
//!
//! ## Why rendezvous (HRW), not a hash-into-a-list
//!
//! `hash(key) % n` reshuffles ~all sessions when `n` changes — an added provider
//! would re-pin every ongoing conversation. Rendezvous hashing scores each
//! provider independently, so adding or removing one remaps only the sessions
//! that scored it best (a `1/n` minority). The weight enters as a DIVISOR on the
//! exponential draw: `-ln(u) / weight` ~ `Exp(rate = weight)`, and the MINIMUM
//! wins, so a provider weighted 3 is pinned by ~3/4 of the sessions a provider
//! weighted 1 is pinned by — i.e. `P(i) ∝ weight_i`, the SAME split `swrr`
//! applies to request load, which is why the two paths cannot disagree about
//! what a weight means. The same `u ∈ (0,1]` per `(key, provider)` pair also
//! makes the pick stable across processes without any coordination.
//!
//! `u = 0` (the u64 head of the digest is exactly zero) maps to a score of
//! `+inf`: that provider LOSES the race — never picked while any other candidate
//! is selectable — rather than dividing by zero or poisoning the comparison.
//!
//! Degenerate input never panics: `weight <= 0` candidates are skipped
//! (`router`'s step 3 already drops them, but this module is public and must
//! not depend on that), and an empty candidate list yields `None` / `false`.

use crate::auth::{sha256_hex, sha256_hex_string};
use crate::model::Candidate;

/// The `0x1f` (US) byte separating the joined fields of a digest input.
///
/// A separator that cannot appear in a header-derived field keeps the join
/// unambiguous: `("ab", "c")` and `("a", "bc")` must NOT digest identically.
const FIELD_SEP: &str = "\u{1f}";

/// `2^64`, the divisor turning the digest's u64 head into `u ∈ [0,1)`.
const SCALE_2_POW_64: f64 = 18446744073709551616.0;

/// The session-affinity key: lower-case sha256 hex of
/// `tenant_id || 0x1f || model_key || 0x1f || api_key_id || 0x1f || session`.
///
/// Hex rather than the raw digest because the value is carried in a map key /
/// log label downstream. All four fields participate, so a different tenant,
/// model, api-key or session is a different key — and `api_key_id` participating
/// is what keeps two users of the same tenant/model from sharing a pinned
/// provider.
#[must_use]
pub fn sticky_key(tenant_id: &str, model_key: &str, api_key_id: &str, session: &str) -> String {
    let mut joined = String::new();
    joined.reserve(tenant_id.len() + model_key.len() + api_key_id.len() + session.len() + 3);
    joined.push_str(tenant_id);
    joined.push_str(FIELD_SEP);
    joined.push_str(model_key);
    joined.push_str(FIELD_SEP);
    joined.push_str(api_key_id);
    joined.push_str(FIELD_SEP);
    joined.push_str(session);
    sha256_hex_string(joined.as_bytes())
}

/// The rendezvous unit `u ∈ [0,1)` for one `(key, provider)` pair: the first
/// 8 bytes of `sha256(key || 0x1f || provider_id)` read big-endian, divided by
/// `2^64`.
///
/// `key` is the hex TEXT from [`sticky_key`] here, never raw digest bytes —
/// keeping the same string form on both sides of the pipe is what lets a replica
/// recompute an identical pick.
#[must_use]
fn unit_interval(key: &str, provider_id: &str) -> f64 {
    let mut material = String::new();
    material.reserve(key.len() + provider_id.len() + 1);
    material.push_str(key);
    material.push_str(FIELD_SEP);
    material.push_str(provider_id);

    let digest = sha256_hex(material.as_bytes());
    let mut head: u64 = 0;
    for byte in digest.iter().take(8) {
        head = (head << 8) | (*byte as u64);
    }
    (head as f64) / SCALE_2_POW_64
}

/// Rendezvous score for one candidate: `-ln(u) / weight`, a draw from
/// `Exp(rate = weight)`.
///
/// LOWER wins the race, so `P(i) ∝ weight_i`. The weight enters as a divisor
/// precisely because that shrinks the exponential draw: `u = 0.5` scores `≈0.69`
/// at weight 1 and `≈0.23` at weight 3, so the heavier provider wins more often
/// (~3/4 against a single weight-1 rival), matching `swrr`'s load split.
/// `u == 0` ⇒ `+inf`, which loses to every other selectable candidate.
#[must_use]
fn affinity_score(u: f64, weight: i32) -> f64 {
    if u <= 0.0 {
        return f64::INFINITY;
    }
    -u.ln() / (weight as f64)
}

/// Does `(score, provider_id)` beat the running best of the race?
///
/// MINIMUM score wins; an exact score tie is broken by `provider_id` ASCENDING
/// so the pick cannot depend on the order the candidate list happened to arrive
/// in. The loop pairs this with `best.is_none()` because the first candidate
/// may itself carry the sentinel score `+inf` (`u == 0`), which would otherwise
/// lose to the running best of `+inf` and leave the race undecided.
#[must_use]
fn outranks(score: f64, provider_id: &str, best_id: &str, best_score: f64) -> bool {
    if score < best_score {
        true
    } else if score == best_score {
        provider_id < best_id
    } else {
        false
    }
}

/// Which candidate a session key selects, as its **index** into `candidates`:
/// the MINIMUM `-ln(u) / weight` score, ties by `provider_id` ascending.
///
/// `weight <= 0` candidates are skipped outright (their score would divide by
/// zero or a negative), so a list where every weight is `<= 0` yields `None`.
/// A `u == 0` candidate scores `+inf` and loses to every other selectable
/// candidate. The index is returned, not the provider, so
/// [`order_with_affinity`] can splice it without re-scoring.
#[must_use]
pub fn pick_sticky(candidates: &[Candidate], key: &str) -> Option<usize> {
    let mut best: Option<usize> = None;
    let mut best_score: f64 = f64::INFINITY;
    let mut best_id: &str = "";
    for (i, c) in candidates.iter().enumerate() {
        if c.weight <= 0 {
            continue;
        }
        let id: &str = c.provider_id.as_str();
        let score: f64 = affinity_score(unit_interval(key, id), c.weight);
        if best.is_none() || outranks(score, id, best_id, best_score) {
            best = Some(i);
            best_score = score;
            best_id = id;
        }
    }
    best
}

/// Order `candidates` for one session: the rendezvous pick moves to the FRONT
/// and every other candidate keeps its existing relative order (so the tail stays
/// the `provider_id`-ascending failover line the router produced).
///
/// Returns whether a sticky pick was applied. `key = None` (no session header)
/// or no selectable candidate ⇒ `false` and the list is left exactly as it was,
/// which is the caller's signal to fall through to [`crate::swrr`].
#[must_use]
pub fn order_with_affinity(candidates: &mut Vec<Candidate>, key: Option<&str>) -> bool {
    let picked_index: Option<usize> = match key {
        None => None,
        Some(k) => pick_sticky(candidates.as_slice(), k),
    };
    match picked_index {
        None => false,
        Some(i) => {
            let picked: Candidate = candidates.remove(i);
            candidates.insert(0, picked);
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{affinity_score, outranks};
    use super::{order_with_affinity, pick_sticky, sticky_key};
    use crate::model::Candidate;

    fn cand(id: &str, weight: i32) -> Candidate {
        Candidate {
            provider_id: id.into(),
            endpoint: format!("https://{id}.example.com"),
            weight,
        }
    }

    /// A fresh session id per iteration, all sharing tenant/model/api-key.
    fn key_for(i: i32) -> String {
        sticky_key("t_acme", "gpt-4o", "k_1", format!("session-{i}").as_str())
    }

    /// The provider a key selects, named rather than indexed (the tests below
    /// compare across DIFFERENT candidate lists, where an index is meaningless).
    fn pick_id(list: &[Candidate], key: &str) -> String {
        let index = pick_sticky(list, key).expect("a selectable candidate");
        list[index].provider_id.clone()
    }

    // ---------------------------------------------------------------- key ---

    #[test]
    fn sticky_key_is_stable_hex_and_field_sensitive() {
        let base = sticky_key("t_acme", "gpt-4o", "k_1", "s_1");
        assert_eq!(
            base,
            sticky_key("t_acme", "gpt-4o", "k_1", "s_1"),
            "pure: same inputs, same key"
        );
        assert_eq!(base.len(), 64, "lower-case sha256 hex");
        for b in base.as_bytes() {
            let is_hex = (*b >= 0x30 && *b <= 0x39) || (*b >= 0x61 && *b <= 0x66);
            assert!(is_hex, "expected lower-case hex, got {b}");
        }

        // Every one of the four fields participates.
        assert_ne!(
            base,
            sticky_key("t_other", "gpt-4o", "k_1", "s_1"),
            "tenant participates"
        );
        assert_ne!(
            base,
            sticky_key("t_acme", "gpt-5", "k_1", "s_1"),
            "model participates"
        );
        assert_ne!(
            base,
            sticky_key("t_acme", "gpt-4o", "k_2", "s_1"),
            "api key participates"
        );
        assert_ne!(
            base,
            sticky_key("t_acme", "gpt-4o", "k_1", "s_2"),
            "session participates"
        );

        // The 0x1f separator makes the join unambiguous.
        assert_ne!(
            sticky_key("ab", "c", "k_1", "s_1"),
            sticky_key("a", "bc", "k_1", "s_1"),
            "field boundaries must not be forgeable",
        );
    }

    // ------------------------------------------------------------ pick ------

    #[test]
    fn sticky_pick_is_deterministic_and_order_independent() {
        let ascending = [cand("p_a", 1), cand("p_b", 1), cand("p_c", 1)];
        let descending = [cand("p_c", 1), cand("p_b", 1), cand("p_a", 1)];
        let key = sticky_key("t_acme", "gpt-4o", "k_1", "s_1");

        let first = pick_id(&ascending, key.as_str());
        assert_eq!(
            first,
            pick_id(&ascending, key.as_str()),
            "pure: same inputs, same provider"
        );
        assert_eq!(
            first,
            pick_id(&descending, key.as_str()),
            "no candidate-list-order influence"
        );
    }

    #[test]
    fn sticky_pick_survives_provider_add_and_remove_as_minority() {
        let three = [cand("p_a", 1), cand("p_b", 1), cand("p_c", 1)];
        let four = [
            cand("p_a", 1),
            cand("p_b", 1),
            cand("p_c", 1),
            cand("p_d", 1),
        ];

        let mut unchanged_add = 0usize;
        let mut moved_to_new = 0usize;
        for i in 0..1000 {
            let key = key_for(i);
            let before = pick_id(&three, key.as_str());
            let after = pick_id(&four, key.as_str());
            if before == after {
                unchanged_add += 1;
            }
            if after == "p_d" {
                moved_to_new += 1;
            }
        }
        assert!(
            unchanged_add > 500,
            "adding a provider must keep a MAJORITY pinned: {}",
            unchanged_add
        );
        assert!(
            moved_to_new > 0,
            "the new provider must win SOMETHING: {}",
            moved_to_new
        );
        assert!(
            moved_to_new < 500,
            "a single added provider must not take over: {}",
            moved_to_new
        );

        // Removal is the mirror: sessions that did NOT use the removed provider
        // stay where they were.
        let mut unchanged_drop = 0usize;
        for i in 0..1000 {
            let key = key_for(i);
            let before = pick_id(&four, key.as_str());
            if before != "p_d" {
                let after = pick_id(&three, key.as_str());
                if before == after {
                    unchanged_drop += 1;
                }
            }
        }
        assert!(
            unchanged_drop > 500,
            "removal must keep a MAJORITY of the untouched sessions: {}",
            unchanged_drop
        );
    }

    #[test]
    fn sticky_weight_3_carries_roughly_triple_the_sessions() {
        // The race is `Exp(rate = weight)` and the MINIMUM wins, so the pinned
        // share is proportional to the weight: ~750 of the 1000 keys go to the
        // weight-3 provider, the same split `swrr` gives it on request load.
        let pair = [cand("p_light", 1), cand("p_heavy", 3)];
        let mut heavy = 0usize;
        for i in 0..1000 {
            if pick_id(&pair, key_for(i).as_str()) == "p_heavy" {
                heavy += 1;
            }
        }
        // Bounds deliberately LOOSE (hashing noise over a 1000-key sample, not a
        // distribution proof) yet tight enough to pin the DIRECTION: a race that
        // inverted the weight would land near 250 and fail here.
        assert!(
            heavy >= 700,
            "weight 3 must carry ~3x weight 1: {} of 1000",
            heavy
        );
        assert!(
            heavy <= 800,
            "weight 3 must not monopolise: {} of 1000",
            heavy
        );
        assert!(heavy > 1000 - heavy, "weight 3 out-pins weight 1");
    }

    // ------------------------------------------------------- score / tie -----

    #[test]
    fn sticky_score_handles_zero_u_and_weight_monotonicity() {
        assert_eq!(
            affinity_score(0.0, 1),
            f64::INFINITY,
            "u == 0 is +inf, which LOSES the min race (never a division by zero)",
        );
        assert_eq!(
            affinity_score(1.0, 3),
            0.0,
            "u == 1 scores exactly 0 (the fastest clock)"
        );
        assert!(
            affinity_score(0.9, 1) < affinity_score(0.1, 1),
            "a luckier u scores LOWER, so it wins the min race",
        );
        assert!(
            affinity_score(0.5, 3) < affinity_score(0.5, 1),
            "the same u scores LOWER for a heavier provider: that is how weight buys share",
        );
    }

    #[test]
    fn sticky_tie_break_prefers_the_lower_provider_id() {
        // Exercised on the comparator directly: a real sha256 collision cannot
        // be manufactured, and the tie rule is decided exactly here.
        assert!(
            outranks(1.0, "p_a", "p_b", 1.0),
            "exact tie -> provider_id ascending"
        );
        assert!(
            !outranks(1.0, "p_b", "p_a", 1.0),
            "... and never the other way round"
        );
        assert!(
            outranks(0.5, "p_z", "p_a", 1.0),
            "a LOWER score wins despite the larger id"
        );
        assert!(
            !outranks(2.0, "p_a", "p_b", 1.0),
            "a HIGHER score loses despite the smaller id"
        );

        // The SAME provider listed twice: identical score AND identical id, so
        // the FIRST row wins — nothing can flip-flop between equal rows.
        let dupes = [cand("p_a", 1), cand("p_a", 1)];
        let key = key_for(3);
        assert_eq!(
            pick_sticky(&dupes, key.as_str()),
            Some(0usize),
            "first equal row wins"
        );
        assert_eq!(
            pick_sticky(&dupes, key.as_str()),
            pick_sticky(&dupes, key.as_str()),
            "deterministic on duplicates",
        );
    }

    #[test]
    fn sticky_skips_non_positive_weights() {
        let mixed = [cand("p_zero", 0), cand("p_negative", -5), cand("p_live", 1)];
        assert_eq!(
            pick_sticky(&mixed, key_for(7).as_str()),
            Some(2usize),
            "only weight > 0 can carry a score",
        );

        let dead = [cand("p_zero", 0), cand("p_negative", -1)];
        assert_eq!(
            pick_sticky(&dead, key_for(7).as_str()),
            None,
            "nothing selectable -> None"
        );

        let mut untouched = dead.to_vec();
        assert!(
            !order_with_affinity(&mut untouched, Some(key_for(7).as_str())),
            "no pick -> false"
        );
        assert_eq!(untouched, dead.to_vec(), "and the list is left untouched");
    }

    // ------------------------------------------------------------- order -----

    #[test]
    fn order_moves_the_pick_first_and_keeps_the_tail() {
        let list = [
            cand("p_a", 1),
            cand("p_b", 1),
            cand("p_c", 1),
            cand("p_d", 1),
        ];
        let key = key_for(42);
        let picked = pick_sticky(&list, key.as_str()).expect("a selectable candidate");

        let mut ordered = list.to_vec();
        assert!(
            order_with_affinity(&mut ordered, Some(key.as_str())),
            "a session key yields a sticky pick"
        );
        assert_eq!(
            ordered[0].provider_id, list[picked].provider_id,
            "the pick moves to the FRONT"
        );

        // The tail keeps the relative order the router produced (provider_id
        // ascending), so failover still walks a predictable line.
        let mut expected_tail: Vec<String> = vec![];
        for (i, entry) in list.iter().take(4).enumerate() {
            if i != picked {
                expected_tail.push(entry.provider_id.clone());
            }
        }
        let actual_tail: Vec<String> = ordered[1..].iter().map(|c| c.provider_id.clone()).collect();
        assert_eq!(actual_tail, expected_tail, "tail order is preserved");

        // No session header at all: nothing happens — the caller's signal to
        // fall through to `swrr::order`.
        let mut no_session = list.to_vec();
        assert!(
            !order_with_affinity(&mut no_session, None),
            "key = None is a no-op"
        );
        assert_eq!(no_session, list.to_vec(), "list untouched");

        // Empty candidate list never panics.
        let mut empty: Vec<Candidate> = vec![];
        assert!(
            !order_with_affinity(&mut empty, Some(key.as_str())),
            "empty list is a no-op"
        );
        assert_eq!(empty.len(), 0, "still empty");
    }
}
