//! Tenant self-service API on the DATA-PLANE listener — interception and the
//! token gate (plan T4).
//!
//! T4 delivers the reserved-prefix interception, the token gate and an honest
//! routing skeleton: no endpoint is wired yet, so every parsed route answers
//! `404 unknown path` **locally**. E1 arrived in T5 and E3 in T8 — each
//! replacing exactly one 404 arm. That ordering is deliberate: if T4 wired E1,
//! T5's RED step could never fail for the right reason. E2 (the auth-cache
//! invalidate route) was retired together with the external-auth surface it
//! served, so its tests are gone and the arm it occupied answers `404` again.

#![cfg(all(feature = "db", feature = "http-client", feature = "proxy"))]

mod common;

use std::sync::Arc;
use std::time::Duration;

use hydra_core::auth::sha256_hex_string;
use hydra_core::breaker::BreakerConfig;
use hydra_core::model::{Provider, ProviderModel, Tenant, TenantModel, TenantProvider};
use hydra_server::crypto::StaticKeyProvider;
use hydra_server::proxy::breaker_wrap::CircuitBreaker;
use hydra_server::proxy::config::ProxyConfig;
use hydra_server::proxy::limiter::RateLimiter;
use hydra_server::proxy::{AppState, HydraProxy};
use hydra_server::store::ConfigStore;
use hydra_server::tenant_api::TenantApiConfig;
use hydra_server::usage::UsageSink;
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use serde_json::{json, Value};

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

/// The token lives in the `tenant_access_token` TABLE — the snapshot the gate
/// reads is built from it (`ConfigData.tenant_tokens`, enabled rows only). A
/// hash in the legacy `tenant.access_token_hash` column is no longer what
/// authenticates.
async fn seed_tenant(
    pool: &sqlx::SqlitePool,
    id: &str,
    domain: &str,
    token: Option<&str>,
) -> Option<String> {
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
        Some(common::seed_tenant_access_token(pool, id, "test", tok, true, None).await)
    } else {
        None
    }
}

async fn build_state(pool: &sqlx::SqlitePool, cfg: TenantApiConfig) -> Arc<AppState> {
    let kp: Arc<dyn hydra_server::crypto::KeyProvider> =
        Arc::new(StaticKeyProvider::new([7u8; 32], 1));
    let store = ConfigStore::load(pool.clone(), kp)
        .await
        .expect("ConfigStore::load");
    let breaker = Arc::new(CircuitBreaker::new(BreakerConfig::new(5)));
    let limiter = Arc::new(RateLimiter::new());
    let sink: Arc<dyn UsageSink> = Arc::new(NoopSink);
    AppState::for_tests(store, breaker, limiter, sink, ProxyConfig::default(), cfg)
}

/// A usage reader that always answers a real zero.
///
/// `for_tests` deliberately gives a node NO reader (ADR-0002 D-3 retired the local SQLite usage
/// store, and "which store answers" belongs to the configured backend). The legs that need
/// `GET /usage` to answer are about the tenant GATE, so they inject this.
struct ZeroUsageQuery;

impl hydra_server::usage::query::UsageQuery for ZeroUsageQuery {
    fn aggregate<'a>(
        &'a self,
        _tenant_id: &'a str,
        _since: &'a str,
        _until: &'a str,
        _group_by: hydra_server::usage::query::GroupBy,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        hydra_core::tenant_api::UsageAggregate,
                        hydra_server::usage::query::UsageQueryError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async {
            Ok(hydra_core::tenant_api::UsageAggregate {
                rows: Vec::new(),
                totals: hydra_core::tenant_api::UsageTotals::default(),
                as_of: None,
            })
        })
    }

    fn source(&self) -> &'static str {
        "testing"
    }
}

/// As [`build_state`], with the zero reader injected so `GET /usage` answers.
async fn build_state_with_usage(pool: &sqlx::SqlitePool, cfg: TenantApiConfig) -> Arc<AppState> {
    let base = build_state(pool, cfg).await;
    // The state is an `Arc<AppState>`; the reader is part of it, so it is injected at construction
    // instead. Rebuild through the same path with the extra argument.
    let _ = base;
    let kp: Arc<dyn hydra_server::crypto::KeyProvider> =
        Arc::new(StaticKeyProvider::new([7u8; 32], 1));
    let store = ConfigStore::load(pool.clone(), kp)
        .await
        .expect("ConfigStore::load");
    AppState::for_tests_with_usage(
        store,
        Arc::new(CircuitBreaker::new(BreakerConfig::new(5))),
        Arc::new(RateLimiter::new()),
        Arc::new(NoopSink) as Arc<dyn UsageSink>,
        ProxyConfig::default(),
        TenantApiConfig::default(),
        Some(Arc::new(ZeroUsageQuery) as Arc<dyn hydra_server::usage::query::UsageQuery>),
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
    let old_token_id = seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN))
        .await
        .expect("seeded token row id");
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state.clone());
    let c = client();
    let url = format!("{root}/tenant/t1/api/v1/whoami");

    // Rotate in the DB only: retire the old row, insert the new one. No reload_all.
    // (The multi-token model rotates by REPLACING rows in `tenant_access_token`;
    // the legacy `tenant.access_token_hash` column is not what the gate reads.)
    let rotated = "sk-rotated-token-9999999999";
    hydra_server::db::delete_tenant_access_token(&pool, &old_token_id)
        .await
        .expect("retire the old token row");
    common::seed_tenant_access_token(&pool, "t1", "rotated", rotated, true, None).await;

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

/// T20 (gate layer): before the first snapshot a node holds no configuration at
/// all (version 0), so the gate must fail CLOSED with 503 — never pass a request
/// through because it "could not check", and never call the token wrong (401)
/// when there is nothing to judge it against.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn before_the_first_snapshot_the_gate_fails_closed_with_503() {
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let kp: Arc<dyn hydra_server::crypto::KeyProvider> =
        Arc::new(StaticKeyProvider::new([7u8; 32], 1));
    // The state this test is about: a node whose IN-MEMORY snapshot holds nothing yet, while the
    // database it was seeded through is not what it is serving from (`version() == 0`). That is the
    // window between process start and the first materialization, and it is what the gate answers
    // 503 for.
    let store = ConfigStore::from_data(
        common::setup_pool().await,
        hydra_core::config::ConfigData::default(),
        kp,
    )
    .await
    .expect("from_data");
    let state = AppState::for_tests(
        store,
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
async fn whoami_works_from_a_materialized_snapshot_alone() {
    use hydra_server::cluster::content::FidelityRows;
    use hydra_server::cluster::snapshot::HydratedWire;

    let kp: Arc<dyn hydra_server::crypto::KeyProvider> =
        Arc::new(StaticKeyProvider::new([7u8; 32], 1));

    // The replica shape: a store whose own database holds nothing, fed one snapshot — its config
    // comes from the materializer, not from its tables, and the read-only endpoints must not care.
    let store = ConfigStore::from_data(
        common::setup_pool().await,
        hydra_core::config::ConfigData::default(),
        kp,
    )
    .await
    .expect("from_data");
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
    // The gate authenticates from `ConfigData.tenant_tokens` (hash → identity).
    // `hydrate` fills it from the `tenant_access_tokens` fidelity rows; here we
    // are the producer of the wire, so we state the same identity directly.
    cfg.tenant_tokens.insert(
        sha256_hex_string(TENANT_TOKEN.as_bytes()),
        hydra_core::identity::TenantTokenIdentity {
            tenant_id: "t1".into(),
            token_name: "edge-token".into(),
            expires_at: None,
        },
    );
    // `hydrate` does this in production; here we are the producer of the wire.
    cfg.reindex_tenants();
    store.apply_snapshot(HydratedWire {
        version: 7,
        cfg,
        fidelity: FidelityRows {
            limit_roles: vec![],
            provider_keys: vec![],
            tenant_token_hashes: vec![(
                "t1".to_string(),
                sha256_hex_string(TENANT_TOKEN.as_bytes()),
            )],
            provider_models: vec![],
            tenant_providers: vec![],
            tenant_models: vec![],
            user_groups: vec![],
            users: vec![],
            user_api_keys: vec![],
            tenant_access_tokens: vec![hydra_core::model::TenantAccessToken {
                id: "tok-edge".into(),
                tenant_id: "t1".into(),
                name: "edge-token".into(),
                token_hash: sha256_hex_string(TENANT_TOKEN.as_bytes()),
                token_prefix: "sk-tenant".into(),
                token_last4: "cdef".into(),
                enabled: true,
                expires_at: None,
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
            }],
            global_roles: vec![],
        },
    });

    let state = AppState::for_tests(
        store,
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

/// A SUSPENDED tenant (欠费停机) must still be able to run its own recovery: read
/// its state and check what it has used. The gate never consults `enabled` —
/// suspension is the tenant's own policy, expressed through its `auth_url`, and a
/// tenant that cannot reach the recovery path cannot recover.
///
/// E1 alone was covered; this pins both surviving self-service endpoints (E2,
/// the auth-cache invalidate route, was retired with the external-auth surface
/// it served), because the contract document for tenants states them and a
/// document is only as good as its test.
/// The window a read asks for is computed from NOW: a hard-coded `since` ages out of the 31-day
/// ceiling (`HYDRA_TENANT_API_USAGE_MAX_WINDOW_DAYS`) and turns the leg red on a date rather than on
/// a change — measured 2026-10-07 on two drills that did exactly that.
fn window() -> (String, String) {
    let now = chrono::Utc::now();
    (
        (now - chrono::Duration::days(1))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string(),
        now.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_suspended_tenant_can_still_use_both_surviving_endpoints() {
    let (since, until) = window();
    let pool = common::setup_pool().await;
    seed_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let mut t = hydra_server::db::get_tenant(&pool, "t1")
        .await
        .expect("get");
    t.enabled = false;
    hydra_server::db::update_tenant(&pool, &t)
        .await
        .expect("disable");
    let state = build_state_with_usage(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);
    let c = client();

    let _ = &c;
    let (s1, v1) = get_tenant_json(&root, "t1", TENANT_TOKEN, "whoami").await;
    assert_eq!(s1, 200);
    assert_eq!(v1["enabled"], false, "suspended, and it can see that: {v1}");

    let (s3, v3) = get_tenant_json(
        &root,
        "t1",
        TENANT_TOKEN,
        &format!("usage?since={since}&until={until}"),
    )
    .await;
    assert_eq!(s3, 200, "and must be able to read its own usage: {v3}");
    assert_eq!(
        v3["source"], "testing",
        "the reader that answered is named in the envelope: {v3}"
    );
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
// V5 — D9 data-plane identity-plane writes (user groups, users, api keys)
//
// These run on the single-node shape (`AppState::for_tests` installs NO
// forwarder), so every write exercises the D8 LOCAL path: gate → handler
// statement → reload/publish tail → `config_version` advances. The forward
// path is covered separately in the `cluster-redis` e2e target.
// ---------------------------------------------------------------------------

const NOW: &str = "2026-01-01T00:00:00Z";

/// A tenant with a token PLUS the provider/model/bindings a route write needs
/// (`p1` serving `gpt-4o`, `t` authorised for `p1` with `gpt-4o`).
async fn seed_full_tenant(pool: &sqlx::SqlitePool, id: &str, domain: &str, token: Option<&str>) {
    seed_tenant(pool, id, domain, token).await;
    hydra_server::db::insert_provider(
        pool,
        &Provider {
            id: "p1".into(),
            key: "openai".into(),
            name: "openai".into(),
            endpoint: "https://api.openai.example.com".into(),
            weight: 1,
            created_at: NOW.into(),
            updated_at: NOW.into(),
            max_concurrency: None,
            max_queue_depth: None,
            queue_wait_timeout_ms: None,
        },
    )
    .await
    .expect("insert p1");
    hydra_server::db::insert_provider_model(
        pool,
        &ProviderModel {
            id: "pm1".into(),
            key: "gpt-4o".into(),
            name: "gpt-4o".into(),
            provider_id: "p1".into(),
            status: 1,
        },
    )
    .await
    .expect("insert gpt-4o");
    hydra_server::db::insert_tenant_provider(
        pool,
        &TenantProvider {
            id: "tp1".into(),
            tenant_id: id.into(),
            provider_id: "p1".into(),
        },
    )
    .await
    .expect("insert t->p1");
    hydra_server::db::insert_tenant_model(
        pool,
        &TenantModel {
            id: "tm1".into(),
            tenant_id: id.into(),
            model_key: "gpt-4o".into(),
        },
    )
    .await
    .expect("insert t gpt-4o");
}

/// Send a data-plane write (PUT/DELETE) with a Bearer token and an optional
/// JSON body, retrying until the listener accepts.
async fn send_write(
    root: &str,
    method: reqwest::Method,
    path: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (u16, Value) {
    let c = client();
    let url = format!("{root}{path}");
    for _ in 0..60 {
        let mut req = c.request(method.clone(), &url);
        if let Some(b) = bearer {
            req = req.bearer_auth(b);
        }
        if let Some(b) = &body {
            req = req.json(b);
        }
        match req.send().await {
            Ok(r) => return body_json(r).await,
            Err(_) => tokio::time::sleep(Duration::from_millis(150)).await,
        }
    }
    panic!("proxy never became ready at {url}");
}

/// PUT a user-group row with an explicit id, so a repeated PUT can be shown to
/// converge on the SAME row rather than mint a new one.
async fn put_user_group(
    root: &str,
    tenant: &str,
    token: &str,
    id: &str,
    name: &str,
) -> (u16, Value) {
    send_write(
        root,
        reqwest::Method::PUT,
        &format!("/tenant/{tenant}/api/v1/user-groups"),
        Some(token),
        Some(json!({ "id": id, "name": name, "enabled": true })),
    )
    .await
}

/// PUT a user row in `group_id`, with an explicit id for the same reason.
async fn put_user(
    root: &str,
    tenant: &str,
    token: &str,
    id: &str,
    group_id: &str,
    name: &str,
) -> (u16, Value) {
    send_write(
        root,
        reqwest::Method::PUT,
        &format!("/tenant/{tenant}/api/v1/users"),
        Some(token),
        Some(json!({ "id": id, "user_group_id": group_id, "name": name, "enabled": true })),
    )
    .await
}

/// The D8 local write path, end to end: a single-node node applies the write
/// itself, `config_version` advances, and a repeated PUT with the SAME row id
/// converges on that row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_node_local_write_put_is_idempotent_and_advances_version() {
    let pool = common::setup_pool().await;
    seed_full_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let version_before = state.store.version();
    let root = start_proxy(state.clone());

    let (status, v) = put_user_group(&root, "t1", TENANT_TOKEN, "grp-a", "team-a").await;
    assert_eq!(status, 200, "got {v}");
    let id = v["user_group"]["id"].as_str().expect("id").to_string();
    assert_eq!(id, "grp-a");
    assert_eq!(v["user_group"]["name"], "team-a");
    let version_after = v["config_version"].as_u64().expect("version");
    assert!(
        version_after > version_before,
        "a local write must advance config_version: before={version_before} after={version_after}"
    );
    // The write landed in the snapshot the data plane reads.
    let snap = state.store.snapshot();
    assert!(
        snap.user_groups.iter().any(|g| g.id == id),
        "the row is in the snapshot"
    );
    drop(snap);

    // A repeated PUT by the same id converges to the SAME row.
    let (status2, v2) = put_user_group(&root, "t1", TENANT_TOKEN, "grp-a", "team-a").await;
    assert_eq!(status2, 200, "got {v2}");
    assert_eq!(
        v2["user_group"]["id"].as_str().expect("id"),
        id,
        "a repeated PUT with the same id converges to the existing row"
    );
}

/// A deletion is reported by ABSENCE: 204 the first time, then 404 `not_found`
/// once the row is gone (the immutable id is never silently invented back).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_node_local_write_delete_reports_absence() {
    let pool = common::setup_pool().await;
    seed_full_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);

    let (status, v) = put_user_group(&root, "t1", TENANT_TOKEN, "grp-del", "team-del").await;
    assert_eq!(status, 200, "got {v}");
    let id = v["user_group"]["id"].as_str().expect("id").to_string();
    assert_eq!(id, "grp-del");

    let (s1, _) = send_write(
        &root,
        reqwest::Method::DELETE,
        &format!("/tenant/t1/api/v1/user-groups/{id}"),
        Some(TENANT_TOKEN),
        None,
    )
    .await;
    assert_eq!(s1, 204, "first delete removes the row");

    let (s2, v2) = send_write(
        &root,
        reqwest::Method::DELETE,
        &format!("/tenant/t1/api/v1/user-groups/{id}"),
        Some(TENANT_TOKEN),
        None,
    )
    .await;
    assert_eq!(s2, 404, "the deleted id is now absent: {v2}");
    assert_eq!(v2["error"]["code"], "not_found", "got {v2}");
}

/// The user upsert is FLAT too: the row id is in the BODY, and the DELETE
/// removes that immutable id again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_node_local_user_write_upsert_and_delete() {
    let pool = common::setup_pool().await;
    seed_full_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let version_before = state.store.version();
    let root = start_proxy(state);

    let (gs, gv) = put_user_group(&root, "t1", TENANT_TOKEN, "grp-user-parent", "parent").await;
    assert_eq!(gs, 200, "got {gv}");
    let group_id = gv["user_group"]["id"]
        .as_str()
        .expect("group id")
        .to_string();

    let (us, uv) = put_user(
        &root,
        "t1",
        TENANT_TOKEN,
        "usr-1",
        group_id.as_str(),
        "named-user",
    )
    .await;
    assert_eq!(us, 200, "got {uv}");
    assert_eq!(uv["user"]["id"].as_str().expect("id"), "usr-1");
    assert_eq!(uv["user"]["user_group_id"], group_id);
    assert!(
        uv["config_version"].as_u64().expect("version") > version_before,
        "a user write must advance config_version"
    );

    let (us2, uv2) = put_user(
        &root,
        "t1",
        TENANT_TOKEN,
        "usr-1",
        group_id.as_str(),
        "named-user",
    )
    .await;
    assert_eq!(us2, 200, "got {uv2}");
    assert_eq!(
        uv2["user"]["id"].as_str().expect("id"),
        "usr-1",
        "a repeated user PUT with the same id converges"
    );

    let (ds, _) = send_write(
        &root,
        reqwest::Method::DELETE,
        "/tenant/t1/api/v1/users/usr-1",
        Some(TENANT_TOKEN),
        None,
    )
    .await;
    assert_eq!(ds, 204, "first user delete");

    let (ds2, dv) = send_write(
        &root,
        reqwest::Method::DELETE,
        "/tenant/t1/api/v1/users/usr-1",
        Some(TENANT_TOKEN),
        None,
    )
    .await;
    assert_eq!(ds2, 404, "the deleted id is now absent: {dv}");
    assert_eq!(dv["error"]["code"], "not_found", "got {dv}");
}

/// No credential at all: 401 from the gate (the same gate a read uses).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_with_no_token_is_401() {
    let pool = common::setup_pool().await;
    seed_full_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);
    let (status, v) = send_write(
        &root,
        reqwest::Method::PUT,
        "/tenant/t1/api/v1/user-groups",
        None,
        Some(json!({ "id": "evil", "name": "evil", "enabled": true })),
    )
    .await;
    assert_eq!(status, 401, "got {v}");
    assert_eq!(v["error"]["code"], "unauthorized", "got {v}");
}

/// OC-5 — a WRITE refused by the token gate must be attributed to its ROUTE, not
/// to `unrouted`.
///
/// The write path assigned `ctx.tenant_api_endpoint` AFTER `run_gate`, but the
/// gate is exactly what writes the 401/403/429/503 refusal: every refused write
/// was therefore counted under `endpoint="unrouted"` — the label reserved for
/// "this path is not one of our routes" — for a request that had hit a perfectly
/// well-known one. An operator alerting on refusals per endpoint saw write
/// refusals nowhere, and a spike in `unrouted` (which is supposed to mean a path
/// drift, i.e. a code/config bug) was actually credential guessing against the
/// write endpoints. The READ path always set the label before its gate.
///
/// `user-group-delete` is the low-cardinality label for
/// `DELETE /user-groups/{id}`; no other test in this binary rejects that
/// route, so the exact `+1` is safe under a parallel test run.
///
/// Falsification: move the assignment back below `run_gate` and this fails with
/// no increase at all (`before == after`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_write_is_attributed_to_its_route_not_to_unrouted() {
    use hydra_server::admin::metrics;
    let pool = common::setup_pool().await;
    seed_full_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);

    let before = metrics::tenant_api_requests_total("user-group-delete", 401);
    let (status, v) = send_write(
        &root,
        reqwest::Method::DELETE,
        "/tenant/t1/api/v1/user-groups/never-seeded",
        None,
        None,
    )
    .await;
    assert_eq!(status, 401, "got {v}");
    assert_eq!(v["error"]["code"], "unauthorized", "got {v}");
    let after = metrics::tenant_api_requests_total("user-group-delete", 401);
    assert_eq!(
        after,
        before + 1.0,
        "a refused write must be counted under its own route label, not `unrouted` (before={before}, after={after})"
    );
}

/// The URL's tenant id is a cross-check: a token for `t1` against a `t2` URL is
/// 403 `tenant_id_mismatch` (never 200, never a silent write to the other
/// tenant).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_url_token_mismatch_is_403() {
    let pool = common::setup_pool().await;
    seed_full_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    // t2 only needs a token: the mismatch is decided at the gate, before any
    // handler statement, so no provider/model rows are required for it.
    seed_tenant(&pool, "t2", "other.example", Some(OTHER_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);
    let (status, v) = send_write(
        &root,
        reqwest::Method::PUT,
        "/tenant/t2/api/v1/user-groups",
        Some(TENANT_TOKEN),
        Some(json!({ "id": "evil", "name": "evil", "enabled": true })),
    )
    .await;
    assert_eq!(status, 403, "got {v}");
    assert_eq!(v["error"]["code"], "tenant_id_mismatch", "got {v}");
}

/// Cross-tenant isolation: `t2`'s token cannot create a user-group under `t1`,
/// and `t1`'s existing row is left intact by the attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_cross_tenant_is_rejected_and_leaves_rows_intact() {
    let pool = common::setup_pool().await;
    seed_full_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    // t2 only needs a token: its write is refused at the gate (403) before any
    // handler statement, so no provider/model rows are required for it.
    seed_tenant(&pool, "t2", "other.example", Some(OTHER_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state.clone());

    // t1 owns a user-group.
    let (s, v) = put_user_group(&root, "t1", TENANT_TOKEN, "grp-real", "team-a").await;
    assert_eq!(s, 200, "got {v}");

    // t2's token cannot write to t1's namespace: 403 (URL cross-check).
    let (s2, v2) = send_write(
        &root,
        reqwest::Method::PUT,
        "/tenant/t1/api/v1/user-groups",
        Some(OTHER_TOKEN),
        Some(json!({ "id": "evil", "name": "evil", "enabled": true })),
    )
    .await;
    assert_eq!(s2, 403, "t2 cannot write t1's user-groups: {v2}");
    assert_eq!(v2["error"]["code"], "tenant_id_mismatch", "got {v2}");

    // t1's row is untouched by t2's attempt.
    let snap = state.store.snapshot();
    let names: Vec<String> = snap
        .user_groups
        .iter()
        .filter(|g| g.tenant_id == "t1")
        .map(|g| g.name.clone())
        .collect();
    drop(snap);
    assert!(
        names.contains(&"team-a".to_string()),
        "t1 keeps its user-group"
    );
    assert!(
        !names.contains(&"evil".to_string()),
        "t2's write did not land"
    );
}

/// A near miss under the reserved prefix is answered with a LOCAL 404 — it must
/// never fall through to the proxy pipeline (where a tenant token would be read
/// as a client api-key).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_near_miss_is_a_local_404_not_a_fallthrough() {
    let pool = common::setup_pool().await;
    seed_full_tenant(&pool, "t1", "acme.example", Some(TENANT_TOKEN)).await;
    let state = build_state(&pool, TenantApiConfig::default()).await;
    let root = start_proxy(state);
    for (method, path) in [
        // A flat upsert path with an extra segment is not an item route.
        (reqwest::Method::PUT, "/tenant/t1/api/v1/user-groups/extra"),
        // A trailing slash on the flat upsert path.
        (reqwest::Method::PUT, "/tenant/t1/api/v1/user-groups/"),
        // PUT on a made-up resource.
        (reqwest::Method::PUT, "/tenant/t1/api/v1/typo"),
        // DELETE with no id.
        (reqwest::Method::DELETE, "/tenant/t1/api/v1/user-groups/"),
        // DELETE with no id segment.
        (reqwest::Method::DELETE, "/tenant/t1/api/v1/user-groups"),
        // POST on a user-shaped path that is not the api-key mint route.
        (reqwest::Method::POST, "/tenant/t1/api/v1/users/extra"),
    ] {
        let (status, v) = send_write(&root, method, path, Some(TENANT_TOKEN), None).await;
        assert_eq!(status, 404, "{path} must be a local 404, got {v}");
        assert_eq!(
            v["error"]["code"], "not_found",
            "{path} must carry our envelope: {v}"
        );
        // The proxy short-circuit body has `type: "proxy_error"`; ours never does.
        assert_eq!(
            v["error"]["type"],
            Value::Null,
            "{path} fell through to the proxy pipeline: {v}"
        );
    }
}
