//! Tenant self-service API on the DATA-PLANE listener — interception and the
//! token gate (plan T4).
//!
//! T4 delivers the reserved-prefix interception, the token gate and an honest
//! routing skeleton: no endpoint is wired yet, so every parsed route answers
//! `404 unknown path` **locally**. E1 arrives in T5, E2 in T6, E3 in T8 — each
//! replacing exactly one 404 arm. That ordering is deliberate: if T4 wired E1,
//! T5's RED step could never fail for the right reason.

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
use serde_json::Value;

/// A sink that records nothing: these tests never proxy a request.
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
const OTHER_TOKEN: &str = "sk-other-tenant-token-0123456789";

/// The tenant row must carry the token's HASH — the snapshot is what the gate
/// reads, and a plaintext token in the row would be a different bug.
async fn seed_tenant(pool: &sqlx::SqlitePool, id: &str, domain: &str, token: Option<&str>) {
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
    if let Some(tok) = token {
        hydra_server::db::set_tenant_access_token_hash(
            pool,
            id,
            Some(&sha256_hex_string(tok.as_bytes())),
        )
        .await
        .expect("set token hash");
    }
}

async fn build_state(pool: &sqlx::SqlitePool, cfg: TenantApiConfig) -> Arc<AppState> {
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
        cfg,
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

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client")
}

/// Retry until the listener accepts; a sleep would be a flake generator.
async fn send_until_ready(
    c: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
    host: Option<&str>,
) -> reqwest::Response {
    for _ in 0..60 {
        let mut req = c.get(url);
        if let Some(b) = bearer {
            req = req.bearer_auth(b);
        }
        if let Some(h) = host {
            req = req.header("host", h);
        }
        match req.send().await {
            Ok(r) => return r,
            Err(_) => tokio::time::sleep(Duration::from_millis(150)).await,
        }
    }
    panic!("proxy never became ready at {url}");
}

/// A raw GET with a caller-controlled `X-Forwarded-For`, for the trusted-proxy
/// tests. A real end client never sets this — only a trusted proxy does — so the
/// test plays the proxy's role by setting it directly (its peer is 127.0.0.1).
async fn send_with_xff(
    c: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
    xff: Option<&str>,
) -> reqwest::Response {
    for _ in 0..60 {
        let mut req = c.get(url);
        if let Some(b) = bearer {
            req = req.bearer_auth(b);
        }
        if let Some(x) = xff {
            req = req.header("x-forwarded-for", x);
        }
        match req.send().await {
            Ok(r) => return r,
            Err(_) => tokio::time::sleep(Duration::from_millis(150)).await,
        }
    }
    panic!("proxy never became ready at {url}");
}

/// A raw GET with TWO separate `X-Forwarded-For` header lines (as emitted by
/// proxies that append a new line rather than merging into one). The first
/// line is the "forged" value a client might inject; the second is the
/// proxy-appended real client. The test relies on both lines being emitted on
/// the wire — if reqwest collapsed them, the server would see only the first
/// (forged) line and the discriminating assertions below would fail.
async fn send_with_xff_two_lines(
    c: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
    first_line: &str,
    second_line: &str,
) -> reqwest::Response {
    let first = reqwest::header::HeaderValue::from_str(first_line).expect("valid header value");
    let second = reqwest::header::HeaderValue::from_str(second_line).expect("valid header value");
    for _ in 0..60 {
        let mut req = c.get(url);
        if let Some(b) = bearer {
            req = req.bearer_auth(b);
        }
        // Build a HeaderMap with two entries for the same key. `append` (not
        // `insert`) ensures both are present; when serialized to the wire this
        // produces two separate `X-Forwarded-For:` header lines.
        let xff_name = reqwest::header::HeaderName::from_static("x-forwarded-for");
        let mut headers = reqwest::header::HeaderMap::new();
        headers.append(xff_name.clone(), first.clone());
        headers.append(xff_name, second.clone());
        req = req.headers(headers);
        match req.send().await {
            Ok(r) => return r,
            Err(_) => tokio::time::sleep(Duration::from_millis(150)).await,
        }
    }
    panic!("proxy never became ready at {url}");
}

async fn body_json(r: reqwest::Response) -> (u16, Value) {
    let status = r.status().as_u16();
    let text = r.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    (status, v)
}

// ---------------------------------------------------------------------------
// T2 / T3 / T4 — the gate itself
// ---------------------------------------------------------------------------

/// No credential at all: 401, and the answer must come from the gate — not
/// from the proxy pipeline's `missing_api_key` (which is a 401 too, but means
/// "you are a client without a key", a different contract).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_credential_is_401_from_the_gate_not_from_the_pipeline() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);
    let (status, v) = body_json(
        send_until_ready(
            &client(),
            &format!("{root}/tenant/t1/api/v1/whoami"),
            None,
            None,
        )
        .await,
    )
    .await;
    assert_eq!(status, 401, "got {v}");
    assert_eq!(v["error"]["code"], "unauthorized", "got {v}");
    let msg = v["error"]["message"].as_str().unwrap_or_default();
    assert!(
        !msg.contains("api_key") && !msg.contains("missing_api_key"),
        "the gate must answer, not the client-key pipeline: {msg}"
    );
}

/// A wrong token is 401 with the SAME wording as a missing one — the response
/// must not help an attacker distinguish "unknown token" from "no token".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wrong_token_is_401_with_identical_wording() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);
    let c = client();
    let url = format!("{root}/tenant/t1/api/v1/whoami");
    let (s_none, v_none) = body_json(send_until_ready(&c, &url, None, None).await).await;
    let (s_wrong, v_wrong) =
        body_json(send_until_ready(&c, &url, Some("sk-not-the-tenant-token-000000"), None).await)
            .await;
    assert_eq!(s_none, 401, "{v_none}");
    assert_eq!(s_wrong, 401, "{v_wrong}");
    assert_eq!(v_none["error"]["message"], v_wrong["error"]["message"]);
}

/// The URL's tenant id is a CROSS-CHECK, not an identity: a token that belongs
/// to another tenant must be refused with 403 (never 200, never 404-for-safety,
/// because the tenant id is not a secret — it is in the caller's own base URL).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_for_another_tenant_is_403_not_404() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    seed_tenant(&pool, "t2", "other.example", Some(OTHER_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);
    let (status, v) = body_json(
        send_until_ready(
            &client(),
            &format!("{root}/tenant/t2/api/v1/whoami"),
            Some(TENANT_TOKEN),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(status, 403, "got {v}");
    assert_eq!(v["error"]["code"], "tenant_id_mismatch", "got {v}");
}

/// THE reason the interception must precede the api-key extraction: a tenant
/// token presented here must never be read as a client key. Nothing may reach
/// the tenant's `auth_url`, and nothing may be written to the metering sink.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_tenant_token_is_never_treated_as_a_client_key() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.local", Some(TENANT_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);
    let (status, v) = body_json(
        send_until_ready(
            &client(),
            &format!("{root}/tenant/t1/api/v1/whoami"),
            Some(TENANT_TOKEN),
            Some("acme.local"),
        )
        .await,
    )
    .await;
    // An authorised call answers E1 (T5). If the token had been taken for a
    // client key, external auth would have run against
    // `https://auth.acme.local/verify` and failed closed with 503 — a different
    // status and a different code. A 200 here proves the gate ran first.
    assert_eq!(status, 200, "expected E1, got {v}");
    assert_eq!(v["tenant_id"], "t1", "got {v}");
}

/// T19: identity comes from the SNAPSHOT, not the DB. Rotating the hash in the
/// database without reloading must NOT authenticate the new value.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_gate_reads_the_snapshot_not_the_database() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state.clone());
    let c = client();
    let url = format!("{root}/tenant/t1/api/v1/whoami");

    // Rotate in the DB only. No reload_all.
    let rotated = "sk-rotated-token-9999999999";
    hydra_server::db::set_tenant_access_token_hash(
        &pool,
        "t1",
        Some(&sha256_hex_string(rotated.as_bytes())),
    )
    .await
    .expect("rotate");

    let (old_status, _) =
        body_json(send_until_ready(&c, &url, Some(TENANT_TOKEN), None).await).await;
    assert_eq!(
        old_status, 200,
        "the OLD token must still work until the snapshot is reloaded (it is what the gate reads)"
    );
    let (new_status, _) = body_json(send_until_ready(&c, &url, Some(rotated), None).await).await;
    assert_eq!(
        new_status, 401,
        "the new token must NOT work before the snapshot carries it"
    );

    // After a reload the snapshot catches up and the roles swap.
    state.store.reload_all().await.expect("reload");
    let (old_after, _) =
        body_json(send_until_ready(&c, &url, Some(TENANT_TOKEN), None).await).await;
    let (new_after, _) = body_json(send_until_ready(&c, &url, Some(rotated), None).await).await;
    assert_eq!(
        old_after, 401,
        "the old token must stop working after the reload"
    );
    assert_eq!(new_after, 200, "the new token must work after the reload");
}

/// A tenant with no configured token can never authenticate (fail closed).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tenant_without_a_token_is_never_authenticated() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", None).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);
    for tok in [None, Some(""), Some(TENANT_TOKEN)] {
        let (status, v) = body_json(
            send_until_ready(
                &client(),
                &format!("{root}/tenant/t1/api/v1/whoami"),
                tok,
                None,
            )
            .await,
        )
        .await;
        assert_eq!(status, 401, "token={tok:?} got {v}");
    }
}

/// T20 (gate layer): before the first snapshot a node holds no token hashes at
/// all, so the gate must fail CLOSED with 503 — never pass a request through
/// because it "could not check".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn before_the_first_snapshot_the_gate_fails_closed_with_503() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let kp: Arc<dyn hydra_server::crypto::KeyProvider> =
        Arc::new(StaticKeyProvider::new([7u8; 32], 1));
    // `from_snapshot` is the edge shape: no replication content until the first
    // snapshot arrives.
    let store = ConfigStore::from_snapshot(hydra_core::config::ConfigData::default(), kp);
    let auth = Arc::new(
        HttpAuthChecker::new(
            AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
            AuthConfig::default(),
        )
        .expect("checker"),
    );
    let state = AppState::for_tests(
        store,
        auth,
        Arc::new(CircuitBreaker::new(BreakerConfig::new(5))),
        Arc::new(RateLimiter::new()),
        Arc::new(NoopSink) as Arc<dyn UsageSink>,
        ProxyConfig::default(),
        TenantApiConfig::default(),
    );
    let root = start_proxy(state);
    let (status, v) = body_json(
        send_until_ready(
            &client(),
            &format!("{root}/tenant/t1/api/v1/whoami"),
            Some(TENANT_TOKEN),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(status, 503, "got {v}");
    assert_eq!(v["error"]["code"], "not_ready", "got {v}");
}

// ---------------------------------------------------------------------------
// The reserved prefix: near misses must NOT reach the proxy pipeline
// ---------------------------------------------------------------------------

/// Reason C of the design, in its general form: gating on the three literal
/// paths would let `/tenant/…/typo` fall through to the api-key extraction and
/// external auth, i.e. POST the tenant's own token to its `auth_url`. Anything
/// under the reserved prefix is answered HERE.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_path_under_the_reserved_prefix_is_answered_locally() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);
    let c = client();
    for path in [
        "/tenant/t1/api/v1/typo",
        "/tenant/t1/api/v1/models",
        "/tenant/t1/api/v2/whoami",
        "/tenant/t1/api/v1/usage/",
        "/tenant/",
        "/tenant",
    ] {
        let (status, v) = body_json(
            send_until_ready(&c, &format!("{root}{path}"), Some(TENANT_TOKEN), None).await,
        )
        .await;
        assert!(
            status == 404 || status == 401,
            "{path} must be answered locally, got {status} {v}"
        );
        // Differential assertion on the ENVELOPE, not on a single field.
        //
        // The proxy's short-circuit body is `{"error":{"message":"<reason>",
        // "type":"proxy_error"}}` — the reason is in `message` and there is no
        // `code` at all. The tenant API uses the admin error model
        // (`{"error":{"code","message","trace_id"}}`). Checking only
        // `error.code` would pass VACUOUSLY against the pipeline (it is null
        // there), which is exactly how this assertion was first written.
        assert_eq!(
            v["error"]["type"],
            serde_json::Value::Null,
            "{path} carried the proxy envelope (type=proxy_error): {v}"
        );
        assert!(
            v["error"]["code"].is_string(),
            "{path} must carry our envelope, which always has a code: {v}"
        );
        assert!(
            v["error"]["trace_id"].is_string(),
            "{path} must carry our envelope, which always has a trace_id: {v}"
        );
    }
}

/// T10: the kill switch restores the baseline exactly — the prefix is no longer
/// intercepted, so the request is handled as an ordinary data-plane path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disabling_the_api_restores_the_proxy_pipeline() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let state = build_state(
        &pool,
        TenantApiConfig {
            enabled: false,
            ..TenantApiConfig::default()
        },
    )
    .await;
    let root = start_proxy(state);
    let (status, v) = body_json(
        send_until_ready(
            &client(),
            &format!("{root}/tenant/t1/api/v1/whoami"),
            Some(TENANT_TOKEN),
            None,
        )
        .await,
    )
    .await;
    // Differential: with the switch off, this path must get EXACTLY what any
    // unknown data-plane path gets — i.e. the pipeline answers, and it is not a
    // status check that could pass for the wrong reason.
    //
    // NOTE on RED: this test cannot fail while the interception is absent, and
    // that is its point — it is the baseline-equivalence half of a pair whose
    // other half (the tests above) proves that with the switch ON the behaviour
    // differs. Recorded rather than forced red.
    let (ctrl_status, ctrl_v) = body_json(
        send_until_ready(
            &client(),
            &format!("{root}/api/v1/definitely-not-a-route"),
            Some(TENANT_TOKEN),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(
        (status, v["error"]["message"].clone()),
        (ctrl_status, ctrl_v["error"]["message"].clone()),
        "with the switch off the reserved prefix must be indistinguishable from any unknown path"
    );
    assert_eq!(
        v["error"]["type"], "proxy_error",
        "the pipeline (not our envelope) must answer when the API is disabled: {v}"
    );
}

/// C18: the data-plane listener must not expose the control plane's internal
/// routes. `/api/v1/internal/*` exists only on the admin service.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn internal_routes_are_not_reachable_on_the_data_plane() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);
    let c = client();
    let (status, v) = body_json(
        send_until_ready(
            &c,
            &format!("{root}/api/v1/internal/control"),
            Some("cluster-token-attempt"),
            None,
        )
        .await,
    )
    .await;
    assert_ne!(status, 200, "internal control must not answer on 8080: {v}");
    // Differential: the internal path must get exactly the treatment any other
    // unknown data-plane path gets. Asserting only "!= 200" passes vacuously if
    // the whole listener is unreachable, which is how this was first written.
    let (other_status, other_v) = body_json(
        send_until_ready(
            &c,
            &format!("{root}/api/v1/definitely-not-a-route"),
            Some("cluster-token-attempt"),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(
        (status, v["error"]["message"].clone()),
        (other_status, other_v["error"]["message"].clone()),
        "an internal path must be indistinguishable from any other unknown path"
    );
    // And no control-plane payload may appear.
    for leak in ["nodes", "lease", "generation", "snapshot"] {
        assert!(
            v.get(leak).is_none() && v["error"].get(leak).is_none(),
            "internal response leaked a control-plane field `{leak}`: {v}"
        );
    }
}

// ---------------------------------------------------------------------------
// T5 — E1 `GET /whoami` (snapshot-only)
// ---------------------------------------------------------------------------

async fn get_whoami(root: &str, tenant: &str, token: &str) -> (u16, Value) {
    body_json(
        send_until_ready(
            &client(),
            &format!("{root}/tenant/{tenant}/api/v1/whoami"),
            Some(token),
            None,
        )
        .await,
    )
    .await
}

/// T1: the snapshot view is complete and its values are the tenant's own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn whoami_returns_the_snapshot_view() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let expected_version = state.store.version();
    let root = start_proxy(state);
    let (status, v) = get_whoami(&root, "t1", TENANT_TOKEN).await;
    assert_eq!(status, 200, "got {v}");
    assert_eq!(v["tenant_id"], "t1");
    assert_eq!(v["name"], "t1-name");
    assert_eq!(v["domain"], "acme.example");
    assert_eq!(v["auth_url"], "https://auth.acme.example/verify");
    assert_eq!(v["enabled"], true);
    assert_eq!(
        v["config_version"].as_u64(),
        Some(expected_version),
        "the version must be the one the gate read, so a tenant can tell whether its change is live"
    );
    assert_eq!(v["base_url"], "/tenant/t1/api/v1");
}

/// T2 (strengthened): the response is the tenant's own NON-SECRET configuration.
/// The token hash, the certificate private key and any provider key must not
/// appear — not even as a key name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn whoami_never_leaks_a_secret() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);
    let (status, v) = get_whoami(&root, "t1", TENANT_TOKEN).await;
    assert_eq!(status, 200, "got {v}");
    let body = v.to_string();
    for forbidden in [
        "access_token",
        "token_hash",
        "cert_key",
        "cert_file",
        "pem",
        "private",
        TENANT_TOKEN,
    ] {
        assert!(
            !body.contains(forbidden),
            "the response must not mention {forbidden:?}: {body}"
        );
    }
    // ...while the two fields it DOES expose are the tenant's own and are not
    // credentials: the domain it is bound to and the auth endpoint Hydra calls.
    assert!(v["domain"].is_string());
    assert!(v["auth_url"].is_string());
}

/// T7 (E1 form): a suspended tenant must still be able to read its own state.
/// Self-recovery depends on it, and E1 changes nothing on the data plane.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn whoami_answers_for_a_disabled_tenant() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let mut t = hydra_server::db::get_tenant(&pool, "t1")
        .await
        .expect("get");
    t.enabled = false;
    hydra_server::db::update_tenant(&pool, &t)
        .await
        .expect("disable");
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);
    let (status, v) = get_whoami(&root, "t1", TENANT_TOKEN).await;
    assert_eq!(
        status, 200,
        "a suspended tenant must still read its own state: {v}"
    );
    assert_eq!(v["enabled"], false, "and must see that it is suspended");
}

/// T23: `base_url` must not be a self-contradiction. Using the value the API
/// reported has to work.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_reported_base_url_is_actually_usable() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);
    let (_, v) = get_whoami(&root, "t1", TENANT_TOKEN).await;
    let base = v["base_url"].as_str().expect("base_url");
    let (status, w) = body_json(
        send_until_ready(
            &client(),
            &format!("{root}{base}/whoami"),
            Some(TENANT_TOKEN),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(status, 200, "the reported base_url must work: {w}");
}

/// C12: an EDGE node has no local database at all. E1 must still answer, from
/// the replicated snapshot — this is the evidence behind "E1 needs no DB and no
/// forwarding", and it is why an edge can serve the whole tenant API locally.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn whoami_works_on_an_edge_from_the_snapshot_alone() {
    use hydra_server::cluster::content::FidelityRows;
    use hydra_server::cluster::snapshot::HydratedWire;

    let kp: Arc<dyn hydra_server::crypto::KeyProvider> =
        Arc::new(StaticKeyProvider::new([7u8; 32], 1));

    // The replica shape: a store with NO pool, fed one snapshot.
    let store = ConfigStore::from_snapshot(hydra_core::config::ConfigData::default(), kp);
    let mut cfg = hydra_core::config::ConfigData::default();
    let t = Tenant {
        id: "t1".into(),
        name: "edge-tenant".into(),
        domain: "edge.example".into(),
        auth_url: "https://auth.edge.example/verify".into(),
        cert_key: None,
        cert_file: None,
        enabled: true,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    };
    cfg.tenants_by_domain.insert(t.domain.clone(), t);
    // `hydrate` does this in production; here we are the producer of the wire.
    cfg.reindex_tenants();
    store.apply_snapshot(HydratedWire {
        version: 7,
        cfg,
        fidelity: FidelityRows {
            limit_roles: vec![],
            key_prefix_bindings: vec![],
            provider_keys: vec![],
            tenant_token_hashes: vec![(
                "t1".to_string(),
                sha256_hex_string(TENANT_TOKEN.as_bytes()),
            )],
            provider_models: vec![],
            tenant_providers: vec![],
            tenant_models: vec![],
            sub_tenants: vec![],
            sub_tenant_routes: vec![],
        },
    });

    let auth = Arc::new(
        HttpAuthChecker::new(
            AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
            AuthConfig::default(),
        )
        .expect("checker"),
    );
    let state = AppState::for_tests(
        store,
        auth,
        Arc::new(CircuitBreaker::new(BreakerConfig::new(5))),
        Arc::new(RateLimiter::new()),
        Arc::new(NoopSink) as Arc<dyn UsageSink>,
        ProxyConfig::default(),
        TenantApiConfig::default(),
    );
    let root = start_proxy(state);
    let (status, v) = get_whoami(&root, "t1", TENANT_TOKEN).await;
    assert_eq!(status, 200, "an edge must serve E1 from its snapshot: {v}");
    assert_eq!(v["tenant_id"], "t1");
    assert_eq!(v["name"], "edge-tenant");
    // The snapshot's version, not a database's.
    assert_eq!(v["config_version"].as_u64(), Some(7));
}

// ---------------------------------------------------------------------------
// T6 — E2 `POST /auth/cache/invalidate`
// ---------------------------------------------------------------------------

async fn post_invalidate(
    root: &str,
    tenant: &str,
    token: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let c = client();
    let url = format!("{root}/tenant/{tenant}/api/v1/auth/cache/invalidate");
    for _ in 0..60 {
        let mut req = c.post(&url).bearer_auth(token);
        if let Some(b) = &body {
            req = req.json(b);
        }
        if let Ok(r) = req.send().await {
            return body_json(r).await;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    panic!("proxy never became ready at {url}");
}

/// C3: a single-node build has no peers, so the local clear IS the whole answer
/// and the report must say exactly that — not "applied" (which implies a fleet
/// confirmed) and not "pending" (which implies someone is behind).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalidate_reports_single_node_when_there_are_no_peers() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);
    let (status, v) = post_invalidate(&root, "t1", TENANT_TOKEN, None).await;
    assert_eq!(status, 200, "got {v}");
    assert_eq!(v["fleet"]["state"], "single_node", "got {v}");
    assert_eq!(v["fleet"]["nodes_total"], 1);
    assert_eq!(
        v["scope"], "tenant",
        "an absent body means the whole tenant: {v}"
    );
    assert_eq!(v["checked"], 0);
}

/// T13: the shape caps. A request that names absurd keys is rejected before any
/// cache work happens.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalidate_rejects_an_oversized_request() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);

    let too_many: Vec<String> = (0..1_001).map(|i| format!("sk-{i}")).collect();
    let (status, v) = post_invalidate(
        &root,
        "t1",
        TENANT_TOKEN,
        Some(serde_json::json!({ "api_keys": too_many })),
    )
    .await;
    assert_eq!(status, 400, "got {v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("api_keys"),
        "got {v}"
    );

    let (status2, v2) = post_invalidate(
        &root,
        "t1",
        TENANT_TOKEN,
        Some(serde_json::json!({ "api_keys": ["x".repeat(4_097)] })),
    )
    .await;
    assert_eq!(status2, 400, "got {v2}");
}

/// T11: a precise key clears that key HERE and forces the next request for it to
/// re-query the tenant's auth service.
///
/// The cache is keyed `(tenant_id, sha256(key))`, so this is the only way to
/// verify the clear took effect: observe that the upstream is asked again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalidate_clears_exactly_the_named_key() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let auth_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/verify"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(r#"{"allowed":true}"#, "application/json"),
        )
        .expect(2) // once for the first request, once after the clear
        .mount(&auth_server)
        .await;

    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.local", Some(TENANT_TOKEN)).await;
    // Point the tenant at the mock auth service.
    let mut t = hydra_server::db::get_tenant(&pool, "t1")
        .await
        .expect("get");
    t.auth_url = format!("{}/verify", auth_server.uri());
    hydra_server::db::update_tenant(&pool, &t)
        .await
        .expect("update");
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state.clone());

    // A client request with key K populates the cache.
    let c = client();
    let chat = format!("{root}/v1/chat/completions");
    let probe = |key: &str| {
        let c = c.clone();
        let url = chat.clone();
        let key = key.to_string();
        async move {
            for _ in 0..60 {
                let r = c
                    .post(&url)
                    .bearer_auth(&key)
                    .header("host", "acme.local")
                    .json(&serde_json::json!({"model": "nope", "messages": []}))
                    .send()
                    .await;
                if let Ok(r) = r {
                    return r.status().as_u16();
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            panic!("never ready");
        }
    };
    let _ = probe("sk-key-to-clear").await;
    assert!(
        !state.auth.cache().is_empty(),
        "the first request must have cached a verdict"
    );

    let before = state.auth.cache().len();
    let (status, v) = post_invalidate(
        &root,
        "t1",
        TENANT_TOKEN,
        Some(serde_json::json!({ "api_keys": ["sk-key-to-clear"] })),
    )
    .await;
    assert_eq!(status, 200, "got {v}");
    assert_eq!(v["checked"], 1, "got {v}");
    assert_eq!(v["scope"], "keys", "got {v}");
    assert_eq!(
        v["invalidated"], 1,
        "the named key was cached, so exactly one entry must go: {v}"
    );
    assert!(state.auth.cache().len() < before, "the cache must shrink");

    // And the next request for that key goes upstream again (the mock's
    // `.expect(2)` is asserted when it is dropped).
    let _ = probe("sk-key-to-clear").await;
}

/// The per-tenant invalidate throttle is checked BEFORE the local cache clear,
/// and that order is the security property: a throttled call must NOT clear the
/// cache. If the clear were reordered ahead of the throttle check, a banned key
/// would be re-served (cleared) by a request the tenant was not even allowed to
/// make — the throttle would be a no-op. This pins the order end to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_throttled_invalidate_does_not_clear_the_cache() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let cfg = TenantApiConfig {
        // A budget of one: the second invalidate in the window is refused.
        invalidate_per_min: 1,
        ..TenantApiConfig::default()
    };
    let state = build_state(&pool, cfg).await;
    let root = start_proxy(state.clone());

    // Seed a cached verdict so there is something real to clear. The direct
    // seam is the same one `a_suspended_tenant_can_still_use_all_three_endpoints`
    // uses: `state.auth.cache()`.
    state
        .auth
        .cache()
        .set("t1", "sk-victim", true, Duration::from_secs(300))
        .await;
    assert_eq!(state.auth.cache().len(), 1, "the seeded verdict is cached");

    // First invalidate: allowed (budget of one). It clears the entry.
    let (s1, v1) = post_invalidate(&root, "t1", TENANT_TOKEN, None).await;
    assert_eq!(s1, 200, "the first invalidate must succeed: {v1}");
    assert!(
        state.auth.cache().is_empty(),
        "the first invalidate cleared it"
    );

    // Re-seed the same verdict: it is back in the L1, and the second call is
    // about to be judged against the (already spent) invalidate budget.
    state
        .auth
        .cache()
        .set("t1", "sk-victim", true, Duration::from_secs(300))
        .await;
    assert_eq!(
        state.auth.cache().check("t1", "sk-victim").await,
        hydra_core::auth::Verdict::Hit(true),
        "the re-seeded verdict is cached"
    );

    // Second invalidate, same tenant, same minute: throttled. The raw response
    // is needed for the `Retry-After` header, so it is not sent through
    // `post_invalidate` (which discards headers).
    let c = client();
    let url = format!("{root}/tenant/t1/api/v1/auth/cache/invalidate");
    let resp = c
        .post(&url)
        .bearer_auth(TENANT_TOKEN)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("send");
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let text = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);

    assert_eq!(status, 429, "the second invalidate must be throttled: {v}");
    assert_eq!(v["error"]["code"], "rate_limited", "got {v}");
    assert!(
        retry_after.as_deref().is_some_and(|r| !r.is_empty()),
        "a 429 must carry a Retry-After header: {text}"
    );

    // THE ordering guard: the throttled call never reached the local clear, so the
    // re-seeded verdict is still cached. A reorder (clear before the throttle
    // check) would have dropped it.
    assert_eq!(
        state.auth.cache().check("t1", "sk-victim").await,
        hydra_core::auth::Verdict::Hit(true),
        "a throttled invalidate must NOT clear the cache (throttle check precedes the clear): {text}"
    );
}

/// A GET on the invalidation route must not clear anything: a prefetching client
/// or a browser address bar is not a tenant action.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalidate_is_post_only() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);
    let (status, v) = body_json(
        send_until_ready(
            &client(),
            &format!("{root}/tenant/t1/api/v1/auth/cache/invalidate"),
            Some(TENANT_TOKEN),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(status, 405, "got {v}");
    assert_eq!(v["error"]["code"], "method_not_allowed", "got {v}");
}

// ---------------------------------------------------------------------------
// T21 — failure limiting and lockout (design §5.1)
// ---------------------------------------------------------------------------

/// A data-plane listener is internet-facing, so the gate needs a budget. Before
/// this, an unauthenticated caller could drive it as fast as it could open
/// connections — and each attempt, if it ever succeeded, was a cross-tenant
/// compromise.
///
/// Asserted end to end because the ORDER is the security property: the lockout
/// must be checked before the gate (so it is cheap) and it must answer `429`
/// rather than `401` (so a locked-out guesser cannot tell a real token from a
/// guessed one).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_bad_tokens_are_throttled_then_locked_out() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let cfg = TenantApiConfig {
        // A budget small enough to reach inside a test.
        auth_fail_limit_per_min: 3,
        lockout_secs: 60,
        ..TenantApiConfig::default()
    };
    let state = build_state(&pool, cfg).await;
    let root = start_proxy(state);
    let c = client();
    let url = format!("{root}/tenant/t1/api/v1/whoami");

    let mut first_429 = None;
    for i in 0..8 {
        let r = send_until_ready(&c, &url, Some("sk-wrong-token-000000000000"), None).await;
        let status = r.status().as_u16();
        if status == 429 {
            first_429 = Some(i);
            let retry = r
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            assert!(!retry.is_empty(), "a 429 must carry Retry-After");
            let body = r.text().await.unwrap_or_default();
            assert!(
                body.contains("rate_limited"),
                "the envelope must say why: {body}"
            );
            // The message names the DIMENSION, which is what an operator needs
            // to tell "one host is guessing" from "one token is being probed".
            assert!(
                body.contains("ip") || body.contains("token"),
                "the refused dimension must be named: {body}"
            );
            break;
        }
        assert_eq!(status, 401, "below the budget the answer is still 401");
    }
    assert!(
        first_429.is_some(),
        "8 failures against a budget of 3 must eventually be refused"
    );

    // The lockout still refuses FURTHER FAILURES: another bad token is 429 (the
    // lockout has not leaked a 401 that would tell a guesser the token "wasn't
    // found").
    let r = send_until_ready(&c, &url, Some("sk-still-wrong-0000000000"), None).await;
    assert_eq!(
        r.status().as_u16(),
        429,
        "the failure lockout must keep refusing further failures"
    );

    // ...and, under B1, a request that presents a VALID token is never refused
    // by the failure lockout: the lockout only ever stops *failed*
    // authentications. (Before B1 this asserted 429 — the cross-tenant blackout
    // the pre-gate lockout caused behind a shared LB.)
    let r = send_until_ready(&c, &url, Some(TENANT_TOKEN), None).await;
    assert_eq!(
        r.status().as_u16(),
        200,
        "a valid token must never be refused by the failure lockout (B1)"
    );
}

/// C: with a trusted proxy configured, the failure limiter keys on the XFF
/// client, NOT the shared peer. The test client's peer is 127.0.0.1; declaring it
/// trusted makes the node read `X-Forwarded-For`. Drive one XFF client past its
/// budget until the lockout trips, then a DIFFERENT XFF client (same peer) is
/// still fresh — which is only possible if the bucket is the XFF, not the peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_trusted_proxy_keys_the_limiter_on_the_xff_client_not_the_peer() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let cfg = TenantApiConfig {
        // A budget small enough to reach inside a test.
        auth_fail_limit_per_min: 3,
        lockout_secs: 60,
        // The test client's peer (127.0.0.1) is a trusted proxy, so XFF is
        // honoured and the limiter keys on it.
        trusted_proxies: hydra_server::tenant_api::trusted_proxy::parse_trusted_proxies(
            "127.0.0.1/32",
        )
        .expect("trusted proxy"),
        ..TenantApiConfig::default()
    };
    let state = build_state(&pool, cfg).await;
    let root = start_proxy(state);
    let c = client();
    let url = format!("{root}/tenant/t1/api/v1/whoami");

    // Drive the "1.1.1.1" bucket (the XFF client) past its budget. A FRESH token
    // each request, so ONLY the XFF-client (IP) dimension accumulates — the token
    // dimension can never lock (each token is distinct).
    let mut first_429 = false;
    for i in 0..8 {
        let tok = format!("sk-wrong-a-{i:04}");
        let r = send_with_xff(&c, &url, Some(&tok), Some("1.1.1.1")).await;
        let status = r.status().as_u16();
        if status == 429 {
            first_429 = true;
            break;
        }
        assert_eq!(status, 401, "below the budget the answer is still 401");
    }
    assert!(
        first_429,
        "8 failures from one XFF client must trip the lockout"
    );

    // A different XFF client (same peer), fresh token: NOT locked. If the limiter
    // keyed on the peer 127.0.0.1, this would be 429.
    let r = send_with_xff(&c, &url, Some("sk-wrong-b-0000"), Some("2.2.2.2")).await;
    assert_eq!(
        r.status().as_u16(),
        401,
        "a different XFF client must not inherit the first client's lockout \
         (the limiter keys on the XFF, not the peer)"
    );
}

/// C: with NO trusted proxies, `X-Forwarded-For` is ignored — every failure
/// counts against the shared peer (the LB), so different XFF values still lock the
/// SAME peer. This is the conservative default and the whole reason a trusted
/// proxy must be configured to key on the real client.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_untrusted_peer_ignores_xff_and_locks_the_shared_peer() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let cfg = TenantApiConfig {
        // A budget small enough to reach inside a test.
        auth_fail_limit_per_min: 3,
        lockout_secs: 60,
        // No trusted proxies: XFF is ignored, so every failure is the peer.
        trusted_proxies: Vec::new(),
        ..TenantApiConfig::default()
    };
    let state = build_state(&pool, cfg).await;
    let root = start_proxy(state);
    let c = client();
    let url = format!("{root}/tenant/t1/api/v1/whoami");

    // Two different XFF values (same token would also work, but a fresh token each
    // request keeps ONLY the peer/IP dimension accumulating, which is the point).
    let mut first_429 = false;
    for i in 0..8 {
        let tok = format!("sk-wrong-peer-{i:04}");
        let xff = if i % 2 == 0 {
            Some("9.9.9.9")
        } else {
            Some("8.8.8.8")
        };
        let r = send_with_xff(&c, &url, Some(&tok), xff).await;
        let status = r.status().as_u16();
        if status == 429 {
            first_429 = true;
            break;
        }
        assert_eq!(status, 401, "below the budget the answer is still 401");
    }
    assert!(
        first_429,
        "8 failures across two XFF values must trip the shared-peer lockout"
    );

    // A third XFF value (same peer) is locked too: XFF is ignored, so all of them
    // share the peer's bucket.
    let r = send_with_xff(&c, &url, Some("sk-wrong-peer-0000"), Some("7.7.7.7")).await;
    assert_eq!(
        r.status().as_u16(),
        429,
        "with no trusted proxy, every XFF shares the peer's lockout"
    );
}

/// N-2: when a trusted proxy appends the real client as a SEPARATE header line
/// (rather than merging into one comma-joined value), the server must collect
/// ALL `X-Forwarded-For` lines so that a client-forged first line cannot shadow
/// the proxy-appended real client.
///
/// Non-vacuousness: if reqwest emitted only one header line (or the server read
/// only the first line), the limiter would key on the rotating forged IP and
/// never accumulate to the budget — the 429 assertion below would fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_line_xff_keys_on_the_proxy_appended_line_not_the_forged_first_line() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let cfg = TenantApiConfig {
        auth_fail_limit_per_min: 3,
        lockout_secs: 60,
        trusted_proxies: hydra_server::tenant_api::trusted_proxy::parse_trusted_proxies(
            "127.0.0.1/32",
        )
        .expect("trusted proxy"),
        ..TenantApiConfig::default()
    };
    let state = build_state(&pool, cfg).await;
    let root = start_proxy(state);
    let c = client();
    let url = format!("{root}/tenant/t1/api/v1/whoami");

    // The FIXED real-client IP in the second (proxy-appended) line. This is
    // the IP the limiter must key on.
    let real_client = "203.0.113.7";

    // Drive the real_client bucket past its budget. The first (forged) line
    // rotates per request — if the server keyed on it, each request would be a
    // fresh bucket and no 429 would ever appear. The second line is fixed, so
    // the limiter accumulates on real_client.
    let mut first_429 = false;
    for i in 0..8 {
        let tok = format!("sk-wrong-a-{i:04}");
        let forged = format!("10.0.0.{i}");
        let r = send_with_xff_two_lines(&c, &url, Some(&tok), &forged, real_client).await;
        let status = r.status().as_u16();
        if status == 429 {
            first_429 = true;
            break;
        }
        assert_eq!(
            status, 401,
            "below the budget the answer is still 401 (request {i}, forged={forged})"
        );
    }
    assert!(
        first_429,
        "8 failures with a fixed second-line IP must trip the lockout \
         (if only the first line were read, the rotating forged IP would never \
         accumulate to the budget)"
    );

    // After the lockout: a DIFFERENT forged first line but the SAME second line
    // is still locked — proving the key is the second line, not the first.
    let r =
        send_with_xff_two_lines(&c, &url, Some("sk-wrong-b-0000"), "10.0.0.200", real_client).await;
    assert_eq!(
        r.status().as_u16(),
        429,
        "a different forged first line but the same second line must still be locked"
    );

    // A DIFFERENT second line (same peer) is a fresh bucket: NOT locked.
    let r = send_with_xff_two_lines(
        &c,
        &url,
        Some("sk-wrong-c-0000"),
        "10.0.0.99",
        "198.51.100.1",
    )
    .await;
    assert_eq!(
        r.status().as_u16(),
        401,
        "a different second-line IP must be a fresh bucket (not locked)"
    );
}

/// The success budget is per tenant and independent of the failure budget: an
/// attacker burning its own failures must not consume the budget a tenant's real
/// clients depend on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_per_tenant_success_budget_is_enforced_and_is_not_spent_by_failures() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let cfg = TenantApiConfig {
        rate_limit_per_min: 2,
        auth_fail_limit_per_min: 100,
        ..TenantApiConfig::default()
    };
    let state = build_state(&pool, cfg).await;
    let root = start_proxy(state);
    let c = client();
    let url = format!("{root}/tenant/t1/api/v1/whoami");

    // A pile of failures first, with a HIGH failure budget so nothing is locked.
    for _ in 0..5 {
        let r = send_until_ready(&c, &url, Some("sk-wrong-token-000000000000"), None).await;
        assert_eq!(r.status().as_u16(), 401, "no lockout at this budget");
    }

    // The successful budget is still intact: 2 successes, then 429.
    let (s1, _) = body_json(send_until_ready(&c, &url, Some(TENANT_TOKEN), None).await).await;
    let (s2, _) = body_json(send_until_ready(&c, &url, Some(TENANT_TOKEN), None).await).await;
    let (s3, v3) = body_json(send_until_ready(&c, &url, Some(TENANT_TOKEN), None).await).await;
    assert_eq!(s1, 200, "{v3}");
    assert_eq!(s2, 200, "two successes fit the budget: {v3}");
    assert_eq!(s3, 429, "the third exceeds it: {v3}");
    assert_eq!(v3["error"]["code"], "rate_limited", "{v3}");
}

/// The counters are labelled so an operator can act, and the token itself is
/// never one of the labels.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_throttled_request_is_counted_and_a_bad_token_is_classified() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let cfg = TenantApiConfig {
        rate_limit_per_min: 1,
        auth_fail_limit_per_min: 100,
        ..TenantApiConfig::default()
    };
    let state = build_state(&pool, cfg).await;
    let root = start_proxy(state);
    let c = client();
    let url = format!("{root}/tenant/t1/api/v1/whoami");

    let unknown_before = hydra_server::admin::metrics::tenant_api_auth_failures_total("unknown");
    let missing_before = hydra_server::admin::metrics::tenant_api_auth_failures_total("missing");
    let throttled_before = hydra_server::admin::metrics::tenant_api_throttled_total("tenant");

    // One classified failure of each kind...
    let _ = send_until_ready(&c, &url, Some("sk-wrong-token-000000000000"), None).await;
    let _ = send_until_ready(&c, &url, None, None).await;
    // ...then spend the success budget and get throttled.
    let _ = send_until_ready(&c, &url, Some(TENANT_TOKEN), None).await;
    let (status, _) = body_json(send_until_ready(&c, &url, Some(TENANT_TOKEN), None).await).await;
    assert_eq!(status, 429);

    assert!(
        hydra_server::admin::metrics::tenant_api_auth_failures_total("unknown") > unknown_before,
        "a wrong token must be classified as `unknown`"
    );
    assert!(
        hydra_server::admin::metrics::tenant_api_auth_failures_total("missing") > missing_before,
        "no credential must be classified as `missing`"
    );
    assert!(
        hydra_server::admin::metrics::tenant_api_throttled_total("tenant") > throttled_before,
        "the refusal must be attributed to the tenant dimension"
    );
}

/// The success budget meters AUTHORISED work: a client whose base URL names the
/// wrong tenant must not be able to burn its own tenant's quota. Otherwise a
/// single misconfigured client locks its tenant out of its own API.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wrong_tenant_in_the_url_does_not_spend_the_success_budget() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    seed_tenant(&pool, "t2", "other.example", Some(OTHER_TOKEN)).await;
    let cfg = TenantApiConfig {
        rate_limit_per_min: 2,
        ..TenantApiConfig::default()
    };
    let state = build_state(&pool, cfg).await;
    let root = start_proxy(state);
    let c = client();

    // Five rejected cross-checks: t1's token against t2's URL.
    for _ in 0..5 {
        let r = send_until_ready(
            &c,
            &format!("{root}/tenant/t2/api/v1/whoami"),
            Some(TENANT_TOKEN),
            None,
        )
        .await;
        assert_eq!(r.status().as_u16(), 403, "the cross-check must refuse");
    }

    // t1's own calls still have their full budget.
    let url = format!("{root}/tenant/t1/api/v1/whoami");
    let (s1, _) = body_json(send_until_ready(&c, &url, Some(TENANT_TOKEN), None).await).await;
    let (s2, _) = body_json(send_until_ready(&c, &url, Some(TENANT_TOKEN), None).await).await;
    assert_eq!(s1, 200, "a 403 must not consume the budget");
    assert_eq!(s2, 200);
}

/// The two `read_body` failures used to hand back hand-written static JSON with
/// no `trace_id` — the one field that ties a rejection to the log line that
/// explains it, and which this API's envelope promises on EVERY response.
///
/// The body cap is the reason the cap exists at all: the gate runs before the
/// body is read, so a caller that already holds a token can otherwise make the
/// node buffer arbitrarily.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_oversized_body_is_413_in_the_documented_envelope() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);
    let c = client();
    let url = format!("{root}/tenant/t1/api/v1/auth/cache/invalidate");

    // 2 MiB: one key far past the 1 MiB body cap.
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
    let header_trace = r
        .headers()
        .get("X-Hydra-Trace-Id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let text = r.text().await.unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);

    assert_eq!(status, 413, "the body cap must fire: {text}");
    assert_eq!(v["error"]["code"], "payload_too_large", "{text}");
    assert!(
        v["error"]["trace_id"].is_string(),
        "the envelope must carry trace_id (this 413 body used to omit it): {text}"
    );
    assert_eq!(
        header_trace.as_deref(),
        v["error"]["trace_id"].as_str(),
        "the header and the body must agree on the trace id: {text}"
    );
}

// ---------------------------------------------------------------------------
// E2's wait / timeout_ms parameters (design §4.2.2, Q10)
// ---------------------------------------------------------------------------

async fn post_invalidate_query(root: &str, tenant: &str, token: &str, query: &str) -> (u16, Value) {
    let c = client();
    let url = format!("{root}/tenant/{tenant}/api/v1/auth/cache/invalidate?{query}");
    for _ in 0..60 {
        if let Ok(r) = c
            .post(&url)
            .bearer_auth(token)
            .json(&serde_json::json!({}))
            .send()
            .await
        {
            return body_json(r).await;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    panic!("proxy never became ready at {url}");
}

/// `wait=none` is **accepted** on a non-cluster build: there is no convergence
/// stream here, so the answer is `200` with `single_node` — the same answer the
/// default `wait` produces in this build. What this test pins is that the
/// parameter is PARSED (a misspelt value is refused, see below), not merely
/// tolerated.
///
/// The real `202` "published, fleet still catching up" path is exercised at the
/// `broadcast_and_confirm` level in `crates/hydra-server/tests/tenant_api_cluster.rs`,
/// where an actual convergence stream exists.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_none_is_accepted_and_is_single_node_in_a_non_cluster_build() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);

    let (status, v) = post_invalidate_query(&root, "t1", TENANT_TOKEN, "wait=none").await;
    // No stream in this build ⇒ `single_node`; that is the same answer the
    // default produces here, which is why this test also pins the 400s below
    // (the parameters must be PARSED, not merely tolerated).
    assert_eq!(status, 200, "got {v}");
    assert_eq!(v["fleet"]["state"], "single_node", "got {v}");
}

/// A misspelt `wait` must be refused, not silently treated as the default: a
/// caller that asked to skip the wait and got one (or vice versa) has been lied
/// to about the only thing these parameters control.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_wait_value_is_rejected_rather_than_defaulted() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);

    let (status, v) = post_invalidate_query(&root, "t1", TENANT_TOKEN, "wait=eventually").await;
    assert_eq!(status, 400, "got {v}");
    assert_eq!(v["error"]["code"], "invalid_wait", "got {v}");

    // The explicit default is accepted.
    let (status, v) =
        post_invalidate_query(&root, "t1", TENANT_TOKEN, "wait=converged&timeout_ms=1500").await;
    assert_eq!(status, 200, "got {v}");
}

/// `timeout_ms` is clamped at both ends: `0` would mean "do not wait" while
/// claiming to wait, and an unbounded value would let one caller hold a worker
/// (and a share of the shared Redis) indefinitely.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_out_of_range_timeout_ms_is_rejected() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);

    for bad in ["0", "60001", "abc", "-5"] {
        let (status, v) =
            post_invalidate_query(&root, "t1", TENANT_TOKEN, &format!("timeout_ms={bad}")).await;
        assert_eq!(status, 400, "timeout_ms={bad} got {v}");
        assert_eq!(
            v["error"]["code"], "invalid_timeout_ms",
            "timeout_ms={bad}: {v}"
        );
    }

    let (status, v) = post_invalidate_query(&root, "t1", TENANT_TOKEN, "timeout_ms=1").await;
    assert_eq!(status, 200, "the boundary value 1 must be accepted: {v}");
}

/// A SUSPENDED tenant (欠费停机) must still be able to run its own recovery: read
/// its state, force a re-authentication after a top-up, and check what it has
/// used. The gate never consults `enabled` — suspension is the tenant's own
/// policy, expressed through its `auth_url`, and a tenant that cannot reach the
/// recovery path cannot recover.
///
/// E1 alone was covered; this pins all three, because the contract document for
/// tenants states all three and a document is only as good as its test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_suspended_tenant_can_still_use_all_three_endpoints() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let mut t = hydra_server::db::get_tenant(&pool, "t1")
        .await
        .expect("get");
    t.enabled = false;
    hydra_server::db::update_tenant(&pool, &t)
        .await
        .expect("disable");
    // A cached allow, so E2 has something real to clear.
    let state = build_state(&pool, TenantApiConfig::default()).await;
    state
        .auth
        .cache()
        .set("t1", "sk-cached", true, Duration::from_secs(300))
        .await;
    let root = start_proxy(state);
    let c = client();

    let _ = &c;
    let (s1, v1) = get_tenant_json(&root, "t1", TENANT_TOKEN, "whoami").await;
    assert_eq!(s1, 200);
    assert_eq!(v1["enabled"], false, "suspended, and it can see that: {v1}");

    let (s2, v2) = post_invalidate(&root, "t1", TENANT_TOKEN, None).await;
    assert_eq!(
        s2, 200,
        "a suspended tenant must be able to force re-auth: {v2}"
    );
    assert!(
        v2["invalidated"].as_u64().unwrap_or(0) >= 1,
        "the cached allow must actually be gone: {v2}"
    );

    let (s3, v3) = get_tenant_json(
        &root,
        "t1",
        TENANT_TOKEN,
        "usage?since=2026-09-01T00:00:00Z&until=2026-09-02T00:00:00Z",
    )
    .await;
    assert_eq!(s3, 200, "and must be able to read its own usage: {v3}");
    assert_eq!(v3["source"], "sqlite", "{v3}");
}

/// A raw GET against a tenant path, for the cases that need a URL the
/// convenience helpers do not build (a query string, a suspended tenant).
async fn get_tenant_json(root: &str, tenant: &str, token: &str, suffix: &str) -> (u16, Value) {
    let c = client();
    let url = format!("{root}/tenant/{tenant}/api/v1/{suffix}");
    for _ in 0..60 {
        match c.get(&url).bearer_auth(token).send().await {
            Ok(r) => return body_json(r).await,
            Err(_) => tokio::time::sleep(Duration::from_millis(150)).await,
        }
    }
    panic!("proxy never became ready at {url}");
}

// ---------------------------------------------------------------------------
// T7 — E4 `GET /sub-tenants` and `GET /sub-tenant-routes`
//       (read-only, snapshot-fed; an edge with no DB serves them)
// ---------------------------------------------------------------------------

/// Seed a provider plus sub-tenants and routes for BOTH `t1` and `t2`, so the
/// isolation assertions below are non-vacuous: each tenant has rows of its own
/// AND rows belonging to the other tenant that it must never see.
async fn seed_sub_tenants(pool: &sqlx::SqlitePool) {
    hydra_server::db::insert_provider(
        pool,
        &hydra_core::model::Provider {
            id: "p1".into(),
            key: "prov1".into(),
            name: "P1".into(),
            endpoint: "http://127.0.0.1:1/".into(),
            weight: 1,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            max_concurrency: None,
            max_queue_depth: None,
            queue_wait_timeout_ms: None,
        },
    )
    .await
    .expect("insert provider");

    for (id, tenant, name, prefix) in [
        ("st1", "t1", "st-one", "QQCX_"),
        ("st2", "t1", "st-two", "QQCY_"),
        ("st9", "t2", "st-other", "ZZZZ_"),
    ] {
        hydra_server::db::insert_sub_tenant(
            pool,
            &hydra_core::model::SubTenant {
                id: id.into(),
                tenant_id: tenant.into(),
                name: name.into(),
                key_prefix: prefix.into(),
                enabled: true,
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
            },
        )
        .await
        .expect("insert sub-tenant");
    }

    // `st1` (t1) has two routes: one model-specific and the default (`None`).
    // `st9` (t2) has one. `st2` (t1) has none.
    for (id, st, model) in [
        ("r1", "st1", Some("gpt-4".to_string())),
        ("r2", "st1", None),
        ("r9", "st9", Some("gpt-4".to_string())),
    ] {
        hydra_server::db::insert_sub_tenant_route(
            pool,
            &hydra_core::model::SubTenantRoute {
                id: id.into(),
                sub_tenant_id: st.into(),
                model_key: model,
                provider_id: "p1".into(),
                enabled: true,
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
            },
        )
        .await
        .expect("insert sub-tenant route");
    }
}

/// T7: `GET /sub-tenants` answers from the snapshot (no DB), returns only the
/// tenant's own rows, and carries the snapshot `config_version` as the v2
/// reconciliation baseline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_sub_tenants_returns_only_the_tenants_own_rows() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    seed_tenant(&pool, "t2", "other.example", Some(OTHER_TOKEN)).await;
    seed_sub_tenants(&pool).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let expected_version = state.store.version();
    let root = start_proxy(state);
    let (status, v) = get_tenant_json(&root, "t1", TENANT_TOKEN, "sub-tenants").await;
    assert_eq!(status, 200, "got {v}");
    // The version the snapshot was read at — the reconciliation baseline a
    // tenant polls to see when its change is live.
    assert_eq!(
        v["config_version"].as_u64(),
        Some(expected_version),
        "the response must carry the snapshot version: {v}"
    );
    // t1 sees exactly its two sub-tenants, never t2's.
    let arr = v["sub_tenants"].as_array().expect("sub_tenants array");
    assert_eq!(arr.len(), 2, "got {v}");
    let names: Vec<&str> = arr
        .iter()
        .map(|x| x["name"].as_str().unwrap_or_default())
        .collect();
    assert!(
        names.contains(&"st-one") && names.contains(&"st-two"),
        "got {names:?}"
    );
    assert!(
        names.iter().all(|n| *n != "st-other"),
        "t2's sub-tenant leaked into t1's view: {v}"
    );
}

/// T7: `GET /sub-tenant-routes` answers from the snapshot and returns only the
/// routes whose sub-tenant belongs to the caller's tenant. A route is
/// tenant-scoped THROUGH its sub-tenant, so another tenant's route can never
/// appear — even though the route row itself names no `tenant_id`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_sub_tenant_routes_returns_only_the_tenants_own_routes() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    seed_tenant(&pool, "t2", "other.example", Some(OTHER_TOKEN)).await;
    seed_sub_tenants(&pool).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let expected_version = state.store.version();
    let root = start_proxy(state);
    let (status, v) = get_tenant_json(&root, "t1", TENANT_TOKEN, "sub-tenant-routes").await;
    assert_eq!(status, 200, "got {v}");
    assert_eq!(
        v["config_version"].as_u64(),
        Some(expected_version),
        "the response must carry the snapshot version: {v}"
    );
    // t1's sub-tenant `st1` has two routes (one model-specific, one default);
    // `st2` has none; t2's `r9` (via `st9`) must NOT appear.
    let arr = v["sub_tenant_routes"]
        .as_array()
        .expect("sub_tenant_routes array");
    assert_eq!(arr.len(), 2, "got {v}");
    let sub_ids: Vec<&str> = arr
        .iter()
        .map(|x| x["sub_tenant_id"].as_str().unwrap_or_default())
        .collect();
    assert!(
        sub_ids.iter().all(|s| *s == "st1"),
        "only t1's own sub-tenant's routes may appear: {v}"
    );
    assert!(
        arr.iter().any(|x| x["model_key"].is_null()),
        "the default route must be present: {v}"
    );
    assert!(
        arr.iter()
            .any(|x| x["model_key"] == serde_json::json!("gpt-4")),
        "the model-specific route must be present: {v}"
    );
}

/// T7: like every tenant endpoint, both read-only endpoints sit behind the same
/// token gate — no token is a 401 from the gate, never a fall-through to the
/// proxy pipeline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_read_only_endpoints_require_a_token() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);
    let c = client();
    for endpoint in ["sub-tenants", "sub-tenant-routes"] {
        let (status, v) = body_json(
            send_until_ready(
                &c,
                &format!("{root}/tenant/t1/api/v1/{endpoint}"),
                None,
                None,
            )
            .await,
        )
        .await;
        assert_eq!(status, 401, "{endpoint}: got {v}");
        assert_eq!(v["error"]["code"], "unauthorized", "{endpoint}: {v}");
    }
}

/// T7: the whole security property of the read-only view. A tenant sees ONLY
/// its own sub-tenants and routes; `t2` must not read `t1`'s rows, and the gate
/// still enforces the URL cross-check (`t1`'s token on `t2`'s URL is a 403).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_read_only_view_is_scoped_to_the_authenticated_tenant() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    seed_tenant(&pool, "t2", "other.example", Some(OTHER_TOKEN)).await;
    seed_sub_tenants(&pool).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);

    // t2 sees exactly its own sub-tenant (st-other) — never t1's two.
    let (s, v) = get_tenant_json(&root, "t2", OTHER_TOKEN, "sub-tenants").await;
    assert_eq!(s, 200, "got {v}");
    let arr = v["sub_tenants"].as_array().expect("array");
    assert_eq!(arr.len(), 1, "t2 must see only its own sub-tenant: {v}");
    assert_eq!(arr[0]["name"], "st-other");
    assert_eq!(arr[0]["tenant_id"], "t2", "the row must belong to t2: {v}");

    // t2 sees exactly its own route (r9, via st9) — never t1's routes.
    let (s2, v2) = get_tenant_json(&root, "t2", OTHER_TOKEN, "sub-tenant-routes").await;
    assert_eq!(s2, 200, "got {v2}");
    let arr2 = v2["sub_tenant_routes"].as_array().expect("array");
    assert_eq!(arr2.len(), 1, "t2 must see only its own route: {v2}");
    assert_eq!(arr2[0]["sub_tenant_id"], "st9");
    assert_eq!(arr2[0]["id"], "r9");

    // The gate still enforces the URL cross-check: t1's token on t2's URL is 403.
    let (s3, v3) = get_tenant_json(&root, "t2", TENANT_TOKEN, "sub-tenants").await;
    assert_eq!(s3, 403, "a token for another tenant must be 403: {v3}");
    assert_eq!(v3["error"]["code"], "tenant_id_mismatch", "got {v3}");
}

/// T7: a suspended (disabled) tenant still reads its own sub-tenant view —
/// self-recovery depends on it, exactly as for `whoami`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_suspended_tenant_can_still_list_its_sub_tenants() {
    let pool = common::setup_pool().await;
    // `t2` is seeded so the fixture's cross-tenant rows have their tenant to
    // reference (the assertion is purely about the suspended `t1`).
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    seed_tenant(&pool, "t2", "other.example", Some(OTHER_TOKEN)).await;
    seed_sub_tenants(&pool).await;
    let mut t = hydra_server::db::get_tenant(&pool, "t1")
        .await
        .expect("get");
    t.enabled = false;
    hydra_server::db::update_tenant(&pool, &t)
        .await
        .expect("disable");
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);
    let (status, v) = get_tenant_json(&root, "t1", TENANT_TOKEN, "sub-tenants").await;
    assert_eq!(
        status, 200,
        "a suspended tenant must still read its own state: {v}"
    );
    let arr = v["sub_tenants"].as_array().expect("array");
    assert_eq!(
        arr.len(),
        2,
        "the two enabled sub-tenants are still listed: {v}"
    );
}

/// T7: an EDGE node has no local database. The two read-only endpoints must
/// still answer, from the replicated snapshot alone — the same property that
/// makes `whoami` work on an edge, and the whole point of "snapshot-fed": an
/// edge with no DB serves the tenant's own sub-tenant view locally.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_read_only_endpoints_work_on_an_edge_from_the_snapshot_alone() {
    use hydra_server::cluster::content::FidelityRows;
    use hydra_server::cluster::snapshot::HydratedWire;

    let kp: Arc<dyn hydra_server::crypto::KeyProvider> =
        Arc::new(StaticKeyProvider::new([7u8; 32], 1));

    // The replica shape: a store with NO pool, fed one snapshot.
    let store = ConfigStore::from_snapshot(hydra_core::config::ConfigData::default(), kp);
    let mut cfg = hydra_core::config::ConfigData::default();
    let t = Tenant {
        id: "t1".into(),
        name: "edge-tenant".into(),
        domain: "edge.example".into(),
        auth_url: "https://auth.edge.example/verify".into(),
        cert_key: None,
        cert_file: None,
        enabled: true,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    };
    cfg.tenants_by_domain.insert(t.domain.clone(), t);
    let st = hydra_core::model::SubTenant {
        id: "st1".into(),
        tenant_id: "t1".into(),
        name: "edge-sub".into(),
        key_prefix: "QQCX_".into(),
        enabled: true,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    };
    cfg.sub_tenants.push(st.clone());
    let route = hydra_core::model::SubTenantRoute {
        id: "r1".into(),
        sub_tenant_id: "st1".into(),
        model_key: None, // the default route
        provider_id: "p1".into(),
        enabled: true,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    };
    cfg.sub_tenant_routes.push(route.clone());
    // `hydrate` does this in production; here we are the producer of the wire.
    cfg.reindex_tenants();
    store.apply_snapshot(HydratedWire {
        version: 9,
        cfg,
        fidelity: FidelityRows {
            limit_roles: vec![],
            key_prefix_bindings: vec![],
            provider_keys: vec![],
            tenant_token_hashes: vec![(
                "t1".to_string(),
                sha256_hex_string(TENANT_TOKEN.as_bytes()),
            )],
            provider_models: vec![],
            tenant_providers: vec![],
            tenant_models: vec![],
            sub_tenants: vec![st],
            sub_tenant_routes: vec![route],
        },
    });

    let auth = Arc::new(
        HttpAuthChecker::new(
            AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
            AuthConfig::default(),
        )
        .expect("checker"),
    );
    let state = AppState::for_tests(
        store,
        auth,
        Arc::new(CircuitBreaker::new(BreakerConfig::new(5))),
        Arc::new(RateLimiter::new()),
        Arc::new(NoopSink) as Arc<dyn UsageSink>,
        ProxyConfig::default(),
        TenantApiConfig::default(),
    );
    let root = start_proxy(state);

    // The edge serves both read-only endpoints from its snapshot alone.
    let (s1, v1) = get_tenant_json(&root, "t1", TENANT_TOKEN, "sub-tenants").await;
    assert_eq!(
        s1, 200,
        "an edge must serve /sub-tenants from its snapshot: {v1}"
    );
    assert_eq!(
        v1["config_version"].as_u64(),
        Some(9),
        "the snapshot version: {v1}"
    );
    let arr1 = v1["sub_tenants"].as_array().expect("array");
    assert_eq!(arr1.len(), 1);
    assert_eq!(arr1[0]["name"], "edge-sub");

    let (s2, v2) = get_tenant_json(&root, "t1", TENANT_TOKEN, "sub-tenant-routes").await;
    assert_eq!(
        s2, 200,
        "an edge must serve /sub-tenant-routes from its snapshot: {v2}"
    );
    assert_eq!(
        v2["config_version"].as_u64(),
        Some(9),
        "the snapshot version: {v2}"
    );
    let arr2 = v2["sub_tenant_routes"].as_array().expect("array");
    assert_eq!(arr2.len(), 1);
    assert_eq!(arr2[0]["id"], "r1");
}
