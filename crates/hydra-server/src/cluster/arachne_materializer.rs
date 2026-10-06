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
//! Rebuild the local SQLite replica. Today `FidelityRows` reach the target but the SQLite
//! restore path is still driven by the snapshot channel; switching it over is the step this
//! module exists to make possible, and it is called out in the commit that adds this file
//! rather than left as an implied completion.

use std::sync::Arc;

use hydra_core::config::ConfigData;

use super::arachne_entities::{build_config_with_fidelity, split_config, tree_of, SealedMaterial};
use super::arachne_materialize::{MaterializeGate, Plan};
use super::arachne_store::{ArachneConfigStore, ConfigTree, ReadOutcome, StoreError};
use super::content::FidelityRows;

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
/// A trait rather than a direct `ConfigStore` call so the loop can be driven against a
/// recording target: the properties above are about ORDER (apply before watermark), which a
/// test that only inspected a live store could not observe.
pub trait MaterializeTarget: Send + Sync {
    /// Install `cfg` (and the fidelity rows a replica needs to rebuild its own SQLite tables)
    /// as the state this node serves.
    fn apply(&self, cfg: Arc<ConfigData>, fidelity: Arc<FidelityRows>) -> Result<(), String>;
}

/// The per-node loop: gate + store + target.
pub struct Materializer {
    store: ArachneConfigStore,
    gate: MaterializeGate,
    target: Arc<dyn MaterializeTarget>,
    /// Needed to open the sealed fidelity rows. Held rather than passed per call so a
    /// materializer cannot be driven without the key that unseals its own replica.
    key_provider: Arc<dyn crate::crypto::KeyProvider>,
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
            store,
            gate: MaterializeGate::new(),
            target,
            key_provider,
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
        let head = self
            .store
            .current_hash()
            .await
            .map_err(|e| MaterializeError::Head {
                reason: e.to_string(),
            })?;

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
                                self.target.apply(Arc::new(cfg), Arc::new(fidelity))
                            {
                                let wait = self.gate.failed(&hash);
                                return Err(MaterializeError::Unavailable {
                                    hash,
                                    reason: format!("{reason} (retry in {wait:?})"),
                                });
                            }
                            self.gate.succeeded(&hash);
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

/// Encode a config into the tree a publisher commits.
///
/// Encoding only: committing is `ArachneConfigStore::publish`, which is the single writer of the
/// head. Splitting the two keeps "what bytes describe this config" testable without a cluster.
///
/// # Errors
/// [`StoreError::Arachne`] when the codec refuses the config (an unkeyable id, a missing sealed
/// row, a seal that fails). All of them must stop the publish rather than degrade.
pub fn encode_config(
    cfg: &ConfigData,
    fidelity: &crate::cluster::content::FidelityRows,
    sealed: SealedMaterial,
) -> Result<ConfigTree, StoreError> {
    let blobs = split_config(cfg, fidelity, sealed)
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
    use hydra_core::model::{Provider, Tenant};

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

    impl MaterializeTarget for RecordingTarget {
        fn apply(&self, cfg: Arc<ConfigData>, fidelity: Arc<FidelityRows>) -> Result<(), String> {
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

    const PORTS: [u16; 4] = [18401, 18402, 18403, 18404];

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
        let tree = encode_config(cfg, &fidelity(), sealed_fixture()).expect("encode");
        store.publish(&tree).await.expect("commit the config tree")
    }

    fn kp() -> crate::crypto::StaticKeyProvider {
        crate::crypto::StaticKeyProvider::new([7u8; 32], 1)
    }

    /// Sealed ONCE and reused: the tree name must not move between publishes of the same rows
    /// (a fresh AES-GCM seal would use a new nonce and rename the tree every time).
    fn sealed_fixture() -> SealedMaterial {
        use std::sync::OnceLock;
        static SEALED: OnceLock<SealedMaterial> = OnceLock::new();
        SEALED
            .get_or_init(|| {
                SealedMaterial::seal_plaintext(
                    &config(&[("t1", "acme.example")]),
                    &fidelity(),
                    &kp(),
                )
                .expect("seal fixture")
            })
            .clone()
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
                window: "1m".into(),
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
}
