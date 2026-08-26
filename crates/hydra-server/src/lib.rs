//! # hydra-server — I/O shell over [`hydra_core`].
//!
//! Thin adapter layer: Pingora proxy lifecycle, sqlx store (with `ArcSwap`
//! hot config), reqwest auth upstream, usage sinks, multi-tenant TLS.
//! All "internal logic" lives in the pure core; this crate only translates
//! between I/O (sessions / rows / responses) and core types.
//!
//! **Status:** Wave-1 foundation skeleton — modules are feature-gated and
//! intentionally empty. Waves 2–6 fill them in.
//!
//! ## Feature model
//!
//! The crate is split into composable Cargo features so each wave compiles
//! only the slice of the I/O shell it needs (see `Cargo.toml` `[features]`):
//!
//! | Feature        | Module(s)        | Wave  | Native dep        |
//! | -------------- | ---------------- | ----- | ----------------- |
//! | `runtime`      | `sink`, `admin`  | W3/W5 | tokio/dashmap/... |
//! | `db`           | `db`, `store`    | W2    | sqlx (sqlite)     |
//! | `http-client`  | `http`           | W3    | reqwest (rustls)  |
//! | `proxy`        | `proxy`, `tls`   | W4    | pingora/BoringSSL |
//! | `server`       | (umbrella)       | W4+   | all of the above  |
//! | `usage-clickhouse` | (within sink) | W3  | clickhouse (opt)  |
//!
//! With no features the crate is an empty lib; `db,http-client` builds sqlx +
//! reqwest/rustls **without** pingora/BoringSSL, letting W2/W3 run natively
//! on macOS.
#![forbid(unsafe_code)]

// --- W2: persistence & config store ---------------------------------------
/// At-rest encryption for persisted secrets (provider upstream api-keys).
/// Gated on `db` (the encrypt-on-write / decrypt-on-read boundary is `db.rs`).
#[cfg(feature = "db")]
pub mod crypto;
/// sqlx pool, migrations, and the repo layer.
#[cfg(feature = "db")]
pub mod db;
/// `ConfigStore` — `ArcSwap<ConfigData>` hot-reload shell over the DB.
#[cfg(feature = "db")]
pub mod store;

// --- Cluster mode (v8 plan) ------------------------------------------------
/// Node role (`HYDRA_ROLE`: all/leader/edge), control config, snapshot wire
/// and the control-plane client. Needs the full proxy shell (`proxy` implies
/// `db` + `http-client`).
#[cfg(feature = "proxy")]
pub mod cluster;

// --- Redis backbone (cluster P2+, v8 plan Q5/Q6) ---------------------------
/// Shared Redis state: leader lease (P2), node registry / invalidation bus /
/// shared limits / auth L2 (P4). Opt-in via `cluster-redis` so the default
/// single-node build keeps zero external deps. Requires the proxy shell (the
/// lease store plugs into the cluster module).
#[cfg(all(feature = "cluster-redis", feature = "proxy"))]
pub mod redis;

// --- W3: external boundaries ----------------------------------------------
/// `HttpAuthChecker` (reqwest) + admin `ServeHttp` HTTP helpers.
#[cfg(feature = "http-client")]
pub mod http;
/// `UsageSink` trait + `SqliteSink` / `ClickHouseSink` adapters.
#[cfg(feature = "runtime")]
pub mod sink;

// --- W4: Pingora proxy shell ----------------------------------------------
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
