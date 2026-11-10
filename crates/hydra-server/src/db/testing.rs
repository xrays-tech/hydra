//! Isolated real-database test harness (Phase 2c).
//!
//! Integration tests and the crate's `#[cfg(test)]` modules need a migrated
//! database that nothing else can observe. SQLite's `:memory:` used to provide
//! that for free; the PostgreSQL/MySQL backends need explicit provisioning:
//!
//! * PostgreSQL — a **template database** is created and migrated once per test
//!   process, then every test clones it with `CREATE DATABASE ... TEMPLATE ...`.
//!   Cloning is O(template size) and skips `run_migrate` entirely; the template
//!   itself is tiny because it only contains the baseline schema.
//! * MySQL — a uniquely-named **database** per test. MySQL has no comparable
//!   database template, so each test still creates the database and runs
//!   `run_migrate`.
//!
//! The base URL comes from `HYDRA_TEST_DATABASE_URL`. Provisioning connects with
//! that same URL; the user therefore needs `CREATEDB` (PostgreSQL) or
//! `CREATE DATABASE` (MySQL). PostgreSQL template creation also needs permission
//! to use `template0` (the normal `CREATEDB` grant is enough). For MySQL the URL
//! must name some existing database (any value works; only the server connection
//! is used):
//!
//! ```text
//! postgres://hydra:hydra@127.0.0.1:55432/hydra
//! mysql://hydra:hydra@127.0.0.1:53306/hydra
//! ```
//!
//! [`isolated`] returns a pool that is already migrated (PostgreSQL inherits the
//! template's migrations; MySQL runs them), so a test starts from a clean,
//! migrated database. [`TestDb::cleanup`] closes the pool and, by default,
//! **leaves the database in place**: CI database containers are disposable, and
//! `DROP DATABASE` forces a PostgreSQL checkpoint, which made cleanup more
//! expensive than the provisioning it saved. Set `HYDRA_TEST_DROP_DBS=1` to
//! terminate sessions and drop the per-test database eagerly for local runs.
//! [`setup_pool`] is the convenience form for tests that only need a pool: it
//! always leaks the database.
//!
//! The PostgreSQL template is named `hydra_tmpl_<pid>` and is left in place when
//! the process exits (CI database containers are disposable); a later process
//! that reuses the same PID drops and recreates it. Per-test names are
//! `hydra_test_<pid>_<counter>_<rand>`, unique per call. At template creation,
//! any leftovers with the SAME process prefix (`hydra_test_<pid>_%`) are swept,
//! so a reused PID does not accumulate databases; databases from other live
//! test processes are never touched. There is no cross-test reuse: a test that
//! spawns the real binary must pass its own `TestDb::url` and keep the `TestDb`
//! alive for the process's lifetime.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::db::{self, Pool};

/// A migrated, isolated database plus the URL that reaches it.
///
/// Keep this value alive for as long as the pool (or a spawned binary using
/// [`Self::url`]) is in use. Call [`Self::cleanup`] to drop the database
/// eagerly; dropping the value only closes the pool and leaves the database
/// behind.
pub struct TestDb {
    /// Pool connected to the isolated database.
    pub pool: Pool,
    /// Ready-to-use URL for this test's database.
    pub url: String,
    /// Database name.
    pub name: String,
}

impl TestDb {
    /// Close the pool. By default the database is left behind (leak-by-design,
    /// like [`setup_pool`]: CI containers are disposable and PostgreSQL's
    /// `DROP DATABASE` forces a checkpoint). Set `HYDRA_TEST_DROP_DBS=1` to also
    /// terminate remaining sessions and drop this test's database. Best-effort:
    /// a provisioning/grant failure never masks the test's own result.
    pub async fn cleanup(self) {
        self.pool.close().await;
        if std::env::var("HYDRA_TEST_DROP_DBS")
            .map(|v| v == "1")
            .unwrap_or(false)
        {
            drop_isolated(&self.name).await;
        }
    }
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// The process-wide PostgreSQL template database name.
///
/// `tokio::sync::OnceCell` lets the parallel test threads/closures race to
/// provision it exactly once. It stores only a name, not a live pool: each test
/// runs on its own Tokio runtime, so a shared pool would outlive the runtime
/// that created it.
#[cfg(feature = "db-postgres")]
static PG_TEMPLATE_DB: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();

fn base_url() -> String {
    std::env::var("HYDRA_TEST_DATABASE_URL").unwrap_or_else(|_| {
        panic!(
            "HYDRA_TEST_DATABASE_URL is not set, and this test needs a REAL database \
             (design wave-2 §3: real engine, never a mock).\n  \
             PostgreSQL: export HYDRA_TEST_DATABASE_URL=postgres://hydra:hydra@127.0.0.1:55432/hydra\n  \
             MySQL:      export HYDRA_TEST_DATABASE_URL=mysql://hydra:hydra@127.0.0.1:53306/hydra\n  \
             The user needs CREATEDB (PostgreSQL) or CREATE DATABASE (MySQL)."
        )
    })
}

fn base_url_opt() -> Option<String> {
    std::env::var("HYDRA_TEST_DATABASE_URL").ok()
}

fn unique_name() -> String {
    let pid = std::process::id();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let rand: u64 = rand::random();
    format!("hydra_test_{pid}_{seq}_{rand:016x}")
}

/// Create and migrate a fresh isolated database.
pub async fn isolated() -> TestDb {
    let base = base_url();
    let name = unique_name();

    #[cfg(feature = "db-postgres")]
    {
        pg_isolated(&base, name).await
    }

    #[cfg(feature = "db-mysql")]
    {
        mysql_isolated(&base, name).await
    }

    #[cfg(not(any(feature = "db-postgres", feature = "db-mysql")))]
    {
        let _ = (base, name);
        unreachable!("db::backend emits compile_error! when no backend is selected")
    }
}

/// A migrated pool for tests that only need the database.
///
/// The backing database is intentionally leaked: the URL is not returned, so
/// there is no way to clean it up. CI database containers are disposable.
pub async fn setup_pool() -> Pool {
    isolated().await.pool
}

/// Provision (once per process) the migrated PostgreSQL template and clone it
/// for this test.
#[cfg(feature = "db-postgres")]
async fn pg_isolated(base: &str, name: String) -> TestDb {
    let template = pg_template().await;
    let admin = Pool::connect(base)
        .await
        .unwrap_or_else(|e| panic!("cannot reach the test PostgreSQL at {base}: {e}"));

    // A template can only be copied when no session is connected to it; the
    // template pool was closed after migrating.
    sqlx::query(&format!(
        "CREATE DATABASE \"{name}\" TEMPLATE \"{template}\""
    ))
    .execute(&admin)
    .await
    .unwrap_or_else(|e| {
        panic!(
            "cannot CREATE DATABASE {name} from template {template} (the \
             HYDRA_TEST_DATABASE_URL user needs CREATEDB and permission to use \
             the template database): {e}"
        )
    });
    admin.close().await;

    let url = url_with_database(base, &name);
    let pool = db::init_pool(&url)
        .await
        .unwrap_or_else(|e| panic!("cannot connect to the isolated database {name}: {e}"));
    // No run_migrate here: the clone already has the template's schema and seed
    // rows, including `_sqlx_migrations`.
    TestDb { pool, url, name }
}

/// Create and migrate the process-wide PostgreSQL template database.
#[cfg(feature = "db-postgres")]
async fn pg_template() -> &'static str {
    PG_TEMPLATE_DB
        .get_or_init(|| async {
            let base = base_url();
            let template = format!("hydra_tmpl_{}", std::process::id());

            let admin = Pool::connect(&base)
                .await
                .unwrap_or_else(|e| panic!("cannot reach the test PostgreSQL at {base}: {e}"));

            // A previous process may have had this PID. Sweep only databases
            // with THIS process prefix so a concurrent test binary using a
            // different PID is never disturbed.
            sweep_stale_test_databases(&admin).await;

            // FORCE (PG13+; CI uses PG16) terminates stale sessions so the drop
            // cannot be blocked.
            sqlx::query(&format!(
                "DROP DATABASE IF EXISTS \"{template}\" WITH (FORCE)"
            ))
            .execute(&admin)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "cannot DROP stale PostgreSQL template {template} (the \
                     HYDRA_TEST_DATABASE_URL user needs CREATEDB and must own \
                     the database): {e}"
                )
            });

            sqlx::query(&format!(
                "CREATE DATABASE \"{template}\" TEMPLATE template0"
            ))
            .execute(&admin)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "cannot CREATE DATABASE {template} (the HYDRA_TEST_DATABASE_URL \
                     user needs CREATEDB and permission to use template0): {e}"
                )
            });
            admin.close().await;

            let template_url = url_with_database(&base, &template);
            let pool = db::init_pool(&template_url)
                .await
                .unwrap_or_else(|e| panic!("cannot connect to template {template}: {e}"));
            db::run_migrate(&pool)
                .await
                .expect("run_migrate on the PostgreSQL template database");
            // The template must have no remaining sessions before it can be
            // used by CREATE DATABASE ... TEMPLATE.
            pool.close().await;

            template
        })
        .await
        .as_str()
}

/// Drop per-test databases left by an earlier process that used this PID.
///
/// Only the current process prefix is swept: another test binary in the same
/// suite has a different PID and may be using its own databases right now.
#[cfg(feature = "db-postgres")]
async fn sweep_stale_test_databases(admin: &Pool) {
    let pid = std::process::id();
    let prefix = format!("hydra_test_{pid}_");
    let stale: Vec<(String,)> = sqlx::query_as(
        "SELECT datname::text FROM pg_database \
         WHERE starts_with(datname::text, $1) ORDER BY 1",
    )
    .bind(&prefix)
    .fetch_all(admin)
    .await
    .unwrap_or_else(|e| panic!("cannot list stale PostgreSQL test databases for pid {pid}: {e}"));

    for (name,) in stale {
        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
            .execute(admin)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "cannot DROP stale PostgreSQL test database {name} (the \
                     HYDRA_TEST_DATABASE_URL user needs CREATEDB and must own \
                     the database): {e}"
                )
            });
    }
}

/// Create and migrate a fresh MySQL database (no template equivalent).
#[cfg(feature = "db-mysql")]
async fn mysql_isolated(base: &str, name: String) -> TestDb {
    let admin = Pool::connect(base)
        .await
        .unwrap_or_else(|e| panic!("cannot reach the test MySQL at {base}: {e}"));
    sqlx::query(&format!(
        "CREATE DATABASE `{name}` CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci"
    ))
    .execute(&admin)
    .await
    .unwrap_or_else(|e| {
        panic!(
            "cannot CREATE DATABASE {name} (the HYDRA_TEST_DATABASE_URL user needs the \
             CREATE privilege): {e}"
        )
    });
    admin.close().await;

    let url = url_with_database(base, &name);
    let pool = db::init_pool(&url)
        .await
        .unwrap_or_else(|e| panic!("cannot connect to the isolated database {name}: {e}"));
    db::run_migrate(&pool)
        .await
        .expect("run_migrate on the isolated database");
    TestDb { pool, url, name }
}

/// Replace the database component of a database URL, preserving credentials,
/// host, port and query string.
fn url_with_database(base: &str, dbname: &str) -> String {
    let (before_query, query) = match base.find('?') {
        Some(i) => (&base[..i], Some(&base[i..])),
        None => (base, None),
    };
    let authority_start = before_query.find("://").map_or(0, |i| i + 3);
    let mut url = match before_query[authority_start..].find('/') {
        Some(i) => format!("{}/{}", &before_query[..authority_start + i], dbname),
        None => format!("{before_query}/{dbname}"),
    };
    if let Some(query) = query {
        url.push_str(query);
    }
    url
}

async fn drop_isolated(name: &str) {
    let Some(base) = base_url_opt() else {
        return;
    };
    let Ok(admin) = Pool::connect(&base).await else {
        return;
    };

    #[cfg(feature = "db-postgres")]
    {
        // Terminate leftover sessions (e.g. a spawned binary) before dropping.
        let _ = sqlx::query(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datname = $1 AND pid <> pg_backend_pid()",
        )
        .bind(name)
        .execute(&admin)
        .await;
        let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
            .execute(&admin)
            .await;
    }

    #[cfg(feature = "db-mysql")]
    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS `{name}`"))
        .execute(&admin)
        .await;

    #[cfg(not(any(feature = "db-postgres", feature = "db-mysql")))]
    let _ = name;

    admin.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_replaces_only_the_database() {
        assert_eq!(
            url_with_database("postgres://u:p@h:5432/base", "db1"),
            "postgres://u:p@h:5432/db1"
        );
        assert_eq!(
            url_with_database("postgres://u:p@h:5432/base?sslmode=disable", "db1"),
            "postgres://u:p@h:5432/db1?sslmode=disable"
        );
        assert_eq!(
            url_with_database("postgres://u:p@h", "db1"),
            "postgres://u:p@h/db1"
        );
        assert_eq!(
            url_with_database("mysql://u:p@h:3306/base", "db1"),
            "mysql://u:p@h:3306/db1"
        );
        assert_eq!(
            url_with_database("mysql://u:p@h:3306/base?ssl-mode=disabled", "db1"),
            "mysql://u:p@h:3306/db1?ssl-mode=disabled"
        );
    }

    /// The template path must hand out distinct databases, each with the
    /// baseline tables and seed rows, without re-running migrations.
    ///
    /// This deliberately uses `setup_pool` (which leaks by design) and does not
    /// assert timing: the point is correctness of the clone, not a benchmark.
    #[cfg(feature = "db-postgres")]
    #[tokio::test]
    async fn setup_pool_clones_distinct_migrated_databases() {
        let a = setup_pool().await;
        let b = setup_pool().await;

        let (a_db,): (String,) = sqlx::query_as("SELECT current_database()::text")
            .fetch_one(&a)
            .await
            .expect("read current_database for first pool");
        let (b_db,): (String,) = sqlx::query_as("SELECT current_database()::text")
            .fetch_one(&b)
            .await
            .expect("read current_database for second pool");
        assert_ne!(a_db, b_db, "each setup_pool must get its own database");

        for (label, pool) in [("first", &a), ("second", &b)] {
            let (lock_rows, revision, migrations): (i64, i64, i64) = sqlx::query_as(
                "SELECT \
                    (SELECT COUNT(*) FROM hydra_lock WHERE id = 1), \
                    (SELECT version FROM config_revision WHERE id = 1), \
                    (SELECT COUNT(*) FROM _sqlx_migrations)",
            )
            .fetch_one(pool)
            .await
            .unwrap_or_else(|e| panic!("{label} clone is missing the baseline schema: {e}"));
            assert_eq!(
                lock_rows, 1,
                "{label} clone is missing the hydra_lock seed row"
            );
            assert_eq!(
                revision, 0,
                "{label} clone is missing the config_revision seed row"
            );
            assert!(
                migrations > 0,
                "{label} clone has no applied migrations: the template was not migrated"
            );
        }

        a.close().await;
        b.close().await;
    }
}
