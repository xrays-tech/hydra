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

#[tokio::test]
async fn tenant_models_path_rejects_non_get() {
    // The tenant model catalog endpoint is GET-only: a POST on the same path
    // (design-tenant-model-catalog §2.3 route branch guards method == GET)
    // must fall through to the generic deep-path 404, never reach the handler.
    let state = admin_state().await;
    let port = start_admin(state);
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/tenants/t1/models",
        Some(TOKEN),
        Some("{}"),
    )
    .await;
    assert_eq!(r.status(), 404);
    let body: serde_json::Value = r.json().await.expect("json");
    assert_eq!(body["error"]["code"], "not_found");
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

/// Tenant cert content via the admin API (migration 0007): PEM fields are
/// accepted, the private key is sealed at rest, and the API never echoes the
/// key back (management-plane plaintext rule, design §16.2).
#[tokio::test]
async fn tenant_cert_content_http() {
    let state = admin_state().await;
    let port = start_admin(state.clone());

    let cert_pem = "-----BEGIN CERTIFICATE-----\nCERTBODY\n-----END CERTIFICATE-----\n";
    let key_pem = "-----BEGIN PRIVATE KEY-----\nKEYBODY\n-----END PRIVATE KEY-----\n";

    // POST with content.
    let body = format!(
        r#"{{"id":"t1","name":"T","domain":"acme.com","auth_url":"https://auth.acme.com/v","cert_key":null,"cert_file":null,"enabled":true,"created_at":"","updated_at":"","cert_pem":{cert_pem:?},"cert_key_pem":{key_pem:?}}}"#,
        cert_pem = cert_pem,
        key_pem = key_pem
    );
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/tenants",
        Some(TOKEN),
        Some(&body),
    )
    .await;
    assert_eq!(r.status(), 201);
    let resp = r.json::<serde_json::Value>().await.unwrap();
    assert_eq!(resp["id"], "t1");
    // The response must not leak the private key (or any cert content).
    let resp_text = serde_json::to_string(&resp).unwrap();
    assert!(
        !resp_text.contains("KEYBODY"),
        "admin response must never contain the private key"
    );
    assert!(
        !resp_text.contains("CERTBODY"),
        "admin response has no cert content"
    );

    // GET also stays content-free.
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/tenants/t1",
        Some(TOKEN),
        None,
    )
    .await;
    let resp = r.json::<serde_json::Value>().await.unwrap();
    let resp_text = serde_json::to_string(&resp).unwrap();
    assert!(!resp_text.contains("KEYBODY"));
    assert!(!resp_text.contains("CERTBODY"));

    // The content IS stored (sealed) — verify through the repo.
    let kp = hydra_server::crypto::StaticKeyProvider::new([1u8; 32], 1);
    let got = hydra_server::db::get_tenant_cert(state.db(), &kp, "t1")
        .await
        .expect("get cert")
        .expect("cert present");
    assert_eq!(got.cert_pem.as_deref(), Some(cert_pem));
    assert_eq!(got.cert_key_pem.as_deref(), Some(key_pem));

    // cert_key_pem without cert_pem → 400.
    let bad = format!(
        r#"{{"id":"t1","name":"T","domain":"acme.com","auth_url":"https://auth.acme.com/v","cert_key":null,"cert_file":null,"enabled":true,"created_at":"","updated_at":"","cert_key_pem":{key_pem:?}}}"#,
        key_pem = key_pem
    );
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/tenants/t1",
        Some(TOKEN),
        Some(&bad),
    )
    .await;
    assert_eq!(r.status(), 400);

    // Explicit clear: cert_pem "" → cert removed.
    let clear = r#"{"id":"t1","name":"T","domain":"acme.com","auth_url":"https://auth.acme.com/v","cert_key":null,"cert_file":null,"enabled":true,"created_at":"","updated_at":"","cert_pem":"","cert_key_pem":null}"#;
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/tenants/t1",
        Some(TOKEN),
        Some(clear),
    )
    .await;
    assert_eq!(r.status(), 200);
    let got = hydra_server::db::get_tenant_cert(state.db(), &kp, "t1")
        .await
        .expect("get cert")
        .expect("cert row present");
    assert_eq!(got.cert_pem, None, "empty cert_pem clears the cert");
    assert_eq!(got.cert_key_pem, None);
}

/// Regression (admin UI create-tenant): the UI sends blank optional
/// `cert_file`/`cert_key` as `""` (empty string), which was treated as a
/// real path — the tenant was INSERTED first, then the legacy-path conversion
/// failed with 400 `cert_file_unreadable`, so the UI reported failure while
/// the tenant was actually created. Fix: empty strings are not paths (stored
/// as NULL), and certificate inputs are validated BEFORE the row is written,
/// so a 4xx can never leave a "reported as failed but created" tenant behind.
#[tokio::test]
async fn tenant_create_ui_payload_no_zombie_on_400() {
    let state = admin_state().await;
    let port = start_admin(state.clone());

    // 1. Exact admin-UI payload: blank legacy paths arrive as "" (not null).
    let ui = r#"{"id":"t1","name":"T","domain":"acme.com","auth_url":"https://auth.acme.com/v","cert_file":"","cert_key":"","cert_pem":null,"cert_key_pem":null,"enabled":true,"created_at":"","updated_at":""}"#;
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/tenants",
        Some(TOKEN),
        Some(ui),
    )
    .await;
    assert_eq!(
        r.status(),
        201,
        "blank cert fields must not fail the create"
    );

    // The stored row normalizes "" to NULL (empty string is not a path).
    let list = repo::list_tenants(state.db()).await.expect("list");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].cert_file, None, "\"\" stored as NULL");
    assert_eq!(list[0].cert_key, None, "\"\" stored as NULL");

    // 2. cert_pem without cert_key_pem → 400 BEFORE the row is written.
    let bad = r#"{"id":"t2","name":"T2","domain":"acme2.com","auth_url":"https://auth.acme.com/v","cert_key":null,"cert_file":null,"enabled":true,"created_at":"","updated_at":"","cert_pem":"-----BEGIN CERTIFICATE-----\nX\n-----END CERTIFICATE-----"}"#;
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/tenants",
        Some(TOKEN),
        Some(bad),
    )
    .await;
    assert_eq!(r.status(), 400, "missing cert_key_pem → 400");
    let list = repo::list_tenants(state.db()).await.expect("list");
    assert_eq!(
        list.len(),
        1,
        "the 400 must NOT leave a partially-created tenant behind"
    );

    // 3. Non-empty but unreadable legacy path → 400 BEFORE the row is written.
    let bad_path = r#"{"id":"t3","name":"T3","domain":"acme3.com","auth_url":"https://auth.acme.com/v","cert_file":"/nonexistent/cert.pem","cert_key":"/nonexistent/key.pem","cert_pem":null,"cert_key_pem":null,"enabled":true,"created_at":"","updated_at":""}"#;
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/tenants",
        Some(TOKEN),
        Some(bad_path),
    )
    .await;
    assert_eq!(r.status(), 400, "unreadable legacy path → 400");
    let list = repo::list_tenants(state.db()).await.expect("list");
    assert_eq!(
        list.len(),
        1,
        "the 400 must NOT leave a partially-created tenant behind"
    );

    // 4. Valid legacy paths still convert (PEM content stored, sealed).
    let dir = std::env::temp_dir();
    let cert_path = dir.join(format!("hydra-test-cert-{}.pem", std::process::id()));
    let key_path = dir.join(format!("hydra-test-key-{}.pem", std::process::id()));
    std::fs::write(
        &cert_path,
        "-----BEGIN CERTIFICATE-----\nLEGCERT\n-----END CERTIFICATE-----\n",
    )
    .unwrap();
    std::fs::write(
        &key_path,
        "-----BEGIN PRIVATE KEY-----\nLEGKEY\n-----END PRIVATE KEY-----\n",
    )
    .unwrap();
    let legacy = format!(
        r#"{{"id":"t4","name":"T4","domain":"acme4.com","auth_url":"https://auth.acme.com/v","cert_file":{:?},"cert_key":{:?},"cert_pem":null,"cert_key_pem":null,"enabled":true,"created_at":"","updated_at":""}}"#,
        cert_path.to_string_lossy(),
        key_path.to_string_lossy()
    );
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/tenants",
        Some(TOKEN),
        Some(&legacy),
    )
    .await;
    assert_eq!(r.status(), 201, "readable legacy paths convert on create");
    let kp = StaticKeyProvider::new([1u8; 32], 1);
    let got = repo::get_tenant_cert(state.db(), &kp, "t4")
        .await
        .expect("get cert")
        .expect("cert present");
    assert!(got.cert_pem.as_deref().unwrap_or("").contains("LEGCERT"));
    assert!(got.cert_key_pem.as_deref().unwrap_or("").contains("LEGKEY"));
    let _ = std::fs::remove_file(&cert_path);
    let _ = std::fs::remove_file(&key_path);
}

/// Edge data-plane admin (cluster P0b): pool-less state serving ONLY the
/// probe endpoints (`/metrics` `/healthz` `/readyz`); no admin UI, no CRUD —
/// everything else is 404, even with a valid admin token.
#[tokio::test]
async fn edge_admin_probes_only() {
    let key_provider: Arc<dyn KeyProvider> = Arc::new(StaticKeyProvider::new([1u8; 32], 1));
    let store = ConfigStore::from_snapshot(
        hydra_core::config::ConfigData::default(),
        key_provider.clone(),
    );
    let auth = Arc::new(
        HttpAuthChecker::new(
            AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
            AuthConfig::default(),
        )
        .expect("HttpAuthChecker"),
    );
    let breaker = Arc::new(CircuitBreaker::new(BreakerConfig::new(2)));
    let state = Arc::new(AdminState::new(
        None, // edge: no local SQLite
        store,
        auth,
        breaker,
        key_provider,
        Some(TOKEN.to_string()),
        None,
        hydra_server::proxy::admission::AdmissionControl::new(),
        true, // edge_mode
        None, // no cluster token in tests
        None, // no leader election in tests
    ));
    let port = start_admin(state);

    // Probe endpoints are token-free.
    let r = req(port, reqwest::Method::GET, "/healthz", None, None).await;
    assert_eq!(r.status(), 200);
    let r = req(port, reqwest::Method::GET, "/metrics", None, None).await;
    assert_eq!(r.status(), 200);
    let r = req(port, reqwest::Method::GET, "/readyz", None, None).await;
    assert_eq!(r.status(), 200);

    // Admin API + UI are gone, even with the token.
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/tenants",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 404, "edge has no CRUD GET");
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/providers",
        Some(TOKEN),
        Some(r#"{"id":"p1","key":"openai","name":"O","endpoint":"https://api.openai.com","weight":1,"created_at":"","updated_at":""}"#),
    )
    .await;
    assert_eq!(r.status(), 404, "edge has no CRUD POST");
    let r = req(port, reqwest::Method::GET, "/admin/", None, None).await;
    assert_eq!(r.status(), 404, "edge has no admin UI");
    // The read-only model-catalog endpoint is admin API too ⇒ 404 on edge
    // (edge short-circuits BEFORE the token gate / router).
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/tenants/t1/models",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 404, "edge has no tenant model catalog");
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
        state.db(),
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
        .set("t1", "sk-aaa", true, Duration::from_secs(300))
        .await;
    state
        .auth
        .cache()
        .set("t1", "sk-bbb", true, Duration::from_secs(300))
        .await;
    state
        .auth
        .cache()
        .set("t2", "sk-ccc", true, Duration::from_secs(300))
        .await;
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
        Some(pool),
        store,
        auth,
        breaker,
        key_provider,
        Some(TOKEN.to_string()),
        None,
        admission,
        false,
        None, // no cluster token in tests
        None, // no leader election in tests
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

// ===========================================================================
// §2.10 — tenant model catalog (design-tenant-model-catalog §2.3, P1):
// GET /api/v1/tenants/{tenant_id}/models — read-only config-full-set view.
// ===========================================================================

const CAT_NOW: &str = "2026-01-01 00:00:00";

/// Direct-repo provider insert (fixture row: weight as given).
async fn cat_seed_provider(state: &AdminState, id: &str, key: &str, weight: i32) {
    repo::insert_provider(
        state.db(),
        &hydra_core::model::Provider {
            id: id.into(),
            key: key.into(),
            name: key.into(),
            endpoint: format!("https://api.{key}.example.com"),
            weight,
            created_at: CAT_NOW.into(),
            updated_at: CAT_NOW.into(),
            max_concurrency: None,
            max_queue_depth: None,
            queue_wait_timeout_ms: None,
        },
    )
    .await
    .expect("insert provider");
}

/// Direct-repo provider_model insert.
async fn cat_seed_model(state: &AdminState, id: &str, key: &str, provider_id: &str, status: i32) {
    repo::insert_provider_model(
        state.db(),
        &hydra_core::model::ProviderModel {
            id: id.into(),
            key: key.into(),
            name: key.into(),
            provider_id: provider_id.into(),
            status,
        },
    )
    .await
    .expect("insert provider_model");
}

/// Direct-repo provider_key insert (plaintext sealed at the repo boundary).
async fn cat_seed_key(state: &AdminState, id: &str, provider_id: &str) {
    repo::insert_provider_key(
        state.db(),
        state.key_provider.as_ref(),
        &hydra_core::model::ProviderKey {
            id: id.into(),
            provider_id: provider_id.into(),
            api_key: format!("sk-{provider_id}"),
            created_at: CAT_NOW.into(),
        },
    )
    .await
    .expect("insert provider_key");
}

/// Direct-repo tenant insert (domain lowercased by the loader).
async fn cat_seed_tenant(state: &AdminState, id: &str, domain: &str) {
    repo::insert_tenant(
        state.db(),
        &hydra_core::model::Tenant {
            id: id.into(),
            name: id.into(),
            domain: domain.into(),
            auth_url: "https://auth.example.com/v".into(),
            cert_key: None,
            cert_file: None,
            enabled: true,
            created_at: CAT_NOW.into(),
            updated_at: CAT_NOW.into(),
        },
    )
    .await
    .expect("insert tenant");
}

async fn cat_seed_tenant_provider(
    state: &AdminState,
    id: &str,
    tenant_id: &str,
    provider_id: &str,
) {
    repo::insert_tenant_provider(
        state.db(),
        &hydra_core::model::TenantProvider {
            id: id.into(),
            tenant_id: tenant_id.into(),
            provider_id: provider_id.into(),
        },
    )
    .await
    .expect("insert tenant_provider");
}

async fn cat_seed_tenant_model(state: &AdminState, id: &str, tenant_id: &str, model_key: &str) {
    repo::insert_tenant_model(
        state.db(),
        &hydra_core::model::TenantModel {
            id: id.into(),
            tenant_id: tenant_id.into(),
            model_key: model_key.into(),
        },
    )
    .await
    .expect("insert tenant_model");
}

/// Fetch one model entry (by model key) from the catalog JSON.
fn cat_entry<'a>(body: &'a serde_json::Value, model: &str) -> &'a serde_json::Value {
    body["models"]
        .as_array()
        .expect("models array")
        .iter()
        .find(|e| e["model"] == model)
        .unwrap_or_else(|| panic!("model {model} missing from catalog"))
}

#[tokio::test]
async fn tenant_model_catalog_config_view() {
    let state = admin_state().await;

    // Providers: pA live · pB breaker-dead below · pC weight=0 (soft-disabled) ·
    // pD keyless. All four are authorised to tenant t1.
    cat_seed_provider(&state, "pA", "openai", 1).await;
    cat_seed_provider(&state, "pB", "anthropic", 1).await;
    cat_seed_provider(&state, "pC", "azure", 0).await;
    cat_seed_provider(&state, "pD", "local", 1).await;

    // Tenant t1 (catalog target) + t2 (exists, no grants — control).
    cat_seed_tenant(&state, "t1", "acme.test").await;
    cat_seed_tenant(&state, "t2", "other.test").await;

    // Models (all status=1 except `offline`): `union` is served by pA+pB+pD,
    // `echo` is served but NOT whitelisted, `offline` is whitelisted but the
    // loader excludes status!=1 rows from models_by_key.
    cat_seed_model(&state, "m1", "alpha", "pA", 1).await;
    cat_seed_model(&state, "m2", "beta", "pB", 1).await;
    cat_seed_model(&state, "m3", "gamma", "pC", 1).await;
    cat_seed_model(&state, "m4", "delta", "pD", 1).await;
    cat_seed_model(&state, "m5", "union", "pA", 1).await;
    cat_seed_model(&state, "m6", "union", "pB", 1).await;
    cat_seed_model(&state, "m7", "union", "pD", 1).await;
    cat_seed_model(&state, "m8", "echo", "pA", 1).await;
    cat_seed_model(&state, "m9", "offline", "pA", 0).await;

    // tenant_providers: t1 → all four providers.
    for pid in ["pA", "pB", "pC", "pD"] {
        cat_seed_tenant_provider(&state, &format!("tp-{pid}"), "t1", pid).await;
    }
    // tenant_models whitelist for t1 = SUBSET: alpha/beta/gamma/delta/union +
    // `offline` (which can never appear — no status==1 row). `echo` is served
    // but outside the whitelist ⇒ must be filtered.
    for key in ["alpha", "beta", "gamma", "delta", "union", "offline"] {
        cat_seed_tenant_model(&state, &format!("tm-{key}"), "t1", key).await;
    }

    // provider_keys: pA, pB and pC have keys; pD is keyless.
    cat_seed_key(&state, "k-a", "pA").await;
    cat_seed_key(&state, "k-b", "pB").await;
    cat_seed_key(&state, "k-c", "pC").await;

    // Trip the breaker for pB (threshold=2 in the fixture).
    state.breaker.on_failure("pB");
    state.breaker.on_failure("pB");
    assert!(state.breaker.is_dead("pB"));

    // Publish the seeded rows into the live snapshot (repo writes bypass the
    // store; the admin endpoints reload on write, but direct inserts don't).
    state.store.reload_all().await.expect("reload_all");

    let port = start_admin(state);
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/tenants/t1/models",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 200);
    let body: serde_json::Value = r.json().await.expect("json");
    assert_eq!(body["tenant_id"], "t1");

    let models = body["models"].as_array().expect("models array");
    // Deterministic ordering: model ascending.
    let keys: Vec<&str> = models
        .iter()
        .map(|m| m["model"].as_str().unwrap())
        .collect();
    assert_eq!(keys, vec!["alpha", "beta", "delta", "gamma", "union"]);

    // alpha — served by live pA only ⇒ online.
    let e = cat_entry(&body, "alpha");
    assert_eq!(e["providers"].as_array().unwrap().len(), 1);
    assert_eq!(e["providers"][0]["provider_id"], "pA");
    assert_eq!(e["providers"][0]["online"], true);

    // beta — breaker-dead pB: still LISTED (config view) but online=false.
    let e = cat_entry(&body, "beta");
    assert_eq!(e["providers"][0]["provider_id"], "pB");
    assert_eq!(e["providers"][0]["online"], false);

    // gamma — weight=0 provider: listed, online=false.
    let e = cat_entry(&body, "gamma");
    assert_eq!(e["providers"][0]["provider_id"], "pC");
    assert_eq!(e["providers"][0]["online"], false);

    // delta — keyless provider: listed, online=false.
    let e = cat_entry(&body, "delta");
    assert_eq!(e["providers"][0]["provider_id"], "pD");
    assert_eq!(e["providers"][0]["online"], false);

    // union — cross-provider union incl. unroutable members, providers sorted.
    let e = cat_entry(&body, "union");
    let provs = e["providers"].as_array().expect("providers array");
    let ids: Vec<&str> = provs
        .iter()
        .map(|p| p["provider_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec!["pA", "pB", "pD"],
        "providers ascending, union kept"
    );
    assert_eq!(provs[0]["online"], true); // pA live
    assert_eq!(provs[1]["online"], false); // pB dead
    assert_eq!(provs[2]["online"], false); // pD keyless

    // Whitelist filtering: `echo` is served by an authorised provider but not
    // whitelisted ⇒ absent. `offline` (status=0) never enters models_by_key ⇒
    // absent even though whitelisted.
    assert!(body["models"]
        .as_array()
        .unwrap()
        .iter()
        .all(|m| m["model"] != "echo"));
    assert!(body["models"]
        .as_array()
        .unwrap()
        .iter()
        .all(|m| m["model"] != "offline"));
}

/// Unknown tenant ⇒ 404 (existence decided by scanning tenants_by_domain, NOT
/// by the presence of a tenant_providers entry).
#[tokio::test]
async fn tenant_model_catalog_unknown_tenant_404() {
    let state = admin_state().await;
    let port = start_admin(state);
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/tenants/no-such-tenant/models",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 404);
    let body: serde_json::Value = r.json().await.expect("json");
    assert_eq!(body["error"]["code"], "not_found");
}

/// A tenant that EXISTS but has no tenant_providers entry is a valid tenant:
/// 200 with an empty catalog (catalog semantics; the chat path would 403).
#[tokio::test]
async fn tenant_model_catalog_existing_tenant_no_grants_is_200_empty() {
    let state = admin_state().await;
    // One provider serves a model, but tenant t1 has no grants.
    cat_seed_provider(&state, "pA", "openai", 1).await;
    cat_seed_model(&state, "m1", "alpha", "pA", 1).await;
    cat_seed_tenant(&state, "t1", "acme.test").await;
    state.store.reload_all().await.expect("reload_all");

    let port = start_admin(state);
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/tenants/t1/models",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 200);
    let body: serde_json::Value = r.json().await.expect("json");
    assert_eq!(body["tenant_id"], "t1");
    assert_eq!(body["models"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn tenant_model_catalog_requires_admin_token() {
    let state = admin_state().await;
    let port = start_admin(state);
    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/tenants/t1/models",
        None,
        None,
    )
    .await;
    assert_eq!(r.status(), 401);
}

/// Existence-guard regression at the handler: a snapshot whose models_by_key
/// references a provider MISSING from cfg.providers (an orphan row — reachable
/// at load, config::validate only Warns) must be silently dropped via
/// cfg.providers.get(P) — never a bare index panic. Built snapshot-fed because
/// the DB FK + ON DELETE CASCADE cannot produce orphans (design §2.3 note).
#[tokio::test]
async fn tenant_model_catalog_orphan_provider_row_dropped() {
    use std::collections::{HashMap, HashSet};
    let mut cfg = hydra_core::config::ConfigData::default();
    cfg.tenants_by_domain.insert(
        "acme.test".into(),
        hydra_core::model::Tenant {
            id: "t1".into(),
            name: "T1".into(),
            domain: "acme.test".into(),
            auth_url: "https://auth.example.com/v".into(),
            cert_key: None,
            cert_file: None,
            enabled: true,
            created_at: CAT_NOW.into(),
            updated_at: CAT_NOW.into(),
        },
    );
    let mut tenant_providers: HashMap<String, HashSet<String>> = HashMap::new();
    tenant_providers.insert(
        "t1".into(),
        ["ghost", "real"].iter().map(|s| s.to_string()).collect(),
    );
    cfg.tenant_providers = tenant_providers;
    let mut tenant_models: HashMap<String, HashSet<String>> = HashMap::new();
    tenant_models.insert(
        "t1".into(),
        ["ghost-model", "real-model"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
    );
    cfg.tenant_models = tenant_models;
    cfg.providers.insert(
        "real".into(),
        hydra_core::model::Provider {
            id: "real".into(),
            key: "real".into(),
            name: "Real".into(),
            endpoint: "https://api.real.example.com".into(),
            weight: 1,
            created_at: CAT_NOW.into(),
            updated_at: CAT_NOW.into(),
            max_concurrency: None,
            max_queue_depth: None,
            queue_wait_timeout_ms: None,
        },
    );
    cfg.provider_keys
        .insert("real".into(), vec!["sk-real".into()]);
    cfg.models_by_key.insert(
        "ghost-model".into(),
        vec![hydra_core::config::ModelProvider {
            provider_id: "ghost".into(),
            weight: 0,
        }],
    );
    cfg.models_by_key.insert(
        "real-model".into(),
        vec![hydra_core::config::ModelProvider {
            provider_id: "real".into(),
            weight: 1,
        }],
    );

    let key_provider: Arc<dyn KeyProvider> = Arc::new(StaticKeyProvider::new([1u8; 32], 1));
    let store = ConfigStore::from_snapshot(cfg, key_provider.clone());
    let auth = Arc::new(
        HttpAuthChecker::new(
            AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
            AuthConfig::default(),
        )
        .expect("HttpAuthChecker"),
    );
    let breaker = Arc::new(CircuitBreaker::new(BreakerConfig::new(2)));
    let state = Arc::new(AdminState::new(
        None,
        store,
        auth,
        breaker,
        key_provider,
        Some(TOKEN.to_string()),
        None,
        hydra_server::proxy::admission::AdmissionControl::new(),
        false,
        None,
        None,
    ));
    let port = start_admin(state);

    let r = req(
        port,
        reqwest::Method::GET,
        "/api/v1/tenants/t1/models",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 200, "orphan row must not panic the handler");
    let body: serde_json::Value = r.json().await.expect("json");
    let models = body["models"].as_array().expect("models array");
    assert_eq!(models.len(), 1, "only the existing-provider model survives");
    assert_eq!(models[0]["model"], "real-model");
    assert_eq!(models[0]["providers"][0]["provider_id"], "real");
    assert_eq!(models[0]["providers"][0]["online"], true);
}
