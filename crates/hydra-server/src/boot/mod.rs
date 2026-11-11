//! `hydra` boot orchestration (F6 split of `main.rs`).
//!
//! Boots a [`pingora_core::server::Server`] hosting one `http_proxy_service`
//! running [`hydra_server::proxy::HydraProxy`]. The listener topology comes from
//! CONFIGURATION ONLY: the plaintext listener (`HYDRA_LISTEN`) is always bound,
//! and setting `HYDRA_TLS_LISTEN` adds a downstream-TLS listener that selects a
//! per-tenant certificate by SNI (design §12 / W4b). Whether tenants HAVE
//! certificates never decides which listeners exist — deriving the protocol from
//! the data is the bug that took the plaintext entry port down
//! (`dev-docs/bug-2026-09-16-tenant-cert-flips-listener-to-tls.md`).
//!
//! ## Startup sequence
//!
//! 1. Initialise tracing (`tracing_subscriber`).
//! 2. On a dedicated **background runtime** (so Pingora can own its own):
//!    open the PostgreSQL/MySQL pool and run migrations, load
//!    [`hydra_server::store::ConfigStore`], build the auth checker / usage sink /
//!    breaker / limiter, and spawn the long-lived background tasks (breaker
//!    probe, limiter GC). The runtime is **kept alive** for the process
//!    lifetime — the tasks need it.
//! 3. Resolve certs (if any) into the shared `HydraCertStore` (§12.1).
//! 4. Boot Pingora with an `http_proxy_service` — TLS when certs are present,
//!    plain TCP otherwise — plus the admin `ServeHttp` service on its own port.
//!
//! ## Why not `#[tokio::main]`
//!
//! Pingora's [`pingora_core::server::Server::run_forever`] is **blocking** and
//! builds its own tokio runtime internally. Calling it from inside
//! `#[tokio::main]` (or any nested `block_on`) panics with *"Cannot start a
//! runtime from within a runtime"*. The background runtime here is a **sibling**,
//! not nested: we use it only for the async bootstrap + the long-lived bg tasks,
//! drop out of `block_on`, keep the runtime alive via a binding, and let
//! `run_forever` own the main thread and its own runtime. This is the canonical
//! Pingora binary layout (see the integration tests in `tests/admin_api.rs` which
//! use the same `std::thread::spawn(run_forever)` shape to avoid the nesting).

use std::sync::Arc;

use hydra_server::admin::{AdminService, AdminState};
use hydra_server::db;
use hydra_server::proxy::limiter::spawn_gc_task;
use hydra_server::proxy::{AppState, HydraProxy};
use hydra_server::store::ConfigStore;
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use tracing::{error, info};

mod database;
mod env;
mod fanout;
mod listeners;
mod state;

/// Process entry point: tracing, the background runtime, the async bootstrap and
/// the blocking Pingora `run_forever`.
pub fn run() {
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
    // (0) Node role: `all` (default, single-node) or `cluster`. Cluster mode is
    //     exactly "HYDRA_REDIS_URL is configured"; there is no raft control plane.
    let role = hydra_server::cluster::NodeRole::from_env();

    // DD-1 (2026-10-09): `HYDRA_REDIS_MODE` is validated whenever it is SET —
    // not only on cluster nodes. Round 191 measured a cluster node booting as
    // `single` under `clustr`; the fix caught that inside `role.is_cluster()`,
    // but the boundary meant a single node with a stray/misspelt mode inherited
    // from some manifest was silently ignored (K12 pinned that gap explicitly).
    // The parse is cheap and pure; run it unconditionally so a bad value fails
    // fast on ANY role. The cluster branch below re-parses for the value itself.
    #[cfg(feature = "cluster-redis")]
    if let Ok(mode) = std::env::var("HYDRA_REDIS_MODE") {
        if !mode.trim().is_empty() {
            hydra_server::redis::RedisMode::parse(Some(&mode))?;
        }
    }

    info!(role = %role, "hydra gateway starting");

    // Cluster data plane: `HYDRA_REDIS_URL` is the only cluster switch. Every
    // node writes the shared relational database directly, so no control plane
    // or leader is required. The config is read from `cluster` later for the
    // node id (breaker votes / invalidation watermark) and the admin status.
    let cluster = hydra_server::cluster::ClusterConfig::from_env(role);
    #[cfg_attr(not(feature = "cluster-redis"), allow(unused_variables))]
    let redis_url = std::env::var("HYDRA_REDIS_URL")
        .ok()
        .filter(|u| !u.is_empty());
    // F3: opt-in Redis auth enforcement, checked here — before the shared pool
    // (`RedisBackend::connect`) and the config-bus subscriber are built below.
    #[cfg(feature = "cluster-redis")]
    env::enforce_redis_auth(
        redis_url.as_deref(),
        env::env_flag("HYDRA_REDIS_REQUIRE_AUTH", 0)?,
    )?;
    // Cluster admin-token requirement + strength floor (design §13.3).
    env::validate_admin_access(role)?;
    // REQUIRED, with no default (ADR-0002 D-1): "where does the billing data go" is a decision, and
    // every value this product could guess is wrong for someone. The list of accepted values comes
    // from the registry, so this message cannot rot into naming a backend that no longer exists.
    //
    // The old cluster-mode check ("cluster mode requires clickhouse") is GONE with the SQLite store
    // it existed for: `none` is a legitimate — if loudly-announced — choice everywhere, including a
    // cluster, and there is no longer a per-node store to forget to switch off.
    let sink_kind = env::usage_sink_kind()?;

    // (2b) Master key for provider-key encryption-at-rest (fail-closed: the
    //      process refuses to start without HYDRA_ENCRYPTION_KEY[_FILE]).
    let key_provider = state::load_key_provider()?;

    // (2a) DB pool + migrations. EVERY node has one (ADR-0001 D-2: a node that cannot materialize
    // the config tree into its own database cannot serve it) — so this is not an `Option`, and until
    // 2026-10-05 it was: `let pool = if false { None } else { … }` kept a pool-less arm compiling
    // for a role that no longer exists, and every reader downstream had to prove for themselves
    // that the arm could not be taken.
    let pool = database::init_pool().await?;
    // (2b) one-shot re-seal + (2c) legacy limit-key sealing. The re-seal runs
    // BEFORE the config store is loaded (loading decrypts provider keys); a
    // requested/failed rotation exits the process here.
    database::run_startup_maintenance(&pool, key_provider.as_ref()).await;

    // (2a-audit) Daily `admin_audit` retention sweep. It issues plain DELETEs
    // on a table outside `hydra_lock`/`config_revision`, so it never blocks a
    // config write and never triggers a reload/fanout.
    db::spawn_retention_task(pool.clone());

    // (2c-redis) Cluster Redis backbone: build the pool BEFORE the config store
    // is loaded. The pool does not depend on the store, and Phase 3b needs the
    // notifier (which borrows this pool) injected before the watcher starts.
    #[cfg(feature = "cluster-redis")]
    let redis_backend: Option<hydra_server::redis::RedisBackend> =
        fanout::connect_redis(role, redis_url.as_deref()).await?;
    #[cfg(not(feature = "cluster-redis"))]
    #[allow(unused_variables)]
    let redis_backend: Option<()> = None;

    // (2c) Config store (initial snapshot). ONE path: every node loads its config from its own
    // database (ADR-0001 D-2). The `match &pool` that used to be here had an edge arm that built a
    // snapshot-fed store with no database at all — the role is retired, and with it the branch.
    let store = state::load_config_store(pool.clone(), key_provider.clone()).await?;

    // (2c-bus) Config fanout (Phase 3b): notifier + watcher, started only AFTER
    // the initial load + cert-backfill reload, so a message that arrived during
    // boot cannot roll the snapshot backwards. Single-node builds keep `store`
    // exactly as loaded.
    #[cfg(feature = "cluster-redis")]
    let store = fanout::wire_config_bus(
        store,
        redis_backend.as_ref(),
        redis_url.as_deref(),
        &cluster.node_id,
    )
    .await?;

    // (2c'') Resolved-cert store (design §12.1) + its single wiring: follow the
    //        snapshot. Created here because the snapshot is the only thing this
    //        consumer needs to observe, and a config reload can carry new tenant
    //        certs before Pingora starts.
    #[cfg(any(feature = "tls-boringssl", feature = "tls-openssl"))]
    let cert_store: Option<Arc<hydra_server::tls::HydraCertStore>> =
        Some(state::build_cert_store(&store));
    #[cfg(not(any(feature = "tls-boringssl", feature = "tls-openssl")))]
    let cert_store: Option<()> = None;

    // (2d) The usage backend — ONE call for both halves (ADR-0002).
    //
    // The registry owns "which backends exist", what each one requires and whether it can read back
    // what it writes; `open` refuses an unknown kind, a kind this binary cannot serve, and a missing
    // variable, each with a message naming what to do. The reader used to be selected by a SECOND
    // `match` over the same kind, kept in step with the writer's by a comment.
    let usage_backend = state::open_usage_backend(&sink_kind, &pool)?;
    let sink = usage_backend.sink.clone();

    // (2e) Build shared app state. In cluster mode the breaker announces its
    //     local trips to the cluster (shared votes) and converges on the
    //     cluster-wide dead-set via the sync task (P4).
    let proxy_cfg = state::proxy_config_from_env()?;
    #[cfg(feature = "cluster-redis")]
    let breaker = state::build_breaker(&proxy_cfg, redis_backend.as_ref(), &cluster)?;
    #[cfg(not(feature = "cluster-redis"))]
    let breaker = state::build_breaker(&proxy_cfg)?;
    // Cluster mode uses the Redis-backed shared limiter (limits enforced
    // across the whole cluster); single-node keeps the in-memory one.
    #[cfg(feature = "cluster-redis")]
    let limiter = state::build_limiter(redis_backend.as_ref());
    #[cfg(not(feature = "cluster-redis"))]
    let limiter = state::build_limiter();
    let admission = hydra_server::proxy::admission::AdmissionControl::new();

    // The sink is moved into `AppState` further down, once the invalidation
    // stream exists (see the note there), so keep a handle for the flush hook.
    let sink_for_flush = sink.clone();

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
    //      long as the runtime, which is kept alive in `run`).
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
    hydra_server::proxy::breaker_wrap::spawn_probe_task(
        breaker.clone(),
        snapshot_provider,
        proxy_cfg.breaker.probe_interval,
    );
    spawn_gc_task(limiter.clone(), std::time::Duration::from_secs(30));
    let tenant_api_limiter = Arc::new(hydra_server::tenant_api::limit::TenantApiLimiter::new());
    // The tenant API's windows are keyed by source IP (caller-chosen) as well as
    // by tenant, so they MUST be swept: without this the failure map grows with
    // every address that ever failed, which is a memory-exhaustion vector handed
    // to the caller. `state` does not exist yet here, so the two maps are
    // constructed above and moved into it below.
    hydra_server::tenant_api::limit::spawn_gc_task(
        tenant_api_limiter.clone(),
        std::time::Duration::from_secs(60),
    );

    // (2f-redis) Invalidation consumer (P4): every node consumes the
    // invalidation stream so config invalidations propagate cluster-wide.
    #[cfg(feature = "cluster-redis")]
    let invalidation_stream =
        fanout::start_invalidation_consumer(redis_backend.as_ref(), &cluster.node_id);

    // (2e-ter) Tenant API config + shared proxy state.
    let tenant_api_cfg = state::tenant_api_config_from_env()?;
    // The reader came from the SAME `open` call as the writer: a backend either reads back what it
    // writes, or it declared why it cannot (`usage::ReaderContract`), and the registry refuses a
    // descriptor whose declaration and behaviour disagree. `None` here is the documented
    // `503 usage_store_unavailable` in the tenant handler, not an accident.
    let usage = usage_backend.query.clone();

    let state = state::assemble(state::StateParts {
        store: store.clone(),
        breaker: breaker.clone(),
        limiter: limiter.clone(),
        admission: admission.clone(),
        sink,
        proxy: proxy_cfg.clone(),
        tenant_api: tenant_api_cfg.clone(),
        tenant_api_limiter,
        #[cfg(feature = "cluster-redis")]
        invalidation: invalidation_stream.clone(),
        #[cfg(feature = "db")]
        usage,
    });
    #[cfg(not(feature = "cluster-redis"))]
    let invalidation_stream: Option<()> = None;

    Ok(BootstrapComponents {
        pool,
        store,
        breaker,
        key_provider,
        state,
        cert_store,
        #[cfg(feature = "cluster-redis")]
        invalidation_stream,
        #[cfg(not(feature = "cluster-redis"))]
        invalidation_stream,
        #[cfg(feature = "cluster-redis")]
        redis_backend,
        cluster,
    })
}

/// The shared components built by [`bootstrap`] and consumed by [`run_server`].
struct BootstrapComponents {
    pool: db::Pool,
    store: ConfigStore,
    breaker: Arc<hydra_server::proxy::breaker_wrap::CircuitBreaker>,
    key_provider: Arc<dyn hydra_server::crypto::KeyProvider>,
    state: Arc<AppState>,
    /// This node's role and identity (breaker votes, invalidation watermarks,
    /// and the admin cluster-status payload).
    cluster: hydra_server::cluster::ClusterConfig,
    /// Invalidation bus publisher (cluster P4): handed to the admin service so
    /// config invalidation broadcasts cluster-wide.
    #[cfg(feature = "cluster-redis")]
    invalidation_stream: Option<hydra_server::cluster::events::InvalidationStream>,
    #[cfg(not(feature = "cluster-redis"))]
    #[allow(dead_code)]
    invalidation_stream: Option<()>,
    /// The shared Redis backend, handed to the admin status endpoint so its
    /// `redis_reachable` probe reuses the SAME pool and command timeout as the
    /// data plane (no second client).
    #[cfg(feature = "cluster-redis")]
    redis_backend: Option<hydra_server::redis::RedisBackend>,
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
        grace_period_seconds: Some(env::shutdown_drain_secs()),
        graceful_shutdown_timeout_seconds: Some(5),
        ..Default::default()
    };
    // `-u` / `--upgrade` — the documented zero-downtime upgrade (`ops.md` §2).
    //
    // This flag is consumed by PINGORA's bootstrap, not by `ServerConf`:
    // `Bootstrap::new` reads `(opt.test, opt.upgrade)` and only then calls
    // `load_fds(upgrade)` to inherit the running process's listening sockets
    // (pingora-core 0.8.1 `server/bootstrap_services.rs:148`). Until 2026-09-29 the
    // binary passed `Opt::default()` and never looked at argv at all, so the
    // runbook's `hydra -u` was a NO-OP: the new process ignored the old one's
    // socket handover, tried to bind the same ports itself and died with
    // `cannot bind … Address already in use … refusing to start` (measured; the old
    // process logged `Trying to send socks` with nothing listening).
    //
    // Argv is sniffed rather than handed to pingora's clap `Opt::parse_args()`,
    // which would also accept `-c/-d/-t/--log` and exit on unknown arguments:
    // hydra documents exactly one flag, and unknown arguments stay ignored exactly
    // as before.
    let upgrade = env::upgrade_requested(&std::env::args().skip(1).collect::<Vec<_>>());
    if upgrade {
        info!("upgrade mode: inheriting the listening sockets of the running process");
    }
    // `new_with_opt_and_conf` returns a `Server` (not a `Result`), so there is
    // nothing to map here; the only thing it does not do that `Server::new` did
    // is derive the unused `version` field from `Opt`.
    let mut server = Server::new_with_opt_and_conf(
        Some(Opt {
            upgrade,
            ..Opt::default()
        }),
        conf,
    );
    server.bootstrap();

    // Clone the admission controller out of AppState BEFORE c.state is moved
    // into HydraProxy below, so AdminState::new can share the same DashMap.
    let admission = c.state.admission.clone();
    let app = HydraProxy::new(c.state)?;

    // (3b) Data-plane listeners: config-derived topology, bindability probe,
    // optional downstream TLS, and the external startup self-check.
    listeners::setup_data_plane(&mut server, app, &c.store, c.cert_store, upgrade)?;

    // (3c) Admin service — a second Pingora `Service` (ServeHttp) on its own
    //      plain-TCP port (design §13.1). Same runtime, admin-token-gated.
    //      Also serves the embedded `/admin/*` UI (design §14) without the
    //      token gate so the browser can render the login prompt.
    let admin_token = AdminService::token_from_env();
    let admin_addr =
        std::env::var("HYDRA_ADMIN_ADDR").unwrap_or_else(|_| env::DEFAULT_ADMIN_LISTEN.to_string());

    let admin_state = AdminState::new(
        c.pool,
        c.store,
        c.breaker,
        c.key_provider.clone(),
        admin_token.clone(),
        admission.clone(),
    )
    .with_cluster_identity(c.cluster.node_id.clone(), c.cluster.role);
    #[cfg_attr(not(feature = "cluster-redis"), allow(unused_mut))]
    let mut admin_state = admin_state;
    // Cluster status Redis probe: inject the SAME backend the data plane uses.
    #[cfg(feature = "cluster-redis")]
    {
        admin_state = admin_state.with_redis_backend(c.redis_backend.map(Arc::new));
    }
    // Invalidation bus publisher (P4): admin config invalidations are
    // broadcast cluster-wide, not just applied locally.
    #[cfg(feature = "cluster-redis")]
    {
        admin_state.invalidation = c.invalidation_stream;
    }
    let admin_state = Arc::new(admin_state);
    let admin_app = AdminService::new(admin_state);
    // Same reasoning as the plaintext probe above, and the failure shape is INVERTED
    // here: a bind failure inside Pingora's admin service task leaves the DATA plane
    // serving while the admin port silently never listens — every container
    // healthcheck (`/api/v1/health` on the admin port, in all three compose files)
    // then fails and the orchestrator restarts the container in a loop, with no
    // startup error to read anywhere.
    // Upgrade mode excluded for the same reason as the data listener above: the
    // predecessor holds this address and Pingora inherits the socket from it.
    // Measured 2026-09-29: without this exclusion the handover died here instead
    // (`HYDRA_ADMIN_ADDR=…: cannot bind … Address already in use`).
    if !upgrade {
        if let Err(e) = hydra_server::listeners::probe_bind(&admin_addr) {
            return Err(format!(
                "HYDRA_ADMIN_ADDR={admin_addr}: {e}; refusing to start — the admin API would \
                 never listen while the data plane reported healthy"
            )
            .into());
        }
    }
    let mut admin_service =
        pingora_core::services::listening::Service::new("Hydra admin API".to_string(), admin_app);
    admin_service.add_tcp(&admin_addr);
    server.add_service(admin_service);
    if admin_token.is_some() {
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
/// The only chance to persist buffered usage: see the call site.
///
/// SIGQUIT is included on purpose: it is the signal the documented deployments
/// actually send. `ops.md` §1.2 ships `KillSignal=SIGQUIT` in the systemd unit
/// (so a plain `systemctl restart` sends it) and §3's rolling upgrade starts
/// with `kill -SIGQUIT <pid>`. Pingora handles SIGQUIT itself (socket handover,
/// then drain) and ends in `process::exit(0)`, which runs no destructors — so
/// without an explicit hook here every routine restart silently discarded the
/// whole sink buffer.
fn spawn_sink_flush_on_shutdown(sink: Arc<dyn hydra_server::usage::UsageSink>) {
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
        // SIGQUIT: the documented systemd `KillSignal` / upgrade signal.
        let Ok(mut quit) = signal(SignalKind::quit()) else {
            tracing::warn!("cannot listen for SIGQUIT; buffered usage may be lost on shutdown");
            return;
        };
        tokio::select! {
            _ = term.recv() => info!("SIGTERM: flushing usage sinks"),
            _ = interrupt.recv() => info!("SIGINT: flushing usage sinks"),
            _ = quit.recv() => info!("SIGQUIT: flushing usage sinks"),
        }
        sink.shutdown().await;
        info!("usage sinks flushed");
    });
}
