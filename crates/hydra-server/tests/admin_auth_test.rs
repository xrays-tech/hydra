//! Admin auth-url probe endpoint — `POST /api/v1/tenants/auth/test` (design
//! §11.3). Boots the real `AdminService` (same harness as `admin_api.rs`)
//! and asserts the simulated-probe classification: a fake api-key MUST be
//! rejected, so 401/403 or an explicit denial flag in a 2xx body PASS; an
//! allow, 404/405, 422, 5xx or an unreachable URL FAIL.

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
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "test-admin-token";

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
    let mut svc = Service::new("admin-auth-test".to_string(), app);
    svc.add_tcp(&addr);
    server.add_service(svc);
    std::thread::spawn(move || server.run_forever());
    port
}

async fn test_auth(port: u16, auth_url: &str) -> serde_json::Value {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client");
    // Give the admin server a moment to bind.
    let url = format!("http://127.0.0.1:{port}/api/v1/tenants/auth/test");
    let mut resp = None;
    for _ in 0..50 {
        match client
            .post(&url)
            .header("authorization", format!("Bearer {TOKEN}"))
            .json(&serde_json::json!({ "auth_url": auth_url }))
            .send()
            .await
        {
            Ok(r) => {
                resp = Some(r);
                break;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    resp.expect("admin server never ready")
        .json()
        .await
        .expect("json")
}

/// Mount a mock auth service that answers `status` (with an optional body)
/// to POST /auth, then run the probe and return the JSON result.
async fn probe_with(mock_status: u16, body: Option<serde_json::Value>) -> serde_json::Value {
    let mock = MockServer::start().await;
    let mut tmpl = ResponseTemplate::new(mock_status);
    if let Some(b) = body {
        tmpl = tmpl.set_body_json(b);
    }
    Mock::given(method("POST"))
        .and(path("/auth"))
        .respond_with(tmpl)
        .expect(1)
        .mount(&mock)
        .await;
    let port = start_admin(admin_state().await);
    test_auth(port, &(mock.uri().to_string() + "/auth")).await
}

#[tokio::test]
async fn auth_url_401_denied_passes() {
    // Simulated key must be rejected → 401 is the EXPECTED, passing verdict.
    let r = probe_with(401, None).await;
    assert_eq!(r["ok"], true, "got {r}");
    assert_eq!(r["reachable"], true, "got {r}");
    assert_eq!(r["protocol_ok"], true, "got {r}");
    assert_eq!(r["verdict"], "denied", "got {r}");
    assert_eq!(r["status"], 401, "got {r}");
}

#[tokio::test]
async fn auth_url_403_denied_passes() {
    let r = probe_with(403, None).await;
    assert_eq!(r["ok"], true, "got {r}");
    assert_eq!(r["verdict"], "denied", "got {r}");
}

#[tokio::test]
async fn auth_url_2xx_status_false_denied_passes() {
    // Dogress-style: 200 + {"status":false} = explicit denial → PASS.
    let r = probe_with(
        200,
        Some(serde_json::json!({ "status": false, "reason": "invalid_key" })),
    )
    .await;
    assert_eq!(r["ok"], true, "got {r}");
    assert_eq!(r["verdict"], "denied", "got {r}");
    assert!(
        r["body_snippet"].as_str().unwrap_or("").contains("status"),
        "got {r}"
    );
}

#[tokio::test]
async fn auth_url_2xx_html_not_json_fails() {
    // 200 + non-JSON body (bare 200 = empty body here; an HTML login page
    // behaves identically) is NOT a valid verdict — the Test button must
    // report it as a failure (hydra now treats 2xx non-JSON as unavailable).
    let r = probe_with(200, None).await;
    assert_eq!(r["ok"], false, "got {r}");
    assert_eq!(r["verdict"], "not_json", "got {r}");
}

#[tokio::test]
async fn auth_url_2xx_allowed_fails() {
    // A fake key being ALLOWED means the URL is not the real auth endpoint.
    let r = probe_with(200, Some(serde_json::json!({ "status": true }))).await;
    assert_eq!(r["ok"], false, "got {r}");
    assert_eq!(r["verdict"], "allowed", "got {r}");
}

#[tokio::test]
async fn auth_url_not_found_fails() {
    let r = probe_with(404, None).await;
    assert_eq!(r["ok"], false, "got {r}");
    assert_eq!(r["verdict"], "not_found", "got {r}");
    assert_eq!(r["protocol_ok"], false, "got {r}");
}

#[tokio::test]
async fn auth_url_method_not_allowed_fails() {
    // POST rejected → the URL is a GET-only endpoint (wrong protocol).
    let r = probe_with(405, None).await;
    assert_eq!(r["ok"], false, "got {r}");
    assert_eq!(r["verdict"], "method_not_allowed", "got {r}");
    assert_eq!(r["protocol_ok"], false, "got {r}");
}

#[tokio::test]
async fn auth_url_422_fails() {
    let r = probe_with(422, None).await;
    assert_eq!(r["ok"], false, "got {r}");
    assert_eq!(r["verdict"], "unprocessable", "got {r}");
}

#[tokio::test]
async fn auth_url_server_error_fails() {
    let r = probe_with(500, None).await;
    assert_eq!(r["ok"], false, "got {r}");
    assert_eq!(r["verdict"], "server_error", "got {r}");
}

#[tokio::test]
async fn auth_url_unreachable_fails() {
    // A closed loopback port → connection refused → unreachable.
    let port = start_admin(admin_state().await);
    let dead = ephemeral_port();
    let r = test_auth(port, &format!("http://127.0.0.1:{dead}/auth")).await;
    assert_eq!(r["ok"], false, "got {r}");
    assert_eq!(r["reachable"], false, "got {r}");
    assert_eq!(r["verdict"], "unreachable", "got {r}");
}

#[tokio::test]
async fn auth_url_missing_is_400() {
    let port = start_admin(admin_state().await);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client");
    let mut resp = None;
    for _ in 0..50 {
        match client
            .post(format!("http://127.0.0.1:{port}/api/v1/tenants/auth/test"))
            .header("authorization", format!("Bearer {TOKEN}"))
            .json(&serde_json::json!({ "auth_url": "" }))
            .send()
            .await
        {
            Ok(r) => {
                resp = Some(r);
                break;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    let r = resp.expect("admin server never ready");
    assert_eq!(r.status(), 400);
    let body: serde_json::Value = r.json().await.expect("json");
    assert_eq!(body["error"]["code"], "missing_auth_url", "got {body}");
}

#[tokio::test]
async fn auth_url_requires_admin_token() {
    // The probe endpoint sits behind the admin-token gate (design §13.3).
    let port = start_admin(admin_state().await);
    let mock = MockServer::start().await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client");
    let mut status = 0;
    for _ in 0..50 {
        match client
            .post(format!("http://127.0.0.1:{port}/api/v1/tenants/auth/test"))
            .json(&serde_json::json!({ "auth_url": mock.uri() }))
            .send()
            .await
        {
            Ok(r) => {
                status = r.status().as_u16();
                break;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    assert_eq!(status, 401, "probe must be admin-token gated");
}
