#![cfg(any(feature = "tls-boringssl", feature = "tls-openssl"))]
//! Plan T10.3 — PROCESS-level boot wiring, not the pure planner.
//!
//! `listeners.rs` has good unit tests for `plan()`, but two behaviours only exist
//! once the real binary runs:
//!
//!   1. an unusable listener configuration (here: plaintext and TLS on the SAME
//!      address) must make the process EXIT NON-ZERO with a fatal log line, and
//!   2. a TLS port that is already taken must NOT take the data plane down: the
//!      process keeps running, the plaintext entry keeps serving, and the failure
//!      is reported (log + `hydra_listener_misconfig_total{kind="tls_bind_failed"}`).
//!
//! The first line must stay feature-gated: without a TLS backend the TLS listener
//! does not exist at all.

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hydra_server::db;

/// `HYDRA_ENCRYPTION_KEY`: base64 of 32 bytes (all `0x07`), as the binary requires.
const TEST_MASTER_KEY: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=";

/// A throwaway migrated SQLite file that the spawned binary opens.
struct TempDb(PathBuf);

impl TempDb {
    async fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "hydra-boot-listeners-{}-{}.db",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        let pool = db::init_pool(&format!("sqlite:{}?mode=rwc", path.display()))
            .await
            .expect("init_pool");
        db::run_migrate(&pool).await.expect("run_migrate");
        pool.close().await; // the child opens the same file
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

/// Spawn, capturing stdout AND stderr into one buffer.
///
/// The `tracing` subscriber is built without `.with_writer`, so it writes to
/// STDOUT by default while panics and `eprintln!` go to stderr: asserting on only
/// one of them would time out forever even though the message was printed.
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
                        if let Ok(text) = std::str::from_utf8(&buf[..n]) {
                            log.lock().expect("log mutex").push_str(text);
                        }
                    }
                }
            }
        });
    }
    (child, log)
}

fn log_has(log: &Arc<Mutex<String>>, needle: &str) -> bool {
    for _ in 0..40 {
        if log.lock().expect("log mutex").contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn log_text(log: &Arc<Mutex<String>>) -> String {
    log.lock().expect("log mutex").clone()
}

/// A. Plaintext and TLS on the SAME address is a fatal configuration: the process
/// must refuse to start rather than come up half-listening.
#[tokio::test]
async fn same_plaintext_and_tls_address_is_fatal() {
    let db = TempDb::new().await;
    let port = common::ephemeral_port();
    let admin = common::ephemeral_port();
    let mut cmd = base_command(&db, port, admin);
    // Identical address for both listeners — the planner rejects this.
    cmd.env("HYDRA_TLS_LISTEN", format!("127.0.0.1:{port}"));

    let (mut child, log) = spawn_capturing(cmd);
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the process must EXIT on a fatal listener config, but it is still \
             running. Output so far:\n{}",
            log_text(&log)
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        !status.success(),
        "a fatal listener configuration must exit NON-ZERO, got {status:?}. \
         Output:\n{}",
        log_text(&log)
    );
    assert!(
        log_has(&log, "HYDRA_LISTEN and HYDRA_TLS_LISTEN both point at"),
        "the fatal reason must be logged (stdout or stderr):\n{}",
        log_text(&log)
    );
}

/// B. A TLS port that an unrelated process already holds must degrade, not crash:
/// the process keeps running and the plaintext entry still serves.
#[tokio::test]
async fn a_taken_tls_port_degrades_to_plaintext() {
    // Occupy a port and KEEP holding it (dropping the listener would free it).
    let squatter = TcpListener::bind("127.0.0.1:0").expect("bind squatter");
    let taken = squatter.local_addr().expect("addr").port();

    let db = TempDb::new().await;
    let plain = common::ephemeral_port();
    let admin = common::ephemeral_port();
    let mut cmd = base_command(&db, plain, admin);
    cmd.env("HYDRA_TLS_LISTEN", format!("127.0.0.1:{taken}"));

    let (mut child, log) = spawn_capturing(cmd);
    let deadline = Instant::now() + Duration::from_secs(20);
    let plain_addr = format!("127.0.0.1:{plain}");
    let mut served = false;
    while Instant::now() < deadline {
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "a taken TLS port must NOT kill the process (the data plane stays up). \
             Output:\n{}",
            log_text(&log)
        );
        // The plaintext entry must accept a connection.
        if let Ok(mut s) = TcpStream::connect(&plain_addr) {
            let _ = s.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n");
            let mut buf = [0u8; 64];
            if s.read(&mut buf).map(|n| n > 0).unwrap_or(false) {
                served = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        served,
        "the plaintext listener must keep serving after the TLS bind failed. \
         Output:\n{}",
        log_text(&log)
    );

    // The failure must be REPORTED — by the real log text or by the metric. The
    // log literal is asserted exactly; `tls_bind_failed` is only a Prometheus
    // LABEL, so it must never be asserted as log text.
    let reported_by_log = log_has(&log, "could not bind the configured TLS listener");
    if !reported_by_log {
        let metrics = format!("http://127.0.0.1:{admin}/metrics");
        let body = reqwest::get(&metrics)
            .await
            .expect("fetch /metrics")
            .text()
            .await
            .expect("metrics body");
        assert!(
            body.contains("hydra_listener_misconfig_total")
                && body.contains("kind=\"tls_bind_failed\""),
            "the degraded TLS bind must be observable either in the log or in \
             /metrics. Log:\n{}\nMetrics excerpt:\n{}",
            log_text(&log),
            body.lines()
                .filter(|l| l.contains("listener_"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    let _ = child.kill();
    let _ = child.wait();
    drop(squatter);
}

/// C. KNOWN LIMITATION (asserted, not hidden): the planner compares listener
/// addresses as STRINGS, so the same PORT on different bind addresses
/// (`0.0.0.0:8080` vs `127.0.0.1:8080`) is NOT caught statically. It is left to
/// the runtime: `probe_bind` plus Pingora's all-or-nothing service build.
///
/// Marked `#[ignore]` on purpose — the plan requires the limitation to be
/// recorded explicitly rather than silently "fixed" by a check that would also
/// reject legitimate configurations (different interfaces, same port, is a valid
/// setup on some hosts).
#[tokio::test]
#[ignore = "known limitation: same port on different bind addresses is not detected statically"]
async fn same_port_on_different_addresses_is_not_detected_statically() {
    let db = TempDb::new().await;
    let port = common::ephemeral_port();
    let admin = common::ephemeral_port();
    let mut cmd = base_command(&db, port, admin);
    cmd.env("HYDRA_LISTEN", format!("0.0.0.0:{port}"))
        .env("HYDRA_TLS_LISTEN", format!("127.0.0.1:{port}"));

    let (mut child, log) = spawn_capturing(cmd);
    // The current implementation lets this through (pure string comparison).
    std::thread::sleep(Duration::from_secs(3));
    let still_running = child.try_wait().expect("try_wait").is_none();
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        still_running,
        "documented behaviour: this configuration is NOT rejected statically. \
         Output:\n{}",
        log_text(&log)
    );
}
