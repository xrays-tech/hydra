//! Access-limit matching + sliding-window counter (pure).
//!
//! `MatchCtx` (the borrowed matching context) is the foundation type; the pure
//! [`match_roles`] selector and the [`SlidingWindow`] counter (both driven by
//! an explicit `now: Instant`) are the Limit lane's T6.x deliverables. The
//! concurrent `DashMap<LimitKey, SlidingWindow>` wrapper and its GC task live
//! in `hydra-server` (wave-1 §3.1 / design §10.2) — everything here is pure
//! state with no hidden time.
//!
//! `MatchCtx` borrows request attributes (api-key / model / tenant / provider)
//! with no allocation; `provider` is `None` until routing selects one, so
//! provider-dimension limits are checked in the `logging` phase (design §10.3).

use crate::model::LimitRole;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Borrowed per-request context used to match `LimitRole`s. All fields
/// optional; `None` means "not yet known" (not "wildcard" — wildcards are
/// expressed as `None` on the *role* side).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatchCtx<'a> {
    pub api_key: Option<&'a str>,
    pub model: Option<&'a str>,
    pub tenant: Option<&'a str>,
    pub provider: Option<&'a str>,
}

/// Match every enabled `LimitRole` against `ctx`.
///
/// A role matches when **every** non-`None` `matching_*` field equals the
/// corresponding `ctx` value; a `None` field is match-all (design §10.1: "为
/// NULL 或 等于"). A role value of `Some(x)` does **not** match a `ctx`
/// dimension that is `None` — "unknown" is never equal to a specific value.
/// Disabled roles (`enabled == false`) never match (design §10.1: only
/// `enabled=1` participates).
///
/// Multiple matches are returned in input order; the caller applies the
/// strictest (design §10.1: 多个匹配项叠加生效，取最严). Borrows the roles —
/// the only allocation is the returned `Vec`.
pub fn match_roles<'a>(roles: &'a [LimitRole], ctx: &MatchCtx) -> Vec<&'a LimitRole> {
    roles
        .iter()
        .filter(|r| {
            r.enabled
                && dim_matches(r.matching_key.as_deref(), ctx.api_key)
                && dim_matches(r.matching_model.as_deref(), ctx.model)
                && dim_matches(r.matching_tenant.as_deref(), ctx.tenant)
                && dim_matches(r.matching_provider.as_deref(), ctx.provider)
        })
        .collect()
}

/// `None` (match-all) or exact equality with the ctx value.
fn dim_matches(role_dim: Option<&str>, ctx_val: Option<&str>) -> bool {
    match role_dim {
        None => true,
        Some(wanted) => ctx_val == Some(wanted),
    }
}

/// Pure sliding-window counter for one `(role, bucket)` limit key.
///
/// Two independent dimensions share the same `window` length:
/// - **request count** — one `Instant` sample per admitted request, evicted by
///   age via [`check_and_inc`](Self::check_and_inc);
/// - **token sum** — `(Instant, tokens)` chunks evicted by age via
///   [`add`](Self::add), read with [`token_used`](Self::token_used).
///
/// Both dimensions are driven by an explicitly-injected `now: Instant` (there
/// is no hidden `Instant::now()`), so tests are deterministic. Per design
/// §10.2 the in-memory counter is keyed `LimitKey = (role_id, bucket)`; that
/// `DashMap` + periodic GC is assembled in `hydra-server` (W4) — this struct is
/// the pure state machine the shell wraps.
#[derive(Debug)]
pub struct SlidingWindow {
    /// Window length (design §10.2: m=60s, h=3600s, d=86400s).
    window: Duration,
    /// Request-count samples (one `Instant` per admitted request), kept
    /// time-sorted so stale ones are evicted from the front.
    samples: VecDeque<Instant>,
    /// Token-dimension chunks: `(admitted_at, tokens)`, time-sorted.
    token_samples: VecDeque<(Instant, u64)>,
    /// Cached running sum of `token_samples`, maintained incrementally so
    /// [`token_used`](Self::token_used) is O(1) (cheap evictions + reads).
    token_sum: u64,
}

impl SlidingWindow {
    /// New empty window of length `window`.
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            samples: VecDeque::new(),
            token_samples: VecDeque::new(),
            token_sum: 0,
        }
    }

    /// Drop count-samples whose age is `>= window` (i.e. `sample <= now -
    /// window`). Samples are pushed in non-decreasing time order, so the deque
    /// stays time-sorted and we evict strictly from the front.
    fn evict_samples(&mut self, now: Instant) {
        // `checked_sub`: if `now` somehow predates the window length, nothing
        // can be old enough to evict (and we never underflow `Instant`).
        let Some(cutoff) = now.checked_sub(self.window) else {
            return;
        };
        while self.samples.front().is_some_and(|&t| t <= cutoff) {
            self.samples.pop_front();
        }
    }

    /// Drop token chunks whose age is `>= window`, keeping `token_sum` in sync.
    fn evict_tokens(&mut self, now: Instant) {
        let Some(cutoff) = now.checked_sub(self.window) else {
            return;
        };
        while let Some(&(t, tokens)) = self.token_samples.front() {
            if t <= cutoff {
                self.token_sum = self.token_sum.saturating_sub(tokens);
                self.token_samples.pop_front();
            } else {
                break;
            }
        }
    }

    /// **Count dimension**: evict stale samples, then if fewer than `limit`
    /// live samples remain, record `now` and return `true` (admit); otherwise
    /// return `false` (over the count limit — design §10.3: pre-gate → 429).
    /// A rejected request is **not** enqueued.
    pub fn check_and_inc(&mut self, now: Instant, limit: u64) -> bool {
        self.evict_samples(now);
        if (self.samples.len() as u64) < limit {
            self.samples.push_back(now);
            true
        } else {
            false
        }
    }

    /// Live request-count after the last [`check_and_inc`](Self::check_and_inc)
    /// (eviction only happens there, so this is a cheap O(1) read of current
    /// state).
    pub fn count(&self) -> usize {
        self.samples.len()
    }

    /// **Token dimension**: record `tokens` consumed at `now`, evicting stale
    /// chunks first. Called in the `logging` phase once usage is known (design
    /// §10.3: the request is always counted; overage is flagged for next time).
    pub fn add(&mut self, now: Instant, tokens: u64) {
        self.evict_tokens(now);
        self.token_samples.push_back((now, tokens));
        self.token_sum = self.token_sum.saturating_add(tokens);
    }

    /// **Token dimension**: evict stale chunks and return the live token sum —
    /// the value the next request is checked against (`sum <= limit_token`).
    pub fn token_used(&mut self, now: Instant) -> u64 {
        self.evict_tokens(now);
        self.token_sum
    }

    /// Evict everything outside the window and report whether anything is left.
    ///
    /// Used by the limiter's background GC. Eviction otherwise only happens as
    /// a side effect of `check_and_inc`/`add`/`token_used`, so a window whose
    /// traffic stopped kept its expired samples — and a non-zero `count()` —
    /// forever, and the GC predicate could never drop it.
    pub fn evict_stale(&mut self, now: Instant) -> bool {
        self.evict_samples(now);
        self.evict_tokens(now);
        !self.samples.is_empty() || !self.token_samples.is_empty()
    }
}
