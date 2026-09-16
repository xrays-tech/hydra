//! Shared integration-test helpers.
//!
//! Every persistence test gets its own fresh `:memory:` SQLite database
//! (design wave-2 §3: real engine, never a mock). The pool is pinned to a
//! single connection (see [`db::init_pool`]) so migrations are visible to all
//! queries within the test.

use hydra_server::db;
use sqlx::SqlitePool;

/// A migrated, PRAGMA-configured in-memory pool. One per test.
// Each test target compiles this module on its own, so a helper used by one
// target is "dead code" in all the others (same as [`ephemeral_port`]).
#[allow(dead_code)]
pub async fn setup_pool() -> SqlitePool {
    let pool = db::init_pool("sqlite::memory:")
        .await
        .expect("init_pool should connect to :memory:");
    db::run_migrate(&pool)
        .await
        .expect("run_migrate should create the schema");
    pool
}

/// A REAL Redis for integration tests (dev-plan 铁律 2: Redis is an external
/// system boundary and the in-process double is no longer acceptable).
///
/// The endpoint comes from `HYDRA_TEST_REDIS_URL`; when it is unset the helper
/// PANICS with the commands needed to start one — never a silent skip, never a
/// fallback to a mock.
///
/// Databases are partitioned so parallel test BINARIES cannot collide: lib unit
/// tests own 1..=40 (`redis::test_redis::isolated_pool`), integration tests
/// hand-assign 41..=63 here. Each call flushes its database.
// Each test target compiles this module on its own, so a helper used by one
// target is "dead code" in all the others.
#[allow(dead_code)]
#[cfg(feature = "cluster-redis")]
pub async fn real_redis_pool(db: u8) -> fred::clients::Pool {
    use fred::prelude::*;
    assert!(
        (41..=63).contains(&db),
        "integration tests use Redis database 41..=63 (lib unit tests own 1..=40)"
    );
    let base = std::env::var("HYDRA_TEST_REDIS_URL").unwrap_or_else(|_| {
        panic!(
            "HYDRA_TEST_REDIS_URL is not set, and this test needs a REAL Redis \
             (dev-plan 铁律 2: no in-process Redis mock).\n  \
             start one: docker compose -f environment/docker-compose.local.yml up -d redis-test\n  \
             then:      export HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380"
        )
    });
    let url = format!("{}/{}", base.trim_end_matches('/'), db);
    let config = Config::from_url(&url).expect("HYDRA_TEST_REDIS_URL must parse");
    let pool = Pool::new(config, None, None, None, 1).expect("test pool builds");
    pool.init()
        .await
        .unwrap_or_else(|e| panic!("cannot reach the test Redis at {url}: {e}"));
    // Clear only THIS database: FLUSHALL would wipe the databases the other
    // test binaries are using.
    let _: Value = pool
        .custom(
            fred::types::CustomCommand::new_static("FLUSHDB", None, false),
            Vec::<String>::new(),
        )
        .await
        .unwrap_or_else(|e| panic!("cannot flush test db {db}: {e}"));
    pool
}

/// How many 100-port bands the allocator spreads processes across. Below the
/// Linux ephemeral range (32768+) so Pingora's own outbound connections cannot
/// collide with the block.
pub const PORT_BANDS: u64 = 200;
/// Ports probed inside one band before giving up.
pub const PORTS_PER_BAND: u32 = 100;

/// The band a process id maps to. Exposed so a test can pin the collision rule
/// (see `tests/test_port_allocation.rs`) without duplicating the formula.
///
/// A HASH, not `pid % N`: with `% 100` two pids 100 apart shared a band, so two
/// test processes fought over one range of ports. Hashing moves collisions — it
/// does not remove them: `port_band(pid) == port_band(pid')` exactly when
/// `pid ≡ pid' (mod PORT_BANDS)`.
#[must_use]
pub fn port_band(pid: u64) -> u64 {
    pid.wrapping_mul(2_654_435_761) % PORT_BANDS
}

/// A unique TCP port for a test listener.
///
/// HONEST ABOUT THE MECHANISM: the candidate is probed with a real `bind` and the
/// probe socket is then RELEASED, because the caller hands this port to Pingora,
/// which binds it itself. Holding the socket here would make that second bind
/// fail with `EADDRINUSE` (Pingora sets only `SO_REUSEADDR`, never
/// `SO_REUSEPORT`), i.e. it would break nearly every integration test rather than
/// fix anything. So the probe-to-bind window still exists; what this function
/// does is make two PROCESSES very unlikely to probe the same candidate at the
/// same moment, not reserve a port.
///
/// The per-process band is derived by HASHING the pid instead of `pid % N`. With
/// `% 100`, two pids 100 apart were handed the SAME band, so two test binaries
/// (or a test run and a leftover process) fought over one block of ports — which
/// is how a test's requests ended up answered by another test's proxy,
/// observed as a mystery 404 from a pass-through assertion
/// (`catalog_get_v1_models_id_still_passes_through_upstream`, ~1 run in 7).
///
/// RESIDUAL RISK, stated rather than papered over: hashing changes which pids
/// collide, it does not eliminate collisions — `band(pid) == band(pid')` exactly
/// when `pid ≡ pid' (mod 200)`. Two processes whose pids differ by a multiple of
/// 200 still share a band (and a single process that runs out of its 100 probed
/// ports still panics).
#[allow(dead_code)]
pub fn ephemeral_port() -> u16 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);

    let base = 12_000u32 + (port_band(u64::from(std::process::id())) as u32) * PORTS_PER_BAND;
    for _ in 0..PORTS_PER_BAND {
        let port = (base + NEXT.fetch_add(1, Ordering::Relaxed) % PORTS_PER_BAND) as u16;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port; // probe socket dropped here, on purpose (see above)
        }
    }
    panic!("no free test port in the allocated block");
}

/// A hydrated wire with NO fidelity rows, for tests that only exercise the
/// config swap / version adoption. Production never builds one this way: the
/// leader uses `ReplicationContent::load` and a replica uses `hydrate` on a real
/// wire (which supplies both).
#[allow(dead_code)]
pub fn hydrated(
    version: u64,
    cfg: hydra_core::config::ConfigData,
) -> hydra_server::cluster::snapshot::HydratedWire {
    hydra_server::cluster::snapshot::HydratedWire {
        version,
        cfg,
        fidelity: hydra_server::cluster::content::FidelityRows {
            limit_roles: Vec::new(),
            key_prefix_bindings: Vec::new(),
            provider_keys: Vec::new(),
            tenant_token_hashes: Vec::new(),
            provider_models: Vec::new(),
            tenant_providers: Vec::new(),
            tenant_models: Vec::new(),
        },
    }
}
