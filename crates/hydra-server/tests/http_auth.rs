//! W3 §2.2 — `HttpAuthChecker` (reqwest boundary) tests.
//!
//! Every test spins up a **real `wiremock` HTTP server** that stands in for
//! the external tenant auth service (a network-layer double, dev-plan §1
//! 铁律 2 — not a mock of any of our own functions). The pure cache-decision
//! / status→CacheOp / Verdict→AuthVerdict logic is exercised by `hydra-core`;
//! here we only assert the reqwest round-trip + cache-back-fill assembly
//! against the design §11.2/§11.3 contract.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hydra_core::auth::{AuthVerdict, CacheSource};
use hydra_core::model::Tenant;
use hydra_server::http::{AuthCache, AuthChecker, AuthConfig, Clock, FailMode, HttpAuthChecker};
use wiremock::matchers::{header, header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ALLOW_TTL: Duration = Duration::from_secs(300);
const DENY_TTL: Duration = Duration::from_secs(30);
const SHORT_TIMEOUT: Duration = Duration::from_secs(2);

/// A frozen, manually-advancable clock so TTL expiry is deterministic.
fn frozen_clock() -> (Arc<Mutex<Instant>>, Clock) {
    let t = Arc::new(Mutex::new(Instant::now()));
    let clock: Clock = {
        let t = Arc::clone(&t);
        Arc::new(move || *t.lock().unwrap())
    };
    (t, clock)
}

fn config(fail_mode: FailMode, timeout: Duration) -> AuthConfig {
    AuthConfig {
        allow_ttl: ALLOW_TTL,
        deny_ttl: DENY_TTL,
        timeout,
        fail_mode,
    }
}

fn cache_with_default_clock() -> AuthCache {
    AuthCache::new(ALLOW_TTL, DENY_TTL)
}

fn tenant_at(server_uri: &str) -> Tenant {
    Tenant {
        id: "t1".to_string(),
        name: "Test Tenant".to_string(),
        domain: "test.example.com".to_string(),
        auth_url: format!("{server_uri}/auth"),
        cert_key: None,
        cert_file: None,
        enabled: true,
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
    }
}

/// Wire up a checker against a freshly-started mock server with a real
/// (default clock) cache + given config.
async fn setup(fail_mode: FailMode, timeout: Duration) -> (MockServer, HttpAuthChecker) {
    let server = MockServer::start().await;
    let checker = HttpAuthChecker::new(cache_with_default_clock(), config(fail_mode, timeout))
        .expect("reqwest client must build");
    (server, checker)
}

// ---------------------------------------------------------------------------
// T2.1 — upstream 2xx → Allowed{Miss}, cached; 2nd call hits cache (1 HTTP).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_upstream_2xx_caches_allowed() {
    let (server, checker) = setup(FailMode::Closed, SHORT_TIMEOUT).await;
    Mock::given(method("POST"))
        .and(path("/auth"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1) // second call MUST hit the cache, not the wiremock
        .mount(&server)
        .await;

    let tenant = tenant_at(&server.uri());

    let v1 = checker.check(&tenant, "sk-test").await;
    assert_eq!(
        v1,
        AuthVerdict::Allowed {
            source: CacheSource::Miss
        }
    );

    // second call served from cache (Mock::expect(1) verifies on drop)
    let v2 = checker.check(&tenant, "sk-test").await;
    assert_eq!(
        v2,
        AuthVerdict::Allowed {
            source: CacheSource::Hit
        }
    );
    assert_eq!(checker.cache().len(), 1);
}

// ---------------------------------------------------------------------------
// T2.2 — upstream 401 → Denied{401,Miss}, cached; 2nd call hits cache.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_upstream_401_caches_denied() {
    let (server, checker) = setup(FailMode::Closed, SHORT_TIMEOUT).await;
    Mock::given(method("POST"))
        .and(path("/auth"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;

    let tenant = tenant_at(&server.uri());

    let v1 = checker.check(&tenant, "sk-bad").await;
    assert_eq!(
        v1,
        AuthVerdict::Denied {
            status: 401,
            reason: "denied",
            source: CacheSource::Miss
        }
    );

    // second call served from cache (expect(1) verifies)
    let v2 = checker.check(&tenant, "sk-bad").await;
    assert_eq!(
        v2,
        AuthVerdict::Denied {
            status: 401,
            reason: "denied",
            source: CacheSource::Hit
        }
    );
}

// ---------------------------------------------------------------------------
// T2.3 — upstream 403 handled like 401 (deny, cached).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_upstream_403_denied_cached() {
    let (server, checker) = setup(FailMode::Closed, SHORT_TIMEOUT).await;
    Mock::given(method("POST"))
        .and(path("/auth"))
        .respond_with(ResponseTemplate::new(403))
        .expect(1)
        .mount(&server)
        .await;

    let tenant = tenant_at(&server.uri());

    let v1 = checker.check(&tenant, "sk-forbidden").await;
    assert_eq!(
        v1,
        AuthVerdict::Denied {
            status: 401,
            reason: "denied",
            source: CacheSource::Miss
        }
    );
    // cached
    let v2 = checker.check(&tenant, "sk-forbidden").await;
    assert_eq!(
        v2,
        AuthVerdict::Denied {
            status: 401,
            reason: "denied",
            source: CacheSource::Hit
        }
    );
}

// ---------------------------------------------------------------------------
// T2.4 — upstream 5xx → fail-closed 503, NOT cached; each call re-hits.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_upstream_5xx_fail_closed_no_cache() {
    let (server, checker) = setup(FailMode::Closed, SHORT_TIMEOUT).await;
    Mock::given(method("POST"))
        .and(path("/auth"))
        .respond_with(ResponseTemplate::new(500))
        // no expect(1): 5xx is never cached, so each call re-goes-upstream
        .mount(&server)
        .await;

    let tenant = tenant_at(&server.uri());

    let v1 = checker.check(&tenant, "sk-err").await;
    assert_eq!(
        v1,
        AuthVerdict::Denied {
            status: 503,
            reason: "auth_upstream_unavailable",
            source: CacheSource::Local
        }
    );
    assert_eq!(checker.cache().len(), 0, "5xx must not be cached");

    // second call goes upstream again (still not cached)
    let v2 = checker.check(&tenant, "sk-err").await;
    assert_eq!(
        v2,
        AuthVerdict::Denied {
            status: 503,
            reason: "auth_upstream_unavailable",
            source: CacheSource::Local
        }
    );
    assert_eq!(checker.cache().len(), 0);

    // confirm two real upstream calls happened
    let n = server
        .received_requests()
        .await
        .expect("recording enabled")
        .len();
    assert_eq!(n, 2);
}

// ---------------------------------------------------------------------------
// T2.5 — upstream slower than timeout → fail-closed 503, not cached.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_upstream_timeout_fail_closed() {
    let (server, checker) = setup(FailMode::Closed, Duration::from_millis(100)).await;
    Mock::given(method("POST"))
        .and(path("/auth"))
        .respond_with(
            ResponseTemplate::new(200).set_delay(Duration::from_millis(500)), // > 100ms
        )
        .mount(&server)
        .await;

    let tenant = tenant_at(&server.uri());

    let v = checker.check(&tenant, "sk-slow").await;
    assert_eq!(
        v,
        AuthVerdict::Denied {
            status: 503,
            reason: "auth_upstream_unavailable",
            source: CacheSource::Local
        }
    );
    assert_eq!(checker.cache().len(), 0, "timeout must not be cached");
}

// ---------------------------------------------------------------------------
// T2.6 — fail-open: 5xx → Allowed{Local}, not cached.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_fail_open_on_5xx() {
    let (server, checker) = setup(FailMode::Open, SHORT_TIMEOUT).await;
    Mock::given(method("POST"))
        .and(path("/auth"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let tenant = tenant_at(&server.uri());

    let v = checker.check(&tenant, "sk-open").await;
    assert_eq!(
        v,
        AuthVerdict::Allowed {
            source: CacheSource::Local
        }
    );
    assert_eq!(checker.cache().len(), 0, "fail-open must not be cached");
}

#[tokio::test]
async fn auth_fail_open_on_timeout() {
    let (server, checker) = setup(FailMode::Open, Duration::from_millis(100)).await;
    Mock::given(method("POST"))
        .and(path("/auth"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(500)))
        .mount(&server)
        .await;

    let tenant = tenant_at(&server.uri());
    let v = checker.check(&tenant, "sk-open").await;
    assert_eq!(
        v,
        AuthVerdict::Allowed {
            source: CacheSource::Local
        }
    );
}

// ---------------------------------------------------------------------------
// T2.7 — request contract: POST, Authorization: Bearer, X-Hydra-Tenant,
// X-Hydra-Trace-Id, JSON body with api_key/tenant_id (design §11.3).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_request_contract() {
    let (server, checker) = setup(FailMode::Closed, SHORT_TIMEOUT).await;
    // Use matchers to require the exact contract; if the request is wrong the
    // mock won't match → wiremock returns its default non-200 → checker maps
    // it to CacheOp::None → fail-closed 503, failing the verdict assert below.
    Mock::given(method("POST"))
        .and(path("/auth"))
        .and(header("authorization", "Bearer sk-contract"))
        .and(header("x-hydra-tenant", "t1"))
        .and(header_exists("x-hydra-trace-id"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let tenant = tenant_at(&server.uri());
    let v = checker.check(&tenant, "sk-contract").await;
    assert_eq!(
        v,
        AuthVerdict::Allowed {
            source: CacheSource::Miss
        }
    );

    // Additionally inspect the recorded request body for the JSON contract.
    let reqs = server.received_requests().await.expect("recording enabled");
    assert_eq!(reqs.len(), 1);
    let req = &reqs[0];
    assert_eq!(req.method.as_str(), "POST");
    // headers
    assert_eq!(
        req.headers.get("authorization").unwrap().to_str().unwrap(),
        "Bearer sk-contract"
    );
    assert_eq!(
        req.headers.get("x-hydra-tenant").unwrap().to_str().unwrap(),
        "t1"
    );
    assert!(req.headers.contains_key("x-hydra-trace-id"));
    assert!(req.headers.contains_key("content-type"));
    // body JSON
    let body = String::from_utf8_lossy(&req.body);
    assert!(body.contains("\"api_key\":\"sk-contract\""), "body: {body}");
    assert!(body.contains("\"key\":\"sk-contract\""), "body: {body}");
    assert!(body.contains("\"tenant_id\":\"t1\""), "body: {body}");
}

// ---------------------------------------------------------------------------
// T2.8 — 2xx body `expires_in` overrides the default allow TTL.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_response_expires_in_override() {
    // Deterministic clock: advance past expires_in (60s) but well under the
    // default allow_ttl (300s) to PROVE the override took effect.
    let (time, clock) = frozen_clock();
    let cache = AuthCache::with_clock(ALLOW_TTL, DENY_TTL, clock);
    let checker = HttpAuthChecker::new(cache, config(FailMode::Closed, SHORT_TIMEOUT))
        .expect("reqwest client must build");

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(b"{\"allowed\":true,\"expires_in\":60}", "application/json"),
        )
        // called twice: once fresh, once after expiry forces re-auth
        .expect(2)
        .mount(&server)
        .await;

    let tenant = tenant_at(&server.uri());

    // (1) fresh allow — upstream called, cached with TTL=60 (overridden)
    let v1 = checker.check(&tenant, "sk-exp").await;
    assert_eq!(
        v1,
        AuthVerdict::Allowed {
            source: CacheSource::Miss
        }
    );

    // (2) within 60s — still cached (would be Hit even under default 300s)
    let v2 = checker.check(&tenant, "sk-exp").await;
    assert_eq!(
        v2,
        AuthVerdict::Allowed {
            source: CacheSource::Hit
        }
    );

    // (3) advance 120s — past expires_in=60, so MUST re-auth.
    //     If the default 300s had been used, this would still be a Hit and
    //     expect(2) would fail (only 1 upstream call).
    *time.lock().unwrap() += Duration::from_secs(120);
    let v3 = checker.check(&tenant, "sk-exp").await;
    assert_eq!(
        v3,
        AuthVerdict::Allowed {
            source: CacheSource::Miss
        }
    );
}

// ---------------------------------------------------------------------------
// T2.9 — HttpAuthChecker owns an independent reqwest client (design §11.4:
// pool isolated from the Pingora upstream channel).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_independent_client_pool() {
    // Two independently-built checkers each build their own reqwest::Client
    // (never sharing an externally-supplied pool by default). We assert they
    // own a configured client with its own pool, and that the public accessor
    // exposes the same instance across calls on one checker.
    let c1 = HttpAuthChecker::new(
        cache_with_default_clock(),
        config(FailMode::Closed, SHORT_TIMEOUT),
    )
    .expect("c1 build");
    let c2 = HttpAuthChecker::new(
        cache_with_default_clock(),
        config(FailMode::Closed, SHORT_TIMEOUT),
    )
    .expect("c2 build");

    // The client accessor returns a stable, owned pool (same pointer each call).
    let client_a = c1.client();
    let client_b = c1.client();
    assert!(
        std::ptr::addr_eq(client_a, client_b),
        "client accessor must return the stable owned pool"
    );

    // Two checkers carry distinct client instances (independent pools).
    assert!(
        !std::ptr::addr_eq(c1.client(), c2.client()),
        "two checkers must own independent client pools"
    );

    // The independent client actually performs a real round-trip via wiremock
    // (proves it's a fully-wired pool, not a no-op).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let tenant = tenant_at(&server.uri());
    let v = c1.check(&tenant, "sk-pool").await;
    assert_eq!(
        v,
        AuthVerdict::Allowed {
            source: CacheSource::Miss
        }
    );
}

// ---------------------------------------------------------------------------
// Extra: invalidate() forces re-auth (design §11.7); empty auth_url → 401.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invalidate_forces_reauth() {
    let (server, checker) = setup(FailMode::Closed, SHORT_TIMEOUT).await;
    Mock::given(method("POST"))
        .and(path("/auth"))
        .respond_with(ResponseTemplate::new(200))
        .expect(2) // initial allow + post-invalidate re-auth
        .mount(&server)
        .await;

    let tenant = tenant_at(&server.uri());

    let v1 = checker.check(&tenant, "sk-inv").await;
    assert_eq!(
        v1,
        AuthVerdict::Allowed {
            source: CacheSource::Miss
        }
    );

    // cached now
    assert_eq!(
        checker.check(&tenant, "sk-inv").await,
        AuthVerdict::Allowed {
            source: CacheSource::Hit
        }
    );

    // force-invalidate → next call re-goes-upstream
    let n = checker
        .invalidate(&tenant.id, &["sk-inv".to_string()])
        .await;
    assert_eq!(n, 1);
    let v3 = checker.check(&tenant, "sk-inv").await;
    assert_eq!(
        v3,
        AuthVerdict::Allowed {
            source: CacheSource::Miss
        }
    );
}

#[tokio::test]
async fn invalidate_tenant_forces_reauth() {
    let (server, checker) = setup(FailMode::Closed, SHORT_TIMEOUT).await;
    Mock::given(method("POST"))
        .and(path("/auth"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let tenant = tenant_at(&server.uri());
    checker.check(&tenant, "k1").await;
    checker.check(&tenant, "k2").await;
    // both cached now
    assert_eq!(checker.cache().len(), 2);

    let n = checker.invalidate_tenant(&tenant.id).await;
    assert_eq!(n, 2);
    assert_eq!(checker.cache().len(), 0);
}

#[tokio::test]
async fn empty_auth_url_is_denied_locally() {
    // design §11.1: empty/missing auth_url → always 401, no upstream call.
    let (_server, checker) = setup(FailMode::Closed, SHORT_TIMEOUT).await;
    let mut tenant = tenant_at("http://unused");
    tenant.auth_url = String::new();

    let v = checker.check(&tenant, "sk-x").await;
    assert_eq!(
        v,
        AuthVerdict::Denied {
            status: 401,
            reason: "no_auth_url",
            source: CacheSource::Local
        }
    );
    assert_eq!(checker.cache().len(), 0);
}

#[tokio::test]
async fn fail_open_on_connection_refused() {
    // No mock mounted on a real-but-unused port range: connection refused →
    // fail-open verdict, not cached. Point auth_url at a closed port.
    let checker = HttpAuthChecker::new(
        cache_with_default_clock(),
        config(FailMode::Open, SHORT_TIMEOUT),
    )
    .expect("build");
    let mut tenant = tenant_at("http://unused");
    // 127.0.0.1:9 (discard port) — reliably refuses connections.
    tenant.auth_url = "http://127.0.0.1:9/auth".to_string();

    let v = checker.check(&tenant, "sk-conn").await;
    assert_eq!(
        v,
        AuthVerdict::Allowed {
            source: CacheSource::Local
        }
    );
    assert_eq!(checker.cache().len(), 0);
}

// ---------------------------------------------------------------------------
// Dogress `/auth/api_key` contract (crates/api AuthApiKeyResponse): the
// service ALWAYS answers HTTP 200 and flags denials in the body as
// `{"status":false}`. The checker must read the body, not the status code.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_upstream_200_body_status_false_denies_cached() {
    let (server, checker) = setup(FailMode::Closed, SHORT_TIMEOUT).await;
    Mock::given(method("POST"))
        .and(path("/auth"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            b"{\"status\":false,\"reason\":\"invalid_key\"}",
            "application/json",
        ))
        .expect(1) // denial is cached: second call MUST hit the cache
        .mount(&server)
        .await;

    let tenant = tenant_at(&server.uri());

    let v1 = checker.check(&tenant, "sh-bad").await;
    assert_eq!(
        v1,
        AuthVerdict::Denied {
            status: 401,
            reason: "denied",
            source: CacheSource::Miss
        }
    );

    let v2 = checker.check(&tenant, "sh-bad").await;
    assert_eq!(
        v2,
        AuthVerdict::Denied {
            status: 401,
            reason: "denied",
            source: CacheSource::Hit
        }
    );
    assert_eq!(checker.cache().len(), 1);
}

#[tokio::test]
async fn auth_upstream_200_body_status_true_allows_cached() {
    let (server, checker) = setup(FailMode::Closed, SHORT_TIMEOUT).await;
    Mock::given(method("POST"))
        .and(path("/auth"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            b"{\"status\":true,\"reason\":\"\",\"user_id\":42}",
            "application/json",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let tenant = tenant_at(&server.uri());

    let v1 = checker.check(&tenant, "sh-good").await;
    assert_eq!(
        v1,
        AuthVerdict::Allowed {
            source: CacheSource::Miss
        }
    );
    let v2 = checker.check(&tenant, "sh-good").await;
    assert_eq!(
        v2,
        AuthVerdict::Allowed {
            source: CacheSource::Hit
        }
    );
}

#[tokio::test]
async fn auth_upstream_200_body_design_allowed_false_denies() {
    // design §11.3 optional refinement: `{"allowed":false}` on a 2xx is a
    // denial too (mock tenant / §11.3-style services use this shape).
    let (server, checker) = setup(FailMode::Closed, SHORT_TIMEOUT).await;
    Mock::given(method("POST"))
        .and(path("/auth"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            b"{\"allowed\":false,\"reason\":\"blocked\"}",
            "application/json",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let tenant = tenant_at(&server.uri());
    let v = checker.check(&tenant, "sk-blocked").await;
    assert_eq!(
        v,
        AuthVerdict::Denied {
            status: 401,
            reason: "denied",
            source: CacheSource::Miss
        }
    );
}
