//! The tenant self-service API on the **data-plane listener**.
//!
//! ## Why the data plane
//!
//! The tenant's only self-service endpoint used to live on the admin service,
//! which binds `127.0.0.1` by default: a tenant could not reach it at all unless
//! the operator exposed the whole management API (every provider key, every
//! tenant row) on the same listener. The data-plane listener is the one tenants
//! already reach, already has per-tenant TLS, and is the surface they are
//! documented against. So the tenant API is a **reserved prefix on the data
//! plane**, not a second listener and not a second service.
//!
//! ## The prefix is reserved, the three paths are not
//!
//! Interception keys on `path.starts_with("/tenant/")`, NOT on "one of the three
//! known routes parsed". Matching only the three literal paths would let
//! `/tenant/t1/api/v1/typo` — and every other near miss — fall through into the
//! normal proxy pipeline: Host→tenant resolution, then the client api-key
//! extraction. The tenant's own access token travels in `Authorization: Bearer`,
//! which is ALSO a legitimate client api-key transport, so a fall-through would
//! POST that token to the tenant's `auth_url` and mask it into a usage record.
//! Anything under the reserved prefix is therefore answered here, and a path
//! that is not one of the three routes gets a local `404`.
//!
//! ## Fail-closed vocabulary
//!
//! | condition | answer |
//! |---|---|
//! | no/invalid token | `401 unauthorized` (the two are indistinguishable on purpose) |
//! | token valid, URL tenant id different | `403 tenant_id_mismatch` |
//! | this node holds no config yet | `503 not_ready` |
//! | path under the prefix, not a route | `404 not_found` |
//!
//! ## What this module owns
//!
//! Routing, the gate, and the response shape. Business logic is delegated: the
//! cache primitives to [`crate::http::AuthCache`], cache-clearing fan-out and its
//! convergence barrier to [`crate::cluster::events::InvalidationStream`], and
//! usage reads to [`crate::usage_query::UsageQuery`]. The interception in
//! `proxy::request_filter` is one call into [`dispatch`].

pub mod auth;
pub mod handlers;
pub mod throttle;
pub mod time_bound;

use hydra_core::tenant_api::{parse_route, Endpoint};
use pingora_http::ResponseHeader;
use pingora_proxy::Session;
use tracing::debug;

use crate::proxy::ctx::RequestContext;
use crate::proxy::AppState;

/// The reserved data-plane prefix. Everything under it is answered by this
/// module and never reaches the proxy pipeline.
pub const RESERVED_PREFIX: &str = "/tenant/";

/// Minimum accepted length of a tenant access token, enforced when an operator
/// sets one. The gate itself does not care, but a human-chosen short token on an
/// internet-facing port is guessable, and the minimum is the cheapest thing that
/// makes guessing impractical.
pub const MIN_TENANT_TOKEN_LEN: usize = 16;

/// Runtime configuration for the tenant API.
///
/// Deliberately minimal: every field here is one that changes behaviour. (A knob
/// that is read but ignored is the "ghost switch" this repository has been
/// bitten by — see the historical `[proxy] non_route_strategy` in `design.md`
/// §15.1 — so parameters are added by the task that makes them take effect.)
#[derive(Clone)]
pub struct TenantApiConfig {
    /// Master switch (`HYDRA_TENANT_API`, default on). When off, `request_filter`
    /// does not intercept anything under `/tenant/` and the process behaves
    /// exactly as it did before this API existed.
    pub enabled: bool,
    /// How long `POST /auth/cache/invalidate` waits for the fleet to confirm.
    /// Elapsing is not an error — it produces `202` with the lagging nodes named.
    pub converge_timeout: std::time::Duration,
    /// Cap on invalidations per tenant per minute. Each one costs the auth-cache
    /// fan-out, an upstream re-verification wave on every affected node, and — if
    /// the stream is trimmed past a lagging consumer — a fleet-wide whole-cache
    /// clear. That is too much power to hand a single tenant's credential without
    /// a ceiling.
    pub invalidate_per_min: u32,
    /// Ceiling on the E3 window, in days
    /// (`HYDRA_TENANT_API_USAGE_MAX_WINDOW_DAYS`, default 31).
    ///
    /// Not an optimisation: it is what stops one tenant's query from scanning
    /// every other tenant's rows in the store (design §5.1).
    pub usage_max_window_days: u32,
    /// The ids of the LIVE data-plane nodes, or `None` off-cluster.
    ///
    /// Injected as a closure rather than by handing `AppState` the node registry:
    /// the data plane must not gain the ability to talk to its peers (design
    /// §6.4, decision A-1). The closure reads a registry the node already
    /// maintains, and MUST filter `alive == true` — the registry also reports dead
    /// rows, and a dead node must not hold a convergence decision hostage.
    pub live_nodes: Option<std::sync::Arc<dyn Fn() -> Vec<String> + Send + Sync>>,
}

impl std::fmt::Debug for TenantApiConfig {
    /// Manual: a boxed closure has no `Debug`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantApiConfig")
            .field("enabled", &self.enabled)
            .field("converge_timeout", &self.converge_timeout)
            .field("invalidate_per_min", &self.invalidate_per_min)
            .field("usage_max_window_days", &self.usage_max_window_days)
            .field("live_nodes", &self.live_nodes.is_some())
            .finish()
    }
}

impl Default for TenantApiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            converge_timeout: std::time::Duration::from_millis(2_000),
            invalidate_per_min: 10,
            usage_max_window_days: time_bound::DEFAULT_USAGE_MAX_WINDOW_DAYS,
            live_nodes: None,
        }
    }
}

impl TenantApiConfig {
    /// Read the switch from the environment. Only `off`/`0`/`false` disable it;
    /// anything else (including an unset variable) leaves it on, so a typo cannot
    /// silently remove the API.
    #[must_use]
    pub fn from_env() -> Self {
        let enabled = match std::env::var("HYDRA_TENANT_API") {
            Ok(v) => !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "off" | "0" | "false"
            ),
            Err(_) => true,
        };
        let converge_timeout = std::env::var("HYDRA_TENANT_API_CONVERGE_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|v| *v > 0)
            .map(std::time::Duration::from_millis)
            .unwrap_or_else(|| std::time::Duration::from_millis(2_000));
        let invalidate_per_min = std::env::var("HYDRA_TENANT_API_INVALIDATE_PER_MIN")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(10);
        // A missing, unparseable or zero value falls back to the default: 0
        // would reject every request, which is a denial of service triggered by
        // a typo.
        let usage_max_window_days = std::env::var("HYDRA_TENANT_API_USAGE_MAX_WINDOW_DAYS")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(time_bound::DEFAULT_USAGE_MAX_WINDOW_DAYS);
        Self {
            enabled,
            converge_timeout,
            invalidate_per_min,
            usage_max_window_days,
            // Wired by `main` for cluster nodes; a single-node build leaves it
            // `None`, and the endpoint then reports `single_node`.
            live_nodes: None,
        }
    }
}

/// Answer a request under the reserved prefix.
///
/// Returns `Ok(true)`: the response has been written and Pingora must not dial an
/// upstream. The caller (`proxy::request_filter`) only reaches this function for
/// paths under the reserved prefix.
pub async fn dispatch(
    state: &AppState,
    session: &mut Session,
    ctx: &mut RequestContext,
    path: &str,
) -> pingora_core::Result<bool> {
    // 1. Is it one of our routes? The reserved-prefix caller has already
    //    guaranteed the prefix; a miss here is a local 404, never a fall-through.
    let Some(route) = parse_route(path) else {
        return respond_error(session, ctx, 404, "not_found", "unknown path").await;
    };

    // 1b. Method. `POST /auth/cache/invalidate` must not be reachable by GET:
    //     a method-agnostic handler would let a prefetching client, a
    //     mis-configured health check or a browser address bar clear caches.
    //     The method contract belongs to ROUTING, not to each handler, so it is
    //     checked once here.
    let expected = match route.endpoint {
        Endpoint::Whoami | Endpoint::Usage => "GET",
        Endpoint::InvalidateAuthCache => "POST",
    };
    let method = session.req_header().method.as_str();
    if method != expected {
        return respond_error(
            session,
            ctx,
            405,
            "method_not_allowed",
            &format!("{} requires {expected}", path),
        )
        .await;
    }

    // 2. Gate. No token and a wrong token are the same answer on purpose.
    let Some(bearer) = bearer_token(session) else {
        debug!(target: "hydra::tenant_api", path = %path, "no tenant token presented");
        return respond_error(
            session,
            ctx,
            401,
            "unauthorized",
            "invalid tenant access token",
        )
        .await;
    };
    let authenticated = match auth::authenticate(&state.store, bearer) {
        Ok(a) => a,
        Err(auth::AuthError::Unauthorized) => {
            debug!(target: "hydra::tenant_api", path = %path, "tenant token rejected");
            return respond_error(
                session,
                ctx,
                401,
                "unauthorized",
                "invalid tenant access token",
            )
            .await;
        }
        Err(auth::AuthError::NotReady) => {
            return respond_error(
                session,
                ctx,
                503,
                "not_ready",
                "this node has no configuration yet",
            )
            .await;
        }
    };

    // 3. The URL's tenant id is a cross-check, not an identity: the token already
    //    said who the caller is, and a mismatch is a client-side bug worth
    //    failing loudly (403, not 404 — the tenant id is in the caller's own base
    //    URL, so it is not a secret).
    if authenticated.tenant.id != route.tenant_id {
        return respond_error(
            session,
            ctx,
            403,
            "tenant_id_mismatch",
            "the token does not belong to the tenant in the URL",
        )
        .await;
    }

    // Attribute the request for logs/metrics. `ctx.selected` stays None, which is
    // what keeps this request out of `hydra_requests_total` and out of the usage
    // record: a tenant's own control-plane call must not be billed to it, and
    // querying usage must not itself count as usage.
    ctx.tenant = Some(authenticated.tenant.clone());

    // 4. Route. T4 delivers the routing skeleton: no endpoint is wired yet, so
    //    each arm answers the same local 404 the path produced before this API
    //    existed. T5/T6/T8 replace exactly one arm each with its real handler —
    //    which is why this task wires nothing: wiring E1 here would make T5's RED
    //    step unable to fail for the right reason.
    match route.endpoint {
        // T5: E1 is wired. It needs nothing but the row the gate already
        // resolved, so there is no second lookup to keep consistent.
        Endpoint::Whoami => handlers::whoami(session, ctx, &authenticated).await,
        // T6 and T8 replace these two arms, one each. Until then they answer the
        // same local 404 the path produced before this API existed — a routing
        // skeleton, not a stub: the behaviour is correct for a route that is not
        // served yet.
        // T6: E2 is wired.
        Endpoint::InvalidateAuthCache => {
            handlers::invalidate(state, session, ctx, &authenticated).await
        }
        // T8: E3 is wired.
        Endpoint::Usage => handlers::usage(state, session, ctx, &authenticated).await,
    }
}

/// The `Authorization: Bearer …` value, if present.
fn bearer_token(session: &Session) -> Option<&str> {
    session
        .req_header()
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.strip_prefix("Bearer ")
                .or_else(|| s.strip_prefix("bearer "))
        })
        .filter(|t| !t.is_empty())
}

/// What the gate resolved, re-exported so handlers take one argument instead of
/// threading three.
pub use auth::AuthenticatedTenant as Authenticated;

/// Write a JSON body with the tenant API's headers.
///
/// Terminates the response here (as `proxy::respond_catalog` does), so Pingora
/// never dials an upstream for a request under the reserved prefix.
pub(super) async fn respond_json<T: serde::Serialize>(
    session: &mut Session,
    ctx: &mut RequestContext,
    status: u16,
    body: &T,
) -> pingora_core::Result<bool> {
    ctx.status_code = status;
    let body = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    let mut header = ResponseHeader::build(status, Some(3))?;
    header.insert_header("Content-Type", "application/json")?;
    header.insert_header("X-Hydra-Trace-Id", &ctx.trace_id)?;
    header.insert_header("Content-Length", body.len().to_string())?;
    session.set_keepalive(None);
    session
        .write_response_header(Box::new(header), false)
        .await?;
    session
        .write_response_body(Some(bytes::Bytes::from(body)), true)
        .await?;
    Ok(true)
}

/// Read the whole request body, bounded.
///
/// The tenant API is a control surface: its bodies are a key list, never a
/// payload. The cap therefore exists to stop an anonymous-looking caller (the
/// gate runs before this, but the body is read before the handler decides
/// anything) from making the node buffer arbitrarily, not to accommodate large
/// legitimate requests.
pub(super) async fn read_body(session: &mut Session) -> Result<Vec<u8>, (u16, Vec<u8>)> {
    const MAX_BODY: usize = 1024 * 1024;
    let mut buf = Vec::new();
    loop {
        match session.as_downstream_mut().read_request_body().await {
            Ok(Some(chunk)) => {
                if buf.len() + chunk.len() > MAX_BODY {
                    // A small tuple, not a whole `Response`: the error is only
                    // ever re-emitted immediately, and a `Response` in the Err
                    // variant is large enough that clippy objects (rightly).
                    return Err((
                        413,
                        br#"{"error":{"code":"payload_too_large","message":"request body exceeds 1 MiB"}}"#
                            .to_vec(),
                    ));
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(buf),
            Err(e) => {
                tracing::warn!(error = %e, "tenant API: reading the request body failed");
                return Err((
                    400,
                    br#"{"error":{"code":"invalid_request","message":"could not read the request body"}}"#
                        .to_vec(),
                ));
            }
        }
    }
}

/// Write an already-built error response, re-emitting it through this module's
/// writer so the envelope, the trace header and the termination behaviour stay
/// uniform no matter which helper produced the body.
pub(super) async fn respond_raw(
    session: &mut Session,
    ctx: &mut RequestContext,
    status: u16,
    body: Vec<u8>,
) -> pingora_core::Result<bool> {
    ctx.status_code = status;
    let mut header = ResponseHeader::build(status, Some(3))?;
    header.insert_header("Content-Type", "application/json")?;
    header.insert_header("X-Hydra-Trace-Id", &ctx.trace_id)?;
    header.insert_header("Content-Length", body.len().to_string())?;
    session.set_keepalive(None);
    session
        .write_response_header(Box::new(header), false)
        .await?;
    session
        .write_response_body(Some(bytes::Bytes::from(body)), true)
        .await?;
    Ok(true)
}

/// `respond_json` plus `Retry-After`, for the rate-limited answer.
pub(super) async fn respond_json_with_retry_after<T: serde::Serialize>(
    session: &mut Session,
    ctx: &mut RequestContext,
    status: u16,
    body: &T,
    retry_after_secs: u64,
) -> pingora_core::Result<bool> {
    ctx.status_code = status;
    let body = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    let mut header = ResponseHeader::build(status, Some(4))?;
    header.insert_header("Content-Type", "application/json")?;
    header.insert_header("X-Hydra-Trace-Id", &ctx.trace_id)?;
    header.insert_header("Retry-After", retry_after_secs.to_string())?;
    header.insert_header("Content-Length", body.len().to_string())?;
    session.set_keepalive(None);
    session
        .write_response_header(Box::new(header), false)
        .await?;
    session
        .write_response_body(Some(bytes::Bytes::from(body)), true)
        .await?;
    Ok(true)
}

/// Write the shared error envelope: `{"error":{"code","message","trace_id"}}`.
///
/// The same shape the admin API uses (design §13.4), so a tenant integrating
/// against either surface parses one error model. Note the deliberate difference
/// from the proxy's own short-circuit body
/// (`{"error":{"message":…,"type":"proxy_error"}}`): ours always carries `code`
/// and `trace_id` and never `type`, which is what makes "did this request reach
/// the tenant API or the proxy pipeline?" answerable from a response alone.
async fn respond_error(
    session: &mut Session,
    ctx: &mut RequestContext,
    status: u16,
    code: &str,
    message: &str,
) -> pingora_core::Result<bool> {
    let body = serde_json::json!({
        "error": { "code": code, "message": message, "trace_id": ctx.trace_id }
    });
    respond_json(session, ctx, status, &body).await
}
