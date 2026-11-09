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
use hydra_core::identity::TenantTokenIdentity;
use hydra_core::model::{
    GlobalRole, LimitRole, Provider, ProviderKey, ProviderModel, Tenant, TenantAccessToken,
    TenantModel, TenantProvider, User, UserApiKey, UserGroup,
};
use hydra_core::swrr::SwrrState;

use crate::cluster::content::{FidelityRows, ReplicationContent};
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
    /// The config committed to the LOCAL database but could not be published to
    /// the control plane (Arachne), so the rest of the cluster will not see it.
    ///
    /// The wording is deliberate: the SQLite transaction DID commit, so this is
    /// not "the write failed" — it is "the write is local only". An operator who
    /// reads "database error" would look for a rollback that never happened.
    #[error(
        "the config was committed to the local database but NOT PUBLISHED to the cluster: {reason}"
    )]
    NotPublished { reason: String },
    /// Legacy config GC failed; the cluster keeps serving, and the next pass retries.
    #[error("config GC failed: {0}")]
    Gc(String),
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
    let rows = load_config_rows(pool, kp).await?;
    config_from_rows(&rows)
}

/// Load the DB rows ONCE and build both the served config and the fidelity rows
/// the control plane carries.
///
/// `reload_all_with` used to call [`build_config`] and then
/// [`ReplicationContent::load`], which each read the full hierarchy tables
/// (48k rows in the scale drill) — ~600 ms of duplicated SQLite work on every
/// management write. This helper reads everything in ONE read transaction, then
/// derives both views from those rows.
pub(crate) async fn build_config_with_fidelity(
    pool: &SqlitePool,
    kp: &dyn KeyProvider,
) -> Result<(ConfigData, FidelityRows), StoreError> {
    let rows = load_config_rows(pool, kp).await?;
    let cfg = config_from_rows(&rows)?;
    Ok((cfg, fidelity_from_rows(rows)))
}

/// Every raw row set the loader needs, read at ONE applied SQLite snapshot.
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

/// Read every config row in one read-only SQLite transaction.
///
/// One transaction is load-bearing: the served config and the fidelity rows must
/// describe the same revision, or a replica can be rebuilt with a user whose key,
/// group or role came from a different commit. `seal_legacy_limit_keys` is a
/// WRITE, so it runs before the transaction (the same order `build_config` used).
async fn load_config_rows(
    pool: &SqlitePool,
    kp: &dyn KeyProvider,
) -> Result<ConfigRows, StoreError> {
    // `seal_legacy_limit_keys` runs BEFORE the read, and it is the whole of the D-16 migration: a
    // `matching_key` written before 2026-10-08 is plaintext in the column, and this is the pass that
    // rewrites it (the loader runs at boot and on every reload, so an upgrade needs no operator
    // step). It is a separate call because it WRITES — see its own docs for why the reader cannot do
    // it. Note the order: sealing first means the rows this loader reads are already in the form a
    // replica will end up storing, so leader and replica cannot disagree about it.
    let resealed = db::seal_legacy_limit_keys(pool, kp).await?;
    if resealed > 0 {
        tracing::info!(
            target: "hydra::store",
            count = resealed,
            "sealed {} legacy plaintext `limit_role.matching_key` value(s) (decision D-16)",
            resealed
        );
    }

    let mut tx = pool.begin().await?;
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

    Ok(ConfigRows {
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
    })
}

/// Derive the served [`ConfigData`] from one consistent row set.
///
/// The filtering asymmetry is load-bearing (plan §3.2):
///   * identity rows (groups/users/keys) load FULL — disabled/expired/delinquent
///     entities must stay KNOWN (401/402/403 by status, never "unknown" 401-
///     equivalent that lets a replica enumerate live keys);
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
        // 在 core 里，与副本侧的 hydrate 共用同一实现）。
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

    // 派生索引在这里一次性建好：loader 是 leader 侧唯一的构建点，
    // 与副本侧 hydrate 的调用共用 core 里的同一个实现。
    cfg.reindex_tenants();
    // Identity index + Global Role indexes (plan §3.2/§3.5): same single-entry-point
    // rule; the replica hydrate path calls the same two functions (plan §3.5).
    cfg.reindex_identity(user_api_keys);
    cfg.reindex_global_roles();

    validate_and_log(&cfg)?;
    Ok(cfg)
}

/// Derive the FULL row sets the control plane carries from the same rows.
///
/// Unlike [`ConfigData`], fidelity rows keep disabled/expired rows: a replica must
/// rebuild the leader's tables byte-faithfully, including rows that no request
/// currently matches.
fn fidelity_from_rows(rows: ConfigRows) -> FidelityRows {
    FidelityRows {
        limit_roles: rows.limit_roles,
        provider_keys: rows.provider_keys,
        provider_models: rows.provider_models,
        tenant_providers: rows.tenant_providers,
        tenant_models: rows.tenant_models,
        user_groups: rows.user_groups,
        users: rows.users,
        user_api_keys: rows.user_api_keys,
        tenant_access_tokens: rows.tenant_access_tokens,
        global_roles: rows.global_roles,
    }
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
    /// The node's local SQLite pool.
    ///
    /// It used to be `Option`, because an `edge` node had no local config database by design
    /// (cluster P0b) and `from_snapshot` built one without a pool. That role was retired with the
    /// homogeneous topology (ADR-0001 D-2), and with it the possibility: **every node has a local
    /// database**, `main.rs` builds the store from `HYDRA_DB_URL` unconditionally, and the
    /// `if false { None } else { … }` that had kept the two shapes compiling was the last thing
    /// pretending otherwise. The guards that answered "no local database" —
    /// `tenant_api::local_write`'s `not_ready` and `ReplicaTarget::apply`'s refusal — were therefore
    /// unreachable, and are gone with the type: a check that cannot fail is not a check.
    pool: SqlitePool,
    swrr: Arc<DashMap<(String, String), SwrrState>>,
    key_provider: Arc<dyn KeyProvider>,
    /// The replicated bytes, version included (cluster P1/P2). The generation
    /// predicate is defined HERE, so "the version advanced" and "the replicated
    /// content changed" are one statement, and [`Self::version`] DERIVES from it
    /// (a second `AtomicU64` would be a second owner — review C16).
    ///
    /// `Arc<ArcSwap<..>>`: the outer `Arc` is required because `ArcSwapAny` is not `Clone` and
    /// `ConfigStore` is `#[derive(Clone)]`.
    ///
    /// It was an `ArcSwapOption` until 2026-10-05, on the theory that a store could exist "before
    /// its first snapshot". No path produces that state: [`Self::assemble`] builds the content at
    /// CONSTRUCTION (from the pool), [`Self::apply_snapshot`] replaces it, and the None was written
    /// by exactly one function — the deleted `from_snapshot` (the `edge` role). The `None` branch
    /// that readers had to handle was therefore about a shape no configuration can build, and the
    /// guards it fed (a `503 not_ready` in the tenant API, a skipped publish in
    /// [`Self::reload_all_with`]) could not fire: an EMPTY store is `Some(empty content at version
    /// 0)`, which is what a brand-new node actually holds, and it is a different state — one the
    /// code now has to answer for on its merits instead of pattern-matching it away.
    replication: Arc<arc_swap::ArcSwap<ReplicationContent>>,
    /// Snapshot-change hooks (审核四 P3). Every swap path funnels through
    /// [`Self::notify`], so a consumer that has to follow the snapshot cannot
    /// be forgotten by one of the writers.
    hooks: Arc<std::sync::Mutex<Vec<SnapshotHook>>>,
    /// Serializes local publish/read-modify-publish coordination without changing
    /// reload/publish semantics. Shared through `Arc` so `ConfigStore` clones (and
    /// therefore the whole store view) guard the same lock.
    publish_lock: Arc<tokio::sync::Mutex<()>>,
    /// Highest raft index this node has PUBLISHED locally. The materializer must
    /// not apply a head below it: a stale `get_stale` read after a local write
    /// used to let it rebuild the DB from an older tree and silently delete the
    /// just-committed rows (measured: random role losses under 2000-role load).
    local_write_index: Arc<std::sync::atomic::AtomicU64>,
    /// The head hash this node last published locally. On a FOLLOWER the local
    /// raft replica can lag; without this gate the materializer applies an older
    /// head and wipes the committed-but-not-yet-materialized write.
    pending_head: Arc<std::sync::Mutex<Option<String>>>,
    /// The Arachne publisher (ADR-0001), present only in a cluster node built with the `arachne`
    /// feature. `None` in single-node mode, where a local reload IS the whole truth.
    #[cfg(feature = "arachne")]
    publisher: Option<Arc<crate::cluster::arachne_publish::ConfigPublisher>>,
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
        let (cfg, fidelity) = build_config_with_fidelity(&pool, key_provider.as_ref()).await?;
        Self::assemble(pool, cfg, Some(fidelity), key_provider).await
    }

    /// A store serving a config built IN MEMORY, materialized into `pool` — the seam the tests use
    /// to drive the store with a `ConfigData` they assembled by hand, in place of the deleted
    /// `from_snapshot` (which took no pool, because the `edge` role had none).
    ///
    /// It takes the pool for the same reason every other path does: the store's claim is "the
    /// config THIS NODE serves is in THIS NODE's database", and a store built without one could not
    /// honour it — the fidelity rows, the version watermark and every `pool()` reader would have
    /// been describing a database that does not exist. Everything else is shared verbatim with
    /// [`Self::load`] (see [`Self::assemble`]), so a test store cannot end up with a shape
    /// production never builds.
    pub async fn from_data(
        pool: SqlitePool,
        cfg: ConfigData,
        key_provider: Arc<dyn KeyProvider>,
    ) -> Result<Self, StoreError> {
        Self::assemble(pool, cfg, None, key_provider).await
    }

    /// The ONE construction path: an already-built config, plus the pool it is (or is about to be)
    /// materialized into.
    ///
    /// `preloaded_fidelity` is `Some` only from [`Self::load`], which already read every row in one
    /// transaction. Passing it through avoids a second full hierarchy read at boot; `from_data`
    /// passes `None` and falls back to [`ReplicationContent::load`].
    async fn assemble(
        pool: SqlitePool,
        cfg: ConfigData,
        preloaded_fidelity: Option<FidelityRows>,
        key_provider: Arc<dyn KeyProvider>,
    ) -> Result<Self, StoreError> {
        // Resume the config version from the DB instead of restarting at 1:
        // a restarted leader otherwise serves a LOW version watermark and
        // peers (`since` comparison) never re-sync from it — even when its
        // snapshot is newer (accepted live: failover-then-rejoin regressed
        // the config). Monotonicity across restarts is what makes the
        // control channel's `?since=` watermark meaningful.
        let persisted = match db::get_config_version(&pool).await {
            Ok(v) => v,
            Err(e) => {
                // A read FAILURE is not "no marker": keep the loud path.
                tracing::warn!(target: "hydra::store", error = %e, "could not read the persisted config version");
                db::ConfigVersion::Absent
            }
        };
        // Three states, three different meanings — the freshness gate depends on
        // telling them apart:
        // - a parsed watermark → use it;
        // - no marker (predates migration 0008, or was imported) → version 1 if
        //   config content exists: the cluster really does have config, so peers
        //   polling with `since = 0` are sent a full snapshot;
        // - no marker AND no content (a brand-new DB) → version 0: this node
        //   holds NOTHING, so an `UpToDate` poll must not count as "synced" (an
        //   empty replica must not be eligible to lead — F-4);
        // - a CORRUPT marker → cannot be used as a watermark, but must not stop
        //   the data plane from serving valid config (that would turn metadata
        //   corruption into an outage). Fall back to the "content but no marker"
        //   value, loudly: `replica_is_current` already fails closed on Corrupt,
        //   so this node cannot lead until a materialization rewrites the marker.
        let has_content = db::config_content_exists(&pool).await.unwrap_or(false);
        let version = match persisted {
            db::ConfigVersion::Value(v) => v,
            db::ConfigVersion::Absent => u64::from(has_content),
            db::ConfigVersion::Corrupt(raw) => {
                tracing::warn!(
                    target: "hydra::store",
                    raw = %raw,
                    "config_meta.config_version is not a number; serving with the conservative \
                     watermark and refusing leader eligibility until it is rewritten"
                );
                u64::from(has_content)
            }
        };
        // The replication content is built at construction (not lazily): a
        // leader that served a wire with EMPTY fidelity rows would instruct
        // every replica to wipe its fidelity tables and insert nothing.
        let content = match preloaded_fidelity {
            // `ConfigStore::load` read the rows in one transaction; reuse them
            // instead of issuing a second full hierarchy read at boot.
            Some(fidelity) => ReplicationContent::from_loaded(cfg.clone(), fidelity, version),
            None => {
                ReplicationContent::load(&pool, key_provider.as_ref(), cfg.clone(), version).await?
            }
        };
        Ok(Self {
            inner: Arc::new(ArcSwap::from_pointee(cfg)),
            pool,
            swrr: Arc::new(DashMap::new()),
            key_provider,
            replication: Arc::new(arc_swap::ArcSwap::from_pointee(content)),
            hooks: Arc::new(std::sync::Mutex::new(Vec::new())),
            publish_lock: Arc::new(tokio::sync::Mutex::new(())),
            local_write_index: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            pending_head: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(feature = "arachne")]
            publisher: None,
        })
    }

    /// The current replication content, version included (one atomic load).
    ///
    /// NEVER absent: a store is built with content ([`Self::assemble`]) and every later swap
    /// ([`Self::apply_snapshot`], [`Self::reload_all_with`]) replaces it. "This node holds no
    /// config yet" is the EMPTY content at version 0, not a missing one — see the field's note for
    /// why that distinction had to become the caller's problem rather than a `None` to match on.
    #[must_use]
    pub fn replication(&self) -> Guard<Arc<ReplicationContent>> {
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

    /// The local SQLite pool: every node has one (see the field's note).
    #[must_use]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Current config version (cluster P1): the `since` watermark for the
    /// control channel and the local last-applied version.
    ///
    /// Version 0 means "nothing applied yet" — the content is present but empty (see
    /// [`Self::replication`]), NOT absent.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.replication.load().version
    }

    /// Register a hook that runs after **every** snapshot swap.
    ///
    /// Single owner for "something has to follow the config": the resolved TLS
    /// cert store is why it exists. Wiring that re-resolution at each writer is
    /// how the cluster path was missed — an edge node applies a snapshot
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

    /// One bounded legacy-config GC pass, serialized with local writes/publish.
    ///
    /// `None` off-cluster (no publisher). The control plane keeps old content-addressed
    /// entities/tocs forever otherwise; at scale their snapshots block the raft leader.
    #[cfg(feature = "arachne")]
    pub async fn gc(
        &self,
        max_deletes: usize,
    ) -> Result<Option<crate::cluster::arachne_store::GcStats>, StoreError> {
        let _guard = self.publish_guard().await;
        match &self.publisher {
            Some(p) => p.gc(max_deletes).await.map(Some).map_err(StoreError::Gc),
            None => Ok(None),
        }
    }

    /// The head this node published locally and has not yet observed applied.
    #[must_use]
    pub fn pending_head(&self) -> Option<String> {
        self.pending_head
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Clear the pending marker once the materializer has applied that head.
    pub fn clear_pending_head(&self, head: &str) {
        let mut guard = self.pending_head.lock().unwrap_or_else(|e| e.into_inner());
        if guard.as_deref() == Some(head) {
            *guard = None;
        }
    }

    /// Highest raft index this node has published locally (0 before any write).
    #[must_use]
    pub fn local_write_index(&self) -> u64 {
        self.local_write_index
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Acquire the publish coordination lock.
    pub async fn publish_guard(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.publish_lock.lock().await
    }

    /// The same lock, as an owned handle for a consumer that must hold it across
    /// an entire async loop (the materializer: head read -> plan -> apply).
    #[must_use]
    pub fn write_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(&self.publish_lock)
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

    /// Atomically apply a snapshot received from the control plane (the materializer, ADR-0001
    /// §3). Same COW semantics as [`Self::reload_all`]:
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
        self.replication.store(Arc::new(content));
        self.swrr.clear();
        self.notify(&self.snapshot());
    }

    /// Rebuild the snapshot from the DB and atomically swap it in (design §5.3).
    ///
    /// On a **fatal** validation issue the old snapshot is kept (`Err`
    /// returned, `inner` untouched). On success the new snapshot is published
    /// (version bumped) and the SWRR map is cleared so per-`(tenant, model)`
    /// weights are rebuilt lazily on the next request.
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
        let pool = self.pool.clone();
        let kp = self.key_provider.clone();
        let (new_cfg, fidelity) = build_config_with_fidelity(&pool, kp.as_ref()).await?;
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
        let candidate = ReplicationContent::from_loaded(new_cfg, fidelity, prev_version);
        let changed = force || self.replication.load().as_ref() != &candidate;

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
        self.replication.store(Arc::new(new_content));
        self.swrr.clear();
        // Followers of the snapshot (the TLS cert store) re-resolve here, so a
        // cert written through the admin API is live on the very next
        // handshake — on every node, the write and the apply being independent paths.
        self.notify(&self.snapshot());

        // ...and now the CLUSTER. Order (local swap, then publish) is deliberate: the SQLite
        // transaction committed before any of this, so this node's own state must be consistent
        // with its own database whether or not the cluster accepts the publish. A failure here is
        // returned as `NotPublished` — the admin layer turns that into a 503 saying the change is
        // local only — and the local config STAYS SERVED by this node until some other node's
        // publish moves the head, at which point the materializer rebuilds this node from that
        // head and the un-published change is gone. That is the correct direction (the head is
        // the authority) and it is why the 503 must not be silent.
        #[cfg(feature = "arachne")]
        if let Some(publisher) = &self.publisher {
            let content = self.replication();
            let (published_head, final_index) = publisher
                .publish_with_index(&content.cfg, content.fidelity())
                .await
                .map_err(|reason| StoreError::NotPublished { reason })?;
            *self.pending_head.lock().unwrap_or_else(|e| e.into_inner()) = Some(published_head);
            // The final manifest/head batch's index IS the tree generation. Record it directly:
            // a follow-up head read could race a newer publish and watermark this node with the
            // wrong generation.
            self.local_write_index
                .store(final_index, std::sync::atomic::Ordering::Release);
        }
        Ok(true)
    }

    /// Attach the Arachne publisher (ADR-0001, plan T3.2).
    ///
    /// A builder rather than a `load` parameter: single-node and non-`arachne` builds have no
    /// publisher at all, and the call site that HAS one is the cluster bootstrap, which is also the
    /// only place that knows whether the control plane started.
    #[cfg(feature = "arachne")]
    #[must_use]
    pub fn with_publisher(
        mut self,
        publisher: Arc<crate::cluster::arachne_publish::ConfigPublisher>,
    ) -> Self {
        self.publisher = Some(publisher);
        self
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
                provider_keys: Vec::new(),
                provider_models: Vec::new(),
                tenant_providers: Vec::new(),
                tenant_models: Vec::new(),
                ..crate::cluster::content::FidelityRows::default()
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
    /// path was missed: a node applies snapshots through
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

        // Control-plane path (materialized snapshot): one config version.
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
            crate::db::ConfigVersion::Value(before + 1),
            "marker and memory agree"
        );
    }

    /// A store built from a hand-made `ConfigData` serves it, swaps it, and is backed by the pool it
    /// was handed.
    ///
    /// This replaces `from_snapshot_serves_and_applies`, whose first assertion was
    /// `store.pool().is_none()` — the store type itself asserted the retired "no local database"
    /// shape. The rest of the test is unchanged, because the behavior under test (serve, then
    /// replace on `apply_snapshot`, clearing SWRR) was never about that shape.
    #[tokio::test]
    async fn from_data_serves_and_applies() {
        let pool = crate::db::init_pool("sqlite::memory:")
            .await
            .expect("init_pool");
        crate::db::run_migrate(&pool).await.expect("migrate");
        let mut c1 = ConfigData::default();
        cfg_with_tenant(&mut c1);
        let store = ConfigStore::from_data(pool, c1, kp())
            .await
            .expect("from_data");
        // The store's pool is a real handle to a real (empty) database, which is the point: it is
        // the database `version()` reports on and the one every `pool()` reader queries.
        assert!(
            !crate::db::config_content_exists(store.pool())
                .await
                .expect("content probe"),
            "a config injected in memory is NOT in the database until a write puts it there"
        );
        // Version 0: the database carries no watermark yet, so this node's first control poll asks
        // with `?since=0` and is sent a full snapshot.
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

    // `reload_all_without_db_errors` stood here: it built a pool-less store and asserted
    // `StoreError::NoDatabase`. Both the constructor and the variant are gone — a store without a
    // database is not a state this process can be in — so the test went with them rather than
    // being rewritten to assert something else under the same name.

    /// The combined reload path must derive the SAME config and fidelity rows as
    /// the two separate loaders it replaced.
    ///
    /// `reload_all_with` used to call `build_config` and then
    /// `ReplicationContent::load`, reading the hierarchy twice. The one-read
    /// version is only a win if it is not a silent divergence. The fixture
    /// carries enabled AND disabled hierarchy rows plus an offline provider
    /// model, so the load-bearing filtering asymmetry (full fidelity rows vs
    /// enabled-only served config) participates in the comparison.
    #[tokio::test]
    async fn combined_loader_matches_the_separate_loaders() {
        use hydra_core::model::{
            BillingStatus, GlobalRole, LimitRole, ProviderKey, ProviderModel, Tenant,
            TenantAccessToken, UserApiKey,
        };

        let pool = crate::db::init_pool("sqlite::memory:")
            .await
            .expect("init_pool");
        crate::db::run_migrate(&pool).await.expect("migrate");

        crate::db::insert_tenant(
            &pool,
            &Tenant {
                id: "t1".into(),
                name: "T".into(),
                domain: "acme.com".into(),
                cert_key: None,
                cert_file: None,
                enabled: true,
                created_at: String::new(),
                updated_at: String::new(),
            },
        )
        .await
        .expect("tenant");
        crate::db::upsert_user_group(
            &pool,
            &hydra_core::model::UserGroup {
                id: "g1".into(),
                tenant_id: "t1".into(),
                name: "g".into(),
                enabled: true,
                created_at: String::new(),
                updated_at: String::new(),
            },
        )
        .await
        .expect("group");
        crate::db::upsert_user(
            &pool,
            &User {
                id: "u1".into(),
                tenant_id: "t1".into(),
                user_group_id: "g1".into(),
                external_id: None,
                name: "u".into(),
                enabled: true,
                billing_status: BillingStatus::Active,
                billing_reason: None,
                billing_updated_at: None,
                created_at: String::new(),
                updated_at: String::new(),
            },
        )
        .await
        .expect("user");
        crate::db::insert_user_api_key(
            &pool,
            &UserApiKey {
                id: "k1".into(),
                user_id: "u1".into(),
                name: "k".into(),
                key_hash: "hash-k1".into(),
                key_prefix: "hk_".into(),
                key_last4: "k1".into(),
                enabled: true,
                expires_at: None,
                created_at: String::new(),
                updated_at: String::new(),
            },
        )
        .await
        .expect("key");

        // Provider row + two keys sharing a second-granularity `created_at`:
        // the loader's total order (`, id` tie-breaker) must be identical in
        // both paths.
        seed_change(&pool, "p-combined").await;
        for (id, api_key) in [("pk1", "sk-one"), ("pk2", "sk-two")] {
            crate::db::insert_provider_key(
                &pool,
                kp().as_ref(),
                &ProviderKey {
                    id: id.into(),
                    provider_id: "p-combined".into(),
                    api_key: api_key.into(),
                    created_at: "2026-01-01 00:00:00".into(),
                },
            )
            .await
            .expect("provider key");
        }
        // One online model enters `models_by_key`; the offline row is dropped
        // from the served config but MUST stay in the fidelity rows.
        for (id, key, status) in [("pm-online", "m-online", 1), ("pm-offline", "m-offline", 0)] {
            crate::db::insert_provider_model(
                &pool,
                &ProviderModel {
                    id: id.into(),
                    key: key.into(),
                    name: key.into(),
                    provider_id: "p-combined".into(),
                    status,
                },
            )
            .await
            .expect("provider model");
        }
        // Enabled + disabled limit roles / tokens / global roles: the served
        // config keeps only the enabled ones; fidelity keeps BOTH.
        for (id, enabled) in [("lr-on", true), ("lr-off", false)] {
            crate::db::insert_limit_role(
                &pool,
                kp().as_ref(),
                &LimitRole {
                    id: id.into(),
                    name: id.into(),
                    matching_key: None,
                    matching_model: None,
                    matching_tenant: None,
                    matching_provider: None,
                    limit_count: Some(1),
                    limit_token: None,
                    window: "m".into(),
                    enabled,
                    created_at: String::new(),
                },
            )
            .await
            .expect("limit role");
        }
        for (id, enabled) in [("tok-on", true), ("tok-off", false)] {
            crate::db::insert_tenant_access_token(
                &pool,
                &TenantAccessToken {
                    id: id.into(),
                    tenant_id: "t1".into(),
                    name: id.into(),
                    token_hash: format!("hash-{id}"),
                    token_prefix: "ht_".into(),
                    token_last4: "abcd".into(),
                    enabled,
                    expires_at: None,
                    created_at: String::new(),
                    updated_at: String::new(),
                },
            )
            .await
            .expect("tenant token");
        }
        for (id, enabled) in [("role-on", true), ("role-off", false)] {
            crate::db::upsert_global_role_with_providers(
                &pool,
                &GlobalRole {
                    id: id.into(),
                    name: id.into(),
                    tenant_id: "t1".into(),
                    user_group_id: "*".into(),
                    user_id: "*".into(),
                    api_key_id: "*".into(),
                    model_key: "*".into(),
                    provider_ids: Vec::new(),
                    enabled,
                    created_at: String::new(),
                    updated_at: String::new(),
                },
                &[],
            )
            .await
            .expect("global role");
        }

        let cfg_only = build_config(&pool, kp().as_ref())
            .await
            .expect("build_config");
        let expected = ReplicationContent::load(&pool, kp().as_ref(), cfg_only.clone(), 7)
            .await
            .expect("separate load");

        let (cfg, fidelity) = build_config_with_fidelity(&pool, kp().as_ref())
            .await
            .expect("combined load");
        assert_eq!(cfg, cfg_only, "the served config view must not change");
        assert_eq!(
            ReplicationContent::from_loaded(cfg, fidelity, 7),
            expected,
            "the combined loader must produce the same config + fidelity rows"
        );
    }
}
