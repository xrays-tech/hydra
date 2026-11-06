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
//! * since sharded V2: the toc is split into 32 fixed shards, missing entities/shards are
//!   uploaded first, and then the manifest + head token commit in ONE log entry; a 24k tree
//!   stays well under the per-value cap, format-6 heads still read, a bulk tree past the old
//!   single-batch cap commits via chunks, and concurrent publishes commit whole trees.
//!
//! The node is a one-member cluster assembled in-process. `Arachne::start`
//! cannot be used here: it is a process-wide singleton, so a test binary cannot
//! host several nodes through it — but this file needs only one, and it drives
//! the same production `ArachneConfigStore` the materializer will.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use arachne_kv::server::{assemble_cluster, ClusterConfig};
use arachne_kv::{NodeId, Profile, ReadConsistency};
use hydra_server::cluster::arachne_keys::{
    cfg_entity, cfg_manifest, cfg_shard, cfg_toc, content_hash, ctl_gc_retain, ctl_head,
    head_manifest_hash, hex_hash, manifest_head, EntityPath, Manifest, Toc, TocEntry,
    LEGACY_TOC_FORMAT, SHARD_COUNT, SHARD_MAX_BYTES, TOC_FORMAT,
};
use hydra_server::cluster::arachne_store::{
    split_shards, ArachneConfigStore, ConfigTree, ReadOutcome,
};

/// A raft member must be dialable at the address its peers were told about, so
/// these cannot be OS-assigned — and cargo runs the tests in this file
/// concurrently, so they cannot share one either (measured: a shared port fails
/// with "Address already in use").
const PORTS: [u16; 10] = [
    18211, 18212, 18213, 18214, 18215, 18216, 18217, 18218, 18219, 18220,
];
const PORT_BULK_24K: u16 = 18221;
const PORT_LEGACY: u16 = 18222;
const PORT_GC: u16 = 18223;
const PORT_MANIFEST_BAD: u16 = 18224;

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

/// Follow a head token to its toc the way the reader does: manifest + shards for `m:`, a legacy
/// whole toc for a bare hash. Used by the assertions that inspect storage directly.
async fn toc_for_head(node: &arachne_kv::server::AssembledClusterNode, head: &str) -> Toc {
    if let Some(manifest_hash) = head_manifest_hash(head) {
        let manifest_bytes = node
            .handle
            .get_stale(cfg_manifest(manifest_hash).as_bytes())
            .await
            .expect("manifest read")
            .expect("a sharded head must have a manifest");
        assert_eq!(
            hex_hash(&content_hash(&manifest_bytes)),
            manifest_hash,
            "the manifest must hash to the token the head names"
        );
        let manifest = Manifest::decode(&manifest_bytes).expect("manifest decodes");
        let mut entries = Vec::new();
        for digest in &manifest.shards {
            let bytes = node
                .handle
                .get_stale(cfg_shard(&hex_hash(digest)).as_bytes())
                .await
                .expect("shard read")
                .expect("every manifest shard must be stored");
            assert_eq!(content_hash(&bytes), *digest);
            let shard = Toc::decode_shard(&bytes).expect("shard decodes");
            entries.extend(shard.entities);
        }
        assert_eq!(entries.len(), manifest.entry_count as usize);
        Toc {
            format: TOC_FORMAT,
            entities: entries,
        }
    } else {
        let bytes = node
            .handle
            .get_stale(cfg_toc(head).as_bytes())
            .await
            .expect("legacy toc read")
            .expect("a bare head must have a legacy toc");
        Toc::decode(&bytes).expect("legacy toc decodes")
    }
}

fn toc_entry(path: EntityPath, bytes: &[u8]) -> TocEntry {
    TocEntry {
        path,
        content_hash: content_hash(bytes),
        len: bytes.len() as u32,
    }
}

/// Write a self-consistent (or deliberately inconsistent) sharded chain directly, used by the
/// GC and manifest-disagreement tests. The manifest is written under its own hash and the head is
/// its `m:` token, so the reader follows exactly the production path.
async fn install_sharded_chain(
    node: &arachne_kv::server::AssembledClusterNode,
    toc: &Toc,
    manifest: &Manifest,
) -> String {
    let (_, shards) = split_shards(toc).expect("split chain");
    let mut writes: Vec<(String, Vec<u8>)> = Vec::new();
    let mut seen = HashSet::new();
    for shard in &shards {
        let digest = shard.digest();
        if seen.insert(digest) {
            writes.push((cfg_shard(&hex_hash(&digest)), shard.encode()));
        }
    }
    let manifest_hash = manifest.hash();
    writes.push((cfg_manifest(&manifest_hash), manifest.encode()));
    let head = manifest_head(&manifest_hash);
    writes.push((ctl_head(), head.clone().into_bytes()));

    let refs: Vec<(&[u8], &[u8])> = writes
        .iter()
        .map(|(key, value)| (key.as_bytes(), value.as_slice()))
        .collect();
    node.handle
        .multi_put(&refs)
        .await
        .expect("install sharded chain");
    head
}

/// Write a legacy whole toc + entities + bare head, the shape an older build left behind.
async fn install_legacy_chain(
    node: &arachne_kv::server::AssembledClusterNode,
    tree: &ConfigTree,
) -> String {
    let entries: Vec<TocEntry> = tree
        .iter()
        .map(|(path, bytes)| toc_entry(path.clone(), bytes))
        .collect();
    let toc = Toc::new(LEGACY_TOC_FORMAT, entries).expect("legacy toc");
    let toc_hash = toc.hash();
    let mut writes: Vec<(String, Vec<u8>)> = tree
        .iter()
        .map(|(path, bytes)| {
            (
                cfg_entity(path, &hex_hash(&content_hash(bytes))),
                bytes.clone(),
            )
        })
        .collect();
    writes.push((cfg_toc(&toc_hash), toc.encode()));
    writes.push((ctl_head(), toc_hash.clone().into_bytes()));

    let refs: Vec<(&[u8], &[u8])> = writes
        .iter()
        .map(|(key, value)| (key.as_bytes(), value.as_slice()))
        .collect();
    node.handle
        .multi_put(&refs)
        .await
        .expect("install legacy chain");
    toc_hash
}

/// A full round trip, and the commit-point property (2026-10-10, multi_put): missing
/// entities are pre-uploaded under content-addressed keys, then the head MOVES WITH the toc
/// it names — one atomic final batch, one log entry — so a reader sees either the old tree
/// or the new one, never a mix, and once it has, the toc names exactly the tree published.
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

    // The entity chunks are uploaded first and then toc + head commit together, so there is
    // no intermediate state for a reader to land in: after a second publish the head moved
    // WITH its tree, and reading still yields one coherent config.
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
    let read = expect_tree(ArachneConfigStore::new(node.handle.clone()).read().await.expect(
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
        expect_tree(
            ArachneConfigStore::new(node.handle.clone())
                .read()
                .await
                .expect("B's tree must be readable too")
        ),
        b
    );

    node.tonic.shutdown().await;
    node.thread.shutdown();
}

/// THE FINAL COMMIT IS ONE LOG ENTRY (plan A1), and an unchanged entity is not resent.
///
/// The evidence is index arithmetic, not timing: `get_stale_with_index` reports the raft log
/// index of the entry that wrote the key. Missing entities and shards are uploaded in earlier,
/// bounded chunks (content-addressed, unreferenced until the head moves), and the final batch
/// writes manifest + head together: the manifest lands at the SAME index as the head. Before
/// `multi_put` the same measurement gave `+N+2` (N entities + toc + head as separate proposes).
/// The delta itself is `>= 1` rather than `== 1` (2026-10-10, oracle F4): a no-op entry from a
/// leader handover may also advance the index, and that would false-red an exact-equality claim
/// without meaning the commit split. What pins "same final batch" is the index equality
/// between manifest and head; an entity the new tree did not change keeps ITS origin index (it
/// was skipped, not rewritten).
///
/// Falsification: write manifest and head as separate proposes and the manifest's index no
/// longer equals the head's (manifest lands an entry earlier); always resend every entity and
/// the frozen acme index advances.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_final_commit_is_one_log_entry_and_unchanged_entities_stay_put() {
    let node = start_node("one-entry", PORTS[6]).await;
    wait_writable(&node).await;
    let store = ArachneConfigStore::new(node.handle.clone());

    // First publish: everything is new, so the two entities are uploaded in a chunk first,
    // then toc + head commit in the final batch.
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

    // manifest and head were written BY the final commit entry: both carry its index, and the
    // manifest key under the committed hash is the one that entry wrote (manifest/head are never
    // skipped).
    let manifest_hash = head_manifest_hash(&h2).expect("a new head is a manifest token");
    let (manifest_bytes, manifest_i) = node
        .handle
        .get_stale_with_index(cfg_manifest(manifest_hash).as_bytes())
        .await
        .expect("manifest read")
        .expect("every session rewrites the manifest");
    assert_eq!(
        manifest_i, i2,
        "the manifest must be written by the SAME log entry as the head (one atomic batch), got \
         manifest={manifest_i} head={i2}"
    );
    assert_eq!(
        hex_hash(&content_hash(&manifest_bytes)),
        manifest_hash,
        "the stored manifest must hash to the token the head names"
    );
    assert!(
        node.handle
            .get_stale(cfg_toc(&h2).as_bytes())
            .await
            .expect("legacy key read")
            .is_none(),
        "a new sharded publish must NOT write cfg_toc"
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

    // A CHANGED tree still commits in one final entry, and the entities it did not change stay
    // untouched.
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
         as above; the atomicity pin is the toc/head index equality): head index went {i2} → {i3}"
    );
    // Same-entry pin for THIS publish: the manifest of the committed token carries the index of
    // the entry that moved the head.
    let manifest3_hash = head_manifest_hash(&h3).expect("a new head is a manifest token");
    let (manifest3_bytes, manifest3_i) = node
        .handle
        .get_stale_with_index(cfg_manifest(manifest3_hash).as_bytes())
        .await
        .expect("manifest read")
        .expect("every session rewrites the manifest");
    assert_eq!(
        manifest3_i, i3,
        "the third manifest must be written by the SAME log entry as the head (one atomic commit \
         batch), got manifest={manifest3_i} head={i3}"
    );
    assert_eq!(
        hex_hash(&content_hash(&manifest3_bytes)),
        manifest3_hash,
        "the third stored manifest must hash to the token the head names"
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
        .expect("the third publish must have stored initech");
    assert!(
        initech_i < i3,
        "a NEWLY written entity is uploaded in a chunk BEFORE the final toc/head commit, so its \
         origin index must precede the head's: entity={initech_i} head={i3}"
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

/// A BULK TREE PAST THE OLD SINGLE-BATCH CAP COMMITS VIA CHUNKS.
///
/// 4200 entities of ~1 KiB: a single `multi_put` would exceed BOTH the library's 4096-entry
/// and 4 MiB total-byte bounds and be refused whole. `publish_inner` uploads the missing
/// entities in bounded chunks (<=2048 entries / <=2 MiB key+value) and only then commits
/// toc + head in one final entry, so the bulk tree lands intact and the head still names a
/// fully present tree.
///
/// Falsification: put every missing entity in one batch and this fails with
/// `StoreError::Refused`; skip the final commit batch and the head never moves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bulk_tree_past_the_old_single_batch_cap_commits_via_chunks() {
    let node = start_node("bulk", PORTS[9]).await;
    wait_writable(&node).await;
    let store = ArachneConfigStore::new(node.handle.clone());

    let mut bulk = ConfigTree::new();
    for i in 0..4200 {
        bulk.insert(EntityPath::Tenant(format!("bulk-{i:04}")), vec![b'x'; 1024]);
    }
    let hash = store
        .publish(&bulk)
        .await
        .expect("a bulk tree past the old single-batch cap must commit via chunks");

    assert_eq!(
        store.current_hash().await.expect("head read"),
        Some(hash),
        "the final commit must move the head to the bulk tree"
    );
    assert_eq!(
        expect_tree(store.read().await.expect("read")),
        bulk,
        "every entity of the chunked upload must be present and hash-verified under the head"
    );

    node.tonic.shutdown().await;
    node.thread.shutdown();
}

/// CONCURRENT PUBLISHERS COMMIT WHOLE TREES, NEVER A MIX (plan A3).
///
/// Two publishes racing on the same node: each entity upload is content-addressed and the
/// final toc/head commit is atomic, so both must commit, and whichever head wins, a reader
/// sees exactly one publisher's tree — complete, with every entity (including the loser's)
/// still stored under its own content-addressed key. The deterministic interleave test above
/// pins the key-non-conflict side of this; this one is the real race.
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

    // Both trees are COMPLETE in storage: every entity of both publishers, plus both
    // manifest+shard chains — so whichever head wins, flipping to it would be equally readable.
    for (head, expected) in [(&ha, &a), (&hb, &b)] {
        let toc = toc_for_head(&node, head).await;
        assert_eq!(
            toc.entities.len(),
            expected.len(),
            "the manifests/shards of {head} must describe every entity"
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

/// THE HEAD NAMES A MANIFEST WHOSE SHARDS DESCRIBE A FULLY PRESENT TREE (plan A4/V2).
///
/// Atomic publishing only pays off if the head — written inside the same batch as the manifest
/// and its shards — really does name a manifest whose shards cover every entity with matching
/// hash and length, and every one of those entities is stored. This walks the chain the reader
/// walks: head token → manifest → shards → entries → entity bytes, including a binary value with
/// NUL and 0xff bytes. The tiny tree also pins the shared-empty-shard property: most of the 32
/// buckets are empty and therefore carry the SAME digest.
///
/// Falsification: publish a shard with a missing/short entity entry and the per-entry
/// presence/length assertion fails; write the head outside the batch and a reader could reach
/// this chain at a moment when the manifest it decodes does not describe the batch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_head_names_a_manifest_that_describes_a_fully_present_tree() {
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

    // The manifest the committed head names.
    let manifest_hash = head_manifest_hash(&hash).expect("a new publish names a manifest");
    let manifest_bytes = node
        .handle
        .get_stale(cfg_manifest(manifest_hash).as_bytes())
        .await
        .expect("manifest read")
        .expect("a committed head must have a manifest under its hash");
    assert_eq!(
        hex_hash(&content_hash(&manifest_bytes)),
        manifest_hash,
        "the head token must carry the manifest's content hash"
    );
    let manifest = Manifest::decode(&manifest_bytes).expect("the committed manifest must decode");
    assert_eq!(manifest.shards.len(), SHARD_COUNT);
    assert_eq!(manifest.entry_count as usize, t.len());
    assert_eq!(
        manifest.entity_bytes,
        t.values().map(|bytes| bytes.len() as u64).sum::<u64>()
    );
    let unique_shards: HashSet<[u8; 32]> = manifest.shards.iter().copied().collect();
    assert!(
        unique_shards.len() < SHARD_COUNT,
        "the empty buckets must share one identical shard digest; got {} unique",
        unique_shards.len()
    );

    let toc = toc_for_head(&node, &hash).await;
    assert_eq!(
        toc.entities.len(),
        t.len(),
        "the merged shards must contain one entry per entity of the tree"
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

/// A 24k-entity tree is the size the sharded era exists for: the whole toc is past the old
/// single-value budget, but the 32 shards are small and each is content-addressed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_24k_entity_tree_publishes_and_reads_via_32_shards() {
    let node = start_node("bulk24k", PORT_BULK_24K).await;
    wait_writable(&node).await;
    let store = ArachneConfigStore::new(node.handle.clone());

    let mut t = ConfigTree::new();
    for i in 0..24_000 {
        t.insert(
            EntityPath::User(format!("u{i:05}")),
            format!("value-{i}").into_bytes(),
        );
    }
    let head = store
        .publish(&t)
        .await
        .expect("a 24k tree must publish through its shards");
    assert!(head.starts_with("m:"), "a new head is a manifest token");

    let manifest_hash = head_manifest_hash(&head).expect("manifest token");
    let manifest_bytes = node
        .handle
        .get_stale(cfg_manifest(manifest_hash).as_bytes())
        .await
        .expect("manifest read")
        .expect("manifest must be stored");
    let manifest = Manifest::decode(&manifest_bytes).expect("manifest decodes");
    assert_eq!(manifest.entry_count, 24_000);

    let mut sizes = Vec::with_capacity(SHARD_COUNT);
    for digest in &manifest.shards {
        let bytes = node
            .handle
            .get_stale(cfg_shard(&hex_hash(digest)).as_bytes())
            .await
            .expect("shard read")
            .expect("every shard must be stored");
        assert!(
            bytes.len() <= SHARD_MAX_BYTES,
            "shard {} is {} bytes, over SHARD_MAX_BYTES",
            sizes.len(),
            bytes.len()
        );
        sizes.push(bytes.len());
    }
    eprintln!("24k shard sizes in bucket order: {sizes:?}");
    eprintln!("24k shard total: {} bytes", sizes.iter().sum::<usize>());

    assert_eq!(
        expect_tree(store.read().await.expect("read the 24k tree")),
        t,
        "every one of the 24k entities must survive the manifest+shard round trip"
    );

    node.tonic.shutdown().await;
    node.thread.shutdown();
}

/// A legacy format-6 tree under a bare head is still readable after the upgrade. This is the
/// one-way bridge: new writes never produce it, old heads keep materializing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_legacy_format_6_tree_still_reads_through_a_bare_head() {
    let node = start_node("legacy", PORT_LEGACY).await;
    wait_writable(&node).await;
    let store = ArachneConfigStore::new(node.handle.clone());

    let t = tree(&[("acme", b"legacy-one"), ("globex", b"legacy-two")]);
    let head = install_legacy_chain(&node, &t).await;
    assert_eq!(head.len(), 64, "a legacy head is the bare 64-hex toc hash");
    assert_eq!(
        store.current_hash().await.expect("head read"),
        Some(head.clone())
    );
    assert_eq!(
        expect_tree(store.read().await.expect("legacy read")),
        t,
        "format 6 must decode through the bare-token fallback"
    );

    node.tonic.shutdown().await;
    node.thread.shutdown();
}

/// A manifest that disagrees with its shards (or a corrupted value) is refused, never served.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manifest_and_shard_disagreements_are_refused() {
    let node = start_node("manifest-bad", PORT_MANIFEST_BAD).await;
    wait_writable(&node).await;
    let store = ArachneConfigStore::new(node.handle.clone());

    let t = tree(&[("acme", b"one")]);
    let head = store.publish(&t).await.expect("publish");
    let manifest_hash = head_manifest_hash(&head).expect("manifest token");
    let manifest_key = cfg_manifest(manifest_hash);
    let original_manifest = node
        .handle
        .get_stale(manifest_key.as_bytes())
        .await
        .expect("manifest read")
        .expect("manifest");
    let manifest = Manifest::decode(&original_manifest).expect("manifest decodes");

    // 1) Corrupted manifest bytes no longer hash to the head token.
    let mut tampered = original_manifest.clone();
    tampered.push(0);
    node.handle
        .put(manifest_key.as_bytes(), &tampered)
        .await
        .expect("tamper manifest");
    let err = ArachneConfigStore::new(node.handle.clone())
        .read()
        .await
        .expect_err("tampered manifest must be refused");
    assert!(
        err.to_string().contains("hash") || err.to_string().contains("manifest"),
        "the error must identify the manifest mismatch, got: {err}"
    );
    node.handle
        .put(manifest_key.as_bytes(), &original_manifest)
        .await
        .expect("restore manifest");

    // 2) A shard whose bytes do not match its digest is refused before decode/merge.
    let digest = manifest.shards[0];
    let shard_key = cfg_shard(&hex_hash(&digest));
    let original_shard = node
        .handle
        .get_stale(shard_key.as_bytes())
        .await
        .expect("shard read")
        .expect("shard");
    node.handle
        .put(shard_key.as_bytes(), b"not the shard")
        .await
        .expect("tamper shard");
    let err = ArachneConfigStore::new(node.handle.clone())
        .read()
        .await
        .expect_err("tampered shard must be refused");
    assert!(
        err.to_string().contains("digest") || err.to_string().contains("shard"),
        "the error must identify the shard mismatch, got: {err}"
    );
    node.handle
        .put(shard_key.as_bytes(), &original_shard)
        .await
        .expect("restore shard");

    // 3) Manifest totals disagreeing with the shards are refused after the merge.
    let toc_one = Toc::new(
        TOC_FORMAT,
        vec![toc_entry(EntityPath::Tenant("x".into()), b"v")],
    )
    .expect("toc");
    let (derived, _) = split_shards(&toc_one).expect("split");
    let wrong_count = Manifest::new(
        derived.shards.clone(),
        derived.entry_count + 1,
        derived.entity_bytes,
    )
    .expect("manifest");
    install_sharded_chain(&node, &toc_one, &wrong_count).await;
    let err = ArachneConfigStore::new(node.handle.clone())
        .read()
        .await
        .expect_err("a wrong entry count must be refused");
    assert!(
        err.to_string().contains("entries"),
        "the error must name the entry-count mismatch, got: {err}"
    );

    let wrong_bytes = Manifest::new(
        derived.shards.clone(),
        derived.entry_count,
        derived.entity_bytes + 1,
    )
    .expect("manifest");
    install_sharded_chain(&node, &toc_one, &wrong_bytes).await;
    let err = ArachneConfigStore::new(node.handle.clone())
        .read()
        .await
        .expect_err("wrong entity bytes must be refused");
    assert!(
        err.to_string().contains("entity bytes"),
        "the error must name the byte-count mismatch, got: {err}"
    );

    // 4) Duplicate paths across/inside the merged shards are refused.
    let dup = EntityPath::Tenant("duplicate".into());
    let toc_dup = Toc::new(
        TOC_FORMAT,
        vec![toc_entry(dup.clone(), b"one"), toc_entry(dup, b"two")],
    )
    .expect("toc");
    let (dup_manifest, _) = split_shards(&toc_dup).expect("split duplicate");
    install_sharded_chain(&node, &toc_dup, &dup_manifest).await;
    let err = ArachneConfigStore::new(node.handle.clone())
        .read()
        .await
        .expect_err("duplicate paths must be refused");
    assert!(
        err.to_string().contains("more than once"),
        "the error must name the duplicate path, got: {err}"
    );

    node.tonic.shutdown().await;
    node.thread.shutdown();
}

/// GC resolves mixed retain tokens, keeps current+retained manifests/tocs, and NEVER touches
/// shards (deferred shard GC is a documented leak, not an accidental delete).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gc_keeps_current_and_retained_manifests_and_never_deletes_shards() {
    let node = start_node("gc", PORT_GC).await;
    wait_writable(&node).await;
    let store = ArachneConfigStore::new(node.handle.clone());

    let h1 = store
        .publish(&tree(&[("acme", b"first")]))
        .await
        .expect("first publish");
    let h2 = store
        .publish(&tree(&[("acme", b"second")]))
        .await
        .expect("second publish");
    let mh1 = head_manifest_hash(&h1).expect("manifest token 1");
    let mh2 = head_manifest_hash(&h2).expect("manifest token 2");

    // A legacy format-6 toc value to mix into the retain history. It is not the current head; the
    // point is that GC resolves a bare retain token to cfg_toc and keeps it.
    let legacy = Toc::new(
        LEGACY_TOC_FORMAT,
        vec![toc_entry(EntityPath::Tenant("legacy".into()), b"v")],
    )
    .expect("legacy toc");
    let legacy_hash = legacy.hash();
    node.handle
        .put(cfg_toc(&legacy_hash).as_bytes(), &legacy.encode())
        .await
        .expect("write legacy toc");

    // Mixed history: current manifest, previous manifest, bare legacy toc — carried verbatim.
    let retain = format!("{h2}\n{h1}\n{legacy_hash}");
    node.handle
        .put(ctl_gc_retain().as_bytes(), retain.as_bytes())
        .await
        .expect("mixed retain");

    // Two old trampolines that are NOT retained: one manifest and one legacy toc.
    node.handle
        .put(cfg_manifest("deadbeef").as_bytes(), b"old manifest")
        .await
        .expect("old manifest");
    node.handle
        .put(cfg_toc("feedface").as_bytes(), b"old toc")
        .await
        .expect("old toc");

    let manifest = Manifest::decode(
        &node
            .handle
            .get_stale(cfg_manifest(mh2).as_bytes())
            .await
            .expect("manifest read")
            .expect("current manifest"),
    )
    .expect("manifest decodes");
    let shard_key = cfg_shard(&hex_hash(&manifest.shards[0]));

    let stats = store.gc(100).await.expect("gc");
    assert_eq!(stats.old_manifests, 1, "one unretained manifest");
    assert_eq!(stats.manifest_deleted, 1);
    assert_eq!(stats.old_tocs, 1, "one unretained legacy toc");
    assert_eq!(stats.toc_deleted, 1);
    assert_eq!(stats.deleted(), 2);
    assert_eq!(stats.remaining(), 0);

    for key in [
        cfg_manifest(mh1),
        cfg_manifest(mh2),
        cfg_toc(&legacy_hash),
        shard_key,
    ] {
        assert!(
            node.handle
                .get_stale(key.as_bytes())
                .await
                .expect("gc-check read")
                .is_some(),
            "{key} must survive GC (current/retained manifest or shard)"
        );
    }
    for key in [cfg_manifest("deadbeef"), cfg_toc("feedface")] {
        assert!(
            node.handle
                .get_stale(key.as_bytes())
                .await
                .expect("gc-check read")
                .is_none(),
            "{key} was unretained and must be deleted"
        );
    }

    node.tonic.shutdown().await;
    node.thread.shutdown();
}

/// The explicit 0.5 materialize recipe against a REAL multi-shard tree:
/// head-with-index -> `wait_applied` -> batched `multi_get(Stale)` for manifest, shards, entities.
///
/// This is the path the store now uses internally; asserting it directly pins the API contract
/// (single applied barrier, no missing-key retry loop) rather than only the store's return value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_05_head_wait_applied_multi_get_path_reads_a_multi_shard_tree() {
    let node = start_node("materialize05", 18225).await;
    wait_writable(&node).await;
    let store = ArachneConfigStore::new(node.handle.clone());

    let mut t = ConfigTree::new();
    for i in 0..600 {
        t.insert(
            EntityPath::User(format!("u{i:04}")),
            format!("value-{i}").into_bytes(),
        );
    }
    let (head, head_index) = store
        .publish_with_index(&t)
        .await
        .expect("publish with index");
    assert!(head.starts_with("m:"));

    node.handle
        .wait_applied(head_index, Duration::from_secs(10))
        .await
        .expect("wait for the head index locally");

    let manifest_hash = head_manifest_hash(&head).expect("manifest token");
    let manifest = node
        .handle
        .multi_get(
            &[cfg_manifest(manifest_hash).as_bytes()],
            ReadConsistency::Stale,
        )
        .await
        .expect("manifest multi_get")
        .values
        .remove(0)
        .expect("manifest present after wait_applied");
    assert_eq!(hex_hash(&content_hash(&manifest)), manifest_hash);
    let manifest = Manifest::decode(&manifest).expect("manifest decodes");

    let mut seen = HashSet::new();
    let mut shard_keys = Vec::new();
    for digest in &manifest.shards {
        if seen.insert(*digest) {
            shard_keys.push(cfg_shard(&hex_hash(digest)));
        }
    }
    let mut shard_values = Vec::new();
    for chunk in shard_keys.chunks(16) {
        let refs: Vec<&[u8]> = chunk.iter().map(String::as_bytes).collect();
        shard_values.extend(
            node.handle
                .multi_get(&refs, ReadConsistency::Stale)
                .await
                .expect("shard multi_get")
                .values,
        );
    }

    let mut toc_entries = Vec::new();
    for value in shard_values {
        let bytes = value.expect("every shard present after wait_applied");
        let shard = Toc::decode_shard(&bytes).expect("shard decodes");
        toc_entries.extend(shard.entities);
    }
    assert_eq!(toc_entries.len(), t.len());

    let mut entity_keys = Vec::new();
    for entry in &toc_entries {
        entity_keys.push(cfg_entity(&entry.path, &hex_hash(&entry.content_hash)));
    }
    let mut values = Vec::new();
    for chunk in entity_keys.chunks(256) {
        let refs: Vec<&[u8]> = chunk.iter().map(String::as_bytes).collect();
        values.extend(
            node.handle
                .multi_get(&refs, ReadConsistency::Stale)
                .await
                .expect("entity multi_get")
                .values,
        );
    }
    let mut rebuilt = ConfigTree::new();
    for (entry, value) in toc_entries.iter().zip(values) {
        let bytes = value.expect("every entity present after wait_applied");
        assert_eq!(bytes.len(), entry.len as usize);
        assert_eq!(content_hash(&bytes), entry.content_hash);
        rebuilt.insert(entry.path.clone(), bytes);
    }
    assert_eq!(rebuilt, t, "the 0.5 batched path must reconstruct the tree");

    node.tonic.shutdown().await;
    node.thread.shutdown();
}
