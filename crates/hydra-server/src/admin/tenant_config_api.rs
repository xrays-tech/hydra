//! Leader internal tenant-config write endpoint (sub-tenant v2, D3/D5/D6/D7).
//!
//! The edge data-plane forwards an authenticated tenant's sub-tenant / route
//! write here (`/api/v1/internal/tenant-config/...`, cluster-token gated in
//! `AdminService::response`). This handler is the SINGLE write point for tenant
//! self-service config, and enforces the A-2 receiver-side requirements, in
//! order:
//!
//! 1. **Lease assertion (A-2 前置 7)** — only the node that HOLDS the leader
//!    lease may execute a write: non-candidate → 404, candidate without the
//!    lease → 503 `not_leader`, `None`/holding → execute locally. A standby
//!    NEVER writes locally (mirror of `cluster_api::internal_control`).
//! 2. **Re-authentication (A-2 前置 4)** — the forwarded `x-hydra-tenant-token`
//!    (the caller's Bearer, D2) is re-authenticated against the SAME
//!    `ConfigStore` the data-plane used (`tenant_api::authenticate`): missing /
//!    empty → 401, `NotReady` → 503, `Unauthorized` → 401. This resolves the
//!    tenant `T` the write is attributed to.
//! 3. **Authorization binding (A-2 前置 4)** — the write target must be `T`'s
//!    own resources: a sub-tenant write with `body.tenant_id != T` is 403 by
//!    pure string comparison BEFORE any lookup (never a tenant-existence
//!    oracle); a route write whose `sub_tenant_id` is missing or owned by
//!    another tenant is 404 (resource-scoped). No other tenant's row is ever
//!    written.
//! 4. **Write (D5)** — the shared transactional write core
//!    (`admin::sub_tenant_write`), which upserts by natural key
//!    (`(tenant_id, name)` / `(sub_tenant_id, model_key)`) and keeps the
//!    auto-prefix retry (F3) intact.
//! 5. **Audit (A-2 前置 6 / D7)** — one structured `tracing` record per write
//!    (tenant_id, trace_id, action, resource, resource_id, config_version). The
//!    tenant bearer and request body are NEVER logged (A-2 前置 5).
//!
//! **V6 / D6 rate-limit seam**: a per-tenant `Throttle` (fixed window,
//! process-local) belongs between step 3 (binding) and step 4 (write) — keyed on
//! the authenticated tenant `T`, so it meters authorised work. It is
//! intentionally NOT implemented here yet (V6 adds the `Throttle`).
//!
//! These routes are NOT routed through the admin token or
//! `maybe_forward_mutation`: the edge already arrived here carrying the cluster
//! token (the internal gate), and a forward-once marker would make a retry loop
//! fail closed rather than recurse.

use serde::{Deserialize, Serialize};

use super::handlers::{empty, err_json, ok_json, read_body, Resp};
use super::sub_tenant_write::{self, CoreError};
use super::AdminState;
use crate::cluster::forward::TENANT_TOKEN_HEADER;
use crate::tenant_api::auth::{self, AuthError, AuthenticatedTenant};
use hydra_core::config::ConfigData;
use hydra_core::model::{SubTenant, SubTenantRoute};
use hydra_core::sub_tenant::SubTenantWriteError;
use pingora_core::protocols::http::ServerSession;
use sqlx::SqlitePool;

/// The resource written by a successful PUT, plus the config version it is live
/// at (A-2: the tenant can read `config_version` to confirm the write is applied
/// on the node that answered).
#[derive(Serialize)]
struct SubTenantWriteResp {
    sub_tenant: SubTenant,
    config_version: u64,
}

/// The route written by a successful PUT, plus the config version it is live at.
#[derive(Serialize)]
struct RouteWriteResp {
    route: SubTenantRoute,
    config_version: u64,
}

/// `PUT /internal/tenant-config/sub-tenants` body. `key_prefix` omitted ⇒
/// auto-generate with retry (Q13 / F3); an explicit value (incl. `""`) is
/// validated. `id` is NOT accepted: the upsert by natural key retains the
/// existing row's id on conflict, and a new row gets a server-generated id.
#[derive(Deserialize)]
struct SubTenantReq {
    tenant_id: String,
    name: String,
    #[serde(default)]
    key_prefix: Option<String>,
    #[serde(default = "default_true")]
    enabled: bool,
}

/// `PUT /internal/tenant-config/sub-tenant-routes` body. `model_key` omitted /
/// `null` ⇒ the sub-tenant's default route (the partial unique index on
/// `model_key IS NULL`). `id` is NOT accepted.
#[derive(Deserialize)]
struct RouteReq {
    sub_tenant_id: String,
    #[serde(default)]
    model_key: Option<String>,
    provider_id: String,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

/// A server-generated row id (used only for NEW rows; the natural-key upsert
/// retains the existing id on conflict). Mirrors `handlers::gen_id`.
fn gen_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tid = format!("{:?}", std::thread::current().id());
    format!("id-{nanos:x}-{}", tid.len())
}

// ---------------------------------------------------------------------------
// The plane-agnostic tenant config write (D5: admin internal + data-plane local)
// ---------------------------------------------------------------------------

/// The parsed tenant config write — the A-2 "authorised work" — independent of
/// which plane (the leader internal handler, or the data-plane local path, D8)
/// produced it. The write's declared target (`tenant_id` / `sub_tenant_id`) is
/// checked against the authenticated tenant by [`apply_config_write`] (the
/// binding gate), so a body that names another tenant is refused before any
/// lookup.
#[derive(Clone, Debug)]
pub(crate) enum TenantConfigWrite {
    /// `PUT .../sub-tenants` — upsert by natural key `(tenant_id, name)` (D4).
    /// Idempotent: a repeated PUT converges to the same row.
    UpsertSubTenant {
        tenant_id: String,
        name: String,
        key_prefix: Option<String>,
        enabled: bool,
    },
    /// `DELETE .../sub-tenants/{id}` — delete by immutable id (D4). Idempotent:
    /// an absent id is a no-op; a foreign id is a resource-scoped 404.
    DeleteSubTenant { id: String },
    /// `PUT .../sub-tenant-routes` — upsert by `(sub_tenant_id, model_key)` (D4,
    /// `None` = the default route). The sub-tenant must exist and be the
    /// authenticated tenant's own.
    UpsertRoute {
        sub_tenant_id: String,
        model_key: Option<String>,
        provider_id: String,
        enabled: bool,
    },
    /// `DELETE .../sub-tenant-routes/{id}` — delete by immutable id (D4).
    /// Idempotent: an absent id is a no-op; a foreign route is a 404.
    DeleteRoute { id: String },
}

/// The resource a successful write produced (the caller reports it with the
/// `config_version` it is live at), or the idempotent delete marker.
pub(crate) enum WriteOutcome {
    SubTenant(SubTenant),
    Route(SubTenantRoute),
    /// `DELETE` — idempotent; carries the (deleted or already-absent) id.
    Deleted(String),
}

/// Why a tenant config write was refused — by the A-2 binding gate (before the
/// write core) or by the write core itself. The caller maps each variant to a
/// precise HTTP response (the admin face and the data plane map to their own
/// envelopes, but the same variant → the same status/code).
#[derive(Debug)]
pub(crate) enum ApplyError {
    /// The write target tenant does not match the authenticated tenant (403).
    /// A pure string comparison, before any lookup — never a tenant-existence
    /// oracle.
    TenantMismatch,
    /// The resource does not exist, or belongs to another tenant (404). Missing
    /// and foreign are deliberately indistinguishable (no oracle).
    NotFound,
    /// A transactional write-core failure (400/409/500).
    Core(CoreError),
}

/// Apply a tenant config write: run the A-2 **binding gate** (the write's
/// declared target must be the authenticated tenant's own resources), then the
/// shared **transactional write core** (`sub_tenant_write`). This is the SINGLE
/// write point shared by the leader internal handler (V2/V3) and the data-plane
/// local path (V5, D8) — so the two faces cannot diverge on validation, quota or
/// natural-key upsert semantics.
///
/// It does **not** depend on `AdminState` or `ServerSession`: it takes the
/// config [`SqlitePool`], the current [`ConfigData`] (provider / model / tenant
/// membership), the authenticated tenant id (the write target, resolved from the
/// tenant token) and the parsed write. It performs the write and returns the
/// produced resource, leaving the snapshot **reload** to the caller — the admin
/// face uses the `reload_best_effort` helper (serialising lock + stale flag),
/// the data-plane local path calls `ConfigStore::reload_all` directly
/// (single-node: the node is the only writer).
pub(crate) async fn apply_config_write(
    pool: &SqlitePool,
    cfg: &ConfigData,
    tenant_id: &str,
    write: &TenantConfigWrite,
) -> Result<WriteOutcome, ApplyError> {
    match write {
        TenantConfigWrite::UpsertSubTenant {
            tenant_id: target,
            name,
            key_prefix,
            enabled,
        } => {
            // A-2 binding: the write target must be the authenticated tenant, by
            // pure string comparison BEFORE any lookup (never a tenant-existence
            // oracle).
            if target != tenant_id {
                return Err(ApplyError::TenantMismatch);
            }
            // D4 idempotent upsert by natural key `(tenant_id, name)`: resolve the
            // existing row and CONVERGE (update by its immutable id), or create it
            // (the atomic upsert handles a concurrent race). An omitted
            // `key_prefix` keeps the current one on an existing row and
            // auto-generates (Q13 / F3) on a new one.
            let existing = match crate::db::list_sub_tenants(pool).await {
                Ok(rows) => rows
                    .into_iter()
                    .find(|s| s.tenant_id == *target && s.name == *name),
                Err(e) => return Err(ApplyError::Core(CoreError::Db(e))),
            };
            let st = match existing {
                Some(st) => {
                    let prefix = key_prefix.clone().unwrap_or_else(|| st.key_prefix.clone());
                    sub_tenant_write::update_sub_tenant(pool, cfg, &st.id, name, &prefix, *enabled)
                        .await
                        .map_err(ApplyError::Core)?
                }
                None => {
                    let id = gen_id();
                    sub_tenant_write::create_sub_tenant(
                        pool,
                        cfg,
                        target,
                        name,
                        key_prefix.as_deref(),
                        &id,
                        *enabled,
                    )
                    .await
                    .map_err(ApplyError::Core)?
                }
            };
            Ok(WriteOutcome::SubTenant(st))
        }
        TenantConfigWrite::DeleteSubTenant { id } => {
            // A-2 binding: the row (if present) must be the tenant's own. A
            // foreign row is 404 (resource-scoped; never written); an absent row
            // is an idempotent no-op.
            match crate::db::get_sub_tenant(pool, id).await {
                Ok(st) if st.tenant_id == tenant_id => {}
                Ok(_) => return Err(ApplyError::NotFound),
                Err(sqlx::Error::RowNotFound) => return Ok(WriteOutcome::Deleted(id.clone())),
                Err(e) => return Err(ApplyError::Core(CoreError::Db(e))),
            }
            sub_tenant_write::delete_sub_tenant(pool, id)
                .await
                .map_err(ApplyError::Core)?;
            Ok(WriteOutcome::Deleted(id.clone()))
        }
        TenantConfigWrite::UpsertRoute {
            sub_tenant_id,
            model_key,
            provider_id,
            enabled,
        } => {
            // A-2 binding: the sub-tenant must exist and be the tenant's own.
            // Missing OR foreign → 404 (resource-scoped; indistinguishable, no
            // oracle).
            let st = match crate::db::get_sub_tenant(pool, sub_tenant_id).await {
                Ok(st) => st,
                Err(sqlx::Error::RowNotFound) => return Err(ApplyError::NotFound),
                Err(e) => return Err(ApplyError::Core(CoreError::Db(e))),
            };
            if st.tenant_id != tenant_id {
                return Err(ApplyError::NotFound);
            }
            let id = gen_id();
            let r = sub_tenant_write::create_route(
                pool,
                cfg,
                sub_tenant_id,
                provider_id,
                model_key.as_deref(),
                &id,
                *enabled,
            )
            .await
            .map_err(ApplyError::Core)?;
            Ok(WriteOutcome::Route(r))
        }
        TenantConfigWrite::DeleteRoute { id } => {
            // A-2 binding: the route (if present) must belong to the tenant's
            // sub-tenant. A foreign sub-tenant is 404 (resource-scoped); an
            // absent row is a no-op.
            match crate::db::get_sub_tenant_route(pool, id).await {
                Ok(r) => {
                    let st = match crate::db::get_sub_tenant(pool, &r.sub_tenant_id).await {
                        Ok(st) => st,
                        // A dangling sub-tenant is impossible (FK CASCADE); treat
                        // as a no-op so a replay after a cascade cannot 404.
                        Err(sqlx::Error::RowNotFound) => {
                            return Ok(WriteOutcome::Deleted(id.clone()))
                        }
                        Err(e) => return Err(ApplyError::Core(CoreError::Db(e))),
                    };
                    if st.tenant_id != tenant_id {
                        return Err(ApplyError::NotFound);
                    }
                }
                Err(sqlx::Error::RowNotFound) => return Ok(WriteOutcome::Deleted(id.clone())),
                Err(e) => return Err(ApplyError::Core(CoreError::Db(e))),
            }
            sub_tenant_write::delete_route(pool, id)
                .await
                .map_err(ApplyError::Core)?;
            Ok(WriteOutcome::Deleted(id.clone()))
        }
    }
}

// ---------------------------------------------------------------------------
// A-2 receiver-side gates (lease → re-auth → binding)
// ---------------------------------------------------------------------------

/// A-2 前置 7 — receiver-side lease assertion. Returns `Some(resp)` when the
/// node must NOT execute locally (404 non-candidate / 503 not-leader); `None`
/// when it may proceed (holding the lease, or no election — single-node).
/// Mirrors `cluster_api::internal_control` (the snapshot producer gate).
fn lease_gate(state: &AdminState, trace_id: &str) -> Option<Resp> {
    if !state.is_leader_candidate() {
        return Some(err_json(
            404,
            "not_found",
            "the internal tenant-config write endpoint is only served by leader-candidate nodes",
            trace_id,
        ));
    }
    if let Some(is_leader) = state.leader_ready.as_ref() {
        if !is_leader() {
            return Some(err_json(
                503,
                "not_leader",
                "this node does not hold the leader lease; retry against the active leader",
                trace_id,
            ));
        }
    }
    None
}

/// A-2 前置 4 — tenant re-authentication. Reads the `x-hydra-tenant-token`
/// header the edge set (D2: the caller's Bearer, NOT the cluster token);
/// missing / empty → 401, then `authenticate` against the shared `ConfigStore`:
/// `NotReady` → 503, `Unauthorized` → 401. Returns the resolved tenant `T`.
// `Resp` (~336 bytes) is the error variant; the `Ok` path stays cheap (same
// allowance as `handlers::read_body` / `parse_body`).
#[allow(clippy::result_large_err)]
fn reauth(
    state: &AdminState,
    session: &ServerSession,
    trace_id: &str,
) -> Result<AuthenticatedTenant, Resp> {
    let bearer = session
        .req_header()
        .headers
        .get(TENANT_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty());
    let Some(bearer) = bearer else {
        return Err(err_json(
            401,
            "unauthorized",
            "missing tenant token",
            trace_id,
        ));
    };
    match auth::authenticate(&state.store, bearer) {
        Ok(t) => Ok(t),
        Err(AuthError::NotReady) => Err(err_json(
            503,
            "not_ready",
            "this node has no configuration yet",
            trace_id,
        )),
        Err(AuthError::Unauthorized) => Err(err_json(
            401,
            "unauthorized",
            "invalid tenant token",
            trace_id,
        )),
    }
}

/// v2 D6 (A-2 6) — per-tenant config-write throttle. Applied AFTER re-auth, so
/// the key is the authenticated tenant id, and BEFORE the write. A tenant
/// cannot amplify config writes past its budget. The window is process-local
/// and anti-DoS only (all writes land on the leader; it resets on failover).
///
/// `None` ⇒ allowed; `Some(resp)` ⇒ a `429` was produced.
fn throttle_gate(state: &AdminState, tenant_id: &str, trace_id: &str) -> Option<Resp> {
    let window = std::time::Duration::from_secs(60);
    let now = std::time::Instant::now();
    if state
        .config_write_throttle
        .allow(tenant_id, state.config_write_per_min, window, now)
    {
        return None;
    }
    let retry_after = state
        .config_write_throttle
        .retry_after_secs(tenant_id, window, now);
    Some(err_json(
        429,
        "too_many_requests",
        &format!("config-write budget exceeded; retry in {retry_after}s"),
        trace_id,
    ))
}

// ---------------------------------------------------------------------------
// Entry point + the four write handlers
// ---------------------------------------------------------------------------

/// Dispatch the `/api/v1/internal/tenant-config/...` family. The caller has
/// already checked `parts[0] == "internal"` and `parts[1] == "tenant-config"`.
/// Shape: `[internal, tenant-config, <resource>(, <id>)]`.
pub(super) async fn route(
    state: &AdminState,
    method: &str,
    parts: &[&str],
    session: &mut ServerSession,
    trace_id: &str,
) -> Resp {
    let Some(&resource) = parts.get(2) else {
        return err_json(404, "not_found", "unknown path", trace_id);
    };
    let id = parts.get(3).copied();
    match resource {
        "sub-tenants" => match (method, id) {
            ("PUT", None) => write_sub_tenant(state, session, trace_id).await,
            ("DELETE", Some(id)) => delete_sub_tenant(state, id, session, trace_id).await,
            _ => method_not_allowed(trace_id),
        },
        "sub-tenant-routes" => match (method, id) {
            ("PUT", None) => write_route(state, session, trace_id).await,
            ("DELETE", Some(id)) => delete_route(state, id, session, trace_id).await,
            _ => method_not_allowed(trace_id),
        },
        _ => err_json(404, "not_found", "unknown path", trace_id),
    }
}

/// `PUT /internal/tenant-config/sub-tenants` — upsert by natural key
/// `(tenant_id, name)` (D4). Idempotent: a repeated PUT converges to the same
/// row (the existing id is retained on a name conflict).
async fn write_sub_tenant(state: &AdminState, session: &mut ServerSession, trace_id: &str) -> Resp {
    // A-2 7: only the lease holder writes.
    if let Some(r) = lease_gate(state, trace_id) {
        return r;
    }
    // A-2 4: re-auth the forwarded tenant bearer → tenant T.
    let t = match reauth(state, session, trace_id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let tenant_id = t.tenant.id.clone();

    let body = match read_body(session, trace_id).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let req: SubTenantReq = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(e) => {
            return err_json(
                400,
                "invalid_json",
                &format!("failed to parse request body: {e}"),
                trace_id,
            )
        }
    };

    // V6 / D6 (A-2 6): per-tenant config-write throttle, after re-auth.
    if let Some(r) = throttle_gate(state, &tenant_id, trace_id) {
        return r;
    }

    // The shared transactional write core (D5): binding (the write target must
    // be T, a pure string comparison) + the D4 idempotent upsert by
    // `(tenant_id, name)`. An omitted `key_prefix` keeps the current one on an
    // existing row and auto-generates (Q13 / F3) on a new one.
    let cfg = std::sync::Arc::clone(&*state.store.snapshot());
    let write = TenantConfigWrite::UpsertSubTenant {
        tenant_id: req.tenant_id,
        name: req.name,
        key_prefix: req.key_prefix,
        enabled: req.enabled,
    };
    match apply_config_write(state.db(), &cfg, &tenant_id, &write).await {
        Ok(WriteOutcome::SubTenant(st)) => {
            reload_best_effort(state, trace_id).await;
            let config_version = state.store.version();
            // A-2 6 / D7: one structured audit record per write (never the
            // bearer/body).
            audit(
                trace_id,
                "upsert",
                "sub_tenant",
                &st.id,
                &tenant_id,
                config_version,
            );
            ok_json(
                200,
                &SubTenantWriteResp {
                    sub_tenant: st,
                    config_version,
                },
            )
        }
        // A sub-tenant upsert never yields a route row or a delete marker; fail
        // closed (404) rather than panic on the (unreachable) outcome.
        Ok(WriteOutcome::Route(_) | WriteOutcome::Deleted(_)) => {
            apply_err_resp(ApplyError::NotFound, trace_id, "sub_tenant not found")
        }
        Err(e) => apply_err_resp(e, trace_id, "sub_tenant not found"),
    }
}

/// `DELETE /internal/tenant-config/sub-tenants/{id}` — delete by immutable id.
/// Idempotent: an absent id is a no-op (204); a foreign id is 404 (resource-
/// scoped, never another tenant's row).
async fn delete_sub_tenant(
    state: &AdminState,
    id: &str,
    session: &mut ServerSession,
    trace_id: &str,
) -> Resp {
    if let Some(r) = lease_gate(state, trace_id) {
        return r;
    }
    let t = match reauth(state, session, trace_id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let tenant_id = t.tenant.id.clone();
    if let Some(r) = throttle_gate(state, &tenant_id, trace_id) {
        return r;
    }

    // The shared write core (D5): binding (the row, if present, must be T's own —
    // a foreign row is 404, an absent row an idempotent no-op) + the D4 delete by
    // immutable id.
    let write = TenantConfigWrite::DeleteSubTenant { id: id.to_string() };
    let cfg = std::sync::Arc::clone(&*state.store.snapshot());
    match apply_config_write(state.db(), &cfg, &tenant_id, &write).await {
        Ok(WriteOutcome::Deleted(id)) => {
            reload_best_effort(state, trace_id).await;
            let config_version = state.store.version();
            audit(
                trace_id,
                "delete",
                "sub_tenant",
                &id,
                &tenant_id,
                config_version,
            );
            empty(204)
        }
        // A delete never yields a sub-tenant/route row; fail closed (404) rather
        // than panic on the (unreachable) outcome.
        Ok(WriteOutcome::SubTenant(_) | WriteOutcome::Route(_)) => {
            apply_err_resp(ApplyError::NotFound, trace_id, "sub_tenant not found")
        }
        Err(e) => apply_err_resp(e, trace_id, "sub_tenant not found"),
    }
}

/// `PUT /internal/tenant-config/sub-tenant-routes` — upsert by natural key
/// `(sub_tenant_id, model_key)` (D4, `NULL` = default route).
async fn write_route(state: &AdminState, session: &mut ServerSession, trace_id: &str) -> Resp {
    if let Some(r) = lease_gate(state, trace_id) {
        return r;
    }
    let t = match reauth(state, session, trace_id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let tenant_id = t.tenant.id.clone();

    let body = match read_body(session, trace_id).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let req: RouteReq = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(e) => {
            return err_json(
                400,
                "invalid_json",
                &format!("failed to parse request body: {e}"),
                trace_id,
            )
        }
    };

    // V6 / D6 (A-2 6): per-tenant config-write throttle, after re-auth.
    if let Some(r) = throttle_gate(state, &tenant_id, trace_id) {
        return r;
    }

    // The shared write core (D5): binding (the sub-tenant must exist and be T's
    // own — missing OR foreign → 404, indistinguishable) + the D4 upsert by
    // `(sub_tenant_id, model_key)`.
    let cfg = std::sync::Arc::clone(&*state.store.snapshot());
    let write = TenantConfigWrite::UpsertRoute {
        sub_tenant_id: req.sub_tenant_id,
        model_key: req.model_key,
        provider_id: req.provider_id,
        enabled: req.enabled,
    };
    match apply_config_write(state.db(), &cfg, &tenant_id, &write).await {
        Ok(WriteOutcome::Route(r)) => {
            reload_best_effort(state, trace_id).await;
            let config_version = state.store.version();
            audit(
                trace_id,
                "upsert",
                "sub_tenant_route",
                &r.id,
                &tenant_id,
                config_version,
            );
            ok_json(
                200,
                &RouteWriteResp {
                    route: r,
                    config_version,
                },
            )
        }
        // A route upsert never yields a sub-tenant row or a delete marker; fail
        // closed (404) rather than panic on the (unreachable) outcome.
        Ok(WriteOutcome::SubTenant(_) | WriteOutcome::Deleted(_)) => {
            apply_err_resp(ApplyError::NotFound, trace_id, "sub_tenant not found")
        }
        Err(e) => apply_err_resp(e, trace_id, "sub_tenant not found"),
    }
}

/// `DELETE /internal/tenant-config/sub-tenant-routes/{id}` — delete by
/// immutable id. Idempotent (absent → 204); a foreign row is 404.
async fn delete_route(
    state: &AdminState,
    id: &str,
    session: &mut ServerSession,
    trace_id: &str,
) -> Resp {
    if let Some(r) = lease_gate(state, trace_id) {
        return r;
    }
    let t = match reauth(state, session, trace_id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let tenant_id = t.tenant.id.clone();
    if let Some(r) = throttle_gate(state, &tenant_id, trace_id) {
        return r;
    }

    // The shared write core (D5): binding (the route, if present, must belong to
    // T's sub-tenant — a foreign sub-tenant is 404, an absent row a no-op) + the
    // D4 delete by immutable id.
    let cfg = std::sync::Arc::clone(&*state.store.snapshot());
    let write = TenantConfigWrite::DeleteRoute { id: id.to_string() };
    match apply_config_write(state.db(), &cfg, &tenant_id, &write).await {
        Ok(WriteOutcome::Deleted(id)) => {
            reload_best_effort(state, trace_id).await;
            let config_version = state.store.version();
            audit(
                trace_id,
                "delete",
                "sub_tenant_route",
                &id,
                &tenant_id,
                config_version,
            );
            empty(204)
        }
        // A route delete never yields a sub-tenant/route row; fail closed (404)
        // rather than panic on the (unreachable) outcome.
        Ok(WriteOutcome::SubTenant(_) | WriteOutcome::Route(_)) => {
            apply_err_resp(ApplyError::NotFound, trace_id, "sub_tenant_route not found")
        }
        Err(e) => apply_err_resp(e, trace_id, "sub_tenant_route not found"),
    }
}

// ---------------------------------------------------------------------------
// Response mapping + audit
// ---------------------------------------------------------------------------

/// Map an [`ApplyError`] (the shared write core's binding / core outcome) to the
/// internal endpoint's HTTP response. `not_found_msg` is the 404 message for the
/// resource being written (a sub-tenant vs. a route).
fn apply_err_resp(e: ApplyError, trace_id: &str, not_found_msg: &str) -> Resp {
    match e {
        ApplyError::TenantMismatch => err_json(
            403,
            "tenant_id_mismatch",
            "the write target tenant does not match the authenticated tenant",
            trace_id,
        ),
        ApplyError::NotFound => err_json(404, "not_found", not_found_msg, trace_id),
        ApplyError::Core(core) => core_err_resp(core, trace_id, not_found_msg),
    }
}

/// Map a write-core [`CoreError`] to a semantic HTTP response (400/409/500/404).
/// Mirrors `handlers::sub_tenant_core_err_resp` (private there), so the internal
/// endpoint maps the shared core's typed errors identically to the admin face.
fn core_err_resp(e: CoreError, trace_id: &str, not_found_msg: &str) -> Resp {
    match e {
        CoreError::Validation(v) => validation_err_resp(&v, trace_id),
        CoreError::Db(d) => db_err_resp(&d, trace_id),
        CoreError::PrefixGenerationFailed => err_json(
            400,
            "prefix_generation_failed",
            &format!(
                "could not generate a non-conflicting key_prefix after {} attempts",
                sub_tenant_write::PREFIX_ATTEMPTS
            ),
            trace_id,
        ),
        CoreError::NotFound => err_json(404, "not_found", not_found_msg, trace_id),
    }
}

/// Map a [`SubTenantWriteError`] to a 400/409 (duplicates are 409; every other
/// rule violation is 400). Mirrors `handlers::sub_tenant_write_err_resp`.
fn validation_err_resp(e: &SubTenantWriteError, trace_id: &str) -> Resp {
    let (status, code) = match e {
        SubTenantWriteError::NameDuplicate => (409, "name_duplicate"),
        SubTenantWriteError::PrefixDuplicate => (409, "prefix_duplicate"),
        SubTenantWriteError::ProviderNotFound => (400, "provider_not_found"),
        SubTenantWriteError::ProviderNotInTenant => (400, "provider_not_in_tenant"),
        SubTenantWriteError::ModelNotInTenant => (400, "model_not_in_tenant"),
        SubTenantWriteError::ModelNotServedByProvider => (400, "model_not_served_by_provider"),
        SubTenantWriteError::NameInvalid => (400, "invalid_name"),
        SubTenantWriteError::PrefixEmpty => (400, "empty_key_prefix"),
        SubTenantWriteError::PrefixNonAscii => (400, "invalid_key_prefix"),
        SubTenantWriteError::PrefixNoSeparator => (400, "invalid_key_prefix"),
        SubTenantWriteError::PrefixOverlap => (400, "key_prefix_overlap"),
        SubTenantWriteError::SubTenantQuotaExceeded => (400, "quota_exceeded"),
        SubTenantWriteError::RouteQuotaExceeded => (400, "quota_exceeded"),
    };
    err_json(status, code, &e.to_string(), trace_id)
}

/// Classify a sqlx error into (HTTP status, stable code) — the same mapping as
/// the admin face (`handlers::classify_db_err`).
fn db_err_resp(e: &sqlx::Error, trace_id: &str) -> Resp {
    let (status, code) = if let sqlx::Error::Database(db) = e {
        if db.is_unique_violation() {
            (409, "conflict")
        } else if db.is_foreign_key_violation() {
            (400, "foreign_key_violation")
        } else if db.is_check_violation() {
            (400, "check_violation")
        } else if matches!(db.kind(), sqlx::error::ErrorKind::NotNullViolation) {
            (400, "missing_required_field")
        } else {
            (500, "database_error")
        }
    } else {
        (500, "database_error")
    };
    err_json(status, code, &e.to_string(), trace_id)
}

/// Reload the in-memory snapshot after a committed write (write-after
/// consistency, design §13.2). Best-effort: a fatal validation failure is
/// logged and recorded, not fatal — the write already committed (design §5.3
/// keeps the old snapshot; the next successful reload recovers).
async fn reload_best_effort(state: &AdminState, trace_id: &str) {
    let _guard = state.reload_lock.lock().await;
    if let Err(e) = state.store.reload_all().await {
        tracing::error!(
            target: "hydra::tenant_config_write",
            trace_id, error = %e,
            "post-write reload_all FAILED: the in-memory config snapshot is now STALE"
        );
        super::metrics::record_config_snapshot_stale(true);
        state
            .snapshot_stale
            .store(true, std::sync::atomic::Ordering::Release);
        return;
    }
    super::metrics::record_config_snapshot_stale(false);
    state
        .snapshot_stale
        .store(false, std::sync::atomic::Ordering::Release);
}

/// A-2 前置 6 / D7 — one structured audit record per successful write.
/// Carries the attribution (who / what / which / when). NEVER the tenant bearer
/// or the request body (A-2 前置 5).
fn audit(
    trace_id: &str,
    action: &str,
    resource: &str,
    resource_id: &str,
    tenant_id: &str,
    config_version: u64,
) {
    tracing::info!(
        target: "hydra::tenant_config_write",
        tenant_id = %tenant_id,
        trace_id = %trace_id,
        action = %action,
        resource = %resource,
        resource_id = %resource_id,
        config_version = config_version,
        "sub-tenant config write (leader internal)"
    );
}

fn method_not_allowed(trace_id: &str) -> Resp {
    err_json(
        405,
        "method_not_allowed",
        "HTTP method not allowed for this resource",
        trace_id,
    )
}

// ===========================================================================
// Unit tests — the A-2 前置 7 lease gate (direct, no session needed).
//
// Mirrors `cluster_api::tests`: over HTTP an edge is 404'd by the router before
// dispatch, so the non-candidate branch is only reachable by calling the gate
// directly (which is what makes the HTTP 404 "router behaviour" and the 503
// "handler behaviour" verifiable as distinct claims).
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{KeyProvider, StaticKeyProvider};
    use std::sync::Arc;

    fn body_json(resp: &Resp) -> serde_json::Value {
        serde_json::from_slice(resp.body()).expect("error body is JSON")
    }

    /// Build a candidate/edge state with the lease answer under test control.
    async fn state(edge_mode: bool, is_leader: Option<bool>) -> Arc<AdminState> {
        let pool = crate::db::init_pool("sqlite::memory:")
            .await
            .expect("init_pool");
        crate::db::run_migrate(&pool).await.expect("migrate");
        let kp: Arc<dyn KeyProvider> = Arc::new(StaticKeyProvider::new([1u8; 32], 1));
        let store = crate::store::ConfigStore::load(pool.clone(), kp.clone())
            .await
            .expect("ConfigStore::load");
        let auth = Arc::new(
            crate::http::HttpAuthChecker::new(
                crate::http::AuthCache::new(
                    std::time::Duration::from_secs(300),
                    std::time::Duration::from_secs(30),
                ),
                crate::http::AuthConfig::default(),
            )
            .expect("HttpAuthChecker"),
        );
        let breaker = Arc::new(crate::proxy::breaker_wrap::CircuitBreaker::new(
            hydra_core::breaker::BreakerConfig::new(2),
        ));
        let leader_ready: Option<Arc<dyn Fn() -> bool + Send + Sync>> =
            is_leader.map(|v| Arc::new(move || v) as Arc<dyn Fn() -> bool + Send + Sync>);
        Arc::new(AdminState::new(
            Some(pool),
            store,
            auth,
            breaker,
            kp,
            Some("admin-token-16chars".to_string()),
            crate::proxy::admission::AdmissionControl::new(),
            edge_mode,
            Some("cluster-token".to_string()),
            leader_ready,
        ))
    }

    /// A non-candidate (edge) is refused by the gate itself, not only by the
    /// router (defence in depth).
    #[tokio::test]
    async fn a_non_candidate_is_refused_by_the_gate() {
        // edge_mode ⇒ is_leader_candidate() == false, *and* it claims to hold the
        // lease: the role gate must still win.
        let state = state(true, Some(true)).await;
        let resp = lease_gate(&state, "t").expect("a non-candidate is refused");
        assert_eq!(resp.status().as_u16(), 404);
        assert_eq!(body_json(&resp)["error"]["code"], "not_found");
    }

    /// A candidate WITHOUT the lease is 503 `not_leader` — never a local write.
    #[tokio::test]
    async fn a_candidate_without_the_lease_is_503_not_leader() {
        let state = state(false, Some(false)).await;
        let resp = lease_gate(&state, "t").expect("a standby is refused");
        assert_eq!(resp.status().as_u16(), 503);
        assert_eq!(body_json(&resp)["error"]["code"], "not_leader");
    }

    /// A candidate HOLDING the lease passes the gate.
    #[tokio::test]
    async fn a_candidate_holding_the_lease_passes() {
        let state = state(false, Some(true)).await;
        assert!(
            lease_gate(&state, "t").is_none(),
            "a lease holder executes locally"
        );
    }

    /// No election (`leader_ready: None`, the single-node/`all` shape) passes the
    /// gate — pre-existing behaviour (D8: single-node = local execution).
    #[tokio::test]
    async fn no_election_passes_the_gate() {
        let state = state(false, None).await;
        assert!(
            lease_gate(&state, "t").is_none(),
            "no election ⇒ no lease gate"
        );
    }
}
