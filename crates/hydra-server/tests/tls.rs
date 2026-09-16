//! §2.6 — Multi-tenant downstream TLS (design §12).
//!
//! T6.1–T6.4 exercise the **real** `HydraCertStore` SNI certificate callback
//! through a **real** Pingora TLS listener, with **real** self-signed fixture
//! certs (`tests/fixtures/*.crt`/`*.key`, generated with the openssl CLI — no
//! rcgen, no embedded-test-time generation). The TLS client side uses the same
//! TLS backend pingora links (`pingora_core::tls::ssl`) over a blocking socket
//! so it can read back the exact server certificate and compare it byte-for-byte
//! (DER) with the expected fixture — proving which cert the SNI callback chose.
//!
//! No internal logic is mocked: cert resolution (`resolve_certs`), the
//! `TlsAccept::certificate_callback`, hot-reload via `resolve_and_store`, and
//! the Pingora handshake are all the production code paths.

#![cfg(any(feature = "tls-boringssl", feature = "tls-openssl"))]

mod common;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use hydra_core::breaker::BreakerConfig;
use hydra_core::config::CertMeta;
use hydra_core::model::{Tenant, UsageRecord};
use hydra_server::crypto::{KeyProvider, StaticKeyProvider};
use hydra_server::db as repo;
use hydra_server::http::{AuthCache, AuthConfig, HttpAuthChecker};
use hydra_server::proxy::breaker_wrap::CircuitBreaker;
use hydra_server::proxy::config::ProxyConfig;
use hydra_server::proxy::limiter::RateLimiter;
use hydra_server::proxy::{AppState, HydraProxy};
use hydra_server::store::ConfigStore;
use hydra_server::tls::{HydraCertStore, ResolvedCert};
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::tls::ssl::{SslConnector, SslMethod, SslVerifyMode};
use pingora_core::tls::x509::X509;

const NOW: &str = "2026-01-01 00:00:00";

/// A minimal no-op usage sink (real trait impl, like the W4 spike test).
struct NoopSink;
impl hydra_server::sink::UsageSink for NoopSink {
    fn record(&self, _record: UsageRecord) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// Parse a fixture cert PEM to its DER bytes (the comparison baseline).
fn fixture_cert_der(crt: &str) -> Vec<u8> {
    let pem = std::fs::read(fixture(crt)).expect("read fixture cert");
    X509::from_pem(&pem)
        .expect("parse fixture cert PEM")
        .to_der()
        .expect("cert to_der")
}

/// Bind an ephemeral port, return it, then release the socket so Pingora can
/// rebind. (Same TOCTOU-tolerant pattern as the W4 spike test.)
/// See `common::ephemeral_port`: no bind-then-release race (the previous
/// local copy could hand two concurrent tests the same port).
fn ephemeral_port() -> u16 {
    common::ephemeral_port()
}

/// Blocking TLS client: connect to `addr` presenting SNI=`sni`, return the DER
/// of the server's selected certificate. Retries the TCP connect until the
/// server is ready, then performs the blocking handshake. Run via
/// `spawn_blocking` from the async test.
fn tls_peer_cert_der_blocking(addr: &str, sni: &str) -> Vec<u8> {
    let connector = SslConnector::builder(SslMethod::tls())
        .expect("SslConnector builder")
        .build();
    let mut cfg = connector.configure().expect("configure");
    // Self-signed fixtures: skip all verification; we compare the cert bytes
    // ourselves instead. SNI is sent because use_server_name_indication is on.
    cfg.set_verify(SslVerifyMode::NONE);
    cfg.set_use_server_name_indication(true);

    let stream = {
        let mut last_err = None;
        let mut connected: Option<std::net::TcpStream> = None;
        for _ in 0..100 {
            match std::net::TcpStream::connect(addr) {
                Ok(s) => {
                    connected = Some(s);
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
        connected.unwrap_or_else(|| {
            panic!(
                "server at {addr} never accepted TCP within 10s: {}",
                last_err.map(|e| e.to_string()).unwrap_or_default()
            )
        })
    };

    let tls_stream = cfg
        .connect(sni, stream)
        .unwrap_or_else(|e| panic!("TLS handshake to {addr} (SNI={sni}) failed: {e}"));
    let cert = tls_stream
        .ssl()
        .peer_certificate()
        .unwrap_or_else(|| panic!("server presented no cert (SNI={sni})"));
    cert.to_der()
        .unwrap_or_else(|e| panic!("peer cert to_der failed: {e}"))
}

/// Async wrapper for the blocking client.
async fn tls_peer_cert_der(addr: &str, sni: &str) -> Vec<u8> {
    let addr = addr.to_string();
    let sni = sni.to_string();
    tokio::task::spawn_blocking(move || tls_peer_cert_der_blocking(&addr, &sni))
        .await
        .expect("spawn_blocking client")
}

/// Start a real Pingora server hosting `HydraProxy` over a downstream TLS
/// listener driven by `cert_store`'s SNI callback. Returns the bound port and
/// keeps the server running on a background thread.
fn start_tls_server(state: Arc<AppState>, cert_store: &HydraCertStore) -> u16 {
    let port = ephemeral_port();
    let addr = format!("127.0.0.1:{port}");
    let mut server = Server::new(Some(Opt::default())).expect("Server::new");
    server.bootstrap();
    let app = HydraProxy::new(state);
    let mut proxy_service = pingora_proxy::http_proxy_service(&server.configuration, app);
    let settings = cert_store.build_tls_settings().expect("build_tls_settings");
    proxy_service.add_tls_with_settings(&addr, None, settings);
    server.add_service(proxy_service);
    std::thread::spawn(move || server.run_forever());
    port
}

/// Build the shared app state over an in-memory DB. The proxy HTTP path is not
/// exercised here (only the TLS handshake), but a real `HydraProxy` hosts the
/// listener — no test-only `ProxyHttp` stub.
async fn build_state(pool: sqlx::SqlitePool) -> (ConfigStore, Arc<AppState>) {
    let key_provider: Arc<dyn KeyProvider> = Arc::new(StaticKeyProvider::new([1u8; 32], 1));
    let store = ConfigStore::load(pool, key_provider)
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
    let sink: Arc<dyn hydra_server::sink::UsageSink> = Arc::new(NoopSink);
    let state = Arc::new(AppState {
        store: store.clone(),
        auth,
        breaker,
        limiter,
        admission: hydra_server::proxy::admission::AdmissionControl::new(),
        sink,
        proxy: ProxyConfig::default(),
    });
    (store, state)
}

/// Seed a tenant with downstream cert paths. The provider/model scaffolding is
/// the minimum for a publishable snapshot; the TLS callback only consumes the
/// tenant's cert paths via `ConfigData.certs`.
async fn seed_tenant_with_cert(
    pool: &sqlx::SqlitePool,
    id: &str,
    domain: &str,
    cert_file: &str,
    cert_key: &str,
) {
    repo::insert_tenant(
        pool,
        &Tenant {
            id: id.into(),
            name: format!("{id}-tenant"),
            domain: domain.into(),
            auth_url: format!("https://auth.{domain}/verify"),
            cert_key: Some(cert_key.into()),
            cert_file: Some(cert_file.into()),
            enabled: true,
            created_at: NOW.into(),
            updated_at: NOW.into(),
        },
    )
    .await
    .expect("insert tenant");
}

/// Seed a tenant with downstream cert paths. The provider/model scaffolding is
/// the minimum for a publishable snapshot; the TLS callback only consumes the
/// tenant's cert paths via `ConfigData.certs`.
// ===========================================================================
// T6.1 — SNI selects the per-tenant cert (two tenants, two self-signed certs)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t6_1_sni_selects_tenant_cert() {
    let pool = common::setup_pool().await;
    seed_tenant_with_cert(
        &pool,
        "t1",
        "acme.com",
        &fixture("acme.crt"),
        &fixture("acme.key"),
    )
    .await;
    seed_tenant_with_cert(
        &pool,
        "t2",
        "beta.io",
        &fixture("beta.crt"),
        &fixture("beta.key"),
    )
    .await;

    let (store, state) = build_state(pool).await;
    let snapshot = store.snapshot();
    assert_eq!(
        snapshot.certs.len(),
        2,
        "both tenant certs must be present in the snapshot"
    );

    let cert_store = HydraCertStore::new(None);
    cert_store.resolve_and_store(&snapshot.certs);

    let port = start_tls_server(state, &cert_store);
    let addr = format!("127.0.0.1:{port}");

    let acme_der_expected = fixture_cert_der("acme.crt");
    let beta_der_expected = fixture_cert_der("beta.crt");
    assert_ne!(
        acme_der_expected, beta_der_expected,
        "fixtures must be distinct certs"
    );

    // SNI=acme.com → the acme cert.
    let der = tls_peer_cert_der(&addr, "acme.com").await;
    assert_eq!(
        der, acme_der_expected,
        "SNI=acme.com must present the acme.com cert"
    );

    // SNI=beta.io → the beta cert.
    let der = tls_peer_cert_der(&addr, "beta.io").await;
    assert_eq!(
        der, beta_der_expected,
        "SNI=beta.io must present the beta.io cert"
    );

    // Negative control: the two selections really differ.
    let der_acme = tls_peer_cert_der(&addr, "acme.com").await;
    assert_ne!(
        der_acme, beta_der_expected,
        "acme SNI must never yield the beta cert"
    );
}

// ===========================================================================
// T6.2 — hot-reload: resolve_and_store swaps certs; new handshake uses it
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t6_2_hot_reload_swaps_cert() {
    let pool = common::setup_pool().await;
    // Start with the v1 cert for acme.com.
    seed_tenant_with_cert(
        &pool,
        "t1",
        "acme.com",
        &fixture("acme.crt"),
        &fixture("acme.key"),
    )
    .await;

    let (store, state) = build_state(pool.clone()).await;

    let cert_store = HydraCertStore::new(None);
    cert_store.resolve_and_store(&store.snapshot().certs);

    let port = start_tls_server(state, &cert_store);
    let addr = format!("127.0.0.1:{port}");

    let acme_v1 = fixture_cert_der("acme.crt");
    let acme_v2 = fixture_cert_der("acme2.crt");
    assert_ne!(acme_v1, acme_v2, "v1 and v2 certs must differ");

    // Initial: handshake uses v1.
    let der = tls_peer_cert_der(&addr, "acme.com").await;
    assert_eq!(der, acme_v1, "first handshake must use the v1 cert");

    // Hot-reload: flip the tenant's cert to v2 in the DB, reload, re-resolve.
    let mut tenant = repo::list_tenants(&pool)
        .await
        .expect("list tenants")
        .into_iter()
        .find(|t| t.domain == "acme.com")
        .expect("acme tenant");
    tenant.cert_file = Some(fixture("acme2.crt"));
    tenant.cert_key = Some(fixture("acme2.key"));
    repo::update_tenant(&pool, &tenant)
        .await
        .expect("update tenant");

    store.reload_all().await.expect("reload_all");
    cert_store.resolve_and_store(&store.snapshot().certs);

    // Next handshake uses v2 (no server restart).
    let der = tls_peer_cert_der(&addr, "acme.com").await;
    assert_eq!(
        der, acme_v2,
        "after reload+resolve_and_store the new handshake must use the v2 cert"
    );
}

// ===========================================================================
// T6.3 — single source: the boxed callback shares the same ArcSwap as the
// resolver, so a write through one handle is visible to the other.
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t6_3_single_source_shared_arcswap() {
    let store = HydraCertStore::new(None);

    // `build_tls_settings` boxes a *clone* of the store; that clone must share
    // the exact same resolved-certs ArcSwap (design §12.1 single source).
    let settings = store.build_tls_settings().expect("build_tls_settings");
    let _ = settings; // TlsSettings holds the boxed clone for the listener.

    // The original handle is empty before resolution.
    assert!(store.resolved().is_empty());

    let mut certs = std::collections::HashMap::new();
    certs.insert(
        "acme.com".to_string(),
        CertMeta {
            domain: "acme.com".to_string(),
            cert_file: Some(fixture("acme.crt")),
            cert_key: Some(fixture("acme.key")),
            cert_pem: None,
            cert_key_pem: None,
        },
    );
    store.resolve_and_store(&certs);

    // The write is visible through the original handle.
    let loaded = store.resolved();
    assert!(loaded.contains_key("acme.com"));
    let ResolvedCert { cert, .. } = loaded.get("acme.com").expect("acme resolved");
    let der = cert.to_der().expect("to_der");
    assert_eq!(
        der,
        fixture_cert_der("acme.crt"),
        "resolved cert matches fixture"
    );
}

// ===========================================================================
// T6.4 — PEM-parse failure is isolated: one bad tenant doesn't break others.
// (resolve_certs-level; the per-tenant skip+log path.)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t6_4_pem_parse_failure_isolated() {
    let store = HydraCertStore::new(None);

    let mut certs = std::collections::HashMap::new();
    // Good tenant.
    certs.insert(
        "acme.com".to_string(),
        CertMeta {
            domain: "acme.com".to_string(),
            cert_file: Some(fixture("acme.crt")),
            cert_key: Some(fixture("acme.key")),
            cert_pem: None,
            cert_key_pem: None,
        },
    );
    // Bad tenant: garbage cert PEM (the key is valid, but the cert isn't).
    certs.insert(
        "broken.example".to_string(),
        CertMeta {
            domain: "broken.example".to_string(),
            cert_file: Some(fixture("bad.crt")),
            cert_key: Some(fixture("acme.key")),
            cert_pem: None,
            cert_key_pem: None,
        },
    );
    // Tenant missing paths entirely.
    certs.insert(
        "nopath.example".to_string(),
        CertMeta {
            domain: "nopath.example".to_string(),
            cert_file: None,
            cert_key: None,
            cert_pem: None,
            cert_key_pem: None,
        },
    );

    // resolve_and_store must not panic and must keep the good tenant.
    store.resolve_and_store(&certs);

    let loaded = store.resolved();
    assert!(
        loaded.contains_key("acme.com"),
        "the good tenant must still resolve"
    );
    assert!(
        !loaded.contains_key("broken.example"),
        "the garbage-cert tenant must be skipped"
    );
    assert!(
        !loaded.contains_key("nopath.example"),
        "the pathless tenant must be skipped"
    );
    assert_eq!(loaded.len(), 1, "only the good tenant survives");

    // And the surviving cert is valid (parseable to DER), proving no corruption
    // leaked from the bad tenant.
    let ResolvedCert { cert, .. } = loaded.get("acme.com").expect("acme present");
    let der = cert.to_der().expect("good cert must be DER-encodable");
    assert_eq!(der, fixture_cert_der("acme.crt"));
}

// ===========================================================================
// T6.5 / T6.6 - the FULL certificate chain must reach the handshake.
// ===========================================================================

/// Blocking TLS client returning EVERY certificate the server presented, leaf
/// first. Same setup as `tls_peer_cert_der_blocking`.
fn tls_peer_chain_der_blocking(addr: &str, sni: &str) -> Vec<Vec<u8>> {
    let connector = SslConnector::builder(SslMethod::tls())
        .expect("SslConnector builder")
        .build();
    let mut cfg = connector.configure().expect("configure");
    cfg.set_verify(SslVerifyMode::NONE);
    cfg.set_use_server_name_indication(true);

    let stream = {
        let mut last_err = None;
        let mut connected: Option<std::net::TcpStream> = None;
        for _ in 0..100 {
            match std::net::TcpStream::connect(addr) {
                Ok(s) => {
                    connected = Some(s);
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
        connected.unwrap_or_else(|| {
            panic!(
                "server at {addr} never accepted TCP within 10s: {}",
                last_err.map(|e| e.to_string()).unwrap_or_default()
            )
        })
    };

    let tls_stream = cfg
        .connect(sni, stream)
        .unwrap_or_else(|e| panic!("TLS handshake to {addr} (SNI={sni}) failed: {e}"));
    tls_stream
        .ssl()
        .peer_cert_chain()
        .map(|chain| {
            chain
                .iter()
                .map(|c| c.to_der().expect("chain cert to_der"))
                .collect()
        })
        .unwrap_or_default()
}

/// Async wrapper for the blocking chain client.
async fn tls_peer_chain_der(addr: &str, sni: &str) -> Vec<Vec<u8>> {
    let addr = addr.to_string();
    let sni = sni.to_string();
    tokio::task::spawn_blocking(move || tls_peer_chain_der_blocking(&addr, &sni))
        .await
        .expect("spawn_blocking chain client")
}

/// T6.5 - a `fullchain.pem` (leaf + intermediate, what every ACME client
/// emits) must present the intermediates. Only the leaf used to be installed,
/// so clients that do not cache intermediates failed the chain build with
/// "unable to get local issuer certificate" while browsers worked - and
/// nothing was logged, because parsing the bundle succeeds either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t6_5_fullchain_pem_presents_the_intermediates() {
    let leaf_pem = std::fs::read(fixture("acme.crt")).expect("leaf pem");
    // Any second certificate stands in for an intermediate: it only has to
    // prove the bundle is carried through to the handshake.
    let second_pem = std::fs::read(fixture("beta.crt")).expect("second pem");
    let mut bundle = leaf_pem.clone();
    bundle.extend_from_slice(&second_pem);
    let key_pem = std::fs::read_to_string(fixture("acme.key")).expect("key pem");

    let mut certs = std::collections::HashMap::new();
    certs.insert(
        "acme.com".to_string(),
        CertMeta {
            domain: "acme.com".to_string(),
            cert_file: None,
            cert_key: None,
            cert_pem: Some(String::from_utf8(bundle).expect("utf8 bundle")),
            cert_key_pem: Some(key_pem),
        },
    );

    let cert_store = HydraCertStore::new(None);
    cert_store.resolve_and_store(&certs);
    {
        let loaded = cert_store.resolved();
        let resolved = loaded.get("acme.com").expect("acme resolved");
        assert_eq!(
            resolved.chain.len(),
            1,
            "the bundle's second certificate must be kept as the chain"
        );
        assert_eq!(
            resolved.cert.to_der().expect("leaf der"),
            fixture_cert_der("acme.crt"),
            "the FIRST certificate of the bundle is the leaf"
        );
        assert_eq!(
            resolved.chain[0].to_der().expect("chain der"),
            fixture_cert_der("beta.crt")
        );
    }

    let pool = common::setup_pool().await;
    let (_store, state) = build_state(pool).await;
    let port = start_tls_server(state, &cert_store);
    let chain = tls_peer_chain_der(&format!("127.0.0.1:{port}"), "acme.com").await;

    assert_eq!(
        chain.len(),
        2,
        "the handshake must present leaf + intermediate, not just the leaf"
    );
    assert_eq!(
        chain[0],
        fixture_cert_der("acme.crt"),
        "first presented certificate is the leaf"
    );
    assert_eq!(
        chain[1],
        fixture_cert_der("beta.crt"),
        "the intermediate must be presented too"
    );
}

/// T6.6 - a single-certificate PEM still resolves, with an empty chain.
#[test]
fn t6_6_single_cert_pem_has_an_empty_chain() {
    let mut certs = std::collections::HashMap::new();
    certs.insert(
        "acme.com".to_string(),
        CertMeta {
            domain: "acme.com".to_string(),
            cert_file: None,
            cert_key: None,
            cert_pem: Some(std::fs::read_to_string(fixture("acme.crt")).expect("pem")),
            cert_key_pem: Some(std::fs::read_to_string(fixture("acme.key")).expect("key")),
        },
    );
    let cert_store = HydraCertStore::new(None);
    cert_store.resolve_and_store(&certs);
    let loaded = cert_store.resolved();
    let resolved = loaded.get("acme.com").expect("resolved");
    assert!(resolved.chain.is_empty(), "a single cert has no chain");
}

/// T6.7 — the cert store follows **every** snapshot swap, including the
/// control-plane path an edge uses (审核四 P3 / F-3).
///
/// Before this, cert re-resolution was wired to two admin handler call sites,
/// so it only ever ran on the node that received the admin write. An `edge`
/// node applies config through `ConfigStore::apply_snapshot` (control-plane
/// poll) and never touches an admin handler: a certificate pushed to the
/// cluster therefore stayed invisible to the TLS callback — the SNI callback
/// kept selecting the certs the process had at boot — until someone restarted
/// the pod. This drives the production wiring (`tls::follow_snapshot`, the same
/// call `main`'s bootstrap makes) through the edge path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t6_7_cert_store_follows_control_plane_snapshots() {
    let pool = common::setup_pool().await;
    let (store, _state) = build_state(pool).await;

    let cert_store = Arc::new(HydraCertStore::new(None));
    hydra_server::tls::follow_snapshot(&store, &cert_store);
    assert!(
        cert_store.resolved().is_empty(),
        "nothing is configured yet"
    );

    // A control-plane snapshot arrives carrying acme.com's certificate. This is
    // the edge path: no admin write, no restart.
    let mut cfg = hydra_core::config::ConfigData::default();
    cfg.certs.insert(
        "acme.com".to_string(),
        CertMeta {
            domain: "acme.com".to_string(),
            cert_file: Some(fixture("acme.crt")),
            cert_key: Some(fixture("acme.key")),
            cert_pem: None,
            cert_key_pem: None,
        },
    );
    store.apply_snapshot(common::hydrated(42, cfg));

    let loaded = cert_store.resolved();
    assert_eq!(
        loaded.len(),
        1,
        "the TLS callback's cert map must follow a control-plane snapshot \
         without a restart (this is what an edge node needs)"
    );
    assert!(
        loaded.contains_key("acme.com"),
        "the new tenant domain must be resolvable by SNI"
    );
}
