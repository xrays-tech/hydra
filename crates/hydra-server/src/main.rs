//! `hydra` — Pingora-based LLM gateway binary (design §6.1 / §15.1).
//!
//! Boots a [`pingora_core::server::Server`] hosting one `http_proxy_service`
//! running [`HydraProxy`]. The listener topology comes from CONFIGURATION ONLY:
//! the plaintext listener (`HYDRA_LISTEN`) is always bound, and setting
//! `HYDRA_TLS_LISTEN` adds a downstream-TLS listener that selects a per-tenant
//! certificate by SNI (design §12 / W4b). Whether tenants HAVE certificates never
//! decides which listeners exist — deriving the protocol from the data is the
//! bug that took the plaintext entry port down (`dev-docs/bug-2026-09-16-tenant-
//! cert-flips-listener-to-tls.md`).
//!
//! ## Startup sequence
//!
//! 1. Initialise tracing (`tracing_subscriber`).
//! 2. On a dedicated **background runtime** (so Pingora can own its own):
//!    open the SQLite pool and run migrations, load [`ConfigStore`], build the
//!    auth checker / usage sink / breaker / limiter, and spawn the long-lived
//!    background tasks (breaker probe, limiter GC). The runtime is **kept
//!    alive** for the process lifetime — the tasks need it.
//! 3. Resolve certs (if any) into the shared `HydraCertStore` (§12.1).
//! 4. Boot Pingora with an `http_proxy_service` — TLS when certs are present,
//!    plain TCP otherwise — plus the admin `ServeHttp` service on its own port.
//!
//! ## Why not `#[tokio::main]`
//!
//! Pingora's [`Server::run_forever`] is **blocking** and builds its own tokio
//! runtime internally. Calling it from inside `#[tokio::main]` (or any nested
//! `block_on`) panics with *"Cannot start a runtime from within a runtime"*.
//! The background runtime here is a **sibling**, not nested: we use it only for
//! the async bootstrap + the long-lived bg tasks, drop out of `block_on`,
//! keep the runtime alive via a binding, and let `run_forever` own the main
//! thread and its own runtime. This is the canonical Pingora binary layout
//! (see the integration tests in `tests/admin_api.rs` which use the same
//! `std::thread::spawn(run_forever)` shape to avoid the nesting).

use std::sync::Arc;

use hydra_core::breaker::BreakerConfig;
use hydra_server::admin::{AdminService, AdminState};
use hydra_server::crypto;
use hydra_server::db;
use hydra_server::http::{AuthCache, AuthConfig, HttpAuthChecker};
use hydra_server::proxy::breaker_wrap::{spawn_probe_task, CircuitBreaker};
use hydra_server::proxy::config::ProxyConfig;
use hydra_server::proxy::limiter::{spawn_gc_task, RateLimiter};
use hydra_server::proxy::{AppState, HydraProxy};
use hydra_server::sink::build_sink;
use hydra_server::store::ConfigStore;
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use std::time::Duration;
use tracing::{error, info, warn};

const DEFAULT_DB_URL: &str = "sqlite:hydra.db?mode=rwc";
const DEFAULT_LISTEN: &str = "0.0.0.0:8080";
const DEFAULT_ADMIN_LISTEN: &str = "127.0.0.1:8081";
const DEFAULT_USAGE_SINK: &str = "sqlite";

/// `HYDRA_BREAKER_QUORUM`: minimum live votes for a provider to be
/// cluster-dead (default 1 = any live vote). A missing, unparseable or
/// non-positive value falls back to 1 rather than disabling the breaker
/// (0 would mean "never cluster-dead").
#[cfg(feature = "cluster-redis")]
fn breaker_quorum_from_env() -> usize {
    std::env::var("HYDRA_BREAKER_QUORUM")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|q| *q > 0)
        .unwrap_or(1)
}

/// Bounds for `HYDRA_LEADER_LEASE_MS`.
///
/// Below the minimum, leadership becomes meaningless: `valid_until = now + 0`
/// makes `is_leader()` permanently false while the election loop spins at
/// `(lease_ms/3).max(1)` = 1 ms, i.e. ~1000 `SET NX` per second per node with
/// no leader ever elected (and no startup error to say so). A value like `15`
/// (someone meaning seconds) gives a 5 ms tick and a lease that any 15 ms stall
/// loses — the active writer flaps. Above the maximum, `Expiration::PX` would
/// overflow into a negative TTL.
#[cfg(feature = "cluster-redis")]
const MIN_LEADER_LEASE_MS: u64 = 1_000;
#[cfg(feature = "cluster-redis")]
const MAX_LEADER_LEASE_MS: u64 = 600_000;
#[cfg(feature = "cluster-redis")]
const DEFAULT_LEADER_LEASE_MS: u64 = 15_000;

/// `HYDRA_LEADER_LEASE_MS` (cluster mode), validated: unset → the default,
/// out-of-range or unparseable → an error that fails startup. Deliberately NOT
/// a silent fallback: this is a safety-relevant value whose wrong setting is
/// invisible at runtime (see [`MIN_LEADER_LEASE_MS`]).
#[cfg(feature = "cluster-redis")]
fn leader_lease_ms_from_env() -> Result<u64, String> {
    let Ok(raw) = std::env::var("HYDRA_LEADER_LEASE_MS") else {
        return Ok(DEFAULT_LEADER_LEASE_MS);
    };
    let ms: u64 = raw
        .parse()
        .map_err(|e| format!("HYDRA_LEADER_LEASE_MS={raw:?} is not a millisecond count: {e}"))?;
    if !(MIN_LEADER_LEASE_MS..=MAX_LEADER_LEASE_MS).contains(&ms) {
        return Err(format!(
            "HYDRA_LEADER_LEASE_MS={ms} is outside {MIN_LEADER_LEASE_MS}..={MAX_LEADER_LEASE_MS} ms \
             (0 or a tiny value makes this node permanently ineligible while the election loop spins)"
        ));
    }
    Ok(ms)
}

/// `HYDRA_NON_ROUTE_STRATEGY` = `passthrough` | `reject` (case-insensitive).
///
/// What happens to a request that carries no `model` at all (a well-formed JSON
/// object with no such member — a request Hydra could NOT parse is rejected
/// outright, see `ModelField::Malformed`). `passthrough` forwards it to the
/// tenant's first live provider; `reject` answers `400 no_model_field`, which is
/// the only operator-facing backstop against a request reaching a provider
/// without the tenant model whitelist having been consulted.
///
/// Until now the documented `[proxy] non_route_strategy` was a ghost switch:
/// `main.rs` built `ProxyConfig::default()` and nothing ever read it, so
/// `Reject` was unreachable in the shipped binary (review M-8 / C3).
///
/// Unset → `passthrough` (the historical default). Anything else, including an
/// unknown word, FAILS STARTUP: an operator who typed `reject` and got a silent
/// fallback to `passthrough` would believe a safety control was on.
fn non_route_strategy_from_env() -> Result<hydra_server::proxy::config::NonRouteStrategy, String> {
    use hydra_server::proxy::config::NonRouteStrategy;
    match std::env::var("HYDRA_NON_ROUTE_STRATEGY") {
        Err(_) => Ok(NonRouteStrategy::Passthrough),
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "passthrough" => Ok(NonRouteStrategy::Passthrough),
            "reject" => Ok(NonRouteStrategy::Reject),
            other => Err(format!(
                "HYDRA_NON_ROUTE_STRATEGY={other:?} is not a known strategy (expected \"passthrough\" or \"reject\")"
            )),
        },
    }
}

/// `HYDRA_AUTH_ALLOW_TTL_MAX_SECS` (T7, design §4.2.5): the ceiling on an
/// ALLOW auth-cache TTL, including one the tenant auth service asked for via
/// `expires_in`. Default 300 s = the default `allow_ttl`, so an unconfigured
/// deployment is unchanged; the knob only *lowers* what a tenant can ask for.
///
/// Deliberately fails startup on a bad value, for the same reason as
/// `HYDRA_NON_ROUTE_STRATEGY` above: an operator who typed `60` and silently
/// got 300 would believe a safety bound was in force when it was not.
fn allow_ttl_max_from_env() -> Result<Duration, String> {
    let Ok(raw) = std::env::var("HYDRA_AUTH_ALLOW_TTL_MAX_SECS") else {
        return Ok(Duration::from_secs(
            hydra_server::http::DEFAULT_ALLOW_TTL_MAX_SECS,
        ));
    };
    match raw.trim().parse::<u64>() {
        Ok(secs) if secs > 0 => Ok(Duration::from_secs(secs)),
        _ => Err(format!(
            "HYDRA_AUTH_ALLOW_TTL_MAX_SECS={raw:?} is not a positive integer number of seconds"
        )),
    }
}

fn main() {
    // (1) Tracing.
    let _ = tracing_subscriber::fmt::Subscriber::builder()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    info!("hydra gateway starting (W6: UI + ops hardening)");

    // (2) Background runtime: drives the async bootstrap AND hosts the
    //     long-lived tasks (breaker probe, limiter GC). Kept alive for the
    //     process lifetime via the `_bg_runtime` binding below — see the
    //     module docs for why we don't use #[tokio::main].
    let bg_runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            error!(error = %e, "failed to build background tokio runtime");
            std::process::exit(1);
        }
    };

    let boot = bg_runtime.block_on(bootstrap());
    let components = match boot {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "fatal startup error");
            std::process::exit(1);
        }
    };

    // (3) Run Pingora on the bare main thread. `run_forever` builds its own
    //     runtime; the bg_runtime above is a sibling (kept alive, not nested).
    //     `_bg_runtime` is never dropped because `run_forever` diverges.
    let _bg_runtime = bg_runtime;
    if let Err(e) = run_server(components) {
        error!(error = %e, "fatal pingora startup error");
        std::process::exit(1);
    }
}

/// All async + shared-component construction done on the background runtime
/// (so the resulting `Arc`s are usable both by Pingora's services and by the
/// bg tasks that share them).
async fn bootstrap() -> Result<BootstrapComponents, Box<dyn std::error::Error>> {
    // (0) Node role (cluster P0b): all (default, single-node) | leader | edge.
    let role = hydra_server::cluster::NodeRole::from_env();
    info!(role = %role, "hydra gateway starting");

    // Cluster-mode fail-closed startup contract (v8 plan §2.1 / §7.3):
    // - `HYDRA_REDIS_URL` is the cluster backbone — required whenever a node
    //   participates in a cluster (leader/edge). Connectivity is verified
    //   here for the leader lease (P2).
    // - `HYDRA_CLUSTER_TOKEN` authenticates the control channel (leader
    //   serves it, edges/standbys call it).
    // - the usage sink must be ClickHouse — per-node SQLite usage records are
    //   meaningless across a cluster (each node would hold its own slice).
    // - the `cluster-redis` cargo feature must be enabled for leader mode (the
    //   Redis-backed lease).
    let cluster = hydra_server::cluster::ClusterConfig::from_env(role);
    let redis_url = std::env::var("HYDRA_REDIS_URL")
        .ok()
        .filter(|u| !u.is_empty());
    if role.is_cluster() {
        if redis_url.is_none() {
            return Err(
                "cluster mode (HYDRA_ROLE=leader|edge) requires HYDRA_REDIS_URL (Redis backbone); \
                 refusing to start"
                    .into(),
            );
        }
        if cluster.cluster_token.is_none() {
            return Err(
                "cluster mode requires HYDRA_CLUSTER_TOKEN (shared control-channel token); \
                 refusing to start"
                    .into(),
            );
        }
    }
    if role == hydra_server::cluster::NodeRole::Leader
        && std::env::var("HYDRA_ADMIN_TOKEN")
            .map(|t| t.is_empty())
            .unwrap_or(true)
    {
        return Err(
            "leader mode requires HYDRA_ADMIN_TOKEN (shared across the cluster — standby              nodes forward admin mutations to the active with it); refusing to start"
                .into(),
        );
    }
    // Admin-token strength (design §13.3). Refuse to BOOT on a short token
    // rather than warn: the gate has no rate limit or lockout, and the admin
    // API is the authority over every tenant, provider and stored upstream
    // api-key — so a guessable token is a full gateway takeover. Shipping a
    // weak default is exactly the failure mode this prevents.
    if let Ok(token) = std::env::var("HYDRA_ADMIN_TOKEN") {
        if !token.is_empty() && token.len() < AdminService::MIN_ADMIN_TOKEN_LEN {
            return Err(format!(
                "HYDRA_ADMIN_TOKEN is too short ({} chars, minimum {}); \
                 generate one with `openssl rand -hex 32`; refusing to start",
                token.len(),
                AdminService::MIN_ADMIN_TOKEN_LEN
            )
            .into());
        }
    }
    if role == hydra_server::cluster::NodeRole::Leader && !cfg!(feature = "cluster-redis") {
        return Err(
            "HYDRA_ROLE=leader requires the 'cluster-redis' cargo feature \
             (rebuild with --features cluster-redis); refusing to start"
                .into(),
        );
    }
    if role == hydra_server::cluster::NodeRole::Leader && cluster.control_url.is_none() {
        return Err(
            "leader mode requires HYDRA_CONTROL_URL (the active leader's control endpoint, \
             used by the standby sync); refusing to start"
                .into(),
        );
    }
    if role == hydra_server::cluster::NodeRole::Edge && cluster.control_url.is_none() {
        return Err(
            "edge mode requires HYDRA_CONTROL_URL (leader control endpoint); refusing to start"
                .into(),
        );
    }
    let sink_kind =
        std::env::var("HYDRA_USAGE_SINK").unwrap_or_else(|_| DEFAULT_USAGE_SINK.to_string());
    if role.is_cluster() && sink_kind != "clickhouse" {
        return Err(format!(
            "cluster mode requires HYDRA_USAGE_SINK=clickhouse (per-node sqlite usage is \
             meaningless in a cluster), got '{sink_kind}'"
        )
        .into());
    }

    // (2b) Master key for provider-key encryption-at-rest (fail-closed: the
    //      process refuses to start without HYDRA_ENCRYPTION_KEY[_FILE]).
    let static_kp =
        crypto::StaticKeyProvider::from_env().map_err(|e| -> Box<dyn std::error::Error> {
            format!("master key load failed: {e}").into()
        })?;
    info!(
        "provider-key encryption enabled (master key version {})",
        static_kp.version()
    );
    let key_provider: Arc<dyn crypto::KeyProvider> = Arc::new(static_kp);

    // (2a) DB pool + migrations — leader/all only. Edge nodes are stateless:
    // no local SQLite, the config snapshot arrives via the control plane.
    let pool = if role == hydra_server::cluster::NodeRole::Edge {
        None
    } else {
        let db_url = std::env::var("HYDRA_DB_URL").unwrap_or_else(|_| DEFAULT_DB_URL.to_string());
        let p = db::init_pool(&db_url).await?;
        db::run_migrate(&p).await?;
        info!(db_url = %db_url, "database pool ready");
        Some(p)
    };

    // (2c) Config store (initial snapshot).
    let store = match &pool {
        Some(p) => {
            let s = ConfigStore::load(p.clone(), key_provider.clone()).await?;
            // (2c') Migration-0007 transition: backfill legacy path-based
            // tenant certs into PEM content (best-effort; path fallback keeps
            // serving on failure), so the DB becomes self-contained and the
            // shared cert volume can be dropped in cluster deployments.
            db::backfill_legacy_certs(p, key_provider.as_ref()).await;
            info!("legacy cert backfill finished");
            s
        }
        None => {
            info!("edge mode: no local SQLite; config arrives via the control plane");
            ConfigStore::from_snapshot(
                hydra_core::config::ConfigData::default(),
                key_provider.clone(),
            )
        }
    };
    info!("config store loaded");

    // (2c'') Resolved-cert store (design §12.1) + its single wiring: follow the
    //        snapshot. It is created here, not in `run_server`, because the
    //        control client (spawned below) can apply a snapshot — carrying new
    //        tenant certs — before Pingora starts, and because the snapshot is
    //        the only thing this consumer needs to observe. The previous design
    //        re-resolved certs from two admin handler call sites only, so an
    //        edge node never saw a cert pushed through the control plane until
    //        it restarted (审核四 P3 / F-3).
    #[cfg(any(feature = "tls-boringssl", feature = "tls-openssl"))]
    let cert_store: Option<Arc<hydra_server::tls::HydraCertStore>> = {
        let cs = Arc::new(hydra_server::tls::HydraCertStore::new(None));
        // The wiring itself lives in `tls::follow_snapshot` so the integration
        // suite drives the production path instead of a copy of it.
        hydra_server::tls::follow_snapshot(&store, &cs);
        info!("tenant cert store wired to the config snapshot");
        Some(cs)
    };
    #[cfg(not(any(feature = "tls-boringssl", feature = "tls-openssl")))]
    let cert_store: Option<()> = None;

    // (2c-redis) Cluster Redis backbone (P4): every cluster node (leader AND
    // edge) shares one Redis for the lease, registry, invalidation bus,
    // shared limits / breaker and the auth-cache L2.
    #[cfg(feature = "cluster-redis")]
    let redis_backend: Option<hydra_server::redis::RedisBackend> = if role.is_cluster() {
        Some(
            hydra_server::redis::RedisBackend::connect(
                redis_url
                    .as_deref()
                    .ok_or("HYDRA_REDIS_URL must be set in cluster mode (checked above)")?,
                hydra_server::redis::RedisMode::from_env(),
            )
            .await?,
        )
    } else {
        None
    };
    #[cfg(not(feature = "cluster-redis"))]
    #[allow(unused_variables)]
    let redis_backend: Option<()> = None;

    // (2c-registry) Node registry (P4): every cluster node registers + sends
    // heartbeats; edges use it to follow the active leader across failover.
    #[cfg(feature = "cluster-redis")]
    let registry: Option<Arc<hydra_server::cluster::registry::NodeRegistry>> = match &redis_backend
    {
        Some(b) => {
            let public_url = std::env::var("HYDRA_PUBLIC_URL")
                .ok()
                .filter(|u| !u.is_empty());
            if role == hydra_server::cluster::NodeRole::Leader && public_url.is_none() {
                tracing::warn!(
                        "HYDRA_PUBLIC_URL unset: this leader registers without a pollable URL —                          edges cannot discover it through the registry"
                    );
            }
            let reg = Arc::new(hydra_server::cluster::registry::NodeRegistry::new(
                b.pool().clone(),
                cluster.node_id.clone(),
                role,
                public_url.clone().unwrap_or_default(),
            ));
            // Best-effort de-registration on shutdown, so a clean restart does
            // not leave a row behind for the reaper to clean up later.
            spawn_registry_unregister_on_shutdown((*reg).clone());
            // Register once and FAIL FAST: a node that cannot register must not
            // look healthy to the cluster (audit §3). The renewal loop below
            // must not re-register before its first interval elapses —
            // `tokio::time::interval` fires immediately, hence the leading
            // `tick.tick().await`.
            let grace = registry_stale_grace_secs();
            reg.register(30, grace).await?;
            let reg2 = reg.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(20));
                ticker.tick().await;
                loop {
                    ticker.tick().await;
                    // `register`, NOT a heartbeat-only refresh: renewal must
                    // rewrite the row too, or a node whose role/control_url
                    // changed after boot advertises the boot-time value forever.
                    if let Err(e) = reg2.register(30, grace).await {
                        tracing::warn!(error = %e, "node registry: renew failed");
                    }
                }
            });
            // The reaper: without it nothing ever deletes a registry row (the
            // 113-rows/108-offline symptom — there was no delete path at all).
            let reaper = reg.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
                loop {
                    ticker.tick().await;
                    match reaper.sweep_stale().await {
                        Ok(0) => {}
                        Ok(n) => {
                            hydra_server::admin::metrics::record_registry_reaped(n as u64);
                            info!(reaped = n, "node registry: reaped stale rows");
                        }
                        Err(e) => tracing::warn!(error = %e, "node registry: sweep failed"),
                    }
                    if let Ok(nodes) = reaper.list_nodes().await {
                        let alive = nodes.iter().filter(|n| n.alive).count();
                        hydra_server::admin::metrics::record_registry_nodes(
                            alive as i64,
                            (nodes.len() - alive) as i64,
                        );
                    }
                }
            });
            info!(node_id = %cluster.node_id, "node registered in the cluster registry");
            Some(reg)
        }
        None => None,
    };
    #[cfg(not(feature = "cluster-redis"))]
    #[allow(unused_variables)]
    let registry: Option<()> = None;

    // (2c) Auth checker (with the Redis L2 in cluster mode: L1 misses are
    // served verdicts the cluster already resolved).
    // T7: the ceiling is read HERE (the single construction site) and applied
    // to BOTH the cache and the config it is handed, so the value cannot reach
    // one and miss the other.
    let auth_config = AuthConfig {
        allow_ttl_max: allow_ttl_max_from_env().map_err(Box::<dyn std::error::Error>::from)?,
        ..AuthConfig::default()
    };
    let auth_cache_base = AuthCache::new(auth_config.allow_ttl, auth_config.deny_ttl)
        .with_allow_ttl_max(auth_config.allow_ttl_max);
    #[cfg(feature = "cluster-redis")]
    let auth_cache = match &redis_backend {
        Some(b) => auth_cache_base.with_l2(Arc::new(
            hydra_server::redis::auth_cache::RedisAuthL2::new(b.pool().clone()),
        )),
        None => auth_cache_base,
    };
    #[cfg(not(feature = "cluster-redis"))]
    let auth_cache = auth_cache_base;
    let auth = Arc::new(HttpAuthChecker::new(auth_cache, auth_config)?);
    info!("auth checker initialised");

    // (2d) Usage sink.
    let ch_url = std::env::var("HYDRA_CLICKHOUSE_URL").ok();
    let sink = build_sink(&sink_kind, pool.clone(), ch_url.as_deref())?;
    let sink: Arc<dyn hydra_server::sink::UsageSink> = Arc::from(sink);
    info!(kind = %sink_kind, "usage sink built");

    // (2e) Build shared app state. In cluster mode the breaker announces its
    // local trips to the cluster (shared votes) and converges on the
    // cluster-wide dead-set via the sync task (P4).
    // C3: the documented non-route strategy is now actually read (it was a
    // ghost switch, so `Reject` could not be configured at all).
    let proxy_cfg = ProxyConfig {
        non_route_strategy: non_route_strategy_from_env()
            .map_err(Box::<dyn std::error::Error>::from)?,
        // Read HERE (the single construction site) or the env var is a ghost:
        // the value would never reach the request path.
        upstream_first_byte_timeout_secs:
            hydra_server::proxy::config::parse_upstream_first_byte_timeout_secs(
                std::env::var("HYDRA_UPSTREAM_FIRST_BYTE_TIMEOUT_SECS")
                    .ok()
                    .as_deref(),
            ),
        ..ProxyConfig::default()
    };
    #[cfg_attr(not(feature = "cluster-redis"), allow(unused_mut))]
    let mut breaker = Arc::new(CircuitBreaker::new(BreakerConfig::new(
        proxy_cfg.breaker.threshold,
    )));
    #[cfg(feature = "cluster-redis")]
    {
        if let Some(b) = &redis_backend {
            // F-5: HYDRA_BREAKER_QUORUM is now actually read (it was a ghost
            // env — referenced in a comment, never parsed). One parse, shared
            // by the vote and sync handles.
            let quorum = breaker_quorum_from_env();
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
                    .ok_or(
                        "breaker Arc must be unique before wiring (no clones taken yet)",
                    )?
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
    }
    // Cluster mode uses the Redis-backed shared limiter (limits enforced
    // across the whole cluster); single-node keeps the in-memory one.
    #[cfg(feature = "cluster-redis")]
    let limiter: Arc<dyn hydra_server::proxy::limiter::Limiter> = match &redis_backend {
        Some(b) => Arc::new(hydra_server::redis::rate_limit::RedisRateLimiter::new(
            b.pool().clone(),
        )),
        None => Arc::new(RateLimiter::new()),
    };
    #[cfg(not(feature = "cluster-redis"))]
    let limiter: Arc<dyn hydra_server::proxy::limiter::Limiter> = Arc::new(RateLimiter::new());
    let admission = hydra_server::proxy::admission::AdmissionControl::new();

    // The sink is moved into `AppState` further down, once the invalidation
    // stream exists (see the note there), so keep a handle for the flush hook and
    // for the usage reader that both need it before then.
    let sink_for_flush = sink.clone();
    let sink_kind_for_api = sink_kind.clone();
    let pool_for_api = pool.clone();
    // Same value the SINK was built from: one parse of `HYDRA_CLICKHOUSE_URL`,
    // used by both the writer and the reader, so they cannot disagree about
    // which store they are talking to.
    let ch_url_for_api = ch_url.clone();

    // (2e-bis) Flush usage on SIGTERM/SIGINT.
    //
    // `run_forever` ends in std::process::exit(0), which runs NO destructors,
    // so the sinks' Drop never fired: every SIGTERM (a k8s rolling update,
    // `docker stop`) discarded the in-flight batch, everything queued in the
    // sink channel and up to flush_secs of traffic. This must be spawned HERE,
    // on the background runtime — `run_server` runs on the bare main thread,
    // where tokio::spawn has no reactor.
    //
    // The handler deliberately does NOT exit: pingora's own SIGTERM handler
    // performs the graceful connection drain, and jumping that queue would cut
    // it short. Both observe the same signal (tokio broadcasts to every
    // registered listener), so the flush runs alongside the drain.
    spawn_sink_flush_on_shutdown(sink_for_flush);

    // (2f) Background tasks (spawned onto this background runtime; they live as
    //      long as the runtime, which is kept alive in `main`).
    let snapshot_provider = {
        let store = store.clone();
        Arc::new(move || {
            let cfg = store.snapshot();
            cfg.providers
                .values()
                .map(|p| (p.id.clone(), p.endpoint.clone()))
                .collect::<Vec<_>>()
        })
    };
    spawn_probe_task(
        breaker.clone(),
        snapshot_provider,
        proxy_cfg.breaker.probe_interval,
    );
    spawn_gc_task(limiter.clone(), std::time::Duration::from_secs(30));
    // Auth-cache L1 sweep: `AuthCache::gc` had no caller, so expired verdicts
    // were never evicted (unbounded memory for rotating keys, and — before the
    // guard fix in `check` — a permanently available deadlock precondition).
    hydra_server::http::spawn_gc_task(auth.clone(), std::time::Duration::from_secs(60));
    let tenant_api_throttle = Arc::new(hydra_server::tenant_api::throttle::Throttle::new());
    let tenant_api_limiter = Arc::new(hydra_server::tenant_api::limit::TenantApiLimiter::new());
    // The tenant API's windows are keyed by source IP (caller-chosen) as well as
    // by tenant, so they MUST be swept: without this the failure map grows with
    // every address that ever failed, which is a memory-exhaustion vector handed
    // to the caller. `state` does not exist yet here, so the two maps are
    // constructed above and moved into it below.
    hydra_server::tenant_api::limit::spawn_gc_task(
        tenant_api_limiter.clone(),
        tenant_api_throttle.clone(),
        std::time::Duration::from_secs(60),
    );

    // (2f-redis) Invalidation consumer (P4): every node consumes the
    // invalidation stream so auth-cache invalidations propagate cluster-wide.
    #[cfg(feature = "cluster-redis")]
    let invalidation_stream = if let Some(b) = &redis_backend {
        let stream = hydra_server::cluster::events::InvalidationStream::new(b.pool().clone());
        hydra_server::cluster::events::spawn_invalidation_consumer(
            stream.clone(),
            auth.clone(),
            store.clone(),
            cluster.node_id.clone(),
        );
        // F-6: keep the invalidation stream bounded. A trim that removes
        // entries bumps the generation so lagging consumers re-hydrate
        // (idempotent full clear).
        hydra_server::cluster::events::spawn_trim_task(
            stream.clone(),
            // retain the most recent N invalidation events
            10_000,
            std::time::Duration::from_secs(30),
        );
        info!("invalidation consumer started");
        Some(stream)
    } else {
        None
    };

    // (2e-ter) Shared proxy state. Built HERE — after the invalidation stream —
    // so `AppState` can hold it directly instead of every consumer reaching for a
    // late-filled cell. The two fields below it are the reason this order exists.
    //
    // `usage` is chosen once, from the configured sink kind, by a LIBRARY function
    // rather than a branch in this file: the choice is what stops a cluster node
    // (which has a local SQLite file that the ClickHouse sink never writes to)
    // from answering a well-formed "zero usage".
    // The tenant API's live-node view.
    //
    // Injected as a CLOSURE over a periodically-refreshed snapshot, never as the
    // registry itself: the data plane must not gain the ability to talk to its
    // peers (design §6.4, decision A-1), and calling the async registry from the
    // request path would either block a worker or panic ("cannot start a runtime
    // from within a runtime") — so a background task owns the async read and the
    // closure is a cheap load.
    //
    // `alive == true` is the filter: a node whose heartbeat expired is not in the
    // load balancer's pool and must not hold a convergence decision hostage.
    #[allow(unused_mut)]
    let mut tenant_api_cfg = hydra_server::tenant_api::TenantApiConfig::from_env();
    // C: trust `X-Forwarded-For` only from configured trusted proxies, so the
    // failure limiter keys on the REAL client IP behind the LB rather than the
    // LB's egress address (design §5.1, C). A malformed value fails startup
    // rather than silently changing bucketing; an empty/unset value trusts nobody.
    tenant_api_cfg.trusted_proxies =
        hydra_server::tenant_api::TenantApiConfig::trusted_proxies_from_env()
            .map_err(Box::<dyn std::error::Error>::from)?;
    if !tenant_api_cfg.trusted_proxies.is_empty() {
        warn!(
            count = tenant_api_cfg.trusted_proxies.len(),
            "trusting X-Forwarded-For from these peers only; a trusted peer that \
             does not strip inbound X-Forwarded-For lets clients rotate limiter buckets"
        );
        // Catch-all range footgun: a /0 entry trusts X-Forwarded-For from ANY
        // peer, which makes the per-IP dimension forgeable and effectively
        // disables it. Warn loudly but do not fail startup (the operator may
        // have a reason, e.g. a single-node test rig behind NAT).
        let has_catch_all = tenant_api_cfg
            .trusted_proxies
            .iter()
            .any(|net| net.prefix_len() == 0);
        if has_catch_all {
            warn!(
                "HYDRA_TRUSTED_PROXIES contains a catch-all range (0.0.0.0/0 or ::/0); \
                 this trusts X-Forwarded-For from ANY peer, making the per-IP lockout \
                 dimension forgeable and effectively disabling it. Remove the /0 entry \
                 and list only the specific proxy IPs you control."
            );
        }
    }
    // ONE live-node view, shared by the tenant API's E2 and the operator's
    // `DELETE /api/v1/auth/cache`: two views could disagree about the fleet, and
    // the whole point of the barrier is that both callers get the same answer.
    #[cfg(feature = "cluster-redis")]
    let mut fleet_live: Option<Arc<dyn Fn() -> Vec<String> + Send + Sync>> = None;
    #[cfg(feature = "cluster-redis")]
    if let Some(reg) = &registry {
        let live: Arc<arc_swap::ArcSwap<Vec<String>>> =
            Arc::new(arc_swap::ArcSwap::from_pointee(Vec::new()));
        let refresh = {
            let reg = reg.clone();
            let live = live.clone();
            async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
                // One refresh immediately, then one per second. The previous form
                // skipped the first tick, so the view stayed empty for up to a
                // second after boot — and an empty view must never be read as
                // "converged". Refreshing up front shrinks that window to a single
                // `list_nodes` round trip.
                let refresh_once = || {
                    // Clone the `Arc`s so the closure is `Fn` (callable every tick)
                    // while each future owns its own handles.
                    let reg = reg.clone();
                    let live = live.clone();
                    async move {
                        match reg.list_nodes().await {
                            Ok(nodes) => {
                                let ids: Vec<String> = nodes
                                    .into_iter()
                                    .filter(|n| n.alive)
                                    .map(|n| n.node_id)
                                    .collect();
                                live.store(Arc::new(ids));
                            }
                            Err(e) => {
                                // Keep the PREVIOUS view rather than reporting an
                                // empty fleet: an empty set makes the barrier
                                // report `pending` (nobody checked), and a stale
                                // view of nodes we KNOW were live is strictly more
                                // informative than "we cannot enumerate the fleet".
                                // Either way the answer is never "converged".
                                tracing::warn!(error = %e, "live-node refresh failed; keeping the previous view");
                            }
                        }
                    }
                };
                refresh_once().await;
                // `interval`'s first tick resolves immediately; consume it so the
                // rhythm really is "once now, then once per second" rather than two
                // back-to-back `list_nodes` calls at boot.
                ticker.tick().await;
                loop {
                    ticker.tick().await;
                    refresh_once().await;
                }
            }
        };
        tokio::spawn(refresh);
        let view: Arc<dyn Fn() -> Vec<String> + Send + Sync> =
            Arc::new(move || live.load().to_vec());
        tenant_api_cfg.live_nodes = Some(view.clone());
        fleet_live = Some(view);
    }

    #[cfg(feature = "db")]
    // The read capability is chosen from the SINK KIND, in one library function
    // (`select`), not by probing for a pool: in a cluster the leader also has a
    // local SQLite file and it holds no usage rows at all, so "is there a pool?"
    // would answer a well-formed zero from an empty table.
    //
    // A failure here is NOT fatal: the endpoint reports 503 for a missing
    // capability, which is the honest answer, and refusing to boot the whole
    // data plane over an unreadable metering store would take the proxy down
    // with it.
    let usage: Option<Arc<dyn hydra_server::usage_query::UsageQuery>> =
        match hydra_server::usage_query::select(
            &sink_kind_for_api,
            pool_for_api.as_ref(),
            ch_url_for_api.as_deref(),
        ) {
            Ok(q) => Some(q),
            Err(e) => {
                warn!(kind = %sink_kind_for_api, error = ?e, "no usage reader for this sink kind; GET /usage will report 503");
                None
            }
        };

    // (2e-quint) Tenant config write forwarder (sub-tenant v2, A-2 / D1/D2).
    //
    // The data plane reaches the leader's internal control plane through a
    // TRUST-SCOPED forwarder: it holds the shared cluster token (node identity)
    // and a single closure that resolves the active leader's control URL. The
    // `NodeRegistry` is deliberately NOT handed to the data plane (A-2
    // precond. 8): the closure is the only leader-resolution seam.
    //
    // The closure is backed by a periodically-refreshed snapshot (the same
    // pattern as `live_nodes` above): `forward_target_from_registry` is async,
    // so a background task owns the registry read and the closure is a cheap
    // sync load. A `None` view is a definite fail-closed signal, never a local
    // write.
    //
    // Injected only when this node participates in a cluster (has a registry
    // and a cluster token): on single-node / no-cluster nodes it is `None`, and
    // the tenant config write is then applied locally (D8).
    #[cfg(feature = "cluster-redis")]
    let tenant_config_forwarder: Option<
        Arc<hydra_server::tenant_config::TenantConfigForwarder>,
    > = match (&registry, &cluster.cluster_token) {
        (Some(reg), Some(token)) => {
            let leader_url_live: Arc<arc_swap::ArcSwap<Option<String>>> =
                Arc::new(arc_swap::ArcSwap::from_pointee(None));
            let refresh = {
                let reg = reg.clone();
                let live = leader_url_live.clone();
                async move {
                    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
                    let refresh_once = || {
                        let reg = reg.clone();
                        let live = live.clone();
                        async move {
                            match hydra_server::cluster::forward::forward_target_from_registry(&reg)
                                .await
                            {
                                Ok(url) => live.store(Arc::new(url)),
                                Err(e) => {
                                    // Keep the previous view on a transient
                                    // registry error: a spurious "no leader"
                                    // would fail a write the leader could
                                    // have accepted, and the write stays
                                    // fail-closed either way.
                                    tracing::warn!(
                                        error = %e,
                                        "tenant-config leader-URL refresh failed; keeping the previous view"
                                    );
                                }
                            }
                        }
                    };
                    refresh_once().await;
                    // `interval`'s first tick resolves immediately; consume
                    // it so the rhythm is "once now, then once per second".
                    ticker.tick().await;
                    loop {
                        ticker.tick().await;
                        refresh_once().await;
                    }
                }
            };
            tokio::spawn(refresh);
            let view: Arc<dyn Fn() -> Option<String> + Send + Sync> = Arc::new(move || {
                // `ArcSwap<Option<String>>::load()` derefs through the held
                // `Arc` to the inner `Option<String>`; clone the inner value
                // (NOT the `Arc`) so the closure yields a plain `Option<String>`.
                (*(*leader_url_live.load())).clone()
            });
            Some(Arc::new(
                hydra_server::tenant_config::TenantConfigForwarder::new(token.clone(), view),
            ))
        }
        _ => None,
    };
    #[cfg(not(feature = "cluster-redis"))]
    let tenant_config_forwarder: Option<()> = None;

    let state = Arc::new(AppState {
        store: store.clone(),
        auth: auth.clone(),
        breaker: breaker.clone(),
        limiter: limiter.clone(),
        admission: admission.clone(),
        sink,
        proxy: proxy_cfg.clone(),
        tenant_api: tenant_api_cfg.clone(),
        tenant_api_throttle,
        tenant_api_limiter,
        #[cfg(feature = "cluster-redis")]
        invalidation: invalidation_stream.clone(),
        #[cfg(not(feature = "cluster-redis"))]
        invalidation: None,
        #[cfg(feature = "db")]
        usage,
        #[cfg(not(feature = "db"))]
        usage: None,
        tenant_config_forwarder,
    });
    #[cfg(not(feature = "cluster-redis"))]
    let invalidation_stream: Option<()> = None;

    // (2f') Control-plane client (cluster P1): edge nodes poll the leader for
    // config snapshots. Last-known-good semantics — the data plane keeps
    // serving whatever snapshot it has when the control plane is unreachable.
    if role == hydra_server::cluster::NodeRole::Edge {
        let url = cluster
            .control_url
            .clone()
            .ok_or("HYDRA_CONTROL_URL must be set (checked above)")?;
        let token = cluster
            .cluster_token
            .clone()
            .ok_or("HYDRA_CLUSTER_TOKEN must be set (checked above)")?;
        let client = hydra_server::cluster::control_client::ControlClient::new(
            hydra_server::cluster::control_client::ControlClientConfig {
                url,
                token,
                poll_interval: cluster.poll_interval,
            },
            store.clone(),
            key_provider.clone(),
            // No per-poll hook needed for certs: `ConfigStore::apply_snapshot`
            // notifies its followers, and the cert store is one of them
            // (`tls::follow_snapshot`), so a cert arriving in a control-plane
            // snapshot reaches the SNI callback without a restart (审核四 P3).
            // This slot is for the leader-eligibility gate on leader nodes.
            None,
        );
        #[cfg(feature = "cluster-redis")]
        let client = match &registry {
            Some(r) => client.with_discovery(r.clone()),
            None => client,
        };
        client.spawn();
        info!(
            poll_ms = cluster.poll_interval.as_millis() as u64,
            "control client started"
        );
    }

    // (2f''') Leader election (cluster P2): leader-candidate nodes run the
    // lease machine against Redis; exactly one holds the lease (the active
    // writer). Standbys additionally run the control client + replica
    // materialization so they are ready to take over within one lease.
    // Gated on `cluster-redis` (the Redis backbone); leader mode without the
    // feature already failed the startup checks above.
    #[cfg(feature = "cluster-redis")]
    let leader_ready: Option<Arc<dyn Fn() -> bool + Send + Sync>> = if role
        == hydra_server::cluster::NodeRole::Leader
    {
        let backend = redis_backend.ok_or("cluster mode has a Redis backbone (checked above)")?;
        let lease_ms = leader_lease_ms_from_env()?;
        let lease_store: Arc<dyn hydra_server::cluster::lease::LeaseStore> = Arc::new(
            hydra_server::redis::RedisLeaseStore::new(backend.pool().clone()),
        );
        let election = Arc::new(hydra_server::cluster::lease::LeaderElection::new(
            lease_store,
            cluster.node_id.clone(),
            lease_ms,
        ));

        // Standby sync: poll the active leader, materialize the local replica
        // on every applied snapshot (out-of-order guarded, F-4), and drive
        // the election freshness gate from the materialization result.
        let url = cluster
            .control_url
            .clone()
            .ok_or("HYDRA_CONTROL_URL must be set (checked above)")?;
        let token = cluster
            .cluster_token
            .clone()
            .ok_or("HYDRA_CLUSTER_TOKEN must be set (checked above)")?;
        let on_poll = {
            let election = election.clone();
            let pool = pool
                .clone()
                .ok_or("leader mode has a SQLite pool (checked above)")?;
            let key_provider = key_provider.clone();
            // F-4: monotonic out-of-order guard — a stale snapshot (version
            // <= the last claimed) is never materialized, so the replica can
            // never regress below a version already in flight / committed.
            let guard = Arc::new(hydra_server::cluster::replica::MaterializationGuard::new());
            let gate = Arc::new(move |ok: bool| election.mark_sync_ok(ok))
                as Arc<dyn Fn(bool) + Send + Sync>;
            // The gate decision itself lives in `replica::gate_hook`, so tests
            // can drive the real wiring instead of a copy of it.
            Some(hydra_server::cluster::replica::gate_hook(
                guard,
                pool,
                store.clone(),
                key_provider,
                gate,
            ))
        };
        let client = hydra_server::cluster::control_client::ControlClient::new(
            hydra_server::cluster::control_client::ControlClientConfig {
                url,
                token,
                poll_interval: cluster.poll_interval,
            },
            store.clone(),
            key_provider.clone(),
            on_poll,
        );
        #[cfg(feature = "cluster-redis")]
        let client = match &registry {
            Some(r) => client.with_discovery(r.clone()),
            None => client,
        };
        client.spawn();
        hydra_server::cluster::lease::spawn_election_task(election.clone(), lease_ms);
        info!(
            node_id = %cluster.node_id,
            lease_ms,
            "leader election started (lease holder = active writer)"
        );

        let ready = election.clone();
        Some(Arc::new(move || ready.is_leader()) as Arc<dyn Fn() -> bool + Send + Sync>)
    } else {
        None
    };
    #[cfg(not(feature = "cluster-redis"))]
    let leader_ready: Option<Arc<dyn Fn() -> bool + Send + Sync>> = None;

    Ok(BootstrapComponents {
        role,
        cluster,
        pool,
        store,
        auth,
        breaker,
        key_provider,
        state,
        leader_ready,
        cert_store,
        #[cfg(feature = "cluster-redis")]
        invalidation_stream,
        #[cfg(not(feature = "cluster-redis"))]
        invalidation_stream,
        #[cfg(feature = "cluster-redis")]
        cluster_registry: registry,
        #[cfg(not(feature = "cluster-redis"))]
        cluster_registry: None,
        #[cfg(feature = "cluster-redis")]
        fleet_live,
        #[cfg(feature = "cluster-redis")]
        converge_timeout: tenant_api_cfg.converge_timeout,
    })
}

/// The shared components built by [`bootstrap`] and consumed by [`run_server`].
struct BootstrapComponents {
    role: hydra_server::cluster::NodeRole,
    cluster: hydra_server::cluster::ClusterConfig,
    pool: Option<sqlx::SqlitePool>,
    store: ConfigStore,
    auth: Arc<HttpAuthChecker>,
    breaker: Arc<CircuitBreaker>,
    key_provider: Arc<dyn crypto::KeyProvider>,
    state: Arc<AppState>,
    leader_ready: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// Invalidation bus publisher (cluster P4): handed to the admin service so
    /// `DELETE /api/v1/auth/cache` broadcasts cluster-wide.
    #[cfg(feature = "cluster-redis")]
    invalidation_stream: Option<hydra_server::cluster::events::InvalidationStream>,
    #[cfg(not(feature = "cluster-redis"))]
    #[allow(dead_code)]
    invalidation_stream: Option<()>,
    /// Fleet registry (cluster P4): handed to the admin service for the
    /// whole-cluster status endpoint (`/api/v1/cluster/status`).
    #[cfg(feature = "cluster-redis")]
    cluster_registry: Option<Arc<hydra_server::cluster::registry::NodeRegistry>>,
    #[cfg(not(feature = "cluster-redis"))]
    #[allow(dead_code)]
    cluster_registry: Option<()>,
    /// The live-node view the tenant API uses, carried to `run_server` so the
    /// admin service waits on the SAME fleet — a second view could disagree.
    #[cfg(feature = "cluster-redis")]
    fleet_live: Option<Arc<dyn Fn() -> Vec<String> + Send + Sync>>,
    /// The convergence budget, same value for both entry points.
    #[cfg(feature = "cluster-redis")]
    converge_timeout: std::time::Duration,
    /// Resolved multi-tenant cert store (design §12.1 single source). Built in
    /// `bootstrap` — not in `run_server` — because it registers itself as a
    /// follower of the config snapshot, and a snapshot can be applied (by the
    /// control client) before Pingora ever starts.
    #[cfg(any(feature = "tls-boringssl", feature = "tls-openssl"))]
    cert_store: Option<Arc<hydra_server::tls::HydraCertStore>>,
    #[cfg(not(any(feature = "tls-boringssl", feature = "tls-openssl")))]
    #[allow(dead_code)]
    cert_store: Option<()>,
}

/// Synchronous Pingora setup: build the proxy + admin services and call
/// [`Server::run_forever`]. Must run on a bare thread (no enclosing tokio
/// runtime) so Pingora can build its own.
fn run_server(c: BootstrapComponents) -> Result<(), Box<dyn std::error::Error>> {
    // (3a) Pingora server.
    //
    // The drain window is set EXPLICITLY. Pingora's default `grace_period_seconds`
    // is `None` ⇒ `EXIT_TIMEOUT` = 300s, which is far longer than a typical
    // Kubernetes `terminationGracePeriodSeconds` (30s): the pod would be
    // SIGKILLed mid-drain, losing the usage-sink flush AND the registry
    // `unregister()`. `graceful_shutdown_timeout_seconds` (Pingora's 5s default)
    // is only the bound on the FINAL runtime-shutdown step, so it is not the
    // knob that governs in-flight requests.
    let conf = pingora_core::server::configuration::ServerConf {
        grace_period_seconds: Some(shutdown_drain_secs()),
        graceful_shutdown_timeout_seconds: Some(5),
        ..Default::default()
    };
    // `new_with_opt_and_conf` returns a `Server` (not a `Result`), so there is
    // nothing to map here; the only thing it does not do that `Server::new` did
    // is derive the unused `version` field from `Opt`.
    let mut server = Server::new_with_opt_and_conf(Some(Opt::default()), conf);
    server.bootstrap();

    // Clone the admission controller out of AppState BEFORE c.state is moved
    // into HydraProxy below, so AdminState::new can share the same DashMap.
    let admission = c.state.admission.clone();
    let app = HydraProxy::new(c.state);

    let listen_addr = std::env::var("HYDRA_LISTEN").unwrap_or_else(|_| DEFAULT_LISTEN.to_string());
    let mut proxy_service = pingora_proxy::http_proxy_service(&server.configuration, app);

    // (3b) Downstream listener topology (design §12.1 / §15.1; 审核四 P2).
    //
    // The decision belongs to `listeners::plan`, a pure function of DEPLOYMENT
    // CONFIG: plaintext always, HTTPS iff `HYDRA_TLS_LISTEN` is set. The tenant
    // certs in the snapshot are *reported*, never consulted — writing one used
    // to flip the single listener to TLS, so the next restart took the plaintext
    // entry (80 → NodePort → pod :8080) down with RST while the process stayed
    // healthy and the probes stayed green:
    // dev-docs/bug-2026-09-16-tenant-cert-flips-listener-to-tls.md.
    let cert_count = c.store.snapshot().certs.len();
    let planned = hydra_server::listeners::plan(
        &listen_addr,
        hydra_server::listeners::tls_listen_from_env().as_deref(),
        hydra_server::listeners::tls_backend_available(),
        cert_count,
    )?;
    for note in &planned.notes {
        error!(kind = note.kind(), "{}", note.message());
        hydra_server::admin::metrics::record_listener_misconfig(note.kind());
    }
    let plan = planned.plan;

    // Bindability is established BEFORE Pingora: a bind failure inside Pingora's
    // service task is invisible from outside (its own log line is printed before
    // the bind, the retry loop only WARNs, and the final state is a process that
    // is alive with zero data-plane listeners while admin probes answer 200).
    // The plaintext entry is the availability path — if it cannot bind, refuse
    // to start instead of pretending to serve.
    if let Err(e) = hydra_server::listeners::probe_bind(&plan.plain) {
        return Err(format!(
            "{}={}: {e}; refusing to start — the data plane would have no listener while the \
             process still reported healthy",
            hydra_server::listeners::LISTEN_ENV,
            plan.plain
        )
        .into());
    }
    proxy_service.add_tcp(&plan.plain);

    // HTTPS is optional and can never take the plaintext entry with it
    // (`Listeners::build()` is all-or-nothing per service, which is why a TLS
    // port that cannot bind used to be fatal for the whole data plane).
    #[cfg(any(feature = "tls-boringssl", feature = "tls-openssl"))]
    let tls_bound: Option<String> = match plan.tls.clone() {
        Some(tls_addr) => match hydra_server::listeners::probe_bind(&tls_addr) {
            Ok(()) => {
                let cert_store = c
                    .cert_store
                    .clone()
                    .expect("the cert store is built whenever a TLS listener is configured");
                match cert_store.build_tls_settings() {
                    Ok(settings) => {
                        proxy_service.add_tls_with_settings(&tls_addr, None, settings);
                        Some(tls_addr)
                    }
                    Err(e) => {
                        error!(
                            error = %e,
                            addr = %tls_addr,
                            "could not build downstream TLS settings; serving plaintext only \
                             (the TLS listener is NOT available)"
                        );
                        hydra_server::admin::metrics::record_listener_misconfig(
                            "tls_settings_failed",
                        );
                        None
                    }
                }
            }
            Err(e) => {
                error!(
                    error = %e,
                    addr = %tls_addr,
                    "could not bind the configured TLS listener; serving plaintext only"
                );
                hydra_server::admin::metrics::record_listener_misconfig("tls_bind_failed");
                None
            }
        },
        None => None,
    };
    #[cfg(not(any(feature = "tls-boringssl", feature = "tls-openssl")))]
    let tls_bound: Option<String> = None;

    server.add_service(proxy_service);

    // Publish the effective listener set for `/api/v1/health` (the plan is
    // config-derived; `tls` reflects whether the HTTPS listener really came up).
    hydra_server::listeners::record_active(hydra_server::listeners::ActiveListeners {
        plain: plan.plain.clone(),
        tls: tls_bound.clone(),
        tls_configured: plan.tls.is_some(),
        tenant_certs: cert_count,
    });

    // Startup self-check: verify the entry port really accepts connections and
    // publish it as `hydra_listener_bound{protocol="plain"}`. It is the external
    // evidence the log line above cannot provide (see the function docs).
    spawn_listener_self_check(plan.plain.clone());
    hydra_server::admin::metrics::record_listener_bound("tls", tls_bound.is_some());

    // Startup banner (bug report §5.5): one line that shows the whole protocol
    // shape, so every restart makes it obvious what is being served.
    info!(
        plain = %plan.plain,
        tls = tls_bound.as_deref().unwrap_or("disabled"),
        tenant_certs = cert_count,
        tls_backend = hydra_server::listeners::tls_backend_available(),
        "downstream listeners (config-derived): plaintext always, TLS only when {}=<addr> is set",
        hydra_server::listeners::TLS_LISTEN_ENV
    );

    // (3c) Admin service — a second Pingora `Service` (ServeHttp) on its own
    //      plain-TCP port (design §13.1). Same runtime, admin-token-gated.
    //      Also serves the embedded `/admin/*` UI (design §14) without the
    //      token gate so the browser can render the login prompt.
    let admin_token = AdminService::token_from_env();
    let admin_addr =
        std::env::var("HYDRA_ADMIN_ADDR").unwrap_or_else(|_| DEFAULT_ADMIN_LISTEN.to_string());

    #[cfg_attr(not(feature = "cluster-redis"), allow(unused_mut))]
    let mut admin_state = AdminState::new(
        c.pool,
        c.store,
        c.auth,
        c.breaker,
        c.key_provider.clone(),
        admin_token.clone(),
        admission.clone(),
        // Edge data-plane nodes serve only probe endpoints (cluster P0b).
        c.role == hydra_server::cluster::NodeRole::Edge,
        // Internal control-plane endpoints (cluster P1).
        c.cluster.cluster_token.clone(),
        // Leader-lease gate (/healthz/leader + admin mutation forwarding, P2/P3).
        // The forward target is resolved LIVE from the cluster registry (the
        // actual lease holder) at forward time — never from HYDRA_CONTROL_URL,
        // which for a primary leader candidate points at this node itself.
        c.leader_ready,
    );
    // Invalidation bus publisher (P4): admin auth-cache invalidations are
    // broadcast cluster-wide, not just applied locally.
    #[cfg(feature = "cluster-redis")]
    {
        admin_state.invalidation = c.invalidation_stream;
        admin_state.cluster_registry = c.cluster_registry;
    }
    // The same live-node view and convergence budget the tenant API got, so
    // `DELETE /api/v1/auth/cache` can report the fleet honestly (and so the two
    // entry points cannot disagree about what "applied" means).
    #[cfg(feature = "cluster-redis")]
    let admin_state = match c.fleet_live {
        Some(view) => admin_state.with_fleet(view, c.converge_timeout),
        // No live-node view (not a cluster): the local clear is the whole
        // answer and the report says `single_node`.
        None => admin_state,
    };
    let admin_state = Arc::new(admin_state);
    let admin_app = AdminService::new(admin_state);
    let mut admin_service =
        pingora_core::services::listening::Service::new("Hydra admin API".to_string(), admin_app);
    admin_service.add_tcp(&admin_addr);
    server.add_service(admin_service);
    if c.role == hydra_server::cluster::NodeRole::Edge {
        info!(admin = %admin_addr, "edge admin bound: /metrics /healthz /readyz only (no admin API)");
    } else if admin_token.is_some() {
        info!(admin = %admin_addr, "admin REST API + UI bound (admin token configured)");
    } else {
        error!(
            admin = %admin_addr,
            "admin REST API + UI bound but HYDRA_ADMIN_TOKEN is unset — all admin requests will be denied (§13.3)"
        );
    }

    server.run_forever();
}

/// Verify — from outside our own logging — that the plaintext entry port
/// actually accepts connections, and publish the result as
/// `hydra_listener_bound{protocol="plain"}`.
///
/// ## Why this exists
///
/// Our "listener bound" log line is printed *before* Pingora binds, so it is
/// not evidence. When the bind really fails, Pingora retries once a second for
/// 30 s and then panics **inside its service task**: the process stays alive and
/// keeps answering `/healthz` `/readyz` on the admin port while the data plane
/// has no listener at all (measured 2026-09-16; `dev-docs/dev-plan.md`
/// 「监听拓扑与启动约定」). A connect attempt is the cheapest honest check.
///
/// The plaintext listener is the one probed: both listeners live in the same
/// Pingora service, whose `Listeners::build()` is all-or-nothing — if the TLS
/// address fails to bind, the plaintext accept loop never starts either. So a
/// successful connect to the entry port also proves the TLS listener was
/// configured successfully; `hydra_listener_bound{protocol="tls"}` is published
/// separately from the config-time decision.
fn spawn_listener_self_check(plain: String) {
    std::thread::spawn(move || {
        let Ok(addr) = plain.parse::<std::net::SocketAddr>() else {
            // `listeners::plan` already rejected unparseable addresses.
            return;
        };
        // 0.0.0.0 / :: are bind wildcards, not dialable destinations.
        let target = if addr.ip().is_unspecified() {
            match addr {
                std::net::SocketAddr::V4(_) => {
                    std::net::SocketAddr::from(([127, 0, 0, 1], addr.port()))
                }
                std::net::SocketAddr::V6(_) => {
                    std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, addr.port()))
                }
            }
        } else {
            addr
        };

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            match std::net::TcpStream::connect_timeout(&target, std::time::Duration::from_secs(2)) {
                Ok(_) => {
                    hydra_server::admin::metrics::record_listener_bound("plain", true);
                    info!(
                        address = %plain,
                        "startup self-check: the plaintext entry port accepts connections"
                    );
                    return;
                }
                Err(e) if std::time::Instant::now() < deadline => {
                    tracing::debug!(address = %plain, error = %e, "self-check connect failed; retrying");
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
                Err(e) => {
                    hydra_server::admin::metrics::record_listener_bound("plain", false);
                    error!(
                        address = %plain,
                        error = %e,
                        "startup self-check FAILED: nothing is listening on the plaintext entry port — \
                         the process looks healthy but serves no traffic (see hydra_listener_bound)"
                    );
                    return;
                }
            }
        }
    });
}

/// Flush the usage sink when the process is asked to terminate.
/// The only chance to persist buffered usage: see the call site.
fn spawn_sink_flush_on_shutdown(sink: Arc<dyn hydra_server::sink::UsageSink>) {
    use tokio::signal::unix::{signal, SignalKind};

    tokio::spawn(async move {
        let Ok(mut term) = signal(SignalKind::terminate()) else {
            tracing::warn!("cannot listen for SIGTERM; buffered usage may be lost on shutdown");
            return;
        };
        let Ok(mut interrupt) = signal(SignalKind::interrupt()) else {
            tracing::warn!("cannot listen for SIGINT; buffered usage may be lost on shutdown");
            return;
        };
        tokio::select! {
            _ = term.recv() => info!("SIGTERM: flushing usage sinks"),
            _ = interrupt.recv() => info!("SIGINT: flushing usage sinks"),
        }
        sink.shutdown().await;
        info!("usage sinks flushed");
    });
}

/// How long a node may go without re-registering before its row becomes
/// reapable (`HYDRA_REGISTRY_STALE_GRACE_SECS`, default 120).
///
/// This value is used ONLY here, at registration time: it is the TTL of the
/// witness key that `register` writes. The reaper itself does no time
/// arithmetic — Redis expiry is what makes a row reapable, so there is no
/// timestamp comparison and no clock-skew handling anywhere.
#[cfg(feature = "cluster-redis")]
fn registry_stale_grace_secs() -> u64 {
    std::env::var("HYDRA_REGISTRY_STALE_GRACE_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0) // 0 would make every row instantly reapable
        .unwrap_or(120)
}

/// Seconds Pingora may spend draining in-flight requests after SIGTERM
/// (`HYDRA_SHUTDOWN_DRAIN_SECS`, default 20).
///
/// This maps to Pingora's `grace_period_seconds`. The deployment's
/// `terminationGracePeriodSeconds` MUST exceed this value plus the final
/// runtime-shutdown step (5s) plus slack, or the process is SIGKILLed mid-drain
/// and both the usage-sink flush and the registry de-registration are lost. The
/// value is deployment-visible, so it is recorded in `dev-docs/ops.md`.
///
/// `0` is rejected: Pingora would read `Some(0)` as "no drain at all", silently
/// defeating the point of configuring it.
fn shutdown_drain_secs() -> u64 {
    parse_shutdown_drain_secs(std::env::var("HYDRA_SHUTDOWN_DRAIN_SECS").ok().as_deref())
}

/// Parse half of [`shutdown_drain_secs`], kept PURE so it can be tested without
/// touching the process environment (like the other config parsers here).
///
/// `0` is rejected: Pingora would read `Some(0)` as "no drain at all", silently
/// defeating the point of configuring it.
fn parse_shutdown_drain_secs(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(20)
}

/// Best-effort registry de-registration on shutdown. Mirrors
/// [`spawn_sink_flush_on_shutdown`]: pingora's SIGTERM path ends in
/// `process::exit(0)`, which runs no destructors, so this is the only chance to
/// remove our row (otherwise a clean restart leaves a row for the reaper).
#[cfg(feature = "cluster-redis")]
fn spawn_registry_unregister_on_shutdown(reg: hydra_server::cluster::registry::NodeRegistry) {
    use tokio::signal::unix::{signal, SignalKind};

    tokio::spawn(async move {
        let (Ok(mut term), Ok(mut interrupt)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) else {
            tracing::warn!("cannot listen for shutdown signals; the registry row stays behind");
            return;
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = interrupt.recv() => {}
        }
        if let Err(e) = reg.unregister().await {
            tracing::warn!(error = %e, "node registry: unregister on shutdown failed");
        }
    });
}

#[cfg(test)]
mod drain_tests {
    use super::parse_shutdown_drain_secs;

    /// T9.4 — the drain window is configurable, `0` is rejected, and the default
    /// is the documented 20s. Without this, deleting the `grace_period_seconds`
    /// field from the `ServerConf` would silently restore Pingora's 300s default
    /// with every gate still green.
    ///
    /// Deliberately NOT inside the `cluster-redis` test module: the parser has
    /// nothing to do with clustering, and that module does not compile under the
    /// `--features server` gate.
    #[test]
    fn shutdown_drain_parsing_is_total() {
        assert_eq!(parse_shutdown_drain_secs(None), 20, "documented default");
        assert_eq!(parse_shutdown_drain_secs(Some("45")), 45);
        assert_eq!(parse_shutdown_drain_secs(Some(" 45 ")), 45, "trimmed");
        assert_eq!(
            parse_shutdown_drain_secs(Some("0")),
            20,
            "0 ⇒ no drain ⇒ default"
        );
        assert_eq!(parse_shutdown_drain_secs(Some("nope")), 20);
        assert_eq!(parse_shutdown_drain_secs(Some("-1")), 20);
    }
}

#[cfg(all(test, feature = "cluster-redis"))]
mod tests {
    use super::*;

    /// F-5: `HYDRA_BREAKER_QUORUM` must actually be honored (before the fix
    /// it was a ghost env — mentioned in a comment and the docs, never read).
    /// The only test in this binary touches this key, so the process-global
    /// env is safe to mutate here.
    #[test]
    fn breaker_quorum_env_injection() {
        std::env::set_var("HYDRA_BREAKER_QUORUM", "3");
        assert_eq!(breaker_quorum_from_env(), 3, "injected quorum is honored");

        std::env::set_var("HYDRA_BREAKER_QUORUM", "bogus");
        assert_eq!(breaker_quorum_from_env(), 1, "unparseable → default 1");

        std::env::set_var("HYDRA_BREAKER_QUORUM", "0");
        assert_eq!(breaker_quorum_from_env(), 1, "non-positive → default 1");

        std::env::remove_var("HYDRA_BREAKER_QUORUM");
        assert_eq!(breaker_quorum_from_env(), 1, "unset → default 1");
    }

    /// B5a: `HYDRA_LEADER_LEASE_MS` must be validated, not silently defaulted.
    /// `0` used to be accepted — the node then never became leader (its fence
    /// expired instantly) while the election loop spun at ~1 kHz, with no error
    /// anywhere. The same test binary owns this env key.
    #[test]
    fn leader_lease_ms_env_is_validated() {
        std::env::remove_var("HYDRA_LEADER_LEASE_MS");
        assert_eq!(
            leader_lease_ms_from_env(),
            Ok(15_000),
            "unset → the documented default"
        );

        std::env::set_var("HYDRA_LEADER_LEASE_MS", "2000");
        assert_eq!(leader_lease_ms_from_env(), Ok(2000), "in range is honored");

        for bad in ["0", "15", "999", "600001", "99999999", "bogus", ""] {
            std::env::set_var("HYDRA_LEADER_LEASE_MS", bad);
            assert!(
                leader_lease_ms_from_env().is_err(),
                "HYDRA_LEADER_LEASE_MS={bad:?} must fail startup, not silently default"
            );
        }

        std::env::remove_var("HYDRA_LEADER_LEASE_MS");
    }
}

/// C3: the non-route strategy parser. NOT gated on `cluster-redis` — the proxy
/// config exists in every build, and this switch is the backstop that keeps a
/// model-less request away from a provider.
#[cfg(test)]
mod non_route_strategy_tests {
    use super::non_route_strategy_from_env;
    use hydra_server::proxy::config::NonRouteStrategy;

    #[test]
    fn non_route_strategy_env_is_validated() {
        std::env::remove_var("HYDRA_NON_ROUTE_STRATEGY");
        assert_eq!(
            non_route_strategy_from_env(),
            Ok(NonRouteStrategy::Passthrough),
            "unset keeps the historical default"
        );

        std::env::set_var("HYDRA_NON_ROUTE_STRATEGY", "reject");
        assert_eq!(non_route_strategy_from_env(), Ok(NonRouteStrategy::Reject));

        std::env::set_var("HYDRA_NON_ROUTE_STRATEGY", "REJECT");
        assert_eq!(
            non_route_strategy_from_env(),
            Ok(NonRouteStrategy::Reject),
            "case-insensitive"
        );

        std::env::set_var("HYDRA_NON_ROUTE_STRATEGY", " reject ");
        assert_eq!(
            non_route_strategy_from_env(),
            Ok(NonRouteStrategy::Reject),
            "surrounding whitespace is tolerated"
        );

        std::env::set_var("HYDRA_NON_ROUTE_STRATEGY", "passthrough");
        assert_eq!(
            non_route_strategy_from_env(),
            Ok(NonRouteStrategy::Passthrough)
        );

        for bad in ["rejct", "deny", "false", "1", ""] {
            std::env::set_var("HYDRA_NON_ROUTE_STRATEGY", bad);
            assert!(
                non_route_strategy_from_env().is_err(),
                "HYDRA_NON_ROUTE_STRATEGY={bad:?} must fail startup, not silently pass through"
            );
        }

        std::env::remove_var("HYDRA_NON_ROUTE_STRATEGY");
    }
}
