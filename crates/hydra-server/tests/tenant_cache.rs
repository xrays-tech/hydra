//! The tenant access-token lifecycle on the **management** API (migration 0009):
//! set / rotate / clear, the `has_access_token` view, and that the token is
//! never echoed back.
//!
//! The self-service endpoints themselves moved to the **data plane**, where a
//! tenant can actually reach them without the operator exposing the whole
//! management API: `POST /tenant/{tenant_id}/api/v1/auth/cache/invalidate` and
//! friends. Their tests live in `tests/tenant_api.rs` (single node) and
//! `tests/tenant_api_cluster.rs` (real Redis), and the contract is
//! `dev-docs/design-tenant-api.md`. The five tests that used to live here drove
//! the deleted `POST /api/v1/tenants/{id}/auth/cache/invalidate` route; each of
//! their assertions is carried by a data-plane test (see the T9 record in
//! `dev-docs/aegis/plans/2026-09-17-tenant-api.md`), so this file no longer
//! asserts anything about cache invalidation.

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

/// See `common::ephemeral_port`: no bind-then-release race (the previous
/// local copy could hand two concurrent tests the same port).
fn ephemeral_port() -> u16 {
    common::ephemeral_port()
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
    let row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "t-acme")
        .expect("row");
    assert_eq!(row["has_access_token"], true, "got {list}");
    assert!(
        !row.to_string().contains("access_token_hash"),
        "hash leaked in list"
    );
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
    assert_eq!(
        updated["has_access_token"], true,
        "blank must keep: got {updated}"
    );
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
    assert_eq!(
        cleared["has_access_token"], false,
        "empty must clear: got {cleared}"
    );
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

// ---------------------------------------------------------------------------
// C16 — the old management-plane tenant route is GONE
// ---------------------------------------------------------------------------

/// The tenant's self-service endpoint used to live on the management API, at
/// `POST /api/v1/tenants/{id}/auth/cache/invalidate`, reachable with a TENANT
/// token through a special branch placed **before** the admin gate. It is now
/// served on the data plane only.
///
/// **Both credentials must be asserted, because only one of them is a
/// falsification signal** (measured on the router, not assumed):
///
/// | credential | before the deletion | after | is it evidence? |
/// |---|---|---|---|
/// | tenant token | `200` (the special branch ran first) | `401` (falls through to the admin gate) | **yes** — 200→401 is the signal |
/// | admin token | `404` (`parts.len() > 2` refuses the deep path) | `404` | **no** — identical either way |
///
/// Asserting only the 404 would therefore be a test that can never fail, and
/// asserting only "404" for the tenant token would be a test that can never pass.
#[tokio::test]
async fn the_old_management_plane_tenant_route_is_gone() {
    let port = start_admin(admin_state().await);
    create_tenant(port, "t-gone", Some(TENANT_TOKEN)).await;
    let url = format!("http://127.0.0.1:{port}/api/v1/tenants/t-gone/auth/cache/invalidate");

    // (a) The tenant token: 200 while the old route existed, 401 now.
    let r = client()
        .post(&url)
        .header("authorization", format!("Bearer {TENANT_TOKEN}"))
        .send()
        .await
        .expect("request");
    let status = r.status().as_u16();
    let body = r.text().await.unwrap_or_default();
    assert_eq!(
        status, 401,
        "a tenant token must no longer reach any management route: {body}"
    );
    assert!(
        body.contains("admin token"),
        "it must be the ADMIN gate that refuses it, not a stray route: {body}"
    );

    // (b) The admin token: 404, which proves the path is not an operator
    //     resource either. Same before and after, so it is a guard, not a signal.
    let r = client()
        .post(&url)
        .header("authorization", format!("Bearer {TOKEN}"))
        .send()
        .await
        .expect("request");
    assert_eq!(
        r.status().as_u16(),
        404,
        "the tenant self-service path must not exist on the management API"
    );
}
