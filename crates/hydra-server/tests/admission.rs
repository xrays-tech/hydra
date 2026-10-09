//! P0.2 — admission-controller semaphore tests (concurrent, tokio).
//!
//! Design `dev-docs/design-admission-queue.md` §3 (algorithm), §7 (breaker
//! boundary — these tests assert the error VARIANT only; the no-trip wiring is
//! a caller concern in P0.3), §11 P0.2.
//!
//! Covers: unlimited opt-out, basic concurrency cap, queue-full, wait-timeout
//! (with leak-free `queue_depth` accounting), permit-release-on-drop, and a
//! FIFO fairness stress check.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use hydra_core::config::ConcurrencyPolicy;
use hydra_server::proxy::admission::{AdmissionControl, AdmissionError, Permit};

use tokio::time::{Duration, Instant};

fn policy(max_concurrency: u32, max_queue_depth: u32, wait_ms: u64) -> ConcurrencyPolicy {
    ConcurrencyPolicy {
        max_concurrency,
        max_queue_depth,
        queue_wait_timeout_ms: wait_ms,
    }
}

/// `max_queue_depth` must be a real BOUND, not a hint.
///
/// The check was `queue_depth.load()` in `acquire` and the reservation was a
/// `fetch_add(1)` inside `WaitGuard::new`, so a simultaneous burst that all read
/// the same depth slipped past the limit TOGETHER: with `max_queue_depth = 1` the
/// real waiter count reached the size of the burst, and `hydra_queue_depth` and
/// the `Retry-After` derived from it described a bound the gate never enforced.
/// One CAS now covers check-and-reserve, so the count is exact.
///
/// The only permit is HELD for the whole test, so any request that gets past the
/// check must genuinely queue: the bounded wait then expires and reports `Timeout`.
/// That makes the assertion exact rather than timing-dependent — admitted
/// (`Ok` or `Timeout`) must be exactly 1, and everything else is `QueueFull`.
///
/// Falsification: restore `load()` + `fetch_add` and the burst admits more than
/// one waiter (the assertion fails).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_queue_depth_bound_holds_under_a_simultaneous_burst() {
    const BURST: usize = 32;
    let ac = AdmissionControl::new();
    let p = policy(1, 1, 100); // one permit, room for a single waiter, 100 ms wait

    // Hold the only permit so a queued waiter cannot finish early.
    let held: Permit = ac.acquire("pBurst", p).await.expect("the only permit");

    let barrier = Arc::new(tokio::sync::Barrier::new(BURST));
    let mut handles = Vec::with_capacity(BURST);
    for _ in 0..BURST {
        let ac = ac.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await; // release them all as close together as tokio can
            ac.acquire("pBurst", p).await
        }));
    }

    let (mut admitted, mut full, mut other) = (0usize, 0usize, 0usize);
    for h in handles {
        match h.await.expect("task") {
            // Passed the queue check (whether it then got a permit or timed out
            // waiting for the one we are holding).
            Ok(_) | Err(AdmissionError::WaitTimeout) => admitted += 1,
            Err(AdmissionError::QueueFull) => full += 1,
            Err(e) => {
                other += 1;
                eprintln!("unexpected admission error: {e:?}");
            }
        }
    }

    assert_eq!(other, 0, "only QueueFull/Timeout are expected here");
    assert_eq!(
        admitted, 1,
        "max_queue_depth = 1 must admit EXACTLY one waiter, no matter how many \
         requests arrive together (admitted={admitted}, full={full})"
    );
    assert_eq!(full, BURST - 1, "every other request must fail fast");

    drop(held);
}

/// `max_concurrency == 0` ⇒ unlimited: acquire never blocks, 100 concurrent
/// acquires all succeed instantly, and the permits are `Passthrough` (no gate
/// created).
#[tokio::test]
async fn unlimited_zero_concurrency_never_blocks() {
    let ac = AdmissionControl::new();
    let p = policy(0, 0, 1000);

    // 100 concurrent acquires all succeed immediately.
    let mut handles = Vec::new();
    for _ in 0..100 {
        let ac = ac.clone();
        handles.push(tokio::spawn(async move { ac.acquire("pA", p).await }));
    }
    let start = Instant::now();
    for h in handles {
        let permit = h.await.expect("task join").expect("acquire ok");
        assert!(
            matches!(permit, Permit::Passthrough),
            "unlimited path must return Passthrough"
        );
    }
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "100 unlimited acquires should be near-instant, took {:?}",
        start.elapsed()
    );
    // No gate should be created for an unlimited provider.
    assert_eq!(ac.len(), 0, "unlimited provider must not create a gate");
}

/// Basic concurrency cap: N=2 permits, acquire 2 (both Ok), 3rd waits; release
/// one → 3rd succeeds.
#[tokio::test]
async fn basic_concurrency_cap_third_waits_then_succeeds() {
    let ac = AdmissionControl::new();
    let p = policy(2, 16, 2000);

    let p1 = ac.acquire("pA", p).await.expect("acquire 1");
    let p2 = ac.acquire("pA", p).await.expect("acquire 2");

    // 3rd acquire should block because both permits are held. Spawn it.
    let ac3 = ac.clone();
    let third = tokio::spawn(async move { ac3.acquire("pA", p).await });

    // Give it a moment to register as a waiter.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !third.is_finished(),
        "3rd acquire must be blocked while both permits are held"
    );
    assert_eq!(ac.queue_depth("pA"), 1, "one waiter should be queued");

    // Release one permit → 3rd should succeed.
    drop(p1);
    let p3 = tokio::time::timeout(Duration::from_millis(500), third)
        .await
        .expect("3rd acquire did not complete within 500ms")
        .expect("task join")
        .expect("acquire 3 ok after release");

    // After all drops, queue depth returns to 0.
    drop(p2);
    drop(p3);
    assert_eq!(ac.queue_depth("pA"), 0, "queue depth must return to 0");
}

/// Queue-full: `max_queue_depth=1`, N=1 permit. Acquire 1 (Ok), 2nd queues
/// (in-flight wait), 3rd → `Err(QueueFull)` immediately.
#[tokio::test]
async fn queue_full_rejects_third_immediately() {
    let ac = AdmissionControl::new();
    let p = policy(1, 1, 5000);

    let _p1 = ac.acquire("pA", p).await.expect("acquire 1");

    // 2nd acquire queues (it is within max_queue_depth=1).
    let ac2 = ac.clone();
    let second = tokio::spawn(async move { ac2.acquire("pA", p).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(ac.queue_depth("pA"), 1, "one waiter queued");

    // 3rd acquire → QueueFull immediately (queue already at capacity).
    let start = Instant::now();
    let result = ac.acquire("pA", p).await;
    let elapsed = start.elapsed();
    assert_eq!(
        result.unwrap_err(),
        AdmissionError::QueueFull,
        "3rd acquire must be QueueFull"
    );
    assert!(
        elapsed < Duration::from_millis(100),
        "QueueFull must be immediate, took {elapsed:?}"
    );

    // Queue depth unchanged (the rejected acquire didn't register).
    assert_eq!(
        ac.queue_depth("pA"),
        1,
        "rejected acquire must not increment"
    );

    // The 2nd (queued) acquire is still waiting.
    assert!(!second.is_finished());

    // Cleanup: dropping p1 lets the 2nd succeed.
    drop(_p1);
    let _ = second.await.expect("join second");
}

/// WaitTimeout: small `queue_wait_timeout_ms`, N=1, acquire 1, 2nd waits →
/// `Err(WaitTimeout)` after ~timeout, and `queue_depth` returns to 0 (leak-free).
#[tokio::test]
async fn wait_timeout_returns_and_releases_queue_slot() {
    let ac = AdmissionControl::new();
    // 50ms timeout — short enough to be quick, long enough to not flake.
    let p = policy(1, 8, 50);

    let _held = ac.acquire("pA", p).await.expect("acquire 1");

    let start = Instant::now();
    let result = ac.acquire("pA", p).await;
    let elapsed = start.elapsed();

    assert_eq!(
        result.unwrap_err(),
        AdmissionError::WaitTimeout,
        "2nd acquire must time out"
    );
    // Allow generous slack for CI scheduling jitter.
    assert!(
        elapsed >= Duration::from_millis(40),
        "must have waited ~50ms, took {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "must not wait much longer than the timeout, took {elapsed:?}"
    );

    // Leak-free accounting: the timed-out waiter must have decremented.
    assert_eq!(
        ac.queue_depth("pA"),
        0,
        "queue_depth must return to 0 after WaitTimeout (leak-free)"
    );
}

/// After a WaitTimeout/QueueFull, a subsequent acquire succeeds once a permit
/// frees — proving the slot was correctly released (no leak).
#[tokio::test]
async fn permit_release_on_drop_after_failure() {
    let ac = AdmissionControl::new();
    let p = policy(1, 8, 50);

    let held = ac.acquire("pA", p).await.expect("acquire 1");
    // This one times out while held is still held.
    let _ = ac.acquire("pA", p).await; // WaitTimeout
    assert_eq!(ac.queue_depth("pA"), 0, "no leaked waiter after timeout");

    // Drop the held permit; a fresh acquire must succeed immediately.
    drop(held);
    let _again = ac.acquire("pA", p).await.expect("acquire after release");
    assert_eq!(ac.queue_depth("pA"), 0);
}

/// Fail-fast: `max_queue_depth == 0` means no queue at all. With N=1, acquire 1
/// (Ok), 2nd → `Err(QueueFull)` immediately (the single waiter slot is not
/// allowed).
#[tokio::test]
async fn fail_fast_zero_queue_depth_rejects_waiter() {
    let ac = AdmissionControl::new();
    let p = policy(1, 0, 5000);

    let _held = ac.acquire("pA", p).await.expect("acquire 1");

    // A 2nd acquire finds one waiter already? No — depth is 0, but
    // max_queue_depth==0 means ANY waiter is rejected. Since depth is 0, the
    // guard against ">0" triggers only after a waiter registers. So this 2nd
    // call will register as a waiter (depth becomes 1 momentarily inside
    // acquire) only if we don't fail-fast first.
    //
    // Per design §5: max_queue_depth==0 ⇒ fail-fast (no queue). Our impl
    // checks `depth > 0` BEFORE registering, so a single contender with depth
    // already 0... let's verify: depth is 0 (no waiter yet), so the check
    // `depth > 0` is false → it proceeds to wait. That means with depth=0
    // initially, the FIRST waiter IS allowed to wait. A SECOND waiter finds
    // depth==1 > 0 → QueueFull.
    //
    // So: the 2nd acquire here WILL wait (depth was 0). We need a 2nd waiter
    // to trigger fail-fast. Let's spawn the 2nd, wait for it to register,
    // then the 3rd should fail-fast.
    let ac2 = ac.clone();
    let second = tokio::spawn(async move { ac2.acquire("pA", p).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(ac.queue_depth("pA"), 1, "first waiter registers");

    // Now the 3rd finds depth==1 > 0 → QueueFull immediately.
    let start = Instant::now();
    let result = ac.acquire("pA", p).await;
    assert_eq!(result.unwrap_err(), AdmissionError::QueueFull);
    assert!(
        start.elapsed() < Duration::from_millis(100),
        "fail-fast must be immediate"
    );

    // Cleanup.
    drop(_held);
    let _ = second.await;
}

/// FIFO fairness stress: N=1, spawn 5 acquires. They must be granted in
/// arrival order (tokio's Semaphore documents FIFO wake-up). We hold the single
/// permit, spawn 5 contenders with small yields between each (so they register
/// in the semaphore's wait queue in spawn order), then release the holder and
/// record the grant sequence.
#[tokio::test]
async fn fifo_fairness_under_contention() {
    let ac = AdmissionControl::new();
    let p = policy(1, 64, 10_000); // long timeout so nobody gives up

    // Hold the single permit ourselves first so all 5 contenders queue.
    let holder = ac.acquire("pA", p).await.expect("holder acquire");

    let order = Arc::new(AtomicUsize::new(0));
    // Each contender records the global grant-sequence it observed.
    let results: Arc<Vec<AtomicUsize>> = Arc::new((0..5).map(|_| AtomicUsize::new(99)).collect());

    let mut tasks = Vec::new();
    for i in 0..5usize {
        let ac = ac.clone();
        let results = Arc::clone(&results);
        let order = Arc::clone(&order);
        tasks.push(tokio::spawn(async move {
            let permit = ac.acquire("pA", p).await.expect("acquire ok");
            let seq = order.fetch_add(1, Ordering::AcqRel);
            results[i].store(seq, Ordering::Release);
            // Hold briefly so the next contender must wait for our drop.
            tokio::time::sleep(Duration::from_millis(15)).await;
            drop(permit);
        }));
        // Yield between spawns so each contender registers in the semaphore's
        // wait queue in spawn order (tokio::task::yield_now alone is not
        // enough — the acquire future needs a turn to register).
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // All 5 are now registered as waiters.
    assert_eq!(ac.queue_depth("pA"), 5, "all 5 should be queued");

    // Release the holder — the semaphore grants the next permit to the oldest
    // waiter (FIFO). Each contender holds briefly then drops, waking the next.
    drop(holder);

    for t in tasks {
        t.await.expect("task join");
    }

    // Each contender recorded the global sequence at its grant time. FIFO ⇒
    // contender i got sequence i.
    for (i, r) in results.iter().enumerate() {
        assert_eq!(
            r.load(Ordering::Acquire),
            i,
            "contender {i} should have grant sequence {i} (FIFO)"
        );
    }
}

/// A permit that is dropped (via early-return / `?` / `continue` semantics)
/// frees the slot for the next acquire — the RAII guarantee (risk #3).
#[tokio::test]
async fn dropping_permit_frees_slot() {
    let ac = AdmissionControl::new();
    let p = policy(1, 8, 2000);

    {
        let _permit = ac.acquire("pA", p).await.expect("acquire");
        assert_eq!(ac.queue_depth("pA"), 0);
    } // permit dropped here

    // Slot is free again — immediate acquire.
    let start = Instant::now();
    let _permit2 = ac.acquire("pA", p).await.expect("acquire after drop");
    assert!(
        start.elapsed() < Duration::from_millis(50),
        "acquire after permit drop should be immediate"
    );
}

/// Two different providers get independent gates — saturation on one does not
/// affect the other.
#[tokio::test]
async fn per_provider_isolation() {
    let ac = AdmissionControl::new();
    let p = policy(1, 8, 2000);

    let _a = ac.acquire("pA", p).await.expect("pA acquire 1");
    // pA is now saturated; pB should be unaffected.
    let b1 = ac.acquire("pB", p).await.expect("pB acquire 1");
    assert!(
        matches!(b1, Permit::Real { .. }),
        "pB should get a real permit independent of pA"
    );
    drop(b1);
    assert_eq!(ac.len(), 2, "two independent gates");
}

/// A configuration change must be APPLIED, and the split "enforced vs configured"
/// view must reflect that (decision item D-14, implemented 2026-10-09).
///
/// Gates were created on a provider's first request and never resized, so a
/// `PUT /api/v1/providers/{id}` that lowered/raised the limits used to leave the
/// runtime enforcing the OLD values while the endpoint reported the (stale) cap.
/// Since D-14 the gate RESIZES on the next `acquire` (a new generation replaces
/// the previous one; in-flight permits keep the old generation until they drain),
/// and `snapshot_with_configured` — the live view the admin endpoint serves —
/// reports the enforced side from the CURRENT generation. After a resize the two
/// sides agree; a change that has not yet been applied (no request since the
/// config write) is transiently `limits_stale`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_configured_limit_change_resizes_the_gate() {
    let admission = AdmissionControl::new();

    // First request creates the gate with these limits.
    let permit = admission
        .acquire("p1", policy(4, 8, 1_000))
        .await
        .expect("the first acquire must succeed");
    drop(permit);

    let fresh = admission.snapshot();
    let entry = fresh
        .iter()
        .find(|e| e.provider_id == "p1")
        .expect("p1 in snapshot");
    assert_eq!(
        entry.max_concurrency, 4,
        "the gate enforces what it was created with"
    );
    assert!(
        !entry.limits_stale,
        "nothing changed yet: the configured limits match the enforced ones"
    );

    // The configuration now asks for 50 concurrent and a 2s wait budget.
    let permit = admission
        .acquire("p1", policy(50, 8, 2_000))
        .await
        .expect("acquire still works under the new configuration");
    drop(permit);

    // The live view: after the resize the enforced cap IS the new one.
    let after = admission.snapshot_with_configured(|_| Some(policy(50, 8, 2_000)));
    let entry = after
        .iter()
        .find(|e| e.provider_id == "p1")
        .expect("p1 in snapshot");
    assert_eq!(
        entry.max_concurrency, 50,
        "the ENFORCED cap is now the resized one — gates are resized on hot-reload (D-14)"
    );
    assert_eq!(
        entry.configured_max_concurrency, 50,
        "the configured cap matches the enforced cap after the resize"
    );
    assert_eq!(entry.configured_queue_wait_timeout_ms, 2_000);
    assert!(
        !entry.limits_stale,
        "after the resize the gate enforces exactly what the configuration asks for"
    );

    // A config change that has NOT yet been applied stays visible: ask the live view
    // for a policy the gate has not been asked to enforce yet.
    let pending = admission.snapshot_with_configured(|_| Some(policy(2, 1, 500)));
    let entry = pending
        .iter()
        .find(|e| e.provider_id == "p1")
        .expect("p1 in snapshot");
    assert_eq!(
        entry.max_concurrency, 50,
        "still enforcing the last applied generation"
    );
    assert_eq!(entry.configured_max_concurrency, 2);
    assert!(
        entry.limits_stale,
        "a config change that has not reached the gate must be flagged, never presented \
         as the enforced cap"
    );
}
