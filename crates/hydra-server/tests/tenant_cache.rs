//! The tenant access-token lifecycle on the **management** API.
//!
//! Tokens live in the `tenant_access_token` table. The management API creates,
//! lists, and deletes them; the self-service surface lives on the **data plane**
//! under `/tenant/{tenant_id}/api/v1/` (see `tests/tenant_api.rs` and
//! `tests/tenant_api_cluster.rs`, contract `dev-docs/design-tenant-api.md`).
//!
//! The old single-column tenant token field and its old view are
//! gone (T10 Stage D). The plaintext token is minted by the management API and
//! returned ONCE; list and read routes never carry the token or its hash.

#![cfg(all(feature = "db", feature = "http-client", feature = "proxy"))]

mod common;

use std::sync::Arc;
use std::time::Duration;

use hydra_core::breaker::BreakerConfig;
use hydra_server::admin::{AdminService, AdminState};
use hydra_server::crypto::{KeyProvider, StaticKeyProvider};
use hydra_server::proxy::breaker_wrap::CircuitBreaker;
use hydra_server::store::ConfigStore;
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::services::listening::Service;

const TOKEN: &str = "test-admin-token";

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
    let breaker = Arc::new(CircuitBreaker::new(BreakerConfig::new(2)));
    Arc::new(AdminState::new(
        pool,
        store,
        breaker,
        key_provider,
        Some(TOKEN.to_string()),
        hydra_server::proxy::admission::AdmissionControl::new(),
        None, // no leader gate in tests
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

async fn create_tenant(port: u16, id: &str) -> serde_json::Value {
    let body = serde_json::json!({
        "id": id,
        "name": "Acme",
        "domain": format!("{id}.test"),
        "cert_key": null,
        "cert_file": null,
        "enabled": true,
        "created_at": "",
        "updated_at": "",
    });
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

async fn create_access_token(port: u16, tenant_id: &str, name: &str) -> serde_json::Value {
    let body = serde_json::json!({ "name": name });
    wait_ready(|| {
        let c = client();
        let b = body.clone();
        let u = format!("http://127.0.0.1:{port}/api/v1/tenants/{tenant_id}/access-tokens");
        Box::pin(async move {
            match c
                .post(u)
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

async fn list_access_tokens(port: u16, tenant_id: &str) -> serde_json::Value {
    wait_ready(|| {
        let c = client();
        let u = format!("http://127.0.0.1:{port}/api/v1/tenants/{tenant_id}/access-tokens");
        Box::pin(async move {
            match c
                .get(u)
                .header("authorization", format!("Bearer {TOKEN}"))
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

async fn delete_access_token(port: u16, tenant_id: &str, token_id: &str) -> reqwest::Response {
    wait_ready(|| {
        let c = client();
        let u =
            format!("http://127.0.0.1:{port}/api/v1/tenants/{tenant_id}/access-tokens/{token_id}");
        Box::pin(async move {
            c.delete(u)
                .header("authorization", format!("Bearer {TOKEN}"))
                .send()
                .await
                .ok()
        })
    })
    .await
}

#[tokio::test]
async fn mints_a_tenant_access_token_and_never_echoes_the_plaintext_or_hash() {
    let port = start_admin(admin_state().await);
    create_tenant(port, "t-acme").await;
    let created = create_access_token(port, "t-acme", "ci").await;
    assert_eq!(created["name"], "ci", "got {created}");
    assert!(
        created["token"]
            .as_str()
            .unwrap_or_default()
            .starts_with("ht_"),
        "the mint response must carry the plaintext once: {created}"
    );

    let plaintext = created["token"].as_str().unwrap_or_default();
    let list_value = list_access_tokens(port, "t-acme").await;
    let rows = list_value.as_array().expect("list");
    let row = rows
        .iter()
        .find(|r| r["id"] == created["id"])
        .expect("token row");
    assert_eq!(row["name"], "ci", "got {row}");
    assert!(
        row["token"].is_null(),
        "the list must not echo the plaintext: {row}"
    );
    let list_text = list_value.to_string();
    let forbidden_hash_key = "access_token".to_owned() + "_hash";
    assert!(
        !list_text.contains(&forbidden_hash_key),
        "hash leaked in list: {list_text}"
    );
    assert!(
        !list_text.contains(plaintext),
        "plaintext leaked in list: {list_text}"
    );
}

#[tokio::test]
async fn deletes_an_access_token_without_touching_the_tenant() {
    let port = start_admin(admin_state().await);
    create_tenant(port, "t-del").await;
    let created = create_access_token(port, "t-del", "ci").await;
    let token_id = created["id"].as_str().expect("token id");

    let deleted = delete_access_token(port, "t-del", token_id).await;
    assert_eq!(deleted.status().as_u16(), 204, "delete must report success");
    let list_value = list_access_tokens(port, "t-del").await;
    assert!(
        list_value.as_array().expect("list").is_empty(),
        "deleted token is still visible: {list_value:?}"
    );

    let tenants: serde_json::Value = client()
        .get(format!("http://127.0.0.1:{port}/api/v1/tenants"))
        .header("authorization", format!("Bearer {TOKEN}"))
        .send()
        .await
        .expect("list tenants")
        .json()
        .await
        .expect("json");
    assert!(
        tenants
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == "t-del"),
        "deleting the token must not delete the tenant: {tenants}"
    );
}

#[tokio::test]
async fn rejects_a_blank_access_token_name() {
    let port = start_admin(admin_state().await);
    create_tenant(port, "t-name").await;
    let body = serde_json::json!({ "name": "   " });
    let r = client()
        .post(format!(
            "http://127.0.0.1:{port}/api/v1/tenants/t-name/access-tokens"
        ))
        .header("authorization", format!("Bearer {TOKEN}"))
        .json(&body)
        .send()
        .await
        .expect("create");
    assert_eq!(r.status().as_u16(), 400, "got {r:?}");
    let e: serde_json::Value = r.json().await.expect("json");
    assert_eq!(e["error"]["code"], "invalid_name", "got {e}");
}

#[tokio::test]
async fn rejects_an_unknown_tenant_access_token_create() {
    let port = start_admin(admin_state().await);
    let r = client()
        .post(format!(
            "http://127.0.0.1:{port}/api/v1/tenants/missing/access-tokens"
        ))
        .header("authorization", format!("Bearer {TOKEN}"))
        .json(&serde_json::json!({ "name": "ci" }))
        .send()
        .await
        .expect("create");
    assert_eq!(r.status().as_u16(), 404, "got {r:?}");
    let e: serde_json::Value = r.json().await.expect("json");
    assert_eq!(e["error"]["code"], "not_found", "got {e}");
}

// ---------------------------------------------------------------------------
// C16 — the old management-plane tenant token route is GONE
// ---------------------------------------------------------------------------

/// Both invalidation shapes used to be reachable from here: the tenant self-service
/// one sat on the management API in a special branch placed **before** the admin
/// gate, and the operator one was an ordinary admin route. Both are gone (retired
/// with the external-auth surface, T10 Stage B), so the ADMIN gate is what answers
/// now — for a tenant token, and an unknown-path 404 for an admin token.
///
/// **Both credentials must be asserted, because only one of them is a
/// falsification signal** (measured on the router, not assumed):
///
/// | credential | before the deletion | after | is it evidence? |
/// |---|---|---|---|
/// | tenant token | `200` (the special branch ran first) | `401` (falls through to the admin gate) | **yes** — 200→401 is the signal |
/// | admin token | `200` (the operator route ran) | `404` (unknown path) | **yes** — 200→404 is the signal |
///
/// Asserting only the 404 would therefore be a test that can never fail, and
/// asserting only "404" for the tenant token would be a test that can never pass.
#[tokio::test]
async fn the_retired_auth_cache_management_shapes_are_gone() {
    let port = start_admin(admin_state().await);
    create_tenant(port, "t-gone").await;
    let created = create_access_token(port, "t-gone", "ci").await;
    let token = created["token"].as_str().expect("token");
    let url = format!("http://127.0.0.1:{port}/api/v1/auth/cache");

    // (a) The tenant token: 200 while the old route existed, 401 now.
    let r = client()
        .post(&url)
        .header("authorization", format!("Bearer {token}"))
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

    // (b) The admin token: 404, which proves the operator route is retired rather
    //     than merely hidden (it used to answer here).
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
