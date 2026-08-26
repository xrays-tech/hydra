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

use std::sync::OnceLock;
use std::time::Duration;

use http::{HeaderMap, Response};

/// Forward timeout for admin mutations (generous; admin ops are rare).
const FORWARD_TIMEOUT: Duration = Duration::from_secs(5);

/// Forward-once marker: set on every forwarded admin mutation so the
/// receiving node can tell a mutation that already travelled through a
/// standby. A node that is not the active leader must never forward such a
/// request again — see the module docs (forward loop guard).
pub const FORWARD_ONCE_HEADER: &str = "x-hydra-forwarded";

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
) -> Result<Response<Vec<u8>>, String> {
    let url = format!("{}{}", base_url.trim_end_matches('/'), path_and_query);
    let mut req = client()
        .request(
            reqwest::Method::from_bytes(method.as_bytes())
                .map_err(|e| format!("unsupported method {method}: {e}"))?,
            &url,
        )
        .header("x-hydra-trace-id", trace_id)
        .header(FORWARD_ONCE_HEADER, "1")
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

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::HeaderValue;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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
}

#[cfg(all(test, feature = "cluster-redis"))]
mod registry_tests {
    use super::*;
    use crate::cluster::registry::NodeRegistry;
    use crate::cluster::NodeRole;
    use crate::redis::mock::MockRedis;
    use fred::prelude::*;
    use std::sync::Arc;

    /// Fresh MockRedis-backed pool (in-process command-level test double).
    async fn pool() -> Pool {
        let mock = Arc::new(MockRedis::new());
        let cfg = Config {
            mocks: Some(mock),
            ..Default::default()
        };
        let pool = Pool::new(cfg, None, None, None, 1).expect("pool");
        pool.init().await.expect("init");
        pool
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
        leader.register(60).await.expect("register leader");
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
        misregistered.register(60).await.expect("register leader");
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
