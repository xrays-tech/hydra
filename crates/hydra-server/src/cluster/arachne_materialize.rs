//! # Per-node config materialization: the gate (ADR-0001, plan T3.1)
//!
//! Every node runs the same loop: read `head`, compare it with what this node
//! last materialized, and act. This module owns the **decision** — the gate and
//! its retry bookkeeping — while the I/O (read the tree, rebuild SQLite, swap the
//! in-memory config) belongs to the caller.
//!
//! Splitting it this way is not tidiness: the gate is the part whose failure
//! modes are subtle and expensive. In particular, three properties are pinned
//! here by tests, because each one was a real trap in the design:
//!
//! 1. **`NoChange` means no work.** Steady state must cost one local read per
//!    interval, so the head hash — not a content comparison — decides.
//! 2. **A failed attempt must not advance the watermark.** If failure looked
//!    like success, the node would serve a stale config while believing it was
//!    current, and nothing would ever retry.
//! 3. **Retries are bounded in rate, not in count.** The predecessor of this
//!    code gave up permanently after a fixed number of attempts
//!    (`MaterializationGuard`, 3 tries), which left a node that could not
//!    materialize with exactly two ways back: a config write from a working
//!    leader — which is the one thing a cluster without a working leader cannot
//!    produce — or a restart.
//!
//! ## What this module deliberately does NOT decide
//!
//! Whether a node may become leader. The plan requires "not materialized ⇒ not
//! electable", and that belongs to the leader-watch path, which asks this gate
//! for its watermark. Keeping the two apart means the gate can be tested without
//! a cluster and the election rule can be tested without a config store.

use std::time::{Duration, Instant};

/// How long to wait before the first retry, and the ceiling the backoff grows to.
pub const RETRY_BACKOFF_MIN: Duration = Duration::from_secs(1);
/// The ceiling of the retry backoff. A node that cannot materialize must keep
/// trying forever — see property 3 in the module docs — but must not spin.
pub const RETRY_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// What a convergence pass should do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// The head names what this node already has: nothing to do.
    NoChange,
    /// Materialize the tree named by `hash`.
    Materialize { hash: String },
    /// A previous attempt failed; wait until `at` and do not try before then.
    ///
    /// Carries the deadline rather than a duration so the caller can decide how
    /// to wait (a timer, a select, an injected clock) without this module owning
    /// a runtime.
    Hold {
        hash: String,
        at: Instant,
        attempt: u32,
    },
}

/// Why materialization cannot proceed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateError {
    /// The head's value is not usable as a tree name.
    MalformedHead { raw: String },
}

impl std::fmt::Display for GateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MalformedHead { raw } => {
                write!(f, "the config head is not a usable tree name: {raw:?}")
            }
        }
    }
}

impl std::error::Error for GateError {}

/// The length of a tree name: the hex encoding of a 32-byte content hash.
const TREE_HASH_LEN: usize = 64;

/// Whether `raw` looks like a tree name this build could have written.
///
/// Refused rather than repaired: a head that is not a 64-character lowercase hex
/// string means something else is writing this key, and materializing "the best
/// guess" would build a config out of a foreign value.
#[must_use]
fn is_tree_name(raw: &str) -> bool {
    raw.len() == TREE_HASH_LEN
        && raw
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The per-node materialization gate.
///
/// Holds only what the decision needs: which tree this node last **successfully**
/// materialized, and the state of the last failure.
#[derive(Debug)]
pub struct MaterializeGate {
    /// The tree this node currently serves. `None` until the first success.
    materialized: Option<String>,
    /// Consecutive failed attempts against `failing_hash`.
    attempts: u32,
    /// The tree the failures were against. A different hash resets the backoff,
    /// because a new config deserves an immediate try — and, more importantly,
    /// must not inherit a backoff earned by a config that no longer exists.
    failing_hash: Option<String>,
    /// When the next attempt is allowed. Stored as an absolute deadline: the
    /// first version recomputed `now + backoff` on every call, which meant the
    /// deadline moved with the clock and the wait NEVER expired — a retry that
    /// looks bounded and is infinite.
    next_attempt_at: Option<Instant>,
}

impl Default for MaterializeGate {
    fn default() -> Self {
        Self::new()
    }
}

impl MaterializeGate {
    /// A gate that has materialized nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            materialized: None,
            attempts: 0,
            failing_hash: None,
            next_attempt_at: None,
        }
    }

    /// The tree this node currently serves, if any.
    #[must_use]
    pub fn materialized(&self) -> Option<&str> {
        self.materialized.as_deref()
    }

    /// How many consecutive attempts have failed against the current head.
    #[must_use]
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Decide what to do for a head value read at `now`.
    ///
    /// # Errors
    /// [`GateError::MalformedHead`] when the value is not a tree name.
    pub fn plan(&self, head: Option<&[u8]>, now: Instant) -> Result<Plan, GateError> {
        let Some(raw) = head else {
            // No head: nothing has ever been published. That is NOT an error and
            // NOT "materialize an empty config" — a cluster that has never been
            // configured must keep whatever it has and wait.
            return Ok(Plan::NoChange);
        };
        let raw = String::from_utf8_lossy(raw).into_owned();
        if !is_tree_name(&raw) {
            return Err(GateError::MalformedHead { raw });
        }

        // Already there: the steady state, one comparison per interval.
        if self.materialized.as_deref() == Some(raw.as_str()) {
            return Ok(Plan::NoChange);
        }

        if self.failing_hash.as_deref() == Some(raw.as_str()) && self.attempts > 0 {
            if let Some(at) = self.next_attempt_at {
                if now < at {
                    return Ok(Plan::Hold {
                        hash: raw,
                        at,
                        attempt: self.attempts,
                    });
                }
            }
            // The backoff has elapsed: this is a retry, and the caller owes us
            // an attempt.
        }

        Ok(Plan::Materialize { hash: raw })
    }

    /// Record a successful materialization of `hash`.
    ///
    /// This is the **only** place the watermark moves, which is what property 2
    /// in the module docs is about.
    pub fn succeeded(&mut self, hash: &str) {
        self.materialized = Some(hash.to_string());
        self.attempts = 0;
        self.failing_hash = None;
        self.next_attempt_at = None;
    }

    /// Record a failed attempt against `hash`.
    ///
    /// Returns the backoff the caller should wait before the next attempt.
    pub fn failed(&mut self, hash: &str) -> Duration {
        self.failed_at(hash, Instant::now())
    }

    /// [`Self::failed`] with an explicit clock, so the backoff window is
    /// assertable without sleeping.
    pub fn failed_at(&mut self, hash: &str, now: Instant) -> Duration {
        if self.failing_hash.as_deref() == Some(hash) {
            self.attempts = self.attempts.saturating_add(1);
        } else {
            self.failing_hash = Some(hash.to_string());
            self.attempts = 1;
        }
        let wait = backoff_for(self.attempts);
        self.next_attempt_at = now.checked_add(wait);
        wait
    }

    /// Whether this node is allowed to become the leader.
    ///
    /// A node that has never materialized anything is **not** electable: it would
    /// accept management writes and publish config built from whatever it had
    /// (nothing, or an arbitrarily old tree). A node whose watermark is behind
    /// the head is electable — it is catching up, which is normal — because
    /// refusing that would make a rolling restart unable to elect anyone.
    #[must_use]
    pub fn may_be_leader(&self) -> bool {
        self.materialized.is_some()
    }
}

/// Exponential backoff in whole doublings, capped.
///
/// `attempts` is 1-based, so the first retry waits [`RETRY_BACKOFF_MIN`].
#[must_use]
fn backoff_for(attempts: u32) -> Duration {
    let doublings = attempts.saturating_sub(1).min(16);
    let scaled = RETRY_BACKOFF_MIN.saturating_mul(1u32 << doublings.min(16));
    scaled.min(RETRY_BACKOFF_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_of(seed: u8) -> String {
        (0..64)
            .map(|i| char::from(b"0123456789abcdef"[((seed as usize) + i) % 16]))
            .collect()
    }

    fn now() -> Instant {
        Instant::now()
    }

    /// The steady state must be free: once the node is on the head's tree, the
    /// gate does no work.
    ///
    /// Falsification: compare content instead of the hash and this still passes —
    /// which is why the assertion is paired with the watermark check below.
    #[test]
    fn a_node_already_on_the_head_plans_no_work() {
        let mut gate = MaterializeGate::new();
        let head = hash_of(1);
        gate.succeeded(&head);

        assert_eq!(
            gate.plan(Some(head.as_bytes()), now()).expect("plan"),
            Plan::NoChange
        );
        assert_eq!(gate.materialized(), Some(head.as_str()));
    }

    /// No head at all is not an error: a cluster that was never configured must
    /// keep serving what it has rather than be told to materialize nothing.
    ///
    /// Falsification: return `Materialize { hash: "" }` for `None` and this
    /// fails — and a node would then wipe its own config on a fresh cluster.
    #[test]
    fn no_head_plans_no_work_rather_than_an_empty_config() {
        let gate = MaterializeGate::new();
        assert_eq!(gate.plan(None, now()).expect("plan"), Plan::NoChange);
        assert!(
            !gate.may_be_leader(),
            "nothing materialized ⇒ not electable"
        );
    }

    /// A head this build could not have written is refused, not guessed at.
    ///
    /// Falsification: accept any short string and the first three cases fail.
    #[test]
    fn a_malformed_head_is_refused() {
        let gate = MaterializeGate::new();
        for raw in [
            "",
            "nope",
            &"Z".repeat(64),
            &"a".repeat(63),
            &"a".repeat(65),
        ] {
            let got = gate.plan(Some(raw.as_bytes()), now());
            assert!(
                matches!(got, Err(GateError::MalformedHead { .. })),
                "{raw:?} must be refused, got {got:?}"
            );
        }
        // And a well-formed one is accepted, so the check is not vacuous.
        let good = "0123456789abcdef".repeat(4);
        assert!(gate.plan(Some(good.as_bytes()), now()).is_ok());
    }

    /// A failed attempt must NOT move the watermark — the property that keeps a
    /// node from serving a stale config while believing it is current.
    ///
    /// Falsification: call `succeeded` on the failure path (or move the watermark
    /// in `plan`) and the watermark assertion fails.
    #[test]
    fn a_failed_attempt_does_not_advance_the_watermark() {
        let mut gate = MaterializeGate::new();
        let head = hash_of(3);
        assert_eq!(
            gate.plan(Some(head.as_bytes()), now()).expect("plan"),
            Plan::Materialize { hash: head.clone() }
        );

        gate.failed(&head);
        assert_eq!(
            gate.materialized(),
            None,
            "a failure is not a materialization"
        );
        assert_eq!(gate.attempts(), 1);

        // And the same head now plans a HOLD rather than another immediate try.
        let plan = gate.plan(Some(head.as_bytes()), now()).expect("plan");
        match plan {
            Plan::Hold { hash, attempt, .. } => {
                assert_eq!(hash, head);
                assert_eq!(attempt, 1);
            }
            other => panic!("expected a hold after a failure, got {other:?}"),
        }
    }

    /// The hold must EXPIRE. The first implementation recomputed the deadline as
    /// `now + backoff` on every call, so the wait never ended — a retry loop that
    /// looked bounded and was infinite (found by reasoning about the code, not by
    /// a test, which is why this test now exists).
    ///
    /// Falsification: go back to computing `at` from `now` inside `plan` and the
    /// second assertion fails, because the plan stays a `Hold` forever.
    #[test]
    fn the_backoff_window_actually_elapses() {
        let mut gate = MaterializeGate::new();
        let head = hash_of(6);
        let t0 = now();
        let wait = gate.failed_at(&head, t0);
        assert_eq!(wait, RETRY_BACKOFF_MIN);

        // Immediately after the failure: a hold.
        match gate.plan(Some(head.as_bytes()), t0).expect("plan") {
            Plan::Hold { attempt, .. } => assert_eq!(attempt, 1),
            other => panic!("expected a hold right after a failure, got {other:?}"),
        }

        // After the backoff: a retry, so the caller can make progress.
        match gate.plan(Some(head.as_bytes()), t0 + wait).expect("plan") {
            Plan::Materialize { hash } => assert_eq!(hash, head),
            other => panic!("expected a retry once the backoff elapsed, got {other:?}"),
        }
    }

    /// Backoff grows and is capped, and it is per-hash: a NEW config must not
    /// inherit the backoff earned by one that no longer exists.
    ///
    /// Falsification: key the backoff on nothing (a single counter) and the
    /// "new hash plans immediately" assertion fails.
    #[test]
    fn backoff_grows_is_capped_and_resets_for_a_new_tree() {
        let mut gate = MaterializeGate::new();
        let a = hash_of(4);
        let b = hash_of(9);

        let first = gate.failed(&a);
        assert_eq!(first, RETRY_BACKOFF_MIN);
        let second = gate.failed(&a);
        assert!(second > first, "{second:?} must exceed {first:?}");
        for _ in 0..10 {
            gate.failed(&a);
        }
        assert_eq!(
            gate.failed(&a),
            RETRY_BACKOFF_MAX,
            "the backoff must be capped"
        );

        // A different tree plans immediately, with no hold.
        assert_eq!(
            gate.plan(Some(b.as_bytes()), now()).expect("plan"),
            Plan::Materialize { hash: b.clone() }
        );
        assert_eq!(
            gate.failed(&b),
            RETRY_BACKOFF_MIN,
            "a new tree must start its own backoff"
        );
    }

    /// Success clears the failure state, so the next failure starts over instead
    /// of inheriting an old backoff.
    ///
    /// Falsification: keep `attempts` on success and the last assertion fails.
    #[test]
    fn success_clears_the_failure_state() {
        let mut gate = MaterializeGate::new();
        let a = hash_of(5);
        gate.failed(&a);
        gate.failed(&a);
        assert_eq!(gate.attempts(), 2);

        gate.succeeded(&a);
        assert_eq!(gate.attempts(), 0);
        assert!(gate.may_be_leader(), "a materialized node is electable");

        let b = hash_of(7);
        assert_eq!(gate.failed(&b), RETRY_BACKOFF_MIN);
    }

    /// Electability is exactly "has materialized something": a node that never
    /// has must not be allowed to accept management writes.
    #[test]
    fn only_a_materialized_node_may_lead() {
        let mut gate = MaterializeGate::new();
        assert!(!gate.may_be_leader());

        gate.succeeded(&hash_of(1));
        assert!(gate.may_be_leader());

        // A failure after a success does NOT make it ineligible: it still serves
        // the tree it materialized, which is the documented last-known-good
        // behaviour.
        gate.failed(&hash_of(2));
        assert!(
            gate.may_be_leader(),
            "a node catching up still serves its last-known-good config and stays electable"
        );
    }
}
