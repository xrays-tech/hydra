//! Admin REST handlers — the business logic behind each route (design §13.2).
//!
//! Every handler takes a borrowed [`AdminState`] and the live Pingora
//! [`ServerSession`] (so it can read the request body when needed) and returns a
//! complete `http::Response<Vec<u8>>`. The router in [`super`] dispatches to
//! these after the admin-token gate. No internal logic is mocked: each handler
//! drives the real `db::repo`, `ConfigStore`, `CircuitBreaker`
//! and `HydraCertStore`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use http::Response;
use hydra_core::auth::sha256_hex;
use hydra_core::model::{
    GlobalRole, LimitRole, Provider, ProviderKey, ProviderKeyDto, ProviderModel, Tenant,
    TenantAccessToken, TenantModel, TenantProvider, User, UserGroup,
};
use pingora_core::protocols::http::ServerSession;
use serde::{Deserialize, Serialize};
// `Engine` and `RngCore` look unused: both are METHOD PROVIDERS — `Engine` for
// `.encode` on the base64url alphabet, `RngCore` for `.fill_bytes` on the CSPRNG.
// The deps are on the `db` feature, which `proxy` (and therefore this module)
// always implies.
use base64::Engine;
use rand::RngCore;

use super::AdminState;
use crate::admin::metrics;

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

/// `err_json` plus `Retry-After`, for a rate-limited answer (the admin-plane
/// counterpart of the tenant API's `respond_json_with_retry_after`).
pub(super) fn err_json_throttled(
    status: u16,
    code: &str,
    message: &str,
    retry_after_secs: u64,
    trace_id: &str,
) -> Resp {
    let body = ErrorBody {
        error: ErrorDetail {
            code: code.to_string(),
            message: message.to_string(),
            trace_id: trace_id.to_string(),
        },
    };
    let mut resp = json_response(status, to_json(&body));
    resp.headers_mut().insert(
        http::header::RETRY_AFTER,
        http::HeaderValue::from_str(&retry_after_secs.to_string())
            .unwrap_or_else(|_| http::HeaderValue::from_static("60")),
    );
    resp
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
///
/// `pub(super)` because the tenant-config write core answers through it too: this
/// mapping must have exactly one owner, and a hand-copied twin had already drifted
/// (it lost the SQLITE_BUSY → 503 `storage_busy` branch).
pub(crate) fn classify_db_err(e: &sqlx::Error) -> (u16, &'static str) {
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
        // SQLite lock contention (SQLITE_BUSY = 5, SQLITE_BUSY_SNAPSHOT = 517):
        // the writer could not get or keep the write lock inside `busy_timeout`.
        // This is TRANSIENT — the same request usually succeeds immediately — so
        // reporting it as 500 (an internal error) told clients "there is a bug"
        // and made them retry at human speed, when what they need is a short
        // backoff. All write transactions now take the lock up front via
        // `db::begin_write`, so this path is the residual case (a lock held past
        // the timeout by a long transaction).
        if matches!(db.code().as_deref(), Some("5") | Some("517")) {
            return (503, "storage_busy");
        }
    }
    (500, "database_error")
}

/// True when the sqlx error is "row not found" (→ 404).
fn is_not_found(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::RowNotFound)
}

// ---------------------------------------------------------------------------
// Shared reload (write-after consistency, design §13.2/§12.1)
// ---------------------------------------------------------------------------

/// Reload the in-memory snapshot. Cert re-resolution needs no call here: the
/// cert store is registered as a `ConfigStore` snapshot-change hook, so the swap
/// itself re-resolves certs for the next TLS handshake. Serialised by the
/// per-state mutex so concurrent writes don't race (design §6 risk note).
/// Best-effort: a fatal validation failure is logged but does **not** fail an
/// already-committed write (design §5.3 keeps the old snapshot; the next
/// successful reload recovers).
/// The validation warnings that belong to ONE limit role, for the write response (decision D-17).
///
/// The warnings were already produced at write time — `reload_all` validates and logs them — but a
/// scripted operator sees only `201` and has to go read the leader's log to learn that, say, the role
/// it just created can never match. `ValidationIssue::subject` makes them filterable; issues with no
/// subject are about the configuration as a whole and are deliberately NOT returned here (returning
/// them would make every write look noisy and train operators to ignore the field).
fn warnings_for(state: &AdminState, subject: &str) -> Vec<String> {
    hydra_core::config::validate(&state.store.snapshot())
        .into_iter()
        .filter(|i| i.subject.as_deref() == Some(subject))
        .map(|i| i.message)
        .collect()
}

/// P3-4 / D-16 second half (2026-10-09): the admin API must not echo a
/// recoverable client key. `limit_role.matching_key` used to be the LAST
/// plaintext-echo exception — provider keys never are (`?reveal=1` is a no-op
/// since P1-5). The digest form (`sha256:` + hex, decision D-16③) is already
/// unrecoverable and is echoed as-is (it is the form operators are told to
/// use); every other value — a raw key, or a mask — is masked, so nothing
/// carryable off an admin response or the UI can reconstruct the original.
fn masked_matching_key(v: &str) -> String {
    if hydra_core::limit::is_key_digest(v) {
        v.to_string()
    } else {
        hydra_core::rewrite::mask_key(v)
    }
}

/// A limit role plus its warnings, as the write endpoints answer. `warnings` is ALWAYS present
/// (possibly empty) so a script can read it without sniffing for the key.
fn limit_role_body(state: &AdminState, role: &LimitRole) -> serde_json::Value {
    let mut body = serde_json::to_value(role).unwrap_or(serde_json::Value::Null);
    if let serde_json::Value::Object(map) = &mut body {
        if let Some(k) = role.matching_key.as_deref() {
            map.insert(
                "matching_key".to_string(),
                serde_json::json!(masked_matching_key(k)),
            );
        }
        map.insert(
            "warnings".to_string(),
            serde_json::json!(warnings_for(state, &role.id)),
        );
    }
    body
}

async fn reload_best_effort(state: &AdminState, trace_id: &str) -> Option<Resp> {
    let _guard = state.reload_lock.lock().await;
    if let Err(e) = state.store.reload_all().await {
        // ADR-0001 D-9/T3.2: the local commit succeeded but the config did NOT reach the cluster
        // (Arachne refused to encode or the head could not be committed). This is NOT the
        // best-effort case below — the response has to say "local only" or an operator will
        // believe a change is fleet-wide when exactly one node has it.
        if let crate::store::StoreError::NotPublished { reason } = &e {
            tracing::error!(
                target: "hydra::admin",
                trace_id, %reason,
                "post-write PUBLISH FAILED: the local SQLite transaction committed and this node \
                 serves the new config, but the cluster does not have it"
            );
            note_reload_outcome(state, false);
            return Some(err_json(
                503,
                "config_not_published",
                &format!(
                    "the change was committed to this node's database but could NOT be published \
                     to the cluster, so no other node will see it: {reason}"
                ),
                trace_id,
            ));
        }
        // ERROR, not WARN (audit §3.14): the write was committed, but the
        // runtime is now permanently serving the PREVIOUS snapshot — every later
        // write (key rotation, revocation, tenant disable) will also fail to
        // take effect. That must be impossible to miss in logs and alertable
        // from /metrics.
        tracing::error!(
            target: "hydra::admin",
            trace_id, error = %e,
            "post-write reload_all FAILED: the in-memory config snapshot is now STALE              (design §5.3 keeps the old snapshot; admin writes will keep returning              2xx while having no runtime effect until a reload succeeds)"
        );
        note_reload_outcome(state, true);
        return None;
    }
    note_reload_outcome(state, false);
    None
}

/// Record the outcome of a config reload — **the single owner** of
/// `hydra_config_snapshot_stale` and of `AdminState::snapshot_stale`.
///
/// Why it exists: the post-write path ([`reload_best_effort`]) recorded the outcome, and the
/// explicit `POST /api/v1/reload` endpoint (`admin::cluster_api::reload`) did **not** — it shared
/// the loading but not the recording. Measured 2026-09-30 (`integration/test_snapshot_stale.py`):
/// a failing explicit reload answered `400 reload_failed` ("old snapshot retained") while the
/// documented gauge stayed **0**, so the alert the docs point operators at never fired for it; and
/// after a later successful explicit reload the gauge stayed **1**, so the alert kept firing after
/// the documented recovery — while `ops.md` §9.1 tells an operator to alert on exactly that gauge
/// and §5.3 to recover by reloading.
pub(super) fn note_reload_outcome(state: &AdminState, stale: bool) {
    metrics::record_config_snapshot_stale(stale);
    state
        .snapshot_stale
        .store(stale, std::sync::atomic::Ordering::Release);
}

/// Write-boundary mirror of the loader's **fatal** endpoint check
/// (`store::is_usable_endpoint`). Without it a typo such as
/// `{"endpoint":"api.openai.com"}` (missing scheme) was persisted with a 201;
/// from then on every `reload_all` failed fatal validation, the in-memory
/// snapshot stayed frozen at the old version and **every later write silently
/// stopped taking effect** — including key rotation and revocation — while the
/// API kept answering 201/200 (audit §3.14). `None` means "acceptable".
fn endpoint_error(p: &Provider, trace_id: &str) -> Option<Resp> {
    if crate::store::is_usable_endpoint(&p.endpoint) {
        return None;
    }
    Some(err_json(
        400,
        "invalid_endpoint",
        &format!(
            "provider endpoint {:?} is not a usable URL: it must start with \
             http:// or https:// and be followed by a host",
            p.endpoint
        ),
        trace_id,
    ))
}

// ---------------------------------------------------------------------------
// id/timestamp helpers (server-generated when the client omits them)
// ---------------------------------------------------------------------------

// `pub(crate)` because the tenant-API row handlers (`tenant_api::handlers`, plan
// T5) stamp `created_at`/`updated_at` for the user hierarchy, and those columns'
// DDL default is SQLite `datetime('now')` — this is the ONE wall-clock formatter
// that matches it. A second copy is how two faces of one column start holding
// two formats.
pub(crate) fn now_ts() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// The value of a single query parameter from a raw query string (`None` when
/// the key is absent).
fn query_param<'a>(query: Option<&'a str>, key: &str) -> Option<&'a str> {
    query.and_then(|q| {
        q.split('&').find_map(|kv| {
            let mut it = kv.splitn(2, '=');
            match (it.next(), it.next()) {
                (Some(k), Some(v)) if k == key => Some(v),
                _ => None,
            }
        })
    })
}

/// A fresh opaque id (`id-<nanos>-<len(thread id)>`).
///
/// `pub(crate)` because the data-plane write paths need it too: they moved out of
/// the module that used to own the internal endpoint, and this is the ONE id generator — a second
/// copy is how two faces of the same write start minting different shapes of id.
pub(crate) fn gen_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Nanoseconds alone are NOT unique: concurrent writers can read the same
    // clock tick, and the old suffix (`thread id LENGTH`) was identical for every
    // small ThreadId. Measured under 4-way user creation: 12 rows were silently
    // overwritten by peers (4-user chunks), then key creation answered 404.
    // Randomness makes the id unique ACROSS nodes too (ids are global keys);
    // the atomic sequence keeps even same-tick, same-random calls distinct.
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let rand = rand::random::<u64>();
    format!("id-{nanos:x}-{rand:016x}-{seq:x}")
}

/// Read the full request body into a vec (empty for bodyless requests).
/// Hard cap on an admin request body (review N4).
///
/// The proxy path has had one for a long time (`max_request_body_hard`, 413);
/// the admin service had NONE, so a single request — including one carrying
/// only a *tenant* token — could make the process buffer an arbitrary amount of
/// memory before anything was even parsed. 1 MiB is ~1000x the largest
/// legitimate admin body (a JSON config entity).
pub(super) const MAX_ADMIN_BODY_BYTES: usize = 1024 * 1024;

/// Read the whole request body, refusing anything over
/// [`MAX_ADMIN_BODY_BYTES`] with `413 request_body_too_large`.
///
/// The remainder of an oversized body is drained first, so the connection
/// stays usable for the next request instead of desynchronizing the stream.
///
/// `Err` carries the ready-made 413 `Resp` (`Resp` is ~336 bytes, same
/// `result_large_err` allowance as [`parse_body`]); the `Ok` path stays cheap.
#[allow(clippy::result_large_err)]
pub(super) async fn read_body(
    session: &mut ServerSession,
    trace_id: &str,
) -> Result<Vec<u8>, Resp> {
    let mut buf = Vec::new();
    while let Ok(Some(chunk)) = session.read_request_body().await {
        buf.extend_from_slice(&chunk);
        if buf.len() > MAX_ADMIN_BODY_BYTES {
            session.set_keepalive(None);
            let _ = session.drain_request_body().await;
            return Err(err_json(
                413,
                "request_body_too_large",
                &format!("request body exceeds {MAX_ADMIN_BODY_BYTES} bytes"),
                trace_id,
            ));
        }
    }
    Ok(buf)
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
        match crate::db::list_providers(state.db()).await {
            Ok(rows) => ok_json(200, &rows),
            Err(e) => db_err_resp(e, trace_id),
        }
    } else if method == "POST" {
        let body = match read_body(session, trace_id).await {
            Ok(b) => b,
            Err(r) => return r,
        };
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
        if let Some(r) = endpoint_error(&p, trace_id) {
            return r;
        }
        match crate::db::insert_provider(state.db(), &p).await {
            Ok(()) => {}
            Err(e) => return db_err_resp(e, trace_id),
        }
        if let Some(r) = reload_best_effort(state, trace_id).await {
            return r;
        }
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
        "GET" => match crate::db::get_provider(state.db(), id).await {
            Ok(p) => ok_json(200, &p),
            Err(e) if is_not_found(&e) => {
                err_json(404, "not_found", "provider not found", trace_id)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        "PUT" => {
            let body = match read_body(session, trace_id).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let mut p: Provider = match parse_body(&body, trace_id) {
                Ok(p) => p,
                Err(r) => return r,
            };
            p.id = id.to_string();
            p.updated_at = now_ts();
            if let Some(r) = endpoint_error(&p, trace_id) {
                return r;
            }
            match crate::db::update_provider(state.db(), &p).await {
                Ok(()) => {}
                Err(e) => return db_err_resp(e, trace_id),
            }
            match crate::db::get_provider(state.db(), id).await {
                Ok(p) => {
                    if let Some(r) = reload_best_effort(state, trace_id).await {
                        return r;
                    }
                    ok_json(200, &p)
                }
                Err(_) => err_json(404, "not_found", "provider not found", trace_id),
            }
        }
        "DELETE" => match crate::db::delete_provider(state.db(), id).await {
            Ok(()) => {
                if let Some(r) = reload_best_effort(state, trace_id).await {
                    return r;
                }
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
        match crate::db::list_provider_models(state.db()).await {
            Ok(rows) => ok_json(200, &rows),
            Err(e) => db_err_resp(e, trace_id),
        }
    } else if method == "POST" {
        let body = match read_body(session, trace_id).await {
            Ok(b) => b,
            Err(r) => return r,
        };
        let m: ProviderModel = match parse_body(&body, trace_id) {
            Ok(m) => m,
            Err(r) => return r,
        };
        match crate::db::insert_provider_model(state.db(), &m).await {
            Ok(()) => {}
            Err(e) => return db_err_resp(e, trace_id),
        }
        if let Some(r) = reload_best_effort(state, trace_id).await {
            return r;
        }
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
        "GET" => match crate::db::get_provider_model(state.db(), id).await {
            Ok(m) => ok_json(200, &m),
            Err(e) if is_not_found(&e) => err_json(404, "not_found", "model not found", trace_id),
            Err(e) => db_err_resp(e, trace_id),
        },
        "PUT" => {
            let body = match read_body(session, trace_id).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let mut m: ProviderModel = match parse_body(&body, trace_id) {
                Ok(m) => m,
                Err(r) => return r,
            };
            m.id = id.to_string();
            match crate::db::update_provider_model(state.db(), &m).await {
                Ok(()) => {}
                Err(e) => return db_err_resp(e, trace_id),
            }
            match crate::db::get_provider_model(state.db(), id).await {
                Ok(m) => {
                    if let Some(r) = reload_best_effort(state, trace_id).await {
                        return r;
                    }
                    ok_json(200, &m)
                }
                Err(_) => err_json(404, "not_found", "model not found", trace_id),
            }
        }
        "DELETE" => match crate::db::delete_provider_model(state.db(), id).await {
            Ok(()) => {
                if let Some(r) = reload_best_effort(state, trace_id).await {
                    return r;
                }
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
        match crate::db::list_provider_keys(state.db(), state.key_provider.as_ref()).await {
            Ok(rows) => {
                let out: Vec<ProviderKeyDto> = rows
                    .into_iter()
                    .map(|k| {
                        let masked = hydra_core::rewrite::mask_key(&k.api_key);
                        ProviderKeyDto {
                            id: k.id,
                            provider_id: k.provider_id,
                            api_key: masked,
                            created_at: k.created_at,
                        }
                    })
                    .collect();
                ok_json(200, &out)
            }
            Err(e) => db_err_resp(e, trace_id),
        }
    } else if method == "POST" {
        let body = match read_body(session, trace_id).await {
            Ok(b) => b,
            Err(r) => return r,
        };
        let mut k: ProviderKey = match parse_body(&body, trace_id) {
            Ok(k) => k,
            Err(r) => return r,
        };
        // A provider key must carry a real credential: an empty value would be
        // stored as an authoritative-but-useless key, so every request routed to
        // that provider would fail upstream with an empty credential (review A3).
        if k.api_key.trim().is_empty() {
            return err_json(
                400,
                "invalid_api_key",
                "api_key must not be empty",
                trace_id,
            );
        }
        if k.id.is_empty() {
            k.id = gen_id();
        }
        if k.created_at.is_empty() {
            k.created_at = now_ts();
        }
        match crate::db::insert_provider_key(state.db(), state.key_provider.as_ref(), &k).await {
            Ok(()) => {}
            Err(e) => return db_err_resp(e, trace_id),
        }
        if let Some(r) = reload_best_effort(state, trace_id).await {
            return r;
        }
        // Never echo plaintext back (P1-5) — re-expose only via the masked DTO.
        let masked = hydra_core::rewrite::mask_key(&k.api_key);
        let dto = ProviderKeyDto {
            id: k.id,
            provider_id: k.provider_id,
            api_key: masked,
            created_at: k.created_at,
        };
        ok_json(201, &dto)
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
            match crate::db::get_provider_key(state.db(), state.key_provider.as_ref(), id).await {
                Ok(k) => {
                    let masked = hydra_core::rewrite::mask_key(&k.api_key);
                    let dto = ProviderKeyDto {
                        id: k.id,
                        provider_id: k.provider_id,
                        api_key: masked,
                        created_at: k.created_at,
                    };
                    ok_json(200, &dto)
                }
                Err(e) if is_not_found(&e) => err_json(404, "not_found", "key not found", trace_id),
                Err(e) => db_err_resp(e, trace_id),
            }
        }
        "PUT" => {
            let body = match read_body(session, trace_id).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let mut k: ProviderKey = match parse_body(&body, trace_id) {
                Ok(k) => k,
                Err(r) => return r,
            };
            // PUT is an OVERWRITE (upsert), so an empty credential must never be
            // accepted: it would silently destroy the working key that was there
            // before (review A3). Rotation means supplying a new non-empty key.
            if k.api_key.trim().is_empty() {
                return err_json(
                    400,
                    "invalid_api_key",
                    "api_key must not be empty",
                    trace_id,
                );
            }
            k.id = id.to_string();
            if k.created_at.is_empty() {
                k.created_at = now_ts();
            }
            // Atomic (audit §3.15): delete + insert inside ONE transaction, so a
            // failing insert (e.g. an unknown provider_id hitting the FK) rolls
            // the delete back and the previously working key is NOT lost.
            match crate::db::upsert_provider_key(state.db(), state.key_provider.as_ref(), &k).await
            {
                Ok(()) => {}
                Err(e) => return db_err_resp(e, trace_id),
            }
            if let Some(r) = reload_best_effort(state, trace_id).await {
                return r;
            }
            let masked = hydra_core::rewrite::mask_key(&k.api_key);
            let dto = ProviderKeyDto {
                id: k.id,
                provider_id: k.provider_id,
                api_key: masked,
                created_at: k.created_at,
            };
            ok_json(200, &dto)
        }
        "DELETE" => match crate::db::delete_provider_key(state.db(), id).await {
            Ok(()) => {
                if let Some(r) = reload_best_effort(state, trace_id).await {
                    return r;
                }
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
// Tenant CRUD (design §13.2)
// ===========================================================================

/// Tenant create/update body: the existing [`Tenant`] fields (legacy cert
/// paths included, kept for read compatibility) plus the migration-0007 PEM
/// certificate content fields and the migration-0009 self-service access
/// token. The private key PEM and the access token are consumed here and
/// never echoed back in responses.
#[derive(Deserialize)]
struct TenantUpsert {
    #[serde(flatten)]
    tenant: Tenant,
    /// Public cert PEM (content mode, primary). `Some("")` clears the cert.
    #[serde(default)]
    cert_pem: Option<String>,
    /// Private key PEM (content mode, primary). Required when `cert_pem` set.
    #[serde(default)]
    cert_key_pem: Option<String>,
}

/// Tenant response view: the entity plus the snapshot staleness flag.
#[derive(Serialize)]
struct TenantView {
    #[serde(flatten)]
    tenant: Tenant,
    /// The write IS committed; this only says the post-write reload failed, so
    /// the running config still shows the previous value. Process-level flag —
    /// see `AdminState::snapshot_stale`.
    snapshot_stale: bool,
}

impl TenantView {
    fn new(tenant: Tenant, snapshot_stale: bool) -> Self {
        Self {
            tenant,
            snapshot_stale,
        }
    }

    /// Build from live admin state (the common case).
    fn from_state(state: &AdminState, tenant: Tenant) -> Self {
        Self::new(
            tenant,
            state
                .snapshot_stale
                .load(std::sync::atomic::Ordering::Acquire),
        )
    }
}

/// SHA-256 of `s` as a lowercase hex string (the `_hex`-suffixed core
/// helper returns the raw `[u8; 32]` — the suffix is historical).
fn sha256_hex_str(s: &str) -> String {
    sha256_hex(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Constant-time byte comparison (no timing side-channel on the token).
///
/// `pub(super)` so the admin service's own gates (`cluster token` on
/// `/api/v1/internal/*`) share the same primitive instead of using `==`.
pub(crate) fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Certificate write resolved from an upsert body (see
/// [`resolve_tenant_cert_write`]).
enum CertWrite {
    /// No cert change (no PEM content and no usable legacy paths).
    None,
    /// Explicit removal (`cert_pem: ""`).
    Clear,
    /// Persist this PEM content (content mode, or legacy paths converted).
    Content {
        cert_pem: String,
        cert_key_pem: String,
    },
}

/// Certificate write resolved from an upsert body — PURE input validation
/// (no DB access), so a 4xx is returned BEFORE the tenant row is persisted
/// and can never leave a "reported as failed but actually created" tenant
/// behind (the empty-string legacy-path bug: the admin UI sends `""` for a
/// blank optional `cert_file`/`cert_key`, which was treated as a real path).
///
/// Rules:
/// - `cert_pem` non-empty → store content (seal the key); missing `cert_key_pem` → 400;
/// - `cert_pem` empty string → explicit removal (clear the cert columns);
/// - `cert_key_pem` without `cert_pem` → 400;
/// - neither given → legacy `cert_file`/`cert_key` paths: convert by
///   reading the files **on this node**; unreadable → 400 with a hint to
///   switch to PEM content. Empty strings are NOT paths (skip).
// `Resp` is the admin-wide `http::Response<Vec<u8>>` (large by design, shared
// by every handler); clippy's `result_large_err` (default-warn since 1.98)
// fires on the unit-Err shape here — boxed responses would ripple through the
// whole admin layer for no benefit.
#[allow(clippy::result_large_err)]
fn resolve_tenant_cert_write(
    cert_pem: &Option<String>,
    cert_key_pem: &Option<String>,
    cert_file: Option<&str>,
    cert_key: Option<&str>,
    trace_id: &str,
) -> Result<CertWrite, Resp> {
    let pem_raw = cert_pem.as_deref();
    let key_raw = cert_key_pem.as_deref();
    let pem = pem_raw.map(str::trim);
    let key = key_raw.map(str::trim);
    match (pem, key) {
        // Explicit removal: `cert_pem: ""`.
        (Some(""), _) => Ok(CertWrite::Clear),
        // Content mode: non-empty `cert_pem` (+ required `cert_key_pem`).
        // Note: the trim above is only for the emptiness check; the stored
        // content is the RAW body (a trailing newline is PEM-normal and must
        // round-trip untouched).
        (Some(_), Some(k)) if !k.is_empty() => Ok(CertWrite::Content {
            cert_pem: pem_raw.unwrap_or_default().to_string(),
            cert_key_pem: key_raw.unwrap_or_default().to_string(),
        }),
        (Some(_), Some(_)) => Err(err_json(
            400,
            "missing_required_field",
            "cert_key_pem is required when cert_pem is set",
            trace_id,
        )),
        (Some(_), None) => Err(err_json(
            400,
            "missing_required_field",
            "cert_key_pem is required when cert_pem is set",
            trace_id,
        )),
        (None, Some(_)) => Err(err_json(
            400,
            "missing_required_field",
            "cert_pem is required when cert_key_pem is set",
            trace_id,
        )),
        (None, None) => {
            // Legacy path form: convert by reading the files on this node.
            // Empty strings are NOT paths (a blank optional field arrives as
            // "" from the admin UI) — convert only when BOTH sides carry a
            // non-empty path; otherwise no cert change.
            let cf = cert_file.map(str::trim).filter(|s| !s.is_empty());
            let ck = cert_key.map(str::trim).filter(|s| !s.is_empty());
            match (cf, ck) {
                (Some(cert_path), Some(key_path)) => {
                    match (std::fs::read(cert_path), std::fs::read(key_path)) {
                        (Ok(cert_bytes), Ok(key_bytes)) => Ok(CertWrite::Content {
                            cert_pem: String::from_utf8_lossy(&cert_bytes).into_owned(),
                            cert_key_pem: String::from_utf8_lossy(&key_bytes).into_owned(),
                        }),
                        _ => Err(err_json(
                            400,
                            "cert_file_unreadable",
                            "cert_file/cert_key paths given but not readable on this node; \
                             provide cert_pem/cert_key_pem content instead",
                            trace_id,
                        )),
                    }
                }
                _ => Ok(CertWrite::None),
            }
        }
    }
}

/// The two optional secret writes a tenant write may carry:
/// Certificate write that [`crate::db::write_tenant`] applies inside its
/// transaction.
#[allow(clippy::result_large_err)]
fn resolved_cert_write(action: CertWrite) -> Option<(Option<String>, Option<String>)> {
    match action {
        CertWrite::None => None,
        CertWrite::Clear => Some((None, None)),
        CertWrite::Content {
            cert_pem,
            cert_key_pem,
        } => Some((Some(cert_pem), Some(cert_key_pem))),
    }
}

pub(super) async fn tenant_collection(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    trace_id: &str,
) -> Resp {
    if method == "GET" {
        match crate::db::list_tenants(state.db()).await {
            Ok(rows) => {
                let views: Vec<TenantView> = rows
                    .into_iter()
                    .map(|t| TenantView::from_state(state, t))
                    .collect();
                ok_json(200, &views)
            }
            Err(e) => db_err_resp(e, trace_id),
        }
    } else if method == "POST" {
        let body = match read_body(session, trace_id).await {
            Ok(b) => b,
            Err(r) => return r,
        };
        let up: TenantUpsert = match parse_body(&body, trace_id) {
            Ok(u) => u,
            Err(r) => return r,
        };
        let mut t = up.tenant;
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
        // Certificate inputs are validated BEFORE the row is written so a 4xx
        // can never leave a "reported as failed but actually created" tenant
        // behind; the resolved cert is applied after insert (migration 0007:
        // the DB becomes self-contained).
        let cert_action = match resolve_tenant_cert_write(
            &up.cert_pem,
            &up.cert_key_pem,
            t.cert_file.as_deref(),
            t.cert_key.as_deref(),
            trace_id,
        ) {
            Ok(a) => a,
            Err(r) => return r,
        };
        // ONE transaction: the tenant row and its certificate commit together or
        // not at all (audit §2.2 G-P1). Access tokens live in their own table
        // and have their own write path.
        let cert = resolved_cert_write(cert_action);
        if let Err(e) = crate::db::write_tenant(
            state.db(),
            state.key_provider.as_ref(),
            crate::db::TenantWrite {
                tenant: &t,
                is_create: true,
                cert,
            },
        )
        .await
        {
            return db_err_resp(e, trace_id);
        }
        if let Some(r) = reload_best_effort(state, trace_id).await {
            return r;
        }
        ok_json(201, &TenantView::from_state(state, t))
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
        "GET" => match crate::db::get_tenant(state.db(), id).await {
            Ok(t) => ok_json(200, &TenantView::from_state(state, t)),
            Err(e) if is_not_found(&e) => err_json(404, "not_found", "tenant not found", trace_id),
            Err(e) => db_err_resp(e, trace_id),
        },
        "PUT" => {
            let body = match read_body(session, trace_id).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let up: TenantUpsert = match parse_body(&body, trace_id) {
                Ok(u) => u,
                Err(r) => return r,
            };
            let mut t = up.tenant;
            t.id = id.to_string();
            t.updated_at = now_ts();
            // Certificate inputs validated BEFORE the update (same rule as
            // POST); applied after the row write (migration 0007).
            let cert_action = match resolve_tenant_cert_write(
                &up.cert_pem,
                &up.cert_key_pem,
                t.cert_file.as_deref(),
                t.cert_key.as_deref(),
                trace_id,
            ) {
                Ok(a) => a,
                Err(r) => return r,
            };
            let cert = resolved_cert_write(cert_action);
            if let Err(e) = crate::db::write_tenant(
                state.db(),
                state.key_provider.as_ref(),
                crate::db::TenantWrite {
                    tenant: &t,
                    is_create: false,
                    cert,
                },
            )
            .await
            {
                return db_err_resp(e, trace_id);
            }
            match crate::db::get_tenant(state.db(), id).await {
                Ok(t) => {
                    if let Some(r) = reload_best_effort(state, trace_id).await {
                        return r;
                    }
                    ok_json(200, &TenantView::from_state(state, t))
                }
                Err(e) if is_not_found(&e) => {
                    err_json(404, "not_found", "tenant not found", trace_id)
                }
                Err(e) => db_err_resp(e, trace_id),
            }
        }
        "DELETE" => match crate::db::delete_tenant(state.db(), id).await {
            Ok(()) => {
                if let Some(r) = reload_best_effort(state, trace_id).await {
                    return r;
                }
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
        match crate::db::list_tenant_providers(state.db()).await {
            Ok(rows) => ok_json(200, &rows),
            Err(e) => db_err_resp(e, trace_id),
        }
    } else if method == "POST" {
        let body = match read_body(session, trace_id).await {
            Ok(b) => b,
            Err(r) => return r,
        };
        let mut tp: TenantProvider = match parse_body(&body, trace_id) {
            Ok(tp) => tp,
            Err(r) => return r,
        };
        if tp.id.is_empty() {
            tp.id = gen_id();
        }
        match crate::db::insert_tenant_provider(state.db(), &tp).await {
            Ok(()) => {}
            Err(e) => return db_err_resp(e, trace_id),
        }
        if let Some(r) = reload_best_effort(state, trace_id).await {
            return r;
        }
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
        "GET" => match crate::db::get_tenant_provider(state.db(), id).await {
            Ok(tp) => ok_json(200, &tp),
            Err(e) if is_not_found(&e) => {
                err_json(404, "not_found", "tenant_provider not found", trace_id)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        "DELETE" => match crate::db::delete_tenant_provider(state.db(), id).await {
            Ok(()) => {
                if let Some(r) = reload_best_effort(state, trace_id).await {
                    return r;
                }
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
        match crate::db::list_tenant_models(state.db()).await {
            Ok(rows) => ok_json(200, &rows),
            Err(e) => db_err_resp(e, trace_id),
        }
    } else if method == "POST" {
        let body = match read_body(session, trace_id).await {
            Ok(b) => b,
            Err(r) => return r,
        };
        let mut tm: TenantModel = match parse_body(&body, trace_id) {
            Ok(tm) => tm,
            Err(r) => return r,
        };
        if tm.id.is_empty() {
            tm.id = gen_id();
        }
        match crate::db::insert_tenant_model(state.db(), &tm).await {
            Ok(()) => {}
            Err(e) => return db_err_resp(e, trace_id),
        }
        if let Some(r) = reload_best_effort(state, trace_id).await {
            return r;
        }
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
        "GET" => match crate::db::get_tenant_model(state.db(), id).await {
            Ok(tm) => ok_json(200, &tm),
            Err(e) if is_not_found(&e) => {
                err_json(404, "not_found", "tenant_model not found", trace_id)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        "DELETE" => match crate::db::delete_tenant_model(state.db(), id).await {
            Ok(()) => {
                if let Some(r) = reload_best_effort(state, trace_id).await {
                    return r;
                }
                empty(204)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        _ => method_not_allowed(trace_id),
    }
}

// ===========================================================================
// Tenant model catalog (design-tenant-model-catalog §2.3, P1)
// ===========================================================================

/// One configured provider behind a catalog model, with its CURRENT online
/// status (a live runtime probe, not config).
#[derive(Serialize)]
struct CatalogProvider {
    provider_id: String,
    /// True when the provider can currently route traffic for this model:
    /// circuit-breaker open? no — weight > 0 (soft-enabled) and ≥ 1 api-key
    /// present. False ⇒ listed for diagnosis but unroutable right now.
    online: bool,
}

/// One model in the tenant's catalog with every configured provider behind it.
#[derive(Serialize)]
struct CatalogModelEntry {
    model: String,
    providers: Vec<CatalogProvider>,
}

/// `GET /api/v1/tenants/{tenant_id}/models` response body.
#[derive(Serialize)]
struct TenantModelCatalog {
    tenant_id: String,
    models: Vec<CatalogModelEntry>,
}

/// Read-only aggregate: every model a tenant may route, across ALL providers
/// that serve it (design-tenant-model-catalog §2.3).
///
/// **View calibre** — the CONFIG full-set view (management/ops side): same
/// gates as the data-plane catalog (hydra_core::router::accessible_models)
/// — tenant_models whitelist (default-open), tenant_providers ∩ models_by_key,
/// and the cfg.providers existence guard (orphan references dropped; never a
/// bare index into cfg.providers) — but it does **not** apply the runtime
/// online filter: a dead / weight<=0 / keyless provider is STILL listed, only
/// flagged online:false. The data-plane GET /v1/models (and accessible_models)
/// return the runtime online view; this endpoint exists so operators can see
/// the full configured set and diagnose WHY a model is currently unroutable.
/// The view is keyless, so this key-scoped no-op mirrors router::accessible_models. A
/// model is listed as long as it keeps ≥ 1 configured AND existing
/// (cfg.providers-present) authorised provider.
///
/// Tenant existence: ConfigData has NO tenant-id index (only
/// tenants_by_domain) ⇒ scan the domain index's values for the id
/// (domain is mandatory, exactly one row per tenant) — the presence of a
/// tenant_providers entry is NOT a valid existence probe (a tenant with no
/// grants is a valid tenant and returns 200 with models: []).
///
/// Deterministic output: models ascending, providers ascending per model.
pub(super) async fn tenant_model_catalog(
    state: &AdminState,
    tenant_id: &str,
    trace_id: &str,
) -> Resp {
    let snap = state.store.snapshot();

    // 404 for an unknown tenant before any enumeration (see calibre note).
    let tenant_exists = snap.tenants_by_domain.values().any(|t| t.id == tenant_id);
    if !tenant_exists {
        return err_json(404, "not_found", "tenant not found", trace_id);
    }

    // A tenant without any provider grants has an empty catalog — still 200
    // (the chat path would 403 TenantForbidden; catalog semantics = no rows).
    let Some(authorized) = snap.tenant_providers.get(tenant_id) else {
        return ok_json(
            200,
            &TenantModelCatalog {
                tenant_id: tenant_id.to_string(),
                models: Vec::new(),
            },
        );
    };

    let mut models: Vec<CatalogModelEntry> = Vec::new();
    for (model_key, serving) in &snap.models_by_key {
        // Gate 1 — tenant_models whitelist (default-open): with a mapping, a
        // model outside it is skipped entirely.
        if let Some(allowed) = snap.tenant_models.get(tenant_id) {
            if !allowed.contains(model_key) {
                continue;
            }
        }
        // Gates 2+3 — serving providers ∩ tenant-authorised providers.
        let mut providers: Vec<CatalogProvider> = Vec::new();
        for row in serving {
            let pid = &row.provider_id;
            if !authorized.contains(pid) {
                continue;
            }
            // Existence guard — never a bare index into cfg.providers: a
            // serving row referencing a provider missing from the snapshot
            // (orphan; config::validate only Warns) is silently dropped.
            let Some(p) = snap.providers.get(pid) else {
                continue;
            };
            // online is a live probe, NOT the static filter: dead / weight<=0 /
            // keyless providers stay listed (for diagnosis) with online=false.
            // weight is read from the provider snapshot reference obtained by
            // the existence guard above (Provider.weight), never bare-indexed.
            // `weight > 0` = routable by weight (0 = deliberate soft-disable,
            // < 0 = invalid; both are "not online" for the catalog).
            let online = !state.breaker.is_dead(pid)
                && p.weight > 0
                && snap
                    .provider_keys
                    .get(pid)
                    .is_some_and(|keys| !keys.is_empty());
            providers.push(CatalogProvider {
                provider_id: pid.clone(),
                online,
            });
        }
        // Deterministic per-model order (provider_id ascending). DB
        // UNIQUE(key, provider_id) makes duplicates impossible at load time.
        providers.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));
        if !providers.is_empty() {
            models.push(CatalogModelEntry {
                model: model_key.clone(),
                providers,
            });
        }
    }
    // Deterministic overall order (model ascending).
    models.sort_by(|a, b| a.model.cmp(&b.model));
    ok_json(
        200,
        &TenantModelCatalog {
            tenant_id: tenant_id.to_string(),
            models,
        },
    )
}

// ===========================================================================
// Limit roles
// ===========================================================================

/// Decision D-11 (2026-10-09): the `matching_provider` dimension is REFUSED at the admin write
/// boundary — `POST /limit-roles` and `PUT /limit-roles/:id` both answer 400 for it.
///
/// The dimension can never match: the pre-gate builds its `MatchCtx` BEFORE routing, so
/// `provider` is always `None` there, while `limit::dim_matches` requires equality with the
/// configured value — a role that declares it is skipped by BOTH the count and the token check
/// (its limits never apply) while remaining listed, persisted and shown in the admin UI. The old
/// behavior (201 + a Warn in the node log) was SILENT: a script never reads the log, so the
/// operator believed a provider was capped when nothing was enforced. The write boundary is the
/// one place EVERY writer is caught; `config::validate` keeps its Warn for rows the boundary
/// cannot see (legacy databases, file-loaded configs, restores).
fn matching_provider_write_error(provider: &str, trace_id: &str) -> Resp {
    err_json(
        400,
        "matching_provider_cannot_match",
        &format!(
            "matching_provider '{}' CANNOT match: the limit pre-gate runs before routing, so \
             no provider is known yet when roles are matched — a role with this dimension is \
             skipped entirely (its limits never apply). Set matching_provider to null \
             (decision D-11, 2026-10-09)",
            provider
        ),
        trace_id,
    )
}

/// A limit role with `matching_key` masked for READ responses (P3-4 / D-16
/// second half). Clones the role and replaces `matching_key` with the masked /
/// digest-preserved form — never the recoverable value.
fn masked_limit_role(role: &LimitRole) -> LimitRole {
    let mut out = role.clone();
    if let Some(k) = role.matching_key.as_deref() {
        out.matching_key = Some(masked_matching_key(k));
    }
    out
}

pub(super) async fn limit_role_collection(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    trace_id: &str,
) -> Resp {
    if method == "GET" {
        match crate::db::list_limit_roles(state.db(), state.key_provider.as_ref()).await {
            Ok(rows) => {
                let masked: Vec<LimitRole> = rows.iter().map(masked_limit_role).collect();
                ok_json(200, &masked)
            }
            Err(e) => db_err_resp(e, trace_id),
        }
    } else if method == "POST" {
        let body = match read_body(session, trace_id).await {
            Ok(b) => b,
            Err(r) => return r,
        };
        let mut r: LimitRole = match parse_body(&body, trace_id) {
            Ok(r) => r,
            Err(resp) => return resp,
        };
        // D-11 (2026-10-09): refuse the dead dimension BEFORE anything is persisted.
        if let Some(provider) = r.matching_provider.as_deref() {
            return matching_provider_write_error(provider, trace_id);
        }
        if r.id.is_empty() {
            r.id = gen_id();
        }
        if r.created_at.is_empty() {
            r.created_at = now_ts();
        }
        match crate::db::insert_limit_role(state.db(), state.key_provider.as_ref(), &r).await {
            Ok(()) => {}
            Err(e) => return db_err_resp(e, trace_id),
        }
        if let Some(r) = reload_best_effort(state, trace_id).await {
            return r;
        }
        ok_json(201, &limit_role_body(state, &r))
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
        "GET" => match crate::db::get_limit_role(state.db(), state.key_provider.as_ref(), id).await
        {
            Ok(r) => ok_json(200, &masked_limit_role(&r)),
            Err(e) if is_not_found(&e) => err_json(404, "not_found", "role not found", trace_id),
            Err(e) => db_err_resp(e, trace_id),
        },
        "PUT" => {
            let body = match read_body(session, trace_id).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let mut r: LimitRole = match parse_body(&body, trace_id) {
                Ok(r) => r,
                Err(resp) => return resp,
            };
            // D-11 (2026-10-09): refuse the dead dimension BEFORE anything is persisted.
            if let Some(provider) = r.matching_provider.as_deref() {
                return matching_provider_write_error(provider, trace_id);
            }
            r.id = id.to_string();
            match crate::db::update_limit_role(state.db(), state.key_provider.as_ref(), &r).await {
                Ok(()) => {}
                Err(e) => return db_err_resp(e, trace_id),
            }
            match crate::db::get_limit_role(state.db(), state.key_provider.as_ref(), id).await {
                Ok(r) => {
                    if let Some(r) = reload_best_effort(state, trace_id).await {
                        return r;
                    }
                    ok_json(200, &limit_role_body(state, &r))
                }
                Err(_) => err_json(404, "not_found", "role not found", trace_id),
            }
        }
        "DELETE" => match crate::db::delete_limit_role(state.db(), id).await {
            Ok(()) => {
                if let Some(r) = reload_best_effort(state, trace_id).await {
                    return r;
                }
                empty(204)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        _ => method_not_allowed(trace_id),
    }
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
    // The configured side comes from the LIVE snapshot (not from what the last request
    // happened to observe), so the response always contrasts "what the configuration asks
    // for" with "what this gate is enforcing". Since 2026-10-09 a gate RESIZES on
    // hot-reload (decision item D-14 — the next acquire swaps in a new generation), so
    // `limits_stale=true` means "a configuration change has been made but no request has
    // applied it yet" (or the provider is no longer in the config); reporting only the
    // enforced value would make a configuration change look applied before it reaches the
    // gate.
    let cfg = state.store.snapshot();
    let providers = state.admission.snapshot_with_configured(|provider_id| {
        // `cfg.providers` is keyed by provider id.
        cfg.providers.get(provider_id).map(|p| {
            hydra_core::config::resolve_policy(
                p.max_concurrency,
                p.max_queue_depth,
                p.queue_wait_timeout_ms,
                state.default_concurrency_policy,
            )
        })
    });
    ok_json(200, &ConcurrencyList { providers })
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
    /// What this node is actually serving (审核四 P4). `null` when the process
    /// did not publish a listener set (e.g. an `AdminService` built directly by
    /// a test), never "assume plaintext".
    listeners: Option<&'static crate::listeners::ActiveListeners>,
}

pub(super) async fn health(state: &AdminState, trace_id: &str) -> Resp {
    let snap = state.store.snapshot();
    // Every node has a local database, so the probe always runs. Until 2026-10-05 a `None` pool
    // short-circuited it to `db: "n/a"` — a status an operator could only ever see on the retired
    // `edge` role, and one that hid a database which really was broken (the probe is how a
    // corrupted or unreachable DB shows up on `/health` at all).
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
            listeners: crate::listeners::active(),
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

/// `GET /api/v1/stats/usage` — aggregated usage statistics for the Admin UI
/// Stats page: request counts + token totals (prompt / completion) broken down
/// by tenant and by provider, derived live from the prometheus counters
/// (`hydra_requests_total` / `hydra_tokens_total`). Cumulative since process
/// start; token-gated like every other `/api/v1/*` route.
pub(super) fn stats_usage() -> Resp {
    ok_json(200, &metrics::usage_aggregate())
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

// ===========================================================================
// Global Roles (plan §3.4: the five-dimensional rule + its provider SET)
// ===========================================================================

/// What `PUT /api/v1/global-roles` accepts.
///
/// EVERY field is optional except that `name` must end up NON-empty — checked in the
/// validator, so the answer is `400 invalid_role` rather than a serde parse error.
/// A MISSING (or empty) dimension means the `*` WILDCARD (decision D-2), which is
/// exactly what a hand-written "grant these providers, everywhere" body means.
///
/// `id` absent ⇒ CREATE, minted by [`gen_id`]. `id` present ⇒ UPDATE, and the row's
/// `created_at` is PRESERVED (the SQL `ON CONFLICT` arm never touches it — see
/// `db::upsert_global_role_on_conn`); `updated_at` moves, which is the operator's
/// audit signal that the rule was rewritten rather than created.
#[derive(Debug, Deserialize)]
struct GlobalRoleWrite {
    id: Option<String>,
    name: Option<String>,
    tenant_id: Option<String>,
    user_group_id: Option<String>,
    user_id: Option<String>,
    api_key_id: Option<String>,
    model_key: Option<String>,
    /// The rule's provider set. `provider_ids` is accepted as an ALIAS so a client
    /// can round-trip what `GET /api/v1/global-roles` handed it.
    #[serde(default, alias = "provider_ids")]
    providers: Vec<String>,
    /// Absent ⇒ ENABLED. A create that silently landed DISABLED would match NOTHING
    /// (the loader drops disabled rules ENTIRELY, unlike the identity rows) and would
    /// read as a broken router rather than as an intentional disable.
    enabled: Option<bool>,
}

/// A dimension that was not sent, or sent empty/whitespace, is the `*` WILDCARD.
fn dim_or_any(value: Option<&str>) -> String {
    match value.map(|v| v.trim()).filter(|v| !v.is_empty()) {
        Some(v) => v.to_string(),
        None => "*".to_string(),
    }
}

/// The provider ids AS the DB will store them: trimmed, de-duplicated, ascending —
/// `replace_global_role_providers_on_conn` normalizes the same way, so the response
/// body reports what a later GET returns rather than what was typed.
fn normalize_providers(providers: &[String]) -> Vec<String> {
    let mut out: Vec<String> = providers
        .iter()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// The CURRENT hierarchy + provider ids as ONE lookup snapshot, for Global Role
/// write validation (plan §3.1 write-time invariants — the load-time backstop for the
/// same rules lives in `hydra_core::config::validate`).
///
/// Read from the LIVE tables incl. `enabled = false` rows: a rule pinned to a DISABLED
/// group is LEGITIMATE (the group exists, and it answers 403 `group_disabled` at auth
/// time), so existence must NOT depend on the `enabled` flag.
struct RoleDims {
    tenants: HashSet<String>,
    /// `user_group_id` -> owning tenant id.
    groups: HashMap<String, String>,
    /// `user_id` -> (owning tenant id, owning group id).
    users: HashMap<String, (String, String)>,
    /// `api_key_id` -> owning user id.
    keys: HashMap<String, String>,
    providers: HashSet<String>,
}

#[allow(clippy::result_large_err)]
async fn load_role_dims(state: &AdminState, trace_id: &str) -> Result<RoleDims, Resp> {
    let tenants = match crate::db::list_tenants(state.db()).await {
        Ok(rows) => rows,
        Err(e) => return Err(db_err_resp(e, trace_id)),
    };
    let groups = match crate::db::list_user_groups(state.db()).await {
        Ok(rows) => rows,
        Err(e) => return Err(db_err_resp(e, trace_id)),
    };
    let users = match crate::db::list_users(state.db()).await {
        Ok(rows) => rows,
        Err(e) => return Err(db_err_resp(e, trace_id)),
    };
    let keys = match crate::db::list_user_api_keys(state.db()).await {
        Ok(rows) => rows,
        Err(e) => return Err(db_err_resp(e, trace_id)),
    };
    let providers = match crate::db::list_providers(state.db()).await {
        Ok(rows) => rows,
        Err(e) => return Err(db_err_resp(e, trace_id)),
    };

    Ok(RoleDims {
        tenants: tenants.iter().map(|t| t.id.clone()).collect(),
        groups: groups
            .iter()
            .map(|g| (g.id.clone(), g.tenant_id.clone()))
            .collect(),
        users: users
            .iter()
            .map(|u| (u.id.clone(), (u.tenant_id.clone(), u.user_group_id.clone())))
            .collect(),
        keys: keys
            .iter()
            .map(|k| (k.id.clone(), k.user_id.clone()))
            .collect(),
        providers: providers.iter().map(|p| p.id.clone()).collect(),
    })
}

/// The rule's TENANT anchor, updated by ONE pinned row: `owner` is the tenant that
/// row belongs to. A set anchor must not disagree; an UNSET anchor means the
/// caller adopts `owner`. `None` = consistent.
///
/// Comparing every pinned row to this single anchor is what implements the
/// "same tenant" requirement: it is transitive, so `group` in `t_1` + `user` in
/// `t_2` is rejected even when the rule's own `tenant_id` is the `*` wildcard.
#[must_use]
fn tenant_conflict(subject: &str, owner: &str, anchor: &Option<String>) -> Option<String> {
    match anchor {
        Some(a) if *a != owner => Some(format!(
            "{subject} belongs to tenant '{owner}', not to the rule's tenant '{a}'"
        )),
        _ => None,
    }
}

/// The write-boundary check for a Global Role: `Some(reason)` ⇒ `400 invalid_role`.
///
/// - `name` must not be empty.
/// - The provider SET may not contain `*` — the wildcard is RESERVED for the rule
///   dimensions, and `global_role_provider.provider_id` is an FK to `provider(id)`,
///   so a `*` (or any unknown id) would be a constraint error at best; catching it
///   here turns it into a readable answer. Duplicate ids are NOT an error
///   (`replace_global_role_providers_*` de-duplicates), and an EMPTY set is NOT an
///   error either (the rule matches and contributes nothing — plan §3.4).
/// - Every PINNED identity dimension (`tenant` / `user_group` / `user` / `api_key`)
///   must name a row that EXISTS and must agree with every other pinned row along
///   the chain `api_key` -> `user` -> `user_group` -> `tenant`.
/// - `model_key` may be concrete: legitimate (pre-provisioning, or a rule scoped to
///   one model), hence WARN — see [`global_role_write_warnings`].
#[must_use]
fn global_role_write_error(role: &GlobalRole, dims: &RoleDims) -> Option<String> {
    if role.name.trim().is_empty() {
        return Some("name must not be empty".to_string());
    }

    // --- provider SET ---------------------------------------------------------
    for provider_id in &role.provider_ids {
        // A String (not a &str): `dims.providers` is a `HashSet<String>`, and
        // `HashSet<String>::contains` wants a `&String`.
        let trimmed = provider_id.trim().to_string();
        if trimmed.is_empty() {
            return Some("a provider id must not be empty".to_string());
        }
        if trimmed == "*" {
            return Some(
                "provider id \"*\" is not accepted: the wildcard is reserved for the \
                 rule dimensions, so name the provider you mean"
                    .to_string(),
            );
        }
        if !dims.providers.contains(&trimmed) {
            return Some(format!("unknown provider id '{}'", trimmed));
        }
    }

    // --- dimensions -----------------------------------------------------------
    let mut anchor: Option<String> = None;

    if role.tenant_id != "*" {
        if !dims.tenants.contains(&role.tenant_id) {
            return Some(format!("unknown tenant_id '{}'", role.tenant_id));
        }
        anchor = Some(role.tenant_id.clone());
    }

    if role.user_group_id != "*" {
        let owner = match dims.groups.get(&role.user_group_id) {
            Some(v) => v.clone(),
            None => return Some(format!("unknown user_group_id '{}'", role.user_group_id)),
        };
        if let Some(reason) = tenant_conflict("user_group_id", &owner, &anchor) {
            return Some(reason);
        }
        if anchor.is_none() {
            anchor = Some(owner);
        }
    }

    if role.user_id != "*" {
        let entry = match dims.users.get(&role.user_id) {
            Some(v) => v,
            None => return Some(format!("unknown user_id '{}'", role.user_id)),
        };
        let (owner_tenant, owner_group) = (entry.0.clone(), entry.1.clone());
        if let Some(reason) = tenant_conflict("user_id", &owner_tenant, &anchor) {
            return Some(reason);
        }
        if anchor.is_none() {
            anchor = Some(owner_tenant);
        }
        if role.user_group_id != "*" && role.user_group_id != owner_group {
            return Some(format!(
                "user_id '{}' belongs to user_group_id '{}', not to the rule's user_group_id '{}'",
                role.user_id, owner_group, role.user_group_id
            ));
        }
    }

    if role.api_key_id != "*" {
        let owner_user = match dims.keys.get(&role.api_key_id) {
            Some(v) => v.clone(),
            None => return Some(format!("unknown api_key_id '{}'", role.api_key_id)),
        };
        if role.user_id != "*" && role.user_id != owner_user {
            return Some(format!(
                "api_key_id '{}' belongs to user_id '{}', not to the rule's user_id '{}'",
                role.api_key_id, owner_user, role.user_id
            ));
        }
        // The key's TENANT and GROUP are its OWNER's, so the chain is followed one
        // hop further. A key whose owner row is gone is an FK violation the schema
        // should prevent; it is rejected HERE too (fail closed) because such a rule
        // could never match a principal anyway.
        let entry = match dims.users.get(&owner_user) {
            Some(v) => v,
            None => {
                return Some(format!(
                    "api_key_id '{}' has no resolvable owning user ('{}')",
                    role.api_key_id, owner_user
                ))
            }
        };
        let (owner_tenant, owner_group) = (entry.0.clone(), entry.1.clone());
        if let Some(reason) = tenant_conflict("api_key_id", &owner_tenant, &anchor) {
            return Some(reason);
        }
        // NO anchor adoption HERE: `api_key` is the LAST dimension checked, so nothing
        // reads the anchor after this point (an assignment would be dead code).
        if role.user_group_id != "*" && role.user_group_id != owner_group {
            return Some(format!(
                "api_key_id '{}' belongs to user_group_id '{}', not to the rule's user_group_id '{}'",
                role.api_key_id, owner_group, role.user_group_id
            ));
        }
    }

    None
}

/// WARN-level notes for an ACCEPTED rule (pure, so the contract is testable): they
/// go into the response's `warnings` array AND the log.
///
/// A CONCRETE `model_key` is legal — pre-provisioning a rule before the model comes
/// online is plan §3.1's "允许预配置", and scoping a rule to one model is the
/// point of the dimension — but it is also the dimension that silently narrows a
/// broad-looking rule to requests carrying exactly that model. The load-time
/// `config::validate` additionally WARNs when NO ONLINE provider offers it.
#[must_use]
fn global_role_write_warnings(role: &GlobalRole) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if role.model_key != "*" {
        out.push(format!(
            "model_key '{}' is PINNED: allowed, but this rule can only match requests \
             carrying that model, and it routes at all only once an ONLINE provider \
             offers it",
            role.model_key,
        ));
    }
    out
}

/// The shared write core behind `PUT /api/v1/global-roles` (create-or-replace by the
/// body's `id`) and `PUT /api/v1/global-roles/{id}` (the PATH id WINS over a body
/// `id`: the path is what the operator addressed, and a body/path disagreement must
/// not silently create a SECOND rule).
///
/// Order of operations, and why:
/// 1. read + parse the body (`400 invalid_json` on garbage);
/// 2. read the CURRENT rules once — `created_at` must SURVIVE an UPDATE, and the
///    same read says whether this is a create (201) or an update (200);
/// 3. read [`RoleDims`] and validate BEFORE persisting anything: a rule that lands
///    is a rule that ROUTES, so a dangling or cross-tenant dimension has to be refused
///    rather than published and WARNed about later;
/// 4. WARN (log + response) about a pinned `model_key`;
/// 5. persist rule + provider set in ONE transaction
///    ([`crate::db::upsert_global_role_with_providers`]);
/// 6. [`reload_best_effort`] — a refused PUBLISH answers 503 `config_not_published`
///    (ADR-0001 D-9: the local commit succeeded, the cluster did NOT get it).
async fn global_role_put(
    state: &AdminState,
    session: &mut ServerSession,
    path_id: Option<String>,
    trace_id: &str,
) -> Resp {
    let body = match read_body(session, trace_id).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let req: GlobalRoleWrite = match parse_body(&body, trace_id) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    let id = match path_id {
        Some(v) => v,
        None => match req.id {
            Some(v) => v,
            None => gen_id(),
        },
    };

    let existing: Vec<GlobalRole> = match crate::db::list_global_roles(state.db()).await {
        Ok(rows) => rows,
        Err(e) => return db_err_resp(e, trace_id),
    };
    let previous = existing.iter().find(|r| r.id == id);

    let dims: RoleDims = match load_role_dims(state, trace_id).await {
        Ok(d) => d,
        Err(r) => return r,
    };

    let role = GlobalRole {
        id,
        name: req.name.unwrap_or_default().trim().to_string(),
        tenant_id: dim_or_any(req.tenant_id.as_deref()),
        user_group_id: dim_or_any(req.user_group_id.as_deref()),
        user_id: dim_or_any(req.user_id.as_deref()),
        api_key_id: dim_or_any(req.api_key_id.as_deref()),
        model_key: dim_or_any(req.model_key.as_deref()),
        provider_ids: req.providers,
        enabled: req.enabled.unwrap_or(true),
        // PRESERVED on an UPDATE — see `db::upsert_global_role_on_conn`.
        created_at: match previous {
            Some(r) => r.created_at.clone(),
            None => now_ts(),
        },
        updated_at: now_ts(),
    };

    if let Some(reason) = global_role_write_error(&role, &dims) {
        return err_json(400, "invalid_role", &reason, trace_id);
    }

    let warnings = global_role_write_warnings(&role);
    for warning in &warnings {
        tracing::warn!(
            target: "hydra::admin",
            trace_id,
            role_id = %role.id,
            "global_role write: {warning}",
        );
    }

    // The set as it will be STORED (trimmed/dedup/ascending) — the response must
    // report what a later `GET` returns, not what was typed.
    let providers = normalize_providers(&role.provider_ids);
    match crate::db::upsert_global_role_with_providers(state.db(), &role, &providers).await {
        Ok(_) => {}
        Err(e) => return db_err_resp(e, trace_id),
    }

    if let Some(r) = reload_best_effort(state, trace_id).await {
        return r;
    }

    let mut out = serde_json::to_value(&role).unwrap_or(serde_json::Value::Null);
    if let serde_json::Value::Object(map) = &mut out {
        map.insert("provider_ids".to_string(), serde_json::json!(providers));
        // ALWAYS present (possibly empty), like the limit-role write bodies.
        map.insert("warnings".to_string(), serde_json::json!(warnings));
    }
    ok_json(if previous.is_some() { 200 } else { 201 }, &out)
}

/// `GET /api/v1/global-roles` + `PUT /api/v1/global-roles`.
///
/// The list answers EVERY rule incl. `enabled = false` ones (an operator managing
/// rules has to SEE the disabled ones — the loader's enabled-only filter is a
/// routing decision, not a management one), and both queries are `ORDER BY id` with
/// each rule's `provider_ids` ascending, so two nodes with the same data answer
/// identically (plan §3.4 determinism).
pub(super) async fn global_role_collection(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    trace_id: &str,
) -> Resp {
    if method == "GET" {
        match crate::db::list_global_roles(state.db()).await {
            Ok(rows) => ok_json(200, &rows),
            Err(e) => db_err_resp(e, trace_id),
        }
    } else if method == "PUT" {
        global_role_put(state, session, None, trace_id).await
    } else {
        method_not_allowed(trace_id)
    }
}

/// `GET` / `PUT` / `DELETE /api/v1/global-roles/{id}`.
///
/// `DELETE` is the ONLY mutation that has to distinguish "no such rule" from "removed
/// it": `delete_global_role` reports the row count, and 0 means the id was never
/// there ⇒ 404 (the provider child rows go with it through `ON DELETE CASCADE`).
pub(super) async fn global_role_item(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    id: &str,
    trace_id: &str,
) -> Resp {
    match method {
        "GET" => match crate::db::list_global_roles(state.db()).await {
            Ok(rows) => match rows.iter().find(|r| r.id == id) {
                Some(role) => ok_json(200, role),
                None => err_json(404, "not_found", "global role not found", trace_id),
            },
            Err(e) => db_err_resp(e, trace_id),
        },
        "PUT" => global_role_put(state, session, Some(id.to_string()), trace_id).await,
        "DELETE" => match crate::db::delete_global_role(state.db(), id).await {
            Ok(0) => err_json(404, "not_found", "global role not found", trace_id),
            Ok(_) => {
                if let Some(r) = reload_best_effort(state, trace_id).await {
                    return r;
                }
                empty(204)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        _ => method_not_allowed(trace_id),
    }
}

// ===========================================================================
// The user hierarchy, READ (plan §3.5 / the cascading global-role UI)
//
// These three paths exist so an operator can render `role → users → keys` and
// reconcile a cascade against what is actually stored. They are READ-ONLY by
// design: the WRITE half of this surface belongs to the tenant self-service API,
// whose auth is the TENANT's own credential and whose writes carry that tenant's
// scoping rules (`duplicate_name`, `group_not_empty`, the ceilings). An admin
// write path here would be a SECOND set of rules for one table, and the plan has
// no such thing.
// ===========================================================================

/// `GET /api/v1/user-groups[?tenant_id=]` — every group on this node.
///
/// Includes the DISABLED ones: a disabled group still has users in it, and a UI
/// that hides it would show those users pointing at an id that does not exist.
pub(super) async fn user_group_collection(
    state: &AdminState,
    method: &str,
    query: Option<&str>,
    trace_id: &str,
) -> Resp {
    if method == "GET" {
        match crate::db::list_user_groups(state.db()).await {
            Ok(rows) => {
                let by_tenant = query_param(query, "tenant_id");
                let mut out: Vec<UserGroup> = match by_tenant {
                    Some(tid) => rows.into_iter().filter(|g| g.tenant_id == tid).collect(),
                    None => rows,
                };
                // The DB orders this table by `(tenant_id, name)` (its UNIQUE key);
                // this surface orders by id so the three hierarchy reads share ONE
                // ordering rule and a diff of two calls is readable.
                out.sort_by(|a, b| a.id.cmp(&b.id));
                ok_json(200, &out)
            }
            Err(e) => db_err_resp(e, trace_id),
        }
    } else {
        method_not_allowed(trace_id)
    }
}

/// `GET /api/v1/users[?tenant_id=]` — every user on this node.
///
/// `billing_status` is included because it is the state a cascade explains: a
/// user in `arrears` stopped being billable, and a UI that cannot see that
/// cannot tell a cascade from an operator's manual suspension. The user rows
/// carry NO credential material (a user is not a secret; its keys are).
pub(super) async fn user_collection(
    state: &AdminState,
    method: &str,
    query: Option<&str>,
    trace_id: &str,
) -> Resp {
    if method == "GET" {
        match crate::db::list_users(state.db()).await {
            Ok(rows) => {
                let by_tenant = query_param(query, "tenant_id");
                let out: Vec<User> = match by_tenant {
                    Some(tid) => rows.into_iter().filter(|u| u.tenant_id == tid).collect(),
                    None => rows,
                };
                ok_json(200, &out)
            }
            Err(e) => db_err_resp(e, trace_id),
        }
    } else {
        method_not_allowed(trace_id)
    }
}

/// The admin view of a `user_api_key` row, WITHOUT `key_hash`.
///
/// A SEPARATE struct rather than serializing [`hydra_core::model::UserApiKey`]: the model carries the
/// SHA-256 of the plaintext, and the admin API has no use for it — echoing it
/// would put an offline-crackable digest of every credential on a plane whose
/// token is a single operator secret. Deriving this view is the structural
/// guarantee: there is no field here to forget to strip. Same shape as the
/// tenant-facing `ApiKeyRow`, plus `user_id`, because this list is node-wide.
#[derive(Serialize)]
struct UserApiKeyView {
    id: String,
    user_id: String,
    name: String,
    /// First 8 characters of the plaintext — recognisable, far too short to be
    /// worth guessing.
    key_prefix: String,
    /// Last 4 characters, for the same reason.
    key_last4: String,
    enabled: bool,
    /// Canonical RFC3339 UTC, or `None` = never expires.
    expires_at: Option<String>,
    created_at: String,
}

/// `GET /api/v1/user-api-keys[?tenant_id=]` — every key on this node, MASKED.
///
/// Includes revoked / expired keys on purpose: the identity index keeps them so
/// a stale credential answers 401 "known" instead of "unknown", and an operator
/// debugging a cascade needs to see the row that is still being presented.
pub(super) async fn user_api_key_collection(
    state: &AdminState,
    method: &str,
    query: Option<&str>,
    trace_id: &str,
) -> Resp {
    if method == "GET" {
        match crate::db::list_user_api_keys(state.db()).await {
            Ok(rows) => {
                // `?tenant_id=` resolves through the key's OWNER: a `user_api_key`
                // row names only `user_id`, so the tenant is whatever the owner user
                // belongs to. That is ONE extra read of the user table, taken ONLY
                // when the filter is actually present. A key whose owner is not in
                // the user table cannot be attributed to any tenant, so it drops
                // out of a FILTERED answer rather than pretending to match — and is
                // kept in an unfiltered one, because hiding a row the DB holds is
                // worse than showing one whose owner this node has not loaded.
                let wanted: HashSet<String> = match query_param(query, "tenant_id") {
                    None => HashSet::new(),
                    Some(tid) => match crate::db::list_users(state.db()).await {
                        Err(e) => return db_err_resp(e, trace_id),
                        Ok(users) => users
                            .iter()
                            .filter(|u| u.tenant_id == tid)
                            .map(|u| u.id.clone())
                            .collect(),
                    },
                };
                let filtering = query_param(query, "tenant_id").is_some();
                let out: Vec<UserApiKeyView> = rows
                    .into_iter()
                    .filter(|k| !filtering || wanted.contains(&k.user_id))
                    .map(|k| UserApiKeyView {
                        id: k.id,
                        user_id: k.user_id,
                        name: k.name,
                        key_prefix: k.key_prefix,
                        key_last4: k.key_last4,
                        enabled: k.enabled,
                        expires_at: k.expires_at,
                        created_at: k.created_at,
                    })
                    .collect();
                ok_json(200, &out)
            }
            Err(e) => db_err_resp(e, trace_id),
        }
    } else {
        method_not_allowed(trace_id)
    }
}

// ===========================================================================
// Tenant access tokens (plan §3.2 / T1 + T4a: MULTIPLE named tokens per tenant)
// ===========================================================================

/// What `POST /api/v1/tenants/{tenant_id}/access-tokens` accepts.
///
/// ONLY `name` and `expires_at` are settable. `id` is minted by [`gen_id`], the
/// PLAINTEXT is minted by the SERVER and echoed ONCE, and a create always ENABLES:
/// an endpoint that minted a DISABLED credential would read as a bug rather than
/// as intent, so disabling stays an explicit state change.
#[derive(Debug, Deserialize)]
struct AccessTokenWrite {
    name: Option<String>,
    /// Absent or empty ⇒ NEVER expires; otherwise normalized to the canonical
    /// RFC3339 UTC form (plan §3.2).
    expires_at: Option<String>,
}

/// The create response, and the ONE place the PLAINTEXT ever leaves the process.
///
/// `token_hash` is deliberately absent from EVERY body: the hash has no operator-
/// visible purpose (a token either works for its holder or it does not), and
/// echoing it would hand a reader of the response exactly the value the SHA-256
/// exists to protect.
#[derive(Serialize)]
struct AccessTokenCreated {
    id: String,
    name: String,
    /// Plaintext, ONCE, in this body only.
    token: String,
    token_prefix: String,
    token_last4: String,
    expires_at: Option<String>,
}

/// A stored token as the MANAGEMENT list shows it: enough to recognize the token,
/// never enough to USE it.
#[derive(Serialize)]
struct AccessTokenView {
    id: String,
    name: String,
    token_prefix: String,
    token_last4: String,
    enabled: bool,
    expires_at: Option<String>,
    created_at: String,
}

/// The list projection. A FREE function rather than an `Into` impl so the field
/// list has exactly one owner and the `token_hash` column cannot leak into a
/// response because someone reached for a convenient conversion.
#[must_use]
fn access_token_view(row: &TenantAccessToken) -> AccessTokenView {
    AccessTokenView {
        id: row.id.clone(),
        name: row.name.clone(),
        token_prefix: row.token_prefix.clone(),
        token_last4: row.token_last4.clone(),
        enabled: row.enabled,
        expires_at: row.expires_at.clone(),
        created_at: row.created_at.clone(),
    }
}

/// A fresh tenant access token plaintext: `ht_` + 32 CSPRNG bytes, base64url
/// WITHOUT padding (43 characters) — the tenant-token twin of the `hk_` api-key
/// mint in `tenant_api::handlers`, same shape, different prefix so an operator
/// can tell the two credential families apart from a log line alone.
///
/// `fill_bytes` comes from `rand`'s CSPRNG (`thread_rng`), never a seeded or
/// hash-based source: a credential whose space can be walked is not a credential.
/// The plaintext is returned to exactly ONE caller, in exactly one response body,
/// and is NEVER logged — what outlives this function is its SHA-256 digest plus
/// the two display atoms.
#[must_use]
fn new_access_token_plaintext() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(bytes.as_mut());
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    format!("ht_{encoded}")
}

/// The first `n` characters of a value, as its own string (a display atom).
///
/// Character-based rather than byte-sliced: this alphabet is ASCII, but a byte
/// slice on a non-ASCII string PANICS, and an atom helper should not be the thing
/// that discovers that.
#[must_use]
fn head_chars(value: &str, n: usize) -> String {
    value.chars().take(n).collect()
}

/// The last `n` characters of a value.
#[must_use]
fn tail_chars(value: &str, n: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= n {
        return value.to_string();
    }
    chars[chars.len() - n..].iter().collect()
}

/// "Now" in the canonical RFC3339 UTC form the identity-family rows use.
///
/// Deliberately NOT [`now_ts`], whose `%Y-%m-%d %H:%M:%S` shape would sort WRONG
/// against a `Z`-terminated expiry: `'T'` (0x54) is greater than `' '` (0x20), so
/// mixed spellings make an expiry test lie about WHEN, not about WHETHER.
#[must_use]
fn now_rfc3339() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// The create request's NAME rule, PURE (no DB, no HTTP): trimmed, and a name
/// that survives trimming EMPTY is refused.
///
/// `UNIQUE(tenant_id, name)` would happily store `""`, leaving the operator a
/// token they can only address by id — and the name is how this endpoint is
/// documented to address a token. Returns the normalized name, or the 400
/// `message`.
fn access_token_name_or_error(raw: Option<&str>) -> Result<String, String> {
    let value = raw.map(|v| v.trim()).filter(|v| !v.is_empty());
    match value {
        Some(v) => Ok(v.to_string()),
        None => Err("name must be a non-empty string".to_string()),
    }
}

/// Normalize the requested expiry to the canonical form, or say WHY it cannot be.
///
/// `None` input (or an empty/whitespace value) means "never expires". Anything
/// else must PARSE. The parser is the E3 usage-window parser
/// ([`crate::tenant_api::time_bound::parse_bound`]) rather than a second
/// implementation: it accepts RFC3339 with or without an offset, the dropped-`Z`
/// spelling, the space-separated historical form, and epoch seconds or
/// milliseconds, and it returns `None` rather than rounding an
/// un-understood value into a different instant.
///
/// Normalizing on the way IN is what makes the STORED form canonical, and the
/// expiry gate in `hydra_core::identity` compares `expires_at` as a STRING — a
/// `+08:00` value written verbatim would sit a whole day off in that comparison
/// while looking correct to a human.
fn normalize_expires_at(raw: Option<&str>) -> Result<Option<String>, String> {
    let value = raw.map(|v| v.trim()).filter(|v| !v.is_empty());
    match value {
        None => Ok(None),
        Some(v) => match crate::tenant_api::time_bound::parse_bound(v) {
            Some(dt) => Ok(Some(crate::tenant_api::time_bound::canonical(dt))),
            None => Err(format!(
                "`expires_at` must be RFC3339 (2026-09-16T00:00:00Z), epoch seconds or \
                     epoch milliseconds; '{}' could not be understood",
                v
            )),
        },
    }
}

/// `GET` / `POST /api/v1/tenants/{tenant_id}/access-tokens`.
///
/// `GET` answers EVERY token of that tenant INCLUDING the DISABLED ones (an
/// operator managing tokens has to see them — the loader's enabled-only filter is
/// a routing decision, not a management one). Rows come back `ORDER BY id` from
/// `db::list_tenant_access_tokens`, so two nodes with the same data answer
/// identically.
///
/// An unknown tenant segment answers an EMPTY list on `GET` rather than 404: this
/// is the read side. The same path on `POST` DOES report `404 tenant not found`
/// (see [`tenant_access_token_create`]), because a token without its tenant would
/// be a dangling credential the moment its INSERT met the FK.
pub(super) async fn tenant_access_token_collection(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    tenant_id: &str,
    trace_id: &str,
) -> Resp {
    match method {
        "GET" => match crate::db::list_tenant_access_tokens(state.db()).await {
            Ok(rows) => {
                let views: Vec<AccessTokenView> = rows
                    .iter()
                    .filter(|r| r.tenant_id == tenant_id)
                    .map(access_token_view)
                    .collect();
                ok_json(200, &views)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        "POST" => tenant_access_token_create(state, session, tenant_id, trace_id).await,
        _ => method_not_allowed(trace_id),
    }
}

/// `POST` half of the collection route: mint, store, publish, echo ONCE.
///
/// The TENANT is checked BEFORE the body is read and BEFORE anything is minted
/// (see [`tenant_access_token_collection`]'s note on why 404 beats an FK 400).
async fn tenant_access_token_create(
    state: &AdminState,
    session: &mut ServerSession,
    tenant_id: &str,
    trace_id: &str,
) -> Resp {
    match crate::db::get_tenant(state.db(), tenant_id).await {
        Ok(_) => {}
        Err(e) if is_not_found(&e) => {
            return err_json(404, "not_found", "tenant not found", trace_id);
        }
        Err(e) => return db_err_resp(e, trace_id),
    }

    let body = match read_body(session, trace_id).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let req: AccessTokenWrite = match parse_body(&body, trace_id) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    let name: String = match access_token_name_or_error(req.name.as_deref()) {
        Ok(v) => v,
        Err(reason) => return err_json(400, "invalid_name", &reason, trace_id),
    };

    let expires_at: Option<String> = match normalize_expires_at(req.expires_at.as_deref()) {
        Ok(v) => v,
        Err(reason) => return err_json(400, "invalid_expires_at", &reason, trace_id),
    };

    // `UNIQUE(tenant_id, name)` pre-checked from the CURRENT rows so the answer is
    // the OPERATOR-legible `duplicate_name`, not a raw SQLite constraint message.
    let existing: Vec<TenantAccessToken> =
        match crate::db::list_tenant_access_tokens(state.db()).await {
            Ok(rows) => rows,
            Err(e) => return db_err_resp(e, trace_id),
        };
    if existing
        .iter()
        .any(|r| r.tenant_id == tenant_id && r.name == name)
    {
        return err_json(
            409,
            "duplicate_name",
            "this tenant already has an access token with that name",
            trace_id,
        );
    }

    // Mint AFTER the pre-check: a request that is going to 409 must not mint a
    // credential it never hands out.
    let plaintext = new_access_token_plaintext();
    let token = TenantAccessToken {
        id: gen_id(),
        tenant_id: tenant_id.to_string(),
        name: name.to_string(),
        token_hash: sha256_hex_str(&plaintext),
        // The display atoms of the PLAINTEXT: 8 leading characters (the `ht_`
        // prefix included) and 4 trailing ones — the same atoms the api-key rows
        // carry, so a listed token can be matched against what the holder sees.
        token_prefix: head_chars(&plaintext, 8),
        token_last4: tail_chars(&plaintext, 4),
        enabled: true,
        expires_at,
        created_at: now_rfc3339(),
        // The INSERT binds the column's own `datetime('now')` for `updated_at`
        // (see `db::insert_tenant_access_token`); carried here so the in-memory
        // row stays coherent.
        updated_at: now_rfc3339(),
    };

    if let Err(e) = crate::db::insert_tenant_access_token(state.db(), &token).await {
        // A UNIQUE hit HERE is `UNIQUE(tenant_id, name)`: the race between the
        // pre-check above and this INSERT (two concurrent creates with one name).
        // The `token_hash` UNIQUE cannot realistically fire — that would mean the
        // SAME 32-CSPRNG-byte plaintext was minted twice.
        if let sqlx::Error::Database(db) = &e {
            if db.is_unique_violation() {
                return err_json(
                    409,
                    "duplicate_name",
                    "another request created a token with that name first",
                    trace_id,
                );
            }
        }
        return db_err_resp(e, trace_id);
    }

    // Committed locally ⇒ the PUBLISH outcome decides 2xx vs 503 (ADR-0001 D-9):
    // `config_not_published` says the token is live ON THIS node only. The plaintext
    // has NOT been handed out yet, and we cannot recover it after this point, so a
    // failed publish must roll the row back: otherwise the operator gets a 503 with a
    // live-but-unusable credential nobody knows. (Greenfield: no migration/backfill.)
    if let Some(r) = reload_best_effort(state, trace_id).await {
        if let Err(e) = crate::db::delete_tenant_access_token(state.db(), &token.id).await {
            tracing::error!(
                error = %e,
                token_id = %token.id,
                "failed to roll back access token after publish failure; token row is orphaned"
            );
        }
        return r;
    }

    ok_json(
        201,
        &AccessTokenCreated {
            id: token.id.clone(),
            name: token.name.clone(),
            // The ONLY echo of the plaintext anywhere in the API surface.
            token: plaintext,
            token_prefix: token.token_prefix.clone(),
            token_last4: token.token_last4.clone(),
            expires_at: token.expires_at,
        },
    )
}

/// `DELETE /api/v1/tenants/{tenant_id}/access-tokens/{token_id}`.
///
/// The TENANT segment is part of the ADDRESS, so a token that exists under a
/// DIFFERENT tenant is a 404 rather than a delete: the caller cannot have learned
/// that id from this tenant's own list. `db::delete_tenant_access_token` reports
/// the row count, and 0 is the other half of "nothing matched".
///
/// Both misses answer the SAME body on purpose: distinguishing "no such id" from
/// "that id belongs to another tenant" would let an operator probe the id space
/// of tenants they are not allowed to see.
pub(super) async fn tenant_access_token_item(
    state: &AdminState,
    method: &str,
    tenant_id: &str,
    token_id: &str,
    trace_id: &str,
) -> Resp {
    if method != "DELETE" {
        return method_not_allowed(trace_id);
    }

    let existing: Vec<TenantAccessToken> =
        match crate::db::list_tenant_access_tokens(state.db()).await {
            Ok(rows) => rows,
            Err(e) => return db_err_resp(e, trace_id),
        };
    match existing.iter().find(|r| r.id == token_id) {
        None => err_json(404, "not_found", "access token not found", trace_id),
        Some(r) if r.tenant_id != tenant_id => {
            err_json(404, "not_found", "access token not found", trace_id)
        }
        Some(_) => match crate::db::delete_tenant_access_token(state.db(), token_id).await {
            Ok(0) => err_json(404, "not_found", "access token not found", trace_id),
            Ok(_) => {
                if let Some(r) = reload_best_effort(state, trace_id).await {
                    return r;
                }
                empty(204)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
    }
}

#[cfg(test)]
mod tests {
    use hydra_core::model::GlobalRole;

    // A nested `mod` does not see the enclosing module's names implicitly.
    use super::{global_role_write_error, global_role_write_warnings, RoleDims};

    // The validator is PURE over these fixtures — no DB, no HTTP. It is worth
    // pinning because the alternative is a bad rule that PUBLISHES and ROUTES:
    // `global_role_write_error` is the last line of defence before
    // `upsert_global_role_with_providers`.
    fn dims() -> RoleDims {
        let tenants: Vec<String> = vec!["t_acme".to_string(), "t_other".to_string()];
        // (group_id -> tenant_id)
        let groups: Vec<(String, String)> = vec![
            ("g_1".to_string(), "t_acme".to_string()),
            // A SECOND group in `t_acme`, so a group-level contradiction can fire
            // WITHOUT a tenant-level one masking it.
            ("g_1a".to_string(), "t_acme".to_string()),
            ("g_2".to_string(), "t_other".to_string()),
        ];
        // (user_id -> (tenant_id, group_id))
        let users: Vec<(String, (String, String))> = vec![
            ("u_1".to_string(), ("t_acme".to_string(), "g_1".to_string())),
            // Same TENANT as u_1, but in a DIFFERENT group.
            (
                "u_3".to_string(),
                ("t_acme".to_string(), "g_1a".to_string()),
            ),
            (
                "u_2".to_string(),
                ("t_other".to_string(), "g_2".to_string()),
            ),
        ];
        // (api_key_id -> user_id)
        let keys: Vec<(String, String)> = vec![("k_1".to_string(), "u_1".to_string())];
        let providers: Vec<String> = vec!["p_anthropic".to_string(), "p_openai".to_string()];
        RoleDims {
            tenants: tenants.iter().cloned().collect(),
            groups: groups
                .iter()
                .map(|(id, tenant)| (id.clone(), tenant.clone()))
                .collect(),
            users: users
                .iter()
                .map(|(id, owners)| (id.clone(), owners.clone()))
                .collect(),
            keys: keys
                .iter()
                .map(|(id, user)| (id.clone(), user.clone()))
                .collect(),
            providers: providers.iter().cloned().collect(),
        }
    }

    /// A rule with the five dimensions given EXPLICITLY (the handler defaults them
    /// to `*`; the validator only ever sees a finished [`GlobalRole`]).
    fn rule(
        name: &str,
        tenant_id: &str,
        user_group_id: &str,
        user_id: &str,
        api_key_id: &str,
        model_key: &str,
        provider_ids: Vec<String>,
    ) -> GlobalRole {
        GlobalRole {
            id: "r_test".to_string(),
            name: name.to_string(),
            tenant_id: tenant_id.to_string(),
            user_group_id: user_group_id.to_string(),
            user_id: user_id.to_string(),
            api_key_id: api_key_id.to_string(),
            model_key: model_key.to_string(),
            provider_ids,
            enabled: true,
            created_at: "".to_string(),
            updated_at: "".to_string(),
        }
    }

    /// The validator's verdict as an `Option`, so the assertions read as prose:
    /// `None` = the rule WOULD be persisted, `Some(reason)` = 400 `invalid_role`.
    /// The locals exist because BOTH arguments are taken by reference.
    fn verdict(
        name: &str,
        t: &str,
        g: &str,
        u: &str,
        k: &str,
        m: &str,
        providers: Vec<String>,
    ) -> Option<String> {
        let role = rule(name, t, g, u, k, m, providers);
        let known = dims();
        global_role_write_error(&role, &known)
    }

    fn accepts(
        name: &str,
        t: &str,
        g: &str,
        u: &str,
        k: &str,
        m: &str,
        providers: Vec<String>,
    ) -> bool {
        verdict(name, t, g, u, k, m, providers).is_none()
    }

    /// All-`*` dimensions + known providers: the "grant these providers to everyone"
    /// shape, and the empty provider set (matches, contributes nothing) is legal too.
    #[test]
    fn an_all_wildcard_rule_is_accepted() {
        let p = vec!["p_openai".to_string()];
        assert!(accepts("any", "*", "*", "*", "*", "*", p));
        assert!(accepts("no providers", "*", "*", "*", "*", "*", Vec::new()));
    }

    #[test]
    fn an_empty_name_is_refused() {
        let reason = verdict("   ", "*", "*", "*", "*", "*", Vec::new());
        assert!(reason.is_some());
        assert!(reason.unwrap_or_default().contains("name"));
    }

    /// `*` is the wildcard of the RULE dimensions; `global_role_provider.provider_id`
    /// is an FK to `provider(id)`, so a `*` here could only ever be a typo.
    #[test]
    fn the_wildcard_is_never_a_provider_id() {
        let reason = verdict("x", "*", "*", "*", "*", "*", vec!["*".to_string()]);
        assert!(reason.is_some());
        assert!(reason.unwrap_or_default().contains("wildcard"));
    }

    #[test]
    fn an_unknown_provider_is_refused() {
        let p = vec!["p_openai".to_string(), "p_ghost".to_string()];
        let reason = verdict("x", "*", "*", "*", "*", "*", p);
        assert!(reason.unwrap_or_default().contains("p_ghost"));
    }

    #[test]
    fn every_pinned_dimension_must_name_an_existing_row() {
        assert!(verdict("x", "t_ghost", "*", "*", "*", "*", Vec::new()).is_some());
        assert!(verdict("x", "*", "g_ghost", "*", "*", "*", Vec::new()).is_some());
        assert!(verdict("x", "*", "*", "u_ghost", "*", "*", Vec::new()).is_some());
        assert!(verdict("x", "*", "*", "*", "k_ghost", "*", Vec::new()).is_some());
    }

    /// The cross-tenant contradictions, including the one a `*` tenant would hide:
    /// `group` in `t_acme` + `user` in `t_other` is refused even with the rule's own
    /// `tenant_id` left as the wildcard.
    #[test]
    fn pinned_rows_must_agree_on_one_tenant() {
        // pinned tenant vs pinned group's owner
        let reason = verdict("x", "t_acme", "g_2", "*", "*", "*", Vec::new());
        assert!(reason.unwrap_or_default().contains("t_other"));
        // wildcard tenant, but the two pinned rows live in DIFFERENT tenants
        let reason = verdict("x", "*", "g_2", "u_1", "*", "*", Vec::new());
        assert!(reason.is_some());
        // same tenant, different group: the GROUP-level check fires on its own
        let reason = verdict("x", "t_acme", "g_1", "u_3", "*", "*", Vec::new());
        assert!(reason.unwrap_or_default().contains("g_1a"));
        // consistent chain: accepted
        assert!(accepts("ok", "t_acme", "g_1", "u_1", "*", "*", Vec::new()));
    }

    /// The `api_key -> user -> group -> tenant` chain is FOLLOWED one hop further:
    /// a key pinned alongside a DIFFERENT user (or a group its owner is not in) is a
    /// contradiction, not a narrower rule.
    #[test]
    fn an_api_key_rule_is_checked_through_its_owner() {
        // k_1 belongs to u_1 (in t_acme / g_1)
        assert!(verdict("x", "*", "*", "u_2", "k_1", "*", Vec::new()).is_some());
        assert!(verdict("x", "*", "g_2", "*", "k_1", "*", Vec::new()).is_some());
        // key alone, and key + its full consistent chain: both fine
        assert!(accepts("ok", "*", "*", "*", "k_1", "*", Vec::new()));
        assert!(accepts(
            "ok",
            "t_acme",
            "g_1",
            "u_1",
            "k_1",
            "*",
            Vec::new()
        ));
    }

    /// A concrete `model_key` is legal (pre-provisioning / a one-model rule) but
    /// WARNs; `*` never warns.
    #[test]
    fn a_pinned_model_key_only_warns() {
        let pinned = rule("m", "*", "*", "*", "*", "gpt-4o", Vec::new());
        let known = dims();
        assert!(global_role_write_error(&pinned, &known).is_none());
        let warnings = global_role_write_warnings(&pinned);
        assert_eq!(warnings.len(), 1);
        let first: String = warnings.first().cloned().unwrap_or_default();
        assert!(first.contains("gpt-4o"));

        let wildcard = rule("m", "*", "*", "*", "*", "*", Vec::new());
        assert_eq!(global_role_write_warnings(&wildcard).len(), 0);
    }

    // --- tenant access tokens (plan §3.2 / T1 + T4a) --------------------------
    // PURE pieces only: the name rule, the expiry normalizer, the display atoms,
    // and the list projection. No DB, no HTTP.
    use super::{
        access_token_name_or_error, access_token_view, head_chars, new_access_token_plaintext,
        normalize_expires_at, tail_chars,
    };
    use hydra_core::model::TenantAccessToken;

    /// `normalize_expires_at` rendered as ONE comparable string: the canonical
    /// value, `<never>` for "never expires", or `ERR:` + the reason. A string
    /// keeps the assertions readable and pins the DISTINCTION the handler relies
    /// on (refused vs never-expires vs normalized).
    fn norm(raw: Option<&str>) -> String {
        match normalize_expires_at(raw) {
            Ok(v) => match v {
                Some(s) => s,
                None => "<never>".to_string(),
            },
            Err(reason) => format!("ERR: {reason}"),
        }
    }

    #[test]
    fn an_empty_or_whitespace_token_name_is_refused() {
        assert!(access_token_name_or_error(None).is_err());
        assert!(access_token_name_or_error(Some("   ")).is_err());
        // Padded input is ACCEPTED with the padding STRIPPED — the strip is what
        // makes the pre-check and the stored `UNIQUE(tenant_id, name)` agree.
        let name: String = access_token_name_or_error(Some("  ci  ")).unwrap_or_default();
        assert!(name == "ci");
    }

    #[test]
    fn an_expires_at_is_normalized_to_canonical_utc() {
        // Absent, empty, or whitespace all mean "never expires" — NOT a 400.
        assert!(norm(None) == "<never>");
        assert!(norm(Some("   ")) == "<never>");
        // Canonical in, canonical out.
        assert!(norm(Some("2026-09-16T00:00:00Z")) == "2026-09-16T00:00:00Z");
        // An OFFSET is CONVERTED to UTC, not copied: this is what keeps the
        // string comparison in `hydra_core::identity` honest.
        assert!(norm(Some("2026-09-16T00:00:00+08:00")) == "2026-09-15T16:00:00Z");
        // The dropped-`Z` and space-separated historical forms are ACCEPTED
        // and REWRITTEN.
        assert!(norm(Some("2026-09-16T00:00:00")) == "2026-09-16T00:00:00Z");
        assert!(norm(Some("2026-09-16 00:00:00")) == "2026-09-16T00:00:00Z");
        // Epoch seconds parse too (the shape a script often emits).
        assert!(!norm(Some("1789516800")).starts_with("ERR"));
        // Garbage is REFUSED rather than rounded.
        assert!(norm(Some("next friday")).starts_with("ERR"));
    }

    #[test]
    fn a_minted_token_is_ht_plus_43_base64url_characters() {
        let token: String = new_access_token_plaintext();
        assert!(head_chars(&token, 3) == "ht_");
        // 3 + 43: 32 CSPRNG bytes, base64url WITHOUT padding.
        assert_eq!(token.chars().collect::<Vec<char>>().len(), 46);
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')),
            "must stay inside the URL_SAFE_NO_PAD alphabet",
        );
        // Two mints differ (CSPRNG, not a counter).
        assert!(new_access_token_plaintext() != token);
    }

    #[test]
    fn the_display_atoms_are_a_prefix_and_a_suffix_never_the_whole_token() {
        let token: String = new_access_token_plaintext();
        let prefix: String = head_chars(&token, 8);
        let last4: String = tail_chars(&token, 4);
        assert!(token.starts_with(&prefix));
        assert!(token.ends_with(&last4));
        assert_eq!(prefix.chars().collect::<Vec<char>>().len(), 8);
        assert_eq!(last4.chars().collect::<Vec<char>>().len(), 4);
        // Neither atom is the token: the atoms are for RECOGNIZING it.
        assert!(prefix != token);
        assert!(last4 != token);
        // Short input rule: the value itself, not a panic and not a truncation
        // that loses characters.
        assert!(tail_chars("ht_", 4) == "ht_");
    }

    #[test]
    fn a_listed_token_carries_neither_the_hash_nor_the_plaintext() {
        let row = TenantAccessToken {
            id: "tok_1".to_string(),
            tenant_id: "t_acme".to_string(),
            name: "ci".to_string(),
            token_hash: "deadbeefdeadbeef".to_string(),
            token_prefix: "ht_ABCD".to_string(),
            token_last4: "wxyz".to_string(),
            enabled: true,
            expires_at: Some("2026-09-16T00:00:00Z".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let body: String = serde_json::json!(access_token_view(&row)).to_string();
        assert!(
            !body.contains("deadbeef"),
            "the hash must never be serialized"
        );
        assert!(body.contains("ht_ABCD"));
        assert!(body.contains("wxyz"));
        for field in [
            "id",
            "name",
            "token_prefix",
            "token_last4",
            "enabled",
            "expires_at",
            "created_at",
        ] {
            assert!(
                body.contains(&format!("\"{field}\"")),
                "every documented field is present: {body}"
            );
        }
    }
}

#[cfg(test)]
mod gen_id_tests {
    use super::gen_id;
    use std::collections::HashSet;
    use std::sync::{Arc, Barrier};

    #[test]
    fn gen_id_is_unique_under_concurrency() {
        let n = 8usize;
        let per = 500usize;
        let barrier = Arc::new(Barrier::new(n));
        let mut handles = Vec::new();
        for _ in 0..n {
            let b = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                b.wait();
                (0..per).map(|_| gen_id()).collect::<Vec<_>>()
            }));
        }
        let ids: HashSet<String> = handles
            .into_iter()
            .flat_map(|h| h.join().expect("thread"))
            .collect();
        assert_eq!(
            ids.len(),
            n * per,
            "concurrent gen_id() produced duplicates"
        );
    }
}
