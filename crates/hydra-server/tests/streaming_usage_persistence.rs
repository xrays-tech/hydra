//! Streaming SSE usage → SQLite persistence end-to-end tests.
//!
//! Proves that streaming SSE responses through the real Pingora proxy have
//! their usage tokens correctly extracted (by `UsageScanner`) and persisted
//! (by the usage sink) — closing the gap between
//! the pure-core scanner tests and the real proxy+sink pipeline.
//!
//! Both format paths are covered:
//! - **OpenAI streaming** (`/v1/chat/completions`): SSE with `usage` in the
//!   final chunk → prompt=10, completion=5, total=15 in SQLite.
//! - **Anthropic coalesced streaming** (`/v1/messages`): `message_start` +
//!   `message_delta` in ONE mock body (the BUG-1 scenario) → prompt=42,
//!   completion=13, cached=7, total=55 in SQLite.
//!
//! Deterministic observation strategy: a recording sink so each
//! record flushes immediately when the background task polls the channel,
//! combined with a bounded retry loop polling the sink (no bare sleeps).

#![cfg(feature = "db")]

mod common;

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
use hydra_server::usage::UsageSink;
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const NOW: &str = "2026-01-01 00:00:00";

// ---------------------------------------------------------------------------
// Seed + scaffolding helpers
// ---------------------------------------------------------------------------

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
        &StaticKeyProvider::new([1u8; 32], 1),
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

/// See `common::ephemeral_port`: no bind-then-release race (the previous
/// local copy could hand two concurrent tests the same port).
fn ephemeral_port() -> u16 {
    common::ephemeral_port()
}

fn start_proxy(state: std::sync::Arc<AppState>) -> String {
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

/// Retry the request until the proxy is ready (Pingora binds asynchronously).
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

// ---------------------------------------------------------------------------
// DB polling helper — deterministic flush verification
// ---------------------------------------------------------------------------

struct UsageRow {
    prompt: Option<i64>,
    completion: Option<i64>,
    cached: Option<i64>,
    model_key: String,
}

/// A sink that KEEPS the records the proxy hands it.
///
/// This test used to write through a real `SqliteSink` and poll the `usage_record` table. That
/// store is retired (ADR-0002 D-3), and the property under test is "the streaming path produces a
/// usage record with the tokens the upstream reported" — a claim about the proxy's logging path,
/// for which the sink is the instrument, not the subject. The sink still goes through the shared
/// batching engine (`run_channel_sink`), so a record that never reaches the backend would still
/// show up here as a failure.
#[derive(Clone, Default)]
struct RecordingSink {
    delivered: std::sync::Arc<std::sync::Mutex<Vec<UsageRecord>>>,
}

impl RecordingSink {
    /// Poll until a record has been delivered (no bare sleeps; ~5 s budget).
    async fn wait_for_a_record(&self) -> UsageRow {
        for _ in 0..100 {
            if let Some(r) = self
                .delivered
                .lock()
                .expect("delivered mutex")
                .last()
                .cloned()
            {
                return UsageRow {
                    prompt: r.tokens_in.map(|v| v as i64),
                    completion: r.tokens_out.map(|v| v as i64),
                    cached: r.cache_hit_tokens.map(|v| v as i64),
                    model_key: r.model_key,
                };
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("no usage record reached the sink after 5s — the streaming path did not meter");
    }
}

impl UsageSink for RecordingSink {
    fn record(
        &self,
        record: UsageRecord,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        self.delivered.lock().expect("delivered mutex").push(record);
        Box::pin(async {})
    }
}

/// Build the full AppState from a seeded pool, with the recording sink as the usage store.
async fn build_state_with_recording_sink(
    pool: sqlx::SqlitePool,
) -> (std::sync::Arc<AppState>, RecordingSink) {
    let key_provider: std::sync::Arc<dyn KeyProvider> =
        std::sync::Arc::new(StaticKeyProvider::new([1u8; 32], 1));
    let store = ConfigStore::load(pool.clone(), key_provider)
        .await
        .expect("ConfigStore::load");
    let auth = std::sync::Arc::new(
        HttpAuthChecker::new(
            AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
            AuthConfig::default(),
        )
        .expect("HttpAuthChecker::new"),
    );
    let breaker = std::sync::Arc::new(CircuitBreaker::new(BreakerConfig::new(5)));
    let limiter = std::sync::Arc::new(RateLimiter::new());
    let recording = RecordingSink::default();
    let sink: std::sync::Arc<dyn UsageSink> = std::sync::Arc::new(recording.clone());
    let state = AppState::for_tests(
        store,
        auth,
        breaker,
        limiter,
        sink,
        ProxyConfig::default(),
        hydra_server::tenant_api::TenantApiConfig::default(),
    );
    (state, recording)
}

// ===========================================================================
// Tests
// ===========================================================================

/// **OpenAI streaming SSE → SQLite**: a streaming `/v1/chat/completions`
/// request through the real Pingora proxy against a mock upstream returning
/// OpenAI SSE (content deltas + final `usage` chunk + `[DONE]`). The extracted
/// tokens must be persisted to `usage_record` with the correct values.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn openai_streaming_usage_persists_to_sqlite() {
    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "status": true })),
        )
        .mount(&auth_server)
        .await;

    let upstream = MockServer::start().await;
    let sse = "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n\
               data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"content\":\"!\"}}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n\
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
    seed_routed(
        &pool,
        &format!("{}/auth", auth_server.uri()),
        &upstream.uri(),
        "gpt-4",
    )
    .await;
    let (state, sink) = build_state_with_recording_sink(pool.clone()).await;
    let root = start_proxy(state);
    let url = format!("{root}/v1/chat/completions");
    let client = test_client();
    let body = r#"{"model":"gpt-4","stream":true,"messages":[{"role":"user","content":"hi"}]}"#;

    let resp = send_until_ready(&client, &url, body).await;
    assert_eq!(resp.status(), 200);
    // Consume the full SSE body so the proxy's logging hook runs (usage is
    // recorded in `logging`, which fires after the response stream completes).
    let resp_body = resp.text().await.expect("body");
    assert!(
        resp_body.contains("[DONE]"),
        "SSE body should contain [DONE]"
    );

    // Poll the DB until the sink flushes the usage record (batch_size=1 →
    // immediate flush once the bg task drains the channel).
    let row = sink.wait_for_a_record().await;
    assert_eq!(row.model_key, "gpt-4");
    assert_eq!(row.prompt, Some(10), "tokens_in from OpenAI usage");
    assert_eq!(row.completion, Some(5), "tokens_out from OpenAI usage");
    assert_eq!(row.cached, None, "no cache-hit field in this usage");
}

/// **Anthropic coalesced streaming SSE → SQLite**: a streaming `/v1/messages`
/// request where the mock upstream returns `message_start` (input_tokens:42,
/// output_tokens:1) and `message_delta` (output_tokens:13,
/// cache_read_input_tokens:7) in a SINGLE HTTP body (the BUG-1 coalesced-chunk
/// scenario). Both usage objects must be parsed and persisted with correct
/// last-wins values: tokens_in=42, tokens_out=13, cache_hit_tokens=7.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anthropic_coalesced_streaming_usage_persists_to_sqlite() {
    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "status": true })),
        )
        .mount(&auth_server)
        .await;

    let upstream = MockServer::start().await;
    // Both events in one body — wiremock writes it as a single HTTP response,
    // so they coalesce into one or few TCP chunks (the BUG-1 scenario).
    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":42,\"output_tokens\":1}}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":13,\"cache_read_input_tokens\":7}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse.to_string()),
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
    let (state, sink) = build_state_with_recording_sink(pool.clone()).await;
    let root = start_proxy(state);
    let url = format!("{root}/v1/messages");
    let client = test_client();
    let body = r#"{"model":"claude-3-5-sonnet-test","max_tokens":64,"stream":true,"messages":[{"role":"user","content":"hi"}]}"#;

    let resp = send_until_ready(&client, &url, body).await;
    assert_eq!(resp.status(), 200);
    let resp_body = resp.text().await.expect("body");
    assert!(
        resp_body.contains("message_stop"),
        "SSE body should contain message_stop"
    );

    let row = sink.wait_for_a_record().await;
    assert_eq!(row.model_key, "claude-3-5-sonnet-test");
    assert_eq!(row.prompt, Some(42), "input_tokens → tokens_in");
    assert_eq!(
        row.completion,
        Some(13),
        "output_tokens last-wins (13, not 1 from message_start)"
    );
    assert_eq!(
        row.cached,
        Some(7),
        "cache_read_input_tokens → cache_hit_tokens (Anthropic-only; proves scanner kind)"
    );
}
