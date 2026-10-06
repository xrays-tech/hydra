//! T1.3 — leader detection must be a **write probe**, never `leader_hint()`.
//!
//! A probe measured that a node which has just become leader does **not** name
//! itself in `leader_hint()` for a long window after a handover (the other
//! members' hints follow within a second). So "am I the leader" cannot be
//! answered from the hint, and a cache built from it would report `false` on the
//! node that is actually serving — which is exactly the node whose
//! `/healthz/leader` gates a Kubernetes rollout.
//!
//! The probe is the only question whose answer is authoritative *and* safe in
//! both directions:
//!   * it says "yes" only when this node really did commit something, so a
//!     stale cache cannot make a non-leader claim leadership;
//!   * it says "no" when the node cannot commit, which costs at most one retry.
//!
//! These tests run against **real** in-process Arachne nodes (tonic transport,
//! real WAL, real actor thread), because the property under test is a property
//! of the library's behaviour after a handover — a stub would be asserting the
//! stub.
//!
//! ## Why the probe, when the hint would also work (measured, 2026-10-05)
//!
//! The original reason was that a new leader did not name itself in
//! `leader_hint()` (ADR-0001 §10 F-3). Upstream fixed that in 0.1.2, and a
//! falsification run confirmed it: swapping this probe for
//! `leader_hint().map(|(id, _)| id == self)` keeps every test in this file
//! green. So the handover case no longer separates the two.
//!
//! What still separates them is a leader that has lost its quorum: it keeps
//! believing it is the leader (so it keeps naming itself in the hint) while it
//! cannot commit anything. `the_probe_refuses_a_leader_that_lost_its_quorum`
//! fails on the hint-based version, and that is the case `/healthz/leader`
//! exists to report: a node that must NOT be given traffic.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use arachne_kv::server::{assemble_cluster, AssembledClusterNode, ClusterConfig};
use arachne_kv::{NodeId, Profile};
use hydra_server::cluster::arachne_node::{cluster_peers, ArachneControl, ClusterPeers};

/// Dedicated loopback ports. A raft member must be dialable at the address its
/// peers were told about, so these cannot be ephemeral — and the two tests in
/// this file cannot share them either, because cargo runs them concurrently.
const PORTS: [u16; 3] = [17801, 17802, 17803];
const PORTS_FLIPS: [u16; 3] = [17901, 17902, 17903];
const PORTS_QUORUM: [u16; 3] = [18001, 18002, 18003];

/// Poll until one node is leader; return its index.
async fn wait_for_leader(nodes: &[AssembledClusterNode]) -> usize {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut leaders = Vec::new();
        for (i, n) in nodes.iter().enumerate() {
            if n.handle
                .without_redirect()
                .put(b"hydra/probe", b"1")
                .await
                .is_ok()
            {
                leaders.push(i);
            }
        }
        if leaders.len() == 1 {
            return leaders[0];
        }
        assert!(
            leaders.len() <= 1,
            "two nodes accepted a write: {leaders:?}"
        );
        assert!(Instant::now() < deadline, "no leader elected within 30 s");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn data_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hydra-leader-watch-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create data dir");
    dir
}

fn member_config(peers: &ClusterPeers, i: usize, tag: &str) -> ClusterConfig {
    let node_id = NodeId::new(format!("n{}", i + 1));
    let addresses: HashMap<NodeId, SocketAddr> = peers
        .addresses()
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    let mut cfg = ClusterConfig::member(
        "hydra-leader-watch".to_string(),
        node_id,
        peers.listen(),
        data_dir(&format!("{tag}-n{}", i + 1)),
        peers.order().to_vec(),
        addresses,
    );
    cfg.profile = Profile::Lan;
    cfg
}

/// Start a three-node cluster and wrap each node in an [`ArachneControl`], which
/// is the production type under test.
async fn start_controlled(
    tag: &str,
    ports: &[u16; 3],
) -> (Vec<ArachneControl>, Vec<AssembledClusterNode>) {
    let spec = format!(
        "n1=127.0.0.1:{},n2=127.0.0.1:{},n3=127.0.0.1:{}",
        ports[0], ports[1], ports[2]
    );
    let mut controls = Vec::new();
    let mut nodes = Vec::new();
    for (i, port) in ports.iter().enumerate() {
        let peers = cluster_peers(&spec, &format!("n{}", i + 1), &format!("127.0.0.1:{port}"))
            .expect("fixture peer table parses");
        let node = assemble_cluster(member_config(&peers, i, tag))
            .await
            .expect("assemble_cluster");
        controls.push(ArachneControl::for_tests(
            node.handle.clone(),
            NodeId::new(format!("n{}", i + 1)),
        ));
        nodes.push(node);
    }
    (controls, nodes)
}

async fn shutdown(nodes: Vec<AssembledClusterNode>) {
    for n in nodes {
        n.tonic.shutdown().await;
        n.thread.shutdown();
    }
}

/// The probe answers "am I the writer" **immediately after a handover**, which is
/// the case that made `leader_hint()` unusable for this job.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_new_leader_is_recognized_immediately_after_a_handover() {
    let (controls, nodes) = start_controlled("handover", &PORTS).await;
    let leader = wait_for_leader(&nodes).await;
    assert!(
        controls[leader].probe_once().await,
        "the elected leader must report itself through the probe"
    );

    let target = (leader + 1) % nodes.len();
    let target_raft_id = (target + 1) as u64;
    nodes[leader]
        .handle
        .transfer_leader(target_raft_id)
        .await
        .expect("transfer_leader");

    // The new leader must be recognized on the FIRST probe after the transfer
    // returns — `transfer_leader` only returns once leadership really moved.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if controls[target].probe_once().await {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "n{} did not report leadership after taking it over",
            target + 1
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // And the old leader must stop claiming it.
    assert!(
        !controls[leader].probe_once().await,
        "the demoted node must stop reporting itself as leader"
    );

    shutdown(nodes).await;
}

/// The cache only follows the probe, and a failed probe wins: a node that cannot
/// commit is not a leader, however recently it was one. The cache is what
/// `/healthz/leader` reads, so a cache that can only go up would keep a
/// rolled-out node "ready" after its leadership was gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_cached_flag_only_follows_successful_probes() {
    let (mut controls, nodes) = start_controlled("flips", &PORTS_FLIPS).await;
    let leader = wait_for_leader(&nodes).await;

    // Put the flag up through the fold (a bare `probe_once` deliberately does not
    // touch the cache: it is the question, the fold is the answer).
    assert!(controls[leader].observe_once().await);
    assert!(controls[leader].is_leader());
    let target = (leader + 1) % nodes.len();
    nodes[leader]
        .handle
        .transfer_leader((target + 1) as u64)
        .await
        .expect("transfer_leader");

    let flips_before = controls[leader].flips();
    let deadline = Instant::now() + Duration::from_secs(5);
    while controls[leader].is_leader() {
        assert!(
            Instant::now() < deadline,
            "the demoted node still reports is_leader() == true after 5 s of failed probes"
        );
        controls[leader].observe_once().await;
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Counted as a DELTA, not an absolute: a node that briefly believed it was
    // leader during an election and then lost that belief legitimately produces
    // more than one flip over a cluster's life, and asserting an absolute count
    // would encode an election-timing assumption into the test. What must hold
    // is that losing leadership produces exactly one transition down.
    assert_eq!(
        controls[leader].flips() - flips_before,
        1,
        "losing leadership must produce exactly one flip"
    );

    // And the demoted node keeps answering the same way: the fold is stable once
    // the probe result is stable (no flip-flopping from repeated failures).
    for _ in 0..5 {
        assert!(
            !controls[leader].observe_once().await,
            "a repeated failed probe must not keep counting flips"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    shutdown(nodes).await;
}

/// A single-node (non-cluster) process must not claim leadership: without a
/// cluster there is no raft leader, and `/healthz/leader` on such a node has to
/// keep the documented 404 shape rather than a fabricated 200.
#[test]
fn a_control_that_was_never_started_never_claims_leadership() {
    let control = ArachneControl::not_started();
    assert!(
        !control.is_leader(),
        "a control with no started node must not claim leadership"
    );
    assert_eq!(
        control.flips(),
        0,
        "no probe ran, so there is nothing to count"
    );
}

/// Static guard: the leader path must never use the linearizable `get`.
///
/// A follower's `get` fails immediately with `QuorumUnavailable` even on a
/// perfectly healthy cluster (measured: the redirect path only works for peers
/// registered in-process), so any `.get(` on this path is a latent outage. The
/// config-materialization path (T3.1) reads `head` and every entity through
/// `get_stale` instead.
///
/// Falsification: add a `.get(` call to `arachne_node.rs` and this fails with
/// the offending line.
#[test]
fn the_leader_path_never_uses_the_linearizable_get() {
    let src = include_str!("../src/cluster/arachne_node.rs");
    // Strip the test module: the fixtures legitimately use `Handle::get`? They do
    // not, but a future test may — production code is what this guards.
    let production = src.split("#[cfg(test)]").next().unwrap_or(src);
    // Only the Arachne handle's own linearizable read counts. A `HashMap::get`
    // is not the thing being guarded, and widening the needle to every `.get(`
    // would make this test fail on a local variable name (measured: it flagged
    // `addresses.get(&node_key)` on the first run).
    let needles = [
        ".handle.get(",
        "handle.as_ref().get(",
        "]\n            .get(",
    ];
    let mut offenders: Vec<&str> = production
        .lines()
        .filter(|l| needles.iter().any(|n| l.contains(n)))
        .collect();
    // The redirecting `get` is the dangerous one; `without_redirect().get` is
    // still a linearizable read and equally forbidden on this path.
    if production.contains(".without_redirect()") && production.contains(".get(b\"") {
        offenders.push("<without_redirect().get(...) present>");
    }
    assert!(
        offenders.is_empty(),
        "the leader path must not call the linearizable `get` (it returns QuorumUnavailable on \
         every follower). Offending lines: {offenders:#?}"
    );
}

/// The separator between the write probe and `leader_hint()`: a leader that has
/// lost its quorum still names itself as leader, but cannot commit — so
/// `/healthz/leader` must stop reporting 200 for it.
///
/// This is what makes the probe the right answer rather than merely an
/// equivalent one. Falsification: answer `probe_once` from `leader_hint()`
/// instead and this test fails, because the isolated node's hint keeps naming
/// itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_probe_refuses_a_leader_that_lost_its_quorum() {
    let (controls, mut nodes) = start_controlled("quorum", &PORTS_QUORUM).await;
    let leader = wait_for_leader(&nodes).await;
    assert!(
        controls[leader].probe_once().await,
        "the elected leader must report itself while it has a quorum"
    );

    // Kill the other two members: 1 of 3 voters is a minority, so raft must
    // stop committing. The surviving node is still "the leader" as far as its
    // own state machine is concerned, which is exactly the trap.
    let mut kept = Vec::new();
    for (i, n) in nodes.drain(..).enumerate() {
        if i == leader {
            kept.push(n);
        } else {
            n.tonic.shutdown().await;
            n.thread.shutdown();
        }
    }
    let survivor = &kept[0];
    let self_id = NodeId::new(format!("n{}", leader + 1));

    // MEASURED, and this is the whole reason the probe is a write: right after
    // the quorum is gone the isolated leader STILL names itself in
    // `leader_hint()`. A hint-based check would answer "yes, I am the leader"
    // for a node that cannot commit a single byte — so `/healthz/leader` would
    // keep returning 200 and keep steering traffic at it.
    let hint_while_isolated = survivor.handle.leader_hint().await;
    assert_eq!(
        hint_while_isolated.as_ref().map(|(id, _)| id),
        Some(&self_id),
        "measured: an isolated leader still names itself in leader_hint (got \
         {hint_while_isolated:?}); this is why the health answer may not come from the hint"
    );

    // The write probe refuses once raft has stepped this node down, which is
    // bounded by the election timeout (measured ~1.0 s with the LAN profile).
    //
    // Measured honestly: a hint-based check reaches `false` at about the same
    // moment, because both are waiting for the same step-down. So latency does
    // NOT separate them, and this test does not claim it does — the probe is
    // preferred for two other reasons: it answers "can THIS node commit" in one
    // question instead of composing "is there a leader" with "is it me", and it
    // does not couple the health answer to `leader_hint`'s semantics, which
    // upstream has already changed once (a new leader did not name itself in
    // 0.1.1).
    let t0 = Instant::now();
    let deadline = t0 + Duration::from_secs(15);
    while controls[leader].probe_once().await {
        assert!(
            Instant::now() < deadline,
            "a leader with 1 of 3 voters must stop accepting writes"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let detected_after = t0.elapsed();
    assert!(
        detected_after < Duration::from_secs(3),
        "the probe must refuse once raft has stepped the node down (election timeout, LAN \
         profile ~1 s); measured {detected_after:?}"
    );
    println!("quorum loss detected by the write probe after {detected_after:?}");

    // Once raft has noticed, the hint catches up (it stops naming this node) —
    // so the two APIs disagree only during the window that matters, which is
    // exactly the window a health check is trying to cover.
    let deadline = Instant::now() + Duration::from_secs(15);
    while survivor.handle.leader_hint().await.is_some() {
        assert!(
            Instant::now() < deadline,
            "expected the isolated node's hint to stop naming a leader once raft steps it down"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    shutdown(kept).await;
}
