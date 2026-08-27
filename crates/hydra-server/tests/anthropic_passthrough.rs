//! Format-homogeneous Anthropic pass-through integration tests.
//!
//! Proves the client request path selects the usage-parser family end-to-end:
//! - `POST /v1/messages`   → Anthropic usage schema (input_tokens /
//!   output_tokens / cache_read_input_tokens) — `ProviderKind::Anthropic`.
//! - `POST /v1/chat/completions` → Generic (OpenAI-compatible) schema —
//!   `ProviderKind::Generic` (ZERO behaviour change, the safe default).
//!
//! Both tests send a request through a real Pingora proxy backed by a wiremock
//! mock upstream and assert the recorded `UsageRecord` tokens. The usage
//! recording is the single functional difference between the two paths: if the
//! wrong scanner kind were selected, `cache_hit_tokens` (Anthropic-only field
//! `cache_read_input_tokens`) would be `None` under Generic, and
//! `tokens_in`/`tokens_out` would fail to parse under the wrong
//! schema.

mod common;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use hydra_core::breaker::BreakerConfig;
use hydra_core::model::{
    LimitRole, Provider, ProviderKey, ProviderModel, Tenant, TenantModel, TenantProvider,
    UsageRecord,
};
use hydra_server::crypto::{KeyProvider, StaticKeyProvider};
use hydra_server::db as repo;
use hydra_server::http::{AuthCache, AuthConfig, HttpAuthChecker};
use hydra_server::proxy::breaker_wrap::CircuitBreaker;
use hydra_server::proxy::config::ProxyConfig;
use hydra_server::proxy::limiter::RateLimiter;
use hydra_server::proxy::{AppState, HydraProxy};
use hydra_server::store::ConfigStore;
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const NOW: &str = "2026-01-01 00:00:00";

/// Seed one tenant + one provider + one model (`model_key`) + one key. The
/// provider endpoint is pointed at `upstream_endpoint`.
async fn seed_routed(
    pool: &sqlx::SqlitePool,
    auth_url: &str,
    upstream_endpoint: &str,
    model_key: &str,
) {
    repo::insert_provider(
        pool,
        &Provider {
            id: "p1".into(),
            key: "prov1".into(),
            name: "Provider1".into(),
            endpoint: upstream_endpoint.into(),
            weight: 1,
            created_at: NOW.into(),
            updated_at: NOW.into(),
            max_concurrency: None,
            max_queue_depth: None,
            queue_wait_timeout_ms: None,
        },
    )
    .await
    .expect("insert provider");
    repo::insert_provider_model(
        pool,
        &ProviderModel {
            id: "m1".into(),
            key: model_key.into(),
            name: model_key.into(),
            provider_id: "p1".into(),
            status: 1,
        },
    )
    .await
    .expect("insert provider_model");
    repo::insert_tenant(
        pool,
        &Tenant {
            id: "t1".into(),
            name: "t1".into(),
            domain: "localhost".into(),
            auth_url: auth_url.into(),
            cert_key: None,
            cert_file: None,
            enabled: true,
            created_at: NOW.into(),
            updated_at: NOW.into(),
        },
    )
    .await
    .expect("insert tenant");
    repo::insert_tenant_provider(
        pool,
        &TenantProvider {
            id: "tp1".into(),
            tenant_id: "t1".into(),
            provider_id: "p1".into(),
        },
    )
    .await
    .expect("insert tenant_provider");
    repo::insert_tenant_model(
        pool,
        &TenantModel {
            id: "tm1".into(),
            tenant_id: "t1".into(),
            model_key: model_key.into(),
        },
    )
    .await
    .expect("insert tenant_model");
    repo::insert_provider_key(
        pool,
        &StaticKeyProvider::new([1u8; 32], 1),
        &ProviderKey {
            id: "pk1".into(),
            provider_id: "p1".into(),
            api_key: "sk-upstream-secret".into(),
            created_at: NOW.into(),
        },
    )
    .await
    .expect("insert provider_key");
    repo::insert_limit_role(
        pool,
        &LimitRole {
            id: "default".into(),
            name: "default".into(),
            matching_key: None,
            matching_model: None,
            matching_tenant: Some("t1".into()),
            matching_provider: None,
            limit_count: Some(1000),
            limit_token: None,
            window: "m".into(),
            enabled: true,
            created_at: NOW.into(),
        },
    )
    .await
    .expect("insert limit_role");
}

/// Bind an ephemeral port, return it, then release the socket so Pingora can
/// rebind. (TOCTOU window is negligible in test environments.)
fn ephemeral_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    listener.local_addr().expect("local_addr").port()
}

/// Start a Pingora proxy service on an ephemeral port, return the URL root.
fn start_proxy(state: Arc<AppState>) -> String {
    let port = ephemeral_port();
    let listen_addr = format!("127.0.0.1:{port}");
    let app = HydraProxy::new(state);
    let mut server = Server::new(Some(Opt::default())).expect("Server::new");
    server.bootstrap();
    let mut proxy_service = pingora_proxy::http_proxy_service(&server.configuration, app);
    proxy_service.add_tcp(&listen_addr);
    server.add_service(proxy_service);
    std::thread::spawn(move || {
        server.run_forever();
    });
    format!("http://localhost:{port}")
}

fn test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .expect("reqwest client")
}

/// Retry the request until the proxy is ready (Pingora binds asynchronously),
/// returning the first successful response. `api_key_header` selects the auth
/// header used (`authorization` for OpenAI-style, `x-api-key` for Anthropic).
async fn send_until_ready(
    client: &reqwest::Client,
    url: &str,
    body: &str,
    api_key_header: &str,
) -> reqwest::Response {
    let mut last_err = None;
    for _ in 0..60 {
        match client
            .post(url)
            .header(api_key_header, "Bearer test-client-key")
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
        {
            Ok(r) => return r,
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }
    }
    panic!(
        "proxy never became ready: {}",
        last_err.map(|e| e.to_string()).unwrap_or_default()
    );
}

// ===========================================================================
// RecordingSink helper (captures usage records for verification)
// ===========================================================================

#[derive(Default)]
struct RecordingSink {
    inner: Arc<std::sync::Mutex<Vec<UsageRecord>>>,
}

impl hydra_server::sink::UsageSink for RecordingSink {
    fn record(&self, record: UsageRecord) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let store = self.inner.clone();
        Box::pin(async move {
            store.lock().unwrap().push(record);
        })
    }
}

impl RecordingSink {
    fn records(&self) -> Vec<UsageRecord> {
        self.inner.lock().unwrap().clone()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

/// **Test A — `/v1/messages` selects the Anthropic usage scanner.**
///
/// A non-streaming Anthropic Messages-API JSON response carries a single
/// complete `usage` object with Anthropic field names (`input_tokens`,
/// `output_tokens`, `cache_read_input_tokens`). The Anthropic scanner parses
/// these into the neutral tokens_in / cache_hit_tokens / tokens_out fields.
///
/// This PROVES the Anthropic scanner was selected: the Generic/OpenAI schema
/// would not parse `cache_read_input_tokens` (it is not an `OpenAiUsageFields`
/// member), so `cache_hit_tokens` would be `None` — the distinguishing assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn v1_messages_selects_anthropic_usage_scanner() {
    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "status": true })),
        )
        .mount(&auth_server)
        .await;

    let upstream = MockServer::start().await;
    // Non-streaming Anthropic Messages response with a single usage object.
    let anthropic_json = concat!(
        r#"{"id":"msg_1","type":"message","role":"assistant","#,
        r#""content":[{"type":"text","text":"Hi"}],"model":"claude-3-5-sonnet-test","#,
        r#""stop_reason":"end_turn","#,
        r#""usage":{"input_tokens":42,"output_tokens":13,"cache_read_input_tokens":7}}"#
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(anthropic_json.to_string()),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let pool = common::setup_pool().await;
    seed_routed(
        &pool,
        &format!("{}/auth", auth_server.uri()),
        &upstream.uri(),
        "claude-3-5-sonnet-test",
    )
    .await;

    let recording = Arc::new(RecordingSink::default());
    let key_provider: Arc<dyn KeyProvider> = Arc::new(StaticKeyProvider::new([1u8; 32], 1));
    let store = ConfigStore::load(pool.clone(), key_provider)
        .await
        .expect("ConfigStore::load");
    let auth = Arc::new(
        HttpAuthChecker::new(
            AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
            AuthConfig::default(),
        )
        .expect("HttpAuthChecker::new"),
    );
    let breaker = Arc::new(CircuitBreaker::new(BreakerConfig::new(5)));
    let limiter = Arc::new(RateLimiter::new());
    let state = Arc::new(AppState {
        store,
        auth,
        breaker,
        limiter,
        admission: hydra_server::proxy::admission::AdmissionControl::new(),
        sink: recording.clone(),
        proxy: ProxyConfig::default(),
    });
    let root = start_proxy(state);
    let url = format!("{root}/v1/messages");
    let client = test_client();
    let body = r#"{"model":"claude-3-5-sonnet-test","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#;

    let resp = send_until_ready(&client, &url, body, "x-api-key").await;
    assert_eq!(resp.status(), 200);
    let _ = resp.text().await;

    let records = recording.records();
    assert_eq!(records.len(), 1, "exactly one usage record expected");
    let r = &records[0];
    // Anthropic scanner: input_tokens → tokens_in, output_tokens → tokens_out,
    // cache_read_input_tokens → cache_hit_tokens. No derived total is stored.
    assert_eq!(r.tokens_in, Some(42), "input_tokens → tokens_in");
    assert_eq!(r.tokens_out, Some(13), "output_tokens → tokens_out");
    assert_eq!(
        r.cache_hit_tokens,
        Some(7),
        "cache_read_input_tokens → cache_hit_tokens (Anthropic-only; Generic would be None)"
    );
    assert_eq!(r.model_key, "claude-3-5-sonnet-test");
    assert_eq!(r.status_code, 200);
}

/// **Test B — `/v1/chat/completions` regression (Generic, unchanged).**
///
/// The OpenAI-compatible path must parse `prompt_tokens`/`completion_tokens`
/// into the neutral `tokens_in`/`tokens_out` exactly as before — ZERO
/// behaviour change for the default
/// path. Proves the `Generic` scanner is still selected for non-`/v1/messages`
/// routes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn v1_chat_completions_regression_generic_scanner() {
    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "status": true })),
        )
        .mount(&auth_server)
        .await;

    let upstream = MockServer::start().await;
    let openai_json = concat!(
        r#"{"id":"chatcmpl-1","object":"chat.completion","choices":[],"#,
        r#""usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(openai_json.to_string()),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let pool = common::setup_pool().await;
    seed_routed(
        &pool,
        &format!("{}/auth", auth_server.uri()),
        &upstream.uri(),
        "gpt-4",
    )
    .await;

    let recording = Arc::new(RecordingSink::default());
    let key_provider: Arc<dyn KeyProvider> = Arc::new(StaticKeyProvider::new([1u8; 32], 1));
    let store = ConfigStore::load(pool.clone(), key_provider)
        .await
        .expect("ConfigStore::load");
    let auth = Arc::new(
        HttpAuthChecker::new(
            AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
            AuthConfig::default(),
        )
        .expect("HttpAuthChecker::new"),
    );
    let breaker = Arc::new(CircuitBreaker::new(BreakerConfig::new(5)));
    let limiter = Arc::new(RateLimiter::new());
    let state = Arc::new(AppState {
        store,
        auth,
        breaker,
        limiter,
        admission: hydra_server::proxy::admission::AdmissionControl::new(),
        sink: recording.clone(),
        proxy: ProxyConfig::default(),
    });
    let root = start_proxy(state);
    let url = format!("{root}/v1/chat/completions");
    let client = test_client();
    let body = r#"{"model":"gpt-4","messages":[{"role":"user","content":"hi"}]}"#;

    let resp = send_until_ready(&client, &url, body, "authorization").await;
    assert_eq!(resp.status(), 200);
    let _ = resp.text().await;

    let records = recording.records();
    assert_eq!(records.len(), 1, "exactly one usage record expected");
    let r = &records[0];
    // Generic scanner: OpenAI-style fields parsed into neutral names.
    assert_eq!(r.tokens_in, Some(10));
    assert_eq!(r.tokens_out, Some(5));
    assert_eq!(r.cache_hit_tokens, None);
    assert_eq!(r.model_key, "gpt-4");
    assert_eq!(r.status_code, 200);
}
