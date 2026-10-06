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
//! | `hydra_auth_allow_ttl_capped_total` | counter | tenant | `http::AuthCache::set` |
//! | `hydra_tenant_api_usage_query_total` | counter | source, group_by, result | `tenant_api::handlers::usage` |
//! | `hydra_tenant_api_usage_query_seconds` | histogram | source | `tenant_api::handlers::usage` |
//! | `hydra_tenant_api_requests_total` | counter | endpoint, status | `tenant_api` response writers |
//! | `hydra_tenant_api_auth_failures_total` | counter | reason | `tenant_api::dispatch` gate |
//! | `hydra_tenant_api_auth_latency_seconds` | histogram | — | `tenant_api::dispatch` gate |
//! | `hydra_tenant_api_throttled_total` | counter | scope | `tenant_api::limit` |
//! | `hydra_tenant_api_invalidate_pending_total` | counter | — | `tenant_api::handlers::invalidate` (202) |
//! | `hydra_tenant_api_invalidate_converge_seconds` | histogram | result | `tenant_api::handlers::invalidate` |
//! | `hydra_invalidation_consumer_applied_id` | gauge | node | `cluster::events` consumer |
//! | `hydra_invalidation_consumer_lag_events` | gauge | node | `cluster::events` consumer |
//! | `hydra_invalidation_consumer_stalled_seconds` | gauge | node | `cluster::events` consumer (**the alerting signal**) |
//! | `hydra_auth_cache_size` | gauge | — | proxy `request_filter` |
//! | `hydra_breaker_dead` | gauge | provider | breaker transitions |
//! | `hydra_breaker_state_transitions_total` | counter | provider, to | breaker `on_failure`/`on_success` |
//! | `hydra_limit_rejected_total` | counter | tenant, role, dim | proxy `request_filter` (429). **`dim` is `count` or `tokens`** (measured 2026-09-30: the plural is the real value, so a rule written on `dim="token"` would never fire) |
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
//! | `hydra_admission_limits_stale_total` | counter | provider | admission module (requests admitted under limits a configuration change has not applied — restart needed, plan §2bi / D-14) |
//! | `hydra_config_snapshot_stale` | gauge | — | `admin::reload_best_effort` (1 = last reload failed, snapshot stale) |
//! | `hydra_listener_bound` | gauge | protocol | startup self-check in `main` (1 = the configured listener really accepts) |
//! | `hydra_listener_misconfig_total` | counter | kind | `listeners::plan` notes (certs without a TLS port / a TLS port without certs) |
//! | `hydra_usage_records_dropped_total` | counter | reason | usage sink (`channel_full` / `channel_closed` / `retention_cap`) |
//! | `hydra_mid_stream_errors_total` | counter | provider | proxy `stream_response` (mid-stream write/read failure after 200 sent) |
//! | `hydra_candidate_skipped_total` | counter | provider, reason | candidates dropped before any attempt — SELECTION (breaker_dead \| no_key; deliberate soft-disables are NOT counted, invalid_weight cannot occur) and the defensive failover-loop set (missing_config \| bad_endpoint \| no_key \| no_usable_key) |
//! | `hydra_listener_tenant_certs` | gauge | — | `tls::follow_snapshot` (certs in the current snapshot) |
//! | `hydra_upstream_first_byte_timeout_total` | counter | provider | proxy send path (upstream accepted, then sent no headers within the bound) |
//! | `hydra_upstream_stream_idle_timeout_total` | counter | provider | proxy stream path (upstream sent headers, then no body byte within the idle bound) |
//! | `hydra_invalidation_trimmed_total` | counter | — | invalidation stream trim task (entries dropped; whether or not a bump was needed) |
//! | `hydra_invalidation_generation_bumps_total` | counter | — | invalidation stream trim task (drops that a live consumer had NOT applied ⇒ every node clears its auth cache) |
//! | `hydra_replica_materialize_retries_total` | counter | outcome | replica materialization passes (attempt\|succeeded\|failed\|throttled) — `main.rs`'s convergence loop |
//! | `hydra_arachne_this_node_leader` | gauge | — | raft `ArachneControl` leader watch (1 = this node is the writer) |
//! | `hydra_arachne_leader_flips_total` | counter | — | raft `ArachneControl` leader watch (a change of answer) |
//! | `hydra_arachne_publish_total` | counter | result | `ArachneConfigStore::publish` + `ConfigPublisher` (ok\|not_leader\|quorum_unavailable\|error\|refused) |
//! | `hydra_arachne_config_bytes` | gauge | — | bytes in the tree the last publish committed (`ArachneConfigStore::publish`) |
//! | `hydra_arachne_quorum_unavailable_total` | counter | op | raft operation that failed because no majority was reachable (publish\|read) |
//! | `hydra_auth_cache_clear_total` | counter | layer,result | whole-cache clears (layer=l1|l2, result=ok|partial|error) |
//! | `hydra_admin_auth_failures_total` | counter | result | failed credential checks on the admin port, per GATE (result=<gate>_denied|<gate>_throttled, gate=admin|cluster) |
//!
//! The record helpers tolerate a `None` handle (failed registration) by becoming
//! a cheap no-op, so instrumentation can never break the hot path. The
//! `hydra_sni_host_mismatch_total` counter is owned by the W4b `tls` module
//! (registered there); it shares the same default registry and thus appears on
//! `/metrics` automatically once a TLS backend is compiled in.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use prometheus::{
    register_histogram, register_histogram_vec, register_int_counter, register_int_counter_vec,
    register_int_gauge, register_int_gauge_vec, Histogram, HistogramVec, IntCounter, IntCounterVec,
    IntGauge, IntGaugeVec,
};
use serde::Serialize;

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
    /// Allow writes whose TTL was clamped to `allow_ttl_max` (T7).
    allow_ttl_capped: IntCounterVec,
    /// E3 usage reads, by store / grouping / outcome (`result` includes
    /// `decode_error`, which the tenant sees only as "unavailable").
    tenant_api_usage_query: IntCounterVec,
    /// E3 usage-read latency, by store. A wide window is what makes this move,
    /// and it is the signal behind the `HYDRA_TENANT_API_USAGE_MAX_WINDOW_DAYS`
    /// knob.
    tenant_api_usage_query_seconds: HistogramVec,
    /// Every tenant-API response, by endpoint and status. This is the entry
    /// traffic view; it deliberately does NOT feed `hydra_requests_total`, which
    /// is the billed data-plane family.
    tenant_api_requests: IntCounterVec,
    /// Token-gate failures by reason. The reason vocabulary is low-cardinality
    /// and never contains a token.
    tenant_api_auth_failures: IntCounterVec,
    /// Token-gate latency. The gate is documented as zero-I/O (it reads the
    /// config snapshot); this is the regression detector for that claim.
    tenant_api_auth_latency: Histogram,
    /// Requests the tenant-API limiters refused, by dimension.
    tenant_api_throttled: IntCounterVec,
    /// E2 answers that were `202` — published but not confirmed everywhere.
    /// Persistently non-zero means some node is not consuming the stream.
    tenant_api_invalidate_pending: IntCounter,
    /// How long the convergence barrier took, by outcome.
    tenant_api_invalidate_converge: HistogramVec,
    /// Per-node consumer watermark (the barrier's data source), and how long it
    /// has been standing still.
    invalidation_consumer_applied_id: IntGaugeVec,
    invalidation_consumer_lag_events: IntGaugeVec,
    invalidation_consumer_stalled_seconds: IntGaugeVec,
    catalog_requests: IntCounterVec,
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
    /// Requests admitted under a gate whose limits are stale (config changed, restart
    /// needed) — see `record_admission_limits_stale` and plan §2bi / D-14.
    admission_limits_stale: IntCounterVec,
    /// Admission decisions by outcome.
    admission_decisions: IntCounterVec,
    // ── Mid-stream observability (P2-9) ─────────────────────────────────
    /// Mid-stream errors: a chunk read/write failed AFTER the 200 + first
    /// chunk was already sent to the client (failover impossible).
    mid_stream_errors: IntCounterVec,
    candidate_skipped: IntCounterVec,
    /// 1 = the last post-write `reload_all` failed and the in-memory
    /// snapshot is stale (later admin writes may have silently not taken
    /// effect). Alert on this (audit §3.14).
    config_snapshot_stale: IntGauge,
    /// Usage records the sink dropped, by reason. Billing data loss must be
    /// alertable, not just a WARN in the log (audit §3.9).
    usage_dropped: IntCounterVec,
    // ── Control plane (cluster P1) ──────────────────────────────────────
    /// Control-channel poll outcomes (result=ok|error).
    control_poll: IntCounterVec,
    /// Last config snapshot version applied (edge/standby).
    /// Entries dropped from the invalidation stream by the trim task, whether or
    /// not a generation bump was needed.
    ///
    /// With `invalidation_generation_bumps_total` this separates "we are trimming
    /// steadily because the fleet is busy" from "we are dropping events somebody
    /// had not read" — the two used to be the same event, and the whole chain was
    /// invisible.
    /// Replica materialization retries by outcome. `failed` non-zero with
    /// `throttled` growing means a node that cannot materialize (and therefore
    /// cannot lead) — the state that used to be both permanent and invisible.
    /// Whole-cache auth-cache clears, by layer and result. A `partial`/`error`
    /// on the L2 is the state that leaves revoked keys served until their allow
    /// TTL expires — previously only a `warn!` line, while
    /// `hydra_auth_cache_size` was set to 0 and claimed the cache was empty.
    /// Failed admin-token attempts by outcome. Before this existed the admin
    /// gate had no counter at all and logged at `debug!` (filtered out at the
    /// shipped `RUST_LOG=info`), so a brute-force run against the single secret
    /// gating every provider key was completely invisible.
    admin_auth_failures: IntCounterVec,
    auth_cache_clears: IntCounterVec,
    replica_materialize_retries: IntCounterVec,
    invalidation_trimmed: IntCounter,
    /// Trims that dropped an entry a LIVE consumer had not applied. Each one
    /// makes every node clear its whole auth cache (L1+L2), so a non-zero rate
    /// here means real convergence loss — or an event rate above
    /// `maxlen / trim_interval` with a consumer that cannot keep up.
    invalidation_generation_bumps: IntCounter,
    // ── Downstream listener topology (审核四 P4) ─────────────────────────
    /// 1 = the configured listener was verified to accept connections at
    /// startup (protocol=plain|tls); 0 = configured but NOT accepting — the
    /// shape of the 2026-09-16 outage (process healthy, data plane deaf).
    listener_bound: IntGaugeVec,
    /// Config combinations that leave certificates unserved or a configured
    /// port unusable, by kind. Non-zero means an operator decision is missing.
    listener_misconfig: IntCounterVec,
    /// Upstream attempts that established a connection and then produced no
    /// response HEADERS within `HYDRA_UPSTREAM_FIRST_BYTE_TIMEOUT_SECS`.
    ///
    /// Without this, the first-byte path returned before any counter was
    /// incremented, so an alert row written against it could never fire.
    upstream_first_byte_timeouts: IntCounterVec,
    /// Upstream streaming responses whose BODY stalled mid-stream: headers (or
    /// earlier chunks) arrived, then no byte for the whole
    /// `HYDRA_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS` window.
    ///
    /// This is the bound on the body now that the client-level total timeout is
    /// gone (it used to truncate every generation longer than 300 s), so it
    /// needs its own series: otherwise a wedged upstream is indistinguishable
    /// from one that simply closed early.
    upstream_stream_idle_timeouts: IntCounterVec,
    /// Tenant certificates currently in the config snapshot. Together with
    /// `hydra_listener_bound{protocol="tls"}` this makes "certificates are
    /// configured but no TLS listener is bound" ALERTABLE instead of a log line.
    ///
    /// Published from the snapshot FOLLOWER, not at boot: tenant certs are
    /// hot-reloaded at runtime, so a boot-time-only gauge would be permanently
    /// stale in exactly the scenario this alert targets.
    listener_tenant_certs: IntGauge,
    // ── Arachne control plane (ADR-0001) ────────────────────────────────
    //
    // The node registry that used to sit here is DELETED, and so are its two
    // series (`hydra_registry_nodes`, `hydra_registry_reaped_total`): the reaper
    // went with the registry (T4.1), so the metrics had no recorder left — their
    // two alert rows could never fire, which is the exact failure mode
    // `check_documented_metrics` and the §9.1 table exist to prevent. What
    // replaces them is the raft surface below: "is anyone the writer" and "is
    // publishing getting through" are the questions an operator actually has now.
    /// 1 while THIS node is the raft leader (the writer), labelled by node id.
    /// `sum()` over the fleet is 0 when the cluster has no writer — the alert that
    /// replaces "registry rows piling up".
    ///
    /// A `Vec`, not a bare gauge, and that is load-bearing: a bare `IntGauge` is
    /// exported with its zero as soon as it is registered, so on a SINGLE-NODE
    /// deployment (no raft node exists, the watch never runs) it would read 0 and
    /// an alert written as `sum(...) == 0` would fire forever on every deployment
    /// that is not a cluster. A `Vec` exports no series until a label set is used,
    /// so the series exists exactly where the question is meaningful — the same
    /// reason `hydra_invalidation_consumer_*` is labelled by node.
    arachne_this_node_leader: IntGaugeVec,
    /// How many times this node's answer to "am I the leader" CHANGED. A node
    /// that flaps is a cluster whose commit point keeps moving.
    arachne_leader_flips_total: IntCounter,
    /// Publish outcomes: `ok` (the head moved), `not_leader` (the library refused
    /// the commit), `quorum_unavailable` (no majority), `error` (anything else,
    /// including a malformed tree), `refused` (encoding rejected the config
    /// before a byte was sent — the size limit, an unkeyable id, a failed seal).
    arachne_publish_total: IntCounterVec,
    /// Bytes in the tree the last successful publish committed. The growth series
    /// for the capacity question R2 raises; the HARD signal is
    /// `publish_total{result="refused"}`, not a byte threshold invented here.
    arachne_config_bytes: IntGauge,
    /// Raft operations that failed because no majority was reachable, by
    /// operation (`publish`|`read`). Distinct from `error`: a lost quorum is a
    /// cluster-level fact with a different fix (start a majority) from a
    /// transport or codec failure.
    arachne_quorum_unavailable_total: IntCounterVec,
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
            allow_ttl_capped: register_int_counter_vec!(
                "hydra_auth_allow_ttl_capped_total",
                "Allow verdicts whose TTL was clamped to the configured maximum",
                &["tenant"]
            )
            .ok()?,
            tenant_api_usage_query: register_int_counter_vec!(
                "hydra_tenant_api_usage_query_total",
                "Tenant API usage reads, by store, grouping and outcome",
                &["source", "group_by", "result"]
            )
            .ok()?,
            tenant_api_usage_query_seconds: register_histogram_vec!(
                "hydra_tenant_api_usage_query_seconds",
                "Tenant API usage-read latency, by store",
                &["source"]
            )
            .ok()?,
            tenant_api_requests: register_int_counter_vec!(
                "hydra_tenant_api_requests_total",
                "Tenant API responses, by endpoint and status",
                &["endpoint", "status"]
            )
            .ok()?,
            tenant_api_auth_failures: register_int_counter_vec!(
                "hydra_tenant_api_auth_failures_total",
                "Tenant API token-gate failures, by reason",
                &["reason"]
            )
            .ok()?,
            tenant_api_auth_latency: register_histogram!(
                "hydra_tenant_api_auth_latency_seconds",
                "Tenant API token-gate latency (documented as zero-I/O)"
            )
            .ok()?,
            tenant_api_throttled: register_int_counter_vec!(
                "hydra_tenant_api_throttled_total",
                "Tenant API requests refused by a limiter, by dimension",
                &["scope"]
            )
            .ok()?,
            tenant_api_invalidate_pending: register_int_counter!(
                "hydra_tenant_api_invalidate_pending_total",
                "Invalidations published but not confirmed by every live node (202)"
            )
            .ok()?,
            tenant_api_invalidate_converge: register_histogram_vec!(
                "hydra_tenant_api_invalidate_converge_seconds",
                "Time to confirm an invalidation across the fleet, by outcome",
                &["result"]
            )
            .ok()?,
            invalidation_consumer_applied_id: register_int_gauge_vec!(
                "hydra_invalidation_consumer_applied_id",
                "Per-node applied watermark (stream id, ms part)",
                &["node"]
            )
            .ok()?,
            invalidation_consumer_lag_events: register_int_gauge_vec!(
                "hydra_invalidation_consumer_lag_events",
                "Events between the stream tail and this node's watermark",
                &["node"]
            )
            .ok()?,
            invalidation_consumer_stalled_seconds: register_int_gauge_vec!(
                "hydra_invalidation_consumer_stalled_seconds",
                "How long this node's applied watermark has not advanced (alert > 60)",
                &["node"]
            )
            .ok()?,
            catalog_requests: register_int_counter_vec!(
                "hydra_catalog_requests_total",
                "Tenant model catalog GETs answered locally (GET /v1/models)",
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
            admission_limits_stale: register_int_counter_vec!(
                "hydra_admission_limits_stale_total",
                "Requests admitted while a provider's admission limits were STALE (the gate \
                 is not resized on hot-reload; the configured limits take effect after a \
                 restart)",
                &["provider"]
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
            candidate_skipped: register_int_counter_vec!(
                "hydra_candidate_skipped_total",
                "Candidates dropped before any attempt, per provider: selection (breaker_dead|no_key; deliberate weight-0 soft-disables are not counted, and invalid_weight cannot occur: the schema forbids weight < 0) or the defensive failover-loop set (missing_config|bad_endpoint|no_key|no_usable_key)",
                &["provider", "reason"]
            )
            .ok()?,
            usage_dropped: register_int_counter_vec!(
                "hydra_usage_records_dropped_total",
                "Usage records dropped by the sink (reason=channel_full|channel_closed|retention_cap)",
                &["reason"]
            )
            .ok()?,
            config_snapshot_stale: register_int_gauge!(
                "hydra_config_snapshot_stale",
                "1 = the last post-write reload_all failed: the in-memory snapshot is stale"
            )
            .ok()?,
            // ── Control plane (cluster P1) ────────────────────────────────
            control_poll: register_int_counter_vec!(
                "hydra_control_poll_total",
                "Control-channel poll outcomes (result=ok|error)",
                &["result"]
            )
            .ok()?,
            admin_auth_failures: register_int_counter_vec!(
                "hydra_admin_auth_failures_total",
                "Failed credential attempts on the admin port, by gate and outcome (admin|cluster _denied|_throttled)",
                &["result"]
            )
            .ok()?,
            auth_cache_clears: register_int_counter_vec!(
                "hydra_auth_cache_clear_total",
                "Whole-cache auth-cache clears (layer=l1|l2, result=ok|partial|error)",
                &["layer", "result"]
            )
            .ok()?,
            replica_materialize_retries: register_int_counter_vec!(
                "hydra_replica_materialize_retries_total",
                "Replica materialization retries by outcome (attempt|succeeded|failed|throttled)",
                &["outcome"]
            )
            .ok()?,
            invalidation_trimmed: register_int_counter!(
                "hydra_invalidation_trimmed_total",
                "Invalidation stream entries dropped by the trim task"
            )
            .ok()?,
            invalidation_generation_bumps: register_int_counter!(
                "hydra_invalidation_generation_bumps_total",
                "Trims that dropped an entry a live consumer had not applied (whole-fleet cache clear)"
            )
            .ok()?,
            // ── Downstream listener topology ──────────────────────────────
            listener_bound: register_int_gauge_vec!(
                "hydra_listener_bound",
                "1 = this configured downstream listener accepts connections (protocol=plain|tls)",
                &["protocol"]
            )
            .ok()?,
            listener_misconfig: register_int_counter_vec!(
                "hydra_listener_misconfig_total",
                "Listener configuration that leaves certs unserved or a port unusable, by kind",
                &["kind"]
            )
            .ok()?,
            upstream_first_byte_timeouts: register_int_counter_vec!(
                "hydra_upstream_first_byte_timeout_total",
                "Upstream attempts that connected but sent no response headers within the bound",
                &["provider"]
            )
            .ok()?,
            upstream_stream_idle_timeouts: register_int_counter_vec!(
                "hydra_upstream_stream_idle_timeout_total",
                "Upstream streaming responses that sent no body byte within the idle bound",
                &["provider"]
            )
            .ok()?,
            listener_tenant_certs: register_int_gauge!(
                "hydra_listener_tenant_certs",
                "Tenant certificates present in the config snapshot"
            )
            .ok()?,
            // ── Arachne control plane (ADR-0001) ──────────────────────
            arachne_this_node_leader: register_int_gauge_vec!(
                "hydra_arachne_this_node_leader",
                "1 = this node currently answers the raft write probe; the fleet sum is the writer count (node = raft member name)",
                &["node"]
            )
            .ok()?,
            arachne_leader_flips_total: register_int_counter!(
                "hydra_arachne_leader_flips_total",
                "Changes of this node's own leadership answer (0 -> 1 or 1 -> 0)"
            )
            .ok()?,
            arachne_publish_total: register_int_counter_vec!(
                "hydra_arachne_publish_total",
                "Config publish outcomes (result=ok|not_leader|quorum_unavailable|error|refused)",
                &["result"]
            )
            .ok()?,
            arachne_config_bytes: register_int_gauge!(
                "hydra_arachne_config_bytes",
                "Bytes in the config tree the last successful publish committed"
            )
            .ok()?,
            arachne_quorum_unavailable_total: register_int_counter_vec!(
                "hydra_arachne_quorum_unavailable_total",
                "Raft operations refused for lack of a majority (op=publish|read)",
                &["op"]
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

/// Record a tenant model catalog GET answered locally (`GET /v1/models`, P0).
/// Error paths (unknown domain / disabled tenant / unauthorised key) short-
/// circuit before this point, so the counter counts served catalogs only.
#[allow(dead_code)]
pub fn record_catalog(tenant: &str) {
    if let Some(m) = metrics() {
        m.catalog_requests.with_label_values(&[tenant]).inc();
    }
}

/// Set the auth-cache-size gauge.
#[allow(dead_code)]
pub fn record_auth_cache_size(n: usize) {
    if let Some(m) = metrics() {
        m.auth_cache_size.set(n as i64);
    }
}

/// Record an allow verdict whose TTL was clamped to `allow_ttl_max` (T7).
/// Attributing this per tenant is the point: it tells an operator WHICH tenant
/// is asking for long TTLs when they are deciding whether to raise the knob.
pub fn record_allow_ttl_capped(tenant: &str) {
    if let Some(m) = metrics() {
        m.allow_ttl_capped.with_label_values(&[tenant]).inc();
    }
}

/// Record one tenant-API response.
pub fn record_tenant_api_request(endpoint: &str, status: u16) {
    if let Some(m) = metrics() {
        m.tenant_api_requests
            .with_label_values(&[endpoint, &status.to_string()])
            .inc();
    }
}

/// Current value of `hydra_tenant_api_requests_total{endpoint,status}` (tests).
///
/// `endpoint` is the low-cardinality route label (`whoami`, `invalidate`,
/// `usage`, `sub-tenants`, `sub-tenant-routes`) or `unrouted` for a path under
/// the reserved prefix that is not one of the five routes. A request refused by
/// the token gate is still attributed to its ROUTE, on both the read and the
/// write path.
#[must_use]
pub fn tenant_api_requests_total(endpoint: &str, status: u16) -> f64 {
    match metrics() {
        Some(m) => m
            .tenant_api_requests
            .with_label_values(&[endpoint, &status.to_string()])
            .get() as f64,
        None => 0.0,
    }
}

/// Record a token-gate failure and how long the (zero-I/O) gate took.
pub fn record_tenant_api_auth_failure(reason: &str, elapsed: std::time::Duration) {
    if let Some(m) = metrics() {
        m.tenant_api_auth_failures
            .with_label_values(&[reason])
            .inc();
        m.tenant_api_auth_latency.observe(elapsed.as_secs_f64());
    }
}

/// Record a successful token-gate decision (latency only: the gate's cost is
/// the point, not the outcome).
pub fn record_tenant_api_auth_latency(elapsed: std::time::Duration) {
    if let Some(m) = metrics() {
        m.tenant_api_auth_latency.observe(elapsed.as_secs_f64());
    }
}

/// Record a limiter refusal. `scope` is `ip` | `token` | `tenant` | `invalidate`.
pub fn record_tenant_api_throttled(scope: &str) {
    if let Some(m) = metrics() {
        m.tenant_api_throttled.with_label_values(&[scope]).inc();
    }
}

/// Record an invalidation that was published but not confirmed everywhere.
pub fn record_tenant_api_invalidate_pending() {
    if let Some(m) = metrics() {
        m.tenant_api_invalidate_pending.inc();
    }
}

/// Record how long the convergence barrier took, by outcome.
pub fn record_tenant_api_invalidate_converge(result: &str, elapsed: std::time::Duration) {
    if let Some(m) = metrics() {
        m.tenant_api_invalidate_converge
            .with_label_values(&[result])
            .observe(elapsed.as_secs_f64());
    }
}

/// Publish one consumer's watermark, its lag and how long the watermark has
/// stood still.
///
/// `stalled_seconds` is the only signal that distinguishes "this node is alive
/// and serving" from "this node is alive, serving and NOT consuming the
/// invalidation stream" — before it, that state was completely invisible (the
/// consumer only logged a warning and retried).
pub fn record_invalidation_consumer(
    node: &str,
    applied_id_ms: i64,
    lag_events: i64,
    stalled_seconds: i64,
) {
    if let Some(m) = metrics() {
        m.invalidation_consumer_applied_id
            .with_label_values(&[node])
            .set(applied_id_ms);
        m.invalidation_consumer_lag_events
            .with_label_values(&[node])
            .set(lag_events);
        m.invalidation_consumer_stalled_seconds
            .with_label_values(&[node])
            .set(stalled_seconds);
    }
}

/// Current value of `hydra_tenant_api_throttled_total{scope}` (tests).
#[must_use]
pub fn tenant_api_throttled_total(scope: &str) -> f64 {
    match metrics() {
        Some(m) => m.tenant_api_throttled.with_label_values(&[scope]).get() as f64,
        None => 0.0,
    }
}

/// Current value of `hydra_tenant_api_auth_failures_total{reason}` (tests).
#[must_use]
pub fn tenant_api_auth_failures_total(reason: &str) -> f64 {
    match metrics() {
        Some(m) => m
            .tenant_api_auth_failures
            .with_label_values(&[reason])
            .get() as f64,
        None => 0.0,
    }
}

/// Current value of `hydra_tenant_api_invalidate_pending_total` (tests).
#[must_use]
pub fn tenant_api_invalidate_pending_total() -> f64 {
    match metrics() {
        Some(m) => m.tenant_api_invalidate_pending.get() as f64,
        None => 0.0,
    }
}

/// Record one E3 usage read. `result` is `ok`, `store_unavailable`,
/// `decode_error`, or `result_too_large`; the latter three are the same 503 to
/// the tenant and are told apart here so an operator can distinguish a shape
/// drift from an unreachable store. `result_too_large` means the result
/// exceeded the gateway's response-size cap: the caller must narrow
/// `since`/`until` or reduce `group_by` — retrying does not help.
pub fn record_tenant_api_usage_query(
    source: &str,
    group_by: &str,
    result: &str,
    elapsed: std::time::Duration,
) {
    if let Some(m) = metrics() {
        m.tenant_api_usage_query
            .with_label_values(&[source, group_by, result])
            .inc();
        m.tenant_api_usage_query_seconds
            .with_label_values(&[source])
            .observe(elapsed.as_secs_f64());
    }
}

/// Current value of the E3 usage-read counter for one label combination, so a
/// test can assert that a failure is attributed (a `decode_error` is
/// indistinguishable from an unreachable store in the response, and telling them
/// apart is the operator's only signal of a shape drift).
#[must_use]
pub fn tenant_api_usage_query_total(source: &str, group_by: &str, result: &str) -> f64 {
    match metrics() {
        Some(m) => m
            .tenant_api_usage_query
            .with_label_values(&[source, group_by, result])
            .get() as f64,
        None => 0.0,
    }
}

/// Current value of the allow-TTL-capped counter for a tenant (0.0 when
/// metrics are unregistered, which mirrors a metric that was never recorded).
#[must_use]
pub fn allow_ttl_capped_total(tenant: &str) -> f64 {
    match metrics() {
        Some(m) => m.allow_ttl_capped.with_label_values(&[tenant]).get() as f64,
        None => 0.0,
    }
}

/// Count usage records the sink dropped, by `reason` ("channel_full",
/// "channel_closed", "retention_cap").
///
/// Usage records are billing data: the audit (§3.9) found them dropped with only
/// a WARN, so a silently degrading metering pipeline was invisible to operators.
/// Current value of `hydra_usage_dropped_total{reason}` (tests).
///
/// The whole point of the shutdown/retention drop reporting is that a lost usage
/// record is NEVER silent, so the counters are part of the contract and need to be
/// assertable — this is what makes "the final drain reported its loss" a checked
/// property instead of a claim.
#[must_use]
pub fn usage_dropped_total(reason: &str) -> u64 {
    match metrics() {
        Some(m) => m.usage_dropped.with_label_values(&[reason]).get(),
        None => 0,
    }
}

pub fn record_usage_drop(reason: &str, n: u64) {
    if let Some(m) = metrics() {
        m.usage_dropped.with_label_values(&[reason]).inc_by(n);
    }
}

/// Set `hydra_listener_bound{protocol}`: did the listener we configured
/// actually end up accepting connections at startup?
///
/// Its whole purpose is that our own "listener bound" log line is printed
/// BEFORE Pingora binds, and a bind failure there dies inside Pingora's service
/// task — the process keeps running and keeps answering `/healthz` while the
/// data plane has no listener. Alert on `hydra_listener_bound == 0`.
pub fn record_listener_bound(protocol: &str, bound: bool) {
    if let Some(m) = metrics() {
        m.listener_bound
            .with_label_values(&[protocol])
            .set(i64::from(bound));
    }
}

/// Count a listener-configuration combination that needs an operator decision
/// (`kind` = `certs_without_tls_port` / `tls_port_without_certs`). See
/// [`crate::listeners::PlanNote`].
pub fn record_listener_misconfig(kind: &str) {
    if let Some(m) = metrics() {
        m.listener_misconfig.with_label_values(&[kind]).inc();
    }
}

/// Publish whether the in-memory config snapshot is stale because the last
/// post-write `reload_all` failed (1) or is in sync with the DB (0).
///
/// This is the ops-visible signal the audit asked for in §3.14: a write that is
/// committed to SQLite but cannot be loaded leaves the API answering 200 while
/// the runtime keeps serving the PREVIOUS config — key rotation and revocation
/// silently stop taking effect. Alert on `hydra_config_snapshot_stale == 1`.
pub fn record_config_snapshot_stale(stale: bool) {
    if let Some(m) = metrics() {
        m.config_snapshot_stale.set(i64::from(stale));
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
/// `dim` is `"count"` or `"tokens"` — the exact label values, because the two gates record
/// different dimensions and a rule written against the wrong spelling can never match.
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

/// Count a failed credential attempt on either gate of the admin port.
///
/// `result` is `<gate>_denied` (answered 401) or `<gate>_throttled` (answered 429
/// after the per-peer budget), where `<gate>` is `admin` (the operator token) or
/// `cluster` (the internal control-plane token). The gate is part of the label so
/// either can be alerted on independently — the cluster gate had NO counter at all
/// before it shared this path, which is why nobody could see it being guessed at.
pub fn record_admin_auth_failure(result: &str) {
    if let Some(m) = metrics() {
        m.admin_auth_failures.with_label_values(&[result]).inc();
    }
}

/// Current value of `hydra_admin_auth_failures_total{result}` (tests).
///
/// `result` is `<gate>_denied` or `<gate>_throttled` with `gate` = `admin` |
/// `cluster`. Kept here so a test can assert that BOTH gates are metered — the
/// cluster gate's absence of any counter was the whole defect.
#[must_use]
pub fn admin_auth_failures_total(result: &str) -> f64 {
    match metrics() {
        Some(m) => m.admin_auth_failures.with_label_values(&[result]).get() as f64,
        None => 0.0,
    }
}

/// Count a whole-cache auth-cache clear by `layer` (`l1`/`l2`) and `result`
/// (`ok`/`partial`/`error`).
pub fn record_auth_cache_clear(layer: &str, result: &str) {
    if let Some(m) = metrics() {
        m.auth_cache_clears
            .with_label_values(&[layer, result])
            .inc();
    }
}

/// Count a replica materialization retry by `outcome`
/// (`attempt`/`succeeded`/`failed`/`throttled`).
pub fn record_replica_materialize_retry(outcome: &str) {
    if let Some(m) = metrics() {
        m.replica_materialize_retries
            .with_label_values(&[outcome])
            .inc();
    }
}

/// Count entries the invalidation trim dropped.
pub fn record_invalidation_trimmed(entries: i64) {
    if let Some(m) = metrics() {
        m.invalidation_trimmed.inc_by(entries.max(0) as u64);
    }
}

/// Count an invalidation-stream generation bump (every node clears its auth
/// cache in response).
pub fn record_invalidation_generation_bump() {
    if let Some(m) = metrics() {
        m.invalidation_generation_bumps.inc();
    }
}

/// Count an upstream first-byte timeout for `provider`.
pub fn record_upstream_first_byte_timeout(provider: &str) {
    if let Some(m) = metrics() {
        m.upstream_first_byte_timeouts
            .with_label_values(&[provider])
            .inc();
    }
}

/// Count an upstream stream that stalled mid-body for `provider`
/// (`HYDRA_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS`; see the field docs).
pub fn record_upstream_stream_idle_timeout(provider: &str) {
    if let Some(m) = metrics() {
        m.upstream_stream_idle_timeouts
            .with_label_values(&[provider])
            .inc();
    }
}

/// Publish how many tenant certificates the current snapshot holds.
///
/// Called by the certificate follower on every snapshot change (and once at
/// startup), so the value tracks hot-reloaded certs.
pub fn record_listener_tenant_certs(n: usize) {
    if let Some(m) = metrics() {
        m.listener_tenant_certs.set(n as i64);
    }
}

// ---------------------------------------------------------------------------
// Arachne control plane (ADR-0001) — the raft surface an operator alerts on
// ---------------------------------------------------------------------------

/// Publish whether THIS node is the raft leader.
///
/// Called by the leader watch on every round, so the fleet `sum()` answers "does
/// the cluster have a writer" — the question the retired registry answered with a
/// heartbeat table, and the one an operator must be able to alert on.
pub fn record_arachne_leader(node: &str, is_leader: bool) {
    if let Some(m) = metrics() {
        m.arachne_this_node_leader
            .with_label_values(&[node])
            .set(i64::from(is_leader));
    }
}

/// Count a CHANGE in this node's own leadership answer (either direction).
///
/// Separate from the gauge because a flapping leader is invisible in a sampled
/// gauge: by the time a scrape lands the answer is true again.
pub fn record_arachne_leader_flip() {
    if let Some(m) = metrics() {
        m.arachne_leader_flips_total.inc();
    }
}

/// Count a config publish by outcome.
///
/// The outcomes are not interchangeable, which is why they are one family with a
/// label rather than one counter: `not_leader` is momentary (the library retries
/// on the next write), `quorum_unavailable` is an outage, `refused` is a config
/// that can never be published as it stands, and `error` is a bug or a
/// transport failure.
pub fn record_arachne_publish(result: &str) {
    if let Some(m) = metrics() {
        m.arachne_publish_total.with_label_values(&[result]).inc();
    }
}

/// Publish the size of the tree the last publish committed.
pub fn record_arachne_config_bytes(bytes: usize) {
    if let Some(m) = metrics() {
        m.arachne_config_bytes.set(bytes as i64);
    }
}

/// Count a raft operation that failed because no majority was reachable.
///
/// `op` is `publish` or `read`: a lost quorum makes the cluster unable to commit
/// a new config while still serving the materialized one, and the two operations
/// show that asymmetry at different times.
pub fn record_arachne_quorum_unavailable(op: &str) {
    if let Some(m) = metrics() {
        m.arachne_quorum_unavailable_total
            .with_label_values(&[op])
            .inc();
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

/// Count requests admitted while a provider's gate enforces STALE limits (a configuration
/// change that has not taken effect because gates are not resized — plan §2bi / D-14).
///
/// A counter rather than a gauge on purpose: what an operator wants to know is "is traffic
/// still being admitted under the old limits?", and `rate()` on this answers that
/// unambiguously. The gate also logs a WARN the first time it sees a given configuration.
#[allow(dead_code)]
pub fn record_admission_limits_stale(provider: &str) {
    if let Some(m) = metrics() {
        m.admission_limits_stale
            .with_label_values(&[provider])
            .inc();
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

/// Current value of `hydra_candidate_skipped_total{provider,reason}` (tests).
#[must_use]
pub fn candidate_skipped_total(provider: &str, reason: &str) -> u64 {
    match metrics() {
        Some(m) => m
            .candidate_skipped
            .with_label_values(&[provider, reason])
            .get(),
        None => 0,
    }
}

/// Record a candidate that was skipped BEFORE any upstream attempt, with a
/// low-cardinality `reason`.
///
/// Two families share this counter. The first one produces series today; the
/// second is defensive and, as measured, unreachable with a valid config:
///
/// 1. **Candidate SELECTION** (`breaker_dead` | `no_key` | `invalid_weight`) —
///    the drops `router::resolve_detailed` reports in its `excluded` list; the
///    proxy records them right after a successful resolve. This is the family
///    that answers "which provider went quiet": a provider whose last api-key was
///    deleted simply stopped being chosen, and because the tenant's other
///    providers kept serving, neither the response nor
///    `hydra_route_errors_total{tenant}` (which only counts FAILED requests)
///    showed anything at all.
///
///    NOT included, deliberately: `soft_disabled` (`weight == 0`). That is a
///    supported, intentional action (`ops.md` documents weight 0 as
///    soft-disabled), so counting it per request would make the series track
///    traffic and an `increase(...) > 0` rule fire forever on a healthy fleet.
///    `invalid_weight` (`weight < 0`) IS counted and is defensive vocabulary: the
///    schema forbids it (`migrations/0001_init.sql` has `CHECK (weight >= 0)`) and
///    every provider in memory comes from those rows, so no running process can
///    produce this series — a review measured that, which is why `ops.md` §9.1
///    leaves it out of the alert expression. It stays because hand-built
///    `ConfigData` (unit tests, future loaders) can carry it.
/// 2. **Per-candidate failover loop** (`missing_config` | `bad_endpoint` |
///    `no_key` | `no_usable_key`) — defensive; expect no series from it.
///    **Measured on 2026-09-29:** with a VALID config those four branches are
///    UNREACHABLE — deleting every `provider_key` row and reloading makes the
///    request fail with "no candidates" rather than entering the loop, and an
///    unparseable endpoint makes `ConfigStore::reload_all` return
///    `FatalValidation`. (That path leaves the snapshot unpublished; the replica
///    hydrate path `ConfigStore::apply_snapshot` does not re-validate, so
///    "never published" rests on the upstream invariant that only a validated
///    leader snapshot is shipped — see `store.rs`.) Those branches fire only if a
///    future change lets such a candidate reach the loop.
///
/// `no_key` is deliberately the SAME string in both families: an operator
/// alerting on "a provider has no usable key" should not have to know which
/// code path noticed.
pub fn record_candidate_skipped(provider: &str, reason: &str) {
    if let Some(m) = metrics() {
        m.candidate_skipped
            .with_label_values(&[provider, reason])
            .inc();
    }
}

/// Increment `hydra_mid_stream_errors_total{provider}` — a streaming response
/// failed AFTER the 200 + first chunk was already sent to the client (the point
/// of no failover). Observability only; the existing close-connection behavior
/// stays.
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

// ---------------------------------------------------------------------------
// Usage aggregation - `GET /api/v1/stats/usage` (Admin UI Stats page)
// ---------------------------------------------------------------------------

/// Per-dimension usage totals for one tenant or provider (derived live from the
/// prometheus counters - cumulative since process start, not time-windowed).
#[derive(Serialize, Default, Clone, Debug)]
pub struct UsageRow {
    /// Tenant id or provider id (as recorded in the metric labels).
    pub name: String,
    /// Proxied request count (`hydra_requests_total`).
    pub requests: u64,
    /// Total tokens (`hydra_tokens_total`, prompt + completion).
    pub tokens: u64,
    /// Prompt (input) tokens.
    pub tokens_prompt: u64,
    /// Completion (output) tokens.
    pub tokens_completion: u64,
}

/// Whole-gateway totals for the Stats page header.
#[derive(Serialize, Default, Clone, Debug)]
pub struct UsageTotals {
    pub requests: u64,
    pub tokens: u64,
    pub tokens_prompt: u64,
    pub tokens_completion: u64,
    /// Number of distinct tenants / providers seen in the counters.
    pub tenants: usize,
    pub providers: usize,
}

/// Response body of `GET /api/v1/stats/usage`.
#[derive(Serialize, Debug)]
pub struct UsageAggregate {
    /// RFC3339 timestamp of the aggregation (for the UI "updated at" line).
    pub generated_at: String,
    pub totals: UsageTotals,
    /// Sorted by tokens desc, then requests desc, then name.
    pub by_tenant: Vec<UsageRow>,
    pub by_provider: Vec<UsageRow>,
}

impl UsageRow {
    fn add_request(&mut self, n: u64) {
        self.requests = self.requests.saturating_add(n);
    }
    fn add_tokens(&mut self, prompt: u64, completion: u64) {
        self.tokens = self
            .tokens
            .saturating_add(prompt)
            .saturating_add(completion);
        self.tokens_prompt = self.tokens_prompt.saturating_add(prompt);
        self.tokens_completion = self.tokens_completion.saturating_add(completion);
    }
    fn has_usage(&self) -> bool {
        self.requests > 0 || self.tokens > 0
    }
}

/// Read one label value from a prometheus metric label set ("" when absent).
fn label_value<'a>(labels: &'a [prometheus::proto::LabelPair], key: &str) -> &'a str {
    for lp in labels {
        if lp.get_name() == key {
            return lp.get_value();
        }
    }
    ""
}

/// Aggregate the live prometheus counters into per-tenant / per-provider usage
/// (Admin UI Stats page, design §17). Only the request + token counters are
/// consumed; every other family is skipped. Zero-count series (e.g. a label
/// combo touched but never incremented) are dropped from the output.
#[must_use]
pub fn usage_aggregate() -> UsageAggregate {
    let mut tenants: BTreeMap<String, UsageRow> = BTreeMap::new();
    let mut providers: BTreeMap<String, UsageRow> = BTreeMap::new();
    let mut totals = UsageTotals::default();

    for family in prometheus::gather() {
        match family.get_name() {
            "hydra_requests_total" => {
                for m in family.get_metric() {
                    let n = counter_value(m.get_counter().get_value());
                    if n == 0 {
                        continue;
                    }
                    let tenant = label_value(m.get_label(), "tenant").to_string();
                    let provider = label_value(m.get_label(), "provider").to_string();
                    totals.requests = totals.requests.saturating_add(n);
                    tenants.entry(tenant).or_default().add_request(n);
                    providers.entry(provider).or_default().add_request(n);
                }
            }
            "hydra_tokens_total" => {
                for m in family.get_metric() {
                    let n = counter_value(m.get_counter().get_value());
                    if n == 0 {
                        continue;
                    }
                    let tenant = label_value(m.get_label(), "tenant").to_string();
                    let provider = label_value(m.get_label(), "provider").to_string();
                    let kind = label_value(m.get_label(), "kind");
                    let (prompt, completion) = if kind == "completion" {
                        (0, n)
                    } else {
                        (n, 0) // "prompt" (or any unknown kind is counted as prompt)
                    };
                    totals.tokens = totals
                        .tokens
                        .saturating_add(prompt)
                        .saturating_add(completion);
                    totals.tokens_prompt = totals.tokens_prompt.saturating_add(prompt);
                    totals.tokens_completion = totals.tokens_completion.saturating_add(completion);
                    tenants
                        .entry(tenant)
                        .or_default()
                        .add_tokens(prompt, completion);
                    providers
                        .entry(provider)
                        .or_default()
                        .add_tokens(prompt, completion);
                }
            }
            _ => {}
        }
    }

    // Move the map keys into the rows (the entry API only defaulted them),
    // order deterministically (usage desc, then name asc) and drop rows
    // without any recorded usage.
    let mut by_tenant: Vec<UsageRow> = tenants
        .into_iter()
        .map(|(name, mut row)| {
            row.name = name;
            row
        })
        .filter(UsageRow::has_usage)
        .collect();
    let mut by_provider: Vec<UsageRow> = providers
        .into_iter()
        .map(|(name, mut row)| {
            row.name = name;
            row
        })
        .filter(UsageRow::has_usage)
        .collect();
    by_tenant.sort_by(usage_cmp);
    by_provider.sort_by(usage_cmp);

    totals.tenants = by_tenant.len();
    totals.providers = by_provider.len();

    UsageAggregate {
        generated_at: chrono::Utc::now().to_rfc3339(),
        totals,
        by_tenant,
        by_provider,
    }
}

fn counter_value(v: f64) -> u64 {
    if v.is_finite() && v > 0.0 {
        v as u64
    } else {
        0
    }
}

/// Desc by tokens, then requests, then name asc (deterministic for tests/UI).
fn usage_cmp(a: &UsageRow, b: &UsageRow) -> std::cmp::Ordering {
    b.tokens
        .cmp(&a.tokens)
        .then_with(|| b.requests.cmp(&a.requests))
        .then_with(|| a.name.cmp(&b.name))
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

    /// The raft surface must actually be PUBLISHED, not merely panic-free.
    ///
    /// This replaces the same test for the retired registry metrics, which had
    /// exactly this purpose and outlived their recorder: a `record_*` helper that
    /// nothing calls is indistinguishable from a working signal until someone
    /// writes an alert rule against it. These five families are what an operator
    /// alerts on now, so the exposition is asserted rather than assumed — and the
    /// LABELS with it, because a renamed label value is a rule that stops firing.
    #[test]
    fn arachne_metrics_reach_the_exposition() {
        record_arachne_leader("probe-node", true);
        record_arachne_leader_flip();
        record_arachne_publish("ok");
        record_arachne_publish("quorum_unavailable");
        record_arachne_publish("refused");
        record_arachne_config_bytes(4096);
        record_arachne_quorum_unavailable("read");
        let out = render();
        for needle in [
            "hydra_arachne_this_node_leader{node=\"probe-node\"} 1",
            "hydra_arachne_leader_flips_total 1",
            "hydra_arachne_publish_total{result=\"ok\"} 1",
            "hydra_arachne_publish_total{result=\"quorum_unavailable\"} 1",
            "hydra_arachne_publish_total{result=\"refused\"} 1",
            "hydra_arachne_config_bytes 4096",
            "hydra_arachne_quorum_unavailable_total{op=\"read\"} 1",
        ] {
            assert!(out.contains(needle), "missing {needle}:\n{out}");
        }
        // The retired series must be GONE, or an old dashboard keeps rendering a
        // flat line that no longer means anything.
        for dead in [
            "hydra_registry_nodes",
            "hydra_registry_reaped_total",
            "hydra_control_snapshot_version",
        ] {
            assert!(!out.contains(dead), "{dead} is retired but still exposed");
        }
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

    #[test]
    fn usage_aggregate_groups_by_tenant_and_provider() {
        // Unique labels so the assertion is delta-based (the registry is
        // process-global and shared with other tests in this binary).
        let tag = format!("agg_{}", std::process::id());
        let tenant_a = format!("{tag}_tenant_a");
        let tenant_b = format!("{tag}_tenant_b");
        let prov_a = format!("{tag}_prov_a");
        let prov_b = format!("{tag}_prov_b");

        record_request(&tenant_a, &prov_a, "m", 200);
        record_request(&tenant_a, &prov_a, "m", 200);
        record_request(&tenant_a, &prov_b, "m", 500);
        record_request(&tenant_b, &prov_a, "m", 200);
        record_tokens(&tenant_a, &prov_a, "m", "prompt", 100);
        record_tokens(&tenant_a, &prov_a, "m", "completion", 40);
        record_tokens(&tenant_a, &prov_b, "m", "prompt", 10);

        let agg = usage_aggregate();

        // Totals: 4 requests, 150 tokens (110 prompt + 40 completion).
        assert!(
            agg.totals.requests >= 4,
            "totals.requests={}",
            agg.totals.requests
        );
        assert!(
            agg.totals.tokens >= 150,
            "totals.tokens={}",
            agg.totals.tokens
        );
        assert!(
            agg.totals.tokens_prompt >= 110,
            "totals.tokens_prompt={}",
            agg.totals.tokens_prompt
        );
        assert!(
            agg.totals.tokens_completion >= 40,
            "totals.tokens_completion={}",
            agg.totals.tokens_completion
        );

        // By tenant: tenant_a sees 3 requests / 150 tokens.
        let row_a = agg
            .by_tenant
            .iter()
            .find(|r| r.name == tenant_a)
            .expect("tenant_a row");
        assert!(row_a.requests >= 3, "tenant_a.requests={}", row_a.requests);
        assert!(row_a.tokens >= 150, "tenant_a.tokens={}", row_a.tokens);
        assert!(
            row_a.tokens_prompt >= 110,
            "tenant_a.tokens_prompt={}",
            row_a.tokens_prompt
        );
        assert!(
            row_a.tokens_completion >= 40,
            "tenant_a.tokens_completion={}",
            row_a.tokens_completion
        );
        let row_b = agg
            .by_tenant
            .iter()
            .find(|r| r.name == tenant_b)
            .expect("tenant_b row");
        assert!(row_b.requests >= 1, "tenant_b.requests={}", row_b.requests);
        assert_eq!(row_b.tokens, 0, "tenant_b has no token usage");

        // By provider: prov_a sees 3 requests / 140 tokens.
        let row_pa = agg
            .by_provider
            .iter()
            .find(|r| r.name == prov_a)
            .expect("prov_a row");
        assert!(row_pa.requests >= 3, "prov_a.requests={}", row_pa.requests);
        assert!(row_pa.tokens >= 140, "prov_a.tokens={}", row_pa.tokens);
        assert!(
            row_pa.tokens_prompt >= 100,
            "prov_a.tokens_prompt={}",
            row_pa.tokens_prompt
        );
        assert!(
            row_pa.tokens_completion >= 40,
            "prov_a.tokens_completion={}",
            row_pa.tokens_completion
        );
        let row_pb = agg
            .by_provider
            .iter()
            .find(|r| r.name == prov_b)
            .expect("prov_b row");
        assert!(row_pb.requests >= 1, "prov_b.requests={}", row_pb.requests);
        assert!(row_pb.tokens >= 10, "prov_b.tokens={}", row_pb.tokens);

        // Sorting: primary key is tokens desc.
        if let (Some(first), Some(second)) = (agg.by_tenant.first(), agg.by_tenant.get(1)) {
            assert!(
                first.tokens >= second.tokens,
                "by_tenant must be tokens-desc, got {} vs {}",
                first.tokens,
                second.tokens
            );
        }
    }
}
