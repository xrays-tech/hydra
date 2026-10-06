//! # Arachne control-plane bootstrap (ADR-0001, plan Phase 1 T1.2)
//!
//! One owner for the three facts a clustered Hydra node needs before it can
//! start a raft node, and for nothing else:
//!
//! 1. **identity** — this node's `HYDRA_NODE_ID`, which must be a member of the
//!    peer list, because `HYDRA_NODE_ID` is simultaneously a raft node id and a
//!    data-plane identity;
//! 2. **the member list** — `HYDRA_CLUSTER_PEERS`, a static, ordered
//!    `id=host:port` list covering every cluster member **including this node**;
//! 3. **the listen address** — where this node's raft transport binds, which the
//!    plan pins to the **same port as the admin listener** so that the address
//!    Arachne reports in `leader_hint()` is directly usable as an admin endpoint.
//!
//! ## Why this is fail-closed (and why there is no fallback)
//!
//! The plan's T1.2 requires every malformed cluster configuration to be an
//! `Err`. The historical fallback chain it replaces was
//! `HYDRA_NODE_ID` → `HOSTNAME` → random, and `dev-docs/cluster.md` §3.1 records
//! what the last resort cost: two processes that share a node id share a lease
//! identity and therefore **both believe they are the leader**, each accepting
//! management writes and publishing its own snapshot. Under raft the same
//! mistake is worse, not better: `NodeId` is also the key of the
//! raft-id→node-id map, so a duplicate identity silently corrupts leader
//! resolution instead of merely racing for a lease.
//!
//! ## What is deliberately NOT here
//!
//! Starting a node, watching leadership and publishing configuration. Those
//! belong to the phases that own them (T1.3, T2.x, T3.x); this module stays a
//! pure parser plus its errors so the whole table is unit-testable without a
//! process, a socket or a clock.

use std::collections::HashMap;
use std::net::SocketAddr;

use arachne_kv::NodeId;

/// The environment variable carrying the static member list.
pub const PEERS_ENV: &str = "HYDRA_CLUSTER_PEERS";
/// The environment variable carrying this node's identity.
pub const NODE_ID_ENV: &str = "HYDRA_NODE_ID";
/// The environment variable carrying this node's raft listen address.
pub const LISTEN_ENV: &str = "HYDRA_ARACHNE_LISTEN";

/// Every way a cluster configuration can be wrong.
///
/// Each variant carries the offending input so the startup message names it —
/// an operator who mistyped one member must not have to guess which one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClusterPeersError {
    /// The variable is unset, blank, or has no `id=addr` entries.
    EmptyPeers { var: &'static str },
    /// An entry is not `id=host:port`.
    MalformedEntry { entry: String },
    /// Two entries declare the same member id.
    DuplicateMember { id: String },
    /// An entry's address is not a `host:port` socket address.
    BadAddress {
        id: String,
        addr: String,
        reason: String,
    },
    /// A member id is empty.
    EmptyMemberId { entry: String },
    /// This node's own id is unset/blank.
    MissingNodeId { var: &'static str },
    /// This node's listen address is unset/blank, or is not `host:port`.
    BadListen { var: &'static str, value: String },
    /// This node's id does not appear in the peer list.
    SelfNotAMember { node_id: String },
    /// This node's listen address disagrees with its entry in the peer list.
    ListenMismatch {
        node_id: String,
        listed: String,
        listen: String,
    },
    /// Fewer than three members: raft needs a majority, so two members tolerate
    /// no failure at all (ADR-0001 D-5).
    TooFewMembers { members: usize, minimum: usize },
}

impl std::fmt::Display for ClusterPeersError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyPeers { var } => write!(
                f,
                "{var} is unset or has no `id=host:port` entries; a cluster node needs the full \
                 member list (this node included)"
            ),
            Self::MalformedEntry { entry } => {
                write!(f, "{PEERS_ENV} entry {entry:?} is not `id=host:port`")
            }
            Self::DuplicateMember { id } => {
                write!(f, "{PEERS_ENV} names member {id:?} more than once")
            }
            Self::BadAddress { id, addr, reason } => write!(
                f,
                "{PEERS_ENV} entry {id:?} has an unusable address {addr:?}: {reason}"
            ),
            Self::EmptyMemberId { entry } => {
                write!(f, "{PEERS_ENV} entry {entry:?} has an empty member id")
            }
            Self::MissingNodeId { var } => write!(
                f,
                "{var} is unset or blank; a cluster node must state its own identity explicitly \
                 (there is no HOSTNAME or random fallback: a duplicate id makes two nodes share \
                 one raft identity)"
            ),
            Self::BadListen { var, value } => {
                write!(f, "{var}={value:?} is not a `host:port` socket address")
            }
            Self::SelfNotAMember { node_id } => write!(
                f,
                "this node's id {node_id:?} does not appear in {PEERS_ENV}; the member list must \
                 cover every member, this node included"
            ),
            Self::ListenMismatch {
                node_id,
                listed,
                listen,
            } => write!(
                f,
                "this node's listen address {listen:?} disagrees with its {PEERS_ENV} entry \
                 {listed:?} (member {node_id:?}); peers would dial one address while this node \
                 binds another"
            ),
            Self::TooFewMembers { members, minimum } => write!(
                f,
                "{PEERS_ENV} lists {members} member(s); at least {minimum} are required because \
                 raft needs a majority and two members tolerate no failure"
            ),
        }
    }
}

impl std::error::Error for ClusterPeersError {}

/// The minimum member count. Three is a hard requirement, not a default: with
/// two members the loss of either one loses the majority, so a "highly
/// available" pair is strictly less available than a single node.
pub const MINIMUM_MEMBERS: usize = 3;

/// A validated cluster member list plus this node's place in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClusterPeers {
    /// Members in declaration order. The order is **load-bearing**: Arachne
    /// derives each member's numeric raft id from its position in this list
    /// (`assemble_cluster` uses `position + 1`), so reordering the variable
    /// changes cluster identity.
    order: Vec<NodeId>,
    /// Member id → the address that member listens on.
    addresses: HashMap<NodeId, SocketAddr>,
    /// This node's identity.
    node_id: NodeId,
    /// This node's own listen address (equal to its entry in `addresses`).
    listen: SocketAddr,
}

impl ClusterPeers {
    /// Members in declaration order (the raft id order).
    #[must_use]
    pub fn order(&self) -> &[NodeId] {
        &self.order
    }

    /// Member id → listen address.
    #[must_use]
    pub fn addresses(&self) -> &HashMap<NodeId, SocketAddr> {
        &self.addresses
    }

    /// This node's identity.
    #[must_use]
    pub fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    /// This node's listen address.
    #[must_use]
    pub fn listen(&self) -> SocketAddr {
        self.listen
    }

    /// This node's numeric raft id: its **1-based position** in the declaration
    /// order.
    ///
    /// Arachne derives this mapping itself (`assemble_cluster` uses
    /// `position + 1`) and the two identities are easy to confuse: the
    /// membership APIs (`transfer_leader`, `add_learner`, `promote_learner`)
    /// take the numeric id, while `leader_hint()` and `membership()` speak
    /// `NodeId` strings. Exposing the mapping here gives callers one place that
    /// agrees with Arachne, and gives this module a way to pin it in a test.
    ///
    /// `None` means this node's id is absent from the list — which
    /// [`cluster_peers`] already rejects — so a caller treats `None` as a bug
    /// rather than as an operator error.
    #[must_use]
    pub fn raft_id(&self) -> Option<u64> {
        self.order
            .iter()
            .position(|n| n == &self.node_id)
            .map(|i| (i + 1) as u64)
    }
}

/// Parse and validate a cluster configuration.
///
/// `spec` is the raw `HYDRA_CLUSTER_PEERS` value, `node_id` the raw
/// `HYDRA_NODE_ID` value and `listen` the raw `HYDRA_ARACHNE_LISTEN` value —
/// passed in rather than read here so the entire failure table is unit-testable
/// without touching the process environment (the same split the role parser uses
/// for `RoleNotice`).
///
/// The accepted shape is `id=host:port` entries separated by commas, with any
/// surrounding whitespace per entry ignored (so a multi-line YAML block or a
/// template's trailing comma does not fail). The result is validated as a whole;
/// the first problem found is returned, and the caller must refuse to start.
pub fn cluster_peers(
    spec: &str,
    node_id: &str,
    listen: &str,
) -> Result<ClusterPeers, ClusterPeersError> {
    // 1. Members. An entry is `id=host:port`; blank entries (a trailing comma, a
    //    YAML block's last line) are skipped rather than rejected.
    let mut order: Vec<NodeId> = Vec::new();
    let mut addresses: HashMap<NodeId, SocketAddr> = HashMap::new();
    for raw_entry in spec.split(',') {
        let entry = raw_entry.trim();
        if entry.is_empty() {
            continue;
        }
        // Split on the LAST `=` so an id containing `=` cannot silently swallow
        // part of the address.
        let Some((id, addr)) = entry.rsplit_once('=') else {
            return Err(ClusterPeersError::MalformedEntry {
                entry: entry.to_string(),
            });
        };
        let id = id.trim();
        if id.is_empty() {
            return Err(ClusterPeersError::EmptyMemberId {
                entry: entry.to_string(),
            });
        }
        let addr = addr.trim();
        // The closure parameter needs its type spelled out: `map_err` alone does
        // not pin the error type the parser produces.
        let parsed: SocketAddr =
            addr.parse().map_err(
                |e: std::net::AddrParseError| ClusterPeersError::BadAddress {
                    id: id.to_string(),
                    addr: addr.to_string(),
                    reason: e.to_string(),
                },
            )?;
        let key = NodeId::new(id.to_string());
        if addresses.contains_key(&key) {
            return Err(ClusterPeersError::DuplicateMember { id: id.to_string() });
        }
        order.push(key.clone());
        addresses.insert(key, parsed);
    }

    if order.is_empty() {
        return Err(ClusterPeersError::EmptyPeers { var: PEERS_ENV });
    }

    // 2. This node's identity. Deliberately before the membership checks: an
    //    unset id is a different operator mistake from an id that is not in the
    //    list, and naming the wrong one sends them to the wrong variable.
    let node_id = node_id.trim();
    if node_id.is_empty() {
        return Err(ClusterPeersError::MissingNodeId { var: NODE_ID_ENV });
    }
    let node_key = NodeId::new(node_id.to_string());

    // 3. This node's listen address. It must equal the address its peers will
    //    dial, or half the cluster talks to a socket nobody is bound to.
    let listen = listen.trim();
    if listen.is_empty() {
        return Err(ClusterPeersError::BadListen {
            var: LISTEN_ENV,
            value: String::new(),
        });
    }
    let listen_addr: SocketAddr = listen.parse().map_err(|_| ClusterPeersError::BadListen {
        var: LISTEN_ENV,
        value: listen.to_string(),
    })?;

    // 4. Self must be a member, at the address it binds.
    let Some(listed) = addresses.get(&node_key) else {
        return Err(ClusterPeersError::SelfNotAMember {
            node_id: node_id.to_string(),
        });
    };
    if *listed != listen_addr {
        return Err(ClusterPeersError::ListenMismatch {
            node_id: node_id.to_string(),
            listed: listed.to_string(),
            listen: listen_addr.to_string(),
        });
    }

    // 5. A majority must exist. Checked last so a small-but-otherwise-valid list
    //    reports the member count rather than a parsing complaint.
    if order.len() < MINIMUM_MEMBERS {
        return Err(ClusterPeersError::TooFewMembers {
            members: order.len(),
            minimum: MINIMUM_MEMBERS,
        });
    }

    Ok(ClusterPeers {
        order,
        addresses,
        node_id: node_key,
        listen: listen_addr,
    })
}

/// Whether this process was asked to run as a cluster node.
///
/// The presence of [`PEERS_ENV`] is the whole test (ADR-0001: the cluster
/// decision replaces `HYDRA_ROLE`, which is retired along with the `edge`
/// role). An unset or blank variable means single-node mode, where no Arachne
/// node is started and no cluster-only variable is required.
#[must_use]
pub fn cluster_enabled() -> bool {
    cluster_enabled_from(std::env::var(PEERS_ENV).ok().as_deref())
}

/// The decision itself, split from the env read so it is testable without
/// touching the process environment.
#[must_use]
fn cluster_enabled_from(raw: Option<&str>) -> bool {
    matches!(raw, Some(v) if !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A three-member LAN list, the minimum the parser accepts.
    const THREE: &str = "a=10.0.0.1:7001,b=10.0.0.2:7001,c=10.0.0.3:7001";

    fn addr(s: &str) -> SocketAddr {
        s.parse().expect("test fixture address")
    }

    /// The happy path, and the two facts downstream code depends on: the peer
    /// ORDER is preserved (Arachne derives raft ids from position), and the
    /// address map covers every member.
    ///
    /// Falsification: return the members in a `HashMap` iteration order and the
    /// `order()` assertion fails intermittently-to-always; drop the self entry
    /// from `addresses` and the second assertion fails.
    #[test]
    fn parses_a_three_member_list_and_keeps_declaration_order() {
        let peers = cluster_peers(THREE, "b", "10.0.0.2:7001").expect("valid three-member list");

        assert_eq!(
            peers.order(),
            &[NodeId::new("a"), NodeId::new("b"), NodeId::new("c")],
            "member order must be the declaration order: Arachne derives each member's raft id \
             from its position in this list"
        );
        assert_eq!(
            peers.addresses().len(),
            3,
            "every member must have an address"
        );
        assert_eq!(peers.addresses()[&NodeId::new("a")], addr("10.0.0.1:7001"));
        assert_eq!(peers.addresses()[&NodeId::new("c")], addr("10.0.0.3:7001"));
        assert_eq!(peers.node_id(), &NodeId::new("b"));
        assert_eq!(peers.listen(), addr("10.0.0.2:7001"));
    }

    /// Whitespace around entries (and a trailing comma from a template) is
    /// tolerated, because failing on it would make the variable impossible to
    /// write as a YAML block scalar.
    ///
    /// Falsification: remove the `trim()` and this test fails on the first entry.
    #[test]
    fn tolerates_surrounding_whitespace_and_a_trailing_comma() {
        let spec = " a=10.0.0.1:7001 ,\n b=10.0.0.2:7001 ,\n c=10.0.0.3:7001 ,\n";
        let peers = cluster_peers(spec, "a", "10.0.0.1:7001").expect("whitespace is not an error");
        assert_eq!(peers.order().len(), 3);
        assert_eq!(peers.listen(), addr("10.0.0.1:7001"));
    }

    /// Every malformed configuration must be an `Err`. The table is the whole
    /// point of this task: each row is an operator mistake that, if accepted,
    /// starts a node that cannot participate correctly.
    #[test]
    fn rejects_every_malformed_configuration() {
        let cases: &[(&str, &str, &str, ClusterPeersError)] = &[
            (
                "",
                "a",
                "10.0.0.1:7001",
                ClusterPeersError::EmptyPeers { var: PEERS_ENV },
            ),
            (
                "   ",
                "a",
                "10.0.0.1:7001",
                ClusterPeersError::EmptyPeers { var: PEERS_ENV },
            ),
            // A list of nothing but separators declares no member at all — it is
            // the same operator mistake as an empty variable, and it is caught by
            // the entry count rather than by a per-entry rule. (Blank *entries*
            // are skipped, which is the same leniency that makes a trailing comma
            // acceptable; the two cannot both be errors.)
            (
                " , , ",
                "a",
                "10.0.0.1:7001",
                ClusterPeersError::EmptyPeers { var: PEERS_ENV },
            ),
            (
                "a:10.0.0.1:7001,b=10.0.0.2:7001,c=10.0.0.3:7001",
                "a",
                "10.0.0.1:7001",
                ClusterPeersError::MalformedEntry {
                    entry: "a:10.0.0.1:7001".to_string(),
                },
            ),
            (
                "=10.0.0.1:7001,b=10.0.0.2:7001,c=10.0.0.3:7001",
                "b",
                "10.0.0.2:7001",
                ClusterPeersError::EmptyMemberId {
                    entry: "=10.0.0.1:7001".to_string(),
                },
            ),
            (
                "a=10.0.0.1:7001,a=10.0.0.9:7001,c=10.0.0.3:7001",
                "a",
                "10.0.0.1:7001",
                ClusterPeersError::DuplicateMember {
                    id: "a".to_string(),
                },
            ),
            (
                "a=nonsense,b=10.0.0.2:7001,c=10.0.0.3:7001",
                "a",
                "10.0.0.1:7001",
                ClusterPeersError::BadAddress {
                    id: "a".to_string(),
                    addr: "nonsense".to_string(),
                    reason: String::new(),
                },
            ),
            (
                "a=10.0.0.1:7001,b=10.0.0.2:7001,c=10.0.0.3:7001",
                "",
                "10.0.0.2:7001",
                ClusterPeersError::MissingNodeId { var: NODE_ID_ENV },
            ),
            (
                "a=10.0.0.1:7001,b=10.0.0.2:7001,c=10.0.0.3:7001",
                "b",
                "",
                ClusterPeersError::BadListen {
                    var: LISTEN_ENV,
                    value: String::new(),
                },
            ),
            (
                "a=10.0.0.1:7001,b=10.0.0.2:7001,c=10.0.0.3:7001",
                "b",
                "not-an-address",
                ClusterPeersError::BadListen {
                    var: LISTEN_ENV,
                    value: "not-an-address".to_string(),
                },
            ),
            (
                "a=10.0.0.1:7001,b=10.0.0.2:7001,c=10.0.0.3:7001",
                "zzz",
                "10.0.0.9:7001",
                ClusterPeersError::SelfNotAMember {
                    node_id: "zzz".to_string(),
                },
            ),
            (
                "a=10.0.0.1:7001,b=10.0.0.2:7001,c=10.0.0.3:7001",
                "b",
                "10.0.0.9:7001",
                ClusterPeersError::ListenMismatch {
                    node_id: "b".to_string(),
                    listed: "10.0.0.2:7001".to_string(),
                    listen: "10.0.0.9:7001".to_string(),
                },
            ),
            (
                "a=10.0.0.1:7001,b=10.0.0.2:7001",
                "a",
                "10.0.0.1:7001",
                ClusterPeersError::TooFewMembers {
                    members: 2,
                    minimum: MINIMUM_MEMBERS,
                },
            ),
            (
                "a=10.0.0.1:7001",
                "a",
                "10.0.0.1:7001",
                ClusterPeersError::TooFewMembers {
                    members: 1,
                    minimum: MINIMUM_MEMBERS,
                },
            ),
        ];

        for (spec, node_id, listen, expected) in cases {
            let got = cluster_peers(spec, node_id, listen).expect_err(&format!(
                "must reject spec={spec:?} node_id={node_id:?} listen={listen:?}"
            ));
            // The `BadAddress` variant carries the parser's own reason text, which
            // is not part of the contract; compare the identifying fields only.
            let matches = match (expected, &got) {
                (
                    ClusterPeersError::BadAddress {
                        id: ei, addr: ea, ..
                    },
                    ClusterPeersError::BadAddress {
                        id: gi, addr: ga, ..
                    },
                ) => ei == gi && ea == ga,
                (e, g) => e == g,
            };
            assert!(
                matches,
                "spec={spec:?} node_id={node_id:?} listen={listen:?}: expected {expected:?}, got {got:?}"
            );
        }
    }

    /// A cluster node must never invent its own identity. The historical chain
    /// (`HYDRA_NODE_ID` → `HOSTNAME` → random) is exactly what made two
    /// processes share a lease identity and both believe they were leader
    /// (`dev-docs/cluster.md` §3.1); under raft a duplicate id is worse, because
    /// it is also the raft-id→node-id map's value.
    ///
    /// Falsification: add the HOSTNAME/random fallback back and both assertions
    /// below fail.
    #[test]
    fn never_falls_back_to_hostname_or_random_for_identity() {
        let blank = cluster_peers(THREE, "   ", "10.0.0.2:7001");
        assert!(
            matches!(blank, Err(ClusterPeersError::MissingNodeId { .. })),
            "a blank id must be rejected, not defaulted: got {blank:?}"
        );

        let unset = cluster_peers(THREE, "", "10.0.0.2:7001");
        assert!(
            matches!(unset, Err(ClusterPeersError::MissingNodeId { .. })),
            "an unset id must be rejected, not defaulted: got {unset:?}"
        );
    }

    /// The numeric raft id is the 1-based declaration position, so the two
    /// identities (the `NodeId` string Arachne reports and the `RaftId` integer
    /// its membership APIs take) cannot drift apart silently.
    ///
    /// Falsification: return the index instead of `index + 1` and every
    /// assertion below fails by one.
    #[test]
    fn raft_id_is_the_one_based_declaration_position() {
        let first = cluster_peers(THREE, "a", "10.0.0.1:7001").expect("valid");
        assert_eq!(
            first.raft_id(),
            Some(1),
            "the first declared member is raft id 1"
        );

        let third = cluster_peers(THREE, "c", "10.0.0.3:7001").expect("valid");
        assert_eq!(
            third.raft_id(),
            Some(3),
            "the third declared member is raft id 3"
        );

        // Order is what decides the id, not the name.
        let reordered = cluster_peers(
            "c=10.0.0.3:7001,b=10.0.0.2:7001,a=10.0.0.1:7001",
            "c",
            "10.0.0.3:7001",
        )
        .expect("valid");
        assert_eq!(
            reordered.raft_id(),
            Some(1),
            "reordering the list changes the raft ids, which is why the variable's order is \
             documented as immutable"
        );
    }
}
