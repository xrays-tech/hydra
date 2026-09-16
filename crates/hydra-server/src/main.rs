//! `hydra` — Pingora-based LLM gateway binary (design §6.1 / §15.1).
//!
//! Boots a [`pingora_core::server::Server`] hosting one `http_proxy_service`
//! running [`HydraProxy`]. The listener is downstream TLS (per-tenant SNI cert
//! callback, design §12 / W4b) whenever any tenant has certs configured; a
//! plain `add_tcp` listener is used for the localhost/dev case (no certs).
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
use tracing::{error, info};

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
            reg.register(30).await?;
            let reg2 = reg.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(20));
                ticker.tick().await;
                loop {
                    ticker.tick().await;
                    let _ = reg2.refresh_heartbeat(30).await;
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
    let auth_cache_base = AuthCache::new(
        AuthConfig::default().allow_ttl,
        AuthConfig::default().deny_ttl,
    );
    #[cfg(feature = "cluster-redis")]
    let auth_cache = match &redis_backend {
        Some(b) => auth_cache_base.with_l2(Arc::new(
            hydra_server::redis::auth_cache::RedisAuthL2::new(b.pool().clone()),
        )),
        None => auth_cache_base,
    };
    #[cfg(not(feature = "cluster-redis"))]
    let auth_cache = auth_cache_base;
    let auth_config = AuthConfig::default();
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
    let proxy_cfg = ProxyConfig::default();
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

    let state = Arc::new(AppState {
        store: store.clone(),
        auth: auth.clone(),
        breaker: breaker.clone(),
        limiter: limiter.clone(),
        admission: admission.clone(),
        sink,
        proxy: proxy_cfg.clone(),
    });

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
    spawn_sink_flush_on_shutdown(state.sink.clone());

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

    // (2f-redis) Invalidation consumer (P4): every node consumes the
    // invalidation stream so auth-cache invalidations propagate cluster-wide.
    #[cfg(feature = "cluster-redis")]
    let invalidation_stream = if let Some(b) = &redis_backend {
        let stream = hydra_server::cluster::events::InvalidationStream::new(b.pool().clone());
        hydra_server::cluster::events::spawn_invalidation_consumer(
            stream.clone(),
            auth.clone(),
            store.clone(),
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
            None, // edge TLS cert re-resolution lands with the edge TLS wiring
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
        #[cfg(feature = "cluster-redis")]
        invalidation_stream,
        #[cfg(not(feature = "cluster-redis"))]
        invalidation_stream,
        #[cfg(feature = "cluster-redis")]
        cluster_registry: registry,
        #[cfg(not(feature = "cluster-redis"))]
        cluster_registry: None,
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
}

/// Synchronous Pingora setup: build the proxy + admin services and call
/// [`Server::run_forever`]. Must run on a bare thread (no enclosing tokio
/// runtime) so Pingora can build its own.
fn run_server(c: BootstrapComponents) -> Result<(), Box<dyn std::error::Error>> {
    // (3a) Pingora server.
    let mut server =
        Server::new(Some(Opt::default())).map_err(|e| format!("pingora server init: {e:?}"))?;
    server.bootstrap();

    // Clone the admission controller out of AppState BEFORE c.state is moved
    // into HydraProxy below, so AdminState::new can share the same DashMap.
    let admission = c.state.admission.clone();
    let app = HydraProxy::new(c.state);

    let listen_addr = std::env::var("HYDRA_LISTEN").unwrap_or_else(|_| DEFAULT_LISTEN.to_string());
    let mut proxy_service = pingora_proxy::http_proxy_service(&server.configuration, app);

    // (3b) Downstream TLS when any tenant has certs (design §12 / W4b); else
    //      plain TCP for the localhost/dev case. The cfg split keeps the binary
    //      buildable without a TLS backend (plain `proxy` feature). Under a TLS
    //      backend the resolved `HydraCertStore` is kept so the admin reload
    //      endpoint can re-resolve certs (W4b contract).
    #[cfg(any(feature = "tls-boringssl", feature = "tls-openssl"))]
    let (tls_enabled, cert_store) = {
        use hydra_server::tls::HydraCertStore;

        let snapshot = c.store.snapshot();
        if snapshot.certs.is_empty() {
            proxy_service.add_tcp(&listen_addr);
            (false, None::<HydraCertStore>)
        } else {
            // Resolve CertMeta → parsed certs into the shared ArcSwap (§12.1
            // single source). The box inside TlsSettings shares this same
            // ArcSwap, so hot-reload only needs another `resolve_and_store`
            // after `reload_all`.
            let cert_store = HydraCertStore::new(None);
            cert_store.resolve_and_store(&snapshot.certs);
            match cert_store.build_tls_settings() {
                Ok(settings) => {
                    proxy_service.add_tls_with_settings(&listen_addr, None, settings);
                    (true, Some(cert_store))
                }
                Err(e) => {
                    error!(error = %e, "failed to build TLS settings; falling back to plain TCP");
                    proxy_service.add_tcp(&listen_addr);
                    (false, None)
                }
            }
        }
    };
    #[cfg(not(any(feature = "tls-boringssl", feature = "tls-openssl")))]
    let tls_enabled = {
        proxy_service.add_tcp(&listen_addr);
        false
    };

    server.add_service(proxy_service);

    if tls_enabled {
        info!(listen = %listen_addr, "proxy TLS listener bound (per-tenant SNI cert callback)");
    } else {
        info!(listen = %listen_addr, "proxy plain-TCP listener bound (no tenant certs configured)");
    }

    // (3c) Admin service — a second Pingora `Service` (ServeHttp) on its own
    //      plain-TCP port (design §13.1). Same runtime, admin-token-gated.
    //      Also serves the embedded `/admin/*` UI (design §14) without the
    //      token gate so the browser can render the login prompt.
    let admin_token = AdminService::token_from_env();
    let admin_addr =
        std::env::var("HYDRA_ADMIN_ADDR").unwrap_or_else(|_| DEFAULT_ADMIN_LISTEN.to_string());

    // Cert-reload hook for the W4b contract: re-resolve certs from the latest
    // snapshot after every reload. Only meaningful under a TLS backend.
    #[cfg(any(feature = "tls-boringssl", feature = "tls-openssl"))]
    let cert_reloader: Option<Arc<dyn Fn() + Send + Sync>> = cert_store.as_ref().map(|cs| {
        let cs = cs.clone();
        let store = c.store.clone();
        Arc::new(move || {
            let snap = store.snapshot();
            cs.resolve_and_store(&snap.certs);
        }) as Arc<dyn Fn() + Send + Sync>
    });
    #[cfg(not(any(feature = "tls-boringssl", feature = "tls-openssl")))]
    let cert_reloader: Option<Arc<dyn Fn() + Send + Sync>> = None;

    #[cfg_attr(not(feature = "cluster-redis"), allow(unused_mut))]
    let mut admin_state = AdminState::new(
        c.pool,
        c.store,
        c.auth,
        c.breaker,
        c.key_provider.clone(),
        admin_token.clone(),
        cert_reloader,
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

/// Flush the usage sink when the process is asked to terminate.
///
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
