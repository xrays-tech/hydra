#![cfg(feature = "cluster-redis")]
//! Plan T2 / audit G2 — the node registry must be able to FORGET.
//!
//! THE DEFECT THIS GUARDS. There was no delete path at all: `unregister()` had
//! no production caller, `register()` ran once at boot, and the 20s "renewal"
//! only refreshed the heartbeat — so the hash row was written exactly once per
//! process lifetime and never removed. Every restart added a row that stayed
//! forever, which is the "113 rows, 108 offline" symptom: the admin Health page
//! rendered an unbounded pile of dead nodes.
//!
//! These tests drive the REAL registry against a REAL Redis (dev-plan rule 2),
//! and pin the two properties the design turns on:
//!   * the value format is FROZEN (`role|control_url`) — an old reader must keep
//!     parsing it, or its forward target breaks / every admin write 503s, and
//!   * a row is reapable ONLY when the heartbeat AND the grace witness are both
//!     gone, and never when it is the current lease holder.

mod common;

use fred::prelude::*;
use hydra_server::cluster::registry::{NodeRegistry, HEARTBEAT_PREFIX, NODES_KEY, SEEN_PREFIX};
use hydra_server::cluster::NodeRole;
use hydra_server::redis::LEASE_KEY;

// Integration tests own Redis databases 41..=63 (`tests/common/mod.rs`), and
// `real_redis_pool` flushes the database it hands out. Tests in ONE binary run
// in parallel, so each test below takes its OWN index (41/42/43 are already
// used by admin_api/cluster) — sharing one would have them flush each other
// mid-test, which is exactly how the first version of this file failed.

fn reg(pool: &Pool, id: &str, role: NodeRole, url: &str) -> NodeRegistry {
    NodeRegistry::new(pool.clone(), id.to_string(), role, url.to_string())
}

/// The value format is a CONTRACT, not an implementation detail: a
/// not-yet-upgraded reader splits it on `|` and compares `role == "leader"`.
/// Anything appended or prefixed breaks it (a suffix glues onto `control_url`;
/// a `v2` prefix makes `active_leader_url()` return `None` ⇒ every standby
/// admin write answers 503). Assert the exact bytes in Redis.
#[tokio::test]
async fn the_registry_value_format_is_frozen() {
    let pool = common::real_redis_pool(44).await;
    let a = reg(&pool, "node-a", NodeRole::Leader, "http://a:8081");
    a.register(60, 120).await.expect("register");

    let raw: Option<String> = pool.hget(NODES_KEY, "node-a").await.expect("hget");
    assert_eq!(
        raw.as_deref(),
        Some("leader|http://a:8081"),
        "the value must stay `role|control_url` — byte for byte"
    );

    // The witness lives in its OWN key (that is the whole design): adding it to
    // the value is what would break old readers.
    let seen: i64 = pool
        .exists(format!("{SEEN_PREFIX}node-a"))
        .await
        .expect("exists seen");
    let hb: i64 = pool
        .exists(format!("{HEARTBEAT_PREFIX}node-a"))
        .await
        .expect("exists hb");
    assert_eq!(seen, 1, "the seen witness key exists");
    assert_eq!(hb, 1, "the heartbeat key exists");
}

/// The pre-existing backlog can actually be cleaned: rows written by an
/// un-upgraded node have NO witness key, so for them "heartbeat gone" is the
/// whole predicate — the same condition `leader_control_urls()` already uses to
/// skip a node.
#[tokio::test]
async fn sweep_clears_the_legacy_backlog() {
    let pool = common::real_redis_pool(45).await;
    // 113 rows / 108 offline is the documented symptom; a smaller multiple keeps
    // the test fast while still being a BATCH (the sweep does one HDEL).
    for i in 0..12 {
        let _: i64 = pool
            .hset(
                NODES_KEY,
                (format!("legacy-{i}"), format!("edge|http://l{i}:8081")),
            )
            .await
            .expect("hset legacy row");
    }
    // Two of them are still alive (fresh heartbeat, no witness — exactly what an
    // un-upgraded node produces).
    for i in [3, 7] {
        let _: Option<String> = pool
            .set(
                format!("{HEARTBEAT_PREFIX}legacy-{i}"),
                "1",
                Some(fred::types::Expiration::EX(60)),
                None,
                false,
            )
            .await
            .expect("hb");
    }

    let reaper = reg(&pool, "node-self", NodeRole::Leader, "http://self:8081");
    reaper.register(60, 120).await.expect("register self");

    assert_eq!(
        reaper.sweep_stale().await.expect("sweep"),
        10,
        "the ten heartbeat-less legacy rows are reaped in one batch"
    );
    let ids: Vec<String> = reaper
        .list_nodes()
        .await
        .expect("list")
        .into_iter()
        .map(|n| n.node_id)
        .collect();
    assert_eq!(ids.len(), 3, "two live legacy rows + self: {ids:?}");
    assert!(ids.contains(&"legacy-3".to_string()) && ids.contains(&"legacy-7".to_string()));
    assert!(ids.contains(&"node-self".to_string()));

    // Idempotent: nothing left to reap.
    assert_eq!(reaper.sweep_stale().await.expect("sweep2"), 0);
}

/// A node that dies after registering (both keys expire) is reaped; the CURRENT
/// lease holder is not, even when it looks equally dead — `active_leader_url()`
/// ignores the heartbeat, so deleting that row would strand every standby's
/// forward path.
#[tokio::test]
async fn sweep_spares_the_lease_holder_but_reaps_the_dead() {
    let pool = common::real_redis_pool(46).await;
    let leader = reg(&pool, "node-leader", NodeRole::Leader, "http://leader:8081");
    let dead = reg(&pool, "node-dead", NodeRole::Edge, "http://dead:8081");
    leader.register(60, 120).await.expect("register leader");
    dead.register(60, 120).await.expect("register dead");
    let _: Option<String> = pool
        .set(LEASE_KEY, "node-leader", None, None, false)
        .await
        .expect("set lease");

    // The dead node really expired: both keys gone.
    let _: i64 = pool
        .del(format!("{HEARTBEAT_PREFIX}node-dead"))
        .await
        .expect("del hb");
    let _: i64 = pool
        .del(format!("{SEEN_PREFIX}node-dead"))
        .await
        .expect("del seen");
    // And the holder ALSO loses both keys (a long GC pause, a partitioned
    // holder) — the case the guard exists for.
    let _: i64 = pool
        .del(format!("{HEARTBEAT_PREFIX}node-leader"))
        .await
        .expect("del holder hb");
    let _: i64 = pool
        .del(format!("{SEEN_PREFIX}node-leader"))
        .await
        .expect("del holder seen");

    let reaper = reg(&pool, "node-self", NodeRole::Leader, "http://self:8081");
    reaper.register(60, 120).await.expect("register self");

    assert_eq!(reaper.sweep_stale().await.expect("sweep"), 1);
    let ids: Vec<String> = reaper
        .list_nodes()
        .await
        .expect("list")
        .into_iter()
        .map(|n| n.node_id)
        .collect();
    assert!(!ids.contains(&"node-dead".to_string()), "{ids:?}");
    assert!(
        ids.contains(&"node-leader".to_string()),
        "the lease holder's row is the only forward pointer a standby has: {ids:?}"
    );

    // The forward target still resolves through the surviving row.
    let follower = reg(&pool, "node-follower", NodeRole::Edge, "http://f:8081");
    assert_eq!(
        follower.active_leader_url().await.expect("active"),
        Some("http://leader:8081".to_string()),
        "which is exactly why the holder is never reaped"
    );
}

/// Renewal rewrites the row and pushes both TTLs forward — the retired
/// `refresh_heartbeat` renewed ONLY the heartbeat, so a node whose role or
/// control_url changed after boot kept advertising the boot-time value.
#[tokio::test]
async fn renewal_rewrites_the_row_and_extends_both_ttls() {
    let pool = common::real_redis_pool(47).await;
    let boot = reg(&pool, "node-r", NodeRole::Edge, "http://old:8081");
    boot.register(60, 120).await.expect("register boot");

    // 1s TTLs so "extended" is observable without a long sleep.
    let promoted = reg(&pool, "node-r", NodeRole::Leader, "http://new:8081");
    promoted.register(60, 120).await.expect("re-register");

    let raw: Option<String> = pool.hget(NODES_KEY, "node-r").await.expect("hget");
    assert_eq!(
        raw.as_deref(),
        Some("leader|http://new:8081"),
        "renewal rewrote the row (role + url), not just the heartbeat"
    );
    for key in [
        format!("{HEARTBEAT_PREFIX}node-r"),
        format!("{SEEN_PREFIX}node-r"),
    ] {
        let ttl: i64 = pool.ttl(&key).await.expect("ttl");
        assert!(ttl > 0, "{key} has a live TTL, got {ttl}");
    }
    assert_eq!(
        boot.leader_control_urls().await.expect("discover"),
        vec!["http://new:8081".to_string()],
        "discovery follows the rewritten row"
    );
}

/// Graceful shutdown removes the row AND both liveness keys, so a clean restart
/// leaves nothing for the reaper to clean up later.
#[tokio::test]
async fn unregister_removes_the_row_and_both_keys() {
    let pool = common::real_redis_pool(48).await;
    let a = reg(&pool, "node-u", NodeRole::Leader, "http://u:8081");
    a.register(60, 120).await.expect("register");
    a.unregister().await.expect("unregister");

    let raw: Option<String> = pool.hget(NODES_KEY, "node-u").await.expect("hget");
    assert!(raw.is_none(), "row removed");
    let hb: i64 = pool
        .exists(format!("{HEARTBEAT_PREFIX}node-u"))
        .await
        .expect("hb");
    let seen: i64 = pool
        .exists(format!("{SEEN_PREFIX}node-u"))
        .await
        .expect("seen");
    assert_eq!((hb, seen), (0, 0), "no keys left behind");
}

/// The reaper's metric hooks are callable from the reaper task (they are what
/// makes the symptom alertable) — a registration conflict must degrade to a
/// no-op rather than panic.
#[test]
fn registry_metric_hooks_do_not_panic() {
    hydra_server::admin::metrics::record_registry_nodes(3, 108);
    hydra_server::admin::metrics::record_registry_reaped(108);
    hydra_server::admin::metrics::record_registry_reaped(0);
}
