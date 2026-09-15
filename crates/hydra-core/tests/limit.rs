//! T6.1–T6.7 — limit role matching + sliding-window counter (pure).
//!
//! Every time-dependent behaviour is driven by an explicit `now: Instant`,
//! advanced via `Instant + Duration`. There is no hidden `Instant::now()` in
//! the production code under test, so these tests are fully deterministic.

use std::time::{Duration, Instant};

use hydra_core::limit::{match_roles, MatchCtx, SlidingWindow};
use hydra_core::model::LimitRole;
use pretty_assertions::assert_eq;

/// A role with every `matching_*` dimension `None` (match-all). `enabled` is
/// configurable so we can also exercise the enabled gate.
fn role_all_null(id: &str, enabled: bool) -> LimitRole {
    LimitRole {
        id: id.into(),
        name: format!("{id}-name"),
        matching_key: None,
        matching_model: None,
        matching_tenant: None,
        matching_provider: None,
        limit_count: Some(100),
        limit_token: None,
        window: "m".into(),
        enabled,
        created_at: "2026-01-01T00:00:00Z".into(),
    }
}

fn ctx<'a>(
    api_key: Option<&'a str>,
    model: Option<&'a str>,
    tenant: Option<&'a str>,
    provider: Option<&'a str>,
) -> MatchCtx<'a> {
    MatchCtx {
        api_key,
        model,
        tenant,
        provider,
    }
}

// T6.1 — an all-NULL role matches any MatchCtx (including an all-None one).
#[test]
fn limit_match_all_null_matches_everything() {
    let roles = [role_all_null("r1", true)];

    let got = match_roles(
        &roles,
        &ctx(Some("sk-1"), Some("gpt-4o"), Some("t1"), Some("openai")),
    );
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].id, "r1");

    // also matches when nothing is known about the request yet
    let got_none = match_roles(&roles, &ctx(None, None, None, None));
    assert_eq!(got_none.len(), 1);
}

// T6.2 — specified dimensions must equal the ctx value exactly; unspecified
// dimensions act as wildcards. A Some(x) role does NOT match a None ctx value.
#[test]
fn limit_match_specific_dimensions() {
    let mut r = role_all_null("r1", true);
    r.matching_key = Some("sk-1".into());
    r.matching_model = Some("gpt-4o".into());

    // exact on specified dims, wildcard on the rest -> match
    assert_eq!(
        match_roles(&[r.clone()], &ctx(Some("sk-1"), Some("gpt-4o"), None, None)).len(),
        1
    );
    // wildcard dims still match even when ctx carries arbitrary values
    assert_eq!(
        match_roles(
            &[r.clone()],
            &ctx(Some("sk-1"), Some("gpt-4o"), Some("t9"), Some("p9"))
        )
        .len(),
        1
    );
    // wrong api-key -> no match
    assert_eq!(
        match_roles(&[r.clone()], &ctx(Some("sk-2"), Some("gpt-4o"), None, None)).len(),
        0
    );
    // wrong model -> no match
    assert_eq!(
        match_roles(&[r.clone()], &ctx(Some("sk-1"), Some("claude"), None, None)).len(),
        0
    );
    // role requires api-key but ctx has none (unknown != specific) -> no match
    assert_eq!(
        match_roles(&[r], &ctx(None, Some("gpt-4o"), None, None)).len(),
        0
    );
}

// T6.3 — multiple roles may match the same request; all are returned in input
// order. The caller picks the strictest (design §10.1: 叠加生效，取最严).
#[test]
fn limit_match_multiple_overlay() {
    let broad = role_all_null("broad", true); // match-all
    let mut narrow = role_all_null("narrow", true);
    narrow.matching_key = Some("sk-1".into());
    let mut disabled = role_all_null("disabled", false); // enabled=false -> never matches
    disabled.matching_key = Some("sk-1".into());

    let roles = [broad, narrow, disabled];
    let got = match_roles(
        &roles,
        &ctx(Some("sk-1"), Some("gpt-4o"), Some("t1"), Some("openai")),
    );
    assert_eq!(
        got.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        vec!["broad", "narrow"]
    );
}

// T6.4 — within the limit, every check_and_inc admits and enqueues a sample.
#[test]
fn window_count_within_limit() {
    let mut w = SlidingWindow::new(Duration::from_secs(60));
    let t0 = Instant::now();
    assert!(w.check_and_inc(t0, 3));
    assert!(w.check_and_inc(t0 + Duration::from_secs(1), 3));
    assert!(w.check_and_inc(t0 + Duration::from_secs(2), 3));
    assert_eq!(w.count(), 3);
}

// T6.5 — once the live sample count reaches the limit, further calls reject
// (return false) and do NOT enqueue a sample.
#[test]
fn window_count_exceeds() {
    let mut w = SlidingWindow::new(Duration::from_secs(60));
    let t0 = Instant::now();
    assert!(w.check_and_inc(t0, 2));
    assert!(w.check_and_inc(t0 + Duration::from_secs(1), 2));
    // at limit -> rejected, sample not enqueued
    assert!(!w.check_and_inc(t0 + Duration::from_secs(2), 2));
    assert_eq!(w.count(), 2);
}

// T6.6 — advancing `now` past the window evicts stale samples, which then
// re-admits the request (sliding window, not fixed window).
#[test]
fn window_sliding_eviction() {
    let mut w = SlidingWindow::new(Duration::from_secs(60));
    let t0 = Instant::now();
    assert!(w.check_and_inc(t0, 2));
    assert!(w.check_and_inc(t0 + Duration::from_secs(10), 2));
    // full now (2 live samples)
    assert!(!w.check_and_inc(t0 + Duration::from_secs(20), 2));
    assert_eq!(w.count(), 2);

    // advance: t0 (age 61s) is evicted; t0+10 (age 51s) is retained, so the
    // window has room again.
    let t1 = t0 + Duration::from_secs(61);
    assert!(w.check_and_inc(t1, 2));
    assert_eq!(w.count(), 2); // [t0+10, t1]
}

// T6.7 — the token dimension accumulates and evicts by age independently of
// the request-count dimension.
#[test]
fn window_token_dimension() {
    let mut w = SlidingWindow::new(Duration::from_secs(60));
    let t0 = Instant::now();
    w.add(t0, 100);
    w.add(t0 + Duration::from_secs(30), 200);
    // both chunks inside the window
    assert_eq!(w.token_used(t0 + Duration::from_secs(40)), 300);
    // advance so only the first chunk (t0, age 61s) falls out; t0+30 (age 31s)
    // is retained.
    assert_eq!(w.token_used(t0 + Duration::from_secs(61)), 200);

    // token and count dimensions are independent on the same window
    let mut w2 = SlidingWindow::new(Duration::from_secs(60));
    assert!(w2.check_and_inc(t0, 5)); // count: 1 sample enqueued
    w2.add(t0, 100); // tokens: 100 recorded
    assert_eq!(w2.count(), 1);
    assert_eq!(w2.token_used(t0), 100);
}

/// The GC predicate must be able to reclaim a window whose traffic stopped.
///
/// `count()` deliberately does not evict (it is the O(1) read taken under the
/// limiter's entry guard), so `count() > 0` stayed true forever for an idle
/// window and the limiter's GC could never drop it.
#[test]
fn evict_stale_reclaims_an_expired_window() {
    let t0 = Instant::now();
    let mut w = SlidingWindow::new(Duration::from_secs(60));
    assert!(w.check_and_inc(t0, 10), "first request admitted");
    w.add(t0, 500);

    // Still inside the window: nothing to reclaim, and the counts survive.
    assert!(w.evict_stale(t0 + Duration::from_secs(30)));
    assert_eq!(w.count(), 1);

    // Past the window on both dimensions: reclaimable.
    assert!(!w.evict_stale(t0 + Duration::from_secs(61)));
    assert_eq!(w.count(), 0);
    assert_eq!(w.token_used(t0 + Duration::from_secs(61)), 0);
}
