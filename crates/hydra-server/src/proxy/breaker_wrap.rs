//! Concurrent circuit-breaker shell over the pure [`hydra_core::Breaker`]
//! state machine (design §8.4 / wave-4 §1).
//!
//! [`CircuitBreaker`] wraps the per-provider failure counter + dead-set in a
//! `DashMap`/`DashSet` so the hot path (`is_dead` from `router::resolve`) is
//! lock-free. It implements [`hydra_core::breaker::BreakerView`] so the pure
//! router can call it without depending on `dashmap`.
//!
//! A background [`probe_task`] periodically revives dead providers by issuing a
//! real HTTP `GET {endpoint}/v1/models` (falling back to a TCP connect). Both
//! are real I/O — never a mock of our own logic (dev-plan §1 铁律 2).

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use dashmap::DashSet;
use hydra_core::breaker::{BreakerConfig, BreakerView};
use tracing::{debug, info, warn};

/// Cluster-vote hook type (P4): fired on local trip / revive.
pub type ClusterHook = Arc<dyn Fn(&str) + Send + Sync>;

/// Concurrent circuit-breaker: lock-free dead-set + per-provider consecutive
/// failure counts (design §8.4).
///
/// The transition rules live in [`hydra_core::breaker::Breaker`]; this shell
/// only adds concurrency. `on_failure` increments the count and, on reaching
/// the threshold, inserts into the dead-set; `on_success` clears both. The
/// probe task (see [`probe_task`]) calls `on_success` to recover a dead
/// provider after a real HTTP/TCP probe succeeds.
pub struct CircuitBreaker {
    /// Locally-tripped providers (this node's own consecutive failures).
    dead: DashSet<String>,
    /// Cluster-wide dead providers (shared votes, synced from Redis, P4).
    cluster_dead: DashSet<String>,
    fails: DashMap<String, u32>,
    threshold: u32,
    /// Optional hook fired when a provider enters the local dead-set (the
    /// cluster votes it dead, P4). Sync `Fn`; async work is spawned by the
    /// hook body.
    on_trip: Option<ClusterHook>,
    /// Optional hook fired when a provider is revived locally (the node's
    /// vote is withdrawn, P4).
    on_revive: Option<ClusterHook>,
}

impl CircuitBreaker {
    /// Build a breaker from a [`BreakerConfig`] (the threshold is the only
    /// timing-independent parameter; probe interval / cooldown are owned by the
    /// shell's probe task, §8.4).
    #[must_use]
    pub fn new(cfg: BreakerConfig) -> Self {
        Self {
            dead: DashSet::new(),
            cluster_dead: DashSet::new(),
            fails: DashMap::new(),
            threshold: cfg.threshold,
            on_trip: None,
            on_revive: None,
        }
    }

    /// Wire the cluster vote hooks (P4): fired on local trip / revive.
    pub fn set_cluster_hooks(
        &mut self,
        on_trip: Option<ClusterHook>,
        on_revive: Option<ClusterHook>,
    ) {
        self.on_trip = on_trip;
        self.on_revive = on_revive;
    }

    /// Reconcile the cluster-wide dead set from the shared votes (P4):
    /// providers with a live cluster vote are excluded from candidates even
    /// before this node trips locally. Providers whose votes lapsed are
    /// removed.
    pub fn apply_cluster_dead(&self, providers: &std::collections::HashSet<String>) {
        self.cluster_dead.retain(|p| providers.contains(p));
        for p in providers {
            if !self.cluster_dead.contains(p) {
                self.cluster_dead.insert(p.clone());
                crate::admin::metrics::record_breaker_dead(p, 1);
            }
        }
    }

    /// Number of consecutive failures recorded for `provider_id` (0 if unseen).
    #[must_use]
    pub fn fail_count(&self, provider_id: &str) -> u32 {
        self.fails.get(provider_id).map(|v| *v).unwrap_or(0)
    }

    /// Record a failure for `provider_id`. Once the **consecutive** failure
    /// count reaches the threshold, the provider enters the dead-set (§8.4).
    pub fn on_failure(&self, provider_id: &str) {
        let mut entry = self.fails.entry(provider_id.to_string()).or_insert(0);
        *entry += 1;
        let count = *entry;
        drop(entry);
        if count >= self.threshold {
            if !self.dead.contains(provider_id) {
                self.dead.insert(provider_id.to_string());
                // Metrics (§17): breaker just went dead.
                crate::admin::metrics::record_breaker_dead(provider_id, 1);
                crate::admin::metrics::record_breaker_transition(provider_id, "dead");
                warn!(
                    provider = provider_id,
                    consecutive_failures = count,
                    threshold = self.threshold,
                    "circuit breaker OPENED for provider"
                );
                // Cluster vote (P4): tell every node this provider is dead.
                if let Some(hook) = &self.on_trip {
                    hook(provider_id);
                }
            }
        } else {
            debug!(
                provider = provider_id,
                consecutive_failures = count,
                "breaker failure recorded"
            );
        }
    }

    /// Record a success for `provider_id`: resets its consecutive-failure
    /// counter and removes it from the dead-set (consecutive semantics — a
    /// single success resets the streak). The probe task calls this on a
    /// successful probe to recover a provider.
    pub fn on_success(&self, provider_id: &str) {
        self.fails.remove(provider_id);
        if self.dead.remove(provider_id).is_some() {
            // Metrics (§17): breaker just revived.
            crate::admin::metrics::record_breaker_dead(provider_id, 0);
            crate::admin::metrics::record_breaker_transition(provider_id, "alive");
            info!(
                provider = provider_id,
                "circuit breaker CLOSED (provider revived)"
            );
            // Withdraw the cluster vote (P4).
            if let Some(hook) = &self.on_revive {
                hook(provider_id);
            }
        }
    }

    /// Whether `provider_id` is currently dead (excluded from candidates):
    /// locally tripped OR voted dead cluster-wide (P4).
    #[must_use]
    pub fn is_dead(&self, provider_id: &str) -> bool {
        self.dead.contains(provider_id) || self.cluster_dead.contains(provider_id)
    }

    /// The LOCALLY-tripped providers only (the node's own votes; the cluster
    /// heartbeat must re-vote based on this, not on the cluster view — else a
    /// single vote would keep itself alive forever).
    #[must_use]
    pub fn locally_dead_providers(&self) -> Vec<String> {
        self.dead.iter().map(|v| v.clone()).collect()
    }

    /// Snapshot of the dead-set (local ∪ cluster, for introspection / probes).
    #[must_use]
    pub fn dead_providers(&self) -> Vec<String> {
        let mut out: Vec<String> = self.dead.iter().map(|v| v.clone()).collect();
        for p in self.cluster_dead.iter() {
            if !out.contains(&*p) {
                out.push(p.clone());
            }
        }
        out
    }

    /// Remove breaker entries for providers no longer in the config (called on
    /// reload by the shell to prune deleted providers, §8.4).
    pub fn prune_to(&self, live_provider_ids: &std::collections::HashSet<String>) {
        self.dead.retain(|p| live_provider_ids.contains(p));
        self.cluster_dead.retain(|p| live_provider_ids.contains(p));
        self.fails.retain(|p, _| live_provider_ids.contains(p));
    }
}

impl BreakerView for CircuitBreaker {
    fn is_dead(&self, provider_id: &str) -> bool {
        CircuitBreaker::is_dead(self, provider_id)
    }
}

// Note: we intentionally do NOT implement `BreakerView for Arc<CircuitBreaker>`
// here (orphan rule — both the trait and the type would be foreign). Callers
// pass `breaker.as_ref()` to `router::resolve`, which derefs the Arc.

// ---------------------------------------------------------------------------
// Background probe task (design §8.4)
// ---------------------------------------------------------------------------

/// Spawn a background task that periodically probes dead providers and revives
/// the ones that respond. Uses a real HTTP `GET {endpoint}/v1/models` with a
/// short timeout, falling back to a plain TCP connect probe when the HTTP probe
/// cannot be built (no reqwest at this layer would be ideal, but the proxy
/// module already depends on reqwest via the `http-client` feature under
/// `server`). The endpoint map is read from the snapshot each tick so reloads
/// take effect without restarting the task.
///
/// Returns immediately; the task runs until the runtime shuts down.
pub fn spawn_probe_task(
    breaker: Arc<CircuitBreaker>,
    snapshot_provider: Arc<dyn Fn() -> Vec<(String, String)> + Send + Sync>,
    probe_interval: Duration,
) {
    tokio::spawn(probe_task(breaker, snapshot_provider, probe_interval));
}

/// The probe loop body, factored out so it is testable as a plain future.
///
/// `snapshot_provider` returns the current `(provider_id, endpoint)` pairs for
/// dead providers (the closure filters the live config snapshot to dead
/// providers); a fresh reqwest client is built once.
pub async fn probe_task(
    breaker: Arc<CircuitBreaker>,
    snapshot_provider: Arc<dyn Fn() -> Vec<(String, String)> + Send + Sync>,
    probe_interval: Duration,
) {
    // Build the probe client once. A short connect+read timeout so a dead host
    // is confirmed quickly without blocking the task.
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_millis(1500))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "failed to build probe reqwest client; probe task exiting");
            return;
        }
    };

    let mut ticker = tokio::time::interval(probe_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Skip the immediate first tick so we don't probe before the server has
    // even finished booting up the first snapshot.
    ticker.tick().await;

    loop {
        ticker.tick().await;
        let dead = breaker.dead_providers();
        if dead.is_empty() {
            continue;
        }
        let candidates = snapshot_provider();
        for (pid, endpoint) in candidates {
            if !breaker.is_dead(&pid) {
                continue;
            }
            if probe_one(&client, &endpoint).await {
                breaker.on_success(&pid);
            }
        }
    }
}

/// One probe: `GET {endpoint}/v1/models`, success on any HTTP response whose
/// status is not a connection-level failure (i.e. the upstream answered — even
/// a 401 means the host is alive). Falls back to a TCP connect on reqwest
/// errors. Real I/O only; no mocks.
async fn probe_one(client: &reqwest::Client, endpoint: &str) -> bool {
    let url = format!("{}/v1/models", endpoint.trim_end_matches('/'));
    match client.get(&url).send().await {
        // Only a healthy response revives the provider. A 5xx (or anything
        // >= 500) means the upstream is still failing — reviving on any
        // response made a 500-ing provider flap: trip → probe "revives" it
        // seconds later → 5 more failures → trip again.
        Ok(resp) => resp.status().as_u16() < 500,
        Err(_) => {
            // HTTP probe failed — try a bare TCP connect as a last resort.
            tcp_probe(endpoint).await
        }
    }
}

/// Bare TCP connect probe: parse host:port out of the endpoint and try to open
/// a connection. Real socket I/O.
async fn tcp_probe(endpoint: &str) -> bool {
    let stripped = endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"))
        .unwrap_or(endpoint);
    let authority = stripped.split(['/', '?', '#']).next().unwrap_or(stripped);
    let addr = match authority.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() => format!("{h}:{p}"),
        _ => {
            let scheme_https = endpoint.starts_with("https://");
            let port = if scheme_https { "443" } else { "80" };
            format!("{authority}:{port}")
        }
    };
    tokio::net::TcpStream::connect(addr).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_dead_after_threshold() {
        let b = CircuitBreaker::new(BreakerConfig::new(3));
        assert!(!b.is_dead("p1"));
        b.on_failure("p1");
        b.on_failure("p1");
        assert!(!b.is_dead("p1"));
        b.on_failure("p1");
        assert!(b.is_dead("p1"));
    }

    #[test]
    fn on_success_revives() {
        let b = CircuitBreaker::new(BreakerConfig::new(2));
        b.on_failure("p1");
        b.on_failure("p1");
        assert!(b.is_dead("p1"));
        b.on_success("p1");
        assert!(!b.is_dead("p1"));
        assert_eq!(b.fail_count("p1"), 0);
    }

    #[test]
    fn success_resets_streak() {
        let b = CircuitBreaker::new(BreakerConfig::new(3));
        b.on_failure("p1");
        b.on_failure("p1");
        b.on_success("p1");
        b.on_failure("p1");
        b.on_failure("p1");
        assert!(!b.is_dead("p1"), "streak reset means 2 < 3");
    }

    #[test]
    fn breaker_view_for_arc() {
        let b = Arc::new(CircuitBreaker::new(BreakerConfig::new(1)));
        b.on_failure("p1");
        let view: &dyn BreakerView = b.as_ref();
        assert!(view.is_dead("p1"));
        assert!(!view.is_dead("p2"));
    }

    #[test]
    fn prune_to_removes_unknown() {
        use std::collections::HashSet;
        let b = CircuitBreaker::new(BreakerConfig::new(1));
        b.on_failure("gone");
        b.on_failure("live");
        let mut live = HashSet::new();
        live.insert("live".to_string());
        b.prune_to(&live);
        assert!(b.fail_count("gone") == 0);
        assert!(b.fail_count("live") == 1);
    }
}
