//! §2.1 — migrations & connection: tables created, PRAGMAs applied, idempotent.

mod common;

use sqlx::Row;

const SEVEN_BUSINESS_TABLES: &[&str] = &[
    "provider",
    "provider_model",
    "provider_key",
    "tenant",
    "tenant_provider",
    "tenant_model",
    "limit_role",
];

/// The table the SQLite usage store used, DROPPED by migration 0013 (ADR-0002 D-3).
///
/// Named here because its ABSENCE is the claim: a migration that silently stopped running, or a
/// database restored from a pre-0013 backup, would otherwise go unnoticed until someone wondered
/// why usage was being written locally again.
const DROPPED_USAGE_TABLE: &str = "usage_record";

/// T1.1 — after migrate, `sqlite_master` contains all 7 business tables plus
/// the `_sqlx_migrations` bookkeeping table — and NOT the dropped usage table.
#[tokio::test]
async fn migrate_creates_all_tables() {
    let pool = common::setup_pool().await;

    let rows = sqlx::query("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .fetch_all(&pool)
        .await
        .expect("query sqlite_master");

    let mut names: Vec<String> = rows.iter().map(|r| r.get::<String, _>(0)).collect();
    names.sort();

    for expected in SEVEN_BUSINESS_TABLES
        .iter()
        .chain(std::iter::once(&"_sqlx_migrations"))
    {
        assert!(
            names.iter().any(|n| n == expected),
            "expected table '{expected}' to exist, tables were: {names:?}"
        );
    }
}

// `the_sub_tenant_usage_index_exists` stood here, asserting `idx_usage_record_sub_tenant` on
// `(tenant_id, sub_tenant_id, created_at)`. Migration 0013 drops the table, and an index cannot
// outlive it, so the subject is gone rather than unasserted — `usage_record_is_gone` below asserts
// the outcome that matters now.

/// Migration 0013 must have DROPPED the local usage table (ADR-0002 D-3).
///
/// The ruling was "delete it, do not keep it", and this is the executable form: a fresh database
/// must not have the table at all. (The migrations that CREATED and extended it are history and
/// stay untouched — `sqlx::migrate!` checksums them, so editing them would break every existing
/// database; a new migration is the only way to remove anything.)
#[tokio::test]
async fn usage_record_is_gone() {
    let pool = common::setup_pool().await;
    let rows = sqlx::query("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?1")
        .bind(DROPPED_USAGE_TABLE)
        .fetch_all(&pool)
        .await
        .expect("query sqlite_master");
    assert!(
        rows.is_empty(),
        "migration 0013 must DROP {DROPPED_USAGE_TABLE}; it is still present in a fresh database"
    );

    // And the indexes that belonged to it went with it (SQLite drops them with the table) — a
    // leftover index would mean the DROP did not run against the schema this test thinks it has.
    let leftovers: Vec<String> =
        sqlx::query("SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = ?1")
            .bind(DROPPED_USAGE_TABLE)
            .fetch_all(&pool)
            .await
            .expect("query sqlite_master indexes")
            .iter()
            .map(|r| r.get::<String, _>(0))
            .collect();
    assert!(
        leftovers.is_empty(),
        "the dropped table's indexes must go with it, found: {leftovers:?}"
    );
}

/// T2.1 — `foreign_keys=ON` on the in-memory pool; `journal_mode=WAL` (which
/// requires a file — `:memory:` silently degrades to `memory`) is verified on
/// a temp file database (wave-2 §6 note).
#[tokio::test]
async fn pragma_settings_applied() {
    // foreign_keys on :memory: (returns 0/1 INTEGER).
    let mem = common::setup_pool().await;
    let fk: i64 = sqlx::query("PRAGMA foreign_keys")
        .fetch_one(&mem)
        .await
        .expect("fetch foreign_keys")
        .get(0);
    assert_eq!(fk, 1, "foreign_keys must be ON");

    // WAL on a temp file database (WAL needs a file; verify explicitly here).
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("hydra-wal-test-{unique}.db"));
    let url = format!("sqlite://{}?mode=rwc", path.display());

    let pool = hydra_server::db::init_pool(&url)
        .await
        .expect("init_pool file");
    hydra_server::db::run_migrate(&pool)
        .await
        .expect("migrate file");

    // journal_mode returns TEXT ("wal" on a file database).
    let jm: String = sqlx::query("PRAGMA journal_mode")
        .fetch_one(&pool)
        .await
        .expect("fetch journal_mode")
        .get(0);
    assert_eq!(
        jm.to_ascii_lowercase(),
        "wal",
        "journal_mode must be WAL on a file database, got {jm:?}"
    );

    drop(pool);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
    let _ = std::fs::remove_file(path.with_extension("db-shm"));
}

/// T3.1 — running migrate twice does not error and does not change the schema.
#[tokio::test]
async fn migrate_idempotent() {
    let pool = common::setup_pool().await;

    let before = table_set(&pool).await;

    // Second run is a no-op.
    hydra_server::db::run_migrate(&pool)
        .await
        .expect("re-running migrate must be idempotent");

    let after = table_set(&pool).await;
    assert_eq!(before, after, "schema must be unchanged after re-migrate");
}

async fn table_set(pool: &sqlx::SqlitePool) -> Vec<String> {
    let mut v: Vec<String> = sqlx::query("SELECT name FROM sqlite_master WHERE type = 'table'")
        .fetch_all(pool)
        .await
        .expect("fetch tables")
        .iter()
        .map(|r| r.get::<String, _>(0))
        .collect();
    v.sort();
    v
}
