//! # Control-flow forwarding (cluster P3)
//!
//! A standby leader-candidate serves **reads** from its local replica but
//! forwards every admin **mutation** to the active leader, so operators can
//! point their admin tools (REST / UI / CLI) at ANY leader-candidate node and
//! failover stays transparent to them.
//!
//! **Fail-closed**: when the active is unreachable the standby answers 503 —
//! it never "takes over" a write on its own. Taking over is exclusively the
//! lease machine's job (`cluster::lease`), so a partition can never produce
//! two writers via the forwarding path.
//!
//! **Forward target = the ACTUAL lease holder** (cluster P2/P4): the target
//! is resolved live from the node registry at forward time
//! (`forward_target_from_registry`) — never from a static
//! `HYDRA_CONTROL_URL`. A static URL cannot track the lease across failover,
//! and for a primary leader candidate it may point at the node ITSELF, which
//! would make a standby forward every mutation back into itself in an
//! infinite loop (the original self-forward bug).
//!
//! **Forward-once marker** (`FORWARD_ONCE_HEADER`): every forwarded
//! mutation carries the marker, and a node that is not the active leader
//! must never forward a request that already carries it. This turns ANY
//! (self- or mutual-) forward loop into an immediate fail-closed 503 instead
//! of a 5 s timeout recursion — a belt-and-suspenders guard underneath the
//! registry resolution.

use std::time::Duration;

use http::{HeaderMap, Response};

/// Default forward timeout for admin mutations (generous; admin ops are rare).
const DEFAULT_FORWARD_TIMEOUT_SECS: u64 = 5;

/// The forward timeout in SECONDS (`HYDRA_FORWARD_TIMEOUT_SECS`, default 5).
///
/// Seconds rather than a `Duration` because the value is quoted back to the
/// caller in the `504 forward_result_unknown` message.
fn forward_timeout_secs() -> u64 {
    parse_forward_timeout_secs(std::env::var("HYDRA_FORWARD_TIMEOUT_SECS").ok().as_deref())
}

/// The parse half of [`forward_timeout_secs`], kept PURE so the tests do not
/// mutate the process environment (parallel-safe, like the rest of this crate's
/// config helpers). `0`, garbage and a missing value all mean "use the default":
/// a zero-second forward timeout would fail every forward for no reason.
fn parse_forward_timeout_secs(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(DEFAULT_FORWARD_TIMEOUT_SECS)
}

/// Why a forward attempt failed.
///
/// The distinction is LOAD-BEARING: a timeout cannot tell whether the leader
/// already committed the write, while a connect error proves the request never
/// left this node. Reporting both as "failed, nothing was written" was the
/// defect (a standby told the operator the write did not happen when it
/// might have).
#[derive(Debug, thiserror::Error)]
pub enum ForwardError {
    /// The leader's response HEADERS arrived but the exchange failed afterwards.
    ///
    /// As ambiguous as [`Self::Timeout`]: the leader demonstrably received and
    /// started answering the request, so the write may already have been applied.
    /// Classifying this as a definite failure would be the same class of lie the
    /// timeout/connect split exists to prevent.
    #[error("the leader answered but the response could not be read: {0}")]
    AfterResponse(String),

    /// A timeout AFTER the connection was established: the request was written,
    /// so the leader may already have processed it.
    ///
    /// `secs` is the TOTAL deadline that actually elapsed (the configured bound
    /// plus [`CONNECT_SLACK_SECS`]), i.e. the number the operator really waited —
    /// not the raw configuration value.
    #[error(
        "leader forward timed out after {secs}s; the write may already have landed on the leader"
    )]
    Timeout { secs: u64 },
    #[error("{0}")]
    Other(String),
}

impl ForwardError {
    /// Classify a transport error.
    ///
    /// ORDER IS SEMANTIC: `is_connect()` is checked FIRST.
    ///
    /// That order only works because the client sets a CONNECT bound (see
    /// [`client`]): with the request-level deadline alone, a SYN-dropping address
    /// (a dead pod whose IP is still routed — the common Kubernetes case)
    /// produces a bare `TimedOut` with `is_connect() == false`, so a request that
    /// never left this node would be reported as "the outcome is unknown". This
    /// was MEASURED, not assumed — the table is in [`client`].
    ///
    /// So: a connect failure (refused, unreachable, connect-timeout) is a
    /// DEFINITE failure, while a timeout after the connection was established is
    /// genuinely ambiguous, because the leader may already have processed it.
    fn from_transport(e: &reqwest::Error, secs: u64) -> Self {
        if e.is_connect() {
            Self::Other(format!("forward to active failed (connection): {e}"))
        } else if e.is_timeout() {
            Self::Timeout { secs }
        } else {
            Self::Other(format!("forward to active failed: {e}"))
        }
    }
}

/// Forward-once marker: set on every forwarded admin mutation so the
/// receiving node can tell a mutation that already travelled through a
/// standby. A node that is not the active leader must never forward such a
/// request again — see the module docs (forward loop guard).
pub const FORWARD_ONCE_HEADER: &str = "x-hydra-forwarded";

/// Dedicated header carrying the tenant's Bearer token when a **tenant config
/// write** is forwarded to the leader (sub-tenant v2, decision A-2, precond. 5).
///
/// It is a SEPARATE header from `Authorization` on purpose: `Authorization`
/// carries the NODE's cluster token (who the sender is), while this header
/// carries the TENANT's Bearer (who the write is on behalf of). Keeping the
/// tenant credential out of `Authorization` AND out of the request body means
/// it can be neither relayed by the generic admin forwarder nor leaked by the
/// body logging/diagnostics on the forward path.
pub const TENANT_TOKEN_HEADER: &str = "x-hydra-tenant-token";

/// Resolve the admin-mutation forward target for a standby: the ACTUAL lease
/// holder's registered control URL, looked up live in the cluster registry.
///
/// The static `HYDRA_CONTROL_URL` is deliberately NOT used here: for a
/// primary leader candidate it may point at THIS node itself (the
/// self-forward loop bug), and it never tracks the lease across failover.
///
/// `Ok(None)` ⇒ no forward target is resolvable right now (no lease holder,
/// the holder is this node, the holder is not registered, or the holder's
/// registered URL is this node's own — the self-forward guard) — the caller
/// must fail closed (503) and never fall back to a static URL.
#[cfg(feature = "cluster-redis")]
pub async fn forward_target_from_registry(
    registry: &crate::cluster::registry::NodeRegistry,
) -> Result<Option<String>, String> {
    match registry.active_leader_url().await {
        Ok(Some(url)) => {
            if url == registry.control_url() {
                tracing::warn!(
                    target: "hydra::cluster",
                    url = %url,
                    "forward target is this node's own control URL; refusing to forward (self-forward guard)"
                );
                return Ok(None);
            }
            Ok(Some(url))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(format!("registry lookup failed: {e}")),
    }
}

/// How much longer the TOTAL forward deadline is than the CONNECT bound.
///
/// The two bounds must differ, with the connect one SHORTER: reqwest's
/// request-level `.timeout()` is a TOTAL deadline whose expiry error carries no
/// connect marker, so if both fired together a black-holed leader could still be
/// misreported as "the outcome is unknown". This margin makes the connect timer
/// resolve first.
const CONNECT_SLACK_SECS: u64 = 2;

/// Build the client for ONE forward.
///
/// It carries BOTH bounds, and the CONNECT one is load-bearing: a SYN-dropping
/// address (a dead pod whose IP is still routed — the common Kubernetes case)
/// must be a DEFINITE failure, because the request provably never left this node.
/// MEASURED, not assumed — a black-holed address with `secs = 2`:
///
/// | client setup | elapsed | `is_connect()` | `is_timeout()` |
/// |---|---|---|---|
/// | request `.timeout(2s)` only | 2.00s | **false** | true |
/// | client `.connect_timeout(2s)` + request `.timeout(4s)` | 2.00s | **true** | true |
///
/// A per-call client gives up connection pooling, which is the right trade for a
/// rare admin mutation: a false "the write may have been applied" costs far more
/// than one extra handshake.
fn client_for(secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(secs))
        .pool_idle_timeout(Some(Duration::from_secs(90)))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Forward one admin request to the active leader's admin endpoint,
/// preserving the operator's `Authorization` (the fleet shares
/// `HYDRA_ADMIN_TOKEN`), the content type and the trace id. The forwarded
/// request carries [`FORWARD_ONCE_HEADER`] so a receiving node that is not
/// the active leader fails closed instead of forwarding it again (forward
/// loop guard). Returns the active's response (status + content-type + body)
/// or an error message when the active is unreachable (the caller maps it to
/// 502/503).
pub async fn forward_mutation(
    base_url: &str,
    method: &str,
    path_and_query: &str,
    body: Vec<u8>,
    headers: &HeaderMap,
    trace_id: &str,
) -> Result<Response<Vec<u8>>, ForwardError> {
    forward_mutation_with_timeout(
        base_url,
        method,
        path_and_query,
        body,
        headers,
        trace_id,
        forward_timeout_secs(),
    )
    .await
}

/// [`forward_mutation`] with an explicit timeout — the seam that lets the
/// timeout path be exercised in ~1s instead of the 5s production default. The
/// production entry point always derives the value from the environment; there
/// is no "are we testing" branch anywhere.
#[allow(clippy::too_many_arguments)]
async fn forward_mutation_with_timeout(
    base_url: &str,
    method: &str,
    path_and_query: &str,
    body: Vec<u8>,
    headers: &HeaderMap,
    trace_id: &str,
    secs: u64,
) -> Result<Response<Vec<u8>>, ForwardError> {
    // `secs` bounds the CONNECT phase; the total deadline is longer so a
    // connect-phase failure is never masked by it (see `CONNECT_SLACK_SECS`).
    let total_secs = secs + CONNECT_SLACK_SECS;
    let url = format!("{}{}", base_url.trim_end_matches('/'), path_and_query);
    let mut req = client_for(secs)
        .request(
            reqwest::Method::from_bytes(method.as_bytes())
                .map_err(|e| ForwardError::Other(format!("unsupported method {method}: {e}")))?,
            &url,
        )
        .header("x-hydra-trace-id", trace_id)
        .header(FORWARD_ONCE_HEADER, "1")
        .timeout(Duration::from_secs(total_secs));
    if let Some(auth) = headers.get("authorization") {
        req = req.header("authorization", auth);
    }
    if let Some(ct) = headers.get("content-type") {
        req = req.header("content-type", ct);
    }
    if !body.is_empty() {
        req = req.body(body);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| ForwardError::from_transport(&e, total_secs))?;
    let status = resp.status();
    let content_type = resp.headers().get("content-type").cloned();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| ForwardError::AfterResponse(format!("response read failed: {e}")))?;

    let mut out = Response::builder().status(status);
    if let Some(ct) = content_type {
        out = out.header("content-type", ct);
    }
    out.body(bytes.to_vec())
        .map_err(|e| ForwardError::AfterResponse(e.to_string()))
}

/// Forward one **tenant config write** to the leader's internal control
/// endpoint (sub-tenant v2, decision A-2).
///
/// This is the data plane's ONLY trust-scoped path to the leader, and it is a
/// SEPARATE function from [`forward_mutation`] on purpose. The admin forwarder
/// relays the operator's `Authorization` verbatim (`forward.rs` fixed
/// `authorization` relay), because the fleet shares `HYDRA_ADMIN_TOKEN`. A
/// tenant config write cannot do that: on the edge the caller presents its
/// TENANT Bearer in `Authorization`, which is not the cluster token and must
/// never reach the internal gate under `Authorization`.
///
/// Header contract (A-2 precond. 5/8):
/// - `Authorization: Bearer <cluster_token>` — authenticates the NODE (who
///   sent it), via the shared control-plane token.
/// - [`TENANT_TOKEN_HEADER`] (`x-hydra-tenant-token`): the TENANT's Bearer
///   (who the write is on behalf of). The leader re-authenticates it and binds
///   it to the write target. It is NEVER relayed from the caller's
///   `Authorization` and NEVER placed in the body.
/// - [`FORWARD_ONCE_HEADER`]: the forward-loop guard (a non-leader must never
///   forward a request that already carried it).
/// - `x-hydra-trace-id`: audit attribution (relayed, as the admin forwarder
///   does).
/// - `content-type: application/json`: the config write body is always JSON.
///
/// The caller's `Authorization` is deliberately NOT forwarded. The tenant
/// Bearer and the body are never logged on this path.
///
/// Reuses the exact timeout / connect-classification of [`forward_mutation`]
/// (see [`ForwardError`]): a connect failure is a definite failure, a timeout
/// after connect is genuinely ambiguous (the write may have landed).
#[allow(clippy::too_many_arguments)]
pub async fn forward_config_write(
    base_url: &str,
    method: &str,
    path_and_query: &str,
    cluster_token: &str,
    tenant_bearer: &str,
    body: Vec<u8>,
    trace_id: &str,
) -> Result<Response<Vec<u8>>, ForwardError> {
    forward_config_write_with_timeout(
        base_url,
        method,
        path_and_query,
        cluster_token,
        tenant_bearer,
        body,
        trace_id,
        forward_timeout_secs(),
    )
    .await
}

/// [`forward_config_write`] with an explicit timeout — the seam that lets the
/// timeout / error-classification path be exercised in ~1s instead of the 5s
/// production default. The production entry point always derives the value
/// from the environment; there is no "are we testing" branch anywhere.
#[allow(clippy::too_many_arguments)]
async fn forward_config_write_with_timeout(
    base_url: &str,
    method: &str,
    path_and_query: &str,
    cluster_token: &str,
    tenant_bearer: &str,
    body: Vec<u8>,
    trace_id: &str,
    secs: u64,
) -> Result<Response<Vec<u8>>, ForwardError> {
    // `secs` bounds the CONNECT phase; the total deadline is longer so a
    // connect-phase failure is never masked by it (see `CONNECT_SLACK_SECS`).
    let total_secs = secs + CONNECT_SLACK_SECS;
    let url = format!("{}{}", base_url.trim_end_matches('/'), path_and_query);
    let mut req = client_for(secs)
        .request(
            reqwest::Method::from_bytes(method.as_bytes())
                .map_err(|e| ForwardError::Other(format!("unsupported method {method}: {e}")))?,
            &url,
        )
        // The NODE is authenticated by the shared cluster token. This is the
        // ONLY credential in `Authorization` — the caller's `Authorization`
        // (the tenant Bearer) is deliberately NOT relayed here.
        .header("authorization", format!("Bearer {cluster_token}"))
        // The TENANT's Bearer travels in a dedicated header: never in
        // `Authorization`, never in the body, never logged (A-2 precond. 5).
        .header(TENANT_TOKEN_HEADER, tenant_bearer)
        .header("x-hydra-trace-id", trace_id)
        .header(FORWARD_ONCE_HEADER, "1")
        .header("content-type", "application/json")
        .timeout(Duration::from_secs(total_secs));
    if !body.is_empty() {
        req = req.body(body);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| ForwardError::from_transport(&e, total_secs))?;
    let status = resp.status();
    let content_type = resp.headers().get("content-type").cloned();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| ForwardError::AfterResponse(format!("response read failed: {e}")))?;

    let mut out = Response::builder().status(status);
    if let Some(ct) = content_type {
        out = out.header("content-type", ct);
    }
    out.body(bytes.to_vec())
        .map_err(|e| ForwardError::AfterResponse(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::HeaderValue;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn forward_timeout_parsing_is_total() {
        assert_eq!(parse_forward_timeout_secs(None), 5, "default");
        assert_eq!(parse_forward_timeout_secs(Some("12")), 12);
        assert_eq!(parse_forward_timeout_secs(Some(" 12 ")), 12, "trimmed");
        assert_eq!(parse_forward_timeout_secs(Some("0")), 5, "0 ⇒ default");
        assert_eq!(
            parse_forward_timeout_secs(Some("nope")),
            5,
            "garbage ⇒ default"
        );
        assert_eq!(
            parse_forward_timeout_secs(Some("-3")),
            5,
            "negative ⇒ default"
        );
    }

    /// A leader that ACCEPTS the connection and then never answers: the request
    /// WAS written, so the outcome is genuinely unknown — `Timeout`, not a
    /// definite failure.
    #[tokio::test]
    async fn a_silent_leader_times_out_as_result_unknown() {
        // A black hole: the socket is accepted and never written to.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind black hole");
        let addr = listener.local_addr().expect("addr");
        let held: Vec<std::net::TcpStream> = Vec::new();
        let keep = std::sync::Arc::new(std::sync::Mutex::new(held));
        let keep2 = keep.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(s) = stream else { break };
                // Hold the connection open and stay silent.
                keep2.lock().expect("lock").push(s);
            }
        });

        let err = forward_mutation_with_timeout(
            &format!("http://{addr}"),
            "POST",
            "/api/v1/providers",
            Vec::new(),
            &HeaderMap::new(),
            "t",
            1,
        )
        .await
        .expect_err("no response ⇒ error");
        match err {
            // The reported value is the TOTAL deadline that elapsed: the
            // configured 1s connect bound + CONNECT_SLACK_SECS (2).
            ForwardError::Timeout { secs } => {
                assert_eq!(secs, 1 + CONNECT_SLACK_SECS, "the elapsed total is quoted")
            }
            other => panic!("a silent leader must be `Timeout`, got {other:?}"),
        }
        assert!(
            err.to_string().contains("may already have landed"),
            "the message must not claim certainty about a request that was sent: {err}"
        );
    }

    /// A leader whose port is CLOSED: the request never left this node, so this
    /// must NOT be reported as "the outcome is unknown" — otherwise the most
    /// certain failure would look like the least certain one.
    #[tokio::test]
    async fn a_refused_connection_is_a_definite_failure() {
        // Bind then drop to get a port nobody is listening on.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr").port()
        };
        let err = forward_mutation_with_timeout(
            &format!("http://127.0.0.1:{port}"),
            "POST",
            "/api/v1/providers",
            Vec::new(),
            &HeaderMap::new(),
            "t",
            1,
        )
        .await
        .expect_err("refused ⇒ error");
        assert!(
            matches!(err, ForwardError::Other(_)),
            "connection refusal proves nothing was written, got {err:?}"
        );
        assert!(
            !err.to_string().contains("may already have landed"),
            "a refused connection must NOT be reported as an unknown outcome: {err}"
        );
    }

    /// The plan MANDATED this case ("先实测确认 `is_connect()` 在连接阶段超时下为
    /// 真；若实测为假，则不得保留'结果未知'这一确定措辞"), and its absence is why a
    /// wrong classification shipped: both existing tests covered cases that
    /// classify correctly, so the SYN-dropped case was invisible.
    ///
    /// A black-holed address (SYN dropped, no RST) is the "dead pod whose IP is
    /// still routed" case. It must be a DEFINITE failure: the request never left
    /// this node, so answering "the outcome is unknown" would be a lie.
    #[tokio::test]
    async fn a_syn_dropping_leader_is_a_definite_failure_not_an_unknown_outcome() {
        // RFC 5737 TEST-NET-1: unroutable, so the SYN is dropped rather than
        // answered. (In an environment that rejects it outright the error is
        // still a connect error, so the assertion holds either way.)
        let err = forward_mutation_with_timeout(
            "http://192.0.2.1:81",
            "POST",
            "/api/v1/providers",
            Vec::new(),
            &HeaderMap::new(),
            "t",
            2,
        )
        .await
        .expect_err("a black-holed leader must fail");
        assert!(
            matches!(err, ForwardError::Other(_)),
            "a request that never left this node must NOT be reported as an \
             unknown outcome, got {err:?}"
        );
        assert!(
            !err.to_string().contains("may already have landed"),
            "and the message must not suggest the write may have landed: {err}"
        );
    }

    #[tokio::test]
    async fn forwarded_mutation_carries_forward_once_marker() {
        // The forwarded request MUST carry the loop-guard marker so a
        // receiving node that is not the active leader fails closed instead
        // of forwarding it again (self-/mutual-forward loop termination).
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/providers"))
            .and(header("x-hydra-forwarded", "1"))
            .and(header("authorization", "Bearer admin-secret"))
            .and(header("content-type", "application/json"))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;

        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer admin-secret"),
        );
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        let resp = forward_mutation(
            &server.uri(),
            "POST",
            "/api/v1/providers",
            br#"{"id":"sl01","key":"silicon-flow"}"#.to_vec(),
            &headers,
            "test-trace",
        )
        .await
        .expect("forward succeeds");
        assert_eq!(resp.status(), 201, "active's response is relayed verbatim");
    }

    /// The tenant config write forward must authenticate the NODE with the
    /// cluster token in `Authorization`, carry the TENANT bearer in the
    /// dedicated `x-hydra-tenant-token` header, and send the forward-loop
    /// marker, the trace id, a JSON content type and the write body — then
    /// relay the leader's response verbatim (A-2 precond. 5/8).
    #[tokio::test]
    async fn forward_config_write_sends_cluster_token_and_tenant_bearer_headers() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/internal/tenant-config/sub-tenants"))
            .and(header("authorization", "Bearer secret-cluster-token"))
            .and(header(TENANT_TOKEN_HEADER, "sk-tenant-bearer-123"))
            .and(header("x-hydra-forwarded", "1"))
            .and(header("x-hydra-trace-id", "cfg-trace-42"))
            .and(header("content-type", "application/json"))
            .and(body_json(
                serde_json::json!({"tenant_id":"t1","name":"acme"}),
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"config_version":7})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let resp = forward_config_write(
            &server.uri(),
            "PUT",
            "/api/v1/internal/tenant-config/sub-tenants",
            "secret-cluster-token",
            "sk-tenant-bearer-123",
            br#"{"tenant_id":"t1","name":"acme"}"#.to_vec(),
            "cfg-trace-42",
        )
        .await
        .expect("forward succeeds");
        assert_eq!(resp.status(), 200, "leader's response is relayed verbatim");
        let got: serde_json::Value =
            serde_json::from_slice(&resp.into_body()).expect("response is JSON");
        assert_eq!(got.get("config_version").and_then(|v| v.as_u64()), Some(7));
    }

    /// The tenant bearer must land in the dedicated header, NOT in
    /// `Authorization` (which carries only the cluster token). A regression
    /// that put the tenant bearer in `Authorization` — or relayed the caller's
    /// `Authorization` — would be a live-credential leak into the node-auth slot
    /// (A-2 precond. 5).
    #[tokio::test]
    async fn forward_config_write_keeps_tenant_bearer_out_of_authorization() {
        let server = MockServer::start().await;
        // `Authorization` must be EXACTLY the cluster token, nothing else.
        Mock::given(method("PUT"))
            .and(path("/api/v1/internal/tenant-config/sub-tenant-routes"))
            .and(header("authorization", "Bearer only-the-cluster-token"))
            .and(header(TENANT_TOKEN_HEADER, "sk-tenant-bearer-secret"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        // The caller's own `Authorization` (its tenant bearer) is never a
        // parameter of this function — the only credential in `Authorization`
        // is `cluster_token`. The tenant bearer goes in the dedicated header.
        let _ = forward_config_write(
            &server.uri(),
            "PUT",
            "/api/v1/internal/tenant-config/sub-tenant-routes",
            "only-the-cluster-token",
            "sk-tenant-bearer-secret",
            br#"{}"#.to_vec(),
            "t",
        )
        .await
        .expect("forward succeeds");
    }

    /// A leader whose port is CLOSED: the write never left this node, so this
    /// is a DEFINITE failure (not "the outcome is unknown") — same
    /// classification as the admin forwarder.
    #[tokio::test]
    async fn forward_config_write_refused_connection_is_a_definite_failure() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr").port()
        };
        let err = forward_config_write_with_timeout(
            &format!("http://127.0.0.1:{port}"),
            "PUT",
            "/api/v1/internal/tenant-config/sub-tenants",
            "token",
            "bearer",
            Vec::new(),
            "t",
            1,
        )
        .await
        .expect_err("refused ⇒ error");
        assert!(
            matches!(err, ForwardError::Other(_)),
            "connection refusal proves nothing was written, got {err:?}"
        );
    }
}

#[cfg(all(test, feature = "cluster-redis"))]
mod registry_tests {
    use super::*;
    use crate::cluster::registry::NodeRegistry;
    use crate::cluster::NodeRole;
    use fred::prelude::*;

    /// A REAL Redis on its own database (dev-plan 铁律 2: no in-process mock).
    async fn pool() -> Pool {
        crate::redis::test_redis::isolated_pool().await
    }

    fn registry(pool: &Pool, node_id: &str, url: &str) -> NodeRegistry {
        NodeRegistry::new(
            pool.clone(),
            node_id.to_string(),
            NodeRole::Leader,
            url.to_string(),
        )
    }

    #[tokio::test]
    async fn resolves_to_the_lease_holder() {
        let pool = pool().await;
        let standby = registry(&pool, "control-a", "http://control-a:8081");
        let leader = registry(&pool, "control-b", "http://control-b:8081");
        leader.register(60, 120).await.expect("register leader");
        let _: Option<String> = pool
            .set(crate::redis::LEASE_KEY, "control-b", None, None, false)
            .await
            .expect("set lease");

        let target = forward_target_from_registry(&standby)
            .await
            .expect("registry lookup succeeds");
        assert_eq!(
            target.as_deref(),
            Some("http://control-b:8081"),
            "standby forwards to the ACTUAL lease holder"
        );
    }

    #[tokio::test]
    async fn none_when_we_hold_the_lease() {
        let pool = pool().await;
        let standby = registry(&pool, "control-a", "http://control-a:8081");
        let _: Option<String> = pool
            .set(crate::redis::LEASE_KEY, "control-a", None, None, false)
            .await
            .expect("set lease");

        let target = forward_target_from_registry(&standby)
            .await
            .expect("registry lookup succeeds");
        assert_eq!(target, None, "we are the active writer — nothing to follow");
    }

    #[tokio::test]
    async fn none_when_holder_not_registered() {
        let pool = pool().await;
        let standby = registry(&pool, "control-a", "http://control-a:8081");
        let _: Option<String> = pool
            .set(crate::redis::LEASE_KEY, "ghost", None, None, false)
            .await
            .expect("set lease");

        let target = forward_target_from_registry(&standby)
            .await
            .expect("registry lookup succeeds");
        assert_eq!(target, None, "unregistered holder → fail closed");
    }

    #[tokio::test]
    async fn self_forward_guard_rejects_own_url() {
        // Misconfiguration: the lease holder's registered URL is THIS node's
        // own control URL — forwarding would loop back into ourselves. The
        // guard must reject it (fail closed), never forward.
        let pool = pool().await;
        let standby = registry(&pool, "control-a", "http://shared:8081");
        let misregistered = registry(&pool, "control-b", "http://shared:8081"); // same URL as ours
        misregistered
            .register(60, 120)
            .await
            .expect("register leader");
        let _: Option<String> = pool
            .set(crate::redis::LEASE_KEY, "control-b", None, None, false)
            .await
            .expect("set lease");

        let target = forward_target_from_registry(&standby)
            .await
            .expect("registry lookup succeeds");
        assert_eq!(target, None, "must never forward to our own control URL");
    }
}
