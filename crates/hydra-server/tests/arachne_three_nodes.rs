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
use hydra_server::store::{ConfigStore, StoreError};

/// Two disjoint ranges: the two tests run CONCURRENTLY (cargo), and a raft member must be
/// dialable at the address its peers were told about, so these cannot be ephemeral.
const PORTS_ANY_NODE: [u16; 3] = [18501, 18502, 18503];
const PORTS_ANY_NODE_ALT: [u16; 3] = [18701, 18702, 18703];
const PORTS_NO_QUORUM: [u16; 3] = [18801, 18802, 18803];
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
    let mut previous: Option<String> = None;
    loop {
        if let Ok(Some(hash)) = node.ctl().current_hash().await {
            // STABLE, not merely visible: this node's view is a local `get_stale`, so right after
            // a publish it can still show the PREVIOUS head. Returning that value made a test
            // compare a stale hash against the real one — the cluster had converged correctly and
            // the assertion was wrong (measured: expected 22f9af3c, all three nodes on 67ac96b8).
            if previous.as_deref() == Some(hash.as_str()) {
                return hash;
            }
            previous = Some(hash);
        }
        assert!(
            Instant::now() < deadline,
            "no STABLE head became visible on this node within 10 s, although the publish \
             returned Ok"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
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
    // The LAST outcome per node, so a failure says WHICH node is stuck and WHY instead of only
    // "not converged" — the first version of this helper left an operator to guess.
    let mut last: Vec<String> = vec!["(never ran)".to_string(); nodes.len()];
    loop {
        let mut done = 0;
        for (i, node) in nodes.iter_mut().enumerate() {
            if node.materializer.materialized() == Some(expected) {
                done += 1;
                continue;
            }
            passes[i] += 1;
            match node.converge().await {
                Ok(outcome) => {
                    last[i] = format!("{outcome:?}");
                    if node.materializer.materialized() == Some(expected) {
                        done += 1;
                    }
                }
                Err(e) => last[i] = format!("ERR {e}"),
            }
        }
        if done == nodes.len() {
            return passes;
        }
        assert!(
            Instant::now() < deadline,
            "not every node reached head {expected} within 10 s (passes: {passes:?}, last \
             outcomes: {last:?}, materialized: {:?})",
            nodes
                .iter()
                .map(|n| n.materializer.materialized().map(str::to_string))
                .collect::<Vec<_>>()
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

/// A MANAGEMENT WRITE ON EVERY NODE IS ACCEPTED, AND THE CLUSTER ENDS ON ONE CONFIG.
///
/// This is the contract that replaced HTTP forwarding (ADR-0001 D-3, T3.3): a node executes the
/// mutation against its OWN database and publishes the result, instead of relaying the request to
/// the lease holder. The retired contract sat in `tests/cluster.rs`
/// (`standby_forwards_mutations_to_active`, and the 502/504 mapping tests): they asserted that a
/// standby never writes locally, which is now the OPPOSITE of the design.
///
/// Three writes go to three different nodes. Each is a DIFFERENT config (a different provider id),
/// so the head legitimately moves three times and the cluster converges on whichever publish
/// landed last — the point is that every write was ACCEPTED and that all three nodes agree
/// afterwards, not that the three changes merge (they do not; concurrent management writes are
/// last-writer-wins, which is the model's accepted exposure).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_management_write_on_any_node_is_accepted_and_the_cluster_converges() {
    let mut nodes = start_cluster("write-anywhere", &PORTS_ANY_NODE_ALT).await;
    wait_writable(&nodes).await;

    for (i, node) in nodes.iter().enumerate() {
        let id = format!("p-node{}", i + 1);
        repo::insert_provider(&node.pool, &provider(&id, "written-here"))
            .await
            .unwrap_or_else(|e| panic!("insert on node {i}: {e}"));
        assert!(
            node.store
                .reload_all()
                .await
                .unwrap_or_else(|e| panic!("node {i} could not publish its own write: {e}")),
            "node {i} must see a change and publish it"
        );
    }

    // WHICH tree wins is decided by which publish landed last, so the assertion is not "the head
    // is X" but "every node ends on the SAME head" — read after the dust settles.
    let head = wait_for_head(&nodes[0]).await;
    converge_all(&mut nodes, &head).await;

    let served: Vec<Vec<String>> = nodes
        .iter()
        .map(|n| {
            let mut ids: Vec<String> = n.store.snapshot().providers.keys().cloned().collect();
            ids.sort();
            ids
        })
        .collect();
    assert!(
        served.windows(2).all(|w| w[0] == w[1]),
        "all three nodes must serve the same provider set after converging, got {served:?}"
    );
    assert!(
        !served[0].is_empty(),
        "and it must not be empty: a converged node serves what the head names"
    );

    shutdown(nodes).await;
}

/// A write whose PUBLISH cannot commit is refused with 503, not silently accepted.
///
/// The successor of the retired `a_refused_leader_produces_502_forward_failed`: there is no
/// forwarding to fail any more, so the interesting failure is a publish that cannot reach the
/// cluster. This node is a member of a three-member cluster whose peers were NEVER STARTED, so no
/// leader can be elected — deterministic, unlike killing a leader and racing the election.
///
/// The write itself still lands in the local database (the SQLite transaction is local and
/// committed); what fails is making the cluster see it, which is exactly what
/// `StoreError::NotPublished` exists to say and what the admin layer turns into 503
/// `config_not_published`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_that_cannot_be_published_is_refused_rather_than_accepted() {
    // One member of three, alone: its peers are unreachable, so there is no quorum and no leader.
    let mut nodes = start_cluster("no-quorum", &PORTS_NO_QUORUM).await;
    let lonely = &mut nodes[0];
    // No `wait_writable` here on purpose: the point is that the cluster never becomes writable.
    repo::insert_provider(&lonely.pool, &provider("p1", "written-while-alone"))
        .await
        .expect("the local insert succeeds: it is this node's own database");

    let got = lonely.store.reload_all().await;
    match got {
        Err(StoreError::NotPublished { reason }) => {
            assert!(
                !reason.is_empty(),
                "the refusal must carry the reason the cluster did not take the config"
            );
        }
        other => panic!(
            "a config that cannot be published must be refused, not reported as applied: {other:?}"
        ),
    }

    // The local half stands — and that is precisely why the error is worded the way it is.
    assert!(
        lonely.store.snapshot().providers.contains_key("p1"),
        "this node committed and serves its own write; the cluster simply does not have it yet"
    );

    shutdown(nodes).await;
}

/// The ordering predicate, deterministically — no cluster, no timing.
///
/// A head whose log index is LOWER than the generation this node has applied is a STALE READ: the read
/// upstream provides is "arbitrarily old (N1)", so it can answer with an older head, and applying that
/// would REPLACE this node's database and snapshot with an older tree — rolling back over writes it had
/// already published, which later publishes then propagate into the head as a permanent loss
/// (ADR-0001 plan, "观察"). Falsification: delete the `<` in `is_stale_generation` and the first
/// assertion below goes red.
#[test]
fn a_lower_log_index_is_a_stale_generation_and_is_refused() {
    use hydra_server::cluster::arachne_materialize::is_stale_generation;

    assert!(
        is_stale_generation(Some(4), Some(5)),
        "an older index must be refused"
    );
    assert!(
        !is_stale_generation(Some(5), Some(5)),
        "the same generation is not older — re-materializing it is a no-op the gate handles"
    );
    assert!(
        !is_stale_generation(Some(6), Some(5)),
        "a newer index applies"
    );
    assert!(
        !is_stale_generation(None, Some(5)),
        "an upstream that reports no index must not freeze a node: None is not an order"
    );
    assert!(
        !is_stale_generation(Some(5), None),
        "a node that has applied nothing has no generation to be older than"
    );
}
