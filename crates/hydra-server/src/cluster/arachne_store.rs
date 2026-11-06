//! # The config commit point (ADR-0001, plan T2.2; sharded V2)
//!
//! This is the **only** place allowed to write [`crate::cluster::arachne_keys::ctl_head`].
//! Everything else reads through it. That single-writer rule is what makes the
//! head a commit point rather than a label:
//!
//! ```text
//! publish(tree) =
//!   1. build the whole toc, split it into 32 fixed buckets (format-7 shards)
//!   2. multi_put_with_index([missing entities + missing shards])  … bounded chunks, no head
//!   3. multi_put_with_index([manifest, gc_retain, head]) <- ONE log entry = the commit
//! ```
//!
//! The **head is a token**, not necessarily a bare hash: `m:<manifest-hash>` names the sharded
//! era, while a bare 64-hex value is a legacy format-6 tree. The manifest is one small value
//! committed atomically with the head; it names 32 content-addressed shards. This is what keeps
//! a ~24k-entity TOC below the library's per-value cap without a profile change.
//!
//! A bulk config can have far more entities than the library's 4096-entry / 4 MiB single-batch
//! bound, so missing entities AND missing shards are pre-uploaded in bounded chunks. The final
//! batch (manifest + retain + head) is one log entry, one session, one apply, all or nothing: a
//! failed publish leaves the previous tree current, and an over-`SHARD_MAX_BYTES` shard is
//! refused at PLAN time before any pre-upload.
//!
//! Uploading before the head is safe because entity/shard keys are content-addressed: a value is
//! stored under the hash of its own bytes, so it cannot overwrite another value, and nothing
//! names it until the head moves. A failed chunk therefore leaves only unreferenced (GC-able)
//! orphans while the old head and tree stay exactly as they were.
//!
//! ## Reading (Arachne 0.5)
//!
//! `read` is the 0.5 materialize recipe: read the head WITH its index, `wait_applied(head_index)`
//! locally, then `multi_get(Stale)` in bounded batches for manifest → shards → entities. Two
//! consequences, both deliberate:
//!
//! * a follower can materialize without a linearizable read or a quorum: `wait_applied` is local
//!   and `multi_get(Stale)` works on a minority follower (ADR-0001 §10 F-2);
//! * after the head index has applied, every earlier entity/shard write is applied too, so a
//!   missing key is a real inconsistency — an error, never a retry loop. A reader that catches
//!   the cluster mid-publish sees EITHER the old tree or the new one, never a mix.
//!
//! ## Upgrade and garbage collection
//!
//! Format 6 is still readable through the bare-token fallback, but a new publish never writes
//! `cfg_toc`; the upgrade is one-way (old builds cannot read `m:`). GC resolves each retain token
//! by prefix, keeps current+retained manifests/tocs, and deletes only old trampoline values.
//! **Shards are deliberately not scanned or deleted** (deferred shard GC; see [`ArachneConfigStore::gc`]),
//! so they leak until a reference census exists. `ctl_format` is documented but has no writer:
//! the format is self-describing in the toc values.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arachne_kv::client::{ArachneError, Handle};
use arachne_kv::{Index, ReadConsistency};

use super::arachne_keys::{
    cfg_entity, cfg_manifest, cfg_shard, cfg_toc, content_hash, ctl_gc_retain, ctl_head,
    head_manifest_hash, hex_hash, manifest_head, shard_bucket, validate_entities, EntityPath,
    KeysError, Manifest, Toc, TocEntry, CFG_PREFIX, SHARD_COUNT, SHARD_MAX_BYTES, TOC_FORMAT,
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
    /// overrun (a value over `max_value_bytes`, or a `multi_put_with_index` batch over the
    /// entry-count / total-byte caps). The refused batch commits nothing, and the
    /// head/tree are untouched; entities uploaded by an earlier chunk can remain as
    /// unreferenced, GC-able orphans.
    ///
    /// Produced at plan time when a shard exceeds [`SHARD_MAX_BYTES`], and at the
    /// `multi_put_with_index` call sites in [`Self::publish_inner`] (one per entity/shard chunk, plus the final
    /// manifest/head batch), which discriminate `ArachneError::InvalidArgument` for the batch
    /// they assembled (2026-10-10: the shared `map_err` that serves the read path deliberately
    /// does NOT map `InvalidArgument` here — a bound refusal is a fact about a batch).
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

/// How long a materializer waits for the local state machine to apply the head index before
/// reading its manifest/shards/entities. `wait_applied` is local (no quorum), so this is a
/// convergence bound, not a leadership/barrier bound.
const MATERIALIZE_WAIT: Duration = Duration::from_secs(10);

/// How long the publisher waits for its own final batch to apply before
/// checking that `ctl/head` actually names the tree it just wrote. This is the
/// 0.5.1 read-back guard: a stale/duplicate session must not turn into a
/// silent "success" whose head is still the previous generation.
const PUBLISH_READBACK_WAIT: Duration = Duration::from_secs(5);

/// Entities per `multi_get` batch during materialization. 24k entities therefore cost ~94 read
/// commands, inside the 0.5 integration target (~100) and below the 4096-key/4 MiB caps.
const ENTITY_READ_BATCH: usize = 256;

/// Prefix and snapshot bound for the materializer's watch cache. Arachne's
/// `watch` returns a consistent snapshot plus ordered events; values are
/// content-addressed, so a cached key is immutable even if the cache is older
/// than the current head. Missing keys fall back to multi_get.
const WATCH_PREFIX: &[u8] = b"hydra/cfg/";
const WATCH_SNAPSHOT_LIMIT: usize = 100_000;
/// Hard bound for the best-effort watch cache. The normal path prunes to the
/// current tree; this bound covers bursts between materializations so a long
/// running process cannot grow the cache without limit.
const WATCH_CACHE_MAX_ENTRIES: usize = 200_000;

/// Shard keys per `multi_get` batch. 32 buckets fit in two batches even before de-duplicating
/// the shared empty shard; each batch also stays well below the 4 MiB returned-value cap at the
/// 24k target (~34 KiB per shard).
const SHARD_READ_BATCH: usize = 16;

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
        // and a pre-propose bound refusal is a fact about a `multi_put_with_index` BATCH, not about a
        // read. The `Refused` discrimination therefore lives at the `multi_put_with_index` call sites in
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

/// Split a whole toc into the fixed manifest/shards a sharded publish commits.
///
/// The buckets are fixed by [`shard_bucket`] (explicit FNV-1a), not by insertion order, so every
/// publisher derives the same shard for the same path. Every shard is format [`TOC_FORMAT`], even
/// an empty one: the empty shard is a real content-addressed value shared by every empty bucket.
///
/// **Plan-time refusal:** a shard whose encoded bytes exceed [`SHARD_MAX_BYTES`] rejects the whole
/// publish here, before any entity/shard is uploaded. Splitting further would change the manifest's
/// fixed 32-bucket contract; the fix is a smaller config, which is the same capacity answer as the
/// other pre-propose refusals.
///
/// # Errors
/// [`StoreError::Keys`] if a shard toc cannot be built; [`StoreError::Refused`] if a shard is over
/// [`SHARD_MAX_BYTES`].
pub fn split_shards(toc: &Toc) -> Result<(Manifest, Vec<Toc>), StoreError> {
    let mut buckets: Vec<Vec<TocEntry>> = (0..SHARD_COUNT).map(|_| Vec::new()).collect();
    for entry in &toc.entities {
        buckets[shard_bucket(&entry.path)].push(entry.clone());
    }

    let mut shards = Vec::with_capacity(SHARD_COUNT);
    let mut digests = Vec::with_capacity(SHARD_COUNT);
    for (index, entities) in buckets.into_iter().enumerate() {
        let shard = Toc::new(TOC_FORMAT, entities)?;
        let encoded_len = shard.encode().len();
        if encoded_len > SHARD_MAX_BYTES {
            return Err(StoreError::Refused(format!(
                "toc shard {index} encodes to {encoded_len} bytes, over SHARD_MAX_BYTES \
                 ({SHARD_MAX_BYTES}); shard the config more coarsely or shrink it"
            )));
        }
        digests.push(shard.digest());
        shards.push(shard);
    }

    let entry_count = toc.entities.len() as u32;
    let entity_bytes = toc.entities.iter().map(|e| u64::from(e.len)).sum::<u64>();
    let manifest = Manifest::new(digests, entry_count, entity_bytes)?;
    Ok((manifest, shards))
}

/// The commit point for the config tree.
///
/// `Clone` is cheap (the handle wraps an `Arc` to the node's command channel) and exists so a
/// materializer and a publisher can share one node without either owning it.
#[derive(Clone)]
pub struct ArachneConfigStore {
    handle: Handle,
    /// Content-addressed entity values seen by this store's materializer. Entity
    /// keys include their content hash, so a hit is immutable and can be reused
    /// across trees; the map is pruned to the current tree after each read.
    entity_cache: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    /// Content-addressed TOC shard values. Shards are immutable, so unchanged
    /// buckets are reused across trees; the map is pruned to the current
    /// manifest's digests after each read.
    shard_cache: Arc<Mutex<HashMap<[u8; 32], Vec<u8>>>>,
    /// Best-effort event-driven cache of `hydra/cfg/` values. The watch is
    /// started lazily; read paths use a key when present and fall back to
    /// multi_get when absent. Only content-addressed, immutable keys are stored.
    watch_cache: Arc<WatchCache>,
}

struct WatchCache {
    values: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    started: AtomicBool,
}

impl WatchCache {
    fn new() -> Self {
        Self {
            values: Mutex::new(HashMap::new()),
            started: AtomicBool::new(false),
        }
    }

    /// Keep only keys referenced by the tree this materializer just read.
    fn prune_to(&self, keep: &HashSet<Vec<u8>>) {
        let mut values = self.values.lock().unwrap_or_else(|e| e.into_inner());
        values.retain(|key, _| keep.contains(key));
    }
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
    /// Old legacy toc values deleted in this pass.
    pub toc_deleted: usize,
    /// Old manifest values deleted in this pass.
    pub manifest_deleted: usize,
    /// Unreferenced entity values deleted in this pass.
    pub entities_deleted: usize,
    /// Old legacy toc values that existed when the pass started.
    pub old_tocs: usize,
    /// Old manifest values that existed when the pass started.
    pub old_manifests: usize,
    /// Unreferenced entity values that existed when the pass started.
    pub unreferenced_entities: usize,
}

impl GcStats {
    #[must_use]
    pub fn deleted(&self) -> usize {
        self.toc_deleted + self.manifest_deleted + self.entities_deleted
    }

    #[must_use]
    pub fn remaining(&self) -> usize {
        self.old_tocs
            .saturating_sub(self.toc_deleted)
            .saturating_add(self.old_manifests.saturating_sub(self.manifest_deleted))
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
        Self {
            handle,
            entity_cache: Arc::new(Mutex::new(HashMap::new())),
            shard_cache: Arc::new(Mutex::new(HashMap::new())),
            watch_cache: Arc::new(WatchCache::new()),
        }
    }

    /// Start the prefix watch once. Failure is non-fatal: every read still has
    /// the multi_get fallback, and a later read retries the subscription.
    async fn ensure_watch(&self) {
        if self.watch_cache.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let handle = self.handle.clone();
        let cache = self.watch_cache.clone();
        match handle.watch(WATCH_PREFIX, WATCH_SNAPSHOT_LIMIT).await {
            Ok(sub) => {
                {
                    let mut values = cache.values.lock().unwrap_or_else(|e| e.into_inner());
                    values.clear();
                    values.extend(sub.snapshot);
                }
                let mut events = sub.events;
                tokio::spawn(async move {
                    while let Some(event) = events.recv().await {
                        let mut values = cache.values.lock().unwrap_or_else(|e| e.into_inner());
                        match event.value {
                            Some(value) => {
                                values.insert(event.key, value);
                            }
                            None => {
                                values.remove(&event.key);
                            }
                        }
                        if values.len() > WATCH_CACHE_MAX_ENTRIES {
                            values.clear();
                            tracing::warn!(
                                "config watch cache exceeded {WATCH_CACHE_MAX_ENTRIES} entries;                                  dropping it and falling back to multi_get"
                            );
                        }
                    }
                    cache.started.store(false, Ordering::Release);
                });
            }
            Err(e) => {
                cache.started.store(false, Ordering::Release);
                tracing::warn!(error = %e, "config watch cache unavailable; using multi_get fallback");
            }
        }
    }

    fn cached_content(&self, key: &str) -> Option<Vec<u8>> {
        self.watch_cache
            .values
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key.as_bytes())
            .cloned()
    }

    /// The tree the head currently names, or [`ReadOutcome::Empty`].
    ///
    /// # Errors
    /// [`StoreError`] when the cluster cannot answer, the head index cannot be applied within
    /// [`MATERIALIZE_WAIT`], or what it returned does not match what the toc describes. After
    /// `wait_applied` a mismatch/missing key is real inconsistency, not a lag to retry.
    pub async fn read(&self) -> Result<ReadOutcome, StoreError> {
        let outcome = self.read_inner().await;
        if let Err(e) = &outcome {
            note_quorum_failure("read", e);
        }
        outcome
    }

    /// [`Self::read`] without the instrumentation, so the counting happens once.
    async fn read_inner(&self) -> Result<ReadOutcome, StoreError> {
        self.ensure_watch().await;
        // Read the head WITH its index, then wait for THIS node's state machine to apply that
        // index before reading anything it names. Every manifest/shard/entity write was committed
        // before the head entry, so "head applied locally" implies they are readable locally.
        // `wait_applied` is local (no quorum) and event-driven, not a poll.
        let Some((head, head_index)) = self.current_head_with_index().await? else {
            return Ok(ReadOutcome::Empty);
        };
        tracing::debug!(
            target: "hydra::cluster",
            head = %head,
            head_index,
            "materializer head read"
        );
        self.handle
            .wait_applied(head_index, MATERIALIZE_WAIT)
            .await
            .map_err(map_err)?;

        // Head tokens are self-describing: `m:<manifest-hash>` is the sharded era, anything else
        // is a legacy bare toc hash. The format inside each value is checked as well, so a mixed
        // or forged key cannot make the decoder guess.
        let (toc, mut keep) = if let Some(manifest_hash) = head_manifest_hash(&head) {
            let manifest_bytes = self
                .multi_get_one(&cfg_manifest(manifest_hash))
                .await?
                .ok_or_else(|| {
                    StoreError::Arachne(format!(
                        "the head names manifest {manifest_hash} but it is missing after wait_applied({head_index}); refusing to serve a mixed tree"
                    ))
                })?;
            if hex_hash(&content_hash(&manifest_bytes)) != manifest_hash {
                return Err(StoreError::Arachne(format!(
                    "the head names manifest {manifest_hash} but the stored bytes hash to {}; refusing to serve a mixed tree",
                    hex_hash(&content_hash(&manifest_bytes))
                )));
            }
            let manifest = Manifest::decode(&manifest_bytes)?;
            let mut keep = HashSet::new();
            keep.insert(cfg_manifest(manifest_hash).into_bytes());
            for digest in &manifest.shards {
                keep.insert(cfg_shard(&hex_hash(digest)).into_bytes());
            }
            let toc = self.read_shards(&head, &manifest).await?;
            (toc, keep)
        } else {
            let toc_bytes = self
                .multi_get_one(&cfg_toc(&head))
                .await?
                .ok_or_else(|| {
                    StoreError::Arachne(format!(
                        "the legacy head names tree {head} but its table of contents is missing after wait_applied({head_index}); refusing to serve a mixed tree"
                    ))
                })?;
            let mut keep = HashSet::new();
            keep.insert(cfg_toc(&head).into_bytes());
            (Toc::decode(&toc_bytes)?, keep)
        };

        for entry in &toc.entities {
            keep.insert(cfg_entity(&entry.path, &hex_hash(&entry.content_hash)).into_bytes());
        }
        let tree = self.read_entities(&head, &toc.entities).await?;
        self.watch_cache.prune_to(&keep);
        Ok(ReadOutcome::Tree(Arc::new(tree)))
    }

    /// A one-key `multi_get(Stale)`, so manifest/toc reads use the same batch API as the rest of
    /// the 0.5 materialize path.
    async fn multi_get_one(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        if let Some(value) = self.cached_content(key) {
            return Ok(Some(value));
        }
        let result = self
            .handle
            .multi_get(&[key.as_bytes()], ReadConsistency::Stale)
            .await
            .map_err(map_err)?;
        Ok(result.values.into_iter().next().flatten())
    }

    /// Fetch entity values in bounded `multi_get(Stale)` batches.
    ///
    /// The caller has already waited for the head index to apply locally, so a missing value is
    /// not a race to retry: it is a missing tree and is refused. Verification is unchanged —
    /// length and content hash must match the toc entry exactly.
    async fn read_entities(
        &self,
        head: &str,
        entries: &[TocEntry],
    ) -> Result<ConfigTree, StoreError> {
        let mut tree = ConfigTree::new();
        let mut missing: Vec<(String, TocEntry)> = Vec::new();
        let current_keys: HashSet<String> = entries
            .iter()
            .map(|entry| cfg_entity(&entry.path, &hex_hash(&entry.content_hash)))
            .collect();

        {
            let mut cache = self.entity_cache.lock().unwrap_or_else(|e| e.into_inner());
            for entry in entries {
                let key = cfg_entity(&entry.path, &hex_hash(&entry.content_hash));
                if let Some(bytes) = self.cached_content(&key) {
                    if bytes.len() == entry.len as usize
                        && content_hash(&bytes) == entry.content_hash
                    {
                        tree.insert(entry.path.clone(), bytes);
                        continue;
                    }
                }
                if let Some(bytes) = cache.get(&key) {
                    if bytes.len() == entry.len as usize
                        && content_hash(bytes) == entry.content_hash
                    {
                        tree.insert(entry.path.clone(), bytes.clone());
                        continue;
                    }
                    cache.remove(&key);
                }
                missing.push((key, entry.clone()));
            }
        }

        let mut fetched: Vec<(String, Vec<u8>)> = Vec::new();
        for chunk in missing.chunks(ENTITY_READ_BATCH) {
            let keys: Vec<String> = chunk.iter().map(|(key, _)| key.clone()).collect();
            let key_refs: Vec<&[u8]> = keys.iter().map(String::as_bytes).collect();
            let result = self
                .handle
                .multi_get(&key_refs, ReadConsistency::Stale)
                .await
                .map_err(map_err)?;
            for ((key, entry), value) in chunk.iter().zip(result.values) {
                let Some(bytes) = value else {
                    return Err(StoreError::Arachne(format!(
                        "tree {head} is missing entity {} after wait_applied; refusing to serve a mixed tree",
                        entry.path.to_key_segment()
                    )));
                };
                if bytes.len() != entry.len as usize || content_hash(&bytes) != entry.content_hash {
                    return Err(StoreError::Arachne(format!(
                        "entity {} in tree {head} does not match the hash its table of contents \
                         records; refusing to serve a mixed tree",
                        entry.path.to_key_segment()
                    )));
                }
                tree.insert(entry.path.clone(), bytes.clone());
                fetched.push((key.clone(), bytes));
            }
        }

        let mut cache = self.entity_cache.lock().unwrap_or_else(|e| e.into_inner());
        for (key, bytes) in fetched {
            cache.insert(key, bytes);
        }
        cache.retain(|key, _| current_keys.contains(key));
        Ok(tree)
    }

    /// Fetch, verify, and merge the shards a manifest names.
    ///
    /// Identical digests are fetched once: every empty bucket encodes the same empty format-7 toc,
    /// so a 3-entity tree legitimately names that one value 29+ times. Unique keys are read with
    /// `multi_get(Stale)` in bounded batches, then every shard is content checked before decode,
    /// every entry is pinned to its bucket, and the manifest's totals are re-derived from the
    /// merged entries — a manifest that disagrees with its shards (count, entity bytes, duplicate
    /// paths) is refused rather than served.
    async fn read_shards(&self, head: &str, manifest: &Manifest) -> Result<Toc, StoreError> {
        // One key per DISTINCT digest. Deduplicating first is what makes the 32-bucket manifest
        // cheap for a tiny tree (29+ empty buckets share one digest).
        let mut seen = HashSet::with_capacity(SHARD_COUNT);
        let mut unique: Vec<([u8; 32], String)> = Vec::with_capacity(SHARD_COUNT);
        for digest in &manifest.shards {
            if seen.insert(*digest) {
                unique.push((*digest, cfg_shard(&hex_hash(digest))));
            }
        }

        let mut fetched: HashMap<[u8; 32], Vec<u8>> = HashMap::with_capacity(unique.len());
        let mut pending: Vec<([u8; 32], String)> = Vec::new();
        {
            let mut cache = self.shard_cache.lock().unwrap_or_else(|e| e.into_inner());
            for (digest, key) in unique {
                if let Some(bytes) = self.cached_content(&key) {
                    if content_hash(&bytes) == digest {
                        fetched.insert(digest, bytes);
                        continue;
                    }
                }
                if let Some(bytes) = cache.get(&digest) {
                    if content_hash(bytes) == digest {
                        fetched.insert(digest, bytes.clone());
                        continue;
                    }
                    cache.remove(&digest);
                }
                pending.push((digest, key));
            }
        }
        for chunk in pending.chunks(SHARD_READ_BATCH) {
            let keys: Vec<&[u8]> = chunk.iter().map(|(_, key)| key.as_bytes()).collect();
            let result = self
                .handle
                .multi_get(&keys, ReadConsistency::Stale)
                .await
                .map_err(map_err)?;
            for ((digest, shard_hash), value) in chunk.iter().zip(result.values) {
                let Some(bytes) = value else {
                    return Err(StoreError::Arachne(format!(
                        "manifest {head} names shard {shard_hash} but it is missing after \
                         wait_applied; refusing to serve a mixed tree"
                    )));
                };
                if content_hash(&bytes) != *digest {
                    return Err(StoreError::Arachne(format!(
                        "shard {shard_hash} of manifest {head} does not match the digest the \
                         manifest records; refusing to serve a mixed tree"
                    )));
                }
                fetched.insert(*digest, bytes);
            }
        }

        // Decode each distinct shard exactly once.
        let mut decoded: HashMap<[u8; 32], Vec<TocEntry>> = HashMap::with_capacity(fetched.len());
        for (digest, bytes) in &fetched {
            let shard = Toc::decode_shard(bytes)?;
            decoded.insert(*digest, shard.entities);
        }

        let mut merged: Vec<TocEntry> = Vec::new();
        for (index, digest) in manifest.shards.iter().enumerate() {
            let entries = decoded.get(digest).ok_or_else(|| {
                StoreError::Arachne(format!(
                    "manifest {head} names a shard that was not fetched (bucket {index})"
                ))
            })?;
            for entry in entries {
                debug_assert_eq!(
                    shard_bucket(&entry.path),
                    index,
                    "shard {index} carries {:?}, which belongs in another bucket",
                    entry.path
                );
            }
            merged.extend(entries.iter().cloned());
        }

        if merged.len() != manifest.entry_count as usize {
            return Err(StoreError::Arachne(format!(
                "manifest {head} declares {} entries but its shards carry {}; refusing to serve \
                 a mixed tree",
                manifest.entry_count,
                merged.len()
            )));
        }
        let entity_bytes: u64 = merged.iter().map(|e| u64::from(e.len)).sum();
        if entity_bytes != manifest.entity_bytes {
            return Err(StoreError::Arachne(format!(
                "manifest {head} declares {} entity bytes but its shards carry {entity_bytes}; \
                 refusing to serve a mixed tree",
                manifest.entity_bytes
            )));
        }
        let mut unique_paths = HashSet::with_capacity(merged.len());
        for entry in &merged {
            if !unique_paths.insert(entry.path.clone()) {
                return Err(StoreError::Arachne(format!(
                    "manifest {head} names entity {} more than once; refusing to serve a mixed \
                     tree",
                    entry.path.to_key_segment()
                )));
            }
        }

        let mut shard_cache = self.shard_cache.lock().unwrap_or_else(|e| e.into_inner());
        for (digest, bytes) in &fetched {
            shard_cache.insert(*digest, bytes.clone());
        }
        let keep: HashSet<[u8; 32]> = manifest.shards.iter().copied().collect();
        shard_cache.retain(|digest, _| keep.contains(digest));

        Ok(Toc {
            format: TOC_FORMAT,
            entities: merged,
        })
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
    pub async fn current_head_with_index(&self) -> Result<Option<(String, Index)>, StoreError> {
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

    /// Delete up to `max_deletes` old trampoline values that the CURRENT tree does not name.
    ///
    /// Scans BOTH `hydra/cfg/manifest/` (sharded era) and `hydra/cfg/toc/` (legacy trees). The
    /// current head token resolves to one of those keys by its prefix (`m:` → manifest, bare →
    /// legacy toc), and every retained token is kept verbatim: a retain line is exactly a head
    /// token, so a mixed history of bare and `m:` lines survives an upgrade.
    ///
    /// **Shards are deliberately not scanned or deleted.** They are content-addressed and shared
    /// by many manifests, so deleting one is only safe with a full manifest reference census that
    /// a bounded pass cannot take; this leaks shard values until a dedicated shard GC exists.
    /// Entities are likewise still not collected here (the old code never did); only the small
    /// trampoline values (manifests/tocs) are reclaimed.
    ///
    /// A bounded pass lets a caller hold the publish lock for a short time; the next pass
    /// continues.
    ///
    /// # Errors
    /// [`StoreError`] when the head/current manifest or toc cannot be read or a delete is
    /// refused. A refusal leaves the remaining keys for the next pass.
    pub async fn gc(&self, max_deletes: usize) -> Result<GcStats, StoreError> {
        let Some(head) = self.current_hash().await? else {
            return Ok(GcStats::default());
        };
        let current_key = match head_manifest_hash(&head) {
            Some(manifest_hash) => cfg_manifest(manifest_hash),
            None => cfg_toc(&head),
        };
        // The current trampoline must exist; if the head moved between these two reads, the next
        // pass will see the newer one.
        if !matches!(
            self.handle.get_stale(current_key.as_bytes()).await,
            Ok(Some(_))
        ) {
            return Ok(GcStats::default());
        }

        let manifest_prefix = format!("{CFG_PREFIX}manifest/");
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
            .filter(|token| !token.is_empty())
            .map(|token| match head_manifest_hash(token) {
                Some(manifest_hash) => cfg_manifest(manifest_hash).into_bytes(),
                None => cfg_toc(token).into_bytes(),
            })
            .collect();
        keep.insert(current_key.clone().into_bytes());

        let mut old_manifests = self.scan_prefix(manifest_prefix.as_bytes()).await?;
        old_manifests.retain(|key| !keep.contains(key));
        let mut old_tocs = self.scan_prefix(toc_prefix.as_bytes()).await?;
        old_tocs.retain(|key| !keep.contains(key));

        // A concurrent publish (from another node: this node's write lock does
        // not cover the leader) may have moved the head after the scan. Abort
        // this pass rather than delete a manifest/toc the new head now names.
        if self.current_hash().await? != Some(head.clone()) {
            return Ok(GcStats {
                old_manifests: old_manifests.len(),
                old_tocs: old_tocs.len(),
                ..GcStats::default()
            });
        }

        let mut stats = GcStats {
            old_manifests: old_manifests.len(),
            old_tocs: old_tocs.len(),
            unreferenced_entities: 0,
            ..GcStats::default()
        };
        let mut budget = max_deletes;
        for key in old_manifests.into_iter().take(budget) {
            self.handle.delete(&key).await.map_err(map_err)?;
            stats.manifest_deleted += 1;
            budget -= 1;
        }
        for key in old_tocs.into_iter().take(budget) {
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

    /// Publish `tree`: split the toc into shards, pre-upload missing entities/shards in bounded
    /// chunks, then commit manifest + gc_retain + head in one final atomic batch.
    ///
    /// Returns the new head token (`m:<manifest-hash>`), never a bare legacy hash. Callers that
    /// need the tree generation should use [`Self::publish_with_index`].
    ///
    /// # Errors
    /// [`StoreError::NotLeader`] when this node does not own the commit point,
    /// [`StoreError::Keys`] for a malformed tree, [`StoreError::Refused`] when a shard exceeds
    /// [`SHARD_MAX_BYTES`] at plan time or a chunk/final batch overruns a library bound (that
    /// batch enters nothing; the head never moves), [`StoreError::Arachne`] for a transport or
    /// quorum failure.
    pub async fn publish(&self, tree: &ConfigTree) -> Result<String, StoreError> {
        self.publish_with_index(tree)
            .await
            .map(|(head, _index)| head)
    }

    /// [`Self::publish`] plus the **final log index** that committed the manifest and head.
    ///
    /// That index is the tree generation: every entity/shard write precedes the head write in
    /// the log, so a reader that waits for this index locally has everything the tree names.
    ///
    /// # Errors
    /// The same errors as [`Self::publish`].
    pub async fn publish_with_index(
        &self,
        tree: &ConfigTree,
    ) -> Result<(String, Index), StoreError> {
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

    /// [`Self::publish_with_index`] without the instrumentation, so counting happens once.
    async fn publish_inner(&self, tree: &ConfigTree) -> Result<(String, Index), StoreError> {
        // The toc first: it validates every id, so an unkeyable entity is refused before a single
        // byte is sent to the cluster. Then split it. A shard over SHARD_MAX_BYTES is refused
        // HERE, at plan time, before any entity is pre-uploaded.
        let toc = toc_for(tree)?;
        let (manifest, shards) = split_shards(&toc)?;
        let manifest_hash = manifest.hash();
        let manifest_bytes = manifest.encode();
        let manifest_key = cfg_manifest(&manifest_hash);
        let head = manifest_head(&manifest_hash);
        let head_key = ctl_head();

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
        let writer = &self.handle;

        // Pre-upload anything the head will name EXCEPT the manifest itself, all without the
        // head. Entity and shard keys are both content-addressed, so this is idempotent and
        // cannot overwrite another tree's value; nothing here is visible to a reader until the
        // final batch moves the head. A failure leaves only unreferenced, GC-able orphans.
        //
        // Entity skip: `get_stale` against this node's own replica. A "missing" answer only costs
        // a redundant idempotent write; a "present" answer cannot be wrong about CONTENT because
        // the key carries the hash. Hydra never deletes entities, so a skip cannot go stale.
        let mut missing: Vec<(String, &[u8])> = Vec::new();
        for (path, bytes) in tree.iter() {
            let key = cfg_entity(path, &hex_hash(&content_hash(bytes)));
            if matches!(self.handle.get_stale(key.as_bytes()).await, Ok(Some(_))) {
                continue;
            }
            missing.push((key, bytes.as_slice()));
        }

        // Shards are content-addressed too, and committed BEFORE the head. A shard that is not
        // already present is queued with the entities and goes through the same bounded chunk
        // helper. Empty buckets share one digest, so deduplicate first.
        let shard_bytes: Vec<Vec<u8>> = shards.iter().map(Toc::encode).collect();
        let mut seen_shards: HashSet<[u8; 32]> = HashSet::with_capacity(SHARD_COUNT);
        for (index, bytes) in shard_bytes.iter().enumerate() {
            let digest = manifest.shards[index];
            if !seen_shards.insert(digest) {
                continue;
            }
            let key = cfg_shard(&hex_hash(&digest));
            if matches!(self.handle.get_stale(key.as_bytes()).await, Ok(Some(_))) {
                continue;
            }
            missing.push((key, bytes.as_slice()));
        }

        // Keep a bounded HISTORY of HEAD TOKENS in the same commit batch. Tokens are carried
        // verbatim, so a mixed history of legacy bare hashes and `m:` manifests survives an
        // upgrade. A materializer that read token H while H+1 publishes needs H's trampoline to
        // exist long enough to apply; GC may delete only what is outside this window. 64 covers
        // a follower catching up across ~1 minute of writes.
        let retain_key = ctl_gc_retain();
        let previous_retain = self
            .handle
            .get_stale(retain_key.as_bytes())
            .await
            .ok()
            .flatten()
            .map(|v| String::from_utf8_lossy(&v).into_owned())
            .unwrap_or_default();
        let mut retain: Vec<String> = vec![head.clone()];
        for token in previous_retain.lines().filter(|token| !token.is_empty()) {
            if token != head && retain.len() < 64 {
                retain.push(token.to_string());
            }
        }
        let retain_bytes = retain.join("\n").into_bytes();

        // Chunking exists because a ~12k-entity bulk config is far past the library's
        // 4096-entry / 4 MiB `multi_put` bound. Both bounds are enforced per chunk; a value that
        // alone exceeds the byte budget gets a chunk to itself and is left to the library's own
        // per-value/total caps to judge.
        //
        // `multi_put_with_index` returns the index each chunk actually applied at. Only the
        // FINAL batch's index is the tree generation; these earlier indexes prove the uploads
        // are committed before the head entry.
        for chunk in chunk_missing_entities(&missing) {
            writer
                .multi_put_with_index(&chunk)
                .await
                .map_err(|e| match e {
                    ArachneError::InvalidArgument(msg) => StoreError::Refused(msg),
                    other => map_err(other),
                })?;
        }

        // FINAL BATCH = the commit point. manifest + gc_retain + head land in ONE log entry (all
        // or nothing), so a reader sees either the old tree or the new one: the head token moves
        // with the manifest it names; every shard/entity it references was already uploaded
        // above. `cfg_toc` is NOT written for a new tree.
        let batch: Vec<(&[u8], &[u8])> = vec![
            (manifest_key.as_bytes(), manifest_bytes.as_slice()),
            (retain_key.as_bytes(), retain_bytes.as_slice()),
            (head_key.as_bytes(), head.as_bytes()),
        ];
        // Same `InvalidArgument` → `Refused` discrimination as the chunks above: the library
        // validates this batch BEFORE propose, so a bound overrun commits nothing and the
        // previous head stays current.
        let final_index = writer
            .multi_put_with_index(&batch)
            .await
            .map_err(|e| match e {
                ArachneError::InvalidArgument(msg) => StoreError::Refused(msg),
                other => map_err(other),
            })?;

        tracing::debug!(
            target: "hydra::cluster",
            head = %head,
            final_index,
            "control-plane tree committed"
        );

        // 0.5.1 read-back guard (consumer pack §3.1). A successful propose is not
        // enough: the head key must actually name this tree on a local read.
        // Wait for the batch's index to apply first; then compare the head token
        // and its origin index. A stale session/leader must fail loudly here, not
        // return 200 while every other node keeps the old head.
        writer
            .wait_applied(final_index, PUBLISH_READBACK_WAIT)
            .await
            .map_err(map_err)?;
        let observed = self.current_head_with_index().await?;
        if observed.is_none() {
            crate::admin::metrics::record_arachne_publish_readback("stale");
        }
        let (observed_head, observed_index) = observed.ok_or_else(|| {
            StoreError::Arachne(format!(
                "published tree {head} at index {final_index}, but ctl/head is absent after read-back"
            ))
        })?;
        if observed_index < final_index {
            crate::admin::metrics::record_arachne_publish_readback("stale");
            return Err(StoreError::Arachne(format!(
                "published tree {head} at index {final_index}, but ctl/head reads as \
                 {observed_head} at index {observed_index}; refusing to report a head the \
                 cluster does not name"
            )));
        }
        if observed_head != head {
            if observed_index == final_index {
                crate::admin::metrics::record_arachne_publish_readback("conflict");
                return Err(StoreError::Arachne(format!(
                    "published tree {head} at index {final_index}, but ctl/head names a different \
                     tree {observed_head} at the same index"
                )));
            }
            // A concurrent publish with a genuinely higher index won. Our final
            // batch did commit, but its tree is superseded by the newer head; that
            // is last-writer-wins, not a stale-session/read-back failure.
            crate::admin::metrics::record_arachne_publish_readback("superseded");
            tracing::warn!(
                target: "hydra::cluster",
                published = %head,
                final_index,
                observed = %observed_head,
                observed_index,
                "published tree was superseded by a newer head"
            );
        }
        Ok((head, final_index))
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

    fn entry(path: EntityPath, len: u32) -> TocEntry {
        TocEntry {
            path,
            content_hash: [7u8; 32],
            len,
        }
    }

    /// The split is a fixed 32-bucket partition: every entry appears exactly once, each shard is
    /// within the plan-time limit, and the manifest's totals are derived from the entries.
    #[test]
    fn split_shards_uses_32_buckets_and_preserves_every_entry() {
        let entities: Vec<TocEntry> = (0..24_000)
            .map(|i| entry(EntityPath::User(format!("user-{i:05}")), 17))
            .collect();
        let toc = Toc::new(TOC_FORMAT, entities.clone()).expect("toc");
        let (manifest, shards) = split_shards(&toc).expect("split");

        assert_eq!(shards.len(), SHARD_COUNT);
        assert_eq!(manifest.shards.len(), SHARD_COUNT);
        assert_eq!(manifest.entry_count as usize, entities.len());
        assert_eq!(
            manifest.entity_bytes,
            entities.iter().map(|e| u64::from(e.len)).sum::<u64>()
        );

        let mut rebuilt = Vec::new();
        for (index, shard) in shards.iter().enumerate() {
            assert_eq!(shard.format, TOC_FORMAT);
            assert!(
                shard.encode().len() <= SHARD_MAX_BYTES,
                "shard {index} must stay under the plan-time cap"
            );
            assert_eq!(manifest.shards[index], shard.digest());
            assert!(shard
                .entities
                .iter()
                .all(|e| shard_bucket(&e.path) == index));
            rebuilt.extend(shard.entities.iter().cloned());
        }
        rebuilt.sort_by(|a, b| a.path.cmp(&b.path));
        let mut expected = entities;
        expected.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(rebuilt, expected, "every entry must survive the split");
    }

    /// A bucket over SHARD_MAX_BYTES refuses the WHOLE publish at plan time. This is a pure
    /// function, so "before any pre-upload" is structural: the caller receives Err before it can
    /// construct a batch.
    #[test]
    fn split_shards_refuses_an_over_limit_bucket_at_plan_time() {
        // Every entry is the same path, so every entry lands in the same bucket.
        let entities: Vec<TocEntry> = (0..24_000)
            .map(|_| entry(EntityPath::Tenant("x".into()), 0))
            .collect();
        let toc = Toc::new(TOC_FORMAT, entities).expect("toc");
        let got = split_shards(&toc);
        match got {
            Err(StoreError::Refused(reason)) => assert!(
                reason.contains("SHARD_MAX_BYTES"),
                "the refusal must name the shard cap, got: {reason}"
            ),
            other => panic!("an over-limit shard must be refused at plan time, got {other:?}"),
        }
    }

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
