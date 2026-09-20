//! Sub-tenant v2, V5 — the data-plane tenant config WRITE forward path (D1/D2,
//! A-2) and its single-node local path (D8).
//!
//! The edge/standby data plane authenticates a tenant's sub-tenant write with the
//! tenant token (the same gate a read uses) and forwards it to the LEASE-HOLDING
//! leader's internal endpoint (`/api/v1/internal/tenant-config/...`). This test
//! drives the whole path end to end with REAL components (dev-plan 铁律 2):
//!
//! - a REAL Redis (via `common::real_redis_pool`) backs the node registry +
//!   leader lease, and the forward target is resolved through the production
//!   `forward_target_from_registry` — the exact seam `main` wires;
//! - a REAL `AdminService` (the leader) executes the write against a REAL SQLite;
//! - a REAL `HydraProxy` data plane forwards the request.
//!
//! Nothing internal is mocked: the gate, the forwarder's header contract
//! (cluster token in `Authorization`, tenant bearer in `x-hydra-tenant-token`),
//! the receiver-side re-auth + binding, the transactional write core and the
//! `config_version` advance are all production code.

#![cfg(all(
    feature = "cluster-redis",
    feature = "db",
    feature = "http-client",
    feature = "proxy"
))]

mod common;

use std::sync::Arc;
use std::time::Duration;

use fred::interfaces::KeysInterface;
use hydra_core::auth::sha256_hex_string;
use hydra_core::breaker::BreakerConfig;
use hydra_core::config::ConfigData;
use hydra_core::model::{Provider, ProviderModel, Tenant, TenantModel, TenantProvider};
use hydra_server::admin::{AdminService, AdminState};
use hydra_server::cluster::content::FidelityRows;
use hydra_server::cluster::forward::forward_target_from_registry;
use hydra_server::cluster::registry::NodeRegistry;
use hydra_server::cluster::snapshot::HydratedWire;
use hydra_server::cluster::NodeRole;
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
use hydra_server::tenant_config::TenantConfigForwarder;
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::services::listening::Service;
use serde_json::json;

const CLUSTER_TOKEN: &str = "cluster-tok";
const ADMIN_TOKEN: &str = "admin-tok";
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

/// A leader `AdminState` that HOLDS the lease (so it executes the write locally,
/// per the A-2 receiver-side lease assertion).
async fn leader_state() -> Arc<AdminState> {
    let pool = common::setup_pool().await;
    seed_leader(&pool).await;
    let kp: Arc<dyn KeyProvider> = Arc::new(StaticKeyProvider::new([1u8; 32], 1));
    let store = ConfigStore::load(pool.clone(), kp.clone())
        .await
        .expect("ConfigStore::load");
    store.reload_all().await.expect("reload");
    let auth = Arc::new(
        HttpAuthChecker::new(
            AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
            AuthConfig::default(),
        )
        .expect("HttpAuthChecker"),
    );
    let breaker = Arc::new(CircuitBreaker::new(BreakerConfig::new(2)));
    Arc::new(AdminState::new(
        Some(pool),
        store,
        auth,
        breaker,
        kp,
        Some(ADMIN_TOKEN.to_string()),
        AdmissionControl::new(),
        false, // edge_mode: a leader is a candidate
        Some(CLUSTER_TOKEN.to_string()),
        Some(Arc::new(|| true) as Arc<dyn Fn() -> bool + Send + Sync>), // holds the lease
    ))
}

/// Start a real Pingora `Service` hosting `AdminService` on an ephemeral port.
fn start_admin(state: Arc<AdminState>) -> u16 {
    let port = common::ephemeral_port();
    let addr = format!("127.0.0.1:{port}");
    let app = AdminService::new(state);
    let mut server = Server::new(Some(Opt::default())).expect("Server::new");
    server.bootstrap();
    let mut svc = Service::new("sub-tenant-data-plane-leader".to_string(), app);
    svc.add_tcp(&addr);
    server.add_service(svc);
    std::thread::spawn(move || server.run_forever());
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if std::net::TcpStream::connect(&addr).is_ok() {
            return port;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "leader admin service did not start on {addr} within 5s"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

// --- data plane (the forwarder) ------------------------------------------------

/// An edge/standby data-plane `AppState`: a snapshot-fed store (no local DB), a
/// real forwarder (the cluster token + the resolved leader URL), and the tenant
/// API machinery. The forwarder's leader-URL closure returns the value the
/// production refresh task would have stored after `forward_target_from_registry`.
fn edge_state(leader_url: Option<String>, token: &str) -> Arc<AppState> {
    let kp: Arc<dyn KeyProvider> = Arc::new(StaticKeyProvider::new([7u8; 32], 1));
    // The replica shape: no pool, one snapshot carrying the tenant + its token
    // hash (the gate's only input).
    let store = ConfigStore::from_snapshot(ConfigData::default(), kp);
    let mut cfg = ConfigData::default();
    let t = Tenant {
        id: "t1".into(),
        name: "t1".into(),
        domain: "acme.example".into(),
        auth_url: "https://auth.acme.example/verify".into(),
        cert_key: None,
        cert_file: None,
        enabled: true,
        created_at: NOW.into(),
        updated_at: NOW.into(),
    };
    cfg.tenants_by_domain.insert(t.domain.clone(), t);
    cfg.reindex_tenants();
    store.apply_snapshot(HydratedWire {
        version: 3,
        cfg,
        fidelity: FidelityRows {
            limit_roles: vec![],
            key_prefix_bindings: vec![],
            provider_keys: vec![],
            tenant_token_hashes: vec![("t1".to_string(), sha256_hex_string(token.as_bytes()))],
            provider_models: vec![],
            tenant_providers: vec![],
            tenant_models: vec![],
            sub_tenants: vec![],
            sub_tenant_routes: vec![],
        },
    });

    let auth = Arc::new(
        HttpAuthChecker::new(
            AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
            AuthConfig::default(),
        )
        .expect("HttpAuthChecker"),
    );
    let forwarder = TenantConfigForwarder::new(
        CLUSTER_TOKEN.to_string(),
        Arc::new(move || leader_url.clone()),
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
        tenant_config_forwarder: Some(Arc::new(forwarder)),
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

/// D1/D2/A-2: a standby/edge data-plane write is forwarded (via the REAL Redis
/// lease resolution) to the lease-holding leader's internal handler, lands in
/// the leader's DB, and `config_version` advances — and the edge relays the
/// leader's verdict.
#[tokio::test]
async fn edge_write_forwards_to_lease_holding_leader_and_lands() {
    let pool = common::real_redis_pool(47).await;

    // The leader: a real AdminService holding the lease, seeded for `t1`.
    let leader = leader_state().await;
    let leader_port = start_admin(leader.clone());
    let leader_version_before = leader.store.version();

    // Register the leader in the REAL registry and hand it the lease.
    let leader_registry = NodeRegistry::new(
        pool.clone(),
        "leader-a".into(),
        NodeRole::Leader,
        format!("http://127.0.0.1:{leader_port}"),
    );
    leader_registry
        .register(60, 120)
        .await
        .expect("register leader");
    let _: Option<String> = pool
        .set(
            hydra_server::redis::LEASE_KEY,
            "leader-a",
            None,
            None,
            false,
        )
        .await
        .expect("set lease");

    // The edge's registry resolves the forward target the SAME way `main` does
    // (the production seam). It must be a DIFFERENT node than the lease holder:
    // the self-forward guard (`holder == self.node_id`) makes a node that holds
    // the lease resolve to `None` (it IS the writer), which is exactly why the
    // edge resolves through its own registry.
    let edge_registry = NodeRegistry::new(
        pool,
        "edge-b".into(),
        NodeRole::Edge,
        "http://edge-b:8080".to_string(),
    );
    let resolved = forward_target_from_registry(&edge_registry)
        .await
        .expect("resolve forward target");
    assert_eq!(
        resolved.as_deref(),
        Some(format!("http://127.0.0.1:{leader_port}").as_str()),
        "the forward target must be the ACTUAL lease holder"
    );

    // The edge data plane forwards to that URL.
    let edge = edge_state(resolved, T1_TOKEN);
    let edge_root = start_data_plane(edge);

    // A sub-tenant write on the edge (tenant token in `Authorization`).
    let (status, v) = send_write(
        &edge_root,
        reqwest::Method::PUT,
        "/tenant/t1/api/v1/sub-tenants/team-a",
        Some(T1_TOKEN),
        Some(json!({ "key_prefix": "QQCX_", "enabled": true })),
    )
    .await;
    assert_eq!(status, 200, "the edge must relay the leader's 200: {v}");
    let st_id = v["sub_tenant"]["id"].as_str().expect("id").to_string();
    let version_after = v["config_version"].as_u64().expect("version");
    assert!(
        version_after > leader_version_before,
        "the forwarded write must advance the leader's config_version: \
         before={leader_version_before} after={version_after}"
    );

    // The write LANDED in the leader's DB (the single writer), not on the edge.
    let snap = leader.store.snapshot();
    assert!(
        snap.sub_tenants.iter().any(|s| s.id == st_id),
        "the sub-tenant must be present in the leader's snapshot"
    );
    drop(snap);

    // Idempotent: a repeated PUT converges to the same row (natural-key upsert).
    let (status2, v2) = send_write(
        &edge_root,
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
        "a repeated forwarded PUT converges to the same row"
    );
}

/// The forward error classification, at the data-plane level: a leader that
/// accepts the connection and never answers is an AMBIGUOUS timeout → `504
/// forward_result_unknown` (the write may or may not have landed — re-read).
#[tokio::test]
async fn edge_write_forward_timeout_is_504() {
    // A black hole: the socket is accepted and held open, never written to.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind black hole");
    let addr = listener.local_addr().expect("addr");
    let held: Vec<std::net::TcpStream> = Vec::new();
    let keep = Arc::new(std::sync::Mutex::new(held));
    let keep2 = keep.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(s) = stream else { break };
            keep2.lock().expect("lock").push(s);
        }
    });

    // Tighten the forward deadline (connect 1s + slack 2s = 3s total) so the
    // test does not wait the 7s production default. This env var is read only by
    // the forward path, and only this test binary runs here, so it is scoped.
    std::env::set_var("HYDRA_FORWARD_TIMEOUT_SECS", "1");

    let edge = edge_state(Some(format!("http://{addr}")), T1_TOKEN);
    let edge_root = start_data_plane(edge);

    let (status, v) = send_write(
        &edge_root,
        reqwest::Method::PUT,
        "/tenant/t1/api/v1/sub-tenants/team-a",
        Some(T1_TOKEN),
        Some(json!({ "key_prefix": "QQCX_" })),
    )
    .await;
    assert_eq!(status, 504, "a silent leader must be a 504, got {v}");
    assert_eq!(
        v["error"]["code"], "forward_result_unknown",
        "the ambiguous outcome must be named: {v}"
    );
    let msg = v["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("re-read"),
        "a 504 must tell the caller to re-read before retrying: {msg}"
    );
}
