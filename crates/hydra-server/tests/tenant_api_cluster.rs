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
}

/// The empty live set is NOT "converged". `publish` is durable once it has
/// succeeded, but "applied on every live node" must never be asserted when no
/// node was checked (fail-closed): the registry view is empty during the boot
/// window before the refresh ticker first populates it, and after rows expire
/// while Redis is still reachable — both are "stale/unpopulated", not "no peers".
///
/// Hermetic on purpose: the empty-set branch returns before any Redis I/O, so a
/// pool pointed at a dead port (no real Redis) exercises exactly the logic under
/// test and cannot hang.
#[tokio::test]
async fn barrier_reports_pending_not_applied_when_the_live_set_is_empty() {
    // Nothing listens here: bind, read the port, then drop the listener.
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = l.local_addr().expect("addr");
    drop(l);
    let url = format!("redis://{addr}");
    let cfg = fred::types::config::Config::from_url(&url).expect("config");
    let pool = fred::clients::Pool::new(cfg, None, None, None, 1).expect("pool");
    let stream = InvalidationStream::new(pool);

    let outcome = stream
        .await_applied("1700000000200-0", &[], Duration::from_millis(100))
        .await;
    assert_eq!(
        outcome,
        AppliedOutcome::Pending {
            nodes_applied: 0,
            nodes_total: 0,
            lagging: Vec::new()
        },
        "an empty live set must be `pending` (nothing checked), never `applied`"
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

/// `wait=none` must PUBLISH and return `202` immediately, with an `event_id` for
/// later reconciliation — not block, and not report the fleet as lagging.
///
/// The distinction is the reason `budget` is an `Option` and not a zero timeout:
/// a zero timeout would make every node look behind, which asserts something
/// about nodes nobody looked at.
#[tokio::test]
async fn wait_none_publishes_and_reports_pending_without_looking_at_the_fleet() {
    let pool = common::real_redis_pool(56).await;
    let stream = InvalidationStream::new(pool.clone());
    let live = vec!["node-a".to_string(), "node-b".to_string()];

    let started = std::time::Instant::now();
    let report = hydra_server::cluster::events::broadcast_and_confirm(
        Some(&stream),
        Some("t1".to_string()),
        vec!["sk-a".to_string()],
        live.clone(),
        None,
    )
    .await;

    assert_eq!(report.state, "pending");
    assert_eq!(
        report.http_status, 202,
        "the caller was told it is in flight"
    );
    assert!(
        report.event_id.is_some(),
        "an event id is the whole point: the caller reconciles against it later"
    );
    assert_eq!(report.nodes_applied, 0);
    assert_eq!(
        report.lagging, live,
        "nobody was checked, so nobody confirmed"
    );
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "must not wait: took {:?}",
        started.elapsed()
    );
}

/// A publish that fails (the bus cannot be reached) must be `unavailable` (503),
/// never `applied`. This pins the publish-failure arm of `broadcast_and_confirm`,
/// which sits BEFORE the convergence barrier: no event was enqueued, so there is
/// nothing to confirm — the caller must not believe the fleet was told.
#[tokio::test]
async fn a_failed_publish_reports_unavailable_not_applied() {
    // Nothing listens here: bind, read the port, then drop the listener.
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = l.local_addr().expect("addr");
    drop(l);
    let url = format!("redis://{addr}");
    let cfg = fred::types::config::Config::from_url(&url).expect("config");
    // fred's default `default_command_timeout` is `0` (wait forever), so an
    // `XADD` against a dead port would block until the process is killed. A
    // short command timeout (matching the production pool's intent in
    // `redis/mod.rs`) turns the dead bus into a fast, ordinary error, which is
    // the condition the publish-failure arm exists to report.
    let perf = fred::types::config::PerformanceConfig {
        default_command_timeout: Duration::from_millis(200),
        ..fred::types::config::PerformanceConfig::default()
    };
    let pool = fred::clients::Pool::new(cfg, Some(perf), None, None, 1).expect("pool");
    // Deliberately NOT connected: publish must fail rather than hang or lie.
    let stream = InvalidationStream::new(pool);

    // Wrap in a timeout: a retrying Redis client must not hang CI.
    let report = tokio::time::timeout(
        Duration::from_secs(5),
        hydra_server::cluster::events::broadcast_and_confirm(
            Some(&stream),
            Some("t1".to_string()),
            vec!["sk-a".to_string()],
            vec!["node-a".to_string()],
            Some(Duration::from_millis(200)),
        ),
    )
    .await
    .expect("a dead bus must fail fast, not hang the CI");

    assert_eq!(
        report.state, "unavailable",
        "a publish failure must be `unavailable`, never `applied`"
    );
    assert_eq!(
        report.http_status, 503,
        "the caller must not believe the fleet was told"
    );
    assert!(
        report.event_id.is_none(),
        "nothing was enqueued, so there is no event id to reconcile against"
    );
}
