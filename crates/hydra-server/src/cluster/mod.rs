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
//! Cluster mode is opt-in via `HYDRA_CLUSTER_PEERS` (ADR-0001: the member list IS the
//! decision, replacing `HYDRA_ROLE`, which is retired); single-node builds keep the
//! zero-dependency behavior unchanged.

use std::fmt;

pub mod content;
#[cfg(feature = "cluster-redis")]
pub mod events;
// UNGATED: what is left of this module is `HydratedWire`, the value `ConfigStore::apply_snapshot`
// installs — a plain data type used by every build, not a cluster facility.
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
pub mod arachne_materializer;
#[cfg(feature = "arachne")]
pub mod arachne_node;
#[cfg(feature = "arachne")]
pub mod arachne_publish;
#[cfg(feature = "arachne")]
pub mod arachne_store;

/// Node role in a Hydra cluster (v8 plan §2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeRole {
    /// Single-node mode (default): today's behavior, zero cluster machinery.
    All,
    /// A raft member (ADR-0001 D-2): the FULL node — proxy, admin API, local SQLite, the control
    /// plane — exactly like every other member. The former `edge` state is retired: a node that
    /// serves traffic without the config database cannot materialize the tree, and "all nodes are
    /// configured the same" is what makes the cluster's behaviour predictable.
    Cluster,
}

impl NodeRole {
    /// Decide this node's role from the environment (ADR-0001).
    ///
    /// A cluster node is one with `HYDRA_CLUSTER_PEERS` set; there is no role variable to
    /// mistype and therefore no silent fallback. What CAN still be silent is a deployment
    /// that configures cluster settings without the member list, or that still sets a
    /// variable this plan retired — [`cluster_decision`] names both, and names them on the
    /// node that would otherwise serve from its OWN database without a word.
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
                    && name.as_str() != "HYDRA_CLUSTER_PEERS"
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
            std::env::var("HYDRA_CLUSTER_PEERS").ok().as_deref(),
            &live_wiring,
            &retired,
        );
        match &decision.notice {
            ClusterNotice::Quiet => {}
            ClusterNotice::WiringWithoutMembers { ignored } => tracing::error!(
                ignored = %ignored,
                "cluster wiring is configured but HYDRA_CLUSTER_PEERS is not set; this node is \
                 standalone — the wiring listed in `ignored` is NOT used (no raft membership, no \
                 shared L2 cache, and tenant writes land in the LOCAL database)"
            ),
            ClusterNotice::RetiredIgnored { ignored } => tracing::error!(
                ignored = %ignored,
                "these variables were retired by the Arachne control plane (ADR-0001) and are \
                 IGNORED; remove them from the deployment so nobody believes they still do \
                 something"
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

/// Shared control-plane configuration (cluster P1, parsed from env).
#[derive(Clone, Debug)]
pub struct ClusterConfig {
    pub role: NodeRole,
    /// Shared control-plane token (`HYDRA_CLUSTER_TOKEN`), required in cluster mode
    /// (fail-closed). It is what gates the internal endpoints that remain — the health and status
    /// surfaces — and what identifies a peer.
    pub cluster_token: Option<String>,
    /// Stable node identity (`HYDRA_NODE_ID`, else `node-<random hex>`). This is a raft member's
    /// name, and its POSITION in `HYDRA_CLUSTER_PEERS` is its numeric raft id.
    pub node_id: String,
}

impl ClusterConfig {
    /// Parse the cluster configuration from the environment.
    #[must_use]
    pub fn from_env(role: NodeRole) -> Self {
        Self {
            role,
            cluster_token: std::env::var("HYDRA_CLUSTER_TOKEN")
                .ok()
                .filter(|t| !t.is_empty()),
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
/// (the retired lease election renewed whenever `GET hydra:lease == our node_id`) — BOTH
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

/// Variables that ONLY a cluster node uses — the single owner of "what counts as cluster
/// wiring", and therefore what the standalone diagnostic names.
///
/// ADR-0001 made the member list the decision, so `HYDRA_CLUSTER_PEERS` is the first entry:
/// setting anything else here without it is the mistake the diagnostic exists for.
///
/// `HYDRA_NODE_ID` and `HYDRA_ARACHNE_LISTEN` are read by the Arachne assembly path
/// (`cluster/arachne_node.rs`, through named constants rather than literals) and by
/// `main.rs`; `HYDRA_REDIS_URL` / `HYDRA_REDIS_MODE` / `HYDRA_CLUSTER_TOKEN` are still read
/// by the Redis backbone, which stays for the data-plane hot path (ADR-0001 D-1).
const CLUSTER_ONLY_ENV: [&str; 7] = [
    "HYDRA_CLUSTER_PEERS",  // the member list — the decision itself
    "HYDRA_CLUSTER_ID",     // optional cluster name: refuses a data directory from another cluster
    "HYDRA_REDIS_URL",      // the data-plane backbone (still required in a cluster)
    "HYDRA_REDIS_MODE",     // ...and its topology
    "HYDRA_CLUSTER_TOKEN",  // shared control-plane token
    "HYDRA_NODE_ID",        // this node's identity (registry today, raft id after T1.3)
    "HYDRA_ARACHNE_LISTEN", // where this node's raft transport binds
];

/// Variables the Arachne control plane RETIRES, once their readers are gone.
///
/// A name belongs here only when NOTHING reads it: the table's diagnostic tells an operator that
/// the setting does nothing, and `scripts/check_documented_env.cjs` checks exactly that, in both
/// directions.
///
/// **First entry (2026-10-05, plan T3.3)**: `HYDRA_FORWARD_TIMEOUT_SECS`. Its only reader was the
/// standby→leader admin-write forwarder, which D-3 retired; the environment guard is what noticed
/// that the variable had lost its last reader while still being documented as live.
///
/// **Second entry (2026-10-05, plan T4.1's boot half)**: `HYDRA_LEADER_LEASE_MS`. The lease
/// machine, the snapshot-polling client and the standby materializer are no longer started — the
/// raft write probe is the only source of leadership — so the lease TTL has no reader. Again the
/// environment guard found it, not memory.
///
/// The remaining ADR-0001 retirements are STILL READ by the Redis code that has not been deleted
/// yet (`cluster/lease.rs`, `cluster/registry.rs`, `main.rs`'s cluster validation). They move here
/// in the commit that deletes their readers, not before — claiming otherwise would make the
/// diagnostic a lie. `HYDRA_ROLE` is already unread (the member list replaced it).
const RETIRED_CLUSTER_ENV: [&str; 9] = [
    "HYDRA_FORWARD_TIMEOUT_SECS",
    "HYDRA_LEADER_LEASE_MS",
    "HYDRA_CONTROL_URL",
    "HYDRA_CONTROL_POLL_MS",
    "HYDRA_PUBLIC_URL",
    "HYDRA_REGISTRY_STALE_GRACE_SECS",
    "HYDRA_FAILOVER_GRACE_MS",
    "HYDRA_ROLE",
    // Not a variable a deployment may keep: it is read by nobody, and it is listed so a
    // deployment that still carries it is TOLD rather than left believing it does something.
    "HYDRA_EDGE",
];

/// Which of `present` are retired, according to `table`.
///
/// `table` is a parameter rather than a read of [`RETIRED_CLUSTER_ENV`] so the mechanism can
/// be tested while the real table is still empty — and so a test cannot silently pass by
/// depending on a global that happens to contain the fixture's name.
fn retired_present(present: &[String], table: &[&str]) -> Vec<String> {
    present
        .iter()
        .filter(|name| table.contains(&name.as_str()))
        .cloned()
        .collect()
}

/// What a node should say about its own cluster-shaped configuration.
///
/// Split out as a pure value so every row of the table is testable without touching the
/// process env; the logging itself stays in [`NodeRole::from_env`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClusterNotice {
    /// Nothing to say: either this IS a cluster node, or nothing cluster-shaped is set —
    /// which is the documented single-node default.
    Quiet,
    /// Cluster settings are configured while the member list is missing: an ERROR naming
    /// every variable that will be ignored. This is the mistake that used to be SILENT
    /// (measured 2026-10-01) and that leaves a node serving from its OWN database.
    WiringWithoutMembers { ignored: String },
    /// Retired variables are set: an ERROR naming them, on any node.
    RetiredIgnored { ignored: String },
}

/// The decision about this process's cluster role, plus what to say about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClusterDecision {
    /// The role. `Leader` means "a cluster member" — every member is a leader candidate
    /// under the homogeneous topology (ADR-0001 D-2), which is why `Edge` is not
    /// produced here any more.
    pub role: NodeRole,
    /// What the startup path must log.
    pub notice: ClusterNotice,
}

/// Decide this node's role from the member list, and what to say about the rest.
///
/// Pure — it takes the already-read values — so every row is a test. The two diagnostics
/// are ordered deliberately: a missing member list is reported first (the node is not
/// clustered at all), and retirement is reported in the same run because both are true.
pub fn cluster_decision(
    peers: Option<&str>,
    live_wiring: &[String],
    retired: &[String],
) -> ClusterDecision {
    cluster_decision_with(peers, live_wiring, retired, &RETIRED_CLUSTER_ENV)
}

/// [`cluster_decision`] against an explicit retirement table.
///
/// Split out so the classification is testable before the table has entries (it is empty
/// today — see [`RETIRED_CLUSTER_ENV`]) and so the test exercises the real logic rather than
/// a copy of it.
pub fn cluster_decision_with(
    peers: Option<&str>,
    live_wiring: &[String],
    retired: &[String],
    retired_table: &[&str],
) -> ClusterDecision {
    let clustered = matches!(peers, Some(v) if !v.trim().is_empty());
    let retired = retired_present(retired, retired_table);

    if !retired.is_empty() {
        // Retirement outranks the standalone diagnostic: it applies to a healthy cluster
        // node too, and telling an operator about a dropped setting matters more than
        // telling them about their role.
        return ClusterDecision {
            role: if clustered {
                NodeRole::Cluster
            } else {
                NodeRole::All
            },
            notice: ClusterNotice::RetiredIgnored {
                ignored: retired.join(", "),
            },
        };
    }

    if clustered {
        return ClusterDecision {
            role: NodeRole::Cluster,
            notice: ClusterNotice::Quiet,
        };
    }

    if live_wiring.is_empty() {
        ClusterDecision {
            role: NodeRole::All,
            notice: ClusterNotice::Quiet,
        }
    } else {
        ClusterDecision {
            role: NodeRole::All,
            notice: ClusterNotice::WiringWithoutMembers {
                ignored: live_wiring.join(", "),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every string literal in `src`, with comments skipped.
    ///
    /// A `split('"')` was the first version and it was WRONG in the expensive direction: it read the
    /// historical message quoted inside a `//` comment (`"leader mode requires HYDRA_CONTROL_URL"`,
    /// `main.rs:216`) as a live string literal. A guard whose first act is to report a comment is a
    /// guard that gets switched off, and the right fix for "it flagged a comment" is a scanner, not
    /// a narrower pattern. Line comments, block comments, char literals and lifetimes are skipped;
    /// escapes are consumed so `\"` inside a literal does not end it early.
    ///
    /// Residual: a RAW literal (`r#"…"#`) is not modelled. `main.rs` has none today (checked
    /// 2026-10-05), and adding one can only make this scan miss something, never invent it.
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
                    // `'x'` / `'\n'` are char literals; a lifetime tick (`'static`) is not, and
                    // treating it as one would swallow the rest of the file.
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
    /// The retirement diagnostic exists so a deployment that still carries a retired knob is TOLD.
    /// The opposite failure is just as expensive: a REFUSAL that names one sends the operator to set
    /// a variable the same build reports as ignored, so they "fix" nothing and lose the real cause.
    /// Measured 2026-10-05: `main.rs`'s cluster-mode refusal read
    /// `"cluster mode (HYDRA_ROLE=leader|edge) requires HYDRA_REDIS_URL (Redis backbone)"` — the
    /// variable had been retired three commits earlier, and NOTHING read the string (no unit test,
    /// no drill: `integration/test_startup_knobs.py` asserts on the WIRING diagnostic, a different
    /// message). The scan is over `main.rs`'s string LITERALS only: a mention in a comment is
    /// deliberate documentation, and the two live in the same file.
    ///
    /// Falsification: put `HYDRA_ROLE` back into that refusal and this fails; the controls below
    /// fail if `main.rs` cannot be read as source, if the scan parses too few literals to have
    /// looked at the file at all, or if it reports a comment as a literal.
    #[test]
    fn no_operator_facing_message_names_a_retired_variable() {
        let src = include_str!("../main.rs");
        assert!(
            src.contains("fn main("),
            "the scan read {} bytes of main.rs and found no `fn main(` — the include path is wrong, \
             so an empty scan would pass for the wrong reason",
            src.len()
        );

        let literals = string_literals(src);
        assert!(
            literals.len() > 100,
            "only {} literal(s) parsed out of main.rs — the scan is not looking at the file",
            literals.len()
        );
        // The scanner must not report a comment as a literal, or this guard is noise. The exact
        // comment the first version flagged is the control that pins it.
        assert!(
            !literals
                .iter()
                .any(|l| l.contains("leader mode requires HYDRA_CONTROL_URL")),
            "the scanner read the historical message out of a `//` comment in main.rs: {:?}",
            literals
                .iter()
                .find(|l| l.contains("leader mode requires"))
                .unwrap()
        );

        for name in RETIRED_CLUSTER_ENV {
            for lit in &literals {
                assert!(
                    !lit.contains(name),
                    "main.rs has a string literal naming the RETIRED variable {name}: {:?}\n\
                     A refusal or log line that names it tells the operator to set a variable this \
                     build reports as IGNORED (RETIRED_CLUSTER_ENV). Name the live knob instead — \
                     for the member list that is HYDRA_CLUSTER_PEERS.",
                    lit.trim()
                );
            }
        }
    }

    /// A typo'd `HYDRA_ROLE` on a node that HAS cluster wiring must say what it is
    /// dropping. `from_env`'s fallback is documented and stays (a typo must not make
    /// a node unable to proxy), but every cluster-only startup validation in
    /// `main.rs` is gated on `is_cluster()`, so a leader that falls back to `all`
    /// boots "fine" while skipping them, never joining the election, and writing
    /// tenant config to its own local SQLite. Naming the ignored variables is the
    /// difference between a five-second fix and an outage nobody can explain.
    ///
    /// Falsification: return `None` unconditionally and every `Some` assertion fails.
    /// The table is the single owner of "what counts as cluster wiring", so it is asserted
    /// instead of assumed: this is what an operator reads when they configured cluster
    /// settings without turning the node into a cluster node. ADR-0001 replaced the
    /// role-based decision with "is `HYDRA_CLUSTER_PEERS` set".
    ///
    /// The last three entries are variables the plan RETIRES but that the Redis path still
    /// READS. They stay in the live table until the commit that deletes those readers,
    /// because the alternative — listing them as retired while the product honours them —
    /// would make the retirement diagnostic a lie.
    ///
    /// Falsification: drop any name from `CLUSTER_ONLY_ENV` and this fails with the missing one.
    #[test]
    fn the_cluster_only_env_table_is_exactly_the_cluster_topology() {
        assert_eq!(
            CLUSTER_ONLY_ENV,
            [
                "HYDRA_CLUSTER_PEERS",
                "HYDRA_CLUSTER_ID",
                "HYDRA_REDIS_URL",
                "HYDRA_REDIS_MODE",
                "HYDRA_CLUSTER_TOKEN",
                "HYDRA_NODE_ID",
                "HYDRA_ARACHNE_LISTEN",
            ],
            "the cluster-only table is what the fallback diagnostic names — changing it is a \
             user-visible change"
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

    /// The decision table, against the PRODUCTION function, one row per operator mistake.
    ///
    /// ADR-0001: `HYDRA_ROLE` is gone, so "is this a cluster node" is answered by the
    /// presence of the static member list and nothing else. The two SILENT failure modes
    /// this table exists for are different mistakes with different fixes:
    ///   1. cluster wiring set, member list missing — the node is standalone and the
    ///      wiring is dropped;
    ///   2. a RETIRED variable set — the node may be clustered, but that setting does
    ///      nothing now.
    ///
    /// Falsification: make the missing-peers branch `Quiet` and rows 1 fail; drop the
    /// retirement arm and row 2 fails.
    #[test]
    fn the_cluster_decision_covers_every_way_to_be_standalone() {
        let wiring = || {
            vec![
                "HYDRA_REDIS_URL".to_string(),
                "HYDRA_CLUSTER_TOKEN".to_string(),
            ]
        };

        // A member list, whatever else is set: this IS a cluster node.
        let node = cluster_decision(
            Some("a=1.1.1.1:7001,b=1.1.1.1:7002,c=1.1.1.1:7003"),
            &[],
            &[],
        );
        assert_eq!(node.role, NodeRole::Cluster);
        assert_eq!(node.notice, ClusterNotice::Quiet);

        // Nothing cluster-shaped at all: the documented single-node default, silently.
        for peers in [None, Some(""), Some("   ")] {
            let d = cluster_decision(peers, &[], &[]);
            assert_eq!(d.role, NodeRole::All, "peers={peers:?}");
            assert_eq!(d.notice, ClusterNotice::Quiet, "peers={peers:?}");
        }

        // Mistake 1: wiring without a member list.
        for peers in [None, Some(""), Some("  ")] {
            let d = cluster_decision(peers, &wiring(), &[]);
            assert_eq!(d.role, NodeRole::All, "peers={peers:?}");
            assert_eq!(
                d.notice,
                ClusterNotice::WiringWithoutMembers {
                    ignored: "HYDRA_REDIS_URL, HYDRA_CLUSTER_TOKEN".to_string()
                },
                "peers={peers:?}: configured cluster settings must be named, not dropped in silence"
            );
        }

        // Mistake 2: a retired variable. Reported whether or not the node is clustered,
        // because either way the setting does nothing. The retirement table is empty today,
        // so the mechanism is exercised through the explicit-table entry point with the name
        // that is first in line to move in.
        let retired = cluster_decision_with(
            Some("a=1.1.1.1:7001,b=1.1.1.1:7002,c=1.1.1.1:7003"),
            &[],
            &["HYDRA_ROLE".to_string()],
            &["HYDRA_ROLE"],
        );
        assert_eq!(
            retired.notice,
            ClusterNotice::RetiredIgnored {
                ignored: "HYDRA_ROLE".to_string()
            },
            "a retired variable must be named even on a healthy cluster node"
        );
        // ...and it must not change the decision.
        assert_eq!(retired.role, NodeRole::Cluster);
    }

    /// A retired variable must be detectable from the environment table alone, and the
    /// detection must not depend on which of the two mistakes also happened.
    ///
    /// Falsification: return the whole present list instead of the retired subset and
    /// this fails on the healthy-node row.
    #[test]
    fn retired_variables_are_reported_and_live_ones_are_not() {
        let present =
            |names: &[&str]| -> Vec<String> { names.iter().map(|n| n.to_string()).collect() };
        // An explicit table, so this exercises the MECHANISM rather than the current contents of
        // the real one (which is pinned exactly, below). `HYDRA_LEADER_LEASE_MS` doubles as a real
        // example: it moved into `RETIRED_CLUSTER_ENV` when the lease lost its last reader.
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
            retired_present(
                &present(&["HYDRA_CLUSTER_TOKEN", "HYDRA_REDIS_URL"]),
                &table
            )
            .is_empty(),
            "a healthy cluster node must not be told anything is retired"
        );
        assert!(
            retired_present(&[], &table).is_empty(),
            "nothing configured ⇒ nothing to report"
        );
        // The real table is pinned EXACTLY, so a name can only move in or out deliberately —
        // and the environment guard (`scripts/check_documented_env.cjs`) is what proves that
        // nothing reads a retired name: it fails if a documented variable has no read site.
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
                "HYDRA_EDGE",
            ],
            "the retirement table changed: update this test AND confirm (via the env guard) that \
             nothing reads the variable that moved"
        );
    }

    /// The retirement table must never overlap the live table.
    ///
    /// A name in both would be reported to the operator as retired while the product still
    /// honours it — worse than either message alone. This is the honest half of the
    /// retirement guard: the full "nothing reads these" version can only be asserted once
    /// the Redis path is deleted (plan T4.1), and pretending otherwise would be a test that
    /// passes for the wrong reason.
    ///
    /// Falsification: add any `RETIRED_CLUSTER_ENV` name to `CLUSTER_ONLY_ENV` and this
    /// fails naming it.
    #[test]
    fn the_retirement_table_never_shadows_a_live_variable() {
        for name in RETIRED_CLUSTER_ENV {
            assert!(
                !CLUSTER_ONLY_ENV.contains(&name),
                "{name} is listed as BOTH live and retired; the operator would be told it is \
                 ignored while the product still reads it"
            );
        }
        // And the two tables together are what `from_env` reads, so a name that is in
        // neither would be a knob nobody reports on.
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
