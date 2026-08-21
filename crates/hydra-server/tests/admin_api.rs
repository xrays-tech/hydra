//! §2 (wave-5) — Admin REST API integration suite.
//!
//! Exercises the REAL `AdminService` (a Pingora `ServeHttp` app) via real HTTP
//! requests from `reqwest` against a real Pingora `Service` bound to
//! `127.0.0.1:0`, backed by a real `:memory:` SQLite. No internal logic is
//! mocked (dev-plan §1 铁律 2): `db::repo`, `ConfigStore`, `AuthCache` and
//! `CircuitBreaker` are the production types.

#![cfg(all(feature = "db", feature = "http-client", feature = "proxy"))]

mod common;

use std::sync::Arc;
use std::time::Duration;

use hydra_core::breaker::BreakerConfig;
use hydra_server::admin::{AdminService, AdminState};
use hydra_server::crypto::{KeyProvider, StaticKeyProvider};
use hydra_server::db as repo;
use hydra_server::http::{AuthCache, AuthConfig, HttpAuthChecker};
use hydra_server::proxy::breaker_wrap::CircuitBreaker;
use hydra_server::store::ConfigStore;
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::services::listening::Service;

const TOKEN: &str = "test-admin-token";

/// Bind an ephemeral port, return it, release so Pingora can rebind.
fn ephemeral_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    l.local_addr().expect("addr").port()
}

/// Build a fresh admin state on a fresh `:memory:` DB.
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
        pool,
        store,
        auth,
        breaker,
        key_provider,
        Some(TOKEN.to_string()),
        None,
        hydra_server::proxy::admission::AdmissionControl::new(),
    ))
}

/// Start a real Pingora `Service` hosting `AdminService` on an ephemeral port.
fn start_admin(state: Arc<AdminState>) -> u16 {
    let port = ephemeral_port();
    let addr = format!("127.0.0.1:{port}");
    let app = AdminService::new(state);
    let mut server = Server::new(Some(Opt::default())).expect("Server::new");
    server.bootstrap();
    let mut svc = Service::new("admin-test".to_string(), app);
    svc.add_tcp(&addr);
    server.add_service(svc);
    std::thread::spawn(move || server.run_forever());
    port
}

/// Issue a request, retrying briefly until the admin server is ready.
async fn req(
    port: u16,
    method: reqwest::Method,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> reqwest::Response {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client");
    let url = format!("http://127.0.0.1:{port}{path}");
    let mut last = None;
    for _ in 0..50 {
        let mut b = client.request(method.clone(), &url);
        if let Some(t) = token {
            b = b.bearer_auth(t);
        }
        if let Some(body) = body {
            b = b
                .header("content-type", "application/json")
                .body(body.to_string());
        }
        match b.send().await {
            Ok(r) => return r,
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    panic!("admin server never ready: {last:?}");
}

// ===========================================================================
// §2.1 — auth gate + 404
// ===========================================================================

#[tokio::test]
async fn admin_requires_token() {
    let state = admin_state().await;
    let port = start_admin(state);
    // Missing token → 401.
    let r = req(port, reqwest::Method::GET, "/api/v1/health", None, None).await;
    assert_eq!(r.status(), 401);
    // Wrong token → 401.
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/health",
        Some("wrong"),
        None,
    )
    .await;
    assert_eq!(r.status(), 401);
    // Correct token → 200.
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/health",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn admin_unknown_path_404() {
    let state = admin_state().await;
    let port = start_admin(state);
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/nope",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 404);
    let body: serde_json::Value = r.json().await.expect("json");
    assert_eq!(body["error"]["code"], "not_found");
    assert!(body["error"]["trace_id"]
        .as_str()
        .unwrap()
        .starts_with("hydra"));
}

// ===========================================================================
// §2.2 — provider CRUD (incl. UNIQUE conflict + reload snapshot)
// ===========================================================================

#[tokio::test]
async fn provider_crud_http() {
    let state = admin_state().await;
    let port = start_admin(state.clone());

    let body = r#"{"id":"p1","key":"openai","name":"OpenAI","endpoint":"https://api.openai.com","weight":1,"created_at":"","updated_at":""}"#;
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/providers",
        Some(TOKEN),
        Some(body),
    )
    .await;
    assert_eq!(r.status(), 201);
    let created: serde_json::Value = r.json().await.expect("json");
    assert_eq!(created["id"], "p1");
    assert_eq!(created["key"], "openai");
    // created_at/updated_at filled by the server.
    assert!(!created["created_at"].as_str().unwrap().is_empty());

    // GET list.
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/providers",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 200);
    let list: serde_json::Value = r.json().await.expect("json");
    assert_eq!(list.as_array().unwrap().len(), 1);

    // GET by id.
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/providers/p1",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<serde_json::Value>().await.unwrap()["weight"], 1);

    // PUT update.
    let upd = r#"{"id":"p1","key":"openai","name":"Renamed","endpoint":"https://api.openai.com","weight":9,"created_at":"x","updated_at":""}"#;
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/providers/p1",
        Some(TOKEN),
        Some(upd),
    )
    .await;
    assert_eq!(r.status(), 200);
    let updated: serde_json::Value = r.json().await.expect("json");
    assert_eq!(updated["weight"], 9);
    assert_eq!(updated["name"], "Renamed");

    // Write-after consistency: snapshot reflects the new weight.
    let snap = state.store.snapshot();
    assert_eq!(snap.providers.get("p1").unwrap().weight, 9);

    // DELETE.
    let r = req(
        port,
        reqwest::Method::DELETE,
        "/api/v1/providers/p1",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 204);

    // Duplicate key → 409.
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/providers",
        Some(TOKEN),
        Some(body),
    )
    .await;
    // (After delete it's gone, so this inserts fine first time; insert a second
    // to force the UNIQUE conflict.)
    assert_eq!(r.status(), 201);
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/providers",
        Some(TOKEN),
        Some(body),
    )
    .await;
    assert_eq!(r.status(), 409);
    let eb: serde_json::Value = r.json().await.expect("json");
    assert_eq!(eb["error"]["code"], "conflict");
}

#[tokio::test]
async fn provider_crud_generates_id_when_empty() {
    let state = admin_state().await;
    let port = start_admin(state);
    let body = r#"{"id":"","key":"anthropic","name":"A","endpoint":"https://api.anthropic.com","weight":1,"created_at":"","updated_at":""}"#;
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/providers",
        Some(TOKEN),
        Some(body),
    )
    .await;
    assert_eq!(r.status(), 201);
    let created: serde_json::Value = r.json().await.expect("json");
    assert!(!created["id"].as_str().unwrap().is_empty());
}

// ===========================================================================
// §2.2 — provider-model CRUD (FK + status CHECK)
// ===========================================================================

#[tokio::test]
async fn provider_model_crud_http() {
    let state = admin_state().await;
    let port = start_admin(state);
    // Parent provider first.
    let p = r#"{"id":"p1","key":"openai","name":"O","endpoint":"https://api.openai.com","weight":1,"created_at":"","updated_at":""}"#;
    let _ = req(
        port,
        reqwest::Method::POST,
        "/api/v1/providers",
        Some(TOKEN),
        Some(p),
    )
    .await;

    // FK violation: model → non-existent provider → 400.
    let bad = r#"{"id":"m9","key":"gpt-4","name":"g","provider_id":"ghost","status":1}"#;
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/provider-models",
        Some(TOKEN),
        Some(bad),
    )
    .await;
    assert_eq!(r.status(), 400);

    // CHECK violation: invalid status.
    let badstatus = r#"{"id":"mx","key":"k","name":"g","provider_id":"p1","status":7}"#;
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/provider-models",
        Some(TOKEN),
        Some(badstatus),
    )
    .await;
    assert_eq!(r.status(), 400);

    // Valid model.
    let m = r#"{"id":"m1","key":"gpt-4","name":"gpt-4","provider_id":"p1","status":1}"#;
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/provider-models",
        Some(TOKEN),
        Some(m),
    )
    .await;
    assert_eq!(r.status(), 201);

    // UNIQUE(key, provider_id) conflict.
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/provider-models",
        Some(TOKEN),
        Some(m),
    )
    .await;
    assert_eq!(r.status(), 409);

    // GET / DELETE.
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/provider-models/m1",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 200);
    let r = req(
        port,
        reqwest::Method::DELETE,
        "/api/v1/provider-models/m1",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 204);
}

// ===========================================================================
// §2.3 — provider-key masking (P1-5: NEVER returns plaintext)
// ===========================================================================

#[tokio::test]
async fn provider_key_crud_http() {
    let state = admin_state().await;
    let port = start_admin(state);
    let p = r#"{"id":"p1","key":"openai","name":"O","endpoint":"https://api.openai.com","weight":1,"created_at":"","updated_at":""}"#;
    let _ = req(
        port,
        reqwest::Method::POST,
        "/api/v1/providers",
        Some(TOKEN),
        Some(p),
    )
    .await;

    let plaintext = "sk-supersecret-12345";
    let k = r#"{"id":"k1","provider_id":"p1","api_key":"sk-supersecret-12345","created_at":""}"#;
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/provider-keys",
        Some(TOKEN),
        Some(k),
    )
    .await;
    assert_eq!(r.status(), 201);
    let created: serde_json::Value = r.json().await.expect("json");
    // Create ALWAYS returns masked form (P1-5: never plaintext).
    let created_key = created["api_key"].as_str().unwrap();
    assert_ne!(created_key, plaintext);
    assert!(
        created_key.contains('*'),
        "masked key should contain stars, got: {created_key}"
    );
    assert!(
        !created_key.contains(plaintext),
        "masked key must not contain plaintext"
    );

    // List — always masked, even without ?reveal.
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/provider-keys",
        Some(TOKEN),
        None,
    )
    .await;
    let list: serde_json::Value = r.json().await.expect("json");
    let v = list.as_array().unwrap();
    assert_eq!(v.len(), 1);
    let listed = v[0]["api_key"].as_str().unwrap();
    assert_ne!(listed, plaintext);
    assert!(
        listed.contains('*'),
        "masked key should contain stars, got: {listed}"
    );
    assert!(
        !listed.contains(plaintext),
        "masked key must not contain plaintext"
    );

    // ?reveal=1 is accepted (200) but is now a NO-OP — still masked (P1-5).
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/provider-keys?reveal=1",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 200);
    let list: serde_json::Value = r.json().await.expect("json");
    let revealed = list.as_array().unwrap()[0]["api_key"].as_str().unwrap();
    assert_ne!(
        revealed, plaintext,
        "?reveal=1 must NOT return plaintext (P1-5)"
    );
    assert!(
        revealed.contains('*'),
        "masked key should contain stars even with ?reveal=1, got: {revealed}"
    );
    assert!(
        !revealed.contains(plaintext),
        "masked key must not contain plaintext even with ?reveal=1"
    );

    // Single-item GET — always masked.
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/provider-keys/k1",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 200);
    let item: serde_json::Value = r.json().await.expect("json");
    let item_key = item["api_key"].as_str().unwrap();
    assert_ne!(item_key, plaintext);
    assert!(
        item_key.contains('*'),
        "masked key should contain stars, got: {item_key}"
    );
    assert!(
        !item_key.contains(plaintext),
        "masked key must not contain plaintext"
    );
}

// ===========================================================================
// §2.4 — tenant CRUD (auth_url required + domain UNIQUE)
// ===========================================================================

#[tokio::test]
async fn tenant_crud_http() {
    let state = admin_state().await;
    let port = start_admin(state);

    // auth_url empty → 400.
    let bad = r#"{"id":"t1","name":"T","domain":"acme.com","auth_url":"","cert_key":null,"cert_file":null,"enabled":true,"created_at":"","updated_at":""}"#;
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/tenants",
        Some(TOKEN),
        Some(bad),
    )
    .await;
    assert_eq!(r.status(), 400);

    // Valid.
    let t = r#"{"id":"t1","name":"T","domain":"acme.com","auth_url":"https://auth.acme.com/v","cert_key":null,"cert_file":null,"enabled":true,"created_at":"","updated_at":""}"#;
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/tenants",
        Some(TOKEN),
        Some(t),
    )
    .await;
    assert_eq!(r.status(), 201);

    // Domain UNIQUE conflict → 409.
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/tenants",
        Some(TOKEN),
        Some(t),
    )
    .await;
    assert_eq!(r.status(), 409);

    // PUT + DELETE.
    let upd = r#"{"id":"t1","name":"T2","domain":"acme.com","auth_url":"https://auth.acme.com/v2","cert_key":null,"cert_file":null,"enabled":false,"created_at":"x","updated_at":""}"#;
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/tenants/t1",
        Some(TOKEN),
        Some(upd),
    )
    .await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.json::<serde_json::Value>().await.unwrap()["auth_url"],
        "https://auth.acme.com/v2"
    );
    let r = req(
        port,
        reqwest::Method::DELETE,
        "/api/v1/tenants/t1",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 204);
}

// ===========================================================================
// §2.5 / §2.6 — tenant-provider / tenant-model (UNIQUE conflict)
// ===========================================================================

#[tokio::test]
async fn tenant_provider_and_model_crud_http() {
    let state = admin_state().await;
    let port = start_admin(state);
    let p = r#"{"id":"p1","key":"openai","name":"O","endpoint":"https://api.openai.com","weight":1,"created_at":"","updated_at":""}"#;
    let _ = req(
        port,
        reqwest::Method::POST,
        "/api/v1/providers",
        Some(TOKEN),
        Some(p),
    )
    .await;
    let t = r#"{"id":"t1","name":"T","domain":"acme.com","auth_url":"https://a.example/v","cert_key":null,"cert_file":null,"enabled":true,"created_at":"","updated_at":""}"#;
    let _ = req(
        port,
        reqwest::Method::POST,
        "/api/v1/tenants",
        Some(TOKEN),
        Some(t),
    )
    .await;

    let tp = r#"{"id":"tp1","tenant_id":"t1","provider_id":"p1"}"#;
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/tenant-providers",
        Some(TOKEN),
        Some(tp),
    )
    .await;
    assert_eq!(r.status(), 201);
    // UNIQUE(tenant_id, provider_id) conflict → 409.
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/tenant-providers",
        Some(TOKEN),
        Some(tp),
    )
    .await;
    assert_eq!(r.status(), 409);

    let tm = r#"{"id":"tm1","tenant_id":"t1","model_key":"gpt-4"}"#;
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/tenant-models",
        Some(TOKEN),
        Some(tm),
    )
    .await;
    assert_eq!(r.status(), 201);
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/tenant-models",
        Some(TOKEN),
        Some(tm),
    )
    .await;
    assert_eq!(r.status(), 409);
}

// ===========================================================================
// §2.7 — limit-role CRUD (window CHECK)
// ===========================================================================

#[tokio::test]
async fn limit_role_crud_http() {
    let state = admin_state().await;
    let port = start_admin(state);

    // Invalid window → CHECK violation → 400.
    let bad = r#"{"id":"r1","name":"r","matching_key":null,"matching_model":null,"matching_tenant":null,"matching_provider":null,"limit_count":100,"limit_token":null,"window":"z","enabled":true,"created_at":""}"#;
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/limit-roles",
        Some(TOKEN),
        Some(bad),
    )
    .await;
    assert_eq!(r.status(), 400);

    // Valid.
    let r1 = r#"{"id":"r1","name":"r","matching_key":null,"matching_model":null,"matching_tenant":"t1","matching_provider":null,"limit_count":100,"limit_token":null,"window":"m","enabled":true,"created_at":""}"#;
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/limit-roles",
        Some(TOKEN),
        Some(r1),
    )
    .await;
    assert_eq!(r.status(), 201);

    // GET / PUT / DELETE.
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/limit-roles/r1",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 200);
    let upd = r#"{"id":"r1","name":"r2","matching_key":null,"matching_model":null,"matching_tenant":"t1","matching_provider":null,"limit_count":50,"limit_token":null,"window":"h","enabled":true,"created_at":"x"}"#;
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/limit-roles/r1",
        Some(TOKEN),
        Some(upd),
    )
    .await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.json::<serde_json::Value>().await.unwrap()["limit_count"],
        50
    );
    let r = req(
        port,
        reqwest::Method::DELETE,
        "/api/v1/limit-roles/r1",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 204);
}

// ===========================================================================
// §2.8/§2.9 — write triggers reload_all + returns latest snapshot
// ===========================================================================

#[tokio::test]
async fn reload_endpoint_triggers_reload_all() {
    let state = admin_state().await;
    let port = start_admin(state.clone());
    // Insert a provider directly via repo, then POST /reload to pick it up.
    repo::insert_provider(
        &state.pool,
        &hydra_core::model::Provider {
            id: "px".into(),
            key: "direct".into(),
            name: "D".into(),
            endpoint: "https://api.direct.com".into(),
            weight: 3,
            created_at: "2026-01-01 00:00:00".into(),
            updated_at: "2026-01-01 00:00:00".into(),
            max_concurrency: None,
            max_queue_depth: None,
            queue_wait_timeout_ms: None,
        },
    )
    .await
    .expect("insert");
    // Snapshot before reload does NOT include the directly-inserted row.
    assert!(!state.store.snapshot().providers.contains_key("px"));

    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/reload",
        Some(TOKEN),
        Some("{}"),
    )
    .await;
    assert_eq!(r.status(), 200);
    let body: serde_json::Value = r.json().await.expect("json");
    assert_eq!(body["status"], "reloaded");
    assert_eq!(body["providers"], 1);
    // Snapshot now reflects the reloaded row.
    assert_eq!(
        state.store.snapshot().providers.get("px").unwrap().weight,
        3
    );
}

// ===========================================================================
// §2.3 — auth cache invalidation (by keys, by tenant, unknown)
// ===========================================================================

#[tokio::test]
async fn auth_cache_invalidate() {
    let state = admin_state().await;
    let port = start_admin(state.clone());

    // Populate the cache directly: two keys for tenant t1, one for t2.
    state
        .auth
        .cache()
        .set("t1", "sk-aaa", true, Duration::from_secs(300));
    state
        .auth
        .cache()
        .set("t1", "sk-bbb", true, Duration::from_secs(300));
    state
        .auth
        .cache()
        .set("t2", "sk-ccc", true, Duration::from_secs(300));
    assert_eq!(state.auth.cache().len(), 3);

    // By keys for t1 → invalidates 2.
    let body = r#"{"tenant_id":"t1","api_keys":["sk-aaa","sk-bbb"]}"#;
    let r = req(
        port,
        reqwest::Method::DELETE,
        "/api/v1/auth/cache",
        Some(TOKEN),
        Some(body),
    )
    .await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("json");
    assert_eq!(v["invalidated"], 2);
    assert_eq!(v["tenant_id"], "t1");
    assert_eq!(state.auth.cache().len(), 1);

    // Unknown key → 0, no error.
    let body = r#"{"tenant_id":"t1","api_keys":["sk-nope"]}"#;
    let r = req(
        port,
        reqwest::Method::DELETE,
        "/api/v1/auth/cache",
        Some(TOKEN),
        Some(body),
    )
    .await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.json::<serde_json::Value>().await.unwrap()["invalidated"],
        0
    );

    // By tenant (t2) → invalidates all for t2.
    let body = r#"{"tenant_id":"t2"}"#;
    let r = req(
        port,
        reqwest::Method::DELETE,
        "/api/v1/auth/cache",
        Some(TOKEN),
        Some(body),
    )
    .await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.json::<serde_json::Value>().await.unwrap()["invalidated"],
        1
    );
    assert_eq!(state.auth.cache().len(), 0);
}

// ===========================================================================
// §2.4 — breaker inspect / reset
// ===========================================================================

#[tokio::test]
async fn breaker_inspect_and_reset() {
    let state = admin_state().await;
    let port = start_admin(state.clone());

    // No dead providers initially.
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/breaker",
        Some(TOKEN),
        None,
    )
    .await;
    let v: serde_json::Value = r.json().await.expect("json");
    assert!(v["dead"].as_array().unwrap().is_empty());

    // Force a provider dead via the breaker directly (threshold=2).
    state.breaker.on_failure("p1");
    state.breaker.on_failure("p1");
    assert!(state.breaker.is_dead("p1"));

    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/breaker",
        Some(TOKEN),
        None,
    )
    .await;
    let v: serde_json::Value = r.json().await.expect("json");
    assert!(v["dead"].as_array().unwrap().iter().any(|x| x == "p1"));

    // Manual reset.
    let r = req(
        port,
        reqwest::Method::DELETE,
        "/api/v1/breaker/p1",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("json");
    assert_eq!(v["reset"], "p1");
    assert_eq!(v["was_dead"], true);
    assert!(!state.breaker.is_dead("p1"));
}

// ===========================================================================
// §2.5 — health
// ===========================================================================

#[tokio::test]
async fn health_returns_ok() {
    let state = admin_state().await;
    let port = start_admin(state);
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/health",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("json");
    assert_eq!(v["status"], "ok");
    assert_eq!(v["db"], "ok");
}

// ===========================================================================
// Concurrency admission snapshot (design §10 / §13.2)
// ===========================================================================

#[tokio::test]
async fn concurrency_snapshot_reports_live_gates() {
    use hydra_core::config::ConcurrencyPolicy;
    use hydra_server::proxy::admission::AdmissionControl;

    // Build a dedicated admin state with a shared admission controller so we
    // can seed live gates before the server starts.
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
    let admission = AdmissionControl::new();

    // Seed two gates: hold one permit on "p-capped" (max_concurrency=2) so
    // inflight=1/available=1, and leave "p-idle" at 0 inflight.
    let policy = ConcurrencyPolicy {
        max_concurrency: 2,
        max_queue_depth: 4,
        queue_wait_timeout_ms: 1000,
    };
    let _held_permit = admission
        .acquire("p-capped", policy)
        .await
        .expect("acquire p-capped");
    let _idle_permit = admission
        .acquire("p-idle", policy)
        .await
        .expect("acquire p-idle");
    drop(_idle_permit); // p-idle back to inflight=0

    let state = Arc::new(AdminState::new(
        pool,
        store,
        auth,
        breaker,
        key_provider,
        Some(TOKEN.to_string()),
        None,
        admission,
    ));
    let port = start_admin(state);

    // Act.
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/concurrency",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("json");

    let providers = v["providers"].as_array().expect("providers array");
    assert_eq!(providers.len(), 2, "two live gates");

    let capped = providers
        .iter()
        .find(|e| e["provider_id"] == "p-capped")
        .expect("p-capped entry");
    assert_eq!(capped["max_concurrency"], 2);
    assert_eq!(capped["inflight"], 1);
    assert_eq!(capped["available"], 1);
    assert_eq!(capped["queue_depth"], 0);

    let idle = providers
        .iter()
        .find(|e| e["provider_id"] == "p-idle")
        .expect("p-idle entry");
    assert_eq!(idle["max_concurrency"], 2);
    assert_eq!(idle["inflight"], 0);
    assert_eq!(idle["available"], 2);
    assert_eq!(idle["queue_depth"], 0);
}

#[tokio::test]
async fn concurrency_snapshot_empty_when_no_gates() {
    let state = admin_state().await;
    let port = start_admin(state);
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/concurrency",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("json");
    assert!(v["providers"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn concurrency_snapshot_requires_admin_token() {
    let state = admin_state().await;
    let port = start_admin(state);
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/concurrency",
        None,
        None,
    )
    .await;
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn provider_key_bindings_crud_http() {
    let state = admin_state().await;
    let port = start_admin(state.clone());

    // Seed a provider so the FK holds.
    let p = r#"{"id":"p1","key":"openai","name":"O","endpoint":"https://api.openai.com","weight":1,"created_at":"","updated_at":""}"#;
    let _ = req(
        port,
        reqwest::Method::POST,
        "/api/v1/providers",
        Some(TOKEN),
        Some(p),
    )
    .await;

    // Create → 201.
    let b = r#"{"id":"b1","key_prefix":"sk_aaa_","provider_id":"p1","enabled":true,"created_at":"","updated_at":""}"#;
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/provider-key-bindings",
        Some(TOKEN),
        Some(b),
    )
    .await;
    assert_eq!(r.status(), 201);
    let created: serde_json::Value = r.json().await.expect("json");
    assert_eq!(created["key_prefix"], "sk_aaa_");

    // Hot reload: the in-memory snapshot now carries the enabled binding.
    let snap = state.store.snapshot();
    assert_eq!(snap.key_prefix_bindings.len(), 1);
    assert_eq!(snap.key_prefix_bindings[0].provider_id, "p1");
    drop(snap);

    // Duplicate prefix → 409 (UNIQUE).
    let dup = r#"{"id":"b2","key_prefix":"sk_aaa_","provider_id":"p1","enabled":true,"created_at":"","updated_at":""}"#;
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/provider-key-bindings",
        Some(TOKEN),
        Some(dup),
    )
    .await;
    assert_eq!(r.status(), 409);

    // Empty prefix → 400 (handler guard).
    let empty = r#"{"id":"b3","key_prefix":"","provider_id":"p1","enabled":true,"created_at":"","updated_at":""}"#;
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/provider-key-bindings",
        Some(TOKEN),
        Some(empty),
    )
    .await;
    assert_eq!(r.status(), 400);

    // Unknown provider → 400 (FK violation).
    let ghost = r#"{"id":"b4","key_prefix":"hk_","provider_id":"ghost","enabled":true,"created_at":"","updated_at":""}"#;
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/provider-key-bindings",
        Some(TOKEN),
        Some(ghost),
    )
    .await;
    assert_eq!(r.status(), 400);

    // List → 1 row.
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/provider-key-bindings",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 200);
    let list: serde_json::Value = r.json().await.expect("json");
    assert_eq!(list.as_array().unwrap().len(), 1);

    // Update (PUT) → 200, disabled reflected.
    let upd = r#"{"id":"b1","key_prefix":"sk_aaa_v2","provider_id":"p1","enabled":false,"created_at":"","updated_at":""}"#;
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/provider-key-bindings/b1",
        Some(TOKEN),
        Some(upd),
    )
    .await;
    assert_eq!(r.status(), 200);
    let item: serde_json::Value = r.json().await.expect("json");
    assert_eq!(item["enabled"], serde_json::Value::Bool(false));

    // Disabled binding leaves the hot snapshot.
    let snap2 = state.store.snapshot();
    assert_eq!(
        snap2.key_prefix_bindings.len(),
        0,
        "disabled binding not loaded"
    );
    drop(snap2);

    // Single GET → 200; unknown id → 404.
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/provider-key-bindings/b1",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 200);
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/provider-key-bindings/nope",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 404);

    // DELETE → 204.
    let r = req(
        port,
        reqwest::Method::DELETE,
        "/api/v1/provider-key-bindings/b1",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 204);
}
