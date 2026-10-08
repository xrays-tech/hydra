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

// A `[[bin]]` target is its own compilation unit: the inner attribute in
// `lib.rs` does NOT apply here, so without this line the binary — the artifact
// the image ships and the only code that boots the process — was the one place
// `unsafe` would have compiled silently. `dev-docs/HANDOFF.md` claims
// "both crates: `#![forbid(unsafe_code)]`"; `scripts/check_source_purity.cjs`
// now asserts the attribute is present on every crate ROOT, binary included.
#![forbid(unsafe_code)]

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
use hydra_server::store::ConfigStore;
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use std::time::Duration;
use tracing::{error, info, warn};

const DEFAULT_DB_URL: &str = "sqlite:hydra.db?mode=rwc";
const DEFAULT_LISTEN: &str = "0.0.0.0:8080";
const DEFAULT_ADMIN_LISTEN: &str = "127.0.0.1:8081";

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
    //     ADR-0001: the role is now derived from `HYDRA_CLUSTER_PEERS` — the member list IS
    //     the decision — and a node with the list set is a raft member.
    let role = hydra_server::cluster::NodeRole::from_env();

    // (0b) Arachne control plane (ADR-0001). Started HERE, inside the async bootstrap, and
    //      that placement is a requirement rather than a preference: `Arachne::start` builds a
    //      `tokio::time::Interval` while assembling its actor and panics with
    //      "there is no reactor running" when called from a synchronous context (measured on
    //      both the single-node and the member path). `main` runs the bootstrap inside the
    //      background runtime, so this is the one place the call is legal.
    //
    //      In single-node mode (`HYDRA_CLUSTER_PEERS` unset) this is a no-op and the process
    //      keeps zero new dependencies on its path.
    #[cfg(feature = "arachne")]
    let arachne_control: Option<
        std::sync::Arc<hydra_server::cluster::arachne_node::ArachneControl>,
    > = {
        let sqlite_path = std::env::var("HYDRA_DB").unwrap_or_else(|_| "hydra.db".to_string());
        let data_dir_override = std::env::var("HYDRA_ARACHNE_DATA_DIR").ok();
        hydra_server::cluster::arachne_node::ArachneControl::start_from_env(
            &sqlite_path,
            data_dir_override.as_deref(),
        )
        .await
        .map_err(|e| -> Box<dyn std::error::Error> {
            format!("arachne control plane refused to start: {e}").into()
        })?
    };
    // Without the feature the control plane does not exist at all; the binding stays so the
    // `leader_ready` merge below is written once rather than twice (the same shape the
    // `invalidation_stream` / `fleet_live` fields use in `BootstrapComponents`).
    #[cfg(not(feature = "arachne"))]
    #[allow(unused_variables, clippy::no_effect_underscore_binding)]
    let _arachne_control: Option<()> = None;
    // Does the raft control plane carry the WRITER decision on this node? True only when a raft
    // node was actually started (`HYDRA_CLUSTER_PEERS` set and the feature compiled in), because
    // `ArachneControl::start_from_env` returns `None` on a standalone node.
    //
    // This is what the cluster-mode startup contract below is gated on — NOT the retired Redis
    // control path, which no longer exists (the lease election, the snapshot-polling client and the
    // standby materializer were all deleted, ADR-0001 T3/T4). The gate used to be named for that
    // path, and the refusal it guards used to name a retired variable with it.
    //
    // Measured 2026-10-05 by `integration/test_arachne_control_plane.py`: before this gate existed,
    // a raft member refused to boot at all (the product still demanded a variable that only the
    // retired world read), so the acceptance gates could not be executed against the new model.
    #[cfg(feature = "arachne")]
    let arachne_carries_leadership = arachne_control
        .as_ref()
        .is_some_and(|c| c.config_store().is_some());
    #[cfg(not(feature = "arachne"))]
    let arachne_carries_leadership = false;

    info!(role = %role, "hydra gateway starting");

    // Cluster-mode fail-closed startup contract:
    // - `HYDRA_REDIS_URL` is the data-plane backbone — required whenever a node
    //   participates in a cluster (ADR-0001 D-1: the control plane moved to
    //   Arachne, Redis stayed for the hot path).
    // - `HYDRA_CLUSTER_TOKEN` gates the internal endpoints. NOTE: no
    //   `/api/v1/internal/*` route exists any more (they went with the snapshot
    //   channel and the forwarded write, T3.5/T4.1), so the token is currently a
    //   BOOT requirement with no consumer — kept, not silently dropped, because
    //   removing it is a deployment change that needs its own decision.
    // - the usage sink must be ClickHouse — per-node SQLite usage records are
    //   meaningless across a cluster (each node would hold its own slice).
    // - the `cluster-redis` cargo feature must be enabled for the shared data
    //   plane (rate limits / breaker / auth L2 / invalidation bus).
    // Read for `node_id` under `cluster-redis` (breaker votes and the invalidation watermark); a
    // build without that feature never looks at it, which the attribute makes explicit rather than
    // leaving a warning that invites someone to delete a value the cluster build needs.
    #[cfg_attr(not(feature = "cluster-redis"), allow(unused_variables))]
    let cluster = hydra_server::cluster::ClusterConfig::from_env(role);
    let redis_url = std::env::var("HYDRA_REDIS_URL")
        .ok()
        .filter(|u| !u.is_empty());
    if role.is_cluster() && redis_url.is_none() {
        // Names the member list, NOT the retired `HYDRA_ROLE`: an operator who follows a
        // message that tells them to set a retired variable makes the deployment worse, and
        // that variable is exactly what this build reports as IGNORED at boot. Pinned by
        // `cluster::tests::no_operator_facing_message_names_a_retired_variable`.
        return Err(
            "cluster mode (HYDRA_CLUSTER_PEERS is set) requires HYDRA_REDIS_URL (the \
                 data-plane backbone); refusing to start"
                .into(),
        );
    }
    // NO `HYDRA_CLUSTER_TOKEN` requirement any more (2026-10-05, user ruling). It gated the
    // `/api/v1/internal/*` family, and BOTH of its members are retired — the config snapshot channel
    // (T4.1) and the internal tenant-write family (T3.5, D-6). Requiring a secret for endpoints that
    // do not exist made every deployment provision, rotate and leak-check a credential that guarded
    // nothing. The name is now in `RETIRED_CLUSTER_ENV`, so a deployment that still sets it is TOLD
    // rather than left believing it does something.
    if role.is_cluster()
        && std::env::var("HYDRA_ADMIN_TOKEN")
            .map(|t| t.is_empty())
            .unwrap_or(true)
    {
        // Still required, but no longer because a standby relays with it (that layer is retired,
        // T3.3): every node now serves its own admin API, so the token is what gates EACH node's
        // admin surface. A cluster node without one would expose an unauthenticated admin API.
        return Err(
            "cluster mode requires HYDRA_ADMIN_TOKEN (each node serves its own admin API); \
             refusing to start"
                .into(),
        );
    }
    // Cluster mode IS the raft member list: without the control plane there is no writer to agree
    // on, and the old answer (a Redis lease) has been deleted. Refused at startup rather than
    // served as "no leader", which would accept writes on every node with no commit point.
    if role.is_cluster() && !arachne_carries_leadership {
        return Err(format!(
            "HYDRA_CLUSTER_PEERS is set but this build has no control plane: rebuild with \
             --features arachne (the member list is the cluster, and raft is what decides the \
             writer). Refusing to start as a 'cluster' whose nodes each believe they are alone; \
             got role={role}."
        )
        .into());
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
    // The old mode validations lived here: `HYDRA_ROLE=leader` needing the Redis backbone and a
    // control URL, and `HYDRA_ROLE=edge` needing one too. All three are retired — there is no role
    // variable (the member list decides), no control channel to point at, and no stateless role.
    // What replaced them is the pair above: cluster mode requires the control plane, and it
    // requires an admin token, because every node now serves its own admin API.
    // REQUIRED, with no default (ADR-0002 D-1): "where does the billing data go" is a decision, and
    // every value this product could guess is wrong for someone. The list of accepted values comes
    // from the registry, so this message cannot rot into naming a backend that no longer exists.
    //
    // The old cluster-mode check ("cluster mode requires clickhouse") is GONE with the SQLite store
    // it existed for: `none` is a legitimate — if loudly-announced — choice everywhere, including a
    // cluster, and there is no longer a per-node store to forget to switch off.
    let sink_kind = match std::env::var("HYDRA_USAGE_SINK") {
        Ok(v) => v,
        Err(_) => {
            return Err(format!(
                "HYDRA_USAGE_SINK is not set; there is no default. Set it to one of: {} \
                 (`none` records nothing at all and says so at startup)",
                hydra_server::usage::known_kinds()
            )
            .into())
        }
    };

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

    // (2a) DB pool + migrations. EVERY node has one (ADR-0001 D-2: a node that cannot materialize
    // the config tree into its own database cannot serve it) — so this is not an `Option`, and until
    // 2026-10-05 it was: `let pool = if false { None } else { … }` kept a pool-less arm compiling
    // for a role that no longer exists, and every reader downstream had to prove for themselves
    // that the arm could not be taken.
    let pool = {
        let db_url = std::env::var("HYDRA_DB_URL").unwrap_or_else(|_| DEFAULT_DB_URL.to_string());
        let p = db::init_pool(&db_url).await?;
        db::run_migrate(&p).await?;
        info!(db_url = %db_url, "database pool ready");
        p
    };

    // (2b) One-shot master-key rotation (`HYDRA_RESEAL_SECRETS=1`).
    //
    // Runs BEFORE the config store is loaded, because loading decrypts provider
    // keys and certificate private keys: with only the new key configured those
    // reads fail and the process refuses to start — which is exactly the
    // no-recovery hole this closes. Start once with
    // `HYDRA_ENCRYPTION_KEY=<new>` + `HYDRA_ENCRYPTION_KEY_VERSION=<new>` +
    // `HYDRA_ENCRYPTION_KEY_PREVIOUS=<old>` (+ `..._PREVIOUS_VERSION`), and this
    // re-seals every row in one transaction per table, prints a report and exits
    // instead of serving. Drop the previous-key variables afterwards.
    //
    // A FAILED row is reported and left untouched, and the exit code is non-zero,
    // so an incomplete rotation cannot be mistaken for a finished one.
    if let ResealSwitch::Invalid(value) = reseal_switch() {
        // Fail LOUD: the alternative is a node that serves traffic while the operator believes
        // the rotation ran (measured: every unrecognised value used to do exactly that).
        error!(
            value = %value,
            "HYDRA_RESEAL_SECRETS is not a value this process understands: use 1/true/yes/on \
             to re-seal every stored secret and exit, or 0/false/no/off (or unset) to serve \
             normally; refusing to start rather than silently skipping the rotation"
        );
        std::process::exit(1);
    }
    if reseal_requested() {
        match db::reseal_secrets(&pool, key_provider.as_ref()).await {
            Ok(report) => {
                println!(
                    "reseal: provider_keys={} tenant_certs={} already_current={} failed={}",
                    report.provider_keys_resealed,
                    report.tenant_certs_resealed,
                    report.already_current,
                    report.failed.len()
                );
                for f in &report.failed {
                    println!("reseal FAILED: {f}");
                }
                std::process::exit(if report.is_complete() { 0 } else { 1 });
            }
            Err(e) => {
                error!(error = %e, "reseal failed; nothing was rewritten");
                std::process::exit(1);
            }
        }
    }

    // (2c) Config store (initial snapshot). ONE path: every node loads its config from its own
    // database (ADR-0001 D-2). The `match &pool` that used to be here had an edge arm that built a
    // snapshot-fed store with no database at all — the role is retired, and with it the branch.
    let store = ConfigStore::load(pool.clone(), key_provider.clone()).await?;
    // (2c') Migration-0007 transition: backfill legacy path-based
    // tenant certs into PEM content (best-effort; path fallback keeps
    // serving on failure), so the DB becomes self-contained and the
    // shared cert volume can be dropped in cluster deployments.
    db::backfill_legacy_certs(&pool, key_provider.as_ref()).await;
    info!("legacy cert backfill finished");
    info!("config store loaded");

    // (2c-arachne) The two halves of the Arachne control plane that touch the config store
    // (ADR-0001, plan T3.2). Both are no-ops when the control plane did not start (single-node
    // mode, i.e. no `HYDRA_CLUSTER_PEERS`), which is the same condition under which
    // `arachne_control` is `None`.
    //
    // 1. PUBLISHER: a management write commits its config to the cluster instead of only to this
    //    node's database. Attached here because this is the only place that knows whether a
    //    control plane exists.
    // 2. MATERIALIZER: every node follows `ctl/head`, so a write on ANY node reaches all of them.
    //    Spawned here, and this is load-bearing — publishing without it would move the head that
    //    nobody reads, and the cluster would silently diverge.
    #[cfg(feature = "arachne")]
    let store = match arachne_control.as_ref().and_then(|c| c.config_store()) {
        Some(ctl_store) => {
            use hydra_server::cluster::arachne_materializer::{Materializer, ReplicaTarget};
            use hydra_server::cluster::arachne_publish::ConfigPublisher;

            let publisher = Arc::new(ConfigPublisher::new(
                ctl_store.clone(),
                key_provider.clone(),
            ));
            let store = store.with_publisher(publisher);

            let target = Arc::new(ReplicaTarget::new(store.clone(), key_provider.clone()));
            let mut materializer = Materializer::new(ctl_store, target, key_provider.clone());
            tokio::spawn(async move {
                // One head read per tick in the steady state, so the interval is what bounds
                // propagation latency (the measured follower convergence is milliseconds;
                // ADR-0001 §10 F-5). That read is `get_stale`, i.e. NOT monotone, and what the gate
                // does with the value is an APPLY that replaces this node's database and snapshot —
                // a known hazard with a measured shape; read `ArachneConfigStore::current_hash`'s
                // comment before touching either side, because making the read linearizable was
                // tried and broke leader-election timing in the acceptance drill.
                let mut tick = tokio::time::interval(Duration::from_secs(1));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tick.tick().await;
                    // `hydra_replica_materialize_retries_total` was registered with this loop in
                    // mind and had NO caller until 2026-10-05 — while `ops.md` §13 told the operator
                    // to watch `{outcome="failed"}` and ADR-0001 risk R7 offered it as the mitigation
                    // for the N-fold materialization cost. A documented series nobody records is a
                    // panel that can never move.
                    let outcome = match materializer.converge().await {
                        Ok(hydra_server::cluster::arachne_materializer::Converged::Applied {
                            hash,
                        }) => {
                            info!(head = %hash, "config materialized from the control plane");
                            // A real apply: the tree moved and this node now serves it.
                            Some("succeeded")
                        }
                        // Nothing to do (the head already names what we serve) — NOT an
                        // attempt, so it does not count. The steady state is one of these
                        // per second and counting it would drown the signal.
                        Ok(hydra_server::cluster::arachne_materializer::Converged::NoChange) => {
                            None
                        }
                        // A previous failure is still inside its backoff window.
                        Ok(_) => Some("throttled"),
                        Err(e) => {
                            // Not fatal: the node keeps serving its last-known-good config, and
                            // an un-materialized node is not eligible to lead.
                            warn!(error = %e, "config materialization did not complete; will retry");
                            Some("failed")
                        }
                    };
                    if let Some(o) = outcome {
                        // `attempt` counts every pass that TRIED to materialize a new tree,
                        // including the ones that succeeded or failed, so a rate on it answers
                        // "how much work is this loop doing" separately from the outcome mix.
                        if o != "throttled" {
                            hydra_server::admin::metrics::record_replica_materialize_retry(
                                "attempt",
                            );
                        }
                        hydra_server::admin::metrics::record_replica_materialize_retry(o);
                    }
                }
            });
            info!("config materialization loop started (poll every 1s)");
            store
        }
        None => store,
    };

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
                hydra_server::redis::RedisMode::from_env()?,
            )
            .await?,
        )
    } else {
        None
    };
    #[cfg(not(feature = "cluster-redis"))]
    #[allow(unused_variables)]
    let redis_backend: Option<()> = None;

    // (2c-registry) The node registry USED TO BE HERE: every cluster node registered and sent
    // heartbeats into Redis, and edges followed the active leader through it. Retired (ADR-0001
    // T4.1) — membership is the static member list, leadership is raft's, and no node polls a peer.

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

    // (2d) The usage backend — ONE call for both halves (ADR-0002).
    //
    // The registry owns "which backends exist", what each one requires and whether it can read back
    // what it writes; `open` refuses an unknown kind, a kind this binary cannot serve, and a missing
    // variable, each with a message naming what to do. The reader used to be selected by a SECOND
    // `match` over the same kind, kept in step with the writer's by a comment.
    let usage_backend: hydra_server::usage::Backend = hydra_server::usage::open(
        &sink_kind,
        &hydra_server::usage::BackendConfig::new(pool.clone()),
    )?;
    let sink = usage_backend.sink.clone();
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

    // (2e) Build shared app state. In cluster mode the breaker announces its
    // local trips to the cluster (shared votes) and converges on the
    // cluster-wide dead-set via the sync task (P4).
    // C3: the documented non-route strategy is now actually read (it was a
    // ghost switch, so `Reject` could not be configured at all).
    // Both upstream deadline env vars are read here (the single construction site) or they
    // are ghosts, and the CONNECT bound is REFUSED unless it sits strictly below the
    // first-byte bound: the latter wraps the whole `send()` including the connect, so a
    // connect bound at or above it can never fire — a dead route would keep being reported as
    // a post-send first-byte timeout (and would keep NOT failing over) with nothing on screen
    // to explain it.
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
    let proxy_cfg = ProxyConfig {
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
        // F-6: the trim task is spawned LATER, once the live-node view exists
        // (see below) — without it a trim cannot tell "every consumer already
        // applied this" from "we just dropped an unread event".
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
    // The live-node view the tenant API's E2 and `DELETE /api/v1/auth/cache` share. It used to be
    // refreshed from the registry's heartbeat table; with the registry retired (ADR-0001 T4.1) the
    // honest membership is the CONFIGURED member list, which is what the convergence barrier can
    // verify against (a peer that is down cannot answer, and the barrier's timeout is what
    // expresses that).
    #[cfg(all(feature = "cluster-redis", feature = "arachne"))]
    let fleet_live: Option<Arc<dyn Fn() -> Vec<String> + Send + Sync>> = arachne_control
        .as_ref()
        .and_then(|c| c.peers())
        .map(|peers| {
            let names: Vec<String> = peers
                .order()
                .iter()
                .map(std::string::ToString::to_string)
                .collect();
            Arc::new(move || names.clone()) as Arc<dyn Fn() -> Vec<String> + Send + Sync>
        });
    #[cfg(all(feature = "cluster-redis", not(feature = "arachne")))]
    let fleet_live: Option<Arc<dyn Fn() -> Vec<String> + Send + Sync>> = None;

    // ...and the TENANT API needs the same view — which is what T4.1 dropped.
    //
    // Before the registry was deleted, ONE refresh task fed both: `tenant_api_cfg.live_nodes = view`
    // and `fleet_live = view`. The replacement (the configured member list) was wired into the ADMIN
    // state only, so `TenantApiConfig::live_nodes` stayed `None` and the tenant self-service
    // `DELETE /tenant/{id}/api/v1/auth/cache` could never confirm anything: `fan_out_and_confirm` got
    // an EMPTY live list, which reports `nodes_total: 0` and `pending` forever — with `lagging` empty
    // too, so the tenant could not even see which member was behind. Measured 2026-10-05 by
    // `integration/test_auth_cache_layers.py`, whose PREMISE leg requires the fleet report to say
    // `applied` before its L2 legs mean anything.
    //
    // The ADMIN route was unaffected, which is why nothing else noticed: two entry points to the same
    // barrier reported different things, and only the quieter one was broken.
    #[cfg(all(feature = "cluster-redis", feature = "arachne"))]
    if let Some(view) = fleet_live.clone() {
        tenant_api_cfg.live_nodes = Some(view);
    }

    // The reader came from the SAME `open` call as the writer: a backend either reads back what it
    // writes, or it declared why it cannot (`usage::ReaderContract`), and the registry refuses a
    // descriptor whose declaration and behaviour disagree. `None` here is the documented
    // `503 usage_store_unavailable` in the tenant handler, not an accident.
    let usage = usage_backend.query.clone();

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
    });
    #[cfg(not(feature = "cluster-redis"))]
    let invalidation_stream: Option<()> = None;

    // (2f') The edge polling client USED TO BE HERE (cluster P1): an `edge` node polled the
    // leader's control endpoint for config snapshots. Retired with the role itself (ADR-0001 D-2:
    // nodes are homogeneous, and each one materializes the config tree locally).

    // The Redis control path USED TO BE HERE: the lease election, the standby sync (the
    // snapshot-polling client plus its replica materializer) and the leader-lease gate they fed.
    // All of it is retired (ADR-0001 T4.1). Leadership is the raft write probe, and the per-node
    // materializer follows `ctl/head` — see the Arachne block below, which is now the ONLY source
    // of `leader_ready`.
    //
    // A cluster build therefore has no leader notion at all without the `arachne` feature, which is
    // refused at startup rather than served as "no leader": a cluster whose nodes never agree on a
    // writer is worse than one that does not start.

    // Arachne control plane (ADR-0001): when it is up, leadership comes from the WRITE PROBE
    // and not from the Redis lease. The probe answers "can THIS node commit", which is the
    // question `/healthz/leader` and the admin write path actually ask; the lease answered
    // "does this node hold a key with a TTL", which was only ever a proxy for it. The Redis
    // election above still runs when its feature is compiled in, because the data-plane
    // subsystems (rate limit, breaker, L2 cache, invalidation) are still backed by Redis —
    // retiring the lease itself is plan T4.1.
    // The ONLY source of `leader_ready` now. When the control plane did not start — single-node
    // mode, i.e. no `HYDRA_CLUSTER_PEERS` — there is deliberately no leader notion at all and
    // `/healthz/leader` answers 404 rather than fabricating one (the documented single-node shape).
    #[cfg(feature = "arachne")]
    let leader_ready: Option<Arc<dyn Fn() -> bool + Send + Sync>> = match &arachne_control {
        Some(control) => {
            let control = Arc::clone(control);
            info!(
                node_id = %control.node_id(),
                "leadership is decided by the Arachne write probe (raft)"
            );
            Some(Arc::new(move || control.is_leader()) as Arc<dyn Fn() -> bool + Send + Sync>)
        }
        None => None,
    };
    #[cfg(not(feature = "arachne"))]
    let leader_ready: Option<Arc<dyn Fn() -> bool + Send + Sync>> = None;

    Ok(BootstrapComponents {
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
        fleet_live,
        #[cfg(feature = "cluster-redis")]
        converge_timeout: tenant_api_cfg.converge_timeout,
    })
}

/// The shared components built by [`bootstrap`] and consumed by [`run_server`].
struct BootstrapComponents {
    // `cluster` used to be carried here for the admin service's cluster token. That token is deleted
    // (2026-10-05), and `ClusterConfig` is read inside `bootstrap` itself — so the field is gone
    // rather than kept as dead weight that a reader would have to check.
    pool: sqlx::SqlitePool,
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
    let upgrade = upgrade_requested(&std::env::args().skip(1).collect::<Vec<_>>());
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
    //
    // EXCEPT in upgrade mode (`-u`): there the address is SUPPOSED to be in use —
    // the predecessor still holds it, and Pingora's `listen(fds)` reuses the socket
    // transferred over the upgrade socket (`pingora-core listeners/l4.rs:326`
    // looks the address up in the inherited FD table). Probing anyway aborted every
    // handover: measured 2026-09-29, the new process died with `Address already in
    // use … refusing to start` immediately after Pingora's own `Bootstrap done`, so
    // the documented `hydra -u` could never take the port over.
    if !upgrade {
        if let Err(e) = hydra_server::listeners::probe_bind(&plan.plain) {
            return Err(format!(
                "{}={}: {e}; refusing to start — the data plane would have no listener while the \
                 process still reported healthy",
                hydra_server::listeners::LISTEN_ENV,
                plan.plain
            )
            .into());
        }
    } else {
        info!(
            listener = %plan.plain,
            "upgrade mode: not probing the plaintext listener — the address is held by the \
             process we are taking it over from"
        );
    }
    proxy_service.add_tcp(&plan.plain);

    // HTTPS is optional and can never take the plaintext entry with it
    // (`Listeners::build()` is all-or-nothing per service, which is why a TLS
    // port that cannot bind used to be fatal for the whole data plane).
    #[cfg(any(feature = "tls-boringssl", feature = "tls-openssl"))]
    let tls_bound: Option<String> = match plan.tls.clone() {
        Some(tls_addr) => match if upgrade {
            // Same reasoning as the plaintext entry above.
            Ok(())
        } else {
            hydra_server::listeners::probe_bind(&tls_addr)
        } {
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
    // READ THIS GAUGE AS CONFIGURATION, NOT LIVENESS.
    //
    // `protocol="plain"` is the liveness signal: the self-check above dials the port
    // and rewrites it to false when the service never came up. `protocol="tls"` is
    // published here, from `tls_bound` — i.e. from the CONFIG decision — and nothing
    // ever revises it. That is deliberate rather than a gap: both addresses are
    // added to the SAME Pingora service (`add_tcp` above, `add_tls_with_settings`
    // below), and `Listeners::build()` is all-or-nothing per service, so a bind
    // failure (including the microsecond race between `probe_bind` and Pingora's own
    // bind) takes BOTH listeners down and is reported by `plain` going false. A
    // separate TLS dial would therefore be redundant. The pair of alerts in
    // `ops.md` §9.1 must be read the same way: `bound{plain} == 0` means the data
    // plane is not listening; `certs > 0 and bound{tls} == 0` means TLS was never
    // configured. (Its text says exactly that.)
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
        // Leader gate (/healthz/leader + admin mutation forwarding, P2/P3).
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

/// Verify — from outside our own logging — that the plaintext entry port
/// actually accepts connections, and publish the result as
/// `hydra_listener_bound{protocol="plain"}`.
///
/// ## Why this exists
///
/// Our "listener bound" log line is printed *before* Pingora binds, so it is
/// not evidence. When the bind really fails, Pingora retries once a second for
/// 30 s and then panics **inside its service task**: the process stays alive and
/// keeps answering its health probes on the admin port while the data plane has
/// no listener at all (measured 2026-09-16; the probe paths then were the
/// token-free `/healthz` `/readyz` an `edge` served, both retired with the role —
/// what answers today is `/api/v1/health` with the admin token; `dev-docs/dev-plan.md`
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

/// `-u` / `--upgrade`: inherit the running process's listening sockets instead of
/// binding them (Pingora's zero-downtime upgrade, `ops.md` §2).
///
/// Pure so it can be tested without touching the process environment — and so the
/// documented flag has exactly one definition. Only the exact short/long flags are
/// accepted (`--upgrade=1` is not, deliberately: an operator who typos the flag gets
/// the loud "address already in use" refusal instead of a silent non-upgrade).
#[must_use]
fn upgrade_requested(args: &[String]) -> bool {
    args.iter().any(|a| a == "-u" || a == "--upgrade")
}

/// `HYDRA_RESEAL_SECRETS=1` (or `true`) — run the one-shot re-seal and exit.
///
/// Container-friendly on purpose: the alternative (a CLI subcommand) has to fight
/// the Pingora argument parser, and this is an operation you run with
/// `docker run --rm -e ... <same image>`.
fn reseal_requested() -> bool {
    matches!(reseal_switch(), ResealSwitch::On)
}

/// The three states of `HYDRA_RESEAL_SECRETS` (see [`reseal_requested`]).
#[derive(Clone, Debug, PartialEq, Eq)]
enum ResealSwitch {
    /// Serve traffic normally.
    Off,
    /// Re-seal every stored secret and exit.
    On,
    /// A value this process does not understand — refused at startup.
    Invalid(String),
}

/// Parse the one-shot maintenance switch **strictly**.
///
/// Measured 2026-09-30 (`integration/test_key_rotation_live.py`, case K8): the previous version
/// was `matches!(var, Ok("1") | Ok("true") | Ok("yes"))`, so EVERY other value — `YES`, `on`,
/// `TRUE`, `reseal`, `2` — silently fell through to "serve traffic normally". The documented
/// procedure is "run this once, read the report line and the exit code"; an operator whose
/// container then came up and served had no signal at all that the rotation never ran, and the
/// controller in `ops.md` §3 (`docker run --rm -e HYDRA_RESEAL_SECRETS=1 …`) would sit there
/// serving instead of exiting. `upgrade_requested` is deliberately strict about the same shape
/// of mistake ("an operator who typos the flag gets the loud refusal instead of a silent
/// non-upgrade"); this switch now matches that standard. Values are matched case-insensitively
/// so `TRUE`/`Yes` do what a human means.
///
/// `ON_VALUES`/`OFF_VALUES` are documented in `ops.md` §1.2.
fn reseal_switch() -> ResealSwitch {
    parse_reseal_switch(std::env::var("HYDRA_RESEAL_SECRETS").ok().as_deref())
}

/// Pure half of [`reseal_switch`] — free of the process environment so the whole vocabulary can
/// be tested directly (the same shape as the `parse_*` helpers in `proxy::config`).
fn parse_reseal_switch(raw: Option<&str>) -> ResealSwitch {
    const ON_VALUES: [&str; 4] = ["1", "true", "yes", "on"];
    const OFF_VALUES: [&str; 5] = ["", "0", "false", "no", "off"];
    let Some(raw) = raw else {
        // Unset is the ordinary case: serve traffic.
        return ResealSwitch::Off;
    };
    let value = raw.trim().to_ascii_lowercase();
    if ON_VALUES.contains(&value.as_str()) {
        ResealSwitch::On
    } else if OFF_VALUES.contains(&value.as_str()) {
        ResealSwitch::Off
    } else {
        ResealSwitch::Invalid(raw.to_string())
    }
}

/// How long a node may go without re-registering before its row becomes
/// reapable (`HYDRA_REGISTRY_STALE_GRACE_SECS`, default 120).
///
/// This value is used ONLY here, at registration time: it is the TTL of the
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

    /// The one-shot rotation switch is parsed STRICTLY and case-insensitively.
    ///
    /// Regression (measured 2026-09-30, `integration/test_key_rotation_live.py` K8): the previous
    /// `matches!(var, Ok("1") | Ok("true") | Ok("yes"))` turned every other value — `YES`, `on`,
    /// `TRUE`, `reseal`, `2` — into "serve traffic normally", so a typo in the documented one-shot
    /// command silently skipped the rotation while the node came up looking healthy.
    #[test]
    fn the_reseal_switch_is_strict_about_values_it_does_not_know() {
        use ResealSwitch::{Invalid, Off, On};
        // The ordinary cases.
        assert_eq!(parse_reseal_switch(None), Off);
        for v in ["", "0", "false", "no", "off", "  ", "FALSE", "Off"] {
            assert_eq!(
                parse_reseal_switch(Some(v)),
                Off,
                "{v:?} must mean 'serve normally'"
            );
        }
        for v in ["1", "true", "yes", "on", "TRUE", "Yes", " on ", "1 "] {
            assert_eq!(
                parse_reseal_switch(Some(v)),
                On,
                "{v:?} must mean 're-seal and exit'"
            );
        }
        // ...and everything else is REFUSED rather than silently served.
        for v in ["reseal", "2", "enabled", "y", "1x", "ture", "re-seal"] {
            match parse_reseal_switch(Some(v)) {
                Invalid(echoed) => assert_eq!(echoed, v, "the offending value must be echoed"),
                other => panic!("{v:?} must be Invalid, got {other:?}"),
            }
        }
    }

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

/// The documented `-u` flag has its own test module so it runs in the default
/// `--features server` job (the pre-existing module is gated on `cluster-redis`).
#[cfg(test)]
mod upgrade_flag_tests {
    use super::upgrade_requested;

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_string()).collect()
    }

    /// The regression this pins: `hydra -u` used to be a NO-OP (argv was never
    /// read), so the runbook's zero-downtime upgrade ended with the new process
    /// dying on `Address already in use` — measured 2026-09-29.
    #[test]
    fn the_upgrade_flag_is_recognised_exactly() {
        assert!(upgrade_requested(&argv(&["-u"])));
        assert!(upgrade_requested(&argv(&["--upgrade"])));
        assert!(upgrade_requested(&argv(&["-u", "--log", "info"])));
        assert!(!upgrade_requested(&argv(&[])));
        assert!(!upgrade_requested(&argv(&["--log", "info"])));
        assert!(
            !upgrade_requested(&argv(&["--upgrade=1"])),
            "no value form: the flag is boolean"
        );
        assert!(!upgrade_requested(&argv(&["-U"])), "case matters");
    }
}
