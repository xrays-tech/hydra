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

    let cfg = ConfigData {
        tenants_by_domain,
        models_by_key,
        tenant_providers,
        tenant_models,
        providers,
        provider_keys,
        limit_roles,
        key_prefix_bindings,
        certs,
    };

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
fn is_usable_endpoint(endpoint: &str) -> bool {
    let lower = endpoint.to_ascii_lowercase();
    let rest = match lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
    {
        Some(r) => r,
        None => return false,
    };
    // Reject empty host (e.g. "https://") and an immediate path (e.g. "https:///x").
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    host_end > 0
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
    /// Monotonic config version (cluster P1): bumped on every local reload
    /// and set to the control-plane version on `apply_snapshot`. The control
    /// endpoint serves `?since=` against it; the control client skips
    /// re-applying unchanged snapshots.
    version: Arc<std::sync::atomic::AtomicU64>,
}

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
        let version = persisted.unwrap_or(1).max(1);
        Ok(Self {
            inner: Arc::new(ArcSwap::from_pointee(cfg)),
            pool: Some(pool),
            swrr: Arc::new(DashMap::new()),
            key_provider,
            version: Arc::new(std::sync::atomic::AtomicU64::new(version)),
        })
    }

    /// Build a store without a local DB (edge mode, cluster P0b): starts from
    /// a shipped snapshot (initially empty; the control client replaces it via
    /// [`Self::apply_snapshot`] once wired).
    #[must_use]
    pub fn from_snapshot(cfg: ConfigData, key_provider: Arc<dyn KeyProvider>) -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(cfg)),
            pool: None,
            swrr: Arc::new(DashMap::new()),
            key_provider,
            version: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
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
        self.version.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Atomically apply a snapshot received from the control plane (edge /
    /// standby, cluster P1). Same COW semantics as [`Self::reload_all`]:
    /// the swap is lock-free for readers and the SWRR map is cleared so stale
    /// per-`(tenant, model)` weights never survive a config change. The store
    /// adopts the control-plane `version` (monotonic across the cluster).
    pub fn apply_snapshot(&self, cfg: ConfigData, version: u64) {
        self.inner.store(Arc::new(cfg));
        self.swrr.clear();
        self.version
            .store(version, std::sync::atomic::Ordering::Release);
    }

    /// Rebuild the snapshot from the DB and atomically swap it in (design §5.3).
    ///
    /// On a **fatal** validation issue the old snapshot is kept (`Err`
    /// returned, `inner` untouched). On success the new snapshot is published
    /// (version bumped) and the SWRR map is cleared so per-`(tenant, model)`
    /// weights are rebuilt lazily on the next request. Snapshot-fed stores
    /// (edge) have no DB to reload from and return [`StoreError::NoDatabase`].
    pub async fn reload_all(&self) -> Result<(), StoreError> {
        let pool = self.pool.as_ref().ok_or(StoreError::NoDatabase)?;
        let new_cfg = build_config(pool, self.key_provider.as_ref()).await?;
        // Fatal validation surfaced as Err above → we never reach the store,
        // so the previous snapshot is preserved.
        self.inner.store(Arc::new(new_cfg));
        self.swrr.clear();
        let next = self
            .version
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            + 1;
        // Persist so a restart resumes at this version (monotonic watermark;
        // see `load`). Best-effort — the in-memory value still governs this
        // process; a failed write only risks a lower watermark after restart.
        if let Err(e) = db::set_config_version(pool, next).await {
            tracing::warn!(version = next, error = %e, "failed to persist config version");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hydra_core::config::ConfigData;

    fn kp() -> std::sync::Arc<dyn KeyProvider> {
        std::sync::Arc::new(crate::crypto::StaticKeyProvider::new([1u8; 32], 1))
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

    #[test]
    fn from_snapshot_serves_and_applies() {
        let mut c1 = ConfigData::default();
        cfg_with_tenant(&mut c1);
        let store = ConfigStore::from_snapshot(c1, kp());
        assert!(store.pool().is_none(), "snapshot-fed store has no DB");
        assert_eq!(store.version(), 1);

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
        store.apply_snapshot(c2, 42);
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
