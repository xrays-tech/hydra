//! T6.1/T6.2 — sub-tenant & sub-tenant-route admin CRUD + error-level write
//! validation + hot reload.
//!
//! Exercises the REAL `AdminService` via real HTTP against a real Pingora
//! `Service` backed by a real `:memory:` SQLite (mirrors
//! `tests/admin_api.rs::provider_key_bindings_crud_http`). No internal logic is
//! mocked (dev-plan §1 铁律 2): the write path is the pure
//! `hydra_core::sub_tenant::validate_sub_tenant_write` gate plus the real repo.

#![cfg(all(feature = "db", feature = "http-client", feature = "proxy"))]

mod common;

use std::sync::Arc;
use std::time::Duration;

use hydra_core::breaker::BreakerConfig;
use hydra_core::model::{
    Provider, ProviderKeyBinding, ProviderModel, SubTenant, Tenant, TenantModel, TenantProvider,
};
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

const TOKEN: &str = "test-admin-token";
const NOW: &str = "2026-01-01 00:00:00";

fn ephemeral_port() -> u16 {
    common::ephemeral_port()
}

async fn admin_state() -> Arc<AdminState> {
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
    Arc::new(AdminState::new(
        Some(pool),
        store,
        auth,
        breaker,
        key_provider,
        Some(TOKEN.to_string()),
        AdmissionControl::new(),
        false,
        None,
        None,
    ))
}

fn start_admin(state: Arc<AdminState>) -> u16 {
    let port = ephemeral_port();
    let addr = format!("127.0.0.1:{port}");
    let app = AdminService::new(state);
    let mut server = Server::new(Some(Opt::default())).expect("Server::new");
    server.bootstrap();
    let mut svc = Service::new("sub-tenant-test".to_string(), app);
    svc.add_tcp(&addr);
    server.add_service(svc);
    std::thread::spawn(move || server.run_forever());
    port
}

async fn req(
    port: u16,
    method: reqwest::Method,
    path: &str,
    body: Option<&str>,
) -> reqwest::Response {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client");
    let url = format!("http://127.0.0.1:{port}{path}");
    let mut last = None;
    for _ in 0..50 {
        let mut b = client.request(method.clone(), &url).bearer_auth(TOKEN);
        if let Some(body) = body {
            b = b
                .header("content-type", "application/json")
                .body(body.to_string());
        }
        match b.send().await {
            Ok(r) => return r,
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    panic!("admin server never ready: {last:?}");
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

fn tenant() -> Tenant {
    Tenant {
        id: "t1".into(),
        name: "Acme".into(),
        domain: "acme.com".into(),
        auth_url: "https://auth.acme.com/verify".into(),
        cert_key: None,
        cert_file: None,
        enabled: true,
        created_at: NOW.into(),
        updated_at: NOW.into(),
    }
}

fn binding(id: &str, prefix: &str, provider_id: &str) -> ProviderKeyBinding {
    ProviderKeyBinding {
        id: id.into(),
        key_prefix: prefix.into(),
        provider_id: provider_id.into(),
        enabled: true,
        created_at: NOW.into(),
        updated_at: NOW.into(),
    }
}

fn sub_tenant(id: &str, name: &str, key_prefix: &str) -> SubTenant {
    SubTenant {
        id: id.into(),
        tenant_id: "t1".into(),
        name: name.into(),
        key_prefix: key_prefix.into(),
        enabled: true,
        created_at: NOW.into(),
        updated_at: NOW.into(),
    }
}

/// Base config: provider `p1` serving `gpt-4o`; tenant `t1` authorised for `p1`
/// with `gpt-4o` as its only allowed model. Reloaded into the snapshot so the
/// write validator sees it.
async fn seed_base(state: &AdminState) {
    let db = state.db();
    repo::insert_provider(db, &provider("p1", "openai"))
        .await
        .expect("insert p1");
    repo::insert_provider_model(db, &provider_model("pm1", "gpt-4o", "p1"))
        .await
        .expect("insert gpt-4o");
    repo::insert_tenant(db, &tenant()).await.expect("insert t1");
    repo::insert_tenant_provider(
        db,
        &TenantProvider {
            id: "tp1".into(),
            tenant_id: "t1".into(),
            provider_id: "p1".into(),
        },
    )
    .await
    .expect("insert t1→p1");
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

/// Extra: a second provider `p2` serving `solo`, authorised for `t1` (so a route
/// to `p1`/`solo` is "model not served by provider"), and a provider `p3` that
/// `t1` is NOT authorised for (so a route to `p3` is "provider not in tenant").
async fn seed_extra_providers(state: &AdminState) {
    let db = state.db();
    repo::insert_provider(db, &provider("p2", "anthropic"))
        .await
        .expect("insert p2");
    repo::insert_provider_model(db, &provider_model("pm2", "solo", "p2"))
        .await
        .expect("insert solo");
    repo::insert_provider(db, &provider("p3", "ghost"))
        .await
        .expect("insert p3");
    repo::insert_tenant_provider(
        db,
        &TenantProvider {
            id: "tp2".into(),
            tenant_id: "t1".into(),
            provider_id: "p2".into(),
        },
    )
    .await
    .expect("insert t1→p2");
    repo::insert_tenant_model(
        db,
        &TenantModel {
            id: "tm2".into(),
            tenant_id: "t1".into(),
            model_key: "solo".into(),
        },
    )
    .await
    .expect("insert t1 solo");
    state.store.reload_all().await.expect("reload extra");
}

// --- tests -------------------------------------------------------------------

/// 201 create (explicit + auto-generated), 409 duplicate name/prefix, 400
/// empty/no-separator prefix, and 400 overlap with an enabled operator binding.
#[tokio::test]
async fn sub_tenant_prefix_validation() {
    let state = admin_state().await;
    let port = start_admin(state.clone());
    seed_base(&state).await;
    // An enabled operator binding the new prefix may not overlap.
    repo::insert_provider_key_binding(state.db(), &binding("b1", "sk_aaa_", "p1"))
        .await
        .expect("insert binding");
    state.store.reload_all().await.expect("reload binding");

    // 201 create with an explicit valid prefix.
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/sub-tenants",
        Some(r#"{"tenant_id":"t1","name":"team-a","key_prefix":"QQCX_","enabled":true}"#),
    )
    .await;
    assert_eq!(r.status(), 201);
    let created: serde_json::Value = r.json().await.expect("json");
    assert_eq!(created["key_prefix"], "QQCX_");

    // 201 create with NO key_prefix ⇒ leader auto-generates 8×[A-Z0-9] + `_` (Q13).
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/sub-tenants",
        Some(r#"{"tenant_id":"t1","name":"team-b","enabled":true}"#),
    )
    .await;
    assert_eq!(r.status(), 201);
    let auto: serde_json::Value = r.json().await.expect("json");
    let prefix = auto["key_prefix"]
        .as_str()
        .expect("prefix string")
        .to_string();
    assert_eq!(prefix.len(), 9, "auto prefix must be 8 chars + '_'");
    assert!(prefix.ends_with('_'), "auto prefix must end with '_'");
    assert!(
        prefix[..8]
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()),
        "auto prefix body must be [A-Z0-9], got {prefix}"
    );

    // 409 duplicate name (different prefix).
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/sub-tenants",
        Some(r#"{"tenant_id":"t1","name":"team-a","key_prefix":"ZZZ_"}"#),
    )
    .await;
    assert_eq!(r.status(), 409, "duplicate name must be 409");

    // 409 duplicate prefix (different name).
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/sub-tenants",
        Some(r#"{"tenant_id":"t1","name":"team-c","key_prefix":"QQCX_"}"#),
    )
    .await;
    assert_eq!(r.status(), 409, "duplicate prefix must be 409");

    // 400 empty prefix (explicit empty string is distinct from omitted).
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/sub-tenants",
        Some(r#"{"tenant_id":"t1","name":"team-d","key_prefix":""}"#),
    )
    .await;
    assert_eq!(r.status(), 400, "empty prefix must be 400");

    // 400 no-separator prefix.
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/sub-tenants",
        Some(r#"{"tenant_id":"t1","name":"team-e","key_prefix":"QQCX"}"#),
    )
    .await;
    assert_eq!(r.status(), 400, "no-separator prefix must be 400");

    // 400 overlap with the enabled operator binding (sk_aaa_).
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/sub-tenants",
        Some(r#"{"tenant_id":"t1","name":"team-f","key_prefix":"sk_aaa_bbb"}"#),
    )
    .await;
    assert_eq!(r.status(), 400, "overlap with operator binding must be 400");
}

/// Route write validation: 201 valid, 400 provider-not-in-tenant, 400 model-not-
/// served-by-provider; and the hot snapshot reflects a created route.
#[tokio::test]
async fn sub_tenant_route_validation() {
    let state = admin_state().await;
    let port = start_admin(state.clone());
    seed_base(&state).await;
    seed_extra_providers(&state).await;

    // A sub-tenant to attach routes to.
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/sub-tenants",
        Some(r#"{"tenant_id":"t1","name":"team-a","key_prefix":"QQCX_"}"#),
    )
    .await;
    assert_eq!(r.status(), 201);
    let st: serde_json::Value = r.json().await.expect("json");
    let st_id = st["id"].as_str().expect("sub-tenant id").to_string();

    // 201 valid route (p1 serves gpt-4o; both in t1's providers/models).
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/sub-tenant-routes",
        Some(&format!(
            r#"{{"sub_tenant_id":"{st_id}","provider_id":"p1","model_key":"gpt-4o","enabled":true}}"#
        )),
    )
    .await;
    assert_eq!(r.status(), 201);
    // Hot snapshot reflects the enabled route.
    let snap = state.store.snapshot();
    assert!(
        snap.sub_tenant_routes
            .iter()
            .any(|r| r.sub_tenant_id == st_id && r.provider_id == "p1"),
        "created route must be in the hot snapshot"
    );
    drop(snap);

    // 400 provider not in tenant_providers (p3 exists but t1 is not authorised).
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/sub-tenant-routes",
        Some(&format!(
            r#"{{"sub_tenant_id":"{st_id}","provider_id":"p3","model_key":null,"enabled":true}}"#
        )),
    )
    .await;
    assert_eq!(r.status(), 400, "provider not in tenant must be 400");

    // 400 model not served by provider (p1 does not serve `solo`).
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/sub-tenant-routes",
        Some(&format!(
            r#"{{"sub_tenant_id":"{st_id}","provider_id":"p1","model_key":"solo","enabled":true}}"#
        )),
    )
    .await;
    assert_eq!(r.status(), 400, "model not served by provider must be 400");
}

/// Per-tenant sub-tenant quota: at `MAX_SUB_TENANTS_PER_TENANT` a create is
/// rejected 400 (the boundary).
#[tokio::test]
async fn sub_tenant_quota_exceeded() {
    let state = admin_state().await;
    let port = start_admin(state.clone());
    seed_base(&state).await;

    // Seed the tenant up to the cap.
    let db = state.db();
    for i in 0..hydra_core::sub_tenant::MAX_SUB_TENANTS_PER_TENANT {
        let prefix = format!("ST{i:02}_");
        let name = format!("st{i}");
        repo::insert_sub_tenant(db, &sub_tenant(&format!("st{i}"), &name, &prefix))
            .await
            .expect("seed sub-tenant");
    }
    state.store.reload_all().await.expect("reload quota");

    // The next create is over the quota ⇒ 400.
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/sub-tenants",
        Some(r#"{"tenant_id":"t1","name":"overflow","key_prefix":"ST64_"}"#),
    )
    .await;
    assert_eq!(r.status(), 400, "quota exceeded must be 400");
}

/// A-2 finding 1 — the quota counts **all** DB rows (including disabled), not
/// just the enabled snapshot. Here the tenant has `MAX` sub-tenants, ALL
/// disabled (so the enabled-only `cfg.sub_tenants` snapshot is empty for t1).
/// The in-transaction all-rows quota must still reject the next create.
#[tokio::test]
async fn sub_tenant_quota_counts_disabled_rows() {
    let state = admin_state().await;
    let port = start_admin(state.clone());
    seed_base(&state).await;

    // Seed the tenant up to the cap, ALL DISABLED (invisible to the enabled-only
    // snapshot, visible to the all-rows DB read).
    let db = state.db();
    for i in 0..hydra_core::sub_tenant::MAX_SUB_TENANTS_PER_TENANT {
        let mut st = sub_tenant(&format!("st{i}"), &format!("st{i}"), &format!("ST{i:02}_"));
        st.enabled = false;
        repo::insert_sub_tenant(db, &st)
            .await
            .expect("seed disabled sub-tenant");
    }
    state
        .store
        .reload_all()
        .await
        .expect("reload disabled quota");

    // The enabled-only snapshot has 0 rows for t1, but the DB has MAX rows. The
    // all-rows quota must reject this create (v1's enabled-only count would let
    // it through — the bug being fixed).
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/sub-tenants",
        Some(r#"{"tenant_id":"t1","name":"overflow","key_prefix":"ST64_"}"#),
    )
    .await;
    assert_eq!(
        r.status(),
        400,
        "all-rows quota must reject when only disabled rows exist"
    );
}

/// A-2 finding 2 — the prefix-overlap check runs against **all** rows
/// (including disabled), not just the enabled snapshot. Create A, disable it
/// (so it leaves the enabled-only snapshot), then create B with an overlapping
/// prefix: B must be rejected 400.
#[tokio::test]
async fn sub_tenant_overlap_with_disabled_row_rejected() {
    let state = admin_state().await;
    let port = start_admin(state.clone());
    seed_base(&state).await;

    // Create A with prefix QQCX_.
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/sub-tenants",
        Some(r#"{"tenant_id":"t1","name":"team-a","key_prefix":"QQCX_"}"#),
    )
    .await;
    assert_eq!(r.status(), 201);
    let st: serde_json::Value = r.json().await.expect("json");
    let st_id = st["id"].as_str().expect("id").to_string();

    // Disable A (it now leaves the enabled-only snapshot).
    let r = req(
        port,
        reqwest::Method::PUT,
        &format!("/api/v1/sub-tenants/{st_id}"),
        Some(r#"{"tenant_id":"t1","name":"team-a","key_prefix":"QQCX_","enabled":false}"#),
    )
    .await;
    assert_eq!(r.status(), 200);

    // B's prefix QQCX_WXYZ overlaps A's QQCX_ (starts_with). Must be rejected
    // 400 — the overlap check sees the disabled A row (all-rows), which the
    // enabled-only snapshot would not.
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/sub-tenants",
        Some(r#"{"tenant_id":"t1","name":"team-b","key_prefix":"QQCX_WXYZ"}"#),
    )
    .await;
    assert_eq!(
        r.status(),
        400,
        "overlap with a disabled row must be rejected (all-rows)"
    );
}

/// A-2 finding 2 — a sequential overlapping (non-equal) prefix is rejected by
/// the in-transaction check. The DB `UNIQUE(tenant_id, key_prefix)` constraint
/// only catches EXACT equality, not overlap, so this proves the in-transaction
/// all-rows overlap validation is the line of defence.
#[tokio::test]
async fn sub_tenant_sequential_overlap_rejected() {
    let state = admin_state().await;
    let port = start_admin(state.clone());
    seed_base(&state).await;

    // Create A.
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/sub-tenants",
        Some(r#"{"tenant_id":"t1","name":"team-a","key_prefix":"QQCX_"}"#),
    )
    .await;
    assert_eq!(r.status(), 201);

    // B's prefix QQCX_MORE overlaps A's QQCX_ (starts_with, non-equal). Must be
    // rejected 400 by the in-transaction overlap check.
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/sub-tenants",
        Some(r#"{"tenant_id":"t1","name":"team-b","key_prefix":"QQCX_MORE"}"#),
    )
    .await;
    assert_eq!(
        r.status(),
        400,
        "overlapping (non-equal) prefix must be rejected in-tx"
    );
}

/// CRUD lifecycle: 200 list, 200 PUT, 404, 204 delete — and the hot snapshot
/// tracks each mutation after reload.
#[tokio::test]
async fn sub_tenant_crud_lifecycle() {
    let state = admin_state().await;
    let port = start_admin(state.clone());
    seed_base(&state).await;

    // Create.
    let r = req(
        port,
        reqwest::Method::POST,
        "/api/v1/sub-tenants",
        Some(r#"{"tenant_id":"t1","name":"team-a","key_prefix":"QQCX_"}"#),
    )
    .await;
    assert_eq!(r.status(), 201);
    let st: serde_json::Value = r.json().await.expect("json");
    let st_id = st["id"].as_str().expect("id").to_string();

    // Hot snapshot reflects the created (enabled) sub-tenant.
    let snap = state.store.snapshot();
    assert!(
        snap.sub_tenants.iter().any(|s| s.id == st_id),
        "created sub-tenant must be in the hot snapshot"
    );
    drop(snap);

    // 200 list.
    let r = req(port, reqwest::Method::GET, "/api/v1/sub-tenants", None).await;
    assert_eq!(r.status(), 200);
    let list: serde_json::Value = r.json().await.expect("json");
    assert!(list.as_array().map(|a| !a.is_empty()).unwrap_or(false));

    // 200 PUT (rename + change prefix).
    let r = req(
        port,
        reqwest::Method::PUT,
        &format!("/api/v1/sub-tenants/{st_id}"),
        Some(r#"{"tenant_id":"t1","name":"team-a2","key_prefix":"QQCX_V2_","enabled":true}"#),
    )
    .await;
    assert_eq!(r.status(), 200);
    let item: serde_json::Value = r.json().await.expect("json");
    assert_eq!(item["name"], "team-a2");
    assert_eq!(item["key_prefix"], "QQCX_V2_");

    // Hot snapshot reflects the PUT.
    let snap = state.store.snapshot();
    assert!(
        snap.sub_tenants
            .iter()
            .any(|s| s.id == st_id && s.key_prefix == "QQCX_V2_"),
        "updated sub-tenant must be in the hot snapshot"
    );
    drop(snap);

    // 404 unknown id.
    let r = req(port, reqwest::Method::GET, "/api/v1/sub-tenants/nope", None).await;
    assert_eq!(r.status(), 404);

    // 204 delete.
    let r = req(
        port,
        reqwest::Method::DELETE,
        &format!("/api/v1/sub-tenants/{st_id}"),
        None,
    )
    .await;
    assert_eq!(r.status(), 204);

    // Hot snapshot no longer carries the deleted sub-tenant.
    let snap = state.store.snapshot();
    assert!(
        !snap.sub_tenants.iter().any(|s| s.id == st_id),
        "deleted sub-tenant must leave the hot snapshot"
    );
    drop(snap);
}
