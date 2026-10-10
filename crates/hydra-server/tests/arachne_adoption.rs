#![cfg(feature = "arachne")]
//! B test group — plan 2026-10-10 §1.2 B1–B5 / §3.2: adoption hardening.
//!
//! `preflight_cluster_id` used to read the handshake key and then BLINDLY `put`
//! it. That opens a TOCTOU window: a stale (or failed) read followed by a
//! leader-accepted overwrite silently replaces a cluster id another member just
//! claimed. §3.2 replaces the put with `without_redirect().cas(NotExists)` and
//! this file pins the contract that produces:
//!
//! * **B1** — two handles in one raft group race DIFFERENT ids → exactly one
//!   wins, the loser gets the mismatch refusal (never a silent overwrite);
//! * **B2** — same id raced → BOTH succeed (the equal-value-is-success rule is
//!   a hydra-side wrapper convention; the assertion is `Ok(())`, not the
//!   library's `Applied`);
//! * **B3** — an unclaimed directory keeps answering `PENDING_ADOPTION` for the
//!   whole 100 ms / 10 s window, then fails with the marker + guidance intact;
//! * **B4** — a follower's adoption is refused WITHOUT being forwarded (the
//!   default `cas` forwards — spike B4 — which is why `without_redirect()` must
//!   stay), and once the leader adopts, the follower passes on a local read;
//! * **B5** — regression: the pre-existing first-contact test and the arachne
//!   gate stay green (run by the gate, named here for the checklist).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use arachne_kv::server::{assemble_cluster, AssembledClusterNode, ClusterConfig};
use arachne_kv::{NodeId, Profile};
use hydra_server::cluster::arachne_node::{
    await_cluster_preflight, preflight_cluster_id, ArachneControl, ADOPTION_GUIDANCE,
    HANDSHAKE_CLUSTER_KEY, PENDING_ADOPTION, PREFLIGHT_DEADLINE,
};

/// Dedicated loopback ports: cargo runs this file's tests concurrently, and a
/// raft member must be dialable at the address its peers were told about.
/// B1/B2 are single-member (a lone voter elects itself, which is all a race
/// needs); B3/B4 need a follower, so they are three-member.
const PORT_B1: u16 = 18951;
const PORT_B2: u16 = 18961;
const PORTS_B3: [u16; 3] = [18971, 18972, 18973];
const PORTS_B4: [u16; 3] = [18981, 18982, 18983];

fn data_dir(tag: &str, i: usize) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hydra-adoption-{tag}-n{}-{}",
        i + 1,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create data dir");
    dir
}

/// A one-member cluster — the fixture the two racing handles share. A lone
/// voter elects itself, so both handles reach the same leader's state machine
/// and raft serialises their proposals, which is the race B1/B2 is about.
async fn start_single(tag: &str, port: u16) -> AssembledClusterNode {
    let id = NodeId::new("solo");
    let addr: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .expect("fixture address");
    let mut cfg = ClusterConfig::member(
        format!("hydra-adoption-{tag}"),
        id.clone(),
        addr,
        data_dir(tag, 0),
        vec![id.clone()],
        HashMap::from([(id, addr)]),
    );
    cfg.profile = Profile::Lan;
    assemble_cluster(cfg).await.expect("assemble member")
}

/// A bare three-member cluster (no ConfigStore/SQLite): B3/B4 only need one
/// follower to exist, so the fixture is raft only.
async fn start_cluster(tag: &str, ports: &[u16; 3]) -> Vec<AssembledClusterNode> {
    let spec = format!(
        "n1=127.0.0.1:{},n2=127.0.0.1:{},n3=127.0.0.1:{}",
        ports[0], ports[1], ports[2]
    );
    let mut nodes = Vec::new();
    for (i, port) in ports.iter().enumerate() {
        let peers = hydra_server::cluster::arachne_node::cluster_peers(
            &spec,
            &format!("n{}", i + 1),
            &format!("127.0.0.1:{port}"),
        )
        .expect("fixture peer table parses");
        let id = NodeId::new(format!("n{}", i + 1));
        let addresses: HashMap<NodeId, SocketAddr> = peers
            .addresses()
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        let mut cfg = ClusterConfig::member(
            format!("hydra-adoption-{tag}"),
            id,
            peers.listen(),
            data_dir(tag, i),
            peers.order().to_vec(),
            addresses,
        );
        cfg.profile = Profile::Lan;
        nodes.push(assemble_cluster(cfg).await.expect("assemble member"));
    }
    nodes
}

/// Leader detection by WRITE PROBE (the established pattern, never
/// `leader_hint()`): exactly one node accepts a `without_redirect().put`.
async fn wait_for_leader(nodes: &[AssembledClusterNode]) -> usize {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut leaders = Vec::new();
        for (i, n) in nodes.iter().enumerate() {
            if n.handle
                .without_redirect()
                .put(b"hydra/adoption/probe", b"1")
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

/// Wait until this single node accepts writes (it has self-elected), so a race
/// started right after cannot be decided by "who saw a leader first".
async fn wait_writable(node: &AssembledClusterNode) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while node
        .handle
        .without_redirect()
        .put(b"hydra/adoption/probe", b"1")
        .await
        .is_err()
    {
        assert!(Instant::now() < deadline, "node never accepted a write");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn shutdown(nodes: Vec<AssembledClusterNode>) {
    for n in nodes {
        n.tonic.shutdown().await;
        n.thread.shutdown();
    }
}

async fn shutdown_one(node: AssembledClusterNode) {
    node.tonic.shutdown().await;
    node.thread.shutdown();
}

/// B1 — two handles, one raft group, two DIFFERENT cluster ids: exactly one
/// adoption wins and the other is refused with the mismatch message. The lost
/// race must never be papered over with a second write — that silent overwrite
/// is precisely what `cas(NotExists)` closes (§3.2, TOCTOU).
///
/// The two racing `preflight_cluster_id` calls share the window the hard way:
/// both typically read "absent" and both propose, so the loser's refusal comes
/// from the NEW `NotApplied` branch; if the interleaving instead lets the loser
/// observe the winner in step 1, the shared `cluster_mismatch_error` makes the
/// refusal byte-identical — either way exactly one `Ok` and one mismatch.
///
/// Falsification: revert step 2 to a blind `put` and BOTH calls return `Ok`
/// (the overwrite succeeds), so the exactly-one-winner assertion fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_adoptions_of_different_ids_let_exactly_one_win() {
    let node = start_single("b1", PORT_B1).await;
    wait_writable(&node).await;

    // Two handles over the same raft group — the fixture §7 specifies for the race.
    let a = ArachneControl::for_tests(node.handle.clone(), NodeId::new("solo"));
    let b = ArachneControl::for_tests(node.handle.clone(), NodeId::new("solo"));

    let (ra, rb) = tokio::join!(
        preflight_cluster_id(&a, "hydra-b1-a"),
        preflight_cluster_id(&b, "hydra-b1-b"),
    );

    let (winner, loser, loser_msg) = match (&ra, &rb) {
        (Ok(()), Err(e)) => ("hydra-b1-a", "hydra-b1-b", e),
        (Err(e), Ok(())) => ("hydra-b1-b", "hydra-b1-a", e),
        (Ok(()), Ok(())) => panic!(
            "two DIFFERENT cluster ids were both adopted — the loser's write silently \
             overwrote the winner: {ra:?} / {rb:?}"
        ),
        (Err(e1), Err(e2)) => {
            panic!("exactly one adoption must win, but neither did: {e1:?} / {e2:?}")
        }
    };
    assert!(
        loser_msg.contains("belongs to a different cluster"),
        "the loser must get the mismatch refusal, got: {loser_msg}"
    );
    assert!(
        loser_msg.contains(winner) && loser_msg.contains(loser),
        "the refusal must name BOTH sides of the disagreement (recorded {winner}, configured \
         {loser}), got: {loser_msg}"
    );

    // The key holds the WINNER's id — one claim, not a last-writer-wins mush.
    let stored = node
        .handle
        .get_stale(HANDSHAKE_CLUSTER_KEY)
        .await
        .expect("stale read")
        .expect("someone adopted");
    assert_eq!(
        stored.as_slice(),
        winner.as_bytes(),
        "the directory must record exactly the winning cluster id"
    );

    shutdown_one(node).await;
}

/// B2 — two handles race the SAME cluster id: BOTH must come back `Ok(())`.
///
/// The library's verdict for the loser is `NotApplied` (the `NotExists`
/// predicate misses an existing key — there is no library-level "equal value
/// counts"), so success here is entirely the hydra-side wrapper convention in
/// §3.2: compare `current_value` with what we wanted and accept equality. The
/// assertion is deliberately the OUTER contract — `Ok`, not `Applied`.
///
/// Falsification: drop the equality check and treat every `NotApplied` as a
/// mismatch, and the second racer fails (restarts of an adopted directory would
/// break); return `Ok` for `Applied` only, same failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_adoptions_of_the_same_id_both_succeed() {
    let node = start_single("b2", PORT_B2).await;
    wait_writable(&node).await;

    let a = ArachneControl::for_tests(node.handle.clone(), NodeId::new("solo"));
    let b = ArachneControl::for_tests(node.handle.clone(), NodeId::new("solo"));

    let (ra, rb) = tokio::join!(
        preflight_cluster_id(&a, "hydra-b2-same"),
        preflight_cluster_id(&b, "hydra-b2-same"),
    );
    assert!(
        ra.is_ok() && rb.is_ok(),
        "the SAME cluster id must be accepted by BOTH racers (one Applied, one \
         NotApplied-but-equal — the wrapper treats equality as success): {ra:?} / {rb:?}"
    );

    let stored = node
        .handle
        .get_stale(HANDSHAKE_CLUSTER_KEY)
        .await
        .expect("stale read")
        .expect("adopted");
    assert_eq!(stored.as_slice(), b"hydra-b2-same");

    shutdown_one(node).await;
}

/// B3 — the cold-start window is unchanged: while nobody has claimed the
/// directory, every ask answers `PENDING_ADOPTION` (retry), and the deadline
/// loop keeps its 100 ms step, its 10 s budget and its marker+guidance error.
///
/// The fixture is a follower: without a leader-adopted key it can never get a
/// verdict through `without_redirect()`, which is exactly the "no member has
/// adopted yet" window a cold cluster starts in.
///
/// Falsification: map the refusal errors to a hard `Err` and the first single
/// ask fails instead of returning the marker; shorten/alter the loop and the
/// elapsed/deadline assertions catch it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unclaimed_directory_stays_pending_until_the_deadline() {
    let nodes = start_cluster("b3", &PORTS_B3).await;
    let leader = wait_for_leader(&nodes).await;
    let follower = (leader + 1) % 3;
    let ctl = ArachneControl::for_tests(
        nodes[follower].handle.clone(),
        NodeId::new(format!("n{}", follower + 1)),
    );

    // A single ask in the window: RETRY marker, never a verdict either way.
    let one = preflight_cluster_id(&ctl, "hydra-b3").await;
    let msg = one.expect_err("an unclaimed directory must not answer Ok on a follower");
    assert!(
        msg.contains(PENDING_ADOPTION),
        "the cold-start answer must be the retry marker {PENDING_ADOPTION:?}, got: {msg}"
    );
    assert!(
        !msg.contains("different cluster"),
        "pending must never read as a mismatch — the two need opposite reactions \
         (retry vs refuse), got: {msg}"
    );

    // The full loop: keeps asking until PREFLIGHT_DEADLINE, then fails with the
    // marker AND the action guidance the operator depends on.
    let started = Instant::now();
    let err = await_cluster_preflight(&ctl, "hydra-b3")
        .await
        .expect_err("nobody adopts this directory within the deadline");
    let elapsed = started.elapsed();
    assert!(
        err.contains(PENDING_ADOPTION),
        "the deadline error must carry the pending marker, got: {err}"
    );
    assert!(
        err.contains(&format!("within {PREFLIGHT_DEADLINE:?}")),
        "the deadline error must state the budget, got: {err}"
    );
    assert!(
        err.contains("start the members together") && err.contains(ADOPTION_GUIDANCE),
        "the deadline error must carry the adoption guidance, got: {err}"
    );
    assert!(
        elapsed >= PREFLIGHT_DEADLINE,
        "the loop must keep asking for the full deadline (asked for {elapsed:?})"
    );
    assert!(
        elapsed < PREFLIGHT_DEADLINE + Duration::from_secs(10),
        "the 100 ms step must not turn the deadline into a hang (took {elapsed:?})"
    );

    // Nothing was written behind the retries' back either.
    assert!(
        nodes[leader]
            .handle
            .get_stale(HANDSHAKE_CLUSTER_KEY)
            .await
            .expect("stale read")
            .is_none(),
        "a pending adoption must leave the directory unclaimed"
    );

    shutdown(nodes).await;
}

/// B4 — a follower's adoption is refused LOCALLY and never forwarded, and the
/// refusal is the retry marker (so preflight keeps polling instead of dying).
/// The default `cas` WOULD forward — measured in spike B4 — which is exactly
/// why §3.2 keeps `without_redirect()`: forwarding would let a follower claim
/// the directory and end "only a leader adopts".
///
/// Second half: once the LEADER adopts, the follower passes on its own LOCAL
/// read — no leadership required — which is the "a restarting member needs no
/// majority" story the adoption guidance tells operators.
///
/// Falsification: drop `without_redirect()` and the first assertion fails: the
/// follower's call would come back `Ok(())` having adopted through the leader.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_followers_adoption_is_refused_locally_and_never_forwarded() {
    let nodes = start_cluster("b4", &PORTS_B4).await;
    let leader = wait_for_leader(&nodes).await;
    let follower = (leader + 1) % 3;
    let f_ctl = ArachneControl::for_tests(
        nodes[follower].handle.clone(),
        NodeId::new(format!("n{}", follower + 1)),
    );

    // The follower asks first — with the leader RIGHT THERE able to take the
    // write. If the cas were forwarded, this would succeed and adopt.
    let r = preflight_cluster_id(&f_ctl, "hydra-b4").await;
    let msg = r.expect_err("a follower must not adopt, forwarded or not");
    assert!(
        msg.contains(PENDING_ADOPTION),
        "the follower's refusal must be the retry marker so preflight keeps polling, got: {msg}"
    );

    // …and the group wrote NOTHING: no trace of the attempt on any member.
    for (i, n) in nodes.iter().enumerate() {
        assert!(
            n.handle
                .get_stale(HANDSHAKE_CLUSTER_KEY)
                .await
                .expect("stale read")
                .is_none(),
            "node n{} must not hold a cluster id: the follower's adoption was not forwarded",
            i + 1
        );
    }

    // The leader adopts normally…
    let l_ctl = ArachneControl::for_tests(
        nodes[leader].handle.clone(),
        NodeId::new(format!("n{}", leader + 1)),
    );
    assert!(
        preflight_cluster_id(&l_ctl, "hydra-b4").await.is_ok(),
        "the leader must be able to claim the directory"
    );

    // …and the follower now passes on its LOCAL read once replication delivers
    // the key — this is the whole reason step 1 is a read before any write.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match preflight_cluster_id(&f_ctl, "hydra-b4").await {
            Ok(()) => break,
            Err(e) => {
                assert!(
                    e.contains(PENDING_ADOPTION),
                    "after the leader adopts, the follower may only still be PENDING (its \
                     local copy has not landed yet), never a mismatch: {e}"
                );
                assert!(
                    Instant::now() < deadline,
                    "the follower never saw the leader's adoption within 5 s: {e}"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    shutdown(nodes).await;
}
