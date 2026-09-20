//! Sub-tenant v2, V2+V3 — the leader's INTERNAL tenant-config write endpoint
//! (`/api/v1/internal/tenant-config/...`).
//!
//! Exercises the REAL `AdminService` via real HTTP against a real Pingora
//! `Service` backed by a real `:memory:` SQLite (mirrors `tests/cluster.rs`
//! control tests + `tests/sub_tenant_admin.rs`). No internal logic is mocked
//! (dev-plan 铁律 2): the cluster-token gate, the receiver-side lease assertion
//! (A-2 前置 7), the tenant re-auth (A-2 前置 4), the authorization binding
//! (A-2 前置 4), the transactional write core (D5) and the audit record (A-2 前置
//! 6) are all production code.
//!
//! The edge (data-plane) forwarding path is V1; here we drive the RECEIVING side
//! directly, exactly as `forward_config_write` would: `Authorization: Bearer
//! <cluster token>` (the internal gate) + `x-hydra-tenant-token: <tenant
//! bearer>` (the re-auth), per D2.

#![cfg(all(feature = "db", feature = "http-client", feature = "proxy"))]

mod common;

use std::sync::Arc;
use std::time::Duration;

use hydra_core::auth::sha256_hex_string;
use hydra_core::breaker::BreakerConfig;
use hydra_core::model::{Provider, ProviderModel, Tenant, TenantModel, TenantProvider};
use hydra_server::admin::{AdminService, AdminState};
use hydra_server::crypto::{KeyProvider, StaticKeyProvider};
use hydra_server::db as repo;
use hydra_server::http::{AuthCache, AuthConfig, HttpAuthChecker};
use hydra_server::proxy::admission::AdmissionControl;
use hydra_server::proxy::breaker_wrap::CircuitBreaker;
use hydra_server::store::ConfigStore;
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::services::listening::Service;
use serde_json::json;

const CLUSTER_TOKEN: &str = "cluster-tok";
const ADMIN_TOKEN: &str = "admin-tok";
/// Tenant `t1`'s bearer. Its SHA-256 hex is what the store compares.
const T1_TOKEN: &str = "sk-tenant-1";
/// Tenant `t2`'s bearer (for the cross-tenant negative tests).
const T2_TOKEN: &str = "sk-tenant-2";
const NOW: &str = "2026-01-01 00:00:00";

// --- harness -----------------------------------------------------------------

fn ephemeral_port() -> u16 {
    common::ephemeral_port()
}

/// A leader/edge `AdminState` over a real DB-backed store. `is_leader` controls
/// the lease answer: `Some(true)` = holds the lease, `Some(false)` = standby,
/// `None` = no election (single-node). `edge_mode` toggles the candidate gate.
async fn admin_state(edge_mode: bool, is_leader: Option<bool>) -> Arc<AdminState> {
    Arc::new(admin_state_inner(edge_mode, is_leader).await)
}

/// Like [`admin_state`], with the v2 D6 per-tenant config-write budget lowered so
/// the throttle can be exercised deterministically — via the state field, not an
/// env var, so parallel tests cannot interfere.
async fn admin_state_with_config_limit(
    edge_mode: bool,
    is_leader: Option<bool>,
    per_min: u32,
) -> Arc<AdminState> {
    let mut state = admin_state_inner(edge_mode, is_leader).await;
    state.config_write_per_min = per_min;
    Arc::new(state)
}

async fn admin_state_inner(edge_mode: bool, is_leader: Option<bool>) -> AdminState {
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
    let leader_ready: Option<Arc<dyn Fn() -> bool + Send + Sync>> =
        is_leader.map(|v| Arc::new(move || v) as Arc<dyn Fn() -> bool + Send + Sync>);
    AdminState::new(
        Some(pool),
        store,
        auth,
        breaker,
        key_provider,
        Some(ADMIN_TOKEN.to_string()),
        AdmissionControl::new(),
        edge_mode,
        Some(CLUSTER_TOKEN.to_string()),
        leader_ready,
    )
}

/// Start a real Pingora `Service` hosting `AdminService` on an ephemeral port,
/// polling until the listener actually accepts (bind races the test thread).
fn start_admin(state: Arc<AdminState>) -> u16 {
    let port = ephemeral_port();
    let addr = format!("127.0.0.1:{port}");
    let app = AdminService::new(state);
    let mut server = Server::new(Some(Opt::default())).expect("Server::new");
    server.bootstrap();
    let mut svc = Service::new("sub-tenant-internal-write-test".to_string(), app);
    svc.add_tcp(&addr);
    server.add_service(svc);
    std::thread::spawn(move || server.run_forever());
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if std::net::TcpStream::connect(&addr).is_ok() {
            return port;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "admin service did not start listening on {addr} within 5s"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A request to the internal tenant-config family. The cluster token goes in
/// `Authorization` (the internal gate); the tenant bearer goes in
/// `x-hydra-tenant-token` (the re-auth) — exactly the header combination
/// `forward_config_write` (D2) produces.
async fn req(
    port: u16,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
    cluster_token: Option<&str>,
    tenant_token: Option<&str>,
) -> reqwest::Response {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}{path}");
    for _ in 0..50 {
        let mut b = client.request(method.clone(), &url);
        if let Some(t) = cluster_token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        if let Some(t) = tenant_token {
            b = b.header("x-hydra-tenant-token", t);
        }
        if let Some(payload) = &body {
            b = b.json(payload);
        }
        match b.send().await {
            Ok(r) => return r,
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    panic!("server on {port} did not come up");
}

// --- seeding -----------------------------------------------------------------

fn provider(id: &str, key: &str) -> Provider {
    Provider {
        id: id.into(),
        key: key.into(),
        name: key.into(),
        endpoint: format!("https://api.{key}.example.com"),
        weight: 1,
        created_at: NOW.into(),
        updated_at: NOW.into(),
        max_concurrency: None,
        max_queue_depth: None,
        queue_wait_timeout_ms: None,
    }
}

fn provider_model(id: &str, key: &str, provider_id: &str) -> ProviderModel {
    ProviderModel {
        id: id.into(),
        key: key.into(),
        name: key.into(),
        provider_id: provider_id.into(),
        status: 1,
    }
}

fn tenant(id: &str, domain: &str) -> Tenant {
    Tenant {
        id: id.into(),
        name: id.into(),
        domain: domain.into(),
        auth_url: format!("https://auth.{domain}/verify"),
        cert_key: None,
        cert_file: None,
        enabled: true,
        created_at: NOW.into(),
        updated_at: NOW.into(),
    }
}

/// Base config for `t1`: provider `p1` serving `gpt-4o`, `t1` authorised for
/// `p1` with `gpt-4o` allowed, and a tenant access token (so re-auth resolves
/// `t1` for `T1_TOKEN`). Reloaded so the snapshot + token hash are live.
async fn seed_base(state: &AdminState) {
    let db = state.db();
    repo::insert_provider(db, &provider("p1", "openai"))
        .await
        .expect("insert p1");
    repo::insert_provider_model(db, &provider_model("pm1", "gpt-4o", "p1"))
        .await
        .expect("insert gpt-4o");
    repo::insert_tenant(db, &tenant("t1", "acme.com"))
        .await
        .expect("insert t1");
    let t1_hash = sha256_hex_string(T1_TOKEN.as_bytes());
    repo::set_tenant_access_token_hash(db, "t1", Some(&t1_hash))
        .await
        .expect("set t1 token hash");
    repo::insert_tenant_provider(
        db,
        &TenantProvider {
            id: "tp1".into(),
            tenant_id: "t1".into(),
            provider_id: "p1".into(),
        },
    )
    .await
    .expect("insert t1->p1");
    repo::insert_tenant_model(
        db,
        &TenantModel {
            id: "tm1".into(),
            tenant_id: "t1".into(),
            model_key: "gpt-4o".into(),
        },
    )
    .await
    .expect("insert t1 gpt-4o");
    state.store.reload_all().await.expect("reload base");
}

/// A second tenant `t2` (own token + provider `p2` / `gpt-4b`) for the
/// cross-tenant negative tests.
async fn seed_second_tenant(state: &AdminState) {
    let db = state.db();
    repo::insert_provider(db, &provider("p2", "anthropic"))
        .await
        .expect("insert p2");
    repo::insert_provider_model(db, &provider_model("pm2", "gpt-4b", "p2"))
        .await
        .expect("insert gpt-4b");
    repo::insert_tenant(db, &tenant("t2", "other.com"))
        .await
        .expect("insert t2");
    let t2_hash = sha256_hex_string(T2_TOKEN.as_bytes());
    repo::set_tenant_access_token_hash(db, "t2", Some(&t2_hash))
        .await
        .expect("set t2 token hash");
    repo::insert_tenant_provider(
        db,
        &TenantProvider {
            id: "tp2".into(),
            tenant_id: "t2".into(),
            provider_id: "p2".into(),
        },
    )
    .await
    .expect("insert t2->p2");
    repo::insert_tenant_model(
        db,
        &TenantModel {
            id: "tm2".into(),
            tenant_id: "t2".into(),
            model_key: "gpt-4b".into(),
        },
    )
    .await
    .expect("insert t2 gpt-4b");
    state.store.reload_all().await.expect("reload t2");
}

// --- tests -------------------------------------------------------------------

/// A-2 前置 7 + the internal gate: no cluster token ⇒ 401; an edge (non-candidate)
/// ⇒ 404; a candidate without the lease ⇒ 503 `not_leader`; the lease holder
/// executes.
#[tokio::test]
async fn gate_cluster_token_lease_and_candidate() {
    // (a) + (b): a lease holder, but the internal gate (cluster token) is first.
    let state = admin_state(false, Some(true)).await;
    let port = start_admin(state.clone());
    seed_base(&state).await;

    let body = json!({ "tenant_id": "t1", "name": "team-a", "key_prefix": "QQCX_" });
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenants",
        Some(body.clone()),
        None,
        Some(T1_TOKEN),
    )
    .await;
    assert_eq!(r.status(), 401, "no cluster token is rejected");

    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenants",
        Some(body.clone()),
        Some("wrong-cluster"),
        Some(T1_TOKEN),
    )
    .await;
    assert_eq!(r.status(), 401, "a wrong cluster token is rejected");

    // (c) edge (non-candidate): the router 404s before dispatch.
    let edge = admin_state(true, Some(true)).await;
    let edge_port = start_admin(edge);
    let r = req(
        edge_port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenants",
        Some(body.clone()),
        Some(CLUSTER_TOKEN),
        Some(T1_TOKEN),
    )
    .await;
    assert_eq!(r.status(), 404, "a non-candidate serves nothing");

    // (d) candidate WITHOUT the lease: the handler's lease gate answers 503.
    let standby = admin_state(false, Some(false)).await;
    let standby_port = start_admin(standby);
    let r = req(
        standby_port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenants",
        Some(body.clone()),
        Some(CLUSTER_TOKEN),
        Some(T1_TOKEN),
    )
    .await;
    assert_eq!(r.status(), 503, "a standby must not write locally");
    let v: serde_json::Value = r.json().await.expect("json");
    assert_eq!(v["error"]["code"], "not_leader", "got {v}");

    // (e) the lease holder (the first `state`) executes the write.
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenants",
        Some(body),
        Some(CLUSTER_TOKEN),
        Some(T1_TOKEN),
    )
    .await;
    assert_eq!(r.status(), 200, "the lease holder executes: {v}");
}

/// A-2 前置 4 — re-authentication: a missing `x-hydra-tenant-token` is 401, and
/// a token no tenant owns is 401; the valid token resolves `t1` and the write
/// proceeds.
#[tokio::test]
async fn reauth_requires_a_valid_tenant_token() {
    let state = admin_state(false, Some(true)).await;
    let port = start_admin(state.clone());
    seed_base(&state).await;

    let body = json!({ "tenant_id": "t1", "name": "team-a", "key_prefix": "QQCX_" });

    // Missing tenant token → 401.
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenants",
        Some(body.clone()),
        Some(CLUSTER_TOKEN),
        None,
    )
    .await;
    assert_eq!(r.status(), 401, "a missing tenant token is rejected");

    // A token no tenant owns → 401.
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenants",
        Some(body.clone()),
        Some(CLUSTER_TOKEN),
        Some("sk-attacker-token"),
    )
    .await;
    assert_eq!(r.status(), 401, "an unknown tenant token is rejected");

    // The valid token resolves t1 and the write succeeds.
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenants",
        Some(body),
        Some(CLUSTER_TOKEN),
        Some(T1_TOKEN),
    )
    .await;
    assert_eq!(r.status(), 200, "a valid token passes re-auth");
}

/// A-2 前置 4 — authorization binding: a sub-tenant PUT whose `tenant_id` is not
/// the authenticated tenant is 403 (pure string comparison, before any lookup);
/// a route PUT whose `sub_tenant_id` is missing or owned by another tenant is
/// 404 (resource-scoped).
#[tokio::test]
async fn binding_rejects_foreign_and_mismatched_targets() {
    let state = admin_state(false, Some(true)).await;
    let port = start_admin(state.clone());
    seed_base(&state).await;
    seed_second_tenant(&state).await;

    // (a) t1's token, but the write target is t2's tenant → 403.
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenants",
        Some(json!({ "tenant_id": "t2", "name": "evil", "key_prefix": "EVIL_" })),
        Some(CLUSTER_TOKEN),
        Some(T1_TOKEN),
    )
    .await;
    assert_eq!(
        r.status(),
        403,
        "a foreign tenant_id is a mismatch, not an oracle"
    );
    let v: serde_json::Value = r.json().await.expect("json");
    assert_eq!(v["error"]["code"], "tenant_id_mismatch", "got {v}");

    // A nonexistent tenant_id is ALSO 403 (never a 404 tenant-existence oracle).
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenants",
        Some(json!({ "tenant_id": "ghost", "name": "evil", "key_prefix": "EVIL_" })),
        Some(CLUSTER_TOKEN),
        Some(T1_TOKEN),
    )
    .await;
    assert_eq!(
        r.status(),
        403,
        "a nonexistent target is indistinguishable from a foreign one"
    );

    // (b) t1's token, route on t2's sub-tenant → 404 (resource-scoped).
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenants",
        Some(json!({ "tenant_id": "t2", "name": "t2st", "key_prefix": "ZZZZ_" })),
        Some(CLUSTER_TOKEN),
        Some(T2_TOKEN),
    )
    .await;
    assert_eq!(r.status(), 200, "t2 may create its own sub-tenant");
    let st: serde_json::Value = r.json().await.expect("json");
    let t2_st_id = st["sub_tenant"]["id"].as_str().expect("id").to_string();

    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenant-routes",
        Some(json!({ "sub_tenant_id": t2_st_id, "provider_id": "p1", "model_key": null })),
        Some(CLUSTER_TOKEN),
        Some(T1_TOKEN),
    )
    .await;
    assert_eq!(
        r.status(),
        404,
        "a route on another tenant's sub-tenant is 404"
    );

    // (c) t1's token, route on a missing sub-tenant → 404 (same as foreign).
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenant-routes",
        Some(json!({ "sub_tenant_id": "nope", "provider_id": "p1", "model_key": null })),
        Some(CLUSTER_TOKEN),
        Some(T1_TOKEN),
    )
    .await;
    assert_eq!(
        r.status(),
        404,
        "a missing sub-tenant is the same 404 as a foreign one"
    );
}

/// D4 idempotency + D5 write core + A-2 config_version: sub-tenant PUT upserts
/// by `(tenant_id, name)` (a repeated PUT converges to the same id), route PUT
/// upserts by `(sub_tenant_id, model_key)`, DELETE is idempotent (204 both
/// times), and `config_version` advances.
#[tokio::test]
async fn success_upsert_delete_and_version_advances() {
    let state = admin_state(false, Some(true)).await;
    let port = start_admin(state.clone());
    seed_base(&state).await;
    let version_before = state.store.version();

    // Sub-tenant PUT (upsert by name).
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenants",
        Some(json!({ "tenant_id": "t1", "name": "team-a", "key_prefix": "QQCX_" })),
        Some(CLUSTER_TOKEN),
        Some(T1_TOKEN),
    )
    .await;
    assert_eq!(r.status(), 200);
    let st: serde_json::Value = r.json().await.expect("json");
    assert_eq!(st["sub_tenant"]["name"], "team-a");
    assert!(
        st["config_version"].as_u64().expect("version") > version_before,
        "the write must advance config_version"
    );
    let st_id = st["sub_tenant"]["id"].as_str().expect("id").to_string();

    // A repeated PUT converges to the SAME row (idempotent upsert by name).
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenants",
        Some(json!({ "tenant_id": "t1", "name": "team-a", "key_prefix": "QQCX_" })),
        Some(CLUSTER_TOKEN),
        Some(T1_TOKEN),
    )
    .await;
    assert_eq!(r.status(), 200);
    let st2: serde_json::Value = r.json().await.expect("json");
    assert_eq!(
        st2["sub_tenant"]["id"].as_str().expect("id"),
        st_id,
        "a repeated PUT by name converges to the existing row"
    );

    // Route PUT by (sub_tenant_id, model_key).
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenant-routes",
        Some(json!({ "sub_tenant_id": st_id, "provider_id": "p1", "model_key": "gpt-4o" })),
        Some(CLUSTER_TOKEN),
        Some(T1_TOKEN),
    )
    .await;
    assert_eq!(r.status(), 200);
    let route: serde_json::Value = r.json().await.expect("json");
    assert_eq!(route["route"]["sub_tenant_id"].as_str().expect(""), st_id);
    assert_eq!(route["route"]["model_key"].as_str().expect(""), "gpt-4o");
    let route_id = route["route"]["id"].as_str().expect("id").to_string();

    // Route DELETE (204), then idempotent (204 again).
    let r = req(
        port,
        reqwest::Method::DELETE,
        &format!("/api/v1/internal/tenant-config/sub-tenant-routes/{route_id}"),
        None,
        Some(CLUSTER_TOKEN),
        Some(T1_TOKEN),
    )
    .await;
    assert_eq!(r.status(), 204, "route delete");
    let r = req(
        port,
        reqwest::Method::DELETE,
        &format!("/api/v1/internal/tenant-config/sub-tenant-routes/{route_id}"),
        None,
        Some(CLUSTER_TOKEN),
        Some(T1_TOKEN),
    )
    .await;
    assert_eq!(
        r.status(),
        204,
        "route delete is idempotent (absent → 204 no-op)"
    );

    // Sub-tenant DELETE (204), then idempotent (204 again).
    let r = req(
        port,
        reqwest::Method::DELETE,
        &format!("/api/v1/internal/tenant-config/sub-tenants/{st_id}"),
        None,
        Some(CLUSTER_TOKEN),
        Some(T1_TOKEN),
    )
    .await;
    assert_eq!(r.status(), 204, "sub-tenant delete");
    let r = req(
        port,
        reqwest::Method::DELETE,
        &format!("/api/v1/internal/tenant-config/sub-tenants/{st_id}"),
        None,
        Some(CLUSTER_TOKEN),
        Some(T1_TOKEN),
    )
    .await;
    assert_eq!(
        r.status(),
        204,
        "sub-tenant delete is idempotent (absent → 204 no-op)"
    );
}

/// The core negative: a cluster-token holder carrying tenant `t2`'s bearer
/// CANNOT write another tenant's (`t1`'s) resources — 403 on a sub-tenant write,
/// 404 on a route write / delete — and `t1`'s row is left intact.
#[tokio::test]
async fn core_negative_cannot_write_another_tenants_resources() {
    let state = admin_state(false, Some(true)).await;
    let port = start_admin(state.clone());
    seed_base(&state).await;
    seed_second_tenant(&state).await;

    // t1 creates its own sub-tenant.
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenants",
        Some(json!({ "tenant_id": "t1", "name": "t1st", "key_prefix": "QQQQ_" })),
        Some(CLUSTER_TOKEN),
        Some(T1_TOKEN),
    )
    .await;
    assert_eq!(r.status(), 200);
    let st: serde_json::Value = r.json().await.expect("json");
    let t1_st_id = st["sub_tenant"]["id"].as_str().expect("id").to_string();

    // t2 (valid cluster token + t2's bearer) CANNOT create a sub-tenant under t1.
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenants",
        Some(json!({ "tenant_id": "t1", "name": "evil", "key_prefix": "EVIL_" })),
        Some(CLUSTER_TOKEN),
        Some(T2_TOKEN),
    )
    .await;
    assert_eq!(r.status(), 403, "t2 cannot write t1's sub-tenants");

    // ...CANNOT create a route on t1's sub-tenant.
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenant-routes",
        Some(json!({ "sub_tenant_id": t1_st_id, "provider_id": "p1", "model_key": null })),
        Some(CLUSTER_TOKEN),
        Some(T2_TOKEN),
    )
    .await;
    assert_eq!(r.status(), 404, "t2 cannot route t1's sub-tenant");

    // ...CANNOT delete t1's sub-tenant.
    let r = req(
        port,
        reqwest::Method::DELETE,
        &format!("/api/v1/internal/tenant-config/sub-tenants/{t1_st_id}"),
        None,
        Some(CLUSTER_TOKEN),
        Some(T2_TOKEN),
    )
    .await;
    assert_eq!(r.status(), 404, "t2 cannot delete t1's sub-tenant");

    // t1's sub-tenant is intact (no write happened).
    let snap = state.store.snapshot();
    assert!(
        snap.sub_tenants.iter().any(|s| s.id == t1_st_id),
        "t1's sub-tenant must be untouched by t2's attempts"
    );
    drop(snap);
}

/// v2 D6 — the per-tenant config-write throttle: past the budget a write is 429,
/// and the budget is **per tenant** (t2 is unaffected by t1 exhausting its own).
#[tokio::test]
async fn config_write_throttle_is_per_tenant() {
    let state = admin_state_with_config_limit(false, Some(true), 1).await;
    let port = start_admin(state.clone());
    seed_base(&state).await;
    seed_second_tenant(&state).await;

    // t1's first write within the window is allowed.
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenants",
        Some(json!({ "tenant_id": "t1", "name": "team-t1", "key_prefix": "T1AA_" })),
        Some(CLUSTER_TOKEN),
        Some(T1_TOKEN),
    )
    .await;
    assert_eq!(r.status(), 200, "the first write is within the budget");

    // t1's second write exceeds the 1/min budget → 429.
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenants",
        Some(json!({ "tenant_id": "t1", "name": "team-t1b", "key_prefix": "T1BB_" })),
        Some(CLUSTER_TOKEN),
        Some(T1_TOKEN),
    )
    .await;
    assert_eq!(r.status(), 429, "the second write exceeds the 1/min budget");

    // t2 has its OWN budget: its first write is still allowed.
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenants",
        Some(json!({ "tenant_id": "t2", "name": "team-t2", "key_prefix": "T2AA_" })),
        Some(CLUSTER_TOKEN),
        Some(T2_TOKEN),
    )
    .await;
    assert_eq!(r.status(), 200, "the budget is per tenant, not global");
}

/// The natural-key route PUT is idempotent: a repeated PUT converges to the
/// SAME immutable id (D4 / A-2 理由 4). Exercises the route-upsert self-exclusion
/// in the write core — a repeated PUT targets the existing row, so it must not
/// be counted as a new route (the oracle v2 BLOCKING fix).
#[tokio::test]
async fn route_put_is_idempotent() {
    let state = admin_state(false, Some(true)).await;
    let port = start_admin(state.clone());
    seed_base(&state).await;

    // A sub-tenant to hang the route on.
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenants",
        Some(json!({ "tenant_id": "t1", "name": "team-a", "key_prefix": "QQCX_" })),
        Some(CLUSTER_TOKEN),
        Some(T1_TOKEN),
    )
    .await;
    assert_eq!(r.status(), 200);
    let st: serde_json::Value = r.json().await.expect("json");
    let st_id = st["sub_tenant"]["id"].as_str().expect("id").to_string();

    let route_body = json!({
        "sub_tenant_id": st_id,
        "provider_id": "p1",
        "model_key": "gpt-4o"
    });
    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenant-routes",
        Some(route_body.clone()),
        Some(CLUSTER_TOKEN),
        Some(T1_TOKEN),
    )
    .await;
    assert_eq!(r.status(), 200);
    let id1 = r.json::<serde_json::Value>().await.expect("json")["route"]["id"]
        .as_str()
        .expect("id")
        .to_string();

    let r = req(
        port,
        reqwest::Method::PUT,
        "/api/v1/internal/tenant-config/sub-tenant-routes",
        Some(route_body),
        Some(CLUSTER_TOKEN),
        Some(T1_TOKEN),
    )
    .await;
    assert_eq!(r.status(), 200, "a repeated route PUT is accepted");
    let id2 = r.json::<serde_json::Value>().await.expect("json")["route"]["id"]
        .as_str()
        .expect("id")
        .to_string();
    assert_eq!(
        id1, id2,
        "a repeated route PUT by natural key converges to the same id"
    );
}
