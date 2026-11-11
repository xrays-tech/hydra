//! Cluster admin API — node status and the reload endpoint.
//!
//! With the shared-database architecture there is no raft control plane and no
//! leader: every node writes the shared relational database directly. The
//! status payload therefore reports only facts a single node can know about
//! itself; it deliberately has no peer list, lease holder or raft term.

use serde::Serialize;

use super::handlers::{err_json, ok_json, Resp};
use super::{AdminState, WriteCtx};
use crate::store::ReloadSource;

/// This node's view of itself. `cluster=false` on a single-node deployment —
/// the UI then renders just the local Health panel.
#[derive(Serialize)]
struct ClusterStatusDto {
    cluster: bool,
    mode: String,
    node_id: String,
    /// `all` or `cluster`.
    role: String,
    /// The config version this node currently serves.
    config_version: u64,
    /// Whether the shared relational database answered a liveness query.
    db_reachable: bool,
    /// Whether a Redis URL is configured for the data plane.
    redis_configured: bool,
    /// `None` when no Redis is configured, otherwise the result of one bounded
    /// PING against the shared backend. The probe is capped by the normal Redis
    /// command timeout (500 ms by default), so this endpoint stays cheap.
    redis_reachable: Option<bool>,
}

/// `GET /api/v1/cluster/status` (admin-token gated): this node's own status.
pub(super) async fn cluster_status(state: &AdminState, _trace_id: &str) -> Resp {
    // Liveness only: execute the query and do not decode a scalar. `SELECT 1`
    // is INT4 on PostgreSQL and BIGINT on MySQL, so a typed scalar decode is
    // backend-fragile; `fetch_one` proves the round trip without that coupling.
    let db_reachable = sqlx::query("SELECT 1").fetch_one(state.db()).await.is_ok();
    let redis_configured = std::env::var("HYDRA_REDIS_URL")
        .ok()
        .is_some_and(|url| !url.trim().is_empty());
    // `Some(false)` when configured but the backend was not injected: that is
    // not expected (main injects it for every cluster node), and "unreachable"
    // is the honest answer when we cannot prove otherwise.
    #[cfg(feature = "cluster-redis")]
    let redis_reachable = if redis_configured {
        match &state.redis {
            Some(backend) => Some(backend.ping().await),
            None => Some(false),
        }
    } else {
        None
    };
    #[cfg(not(feature = "cluster-redis"))]
    let redis_reachable = if redis_configured { Some(false) } else { None };
    let clustered = state.role.is_cluster();
    let dto = ClusterStatusDto {
        cluster: clustered,
        mode: if clustered { "cluster" } else { "single" }.to_string(),
        node_id: state.node_id.clone(),
        role: state.role.to_string(),
        config_version: state.store.version(),
        db_reachable,
        redis_configured,
        redis_reachable,
    };
    ok_json(200, &dto)
}

/// Response of `POST /api/v1/reload`.
///
/// `status` and the counters are a documented contract consumed by the Admin UI
/// (`admin-ui/app.js`) and asserted in `tests/admin_api.rs`.
#[derive(Serialize)]
struct ReloadBody {
    status: &'static str,
    /// Whether this reload advanced the generation (false ⇒ no-op, the
    /// snapshot was unchanged). `?force=1` makes this true by construction.
    changed: bool,
    /// The version AFTER the reload.
    version: u64,
    tenants: usize,
    providers: usize,
    models: usize,
    keys: usize,
    certs: usize,
}

/// `POST /api/v1/reload?force=1`.
///
/// The error stays **400 `reload_failed`** (documented in
/// `admin-ui/api-docs.js`). `force` preserves the pre-T6 behaviour of advancing
/// the generation unconditionally. Without it a reload whose content is
/// unchanged is a NO-OP.
///
/// The admin router already holds the store's reload guard for this POST, so
/// the reload goes through the locked entry point — one lock only. The store
/// owns `snapshot_stale` and the `hydra_config_snapshot_stale` gauge, so a
/// failure here is recorded by the same code that serves the write paths.
pub(super) async fn reload(state: &AdminState, ctx: &WriteCtx<'_>, force: bool) -> Resp {
    // A fatal validation failure is reported as 400 (design §5.3: the old
    // snapshot is retained). Certs follow the swap via the `ConfigStore` hook.
    let result = match ctx.guard {
        Some(guard) => {
            state
                .store
                .reload_with(guard, force, ReloadSource::Local)
                .await
        }
        // Defensive fallback for a direct/test caller outside the router: the
        // public entry point takes the lock itself, so this cannot deadlock.
        None => state.store.reload_local(force).await,
    };
    let changed = match result {
        Ok(changed) => changed,
        Err(e) => {
            // The store already marked itself stale (and the gauge); the
            // response keeps the documented contract.
            return err_json(
                400,
                "reload_failed",
                &format!("config reload failed (old snapshot retained): {e}"),
                ctx.trace_id,
            );
        }
    };
    let snap = state.store.snapshot();
    ok_json(
        200,
        &ReloadBody {
            status: "reloaded",
            changed,
            version: state.store.version(),
            tenants: snap.tenants_by_domain.len(),
            providers: snap.providers.len(),
            models: snap.models_by_key.len(),
            keys: snap.provider_keys.len(),
            certs: snap.certs.len(),
        },
    )
}
