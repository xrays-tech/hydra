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
/// futures for object safety (same pattern as `UsageSink`).
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

    /// Pre-gate TOKEN check: deny when any matched role's live token window has
    /// already reached `limit_token`.
    ///
    /// `limit_token` used to be write-only — `add_tokens` recorded the usage
    /// and NOTHING ever read it back, so a token quota was documented and
    /// configurable but never enforced on any request. Unlike the count gate,
    /// this is advisory-by-one-request (the window is only known after the
    /// response), which is exactly the design's next-request semantics — with
    /// the precision that the charge lands in the `logging` phase, so the
    /// refusal applies to requests arriving AFTER it, not to a follow-up that
    /// overtakes the write.
    fn check_tokens<'a>(
        &'a self,
        roles: &'a [LimitRole],
        ctx: &'a MatchCtx<'a>,
        now: Instant,
    ) -> Pin<Box<dyn Future<Output = CountVerdict> + Send + 'a>>;

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
    /// each matched role with a `limit_count`, admit iff EVERY matching window is
    /// under its limit. Returns `false` (and the matched role that denied)
    ///     the moment any window rejects.
    ///
    /// **Two-phase** (P3-7 + final review N3, 2026-10-09): phase 1 checks every
    /// matched window READ-ONLY ([`SlidingWindow::check`]) so a request denied by
    /// ANY role charges NO quota (the old per-role `check_and_inc` loop consumed
    /// the roles before the refusing one and this was the silent over-charge);
    /// only if ALL of them admit does phase 2 charge each window. Phase 2 uses
    /// the ATOMIC [`SlidingWindow::check_and_inc`] (never a bare append): adding
    /// under the same DashMap entry lock as the re-check keeps every window
    /// exactly bounded, so a single-role gate stays precise (phase-1 is a
    /// fast path; the bound comes from phase-2) and a multi-role gate is only
    /// as wide as the phase-1→phase-2 gap.
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

        // Phase 1 — read-only: every matched role must admit; the first refusal
        // denies WITHOUT charging any window.
        for role in &matched {
            let Some(limit) = role.limit_count else {
                continue;
            };
            let limit_u64 = u64::try_from(limit.max(0)).unwrap_or(0);
            if limit_u64 == 0 {
                // limit_count == 0 ⇒ deny unconditionally (no window involved, so
                // there is no window remainder to promise).
                return CountVerdict::Denied {
                    role_id: role.id.clone(),
                    retry_after: None,
                };
            }
            let key = LimitKey {
                role_id: role.id.clone(),
                bucket: bucket_for(role, ctx),
            };
            // Read-only: a check must NOT create a window merely by being checked
            // (that would grow the map per request for roles that never admit a
            // sample). A missing window admits (0 live < limit).
            let admitted = match self.windows.get_mut(&key) {
                Some(mut window) => window.check(now, limit_u64),
                None => true,
            };
            if !admitted {
                debug!(role = %role.id, limit = limit_u64, "rate limit denied (count)");
                let retry_after = self
                    .windows
                    .get(&key)
                    .and_then(|w| w.value().retry_after(now));
                return CountVerdict::Denied {
                    role_id: role.id.clone(),
                    retry_after,
                };
            }
        }

        // Phase 2 — write, ATOMICALLY: every role admitted in phase 1. Each window
        // is charged under its own DashMap entry lock with the check-and-increment
        // in ONE step, so no request ever pushes a window past its limit (the
        // period between phase 1 and phase 2 is a fast path, not a bound). In the
        // rare concurrent case where phase-2's re-check finds a window already
        // full, deny (the earlier windows that did admit are charged, exactly as
        // any single-role gate would be).
        for role in &matched {
            let Some(limit) = role.limit_count else {
                continue;
            };
            let limit_u64 = u64::try_from(limit.max(0)).unwrap_or(0);
            if limit_u64 == 0 {
                continue; // unreachable (phase 1 denied) — defensive
            }
            let key = LimitKey {
                role_id: role.id.clone(),
                bucket: bucket_for(role, ctx),
            };
            let admitted = self
                .windows
                .entry(key.clone())
                .or_insert_with(|| SlidingWindow::new(window_len(role)))
                .check_and_inc(now, limit_u64);
            if !admitted {
                debug!(role = %role.id, limit = limit_u64, "rate limit denied at the atomic re-check (count)");
                let retry_after = self
                    .windows
                    .get(&key)
                    .and_then(|w| w.value().retry_after(now));
                return CountVerdict::Denied {
                    role_id: role.id.clone(),
                    retry_after,
                };
            }
        }
        CountVerdict::Admitted
    }

    /// Pre-gate token check: deny when a matched role's token window already
    /// holds `limit_token` or more.
    ///
    /// Read-only: a role that never recorded tokens must NOT have a window
    /// created merely by being checked (that would grow the map per request).
    pub fn check_tokens(
        &self,
        roles: &[LimitRole],
        ctx: &MatchCtx<'_>,
        now: Instant,
    ) -> CountVerdict {
        for role in match_roles(roles, ctx) {
            let Some(limit) = role.limit_token else {
                continue;
            };
            let limit = u64::try_from(limit.max(0)).unwrap_or(0);
            let key = LimitKey {
                role_id: role.id.clone(),
                bucket: bucket_for(role, ctx),
            };
            let used = match self.windows.get_mut(&key) {
                Some(mut window) => window.token_used(now),
                None => 0,
            };
            // `limit_token == 0` means deny unconditionally, mirroring
            // `limit_count == 0`.
            if limit == 0 || used >= limit {
                debug!(role = %role.id, used, limit, "token quota exhausted");
                let retry_after = self.windows.get(&key).and_then(|w| w.retry_after(now));
                return CountVerdict::Denied {
                    role_id: role.id.clone(),
                    retry_after,
                };
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
        // Evicting BEFORE deciding is what makes this reclaim anything:
        // `count()` deliberately does not evict (it is the O(1) read used under
        // the entry guard), so a window whose traffic stopped kept its expired
        // samples — and therefore a non-zero count — forever, and the retain
        // never dropped it.
        let now = Instant::now();
        // DashMap retain is sharded; cheap relative to a full scan when the map
        // is small. Windows with no live samples and no live tokens are removed.
        self.windows.retain(|_, w| w.evict_stale(now));
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

    fn check_tokens<'a>(
        &'a self,
        roles: &'a [LimitRole],
        ctx: &'a MatchCtx<'a>,
        now: Instant,
    ) -> Pin<Box<dyn Future<Output = CountVerdict> + Send + 'a>> {
        Box::pin(async move { RateLimiter::check_tokens(self, roles, ctx, now) })
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
    /// Over at least one matched `limit_count`/`limit_token` — deny with 429 (§10.3).
    ///
    /// `retry_after` is how long the DENYING window needs to start draining; the shell
    /// puts it in `Retry-After`, which `ops.md` §4.2 promises on exactly this 429
    /// ("reflecting the remainder of the current window"). `None` only when the window
    /// is already empty (nothing to wait for).
    Denied {
        role_id: String,
        retry_after: Option<Duration>,
    },
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
    let mut parts: Vec<String> = Vec::with_capacity(3);
    if role.matching_key.is_some() {
        // `hydra_core::limit::bucket_key` (single owner): the masked key, or the raw key masked
        // here — never `""`, which would give every client of this role one shared window.
        parts.push(hydra_core::limit::bucket_key(ctx));
    }
    if role.matching_model.is_some() {
        parts.push(ctx.model.unwrap_or("").to_string());
    }
    if role.matching_tenant.is_some() {
        parts.push(ctx.tenant.unwrap_or("").to_string());
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

    /// The key dimension must actually separate CLIENTS.
    ///
    /// Reviewer finding F5: all seven fixtures here built key-LESS contexts, so `bucket_for`'s
    /// `matching_key.is_some()` branch had no coverage at all — which is how the F3 defect (a
    /// raw-only context collapsing the bucket to `""`, i.e. one shared window for every client of
    /// the role) stayed invisible. Two clients, one key-scoped role, limit 1: each must get its own
    /// window.
    #[test]
    fn a_key_scoped_role_keeps_one_window_per_client() {
        let rl = RateLimiter::new();
        let mut scoped = role("r-key", Some(1), "m");
        scoped.matching_key = Some("sk-client-one-aaaaaaaa".to_string());
        let roles = vec![scoped];
        let client_one = MatchCtx {
            api_key: None,
            api_key_raw: Some("sk-client-one-aaaaaaaa"),
            model: None,
            tenant: Some("t1"),
            provider: None,
        };
        let client_two = MatchCtx {
            api_key_raw: Some("sk-client-two-bbbbbbbb"),
            ..client_one
        };
        let now = Instant::now();
        // The role matches only client one (exact equality on the raw form).
        assert!(matches!(
            rl.check_count(&roles, &client_one, now),
            CountVerdict::Admitted
        ));
        assert!(matches!(
            rl.check_count(&roles, &client_one, now),
            CountVerdict::Denied { .. }
        ));
        // ...and the OTHER client is untouched: it matched no role, so it is admitted even though
        // the role's window is spent. Before `bucket_key` this path was unreachable in production
        // only because every caller passed a mask; the invariant is now asserted.
        assert!(matches!(
            rl.check_count(&roles, &client_two, now),
            CountVerdict::Admitted
        ));
    }

    /// A raw-only context for the SAME client is bucketed by its mask, so the window is the same
    /// one a masked context would use — no split, and no shared/empty bucket.
    #[test]
    fn a_raw_only_context_shares_the_window_with_the_masked_form() {
        let rl = RateLimiter::new();
        let mut scoped = role("r-both", Some(1), "m");
        scoped.matching_key = Some("sk-raw-form-9999999999".to_string());
        let roles = vec![scoped];
        let raw_only = MatchCtx {
            api_key: None,
            api_key_raw: Some("sk-raw-form-9999999999"),
            model: None,
            tenant: Some("t1"),
            provider: None,
        };
        let masked = MatchCtx {
            api_key: Some(&hydra_core::rewrite::mask_key("sk-raw-form-9999999999")),
            ..raw_only
        };
        let now = Instant::now();
        assert!(matches!(
            rl.check_count(&roles, &raw_only, now),
            CountVerdict::Admitted
        ));
        assert!(
            matches!(
                rl.check_count(&roles, &masked, now),
                CountVerdict::Denied { .. }
            ),
            "the masked form must land in the same window as the raw form"
        );
    }

    #[test]
    fn admits_under_limit() {
        let rl = RateLimiter::new();
        let roles = vec![role("r1", Some(3), "m")];
        let ctx = MatchCtx {
            api_key: None,
            api_key_raw: None, // this fixture exercises key-less roles, so no raw key is needed
            model: None,
            tenant: Some("t1"),
            provider: None,
        };
        let now = Instant::now();
        assert_eq!(rl.check_count(&roles, &ctx, now), CountVerdict::Admitted);
        assert_eq!(rl.check_count(&roles, &ctx, now), CountVerdict::Admitted);
        assert_eq!(rl.check_count(&roles, &ctx, now), CountVerdict::Admitted);
        // 4th within the window → denied.
        // `retry_after` is the remainder of the denying window (~60s here): asserting it
        // is what turns "the header exists" into "it carries the right number".
        match rl.check_count(&roles, &ctx, now) {
            CountVerdict::Denied {
                role_id,
                retry_after,
            } => {
                assert_eq!(role_id, "r1");
                let wait =
                    retry_after.expect("the denying window has samples, so it has a remainder");
                assert!(
                    wait > Duration::from_secs(59) && wait <= Duration::from_secs(60),
                    "expected the remainder of a 60s window, got {wait:?}"
                );
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn window_evicts_after_expiry() {
        let rl = RateLimiter::new();
        let roles = vec![role("r1", Some(1), "m")];
        let ctx = MatchCtx {
            api_key: None,
            api_key_raw: None, // this fixture exercises key-less roles, so no raw key is needed
            model: None,
            tenant: Some("t1"),
            provider: None,
        };
        let t0 = Instant::now();
        assert_eq!(rl.check_count(&roles, &ctx, t0), CountVerdict::Admitted);
        match rl.check_count(&roles, &ctx, t0) {
            CountVerdict::Denied {
                role_id,
                retry_after,
            } => {
                assert_eq!(role_id, "r1");
                assert!(
                    retry_after.is_some(),
                    "a count denial from a non-empty window must report the remainder"
                );
            }
            other => panic!("expected Denied, got {other:?}"),
        }
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
            api_key_raw: None, // this fixture exercises key-less roles, so no raw key is needed
            model: None,
            tenant: Some("t1"),
            provider: None,
        };
        assert_eq!(
            rl.check_count(&roles, &ctx, Instant::now()),
            CountVerdict::Denied {
                role_id: "block".into(),
                // `limit_count = 0` denies unconditionally: no window is involved, so
                // there is no window remainder to promise (the shell then sends 1s).
                retry_after: None,
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
            api_key_raw: None, // this fixture exercises key-less roles, so no raw key is needed
            model: None,
            tenant: Some("t1"),
            provider: None,
        };
        assert_eq!(
            rl.check_count(&roles, &ctx, Instant::now()),
            CountVerdict::Admitted
        );
    }

    /// P3-7 (2026-10-09) — a request denied by a LATER role must not have consumed
    /// quota on the roles that came BEFORE it.
    ///
    /// Two roles match the same context: `r-wide` (limit 3, matches first) and
    /// `r-narrow` (limit 1, matches after). The old limit loop used `check_and_inc`
    /// per role and returned at the first denial — so the SECOND request (refused by
    /// `r-narrow`, now full) still incremented `r-wide`'s window: a denied request
    /// consumed quota on the wider role. The two-phase gate checks every role
    /// read-only first and only increments after all of them admit.
    ///
    /// Falsification: revert `check_count` to the single `check_and_inc` loop and
    /// `r-wide`'s window counts 2 instead of 1 at the end (the refused request was
    /// charged to it).
    #[test]
    fn a_request_denied_by_a_later_role_charges_no_earlier_role() {
        let rl = RateLimiter::new();
        let mut wide = role("r-wide", Some(3), "m");
        wide.matching_model = Some("m1".into()); // distinct bucket from r-narrow
        let mut narrow = role("r-narrow", Some(1), "m");
        narrow.matching_model = Some("m1".into());
        let roles = vec![wide, narrow];
        let ctx = MatchCtx {
            api_key: None,
            api_key_raw: None,
            model: Some("m1"),
            tenant: Some("t1"),
            provider: None,
        };
        let now = Instant::now();

        // Request 1: both roles admit (wide 1/3, narrow 1/1).
        assert_eq!(rl.check_count(&roles, &ctx, now), CountVerdict::Admitted);
        // Request 2: wide still admits (2/3) but narrow is full (1/1) → denied.
        let v2 = rl.check_count(&roles, &ctx, now);
        assert!(
            matches!(&v2, CountVerdict::Denied { role_id, .. } if role_id == "r-narrow"),
            "the second request must be denied by the narrow role (it is full): {v2:?}"
        );
        // The denied request must NOT have charged r-wide: its window holds exactly
        // ONE sample (only request 1, which admitted). Before the two-phase fix this
        // was 2 — the refused request was counted on r-wide too.
        let wide_key = LimitKey {
            role_id: "r-wide".into(),
            bucket: bucket_for(
                &LimitRole {
                    id: "r-wide".into(),
                    name: String::new(),
                    matching_key: None,
                    matching_model: Some("m1".into()),
                    matching_tenant: None,
                    matching_provider: None,
                    limit_count: Some(3),
                    limit_token: None,
                    window: "m".into(),
                    enabled: true,
                    created_at: String::new(),
                },
                &ctx,
            ),
        };
        let wide_samples = rl.windows.get(&wide_key).map(|w| w.count()).unwrap_or(0);
        assert_eq!(
            wide_samples, 1,
            "r-wide must hold exactly 1 sample: request 1 admitted it, request 2 was \
             REFUSED by r-narrow and must not have charged r-wide"
        );
    }

    /// REGRESSION — `limit_token` must actually be enforced.
    ///
    /// `add_tokens` recorded usage in the logging phase and NOTHING ever read
    /// it back: `token_used()` had no production caller anywhere, so a role
    /// configured with only a token quota was documented, persisted, shown in
    /// the admin UI — and enforced on nothing. Every request sailed through.
    #[test]
    fn token_quota_is_enforced_once_the_window_is_full() {
        let limiter = RateLimiter::new();
        let mut r = role("r-tok", None, "m");
        r.limit_token = Some(100);
        let roles = vec![r];
        let ctx = MatchCtx {
            api_key: None,
            api_key_raw: None, // this fixture exercises key-less roles, so no raw key is needed
            model: None,
            tenant: None,
            provider: None,
        };
        let t0 = Instant::now();

        // Nothing recorded yet → admitted.
        assert_eq!(
            limiter.check_tokens(&roles, &ctx, t0),
            CountVerdict::Admitted
        );

        // 80 of 100 used → still admitted.
        limiter.add_tokens(&roles, &ctx, 80, t0);
        assert_eq!(
            limiter.check_tokens(&roles, &ctx, t0),
            CountVerdict::Admitted
        );

        // 80 + 30 = 110 > 100 → the NEXT request is rejected.
        limiter.add_tokens(&roles, &ctx, 30, t0);
        assert_eq!(
            limiter.check_tokens(&roles, &ctx, t0),
            CountVerdict::Denied {
                role_id: "r-tok".to_string(),
                retry_after: Some(Duration::from_secs(60)),
            },
            "a token quota must reject once the window is over it"
        );

        // Once the window has rolled over, the quota frees up again.
        assert_eq!(
            limiter.check_tokens(&roles, &ctx, t0 + Duration::from_secs(61)),
            CountVerdict::Admitted,
            "the sliding window must release the quota"
        );
    }

    /// A role with only a COUNT limit must not be judged by tokens, and a
    /// role with only a TOKEN limit must not be judged by count (the cluster
    /// limiter mapped a NULL count to 0, the deny-everything sentinel).
    #[test]
    fn count_and_token_dimensions_are_independent() {
        let limiter = RateLimiter::new();
        let mut count_only = role("r-count", Some(5), "m");
        count_only.limit_token = None;
        let roles = vec![count_only];
        let ctx = MatchCtx {
            api_key: None,
            api_key_raw: None, // this fixture exercises key-less roles, so no raw key is needed
            model: None,
            tenant: None,
            provider: None,
        };
        let t0 = Instant::now();
        limiter.add_tokens(&roles, &ctx, 10_000, t0);
        assert_eq!(
            limiter.check_tokens(&roles, &ctx, t0),
            CountVerdict::Admitted,
            "no limit_token ⇒ the token dimension does not apply"
        );
    }
}
