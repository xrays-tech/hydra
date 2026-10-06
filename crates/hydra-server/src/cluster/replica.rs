//! # Standby replica materialization (cluster P2)
//!
//! A leader-candidate node that does NOT hold the lease (standby) keeps its
//! local SQLite in sync with the active leader by materializing every applied
//! control snapshot into it ([`materialize`]). On promotion the replica IS
//! the config DB — the version continues from `config_meta`, never resets.
//!
//! The rebuild is a single transaction (see `db::restore_config`), so a crash
//! mid-restore never leaves a half-config; the previous (last-good) replica
//! remains in place until the new one commits.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use crate::cluster::control_client::PollOutcome;
use crate::cluster::snapshot::{SnapshotError, SnapshotWire};
use crate::crypto::KeyProvider;
use crate::db;
use crate::store::ConfigStore;

/// Materialize a received control snapshot into the local replica DB:
/// hydrate (decrypt secrets) → full-table rebuild → persist the version.
///
/// Out-of-order guard (F-4): if a NEWER version is already committed to the
/// replica (its rebuild finished while this stale snapshot was dispatched /
/// in flight), the rebuild is skipped — an older snapshot must never
/// overwrite a newer one, or the replica content would contradict its
/// version marker.
pub async fn materialize(
    pool: &SqlitePool,
    kp: &dyn KeyProvider,
    wire: &SnapshotWire,
) -> Result<(), SnapshotError> {
    match db::get_config_version(pool).await? {
        // Out-of-order guard: never let an older snapshot overwrite a newer one.
        db::ConfigVersion::Value(current) if wire.version <= current => {
            debug!(
                version = wire.version,
                current, "replica already at a newer version; skipping stale materialization"
            );
            return Ok(());
        }
        // A corrupt watermark gives no ordering information, and the incoming
        // snapshot is the leader's CURRENT state — so applying it is both correct
        // and the repair path (the rebuild writes content and marker together).
        db::ConfigVersion::Corrupt(raw) => {
            warn!(
                raw = %raw,
                version = wire.version,
                "replica config_version is not a number; applying the incoming snapshot to \
                 repair it"
            );
        }
        db::ConfigVersion::Value(_) | db::ConfigVersion::Absent => {}
    }
    // `hydrate` verifies the wire version and unseals BOTH the config and the
    // fidelity rows, so "unsealing" and "what to rebuild" are one decision.
    let hydrated = wire.clone().hydrate(kp)?;
    // Content and version marker commit together (B4): a marker written
    // afterwards could be lost on its own, leaving the replica at one version's
    // content with another version's marker — which the freshness gate reads as
    // "not synced".
    db::restore_config(
        pool,
        kp,
        &hydrated.cfg,
        &hydrated.fidelity,
        hydrated.version,
    )
    .await?;
    Ok(())
}

/// Out-of-order guard for SQLite replica materialization (F-4).
///
/// The control client guarantees the in-memory store never regresses (a
/// non-newer `body.version` is treated as `UpToDate` — see
/// `control_client.rs`), but the SQLite replica is rebuilt by spawned tasks:
/// a stale snapshot (v5) dispatched around a newer one (v7) can finish after
/// it, and its rebuild + `set_config_version` would leave the replica at v5's
/// content and marker while the store sits at v7 — a promoted replica would
/// regress the config.
///
/// Two complementary guards:
/// - a monotonic CLAIM watermark (`last_materialized_version`, CAS): a
///   version is materializable only if it is strictly newer than every
///   version claimed before it — a stale snapshot is dropped at dispatch;
/// - a serialization lock: rebuilds run one at a time, so the per-write
///   version check in [`materialize`] always sees the DB as of the last
///   COMPLETED rebuild, and a stale rebuild is skipped even when it was
///   dispatched before the newer one.
#[derive(Clone)]
pub struct MaterializationGuard {
    last_materialized_version: Arc<AtomicU64>,
    /// One rebuild at a time (see the docs above).
    in_flight: Arc<tokio::sync::Mutex<()>>,
    /// The last snapshot DISPATCHED for materialization, kept so a failed
    /// rebuild can be retried.
    ///
    /// The control client never re-delivers a version whose watermark already
    /// moved in memory (`poll_once` asks with `since = store.version()`), so
    /// without this a transient failure (a locked SQLite, a marker write that
    /// could not commit) left the replica behind — and the node ineligible to
    /// lead — until a NEWER config write or a process restart. The retry count
    /// is bounded so a permanently unusable snapshot (wrong master key) cannot
    /// burn the CPU rebuilding on every poll.
    last_wire: Arc<std::sync::Mutex<Option<Arc<SnapshotWire>>>>,
    /// When the next retry of `last_wire` may run, and how many attempts have
    /// failed in a row (which sets the backoff).
    ///
    /// This replaced a fixed budget of `3 retries per snapshot`. A spent budget
    /// was PERMANENT: the control client never re-delivers a version its memory
    /// watermark already passed, so the only two ways back were a NEWER config
    /// write (which needs a working leader — exactly what a cluster with no
    /// eligible leader cannot produce) or a process restart. A node whose replica
    /// failed to materialize three times over a transient cause (a locked SQLite,
    /// a full disk that was then fixed) stayed unable to lead forever.
    ///
    /// Retrying is safe: `materialize` is idempotent, guarded by the out-of-order
    /// version check, and serialized by `in_flight`. What must be bounded is the
    /// RATE, not the number of tries — a permanently unusable snapshot (wrong
    /// master key) must not rebuild once per poll forever, and a transient one
    /// must recover on its own. Hence exponential backoff up to a cap, and
    /// unbounded retries.
    next_attempt_at: Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    fail_streak: Arc<std::sync::atomic::AtomicU32>,
}

/// First retry delay after a failed materialization.
const RETRY_BASE: std::time::Duration = std::time::Duration::from_secs(1);
/// Ceiling for the backoff (a permanently broken snapshot then costs at most one
/// attempt a minute, forever, instead of three attempts and permanent
/// ineligibility).
const RETRY_CAP: std::time::Duration = std::time::Duration::from_secs(60);

impl Default for MaterializationGuard {
    fn default() -> Self {
        Self {
            last_materialized_version: Arc::new(AtomicU64::new(0)),
            in_flight: Arc::new(tokio::sync::Mutex::new(())),
            last_wire: Arc::new(std::sync::Mutex::new(None)),
            next_attempt_at: Arc::new(std::sync::Mutex::new(None)),
            fail_streak: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        }
    }
}

impl MaterializationGuard {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim `version` for materialization. Returns `true` iff it is strictly
    /// newer than the last claimed version (the CAS arbitrates concurrent
    /// claims); callers must only materialize on `true`.
    pub fn claim(&self, version: u64) -> bool {
        loop {
            let last = self.last_materialized_version.load(Ordering::Acquire);
            if version <= last {
                return false;
            }
            match self.last_materialized_version.compare_exchange_weak(
                last,
                version,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(_) => continue,
            }
        }
    }

    /// The last claimed version (0 = nothing materialized yet).
    #[must_use]
    pub fn last(&self) -> u64 {
        self.last_materialized_version.load(Ordering::Acquire)
    }

    /// Remember a snapshot that is being dispatched. A NEWER snapshot is a fresh
    /// chance, so it resets both the backoff and the failure streak (and may be
    /// attempted immediately).
    fn remember(&self, wire: &SnapshotWire) {
        *self
            .last_wire
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(wire.clone()));
        self.fail_streak.store(0, Ordering::Release);
        *self
            .next_attempt_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// The delay before attempt number `streak` (1-based): `RETRY_BASE`
    /// doubling, capped at `RETRY_CAP`.
    fn backoff_for(streak: u32) -> std::time::Duration {
        let shift = streak.saturating_sub(1).min(6);
        (RETRY_BASE * 2u32.saturating_pow(shift)).min(RETRY_CAP)
    }

    /// Take one retry of the last dispatched snapshot, if there is one and the
    /// backoff has elapsed. `None` = do not retry now (the caller keeps the gate
    /// CLOSED; a later poll will try again).
    fn take_retry(&self) -> Option<Arc<SnapshotWire>> {
        let wire = self
            .last_wire
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()?;
        let mut next = self
            .next_attempt_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = std::time::Instant::now();
        if let Some(at) = *next {
            if now < at {
                crate::admin::metrics::record_replica_materialize_retry("throttled");
                return None;
            }
        }
        let streak = self.fail_streak.fetch_add(1, Ordering::AcqRel) + 1;
        *next = Some(now + Self::backoff_for(streak));
        crate::admin::metrics::record_replica_materialize_retry("attempt");
        Some(wire)
    }

    /// Test-only: make a pending backoff elapse immediately (the alternative is
    /// sleeping `RETRY_BASE * 2^n` seconds inside a unit test).
    #[cfg(test)]
    fn note_backoff_elapsed_for_tests(&self) {
        *self
            .next_attempt_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
    }

    /// Record that the current attempt succeeded: clear the streak so the next
    /// failure starts from the base delay again.
    pub fn note_attempt_succeeded(&self) {
        self.fail_streak.store(0, Ordering::Release);
        *self
            .next_attempt_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

/// Handle one `Applied` control outcome for the standby replica (F-4):
/// claim the snapshot's version (out-of-order guard), then materialize it
/// into the replica DB (serialized), driving the election freshness gate
/// from the result:
///
/// - success → `mark_sync(true)` (the replica now holds the claimed version);
/// - failure (e.g. hydrate with the wrong master key) → `mark_sync(false)`
///   — a node whose replica is not at the claimed version must not be
///   eligible to lead;
/// - stale version (a newer one was already claimed) → skipped entirely;
///   the gate is left to the newer materialization.
pub fn on_applied(
    guard: &MaterializationGuard,
    pool: &SqlitePool,
    kp: Arc<dyn KeyProvider>,
    wire: &SnapshotWire,
    mark_sync: &Arc<dyn Fn(bool) + Send + Sync>,
) {
    if !guard.claim(wire.version) {
        debug!(
            version = wire.version,
            last = guard.last(),
            "replica: skipping stale snapshot (a newer version was already claimed)"
        );
        return;
    }
    guard.remember(wire);
    let pool = pool.clone();
    let wire = wire.clone();
    let mark_sync = mark_sync.clone();
    let in_flight = guard.in_flight.clone();
    tokio::spawn(async move {
        // One rebuild at a time: the version check inside `materialize`
        // then sees the DB as of the last COMPLETED rebuild, so a stale
        // rebuild (dispatched before a newer one) is skipped instead of
        // regressing the replica.
        let _serial = in_flight.lock().await;
        match materialize(&pool, kp.as_ref(), &wire).await {
            Ok(()) => mark_sync(true),
            Err(e) => {
                warn!(
                    error = %e,
                    "replica materialization failed (lease remains safe); keeping the sync gate closed"
                );
                mark_sync(false);
            }
        }
    });
}

/// Whether the local replica DB is materialized AT (or ahead of)
/// `store_version`.
///
/// This is the **evidence** the election freshness gate must be driven by: "the
/// replica holds what the control plane told me" is a fact about this node's
/// disk, not about the poll that carried the snapshot.
///
/// A missing version marker counts as current only when there is nothing to be
/// current about (`store_version == 0`, i.e. the cluster has no config yet) —
/// that is what lets a genuinely cold cluster elect its first leader while a
/// node whose replica was never written stays ineligible.
pub async fn replica_is_current(pool: &SqlitePool, store_version: u64) -> bool {
    match db::get_config_version(pool).await {
        Ok(db::ConfigVersion::Value(v)) => v >= store_version,
        Ok(db::ConfigVersion::Absent) => store_version == 0,
        // A watermark that cannot be read is NOT evidence of freshness, so fail
        // closed. Unlike a spent materialization retry budget this is temporary:
        // the next successful materialization rewrites content and marker
        // together, which repairs it. (Collapsing Corrupt into Absent here is
        // exactly how a node with an empty replica used to be judged synced and
        // become eligible to lead.)
        Ok(db::ConfigVersion::Corrupt(raw)) => {
            warn!(
                raw = %raw,
                "replica config_version is not a number; refusing to treat this node as synced \
                 (fix config_meta.config_version, or let a materialization rewrite it)"
            );
            false
        }
        Err(e) => {
            warn!(error = %e, "could not read the replica version; keeping the gate closed");
            false
        }
    }
}

/// Build the control-client poll hook that drives the election freshness gate
/// (F-4).
///
/// This function is the single owner of the decision "may this node be eligible
/// to lead?", so it can be exercised directly by tests instead of being
/// re-implemented (the bug it exists to prevent lived precisely in the wiring,
/// not in a helper):
///
/// - `Error` → closed: the control plane is unreachable, the replica's
///   freshness cannot be established.
/// - `UpToDate` → decided by [`replica_is_current`], NOT opened blindly. An
///   `UpToDate` poll only proves the control plane had nothing newer to send
///   (the client asks with `since = store.version()`, and it advances that
///   watermark BEFORE the replica rebuild is dispatched) — it says nothing
///   about this node's replica, which may be empty or stale after a failed or
///   still-running materialization. Opening the gate here let such a node win
///   the lease and then rebuild the cluster's config from its stale DB
///   (`ConfigStore::reload_all` reads the local DB).
/// - `Applied` → materialize the snapshot, then open only on SUCCESS.
pub fn gate_hook(
    guard: Arc<MaterializationGuard>,
    pool: SqlitePool,
    store: ConfigStore,
    kp: Arc<dyn KeyProvider>,
    gate: Arc<dyn Fn(bool) + Send + Sync>,
) -> Arc<dyn Fn(&PollOutcome) + Send + Sync> {
    Arc::new(move |outcome: &PollOutcome| match outcome {
        PollOutcome::Error => gate(false),
        PollOutcome::UpToDate => {
            // Nothing newer to apply — but that is a statement about the CONTROL
            // plane, not about this node's replica. Verify the evidence before
            // opening the gate (async, so the check is spawned like the
            // materialization itself).
            let pool = pool.clone();
            let store = store.clone();
            let gate = gate.clone();
            let guard = guard.clone();
            let kp = kp.clone();
            tokio::spawn(async move {
                if replica_is_current(&pool, store.version()).await {
                    gate(true);
                    return;
                }
                // Behind: retry the last dispatched snapshot before giving up.
                // Without this the node stays ineligible until a NEWER config
                // write or a restart, because the client never re-delivers a
                // version its memory watermark already passed.
                let Some(wire) = guard.take_retry() else {
                    // Either no snapshot was ever dispatched, or the backoff has
                    // not elapsed. Both keep the gate CLOSED (correct — the node
                    // cannot prove it is current) and both are retried later by
                    // the next poll, so this must not read as "given up".
                    debug!(
                        store_version = store.version(),
                        "replica is behind the store; retry deferred (no snapshot yet or still \
                         backing off); keeping the leader-eligibility gate closed"
                    );
                    gate(false);
                    return;
                };
                let _serial = guard.in_flight.lock().await;
                match materialize(&pool, kp.as_ref(), &wire).await {
                    Ok(()) => {
                        let ok = replica_is_current(&pool, store.version()).await;
                        if ok {
                            crate::admin::metrics::record_replica_materialize_retry("succeeded");
                            guard.note_attempt_succeeded();
                            info!(
                                version = wire.version,
                                "replica materialization retry succeeded"
                            );
                        } else {
                            warn!(
                                version = wire.version,
                                "replica materialization retry ran but the marker is still behind"
                            );
                        }
                        gate(ok);
                    }
                    Err(e) => {
                        crate::admin::metrics::record_replica_materialize_retry("failed");
                        let next = MaterializationGuard::backoff_for(
                            guard.fail_streak.load(Ordering::Acquire).max(1),
                        );
                        warn!(
                            version = wire.version,
                            error = %e,
                            next_retry_secs = next.as_secs(),
                            "replica materialization retry failed; keeping the gate closed and \
                             backing off (retries are unbounded — the node recovers on its own \
                             once the cause clears)"
                        );
                        gate(false);
                    }
                }
            });
        }
        PollOutcome::Applied(wire) => on_applied(&guard, &pool, kp.clone(), wire, &gate),
    })
}

/// The last-applied config watermark stored in the replica — three states, so
/// "corrupt" cannot be mistaken for "fresh DB" (see [`db::ConfigVersion`]).
pub async fn replica_version(pool: &SqlitePool) -> Result<db::ConfigVersion, sqlx::Error> {
    db::get_config_version(pool).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use hydra_core::config::ConfigData;

    use crate::cluster::snapshot::{SealedDto, SealedProviderKeyDto};

    async fn pool() -> sqlx::SqlitePool {
        let p = crate::db::init_pool("sqlite::memory:")
            .await
            .expect("init_pool");
        crate::db::run_migrate(&p).await.expect("migrate");
        p
    }

    /// A minimal snapshot wire (no secrets / no fidelity rows) at `version`.
    fn wire(version: u64) -> SnapshotWire {
        SnapshotWire {
            wire_version: crate::cluster::snapshot::WIRE_VERSION,
            version,
            cfg: ConfigData::default(),
            sealed_provider_keys: HashMap::new(),
            sealed_certs: HashMap::new(),
            fidelity: crate::cluster::snapshot::FidelityWireRows {
                limit_roles: Vec::new(),
                key_prefix_bindings: Vec::new(),
                tenant_token_hashes: Vec::new(),
                provider_models: Vec::new(),
                tenant_providers: Vec::new(),
                tenant_models: Vec::new(),
                sub_tenants: Vec::new(),
                sub_tenant_routes: Vec::new(),
            },
        }
    }

    /// Records every gate decision the handler made (`true` = sync ok).
    fn gate(calls: &Arc<Mutex<Vec<bool>>>) -> Arc<dyn Fn(bool) + Send + Sync> {
        Arc::new({
            let calls = calls.clone();
            move |ok| calls.lock().expect("gate calls mutex").push(ok)
        }) as Arc<dyn Fn(bool) + Send + Sync>
    }

    async fn wait_for_replica_version(pool: &SqlitePool, want: u64) {
        for _ in 0..100 {
            if replica_version(pool).await.unwrap() == db::ConfigVersion::Value(want) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("replica did not reach version {want}");
    }

    async fn wait_for_gate_calls(calls: &Arc<Mutex<Vec<bool>>>, want: usize) {
        for _ in 0..100 {
            if calls.lock().expect("gate calls mutex").len() >= want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("gate not called {want} time(s)");
    }

    /// REVIEW B4 — content and version marker commit together, or neither does.
    ///
    /// The marker used to be a SEPARATE statement after `restore_config`'s
    /// transaction had committed: a crash or error in between left the replica
    /// holding one version's content and another version's marker — which the
    /// freshness gate reads as "not synced", disqualifying a node whose content
    /// is perfectly correct.
    ///
    /// Fault injection is a real SQLite trigger (no mock): abort every write to
    /// `config_meta`.
    #[tokio::test]
    async fn content_and_marker_roll_back_together() {
        let pool = pool().await;
        sqlx::query(
            "CREATE TRIGGER block_marker BEFORE INSERT ON config_meta \
             BEGIN SELECT RAISE(ABORT, 'oracle: marker write blocked'); END",
        )
        .execute(&pool)
        .await
        .expect("install trigger");

        let kp: Arc<dyn KeyProvider> =
            Arc::new(crate::crypto::StaticKeyProvider::new([1u8; 32], 1));
        let mut cfg = ConfigData::default();
        cfg.providers.insert(
            "p1".into(),
            hydra_core::model::Provider {
                id: "p1".into(),
                key: "openai".into(),
                name: "O".into(),
                endpoint: "https://api.openai.com".into(),
                weight: 1,
                created_at: "2026-01-01 00:00:00".into(),
                updated_at: "2026-01-01 00:00:00".into(),
                max_concurrency: None,
                max_queue_depth: None,
                queue_wait_timeout_ms: None,
            },
        );
        let wire = SnapshotWire {
            wire_version: crate::cluster::snapshot::WIRE_VERSION,
            version: 7,
            cfg,
            sealed_provider_keys: HashMap::new(),
            sealed_certs: HashMap::new(),
            fidelity: crate::cluster::snapshot::FidelityWireRows {
                limit_roles: Vec::new(),
                key_prefix_bindings: Vec::new(),
                tenant_token_hashes: Vec::new(),
                provider_models: Vec::new(),
                tenant_providers: Vec::new(),
                tenant_models: Vec::new(),
                sub_tenants: Vec::new(),
                sub_tenant_routes: Vec::new(),
            },
        };

        let err = materialize(&pool, kp.as_ref(), &wire)
            .await
            .expect_err("the marker write must fail");
        assert!(
            err.to_string().contains("marker write blocked"),
            "unexpected error: {err}"
        );

        // Neither half may survive: the content transaction must have rolled
        // back WITH the marker instead of committing on its own.
        assert!(
            !crate::db::config_content_exists(&pool)
                .await
                .expect("content probe"),
            "content committed without its version marker"
        );
        assert_eq!(
            replica_version(&pool).await.expect("version"),
            db::ConfigVersion::Absent,
            "no marker either"
        );
    }

    #[test]
    fn version_helpers_roundtrip() {
        // Sync smoke of config_meta get/set against a real in-memory DB.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let pool = crate::db::init_pool("sqlite::memory:")
                .await
                .expect("init_pool");
            crate::db::run_migrate(&pool).await.expect("migrate");
            assert_eq!(
                replica_version(&pool).await.unwrap(),
                db::ConfigVersion::Absent
            );
            crate::db::set_config_version(&pool, 7).await.expect("set");
            assert_eq!(
                replica_version(&pool).await.unwrap(),
                db::ConfigVersion::Value(7)
            );
            crate::db::set_config_version(&pool, 8)
                .await
                .expect("update");
            assert_eq!(
                replica_version(&pool).await.unwrap(),
                db::ConfigVersion::Value(8)
            );

            // A marker that is not a number is CORRUPT, not absent — and the
            // freshness gate must fail closed on it rather than judging a node
            // with an empty replica "synced" (which is how `store_version == 0`
            // used to be treated).
            sqlx::query(
                "UPDATE config_meta SET value = 'not-a-number' WHERE key = 'config_version'",
            )
            .execute(&pool)
            .await
            .expect("corrupt the marker");
            assert_eq!(
                replica_version(&pool).await.unwrap(),
                db::ConfigVersion::Corrupt("not-a-number".to_string())
            );
            assert!(
                !replica_is_current(&pool, 0).await,
                "a corrupt watermark must not count as synced, even against version 0"
            );
            assert!(
                !replica_is_current(&pool, 8).await,
                "...and certainly not against a real version"
            );
        });
    }

    /// F-4: a version can only be claimed if it is strictly NEWER than the
    /// last claimed one — a stale snapshot is dropped at dispatch (the CAS
    /// arbitrates concurrent claims; equal versions are not re-applied).
    #[test]
    fn guard_claim_is_monotonic() {
        let g = MaterializationGuard::new();
        assert_eq!(g.last(), 0, "fresh guard: nothing claimed yet");
        assert!(g.claim(7), "first version claims");
        assert_eq!(g.last(), 7);
        assert!(!g.claim(5), "v5 after v7 → stale, not claimed");
        assert!(!g.claim(7), "equal version → not claimed (no re-apply)");
        assert!(g.claim(8), "newer version claims");
        assert!(!g.claim(6), "v6 after v8 → stale");
        assert_eq!(g.last(), 8);
    }

    /// F-4 end-to-end: a stale snapshot (v5) arriving AFTER v7 was claimed
    /// must never materialize — the replica's final version is 7, so the
    /// replica content can never contradict its version marker.
    #[tokio::test]
    async fn stale_snapshot_after_newer_keeps_final_version() {
        let pool = pool().await;
        let kp = Arc::new(crate::crypto::StaticKeyProvider::new([7u8; 32], 1));
        let guard = MaterializationGuard::new();
        let calls = Arc::new(Mutex::new(Vec::<bool>::new()));

        on_applied(&guard, &pool, kp.clone(), &wire(7), &gate(&calls));
        wait_for_replica_version(&pool, 7).await;

        on_applied(&guard, &pool, kp.clone(), &wire(5), &gate(&calls));
        assert_eq!(
            replica_version(&pool).await.unwrap(),
            db::ConfigVersion::Value(7),
            "v5 skipped (stale) → final version stays 7"
        );
        assert_eq!(guard.last(), 7);
        assert_eq!(
            calls.lock().expect("gate calls mutex").as_slice(),
            &[true],
            "only v7 materialized; v5 touched neither the DB nor the gate"
        );
    }

    /// F-4 in-flight: v5 dispatched first, v7 while v5's rebuild is in
    /// flight. Rebuilds are serialized and each is version-checked, so the
    /// replica ends at the NEWEST version no matter which rebuild started
    /// first (v5 completing after v7 can never regress the replica).
    #[tokio::test]
    async fn concurrent_applied_versions_end_at_newest() {
        let pool = pool().await;
        let kp = Arc::new(crate::crypto::StaticKeyProvider::new([7u8; 32], 1));
        let guard = MaterializationGuard::new();
        let calls = Arc::new(Mutex::new(Vec::<bool>::new()));

        on_applied(&guard, &pool, kp.clone(), &wire(5), &gate(&calls));
        on_applied(&guard, &pool, kp.clone(), &wire(7), &gate(&calls));

        wait_for_gate_calls(&calls, 2).await;
        assert_eq!(
            replica_version(&pool).await.unwrap(),
            db::ConfigVersion::Value(7),
            "the newest version wins even when v5 started first"
        );
        assert_eq!(
            calls.lock().expect("gate calls mutex").as_slice(),
            &[true, true],
            "both rebuilds succeeded (v5 then v7, serialized)"
        );
    }

    /// F-4: a materialization failure (wrong master key → hydrate error)
    /// must leave the freshness gate CLOSED — the replica is not at the
    /// claimed version, so the node must not be eligible to lead.
    #[tokio::test]
    async fn materialize_failure_keeps_gate_closed() {
        let pool = pool().await;
        let sealer = crate::crypto::StaticKeyProvider::new([1u8; 32], 1);
        let other = Arc::new(crate::crypto::StaticKeyProvider::new([2u8; 32], 1));

        // A snapshot sealed under one master key; the replica holds another
        // → hydrate fails (fail-closed) → materialize errors.
        let sealed = SealedProviderKeyDto {
            id: "k1".into(),
            created_at: String::new(),
            sealed: SealedDto::from(&sealer.seal(b"sk-test").expect("seal")),
        };
        let mut w = wire(3);
        w.sealed_provider_keys.insert("p1".into(), vec![sealed]);

        let guard = MaterializationGuard::new();
        let calls = Arc::new(Mutex::new(Vec::<bool>::new()));
        on_applied(&guard, &pool, other, &w, &gate(&calls));

        wait_for_gate_calls(&calls, 1).await;
        assert_eq!(
            calls.lock().expect("gate calls mutex").as_slice(),
            &[false],
            "the failure is the only gate decision — it stays closed"
        );
        assert_eq!(
            replica_version(&pool).await.unwrap(),
            db::ConfigVersion::Absent,
            "nothing was written on failure (last-known-good kept)"
        );
    }

    /// The invariant that replaced the fixed 3-retry budget: a failed
    /// materialization is retried UNBOUNDED in count, rate-limited in time.
    ///
    /// With the old budget `take_retry` returned `None` forever once three
    /// attempts had failed — and because the control client never re-delivers a
    /// version its memory watermark already passed, the node stayed ineligible to
    /// lead until a NEWER config write (which needs a working leader) or a
    /// restart.
    #[test]
    fn a_failed_materialization_is_always_offered_again_after_a_backoff() {
        let guard = MaterializationGuard::new();
        let w = wire(9);
        guard.remember(&w);

        // Three failures in a row — exactly the budget the old code spent.
        for attempt in 0..3 {
            assert!(
                guard.take_retry().is_some(),
                "attempt {attempt} must be offered"
            );
            assert!(
                guard.take_retry().is_none(),
                "attempt {attempt}: a second try inside the backoff must be declined"
            );
            guard.note_backoff_elapsed_for_tests();
        }

        // The fourth attempt is STILL offered; the old code returned None here
        // forever.
        assert!(
            guard.take_retry().is_some(),
            "after three failures a further retry must still be offered (this is the \
             permanent-ineligibility fix)"
        );
    }

    /// The backoff grows and is capped, so a permanently unusable snapshot (a
    /// wrong master key) costs a bounded rate instead of a rebuild per poll.
    #[test]
    fn the_retry_backoff_grows_and_is_capped() {
        assert_eq!(MaterializationGuard::backoff_for(1), RETRY_BASE);
        assert_eq!(MaterializationGuard::backoff_for(2), RETRY_BASE * 2);
        assert_eq!(MaterializationGuard::backoff_for(3), RETRY_BASE * 4);
        assert_eq!(
            MaterializationGuard::backoff_for(60),
            RETRY_CAP,
            "the delay saturates at the cap"
        );
    }

    /// End-to-end: a TRANSIENT materialization failure must heal on its own —
    /// fault removed, no new snapshot, no restart — and the gate must reopen.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_transient_materialization_failure_heals_without_a_new_snapshot() {
        let pool = pool().await;
        sqlx::query(
            "CREATE TRIGGER block_marker BEFORE INSERT ON config_meta \
             BEGIN SELECT RAISE(ABORT, 'oracle: marker write blocked'); END",
        )
        .execute(&pool)
        .await
        .expect("install trigger");

        let kp: Arc<dyn KeyProvider> =
            Arc::new(crate::crypto::StaticKeyProvider::new([1u8; 32], 1));
        let w = wire(7);
        let guard = Arc::new(MaterializationGuard::new());
        let calls = Arc::new(Mutex::new(Vec::<bool>::new()));
        // Memory is at v7 (what the control client's `apply_snapshot` does) while
        // the replica DB is still empty — the exact state a failed
        // materialization leaves behind, and the state the freshness gate must
        // treat as "not synced".
        let store = crate::store::ConfigStore::from_snapshot(w.cfg.clone(), kp.clone());
        store.apply_snapshot(w.clone().hydrate(kp.as_ref()).expect("hydrate"));
        assert_eq!(
            store.version(),
            7,
            "precondition: the store's memory version is ahead of the replica DB"
        );
        let hook = gate_hook(guard.clone(), pool.clone(), store, kp.clone(), gate(&calls));

        // Dispatched once, then the poll keeps finding the node behind.
        on_applied(&guard, &pool, kp.clone(), &w, &gate(&calls));
        wait_for_gate_calls(&calls, 1).await;
        for attempt in 0..4 {
            hook(&PollOutcome::UpToDate);
            tokio::time::sleep(Duration::from_millis(50)).await;
            // Fast-forward the backoff so these polls produce REAL attempts
            // rather than being throttled: without at least three failed
            // attempts this test would not discriminate the old fixed budget.
            guard.note_backoff_elapsed_for_tests();
            let _ = attempt;
        }
        assert!(
            guard.fail_streak.load(Ordering::Acquire) >= 3,
            "precondition: at least three attempts really failed (streak {})",
            guard.fail_streak.load(Ordering::Acquire)
        );
        assert!(
            calls.lock().expect("gate calls mutex").iter().all(|c| !*c),
            "the gate must stay closed while materialization keeps failing"
        );
        assert_eq!(
            replica_version(&pool).await.unwrap(),
            db::ConfigVersion::Absent,
            "nothing was written while the fault was in place"
        );

        // The cause clears. No new snapshot, no restart.
        sqlx::query("DROP TRIGGER block_marker")
            .execute(&pool)
            .await
            .expect("drop trigger");

        let recovered = tokio::time::timeout(Duration::from_secs(25), async {
            loop {
                hook(&PollOutcome::UpToDate);
                tokio::time::sleep(Duration::from_millis(200)).await;
                if calls.lock().expect("gate calls mutex").iter().any(|c| *c) {
                    return;
                }
            }
        })
        .await;
        assert!(
            recovered.is_ok(),
            "a node whose replica failed over a TRANSIENT cause must reopen its gate on its \
             own; the old 3-retry budget made this permanent (calls: {:?})",
            calls.lock().expect("gate calls mutex")
        );
    }
}
