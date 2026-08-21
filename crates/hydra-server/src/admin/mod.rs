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
    pub pool: SqlitePool,
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
}

impl AdminState {
    /// Build admin state from the shared components. `cert_reloader` is invoked
    /// after every successful `reload_all` (and by `POST /api/v1/reload`).
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: SqlitePool,
        store: ConfigStore,
        auth: Arc<HttpAuthChecker>,
        breaker: Arc<CircuitBreaker>,
        key_provider: Arc<dyn KeyProvider>,
        admin_token: Option<String>,
        cert_reloader: Option<Arc<dyn Fn() + Send + Sync>>,
        admission: AdmissionControl,
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

    /// Token gate (design §13.3): require `Authorization: Bearer <token>` to
    /// match the configured token. Fail-closed when no token is configured.
    fn check_auth(&self, session: &ServerSession) -> bool {
        let Some(token) = &self.state.admin_token else {
            return false;
        };
        if let Some(val) = session.req_header().headers.get("authorization") {
            if let Ok(s) = val.to_str() {
                if let Some(rest) = s
                    .strip_prefix("Bearer ")
                    .or_else(|| s.strip_prefix("bearer "))
                {
                    return rest == token;
                }
            }
        }
        false
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
}

#[async_trait]
impl ServeHttp for AdminService {
    async fn response(&self, session: &mut ServerSession) -> http::Response<Vec<u8>> {
        let trace_id = crate::proxy::new_trace_id();
        let method = session.req_header().method.as_str().to_string();
        let path = session.req_header().uri.path().to_string();
        let query = session.req_header().uri.query().map(str::to_string);

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
            debug!(target: "hydra::admin", path = %path, "admin auth denied");
            return handlers::err_json(
                401,
                "unauthorized",
                "missing or invalid admin token",
                &trace_id,
            );
        }

        self.route(&method, &path, query.as_deref(), session, &trace_id)
            .await
    }
}
