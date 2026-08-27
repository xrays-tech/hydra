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
//!   `ConfigStore::reload_all()` then re-resolves certs (design §13.2 / W4b
//!   cert-reload contract), serialised by a per-state mutex.
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
use tracing::debug;

use crate::crypto::KeyProvider;
use crate::http::HttpAuthChecker;
use crate::proxy::admission::AdmissionControl;
use crate::proxy::breaker_wrap::CircuitBreaker;
use crate::store::ConfigStore;

pub mod handlers;
pub mod metrics;
mod static_files;

// Re-export the metrics module publicly so the proxy / breaker / tls can reach
// the `record_*` call-sites and the `/metrics` renderer.
pub use metrics as metrics_export;

use handlers::Resp;

/// Shared state for the admin service (design §13.1: a subset of `AppState`).
/// Cheap to `Arc`-clone so tests can inspect it after requests.
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
    /// Optional cert-reload hook (W4b contract): after `reload_all`, re-resolves
    /// certs so downstream TLS picks up new cert paths. `None` on plain-TCP
    /// builds (no TLS listener) or when no cert store is wired. Held as a
    /// cfg-free closure so `AdminState` has a uniform shape across feature sets.
    pub cert_reloader: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Shared admission controller (design §3 / §13.2). Cloned from the same
    /// `Arc<DashMap>` backing the proxy's `AppState.admission` — the
    /// `GET /api/v1/concurrency` endpoint reads live gate state from here.
    pub admission: AdmissionControl,
    /// Edge data-plane mode (cluster P0b): the admin service serves only
    /// `/metrics` `/healthz` `/readyz`; everything else is 404 (no CRUD, no UI).
    pub edge_mode: bool,
    /// Shared control-plane token (`HYDRA_CLUSTER_TOKEN`): gates the internal
    /// `/api/v1/internal/*` endpoints (cluster P1). `None` ⇒ internal
    /// endpoints are denied (fail-closed).
    pub cluster_token: Option<String>,
    /// Whether this node currently holds the leader lease (cluster P2).
    /// `Some(f)` on leader-candidate nodes: gates admin mutations (non-leader
    /// ⇒ forward to the active, P3) and `/healthz/leader`. `None` on
    /// single-node (`all`) and edge.
    pub leader_ready: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// Invalidation-stream publisher (cluster P4): `DELETE /api/v1/auth/cache`
    /// broadcasts the invalidation cluster-wide instead of clearing only the
    /// local cache. `None` off-cluster / on the single-node build.
    #[cfg(feature = "cluster-redis")]
    pub invalidation: Option<crate::cluster::events::InvalidationStream>,
    /// Placeholder so single-node builds keep a uniform shape.
    #[cfg(not(feature = "cluster-redis"))]
    #[allow(dead_code)]
    pub invalidation: Option<()>,
    /// Fleet registry (cluster P4): backs `GET /api/v1/cluster/status` for
    /// the Admin UI Health page (whole-cluster view: nodes, roles, liveness,
    /// lease holder). `None` off-cluster.
    #[cfg(feature = "cluster-redis")]
    pub cluster_registry: Option<Arc<crate::cluster::registry::NodeRegistry>>,
    /// Placeholder so single-node builds keep a uniform shape.
    #[cfg(not(feature = "cluster-redis"))]
    #[allow(dead_code)]
    pub cluster_registry: Option<()>,
}

impl AdminState {
    /// Build admin state from the shared components. `cert_reloader` is invoked
    /// after every successful `reload_all` (and by `POST /api/v1/reload`).
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: Option<SqlitePool>,
        store: ConfigStore,
        auth: Arc<HttpAuthChecker>,
        breaker: Arc<CircuitBreaker>,
        key_provider: Arc<dyn KeyProvider>,
        admin_token: Option<String>,
        cert_reloader: Option<Arc<dyn Fn() + Send + Sync>>,
        admission: AdmissionControl,
        edge_mode: bool,
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
            cert_reloader,
            admission,
            edge_mode,
            cluster_token,
            leader_ready,
            #[cfg(feature = "cluster-redis")]
            invalidation: None,
            #[cfg(not(feature = "cluster-redis"))]
            invalidation: None,
            #[cfg(feature = "cluster-redis")]
            cluster_registry: None,
            #[cfg(not(feature = "cluster-redis"))]
            cluster_registry: None,
        }
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

    /// Resolve the URL admin mutations should be forwarded to: the ACTUAL
    /// lease holder from the cluster registry (cluster P3/P4). The target is
    /// resolved LIVE at forward time — never from a static
    /// `HYDRA_CONTROL_URL`, which for a primary leader candidate may point
    /// at THIS node itself (the self-forward loop bug) and cannot track the
    /// lease across failover.
    ///
    /// `Ok(None)` ⇒ no forward target is resolvable right now — the caller
    /// must fail closed (503, never a local write on a standby).
    async fn resolve_forward_target(&self) -> Result<Option<String>, String> {
        #[cfg(feature = "cluster-redis")]
        {
            match &self.cluster_registry {
                Some(registry) => {
                    crate::cluster::forward::forward_target_from_registry(registry).await
                }
                None => Ok(None), // no registry → cannot know the active leader
            }
        }
        #[cfg(not(feature = "cluster-redis"))]
        {
            Ok(None)
        }
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

    /// Read the configured admin token from the environment (used by `main`).
    /// Returns `None` when unset ⇒ the service denies all requests (fail-closed,
    /// design §13.3).
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
        Self::bearer_token(session) == Some(token.as_str())
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
            return handlers::reload(&self.state, trace_id).await;
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
        // Internal control plane (cluster P1): snapshot distribution.
        if parts == ["internal", "control"] && method == "GET" {
            return handlers::internal_control(&self.state, query, trace_id).await;
        }
        // Cluster status (cluster P4): whole-fleet view for the Health page.
        if parts == ["cluster", "status"] && method == "GET" {
            return handlers::cluster_status(&self.state, trace_id).await;
        }
        // Tenant auth-url probe (Admin UI "Test" button on the Tenants form):
        // POSTs a simulated auth request to the given auth_url and reports
        // reachability / protocol / verdict. Non-mutating (no DB write), but
        // POST so it carries the URL in the body.
        if parts == ["tenants", "auth", "test"] && method == "POST" {
            return handlers::tenant_auth_test(&self.state, session, trace_id).await;
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
            _ => handlers::err_json(404, "not_found", "unknown path", trace_id),
        }
    }

    /// Leader write gate (cluster P2/P3): on leader-candidate nodes that do
    /// NOT hold the lease, admin mutations are FORWARDED to the active
    /// leader (P3) — reads stay local (the replica serves them). The
    /// forward target is resolved LIVE from the cluster registry (the
    /// ACTUAL lease holder), never from a static HYDRA_CONTROL_URL: a
    /// static URL may point at this node itself (the self-forward loop
    /// bug) and cannot track the lease across failover. Fail-closed: when
    /// no target is resolvable, or the active is unreachable, the mutation
    /// fails 503/502 (a standby must never write locally; taking over is
    /// the lease machine's job).
    ///
    /// Returns `Some(resp)` when the request was handled (forwarded /
    /// loop-guarded / forward failed); `None` when the node is the lease
    /// holder (or there is no election) — the caller executes locally.
    async fn maybe_forward_mutation(
        &self,
        method: &str,
        path: &str,
        query: Option<&str>,
        session: &mut ServerSession,
        trace_id: &str,
    ) -> Option<Resp> {
        let Some(is_leader) = &self.state.leader_ready else {
            return None;
        };
        let mutation = matches!(method, "POST" | "PUT" | "PATCH" | "DELETE");
        if !mutation || is_leader() {
            return None;
        }
        // Forward-once marker (loop guard): a mutation that already
        // travelled through a standby must never be forwarded again —
        // this terminates any (self- or mutual-) forward loop with an
        // immediate fail-closed 503 instead of a timeout recursion.
        if session
            .req_header()
            .headers
            .contains_key(crate::cluster::forward::FORWARD_ONCE_HEADER)
        {
            return Some(handlers::err_json(
                503,
                "forward_loop",
                "mutation already forwarded once; refusing to forward again (forward loop guard)",
                trace_id,
            ));
        }
        let path_and_query = match query {
            Some(q) => format!("{path}?{q}"),
            None => path.to_string(),
        };
        let target = match self.state.resolve_forward_target().await {
            Ok(Some(target)) => target,
            Ok(None) => {
                return Some(handlers::err_json(
                    503,
                    "not_leader",
                    "this node is not the active leader and no forward target is resolvable (lease holder unknown)",
                    trace_id,
                ));
            }
            Err(e) => {
                return Some(handlers::err_json(
                    503,
                    "not_leader",
                    &format!("this node is not the active leader; forward target resolution failed: {e}"),
                    trace_id,
                ));
            }
        };
        let body = handlers::read_body(session).await;
        match crate::cluster::forward::forward_mutation(
            &target,
            method,
            &path_and_query,
            body,
            &session.req_header().headers,
            trace_id,
        )
        .await
        {
            Ok(resp) => Some(resp),
            Err(e) => Some(handlers::err_json(
                502,
                "forward_failed",
                &format!("{e}; the active leader is unreachable (no local write)"),
                trace_id,
            )),
        }
    }

}

#[async_trait]
impl ServeHttp for AdminService {
    async fn response(&self, session: &mut ServerSession) -> http::Response<Vec<u8>> {
        let trace_id = crate::proxy::new_trace_id();
        let method = session.req_header().method.as_str().to_string();
        let path = session.req_header().uri.path().to_string();
        let query = session.req_header().uri.query().map(str::to_string);

        // Edge data-plane node (cluster P0b): serve ONLY the probe endpoints
        // (`/metrics` `/healthz` `/readyz`) — no token (healthchecks), no
        // admin UI, no CRUD. Everything else is 404.
        if self.state.edge_mode {
            if path == "/metrics" || path == "/healthz" || path == "/readyz" {
                return match path.as_str() {
                    "/metrics" => handlers::metrics_endpoint(),
                    _ => handlers::health(&self.state, &trace_id).await,
                };
            }
            return handlers::err_json(404, "not_found", "edge node: no admin API", &trace_id);
        }

        // Leader-lease probe (cluster P2): 200 while this node holds the
        // lease, 503 on standby, 404 on non-candidate nodes. Token-free so
        // LBs / orchestrators can route to the active leader.
        if path == "/healthz/leader" {
            return handlers::leader_health(&self.state, &trace_id);
        }

        // Internal control-plane endpoints (cluster P1): gated by the SHARED
        // cluster token (`HYDRA_CLUSTER_TOKEN`), not the admin token — edges
        // hold only the cluster token. Fail-closed when unset.
        if path.starts_with("/api/v1/internal/") {
            let bearer = Self::bearer_token(session);
            if self.state.cluster_token.as_deref() != bearer {
                return handlers::err_json(401, "unauthorized", "invalid cluster token", &trace_id);
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

        // Tenant self-service endpoint (migration 0009): POST
        // /api/v1/tenants/{tenant_id}/auth/cache/invalidate — gated by the
        // TENANT access token (NOT the admin token). Identity comes from the
        // token; the URL tenant_id must match it or the call is rejected
        // (no cross-tenant spoofing). Runs before the admin-token gate so a
        // tenant never needs the operator's admin token (欠费停机 / 付费恢复).
        let tenant_self_path = path
            .strip_prefix("/api/v1/tenants/")
            .and_then(|r| r.strip_suffix("/auth/cache/invalidate"));
        if let Some(url_tenant) = tenant_self_path {
            if method == "POST" && !url_tenant.is_empty() && !url_tenant.contains('/') {
                let Some(bearer) = Self::bearer_token(session).map(str::to_string) else {
                    return handlers::err_json(401, "unauthorized", "invalid tenant access token", &trace_id);
                };
                let Some(tenant_id) = handlers::tenant_id_for_token(&self.state, &bearer).await else {
                    return handlers::err_json(401, "unauthorized", "invalid tenant access token", &trace_id);
                };
                if tenant_id != url_tenant {
                    return handlers::err_json(403, "forbidden", "token does not match tenant_id", &trace_id);
                }
                // Leader write gate: a standby forwards the invalidation to
                // the ACTUAL lease holder (which re-validates the token).
                if let Some(resp) = self
                    .maybe_forward_mutation(&method, &path, query.as_deref(), session, &trace_id)
                    .await
                {
                    return resp;
                }
                return handlers::tenant_auth_cache_invalidate(
                    &self.state, session, &tenant_id, &trace_id,
                )
                .await;
            }
        }

        // Admin-token gate (design §13.3) — every request to the admin port.
        if !self.check_auth(session) {
            debug!(target: "hydra::admin", path = %path, "admin auth denied");
            return handlers::err_json(
                401,
                "unauthorized",
                "missing or invalid admin token",
                &trace_id,
            );
        }

        // Leader write gate (cluster P2/P3) — see `maybe_forward_mutation`.
        if let Some(resp) = self
            .maybe_forward_mutation(&method, &path, query.as_deref(), session, &trace_id)
            .await
        {
            return resp;
        }

        self.route(&method, &path, query.as_deref(), session, &trace_id)
            .await
    }
}
