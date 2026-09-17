//! `ConfigStore` — the `ArcSwap<ConfigData>` hot-reload shell over the DB,
//! plus the loader (`row → ConfigData`) and load-time validation (design §5).
//!
//! ## Validation split (design §5.4)
//!
//! The pure data-graph checks live in [`hydra_core::config::validate`] (they
//! need no I/O). This module adds the loader-side checks that **do** need the
//! assembled graph but no external I/O — most importantly provider-endpoint
//! scheme sanity, which is the [`Severity::Fatal`] source today: a provider
//! whose endpoint is not a parseable `http://` / `https://` URL cannot become
//! an `HttpPeer`, so publishing such a snapshot would route every request for
//! that provider to failure. A fatal issue aborts [`ConfigStore::reload_all`]
//! and keeps the previous snapshot (design §5.3).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arc_swap::{ArcSwap, Guard};
use dashmap::DashMap;
use sqlx::SqlitePool;

use hydra_core::config::{validate, CertMeta, ConfigData, ModelProvider, Severity};
use hydra_core::model::{LimitRole, ProviderKeyBinding};
use hydra_core::swrr::SwrrState;

use crate::cluster::content::ReplicationContent;
use crate::cluster::snapshot::HydratedWire;
use crate::crypto::KeyProvider;
use crate::db;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors raised while building or hot-reloading a [`ConfigData`] snapshot.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("crypto: {0}")]
    Crypto(#[from] crate::crypto::CryptoError),
    /// One or more fatal validation issues were found; the snapshot was not
    /// published (the caller keeps the previous one).
    #[error("fatal config validation: {0}")]
    FatalValidation(String),
    /// The store was built snapshot-fed (edge mode) and has no local DB to
    /// rebuild from; `apply_snapshot` is the only mutation path.
    #[error("config store has no local database (edge/snapshot-fed mode)")]
    NoDatabase,
}

// ---------------------------------------------------------------------------
// Loader: DB rows → ConfigData (+ load-time validation)
// ---------------------------------------------------------------------------

/// Build a [`ConfigData`] snapshot from the DB and run load-time validation
/// (design §5.3 `loader::build` + §5.4 `validate`).
///
/// Returns `Ok(ConfigData)` when the snapshot is publishable (non-fatal issues
/// are logged at `WARN`). Returns `Err(StoreError::FatalValidation)` when any
/// fatal issue is found, so [`ConfigStore::reload_all`] keeps the old snapshot.
pub async fn build_config(
    pool: &SqlitePool,
    kp: &dyn KeyProvider,
) -> Result<ConfigData, StoreError> {
    // providers
    let mut providers: HashMap<String, _> = HashMap::new();
    for p in db::list_providers(pool).await? {
        providers.insert(p.id.clone(), p);
    }

    // models: only status == 1 (online) enter models_by_key (design §4.2).
    let mut models_by_key: HashMap<String, Vec<ModelProvider>> = HashMap::new();
    for m in db::list_provider_models(pool).await? {
        if m.status != 1 {
            continue;
        }
        let weight = providers.get(&m.provider_id).map(|p| p.weight).unwrap_or(0);
        models_by_key
            .entry(m.key.clone())
            .or_default()
            .push(ModelProvider {
                provider_id: m.provider_id.clone(),
                weight,
            });
    }

    // provider keys (decrypted at the DB boundary; plaintext lives in-memory only)
    let mut provider_keys: HashMap<String, Vec<String>> = HashMap::new();
    for k in db::list_provider_keys(pool, kp).await? {
        provider_keys
            .entry(k.provider_id.clone())
            .or_default()
            .push(k.api_key);
    }

    // tenants (+ certs meta). domain is lowercased, incl. the `localhost`
    // special case (design §5.2). Certs resolve content-first (migration
    // 0007); legacy path rows are carried too so the TLS layer can fall back.
    let mut tenants_by_domain: HashMap<String, hydra_core::model::Tenant> = HashMap::new();
    let mut tenant_domains: HashMap<String, String> = HashMap::new();
    let mut certs: HashMap<String, CertMeta> = HashMap::new();
    for t in db::list_tenants(pool).await? {
        let domain = t.domain.to_lowercase();
        if t.cert_file.is_some() || t.cert_key.is_some() {
            certs.insert(
                domain.clone(),
                CertMeta {
                    domain: domain.clone(),
                    cert_file: t.cert_file.clone(),
                    cert_key: t.cert_key.clone(),
                    cert_pem: None,
                    cert_key_pem: None,
                },
            );
        }
        tenant_domains.insert(t.id.clone(), domain.clone());
        tenants_by_domain.insert(domain, t);
    }

    // Overlay stored cert content (migration 0007): a tenant with content in
    // the DB wins over its (possibly stale) legacy path fields. The loader
    // decrypts the sealed key at this boundary; plaintext lives in the
    // snapshot in-memory only.
    for tc in db::list_tenant_certs(pool, kp).await? {
        if tc.cert_pem.is_none() {
            continue;
        }
        let Some(domain) = tenant_domains.get(&tc.tenant_id) else {
            continue;
        };
        certs.insert(
            domain.clone(),
            CertMeta {
                domain: domain.clone(),
                cert_file: None,
                cert_key: None,
                cert_pem: tc.cert_pem,
                cert_key_pem: tc.cert_key_pem,
            },
        );
    }

    // tenant_providers
    let mut tenant_providers: HashMap<String, HashSet<String>> = HashMap::new();
    for tp in db::list_tenant_providers(pool).await? {
        tenant_providers
            .entry(tp.tenant_id.clone())
            .or_default()
            .insert(tp.provider_id);
    }

    // tenant_models
    let mut tenant_models: HashMap<String, HashSet<String>> = HashMap::new();
    for tm in db::list_tenant_models(pool).await? {
        tenant_models
            .entry(tm.tenant_id.clone())
            .or_default()
            .insert(tm.model_key);
    }

    // limit_roles: only enabled roles (design §5.2 "启用的限流角色").
    let limit_roles: Vec<LimitRole> = db::list_limit_roles(pool)
        .await?
        .into_iter()
        .filter(|r| r.enabled)
        .collect();

    // provider_key_bindings: only enabled bindings participate (design §7.1b).
    let key_prefix_bindings: Vec<ProviderKeyBinding> = db::list_provider_key_bindings(pool)
        .await?
        .into_iter()
        .filter(|b| b.enabled)
        .collect();

    let mut cfg = ConfigData {
        tenants_by_domain,
        // 派生索引：由紧随其后的 reindex_tenants 填充（唯一写入口，
        // 在 core 里，与副本侧的 hydrate 共用同一实现）。
        tenants_by_id: HashMap::new(),
        models_by_key,
        tenant_providers,
        tenant_models,
        providers,
        provider_keys,
        limit_roles,
        key_prefix_bindings,
        certs,
    };

    // 派生索引在这里一次性建好：loader 是 leader 侧唯一的构建点，
    // 与副本侧 hydrate 的调用共用 core 里的同一个实现。
    cfg.reindex_tenants();

    validate_and_log(&cfg)?;
    Ok(cfg)
}

/// Run the pure [`validate`] plus the loader-side fatal checks.
///
/// Non-fatal (`Warn`) issues are logged; any fatal issue short-circuits with
/// [`StoreError::FatalValidation`] (deterministic message ordering so error
/// strings are stable).
fn validate_and_log(cfg: &ConfigData) -> Result<(), StoreError> {
    // Pure data-graph checks (all Warn today).
    for issue in validate(cfg) {
        debug_assert_eq!(issue.severity, Severity::Warn);
        tracing::warn!(target: "hydra::store", "config: {}", issue.message);
    }

    // Loader-side fatal checks (design §5.4): endpoint scheme must be a
    // usable http/https URL, otherwise the provider can never become a peer.
    let mut fatal: Vec<String> = Vec::new();
    for p in cfg.providers.values() {
        if !is_usable_endpoint(&p.endpoint) {
            fatal.push(format!(
                "provider '{}' (key='{}') has invalid endpoint '{}': must be an http:// or https:// URL",
                p.id, p.key, p.endpoint
            ));
        }
    }
    fatal.sort();

    if fatal.is_empty() {
        Ok(())
    } else {
        for msg in &fatal {
            tracing::error!(target: "hydra::store", "config (fatal): {msg}");
        }
        Err(StoreError::FatalValidation(fatal.join("; ")))
    }
}

/// Minimal endpoint sanity check (no `url` crate available under the `db`
/// feature): the scheme must be `http`/`https` and a non-empty host must
/// follow. The full URL→`{scheme,host,port}` parse is a W4 proxy concern.
///
/// `pub(crate)` so the **write boundary** (admin provider POST/PUT) can reject
/// exactly the set the loader treats as fatal. Before that call-site existed, a
/// typo such as `"api.openai.com"` (missing scheme) was persisted with a 201,
/// after which *every* later `reload_all` failed fatal validation: the snapshot
/// froze and key rotation / revocation silently stopped taking effect (audit
/// §3.14). Write-side and load-side now share one predicate by construction.
pub(crate) fn is_usable_endpoint(endpoint: &str) -> bool {
    // The loader, the admin write boundary and the upstream dialler all ask the
    // SAME question, through the SAME parser: can this endpoint become a peer?
    // The previous hand-rolled prefix test here was weaker than
    // `proxy::peer::parse_endpoint`, and the gap was reachable — see §21.
    hydra_core::rewrite::EndpointUrl::parse(endpoint).is_some()
}

// ---------------------------------------------------------------------------
// ConfigStore — ArcSwap hot-reload shell (design §5.3)
// ---------------------------------------------------------------------------

/// Hot-read config centre. The snapshot is held behind [`ArcSwap`] (lock-free
/// reads on the hot path); [`reload_all`] does an atomic COW replacement.
///
/// The SWRR state map is owned here and cleared on every successful reload
/// (design §5.3 / P1-B2: candidate sets may change, so stale per-`(tenant,
/// model)` weights must not survive a reload). The concurrent `CircuitBreaker`
/// wiring lands in W4; `reload_all` will also prune deleted-provider breaker
/// entries there — out of scope for this wave.
#[derive(Clone)]
pub struct ConfigStore {
    inner: Arc<ArcSwap<ConfigData>>,
    /// Local SQLite pool (leader/all mode). `None` on snapshot-fed stores
    /// (edge mode — no local config DB by design, cluster P0b).
    pool: Option<SqlitePool>,
    swrr: Arc<DashMap<(String, String), SwrrState>>,
    key_provider: Arc<dyn KeyProvider>,
    /// The replicated bytes, version included (cluster P1/P2). The generation
    /// predicate is defined HERE, so "the version advanced" and "the replicated
    /// content changed" are one statement, and [`Self::version`] DERIVES from it
    /// (a second `AtomicU64` would be a second owner — review C16).
    ///
    /// `Arc<ArcSwapOption<..>>`: the outer `Arc` is required because
    /// `ArcSwapAny` is not `Clone` and `ConfigStore` is `#[derive(Clone)]`; the
    /// `Option` is required because an edge store built by [`Self::from_snapshot`]
    /// has no fidelity rows until its first [`Self::apply_snapshot`].
    replication: Arc<arc_swap::ArcSwapOption<ReplicationContent>>,
    /// Snapshot-change hooks (审核四 P3). Every swap path funnels through
    /// [`Self::notify`], so a consumer that has to follow the snapshot cannot
    /// be forgotten by one of the writers.
    hooks: Arc<std::sync::Mutex<Vec<SnapshotHook>>>,
}

/// A consumer that follows every snapshot swap (see
/// [`ConfigStore::on_snapshot_change`]).
pub type SnapshotHook = Arc<dyn Fn(&ConfigData) + Send + Sync>;

impl ConfigStore {
    /// Build the initial snapshot from the DB and wrap it in `ArcSwap`
    /// (leader/all mode).
    pub async fn load(
        pool: SqlitePool,
        key_provider: Arc<dyn KeyProvider>,
    ) -> Result<Self, StoreError> {
        let cfg = build_config(&pool, key_provider.as_ref()).await?;
        // Resume the config version from the DB instead of restarting at 1:
        // a restarted leader otherwise serves a LOW version watermark and
        // peers (`since` comparison) never re-sync from it — even when its
        // snapshot is newer (accepted live: failover-then-rejoin regressed
        // the config). Monotonicity across restarts is what makes the
        // control channel's `?since=` watermark meaningful.
        let persisted = db::get_config_version(&pool).await.ok().flatten();
        // A missing marker means one of two very different things, and the
        // freshness gate depends on telling them apart:
        // - a brand-new DB (no config at all) → version 0: this node holds
        //   NOTHING, so an `UpToDate` poll must not count as "synced" (a node
        //   with an empty replica must not be eligible to lead — F-4);
        // - a DB with config but no marker (predates migration 0008, or was
        //   imported) → version 1: the cluster really does have config, so
        //   peers polling with `since = 0` are sent a full snapshot.
        let version = match persisted {
            Some(v) => v,
            None if db::config_content_exists(&pool).await.unwrap_or(false) => 1,
            None => 0,
        };
        // The replication content is built at construction (not lazily): a
        // leader that served a wire with EMPTY fidelity rows would instruct
        // every replica to wipe its fidelity tables and insert nothing.
        let content =
            ReplicationContent::load(&pool, key_provider.as_ref(), cfg.clone(), version).await?;
        Ok(Self {
            inner: Arc::new(ArcSwap::from_pointee(cfg)),
            pool: Some(pool),
            swrr: Arc::new(DashMap::new()),
            key_provider,
            replication: Arc::new(arc_swap::ArcSwapOption::from(Some(Arc::new(content)))),
            hooks: Arc::new(std::sync::Mutex::new(Vec::new())),
        })
    }

    /// Build a store without a local DB (edge mode, cluster P0b): starts from
    /// a shipped snapshot (initially empty; the control client replaces it via
    /// [`Self::apply_snapshot`] once wired).
    ///
    /// Version 0 = "this node holds nothing yet", so the first poll asks with
    /// `?since=0` and is always sent a full snapshot. Starting at 1 would let a
    /// node that has never synced claim to be current with a leader whose own
    /// first version is 1 (see [`Self::load`]).
    #[must_use]
    pub fn from_snapshot(cfg: ConfigData, key_provider: Arc<dyn KeyProvider>) -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(cfg)),
            pool: None,
            swrr: Arc::new(DashMap::new()),
            key_provider,
            // No pool on an edge ⇒ no fidelity rows yet. Left `None` on purpose:
            // only the first `apply_snapshot` (from a verified wire) fills it.
            replication: Arc::new(arc_swap::ArcSwapOption::empty()),
            hooks: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// The current replication content, version included (one atomic load).
    ///
    /// `None` until a node has content: on an edge that means "before the first
    /// `apply_snapshot`".
    #[must_use]
    pub fn replication(&self) -> Guard<Option<Arc<ReplicationContent>>> {
        self.replication.load()
    }

    /// Lock-free hot-path read. Returns a [`Guard`] that derefs to
    /// `Arc<ConfigData>`; callers may hold it for as long as a single request
    /// needs a consistent view.
    pub fn snapshot(&self) -> Guard<Arc<ConfigData>> {
        self.inner.load()
    }

    /// Handle to the SWRR state map. Exposed so W4 (and tests) can reach it
    /// for the per-request `order` transition and for `reload_all` assertions.
    pub fn swrr(&self) -> &Arc<DashMap<(String, String), SwrrState>> {
        &self.swrr
    }

    /// The local SQLite pool, when present (leader/all mode). `None` on
    /// snapshot-fed edge stores.
    #[must_use]
    pub fn pool(&self) -> Option<&SqlitePool> {
        self.pool.as_ref()
    }

    /// Current config version (cluster P1): the `since` watermark for the
    /// control channel and the local last-applied version on edges.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.replication.load().as_deref().map_or(0, |c| c.version)
    }

    /// Register a hook that runs after **every** snapshot swap.
    ///
    /// Single owner for "something has to follow the config": the resolved TLS
    /// cert store is why it exists. Wiring that re-resolution at each writer is
    /// how the cluster path was missed — an edge node applied a snapshot
    /// containing new tenant certs and kept serving the old ones until a
    /// restart (`main.rs` carried the fossil: `on_poll: None, // edge TLS cert
    /// re-resolution lands with the edge TLS wiring`). Both swap paths
    /// ([`Self::reload_all`], [`Self::apply_snapshot`]) funnel through
    /// [`Self::notify`], so a future writer cannot forget.
    ///
    /// The hook runs synchronously on the caller's thread, after the swap, with
    /// no lock of ours held — it must not block or panic (a panicking hook would
    /// abort a reload that already succeeded).
    pub fn on_snapshot_change(&self, hook: SnapshotHook) {
        let mut hooks = self.hooks.lock().unwrap_or_else(|e| e.into_inner());
        hooks.push(hook);
    }

    /// Run every registered hook against the snapshot that was just published.
    fn notify(&self, cfg: &ConfigData) {
        // Cloned out of the lock before calling: a hook may (re)register.
        let hooks: Vec<SnapshotHook> = {
            let hooks = self.hooks.lock().unwrap_or_else(|e| e.into_inner());
            hooks.clone()
        };
        for hook in hooks {
            hook(cfg);
        }
    }

    /// Atomically apply a snapshot received from the control plane (edge /
    /// standby, cluster P1). Same COW semantics as [`Self::reload_all`]:
    /// the swap is lock-free for readers and the SWRR map is cleared so stale
    /// per-`(tenant, model)` weights never survive a config change. The store
    /// adopts the control-plane `version` (monotonic across the cluster).
    pub fn apply_snapshot(&self, hydrated: HydratedWire) {
        let content = ReplicationContent::from_hydrated(
            hydrated.version,
            Arc::new(hydrated.cfg.clone()),
            hydrated.fidelity,
        );
        self.inner.store(Arc::new(hydrated.cfg));
        self.replication.store(Some(Arc::new(content)));
        self.swrr.clear();
        self.notify(&self.snapshot());
    }

    /// Rebuild the snapshot from the DB and atomically swap it in (design §5.3).
    ///
    /// On a **fatal** validation issue the old snapshot is kept (`Err`
    /// returned, `inner` untouched). On success the new snapshot is published
    /// (version bumped) and the SWRR map is cleared so per-`(tenant, model)`
    /// weights are rebuilt lazily on the next request. Snapshot-fed stores
    /// (edge) have no DB to reload from and return [`StoreError::NoDatabase`].
    /// Returns whether the REPLICATION CONTENT actually changed (plan T6).
    pub async fn reload_all(&self) -> Result<bool, StoreError> {
        self.reload_all_with(false).await
    }

    /// Reload from the DB and publish, advancing the generation **only when the
    /// replication content changed**.
    ///
    /// An unconditional bump made an idempotent `POST /reload` (and any write
    /// that touched no replicated column) rebuild every replica for nothing.
    /// `force` restores the operator's "push this out anyway".
    pub async fn reload_all_with(&self, force: bool) -> Result<bool, StoreError> {
        let pool = self.pool.clone().ok_or(StoreError::NoDatabase)?;
        let kp = self.key_provider.clone();
        let new_cfg = build_config(&pool, kp.as_ref()).await?;
        // Fatal validation surfaced as Err above → we never reach the store,
        // so the previous snapshot is preserved.
        //
        // ORDER IS LOAD-BEARING: `ReplicationContent` derives `PartialEq`
        // INCLUDING `version`, so the candidate is loaded at the CURRENT version
        // and compared before the new version is applied. Loading it at
        // `prev + 1` would make the comparison unequal always ⇒ the generation
        // would still advance on every reload (the defect this predicate
        // exists to remove).
        let prev_version = self.version();
        let candidate = ReplicationContent::load(&pool, kp.as_ref(), new_cfg, prev_version).await?;
        let changed = force || self.replication.load().as_deref() != Some(&candidate);

        if !changed {
            tracing::debug!(version = prev_version, "reload: no replicated change");
            return Ok(false);
        }

        let mut new_content = candidate;
        new_content.version = prev_version + 1;
        // PERSIST FIRST, THEN PUBLISH (review N3): a durable watermark that lags
        // the in-memory one reads as "this node is behind".
        db::set_config_version(&pool, new_content.version).await?;
        self.inner.store(new_content.cfg.clone());
        self.replication.store(Some(Arc::new(new_content)));
        self.swrr.clear();
        // Followers of the snapshot (the TLS cert store) re-resolve here, so a
        // cert written through the admin API is live on the very next
        // handshake — on every node role, including edge.
        self.notify(&self.snapshot());
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hydra_core::config::ConfigData;

    fn kp() -> std::sync::Arc<dyn KeyProvider> {
        std::sync::Arc::new(crate::crypto::StaticKeyProvider::new([1u8; 32], 1))
    }

    /// A hydrated wire with NO fidelity rows, for tests that only exercise the
    /// config swap. Production never builds one this way: the leader uses
    /// `ReplicationContent::load` and a replica uses `hydrate` on a real wire.
    fn hydrated(version: u64, cfg: ConfigData) -> HydratedWire {
        HydratedWire {
            version,
            cfg,
            fidelity: crate::cluster::content::FidelityRows {
                limit_roles: Vec::new(),
                key_prefix_bindings: Vec::new(),
                provider_keys: Vec::new(),
                tenant_token_hashes: Vec::new(),
                provider_models: Vec::new(),
                tenant_providers: Vec::new(),
                tenant_models: Vec::new(),
            },
        }
    }

    /// Insert one provider row so the next `reload_all` sees a REAL change.
    ///
    /// The generation now advances only when the replication content changes
    /// (plan T6), so a test that wants a notify/marker write must first make one.
    async fn seed_change(pool: &sqlx::SqlitePool, id: &str) {
        crate::db::insert_provider(
            pool,
            &hydra_core::model::Provider {
                id: id.to_string(),
                key: format!("k-{id}"),
                name: id.to_string(),
                endpoint: "http://127.0.0.1:1/".to_string(),
                weight: 1,
                created_at: String::new(),
                updated_at: String::new(),
                max_concurrency: None,
                max_queue_depth: None,
                queue_wait_timeout_ms: None,
            },
        )
        .await
        .expect("seed provider");
    }

    fn cfg_with_tenant(cfg: &mut ConfigData) {
        cfg.tenants_by_domain.insert(
            "acme.com".to_string(),
            hydra_core::model::Tenant {
                id: "t1".into(),
                name: "T".into(),
                domain: "acme.com".into(),
                auth_url: "https://auth.acme.com/v".into(),
                cert_key: None,
                cert_file: None,
                enabled: true,
                created_at: String::new(),
                updated_at: String::new(),
            },
        );
    }

    /// Every snapshot swap must notify the registered followers.
    ///
    /// This is the invariant that keeps the TLS cert store honest (审核四 P3).
    /// Re-resolving certs from the admin write path only is how the cluster
    /// path was missed: an edge applies snapshots through
    /// [`ConfigStore::apply_snapshot`] and never touches an admin handler, so a
    /// cert pushed through the control plane stayed invisible to the TLS
    /// callback until the process restarted. Both paths are asserted here, so a
    /// future writer that bypasses `notify` fails this test instead of shipping.
    #[tokio::test]
    async fn every_snapshot_swap_notifies_followers() {
        let pool = crate::db::init_pool("sqlite::memory:")
            .await
            .expect("init_pool");
        crate::db::run_migrate(&pool).await.expect("migrate");
        let store = ConfigStore::load(pool.clone(), kp()).await.expect("load");

        let seen: Arc<std::sync::Mutex<Vec<u64>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = seen.clone();
        store.on_snapshot_change(Arc::new(move |cfg: &ConfigData| {
            // The hook must observe the NEW snapshot, not the old one.
            recorded
                .lock()
                .expect("hook mutex")
                .push(cfg.tenants_by_domain.len() as u64);
        }));

        // Control-plane path (edge / standby): one config version.
        store.apply_snapshot(hydrated(7, ConfigData::default()));
        // Local rebuild path (admin write, `POST /api/v1/reload`). The content
        // must ACTUALLY change: the generation now advances — and followers are
        // notified — only when the replicated bytes differ (plan T6).
        seed_change(&pool, "p-notify").await;
        assert!(
            store.reload_all().await.expect("reload_all"),
            "a real DB change must advance the replication content"
        );

        let seen = seen.lock().expect("hook mutex").clone();
        assert_eq!(
            seen.len(),
            2,
            "both snapshot swap paths must notify their followers; saw {seen:?}"
        );
    }

    /// REVIEW N3 — `reload_all` must persist the version marker BEFORE it
    /// publishes the new snapshot.
    /// It used to swap the snapshot and advance the counter first, then write
    /// the marker "best-effort" (warn only). One failed write therefore left the
    /// in-memory version ahead of the durable one — and the evidence-based
    /// freshness gate (`replica::replica_is_current`) reads that as "this node's
    /// replica is behind", disqualifying a node whose replica is exactly what it
    /// serves. Failing the reload is recoverable; a silently diverging watermark
    /// is not.
    ///
    /// Fault injection is a real SQLite trigger (no mock).
    #[tokio::test]
    async fn reload_all_persists_the_marker_before_publishing() {
        let pool = crate::db::init_pool("sqlite::memory:")
            .await
            .expect("init_pool");
        crate::db::run_migrate(&pool).await.expect("migrate");
        let store = ConfigStore::load(pool.clone(), kp()).await.expect("load");
        let before = store.version();
        // Make a REAL change first: an unchanged store skips the marker write
        // entirely under the new predicate, which would make the fault
        // injection below vacuous (plan T6 step 7).
        seed_change(&pool, "p-marker").await;

        sqlx::query(
            "CREATE TRIGGER block_marker BEFORE INSERT ON config_meta \
             BEGIN SELECT RAISE(ABORT, 'oracle: marker write blocked'); END",
        )
        .execute(&pool)
        .await
        .expect("install trigger");

        assert!(
            store.reload_all().await.is_err(),
            "an unpersistable version must fail the reload"
        );
        assert_eq!(
            store.version(),
            before,
            "the in-memory watermark must not advance past the durable one"
        );

        // Dropping the trigger lets the same call succeed and advance both.
        sqlx::query("DROP TRIGGER block_marker")
            .execute(&pool)
            .await
            .expect("drop trigger");
        store.reload_all().await.expect("reload now succeeds");
        assert_eq!(store.version(), before + 1);
        assert_eq!(
            crate::db::get_config_version(&pool)
                .await
                .expect("read marker"),
            Some(before + 1),
            "marker and memory agree"
        );
    }

    #[test]
    fn from_snapshot_serves_and_applies() {
        let mut c1 = ConfigData::default();
        cfg_with_tenant(&mut c1);
        let store = ConfigStore::from_snapshot(c1, kp());
        assert!(store.pool().is_none(), "snapshot-fed store has no DB");
        // A snapshot-fed store starts at version 0: it holds nothing yet, so its
        // first control poll asks with `?since=0` and is sent a full snapshot.
        assert_eq!(store.version(), 0);

        // Initial snapshot is served.
        assert!(store.snapshot().tenants_by_domain.contains_key("acme.com"));

        // Seed some SWRR state, then apply a new snapshot: it replaces the
        // config AND clears SWRR (same semantics as reload_all).
        store.swrr().insert(
            ("t1".into(), "gpt-4".into()),
            hydra_core::swrr::SwrrState::default(),
        );
        assert!(!store.swrr().is_empty());

        let c2 = ConfigData::default();
        store.apply_snapshot(hydrated(42, c2));
        assert!(
            store.snapshot().tenants_by_domain.is_empty(),
            "apply_snapshot replaced the config"
        );
        assert!(store.swrr().is_empty(), "apply_snapshot cleared SWRR state");
        assert_eq!(
            store.version(),
            42,
            "apply_snapshot adopts the control-plane version"
        );
    }

    #[tokio::test]
    async fn reload_all_without_db_errors() {
        let store = ConfigStore::from_snapshot(ConfigData::default(), kp());
        assert!(matches!(
            store.reload_all().await,
            Err(StoreError::NoDatabase)
        ));
    }
}
