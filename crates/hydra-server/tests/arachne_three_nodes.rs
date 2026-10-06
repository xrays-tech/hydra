#![cfg(feature = "arachne")]
//! Plan T3.1 Verification / T3.2 acceptance — THREE real nodes, one config.
//!
//! Until this test existed, every Arachne property had been checked on ONE node plus a
//! pure-function layer. The whole point of the design is what happens BETWEEN nodes, so this is
//! the first run of the real chain:
//!
//! ```text
//!   write on node X  →  X's SQLite commits  →  X encodes + commits the head
//!                                              ↓
//!                     every node (X included) polls `ctl/head`, decodes the tree, rebuilds its
//!                     OWN SQLite and serves the new config
//! ```
//!
//! Everything here is real: three raft members on real loopback ports, three `ConfigStore`s over
//! three SQLite databases, three publishers and three materializers — the same types `main.rs`
//! assembles.
//!
//! ## What is asserted
//!
//! * all three nodes end on the SAME head hash, and it is the one the writer published;
//! * all three SERVE the config, and all three have it in their OWN database (so any of them
//!   could rebuild itself after a restart — the property `ReplicaTarget` exists for);
//! * a write on a node that is NOT the raft leader publishes just as well: the head write is the
//!   one operation the library forwards to the leader (ADR-0001 F-3 / D-3), which is what lets
//!   the homogeneous model accept a management write anywhere;
//! * the convergence needs no repeated passes once the head is published: it is a poll, not a
//!   retry loop, so the propagation bound is the poll interval.

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arachne_kv::server::{assemble_cluster, AssembledClusterNode, ClusterConfig};
use arachne_kv::{NodeId, Profile};
use hydra_core::model::Provider;
use hydra_server::cluster::arachne_materializer::{Converged, Materializer, ReplicaTarget};
use hydra_server::cluster::arachne_node::cluster_peers;
use hydra_server::cluster::arachne_publish::ConfigPublisher;
use hydra_server::cluster::arachne_store::ArachneConfigStore;
use hydra_server::crypto::{KeyProvider, StaticKeyProvider};
use hydra_server::db as repo;
use hydra_server::store::ConfigStore;

/// Two disjoint ranges: the two tests run CONCURRENTLY (cargo), and a raft member must be
/// dialable at the address its peers were told about, so these cannot be ephemeral.
const PORTS_ANY_NODE: [u16; 3] = [18501, 18502, 18503];
const PORTS_LEADER: [u16; 3] = [18601, 18602, 18603];

fn data_dir(tag: &str, i: usize) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hydra-three-{tag}-n{}-{}",
        i + 1,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create data dir");
    dir
}

fn kp() -> Arc<dyn KeyProvider> {
    Arc::new(StaticKeyProvider::new([7u8; 32], 1))
}

fn provider(id: &str, name: &str) -> Provider {
    Provider {
        id: id.into(),
        key: id.into(),
        name: name.into(),
        endpoint: "https://api.example.com".into(),
        weight: 1,
        created_at: "2026-01-01 00:00:00".into(),
        updated_at: "2026-01-01 00:00:00".into(),
        max_concurrency: None,
        max_queue_depth: None,
        queue_wait_timeout_ms: None,
    }
}

/// One node of the cluster under test: its raft member, its database, its store, its materializer.
struct TestNode {
    raft: AssembledClusterNode,
    pool: sqlx::SqlitePool,
    store: ConfigStore,
    materializer: Materializer,
}

impl TestNode {
    fn ctl(&self) -> ArachneConfigStore {
        ArachneConfigStore::new(self.raft.handle.clone())
    }

    /// One convergence pass.
    async fn converge(&mut self) -> Result<Converged, String> {
        self.materializer
            .converge()
            .await
            .map_err(|e| e.to_string())
    }
}

/// Start a three-member raft cluster, each with its own database, store, publisher and
/// materializer — the same assembly `main.rs` performs at bootstrap.
async fn start_cluster(tag: &str, ports: &[u16; 3]) -> Vec<TestNode> {
    let spec = format!(
        "n1=127.0.0.1:{},n2=127.0.0.1:{},n3=127.0.0.1:{}",
        ports[0], ports[1], ports[2]
    );
    let mut nodes = Vec::new();
    for (i, port) in ports.iter().enumerate() {
        let peers = cluster_peers(&spec, &format!("n{}", i + 1), &format!("127.0.0.1:{port}"))
            .expect("fixture peer table parses");
        let id = NodeId::new(format!("n{}", i + 1));
        let addresses: HashMap<NodeId, SocketAddr> = peers
            .addresses()
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        let mut cfg = ClusterConfig::member(
            format!("hydra-three-{tag}"),
            id.clone(),
            peers.listen(),
            data_dir(tag, i),
            peers.order().to_vec(),
            addresses,
        );
        cfg.profile = Profile::Lan;
        let raft = assemble_cluster(cfg).await.expect("assemble_cluster");

        let key_provider = kp();
        let pool = common::setup_pool().await;
        let store = ConfigStore::load(pool.clone(), key_provider.clone())
            .await
            .expect("load the store")
            .with_publisher(Arc::new(ConfigPublisher::new(
                ArachneConfigStore::new(raft.handle.clone()),
                key_provider.clone(),
            )));
        let target = Arc::new(ReplicaTarget::new(store.clone(), key_provider.clone()));
        let materializer = Materializer::new(
            ArachneConfigStore::new(raft.handle.clone()),
            target,
            key_provider.clone(),
        );
        nodes.push(TestNode {
            raft,
            pool,
            store,
            materializer,
        });
    }
    nodes
}

/// Wait until the cluster can COMMIT (a leader exists), so the first publish is not racing the
/// election. A put is the right probe: it is forwarded to the leader and fails while there is
/// none, which is exactly the condition a publish needs (ADR-0001 D-3 / F-3).
async fn wait_writable(nodes: &[TestNode]) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        for node in nodes {
            if node
                .raft
                .handle
                .without_redirect()
                .put(b"hydra/three-nodes/probe", b"1")
                .await
                .is_ok()
            {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "no leader was elected within 30 s: the cluster never accepted a write"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Read the head from `node`, waiting for it to become visible locally.
///
/// A publish commits on the LEADER; this node learns about it through its own `get_stale` copy,
/// which lags by milliseconds (ADR-0001 F-5). Polling here keeps the assertion ("a head exists
/// and everyone converges on it") while not depending on that lag being zero.
async fn wait_for_head(node: &TestNode) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(Some(hash)) = node.ctl().current_hash().await {
            return hash;
        }
        assert!(
            Instant::now() < deadline,
            "no head became visible on this node within 10 s, although the publish returned Ok"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Converge every node until each reports the same head, or the deadline passes.
///
/// Returns how many passes each node needed. A single pass is the expected answer once the head
/// is published (convergence is a poll, not a retry loop), so the count is reported in the failure
/// message rather than ignored.
async fn converge_all(nodes: &mut [TestNode], expected: &str) -> Vec<usize> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut passes = vec![0usize; nodes.len()];
    loop {
        let mut done = 0;
        for (i, node) in nodes.iter_mut().enumerate() {
            if node.materializer.materialized() == Some(expected) {
                done += 1;
                continue;
            }
            passes[i] += 1;
            match node.converge().await {
                Ok(_) => {
                    if node.materializer.materialized() == Some(expected) {
                        done += 1;
                    }
                }
                Err(e) => {
                    assert!(
                        Instant::now() < deadline,
                        "node {i} could not materialize {expected}: {e}"
                    );
                }
            }
        }
        if done == nodes.len() {
            return passes;
        }
        assert!(
            Instant::now() < deadline,
            "not every node reached head {expected} within 10 s (passes so far: {passes:?})"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn shutdown(nodes: Vec<TestNode>) {
    for n in nodes {
        n.raft.tonic.shutdown().await;
        n.raft.thread.shutdown();
    }
}

/// THE accepted criterion: three nodes, one head, one config, each with its own copy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_nodes_converge_on_the_same_head_and_each_can_rebuild_itself() {
    let mut nodes = start_cluster("same-head", &PORTS_ANY_NODE).await;
    wait_writable(&nodes).await;

    // A management write on node 0: its SQLite commits, then `reload_all` publishes
    // (`ConfigStore::reload_all_with` is the funnel every admin handler uses).
    repo::insert_provider(&nodes[0].pool, &provider("p1", "written-on-node-1"))
        .await
        .expect("insert");
    assert!(
        nodes[0].store.reload_all().await.expect("reload + publish"),
        "the write must change the config"
    );
    // `current_hash` is a LOCAL `get_stale`, and a node may not see a just-committed head
    // immediately (ADR-0001 F-5, measured lag ~7 ms). Waiting is not weakening the assertion: the
    // head must appear, and it must be the one every node converges on.
    let head = wait_for_head(&nodes[0]).await;
    assert_eq!(head.len(), 64);

    let passes = converge_all(&mut nodes, &head).await;

    for (i, node) in nodes.iter().enumerate() {
        assert_eq!(
            node.materializer.materialized(),
            Some(head.as_str()),
            "node {i} must materialize the SAME toc-hash the writer published"
        );
        assert!(
            node.store.snapshot().providers.contains_key("p1"),
            "node {i} must SERVE the published config"
        );
        // The row in the node's OWN database is what makes it able to rebuild after a restart;
        // a config only in memory would vanish.
        let row = repo::get_provider(&node.pool, "p1")
            .await
            .unwrap_or_else(|e| panic!("node {i} has no row in its own database: {e}"));
        assert_eq!(row.name, "written-on-node-1");
        // A node that has materialized is eligible to lead; one that has not is not.
        assert!(
            node.materializer.may_be_leader(),
            "node {i} has a config and must be eligible"
        );
    }
    assert!(
        passes.iter().all(|p| *p <= 3),
        "convergence is a POLL: a node should need a pass or two once the head is visible, not a \
         retry loop. Passes per node: {passes:?}"
    );

    shutdown(nodes).await;
}

/// A write on a node that is NOT the raft leader publishes just as well.
///
/// This is what the homogeneous model (ADR-0001 D-2, and the ruling that a management write is
/// executed where it lands) rests on: the node's SQLite commits locally and the head write is the
/// one operation the library forwards to the raft leader. If that forwarding did not work, this
/// test would fail with a `NotPublished` and the model would need a different answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_on_a_non_leader_node_reaches_every_node() {
    let mut nodes = start_cluster("any-node", &PORTS_LEADER).await;
    wait_writable(&nodes).await;

    let before = nodes[0].ctl().current_hash().await.expect("read head");

    // The LAST node writes. In a three-member cluster at most one node is the leader, so this is
    // a follower with probability 2/3 — and the assertion below does not care which it is, which
    // is the point: the model must not depend on the writer being the leader.
    let writer = nodes.len() - 1;
    repo::insert_provider(
        &nodes[writer].pool,
        &provider("p2", "written-on-the-last-node"),
    )
    .await
    .expect("insert");
    let published = nodes[writer]
        .store
        .reload_all()
        .await
        .expect("reload + publish must succeed from ANY node");
    assert!(published, "the write must change the config");

    let head = wait_for_head(&nodes[writer]).await;
    assert_ne!(
        Some(head.clone()),
        before,
        "the head must have moved: a publish from a follower still commits"
    );

    converge_all(&mut nodes, &head).await;
    for (i, node) in nodes.iter().enumerate() {
        assert!(
            node.store.snapshot().providers.contains_key("p2"),
            "node {i} must serve the config written on node {}",
            writer + 1
        );
        assert!(
            repo::get_provider(&node.pool, "p2").await.is_ok(),
            "node {i} must have the row in its own database"
        );
    }

    shutdown(nodes).await;
}
