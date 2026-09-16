#![cfg(feature = "cluster-redis")]
//! Plan T10.5 — the leader-election invariant on a REAL Redis.
//!
//! THE INVARIANT THIS GUARDS: on a cold start **exactly ONE** node may hold the
//! lease, and after the holder dies **exactly ONE** successor appears — not
//! zero, not two. This is the invariant with a history: a second "leader" means
//! two nodes accepting admin writes and republishing snapshots.
//!
//! Why a separate file from `tests/cluster.rs`: that suite drives the election
//! against `MemoryLeaseStore` (an in-process double), which cannot exercise the
//! Lua compare-and-set that actually arbitrates the lease in production. Here the
//! arbiter is a real Redis (dev-plan rule 2: external systems are never mocked).
//!
//! The first line MUST stay `#![cfg(feature = "cluster-redis")]`: without the
//! feature, `cluster::lease`/`redis` do not exist and the `--features server`
//! build of the test targets would fail to compile.

mod common;

use std::sync::Arc;
use std::time::Duration;

use fred::prelude::*;
use hydra_server::cluster::lease::{ElectionState, LeaderElection, LeaseStore};
use hydra_server::redis::RedisLeaseStore;

/// Integration tests own Redis databases 41..=63 (`tests/common/mod.rs`).
/// Each test takes its own index because the tests in one binary run in
/// parallel and `real_redis_pool` flushes the database it hands out.
async fn election(node_id: &str, lease_ms: u64, db: u8) -> (LeaderElection, Pool) {
    let pool = common::real_redis_pool(db).await;
    let store: Arc<dyn LeaseStore> = Arc::new(RedisLeaseStore::new(pool.clone()));
    let e = LeaderElection::new(store, node_id.to_string(), lease_ms);
    (e, pool)
}

/// How many of `nodes` currently believe they hold the lease.
fn leaders(nodes: &[&LeaderElection]) -> usize {
    nodes.iter().filter(|e| e.is_leader()).count()
}

/// Cold start: three candidates, no snapshot producer anywhere.
///
/// The freshness gate starts CLOSED by design (fail-closed: a node must have
/// synced from the active leader before it may lead), so the test opens it
/// explicitly — in production the control client's poll hook does this. Without
/// that, all three would sit in `Standby` forever, which is the correct
/// fail-closed behaviour and NOT what this test is about.
#[tokio::test]
async fn cold_start_elects_exactly_one_leader() {
    // A generous lease so the 2×lease freshness window stays open for the whole
    // election (a shorter one would expire mid-test and close the gate again).
    let lease_ms = 3_000;
    let (e1, _p) = election("cold-1", lease_ms, 51).await;
    let (e2, _p2) = election("cold-2", lease_ms, 51).await;
    let (e3, _p3) = election("cold-3", lease_ms, 51).await;
    let nodes = [&e1, &e2, &e3];

    for e in nodes {
        assert!(
            !e.is_leader(),
            "a cold node must not consider itself the leader before ticking"
        );
        e.mark_sync_ok(true);
    }

    // Everyone ticks; the first one to win the Lua compare-and-set takes it.
    for e in nodes {
        e.tick().await;
    }

    let holders = leaders(&nodes);
    assert_eq!(
        holders, 1,
        "a cold start must elect EXACTLY one leader (0 would mean no writer, 2+ means split brain)"
    );

    // The losers are standby, and — the part that matters — a second round of
    // ticks must not produce a second leader (a non-atomic acquire would).
    for _ in 0..3 {
        for e in nodes {
            e.mark_sync_ok(true); // keep the freshness window open
            e.tick().await;
        }
        assert_eq!(
            leaders(&nodes),
            1,
            "repeated ticks must not mint a second leader"
        );
    }
    assert_eq!(
        nodes
            .iter()
            .filter(|e| matches!(e.state(), ElectionState::Standby))
            .count(),
        2,
        "the two losers stay Standby (never Uncertain) while the holder renews"
    );
}

/// The holder dies (its lease key expires); exactly ONE successor takes over.
#[tokio::test]
async fn a_dead_holder_is_replaced_by_exactly_one_successor() {
    // Short lease: the takeover has to happen within the test's patience.
    let lease_ms = 700;
    let (e1, _p) = election("fail-1", lease_ms, 52).await;
    let (e2, _p2) = election("fail-2", lease_ms, 52).await;
    let (e3, _p3) = election("fail-3", lease_ms, 52).await;
    let nodes = [&e1, &e2, &e3];

    for e in nodes {
        e.mark_sync_ok(true);
    }
    for e in nodes {
        e.tick().await;
    }
    assert_eq!(
        leaders(&nodes),
        1,
        "precondition: one leader after cold start"
    );

    // Which node is the holder, and which are the survivors?
    let holder_is_1 = e1.is_leader();
    let (holder, survivors): (&LeaderElection, Vec<&LeaderElection>) = if holder_is_1 {
        (&e1, vec![&e2, &e3])
    } else if e2.is_leader() {
        (&e2, vec![&e1, &e3])
    } else {
        (&e3, vec![&e1, &e2])
    };

    // Kill the holder: stop ticking it and let its lease lapse. `tick()` on a
    // dead node never runs again, so its own state stays stale — which is
    // exactly what a killed process looks like to the survivors.
    let _ = holder;
    tokio::time::sleep(Duration::from_millis(lease_ms + 400)).await;

    // Survivors keep the gate open (their processes are alive and still poll).
    let mut new_leaders = 0;
    for e in &survivors {
        e.mark_sync_ok(true);
        e.tick().await;
        if e.is_leader() {
            new_leaders += 1;
        }
    }
    // A second round: the loser must NOT also grab the (now taken) lease.
    for e in &survivors {
        e.mark_sync_ok(true);
        e.tick().await;
    }

    assert_eq!(
        new_leaders, 1,
        "exactly one survivor must take over (0 = no writer, 2 = split brain)"
    );
    let alive_leaders = survivors.iter().filter(|e| e.is_leader()).count();
    assert_eq!(
        alive_leaders, 1,
        "and still exactly one after a second round of ticks"
    );
}

/// The lease lives in Redis under the documented key, with the holder's node id
/// as its value: the registry resolves the forward target from this exact value,
/// so the shape is a contract, not an implementation detail.
#[tokio::test]
async fn the_lease_key_names_the_holder() {
    let lease_ms = 3_000;
    let (e1, pool) = election("shape-1", lease_ms, 53).await;
    let (e2, _p2) = election("shape-2", lease_ms, 53).await;
    for e in [&e1, &e2] {
        e.mark_sync_ok(true);
    }
    e1.tick().await;
    e2.tick().await;

    let holder: Option<String> = pool
        .get(hydra_server::redis::LEASE_KEY)
        .await
        .expect("get lease");
    let holder = holder.expect("a leader exists ⇒ the lease key exists");
    assert!(
        holder == "shape-1" || holder == "shape-2",
        "the lease value is the bare node id (a standby resolves its forward \
         target from it): got {holder}"
    );
    assert!(
        e1.is_leader() || e2.is_leader(),
        "the node named by the lease is a leader"
    );
    if holder == "shape-1" {
        assert!(e1.is_leader() && !e2.is_leader());
    } else {
        assert!(e2.is_leader() && !e1.is_leader());
    }
}
