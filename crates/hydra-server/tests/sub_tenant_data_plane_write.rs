//! D-6 (乙-full) — the data-plane tenant config WRITE path, applied by the node that received it.
//!
//! A tenant's sub-tenant write is authenticated by the tenant token (the same gate a read uses)
//! and then APPLIED HERE, by this node, through the shared transactional write core. There is no
//! forwarding: no leader internal endpoint, no `x-hydra-tenant-token`, no forward error mapping.
//! This test drives the real thing — a real `HydraProxy` data plane, the production gate, the real
//! write core and this node's own SQLite — and asserts what the retired forwarding test was really
//! protecting: the write lands and it lands IN THE RECEIVING NODE'S DATABASE.
//!
//! What this file used to assert (DELETE it if you are looking for that): that an edge forwarded
//! the write to the lease holder through the real Redis registry, and that a silent leader produced
//! a 504. Both contracts are gone with the lease; the cross-node half of the property is covered
//! over real raft nodes by `tests/arachne_three_nodes.rs`.

#![cfg(all(
    feature = "cluster-redis",
    feature = "db",
    feature = "http-client",
    feature = "proxy"
))]

mod common;

use std::sync::Arc;
use std::time::Duration;

use hydra_core::auth::sha256_hex_string;
use hydra_core::breaker::BreakerConfig;
use hydra_core::model::{Provider, ProviderModel, Tenant, TenantModel, TenantProvider};
use hydra_server::crypto::{KeyProvider, StaticKeyProvider};
use hydra_server::db as repo;
use hydra_server::http::{AuthCache, AuthConfig, HttpAuthChecker};
use hydra_server::proxy::admission::AdmissionControl;
use hydra_server::proxy::breaker_wrap::CircuitBreaker;
use hydra_server::proxy::config::ProxyConfig;
use hydra_server::proxy::limiter::RateLimiter;
use hydra_server::proxy::{AppState, HydraProxy};
use hydra_server::sink::UsageSink;
use hydra_server::store::ConfigStore;
use hydra_server::tenant_api::TenantApiConfig;
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use serde_json::json;

/// Tenant `t1`'s bearer (identical on the edge gate and the leader re-auth).
const T1_TOKEN: &str = "sk-tenant-1";
const NOW: &str = "2026-01-01 00:00:00";

// --- a no-op usage sink (these tests never proxy a model request) -------------

struct NoopSink;

impl UsageSink for NoopSink {
    fn record<'a>(
        &'a self,
        _r: hydra_core::model::UsageRecord,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }
}

// --- leader (the internal write handler) --------------------------------------

/// Seed the leader's DB: tenant `t1` (with `T1_TOKEN`) authorised for provider
/// `p1` / `gpt-4o`, so the forwarded write can re-auth and validate.
async fn seed_leader(pool: &sqlx::SqlitePool) {
    let p = Provider {
        id: "p1".into(),
        key: "openai".into(),
        name: "openai".into(),
        endpoint: "https://api.openai.example.com".into(),
        weight: 1,
        created_at: NOW.into(),
        updated_at: NOW.into(),
        max_concurrency: None,
        max_queue_depth: None,
        queue_wait_timeout_ms: None,
    };
    repo::insert_provider(pool, &p).await.expect("insert p1");
    repo::insert_provider_model(
        pool,
        &ProviderModel {
            id: "pm1".into(),
            key: "gpt-4o".into(),
            name: "gpt-4o".into(),
            provider_id: "p1".into(),
            status: 1,
        },
    )
    .await
    .expect("insert gpt-4o");
    repo::insert_tenant(
        pool,
        &Tenant {
            id: "t1".into(),
            name: "t1".into(),
            domain: "acme.example".into(),
            auth_url: "https://auth.acme.example/verify".into(),
            cert_key: None,
            cert_file: None,
            enabled: true,
            created_at: NOW.into(),
            updated_at: NOW.into(),
        },
    )
    .await
    .expect("insert t1");
    repo::set_tenant_access_token_hash(pool, "t1", Some(&sha256_hex_string(T1_TOKEN.as_bytes())))
        .await
        .expect("set t1 token hash");
    repo::insert_tenant_provider(
        pool,
        &TenantProvider {
            id: "tp1".into(),
            tenant_id: "t1".into(),
            provider_id: "p1".into(),
        },
    )
    .await
    .expect("insert t1->p1");
    repo::insert_tenant_model(
        pool,
        &TenantModel {
            id: "tm1".into(),
            tenant_id: "t1".into(),
            model_key: "gpt-4o".into(),
        },
    )
    .await
    .expect("insert t1 gpt-4o");
}

// --- data plane (the forwarder) ------------------------------------------------

/// A data-plane `AppState` for a HOMOGENEOUS node: a REAL local database (not a snapshot-fed
/// replica — the node applies its own writes now), seeded with tenant `t1`, its access-token hash
/// and its provider, plus the tenant-API machinery.
///
/// This replaced `edge_state`, which built a pool-less, snapshot-fed replica plus a forwarder. Both
/// of those shapes are gone with D-6 乙-full: a node whose write is applied locally needs the
/// database it is applied to.
async fn local_node_state(token: &str) -> Arc<AppState> {
    let pool = common::setup_pool().await;
    seed_leader(&pool).await;
    hydra_server::db::set_tenant_access_token_hash(
        &pool,
        "t1",
        Some(&sha256_hex_string(token.as_bytes())),
    )
    .await
    .expect("store the tenant's token hash");
    let kp: Arc<dyn KeyProvider> = Arc::new(StaticKeyProvider::new([7u8; 32], 1));
    let store = ConfigStore::load(pool, kp.clone())
        .await
        .expect("ConfigStore::load");
    store
        .reload_all()
        .await
        .expect("reload so the snapshot carries the token hash");

    let auth = Arc::new(
        HttpAuthChecker::new(
            AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
            AuthConfig::default(),
        )
        .expect("HttpAuthChecker"),
    );
    Arc::new(AppState {
        store,
        auth,
        breaker: Arc::new(CircuitBreaker::new(BreakerConfig::new(5))),
        limiter: Arc::new(RateLimiter::new()),
        admission: AdmissionControl::new(),
        sink: Arc::new(NoopSink),
        proxy: ProxyConfig::default(),
        tenant_api: TenantApiConfig::default(),
        tenant_api_throttle: Arc::new(hydra_server::tenant_api::throttle::Throttle::new()),
        tenant_api_limiter: Arc::new(hydra_server::tenant_api::limit::TenantApiLimiter::new()),
        invalidation: None,
        usage: None,
    })
}

/// Start a real `HydraProxy` data plane on an ephemeral port.
fn start_data_plane(state: Arc<AppState>) -> String {
    let port = common::ephemeral_port();
    let listen = format!("127.0.0.1:{port}");
    let app = HydraProxy::new(state);
    let mut server = Server::new(Some(Opt::default())).expect("Server::new");
    server.bootstrap();
    let mut svc = pingora_proxy::http_proxy_service(&server.configuration, app);
    svc.add_tcp(&listen);
    server.add_service(svc);
    std::thread::spawn(move || server.run_forever());
    format!("http://127.0.0.1:{port}")
}

async fn body_json(r: reqwest::Response) -> (u16, serde_json::Value) {
    let status = r.status().as_u16();
    let text = r.text().await.unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    (status, v)
}

async fn send_write(
    root: &str,
    method: reqwest::Method,
    path: &str,
    bearer: Option<&str>,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    let c = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .expect("client");
    let url = format!("{root}{path}");
    for _ in 0..60 {
        let mut req = c.request(method.clone(), &url);
        if let Some(b) = bearer {
            req = req.bearer_auth(b);
        }
        if let Some(b) = &body {
            req = req.json(b);
        }
        match req.send().await {
            Ok(r) => return body_json(r).await,
            Err(_) => tokio::time::sleep(Duration::from_millis(150)).await,
        }
    }
    panic!("data plane never became ready at {url}");
}

// --- tests ---------------------------------------------------------------------

/// D-6 (乙-full): THE NODE THAT RECEIVED THE WRITE APPLIES IT.
///
/// This replaces two tests that asserted the opposite — `edge_write_forwards_to_lease_holding_leader_and_lands`
/// and `edge_write_forward_timeout_is_504`. Forwarding is retired (ADR-0001 D-6, plan T3.5): there
/// is no leader internal endpoint, no `x-hydra-tenant-token`, and no 504 forward classification,
/// because the entry node runs the shared write core against its OWN database and publishes the
/// resulting config. What still has to hold is what those tests were really protecting: the write
/// lands, it is idempotent, and `config_version` advances so the tenant can see it become live.
///
/// The cross-node half of the old contract (a write accepted anywhere, and every node converging on
/// it) is covered over real raft nodes by
/// `tests/arachne_three_nodes.rs::a_management_write_on_any_node_is_accepted_and_the_cluster_converges`;
/// this file stays the single-node, HTTP-level check of the write path itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_data_plane_write_is_applied_by_the_node_that_received_it() {
    let state = local_node_state(T1_TOKEN).await;
    let root = start_data_plane(state.clone());
    let version_before = state.store.version();

    let (status, v) = send_write(
        &root,
        reqwest::Method::PUT,
        "/tenant/t1/api/v1/sub-tenants/team-a",
        Some(T1_TOKEN),
        Some(json!({ "key_prefix": "QQCX_", "enabled": true })),
    )
    .await;
    assert_eq!(status, 200, "the entry node must apply its own write: {v}");
    let st_id = v["sub_tenant"]["id"].as_str().expect("id").to_string();
    let version_after = v["config_version"].as_u64().expect("version");
    assert!(
        version_after > version_before,
        "the local write must advance config_version: before={version_before} after={version_after}"
    );

    // On THIS node — and in its own database, not merely in memory: a node that
    // served a row it had not committed could not rebuild itself after a restart.
    assert!(
        state
            .store
            .snapshot()
            .sub_tenants
            .iter()
            .any(|s| s.id == st_id),
        "the sub-tenant must be in this node's snapshot"
    );
    let pool = state.store.pool();
    let rows = hydra_server::db::list_sub_tenants(pool)
        .await
        .expect("list sub-tenants");
    assert!(
        rows.iter().any(|s| s.id == st_id),
        "and in its own database"
    );

    // Idempotent: the natural-key upsert converges on the same row.
    let (status2, v2) = send_write(
        &root,
        reqwest::Method::PUT,
        "/tenant/t1/api/v1/sub-tenants/team-a",
        Some(T1_TOKEN),
        Some(json!({ "key_prefix": "QQCX_", "enabled": true })),
    )
    .await;
    assert_eq!(status2, 200, "got {v2}");
    assert_eq!(
        v2["sub_tenant"]["id"].as_str().expect("id"),
        st_id,
        "a repeated PUT converges to the same row"
    );

    // DELETE is idempotent too, and it really removes the row.
    let (status3, _) = send_write(
        &root,
        reqwest::Method::DELETE,
        &format!("/tenant/t1/api/v1/sub-tenants/{st_id}"),
        Some(T1_TOKEN),
        None,
    )
    .await;
    assert!(
        status3 == 204 || status3 == 200,
        "DELETE must succeed, got {status3}"
    );
    let rows = hydra_server::db::list_sub_tenants(pool)
        .await
        .expect("list sub-tenants");
    assert!(
        !rows.iter().any(|s| s.id == st_id),
        "the deleted sub-tenant must be gone from the database"
    );
}
