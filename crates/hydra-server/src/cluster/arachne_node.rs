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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arachne_kv::client::Handle;
use arachne_kv::{NodeId, Profile};

/// The environment variable carrying the static member list.
pub const PEERS_ENV: &str = "HYDRA_CLUSTER_PEERS";
/// The environment variable carrying this node's identity.
pub const NODE_ID_ENV: &str = "HYDRA_NODE_ID";
/// The environment variable carrying this node's raft listen address.
pub const LISTEN_ENV: &str = "HYDRA_ARACHNE_LISTEN";
/// The environment variable carrying this node's Arachne data directory (the
/// WAL + snapshot location). Optional: when unset, the durable state is placed
/// beside the proxy's own data directory under a fixed name.
pub const DATA_DIR_ENV: &str = "HYDRA_ARACHNE_DATA_DIR";
/// The environment variable naming the cluster. Optional, and used for exactly
/// one thing: refusing to join a data directory that belongs to another
/// cluster (`ClusterConfig::cluster_id`, which Arachne checks during the
/// transport handshake).
pub const CLUSTER_ID_ENV: &str = "HYDRA_CLUSTER_ID";

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

/// The probe key. Deliberately **not** under `hydra/ctl/` or `hydra/cfg/`: those
/// namespaces are the control-plane contract (T2.1 owns them), while this key is
/// a liveness marker whose value is meaningless. It is rewritten by every
/// probing node, so it must never be read as state.
const LEADER_PROBE_KEY: &[u8] = b"hydra/probe/leader";

/// How often the background task probes.
///
/// Fast enough that `/healthz/leader` follows a handover well inside a
/// rollout's patience, slow enough that the cost is one small raft entry per
/// healthy node per interval (a follower's probe is refused locally by raft and
/// never reaches the log).
pub const LEADER_PROBE_INTERVAL: Duration = Duration::from_millis(250);

/// A live Arachne node plus the cached answer to "am I the leader".
///
/// The cache exists for the **synchronous** callers — `/healthz/leader` and the
/// admin write path's decision to answer 503 — which cannot await a probe. It is
/// deliberately conservative: it reflects the last probe, so a node that can no
/// longer commit stops claiming leadership at the next probe rather than at the
/// next election.
///
/// ## The consistency discipline (ADR-0001 D-3 / F-3)
///
/// This cache is **not** what decides whether a write is applied. A write is
/// applied if and only if Arachne accepts it: a node whose cache wrongly says
/// "leader" still gets `NotLeader` from `put` and must fail closed, and a node
/// whose cache wrongly says "not leader" costs at most one retry. So the failure
/// direction is safe, and no caller may treat the cache as authority for a
/// mutation.
// No `Debug`: `arachne_kv::client::Handle` does not implement it, and deriving a
// Debug that hides the handle would be a footgun in logs anyway.
pub struct ArachneControl {
    /// `None` when no node was started (single-node mode, or a test double).
    handle: Option<Handle>,
    node_id: NodeId,
    /// The last probe's verdict.
    is_leader: Arc<AtomicBool>,
    /// How many times this node's leadership verdict changed.
    flips: Arc<AtomicU64>,
}

impl ArachneControl {
    /// A control with no started node: never a leader, never a flip.
    ///
    /// Used by the single-node path, where there is no raft leader to be — the
    /// caller must then keep the documented "no election" shape of
    /// `/healthz/leader` (404) rather than fabricate a 200.
    #[must_use]
    pub fn not_started() -> Self {
        Self {
            handle: None,
            node_id: NodeId::new("unstarted"),
            is_leader: Arc::new(AtomicBool::new(false)),
            flips: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Wrap an already-running node.
    ///
    /// Exists so the leader-probe tests can drive real nodes assembled by
    /// `assemble_cluster`: the `Arachne::start` facade is a process-wide
    /// singleton, so a three-node test cannot use it.
    #[must_use]
    pub fn for_tests(handle: Handle, node_id: NodeId) -> Self {
        Self {
            handle: Some(handle),
            node_id,
            is_leader: Arc::new(AtomicBool::new(false)),
            flips: Arc::new(AtomicU64::new(0)),
        }
    }

    /// This node's identity.
    #[must_use]
    pub fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    /// The cached verdict, for synchronous callers.
    #[must_use]
    pub fn is_leader(&self) -> bool {
        self.is_leader.load(Ordering::Acquire)
    }

    /// How many leadership flips have been observed.
    #[must_use]
    pub fn flips(&self) -> u64 {
        self.flips.load(Ordering::Acquire)
    }
    /// The node's handle, when one was started. Crate-internal: the assembly
    /// step needs it, and nothing else should reach past the cache.
    pub(crate) fn handle_ref(&self) -> Option<&Handle> {
        self.handle.as_ref()
    }

    /// Whether this node can currently commit a write — the only question whose
    /// answer is authoritative in both directions.
    ///
    /// `without_redirect` is the point: a redirecting handle would follow the
    /// leader hint and write *somewhere else*, so the answer would become "the
    /// cluster has a leader" instead of "this node is the leader".
    pub async fn probe_once(&self) -> bool {
        let Some(handle) = self.handle.as_ref() else {
            return false;
        };
        handle
            .without_redirect()
            .put(LEADER_PROBE_KEY, self.node_id.as_str().as_bytes())
            .await
            .is_ok()
    }

    /// Run one probe and fold it into the cached verdict, returning whether the
    /// verdict changed.
    pub async fn observe_once(&mut self) -> bool {
        let observed = self.probe_once().await;
        self.record_and_log(observed)
    }

    /// Fold one already-taken probe result into the cache and log a change.
    ///
    /// Split from the await so the fold — which is the part that can be wrong —
    /// is reachable without a live cluster.
    fn record_and_log(&self, observed: bool) -> bool {
        let previous = self.is_leader.swap(observed, Ordering::AcqRel);
        if observed == previous {
            return false;
        }
        if observed {
            tracing::info!(
                node_id = %self.node_id,
                "this node now accepts writes: it is the raft leader"
            );
        } else {
            tracing::warn!(
                node_id = %self.node_id,
                "this node no longer accepts writes: giving up leadership"
            );
        }
        self.flips.fetch_add(1, Ordering::AcqRel);
        true
    }

    /// Keep probing in the background until the process ends.
    ///
    /// `/healthz/leader` must be answerable without awaiting anything, so this
    /// task owns the only place that awaits the probe.
    pub fn spawn_leader_watch(&self) -> tokio::task::JoinHandle<()> {
        let handle = self.handle.clone();
        let node_id = self.node_id.clone();
        let is_leader = Arc::clone(&self.is_leader);
        let flips = Arc::clone(&self.flips);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(LEADER_PROBE_INTERVAL);
            loop {
                ticker.tick().await;
                let observed = match handle.as_ref() {
                    Some(h) => h
                        .without_redirect()
                        .put(LEADER_PROBE_KEY, node_id.as_str().as_bytes())
                        .await
                        .is_ok(),
                    None => false,
                };
                let previous = is_leader.swap(observed, Ordering::AcqRel);
                if observed != previous {
                    if observed {
                        tracing::info!(node_id = %node_id, "this node is now the raft leader");
                    } else {
                        tracing::warn!(node_id = %node_id, "this node is no longer the raft leader");
                    }
                    flips.fetch_add(1, Ordering::AcqRel);
                }
            }
        })
    }
}

/// Build the Arachne config for `peers`.
///
/// A pure function of the parsed table plus the two derived values, so the
/// mapping from the peer table to raft's view of the cluster is asserted
/// without starting a node. The profile is the LAN preset: these are
/// control-plane nodes inside one deployment, not a geo-distributed quorum.
#[must_use]
pub fn arachne_config(
    peers: &ClusterPeers,
    cluster_id: String,
    data_dir: PathBuf,
) -> arachne_kv::server::ClusterConfig {
    let mut cfg = arachne_kv::server::ClusterConfig::member(
        cluster_id,
        peers.node_id().clone(),
        peers.listen(),
        data_dir,
        peers.order().to_vec(),
        peers.addresses().clone(),
    );
    cfg.profile = Profile::Lan;
    cfg
}

/// Where this node's Arachne WAL and snapshots live.
///
/// Defaults to a directory **beside the SQLite file**. The WAL is durable state
/// — it holds the raft log this node needs to rejoin without a snapshot — so a
/// `/tmp` default would lose it on reboot and silently turn every restart into a
/// full catch-up.
#[must_use]
pub fn arachne_data_dir(sqlite_path: &str, override_dir: Option<&str>) -> PathBuf {
    if let Some(dir) = override_dir.map(str::trim).filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    let sqlite = Path::new(sqlite_path);
    let dir = sqlite
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("arachne"), |p| p.join("arachne"));
    dir
}

/// The cluster name given to Arachne.
///
/// Arachne refuses a data directory whose recorded cluster id differs, which is
/// what stops a node from silently joining the wrong raft group after a
/// directory is reused or a volume is mis-mounted.
///
/// The default is derived from the **data directory path**, not from a constant:
/// two clusters on one host must not share an id merely because neither operator
/// set the variable. It is a hash rather than the path itself so the id is a
/// stable, short, filesystem-agnostic token; an explicit `HYDRA_CLUSTER_ID`
/// always wins.
#[must_use]
pub fn cluster_id_from(explicit: Option<&str>, data_dir: &str) -> String {
    if let Some(id) = explicit.map(str::trim).filter(|v| !v.is_empty()) {
        return id.to_string();
    }
    // Standard-library hashing only: this default is a *local* sanity token, not
    // a security boundary, and adding a digest crate to the dependency tree for
    // it would be a poor trade. Determinism across builds is not required either
    // — a cluster that wants a stable, human-chosen id sets HYDRA_CLUSTER_ID.
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    data_dir.hash(&mut hasher);
    format!("hydra-{:016x}", hasher.finish())
}

/// The state a node reports about its own cluster membership, used to verify on
/// every start that this process and its data directory agree about which
/// cluster they belong to.
const HANDSHAKE_CLUSTER_KEY: &[u8] = b"hydra/ctl/cluster_id";

/// Refuse to start when the data directory belongs to another cluster.
///
/// **Read first, write second**, and the order is load-bearing (it was wrong on
/// the first implementation, which turned "this node is not the leader yet" into
/// "this directory belongs to another cluster" — measured). The two cases are:
///
/// * a directory that **already records** an identity: every member must agree
///   with it, whatever its own role is — a plain local read answers this;
/// * a directory with **no** identity yet: it is adopted by writing the key,
///   which only the leader can do. A node that cannot write it is not an error —
///   some other member will, and until then there is nothing to disagree with.
///
/// So a non-leader only ever fails this check when the directory's recorded
/// identity differs from its own configuration, which is exactly the case worth
/// refusing.
///
/// **The caller must retry** this until it returns `Ready` — a node that starts
/// before the cluster has elected anyone cannot have adopted its directory yet,
/// and the first implementation returned success in that window, which left the
/// directory unclaimed and made a later mismatched start look like a fresh one
/// (measured). The assembly step therefore polls this function; see
/// [`PREFLIGHT_DEADLINE`].
pub async fn preflight_cluster_id(
    control: &ArachneControl,
    cluster_id: &str,
) -> Result<(), String> {
    let Some(handle) = control.handle_ref() else {
        return Ok(());
    };
    let expected = cluster_id.as_bytes();

    // 1. What does the directory say, if anything?
    match handle.get_stale(HANDSHAKE_CLUSTER_KEY).await {
        Ok(Some(found)) if found.as_slice() == expected => return Ok(()),
        Ok(Some(found)) => {
            return Err(format!(
                "this node's Arachne data directory belongs to a different cluster: it recorded \
                 {:?} but this process is configured for {:?} (HYDRA_CLUSTER_ID / \
                 HYDRA_ARACHNE_DATA_DIR). Refusing to join.",
                String::from_utf8_lossy(&found),
                cluster_id
            ))
        }
        // Nothing recorded yet (or the read could not answer): try to adopt it.
        Ok(None) => {}
        Err(e) => {
            tracing::debug!("cluster identity read did not answer yet: {e:?}");
        }
    }

    // 2. Adopt the directory. Only the leader can, and until it does the answer
    //    is "ask again" rather than a verdict either way.
    match handle
        .without_redirect()
        .put(HANDSHAKE_CLUSTER_KEY, expected)
        .await
    {
        Ok(()) => Ok(()),
        Err(arachne_kv::client::ArachneError::NotLeader { .. })
        | Err(arachne_kv::client::ArachneError::QuorumUnavailable)
        | Err(arachne_kv::client::ArachneError::Timeout)
        | Err(arachne_kv::client::ArachneError::Busy) => Err(format!(
            "{} (no leader has adopted this data directory yet)",
            PENDING_ADOPTION
        )),
        Err(e) => Err(format!(
            "cannot record this node's cluster identity in its Arachne data directory: {e:?}"
        )),
    }
}

/// The marker a caller retries on: the data directory has no recorded cluster
/// identity yet and THIS node could not claim it. Distinguished from a real
/// mismatch by its text, because the two need opposite responses — retry versus
/// refuse to start.
pub const PENDING_ADOPTION: &str = "cluster identity not adopted yet";

/// How long the assembly step keeps asking before giving up. Longer than one
/// election timeout on the LAN profile (1 s) with room for a cold start, short
/// enough that a genuinely stuck cluster still fails a rollout quickly.
pub const PREFLIGHT_DEADLINE: Duration = Duration::from_secs(10);

/// Poll [`preflight_cluster_id`] until the directory is adopted or the deadline
/// passes.
///
/// Returns `Ok(())` once this node and its data directory agree, and `Err` for a
/// real mismatch (`another cluster`) or for a directory nobody claimed in time.
pub async fn await_cluster_preflight(
    control: &ArachneControl,
    cluster_id: &str,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + PREFLIGHT_DEADLINE;
    loop {
        let pending = match preflight_cluster_id(control, cluster_id).await {
            Ok(()) => return Ok(()),
            Err(e) if e.contains(PENDING_ADOPTION) => e,
            Err(e) => return Err(e),
        };
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "no member adopted this node's Arachne data directory within {:?}: {pending}",
                PREFLIGHT_DEADLINE
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
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

    /// The Arachne config this module hands to `Arachne::start` must carry the
    /// parsed table verbatim: the member order (raft ids), every address, this
    /// node's identity and its listen address.
    ///
    /// Falsification: build `initial_cluster` from a `HashMap` and the order
    /// assertion fails; pass the peer map without the self entry and the
    /// address-count assertion fails.
    #[test]
    fn the_arachne_config_carries_the_parsed_table_verbatim() {
        let peers = cluster_peers(THREE, "b", "10.0.0.2:7001").expect("valid");
        let cfg = arachne_config(
            &peers,
            "hydra-cluster".to_string(),
            PathBuf::from("/var/lib/hydra/arachne"),
        );

        assert_eq!(cfg.cluster_id, "hydra-cluster");
        assert_eq!(cfg.node_id, NodeId::new("b"));
        assert_eq!(cfg.listen, addr("10.0.0.2:7001"));
        assert_eq!(cfg.data_dir, PathBuf::from("/var/lib/hydra/arachne"));
        assert_eq!(
            cfg.initial_cluster,
            vec![NodeId::new("a"), NodeId::new("b"), NodeId::new("c")],
            "position is identity: Arachne derives each member's raft id from this order"
        );
        assert_eq!(
            cfg.addresses.len(),
            3,
            "every member needs a dialable address"
        );
        assert_eq!(cfg.addresses[&NodeId::new("a")], addr("10.0.0.1:7001"));
        assert_eq!(cfg.addresses[&NodeId::new("b")], addr("10.0.0.2:7001"));
    }

    /// This node's data directory defaults next to the proxy's own data, and an
    /// explicit override wins. Both are pure functions of the two inputs, so
    /// they are asserted without touching the environment.
    ///
    /// Falsification: ignore the override and the second assertion fails.
    #[test]
    fn the_data_dir_is_derived_or_overridden() {
        let derived = arachne_data_dir("/var/lib/hydra/hydra.db", None);
        assert_eq!(
            derived,
            PathBuf::from("/var/lib/hydra/arachne"),
            "the default must live beside the SQLite file, not in /tmp: the WAL is durable state"
        );

        let overridden = arachne_data_dir("/var/lib/hydra/hydra.db", Some("/mnt/fast/arachne"));
        assert_eq!(overridden, PathBuf::from("/mnt/fast/arachne"));

        let in_memory = arachne_data_dir("hydra.db", None);
        assert_eq!(
            in_memory,
            PathBuf::from("arachne"),
            "a relative sqlite path must yield a relative arachne dir beside it"
        );
    }

    /// The cluster id defaults per data directory rather than per host: two
    /// clusters on one box must not silently share a raft group just because
    /// neither set the variable.
    ///
    /// Falsification: return a constant default and the two directories produce
    /// the same id, which makes the assertion fail.
    #[test]
    fn the_cluster_id_default_is_per_data_dir() {
        let a = cluster_id_from(None, "/var/lib/hydra/arachne-a");
        let b = cluster_id_from(None, "/var/lib/hydra/arachne-b");
        assert_ne!(
            a, b,
            "the default cluster id must differ per data directory, or two clusters on one host \
             would form one raft group"
        );
        assert_eq!(
            cluster_id_from(Some("named-cluster"), "/var/lib/hydra/arachne-a"),
            "named-cluster",
            "an explicit HYDRA_CLUSTER_ID wins"
        );
        assert!(
            a.starts_with("hydra-"),
            "the default must be recognisable as Hydra's (got {a:?})"
        );
    }
}
