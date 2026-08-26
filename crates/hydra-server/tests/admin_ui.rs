//! §2.1 (wave-6) — embedded admin UI static-file serving (design §14).
//!
//! Boots the real `AdminService` on an ephemeral port (same harness as
//! `admin_api.rs`) and asserts the embedded UI is reachable WITHOUT the admin
//! token (so the browser can render the login prompt) while `/api/v1/*` stays
//! token-gated. Same-origin: UI and `/api` share one origin (T1.3).

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
    let mut svc = Service::new("admin-ui-test".to_string(), app);
    svc.add_tcp(&addr);
    server.add_service(svc);
    std::thread::spawn(move || server.run_forever());
    port
}

async fn req(port: u16, method: reqwest::Method, path: &str) -> reqwest::Response {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client");
    let url = format!("http://127.0.0.1:{port}{path}");
    for _ in 0..50 {
        match client.request(method.clone(), &url).send().await {
            Ok(r) => return r,
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    panic!("admin server never ready for {url}");
}

#[tokio::test]
async fn ui_index_served_without_token() {
    // T1.1 `ui_served_from_admin`: GET /admin/ returns index.html (200,
    // Content-Type text/html) WITHOUT an admin token.
    let port = start_admin(admin_state().await);
    let r = req(port, reqwest::Method::GET, "/admin/").await;
    assert_eq!(r.status(), 200);
    let ct = r.headers().get("content-type").unwrap().to_str().unwrap();
    assert!(ct.starts_with("text/html"), "content-type was {ct}");
    let body = r.text().await.unwrap();
    assert!(body.contains("<title>Hydra Admin</title>"));
    // The login overlay is present so the browser can prompt.
    assert!(body.contains("login-overlay"));
}

#[tokio::test]
async fn ui_index_served_at_admin_without_trailing_slash() {
    let port = start_admin(admin_state().await);
    let r = req(port, reqwest::Method::GET, "/admin").await;
    assert_eq!(r.status(), 200);
    let ct = r.headers().get("content-type").unwrap().to_str().unwrap();
    assert!(ct.starts_with("text/html"));
}

#[tokio::test]
async fn ui_index_html_alias() {
    let port = start_admin(admin_state().await);
    let r = req(port, reqwest::Method::GET, "/admin/index.html").await;
    assert_eq!(r.status(), 200);
    let ct = r.headers().get("content-type").unwrap().to_str().unwrap();
    assert!(ct.starts_with("text/html"));
}

#[tokio::test]
async fn ui_app_js_served_with_js_content_type() {
    // T1.2 `ui_assets_embedded`: the JS asset is in the binary, served with the
    // correct Content-Type.
    let port = start_admin(admin_state().await);
    let r = req(port, reqwest::Method::GET, "/admin/app.js").await;
    assert_eq!(r.status(), 200);
    let ct = r.headers().get("content-type").unwrap().to_str().unwrap();
    assert!(
        ct.starts_with("application/javascript"),
        "content-type was {ct}"
    );
    let body = r.text().await.unwrap();
    // Sentinel from the source so the embedded file is provably correct.
    assert!(body.contains("Hydra admin UI"));
    assert!(body.contains("/api/v1"));
}

#[tokio::test]
async fn ui_style_css_served_with_css_content_type() {
    let port = start_admin(admin_state().await);
    let r = req(port, reqwest::Method::GET, "/admin/style.css").await;
    assert_eq!(r.status(), 200);
    let ct = r.headers().get("content-type").unwrap().to_str().unwrap();
    assert!(ct.starts_with("text/css"), "content-type was {ct}");
    let body = r.text().await.unwrap();
    assert!(body.contains("--bg"));
}

#[tokio::test]
async fn ui_unknown_asset_returns_404() {
    let port = start_admin(admin_state().await);
    let r = req(port, reqwest::Method::GET, "/admin/nope.xyz").await;
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn ui_post_method_not_allowed_for_admin_static() {
    // Only GET serves the UI; a POST to /admin/ falls through to the token gate
    // → 401 (since no token). This keeps the admin static surface read-only.
    let port = start_admin(admin_state().await);
    let r = req(port, reqwest::Method::POST, "/admin/").await;
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn api_still_requires_token_when_ui_is_unauthenticated() {
    // T1.3 `ui_cors_same_origin`: UI loads without a token, but the same-origin
    // `/api/v1/health` still requires the bearer token (defence-in-depth).
    let port = start_admin(admin_state().await);
    let r = req(port, reqwest::Method::GET, "/api/v1/health").await;
    assert_eq!(r.status(), 401);

    // And the UI itself can be re-fetched any number of times without a token
    // (no rate-limit / token state on the static path).
    for _ in 0..3 {
        let r = req(port, reqwest::Method::GET, "/admin/app.js").await;
        assert_eq!(r.status(), 200);
    }
}
