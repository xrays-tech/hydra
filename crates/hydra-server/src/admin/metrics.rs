//! Self-hosted `/metrics` registry + `record_*` helpers (design §17).
//!
//! All metrics are registered on the `prometheus` **default registry** and
//! rendered by the [`crate::admin::AdminService`] `/metrics` handler. They are
//! initialised once (process-wide `OnceLock`); a registration conflict
//! downgrades the whole set to no-ops rather than panicking — the same
//! conflict-tolerant philosophy as the W4b `tls::mismatch_counter`.
//!
//! ## Catalogue (design §17 — ALL implemented)
//!
//! | metric | type | labels | recorded where |
//! |--------|------|--------|----------------|
//! | `hydra_requests_total` | counter | tenant, provider, model, status | proxy `logging` |
//! | `hydra_request_duration_seconds` | histogram | tenant, provider, model | proxy `logging` |
//! | `hydra_upstream_duration_seconds` | histogram | provider, model | proxy `upstream_response_filter` |
//! | `hydra_retries_total` | counter | tenant, model, stage | proxy `fail_to_connect`/`error_while_proxy` |
//! | `hydra_tokens_total` | counter | tenant, provider, model, kind | proxy `logging` |
//! | `hydra_auth_decisions_total` | counter | tenant, verdict, source | proxy `request_filter` |
//! | `hydra_auth_upstream_error_total` | counter | tenant | proxy `request_filter` |
//! | `hydra_auth_cache_size` | gauge | — | proxy `request_filter` |
//! | `hydra_breaker_dead` | gauge | provider | breaker transitions |
//! | `hydra_breaker_state_transitions_total` | counter | provider, to | breaker `on_failure`/`on_success` |
//! | `hydra_limit_rejected_total` | counter | tenant, role, dim | proxy `request_filter` (429) |
//! | `hydra_sni_host_mismatch_total` | counter | — | `tls::note_sni_host_mismatch` (W4b) |
//! | `hydra_route_errors_total` | counter | tenant, reason | proxy `request_filter` (route err) |
//! | `hydra_ttft_seconds` | histogram | tenant, provider, model | proxy `logging` (time to first token) |
//! | `hydra_cached_tokens_total` | counter | tenant, provider, model | proxy `logging` (prompt-cache hits) |
//! | `hydra_permit_inflight` | gauge | provider | admission module (set on acquire/release) |
//! | `hydra_permit_available` | gauge | provider | admission module (capacity − inflight) |
//! | `hydra_queue_depth` | gauge | provider | admission module (current waiters) |
//! | `hydra_queue_wait_seconds` | histogram | provider | admission module (permit-acquired) |
//! | `hydra_queue_drops_total` | counter | provider, reason | admission module (denied acquire) |
//! | `hydra_admission_decisions_total` | counter | provider, outcome | admission module |
//! | `hydra_mid_stream_errors_total` | counter | provider | proxy `stream_response` (mid-stream write/read failure after 200 sent) |
//!
//! The record helpers tolerate a `None` handle (failed registration) by becoming
//! a cheap no-op, so instrumentation can never break the hot path. The
//! `hydra_sni_host_mismatch_total` counter is owned by the W4b `tls` module
//! (registered there); it shares the same default registry and thus appears on
//! `/metrics` automatically once a TLS backend is compiled in.

use std::sync::OnceLock;

use prometheus::{
    register_histogram_vec, register_int_counter_vec, register_int_gauge, register_int_gauge_vec,
    HistogramVec, IntCounterVec, IntGauge, IntGaugeVec,
};

/// Histogram buckets for latency: 5 ms → 120 s (LLM requests are slow).
const LATENCY_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0,
];

/// All `hydra_*` metric handles (minus the tls-owned SNI counter). Registered
/// exactly once against the default registry.
struct Metrics {
    requests: IntCounterVec,
    request_duration: HistogramVec,
    upstream_duration: HistogramVec,
    retries: IntCounterVec,
    tokens: IntCounterVec,
    auth_decisions: IntCounterVec,
    auth_upstream_error: IntCounterVec,
    auth_cache_size: IntGauge,
    breaker_dead: IntGaugeVec,
    breaker_transitions: IntCounterVec,
    limit_rejected: IntCounterVec,
    route_errors: IntCounterVec,
    /// Time To First Token (request start → first response chunk).
    ttft: HistogramVec,
    /// Prompt-cache hit token count (OpenAI cached_tokens / Anthropic
    /// cache_read_input_tokens).
    cached_tokens: IntCounterVec,
    // ── Admission control (design-admission-queue §10) ───────────────────
    /// In-flight permits per provider (held = actively sending/streaming).
    permit_inflight: IntGaugeVec,
    /// Available (free) permits per provider.
    permit_available: IntGaugeVec,
    /// Current queued waiters per provider.
    queue_depth: IntGaugeVec,
    /// Queue wait time histogram (time spent waiting for a permit).
    queue_wait: HistogramVec,
    /// Queue drops (denied acquire) by reason.
    queue_drops: IntCounterVec,
    /// Admission decisions by outcome.
    admission_decisions: IntCounterVec,
    // ── Mid-stream observability (P2-9) ─────────────────────────────────
    /// Mid-stream errors: a chunk read/write failed AFTER the 200 + first
    /// chunk was already sent to the client (failover impossible).
    mid_stream_errors: IntCounterVec,
    // ── Control plane (cluster P1) ──────────────────────────────────────
    /// Control-channel poll outcomes (result=ok|error).
    control_poll: IntCounterVec,
    /// Last config snapshot version applied (edge/standby).
    control_snapshot_version: IntGauge,
}

/// The SNI/Host mismatch counter name, registered by the W4b `tls` module. Kept
/// here only so a test can assert it appears on `/metrics` under a TLS backend.
pub const SNI_MISMATCH_METRIC: &str = "hydra_sni_host_mismatch_total";

/// Lazy global metrics. `None` if registration failed (no-op instrumentation).
fn metrics() -> Option<&'static Metrics> {
    static M: OnceLock<Option<Metrics>> = OnceLock::new();
    M.get_or_init(|| {
        Some(Metrics {
            requests: register_int_counter_vec!(
                "hydra_requests_total",
                "Total proxied requests (incl. failures)",
                &["tenant", "provider", "model", "status"]
            )
            .ok()?,
            request_duration: register_histogram_vec!(
                "hydra_request_duration_seconds",
                "End-to-end request latency",
                &["tenant", "provider", "model"],
                LATENCY_BUCKETS.to_vec()
            )
            .ok()?,
            upstream_duration: register_histogram_vec!(
                "hydra_upstream_duration_seconds",
                "Upstream latency (time to first byte)",
                &["provider", "model"],
                LATENCY_BUCKETS.to_vec()
            )
            .ok()?,
            retries: register_int_counter_vec!(
                "hydra_retries_total",
                "Failover retries",
                &["tenant", "model", "stage"]
            )
            .ok()?,
            tokens: register_int_counter_vec!(
                "hydra_tokens_total",
                "Token usage (when known)",
                &["tenant", "provider", "model", "kind"]
            )
            .ok()?,
            auth_decisions: register_int_counter_vec!(
                "hydra_auth_decisions_total",
                "Auth verdicts",
                &["tenant", "verdict", "source"]
            )
            .ok()?,
            auth_upstream_error: register_int_counter_vec!(
                "hydra_auth_upstream_error_total",
                "Auth upstream failures",
                &["tenant"]
            )
            .ok()?,
            auth_cache_size: register_int_gauge!("hydra_auth_cache_size", "Auth cache entry count")
                .ok()?,
            breaker_dead: register_int_gauge_vec!(
                "hydra_breaker_dead",
                "Dead-set indicator per provider (1=dead)",
                &["provider"]
            )
            .ok()?,
            breaker_transitions: register_int_counter_vec!(
                "hydra_breaker_state_transitions_total",
                "Breaker state transitions",
                &["provider", "to"]
            )
            .ok()?,
            limit_rejected: register_int_counter_vec!(
                "hydra_limit_rejected_total",
                "Rate-limit rejections",
                &["tenant", "role", "dim"]
            )
            .ok()?,
            route_errors: register_int_counter_vec!(
                "hydra_route_errors_total",
                "Routing failures",
                &["tenant", "reason"]
            )
            .ok()?,
            ttft: register_histogram_vec!(
                "hydra_ttft_seconds",
                "Time to first token (request start → first response chunk)",
                &["tenant", "provider", "model"],
                LATENCY_BUCKETS.to_vec()
            )
            .ok()?,
            cached_tokens: register_int_counter_vec!(
                "hydra_cached_tokens_total",
                "Prompt-cache hit tokens (cached_tokens / cache_read_input_tokens)",
                &["tenant", "provider", "model"]
            )
            .ok()?,
            // ── Admission control metrics (design-admission-queue §10) ─────
            permit_inflight: register_int_gauge_vec!(
                "hydra_permit_inflight",
                "In-flight admission permits per provider (held = actively sending/streaming)",
                &["provider"]
            )
            .ok()?,
            permit_available: register_int_gauge_vec!(
                "hydra_permit_available",
                "Available (free) admission permits per provider",
                &["provider"]
            )
            .ok()?,
            queue_depth: register_int_gauge_vec!(
                "hydra_queue_depth",
                "Current queued waiters per provider (waiting for a permit)",
                &["provider"]
            )
            .ok()?,
            queue_wait: register_histogram_vec!(
                "hydra_queue_wait_seconds",
                "Time spent waiting in the admission queue before a permit was granted",
                &["provider"],
                LATENCY_BUCKETS.to_vec()
            )
            .ok()?,
            queue_drops: register_int_counter_vec!(
                "hydra_queue_drops_total",
                "Admission denials (queue full / timeout / closed)",
                &["provider", "reason"]
            )
            .ok()?,
            admission_decisions: register_int_counter_vec!(
                "hydra_admission_decisions_total",
                "Admission decisions (acquired / queued / dropped)",
                &["provider", "outcome"]
            )
            .ok()?,
            // ── Mid-stream observability (P2-9) ───────────────────────────
            mid_stream_errors: register_int_counter_vec!(
                "hydra_mid_stream_errors_total",
                "Mid-stream failures after 200 + first byte sent (no failover possible)",
                &["provider"]
            )
            .ok()?,
            // ── Control plane (cluster P1) ────────────────────────────────
            control_poll: register_int_counter_vec!(
                "hydra_control_poll_total",
                "Control-channel poll outcomes (result=ok|error)",
                &["result"]
            )
            .ok()?,
            control_snapshot_version: register_int_gauge!(
                "hydra_control_snapshot_version",
                "Last config snapshot version applied from the control plane"
            )
            .ok()?,
        })
    })
    .as_ref()
}

// ---------------------------------------------------------------------------
// record_* helpers (instrumentation call-sites)
// ---------------------------------------------------------------------------

/// Increment `hydra_requests_total` (one per proxied request, including fails).
#[allow(dead_code)]
pub fn record_request(tenant: &str, provider: &str, model: &str, status: u16) {
    if let Some(m) = metrics() {
        m.requests
            .with_label_values(&[tenant, provider, model, &status.to_string()])
            .inc();
    }
}

/// Observe end-to-end request latency in seconds.
#[allow(dead_code)]
pub fn record_request_duration(tenant: &str, provider: &str, model: &str, secs: f64) {
    if let Some(m) = metrics() {
        m.request_duration
            .with_label_values(&[tenant, provider, model])
            .observe(secs);
    }
}

/// Observe upstream (time-to-first-byte) latency in seconds.
#[allow(dead_code)]
pub fn record_upstream_duration(provider: &str, model: &str, secs: f64) {
    if let Some(m) = metrics() {
        m.upstream_duration
            .with_label_values(&[provider, model])
            .observe(secs);
    }
}

/// Increment a failover retry (`stage` = "connect" | "proxy").
#[allow(dead_code)]
pub fn record_retry(tenant: &str, model: &str, stage: &str) {
    if let Some(m) = metrics() {
        m.retries.with_label_values(&[tenant, model, stage]).inc();
    }
}

/// Increment token usage (`kind` = "prompt" | "completion") by `n`.
#[allow(dead_code)]
pub fn record_tokens(tenant: &str, provider: &str, model: &str, kind: &str, n: u64) {
    if let Some(m) = metrics() {
        m.tokens
            .with_label_values(&[tenant, provider, model, kind])
            .inc_by(n);
    }
}

/// Record an auth verdict (`verdict` = "allowed" | "denied",
/// `source` = "hit" | "miss" | "local").
#[allow(dead_code)]
pub fn record_auth_decision(tenant: &str, verdict: &str, source: &str) {
    if let Some(m) = metrics() {
        m.auth_decisions
            .with_label_values(&[tenant, verdict, source])
            .inc();
    }
}

/// Increment the auth-upstream-error counter for a tenant.
#[allow(dead_code)]
pub fn record_auth_upstream_error(tenant: &str) {
    if let Some(m) = metrics() {
        m.auth_upstream_error.with_label_values(&[tenant]).inc();
    }
}

/// Set the auth-cache-size gauge.
#[allow(dead_code)]
pub fn record_auth_cache_size(n: usize) {
    if let Some(m) = metrics() {
        m.auth_cache_size.set(n as i64);
    }
}

/// Set `hydra_breaker_dead{provider}` to `val` (1 dead / 0 alive).
#[allow(dead_code)]
pub fn record_breaker_dead(provider: &str, val: i64) {
    if let Some(m) = metrics() {
        m.breaker_dead.with_label_values(&[provider]).set(val);
    }
}

/// Increment a breaker transition (`to` = "dead" | "alive").
#[allow(dead_code)]
pub fn record_breaker_transition(provider: &str, to: &str) {
    if let Some(m) = metrics() {
        m.breaker_transitions
            .with_label_values(&[provider, to])
            .inc();
    }
}

/// Increment a rate-limit rejection (`dim` = "count" | "token").
#[allow(dead_code)]
pub fn record_limit_rejected(tenant: &str, role: &str, dim: &str) {
    if let Some(m) = metrics() {
        m.limit_rejected
            .with_label_values(&[tenant, role, dim])
            .inc();
    }
}

/// Increment a routing failure (`reason` = stable slug).
#[allow(dead_code)]
pub fn record_route_error(tenant: &str, reason: &str) {
    if let Some(m) = metrics() {
        m.route_errors.with_label_values(&[tenant, reason]).inc();
    }
}

/// Observe Time To First Token in seconds (`ttft_ms / 1000.0`).
#[allow(dead_code)]
pub fn record_ttft(tenant: &str, provider: &str, model: &str, secs: f64) {
    if let Some(m) = metrics() {
        m.ttft
            .with_label_values(&[tenant, provider, model])
            .observe(secs);
    }
}

/// Increment prompt-cache hit tokens by `n` (OpenAI `cached_tokens` /
/// Anthropic `cache_read_input_tokens`).
#[allow(dead_code)]
pub fn record_cached_tokens(tenant: &str, provider: &str, model: &str, n: u64) {
    if let Some(m) = metrics() {
        m.cached_tokens
            .with_label_values(&[tenant, provider, model])
            .inc_by(n);
    }
}

// ---------------------------------------------------------------------------
// Control-plane metrics (cluster P1)
// ---------------------------------------------------------------------------

/// Count a control-channel poll outcome (`result` = "ok" | "error").
#[allow(dead_code)]
pub fn record_control_poll(result: &str) {
    if let Some(m) = metrics() {
        m.control_poll.with_label_values(&[result]).inc();
    }
}

/// Set the last applied config snapshot version (edge/standby).
#[allow(dead_code)]
pub fn record_control_snapshot_version(version: u64) {
    if let Some(m) = metrics() {
        m.control_snapshot_version.set(version as i64);
    }
}

// ---------------------------------------------------------------------------
// Admission control metrics (design-admission-queue §10)
// ---------------------------------------------------------------------------

/// Set the in-flight permits gauge for a provider.
#[allow(dead_code)]
pub fn record_permit_inflight(provider: &str, n: i64) {
    if let Some(m) = metrics() {
        m.permit_inflight.with_label_values(&[provider]).set(n);
    }
}

/// Set the available (free) permits gauge for a provider.
#[allow(dead_code)]
pub fn record_permit_available(provider: &str, n: i64) {
    if let Some(m) = metrics() {
        m.permit_available.with_label_values(&[provider]).set(n);
    }
}

/// Set the queue-depth (current waiters) gauge for a provider.
#[allow(dead_code)]
pub fn record_queue_depth(provider: &str, n: i64) {
    if let Some(m) = metrics() {
        m.queue_depth.with_label_values(&[provider]).set(n);
    }
}

/// Observe a queue wait duration in seconds (time spent waiting for a permit).
#[allow(dead_code)]
pub fn record_queue_wait(provider: &str, secs: f64) {
    if let Some(m) = metrics() {
        m.queue_wait.with_label_values(&[provider]).observe(secs);
    }
}

/// Increment a queue drop (`reason` = "full" | "timeout" | "closed" | "client_gone").
#[allow(dead_code)]
pub fn record_queue_drop(provider: &str, reason: &str) {
    if let Some(m) = metrics() {
        m.queue_drops.with_label_values(&[provider, reason]).inc();
    }
}

/// Increment an admission decision (`outcome` = "acquired" | "queued" | "dropped").
#[allow(dead_code)]
pub fn record_admission_decision(provider: &str, outcome: &str) {
    if let Some(m) = metrics() {
        m.admission_decisions
            .with_label_values(&[provider, outcome])
            .inc();
    }
}

// ---------------------------------------------------------------------------
// Mid-stream observability (P2-9)
// ---------------------------------------------------------------------------

/// Increment `hydra_mid_stream_errors_total{provider}` — a streaming response
/// failed AFTER the 200 + first chunk was already sent to the client (the
/// point of no failover). Observability only; the existing close-connection
/// behavior stays.
#[allow(dead_code)]
pub fn record_mid_stream_error(provider: &str) {
    if let Some(m) = metrics() {
        m.mid_stream_errors.with_label_values(&[provider]).inc();
    }
}

// ---------------------------------------------------------------------------
// /metrics rendering (default registry)
// ---------------------------------------------------------------------------

/// Render the entire default prometheus registry as the text exposition format.
/// Used by the [`crate::admin::AdminService`] `/metrics` handler.
#[must_use]
pub fn render() -> String {
    let encoder = prometheus::TextEncoder::new();
    match encoder.encode_to_string(&prometheus::gather()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(target: "hydra::admin::metrics", error = %e, "failed to encode metrics");
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_contains_help_and_type() {
        // Touch a counter so at least one hydra_* family is present.
        record_request("t_test", "p_test", "m_test", 200);
        let out = render();
        assert!(out.contains("# HELP"), "exposition must include HELP lines");
        assert!(out.contains("# TYPE"), "exposition must include TYPE lines");
        assert!(
            out.contains("hydra_requests_total"),
            "requests counter must be registered"
        );
    }

    #[test]
    fn record_helpers_are_idempotent() {
        // Calling twice must not panic even if registered already.
        record_request("t", "p", "m", 200);
        record_request("t", "p", "m", 500);
        record_auth_decision("t", "allowed", "hit");
        record_breaker_transition("p", "dead");
        record_breaker_dead("p", 1);
        record_tokens("t", "p", "m", "prompt", 10);
        record_retry("t", "m", "connect");
        record_limit_rejected("t", "r", "count");
        record_route_error("t", "model_not_found");
        record_auth_cache_size(3);
        record_upstream_duration("p", "m", 0.1);
        record_request_duration("t", "p", "m", 0.2);
        record_auth_upstream_error("t");
        record_ttft("t", "p", "m", 0.35);
        record_cached_tokens("t", "p", "m", 42);
        // Admission metrics (design-admission-queue §10).
        record_permit_inflight("p", 3);
        record_permit_available("p", 5);
        record_queue_depth("p", 2);
        record_queue_wait("p", 0.012);
        record_queue_drop("p", "timeout");
        record_admission_decision("p", "acquired");
        // Mid-stream observability (P2-9).
        record_mid_stream_error("p");
    }

    #[test]
    fn allow_unused_registration_bindings() {
        use prometheus::register_int_counter;
        let _ = register_int_counter!("hydra_unused_test_marker", "test").ok();
        let _ = SNI_MISMATCH_METRIC;
    }
}
