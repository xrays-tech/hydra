//! TEMPORARY adversarial probe — delete after use.
#![cfg(all(feature = "db", feature = "http-client", feature = "proxy"))]

mod common;

use std::sync::Arc;
use std::time::Duration;

use hydra_core::auth::sha256_hex_string;
use hydra_core::breaker::BreakerConfig;
use hydra_core::model::Tenant;
use hydra_server::crypto::StaticKeyProvider;
use hydra_server::http::{AuthCache, AuthConfig, HttpAuthChecker};
use hydra_server::proxy::breaker_wrap::CircuitBreaker;
use hydra_server::proxy::config::ProxyConfig;
use hydra_server::proxy::limiter::RateLimiter;
use hydra_server::proxy::{AppState, HydraProxy};
use hydra_server::sink::UsageSink;
use hydra_server::store::ConfigStore;
use hydra_server::tenant_api::TenantApiConfig;
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;

struct NoopSink;

impl UsageSink for NoopSink {
    fn record<'a>(
        &'a self,
        _r: hydra_core::model::UsageRecord,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }
}

const TENANT_TOKEN: &str = "sk-tenant-token-0123456789abcdef";

async fn seed_tenant(pool: &sqlx::SqlitePool, id: &str, domain: &str, token: &str) {
    hydra_server::db::insert_tenant(
        pool,
        &Tenant {
            id: id.into(),
            name: format!("{id}-name"),
            domain: domain.into(),
            auth_url: format!("https://auth.{domain}/verify"),
            cert_key: None,
            cert_file: None,
            enabled: true,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        },
    )
    .await
    .expect("insert tenant");
    hydra_server::db::set_tenant_access_token_hash(
        pool,
        id,
        Some(&sha256_hex_string(token.as_bytes())),
    )
    .await
    .expect("set token hash");
}

async fn build_state(pool: &sqlx::SqlitePool) -> Arc<AppState> {
    let kp: Arc<dyn hydra_server::crypto::KeyProvider> =
        Arc::new(StaticKeyProvider::new([7u8; 32], 1));
    let store = ConfigStore::load(pool.clone(), kp)
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
    let sink: Arc<dyn UsageSink> = Arc::new(NoopSink);
    AppState::for_tests(
        store,
        auth,
        breaker,
        limiter,
        sink,
        ProxyConfig::default(),
        TenantApiConfig::default(),
    )
}

fn start_proxy(state: Arc<AppState>) -> String {
    let port = common::ephemeral_port();
    let listen = format!("127.0.0.1:{port}");
    let app = HydraProxy::new(state);
    let mut server = Server::new(Some(Opt::default())).expect("Server::new");
    server.bootstrap();
    let mut svc = pingora_proxy::http_proxy_service(&server.configuration, app);
    svc.add_tcp(&listen);
    server.add_service(svc);
    std::thread::spawn(move || {
        server.run_forever();
    });
    format!("http://127.0.0.1:{port}")
}

/// Probe 1: is the 413 body the documented `{code,message,trace_id}` envelope?
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_oversized_body_envelope() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", TENANT_TOKEN).await;
    let state = build_state(&pool).await;
    let root = start_proxy(state);
    let c = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client");

    let url = format!("{root}/tenant/t1/api/v1/auth/cache/invalidate");
    let huge = "x".repeat(2 * 1024 * 1024);
    let body = format!(r#"{{"api_keys":["{huge}"]}}"#);

    let mut resp = None;
    for _ in 0..60 {
        match c
            .post(&url)
            .bearer_auth(TENANT_TOKEN)
            .header("content-type", "application/json")
            .body(body.clone())
            .send()
            .await
        {
            Ok(r) => {
                resp = Some(r);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(150)).await,
        }
    }
    let r = resp.expect("proxy never became ready");
    let status = r.status().as_u16();
    let hdr_trace = r
        .headers()
        .get("X-Hydra-Trace-Id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let text = r.text().await.unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    println!("PROBE1 status={status}");
    println!("PROBE1 header_trace={hdr_trace:?}");
    println!("PROBE1 body={text}");
    println!(
        "PROBE1 body_has_trace={}",
        v["error"]["trace_id"].is_string()
    );
    assert_eq!(status, 413, "expected the body cap to fire: {text}");
    assert!(
        v["error"]["trace_id"].is_string(),
        "413 body violates the documented envelope (no trace_id): {text}"
    );
}
