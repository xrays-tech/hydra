//! # The config commit point (ADR-0001, plan T2.2)
//!
//! This is the **only** place allowed to write [`crate::cluster::arachne_keys::ctl_head`].
//! Everything else reads through it. That single-writer rule is what makes the
//! head a commit point rather than a label:
//!
//! ```text
//! publish(tree) =
//!   1. write every entity whose bytes differ from the previous tree
//!   2. write the new toc
//!   3. write head = <new toc hash>          <- the commit
//! ```
//!
//! A failure before step 3 leaves the previous tree current and every reader
//! untouched; the half-written entities belong to a tree nobody names, so they
//! are unreachable rather than dangerous. This is why the ordering is asserted
//! by a test rather than left to review.
//!
//! ## Reading
//!
//! `read` goes head → toc → entities, using **`get_stale` only**. Two
//! consequences, both deliberate:
//!
//! * a follower can materialize without a linearizable read (a follower's `get`
//!   returns `QuorumUnavailable` immediately — measured, ADR-0001 §10 F-2);
//! * a reader that catches the cluster mid-publish sees EITHER the old tree or
//!   the new one, never a mix, because head names exactly one toc and every
//!   entity is checked against the hash that toc records. A mismatch is a
//!   retry, not a repair.

use std::collections::BTreeMap;
use std::sync::Arc;

use arachne_kv::client::{ArachneError, Handle};

use super::arachne_keys::{
    cfg_entity, cfg_toc, content_hash, ctl_head, hex_hash, validate_entities, EntityPath,
    KeysError, Toc, TocEntry, TOC_FORMAT,
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

fn map_err(e: ArachneError) -> StoreError {
    match e {
        ArachneError::NotLeader { .. } => StoreError::NotLeader,
        ArachneError::QuorumUnavailable => StoreError::QuorumUnavailable,
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

    /// Publish `tree` and commit it by moving the head last.
    ///
    /// # Errors
    /// [`StoreError::NotLeader`] when this node does not own the commit point,
    /// [`StoreError::Keys`] for a malformed tree, [`StoreError::Arachne`] for a
    /// transport or quorum failure.
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
        // Each `put` is forwarded INDIVIDUALLY, so the publisher does not need to be the leader —
        // but see `KNOWN RISK` below: the sequence is not atomic.
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
        // 1. entities — a failure here leaves the old tree committed and the new
        //    one unreachable, because nothing names it yet.
        //
        // `get_stale` on our own replica: a "missing" answer from a lagging node only costs a
        // redundant, idempotent put, while a "present" answer cannot be wrong about the CONTENT
        // (the key it answered for is derived from that content's hash).
        for (path, bytes) in tree.iter() {
            let key = cfg_entity(path, &hex_hash(&content_hash(bytes)));
            if matches!(self.handle.get_stale(key.as_bytes()).await, Ok(Some(_))) {
                continue;
            }
            writer.put(key.as_bytes(), bytes).await.map_err(map_err)?;
        }

        // 2. the toc
        writer
            .put(cfg_toc(&toc_hash).as_bytes(), &toc.encode())
            .await
            .map_err(map_err)?;

        // 3. the commit
        writer
            .put(ctl_head().as_bytes(), toc_hash.as_bytes())
            .await
            .map_err(map_err)?;

        Ok(toc_hash)
    }
}

#[cfg(test)]
mod tests {
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
