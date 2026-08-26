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

use std::sync::OnceLock;
use std::time::Duration;

use http::{HeaderMap, Response};

/// Forward timeout for admin mutations (generous; admin ops are rare).
const FORWARD_TIMEOUT: Duration = Duration::from_secs(5);

/// Shared reqwest client (rare admin ops; reuse the connection pool).
fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .pool_idle_timeout(Some(Duration::from_secs(90)))
            .build()
            .expect("forward reqwest build (infallible)")
    })
}

/// Forward one admin request to the active leader's admin endpoint,
/// preserving the operator's `Authorization` (the fleet shares
/// `HYDRA_ADMIN_TOKEN`), the content type and the trace id. Returns the
/// active's response (status + content-type + body) or an error message when
/// the active is unreachable (the caller maps it to 502/503).
pub async fn forward_mutation(
    base_url: &str,
    method: &str,
    path_and_query: &str,
    body: Vec<u8>,
    headers: &HeaderMap,
    trace_id: &str,
) -> Result<Response<Vec<u8>>, String> {
    let url = format!("{}{}", base_url.trim_end_matches('/'), path_and_query);
    let mut req = client()
        .request(
            reqwest::Method::from_bytes(method.as_bytes())
                .map_err(|e| format!("unsupported method {method}: {e}"))?,
            &url,
        )
        .header("x-hydra-trace-id", trace_id)
        .timeout(FORWARD_TIMEOUT);
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
        .map_err(|e| format!("forward to active failed: {e}"))?;
    let status = resp.status();
    let content_type = resp.headers().get("content-type").cloned();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("forward response read failed: {e}"))?;

    let mut out = Response::builder().status(status);
    if let Some(ct) = content_type {
        out = out.header("content-type", ct);
    }
    out.body(bytes.to_vec()).map_err(|e| e.to_string())
}
