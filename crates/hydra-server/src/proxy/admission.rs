//! Per-provider bounded admission queue — the concurrency valve
//! (design `dev-docs/design-admission-queue.md` §3 / §7 / §10 / §11 P0.2+P0.5).
//!
//! [`AdmissionControl`] owns a `DashMap<provider_id, Arc<ProviderGate>>`. Each
//! gate wraps a `tokio::sync::Semaphore` (`max_concurrency` permits) plus an
//! `AtomicUsize` counting **waiters** (not in-flight). Acquire waits in a
//! bounded FIFO queue up to `max_queue_depth`, with a `queue_wait_timeout_ms`
//! ceiling — capacity errors ([`QueueFull`] / [`WaitTimeout`]) are deliberately
//! distinct from upstream errors so the failover loop (P0.3) can branch on them
//! **without** tripping the breaker (§7).
//!
//! ## Opt-out / unlimited path
//!
//! `policy.max_concurrency == 0` means "do not gate this provider"
//! (design §5 / risk #1). [`AdmissionControl::acquire`] short-circuits and
//! returns a [`Permit::Passthrough`] that holds no real semaphore permit — its
//! `Drop` is a no-op. This is the safe default so a `0`/`None` config leaves
//! behaviour unchanged.
//!
//! ## Accounting model (§3, risk #3)
//!
//! Two INDEPENDENT counters track the two phases of a request's life:
//!
//! | Counter        | What it counts     | Inc/dec site                                   |
//! | -------------- | ------------------ | ---------------------------------------------- |
//! | `queue_depth`  | **waiters** (pending) | [`WaitGuard`] inc on create, dec on drop (when the wait ends — success OR failure) |
//! | semaphore      | **in-flight** (active)  | `acquire_owned` inc, [`Permit`] drop dec |
//!
//! `WaitGuard` increments `queue_depth` on construction and decrements it on
//! `Drop`. In every `acquire` path the guard is dropped **when the wait ends**
//! (permit acquired, timeout, or closed) — so `queue_depth` always reflects the
//! current number of *waiting* requests, never the in-flight ones. There is no
//! double-decrement and no leak: the guard either drops itself (all three match
//! arms) or — there is no other path. The [`Permit`] only holds the semaphore
//! permit; its `Drop` releases the in-flight slot (and is a no-op for
//! [`Permit::Passthrough`]).
//!
//! ## Metrics (§10)
//!
//! On every acquire / drop the module updates the six admission metrics via
//! `crate::admin::metrics` (no-op-on-failure pattern — instrumentation never
//! breaks the hot path):
//!
//! - Gauges (`hydra_permit_inflight`, `hydra_permit_available`,
//!   `hydra_queue_depth`) — set from the live semaphore/atomic state.
//! - Histogram (`hydra_queue_wait_seconds`) — observed on successful acquire.
//! - Counters (`hydra_queue_drops_total`, `hydra_admission_decisions_total`) —
//!   incremented on each outcome.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use dashmap::DashMap;
use hydra_core::config::ConcurrencyPolicy;
use serde::Serialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::timeout;
use tokio::time::Duration;

use crate::admin::metrics;

/// Why an acquire failed. These are **capacity signals**: the failover loop
/// (P0.3) MUST treat them as "candidate unavailable" and `continue` to the next
/// SWRR candidate — NOT as upstream errors. Per design §7 they MUST NOT trip
/// the breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionError {
    /// Queue at capacity (`max_queue_depth` reached) — fail over immediately.
    QueueFull,
    /// `queue_wait_timeout_ms` elapsed before a permit freed.
    WaitTimeout,
    /// The semaphore was closed (only happens on shutdown; should not occur in
    /// normal operation).
    Closed,
}

impl AdmissionError {
    /// Stable metric label for the drop reason (design §10).
    #[must_use]
    fn as_reason(self) -> &'static str {
        match self {
            AdmissionError::QueueFull => "full",
            AdmissionError::WaitTimeout => "timeout",
            AdmissionError::Closed => "closed",
        }
    }
}

/// RAII permit: dropping it frees one in-flight slot and updates the
/// `hydra_permit_inflight` / `hydra_permit_available` gauges. Modeled as an enum
/// so the unlimited (`max_concurrency == 0`) opt-out path returns a
/// [`Permit::Passthrough`] whose `Drop` is a no-op (design §5 / risk #1).
///
/// NOTE: the permit does NOT touch `queue_depth` — that counter is managed
/// entirely by [`WaitGuard`] during the wait phase (see module docs). The
/// permit only owns the semaphore slot (in-flight accounting).
///
/// The `Real` variant wraps the inner `OwnedSemaphorePermit` in an `Option` so
/// that the custom `Drop` impl can release the slot first (via `Option::take`)
/// and THEN read `semaphore.available_permits()` to update the gauges — all in
/// safe Rust (no `ManuallyDrop` / `unsafe` needed, satisfying
/// `#![forbid(unsafe_code)]`).
#[derive(Debug)]
pub enum Permit {
    /// A real semaphore permit — releasing it frees one in-flight slot and
    /// updates the inflight/available gauges.
    Real {
        /// Wrapped in `Option` so `Drop` can `take()` (drop) the inner permit
        /// before reading `available_permits()`.
        inner: Option<OwnedSemaphorePermit>,
        provider_id: String,
        semaphore: Arc<Semaphore>,
        max_concurrency: u32,
    },
    /// Unlimited provider — no gating, no accounting, no-op drop.
    Passthrough,
}

impl Drop for Permit {
    fn drop(&mut self) {
        if let Permit::Real {
            inner,
            provider_id,
            semaphore,
            max_concurrency,
        } = self
        {
            // Release the semaphore slot FIRST (drops the OwnedSemaphorePermit),
            // then read the updated available count to set the gauges.
            inner.take();
            let avail = semaphore.available_permits();
            let inflight = (*max_concurrency).saturating_sub(avail as u32);
            metrics::record_permit_inflight(provider_id, inflight as i64);
            metrics::record_permit_available(provider_id, avail as i64);
        }
        // Passthrough: no-op (no semaphore, no gauges).
    }
}

/// RAII waiter counter: increments `queue_depth` on construction and
/// decrements it on `Drop`. Dropped in EVERY path of `acquire` when the wait
/// ends (success / timeout / closed), so `queue_depth` always reflects the
/// current waiter count — exactly-once, no leak, no double-decrement.
/// Also updates the `hydra_queue_depth` gauge on both inc and dec.
struct WaitGuard {
    queue_depth: Arc<AtomicUsize>,
    provider_id: String,
}

impl WaitGuard {
    /// Reserve a waiter slot ONLY if fewer than `max_depth` waiters are registered,
    /// in ONE atomic step.
    ///
    /// Returns `None` when the queue is full, which the caller turns into
    /// `AdmissionError::QueueFull`. Using a CAS here (instead of `load` in the
    /// caller plus `fetch_add` here) is what makes `max_queue_depth` an actual
    /// bound rather than a hint: two requests that both observed `depth = 0` can no
    /// longer both enqueue.
    fn try_reserve(
        queue_depth: Arc<AtomicUsize>,
        provider_id: String,
        max_depth: usize,
    ) -> Option<Self> {
        let mut current = queue_depth.load(Ordering::Acquire);
        loop {
            if current >= max_depth {
                return None;
            }
            match queue_depth.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                // Another waiter moved the depth: retry against the fresh value.
                Err(actual) => current = actual,
            }
        }
        let g = Self {
            queue_depth,
            provider_id,
        };
        g.record_gauge();
        Some(g)
    }

    /// Set the `hydra_queue_depth` gauge to the current atomic value.
    fn record_gauge(&self) {
        let depth = self.queue_depth.load(Ordering::Acquire);
        metrics::record_queue_depth(&self.provider_id, depth as i64);
    }
}

impl Drop for WaitGuard {
    fn drop(&mut self) {
        self.queue_depth.fetch_sub(1, Ordering::AcqRel);
        self.record_gauge();
    }
}

/// Per-provider gate: one semaphore (concurrency cap) + a waiter counter.
struct ProviderGate {
    semaphore: Arc<Semaphore>,
    /// Current **waiters** (queued, not yet holding a permit). In-flight =
    /// `max_concurrency − semaphore.available_permits()`.
    queue_depth: Arc<AtomicUsize>,
    /// The concurrency cap this gate was created with. Stored separately
    /// because `tokio::sync::Semaphore` does not expose its total permit count
    /// — we need it to compute `inflight = max_concurrency - available`.
    max_concurrency: u32,
    /// The queue bound / wait budget the gate was created with — carried so a later
    /// configuration change can be REPORTED (see `observe_configured_limits`).
    queue_capacity: u32,
    wait_timeout_ms: u64,
    /// The limits the CURRENT configuration asks for, as last seen at acquire time.
    ///
    /// A gate is never resized (decision item D-14), so these can differ from the
    /// enforced values: that difference is a configuration change that has NOT taken
    /// effect, and it used to be invisible — the admin API returned 200, the database
    /// row changed, and `/api/v1/concurrency` reported the (stale) gate value as if it
    /// were the configured one. Measured 2026-09-29; see `ops.md` §4.
    configured: Mutex<ConcurrencyPolicy>,
    /// Warn once per distinct configured policy instead of once per request.
    warned_configured: Mutex<Option<ConcurrencyPolicy>>,
}

use crate::lock_gate;

impl ProviderGate {
    fn new(policy: ConcurrencyPolicy) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(policy.max_concurrency as usize)),
            queue_depth: Arc::new(AtomicUsize::new(0)),
            max_concurrency: policy.max_concurrency,
            queue_capacity: policy.max_queue_depth,
            wait_timeout_ms: policy.queue_wait_timeout_ms,
            configured: Mutex::new(policy),
            warned_configured: Mutex::new(None),
        }
    }

    /// True when the limits enforced by this gate differ from the ones the current
    /// configuration asks for, i.e. a change that requires a restart to take effect.
    fn limits_stale(&self) -> bool {
        let configured = *lock_gate(&self.configured);
        configured.max_concurrency != self.max_concurrency
            || configured.max_queue_depth != self.queue_capacity
            || configured.queue_wait_timeout_ms != self.wait_timeout_ms
    }

    /// Record the limits the current configuration asks for, and when they differ from
    /// the ones this gate enforces, say so ONCE (a WARN naming both sides) and count it.
    ///
    /// This is the visibility half of D-14: the resize itself is still not implemented,
    /// but "the change silently did nothing" is no longer possible to miss.
    fn observe_configured_limits(&self, provider_id: &str, policy: ConcurrencyPolicy) {
        {
            let mut configured = lock_gate(&self.configured);
            *configured = policy;
        }
        if !self.limits_stale() {
            return;
        }
        {
            let mut warned = lock_gate(&self.warned_configured);
            if *warned == Some(policy) {
                return; // already reported this exact configuration
            }
            *warned = Some(policy);
        }
        tracing::warn!(
            target: "hydra::admission",
            provider = provider_id,
            enforced_max_concurrency = self.max_concurrency,
            configured_max_concurrency = policy.max_concurrency,
            enforced_max_queue_depth = self.queue_capacity,
            configured_max_queue_depth = policy.max_queue_depth,
            enforced_queue_wait_timeout_ms = self.wait_timeout_ms,
            configured_queue_wait_timeout_ms = policy.queue_wait_timeout_ms,
            "admission limits changed in configuration but the gate is NOT resized \
             (it keeps the limits captured at this provider's first request): the new \
             limits take effect after a restart. This request is being admitted under \
             the ENFORCED values above."
        );
        crate::admin::metrics::record_admission_limits_stale(provider_id);
    }
}

/// Read-only view of one provider's admission state at a point in time.
/// Returned by [`AdmissionControl::snapshot`] and serialized by the
/// `GET /api/v1/concurrency` admin endpoint (design §10 / §13.2).
#[derive(Debug, Serialize)]
pub struct ProviderConcurrencyStatus {
    /// Provider identifier (matches the `providers.id` column).
    pub provider_id: String,
    /// Configured concurrency cap (`max_concurrency`). `0` would mean
    /// passthrough, but passthrough providers never create a gate — so every
    /// entry in a snapshot has `max_concurrency > 0`.
    pub max_concurrency: u32,
    /// Requests currently holding a permit (= `max_concurrency - available`).
    pub inflight: u32,
    /// Free permits (= `semaphore.available_permits()`).
    pub available: u32,
    /// Requests currently **waiting** in the queue for a permit.
    pub queue_depth: usize,
    /// The limits the CURRENT configuration asks for (as last observed on a request for
    /// this provider). They differ from the enforced ones when a configuration change has
    /// not been applied — see `limits_stale`.
    pub configured_max_concurrency: u32,
    pub configured_max_queue_depth: u32,
    pub configured_queue_wait_timeout_ms: u64,
    /// `true` when the enforced limits differ from the configured ones, i.e. the provider
    /// would need a restart for the change to take effect (admission limits are not
    /// resized on hot-reload — decision item D-14). An operator reading only
    /// `max_concurrency` would otherwise believe the change was applied.
    pub limits_stale: bool,
}

/// Top-level admission controller, keyed by `provider_id`. Cheap to clone
/// (one `Arc` bump) — hold one in `AppState` and clone per request task.
///
/// Gates are created lazily on first `acquire` for a given `provider_id`.
#[derive(Clone)]
pub struct AdmissionControl {
    gates: Arc<DashMap<String, Arc<ProviderGate>>>,
}

impl AdmissionControl {
    /// Build an empty controller (no gates until the first `acquire`).
    #[must_use]
    pub fn new() -> Self {
        Self {
            gates: Arc::new(DashMap::new()),
        }
    }

    /// Number of live gates (introspection / tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.gates.len()
    }

    /// Whether the controller holds no gates.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.gates.is_empty()
    }

    /// Current waiter count for `provider_id` (0 if no gate yet). Test hook for
    /// asserting leak-free accounting.
    #[must_use]
    pub fn queue_depth(&self, provider_id: &str) -> usize {
        self.gates
            .get(provider_id)
            .map(|g| g.queue_depth.load(Ordering::Acquire))
            .unwrap_or(0)
    }

    /// Read-only snapshot of every live gate for the
    /// `GET /api/v1/concurrency` admin endpoint (design §10 / §13.2). Returns
    /// one [`ProviderConcurrencyStatus`] per gate. Providers with
    /// `max_concurrency == 0` (passthrough) never create a gate and are omitted.
    ///
    /// The values are point-in-time and may change immediately after reading
    /// (the semaphore and atomic are live). This is fine for observability —
    /// the endpoint is for operators checking "is the queue backed up?".
    #[must_use]
    pub fn snapshot(&self) -> Vec<ProviderConcurrencyStatus> {
        self.gates
            .iter()
            .map(|entry| {
                let gate = entry.value();
                let available = gate.semaphore.available_permits() as u32;
                let inflight = gate.max_concurrency.saturating_sub(available);
                let configured = *lock_gate(&gate.configured);
                ProviderConcurrencyStatus {
                    provider_id: entry.key().to_string(),
                    max_concurrency: gate.max_concurrency,
                    inflight,
                    available,
                    queue_depth: gate.queue_depth.load(Ordering::Acquire),
                    configured_max_concurrency: configured.max_concurrency,
                    configured_max_queue_depth: configured.max_queue_depth,
                    configured_queue_wait_timeout_ms: configured.queue_wait_timeout_ms,
                    limits_stale: gate.limits_stale(),
                }
            })
            .collect()
    }

    /// The same view as [`Self::snapshot`], but with the configured side taken from the
    /// LIVE configuration instead of from what the last request happened to observe.
    ///
    /// This is what `GET /api/v1/concurrency` serves: a gate is never resized (D-14), so
    /// the interesting fact for an operator is "what does the configuration ask for RIGHT
    /// NOW versus what is this gate enforcing". `configured` returns the resolved policy
    /// for one provider (see `hydra_core::config::resolve_policy`); returning `None` for a
    /// provider that no longer exists in the configuration marks the entry stale too (its
    /// gate is still enforcing something nobody configured).
    pub fn snapshot_with_configured(
        &self,
        configured: impl Fn(&str) -> Option<ConcurrencyPolicy>,
    ) -> Vec<ProviderConcurrencyStatus> {
        let mut out: Vec<ProviderConcurrencyStatus> = self
            .gates
            .iter()
            .map(|entry| {
                let gate = entry.value();
                let available = gate.semaphore.available_permits() as u32;
                let provider_id = entry.key().to_string();
                let configured = configured(&provider_id);
                let stale = match configured {
                    Some(p) => {
                        p.max_concurrency != gate.max_concurrency
                            || p.max_queue_depth != gate.queue_capacity
                            || p.queue_wait_timeout_ms != gate.wait_timeout_ms
                    }
                    None => true, // no longer configured at all
                };
                ProviderConcurrencyStatus {
                    provider_id,
                    max_concurrency: gate.max_concurrency,
                    inflight: gate.max_concurrency.saturating_sub(available),
                    available,
                    queue_depth: gate.queue_depth.load(Ordering::Acquire),
                    configured_max_concurrency: configured.map_or(0, |p| p.max_concurrency),
                    configured_max_queue_depth: configured.map_or(0, |p| p.max_queue_depth),
                    configured_queue_wait_timeout_ms: configured
                        .map_or(0, |p| p.queue_wait_timeout_ms),
                    limits_stale: stale,
                }
            })
            .collect();
        out.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));
        out
    }

    /// Look up or lazily create the gate for `provider_id` under `policy`.
    ///
    /// Returns `Ok(Permit)` on success, `Err(AdmissionError)` on capacity /
    /// timeout. See the module docs for the §7 breaker boundary (the caller
    /// MUST NOT treat these errors as upstream failures).
    ///
    /// # `max_concurrency == 0` (unlimited)
    ///
    /// Short-circuits and returns a [`Permit::Passthrough`] without touching
    /// the semaphore — the opt-out / safe-default path (design §5 / risk #1).
    /// No metrics are recorded for the passthrough path (there is no gate, no
    /// queue, no inflight to track — consistent with "no behaviour change for
    /// unconfigured providers").
    pub async fn acquire(
        &self,
        provider_id: &str,
        policy: ConcurrencyPolicy,
    ) -> Result<Permit, AdmissionError> {
        // 1. Unlimited opt-out: do not gate this provider.
        if policy.max_concurrency == 0 {
            return Ok(Permit::Passthrough);
        }

        let gate = self.get_or_create_gate(provider_id, policy);
        // Report (once) when the configuration asks for different limits than this gate
        // enforces — see `observe_configured_limits` and plan §2bi / D-14.
        gate.observe_configured_limits(provider_id, policy);

        // 2+3. Reserve a waiter slot ATOMICALLY, and fail fast when the queue is
        //      full. `max_queue_depth == 0` means "no queue": at most one waiter
        //      (the same accepted set the previous load-then-increment code had,
        //      since it allowed the increment when the depth was still zero).
        //
        //      The check and the reservation used to be separate — `load()` here
        //      and `fetch_add(1)` inside `WaitGuard::new` — so a burst that all read
        //      the same depth slipped past the bound TOGETHER: with
        //      `max_queue_depth = 1` (or `0`) the real waiter count reached the
        //      concurrency of the burst, and both `hydra_queue_depth` and the
        //      `Retry-After` computed from it described a bound the gate never
        //      enforced. One CAS covers both, and the WaitGuard still decrements on
        //      drop, so the depth keeps meaning "current waiters".
        let max_depth = if policy.max_queue_depth == 0 {
            1
        } else {
            policy.max_queue_depth as usize
        };
        let Some(guard) =
            WaitGuard::try_reserve(gate.queue_depth.clone(), provider_id.to_string(), max_depth)
        else {
            metrics::record_queue_drop(provider_id, AdmissionError::QueueFull.as_reason());
            metrics::record_admission_decision(provider_id, "dropped");
            return Err(AdmissionError::QueueFull);
        };

        // Snapshot available permits to classify the outcome as "acquired"
        // (immediate, avail > 0) vs "queued" (had to wait, avail == 0). There
        // is a benign TOCTOU race here — for metrics labelling only.
        let will_queue = gate.semaphore.available_permits() == 0;

        // 4. Bounded wait for a permit. `acquire_owned` needs an `Arc<Semaphore>`;
        //    clone the Arc (bump refcount) so the gate keeps its own handle.
        let wait_start = Instant::now();
        let wait = Duration::from_millis(policy.queue_wait_timeout_ms);
        match timeout(wait, gate.semaphore.clone().acquire_owned()).await {
            // Got a permit — no longer waiting (guard drops, queue_depth--).
            // The Permit owns the semaphore slot (in-flight), released on its Drop.
            Ok(Ok(permit)) => {
                let elapsed = wait_start.elapsed().as_secs_f64();
                metrics::record_queue_wait(provider_id, elapsed);
                metrics::record_admission_decision(
                    provider_id,
                    if will_queue { "queued" } else { "acquired" },
                );
                // Set inflight/available gauges from the live semaphore state.
                let avail = gate.semaphore.available_permits();
                let inflight = policy.max_concurrency.saturating_sub(avail as u32);
                metrics::record_permit_inflight(provider_id, inflight as i64);
                metrics::record_permit_available(provider_id, avail as i64);
                drop(guard);
                Ok(Permit::Real {
                    inner: Some(permit),
                    provider_id: provider_id.to_string(),
                    semaphore: Arc::clone(&gate.semaphore),
                    max_concurrency: policy.max_concurrency,
                })
            }
            // Semaphore closed (shutdown).
            Ok(Err(_)) => {
                drop(guard);
                metrics::record_queue_drop(provider_id, AdmissionError::Closed.as_reason());
                metrics::record_admission_decision(provider_id, "dropped");
                Err(AdmissionError::Closed)
            }
            // Timed out waiting.
            Err(_) => {
                drop(guard);
                metrics::record_queue_drop(provider_id, AdmissionError::WaitTimeout.as_reason());
                metrics::record_admission_decision(provider_id, "dropped");
                Err(AdmissionError::WaitTimeout)
            }
        }
    }

    /// Get-or-create the gate for `provider_id`, sizing the semaphore from
    /// `policy.max_concurrency`. `entry().or_insert_with` is atomic per shard,
    /// so two concurrent first-acquires for the same id resolve to one gate.
    ///
    /// NOTE: once created, a gate's semaphore is NOT resized if a later call
    /// passes a different `max_concurrency` — the first policy wins. P0.4 will
    /// handle resizing on hot-reload; for P0.2 the policy is stable per
    /// provider within a test.
    fn get_or_create_gate(
        &self,
        provider_id: &str,
        policy: ConcurrencyPolicy,
    ) -> Arc<ProviderGate> {
        // Fast path: entry exists. Return a clone of the Arc.
        if let Some(entry) = self.gates.get(provider_id) {
            return Arc::clone(&entry);
        }
        self.gates
            .entry(provider_id.to_string())
            .or_insert_with(|| Arc::new(ProviderGate::new(policy)))
            .clone()
    }
}

impl Default for AdmissionControl {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(
        max_concurrency: u32,
        max_queue_depth: u32,
        queue_wait_timeout_ms: u64,
    ) -> ConcurrencyPolicy {
        ConcurrencyPolicy {
            max_concurrency,
            max_queue_depth,
            queue_wait_timeout_ms,
        }
    }

    /// A gate mutex poisoned by an unrelated panic must NOT turn the admission hot path
    /// into a panic site: `lock_gate` recovers the inner value (the two mutexes only hold
    /// reporting state — copies of a `ConcurrencyPolicy` — so there is no invariant a
    /// half-finished writer could have left broken, while every task routing to this
    /// provider shares the same gate).
    ///
    /// Without `lock_gate` (i.e. with `.lock().expect(...)`) this test panics with
    /// "admission gate mutex" instead of asserting.
    #[test]
    fn a_poisoned_gate_mutex_does_not_panic_admission() {
        let gate = Arc::new(ProviderGate::new(policy(2, 0, 0)));

        // Poison `configured` on purpose: panic while holding the lock.
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = gate
                .configured
                .lock()
                .expect("a fresh mutex is never poisoned");
            panic!("poison the gate mutex on purpose");
        }));
        assert!(poisoned.is_err(), "the panic must have been raised");
        assert!(
            gate.configured.lock().is_err(),
            "the mutex must really be poisoned (otherwise this test proves nothing)"
        );

        // The proving step: this call panicked before `lock_gate` existed.
        gate.observe_configured_limits("p1", policy(1, 0, 0));
        assert!(
            gate.limits_stale(),
            "the configured side was still recorded after recovering from the poison"
        );
    }
}
