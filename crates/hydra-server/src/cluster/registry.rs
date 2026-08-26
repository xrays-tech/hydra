//! # Node registry (cluster P4)
//!
//! Every cluster node registers itself in Redis (`hydra:{nodes}` hash +
//! heartbeat key), so edges discover the leader control endpoints and rotate
//! among them — no static `HYDRA_CONTROL_URL` needed (it stays as an override
//! for special cases).
//!
//! **Keys** (single-key operations, topology-safe): `hydra:{nodes}` (hash
//! `node_id → "role|control_url"`) and `hydra:{node:hb}:<id>` (heartbeat with
//! TTL; a node whose heartbeat expired is considered gone).

use fred::clients::Pool;
use fred::prelude::*;

use crate::cluster::NodeRole;
use crate::redis::RedisError;

/// Registry hash (field = node id, value = `role|control_url`).
pub const NODES_KEY: &str = "hydra:{nodes}";
/// Heartbeat key prefix (suffix = node id).
pub const HEARTBEAT_PREFIX: &str = "hydra:{node:hb}:";

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

    /// Register this node + write a heartbeat (`ttl_secs`; the caller
    /// refreshes periodically).
    pub async fn register(&self, ttl_secs: u64) -> Result<(), RedisError> {
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
        Ok(())
    }

    /// Remove this node from the registry (graceful shutdown).
    pub async fn unregister(&self) -> Result<(), RedisError> {
        let _: i64 = self.pool.hdel(NODES_KEY, &self.node_id).await?;
        let _: i64 = self.pool.del(heartbeat_key(&self.node_id)).await?;
        Ok(())
    }

    /// Refresh this node's heartbeat.
    pub async fn refresh_heartbeat(&self, ttl_secs: u64) -> Result<(), RedisError> {
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
        Ok(())
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

// ---------------------------------------------------------------------------
// Tests against the in-process Redis double
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redis::mock::MockRedis;

    async fn pool_with_mock() -> Pool {
        let mock = std::sync::Arc::new(MockRedis::new());
        let cfg = Config {
            mocks: Some(mock),
            ..Default::default()
        };
        let p = Pool::new(cfg, None, None, None, 1).expect("pool");
        p.init().await.expect("init");
        p
    }

    #[tokio::test]
    async fn register_discover_unregister() {
        let a = NodeRegistry::new(
            pool_with_mock().await,
            "node-a".into(),
            NodeRole::Leader,
            "http://a:8081".into(),
        );
        let b = NodeRegistry::new(
            pool_with_mock().await,
            "node-b".into(),
            NodeRole::Edge,
            "http://b:8081".into(),
        );

        a.register(60).await.expect("register a");
        b.register(60).await.expect("register b");

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
            pool_with_mock().await,
            "node-a".into(),
            NodeRole::Leader,
            "http://a:8081".into(),
        );
        // 1-second TTL → expires before the check below (mock uses real time).
        a.register(1).await.expect("register");
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let urls = a.leader_control_urls().await.expect("discover");
        assert!(urls.is_empty(), "expired heartbeat ⇒ node considered gone");
    }

    #[tokio::test]
    async fn list_nodes_reports_liveness_and_lease_holder() {
        let pool = pool_with_mock().await;
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
        a.register(60).await.expect("register a");
        b.register(60).await.expect("register b");
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
}
