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

    // tenants (+ certs meta from cert paths). domain is lowercased, incl. the
    // `localhost` special case (design §5.2).
    let mut tenants_by_domain: HashMap<String, hydra_core::model::Tenant> = HashMap::new();
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
                },
            );
        }
        tenants_by_domain.insert(domain, t);
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
    pool: SqlitePool,
    swrr: Arc<DashMap<(String, String), SwrrState>>,
    key_provider: Arc<dyn KeyProvider>,
}

impl ConfigStore {
    /// Build the initial snapshot from the DB and wrap it in `ArcSwap`.
    pub async fn load(
        pool: SqlitePool,
        key_provider: Arc<dyn KeyProvider>,
    ) -> Result<Self, StoreError> {
        let cfg = build_config(&pool, key_provider.as_ref()).await?;
        Ok(Self {
            inner: Arc::new(ArcSwap::from_pointee(cfg)),
            pool,
            swrr: Arc::new(DashMap::new()),
            key_provider,
        })
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

    /// Rebuild the snapshot from the DB and atomically swap it in (design §5.3).
    ///
    /// On a **fatal** validation issue the old snapshot is kept (`Err`
    /// returned, `inner` untouched). On success the new snapshot is published
    /// and the SWRR map is cleared so per-`(tenant, model)` weights are
    /// rebuilt lazily on the next request.
    pub async fn reload_all(&self) -> Result<(), StoreError> {
        let new_cfg = build_config(&self.pool, self.key_provider.as_ref()).await?;
        // Fatal validation surfaced as Err above → we never reach the store,
        // so the previous snapshot is preserved.
        self.inner.store(Arc::new(new_cfg));
        self.swrr.clear();
        Ok(())
    }
}
