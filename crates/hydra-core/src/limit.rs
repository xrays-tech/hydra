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
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MatchCtx<'a> {
    /// The **masked** client key (`mask_key`). Used for MATCHING *and* for the bucket, so the
    /// window's identity (and, in cluster mode, the Redis key name) never contains the raw key.
    pub api_key: Option<&'a str>,
    /// The RAW client key as presented, used for MATCHING only.
    ///
    /// Why it exists (measured 2026-09-30, `integration/test_replica_fidelity.py`): this context was
    /// built with `api_key: mask_key(<presented key>)` alone, so a role whose `matching_key` was the
    /// RAW key — the form `design.md` §10.1 describes ("NULL **or equal to** the client api-key")
    /// and the form an operator copies out of their own inventory — **never fired**: four requests,
    /// all `200`, on both a leader and an edge. Only the mask matched, which is not documented
    /// anywhere and has no documented way to be obtained.
    ///
    /// Matching now accepts **either** form, which is deliberately the non-breaking direction: every
    /// configuration that worked before (the masked form) still works, and the documented form starts
    /// working. The bucket is unchanged (still `api_key`, i.e. the masked value), so no window is
    /// split and no Redis key starts carrying raw client keys — and the pre-existing consequence that
    /// **two distinct keys with the same mask share one window** remains open as plan item D-15.
    pub api_key_raw: Option<&'a str>,
    pub model: Option<&'a str>,
    pub tenant: Option<&'a str>,
    pub provider: Option<&'a str>,
}

/// Hand-written so the RAW client key can never leave the process through a
/// `{:?}` — a derived `Debug` on a struct that holds a live credential is one
/// `debug!(?ctx)` away from writing customer keys into the log (and from
/// appearing in a panic message).
///
/// `api_key` is printed as-is because its contract is "the **masked** key"
/// (`mask_key`), which is already what the usage rows and metrics carry; every
/// caller in the request path passes `mask_key(<presented key>)` there. A
/// caller that violated that contract would be printing its own secret, which
/// is why the field's own doc comment states the contract.
impl std::fmt::Debug for MatchCtx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `api_key` is printed only when it really is the MASKED form — `mask_key`'s fixed point.
        // Relying on "callers pass a mask" was caller discipline, not an invariant: a future call
        // site handing the raw key to `api_key` (the field docs say masked, and `api_key_raw` exists
        // precisely because callers hold raw keys) would print a live credential through one
        // `debug!(?ctx)` or panic message, and the round-118 test could not see it because its
        // fixture used a real mask.
        let api_key: Option<&str> = self.api_key.map(|k| {
            // A genuine mask keeps a non-`*` prefix/suffix; "is a fixed point of `mask_key`" alone was
            // not enough, because an ALL-`*` string is a fixed point of any length (measured) — so a
            // credential that happens to look like `******` printed verbatim, contradicting the
            // guarantee this impl exists for. A mask of a very short key (< 6) is all stars as well
            // and is now redacted too: that costs a diagnostic, not safety, and the direction of the
            // error is the safe one.
            let looks_masked = crate::rewrite::mask_key(k) == k && k.chars().any(|c| c != '*');
            if looks_masked {
                k
            } else {
                "<redacted: api_key is not a mask_key value>"
            }
        });
        f.debug_struct("MatchCtx")
            .field("api_key", &api_key)
            .field("api_key_raw", &self.api_key_raw.map(|_| "<redacted>"))
            .field("model", &self.model)
            .field("tenant", &self.tenant)
            .field("provider", &self.provider)
            .finish()
    }
}

/// The canonical configuration form for a client key: `sha256:<64 lowercase hex>`.
///
/// Decision D-16③ (2026-10-08): a role may state its `matching_key` as this digest instead of the
/// customer's key (or its mask), so the configuration stops being a place where a live credential
/// lives.
///
/// The reason narrowed in round 210, when D-16① sealed the column (`crypto::Sealed`, with legacy
/// rows re-sealed by the loader): `matching_key` used to be a PLAIN column, and it now travels as an
/// envelope at rest, in the config tree and in a replica's database. What the digest still buys is
/// the half sealing cannot: the key is not there to recover AT ALL, not even for a holder of the
/// master key — and `GET /api/v1/limit-roles` still echoes the raw form in the clear.
pub const KEY_DIGEST_PREFIX: &str = "sha256:";

/// The digest form of a client key, i.e. exactly what a role may put in `matching_key`.
#[must_use]
pub fn key_digest(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(key.as_bytes());
    format!("{KEY_DIGEST_PREFIX}{:x}", h.finalize())
}

/// Is `s` a well-formed [`key_digest`]? (Lowercase hex, 64 digits, after the prefix.)
#[must_use]
pub fn is_key_digest(s: &str) -> bool {
    s.strip_prefix(KEY_DIGEST_PREFIX).is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// The KEY component of a request-count bucket: a **digest** of the client key.
///
/// Decision D-15② (2026-10-08). It used to be the MASK (`mask_key`), and that was wrong in two
/// directions, both measured:
///   * **it leaked**: in cluster mode this string becomes a Redis key name (`hydra:{rl:…}:count`), so
///     the first and last characters of a customer's credential sat in the keyspace — and in any
///     metric label derived from it;
///   * **it collided**: a mask keeps 10 leading and 4 trailing characters, so two different keys with
///     the same mask shared ONE budget. Measured then: `sk-fidelity-limited` and `skzzzzzzzzzzzzzzzed`
///     have the same mask, and once the first key's window was spent the second was refused on its
///     very first request — one customer eating another's quota, silently.
///
/// A digest of the RAW key is per-key distinct and carries no recoverable secret; the mask remains
/// what a role may MATCH on, which is a different question from what identifies the window.
#[must_use]
pub fn bucket_key(ctx: &MatchCtx<'_>) -> String {
    match (ctx.api_key_raw, ctx.api_key) {
        (Some(raw), _) => key_bucket_id(raw),
        (None, Some(masked)) => key_bucket_id(masked),
        (None, None) => String::new(),
    }
}

/// The 32-hex-character bucket identity of one key: `sha256(key)` truncated. Truncation is safe here
/// (this is an identity, not a signature: no adversary chooses the input to collide with a target
/// budget) and keeps Redis key names and window maps short.
#[must_use]
pub fn key_bucket_id(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(key.as_bytes());
    let hex = format!("{:x}", h.finalize());
    hex[..32].to_string()
}

/// Match the key dimension against the raw client key, its mask, or its DIGEST.
///
/// The first two forms are the historical ones (see [`MatchCtx::api_key_raw`]); the third is decision
/// D-16③: a role whose `matching_key` is a `sha256:` digest matches when the presented key hashes to
/// it, so the operator can configure the limit without storing the credential anywhere.
fn key_dim_matches(role_dim: Option<&str>, ctx: &MatchCtx) -> bool {
    match role_dim {
        None => true,
        Some(wanted) => {
            ctx.api_key == Some(wanted)
                || ctx.api_key_raw == Some(wanted)
                || (is_key_digest(wanted)
                    && (ctx.api_key_raw.map(key_digest).as_deref() == Some(wanted)
                        || ctx.api_key.map(key_digest).as_deref() == Some(wanted)))
        }
    }
}

/// Match every enabled `LimitRole` against `ctx`.
///
/// A role matches when **every** non-`None` `matching_*` field equals the
/// corresponding `ctx` value; a `None` field is match-all (design §10.1: "为
/// NULL 或 等于"). The `matching_key` dimension accepts the raw, the masked OR the digest
/// client key (`key_dim_matches`) — see [`MatchCtx::api_key_raw`] for why. A role value of `Some(x)` does **not** match a `ctx`
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
                && key_dim_matches(r.matching_key.as_deref(), ctx)
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

/// Hard cap on request-count samples retained in ONE window.
///
/// 1_048_576 `Instant`s ≈ 16 MiB — far above any sane per-window request budget
/// (the default hourly budget for a whole tenant is orders of magnitude smaller),
/// while still bounding a misconfigured `limit_count`.
pub const MAX_SAMPLES_PER_WINDOW: u64 = 1_048_576;

/// Hard cap on token-dimension samples retained in one window (≈24 MiB).
pub const MAX_TOKEN_SAMPLES_PER_WINDOW: usize = 1_048_576;

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
/// A sliding window over `(Instant, value)` samples.
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

    /// **How long until this window starts to drain** — i.e. until its OLDEST live
    /// sample (count or token) ages out.
    ///
    /// This is the value `ops.md` §4.2 promises in `Retry-After` ("reflecting the
    /// remainder of the current window"): a client that was refused can sleep exactly
    /// this long and then be under the ceiling again. `None` when the window holds
    /// nothing (there is nothing to wait for — the refusal did not come from this
    /// window).
    ///
    /// Measured against the oldest of BOTH deques on purpose: a token quota can be the
    /// binding constraint even when no count sample is live, and vice versa.
    #[must_use]
    pub fn retry_after(&self, now: Instant) -> Option<Duration> {
        let oldest = match (self.samples.front(), self.token_samples.front()) {
            (Some(c), Some((t, _))) => Some((*c).min(*t)),
            (Some(c), None) => Some(*c),
            (None, Some((t, _))) => Some(*t),
            (None, None) => None,
        }?;
        Some(
            self.window
                .saturating_sub(now.saturating_duration_since(oldest)),
        )
    }

    /// **Count dimension**: evict stale samples, then if fewer than `limit`
    /// live samples remain, record `now` and return `true` (admit); otherwise
    /// return `false` (over the count limit — design §10.3: pre-gate → 429).
    /// A rejected request is **not** enqueued.
    pub fn check_and_inc(&mut self, now: Instant, limit: u64) -> bool {
        self.evict_samples(now);
        // The queue length was bounded ONLY by the configured `limit`, which comes
        // from an operator-supplied `LimitRole` with no upper-bound validation: a
        // role with `limit_count = 100_000_000` would accumulate 100M `Instant`s
        // (~1.6 GiB) in one window before the first denial. The cap below therefore
        // DOES clamp the effective limit — it is not verdict-neutral. For any
        // `limit` above `MAX_SAMPLES_PER_WINDOW` the window starts denying at
        // 1_048_576 instead of at the configured number, i.e. it errs toward
        // DENYING. That direction is deliberate: a request count that high is a
        // misconfiguration, and honoring it costs gigabytes of attacker-influenced
        // memory. For every `limit` at or below the cap the behaviour is unchanged.
        let effective = limit.min(MAX_SAMPLES_PER_WINDOW);
        if (self.samples.len() as u64) < effective {
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
        // Token samples are bounded by the number of REQUESTS in the window, not
        // by `limit_token` (it is a token budget, not a sample budget), so a busy
        // window grew without any ceiling. Merge the two oldest samples when the
        // cap is reached: `token_sum` is maintained incrementally, so the merge
        // leaves the window's total exactly unchanged.
        //
        // The merged chunk keeps the NEWER of the two timestamps, which is the
        // fail-CLOSED direction. The two samples expire at `t+window` each; merged
        // under `max` the pair is still counted until the newer one expires, so
        // between the two expiries the window OVER-counts (by the older sample's
        // tokens, for at most `t1 - t0`). Merging under `min` would drop both at
        // the older expiry — it would forget tokens that are still inside the
        // window, i.e. UNDER-count and admit a request that is over budget. A
        // bounded over-count merely denies early; an under-count grants free
        // budget, so the over-counting timestamp is the correct one here.
        if self.token_samples.len() >= MAX_TOKEN_SAMPLES_PER_WINDOW {
            if let (Some((t0, v0)), Some((t1, v1))) = (
                self.token_samples.pop_front(),
                self.token_samples.pop_front(),
            ) {
                // `max` also preserves the deque's non-decreasing time order.
                self.token_samples
                    .push_front((t0.max(t1), v0.saturating_add(v1)));
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Fill `token_samples` directly: 1_048_576 `add` calls would work but this
    /// pins the exact timestamps the assertions depend on and keeps the test fast.
    fn window_with_token_samples(now: Instant, oldest: [(Instant, u64); 2]) -> SlidingWindow {
        let mut w = SlidingWindow::new(Duration::from_secs(3600));
        let cap = MAX_TOKEN_SAMPLES_PER_WINDOW;
        w.token_samples.push_back(oldest[0]);
        w.token_samples.push_back(oldest[1]);
        for _ in 2..cap {
            w.token_samples.push_back((now, 1));
        }
        w.token_sum = oldest[0].1 + oldest[1].1 + (cap as u64 - 2);
        assert_eq!(
            w.token_samples.len(),
            cap,
            "fixture must sit exactly at the cap"
        );
        w
    }

    /// The merge at the token cap must (a) be a hard bound, (b) leave the window
    /// total untouched, (c) keep the deque time-sorted, and (d) expire with the
    /// **newer** of the two merged samples.
    ///
    /// (d) is the whole point: under `min` the merged pair would be dropped at the
    /// older expiry, forgetting tokens still inside the window — an under-count,
    /// i.e. free budget for a request that is over it. Only over-counting is
    /// fail-closed. Falsification: switch the merge back to `min` and the
    /// `is_live_between_the_two_expiries` assertion below fails by exactly the
    /// older sample's 7 tokens.
    #[test]
    fn token_cap_merge_keeps_the_total_and_errs_toward_over_counting() {
        let base = Instant::now();
        let cap = MAX_TOKEN_SAMPLES_PER_WINDOW;
        let mut w = window_with_token_samples(
            base,
            [
                (base - Duration::from_secs(10), 7),
                (base - Duration::from_secs(5), 11),
            ],
        );
        let before = w.token_sum;

        // This `add` is the one that hits the cap: the two oldest merge.
        w.add(base, 100);

        assert_eq!(
            w.token_samples.len(),
            cap,
            "the sample cap is a hard bound, not a hint"
        );
        let expected = before + 100;
        assert_eq!(
            w.token_sum, expected,
            "merging must not change the window's total"
        );
        let (t, v) = w.token_samples.front().copied().expect("merged chunk");
        assert_eq!(
            t,
            base - Duration::from_secs(5),
            "merged chunk must expire with the NEWER sample, not the older one"
        );
        assert_eq!(v, 18, "merged chunk must carry both samples' tokens");
        assert!(
            w.token_samples
                .iter()
                .zip(w.token_samples.iter().skip(1))
                .all(|(a, b)| a.0 <= b.0),
            "the merge must preserve the deque's non-decreasing time order"
        );

        // Between the older and the newer expiry the merged tokens are STILL live:
        // the window over-counts (fail-closed) instead of forgetting them.
        let between = base + Duration::from_secs(3591);
        assert_eq!(
            w.token_used(between),
            expected,
            "tokens whose newer half is inside the window must still count"
        );

        // Past the newer expiry everything in the fixture is gone.
        assert_eq!(
            w.token_used(base + Duration::from_secs(3601)),
            0,
            "nothing may be counted past the window"
        );
    }

    /// The count cap deliberately clamps a misconfigured `limit_count`: it errs
    /// toward DENYING rather than honouring a limit whose samples would cost
    /// gigabytes. For limits at or below the cap behaviour is unchanged.
    #[test]
    fn the_count_cap_clamps_a_huge_configured_limit_and_errs_toward_denying() {
        let now = Instant::now();
        let mut fat = SlidingWindow::new(Duration::from_secs(3600));
        for _ in 0..MAX_SAMPLES_PER_WINDOW {
            fat.samples.push_back(now);
        }
        assert!(
            !fat.check_and_inc(now, MAX_SAMPLES_PER_WINDOW + 1_000),
            "a window at the cap must deny even when the configured limit is higher"
        );
        assert!(
            !fat.check_and_inc(now, 100_000_000),
            "an absurd limit_count must not be honoured (it would cost ~1.6 GiB)"
        );
        assert_eq!(fat.count(), MAX_SAMPLES_PER_WINDOW as usize);

        let mut small = SlidingWindow::new(Duration::from_secs(3600));
        assert!(small.check_and_inc(now, 2));
        assert!(small.check_and_inc(now, 2));
        assert!(
            !small.check_and_inc(now, 2),
            "limits below the cap keep their exact old behaviour"
        );
        assert_eq!(small.count(), 2, "a rejected request is not enqueued");
    }
}
