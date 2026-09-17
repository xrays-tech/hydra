//! Endpoint handlers for the tenant self-service API.
//!
//! Each handler is a thin composition over an owner that already exists:
//!
//! | endpoint | delegates to |
//! |---|---|
//! | `whoami` (T5) | the config snapshot, through the row the gate already resolved |
//! | `invalidate` (T6) | [`crate::http::AuthCache`] plus the invalidation stream and its barrier |
//! | `usage` (T8) | [`crate::usage_query::UsageQuery`] |
//!
//! Nothing here re-implements a primitive: routing and the gate live in
//! [`super`], the response envelope in [`super::respond_json`].

use pingora_proxy::Session;
use serde::Serialize;

use crate::http::AuthChecker as _;
use crate::proxy::ctx::RequestContext;
use crate::proxy::AppState;
use crate::tenant_api::{respond_json, Authenticated};

/// The body of `GET /whoami`.
///
/// Every field is either the tenant's own non-secret configuration or a fact the
/// caller cannot otherwise see. In particular there is **no** token digest, no
/// certificate material and no provider key — a tenant reading its own state must
/// not be handed anything it could use against another tenant, and the fields it
/// does get (`domain`, `auth_url`) are the ones it would otherwise have to open a
/// support ticket to learn.
#[derive(Serialize)]
struct Whoami<'a> {
    tenant_id: &'a str,
    name: &'a str,
    enabled: bool,
    /// The domain this tenant is currently bound to — i.e. which `Host` traffic
    /// reaches it. Read-only here: changing it is an operator action.
    domain: &'a str,
    /// Where Hydra sends this tenant's clients' api-keys for verification.
    /// Read-only here for the same reason.
    auth_url: &'a str,
    /// The configuration snapshot version the authorisation was decided against.
    /// A tenant that has just asked an operator for a change can poll this field
    /// to see when the change is live instead of guessing.
    config_version: u64,
    /// The path prefix this API is served under, for the tenant's own base URL.
    base_url: String,
}

/// `GET /tenant/{tenant_id}/api/v1/whoami`
///
/// Answers entirely from the configuration snapshot: no database, no upstream,
/// no external auth. That is what makes it work identically on a leader, a
/// standby and an edge node, and it is why it costs nothing to serve — an edge
/// with no local DB answers it, which no other endpoint can.
pub async fn whoami(
    session: &mut Session,
    ctx: &mut RequestContext,
    auth: &Authenticated,
) -> pingora_core::Result<bool> {
    let t = &auth.tenant;
    let body = Whoami {
        tenant_id: &t.id,
        name: &t.name,
        enabled: t.enabled,
        domain: &t.domain,
        auth_url: &t.auth_url,
        // The version the GATE read, not a fresh one: a response must never be
        // attributed to a configuration newer than the one that authorised it.
        config_version: auth.config_version,
        base_url: format!("{}{}/api/v1", crate::tenant_api::RESERVED_PREFIX, t.id),
    };
    respond_json(session, ctx, 200, &body).await
}

// ---------------------------------------------------------------------------
// E2 — POST /auth/cache/invalidate
// ---------------------------------------------------------------------------

/// The body of `POST /auth/cache/invalidate`.
#[derive(serde::Deserialize)]
struct InvalidateRequest {
    /// Exact client api-keys to clear. Absent or empty ⇒ the whole tenant.
    ///
    /// A PREFIX is deliberately not accepted: the cache stores
    /// `(tenant_id, sha256(api_key))`, so a digest cannot be prefix-matched —
    /// "clear everything starting with sk-x" is not implementable against this
    /// data structure (design §4.2.7). Accepting a prefix and silently treating
    /// it as an exact key would be worse than refusing it.
    #[serde(default)]
    api_keys: Option<Vec<String>>,
}

/// Where the fleet stands, as reported to the tenant.
#[derive(Serialize)]
struct FleetView {
    /// `applied` | `pending` | `single_node`.
    state: &'static str,
    nodes_total: usize,
    nodes_applied: usize,
    /// Nodes that have not confirmed. Named, so an operator can act instead of
    /// guessing which node is behind.
    lagging: Vec<String>,
    /// The stream event this answer is about, for correlating with the operator's
    /// own view of the bus.
    event_id: Option<String>,
    waited_ms: u64,
}

#[derive(Serialize)]
struct InvalidateView {
    /// How many entries THIS node removed from its L1. Not a fleet count, and not
    /// a claim about other nodes — those are the `fleet` field's job.
    invalidated: usize,
    /// How many keys the request named. `invalidated == 0` with `checked > 0`
    /// means "none of them was cached here", which is not a failure: the next
    /// request for those keys goes upstream anyway.
    checked: usize,
    tenant_id: String,
    /// `keys` | `tenant`.
    scope: &'static str,
    fleet: FleetView,
}

/// `POST /tenant/{tenant_id}/api/v1/auth/cache/invalidate`
///
/// Clearing is THREE things, and it is only correct if all three happen (design
/// §4.2.2):
///
/// 1. **L1 fan-out** — this node clears its own in-memory entries *synchronously*,
///    and publishes an event so every OTHER node's consumer clears theirs. A hit
///    on a remote node's L1 never consults the L2, so deleting the shared L2 alone
///    would leave the nodes that matter still serving the stale verdict.
/// 2. **L2 authority** — the shared Redis entry is deleted here, synchronously, so
///    a cold node cannot rehydrate the very verdict this call exists to drop.
/// 3. **L3 confirmation** — the publisher waits for every live node's applied
///    watermark to reach this event, and says so. Without it the response would be
///    reporting "I enqueued something", which is what the old `published: true`
///    did while a banned key kept working elsewhere.
pub async fn invalidate(
    state: &AppState,
    session: &mut Session,
    ctx: &mut RequestContext,
    auth: &Authenticated,
) -> pingora_core::Result<bool> {
    let tenant_id = auth.tenant.id.clone();

    // --- request body -------------------------------------------------------
    let body = match crate::tenant_api::read_body(session).await {
        Ok(b) => b,
        // `read_body` reports (status, body); re-emit it through this module's
        // writer so the envelope and termination stay uniform.
        Err((status, body)) => {
            return super::respond_raw(session, ctx, status, body).await;
        }
    };
    let req: InvalidateRequest = if body.is_empty() || body.iter().all(u8::is_ascii_whitespace) {
        InvalidateRequest { api_keys: None }
    } else {
        match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => {
                return super::respond_error(
                    session,
                    ctx,
                    400,
                    "invalid_request",
                    &format!("request body is not valid JSON: {e}"),
                )
                .await
            }
        }
    };
    if let Some(resp) =
        crate::admin::handlers::invalidate_shape_error(req.api_keys.as_deref(), &ctx.trace_id)
    {
        // The admin shape check returns its own `Response<Vec<u8>>`; re-emit its
        // status and body through this module's writer so the envelope and the
        // trace header stay identical.
        let status = resp.status().as_u16();
        let body = resp.body().clone();
        return super::respond_raw(session, ctx, status, body).await;
    }

    // --- rate limit ---------------------------------------------------------
    let key = format!("invalidate:{tenant_id}");
    let now = std::time::Instant::now();
    let window = std::time::Duration::from_secs(60);
    if !state
        .tenant_api_throttle
        .allow(&key, state.tenant_api.invalidate_per_min, window, now)
    {
        let retry = state
            .tenant_api_throttle
            .retry_after_secs(&key, window, now)
            .max(1);
        ctx.status_code = 429;
        let body = serde_json::json!({
            "error": {
                "code": "rate_limited",
                "message": "too many invalidation requests for this tenant",
                "trace_id": ctx.trace_id,
            }
        });
        return super::respond_json_with_retry_after(session, ctx, 429, &body, retry).await;
    }

    // --- 1 + 2: clear HERE, synchronously ----------------------------------
    let keys = req.api_keys.clone().unwrap_or_default();
    let checked = keys.len();
    let scope = if keys.is_empty() { "tenant" } else { "keys" };
    let invalidated = if keys.is_empty() {
        state.auth.invalidate_tenant(&tenant_id).await
    } else {
        state.auth.invalidate(&tenant_id, &keys).await
    };
    crate::admin::metrics::record_auth_cache_size(state.auth.cache().len());

    // --- 3: fan out, then CONFIRM ------------------------------------------
    let started = std::time::Instant::now();
    let (fleet, status) = fan_out_and_confirm(state, &tenant_id, &keys).await;

    let view = InvalidateView {
        invalidated,
        checked,
        tenant_id: tenant_id.clone(),
        scope,
        fleet: FleetView {
            waited_ms: started.elapsed().as_millis() as u64,
            ..fleet
        },
    };
    super::respond_json(session, ctx, status, &view).await
}

/// Publish the invalidation and wait for the fleet, returning the report and the
/// HTTP status that goes with it.
///
/// Status semantics, which the tenant can act on:
/// - `200` — every live node confirmed (or there are no peers: a single node's
///   local clear IS the whole answer).
/// - `202` — the event is published but not confirmed everywhere yet. NOT an
///   error: the work is in flight, and `lagging` says where.
/// - `503` — there is a stream but it did not answer (publish or watermark read
///   failed). Fail-closed: the caller must not believe the fleet was told.
#[cfg(feature = "cluster-redis")]
async fn fan_out_and_confirm(
    state: &AppState,
    tenant_id: &str,
    keys: &[String],
) -> (FleetView, u16) {
    use crate::cluster::events::AppliedOutcome;
    let Some(stream) = &state.invalidation else {
        // NO STREAM MEANS "NOT PART OF A CLUSTER", so the local clear really is
        // the whole answer — and building with `cluster-redis` does not change
        // that. `main` leaves the stream `None` exactly when there is no Redis
        // backbone, and a `leader`/`edge` role REFUSES TO START without one, so
        // "no stream" can only be the single-node `all` role.
        //
        // Getting this backwards (reporting `unavailable`, as this code first
        // did) makes a single-node deployment built with the cluster feature
        // answer 503 to a clear that actually succeeded everywhere. Only the
        // three-feature test matrix catches that, which is why the plan mandates
        // running it. `Unavailable` is reserved for a stream that EXISTS and then
        // fails — publish error or watermark-read failure.
        return (
            FleetView {
                state: "single_node",
                nodes_total: 1,
                nodes_applied: 1,
                lagging: vec![],
                event_id: None,
                waited_ms: 0,
            },
            200,
        );
    };
    let event_id = match stream
        .publish(Some(tenant_id.to_string()), keys.to_vec())
        .await
    {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "invalidation publish failed");
            return (
                FleetView {
                    state: "unavailable",
                    nodes_total: 0,
                    nodes_applied: 0,
                    lagging: vec![],
                    event_id: None,
                    waited_ms: 0,
                },
                503,
            );
        }
    };
    let live = state
        .tenant_api
        .live_nodes
        .as_ref()
        .map_or_else(Vec::new, |f| f());
    let outcome = stream
        .await_applied(&event_id, &live, state.tenant_api.converge_timeout)
        .await;
    match outcome {
        AppliedOutcome::Applied {
            nodes_applied,
            nodes_total,
        } => (
            FleetView {
                state: "applied",
                nodes_total,
                nodes_applied,
                lagging: vec![],
                event_id: Some(event_id),
                waited_ms: 0,
            },
            200,
        ),
        AppliedOutcome::Pending {
            nodes_applied,
            nodes_total,
            lagging,
        } => (
            FleetView {
                state: "pending",
                nodes_total,
                nodes_applied,
                lagging,
                event_id: Some(event_id),
                waited_ms: 0,
            },
            202,
        ),
        AppliedOutcome::Unavailable(e) => {
            tracing::warn!(error = %e, "convergence barrier could not run");
            (
                FleetView {
                    state: "unavailable",
                    nodes_total: live.len(),
                    nodes_applied: 0,
                    lagging: live,
                    event_id: Some(event_id),
                    waited_ms: 0,
                },
                503,
            )
        }
    }
}

/// A build without `cluster-redis` cannot be a cluster: `main` refuses
/// `HYDRA_ROLE=leader|edge` without the feature. So the local clear really is the
/// whole answer, and `single_node` is a derivation rather than a guess.
#[cfg(not(feature = "cluster-redis"))]
async fn fan_out_and_confirm(
    _state: &AppState,
    _tenant_id: &str,
    _keys: &[String],
) -> (FleetView, u16) {
    (
        FleetView {
            state: "single_node",
            nodes_total: 1,
            nodes_applied: 1,
            lagging: vec![],
            event_id: None,
            waited_ms: 0,
        },
        200,
    )
}
