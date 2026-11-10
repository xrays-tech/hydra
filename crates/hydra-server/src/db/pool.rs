//! Pool initialisation, migrations, write transactions, error classification,
//! and the shared config-revision watermark.

use sqlx::migrate::MigrateError;

use super::dialect;
use super::{Db, DbConnection, Pool, Tx};

// ---------------------------------------------------------------------------
// Pool initialisation, migrations, write transactions
// ---------------------------------------------------------------------------

/// Connection budget: `HYDRA_DB_MAX_CONNECTIONS`, default 16.
fn max_connections() -> u32 {
    std::env::var("HYDRA_DB_MAX_CONNECTIONS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(16)
}

/// Connect the selected backend and pin every connection's session timezone to
/// UTC. The shared SQL uses unquoted identifiers throughout, so no session
/// `sql_mode` override is needed on MySQL.
pub async fn init_pool(url: &str) -> Result<Pool, sqlx::Error> {
    let max = max_connections();

    #[cfg(feature = "db-postgres")]
    {
        use sqlx::postgres::PgPoolOptions;
        let pool = PgPoolOptions::new()
            .max_connections(max)
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    sqlx::query(&dialect::sql("SET TIME ZONE 'UTC'"))
                        .execute(&mut *conn)
                        .await?;
                    // A node that loses the DB mid-transaction must not hold
                    // `hydra_lock` until TCP keepalive notices; bound the waits
                    // and the idle-in-transaction window. 60s is far above any
                    // healthy config read/write and still finite on a partition.
                    sqlx::query(&dialect::sql("SET statement_timeout = '60s'"))
                        .execute(&mut *conn)
                        .await?;
                    sqlx::query(&dialect::sql(
                        "SET idle_in_transaction_session_timeout = '60s'",
                    ))
                    .execute(&mut *conn)
                    .await?;
                    Ok(())
                })
            })
            .connect(url)
            .await?;
        Ok(pool)
    }

    #[cfg(feature = "db-mysql")]
    {
        use sqlx::mysql::MySqlPoolOptions;
        let pool = MySqlPoolOptions::new()
            .max_connections(max)
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    sqlx::query(&dialect::sql("SET SESSION time_zone = '+00:00'"))
                        .execute(&mut *conn)
                        .await?;
                    // Same partition guard as PostgreSQL: a lost connection
                    // must not wait forever on a row lock. MySQL's default is
                    // 50s; 10s keeps the cluster-level write stall bounded.
                    sqlx::query(&dialect::sql("SET SESSION innodb_lock_wait_timeout = 10"))
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(url)
            .await?;
        Ok(pool)
    }

    #[cfg(not(any(feature = "db-postgres", feature = "db-mysql")))]
    {
        let _ = (url, max);
        Err(sqlx::Error::Configuration(
            "no database backend selected (enable db-postgres or db-mysql)".into(),
        ))
    }
}

/// Begin a read-only snapshot transaction for the config loader.
///
/// PostgreSQL's default READ COMMITTED gives every statement a fresh snapshot,
/// so the revision and the 13 table reads would not describe one config state.
/// REPEATABLE READ pins them together; it is read-only, so it cannot raise a
/// serialization failure. MySQL's default is already REPEATABLE READ.
#[allow(unused_mut)] // `mut` is only needed by the PostgreSQL branch
pub async fn begin_read_snapshot(pool: &Pool) -> Result<Tx<'static>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    #[cfg(feature = "db-postgres")]
    sqlx::query(&dialect::sql(
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
    ))
    .execute(&mut *tx)
    .await?;
    Ok(tx)
}

/// Run the embedded migrations for the selected backend.
pub async fn run_migrate(pool: &Pool) -> Result<(), MigrateError> {
    #[cfg(feature = "db-postgres")]
    {
        sqlx::migrate!("./migrations/postgres").run(pool).await
    }
    #[cfg(feature = "db-mysql")]
    {
        sqlx::migrate!("./migrations/mysql").run(pool).await
    }
    #[cfg(not(any(feature = "db-postgres", feature = "db-mysql")))]
    {
        let _ = pool;
        Ok(())
    }
}

/// Begin a write transaction: begin, idempotently seed the single lock row, and
/// take `SELECT ... FOR UPDATE` on it. Every read-before-write transaction uses
/// this primitive so writers serialize on the same row across nodes.
pub async fn begin_write(pool: &Pool) -> Result<Tx<'static>, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Bound the wait for `hydra_lock`/row locks, but ONLY in write
    // transactions: a session-wide lock_timeout also applies to sqlx's
    // migration advisory lock and makes concurrent schema provisioning fail
    // with 55P03. `SET LOCAL` is transaction-scoped and leaves migrations alone.
    #[cfg(feature = "db-postgres")]
    sqlx::query(&dialect::sql("SET LOCAL lock_timeout = '5s'"))
        .execute(&mut *tx)
        .await?;

    #[cfg(feature = "db-postgres")]
    sqlx::query(&dialect::sql(
        "INSERT INTO hydra_lock (id) VALUES (1) ON CONFLICT (id) DO NOTHING",
    ))
    .execute(&mut *tx)
    .await?;
    #[cfg(feature = "db-mysql")]
    sqlx::query(&dialect::sql(
        "INSERT INTO hydra_lock (id) VALUES (1) ON DUPLICATE KEY UPDATE id = id",
    ))
    .execute(&mut *tx)
    .await?;

    sqlx::query(&dialect::sql(
        "SELECT id FROM hydra_lock WHERE id = 1 FOR UPDATE",
    ))
    .execute(&mut *tx)
    .await?;
    Ok(tx)
}

/// True when the DB error means the backend is UNAVAILABLE, not that the
/// request was invalid: the connection failed, the pool could not hand out a
/// connection in time, TLS failed, or the pool/configuration was never usable.
///
/// This is the transport half of the classification. It deliberately excludes
/// `Database` errors (a live server answered with a constraint/serialization
/// error) and `Protocol`/decode errors (a live connection produced bad data).
/// Callers map this to a retryable `503 database_unavailable`, never a `500`:
/// during a shared-DB partition the request may succeed unchanged after
/// reconnecting.
pub fn is_unavailable(e: &sqlx::Error) -> bool {
    matches!(
        e,
        sqlx::Error::Io(_)
            | sqlx::Error::PoolTimedOut
            | sqlx::Error::PoolClosed
            | sqlx::Error::Tls(_)
            | sqlx::Error::Configuration(_)
    )
}

/// True when the DB error is a transient serialization/lock failure that the
/// caller may retry: PostgreSQL SQLSTATE `40001`/`40P01`, MySQL `1213`/`1205`.
pub fn is_retryable(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .and_then(|db| db.code())
        .map(|code| retryable_code(code.as_ref()))
        .unwrap_or(false)
}

#[cfg(feature = "db-postgres")]
fn retryable_code(code: &str) -> bool {
    matches!(code, "40001" | "40P01")
}

#[cfg(feature = "db-mysql")]
fn retryable_code(code: &str) -> bool {
    matches!(code, "1213" | "1205")
}

#[cfg(not(any(feature = "db-postgres", feature = "db-mysql")))]
fn retryable_code(_code: &str) -> bool {
    false
}

// ---------------------------------------------------------------------------
// Config version metadata (shared-DB config watermark)
// ---------------------------------------------------------------------------
//
// `config_revision` holds the shared config watermark: every mutating writer
// bumps it in the same transaction, and both the pub/sub fanout and the poll
// fallback use it to decide whether a reload is needed. These statements use
// runtime-checked `sqlx::query` (like the rest of the repo): no compile-time
// query macros, so no per-dialect offline cache.

/// The stored config watermark, in three states — see [`ConfigVersion`] (this
/// replaced an `Option<u64>` that could not tell "never set" from "corrupt").
///
/// `Absent` (no row) and `Corrupt` (a row whose value is not a number) are
/// different facts for an operator: a brand-new DB legitimately has no watermark
/// yet, while a corrupt row is a hand edit / partial import that must be named in
/// the boot log. Collapsing them — which a bare `value.parse().ok()` did — hid
/// the corruption.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigVersion {
    /// No marker row at all: a brand-new DB, or one predating migration 0008.
    Absent,
    /// A parsed watermark.
    Value(u64),
    /// A row exists but its value is not a `u64` (hand edit, partial import,
    /// truncated restore). Carries the raw text so the warning can quote what an
    /// operator has to look at.
    Corrupt(String),
}

/// Read the config watermark ([`ConfigVersion`] — see why it is three-state).
pub async fn get_config_version(pool: &Pool) -> Result<ConfigVersion, sqlx::Error> {
    let row: Option<(i64,)> = sqlx::query_as(&dialect::sql(
        "SELECT version FROM config_revision WHERE id = 1",
    ))
    .fetch_optional(pool)
    .await?;
    Ok(match row {
        None => ConfigVersion::Absent,
        Some((v,)) if v >= 0 => ConfigVersion::Value(v as u64),
        Some((v,)) => ConfigVersion::Corrupt(v.to_string()),
    })
}

/// Read the shared revision on an arbitrary executor — used by the loader to
/// pair the version with the row snapshot inside ONE read transaction.
pub(crate) async fn read_revision_on<'e, E>(exec: E) -> Result<u64, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Db>,
{
    let row: Option<(i64,)> = sqlx::query_as(&dialect::sql(
        "SELECT version FROM config_revision WHERE id = 1",
    ))
    .fetch_optional(exec)
    .await?;
    Ok(row.map(|(v,)| v.max(0) as u64).unwrap_or(0))
}

/// Cheap revision probe for the Phase 3b poll fallback.
///
/// ONE `SELECT` against the pool: no row rebuild, no decryption, no snapshot
/// construction. The poller compares this with [`crate::store::ConfigStore::version`]
/// and only then asks the store for a paired-read reload.
pub async fn read_revision(pool: &Pool) -> Result<u64, sqlx::Error> {
    read_revision_on(pool).await
}

/// Bump the shared revision on an ALREADY-OPEN write transaction. Every mutator
/// that actually changed a row calls this before committing.
pub(crate) async fn bump_config_revision_on_conn(
    conn: &mut DbConnection,
) -> Result<u64, sqlx::Error> {
    sqlx::query(&dialect::sql(
        "UPDATE config_revision SET version = version + 1 WHERE id = 1",
    ))
    .execute(&mut *conn)
    .await?;
    let row: (i64,) = sqlx::query_as(&dialect::sql(
        "SELECT version FROM config_revision WHERE id = 1",
    ))
    .fetch_one(&mut *conn)
    .await?;
    Ok(row.0 as u64)
}

/// Bump the shared config revision in its own write transaction (lock + bump +
/// commit). Used by `force` reloads; ordinary mutators bump inside their own
/// transaction via [`bump_config_revision_on_conn`].
pub async fn bump_config_revision(pool: &Pool) -> Result<u64, sqlx::Error> {
    #[cfg(test)]
    if test_fault::persist_should_fail(pool).await {
        return Err(sqlx::Error::Protocol(
            "injected: config_revision persist failed".into(),
        ));
    }
    let mut tx = begin_write(pool).await?;
    let version = bump_config_revision_on_conn(&mut tx).await?;
    tx.commit().await?;
    Ok(version)
}

/// Commit a write transaction, bumping the shared revision first iff the
/// mutation changed at least one row. Inserts pass `true`; updates/deletes pass
/// `rows_affected() > 0` so a no-op does not create a reload storm.
pub(crate) async fn commit_write(mut tx: Tx<'static>, changed: bool) -> Result<(), sqlx::Error> {
    if changed {
        bump_config_revision_on_conn(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Persist an ABSOLUTE config version (row 1) under the same lock discipline as
/// every other write. This is the revision itself, so it does not add a second
/// bump.
pub async fn set_config_version(pool: &Pool, version: u64) -> Result<(), sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    sqlx::query(&dialect::sql(
        "UPDATE config_revision SET version = ? WHERE id = 1",
    ))
    .bind(version as i64)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// A migrated isolated database pool, for unit tests inside this crate.
///
/// Kept as a compatibility shim for inline test modules that predate the
/// `testing` helper; new tests should call [`crate::db::testing::setup_pool`] directly.
#[cfg(test)]
pub(crate) async fn test_pool() -> Pool {
    crate::db::testing::setup_pool().await
}

/// Deterministic, backend-agnostic failure injection for the store's
/// persist-before-swap tests.
///
/// The old tests installed a SQLite trigger to abort the marker write. That
/// mechanism does not exist on PostgreSQL/MySQL, so the same property is pinned
/// by forcing [`bump_config_revision`] to fail before it opens a transaction.
///
/// The switch is scoped to the caller's database, not process-wide: tests run
/// in parallel and each owns a separate database, so a global flag made one
/// test's forced failure abort another test's reload. The guard removes only its
/// own entry even when an assertion panics.
#[cfg(test)]
pub(crate) mod test_fault {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};

    use super::Pool;

    fn failing_databases() -> &'static Mutex<HashSet<String>> {
        static FAILING: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
        FAILING.get_or_init(|| Mutex::new(HashSet::new()))
    }

    async fn current_database(pool: &Pool) -> Option<String> {
        #[cfg(feature = "db-postgres")]
        const SQL: &str = "SELECT current_database()::text";
        #[cfg(feature = "db-mysql")]
        const SQL: &str = "SELECT DATABASE()";
        #[cfg(not(any(feature = "db-postgres", feature = "db-mysql")))]
        const SQL: &str = "SELECT NULL::text";

        sqlx::query_as::<_, (Option<String>,)>(SQL)
            .fetch_one(pool)
            .await
            .ok()
            .and_then(|row| row.0)
    }

    pub(crate) async fn persist_should_fail(pool: &Pool) -> bool {
        let empty = failing_databases()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty();
        if empty {
            return false;
        }
        let Some(db) = current_database(pool).await else {
            return false;
        };
        failing_databases()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&db)
    }

    /// RAII switch: failures are injected for one database until the guard drops.
    pub(crate) struct FailPersistGuard {
        db: String,
    }

    impl Drop for FailPersistGuard {
        fn drop(&mut self) {
            failing_databases()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.db);
        }
    }

    pub(crate) async fn fail_persist(pool: &Pool) -> FailPersistGuard {
        let db = current_database(pool)
            .await
            .expect("test_fault: could not identify the pool's database");
        failing_databases()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(db.clone());
        FailPersistGuard { db }
    }
}

#[cfg(test)]
mod tests {
    use super::is_unavailable;

    #[test]
    fn unavailable_covers_connection_and_pool_failures() {
        let io = sqlx::Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "connection refused",
        ));
        for error in [
            io,
            sqlx::Error::PoolTimedOut,
            sqlx::Error::PoolClosed,
            sqlx::Error::Tls("handshake failed".into()),
            sqlx::Error::Configuration("bad database url".into()),
        ] {
            assert!(
                is_unavailable(&error),
                "{error:?} must classify as DB-unavailable"
            );
        }
    }

    #[test]
    fn unavailable_excludes_request_and_data_errors() {
        for error in [
            sqlx::Error::RowNotFound,
            sqlx::Error::Protocol("unexpected packet".into()),
            sqlx::Error::InvalidArgument("bad bind".into()),
            sqlx::Error::ColumnNotFound("missing".into()),
        ] {
            assert!(
                !is_unavailable(&error),
                "{error:?} is not a transport outage"
            );
        }
    }
}
