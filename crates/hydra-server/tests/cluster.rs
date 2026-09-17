//! Cluster P1 integration: control-plane snapshot distribution.
//!
//! Exercises the REAL leader control endpoint (`/api/v1/internal/control`,
//! cluster-token gated) with a REAL `ControlClient` polling it and applying
//! hydrated snapshots to an edge `ConfigStore` — no mocks on the seam: the
//! wire format, the sealing/hydrating crypto, the HTTP channel and the
//! last-known-good failure path are all production code.

mod common;

use std::sync::Arc;
use std::time::Duration;

use hydra_core::breaker::BreakerConfig;
use hydra_core::config::ConfigData;
use hydra_core::model::{
    Provider, ProviderKey, ProviderModel, Tenant, TenantModel, TenantProvider,
};
use hydra_server::admin::{AdminService, AdminState};
use hydra_server::cluster::control_client::{ControlClient, ControlClientConfig, ControlResponse};
use hydra_server::cluster::lease::{LeaderElection, MemoryLeaseStore};
use hydra_server::cluster::replica;
use hydra_server::cluster::snapshot::{SnapshotError, SnapshotWire, WIRE_VERSION};
use hydra_server::crypto::{KeyProvider, StaticKeyProvider};
use hydra_server::db as repo;
use hydra_server::http::{AuthCache, AuthConfig, HttpAuthChecker};
use hydra_server::proxy::admission::AdmissionControl;
use hydra_server::proxy::breaker_wrap::CircuitBreaker;
use hydra_server::store::{build_config, ConfigStore};
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::services::listening::Service;

const CLUSTER_TOKEN: &str = "cluster-tok";
const ADMIN_TOKEN: &str = "admin-tok";

fn kp() -> StaticKeyProvider {
    StaticKeyProvider::new([1u8; 32], 1)
}

fn now() -> &'static str {
    "2026-01-01 00:00:00"
}

/// Bind an ephemeral port, return it, then release the socket so Pingora can
/// rebind. (Same TOCTOU-tolerant pattern as the W4 spike test.)
/// See `common::ephemeral_port`: no bind-then-release race (the previous
/// local copy could hand two concurrent tests the same port).
fn ephemeral_port() -> u16 {
    common::ephemeral_port()
}

/// Start a real Pingora `Service` hosting `AdminService` on an ephemeral port.
///
/// Blocks until the listener actually accepts connections: `run_forever` binds
/// asynchronously on the spawned thread, and a test that fires a request the
/// moment `start_admin` returns can race the bind (CI flake: ConnectionRefused
/// on the first probe). Polling a TCP connect keeps every caller race-free.
fn start_admin(state: Arc<AdminState>) -> u16 {
    let port = ephemeral_port();
    let addr = format!("127.0.0.1:{port}");
    let app = AdminService::new(state);
    let mut server = Server::new(Some(Opt::default())).expect("Server::new");
    server.bootstrap();
    let mut svc = Service::new("cluster-test".to_string(), app);
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

/// Leader-side components: a real DB-backed store + a cluster-token admin.
async fn leader() -> (sqlx::SqlitePool, ConfigStore, u16) {
    let pool = common::setup_pool().await;
    let key_provider: Arc<dyn KeyProvider> = Arc::new(kp());
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
    let state = Arc::new(AdminState::new(
        Some(pool.clone()),
        store.clone(),
        auth,
        breaker,
        key_provider,
        Some(ADMIN_TOKEN.to_string()),
        AdmissionControl::new(),
        false,
        Some(CLUSTER_TOKEN.to_string()),
        None, // no leader election in the leader() test harness
    ));
    let port = start_admin(state);
    (pool, store, port)
}

/// Seed the leader DB with a tenant + provider + (encrypted) provider key,
/// then reload so the version bumps and the snapshot carries them.
async fn seed_and_reload(pool: &sqlx::SqlitePool, store: &ConfigStore) {
    repo::insert_tenant(
        pool,
        &Tenant {
            id: "t1".into(),
            name: "T".into(),
            domain: "acme.com".into(),
            auth_url: "https://auth.acme.com/v".into(),
            cert_key: None,
            cert_file: None,
            enabled: true,
            created_at: now().into(),
            updated_at: now().into(),
        },
    )
    .await
    .expect("insert tenant");
    repo::insert_provider(
        pool,
        &Provider {
            id: "p1".into(),
            key: "openai".into(),
            name: "O".into(),
            endpoint: "https://api.openai.com".into(),
            weight: 1,
            created_at: now().into(),
            updated_at: now().into(),
            max_concurrency: None,
            max_queue_depth: None,
            queue_wait_timeout_ms: None,
        },
    )
    .await
    .expect("insert provider");
    repo::insert_provider_key(
        pool,
        &kp(),
        &ProviderKey {
            id: "k1".into(),
            provider_id: "p1".into(),
            api_key: "sk-upstream-secret".into(),
            created_at: now().into(),
        },
    )
    .await
    .expect("insert provider key");
    repo::insert_provider_model(
        pool,
        &ProviderModel {
            id: "m1".into(),
            key: "gpt-4".into(),
            name: "GPT-4".into(),
            provider_id: "p1".into(),
            status: 1,
        },
    )
    .await
    .expect("insert online model");
    repo::insert_provider_model(
        pool,
        &ProviderModel {
            id: "m2".into(),
            key: "gpt-4-offline".into(),
            name: "GPT-4 (disabled)".into(),
            provider_id: "p1".into(),
            status: 0,
        },
    )
    .await
    .expect("insert offline model");
    repo::insert_tenant_provider(
        pool,
        &TenantProvider {
            id: "tp1".into(),
            tenant_id: "t1".into(),
            provider_id: "p1".into(),
        },
    )
    .await
    .expect("insert tenant provider");
    repo::insert_tenant_model(
        pool,
        &TenantModel {
            id: "tm1".into(),
            tenant_id: "t1".into(),
            model_key: "gpt-4".into(),
        },
    )
    .await
    .expect("insert tenant model");
    store.reload_all().await.expect("reload");
}

/// T-CL-1 — an edge polls the leader and applies the hydrated snapshot
/// (config + decrypted provider key) with the leader's version.
#[tokio::test]
async fn edge_applies_leader_snapshot() {
    let (pool, leader_store, port) = leader().await;
    seed_and_reload(&pool, &leader_store).await;

    let edge_store = ConfigStore::from_snapshot(ConfigData::default(), Arc::new(kp()));
    let client = ControlClient::new(
        ControlClientConfig {
            url: format!("http://127.0.0.1:{port}"),
            token: CLUSTER_TOKEN.to_string(),
            poll_interval: Duration::from_millis(50),
        },
        edge_store.clone(),
        Arc::new(kp()),
        None,
    );

    let mut applied = false;
    for _ in 0..50 {
        let _ = client.poll_once().await;
        if edge_store
            .snapshot()
            .tenants_by_domain
            .contains_key("acme.com")
        {
            applied = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(applied, "edge must apply the leader's snapshot");

    assert_eq!(
        edge_store.version(),
        leader_store.version(),
        "edge adopts the leader's version"
    );
    assert!(edge_store.snapshot().providers.contains_key("p1"));
    assert_eq!(
        edge_store.snapshot().provider_keys["p1"],
        vec!["sk-upstream-secret".to_string()],
        "the sealed provider key decrypts back to the plaintext on the edge"
    );
}

/// T-CL-2 — the control endpoint is gated by the CLUSTER token, not the admin
/// token; a wrong cluster token is rejected with 401.
#[tokio::test]
async fn control_endpoint_requires_cluster_token() {
    let (pool, leader_store, port) = leader().await;
    seed_and_reload(&pool, &leader_store).await;

    let get = |tok: Option<String>| {
        let url = format!("http://127.0.0.1:{port}/api/v1/internal/control");
        async move {
            let client = reqwest::Client::new();
            // Retry until the admin server is listening (bind races with
            // the test thread).
            for _ in 0..50 {
                let mut req = client.get(url.clone());
                if let Some(t) = &tok {
                    req = req.header("authorization", format!("Bearer {t}"));
                }
                match req.send().await {
                    Ok(r) => return r.status().as_u16(),
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
            panic!("admin server did not come up");
        }
    };

    assert_eq!(
        get(Some(CLUSTER_TOKEN.to_string())).await,
        200,
        "cluster token accepted"
    );
    assert_eq!(
        get(Some(ADMIN_TOKEN.to_string())).await,
        401,
        "admin token is NOT the cluster token"
    );
    assert_eq!(get(None).await, 401, "no token rejected");
    assert_eq!(get(Some("wrong".to_string())).await, 401);
}

/// T-CL-3 — last-known-good on hydrate failure: an edge whose master key
/// cannot decrypt the leader's sealed material keeps its previous snapshot.
#[tokio::test]
async fn edge_keeps_last_known_good_on_decrypt_failure() {
    let (pool, leader_store, port) = leader().await;
    seed_and_reload(&pool, &leader_store).await;

    // Wrong master key: [9u8;32] vs the leader's [1u8;32].
    let wrong_kp: Arc<dyn KeyProvider> = Arc::new(StaticKeyProvider::new([9u8; 32], 1));
    let edge_store = ConfigStore::from_snapshot(ConfigData::default(), wrong_kp.clone());
    let client = ControlClient::new(
        ControlClientConfig {
            url: format!("http://127.0.0.1:{port}"),
            token: CLUSTER_TOKEN.to_string(),
            poll_interval: Duration::from_millis(50),
        },
        edge_store.clone(),
        wrong_kp,
        None,
    );

    assert!(
        client.poll_once().await.is_err(),
        "decrypt failure must surface as a poll error"
    );
    assert!(
        edge_store.snapshot().tenants_by_domain.is_empty(),
        "the edge keeps its last-known-good (empty) snapshot"
    );
    assert_eq!(
        edge_store.version(),
        0,
        "version unchanged on failure (an edge that never synced holds nothing: version 0)"
    );
}

// ===========================================================================
// T1 — the wire contract is FAIL-CLOSED IN BOTH DIRECTIONS (plan O1/O3/O4).
//
// The v2 wire moved the three fidelity row-sets the v1 reader REQUIRED
// (`provider_models` / `tenant_providers` / `tenant_models`) INSIDE a nested
// `fidelity` object, added `wire_version`, and changed the `sealed_provider_keys`
// value shape. Two mixed-version windows must therefore be safe:
//   * a NEW reader must reject an OLD wire (and keep last-known-good), and
//   * an OLD reader must reject a NEW wire UNCONDITIONALLY — if it did not, the
//     old reader would ignore the new fields and silently execute the G1 data
//     destruction (wipe the fidelity tables and rebuild them from the
//     ENABLED-only `cfg`).
//
// The unconditional part is what these tests pin: v1 of the plan claimed the
// change to `sealed_provider_keys`' value type was the guard, but that fails
// only when the map is NON-EMPTY. The empty-payload case is covered below.
// ===========================================================================

/// A v1-shaped reader: exactly the pre-T1 `SnapshotWire`, WITHOUT
/// `deny_unknown_fields` (v1 had none — that is what makes the old reader
/// silently tolerant of the new fields).
#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct LegacyReaderWire {
    version: u64,
    cfg: ConfigData,
    sealed_provider_keys: std::collections::HashMap<String, Vec<LegacySealedDto>>,
    sealed_certs: std::collections::HashMap<String, serde_json::Value>,
    provider_models: Vec<serde_json::Value>,
    tenant_providers: Vec<serde_json::Value>,
    tenant_models: Vec<serde_json::Value>,
}

/// v1's per-key payload: `{ciphertext, nonce, key_version}` — no identity.
#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct LegacySealedDto {
    ciphertext: Vec<u8>,
    nonce: Vec<u8>,
    key_version: u32,
}

/// Serve ONE fixed JSON body for every request, over a raw socket.
///
/// `ControlClient` is only constructible with a URL, and the point of this test
/// is the PARSER — so the peer is a stub that emits v1 bytes, not a v2 leader.
fn start_json_stub(json: String) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind stub");
    let port = listener.local_addr().expect("stub local_addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            // Drain the request head so the client sees a complete exchange.
            let mut buf = [0u8; 8192];
            let _ = std::io::Read::read(&mut stream, &mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                json.len(),
                json
            );
            let _ = std::io::Write::write_all(&mut stream, resp.as_bytes());
            let _ = std::io::Write::flush(&mut stream);
        }
    });
    port
}

/// The v1 wire as JSON: fidelity rows at the TOP LEVEL, no `wire_version`, and
/// the old `sealed_provider_keys` value shape (identity-less blobs).
fn legacy_wire_json(version: u64) -> String {
    serde_json::json!({
        "version": version,
        "snapshot": {
            "version": version,
            "cfg": ConfigData::default(),
            "sealed_provider_keys": {},
            "sealed_certs": {},
            "provider_models": [],
            "tenant_providers": [],
            "tenant_models": [],
        }
    })
    .to_string()
}

/// (1) An OLD wire must not be accepted by the NEW reader: `ControlResponse`
/// fails to deserialize (missing `wire_version` / `fidelity`), the poll surfaces
/// an error, and the edge keeps its last-known-good snapshot untouched.
#[tokio::test]
async fn old_wire_is_rejected_and_last_known_good_is_kept() {
    // A genuinely non-empty last-known-good: the edge already holds the
    // leader's config (as if it had synced before the leader was upgraded).
    let (leader_pool, leader_store, _port) = leader().await;
    seed_and_reload(&leader_pool, &leader_store).await;
    let kp: Arc<dyn KeyProvider> = Arc::new(kp());
    let good: ConfigData = leader_store.snapshot().as_ref().clone();
    assert!(!good.providers.is_empty(), "the fixture is non-empty");

    let legacy = legacy_wire_json(leader_store.version() + 1);
    assert!(
        serde_json::from_str::<ControlResponse>(&legacy).is_err(),
        "the new reader must reject the v1 wire shape outright"
    );

    let edge_store = ConfigStore::from_snapshot(good.clone(), kp.clone());
    let outcomes: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = outcomes.clone();
    let hook: hydra_server::cluster::control_client::PollHook =
        Arc::new(move |o| sink.lock().expect("hook mutex").push(format!("{o:?}")));

    let port = start_json_stub(legacy);
    let client = ControlClient::new(
        ControlClientConfig {
            url: format!("http://127.0.0.1:{port}"),
            token: CLUSTER_TOKEN.to_string(),
            poll_interval: Duration::from_millis(50),
        },
        edge_store.clone(),
        kp.clone(),
        Some(hook),
    );

    let res = client.poll_once().await;
    assert!(res.is_err(), "a v1 wire must fail the poll: {res:?}");
    assert!(
        outcomes.lock().expect("hook mutex")[0].contains("Error"),
        "the poll hook must report `PollOutcome::Error`, got {:?}",
        outcomes.lock().expect("hook mutex")
    );
    assert_eq!(
        edge_store.snapshot().as_ref(),
        &good,
        "the edge keeps its last-known-good config"
    );
    assert_eq!(
        edge_store.version(),
        0,
        "and its watermark never advanced from a rejected payload"
    );
}

/// (2) A newer/unknown `wire_version` is rejected by the version check, and that
/// check is the ONLY reachable path to `SnapshotError::WireVersion` (a v1 wire
/// cannot even deserialize into `SnapshotWire`).
#[tokio::test]
async fn unknown_wire_version_is_rejected() {
    let pool = common::setup_pool().await;
    let kp: Arc<dyn KeyProvider> = Arc::new(kp());
    let store = ConfigStore::load(pool.clone(), kp.clone())
        .await
        .expect("load");
    let content = store.replication().as_deref().cloned().expect("content");
    let wire = SnapshotWire::build(&content, kp.as_ref())
        .await
        .expect("build");

    let mut json: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&wire).expect("ser")).expect("json");
    assert_eq!(json["wire_version"], WIRE_VERSION, "the wire is stamped");
    json["wire_version"] = serde_json::json!(3);

    let future: SnapshotWire =
        serde_json::from_value(json.clone()).expect("a same-shape wire still deserializes");
    match future.hydrate(kp.as_ref()) {
        Ok(_) => panic!("hydrate must reject an unknown wire_version"),
        Err(SnapshotError::WireVersion { found, expected }) => {
            assert_eq!(found, 3, "the reader reports what it FOUND");
            assert_eq!(expected, WIRE_VERSION, "and what it supports");
        }
        Err(other) => panic!("expected SnapshotError::WireVersion, got {other:?}"),
    }

    // The version field must stay REQUIRED: `#[serde(default)]` would make a
    // missing version look like a valid one and re-open the "new reader accepts
    // an old wire" direction — i.e. put the G1 data loss back.
    let mut missing = json;
    missing
        .as_object_mut()
        .expect("object")
        .remove("wire_version");
    assert!(
        serde_json::from_value::<SnapshotWire>(missing).is_err(),
        "`wire_version` must NOT have a serde default"
    );
}

/// (3) An OLD reader must reject a NEW wire UNCONDITIONALLY — including the
/// empty-payload case that v1's acceptance text used to miss. The failure is
/// `missing field provider_models`: the field the old reader required is now
/// nested inside `fidelity`, so no payload shape can satisfy both readers.
#[tokio::test]
async fn old_reader_rejects_the_new_wire_unconditionally() {
    let kp: Arc<dyn KeyProvider> = Arc::new(kp());

    // (a) EMPTY payload — the loophole v1 relied on: with the old
    // `sealed_provider_keys` shape match skipped, only the moved fields can
    // fail, so this asserts they DO.
    let empty_pool = common::setup_pool().await;
    let empty_store = ConfigStore::load(empty_pool.clone(), kp.clone())
        .await
        .expect("load");
    let empty_content = empty_store
        .replication()
        .as_deref()
        .cloned()
        .expect("content");
    let empty_wire = SnapshotWire::build(&empty_content, kp.as_ref())
        .await
        .expect("build");
    assert!(
        empty_wire.sealed_provider_keys.is_empty(),
        "fixture: no sealed provider keys"
    );
    let empty_json = serde_json::to_string(&empty_wire).expect("ser");
    let err = serde_json::from_str::<LegacyReaderWire>(&empty_json)
        .expect_err("an old reader must NOT accept an empty-payload v2 wire");
    assert!(
        err.to_string().contains("provider_models"),
        "the old reader must fail on the MOVED field, got: {err}"
    );

    // (b) POPULATED config with an EMPTY key map — the exact input v1's
    // acceptance text failed to cover, and the dangerous one: the old reader
    // would otherwise proceed to wipe the replica's fidelity tables and rebuild
    // them from the ENABLED-only `cfg` (G1). Providers/tenants/models are
    // present, so `fidelity` carries real rows while `sealed_provider_keys`
    // stays `{}`.
    let (leader_pool, leader_store, _port) = leader().await;
    seed_and_reload(&leader_pool, &leader_store).await;
    repo::delete_provider_key(&leader_pool, "k1")
        .await
        .expect("drop the only provider key");
    leader_store.reload_all().await.expect("reload");
    let content = leader_store
        .replication()
        .as_deref()
        .cloned()
        .expect("content");
    let wire = SnapshotWire::build(&content, kp.as_ref())
        .await
        .expect("build");
    assert!(
        wire.sealed_provider_keys.is_empty(),
        "fixture: the key map is empty on purpose"
    );
    assert!(
        !wire.fidelity.provider_models.is_empty(),
        "fixture: ...while the moved fidelity rows are NOT"
    );
    let json = serde_json::to_string(&wire).expect("ser");
    assert!(
        json.contains("fidelity"),
        "fixture: the rows really are nested under `fidelity`"
    );
    let err = serde_json::from_str::<LegacyReaderWire>(&json)
        .expect_err("an old reader must NOT accept an empty-key v2 wire");
    assert!(
        err.to_string().contains("provider_models"),
        "the old reader must fail on the MOVED field, got: {err}"
    );

    // (c) WITH provider keys the old reader is rejected even earlier, by the
    // changed per-key shape (`LegacySealedDto` wants `ciphertext`, the v2 shape
    // nests it under `sealed`). Two independent guards, either one is fatal —
    // asserted so a future refactor cannot quietly remove both.
    let (key_pool, key_store, _port) = leader().await;
    seed_and_reload(&key_pool, &key_store).await;
    let key_content = key_store
        .replication()
        .as_deref()
        .cloned()
        .expect("content");
    let key_wire = SnapshotWire::build(&key_content, kp.as_ref())
        .await
        .expect("build");
    assert!(
        !key_wire.sealed_provider_keys.is_empty(),
        "fixture: sealed provider keys present"
    );
    let key_json = serde_json::to_string(&key_wire).expect("ser");
    let err = serde_json::from_str::<LegacyReaderWire>(&key_json)
        .expect_err("an old reader must NOT accept a populated v2 wire");
    let msg = err.to_string();
    assert!(
        msg.contains("provider_models") || msg.contains("ciphertext"),
        "the rejection must come from one of the two structural changes, got: {msg}"
    );
}

// ===========================================================================
// T4 / audit G9 — the snapshot PRODUCER must hold the lease.
//
// `internal_control` used to be protected by the cluster token ALONE: any node
// holding that token could mint a snapshot, including one that had just lost
// the lease while its heartbeat was still fresh. Edges following that node would
// rebuild their replica from a stale producer. The lease gate (`503
// not_leader`) simply did not exist.
// ===========================================================================

/// An admin state over a REAL DB-backed store (so `replication()` is `Some`),
/// with the lease answer under the test's control.
async fn control_state(
    pool: &sqlx::SqlitePool,
    store: &ConfigStore,
    edge_mode: bool,
    is_leader: Option<bool>,
) -> Arc<AdminState> {
    let key_provider: Arc<dyn KeyProvider> = Arc::new(kp());
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
    Arc::new(AdminState::new(
        Some(pool.clone()),
        store.clone(),
        auth,
        breaker,
        key_provider,
        Some(ADMIN_TOKEN.to_string()),
        AdmissionControl::new(),
        edge_mode,
        Some(CLUSTER_TOKEN.to_string()),
        leader_ready,
    ))
}

/// `GET /api/v1/internal/control?since=N` → (status, body).
async fn control_get(port: u16, since: u64) -> (u16, serde_json::Value) {
    let url = format!("http://127.0.0.1:{port}/api/v1/internal/control?since={since}");
    let client = reqwest::Client::new();
    for _ in 0..50 {
        match client
            .get(&url)
            .header("authorization", format!("Bearer {CLUSTER_TOKEN}"))
            .send()
            .await
        {
            Ok(r) => {
                let status = r.status().as_u16();
                let body = r.json().await.unwrap_or(serde_json::Value::Null);
                return (status, body);
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    panic!("admin server did not come up");
}

/// A candidate that lost (or never held) the lease must NOT hand out a snapshot
/// — but the cheap "you are already current" answer stays available, because
/// gating that would break the poll rhythm of every follower.
#[tokio::test]
async fn control_snapshot_requires_the_leader_lease() {
    let leader_pool = common::setup_pool().await;
    let key_provider: Arc<dyn KeyProvider> = Arc::new(kp());
    let store = ConfigStore::load(leader_pool.clone(), key_provider.clone())
        .await
        .expect("load");
    seed_and_reload(&leader_pool, &store).await;
    let current = store.version();
    assert!(current > 0, "fixture: the store has content");

    // (a) Candidate WITHOUT the lease ⇒ 503 `not_leader`.
    let standby = control_state(&leader_pool, &store, false, Some(false)).await;
    let port = start_admin(standby);
    let (status, body) = control_get(port, 0).await;
    assert_eq!(status, 503, "a standby must not produce snapshots: {body}");
    assert_eq!(body["error"]["code"], "not_leader", "got {body}");
    assert!(
        body.get("snapshot").is_none(),
        "no snapshot may leak in an error body: {body}"
    );

    // (b) ...but `since >= current` is answered normally (cheap path, no lease
    //     gate): this is what keeps a follower's poll a no-op instead of an
    //     error storm while the fleet has no leader.
    let (status, body) = control_get(port, current).await;
    assert_eq!(status, 200, "got {body}");
    assert_eq!(body["version"].as_u64(), Some(current));
    assert_eq!(body["snapshot"], serde_json::Value::Null, "no payload sent");
    let (status, body) = control_get(port, current + 10).await;
    assert_eq!(status, 200, "a follower AHEAD of us is still fine: {body}");
    assert_eq!(body["snapshot"], serde_json::Value::Null);

    // (c) Candidate HOLDING the lease ⇒ a real snapshot, versioned from the SAME
    //     atomic read as the cheap path.
    let holder = control_state(&leader_pool, &store, false, Some(true)).await;
    let port = start_admin(holder);
    let (status, body) = control_get(port, 0).await;
    assert_eq!(status, 200, "the lease holder serves the snapshot: {body}");
    assert_eq!(body["version"].as_u64(), Some(current));
    assert!(
        body["snapshot"].is_object(),
        "the payload is present for a behind follower: {body}"
    );
    assert_eq!(
        body["snapshot"]["version"].as_u64(),
        Some(current),
        "the wire version equals the version we advertised"
    );

    // (d) Candidate with NO election wired (`leader_ready: None`, the
    //     single-node/`all` shape) keeps serving — pre-existing behaviour.
    let all = control_state(&leader_pool, &store, false, None).await;
    let port = start_admin(all);
    let (status, body) = control_get(port, 0).await;
    assert_eq!(status, 200, "no election ⇒ no lease gate: {body}");
    assert!(body["snapshot"].is_object(), "got {body}");
}

/// A non-candidate (edge) gets the PRE-EXISTING 404 "edge node: no admin API"
/// from the router, before dispatch — and crucially it does not panic (the
/// endpoint must never reach a `.expect` on a missing pool).
#[tokio::test]
async fn control_snapshot_on_an_edge_is_404_and_does_not_panic() {
    let leader_pool = common::setup_pool().await;
    let key_provider: Arc<dyn KeyProvider> = Arc::new(kp());
    let store = ConfigStore::load(leader_pool.clone(), key_provider.clone())
        .await
        .expect("load");
    seed_and_reload(&leader_pool, &store).await;

    // edge_mode + `leader_ready: Some(true)`: even a node that THINKS it holds
    // the lease is not a candidate when it is an edge.
    let edge = control_state(&leader_pool, &store, true, Some(true)).await;
    let port = start_admin(edge);
    let (status, body) = control_get(port, 0).await;
    assert_eq!(status, 404, "edge has no admin API: {body}");
    assert_eq!(body["error"]["code"], "not_found", "got {body}");
    assert_eq!(
        body["error"]["message"], "edge node: no admin API",
        "the pre-existing edge response, unchanged: {body}"
    );

    // The service is still alive (a panic would have killed the connection and
    // the second call would fail): the probe endpoint answers.
    let client = reqwest::Client::new();
    let r = client
        .get(format!("http://127.0.0.1:{port}/healthz"))
        .send()
        .await
        .expect("edge stays up");
    assert_eq!(r.status().as_u16(), 200);
}

// ===========================================================================
// P2 — leader election & standby replica
// ===========================================================================

/// T-CL-4 — standby replica materialization: the full-table rebuild from a
/// control snapshot preserves EVERYTHING (incl. offline models and grant
/// rows), so a promoted standby is a faithful copy of the active.
#[tokio::test]
async fn standby_materializes_replica() {
    let (leader_pool, leader_store, _port) = leader().await;
    seed_and_reload(&leader_pool, &leader_store).await;

    let kp: Arc<dyn KeyProvider> = Arc::new(kp());
    // A tenant access-token HASH. It lives in no `ConfigData` field, and before
    // T1 the wire carried no token hashes at all — a promoted replica answered
    // `has_access_token: false` and 401'd every tenant-API request. Set it
    // BEFORE the wire is built.
    let token_hash = "5f4dcc3b5aa765d61d8327deb882cf99";
    repo::set_tenant_access_token_hash(&leader_pool, "t1", Some(token_hash))
        .await
        .expect("set token hash");
    leader_store.reload_all().await.expect("reload");

    // The wire is built from the leader's own replication content: version and
    // payload come from ONE atomic read, so they cannot drift.
    let content = leader_store
        .replication()
        .as_deref()
        .cloned()
        .expect("the leader has replication content");
    let wire = SnapshotWire::build(&content, kp.as_ref())
        .await
        .expect("build wire");
    assert_eq!(
        content.fidelity().tenant_token_hashes,
        vec![("t1".to_string(), token_hash.to_string())],
        "the leader's replication content carries the token hash"
    );
    // Sealed on the wire (plan O25): the hash must not appear as plaintext JSON,
    // or the control channel would leak a bearer-equivalent secret.
    let wire_json = serde_json::to_string(&wire).expect("serialize wire");
    assert!(
        !wire_json.contains(token_hash),
        "the access-token hash must be SEALED on the wire, not plaintext"
    );

    let replica_pool = common::setup_pool().await;
    replica::materialize(&replica_pool, kp.as_ref(), &wire)
        .await
        .expect("materialize");

    let leader_cfg = build_config(&leader_pool, kp.as_ref())
        .await
        .expect("leader cfg");
    let replica_cfg = build_config(&replica_pool, kp.as_ref())
        .await
        .expect("replica cfg");
    assert_eq!(replica_cfg.providers, leader_cfg.providers);
    assert_eq!(replica_cfg.provider_keys, leader_cfg.provider_keys);
    assert_eq!(replica_cfg.tenants_by_domain, leader_cfg.tenants_by_domain);
    assert_eq!(replica_cfg.models_by_key, leader_cfg.models_by_key);
    assert_eq!(replica_cfg.tenant_providers, leader_cfg.tenant_providers);
    assert_eq!(replica_cfg.tenant_models, leader_cfg.tenant_models);

    // G1 THROUGH THE REAL PLUMBING: the disabled rows must survive
    // `replica::materialize` (hydrate → restore_config), not just a direct
    // `restore_config` call — a regression inside `materialize` that handed the
    // rebuild the ENABLED-only `cfg` instead of the fidelity rows would pass a
    // direct-call test and destroy the replica's disabled rows in production.
    repo::insert_limit_role(
        &leader_pool,
        &hydra_core::model::LimitRole {
            id: "r-off".into(),
            name: "disabled role".into(),
            matching_key: None,
            matching_model: None,
            matching_tenant: None,
            matching_provider: None,
            limit_count: Some(5),
            limit_token: None,
            window: "m".into(),
            enabled: false,
            created_at: now().into(),
        },
    )
    .await
    .expect("insert disabled role");
    repo::insert_provider_key_binding(
        &leader_pool,
        &hydra_core::model::ProviderKeyBinding {
            id: "b-off".into(),
            key_prefix: "sk-off-".into(),
            provider_id: "p1".into(),
            enabled: false,
            created_at: now().into(),
            updated_at: now().into(),
        },
    )
    .await
    .expect("insert disabled binding");
    leader_store.reload_all().await.expect("reload");

    // Rebuild the wire from the (now larger) content and re-materialize.
    let content = leader_store
        .replication()
        .as_deref()
        .cloned()
        .expect("content");
    let wire = SnapshotWire::build(&content, kp.as_ref())
        .await
        .expect("build wire");
    replica::materialize(&replica_pool, kp.as_ref(), &wire)
        .await
        .expect("materialize");

    let replica_roles = repo::list_limit_roles(&replica_pool).await.expect("roles");
    assert!(
        replica_roles.iter().any(|r| r.id == "r-off" && !r.enabled),
        "the DISABLED limit role must survive the real materialize path: {replica_roles:?}"
    );
    let replica_bindings = repo::list_provider_key_bindings(&replica_pool)
        .await
        .expect("bindings");
    assert!(
        replica_bindings
            .iter()
            .any(|b| b.id == "b-off" && !b.enabled),
        "the DISABLED binding must survive the real materialize path: {replica_bindings:?}"
    );

    // Fidelity: the OFFLINE model survives the round-trip (it is absent from
    // the derived `models_by_key` but must not be dropped from the DB).
    let models = repo::list_provider_models(&replica_pool)
        .await
        .expect("models");
    assert!(
        models
            .iter()
            .any(|m| m.key == "gpt-4-offline" && m.status == 0),
        "offline model preserved in the replica"
    );

    // The config version persists in config_meta.
    assert_eq!(
        replica::replica_version(&replica_pool)
            .await
            .expect("version"),
        Some(leader_store.version()),
        "promoted replica continues the version sequence"
    );

    // §7-7: the replica can answer `has_access_token` (and serve token-cache
    // invalidation) without a 401 — the hash is rebuilt, not re-hashed from a
    // token the replica never sees.
    assert!(
        repo::tenant_has_access_token(&replica_pool, "t1")
            .await
            .expect("has access token"),
        "the replica must carry the tenant's access-token hash"
    );
    assert_eq!(
        repo::list_tenant_access_token_hashes(&replica_pool)
            .await
            .expect("hashes"),
        vec![("t1".to_string(), token_hash.to_string())],
        "byte-identical hash, not a re-derived one"
    );
}

/// T-CL-5 — two candidates, exactly one leader; the standby takes over after
/// the lease expires; the old leader demotes on its next tick.
#[tokio::test]
async fn election_two_nodes_one_leader() {
    let store: std::sync::Arc<dyn hydra_server::cluster::lease::LeaseStore> =
        std::sync::Arc::new(MemoryLeaseStore::new());
    let e1 = LeaderElection::new(store.clone(), "n1".into(), 600);
    let e2 = LeaderElection::new(store.clone(), "n2".into(), 600);

    // F-4: the freshness gate starts closed — both nodes sync from the
    // active leader first (in production the control client does this).
    e1.mark_sync_ok(true);
    e2.mark_sync_ok(true);

    e1.tick().await;
    assert!(e1.is_leader(), "n1 acquires first");
    e2.tick().await;
    assert!(!e2.is_leader(), "n2 stays standby while n1 holds the lease");

    // Lease expires (600 ms) → n2 acquires on its next tick.
    tokio::time::sleep(Duration::from_millis(700)).await;
    e2.tick().await;
    assert!(e2.is_leader(), "n2 takes over after expiry");

    // n1's next tick: renew fails (n2 holds) → immediate demotion.
    e1.tick().await;
    assert!(!e1.is_leader(), "n1 demotes after losing the lease");
}

/// T-CL-6 — `/healthz/leader` reflects the lease (200 active / 503 standby /
/// 404 non-candidate), and admin mutations are gated on leadership.
#[tokio::test]
async fn leader_health_and_write_gate() {
    // Standby node: leader_ready = Some(|| false).
    let key_provider: Arc<dyn KeyProvider> = Arc::new(kp());
    let store = ConfigStore::from_snapshot(ConfigData::default(), key_provider.clone());
    let auth = Arc::new(
        HttpAuthChecker::new(
            AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
            AuthConfig::default(),
        )
        .expect("HttpAuthChecker"),
    );
    let breaker = Arc::new(CircuitBreaker::new(BreakerConfig::new(2)));
    let state = Arc::new(AdminState::new(
        Some(common::setup_pool().await),
        store,
        auth,
        breaker,
        key_provider,
        Some(ADMIN_TOKEN.to_string()),
        AdmissionControl::new(),
        false,
        None,
        Some(Arc::new(|| false) as Arc<dyn Fn() -> bool + Send + Sync>),
    ));
    let port = start_admin(state);

    let client = reqwest::Client::new();
    let leader_url = format!("http://127.0.0.1:{port}/healthz/leader");
    let r = client.get(&leader_url).send().await.expect("send");
    assert_eq!(r.status().as_u16(), 503, "standby reports not-leader");

    // Admin mutation on a standby → 503 (fail-closed; P3 adds forwarding).
    let r = client
        .post(format!("http://127.0.0.1:{port}/api/v1/providers"))
        .header("authorization", format!("Bearer {ADMIN_TOKEN}"))
        .json(&serde_json::json!({
            "id":"p1","key":"openai","name":"O","endpoint":"https://api.openai.com",
            "weight":1,"created_at":"","updated_at":""
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(r.status().as_u16(), 503, "standby rejects writes");

    // Non-candidate (all/single-node): /healthz/leader → 404.
    let state_all = Arc::new(AdminState::new(
        Some(common::setup_pool().await),
        ConfigStore::from_snapshot(ConfigData::default(), Arc::new(kp())),
        Arc::new(
            HttpAuthChecker::new(
                AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
                AuthConfig::default(),
            )
            .expect("HttpAuthChecker"),
        ),
        Arc::new(CircuitBreaker::new(BreakerConfig::new(2))),
        Arc::new(kp()),
        Some(ADMIN_TOKEN.to_string()),
        AdmissionControl::new(),
        false,
        None,
        None, // single-node: no election
    ));
    let port_all = start_admin(state_all);
    let r = http(port_all, reqwest::Method::GET, "/healthz/leader", None).await;
    assert_eq!(
        r.status().as_u16(),
        404,
        "non-candidate has no leader health"
    );
}

/// A reqwest call with bind-retry (the Pingora server races the test thread).
async fn http(
    port: u16,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> reqwest::Response {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}{path}");
    for _ in 0..50 {
        let mut req = client.request(method.clone(), url.clone());
        req = req.header("authorization", format!("Bearer {ADMIN_TOKEN}"));
        if let Some(b) = &body {
            req = req.json(b);
        }
        match req.send().await {
            Ok(r) => return r,
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    panic!("server on {port} did not come up");
}

#[cfg(feature = "cluster-redis")]
async fn admin_components(
    pool: sqlx::SqlitePool,
    kp: Arc<dyn KeyProvider>,
) -> (ConfigStore, Arc<HttpAuthChecker>, Arc<CircuitBreaker>) {
    let store = ConfigStore::load(pool, kp.clone())
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
    (store, auth, breaker)
}

/// T9.2 — a forward that TIMES OUT must be reported as an UNKNOWN outcome.
///
/// The old mapping answered 502 `forward_failed` with "…(no local write)" for
/// every failure, including a timeout — i.e. it told the operator the write had
/// not happened when the request had in fact been sent and may well have been
/// applied. A blind retry could then double-apply it. Timeouts now get their own
/// code (`504 forward_result_unknown`) and the message tells the caller to
/// re-read the resource first; connection failures keep the definite 502.
#[cfg(feature = "cluster-redis")]
#[tokio::test]
async fn a_silent_leader_produces_504_forward_result_unknown() {
    use fred::prelude::*;
    use hydra_server::cluster::registry::NodeRegistry;
    use hydra_server::cluster::NodeRole;
    use hydra_server::redis::LEASE_KEY;

    // A black hole: accepts the TCP connection and never answers, which is what
    // a wedged leader looks like from here.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind black hole");
    let black_hole = listener.local_addr().expect("addr");
    let keep: Arc<std::sync::Mutex<Vec<std::net::TcpStream>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let keep2 = keep.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(s) = stream else { break };
            keep2.lock().expect("lock").push(s); // hold it open, say nothing
        }
    });

    let pool = common::real_redis_pool(49).await;
    let kp_arc: Arc<dyn KeyProvider> = Arc::new(kp());

    // The "leader" is the black hole, and it holds the lease: that is exactly
    // the forward target a standby resolves.
    let silent_registry = NodeRegistry::new(
        pool.clone(),
        "silent".into(),
        NodeRole::Leader,
        format!("http://{black_hole}"),
    );
    silent_registry.register(60, 120).await.expect("register");
    let _: Option<String> = pool
        .set(LEASE_KEY, "silent", None, None, false)
        .await
        .expect("set lease");

    let standby_pool = common::setup_pool().await;
    let (standby_store, auth2, breaker2) =
        admin_components(standby_pool.clone(), kp_arc.clone()).await;
    let mut standby_state = AdminState::new(
        Some(standby_pool.clone()),
        standby_store,
        auth2,
        breaker2,
        kp_arc,
        Some(ADMIN_TOKEN.to_string()),
        AdmissionControl::new(),
        false,
        None,
        Some(Arc::new(|| false) as Arc<dyn Fn() -> bool + Send + Sync>),
    );
    standby_state.cluster_registry = Some(Arc::new(NodeRegistry::new(
        pool.clone(),
        "standby".into(),
        NodeRole::Leader,
        format!("http://127.0.0.1:{}", ephemeral_port()),
    )));
    let standby_port = start_admin(Arc::new(standby_state));

    let body = serde_json::json!({
        "id":"p1","key":"openai","name":"O","endpoint":"https://api.openai.com",
        "weight":1,"created_at":"","updated_at":""
    });
    let r = http(
        standby_port,
        reqwest::Method::POST,
        "/api/v1/providers",
        Some(body),
    )
    .await;
    assert_eq!(
        r.status().as_u16(),
        504,
        "a timed-out forward is not a definite failure"
    );
    let text = r.text().await.expect("body");
    assert!(
        text.contains("forward_result_unknown"),
        "the dedicated code must be reported: {text}"
    );
    assert!(
        text.contains("may or may not have been applied"),
        "and the message must state the uncertainty: {text}"
    );
    assert!(
        !text.contains("no local write"),
        "the response must NOT claim the write did not happen: {text}"
    );
    // The standby itself still wrote nothing (its DB is empty).
    assert!(
        repo::list_providers(&standby_pool)
            .await
            .expect("list")
            .is_empty(),
        "a standby never writes locally"
    );
}

/// T9.2 — the complementary case: a leader that REFUSES the connection proves
/// nothing was sent, so it must stay a definite 502 (never "unknown"). This is
/// the classification that keeps a dead pod (IP present, process gone — the
/// normal Kubernetes case) from being mislabelled as possibly-applied.
#[cfg(feature = "cluster-redis")]
#[tokio::test]
async fn a_refused_leader_produces_502_forward_failed() {
    use fred::prelude::*;
    use hydra_server::cluster::registry::NodeRegistry;
    use hydra_server::cluster::NodeRole;
    use hydra_server::redis::LEASE_KEY;

    let pool = common::real_redis_pool(50).await;
    let kp_arc: Arc<dyn KeyProvider> = Arc::new(kp());

    // A port nobody listens on: bound, then released.
    let dead_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    };
    let dead_registry = NodeRegistry::new(
        pool.clone(),
        "dead".into(),
        NodeRole::Leader,
        format!("http://127.0.0.1:{dead_port}"),
    );
    dead_registry.register(60, 120).await.expect("register");
    let _: Option<String> = pool
        .set(LEASE_KEY, "dead", None, None, false)
        .await
        .expect("set lease");

    let standby_pool = common::setup_pool().await;
    let (standby_store, auth2, breaker2) =
        admin_components(standby_pool.clone(), kp_arc.clone()).await;
    let mut standby_state = AdminState::new(
        Some(standby_pool.clone()),
        standby_store,
        auth2,
        breaker2,
        kp_arc,
        Some(ADMIN_TOKEN.to_string()),
        AdmissionControl::new(),
        false,
        None,
        Some(Arc::new(|| false) as Arc<dyn Fn() -> bool + Send + Sync>),
    );
    standby_state.cluster_registry = Some(Arc::new(NodeRegistry::new(
        pool.clone(),
        "standby".into(),
        NodeRole::Leader,
        format!("http://127.0.0.1:{}", ephemeral_port()),
    )));
    let standby_port = start_admin(Arc::new(standby_state));

    let body = serde_json::json!({
        "id":"p1","key":"openai","name":"O","endpoint":"https://api.openai.com",
        "weight":1,"created_at":"","updated_at":""
    });
    let r = http(
        standby_port,
        reqwest::Method::POST,
        "/api/v1/providers",
        Some(body),
    )
    .await;
    assert_eq!(r.status().as_u16(), 502, "a refusal is definite");
    let text = r.text().await.expect("body");
    assert!(text.contains("forward_failed"), "got {text}");
    assert!(
        !text.contains("forward_result_unknown"),
        "a refused connection must not be labelled an unknown outcome: {text}"
    );
}

/// T-CL-7 — a standby FORWARDS admin mutations to the ACTUAL lease holder
/// (resolved live from the cluster registry — the target is never a static
/// `HYDRA_CONTROL_URL`, which for a primary candidate points at the node
/// itself), serves reads locally, and never writes locally. A dead active
/// yields 502 (fail-closed, no self-promotion via forwarding); a mutation
/// that already carries the forward-once marker is refused (loop guard).
#[cfg(feature = "cluster-redis")]
#[tokio::test]
async fn standby_forwards_mutations_to_active() {
    use fred::prelude::*;
    use hydra_server::cluster::registry::NodeRegistry;
    use hydra_server::cluster::NodeRole;
    use hydra_server::redis::LEASE_KEY;

    // A REAL Redis (dev-plan 铁律 2): holds the lease + the registry entries the
    // forward target is resolved from. Integration database 42.
    let pool = common::real_redis_pool(42).await;

    let active_pool = common::setup_pool().await;
    let kp_arc: Arc<dyn KeyProvider> = Arc::new(kp());
    let (active_store, auth, breaker) = admin_components(active_pool.clone(), kp_arc.clone()).await;
    let active = Arc::new(AdminState::new(
        Some(active_pool.clone()),
        active_store,
        auth,
        breaker,
        kp_arc.clone(),
        Some(ADMIN_TOKEN.to_string()),
        AdmissionControl::new(),
        false,
        None,
        Some(Arc::new(|| true) as Arc<dyn Fn() -> bool + Send + Sync>),
    ));
    let active_port = start_admin(active);

    // Register the ACTIVE in the registry and hand it the lease: the standby
    // must resolve THIS node as its forward target — the node id it polls
    // (`HYDRA_CONTROL_URL`) is irrelevant to forwarding.
    let active_registry = NodeRegistry::new(
        pool.clone(),
        "active".into(),
        NodeRole::Leader,
        format!("http://127.0.0.1:{active_port}"),
    );
    active_registry
        .register(60, 120)
        .await
        .expect("register active");
    let _: Option<String> = pool
        .set(LEASE_KEY, "active", None, None, false)
        .await
        .expect("set lease");

    let standby_pool = common::setup_pool().await;
    let (standby_store, auth2, breaker2) =
        admin_components(standby_pool.clone(), kp_arc.clone()).await;
    let mut standby_state = AdminState::new(
        Some(standby_pool.clone()),
        standby_store,
        auth2,
        breaker2,
        kp_arc,
        Some(ADMIN_TOKEN.to_string()),
        AdmissionControl::new(),
        false,
        None,
        Some(Arc::new(|| false) as Arc<dyn Fn() -> bool + Send + Sync>),
    );
    standby_state.cluster_registry = Some(Arc::new(NodeRegistry::new(
        pool.clone(),
        "standby".into(),
        NodeRole::Leader,
        format!("http://127.0.0.1:{}", ephemeral_port()), // own URL; never the target here
    )));
    let standby = Arc::new(standby_state);
    let standby_port = start_admin(standby);

    // POST a provider to the STANDBY → forwarded to the active → 201, and the
    // provider lands on the ACTIVE's DB only.
    let body = serde_json::json!({
        "id":"p1","key":"openai","name":"O","endpoint":"https://api.openai.com",
        "weight":1,"created_at":"","updated_at":""
    });
    let r = http(
        standby_port,
        reqwest::Method::POST,
        "/api/v1/providers",
        Some(body),
    )
    .await;
    assert_eq!(r.status().as_u16(), 201, "mutation forwarded to the active");
    assert!(
        repo::list_providers(&active_pool)
            .await
            .unwrap()
            .iter()
            .any(|p| p.id == "p1"),
        "provider created on the ACTIVE's DB"
    );
    assert!(
        repo::list_providers(&standby_pool)
            .await
            .unwrap()
            .is_empty(),
        "the standby never writes locally"
    );

    // Reads on the standby are served locally (its own (empty) replica).
    let r = http(
        standby_port,
        reqwest::Method::GET,
        "/api/v1/providers",
        None,
    )
    .await;
    assert_eq!(r.status().as_u16(), 200, "read served locally");
    let list: serde_json::Value = r.json().await.expect("json");
    assert_eq!(
        list.as_array().map(|a| a.len()).unwrap_or(0),
        0,
        "standby reads its replica"
    );

    // Auth-cache invalidate is a DELETE → forwarded too.
    let r = http(
        standby_port,
        reqwest::Method::DELETE,
        "/api/v1/auth/cache",
        Some(serde_json::json!({"tenant_id": "nope"})),
    )
    .await;
    assert_eq!(r.status().as_u16(), 200, "DELETE forwarded to the active");

    // Forward-once loop guard: a mutation that already carries the marker is
    // refused with 503 even though we are a standby — it must never be
    // forwarded a second time (self-/mutual-forward loop termination).
    let r = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{standby_port}/api/v1/providers"))
        .header("authorization", format!("Bearer {ADMIN_TOKEN}"))
        .header("x-hydra-forwarded", "1")
        .json(&serde_json::json!({"id":"loop","key":"l","name":"L","endpoint":"https://l.example.com","weight":1,"created_at":"","updated_at":""}))
        .send()
        .await
        .expect("send");
    assert_eq!(
        r.status().as_u16(),
        503,
        "forward-once guard refuses re-forwarding"
    );

    // Dead active → 502 (fail-closed), never a local write.
    let dead_port = ephemeral_port(); // nothing listening there
    let dead_pool = common::setup_pool().await;
    let (dead_store, auth3, breaker3) = admin_components(dead_pool.clone(), Arc::new(kp())).await;
    let mut dead_state = AdminState::new(
        Some(dead_pool.clone()),
        dead_store,
        auth3,
        breaker3,
        Arc::new(kp()),
        Some(ADMIN_TOKEN.to_string()),
        AdmissionControl::new(),
        false,
        None,
        Some(Arc::new(|| false) as Arc<dyn Fn() -> bool + Send + Sync>),
    );
    dead_state.cluster_registry = Some(Arc::new(NodeRegistry::new(
        pool.clone(),
        "dead-standby".into(),
        NodeRole::Leader,
        String::new(),
    )));
    // A "dead active" registered in the registry with an unreachable URL.
    let dead_active = NodeRegistry::new(
        pool.clone(),
        "dead-active".into(),
        NodeRole::Leader,
        format!("http://127.0.0.1:{dead_port}"),
    );
    dead_active
        .register(60, 120)
        .await
        .expect("register dead active");
    let _: Option<String> = pool
        .set(LEASE_KEY, "dead-active", None, None, false)
        .await
        .expect("set dead lease");
    let dead = Arc::new(dead_state);
    let dead_standby_port = start_admin(dead);
    let r = http(
        dead_standby_port,
        reqwest::Method::POST,
        "/api/v1/providers",
        Some(serde_json::json!({"id":"p2","key":"x","name":"X","endpoint":"https://x.example.com","weight":1,"created_at":"","updated_at":""})),
    )
    .await;
    assert_eq!(
        r.status().as_u16(),
        502,
        "unreachable active → 502, no local write"
    );
    assert!(
        repo::list_providers(&dead_pool).await.unwrap().is_empty(),
        "standby never self-promotes via the forwarding path"
    );
}

// ===========================================================================
// REVIEW B1 — the election freshness gate must be driven by EVIDENCE (this
// node's replica DB), not by the poll signal.
//
// `UpToDate` means "the control plane had nothing newer to send" — the client
// asks with `since = store.version()` and advances that watermark BEFORE the
// replica rebuild is dispatched (`control_client`: `apply_snapshot` then the
// hook). So a node whose materialization FAILED (wrong master key, disk full,
// `restore_config` error) or is still running looks "up to date" on the very
// next poll. Opening the gate there made that node eligible to lead with a
// stale/empty replica, and a promoted node whose replica is stale then rebuilds
// the cluster's config FROM that replica (`ConfigStore::reload_all` reads the
// local DB) and republishes it at a higher version.
//
// These tests drive the REAL hook (`replica::gate_hook`), so the wiring itself
// is what is under test — not a copy of it.
// ===========================================================================

/// Every gate decision the hook made, in order.
type GateCalls = Arc<std::sync::Mutex<Vec<bool>>>;
/// The closure the gate driver calls with each decision.
type GateFn = Arc<dyn Fn(bool) + Send + Sync>;

/// Collect every gate decision the hook makes, in order.
fn gate_recorder() -> (GateFn, GateCalls) {
    let calls: GateCalls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = calls.clone();
    let gate = Arc::new(move |ok: bool| {
        sink.lock().expect("gate mutex").push(ok);
    }) as GateFn;
    (gate, calls)
}

/// Wait until the (spawned) hook has recorded `n` decisions.
async fn gate_calls(calls: &GateCalls, n: usize) -> Vec<bool> {
    for _ in 0..200 {
        {
            let seen = calls.lock().expect("gate mutex");
            if seen.len() >= n {
                return seen.clone();
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    calls.lock().expect("gate mutex").clone()
}

/// (a) + (c): a FAILED materialization must keep the gate closed even when the
/// next poll says `UpToDate`; a SUCCESSFUL one opens it.
#[tokio::test]
async fn freshness_gate_needs_a_materialized_replica() {
    let leader_pool = common::setup_pool().await;
    let sealing_kp: Arc<dyn KeyProvider> = Arc::new(kp());
    let leader_store = ConfigStore::load(leader_pool.clone(), sealing_kp.clone())
        .await
        .expect("ConfigStore::load");
    seed_and_reload(&leader_pool, &leader_store).await;
    let version = leader_store.version();
    assert!(version >= 1, "the leader must have a real config version");

    let content = leader_store
        .replication()
        .as_deref()
        .cloned()
        .expect("the leader has replication content");
    let wire = SnapshotWire::build(&content, sealing_kp.as_ref())
        .await
        .expect("build wire");

    // --- (a) the standby CANNOT open the sealed snapshot (wrong master key):
    //         materialization fails, so the replica stays empty.
    //
    //         The standby's STORE is at the leader's version: the control client
    //         applies the snapshot to memory (`apply_snapshot`) BEFORE the
    //         replica rebuild is dispatched, which is exactly why the next poll
    //         answers `UpToDate` and why that signal must not open the gate.
    let wrong_kp: Arc<dyn KeyProvider> = Arc::new(StaticKeyProvider::new([9u8; 32], 1));
    let replica_pool = common::setup_pool().await;
    let standby_store = ConfigStore::from_snapshot(ConfigData::default(), wrong_kp.clone());
    // The store adopts the leader's version WITHOUT the content being usable
    // (the sealed snapshot cannot be opened with this key) — exactly the state
    // that must not open the gate.
    standby_store.apply_snapshot(common::hydrated(version, ConfigData::default()));
    let (gate, calls) = gate_recorder();
    let hook = replica::gate_hook(
        Arc::new(replica::MaterializationGuard::new()),
        replica_pool.clone(),
        standby_store.clone(),
        wrong_kp.clone(),
        gate,
    );

    hook(&hydra_server::cluster::control_client::PollOutcome::Applied(Box::new(wire.clone())));
    assert_eq!(
        gate_calls(&calls, 1).await,
        vec![false],
        "a failed materialization must close the gate"
    );
    assert_eq!(
        replica::replica_version(&replica_pool)
            .await
            .expect("version"),
        None,
        "the replica really is empty in this arm"
    );

    // The next poll has nothing newer to apply (the store watermark already
    // moved) — that must NOT be read as "synced".
    hook(&hydra_server::cluster::control_client::PollOutcome::UpToDate);
    let seen = gate_calls(&calls, 2).await;
    assert_eq!(
        seen,
        vec![false, false],
        "an UpToDate poll must not open the gate while the replica is empty"
    );

    // --- (c) with the RIGHT master key the materialization succeeds and the
    //         gate opens; the following UpToDate poll keeps it open.
    let replica_pool2 = common::setup_pool().await;
    let standby_store2 = ConfigStore::from_snapshot(ConfigData::default(), sealing_kp.clone());
    standby_store2.apply_snapshot(
        wire.clone()
            .hydrate(sealing_kp.as_ref())
            .expect("hydrate with the right key"),
    );
    let (gate2, calls2) = gate_recorder();
    let hook2 = replica::gate_hook(
        Arc::new(replica::MaterializationGuard::new()),
        replica_pool2.clone(),
        standby_store2,
        sealing_kp.clone(),
        gate2,
    );
    hook2(&hydra_server::cluster::control_client::PollOutcome::Applied(Box::new(wire)));
    assert_eq!(
        gate_calls(&calls2, 1).await,
        vec![true],
        "a successful materialization opens the gate"
    );
    assert_eq!(
        replica::replica_version(&replica_pool2)
            .await
            .expect("version"),
        Some(version),
        "the replica now holds the store's version"
    );
    hook2(&hydra_server::cluster::control_client::PollOutcome::UpToDate);
    assert_eq!(
        gate_calls(&calls2, 2).await,
        vec![true, true],
        "with the replica current, UpToDate keeps the gate open"
    );
}

/// (d) A TRANSIENT materialization failure must heal itself: the client never
/// re-delivers a version its memory watermark already passed, so the gate would
/// otherwise stay closed until a newer config write or a restart.
#[tokio::test]
async fn a_transient_materialization_failure_heals_on_the_next_poll() {
    let leader_pool = common::setup_pool().await;
    let kp: Arc<dyn KeyProvider> = Arc::new(kp());
    let leader_store = ConfigStore::load(leader_pool.clone(), kp.clone())
        .await
        .expect("ConfigStore::load");
    seed_and_reload(&leader_pool, &leader_store).await;
    let version = leader_store.version();

    let content = leader_store
        .replication()
        .as_deref()
        .cloned()
        .expect("the leader has replication content");
    let wire = SnapshotWire::build(&content, kp.as_ref())
        .await
        .expect("build wire");

    // The replica's DB cannot commit the version marker yet (a REAL SQLite
    // trigger, no mock): the rebuild fails, so the replica stays empty.
    let replica_pool = common::setup_pool().await;
    sqlx::query(
        "CREATE TRIGGER block_marker BEFORE INSERT ON config_meta \
         BEGIN SELECT RAISE(ABORT, 'oracle: marker write blocked'); END",
    )
    .execute(&replica_pool)
    .await
    .expect("install trigger");

    let standby_store = ConfigStore::from_snapshot(ConfigData::default(), kp.clone());
    standby_store.apply_snapshot(wire.clone().hydrate(kp.as_ref()).expect("hydrate the wire"));
    let (gate, calls) = gate_recorder();
    let hook = replica::gate_hook(
        Arc::new(replica::MaterializationGuard::new()),
        replica_pool.clone(),
        standby_store,
        kp.clone(),
        gate,
    );

    hook(&hydra_server::cluster::control_client::PollOutcome::Applied(Box::new(wire)));
    assert_eq!(
        gate_calls(&calls, 1).await,
        vec![false],
        "the blocked marker must keep the gate closed"
    );

    // The fault clears; the NEXT poll (UpToDate — there is nothing newer to
    // send) must retry the last snapshot and open the gate.
    sqlx::query("DROP TRIGGER block_marker")
        .execute(&replica_pool)
        .await
        .expect("drop trigger");
    hook(&hydra_server::cluster::control_client::PollOutcome::UpToDate);
    let seen = gate_calls(&calls, 2).await;
    assert_eq!(
        seen,
        vec![false, true],
        "a transient fault must heal without a new config version or a restart"
    );
    assert_eq!(
        replica::replica_version(&replica_pool)
            .await
            .expect("version"),
        Some(version),
        "the replica caught up"
    );
}

/// (b) A node whose STORE holds config but whose replica DB holds nothing must
/// not be considered synced (the version watermark alone cannot tell a fresh
/// node from a node that has content).
#[tokio::test]
async fn an_empty_replica_is_not_synced_with_a_non_empty_store() {
    let leader_pool = common::setup_pool().await;
    let sealing_kp: Arc<dyn KeyProvider> = Arc::new(kp());
    let leader_store = ConfigStore::load(leader_pool.clone(), sealing_kp.clone())
        .await
        .expect("ConfigStore::load");
    seed_and_reload(&leader_pool, &leader_store).await;

    // A standby that has never materialized anything.
    let replica_pool = common::setup_pool().await;
    assert_eq!(
        replica::replica_version(&replica_pool)
            .await
            .expect("version"),
        None,
        "fresh replica: no version marker"
    );

    let (gate, calls) = gate_recorder();
    let hook = replica::gate_hook(
        Arc::new(replica::MaterializationGuard::new()),
        replica_pool.clone(),
        leader_store.clone(),
        sealing_kp,
        gate,
    );
    hook(&hydra_server::cluster::control_client::PollOutcome::UpToDate);
    assert_eq!(
        gate_calls(&calls, 1).await,
        vec![false],
        "an empty replica must not be 'up to date' with a store at version {}",
        leader_store.version()
    );

    // Control: a store with NO config at all (a genuinely cold cluster) accepts
    // an empty replica — otherwise the first node could never take the lease.
    let cold_pool = common::setup_pool().await;
    let cold_kp: Arc<dyn KeyProvider> = Arc::new(kp());
    let cold_store = ConfigStore::load(cold_pool.clone(), cold_kp.clone())
        .await
        .expect("ConfigStore::load");
    assert_eq!(
        cold_store.version(),
        0,
        "a DB with no config and no marker is version 0, not 1"
    );
    assert!(
        replica::replica_is_current(&cold_pool, cold_store.version()).await,
        "nothing to sync ⇒ a cold cluster may still elect its first leader"
    );
}
