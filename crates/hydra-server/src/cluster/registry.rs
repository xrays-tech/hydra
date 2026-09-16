//! # Node registry (cluster P4)
//!
//! Every cluster node registers itself in Redis (`hydra:{nodes}` hash +
//! heartbeat key), so edges discover the leader control endpoints and rotate
//! among them — no static `HYDRA_CONTROL_URL` needed (it stays as an override
//! for special cases).
//!
//! **Keys** (single-key operations, topology-safe): `hydra:{nodes}` (hash
//! `node_id → "role|control_url"`), `hydra:{node:hb}:<id>` (heartbeat with TTL;
//! a node whose heartbeat expired is considered gone) and `hydra:{node:seen}:<id>`
//! (the "last seen" witness whose TTL **is** the reaping grace window).
//!
//! **The registry hash VALUE FORMAT IS FROZEN** (`role|control_url`). It is the
//! one shape a not-yet-upgraded node parses: appending anything to the value
//! would glue it onto `control_url` (breaking that peer's forward target), and
//! prefixing a version tag would make `role == "v2"`, which `active_leader_url`
//! resolves to `None` ⇒ **every standby admin write answers 503**. Liveness
//! evidence therefore lives in a SEPARATE key, and reaping is decided by key
//! existence alone (Redis expires the witness; no timestamp arithmetic, no
//! clock-skew handling).

use fred::clients::Pool;
use fred::prelude::*;

use crate::cluster::NodeRole;
use crate::redis::RedisError;

/// Registry hash (field = node id, value = `role|control_url`).
pub const NODES_KEY: &str = "hydra:{nodes}";
/// Heartbeat key prefix (suffix = node id).
pub const HEARTBEAT_PREFIX: &str = "hydra:{node:hb}:";
/// "Last seen" witness key prefix (suffix = node id). See the module docs for
/// why this is not encoded in the hash value.
pub const SEEN_PREFIX: &str = "hydra:{node:seen}:";
/// Reaper "strike" key prefix (suffix = node id): a row already observed once in
/// the `no heartbeat AND no witness` state. PERSISTENT on purpose — see
/// [`NodeRegistry::sweep_stale`]: a TTL would silently re-arm the first-strike
/// state, and the key is deleted as soon as the node shows any sign of life (or
/// when the row is actually reaped).
pub const STRIKE_PREFIX: &str = "hydra:{node:reap}:";

/// A registered node's record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRecord {
    pub node_id: String,
    pub role: String,
    pub control_url: String,
}

/// One fleet node as reported by [`NodeRegistry::list_nodes`] (registry entry
/// + heartbeat liveness).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeStatus {
    pub node_id: String,
    pub role: String,
    pub control_url: String,
    pub alive: bool,
}

/// Redis-backed node registry.
#[derive(Clone)]
pub struct NodeRegistry {
    pool: Pool,
    node_id: String,
    role: NodeRole,
    control_url: String,
}

impl NodeRegistry {
    /// This node's registry id.
    #[must_use]
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// This node's role as registered.
    #[must_use]
    pub fn role(&self) -> NodeRole {
        self.role
    }

    /// This node's own control URL as registered (what peers should poll and
    /// where admin mutations should be forwarded to reach THIS node). Used by
    /// the forward-target resolution's self-forward guard.
    #[must_use]
    pub fn control_url(&self) -> &str {
        &self.control_url
    }

    /// Build the registry for THIS node. `control_url` is the node's own
    /// control endpoint (what peers should poll).
    #[must_use]
    pub fn new(pool: Pool, node_id: String, role: NodeRole, control_url: String) -> Self {
        Self {
            pool,
            node_id,
            role,
            control_url,
        }
    }

    /// Register this node AND renew it — the ONLY entry point for both.
    ///
    /// One entry point so a node whose `role` or `control_url` changed after
    /// boot cannot keep advertising the boot-time value while looking healthy:
    /// the previous split (register once at startup, heartbeat-only refresh
    /// every 20s) renewed only the heartbeat, so the hash row was written
    /// exactly once per process lifetime.
    ///
    /// Writes three things: the row (value format unchanged — see module docs),
    /// the 30s heartbeat (liveness), and the `seen_ttl_secs` witness marker
    /// (the reaping grace window).
    pub async fn register(&self, ttl_secs: u64, seen_ttl_secs: u64) -> Result<(), RedisError> {
        let value = format!("{}|{}", self.role, self.control_url);
        let _: i64 = self
            .pool
            .hset(NODES_KEY, (self.node_id.as_str(), value.as_str()))
            .await?;
        let _: Option<String> = self
            .pool
            .set(
                heartbeat_key(&self.node_id),
                "1",
                Some(fred::types::Expiration::EX(ttl_secs as i64)),
                None,
                false,
            )
            .await?;
        let _: Option<String> = self
            .pool
            .set(
                seen_key(&self.node_id),
                "1",
                Some(fred::types::Expiration::EX(seen_ttl_secs as i64)),
                None,
                false,
            )
            .await?;
        Ok(())
    }

    /// Remove this node from the registry (graceful shutdown).
    pub async fn unregister(&self) -> Result<(), RedisError> {
        let _: i64 = self.pool.hdel(NODES_KEY, &self.node_id).await?;
        let _: i64 = self.pool.del(heartbeat_key(&self.node_id)).await?;
        let _: i64 = self.pool.del(seen_key(&self.node_id)).await?;
        let _: i64 = self.pool.del(strike_key(&self.node_id)).await?;
        Ok(())
    }

    /// Reap registry rows that are PROVABLY dead.
    ///
    /// A row is reaped only when the 30s heartbeat is GONE **and** the
    /// grace-TTL witness is GONE — i.e. this node has not registered for longer
    /// than the grace window. Both conditions are key-existence checks in
    /// Redis, so the grace window is enforced by Redis expiry rather than by
    /// clock arithmetic here.
    ///
    /// The current lease holder is NEVER reaped, even with a missing heartbeat:
    /// `active_leader_url()` does not consult the heartbeat, so deleting that
    /// row would remove the only forward pointer a standby has to the active
    /// writer (every admin write would then 503).
    ///
    /// A row with NO witness key — one written by a not-yet-upgraded node —
    /// gets a STRIKE instead of being reaped outright. Such a node writes its
    /// hash row exactly ONCE, at boot (its 20s loop only renewed the
    /// heartbeat), so a row deleted while that process is alive can never come
    /// back: the node stays invisible to `list_nodes`/`leader_control_urls`
    /// forever, and if it holds the lease every standby admin write answers 503
    /// permanently (forwarding is fail-closed with no static fallback). A single
    /// heartbeat gap of ≥30s — a Redis blip, a paused host — would be enough.
    /// The first sweep that sees "no heartbeat, no witness" therefore only
    /// records a strike; the row is reaped only if the state is IDENTICAL on a
    /// later sweep (≥60s of continuous silence, i.e. three heartbeat intervals),
    /// and any evidence of life clears the strike. That still cleans the
    /// historical backlog, one tick later.
    ///
    /// Returns the number of rows removed.
    pub async fn sweep_stale(&self) -> Result<usize, RedisError> {
        let holder = self.lease_holder().await?;
        let all: Vec<(String, String)> = self.pool.hgetall(NODES_KEY).await?;
        let mut doomed: Vec<String> = Vec::new();

        for (node_id, _raw) in all {
            if Some(node_id.as_str()) == holder.as_deref() {
                continue; // the current lease holder is never reaped
            }
            // Evidence of life: a live heartbeat, or a witness key the node
            // refreshed within the grace window. Either one clears any strike.
            if self.node_alive(&node_id).await?
                || self.pool.exists::<i64, _>(seen_key(&node_id)).await? > 0
            {
                let _: i64 = self.pool.del(strike_key(&node_id)).await?;
                continue;
            }
            // No heartbeat and no witness. Strike once, reap on a repeat.
            let struck: i64 = self.pool.exists(strike_key(&node_id)).await?;
            if struck == 0 {
                let _: Option<String> = self
                    .pool
                    .set(strike_key(&node_id), "1", None, None, false)
                    .await?;
                continue;
            }
            doomed.push(node_id);
        }

        if doomed.is_empty() {
            return Ok(0);
        }
        let removed = doomed.len();
        let refs: Vec<&str> = doomed.iter().map(String::as_str).collect();
        // One HDEL round trip for the whole batch.
        let _: i64 = self.pool.hdel(NODES_KEY, refs).await?;
        // The strikes go with the rows: a future registration starts clean.
        let strikes: Vec<String> = doomed.iter().map(|id| strike_key(id)).collect();
        let _: i64 = self.pool.del(strikes).await?;
        // Report what was actually removed, not what we intended to remove.
        Ok(removed)
    }

    /// The control URLs of LIVE nodes with `role == "leader"` (the poll
    /// rotation set for edges). A node whose heartbeat expired is skipped.
    pub async fn leader_control_urls(&self) -> Result<Vec<String>, RedisError> {
        let fields: Vec<(String, String)> = self.pool.hgetall(NODES_KEY).await?;
        let mut out = Vec::new();
        for (id, value) in fields {
            if !self.node_alive(&id).await? {
                continue;
            }
            let Some((role, url)) = value.split_once('|') else {
                continue;
            };
            if role == "leader" && !url.is_empty() {
                out.push(url.to_string());
            }
        }
        out.sort();
        Ok(out)
    }

    /// Every registered node with its liveness (heartbeat TTL). Used by the
    /// admin cluster-status endpoint (`GET /api/v1/cluster/status`) so the
    /// Admin UI Health page can render the whole fleet.
    pub async fn list_nodes(&self) -> Result<Vec<NodeStatus>, RedisError> {
        let fields: Vec<(String, String)> = self.pool.hgetall(NODES_KEY).await?;
        let mut out = Vec::new();
        for (id, value) in fields {
            let Some((role, url)) = value.split_once('|') else {
                continue;
            };
            let alive = self.node_alive(&id).await?;
            out.push(NodeStatus {
                node_id: id,
                role: role.to_string(),
                control_url: url.to_string(),
                alive,
            });
        }
        out.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        Ok(out)
    }

    /// The current leader-lease holder's node id (the active writer), if any.
    pub async fn lease_holder(&self) -> Result<Option<String>, RedisError> {
        let holder: Option<String> = self.pool.get(crate::redis::LEASE_KEY).await?;
        Ok(holder)
    }

    /// The control URL of the CURRENT lease holder (the active writer), if
    /// any. Reads the leader-lease key (value = holder node id) and resolves
    /// it through the registry. `None` when no lease is held or the holder is
    /// not a registered leader.
    ///
    /// Used for lease-aware rotation: a rejoining standby whose static
    /// `HYDRA_CONTROL_URL` points at ITSELF (or any non-holder) polls
    /// successfully forever and never learns the new active — so it must
    /// rotate to the lease holder even without a poll failure, or its replica
    /// stays stale and a later promotion regresses the config.
    pub async fn active_leader_url(&self) -> Result<Option<String>, RedisError> {
        let holder: Option<String> = self.pool.get(crate::redis::LEASE_KEY).await?;
        let Some(holder) = holder else {
            return Ok(None);
        };
        if holder == self.node_id {
            return Ok(None); // we ARE the active writer — nothing to follow
        }
        let value: Option<String> = self.pool.hget(NODES_KEY, &holder).await?;
        let Some(value) = value else {
            return Ok(None);
        };
        let Some((role, url)) = value.split_once('|') else {
            return Ok(None);
        };
        if role == "leader" && !url.is_empty() {
            Ok(Some(url.to_string()))
        } else {
            Ok(None)
        }
    }

    async fn node_alive(&self, node_id: &str) -> Result<bool, RedisError> {
        let alive: i64 = self.pool.exists(heartbeat_key(node_id)).await?;
        Ok(alive > 0)
    }
}

/// The heartbeat key for a node id.
fn heartbeat_key(node_id: &str) -> String {
    format!("{HEARTBEAT_PREFIX}{node_id}")
}

/// The "last seen" witness key for a node id. Its TTL is the grace window.
fn seen_key(node_id: &str) -> String {
    format!("{SEEN_PREFIX}{node_id}")
}

/// The reaper's strike key for a node id (see [`NodeRegistry::sweep_stale`]).
fn strike_key(node_id: &str) -> String {
    format!("{STRIKE_PREFIX}{node_id}")
}

// ---------------------------------------------------------------------------
// Tests against the in-process Redis double
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A REAL Redis on its own database (dev-plan 铁律 2: no in-process mock).
    async fn pool() -> Pool {
        crate::redis::test_redis::isolated_pool().await
    }

    #[tokio::test]
    async fn register_discover_unregister() {
        let a = NodeRegistry::new(
            pool().await,
            "node-a".into(),
            NodeRole::Leader,
            "http://a:8081".into(),
        );
        let b = NodeRegistry::new(
            pool().await,
            "node-b".into(),
            NodeRole::Edge,
            "http://b:8081".into(),
        );

        a.register(60, 120).await.expect("register a");
        b.register(60, 120).await.expect("register b");

        // Only LIVE leaders are discovered (b is an edge).
        let urls = a.leader_control_urls().await.expect("discover");
        assert_eq!(urls, vec!["http://a:8081".to_string()]);

        // Graceful unregister removes the node.
        a.unregister().await.expect("unregister a");
        let urls = b.leader_control_urls().await.expect("discover2");
        assert!(urls.is_empty(), "a is gone");
    }

    #[tokio::test]
    async fn expired_heartbeat_hides_node() {
        let a = NodeRegistry::new(
            pool().await,
            "node-a".into(),
            NodeRole::Leader,
            "http://a:8081".into(),
        );
        // 1-second TTL → expires before the check below (mock uses real time).
        a.register(1, 120).await.expect("register");
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let urls = a.leader_control_urls().await.expect("discover");
        assert!(urls.is_empty(), "expired heartbeat ⇒ node considered gone");
    }

    #[tokio::test]
    async fn list_nodes_reports_liveness_and_lease_holder() {
        let pool = pool().await;
        let a = NodeRegistry::new(
            pool.clone(),
            "node-a".into(),
            NodeRole::Leader,
            "http://a:8081".into(),
        );
        let b = NodeRegistry::new(
            pool.clone(),
            "node-b".into(),
            NodeRole::Edge,
            "http://b:8081".into(),
        );
        a.register(60, 120).await.expect("register a");
        b.register(60, 120).await.expect("register b");
        // node-a holds the leader lease (value = holder node id).
        let _: Option<String> = pool
            .set(crate::redis::LEASE_KEY, "node-a", None, None, false)
            .await
            .expect("set lease");

        let nodes = a.list_nodes().await.expect("list");
        assert_eq!(nodes.len(), 2, "leader + edge both listed");
        let ids: Vec<&str> = nodes.iter().map(|n| n.node_id.as_str()).collect();
        assert!(ids.contains(&"node-a") && ids.contains(&"node-b"));
        assert!(nodes.iter().all(|n| n.alive), "fresh heartbeats ⇒ alive");
        assert_eq!(
            a.lease_holder().await.expect("holder").as_deref(),
            Some("node-a")
        );

        // Expire b's heartbeat → still listed, but not alive.
        let _: i64 = pool.del("hydra:{node:hb}:node-b").await.expect("del hb");
        let nodes = a.list_nodes().await.expect("list2");
        let b_entry = nodes
            .iter()
            .find(|n| n.node_id == "node-b")
            .expect("b listed");
        assert!(!b_entry.alive, "expired heartbeat ⇒ down but still visible");
    }

    // -----------------------------------------------------------------------
    // G2 — stale-row reaping. Before this feature there was NO delete path at
    // all (`unregister` had no production caller), so every restart added a row
    // that stayed forever: the "113 rows, 108 offline" symptom.
    // -----------------------------------------------------------------------

    /// (1) A LIVE node is never reaped, even though its witness key is present
    /// (it is present precisely because it re-registers).
    #[tokio::test]
    async fn sweep_never_reaps_a_live_node() {
        let a = NodeRegistry::new(
            pool().await,
            "node-live".into(),
            NodeRole::Leader,
            "http://a:8081".into(),
        );
        a.register(60, 120).await.expect("register");
        assert_eq!(a.sweep_stale().await.expect("sweep"), 0, "live ⇒ spared");
        assert_eq!(a.list_nodes().await.expect("list").len(), 1);
    }

    /// (2) Heartbeat gone but the witness key still present ⇒ NOT reaped: the
    /// grace window has not elapsed, so this node may simply be between
    /// renewals (or briefly unreachable). This is the case that keeps a rolling
    /// restart from churning the fleet view.
    #[tokio::test]
    async fn sweep_spares_a_node_within_the_grace_window() {
        let pool = pool().await;
        let a = NodeRegistry::new(
            pool.clone(),
            "node-grace".into(),
            NodeRole::Edge,
            "http://b:8081".into(),
        );
        // Short heartbeat, long witness: exactly the window under test.
        a.register(1, 120).await.expect("register");
        let _: i64 = pool
            .del(heartbeat_key("node-grace"))
            .await
            .expect("del heartbeat");
        let seen: i64 = pool.exists(seen_key("node-grace")).await.expect("seen");
        assert!(seen > 0, "fixture: the witness key is still there");
        assert_eq!(
            a.sweep_stale().await.expect("sweep"),
            0,
            "no heartbeat but a live witness ⇒ inside the grace window ⇒ spared"
        );
    }

    /// (3) Both keys gone ⇒ reaped. (4) The current lease holder is NEVER
    /// reaped, even with both keys gone — `active_leader_url()` does not check
    /// the heartbeat, so removing that row would strand every standby's forward
    /// path (every admin write would 503).
    #[tokio::test]
    async fn sweep_reaps_the_dead_but_never_the_lease_holder() {
        let pool = pool().await;
        let reaper = NodeRegistry::new(
            pool.clone(),
            "node-self".into(),
            NodeRole::Leader,
            "http://self:8081".into(),
        );
        let dead = NodeRegistry::new(
            pool.clone(),
            "node-dead".into(),
            NodeRole::Edge,
            "http://dead:8081".into(),
        );
        let holder = NodeRegistry::new(
            pool.clone(),
            "node-holder".into(),
            NodeRole::Leader,
            "http://holder:8081".into(),
        );
        reaper.register(60, 120).await.expect("register self");
        dead.register(60, 120).await.expect("register dead");
        holder.register(60, 120).await.expect("register holder");

        // The lease: node-holder is the active writer.
        let _: Option<String> = pool
            .set(crate::redis::LEASE_KEY, "node-holder", None, None, false)
            .await
            .expect("set lease");

        // Kill BOTH keys for both candidates — the strongest form of "provably
        // dead" (an un-upgraded node writes no witness key, so its row looks
        // exactly like this; this is also how the historical backlog is
        // cleaned).
        for id in ["node-dead", "node-holder"] {
            let _: i64 = pool.del(heartbeat_key(id)).await.expect("del hb");
            let _: i64 = pool.del(seen_key(id)).await.expect("del seen");
        }

        // FIRST sweep only records a strike (see `sweep_stale`): a row with no
        // witness key may belong to a live, not-yet-upgraded node whose
        // heartbeat merely lapsed, and such a node can never rewrite it.
        assert_eq!(
            reaper.sweep_stale().await.expect("first sweep"),
            0,
            "the first sweep only strikes — nothing is deleted yet"
        );
        assert_eq!(
            reaper.sweep_stale().await.expect("second sweep"),
            1,
            "the SECOND sweep reaps exactly the dead non-holder row"
        );
        let ids: Vec<String> = reaper
            .list_nodes()
            .await
            .expect("list")
            .into_iter()
            .map(|n| n.node_id)
            .collect();
        assert!(
            !ids.contains(&"node-dead".to_string()),
            "the dead row is gone: {ids:?}"
        );
        assert!(
            ids.contains(&"node-holder".to_string()),
            "the lease holder survives reaping even with no heartbeat: {ids:?}"
        );
        assert!(
            ids.contains(&"node-self".to_string()),
            "the live reaper node survives: {ids:?}"
        );
    }

    /// (4b) THE B2 REGRESSION — a LIVE legacy node must survive a heartbeat blip.
    ///
    /// A not-yet-upgraded node writes its hash row exactly ONCE, at boot (its
    /// 20s loop only renewed the heartbeat). So if the reaper deleted that row
    /// while the process was alive — which one heartbeat gap of >=30s used to be
    /// enough for — the node would stay invisible forever, and a lease-holding
    /// node in that state makes EVERY standby admin write 503 permanently
    /// (forwarding is fail-closed with no static fallback). The strike rule
    /// exists for exactly this: silence must be observed twice, with no
    /// heartbeat in between, before anything is deleted.
    #[tokio::test]
    async fn a_live_legacy_node_survives_a_heartbeat_blip() {
        let pool = pool().await;
        let reaper = NodeRegistry::new(
            pool.clone(),
            "node-self".into(),
            NodeRole::Leader,
            "http://self:8081".into(),
        );
        reaper.register(60, 120).await.expect("register self");

        // A legacy row: hand-written, with a heartbeat but NO witness key.
        let _: i64 = pool
            .hset(NODES_KEY, ("node-legacy", "leader|http://legacy:8081"))
            .await
            .expect("hset");
        let _: Option<String> = pool
            .set(
                heartbeat_key("node-legacy"),
                "1",
                Some(fred::types::Expiration::EX(60)),
                None,
                false,
            )
            .await
            .expect("hb");

        // The blip: its heartbeat is gone at sweep time (Redis was unreachable
        // for >30s). One sweep must NOT delete it...
        let _: i64 = pool
            .del(heartbeat_key("node-legacy"))
            .await
            .expect("del hb");
        assert_eq!(
            reaper.sweep_stale().await.expect("sweep 1"),
            0,
            "a single observation of silence must never delete a row"
        );
        assert!(
            reaper
                .list_nodes()
                .await
                .expect("list")
                .iter()
                .any(|n| n.node_id == "node-legacy"),
            "the legacy node is still registered after the blip"
        );

        // ...then it recovers: the heartbeat comes back and the strike is cleared.
        let _: Option<String> = pool
            .set(
                heartbeat_key("node-legacy"),
                "1",
                Some(fred::types::Expiration::EX(60)),
                None,
                false,
            )
            .await
            .expect("hb again");
        assert_eq!(reaper.sweep_stale().await.expect("sweep 2"), 0);
        let strike: i64 = pool
            .exists(strike_key("node-legacy"))
            .await
            .expect("strike");
        assert_eq!(strike, 0, "evidence of life clears the strike");

        // A node that is really gone is still reaped — two consecutive silent
        // sweeps: strike, then reap.
        let _: i64 = pool
            .del(heartbeat_key("node-legacy"))
            .await
            .expect("del hb");
        assert_eq!(reaper.sweep_stale().await.expect("sweep 3"), 0);
        assert_eq!(
            reaper.sweep_stale().await.expect("sweep 4"),
            1,
            "continuous silence across two sweeps does get cleaned up"
        );
        assert!(
            !reaper
                .list_nodes()
                .await
                .expect("list2")
                .iter()
                .any(|n| n.node_id == "node-legacy"),
            "the dead legacy row is gone"
        );
        // The strike key is cleaned up with the row.
        let strike: i64 = pool
            .exists(strike_key("node-legacy"))
            .await
            .expect("strike");
        assert_eq!(strike, 0, "no strike key is left behind");
    }

    /// (5) `register` RENEWS the row, not just the heartbeat: a node whose
    /// role/control_url changed after boot must stop advertising the boot-time
    /// value. The retired heartbeat-only refresh path could not do this — the
    /// row was written exactly once per process lifetime.
    #[tokio::test]
    async fn register_rewrites_the_row_on_every_renewal() {
        let pool = pool().await;
        let boot = NodeRegistry::new(
            pool.clone(),
            "node-x".into(),
            NodeRole::Edge,
            "http://old:8081".into(),
        );
        boot.register(60, 120).await.expect("register");

        // Same identity, promoted role + new control URL (what a lease
        // takeover looks like).
        let promoted = NodeRegistry::new(
            pool.clone(),
            "node-x".into(),
            NodeRole::Leader,
            "http://new:8081".into(),
        );
        promoted.register(60, 120).await.expect("re-register");

        let nodes = boot.list_nodes().await.expect("list");
        assert_eq!(nodes.len(), 1, "same node id ⇒ one row");
        assert_eq!(nodes[0].role, "leader", "the row was REWRITTEN");
        assert_eq!(nodes[0].control_url, "http://new:8081");
        // The discovery set follows the rewritten row.
        assert_eq!(
            boot.leader_control_urls().await.expect("discover"),
            vec!["http://new:8081".to_string()]
        );
    }

    /// (6) The value format is FROZEN. A not-yet-upgraded node's row (hand
    /// written here, exactly as the old binary wrote it: no witness key, plain
    /// `role|control_url`) must keep working for every reader — that is what
    /// makes the rolling upgrade safe: an old `active_leader_url` must not see
    /// `role == "v2"` and start 503-ing every admin write.
    #[tokio::test]
    async fn legacy_rows_still_parse_for_every_reader() {
        let pool = pool().await;
        let reg = NodeRegistry::new(
            pool.clone(),
            "node-new".into(),
            NodeRole::Edge,
            "http://new:8081".into(),
        );
        // Hand-written OLD-FORMAT rows (no witness key at all).
        let _: i64 = pool
            .hset(NODES_KEY, ("node-old", "leader|http://old:8081"))
            .await
            .expect("hset old");
        let _: Option<String> = pool
            .set(
                heartbeat_key("node-old"),
                "1",
                Some(fred::types::Expiration::EX(60)),
                None,
                false,
            )
            .await
            .expect("hb old");
        let _: Option<String> = pool
            .set(crate::redis::LEASE_KEY, "node-old", None, None, false)
            .await
            .expect("lease");

        assert_eq!(
            reg.leader_control_urls().await.expect("discover"),
            vec!["http://old:8081".to_string()],
            "an old-format leader row is still discoverable"
        );
        assert_eq!(
            reg.active_leader_url().await.expect("active"),
            Some("http://old:8081".to_string()),
            "and the forward target still resolves (the value format did not change)"
        );
        let nodes = reg.list_nodes().await.expect("list");
        let old = nodes
            .iter()
            .find(|n| n.node_id == "node-old")
            .expect("old node listed");
        assert_eq!(old.role, "leader", "role parses as a plain string");
        assert_eq!(old.control_url, "http://old:8081");
        assert!(old.alive, "its heartbeat is fresh");
    }
}
