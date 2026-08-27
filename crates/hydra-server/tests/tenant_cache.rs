//! Tenant self-service auth-cache invalidation (migration 0009): the
//! `POST /api/v1/tenants/{tenant_id}/auth/cache/invalidate` endpoint gated by
//! the TENANT access token (not the admin token), plus the tenant token
//! lifecycle via the admin API (set/rotate/clear, has_access_token view, never
//! echoed).

#![cfg(all(feature = "db", feature = "http-client", feature = "proxy"))]

mod common;

use std::sync::Arc;
use std::time::Duration;

use hydra_core::breaker::BreakerConfig;
use hydra_server::admin::{AdminService, AdminState};
use hydra_server::crypto::{KeyProvider, StaticKeyProvider};
use hydra_server::http::{AuthCache, AuthConfig, HttpAuthChecker};
use hydra_server::proxy::breaker_wrap::CircuitBreaker;
use hydra_server::store::ConfigStore;
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::services::listening::Service;

const TOKEN: &str = "test-admin-token";
const TENANT_TOKEN: &str = "sk-tenant-self-service-0123456789abcdef";

fn ephemeral_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    l.local_addr().expect("addr").port()
}

async fn admin_state() -> Arc<AdminState> {
    let pool = common::setup_pool().await;
    let key_provider: Arc<dyn KeyProvider> = Arc::new(StaticKeyProvider::new([1u8; 32], 1));
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
    Arc::new(AdminState::new(
        Some(pool),
        store,
        auth,
        breaker,
        key_provider,
        Some(TOKEN.to_string()),
        None,
        hydra_server::proxy::admission::AdmissionControl::new(),
        false,
        None, // no cluster token in tests
        None, // no leader election in tests
    ))
}

fn start_admin(state: Arc<AdminState>) -> u16 {
    let port = ephemeral_port();
    let addr = format!("127.0.0.1:{port}");
    let app = AdminService::new(state);
    let mut server = Server::new(Some(Opt::default())).expect("Server::new");
    server.bootstrap();
    let mut svc = Service::new("tenant-cache-test".to_string(), app);
    svc.add_tcp(&addr);
    server.add_service(svc);
    std::thread::spawn(move || server.run_forever());
    port
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client")
}

async fn wait_ready<T, F>(mut f: F) -> T
where
    F: FnMut() -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<T>> + Send>> + Send,
{
    for _ in 0..50 {
        if let Some(v) = f().await {
            return v;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("admin server never ready");
}

async fn create_tenant(port: u16, id: &str, token: Option<&str>) -> serde_json::Value {
    let mut body = serde_json::json!({
        "id": id, "name": "Acme", "domain": format!("{id}.test"),
        "auth_url": "https://auth.example.com/v1/verify",
        "enabled": true, "created_at": "", "updated_at": ""
    });
    if let Some(t) = token {
        body["access_token"] = serde_json::Value::String(t.to_string());
    }
    wait_ready(|| {
        let c = client();
        let b = body.clone();
        Box::pin(async move {
            match c
                .post(format!("http://127.0.0.1:{port}/api/v1/tenants"))
                .header("authorization", format!("Bearer {TOKEN}"))
                .json(&b)
                .send()
                .await
            {
                Ok(r) => Some(r.json().await.expect("json")),
                Err(_) => None,
            }
        })
    })
    .await
}

async fn invalidate(port: u16, tenant_id: &str, bearer: &str, body: Option<serde_json::Value>) -> (u16, serde_json::Value) {
    let c = client();
    let mut req = c
        .post(format!("http://127.0.0.1:{port}/api/v1/tenants/{tenant_id}/auth/cache/invalidate"))
        .header("authorization", format!("Bearer {bearer}"));
    if let Some(b) = body {
        req = req.json(&b);
    }
    let r = req.send().await.expect("request");
    let status = r.status().as_u16();
    let json: serde_json::Value = r.json().await.unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn tenant_token_set_never_echoed_and_has_flag() {
    let port = start_admin(admin_state().await);
    let created = create_tenant(port, "t-acme", Some(TENANT_TOKEN)).await;
    assert_eq!(created["has_access_token"], true, "got {created}");
    let body = created.to_string();
    assert!(!body.contains("access_token_hash"), "hash leaked: {body}");
    assert!(!body.contains(TENANT_TOKEN), "token leaked: {body}");
    // list view also carries the flag
    let list: serde_json::Value = client()
        .get(format!("http://127.0.0.1:{port}/api/v1/tenants"))
        .header("authorization", format!("Bearer {TOKEN}"))
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("json");
    let row = list.as_array().unwrap().iter().find(|r| r["id"] == "t-acme").expect("row");
    assert_eq!(row["has_access_token"], true, "got {list}");
    assert!(!row.to_string().contains("access_token_hash"), "hash leaked in list");
}

#[tokio::test]
async fn tenant_token_blank_keeps_and_empty_clears() {
    let port = start_admin(admin_state().await);
    create_tenant(port, "t-rot", Some(TENANT_TOKEN)).await;
    let c = client();
    // blank (null) keeps the token
    let body = serde_json::json!({
        "id": "t-rot", "name": "Rot", "domain": "t-rot.test",
        "auth_url": "https://auth.example.com/v1/verify", "enabled": true,
        "created_at": "", "updated_at": "", "access_token": null
    });
    let r = c
        .put(format!("http://127.0.0.1:{port}/api/v1/tenants/t-rot"))
        .header("authorization", format!("Bearer {TOKEN}"))
        .json(&body)
        .send()
        .await
        .expect("put");
    let updated: serde_json::Value = r.json().await.expect("json");
    assert_eq!(updated["has_access_token"], true, "blank must keep: got {updated}");
    // explicit "" clears
    let body2 = serde_json::json!({
        "id": "t-rot", "name": "Rot", "domain": "t-rot.test",
        "auth_url": "https://auth.example.com/v1/verify", "enabled": true,
        "created_at": "", "updated_at": "", "access_token": ""
    });
    let r2 = c
        .put(format!("http://127.0.0.1:{port}/api/v1/tenants/t-rot"))
        .header("authorization", format!("Bearer {TOKEN}"))
        .json(&body2)
        .send()
        .await
        .expect("put2");
    let cleared: serde_json::Value = r2.json().await.expect("json");
    assert_eq!(cleared["has_access_token"], false, "empty must clear: got {cleared}");
}

#[tokio::test]
async fn tenant_token_too_short_rejected_400() {
    let port = start_admin(admin_state().await);
    let body = serde_json::json!({
        "id": "t-short", "name": "S", "domain": "t-short.test",
        "auth_url": "https://auth.example.com/v1/verify", "enabled": true,
        "created_at": "", "updated_at": "", "access_token": "short"
    });
    let r = wait_ready(|| {
        let c = client();
        let b = body.clone();
        Box::pin(async move {
            c.post(format!("http://127.0.0.1:{port}/api/v1/tenants"))
                .header("authorization", format!("Bearer {TOKEN}"))
                .json(&b)
                .send()
                .await
                .ok()
        })
    })
    .await;
    assert_eq!(r.status(), 400);
    let body: serde_json::Value = r.json().await.expect("json");
    assert_eq!(body["error"]["code"], "invalid_access_token", "got {body}");
}

#[tokio::test]
async fn tenant_token_invalidates_own_cache() {
    let state = admin_state().await;
    let port = start_admin(state.clone());
    create_tenant(port, "t-cache", Some(TENANT_TOKEN)).await;
    // seed one cached allow decision for this tenant's key
    state
        .auth
        .cache()
        .set("t-cache", "sk-real-key-123", true, Duration::from_secs(300))
        .await;
    let (status, json) = invalidate(port, "t-cache", TENANT_TOKEN, None).await;
    assert_eq!(status, 200, "got {json}");
    assert_eq!(json["tenant_id"], "t-cache", "got {json}");
    assert_eq!(json["invalidated"], 1, "got {json}");
}

#[tokio::test]
async fn tenant_token_invalidates_selected_keys() {
    let state = admin_state().await;
    let port = start_admin(state.clone());
    create_tenant(port, "t-sel", Some(TENANT_TOKEN)).await;
    state.auth.cache().set("t-sel", "sk-a", true, Duration::from_secs(300)).await;
    state.auth.cache().set("t-sel", "sk-b", true, Duration::from_secs(300)).await;
    let (status, json) = invalidate(
        port,
        "t-sel",
        TENANT_TOKEN,
        Some(serde_json::json!({ "api_keys": ["sk-a"] })),
    )
    .await;
    assert_eq!(status, 200, "got {json}");
    assert_eq!(json["invalidated"], 1, "got {json}");
}

#[tokio::test]
async fn tenant_token_wrong_or_missing_rejected() {
    let port = start_admin(admin_state().await);
    create_tenant(port, "t-auth", Some(TENANT_TOKEN)).await;
    // wrong token
    let (s1, j1) = invalidate(port, "t-auth", "sk-wrong-token-abcdefghijklmnop", None).await;
    assert_eq!(s1, 401, "got {j1}");
    // no token
    let (s2, j2) = invalidate(port, "t-auth", "", None).await;
    assert_eq!(s2, 401, "got {j2}");
    // admin token is NOT a tenant token here
    let (s3, j3) = invalidate(port, "t-auth", TOKEN, None).await;
    assert_eq!(s3, 401, "got {j3}");
}

#[tokio::test]
async fn tenant_token_mismatched_url_tenant_is_403() {
    let port = start_admin(admin_state().await);
    create_tenant(port, "t-own", Some(TENANT_TOKEN)).await;
    create_tenant(port, "t-other", Some("sk-other-token-0123456789abcdef")).await;
    // t-own's token used against t-other's URL → 403 (no cross-tenant spoofing)
    let (s, j) = invalidate(port, "t-other", TENANT_TOKEN, None).await;
    assert_eq!(s, 403, "got {j}");
}

#[tokio::test]
async fn tenant_without_token_cannot_use_endpoint() {
    let port = start_admin(admin_state().await);
    create_tenant(port, "t-notoken", None).await;
    let (s, j) = invalidate(port, "t-notoken", "sk-anything-0123456789abcdef", None).await;
    assert_eq!(s, 401, "got {j}");
}
