//! Compile-time database backend selection.
//!
//! The shared-DB architecture uses exactly one relational backend, chosen by a
//! Cargo feature:
//!
//! * `db-postgres` → [`sqlx::Postgres`]
//! * `db-mysql` → [`sqlx::MySql`]
//!
//! **Every build/test recipe must select exactly one backend.** `server` does
//! not imply one; selecting neither or both is a compile error. The aliases
//! below are the only place the server source names a concrete sqlx driver.
//!
//! Pool construction, migrations, session settings, transaction helpers, error
//! classification and the portable query layer live in `db.rs` and `db::dialect`.

#[cfg(not(any(feature = "db-postgres", feature = "db-mysql")))]
compile_error!("no database backend selected: enable exactly one of `db-postgres` or `db-mysql`");

#[cfg(all(feature = "db-postgres", feature = "db-mysql"))]
compile_error!(
    "features `db-postgres` and `db-mysql` are mutually exclusive; enable exactly one backend"
);

/// The selected sqlx database driver.
#[cfg(all(feature = "db-postgres", not(feature = "db-mysql")))]
pub type Db = sqlx::Postgres;

/// The selected sqlx database driver.
#[cfg(all(feature = "db-mysql", not(feature = "db-postgres")))]
pub type Db = sqlx::MySql;

/// A pool for the selected backend.
pub type Pool = sqlx::Pool<Db>;

/// A transaction for the selected backend.
pub type Tx<'a> = sqlx::Transaction<'a, Db>;

/// The selected backend's connection type.
pub type DbConnection = <Db as sqlx::Database>::Connection;

#[cfg(test)]
mod tests {
    /// The alias must resolve to a real sqlx `Database` in every feature mode.
    #[test]
    fn db_alias_is_a_database() {
        fn assert_database<D: sqlx::Database>() {}
        assert_database::<super::Db>();
    }
}
