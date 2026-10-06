//! # Shared rate limiting over Redis (cluster P4)
//!
//! The pure matching logic (`hydra_core::limit::match_roles` /
//! `bucket_for`) stays client-side; only the counter backend is Redis. The
//! sliding-log window is a Lua script (one atomic round trip), mirroring the
//! classic Redis sliding-window pattern (Kong rate-limiting-advanced).
//!
//! **Keys**: `hydra:{rl:role:bucket}:count` and `:tokens` share the
//! `{rl:role:bucket}` hash tag → one slot in Redis Cluster (plan §6.1).
//!
//! **Hot path**: only requests that match a limited role touch Redis (~0.2–
//! 0.5 ms local round trip); unlimited traffic stays untouched.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fred::clients::Pool;
use fred::prelude::*;
use tracing::warn;

use hydra_core::limit::{match_roles, MatchCtx};
use hydra_core::model::LimitRole;

use crate::proxy::limiter::{CountVerdict, LimitKey};

/// Atomic check-and-increment (request count): prune the window, admit iff
/// under the limit. `ARGV`: [now_ms, window_ms, limit, member].
pub const CHECK_AND_INC_SCRIPT: &str = r#"
local now = tonumber(ARGV[1])
local window_ms = tonumber(ARGV[2])
local limit = tonumber(ARGV[3])
local member = ARGV[4]
local zk = KEYS[1]
redis.call('ZREMRANGEBYSCORE', zk, '-inf', now - window_ms)
local count = redis.call('ZCARD', zk)
if count < limit then
  redis.call('ZADD', zk, now, member)
  redis.call('PEXPIRE', zk, window_ms)
  return 1
else
  return 0
end
"#;

/// Atomic token accounting: record `tokens` for this request in the window.
/// `ARGV`: [now_ms, window_ms, member, tokens].
///
/// The SCORE is the token count, not the timestamp: the previous version stored
/// the timestamp and returned ZCARD, so the cluster token window held a REQUEST
/// COUNT where every consumer expects a token sum — wrong by two to three orders
/// of magnitude, and it would have under-enforced the moment token limits were
/// actually checked.
/// Token accounting: the SCORE is the TIME, and the member carries the token
/// count (`<now>:<tokens>:<salt>`).
///
/// The score used to be the token count, so that the check could `SUM` the
/// scores — but eviction is `ZREMRANGEBYSCORE zk '-inf' (now - window_ms)`, which
/// compares scores against a millisecond TIMESTAMP. With a real clock
/// (`now ≈ 1.79e12`) that bound exceeds any token count, so every add wiped the
/// whole window and the check summed 0: a token budget never accumulated and was
/// never enforced. The existing script test could not see it because it passed a
/// fake `now` of 1000/2000, where `now - window_ms` is negative. Score = time
/// keeps eviction and accumulation consistent; the member is the only place a
/// count can live, so the sum parses it (and SKIPS entries it cannot parse, i.e.
/// legacy-format members, which the first new-format call evicts anyway because
/// their score — a token count — is far below the time-based bound).
pub const ADD_TOKENS_SCRIPT: &str = r#"
local now = tonumber(ARGV[1])
local window_ms = tonumber(ARGV[2])
local member = ARGV[3]
local zk = KEYS[1]
redis.call('ZREMRANGEBYSCORE', zk, '-inf', now - window_ms)
redis.call('ZADD', zk, now, member)
redis.call('PEXPIRE', zk, window_ms)
local total = 0
for _, m in ipairs(redis.call('ZRANGE', zk, 0, -1)) do
  local t = string.match(m, '^%d+:(%d+):')
  if t then total = total + tonumber(t) end
end
return total
"#;

/// Atomic token check: prune the window, sum the live token scores.
/// `ARGV`: [now_ms, window_ms, limit] → 1 admit, 0 deny.
/// Token verdict: evict expired entries by SCORE (time), sum the token counts
/// carried by the members, and compare. 1 = admit, 0 = deny.
///
/// The eviction here is housekeeping only — it must never be able to empty a
/// live window. The previous version summed the SCORES, which (with score =
/// tokens) meant the eviction bound above removed everything first and the sum
/// was always 0: this read path admitted unconditionally AND destroyed the window
/// it read.
pub const CHECK_TOKENS_SCRIPT: &str = r#"
local now = tonumber(ARGV[1])
local window_ms = tonumber(ARGV[2])
local limit = tonumber(ARGV[3])
local zk = KEYS[1]
redis.call('ZREMRANGEBYSCORE', zk, '-inf', now - window_ms)
local total = 0
for _, m in ipairs(redis.call('ZRANGE', zk, 0, -1)) do
  local t = string.match(m, '^%d+:(%d+):')
  if t then total = total + tonumber(t) end
end
if total >= limit then return 0 else return 1 end
"#;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Cluster-wide rate limiter (P4): same interface as the in-memory
/// [`crate::proxy::limiter::RateLimiter`], but the sliding windows live in
/// Redis (Lua), so limits are enforced across the WHOLE cluster.
pub struct RedisRateLimiter {
    pool: Pool,
    /// This INSTANCE's member prefix, mixed into every window member.
    ///
    /// The members are `SET`-like entries in a sorted set: two writes whose
    /// member strings are equal COLLAPSE into one (`ZADD` overwrites the score),
    /// so the count silently under-reports. The member used to be
    /// `<now_ms>-<counter>` with a per-PROCESS counter starting at 0, which means
    /// the first request of the same millisecond on two different nodes produced
    /// the SAME member on both — the fleet counted one, not two. Under-counting
    /// is the fail-open direction: a tenant could hold a window full of requests
    /// that the limiter never saw.
    ///
    /// The prefix is per instance (and carries a boot-time nonce) so it is unique
    /// across processes AND testable: two limiter instances in one test process
    /// behave like two nodes. `pid` alone would NOT be enough — every container
    /// can be PID 1.
    instance: String,
}

impl crate::proxy::limiter::Limiter for RedisRateLimiter {
    fn check_count<'a>(
        &'a self,
        roles: &'a [LimitRole],
        ctx: &'a MatchCtx<'a>,
        now: Instant,
    ) -> Pin<Box<dyn Future<Output = CountVerdict> + Send + 'a>> {
        Box::pin(async move { self.check_count(roles, ctx, now).await })
    }

    fn add_tokens<'a>(
        &'a self,
        roles: &'a [LimitRole],
        ctx: &'a MatchCtx<'a>,
        tokens: u64,
        now: Instant,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move { self.add_tokens(roles, ctx, tokens, now).await })
    }

    fn check_tokens<'a>(
        &'a self,
        roles: &'a [LimitRole],
        ctx: &'a MatchCtx<'a>,
        now: Instant,
    ) -> Pin<Box<dyn Future<Output = CountVerdict> + Send + 'a>> {
        Box::pin(async move { self.check_tokens(roles, ctx, now).await })
    }

    fn gc(&self) {
        // Redis windows prune themselves (PEXPIRE in the scripts).
    }
}

impl RedisRateLimiter {
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self {
            pool,
            instance: new_instance_nonce(),
        }
    }

    /// Pre-gate count check: for every matched role with a `limit_count`,
    /// atomically check-and-increment its Redis window; any denial → `Denied`.
    pub async fn check_count(
        &self,
        roles: &[LimitRole],
        ctx: &MatchCtx<'_>,
        _now: Instant,
    ) -> CountVerdict {
        let now = now_ms();
        for (role, key, limit) in windows_to_check(roles, ctx) {
            if limit == 0 {
                // Unconditional deny: no window is involved, so there is no window
                // remainder to promise (`Retry-After` then falls back to 1s).
                return CountVerdict::Denied {
                    role_id: role.id.clone(),
                    retry_after: None,
                };
            }
            let member = format!("{now}-{}-{}", self.instance, member_salt());
            let admitted: i64 = match self
                .pool
                .eval(
                    CHECK_AND_INC_SCRIPT,
                    vec![key],
                    vec![
                        now.to_string(),
                        window_ms(role).to_string(),
                        limit.to_string(),
                        member,
                    ],
                )
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "redis rate-limit check failed; failing open");
                    crate::admin::metrics::record_control_poll("rate_limit_error");
                    // fail-open per role — the error is deliberately swallowed
                    // (documented in redis/mod.rs; there is NO env override)
                    continue;
                }
            };
            if admitted == 0 {
                // The Redis limiter keeps its samples inside Redis, so it cannot read the
                // OLDEST one's age the way the in-process window can: it reports the whole
                // window as an upper bound on the remainder. That is deliberately
                // conservative — `Retry-After` must never invite an EARLIER retry than the
                // window allows, or the client walks straight into another 429.
                return CountVerdict::Denied {
                    role_id: role.id.clone(),
                    retry_after: Some(Duration::from_millis(window_ms(role).max(0) as u64)),
                };
            }
        }
        CountVerdict::Admitted
    }

    /// Pre-gate token check: for every matched role with a `limit_token`, sum
    /// its live token window in Redis. Fail-open per role, exactly like the
    /// count gate (a Redis outage must not lock every tenant out).
    pub async fn check_tokens(
        &self,
        roles: &[LimitRole],
        ctx: &MatchCtx<'_>,
        _now: Instant,
    ) -> CountVerdict {
        let now = now_ms();
        for role in match_roles(roles, ctx) {
            let Some(limit) = role.limit_token else {
                continue;
            };
            // `limit_token == 0` ⇒ the script's `sum >= 0` denies
            // unconditionally, mirroring `limit_count == 0`.
            let limit = u64::try_from(limit.max(0)).unwrap_or(0);
            let key = LimitKey {
                role_id: role.id.clone(),
                bucket: bucket_for(role, ctx),
            };
            let admitted: i64 = match self
                .pool
                .eval(
                    CHECK_TOKENS_SCRIPT,
                    vec![tokens_key(&key)],
                    vec![
                        now.to_string(),
                        window_ms(role).to_string(),
                        limit.to_string(),
                    ],
                )
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "redis token-limit check failed; failing open");
                    crate::admin::metrics::record_control_poll("rate_limit_error");
                    continue;
                }
            };
            if admitted == 0 {
                // The Redis limiter keeps its samples inside Redis, so it cannot read the
                // OLDEST one's age the way the in-process window can: it reports the whole
                // window as an upper bound on the remainder. That is deliberately
                // conservative — `Retry-After` must never invite an EARLIER retry than the
                // window allows, or the client walks straight into another 429.
                return CountVerdict::Denied {
                    role_id: role.id.clone(),
                    retry_after: Some(Duration::from_millis(window_ms(role).max(0) as u64)),
                };
            }
        }
        CountVerdict::Admitted
    }

    /// Record token usage in the `logging` phase (fire-and-forget semantics:
    /// the batch insert happens async; here we await since callers are async).
    pub async fn add_tokens(
        &self,
        roles: &[LimitRole],
        ctx: &MatchCtx<'_>,
        tokens: u64,
        _now: Instant,
    ) {
        let now = now_ms();
        let matched = match_roles(roles, ctx);
        for role in &matched {
            if role.limit_token.is_some() {
                let key = LimitKey {
                    role_id: role.id.clone(),
                    bucket: bucket_for(role, ctx),
                };
                // Member = `<now>:<tokens>:<instance>-<salt>`; the score is the
                // time (see ADD_TOKENS_SCRIPT). The token count lives in the
                // member so the sum and the eviction can both be consistent.
                let member = format!("{now}:{tokens}:{}-{}", self.instance, member_salt());
                let _: Result<i64, _> = self
                    .pool
                    .eval(
                        ADD_TOKENS_SCRIPT,
                        vec![tokens_key(&key)],
                        vec![now.to_string(), window_ms(role).to_string(), member],
                    )
                    .await;
            }
        }
    }
}

// -- key helpers (plan §6.1: hash-tagged, one slot per (role, bucket)) ------

fn count_key(key: &LimitKey) -> String {
    format!("hydra:{{rl:{}:{}}}:count", key.role_id, key.bucket)
}

fn tokens_key(key: &LimitKey) -> String {
    format!("hydra:{{rl:{}:{}}}:tokens", key.role_id, key.bucket)
}

/// Pure: for matched roles with a `limit_count`, the `(role, count_key,
/// limit)` triples to check, in match order. Extracted for deterministic
/// testing (the Redis calls themselves are thin).
fn windows_to_check<'a>(
    roles: &'a [LimitRole],
    ctx: &MatchCtx<'a>,
) -> Vec<(&'a LimitRole, String, u64)> {
    match_roles(roles, ctx)
        .into_iter()
        // `limit_count == NULL` means no request-count limit (migration 0001:
        // NULL = unlimited), exactly as the in-memory limiter treats it. Mapping
        // it to 0 made 0 the deny-unconditionally sentinel, so a legal token-only
        // role 429'd EVERY matched request cluster-wide while single-node mode
        // skipped it entirely.
        .filter(|r| r.limit_count.is_some())
        .map(|r| {
            let limit = u64::try_from(r.limit_count.unwrap_or(0).max(0)).unwrap_or(0);
            let key = count_key(&LimitKey {
                role_id: r.id.clone(),
                bucket: bucket_for(r, ctx),
            });
            (r, key, limit)
        })
        .collect()
}

/// Uniqueness WITHIN one instance: a monotonic per-process counter.
fn member_salt() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SALT: AtomicU64 = AtomicU64::new(0);
    SALT.fetch_add(1, Ordering::Relaxed).to_string()
}

/// A nonce unique to this limiter instance (see [`RedisRateLimiter::instance`]).
///
/// `pid` alone is not enough (in containers every node can be PID 1), so it is
/// mixed with the wall clock at construction time and an atomic counter: two
/// instances in one process differ by the counter, and two processes differ by
/// pid and/or the clock reading.
fn new_instance_nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}-{}-{seq:x}", std::process::id(), nanos)
}

/// Window length for a role's `window` field (m=60s, h=3600s, d=86400s).
fn window_ms(role: &LimitRole) -> i64 {
    match role.window.as_str() {
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => 60_000,
    }
}

/// Deterministic bucket string for a role's known-at-pre-gate matching
/// dimensions (api-key / model / tenant). Mirrors
/// [`crate::proxy::limiter::bucket_for`] (kept private there; the provider
/// dimension is unknown until routing, §10.3).
fn bucket_for(role: &LimitRole, ctx: &MatchCtx<'_>) -> String {
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
    parts.join("\u{1f}")
}

// ---------------------------------------------------------------------------
// Tests against the in-process Redis double (real command semantics)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn role(id: &str, count: Option<i64>, window: &str) -> LimitRole {
        LimitRole {
            id: id.into(),
            name: id.into(),
            matching_key: None,
            matching_model: None,
            matching_tenant: None,
            matching_provider: None,
            limit_count: count,
            limit_token: None,
            window: window.into(),
            enabled: true,
            created_at: String::new(),
        }
    }

    fn ctx() -> MatchCtx<'static> {
        MatchCtx {
            api_key: None,
            api_key_raw: None, // this fixture exercises key-less roles, so no raw key is needed
            model: None,
            tenant: Some("t1"),
            provider: None,
        }
    }

    #[test]
    fn windows_to_check_matches_roles() {
        let mut r_tenant = role("r-all", Some(10), "m");
        r_tenant.matching_tenant = Some("t1".into()); // tenant dim → bucket includes it
        let roles = vec![
            r_tenant,
            role("r-zero", Some(0), "m"),
            role("r-unlimited", None, "m"),
        ];
        let checks = windows_to_check(&roles, &ctx());
        assert_eq!(
            checks.len(),
            2,
            "only roles WITH a limit_count are checked: a NULL limit_count means              unlimited (migration 0001), and mapping it to 0 made it the              deny-unconditionally sentinel — a token-only role 429'd everything              cluster-wide while single-node mode skipped it"
        );
        assert!(
            checks[0].1.contains("{rl:r-all:t1}:count"),
            "bucket includes the tenant dim"
        );
        assert!(
            checks[1].1.contains("{rl:r-zero:}:count"),
            "the zero-limit role is still checked (0 = deny-all, handled by the caller)"
        );
        assert!(
            !checks.iter().any(|(r, _, _)| r.id == "r-unlimited"),
            "a NULL limit_count means UNLIMITED and must not be checked at all"
        );
    }

    #[test]
    fn keys_share_hash_tag() {
        let k = LimitKey {
            role_id: "r1".into(),
            bucket: "b".into(),
        };
        assert!(count_key(&k).contains("{rl:r1:b}") && tokens_key(&k).contains("{rl:r1:b}"));
    }

    /// The token window must ACCUMULATE, and the check must not destroy it.
    ///
    /// Found while fixing the member collision above: the scripts store the token
    /// COUNT as the sorted-set score but evict with
    /// `ZREMRANGEBYSCORE zk '-inf' (now - window_ms)` — comparing a token count
    /// against a millisecond timestamp. With a real `now` (1.79e12) that bound is
    /// larger than any token count, so every call wiped the whole window:
    ///
    /// * `add_tokens` left exactly one entry (the last write), so a token budget
    ///   never accumulated; and
    /// * `check_tokens` evicted everything BEFORE summing, so the sum was always
    ///   0 and the check admitted unconditionally — a READ path that also
    ///   destroyed the window it was reading.
    ///
    /// The existing script test missed it because it passes a fake `now` of 2000,
    /// where `now - window_ms` is negative and nothing is evicted.
    #[tokio::test]
    async fn the_token_window_accumulates_and_check_does_not_wipe_it() {
        let pool = crate::redis::test_redis::isolated_pool().await;
        let limiter = RedisRateLimiter::new(pool.clone());
        let mut r = role("r-tok", None, "m");
        r.limit_token = Some(100);
        let roles = vec![r];
        let c = ctx();

        // 60 tokens, then 60 more: the window holds 120 > the 100 budget, so the
        // third check must DENY.
        limiter.add_tokens(&roles, &c, 60, Instant::now()).await;
        limiter.add_tokens(&roles, &c, 60, Instant::now()).await;

        // Read the window directly (the sum must be 120, not 60).
        use fred::prelude::*;
        let key = LimitKey {
            role_id: "r-tok".into(),
            bucket: bucket_for(&roles[0], &c),
        };
        let members: Vec<String> = pool
            .zrange(tokens_key(&key), 0, -1, None, false, None, false)
            .await
            .expect("zrange");
        assert_eq!(
            members.len(),
            2,
            "both token writes must still be in the window: {members:?}"
        );

        // The check must DENY (120 > 100) and must still deny on a second call —
        // a read path that evicted would flip to admit.
        for attempt in 0..2 {
            let verdict = limiter.check_tokens(&roles, &c, Instant::now()).await;
            assert!(
                matches!(verdict, CountVerdict::Denied { .. }),
                "attempt {attempt}: 120 tokens exceed the 100 budget ⇒ deny"
            );
        }
    }

    /// Two NODES must not share a window member.
    ///
    /// Members live in a sorted set, so equal member strings COLLAPSE (`ZADD`
    /// overwrites the score) and the window under-reports. The member used to be
    /// `<now_ms>-<per-process counter>`, so two nodes' first write in the same
    /// millisecond produced the SAME member: the fleet counted one request where
    /// two arrived. Under-counting is the fail-open direction — a tenant could
    /// fill a window with requests the limiter never saw.
    ///
    /// Exercised through the REAL `add_tokens` path (not hand-built members), with
    /// two limiter instances standing in for two nodes.
    #[tokio::test]
    async fn two_instances_do_not_collapse_the_same_window_member() {
        let pool = crate::redis::test_redis::isolated_pool().await;
        let a = RedisRateLimiter::new(pool.clone());
        let b = RedisRateLimiter::new(pool.clone());
        assert_ne!(
            a.instance, b.instance,
            "two instances must not share a member namespace"
        );

        // Same role, same key, same `now` (the same millisecond) on both nodes.
        let mut r = role("r-tok", Some(1000), "m");
        r.limit_token = Some(1_000_000);
        let roles = vec![r];
        let c = ctx();
        let now = Instant::now();
        a.add_tokens(&roles, &c, 5, now).await;
        b.add_tokens(&roles, &c, 5, now).await;

        // The window must hold BOTH writes: 5 + 5.
        let key = bucket_for(&roles[0], &c);
        let parts: Vec<String> = {
            use fred::prelude::*;
            pool.zrange(
                tokens_key(&LimitKey {
                    role_id: "r-tok".into(),
                    bucket: key,
                }),
                0,
                -1,
                None,
                false,
                None,
                false,
            )
            .await
            .expect("zrange")
        };
        assert_eq!(
            parts.len(),
            2,
            "both nodes' writes must be distinct members (one member = the same \
             millisecond collapsed them, i.e. the window under-reported)"
        );
    }

    /// The sliding-window semantics of the REAL Lua script, evaluated by a REAL
    /// Redis (dev-plan 铁律 2). The previous version ran the script through the
    /// in-process double's own interpreter — which could agree with a broken
    /// script, since both were written from the same reading of the semantics.
    #[tokio::test]
    async fn script_semantics_on_real_redis() {
        use fred::prelude::*;
        let pool = crate::redis::test_redis::isolated_pool().await;
        let ck = "hydra:{rl:r1:b}:count".to_string();
        for (i, expect) in [1i64, 1, 0].iter().enumerate() {
            let got: i64 = pool
                .eval(
                    CHECK_AND_INC_SCRIPT,
                    vec![ck.clone()],
                    vec![
                        "1000".to_string(),
                        "60000".to_string(),
                        "2".to_string(),
                        format!("m{i}"),
                    ],
                )
                .await
                .expect("EVAL");
            assert_eq!(got, *expect, "call {i}: admit, admit, then deny");
        }

        // The window expires: an old member falls out and admission resumes.
        let later: i64 = pool
            .eval(
                CHECK_AND_INC_SCRIPT,
                vec![ck.clone()],
                vec![
                    (1000 + 61_000).to_string(),
                    "60000".to_string(),
                    "2".to_string(),
                    "m-later".to_string(),
                ],
            )
            .await
            .expect("EVAL");
        assert_eq!(later, 1, "the window rolled over ⇒ admit again");
    }

    /// Token accounting on the real scripts: the SCORE is the TIME and the member
    /// carries the token count, so eviction and accumulation agree.
    ///
    /// This test used to pass a fake `now` of 1000/2000 and members WITHOUT the
    /// token prefix — exactly the shape that hid the score/time confusion: with
    /// `now = 1000`, `now - window_ms` is negative, so nothing was ever evicted
    /// and the "sum the scores" check looked correct. It now uses a real
    /// millisecond clock and the production member format.
    #[tokio::test]
    async fn token_accounting_on_real_redis() {
        use fred::prelude::*;
        let pool = crate::redis::test_redis::isolated_pool().await;
        let tk = "hydra:{rl:r1:b}:tokens".to_string();
        let now = now_ms();

        let mut expected = 0i64;
        for tokens in [10u64, 20, 30] {
            expected += tokens as i64;
            let sum: i64 = pool
                .eval(
                    ADD_TOKENS_SCRIPT,
                    vec![tk.clone()],
                    vec![
                        now.to_string(),
                        "60000".to_string(),
                        format!("{now}:{tokens}:m-{tokens}"),
                    ],
                )
                .await
                .expect("EVAL");
            assert_eq!(
                sum, expected,
                "the add returns the running total: the window must ACCUMULATE"
            );
        }

        // 60 tokens: admits under 100, denies under 50 — and the second check
        // must not have destroyed the window (a read path that evicted would flip
        // the verdict).
        for (limit, want) in [("100", 1i64), ("50", 0), ("100", 1)] {
            let verdict: i64 = pool
                .eval(
                    CHECK_TOKENS_SCRIPT,
                    vec![tk.clone()],
                    vec![now.to_string(), "60000".to_string(), limit.to_string()],
                )
                .await
                .expect("EVAL");
            assert_eq!(
                verdict, want,
                "60 tokens against a {limit}-token limit ⇒ {want}"
            );
        }

        // The window really is time-based: a check one window later evicts
        // everything by SCORE and admits again.
        let later: i64 = pool
            .eval(
                CHECK_TOKENS_SCRIPT,
                vec![tk.clone()],
                vec![
                    (now + 61_000).to_string(),
                    "60000".to_string(),
                    "50".to_string(),
                ],
            )
            .await
            .expect("EVAL");
        assert_eq!(later, 1, "the window rolled over ⇒ the budget resets");
    }
}
