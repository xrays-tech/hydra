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
    cfg_entity, cfg_toc, content_hash, ctl_head, validate_entities, EntityPath, KeysError, Toc,
    TocEntry, TOC_FORMAT,
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

/// Why a publish failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreError {
    /// This node is not the leader: someone else owns the commit point.
    NotLeader,
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
        other => StoreError::Arachne(format!("{other:?}")),
    }
}

/// What one publish must write, decided **without** touching the cluster.
///
/// Split out because this decision is the part that can be wrong in a way no
/// integration test would localize: writing nothing extra is a performance
/// property, but writing the WRONG entity is a correctness one.
#[derive(Debug, PartialEq, Eq)]
pub struct PublishPlan {
    /// Entities to write: those whose path is new, or whose bytes changed.
    pub write: Vec<(EntityPath, Vec<u8>)>,
    /// The toc describing the new tree.
    pub toc: Toc,
}

/// Decide what to write for `next` given the `previous` tree (if any).
///
/// # Errors
/// [`KeysError`] when an entity id cannot be carried by a key or the toc.
pub fn plan_publish(previous: Option<&Toc>, next: &ConfigTree) -> Result<PublishPlan, KeysError> {
    // Build the toc first: it validates every id, so an unkeyable entity is
    // refused before a single byte is sent to the cluster.
    let entries: Vec<TocEntry> = next
        .iter()
        .map(|(path, bytes)| TocEntry {
            path: path.clone(),
            content_hash: content_hash(bytes),
            len: bytes.len() as u32,
        })
        .collect();
    validate_entities(&entries)?;
    let toc = Toc::new(TOC_FORMAT, entries)?;

    // A previous toc, when there is one, is what makes "unchanged" decidable:
    // same path AND same content hash means the stored value is already the
    // right one, whichever tree names it.
    let previously_stored = |path: &EntityPath, hash: &[u8; 32]| -> bool {
        previous.is_some_and(|prev| {
            prev.entities
                .iter()
                .any(|e| &e.path == path && &e.content_hash == hash)
        })
    };

    let write = next
        .iter()
        .filter(|(path, bytes)| !previously_stored(path, &content_hash(bytes)))
        .map(|(path, bytes)| (path.clone(), bytes.clone()))
        .collect();

    Ok(PublishPlan { write, toc })
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
            let key = cfg_entity(&entry.path);
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
            // The check that makes a torn read impossible to serve: the bytes
            // must be the bytes the toc describes.
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

    /// The head's value, or `None` when nothing was ever published.
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
        // What is currently committed decides what has to be written. A read
        // failure here is fatal to the publish: publishing against an unknown
        // baseline would rewrite everything (correct but wasteful) or, worse,
        // the caller could mistake it for "nothing changed".
        let previous = match self.read().await {
            Ok(ReadOutcome::Empty) => None,
            Ok(ReadOutcome::Tree(t)) => Some(t),
            Err(e) => return Err(e),
        };
        let previous_toc = match previous.as_ref() {
            None => None,
            Some(t) => Some(toc_for(t)?),
        };

        let plan = plan_publish(previous_toc.as_ref(), tree)?;
        let toc_hash = plan.toc.hash();
        let writer = self.handle.without_redirect();

        // 1. entities — a failure here leaves the old tree committed and the new
        //    one unreachable, because nothing names it yet.
        for (path, bytes) in &plan.write {
            writer
                .put(cfg_entity(path).as_bytes(), bytes)
                .await
                .map_err(map_err)?;
        }

        // 2. the toc
        writer
            .put(cfg_toc(&toc_hash).as_bytes(), &plan.toc.encode())
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

/// The toc for an already-materialized tree, used to compare a publish against
/// what is committed.
fn toc_for(tree: &ConfigTree) -> Result<Toc, KeysError> {
    Toc::new(
        TOC_FORMAT,
        tree.iter()
            .map(|(path, bytes)| TocEntry {
                path: path.clone(),
                content_hash: content_hash(bytes),
                len: bytes.len() as u32,
            })
            .collect(),
    )
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

    /// The previous toc for a tree, as a publisher would have written it.
    fn toc_of(t: &ConfigTree) -> Toc {
        Toc::new(
            TOC_FORMAT,
            t.iter()
                .map(|(path, bytes)| TocEntry {
                    path: path.clone(),
                    content_hash: content_hash(bytes),
                    len: bytes.len() as u32,
                })
                .collect(),
        )
        .expect("fixture toc")
    }

    /// The first publish has nothing to compare against, so everything is new.
    ///
    /// Falsification: treat "no previous toc" as "nothing changed" and this
    /// fails with an empty write set — a cluster that publishes an empty config.
    #[test]
    fn the_first_publish_writes_every_entity() {
        let next = tree(&[("a", b"one"), ("b", b"two")]);
        let plan = plan_publish(None, &next).expect("plan");
        assert_eq!(
            plan.write.len(),
            2,
            "with no previous tree every entity must be written"
        );
        assert_eq!(plan.toc.entities.len(), 2);
    }

    /// The property that makes this a tree and not a blob: an edit writes THAT
    /// entity and nothing else.
    ///
    /// Falsification: compare paths instead of content hashes and the
    /// "unchanged" assertion fails, because the path of `a` is present in both
    /// trees.
    #[test]
    fn an_edit_rewrites_only_the_entities_that_changed() {
        let before = tree(&[("a", b"one"), ("b", b"two"), ("c", b"three")]);
        let mut after = before.clone();
        after.insert(EntityPath::Tenant("b".into()), b"TWO".to_vec());

        let plan = plan_publish(Some(&toc_of(&before)), &after).expect("plan");
        assert_eq!(
            plan.write,
            vec![(EntityPath::Tenant("b".into()), b"TWO".to_vec())],
            "only the edited entity may be written"
        );
        assert_eq!(
            plan.toc.entities.len(),
            3,
            "the new tree still names all three"
        );
    }

    /// An untouched config plans no writes at all: publishing is idempotent, and
    /// a no-op publish must not churn the log.
    ///
    /// Falsification: rebuild every entry unconditionally and this fails.
    #[test]
    fn an_unchanged_tree_plans_no_writes() {
        let t = tree(&[("a", b"one"), ("b", b"two")]);
        let plan = plan_publish(Some(&toc_of(&t)), &t).expect("plan");
        assert!(
            plan.write.is_empty(),
            "an unchanged tree must write nothing, got {:?}",
            plan.write
        );
    }

    /// Addition and removal are both expressed by the toc alone: a removal needs
    /// no write, and the removed entity is simply absent from the new tree.
    ///
    /// Falsification: carry the entity set from the PREVIOUS toc and the
    /// removal assertion fails — a deleted tenant would keep serving.
    #[test]
    fn additions_and_removals_are_expressed_by_the_toc() {
        let before = tree(&[("a", b"one"), ("b", b"two")]);
        let mut after = before.clone();
        after.remove(&EntityPath::Tenant("b".into()));
        after.insert(EntityPath::Tenant("c".into()), b"three".to_vec());

        let plan = plan_publish(Some(&toc_of(&before)), &after).expect("plan");
        assert_eq!(
            plan.write,
            vec![(EntityPath::Tenant("c".into()), b"three".to_vec())],
            "only the addition is written; the removal is expressed by the toc"
        );
        let paths: Vec<&EntityPath> = plan.toc.entities.iter().map(|e| &e.path).collect();
        assert!(
            !paths.contains(&&EntityPath::Tenant("b".into())),
            "the removed entity must not appear in the new toc"
        );
        assert!(paths.contains(&&EntityPath::Tenant("c".into())));
    }

    /// The toc a plan carries must hash exactly like the tree it describes, or
    /// the head would name a tree whose contents differ from what was planned.
    #[test]
    fn the_planned_toc_describes_exactly_the_planned_tree() {
        let t = tree(&[("a", b"one"), ("b", b"two")]);
        let plan = plan_publish(None, &t).expect("plan");
        for entry in &plan.toc.entities {
            let bytes = t.get(&entry.path).expect("every entity is in the tree");
            assert_eq!(
                entry.content_hash,
                content_hash(bytes),
                "entry {} does not describe its own bytes",
                entry.path.to_key_segment()
            );
            assert_eq!(entry.len as usize, bytes.len());
        }
        assert_eq!(plan.toc.entities.len(), t.len());
    }

    /// An id that cannot be keyed must be refused at plan time — before anything
    /// reaches the cluster — because a `/` would write into another entity's
    /// namespace.
    ///
    /// Falsification: drop the validation and this returns `Ok`.
    #[test]
    fn an_unkeyable_entity_id_is_refused_before_any_write() {
        let bad = tree(&[("../../ctl/head", b"x")]);
        let got = plan_publish(None, &bad);
        assert!(
            matches!(got, Err(KeysError::IdHasSeparator { .. })),
            "a path-bearing id must be refused, got {got:?}"
        );
    }
}
