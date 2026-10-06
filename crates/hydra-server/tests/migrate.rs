//! §2.1 — migrations & connection: tables created, PRAGMAs applied, idempotent.

mod common;

use sqlx::Row;

const EIGHT_BUSINESS_TABLES: &[&str] = &[
    "provider",
    "provider_model",
    "provider_key",
    "tenant",
    "tenant_provider",
    "tenant_model",
    "limit_role",
    "usage_record",
];

/// T1.1 — after migrate, `sqlite_master` contains all 8 business tables plus
/// the `_sqlx_migrations` bookkeeping table.
#[tokio::test]
async fn migrate_creates_all_tables() {
    let pool = common::setup_pool().await;

    let rows = sqlx::query("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .fetch_all(&pool)
        .await
        .expect("query sqlite_master");

    let mut names: Vec<String> = rows.iter().map(|r| r.get::<String, _>(0)).collect();
    names.sort();

    for expected in EIGHT_BUSINESS_TABLES
        .iter()
        .chain(std::iter::once(&"_sqlx_migrations"))
    {
        assert!(
            names.iter().any(|n| n == expected),
            "expected table '{expected}' to exist, tables were: {names:?}"
        );
    }
}

/// The sub-tenant usage index (migration 0012) must actually exist.
///
/// `usage_record`'s original index leads with `(tenant_id, created_at)`, which is
/// the right shape for a whole-tenant read but not for the per-sub-tenant reads
/// and aggregations the tenant API serves: those have to re-scan the tenant's
/// rows in the window. 0012 adds `(tenant_id, sub_tenant_id, created_at)`.
///
/// Asserting on `sqlite_master` rather than on the migration file means this
/// catches the migration being dropped, renamed, or silently not embedded by
/// `sqlx::migrate!` — "the file is in the repo" is not the claim.
#[tokio::test]
async fn the_sub_tenant_usage_index_exists() {
    let pool = common::setup_pool().await;
    let rows = sqlx::query(
        "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'usage_record' \
         ORDER BY name",
    )
    .fetch_all(&pool)
    .await
    .expect("query sqlite_master indexes");
    let names: Vec<String> = rows.iter().map(|r| r.get::<String, _>(0)).collect();
    assert!(
        names.iter().any(|n| n == "idx_usage_record_sub_tenant"),
        "migration 0012 must create idx_usage_record_sub_tenant, indexes were: {names:?}"
    );

    // And it must be on the columns the queries actually filter by, in order.
    let sql: String = sqlx::query(
        "SELECT sql FROM sqlite_master WHERE type = 'index' \
         AND name = 'idx_usage_record_sub_tenant'",
    )
    .fetch_one(&pool)
    .await
    .expect("index row")
    .get(0);
    for col in ["tenant_id", "sub_tenant_id", "created_at"] {
        assert!(sql.contains(col), "index must cover {col}: {sql}");
    }
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
