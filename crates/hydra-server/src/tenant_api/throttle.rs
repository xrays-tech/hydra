//! A fixed-window request counter for the tenant API.
//!
//! ## What it protects
//!
//! Clearing a cache is not free, and one of its costs is cross-tenant: the auth
//! cache is per node, so "clear this key" is a fan-out, and a stream trim that
//! overtakes a lagging consumer bumps a generation which makes EVERY node drop
//! its WHOLE cache (`cluster::events`). Each clear also sends every affected
//! client back to the tenant's `auth_url`. A single tenant's credential must not
//! be able to spend other tenants' availability, so the endpoint is capped.
//!
//! ## Bound, stated honestly
//!
//! The window lives in this process, so an N-node data-plane fleet allows N times
//! the configured rate. That still bounds each node's publish rate — the
//! quantity that drives the generation bump — and it costs nothing on the request
//! path. A shared (Redis) window would make the fleet-wide limit exact; it is a
//! deliberate follow-up rather than a silent assumption.
//!
//! Shape: one `(window_start, count)` per key, lazily reset. Time is injected so
//! the behaviour is testable without sleeping.

use std::time::{Duration, Instant};

use dashmap::DashMap;

/// A fixed-window counter.
#[derive(Debug, Default)]
pub struct Throttle {
    windows: DashMap<String, (Instant, u32)>,
}

impl Throttle {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Count one request against `key`, returning `false` when the limit is
    /// already reached for the current window.
    ///
    /// A rejected request does NOT consume budget: otherwise a caller that keeps
    /// hammering would extend its own lockout indefinitely, and the reported
    /// `Retry-After` would never converge.
    pub fn allow(&self, key: &str, limit: u32, window: Duration, now: Instant) -> bool {
        let mut entry = self.windows.entry(key.to_string()).or_insert((now, 0));
        let (start, count) = &mut *entry;
        if now.duration_since(*start) >= window {
            *start = now;
            *count = 0;
        }
        if *count >= limit {
            return false;
        }
        *count += 1;
        true
    }

    /// Seconds until the window for `key` resets, for `Retry-After`. `None` when
    /// the key has no window (nothing was ever counted).
    #[must_use]
    pub fn retry_after_secs(&self, key: &str, window: Duration, now: Instant) -> u64 {
        match self.windows.get(key) {
            Some(e) => {
                let (start, _) = *e;
                let elapsed = now.duration_since(start);
                if elapsed >= window {
                    0
                } else {
                    (window - elapsed).as_secs().max(1)
                }
            }
            None => 0,
        }
    }

    /// Drop windows that have expired (housekeeping, so a deleted tenant's key
    /// does not live forever).
    pub fn gc(&self, window: Duration, now: Instant) -> usize {
        let before = self.windows.len();
        self.windows
            .retain(|_, (start, _)| now.duration_since(*start) < window);
        before - self.windows.len()
    }

    /// Number of live windows (metrics / tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.windows.len()
    }

    /// Whether no window is currently tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn allows_up_to_the_limit_then_refuses() {
        let th = Throttle::new();
        let now = t0();
        let w = Duration::from_secs(60);
        for _ in 0..10 {
            assert!(th.allow("t1", 10, w, now));
        }
        assert!(!th.allow("t1", 10, w, now), "the 11th must be refused");
        // A different tenant has its own budget.
        assert!(th.allow("t2", 10, w, now));
    }

    #[test]
    fn a_refusal_does_not_consume_budget() {
        let th = Throttle::new();
        let now = t0();
        let w = Duration::from_secs(60);
        assert!(th.allow("t1", 1, w, now));
        for _ in 0..50 {
            assert!(!th.allow("t1", 1, w, now));
        }
        // The window still resets on schedule, so hammering cannot extend the
        // lockout.
        assert!(th.allow("t1", 1, w, now + w));
    }

    #[test]
    fn the_window_resets_after_its_duration() {
        let th = Throttle::new();
        let now = t0();
        let w = Duration::from_millis(100);
        assert!(th.allow("t1", 1, w, now));
        assert!(!th.allow("t1", 1, w, now + Duration::from_millis(99)));
        assert!(th.allow("t1", 1, w, now + Duration::from_millis(100)));
    }

    #[test]
    fn retry_after_counts_down_and_gc_drops_dead_windows() {
        let th = Throttle::new();
        let now = t0();
        let w = Duration::from_secs(60);
        assert!(th.allow("t1", 1, w, now));
        assert_eq!(th.retry_after_secs("t1", w, now), 60);
        assert_eq!(
            th.retry_after_secs("t1", w, now + Duration::from_secs(30)),
            30
        );
        assert_eq!(th.retry_after_secs("unknown", w, now), 0);
        assert_eq!(th.len(), 1);
        assert_eq!(th.gc(w, now + w), 1);
        assert_eq!(th.len(), 0);
    }
}
