//! §3 (wave-5) — `/metrics` self-hosted exposition integration test.
//!
//! Full proxy + admin in one Pingora `Server`. After one proxied request the
//! `/metrics` endpoint (served by the admin service) must show:
//!   - `# HELP` / `# TYPE` exposition headers
//!   - `hydra_requests_total` with the tenant/provider/model/status labels
//!
//! No internal logic is mocked (dev-plan §1 铁律 2): `wiremock` stands in for
//! the *external* auth + LLM upstreams only.

#![cfg(all(feature = "db", feature = "http-client", feature = "proxy"))]

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
use hydra_server::admin::{AdminService, AdminState};
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
use pingora_core::services::listening::Service as ListenService;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "metrics-test-token";

/// Minimal no-op usage sink (production sinks tested separately).
struct NoopSink;

impl hydra_server::sink::UsageSink for NoopSink {
    fn record(&self, _record: UsageRecord) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}

/// Bind an ephemeral port, return it, release so Pingora can rebind.
fn ephemeral_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    l.local_addr().expect("addr").port()
}

/// Seed the test DB with one tenant/provider/model/key + limit role.
async fn seed(pool: &sqlx::SqlitePool, auth_url: &str, upstream: &str) {
    repo::insert_provider(
        pool,
        &Provider {
            id: "p1".into(),
            key: "openai".into(),
            name: "O".into(),
            endpoint: upstream.into(),
            weight: 1,
            created_at: "2026-01-01 00:00:00".into(),
            updated_at: "2026-01-01 00:00:00".into(),
            max_concurrency: None,
            max_queue_depth: None,
            queue_wait_timeout_ms: None,
        },
    )
    .await
    .unwrap();
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
    .unwrap();
    repo::insert_tenant(
        pool,
        &Tenant {
            id: "t1".into(),
            name: "T".into(),
            domain: "localhost".into(),
            auth_url: auth_url.into(),
            cert_key: None,
            cert_file: None,
            enabled: true,
            created_at: "2026-01-01 00:00:00".into(),
            updated_at: "2026-01-01 00:00:00".into(),
        },
    )
    .await
    .unwrap();
    repo::insert_tenant_provider(
        pool,
        &TenantProvider {
            id: "tp1".into(),
            tenant_id: "t1".into(),
            provider_id: "p1".into(),
        },
    )
    .await
    .unwrap();
    repo::insert_tenant_model(
        pool,
        &TenantModel {
            id: "tm1".into(),
            tenant_id: "t1".into(),
            model_key: "gpt-4".into(),
        },
    )
    .await
    .unwrap();
    repo::insert_provider_key(
        pool,
        &StaticKeyProvider::new([1u8; 32], 1),
        &ProviderKey {
            id: "pk1".into(),
            provider_id: "p1".into(),
            api_key: "sk-upstream".into(),
            created_at: "2026-01-01 00:00:00".into(),
        },
    )
    .await
    .unwrap();
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
            created_at: "2026-01-01 00:00:00".into(),
        },
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_endpoint_exposes_proxy_counters() {
    // --- wiremock upstreams -------------------------------------------------
    let auth_server = MockServer::start().await;
    let upstream_server = MockServer::start().await;

    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&auth_server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"id":"x","object":"chat.completion","choices":[]}"#),
        )
        .mount(&upstream_server)
        .await;

    // --- seed + build shared state ------------------------------------------
    let pool = common::setup_pool().await;
    seed(
        &pool,
        &format!("{}/auth", auth_server.uri()),
        &upstream_server.uri(),
    )
    .await;

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
    let breaker = Arc::new(CircuitBreaker::new(BreakerConfig::new(5)));
    let limiter = Arc::new(RateLimiter::new());
    let sink: Arc<dyn hydra_server::sink::UsageSink> = Arc::new(NoopSink);

    let proxy_state = Arc::new(AppState {
        store: store.clone(),
        auth: auth.clone(),
        breaker: breaker.clone(),
        limiter,
        admission: hydra_server::proxy::admission::AdmissionControl::new(),
        sink,
        proxy: ProxyConfig::default(),
    });

    let admin_state = Arc::new(AdminState::new(
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
        None, // no forward target in tests
    ));

    // --- start Pingora server with BOTH proxy + admin services ---------------
    let proxy_port = ephemeral_port();
    let admin_port = ephemeral_port();
    let proxy_addr = format!("127.0.0.1:{proxy_port}");
    let admin_addr = format!("127.0.0.1:{admin_port}");

    let proxy_app = HydraProxy::new(proxy_state);
    let admin_app = AdminService::new(admin_state);

    let mut server = Server::new(Some(Opt::default())).expect("Server::new");
    server.bootstrap();
    let mut proxy_svc = pingora_proxy::http_proxy_service(&server.configuration, proxy_app);
    proxy_svc.add_tcp(&proxy_addr);
    server.add_service(proxy_svc);

    let mut admin_svc = ListenService::new("admin".to_string(), admin_app);
    admin_svc.add_tcp(&admin_addr);
    server.add_service(admin_svc);

    let _handle = std::thread::spawn(move || server.run_forever());

    // --- send one proxied request -------------------------------------------
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client");

    let proxy_url = format!("http://localhost:{proxy_port}/v1/chat/completions");
    let body = r#"{"model":"gpt-4","messages":[{"role":"user","content":"hi"}]}"#;

    // Retry until the proxy is ready.
    let mut last_err = None;
    for _ in 0..50 {
        match client
            .post(&proxy_url)
            .header("authorization", "Bearer test-client-key")
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
        {
            Ok(r) => {
                assert_eq!(r.status(), 200, "proxy should succeed");
                break;
            }
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    if last_err.is_some() {
        panic!("proxy never ready: {last_err:?}");
    }

    // --- query /metrics on the admin port -----------------------------------
    let metrics_url = format!("http://127.0.0.1:{admin_port}/metrics");
    let resp = {
        let mut last_err = None;
        let mut ok = None;
        for _ in 0..50 {
            match client.get(&metrics_url).bearer_auth(TOKEN).send().await {
                Ok(r) => {
                    ok = Some(r);
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
        ok.unwrap_or_else(|| panic!("admin never ready: {last_err:?}"))
    };

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "text/plain; version=0.0.4; charset=utf-8"
    );

    let text = resp.text().await.expect("metrics body");

    // Exposition format must include HELP + TYPE.
    assert!(text.contains("# HELP"), "missing HELP line");
    assert!(text.contains("# TYPE"), "missing TYPE line");

    // The proxied request must have incremented hydra_requests_total.
    assert!(
        text.contains("hydra_requests_total"),
        "hydra_requests_total not found in:\n{text}"
    );
    // The label set must include the tenant / provider / model / status.
    assert!(
        text.contains("tenant=\"t1\""),
        "tenant label missing in:\n{text}"
    );
    assert!(
        text.contains("model=\"gpt-4\""),
        "model label missing in:\n{text}"
    );
}
