//! Admin REST handlers — the business logic behind each route (design §13.2).
//!
//! Every handler takes a borrowed [`AdminState`] and the live Pingora
//! [`ServerSession`] (so it can read the request body when needed) and returns a
//! complete `http::Response<Vec<u8>>`. The router in [`super`] dispatches to
//! these after the admin-token gate. No internal logic is mocked: each handler
//! drives the real `db::repo`, `ConfigStore`, `AuthChecker`, `CircuitBreaker`
//! and `HydraCertStore`.

use std::sync::Arc;

use http::Response;
use hydra_core::model::{
    LimitRole, Provider, ProviderKey, ProviderKeyBinding, ProviderModel, Tenant, TenantModel,
    TenantProvider,
};
use pingora_core::protocols::http::ServerSession;
use serde::{Deserialize, Serialize};

use super::AdminState;
use crate::admin::metrics;
use crate::http::AuthChecker;

/// A fully-built HTTP response (the `ServeHttp` return type).
pub(super) type Resp = Response<Vec<u8>>;

// ---------------------------------------------------------------------------
// Error model (design §13.4)
// ---------------------------------------------------------------------------

/// Unified error envelope: `{ "error": { "code", "message", "trace_id" } }`.
#[derive(Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Serialize)]
struct ErrorDetail {
    code: String,
    message: String,
    trace_id: String,
}

/// Build a JSON error response.
pub(super) fn err_json(status: u16, code: &str, message: &str, trace_id: &str) -> Resp {
    let body = ErrorBody {
        error: ErrorDetail {
            code: code.to_string(),
            message: message.to_string(),
            trace_id: trace_id.to_string(),
        },
    };
    json_response(status, to_json(&body))
}

/// Build a JSON success response from any `Serialize` value.
pub(super) fn ok_json<T: Serialize>(status: u16, value: &T) -> Resp {
    json_response(status, to_json(value))
}

/// Empty-body response (e.g. 204 after DELETE).
pub(super) fn empty(status: u16) -> Resp {
    json_response(status, Vec::new())
}

fn json_response(status: u16, body: Vec<u8>) -> Resp {
    let mut builder = Response::builder().status(status);
    if body.is_empty() {
        builder = builder.header("content-length", "0");
    } else {
        builder = builder
            .header("content-type", "application/json")
            .header("content-length", body.len().to_string());
    }
    builder.body(body).unwrap_or_else(|_| Response::new(vec![]))
}

/// Serialise to a JSON byte vec, logging (never panicking) on failure.
fn to_json<T: Serialize>(value: &T) -> Vec<u8> {
    match serde_json::to_vec(value) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!(target: "hydra::admin", error = %e, "failed to serialise admin response");
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// DB error → HTTP status mapping (design §13.4)
// ---------------------------------------------------------------------------

/// Classify a sqlx error into (HTTP status, stable code slug).
///
/// - UNIQUE violation → 409
/// - CHECK / NOT NULL / FK violation → 400 (FK e.g. model→provider, §13.2)
/// - everything else → 500
fn classify_db_err(e: &sqlx::Error) -> (u16, &'static str) {
    if let sqlx::Error::Database(db) = e {
        if db.is_unique_violation() {
            return (409, "conflict");
        }
        if db.is_foreign_key_violation() {
            return (400, "foreign_key_violation");
        }
        if db.is_check_violation() {
            return (400, "check_violation");
        }
        if matches!(db.kind(), sqlx::error::ErrorKind::NotNullViolation) {
            return (400, "missing_required_field");
        }
    }
    (500, "database_error")
}

/// True when the sqlx error is "row not found" (→ 404).
fn is_not_found(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::RowNotFound)
}

// ---------------------------------------------------------------------------
// Shared reload (write-after consistency + cert resolve, design §13.2/§12.1)
// ---------------------------------------------------------------------------

/// Reload the in-memory snapshot and (under a TLS backend) re-resolve certs so
/// downstream TLS picks up new cert paths. Serialised by the per-state mutex so
/// concurrent writes don't race (design §6 risk note). Best-effort: a fatal
/// validation failure is logged but does **not** fail an already-committed write
/// (design §5.3 keeps the old snapshot; the next successful reload recovers).
async fn reload_best_effort(state: &AdminState, trace_id: &str) {
    let _guard = state.reload_lock.lock().await;
    if let Err(e) = state.store.reload_all().await {
        tracing::warn!(
            target: "hydra::admin",
            trace_id, error = %e,
            "post-write reload_all failed; in-memory snapshot kept (design §5.3)"
        );
        return;
    }
    // Cert-reload contract (W4b): after a successful reload, re-resolve certs
    // so the next TLS handshake sees new cert paths.
    if let Some(reload_certs) = state.cert_reloader.as_ref() {
        reload_certs();
    }
}

// ---------------------------------------------------------------------------
// id/timestamp helpers (server-generated when the client omits them)
// ---------------------------------------------------------------------------

fn now_ts() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn gen_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tid = format!("{:?}", std::thread::current().id());
    format!("id-{nanos:x}-{}", tid.len())
}

/// Read the full request body into a vec (empty for bodyless requests).
async fn read_body(session: &mut ServerSession) -> Vec<u8> {
    let mut buf = Vec::new();
    while let Ok(Some(chunk)) = session.read_request_body().await {
        buf.extend_from_slice(&chunk);
    }
    buf
}

// `Resp` (http::Response<Vec<u8>>) is ~336 bytes; clippy flags the `Result`
// err-variant as large. The `Ok` path stays cheap; allow it here.
#[allow(clippy::result_large_err)]
fn parse_body<T: for<'de> Deserialize<'de>>(body: &[u8], trace_id: &str) -> Result<T, Resp> {
    serde_json::from_slice(body).map_err(|e| {
        err_json(
            400,
            "invalid_json",
            &format!("failed to parse request body: {e}"),
            trace_id,
        )
    })
}

// ===========================================================================
// Providers
// ===========================================================================

pub(super) async fn provider_collection(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    trace_id: &str,
) -> Resp {
    if method == "GET" {
        match crate::db::list_providers(&state.pool).await {
            Ok(rows) => ok_json(200, &rows),
            Err(e) => db_err_resp(e, trace_id),
        }
    } else if method == "POST" {
        let body = read_body(session).await;
        let mut p: Provider = match parse_body(&body, trace_id) {
            Ok(p) => p,
            Err(r) => return r,
        };
        if p.id.is_empty() {
            p.id = gen_id();
        }
        let ts = now_ts();
        if p.created_at.is_empty() {
            p.created_at = ts.clone();
        }
        if p.updated_at.is_empty() {
            p.updated_at = ts;
        }
        match crate::db::insert_provider(&state.pool, &p).await {
            Ok(()) => {}
            Err(e) => return db_err_resp(e, trace_id),
        }
        reload_best_effort(state, trace_id).await;
        ok_json(201, &p)
    } else {
        method_not_allowed(trace_id)
    }
}

pub(super) async fn provider_item(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    id: &str,
    trace_id: &str,
) -> Resp {
    match method {
        "GET" => match crate::db::get_provider(&state.pool, id).await {
            Ok(p) => ok_json(200, &p),
            Err(e) if is_not_found(&e) => {
                err_json(404, "not_found", "provider not found", trace_id)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        "PUT" => {
            let body = read_body(session).await;
            let mut p: Provider = match parse_body(&body, trace_id) {
                Ok(p) => p,
                Err(r) => return r,
            };
            p.id = id.to_string();
            p.updated_at = now_ts();
            match crate::db::update_provider(&state.pool, &p).await {
                Ok(()) => {}
                Err(e) => return db_err_resp(e, trace_id),
            }
            match crate::db::get_provider(&state.pool, id).await {
                Ok(p) => {
                    reload_best_effort(state, trace_id).await;
                    ok_json(200, &p)
                }
                Err(_) => err_json(404, "not_found", "provider not found", trace_id),
            }
        }
        "DELETE" => match crate::db::delete_provider(&state.pool, id).await {
            Ok(()) => {
                reload_best_effort(state, trace_id).await;
                empty(204)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        _ => method_not_allowed(trace_id),
    }
}

// ===========================================================================
// Provider models
// ===========================================================================

pub(super) async fn provider_model_collection(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    trace_id: &str,
) -> Resp {
    if method == "GET" {
        match crate::db::list_provider_models(&state.pool).await {
            Ok(rows) => ok_json(200, &rows),
            Err(e) => db_err_resp(e, trace_id),
        }
    } else if method == "POST" {
        let body = read_body(session).await;
        let m: ProviderModel = match parse_body(&body, trace_id) {
            Ok(m) => m,
            Err(r) => return r,
        };
        match crate::db::insert_provider_model(&state.pool, &m).await {
            Ok(()) => {}
            Err(e) => return db_err_resp(e, trace_id),
        }
        reload_best_effort(state, trace_id).await;
        ok_json(201, &m)
    } else {
        method_not_allowed(trace_id)
    }
}

pub(super) async fn provider_model_item(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    id: &str,
    trace_id: &str,
) -> Resp {
    match method {
        "GET" => match crate::db::get_provider_model(&state.pool, id).await {
            Ok(m) => ok_json(200, &m),
            Err(e) if is_not_found(&e) => err_json(404, "not_found", "model not found", trace_id),
            Err(e) => db_err_resp(e, trace_id),
        },
        "PUT" => {
            let body = read_body(session).await;
            let mut m: ProviderModel = match parse_body(&body, trace_id) {
                Ok(m) => m,
                Err(r) => return r,
            };
            m.id = id.to_string();
            match crate::db::update_provider_model(&state.pool, &m).await {
                Ok(()) => {}
                Err(e) => return db_err_resp(e, trace_id),
            }
            match crate::db::get_provider_model(&state.pool, id).await {
                Ok(m) => {
                    reload_best_effort(state, trace_id).await;
                    ok_json(200, &m)
                }
                Err(_) => err_json(404, "not_found", "model not found", trace_id),
            }
        }
        "DELETE" => match crate::db::delete_provider_model(&state.pool, id).await {
            Ok(()) => {
                reload_best_effort(state, trace_id).await;
                empty(204)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        _ => method_not_allowed(trace_id),
    }
}

// ===========================================================================
// Provider keys (ALWAYS masked — P1-5: the admin API never returns plaintext)
// ===========================================================================

pub(super) async fn provider_key_collection(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    query: Option<&str>,
    trace_id: &str,
) -> Resp {
    // `?reveal=1` is accepted for backward-compat but is now a no-op: the
    // admin API NEVER returns plaintext provider keys (P1-5). An admin-token
    // leak must not pull every upstream key.
    let _reveal = query.is_some_and(|q| q.split('&').any(|kv| kv == "reveal=1"));
    if method == "GET" {
        match crate::db::list_provider_keys(&state.pool, state.key_provider.as_ref()).await {
            Ok(rows) => {
                let out: Vec<ProviderKey> = rows
                    .into_iter()
                    .map(|mut k| {
                        k.api_key = hydra_core::rewrite::mask_key(&k.api_key);
                        k
                    })
                    .collect();
                ok_json(200, &out)
            }
            Err(e) => db_err_resp(e, trace_id),
        }
    } else if method == "POST" {
        let body = read_body(session).await;
        let mut k: ProviderKey = match parse_body(&body, trace_id) {
            Ok(k) => k,
            Err(r) => return r,
        };
        if k.id.is_empty() {
            k.id = gen_id();
        }
        if k.created_at.is_empty() {
            k.created_at = now_ts();
        }
        match crate::db::insert_provider_key(&state.pool, state.key_provider.as_ref(), &k).await {
            Ok(()) => {}
            Err(e) => return db_err_resp(e, trace_id),
        }
        reload_best_effort(state, trace_id).await;
        // Never echo plaintext back (P1-5).
        k.api_key = hydra_core::rewrite::mask_key(&k.api_key);
        ok_json(201, &k)
    } else {
        method_not_allowed(trace_id)
    }
}

pub(super) async fn provider_key_item(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    id: &str,
    trace_id: &str,
) -> Resp {
    match method {
        "GET" => {
            match crate::db::get_provider_key(&state.pool, state.key_provider.as_ref(), id).await {
                Ok(mut k) => {
                    k.api_key = hydra_core::rewrite::mask_key(&k.api_key);
                    ok_json(200, &k)
                }
                Err(e) if is_not_found(&e) => err_json(404, "not_found", "key not found", trace_id),
                Err(e) => db_err_resp(e, trace_id),
            }
        }
        "PUT" => {
            // provider_key has no dedicated update fn (W2): upsert via
            // delete + insert.
            let body = read_body(session).await;
            let mut k: ProviderKey = match parse_body(&body, trace_id) {
                Ok(k) => k,
                Err(r) => return r,
            };
            k.id = id.to_string();
            if k.created_at.is_empty() {
                k.created_at = now_ts();
            }
            let _ = crate::db::delete_provider_key(&state.pool, id).await;
            match crate::db::insert_provider_key(&state.pool, state.key_provider.as_ref(), &k).await
            {
                Ok(()) => {}
                Err(e) => return db_err_resp(e, trace_id),
            }
            reload_best_effort(state, trace_id).await;
            k.api_key = hydra_core::rewrite::mask_key(&k.api_key);
            ok_json(200, &k)
        }
        "DELETE" => match crate::db::delete_provider_key(&state.pool, id).await {
            Ok(()) => {
                reload_best_effort(state, trace_id).await;
                empty(204)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        _ => method_not_allowed(trace_id),
    }
}

// ===========================================================================
// Tenants
// ===========================================================================

pub(super) async fn tenant_collection(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    trace_id: &str,
) -> Resp {
    if method == "GET" {
        match crate::db::list_tenants(&state.pool).await {
            Ok(rows) => ok_json(200, &rows),
            Err(e) => db_err_resp(e, trace_id),
        }
    } else if method == "POST" {
        let body = read_body(session).await;
        let mut t: Tenant = match parse_body(&body, trace_id) {
            Ok(t) => t,
            Err(r) => return r,
        };
        // auth_url is mandatory (design §11.1): empty ⇒ 400 (NOT NULL only
        // catches SQL NULL, not the empty string).
        if t.auth_url.trim().is_empty() {
            return err_json(
                400,
                "missing_required_field",
                "auth_url is required",
                trace_id,
            );
        }
        if t.id.is_empty() {
            t.id = gen_id();
        }
        let ts = now_ts();
        if t.created_at.is_empty() {
            t.created_at = ts.clone();
        }
        if t.updated_at.is_empty() {
            t.updated_at = ts;
        }
        match crate::db::insert_tenant(&state.pool, &t).await {
            Ok(()) => {}
            Err(e) => return db_err_resp(e, trace_id),
        }
        reload_best_effort(state, trace_id).await;
        ok_json(201, &t)
    } else {
        method_not_allowed(trace_id)
    }
}

pub(super) async fn tenant_item(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    id: &str,
    trace_id: &str,
) -> Resp {
    match method {
        "GET" => match crate::db::get_tenant(&state.pool, id).await {
            Ok(t) => ok_json(200, &t),
            Err(e) if is_not_found(&e) => err_json(404, "not_found", "tenant not found", trace_id),
            Err(e) => db_err_resp(e, trace_id),
        },
        "PUT" => {
            let body = read_body(session).await;
            let mut t: Tenant = match parse_body(&body, trace_id) {
                Ok(t) => t,
                Err(r) => return r,
            };
            t.id = id.to_string();
            t.updated_at = now_ts();
            match crate::db::update_tenant(&state.pool, &t).await {
                Ok(()) => {}
                Err(e) => return db_err_resp(e, trace_id),
            }
            match crate::db::get_tenant(&state.pool, id).await {
                Ok(t) => {
                    reload_best_effort(state, trace_id).await;
                    ok_json(200, &t)
                }
                Err(_) => err_json(404, "not_found", "tenant not found", trace_id),
            }
        }
        "DELETE" => match crate::db::delete_tenant(&state.pool, id).await {
            Ok(()) => {
                reload_best_effort(state, trace_id).await;
                empty(204)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        _ => method_not_allowed(trace_id),
    }
}

// ===========================================================================
// Tenant providers / tenant models (no dedicated update fn → upsert on PUT)
// ===========================================================================

pub(super) async fn tenant_provider_collection(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    trace_id: &str,
) -> Resp {
    if method == "GET" {
        match crate::db::list_tenant_providers(&state.pool).await {
            Ok(rows) => ok_json(200, &rows),
            Err(e) => db_err_resp(e, trace_id),
        }
    } else if method == "POST" {
        let body = read_body(session).await;
        let mut tp: TenantProvider = match parse_body(&body, trace_id) {
            Ok(tp) => tp,
            Err(r) => return r,
        };
        if tp.id.is_empty() {
            tp.id = gen_id();
        }
        match crate::db::insert_tenant_provider(&state.pool, &tp).await {
            Ok(()) => {}
            Err(e) => return db_err_resp(e, trace_id),
        }
        reload_best_effort(state, trace_id).await;
        ok_json(201, &tp)
    } else {
        method_not_allowed(trace_id)
    }
}

pub(super) async fn tenant_provider_item(
    state: &AdminState,
    method: &str,
    id: &str,
    trace_id: &str,
) -> Resp {
    match method {
        "GET" => match crate::db::get_tenant_provider(&state.pool, id).await {
            Ok(tp) => ok_json(200, &tp),
            Err(e) if is_not_found(&e) => {
                err_json(404, "not_found", "tenant_provider not found", trace_id)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        "DELETE" => match crate::db::delete_tenant_provider(&state.pool, id).await {
            Ok(()) => {
                reload_best_effort(state, trace_id).await;
                empty(204)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        _ => method_not_allowed(trace_id),
    }
}

pub(super) async fn tenant_model_collection(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    trace_id: &str,
) -> Resp {
    if method == "GET" {
        match crate::db::list_tenant_models(&state.pool).await {
            Ok(rows) => ok_json(200, &rows),
            Err(e) => db_err_resp(e, trace_id),
        }
    } else if method == "POST" {
        let body = read_body(session).await;
        let mut tm: TenantModel = match parse_body(&body, trace_id) {
            Ok(tm) => tm,
            Err(r) => return r,
        };
        if tm.id.is_empty() {
            tm.id = gen_id();
        }
        match crate::db::insert_tenant_model(&state.pool, &tm).await {
            Ok(()) => {}
            Err(e) => return db_err_resp(e, trace_id),
        }
        reload_best_effort(state, trace_id).await;
        ok_json(201, &tm)
    } else {
        method_not_allowed(trace_id)
    }
}

pub(super) async fn tenant_model_item(
    state: &AdminState,
    method: &str,
    id: &str,
    trace_id: &str,
) -> Resp {
    match method {
        "GET" => match crate::db::get_tenant_model(&state.pool, id).await {
            Ok(tm) => ok_json(200, &tm),
            Err(e) if is_not_found(&e) => {
                err_json(404, "not_found", "tenant_model not found", trace_id)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        "DELETE" => match crate::db::delete_tenant_model(&state.pool, id).await {
            Ok(()) => {
                reload_best_effort(state, trace_id).await;
                empty(204)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        _ => method_not_allowed(trace_id),
    }
}

// ===========================================================================
// Limit roles
// ===========================================================================

pub(super) async fn limit_role_collection(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    trace_id: &str,
) -> Resp {
    if method == "GET" {
        match crate::db::list_limit_roles(&state.pool).await {
            Ok(rows) => ok_json(200, &rows),
            Err(e) => db_err_resp(e, trace_id),
        }
    } else if method == "POST" {
        let body = read_body(session).await;
        let mut r: LimitRole = match parse_body(&body, trace_id) {
            Ok(r) => r,
            Err(resp) => return resp,
        };
        if r.id.is_empty() {
            r.id = gen_id();
        }
        if r.created_at.is_empty() {
            r.created_at = now_ts();
        }
        match crate::db::insert_limit_role(&state.pool, &r).await {
            Ok(()) => {}
            Err(e) => return db_err_resp(e, trace_id),
        }
        reload_best_effort(state, trace_id).await;
        ok_json(201, &r)
    } else {
        method_not_allowed(trace_id)
    }
}

pub(super) async fn limit_role_item(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    id: &str,
    trace_id: &str,
) -> Resp {
    match method {
        "GET" => match crate::db::get_limit_role(&state.pool, id).await {
            Ok(r) => ok_json(200, &r),
            Err(e) if is_not_found(&e) => err_json(404, "not_found", "role not found", trace_id),
            Err(e) => db_err_resp(e, trace_id),
        },
        "PUT" => {
            let body = read_body(session).await;
            let mut r: LimitRole = match parse_body(&body, trace_id) {
                Ok(r) => r,
                Err(resp) => return resp,
            };
            r.id = id.to_string();
            match crate::db::update_limit_role(&state.pool, &r).await {
                Ok(()) => {}
                Err(e) => return db_err_resp(e, trace_id),
            }
            match crate::db::get_limit_role(&state.pool, id).await {
                Ok(r) => {
                    reload_best_effort(state, trace_id).await;
                    ok_json(200, &r)
                }
                Err(_) => err_json(404, "not_found", "role not found", trace_id),
            }
        }
        "DELETE" => match crate::db::delete_limit_role(&state.pool, id).await {
            Ok(()) => {
                reload_best_effort(state, trace_id).await;
                empty(204)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        _ => method_not_allowed(trace_id),
    }
}

// ===========================================================================
// Provider key bindings (design §7.1b)
// ===========================================================================

pub(super) async fn provider_key_binding_collection(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    trace_id: &str,
) -> Resp {
    if method == "GET" {
        match crate::db::list_provider_key_bindings(&state.pool).await {
            Ok(rows) => ok_json(200, &rows),
            Err(e) => db_err_resp(e, trace_id),
        }
    } else if method == "POST" {
        let body = read_body(session).await;
        let mut b: ProviderKeyBinding = match parse_body(&body, trace_id) {
            Ok(b) => b,
            Err(r) => return r,
        };
        if b.key_prefix.trim().is_empty() {
            return err_json(
                400,
                "empty_key_prefix",
                "key_prefix must be a non-empty string",
                trace_id,
            );
        }
        if b.id.is_empty() {
            b.id = gen_id();
        }
        let ts = now_ts();
        if b.created_at.is_empty() {
            b.created_at = ts.clone();
        }
        if b.updated_at.is_empty() {
            b.updated_at = ts;
        }
        match crate::db::insert_provider_key_binding(&state.pool, &b).await {
            Ok(()) => {}
            Err(e) => return db_err_resp(e, trace_id),
        }
        reload_best_effort(state, trace_id).await;
        ok_json(201, &b)
    } else {
        method_not_allowed(trace_id)
    }
}

pub(super) async fn provider_key_binding_item(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    id: &str,
    trace_id: &str,
) -> Resp {
    match method {
        "GET" => match crate::db::get_provider_key_binding(&state.pool, id).await {
            Ok(b) => ok_json(200, &b),
            Err(e) if is_not_found(&e) => {
                err_json(404, "not_found", "provider_key_binding not found", trace_id)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        "PUT" => {
            let body = read_body(session).await;
            let mut b: ProviderKeyBinding = match parse_body(&body, trace_id) {
                Ok(b) => b,
                Err(resp) => return resp,
            };
            if b.key_prefix.trim().is_empty() {
                return err_json(
                    400,
                    "empty_key_prefix",
                    "key_prefix must be a non-empty string",
                    trace_id,
                );
            }
            b.id = id.to_string();
            b.updated_at = now_ts();
            match crate::db::update_provider_key_binding(&state.pool, &b).await {
                Ok(()) => {}
                Err(e) => return db_err_resp(e, trace_id),
            }
            match crate::db::get_provider_key_binding(&state.pool, id).await {
                Ok(b) => {
                    reload_best_effort(state, trace_id).await;
                    ok_json(200, &b)
                }
                Err(_) => err_json(404, "not_found", "provider_key_binding not found", trace_id),
            }
        }
        "DELETE" => match crate::db::delete_provider_key_binding(&state.pool, id).await {
            Ok(()) => {
                reload_best_effort(state, trace_id).await;
                empty(204)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        _ => method_not_allowed(trace_id),
    }
}

// ===========================================================================
// Auth cache invalidation (design §11.7 / §13.2)
// ===========================================================================

#[derive(Deserialize)]
struct InvalidateRequest {
    tenant_id: Option<String>,
    api_keys: Option<Vec<String>>,
}

#[derive(Serialize)]
struct InvalidateResponse {
    invalidated: usize,
    tenant_id: Option<String>,
}

pub(super) async fn auth_cache_invalidate(
    state: &AdminState,
    session: &mut ServerSession,
    trace_id: &str,
) -> Resp {
    let body = read_body(session).await;
    let req: InvalidateRequest = match parse_body(&body, trace_id) {
        Ok(r) => r,
        Err(r) => return r,
    };
    let count = match (req.tenant_id.as_deref(), req.api_keys.as_deref()) {
        (Some(tid), Some(keys)) => state.auth.invalidate(tid, keys),
        (Some(tid), None) => state.auth.invalidate_tenant(tid),
        (None, Some(keys)) => {
            // No tenant: invalidate by api_key across every known tenant
            // (design §11.7 "跨租户匹配").
            let snap = state.store.snapshot();
            let mut total = 0usize;
            for t in snap.tenants_by_domain.values() {
                total += state.auth.invalidate(&t.id, keys);
            }
            total
        }
        (None, None) => 0,
    };
    // Refresh the cache-size gauge after mutation.
    metrics::record_auth_cache_size(state.auth.cache().len());
    ok_json(
        200,
        &InvalidateResponse {
            invalidated: count,
            tenant_id: req.tenant_id,
        },
    )
}

// ===========================================================================
// Breaker inspect / reset (design §8.4 / §13.2)
// ===========================================================================

#[derive(Serialize)]
struct BreakerList {
    dead: Vec<String>,
}

#[derive(Serialize)]
struct BreakerReset {
    reset: String,
    was_dead: bool,
    dead: Vec<String>,
}

pub(super) fn breaker_list(state: &AdminState) -> Resp {
    ok_json(
        200,
        &BreakerList {
            dead: state.breaker.dead_providers(),
        },
    )
}

pub(super) fn breaker_reset(state: &AdminState, id: &str) -> Resp {
    let was_dead = state.breaker.is_dead(id);
    state.breaker.on_success(id);
    ok_json(
        200,
        &BreakerReset {
            reset: id.to_string(),
            was_dead,
            dead: state.breaker.dead_providers(),
        },
    )
}

// ===========================================================================
// Concurrency admission snapshot (design §10 / §13.2)
// ===========================================================================

#[derive(Serialize)]
struct ConcurrencyList {
    providers: Vec<crate::proxy::admission::ProviderConcurrencyStatus>,
}

pub(super) fn concurrency_collection(state: &AdminState) -> Resp {
    ok_json(
        200,
        &ConcurrencyList {
            providers: state.admission.snapshot(),
        },
    )
}

// ===========================================================================
// Health / reload (design §13.2)
// ===========================================================================

#[derive(Serialize)]
struct HealthBody {
    status: &'static str,
    db: &'static str,
    breaker_dead: usize,
    tenants: usize,
    providers: usize,
}

#[derive(Serialize)]
struct ReloadBody {
    status: &'static str,
    tenants: usize,
    providers: usize,
    models: usize,
    keys: usize,
    certs: usize,
}

pub(super) async fn health(state: &AdminState, trace_id: &str) -> Resp {
    let snap = state.store.snapshot();
    let (db_status, providers_count) = match crate::db::list_providers(&state.pool).await {
        Ok(rows) => ("ok", rows.len()),
        Err(e) => {
            tracing::warn!(target: "hydra::admin", trace_id, error = %e, "health db probe failed");
            ("error", snap.providers.len())
        }
    };
    ok_json(
        200,
        &HealthBody {
            status: "ok",
            db: db_status,
            breaker_dead: state.breaker.dead_providers().len(),
            tenants: snap.tenants_by_domain.len(),
            providers: providers_count,
        },
    )
}

pub(super) async fn reload(state: &AdminState, trace_id: &str) -> Resp {
    // Explicit reload shares the same best-effort path (reload_all + cert
    // resolve), but a fatal validation failure is reported as 400 (design
    // §5.3: the old snapshot is retained).
    let result = {
        let _guard = state.reload_lock.lock().await;
        state.store.reload_all().await
    };
    if let Err(e) = result {
        return err_json(
            400,
            "reload_failed",
            &format!("config reload failed (old snapshot retained): {e}"),
            trace_id,
        );
    }
    // Cert-reload contract (W4b).
    if let Some(reload_certs) = state.cert_reloader.as_ref() {
        reload_certs();
    }
    let snap = state.store.snapshot();
    ok_json(
        200,
        &ReloadBody {
            status: "reloaded",
            tenants: snap.tenants_by_domain.len(),
            providers: snap.providers.len(),
            models: snap.models_by_key.len(),
            keys: snap.provider_keys.len(),
            certs: snap.certs.len(),
        },
    )
}

// ===========================================================================
// Metrics (§17) — self-hosted exposition
// ===========================================================================

pub(super) fn metrics_endpoint() -> Resp {
    let body = metrics::render();
    Response::builder()
        .status(200)
        .header("content-type", "text/plain; version=0.0.4; charset=utf-8")
        .header("content-length", body.len().to_string())
        .body(body.into_bytes())
        .unwrap_or_else(|_| Response::new(vec![]))
}

// ===========================================================================
// Shared small helpers
// ===========================================================================

fn db_err_resp(e: sqlx::Error, trace_id: &str) -> Resp {
    let (status, code) = classify_db_err(&e);
    err_json(status, code, &e.to_string(), trace_id)
}

fn method_not_allowed(trace_id: &str) -> Resp {
    err_json(
        405,
        "method_not_allowed",
        "HTTP method not allowed for this resource",
        trace_id,
    )
}

// A marker so `Arc` import stays used even when no handler needs it directly.
#[allow(dead_code)]
fn _arc_marker(_: Arc<()>) {}
