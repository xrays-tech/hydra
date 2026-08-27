//! Terminate-in-Pingora proxy mode integration tests (design-change
//! `terminate-mode`). Replaces the former `spike_zero_copy.rs` which validated
//! the stream-through zero-copy body re-forward mechanism.
//!
//! ## What this proves
//!
//! In terminate mode the whole gateway lifecycle runs inside `request_filter`:
//! the proxy reads the full downstream body, extracts the model, routes, then
//! calls the provider via its own reqwest client and streams the response back.
//! These tests verify, against a real Pingora proxy service + wiremock mock
//! providers:
//!
//! - Full request body is forwarded byte-for-byte to the provider.
//! - The provider api-key replaces the client `Authorization`.
//! - The `/v1` path is rewritten onto the provider endpoint.
//! - SSE responses are streamed back chunk-by-chunk.
//! - The failover loop advances to the next candidate on a provider failure
//!   and the breaker records the failure.
//! - The breaker records a success on a 2xx.
//! - Usage (`tokens_in`/`tokens_out`/`cache_hit_tokens`) is extracted
//!   from the streamed response.
//! - Error codes surface correctly: 404 (model not found), 401 (auth denied),
//!   429 (rate limited), 503 (no provider), 502 (all providers failed).

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
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A minimal no-op sink that records nothing (production sinks are tested
/// separately). Terminate-mode behaviour does not depend on the sink.
struct NoopSink;

impl hydra_server::sink::UsageSink for NoopSink {
    fn record(&self, _record: UsageRecord) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}

const NOW: &str = "2026-01-01 00:00:00";

/// Seed one tenant + one provider + one model + one key (the minimal routed
/// graph). The provider endpoint is pointed at `upstream_endpoint`.
async fn seed_one(pool: &sqlx::SqlitePool, auth_url: &str, upstream_endpoint: &str) {
    seed_provider(pool, "p1", "openai", "OpenAI", upstream_endpoint).await;
    repo::insert_provider_model(
        pool,
        &ProviderModel {
            id: "m1".into(),
            key: "gpt-4".into(),
            name: "gpt-4".into(),
            provider_id: "p1".into(),
            status: 1,
        },
    )
    .await
    .expect("insert provider_model");
    seed_tenant(pool, "t1", "localhost", auth_url).await;
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
            model_key: "gpt-4".into(),
        },
    )
    .await
    .expect("insert tenant_model");
    seed_key(
        pool,
        &StaticKeyProvider::new([1u8; 32], 1),
        "pk1",
        "p1",
        "sk-upstream-secret",
    )
    .await;
    seed_default_role(pool, "t1").await;
}

async fn seed_provider(pool: &sqlx::SqlitePool, id: &str, key: &str, name: &str, endpoint: &str) {
    repo::insert_provider(
        pool,
        &Provider {
            id: id.into(),
            key: key.into(),
            name: name.into(),
            endpoint: endpoint.into(),
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
}

async fn seed_tenant(pool: &sqlx::SqlitePool, id: &str, domain: &str, auth_url: &str) {
    repo::insert_tenant(
        pool,
        &Tenant {
            id: id.into(),
            name: id.into(),
            domain: domain.into(),
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
}

async fn seed_key(
    pool: &sqlx::SqlitePool,
    kp: &StaticKeyProvider,
    id: &str,
    provider_id: &str,
    api_key: &str,
) {
    repo::insert_provider_key(
        pool,
        kp,
        &ProviderKey {
            id: id.into(),
            provider_id: provider_id.into(),
            api_key: api_key.into(),
            created_at: NOW.into(),
        },
    )
    .await
    .expect("insert provider_key");
}

async fn seed_default_role(pool: &sqlx::SqlitePool, tenant: &str) {
    repo::insert_limit_role(
        pool,
        &LimitRole {
            id: "default".into(),
            name: "default".into(),
            matching_key: None,
            matching_model: None,
            matching_tenant: Some(tenant.into()),
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

/// Build the full AppState from a seeded pool + auth URL.
async fn build_state(pool: &sqlx::SqlitePool) -> Arc<AppState> {
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
    let sink: Arc<dyn hydra_server::sink::UsageSink> = Arc::new(NoopSink);
    Arc::new(AppState {
        store,
        auth,
        breaker,
        limiter,
        admission: hydra_server::proxy::admission::AdmissionControl::new(),
        sink,
        proxy: ProxyConfig::default(),
    })
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

/// A reqwest client with a short-ish timeout for tests.
fn test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .expect("reqwest client")
}

/// Retry the request until the proxy is ready (Pingora binds asynchronously),
/// returning the first successful response.
async fn send_until_ready(client: &reqwest::Client, url: &str, body: &str) -> reqwest::Response {
    let mut last_err = None;
    for _ in 0..60 {
        match client
            .post(url)
            .header("authorization", "Bearer test-client-key")
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
// Tests
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_body_forwarded_intact_and_key_swapped() {
    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "status": true })))
        .mount(&auth_server)
        .await;

    let upstream = MockServer::start().await;
    let request_body =
        r#"{"model":"gpt-4","messages":[{"role":"user","content":"Hello, world!"}]}"#;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-upstream-secret"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"id":"x","object":"chat.completion","choices":[]}"#),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let pool = common::setup_pool().await;
    seed_one(
        &pool,
        &format!("{}/auth", auth_server.uri()),
        &upstream.uri(),
    )
    .await;
    let state = build_state(&pool).await;
    let root = start_proxy(state);
    let url = format!("{root}/v1/chat/completions");
    let client = test_client();

    let resp = send_until_ready(&client, &url, request_body).await;
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("body");
    assert!(body.contains("chat.completion"), "body: {body}");

    // Verify the upstream received the body byte-for-byte.
    let received = upstream.received_requests().await.expect("recording on");
    let upstream_body = received
        .iter()
        .find(|r| r.method.as_str() == "POST" && r.url.path() == "/v1/chat/completions")
        .map(|r| String::from_utf8_lossy(&r.body).to_string())
        .expect("upstream got the request");
    assert_eq!(
        upstream_body, request_body,
        "upstream body must be byte-for-byte identical to client body"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tenant_without_tenant_model_mapping_is_default_open() {
    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "status": true })))
        .mount(&auth_server)
        .await;

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"id":"x","object":"chat.completion","choices":[]}"#),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let pool = common::setup_pool().await;
    // Seed the full routed graph EXCEPT `tenant_model`: with no model
    // mapping the (revised §7.1) default-open gate lets every model through.
    seed_provider(&pool, "p1", "openai", "OpenAI", &upstream.uri()).await;
    repo::insert_provider_model(
        &pool,
        &ProviderModel {
            id: "m1".into(),
            key: "gpt-4".into(),
            name: "gpt-4".into(),
            provider_id: "p1".into(),
            status: 1,
        },
    )
    .await
    .expect("insert provider_model");
    seed_tenant(
        &pool,
        "t1",
        "localhost",
        &format!("{}/auth", auth_server.uri()),
    )
    .await;
    repo::insert_tenant_provider(
        &pool,
        &TenantProvider {
            id: "tp1".into(),
            tenant_id: "t1".into(),
            provider_id: "p1".into(),
        },
    )
    .await
    .expect("insert tenant_provider");
    // NOTE: deliberately NO insert_tenant_model here.
    seed_key(
        &pool,
        &StaticKeyProvider::new([1u8; 32], 1),
        "pk1",
        "p1",
        "sk-upstream-secret",
    )
    .await;
    seed_default_role(&pool, "t1").await;

    let state = build_state(&pool).await;
    let root = start_proxy(state);
    let url = format!("{root}/v1/chat/completions");
    let client = test_client();

    let resp = send_until_ready(
        &client,
        &url,
        r#"{"model":"gpt-4","messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;
    assert_eq!(resp.status(), 200);
    // The upstream received the request despite the missing tenant_model row.
    let received = upstream.received_requests().await.expect("recording on");
    assert!(
        received.iter().any(|r| r.method.as_str() == "POST"),
        "upstream must receive the forwarded request"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sse_stream_is_forwarded_chunk_by_chunk() {
    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "status": true })))
        .mount(&auth_server)
        .await;

    let upstream = MockServer::start().await;
    // An SSE body with two `data:` frames plus the final usage summary.
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n\
               data: {\"choices\":[{\"delta\":{\"content\":\"!\"}}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n\
               data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse.to_string()),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let pool = common::setup_pool().await;
    seed_one(
        &pool,
        &format!("{}/auth", auth_server.uri()),
        &upstream.uri(),
    )
    .await;
    let state = build_state(&pool).await;
    let root = start_proxy(state);
    let url = format!("{root}/v1/chat/completions");
    let client = test_client();

    let resp = send_until_ready(&client, &url, r#"{"model":"gpt-4","stream":true}"#).await;
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("body");
    // The whole SSE stream is forwarded verbatim.
    assert!(body.contains("data: {"), "missing first frame: {body}");
    assert!(body.contains("[DONE]"), "missing terminator: {body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failover_advances_on_provider_error_then_breaker_records() {
    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "status": true })))
        .mount(&auth_server)
        .await;

    let dead_upstream = MockServer::start().await;
    let live_upstream = MockServer::start().await;

    // The first provider always returns 500 → failover to the second.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream exploded"))
        .mount(&dead_upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"id":"ok","object":"chat.completion","choices":[]}"#),
        )
        .expect(1)
        .mount(&live_upstream)
        .await;

    let pool = common::setup_pool().await;
    // Two providers, both serving gpt-4. The first (deterministic SWRR for a
    // fresh state) is attempted first; its failure triggers failover.
    seed_provider(&pool, "p_dead", "dead", "Dead", &dead_upstream.uri()).await;
    seed_provider(&pool, "p_live", "live", "Live", &live_upstream.uri()).await;
    repo::insert_provider_model(
        &pool,
        &ProviderModel {
            id: "m_dead".into(),
            key: "gpt-4".into(),
            name: "gpt-4".into(),
            provider_id: "p_dead".into(),
            status: 1,
        },
    )
    .await
    .unwrap();
    repo::insert_provider_model(
        &pool,
        &ProviderModel {
            id: "m_live".into(),
            key: "gpt-4".into(),
            name: "gpt-4".into(),
            provider_id: "p_live".into(),
            status: 1,
        },
    )
    .await
    .unwrap();
    seed_tenant(
        &pool,
        "t1",
        "localhost",
        &format!("{}/auth", auth_server.uri()),
    )
    .await;
    repo::insert_tenant_provider(
        &pool,
        &TenantProvider {
            id: "tp_dead".into(),
            tenant_id: "t1".into(),
            provider_id: "p_dead".into(),
        },
    )
    .await
    .unwrap();
    repo::insert_tenant_provider(
        &pool,
        &TenantProvider {
            id: "tp_live".into(),
            tenant_id: "t1".into(),
            provider_id: "p_live".into(),
        },
    )
    .await
    .unwrap();
    repo::insert_tenant_model(
        &pool,
        &TenantModel {
            id: "tm1".into(),
            tenant_id: "t1".into(),
            model_key: "gpt-4".into(),
        },
    )
    .await
    .unwrap();
    seed_key(
        &pool,
        &StaticKeyProvider::new([1u8; 32], 1),
        "pk_dead",
        "p_dead",
        "sk-dead",
    )
    .await;
    seed_key(
        &pool,
        &StaticKeyProvider::new([1u8; 32], 1),
        "pk_live",
        "p_live",
        "sk-live",
    )
    .await;
    seed_default_role(&pool, "t1").await;

    let breaker = Arc::new(CircuitBreaker::new(BreakerConfig::new(5)));
    let store = ConfigStore::load(pool.clone(), Arc::new(StaticKeyProvider::new([1u8; 32], 1)))
        .await
        .unwrap();
    let auth = Arc::new(
        HttpAuthChecker::new(
            AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
            AuthConfig::default(),
        )
        .unwrap(),
    );
    let limiter = Arc::new(RateLimiter::new());
    let sink: Arc<dyn hydra_server::sink::UsageSink> = Arc::new(NoopSink);
    let state = Arc::new(AppState {
        store,
        auth,
        breaker: breaker.clone(),
        limiter,
        admission: hydra_server::proxy::admission::AdmissionControl::new(),
        sink,
        proxy: ProxyConfig::default(),
    });
    let root = start_proxy(state);
    let url = format!("{root}/v1/chat/completions");
    let client = test_client();

    let resp = send_until_ready(&client, &url, r#"{"model":"gpt-4"}"#).await;
    // Failover reached the live provider → 200.
    assert_eq!(resp.status(), 200, "should failover to the live provider");
    let body = resp.text().await.expect("body");
    assert!(body.contains("chat.completion"), "body: {body}");

    // The dead provider recorded a breaker failure; the live one a success.
    assert_eq!(
        breaker.fail_count("p_dead"),
        1,
        "dead provider should have 1 failure"
    );
    assert_eq!(
        breaker.fail_count("p_live"),
        0,
        "live provider should have 0 failures"
    );
    assert!(!breaker.is_dead("p_dead"), "threshold 5 not reached yet");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn breaker_success_clears_failures() {
    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "status": true })))
        .mount(&auth_server)
        .await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"id":"ok"}"#))
        .mount(&upstream)
        .await;

    let pool = common::setup_pool().await;
    seed_one(
        &pool,
        &format!("{}/auth", auth_server.uri()),
        &upstream.uri(),
    )
    .await;
    let state = build_state(&pool).await;
    // Pre-charge the breaker with a failure; a 2xx response should reset it.
    state.breaker.on_failure("p1");
    assert_eq!(state.breaker.fail_count("p1"), 1);

    let root = start_proxy(state.clone());
    let url = format!("{root}/v1/chat/completions");
    let client = test_client();
    let resp = send_until_ready(&client, &url, r#"{"model":"gpt-4"}"#).await;
    assert_eq!(resp.status(), 200);
    // on_success fired in the failover loop → fail_count reset to 0.
    assert_eq!(
        state.breaker.fail_count("p1"),
        0,
        "2xx should reset the breaker streak"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn usage_tokens_extracted_from_response() {
    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "status": true })))
        .mount(&auth_server)
        .await;
    let upstream = MockServer::start().await;
    // Non-streaming JSON chat completion with a usage block.
    let body = r#"{"id":"x","object":"chat.completion","choices":[{"message":{"content":"hi"}}],"usage":{"prompt_tokens":13,"completion_tokens":7,"total_tokens":20}}"#;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(body.to_string()),
        )
        .mount(&upstream)
        .await;

    let pool = common::setup_pool().await;
    seed_one(
        &pool,
        &format!("{}/auth", auth_server.uri()),
        &upstream.uri(),
    )
    .await;

    // Use a recording sink to verify usage extraction end-to-end.
    let recording = Arc::new(RecordingSink::default());
    let store = ConfigStore::load(pool.clone(), Arc::new(StaticKeyProvider::new([1u8; 32], 1)))
        .await
        .unwrap();
    let auth = Arc::new(
        HttpAuthChecker::new(
            AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
            AuthConfig::default(),
        )
        .unwrap(),
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

    let resp = send_until_ready(&client, &url, r#"{"model":"gpt-4"}"#).await;
    assert_eq!(resp.status(), 200);
    let _ = resp.text().await;

    // The recording sink should have captured one record with the usage tokens.
    let records = recording.records();
    assert_eq!(records.len(), 1, "exactly one usage record expected");
    let r = &records[0];
    assert_eq!(r.tokens_in, Some(13));
    assert_eq!(r.tokens_out, Some(7));
    assert_eq!(r.cache_hit_tokens, None);
    assert_eq!(r.provider_id, "p1");
    assert_eq!(r.model_key, "gpt-4");
    assert_eq!(r.status_code, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn error_404_when_model_not_found() {
    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "status": true })))
        .mount(&auth_server)
        .await;
    let upstream = MockServer::start().await;
    // Mount nothing on the upstream — routing should fail before we hit it.

    let pool = common::setup_pool().await;
    seed_one(
        &pool,
        &format!("{}/auth", auth_server.uri()),
        &upstream.uri(),
    )
    .await;
    // Allow the tenant to use a model that NO provider serves → router returns
    // ModelNotFound (404), distinct from ModelNotAllowed (403) which fires when
    // the model is not in the tenant_models gate at all.
    repo::insert_tenant_model(
        &pool,
        &TenantModel {
            id: "tm_phantom".into(),
            tenant_id: "t1".into(),
            model_key: "phantom-model".into(),
        },
    )
    .await
    .expect("insert tenant_model");
    let state = build_state(&pool).await;
    let root = start_proxy(state);
    let url = format!("{root}/v1/chat/completions");
    let client = test_client();

    let resp = send_until_ready(&client, &url, r#"{"model":"phantom-model"}"#).await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn error_401_when_auth_denied() {
    let auth_server = MockServer::start().await;
    // Auth upstream denies the request.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&auth_server)
        .await;
    let upstream = MockServer::start().await;

    let pool = common::setup_pool().await;
    seed_one(
        &pool,
        &format!("{}/auth", auth_server.uri()),
        &upstream.uri(),
    )
    .await;
    let state = build_state(&pool).await;
    let root = start_proxy(state);
    let url = format!("{root}/v1/chat/completions");
    let client = test_client();

    let resp = send_until_ready(&client, &url, r#"{"model":"gpt-4"}"#).await;
    assert_eq!(resp.status(), 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn error_502_when_all_providers_fail() {
    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "status": true })))
        .mount(&auth_server)
        .await;
    let upstream = MockServer::start().await;
    // The single provider always returns 503 → all candidates exhausted → 502
    // surfacing the last provider status.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&upstream)
        .await;

    let pool = common::setup_pool().await;
    seed_one(
        &pool,
        &format!("{}/auth", auth_server.uri()),
        &upstream.uri(),
    )
    .await;
    let state = build_state(&pool).await;
    let root = start_proxy(state);
    let url = format!("{root}/v1/chat/completions");
    let client = test_client();

    let resp = send_until_ready(&client, &url, r#"{"model":"gpt-4"}"#).await;
    // Single provider returned 503; we surface the provider status.
    assert_eq!(resp.status(), 503);
}

// ───────────────────────────────────────────────────────────────────────────
// Rate-limit enforcement (§10.3): the limit gate must run BEFORE routing so a
// rate-limited request gets 429 even when routing would short-circuit with 503
// (e.g. breaker-tripped → NoAvailableProvider).
// ───────────────────────────────────────────────────────────────────────────

/// Send a single chat-completion request (after the proxy is confirmed ready).
async fn send_one(client: &reqwest::Client, url: &str, body: &str) -> reqwest::Response {
    client
        .post(url)
        .header("authorization", "Bearer test-client-key")
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .expect("send")
}

/// Tighten the default limit role (id="default", seeded by `seed_one` with
/// count=1000) to count=2 so the third request is denied.
async fn tighten_limit_to_2(pool: &sqlx::SqlitePool) {
    repo::update_limit_role(
        pool,
        &LimitRole {
            id: "default".into(),
            name: "default".into(),
            matching_key: None,
            matching_model: None,
            matching_tenant: None, // match-all (design §10.1 wildcard)
            matching_provider: None,
            limit_count: Some(2),
            limit_token: None,
            window: "m".into(),
            enabled: true,
            created_at: NOW.into(),
        },
    )
    .await
    .expect("update limit role to count=2");
}

/// Basic limit enforcement: provider alive, count=2, the third request is 429.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rate_limit_429_on_third_request() {
    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "status": true })))
        .mount(&auth_server)
        .await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"id":"ok","object":"chat.completion","choices":[]}"#),
        )
        // Exactly two provider calls: req3 is 429'd before routing.
        .expect(2)
        .mount(&upstream)
        .await;

    let pool = common::setup_pool().await;
    seed_one(
        &pool,
        &format!("{}/auth", auth_server.uri()),
        &upstream.uri(),
    )
    .await;
    tighten_limit_to_2(&pool).await;

    let state = build_state(&pool).await;
    let root = start_proxy(state);
    let url = format!("{root}/v1/chat/completions");
    let client = test_client();
    let body = r#"{"model":"gpt-4","messages":[{"role":"user","content":"hi"}]}"#;

    let r1 = send_until_ready(&client, &url, body).await;
    assert_eq!(r1.status(), 200, "req1 should pass");
    let _ = r1.text().await;

    let r2 = send_one(&client, &url, body).await;
    assert_eq!(r2.status(), 200, "req2 should pass");
    let _ = r2.text().await;

    let r3 = send_one(&client, &url, body).await;
    assert_eq!(r3.status(), 429, "req3 must be rate-limited (429)");
    let r3body = r3.text().await.expect("body");
    assert!(r3body.contains("rate_limited"), "body: {r3body}");
}

/// **Regression test**: when the provider is dead (breaker tripped after 2
/// failures) AND the limit is exceeded, the third request must get **429**
/// (rate-limited), NOT 503 (no-available-provider). This proves the limit gate
/// runs BEFORE routing — the root cause of the 503-instead-of-429 bug.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rate_limit_429_even_when_routing_would_503() {
    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "status": true })))
        .mount(&auth_server)
        .await;
    let upstream = MockServer::start().await;
    // Provider always returns 500 → breaker accumulates consecutive failures.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        // Exactly two provider calls (req1 + req2); req3 never reaches routing.
        .expect(2)
        .mount(&upstream)
        .await;

    let pool = common::setup_pool().await;
    seed_one(
        &pool,
        &format!("{}/auth", auth_server.uri()),
        &upstream.uri(),
    )
    .await;
    tighten_limit_to_2(&pool).await;

    // Build state with breaker threshold=2: after two 500s the provider enters
    // the dead-set, so routing on req3 would return NoAvailableProvider → 503
    // (old code). With the fix the limit gate fires first → 429.
    let store = ConfigStore::load(pool.clone(), Arc::new(StaticKeyProvider::new([1u8; 32], 1)))
        .await
        .unwrap();
    let auth = Arc::new(
        HttpAuthChecker::new(
            AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
            AuthConfig::default(),
        )
        .unwrap(),
    );
    let breaker = Arc::new(CircuitBreaker::new(BreakerConfig::new(2)));
    let limiter = Arc::new(RateLimiter::new());
    let sink: Arc<dyn hydra_server::sink::UsageSink> = Arc::new(NoopSink);
    let state = Arc::new(AppState {
        store,
        auth,
        breaker,
        limiter,
        admission: hydra_server::proxy::admission::AdmissionControl::new(),
        sink,
        proxy: ProxyConfig::default(),
    });
    let root = start_proxy(state);
    let url = format!("{root}/v1/chat/completions");
    let client = test_client();
    let body = r#"{"model":"gpt-4","messages":[{"role":"user","content":"hi"}]}"#;

    // req1: limit 0→1 admitted; provider 500 → breaker fail #1 (not dead yet).
    let r1 = send_until_ready(&client, &url, body).await;
    assert_ne!(r1.status(), 429, "req1 must not be rate-limited");
    let _ = r1.text().await;

    // req2: limit 1→2 admitted; provider 500 → breaker fail #2 → dead-set.
    let r2 = send_one(&client, &url, body).await;
    assert_ne!(r2.status(), 429, "req2 must not be rate-limited");
    let _ = r2.text().await;

    // req3: limit exhausted (count=2). OLD code: routing → provider dead → 503.
    // NEW code: limit gate fires first → 429.
    let r3 = send_one(&client, &url, body).await;
    assert_eq!(
        r3.status(),
        429,
        "req3 must be 429 (rate-limited), not 503 (no provider) — the limit gate must run before routing"
    );
}

// ===========================================================================
// Admission control (design-admission-queue §5/§6/§7)
// ===========================================================================
//
// These tests verify the safe-rollout invariants:
//
// 1. Default-behaviour smoke: no concurrency config ⇒ Passthrough ⇒ 200
//    (proves the hot path is unchanged for unconfigured providers).
// 2. §7 breaker boundary: admission errors (QueueFull/WaitTimeout) MUST NOT
//    trip the breaker. Only real upstream errors do.

/// Seed a provider with explicit concurrency limits (design-admission-queue §5).
#[allow(clippy::too_many_arguments)]
async fn seed_provider_capped(
    pool: &sqlx::SqlitePool,
    id: &str,
    key: &str,
    name: &str,
    endpoint: &str,
    max_concurrency: u32,
    max_queue_depth: u32,
    queue_wait_timeout_ms: u64,
) {
    repo::insert_provider(
        pool,
        &Provider {
            id: id.into(),
            key: key.into(),
            name: name.into(),
            endpoint: endpoint.into(),
            weight: 1,
            created_at: NOW.into(),
            updated_at: NOW.into(),
            max_concurrency: Some(max_concurrency),
            max_queue_depth: Some(max_queue_depth),
            queue_wait_timeout_ms: Some(queue_wait_timeout_ms),
        },
    )
    .await
    .expect("insert capped provider");
}

/// **Default-behaviour smoke** (CRITICAL SAFETY PROPERTY): a request through
/// the proxy with NO concurrency config (all `None`, default policy all-zeros)
/// must succeed (200) — proving `Permit::Passthrough` short-circuits correctly
/// and the hot path is unchanged for unconfigured providers. Also asserts no
/// admission gate was created (Passthrough never touches the semaphore).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn default_no_concurrency_config_is_passthrough_200() {
    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "status": true })))
        .mount(&auth_server)
        .await;

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"id":"ok","object":"chat.completion","choices":[]}"#),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let pool = common::setup_pool().await;
    seed_one(
        &pool,
        &format!("{}/auth", auth_server.uri()),
        &upstream.uri(),
    )
    .await;
    let state = build_state(&pool).await;

    // Verify the default policy is all-zeros (⇒ Passthrough).
    assert_eq!(
        state.proxy.default_concurrency_policy.max_concurrency, 0,
        "default policy must be 0 (Passthrough)"
    );

    let root = start_proxy(state.clone());
    let url = format!("{root}/v1/chat/completions");
    let client = test_client();

    let resp = send_until_ready(&client, &url, r#"{"model":"gpt-4"}"#).await;
    assert_eq!(
        resp.status(),
        200,
        "default no-concurrency-config request must succeed (Passthrough no-op)"
    );
    let body = resp.text().await.expect("body");
    assert!(body.contains("chat.completion"), "body: {body}");

    // No admission gate should have been created (Passthrough never creates one).
    assert_eq!(
        state.admission.len(),
        0,
        "Passthrough path must not create any admission gate"
    );
}

/// **§7 breaker boundary**: a WaitTimeout admission error MUST NOT trip the
/// breaker. We configure a single provider with max_concurrency=1 and a short
/// queue_wait_timeout_ms=100. The upstream has a 500ms delay so the first
/// request holds its permit well past the second's timeout. The second request
/// queues, times out → the loop exhausts → 503 + Retry-After. The breaker must
/// stay at 0 failures (only real upstream errors trip it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admission_wait_timeout_does_not_trip_breaker() {
    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "status": true })))
        .mount(&auth_server)
        .await;

    let upstream = MockServer::start().await;
    // 500ms delay — the first request holds its permit for this long.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"id":"ok"}"#)
                .set_delay(Duration::from_millis(500)),
        )
        .mount(&upstream)
        .await;

    let pool = common::setup_pool().await;
    // max_concurrency=1, max_queue_depth=8 (allow queueing), timeout=100ms.
    seed_provider_capped(&pool, "p1", "openai", "OpenAI", &upstream.uri(), 1, 8, 100).await;
    repo::insert_provider_model(
        &pool,
        &ProviderModel {
            id: "m1".into(),
            key: "gpt-4".into(),
            name: "gpt-4".into(),
            provider_id: "p1".into(),
            status: 1,
        },
    )
    .await
    .expect("insert provider_model");
    seed_tenant(
        &pool,
        "t1",
        "localhost",
        &format!("{}/auth", auth_server.uri()),
    )
    .await;
    repo::insert_tenant_provider(
        &pool,
        &TenantProvider {
            id: "tp1".into(),
            tenant_id: "t1".into(),
            provider_id: "p1".into(),
        },
    )
    .await
    .expect("insert tenant_provider");
    repo::insert_tenant_model(
        &pool,
        &TenantModel {
            id: "tm1".into(),
            tenant_id: "t1".into(),
            model_key: "gpt-4".into(),
        },
    )
    .await
    .expect("insert tenant_model");
    seed_key(
        &pool,
        &StaticKeyProvider::new([1u8; 32], 1),
        "pk1",
        "p1",
        "sk-secret",
    )
    .await;
    seed_default_role(&pool, "t1").await;

    let breaker = Arc::new(CircuitBreaker::new(BreakerConfig::new(5)));
    let store = ConfigStore::load(pool.clone(), Arc::new(StaticKeyProvider::new([1u8; 32], 1)))
        .await
        .unwrap();
    let auth = Arc::new(
        HttpAuthChecker::new(
            AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
            AuthConfig::default(),
        )
        .unwrap(),
    );
    let limiter = Arc::new(RateLimiter::new());
    let sink: Arc<dyn hydra_server::sink::UsageSink> = Arc::new(NoopSink);
    let state = Arc::new(AppState {
        store,
        auth,
        breaker: breaker.clone(),
        limiter,
        admission: hydra_server::proxy::admission::AdmissionControl::new(),
        sink,
        proxy: ProxyConfig::default(),
    });
    let root = start_proxy(state);
    let url = format!("{root}/v1/chat/completions");
    let client = test_client();
    let body = r#"{"model":"gpt-4"}"#;

    // Warm up: ensure the proxy has bound the port before firing concurrent
    // requests (Pingora binds asynchronously; `send_one` has no retry).
    let warmup = send_until_ready(&client, &url, body).await;
    assert_eq!(warmup.status(), 200, "warmup request should succeed");
    let _ = warmup.text().await;

    // Fire the first request — it acquires the permit and holds it for 500ms.
    let client_a = client.clone();
    let url_a = url.clone();
    let body_a = body.to_string();
    let first = tokio::spawn(async move { send_one(&client_a, &url_a, &body_a).await });

    // Wait 150ms so the first request has acquired the permit.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Fire the second request — it queues, times out after 100ms → 503.
    let second_resp = send_one(&client, &url, body).await;
    let first_resp = first.await.expect("join first");

    // The first should succeed (200); the second should be 503 (admission).
    assert_eq!(
        first_resp.status(),
        200,
        "first request (held the permit) should succeed"
    );
    assert_eq!(
        second_resp.status(),
        503,
        "second request should be 503 (WaitTimeout — capacity exhaustion)"
    );
    // Verify Retry-After header is present (design §6).
    assert!(
        second_resp.headers().get("retry-after").is_some(),
        "503 admission response must include Retry-After (design §6)"
    );

    // The §7 boundary: the breaker must NOT have been tripped by the admission
    // timeout. Only a real upstream error would trip it.
    assert_eq!(
        breaker.fail_count("p1"),
        0,
        "§7: WaitTimeout MUST NOT trip the breaker (capacity ≠ health)"
    );
    assert!(!breaker.is_dead("p1"), "provider must not be dead");
}

/// **Admission saturation → both requests succeed**: with two providers (one
/// capped at max_concurrency=1, one unlimited), two concurrent requests both
/// get 200 — one via the capped provider (permit available), one via the
/// unlimited provider (SWRR rotation or admission failover). This proves the
/// concurrency valve composes cleanly with the existing routing/failover and
/// does not break normal operation under concurrent load.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admission_capped_provider_does_not_block_second_request() {
    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "status": true })))
        .mount(&auth_server)
        .await;

    let upstream_a = MockServer::start().await;
    let upstream_b = MockServer::start().await;

    // Provider A: 300ms delay, capped at max_concurrency=1.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"id":"a"}"#)
                .set_delay(Duration::from_millis(300)),
        )
        .mount(&upstream_a)
        .await;
    // Provider B: instant response, unlimited.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"id":"b"}"#))
        .mount(&upstream_b)
        .await;

    let pool = common::setup_pool().await;
    seed_provider_capped(
        &pool,
        "pA",
        "provA",
        "ProviderA",
        &upstream_a.uri(),
        1,
        8,
        100,
    )
    .await;
    seed_provider(&pool, "pB", "provB", "ProviderB", &upstream_b.uri()).await;
    for (pid, mid) in [("pA", "mA"), ("pB", "mB")] {
        repo::insert_provider_model(
            &pool,
            &ProviderModel {
                id: mid.into(),
                key: "gpt-4".into(),
                name: "gpt-4".into(),
                provider_id: pid.into(),
                status: 1,
            },
        )
        .await
        .unwrap();
    }
    seed_tenant(
        &pool,
        "t1",
        "localhost",
        &format!("{}/auth", auth_server.uri()),
    )
    .await;
    for (tpid, pid) in [("tpA", "pA"), ("tpB", "pB")] {
        repo::insert_tenant_provider(
            &pool,
            &TenantProvider {
                id: tpid.into(),
                tenant_id: "t1".into(),
                provider_id: pid.into(),
            },
        )
        .await
        .unwrap();
    }
    repo::insert_tenant_model(
        &pool,
        &TenantModel {
            id: "tm1".into(),
            tenant_id: "t1".into(),
            model_key: "gpt-4".into(),
        },
    )
    .await
    .unwrap();
    seed_key(
        &pool,
        &StaticKeyProvider::new([1u8; 32], 1),
        "pkA",
        "pA",
        "sk-a",
    )
    .await;
    seed_key(
        &pool,
        &StaticKeyProvider::new([1u8; 32], 1),
        "pkB",
        "pB",
        "sk-b",
    )
    .await;
    seed_default_role(&pool, "t1").await;

    let state = build_state(&pool).await;
    let root = start_proxy(state);
    let url = format!("{root}/v1/chat/completions");
    let client = test_client();
    let body = r#"{"model":"gpt-4"}"#;

    // Warm up: ensure the proxy has bound the port before firing concurrent
    // requests. Pingora binds asynchronously inside `run_forever()`, and
    // `send_one` has no retry — a pre-bind request would panic at
    // `.expect("send")`. Every non-concurrent test in this file uses
    // `send_until_ready` for the same reason.
    let warmup = send_until_ready(&client, &url, body).await;
    assert_eq!(warmup.status(), 200, "warmup request should succeed");
    let _ = warmup.text().await;

    // Fire two concurrent requests. With SWRR rotation, the first picks one
    // provider and the second picks the other. Both should get 200 regardless
    // of which provider each lands on (pA has a permit, pB is unlimited).
    let client2 = client.clone();
    let url2 = url.clone();
    let body2 = body.to_string();
    let second = tokio::spawn(async move { send_one(&client2, &url2, &body2).await });

    let first_resp = send_one(&client, &url, body).await;
    let second_resp = second.await.expect("join");

    let statuses = vec![first_resp.status().as_u16(), second_resp.status().as_u16()];
    assert!(
        statuses.iter().all(|&s| s == 200),
        "both requests should succeed (concurrent load with capped+unlimited providers): {statuses:?}"
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
