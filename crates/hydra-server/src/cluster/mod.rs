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

pub mod content;
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
            node_id: node_id_from(
                std::env::var("HYDRA_NODE_ID").ok().as_deref(),
                std::env::var("HOSTNAME").ok().as_deref(),
            ),
        }
    }
}

/// This node's registry identity: `HYDRA_NODE_ID` → `HOSTNAME` → random.
///
/// The middle tier is what stops every restart from creating a brand-new
/// registry row (and thereby a fresh "offline node" in the admin view). It
/// requires STABLE pod names, i.e. a StatefulSet (or a Deployment with a pinned
/// name): under a plain Deployment `HOSTNAME` changes on every restart and this
/// tier buys nothing. Two nodes sharing one `HOSTNAME` would share ONE registry
/// row — and the shutdown `unregister()` of either would then delete the
/// peer's registration. Both facts are recorded in `dev-docs/ops.md`.
///
/// A pure function on purpose: the module's tests are parallel-safe and never
/// mutate the process environment.
#[must_use]
pub fn node_id_from(node_id_env: Option<&str>, hostname_env: Option<&str>) -> String {
    node_id_env
        .filter(|n| !n.is_empty())
        .or_else(|| hostname_env.filter(|n| !n.is_empty()))
        .map_or_else(
            || format!("node-{:x}", rand::random::<u64>()),
            ToString::to_string,
        )
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

    /// The identity fallback chain (G2). Each tier is asserted separately
    /// because a regression here is SILENT: it only shows up as a growing pile
    /// of offline rows in the admin view after every restart.
    #[test]
    fn node_id_prefers_explicit_then_hostname_then_random() {
        // 1) An explicit id always wins (the pinned-pod-name case).
        assert_eq!(node_id_from(Some("pinned"), Some("host")), "pinned");
        // 2) `HOSTNAME` is the tier that stops restarts from minting new rows.
        assert_eq!(
            node_id_from(None, Some("k3s-hydra-edge-1")),
            "k3s-hydra-edge-1"
        );
        // An EMPTY value is treated as unset (a bare `HYDRA_NODE_ID=` must not
        // register a node under the empty string).
        assert_eq!(node_id_from(Some(""), Some("host")), "host");
        // 3) Both unset (or empty) ⇒ a fresh random id, still recognisable. The
        // empty-string case must land here too, so a bare `HYDRA_NODE_ID=`
        // cannot register a node under "".
        for random in [node_id_from(Some(""), Some("")), node_id_from(None, None)] {
            assert!(random.starts_with("node-"), "got {random}");
            assert!(random.len() > "node-".len(), "got {random}");
        }
        // Distinct random ids (never a shared row for two unconfigured nodes).
        assert_ne!(node_id_from(None, None), node_id_from(None, None));
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
