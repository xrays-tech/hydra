//! # `HydratedWire` — what a config snapshot installs (ADR-0001, after T4.1)
//!
//! ## What this module used to be
//!
//! The whole **snapshot wire**: `SnapshotWire::build` sealed the leader's secrets, shipped the
//! version-labelled config over the internal control channel, and `hydrate` unsealed it on the
//! receiver. That channel is retired — nodes are homogeneous and each one materializes the config
//! TREE from Arachne (`cluster::arachne_materializer`), where the secrets are sealed per entity and
//! the commit point is the head hash rather than a version number.
//!
//! Two things were deliberately NOT carried over, so nobody looks for them here:
//! **wire versioning** (the tree's format is versioned by `TOC_FORMAT`, and a mismatched tree is
//! refused by name) and **sealed DTOs** (the tree carries `crypto::Sealed` directly, which is why
//! `Sealed` gained `Serialize`).
//!
//! ## What survives, and why it is still called a wire
//!
//! [`HydratedWire`] is the value that installs a config into a [`crate::store::ConfigStore`]:
//! `ConfigStore::apply_snapshot` takes it, and both installers build it — the Arachne materializer
//! (from a decoded tree) and tests. Renaming it would churn every call site for no gain; what
//! matters is that it no longer implies a channel.

use hydra_core::config::ConfigData;

use crate::cluster::content::FidelityRows;

/// What a replica gets after unsealing: the runtime config plus the fidelity
/// rows, so "unsealing" and "what to rebuild" are one decision.
pub struct HydratedWire {
    pub version: u64,
    pub cfg: ConfigData,
    pub fidelity: FidelityRows,
}
