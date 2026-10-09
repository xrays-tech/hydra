//! Proxy / breaker runtime configuration (design §15.1 `[proxy]` / `[breaker]`),
//! parsed from the bootstrap config and held immutably by
//! [`crate::proxy::HydraProxy`].
//!
//! Defaults match the design (§15.1, §8.5, §8.3): soft body cap 8 MiB, hard
//! body cap 32 MiB, breaker threshold 5, probe interval 10 s. The
//! `non_route_strategy` selects passthrough vs reject for requests without a
//! `model` field (§6.3a).

use std::time::Duration;

use hydra_core::breaker::BreakerConfig;
use hydra_core::config::ConcurrencyPolicy;

/// Behaviour for requests that have no parseable `model` field (§6.3a).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NonRouteStrategy {
    /// Connect directly to the tenant's first live provider (default; serves
    /// health checks, webhooks and other model-less requests). Note:
    /// `GET /v1/models` is answered LOCALLY as the tenant model catalog
    /// (design-tenant-model-catalog §2.2) before this strategy is consulted,
    /// so it never reaches passthrough.
    #[default]
    Passthrough,
    /// Reject with 400.
    Reject,
}

/// Breaker policy (design §8.4 / §15.1 `[breaker]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BreakerPolicy {
    /// Consecutive-failure threshold for entering the dead-set.
    pub threshold: u32,
    /// Probe interval for the background revive task.
    pub probe_interval: Duration,
}

impl Default for BreakerPolicy {
    fn default() -> Self {
        Self {
            threshold: 5,
            probe_interval: Duration::from_secs(10),
        }
    }
}

impl BreakerPolicy {
    /// Build the core [`BreakerConfig`] (threshold only — the pure core owns no
    /// timing).
    #[must_use]
    pub fn to_core(&self) -> BreakerConfig {
        BreakerConfig::new(self.threshold)
    }
}

/// Proxy runtime config (design §15.1 `[proxy]` + `[breaker]`).
#[derive(Clone, Debug)]
pub struct ProxyConfig {
    /// Soft body cap: once exceeded, stop accumulating the replay buffer (the
    /// body still forwards untouched) and disable `error_while_proxy` retry
    /// (§8.5). Default 8 MiB.
    pub max_request_body: u64,
    /// Hard body cap: exceeding this returns 413 immediately and closes the
    /// connection (§8.5). Default 32 MiB.
    ///
    /// Configurable since 2026-09-29 (`HYDRA_MAX_REQUEST_BODY_HARD`): peak memory
    /// is roughly `concurrency x average body`, and `dev-docs/ops.md` §8 tells an
    /// operator with a small VPS to lower this cap — advice that was unactionable
    /// while the value came only from `ProxyConfig::default()`. Read at the single
    /// construction site in `main.rs`, like its timeout siblings.
    pub max_request_body_hard: u64,
    /// Behaviour for non-routable requests (no `model` field). Default
    /// passthrough.
    pub non_route_strategy: NonRouteStrategy,
    /// Breaker policy.
    pub breaker: BreakerPolicy,
    /// Default per-provider concurrency admission policy
    /// (design-admission-queue §5). Applied via `resolve_policy` when a
    /// provider leaves a field as `None`. The safe-rollout default is all
    /// zeros: `max_concurrency == 0` ⇒ `Permit::Passthrough` (no gating, no
    /// behaviour change for unconfigured providers — risk #1).
    pub default_concurrency_policy: ConcurrencyPolicy,
    /// Time-to-first-byte bound for one upstream attempt, in seconds
    /// (`HYDRA_UPSTREAM_FIRST_BYTE_TIMEOUT_SECS`, default 30).
    ///
    /// It bounds `send()` only — i.e. how long the upstream may take to produce
    /// response HEADERS — and deliberately does NOT bound the streaming body
    /// (see [`Self::upstream_stream_idle_timeout_secs`] for the body's bound).
    /// `0` is rejected at parse time: it would mean "time out immediately".
    pub upstream_first_byte_timeout_secs: u64,
    /// Bound on ESTABLISHING the connection to the upstream, in seconds
    /// (`HYDRA_UPSTREAM_CONNECT_TIMEOUT_SECS`, default 10).
    ///
    /// Must be **strictly below** [`Self::upstream_first_byte_timeout_secs`], and that is
    /// enforced at startup. Why it exists at all: there is no client-level total timeout on
    /// the upstream client (a total deadline truncates long SSE answers), so `send()` wraps
    /// the WHOLE `req.send()` — connect included — in the first-byte bound. Measured
    /// 2026-09-30 with a black-holed route (`192.0.2.1`, RFC 5737 TEST-NET-1): a provider
    /// whose SYN is dropped burned the entire first-byte bound and was then classified as a
    /// POST-send failure, so the gateway **refused to fail over** to a healthy provider that
    /// was right there — `codes=[502,200,502,200,502,200]`, `retries=0`, and the request was
    /// counted in `hydra_upstream_first_byte_timeout_total` even though the connection never
    /// completed. A connect bound turns that into an ordinary **connect error**, which is the
    /// one class `proxy.rs` treats as "the upstream never saw the request" and fails over.
    ///
    /// `0` is rejected at parse time (it would abort every connection immediately).
    pub upstream_connect_timeout_secs: u64,
    /// Idle bound on the upstream response BODY, in seconds
    /// (`HYDRA_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS`, default 120).
    ///
    /// The gap allowed between successive body chunks while streaming one
    /// response. It exists because the upstream client no longer carries a
    /// client-level total timeout: that deadline covered "until the response
    /// body has finished", so it silently truncated every generation longer
    /// than its 300 s (HTTP 200 + half an SSE body, billed in full). The idle
    /// window replaces it with a bound that cannot cut a live stream — an
    /// upstream emitting tokens at any rate keeps resetting it, while a wedged
    /// one is cut after this window and counted in
    /// `hydra_upstream_stream_idle_timeout_total{provider}`.
    ///
    /// There is intentionally **no total-exchange cap**: a legitimately long
    /// generation must not be truncated, and the client's own patience plus this
    /// idle window bound the resource. `0` is rejected at parse time.
    pub upstream_stream_idle_timeout_secs: u64,
    /// Total bound on reading ONE downstream request body, in seconds
    /// (`HYDRA_REQUEST_BODY_TIMEOUT_SECS`, default 60). Exceeding it is a
    /// `408 request_body_timeout` + close.
    ///
    /// This is a TOTAL deadline, not an idle one, because the threat it answers
    /// is a client that never finishes sending. Pingora bounds HTTP/1 body reads
    /// only per-read (each byte resets that) and HTTP/2 not at all, so a client
    /// dribbling one byte per minute held a worker task — and its buffered
    /// body — indefinitely. An idle bound cannot fix that; a total deadline can.
    ///
    /// The default is sized against `max_request_body_hard` (32 MiB): 60s admits
    /// a full-size body at ~0.5 MB/s, so an operator on a slower link must raise
    /// it deliberately instead of discovering the interaction as a 408.
    /// `0` is rejected at parse time: it would reject every bodied request.
    pub request_body_timeout_secs: u64,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            max_request_body: 8 * 1024 * 1024,
            max_request_body_hard: 32 * 1024 * 1024,
            non_route_strategy: NonRouteStrategy::Passthrough,
            breaker: BreakerPolicy::default(),
            // CRITICAL SAFETY PROPERTY: all zeros ⇒ `max_concurrency == 0` ⇒
            // every unconfigured provider gets `Permit::Passthrough` (no gate,
            // no block, no semaphore). This is the no-op default that preserves
            // the hot path for providers without configured concurrency limits.
            default_concurrency_policy: ConcurrencyPolicy {
                max_concurrency: 0,
                max_queue_depth: 0,
                queue_wait_timeout_ms: 0,
            },
            // 30s: long enough for a slow provider to start answering (TTFT for
            // a large model is routinely several seconds), short enough that a
            // wedged upstream fails over instead of burning the exchange.
            upstream_first_byte_timeout_secs: 30,
            // 10s to ESTABLISH a connection: generous for a healthy provider on any
            // realistic RTT, and far below the 30s first-byte bound so the connect phase
            // fails as a CONNECT error (⇒ failover) rather than as a post-send timeout.
            upstream_connect_timeout_secs: 10,
            // 120s between body chunks: LLM SSE flows token-by-token, so a gap
            // this long already means "wedged", while staying far above any
            // legitimate think-pause inside a stream.
            upstream_stream_idle_timeout_secs: 120,
            // 60s for the WHOLE downstream body: generous for a 32 MiB body at
            // ~0.5 MB/s, and far beyond what any honest client needs to send a
            // prompt. See the field docs for why this is a total, not idle,
            // deadline.
            request_body_timeout_secs: 60,
        }
    }
}

/// The configured first-byte bound: `HYDRA_UPSTREAM_FIRST_BYTE_TIMEOUT_SECS`,
/// defaulting to [`ProxyConfig::default`]'s value. Kept PURE so the tests do not
/// mutate the process environment.
#[must_use]
pub fn parse_upstream_first_byte_timeout_secs(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        // 0 would mean "time out immediately" — ignore it and fall back.
        .filter(|v| *v > 0)
        .unwrap_or(ProxyConfig::default().upstream_first_byte_timeout_secs)
}

/// The configured connect bound: `HYDRA_UPSTREAM_CONNECT_TIMEOUT_SECS`, defaulting to
/// [`ProxyConfig::default`]'s value. Pure, like its siblings. `0` would abort every
/// connection immediately, so it falls back to the default.
#[must_use]
pub fn parse_upstream_connect_timeout_secs(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(ProxyConfig::default().upstream_connect_timeout_secs)
}

/// Whether the connect bound can actually take effect: it must sit **strictly below** the
/// first-byte bound, because the first-byte timeout wraps `send()` (connect included) and
/// would otherwise always fire first — leaving the misclassification this bound exists to
/// remove, with no symptom other than the wrong HTTP status.
///
/// # Errors
/// The message to print before refusing to start.
pub fn check_upstream_connect_before_first_byte(
    connect_secs: u64,
    first_byte_secs: u64,
) -> Result<(), String> {
    if connect_secs >= first_byte_secs {
        return Err(format!(
            "HYDRA_UPSTREAM_CONNECT_TIMEOUT_SECS={connect_secs} must be strictly below \
             HYDRA_UPSTREAM_FIRST_BYTE_TIMEOUT_SECS={first_byte_secs}: the first-byte bound \
             wraps the whole send (connect included), so a connect bound at or above it can \
             never fire — a dead route would keep being reported as a post-send first-byte \
             timeout and the request would NOT fail over"
        ));
    }
    Ok(())
}

/// The configured upstream body idle bound:
/// `HYDRA_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS`, defaulting to
/// [`ProxyConfig::default`]'s value. Pure, like its first-byte sibling.
#[must_use]
pub fn parse_upstream_stream_idle_timeout_secs(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        // 0 would abort every stream at its first chunk gap.
        .filter(|v| *v > 0)
        .unwrap_or(ProxyConfig::default().upstream_stream_idle_timeout_secs)
}

/// The configured downstream request-body deadline:
/// `HYDRA_REQUEST_BODY_TIMEOUT_SECS`, defaulting to [`ProxyConfig::default`]'s
/// value. Pure, like its two siblings.
#[must_use]
pub fn parse_request_body_timeout_secs(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        // 0 would reject every bodied request instantly.
        .filter(|v| *v > 0)
        .unwrap_or(ProxyConfig::default().request_body_timeout_secs)
}

/// The configured hard body cap: `HYDRA_MAX_REQUEST_BODY_HARD`, defaulting to
/// [`ProxyConfig::default`]'s value. Pure, like its timeout siblings.
///
/// `0` is rejected and falls back: it would 413 every request that has a body at
/// all, which looks like a total outage, not a configuration. Values are BYTES
/// (not MiB) — the docs and the error message both speak bytes.
#[must_use]
pub fn parse_max_request_body_hard(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(ProxyConfig::default().max_request_body_hard)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The connect bound parse is TOTAL (garbage and `0` fall back to the default), and the
    /// DEFAULT must sit strictly below the default first-byte bound — otherwise the bound
    /// could never fire and the shipped configuration would be the broken one.
    #[test]
    fn upstream_connect_timeout_parse_is_total_and_fits_below_the_first_byte_bound() {
        let cfg = ProxyConfig::default();
        assert_eq!(parse_upstream_connect_timeout_secs(None), 10, "default");
        assert_eq!(parse_upstream_connect_timeout_secs(Some("5")), 5);
        assert_eq!(
            parse_upstream_connect_timeout_secs(Some(" 5 ")),
            5,
            "trimmed"
        );
        assert_eq!(
            parse_upstream_connect_timeout_secs(Some("0")),
            10,
            "0 would abort every connection immediately and must be rejected"
        );
        assert_eq!(parse_upstream_connect_timeout_secs(Some("nope")), 10);
        assert_eq!(parse_upstream_connect_timeout_secs(Some("-1")), 10);
        // The defaults must be a WORKING pair: strictly below, so check() accepts them.
        assert!(
            cfg.upstream_connect_timeout_secs < cfg.upstream_first_byte_timeout_secs,
            "default connect bound ({}) must be below the default first-byte bound ({})",
            cfg.upstream_connect_timeout_secs,
            cfg.upstream_first_byte_timeout_secs
        );
        assert!(check_upstream_connect_before_first_byte(
            cfg.upstream_connect_timeout_secs,
            cfg.upstream_first_byte_timeout_secs
        )
        .is_ok());
    }

    /// The connect bound can only take effect strictly below the first-byte bound (which wraps
    /// the whole `send()`, connect included). At or above it the bound is dead code whose only
    /// symptom is a wrong HTTP status, so startup must REFUSE the combination — and the
    /// message must name both variables, because that is what the operator has to change.
    #[test]
    fn a_connect_bound_that_cannot_fire_is_refused() {
        let err = check_upstream_connect_before_first_byte(30, 30)
            .expect_err("equal bounds must be refused");
        assert!(
            err.contains("HYDRA_UPSTREAM_CONNECT_TIMEOUT_SECS=30"),
            "{err}"
        );
        assert!(
            err.contains("HYDRA_UPSTREAM_FIRST_BYTE_TIMEOUT_SECS=30"),
            "{err}"
        );
        assert!(
            check_upstream_connect_before_first_byte(31, 30).is_err(),
            "above the first-byte bound is useless too"
        );
        assert!(
            check_upstream_connect_before_first_byte(1, 2).is_ok(),
            "strictly below is the one accepted shape"
        );
    }

    /// T9.1 — the first-byte bound parse is TOTAL: garbage and `0` fall back to
    /// the default, because `0` would mean "time out immediately" and every
    /// request would fail with a first-byte timeout.
    #[test]
    fn upstream_first_byte_timeout_parse_is_total() {
        assert_eq!(parse_upstream_first_byte_timeout_secs(None), 30, "default");
        assert_eq!(parse_upstream_first_byte_timeout_secs(Some("45")), 45);
        assert_eq!(
            parse_upstream_first_byte_timeout_secs(Some(" 45 ")),
            45,
            "trimmed"
        );
        assert_eq!(
            parse_upstream_first_byte_timeout_secs(Some("0")),
            30,
            "0 means 'time out immediately' and must be rejected"
        );
        assert_eq!(parse_upstream_first_byte_timeout_secs(Some("nope")), 30);
        assert_eq!(parse_upstream_first_byte_timeout_secs(Some("-1")), 30);
    }

    /// The hard body cap is documented as tunable (§8: "lower it to cut peak
    /// memory"), so the parse must be TOTAL and must reject `0` — a zero cap
    /// would 413 every bodied request, i.e. look like an outage.
    #[test]
    fn max_request_body_hard_parse_is_total() {
        assert_eq!(
            parse_max_request_body_hard(None),
            32 * 1024 * 1024,
            "default"
        );
        assert_eq!(
            parse_max_request_body_hard(Some("4194304")),
            4 * 1024 * 1024
        );
        assert_eq!(
            parse_max_request_body_hard(Some(" 2097152 ")),
            2 * 1024 * 1024,
            "trimmed"
        );
        assert_eq!(
            parse_max_request_body_hard(Some("0")),
            32 * 1024 * 1024,
            "0 would reject every bodied request; fall back"
        );
        assert_eq!(parse_max_request_body_hard(Some("nope")), 32 * 1024 * 1024);
        assert_eq!(parse_max_request_body_hard(Some("-1")), 32 * 1024 * 1024);
    }

    /// The default is the value the docs quote; drift here would make the
    /// documented timeout a lie.
    #[test]
    fn the_default_first_byte_timeout_is_thirty_seconds() {
        assert_eq!(ProxyConfig::default().upstream_first_byte_timeout_secs, 30);
    }

    /// The body idle bound parses totally (garbage/`0` fall back) and its
    /// default is the documented one. This bound is what makes removing the
    /// client-level total timeout safe, so a silent `0` would be a regressed
    /// worker-leak guard.
    #[test]
    fn upstream_stream_idle_timeout_parse_is_total() {
        assert_eq!(
            parse_upstream_stream_idle_timeout_secs(None),
            120,
            "default"
        );
        assert_eq!(parse_upstream_stream_idle_timeout_secs(Some("45")), 45);
        assert_eq!(parse_upstream_stream_idle_timeout_secs(Some(" 45 ")), 45);
        assert_eq!(
            parse_upstream_stream_idle_timeout_secs(Some("0")),
            120,
            "0 would abort every stream at its first chunk gap"
        );
        assert_eq!(parse_upstream_stream_idle_timeout_secs(Some("nope")), 120);
        assert_eq!(parse_upstream_stream_idle_timeout_secs(Some("-1")), 120);
        assert_eq!(
            ProxyConfig::default().upstream_stream_idle_timeout_secs,
            120
        );
    }

    /// The downstream request-body deadline parses totally and defaults to the
    /// documented value. A silent `0` here would 408 every bodied request, so
    /// the fallback matters as much as the parse.
    #[test]
    fn request_body_timeout_parse_is_total() {
        assert_eq!(parse_request_body_timeout_secs(None), 60, "default");
        assert_eq!(parse_request_body_timeout_secs(Some("90")), 90);
        assert_eq!(parse_request_body_timeout_secs(Some(" 90 ")), 90);
        assert_eq!(
            parse_request_body_timeout_secs(Some("0")),
            60,
            "0 would reject every bodied request"
        );
        assert_eq!(parse_request_body_timeout_secs(Some("nope")), 60);
        assert_eq!(parse_request_body_timeout_secs(Some("-1")), 60);
        assert_eq!(ProxyConfig::default().request_body_timeout_secs, 60);
    }
}
