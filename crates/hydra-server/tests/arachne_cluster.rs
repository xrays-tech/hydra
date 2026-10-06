#![cfg(feature = "arachne")]
//! T1.2 — a peer table parsed from `HYDRA_CLUSTER_PEERS` really can start a
//! raft cluster (ADR-0001, plan Phase 1).
//!
//! The unit tests in `cluster::arachne_node` pin the grammar. This file pins the
//! thing the grammar exists for: the addresses and the **declaration order** it
//! produces are exactly what Arachne needs to elect a leader and to agree on
//! who may write.
//!
//! Three nodes run **in one process** on loopback ports. That is deliberate and
//! it is not a mock: `assemble_cluster` is Arachne's own multi-node assembly
//! (the tonic transport, the real WAL, the real actor thread), and it is the
//! only way to host several nodes in one test binary — the `Arachne::start`
//! facade is a process-wide singleton. A probe measured that this path behaves
//! like the production one for everything asserted here (leader election,
//! follower refusal, cross-node reads); what it does *not* exercise is anything
//! process-local, so nothing here depends on that.
//!
//! Falsification for the whole file: reverse `peers.order()` and every node's
//! raft id changes; the cluster either fails to elect or two nodes claim the
//! same id, and the "exactly one leader" assertion below fails.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use arachne_kv::client::ArachneError;
use arachne_kv::server::{assemble_cluster, AssembledClusterNode, ClusterConfig};
use arachne_kv::{NodeId, Profile};
use hydra_server::cluster::arachne_node::{cluster_peers, ClusterPeers};

/// Three loopback addresses. Fixed rather than ephemeral because a raft member
/// must be dialable at the address its peers were told about — an OS-assigned
/// port would be invisible to the other two nodes.
const PORTS: [u16; 3] = [17601, 17602, 17603];

fn spec() -> String {
    "n1=127.0.0.1:17601,n2=127.0.0.1:17602,n3=127.0.0.1:17603".to_string()
}

fn data_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hydra-arachne-cluster-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create data dir");
    dir
}

/// Build the Arachne config for member `i` out of the *parsed* peer table, so
/// the test fails if the parser's order or addresses are wrong.
fn member_config(peers: &ClusterPeers, i: usize, tag: &str) -> ClusterConfig {
    let node_id = NodeId::new(format!("n{}", i + 1));
    let addresses: HashMap<NodeId, SocketAddr> = peers
        .addresses()
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    let mut cfg = ClusterConfig::member(
        "hydra-arachne-test".to_string(),
        node_id,
        peers.listen(),
        data_dir(&format!("{tag}-n{}", i + 1)),
        peers.order().to_vec(),
        addresses,
    );
    cfg.profile = Profile::Lan;
    cfg
}

async fn start_cluster(tag: &str) -> Vec<AssembledClusterNode> {
    let mut nodes = Vec::new();
    for (i, port) in PORTS.iter().enumerate() {
        let peers = cluster_peers(
            &spec(),
            &format!("n{}", i + 1),
            &format!("127.0.0.1:{port}"),
        )
        .expect("the fixture peer table must parse");
        assert_eq!(
            peers.raft_id(),
            Some((i + 1) as u64),
            "member n{} must map to raft id {}",
            i + 1,
            i + 1
        );
        let node = assemble_cluster(member_config(&peers, i, tag))
            .await
            .unwrap_or_else(|e| panic!("assemble_cluster(n{}) failed: {e}", i + 1));
        nodes.push(node);
    }
    nodes
}

async fn shutdown_cluster(nodes: Vec<AssembledClusterNode>) {
    for n in nodes {
        n.tonic.shutdown().await;
        n.thread.shutdown();
    }
}

/// Poll until exactly one node accepts a non-redirecting write — which is both
/// "a leader exists" and "it is this node".
///
/// A node that is not ready answers `NotLeader` / `QuorumUnavailable` / `Timeout`
/// while an election is in flight; all three are "ask again", not "fail".
async fn wait_for_leader(nodes: &[AssembledClusterNode]) -> usize {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut leaders = Vec::new();
        for (i, n) in nodes.iter().enumerate() {
            match n.handle.without_redirect().put(b"hydra/probe", b"1").await {
                Ok(()) => leaders.push(i),
                Err(ArachneError::NotLeader { .. })
                | Err(ArachneError::QuorumUnavailable)
                | Err(ArachneError::Timeout)
                | Err(ArachneError::Busy) => {}
                Err(e) => panic!("unexpected error from n{}: {e:?}", i + 1),
            }
        }
        if leaders.len() == 1 {
            return leaders[0];
        }
        assert!(
            leaders.len() <= 1,
            "TWO nodes accepted a write ({leaders:?}) — the single-writer invariant is broken"
        );
        assert!(
            Instant::now() < deadline,
            "no leader was elected within 30 s"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The whole point of the peer table: three members become a cluster with one
/// leader, every member can serve a **local** read of a committed value (the
/// config-materialization path is built on this), and only the leader accepts a
/// non-redirecting write.
///
/// Falsification: give every node raft id 1 (drop `raft_id()`) and the
/// "exactly one leader" wait fails; skip the follower write-refusal probe and
/// the second assertion below can never fail, which is why it is here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_table_starts_a_cluster_with_exactly_one_writer() {
    let nodes = start_cluster("basic").await;
    let leader = wait_for_leader(&nodes).await;

    // The leader commits a value; every member must see it in its own state
    // machine (this is how a node materializes config, and it is a LOCAL read).
    nodes[leader]
        .handle
        .without_redirect()
        .put(b"hydra/cfg/head", b"toc-hash")
        .await
        .expect("leader write");

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let mut seen = 0;
        for n in &nodes {
            if n.handle
                .get_stale(b"hydra/cfg/head")
                .await
                .ok()
                .flatten()
                .as_deref()
                == Some(b"toc-hash".as_slice())
            {
                seen += 1;
            }
        }
        if seen == nodes.len() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "only {seen}/{} members materialized the committed value",
            nodes.len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // A follower must refuse a non-redirecting write: that refusal is what the
    // leader-only publish step depends on.
    for (i, n) in nodes.iter().enumerate() {
        if i == leader {
            continue;
        }
        let refused = n
            .handle
            .without_redirect()
            .put(b"hydra/probe/other", b"1")
            .await;
        assert!(
            matches!(refused, Err(ArachneError::NotLeader { .. })),
            "follower n{} must refuse a non-redirecting write, got {refused:?}",
            i + 1
        );
    }

    shutdown_cluster(nodes).await;
}
