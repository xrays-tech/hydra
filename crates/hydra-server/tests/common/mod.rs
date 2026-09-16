//! Shared integration-test helpers.
//!
//! Every persistence test gets its own fresh `:memory:` SQLite database
//! (design wave-2 §3: real engine, never a mock). The pool is pinned to a
//! single connection (see [`db::init_pool`]) so migrations are visible to all
//! queries within the test.

use hydra_server::db;
use sqlx::SqlitePool;

/// A migrated, PRAGMA-configured in-memory pool. One per test.
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
