//! # Cluster mode — node role & cluster configuration.
//!
//! `NodeRole` drives the bootstrap branching in `main.rs`; `ClusterConfig`
//! carries the shared node identity. Redis (`cluster::events`) remains the
//! data-plane backbone (rate limits, breaker votes, cache invalidation), but it
//! no longer carries a control plane: config is authoritative in the shared
//! relational database, so there is no raft membership and no leader election.
//!
//! | Role     | Behavior                                                            |
//! |----------|---------------------------------------------------------------------|
//! | `all`    | (default) single-node behavior — no Redis required.                 |
//! | `cluster`| a Redis data-plane deployment; every node writes the shared DB.     |
//!
//! Cluster mode is opt-in via `HYDRA_REDIS_URL`: a node with a non-empty Redis
//! URL participates in the Redis-backed data plane, and a node without one is
//! standalone. The former `HYDRA_CLUSTER_PEERS` member list and the raft/Arachne
//! control plane were retired by the shared-DB architecture.

use std::fmt;

#[cfg(feature = "cluster-redis")]
pub mod config_bus;
#[cfg(feature = "cluster-redis")]
pub mod events;

/// Node role in a Hydra deployment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeRole {
    /// Single-node mode (default): no cluster data-plane machinery.
    All,
    /// A Redis-backed data-plane deployment. Every node serves traffic, writes
    /// the shared relational database directly, and shares rate limits/breaker
    /// votes/invalidation through Redis. There is no leader.
    Cluster,
}

impl NodeRole {
    /// Decide this node's role from the environment.
    ///
    /// Cluster mode is exactly "a non-empty `HYDRA_REDIS_URL` is configured".
    /// The member list and raft topology variables are retired; setting them no
    /// longer makes a node a cluster member. The startup diagnostic still names
    /// retired variables so a stale deployment is told they do nothing.
    #[must_use]
    pub fn from_env() -> Self {
        let mut present: Vec<(String, Option<String>)> = Vec::new();
        for name in CLUSTER_ONLY_ENV.iter().chain(RETIRED_CLUSTER_ENV.iter()) {
            present.push(((*name).to_string(), std::env::var(name).ok()));
        }
        let live_wiring: Vec<String> = present
            .iter()
            .filter(|(name, value)| {
                CLUSTER_ONLY_ENV.contains(&name.as_str())
                    && value.as_deref().is_some_and(|v| !v.trim().is_empty())
            })
            .map(|(name, _)| name.clone())
            .collect();
        let retired: Vec<String> = present
            .iter()
            .filter(|(name, value)| {
                RETIRED_CLUSTER_ENV.contains(&name.as_str())
                    && value.as_deref().is_some_and(|v| !v.trim().is_empty())
            })
            .map(|(name, _)| name.clone())
            .collect();

        let decision = cluster_decision(
            std::env::var("HYDRA_REDIS_URL").ok().as_deref(),
            &live_wiring,
            &retired,
        );
        match &decision.notice {
            ClusterNotice::Quiet => {}
            // ONE line, with each group also a structured field: an operator (or
            // a grep) reading a single line gets the whole story, and the drill
            // can assert on the names without parsing prose. `None` for an empty
            // group records no field at all.
            ClusterNotice::Dropped {
                wiring_without_redis,
                retired,
            } => tracing::error!(
                wiring_without_redis =
                    (!wiring_without_redis.is_empty()).then_some(wiring_without_redis.as_str()),
                retired = (!retired.is_empty()).then_some(retired.as_str()),
                "{}",
                dropped_notice_text(wiring_without_redis, retired)
            ),
        }
        decision.role
    }

    /// Whether this role participates in a cluster.
    #[must_use]
    pub fn is_cluster(self) -> bool {
        matches!(self, Self::Cluster)
    }
}

impl fmt::Display for NodeRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::All => f.write_str("all"),
            Self::Cluster => f.write_str("cluster"),
        }
    }
}

/// Shared cluster configuration (parsed from env).
#[derive(Clone, Debug)]
pub struct ClusterConfig {
    pub role: NodeRole,
    /// Stable node identity (`HYDRA_NODE_ID`, else hostname, else random). Used
    /// by breaker votes and Redis watermarks; it is not a raft member id.
    pub node_id: String,
}

impl ClusterConfig {
    /// Parse the cluster configuration from the environment.
    #[must_use]
    pub fn from_env(role: NodeRole) -> Self {
        Self {
            role,
            node_id: node_id_from(
                std::env::var("HYDRA_NODE_ID").ok().as_deref(),
                std::env::var("HOSTNAME").ok().as_deref(),
            ),
        }
    }
}

/// This node's identity: `HYDRA_NODE_ID` → `HOSTNAME` → random.
///
/// The middle tier is what stops every restart from creating a brand-new
/// breaker-vote identity. It requires STABLE pod names, i.e. a StatefulSet (or
/// a Deployment with a pinned name): under a plain Deployment `HOSTNAME`
/// changes on every restart and this tier buys nothing.
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

/// Variables that ONLY a cluster data-plane node uses — the single owner of
/// "what counts as cluster wiring", and therefore what the standalone diagnostic
/// names.
///
/// `HYDRA_REDIS_URL` is the decision itself: setting the other names without it
/// is the mistake the diagnostic exists for.
const CLUSTER_ONLY_ENV: [&str; 5] = [
    "HYDRA_REDIS_URL",      // the data-plane backbone — the decision itself
    "HYDRA_REDIS_MODE",     // ...and its topology
    "HYDRA_NODE_ID",        // this node's identity (breaker votes / watermarks)
    "HYDRA_BREAKER_QUORUM", // minimum live breaker votes
    "HYDRA_CONFIG_POLL_MS", // config-bus poll fallback (missed pub/sub messages)
];

/// Variables the shared-DB architecture RETIRES, once their readers are gone.
///
/// A name belongs here only when NOTHING reads it: the diagnostic tells an
/// operator that the setting does nothing. The raft/Arachne control plane and
/// its member list were removed in the shared-DB rewrite; `HYDRA_REDIS_URL` is
/// now the only cluster switch, and config authority is the shared relational
/// database rather than a replicated head.
const RETIRED_CLUSTER_ENV: [&str; 15] = [
    "HYDRA_FORWARD_TIMEOUT_SECS",
    "HYDRA_LEADER_LEASE_MS",
    "HYDRA_CONTROL_URL",
    "HYDRA_CONTROL_POLL_MS",
    "HYDRA_PUBLIC_URL",
    "HYDRA_REGISTRY_STALE_GRACE_SECS",
    "HYDRA_FAILOVER_GRACE_MS",
    "HYDRA_ROLE",
    "HYDRA_CLUSTER_TOKEN",
    "HYDRA_EDGE",
    // The raft/Arachne control plane and its static member list.
    "HYDRA_CLUSTER_PEERS",
    "HYDRA_CLUSTER_ID",
    "HYDRA_ARACHNE_LISTEN",
    "HYDRA_ARACHNE_DATA_DIR",
    "HYDRA_ARACHNE_SNAPSHOT_THRESHOLD_MB",
];

/// Which of `present` are retired, according to `table`.
///
/// `table` is a parameter rather than a read of [`RETIRED_CLUSTER_ENV`] so the
/// mechanism can be tested independently of the real table's contents.
fn retired_present(present: &[String], table: &[&str]) -> Vec<String> {
    present
        .iter()
        .filter(|name| table.contains(&name.as_str()))
        .cloned()
        .collect()
}

/// What a node should say about its own cluster-shaped configuration.
///
/// Split out as a pure value so every row of the table is testable without
/// touching the process env; the logging itself stays in [`NodeRole::from_env`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClusterNotice {
    /// Nothing to say: either this IS a cluster node, or nothing cluster-shaped
    /// is set — which is the documented single-node default.
    Quiet,
    /// Something the operator configured is NOT in effect, named. ONE variant
    /// for both classes of problem, because both are usually true at once on a
    /// node mid-migration and a message that reports only one of them reads as
    /// the whole story.
    Dropped {
        /// Live cluster wiring that does nothing because `HYDRA_REDIS_URL` is
        /// not set. Empty when the Redis URL IS set (the wiring is then in use).
        wiring_without_redis: String,
        /// Variables this architecture retired, ignored by the product. Empty
        /// when none is set.
        retired: String,
    },
}

/// The decision about this process's cluster role, plus what to say about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClusterDecision {
    /// The role.
    pub role: NodeRole,
    /// What the startup path must log.
    pub notice: ClusterNotice,
}

/// Decide this node's role from the Redis URL, and what to say about the rest.
///
/// Pure — it takes the already-read values — so every row is a test.
pub fn cluster_decision(
    redis_url: Option<&str>,
    live_wiring: &[String],
    retired: &[String],
) -> ClusterDecision {
    cluster_decision_with(redis_url, live_wiring, retired, &RETIRED_CLUSTER_ENV)
}

/// [`cluster_decision`] against an explicit retirement table.
///
/// Split out so the classification is testable against a fixture table as well
/// as the real one.
pub fn cluster_decision_with(
    redis_url: Option<&str>,
    live_wiring: &[String],
    retired: &[String],
    retired_table: &[&str],
) -> ClusterDecision {
    let clustered = matches!(redis_url, Some(v) if !v.trim().is_empty());
    // Retirement applies to a healthy cluster node too (the setting does
    // nothing either way), while "wiring without Redis" is by definition only
    // true on a node that is NOT clustered. Both are collected before deciding
    // what to say: see `ClusterNotice::Dropped`.
    let retired = retired_present(retired, retired_table).join(", ");
    let wiring_without_redis = if clustered {
        String::new()
    } else {
        live_wiring.join(", ")
    };

    let role = if clustered {
        NodeRole::Cluster
    } else {
        NodeRole::All
    };
    let notice = if retired.is_empty() && wiring_without_redis.is_empty() {
        ClusterNotice::Quiet
    } else {
        ClusterNotice::Dropped {
            wiring_without_redis,
            retired,
        }
    };
    ClusterDecision { role, notice }
}

/// The operator-facing text for [`ClusterNotice::Dropped`], ONE sentence per
/// non-empty group.
///
/// Pure and unit-tested because the two sentences are contract: the retirement
/// sentence is what tells an operator which names to delete, and the wiring
/// sentence says why the settings they kept do nothing. Joining them into one
/// message — rather than returning early on either — is deliberate: a node that
/// is BOTH standalone and carrying retired names must hear both, in a single
/// line, so neither mistake can be mistaken for the whole story.
#[must_use]
pub fn dropped_notice_text(wiring_without_redis: &str, retired: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !wiring_without_redis.is_empty() {
        parts.push(
            "cluster data-plane wiring is configured but HYDRA_REDIS_URL is not set; this node \
             is standalone — the wiring listed in `wiring_without_redis` is NOT used (no shared \
             Redis rate limits/breaker, and config changes are not fanned out across processes)"
                .to_string(),
        );
    }
    if !retired.is_empty() {
        parts.push(
            "these variables were retired by the shared-DB architecture (the raft/Arachne control \
             plane was removed) and are IGNORED; remove them from the deployment so nobody \
             believes they still do something"
                .to_string(),
        );
    }
    parts.join(" — AND, at the same time: ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every string literal in `src`, with comments skipped.
    ///
    /// A `split('"')` was the first version and it was WRONG in the expensive
    /// direction: it read the historical message quoted inside a `//` comment
    /// as a live string literal. A guard whose first act is to report a comment
    /// is a guard that gets switched off, and the right fix for "it flagged a
    /// comment" is a scanner, not a narrower pattern. Line comments, block
    /// comments, char literals and lifetimes are skipped; escapes are consumed
    /// so `\"` inside a literal does not end it early.
    fn string_literals(src: &str) -> Vec<String> {
        let b: Vec<char> = src.chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                '/' if b.get(i + 1) == Some(&'/') => {
                    while i < b.len() && b[i] != '\n' {
                        i += 1;
                    }
                }
                '/' if b.get(i + 1) == Some(&'*') => {
                    i += 2;
                    while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                        i += 1;
                    }
                    i += 2;
                }
                '"' => {
                    i += 1;
                    let mut s = String::new();
                    while i < b.len() && b[i] != '"' {
                        if b[i] == '\\' {
                            i += 1;
                            if i < b.len() {
                                s.push(b[i]);
                            }
                        } else {
                            s.push(b[i]);
                        }
                        i += 1;
                    }
                    i += 1;
                    out.push(s);
                }
                '\'' => {
                    // `'x'` / `'\n'` are char literals; a lifetime tick
                    // (`'static`) is not, and treating it as one would swallow
                    // the rest of the file.
                    let close = if b.get(i + 1) == Some(&'\\') {
                        i + 3
                    } else {
                        i + 2
                    };
                    i = if b.get(close) == Some(&'\'') {
                        close + 1
                    } else {
                        i + 1
                    };
                }
                _ => i += 1,
            }
        }
        out
    }

    /// No operator-facing message may name a variable this table retires.
    ///
    /// The retirement diagnostic exists so a deployment that still carries a
    /// retired knob is TOLD. The opposite failure is just as expensive: a
    /// REFUSAL that names one sends the operator to set a variable the same
    /// build reports as ignored. The scan covers `main.rs` plus the `boot/*`
    /// modules (F6 moved the startup literals there): a mention in a comment is
    /// deliberate documentation, and a literal is what an operator can see.
    #[test]
    fn no_operator_facing_message_names_a_retired_variable() {
        let src = concat!(
            include_str!("../main.rs"),
            "\n",
            include_str!("../boot/mod.rs"),
            "\n",
            include_str!("../boot/env.rs"),
            "\n",
            include_str!("../boot/database.rs"),
            "\n",
            include_str!("../boot/state.rs"),
            "\n",
            include_str!("../boot/fanout.rs"),
            "\n",
            include_str!("../boot/listeners.rs"),
        );
        assert!(
            src.contains("fn main("),
            "the scan read {} bytes of main.rs + boot/* and found no `fn main(` — the include path is wrong, \
             so an empty scan would pass for the wrong reason",
            src.len()
        );

        let literals = string_literals(src);
        assert!(
            literals.len() > 100,
            "only {} literal(s) parsed out of main.rs + boot/* — the scan is not looking at the files",
            literals.len()
        );

        for name in RETIRED_CLUSTER_ENV {
            for lit in &literals {
                assert!(
                    !lit.contains(name),
                    "main.rs or boot/* has a string literal naming the RETIRED variable {name}: {:?}\n\
                     A refusal or log line that names it tells the operator to set a variable this \
                     build reports as IGNORED (RETIRED_CLUSTER_ENV). Name the live knob instead — \
                     for cluster mode that is HYDRA_REDIS_URL.",
                    lit.trim()
                );
            }
        }
    }

    /// The live table is exactly the Redis data-plane wiring a cluster node
    /// reads, and it never contains a retired name.
    #[test]
    fn the_cluster_only_env_table_is_exactly_the_redis_data_plane() {
        assert_eq!(
            CLUSTER_ONLY_ENV,
            [
                "HYDRA_REDIS_URL",
                "HYDRA_REDIS_MODE",
                "HYDRA_NODE_ID",
                "HYDRA_BREAKER_QUORUM",
                "HYDRA_CONFIG_POLL_MS",
            ],
            "the cluster-only table is what the fallback diagnostic names — changing it is a \
             user-visible change"
        );
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

    /// The decision table, against the PRODUCTION function, one row per
    /// operator mistake.
    ///
    /// The two SILENT failure modes this table exists for are different
    /// mistakes with different fixes:
    ///   1. cluster data-plane wiring set, Redis URL missing — the node is
    ///      standalone and the wiring is dropped;
    ///   2. a RETIRED variable set — the node may be clustered, but that setting
    ///      does nothing now.
    #[test]
    fn the_cluster_decision_covers_every_way_to_be_standalone() {
        let wiring = || {
            vec![
                "HYDRA_REDIS_MODE".to_string(),
                "HYDRA_BREAKER_QUORUM".to_string(),
            ]
        };

        // A Redis URL, whatever else is set: this IS a cluster node.
        let node = cluster_decision(Some("redis://127.0.0.1:6379"), &[], &[]);
        assert_eq!(node.role, NodeRole::Cluster);
        assert_eq!(node.notice, ClusterNotice::Quiet);

        // Nothing cluster-shaped at all: the documented single-node default.
        for redis in [None, Some(""), Some("   ")] {
            let d = cluster_decision(redis, &[], &[]);
            assert_eq!(d.role, NodeRole::All, "redis={redis:?}");
            assert_eq!(d.notice, ClusterNotice::Quiet, "redis={redis:?}");
        }

        // Mistake 1: wiring without a Redis URL.
        for redis in [None, Some(""), Some("  ")] {
            let d = cluster_decision(redis, &wiring(), &[]);
            assert_eq!(d.role, NodeRole::All, "redis={redis:?}");
            assert_eq!(
                d.notice,
                ClusterNotice::Dropped {
                    wiring_without_redis: "HYDRA_REDIS_MODE, HYDRA_BREAKER_QUORUM".to_string(),
                    retired: String::new(),
                },
                "redis={redis:?}: configured cluster settings must be named, not dropped in silence"
            );
        }

        // Mistake 2: a retired variable. Reported whether or not the node is
        // clustered, because either way the setting does nothing.
        let retired = cluster_decision_with(
            Some("redis://127.0.0.1:6379"),
            &[],
            &["HYDRA_ROLE".to_string()],
            &["HYDRA_ROLE"],
        );
        assert_eq!(
            retired.notice,
            ClusterNotice::Dropped {
                wiring_without_redis: String::new(),
                retired: "HYDRA_ROLE".to_string(),
            },
            "a retired variable must be named even on a healthy cluster node"
        );
        assert_eq!(retired.role, NodeRole::Cluster);

        // Mistake 1 AND mistake 2 at once.
        let both = cluster_decision(None, &wiring(), &["HYDRA_ROLE".to_string()]);
        assert_eq!(both.role, NodeRole::All);
        assert_eq!(
            both.notice,
            ClusterNotice::Dropped {
                wiring_without_redis: "HYDRA_REDIS_MODE, HYDRA_BREAKER_QUORUM".to_string(),
                retired: "HYDRA_ROLE".to_string(),
            },
            "both mistakes must reach the operator, in one notice"
        );
    }

    /// The two sentences of [`ClusterNotice::Dropped`] are contract, and BOTH
    /// must appear when both groups are non-empty.
    #[test]
    fn the_dropped_notice_text_names_every_group_that_is_present() {
        let wiring = "HYDRA_REDIS_MODE, HYDRA_BREAKER_QUORUM";
        let retired = "HYDRA_ROLE, HYDRA_LEADER_LEASE_MS";

        let only_wiring = dropped_notice_text(wiring, "");
        assert!(only_wiring
            .contains("cluster data-plane wiring is configured but HYDRA_REDIS_URL is not set"));
        assert!(
            !only_wiring.contains("were retired"),
            "no retirement sentence was earned"
        );

        let only_retired = dropped_notice_text("", retired);
        assert!(only_retired.contains("were retired by the shared-DB architecture"));
        assert!(
            !only_retired.contains("cluster data-plane wiring is configured"),
            "a cluster node with a retired variable is not standalone — saying so would be a lie"
        );

        let both = dropped_notice_text(wiring, retired);
        assert!(
            both.contains("cluster data-plane wiring is configured but HYDRA_REDIS_URL is not set")
                && both.contains("were retired by the shared-DB architecture"),
            "one message, both mistakes: {both}"
        );
        assert!(both.len() > only_wiring.len() && both.len() > only_retired.len());
    }

    /// A retired variable must be detectable from the environment table alone,
    /// and the detection must not depend on which of the two mistakes also
    /// happened.
    #[test]
    fn retired_variables_are_reported_and_live_ones_are_not() {
        let present =
            |names: &[&str]| -> Vec<String> { names.iter().map(|n| n.to_string()).collect() };
        let table = ["HYDRA_LEADER_LEASE_MS", "HYDRA_CONTROL_POLL_MS"];

        assert_eq!(
            retired_present(
                &present(&[
                    "HYDRA_CLUSTER_TOKEN",
                    "HYDRA_LEADER_LEASE_MS",
                    "HYDRA_CONTROL_POLL_MS",
                ]),
                &table
            ),
            present(&["HYDRA_LEADER_LEASE_MS", "HYDRA_CONTROL_POLL_MS"]),
            "only the retired names may be reported"
        );
        assert!(
            retired_present(&present(&["HYDRA_REDIS_URL", "HYDRA_NODE_ID"]), &table).is_empty(),
            "a healthy cluster node must not be told anything is retired"
        );
        assert!(
            retired_present(&[], &table).is_empty(),
            "nothing configured ⇒ nothing to report"
        );
        // The real table is pinned EXACTLY, so a name can only move in or out
        // deliberately — and the environment guard is what proves that nothing
        // reads a retired name.
        assert_eq!(
            RETIRED_CLUSTER_ENV,
            [
                "HYDRA_FORWARD_TIMEOUT_SECS",
                "HYDRA_LEADER_LEASE_MS",
                "HYDRA_CONTROL_URL",
                "HYDRA_CONTROL_POLL_MS",
                "HYDRA_PUBLIC_URL",
                "HYDRA_REGISTRY_STALE_GRACE_SECS",
                "HYDRA_FAILOVER_GRACE_MS",
                "HYDRA_ROLE",
                "HYDRA_CLUSTER_TOKEN",
                "HYDRA_EDGE",
                "HYDRA_CLUSTER_PEERS",
                "HYDRA_CLUSTER_ID",
                "HYDRA_ARACHNE_LISTEN",
                "HYDRA_ARACHNE_DATA_DIR",
                "HYDRA_ARACHNE_SNAPSHOT_THRESHOLD_MB",
            ],
            "the retirement table changed: update this test AND confirm (via the env guard) that \
             nothing reads the variable that moved"
        );
    }

    /// The retirement table must never overlap the live table.
    #[test]
    fn the_retirement_table_never_shadows_a_live_variable() {
        for name in RETIRED_CLUSTER_ENV {
            assert!(
                !CLUSTER_ONLY_ENV.contains(&name),
                "{name} is listed as BOTH live and retired; the operator would be told it is \
                 ignored while the product still reads it"
            );
        }
        let mut all: Vec<&str> = CLUSTER_ONLY_ENV.to_vec();
        all.extend(RETIRED_CLUSTER_ENV);
        for name in &all {
            assert!(
                name.starts_with("HYDRA_"),
                "{name} is not a HYDRA_* variable"
            );
        }
        let unique: std::collections::BTreeSet<&&str> = all.iter().collect();
        assert_eq!(unique.len(), all.len(), "a name appears in both tables");
    }
}
