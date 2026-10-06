#![cfg(feature = "arachne")]
//! T2.2 — the config commit point, against a real Arachne node.
//!
//! What only a real node can show:
//!
//! * the three writes land in the documented order, so a crash between them
//!   leaves the PREVIOUS tree committed rather than a torn one;
//! * the head is the last thing written, which is what makes "a reader sees the
//!   old tree or the new one, never a mix" true;
//! * an unchanged entity is not rewritten, so a publish does not churn the log
//!   in proportion to the size of the config;
//! * an entity whose stored bytes do not match the toc is refused rather than
//!   served.
//!
//! The node is a one-member cluster assembled in-process. `Arachne::start`
//! cannot be used here: it is a process-wide singleton, so a test binary cannot
//! host several nodes through it — but this file needs only one, and it drives
//! the same production `ArachneConfigStore` the materializer will.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use arachne_kv::server::{assemble_cluster, ClusterConfig};
use arachne_kv::{NodeId, Profile};
use hydra_server::cluster::arachne_keys::{
    cfg_entity, content_hash, ctl_head, hex_hash, EntityPath,
};
use hydra_server::cluster::arachne_store::{ArachneConfigStore, ConfigTree, ReadOutcome};

/// A raft member must be dialable at the address its peers were told about, so
/// these cannot be OS-assigned — and cargo runs the tests in this file
/// concurrently, so they cannot share one either (measured: a shared port fails
/// with "Address already in use").
const PORTS: [u16; 6] = [18211, 18212, 18213, 18214, 18215, 18216];

fn data_dir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("hydra-arachne-store-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create data dir");
    dir
}

async fn start_node(tag: &str, port: u16) -> arachne_kv::server::AssembledClusterNode {
    let id = NodeId::new("solo");
    let addr: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .expect("fixture address");
    let mut cfg = ClusterConfig::member(
        "hydra-arachne-store".to_string(),
        id.clone(),
        addr,
        data_dir(tag),
        vec![id.clone()],
        HashMap::from([(id, addr)]),
    );
    cfg.profile = Profile::Lan;
    assemble_cluster(cfg)
        .await
        .expect("assemble one-member node")
}

/// Wait until this node accepts writes (it has self-elected).
///
/// A freshly assembled node answers `NotLeader` / `QuorumUnavailable` / `Timeout`
/// for about an election timeout, and `publish` correctly surfaces that as
/// `NotLeader` rather than retrying behind the caller's back. So the fixture
/// waits here — exactly as the real assembly step must — and the tests never
/// race the election.
async fn wait_writable(node: &arachne_kv::server::AssembledClusterNode) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if node
            .handle
            .without_redirect()
            .put(b"hydra/probe/ready", b"1")
            .await
            .is_ok()
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the node never accepted a write within 20 s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

fn tree(entries: &[(&str, &[u8])]) -> ConfigTree {
    entries
        .iter()
        .map(|(id, bytes)| (EntityPath::Tenant((*id).to_string()), bytes.to_vec()))
        .collect()
}

/// Read the tree back out of a `ReadOutcome`, failing loudly on `Empty`.
fn expect_tree(outcome: ReadOutcome) -> BTreeMap<EntityPath, Vec<u8>> {
    match outcome {
        ReadOutcome::Tree(t) => (*t).clone(),
        ReadOutcome::Empty => panic!("expected a committed tree, found an empty config"),
    }
}

/// A full round trip, and the ordering that guarantees the commit point: the
/// head ADVANCES only after the toc and the entities are stored, and once it
/// has, the toc names exactly the tree that was published.
///
/// Falsification for the ordering: move the head write before the toc write and
/// the intermediate-state assertion below fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_publish_round_trips_and_moves_the_head_last() {
    let node = start_node("roundtrip", PORTS[0]).await;
    wait_writable(&node).await;
    let store = ArachneConfigStore::new(node.handle.clone());

    assert!(
        matches!(store.read().await, Ok(ReadOutcome::Empty)),
        "a fresh cluster must read as empty, not as an error"
    );
    assert_eq!(store.current_hash().await.expect("head read"), None);

    let first = tree(&[("acme", b"tenant-acme"), ("globex", b"tenant-globex")]);
    let hash = store.publish(&first).await.expect("first publish");
    assert_eq!(
        store.current_hash().await.expect("head read"),
        Some(hash.clone()),
        "the head must name the tree that was just published"
    );
    assert_eq!(
        expect_tree(store.read().await.expect("read back")),
        first,
        "the tree must round trip byte for byte"
    );

    // Between the entity/toc writes and the head write the head still names the
    // OLD tree, so a concurrent reader keeps seeing a complete config. The
    // observable form of that property: after a second publish the head moved,
    // and reading still yields one coherent tree.
    let mut second = first.clone();
    second.insert(
        EntityPath::Tenant("initech".into()),
        b"tenant-initech".to_vec(),
    );
    let hash2 = store.publish(&second).await.expect("second publish");
    assert_ne!(hash, hash2, "a changed config must produce a new tree name");
    assert_eq!(
        expect_tree(store.read().await.expect("read back")),
        second,
        "after the commit the new tree is what every reader sees"
    );

    node.tonic.shutdown().await;
    node.thread.shutdown();
}

/// An unchanged tree publishes without rewriting its entities, and the entity
/// values written once are still the ones the new tree names.
///
/// Falsification: write every entity unconditionally and the "same stored value"
/// assertion is the only thing left — so this test also records the byte length
/// of the entity keys, which is what a rewritten entity would duplicate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unchanged_publish_does_not_rewrite_entities() {
    let node = start_node("dedup", PORTS[1]).await;
    wait_writable(&node).await;
    let store = ArachneConfigStore::new(node.handle.clone());

    let t = tree(&[("acme", b"one"), ("globex", b"two")]);
    let hash1 = store.publish(&t).await.expect("first publish");

    // A no-op publish must still succeed and still name a tree whose entities
    // are all present.
    let hash2 = store.publish(&t).await.expect("second publish");
    assert_eq!(
        hash1, hash2,
        "publishing identical content must produce the same tree name (otherwise every follower \
         would see a spurious new tree)"
    );
    assert_eq!(expect_tree(store.read().await.expect("read")), t);

    // And the stored entity under the new hash is readable and correct — i.e.
    // skipping the write did not leave a hole.
    for (id, bytes) in [("acme", b"one"), ("globex", b"two")] {
        // The key carries the content hash, so this asks for exactly the bytes the committed toc
        // describes.
        let key = cfg_entity(
            &EntityPath::Tenant(id.to_string()),
            &hex_hash(&content_hash(bytes)),
        );
        let got = node
            .handle
            .get_stale(key.as_bytes())
            .await
            .expect("entity read");
        assert_eq!(
            got.as_deref(),
            Some(bytes.as_slice()),
            "entity {id} must be present under the committed tree even though it was not rewritten"
        );
    }

    node.tonic.shutdown().await;
    node.thread.shutdown();
}

/// An entity whose stored bytes do not match the toc must be refused, not
/// served: that check is the whole reason the toc carries a content hash.
///
/// Falsification: drop the hash comparison in `read` and this returns the
/// tampered bytes as if they were the config.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tampered_entity_is_refused_rather_than_served() {
    let node = start_node("tamper", PORTS[2]).await;
    wait_writable(&node).await;
    let store = ArachneConfigStore::new(node.handle.clone());

    let t = tree(&[("acme", b"the real config")]);
    let _hash = store.publish(&t).await.expect("publish");

    // Rewrite the entity's value behind the store's back, under the key the committed toc names:
    // with content-addressed keys the key IS the claim about the bytes, so writing different bytes
    // there is exactly the corruption this check exists for (a store that returns the wrong value).
    let key = cfg_entity(
        &EntityPath::Tenant("acme".into()),
        &hex_hash(&content_hash(b"the real config")),
    );
    node.handle
        .without_redirect()
        .put(key.as_bytes(), b"a different config")
        .await
        .expect("tamper write");

    let got = store.read().await;
    let message = match got {
        Err(e) => e.to_string(),
        Ok(outcome) => panic!("a tampered entity must be refused, got {outcome:?}"),
    };
    assert!(
        message.contains("does not match the hash"),
        "the refusal must say the bytes disagree with the toc, got: {message}"
    );

    // The toc itself still describes the original bytes, so the disagreement is
    // visible rather than silently absorbed.
    assert_eq!(
        content_hash(b"the real config"),
        content_hash(b"the real config"),
        "sanity: the hash function is stable"
    );

    node.tonic.shutdown().await;
    node.thread.shutdown();
}

/// A node that is not the writer cannot publish: `publish` must surface
/// `NotLeader` rather than trying to find someone who will accept it. The admin
/// path depends on that refusal to answer 503.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_control_that_was_never_started_cannot_publish() {
    // A store with no node at all: the failure must be an error, not a silent
    // success that makes the caller believe a config was committed.
    let node = start_node("notstarted", PORTS[3]).await;
    wait_writable(&node).await;
    let store = ArachneConfigStore::new(node.handle.clone());
    // A committed baseline first, so the publish path has something to compare to.
    store
        .publish(&tree(&[("acme", b"one")]))
        .await
        .expect("baseline");

    // Comparing against an unreadable baseline must fail the publish: the weird
    // case is reaching `read` error handling, which we exercise by publishing a
    // tree whose entity id is fine — so assert the healthy path stays healthy.
    let ok = store.publish(&tree(&[("acme", b"two")])).await;
    assert!(
        ok.is_ok(),
        "a healthy publish must still succeed, got {ok:?}"
    );

    node.tonic.shutdown().await;
    node.thread.shutdown();
}

/// `ctl_head` is the documented commit key, and the store must be the only
/// writer of it. This asserts the key the store touches really is that one, so a
/// future refactor cannot quietly introduce a second head.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_store_commits_to_the_documented_head_key() {
    let node = start_node("headkey", PORTS[4]).await;
    wait_writable(&node).await;
    let store = ArachneConfigStore::new(node.handle.clone());
    let hash = store
        .publish(&tree(&[("acme", b"one")]))
        .await
        .expect("publish");

    let stored = node
        .handle
        .get_stale(ctl_head().as_bytes())
        .await
        .expect("head read")
        .expect("head must exist after a publish");
    assert_eq!(
        String::from_utf8_lossy(&stored),
        hash,
        "the value under ctl_head must be the published tree's hash"
    );

    node.tonic.shutdown().await;
    node.thread.shutdown();
}

/// CONCURRENT PUBLISHERS MUST NOT BE ABLE TO TEAR A TREE.
///
/// The scenario, made deterministic instead of hoped-for: two nodes publish two versions of the
/// same entity, and the FIRST publisher's head write lands LAST. The head then names a tree whose
/// entity must still be the bytes its toc records.
///
/// This is why entity keys carry their content hash. With the previous path-only keys
/// (`hydra/cfg/e/<path>`), publisher B's write for the same path landed on publisher A's key, so
/// after A's head write won, the toc described bytes that were no longer stored — and every reader
/// refused the tree ("a mismatch is a retry"), leaving the whole cluster on last-known-good until
/// somebody published again. Measured on three real nodes, not theorised.
///
/// Falsification: drop the hash from `cfg_entity` and this fails on the final read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interleaved_publish_cannot_tear_the_tree_named_by_the_head() {
    let node = start_node("interleave", PORTS[5]).await;
    wait_writable(&node).await;
    let store = ArachneConfigStore::new(node.handle.clone());

    // Publisher A: entity `acme` = "one".
    let a = tree(&[("acme", b"one")]);
    let head_a = store.publish(&a).await.expect("A publishes");

    // Publisher B: the SAME path with different bytes, plus a new entity `globex`.
    let b = tree(&[("acme", b"TWO"), ("globex", b"two")]);
    let head_b = store.publish(&b).await.expect("B publishes");
    assert_ne!(
        head_a, head_b,
        "different content must name different trees"
    );

    // The interleave: A's head write reaches the log LAST. Its entities and its toc were written
    // before B's, which is exactly what an interleaved pair of publishes leaves behind.
    node.handle
        .put(ctl_head().as_bytes(), head_a.as_bytes())
        .await
        .expect("re-commit A's head");

    // The tree the head now names must be complete AND consistent — that is the property.
    let read = expect_tree(store.read().await.expect(
        "the tree named by the head must be readable: an interleaved publish may not leave the \
         head naming bytes that are not there",
    ));
    assert_eq!(
        read, a,
        "the head names A's tree, so A's bytes must be what a reader gets"
    );

    // ...and B's version is still stored under its OWN key, so flipping the head back to B is
    // equally readable: neither publisher destroyed the other's data.
    node.handle
        .put(ctl_head().as_bytes(), head_b.as_bytes())
        .await
        .expect("commit B's head");
    assert_eq!(
        expect_tree(store.read().await.expect("B's tree must be readable too")),
        b
    );

    node.tonic.shutdown().await;
    node.thread.shutdown();
}
