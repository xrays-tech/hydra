//! Admission control for the tenant API: a success rate limit, a failure rate
//! limit and a lockout.
//!
//! ## Why the data-plane listener needs this and the admin one did not
//!
//! `POST /api/v1/tenants/{id}/auth/cache/invalidate` used to sit on the
//! management port, which binds `127.0.0.1` by default and is behind a token an
//! operator chose. The data plane is internet-facing and its credential is a
//! tenant's own access token, so two things that were merely theoretical become
//! live (design §5.1):
//!
//! - **token guessing**: an unauthenticated caller can drive the gate as fast as
//!   it can open connections. The gate itself is now zero-I/O (it reads the
//!   config snapshot), so the cost per attempt is small — but "small" times
//!   "unbounded" is still an amplifier, and a guess that succeeds is a
//!   cross-tenant compromise.
//! - **resource amplification**: each successful `invalidate` fans out to every
//!   node and sends every affected client back to the tenant's `auth_url`. One
//!   tenant's credential must not be able to spend other tenants' availability.
//!
//! ## Three dimensions, two budgets
//!
//! | dimension | budget | why this dimension |
//! |---|---|---|
//! | source IP | failures | one host guessing token after token is the cheap attack |
//! | token digest | failures | one token being probed from many hosts is the other |
//! | tenant | successes | the amplification budget — this is what caps the fan-out |
//!
//! Failure limits **never** consume the success budget and vice versa, so a busy
//! legitimate tenant cannot be throttled by an attacker's failures on the same
//! IP (and an attacker cannot lock a tenant out by burning its success budget —
//! only the failure dimension for the digest they are guessing, which stops
//! mattering the moment they stop).
//!
//! ## Bound, stated honestly
//!
//! Every window lives in this process: an N-node fleet allows N times the
//! configured rate. It bounds each node, which is the quantity the amplification
//! actually depends on, and it costs nothing on the request path. A shared
//! (Redis) window would make the fleet-wide limit exact and is a deliberate
//! follow-up, not a silent assumption.

use std::time::{Duration, Instant};

use dashmap::DashMap;

use super::throttle::Throttle;

/// Why a request was refused, and what the caller should do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refused {
    /// The dimension that refused: `ip` | `token` | `tenant`.
    pub scope: &'static str,
    /// Seconds until the caller may retry.
    pub retry_after: u64,
}

/// The tenant API's limiters and lockout state.
#[derive(Debug, Default)]
pub struct TenantApiLimiter {
    /// Successful requests, per tenant.
    success: Throttle,
    /// Failed authentications, per source IP.
    fail_ip: Throttle,
    /// Failed authentications, per presented token digest.
    fail_token: Throttle,
    /// Locked dimensions (`ip:<addr>` / `tok:<digest>`) → locked until.
    locks: DashMap<String, Instant>,
}

impl TenantApiLimiter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Refuse before the gate even runs when this caller is locked out.
    ///
    /// Checked FIRST on purpose: a locked dimension must not be allowed to keep
    /// spending work (the whole point of a lockout, as opposed to a rate limit,
    /// is that it is cheap to enforce).
    #[must_use]
    pub fn locked(&self, ip: &str, token_digest: Option<&str>, now: Instant) -> Option<Refused> {
        for (scope, key) in self.dimensions(ip, token_digest) {
            if let Some(until) = self.locks.get(&key) {
                if *until > now {
                    return Some(Refused {
                        scope,
                        retry_after: (*until - now).as_secs().max(1),
                    });
                }
            }
        }
        None
    }

    /// Account one *successful* authentication and enforce the per-tenant budget.
    ///
    /// Counted AFTER the token is verified, so an unauthenticated caller can
    /// never consume a tenant's budget.
    pub fn check_success(
        &self,
        tenant_id: &str,
        limit: u32,
        window: Duration,
        now: Instant,
    ) -> Result<(), Refused> {
        let key = format!("tenant:{tenant_id}");
        if self.success.allow(&key, limit, window, now) {
            return Ok(());
        }
        Err(Refused {
            scope: "tenant",
            retry_after: self.success.retry_after_secs(&key, window, now).max(1),
        })
    }

    /// Account one *failed* authentication on both failure dimensions, and lock a
    /// dimension once it goes over its budget.
    ///
    /// Returns the refusal for the CURRENT request when the budget was already
    /// spent, so the caller answers `429` instead of `401` — a locked-out caller
    /// must not be able to tell whether the token it guessed exists.
    pub fn record_failure(
        &self,
        ip: &str,
        token_digest: Option<&str>,
        limit: u32,
        window: Duration,
        lockout: Duration,
        now: Instant,
    ) -> Option<Refused> {
        let mut refused = None;
        for (scope, key) in self.dimensions(ip, token_digest) {
            if !self.fail_ip_or_token(scope, &key, limit, window, now) {
                // Over budget: lock this dimension and report it.
                self.locks.insert(key.clone(), now + lockout);
                refused = Some(Refused {
                    scope,
                    retry_after: lockout.as_secs().max(1),
                });
            }
        }
        refused
    }

    /// The failure counter for a dimension. Both dimensions share this so the
    /// two budgets cannot diverge in behaviour.
    fn fail_ip_or_token(
        &self,
        scope: &'static str,
        key: &str,
        limit: u32,
        window: Duration,
        now: Instant,
    ) -> bool {
        let counter = if scope == "ip" {
            &self.fail_ip
        } else {
            &self.fail_token
        };
        counter.allow(&format!("{scope}:{key}"), limit, window, now)
    }

    /// The dimensions a request is accounted against: always the source IP, plus
    /// the presented token's digest when there is one.
    ///
    /// The digest, never the token: a limiter keyed by the plaintext would put
    /// the credential in a `DashMap` key, a metric label and any heap dump.
    fn dimensions(&self, ip: &str, token_digest: Option<&str>) -> Vec<(&'static str, String)> {
        let mut out = vec![("ip", format!("ip:{ip}"))];
        if let Some(d) = token_digest {
            out.push(("token", format!("tok:{d}")));
        }
        out
    }

    /// Drop expired windows and lockouts (housekeeping).
    pub fn gc(&self, window: Duration, now: Instant) -> usize {
        let dropped = self.success.gc(window, now)
            + self.fail_ip.gc(window, now)
            + self.fail_token.gc(window, now);
        let before = self.locks.len();
        self.locks.retain(|_, until| *until > now);
        dropped + (before - self.locks.len())
    }

    /// Whether no window and no lockout is tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.success.is_empty()
            && self.fail_ip.is_empty()
            && self.fail_token.is_empty()
            && self.locks.is_empty()
    }

    /// Number of live lockouts (tests).
    #[must_use]
    pub fn locked_len(&self) -> usize {
        self.locks.len()
    }
}

/// SHA-256 hex of a presented token, for use as a limiter key. Owned here so the
/// limiter never sees a plaintext token (the same shared helper the L1/L2 auth
/// cache keys use).
#[must_use]
pub fn token_digest(token: &str) -> String {
    hydra_core::auth::sha256_hex_string(token.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    const W: Duration = Duration::from_secs(60);
    const LOCK: Duration = Duration::from_secs(900);

    #[test]
    fn a_tenant_may_spend_its_success_budget_then_is_refused() {
        let l = TenantApiLimiter::new();
        let now = t0();
        for _ in 0..3 {
            assert!(l.check_success("t1", 3, W, now).is_ok());
        }
        let refused = l.check_success("t1", 3, W, now).expect_err("4th refused");
        assert_eq!(refused.scope, "tenant");
        assert!(refused.retry_after > 0);
        // Another tenant has its own budget.
        assert!(l.check_success("t2", 3, W, now).is_ok());
    }

    #[test]
    fn the_success_budget_recovers_after_the_window() {
        let l = TenantApiLimiter::new();
        let now = t0();
        for _ in 0..2 {
            let _ = l.check_success("t1", 2, W, now);
        }
        assert!(l.check_success("t1", 2, W, now).is_err());
        assert!(l.check_success("t1", 2, W, now + W).is_ok());
    }

    #[test]
    fn failures_are_bounded_per_ip_and_per_token_digest() {
        let l = TenantApiLimiter::new();
        let now = t0();
        let d = token_digest("sk-guess");
        // The budget of 2 is spent by the first two failures...
        assert_eq!(l.record_failure("1.2.3.4", Some(&d), 2, W, LOCK, now), None);
        assert_eq!(l.record_failure("1.2.3.4", Some(&d), 2, W, LOCK, now), None);
        // ...and the THIRD is refused. (A limit of N must admit N, not N-1.)
        let third = l
            .record_failure("1.2.3.4", Some(&d), 2, W, LOCK, now)
            .expect("the 3rd failure is refused");
        assert!(third.scope == "ip" || third.scope == "token");
        assert!(l.locked("1.2.3.4", Some(&d), now).is_some());
    }

    #[test]
    fn a_locked_dimension_is_refused_before_the_gate_runs_and_expires() {
        let l = TenantApiLimiter::new();
        let now = t0();
        let d = token_digest("sk-guess");
        for _ in 0..3 {
            let _ = l.record_failure("1.2.3.4", Some(&d), 1, W, LOCK, now);
        }
        let r = l.locked("1.2.3.4", Some(&d), now).expect("locked");
        assert_eq!(r.retry_after, 900);
        // A different IP is unaffected.
        assert!(l.locked("5.6.7.8", None, now).is_none());
        // ...and the lockout expires.
        assert!(l.locked("1.2.3.4", Some(&d), now + LOCK).is_none());
    }

    /// The two budgets are independent: hammering a tenant with bad tokens must
    /// not consume the budget its real clients depend on.
    #[test]
    fn failure_budget_and_success_budget_do_not_share_a_counter() {
        let l = TenantApiLimiter::new();
        let now = t0();
        let d = token_digest("sk-bad");
        for _ in 0..100 {
            let _ = l.record_failure("1.2.3.4", Some(&d), 1, W, LOCK, now);
        }
        assert!(
            l.check_success("t1", 5, W, now).is_ok(),
            "successes must not be spent by failures"
        );
    }

    /// A failure with no token at all still has to be counted: otherwise
    /// "no Authorization header" is an unmetered probing channel.
    #[test]
    fn a_failure_without_a_token_is_still_bounded_per_ip() {
        let l = TenantApiLimiter::new();
        let now = t0();
        assert_eq!(l.record_failure("9.9.9.9", None, 1, W, LOCK, now), None);
        assert!(l.record_failure("9.9.9.9", None, 1, W, LOCK, now).is_some());
    }

    #[test]
    fn gc_drops_dead_windows_and_expired_lockouts() {
        let l = TenantApiLimiter::new();
        let now = t0();
        let _ = l.check_success("t1", 1, W, now);
        // Two failures with a budget of 1: the second one trips the lockout.
        let _ = l.record_failure("1.2.3.4", None, 1, W, LOCK, now);
        let _ = l.record_failure("1.2.3.4", None, 1, W, LOCK, now);
        assert_eq!(l.locked_len(), 1);
        // Sweeping past the WINDOW drops the counters — but NOT the lockout,
        // which is the whole point of it being longer than the window: a caller
        // that waits out its 60 s budget is still locked for the full 900 s.
        assert!(l.gc(W, now + W) >= 2);
        assert_eq!(l.locked_len(), 1, "the lockout outlives the window");
        assert!(!l.is_empty());
        // Past the lockout, everything is gone.
        assert_eq!(l.gc(W, now + LOCK + Duration::from_secs(1)), 1);
        assert!(l.is_empty());
    }

    #[test]
    fn limiter_keys_never_contain_the_plaintext_token() {
        let l = TenantApiLimiter::new();
        let now = t0();
        let secret = "sk-super-secret-token-value";
        let d = token_digest(secret);
        let _ = l.record_failure("1.2.3.4", Some(&d), 1, W, LOCK, now);
        assert!(!d.contains(secret), "the digest must not carry the token");
        assert_eq!(d.len(), 64, "sha256 hex");
    }
}
