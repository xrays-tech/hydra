//! Per-request context (`RequestContext`) shared across Pingora hooks.
//!
//! Holds everything `request_filter` decides (tenant, auth verdict, route
//! candidates, selected provider/key) plus the incremental usage scanner
//! populated during the response stream-back loop. One `RequestContext` is
//! created per request via [`crate::proxy::HydraProxy::new_ctx`] and dropped at
//! request end.
//!
//! ## Terminate-mode shape
//!
//! In terminate mode nearly all per-request state is produced and consumed
//! inside `request_filter` itself. Only the fields the `logging` hook needs
//! (latency, tenant, selected route, model, status, usage, scanner, trace id)
//! survive here — the stream-through body/replay machinery (`cursor`,
//! `body_buffer`, `first_chunk`, `upstream_bytes_seen`, `body_too_large`, …)
//! has been deleted (design-change §4.5).

use std::time::Instant;

use hydra_core::auth::AuthVerdict;
use hydra_core::model::{Candidate, RouteError, Tenant, Usage};
use hydra_core::sse::UsageScanner;

use crate::proxy::SelectedRoute;

/// Per-request state shared across the Pingora lifecycle hooks (design §6.2).
///
/// In terminate mode the only hooks that run are `request_filter` (which does
/// the whole gateway job) and `logging` (which reads back the fields below).
/// Fields are grouped by the phase that writes them:
/// - *request_filter*: `tenant`, `client_api_key`, `auth_verdict`, `model_key`,
///   `candidates`, `route_error`, `trace_id`, `started_at`, `selected`,
///   `upstream_host`, `upstream_started_at`, `status_code`.
/// - *request_filter (stream-back)*: `scanner` (mutated per response chunk).
/// - *logging*: reads `scanner` → `usage`, then emits metrics + the record.
pub struct RequestContext {
    /// Request start time (for latency reporting in `logging`).
    pub started_at: Instant,
    /// Tenant resolved from the `Host` header (None ⇒ 404 short-circuit).
    pub tenant: Option<Tenant>,
    /// The raw client api-key, parsed from any supported credential
    /// transport (see `hydra_core::apikey`).
    pub client_api_key: Option<String>,
    /// The external-auth verdict (carries the HTTP status to write back).
    pub auth_verdict: Option<AuthVerdict>,
    /// `model_key` extracted from the full request body via `memchr` (None ⇒
    /// non-JSON / no-model ⇒ passthrough degenerate forward or 400 reject).
    pub model_key: Option<String>,
    /// Ordered candidate set from `router::resolve` + `swrr::order` (or a
    /// single-element passthrough list).
    pub candidates: Vec<Candidate>,
    /// The currently-selected provider + its api-key (written in the failover
    /// loop when a candidate answers successfully).
    pub selected: Option<SelectedRoute>,
    /// Instant just before the successful provider send (set in the failover
    /// loop) — used to observe `hydra_upstream_duration_seconds` (TTFT).
    pub upstream_started_at: Option<Instant>,
    /// Status code received from the upstream / written to the client (for the
    /// usage record and metrics).
    pub status_code: u16,
    /// Upstream host actually contacted (for the usage record).
    pub upstream_host: Option<String>,
    /// Hydra's own overhead: elapsed from `started_at` to just before the
    /// successful upstream `send` (auth + routing + body read). `None` when no
    /// upstream attempt succeeded far enough to send. Written in the failover
    /// loop; read by `logging` → `UsageRecord::forward_latency_ms`.
    pub forward_latency_ms: Option<u64>,
    /// Time To First Token: elapsed from `started_at` to the first response
    /// chunk received from the provider. `None` for non-streamed / errored
    /// requests that produced no chunk. Written during the stream-back loop;
    /// read by `logging` → `UsageRecord::ttft_ms`.
    pub ttft_ms: Option<u64>,
    /// Incremental usage scanner (memchr over response chunks; §9.4). Mutated
    /// during the stream-back loop; finalised in `logging`.
    pub scanner: UsageScanner,
    /// Finalised usage (set in `logging` from `scanner.finalize()`).
    pub usage: Option<Usage>,
    /// Routing failure reason (written when `resolve` errors).
    pub route_error: Option<RouteError>,
    /// Per-request trace id (echoed back as `X-Hydra-Trace-Id`).
    pub trace_id: String,
}

impl RequestContext {
    /// Build a fresh context for an incoming request.
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            tenant: None,
            client_api_key: None,
            auth_verdict: None,
            model_key: None,
            candidates: Vec::new(),
            selected: None,
            upstream_started_at: None,
            status_code: 0,
            upstream_host: None,
            forward_latency_ms: None,
            ttft_ms: None,
            // Generic (OpenAI-compatible) schema by default; the SSE scanner
            // normalises the small usage object per provider family.
            scanner: UsageScanner::new(hydra_core::model::ProviderKind::Generic),
            usage: None,
            route_error: None,
            trace_id: crate::proxy::new_trace_id(),
        }
    }
}

impl Default for RequestContext {
    fn default() -> Self {
        Self::new()
    }
}
