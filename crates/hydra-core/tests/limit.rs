//! T6.1–T6.7 — limit role matching + sliding-window counter (pure).
//!
//! Every time-dependent behaviour is driven by an explicit `now: Instant`,
//! advanced via `Instant + Duration`. There is no hidden `Instant::now()` in
//! the production code under test, so these tests are fully deterministic.

use std::time::{Duration, Instant};

use hydra_core::limit::{
    bucket_key, key_bucket_id, key_digest, match_roles, MatchCtx, SlidingWindow,
};
use hydra_core::model::LimitRole;
use hydra_core::rewrite::mask_key;
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
        api_key_raw: None,
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

/// `matching_key` accepts the RAW client key **or** its mask.
///
/// Measured 2026-09-30 (`integration/test_replica_fidelity.py`): the context used to carry only
/// `mask_key(<presented key>)`, so a role written with the raw key — the form `design.md` §10.1
/// describes ("NULL **or equal to** the client api-key") and the form an operator copies out of
/// their inventory — **never fired** (four requests, all 200, on a leader and an edge alike).
/// Matching now takes either form; the bucket is still derived from the masked value, so no window
/// is split and no Redis key name starts carrying raw client keys.
#[test]
fn matching_key_accepts_the_raw_key_or_its_mask() {
    const RAW: &str = "sk-raw-form-1234567890";
    // The mask is DERIVED from the production function, never hand-copied: the first version of this
    // test hard-coded `sk-raw-for********67890` (23 chars, 5 trailing characters kept) while the real
    // rule for a 22-char key keeps 10 ahead / 4 behind (`sk-raw-for********7890`) — a fixture that
    // asserted against a string the product would never produce, so it would have kept passing after
    // any change to the mask rule. `assert_ne!` keeps the fixture discriminating: a mask equal to the
    // raw key would make the "either form" leg a tautology.
    let mask = mask_key(RAW);
    assert_ne!(
        mask, RAW,
        "the fixture must use a real mask, not the key itself"
    );
    let mask: &str = &mask;
    let mut role = role_all_null("r-key", true);
    role.matching_key = Some(RAW.into());

    let ctx_raw = MatchCtx {
        api_key: Some(mask),
        api_key_raw: Some(RAW),
        model: None,
        tenant: None,
        provider: None,
    };
    assert_eq!(
        match_roles(&[role.clone()], &ctx_raw).len(),
        1,
        "raw role must match"
    );

    // The masked form keeps working (nothing that already worked may stop working).
    let mut masked_role = role_all_null("r-mask", true);
    masked_role.matching_key = Some(mask.into());
    assert_eq!(
        match_roles(&[masked_role.clone()], &ctx_raw).len(),
        1,
        "masked role must match"
    );

    // A different key matches neither role, and a context with no raw key still matches the mask.
    let ctx_other = MatchCtx {
        api_key: Some("sk-other******************9999"),
        api_key_raw: Some("sk-other-key-99999999999999"),
        ..ctx_raw
    };
    assert!(match_roles(&[role.clone()], &ctx_other).is_empty());
    assert!(match_roles(&[masked_role], &ctx_other).is_empty());
    let ctx_mask_only = MatchCtx {
        api_key_raw: None,
        ..ctx_raw
    };
    assert_eq!(
        match_roles(&[role], &ctx_mask_only).len(),
        0,
        "without the raw key a raw-form role cannot match (a context must supply it)"
    );
}

/// `Debug` must not print the raw client key.
///
/// The context now carries a live credential (`api_key_raw`), so a derived `Debug` would put
/// customer keys one `debug!(?ctx)` away from the log — and into panic messages. The manual impl
/// redacts that field; this test is the thing that notices if someone puts `Debug` back into the
/// `derive` list (a change that would otherwise be invisible: nothing in the tree logs the context
/// *today*, which is exactly why a silent regression is possible).
#[test]
fn debug_never_prints_the_raw_client_key() {
    const RAW: &str = "sk-raw-form-1234567890";
    let mask = mask_key(RAW);
    let ctx = MatchCtx {
        api_key: Some(&mask),
        api_key_raw: Some(RAW),
        model: Some("m1"),
        tenant: Some("t1"),
        provider: None,
    };
    let rendered = format!("{ctx:?}");
    assert!(
        !rendered.contains(RAW),
        "the raw client key leaked into Debug output: {rendered}"
    );
    // The `api_key` field is documented as the MASKED form; a caller that violates that contract
    // must not turn this into a leak. The round-118 version of this test used a real mask and so
    // could not see the misuse (reviewer finding F4).
    let misused = MatchCtx {
        api_key: Some(RAW),
        api_key_raw: Some(RAW),
        model: None,
        tenant: None,
        provider: None,
    };
    let misused_rendered = format!("{misused:?}");
    assert!(
        !misused_rendered.contains(RAW),
        "a raw key in `api_key` was printed verbatim: {misused_rendered}"
    );
    // An ALL-`*` string is a fixed point of `mask_key` at any length, so "is a fixed point" alone let
    // a credential that looks like `******` print verbatim (product-review P3-3). The redaction now
    // also requires a non-`*` character.
    let all_stars = MatchCtx {
        api_key: Some("******"),
        api_key_raw: Some("******"),
        model: None,
        tenant: None,
        provider: None,
    };
    let stars_rendered = format!("{all_stars:?}");
    assert!(
        !stars_rendered.contains("******"),
        "an all-star credential was printed verbatim: {stars_rendered}"
    );
    assert!(
        misused_rendered.contains("<redacted: api_key is not a mask_key value>"),
        "the misuse is redacted silently (the output must say why): {misused_rendered}"
    );
    assert!(
        rendered.contains("<redacted>"),
        "the redaction is silent (debug output no longer says the field was withheld): {rendered}"
    );
    // The remaining dimensions must stay printable, otherwise the redaction cost us the diagnostics
    // Debug exists for.
    assert!(
        rendered.contains("m1") && rendered.contains("t1"),
        "model/tenant must remain visible in Debug output: {rendered}"
    );
}

/// The KEY-dimension bucket must keep its per-client identity even when the caller supplies only
/// the raw key — and must never contain the raw key itself.
///
/// Reviewer finding F3 / measured 2026-09-30: `bucket_for` built the key component as
/// `ctx.api_key.unwrap_or("")`, so a raw-only context (a shape `MatchCtx` permits and
/// `api_key_raw`'s doc comment describes) made EVERY client of a key-scoped role share one window:
/// the second client would be refused by the first client's spent window, silently — no log, no
/// metric, and no test (the limiter's seven fixtures all set both fields to `None`, so the key
/// branch had zero coverage).
#[test]
fn the_key_bucket_keeps_its_identity_from_the_raw_key_too() {
    let raw_a = "sk-bucket-probe-aaaaaaaa";
    let raw_b = "sk-bucket-probe-bbbbbbbb";
    let only_raw_a = MatchCtx {
        api_key: None,
        api_key_raw: Some(raw_a),
        model: None,
        tenant: None,
        provider: None,
    };
    let only_raw_b = MatchCtx {
        api_key_raw: Some(raw_b),
        ..only_raw_a
    };
    let a = bucket_key(&only_raw_a);
    let b = bucket_key(&only_raw_b);
    assert_eq!(
        a,
        key_bucket_id(raw_a),
        "a raw-only context is bucketed by a DIGEST of the key (D-15②)"
    );
    assert_ne!(
        a, "",
        "an empty bucket component means one window for every client"
    );
    assert!(
        !a.contains(raw_a),
        "the raw key must never appear in a bucket: {a}"
    );
    assert_ne!(a, b, "two clients must not share a bucket");

    // The production shape carries BOTH forms; the RAW key decides the bucket, so the mask cannot
    // make two clients collide.
    let masked = MatchCtx {
        api_key: Some("sk-bucket********aaaa"),
        api_key_raw: Some(raw_a),
        ..only_raw_a
    };
    assert_eq!(bucket_key(&masked), key_bucket_id(raw_a));
    // ...and with neither key present the component is empty, i.e. the old behaviour is preserved
    // for key-less contexts (only reachable for roles whose `matching_key` is NULL).
    let neither = MatchCtx {
        api_key: None,
        api_key_raw: None,
        ..only_raw_a
    };
    assert_eq!(bucket_key(&neither), "");
}

/// Decision D-15② (2026-10-08): two DIFFERENT client keys whose masks are identical must get two
/// windows.
///
/// Measured before the fix: `sk-fidelity-limited` and `skzzzzzzzzzzzzzzzed` produce the SAME mask
/// (10 leading + 4 trailing characters are what `mask_key` keeps), and the window was keyed by that
/// mask — so once the first key's window was spent, the second key was refused on its very FIRST
/// request. One customer eating another's quota, with no log line and no metric.
#[test]
fn two_keys_that_share_a_mask_get_two_buckets() {
    const A: &str = "sk-fidelity-limited";
    const B: &str = "skzzzzzzzzzzzzzzzed";
    let (mask_a, mask_b) = (mask_key(A), mask_key(B));
    assert_eq!(
        mask_a, mask_b,
        "the premise of this test: two different keys, one mask"
    );
    let c_a = MatchCtx {
        api_key: Some(&mask_a),
        api_key_raw: Some(A),
        model: None,
        tenant: None,
        provider: None,
    };
    let c_b = MatchCtx {
        api_key: Some(&mask_b),
        api_key_raw: Some(B),
        model: None,
        tenant: None,
        provider: None,
    };
    assert_ne!(
        bucket_key(&c_a),
        bucket_key(&c_b),
        "same mask, different keys ⇒ DIFFERENT windows (the shared-quota bug)"
    );
    assert_eq!(bucket_key(&c_a), key_bucket_id(A));
    assert!(
        !bucket_key(&c_a).contains(mask_a.as_str()),
        "the mask must not appear in a bucket name — it becomes a Redis key in cluster mode: {}",
        bucket_key(&c_a)
    );
}

/// Decision D-16③ (2026-10-08): a role may state `matching_key` as `sha256:<hex>`, so the limit can be
/// configured WITHOUT the credential being stored anywhere — `limit_role.matching_key` is a plaintext
/// column replicated to every node. The raw and masked forms keep working.
#[test]
fn matching_key_accepts_a_digest_without_storing_the_key() {
    const RAW: &str = "sk-digest-form-0001";
    const OTHER: &str = "sk-digest-form-0002";
    let mask = mask_key(RAW);
    let mut role = role_all_null("r-digest", true);
    role.matching_key = Some(key_digest(RAW));
    assert!(
        !role
            .matching_key
            .as_deref()
            .unwrap_or_default()
            .contains(RAW),
        "the configured value must not contain the key itself"
    );

    let presented = MatchCtx {
        api_key: Some(&mask),
        api_key_raw: Some(RAW),
        model: None,
        tenant: None,
        provider: None,
    };
    assert_eq!(
        match_roles(&[role.clone()], &presented).len(),
        1,
        "the key that was digested must match"
    );

    let other = MatchCtx {
        api_key: Some(&mask_key(OTHER)),
        api_key_raw: Some(OTHER),
        model: None,
        tenant: None,
        provider: None,
    };
    assert!(
        match_roles(&[role.clone()], &other).is_empty(),
        "a DIFFERENT key must not match a digest role"
    );

    // The historical forms are untouched.
    let mut raw_form = role_all_null("r-raw", true);
    raw_form.matching_key = Some(RAW.into());
    assert_eq!(match_roles(&[raw_form], &presented).len(), 1);
    let mut mask_form = role_all_null("r-mask", true);
    mask_form.matching_key = Some(mask.clone());
    assert_eq!(match_roles(&[mask_form], &presented).len(), 1);
}
