//! Shared runtime state assembly: key provider, config store load, cert store,
//! usage backend, proxy config, breaker, limiter, tenant-API config and the
//! final `AppState` (F6 split of `main.rs`).
//!
//! The builders here are called in a fixed order by `boot::bootstrap`; side
//! effects (usage-backend open, breaker wiring, warning logs) must not be
//! reordered, so each builder is invoked at the point the original inline code
//! sat.

use std::sync::Arc;

use hydra_core::breaker::BreakerConfig;
use hydra_server::proxy::breaker_wrap::CircuitBreaker;
use hydra_server::proxy::config::ProxyConfig;
use hydra_server::proxy::limiter::{Limiter, RateLimiter};
use hydra_server::proxy::AppState;
use hydra_server::store::ConfigStore;
use hydra_server::tenant_api::limit::TenantApiLimiter;
use hydra_server::tenant_api::TenantApiConfig;
use tracing::{info, warn};

use super::env::non_route_strategy_from_env;

/// (2b) Master key for provider-key encryption-at-rest (fail-closed: the
/// process refuses to start without `HYDRA_ENCRYPTION_KEY[_FILE]`).
pub(crate) fn load_key_provider(
) -> Result<Arc<dyn hydra_server::crypto::KeyProvider>, Box<dyn std::error::Error>> {
    let static_kp = hydra_server::crypto::StaticKeyProvider::from_env().map_err(
        |e| -> Box<dyn std::error::Error> { format!("master key load failed: {e}").into() },
    )?;
    info!(
        "provider-key encryption enabled (master key version {})",
        static_kp.version()
    );
    Ok(Arc::new(static_kp))
}

/// (2c) Config store (initial snapshot). ONE path: every node loads its config
/// from its own database (ADR-0001 D-2).
///
/// (2c') Migration-0007 transition: backfill legacy path-based tenant certs into
/// PEM content (best-effort; path fallback keeps serving on failure), so the DB
/// becomes self-contained and the shared cert volume can be dropped in cluster
/// deployments.
pub(crate) async fn load_config_store(
    pool: hydra_server::db::Pool,
    key_provider: Arc<dyn hydra_server::crypto::KeyProvider>,
) -> Result<ConfigStore, Box<dyn std::error::Error>> {
    let store = ConfigStore::load(pool.clone(), key_provider.clone()).await?;
    hydra_server::db::backfill_legacy_certs(&pool, key_provider.as_ref()).await;
    // The backfill is a WRITE and bumps `config_revision` only when it actually
    // rewrote a row; the store above was loaded before it. A non-forced reload
    // observes that advance and is a no-op otherwise — forcing here would bump
    // the shared revision on every process start for no config change, and in a
    // cluster that would wake every peer's cache for nothing.
    if let Err(e) = store.reload_local(false).await {
        warn!(error = %e, "legacy cert backfill reload failed; serving the pre-backfill snapshot");
    }
    info!("legacy cert backfill finished");
    info!("config store loaded");
    Ok(store)
}

/// (2c'') Resolved-cert store (design §12.1) + its single wiring: follow the
/// snapshot. Created here because the snapshot is the only thing this consumer
/// needs to observe, and a config reload can carry new tenant certs before
/// Pingora starts.
///
/// The wiring itself lives in `tls::follow_snapshot` so the integration suite
/// drives the production path instead of a copy of it.
#[cfg(any(feature = "tls-boringssl", feature = "tls-openssl"))]
pub(crate) fn build_cert_store(store: &ConfigStore) -> Arc<hydra_server::tls::HydraCertStore> {
    let cs = Arc::new(hydra_server::tls::HydraCertStore::new(None));
    hydra_server::tls::follow_snapshot(store, &cs);
    info!("tenant cert store wired to the config snapshot");
    cs
}

/// (2d) The usage backend — ONE call for both halves (ADR-0002).
///
/// The registry owns "which backends exist", what each one requires and whether
/// it can read back what it writes; `open` refuses an unknown kind, a kind this
/// binary cannot serve, and a missing variable, each with a message naming what
/// to do.
pub(crate) fn open_usage_backend(
    sink_kind: &str,
    pool: &hydra_server::db::Pool,
) -> Result<hydra_server::usage::Backend, Box<dyn std::error::Error>> {
    let usage_backend = hydra_server::usage::open(
        sink_kind,
        &hydra_server::usage::BackendConfig::new(pool.clone()),
    )?;
    // A backend that says it cannot read usage is a deliberate deployment choice, so it is reported
    // where an operator will see it — at startup, with the reason the tenant API will give — rather
    // than discovered later from a 503.
    match usage_backend.reads {
        hydra_server::usage::ReaderContract::Unavailable { why } => {
            warn!(kind = %sink_kind, "{}", why);
        }
        hydra_server::usage::ReaderContract::SameBackend => {
            info!(kind = %sink_kind, notes = usage_backend.describe(), "usage backend open");
        }
    }
    Ok(usage_backend)
}

/// (2e) Documented proxy policy read from the environment.
///
/// C3: the documented non-route strategy is now actually read (it was a ghost
/// switch, so `Reject` could not be configured at all). Both upstream deadline
/// env vars are read here (the single construction site) or they are ghosts, and
/// the CONNECT bound is REFUSED unless it sits strictly below the first-byte
/// bound.
pub(crate) fn proxy_config_from_env() -> Result<ProxyConfig, Box<dyn std::error::Error>> {
    let upstream_first_byte_timeout_secs =
        hydra_server::proxy::config::parse_upstream_first_byte_timeout_secs(
            std::env::var("HYDRA_UPSTREAM_FIRST_BYTE_TIMEOUT_SECS")
                .ok()
                .as_deref(),
        );
    let upstream_connect_timeout_secs =
        hydra_server::proxy::config::parse_upstream_connect_timeout_secs(
            std::env::var("HYDRA_UPSTREAM_CONNECT_TIMEOUT_SECS")
                .ok()
                .as_deref(),
        );
    hydra_server::proxy::config::check_upstream_connect_before_first_byte(
        upstream_connect_timeout_secs,
        upstream_first_byte_timeout_secs,
    )
    .map_err(Box::<dyn std::error::Error>::from)?;
    Ok(ProxyConfig {
        non_route_strategy: non_route_strategy_from_env()
            .map_err(Box::<dyn std::error::Error>::from)?,
        upstream_first_byte_timeout_secs,
        upstream_connect_timeout_secs,
        // The body's own bound. Required for the removal of the upstream
        // client's total timeout to be safe: without it a stalled stream would
        // hold a worker forever.
        upstream_stream_idle_timeout_secs:
            hydra_server::proxy::config::parse_upstream_stream_idle_timeout_secs(
                std::env::var("HYDRA_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS")
                    .ok()
                    .as_deref(),
            ),
        // The downstream body's own bound — the only thing standing between a
        // client that never finishes sending and a permanently occupied worker.
        request_body_timeout_secs: hydra_server::proxy::config::parse_request_body_timeout_secs(
            std::env::var("HYDRA_REQUEST_BODY_TIMEOUT_SECS")
                .ok()
                .as_deref(),
        ),
        // The hard body cap. `ops.md` §8 tells an operator with a small VPS to
        // LOWER this to cut peak memory (memory ≈ concurrency × average body); that
        // advice was unactionable while the value only came from the Default impl.
        max_request_body_hard: hydra_server::proxy::config::parse_max_request_body_hard(
            std::env::var("HYDRA_MAX_REQUEST_BODY_HARD").ok().as_deref(),
        ),
        // Session affinity (T7b): read here or it is a ghost env. Total parse
        // with an ON default — a malformed value must not stop the gateway.
        session_affinity: hydra_server::proxy::config::parse_session_affinity(
            std::env::var("HYDRA_SESSION_AFFINITY").ok().as_deref(),
        ),
        ..ProxyConfig::default()
    })
}

/// (2e) Build the shared circuit breaker. In cluster mode it announces its local
/// trips to the cluster (shared votes) and converges on the cluster-wide
/// dead-set via the sync task (P4).
#[cfg(feature = "cluster-redis")]
pub(crate) fn build_breaker(
    proxy_cfg: &ProxyConfig,
    redis_backend: Option<&hydra_server::redis::RedisBackend>,
    cluster: &hydra_server::cluster::ClusterConfig,
) -> Result<Arc<CircuitBreaker>, Box<dyn std::error::Error>> {
    let mut breaker = Arc::new(CircuitBreaker::new(BreakerConfig::new(
        proxy_cfg.breaker.threshold,
    )));
    if let Some(b) = redis_backend {
        // F-5: HYDRA_BREAKER_QUORUM is now actually read (it was a ghost
        // env — referenced in a comment, never parsed). One parse, shared
        // by the vote and sync handles.
        let quorum = super::env::breaker_quorum_from_env();
        // (i) Vote handle for the trip/revive hooks. `vote_dead` /
        //     `vote_alive` touch only the pool + node id, so its internal
        //     breaker is a throwaway — this instance must NOT be used for
        //     `sync()`.
        let vote_shared = Arc::new(hydra_server::redis::breaker::SharedBreaker::new(
            b.pool().clone(),
            cluster.node_id.clone(),
            Arc::new(CircuitBreaker::new(BreakerConfig::new(
                proxy_cfg.breaker.threshold,
            ))),
            quorum,
        ));
        {
            let shared_trip = vote_shared.clone();
            let shared_revive = vote_shared.clone();
            // The proxy trips happen on Pingora's OWN runtime; plain
            // `tokio::spawn` there did not execute the spawned vote task
            // (accepted live: vote keys never appeared). Spawn the vote
            // onto the BACKGROUND runtime instead — `Handle::spawn` works
            // from any thread/runtime, and the bg runtime demonstrably
            // runs the other Redis-backed tasks.
            let bg_handle = tokio::runtime::Handle::current();
            let bg_handle_trip = bg_handle.clone();
            let bg_handle_revive = bg_handle.clone();
            Arc::get_mut(&mut breaker)
                .ok_or("breaker Arc must be unique before wiring (no clones taken yet)")?
                .set_cluster_hooks(
                    Some(Arc::new(move |p: &str| {
                        let shared = shared_trip.clone();
                        let p = p.to_string();
                        let handle = bg_handle_trip.clone();
                        handle.spawn(async move {
                            if let Err(e) = shared.vote_dead(&p).await {
                                tracing::warn!(provider = %p, error = %e, "cluster breaker vote_dead failed");
                            }
                        });
                    }) as Arc<dyn Fn(&str) + Send + Sync>),
                    Some(Arc::new(move |p: &str| {
                        let shared = shared_revive.clone();
                        let p = p.to_string();
                        let handle = bg_handle_revive.clone();
                        handle.spawn(async move {
                            if let Err(e) = shared.vote_alive(&p).await {
                                tracing::warn!(provider = %p, error = %e, "cluster breaker vote_alive failed");
                            }
                        });
                    }) as Arc<dyn Fn(&str) + Send + Sync>),
                );
        }
        // (ii) Sync handle wraps THE SAME breaker the proxy routes with:
        // `sync()` applies the cluster dead-set to it, so the shared
        // votes actually reach routing. (Wrapping a separate throwaway
        // breaker made votes converge into a breaker routing never
        // consults — accepted live.)
        let sync_shared = Arc::new(hydra_server::redis::breaker::SharedBreaker::new(
            b.pool().clone(),
            cluster.node_id.clone(),
            breaker.clone(),
            quorum,
        ));
        hydra_server::redis::breaker::spawn_breaker_sync(
            sync_shared,
            std::time::Duration::from_secs(1),
        );
        info!("shared circuit breaker wired (votes + 1s sync)");
    }
    Ok(breaker)
}

/// Single-node breaker: no cluster hooks, no shared votes.
#[cfg(not(feature = "cluster-redis"))]
pub(crate) fn build_breaker(
    proxy_cfg: &ProxyConfig,
) -> Result<Arc<CircuitBreaker>, Box<dyn std::error::Error>> {
    Ok(Arc::new(CircuitBreaker::new(BreakerConfig::new(
        proxy_cfg.breaker.threshold,
    ))))
}

/// Cluster mode uses the Redis-backed shared limiter (limits enforced across the
/// whole cluster); single-node keeps the in-memory one.
#[cfg(feature = "cluster-redis")]
pub(crate) fn build_limiter(
    redis_backend: Option<&hydra_server::redis::RedisBackend>,
) -> Arc<dyn Limiter> {
    match redis_backend {
        Some(b) => Arc::new(hydra_server::redis::rate_limit::RedisRateLimiter::new(
            b.pool().clone(),
        )),
        None => Arc::new(RateLimiter::new()),
    }
}

/// Single-node limiter: the in-memory one.
#[cfg(not(feature = "cluster-redis"))]
pub(crate) fn build_limiter() -> Arc<dyn Limiter> {
    Arc::new(RateLimiter::new())
}

/// (2e-ter) Tenant-API runtime configuration, including the trusted-proxy list.
///
/// C: trust `X-Forwarded-For` only from configured trusted proxies, so the
/// failure limiter keys on the REAL client IP behind the LB rather than the
/// LB's egress address (design §5.1, C). A malformed value fails startup rather
/// than silently changing bucketing; an empty/unset value trusts nobody.
pub(crate) fn tenant_api_config_from_env() -> Result<TenantApiConfig, Box<dyn std::error::Error>> {
    let mut tenant_api_cfg = TenantApiConfig::from_env();
    tenant_api_cfg.trusted_proxies =
        TenantApiConfig::trusted_proxies_from_env().map_err(Box::<dyn std::error::Error>::from)?;
    if !tenant_api_cfg.trusted_proxies.is_empty() {
        warn!(
            count = tenant_api_cfg.trusted_proxies.len(),
            "trusting X-Forwarded-For from these peers only; a trusted peer that \
             does not strip inbound X-Forwarded-For lets clients rotate limiter buckets"
        );
        // Catch-all range footgun. NOT "forgeable" — that was this comment's claim
        // until 2026-09-29, when measuring `HYDRA_TRUSTED_PROXIES=0.0.0.0/0` showed the
        // opposite: every candidate counts as "trusted", so `resolve_client_ip` falls
        // back to the PEER, and every client behind that peer shares ONE failure bucket
        // (a fresh X-Forwarded-For plus a fresh bad token was still refused once the
        // budget tripped). The real footgun is bucket SHARING / lockout amplification,
        // which is what the message below says. Warn loudly but do not fail startup (the
        // operator may have a reason, e.g. a single-node test rig behind NAT).
        let has_catch_all = tenant_api_cfg
            .trusted_proxies
            .iter()
            .any(|net| net.prefix_len() == 0);
        if has_catch_all {
            warn!(
                "HYDRA_TRUSTED_PROXIES contains a catch-all range (0.0.0.0/0 or ::/0); \
                 every X-Forwarded-For candidate is then \"trusted\", so the client IP \
                 falls back to the PEER and ALL clients behind it share one failure \
                 bucket — a few bad tokens lock the tenant API out for every one of \
                 them for the lockout window. Remove the /0 entry and list only the \
                 proxy IPs you control."
            );
        }
    }
    Ok(tenant_api_cfg)
}

/// (2e-ter) Shared proxy state, assembled from the parts built above.
///
/// `usage` is chosen once, from the configured sink kind, by a LIBRARY function
/// rather than a branch in the boot code: the choice is what stops a cluster node
/// from answering a well-formed "zero usage".
pub(crate) struct StateParts {
    pub(crate) store: ConfigStore,
    pub(crate) breaker: Arc<CircuitBreaker>,
    pub(crate) limiter: Arc<dyn Limiter>,
    pub(crate) admission: hydra_server::proxy::admission::AdmissionControl,
    pub(crate) sink: Arc<dyn hydra_server::usage::UsageSink>,
    pub(crate) proxy: ProxyConfig,
    pub(crate) tenant_api: TenantApiConfig,
    pub(crate) tenant_api_limiter: Arc<TenantApiLimiter>,
    #[cfg(feature = "cluster-redis")]
    pub(crate) invalidation: Option<hydra_server::cluster::events::InvalidationStream>,
    #[cfg(feature = "db")]
    pub(crate) usage: Option<Arc<dyn hydra_server::usage::UsageQuery>>,
}

pub(crate) fn assemble(parts: StateParts) -> Arc<AppState> {
    Arc::new(AppState {
        store: parts.store,
        breaker: parts.breaker,
        limiter: parts.limiter,
        admission: parts.admission,
        sink: parts.sink,
        proxy: parts.proxy,
        tenant_api: parts.tenant_api,
        tenant_api_limiter: parts.tenant_api_limiter,
        #[cfg(feature = "cluster-redis")]
        invalidation: parts.invalidation,
        #[cfg(not(feature = "cluster-redis"))]
        invalidation: None,
        #[cfg(feature = "db")]
        usage: parts.usage,
        #[cfg(not(feature = "db"))]
        usage: None,
    })
}
