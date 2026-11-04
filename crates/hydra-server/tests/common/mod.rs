//! Shared integration-test helpers.
//!
//! Every persistence test gets its own fresh `:memory:` SQLite database
//! (design wave-2 §3: real engine, never a mock). The pool is pinned to a
//! single connection (see [`db::init_pool`]) so migrations are visible to all
//! queries within the test.

use hydra_server::db;
use sqlx::SqlitePool;

// ---------------------------------------------------------------------------
// Identity seeding helpers (multi-token / internal-auth model)
//
// The tenant gate resolves a bearer from `ConfigData.tenant_tokens`, which the
// loader builds from the `tenant_access_token` TABLE (enabled rows only) — a
// hash written to the legacy `tenant.access_token_hash` column is no longer
// what the gate reads. The client-api-key path (`enforce_auth`) resolves
// `ConfigData.users_by_api_key_hash`, derived from the `user_group` / `user`
// / `user_api_key` tables. These helpers write the rows the snapshot is
// actually built from.
//
// Ids are DETERMINISTIC (`tok-<tenant>-<name>`, `grp-<tenant>`, …) so a test
// can predict and rotate/delete a seeded row; the admin `gen_id()` is
// `pub(crate)` and nanosecond-based, which would make such assertions flaky.
// ---------------------------------------------------------------------------

/// Display atoms for a seeded credential, same SHAPE the admin UI reads: the
/// first 8 characters as the prefix, the last 4 as the tail. Never the hash,
/// never used for matching — only so the seeded row looks like a real one.
#[allow(dead_code)]
fn display_atoms(plaintext: &str) -> (String, String) {
    let prefix = plaintext.chars().take(8).collect::<String>();
    let mut tail = String::new();
    for ch in plaintext.chars().rev().take(4) {
        tail.insert(0, ch);
    }
    (prefix, tail)
}

/// Seed one tenant access token (hash + display atoms of `plaintext`) into
/// `tenant_access_token`. Returns the deterministic row id — a test that
/// rotates the token deletes the old row by this id.
#[allow(dead_code)]
pub async fn seed_tenant_access_token(
    pool: &SqlitePool,
    tenant_id: &str,
    name: &str,
    plaintext: &str,
    enabled: bool,
    expires_at: Option<String>,
) -> String {
    let id = format!("tok-{tenant_id}-{name}");
    let hash = hydra_core::auth::sha256_hex_string(plaintext.as_bytes());
    let (prefix, last4) = display_atoms(plaintext);
    db::insert_tenant_access_token(
        pool,
        &hydra_core::model::TenantAccessToken {
            id: id.clone(),
            tenant_id: tenant_id.into(),
            name: name.into(),
            token_hash: hash,
            token_prefix: prefix,
            token_last4: last4,
            enabled,
            expires_at,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        },
    )
    .await
    .expect("insert tenant access token");
    id
}

/// Seed the full client identity a data-plane request needs under the internal
/// auth model: a `user_group`, an ACTIVE `user` (enabled, Active billing) and
/// a `user_api_key` whose hash `enforce_auth` resolves.
/// Returns `(group_id, user_id, key_id)`.
#[allow(dead_code)]
pub async fn seed_api_identity(
    pool: &SqlitePool,
    tenant_id: &str,
    api_key_plaintext: &str,
) -> (String, String, String) {
    let group_id = format!("grp-{tenant_id}");
    let user_id = format!("usr-{tenant_id}");
    let key_id = format!("key-{tenant_id}");

    db::upsert_user_group(
        pool,
        &hydra_core::model::UserGroup {
            id: group_id.clone(),
            tenant_id: tenant_id.into(),
            name: format!("{tenant_id}-group"),
            enabled: true,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        },
    )
    .await
    .expect("upsert user_group");

    db::upsert_user(
        pool,
        &hydra_core::model::User {
            id: user_id.clone(),
            tenant_id: tenant_id.into(),
            user_group_id: group_id.clone(),
            external_id: None,
            name: format!("{tenant_id}-user"),
            enabled: true,
            billing_status: hydra_core::model::BillingStatus::Active,
            billing_reason: None,
            billing_updated_at: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        },
    )
    .await
    .expect("upsert user");

    let hash = hydra_core::auth::sha256_hex_string(api_key_plaintext.as_bytes());
    let (prefix, last4) = display_atoms(api_key_plaintext);
    db::insert_user_api_key(
        pool,
        &hydra_core::model::UserApiKey {
            id: key_id.clone(),
            user_id: user_id.clone(),
            name: format!("{tenant_id}-key"),
            key_hash: hash,
            key_prefix: prefix,
            key_last4: last4,
            enabled: true,
            expires_at: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        },
    )
    .await
    .expect("insert user_api_key");

    (group_id, user_id, key_id)
}

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
///
/// Hand-assigning means two files CAN claim the same number, and then each one
/// `FLUSHDB`s the other's keys mid-assertion: a flaky failure whose cause is
/// invisible from the failing test (`registry_reaping.rs` and
/// `loader.rs` both used 47 until round 10). That rule is
/// enforced now by `tests/redis_db_partition.rs`, which scans the sibling test
/// sources and fails when a number is claimed twice.
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
    // Through the single owner (production's command timeout, watchdog and reconnect
    // policy): fred's defaults here were `0` (wait forever) and no reconnect at all.
    let pool = hydra_server::redis::build_pool(&url, 1).expect("test pool builds");
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
            provider_keys: Vec::new(),
            tenant_token_hashes: Vec::new(),
            provider_models: Vec::new(),
            tenant_providers: Vec::new(),
            tenant_models: Vec::new(),
            user_groups: Vec::new(),
            users: Vec::new(),
            user_api_keys: Vec::new(),
            tenant_access_tokens: Vec::new(),
            global_roles: Vec::new(),
        },
    }
}
