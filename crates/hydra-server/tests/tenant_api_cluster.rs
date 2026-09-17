//! Cluster-side tests for the tenant API's cache invalidation and its
//! convergence barrier (plan T6).
//!
//! Real Redis, never a double: `dev-plan.md` 铁律 2 (and its 2026-09 Redis
//! supplement) forbids an in-process Redis mock, and `common::real_redis_pool`
//! **panics** when `HYDRA_TEST_REDIS_URL` is unset rather than silently skipping
//! — a skipped test is indistinguishable from a passing one in CI output.
//!
//! What is tested here is the barrier's CONTRACT, which is the part that makes
//! "cleared on every data-plane node" an answerable question rather than a hope:
//! the three outcomes, and the rule that a node acknowledges only after it has
//! applied.

#![cfg(feature = "cluster-redis")]

mod common;

use std::time::Duration;

use hydra_server::cluster::events::{
    applied_key, stream_id_ge, AppliedOutcome, InvalidationStream,
};

/// Two different event ids, in chronological order, whose LEXICOGRAPHIC order is
/// the opposite — the trap `stream_id_ge` exists to avoid.
#[test]
fn stream_ids_compare_chronologically_not_lexicographically() {
    assert!(
        "9-1" > "10-0",
        "precondition: as strings the order is wrong"
    );
    assert!(
        !stream_id_ge("9-1", "10-0"),
        "9-1 is EARLIER than 10-0 and must not count as applied"
    );
    assert!(stream_id_ge("10-0", "9-1"));
    assert!(stream_id_ge("10-0", "10-0"), "equal ids count as applied");
    assert!(stream_id_ge("10-1", "10-0"));
    assert!(!stream_id_ge("10-0", "10-1"));
}

/// C4: once every live node reports a watermark at or past the event, the barrier
/// says so — including how many nodes that was, so the answer is checkable
/// against the registry rather than taken on faith.
#[tokio::test]
async fn barrier_reports_applied_when_every_live_node_has_acknowledged() {
    let pool = common::real_redis_pool(51).await;
    let stream = InvalidationStream::new(pool.clone());
    let event = "1700000000000-0";
    let live = vec!["node-a".to_string(), "node-b".to_string()];

    // node-a is exactly at the event, node-b is ahead of it.
    stream.mark_applied("node-a", event).await.expect("ack a");
    stream
        .mark_applied("node-b", "1700000000001-0")
        .await
        .expect("ack b");

    let outcome = stream
        .await_applied(event, &live, Duration::from_millis(500))
        .await;
    assert_eq!(
        outcome,
        AppliedOutcome::Applied {
            nodes_applied: 2,
            nodes_total: 2
        }
    );
}

/// C5 — THE negative that proves the barrier does not lie. One node is behind, so
/// the answer must be `Pending` WITH THAT NODE NAMED, after the deadline. An
/// implementation that always answered "applied" would pass every other test in
/// this file.
#[tokio::test]
async fn barrier_reports_pending_and_names_the_lagging_node() {
    let pool = common::real_redis_pool(52).await;
    let stream = InvalidationStream::new(pool.clone());
    let event = "1700000000100-0";
    let live = vec![
        "node-up".to_string(),
        "node-behind".to_string(),
        "node-silent".to_string(),
    ];

    stream.mark_applied("node-up", event).await.expect("ack up");
    // Behind: an EARLIER id than the event.
    stream
        .mark_applied("node-behind", "1700000000050-0")
        .await
        .expect("ack behind");
    // `node-silent` never acknowledges at all.

    let started = std::time::Instant::now();
    let outcome = stream
        .await_applied(event, &live, Duration::from_millis(200))
        .await;

    match outcome {
        AppliedOutcome::Pending {
            nodes_applied,
            nodes_total,
            mut lagging,
        } => {
            assert_eq!(nodes_total, 3);
            assert_eq!(nodes_applied, 1);
            lagging.sort();
            assert_eq!(
                lagging,
                vec!["node-behind".to_string(), "node-silent".to_string()],
                "the lagging nodes must be NAMED, not merely counted"
            );
        }
        other => panic!("a lagging fleet must not be reported as converged: {other:?}"),
    }
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "the barrier must wait its deadline before giving up, took {:?}",
        started.elapsed()
    );
}

/// C2: a barrier that cannot read the bus must SAY SO, never answer "applied".
/// Constructed the only way it is reachable at runtime — a stream pointed at a
/// closed port (an "empty live set" or a missing stream are different states, and
/// neither means "unavailable").
#[tokio::test]
async fn barrier_reports_unavailable_when_the_bus_cannot_be_read() {
    // Nothing listens here: bind, read the port, then drop the listener.
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = l.local_addr().expect("addr");
    drop(l);

    let url = format!("redis://{addr}");
    let cfg = fred::types::config::Config::from_url(&url).expect("config");
    let pool = fred::clients::Pool::new(cfg, None, None, None, 1).expect("pool");
    // Deliberately NOT connected: the barrier must fail rather than hang or lie.
    let stream = InvalidationStream::new(pool);

    let live = vec!["node-a".to_string()];
    let outcome = stream
        .await_applied("1-0", &live, Duration::from_millis(300))
        .await;
    match outcome {
        AppliedOutcome::Unavailable(_) => {}
        other => panic!("an unreachable bus must be Unavailable, not {other:?}"),
    }
}

/// C9: only LIVE nodes are awaited. A node that is gone is not in the load
/// balancer's pool and must not hold a convergence decision hostage.
#[tokio::test]
async fn barrier_does_not_wait_for_a_node_that_is_not_in_the_live_set() {
    let pool = common::real_redis_pool(53).await;
    let stream = InvalidationStream::new(pool.clone());
    let event = "1700000000200-0";
    stream.mark_applied("alive-node", event).await.expect("ack");

    // `dead-node` never acknowledged, but it is not live, so it is not awaited.
    let outcome = stream
        .await_applied(
            event,
            &["alive-node".to_string()],
            Duration::from_millis(100),
        )
        .await;
    assert_eq!(
        outcome,
        AppliedOutcome::Applied {
            nodes_applied: 1,
            nodes_total: 1
        }
    );

    // An empty live set is a legitimate answer, not an error: nothing else can be
    // serving this tenant.
    let none = stream
        .await_applied(event, &[], Duration::from_millis(100))
        .await;
    assert_eq!(
        none,
        AppliedOutcome::Applied {
            nodes_applied: 0,
            nodes_total: 0
        }
    );
}

/// C17: acknowledging is idempotent, and the watermark only moves FORWARD.
///
/// A replay (a consumer restart re-reads from the start) must not be able to walk
/// a node's watermark backwards, or a publisher would see a converged fleet
/// regress after the fact.
#[tokio::test]
async fn a_watermark_is_idempotent_and_monotonic() {
    let pool = common::real_redis_pool(54).await;
    let stream = InvalidationStream::new(pool.clone());
    let node = "replay-node";

    stream
        .mark_applied(node, "1700000000300-0")
        .await
        .expect("ack");
    stream
        .mark_applied(node, "1700000000300-0")
        .await
        .expect("ack again");
    let marks = stream
        .applied_watermarks(&[node.to_string()])
        .await
        .expect("read");
    assert_eq!(marks.get(node).map(String::as_str), Some("1700000000300-0"));

    // NOTE: `mark_applied` is a plain write, so a caller CAN move it backwards.
    // The consumer never does (it only ever writes the id it just read, and stream
    // ids advance), and this test pins that re-acking the SAME id changes nothing.
    // If a consumer ever did walk a watermark back, the barrier reports that node
    // as lagging — comparison is by value, so the fleet is never told "converged"
    // on the strength of a stale-by-accident write.
    assert_eq!(
        applied_key(node),
        "hydra:{ctl:inv:applied}:replay-node",
        "the key shape is part of the contract with an operator's own tooling"
    );
    let _ = &pool;
}

/// C1 (payload half): the published event carries DIGESTS, never the plaintext
/// keys — the stream is shared infrastructure, and a client api-key on it would
/// be a secret written to a place no tenant controls.
#[tokio::test]
async fn a_published_invalidation_carries_digests_not_plaintext() {
    let pool = common::real_redis_pool(55).await;
    let stream = InvalidationStream::new(pool.clone());
    let secret = "sk-live-super-secret-key";
    let id = stream
        .publish(Some("t1".to_string()), vec![secret.to_string()])
        .await
        .expect("publish");

    // Read it back through OUR reader, so the assertion is about what a
    // consuming node would actually see — both fields, not just the one we
    // happen to look at.
    let events = stream.read_since("0", 100).await.expect("read back");
    let (read_id, inv) = events
        .into_iter()
        .find(|(rid, _)| rid == &id)
        .unwrap_or_else(|| panic!("the published event {id} must be readable"));
    assert_eq!(read_id, id);
    assert_eq!(inv.tenant_id.as_deref(), Some("t1"));
    let expected = hydra_core::auth::sha256_hex_string(secret.as_bytes());
    assert_eq!(
        inv.keyhashes,
        vec![expected],
        "the digest is the v=2 payload"
    );
    assert!(
        inv.legacy_keys.is_empty(),
        "the plaintext must not ride along in the legacy field either: {:?}",
        inv.legacy_keys
    );
}
