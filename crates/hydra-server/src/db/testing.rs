//! Isolated real-database test harness (Phase 2c).
//!
//! Integration tests and the crate's `#[cfg(test)]` modules need a migrated
//! database that nothing else can observe. SQLite's `:memory:` used to provide
//! that for free; the PostgreSQL/MySQL backends need explicit provisioning:
//!
//! * PostgreSQL — a uniquely-named **schema** in the base database. The per-test
//!   URL pins `search_path` to it through the startup `options` parameter, so
//!   the unqualified migration statements land in the isolated schema.
//! * MySQL — a uniquely-named **database**. The per-test URL points at it.
//!
//! The base URL comes from `HYDRA_TEST_DATABASE_URL`. Provisioning connects with
//! that same URL; the user therefore needs `CREATE SCHEMA` (PostgreSQL) or
//! `CREATE DATABASE` (MySQL). For MySQL the URL must name some existing
//! database (any value works; only the server connection is used):
//!
//! ```text
//! postgres://hydra:hydra@127.0.0.1:55432/hydra
//! mysql://hydra:hydra@127.0.0.1:53306/hydra
//! ```
//!
//! [`isolated`] runs `run_migrate` before returning, so a test starts from a
//! clean, migrated database. [`TestDb::cleanup`] closes the pool and drops the
//! schema/database best-effort. [`setup_pool`] is the convenience form for tests
//! that only need a pool: it intentionally **leaks** the schema/database, which
//! is fine because CI's database containers are disposable.
//!
//! Names are `hydra_test_<pid>_<counter>_<rand>`, unique per call. There is no
//! cross-test reuse: a test that spawns the real binary must pass its own
//! `TestDb::url` and keep the `TestDb` alive for the process's lifetime.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::db::{self, Pool};

/// A migrated, isolated database plus the URL that reaches it.
///
/// Keep this value alive for as long as the pool (or a spawned binary using
/// [`Self::url`]) is in use. Call [`Self::cleanup`] to drop the schema/database
/// eagerly; dropping the value only closes nothing and leaves the empty
/// schema/database behind.
pub struct TestDb {
    /// Pool connected to the isolated schema/database.
    pub pool: Pool,
    /// Ready-to-use URL for this test's schema/database.
    pub url: String,
    /// Schema (PostgreSQL) or database (MySQL) name.
    pub name: String,
}

impl TestDb {
    /// Close the pool and drop this test's schema/database. Best-effort: a
    /// provisioning/grant failure never masks the test's own result.
    pub async fn cleanup(self) {
        self.pool.close().await;
        drop_isolated(&self.name).await;
    }
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn base_url() -> String {
    std::env::var("HYDRA_TEST_DATABASE_URL").unwrap_or_else(|_| {
        panic!(
            "HYDRA_TEST_DATABASE_URL is not set, and this test needs a REAL database \
             (design wave-2 §3: real engine, never a mock).\n  \
             PostgreSQL: export HYDRA_TEST_DATABASE_URL=postgres://hydra:hydra@127.0.0.1:55432/hydra\n  \
             MySQL:      export HYDRA_TEST_DATABASE_URL=mysql://hydra:hydra@127.0.0.1:53306/hydra\n  \
             The user needs CREATE SCHEMA (PostgreSQL) or CREATE DATABASE (MySQL)."
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
        let admin = Pool::connect(&base)
            .await
            .unwrap_or_else(|e| panic!("cannot reach the test PostgreSQL at {base}: {e}"));
        sqlx::query(&format!("CREATE SCHEMA \"{name}\""))
            .execute(&admin)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "cannot CREATE SCHEMA {name} (the HYDRA_TEST_DATABASE_URL user needs \
                     CREATE on the database): {e}"
                )
            });
        admin.close().await;

        let url = pg_url_with_schema(&base, &name);
        let pool = db::init_pool(&url)
            .await
            .unwrap_or_else(|e| panic!("cannot connect to the isolated schema {name}: {e}"));
        db::run_migrate(&pool)
            .await
            .expect("run_migrate on the isolated schema");
        TestDb { pool, url, name }
    }

    #[cfg(feature = "db-mysql")]
    {
        let admin = Pool::connect(&base)
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

        let url = mysql_url_with_db(&base, &name);
        let pool = db::init_pool(&url)
            .await
            .unwrap_or_else(|e| panic!("cannot connect to the isolated database {name}: {e}"));
        db::run_migrate(&pool)
            .await
            .expect("run_migrate on the isolated database");
        TestDb { pool, url, name }
    }

    #[cfg(not(any(feature = "db-postgres", feature = "db-mysql")))]
    {
        let _ = (base, name);
        unreachable!("db::backend emits compile_error! when no backend is selected")
    }
}

/// A migrated pool for tests that only need the database.
///
/// The backing schema/database is intentionally leaked: the URL is not returned,
/// so there is no way to clean it up. CI database containers are disposable.
pub async fn setup_pool() -> Pool {
    isolated().await.pool
}

/// Base URL plus a `search_path` option pinning the session to `schema`.
///
/// PostgreSQL's URI form passes `options` to the server as command-line flags;
/// `-csearch_path=<schema>` is the documented equivalent of setting
/// `search_path` on connect.
#[cfg(feature = "db-postgres")]
fn pg_url_with_schema(base: &str, schema: &str) -> String {
    let sep = if base.contains('?') { '&' } else { '?' };
    format!("{base}{sep}options=-csearch_path%3D{schema}")
}

/// Replace the database component of a MySQL URL, preserving credentials, host,
/// port and query string.
#[cfg(feature = "db-mysql")]
fn mysql_url_with_db(base: &str, dbname: &str) -> String {
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
    let _ = sqlx::query(&format!("DROP SCHEMA IF EXISTS \"{name}\" CASCADE"))
        .execute(&admin)
        .await;

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

    #[cfg(feature = "db-postgres")]
    #[test]
    fn postgres_url_appends_search_path_option() {
        let url = pg_url_with_schema("postgres://u:p@h/db", "s1");
        assert_eq!(url, "postgres://u:p@h/db?options=-csearch_path%3Ds1");
        let url = pg_url_with_schema("postgres://u:p@h/db?sslmode=disable", "s1");
        assert_eq!(
            url,
            "postgres://u:p@h/db?sslmode=disable&options=-csearch_path%3Ds1"
        );
    }

    #[cfg(feature = "db-mysql")]
    #[test]
    fn mysql_url_replaces_only_the_database() {
        assert_eq!(
            mysql_url_with_db("mysql://u:p@h:3306/base", "db1"),
            "mysql://u:p@h:3306/db1"
        );
        assert_eq!(
            mysql_url_with_db("mysql://u:p@h:3306/base?ssl-mode=disabled", "db1"),
            "mysql://u:p@h:3306/db1?ssl-mode=disabled"
        );
        assert_eq!(
            mysql_url_with_db("mysql://u:p@h", "db1"),
            "mysql://u:p@h/db1"
        );
    }
}
