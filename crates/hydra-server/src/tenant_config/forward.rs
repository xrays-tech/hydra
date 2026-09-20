//! # Trust-scoped tenant config write forwarder (sub-tenant v2, decision A-2)
//!
//! The data plane receives a tenant's config write (sub-tenant CRUD) on its own
//! listener, authenticates it with the tenant access token, and — when this
//! node is not the active writer — forwards it to the leader's internal control
//! plane. This type is the data plane's ONLY capability for doing that (D1/D2,
//! A-2 precond. 8):
//!
//! - it holds a shared **cluster token** (node identity) and a single **leader-
//!   URL closure**;
//! - it never holds the `NodeRegistry` — the closure is the only leader-
//!   resolution seam, so the data plane gains exactly "ask where the writer
//!   is" and nothing more.
//!
//! The forward itself is [`crate::cluster::forward::forward_config_write`]: it
//! authenticates the node with the cluster token in `Authorization` and carries
//! the tenant Bearer in the dedicated `x-hydra-tenant-token` header (never in
//! `Authorization`, never in the body, never logged).

use std::sync::Arc;

use http::Response;

use crate::cluster::forward::{forward_config_write, ForwardError};

/// Why a tenant config write forward failed.
#[derive(Debug, thiserror::Error)]
pub enum TenantConfigForwardError {
    /// No leader is resolvable right now: this node IS the writer, there is no
    /// lease holder, or the holder is unresolvable (fail-closed).
    ///
    /// This is a DEFINITE "not the writer" signal, not a transport failure: the
    /// caller must not apply the write locally (a non-writer never writes) and
    /// must not treat it as an unknown outcome.
    #[error("no leader is resolvable; the tenant config write cannot be forwarded (fail-closed)")]
    NoLeader,
    /// The forward to the leader failed (connect / timeout / response read).
    /// See [`ForwardError`] for the definite-vs-unknown classification.
    #[error(transparent)]
    Forward(#[from] ForwardError),
}

/// The data plane's ONLY trust-scoped capability for reaching the leader to
/// apply a tenant config write (sub-tenant v2, decision A-2, precond. 8; D1/D2).
///
/// Deliberately minimal (see the module docs): a shared cluster token plus a
/// single leader-URL closure. The `NodeRegistry` is NOT a field here and is NOT
/// exposed — the closure is the only leader-resolution capability this type
/// carries.
#[derive(Clone)]
pub struct TenantConfigForwarder {
    /// The shared control-plane token (`HYDRA_CLUSTER_TOKEN`): authenticates
    /// THIS node to the leader's internal gate (node identity).
    cluster_token: String,
    /// The single leader-resolution seam: `None` when there is no forward
    /// target (this node is the writer / no lease holder / holder unresolvable).
    leader_url: Arc<dyn Fn() -> Option<String> + Send + Sync>,
}

impl TenantConfigForwarder {
    /// Build a forwarder.
    ///
    /// `leader_url` is the single leader-resolution seam. It returns `None`
    /// when there is no forward target (this node is the writer, there is no
    /// lease holder, or the holder is unresolvable) — the caller then fails
    /// closed (never a local write, never a panic).
    #[must_use]
    pub fn new(
        cluster_token: String,
        leader_url: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    ) -> Self {
        Self {
            cluster_token,
            leader_url,
        }
    }

    /// The active leader's control URL, or `None` when there is no forward
    /// target. Exposed so the caller can distinguish "we are the writer / no
    /// leader" from a transport failure before paying for a forward attempt.
    #[must_use]
    pub fn leader_url(&self) -> Option<String> {
        (self.leader_url)()
    }

    /// Forward one tenant config write to the leader's internal endpoint.
    ///
    /// Resolves the leader URL via the injected closure FIRST: `None` ⇒ a
    /// definite [`TenantConfigForwardError::NoLeader`] (fail-closed — never a
    /// local write, never a panic). Otherwise calls
    /// [`forward_config_write`], which authenticates the node with the cluster
    /// token and carries the tenant Bearer in the dedicated header (never in
    /// `Authorization`, never in the body, never logged).
    pub async fn forward_config_write(
        &self,
        method: &str,
        path_and_query: &str,
        tenant_bearer: &str,
        body: Vec<u8>,
        trace_id: &str,
    ) -> Result<Response<Vec<u8>>, TenantConfigForwardError> {
        let url = self
            .leader_url()
            .ok_or(TenantConfigForwardError::NoLeader)?;
        forward_config_write(
            &url,
            method,
            path_and_query,
            &self.cluster_token,
            tenant_bearer,
            body,
            trace_id,
        )
        .await
        .map_err(TenantConfigForwardError::Forward)
    }
}

/// Build a forwarder whose leader-URL closure returns a fixed value — the
/// same shape `main` injects (a closure over a refreshed registry view). Shared
/// by both the unit and the cluster-registry test modules.
#[cfg(test)]
fn forwarder(leader_url: Option<String>) -> TenantConfigForwarder {
    TenantConfigForwarder::new(
        "secret-cluster-token".to_string(),
        Arc::new(move || leader_url.clone()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// No leader resolvable (the closure returns `None`) is a DEFINITE
    /// fail-closed error — never a panic, never a local write.
    #[tokio::test]
    async fn no_leader_is_a_definite_fail_closed_error() {
        let f = forwarder(None);
        let err = f
            .forward_config_write(
                "PUT",
                "/api/v1/internal/tenant-config/sub-tenants",
                "sk-tenant-bearer",
                br#"{}"#.to_vec(),
                "t",
            )
            .await
            .expect_err("no leader ⇒ error");
        assert!(
            matches!(err, TenantConfigForwardError::NoLeader),
            "got {err:?}"
        );
    }

    /// The forwarder resolves the leader URL from the closure and the write
    /// reaches a REAL HTTP server (the external boundary) with the node cluster
    /// token in `Authorization`, the tenant bearer in the dedicated header, the
    /// forward-loop marker, the trace id and a JSON content type (A-2
    /// precond. 5/8). No mock of our own functions — wiremock is the server.
    #[tokio::test]
    async fn resolves_leader_url_and_forwards_with_headers() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/internal/tenant-config/sub-tenants"))
            .and(header("authorization", "Bearer secret-cluster-token"))
            .and(header(
                crate::cluster::forward::TENANT_TOKEN_HEADER,
                "sk-tenant-bearer-123",
            ))
            .and(header("x-hydra-forwarded", "1"))
            .and(header("x-hydra-trace-id", "cfg-trace-9"))
            .and(header("content-type", "application/json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"config_version":3})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let f = forwarder(Some(server.uri()));
        let resp = f
            .forward_config_write(
                "PUT",
                "/api/v1/internal/tenant-config/sub-tenants",
                "sk-tenant-bearer-123",
                br#"{"tenant_id":"t1","name":"acme"}"#.to_vec(),
                "cfg-trace-9",
            )
            .await
            .expect("forward reaches the leader");
        assert_eq!(resp.status(), 200, "leader's response is relayed");
    }
}

#[cfg(all(test, feature = "cluster-redis"))]
mod registry_tests {
    use super::*;
    use crate::cluster::registry::NodeRegistry;
    use crate::cluster::NodeRole;
    use fred::prelude::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A REAL Redis on its own database (dev-plan 铁律 2: no in-process mock).
    async fn pool() -> Pool {
        crate::redis::test_redis::isolated_pool().await
    }

    /// The forwarder's leader-URL closure is what `main` injects from the
    /// registry (via `forward_target_from_registry`). Here the REAL registry
    /// resolves the lease holder's control URL, and the forwarder reaches that
    /// (wiremock) URL with the node cluster token + tenant bearer headers.
    #[tokio::test]
    async fn registry_resolves_lease_holder_and_forwarder_reaches_it() {
        let server = MockServer::start().await;
        let pool = pool().await;

        // The leader registers its control URL as the (wiremock) server; the
        // standby (the node doing the forward) has a DIFFERENT control URL so
        // the self-forward guard does not fire.
        let leader = NodeRegistry::new(
            pool.clone(),
            "leader-a".into(),
            NodeRole::Leader,
            server.uri(),
        );
        let standby = NodeRegistry::new(
            pool.clone(),
            "standby-b".into(),
            NodeRole::Leader,
            "http://standby-b:8081".into(),
        );
        leader.register(60, 120).await.expect("register leader");
        let _: Option<String> = pool
            .set(crate::redis::LEASE_KEY, "leader-a", None, None, false)
            .await
            .expect("set lease");

        // The same resolution `main`'s refresh task calls.
        let resolved = crate::cluster::forward::forward_target_from_registry(&standby)
            .await
            .expect("resolve");
        assert_eq!(
            resolved.as_deref(),
            Some(server.uri().as_str()),
            "the forward target is the ACTUAL lease holder"
        );

        Mock::given(method("PUT"))
            .and(path("/api/v1/internal/tenant-config/sub-tenants"))
            .and(header("authorization", "Bearer secret-cluster-token"))
            .and(header(
                crate::cluster::forward::TENANT_TOKEN_HEADER,
                "sk-tenant-bearer-123",
            ))
            .and(header("x-hydra-forwarded", "1"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        // The closure is the registry view; here it is the value the refresh
        // task would have stored after resolving the lease holder.
        let f = forwarder(resolved);
        let resp = f
            .forward_config_write(
                "PUT",
                "/api/v1/internal/tenant-config/sub-tenants",
                "sk-tenant-bearer-123",
                br#"{"tenant_id":"t1","name":"acme"}"#.to_vec(),
                "cfg-trace-9",
            )
            .await
            .expect("forward reaches the lease holder");
        assert_eq!(resp.status(), 200);
    }
}
