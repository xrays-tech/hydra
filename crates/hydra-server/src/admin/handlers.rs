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
    LimitRole, Provider, ProviderKey, ProviderKeyBinding, ProviderModel, Tenant, TenantModel,
    TenantProvider,
};
use pingora_core::protocols::http::ServerSession;
use serde::{Deserialize, Serialize};

use super::AdminState;
use crate::admin::metrics;
use crate::cluster::snapshot::SnapshotWire;
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
pub(super) async fn read_body(session: &mut ServerSession) -> Vec<u8> {
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
        match crate::db::list_providers(state.db()).await {
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
            let body = read_body(session).await;
            let mut p: Provider = match parse_body(&body, trace_id) {
                Ok(p) => p,
                Err(r) => return r,
            };
            p.id = id.to_string();
            p.updated_at = now_ts();
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
        let body = read_body(session).await;
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
            let body = read_body(session).await;
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
        match crate::db::insert_provider_key(state.db(), state.key_provider.as_ref(), &k).await {
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
            match crate::db::get_provider_key(state.db(), state.key_provider.as_ref(), id).await {
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
            let _ = crate::db::delete_provider_key(state.db(), id).await;
            match crate::db::insert_provider_key(state.db(), state.key_provider.as_ref(), &k).await
            {
                Ok(()) => {}
                Err(e) => return db_err_resp(e, trace_id),
            }
            reload_best_effort(state, trace_id).await;
            k.api_key = hydra_core::rewrite::mask_key(&k.api_key);
            ok_json(200, &k)
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
}

impl TenantView {
    fn new(tenant: Tenant, has_access_token: bool) -> Self {
        Self {
            tenant,
            has_access_token,
        }
    }
}

/// SHA-256 of `s` as a lowercase hex string (the `_hex`-suffixed core
/// helper returns the raw `[u8; 32]` — the suffix is historical).
fn sha256_hex_str(s: &str) -> String {
    sha256_hex(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time byte comparison (no timing side-channel on the token).
fn constant_time_eq(a: &str, b: &str) -> bool {
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

/// Persist the resolved certificate write for a tenant (post-insert; the
/// tenant row must already exist — migration 0007 makes the DB self-contained).
#[allow(clippy::result_large_err)]
async fn apply_tenant_cert_write(
    state: &AdminState,
    tenant_id: &str,
    action: CertWrite,
    trace_id: &str,
) -> Result<(), Resp> {
    let cert = match action {
        CertWrite::None => return Ok(()),
        CertWrite::Clear => crate::db::TenantCert {
            tenant_id: tenant_id.to_string(),
            cert_pem: None,
            cert_key_pem: None,
        },
        CertWrite::Content {
            cert_pem,
            cert_key_pem,
        } => crate::db::TenantCert {
            tenant_id: tenant_id.to_string(),
            cert_pem: Some(cert_pem),
            cert_key_pem: Some(cert_key_pem),
        },
    };
    crate::db::update_tenant_cert(state.db(), state.key_provider.as_ref(), &cert)
        .await
        .map_err(|e| db_err_resp(e, trace_id))?;
    Ok(())
}

/// Apply a tenant access-token write resolved from an upsert body:
/// - `Some(non-empty)` → set / rotate (stored as SHA-256 hex);
/// - `Some("")` → clear the token;
/// - `None` → keep the current token (no change).
///
/// Runs AFTER the tenant row write (like the cert write) so a 4xx (e.g. a
/// too-short token) is returned before the row is persisted — see the
/// no-zombie-create rule in [`apply_tenant_cert_write`].
#[allow(clippy::result_large_err)]
async fn apply_tenant_access_token_write(
    state: &AdminState,
    tenant_id: &str,
    access_token: &Option<String>,
    trace_id: &str,
) -> Result<(), Resp> {
    let Some(raw) = access_token else {
        return Ok(());
    };
    let token = raw.trim();
    if token.is_empty() {
        crate::db::set_tenant_access_token_hash(state.db(), tenant_id, None)
            .await
            .map_err(|e| db_err_resp(e, trace_id))?;
        return Ok(());
    }
    if token.len() < 16 {
        return Err(err_json(
            400,
            "invalid_access_token",
            "access_token must be at least 16 characters",
            trace_id,
        ));
    }
    let hash = sha256_hex_str(token);
    crate::db::set_tenant_access_token_hash(state.db(), tenant_id, Some(&hash))
        .await
        .map_err(|e| db_err_resp(e, trace_id))?;
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
                let with_token: HashSet<String> = crate::db::list_tenant_access_token_hashes(state.db())
                    .await
                    .map(|pairs| pairs.into_iter().map(|(id, _)| id).collect())
                    .unwrap_or_default();
                let views: Vec<TenantView> = rows
                    .into_iter()
                    .map(|t| {
                        let has = with_token.contains(&t.id);
                        TenantView::new(t, has)
                    })
                    .collect();
                ok_json(200, &views)
            }
            Err(e) => db_err_resp(e, trace_id),
        }
    } else if method == "POST" {
        let body = read_body(session).await;
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
        match crate::db::insert_tenant(state.db(), &t).await {
            Ok(()) => {}
            Err(e) => return db_err_resp(e, trace_id),
        }
        if let Err(resp) = apply_tenant_cert_write(state, &t.id, cert_action, trace_id).await {
            return resp;
        }
        if let Err(resp) = apply_tenant_access_token_write(state, &t.id, &up.access_token, trace_id).await {
            return resp;
        }
        let has = crate::db::tenant_has_access_token(state.db(), &t.id)
            .await
            .unwrap_or(false);
        reload_best_effort(state, trace_id).await;
        ok_json(201, &TenantView::new(t, has))
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
                ok_json(200, &TenantView::new(t, has))
            }
            Err(e) if is_not_found(&e) => err_json(404, "not_found", "tenant not found", trace_id),
            Err(e) => db_err_resp(e, trace_id),
        },
        "PUT" => {
            let body = read_body(session).await;
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
            match crate::db::update_tenant(state.db(), &t).await {
                Ok(()) => {}
                Err(e) => return db_err_resp(e, trace_id),
            }
            if let Err(resp) = apply_tenant_cert_write(state, &t.id, cert_action, trace_id).await {
                return resp;
            }
            if let Err(resp) = apply_tenant_access_token_write(state, &t.id, &up.access_token, trace_id).await {
                return resp;
            }
            match crate::db::get_tenant(state.db(), id).await {
                Ok(t) => {
                    let has = crate::db::tenant_has_access_token(state.db(), id)
                        .await
                        .unwrap_or(false);
                    reload_best_effort(state, trace_id).await;
                    ok_json(200, &TenantView::new(t, has))
                }
                Err(_) => err_json(404, "not_found", "tenant not found", trace_id),
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
        let body = read_body(session).await;
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
        let body = read_body(session).await;
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
            let body = read_body(session).await;
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
    // An empty body means "invalidate everything" — tolerate it instead of
    // failing the parse (curl -X DELETE with no body must work).
    let body = read_body(session).await;
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
        (None, None) => 0,
    };
    // Broadcast the invalidation cluster-wide (P4): every node drops the
    // affected local cache entries via the stream; the L2 entries they
    // re-hydrate from are gone too (they were deleted below on this node).
    #[cfg(feature = "cluster-redis")]
    if let Some(stream) = &state.invalidation {
        if let Err(e) = stream
            .publish(
                req.tenant_id.clone(),
                req.api_keys.clone().unwrap_or_default(),
            )
            .await
        {
            tracing::warn!(error = %e, "invalidation publish failed");
        }
    }
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

/// Resolve the tenant id owning `bearer` (the tenant self-service access
/// token, migration 0009): SHA-256 of the presented token compared
/// constant-time against the stored hashes. `None` ⇒ unknown / missing /
/// unconfigured token (fail-closed 401).
pub(super) async fn tenant_id_for_token(
    state: &AdminState,
    bearer: &str,
) -> Option<String> {
    let presented = sha256_hex_str(bearer);
    let pool = state.pool.as_ref()?;
    let pairs = crate::db::list_tenant_access_token_hashes(pool).await.ok()?;
    for (tid, stored) in pairs {
        if constant_time_eq(&presented, &stored) {
            return Some(tid);
        }
    }
    None
}

/// Body of the tenant self-service invalidation endpoint (optional).
#[derive(Deserialize)]
struct TenantCacheInvalidateRequest {
    /// api-keys to invalidate for the tenant; absent/empty ⇒ clear ALL of
    /// the tenant's cached auth decisions.
    #[serde(default)]
    api_keys: Option<Vec<String>>,
}

/// Tenant self-service auth-cache invalidation (migration 0009): clears the
/// authenticated tenant's own cached auth decisions — 欠费停机 / 付费恢复等
/// 场景。The tenant id is NOT client-chosen here: the router resolved it from
/// the access token and already verified it matches the URL's tenant_id.
pub(super) async fn tenant_auth_cache_invalidate(
    state: &AdminState,
    session: &mut ServerSession,
    tenant_id: &str,
    trace_id: &str,
) -> Resp {
    let body = read_body(session).await;
    let req: TenantCacheInvalidateRequest = if body.is_empty() || body.iter().all(u8::is_ascii_whitespace) {
        TenantCacheInvalidateRequest { api_keys: None }
    } else {
        match parse_body(&body, trace_id) {
            Ok(r) => r,
            Err(r) => return r,
        }
    };
    let count = match req.api_keys.as_deref() {
        Some(keys) if !keys.is_empty() => state.auth.invalidate(tenant_id, keys).await,
        _ => state.auth.invalidate_tenant(tenant_id).await,
    };
    // Broadcast the invalidation cluster-wide (P4), mirroring the admin
    // auth-cache endpoint: every node drops the affected local cache entries.
    #[cfg(feature = "cluster-redis")]
    if let Some(stream) = &state.invalidation {
        if let Err(e) = stream
            .publish(
                Some(tenant_id.to_string()),
                req.api_keys.clone().unwrap_or_default(),
            )
            .await
        {
            tracing::warn!(error = %e, "invalidation publish failed");
        }
    }
    metrics::record_auth_cache_size(state.auth.cache().len());
    ok_json(
        200,
        &InvalidateResponse {
            invalidated: count,
            tenant_id: Some(tenant_id.to_string()),
        },
    )
}

/// Simulated probe against a tenant `auth_url`: POSTs the exact request the
/// proxy would send (same headers + body via `http::auth_request_body`) with a
/// clearly fake api-key and reports whether the endpoint is usable. A fake key
/// MUST be denied — so "api-key not found" / "auth failed" (401/403, or an
/// explicit `status:false`/`allowed:false` in a 2xx body) is a PASS; an allow,
/// a 422, a 404/405, a 5xx or an unreachable URL is a FAIL.
pub(super) async fn tenant_auth_test(
    state: &AdminState,
    session: &mut ServerSession,
    trace_id: &str,
) -> Resp {
    let body = read_body(session).await;
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
                (
                    true,
                    true,
                    "denied",
                    "auth service rejected the simulated api-key (status:false/allowed:false in body)"
                        .to_string(),
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
// Cluster status (cluster P4) — whole-fleet view for the Admin UI Health page
// ===========================================================================

/// One fleet node as rendered by `GET /api/v1/cluster/status`.
#[derive(Serialize)]
struct ClusterNodeDto {
    node_id: String,
    role: String,
    control_url: String,
    alive: bool,
    is_lease_holder: bool,
    is_self: bool,
}

/// Whole-cluster status. `cluster=false` on the single-node build / default
/// mode — the UI then renders just the local Health panel.
#[derive(Serialize)]
struct ClusterStatusDto {
    cluster: bool,
    mode: String,
    node_id: String,
    this_node_leader: bool,
    lease_holder: Option<String>,
    nodes: Vec<ClusterNodeDto>,
}

/// `GET /api/v1/cluster/status` (admin-token gated): fleet nodes from the
/// registry (role + control URL + heartbeat liveness) plus the current
/// leader-lease holder. Single-node mode reports `cluster=false`.
#[allow(unused_variables)] // params are unused on the single-node build
pub(super) async fn cluster_status(state: &AdminState, trace_id: &str) -> Resp {
    #[cfg(feature = "cluster-redis")]
    {
        let Some(registry) = &state.cluster_registry else {
            return ok_json(200, &single_node_status());
        };
        let (nodes, holder) = match (registry.list_nodes().await, registry.lease_holder().await) {
            (Ok(nodes), Ok(holder)) => (nodes, holder),
            _ => {
                return err_json(
                    502,
                    "cluster_unavailable",
                    "cannot read the cluster registry (Redis unreachable?)",
                    trace_id,
                );
            }
        };
        let self_id = registry.node_id().to_string();
        let mode = match registry.role() {
            crate::cluster::NodeRole::Leader => "leader",
            crate::cluster::NodeRole::Edge => "edge",
            crate::cluster::NodeRole::All => "all",
        }
        .to_string();
        let dto = ClusterStatusDto {
            cluster: true,
            mode,
            this_node_leader: holder.as_deref() == Some(registry.node_id()),
            node_id: self_id.clone(),
            lease_holder: holder.clone(),
            nodes: nodes
                .into_iter()
                .map(|n| ClusterNodeDto {
                    is_lease_holder: holder.as_deref() == Some(n.node_id.as_str()),
                    is_self: n.node_id == self_id,
                    node_id: n.node_id,
                    role: n.role,
                    control_url: n.control_url,
                    alive: n.alive,
                })
                .collect(),
        };
        ok_json(200, &dto)
    }
    #[cfg(not(feature = "cluster-redis"))]
    {
        ok_json(200, &single_node_status())
    }
}

/// The single-node status payload (shared by both cfg branches).
fn single_node_status() -> ClusterStatusDto {
    ClusterStatusDto {
        cluster: false,
        mode: "single".to_string(),
        node_id: String::new(),
        this_node_leader: false,
        lease_holder: None,
        nodes: Vec::new(),
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
        },
    )
}

// ===========================================================================
// Internal control plane (cluster P1) — snapshot distribution
// ===========================================================================

/// Control-channel response: `snapshot` is present only when the caller's
/// `since` is older than the current version.
#[derive(Serialize)]
struct InternalControlResponse {
    version: u64,
    snapshot: Option<SnapshotWire>,
}

/// `GET /api/v1/internal/control?since=N` (cluster-token gated): serve the
/// current config snapshot (secrets sealed, versioned) to edge/standby
/// nodes. `snapshot` is `null` when the caller is already current.
pub(super) async fn internal_control(
    state: &AdminState,
    query: Option<&str>,
    trace_id: &str,
) -> Resp {
    let since: u64 = query
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("since=")))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let current = state.store.version();
    if since >= current {
        return ok_json(
            200,
            &InternalControlResponse {
                version: current,
                snapshot: None,
            },
        );
    }
    let cfg: hydra_core::config::ConfigData =
        hydra_core::config::ConfigData::clone(&state.store.snapshot());
    match SnapshotWire::build(current, cfg, state.db(), state.key_provider.as_ref()).await {
        Ok(snapshot) => ok_json(
            200,
            &InternalControlResponse {
                version: current,
                snapshot: Some(snapshot),
            },
        ),
        Err(e) => err_json(
            500,
            "snapshot_build_failed",
            &format!("control snapshot build failed: {e}"),
            trace_id,
        ),
    }
}

/// `GET /healthz/leader` (cluster P2): 200 while this node holds the leader
/// lease, 503 on standby, 404 on non-candidate nodes (`all` / edge).
pub(super) fn leader_health(state: &AdminState, trace_id: &str) -> Resp {
    match &state.leader_ready {
        Some(f) if f() => ok_json(200, &LeaderHealth { leader: true }),
        Some(_) => err_json(
            503,
            "not_leader",
            "this node is not the active leader",
            trace_id,
        ),
        None => err_json(
            404,
            "not_found",
            "leader health is only available on leader-candidate nodes",
            trace_id,
        ),
    }
}

#[derive(Serialize)]
struct LeaderHealth {
    leader: bool,
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
