//! Admin REST handlers — the business logic behind each route (design §13.2).
//!
//! Every handler takes a borrowed [`AdminState`] and the live Pingora
//! [`ServerSession`] (so it can read the request body when needed) and returns a
//! complete `http::Response<Vec<u8>>`. The router in [`super`] dispatches to
//! these after the admin-token gate. No internal logic is mocked: each handler
//! drives the real `db::repo`, `ConfigStore`, `AuthChecker`, `CircuitBreaker`
//! and `HydraCertStore`.

use std::collections::HashSet;
use std::sync::Arc;

use http::Response;
use hydra_core::auth::sha256_hex;
use hydra_core::model::{
    LimitRole, Provider, ProviderKey, ProviderKeyBinding, ProviderKeyDto, ProviderModel, SubTenant,
    SubTenantRoute, Tenant, TenantModel, TenantProvider,
};
use hydra_core::sub_tenant::SubTenantWriteError;
use pingora_core::protocols::http::ServerSession;
use serde::{Deserialize, Serialize};

use super::sub_tenant_write;
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
// Shared reload (write-after consistency, design §13.2/§12.1)
// ---------------------------------------------------------------------------

/// Reload the in-memory snapshot. Cert re-resolution needs no call here: the
/// cert store is registered as a `ConfigStore` snapshot-change hook, so the swap
/// itself re-resolves certs for the next TLS handshake. Serialised by the
/// per-state mutex so concurrent writes don't race (design §6 risk note).
/// Best-effort: a fatal validation failure is logged but does **not** fail an
/// already-committed write (design §5.3 keeps the old snapshot; the next
/// successful reload recovers).
async fn reload_best_effort(state: &AdminState, trace_id: &str) {
    let _guard = state.reload_lock.lock().await;
    if let Err(e) = state.store.reload_all().await {
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
        metrics::record_config_snapshot_stale(true);
        state
            .snapshot_stale
            .store(true, std::sync::atomic::Ordering::Release);
        return;
    }
    metrics::record_config_snapshot_stale(false);
    state
        .snapshot_stale
        .store(false, std::sync::atomic::Ordering::Release);
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
                    reload_best_effort(state, trace_id).await;
                    ok_json(200, &p)
                }
                Err(_) => err_json(404, "not_found", "provider not found", trace_id),
            }
        }
        "DELETE" => match crate::db::delete_provider(state.db(), id).await {
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
                    reload_best_effort(state, trace_id).await;
                    ok_json(200, &m)
                }
                Err(_) => err_json(404, "not_found", "model not found", trace_id),
            }
        }
        "DELETE" => match crate::db::delete_provider_model(state.db(), id).await {
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
        reload_best_effort(state, trace_id).await;
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
            reload_best_effort(state, trace_id).await;
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
    /// Tenant self-service access token (migration 0009): stored as a
    /// SHA-256 hex hash only, never echoed. `Some("")` clears the token;
    /// `None` (or a missing field) keeps the current token on edit.
    #[serde(default)]
    access_token: Option<String>,
}

/// Tenant response view: the entity plus a derived `has_access_token` flag.
/// The token hash itself is one-way and write-only — never serialized.
#[derive(Serialize)]
struct TenantView {
    #[serde(flatten)]
    tenant: Tenant,
    has_access_token: bool,
    /// The write IS committed; this only says the post-write reload failed, so
    /// the running config still shows the previous value. Process-level flag —
    /// see `AdminState::snapshot_stale`.
    snapshot_stale: bool,
}

impl TenantView {
    fn new(tenant: Tenant, has_access_token: bool, snapshot_stale: bool) -> Self {
        Self {
            tenant,
            has_access_token,
            snapshot_stale,
        }
    }

    /// Build from live admin state (the common case).
    fn from_state(state: &AdminState, tenant: Tenant, has_access_token: bool) -> Self {
        Self::new(
            tenant,
            has_access_token,
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
/// `(token_hash, cert)` — see [`crate::db::TenantWrite`].
///
/// Naming the tuple keeps the helper's signature readable and the DB layer's
/// contract explicit.
type SecretWrites = (
    Option<Option<String>>,
    Option<(Option<String>, Option<String>)>,
);

/// Turn the handler-level certificate action and the body's `access_token` into
/// the two optional writes that [`crate::db::write_tenant`] applies inside its
/// transaction.
///
/// PURE on purpose: every validation and every hash happens BEFORE the
/// transaction opens, so a rejected token can never leave a committed tenant
/// behind (audit §4 "zombie tenant"), and the DB layer receives plain values.
///
/// Semantics preserved from the previous step-by-step helpers:
/// - certificate: `None` = leave alone, `Clear` = clear, `Content` = replace;
/// - access token: body `None` = leave alone, `Some("")` = clear,
///   `Some(non-empty)` = set/rotate (stored as a SHA-256 hex digest only).
#[allow(clippy::result_large_err)]
fn resolved_secret_writes(
    action: CertWrite,
    access_token: &Option<String>,
    trace_id: &str,
) -> Result<SecretWrites, Resp> {
    let cert = match action {
        CertWrite::None => None,
        CertWrite::Clear => Some((None, None)),
        CertWrite::Content {
            cert_pem,
            cert_key_pem,
        } => Some((Some(cert_pem), Some(cert_key_pem))),
    };
    let token_hash = match access_token {
        None => None,
        Some(raw) => {
            let token = raw.trim();
            if token.is_empty() {
                Some(None) // empty means "clear the token"
            } else {
                // Defence in depth: `validate_access_token_shape` already ran
                // before the write, but the rule lives here too so the helper
                // cannot silently accept a short token if a caller forgets.
                if token.len() < crate::tenant_api::MIN_TENANT_TOKEN_LEN {
                    return Err(err_json(
                        400,
                        "invalid_access_token",
                        &format!(
                            "access_token must be at least {} characters",
                            crate::tenant_api::MIN_TENANT_TOKEN_LEN
                        ),
                        trace_id,
                    ));
                }
                Some(Some(sha256_hex_str(token)))
            }
        }
    };
    Ok((token_hash, cert))
}

/// Shape-check a tenant access token BEFORE the tenant row is written.
///
/// The certificate path already pre-validates for exactly this reason
/// (`resolve_tenant_cert_write` before `insert_tenant`): a 4xx that arrives
/// after the INSERT leaves a tenant the UI reported as "create failed" but
/// which exists and is enabled - and a retry then hits the UNIQUE constraint,
/// so the operator gives up and keeps the zombie (audit section 4).
#[allow(clippy::result_large_err)]
fn validate_access_token_shape(access_token: &Option<String>, trace_id: &str) -> Result<(), Resp> {
    let Some(raw) = access_token else {
        return Ok(());
    };
    let token = raw.trim();
    // Empty means "clear the token" (see `apply_tenant_access_token_write`).
    if !token.is_empty() && token.len() < crate::tenant_api::MIN_TENANT_TOKEN_LEN {
        return Err(err_json(
            400,
            "invalid_access_token",
            &format!(
                "access_token must be at least {} characters",
                crate::tenant_api::MIN_TENANT_TOKEN_LEN
            ),
            trace_id,
        ));
    }
    Ok(())
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
                let with_token: HashSet<String> =
                    crate::db::list_tenant_access_token_hashes(state.db())
                        .await
                        .map(|pairs| pairs.into_iter().map(|(id, _)| id).collect())
                        .unwrap_or_default();
                let views: Vec<TenantView> = rows
                    .into_iter()
                    .map(|t| {
                        let has = with_token.contains(&t.id);
                        TenantView::from_state(state, t, has)
                    })
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
        // Same no-zombie rule for the access token: a too-short token must be
        // rejected BEFORE the INSERT, otherwise the 400 leaves a committed,
        // enabled tenant behind (audit §4).
        if let Err(r) = validate_access_token_shape(&up.access_token, trace_id) {
            return r;
        }
        // ONE transaction: the row, its certificate and its token hash commit
        // together or not at all (audit §2.2 G-P1). Writing them in separate
        // statements is the "500 but the change is live" defect.
        let (token_hash, cert) =
            match resolved_secret_writes(cert_action, &up.access_token, trace_id) {
                Ok(v) => v,
                Err(resp) => return resp,
            };
        if let Err(e) = crate::db::write_tenant(
            state.db(),
            state.key_provider.as_ref(),
            crate::db::TenantWrite {
                tenant: &t,
                is_create: true,
                token_hash,
                cert,
            },
        )
        .await
        {
            return db_err_resp(e, trace_id);
        }
        let has = crate::db::tenant_has_access_token(state.db(), &t.id)
            .await
            .unwrap_or(false);
        reload_best_effort(state, trace_id).await;
        ok_json(201, &TenantView::from_state(state, t, has))
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
            Ok(t) => {
                let has = crate::db::tenant_has_access_token(state.db(), id)
                    .await
                    .unwrap_or(false);
                ok_json(200, &TenantView::from_state(state, t, has))
            }
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
            // Pre-validate for the same reason as POST: a rejected token must
            // not leave the tenant half-updated.
            if let Err(r) = validate_access_token_shape(&up.access_token, trace_id) {
                return r;
            }
            // One transaction, exactly like create (see above).
            let (token_hash, cert) =
                match resolved_secret_writes(cert_action, &up.access_token, trace_id) {
                    Ok(v) => v,
                    Err(resp) => return resp,
                };
            // Which token state did THIS request write? (`None` = untouched.)
            // Computed before the struct literal moves `token_hash`.
            let written_token_state = token_hash.as_ref().map(|h| h.is_some());
            if let Err(e) = crate::db::write_tenant(
                state.db(),
                state.key_provider.as_ref(),
                crate::db::TenantWrite {
                    tenant: &t,
                    is_create: false,
                    token_hash,
                    cert,
                },
            )
            .await
            {
                return db_err_resp(e, trace_id);
            }
            // The write IS committed at this point, so the answer must not
            // depend on a second read succeeding: a failed re-read used to be
            // reported as `404 not found` (and as `has_access_token: false`) for a
            // tenant that had just been written — "reported as failed but
            // actually live", one step later than §7-3 fixed it.
            let has = match written_token_state {
                // We know what we just wrote, so answer from that.
                Some(written) => written,
                None => crate::db::tenant_has_access_token(state.db(), id)
                    .await
                    .unwrap_or(true), // read failed ⇒ do not claim "no token"
            };
            match crate::db::get_tenant(state.db(), id).await {
                Ok(t) => {
                    reload_best_effort(state, trace_id).await;
                    ok_json(200, &TenantView::from_state(state, t, has))
                }
                Err(e) if is_not_found(&e) => {
                    err_json(404, "not_found", "tenant not found", trace_id)
                }
                Err(e) => db_err_resp(e, trace_id),
            }
        }
        "DELETE" => match crate::db::delete_tenant(state.db(), id).await {
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
        "GET" => match crate::db::get_tenant_provider(state.db(), id).await {
            Ok(tp) => ok_json(200, &tp),
            Err(e) if is_not_found(&e) => {
                err_json(404, "not_found", "tenant_provider not found", trace_id)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        "DELETE" => match crate::db::delete_tenant_provider(state.db(), id).await {
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
        "GET" => match crate::db::get_tenant_model(state.db(), id).await {
            Ok(tm) => ok_json(200, &tm),
            Err(e) if is_not_found(&e) => {
                err_json(404, "not_found", "tenant_model not found", trace_id)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        "DELETE" => match crate::db::delete_tenant_model(state.db(), id).await {
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
/// No key-scoped routing gate applies (client_api_key is None on admin reads ⇒
/// both the (3.5) operator key-prefix binding, `match_key_binding`, and the
/// (3.6) sub-tenant route gate, `match_sub_tenant_route`, never fire — both are
/// key-scoped, so this keyless view is a structural no-op for them, mirroring
/// router::accessible_models). A
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

pub(super) async fn limit_role_collection(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    trace_id: &str,
) -> Resp {
    if method == "GET" {
        match crate::db::list_limit_roles(state.db()).await {
            Ok(rows) => ok_json(200, &rows),
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
        if r.id.is_empty() {
            r.id = gen_id();
        }
        if r.created_at.is_empty() {
            r.created_at = now_ts();
        }
        match crate::db::insert_limit_role(state.db(), &r).await {
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
        "GET" => match crate::db::get_limit_role(state.db(), id).await {
            Ok(r) => ok_json(200, &r),
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
            r.id = id.to_string();
            match crate::db::update_limit_role(state.db(), &r).await {
                Ok(()) => {}
                Err(e) => return db_err_resp(e, trace_id),
            }
            match crate::db::get_limit_role(state.db(), id).await {
                Ok(r) => {
                    reload_best_effort(state, trace_id).await;
                    ok_json(200, &r)
                }
                Err(_) => err_json(404, "not_found", "role not found", trace_id),
            }
        }
        "DELETE" => match crate::db::delete_limit_role(state.db(), id).await {
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
        match crate::db::list_provider_key_bindings(state.db()).await {
            Ok(rows) => ok_json(200, &rows),
            Err(e) => db_err_resp(e, trace_id),
        }
    } else if method == "POST" {
        let body = match read_body(session, trace_id).await {
            Ok(b) => b,
            Err(r) => return r,
        };
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
        match crate::db::insert_provider_key_binding(state.db(), &b).await {
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
        "GET" => match crate::db::get_provider_key_binding(state.db(), id).await {
            Ok(b) => ok_json(200, &b),
            Err(e) if is_not_found(&e) => {
                err_json(404, "not_found", "provider_key_binding not found", trace_id)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        "PUT" => {
            let body = match read_body(session, trace_id).await {
                Ok(b) => b,
                Err(r) => return r,
            };
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
            match crate::db::update_provider_key_binding(state.db(), &b).await {
                Ok(()) => {}
                Err(e) => return db_err_resp(e, trace_id),
            }
            match crate::db::get_provider_key_binding(state.db(), id).await {
                Ok(b) => {
                    reload_best_effort(state, trace_id).await;
                    ok_json(200, &b)
                }
                Err(_) => err_json(404, "not_found", "provider_key_binding not found", trace_id),
            }
        }
        "DELETE" => match crate::db::delete_provider_key_binding(state.db(), id).await {
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
// Sub-tenants & sub-tenant routes (design-sub-tenant.md §3.1, T6)
// ===========================================================================
//
// Write path is error-level fail-closed and **transactional** (sub-tenant v2,
// D5): every create / update / delete goes through the shared write core
// [`sub_tenant_write`], which in ONE transaction reads the FULL set of DB rows
// (incl. disabled), validates the write against that live snapshot + the
// current `ConfigData`, then inserts / upserts / updates / deletes and
// commits. A rejected write is never persisted (no silent accept). This closes
// the two v1 post-implementation findings:
//  * finding 1 — the quota now counts **all** DB rows (not just the enabled
//    snapshot), so disable-then-recreate cannot grow the DB past the cap;
//  * finding 2 — the prefix-overlap check runs against the live in-transaction
//    rows, closing the validate-then-insert TOCTOU.
//
// On a successful mutation the snapshot is reloaded (`reload_best_effort`) and
// the request inherits the automatic leader forwarding of
// `maybe_forward_mutation`.

/// Map a [`SubTenantWriteError`] to a semantic 400/409 response. Duplicates
/// (name / prefix) are 409; every other rule violation is 400.
fn sub_tenant_write_err_resp(e: &SubTenantWriteError, trace_id: &str) -> Resp {
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

/// Map a write-core [`sub_tenant_write::CoreError`] to an HTTP response.
/// `not_found_msg` is the resource-specific 404 message (the core does not
/// know which resource was targeted).
fn sub_tenant_core_err_resp(
    e: sub_tenant_write::CoreError,
    trace_id: &str,
    not_found_msg: &str,
) -> Resp {
    match e {
        sub_tenant_write::CoreError::Validation(v) => sub_tenant_write_err_resp(&v, trace_id),
        sub_tenant_write::CoreError::Db(d) => db_err_resp(d, trace_id),
        sub_tenant_write::CoreError::PrefixGenerationFailed => err_json(
            400,
            "prefix_generation_failed",
            &format!(
                "could not generate a non-conflicting key_prefix after \
                 {} attempts",
                sub_tenant_write::PREFIX_ATTEMPTS
            ),
            trace_id,
        ),
        sub_tenant_write::CoreError::NotFound => {
            err_json(404, "not_found", not_found_msg, trace_id)
        }
    }
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

/// Admin create/update request body for a sub-tenant. `key_prefix` is
/// `Option`: **omitted** (`None`) ⇒ the leader auto-generates one (Q13); an
/// explicit **empty string** is invalid (400 `empty_key_prefix`) and is
/// distinct from omitted. `created_at` / `updated_at` are NOT accepted: the
/// transactional write core (D4 idempotent upsert) sets them server-side
/// (`datetime('now')`), so client-supplied timestamps are ignored.
#[derive(Deserialize)]
struct SubTenantWriteReq {
    #[serde(default)]
    id: String,
    tenant_id: String,
    name: String,
    /// Omitted ⇒ auto-generate; an explicit value (incl. `""`) is validated.
    #[serde(default)]
    key_prefix: Option<String>,
    #[serde(default = "default_true")]
    enabled: bool,
}

/// Admin create/update request body for a sub-tenant route. `model_key` is
/// `Option`: omitted / `null` ⇒ the sub-tenant's **default route**. Timestamps
/// are NOT accepted (set server-side by the write core, D4).
#[derive(Deserialize)]
struct SubTenantRouteWriteReq {
    #[serde(default)]
    id: String,
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

pub(super) async fn sub_tenant_collection(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    query: Option<&str>,
    trace_id: &str,
) -> Resp {
    if method == "GET" {
        match crate::db::list_sub_tenants(state.db()).await {
            Ok(rows) => {
                let by_tenant = query_param(query, "tenant_id");
                let out: Vec<SubTenant> = match by_tenant {
                    Some(tid) => rows.into_iter().filter(|s| s.tenant_id == tid).collect(),
                    None => rows,
                };
                ok_json(200, &out)
            }
            Err(e) => db_err_resp(e, trace_id),
        }
    } else if method == "POST" {
        let body = match read_body(session, trace_id).await {
            Ok(b) => b,
            Err(r) => return r,
        };
        let req: SubTenantWriteReq = match parse_body(&body, trace_id) {
            Ok(req) => req,
            Err(r) => return r,
        };
        // The row id: client-supplied or generated (D4: on a name conflict the
        // existing row's id is retained, so this is only used for new rows).
        let id = if req.id.is_empty() { gen_id() } else { req.id };
        let cfg = std::sync::Arc::clone(&*state.store.snapshot());

        // Transactional write core (D5): reads ALL rows in the tx, validates
        // (all-rows quota + overlap), upserts by (tenant_id, name), commits.
        // `key_prefix = None` ⇒ auto-generate with retry (Q13 / F3).
        let created = match sub_tenant_write::create_sub_tenant(
            state.db(),
            &cfg,
            &req.tenant_id,
            &req.name,
            req.key_prefix.as_deref(),
            &id,
            req.enabled,
        )
        .await
        {
            Ok(st) => st,
            Err(e) => return sub_tenant_core_err_resp(e, trace_id, "sub_tenant not found"),
        };
        reload_best_effort(state, trace_id).await;
        ok_json(201, &created)
    } else {
        method_not_allowed(trace_id)
    }
}

pub(super) async fn sub_tenant_item(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    id: &str,
    trace_id: &str,
) -> Resp {
    match method {
        "GET" => match crate::db::get_sub_tenant(state.db(), id).await {
            Ok(st) => ok_json(200, &st),
            Err(e) if is_not_found(&e) => {
                err_json(404, "not_found", "sub_tenant not found", trace_id)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        "PUT" => {
            let body = match read_body(session, trace_id).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let req: SubTenantWriteReq = match parse_body(&body, trace_id) {
                Ok(req) => req,
                Err(r) => return r,
            };
            // An update must set an explicit prefix (omitting is NOT auto-generate).
            let Some(prefix) = req.key_prefix else {
                return err_json(
                    400,
                    "empty_key_prefix",
                    "key_prefix is required on update",
                    trace_id,
                );
            };
            let cfg = std::sync::Arc::clone(&*state.store.snapshot());

            // Transactional write core (D5): in-transaction existence check
            // (404), all-rows validation (the row's own prefix is self-excluded),
            // update by immutable id, commit, re-read. `tenant_id` is immutable
            // and authoritative from the existing row (the core enforces this).
            let updated = match sub_tenant_write::update_sub_tenant(
                state.db(),
                &cfg,
                id,
                &req.name,
                &prefix,
                req.enabled,
            )
            .await
            {
                Ok(st) => st,
                Err(e) => return sub_tenant_core_err_resp(e, trace_id, "sub_tenant not found"),
            };
            reload_best_effort(state, trace_id).await;
            ok_json(200, &updated)
        }
        "DELETE" => match sub_tenant_write::delete_sub_tenant(state.db(), id).await {
            Ok(()) => {
                reload_best_effort(state, trace_id).await;
                empty(204)
            }
            Err(e) => sub_tenant_core_err_resp(e, trace_id, "sub_tenant not found"),
        },
        _ => method_not_allowed(trace_id),
    }
}

pub(super) async fn sub_tenant_route_collection(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    query: Option<&str>,
    trace_id: &str,
) -> Resp {
    if method == "GET" {
        match crate::db::list_sub_tenant_routes(state.db()).await {
            Ok(rows) => {
                let by_st = query_param(query, "sub_tenant_id");
                let out: Vec<SubTenantRoute> = match by_st {
                    Some(sid) => rows
                        .into_iter()
                        .filter(|r| r.sub_tenant_id == sid)
                        .collect(),
                    None => rows,
                };
                ok_json(200, &out)
            }
            Err(e) => db_err_resp(e, trace_id),
        }
    } else if method == "POST" {
        let body = match read_body(session, trace_id).await {
            Ok(b) => b,
            Err(r) => return r,
        };
        let req: SubTenantRouteWriteReq = match parse_body(&body, trace_id) {
            Ok(req) => req,
            Err(resp) => return resp,
        };
        // The row id: client-supplied or generated (D4: on a key conflict the
        // existing row's id is retained, so this is only used for new rows).
        let id = if req.id.is_empty() { gen_id() } else { req.id };
        let cfg = std::sync::Arc::clone(&*state.store.snapshot());

        // Transactional write core (D5): in-transaction sub-tenant (FK)
        // resolution (404), all-rows validation, upsert by (sub_tenant_id,
        // model_key), commit.
        let created = match sub_tenant_write::create_route(
            state.db(),
            &cfg,
            &req.sub_tenant_id,
            &req.provider_id,
            req.model_key.as_deref(),
            &id,
            req.enabled,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => return sub_tenant_core_err_resp(e, trace_id, "sub_tenant not found"),
        };
        reload_best_effort(state, trace_id).await;
        ok_json(201, &created)
    } else {
        method_not_allowed(trace_id)
    }
}

pub(super) async fn sub_tenant_route_item(
    state: &AdminState,
    session: &mut ServerSession,
    method: &str,
    id: &str,
    trace_id: &str,
) -> Resp {
    match method {
        "GET" => match crate::db::get_sub_tenant_route(state.db(), id).await {
            Ok(r) => ok_json(200, &r),
            Err(e) if is_not_found(&e) => {
                err_json(404, "not_found", "sub_tenant_route not found", trace_id)
            }
            Err(e) => db_err_resp(e, trace_id),
        },
        "PUT" => {
            let body = match read_body(session, trace_id).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let req: SubTenantRouteWriteReq = match parse_body(&body, trace_id) {
                Ok(req) => req,
                Err(resp) => return resp,
            };
            let cfg = std::sync::Arc::clone(&*state.store.snapshot());

            // Transactional write core (D5): in-transaction existence check
            // (404), sub-tenant (FK) resolution, all-rows validation (the
            // route's own row is self-excluded), update by immutable id, commit,
            // re-read. `sub_tenant_id` is immutable and authoritative from the
            // existing row (the core enforces this).
            let updated = match sub_tenant_write::update_route(
                state.db(),
                &cfg,
                id,
                &req.provider_id,
                req.model_key.as_deref(),
                req.enabled,
            )
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    return sub_tenant_core_err_resp(e, trace_id, "sub_tenant_route not found")
                }
            };
            reload_best_effort(state, trace_id).await;
            ok_json(200, &updated)
        }
        "DELETE" => match sub_tenant_write::delete_route(state.db(), id).await {
            Ok(()) => {
                reload_best_effort(state, trace_id).await;
                empty(204)
            }
            Err(e) => sub_tenant_core_err_resp(e, trace_id, "sub_tenant_route not found"),
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
    /// How many entries THIS node removed from its L1. Not a fleet count.
    invalidated: usize,
    /// How many keys the request named (0 for a whole-tenant clear).
    checked: usize,
    tenant_id: Option<String>,
    /// `keys` | `tenant`.
    scope: &'static str,
    /// Where the fleet stands: the SHARED report the tenant API's E2 returns,
    /// serialized directly. It replaces the `published: bool` field, which lied —
    /// it defaulted to `true` and only became `false` when a stream existed AND
    /// the publish failed, so a build with no stream claimed a broadcast that
    /// never happened. One type for both entry points, so the operator's view and
    /// the tenant's view cannot drift apart.
    fleet: Fleet,
}

/// Re-exported under the tenant API's name for this module's readers; the type is
/// the barrier's own report (`http_status` is `#[serde(skip)]`, so it never
/// appears in the body — the admin endpoint already carries the status).
#[cfg(feature = "cluster-redis")]
type Fleet = crate::cluster::events::FleetReport;

/// Without `cluster-redis` this cannot be a cluster, so the local clear is the
/// whole answer. Same shape, same field names as the cluster report.
///
/// `pub(crate)` so the tenant API's non-cluster `FleetView` can reference the
/// same type instead of hand-syncing a second copy.
#[cfg(not(feature = "cluster-redis"))]
#[derive(Serialize)]
pub(crate) struct Fleet {
    state: &'static str,
    nodes_total: usize,
    nodes_applied: usize,
    lagging: Vec<String>,
    event_id: Option<String>,
    waited_ms: u64,
}

#[cfg(not(feature = "cluster-redis"))]
impl Fleet {
    /// The single-node derivation: a build without `cluster-redis` cannot have
    /// peers, so the local clear IS the whole fleet answer.
    pub(crate) fn single_node() -> Self {
        Self {
            state: "single_node",
            nodes_total: 1,
            nodes_applied: 1,
            lagging: Vec::new(),
            event_id: None,
            waited_ms: 0,
        }
    }
}

/// Cap on how many api-keys ONE invalidation request may name (review N4).
///
/// Every key becomes a 64-char digest inside a SINGLE stream entry, so an
/// uncapped list lets one request (a *tenant* token is enough) write a
/// multi-hundred-MB entry onto the shared backbone — and, before that, issue
/// two Redis commands per key (`DEL` + `SREM`). 1000 keys ⇒ ~65 KB per entry.
pub(crate) const MAX_INVALIDATION_KEYS: usize = 1_000;

/// Cap on a single api-key's length. Real keys are short; this only blocks
/// abuse of the field as arbitrary payload.
pub(crate) const MAX_API_KEY_LEN: usize = 4_096;

/// Reject an invalidation request that names too many keys, or a key that is
/// absurdly long. `None` = acceptable.
pub(crate) fn invalidate_shape_error(keys: Option<&[String]>, trace_id: &str) -> Option<Resp> {
    let keys = keys?;
    if keys.len() > MAX_INVALIDATION_KEYS {
        return Some(err_json(
            400,
            "too_many_keys",
            &format!(
                "at most {MAX_INVALIDATION_KEYS} api_keys per request ({} given)",
                keys.len()
            ),
            trace_id,
        ));
    }
    if let Some(len) = keys.iter().map(String::len).find(|l| *l > MAX_API_KEY_LEN) {
        return Some(err_json(
            400,
            "invalid_api_key",
            &format!("an api_key is {len} bytes; the limit is {MAX_API_KEY_LEN}"),
            trace_id,
        ));
    }
    None
}

pub(super) async fn auth_cache_invalidate(
    state: &AdminState,
    session: &mut ServerSession,
    trace_id: &str,
) -> Resp {
    // An empty body means "invalidate everything" — tolerate it instead of
    // failing the parse (curl -X DELETE with no body must work).
    let body = match read_body(session, trace_id).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let req: InvalidateRequest = if body.is_empty() || body.iter().all(u8::is_ascii_whitespace) {
        InvalidateRequest {
            tenant_id: None,
            api_keys: None,
        }
    } else {
        match parse_body(&body, trace_id) {
            Ok(r) => r,
            Err(r) => return r,
        }
    };
    if let Some(resp) = invalidate_shape_error(req.api_keys.as_deref(), trace_id) {
        return resp;
    }
    let checked = req.api_keys.as_ref().map_or(0, Vec::len);
    let scope = if req.api_keys.is_none() {
        "tenant"
    } else {
        "keys"
    };
    let count = match (req.tenant_id.as_deref(), req.api_keys.as_deref()) {
        (Some(tid), Some(keys)) => state.auth.invalidate(tid, keys).await,
        (Some(tid), None) => state.auth.invalidate_tenant(tid).await,
        (None, Some(keys)) => {
            // No tenant: invalidate by api_key across every known tenant
            // (design §11.7 "跨租户匹配").
            let snap = state.store.snapshot();
            let mut total = 0usize;
            for t in snap.tenants_by_domain.values() {
                total += state.auth.invalidate(&t.id, keys).await;
            }
            total
        }
        (None, None) => {
            // F-3: an empty-body DELETE means "invalidate everything" — clear
            // the local cache for EVERY known tenant (the remote stream event
            // is already a whole-cache clear; this keeps the `invalidated`
            // count truthful instead of always 0).
            let snap = state.store.snapshot();
            let mut total = 0usize;
            for t in snap.tenants_by_domain.values() {
                total += state.auth.invalidate_tenant(&t.id).await;
            }
            total
        }
    };
    // Broadcast the invalidation cluster-wide (P4) and WAIT for the fleet to
    // confirm it, through the same barrier the tenant API's E2 uses. The status
    // now distinguishes "applied everywhere" (200) from "published, not yet
    // confirmed" (202, with the laggards named) and "the channel did not answer"
    // (503) — the flat 200 + `published: false` this replaced told the operator
    // nothing they could act on.
    #[cfg(feature = "cluster-redis")]
    let report = {
        let live = state.live_nodes.as_ref().map_or_else(Vec::new, |f| f());
        crate::cluster::events::broadcast_and_confirm(
            state.invalidation.as_ref(),
            req.tenant_id.clone(),
            req.api_keys.clone().unwrap_or_default(),
            live,
            // The operator's endpoint always waits: it exists to force a
            // re-authentication, and "I enqueued something" is the answer it
            // used to give.
            Some(state.converge_timeout),
        )
        .await
    };
    #[cfg(feature = "cluster-redis")]
    let report_status = report.http_status;
    // Without `cluster-redis` this cannot be a cluster (`main` refuses
    // `HYDRA_ROLE=leader|edge` without the feature), so the local clear IS the
    // whole answer — the same derivation the tenant API's E2 makes.
    #[cfg(not(feature = "cluster-redis"))]
    let (fleet, status) = (Fleet::single_node(), 200u16);
    #[cfg(feature = "cluster-redis")]
    let (fleet, status) = (report, report_status);

    // Refresh the cache-size gauge after mutation.
    metrics::record_auth_cache_size(state.auth.cache().len());
    let body = InvalidateResponse {
        invalidated: count,
        checked,
        tenant_id: req.tenant_id,
        scope,
        fleet,
    };
    ok_json(status, &body)
}

// ---------------------------------------------------------------------------
// Tenant auth-url connectivity test (Admin UI "Test" button, design §11.3)
// ---------------------------------------------------------------------------

/// Request body for `POST /api/v1/tenants/auth/test`.
#[derive(Deserialize)]
struct AuthTestRequest {
    /// The tenant `auth_url` to probe (may not be saved yet — the UI tests
    /// the field as typed).
    auth_url: String,
    /// Optional tenant id — sent as `X-Hydra-Tenant` and in the request body,
    /// exactly like the real auth path would.
    #[serde(default)]
    tenant_id: Option<String>,
}

/// Result of the simulated auth probe.
#[derive(Serialize)]
struct AuthTestResult {
    /// Overall outcome: true when the URL is reachable, accepts POST, does
    /// not 4xx/5xx, and the simulated key was REJECTED (denial is the
    /// expected verdict for a fake key — see `verdict`).
    ok: bool,
    /// Whether the HTTP round-trip completed (no connect/timeout error).
    reachable: bool,
    /// The status the auth service returned (`None` when unreachable).
    status: Option<u16>,
    /// Whether the URL accepted POST (`false` on 404/405 — wrong path or a
    /// GET-only endpoint).
    protocol_ok: bool,
    /// Classification: `denied` | `allowed` | `unreachable` | `timeout` |
    /// `not_found` | `method_not_allowed` | `unprocessable` |
    /// `server_error` | `unexpected_status`.
    verdict: &'static str,
    /// Human-readable outcome for the UI toast.
    detail: String,
    /// Round-trip duration (ms).
    duration_ms: u64,
    /// Truncated response body for debugging (empty when unreachable).
    body_snippet: String,
}

/// Simulated probe against a tenant `auth_url`: POSTs the exact request the
/// proxy would send (same headers + body via `http::auth_request_body`) with a
/// clearly fake api-key and reports whether the endpoint is usable. A fake key
/// MUST be denied — so "api-key not found" / "auth failed" (401/403, or an
/// explicit `status:false`/`allowed:false` in a 2xx body) is a PASS; an allow,
/// a 422, a 404/405, a 5xx or an unreachable URL is a FAIL. An HTTP 402 (insufficient balance) is
/// treated as a denial PASS; a 2xx denial whose reason is insufficient_balance
/// is reported with verdict "insufficient_balance".
pub(super) async fn tenant_auth_test(
    state: &AdminState,
    session: &mut ServerSession,
    trace_id: &str,
) -> Resp {
    let body = match read_body(session, trace_id).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let req: AuthTestRequest = match parse_body(&body, trace_id) {
        Ok(r) => r,
        Err(r) => return r,
    };
    let url = req.auth_url.trim();
    if url.is_empty() {
        return err_json(400, "missing_auth_url", "auth_url is required", trace_id);
    }

    // A clearly-fake key — it can never be a real tenant credential, so the
    // auth service MUST reject it. Derives from the trace id for debuggability.
    let api_key = format!("sk-hydra-auth-test-{trace_id}");
    let tenant_id = req.tenant_id.clone().unwrap_or_default();
    let timeout = state.auth.config().timeout;

    let started = std::time::Instant::now();
    let send = state
        .auth
        .client()
        .post(url)
        .header("authorization", format!("Bearer {api_key}"))
        .header("x-hydra-tenant", &tenant_id)
        .header("x-hydra-trace-id", trace_id)
        .header("content-type", "application/json")
        .body(crate::http::auth_request_body(&api_key, &tenant_id))
        .send();

    let resp = match tokio::time::timeout(timeout, send).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            return ok_json(
                200,
                &AuthTestResult {
                    ok: false,
                    reachable: false,
                    status: None,
                    protocol_ok: false,
                    verdict: "unreachable",
                    detail: format!("URL not reachable: {e}"),
                    duration_ms: started.elapsed().as_millis() as u64,
                    body_snippet: String::new(),
                },
            );
        }
        Err(_) => {
            return ok_json(
                200,
                &AuthTestResult {
                    ok: false,
                    reachable: false,
                    status: None,
                    protocol_ok: false,
                    verdict: "timeout",
                    detail: format!("URL timed out after {}ms", timeout.as_millis()),
                    duration_ms: started.elapsed().as_millis() as u64,
                    body_snippet: String::new(),
                },
            );
        }
    };

    let status = resp.status().as_u16();
    let duration_ms = started.elapsed().as_millis() as u64;
    let text = resp.text().await.unwrap_or_default();
    let body_snippet: String = text.chars().take(400).collect();

    let (ok, protocol_ok, verdict, detail): (bool, bool, &'static str, String) = match status {
        401 | 403 => (
            true,
            true,
            "denied",
            "auth service rejected the simulated api-key (expected: key not found / auth failed)"
                .to_string(),
        ),
        200..=299 => {
            if crate::http::body_says_denied(&text) {
                if crate::http::json_string_field(&text, "\"reason\"")
                    .map(|r| r.trim().eq_ignore_ascii_case("insufficient_balance"))
                    .unwrap_or(false)
                {
                    (
                        true,
                        true,
                        "insufficient_balance",
                        "auth service rejected the simulated api-key (insufficient_balance — arrears gate active)"
                            .to_string(),
                    )
                } else {
                    (
                        true,
                        true,
                        "denied",
                        "auth service rejected the simulated api-key (status:false/allowed:false in body)"
                            .to_string(),
                    )
                }
            } else if !crate::http::auth_body_is_json_object(&text) {
                (
                    false,
                    true,
                    "not_json",
                    "auth service returned a non-JSON body (e.g. an HTML login/WAF page) — hydra treats a 2xx non-JSON verdict as auth_unavailable and DENIES; check the auth endpoint".to_string(),
                )
            } else {
                (
                    false,
                    true,
                    "allowed",
                    "auth service ALLOWED the simulated api-key — the URL may not be the real auth endpoint"
                        .to_string(),
                )
            }
        }
        402 => (
            true,
            true,
            "denied",
            "auth service answered 402 Payment Required (insufficient balance — a denial; endpoint usable, tenant may be in arrears)"
                .to_string(),
        ),
        404 => (
            false,
            false,
            "not_found",
            "URL returns 404 — not an auth endpoint or wrong path".to_string(),
        ),
        405 => (
            false,
            false,
            "method_not_allowed",
            "URL does not accept POST — check the protocol (GET-only endpoint?)".to_string(),
        ),
        422 => (
            false,
            true,
            "unprocessable",
            "URL rejected the auth payload (422) — not the expected auth endpoint".to_string(),
        ),
        500..=599 => (
            false,
            true,
            "server_error",
            format!("auth service returned {status} (server error)"),
        ),
        other => (
            false,
            true,
            "unexpected_status",
            format!("unexpected status {other}"),
        ),
    };

    ok_json(
        200,
        &AuthTestResult {
            ok,
            reachable: true,
            status: Some(status),
            protocol_ok,
            verdict,
            detail,
            duration_ms,
            body_snippet,
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
    /// What this node is actually serving (审核四 P4). `null` when the process
    /// did not publish a listener set (e.g. an `AdminService` built directly by
    /// a test), never "assume plaintext".
    listeners: Option<&'static crate::listeners::ActiveListeners>,
}

pub(super) async fn health(state: &AdminState, trace_id: &str) -> Resp {
    let snap = state.store.snapshot();
    // Edge nodes have no local DB (cluster P0b) — skip the DB probe.
    let (db_status, providers_count) = match &state.pool {
        Some(pool) => match crate::db::list_providers(pool).await {
            Ok(rows) => ("ok", rows.len()),
            Err(e) => {
                tracing::warn!(target: "hydra::admin", trace_id, error = %e, "health db probe failed");
                ("error", snap.providers.len())
            }
        },
        None => ("n/a", snap.providers.len()),
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
