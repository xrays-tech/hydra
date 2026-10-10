#![cfg(feature = "arachne")]
//! T3.1 (I/O half) — the per-node materialization loop, end to end.
//!
//! The decision half lives in [`crate::cluster::arachne_materialize`] (the gate and its
//! backoff) and the codec in [`crate::cluster::arachne_entities`]. This module is what puts
//! them together: read the head from Arachne, decide, decode the tree into a `ConfigData`,
//! hand it to the target, and only then move the watermark.
//!
//! ## The properties pinned here
//!
//! 1. **The watermark moves only after a successful apply.** A node that recorded a version it
//!    failed to install would serve its old config while reporting itself current — and would
//!    never retry, because the gate would see "no change".
//! 2. **An edit in one entity still re-materializes** (the tree's name changed), while the
//!    steady state costs one local read and nothing else.
//! 3. **A missing head is not an empty config.** A node with a baseline that suddenly sees no
//!    head (a wiped store) must NOT be handed an empty `ConfigData`: that would take the data
//!    plane down while every probe stayed green.
//! 4. **The target is told what it needs to rebuild.** `FidelityRows` travel with the config,
//!    because a replica that receives only `ConfigData` cannot rebuild its SQLite tables
//!    byte-faithfully (disabled rows, provider-key identity, offline models, token hashes).
//!
//! ## What this layer deliberately does NOT do yet
//!
//! Wire itself into `main.rs`. [`ReplicaTarget`] is the real target (it rebuilds the node's
//! SQLite replica and swaps the in-memory config), and the loop is proven against a live raft node
//! and a live store — but nothing spawns the loop on startup yet, and publishing still goes
//! through `SnapshotWire`, so today no node serves config from Arachne. Both halves belong to the
//! publish cutover (T3.2), and doing one without the other would leave a node that reports itself
//! materialized against a head nobody writes.

#![cfg(feature = "arachne")]

use std::sync::Arc;

use hydra_core::config::ConfigData;

use super::arachne_entities::{build_config_with_fidelity, split_config, tree_of};
use super::arachne_materialize::{MaterializeGate, Plan};
use super::arachne_store::{ArachneConfigStore, ConfigTree, ReadOutcome, StoreError};
use super::content::FidelityRows;
use super::snapshot::HydratedWire;
use crate::crypto::KeyProvider;
use crate::store::ConfigStore;

/// What one convergence pass did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Converged {
    /// Nothing to do: the head names what this node already serves, or no head exists yet.
    NoChange,
    /// The tree named by `hash` was decoded and applied.
    Applied { hash: String },
    /// A previous failure is still in its backoff window.
    Deferred { hash: String },
}

/// Why a convergence pass could not complete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MaterializeError {
    /// The head value is not a tree name this build could have written.
    Head { reason: String },
    /// The tree could not be read, decoded, or applied.
    Unavailable { hash: String, reason: String },
}

impl std::fmt::Display for MaterializeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Head { reason } => write!(f, "config head: {reason}"),
            Self::Unavailable { hash, reason } => {
                write!(f, "tree {hash} could not be materialized: {reason}")
            }
        }
    }
}

impl std::error::Error for MaterializeError {}

/// What a materialized config is installed into.
///
/// A trait rather than a direct [`ConfigStore`] call so the loop can be driven against a
/// recording target: the properties above are about ORDER (apply before watermark), which a
/// test that only inspected a live store could not observe.
///
/// **Async**, because the real target's first step is a SQLite transaction (`db::restore_config`)
/// and the loop must not report success before it commits. `async_trait` boxes the future so the
/// trait stays usable as a `dyn` object; the repo already depends on it for pingora's handler
/// impls.
#[async_trait::async_trait]
pub trait MaterializeTarget: Send + Sync {
    /// Install `cfg` (and the fidelity rows a replica needs to rebuild its own SQLite tables)
    /// as the state this node serves.
    ///
    /// # Errors
    /// A human-readable reason. The loop treats any error as "not materialized": the watermark
    /// does not move, the backoff starts, and the next pass retries.
    async fn apply(&self, cfg: Arc<ConfigData>, fidelity: Arc<FidelityRows>) -> Result<(), String>;
}

/// The per-node loop: gate + store + target.
/// Where the materializer reads the head FROM — one method, so the ordering can be tested with a
/// scripted sequence.
///
/// Why a seam is needed at all: the refusal branch triggers on "an older hash at a LOWER index", and no
/// real cluster can produce that combination — `put` only ever raises a key's index (upstream's
/// monotonicity guarantee, which is the whole point of the ordering). Only a LAGGING REPLICA does, so a
/// test has to inject the sequence. The production path is untouched: `Materializer::new` wraps the
/// concrete store, which is what every caller passes today.
#[async_trait::async_trait]
pub trait HeadSource: Send + Sync {
    /// The head value and the log index of the entry that wrote it.
    ///
    /// # Errors
    /// A reason the head could not be read.
    async fn head_with_index(&self) -> Result<Option<(String, u64)>, String>;
}

#[async_trait::async_trait]
impl HeadSource for ArachneConfigStore {
    async fn head_with_index(&self) -> Result<Option<(String, u64)>, String> {
        self.current_head_with_index()
            .await
            .map_err(|e| e.to_string())
    }
}

pub struct Materializer {
    store: ArachneConfigStore,
    /// The head read, behind the seam above.
    head_source: Arc<dyn HeadSource>,
    gate: MaterializeGate,
    target: Arc<dyn MaterializeTarget>,
    /// Needed to open the sealed fidelity rows. Held rather than passed per call so a
    /// materializer cannot be driven without the key that unseals its own replica.
    key_provider: Arc<dyn crate::crypto::KeyProvider>,
    /// The log index of the head this node last APPLIED — the generation ordering compares against.
    ///
    /// Held here rather than in the gate so the gate's decision signature (and its eleven unit tests)
    /// stay unchanged: the index is an input to "should I even ask the gate?", which is the
    /// materializer's question, not the gate's.
    applied_index: Option<u64>,
}

impl Materializer {
    /// Build a loop over a store and a target. The gate starts empty, i.e. this node has
    /// materialized nothing and is therefore not yet eligible to lead.
    #[must_use]
    pub fn new(
        store: ArachneConfigStore,
        target: Arc<dyn MaterializeTarget>,
        key_provider: Arc<dyn crate::crypto::KeyProvider>,
    ) -> Self {
        Self {
            // Built from the same store the caller handed us, so every existing call site keeps the
            // production behaviour it had.
            head_source: Arc::new(store.clone()),
            store,
            gate: MaterializeGate::new(),
            target,
            key_provider,
            // Nothing applied yet: the first tree this node materializes has no predecessor to be
            // older than.
            applied_index: None,
        }
    }

    /// Test-only: build over a scripted head read, with the real store still doing the tree read and
    /// the real target still receiving the apply — only the (hash, index) sequence is injected.
    #[cfg(test)]
    pub(crate) fn with_head_source(
        head_source: Arc<dyn HeadSource>,
        store: ArachneConfigStore,
        target: Arc<dyn MaterializeTarget>,
        key_provider: Arc<dyn crate::crypto::KeyProvider>,
    ) -> Self {
        Self {
            store,
            head_source,
            gate: MaterializeGate::new(),
            target,
            key_provider,
            applied_index: None,
        }
    }

    /// The tree this node currently serves, if any.
    #[must_use]
    pub fn materialized(&self) -> Option<&str> {
        self.gate.materialized()
    }

    /// Whether this node may be the leader: it has installed a config at least once.
    #[must_use]
    pub fn may_be_leader(&self) -> bool {
        self.gate.may_be_leader()
    }

    /// One pass: read the head, decide, and (if told to) install.
    ///
    /// # Errors
    /// [`MaterializeError`] for a malformed head or a tree that cannot be read/decoded. A
    /// failure is recorded in the gate (which starts a backoff) and does NOT move the
    /// watermark.
    pub async fn converge(&mut self) -> Result<Converged, MaterializeError> {
        // The head WITH its log index: the value may be arbitrarily stale (upstream: "arbitrary old,
        // N1"), but the index says how old, which is the ordering this node needs before it REPLACES
        // its own database and snapshot with the tree that value names.
        let (head, head_index) = match self.head_source.head_with_index().await {
            Ok(Some((hash, index))) => (Some(hash), Some(index)),
            Ok(None) => (None, None),
            Err(reason) => return Err(MaterializeError::Head { reason }),
        };
        if crate::cluster::arachne_materialize::is_stale_generation(head_index, self.applied_index)
        {
            // A stale read answered with an older tree. Keep serving what we have; the next tick reads
            // again, and a fresher answer is applied then.
            tracing::debug!(
                head = head.as_deref().unwrap_or("-"),
                index = head_index.unwrap_or(0),
                applied = self.applied_index.unwrap_or(0),
                "head read is OLDER than the applied generation; refusing to roll back"
            );
            return Ok(Converged::NoChange);
        }

        let plan = self
            .gate
            .plan(
                head.as_deref().map(str::as_bytes),
                std::time::Instant::now(),
            )
            .map_err(|e| MaterializeError::Head {
                reason: e.to_string(),
            })?;

        match plan {
            Plan::NoChange => {
                // No head at all: a cluster that was never configured. It is NOT an empty
                // config — see property 3 — so the target is not called, but the node does
                // become eligible to lead (it serves "nothing configured", which is a valid
                // baseline for a fresh cluster).
                if head.is_none() && self.gate.materialized().is_none() {
                    tracing::info!(
                        "no config has ever been published; this node is eligible to lead with an \
                         empty configuration"
                    );
                    self.gate.succeeded(NO_HEAD_MARKER);
                }
                Ok(Converged::NoChange)
            }
            Plan::Hold { hash, .. } => Ok(Converged::Deferred { hash }),
            Plan::Materialize { hash } => match self.store.read().await {
                Ok(ReadOutcome::Tree(tree)) => {
                    match build_config_with_fidelity(&tree, self.key_provider.as_ref()) {
                        Ok((cfg, fidelity)) => {
                            // Apply FIRST, then move the watermark: the other order is the bug
                            // this test file exists for.
                            if let Err(reason) =
                                self.target.apply(Arc::new(cfg), Arc::new(fidelity)).await
                            {
                                let wait = self.gate.failed(&hash);
                                return Err(MaterializeError::Unavailable {
                                    hash,
                                    reason: format!("{reason} (retry in {wait:?})"),
                                });
                            }
                            self.gate.succeeded(&hash);
                            self.applied_index = head_index;
                            tracing::info!(hash = %hash, "config materialized");
                            Ok(Converged::Applied { hash })
                        }
                        Err(e) => {
                            let wait = self.gate.failed(&hash);
                            Err(MaterializeError::Unavailable {
                                hash,
                                reason: format!("{e} (retry in {wait:?})"),
                            })
                        }
                    }
                }
                Ok(ReadOutcome::Empty) => {
                    // The head named a tree, then vanished. Holding the previous config is the
                    // safe answer; wiping it is not.
                    let wait = self.gate.failed(&hash);
                    Err(MaterializeError::Unavailable {
                        hash,
                        reason: format!(
                            "the head names a tree that is no longer readable; keeping the \
                             last-known-good config (retry in {wait:?})"
                        ),
                    })
                }
                Err(e) => {
                    let wait = self.gate.failed(&hash);
                    Err(MaterializeError::Unavailable {
                        hash,
                        reason: format!("{e} (retry in {wait:?})"),
                    })
                }
            },
        }
    }
}

/// The watermark value for "the cluster has no config at all".
///
/// A distinct marker rather than an empty string: [`MaterializeGate::succeeded`] takes a
/// string, and an empty one would be indistinguishable from a bug that passed `""`.
pub const NO_HEAD_MARKER: &str = "<no-config-published>";

/// The real target: rebuild this node's SQLite replica from the materialized config, then swap it
/// into the [`ConfigStore`] the data plane reads.
///
/// ## Order, and why it is this order
///
/// 1. **SQLite first** (`db::restore_config`, ONE transaction). It can fail on a locked database,
///    a foreign-key violation, or a master key that cannot re-seal — and if the in-memory swap
///    happened first, the node would serve a config its own database does not contain. A crash
///    between the two leaves the database ahead of memory, which the next pass repairs; the
///    reverse would leave memory ahead of the database, which nothing repairs.
/// 2. **Then the in-memory store** (`ConfigStore::apply_snapshot`), which clears the SWRR state
///    and runs the snapshot hooks (tenant TLS cert re-resolution) — the same funnel every other
///    config swap uses, so this path cannot forget them.
///
/// ## The version it writes, honestly
///
/// `restore_config` also writes `config_meta.config_version`, an INTEGER watermark that the
/// Redis-era freshness gate compares against the store's. Under Arachne the authority is the
/// `head` HASH, and making that column carry the hash is T3.2. Until then this target keeps the
/// old column consistent with the old rule — `current + 1`, exactly as `ConfigStore::reload_all_with`
/// does — so the two numbers always agree and nothing reads a stale one. It is a MONOTONIC COUNTER,
/// not the config's identity, and this comment exists so that nobody mistakes it for one.
pub struct ReplicaTarget {
    store: ConfigStore,
    key_provider: Arc<dyn KeyProvider>,
}

impl ReplicaTarget {
    /// Build a target over the store it installs into. The key provider is the same one the store
    /// unseals with: a target that could not open the material it just decoded would be a second,
    /// silently different master key.
    #[must_use]
    pub fn new(store: ConfigStore, key_provider: Arc<dyn KeyProvider>) -> Self {
        Self {
            store,
            key_provider,
        }
    }
}

#[async_trait::async_trait]
impl MaterializeTarget for ReplicaTarget {
    async fn apply(&self, cfg: Arc<ConfigData>, fidelity: Arc<FidelityRows>) -> Result<(), String> {
        // Rebuilt INTO the node's own database, so a restart reproduces what it serves — the whole
        // point of materializing rather than applying in memory. There is no "this node has no
        // database to rebuild" refusal here: every node has one (ADR-0001 D-2 retired the `edge`
        // role, and with it the pool-less store that refusal existed for). Deleted 2026-10-05,
        // together with the test that had to build a store no configuration produces.
        let pool = self.store.pool();
        let version = self.store.version() + 1;
        crate::db::restore_config(
            pool,
            self.key_provider.as_ref(),
            cfg.as_ref(),
            fidelity.as_ref(),
            version,
        )
        .await
        .map_err(|e| format!("the local replica could not be rebuilt: {e}"))?;

        // Only now is the config what this node serves. `apply_snapshot` clones internally (it owns
        // the `Arc` it publishes), so the two clones here are one deep copy of a config that
        // changes rarely — not a hot-path cost.
        self.store.apply_snapshot(HydratedWire {
            version,
            cfg: cfg.as_ref().clone(),
            fidelity: fidelity.as_ref().clone(),
        });
        Ok(())
    }
}

/// Encode a config into the tree a publisher commits.
///
/// Encoding only: committing is `ArachneConfigStore::publish`, which is the single writer of the
/// head. Splitting the two keeps "what bytes describe this config" testable without a cluster.
///
/// `kp` seals the secret-bearing entities. Deterministically, so the tree's name depends on the
/// LOGICAL config and not on which node published it — see `arachne_entities`'s module docs.
///
/// # Errors
/// [`StoreError::Arachne`] when the codec refuses the config (an unkeyable id, a seal that fails).
/// All of them must stop the publish rather than degrade.
pub fn encode_config(
    cfg: &ConfigData,
    fidelity: &crate::cluster::content::FidelityRows,
    kp: &dyn KeyProvider,
) -> Result<ConfigTree, StoreError> {
    let blobs = split_config(cfg, fidelity, kp)
        .map_err(|e| StoreError::Arachne(format!("cannot encode the config tree: {e}")))?;
    tree_of(&blobs).map_err(|e| StoreError::Arachne(format!("cannot build the config tree: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use arachne_kv::server::{assemble_cluster, ClusterConfig};
    use arachne_kv::{NodeId, Profile};
    use hydra_core::config::{CertMeta, ModelProvider};
    use hydra_core::model::{Provider, ProviderKey, ProviderModel, Tenant, TenantProvider};

    /// A migrated in-memory pool — the replica side of the real-target test.
    async fn pool() -> sqlx::SqlitePool {
        let pool = crate::db::init_pool("sqlite::memory:")
            .await
            .expect("init_pool");
        crate::db::run_migrate(&pool).await.expect("migrate");
        pool
    }

    /// A target that records every apply, and can be told to fail.
    #[derive(Default)]
    struct RecordingTarget {
        applied: Mutex<Vec<(usize, usize)>>,
        fail_next: Mutex<bool>,
        tenants_seen: Mutex<usize>,
        /// How many limit roles the last apply carried — the fidelity rows must travel WITH the
        /// config, or a replica cannot rebuild its tables.
        fidelity_rows: Mutex<usize>,
    }

    impl RecordingTarget {
        fn applied_count(&self) -> usize {
            self.applied.lock().expect("lock").len()
        }
        fn fail_next(&self) {
            *self.fail_next.lock().expect("lock") = true;
        }
        fn tenants_seen(&self) -> usize {
            *self.tenants_seen.lock().expect("lock")
        }
        fn fidelity_rows(&self) -> usize {
            *self.fidelity_rows.lock().expect("lock")
        }
    }

    #[async_trait::async_trait]
    impl MaterializeTarget for RecordingTarget {
        async fn apply(
            &self,
            cfg: Arc<ConfigData>,
            fidelity: Arc<FidelityRows>,
        ) -> Result<(), String> {
            *self.fidelity_rows.lock().expect("lock") = fidelity.limit_roles.len();
            if *self.fail_next.lock().expect("lock") {
                *self.fail_next.lock().expect("lock") = false;
                return Err("target refused the apply".to_string());
            }
            self.applied
                .lock()
                .expect("lock")
                .push((cfg.tenants_by_domain.len(), cfg.providers.len()));
            *self.tenants_seen.lock().expect("lock") = cfg.tenants_by_domain.len();
            Ok(())
        }
    }

    /// Dedicated loopback ports: cargo runs the tests in this module CONCURRENTLY, so two tests
    /// sharing a port would fail with "Address already in use" — which is how the fifth one below
    /// was caught when the real-target test first borrowed `PORTS[3]`.
    const PORTS: [u16; 7] = [18401, 18402, 18403, 18404, 18405, 18406, 18407];
    /// Disjoint from all seven: these tests run CONCURRENTLY and a raft member must bind the
    /// address its peers were told about (reusing a port fails with "Address already in use").
    const PORT_STALE_SEAM: u16 = 18432;
    /// Disjoint from everything above (and from `PORT_STALE_SEAM`): the over-cap publish test
    /// needs its own node because its assertions read the head's origin index, which a shared
    /// node could move under it.
    const PORT_OVER_CAP: u16 = 18409;

    fn data_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hydra-mat-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create data dir");
        dir
    }

    async fn node(tag: &str, port: u16) -> arachne_kv::server::AssembledClusterNode {
        let id = NodeId::new("solo");
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().expect("addr");
        let mut cfg = ClusterConfig::member(
            format!("hydra-mat-{tag}"),
            id.clone(),
            addr,
            data_dir(tag),
            vec![id.clone()],
            HashMap::from([(id, addr)]),
        );
        cfg.profile = Profile::Lan;
        assemble_cluster(cfg).await.expect("assemble node")
    }

    async fn wait_writable(node: &arachne_kv::server::AssembledClusterNode) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while node
            .handle
            .without_redirect()
            .put(b"hydra/probe/ready", b"1")
            .await
            .is_err()
        {
            assert!(Instant::now() < deadline, "node never accepted a write");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Encode and commit `cfg`: the two steps a publisher performs, so the tests read as
    /// "publish a config" instead of spelling the split out at every call site.
    async fn publish(store: &ArachneConfigStore, cfg: &ConfigData) -> String {
        let tree = encode_config(cfg, &fidelity(), &kp()).expect("encode");
        store.publish(&tree).await.expect("commit the config tree")
    }

    fn kp() -> crate::crypto::StaticKeyProvider {
        crate::crypto::StaticKeyProvider::new([7u8; 32], 1)
    }

    /// Fidelity rows with one DISABLED row: the shape a replica must be able to rebuild, and
    /// the reason the fidelity entity exists at all.
    fn fidelity() -> FidelityRows {
        FidelityRows {
            limit_roles: vec![hydra_core::model::LimitRole {
                id: "r-disabled".into(),
                name: "disabled".into(),
                matching_key: None,
                matching_model: None,
                matching_tenant: None,
                matching_provider: None,
                limit_count: None,
                limit_token: None,
                // "m", not "1m": the schema CHECKs the window vocabulary, and this fixture only
                // met that CHECK once a target actually WROTE its rows — a recording target
                // accepts anything.
                window: "m".into(),
                enabled: false,
                created_at: String::new(),
            }],
            ..FidelityRows::default()
        }
    }

    fn tenant(id: &str, domain: &str) -> Tenant {
        Tenant {
            id: id.to_string(),
            name: format!("name-{id}"),
            domain: domain.to_string(),
            auth_url: format!("https://auth.{domain}"),
            cert_key: None,
            cert_file: None,
            enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn provider(id: &str) -> Provider {
        Provider {
            id: id.to_string(),
            key: format!("key-{id}"),
            name: format!("provider-{id}"),
            endpoint: format!("https://{id}.example"),
            weight: 1,
            created_at: String::new(),
            updated_at: String::new(),
            max_concurrency: None,
            max_queue_depth: None,
            queue_wait_timeout_ms: None,
        }
    }

    fn config(tenants: &[(&str, &str)]) -> ConfigData {
        let mut cfg = ConfigData::default();
        for (id, domain) in tenants {
            cfg.tenants_by_domain
                .insert((*domain).to_string(), tenant(id, domain));
        }
        cfg.providers.insert("p1".into(), provider("p1"));
        cfg.reindex_tenants();
        cfg
    }

    /// The whole point: publish on one side, converge on the other, and the target receives
    /// the decoded config — with the watermark moving only after the apply.
    ///
    /// Falsification: move `succeeded` before `apply` and the "still not materialized after a
    /// failed apply" assertion in the next test fails; decode nothing and the tenant count is 0.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_published_config_reaches_the_target() {
        let node = node("basic", PORTS[0]).await;
        wait_writable(&node).await;
        let store = ArachneConfigStore::new(node.handle.clone());
        let target = Arc::new(RecordingTarget::default());
        let mut mat = Materializer::new(store.clone(), target.clone(), Arc::new(kp()));

        assert!(
            !mat.may_be_leader(),
            "a node that has materialized nothing must not be eligible to lead"
        );
        assert_eq!(
            mat.converge().await.expect("empty converge"),
            Converged::NoChange,
            "a fresh cluster has no head and must not be handed an empty config"
        );

        let cfg = config(&[("t1", "acme.example"), ("t2", "globex.example")]);
        let hash = publish(&store, &cfg).await;

        let outcome = mat.converge().await.expect("converge");
        assert_eq!(outcome, Converged::Applied { hash: hash.clone() });
        assert_eq!(mat.materialized(), Some(hash.as_str()));
        assert_eq!(target.applied_count(), 1, "the target must be called once");
        assert_eq!(
            target.tenants_seen(),
            2,
            "the target must receive the DECODED config, not a blob"
        );
        assert_eq!(
            target.fidelity_rows(),
            1,
            "the target must receive the fidelity rows TOO: `ConfigData` alone cannot rebuild a \
             replica, and the one row here is DISABLED, so it is absent from the decoded config \
             and present only in the fidelity entity"
        );
        assert!(
            mat.may_be_leader(),
            "an installed config makes this node electable"
        );

        // Steady state: the same head costs nothing and calls nothing.
        assert_eq!(mat.converge().await.expect("steady"), Converged::NoChange);
        assert_eq!(
            target.applied_count(),
            1,
            "no second apply for an unchanged head"
        );
    }

    /// A failed apply must NOT move the watermark, and the next pass must try again — after
    /// the backoff, not immediately.
    ///
    /// Falsification: call `succeeded` on the failure path and the "not materialized" assertion
    /// fails; drop the backoff and the `Deferred` assertion fails.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_apply_does_not_advance_the_watermark() {
        let node = node("failed", PORTS[1]).await;
        wait_writable(&node).await;
        let store = ArachneConfigStore::new(node.handle.clone());
        let target = Arc::new(RecordingTarget::default());
        let mut mat = Materializer::new(store.clone(), target.clone(), Arc::new(kp()));

        let cfg = config(&[("t1", "acme.example")]);
        let hash = publish(&store, &cfg).await;

        target.fail_next();
        let first = mat.converge().await;
        assert!(
            first.is_err(),
            "the apply failure must surface, got {first:?}"
        );
        assert_eq!(
            mat.materialized(),
            None,
            "a failed apply must NOT be recorded as materialized"
        );
        assert_eq!(target.applied_count(), 0);
        assert!(
            !mat.may_be_leader(),
            "a node that failed to install the config must not be electable"
        );

        // The gate now holds: a pass inside the backoff window does not retry.
        assert_eq!(
            mat.converge().await.expect("held"),
            Converged::Deferred { hash: hash.clone() },
            "the failure must start a backoff, not a hot loop"
        );

        // And after the window it retries — proving the hold expires.
        tokio::time::sleep(
            crate::cluster::arachne_materialize::RETRY_BACKOFF_MIN + Duration::from_millis(50),
        )
        .await;
        let retried = mat.converge().await.expect("retry");
        assert_eq!(retried, Converged::Applied { hash });
        assert_eq!(target.applied_count(), 1, "the retry must reach the target");
    }

    /// An edit re-materializes, and a publish that changes NO entity does not.
    ///
    /// The second half is the property that keeps the tree worth having: the config VERSION
    /// (`config_meta.config_version`) is monotone and bumps on every management write, and if it
    /// were part of what names the tree, every write — including one that changes nothing —
    /// would make every node in the cluster re-read the whole config. It is not: the tree is
    /// named by the ENTITY bytes, so a publish whose entities are identical is the same tree.
    ///
    /// Falsification: mix a version counter into the toc (`toc_of`) and the "no
    /// re-materialization" assertion fails — which is exactly the trap this pins.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_entity_edit_rematerializes_and_an_identical_publish_does_not() {
        let node = node("edit", PORTS[2]).await;
        wait_writable(&node).await;
        let store = ArachneConfigStore::new(node.handle.clone());
        let target = Arc::new(RecordingTarget::default());
        let mut mat = Materializer::new(store.clone(), target.clone(), Arc::new(kp()));

        let mut cfg = config(&[("t1", "acme.example")]);
        let first_hash = publish(&store, &cfg).await;
        mat.converge().await.expect("first");
        assert_eq!(target.applied_count(), 1);

        // Publishing the SAME config again (which a management write does after any change,
        // whether or not the change touched an entity) must produce the same tree name and
        // therefore re-materialize nothing.
        let same_hash = publish(&store, &cfg).await;
        assert_eq!(
            first_hash, same_hash,
            "a publish with identical entities must be the same tree"
        );
        assert_eq!(
            mat.converge().await.expect("converge"),
            Converged::NoChange,
            "an identical publish must not re-materialize anything"
        );
        assert_eq!(target.applied_count(), 1, "no apply for an unchanged tree");

        // An entity edit: the tree name changes and the config is applied again.
        cfg.tenants_by_domain
            .insert("initech.example".into(), tenant("t3", "initech.example"));
        cfg.reindex_tenants();
        let edited = publish(&store, &cfg).await;
        assert_ne!(edited, first_hash, "an edit must name a new tree");
        assert_eq!(
            mat.converge().await.expect("converge edit"),
            Converged::Applied { hash: edited }
        );
        assert_eq!(target.applied_count(), 2, "an edit must re-materialize");
        assert_eq!(
            target.tenants_seen(),
            2,
            "the target must see the new tenant"
        );
    }

    /// A malformed head is surfaced, not repaired, and changes nothing.
    ///
    /// Falsification: accept any head value and the `is_err` assertion fails.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_malformed_head_is_surfaced_and_changes_nothing() {
        let node = node("badhead", PORTS[3]).await;
        wait_writable(&node).await;
        let store = ArachneConfigStore::new(node.handle.clone());
        let target = Arc::new(RecordingTarget::default());
        let mut mat = Materializer::new(store.clone(), target.clone(), Arc::new(kp()));

        node.handle
            .without_redirect()
            .put(
                super::super::arachne_keys::ctl_head().as_bytes(),
                b"not-a-tree-name",
            )
            .await
            .expect("write a malformed head");

        let got = mat.converge().await;
        assert!(
            matches!(got, Err(MaterializeError::Head { .. })),
            "a malformed head must be refused, got {got:?}"
        );
        assert_eq!(mat.materialized(), None);
        assert_eq!(target.applied_count(), 0, "nothing may be applied");
    }

    /// A config with everything a replica must reproduce: the rows `ConfigData` keeps, the rows it
    /// throws away, and the two secret-bearing sets.
    fn rich_config() -> ConfigData {
        let mut cfg = ConfigData::default();
        cfg.tenants_by_domain
            .insert("acme.example".into(), tenant("t1", "acme.example"));
        cfg.providers.insert("p1".into(), provider("p1"));
        cfg.provider_keys
            .insert("p1".into(), vec!["sk-one".to_string()]);
        cfg.models_by_key.insert(
            "gpt-x".into(),
            vec![ModelProvider {
                provider_id: "p1".into(),
                weight: 1,
            }],
        );
        cfg.tenant_providers.insert(
            "t1".into(),
            std::collections::HashSet::from(["p1".to_string()]),
        );
        // The cert is the case that was MISSING from the tree until it was caught: the private key
        // is `skip_serializing`, so the entity has to carry it sealed.
        cfg.certs.insert(
            "acme.example".into(),
            CertMeta {
                domain: "acme.example".into(),
                cert_file: None,
                cert_key: None,
                cert_pem: Some(RICH_CERT_PEM.into()),
                cert_key_pem: Some(RICH_KEY_PEM.into()),
            },
        );
        cfg.reindex_tenants();
        cfg
    }

    const RICH_CERT_PEM: &str =
        "-----BEGIN CERTIFICATE-----\nMIIBacme\n-----END CERTIFICATE-----\n";
    const RICH_KEY_PEM: &str =
        "-----BEGIN PRIVATE KEY-----\nMIIEvQacme\n-----END PRIVATE KEY-----\n";

    /// The rows `ConfigData` is derived from and cannot express: a DISABLED role, a provider-key
    /// row with its identity, an OFFLINE model, a grant, and the token hash.
    fn rich_fidelity() -> FidelityRows {
        FidelityRows {
            limit_roles: fidelity().limit_roles,
            provider_keys: vec![ProviderKey {
                id: "pk-1".into(),
                provider_id: "p1".into(),
                api_key: "sk-one".into(),
                created_at: "2026-01-04 00:00:00".into(),
            }],
            tenant_token_hashes: vec![("t1".into(), "a".repeat(64))],
            provider_models: vec![
                ProviderModel {
                    id: "pm-online".into(),
                    key: "gpt-x".into(),
                    name: "GPT X".into(),
                    provider_id: "p1".into(),
                    status: 1,
                },
                ProviderModel {
                    id: "pm-offline".into(),
                    key: "gpt-old".into(),
                    name: "GPT old".into(),
                    provider_id: "p1".into(),
                    status: -1,
                },
            ],
            tenant_providers: vec![TenantProvider {
                id: "tp-1".into(),
                tenant_id: "t1".into(),
                provider_id: "p1".into(),
            }],
            ..FidelityRows::default()
        }
    }

    /// The real target, driven by the real loop over a real raft node: a config published on one
    /// side ends up as this node's SERVED config AND as rows in its own SQLite.
    ///
    /// The last assertion is the one that matters most: the replica's database, read back through
    /// the ordinary loader, reproduces the published config. That is what "a node can rebuild
    /// itself" means — and it fails if ANY part of the tree is incomplete (a disabled row, a
    /// provider-key id, an offline model, the tenant's TLS private key, a token hash).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_real_target_rebuilds_the_replica_and_serves_the_published_config() {
        let node = node("real-target", PORTS[4]).await;
        wait_writable(&node).await;
        let store = ArachneConfigStore::new(node.handle.clone());

        let cfg = rich_config();
        let rows = rich_fidelity();
        let key_provider = Arc::new(kp());
        // What a publisher does: encode (which SEALS the secrets deterministically, so this is
        // the same tree on every node) and then commit it.
        let tree = encode_config(&cfg, &rows, key_provider.as_ref()).expect("encode");
        let hash = store.publish(&tree).await.expect("commit the tree");

        // The node that will serve it: a fresh database, and a store over it. `ConfigStore::load`
        // is how a real node starts, so the target is exercised against the production type
        // rather than a test double.
        let replica_pool = pool().await;
        let replica_store = ConfigStore::load(replica_pool.clone(), key_provider.clone())
            .await
            .expect("load the replica store");
        assert!(
            replica_store.snapshot().tenants_by_domain.is_empty(),
            "fixture: the replica starts with nothing"
        );

        let target = Arc::new(ReplicaTarget::new(
            replica_store.clone(),
            key_provider.clone(),
        ));
        let mut mat = Materializer::new(store.clone(), target, key_provider.clone());

        assert_eq!(
            mat.converge().await.expect("converge"),
            Converged::Applied { hash: hash.clone() },
            "the loop must apply the tree the head names"
        );
        assert_eq!(mat.materialized(), Some(hash.as_str()));

        // 1) What this node SERVES is the published config, whole.
        assert_eq!(
            (**replica_store.snapshot()).clone(),
            cfg,
            "the materialized config must EQUAL the published one"
        );
        assert_eq!(
            replica_store.version(),
            1,
            "the watermark moves to the next value on the first materialization"
        );

        // 2) The FIDELITY rows are in the replica's own database...
        let roles = crate::db::list_limit_roles(&replica_pool, key_provider.as_ref())
            .await
            .expect("list limit roles");
        assert_eq!(
            roles.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["r-disabled"],
            "the DISABLED limit role is not in `ConfigData` at all and must still reach the \
             replica's table, or every materialization deletes it"
        );
        let keys = crate::db::list_provider_keys(&replica_pool, key_provider.as_ref())
            .await
            .expect("list provider keys");
        assert_eq!(
            keys.iter().map(|k| k.id.as_str()).collect::<Vec<_>>(),
            vec!["pk-1"],
            "the provider key keeps the leader's row identity"
        );
        assert_eq!(keys[0].created_at, "2026-01-04 00:00:00");
        let models = crate::db::list_provider_models(&replica_pool)
            .await
            .expect("list models");
        assert_eq!(
            models.len(),
            2,
            "the OFFLINE model is dropped by `models_by_key` and must still reach the replica"
        );
        let hashes = crate::db::list_tenant_access_token_hashes(&replica_pool)
            .await
            .expect("list token hashes");
        assert_eq!(
            hashes,
            vec![("t1".to_string(), "a".repeat(64))],
            "the tenant token hash rides the fidelity entity and is written verbatim"
        );

        // 3) ...and so is the cert private key (the defect that made the entity gain a sealed key).
        let cert = crate::db::get_tenant_cert(&replica_pool, key_provider.as_ref(), "t1")
            .await
            .expect("read the cert")
            .expect("the tenant row exists");
        assert_eq!(
            cert.cert_key_pem.as_deref(),
            Some(RICH_KEY_PEM),
            "the replica must keep the tenant's TLS private key"
        );

        // 4) THE STRONGEST ONE: the replica's database alone reproduces the config. A node that
        //    restarts and rebuilds from its own tables lands on exactly what the leader published.
        let rebuilt = crate::store::build_config(&replica_pool, key_provider.as_ref())
            .await
            .expect("the replica must be able to rebuild itself from its own database");
        assert_eq!(
            rebuilt, cfg,
            "reading the replica's OWN tables back through the loader must reproduce the \
             published config; any entity the tree failed to carry shows up here as a difference"
        );
    }

    // `a_target_without_a_database_refuses_rather_than_applying_in_memory_only` stood here. It
    // built a pool-less store through the deleted `ConfigStore::from_snapshot` and asserted that
    // `ReplicaTarget::apply` refused it with "no local config database". Neither the store nor the
    // refusal exists any more: every node has a database to materialize into (ADR-0001 D-2), and the
    // guard went with the possibility. Deleting the test rather than renaming it keeps the count
    // honest — it covered a shape the type no longer permits.

    /// THE CUTOVER, end to end: a local config change is PUBLISHED, and another node picks it up.
    ///
    /// This is the property plan T3.2 exists for, and it is asserted across two real stores and one
    /// real raft node, in the order production does it:
    ///
    /// 1. node A loads its store from its own database and attaches a publisher;
    /// 2. a write lands in A's database and `reload_all` runs (exactly what every admin handler
    ///    does after a committed transaction) — this PUBLISHES;
    /// 3. node B, which shares nothing with A but the raft cluster, converges and ends up serving
    ///    A's config with the row in its OWN database.
    ///
    /// It also pins the property that makes publishing affordable: A publishing the SAME config
    /// again does NOT move the head, because the tree's name is a function of the config (D-9). A
    /// non-deterministic seal would rename it here, and every publish would re-materialize the
    /// whole cluster.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_local_change_is_published_and_another_node_materializes_it() {
        let raft = node("publish", PORTS[5]).await;
        wait_writable(&raft).await;
        let ctl = ArachneConfigStore::new(raft.handle.clone());
        let key_provider: Arc<dyn crate::crypto::KeyProvider> = Arc::new(kp());

        // --- node A: its own database, its own store, and a publisher -------------------------
        let a_pool = pool().await;
        let a_store = ConfigStore::load(a_pool.clone(), key_provider.clone())
            .await
            .expect("load A")
            .with_publisher(Arc::new(
                crate::cluster::arachne_publish::ConfigPublisher::new(
                    ctl.clone(),
                    key_provider.clone(),
                ),
            ));
        assert_eq!(
            ctl.current_hash().await.expect("read head"),
            None,
            "fixture: nothing is published yet"
        );

        // The write: in production this is the admin handler's SQL transaction.
        crate::db::insert_provider(&a_pool, &provider("p1"))
            .await
            .expect("insert provider");

        assert!(
            a_store.reload_all().await.expect("reload + publish"),
            "a real change must reload"
        );
        let head = ctl
            .current_hash()
            .await
            .expect("read head")
            .expect("the reload must have PUBLISHED a head");
        assert_eq!(head.len(), 64, "the head is a content hash");

        // Republishing an unchanged config must not move the head: `changed` is false, so publish
        // is not even reached — and if it were, the tree name would be identical anyway.
        assert!(
            !a_store.reload_all().await.expect("second reload"),
            "no change means no publish"
        );
        assert_eq!(
            ctl.current_hash().await.expect("read head"),
            Some(head.clone()),
            "the head must not move for an unchanged config"
        );

        // --- node B: nothing but the raft cluster in common ----------------------------------
        let b_pool = pool().await;
        let b_store = ConfigStore::load(b_pool.clone(), key_provider.clone())
            .await
            .expect("load B");
        assert!(
            b_store.snapshot().providers.is_empty(),
            "fixture: B starts empty"
        );
        let target = Arc::new(ReplicaTarget::new(b_store.clone(), key_provider.clone()));
        let mut mat = Materializer::new(ctl.clone(), target, key_provider.clone());
        assert_eq!(
            mat.converge().await.expect("B converges"),
            Converged::Applied { hash: head.clone() },
            "B must materialize the head A published"
        );

        assert!(
            b_store.snapshot().providers.contains_key("p1"),
            "B must SERVE the config A published"
        );
        assert!(
            crate::db::get_provider(&b_pool, "p1").await.is_ok(),
            "B's own database must carry the row, or it could not rebuild itself"
        );
    }

    /// A publish that cannot go through is reported as "committed locally, NOT published".
    ///
    /// The local transaction cannot be rolled back at that point, so the honest answer is the one
    /// the admin layer turns into a 503: the change is on THIS node only. The trigger here is a
    /// value past the library's 1 MiB per-value cap — a real config can reach it (a provider with
    /// a huge name), and it fails at `put` time rather than at encode time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_publish_names_itself_and_keeps_serving_locally() {
        let raft = node("publish-fail", PORTS[6]).await;
        wait_writable(&raft).await;
        let ctl = ArachneConfigStore::new(raft.handle.clone());
        let key_provider: Arc<dyn crate::crypto::KeyProvider> = Arc::new(kp());

        let a_pool = pool().await;
        let a_store = ConfigStore::load(a_pool.clone(), key_provider.clone())
            .await
            .expect("load")
            .with_publisher(Arc::new(
                crate::cluster::arachne_publish::ConfigPublisher::new(
                    ctl.clone(),
                    key_provider.clone(),
                ),
            ));

        // Over the library's per-value cap (1 MiB), so the entity cannot be committed.
        let mut huge = provider("p1");
        huge.name = "x".repeat(2 * 1024 * 1024);
        crate::db::insert_provider(&a_pool, &huge)
            .await
            .expect("insert oversized provider");

        let got = a_store.reload_all().await;
        match got {
            // NB: `crate::store::StoreError` (the ConfigStore's) is a DIFFERENT type from
            // this module's `arachne_store::StoreError` — the publish step belongs to the former.
            Err(crate::store::StoreError::NotPublished { reason }) => {
                assert!(
                    !reason.is_empty(),
                    "the refusal must carry the underlying reason, or the 503 says nothing"
                );
            }
            other => panic!(
                "an uncommittable tree must surface as NotPublished (local commit stands, cluster \
                 does not have it), got {other:?}"
            ),
        }

        // The local side stands: the write committed, and this node serves it. That is precisely
        // why the error says "committed but NOT published" rather than "the write failed".
        assert!(
            a_store.snapshot().providers.contains_key("p1"),
            "the local commit must stand and be served"
        );
        assert_eq!(
            ctl.current_hash().await.expect("read head"),
            None,
            "nothing may be published when the tree cannot be encoded"
        );
    }

    /// Unwrap a committed tree read, failing loudly on `Empty`.
    fn expect_tree(outcome: ReadOutcome) -> ConfigTree {
        match outcome {
            ReadOutcome::Tree(t) => (*t).clone(),
            ReadOutcome::Empty => panic!("expected a committed tree, found an empty config"),
        }
    }

    /// `hydra_arachne_publish_total{result="refused"}` — the capacity alarm ADR-0001 risk R2
    /// asks for. Read from the process-wide registry, which this module's tests SHARE, so
    /// callers compare deltas and allow for a concurrent sibling's own refusal.
    fn refused_publish_count() -> u64 {
        crate::admin::metrics::render()
            .lines()
            .find_map(|l| l.strip_prefix("hydra_arachne_publish_total{result=\"refused\"} "))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    }

    /// AN OVER-CAP PUBLISH IS REFUSED WHOLE: nothing enters the log, the head does not move,
    /// the refusal is counted, and THIS node's local SQLite commit still stands.
    ///
    /// The shape is the one a big-but-legal config produces (plan 2026-10-10 A2): five
    /// providers whose names are 900 KiB each — every single value is UNDER the 1 MiB per-value
    /// cap (so nothing here is the encoder's fault; compare `a_failed_publish_names_itself_…`
    /// above, which trips that cap instead), but the missing batch totals ~4.4 MiB, over the
    /// library's `MAX_MULTI_PUT_TOTAL_BYTES` (4 MiB). The library validates the batch BEFORE
    /// proposing, so the whole batch stays out of the log: the head keeps its value and its
    /// origin index, the cluster keeps serving the previous tree (no half-write), this node
    /// keeps serving — and holding the row of — the providers its own transaction just
    /// committed, and `publish` counts `result="refused"` instead of an opaque error. That is
    /// exactly why the admin layer can answer "committed locally, NOT published" honestly.
    ///
    /// Falsification: split the batch to fit the cap and the head-index assertion fails as soon
    /// as any part of it commits; drop the `InvalidArgument → Refused` discrimination at the
    /// `multi_put` call site and the reason no longer carries the Refused wording nor the
    /// `refused` counter increment.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_over_cap_publish_is_refused_whole_and_the_local_commit_still_stands() {
        let raft = node("publish-over-cap", PORT_OVER_CAP).await;
        wait_writable(&raft).await;
        let ctl = ArachneConfigStore::new(raft.handle.clone());
        let key_provider: Arc<dyn crate::crypto::KeyProvider> = Arc::new(kp());
        let pool = pool().await;
        let store = ConfigStore::load(pool.clone(), key_provider.clone())
            .await
            .expect("load")
            .with_publisher(Arc::new(
                crate::cluster::arachne_publish::ConfigPublisher::new(
                    ctl.clone(),
                    key_provider.clone(),
                ),
            ));

        // Baseline: one small provider, published — so "nothing moved" is measurable.
        crate::db::insert_provider(&pool, &provider("p0"))
            .await
            .expect("insert p0");
        assert!(
            store.reload_all().await.expect("baseline publish"),
            "fixture: the baseline insert must change the config"
        );
        let (head_before, idx_before) = ctl
            .current_head_with_index()
            .await
            .expect("head read")
            .expect("the baseline must have published a head");
        let tree_before = expect_tree(ctl.read().await.expect("baseline read"));
        let refused_before = refused_publish_count();

        // Five providers × 900 KiB: each value fits, the batch does not.
        for i in 1..=5 {
            let mut p = provider(&format!("p{i}"));
            p.name = "x".repeat(900 * 1024);
            crate::db::insert_provider(&pool, &p)
                .await
                .expect("insert oversized provider");
        }

        match store.reload_all().await {
            // NB: `crate::store::StoreError` (the ConfigStore's) is a DIFFERENT type from
            // this module's `arachne_store::StoreError` — the publish step belongs to the former.
            Err(crate::store::StoreError::NotPublished { reason }) => {
                assert!(
                    reason.contains("MAX_MULTI_PUT_TOTAL_BYTES"),
                    "the refusal must name the batch cap that tripped — that is the pre-propose \
                     InvalidArgument, nothing entered the log — got: {reason}"
                );
                assert!(
                    reason.contains("nothing entered the log"),
                    "the reason must carry the Refused wording, got: {reason}"
                );
            }
            other => panic!(
                "an over-cap publish must surface as NotPublished (local commit stands, cluster \
                 does not have it), got {other:?}"
            ),
        }

        // The whole batch stayed OUT of the log: the head is byte-identical AND at the same
        // origin index — a partially applied batch would have moved at least one of them.
        let (head_after, idx_after) = ctl
            .current_head_with_index()
            .await
            .expect("head read")
            .expect("the head must still be there");
        assert_eq!(
            (head_after.as_str(), idx_after),
            (head_before.as_str(), idx_before),
            "the refused batch must not have entered the log: head value and origin index unmoved"
        );
        assert_eq!(
            expect_tree(ctl.read().await.expect("cluster read")),
            tree_before,
            "the cluster must still serve exactly the pre-refusal tree (no half-write)"
        );

        // The LOCAL side stands — the whole reason the error says "committed, not published".
        for i in 1..=5 {
            let id = format!("p{i}");
            assert!(
                crate::db::get_provider(&pool, &id).await.is_ok(),
                "the local row for {id} must stand: the SQLite commit is not rolled back"
            );
        }
        assert_eq!(
            store.snapshot().providers.len(),
            6,
            "this node must still SERVE all six providers (p0 + the five that failed to publish)"
        );

        // The capacity alarm fired: `refused`, not an opaque error. `>` (an increase) rather
        // than `==` because the process-wide counter is shared with the per-value-cap test
        // that can be running concurrently in this same binary.
        let refused_after = refused_publish_count();
        assert!(
            refused_after > refused_before,
            "publish must count a pre-propose refusal as result=\"refused\" (before \
             {refused_before}, after {refused_after})"
        );
    }

    /// A head read that hands out a scripted sequence — the ONE thing a real cluster cannot produce.
    struct ScriptedHead {
        seq: Mutex<std::collections::VecDeque<(String, u64)>>,
        last: Mutex<Option<(String, u64)>>,
    }

    impl ScriptedHead {
        fn new(seq: Vec<(String, u64)>) -> Self {
            Self {
                seq: Mutex::new(seq.into()),
                last: Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl HeadSource for ScriptedHead {
        async fn head_with_index(&self) -> Result<Option<(String, u64)>, String> {
            if let Some(next) = self.seq.lock().expect("lock").pop_front() {
                *self.last.lock().expect("lock") = Some(next.clone());
                return Ok(Some(next));
            }
            // Exhausted: keep answering the last value, so a stray extra converge cannot turn the test
            // into a different scenario.
            Ok(self.last.lock().expect("lock").clone())
        }
    }

    /// END-TO-END causal proof for the ordering: an OLDER hash at a LOWER index is refused, and the node
    /// keeps serving what it applied.
    ///
    /// Real here: both trees (published to a real cluster), the store's tree read, the apply, the
    /// predicate, the refusal, and the state that survives it. Injected: the two indices in the scripted
    /// head read — because a real cluster CANNOT produce this pair (`put` only ever raises a key's index;
    /// that monotonicity is the guarantee the ordering rides on), so only a lagging replica does, and a
    /// test has to stand in for it.
    ///
    /// Falsification: force `is_stale_generation` to `false` and the refusal disappears — the older tree
    /// is applied, `applied_count` becomes 2 and the served hash changes, so both assertions below fail.
    /// (Measured, not assumed: an earlier version of this test injected only the applied generation and
    /// stayed GREEN under that falsification, which is why the seam exists.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_older_hash_at_a_lower_index_is_refused_and_nothing_is_reapplied() {
        let node = node("stale-seam", PORT_STALE_SEAM).await;
        wait_writable(&node).await;
        let store = ArachneConfigStore::new(node.handle.clone());

        // Two REAL trees, published the normal way.
        let first = publish(&store, &config(&[("t1", "d1")])).await;
        let second = publish(&store, &config(&[("t1", "d1"), ("t2", "d2")])).await;
        assert_ne!(first, second, "fixture: two different trees");

        // The script: apply `second` at index 10, then read `first` at index 5 — a stale replica answering
        // the head read with an older tree.
        let head = Arc::new(ScriptedHead::new(vec![
            (second.clone(), 10),
            (first.clone(), 5),
        ]));
        let target = Arc::new(RecordingTarget::default());
        let mut mat =
            Materializer::with_head_source(head, store.clone(), target.clone(), Arc::new(kp()));

        assert_eq!(
            mat.converge().await.expect("first converge"),
            Converged::Applied {
                hash: second.clone()
            },
            "the newer tree is materialized first"
        );
        assert_eq!(target.applied_count(), 1);

        assert_eq!(
            mat.converge().await.expect("a stale head is not an error"),
            Converged::NoChange,
            "an older tree at a LOWER index must be refused: applying it would replace this node's \
             database and snapshot with an older config"
        );
        assert_eq!(
            target.applied_count(),
            1,
            "and nothing is applied a second time"
        );
        assert_eq!(
            mat.materialized(),
            Some(second.as_str()),
            "the node keeps serving the tree it had"
        );
    }
}
