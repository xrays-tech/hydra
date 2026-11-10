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
//! an `HttpPeer`, so serving such a snapshot would route every request for
//! that provider to failure. A fatal issue aborts [`ConfigStore::reload_all`]
//! and keeps the previous snapshot (design §5.3).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use arc_swap::{ArcSwap, Guard};
use dashmap::DashMap;

use hydra_core::config::{validate, CertMeta, ConfigData, ModelProvider, Severity};
use hydra_core::identity::TenantTokenIdentity;
use hydra_core::model::{
    GlobalRole, LimitRole, Provider, ProviderKey, ProviderModel, Tenant, TenantAccessToken,
    TenantModel, TenantProvider, User, UserApiKey, UserGroup,
};
use hydra_core::swrr::SwrrState;

use crate::crypto::KeyProvider;
use crate::db::{self, Pool};

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
    /// applied (the caller keeps the previous one).
    #[error("fatal config validation: {0}")]
    FatalValidation(String),
}

// ---------------------------------------------------------------------------
// Cross-node config-change notifier seam (Phase 3a)
// ---------------------------------------------------------------------------

/// Cross-node config-change notification seam.
///
/// The transport (Phase 3b: Redis pub/sub) owns the node identity/origin; the
/// store only hands it the revision that was successfully applied locally. A
/// notifier is best-effort: a publish failure is logged and counted, never
/// returned to the writer, because the DB is already committed and the poll
/// fallback is the correctness backstop.
#[async_trait::async_trait]
pub trait ConfigNotifier: Send + Sync {
    /// Announce `version` to the other nodes. Implementations must be
    /// timeout-bounded; the caller holds the reload guard while this runs.
    async fn publish(&self, version: u64) -> Result<(), String>;
}

/// Where a reload was triggered from.
///
/// Only [`ReloadSource::Local`] — a write on THIS node (admin API, tenant API,
/// forced `POST /reload`) — may publish a change to peers. [`Self::PubSub`]
/// and [`Self::Poll`] are the QUIET paths: they consume another node's change
/// and must never relay it, or the fleet would rebroadcast the same revision
/// forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadSource {
    /// A local write/admin action; the only source allowed to publish.
    Local,
    /// A message from the config bus (Phase 3b subscriber).
    PubSub,
    /// The cheap revision probe (Phase 3b poller).
    Poll,
}

impl ReloadSource {
    /// Stable, bounded `source` label value for `hydra_config_reload_total`.
    fn metric_label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::PubSub => "pubsub",
            Self::Poll => "poll",
        }
    }

    /// The external reload paths are quiet by construction. Downgrade an
    /// accidental `Local` rather than risk a rebroadcast loop.
    fn quiet(self) -> Self {
        match self {
            Self::Local => Self::Poll,
            other => other,
        }
    }
}

// ---------------------------------------------------------------------------
// Loader: DB rows → ConfigData (+ load-time validation)
// ---------------------------------------------------------------------------

/// Build a [`ConfigData`] snapshot from the DB and run load-time validation
/// (design §5.3 `loader::build` + §5.4 `validate`).
///
/// Returns `Ok(ConfigData)` when the snapshot is serveable (non-fatal issues
/// are logged at `WARN`). Returns `Err(StoreError::FatalValidation)` when any
/// fatal issue is found, so [`ConfigStore::reload_all`] keeps the old snapshot.
pub async fn build_config(pool: &Pool, kp: &dyn KeyProvider) -> Result<ConfigData, StoreError> {
    let (rows, _version) = load_config_rows(pool, kp).await?;
    config_from_rows(&rows)
}

/// Every raw row set the loader needs, read at ONE applied database snapshot.
///
/// This is both the loader input and the store's change-detection state: a
/// reload compares the freshly-read rows against these to decide whether the
/// served config actually changed. All entity types derive `PartialEq`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfigRows {
    providers: Vec<Provider>,
    provider_models: Vec<ProviderModel>,
    provider_keys: Vec<ProviderKey>,
    tenants: Vec<Tenant>,
    tenant_certs: Vec<db::TenantCert>,
    tenant_providers: Vec<TenantProvider>,
    tenant_models: Vec<TenantModel>,
    limit_roles: Vec<LimitRole>,
    user_groups: Vec<UserGroup>,
    users: Vec<User>,
    user_api_keys: Vec<UserApiKey>,
    tenant_access_tokens: Vec<TenantAccessToken>,
    global_roles: Vec<GlobalRole>,
}

/// Read every config row AND the shared `config_revision.version` in one
/// read-only transaction. Returning the version with the rows is load-bearing
/// (Oracle P0-2): a revision read from a different snapshot can tag stale
/// content with a newer version and pin it forever.
///
/// The loader is READ-ONLY. Startup migrations (`seal_legacy_limit_keys`,
/// `backfill_legacy_certs`) run before the first load; a reader never writes.
async fn load_config_rows(
    pool: &Pool,
    kp: &dyn KeyProvider,
) -> Result<(ConfigRows, u64), StoreError> {
    // PostgreSQL READ COMMITTED would give each of the 14 statements a fresh
    // snapshot; the helper pins them to one REPEATABLE READ snapshot (MySQL's
    // default already is). Without this, a concurrent writer can yield a
    // revision tag and rows from different config states.
    let mut tx = db::begin_read_snapshot(pool).await?;
    let version = db::read_revision_on(&mut *tx).await?;

    let providers = db::list_providers_on(&mut *tx).await?;
    let provider_models = db::list_provider_models_on(&mut *tx).await?;
    let provider_keys = db::list_provider_keys_on(&mut *tx, kp).await?;
    let tenants = db::list_tenants_on(&mut *tx).await?;
    let tenant_certs = db::list_tenant_certs_on(&mut *tx, kp).await?;
    let tenant_providers = db::list_tenant_providers_on(&mut *tx).await?;
    let tenant_models = db::list_tenant_models_on(&mut *tx).await?;
    let limit_roles = db::list_limit_roles_on(&mut *tx, kp).await?;
    let user_groups = db::list_user_groups_on(&mut *tx).await?;
    let users = db::list_users_on(&mut *tx).await?;
    let user_api_keys = db::list_user_api_keys_on(&mut *tx).await?;
    let tenant_access_tokens = db::list_tenant_access_tokens_on(&mut *tx).await?;
    let global_roles = db::list_global_roles_on_conn(&mut tx).await?;
    tx.commit().await?;

    Ok((
        ConfigRows {
            providers,
            provider_models,
            provider_keys,
            tenants,
            tenant_certs,
            tenant_providers,
            tenant_models,
            limit_roles,
            user_groups,
            users,
            user_api_keys,
            tenant_access_tokens,
            global_roles,
        },
        version,
    ))
}

/// Derive the served [`ConfigData`] from one consistent row set.
///
/// The filtering asymmetry is load-bearing (plan §3.2):
///   * identity rows (groups/users/keys) load FULL — disabled/expired/delinquent
///     entities must stay KNOWN (401/402/403 by status, never "unknown" 401-
///     equivalent that lets a caller enumerate live keys);
///   * tenant_tokens load ENABLED-ONLY — a revoked tenant token is simply
///     GONE (the token index is not an enumeration surface);
///   * global_roles load ENABLED-ONLY — a disabled rule must match NOTHING,
///     not even as a failover candidate.
fn config_from_rows(rows: &ConfigRows) -> Result<ConfigData, StoreError> {
    // providers
    let providers: HashMap<String, _> = rows
        .providers
        .iter()
        .map(|p| (p.id.clone(), p.clone()))
        .collect();

    // models: only status == 1 (online) enter models_by_key (design §4.2).
    let mut models_by_key: HashMap<String, Vec<ModelProvider>> = HashMap::new();
    for m in &rows.provider_models {
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
    for k in &rows.provider_keys {
        provider_keys
            .entry(k.provider_id.clone())
            .or_default()
            .push(k.api_key.clone());
    }

    // tenants (+ certs meta). domain is lowercased, incl. the `localhost`
    // special case (design §5.2). Certs resolve content-first (migration
    // 0007); legacy path rows are carried too so the TLS layer can fall back.
    let mut tenants_by_domain: HashMap<String, Tenant> = HashMap::new();
    let mut tenant_domains: HashMap<String, String> = HashMap::new();
    let mut certs: HashMap<String, CertMeta> = HashMap::new();
    for t in &rows.tenants {
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
        tenants_by_domain.insert(domain, t.clone());
    }

    // Overlay stored cert content (migration 0007): a tenant with content in
    // the DB wins over its (possibly stale) legacy path fields. The loader
    // decrypts the sealed key at this boundary; plaintext lives in the
    // snapshot in-memory only.
    for tc in &rows.tenant_certs {
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
                cert_pem: tc.cert_pem.clone(),
                cert_key_pem: tc.cert_key_pem.clone(),
            },
        );
    }

    // tenant_providers
    let mut tenant_providers: HashMap<String, HashSet<String>> = HashMap::new();
    for tp in &rows.tenant_providers {
        tenant_providers
            .entry(tp.tenant_id.clone())
            .or_default()
            .insert(tp.provider_id.clone());
    }

    // tenant_models
    let mut tenant_models: HashMap<String, HashSet<String>> = HashMap::new();
    for tm in &rows.tenant_models {
        tenant_models
            .entry(tm.tenant_id.clone())
            .or_default()
            .insert(tm.model_key.clone());
    }

    // limit_roles: only enabled roles (design §5.2 "启用的限流角色").
    let limit_roles: Vec<LimitRole> = rows
        .limit_roles
        .iter()
        .filter(|r| r.enabled)
        .cloned()
        .collect();

    let user_groups: Vec<UserGroup> = rows.user_groups.clone();
    let users_map: HashMap<String, User> = rows
        .users
        .iter()
        .map(|u| (u.id.clone(), u.clone()))
        .collect();
    // FULL key set: reindex_identity derives the identity index from it; the
    // vector itself is NOT stored in ConfigData (the index is the carrier).
    let user_api_keys = &rows.user_api_keys;
    let mut tenant_tokens: HashMap<String, TenantTokenIdentity> = HashMap::new();
    for tok in &rows.tenant_access_tokens {
        if !tok.enabled {
            continue;
        }
        tenant_tokens.insert(
            tok.token_hash.clone(),
            TenantTokenIdentity {
                tenant_id: tok.tenant_id.clone(),
                token_name: tok.name.clone(),
                expires_at: tok.expires_at.clone(),
            },
        );
    }
    let global_roles: Vec<GlobalRole> = rows
        .global_roles
        .iter()
        .filter(|r| r.enabled)
        .cloned()
        .collect();

    let mut cfg = ConfigData {
        tenants_by_domain,
        // 派生索引：由紧随其后的 reindex_tenants 填充（唯一写入口，
        // 在 core 里）。
        tenants_by_id: HashMap::new(),
        models_by_key,
        tenant_providers,
        tenant_models,
        providers,
        provider_keys,
        limit_roles,
        // User hierarchy (plan §3.2): rows as loaded, then the derived identity
        // index below. Empty here too — the two reindex_* calls right after
        // reindex_tenants fill the three derived fields.
        user_groups,
        users: users_map,
        users_by_api_key_hash: HashMap::new(),
        tenant_tokens,
        global_roles,
        global_roles_by_tenant: HashMap::new(),
        global_roles_any_tenant: Vec::new(),
        certs,
    };

    // 派生索引在这里一次性建好：loader 是唯一的构建点。
    cfg.reindex_tenants();
    // Identity index + Global Role indexes (plan §3.2/§3.5): same single-entry-point
    // rule.
    cfg.reindex_identity(user_api_keys);
    cfg.reindex_global_roles();

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
    hydra_core::rewrite::EndpointUrl::parse(endpoint).is_some()
}

// ---------------------------------------------------------------------------
// ConfigStore — ArcSwap hot-reload shell (design §5.3)
// ---------------------------------------------------------------------------

/// Hot-read config centre. The snapshot is held behind [`ArcSwap`] (lock-free
/// reads on the hot path); [`reload_all`] does an atomic COW replacement.
///
/// The DB is the sole authority. A reload reads the rows and the shared
/// `config_revision.version` in one transaction; only an advanced revision (or
/// `force`) rebuilds and swaps the snapshot.
///
/// The SWRR state map is owned here and cleared on every successful reload
/// (design §5.3 / P1-B2: candidate sets may change, so stale per-`(tenant,
/// model)` weights must not survive a reload).
#[derive(Clone)]
pub struct ConfigStore {
    inner: Arc<ArcSwap<ConfigData>>,
    /// The shared relational-DB pool. Every node has one.
    pool: Pool,
    swrr: Arc<DashMap<(String, String), SwrrState>>,
    key_provider: Arc<dyn KeyProvider>,
    /// Snapshot-change hooks (审核四 P3). Every swap path funnels through
    /// [`Self::notify`], so a consumer that has to follow the snapshot cannot
    /// be forgotten by one of the writers.
    hooks: Arc<std::sync::Mutex<Vec<SnapshotHook>>>,
    /// Serializes reload coordination without changing reload semantics. Shared
    /// through `Arc` so `ConfigStore` clones guard the same lock.
    reload_lock: Arc<tokio::sync::Mutex<()>>,
    /// Cross-node change fanout (Phase 3a seam; the Redis implementation is
    /// injected in Phase 3b). `None` on a single-node deployment: a local
    /// write reload is then the whole story.
    notifier: Option<Arc<dyn ConfigNotifier>>,
    /// The rows matching the currently-served snapshot, used as the change
    /// signal across reloads. `None` for a store built from an in-memory
    /// `ConfigData` ([`Self::from_data`]), which forces the first reload to
    /// rebuild from the DB.
    last_rows: Arc<std::sync::Mutex<Option<ConfigRows>>>,
    /// Last-applied config version. Stored separately from the snapshot so
    /// [`Self::version`] is a cheap lock-free read; actual updates are
    /// serialized by the reload lock.
    version: Arc<AtomicU64>,
    /// `true` when the LAST reload failed, i.e. the in-memory snapshot may no
    /// longer match the committed config. Owned here (not by `AdminState`) so
    /// the tenant self-service write path gets the flag for free: every reload
    /// entry point funnels through this store.
    ///
    /// PROCESS-level and last-writer-wins, because the store is shared by every
    /// admin and tenant request. It answers "is the runtime consistent with the
    /// DB right now?", not "did MY write apply?".
    ///
    /// A failed reload sets it; only a successful rebuild + swap clears it. A
    /// no-op (`Ok(false)`) leaves it UNCHANGED: the rows looked unchanged, but
    /// that says nothing about whether the last rebuild succeeded (the previous
    /// failure may be why the snapshot is stale).
    snapshot_stale: Arc<AtomicBool>,
    /// Serialization observation used by the concurrency test. Public reloads
    /// must hold `reload_lock` for the whole rebuild, so `max_active` can never
    /// exceed 1. The atomics are untouched outside `cfg(test)`; keeping the
    /// field unconditional avoids a `#[cfg(test)]` FIELD attribute, which the
    /// repo's Rust blanker (scripts/rust_blank.test.cjs) would mis-handle.
    #[allow(dead_code)]
    reload_probe: Arc<ReloadProbe>,
}

/// Instrumentation proving that the public reload entry points serialize on
/// [`ConfigStore::reload_lock`]. Only the `#[cfg(test)]` branch in
/// [`ConfigStore::reload_all_locked`] touches it.
#[allow(dead_code)]
#[derive(Default)]
struct ReloadProbe {
    active: std::sync::atomic::AtomicUsize,
    max_active: std::sync::atomic::AtomicUsize,
    /// Artificial delay inside the critical section, in milliseconds. Zero
    /// outside the concurrency test so ordinary reload tests stay fast.
    delay_ms: AtomicU64,
}

/// A consumer that follows every snapshot swap (see
/// [`ConfigStore::on_snapshot_change`]).
pub type SnapshotHook = Arc<dyn Fn(&ConfigData) + Send + Sync>;

impl ConfigStore {
    /// Build the initial snapshot from the DB and wrap it in `ArcSwap`.
    pub async fn load(pool: Pool, key_provider: Arc<dyn KeyProvider>) -> Result<Self, StoreError> {
        Self::assemble(pool, None, key_provider).await
    }

    /// A store serving a config built IN MEMORY — the seam the tests use to
    /// drive the store with a `ConfigData` they assembled by hand.
    ///
    /// No rows are recorded, so the next reload rebuilds from the DB (which is
    /// the authority) rather than treating the injected config as the baseline.
    pub async fn from_data(
        pool: Pool,
        cfg: ConfigData,
        key_provider: Arc<dyn KeyProvider>,
    ) -> Result<Self, StoreError> {
        Self::assemble(pool, Some(cfg), key_provider).await
    }

    /// The ONE construction path.
    ///
    /// `prebuilt` is `Some` only from [`Self::from_data`], the test seam that
    /// injects a hand-made `ConfigData`. Otherwise the DB is read ONCE, the
    /// `ConfigData` is built from those rows, and the same rows are recorded as
    /// the change-detection baseline. The persisted version is always resumed
    /// from the DB.
    async fn assemble(
        pool: Pool,
        prebuilt: Option<ConfigData>,
        key_provider: Arc<dyn KeyProvider>,
    ) -> Result<Self, StoreError> {
        // The DB is the authority. The normal path reads rows + revision in ONE
        // transaction; the `from_data` test seam has no rows to pair, so it only
        // resumes the persisted watermark.
        let (cfg, rows, version) = match prebuilt {
            Some(cfg) => {
                let version = match db::get_config_version(&pool).await {
                    Ok(db::ConfigVersion::Value(v)) => v,
                    Ok(db::ConfigVersion::Absent) => 0,
                    Ok(db::ConfigVersion::Corrupt(raw)) => {
                        tracing::warn!(
                            target: "hydra::store",
                            raw = %raw,
                            "config_revision.version is not usable; starting from 0"
                        );
                        0
                    }
                    Err(e) => {
                        tracing::warn!(target: "hydra::store", error = %e, "could not read the persisted config version");
                        0
                    }
                };
                (cfg, None, version)
            }
            None => {
                let (rows, version) = load_config_rows(&pool, key_provider.as_ref()).await?;
                let cfg = config_from_rows(&rows)?;
                (cfg, Some(rows), version)
            }
        };
        // Publish the resumed watermark at boot so `/metrics` has a value even
        // before the first reload (Phase 3b adds the fanout watcher).
        #[cfg(feature = "proxy")]
        crate::admin::metrics::record_config_version(version);
        Ok(Self {
            inner: Arc::new(ArcSwap::from_pointee(cfg)),
            pool,
            swrr: Arc::new(DashMap::new()),
            key_provider,
            hooks: Arc::new(std::sync::Mutex::new(Vec::new())),
            reload_lock: Arc::new(tokio::sync::Mutex::new(())),
            notifier: None,
            last_rows: Arc::new(std::sync::Mutex::new(rows)),
            version: Arc::new(AtomicU64::new(version)),
            snapshot_stale: Arc::new(AtomicBool::new(false)),
            reload_probe: Arc::new(ReloadProbe::default()),
        })
    }

    /// Attach the cross-node config-change notifier (Phase 3b injects the
    /// Redis-backed implementation). Cloned with the store, so every clone
    /// publishes through the same handle.
    ///
    /// `None` (the default) keeps a single-node deployment dependency-free: the
    /// local reload after a write is all that is needed.
    #[must_use]
    pub fn with_notifier(mut self, n: Arc<dyn ConfigNotifier>) -> Self {
        self.notifier = Some(n);
        self
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

    /// The node's shared-backend pool.
    #[must_use]
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Current config version: the local last-applied version.
    ///
    /// Version 0 means "nothing applied yet" — the snapshot is present but
    /// empty, NOT absent.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// Cheap poll probe for the Phase 3b fallback: is the shared DB watermark
    /// ahead of the revision this node has applied?
    ///
    /// This is ONE `SELECT` — it does not rebuild the row set or decrypt any
    /// secrets. The poller calls the paired-read reload only when this is true.
    pub async fn has_newer_revision(&self) -> Result<bool, StoreError> {
        Ok(db::read_revision(&self.pool).await? > self.version())
    }

    /// Whether the last reload failed, leaving the served snapshot behind the
    /// committed DB. Set/cleared by [`Self::reload_all_locked`] (and therefore
    /// by every public reload entry point). See the field docs for the exact
    /// no-op semantics.
    #[must_use]
    pub fn snapshot_stale(&self) -> bool {
        self.snapshot_stale.load(Ordering::Acquire)
    }

    /// Register a hook that runs after **every** snapshot swap.
    ///
    /// Single owner for "something has to follow the config": the resolved TLS
    /// cert store is why it exists. The hook runs synchronously on the caller's
    /// thread, after the swap, with no lock of ours held — it must not block or
    /// panic (a panicking hook would abort a reload that already succeeded).
    pub fn on_snapshot_change(&self, hook: SnapshotHook) {
        let mut hooks = self.hooks.lock().unwrap_or_else(|e| e.into_inner());
        hooks.push(hook);
    }

    /// Acquire the reload coordination lock.
    pub async fn reload_guard(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.reload_lock.lock().await
    }

    /// Run every registered hook against the snapshot that was just swapped in.
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

    /// Rebuild the snapshot from the DB and atomically swap it in (design §5.3).
    ///
    /// On a **fatal** validation issue the old snapshot is kept (`Err`
    /// returned, `inner` untouched). On success the new snapshot is swapped in,
    /// the persisted version is advanced, and the SWRR map is cleared so
    /// per-`(tenant, model)` weights are rebuilt lazily on the next request.
    /// Returns whether the served config actually changed.
    /// Takes the reload lock internally, so a caller cannot forget it. Callers
    /// that must hold the lock across a write + reload use
    /// [`Self::reload_all_locked`] instead.
    pub async fn reload_all(&self) -> Result<bool, StoreError> {
        self.reload_all_with(false).await
    }

    /// [`Self::reload_all`] with the operator's "push this out anyway" flag.
    ///
    /// Takes the reload lock internally and delegates to
    /// [`Self::reload_all_locked`]. This is the LOCAL (notify) path: a forced
    /// `POST /reload` must let the other nodes know too.
    ///
    /// ORDER IS LOAD-BEARING:
    ///   1. read rows AND `config_revision.version` in one plain read transaction;
    ///   2. if the shared revision did not advance and `force` is false → `Ok(false)`;
    ///   3. for `force`, bump the shared revision in a write transaction first
    ///      (unless the paired read already saw a newer one), then re-read;
    ///   4. build + validate the new `ConfigData`;
    ///   5. swap the snapshot and record the rows/version, then run hooks;
    ///   6. on a LOCAL swap only, publish the new revision (best-effort).
    pub async fn reload_all_with(&self, force: bool) -> Result<bool, StoreError> {
        let guard = self.reload_lock.lock().await;
        self.reload_all_locked(force, &guard).await
    }

    /// Quiet reload for another node's change (Phase 3b pub/sub or poll).
    ///
    /// Acquires the reload lock internally and NEVER publishes: the change
    /// originated elsewhere, so relaying it would loop. Returns whether the
    /// local snapshot actually advanced.
    pub async fn reload_external(&self) -> Result<bool, StoreError> {
        self.reload_external_with_source(false, ReloadSource::Poll)
            .await
    }

    /// Quiet reload with the operator's force flag (poll probe / tests).
    pub async fn reload_external_with(&self, force: bool) -> Result<bool, StoreError> {
        self.reload_external_with_source(force, ReloadSource::Poll)
            .await
    }

    /// Quiet reload with an explicit trigger source, so Phase 3b can label a
    /// subscriber-driven reload `pubsub` and a probe-driven one `poll`.
    ///
    /// Takes the reload lock internally. The external path is quiet by
    /// construction; `Local` is downgraded so a caller cannot accidentally
    /// rebroadcast.
    pub async fn reload_external_with_source(
        &self,
        force: bool,
        source: ReloadSource,
    ) -> Result<bool, StoreError> {
        let guard = self.reload_lock.lock().await;
        self.reload_external_locked(force, &guard, source).await
    }

    /// Quiet reload **assuming the caller already holds [`Self::reload_guard`]**.
    ///
    /// Like [`Self::reload_all_locked`] but never notifies. Passing `Local`
    /// is downgraded to `Poll`, because a message/probe must never be relayed.
    pub async fn reload_external_locked(
        &self,
        force: bool,
        _guard: &tokio::sync::MutexGuard<'_, ()>,
        source: ReloadSource,
    ) -> Result<bool, StoreError> {
        self.reload_with_source(force, source.quiet()).await
    }

    /// Reload **assuming the caller already holds [`Self::reload_guard`]**.
    ///
    /// This is the only path that does not take the lock itself; passing the
    /// guard by reference makes that assumption explicit at the call site.
    /// Used by callers whose lock must span a DB write + the reload (admin
    /// mutations and the tenant self-service write path). It must never be
    /// called without the guard — doing so would let two reloads interleave
    /// read/compare/persist/swap.
    ///
    /// This is the LOCAL (write/admin) path, so a successful swap publishes the
    /// new revision to peers.
    ///
    /// Snapshot-staleness ownership lives here too: an `Err` marks the snapshot
    /// stale, a successful rebuild + swap clears it, and a no-op leaves it
    /// unchanged (see [`Self::snapshot_stale`]).
    pub async fn reload_all_locked(
        &self,
        force: bool,
        _guard: &tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<bool, StoreError> {
        // The locked write path is always local (Local → publish).
        self.reload_with_source(force, ReloadSource::Local).await
    }

    /// Instrumented reload wrapper shared by the local and quiet entry points.
    ///
    /// Owns the reload probe, the snapshot-staleness bookkeeping and the
    /// `hydra_config_reload_total` label; the source decides whether a
    /// successful swap is announced ([`ReloadSource::Local`]) or stays quiet.
    async fn reload_with_source(
        &self,
        force: bool,
        source: ReloadSource,
    ) -> Result<bool, StoreError> {
        #[cfg(test)]
        {
            let active = self.reload_probe.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.reload_probe
                .max_active
                .fetch_max(active, Ordering::SeqCst);
            let delay_ms = self.reload_probe.delay_ms.load(Ordering::SeqCst);
            if delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
        }

        let result = self.reload_impl(force, source).await;

        #[cfg(test)]
        self.reload_probe.active.fetch_sub(1, Ordering::SeqCst);

        match &result {
            // Only a successful rebuild + swap proves the runtime caught up.
            Ok(true) => {
                self.record_snapshot_outcome(false);
                #[cfg(feature = "proxy")]
                crate::admin::metrics::record_config_version(self.version());
            }
            // A no-op did not rebuild: the previous failure may still be the
            // reason the snapshot is stale, so leave the flag alone.
            Ok(false) => {}
            Err(_) => self.record_snapshot_outcome(true),
        }
        #[cfg(feature = "proxy")]
        crate::admin::metrics::record_config_reload(
            source.metric_label(),
            matches!(&result, Ok(true)),
        );
        result
    }

    /// The actual paired-read → revision gate → build → swap body. The caller
    /// owns the reload lock.
    ///
    /// `source` is load-bearing at the END of the function: after the swap and
    /// the snapshot hooks, only a `Local` change is published. Quiet
    /// (`PubSub`/`Poll`) reloads must not relay, or the fleet would loop.
    async fn reload_impl(&self, force: bool, source: ReloadSource) -> Result<bool, StoreError> {
        let local = self.version();
        let (mut rows, mut db_version) =
            load_config_rows(&self.pool, self.key_provider.as_ref()).await?;

        // Revision is the gate (never a separate content read): a reload only
        // rebuilds when the shared watermark advanced.
        if !force && db_version <= local {
            tracing::debug!(version = local, "reload: revision unchanged");
            return Ok(false);
        }

        // `force` means "advance the shared revision even if the content looks
        // the same". Bump inside a write transaction first, then re-read the
        // paired rows/version so this node and every other process converge.
        if force && db_version <= local {
            let _ = db::bump_config_revision(&self.pool).await?;
            (rows, db_version) = load_config_rows(&self.pool, self.key_provider.as_ref()).await?;
        }

        let new_cfg = config_from_rows(&rows)?;
        let new_version = db_version.max(local + 1);

        self.inner.store(Arc::new(new_cfg));
        {
            let mut last = self.last_rows.lock().unwrap_or_else(|e| e.into_inner());
            *last = Some(rows);
        }
        self.version.store(new_version, Ordering::Release);
        self.swrr.clear();
        // Followers of the snapshot (the TLS cert store) re-resolve here, so a
        // cert written through the admin API is live on the very next
        // handshake.
        self.notify(&self.snapshot());

        // Publish AFTER the local swap/hooks, while the reload guard is held.
        // Only an actually-applied LOCAL change is announced; a no-op, a
        // quiet external reload, and a failed reload never publish.
        if source == ReloadSource::Local {
            self.publish_notification(new_version).await;
        }
        Ok(true)
    }

    /// Best-effort announcement of a successfully applied local revision.
    ///
    /// A notify failure is a warning + metric, never an error: the DB write is
    /// committed and the local snapshot is live, and the Phase 3b poller is the
    /// correctness backstop. Redis publish is timeout-bounded, so holding the
    /// reload guard here is acceptable.
    async fn publish_notification(&self, version: u64) {
        let Some(notifier) = self.notifier.as_ref() else {
            return;
        };
        match notifier.publish(version).await {
            Ok(()) => {
                #[cfg(feature = "proxy")]
                crate::admin::metrics::record_config_notify(true);
            }
            Err(error) => {
                tracing::warn!(
                    target: "hydra::store",
                    version,
                    error = %error,
                    "config change notification failed; peers will converge by polling"
                );
                #[cfg(feature = "proxy")]
                crate::admin::metrics::record_config_notify(false);
            }
        }
    }

    /// Record the reload outcome in the single source of truth, and mirror it
    /// to the alertable gauge when the admin service is compiled in.
    fn record_snapshot_outcome(&self, stale: bool) {
        self.snapshot_stale.store(stale, Ordering::Release);
        #[cfg(feature = "proxy")]
        crate::admin::metrics::record_config_snapshot_stale(stale);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hydra_core::config::ConfigData;

    fn kp() -> std::sync::Arc<dyn KeyProvider> {
        std::sync::Arc::new(crate::crypto::StaticKeyProvider::new([1u8; 32], 1))
    }

    /// Insert one provider row so the next `reload_all` sees a REAL change.
    async fn seed_change(pool: &Pool, id: &str) {
        crate::db::insert_provider(
            pool,
            &hydra_core::model::Provider {
                id: id.to_string(),
                key: format!("k-{id}"),
                name: id.to_string(),
                endpoint: "http://127.0.0.1:1/".to_string(),
                weight: 1,
                created_at: "2026-01-01 00:00:00".to_string(),
                updated_at: "2026-01-01 00:00:00".to_string(),
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
                cert_key: None,
                cert_file: None,
                enabled: true,
                created_at: String::new(),
                updated_at: String::new(),
            },
        );
    }

    /// Record every publish without any transport. `calls` is the exactly-once
    /// assertion; `last` is the version the store announced.
    #[derive(Default)]
    struct MockNotifier {
        calls: AtomicU64,
        last: AtomicU64,
    }

    impl MockNotifier {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn calls(&self) -> u64 {
            self.calls.load(Ordering::SeqCst)
        }

        fn last(&self) -> u64 {
            self.last.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl ConfigNotifier for MockNotifier {
        async fn publish(&self, version: u64) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.last.store(version, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Every snapshot swap must notify the registered followers.
    ///
    /// This is the invariant that keeps the TLS cert store honest (审核四 P3).
    #[tokio::test]
    async fn every_snapshot_swap_notifies_followers() {
        let pool = crate::db::testing::setup_pool().await;
        let store = ConfigStore::load(pool.clone(), kp()).await.expect("load");

        let seen: Arc<std::sync::Mutex<Vec<usize>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = seen.clone();
        store.on_snapshot_change(Arc::new(move |cfg: &ConfigData| {
            // The hook must observe the NEW snapshot, not the old one.
            recorded
                .lock()
                .expect("hook mutex")
                .push(cfg.providers.len());
        }));

        seed_change(&pool, "p-notify").await;
        assert!(
            store.reload_all().await.expect("reload_all"),
            "a real DB change must swap the snapshot"
        );

        let seen = seen.lock().expect("hook mutex").clone();
        assert_eq!(seen, vec![1], "reload must notify followers");
    }

    /// Phase 3a: a LOCAL write + reload announces the new revision exactly once.
    #[tokio::test]
    async fn local_reload_publishes_exactly_once_with_the_new_version() {
        let pool = crate::db::testing::setup_pool().await;
        let notifier = MockNotifier::new();
        let store = ConfigStore::load(pool.clone(), kp())
            .await
            .expect("load")
            .with_notifier(notifier.clone());

        seed_change(&pool, "p-local-notify").await;
        assert!(
            store.reload_all().await.expect("reload_all"),
            "the seeded write must advance the revision"
        );

        assert_eq!(notifier.calls(), 1, "one local swap = exactly one publish");
        assert_eq!(
            notifier.last(),
            store.version(),
            "the announced version is the one just applied"
        );
    }

    /// Phase 3a: a no-op reload does not publish (nothing changed).
    #[tokio::test]
    async fn no_op_reload_does_not_publish() {
        let pool = crate::db::testing::setup_pool().await;
        let notifier = MockNotifier::new();
        let store = ConfigStore::load(pool.clone(), kp())
            .await
            .expect("load")
            .with_notifier(notifier.clone());

        assert!(
            !store.reload_all().await.expect("no-op reload"),
            "a fresh DB has no new revision"
        );
        assert_eq!(notifier.calls(), 0, "a no-op must not announce anything");
    }

    /// Phase 3a: the quiet external path swaps an advanced revision but NEVER
    /// publishes — this is what prevents a pub/sub/poll rebroadcast loop.
    #[tokio::test]
    async fn external_reload_swaps_without_publishing() {
        let pool = crate::db::testing::setup_pool().await;
        let notifier = MockNotifier::new();
        let store = ConfigStore::load(pool.clone(), kp())
            .await
            .expect("load")
            .with_notifier(notifier.clone());

        seed_change(&pool, "p-external").await;
        assert!(
            store.reload_external().await.expect("external reload"),
            "the external reload must apply the advanced revision"
        );
        assert!(
            store.snapshot().providers.contains_key("p-external"),
            "the quiet reload still swaps the snapshot"
        );
        assert_eq!(
            notifier.calls(),
            0,
            "an external change must never be relayed"
        );

        // An already-current external reload is a no-op, still silent.
        assert!(
            !store
                .reload_external()
                .await
                .expect("second external reload"),
            "nothing advanced since the last swap"
        );
        assert_eq!(notifier.calls(), 0);
    }

    /// Phase 3a: the poll probe is cheap and tracks the shared watermark.
    #[tokio::test]
    async fn has_newer_revision_tracks_the_shared_watermark() {
        let pool = crate::db::testing::setup_pool().await;
        let store = ConfigStore::load(pool.clone(), kp()).await.expect("load");

        assert!(
            !store.has_newer_revision().await.expect("probe"),
            "a fresh store matches the shared revision"
        );

        seed_change(&pool, "p-poll").await;
        assert!(
            store.has_newer_revision().await.expect("probe"),
            "a committed write must make the probe true"
        );

        assert!(store.reload_external().await.expect("quiet reload"));
        assert!(
            !store.has_newer_revision().await.expect("probe"),
            "after applying the revision the probe is false again"
        );
    }

    /// REVIEW N3 — `reload_all` must persist the version marker BEFORE it
    /// swaps in the new snapshot.
    ///
    /// The old test aborted the marker write with a SQLite trigger; that
    /// mechanism does not exist on PostgreSQL/MySQL. The backend-agnostic
    /// equivalent is the `#[cfg(test)]` persist hook in `db.rs`.
    #[tokio::test]
    async fn reload_all_persists_the_marker_before_swap() {
        let pool = crate::db::testing::setup_pool().await;
        let notifier = MockNotifier::new();
        let store = ConfigStore::load(pool.clone(), kp())
            .await
            .expect("load")
            .with_notifier(notifier.clone());
        let before = store.version();

        let failure = crate::db::test_fault::fail_persist();
        assert!(
            store.reload_all_with(true).await.is_err(),
            "an unpersistable version must fail the reload"
        );
        assert_eq!(
            notifier.calls(),
            0,
            "a failed reload must not announce a change"
        );
        assert!(
            store.snapshot_stale(),
            "a failed reload must mark the snapshot stale"
        );
        assert_eq!(
            store.version(),
            before,
            "the in-memory watermark must not advance past the durable one"
        );
        assert_eq!(
            crate::db::get_config_version(&pool)
                .await
                .expect("read marker"),
            crate::db::ConfigVersion::Value(before),
            "the durable marker must not advance when the persist step fails"
        );

        // Releasing the failure lets the same call succeed and advance both.
        drop(failure);
        store
            .reload_all_with(true)
            .await
            .expect("reload now succeeds");
        assert_eq!(store.version(), before + 1);
        assert_eq!(
            notifier.calls(),
            1,
            "the successful forced local reload announces once"
        );
        assert_eq!(
            crate::db::get_config_version(&pool)
                .await
                .expect("read marker"),
            crate::db::ConfigVersion::Value(before + 1),
            "marker and memory agree"
        );
    }

    /// A no-op reload returns `false` without bumping the version; `force`
    /// bypasses the row comparison.
    #[tokio::test]
    async fn no_op_reload_returns_false_and_force_bumps() {
        let pool = crate::db::testing::setup_pool().await;
        let store = ConfigStore::load(pool.clone(), kp()).await.expect("load");
        let before = store.version();

        assert!(
            !store.reload_all().await.expect("no-op reload"),
            "unchanged rows must not swap the snapshot"
        );
        assert_eq!(store.version(), before, "no-op reload must not bump");

        assert!(
            store.reload_all_with(true).await.expect("forced reload"),
            "force must swap even with unchanged rows"
        );
        assert_eq!(store.version(), before + 1, "force advances the version");
    }

    /// A changed DB reload bumps the version AND swaps the served snapshot.
    #[tokio::test]
    async fn changed_reload_bumps_version_and_swaps_snapshot() {
        let pool = crate::db::testing::setup_pool().await;
        let store = ConfigStore::load(pool.clone(), kp()).await.expect("load");
        let before = store.version();
        assert!(store.snapshot().providers.is_empty());

        seed_change(&pool, "p-changed").await;
        assert!(store.reload_all().await.expect("changed reload"));
        assert_eq!(store.version(), before + 1);
        assert!(
            store.snapshot().providers.contains_key("p-changed"),
            "the new snapshot must serve the row just written"
        );
    }

    /// A store built from a hand-made `ConfigData` serves it; the DB remains
    /// the authority, so a later reload rebuilds from the DB and clears SWRR.
    #[tokio::test]
    async fn from_data_serves_and_reload_rebuilds_from_db() {
        let pool = crate::db::testing::setup_pool().await;
        let mut c1 = ConfigData::default();
        cfg_with_tenant(&mut c1);
        let store = ConfigStore::from_data(pool, c1, kp())
            .await
            .expect("from_data");
        assert_eq!(store.version(), 0);
        assert!(store.snapshot().tenants_by_domain.contains_key("acme.com"));

        // Seed some SWRR state, then reload: the DB (empty) is the authority,
        // so the injected config is replaced and SWRR is cleared.
        store.swrr().insert(
            ("t1".into(), "gpt-4".into()),
            hydra_core::swrr::SwrrState::default(),
        );
        assert!(!store.swrr().is_empty());

        assert!(store.reload_all_with(true).await.expect("reload"));
        assert!(
            store.snapshot().tenants_by_domain.is_empty(),
            "reload replaced the injected config with the DB's"
        );
        assert!(store.swrr().is_empty(), "reload cleared SWRR state");
        assert_eq!(store.version(), 1, "the first DB-backed reload bumps");
    }

    /// P1-2: the store owns `snapshot_stale`, so the tenant write path (which
    /// never touches `AdminState`) gets the flag from the same reload that
    /// records it.
    ///
    /// A failed reload sets it; a successful rebuild + swap clears it; a no-op
    /// leaves it untouched because it did not rebuild.
    #[tokio::test]
    async fn reload_failure_marks_snapshot_stale_and_success_clears_it() {
        let pool = crate::db::testing::setup_pool().await;
        let store = ConfigStore::load(pool.clone(), kp()).await.expect("load");
        assert!(!store.snapshot_stale(), "a fresh store is not stale");

        let failure = crate::db::test_fault::fail_persist();
        assert!(
            store.reload_all_with(true).await.is_err(),
            "the marker write must fail the reload"
        );
        assert!(
            store.snapshot_stale(),
            "a failed reload must set the store-owned flag"
        );
        assert!(
            store.snapshot().providers.is_empty(),
            "a failed reload must keep the previously served snapshot"
        );

        // The revision did not move, so this is a no-op; it must NOT clear a
        // flag set by the failure above.
        assert!(
            !store.reload_all().await.expect("no-op reload"),
            "an unchanged revision is a no-op"
        );
        assert!(
            store.snapshot_stale(),
            "a no-op reload did not rebuild, so it must leave the stale flag alone"
        );

        drop(failure);
        assert!(
            store.reload_all_with(true).await.expect("forced reload"),
            "force rebuilds and swaps even when rows are unchanged"
        );
        assert!(
            !store.snapshot_stale(),
            "a successful rebuild + swap must clear the flag"
        );
    }

    /// P1-1: public reloads take the internal lock, so two concurrent calls
    /// cannot interleave read/compare/persist/swap.
    ///
    /// The test hook (compiled only under `#[cfg(test)]`) counts tasks inside
    /// the critical section and can stretch it; with the lock the maximum is
    /// 1. Without it both tasks would sleep concurrently and the maximum would
    /// be 2, which is exactly the stale-swap interleaving this guards against.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn public_reloads_serialize_on_the_internal_lock() {
        let pool = crate::db::testing::setup_pool().await;
        let store = ConfigStore::load(pool, kp()).await.expect("load");
        store.reload_probe.delay_ms.store(40, Ordering::SeqCst);

        let a = store.clone();
        let b = store.clone();
        let first = tokio::spawn(async move { a.reload_all_with(true).await });
        let second = tokio::spawn(async move { b.reload_all_with(true).await });
        let (first, second) = tokio::join!(first, second);
        first.expect("first task").expect("first reload");
        second.expect("second task").expect("second reload");

        assert_eq!(
            store.reload_probe.max_active.load(Ordering::SeqCst),
            1,
            "two public reloads overlapped inside the critical section"
        );
    }
}
