#![cfg(feature = "arachne")]
//! Plan B2 (ADR-0001 §12 / `2026-10-05-arachne-control-plane.md`) — the **ALIVE partition**: a node
//! that is isolated from the cluster but still RUNNING keeps serving the config it already
//! materialized, and the ordering guard never fires on it.
//!
//! ## What this adds that nothing else covers
//!
//! Every existing cluster test reaches "no quorum" by KILLING nodes (three real processes in
//! `integration/test_arachne_control_plane.py`, two of three in `test_tenant_write_publish_failure.py`).
//! That is **crash** semantics. This test is the other half: the node is *alive*, its process, runtime
//! and materializer all keep running, it just cannot exchange a single message with its peers. Those
//! two cases differ exactly where the ordering work of `da622f4` lives — a crashed node applies
//! nothing, while an ALIVE isolated node keeps polling its own lagging replica and deciding whether to
//! apply. So this is the test that says the new "refuse an older head at a lower log index" branch
//! (ADR-0001 §12, `is_stale_generation`) does not fire spuriously and cost a node its availability.
//!
//! ## The partition is REAL, and it is not a crash
//!
//! `arachne_kv_testsupport::InMemoryTransportFactory::firewall(from, to)` drops messages SILENTLY while
//! keeping both channels alive — the in-process equivalent of DROPping packets or hanging a connection.
//! Killing the task or closing the listener would be the crash semantics this test exists to avoid
//! (upstream's recipe says so explicitly, and so does our record).
//!
//! The guard `firewall_drop_count() > 0` is asserted BEFORE any conclusion is drawn: without it the
//! test could pass while measuring nothing at all (a firewall that drops nothing looks exactly like a
//! healthy cluster).
//!
//! ## Scope — what is deliberately NOT asserted here
//!
//! **The catch-up after healing is not in this test.** `arachne-kv-testsupport` 0.3.1 publishes
//! `firewall(from, to)` and `firewall_drop_count()` but NO way to lift a firewall, and a node's
//! transport is handed to `Runtime::new` once (rebuilding the factory would not re-wire the running
//! node). The rejoin direction is covered where it really happens: a node that RESTARTS and rebuilds
//! itself from the head (`three_nodes_converge_on_the_same_head_and_each_can_rebuild_itself`, and the
//! failover drill). The refusal branch that needs a LAGGING REPLICA is covered by
//! `an_older_hash_at_a_lower_index_is_refused_and_nothing_is_reapplied` in `arachne_materializer.rs`.
//!
//! ## Falsification (mandatory, and it is how this file was accepted)
//!
//! Delete the two `factory.firewall(...)` calls: the victim then sees h2 and converges to it, so
//! "the isolated node still serves h1 / is still on h1 / did not take p2" goes RED. A version of this
//! test that stays green without the firewall would be measuring the empty set.

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arachne_kv::client::Handle;
use arachne_kv::consensus::RaftNodeConfig;
use arachne_kv::runtime::{Runtime, RuntimeConfig};
use arachne_kv::storage::{FsyncPolicy, WalConfig, WalOptions, WalStorage};
use arachne_kv::{Metrics, NodeId, Profile, ProfileConfig, TransportFactory};
use arachne_kv_testsupport::InMemoryTransportFactory;
use hydra_core::model::Provider;
use hydra_server::cluster::arachne_materializer::{Converged, Materializer, ReplicaTarget};
use hydra_server::cluster::arachne_publish::ConfigPublisher;
use hydra_server::cluster::arachne_store::ArachneConfigStore;
use hydra_server::crypto::{KeyProvider, StaticKeyProvider};
use hydra_server::db as repo;
use hydra_server::store::ConfigStore;
use slog::{o, Drain, Logger};

/// Three members, and the victim is a FOLLOWER: isolating it leaves a live majority (2 of 3) that
/// keeps its leader and can commit. With two members the same isolation would be a total outage and
/// there would be no h2 to compare against.
const N: usize = 3;

/// The in-memory transport never dials these, but the runtime is given a complete address table (the
/// same shape production hands it), so the test does not depend on the addresses being unused.
const BASE_PORT: u16 = 7_181;

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn logger() -> Logger {
    Logger::root(slog::Discard.fuse(), o!())
}

fn node_id(i: usize) -> NodeId {
    NodeId::new(format!("n{}", i + 1))
}

fn raft_id(i: usize) -> u64 {
    (i + 1) as u64
}

fn peers_of(i: usize) -> HashMap<u64, NodeId> {
    (0..N)
        .filter(|j| *j != i)
        .map(|j| (raft_id(j), node_id(j)))
        .collect()
}

fn addresses() -> HashMap<NodeId, SocketAddr> {
    (0..N)
        .map(|i| {
            (
                node_id(i),
                SocketAddr::from(([127, 0, 0, 1], BASE_PORT + i as u16)),
            )
        })
        .collect()
}

/// Short timeouts: this cluster lives entirely in memory, so a slow election here is a real
/// regression rather than a slow machine. `election_timeout_ms` stays well above the heartbeat.
fn test_profile() -> ProfileConfig {
    ProfileConfig {
        heartbeat_interval_ms: 10,
        election_timeout_ms: 600,
        rpc_timeout_ms: 300,
        read_index_timeout_ms: 3000,
        ..Profile::Lan.config()
    }
}

fn data_dir(tag: &str, i: usize) -> PathBuf {
    let n = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "hydra-alive-partition-{tag}-n{}-{}-{n}",
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

/// One in-process member: its raft runtime, its own database, its store and its materializer — the
/// same assembly `main.rs` performs, with the transport swapped for the in-memory one.
struct Node {
    handle: Handle,
    metrics: Arc<Metrics>,
    pool: sqlx::SqlitePool,
    store: ConfigStore,
    materializer: Materializer,
    dir: PathBuf,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Node {
    fn ctl(&self) -> ArachneConfigStore {
        ArachneConfigStore::new(self.handle.clone())
    }

    /// The runtime task is still RUNNING: not aborted by us, and not finished — a runtime that
    /// panicked under the isolation WOULD be finished, so this is a mechanical version of the
    /// alive-vs-crash distinction rather than a tautology about a handle nothing ever dropped.
    fn is_running(&self) -> bool {
        self.task.as_ref().is_some_and(|t| !t.is_finished())
    }

    async fn converge(&mut self) -> Converged {
        self.materializer
            .converge()
            .await
            .unwrap_or_else(|e| panic!("a convergence pass must not error: {e}"))
    }

    fn kill(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// A WAL on disk, retried: `WalStorage::open` can lose a race with the previous test's cleanup.
async fn open_wal(dir: &Path, i: usize, tag: &str) -> WalStorage {
    let opts = || WalOptions {
        cluster_id: format!("hydra-alive-{tag}"),
        node_id: format!("n{}", i + 1),
        config: WalConfig {
            fsync_policy: FsyncPolicy::Always,
            segment_bytes: 1 << 20,
        },
        created_at_millis: 1_700_000_000_000,
        fsync_observer: None,
    };
    let mut last = String::new();
    for _ in 0..200 {
        match WalStorage::open(dir, opts()) {
            Ok(wal) => return wal,
            Err(e) => {
                last = e.to_string();
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
    panic!("node {} could not open its WAL: {last}", i + 1);
}

async fn spawn_node(
    i: usize,
    tag: &str,
    factory: &InMemoryTransportFactory,
    addresses: &HashMap<NodeId, SocketAddr>,
    profile: &ProfileConfig,
) -> Node {
    let dir = data_dir(tag, i);
    let wal = open_wal(&dir, i, tag).await;
    let (tx, rx) = factory.create(node_id(i));
    let metrics = Arc::new(Metrics::new());
    let config = RuntimeConfig {
        self_raft_id: raft_id(i),
        self_node_id: node_id(i),
        peers: peers_of(i),
        addresses: addresses.clone(),
        raft: RaftNodeConfig::from_profile(profile),
        profile: profile.clone(),
        metrics: Arc::clone(&metrics),
    };
    let (runtime, handle) =
        Runtime::new(config, wal, tx, rx, &logger()).expect("build the runtime");
    let task = tokio::spawn(runtime.run());

    let key_provider = kp();
    let pool = common::setup_pool().await;
    let store = ConfigStore::load(pool.clone(), key_provider.clone())
        .await
        .expect("load the store")
        .with_publisher(Arc::new(ConfigPublisher::new(
            ArachneConfigStore::new(handle.clone()),
            key_provider.clone(),
        )));
    let target = Arc::new(ReplicaTarget::new(store.clone(), key_provider.clone()));
    let materializer = Materializer::new(
        ArachneConfigStore::new(handle.clone()),
        target,
        key_provider.clone(),
    );

    Node {
        handle,
        metrics,
        pool,
        store,
        materializer,
        dir,
        task: Some(task),
    }
}

/// In-memory peers are wired by HANDLES, not by addresses: each node registers every other node's
/// handle, and the factory's firewall sits between them.
fn link_peers(nodes: &[Node]) {
    for i in 0..nodes.len() {
        for j in 0..nodes.len() {
            if i != j {
                nodes[i].handle.register_peer(nodes[j].handle.clone());
            }
        }
    }
}

/// Wait until the cluster can COMMIT, so the first publish does not race the election.
async fn wait_writable(nodes: &[Node]) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        for node in nodes {
            if node
                .handle
                .without_redirect()
                .put(b"hydra/alive-partition/probe", b"1")
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

/// The head as THIS node sees it, waiting for it to be STABLE: `current_hash` is a local
/// `get_stale`, so right after a publish it can still show the previous head (ADR-0001 F-5), and
/// comparing that value is how an earlier test produced a false failure.
async fn wait_for_head(node: &Node) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut previous: Option<String> = None;
    loop {
        if let Ok(Some(hash)) = node.ctl().current_hash().await {
            if previous.as_deref() == Some(hash.as_str()) {
                return hash;
            }
            previous = Some(hash);
        }
        assert!(
            Instant::now() < deadline,
            "no STABLE head became visible on this node within 10 s"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Drive ONE node's materializer until it holds `expected` (or fail with what it actually holds).
async fn converge_until(node: &mut Node, expected: &str, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if node.materializer.materialized() == Some(expected) {
            return;
        }
        let last = format!("{:?}", node.converge().await);
        if node.materializer.materialized() == Some(expected) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{label}: the node never reached head {expected} within 10 s (last pass: {last}, \
             materialized: {:?})",
            node.materializer.materialized()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Publish a provider on `writer` and return the head every node must converge on.
async fn write_and_publish(nodes: &mut [Node], writer: usize, id: &str, name: &str) -> String {
    repo::insert_provider(&nodes[writer].pool, &provider(id, name))
        .await
        .expect("insert the provider row");
    assert!(
        nodes[writer]
            .store
            .reload_all()
            .await
            .expect("reload + publish"),
        "the write must change the config"
    );
    wait_for_head(&nodes[writer]).await
}

async fn shutdown(mut nodes: Vec<Node>) {
    let dirs: Vec<PathBuf> = nodes.iter().map(|n| n.dir.clone()).collect();
    for node in nodes.iter_mut() {
        node.kill();
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    for dir in dirs {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_isolated_but_alive_node_keeps_serving_its_last_materialized_config() {
    let profile = test_profile();
    let factory = InMemoryTransportFactory::new();
    let addresses = addresses();

    let mut nodes = Vec::new();
    for i in 0..N {
        nodes.push(spawn_node(i, "alive", &factory, &addresses, &profile).await);
    }
    link_peers(&nodes);
    wait_writable(&nodes).await;

    // ---- h1: written, published, and materialized by ALL THREE --------------------------------
    let h1 = write_and_publish(&mut nodes, 0, "p1", "before-the-partition").await;
    assert_eq!(h1.len(), 64, "a toc hash is 64 hex characters");
    for (i, node) in nodes.iter_mut().enumerate() {
        converge_until(node, &h1, &format!("node {i} (pre-partition)")).await;
    }

    // A FOLLOWER is isolated: the majority keeps its leader, so h2 is committed WITHOUT a
    // re-election in the middle of the measurement.
    let victim = (0..N)
        .find(|i| !nodes[*i].metrics.is_leader())
        .expect("a three-member cluster has at most one leader, so a follower exists");
    let victim_id = node_id(victim);
    for j in 0..N {
        if j != victim {
            factory.firewall(victim_id.clone(), node_id(j));
            factory.firewall(node_id(j), victim_id.clone());
        }
    }

    // NON-VACUOUS GUARD, before any conclusion: the firewall must actually be dropping traffic.
    let deadline = Instant::now() + Duration::from_secs(5);
    while factory.firewall_drop_count() == 0 {
        assert!(
            Instant::now() < deadline,
            "the firewall never dropped a message within 5 s — this test would be measuring \
             nothing (the victim's runtime must be sending heartbeats/votes into it)"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // ---- h2: the majority moves on while the victim is isolated -------------------------------
    let writer = (0..N).find(|i| *i != victim).expect("a majority exists");
    let h2 = write_and_publish(&mut nodes, writer, "p2", "after-the-partition").await;
    assert_ne!(h1, h2, "the majority write must produce a different tree");
    for (i, node) in nodes.iter_mut().enumerate() {
        if i != victim {
            converge_until(node, &h2, &format!("node {i} (majority)")).await;
        }
    }

    // ---- the victim is ALIVE, and it still serves h1 ------------------------------------------
    // Alive, not crashed: its runtime task is still running, which is the difference between this
    // test and every other "no quorum" case in the suite (those KILL nodes).
    assert!(
        nodes[victim].is_running(),
        "the isolated node's runtime must still be RUNNING — this is an alive partition, not a crash"
    );
    // Canary: its own replica still answers, and answers h1. A crashed node would error here, and a
    // node that had "helpfully" invented a head would report something else.
    let canary = nodes[victim]
        .ctl()
        .current_hash()
        .await
        .expect("the isolated node must still serve its local head (it is ALIVE, not crashed)");
    assert_eq!(
        canary.as_deref(),
        Some(h1.as_str()),
        "the isolated node's head must still be the one it materialized (a head that is GONE \
         would mean the isolated node wiped or could not read its own replica)"
    );

    assert_eq!(
        nodes[victim].materializer.materialized(),
        Some(h1.as_str()),
        "the isolated node must keep the config it materialized"
    );
    assert!(
        nodes[victim].store.snapshot().providers.contains_key("p1"),
        "the isolated node must still SERVE the pre-partition config"
    );
    assert!(
        !nodes[victim].store.snapshot().providers.contains_key("p2"),
        "the isolated node must NOT hold a config it cannot have received"
    );
    let row = repo::get_provider(&nodes[victim].pool, "p1")
        .await
        .expect("its own database must still hold the row (a restart must be able to rebuild)");
    assert_eq!(row.name, "before-the-partition");
    assert!(
        repo::get_provider(&nodes[victim].pool, "p2").await.is_err(),
        "h2 must not appear in the isolated node's own database"
    );

    // The ordering guard must not fire on a node that is merely isolated: it has no newer head to
    // protect, and refusing here would turn a partition into a self-inflicted config wipeout.
    assert_eq!(
        nodes[victim].converge().await,
        Converged::NoChange,
        "an isolated node has nothing to apply, so a pass must be a no-op — not a refusal, and \
         certainly not a wipe"
    );

    // ---- the majority really did move on ------------------------------------------------------
    for (i, node) in nodes.iter().enumerate() {
        if i == victim {
            continue;
        }
        assert_eq!(
            node.materializer.materialized(),
            Some(h2.as_str()),
            "node {i} is in the majority and must hold h2"
        );
        assert!(
            node.store.snapshot().providers.contains_key("p2"),
            "node {i} must serve the post-partition config"
        );
    }

    shutdown(nodes).await;
}
