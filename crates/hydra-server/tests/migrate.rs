//! §2.1 — migrations & connection: tables created, seeds present, idempotent.
//!
//! Backend-agnostic: the metadata queries below are the only place the selected
//! PostgreSQL/MySQL catalog is named. Everything else runs through the portable
//! migration set.

mod common;

use hydra_server::db::{self, Pool};
use sqlx::Row;

/// Every table the greenfield baseline creates, plus sqlx's bookkeeping table.
const EXPECTED_TABLES: &[&str] = &[
    "provider",
    "provider_model",
    "provider_key",
    "tenant",
    "tenant_provider",
    "tenant_model",
    "limit_role",
    "user_group",
    "hydra_user",
    "user_api_key",
    "tenant_access_token",
    "global_role",
    "global_role_provider",
    "hydra_lock",
    "config_revision",
    "_sqlx_migrations",
];

/// The table the retired SQLite usage store used. The golden baseline never
/// recreates it; its ABSENCE is the executable form of ADR-0002 D-3.
const DROPPED_USAGE_TABLE: &str = "usage_record";

/// The tables the connected schema/database actually contains.
#[cfg(feature = "db-postgres")]
async fn table_names(pool: &Pool) -> Vec<String> {
    let rows = sqlx::query(
        "SELECT tablename FROM pg_tables WHERE schemaname = current_schema() ORDER BY tablename",
    )
    .fetch_all(pool)
    .await
    .expect("query pg_tables");

    let mut names: Vec<String> = rows.iter().map(|r| r.get::<String, _>(0)).collect();
    names.sort();
    names
}

/// The tables the connected schema/database actually contains.
///
/// MySQL's `information_schema` string columns decode as `VARBINARY` through
/// sqlx; convert the bytes explicitly.
#[cfg(feature = "db-mysql")]
async fn table_names(pool: &Pool) -> Vec<String> {
    let rows = sqlx::query(
        "SELECT table_name FROM information_schema.tables \
         WHERE table_schema = DATABASE() ORDER BY table_name",
    )
    .fetch_all(pool)
    .await
    .expect("query information_schema.tables");

    let mut names: Vec<String> = rows
        .iter()
        .map(|r| String::from_utf8_lossy(&r.get::<Vec<u8>, _>(0)).into_owned())
        .collect();
    names.sort();
    names
}

/// T1.1 — after migrate every baseline table exists, and the retired usage
/// table does not.
#[tokio::test]
async fn migrate_creates_all_tables() {
    let pool = common::setup_pool().await;
    let names = table_names(&pool).await;

    for expected in EXPECTED_TABLES {
        assert!(
            names.iter().any(|n| n == expected),
            "expected table '{expected}' to exist, tables were: {names:?}"
        );
    }
    assert!(
        !names.iter().any(|n| n == DROPPED_USAGE_TABLE),
        "the retired {DROPPED_USAGE_TABLE} table must not be recreated, tables were: {names:?}"
    );
}

/// The baseline seeds exactly one lock row and one revision row.
#[tokio::test]
async fn migrate_seeds_lock_and_revision() {
    let pool = common::setup_pool().await;

    let lock_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM hydra_lock WHERE id = 1")
        .fetch_one(&pool)
        .await
        .expect("count hydra_lock");
    assert_eq!(lock_rows.0, 1, "hydra_lock row 1 must be seeded");

    let revision: (i64,) = sqlx::query_as("SELECT version FROM config_revision WHERE id = 1")
        .fetch_one(&pool)
        .await
        .expect("read config_revision");
    assert_eq!(revision.0, 0, "a fresh database starts at revision 0");
}

/// T3.1 — running migrate twice does not error and does not change the schema.
#[tokio::test]
async fn migrate_idempotent() {
    let pool = common::setup_pool().await;

    let before = table_names(&pool).await;

    db::run_migrate(&pool)
        .await
        .expect("re-running migrate must be idempotent");

    let after = table_names(&pool).await;
    assert_eq!(before, after, "schema must be unchanged after re-migrate");
}
