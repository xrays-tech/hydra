//! # Cluster mode — node role & cluster configuration (v8 plan P0b+).
//!
//! `NodeRole` drives the bootstrap branching in `main.rs`; `ClusterConfig`
//! carries the shared control-plane settings; the submodules implement the
//! snapshot wire format ([`snapshot`]) and the polling client
//! ([`control_client`]).
//!
//! | Role     | Behavior                                                            |
//! |----------|---------------------------------------------------------------------|
//! | `all`    | (default) today's single-node behavior — zero cluster machinery.    |
//! | `leader` | candidate: full node + the control-plane endpoints & lease.         |
//! | `edge`   | stateless data plane: no local SQLite, no admin CRUD; pulls config  |
//! |          | snapshots from the leader and shares state via Redis.               |
//!
//! Cluster mode (leader/edge) is opt-in via `HYDRA_ROLE`; single-node builds
//! keep the zero-dependency behavior unchanged.

use std::fmt;
use std::time::Duration;

pub mod control_client;
#[cfg(feature = "cluster-redis")]
pub mod events;
pub mod forward;
pub mod lease;
#[cfg(feature = "cluster-redis")]
pub mod registry;
pub mod replica;
pub mod snapshot;

/// Node role in a Hydra cluster (v8 plan §2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeRole {
    /// Single-node mode (default): today's behavior, zero cluster machinery.
    All,
    /// Leader candidate: full node (proxy + admin + SQLite) and, once wired,
    /// the control-plane endpoints and lease participation.
    Leader,
    /// Stateless data-plane node: proxy-only, no local SQLite, no admin CRUD.
    Edge,
}

impl NodeRole {
    /// Parse `HYDRA_ROLE` (default `all`). Unknown values are treated as
    /// `all` + a WARN, so a typo never silently disables the proxy.
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::var("HYDRA_ROLE").as_deref() {
            Ok("leader") => Self::Leader,
            Ok("edge") => Self::Edge,
            Ok(other) => {
                tracing::warn!(
                    role = other,
                    "unknown HYDRA_ROLE; falling back to single-node 'all' mode"
                );
                Self::All
            }
            Err(_) => Self::All,
        }
    }

    /// Whether this role participates in a cluster (leader/edge).
    #[must_use]
    pub fn is_cluster(self) -> bool {
        matches!(self, Self::Leader | Self::Edge)
    }

    /// Whether this node runs the admin CRUD API (leader/all only).
    #[must_use]
    pub fn has_admin_crud(self) -> bool {
        !matches!(self, Self::Edge)
    }
}

impl fmt::Display for NodeRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::All => f.write_str("all"),
            Self::Leader => f.write_str("leader"),
            Self::Edge => f.write_str("edge"),
        }
    }
}

/// Shared control-plane configuration (cluster P1, parsed from env).
#[derive(Clone, Debug)]
pub struct ClusterConfig {
    pub role: NodeRole,
    /// Leader control endpoint base (`HYDRA_CONTROL_URL`), required on edges.
    pub control_url: Option<String>,
    /// Shared control-plane token (`HYDRA_CLUSTER_TOKEN`), required in
    /// cluster mode (fail-closed).
    pub cluster_token: Option<String>,
    /// Control poll interval (`HYDRA_CONTROL_POLL_MS`, default 1000 ms).
    pub poll_interval: Duration,
    /// Stable node identity (`HYDRA_NODE_ID`, else `node-<random hex>`): the
    /// lease holder id and the future registry identity.
    pub node_id: String,
}

impl ClusterConfig {
    /// Parse the cluster configuration from the environment.
    #[must_use]
    pub fn from_env(role: NodeRole) -> Self {
        Self {
            role,
            control_url: std::env::var("HYDRA_CONTROL_URL")
                .ok()
                .filter(|u| !u.is_empty()),
            cluster_token: std::env::var("HYDRA_CLUSTER_TOKEN")
                .ok()
                .filter(|t| !t.is_empty()),
            poll_interval: std::env::var("HYDRA_CONTROL_POLL_MS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_millis)
                .unwrap_or(Duration::from_millis(1000)),
            node_id: std::env::var("HYDRA_NODE_ID")
                .ok()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| format!("node-{:x}", rand::random::<u64>())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure parse helper so tests don't touch the process env (parallel-safe).
    fn parse(raw: Option<&str>) -> NodeRole {
        match raw {
            Some("leader") => NodeRole::Leader,
            Some("edge") => NodeRole::Edge,
            Some(_) | None => NodeRole::All,
        }
    }

    #[test]
    fn parses_roles() {
        assert_eq!(parse(Some("leader")), NodeRole::Leader);
        assert_eq!(parse(Some("edge")), NodeRole::Edge);
        assert_eq!(parse(None), NodeRole::All);
        assert_eq!(
            parse(Some("ALL")),
            NodeRole::All,
            "case-sensitive, unknown → all"
        );
        assert_eq!(parse(Some("typo")), NodeRole::All);
    }

    #[test]
    fn cluster_flag_and_admin() {
        assert!(NodeRole::Leader.is_cluster());
        assert!(NodeRole::Edge.is_cluster());
        assert!(!NodeRole::All.is_cluster());

        assert!(NodeRole::All.has_admin_crud());
        assert!(NodeRole::Leader.has_admin_crud());
        assert!(!NodeRole::Edge.has_admin_crud(), "edge has no admin CRUD");
    }
}
