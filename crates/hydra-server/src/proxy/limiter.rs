//! Concurrent rate-limiter shell over the pure [`hydra_core::limit`] matching
//! + sliding-window counter (design §10.2 / §10.3 / wave-4 §1).
//!
//! [`RateLimiter`] owns a `DashMap<LimitKey, SlidingWindow>` keyed by
//! `(role_id, bucket)` (the bucket is a deterministic join of the role's
//! matching dimensions that are known at pre-gate time). A background GC sweep
//! drops empty windows periodically. The pure matching (`match_roles`) and the
//! pure counter (`check_and_inc`/`add`) are called directly — no internal logic
//! is faked here.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use hydra_core::limit::{match_roles, MatchCtx, SlidingWindow};
use hydra_core::model::LimitRole;
use tracing::debug;

/// Rate-limiter abstraction (cluster P4): the in-memory [`RateLimiter`]
/// (single node) and the Redis-backed [`crate::redis::rate_limit::RedisRateLimiter`]
/// (cluster) both implement it, so the proxy's hot path is agnostic. Boxed
/// futures for object safety (same pattern as `UsageSink` / `LeaseStore`).
pub trait Limiter: Send + Sync {
    /// Pre-gate count check (request count): deny when any matched window is
    /// over its limit.
    fn check_count<'a>(
        &'a self,
        roles: &'a [LimitRole],
        ctx: &'a MatchCtx<'a>,
        now: Instant,
    ) -> Pin<Box<dyn Future<Output = CountVerdict> + Send + 'a>>;

    /// Record token usage in the `logging` phase (always counted; overage is
    /// flagged for next time, §10.3).
    fn add_tokens<'a>(
        &'a self,
        roles: &'a [LimitRole],
        ctx: &'a MatchCtx<'a>,
        tokens: u64,
        now: Instant,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    /// Drop empty windows (background GC). Redis-backed windows expire
    /// themselves, so the shared limiter's `gc` is a no-op.
    fn gc(&self);
}

/// One counter slot: `(role_id, bucket)` (design §10.2). `bucket` is the
/// deterministic join of the role's matching dimensions known at pre-gate
/// time (api-key / model / tenant; provider is unknown until routing and is
/// applied in the `logging` phase instead, §10.3).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LimitKey {
    pub role_id: String,
    pub bucket: String,
}

/// Concurrent rate limiter: `DashMap<LimitKey, SlidingWindow>` (design §10.2).
///
/// The window length is derived from each role's `window` field (`m`/`h`/`d`).
/// Pre-gate count admission runs [`check_and_inc`](SlidingWindow::check_and_inc)
/// under a short-lived DashMap shard lock; token accounting runs
/// [`add`](SlidingWindow::add) in the `logging` phase.
pub struct RateLimiter {
    windows: DashMap<LimitKey, SlidingWindow>,
}

impl RateLimiter {
    /// Build an empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self {
            windows: DashMap::new(),
        }
    }

    /// Pre-gate count check (design §10.3): match roles against `ctx`, then for
    /// each matched role with a `limit_count`, admit iff every matching window
    /// is under its limit. Returns `false` (and the matched role that denied)
    /// the moment any window rejects.
    ///
    /// `now` is injected so the shell can drive it from `Instant::now`; the
    /// pure core takes `now` explicitly.
    pub fn check_count(
        &self,
        roles: &[LimitRole],
        ctx: &MatchCtx<'_>,
        now: Instant,
    ) -> CountVerdict {
        let matched = match_roles(roles, ctx);
        for role in &matched {
            if let Some(limit) = role.limit_count {
                let limit_u64 = u64::try_from(limit.max(0)).unwrap_or(0);
                if limit_u64 == 0 {
                    // limit_count == 0 ⇒ deny unconditionally.
                    return CountVerdict::Denied {
                        role_id: role.id.clone(),
                    };
                }
                let key = LimitKey {
                    role_id: role.id.clone(),
                    bucket: bucket_for(role, ctx),
                };
                // check_and_inc under the DashMap entry guard.
                let admitted = self
                    .windows
                    .entry(key)
                    .or_insert_with(|| SlidingWindow::new(window_len(role)))
                    .check_and_inc(now, limit_u64);
                if !admitted {
                    debug!(role = %role.id, limit = limit, "rate limit denied (count)");
                    return CountVerdict::Denied {
                        role_id: role.id.clone(),
                    };
                }
            }
        }
        CountVerdict::Admitted
    }

    /// Record token usage in the `logging` phase (design §10.3: the request is
    /// always counted; overage is flagged for next time).
    pub fn add_tokens(&self, roles: &[LimitRole], ctx: &MatchCtx<'_>, tokens: u64, now: Instant) {
        let matched = match_roles(roles, ctx);
        for role in &matched {
            if role.limit_token.is_some() {
                let key = LimitKey {
                    role_id: role.id.clone(),
                    bucket: bucket_for(role, ctx),
                };
                self.windows
                    .entry(key)
                    .or_insert_with(|| SlidingWindow::new(window_len(role)))
                    .add(now, tokens);
            }
        }
    }

    /// Number of live windows (introspection / tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.windows.len()
    }

    /// Whether the limiter holds no windows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// Drop empty-window entries whose samples have all aged out (design §10.2
    /// GC). Called by a background sweep task.
    pub fn gc(&self) {
        // DashMap retain is sharded; cheap relative to a full scan when the map
        // is small. Windows with no samples and no tokens are removed.
        self.windows.retain(|_, w| w.count() > 0);
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl Limiter for RateLimiter {
    fn check_count<'a>(
        &'a self,
        roles: &'a [LimitRole],
        ctx: &'a MatchCtx<'a>,
        now: Instant,
    ) -> Pin<Box<dyn Future<Output = CountVerdict> + Send + 'a>> {
        Box::pin(async move { RateLimiter::check_count(self, roles, ctx, now) })
    }

    fn add_tokens<'a>(
        &'a self,
        roles: &'a [LimitRole],
        ctx: &'a MatchCtx<'a>,
        tokens: u64,
        now: Instant,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move { RateLimiter::add_tokens(self, roles, ctx, tokens, now) })
    }

    fn gc(&self) {
        RateLimiter::gc(self);
    }
}

/// Outcome of the pre-gate count check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CountVerdict {
    /// Under all matched limits — admit (each window has been incremented).
    Admitted,
    /// Over at least one matched `limit_count` — deny with 429 (§10.3).
    Denied { role_id: String },
}

/// Window length for a role's `window` field (design §10.2: m=60s, h=3600s,
/// d=86400s). Unknown values default to 60s (the shortest, safest window).
fn window_len(role: &LimitRole) -> Duration {
    match role.window.as_str() {
        "m" => Duration::from_secs(60),
        "h" => Duration::from_secs(3600),
        "d" => Duration::from_secs(86_400),
        _ => Duration::from_secs(60),
    }
}

/// Deterministic bucket string for a role's known-at-pre-gate matching
/// dimensions (api-key / model / tenant). The provider dimension is excluded
/// here because it is unknown until routing (§10.3); it is folded in during
/// `logging` instead.
fn bucket_for(role: &LimitRole, ctx: &MatchCtx<'_>) -> String {
    // Only include the dimensions the role actually constrains; this keeps the
    // bucket keyspace tight (one shared window for wildcard roles).
    let mut parts: Vec<&str> = Vec::with_capacity(3);
    if role.matching_key.is_some() {
        parts.push(ctx.api_key.unwrap_or(""));
    }
    if role.matching_model.is_some() {
        parts.push(ctx.model.unwrap_or(""));
    }
    if role.matching_tenant.is_some() {
        parts.push(ctx.tenant.unwrap_or(""));
    }
    parts.join("\x1f") // ASCII unit separator — cannot appear in real values.
}

/// Spawn a background GC sweep that drops empty windows every `interval`.
pub fn spawn_gc_task(limiter: Arc<dyn Limiter>, interval: Duration) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await; // skip the immediate first tick
        loop {
            ticker.tick().await;
            limiter.gc();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(id: &str, count: Option<i64>, window: &str) -> LimitRole {
        LimitRole {
            id: id.to_string(),
            name: id.to_string(),
            matching_key: None,
            matching_model: None,
            matching_tenant: None,
            matching_provider: None,
            limit_count: count,
            limit_token: None,
            window: window.to_string(),
            enabled: true,
            created_at: String::new(),
        }
    }

    #[test]
    fn admits_under_limit() {
        let rl = RateLimiter::new();
        let roles = vec![role("r1", Some(3), "m")];
        let ctx = MatchCtx {
            api_key: None,
            model: None,
            tenant: Some("t1"),
            provider: None,
        };
        let now = Instant::now();
        assert_eq!(rl.check_count(&roles, &ctx, now), CountVerdict::Admitted);
        assert_eq!(rl.check_count(&roles, &ctx, now), CountVerdict::Admitted);
        assert_eq!(rl.check_count(&roles, &ctx, now), CountVerdict::Admitted);
        // 4th within the window → denied.
        assert_eq!(
            rl.check_count(&roles, &ctx, now),
            CountVerdict::Denied {
                role_id: "r1".into()
            }
        );
    }

    #[test]
    fn window_evicts_after_expiry() {
        let rl = RateLimiter::new();
        let roles = vec![role("r1", Some(1), "m")];
        let ctx = MatchCtx {
            api_key: None,
            model: None,
            tenant: Some("t1"),
            provider: None,
        };
        let t0 = Instant::now();
        assert_eq!(rl.check_count(&roles, &ctx, t0), CountVerdict::Admitted);
        assert_eq!(
            rl.check_count(&roles, &ctx, t0),
            CountVerdict::Denied {
                role_id: "r1".into()
            }
        );
        // Advance past the 60s window.
        let t1 = t0 + Duration::from_secs(61);
        assert_eq!(rl.check_count(&roles, &ctx, t1), CountVerdict::Admitted);
    }

    #[test]
    fn zero_count_denies() {
        let rl = RateLimiter::new();
        let roles = vec![role("block", Some(0), "m")];
        let ctx = MatchCtx {
            api_key: None,
            model: None,
            tenant: Some("t1"),
            provider: None,
        };
        assert_eq!(
            rl.check_count(&roles, &ctx, Instant::now()),
            CountVerdict::Denied {
                role_id: "block".into()
            }
        );
    }

    #[test]
    fn disabled_roles_dont_match() {
        let rl = RateLimiter::new();
        let mut r = role("r1", Some(0), "m");
        r.enabled = false;
        let roles = vec![r];
        let ctx = MatchCtx {
            api_key: None,
            model: None,
            tenant: Some("t1"),
            provider: None,
        };
        assert_eq!(
            rl.check_count(&roles, &ctx, Instant::now()),
            CountVerdict::Admitted
        );
    }
}
