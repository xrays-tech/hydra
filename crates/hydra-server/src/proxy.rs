//! `HydraProxy`: the [`ProxyHttp`] impl running in **terminate-in-Pingora**
//! mode (design-change `terminate-mode`).
//!
//! ## Terminate mode (the current, validated mechanism)
//!
//! The whole gateway lifecycle happens inside [`ProxyHttp::request_filter`]:
//!
//! 1. Domain → tenant, api-key parse, external auth (cache-first).
//! 2. **Read the full downstream body** (`read_request_body` loop → `Bytes`).
//! 3. `extract_model` over the *full* body (memchr — trivial for any
//!    position/schema; the stream-through "first-chunk gamble" is gone).
//! 4. `router::resolve` + `swrr::order` → ordered candidate list.
//! 5. Pre-limit count gate.
//! 6. **Failover loop**: for each candidate, build the upstream request
//!    (swap key, `/v1` rewrite, Host) via [`ProviderClient`], send it, and on
//!    success stream the response back chunk-by-chunk through the downstream
//!    `Session`. On failure, `breaker.on_failure` + `continue` to the next
//!    candidate (the body is `Bytes` — O(1) clone per replay).
//! 7. `return Ok(true)` so Pingora **never dials an upstream itself**.
//!
//! `upstream_peer` is a mandatory trait method but returns a sentinel peer that
//! is never contacted.
//!
//! ## Why not stream-through anymore
//!
//! The previous zero-copy stream-through design fought Pingora's retry
//! machinery (the retry-buffer enablement hack, a `Vec<Bytes>` accumulator,
//! `set_retry`, the 64 KiB retry-buffer ceiling, a passthrough fallback) and only worked
//! when `"model"` sat in the *first* body chunk. Late-model clients (large
//! system/tools/history prefixes) broke routing. Terminating in Pingora makes
//! model extraction trivial and failover a plain `for` loop. See
//! `docs/design-change-terminate-mode.md` (§1/§4.5) and `tests/terminate_mode.rs`.
//!
//! ## What this module does NOT do
//!
//! - It never mocks routing / SWRR / breaker / parsing — those W1 pure fns are
//!   called directly with real `ConfigData`.
//! - It never serialises/deserialises the request body — `Bytes` flows into
//!   reqwest untouched and response chunks flow back untouched (the
//!   "no JSON encode/decode on the hot path" claim is preserved).

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use hydra_core::auth::{AuthVerdict, CacheSource};
use hydra_core::config::{resolve_policy, ConfigData};
use hydra_core::extract::extract_model;
use hydra_core::limit::MatchCtx;
use hydra_core::model::{Candidate, RouteError};
use hydra_core::rewrite::mask_key;
use hydra_core::router;
use hydra_core::swrr;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Error as PingoraError;
use pingora_core::Result as PingoraResult;
use pingora_http::ResponseHeader;
use pingora_proxy::{ProxyHttp, Session};
use rand::seq::SliceRandom;
use tracing::{debug, info, warn};

use crate::http::{AuthChecker, HttpAuthChecker};
use crate::proxy::admission::{AdmissionControl, AdmissionError};
use crate::proxy::breaker_wrap::CircuitBreaker;
use crate::proxy::config::ProxyConfig;
use crate::proxy::ctx::RequestContext;
use crate::proxy::limiter::CountVerdict;
use crate::proxy::peer::parse_endpoint;
use crate::proxy::provider_client::ProviderClient;
use crate::sink::UsageSink;
use crate::store::ConfigStore;

pub mod breaker_wrap;
pub mod config;
pub mod ctx;
pub mod limiter;
pub mod peer;
pub mod provider_client;

pub mod admission;

/// The currently-selected upstream route, written in the failover loop when a
/// candidate answers successfully and read by `logging`.
#[derive(Clone, Debug)]
pub struct SelectedRoute {
    pub provider_id: String,
    /// Parsed endpoint (host used for the usage record).
    pub endpoint: hydra_core::rewrite::EndpointUrl,
    /// The provider api-key chosen at random for this attempt (replaces the
    /// client's `Authorization`).
    pub upstream_api_key: String,
}

/// Shared, long-lived application state threaded through every request
/// (design §6.1 / §15.1). Cheap to `Arc`-clone per request task.
pub struct AppState {
    /// Hot-reload config centre (`ArcSwap<ConfigData>` + SWRR state map).
    pub store: ConfigStore,
    /// External auth boundary (W3 `HttpAuthChecker` held concretely — its
    /// `check` returns an RPITIT future which is not dyn-compatible, so we
    /// hold the concrete production impl and call it directly).
    pub auth: Arc<HttpAuthChecker>,
    /// Concurrent circuit breaker (feeds `router::resolve` via `BreakerView`).
    pub breaker: Arc<CircuitBreaker>,
    /// Rate limiter (in-memory on single node; Redis-backed in cluster, P4).
    pub limiter: Arc<dyn crate::proxy::limiter::Limiter>,
    /// Per-provider bounded admission queue (design-admission-queue §3).
    /// The concurrency valve: `acquire` before send, RAII permit released on
    /// scope-exit. All-zero default policy ⇒ `Passthrough` (no-op for
    /// unconfigured providers).
    pub admission: AdmissionControl,
    /// Usage sink (fire-and-forget).
    pub sink: Arc<dyn UsageSink>,
    /// Proxy / failover / breaker policy.
    pub proxy: ProxyConfig,
}

/// The `ProxyHttp` impl wiring the W1/W2/W3 pure functions to a terminate-mode
/// gateway that lives entirely in `request_filter`. One instance lives for the
/// whole server; per-request state lives in [`RequestContext`].
pub struct HydraProxy {
    pub state: Arc<AppState>,
    /// Long-lived upstream HTTP client (own connection pool, long timeout for
    /// SSE/LLM). Built once; cheap to hold.
    pub provider_client: ProviderClient,
}

impl HydraProxy {
    /// Build with the shared app state. The provider client is constructed
    /// here (infallible — see [`ProviderClient::new`]).
    #[must_use]
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            provider_client: ProviderClient::new(),
        }
    }

    /// Resolve the tenant from the downstream `Host` header (design §6.3 §1).
    /// `localhost` / missing Host maps to the `localhost` tenant.
    fn resolve_tenant(cfg: &ConfigData, host: &str) -> Option<hydra_core::model::Tenant> {
        let domain = host.split(':').next().unwrap_or("").to_ascii_lowercase();
        let lookup = if domain.is_empty() || domain == "localhost" {
            "localhost"
        } else {
            domain.as_str()
        };
        cfg.tenants_by_domain.get(lookup).cloned()
    }

    /// Parse the client api-key from `Authorization: Bearer …` or `x-api-key`.
    fn extract_api_key(session: &Session) -> Option<String> {
        let headers = &session.req_header().headers;
        if let Some(auth) = headers.get("authorization") {
            if let Ok(s) = auth.to_str() {
                if let Some(rest) = s
                    .strip_prefix("Bearer ")
                    .or_else(|| s.strip_prefix("bearer "))
                {
                    return Some(rest.to_string());
                }
                // Some clients send the key bare after `Bearer` with no space,
                // or just the key in this header; fall through to x-api-key.
            }
        }
        if let Some(k) = headers.get("x-api-key") {
            if let Ok(s) = k.to_str() {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
        None
    }
}

/// Generate a per-request trace id (dependency-free, monotonic-ish). Echoed
/// back as `X-Hydra-Trace-Id` and threaded into the usage record.
pub fn new_trace_id() -> String {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Mix in the thread id so concurrent requests don't collide on the same
    // nanosecond; std thread id is opaque but hashable via Debug.
    let tid = format!("{:?}", std::thread::current().id());
    format!("hydra-{:x}-{}", nanos, tid.len())
}

#[async_trait::async_trait]
impl ProxyHttp for HydraProxy {
    type CTX = RequestContext;

    fn new_ctx(&self) -> Self::CTX {
        RequestContext::new()
    }

    // -----------------------------------------------------------------------
    // request_filter — the FULL terminate-mode gateway lifecycle.
    // -----------------------------------------------------------------------
    //
    // Steps (design-change §4.1):
    //   ①  domain → tenant
    //   ②  api-key parse
    //   ③  external auth (cache-first) + metrics
    //   ④  read the FULL downstream body (loop → Bytes)
    //   ⑤  extract_model over the full body (memchr — any position/schema)
    //   ⑥  pre-limit count gate (BEFORE routing: 429 even if routing would 503)
    //   ⑦  router::resolve + swrr::order  (+ passthrough fallback)
    //   ⑧  failover loop: build → send → stream-back, breaker on success/fail
    //   ⑨  return Ok(true)  ← Pingora never dials upstream itself
    //
    // On any short-circuit we write a structured error body and return Ok(true).
    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut RequestContext,
    ) -> PingoraResult<bool>
    where
        Self::CTX: Send + Sync,
    {
        let cfg_guard = self.state.store.snapshot();
        let cfg: &ConfigData = &cfg_guard;

        // (1) Domain → tenant (§6.3 §1). Missing/localhost → "localhost".
        let host = session
            .req_header()
            .headers
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        // §12.3 SNI/Host mismatch observation (never blocks): the cert was
        // selected by TLS SNI; compare it against the Host-derived domain and
        // bump `hydra_sni_host_mismatch_total` on mismatch. Additive only.
        #[cfg(any(feature = "tls-boringssl", feature = "tls-openssl"))]
        crate::tls::observe_sni_host_mismatch(session, host);
        let Some(tenant) = Self::resolve_tenant(cfg, host) else {
            return short_circuit(session, 404, "unknown_domain").await;
        };

        // (2) Tenant enabled (§6.3 §2).
        if !tenant.enabled {
            ctx.tenant = Some(tenant);
            return short_circuit(session, 403, "tenant_disabled").await;
        }
        let tenant_id = tenant.id.clone();
        ctx.tenant = Some(tenant.clone());

        // (3) api-key parse (§6.3 §3).
        let api_key = Self::extract_api_key(session);
        let api_key = match api_key {
            Some(k) => k,
            None => return short_circuit(session, 401, "missing_api_key").await,
        };
        ctx.client_api_key = Some(api_key.clone());

        // (4) External auth (§6.3 §4 / §11). Cache-first via AuthChecker.
        let verdict = self.state.auth.check(&tenant, &api_key).await;
        ctx.auth_verdict = Some(verdict.clone());
        // Metrics (§17): auth decision + cache size (+ upstream-error counter).
        {
            let src = match &verdict {
                AuthVerdict::Allowed { source } | AuthVerdict::Denied { source, .. } => {
                    match source {
                        CacheSource::Hit => "hit",
                        CacheSource::Miss => "miss",
                        CacheSource::Local => "local",
                    }
                }
            };
            let vlabel = match &verdict {
                AuthVerdict::Allowed { .. } => "allowed",
                AuthVerdict::Denied { .. } => "denied",
            };
            crate::admin::metrics::record_auth_decision(&tenant_id, vlabel, src);
            crate::admin::metrics::record_auth_cache_size(self.state.auth.cache().len());
            if let AuthVerdict::Denied { reason, .. } = &verdict {
                if *reason == "auth_upstream_unavailable" {
                    crate::admin::metrics::record_auth_upstream_error(&tenant_id);
                }
            }
        }
        if let AuthVerdict::Denied { status, reason, .. } = &verdict {
            let body = Bytes::from(format!(
                "{{\"error\":{{\"message\":\"{reason}\",\"type\":\"auth_error\"}}}}"
            ));
            session.set_keepalive(None);
            session.respond_error_with_body(*status, body).await?;
            return Ok(true);
        }

        // (5) Read the FULL downstream body (terminate-mode §4.1 ④). Mirror the
        //     admin full-body read pattern (admin/handlers.rs read_body). The
        //     body is held as `Bytes` so failover replay across N candidates is
        //     an O(1) refcount clone per attempt.
        let req_header = session.req_header();
        let req_path = req_header.uri.path().to_string();
        let method = req_header.method.as_str();
        let is_v1_route = req_path.starts_with("/v1/");
        let has_body = method == "POST" || method == "PUT" || method == "PATCH";
        // Format-homogeneous pass-through (§9.4): the client's request path
        // selects the usage-parser family. /v1/messages → Anthropic schema
        // (input_tokens/output_tokens/cache_read_input_tokens); everything
        // else → Generic (OpenAI-compatible) — the safe default with ZERO
        // behaviour change for /v1/chat/completions.
        let api_kind = if req_path.ends_with("/v1/messages") {
            hydra_core::model::ProviderKind::Anthropic
        } else {
            hydra_core::model::ProviderKind::Generic
        };
        ctx.scanner = hydra_core::sse::UsageScanner::new(api_kind);
        // Snapshot the original request header (method/path/headers) so
        // build_request can rebuild the upstream request from it later. The
        // immutable borrow of `session` ends here (NLL), allowing the mutable
        // `as_downstream_mut().read_request_body()` calls below.
        let original_header = req_header.clone();

        let body_bytes: Bytes = if has_body {
            let mut buf = Vec::new();
            while let Ok(Some(chunk)) = session.as_downstream_mut().read_request_body().await {
                buf.extend_from_slice(&chunk);
                // Hard cap (§8.5): a body over the hard cap returns 413. The
                // soft cap is gone (no replay buffer to disable), but the hard
                // cap still protects the gateway from unbounded buffering.
                if buf.len() as u64 > self.state.proxy.max_request_body_hard {
                    session.set_keepalive(None);
                    let _ = session.as_downstream_mut().drain_request_body().await;
                    return short_circuit(session, 413, "request_body_too_large").await;
                }
            }
            Bytes::from(buf)
        } else {
            Bytes::new()
        };

        // (6) Model extraction over the FULL body (terminate-mode §4.1 ④').
        //     memchr anywhere in the body — works for any position/schema, the
        //     root cause fix for late-model clients. No JSON encode/decode.
        let model_opt: Option<String> = if has_body && is_v1_route {
            extract_model(body_bytes.as_ref()).map(|b| String::from_utf8_lossy(b).into_owned())
        } else {
            None
        };

        // (7) Pre-limit count gate (§6.3 §7 / §10.3). Runs BEFORE routing so a
        //     rate-limited request gets 429 even when routing would otherwise
        //     short-circuit (e.g. breaker-tripped → NoAvailableProvider → 503).
        //     The model dimension is the extracted `model_opt` (identical to
        //     what routing sets); provider is unknown until routing → `None`.
        let masked = mask_key(&api_key);
        let match_ctx = MatchCtx {
            api_key: Some(&masked),
            model: model_opt.as_deref(),
            tenant: Some(&tenant_id),
            provider: None,
        };
        let now = Instant::now();
        if let CountVerdict::Denied { role_id } = self
            .state
            .limiter
            .check_count(&cfg.limit_roles, &match_ctx, now)
            .await
        {
            debug!(role = %role_id, tenant = %tenant_id, "rate-limited (count)");
            crate::admin::metrics::record_limit_rejected(&tenant_id, &role_id, "count");
            return short_circuit(session, 429, "rate_limited").await;
        }

        // (8) Route (§6.3 §6 / §7): pure resolve + swrr.order, OR passthrough.
        let (candidates, model_for_route) = match model_opt {
            Some(m) => {
                let model_key = m;
                let cands = match router::resolve(
                    cfg,
                    self.state.breaker.as_ref(),
                    &tenant,
                    &model_key,
                    Some(api_key.as_str()),
                ) {
                    Ok(c) => c,
                    Err(e) => {
                        ctx.route_error = Some(e);
                        ctx.model_key = Some(model_key.clone());
                        let status = route_error_status(e);
                        let reason = route_error_reason(e);
                        crate::admin::metrics::record_route_error(&tenant_id, reason);
                        return short_circuit(session, status, reason).await;
                    }
                };
                (cands, Some(model_key))
            }
            None => {
                // Non-routable: no `model` field. Apply the configured strategy
                // (§6.3a). In terminate mode "passthrough" is just a degenerate
                // single-candidate forward through the same failover loop.
                match self.state.proxy.non_route_strategy {
                    config::NonRouteStrategy::Reject => {
                        return short_circuit(session, 400, "no_model_field").await;
                    }
                    config::NonRouteStrategy::Passthrough => {
                        match passthrough_candidates(cfg, &tenant_id, Some(api_key.as_str())) {
                            Some(c) => (c, None),
                            None => {
                                ctx.route_error = Some(RouteError::NoAvailableProvider);
                                crate::admin::metrics::record_route_error(
                                    &tenant_id,
                                    "no_available_provider",
                                );
                                return short_circuit(session, 503, "no_live_provider").await;
                            }
                        }
                    }
                }
            }
        };
        ctx.model_key = model_for_route.clone();

        // SWRR order over the resolved candidates (skip for passthrough — a
        // single candidate needs no ordering). Thread the per-(tenant,model)
        // state from the ConfigStore's DashMap.
        let mut candidates = candidates;
        if let Some(model_key) = model_for_route.as_deref() {
            let key = (tenant_id.clone(), model_key.to_string());
            let mut guard = self.state.store.swrr().entry(key).or_default();
            swrr::order(&mut candidates, &mut guard);
        }
        ctx.candidates = candidates.clone();

        // (9) Failover loop (terminate-mode §4.1 ⑦/⑧). Body is in hand as
        //     `Bytes`; each attempt is an O(1) clone. On a connect/HTTP error
        //     we breaker.on_failure + continue. On a 2xx response we stream it
        //     back chunk-by-chunk and break.
        //
        // KNOWN LIMITATION: once `write_response_header` + any
        // `write_response_body` chunk has been sent, failover is impossible —
        // the client already received a 200 + partial body. A mid-stream error
        // (provider reset, network drop) is logged and the connection is
        // closed; we do NOT retry. This is the same constraint as the prior
        // design's `upstream_bytes_seen > 0` rule (§8.2/§8.3) and is
        // unavoidable for any streaming gateway.
        let mut last_status: u16 = 502;
        let mut last_error: Option<String> = None;
        // Track whether the LAST candidate failure was an admission (capacity)
        // error vs a real upstream error. Design §6/§7: when ALL candidates
        // fail with admission errors, the post-loop path emits 503 +
        // Retry-After instead of the existing upstream-error behaviour.
        let mut last_failure_was_admission = false;
        // Minimum resolved queue_wait_timeout_ms across admission-failed
        // candidates — used for the Retry-After hint (design §6).
        let mut min_admission_wait_ms: Option<u64> = None;
        for cand in &candidates {
            let Some(provider) = cfg.providers.get(&cand.provider_id) else {
                warn!(provider_id = %cand.provider_id, "candidate provider missing from config");
                last_error = Some(format!("provider {} missing", cand.provider_id));
                last_failure_was_admission = false;
                continue;
            };
            let Some(endpoint) = parse_endpoint(&provider.endpoint) else {
                warn!(provider_id = %cand.provider_id, "candidate endpoint unparseable");
                last_error = Some(format!("provider {} bad endpoint", cand.provider_id));
                last_failure_was_admission = false;
                continue;
            };
            let Some(keys) = cfg.provider_keys.get(&cand.provider_id) else {
                warn!(provider_id = %cand.provider_id, "candidate has no api keys");
                last_error = Some(format!("provider {} no key", cand.provider_id));
                last_failure_was_admission = false;
                continue;
            };
            let Some(upstream_key) = keys.choose(&mut rand::thread_rng()) else {
                last_failure_was_admission = false;
                continue;
            };

            // ── Admission control (design-admission-queue §6/§7) ──────────
            //
            // Acquire a concurrency permit BEFORE sending to the upstream.
            // This is the concurrency valve — it bounds in-flight requests to
            // `max_concurrency` per provider.
            //
            // ╔══════════════════════════════════════════════════════════════╗
            // ║ §7 BREAKER BOUNDARY (inviolable): admission errors          ║
            // ║ (QueueFull / WaitTimeout / Closed) are CAPACITY signals,    ║
            // ║ NOT upstream errors. They MUST NOT call breaker.on_failure. ║
            // ║ A busy provider is not a dead provider.                     ║
            // ╚══════════════════════════════════════════════════════════════╝
            //
            // The permit (`_permit`) is held in this loop-body scope through
            // build_request → send → stream_response, and drops on every
            // `continue` / `return` / end-of-iteration — releasing the slot
            // (RAII, risk #3).
            //
            // DEFAULT NO-OP: when the resolved policy has max_concurrency == 0
            // (the all-zero default for unconfigured providers), acquire
            // returns Permit::Passthrough instantly — no gate, no block, no
            // semaphore. This is the safe-rollout invariant.
            let policy = resolve_policy(
                provider.max_concurrency,
                provider.max_queue_depth,
                provider.queue_wait_timeout_ms,
                self.state.proxy.default_concurrency_policy,
            );
            let _permit = match self
                .state
                .admission
                .acquire(&cand.provider_id, policy)
                .await
            {
                Ok(p) => p,
                // ── Capacity exhausted — §7: do NOT trip the breaker ──────
                Err(AdmissionError::QueueFull)
                | Err(AdmissionError::WaitTimeout)
                | Err(AdmissionError::Closed) => {
                    last_failure_was_admission = true;
                    if policy.queue_wait_timeout_ms > 0 {
                        min_admission_wait_ms = Some(
                            min_admission_wait_ms.map_or(policy.queue_wait_timeout_ms, |m| {
                                m.min(policy.queue_wait_timeout_ms)
                            }),
                        );
                    }
                    last_error = Some(format!(
                        "provider {} admission denied (capacity)",
                        cand.provider_id
                    ));
                    debug!(
                        provider_id = %cand.provider_id,
                        "admission denied; trying next candidate (NO breaker trip — §7)"
                    );
                    continue;
                }
            };
            // Admission succeeded — reset the flag (this candidate reached the
            // upstream send path; any subsequent failure is a real upstream error).
            last_failure_was_admission = false;

            // Build + send (Oracle correction #10: start the TTFT timer before send).
            let req = self.provider_client.build_request(
                &original_header,
                provider,
                upstream_key,
                &body_bytes,
                &ctx.trace_id,
            );
            // forward_latency_ms: Hydra's own overhead (auth + routing + body
            // read) = request start → just before the upstream send. Captured
            // once (first attempt): on failover the additional elapsed time is
            // upstream-connect failure, not Hydra overhead. (design §9.1.)
            if ctx.forward_latency_ms.is_none() {
                ctx.forward_latency_ms = Some(ctx.started_at.elapsed().as_millis() as u64);
            }
            ctx.upstream_started_at = Some(Instant::now());
            let send_result = self.provider_client.send(req).await;

            match send_result {
                Ok(resp) => {
                    let status = resp.status();
                    let status_code = status.as_u16();
                    last_status = status_code;
                    if status.is_success() {
                        // ── Success: stream the response back to the client.
                        ctx.status_code = status_code;
                        ctx.selected = Some(SelectedRoute {
                            provider_id: cand.provider_id.clone(),
                            endpoint: endpoint.clone(),
                            upstream_api_key: upstream_key.clone(),
                        });
                        ctx.upstream_host = Some(endpoint.host.clone());
                        // Metrics (§17): upstream time-to-first-byte.
                        if let Some(model) = model_for_route.as_deref() {
                            if let Some(start) = ctx.upstream_started_at {
                                crate::admin::metrics::record_upstream_duration(
                                    &cand.provider_id,
                                    model,
                                    start.elapsed().as_secs_f64(),
                                );
                            }
                        }
                        self.state.breaker.on_success(&cand.provider_id);

                        if let Err(e) = self
                            .stream_response(session, ctx, resp, &tenant_id, &cand.provider_id)
                            .await
                        {
                            // Mid-stream failure after the header/first chunk was
                            // already written: failover is impossible (client saw
                            // 200 + partial body). Log + close. Do NOT retry.
                            // P2-9: count it for observability.
                            crate::admin::metrics::record_mid_stream_error(&cand.provider_id);
                            warn!(
                                trace_id = %ctx.trace_id,
                                provider_id = %cand.provider_id,
                                error = %e,
                                "mid-stream SSE failure after header sent; closing connection (no failover)"
                            );
                            return Ok(true);
                        }
                        return Ok(true);
                    } else {
                        // 4xx/5xx from provider — not a connect failure, but
                        // the provider answered. Record breaker failure +
                        // retry the next candidate (body still in hand).
                        self.state.breaker.on_failure(&cand.provider_id);
                        last_error = Some(format!(
                            "provider {} returned {}",
                            cand.provider_id, status_code
                        ));
                        debug!(
                            provider_id = %cand.provider_id,
                            status = status_code,
                            "provider returned non-2xx; trying next candidate"
                        );
                    }
                }
                Err(e) => {
                    // Connect / transport failure → breaker + next candidate.
                    self.state.breaker.on_failure(&cand.provider_id);
                    last_error = Some(format!("provider {}: {e}", cand.provider_id));
                    debug!(
                        provider_id = %cand.provider_id,
                        error = %e,
                        "provider send failed; trying next candidate"
                    );
                }
            }

            // Record a failover retry for every candidate we fall through from
            // (Oracle correction #5). Preserves `hydra_retries_total`.
            if let (Some(t), Some(m)) = (ctx.tenant.as_ref(), ctx.model_key.as_deref()) {
                crate::admin::metrics::record_retry(&t.id, m, "terminate_loop");
            } else if let Some(t) = ctx.tenant.as_ref() {
                crate::admin::metrics::record_retry(&t.id, "", "terminate_loop");
            }
        }

        // ── Post-loop: all candidates exhausted ──────────────────────────
        //
        // Design §6: if the LAST candidate failure was an admission (capacity)
        // error, respond 503 + `Retry-After` (a conservative hint that a retry
        // might find capacity). Otherwise (real upstream error), keep the
        // EXISTING behaviour (map last_status to a gateway response).
        if last_failure_was_admission {
            ctx.status_code = 503;
            // Retry-After = min queue_wait_timeout_ms / 1000 (at least 1s).
            let retry_after_secs = (min_admission_wait_ms.unwrap_or(2000) / 1000).max(1);
            info!(
                trace_id = %ctx.trace_id,
                retry_after = retry_after_secs,
                error = ?last_error,
                "all candidates exhausted on admission (capacity); 503 + Retry-After"
            );
            let body = Bytes::from(
                "{\"error\":{\"message\":\"admission_denied\",\
                 \"type\":\"proxy_error\",\"detail\":\"all providers at concurrency capacity\"}}",
            );
            session.set_keepalive(None);
            let mut resp_header = ResponseHeader::build(503, Some(2))?;
            resp_header.insert_header("Content-Type", "application/json")?;
            resp_header.insert_header("Retry-After", retry_after_secs.to_string())?;
            resp_header.insert_header("X-Hydra-Trace-Id", &ctx.trace_id)?;
            session
                .write_response_header(Box::new(resp_header), false)
                .await?;
            session.write_response_body(Some(body), true).await?;
            return Ok(true);
        }

        // All candidates exhausted (§4.1 final branch). Map the last status to
        // a gateway response: provider-surfaced errors are forwarded verbatim
        // when they carry a useful code (e.g. 404/401/429), otherwise 502.
        ctx.status_code = last_status;
        let reason = match last_status {
            400 | 401 | 403 | 404 | 409 | 413 | 422 => "provider_error",
            429 => "provider_rate_limited",
            _ => "all_proxies_failed",
        };
        info!(
            trace_id = %ctx.trace_id,
            status = last_status,
            error = ?last_error,
            "all candidates exhausted; returning last provider status"
        );
        // Forward a compact JSON error echoing the upstream code so clients see
        // a structured error rather than an empty gateway response.
        let body = Bytes::from(format!(
            "{{\"error\":{{\"message\":\"{reason}\",\"type\":\"proxy_error\",\"upstream_status\":{last_status}}}}}"
        ));
        session.set_keepalive(None);
        // respond_error_with_body maps non-standard codes onto its own; use the
        // explicit header writer when we must preserve a specific upstream code.
        if (400..=599).contains(&last_status) {
            let mut resp_header = ResponseHeader::build(last_status, Some(1))?;
            resp_header.insert_header("Content-Type", "application/json")?;
            resp_header.insert_header("X-Hydra-Trace-Id", &ctx.trace_id)?;
            session
                .write_response_header(Box::new(resp_header), false)
                .await?;
            session.write_response_body(Some(body), true).await?;
        } else {
            session.respond_error(502).await?;
        }
        Ok(true)
    }

    // -----------------------------------------------------------------------
    // upstream_peer — trait-mandatory sentinel. NEVER called: request_filter
    // returns Ok(true), so Pingora never reaches the upstream dial path.
    // -----------------------------------------------------------------------
    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut RequestContext,
    ) -> PingoraResult<Box<HttpPeer>> {
        // Oracle correction #9: a real (inert) peer, not unreachable!(), so an
        // accidental call never panics the worker.
        Ok(Box::new(HttpPeer::new("127.0.0.1:0", false, String::new())))
    }

    // -----------------------------------------------------------------------
    // logging — §6.6: latency/usage → UsageSink.record
    // -----------------------------------------------------------------------
    // KEPT UNCHANGED from the stream-through design: it finalises ctx.scanner
    // (populated during the stream-back loop in request_filter) into ctx.usage,
    // then emits metrics + the usage record + token-window accounting.
    async fn logging(
        &self,
        session: &mut Session,
        _e: Option<&PingoraError>,
        ctx: &mut RequestContext,
    ) where
        Self::CTX: Send + Sync,
    {
        let latency_ms = ctx.started_at.elapsed().as_millis() as u64;
        let status = if ctx.status_code > 0 {
            ctx.status_code
        } else {
            session.response_written().map_or(0, |r| r.status.as_u16())
        };
        // Finalise the usage scanner (populated during the SSE stream-back).
        let usage = std::mem::replace(
            &mut ctx.scanner,
            hydra_core::sse::UsageScanner::new(hydra_core::model::ProviderKind::Generic),
        )
        .finalize();
        ctx.usage = usage.clone();

        // Metrics (§17): request counter + latency histogram + token usage.
        // Increment for every proxied request that selected a provider.
        if let (Some(tenant), Some(sel)) = (ctx.tenant.as_ref(), ctx.selected.as_ref()) {
            let model = ctx.model_key.clone().unwrap_or_default();
            crate::admin::metrics::record_request(&tenant.id, &sel.provider_id, &model, status);
            crate::admin::metrics::record_request_duration(
                &tenant.id,
                &sel.provider_id,
                &model,
                ctx.started_at.elapsed().as_secs_f64(),
            );
            if let Some(u) = usage.as_ref() {
                if let Some(p) = u.tokens_in {
                    crate::admin::metrics::record_tokens(
                        &tenant.id,
                        &sel.provider_id,
                        &model,
                        "prompt",
                        p,
                    );
                }
                if let Some(c) = u.tokens_out {
                    crate::admin::metrics::record_tokens(
                        &tenant.id,
                        &sel.provider_id,
                        &model,
                        "completion",
                        c,
                    );
                }
                if let Some(cached) = u.cache_hit_tokens {
                    crate::admin::metrics::record_cached_tokens(
                        &tenant.id,
                        &sel.provider_id,
                        &model,
                        cached,
                    );
                }
            }
            // TTFT histogram (only when a first chunk was observed).
            if let Some(ttft_ms) = ctx.ttft_ms {
                crate::admin::metrics::record_ttft(
                    &tenant.id,
                    &sel.provider_id,
                    &model,
                    ttft_ms as f64 / 1000.0,
                );
            }
        }

        // Record into the sink (fire-and-forget). Only when we actually
        // selected a provider (i.e. forwarded something).
        if let (Some(tenant), Some(sel)) = (ctx.tenant.as_ref(), ctx.selected.as_ref()) {
            let model = ctx.model_key.clone().unwrap_or_default();
            let masked = ctx.client_api_key.as_ref().map(|k| mask_key(k));
            let now_iso = now_iso8601();
            let record = hydra_core::model::UsageRecord {
                tenant_id: tenant.id.clone(),
                provider_id: sel.provider_id.clone(),
                model_key: model,
                client_api_key_masked: masked,
                status_code: status,
                // Preserve None (→ NULL): a provider that does not report a
                // dimension must not masquerade as a zero count.
                tokens_in: usage.as_ref().and_then(|u| u.tokens_in),
                tokens_out: usage.as_ref().and_then(|u| u.tokens_out),
                cache_hit_tokens: usage.as_ref().and_then(|u| u.cache_hit_tokens),
                latency_ms,
                forward_latency_ms: Some(ctx.forward_latency_ms.unwrap_or(0)),
                ttft_ms: Some(ctx.ttft_ms.unwrap_or(0)),
                upstream_host: ctx.upstream_host.clone(),
                error: _e.map(|e| e.to_string()),
                trace_id: ctx.trace_id.clone(),
                created_at: now_iso,
            };
            // Fire-and-forget; the sink buffers internally.
            let _ = self.state.sink.record(record).await;
        }

        // Token-window accounting in the logging phase (§10.3). The limiter
        // needs a single total-token quantity; derive it locally from the
        // neutral fields (the metering record stores no derived total).
        let total = usage
            .as_ref()
            .map(|u| u.tokens_in.unwrap_or(0) + u.tokens_out.unwrap_or(0))
            .unwrap_or(0);
        if total > 0 {
            if let (Some(tenant), Some(sel), Some(model)) = (
                ctx.tenant.as_ref(),
                ctx.selected.as_ref(),
                ctx.model_key.as_deref(),
            ) {
                let masked = ctx.client_api_key.as_ref().map(|k| mask_key(k));
                let match_ctx = MatchCtx {
                    api_key: masked.as_deref(),
                    model: Some(model),
                    tenant: Some(&tenant.id),
                    provider: Some(&sel.provider_id),
                };
                let cfg = self.state.store.snapshot();
                self.state
                    .limiter
                    .add_tokens(&cfg.limit_roles, &match_ctx, total, Instant::now())
                    .await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// HydraProxy helpers
// ---------------------------------------------------------------------------

impl HydraProxy {
    /// Stream a successful provider response back to the downstream `Session`
    /// chunk-by-chunk, scanning each chunk for SSE usage with `ctx.scanner`
    /// (terminate-mode §4.1 ⑧). Keeps the connection from being reused for
    /// keep-alive (SSE/LLM responses are long-lived) and writes a terminal EOS.
    ///
    /// Returns an error if any downstream write fails — the caller treats that
    /// as a non-retryable mid-stream failure (the header was already sent).
    async fn stream_response(
        &self,
        session: &mut Session,
        ctx: &mut RequestContext,
        mut resp: reqwest::Response,
        _tenant_id: &str,
        _provider_id: &str,
    ) -> PingoraResult<()> {
        // Build the downstream response header from the upstream status.
        let status = resp.status().as_u16();
        // Forward content-type + a small set of useful headers, collected as
        // owned `(String, String)` pairs first so the immutable borrow of
        // `resp` ends before the mutable `resp.chunk()` loop. Strip provider
        // fingerprints (server/via) like the old upstream_response_filter did,
        // plus hop-by-hop / encoding headers that must not be echoed.
        const SKIP: &[&str] = &[
            "server",
            "via",
            "transfer-encoding",
            "content-length",
            "connection",
            // reqwest/hyper already decoded any content-encoding; we ship raw
            // bytes downstream, so do not echo it.
            "content-encoding",
        ];
        let forwarded: Vec<(String, String)> = resp
            .headers()
            .iter()
            .filter(|(name, _)| !SKIP.iter().any(|s| name.as_str().eq_ignore_ascii_case(s)))
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|v| (name.as_str().to_string(), v.to_string()))
            })
            .collect();
        let mut resp_header = ResponseHeader::build(status, Some(8))?;
        for (name, value) in forwarded {
            let _ = resp_header.append_header(name, value);
        }
        let _ = resp_header.insert_header("X-Hydra-Trace-Id", &ctx.trace_id);

        // Long-lived response — disable keep-alive reuse (Oracle correction #6).
        session.as_downstream_mut().set_keepalive(None);
        session
            .write_response_header(Box::new(resp_header), false)
            .await?;

        // Stream body chunks: scan for usage (memchr) + write downstream.
        // TTFT (Time To First Token): elapsed from request start → the first
        // response chunk received from the provider. Captured once on the first
        // chunk (design §9.1).
        let mut first_chunk = true;
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| pingora_err(format!("upstream stream read error: {e}")))?
        {
            if first_chunk {
                first_chunk = false;
                ctx.ttft_ms = Some(ctx.started_at.elapsed().as_millis() as u64);
            }
            // memchr usage scan over the chunk (zero-alloc common path).
            let _ = ctx.scanner.scan_chunk(chunk.as_ref());
            session.write_response_body(Some(chunk), false).await?;
        }
        // Terminal EOS.
        session.write_response_body(None, true).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// Write a short error response and return `Ok(true)` (short-circuit the
/// pipeline). Mirrors the gateway example's `respond_error_with_body` pattern
/// with a tiny JSON body so clients see a structured error.
async fn short_circuit(session: &mut Session, status: u16, reason: &str) -> PingoraResult<bool> {
    let body = Bytes::from(format!(
        "{{\"error\":{{\"message\":\"{reason}\",\"type\":\"proxy_error\"}}}}"
    ));
    session.set_keepalive(None);
    session.respond_error_with_body(status, body).await?;
    Ok(true)
}

/// Build a degenerate single-candidate list for **passthrough** requests (no
/// `model` field, `NonRouteStrategy::Passthrough`): the tenant's first live,
/// non-dead provider with weight > 0 and at least one api-key. In terminate
/// mode passthrough is just a one-element failover loop — no upstream_peer /
/// retry-buffer machinery.
///
/// The §7.1b binding gate applies: an api-key matching an enabled prefix
/// binding may only pass through to the bound provider (fail-closed; no match
/// ⇒ unrestricted).
///
/// Returns `None` when no live provider exists (caller maps to 503).
fn passthrough_candidates(
    cfg: &ConfigData,
    tenant_id: &str,
    client_api_key: Option<&str>,
) -> Option<Vec<Candidate>> {
    let providers = cfg.tenant_providers.get(tenant_id)?;
    let bound = client_api_key.and_then(|k| router::match_key_binding(&cfg.key_prefix_bindings, k));
    let mut pids: Vec<&String> = providers.iter().collect();
    pids.sort(); // deterministic ordering
    for pid in pids {
        if let Some(b) = bound {
            if pid != &b.provider_id {
                continue;
            }
        }
        let Some(provider) = cfg.providers.get(pid) else {
            continue;
        };
        if provider.weight <= 0 {
            continue;
        }
        let Some(keys) = cfg.provider_keys.get(pid) else {
            continue;
        };
        if keys.is_empty() {
            continue;
        }
        return Some(vec![Candidate {
            provider_id: pid.clone(),
            endpoint: provider.endpoint.clone(),
            weight: provider.weight,
        }]);
    }
    None
}

/// Map a [`RouteError`] to its HTTP status (design §7.3).
fn route_error_status(e: RouteError) -> u16 {
    match e {
        RouteError::ModelNotAllowed => 403,
        RouteError::ModelNotFound => 404,
        RouteError::TenantForbidden => 403,
        RouteError::NoAvailableProvider | RouteError::NoAvailableKey => 503,
    }
}

/// Map a [`RouteError`] to a stable reason slug.
fn route_error_reason(e: RouteError) -> &'static str {
    match e {
        RouteError::ModelNotAllowed => "model_not_allowed",
        RouteError::ModelNotFound => "model_not_found",
        RouteError::TenantForbidden => "tenant_forbidden",
        RouteError::NoAvailableProvider => "no_available_provider",
        RouteError::NoAvailableKey => "no_available_key",
    }
}

/// Wrap a plain string into a boxed Pingora [`Error`] (internal-error variant).
fn pingora_err<S: Into<String>>(msg: S) -> Box<PingoraError> {
    use pingora_core::ErrorType::InternalError;
    PingoraError::explain(InternalError, msg.into())
}

/// Current time as an RFC 3339 UTC string (e.g. `2026-08-09T12:34:56Z`).
///
/// `chrono` is available under the `runtime` feature (which `proxy` implies);
/// the sink column is text, so we format here rather than in the pure core
/// (which has no chrono dependency). Second precision matches the sink schema.
fn now_iso8601() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[allow(dead_code)]
fn _unused_cache_source_marker() -> CacheSource {
    CacheSource::Local
}
