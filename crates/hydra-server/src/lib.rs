//! # hydra-server — I/O shell over [`hydra_core`].
//!
//! Thin adapter layer: Pingora proxy lifecycle, sqlx store (with `ArcSwap`
//! hot config), reqwest auth upstream, usage sinks, multi-tenant TLS.
//! All "internal logic" lives in the pure core; this crate only translates
//! between I/O (sessions / rows / responses) and core types.
//!
//! **Status:** implemented. The modules are feature-gated so a build compiles
//! only the slice of the I/O shell it needs (see `Cargo.toml` `[features]`):
//!
//! | Feature        | Module(s)        | Wave  | Native dep                     |
//! | -------------- | ---------------- | ----- | ------------------------------ |
//! | `runtime`      | `sink`, `admin`  | W3/W5 | tokio/dashmap/...              |
//! | `db`           | `db`, `store`    | W2    | sqlx (PostgreSQL / MySQL)      |
//! | `http-client`  | `http`           | W3    | reqwest (rustls)               |
//! | `proxy`        | `proxy`, `tls`   | W4    | pingora/BoringSSL              |
//! | `server`       | (umbrella)       | W4+   | all of the above               |
//! | `usage-clickhouse` | (within sink) | W3  | clickhouse (opt)               |
//!
//! `db-postgres` or `db-mysql` is mandatory (exactly one); `db,http-client`
//! builds sqlx + reqwest/rustls **without** pingora/BoringSSL, which is what
//! lets the persistence and HTTP layers build natively on macOS.
#![forbid(unsafe_code)]

// Phase 2: every hydra-server recipe selects exactly one relational backend. A
// feature-less build must fail loudly instead of compiling an empty shell that
// looks deployable.
#[cfg(not(any(feature = "db-postgres", feature = "db-mysql")))]
compile_error!(
    "hydra-server requires exactly one database backend feature: `db-postgres` or `db-mysql`"
);

// --- W2: persistence & config store ---------------------------------------
/// At-rest encryption for persisted secrets (provider upstream api-keys).
/// Gated on `db` (the encrypt-on-write / decrypt-on-read boundary is `db.rs`).
///
/// The `#[cfg]` below is LOAD-BEARING and was restored on 2026-10-08: `lock_gate` (round 201) had been
/// inserted between this doc comment and its item, taking the attribute with it, so `crypto` was
/// compiled in EVERY configuration while its dependencies (`aes-gcm`, `base64`, `rand`) are pulled in
/// by `db` alone — a bare `runtime` slice would have failed inside `crypto.rs` instead of leaving the
/// module out. Every configuration CI builds implies `db` today (`proxy` → `db`, `server` → `db`), so
/// the breakage was latent rather than visible.
#[cfg(feature = "db")]
pub mod crypto;

/// Lock a mutex, recovering from poisoning instead of panicking.
///
/// SINGLE OWNER (round 201): this started as a private helper in `proxy::admission` after a review
/// pointed out that a panic in an UNRELATED thread must not turn every later request through a shared
/// gate into a panic — the values behind those locks are plain copies recorded for reporting, with no
/// invariant a half-finished writer could break. The cluster control client still had three
/// `lock().expect("control url mutex")` sites for a value of exactly the same kind (a cached control
/// URL, replaced wholesale), so the same policy now applies through this one function rather than a
/// private copy per module. `admission.rs`'s test module pins the behaviour (a poisoned lock is
/// recovered, not fatal).
///
/// Use it whenever the guarded value is a plain snapshot; keep `.expect(...)` where a poisoned lock
/// really does mean an invariant was broken mid-update.
///
/// The gate names BOTH users (2026-10-08, decision D-13): the request path and the config store are
/// `db`, and the usage backends that own a `Mutex` — TDengine (`usage-tdengine`) and GreptimeDB
/// (`usage-greptimedb`) — are features that imply no database feature. Gating on `db` alone would
/// make those combinations fail to compile the moment they used the helper.
#[cfg(any(
    feature = "db",
    feature = "usage-tdengine",
    feature = "usage-greptimedb"
))]
pub(crate) fn lock_gate<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// sqlx pool, migrations, and the repo layer.
#[cfg(feature = "db")]
pub mod db;
/// `ConfigStore` — `ArcSwap<ConfigData>` hot-reload shell over the DB.
#[cfg(feature = "db")]
pub mod store;

// --- Cluster mode (shared-DB architecture) ---------------------------------
/// Node role (`HYDRA_ROLE`: all|cluster) and cluster configuration. Needs the
/// full proxy shell (`proxy` implies `db` + `http-client`).
#[cfg(feature = "proxy")]
pub mod cluster;

// --- Redis backbone (cluster P2+) ------------------------------------------
/// Shared Redis state: the invalidation stream, shared rate limits, breaker
/// votes and the auth L2 cache. The leader lease and node registry were retired
/// by the shared-DB architecture. Opt-in via `cluster-redis` so the default
/// single-node build keeps zero external deps. Requires the proxy shell.
#[cfg(all(feature = "cluster-redis", feature = "proxy"))]
pub mod redis;

// --- W3: external boundaries ----------------------------------------------
/// Upstream-response protocol helpers + admin `ServeHttp` HTTP helpers.
#[cfg(feature = "http-client")]
pub mod http;
/// Tenant self-service API: the reserved `/tenant/` prefix on the DATA-PLANE
/// listener, gated by the tenant access token.
///
/// Gated on `proxy`: it is called from `proxy::request_filter` and holds
/// `AppState`.
#[cfg(feature = "proxy")]
pub mod tenant_api;

/// **Which usage backends exist** (ADR-0002): one descriptor per backend, one `open()` that returns
/// the writer and the reader together. `main` no longer knows any backend by name.
///
/// Gated on `db`: a backend is opened against the node's own database (every node has one, ADR-0001
/// D-2), and the reader trait lives here too.
#[cfg(feature = "db")]
pub mod usage;

// --- W4: Pingora proxy shell ----------------------------------------------
/// Downstream listener topology — the single owner of "which port speaks which
/// protocol" (design §12.1 / §15.1). The decision is a pure function of
/// **deployment config**; tenant certs are reported, never consulted.
#[cfg(feature = "proxy")]
pub mod listeners;
/// `ProxyHttp` impl wiring core fns to Pingora hooks.
#[cfg(feature = "proxy")]
pub mod proxy;
/// `HydraCertStore` — multi-tenant dynamic SNI certificate callback (design
/// §12). Only compiled when a TLS backend (`tls-boringssl` / `tls-openssl`) is
/// enabled: it uses the pingora `x509`/`pkey`/`ssl`/`ext` types that exist only
/// under a real backend (plain `proxy` links the `noop_tls` stub instead).
#[cfg(any(feature = "tls-boringssl", feature = "tls-openssl"))]
pub mod tls;

// --- W5: admin service & observability ------------------------------------
/// `ServeHttp` admin REST API + self-hosted metrics. Depends on the proxy shell
/// (`CircuitBreaker`, `new_trace_id`), the config store + repo (`db`) and the
/// auth checker (`http-client`), so it is gated on `proxy` (which since W5
/// implies `db` + `http-client`); the `metrics` sub-module is reached by the
/// proxy / breaker / tls `record_*` call-sites.
#[cfg(feature = "proxy")]
pub mod admin;
