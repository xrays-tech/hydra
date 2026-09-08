//! Proxy / failover runtime configuration (design §15.1 `[proxy]` / `[failover]`
//! / `[breaker]`), parsed from the bootstrap config and held immutably by
//! [`crate::proxy::HydraProxy`].
//!
//! Defaults match the design (§15.1, §8.5, §8.3): soft body cap 8 MiB, hard
//! body cap 32 MiB, `retry_after_connect = false`, breaker threshold 5, probe
//! interval 10 s. The `non_route_strategy` selects passthrough vs reject for
//! requests without a `model` field (§6.3a).

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

/// Failover policy (design §8.3 / §15.1 `[failover]`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FailoverConfig {
    /// Whether to retry on errors *after* a connection was established to the
    /// upstream. Default `false` (safety-first: avoid double billing for
    /// non-idempotent LLM requests). When `true`, retry is still gated by
    /// `upstream_bytes_seen == 0` and `body_replayable` (§8.3).
    pub retry_after_connect: bool,
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

/// Proxy runtime config (design §15.1 `[proxy]` + `[failover]` + `[breaker]`).
#[derive(Clone, Debug)]
pub struct ProxyConfig {
    /// Soft body cap: once exceeded, stop accumulating the replay buffer (the
    /// body still forwards untouched) and disable `error_while_proxy` retry
    /// (§8.5). Default 8 MiB.
    pub max_request_body: u64,
    /// Hard body cap: exceeding this returns 413 immediately and closes the
    /// connection (§8.5). Default 32 MiB.
    pub max_request_body_hard: u64,
    /// Behaviour for non-routable requests (no `model` field). Default
    /// passthrough.
    pub non_route_strategy: NonRouteStrategy,
    /// Failover policy.
    pub failover: FailoverConfig,
    /// Breaker policy.
    pub breaker: BreakerPolicy,
    /// Default per-provider concurrency admission policy
    /// (design-admission-queue §5). Applied via `resolve_policy` when a
    /// provider leaves a field as `None`. The safe-rollout default is all
    /// zeros: `max_concurrency == 0` ⇒ `Permit::Passthrough` (no gating, no
    /// behaviour change for unconfigured providers — risk #1).
    pub default_concurrency_policy: ConcurrencyPolicy,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            max_request_body: 8 * 1024 * 1024,
            max_request_body_hard: 32 * 1024 * 1024,
            non_route_strategy: NonRouteStrategy::Passthrough,
            failover: FailoverConfig::default(),
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
        }
    }
}
