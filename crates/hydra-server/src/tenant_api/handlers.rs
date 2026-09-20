//! Endpoint handlers for the tenant self-service API.
//!
//! Each handler is a thin composition over an owner that already exists:
//!
//! | endpoint | delegates to |
//! |---|---|
//! | `whoami` (T5) | the config snapshot, through the row the gate already resolved |
//! | `invalidate` (T6) | [`crate::http::AuthCache`] plus the invalidation stream and its barrier |
//! | `usage` (T8) | [`crate::usage_query::UsageQuery`] |
//! | `sub-tenants` / `sub-tenant-routes` (T7) | the config snapshot, filtered to the tenant |
//!
//! Nothing here re-implements a primitive: routing and the gate live in
//! [`super`], the response envelope in [`super::respond_json`].

use std::collections::HashSet;

use pingora_proxy::Session;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;

use hydra_core::model::{SubTenant, SubTenantRoute};
use hydra_core::sub_tenant::SubTenantWriteError;
use hydra_core::tenant_api::TenantWriteRoute;

use crate::admin::sub_tenant_write::CoreError;
use crate::admin::tenant_config_api::{
    apply_config_write, ApplyError, TenantConfigWrite, WriteOutcome,
};
use crate::http::AuthChecker as _;
use crate::proxy::ctx::RequestContext;
use crate::proxy::AppState;
use crate::tenant_api::{respond_json, Authenticated};
// The forward path (and only it) is cluster-only; the imports stay gated so a
// single-node build has no unused-import warnings.
#[cfg(feature = "cluster-redis")]
use crate::cluster::forward::ForwardError;
#[cfg(feature = "cluster-redis")]
use crate::tenant_config::TenantConfigForwardError;

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

/// Ceiling on a per-request `timeout_ms`. Above this one caller could hold a
/// worker (and a share of the shared Redis) for as long as it liked.
const MAX_CONVERGE_MS: u64 = 60_000;

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

/// Where the fleet stands, as reported to the tenant: the SHARED
/// [`FleetReport`](crate::cluster::events::FleetReport) itself.
///
/// It is serialized directly rather than copied into a local struct: the
/// operator's `DELETE /api/v1/auth/cache` reports the same object, so a copy here
/// would be a second owner of one response shape — and two shapes that must agree
/// always eventually stop agreeing.
#[cfg(feature = "cluster-redis")]
type FleetView = crate::cluster::events::FleetReport;

/// The same shape in a build without `cluster-redis`, which cannot be a cluster
/// (`main` refuses `HYDRA_ROLE=leader|edge`), so the local clear IS the whole
/// answer. References the admin handler's shared type so the two endpoints
/// cannot drift apart.
#[cfg(not(feature = "cluster-redis"))]
type FleetView = crate::admin::handlers::Fleet;

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
    let body = match crate::tenant_api::read_body(session, &ctx.trace_id).await {
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

    // --- how long to wait for the fleet -------------------------------------
    //
    // `wait=converged` (default) blocks until every live node has applied the
    // event; `wait=none` publishes and answers `202` immediately with the
    // `event_id` for later reconciliation. `timeout_ms` overrides the startup
    // budget for this one request.
    //
    // Both are validated rather than ignored: a misspelt value that silently
    // behaved like the default would leave a caller believing it had asked for
    // (or skipped) a wait it never got.
    let params = super::time_bound::query_params(session.req_header().uri.query().unwrap_or(""));
    let param = |name: &str| {
        params
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let wait_for_fleet = match param("wait") {
        None | Some("converged") => true,
        Some("none") => false,
        Some(other) => {
            return super::respond_error(
                session,
                ctx,
                400,
                "invalid_wait",
                &format!("`wait` must be `converged` or `none`, not {other:?}"),
            )
            .await
        }
    };
    let budget = match param("timeout_ms") {
        None => Some(state.tenant_api.converge_timeout),
        Some(raw) => match raw.trim().parse::<u64>() {
            // Clamped at both ends: 0 would mean "do not wait" while claiming to
            // wait, and an unbounded value would let one caller hold a worker
            // for as long as it likes on a shared Redis.
            Ok(ms) if (1..=MAX_CONVERGE_MS).contains(&ms) => {
                Some(std::time::Duration::from_millis(ms))
            }
            _ => {
                return super::respond_error(
                    session,
                    ctx,
                    400,
                    "invalid_timeout_ms",
                    &format!("`timeout_ms` must be an integer between 1 and {MAX_CONVERGE_MS}"),
                )
                .await
            }
        },
    };

    // --- rate limit ---------------------------------------------------------
    let key = format!("invalidate:{tenant_id}");
    let now = std::time::Instant::now();
    let window = std::time::Duration::from_secs(60);
    if !state
        .tenant_api_throttle
        .allow(&key, state.tenant_api.invalidate_per_min, window, now)
    {
        crate::admin::metrics::record_tenant_api_throttled("invalidate");
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
    let (fleet, status) =
        fan_out_and_confirm(state, &tenant_id, &keys, wait_for_fleet, budget).await;

    let view = InvalidateView {
        invalidated,
        checked,
        tenant_id: tenant_id.clone(),
        scope,
        fleet,
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
    wait_for_fleet: bool,
    budget: Option<std::time::Duration>,
) -> (FleetView, u16) {
    let live = state
        .tenant_api
        .live_nodes
        .as_ref()
        .map_or_else(Vec::new, |f| f());
    let report = crate::cluster::events::broadcast_and_confirm(
        state.invalidation.as_ref(),
        Some(tenant_id.to_string()),
        keys.to_vec(),
        live,
        if wait_for_fleet { budget } else { None },
    )
    .await;
    let status = report.http_status;
    // "Did it actually get cleared everywhere, and how long did that take" is
    // the operator's question about this endpoint, and these two are its answer.
    // The report measures its own wait, so this histogram cannot disagree with
    // the `waited_ms` the tenant was just told.
    crate::admin::metrics::record_tenant_api_invalidate_converge(
        report.state,
        std::time::Duration::from_millis(report.waited_ms),
    );
    if report.state == "pending" {
        crate::admin::metrics::record_tenant_api_invalidate_pending();
    }
    (report, status)
}

/// A build without `cluster-redis` cannot be a cluster: `main` refuses
/// `HYDRA_ROLE=leader|edge` without the feature. So the local clear really is the
/// whole answer, and `single_node` is a derivation rather than a guess.
#[cfg(not(feature = "cluster-redis"))]
async fn fan_out_and_confirm(
    _state: &AppState,
    _tenant_id: &str,
    _keys: &[String],
    _wait_for_fleet: bool,
    _budget: Option<std::time::Duration>,
) -> (FleetView, u16) {
    (FleetView::single_node(), 200)
}

// ---------------------------------------------------------------------------
// E3 — GET /usage
// ---------------------------------------------------------------------------

/// The body of `GET /usage`.
///
/// The window is echoed in its **normalised** form, so a caller can see exactly
/// which rows were counted even when it sent a space-separated or epoch bound.
#[derive(Serialize)]
struct UsageView {
    /// Taken from the token, never from the query string (design §5.2).
    tenant_id: String,
    since: String,
    until: String,
    /// Newest record in the queried window, or `null` when the window has none.
    /// Never an empty string: ClickHouse answers `""` for an empty set and the
    /// decoder maps that to `None`.
    as_of: Option<String>,
    totals: hydra_core::tenant_api::UsageTotals,
    rows: Vec<hydra_core::tenant_api::UsageRow>,
    /// `none` | `model` | `provider` | `day`, echoing the accepted whitelist.
    group_by: &'static str,
    /// Which metering store answered. Reported by the reader itself, so it
    /// cannot disagree with the code that actually ran.
    source: &'static str,
}

/// `GET /tenant/{tenant_id}/api/v1/usage?since=…&until=…&group_by=…`
///
/// `since` is required and `until` defaults to now. The bounds are normalised to
/// the canonical fixed-width UTC form before they reach a store, because both
/// backends compare `created_at` **as a string** and a differently-spelled bound
/// would silently widen the window (see [`super::time_bound`]).
///
/// A read that could not be completed is a `503`, never a zeroed body: a
/// syntactically valid `{"requests":0}` is indistinguishable from "you used
/// nothing", which is the one answer this endpoint must not invent.
pub async fn usage(
    state: &AppState,
    session: &mut Session,
    ctx: &mut RequestContext,
    auth: &Authenticated,
) -> pingora_core::Result<bool> {
    // The capability, not a probe for a pool: in a cluster the leader also has a
    // local SQLite file, and it contains no usage rows at all.
    #[cfg(feature = "db")]
    {
        let Some(reader) = state.usage.as_ref() else {
            return super::respond_error(
                session,
                ctx,
                503,
                "usage_store_unavailable",
                "this node has no usage store it can read",
            )
            .await;
        };

        let tenant_id = auth.tenant.id.clone();
        let raw = session.req_header().uri.query().unwrap_or("");
        let params = super::time_bound::query_params(raw);
        let get = |name: &str| {
            params
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };

        // `group_by` is a whitelist and is validated BEFORE the store is touched:
        // the column name is interpolated into SQL, so an unknown value must
        // never reach a query builder.
        let group_by = match get("group_by") {
            None => crate::usage_query::GroupBy::None,
            Some(v) => match crate::usage_query::GroupBy::parse(v) {
                Some(g) => g,
                None => {
                    return super::respond_error(
                        session,
                        ctx,
                        400,
                        "invalid_group_by",
                        "`group_by` must be one of none, model, provider, day",
                    )
                    .await
                }
            },
        };

        let group_by_label = crate::usage_query::group_by_label(group_by);
        let started = std::time::Instant::now();
        let window = match super::time_bound::resolve(
            get("since"),
            get("until"),
            state.tenant_api.usage_max_window_days,
            super::time_bound::now(),
        ) {
            Ok(w) => w,
            Err(e) => return super::respond_error(session, ctx, 400, e.code(), &e.message()).await,
        };

        match reader
            .aggregate(&tenant_id, &window.since, &window.until, group_by)
            .await
        {
            Ok(agg) => {
                crate::admin::metrics::record_tenant_api_usage_query(
                    reader.source(),
                    group_by_label,
                    "ok",
                    started.elapsed(),
                );
                let view = UsageView {
                    tenant_id,
                    since: window.since,
                    until: window.until,
                    as_of: agg.as_of,
                    totals: agg.totals,
                    rows: agg.rows,
                    group_by: group_by_label,
                    source: reader.source(),
                };
                super::respond_json(session, ctx, 200, &view).await
            }
            Err(e) => {
                // All three map to the same 503 + code for the tenant, but the
                // MESSAGE must differ: `store_unavailable` and `decode_error` are
                // transient ("retry or contact the operator"), while a too-large
                // result is permanent for this request — retrying returns the same
                // cut-off body, so the caller must narrow the window or shrink the
                // grouping instead. The metric label keeps the three apart for the
                // operator. Turning any of these into a zeroed 200 is exactly the
                // silent lie the capability exists to prevent.
                let (result, message) = match &e {
                    crate::usage_query::UsageQueryError::StoreUnavailable(_) => (
                        "store_unavailable",
                        "the usage store could not be read; retry or contact the operator",
                    ),
                    crate::usage_query::UsageQueryError::Decode(_) => (
                        "decode_error",
                        "the usage store could not be read; retry or contact the operator",
                    ),
                    crate::usage_query::UsageQueryError::ResultTooLarge(_) => (
                        "result_too_large",
                        "the usage result exceeded the gateway's response-size cap; narrow since/until or reduce group_by",
                    ),
                };
                tracing::warn!(
                    target: "hydra::tenant_api",
                    tenant = %tenant_id,
                    source = reader.source(),
                    result,
                    error = %e,
                    "usage query failed"
                );
                crate::admin::metrics::record_tenant_api_usage_query(
                    reader.source(),
                    group_by_label,
                    result,
                    started.elapsed(),
                );
                super::respond_error(session, ctx, 503, "usage_store_unavailable", message).await
            }
        }
    }
    // A build without `db` has no `usage` field at all: the sink kind cannot be
    // `sqlite` and `main` refuses to start without a database, so the endpoint is
    // unreachable. Answering `not_ready` keeps the shape honest rather than
    // pretending to know the tenant id.
    #[cfg(not(feature = "db"))]
    {
        let _ = (state, auth);
        super::respond_error(
            session,
            ctx,
            503,
            "usage_store_unavailable",
            "this build has no usage store",
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// T7 — E4 `GET /sub-tenants` and `GET /sub-tenant-routes` (read-only,
//       snapshot-fed; an edge with no DB serves them)
// ---------------------------------------------------------------------------

/// The body of `GET /sub-tenants`.
///
/// A read-only, snapshot-fed mirror of the tenant's OWN sub-tenants. Every
/// field of a [`SubTenant`] is non-secret — the `key_prefix` is a routing
/// selector, not a credential (design §5) — so the rows are returned as-is.
/// Cross-tenant isolation is enforced by the `tenant_id` filter in
/// [`list_sub_tenants`], not by trimming fields: a tenant simply never sees rows
/// that are not its own.
#[derive(Serialize)]
struct SubTenantsView {
    /// The snapshot version the rows were read from — the same guard that
    /// supplies the data, so a tenant can reconcile its last change against this
    /// field (and against `whoami`) instead of guessing whether it is live.
    config_version: u64,
    /// The tenant's own enabled sub-tenants, in config order.
    sub_tenants: Vec<SubTenant>,
}

/// The body of `GET /sub-tenant-routes`.
#[derive(Serialize)]
struct SubTenantRoutesView {
    /// The snapshot version the rows were read from (v2 reconciliation baseline).
    config_version: u64,
    /// The tenant's own enabled sub-tenant routes, in config order. A route is
    /// attributed to the tenant THROUGH its sub-tenant's `tenant_id`, so a route
    /// whose sub-tenant belongs to another tenant can never appear here — even
    /// though the route row itself names no `tenant_id`.
    sub_tenant_routes: Vec<SubTenantRoute>,
}

/// `GET /tenant/{tenant_id}/api/v1/sub-tenants`
///
/// Read-only and snapshot-fed: it reads ONLY the replication snapshot
/// (`store.replication()`), never the database, so an edge with no local DB
/// serves it exactly like [`whoami`]. Only the rows whose `tenant_id` equals the
/// authenticated tenant are returned — a tenant can never read another tenant's
/// sub-tenants.
pub async fn list_sub_tenants(
    state: &AppState,
    session: &mut Session,
    ctx: &mut RequestContext,
    auth: &Authenticated,
) -> pingora_core::Result<bool> {
    // ONE atomic read: the rows and the version they are attributed to come from
    // the SAME guard, so a response can never mix data from one generation with
    // the version of another. The gate already authenticated (which requires a
    // snapshot), so `None` is not expected; fail closed rather than invent an
    // empty list.
    let guard = state.store.replication();
    let Some(content) = guard.as_ref() else {
        return super::respond_error(
            session,
            ctx,
            503,
            "not_ready",
            "this node has no configuration yet",
        )
        .await;
    };
    let tenant_id = &auth.tenant.id;
    let sub_tenants: Vec<SubTenant> = content
        .cfg
        .sub_tenants
        .iter()
        .filter(|st| st.tenant_id == *tenant_id)
        .cloned()
        .collect();
    let view = SubTenantsView {
        config_version: content.version,
        sub_tenants,
    };
    respond_json(session, ctx, 200, &view).await
}

/// `GET /tenant/{tenant_id}/api/v1/sub-tenant-routes`
///
/// Read-only and snapshot-fed, like [`list_sub_tenants`]. A route names only its
/// `sub_tenant_id`, so tenant scoping is done in two steps: collect the ids of
/// the tenant's own sub-tenants, then keep only routes whose sub-tenant is in
/// that set. A route belonging to another tenant's sub-tenant cannot match.
pub async fn list_sub_tenant_routes(
    state: &AppState,
    session: &mut Session,
    ctx: &mut RequestContext,
    auth: &Authenticated,
) -> pingora_core::Result<bool> {
    let guard = state.store.replication();
    let Some(content) = guard.as_ref() else {
        return super::respond_error(
            session,
            ctx,
            503,
            "not_ready",
            "this node has no configuration yet",
        )
        .await;
    };
    let tenant_id = &auth.tenant.id;
    let own_sub_tenants: HashSet<&str> = content
        .cfg
        .sub_tenants
        .iter()
        .filter(|st| st.tenant_id == *tenant_id)
        .map(|st| st.id.as_str())
        .collect();
    let sub_tenant_routes: Vec<SubTenantRoute> = content
        .cfg
        .sub_tenant_routes
        .iter()
        .filter(|r| own_sub_tenants.contains(r.sub_tenant_id.as_str()))
        .cloned()
        .collect();
    let view = SubTenantRoutesView {
        config_version: content.version,
        sub_tenant_routes,
    };
    respond_json(session, ctx, 200, &view).await
}

// ---------------------------------------------------------------------------
// V5 — D9 data-plane tenant config writes (PUT/DELETE sub-tenants & routes)
// ---------------------------------------------------------------------------
//
// The four self-service WRITE endpoints (sub-tenant v2, D9). The gate (token,
// URL cross-check, success budget) already ran in [`super::dispatch`], so the
// authenticated tenant IS the write target. From here the request is EITHER
// forwarded to the lease-holding leader (a cluster node, D1/D2) OR applied
// locally (a single-node node, D8):
//
// - a **cluster** node forwards to the leader's internal endpoint
//   ([`crate::tenant_config::TenantConfigForwarder::forward_config_write`])
//   and relays the leader's status + body; and
// - a **single-node** node (no forwarder) applies the write locally through
//   the shared write core ([`apply_config_write`]) and reloads the snapshot so
//   `config_version` advances.
//
// Both faces run the SAME A-2 binding (the write target must be the
// authenticated tenant's own resources) — the binding is enforced inside
// [`apply_config_write`] for the local path and by the leader for the forward
// path — so they cannot diverge. The tenant Bearer and the request body are
// NEVER logged here.

/// Default for the optional `enabled` field: `true`.
fn default_true() -> bool {
    true
}

/// The body of `PUT .../sub-tenants/{name}` (D9): `{key_prefix?, enabled}`.
/// `tenant_id` and `name` come from the URL, never the body.
#[derive(Deserialize)]
struct SubTenantUpsertBody {
    #[serde(default)]
    key_prefix: Option<String>,
    #[serde(default = "default_true")]
    enabled: bool,
}

/// The body of `PUT .../sub-tenant-routes` (D9):
/// `{sub_tenant_id, model_key?, provider_id, enabled}`.
#[derive(Deserialize)]
struct RouteUpsertBody {
    sub_tenant_id: String,
    #[serde(default)]
    model_key: Option<String>,
    provider_id: String,
    #[serde(default = "default_true")]
    enabled: bool,
}

/// A JSON body that is empty / whitespace-only is treated as `{}`.
fn parse_body<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, String> {
    let bytes = if body.is_empty() || body.iter().all(u8::is_ascii_whitespace) {
        b"{}"
    } else {
        body
    };
    serde_json::from_slice(bytes).map_err(|e| e.to_string())
}

/// The four D9 write endpoints: gate (already done) → binding → (forward |
/// local write).
pub async fn write(
    state: &AppState,
    session: &mut Session,
    ctx: &mut RequestContext,
    auth: &Authenticated,
    route: TenantWriteRoute<'_>,
) -> pingora_core::Result<bool> {
    let tenant_id = auth.tenant.id.clone();
    let trace_id = ctx.trace_id.clone();

    // (1) Read + parse the body (PUT only; DELETE carries none) and build the
    //     shared [`TenantConfigWrite`] (the A-2 "authorised work"). The forward
    //     face (a cluster node) derives its internal request FROM this value, so
    //     the forward and local faces cannot disagree on what is written.
    let config_write = match route {
        TenantWriteRoute::UpsertSubTenant { name, .. } => {
            let raw = match super::read_body(session, &trace_id).await {
                Ok(b) => b,
                Err((status, body)) => {
                    return super::respond_raw(session, ctx, status, body).await;
                }
            };
            let req: SubTenantUpsertBody = match parse_body(&raw) {
                Ok(r) => r,
                Err(e) => {
                    return super::respond_error(
                        session,
                        ctx,
                        400,
                        "invalid_request",
                        &format!("request body is not valid JSON: {e}"),
                    )
                    .await;
                }
            };
            TenantConfigWrite::UpsertSubTenant {
                tenant_id: tenant_id.clone(),
                name: name.to_string(),
                key_prefix: req.key_prefix,
                enabled: req.enabled,
            }
        }
        TenantWriteRoute::DeleteSubTenant { id, .. } => {
            TenantConfigWrite::DeleteSubTenant { id: id.to_string() }
        }
        TenantWriteRoute::UpsertRoute { .. } => {
            let raw = match super::read_body(session, &trace_id).await {
                Ok(b) => b,
                Err((status, body)) => {
                    return super::respond_raw(session, ctx, status, body).await;
                }
            };
            let req: RouteUpsertBody = match parse_body(&raw) {
                Ok(r) => r,
                Err(e) => {
                    return super::respond_error(
                        session,
                        ctx,
                        400,
                        "invalid_request",
                        &format!("request body is not valid JSON: {e}"),
                    )
                    .await;
                }
            };
            TenantConfigWrite::UpsertRoute {
                sub_tenant_id: req.sub_tenant_id,
                model_key: req.model_key,
                provider_id: req.provider_id,
                enabled: req.enabled,
            }
        }
        TenantWriteRoute::DeleteRoute { id, .. } => {
            TenantConfigWrite::DeleteRoute { id: id.to_string() }
        }
    };

    // (2) V6 / D6 (A-2 6): the per-tenant config-write throttle is NOT applied
    //     on this data-plane path. It is enforced by the leader's internal
    //     endpoint (`admin::tenant_config_api::throttle_gate`), which every
    //     forwarded write reaches; a single-node local write is bounded by the
    //     general tenant-API per-tenant request budget instead. Do NOT add a
    //     second throttle here: it would double-meter forwarded writes.

    // (3) Dispatch: a cluster node forwards to the lease-holding leader; a
    //     single-node node (no forwarder) applies locally (D8).
    #[cfg(feature = "cluster-redis")]
    if let Some(fwd) = state.tenant_config_forwarder() {
        // The Bearer the gate already validated (D2): forward it in the
        // dedicated `x-hydra-tenant-token` header, never `Authorization`, never
        // the body, never logged.
        let bearer = super::bearer_token(session).unwrap_or_default().to_string();
        let (method, internal_path, internal_body) = internal_request(&config_write);
        let body = serde_json::to_vec(&internal_body).unwrap_or_default();
        return forward_write(
            session,
            ctx,
            fwd,
            &method,
            &internal_path,
            &bearer,
            body,
            &trace_id,
        )
        .await;
    }

    local_write(session, ctx, state, &config_write, &tenant_id, &trace_id).await
}

/// The internal endpoint (method, path, JSON body) a data-plane write maps to.
/// Derived from the shared [`TenantConfigWrite`] so the forward face cannot
/// disagree with the local face on what is written.
#[cfg(feature = "cluster-redis")]
fn internal_request(write: &TenantConfigWrite) -> (String, String, serde_json::Value) {
    match write {
        TenantConfigWrite::UpsertSubTenant {
            tenant_id,
            name,
            key_prefix,
            enabled,
        } => (
            "PUT".to_string(),
            "/api/v1/internal/tenant-config/sub-tenants".to_string(),
            json!({
                "tenant_id": tenant_id,
                "name": name,
                "key_prefix": key_prefix,
                "enabled": enabled,
            }),
        ),
        TenantConfigWrite::DeleteSubTenant { id } => (
            "DELETE".to_string(),
            format!("/api/v1/internal/tenant-config/sub-tenants/{id}"),
            json!({}),
        ),
        TenantConfigWrite::UpsertRoute {
            sub_tenant_id,
            model_key,
            provider_id,
            enabled,
        } => (
            "PUT".to_string(),
            "/api/v1/internal/tenant-config/sub-tenant-routes".to_string(),
            json!({
                "sub_tenant_id": sub_tenant_id,
                "model_key": model_key,
                "provider_id": provider_id,
                "enabled": enabled,
            }),
        ),
        TenantConfigWrite::DeleteRoute { id } => (
            "DELETE".to_string(),
            format!("/api/v1/internal/tenant-config/sub-tenant-routes/{id}"),
            json!({}),
        ),
    }
}

/// D8 single-node: the node is the only writer. Apply the write through the
/// shared write core ([`apply_config_write`]), reload the snapshot so
/// `config_version` advances, and answer with the produced resource (or the
/// idempotent delete marker).
async fn local_write(
    session: &mut Session,
    ctx: &mut RequestContext,
    state: &AppState,
    write: &TenantConfigWrite,
    tenant_id: &str,
    trace_id: &str,
) -> pingora_core::Result<bool> {
    // D8: a single-node node always has a local DB. An absent pool means this
    // is not the writer (e.g. an edge that should have forwarded) — fail closed.
    let Some(pool) = state.store.pool() else {
        return super::respond_error(
            session,
            ctx,
            503,
            "not_ready",
            "this node has no local database to apply the write",
        )
        .await;
    };
    // The current config (provider / model / tenant membership) the write is
    // validated against — the same snapshot the gate read.
    let cfg = std::sync::Arc::clone(&*state.store.snapshot());
    match apply_config_write(pool, &cfg, tenant_id, write).await {
        Ok(outcome) => {
            // Write-after consistency: reload so the snapshot (and
            // `config_version`) reflects the committed write. Best-effort: a
            // failure is logged, not fatal (the write committed; the next reload
            // recovers).
            if let Err(e) = state.store.reload_all().await {
                tracing::warn!(
                    target: "hydra::tenant_api",
                    tenant = %tenant_id,
                    trace_id = %trace_id,
                    error = %e,
                    "post-write reload_all failed; the in-memory snapshot is now stale"
                );
            }
            let version = state.store.version();
            match outcome {
                WriteOutcome::SubTenant(st) => {
                    super::respond_json(
                        session,
                        ctx,
                        200,
                        &json!({ "sub_tenant": st, "config_version": version }),
                    )
                    .await
                }
                WriteOutcome::Route(r) => {
                    super::respond_json(
                        session,
                        ctx,
                        200,
                        &json!({ "route": r, "config_version": version }),
                    )
                    .await
                }
                // Idempotent delete: 204, no body.
                WriteOutcome::Deleted(_) => super::respond_raw(session, ctx, 204, Vec::new()).await,
            }
        }
        Err(e) => {
            // The same variant → the same status/code as the internal face,
            // rendered in the data-plane envelope.
            let (status, code, message) = match e {
                ApplyError::TenantMismatch => (
                    403,
                    "tenant_id_mismatch",
                    "the write target tenant does not match the authenticated tenant".to_string(),
                ),
                ApplyError::NotFound => (404, "not_found", "not found".to_string()),
                ApplyError::Core(core) => map_core_err(&core),
            };
            super::respond_error(session, ctx, status, code, &message).await
        }
    }
}

/// Map the shared write core's [`CoreError`] to (status, code, message) for the
/// data-plane envelope. The status / code are IDENTICAL to the internal face
/// (`admin::tenant_config_api::core_err_resp`); only the rendering differs.
fn map_core_err(e: &CoreError) -> (u16, &'static str, String) {
    match e {
        CoreError::Validation(v) => {
            let (status, code) = match v {
                SubTenantWriteError::NameDuplicate => (409, "name_duplicate"),
                SubTenantWriteError::PrefixDuplicate => (409, "prefix_duplicate"),
                SubTenantWriteError::ProviderNotFound => (400, "provider_not_found"),
                SubTenantWriteError::ProviderNotInTenant => (400, "provider_not_in_tenant"),
                SubTenantWriteError::ModelNotInTenant => (400, "model_not_in_tenant"),
                SubTenantWriteError::ModelNotServedByProvider => {
                    (400, "model_not_served_by_provider")
                }
                SubTenantWriteError::NameInvalid => (400, "invalid_name"),
                SubTenantWriteError::PrefixEmpty => (400, "empty_key_prefix"),
                SubTenantWriteError::PrefixNonAscii => (400, "invalid_key_prefix"),
                SubTenantWriteError::PrefixNoSeparator => (400, "invalid_key_prefix"),
                SubTenantWriteError::PrefixOverlap => (400, "key_prefix_overlap"),
                SubTenantWriteError::SubTenantQuotaExceeded => (400, "quota_exceeded"),
                SubTenantWriteError::RouteQuotaExceeded => (400, "quota_exceeded"),
            };
            (status, code, v.to_string())
        }
        CoreError::Db(d) => (500, "database_error", d.to_string()),
        CoreError::PrefixGenerationFailed => (
            400,
            "prefix_generation_failed",
            "could not generate a non-conflicting key_prefix after 5 attempts".to_string(),
        ),
        CoreError::NotFound => (404, "not_found", "not found".to_string()),
    }
}

/// D1/D2: forward the tenant config write to the lease-holding leader's
/// internal endpoint and relay its status + body. The tenant Bearer travels in
/// the dedicated `x-hydra-tenant-token` header (never `Authorization`, never
/// the body, never logged) — see
/// [`crate::tenant_config::TenantConfigForwarder::forward_config_write`].
#[cfg(feature = "cluster-redis")]
async fn forward_write(
    session: &mut Session,
    ctx: &mut RequestContext,
    fwd: &crate::tenant_config::TenantConfigForwarder,
    method: &str,
    internal_path: &str,
    tenant_bearer: &str,
    body: Vec<u8>,
    trace_id: &str,
) -> pingora_core::Result<bool> {
    match fwd
        .forward_config_write(method, internal_path, tenant_bearer, body, trace_id)
        .await
    {
        // The leader is the single writer; relay its verdict verbatim (status +
        // body) so the data plane and the internal face never disagree.
        Ok(resp) => {
            super::respond_raw(session, ctx, resp.status().as_u16(), resp.into_body()).await
        }
        // A timeout / response-read failure is AMBIGUOUS (the write may have
        // landed): 504, re-read before retrying. A connect failure is DEFINITE
        // (nothing left this node): 502. No leader is a fail-closed 503.
        Err(e) => {
            let (status, code, message) = match e {
                TenantConfigForwardError::NoLeader => (
                    503,
                    "no_leader",
                    "no leader is resolvable to apply this write; the write was not applied"
                        .to_string(),
                ),
                TenantConfigForwardError::Forward(ForwardError::Timeout { secs }) => (
                    504,
                    "forward_result_unknown",
                    format!(
                        "the leader did not answer within {secs}s; the write may or may not \
                         have been applied — re-read the resource before retrying"
                    ),
                ),
                TenantConfigForwardError::Forward(ForwardError::AfterResponse(reason)) => (
                    504,
                    "forward_result_unknown",
                    format!(
                        "the leader answered but the response could not be read ({reason}); the \
                         write may or may not have been applied — re-read the resource before \
                         retrying"
                    ),
                ),
                // `Other`'s reason embeds the reqwest error, which includes the
                // leader's control-plane URL. Never relay that to a TENANT:
                // the transport detail stays in the leader-facing logs, and the
                // tenant gets a stable message.
                TenantConfigForwardError::Forward(ForwardError::Other(_)) => (
                    502,
                    "forward_failed",
                    "failed to reach the leader; the request was not applied".to_string(),
                ),
            };
            super::respond_error(session, ctx, status, code, &message).await
        }
    }
}
