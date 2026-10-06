#![cfg(feature = "arachne")]
//! Publishing a locally-committed config to the cluster (ADR-0001, plan T3.2).
//!
//! ## The model this implements
//!
//! Under the homogeneous model (ADR-0001 D-2, and the user's ruling on where a management write
//! lands) every node runs the same write path: the SQLite transaction commits LOCALLY, then this
//! node encodes its own config into a tree and commits the head. The head write is the commit
//! point, and it is the one operation the library forwards to the raft leader, so "any node can
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
//! encoded at all (an over-sized value, a provider id that cannot be keyed).

use std::sync::Arc;

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

    /// Encode `cfg` (+ the fidelity rows) and commit it. Returns the new head hash.
    ///
    /// The two steps are separate on purpose: encoding is a pure function of the config (and
    /// testable without a cluster), committing is `ArachneConfigStore::publish`, which is the
    /// single writer of `ctl/head`.
    ///
    /// # Errors
    /// A human-readable reason: the codec refused the config (an unkeyable id, a value over the
    /// library's 1 MiB cap, a seal that failed), or the commit did not go through (no leader, no
    /// quorum). Both mean "the cluster does not have this config".
    pub async fn publish(
        &self,
        cfg: &ConfigData,
        fidelity: &FidelityRows,
    ) -> Result<String, String> {
        // A REFUSAL, not a transport failure: the config cannot be published as it
        // stands (an over-sized value against the library's 1 MiB cap, an id that
        // cannot be keyed, a failed seal), so retrying will not help and the fix is
        // a configuration change. Counted separately from `error` for that reason —
        // it is the capacity alarm ADR-0001 risk R2 asks for, and it is a fact about
        // the CONFIG rather than about the cluster.
        let tree = encode_config(cfg, fidelity, self.key_provider.as_ref()).map_err(|e| {
            crate::admin::metrics::record_arachne_publish("refused");
            format!("cannot encode the config tree: {e}")
        })?;
        self.store
            .publish(&tree)
            .await
            .map_err(|e| format!("the head could not be committed: {e}"))
    }
}
