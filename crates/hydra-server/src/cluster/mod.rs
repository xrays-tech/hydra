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

// Arachne control plane (ADR-0001). Gated on its own feature, independent of
// `cluster-redis`: the default build must neither compile nor link it, and a
// single-node deployment must not reach any of its code paths.
#[cfg(feature = "arachne")]
pub mod arachne_entities;
#[cfg(feature = "arachne")]
pub mod arachne_keys;
#[cfg(feature = "arachne")]
pub mod arachne_materialize;
#[cfg(feature = "arachne")]
pub mod arachne_node;
#[cfg(feature = "arachne")]
pub mod arachne_store;

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
    /// Parse `HYDRA_ROLE` (default `all`). A value that is not `leader`/`edge` is
    /// treated as `all`, so a typo never leaves a node unable to proxy — but it is
    /// never allowed to stay quiet.
    ///
    /// The fallback is deliberate and documented (`dev-docs/jiqun-deploy.md`): a
    /// typo must not leave a node unable to proxy. What it is NOT allowed to do is
    /// stay quiet, because every cluster-only check in `main.rs`
    /// (`HYDRA_REDIS_URL`, `HYDRA_CLUSTER_TOKEN`, `HYDRA_CONTROL_URL`, and the
    /// `HYDRA_ADMIN_TOKEN` requirement that only applies to clustered roles) is
    /// gated on [`is_cluster`](Self::is_cluster). A node that was MEANT to be a
    /// leader and fell back to `all` therefore starts "successfully" while: skipping
    /// all four of those validations, serving tenant writes from its OWN local
    /// SQLite (which the next `restore_config` overwrites), never joining the
    /// election, and answering 404 on `/healthz/leader` — which hangs a Kubernetes
    /// rollout forever with no error anywhere. Hence `error!`, and hence
    /// [`ignored_cluster_wiring`], which names the settings being dropped.
    ///
    /// The diagnosis is attached to the CONDITION (cluster wiring configured while
    /// this node is not a cluster node), not to one arm that reaches it — measured
    /// 2026-10-01 on the wire: with `HYDRA_ROLE` **unset** and all three wiring
    /// variables set, the node used to start as `all` with **no log line at all**,
    /// i.e. the one path where the operator most likely just forgot the variable was
    /// the one path the diagnostic could not reach. `HYDRA_ROLE=all` — a value
    /// `lib.rs` and both deployment docs name — used to be reported as an *unknown*
    /// role, which was a false claim about a documented value.
    #[must_use]
    pub fn from_env() -> Self {
        let raw = std::env::var("HYDRA_ROLE").ok();
        let role = role_from_raw(raw.as_deref());
        let mut present: Vec<(&str, Option<String>)> = Vec::with_capacity(CLUSTER_ONLY_ENV.len());
        for name in CLUSTER_ONLY_ENV {
            present.push((name, std::env::var(name).ok()));
        }
        let wiring = ignored_cluster_wiring(
            &present
                .iter()
                .map(|(name, value)| (*name, value.as_deref()))
                .collect::<Vec<_>>(),
        );
        match role_notice(raw.as_deref(), wiring) {
            RoleNotice::Quiet => {}
            RoleNotice::Unknown { raw } => tracing::warn!(
                role = %raw,
                "unrecognised HYDRA_ROLE; falling back to single-node 'all' mode"
            ),
            RoleNotice::WiringIgnored { why, ignored } => tracing::error!(
                ignored = %ignored,
                "cluster wiring is configured but {why}; falling back to single-node 'all' mode — \
                 the wiring listed in `ignored` is NOT used (no registry, no lease, no L2 cache, \
                 and tenant writes land in the LOCAL database)"
            ),
        }
        role
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
/// tier buys nothing.
///
/// Two nodes sharing one `HOSTNAME` is a MISCONFIGURATION with three effects,
/// and the last one is the dangerous one: they share a registry row, the
/// shutdown `unregister()` of either deletes that shared row (including the
/// peer's registration), and — because this same id is the LEASE identity
/// (`LeaderElection` renews whenever `GET hydra:lease == our node_id`) — BOTH
/// processes would consider themselves the lease holder, i.e. split brain.
/// Runbook: `dev-docs/ops.md` §13.6.
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

/// The one value that means "not a cluster node" on purpose (`lib.rs`, `cluster.md`
/// and `jiqun-deploy.md` all name `all` as the default role).
const NON_CLUSTER_ROLE: &str = "all";

/// `HYDRA_ROLE` → role, with no environment access (so tests are parallel-safe).
///
/// Surrounding whitespace is folded before matching — a manifest value of
/// `"leader "` is a leading/trailing-space accident, not a different role, and
/// treating it as unrecognised would silently drop the node out of the cluster.
/// Case is deliberately NOT folded (unlike `HYDRA_REDIS_MODE`, which folds it):
/// every shipped manifest spells the role in lower case, and `parses_roles` pins
/// the case-sensitivity.
fn role_from_raw(raw: Option<&str>) -> NodeRole {
    match raw.map(str::trim) {
        Some("leader") => NodeRole::Leader,
        Some("edge") => NodeRole::Edge,
        _ => NodeRole::All,
    }
}

/// What `from_env` must say about a role that is not a cluster role.
///
/// Split out as a pure value so every row of the table is testable without
/// touching the process env; the logging itself stays in [`NodeRole::from_env`].
#[derive(Clone, Debug, PartialEq, Eq)]
enum RoleNotice {
    /// Nothing to say: either this IS a cluster node (the wiring is used), or the
    /// role is unset/blank/`all` with no cluster wiring — which is exactly the
    /// documented single-node default.
    Quiet,
    /// An unrecognised value with no cluster wiring: a WARN naming the value.
    Unknown { raw: String },
    /// Cluster wiring is configured while this node is not a cluster node: an ERROR
    /// naming every variable that will be ignored (`why` names the role side of the
    /// mismatch, because "unset", "blank", `all` and a typo are four different
    /// operator mistakes with the same consequence).
    WiringIgnored { why: String, ignored: String },
}

/// Decide what [`NodeRole::from_env`] should log. Pure, so the whole table is a test.
///
/// The condition that matters is `wiring.is_some() && !role.is_cluster()`; reaching
/// it through an unset variable, a blank variable, `all`, or a typo must produce the
/// same diagnosis, because the consequence is identical (the node is standalone:
/// no registry, no lease, no L2 cache, tenant writes to its own SQLite).
fn role_notice(raw: Option<&str>, wiring: Option<String>) -> RoleNotice {
    if role_from_raw(raw).is_cluster() {
        // leader/edge: the wiring is used, so there is nothing to warn about.
        return RoleNotice::Quiet;
    }
    match (raw.map(str::trim), wiring) {
        (None, None) | (Some(""), None) => RoleNotice::Quiet,
        (Some(NON_CLUSTER_ROLE), None) => RoleNotice::Quiet,
        (None, Some(ignored)) => RoleNotice::WiringIgnored {
            why: "HYDRA_ROLE is not set".to_string(),
            ignored,
        },
        (Some(""), Some(ignored)) => RoleNotice::WiringIgnored {
            why: "HYDRA_ROLE is blank".to_string(),
            ignored,
        },
        (Some(NON_CLUSTER_ROLE), Some(ignored)) => RoleNotice::WiringIgnored {
            why: format!("HYDRA_ROLE={NON_CLUSTER_ROLE:?} is not a cluster role"),
            ignored,
        },
        (Some(unknown), Some(ignored)) => RoleNotice::WiringIgnored {
            why: format!("HYDRA_ROLE={unknown:?} is not a known role"),
            ignored,
        },
        (Some(unknown), None) => RoleNotice::Unknown {
            raw: unknown.to_string(),
        },
    }
}

/// Every environment variable that ONLY a cluster role uses — the single owner of "what counts as
/// cluster wiring".
///
/// [`ignored_cluster_wiring`] reports the subset that is actually set, so this table is what the
/// operator sees named in the "you configured cluster wiring but this node is not a cluster node"
/// ERROR. It used to be three literals inside that function, and the other seven were dropped in
/// silence (measured 2026-10-01: `HYDRA_NODE_ID`, `HYDRA_CONTROL_POLL_MS`, `HYDRA_REDIS_MODE`,
/// `HYDRA_PUBLIC_URL`, `HYDRA_LEADER_LEASE_MS`, `HYDRA_REGISTRY_STALE_GRACE_SECS` and
/// `HYDRA_FORWARD_TIMEOUT_SECS` were all configured, all ignored, and none of them mentioned) —
/// the "a hand-written list eats objects" family. Every name here has its read site in code that
/// runs only for a cluster role, which is what `check_cluster_env.cjs` checks in both directions.
///
/// NOT in this list, on purpose: `HYDRA_USAGE_SINK` / `HYDRA_CLICKHOUSE_URL` (mandatory in cluster
/// mode but equally meaningful single-node), `HYDRA_TENANT_API_CONVERGE_TIMEOUT_MS` (the tenant API
/// exists in every role) and `HYDRA_RESEAL_SECRETS` (a one-shot maintenance switch that has nothing
/// to do with topology).
const CLUSTER_ONLY_ENV: [&str; 10] = [
    "HYDRA_REDIS_URL",                 // the backbone itself
    "HYDRA_REDIS_MODE",                // ...and its topology (read only when `is_cluster()`)
    "HYDRA_CLUSTER_TOKEN",             // shared control-plane token
    "HYDRA_CONTROL_URL",               // leader/edge snapshot polling
    "HYDRA_PUBLIC_URL",                // what this node registers as (registry)
    "HYDRA_NODE_ID",                   // registry/lease identity
    "HYDRA_CONTROL_POLL_MS",           // snapshot poll interval (control client)
    "HYDRA_LEADER_LEASE_MS",           // election lease length
    "HYDRA_REGISTRY_STALE_GRACE_SECS", // when a silent node is treated as gone
    "HYDRA_FORWARD_TIMEOUT_SECS",      // standby → active admin-write forwarding
];

/// Which of the cluster-only settings are configured while the role is NOT a cluster role.
///
/// Returns `None` when none of them is configured (then the fallback is exactly the documented
/// single-node default and a WARN is enough), or a comma-separated list of the variable names that
/// will be IGNORED. Kept pure — it takes the `(name, value)` pairs instead of reading the
/// environment — so the diagnostic itself is testable, which matters because the failure it explains
/// (cluster wiring on a node that fell back to `all`) is invisible in the logs otherwise.
///
/// A blank value is not configuration (a bare `HYDRA_REDIS_URL=` in a compose file), the same rule
/// the role side uses.
fn ignored_cluster_wiring(present: &[(&str, Option<&str>)]) -> Option<String> {
    let names: Vec<&str> = present
        .iter()
        .filter(|(_, value)| value.is_some_and(|v| !v.trim().is_empty()))
        .map(|(name, _)| *name)
        .collect();
    if names.is_empty() {
        None
    } else {
        Some(names.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A typo'd `HYDRA_ROLE` on a node that HAS cluster wiring must say what it is
    /// dropping. `from_env`'s fallback is documented and stays (a typo must not make
    /// a node unable to proxy), but every cluster-only startup validation in
    /// `main.rs` is gated on `is_cluster()`, so a leader that falls back to `all`
    /// boots "fine" while skipping them, never joining the election, and writing
    /// tenant config to its own local SQLite. Naming the ignored variables is the
    /// difference between a five-second fix and an outage nobody can explain.
    ///
    /// Falsification: return `None` unconditionally and every `Some` assertion fails.
    #[test]
    fn a_role_fallback_names_the_cluster_wiring_it_ignores() {
        let pair = |name: &'static str, value: Option<&'static str>| (name, value);

        // Nothing cluster-shaped configured: the documented single-node default.
        assert_eq!(ignored_cluster_wiring(&[]), None);
        assert_eq!(
            ignored_cluster_wiring(&[
                pair("HYDRA_REDIS_URL", Some("  ")),
                pair("HYDRA_CLUSTER_TOKEN", Some("")),
                pair("HYDRA_NODE_ID", None),
            ]),
            None,
            "blank/whitespace values are not configuration"
        );

        // The typo case: leader wiring present, role unrecognised. EVERY configured name must be
        // reported — the shipped bug (round 193) was a three-literal list that named
        // HYDRA_REDIS_URL/CLUSTER_TOKEN/CONTROL_URL and silently dropped the other seven.
        let named = ignored_cluster_wiring(&[
            pair("HYDRA_REDIS_URL", Some("redis://cache:6379")),
            pair("HYDRA_CLUSTER_TOKEN", Some("cluster-token-16chars")),
            pair("HYDRA_CONTROL_URL", Some("http://control-a:8081")),
            pair("HYDRA_NODE_ID", Some("control-a-0")),
            pair("HYDRA_LEADER_LEASE_MS", Some("3000")),
            pair("HYDRA_REDIS_MODE", Some("single")),
        ])
        .expect("cluster wiring must be reported");
        for var in [
            "HYDRA_REDIS_URL",
            "HYDRA_CLUSTER_TOKEN",
            "HYDRA_CONTROL_URL",
            "HYDRA_NODE_ID",
            "HYDRA_LEADER_LEASE_MS",
            "HYDRA_REDIS_MODE",
        ] {
            assert!(named.contains(var), "{var} missing from {named:?}");
        }

        // Partial wiring is reported too (a control URL alone is still a typo), and an unset one
        // never appears in the list.
        let only_url = ignored_cluster_wiring(&[
            pair("HYDRA_REDIS_URL", None),
            pair("HYDRA_CONTROL_URL", Some("http://control-a:8081")),
        ])
        .expect("a lone control URL is still cluster wiring");
        assert_eq!(only_url, "HYDRA_CONTROL_URL");
    }

    /// The table is the single owner of "what counts as cluster wiring", so it is asserted instead
    /// of assumed: this is the list an operator reads in the fallback ERROR, and each name here has
    /// a read site that only a cluster role reaches (`check_cluster_env.cjs` verifies the other
    /// direction — that nothing in `src/cluster/` reads an unlisted name).
    ///
    /// Falsification: drop any name from `CLUSTER_ONLY_ENV` and this fails with the missing one.
    #[test]
    fn the_cluster_only_env_table_is_exactly_the_cluster_topology() {
        assert_eq!(
            CLUSTER_ONLY_ENV,
            [
                "HYDRA_REDIS_URL",
                "HYDRA_REDIS_MODE",
                "HYDRA_CLUSTER_TOKEN",
                "HYDRA_CONTROL_URL",
                "HYDRA_PUBLIC_URL",
                "HYDRA_NODE_ID",
                "HYDRA_CONTROL_POLL_MS",
                "HYDRA_LEADER_LEASE_MS",
                "HYDRA_REGISTRY_STALE_GRACE_SECS",
                "HYDRA_FORWARD_TIMEOUT_SECS",
            ],
            "the cluster-only table is what the fallback ERROR names — changing it is a user-visible change"
        );
        // No duplicates, and every name is a real `HYDRA_*` variable spelled the documented way.
        for (i, name) in CLUSTER_ONLY_ENV.iter().enumerate() {
            assert!(
                CLUSTER_ONLY_ENV[..i].iter().all(|other| other != name),
                "{name} is listed twice"
            );
            assert!(
                name.starts_with("HYDRA_")
                    && name[6..]
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c == '_'),
                "{name} is not a HYDRA_* variable name"
            );
        }
    }

    /// The parse table, against the PRODUCTION function. Until round 192 this test
    /// ran a private copy of the `match` living in the test module, so it would have
    /// stayed green after `role_from_raw` changed — a duplicate that only *looked*
    /// like coverage.
    #[test]
    fn parses_roles() {
        assert_eq!(role_from_raw(Some("leader")), NodeRole::Leader);
        assert_eq!(role_from_raw(Some("edge")), NodeRole::Edge);
        assert_eq!(role_from_raw(None), NodeRole::All);
        assert_eq!(
            role_from_raw(Some("ALL")),
            NodeRole::All,
            "case-sensitive, unrecognised → all"
        );
        assert_eq!(role_from_raw(Some("typo")), NodeRole::All);
        // Whitespace is folded (a manifest value of "leader " is not another role);
        // without this the node silently leaves the cluster it was configured for.
        assert_eq!(role_from_raw(Some(" leader ")), NodeRole::Leader);
        assert_eq!(role_from_raw(Some("edge\n")), NodeRole::Edge);
        // A blank value is the unset default, not a role.
        assert_eq!(role_from_raw(Some("")), NodeRole::All);
        assert_eq!(role_from_raw(Some("   ")), NodeRole::All);
    }

    /// The whole notice table. Each row is a distinct operator mistake with the same
    /// consequence, so each row must be asserted separately: the shipped bug
    /// (round 192, measured on the wire) was that the `unset` row was SILENT while
    /// the `typo` row shouted, and that the documented `all` row was called unknown.
    ///
    /// Falsification: make `role_notice` return `Quiet` for `(None, Some(_))` and the
    /// `unset_role_with_wiring` row fails; return `Unknown` for `all` and the
    /// `documented_all` row fails.
    #[test]
    fn role_notice_covers_every_way_to_be_a_non_cluster_node() {
        let wiring = || Some("HYDRA_REDIS_URL, HYDRA_CLUSTER_TOKEN".to_string());

        // A cluster node uses its wiring: nothing to say.
        assert_eq!(role_notice(Some("leader"), wiring()), RoleNotice::Quiet);
        assert_eq!(role_notice(Some("edge"), None), RoleNotice::Quiet);

        // The documented single-node default, with nothing cluster-shaped present.
        assert_eq!(role_notice(None, None), RoleNotice::Quiet);
        assert_eq!(role_notice(Some(""), None), RoleNotice::Quiet);
        assert_eq!(role_notice(Some("all"), None), RoleNotice::Quiet);

        // A typo with no wiring: a WARN naming the value.
        assert_eq!(
            role_notice(Some("ledge"), None),
            RoleNotice::Unknown {
                raw: "ledge".to_string()
            }
        );

        // Wiring configured + not a cluster node ⇒ the ERROR, through ALL FOUR paths.
        let expected = [
            (None, "HYDRA_ROLE is not set"),
            (Some(""), "HYDRA_ROLE is blank"),
            (Some("all"), "HYDRA_ROLE=\"all\" is not a cluster role"),
            (Some("ledge"), "HYDRA_ROLE=\"ledge\" is not a known role"),
        ];
        for (raw, why) in expected {
            match role_notice(raw, wiring()) {
                RoleNotice::WiringIgnored { why: got, ignored } => {
                    assert_eq!(got, why, "raw={raw:?}");
                    assert_eq!(
                        ignored, "HYDRA_REDIS_URL, HYDRA_CLUSTER_TOKEN",
                        "the ignored list must reach the log, raw={raw:?}"
                    );
                }
                other => panic!("raw={raw:?} must report the ignored wiring, got {other:?}"),
            }
        }

        // `all` is a documented value, so the wiring error must NOT call it unknown —
        // that was the false claim (measured 2026-10-01: `HYDRA_ROLE=all` + wiring
        // logged "unknown HYDRA_ROLE" with the role field printed as "all").
        if let RoleNotice::WiringIgnored { why, .. } = role_notice(Some("all"), wiring()) {
            assert!(
                !why.contains("unknown") && !why.contains("known role"),
                "`all` is documented, not unknown: {why:?}"
            );
        } else {
            panic!("`all` with cluster wiring must still report the wiring it drops");
        }
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
