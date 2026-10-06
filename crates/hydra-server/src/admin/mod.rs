//! `AdminService` — a Pingora `ServeHttp` app exposing the management REST API
//! + self-hosted `/metrics` (design §13, §17) and the embedded `/admin/*` UI
//! (design §14). Runs as a second `Service` on its own plain-TCP port
//! (`[admin] addr`), sharing the same Tokio runtime as the proxy (design §13.1
//! — no axum, no second runtime).
//!
//! ## Architecture
//!
//! - **Lightweight router**: method + path-segment match, no framework. All
//!   `/api/v1/*` routes are admin-token-gated (design §13.3); `/metrics` is
//!   served from the prometheus default registry; `/admin/*` serves the
//!   embedded static UI **without** the token gate (the HTML/CSS/JS have no
//!   secrets — `app.js` collects the admin token and attaches
//!   `Authorization: Bearer` on every `/api/v1/*` fetch).
//! - **No internal mocking**: every handler drives the real `db::repo`,
//!   `ConfigStore`, `AuthChecker`, `CircuitBreaker` and `HydraCertStore`.
//! - **Write-after consistency**: every successful config write calls
//!   `ConfigStore::reload_all()` (design §13.2), serialised by a per-state
//!   mutex. Cert re-resolution is no longer the admin service's job: the cert
//!   store follows every config snapshot swap through the `ConfigStore`
//!   snapshot-change hook registered in `main` (design §12.1 / W4b).
//! - **Standby mutation forwarding (cluster P3)**: a leader candidate that
//!   does not hold the lease forwards every admin mutation to the ACTUAL
//!   lease holder, resolved live from the cluster registry — never to a
//!   static `HYDRA_CONTROL_URL` (which for a primary candidate points at the
//!   node itself). Forwarded mutations carry a forward-once marker so any
//!   (self- or mutual-) forward loop terminates fail-closed with 503 instead
//!   of a timeout recursion (see `cluster::forward`).

use std::sync::Arc;

use async_trait::async_trait;
use pingora_core::apps::http_app::ServeHttp;
use pingora_core::protocols::http::ServerSession;
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use tracing::warn;

use crate::crypto::KeyProvider;
use crate::http::HttpAuthChecker;
use crate::proxy::admission::AdmissionControl;
use crate::proxy::breaker_wrap::CircuitBreaker;
use crate::store::ConfigStore;

pub mod cluster_api;
pub mod handlers;
pub mod metrics;
mod static_files;
/// Shared transactional sub-tenant / route write core (sub-tenant v2, D5). The
/// single write path used by the v1 admin handlers now and the v2 leader
/// internal handler later; not HTTP-specific (typed results, not `Resp`).
pub mod sub_tenant_write;
/// Leader internal tenant-config write endpoint (sub-tenant v2, D3/D5/D6/D7):
/// the A-2 receiver-side gates (lease assertion, tenant re-auth, authorization
/// family. Cluster-token gated in `AdminService::response`.
// Re-export the metrics module publicly so the proxy / breaker / tls can reach
// the `record_*` call-sites and the `/metrics` renderer.
pub use metrics as metrics_export;

use handlers::Resp;

/// Shared state for the admin service (design §13.1: a subset of `AppState`).
/// Cheap to `Arc`-clone so tests can inspect it after requests.
/// Per-IP budget for FAILED admin-token attempts, per minute
/// (`HYDRA_ADMIN_AUTH_FAIL_LIMIT_PER_MIN`, default 10; `0`/garbage falls back).
///
/// The admin token is the single secret gating every tenant/provider/api-key
/// mutation, and the tenant-plane gate next door has had a per-IP failure budget
/// since 2026-09-17 — the admin gate had none, so a brute-force run was neither
/// slowed down, nor counted, nor logged above `debug!` (which the shipped
/// `RUST_LOG=info` filters out).
fn admin_auth_fail_limit_per_min_from_env() -> u32 {
    std::env::var("HYDRA_ADMIN_AUTH_FAIL_LIMIT_PER_MIN")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(10)
}

/// The peer address of an admin request, with the PORT DROPPED: every request
/// arrives on a new ephemeral port, so keying a failure budget on the full socket
/// address would give each connection its own bucket and the limit would never
/// trip (the same trap the tenant-plane limiter documents).
///
/// `X-Forwarded-For` is deliberately NOT consulted: it is caller-controlled, and
/// honouring it here would let an attacker rotate buckets to brute-force the one
/// token that gates every provider key. The conservative direction is a shared
/// bucket behind a load balancer.
fn peer_ip(session: &ServerSession) -> String {
    match session.client_addr() {
        Some(a) => match a.as_inet() {
            Some(s) => throttle_bucket(s.ip()),
            None => a.to_string(),
        },
        // An in-process harness (or a unix peer) has no IP: one shared bucket.
        None => "unknown".to_string(),
    }
}

/// The failure-budget bucket for one peer address.
///
/// IPv6 is folded to its /64: a single client is normally delegated a /64 and
/// holds 2^64 addresses inside it, so keying the budget on the full address let
/// it rotate buckets and multiply its budget by 2^64 — the budget became
/// decorative against the exact attacker it exists for. Folding at /64 throttles
/// the whole customer instead (the conservative direction: a shared bucket can
/// only throttle sooner).
///
/// An IPv4-MAPPED IPv6 address (`::ffff:a.b.c.d`, what a dual-stack listener
/// reports for IPv4 peers) must be unwrapped FIRST — otherwise every IPv4 client
/// on the machine folds into the single `::/64` bucket and ten bad tokens from
/// one of them would throttle all of them.
fn throttle_bucket(ip: std::net::IpAddr) -> String {
    match ip {
        std::net::IpAddr::V4(v4) => v4.to_string(),
        std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let mut seg = v6.segments();
                for s in seg.iter_mut().skip(4) {
                    *s = 0;
                }
                std::net::Ipv6Addr::from(seg).to_string()
            }
        },
    }
}

impl AdminService {
    /// Count, throttle and answer a FAILED credential check on one of this port's
    /// two gates.
    ///
    /// The two gates differ ONLY in the token they compare and the message they
    /// answer with, so the bookkeeping lives in one place. It did not, once: the
    /// cluster-token gate compared its token and returned 401 with NO counter, NO
    /// throttle and NO log line while guarding the control plane (fleet config
    /// snapshots, cross-tenant sub-tenant/route writes) — one secret, unlimited
    /// free guesses, and nothing in the metrics or the logs to notice it with.
    ///
    /// `gate` qualifies every signal (`admin_denied` / `cluster_throttled` / …) so
    /// either gate can be alerted on independently.
    ///
    /// The peer budget is SHARED between the two gates — one key, one window. A
    /// peer that burns attempts on one token has spent them on this port, which
    /// can only throttle sooner, never later. The budget is never consulted for a
    /// SUCCESSFUL check (callers reach here only after a failed comparison), so
    /// this cannot lock an operator out of their own gateway. It is a per-process
    /// counter (`tenant_api::throttle`), so N reachable admin ports still mean N
    /// times the budget: a speed bump, not a cluster-wide quota.
    fn refuse_bad_credential(
        &self,
        session: &ServerSession,
        path: &str,
        trace_id: &str,
        gate: &str,
        message: &str,
    ) -> http::Response<Vec<u8>> {
        let key = peer_ip(session);
        let now = std::time::Instant::now();
        let window = std::time::Duration::from_secs(60);
        if !self
            .state
            .auth_fail_throttle
            .allow(&key, self.state.auth_fail_per_min, window, now)
        {
            let label = format!("{gate}_throttled");
            crate::admin::metrics::record_admin_auth_failure(&label);
            let retry_after = self
                .state
                .auth_fail_throttle
                .retry_after_secs(&key, window, now);
            warn!(
                target: "hydra::admin",
                gate = %gate,
                path = %path,
                peer = %key,
                "failed credential attempts from this peer exceed \
                 HYDRA_ADMIN_AUTH_FAIL_LIMIT_PER_MIN; answering 429"
            );
            return handlers::err_json_throttled(
                429,
                "too_many_failed_attempts",
                "too many failed attempts from this address",
                retry_after,
                trace_id,
            );
        }
        let label = format!("{gate}_denied");
        crate::admin::metrics::record_admin_auth_failure(&label);
        warn!(
            target: "hydra::admin",
            gate = %gate,
            path = %path,
            peer = %key,
            "credential denied (bad or missing token)"
        );
        handlers::err_json(401, "unauthorized", message, trace_id)
    }
}

pub struct AdminState {
    /// Leader-mode SQLite pool. `None` on edge nodes (no local DB, cluster
    /// P0b) — edge routes only serve `/metrics` `/healthz` `/readyz`, so no
    /// CRUD handler ever touches a `None` pool.
    pub pool: Option<SqlitePool>,
    pub store: ConfigStore,
    pub auth: Arc<HttpAuthChecker>,
    pub breaker: Arc<CircuitBreaker>,
    /// Master-key provider for sealing/opening provider upstream api-keys
    /// (design §16.2). Every `db::insert/get/list_provider_key[s]` call threads
    /// `key_provider.as_ref()` through the encrypt-on-write / decrypt-on-read
    /// boundary.
    pub key_provider: Arc<dyn KeyProvider>,
    /// Single admin bearer token (design §13.3). Read once at startup from
    /// `HYDRA_ADMIN_TOKEN` (in `main`); held here so there is no per-request env
    /// read (avoids races under parallel tests). `None` ⇒ fail-closed (deny all).
    pub admin_token: Option<String>,
    /// Serialises `reload_all` calls so concurrent writes don't race (design §6
    /// risk note: "最后一次为准").
    pub reload_lock: Mutex<()>,
    /// Shared admission controller (design §3 / §13.2). Cloned from the same
    /// `Arc<DashMap>` backing the proxy's `AppState.admission` — the
    /// `GET /api/v1/concurrency` endpoint reads live gate state from here.
    pub admission: AdmissionControl,
    /// The default concurrency policy (from `ProxyConfig`), needed to resolve a
    /// provider's CONFIGURED limits the same way the request path does
    /// (`hydra_core::config::resolve_policy`) so `GET /api/v1/concurrency` can report
    /// configured-vs-enforced and flag a change that needs a restart (plan §2bi / D-14).
    /// Injected via [`Self::with_default_concurrency_policy`]; the zero default keeps
    /// every existing call site behaving exactly as before, and it IS the value the
    /// request path uses: `ProxyConfig::default().default_concurrency_policy` is a
    /// compile-time constant with no environment reader (`main.rs` builds `ProxyConfig`
    /// as three env-driven timeouts plus `..Default::default()`), so there is nothing to
    /// thread through. If that ever gains an env knob, wire the real value here — a unit
    /// test in `admin/mod.rs` asserts the two defaults still agree.
    pub default_concurrency_policy: hydra_core::config::ConcurrencyPolicy,
    /// `true` when the LAST post-write `reload_all` failed, i.e. the in-memory
    /// snapshot no longer matches the committed config (same condition as the
    /// `hydra_config_snapshot_stale` gauge).
    ///
    /// PROCESS-level and last-writer-wins: it is set/cleared by the reload path
    /// and read by every admin response, so a given response may report ANOTHER
    /// request's reload failure. It answers "is the runtime consistent with the
    /// DB right now?", not "did MY write apply?" — the response field is
    /// documented that way on purpose.
    pub snapshot_stale: Arc<std::sync::atomic::AtomicBool>,

    /// Shared control-plane token (`HYDRA_CLUSTER_TOKEN`): gates the internal
    /// `/api/v1/internal/*` endpoints (cluster P1). `None` ⇒ internal
    /// endpoints are denied (fail-closed).
    pub cluster_token: Option<String>,
    /// Whether this node currently holds the leader lease (cluster P2).
    /// `Some(f)` on leader-candidate nodes: gates admin mutations (non-leader
    /// ⇒ forward to the active, P3) and `/healthz/leader`. `None` on
    /// single-node (`all`) and edge.
    pub leader_ready: Option<Arc<dyn Fn() -> bool + Send + Sync>>,

    /// Per-tenant config-write throttle (v2 D6): a fixed-window, process-local
    /// limiter keyed on the authenticated tenant id, applied by the leader's
    /// internal tenant-config write endpoint. In a cluster all config writes
    /// land on the leader, so one in-process window covers the cluster.
    /// **Anti-DoS only**: the window is in-process and therefore resets when
    /// leadership moves (a bounded burst after failover — the same accepted
    /// class as the E2 allow-TTL bound). On a single node the data-plane local
    /// write path is not the internal endpoint and is bounded by the general
    /// tenant-API per-tenant request budget instead.
    /// Per-IP budget for FAILED admin-token attempts (fixed window, in-process).
    /// Keyed on the PEER address only — `X-Forwarded-For` is deliberately not
    /// consulted here, so a spoofing client cannot rotate buckets; behind a load
    /// balancer every admin caller shares the LB's bucket, which can only
    /// throttle more, never less.
    pub auth_fail_throttle: Arc<crate::tenant_api::throttle::Throttle>,
    /// See [`admin_auth_fail_limit_per_min_from_env`].
    pub auth_fail_per_min: u32,
    /// Invalidation-stream publisher (cluster P4): `DELETE /api/v1/auth/cache`
    /// broadcasts the invalidation cluster-wide instead of clearing only the
    /// local cache. `None` off-cluster / on the single-node build.
    #[cfg(feature = "cluster-redis")]
    pub invalidation: Option<crate::cluster::events::InvalidationStream>,
    /// Placeholder so single-node builds keep a uniform shape.
    #[cfg(not(feature = "cluster-redis"))]
    #[allow(dead_code)]
    pub invalidation: Option<()>,
    /// The ids of the LIVE data-plane nodes, or `None` when this process did
    /// not inject them. Used by `DELETE /api/v1/auth/cache` to WAIT for the
    /// fleet to confirm an invalidation instead of reporting a boolean that
    /// could not be false.
    ///
    /// A closure rather than the registry itself, for the same reason
    /// `tenant_api::TenantApiConfig::live_nodes` is one: reading the registry is
    /// async, the handler is not, and a dead row must not hold a decision
    /// hostage. `main` injects the SAME refreshed view it gives the tenant API,
    /// so the operator's report and a tenant's report cannot disagree.
    #[cfg(feature = "cluster-redis")]
    pub live_nodes: Option<Arc<dyn Fn() -> Vec<String> + Send + Sync>>,
    /// How long `DELETE /api/v1/auth/cache` waits for confirmation.
    #[cfg(feature = "cluster-redis")]
    pub converge_timeout: std::time::Duration,
    /// The CONFIGURED member list (ADR-0001): the fleet, as far as any single node can know it.
    /// `None` off-cluster.
    #[cfg(feature = "arachne")]
    pub cluster_peers: Option<Arc<crate::cluster::arachne_node::ClusterPeers>>,
    /// This node's own identity, so the fleet view can mark which row is "self".
    #[cfg(feature = "arachne")]
    pub node_id: String,
    /// The raft leader's node name, when the control plane knows it (a synchronous read of the
    /// cache the 250 ms watch maintains).
    #[cfg(feature = "arachne")]
    pub leader_name: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>,
}

/// The default admission policy the concurrency endpoint resolves providers against.
///
/// It must equal `ProxyConfig::default().default_concurrency_policy` — the value the
/// REQUEST path uses when a provider sets no limits (all-zero = passthrough, no gate).
/// `ProxyConfig` builds that field with no environment reader, so the constant is the
/// single authority for both sides today; the unit test below fails if they ever drift.
pub const DEFAULT_CONCURRENCY_POLICY: hydra_core::config::ConcurrencyPolicy =
    hydra_core::config::ConcurrencyPolicy {
        max_concurrency: 0,
        max_queue_depth: 0,
        queue_wait_timeout_ms: 0,
    };

impl AdminState {
    /// Build admin state from the shared components. Cert re-resolution is not
    /// wired here: `ConfigStore`'s snapshot-change hook notifies the cert store
    /// after every swap (see the module docs).
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: Option<SqlitePool>,
        store: ConfigStore,
        auth: Arc<HttpAuthChecker>,
        breaker: Arc<CircuitBreaker>,
        key_provider: Arc<dyn KeyProvider>,
        admin_token: Option<String>,
        admission: AdmissionControl,
        cluster_token: Option<String>,
        leader_ready: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    ) -> Self {
        Self {
            pool,
            store,
            auth,
            breaker,
            key_provider,
            admin_token,
            reload_lock: Mutex::new(()),
            snapshot_stale: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            admission,
            default_concurrency_policy: DEFAULT_CONCURRENCY_POLICY,
            cluster_token,
            leader_ready,
            auth_fail_throttle: Arc::new(crate::tenant_api::throttle::Throttle::new()),
            auth_fail_per_min: admin_auth_fail_limit_per_min_from_env(),
            #[cfg(feature = "cluster-redis")]
            invalidation: None,
            #[cfg(not(feature = "cluster-redis"))]
            invalidation: None,
            // Both are injected by `main` through the builder below, not by a
            // new `new` parameter: this constructor has 14 call sites, and a
            // defaulted parameter is exactly what pushes them back to struct
            // literals.
            // Injected by `main` through the builder below, like `live_nodes`: this constructor
            // has many call sites and a defaulted parameter would push them back to struct literals.
            #[cfg(feature = "arachne")]
            cluster_peers: None,
            #[cfg(feature = "arachne")]
            node_id: String::new(),
            #[cfg(feature = "arachne")]
            leader_name: None,
            #[cfg(feature = "cluster-redis")]
            live_nodes: None,
            #[cfg(feature = "cluster-redis")]
            converge_timeout: std::time::Duration::from_millis(2_000),
        }
    }

    /// Inject the live-node view and the convergence budget for
    /// `DELETE /api/v1/auth/cache` (cluster mode).
    #[cfg(feature = "cluster-redis")]
    #[must_use]
    pub fn with_fleet(
        mut self,
        live_nodes: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
        converge_timeout: std::time::Duration,
    ) -> Self {
        self.live_nodes = Some(live_nodes);
        self.converge_timeout = converge_timeout;
        self
    }

    /// Inject the default concurrency policy so the concurrency endpoint can resolve a
    /// provider's configured limits exactly as the request path does (plan §2bi / D-14).
    #[must_use]
    pub fn with_default_concurrency_policy(
        mut self,
        policy: hydra_core::config::ConcurrencyPolicy,
    ) -> Self {
        self.default_concurrency_policy = policy;
        self
    }

    /// The leader-mode SQLite pool. Only leader/all admin routes reach this —
    /// edge mode short-circuits in the router before any CRUD dispatch, so the
    /// `expect` never fires on edge nodes.
    #[must_use]
    pub fn db(&self) -> &SqlitePool {
        self.pool
            .as_ref()
            .expect("admin SQLite pool (leader mode only)")
    }
}

/// The Pingora `ServeHttp` app: dispatches admin requests after the token gate.
pub struct AdminService {
    state: Arc<AdminState>,
}

impl AdminService {
    /// Build with the shared admin state.
    #[must_use]
    pub fn new(state: Arc<AdminState>) -> Self {
        Self { state }
    }

    /// Minimum accepted length for `HYDRA_ADMIN_TOKEN`, enforced at startup.
    ///
    /// The token is one shared secret gating the entire admin API (tenant,
    /// model, provider and `provider-key` CRUD, i.e. every upstream api-key and
    /// the ability to rotate them) and the gate has no rate limit or lockout, so
    /// a short or human-chosen token is brute-forceable. `main` refuses to boot
    /// with one. Generate with `openssl rand -hex 32`.
    pub const MIN_ADMIN_TOKEN_LEN: usize = 16;

    /// Minimum accepted `HYDRA_CLUSTER_TOKEN` length, the same floor as the admin
    /// token. The cluster token was only ever checked for PRESENCE (`main.rs`),
    /// so a 1-character token booted fine while the admin token refused to start
    /// below 16 — and this is the token that authorises the internal control
    /// plane and cross-tenant writes. One floor, two tokens that must both be
    /// unguessable.
    pub const MIN_CLUSTER_TOKEN_LEN: usize = 16;
    #[must_use]
    pub fn token_from_env() -> Option<String> {
        std::env::var("HYDRA_ADMIN_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
    }

    /// Extract the `Authorization: Bearer <token>` value, if present.
    fn bearer_token(session: &ServerSession) -> Option<&str> {
        session
            .req_header()
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| {
                s.strip_prefix("Bearer ")
                    .or_else(|| s.strip_prefix("bearer "))
            })
    }

    /// Token gate (design §13.3): require `Authorization: Bearer <token>` to
    /// match the configured token. Fail-closed when no token is configured.
    fn check_auth(&self, session: &ServerSession) -> bool {
        let Some(token) = &self.state.admin_token else {
            return false;
        };
        // Constant-time, like the cluster-token gate below and the tenant
        // access-token path in handlers: == on &str stops at the first
        // differing byte, which is a (weak, remote, no-lockout) timing oracle
        // over the admin token. Audit section 4.
        match Self::bearer_token(session) {
            Some(presented) => handlers::constant_time_eq(presented, token),
            None => false,
        }
    }

    /// The lightweight router (method + path-segment match, design §13.1).
    /// Every non-`/metrics` route is under `/api/v1/`.
    async fn route(
        &self,
        method: &str,
        path: &str,
        query: Option<&str>,
        session: &mut ServerSession,
        trace_id: &str,
    ) -> Resp {
        // /metrics — self-hosted exposition (§17).
        if path == "/metrics" {
            return handlers::metrics_endpoint();
        }

        let Some(rest) = path.strip_prefix("/api/v1/") else {
            return handlers::err_json(404, "not_found", "unknown path", trace_id);
        };
        let parts: Vec<&str> = rest.split('/').collect();

        // System routes.
        if parts == ["health"] {
            return handlers::health(&self.state, trace_id).await;
        }
        if parts == ["reload"] && method == "POST" {
            // `?force=1` is a SAFETY-RELEVANT ops override, so it is PARSED as a
            // parameter, never substring-matched: `q.contains("force=1")` would
            // also fire on `?x=force=1` and on `?noforce=1`.
            let force = query
                .map(|q| {
                    q.split('&').any(|kv| {
                        let mut it = kv.splitn(2, '=');
                        matches!((it.next(), it.next()), (Some("force"), Some("1")))
                    })
                })
                .unwrap_or(false);
            return cluster_api::reload(&self.state, force, trace_id).await;
        }
        // Auth cache invalidation.
        if parts == ["auth", "cache"] && method == "DELETE" {
            return handlers::auth_cache_invalidate(&self.state, session, trace_id).await;
        }
        // Breaker inspect / reset.
        if parts == ["breaker"] && method == "GET" {
            return handlers::breaker_list(&self.state);
        }
        if parts.len() == 2 && parts[0] == "breaker" && method == "DELETE" {
            return handlers::breaker_reset(&self.state, parts[1]);
        }
        // Concurrency admission snapshot (design §10 / §13.2).
        if parts == ["concurrency"] && method == "GET" {
            return handlers::concurrency_collection(&self.state);
        }
        // Usage statistics (design §17): token totals + request counts by
        // tenant and by provider, for the Admin UI Stats page.
        if parts == ["stats", "usage"] && method == "GET" {
            return handlers::stats_usage();
        }
        // The internal control plane was HERE: `GET /api/v1/internal/control?since=N`, which
        // served version-labelled config snapshots to edge/standby nodes. Retired with the channel
        // it existed for (ADR-0001 T4.1) — nodes materialize the config tree from Arachne, so
        // there is no snapshot to hand out and no `since` watermark to compare. A request to that
        // path is now an ordinary 404.
        // The `/api/v1/internal/tenant-config/*` family USED to be here: the leader's internal
        // write endpoint for sub-tenants and routes, reached by a forwarded data-plane write. It
        // is gone (ADR-0001 D-6, plan T3.5): the entry node applies the write itself, so there is
        // nothing to relay and no internal write face to authenticate. A request to that path now
        // falls through to the unknown-path 404 below, which is the honest answer — the family is
        // retired, not hidden.
        // Cluster status (cluster P4): whole-fleet view for the Health page.
        if parts == ["cluster", "status"] && method == "GET" {
            return cluster_api::cluster_status(&self.state, trace_id).await;
        }
        // Tenant auth-url probe (Admin UI "Test" button on the Tenants form):
        // POSTs a simulated auth request to the given auth_url and reports
        // reachability / protocol / verdict. Non-mutating (no DB write), but
        // POST so it carries the URL in the body.
        if parts == ["tenants", "auth", "test"] && method == "POST" {
            return handlers::tenant_auth_test(&self.state, session, trace_id).await;
        }
        // Tenant model catalog (design-tenant-model-catalog §2.3, P1): GET
        // /api/v1/tenants/{tenant_id}/models — read-only aggregate over the
        // config snapshot (static full-set view with per-provider online
        // flags; see the handler). Registered BEFORE the parts.len() > 2
        // deep-path rejection below (this path is exactly 3 segments); GET
        // only — any other method falls through to the depth rejection.
        if method == "GET" && parts.len() == 3 && parts[0] == "tenants" && parts[2] == "models" {
            return handlers::tenant_model_catalog(&self.state, parts[1], trace_id).await;
        }

        // REST CRUD resources.
        let resource = parts.first().copied().unwrap_or("");
        let id = parts.get(1).copied();
        // Reject paths deeper than resource[/id].
        if parts.len() > 2 {
            return handlers::err_json(404, "not_found", "unknown path", trace_id);
        }
        let state = &self.state;
        match (resource, id) {
            ("providers", None) => {
                handlers::provider_collection(state, session, method, trace_id).await
            }
            ("providers", Some(id)) => {
                handlers::provider_item(state, session, method, id, trace_id).await
            }
            ("provider-models", None) => {
                handlers::provider_model_collection(state, session, method, trace_id).await
            }
            ("provider-models", Some(id)) => {
                handlers::provider_model_item(state, session, method, id, trace_id).await
            }
            ("provider-keys", None) => {
                handlers::provider_key_collection(state, session, method, query, trace_id).await
            }
            ("provider-keys", Some(id)) => {
                handlers::provider_key_item(state, session, method, id, trace_id).await
            }
            ("tenants", None) => {
                handlers::tenant_collection(state, session, method, trace_id).await
            }
            ("tenants", Some(id)) => {
                handlers::tenant_item(state, session, method, id, trace_id).await
            }
            ("tenant-providers", None) => {
                handlers::tenant_provider_collection(state, session, method, trace_id).await
            }
            ("tenant-providers", Some(id)) => {
                handlers::tenant_provider_item(state, method, id, trace_id).await
            }
            ("tenant-models", None) => {
                handlers::tenant_model_collection(state, session, method, trace_id).await
            }
            ("tenant-models", Some(id)) => {
                handlers::tenant_model_item(state, method, id, trace_id).await
            }
            ("limit-roles", None) => {
                handlers::limit_role_collection(state, session, method, trace_id).await
            }
            ("limit-roles", Some(id)) => {
                handlers::limit_role_item(state, session, method, id, trace_id).await
            }
            ("provider-key-bindings", None) => {
                handlers::provider_key_binding_collection(state, session, method, trace_id).await
            }
            ("provider-key-bindings", Some(id)) => {
                handlers::provider_key_binding_item(state, session, method, id, trace_id).await
            }
            ("sub-tenants", None) => {
                handlers::sub_tenant_collection(state, session, method, query, trace_id).await
            }
            ("sub-tenants", Some(id)) => {
                handlers::sub_tenant_item(state, session, method, id, trace_id).await
            }
            ("sub-tenant-routes", None) => {
                handlers::sub_tenant_route_collection(state, session, method, query, trace_id).await
            }
            ("sub-tenant-routes", Some(id)) => {
                handlers::sub_tenant_route_item(state, session, method, id, trace_id).await
            }
            _ => handlers::err_json(404, "not_found", "unknown path", trace_id),
        }
    }
}

/// Longest relayed trace id we accept. Local ids are ~30 chars; 64 leaves room
/// for a peer's own format without letting a caller push a novel into the logs.
const MAX_RELAYED_TRACE_ID: usize = 64;

/// The trace id a forwarding peer put in `x-hydra-trace-id`, if it is usable.
///
/// Shape-checked on purpose: the value is read before authentication and ends up
/// in operator-visible logs and response bodies, so only a short
/// `[A-Za-z0-9._:-]` token is accepted. Anything else (empty, oversized, spaces,
/// control characters, ANSI escapes) returns `None` and the caller mints a local
/// id instead. See the call site for why a valid one is adopted rather than
/// merely recorded.
fn relayed_trace_id(session: &ServerSession) -> Option<String> {
    let raw = session
        .req_header()
        .headers
        .get("x-hydra-trace-id")?
        .to_str()
        .ok()?;
    if raw.is_empty() || raw.len() > MAX_RELAYED_TRACE_ID {
        return None;
    }
    raw.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
        .then(|| raw.to_string())
}

#[async_trait]
impl ServeHttp for AdminService {
    async fn response(&self, session: &mut ServerSession) -> http::Response<Vec<u8>> {
        // OC-3: the peer that forwarded this request put the TENANT-VISIBLE trace
        // id in `x-hydra-trace-id` (see `cluster::forward`, which uses the id the
        // edge also returns to the tenant in `X-Hydra-Trace-Id`). Minting a fresh
        // id here meant the leader's audit record
        // (`hydra::tenant_config_write`, formerly written by the retired internal endpoint) carried an
        // id the tenant could never see, so "here is my trace id, what happened to
        // my write?" could not be answered from the leader's audit trail at all —
        // the two sides of the same write were labelled with different strings.
        //
        // Adopting the inbound value IS the fix: the id in the audit record, in the
        // leader's error bodies and in the tenant's response header become one
        // string. The value is shape-checked first — this line runs BEFORE any
        // gate, so an unauthenticated caller reaching this port could otherwise
        // push a newline- or ANSI-bearing novel, or an unbounded string, into
        // operator-visible logs and response bodies.
        let trace_id = relayed_trace_id(session).unwrap_or_else(crate::proxy::new_trace_id);
        let method = session.req_header().method.as_str().to_string();
        let path = session.req_header().uri.path().to_string();
        let query = session.req_header().uri.query().map(str::to_string);

        // Edge data-plane node (cluster P0b): serve the health PROBES without a
        // token (an LB must be able to probe without holding a secret) and
        // everything else — including `/metrics` — through the ordinary gates.
        // No admin UI, no CRUD.
        //
        // `/metrics` used to be token-free here, which made the metrics exposure
        // depend on the ROLE: an edge published tenant/provider/model-labelled
        // series to anyone who could reach the port, while the same series on a
        // leader required the admin token — and the shipped cluster topology
        // binds the edge admin port to `0.0.0.0` (`jiqun-deploy.md`), so that was
        // the one deployment where it mattered. `/metrics` now takes the same
        // path as everywhere else: admin token required (see ops.md §9).

        // Leader-lease probe (cluster P2): 200 while this node holds the
        // lease, 503 on standby, 404 on non-candidate nodes. Token-free so
        // LBs / orchestrators can route to the active leader.
        if path == "/healthz/leader" {
            return cluster_api::leader_health(&self.state, &trace_id);
        }

        // Internal control-plane endpoints (cluster P1): gated by the SHARED
        // cluster token (`HYDRA_CLUSTER_TOKEN`), not the admin token — edges
        // hold only the cluster token. Fail-closed when unset.
        if path.starts_with("/api/v1/internal/") {
            // Both sides must be PRESENT and equal. Comparing
            // `Option != Option` made an unset `HYDRA_CLUSTER_TOKEN` (None —
            // every single-node deployment) plus an absent Authorization header
            // (also None) evaluate `None != None` == false, skipping the 401
            // and dispatching to `route()` BEFORE the admin-token gate below.
            // Explicitly deny the unset case so this stays fail-closed.
            let authorized = match (
                self.state.cluster_token.as_deref(),
                Self::bearer_token(session),
            ) {
                (Some(expected), Some(presented)) => {
                    handlers::constant_time_eq(expected, presented)
                }
                _ => false,
            };
            if !authorized {
                // Same budget, same counter, same log line as the admin gate: this
                // one guards the control plane and the cross-tenant write path, so
                // leaving it unmetered made guessing the cluster token free and
                // invisible (see `refuse_bad_credential`).
                return self.refuse_bad_credential(
                    session,
                    &path,
                    &trace_id,
                    "cluster",
                    "invalid cluster token",
                );
            }
            return self
                .route(&method, &path, query.as_deref(), session, &trace_id)
                .await;
        }

        // Embedded UI (design §14): serve `/admin/*` WITHOUT the admin token
        // gate. The static HTML/CSS/JS contain no secrets; `app.js` collects
        // the admin token in-memory and attaches `Authorization: Bearer` to
        // every `/api/v1/*` fetch. Only GET is allowed for the UI.
        if method == "GET" {
            if let Some(resp) = static_files::try_serve_admin(&path) {
                return resp;
            }
        }

        // Admin-token gate (design §13.3) — every request to the admin port.
        if !self.check_auth(session) {
            return self.refuse_bad_credential(
                session,
                &path,
                &trace_id,
                "admin",
                "missing or invalid admin token",
            );
        }

        // NO leader write gate any more (ADR-0001 T3.3). This node executes an admin mutation
        // LOCALLY and, once the write commits, publishes the resulting config to the control
        // plane; the raft library forwards the head write to the leader. The layer that relayed
        // the whole HTTP request to the lease holder is retired along with the Redis lease it was
        // built for (the write no longer needs a leader to LAND, only to be COMMITTED).
        self.route(&method, &path, query.as_deref(), session, &trace_id)
            .await
    }
}

#[cfg(test)]
mod tests {
    /// The concurrency endpoint resolves a provider's CONFIGURED limits with
    /// `DEFAULT_CONCURRENCY_POLICY`, while the request path resolves them with
    /// `ProxyConfig::default().default_concurrency_policy`. If those two ever drift, the
    /// endpoint would compare the enforced limits against the WRONG configured side and
    /// `limits_stale` would lie (plan §2bi / D-14).
    #[test]
    fn the_endpoint_default_policy_matches_proxyconfig() {
        assert_eq!(
            super::DEFAULT_CONCURRENCY_POLICY,
            crate::proxy::config::ProxyConfig::default().default_concurrency_policy,
            "AdminState's default admission policy drifted from ProxyConfig's"
        );
    }

    use super::throttle_bucket;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    /// IPv4 peers keep their own bucket. No aggregation is delegated in practice,
    /// and folding them would let one client throttle every other.
    #[test]
    fn an_ipv4_peer_keeps_its_own_bucket() {
        assert_eq!(
            throttle_bucket(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7))),
            "203.0.113.7"
        );
    }

    /// Every address inside one /64 shares a bucket. A client delegated a /64 holds
    /// 2^64 addresses, so keying the failure budget on the full address let it
    /// rotate buckets and multiply its budget by 2^64 — against the very attacker
    /// the budget exists for.
    #[test]
    fn an_ipv6_peer_is_bucketed_by_its_slash_64() {
        let a: Ipv6Addr = "2001:db8:1:2:aaaa:bbbb:cccc:dddd".parse().expect("addr");
        let same_64: Ipv6Addr = "2001:db8:1:2:1111:2222:3333:4444".parse().expect("addr");
        let other_64: Ipv6Addr = "2001:db8:1:3:aaaa:bbbb:cccc:dddd".parse().expect("addr");
        assert_eq!(
            throttle_bucket(IpAddr::V6(a)),
            throttle_bucket(IpAddr::V6(same_64)),
            "two addresses in one /64 must share a bucket"
        );
        assert_ne!(
            throttle_bucket(IpAddr::V6(a)),
            throttle_bucket(IpAddr::V6(other_64)),
            "a different /64 is a different peer"
        );
        assert_eq!(throttle_bucket(IpAddr::V6(a)), "2001:db8:1:2::");
    }

    /// An IPv4-MAPPED address must not fold into `::/64`: a dual-stack listener
    /// reports IPv4 peers that way, so collapsing them would put EVERY IPv4 caller
    /// in one bucket — ten bad tokens from one of them would throttle all of them.
    #[test]
    fn an_ipv4_mapped_peer_is_unwrapped_instead_of_folded() {
        let mapped: Ipv6Addr = "::ffff:203.0.113.7".parse().expect("addr");
        let other: Ipv6Addr = "::ffff:203.0.113.8".parse().expect("addr");
        assert_eq!(throttle_bucket(IpAddr::V6(mapped)), "203.0.113.7");
        assert_ne!(
            throttle_bucket(IpAddr::V6(mapped)),
            throttle_bucket(IpAddr::V6(other)),
            "two mapped IPv4 peers are two peers, not one `::/64`"
        );
    }
}
