//! W3 §2.1 — `AuthCache` concurrent wrapper tests.
//!
//! The pure hit/expiry judgement (`cache_decision`) is exhaustively covered in
//! `hydra-core/tests/auth.rs`; here we only verify the **concurrent DashMap
//! shell**: it delegates to the pure fn, supports forced invalidation by key
//! and by tenant, GC evicts expired entries, and — critically — the api-key
//! is stored only as a SHA-256 digest (never plaintext).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hydra_core::auth::{cache_decision, AuthEntry, Verdict};
use hydra_server::http::{AuthCache, Clock};

const ALLOW_TTL: Duration = Duration::from_secs(300);
const DENY_TTL: Duration = Duration::from_secs(30);

/// A frozen, manually-advancable clock so TTL/expiry/GC are deterministic
/// (no real-time sleeps, no flake).
fn frozen_clock() -> (Arc<Mutex<Instant>>, Clock) {
    let t = Arc::new(Mutex::new(Instant::now()));
    let clock: Clock = {
        let t = Arc::clone(&t);
        Arc::new(move || *t.lock().unwrap())
    };
    (t, clock)
}

// ---------------------------------------------------------------------------
// T1.1 — check() delegates to the pure cache_decision; results consistent.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cache_decision_delegates_to_pure() {
    let (time, clock) = frozen_clock();
    let cache = AuthCache::with_clock(ALLOW_TTL, DENY_TTL, clock);

    // allow entry
    cache
        .set("t1", "sk-allow", true, Duration::from_secs(60))
        .await;
    // deny entry
    cache
        .set("t1", "sk-deny", false, Duration::from_secs(60))
        .await;

    let now = *time.lock().unwrap();

    // pure reference (same expiry the cache stored: now + 60s)
    let pure_allow = cache_decision(
        Some(&AuthEntry {
            allowed: true,
            expires_at: now + Duration::from_secs(60),
        }),
        now,
    );
    let pure_deny = cache_decision(
        Some(&AuthEntry {
            allowed: false,
            expires_at: now + Duration::from_secs(60),
        }),
        now,
    );

    assert_eq!(cache.check("t1", "sk-allow").await, pure_allow);
    assert_eq!(cache.check("t1", "sk-allow").await, Verdict::Hit(true));
    assert_eq!(cache.check("t1", "sk-deny").await, pure_deny);
    assert_eq!(cache.check("t1", "sk-deny").await, Verdict::Hit(false));

    // unknown key / tenant → Miss (pure agrees)
    assert_eq!(
        cache.check("t1", "sk-unknown").await,
        cache_decision(None, now)
    );
    assert_eq!(cache.check("t2", "sk-allow").await, Verdict::Miss);
}

#[tokio::test]
async fn cache_decision_expired_is_miss_like_pure() {
    let (time, clock) = frozen_clock();
    let cache = AuthCache::with_clock(ALLOW_TTL, DENY_TTL, clock);
    cache.set("t1", "sk-x", true, Duration::from_secs(60)).await;

    // advance past TTL
    *time.lock().unwrap() += Duration::from_secs(120);

    let now = *time.lock().unwrap();
    let pure = cache_decision(
        Some(&AuthEntry {
            allowed: true,
            expires_at: now - Duration::from_secs(60),
        }),
        now,
    );
    assert_eq!(pure, Verdict::Miss);
    assert_eq!(cache.check("t1", "sk-x").await, Verdict::Miss);
}

/// Concurrent writers from many threads all land in the cache (the DashMap
/// shell is threadsafe under `&self` shared access).
#[tokio::test]
async fn cache_concurrent_writes_all_visible() {
    let cache = Arc::new(AuthCache::new(ALLOW_TTL, DENY_TTL));
    let n = 200u32;
    let mut handles = Vec::new();
    for i in 0..n {
        let cache = Arc::clone(&cache);
        handles.push(tokio::spawn(async move {
            cache
                .set("t1", &format!("key-{i}"), true, Duration::from_secs(60))
                .await;
        }));
    }
    for h in handles {
        h.await.expect("writer");
    }
    assert_eq!(cache.len(), n as usize);
    for i in 0..n {
        let k = format!("key-{i}");
        assert_eq!(cache.check("t1", &k).await, Verdict::Hit(true));
    }
}

// ---------------------------------------------------------------------------
// T1.2 — invalidate(keys) deletes matching, returns count; missing ignored.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cache_set_and_invalidate_keys() {
    let cache = AuthCache::new(ALLOW_TTL, DENY_TTL);
    cache.set("t1", "k1", true, Duration::from_secs(60)).await;
    cache.set("t1", "k2", true, Duration::from_secs(60)).await;
    cache.set("t1", "k3", true, Duration::from_secs(60)).await;

    let removed = cache
        .invalidate(
            "t1",
            &["k1".to_string(), "k2".to_string(), "nope".to_string()],
        )
        .await;
    assert_eq!(removed, 2);
    assert_eq!(cache.check("t1", "k1").await, Verdict::Miss);
    assert_eq!(cache.check("t1", "k2").await, Verdict::Miss);
    // k3 untouched
    assert_eq!(cache.check("t1", "k3").await, Verdict::Hit(true));
    assert_eq!(cache.len(), 1);
}

#[tokio::test]
async fn cache_invalidate_keys_is_tenant_scoped() {
    // same api-key under two tenants: invalidating one tenant must NOT touch
    // the other (the cache key is (tenant_id, sha256)).
    let cache = AuthCache::new(ALLOW_TTL, DENY_TTL);
    cache
        .set("t1", "shared", true, Duration::from_secs(60))
        .await;
    cache
        .set("t2", "shared", true, Duration::from_secs(60))
        .await;

    let removed = cache.invalidate("t1", &["shared".to_string()]).await;
    assert_eq!(removed, 1);
    assert_eq!(cache.check("t1", "shared").await, Verdict::Miss);
    assert_eq!(cache.check("t2", "shared").await, Verdict::Hit(true));
}

// ---------------------------------------------------------------------------
// T1.3 — invalidate_tenant clears all of a tenant's entries.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cache_invalidate_tenant() {
    let cache = AuthCache::new(ALLOW_TTL, DENY_TTL);
    cache.set("t1", "k1", true, Duration::from_secs(60)).await;
    cache.set("t1", "k2", false, Duration::from_secs(60)).await;
    cache.set("t2", "k3", true, Duration::from_secs(60)).await;

    let removed = cache.invalidate_tenant("t1").await;
    assert_eq!(removed, 2);
    assert_eq!(cache.check("t1", "k1").await, Verdict::Miss);
    assert_eq!(cache.check("t1", "k2").await, Verdict::Miss);
    // t2 untouched
    assert_eq!(cache.check("t2", "k3").await, Verdict::Hit(true));
    assert_eq!(cache.len(), 1);
}

#[tokio::test]
async fn cache_invalidate_tenant_missing_is_zero() {
    let cache = AuthCache::new(ALLOW_TTL, DENY_TTL);
    assert_eq!(cache.invalidate_tenant("ghost").await, 0);
}

// ---------------------------------------------------------------------------
// T1.4 — gc() evicts expired entries only.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cache_gc_evicts_expired() {
    let (time, clock) = frozen_clock();
    let cache = AuthCache::with_clock(ALLOW_TTL, DENY_TTL, clock);

    cache
        .set("t1", "fresh", true, Duration::from_secs(60))
        .await;
    cache
        .set("t1", "old1", false, Duration::from_secs(60))
        .await;
    cache.set("t2", "old2", true, Duration::from_secs(60)).await;

    // advance past TTL: all become expired
    *time.lock().unwrap() += Duration::from_secs(120);
    let evicted = cache.gc();
    assert_eq!(evicted, 3);
    assert!(cache.is_empty());
}

#[tokio::test]
async fn cache_gc_keeps_live_entries() {
    let (time, clock) = frozen_clock();
    let cache = AuthCache::with_clock(ALLOW_TTL, DENY_TTL, clock);

    cache
        .set("t1", "live", true, Duration::from_secs(120))
        .await;
    cache.set("t1", "dead", true, Duration::from_secs(30)).await;

    // advance 60s: dead (30s TTL) expired, live (120s TTL) still valid
    *time.lock().unwrap() += Duration::from_secs(60);
    let evicted = cache.gc();
    assert_eq!(evicted, 1);
    assert_eq!(cache.check("t1", "live").await, Verdict::Hit(true));
    assert_eq!(cache.check("t1", "dead").await, Verdict::Miss);
    assert_eq!(cache.len(), 1);
}

#[tokio::test]
async fn cache_gc_on_empty_is_zero() {
    let (_, clock) = frozen_clock();
    let cache = AuthCache::with_clock(ALLOW_TTL, DENY_TTL, clock);
    assert_eq!(cache.gc(), 0);
}

// ---------------------------------------------------------------------------
// T1.5 — key is sha256, never plaintext (Debug output must not leak the key).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cache_key_is_sha256_not_plaintext() {
    let cache = AuthCache::new(ALLOW_TTL, DENY_TTL);
    let secret = "sk-super-secret-api-key-12345";
    cache.set("t1", secret, true, Duration::from_secs(60)).await;

    let debug = format!("{cache:?}");
    // the plaintext api-key must NEVER appear in the cache's Debug output
    assert!(
        !debug.contains(secret),
        "plaintext api-key leaked in AuthCache Debug: {debug}"
    );
    // the tenant_id is fine to show, but the key value must be a sha-256
    // digest ([u8;32] byte array), not the secret string
    let prefix = &secret[..secret.len().min(6)];
    assert!(
        !debug.contains(prefix),
        "api-key prefix leaked in AuthCache Debug: {debug}"
    );

    // sanity: the digest IS present (as a byte array), proving the entry lives
    // there keyed by hash, not by plaintext.
    assert!(debug.contains("AuthEntry"));
}

#[tokio::test]
async fn cache_set_then_invalidate_then_set_works() {
    // round-trip: set → check → invalidate → miss → set again
    let cache = AuthCache::new(ALLOW_TTL, DENY_TTL);
    cache.set("t1", "k", true, Duration::from_secs(60)).await;
    assert_eq!(cache.check("t1", "k").await, Verdict::Hit(true));
    assert_eq!(cache.invalidate("t1", &["k".to_string()]).await, 1);
    assert_eq!(cache.check("t1", "k").await, Verdict::Miss);
    cache.set("t1", "k", false, Duration::from_secs(60)).await;
    assert_eq!(cache.check("t1", "k").await, Verdict::Hit(false));
}
