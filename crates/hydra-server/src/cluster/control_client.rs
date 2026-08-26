//! # Control-plane client (cluster P1)
//!
//! Generic snapshot-polling task shared by edge (and, from P2, standby)
//! nodes: polls the leader's internal control endpoint
//! `GET /api/v1/internal/control?since=<version>`, hydrates the snapshot
//! locally (sealed secrets decrypted with the fleet-wide master key), and
//! applies it to the [`ConfigStore`].
//!
//! **Last-known-good semantics**: any failure (endpoint down, auth rejected,
//! decrypt error, malformed payload) leaves the current snapshot untouched
//! and retries with backoff — the data plane never depends on the control
//! plane being reachable, and the config can never regress to a partial
//! state.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tracing::{info, warn};

use hydra_core::config::ConfigData;

use crate::admin::metrics;
use crate::cluster::snapshot::SnapshotWire;
use crate::crypto::KeyProvider;
use crate::store::ConfigStore;

/// Control-channel response: `snapshot` is present only when the caller's
/// `since` version is older than the leader's current one.
#[derive(Debug, Deserialize)]
pub struct ControlResponse {
    pub version: u64,
    pub snapshot: Option<SnapshotWire>,
}

/// Control-client settings (cluster P1, from env in `main`).
#[derive(Clone, Debug)]
pub struct ControlClientConfig {
    /// Leader control endpoint base, e.g. `http://hydra-control:8081`.
    pub url: String,
    /// Shared control-plane token (`HYDRA_CLUSTER_TOKEN`).
    pub token: String,
    /// Poll interval (`HYDRA_CONTROL_POLL_MS`; default 1000 ms).
    pub poll_interval: Duration,
}

/// Outcome of one poll, delivered to the optional `on_poll` hook.
#[derive(Debug, Clone)]
pub enum PollOutcome {
    /// Poll succeeded; no newer snapshot (already current).
    UpToDate,
    /// Poll succeeded and a newer snapshot was applied.
    Applied(Box<SnapshotWire>),
    /// Poll failed (network / auth / hydrate). Last-known-good kept.
    Error,
}

/// Per-poll hook type (kept as an alias to keep the field types readable).
pub type PollHook = Arc<dyn Fn(&PollOutcome) + Send + Sync>;

/// The polling control client. Cheap to `Clone` (all fields are `Arc`).
#[derive(Clone)]
pub struct ControlClient {
    config: ControlClientConfig,
    /// The current poll target (swapped by registry rotation on failure).
    url: Arc<std::sync::Mutex<String>>,
    store: ConfigStore,
    key_provider: Arc<dyn KeyProvider>,
    client: reqwest::Client,
    /// Optional per-poll hook: standby nodes use it to materialize the
    /// replica SQLite (`Applied`) and to drive the election freshness gate
    /// (`UpToDate`/`Applied` ⇒ sync-ok, `Error` ⇒ stale). Edges pass `None`.
    on_poll: Option<PollHook>,
    /// Optional registry (P4): on poll failure the client re-resolves the
    /// live leader control endpoints and rotates — so an edge follows the
    /// lease across a leader failover without static reconfiguration.
    #[cfg(feature = "cluster-redis")]
    discovery: Option<Arc<crate::cluster::registry::NodeRegistry>>,
    /// Placeholder (single-node builds never attach discovery).
    #[cfg(not(feature = "cluster-redis"))]
    #[allow(dead_code)]
    discovery: Option<()>,
}

impl ControlClient {
    /// Build the client. `on_poll` receives every poll outcome (see
    /// [`PollOutcome`]); `None` for plain edges.
    #[must_use]
    pub fn new(
        config: ControlClientConfig,
        store: ConfigStore,
        key_provider: Arc<dyn KeyProvider>,
        on_poll: Option<PollHook>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .pool_idle_timeout(Some(Duration::from_secs(90)))
            .tcp_nodelay(true)
            .build()
            .expect("control client reqwest build (infallible)");
        Self {
            url: Arc::new(std::sync::Mutex::new(config.url.clone())),
            config,
            store,
            key_provider,
            client,
            on_poll,
            discovery: None,
        }
    }

    /// Attach registry-based discovery: on poll failure the client rotates to
    /// a live leader's control endpoint (cluster P4 self-sustaining failover).
    #[cfg(feature = "cluster-redis")]
    #[must_use]
    pub fn with_discovery(mut self, registry: Arc<crate::cluster::registry::NodeRegistry>) -> Self {
        self.discovery = Some(registry);
        self
    }

    /// Re-resolve the poll target from the registry (live leaders only).
    /// Returns true when the target changed.
    ///
    /// Prefers a live leader **different from the current (failing) target**:
    /// with several live candidates the lexicographically-first URL may be
    /// the dead active, which would pin us to it until its heartbeat expires
    /// (30 s) — inflating failover from ~lease expiry to ~lease + heartbeat.
    /// Rotating to ANY live candidate is safe: the lease machine arbitrates
    /// who actually writes, and a standby's replica is last-known-good.
    #[cfg(feature = "cluster-redis")]
    pub async fn rotate_from_registry(&self) -> bool {
        let Some(reg) = &self.discovery else {
            return false;
        };
        let Ok(urls) = reg.leader_control_urls().await else {
            return false;
        };
        let mut guard = self.url.lock().expect("control url mutex");
        let current = guard.clone();
        let new_url = urls
            .iter()
            .find(|u| u.as_str() != current)
            .cloned()
            .or_else(|| urls.first().cloned());
        let Some(new_url) = new_url else {
            return false;
        };
        if *guard != new_url {
            tracing::info!(from = %*guard, to = %new_url, "control poll target rotated");
            *guard = new_url.clone();
            true
        } else {
            false
        }
    }

    /// Rotate to the CURRENT lease holder when this node's poll target is
    /// not it. A successful poll is not proof the target is right: a
    /// rejoining leader whose static `HYDRA_CONTROL_URL` points at ITSELF
    /// polls itself happily and never replicates from the new active — its
    /// replica stays stale and a later promotion would regress the config.
    /// Following the lease holder keeps every standby's replica current.
    /// Returns true when the target changed.
    #[cfg(feature = "cluster-redis")]
    pub async fn rotate_to_lease_holder(&self) -> bool {
        let Some(reg) = &self.discovery else {
            return false;
        };
        let Ok(Some(url)) = reg.active_leader_url().await else {
            return false;
        };
        let mut guard = self.url.lock().expect("control url mutex");
        if *guard != url {
            tracing::info!(from = %*guard, to = %url, "control poll target rotated to lease holder");
            *guard = url;
            true
        } else {
            false
        }
    }

    /// Spawn the poll loop on the current tokio runtime (the background
    /// runtime in `main`). The task runs for the process lifetime.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(self.run())
    }

    /// The poll loop: poll immediately, then every `poll_interval`, with
    /// exponential backoff (cap 30 s) on failures.
    async fn run(self) {
        let base = self.config.poll_interval;
        let mut failures: u32 = 0;
        loop {
            match self.poll_once().await {
                Ok(()) => {
                    failures = 0;
                    metrics::record_control_poll("ok");
                    // Lease-aware rotation (P4): even a SUCCESSFUL poll can be
                    // against the wrong target (a rejoining standby polling
                    // itself) — follow the lease holder so the replica is
                    // never stale.
                    #[cfg(feature = "cluster-redis")]
                    {
                        let _ = self.rotate_to_lease_holder().await;
                    }
                }
                Err(e) => {
                    failures = failures.saturating_add(1);
                    metrics::record_control_poll("error");
                    warn!(
                        failures,
                        error = %e,
                        "control poll failed; keeping last-known-good snapshot"
                    );
                    // Leader failover: re-resolve the live leader from the
                    // registry so the next poll follows the new active.
                    #[cfg(feature = "cluster-redis")]
                    if failures >= 2 {
                        self.rotate_from_registry().await;
                    }
                }
            }
            let backoff = base.saturating_mul(2u32.saturating_pow(failures.min(5)));
            tokio::time::sleep(backoff.min(Duration::from_secs(30))).await;
        }
    }

    /// One poll: fetch `?since=<local version>`; when the leader has a newer
    /// snapshot, hydrate + apply it. Public so integration tests can drive a
    /// single poll deterministically.
    pub async fn poll_once(&self) -> Result<(), String> {
        let err = |msg: String| {
            if let Some(hook) = &self.on_poll {
                hook(&PollOutcome::Error);
            }
            Err(msg)
        };
        let since = self.store.version();
        let base = self.url.lock().expect("control url mutex").clone();
        let url = format!(
            "{}/api/v1/internal/control?since={since}",
            base.trim_end_matches('/')
        );
        let resp = match self
            .client
            .get(&url)
            .header("authorization", format!("Bearer {}", self.config.token))
            .timeout(
                self.config
                    .poll_interval
                    .saturating_mul(3)
                    .max(Duration::from_secs(5)),
            )
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return err(format!("control request failed: {e}")),
        };
        if !resp.status().is_success() {
            return err(format!("control endpoint returned {}", resp.status()));
        }
        let body: ControlResponse = match resp.json().await {
            Ok(b) => b,
            Err(e) => return err(format!("control response parse failed: {e}")),
        };

        let Some(wire) = body.snapshot else {
            if let Some(hook) = &self.on_poll {
                hook(&PollOutcome::UpToDate);
            }
            return Ok(()); // already up to date
        };
        if body.version <= since {
            if let Some(hook) = &self.on_poll {
                hook(&PollOutcome::UpToDate);
            }
            return Ok(()); // defensive: never apply a non-newer version
        }
        let cfg: ConfigData = match wire.clone().hydrate(self.key_provider.as_ref()) {
            Ok(c) => c,
            Err(e) => return err(format!("snapshot hydrate failed (wrong master key?): {e}")),
        };
        self.store.apply_snapshot(cfg, body.version);
        if let Some(hook) = &self.on_poll {
            hook(&PollOutcome::Applied(Box::new(wire.clone())));
        }
        metrics::record_control_snapshot_version(body.version);
        info!(version = body.version, "control snapshot applied");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "cluster-redis")]
    use fred::prelude::*;

    #[cfg(feature = "cluster-redis")]
    #[tokio::test]
    async fn rotate_from_registry_follows_leader() {
        // A live leader registered in the (mock) registry; the client's poll
        // target rotates to it.
        let mock = std::sync::Arc::new(crate::redis::mock::MockRedis::new());
        let cfg = Config {
            mocks: Some(mock),
            ..Default::default()
        };
        let pool = Pool::new(cfg, None, None, None, 1).expect("pool");
        pool.init().await.expect("init");
        let reg = std::sync::Arc::new(crate::cluster::registry::NodeRegistry::new(
            pool.clone(),
            "edge".into(),
            crate::cluster::NodeRole::Edge,
            String::new(),
        ));
        // Register a live leader with a pollable URL.
        let leader = crate::cluster::registry::NodeRegistry::new(
            pool.clone(),
            "leader-a".into(),
            crate::cluster::NodeRole::Leader,
            "http://leader-a:8081".into(),
        );
        leader.register(60).await.expect("register leader");

        let client = ControlClient::new(
            ControlClientConfig {
                url: "http://dead:8081".into(),
                token: "t".into(),
                poll_interval: Duration::from_secs(1),
            },
            crate::store::ConfigStore::from_snapshot(
                hydra_core::config::ConfigData::default(),
                std::sync::Arc::new(crate::crypto::StaticKeyProvider::new([1u8; 32], 1)),
            ),
            std::sync::Arc::new(crate::crypto::StaticKeyProvider::new([1u8; 32], 1)),
            None,
        )
        .with_discovery(reg);
        assert!(
            client.rotate_from_registry().await,
            "rotates to the registered leader"
        );
        let url = client.url.lock().expect("url");
        assert_eq!(url.as_str(), "http://leader-a:8081");
    }

    #[cfg(feature = "cluster-redis")]
    #[tokio::test]
    async fn rotate_skips_the_dead_current_target() {
        // Two LIVE leaders; the current poll target is the first (sorted)
        // one — the dead active. Rotation must move to the OTHER leader,
        // not stay pinned to the failing URL.
        let mock = std::sync::Arc::new(crate::redis::mock::MockRedis::new());
        let cfg = Config {
            mocks: Some(mock),
            ..Default::default()
        };
        let pool = Pool::new(cfg, None, None, None, 1).expect("pool");
        pool.init().await.expect("init");
        let reg = std::sync::Arc::new(crate::cluster::registry::NodeRegistry::new(
            pool.clone(),
            "edge".into(),
            crate::cluster::NodeRole::Edge,
            String::new(),
        ));
        // Both "a" and "b" are live leaders; "a" sorts first.
        let a = crate::cluster::registry::NodeRegistry::new(
            pool.clone(),
            "a".into(),
            crate::cluster::NodeRole::Leader,
            "http://a:8081".into(),
        );
        let b = crate::cluster::registry::NodeRegistry::new(
            pool.clone(),
            "b".into(),
            crate::cluster::NodeRole::Leader,
            "http://b:8081".into(),
        );
        a.register(60).await.expect("register a");
        b.register(60).await.expect("register b");

        let client = ControlClient::new(
            ControlClientConfig {
                url: "http://a:8081".into(), // current target == sorted-first live leader
                token: "t".into(),
                poll_interval: Duration::from_secs(1),
            },
            crate::store::ConfigStore::from_snapshot(
                hydra_core::config::ConfigData::default(),
                std::sync::Arc::new(crate::crypto::StaticKeyProvider::new([1u8; 32], 1)),
            ),
            std::sync::Arc::new(crate::crypto::StaticKeyProvider::new([1u8; 32], 1)),
            None,
        )
        .with_discovery(reg);
        assert!(
            client.rotate_from_registry().await,
            "rotates away from the failing sorted-first target"
        );
        let url = client.url.lock().expect("url");
        assert_eq!(
            url.as_str(),
            "http://b:8081",
            "must prefer a different live leader"
        );
    }

    #[cfg(feature = "cluster-redis")]
    #[tokio::test]
    async fn rotate_to_lease_holder_follows_active() {
        // The lease is held by "leader-a" (registered in the registry); the
        // client's poll target is a different (reachable!) node — e.g. a
        // rejoining standby pointing at itself. Lease-aware rotation must
        // move the target to the actual holder.
        let mock = std::sync::Arc::new(crate::redis::mock::MockRedis::new());
        let cfg = Config {
            mocks: Some(mock),
            ..Default::default()
        };
        let pool = Pool::new(cfg, None, None, None, 1).expect("pool");
        pool.init().await.expect("init");
        let reg = std::sync::Arc::new(crate::cluster::registry::NodeRegistry::new(
            pool.clone(),
            "standby".into(),
            crate::cluster::NodeRole::Leader,
            "http://standby:8081".into(),
        ));
        let leader = crate::cluster::registry::NodeRegistry::new(
            pool.clone(),
            "leader-a".into(),
            crate::cluster::NodeRole::Leader,
            "http://leader-a:8081".into(),
        );
        leader.register(60).await.expect("register leader");
        // leader-a holds the leader lease.
        let _: Option<String> = pool
            .set(crate::redis::LEASE_KEY, "leader-a", None, None, false)
            .await
            .expect("set lease");

        let client = ControlClient::new(
            ControlClientConfig {
                url: "http://standby:8081".into(), // polls ITSELF — reachable, wrong
                token: "t".into(),
                poll_interval: Duration::from_secs(1),
            },
            crate::store::ConfigStore::from_snapshot(
                hydra_core::config::ConfigData::default(),
                std::sync::Arc::new(crate::crypto::StaticKeyProvider::new([1u8; 32], 1)),
            ),
            std::sync::Arc::new(crate::crypto::StaticKeyProvider::new([1u8; 32], 1)),
            None,
        )
        .with_discovery(reg);
        assert!(
            client.rotate_to_lease_holder().await,
            "rotates to the lease holder even though the current target is reachable"
        );
        let url = client.url.lock().expect("url");
        assert_eq!(url.as_str(), "http://leader-a:8081");
    }

    #[test]
    fn backoff_caps_at_30s() {
        // Mirrors the loop's backoff computation.
        let base = Duration::from_millis(100);
        let compute = |f: u32| {
            base.saturating_mul(2u32.saturating_pow(f.min(5)))
                .min(Duration::from_secs(30))
        };
        assert_eq!(compute(0), Duration::from_millis(100));
        assert_eq!(compute(1), Duration::from_millis(200));
        assert_eq!(compute(5), Duration::from_millis(3200));
        assert_eq!(compute(20), Duration::from_millis(3200), "capped by 2^5");
        let big = Duration::from_secs(10);
        let compute_big = |f: u32| {
            big.saturating_mul(2u32.saturating_pow(f.min(5)))
                .min(Duration::from_secs(30))
        };
        assert_eq!(compute_big(2), Duration::from_secs(30), "capped at 30s");
    }
}
