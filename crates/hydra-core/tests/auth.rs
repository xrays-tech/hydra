//! T7.1–T7.7 — auth cache hit/expiry decision + upstream→`CacheOp` mapping +
//! `Verdict`→`AuthVerdict` (status-carrying) translation (pure).
//!
//! `cache_decision` takes an explicit `now: Instant` so TTL/expiry is fully
//! deterministic. No `AuthCache`/`DashMap` here — that concurrent wrapper is W3
//! (wave-1 §3.1); these are the pure decision functions it will call.

use std::time::{Duration, Instant};

use hydra_core::auth::{
    apply_upstream, cache_decision, decide, denial_status_for_reason, AuthEntry, AuthVerdict,
    CacheOp, CacheSource, Verdict,
};
use pretty_assertions::assert_eq;

const ALLOW_TTL: Duration = Duration::from_secs(300); // 5 min (design §11.2)
const DENY_TTL: Duration = Duration::from_secs(30);

// T7.1 — cached allow, within TTL -> Hit(true).
#[test]
fn auth_cache_hit_allowed() {
    let now = Instant::now();
    let entry = AuthEntry {
        allowed: true,
        expires_at: now + Duration::from_secs(60),
    };
    assert_eq!(cache_decision(Some(&entry), now), Verdict::Hit(true));
}

// T7.2 — cached deny, within TTL -> Hit(false).
#[test]
fn auth_cache_hit_denied() {
    let now = Instant::now();
    let entry = AuthEntry {
        allowed: false,
        expires_at: now + Duration::from_secs(60),
    };
    assert_eq!(cache_decision(Some(&entry), now), Verdict::Hit(false));
}

// T7.3 — past TTL (or no entry at all) -> Miss.
#[test]
fn auth_cache_expired_is_miss() {
    let now = Instant::now();
    // strictly past expiry -> Miss
    let expired = AuthEntry {
        allowed: true,
        expires_at: now - Duration::from_secs(1),
    };
    assert_eq!(cache_decision(Some(&expired), now), Verdict::Miss);
    // exactly at the expiry boundary -> no longer valid -> Miss
    let boundary = AuthEntry {
        allowed: true,
        expires_at: now,
    };
    assert_eq!(cache_decision(Some(&boundary), now), Verdict::Miss);
    // no entry at all -> Miss
    assert_eq!(cache_decision(None, now), Verdict::Miss);
}

// T7.4 — upstream 2xx -> cache an allow with the allow TTL.
#[test]
fn auth_apply_upstream_2xx_sets_allow() {
    assert_eq!(
        apply_upstream(200, ALLOW_TTL, DENY_TTL),
        CacheOp::Set {
            allowed: true,
            ttl: ALLOW_TTL,
        }
    );
    assert_eq!(
        apply_upstream(204, ALLOW_TTL, DENY_TTL),
        CacheOp::Set {
            allowed: true,
            ttl: ALLOW_TTL,
        }
    );
}

// T7.5 — upstream 401/403 -> cache a deny with the deny TTL.
#[test]
fn auth_apply_upstream_401_sets_deny() {
    assert_eq!(
        apply_upstream(401, ALLOW_TTL, DENY_TTL),
        CacheOp::Set {
            allowed: false,
            ttl: DENY_TTL,
        }
    );
    assert_eq!(
        apply_upstream(403, ALLOW_TTL, DENY_TTL),
        CacheOp::Set {
            allowed: false,
            ttl: DENY_TTL,
        }
    );
}

// T7.6 — upstream 5xx / other -> do NOT cache (fail-mode is the shell's call).
#[test]
fn auth_apply_upstream_5xx_no_cache() {
    assert_eq!(apply_upstream(500, ALLOW_TTL, DENY_TTL), CacheOp::None);
    assert_eq!(apply_upstream(503, ALLOW_TTL, DENY_TTL), CacheOp::None);
    // other 4xx (e.g. 429) are also not cached
    assert_eq!(apply_upstream(429, ALLOW_TTL, DENY_TTL), CacheOp::None);
}

// T7.7 — decide() lifts the low-level Verdict into the status-carrying
// AuthVerdict; Denied carries the caller-supplied status verbatim (401 vs 503
// are distinct), so request_filter can write the response without re-deriving.
#[test]
fn auth_verdict_carries_status() {
    // denied cache hit carries the exact status the shell chooses
    let d401 = decide(Verdict::Hit(false), 401, "unauthorized");
    assert_eq!(
        d401,
        AuthVerdict::Denied {
            status: 401,
            reason: "unauthorized",
            source: CacheSource::Hit,
        }
    );

    let d503 = decide(Verdict::Hit(false), 503, "upstream_unavailable");
    assert_eq!(
        d503,
        AuthVerdict::Denied {
            status: 503,
            reason: "upstream_unavailable",
            source: CacheSource::Hit,
        }
    );

    // allowed cache hit -> Allowed{Hit}
    assert_eq!(
        decide(Verdict::Hit(true), 401, "x"),
        AuthVerdict::Allowed {
            source: CacheSource::Hit,
        }
    );
    // a Miss handed to decide denotes a freshly-resolved (upstream) allow
    assert_eq!(
        decide(Verdict::Miss, 401, "x"),
        AuthVerdict::Allowed {
            source: CacheSource::Miss,
        }
    );
}

// T7.8 — denial_status_for_reason: an insufficient_balance reason (trimmed,
// case-insensitive) maps to 402 Payment Required; anything unknown / missing
// stays 401 (legacy: unclassified denials look like an invalid key).
#[test]
fn auth_denial_reason_insufficient_balance_is_402() {
    assert_eq!(denial_status_for_reason(Some("insufficient_balance")), 402);
    assert_eq!(denial_status_for_reason(Some("Insufficient_Balance")), 402);
    assert_eq!(
        denial_status_for_reason(Some("  INSUFFICIENT_BALANCE  ")),
        402
    );
}

#[test]
fn auth_denial_reason_unknown_or_missing_is_401() {
    assert_eq!(denial_status_for_reason(Some("invalid_key")), 401);
    assert_eq!(denial_status_for_reason(Some("internal_error")), 401);
    assert_eq!(denial_status_for_reason(Some("")), 401);
    assert_eq!(denial_status_for_reason(Some("   ")), 401);
    assert_eq!(denial_status_for_reason(None), 401);
}
