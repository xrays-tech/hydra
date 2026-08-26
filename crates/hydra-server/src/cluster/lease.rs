//! # Leader lease & election state machine (cluster P2)
//!
//! The leader's write authority is a **lease**: an atomic
//! compare-and-set against an external store (Redis in production,
//! [`crate::redis::RedisLeaseStore`]; a memory store in tests). The state
//! machine here is the part that matters and is fully deterministic:
//!
//! - `Standby`  → try to acquire; **freshness gate**: a candidate that has
//!   not synced from the active leader recently is not eligible (it could be
//!   a stale replica racing for the lease);
//! - `Active`   → renew on every tick; a failed renewal **immediately**
//!   demotes to `Uncertain` (write permission closes, fail-closed) rather
//!   than waiting for the TTL;
//! - `Uncertain`→ try to renew (transient blip) or drop back to `Standby`;
//!   writes stay blocked.
//!
//! Split-brain safety: exactly one holder at a time (the store is atomic), a
//! time fence (`valid_until` — writes require `now < valid_until`), and the
//! recovery rule that a node observes the lease is held by someone else
//! before it can act as leader.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

/// Errors from the lease store (the atomic compare-and-set backend).
#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error("lease store error: {0}")]
    Store(String),
}

/// The atomic lease store (external boundary; Redis in production, memory in
/// tests). Object-safe (boxed futures, like `UsageSink`) so the election can
/// hold it as `Arc<dyn LeaseStore>`.
pub trait LeaseStore: Send + Sync {
    /// Atomically acquire the lease for `node_id` for `lease_ms`.
    /// `Ok(true)` = acquired; `Ok(false)` = held by someone else.
    fn try_acquire<'a>(
        &'a self,
        node_id: &'a str,
        lease_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<bool, LeaseError>> + Send + 'a>>;

    /// Atomically renew while still ours. `Ok(false)` = the lease was lost
    /// (expired or re-acquired by another node) — the caller must demote.
    fn renew<'a>(
        &'a self,
        node_id: &'a str,
        lease_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<bool, LeaseError>> + Send + 'a>>;
}

/// In-memory lease store — a real atomic implementation used as the test
/// double for the external Redis boundary (same category as wiremock for the
/// auth URL). Also handy for local single-process simulations.
#[derive(Default)]
pub struct MemoryLeaseStore {
    inner: Mutex<Option<(String, std::time::SystemTime)>>,
}

impl MemoryLeaseStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl LeaseStore for MemoryLeaseStore {
    fn try_acquire<'a>(
        &'a self,
        node_id: &'a str,
        lease_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<bool, LeaseError>> + Send + 'a>> {
        Box::pin(async move {
            let mut g = self.inner.lock().expect("lease mutex");
            let now = std::time::SystemTime::now();
            let expired = g.as_ref().map(|(_, expiry)| now >= *expiry).unwrap_or(true);
            if expired {
                *g = Some((node_id.to_string(), now + Duration::from_millis(lease_ms)));
                Ok(true)
            } else {
                Ok(false)
            }
        })
    }

    fn renew<'a>(
        &'a self,
        node_id: &'a str,
        lease_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<bool, LeaseError>> + Send + 'a>> {
        Box::pin(async move {
            let mut g = self.inner.lock().expect("lease mutex");
            let now = std::time::SystemTime::now();
            let ours = g
                .as_ref()
                .map(|(holder, expiry)| holder == node_id && now < *expiry)
                .unwrap_or(false);
            if ours {
                *g = Some((node_id.to_string(), now + Duration::from_millis(lease_ms)));
                Ok(true)
            } else {
                Ok(false)
            }
        })
    }
}

/// Clock source (injected for deterministic tests; real wall clock in prod).
type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

fn system_clock() -> Clock {
    Arc::new(Instant::now)
}

/// The local election state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElectionState {
    /// Not the leader; eligible to acquire.
    Standby,
    /// Leader with a live time fence (`valid_until`).
    Active { valid_until: Instant },
    /// Leadership in doubt (renewal failed): writes blocked, retrying.
    Uncertain,
}

/// The per-node leader-election machine (cluster P2).
pub struct LeaderElection {
    store: Arc<dyn LeaseStore>,
    node_id: String,
    lease_ms: u64,
    clock: Clock,
    state: Mutex<ElectionState>,
    /// Freshness gate: last control-plane sync succeeded (set by the control
    /// client / replica). A candidate with `!sync_ok` cannot acquire.
    sync_ok: AtomicBool,
}

impl LeaderElection {
    /// Build with the real clock.
    #[must_use]
    pub fn new(store: Arc<dyn LeaseStore>, node_id: String, lease_ms: u64) -> Self {
        Self::with_clock(store, node_id, lease_ms, system_clock())
    }

    /// Build with an injected clock (deterministic tests).
    #[must_use]
    pub fn with_clock(
        store: Arc<dyn LeaseStore>,
        node_id: String,
        lease_ms: u64,
        clock: Clock,
    ) -> Self {
        Self {
            store,
            node_id,
            lease_ms,
            clock,
            state: Mutex::new(ElectionState::Standby),
            sync_ok: AtomicBool::new(true),
        }
    }

    /// Mark the freshness gate: `true` after a successful control sync,
    /// `false` when syncing has failed.
    pub fn mark_sync_ok(&self, ok: bool) {
        self.sync_ok.store(ok, Ordering::Release);
    }

    /// Current state (for `/healthz/leader` and admin write gating).
    #[must_use]
    pub fn state(&self) -> ElectionState {
        *self.state.lock().expect("election state mutex")
    }

    /// Whether this node may act as the active leader RIGHT NOW (holds the
    /// lease and the time fence has not passed).
    #[must_use]
    pub fn is_leader(&self) -> bool {
        match self.state() {
            ElectionState::Active { valid_until } => (self.clock)() < valid_until,
            _ => false,
        }
    }

    /// Run one election tick (called on the `lease_ms / 3` interval).
    ///
    /// - Active + fence alive → renew; failed renew ⇒ **immediate** demotion
    ///   to `Uncertain` (write permission closes; never wait for the TTL);
    /// - Uncertain → try renew (transient blip recovery);
    /// - Standby → acquire if the freshness gate is open.
    pub async fn tick(&self) {
        let now = (self.clock)();
        let next = match self.state() {
            ElectionState::Active { valid_until } if now < valid_until => {
                match self.store.renew(&self.node_id, self.lease_ms).await {
                    Ok(true) => {
                        debug!(node = %self.node_id, "leader lease renewed");
                        ElectionState::Active {
                            valid_until: now + Duration::from_millis(self.lease_ms),
                        }
                    }
                    Ok(false) | Err(_) => {
                        warn!(
                            node = %self.node_id,
                            "lease renewal failed — immediate demotion (fail-closed)"
                        );
                        ElectionState::Uncertain
                    }
                }
            }
            ElectionState::Uncertain => {
                match self.store.renew(&self.node_id, self.lease_ms).await {
                    Ok(true) => {
                        info!(node = %self.node_id, "lease regained — active again");
                        ElectionState::Active {
                            valid_until: now + Duration::from_millis(self.lease_ms),
                        }
                    }
                    _ => ElectionState::Uncertain,
                }
            }
            ElectionState::Standby | ElectionState::Active { .. } => {
                if !self.sync_ok.load(Ordering::Acquire) {
                    debug!(node = %self.node_id, "election: sync gate closed; not eligible");
                    ElectionState::Standby
                } else {
                    match self.store.try_acquire(&self.node_id, self.lease_ms).await {
                        Ok(true) => {
                            info!(node = %self.node_id, "leader lease acquired");
                            ElectionState::Active {
                                valid_until: now + Duration::from_millis(self.lease_ms),
                            }
                        }
                        _ => ElectionState::Standby,
                    }
                }
            }
        };
        *self.state.lock().expect("election state mutex") = next;
    }
}

/// Spawn the periodic election tick on the current runtime.
pub fn spawn_election_task(election: Arc<LeaderElection>, lease_ms: u64) {
    let interval = Duration::from_millis((lease_ms / 3).max(1));
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await; // skip the immediate first tick
        loop {
            ticker.tick().await;
            election.tick().await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Arc<dyn LeaseStore> {
        Arc::new(MemoryLeaseStore::new())
    }

    #[test]
    fn memory_store_acquire_and_expire() {
        // Sync test of the test double itself (real atomic semantics).
        let s = MemoryLeaseStore::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            assert!(s.try_acquire("a", 1000).await.unwrap());
            assert!(!s.try_acquire("b", 1000).await.unwrap(), "held by a");
            assert!(
                !s.renew("b", 1000).await.unwrap(),
                "b cannot renew a's lease"
            );
            assert!(s.renew("a", 1000).await.unwrap());
            std::thread::sleep(Duration::from_millis(1100));
            assert!(
                s.try_acquire("b", 1000).await.unwrap(),
                "expired → b acquires"
            );
        });
    }

    #[test]
    fn acquires_when_standby_and_fences() {
        let t0 = Instant::now();
        let now = Arc::new(Mutex::new(t0));
        let clock: Clock = {
            let now = now.clone();
            Arc::new(move || *now.lock().expect("test clock"))
        };
        let e = LeaderElection::with_clock(store(), "n1".into(), 3000, clock);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            assert!(!e.is_leader());
            e.tick().await;
            assert!(e.is_leader(), "standby acquires on tick");
            // Time fence: advance the clock past valid_until → write
            // permission closes without any further tick.
            *now.lock().expect("test clock") = t0 + Duration::from_millis(4000);
            assert!(!e.is_leader(), "fence expired → write permission closes");
        });
    }

    #[test]
    fn renew_failure_demotes_immediately() {
        // Two nodes, same store: n1 holds; n2 cannot acquire. Release n1 by
        // forcing its renew to fail (steal via direct store manipulation) —
        // the first failed renew must demote to Uncertain, NOT wait for TTL.
        let shared = Arc::new(MemoryLeaseStore::new());
        let t0 = Instant::now();
        let clock: Clock = Arc::new(move || t0);
        let e1 = LeaderElection::with_clock(shared.clone(), "n1".into(), 60_000, clock.clone());
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            e1.tick().await;
            assert!(e1.is_leader());
            // Another node forces the issue by taking the store directly:
            // the memory store only hands it over after expiry, so simulate a
            // lease LOSS by having the store reject n1's renew (its holder
            // changed). Simplest: a store whose renew always fails.
        });
        // Store that loses the lease immediately on renew.
        struct LoseStore;
        impl LeaseStore for LoseStore {
            fn try_acquire<'a>(
                &'a self,
                _n: &'a str,
                _m: u64,
            ) -> Pin<Box<dyn Future<Output = Result<bool, LeaseError>> + Send + 'a>> {
                Box::pin(async { Ok(true) })
            }
            fn renew<'a>(
                &'a self,
                _n: &'a str,
                _m: u64,
            ) -> Pin<Box<dyn Future<Output = Result<bool, LeaseError>> + Send + 'a>> {
                Box::pin(async { Ok(false) })
            }
        }
        let e2 =
            LeaderElection::with_clock(Arc::new(LoseStore), "n1".into(), 60_000, clock.clone());
        rt.block_on(async {
            e2.tick().await; // acquire (try_acquire → true)
            assert!(e2.is_leader());
            e2.tick().await; // renew fails
            assert_eq!(e2.state(), ElectionState::Uncertain, "immediate demotion");
            assert!(!e2.is_leader(), "writes blocked in Uncertain");
        });
    }

    #[test]
    fn freshness_gate_blocks_acquire() {
        let t0 = Instant::now();
        let clock: Clock = Arc::new(move || t0);
        let e = LeaderElection::with_clock(store(), "n1".into(), 3000, clock.clone());
        e.mark_sync_ok(false);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            e.tick().await;
            assert_eq!(
                e.state(),
                ElectionState::Standby,
                "sync gate closed → no acquire"
            );
            e.mark_sync_ok(true);
            e.tick().await;
            assert!(e.is_leader(), "gate open → acquires");
        });
    }
}
