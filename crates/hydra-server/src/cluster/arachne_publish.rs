#![cfg(feature = "arachne")]
//! Publishing a locally-committed config to the cluster (ADR-0001, plan T3.2).
//!
//! ## The model this implements
//!
//! Under the homogeneous model (ADR-0001 D-2, and the user's ruling on where a management write
//! lands) every node runs the same write path: the SQLite transaction commits LOCALLY, then this
//! node encodes its own config into a tree and commits it: the toc is split into shards, missing
//! entities and shards are pre-uploaded in bounded content-addressed chunks, then manifest +
//! head token go into ONE final `multi_put` — a single log entry, all or nothing, the head
//! moving in that same entry as the manifest it names (2026-10-10, plan
//! `2026-10-10-multi-put-atomic-publish`; sharded V2 + Arachne 0.5 index returns).
//! Those writes are forwarded to the raft leader (when this node is a follower), so "any node can
//! accept a management write" does not mean "any node is a writer" at the storage level.
//!
//! ## Why the local write is not rolled back when publishing fails
//!
//! It cannot be: the transaction committed before we knew. So the failure is reported as
//! [`crate::store::StoreError::NotPublished`] — which says "committed locally, NOT published"
//! rather than "the write failed" — and the admin layer answers 503 so an operator sees that the
//! cluster does not have it yet. The node keeps serving its own (new) config meanwhile.
//!
//! ## What is deliberately NOT here
//!
//! A retry queue. A failed publish is retried by the NEXT write (which publishes whatever the
//! config is then) and by `POST /api/v1/reload`; a background retry loop would add a second owner
//! of "the tree the cluster should have" without changing the outcome for a config that cannot be
//! encoded or committed at all (an over-sized value, a provider id that cannot be keyed, a batch
//! over the library's entry-count/total-byte bound).

use std::sync::Arc;

use arachne_kv::Index;
use hydra_core::config::ConfigData;

use super::arachne_materializer::encode_config;
use super::arachne_store::ArachneConfigStore;
use super::content::FidelityRows;
use crate::crypto::KeyProvider;

/// Publishes one node's config to the Arachne control plane.
///
/// Holds the key provider because the tree seals its secret-bearing entities — DETERMINISTICALLY,
/// so the tree's name depends on the config and not on which node published it
/// (`crate::crypto::KeyProvider::seal_deterministic`).
pub struct ConfigPublisher {
    store: ArachneConfigStore,
    key_provider: Arc<dyn KeyProvider>,
}

impl ConfigPublisher {
    /// Build a publisher over a started control plane.
    #[must_use]
    pub fn new(store: ArachneConfigStore, key_provider: Arc<dyn KeyProvider>) -> Self {
        Self {
            store,
            key_provider,
        }
    }

    /// Encode `cfg` (+ the fidelity rows) and commit it. Returns the new head token
    /// (`m:<manifest-hash>`).
    ///
    /// The two steps are separate on purpose: encoding is a pure function of the config (and
    /// testable without a cluster), committing is `ArachneConfigStore::publish`, which is the
    /// single writer of `ctl/head`.
    ///
    /// # Errors
    /// A human-readable reason: the codec refused the config (an unkeyable id, a value over the
    /// configured per-value cap, a seal that failed); a toc shard exceeded `SHARD_MAX_BYTES` at
    /// plan time; an entity/shard chunk or the final manifest/head batch overran a library bound
    /// (that batch enters nothing, and the head never moves); or the commit did not go through
    /// (no leader, no quorum). All mean "the cluster does not have this config"; the first two
    /// are also counted as `publish_total{result="refused"}`.
    /// The local replica's current head and its applied index (for the
    /// ConfigStore's local-write watermark).
    pub async fn head_with_index(&self) -> Result<Option<(String, u64)>, String> {
        self.store
            .current_head_with_index()
            .await
            .map_err(|e| e.to_string())
    }

    /// Run one bounded GC pass. Delegates to the control-plane store (the
    /// publisher is the object the ConfigStore already owns).
    pub async fn gc(&self, max_deletes: usize) -> Result<super::arachne_store::GcStats, String> {
        self.store.gc(max_deletes).await.map_err(|e| e.to_string())
    }

    pub async fn publish(
        &self,
        cfg: &ConfigData,
        fidelity: &FidelityRows,
    ) -> Result<String, String> {
        self.publish_with_index(cfg, fidelity)
            .await
            .map(|(head, _index)| head)
    }

    /// Encode, commit, and return both the new head token and the **final log index** that
    /// committed the manifest+head. That index is the tree generation used by the local-write
    /// watermark.
    pub async fn publish_with_index(
        &self,
        cfg: &ConfigData,
        fidelity: &FidelityRows,
    ) -> Result<(String, Index), String> {
        // A REFUSAL, not a transport failure: the config cannot be published as it
        // stands (a value over the configured per-value cap, an id that cannot be
        // keyed, a failed seal), so retrying will not help and the fix is a
        // configuration change. Counted separately from `error` for that reason —
        // it is the capacity alarm ADR-0001 risk R2 asks for, and it is a fact about
        // the CONFIG rather than about the cluster.
        let tree = encode_config(cfg, fidelity, self.key_provider.as_ref()).map_err(|e| {
            crate::admin::metrics::record_arachne_publish("refused");
            format!("cannot encode the config tree: {e}")
        })?;
        self.store
            .publish_with_index(&tree)
            .await
            .map_err(|e| format!("the head could not be committed: {e}"))
    }
}
