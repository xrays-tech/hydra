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
//! - `Uncertain`→ try to renew (a transient blip may still leave us holding
//!   the key, and renewing recovers on the very next tick); writes stay
//!   blocked. It is NOT terminal: a definitive loss (`renew` reports the key
//!   is not ours) drops straight back to `Standby`, and repeated renew ERRORS
//!   (Redis unreachable, leadership unknown) are tolerated for only
//!   [`UNCERTAIN_ERR_BUDGET`] ticks before doing the same, so a node can never
//!   be stranded as permanently ineligible to lead.
//!
//! Split-brain safety: exactly one holder at a time (the store is atomic), a
//! time fence (`valid_until` — writes require `now < valid_until`), and the
//! recovery rule that a node observes the lease is held by someone else
//! before it can act as leader.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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

/// How many CONSECUTIVE `renew` errors (Redis unreachable — leadership is
/// unknown, not lost) are tolerated in `Uncertain` before the node returns to
/// `Standby` and competes for the lease again.
///
/// Without a bound, a node that left `Active` without regaining the lease on
/// the next tick stayed in `Uncertain` for the rest of the process lifetime:
/// `renew` is a compare-and-renew (`GET key == node_id`), so once the key is
/// gone it can never return `true` again, and only the `Standby | Active` arm
/// can call `try_acquire`. In a two-candidate cluster that silently removes half
/// the failover capacity per lost lease, and two losses leave no writer at all
/// until a restart.
const UNCERTAIN_ERR_BUDGET: u32 = 3;

/// The per-node leader-election machine (cluster P2).
pub struct LeaderElection {
    store: Arc<dyn LeaseStore>,
    node_id: String,
    lease_ms: u64,
    clock: Clock,
    state: Mutex<ElectionState>,
    /// Freshness gate: `true` only after a control-plane sync has succeeded
    /// (set by the control client / replica). Starts CLOSED: a fresh node has
    /// not synced from the active leader yet, so it is not eligible to race
    /// for the lease with a stale replica (F-4).
    sync_ok: AtomicBool,
    /// Consecutive renew errors seen while `Uncertain` (see
    /// [`UNCERTAIN_ERR_BUDGET`]). Reset whenever leadership is re-established.
    uncertain_errs: AtomicU32,
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
            // F-4: starts CLOSED — a fresh node has not synced from the
            // active leader yet (see the field docs).
            sync_ok: AtomicBool::new(false),
            uncertain_errs: AtomicU32::new(0),
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
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
                    // Definitive loss vs an error that leaves leadership
                    // UNKNOWN. Both demote immediately (fail-closed — never
                    // wait for the TTL), but only the ERROR starts the
                    // Uncertain budget, so the budget measures the whole run
                    // of consecutive errors rather than just the ticks spent
                    // in Uncertain.
                    Ok(false) => {
                        self.uncertain_errs.store(0, Ordering::Release);
                        warn!(
                            node = %self.node_id,
                            "lease renewal failed — immediate demotion (fail-closed)"
                        );
                        ElectionState::Uncertain
                    }
                    Err(e) => {
                        self.uncertain_errs.store(1, Ordering::Release);
                        warn!(
                            node = %self.node_id,
                            error = %e,
                            "lease renewal errored — immediate demotion (fail-closed)"
                        );
                        ElectionState::Uncertain
                    }
                }
            }
            ElectionState::Uncertain => {
                match self.store.renew(&self.node_id, self.lease_ms).await {
                    Ok(true) => {
                        self.uncertain_errs.store(0, Ordering::Release);
                        info!(node = %self.node_id, "lease regained — active again");
                        ElectionState::Active {
                            valid_until: now + Duration::from_millis(self.lease_ms),
                        }
                    }
                    // Definitive: the compare-and-renew answered that the key is
                    // no longer ours (it expired, or another node took it while
                    // we were not looking). Staying here is a dead end — renew
                    // can never succeed again — so return to Standby and
                    // compete for the lease.
                    Ok(false) => {
                        self.uncertain_errs.store(0, Ordering::Release);
                        warn!(
                            node = %self.node_id,
                            "lease lost for good — returning to standby to compete again"
                        );
                        ElectionState::Standby
                    }
                    // Redis unreachable: leadership is UNKNOWN, not lost. Keep
                    // failing closed (writes stay blocked) for a bounded number
                    // of ticks, because a transient blip may still leave us
                    // holding the key and one successful renew restores us
                    // without waiting out the TTL.
                    Err(e) => {
                        let seen = self.uncertain_errs.fetch_add(1, Ordering::AcqRel) + 1;
                        if seen >= UNCERTAIN_ERR_BUDGET {
                            self.uncertain_errs.store(0, Ordering::Release);
                            warn!(
                                node = %self.node_id,
                                failures = seen,
                                error = %e,
                                "renew keeps failing and leadership is unknown — back to standby"
                            );
                            ElectionState::Standby
                        } else {
                            warn!(
                                node = %self.node_id,
                                failures = seen,
                                error = %e,
                                "lease renew errored — staying uncertain (fail-closed)"
                            );
                            ElectionState::Uncertain
                        }
                    }
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
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next;
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
            e.mark_sync_ok(true); // first successful control sync opens the gate
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
            e1.mark_sync_ok(true); // first successful control sync opens the gate
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
            e2.mark_sync_ok(true); // first successful control sync opens the gate
            e2.tick().await; // acquire (try_acquire → true)
            assert!(e2.is_leader());
            e2.tick().await; // renew fails
            assert_eq!(e2.state(), ElectionState::Uncertain, "immediate demotion");
            assert!(!e2.is_leader(), "writes blocked in Uncertain");
        });
    }

    /// A fresh node has not synced from the active leader yet: the
    /// freshness gate must start CLOSED (a stale replica must not race for
    /// the lease), and the first successful control sync (cold start
    /// `UpToDate`, or a materialized snapshot) opens it.
    #[test]
    fn freshness_gate_blocks_acquire() {
        let t0 = Instant::now();
        let clock: Clock = Arc::new(move || t0);
        let e = LeaderElection::with_clock(store(), "n1".into(), 3000, clock.clone());
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            e.tick().await;
            assert_eq!(
                e.state(),
                ElectionState::Standby,
                "fresh node (no sync yet) → gate starts closed → no acquire"
            );
            // First successful control sync opens the gate → the node may
            // compete for the lease.
            e.mark_sync_ok(true);
            e.tick().await;
            assert!(e.is_leader(), "gate open → acquires");
        });
    }
    /// A node that definitively loses the lease must not be stranded.
    ///
    /// `Uncertain` used to exit only on `Ok(true)`, and the compare-and-renew
    /// can never succeed once the key is gone (it requires GET key == node_id),
    /// so a node that lost a lease stayed ineligible to lead for the rest of
    /// the process lifetime — a restart was the only recovery. In a
    /// two-candidate cluster that silently removes half the failover capacity
    /// per lost lease, and two losses leave no writer at all.
    #[test]
    fn uncertain_is_not_a_dead_end_and_can_reacquire() {
        use std::sync::atomic::AtomicBool as Flag;
        struct Scripted {
            renew_ok: Flag,
        }
        impl LeaseStore for Scripted {
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
                let ok = self.renew_ok.load(Ordering::Acquire);
                Box::pin(async move { Ok(ok) })
            }
        }
        let store = Arc::new(Scripted {
            renew_ok: Flag::new(true),
        });
        let t0 = Instant::now();
        let clock: Clock = Arc::new(move || t0);
        let e = LeaderElection::with_clock(store.clone(), "n1".into(), 60_000, clock);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            e.mark_sync_ok(true); // first successful control sync opens the gate
            e.tick().await;
            assert!(e.is_leader(), "standby acquires");

            // Another node took the lease: a definitive loss.
            store.renew_ok.store(false, Ordering::Release);
            e.tick().await;
            assert_eq!(e.state(), ElectionState::Uncertain, "immediate demotion");

            e.tick().await;
            assert_eq!(
                e.state(),
                ElectionState::Standby,
                "a definitive loss must return to Standby, not strand the node"
            );

            // …and it must be able to lead again once the lease is free.
            store.renew_ok.store(true, Ordering::Release);
            e.tick().await;
            assert!(e.is_leader(), "re-acquires after the lease frees up");
        });
    }

    /// Renew ERRORS (Redis unreachable — leadership unknown, not lost) are
    /// tolerated for a bounded number of ticks, then the node returns to
    /// Standby. A transient blip still recovers through `Uncertain` first.
    #[test]
    fn uncertain_bounds_renew_errors_then_falls_back_to_standby() {
        struct FlakyStore {
            failing: std::sync::atomic::AtomicBool,
        }
        impl LeaseStore for FlakyStore {
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
                let failing = self.failing.load(Ordering::Acquire);
                Box::pin(async move {
                    if failing {
                        Err(LeaseError::Store("redis unreachable".into()))
                    } else {
                        Ok(true)
                    }
                })
            }
        }
        let store = Arc::new(FlakyStore {
            failing: std::sync::atomic::AtomicBool::new(false),
        });
        let t0 = Instant::now();
        let clock: Clock = Arc::new(move || t0);
        let e = LeaderElection::with_clock(store.clone(), "n1".into(), 60_000, clock);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            e.mark_sync_ok(true); // first successful control sync opens the gate
            e.tick().await;
            assert!(e.is_leader(), "standby acquires");

            // A single blip recovers through Uncertain on the next renew.
            store.failing.store(true, Ordering::Release);
            e.tick().await;
            assert_eq!(e.state(), ElectionState::Uncertain, "blip demotes");
            store.failing.store(false, Ordering::Release);
            e.tick().await;
            assert!(e.is_leader(), "a transient blip is recovered");

            // A sustained outage is bounded, then the node competes again.
            store.failing.store(true, Ordering::Release);
            e.tick().await;
            assert_eq!(e.state(), ElectionState::Uncertain, "1st error");
            e.tick().await;
            assert_eq!(e.state(), ElectionState::Uncertain, "2nd error");
            e.tick().await;
            assert_eq!(
                e.state(),
                ElectionState::Standby,
                "the error budget is bounded so the node stays eligible"
            );
            assert!(!e.is_leader(), "writes stay blocked throughout");
        });
    }
}
