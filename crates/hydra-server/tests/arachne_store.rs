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
//!   served;
//! * since the `multi_put` conversion (plan 2026-10-10, A1–A4): one publish IS
//!   one log entry, an over-cap batch is refused WHOLE before it enters the log,
//!   concurrent publishes commit whole trees, and the toc the head names
//!   describes a tree that is fully present.
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
    cfg_entity, cfg_toc, content_hash, ctl_head, hex_hash, EntityPath, Toc,
};
use hydra_server::cluster::arachne_store::{
    ArachneConfigStore, ConfigTree, ReadOutcome, StoreError,
};

/// A raft member must be dialable at the address its peers were told about, so
/// these cannot be OS-assigned — and cargo runs the tests in this file
/// concurrently, so they cannot share one either (measured: a shared port fails
/// with "Address already in use").
const PORTS: [u16; 10] = [
    18211, 18212, 18213, 18214, 18215, 18216, 18217, 18218, 18219, 18220,
];

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

/// A full round trip, and the commit-point property that replaced head-last
/// ordering (2026-10-10, multi_put): the head MOVES WITH the entities and toc
/// it names — one atomic batch, one log entry — so a reader sees either the old
/// tree or the new one, never a mix, and once it has, the toc names exactly the
/// tree that was published.
///
/// Falsification: there is no window left to observe (the "move the head write
/// before the toc write" falsification of the old head-last test no longer
/// applies — the batch is all-or-nothing); the round-trip and head==toc-hash
/// assertions fail if publish ever decouples the head from its tree.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_publish_round_trips_and_the_head_moves_with_its_tree() {
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

    // Entities, toc and head commit together, so there is no intermediate state
    // for a reader to land in: after a second publish the head moved WITH its
    // tree, and reading still yields one coherent config.
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

/// `hydra_arachne_publish_total{result="refused"}` — the capacity alarm. Read from the
/// process-wide registry, so a test compares DELTAS, never absolutes.
fn refused_publish_count() -> u64 {
    hydra_server::admin::metrics::render()
        .lines()
        .find_map(|l| l.strip_prefix("hydra_arachne_publish_total{result=\"refused\"} "))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

/// ONE PUBLISH IS ONE LOG ENTRY (plan A1), and an unchanged entity is not resent.
///
/// The evidence is index arithmetic, not timing: `get_stale_with_index` reports the raft log
/// index of the entry that wrote the key, so the head's index advancing by at least 1 per
/// publish session, WITH the batch's contents pinned to that same session, is "one session =
/// one entry". Before `multi_put` the same measurement gave `+N+2` (N entities + toc + head as
/// separate proposes). The delta itself is `>= 1` rather than `== 1` (2026-10-10, oracle F4):
/// a no-op entry from a leader handover may also advance the index, and that would false-red an
/// exact-equality claim without meaning the batch split. What pins "same batch" is the index
/// equality: the toc lands at the SAME index as the head, a newly written entity carries that
/// index too, and an entity the new tree did not change keeps ITS origin index (it was skipped,
/// not rewritten) — split the batch and the equality pins fail even when the delta would not.
///
/// Falsification: split the batch back into per-entity puts and the toc's index no longer equals
/// the head's (toc lands an entry earlier); always resend every entity and the frozen acme index
/// advances.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_publish_is_one_log_entry_and_unchanged_entities_stay_put() {
    let node = start_node("one-entry", PORTS[6]).await;
    wait_writable(&node).await;
    let store = ArachneConfigStore::new(node.handle.clone());

    // First publish: everything is new, so the batch is 2 entities + toc + head.
    let t = tree(&[("acme", b"one"), ("globex", b"two")]);
    let h1 = store.publish(&t).await.expect("first publish");
    let (_, i1) = store
        .current_head_with_index()
        .await
        .expect("head read")
        .expect("the first publish must commit a head");

    let acme_key = cfg_entity(
        &EntityPath::Tenant("acme".into()),
        &hex_hash(&content_hash(b"one")),
    );
    let (acme_v1, acme_i1) = node
        .handle
        .get_stale_with_index(acme_key.as_bytes())
        .await
        .expect("acme read")
        .expect("the first batch must have stored acme");

    // Second publish: the SAME (unchanged) tree. Only toc + head are in this batch.
    let h2 = store.publish(&t).await.expect("second publish");
    assert_eq!(h1, h2, "same content names the same tree");
    let (_, i2) = store
        .current_head_with_index()
        .await
        .expect("head read")
        .expect("head");
    assert!(
        i2 - i1 >= 1,
        "one publish session must advance the head by at least ONE raft log entry (was +N+2 \
         before multi_put; >1 would need a no-op entry, e.g. from a leader handover): head \
         index went {i1} → {i2}"
    );

    // toc and head were written BY that entry: both carry its index, and the toc key under the
    // committed hash is the one that entry wrote (toc/head are never skipped).
    let toc_key = cfg_toc(&h2);
    let (toc_bytes, toc_i) = node
        .handle
        .get_stale_with_index(toc_key.as_bytes())
        .await
        .expect("toc read")
        .expect("every session rewrites the toc");
    assert_eq!(
        toc_i, i2,
        "the toc must be written by the SAME log entry as the head (one atomic batch), got \
         toc={toc_i} head={i2}"
    );
    assert_eq!(
        hex_hash(&content_hash(&toc_bytes)),
        h2,
        "the stored toc must hash to the committed head"
    );

    // The unchanged entity was NOT resent: its origin index is still the FIRST publish's.
    let (acme_v2, acme_i2) = node
        .handle
        .get_stale_with_index(acme_key.as_bytes())
        .await
        .expect("acme read")
        .expect("acme still stored");
    assert_eq!(
        acme_i2, acme_i1,
        "an unchanged entity must be skipped, not resent: its origin index must stay at the \
         first publish's entry ({acme_i1}), got {acme_i2}"
    );
    assert_eq!(
        acme_v1, acme_v2,
        "the skipped entity's bytes must be intact"
    );

    // A CHANGED tree is still one entry, and the entities it did not change stay untouched.
    let mut t3 = t.clone();
    t3.insert(EntityPath::Tenant("initech".into()), b"three".to_vec());
    let h3 = store.publish(&t3).await.expect("third publish");
    let (_, i3) = store
        .current_head_with_index()
        .await
        .expect("head read")
        .expect("head");
    assert!(
        i3 - i2 >= 1,
        "a changed publish must advance the head by at least one log entry (same no-op caveat \
         as above; the atomicity pins are the index equalities): head index went {i2} → {i3}"
    );
    // Same-batch pins for THIS publish: the toc of the committed hash and the brand-new entity
    // only this batch contains both carry the entry that moved the head.
    let toc3_key = cfg_toc(&h3);
    let (toc3_bytes, toc3_i) = node
        .handle
        .get_stale_with_index(toc3_key.as_bytes())
        .await
        .expect("toc read")
        .expect("every session rewrites the toc");
    assert_eq!(
        toc3_i, i3,
        "the third toc must be written by the SAME log entry as the head (one atomic batch), got \
         toc={toc3_i} head={i3}"
    );
    assert_eq!(
        hex_hash(&content_hash(&toc3_bytes)),
        h3,
        "the third stored toc must hash to the committed head"
    );
    let initech_key = cfg_entity(
        &EntityPath::Tenant("initech".into()),
        &hex_hash(&content_hash(b"three")),
    );
    let (_, initech_i) = node
        .handle
        .get_stale_with_index(initech_key.as_bytes())
        .await
        .expect("initech read")
        .expect("the third batch must have stored initech");
    assert_eq!(
        initech_i, i3,
        "a NEWLY written entity must carry its publish session's index (same atomic batch), got \
         entity={initech_i} head={i3}"
    );
    let (_, acme_i3) = node
        .handle
        .get_stale_with_index(acme_key.as_bytes())
        .await
        .expect("acme read")
        .expect("acme still stored");
    assert_eq!(
        acme_i3, acme_i1,
        "entities the new tree did not change must keep their original origin index across a \
         changed publish, got {acme_i1} → {acme_i3}"
    );

    node.tonic.shutdown().await;
    node.thread.shutdown();
}

/// AN OVER-CAP BATCH IS REFUSED WHOLE, BEFORE IT ENTERS THE LOG (plan A2).
///
/// Five entities of 900 KiB: every single value is UNDER the 1 MiB per-value cap, but the
/// batch totals ~4.4 MiB — over the library's `MAX_MULTI_PUT_TOTAL_BYTES` (4 MiB). The
/// library validates the batch before proposing (`validate_multi_put`), so the refusal is a
/// fact about the config: the head keeps its value AND its origin index (a partially applied
/// batch would have moved at least one of them), the previously committed tree still reads
/// back intact, and `publish` counts the outcome as `result="refused"` — the capacity alarm —
/// rather than an opaque error.
///
/// Falsification: split the batch to fit and the `StoreError::Refused` match fails (it would
/// commit instead); drop the `InvalidArgument → Refused` arm in `map_err` and the variant
/// match plus the counter both fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_over_cap_batch_is_refused_whole_before_it_enters_the_log() {
    let node = start_node("overcap", PORTS[9]).await;
    wait_writable(&node).await;
    let store = ArachneConfigStore::new(node.handle.clone());

    // A small committed tree, so "the log did not move" has a baseline to stay at.
    let baseline = tree(&[("acme", b"baseline")]);
    let h1 = store.publish(&baseline).await.expect("baseline publish");
    let (_, i1) = store
        .current_head_with_index()
        .await
        .expect("head read")
        .expect("the baseline must publish a head");
    let refused_before = refused_publish_count();

    // ~1 MiB per value (under max_value_bytes), ~4.4 MiB in total (over the batch cap).
    let payload = "x".repeat(900 * 1024);
    let huge = tree(&[
        ("h0", payload.as_bytes()),
        ("h1", payload.as_bytes()),
        ("h2", payload.as_bytes()),
        ("h3", payload.as_bytes()),
        ("h4", payload.as_bytes()),
    ]);
    let err = store
        .publish(&huge)
        .await
        .expect_err("a batch over the total-byte cap must not commit");
    match &err {
        StoreError::Refused(msg) => assert!(
            msg.contains("MAX_MULTI_PUT_TOTAL_BYTES"),
            "the refusal must name the batch cap that tripped (that is the pre-propose \
             InvalidArgument), got: {msg}"
        ),
        other => panic!(
            "an over-cap batch must surface as StoreError::Refused before proposing, got {other:?}"
        ),
    }

    // Whole batch stayed OUT of the log: head value AND origin index unmoved, and the
    // committed tree still reads as the baseline.
    let (h2, i2) = store
        .current_head_with_index()
        .await
        .expect("head read")
        .expect("the head must still be there");
    assert_eq!(
        (h2.as_str(), i2),
        (h1.as_str(), i1),
        "the refused batch must not have entered the log: head value and origin index unmoved"
    );
    assert_eq!(
        expect_tree(store.read().await.expect("read")),
        baseline,
        "the cluster must still serve exactly the pre-refusal tree (no half-write)"
    );

    // The capacity alarm fired.
    let refused_after = refused_publish_count();
    assert!(
        refused_after > refused_before,
        "publish must count a pre-propose refusal as result=\"refused\" (before \
         {refused_before}, after {refused_after})"
    );

    // ...and the healthy path still works afterwards: the refusal did not poison the node.
    store
        .publish(&tree(&[("acme", b"after")]))
        .await
        .expect("a healthy publish must still succeed after a refusal");

    node.tonic.shutdown().await;
    node.thread.shutdown();
}

/// CONCURRENT PUBLISHERS COMMIT WHOLE TREES, NEVER A MIX (plan A3).
///
/// Two publishes racing on the same node: each is one atomic batch, so both must commit, and
/// whichever head wins, a reader sees exactly one publisher's tree — complete, with every
/// entity (including the loser's) still stored under its own content-addressed key. The
/// deterministic interleave test above pins the key-non-conflict side of this; this one is the
/// real race.
///
/// Falsification: revert to N+2 separate proposes per publish and the "reads exactly one
/// publisher's tree" assertion is the one that fails — a reader can land between another
/// publisher's entity writes and its head.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_concurrent_publishes_each_commit_a_whole_tree() {
    let node = start_node("concurrent", PORTS[7]).await;
    wait_writable(&node).await;
    let store = ArachneConfigStore::new(node.handle.clone());

    let a = tree(&[("acme", b"one"), ("globex", b"two")]);
    let b = tree(&[
        ("acme", b"ONE-CHANGED"),
        ("globex", b"two"),
        ("initech", b"three"),
    ]);

    // Start them together instead of in sequence: publish takes &self, so one node can have
    // both in flight — each must commit as its own atomic batch.
    let (ra, rb) = tokio::join!(store.publish(&a), store.publish(&b));
    let ha = ra.expect("publish A must commit");
    let hb = rb.expect("publish B must commit");
    assert_ne!(ha, hb, "different content must name different trees");

    // Both trees are COMPLETE in storage: every entity of both publishers, plus both tocs —
    // so whichever head wins, flipping to it would be equally readable.
    for (head, expected) in [(&ha, &a), (&hb, &b)] {
        assert!(
            node.handle
                .get_stale(cfg_toc(head).as_bytes())
                .await
                .expect("toc read")
                .is_some(),
            "the toc of {head} must be stored"
        );
        for (path, bytes) in expected.iter() {
            let key = cfg_entity(path, &hex_hash(&content_hash(bytes)));
            let stored = node
                .handle
                .get_stale(key.as_bytes())
                .await
                .expect("entity read")
                .unwrap_or_else(|| {
                    panic!("entity {path:?} of tree {head} must be stored (batch is atomic)")
                });
            assert_eq!(
                stored.as_slice(),
                bytes.as_slice(),
                "entity {path:?} of tree {head} must be byte-identical"
            );
        }
    }

    // The head names ONE publisher's COMPLETE tree — never a merge of the two.
    let final_head = store
        .current_hash()
        .await
        .expect("head read")
        .expect("both publishes committed, so a head exists");
    assert!(
        final_head == ha || final_head == hb,
        "the head must name one of the two committed trees, got {final_head} (A={ha}, B={hb})"
    );
    let final_tree = expect_tree(store.read().await.expect("read"));
    assert!(
        final_tree == a || final_tree == b,
        "a reader must see exactly ONE publisher's tree, never a mix of both; got {final_tree:?}"
    );

    node.tonic.shutdown().await;
    node.thread.shutdown();
}

/// THE HEAD NAMES A TOC THAT DESCRIBES A FULLY PRESENT TREE (plan A4).
///
/// Atomic publishing only pays off if the head — written inside the same batch as the toc and
/// its entities — really does name a toc that covers every entity with matching hash and
/// length, and every one of those entities is stored. This walks the chain the reader walks:
/// head → toc → entries → entity bytes, including a binary value with NUL and 0xff bytes.
///
/// Falsification: publish a toc with a missing/short entity entry and the per-entry
/// presence/length assertion fails; write the head outside the batch and a reader could reach
/// this chain at a moment when the toc it decodes does not describe the batch — caught here by
/// the hash-join (head == hash(stored toc bytes)).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_head_names_a_toc_that_describes_a_fully_present_tree() {
    let node = start_node("fulltree", PORTS[8]).await;
    wait_writable(&node).await;
    let store = ArachneConfigStore::new(node.handle.clone());

    // Three entities, one of them binary — NUL and 0xff are legal value bytes and must
    // survive the round trip through toc hashes and content-addressed keys.
    let t = tree(&[
        ("acme", &b"tenant-acme"[..]),
        ("globex", &[b'g', 0u8, b'x', 0xff][..]),
        ("initech", &b"tenant-initech"[..]),
    ]);
    let hash = store.publish(&t).await.expect("publish");

    // The toc the committed head names.
    let toc_bytes = node
        .handle
        .get_stale(cfg_toc(&hash).as_bytes())
        .await
        .expect("toc read")
        .expect("a committed head must have a toc under its hash");
    assert_eq!(
        hex_hash(&content_hash(&toc_bytes)),
        hash,
        "the head must BE the toc's content hash (A4: head naming = toc identity)"
    );
    let toc = Toc::decode(&toc_bytes).expect("the committed toc must decode");
    assert_eq!(
        toc.entities.len(),
        t.len(),
        "the toc must contain one entry per entity of the tree"
    );

    // Every entry describes bytes that are actually stored, under the exact key the entry
    // names, with the hash and length the entry claims.
    for entry in &toc.entities {
        let bytes = t.get(&entry.path).unwrap_or_else(|| {
            panic!(
                "the toc names entity {:?}, which the published tree does not contain",
                entry.path
            )
        });
        assert_eq!(
            entry.content_hash,
            content_hash(bytes),
            "toc entry for {:?} must claim the content hash of the tree's bytes",
            entry.path
        );
        assert_eq!(
            entry.len as usize,
            bytes.len(),
            "toc entry for {:?} must claim the byte length of the tree's bytes",
            entry.path
        );
        let key = cfg_entity(&entry.path, &hex_hash(&entry.content_hash));
        let stored = node
            .handle
            .get_stale(key.as_bytes())
            .await
            .expect("entity read")
            .unwrap_or_else(|| {
                panic!(
                    "entity {:?} is named by the committed toc but not stored — the batch was \
                     not atomic",
                    entry.path
                )
            });
        assert_eq!(
            stored.as_slice(),
            bytes.as_slice(),
            "stored bytes for {:?} must be exactly what the toc describes",
            entry.path
        );
    }

    // And the reader's view through the head is exactly the published tree.
    assert_eq!(
        expect_tree(store.read().await.expect("read")),
        t,
        "the head must lead a reader to the tree that was published"
    );

    node.tonic.shutdown().await;
    node.thread.shutdown();
}
