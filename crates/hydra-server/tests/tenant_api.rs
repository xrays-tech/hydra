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
    let state = build_state(&pool, TenantApiConfig { enabled: false }).await;
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
