//! The downstream listener topology must be a function of **deployment
//! config** — never of the config snapshot.
//!
//! Regression for `dev-docs/bug-2026-09-16-tenant-cert-flips-listener-to-tls.md`:
//! writing a tenant certificate (a pure data change) flipped the process's
//! **only** listener from plaintext to TLS, so every restart made the plaintext
//! entry (port 80 → NodePort 30090 → pod `:8080`) answer with RST — interfaces
//! down, process healthy, `/healthz` `/readyz` green, metrics frozen.
//!
//! These tests are deliberately **black box**: they spawn the real `hydra`
//! binary and talk to it over real sockets. Nothing about the internal decision
//! is mocked or re-implemented here, so they fail on the shipped bug and pass
//! only once the binary's topology stops depending on tenant certs.
//!
//! | test | scenario | must hold |
//! |------|----------|-----------|
//! | [`plain_keeps_serving_when_tenant_certs_exist`] | certs + `HYDRA_TLS_LISTEN` | plaintext entry answers **and** the TLS port serves the tenant cert |
//! | [`tls_port_is_bound_even_without_certs`] | no certs + `HYDRA_TLS_LISTEN` | TLS port is bound (config decides, not data) |
//! | [`certs_without_a_tls_port_keep_the_entry_plain`] | certs, no `HYDRA_TLS_LISTEN` | plaintext entry answers + an explicit startup warning |
//! | [`an_unbindable_entry_port_is_loud`] | `HYDRA_LISTEN` already in use | process exits non-zero with a clear reason |
//!
//! ## Reverse falsification (measured 2026-09-16, before the fix)
//!
//! All four failed, in 80.31s, for exactly the reasons in the report:
//!
//! - `plain_keeps_serving_when_tenant_certs_exist` — the entry port never
//!   answered; the process's own log said `TLS accept() failed: [HTTP_REQUEST]`,
//!   i.e. a plaintext request arriving at a TLS listener. That is the production
//!   outage, reproduced end to end.
//! - `tls_port_is_bound_even_without_certs` — `HYDRA_TLS_LISTEN` was not read at
//!   all (`proxy plain-TCP listener bound (no tenant certs configured)`).
//! - `certs_without_a_tls_port_keep_the_entry_plain` — same RST as the first.
//! - `an_unbindable_entry_port_is_loud` — "the process kept running with no
//!   listener on its entry port".
//!
//! After the fix: 4 passed, 0.87s.

#![cfg(any(feature = "tls-boringssl", feature = "tls-openssl"))]

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hydra_core::model::Tenant;
use hydra_server::db;
use pingora_core::tls::ssl::{SslConnector, SslMethod, SslVerifyMode};
use pingora_core::tls::x509::X509;

const NOW: &str = "2026-01-01 00:00:00";

/// `HYDRA_ENCRYPTION_KEY` for the spawned binary: base64 of 32 bytes (all `0x07`).
const TEST_MASTER_KEY: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=";

/// The seeded tenant. Must match the `acme.crt`/`acme.key` fixtures' CN/SAN so
/// the SNI callback has something to select.
const TENANT_DOMAIN: &str = "acme.com";

/// How long a test waits for a listener the config asked for. Generous: the
/// spawned process has to open SQLite, migrate and boot Pingora first.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn fixture_cert_der(name: &str) -> Vec<u8> {
    let pem = std::fs::read(fixture(name)).expect("read fixture cert");
    X509::from_pem(&pem)
        .expect("parse fixture cert PEM")
        .to_der()
        .expect("cert to_der")
}

// ---------------------------------------------------------------------------
// Temp DB
// ---------------------------------------------------------------------------

/// A throwaway SQLite **file** (the spawned binary opens it itself; an
/// in-memory pool would be invisible to another process).
struct TempDb(PathBuf);

impl TempDb {
    /// Create a migrated DB file and (optionally) seed one tenant whose cert
    /// columns point at the fixture PEMs.
    async fn new(with_cert: bool) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "hydra-listener-topology-{}-{}.db",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }

        let pool = db::init_pool(&format!("sqlite:{}?mode=rwc", path.display()))
            .await
            .expect("init_pool on the temp DB file");
        db::run_migrate(&pool).await.expect("run_migrate");
        if with_cert {
            db::insert_tenant(
                &pool,
                &Tenant {
                    id: "t1".into(),
                    name: "t1-tenant".into(),
                    domain: TENANT_DOMAIN.into(),
                    auth_url: format!("https://auth.{TENANT_DOMAIN}/verify"),
                    cert_key: Some(fixture("acme.key")),
                    cert_file: Some(fixture("acme.crt")),
                    enabled: true,
                    created_at: NOW.into(),
                    updated_at: NOW.into(),
                },
            )
            .await
            .expect("insert tenant");
        }
        // The binary opens the same file: release our handle first.
        pool.close().await;
        Self(path)
    }

    fn url(&self) -> String {
        format!("sqlite:{}?mode=rwc", self.0.display())
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

// ---------------------------------------------------------------------------
// The spawned gateway
// ---------------------------------------------------------------------------

/// A spawned `hydra` process. Killed on drop; its output is captured so a
/// failing assertion can quote what the process actually said.
struct Hydra {
    child: Child,
    plain: u16,
    tls: Option<u16>,
    log: Arc<Mutex<String>>,
    _db: TempDb,
}

impl Hydra {
    /// Spawn the real binary. `tls_listen` mirrors `HYDRA_TLS_LISTEN`.
    async fn start(with_cert: bool, tls_listen: bool) -> Self {
        let db = TempDb::new(with_cert).await;
        let plain = common::ephemeral_port();
        let tls = tls_listen.then(common::ephemeral_port);
        let admin = common::ephemeral_port();

        let mut cmd = base_command(&db, plain, admin);
        if let Some(tls) = tls {
            cmd.env("HYDRA_TLS_LISTEN", format!("127.0.0.1:{tls}"));
        }
        let (child, log) = spawn_capturing(cmd);
        Self {
            child,
            plain,
            tls,
            log,
            _db: db,
        }
    }

    fn plain_addr(&self) -> String {
        format!("127.0.0.1:{}", self.plain)
    }

    fn tls_addr(&self) -> String {
        format!("127.0.0.1:{}", self.tls.expect("test asked for a TLS port"))
    }

    fn log_tail(&self) -> String {
        let log = self.log.lock().expect("log mutex");
        let lines: Vec<&str> = log.lines().rev().take(12).collect();
        lines.into_iter().rev().collect::<Vec<_>>().join("\n")
    }

    /// Does the process's output contain `needle`? Retries briefly: the reader
    /// threads append asynchronously.
    fn log_has(&self, needle: &str) -> bool {
        for _ in 0..25 {
            if self.log.lock().expect("log mutex").contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// A plaintext HTTP exchange against the entry port, retried until success
    /// or `timeout`. Panics with the process's own log rather than a bare
    /// timeout: on the buggy binary the entry port is a TLS listener, and the
    /// interesting evidence is what the process printed about its listeners.
    fn plain_request(&self, path: &str) -> String {
        let addr = self.plain_addr();
        let deadline = Instant::now() + READY_TIMEOUT;
        let mut attempts = 0u32;
        while Instant::now() < deadline {
            if let Some(resp) = try_http(&addr, TENANT_DOMAIN, path) {
                return resp;
            }
            attempts += 1;
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!(
            "the plaintext entry port never answered an HTTP request \
             ({attempts} attempts over {:?}) — the deployment entry \
             (80 → NodePort → pod :8080) is plain HTTP, so a listener that is not \
             plain here is a data-plane outage.\n\
             --- process log (tail) ---\n{}",
            READY_TIMEOUT,
            self.log_tail()
        );
    }

    /// Wait until a TCP connection to the TLS port is accepted (the port is
    /// bound). No handshake is attempted: with zero tenant certs the handshake
    /// is expected to fail, and the claim under test is only that **config**,
    /// not the cert data, decides whether the port exists.
    fn wait_for_tls_port(&self) -> bool {
        let addr = self.tls_addr();
        let deadline = Instant::now() + READY_TIMEOUT;
        while Instant::now() < deadline {
            if TcpStream::connect(&addr).is_ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        false
    }

    /// The DER of the certificate the TLS listener selects for `sni`, retried
    /// while the listener comes up.
    fn tls_peer_cert_der(&self, sni: &str) -> Vec<u8> {
        let addr = self.tls_addr();
        let deadline = Instant::now() + READY_TIMEOUT;
        let mut last = String::from("(no attempt)");
        while Instant::now() < deadline {
            match try_tls_peer_cert_der(&addr, sni) {
                Some(der) => return der,
                None => last = format!("TLS handshake to {addr} (SNI={sni}) failed"),
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!(
            "no TLS listener served {sni} at {addr} — {last}\n\
             --- process log (tail) ---\n{}",
            self.log_tail()
        );
    }
}

impl Drop for Hydra {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn base_command(db: &TempDb, plain: u16, admin: u16) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hydra"));
    cmd.env("HYDRA_DB_URL", db.url())
        .env("HYDRA_ENCRYPTION_KEY", TEST_MASTER_KEY)
        .env("HYDRA_LISTEN", format!("127.0.0.1:{plain}"))
        .env("HYDRA_ADMIN_ADDR", format!("127.0.0.1:{admin}"))
        .env("RUST_LOG", "info")
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null());
    cmd
}

/// Spawn, capturing stdout+stderr into a shared buffer.
fn spawn_capturing(mut cmd: Command) -> (Child, Arc<Mutex<String>>) {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the hydra binary");
    let log = Arc::new(Mutex::new(String::new()));
    for stream in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let log = log.clone();
        std::thread::spawn(move || {
            let mut stream = stream;
            let mut buf = [0u8; 4096];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Ok(mut log) = log.lock() {
                            log.push_str(&String::from_utf8_lossy(&buf[..n]));
                        }
                    }
                }
            }
        });
    }
    (child, log)
}

// ---------------------------------------------------------------------------
// Raw socket helpers (no HTTP/TLS client library beyond the linked backend)
// ---------------------------------------------------------------------------

/// A plaintext HTTP/1.1 exchange. `None` = nothing usable came back (refused,
/// reset, or — the bug — a TLS listener that cannot answer plaintext bytes).
fn try_http(addr: &str, host: &str, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect(addr).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf).to_string();
    if text.starts_with("HTTP/") {
        Some(text)
    } else {
        None
    }
}

fn try_tls_peer_cert_der(addr: &str, sni: &str) -> Option<Vec<u8>> {
    let connector = SslConnector::builder(SslMethod::tls()).ok()?.build();
    let mut cfg = connector.configure().ok()?;
    // Self-signed fixtures: verification is skipped and the DER is compared.
    cfg.set_verify(SslVerifyMode::NONE);
    cfg.set_use_server_name_indication(true);
    let stream = TcpStream::connect(addr).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let tls = cfg.connect(sni, stream).ok()?;
    tls.ssl().peer_certificate()?.to_der().ok()
}

// ---------------------------------------------------------------------------
// T1 — tenant certs must not take the plaintext entry away
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_keeps_serving_when_tenant_certs_exist() {
    let hydra = Hydra::start(true, true).await;

    // The entry path (80 → NodePort → pod :8080) is plaintext and must stay so.
    let resp = hydra.plain_request("/v1/models");
    assert!(
        resp.starts_with("HTTP/"),
        "expected an HTTP status line, got: {resp}"
    );

    // The HTTPS listener, when configured, still serves the tenant's cert by SNI.
    let der = hydra.tls_peer_cert_der(TENANT_DOMAIN);
    assert_eq!(
        der,
        fixture_cert_der("acme.crt"),
        "the TLS listener must present the tenant cert selected by SNI"
    );

    // The startup banner states the whole protocol shape (bug report §5.5), so
    // a restart makes it obvious what a replica is serving.
    assert!(
        hydra.log_has("downstream listeners (config-derived)"),
        "the startup log must state the listener topology; log tail:\n{}",
        hydra.log_tail()
    );
}

// ---------------------------------------------------------------------------
// T2 — topology comes from config, not from the cert data
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tls_port_is_bound_even_without_certs() {
    let hydra = Hydra::start(false, true).await;

    assert!(
        hydra.wait_for_tls_port(),
        "HYDRA_TLS_LISTEN was configured, so the port must be bound even with \
         zero tenant certs (a cert written later must not need a restart to \
         light up HTTPS)\n--- process log (tail) ---\n{}",
        hydra.log_tail()
    );
    let resp = hydra.plain_request("/v1/models");
    assert!(resp.starts_with("HTTP/"), "got: {resp}");
}

// ---------------------------------------------------------------------------
// T3 — certs without a TLS port: plaintext entry + an explicit warning
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certs_without_a_tls_port_keep_the_entry_plain() {
    let hydra = Hydra::start(true, false).await;

    let resp = hydra.plain_request("/v1/models");
    assert!(resp.starts_with("HTTP/"), "got: {resp}");

    // Silence is how this defect shipped: the operator has to be told that the
    // tenant certs are not being served anywhere.
    let log = hydra.log_tail();
    assert!(
        log.contains("HYDRA_TLS_LISTEN"),
        "the startup log must name HYDRA_TLS_LISTEN when tenants have certs but \
         no TLS port is configured; log tail:\n{log}"
    );
}

// ---------------------------------------------------------------------------
// T4 — an unbindable entry port must not end as "alive, healthy, deaf"
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unbindable_entry_port_is_loud() {
    // Hold the port for the whole test: Pingora cannot bind it.
    let held = TcpListener::bind("127.0.0.1:0").expect("occupy a port");
    let held_port = held.local_addr().expect("local_addr").port();

    let db = TempDb::new(false).await;
    let url = db.url();
    let admin = common::ephemeral_port();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hydra"));
    cmd.env("HYDRA_DB_URL", url)
        .env("HYDRA_ENCRYPTION_KEY", TEST_MASTER_KEY)
        .env("HYDRA_LISTEN", format!("127.0.0.1:{held_port}"))
        .env("HYDRA_ADMIN_ADDR", format!("127.0.0.1:{admin}"))
        .env("RUST_LOG", "info")
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null());
    let (mut child, log) = spawn_capturing(cmd);

    // Without the fix the process stays alive forever: Pingora retries the bind
    // for 30 s, then panics *inside its service task* — the process keeps
    // serving admin probes while the data plane has no listener at all.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut exited = None;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("try_wait") {
            exited = Some(status);
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let Some(status) = exited else {
        let _ = child.kill();
        let _ = child.wait();
        let log = log.lock().expect("log mutex").clone();
        panic!(
            "the process kept running with no listener on its entry port; a \
             gateway that cannot bind its data plane must fail loudly instead \
             of reporting healthy\n--- output ---\n{log}"
        );
    };

    assert!(
        !status.success(),
        "the process exited 0 even though its entry port could not be bound"
    );
    let log = log.lock().expect("log mutex").clone();
    assert!(
        log.contains("HYDRA_LISTEN"),
        "the failure must name the offending setting (HYDRA_LISTEN); output:\n{log}"
    );
}
