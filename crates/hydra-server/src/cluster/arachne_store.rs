//! # The config commit point (ADR-0001, plan T2.2)
//!
//! This is the **only** place allowed to write [`crate::cluster::arachne_keys::ctl_head`].
//! Everything else reads through it. That single-writer rule is what makes the
//! head a commit point rather than a label:
//!
//! ```text
//! publish(tree) =
//!   1. collect the entities this node does not have (content-addressed keys)
//!   2. multi_put(chunk of missing entities)  … one per bounded chunk, no head
//!   3. multi_put([new toc, gc_retain, head]) <- ONE log entry = the commit
//! ```
//!
//! A bulk config can have ~12k entities, far past the library's 4096-entry /
//! 4 MiB single-batch bound, so the missing entities are uploaded first in
//! bounded chunks and only then does a FINAL batch move the head. That final
//! batch is one log entry, one session, one apply, all or nothing: a failed
//! publish leaves the previous tree current, and a batch that overruns a
//! library bound is refused BEFORE propose so it never enters the log. Head
//! remains the only commit point; it moves in the same entry as the toc it
//! names.
//!
//! Uploading entities before the head is safe because their keys are
//! content-addressed: a value is stored under the hash of its own bytes, so it
//! cannot overwrite another entity, and nothing names it until the head moves.
//! A failed chunk therefore leaves only unreferenced (GC-able) orphans while
//! the old head and tree stay exactly as they were.
//!
//! ## Reading
//!
//! `read` goes head → toc → entities, using **`get_stale` only**. Two
//! consequences, both deliberate:
//!
//! * a follower can materialize without a linearizable read (a follower's `get`
//!   returns `QuorumUnavailable` immediately — measured, ADR-0001 §10 F-2);
//! * a reader that catches the cluster mid-publish sees EITHER the old tree or
//!   the new one, never a mix: the publish applies in a single log entry (head
//!   moves together with its entities), and past that head names exactly one
//!   toc while every entity is checked against the hash that toc records. A
//!   mismatch is a retry, not a repair.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use arachne_kv::client::{ArachneError, Handle};

use super::arachne_keys::{
    cfg_entity, cfg_toc, content_hash, ctl_gc_retain, ctl_head, hex_hash, validate_entities,
    EntityPath, KeysError, Toc, TocEntry, CFG_PREFIX, TOC_FORMAT,
};

/// The entities of one config tree: path → encoded bytes.
///
/// A `BTreeMap` rather than a `HashMap` so the toc's entity order is canonical
/// by construction. The toc's bytes ARE the tree's name, so an unordered map
/// would make the same logical config hash differently between two publishers —
/// and every follower would then see a "new" tree after every publish.
pub type ConfigTree = BTreeMap<EntityPath, Vec<u8>>;

/// What a reader got, or why it could not get it.
#[derive(Debug)]
pub enum ReadOutcome {
    /// No tree has ever been published: the cluster has an empty config.
    Empty,
    /// The tree named by the head, fully decoded and hash-checked.
    Tree(Arc<ConfigTree>),
}

/// Why a store operation failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreError {
    /// This node is not the leader: someone else owns the commit point.
    NotLeader,
    /// No majority was reachable, so raft could not commit or confirm anything.
    ///
    /// Its own variant rather than a string inside [`StoreError::Arachne`] because
    /// it is a different FACT: the cluster has no quorum, which is an outage with
    /// one fix (get a majority up), not a transport hiccup or a codec bug. It is
    /// what `hydra_arachne_quorum_unavailable_total` counts, and the difference is
    /// what separates "the alert fires when the cluster is down" from "the alert
    /// fires whenever anything goes wrong".
    QuorumUnavailable,
    /// The key space refused a write **before** it entered the log: a bound was
    /// overrun (a value over `max_value_bytes`, or a `multi_put` batch over the
    /// entry-count / total-byte caps). The refused batch commits nothing, and the
    /// head/tree are untouched; entities uploaded by an earlier chunk can remain as
    /// unreferenced, GC-able orphans.
    ///
    /// Produced at the `multi_put` call sites in [`Self::publish_inner`] (one per entity chunk,
    /// plus the final toc/head batch), which discriminate `ArachneError::InvalidArgument` for
    /// the batch they assembled (2026-10-10: the shared `map_err` that serves the read path
    /// deliberately does NOT map `InvalidArgument` here — a bound refusal is a fact about a
    /// batch).
    ///
    /// Its own variant rather than a string inside [`StoreError::Arachne`] because it
    /// is a fact about the CONFIG, not about the cluster — the fix is a smaller config,
    /// not a repair — and it is the same refusal `ConfigPublisher` counts at encode
    /// time. It is what `hydra_arachne_publish_total{result="refused"}` counts
    /// (ADR-0001 risk R2's capacity alarm), and the difference from `Arachne` is what
    /// separates "this config will never fit" from "the cluster had a bad day".
    Refused(String),
    /// The key space refused something (a bad entity id, an undecodable toc).
    Keys(KeysError),
    /// Arachne refused or could not serve the operation.
    Arachne(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotLeader => f.write_str(
                "this node is not the raft leader; the config commit point is owned by the leader",
            ),
            Self::QuorumUnavailable => f.write_str(
                "no raft majority was reachable, so the control plane could not commit or confirm \
                 anything; the node keeps serving the config it already materialized",
            ),
            Self::Refused(e) => write!(
                f,
                "the cluster refused the write before proposing it (nothing entered the log): {e}"
            ),
            Self::Keys(e) => write!(f, "config key space: {e}"),
            Self::Arachne(e) => write!(f, "arachne: {e}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<KeysError> for StoreError {
    fn from(e: KeysError) -> Self {
        Self::Keys(e)
    }
}

/// The first key strictly after every key carrying `prefix`, or `None` when
/// the prefix is all `0xff` (unbounded).
fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut out = prefix.to_vec();
    while let Some(b) = out.pop() {
        if b < 0xff {
            out.push(b + 1);
            return Some(out);
        }
    }
    None
}

/// The most entities one pre-upload `multi_put` carries.
///
/// The library's hard limit is 4096 entries; half of it leaves ample headroom and keeps a
/// single apply's lock hold bounded even for a ~12k-entity bulk config.
const ENTITY_CHUNK_MAX_ENTRIES: usize = 2048;

/// The most key+value bytes one pre-upload `multi_put` carries.
///
/// The library's hard limit is 4 MiB; 2 MiB leaves room for raft framing and means an
/// over-sized entity is the ONLY thing that can push a chunk past the soft budget.
const ENTITY_CHUNK_MAX_BYTES: usize = 2 * 1024 * 1024;

/// Split missing `(key, value)` entities into `multi_put` chunks under both bounds.
///
/// A value that alone exceeds [`ENTITY_CHUNK_MAX_BYTES`] becomes a chunk of its own rather
/// than being split (it cannot be): the library's per-value / total-byte validation then
/// decides whether that entity is representable, and the caller maps its refusal to
/// [`StoreError::Refused`]. Never emits an empty chunk.
fn chunk_missing_entities<'a>(missing: &'a [(String, &'a [u8])]) -> Vec<Vec<(&'a [u8], &'a [u8])>> {
    let mut chunks = Vec::new();
    let mut current: Vec<(&[u8], &[u8])> = Vec::new();
    let mut bytes = 0usize;
    for (key, value) in missing {
        let entry_bytes = key.len() + value.len();
        let full = current.len() >= ENTITY_CHUNK_MAX_ENTRIES;
        // `!current.is_empty()` is what lets a lone oversized entity through as its own chunk.
        let over_budget = !current.is_empty() && bytes + entry_bytes > ENTITY_CHUNK_MAX_BYTES;
        if full || over_budget {
            chunks.push(std::mem::take(&mut current));
            bytes = 0;
        }
        current.push((key.as_bytes(), *value));
        bytes += entry_bytes;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn map_err(e: ArachneError) -> StoreError {
    match e {
        ArachneError::NotLeader { .. } => StoreError::NotLeader,
        ArachneError::QuorumUnavailable => StoreError::QuorumUnavailable,
        // NOTE: `InvalidArgument` is deliberately NOT mapped to `StoreError::Refused` here
        // (2026-10-10, oracle F2). `map_err` serves every call in this module — reads
        // (`read_inner`, `current_hash`, `current_head_with_index`) as well as the publish —
        // and a pre-propose bound refusal is a fact about a `multi_put` BATCH, not about a
        // read. The `Refused` discrimination therefore lives at the `multi_put` call sites in
        // `publish_inner`, where the batches it judges were actually assembled.
        other => StoreError::Arachne(format!("{other:?}")),
    }
}

/// Record a lost-quorum refusal against the operation that hit it.
///
/// `map_err` stays PURE (it is called from five places, and a side effect inside a
/// name like `map_err` is the kind of thing that gets deleted during a refactor);
/// the two public entry points do the counting, which is also what lets the label
/// be `read` or `publish` instead of a guess.
fn note_quorum_failure(op: &str, e: &StoreError) {
    if matches!(e, StoreError::QuorumUnavailable) {
        crate::admin::metrics::record_arachne_quorum_unavailable(op);
    }
}

/// The toc describing `next`, validated — the one thing a publish must decide up front.
///
/// ## What used to be here
///
/// `plan_publish(previous_toc, next) -> {write, toc}`: it compared the new tree against the
/// committed one and listed the entities whose bytes had changed. That comparison was how
/// "unchanged entities are not rewritten" was decided — and it needed a BASELINE (the committed
/// toc) that a lagging node cannot supply reliably, which is why it went away with the
/// content-addressed keys: the key `hydra/cfg/e/<path>/<hash>` answers "is this already stored?"
/// about itself, checked against this node's own replica
/// ([`ArachneConfigStore::publish`]).
///
/// Validation stays, and it stays FIRST: an id carrying a separator would write into another
/// entity's namespace, so it has to be refused before a single byte reaches the cluster.
///
/// # Errors
/// [`KeysError`] when an entity id cannot be carried by a key, or the toc cannot be built.
pub fn toc_for(tree: &ConfigTree) -> Result<Toc, KeysError> {
    let entries: Vec<TocEntry> = tree
        .iter()
        .map(|(path, bytes)| TocEntry {
            path: path.clone(),
            content_hash: content_hash(bytes),
            len: bytes.len() as u32,
        })
        .collect();
    validate_entities(&entries)?;
    Toc::new(TOC_FORMAT, entries)
}

/// The commit point for the config tree.
///
/// `Clone` is cheap (the handle wraps an `Arc` to the node's command channel) and exists so a
/// materializer and a publisher can share one node without either owning it.
#[derive(Clone)]
pub struct ArachneConfigStore {
    handle: Handle,
}

/// One bounded garbage-collection pass over the control-plane keyspace.
///
/// The config tree is content-addressed and append-only: every publish writes a
/// new toc and new entity versions, while the head names only the current tree.
/// Nothing in the earlier design deleted the old ones, so a 2000-role run left
/// 447–511 MB snapshots and the 64 MB snapshot threshold made Arachne build a
/// half-gigabyte snapshot every few hundred writes. Snapshot creation blocks
/// apply, the leader misses quorum, and leadership flaps. This pass keeps the
/// CURRENT tree and deletes the rest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcStats {
    /// Old toc values deleted in this pass.
    pub toc_deleted: usize,
    /// Unreferenced entity values deleted in this pass.
    pub entities_deleted: usize,
    /// Old toc values that existed when the pass started.
    pub old_tocs: usize,
    /// Unreferenced entity values that existed when the pass started.
    pub unreferenced_entities: usize,
}

impl GcStats {
    #[must_use]
    pub fn deleted(&self) -> usize {
        self.toc_deleted + self.entities_deleted
    }

    #[must_use]
    pub fn remaining(&self) -> usize {
        self.old_tocs
            .saturating_sub(self.toc_deleted)
            .saturating_add(
                self.unreferenced_entities
                    .saturating_sub(self.entities_deleted),
            )
    }
}

impl ArachneConfigStore {
    /// Wrap a running node's handle.
    #[must_use]
    pub fn new(handle: Handle) -> Self {
        Self { handle }
    }

    /// The tree the head currently names, or [`ReadOutcome::Empty`].
    ///
    /// # Errors
    /// [`StoreError`] when the cluster cannot answer, or when what it returned
    /// does not match what the toc describes (a retry, not a repair).
    pub async fn read(&self) -> Result<ReadOutcome, StoreError> {
        let outcome = self.read_inner().await;
        if let Err(e) = &outcome {
            note_quorum_failure("read", e);
        }
        outcome
    }

    /// [`Self::read`] without the instrumentation, so the counting happens once.
    async fn read_inner(&self) -> Result<ReadOutcome, StoreError> {
        let Some(hash) = self.current_hash().await? else {
            return Ok(ReadOutcome::Empty);
        };

        let toc_bytes = match self.handle.get_stale(cfg_toc(&hash).as_bytes()).await {
            Ok(Some(v)) => v,
            Ok(None) => {
                return Err(StoreError::Arachne(format!(
                    "the head names tree {hash} but its table of contents is missing; the \
                     materializer should retry"
                )))
            }
            Err(e) => return Err(map_err(e)),
        };
        let toc = Toc::decode(&toc_bytes)?;

        let mut tree = ConfigTree::new();
        for entry in &toc.entities {
            // The key carries the hash, so asking for an entity asks for EXACTLY the bytes the toc
            // describes — a concurrent publisher writing a different version of the same entity
            // writes a different key and cannot be picked up here by mistake.
            let key = cfg_entity(&entry.path, &hex_hash(&entry.content_hash));
            let bytes = match self.handle.get_stale(key.as_bytes()).await {
                Ok(Some(v)) => v,
                Ok(None) => {
                    return Err(StoreError::Arachne(format!(
                        "tree {hash} is missing entity {}; the materializer should retry",
                        entry.path.to_key_segment()
                    )))
                }
                Err(e) => return Err(map_err(e)),
            };
            // Still verified, although the key already commits to the hash: this is what catches a
            // store that returned the wrong value (or bytes that rotted), and it is the check that
            // makes a torn read impossible to SERVE.
            if bytes.len() != entry.len as usize || content_hash(&bytes) != entry.content_hash {
                return Err(StoreError::Arachne(format!(
                    "entity {} in tree {hash} does not match the hash its table of contents \
                     records; refusing to serve a mixed tree",
                    entry.path.to_key_segment()
                )));
            }
            tree.insert(entry.path.clone(), bytes);
        }
        Ok(ReadOutcome::Tree(Arc::new(tree)))
    }

    /// The head and the **log index** of the entry that wrote it: the tree's GENERATION.
    ///
    /// `get_stale_with_index` (upstream v0.3.0, asked for in
    /// `dev-docs/upstream/arachne-kv-commit-index-request.md`) is what makes ordering possible without a
    /// quorum: the value it returns may be arbitrarily old, but it now says HOW old, and the index is
    /// assigned by the consensus layer in log order, so it is comparable across nodes.
    ///
    /// Upstream's constraints, honoured here: the index belongs to THIS key's origin, so entity keys are
    /// never cross-compared against the head's; and the `>=` rule is only sound for keys with no DELETE
    /// in their history — our tree is replace-by-put and never deletes an entity, which is what makes
    /// `ctl/head` a legal ordered key.
    ///
    /// # Errors
    /// [`StoreError`] when the cluster cannot answer.
    pub async fn current_head_with_index(&self) -> Result<Option<(String, u64)>, StoreError> {
        match self
            .handle
            .get_stale_with_index(ctl_head().as_bytes())
            .await
        {
            Ok(Some((v, index))) => Ok(Some((String::from_utf8_lossy(&v).into_owned(), index))),
            Ok(None) => Ok(None),
            Err(e) => Err(map_err(e)),
        }
    }

    /// The head's value, or `None` when nothing was ever published.
    ///
    /// **Read with `get_stale`, and that is a known, measured hazard — not an oversight.** The
    /// library documents its two reads as `get` = "linearizable read (default) … a quorum-confirmed
    /// read on the leader" (ReadIndex) and `get_stale` = "arbitrary stale read allowed; direct local
    /// state-machine read; **not monotone** (propsol N1)". The materializer does not merely display
    /// this value: it hands the tree it names to `ReplicaTarget::apply`, which REPLACES this node's
    /// SQLite config and its in-memory snapshot. A non-monotone read can therefore roll a node back
    /// over writes it already accepted and published, and because later publishes are built from the
    /// rolled-back database, the loss can become permanent — which is the shape of two CI samples
    /// (ADR-0001 plan, "观察": twelve writes all 201, twelve publishes `{result="ok"}`, both
    /// materializers `succeeded`, and one written row absent from BOTH databases).
    ///
    /// **Switching this to `get` was tried, measured and reverted — and `get` turned out to be the
    /// wrong direction** (2026-10-07). Idle machine, 5 runs per arm, the cluster's own acceptance
    /// drill: with `get` **4/5 FAILED**, with `get_stale` **0/5**. The failure MODE is the reason, not
    /// the rate: the survivor of a lost quorum answered the data plane with
    /// `404 {"message":"unknown_domain"}` while listing one provider fewer than a healthy node — a node
    /// that insists on a quorum-confirmed head read can no longer LEARN about newer config and keeps
    /// serving an older one. This stale read is load-bearing for exactly that reason: it is how a
    /// minority node still follows the head. (An earlier "3/3 vs 2/3" reading was confounded by running
    /// the arms alongside cargo builds on the same box; the A/B above is the clean one.)
    ///
    /// So the hazard is real and the fix is NOT a stronger read: it is to make the APPLY monotone
    /// (nothing here orders two tree hashes, so that needs an index or version carried with the tree)
    /// or to stop a publish from racing materialization. Measure any attempt with that same A/B before
    /// landing it.
    ///
    /// # Errors
    /// [`StoreError`] when the cluster cannot answer.
    pub async fn current_hash(&self) -> Result<Option<String>, StoreError> {
        match self.handle.get_stale(ctl_head().as_bytes()).await {
            Ok(Some(v)) => Ok(Some(String::from_utf8_lossy(&v).into_owned())),
            Ok(None) => Ok(None),
            Err(e) => Err(map_err(e)),
        }
    }

    /// Delete up to `max_deletes` keys that the CURRENT tree does not name.
    ///
    /// Scans `hydra/cfg/toc/` and `hydra/cfg/e/`, keeps the current toc and every
    /// entity it references, and deletes the rest oldest-first (tocs before
    /// entities). A bounded pass lets a caller hold the publish lock for a short
    /// time; the next pass continues.
    ///
    /// # Errors
    /// [`StoreError`] when the head/current toc cannot be read or a delete is
    /// refused. A refusal leaves the remaining keys for the next pass.
    pub async fn gc(&self, max_deletes: usize) -> Result<GcStats, StoreError> {
        let Some(head) = self.current_hash().await? else {
            return Ok(GcStats::default());
        };
        let current_toc = cfg_toc(&head);
        // The current toc must exist; if the head moved between these two reads,
        // the next pass will see the newer one.
        if !matches!(
            self.handle.get_stale(current_toc.as_bytes()).await,
            Ok(Some(_))
        ) {
            return Ok(GcStats::default());
        }

        let toc_prefix = format!("{CFG_PREFIX}toc/");
        let retain_key = ctl_gc_retain();
        let retain_raw = self
            .handle
            .get_stale(retain_key.as_bytes())
            .await
            .ok()
            .flatten()
            .map(|v| String::from_utf8_lossy(&v).into_owned())
            .unwrap_or_default();
        let mut keep: HashSet<Vec<u8>> = retain_raw
            .lines()
            .filter(|h| !h.is_empty())
            .map(|h| cfg_toc(h).into_bytes())
            .collect();
        keep.insert(current_toc.clone().into_bytes());
        let mut old_tocs = self.scan_prefix(toc_prefix.as_bytes()).await?;
        old_tocs.retain(|k| !keep.contains(k));

        // A concurrent publish (from another node: this node's write lock does
        // not cover the leader) may have moved the head after the scan. Abort
        // this pass rather than delete a toc the new head now names.
        if self.current_hash().await? != Some(head.clone()) {
            return Ok(GcStats {
                old_tocs: old_tocs.len(),
                ..GcStats::default()
            });
        }

        let mut stats = GcStats {
            old_tocs: old_tocs.len(),
            unreferenced_entities: 0,
            ..GcStats::default()
        };
        for key in old_tocs.into_iter().take(max_deletes) {
            self.handle.delete(&key).await.map_err(map_err)?;
            stats.toc_deleted += 1;
        }
        Ok(stats)
    }

    /// All keys under `prefix`, paged because the server clamps one read to 10k.
    async fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
        let end = prefix_successor(prefix);
        let mut start = prefix.to_vec();
        let mut out = Vec::new();
        loop {
            let (rows, _, truncated) = self
                .handle
                .get_stale_range(&start, end.as_deref().unwrap_or(&[]), 10_000)
                .await
                .map_err(map_err)?;
            for (key, _) in rows {
                if key.starts_with(prefix) {
                    out.push(key);
                }
            }
            if !truncated {
                break;
            }
            let Some(last) = out.last() else { break };
            let mut next = last.clone();
            next.push(0);
            if !next.starts_with(prefix) {
                break;
            }
            start = next;
        }
        Ok(out)
    }

    /// Publish `tree`: upload missing entities in bounded chunks, then commit toc + head in
    /// one final atomic batch.
    ///
    /// # Errors
    /// [`StoreError::NotLeader`] when this node does not own the commit point,
    /// [`StoreError::Keys`] for a malformed tree, [`StoreError::Refused`] when a chunk or the
    /// final batch overruns a library bound (that batch enters nothing; the head never moves),
    /// [`StoreError::Arachne`] for a transport or quorum failure.
    pub async fn publish(&self, tree: &ConfigTree) -> Result<String, StoreError> {
        let outcome = self.publish_inner(tree).await;
        // One record per publish, with the outcome the operator's rule is written
        // against. The SIZE comes from the tree that was SENT, not from what the
        // cluster stored: it is the growth series for the capacity question
        // (ADR-0001 risk R2), and it must move even when the commit is refused.
        crate::admin::metrics::record_arachne_publish(match &outcome {
            Ok(_) => "ok",
            Err(StoreError::NotLeader) => "not_leader",
            Err(StoreError::QuorumUnavailable) => "quorum_unavailable",
            // The capacity alarm: a pre-propose bound refusal is a fact about the
            // config, not a transport or codec failure — `refused`, like the encoder's
            // own refusal in `ConfigPublisher::publish`.
            Err(StoreError::Refused(_)) => "refused",
            Err(_) => "error",
        });
        if let Err(e) = &outcome {
            note_quorum_failure("publish", e);
        }
        if outcome.is_ok() {
            crate::admin::metrics::record_arachne_config_bytes(tree.values().map(Vec::len).sum());
        }
        outcome
    }

    /// [`Self::publish`] without the instrumentation, so the counting happens once.
    async fn publish_inner(&self, tree: &ConfigTree) -> Result<String, StoreError> {
        // The toc first: it validates every id, so an unkeyable entity is refused before a single
        // byte is sent to the cluster.
        let toc = toc_for(tree)?;
        let toc_hash = toc.hash();
        // THE REDIRECTING HANDLE, on purpose (2026-10-05, user ruling).
        //
        // This used to be `without_redirect()`, chosen in T2.2 when `put` on a follower returned
        // `NotLeader` and that refusal was the only available write protection — the plan even
        // recorded it as "leader 判定与写保护的原语". Upstream 0.1.2 changed the premise: a
        // follower's `put` is now forwarded to the leader (probe p11), and the ruling that a
        // management write is executed wherever it lands (any node runs the write path and
        // publishes) is what makes publishing accessible from every node. Keeping
        // `without_redirect()` here made the whole cluster's publish leader-only, which three
        // real nodes demonstrated immediately: 2 of 3 writes failed with `NotLeader`.
        //
        // `multi_put` is forwarded the same way (`propose_with_redirect`): the publisher need
        // not be the leader, and the WHOLE batch crosses as one command — entities, toc and
        // head apply in a single log entry, so there is no sequence left to be half-done.
        let writer = &self.handle;

        // Why no baseline read: with content-addressed entity keys, "is this entity already
        // stored?" is answered by the KEY ITSELF (`hydra/cfg/e/<path>/<hash>`), and can be checked
        // against this node's own replica without a round trip and without trusting a possibly
        // stale view of the head. The earlier design read the committed toc and compared hashes
        // against it; that needed a baseline that a lagging node cannot always supply, and a stale
        // baseline could make it SKIP writing an entity whose key had been collected.
        //
        // The check costs one LOCAL read per entity and saves a raft log entry per unchanged
        // entity — the property the key-path design was chosen for.
        //
        // `get_stale` on our own replica: a "missing" answer from a lagging node only costs a
        // redundant, idempotent entry in the batch, while a "present" answer cannot be wrong about
        // the CONTENT (the key it answered for is derived from that content's hash). And hydra
        // never deletes an entity, so "present" cannot become "gone" between this read and the
        // batch commit either — nothing can invalidate the skip.
        let mut missing: Vec<(String, &[u8])> = Vec::new();
        for (path, bytes) in tree.iter() {
            let key = cfg_entity(path, &hex_hash(&content_hash(bytes)));
            if matches!(self.handle.get_stale(key.as_bytes()).await, Ok(Some(_))) {
                continue;
            }
            missing.push((key, bytes.as_slice()));
        }

        let toc_bytes = toc.encode();
        let toc_key = cfg_toc(&toc_hash);
        let head_key = ctl_head();
        // Keep a bounded HISTORY of toc hashes in the same commit batch. A
        // materializer that read head H while H+1 publishes needs H's toc to
        // exist long enough to apply; GC may delete only what is outside this
        // window. 64 covers a follower catching up across ~1 minute of writes.
        let retain_key = ctl_gc_retain();
        let previous_retain = self
            .handle
            .get_stale(retain_key.as_bytes())
            .await
            .ok()
            .flatten()
            .map(|v| String::from_utf8_lossy(&v).into_owned())
            .unwrap_or_default();
        let mut retain: Vec<String> = vec![toc_hash.clone()];
        for h in previous_retain.lines().filter(|h| !h.is_empty()) {
            if h != toc_hash && retain.len() < 64 {
                retain.push(h.to_string());
            }
        }
        let retain_bytes = retain.join("\n").into_bytes();

        // PRE-UPLOAD the missing entities in bounded chunks, WITHOUT the head.
        //
        // This is safe because entity keys are CONTENT-ADDRESSED: a value is stored under the
        // hash of its own bytes, so writing it cannot overwrite another entity, and the key is
        // idempotent (re-uploading the same bytes writes the same key/value). Nothing names
        // these entities until the final batch moves the head, so a failure here leaves the old
        // head (and therefore the old tree) current; already-uploaded values are harmless
        // orphans that GC collects. Two concurrent publishers writing different versions of one
        // entity use different keys, so neither can corrupt the other's tree.
        //
        // Chunking exists because a ~12k-entity bulk config is far past the library's
        // 4096-entry / 4 MiB `multi_put` bound. Both bounds are enforced per chunk; an entity
        // that alone exceeds the byte budget gets a chunk to itself and is left to the
        // library's own per-value/total caps to judge.
        for chunk in chunk_missing_entities(&missing) {
            // A pre-propose bound refusal is a fact about THIS CHUNK, not a cluster failure.
            // Each call site turns `InvalidArgument` into `StoreError::Refused`, which
            // `publish` counts as `result="refused"` (the capacity alarm) instead of `error`.
            writer.multi_put(&chunk).await.map_err(|e| match e {
                ArachneError::InvalidArgument(msg) => StoreError::Refused(msg),
                other => map_err(other),
            })?;
        }

        // FINAL BATCH = the commit point. toc + gc_retain + head land in ONE log entry (all or
        // nothing), so a reader sees either the old tree or the new one: the head moves together
        // with the toc it names, and every entity that toc references was already uploaded
        // above. Never empty — the library refuses empty batches — and the head value is still
        // the toc's content hash (the toc's bytes ARE the tree's name).
        let batch: Vec<(&[u8], &[u8])> = vec![
            (toc_key.as_bytes(), toc_bytes.as_slice()),
            (retain_key.as_bytes(), retain_bytes.as_slice()),
            (head_key.as_bytes(), toc_hash.as_bytes()),
        ];
        // Same `InvalidArgument` → `Refused` discrimination as the chunks above: the library
        // validates this batch BEFORE propose, so a bound overrun (e.g. a toc over the per-value
        // or total-byte cap) commits nothing and the previous head stays current.
        writer.multi_put(&batch).await.map_err(|e| match e {
            ArachneError::InvalidArgument(msg) => StoreError::Refused(msg),
            other => map_err(other),
        })?;

        Ok(toc_hash)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn prefix_successor_is_exclusive_and_handles_ff() {
        assert_eq!(super::prefix_successor(b"ab"), Some(b"ac".to_vec()));
        assert_eq!(super::prefix_successor(b"a\xff"), Some(b"b".to_vec()));
        assert_eq!(super::prefix_successor(b"\xff\xff"), None);
    }

    /// Every chunk stays inside both `multi_put` bounds, and the helper never
    /// emits an empty chunk (the library refuses those).
    #[test]
    fn entity_chunks_respect_entry_and_byte_bounds() {
        assert!(super::chunk_missing_entities(&[]).is_empty());

        // 600 KiB each: three fit in the 2 MiB soft budget, a fourth does not.
        let value = vec![b'x'; 600 * 1024];
        let missing: Vec<(String, &[u8])> = (0..5)
            .map(|i| (format!("key-{i}"), value.as_slice()))
            .collect();
        let chunks = super::chunk_missing_entities(&missing);
        let sizes: Vec<usize> = chunks.iter().map(Vec::len).collect();
        assert_eq!(sizes, vec![3, 2]);
        for chunk in &chunks {
            let bytes: usize = chunk.iter().map(|(k, v)| k.len() + v.len()).sum();
            assert!(bytes <= super::ENTITY_CHUNK_MAX_BYTES);
        }

        // Entry-count bound: 2049 tiny entities become 2048 + 1.
        let tiny: Vec<(String, &[u8])> = (0..=super::ENTITY_CHUNK_MAX_ENTRIES)
            .map(|i| (format!("k{i}"), b"v".as_slice()))
            .collect();
        let chunks = super::chunk_missing_entities(&tiny);
        let sizes: Vec<usize> = chunks.iter().map(Vec::len).collect();
        assert_eq!(sizes, vec![super::ENTITY_CHUNK_MAX_ENTRIES, 1]);
    }

    /// An entity bigger than the chunk budget still makes progress: it becomes
    /// a one-entry chunk and the library's own caps decide whether it fits.
    #[test]
    fn an_entity_larger_than_the_byte_budget_gets_its_own_chunk() {
        let value = vec![b'x'; super::ENTITY_CHUNK_MAX_BYTES + 1];
        let missing = vec![("oversized".to_string(), value.as_slice())];
        let chunks = super::chunk_missing_entities(&missing);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 1);
    }

    use super::*;

    fn tree(entries: &[(&str, &[u8])]) -> ConfigTree {
        entries
            .iter()
            .map(|(id, bytes)| (EntityPath::Tenant((*id).to_string()), bytes.to_vec()))
            .collect()
    }

    /// The first publish has nothing to compare against, so everything is new.
    ///
    /// The toc describes exactly the tree it is built from, and the toc is what the reader
    /// verifies against — so an entry that does not describe its own bytes would make every read
    /// fail.
    #[test]
    fn the_toc_describes_exactly_the_tree_it_names() {
        let t = tree(&[("a", b"one"), ("b", b"two")]);
        let toc = toc_for(&t).expect("toc");
        for entry in &toc.entities {
            let bytes = t.get(&entry.path).expect("every entity is in the tree");
            assert_eq!(
                entry.content_hash,
                content_hash(bytes),
                "entry {} does not describe its own bytes",
                entry.path.to_key_segment()
            );
            assert_eq!(entry.len as usize, bytes.len());
        }
        assert_eq!(toc.entities.len(), t.len());
    }

    /// One entity's key contains its own content hash, so two versions of it coexist.
    ///
    /// THIS IS THE PROPERTY THAT MAKES CONCURRENT PUBLISHING SAFE. Two nodes publishing at the
    /// same time write different keys for the same entity when their content differs, so whichever
    /// head lands last names a tree whose entities are all present and all match their recorded
    /// hashes. With the previous path-only keys the loser's write landed on the winner's key and
    /// every reader refused the tree.
    #[test]
    fn two_versions_of_one_entity_get_two_keys() {
        let v1 = tree(&[("a", b"one")]);
        let v2 = tree(&[("a", b"TWO")]);
        let key = |t: &ConfigTree| {
            cfg_entity(
                &EntityPath::Tenant("a".into()),
                &hex_hash(&content_hash(
                    t.get(&EntityPath::Tenant("a".into())).expect("entity"),
                )),
            )
        };
        assert_ne!(
            key(&v1),
            key(&v2),
            "two different contents for one path must not share a key, or an interleaved \
             publish would leave the winner's toc describing bytes that are no longer there"
        );
        // ...and the SAME content keeps one key, which is what makes "already stored" decidable
        // and an unchanged entity un-rewritten.
        assert_eq!(key(&v1), key(&tree(&[("a", b"one")])));
    }

    /// An id that cannot be keyed must be refused BEFORE anything reaches the cluster, because a
    /// `/` would write into another entity's namespace.
    ///
    /// Falsification: drop `validate_entities` from `toc_for` and this returns `Ok`.
    #[test]
    fn an_unkeyable_entity_id_is_refused_before_any_write() {
        let bad = tree(&[("../../ctl/head", b"x")]);
        let got = toc_for(&bad);
        assert!(
            matches!(got, Err(KeysError::IdHasSeparator { .. })),
            "a path-bearing id must be refused, got {got:?}"
        );
    }
}
