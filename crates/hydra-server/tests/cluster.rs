//! Cluster P1 integration: control-plane snapshot distribution.
//!
//! Exercises the REAL leader control endpoint (`/api/v1/internal/control`,
//! cluster-token gated) with a REAL `ControlClient` polling it and applying
//! hydrated snapshots to an edge `ConfigStore` — no mocks on the seam: the
//! wire format, the sealing/hydrating crypto, the HTTP channel and the
//! last-known-good failure path are all production code.

mod common;

use std::sync::Arc;
use std::time::Duration;

use hydra_core::breaker::BreakerConfig;
use hydra_core::config::ConfigData;
use hydra_core::model::{
    Provider, ProviderKey, ProviderModel, Tenant, TenantModel, TenantProvider,
};
use hydra_server::admin::{AdminService, AdminState};
use hydra_server::cluster::control_client::{ControlClient, ControlClientConfig};
use hydra_server::cluster::lease::{LeaderElection, MemoryLeaseStore};
use hydra_server::cluster::replica;
use hydra_server::cluster::snapshot::SnapshotWire;
use hydra_server::crypto::{KeyProvider, StaticKeyProvider};
use hydra_server::db as repo;
use hydra_server::http::{AuthCache, AuthConfig, HttpAuthChecker};
use hydra_server::proxy::admission::AdmissionControl;
use hydra_server::proxy::breaker_wrap::CircuitBreaker;
use hydra_server::store::{build_config, ConfigStore};
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::services::listening::Service;

const CLUSTER_TOKEN: &str = "cluster-tok";
const ADMIN_TOKEN: &str = "admin-tok";

fn kp() -> StaticKeyProvider {
    StaticKeyProvider::new([1u8; 32], 1)
}

fn now() -> &'static str {
    "2026-01-01 00:00:00"
}

/// Bind an ephemeral port, return it, then release the socket so Pingora can
/// rebind. (Same TOCTOU-tolerant pattern as the W4 spike test.)
fn ephemeral_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    listener.local_addr().expect("local_addr").port()
}

/// Start a real Pingora `Service` hosting `AdminService` on an ephemeral port.
///
/// Blocks until the listener actually accepts connections: `run_forever` binds
/// asynchronously on the spawned thread, and a test that fires a request the
/// moment `start_admin` returns can race the bind (CI flake: ConnectionRefused
/// on the first probe). Polling a TCP connect keeps every caller race-free.
fn start_admin(state: Arc<AdminState>) -> u16 {
    let port = ephemeral_port();
    let addr = format!("127.0.0.1:{port}");
    let app = AdminService::new(state);
    let mut server = Server::new(Some(Opt::default())).expect("Server::new");
    server.bootstrap();
    let mut svc = Service::new("cluster-test".to_string(), app);
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
            "admin service did not start listening on {addr} within 5s"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Leader-side components: a real DB-backed store + a cluster-token admin.
async fn leader() -> (sqlx::SqlitePool, ConfigStore, u16) {
    let pool = common::setup_pool().await;
    let key_provider: Arc<dyn KeyProvider> = Arc::new(kp());
    let store = ConfigStore::load(pool.clone(), key_provider.clone())
        .await
        .expect("ConfigStore::load");
    let auth = Arc::new(
        HttpAuthChecker::new(
            AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
            AuthConfig::default(),
        )
        .expect("HttpAuthChecker"),
    );
    let breaker = Arc::new(CircuitBreaker::new(BreakerConfig::new(2)));
    let state = Arc::new(AdminState::new(
        Some(pool.clone()),
        store.clone(),
        auth,
        breaker,
        key_provider,
        Some(ADMIN_TOKEN.to_string()),
        None,
        AdmissionControl::new(),
        false,
        Some(CLUSTER_TOKEN.to_string()),
        None, // no leader election in the leader() test harness
    ));
    let port = start_admin(state);
    (pool, store, port)
}

/// Seed the leader DB with a tenant + provider + (encrypted) provider key,
/// then reload so the version bumps and the snapshot carries them.
async fn seed_and_reload(pool: &sqlx::SqlitePool, store: &ConfigStore) {
    repo::insert_tenant(
        pool,
        &Tenant {
            id: "t1".into(),
            name: "T".into(),
            domain: "acme.com".into(),
            auth_url: "https://auth.acme.com/v".into(),
            cert_key: None,
            cert_file: None,
            enabled: true,
            created_at: now().into(),
            updated_at: now().into(),
        },
    )
    .await
    .expect("insert tenant");
    repo::insert_provider(
        pool,
        &Provider {
            id: "p1".into(),
            key: "openai".into(),
            name: "O".into(),
            endpoint: "https://api.openai.com".into(),
            weight: 1,
            created_at: now().into(),
            updated_at: now().into(),
            max_concurrency: None,
            max_queue_depth: None,
            queue_wait_timeout_ms: None,
        },
    )
    .await
    .expect("insert provider");
    repo::insert_provider_key(
        pool,
        &kp(),
        &ProviderKey {
            id: "k1".into(),
            provider_id: "p1".into(),
            api_key: "sk-upstream-secret".into(),
            created_at: now().into(),
        },
    )
    .await
    .expect("insert provider key");
    repo::insert_provider_model(
        pool,
        &ProviderModel {
            id: "m1".into(),
            key: "gpt-4".into(),
            name: "GPT-4".into(),
            provider_id: "p1".into(),
            status: 1,
        },
    )
    .await
    .expect("insert online model");
    repo::insert_provider_model(
        pool,
        &ProviderModel {
            id: "m2".into(),
            key: "gpt-4-offline".into(),
            name: "GPT-4 (disabled)".into(),
            provider_id: "p1".into(),
            status: 0,
        },
    )
    .await
    .expect("insert offline model");
    repo::insert_tenant_provider(
        pool,
        &TenantProvider {
            id: "tp1".into(),
            tenant_id: "t1".into(),
            provider_id: "p1".into(),
        },
    )
    .await
    .expect("insert tenant provider");
    repo::insert_tenant_model(
        pool,
        &TenantModel {
            id: "tm1".into(),
            tenant_id: "t1".into(),
            model_key: "gpt-4".into(),
        },
    )
    .await
    .expect("insert tenant model");
    store.reload_all().await.expect("reload");
}

/// T-CL-1 — an edge polls the leader and applies the hydrated snapshot
/// (config + decrypted provider key) with the leader's version.
#[tokio::test]
async fn edge_applies_leader_snapshot() {
    let (pool, leader_store, port) = leader().await;
    seed_and_reload(&pool, &leader_store).await;

    let edge_store = ConfigStore::from_snapshot(ConfigData::default(), Arc::new(kp()));
    let client = ControlClient::new(
        ControlClientConfig {
            url: format!("http://127.0.0.1:{port}"),
            token: CLUSTER_TOKEN.to_string(),
            poll_interval: Duration::from_millis(50),
        },
        edge_store.clone(),
        Arc::new(kp()),
        None,
    );

    let mut applied = false;
    for _ in 0..50 {
        let _ = client.poll_once().await;
        if edge_store
            .snapshot()
            .tenants_by_domain
            .contains_key("acme.com")
        {
            applied = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(applied, "edge must apply the leader's snapshot");

    assert_eq!(
        edge_store.version(),
        leader_store.version(),
        "edge adopts the leader's version"
    );
    assert!(edge_store.snapshot().providers.contains_key("p1"));
    assert_eq!(
        edge_store.snapshot().provider_keys["p1"],
        vec!["sk-upstream-secret".to_string()],
        "the sealed provider key decrypts back to the plaintext on the edge"
    );
}

/// T-CL-2 — the control endpoint is gated by the CLUSTER token, not the admin
/// token; a wrong cluster token is rejected with 401.
#[tokio::test]
async fn control_endpoint_requires_cluster_token() {
    let (pool, leader_store, port) = leader().await;
    seed_and_reload(&pool, &leader_store).await;

    let get = |tok: Option<String>| {
        let url = format!("http://127.0.0.1:{port}/api/v1/internal/control");
        async move {
            let client = reqwest::Client::new();
            // Retry until the admin server is listening (bind races with
            // the test thread).
            for _ in 0..50 {
                let mut req = client.get(url.clone());
                if let Some(t) = &tok {
                    req = req.header("authorization", format!("Bearer {t}"));
                }
                match req.send().await {
                    Ok(r) => return r.status().as_u16(),
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
            panic!("admin server did not come up");
        }
    };

    assert_eq!(
        get(Some(CLUSTER_TOKEN.to_string())).await,
        200,
        "cluster token accepted"
    );
    assert_eq!(
        get(Some(ADMIN_TOKEN.to_string())).await,
        401,
        "admin token is NOT the cluster token"
    );
    assert_eq!(get(None).await, 401, "no token rejected");
    assert_eq!(get(Some("wrong".to_string())).await, 401);
}

/// T-CL-3 — last-known-good on hydrate failure: an edge whose master key
/// cannot decrypt the leader's sealed material keeps its previous snapshot.
#[tokio::test]
async fn edge_keeps_last_known_good_on_decrypt_failure() {
    let (pool, leader_store, port) = leader().await;
    seed_and_reload(&pool, &leader_store).await;

    // Wrong master key: [9u8;32] vs the leader's [1u8;32].
    let wrong_kp: Arc<dyn KeyProvider> = Arc::new(StaticKeyProvider::new([9u8; 32], 1));
    let edge_store = ConfigStore::from_snapshot(ConfigData::default(), wrong_kp.clone());
    let client = ControlClient::new(
        ControlClientConfig {
            url: format!("http://127.0.0.1:{port}"),
            token: CLUSTER_TOKEN.to_string(),
            poll_interval: Duration::from_millis(50),
        },
        edge_store.clone(),
        wrong_kp,
        None,
    );

    assert!(
        client.poll_once().await.is_err(),
        "decrypt failure must surface as a poll error"
    );
    assert!(
        edge_store.snapshot().tenants_by_domain.is_empty(),
        "the edge keeps its last-known-good (empty) snapshot"
    );
    assert_eq!(edge_store.version(), 1, "version unchanged on failure");
}

// ===========================================================================
// P2 — leader election & standby replica
// ===========================================================================

/// T-CL-4 — standby replica materialization: the full-table rebuild from a
/// control snapshot preserves EVERYTHING (incl. offline models and grant
/// rows), so a promoted standby is a faithful copy of the active.
#[tokio::test]
async fn standby_materializes_replica() {
    let (leader_pool, leader_store, _port) = leader().await;
    seed_and_reload(&leader_pool, &leader_store).await;

    let kp: Arc<dyn KeyProvider> = Arc::new(kp());
    let wire = SnapshotWire::build(
        leader_store.version(),
        hydra_core::config::ConfigData::clone(&leader_store.snapshot()),
        &leader_pool,
        kp.as_ref(),
    )
    .await
    .expect("build wire");

    let replica_pool = common::setup_pool().await;
    replica::materialize(&replica_pool, kp.as_ref(), &wire)
        .await
        .expect("materialize");

    let leader_cfg = build_config(&leader_pool, kp.as_ref())
        .await
        .expect("leader cfg");
    let replica_cfg = build_config(&replica_pool, kp.as_ref())
        .await
        .expect("replica cfg");
    assert_eq!(replica_cfg.providers, leader_cfg.providers);
    assert_eq!(replica_cfg.provider_keys, leader_cfg.provider_keys);
    assert_eq!(replica_cfg.tenants_by_domain, leader_cfg.tenants_by_domain);
    assert_eq!(replica_cfg.models_by_key, leader_cfg.models_by_key);
    assert_eq!(replica_cfg.tenant_providers, leader_cfg.tenant_providers);
    assert_eq!(replica_cfg.tenant_models, leader_cfg.tenant_models);

    // Fidelity: the OFFLINE model survives the round-trip (it is absent from
    // the derived `models_by_key` but must not be dropped from the DB).
    let models = repo::list_provider_models(&replica_pool)
        .await
        .expect("models");
    assert!(
        models
            .iter()
            .any(|m| m.key == "gpt-4-offline" && m.status == 0),
        "offline model preserved in the replica"
    );

    // The config version persists in config_meta.
    assert_eq!(
        replica::replica_version(&replica_pool)
            .await
            .expect("version"),
        Some(leader_store.version()),
        "promoted replica continues the version sequence"
    );
}

/// T-CL-5 — two candidates, exactly one leader; the standby takes over after
/// the lease expires; the old leader demotes on its next tick.
#[tokio::test]
async fn election_two_nodes_one_leader() {
    let store: std::sync::Arc<dyn hydra_server::cluster::lease::LeaseStore> =
        std::sync::Arc::new(MemoryLeaseStore::new());
    let e1 = LeaderElection::new(store.clone(), "n1".into(), 600);
    let e2 = LeaderElection::new(store.clone(), "n2".into(), 600);

    e1.tick().await;
    assert!(e1.is_leader(), "n1 acquires first");
    e2.tick().await;
    assert!(!e2.is_leader(), "n2 stays standby while n1 holds the lease");

    // Lease expires (600 ms) → n2 acquires on its next tick.
    tokio::time::sleep(Duration::from_millis(700)).await;
    e2.tick().await;
    assert!(e2.is_leader(), "n2 takes over after expiry");

    // n1's next tick: renew fails (n2 holds) → immediate demotion.
    e1.tick().await;
    assert!(!e1.is_leader(), "n1 demotes after losing the lease");
}

/// T-CL-6 — `/healthz/leader` reflects the lease (200 active / 503 standby /
/// 404 non-candidate), and admin mutations are gated on leadership.
#[tokio::test]
async fn leader_health_and_write_gate() {
    // Standby node: leader_ready = Some(|| false).
    let key_provider: Arc<dyn KeyProvider> = Arc::new(kp());
    let store = ConfigStore::from_snapshot(ConfigData::default(), key_provider.clone());
    let auth = Arc::new(
        HttpAuthChecker::new(
            AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
            AuthConfig::default(),
        )
        .expect("HttpAuthChecker"),
    );
    let breaker = Arc::new(CircuitBreaker::new(BreakerConfig::new(2)));
    let state = Arc::new(AdminState::new(
        Some(common::setup_pool().await),
        store,
        auth,
        breaker,
        key_provider,
        Some(ADMIN_TOKEN.to_string()),
        None,
        AdmissionControl::new(),
        false,
        None,
        Some(Arc::new(|| false) as Arc<dyn Fn() -> bool + Send + Sync>),
    ));
    let port = start_admin(state);

    let client = reqwest::Client::new();
    let leader_url = format!("http://127.0.0.1:{port}/healthz/leader");
    let r = client.get(&leader_url).send().await.expect("send");
    assert_eq!(r.status().as_u16(), 503, "standby reports not-leader");

    // Admin mutation on a standby → 503 (fail-closed; P3 adds forwarding).
    let r = client
        .post(format!("http://127.0.0.1:{port}/api/v1/providers"))
        .header("authorization", format!("Bearer {ADMIN_TOKEN}"))
        .json(&serde_json::json!({
            "id":"p1","key":"openai","name":"O","endpoint":"https://api.openai.com",
            "weight":1,"created_at":"","updated_at":""
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(r.status().as_u16(), 503, "standby rejects writes");

    // Non-candidate (all/single-node): /healthz/leader → 404.
    let state_all = Arc::new(AdminState::new(
        Some(common::setup_pool().await),
        ConfigStore::from_snapshot(ConfigData::default(), Arc::new(kp())),
        Arc::new(
            HttpAuthChecker::new(
                AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
                AuthConfig::default(),
            )
            .expect("HttpAuthChecker"),
        ),
        Arc::new(CircuitBreaker::new(BreakerConfig::new(2))),
        Arc::new(kp()),
        Some(ADMIN_TOKEN.to_string()),
        None,
        AdmissionControl::new(),
        false,
        None,
        None, // single-node: no election
    ));
    let port_all = start_admin(state_all);
    let r = http(port_all, reqwest::Method::GET, "/healthz/leader", None).await;
    assert_eq!(
        r.status().as_u16(),
        404,
        "non-candidate has no leader health"
    );
}

/// A reqwest call with bind-retry (the Pingora server races the test thread).
async fn http(
    port: u16,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> reqwest::Response {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}{path}");
    for _ in 0..50 {
        let mut req = client.request(method.clone(), url.clone());
        req = req.header("authorization", format!("Bearer {ADMIN_TOKEN}"));
        if let Some(b) = &body {
            req = req.json(b);
        }
        match req.send().await {
            Ok(r) => return r,
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    panic!("server on {port} did not come up");
}

#[cfg(feature = "cluster-redis")]
async fn admin_components(
    pool: sqlx::SqlitePool,
    kp: Arc<dyn KeyProvider>,
) -> (ConfigStore, Arc<HttpAuthChecker>, Arc<CircuitBreaker>) {
    let store = ConfigStore::load(pool, kp.clone())
        .await
        .expect("ConfigStore::load");
    let auth = Arc::new(
        HttpAuthChecker::new(
            AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
            AuthConfig::default(),
        )
        .expect("HttpAuthChecker"),
    );
    let breaker = Arc::new(CircuitBreaker::new(BreakerConfig::new(2)));
    (store, auth, breaker)
}

/// T-CL-7 — a standby FORWARDS admin mutations to the ACTUAL lease holder
/// (resolved live from the cluster registry — the target is never a static
/// `HYDRA_CONTROL_URL`, which for a primary candidate points at the node
/// itself), serves reads locally, and never writes locally. A dead active
/// yields 502 (fail-closed, no self-promotion via forwarding); a mutation
/// that already carries the forward-once marker is refused (loop guard).
#[cfg(feature = "cluster-redis")]
#[tokio::test]
async fn standby_forwards_mutations_to_active() {
    use fred::prelude::*;
    use hydra_server::cluster::registry::NodeRegistry;
    use hydra_server::cluster::NodeRole;
    use hydra_server::redis::mock::MockRedis;
    use hydra_server::redis::LEASE_KEY;

    // Shared Redis double: the lease + the registry entries the forward
    // target is resolved from (MockRedis — no external Redis needed).
    let mock = Arc::new(MockRedis::new());
    let cfg = Config {
        mocks: Some(mock),
        ..Default::default()
    };
    let pool = Pool::new(cfg, None, None, None, 1).expect("pool");
    pool.init().await.expect("init");

    let active_pool = common::setup_pool().await;
    let kp_arc: Arc<dyn KeyProvider> = Arc::new(kp());
    let (active_store, auth, breaker) = admin_components(active_pool.clone(), kp_arc.clone()).await;
    let active = Arc::new(AdminState::new(
        Some(active_pool.clone()),
        active_store,
        auth,
        breaker,
        kp_arc.clone(),
        Some(ADMIN_TOKEN.to_string()),
        None,
        AdmissionControl::new(),
        false,
        None,
        Some(Arc::new(|| true) as Arc<dyn Fn() -> bool + Send + Sync>),
    ));
    let active_port = start_admin(active);

    // Register the ACTIVE in the registry and hand it the lease: the standby
    // must resolve THIS node as its forward target — the node id it polls
    // (`HYDRA_CONTROL_URL`) is irrelevant to forwarding.
    let active_registry = NodeRegistry::new(
        pool.clone(),
        "active".into(),
        NodeRole::Leader,
        format!("http://127.0.0.1:{active_port}"),
    );
    active_registry.register(60).await.expect("register active");
    let _: Option<String> = pool
        .set(LEASE_KEY, "active", None, None, false)
        .await
        .expect("set lease");

    let standby_pool = common::setup_pool().await;
    let (standby_store, auth2, breaker2) =
        admin_components(standby_pool.clone(), kp_arc.clone()).await;
    let mut standby_state = AdminState::new(
        Some(standby_pool.clone()),
        standby_store,
        auth2,
        breaker2,
        kp_arc,
        Some(ADMIN_TOKEN.to_string()),
        None,
        AdmissionControl::new(),
        false,
        None,
        Some(Arc::new(|| false) as Arc<dyn Fn() -> bool + Send + Sync>),
    );
    standby_state.cluster_registry = Some(Arc::new(NodeRegistry::new(
        pool.clone(),
        "standby".into(),
        NodeRole::Leader,
        format!("http://127.0.0.1:{}", ephemeral_port()), // own URL; never the target here
    )));
    let standby = Arc::new(standby_state);
    let standby_port = start_admin(standby);

    // POST a provider to the STANDBY → forwarded to the active → 201, and the
    // provider lands on the ACTIVE's DB only.
    let body = serde_json::json!({
        "id":"p1","key":"openai","name":"O","endpoint":"https://api.openai.com",
        "weight":1,"created_at":"","updated_at":""
    });
    let r = http(
        standby_port,
        reqwest::Method::POST,
        "/api/v1/providers",
        Some(body),
    )
    .await;
    assert_eq!(r.status().as_u16(), 201, "mutation forwarded to the active");
    assert!(
        repo::list_providers(&active_pool)
            .await
            .unwrap()
            .iter()
            .any(|p| p.id == "p1"),
        "provider created on the ACTIVE's DB"
    );
    assert!(
        repo::list_providers(&standby_pool)
            .await
            .unwrap()
            .is_empty(),
        "the standby never writes locally"
    );

    // Reads on the standby are served locally (its own (empty) replica).
    let r = http(
        standby_port,
        reqwest::Method::GET,
        "/api/v1/providers",
        None,
    )
    .await;
    assert_eq!(r.status().as_u16(), 200, "read served locally");
    let list: serde_json::Value = r.json().await.expect("json");
    assert_eq!(
        list.as_array().map(|a| a.len()).unwrap_or(0),
        0,
        "standby reads its replica"
    );

    // Auth-cache invalidate is a DELETE → forwarded too.
    let r = http(
        standby_port,
        reqwest::Method::DELETE,
        "/api/v1/auth/cache",
        Some(serde_json::json!({"tenant_id": "nope"})),
    )
    .await;
    assert_eq!(r.status().as_u16(), 200, "DELETE forwarded to the active");

    // Forward-once loop guard: a mutation that already carries the marker is
    // refused with 503 even though we are a standby — it must never be
    // forwarded a second time (self-/mutual-forward loop termination).
    let r = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{standby_port}/api/v1/providers"))
        .header("authorization", format!("Bearer {ADMIN_TOKEN}"))
        .header("x-hydra-forwarded", "1")
        .json(&serde_json::json!({"id":"loop","key":"l","name":"L","endpoint":"https://l.example.com","weight":1,"created_at":"","updated_at":""}))
        .send()
        .await
        .expect("send");
    assert_eq!(
        r.status().as_u16(),
        503,
        "forward-once guard refuses re-forwarding"
    );

    // Dead active → 502 (fail-closed), never a local write.
    let dead_port = ephemeral_port(); // nothing listening there
    let dead_pool = common::setup_pool().await;
    let (dead_store, auth3, breaker3) = admin_components(dead_pool.clone(), Arc::new(kp())).await;
    let mut dead_state = AdminState::new(
        Some(dead_pool.clone()),
        dead_store,
        auth3,
        breaker3,
        Arc::new(kp()),
        Some(ADMIN_TOKEN.to_string()),
        None,
        AdmissionControl::new(),
        false,
        None,
        Some(Arc::new(|| false) as Arc<dyn Fn() -> bool + Send + Sync>),
    );
    dead_state.cluster_registry = Some(Arc::new(NodeRegistry::new(
        pool.clone(),
        "dead-standby".into(),
        NodeRole::Leader,
        String::new(),
    )));
    // A "dead active" registered in the registry with an unreachable URL.
    let dead_active = NodeRegistry::new(
        pool.clone(),
        "dead-active".into(),
        NodeRole::Leader,
        format!("http://127.0.0.1:{dead_port}"),
    );
    dead_active
        .register(60)
        .await
        .expect("register dead active");
    let _: Option<String> = pool
        .set(LEASE_KEY, "dead-active", None, None, false)
        .await
        .expect("set dead lease");
    let dead = Arc::new(dead_state);
    let dead_standby_port = start_admin(dead);
    let r = http(
        dead_standby_port,
        reqwest::Method::POST,
        "/api/v1/providers",
        Some(serde_json::json!({"id":"p2","key":"x","name":"X","endpoint":"https://x.example.com","weight":1,"created_at":"","updated_at":""})),
    )
    .await;
    assert_eq!(
        r.status().as_u16(),
        502,
        "unreachable active → 502, no local write"
    );
    assert!(
        repo::list_providers(&dead_pool).await.unwrap().is_empty(),
        "standby never self-promotes via the forwarding path"
    );
}
