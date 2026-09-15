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
use std::time::{Instant, SystemTime, UNIX_EPOCH};

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
pub const ADD_TOKENS_SCRIPT: &str = r#"
local now = tonumber(ARGV[1])
local window_ms = tonumber(ARGV[2])
local member = ARGV[3]
local tokens = tonumber(ARGV[4])
local zk = KEYS[1]
redis.call('ZREMRANGEBYSCORE', zk, '-inf', now - window_ms)
redis.call('ZADD', zk, tokens, member)
redis.call('PEXPIRE', zk, window_ms)
return redis.call('ZCARD', zk)
"#;

/// Atomic token check: prune the window, sum the live token scores.
/// `ARGV`: [now_ms, window_ms, limit] → 1 admit, 0 deny.
pub const CHECK_TOKENS_SCRIPT: &str = r#"
local now = tonumber(ARGV[1])
local window_ms = tonumber(ARGV[2])
local limit = tonumber(ARGV[3])
local zk = KEYS[1]
redis.call('ZREMRANGEBYSCORE', zk, '-inf', now - window_ms)
local parts = redis.call('ZRANGE', zk, 0, -1, 'WITHSCORES')
local sum = 0
for i = 2, #parts, 2 do
  sum = sum + tonumber(parts[i])
end
if sum >= limit then return 0 else return 1 end
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
        Self { pool }
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
                return CountVerdict::Denied {
                    role_id: role.id.clone(),
                };
            }
            let member = format!("{now}-{}", member_salt(&key));
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
                    continue; // fail-open per role (HYDRA_RATE_LIMIT_FAIL_MODE=open default)
                }
            };
            if admitted == 0 {
                return CountVerdict::Denied {
                    role_id: role.id.clone(),
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
                return CountVerdict::Denied {
                    role_id: role.id.clone(),
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
                let member = format!("{now}-{}", member_salt(&key.bucket));
                let _: Result<i64, _> = self
                    .pool
                    .eval(
                        ADD_TOKENS_SCRIPT,
                        vec![tokens_key(&key)],
                        vec![
                            now.to_string(),
                            window_ms(role).to_string(),
                            member,
                            tokens.to_string(),
                        ],
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

fn member_salt(_key: &str) -> String {
    // Uniqueness within the same millisecond: a monotonic-ish component.
    use std::sync::atomic::{AtomicU64, Ordering};
    static SALT: AtomicU64 = AtomicU64::new(0);
    SALT.fetch_add(1, Ordering::Relaxed).to_string()
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
    parts.join("\u{1f}")
}

// ---------------------------------------------------------------------------
// Tests against the in-process Redis double (real command semantics)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redis::mock::MockRedis;

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

    #[test]
    fn script_semantics_via_double() {
        // The Lua scripts' sliding-window semantics, executed by the in-process
        // double (fred's mock layer cannot round-trip EVAL; live Redis
        // integration lands with deployment acceptance).
        let m = MockRedis::new();
        let ck = "hydra:{rl:r1:b}:count".to_string();
        for (i, expect) in [1i64, 1, 0].iter().enumerate() {
            let got = m
                .run_script(
                    CHECK_AND_INC_SCRIPT,
                    std::slice::from_ref(&ck),
                    &["1000".into(), "60000".into(), "2".into(), format!("m{i}")],
                )
                .unwrap();
            assert_eq!(got, *expect, "call {i}: admit, admit, then deny");
        }
    }
}
