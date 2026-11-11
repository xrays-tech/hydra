//! Pool initialisation, migrations, write transactions, error classification,
//! and the shared config-revision watermark.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

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

// ---------------------------------------------------------------------------
// Env-tunable session/transaction guardrails (F4)
// ---------------------------------------------------------------------------
//
// The values below were hard-coded literals in `init_pool`/`begin_write`. They
// are now parsed from the environment, but the PRODUCTION DEFAULTS ARE
// UNCHANGED, so an existing deployment that sets nothing keeps the exact
// behaviour it had before.

/// Smallest accepted timeout. `0` would DISABLE PostgreSQL's timeout (wait
/// forever) — the very partition/hang these guardrails exist to remove — so it
/// is clamped up to a finite value rather than honoured.
const MIN_TIMEOUT_MS: u64 = 1;
/// Largest accepted timeout: one hour. Anything longer is operationally
/// indistinguishable from the unbounded wait this guardrail removes.
const MAX_TIMEOUT_MS: u64 = 60 * 60 * 1_000;
const MIN_LOCK_WAIT_SECS: u64 = 1;
const MAX_LOCK_WAIT_SECS: u64 = 60 * 60;

/// The bounded session/transaction guardrails, parsed and clamped.
///
/// Fields mirror the four environment variables named in [`DbGuardrails::from_env`].
/// Parsing is kept in one place so the clamps and the
/// `lock_timeout <= statement_timeout` sanity rule cannot drift between
/// `init_pool` and `begin_write`.
///
/// `pub` (with a `#[doc(hidden)]` marker) only so the live retry tests in
/// `tests/db_write_lock.rs` can build a value through [`DbGuardrails::from_lookup`]
/// and hand it to [`begin_write_with_guardrails`] without racing the shared
/// process environment. The fields stay private.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DbGuardrails {
    lock_timeout_ms: u64,
    statement_timeout_ms: u64,
    idle_tx_timeout_ms: u64,
    lock_wait_timeout_secs: u64,
}

impl DbGuardrails {
    /// Production defaults: the previous hard-coded literals, verbatim.
    const DEFAULT_LOCK_TIMEOUT_MS: u64 = 5_000;
    const DEFAULT_STATEMENT_TIMEOUT_MS: u64 = 60_000;
    const DEFAULT_IDLE_TX_TIMEOUT_MS: u64 = 60_000;
    const DEFAULT_LOCK_WAIT_TIMEOUT_SECS: u64 = 10;

    /// Parse the guardrails from the process environment.
    ///
    /// `begin_write` calls this per acquisition: four cheap lookups next to a
    /// network round-trip, and it lets a test/runtime override take effect.
    /// `init_pool` calls it once for the session-wide settings.
    fn from_env() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Pure form of [`Self::from_env`]: the raw strings come from `lookup`, so
    /// parsing, clamping and the sanity rule are unit-testable without touching
    /// the process environment (which the parallel tests share).
    ///
    /// * unset, blank, non-numeric or negative → the documented default;
    /// * in range → used as-is;
    /// * below/above the clamp → clamped to the nearest bound;
    /// * `lock_timeout > statement_timeout` → the lock timeout is clamped down
    ///   to the statement timeout and a single warning is logged.
    #[doc(hidden)]
    pub fn from_lookup(mut lookup: impl FnMut(&str) -> Option<String>) -> Self {
        let parse =
            |lookup: &mut dyn FnMut(&str) -> Option<String>, key: &str, default: u64, max: u64| {
                lookup(key)
                    .as_deref()
                    .map(str::trim)
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(|v| v.clamp(MIN_TIMEOUT_MS, max))
                    .unwrap_or(default)
            };

        let mut guardrails = Self {
            lock_timeout_ms: parse(
                &mut lookup,
                "HYDRA_DB_LOCK_TIMEOUT_MS",
                Self::DEFAULT_LOCK_TIMEOUT_MS,
                MAX_TIMEOUT_MS,
            ),
            statement_timeout_ms: parse(
                &mut lookup,
                "HYDRA_DB_STATEMENT_TIMEOUT_MS",
                Self::DEFAULT_STATEMENT_TIMEOUT_MS,
                MAX_TIMEOUT_MS,
            ),
            idle_tx_timeout_ms: parse(
                &mut lookup,
                "HYDRA_DB_IDLE_TX_TIMEOUT_MS",
                Self::DEFAULT_IDLE_TX_TIMEOUT_MS,
                MAX_TIMEOUT_MS,
            ),
            lock_wait_timeout_secs: lookup("HYDRA_DB_LOCK_WAIT_TIMEOUT_SECS")
                .as_deref()
                .map(str::trim)
                .and_then(|v| v.parse::<u64>().ok())
                .map(|v| v.clamp(MIN_LOCK_WAIT_SECS, MAX_LOCK_WAIT_SECS))
                .unwrap_or(Self::DEFAULT_LOCK_WAIT_TIMEOUT_SECS),
        };

        // A statement timeout below the lock timeout is a contradiction: the
        // statement would be cancelled while still waiting for the lock, so the
        // lock timeout could never fire. Fold the lock timeout down to keep the
        // pair consistent and warn exactly once.
        if guardrails.lock_timeout_ms > guardrails.statement_timeout_ms {
            guardrails.lock_timeout_ms = guardrails.statement_timeout_ms;
            lock_timeout_sanity_warning();
        }

        guardrails
    }
}

/// Log the `LOCK ≤ STATEMENT` clamp once per process (not once per write).
fn lock_timeout_sanity_warning() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            "HYDRA_DB_LOCK_TIMEOUT_MS exceeds HYDRA_DB_STATEMENT_TIMEOUT_MS; \
             clamping the lock timeout down to the statement timeout so the \
             statement is not cancelled while still waiting for the lock"
        );
    });
}

/// Split a database URL into its scheme and its query string (without the
/// `#fragment`). `None` when the URL has no `scheme://` or no `?query`.
fn scheme_and_query(url: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    let query = rest.split_once('?')?.1;
    let query = query.split_once('#').map_or(query, |(before, _)| before);
    Some((scheme, query))
}

/// Every value attached to one of `keys` in a query string. Keys are matched
/// case-insensitively (a human-written URL, not a strict parser).
fn query_values<'a>(query: &'a str, keys: &[&str]) -> Vec<&'a str> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(key, _)| keys.iter().any(|want| key.eq_ignore_ascii_case(want)))
        .map(|(_, value)| value)
        .collect()
}

/// Does `url` ask the database driver for an **encrypted and server-authenticated**
/// transport? This is the pure predicate behind `HYDRA_DB_REQUIRE_TLS`.
///
/// - PostgreSQL (`postgres://` / `postgresql://`): `sslmode` must be
///   `require`, `verify-ca`, or `verify-full`;
/// - MySQL (`mysql://`): `ssl-mode` **or** `sslmode` (both spellings are
///   accepted) must be `REQUIRED`, `VERIFY_CA`, or `VERIFY_IDENTITY`.
///
/// Keys and values are matched case-insensitively. When a recognised key occurs
/// more than once, **every** occurrence must be acceptable: a weak duplicate
/// must not override a strict one, because URL parsers disagree on which
/// duplicate wins. A missing query/key, an unknown scheme, or a weaker mode is
/// `false`.
///
/// Kept free of the process environment and of a live connection so the boot
/// path can call it before [`init_pool`] (including for the compiled-in
/// `DEFAULT_DB_URL`), and so the accept/reject matrix is unit-testable without a
/// database.
#[must_use]
pub fn db_url_satisfies_tls(url: &str) -> bool {
    let Some((scheme, query)) = scheme_and_query(url) else {
        return false;
    };
    match scheme.to_ascii_lowercase().as_str() {
        "postgres" | "postgresql" => {
            let values = query_values(query, &["sslmode"]);
            !values.is_empty()
                && values.iter().all(|value| {
                    matches!(
                        value.trim().to_ascii_lowercase().as_str(),
                        "require" | "verify-ca" | "verify-full"
                    )
                })
        }
        "mysql" => {
            let values = query_values(query, &["ssl-mode", "sslmode"]);
            !values.is_empty()
                && values.iter().all(|value| {
                    matches!(
                        value.trim().to_ascii_lowercase().as_str(),
                        "required" | "verify_ca" | "verify_identity"
                    )
                })
        }
        _ => false,
    }
}

/// Connect the selected backend and pin every connection's session timezone to
/// UTC. The shared SQL uses unquoted identifiers throughout, so no session
/// `sql_mode` override is needed on MySQL.
pub async fn init_pool(url: &str) -> Result<Pool, sqlx::Error> {
    let max = max_connections();
    let guardrails = DbGuardrails::from_env();

    #[cfg(feature = "db-postgres")]
    {
        use sqlx::postgres::PgPoolOptions;
        let statement_ms = guardrails.statement_timeout_ms;
        let idle_tx_ms = guardrails.idle_tx_timeout_ms;
        let pool = PgPoolOptions::new()
            .max_connections(max)
            .after_connect(move |conn, _meta| {
                Box::pin(async move {
                    sqlx::query(&dialect::sql("SET TIME ZONE 'UTC'"))
                        .execute(&mut *conn)
                        .await?;
                    // A node that loses the DB mid-transaction must not hold
                    // `hydra_lock` until TCP keepalive notices; bound the waits
                    // and the idle-in-transaction window. The defaults (60s) are
                    // far above any healthy config read/write and still finite
                    // on a partition.
                    let statement = format!("SET statement_timeout = '{statement_ms}ms'");
                    sqlx::query(&dialect::sql(&statement))
                        .execute(&mut *conn)
                        .await?;
                    let idle =
                        format!("SET idle_in_transaction_session_timeout = '{idle_tx_ms}ms'");
                    sqlx::query(&dialect::sql(&idle))
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
        let lock_wait_secs = guardrails.lock_wait_timeout_secs;
        let pool = MySqlPoolOptions::new()
            .max_connections(max)
            .after_connect(move |conn, _meta| {
                Box::pin(async move {
                    sqlx::query(&dialect::sql("SET SESSION time_zone = '+00:00'"))
                        .execute(&mut *conn)
                        .await?;
                    // Same partition guard as PostgreSQL: a lost connection
                    // must not wait forever on a row lock. MySQL's default is
                    // 50s; the production default of 10s keeps the
                    // cluster-level write stall bounded.
                    let lock_wait =
                        format!("SET SESSION innodb_lock_wait_timeout = {lock_wait_secs}");
                    sqlx::query(&dialect::sql(&lock_wait))
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
        let _ = (url, max, guardrails);
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

/// Total attempts (first try + retries) for the acquisition phase.
const BEGIN_WRITE_MAX_ATTEMPTS: u32 = 3;

// ---------------------------------------------------------------------------
// Test-observable acquisition probe (Oracle P1-1)
// ---------------------------------------------------------------------------
//
// The retry predicate is unit-tested against a synthetic `TestDbError`, and the
// give-up path is exercised live. What was missing was proof that the LIVE loop
// re-enters and then SUCCEEDS on a real engine (including MySQL's numeric `1205`
// branch, which `code()` cannot produce). These counters let a live test assert
// the attempt count and the first failure code deterministically. They are
// always compiled (a test binary does not see `cfg(test)` for the library) and
// have no effect on production control flow.

/// Acquisition attempts since the last [`begin_write_probe_reset`].
static BEGIN_WRITE_ATTEMPTS: AtomicU64 = AtomicU64::new(0);

/// First retryable acquisition failure since the last [`begin_write_probe_reset`],
/// rendered as its SQLSTATE or (MySQL) numeric error number.
static BEGIN_WRITE_FIRST_FAILURE: Mutex<Option<String>> = Mutex::new(None);

/// Reset the acquisition probe. Intended for tests; serialise callers so one
/// test's attempts are not counted against another's.
#[doc(hidden)]
pub fn begin_write_probe_reset() {
    BEGIN_WRITE_ATTEMPTS.store(0, Ordering::SeqCst);
    *crate::lock_gate(&BEGIN_WRITE_FIRST_FAILURE) = None;
}

/// Acquisition attempts recorded since the last [`begin_write_probe_reset`].
#[doc(hidden)]
#[must_use]
pub fn begin_write_attempts() -> u64 {
    BEGIN_WRITE_ATTEMPTS.load(Ordering::SeqCst)
}

/// The first retryable acquisition failure since the last
/// [`begin_write_probe_reset`], as a SQLSTATE or MySQL error number.
#[doc(hidden)]
#[must_use]
pub fn begin_write_first_failure() -> Option<String> {
    crate::lock_gate(&BEGIN_WRITE_FIRST_FAILURE).clone()
}

/// Record the first retryable acquisition failure, for the live retry tests.
fn record_first_failure(e: &sqlx::Error) {
    let label = lock_failure_label(e);
    let mut slot = crate::lock_gate(&BEGIN_WRITE_FIRST_FAILURE);
    if slot.is_none() {
        *slot = Some(label);
    }
}

/// The most specific identifier available for a lock-acquisition failure: the
/// MySQL numeric error (`1205`/`1213`) when this is a MySQL driver error, else
/// the SQLSTATE. Used only by the test probe.
fn lock_failure_label(e: &sqlx::Error) -> String {
    if let Some(number) = e.as_database_error().and_then(mysql_lock_error_number) {
        return number.to_string();
    }
    e.as_database_error()
        .and_then(|db| db.code())
        .map(|code| code.into_owned())
        .unwrap_or_else(|| e.to_string())
}

/// Begin a write transaction: begin, idempotently seed the single lock row, and
/// take `SELECT ... FOR UPDATE` on it. Every read-before-write transaction uses
/// this primitive so writers serialize on the same row across nodes.
///
/// **Contention retry.** Only the acquisition phase is retried, and only when
/// the failure is [`is_begin_write_retryable`] (the serialization/deadlock set
/// plus PostgreSQL `55P03` `lock_not_available`, the normal result of the
/// transaction-scoped `lock_timeout`). At most [`BEGIN_WRITE_MAX_ATTEMPTS`]
/// attempts are made, separated by bounded jitter. A retry starts a FRESH
/// transaction — the failed one is dropped and rolled back — so a retry can
/// never replay a mutation body: anything that fails AFTER acquisition is
/// returned to the caller unchanged (a retryable serialization/deadlock then
/// surfaces as 503 `storage_busy`).
///
/// Worst-case added write latency before the lock is surrendered is
/// approximately `3 × HYDRA_DB_LOCK_TIMEOUT_MS` — ~15s at the production
/// default of 5s — plus up to ~200ms of jitter per attempt. The final error is
/// returned to the caller unchanged; lock contention — including PostgreSQL
/// `55P03` — is answered as 503 `storage_busy` by `classify_db_err` through
/// [`is_lock_contention`]. (`55P03` stays out of the global [`is_retryable`]
/// classifier; only the write-acquisition predicate widens to include it.)
pub async fn begin_write(pool: &Pool) -> Result<Tx<'static>, sqlx::Error> {
    let guardrails = DbGuardrails::from_env();
    begin_write_loop(pool, &guardrails, false).await
}

/// Test seam: [`begin_write`] with caller-supplied guardrails instead of the
/// process environment, so a live test can use a tiny `lock_timeout` (or
/// `innodb_lock_wait_timeout`) without racing other tests on the shared env.
///
/// Behaviour is otherwise identical, except that the MySQL session lock-wait
/// timeout is (re)applied for this acquisition — that setting is session-scoped
/// and `init_pool`'s `after_connect` cannot know about a per-call override.
#[doc(hidden)]
pub async fn begin_write_with_guardrails(
    pool: &Pool,
    guardrails: &DbGuardrails,
) -> Result<Tx<'static>, sqlx::Error> {
    begin_write_loop(pool, guardrails, true).await
}

async fn begin_write_loop(
    pool: &Pool,
    guardrails: &DbGuardrails,
    apply_mysql_session_guardrails: bool,
) -> Result<Tx<'static>, sqlx::Error> {
    // RAII waiter gauge: one increment for the WHOLE acquisition (all attempts),
    // decremented by `Drop` when this function returns — including on the
    // success path, so the gauge is released before the caller receives the
    // `Tx` and is never tied to the `Tx`'s lifetime. A panic or an early `?`
    // inside the acquisition drops the guard and decrements it, so the gauge
    // cannot leak (the same RAII shape as `FailPersistGuard` below).
    let _waiter = WriteWaiterGuard::new();
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let started = std::time::Instant::now();
        let result = begin_write_once(pool, guardrails, apply_mysql_session_guardrails).await;
        crate::admin::metrics::record_db_write_acquire(started.elapsed().as_secs_f64());
        match result {
            Ok(tx) => return Ok(tx),
            Err(e) if attempt < BEGIN_WRITE_MAX_ATTEMPTS && is_begin_write_retryable(&e) => {
                // A retry is about to be scheduled for a retryable failure.
                crate::admin::metrics::record_db_write_retry("retry");
                record_first_failure(&e);
                let delay = begin_write_retry_delay(attempt - 1);
                tracing::debug!(
                    attempt,
                    max_attempts = BEGIN_WRITE_MAX_ATTEMPTS,
                    delay_ms = delay.as_millis() as u64,
                    error = %e,
                    "begin_write: retryable lock-acquisition failure; retrying"
                );
                tokio::time::sleep(delay).await;
            }
            Err(e) => {
                // The retry budget is spent AND the final error is still
                // retryable: the write is about to surface as `503
                // storage_busy`. A non-retryable failure is not "exhausted"
                // (retrying could not have helped), so it is not counted.
                if attempt == BEGIN_WRITE_MAX_ATTEMPTS && is_begin_write_retryable(&e) {
                    crate::admin::metrics::record_db_write_retry("exhausted");
                }
                return Err(e);
            }
        }
    }
}

/// RAII guard for `hydra_db_write_waiters`: increments the gauge when a caller
/// enters the write-lock acquisition loop and decrements it on `Drop`.
///
/// Scoped to the ACQUISITION only. It is a local of [`begin_write_loop`], so it
/// is released when that function returns — on the success path that happens
/// before the caller receives the `Tx`, so it is never tied to the `Tx`
/// lifetime. `Drop` also runs on a panic or an early `?`, so a failed
/// acquisition cannot leak the gauge. Mirrors the `FailPersistGuard` pattern
/// (test fault injection below).
struct WriteWaiterGuard;

impl WriteWaiterGuard {
    fn new() -> Self {
        crate::admin::metrics::record_db_write_waiter_enter();
        Self
    }
}

impl Drop for WriteWaiterGuard {
    fn drop(&mut self) {
        crate::admin::metrics::record_db_write_waiter_exit();
    }
}

/// One acquisition attempt. `SET LOCAL lock_timeout` (PostgreSQL only) is
/// transaction-scoped so it never leaks onto sqlx's migration advisory lock
/// (a session-wide setting made concurrent provisioning fail with `55P03`).
///
/// MySQL has no `SET LOCAL`: `innodb_lock_wait_timeout` is session-scoped, so
/// the seam re-applies it on the attempt's own connection when requested; the
/// production path relies on `init_pool`'s `after_connect` and is unchanged.
async fn begin_write_once(
    pool: &Pool,
    guardrails: &DbGuardrails,
    apply_mysql_session_guardrails: bool,
) -> Result<Tx<'static>, sqlx::Error> {
    BEGIN_WRITE_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
    let mut tx = pool.begin().await?;

    #[cfg(feature = "db-postgres")]
    {
        let lock_timeout = format!(
            "SET LOCAL lock_timeout = '{}ms'",
            guardrails.lock_timeout_ms
        );
        sqlx::query(&dialect::sql(&lock_timeout))
            .execute(&mut *tx)
            .await?;
    }

    #[cfg(feature = "db-mysql")]
    if apply_mysql_session_guardrails {
        let lock_wait = format!(
            "SET SESSION innodb_lock_wait_timeout = {}",
            guardrails.lock_wait_timeout_secs
        );
        sqlx::query(&dialect::sql(&lock_wait))
            .execute(&mut *tx)
            .await?;
    }

    #[cfg(not(feature = "db-mysql"))]
    let _ = apply_mysql_session_guardrails;
    #[cfg(not(feature = "db-postgres"))]
    let _ = guardrails;

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

/// Bounded exponential jitter between acquisition attempts: attempt 0 waits in
/// `[25, 50]ms`, attempt 1 in `[50, 100]ms`. The ceiling is hard-capped, so the
/// retry budget can never itself become an unbounded stall.
fn begin_write_retry_delay(attempt: u32) -> std::time::Duration {
    const BASE_MS: u64 = 25;
    const MAX_MS: u64 = 200;
    let low = BASE_MS.saturating_mul(1u64 << attempt.min(3)).min(MAX_MS);
    let high = low.saturating_mul(2).min(MAX_MS);
    let span = high - low + 1;
    let jitter = rand::random::<u64>() % span;
    std::time::Duration::from_millis(low + jitter)
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

/// True when a `begin_write` ACQUISITION failure may be retried.
///
/// This is the global [`is_retryable`] set (PostgreSQL `40001`/`40P01`, MySQL
/// `1213`/`1205`) **plus** PostgreSQL `55P03` (`lock_not_available`). `55P03`
/// is exactly how a transaction-scoped `lock_timeout` reports contention on
/// `hydra_lock`, so it is the primary PostgreSQL retry path. It stays OUT of
/// the global classifier (and therefore out of [`is_retryable`]) because
/// outside `begin_write` it can also mean a statement/tooling misuse that
/// retrying would not fix; leaving the classifier untouched keeps the global
/// error→HTTP mapping stable.
///
/// MySQL's SQLSTATE (`DatabaseError::code`) is coarser than its numeric error,
/// so the numeric `1213`/`1205` are also consulted through the driver type.
/// True when a DB error is lock contention on the shared write lock and should
/// be answered as a transient `503 storage_busy`.
///
/// This is [`is_begin_write_retryable`]: the global retryable set plus
/// PostgreSQL `55P03` (`lock_not_available`). It is kept as a distinctly named
/// predicate because the error→HTTP owner must not import a name phrased as a
/// retry decision for `begin_write`.
#[must_use]
pub fn is_lock_contention(e: &sqlx::Error) -> bool {
    is_begin_write_retryable(e)
}

#[must_use]
pub fn is_begin_write_retryable(e: &sqlx::Error) -> bool {
    let Some(db) = e.as_database_error() else {
        return false;
    };
    if db
        .code()
        .map(|code| begin_write_retryable_code(code.as_ref()))
        .unwrap_or(false)
    {
        return true;
    }
    mysql_lock_error_number(db)
        .map(begin_write_retryable_mysql_number)
        .unwrap_or(false)
}

/// SQLSTATE-level codes worth retrying for a `begin_write` acquisition: the
/// union of both backend arms of [`retryable_code`] plus PostgreSQL `55P03`.
/// Kept backend-agnostic so one unit test covers both dialects' codes.
fn begin_write_retryable_code(code: &str) -> bool {
    matches!(code, "40001" | "40P01" | "1213" | "1205" | "55P03")
}

/// MySQL numeric errors worth retrying: `1213` (`ER_LOCK_DEADLOCK`) and `1205`
/// (`ER_LOCK_WAIT_TIMEOUT`). Pure so it is testable in every feature mode.
fn begin_write_retryable_mysql_number(number: u16) -> bool {
    matches!(number, 1213 | 1205)
}

/// The numeric error number behind a MySQL database error, if this is one.
///
/// SQLx exposes MySQL's SQLSTATE through `code()`; the lock/deadlock numbers
/// are only reachable through the driver type. The non-MySQL build returns
/// `None`, which keeps the call site backend-agnostic without referencing the
/// MySQL type in a PostgreSQL-only build.
#[cfg(feature = "db-mysql")]
fn mysql_lock_error_number(db: &dyn sqlx::error::DatabaseError) -> Option<u16> {
    db.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
        .map(sqlx::mysql::MySqlDatabaseError::number)
}

#[cfg(not(feature = "db-mysql"))]
fn mysql_lock_error_number(_db: &dyn sqlx::error::DatabaseError) -> Option<u16> {
    None
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
    use std::borrow::Cow;
    use std::error::Error as StdError;

    use super::{
        begin_write_retryable_code, begin_write_retryable_mysql_number, db_url_satisfies_tls,
        is_begin_write_retryable, is_lock_contention, is_unavailable, DbGuardrails,
    };

    /// A database error carrying an arbitrary SQLSTATE, so the retry predicates
    /// can be unit-tested for both dialects without a live server (and without
    /// constructing a driver-private `PgDatabaseError`/`MySqlDatabaseError`).
    #[derive(Debug)]
    struct TestDbError(&'static str);

    impl std::fmt::Display for TestDbError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "test database error {}", self.0)
        }
    }

    impl StdError for TestDbError {}

    impl sqlx::error::DatabaseError for TestDbError {
        fn message(&self) -> &str {
            "test database error"
        }

        fn code(&self) -> Option<Cow<'_, str>> {
            Some(Cow::Borrowed(self.0))
        }

        fn as_error(&self) -> &(dyn StdError + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn StdError + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn StdError + Send + Sync + 'static> {
            self
        }

        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    fn db_error(code: &'static str) -> sqlx::Error {
        TestDbError(code).into()
    }

    fn guardrails(pairs: &[(&str, &str)]) -> DbGuardrails {
        DbGuardrails::from_lookup(|key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_string())
        })
    }

    /// `HYDRA_DB_REQUIRE_TLS` accepts exactly the PostgreSQL modes that BOTH
    /// encrypt AND verify the server identity, case-insensitively.
    #[test]
    fn postgres_tls_modes_that_authenticate_the_server_are_accepted() {
        for mode in [
            "require",
            "verify-ca",
            "verify-full",
            "REQUIRE",
            "VERIFY-FULL",
            " Verify-Ca ",
        ] {
            let url = format!("postgres://hydra:secret@db.internal:5432/hydra?sslmode={mode}");
            assert!(db_url_satisfies_tls(&url), "{url}");
        }
        // An unrelated query parameter must not get in the way.
        assert!(db_url_satisfies_tls(
            "postgresql://hydra@db.internal/hydra?application_name=hydra&sslmode=verify-full"
        ));
        // ...and the key itself is matched case-insensitively.
        assert!(db_url_satisfies_tls(
            "postgres://hydra@db.internal/hydra?SSLMODE=verify-full"
        ));
    }

    /// Weaker or missing PostgreSQL modes are rejected.
    #[test]
    fn postgres_tls_modes_that_do_not_verify_are_rejected() {
        for mode in [
            "disable",
            "allow",
            "prefer",
            "",
            "verify",
            "required",
            "VERIFY-FULLX",
        ] {
            let url = format!("postgres://hydra@db.internal/hydra?sslmode={mode}");
            assert!(!db_url_satisfies_tls(&url), "{url}");
        }
        for url in [
            "postgres://hydra@db.internal/hydra",
            "postgres://hydra@db.internal/hydra?application_name=hydra",
            "postgres://hydra@db.internal/hydra?sslmode",
        ] {
            assert!(!db_url_satisfies_tls(url), "{url}");
        }
    }

    /// MySQL accepts BOTH key spellings (`ssl-mode` and `sslmode`) with any of
    /// the three identity-verifying modes, case-insensitively.
    #[test]
    fn mysql_tls_modes_are_accepted_with_either_key_spelling() {
        for key in ["ssl-mode", "sslmode", "SSL-Mode"] {
            for mode in [
                "REQUIRED",
                "VERIFY_CA",
                "VERIFY_IDENTITY",
                "required",
                "verify_ca",
                " Verify_Identity ",
            ] {
                let url = format!("mysql://hydra:secret@db.internal:3306/hydra?{key}={mode}");
                assert!(db_url_satisfies_tls(&url), "{url}");
            }
        }
    }

    /// Weaker or missing MySQL modes are rejected under either spelling.
    #[test]
    fn mysql_tls_modes_that_do_not_verify_are_rejected() {
        for key in ["ssl-mode", "sslmode"] {
            for mode in ["DISABLED", "PREFERRED", "VERIFY", "", "REQUIREDX"] {
                let url = format!("mysql://hydra@db.internal/hydra?{key}={mode}");
                assert!(!db_url_satisfies_tls(&url), "{url}");
            }
        }
        for url in [
            "mysql://hydra@db.internal/hydra",
            "mysql://hydra@db.internal/hydra?charset=utf8mb4",
        ] {
            assert!(!db_url_satisfies_tls(url), "{url}");
        }
    }

    /// A repeated recognised key must be strict in EVERY occurrence: a weak
    /// duplicate must not be able to override a strict one.
    #[test]
    fn a_repeated_tls_key_must_be_strict_in_every_occurrence() {
        assert!(db_url_satisfies_tls(
            "postgres://hydra@db.internal/hydra?sslmode=require&sslmode=verify-full"
        ));
        assert!(!db_url_satisfies_tls(
            "postgres://hydra@db.internal/hydra?sslmode=require&sslmode=disable"
        ));
        assert!(!db_url_satisfies_tls(
            "mysql://hydra@db.internal/hydra?ssl-mode=REQUIRED&sslmode=DISABLED"
        ));
    }

    /// Unknown schemes and malformed URLs are `false`, never a panic.
    #[test]
    fn unknown_schemes_and_malformed_urls_are_rejected() {
        for url in [
            "",
            "not-a-url",
            "sqlite:///hydra.db?sslmode=verify-full",
            "http://db.internal/?sslmode=verify-full",
            "postgres:/hydra?sslmode=verify-full",
        ] {
            assert!(!db_url_satisfies_tls(url), "{url}");
        }
    }

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

    // -----------------------------------------------------------------------
    // F4: guardrail parsing/clamping
    // -----------------------------------------------------------------------

    #[test]
    fn guardrail_defaults_match_the_old_hard_coded_values() {
        let g = guardrails(&[]);
        assert_eq!(g.lock_timeout_ms, 5_000, "PG SET LOCAL lock_timeout was 5s");
        assert_eq!(
            g.statement_timeout_ms, 60_000,
            "PG statement_timeout was 60s"
        );
        assert_eq!(
            g.idle_tx_timeout_ms, 60_000,
            "PG idle_in_transaction_session_timeout was 60s"
        );
        assert_eq!(
            g.lock_wait_timeout_secs, 10,
            "MySQL innodb_lock_wait_timeout was 10s"
        );
    }

    #[test]
    fn guardrails_parse_and_trim_overrides() {
        let g = guardrails(&[
            ("HYDRA_DB_LOCK_TIMEOUT_MS", " 250 "),
            ("HYDRA_DB_STATEMENT_TIMEOUT_MS", "9000"),
            ("HYDRA_DB_IDLE_TX_TIMEOUT_MS", "12000"),
            ("HYDRA_DB_LOCK_WAIT_TIMEOUT_SECS", "3"),
        ]);
        assert_eq!(g.lock_timeout_ms, 250);
        assert_eq!(g.statement_timeout_ms, 9_000);
        assert_eq!(g.idle_tx_timeout_ms, 12_000);
        assert_eq!(g.lock_wait_timeout_secs, 3);
    }

    #[test]
    fn guardrails_clamp_zero_garbage_and_overflow() {
        // 0 would DISABLE PostgreSQL's timeout (wait forever), so it is clamped
        // up to the finite minimum rather than honoured.
        assert_eq!(
            guardrails(&[("HYDRA_DB_LOCK_TIMEOUT_MS", "0")]).lock_timeout_ms,
            super::MIN_TIMEOUT_MS
        );

        // Blank, whitespace, non-numeric and negative all fall back to the
        // production default (never an accidental disable/zero).
        for raw in ["", "   ", "abc", "-1", "1.5"] {
            for (key, default) in [
                ("HYDRA_DB_LOCK_TIMEOUT_MS", 5_000),
                ("HYDRA_DB_STATEMENT_TIMEOUT_MS", 60_000),
                ("HYDRA_DB_IDLE_TX_TIMEOUT_MS", 60_000),
                ("HYDRA_DB_LOCK_WAIT_TIMEOUT_SECS", 10),
            ] {
                let g = guardrails(&[(key, raw)]);
                let actual = match key {
                    "HYDRA_DB_LOCK_TIMEOUT_MS" => g.lock_timeout_ms,
                    "HYDRA_DB_STATEMENT_TIMEOUT_MS" => g.statement_timeout_ms,
                    "HYDRA_DB_IDLE_TX_TIMEOUT_MS" => g.idle_tx_timeout_ms,
                    _ => g.lock_wait_timeout_secs,
                };
                assert_eq!(actual, default, "{key}={raw:?} must take the default");
            }
        }

        // Above the ceiling is clamped to the ceiling.
        let g = guardrails(&[
            ("HYDRA_DB_STATEMENT_TIMEOUT_MS", "999999999"),
            ("HYDRA_DB_IDLE_TX_TIMEOUT_MS", "999999999"),
        ]);
        assert_eq!(g.statement_timeout_ms, super::MAX_TIMEOUT_MS);
        assert_eq!(g.idle_tx_timeout_ms, super::MAX_TIMEOUT_MS);

        // MySQL's lock wait timeout has its own (second-scale) clamps.
        assert_eq!(
            guardrails(&[("HYDRA_DB_LOCK_WAIT_TIMEOUT_SECS", "0")]).lock_wait_timeout_secs,
            super::MIN_LOCK_WAIT_SECS
        );
        assert_eq!(
            guardrails(&[("HYDRA_DB_LOCK_WAIT_TIMEOUT_SECS", "999999")]).lock_wait_timeout_secs,
            super::MAX_LOCK_WAIT_SECS
        );
    }

    #[test]
    fn lock_timeout_is_clamped_down_to_the_statement_timeout() {
        let g = guardrails(&[
            ("HYDRA_DB_LOCK_TIMEOUT_MS", "9000"),
            ("HYDRA_DB_STATEMENT_TIMEOUT_MS", "3000"),
        ]);
        assert_eq!(g.statement_timeout_ms, 3_000);
        assert_eq!(
            g.lock_timeout_ms, 3_000,
            "a lock timeout above the statement timeout must be folded down"
        );

        // The sanity rule also holds after the individual clamps.
        let g = guardrails(&[
            ("HYDRA_DB_LOCK_TIMEOUT_MS", "999999999"),
            ("HYDRA_DB_STATEMENT_TIMEOUT_MS", "12000"),
        ]);
        assert_eq!(g.lock_timeout_ms, 12_000);
    }

    // -----------------------------------------------------------------------
    // F4: begin_write retry predicate
    // -----------------------------------------------------------------------

    #[test]
    fn begin_write_is_retryable_for_both_dialects_and_pg_lock_timeout() {
        // The global classifier's codes (both backend arms) plus PG 55P03.
        for code in ["40001", "40P01", "55P03", "1213", "1205"] {
            assert!(
                is_begin_write_retryable(&db_error(code)),
                "{code} must be retried by begin_write"
            );
            assert!(begin_write_retryable_code(code), "{code}");
        }

        // The real MySQL driver surfaces SQLSTATE through `code()`, so the
        // numeric lock/deadlock errors are consulted separately.
        for number in [1213u16, 1205] {
            assert!(
                begin_write_retryable_mysql_number(number),
                "MySQL {number} must be retried by begin_write"
            );
        }
    }

    #[test]
    fn begin_write_retryable_excludes_non_contention_failures() {
        for code in ["23505", "23503", "23514", "42P01", "25P02", "HY000"] {
            assert!(
                !is_begin_write_retryable(&db_error(code)),
                "{code} is not a lock-acquisition failure"
            );
        }
        for number in [1062u16, 1452, 1615] {
            assert!(
                !begin_write_retryable_mysql_number(number),
                "MySQL {number} must not be retried by begin_write"
            );
        }

        for error in [
            sqlx::Error::PoolTimedOut,
            sqlx::Error::RowNotFound,
            sqlx::Error::Protocol("unexpected packet".into()),
        ] {
            assert!(!is_begin_write_retryable(&error), "{error:?}");
        }
    }

    /// The retry predicate deliberately widens `begin_write` only: `55P03` must
    /// stay out of the global classifier so the shared error→HTTP mapping is
    /// unchanged.
    #[cfg(feature = "db-postgres")]
    #[test]
    fn begin_write_adds_55p03_without_changing_the_global_classifier() {
        assert!(!super::is_retryable(&db_error("55P03")));
        assert!(is_begin_write_retryable(&db_error("55P03")));
    }

    /// Lock contention (including PG `55P03`) is a transient 503, never a 500.
    /// The HTTP mapping imports this predicate instead of the retry-only name.
    #[test]
    fn lock_contention_covers_pg_lock_timeout_and_serialization_codes() {
        for code in ["40001", "40P01", "55P03", "1213", "1205"] {
            assert!(is_lock_contention(&db_error(code)), "{code} is contention");
        }
        for code in ["23505", "23503", "42P01", "57014"] {
            assert!(!is_lock_contention(&db_error(code)), "{code}");
        }
    }

    // -----------------------------------------------------------------------
    // F4: real PostgreSQL retry path
    // -----------------------------------------------------------------------

    /// RAII environment override: restores the previous value even when a test
    /// assertion panics. The process environment is global, so the override is
    /// kept to the narrowest possible window.
    #[cfg(any(feature = "db-postgres", feature = "db-mysql"))]
    struct EnvVar(&'static str, Option<String>);

    #[cfg(any(feature = "db-postgres", feature = "db-mysql"))]
    impl EnvVar {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self(key, previous)
        }
    }

    #[cfg(any(feature = "db-postgres", feature = "db-mysql"))]
    impl Drop for EnvVar {
        fn drop(&mut self) {
            match &self.1 {
                Some(value) => std::env::set_var(self.0, value),
                None => std::env::remove_var(self.0),
            }
        }
    }

    /// Hold `hydra_lock` on pool A, give pool B a tiny `lock_timeout`, and prove
    /// `begin_write` retries the resulting `55P03` more than once before giving
    /// up — without hanging.
    #[cfg(feature = "db-postgres")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn begin_write_retries_pg_lock_timeout_then_returns_the_busy_error() {
        use std::time::{Duration, Instant};

        use crate::db::testing;

        let test_db = testing::isolated().await;
        let pool_a = test_db.pool.clone();

        // Pool A keeps `hydra_lock` for the whole failing call.
        let tx_a = super::begin_write(&pool_a)
            .await
            .expect("pool A acquires the lock");

        let pool_b = super::init_pool(&test_db.url)
            .await
            .expect("pool B connects");

        // 50ms per attempt keeps the test fast; the guard keeps the window as
        // short as possible because the environment is process-global.
        let started = Instant::now();
        let err = {
            let _lock_timeout = EnvVar::set("HYDRA_DB_LOCK_TIMEOUT_MS", "50");
            match super::begin_write(&pool_b).await {
                Ok(_) => panic!("pool B must not acquire the row while pool A holds it"),
                Err(e) => e,
            }
        };
        let elapsed = started.elapsed();

        let code = err
            .as_database_error()
            .and_then(|db| db.code())
            .map(Cow::into_owned);
        assert_eq!(
            code.as_deref(),
            Some("55P03"),
            "expected lock_not_available, got {err:?}"
        );

        // Three attempts of ~50ms each (plus jitter) must have elapsed. At
        // least two lock timeouts proves `begin_write` re-entered; the upper
        // bound proves the jitter/retry budget stayed bounded and did not hang.
        assert!(
            elapsed >= Duration::from_millis(100),
            "begin_write must retry the acquisition, elapsed {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the retry budget must stay bounded, elapsed {elapsed:?}"
        );

        // Releasing A must leave pool B usable: the dropped, failed
        // transactions were rolled back, not left dangling.
        tx_a.rollback().await.expect("release pool A lock");
        let tx_b = super::begin_write(&pool_b)
            .await
            .expect("pool B acquires the lock after release");
        tx_b.rollback().await.expect("rollback pool B");

        pool_b.close().await;
        pool_a.close().await;
        test_db.cleanup().await;
    }

    /// MySQL counterpart: hold `hydra_lock` on pool A, give pool B a 1s
    /// `innodb_lock_wait_timeout`, and prove `begin_write` retries the numeric
    /// `1205` (`ER_LOCK_WAIT_TIMEOUT`) before giving up — without hanging.
    #[cfg(feature = "db-mysql")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn begin_write_retries_mysql_lock_wait_timeout_then_returns_the_busy_error() {
        use std::time::{Duration, Instant};

        use crate::db::testing;

        let test_db = testing::isolated().await;
        let pool_a = test_db.pool.clone();

        // Pool A keeps `hydra_lock` for the whole failing call.
        let tx_a = super::begin_write(&pool_a)
            .await
            .expect("pool A acquires the lock");

        // `innodb_lock_wait_timeout` is session-scoped and read when the pool is
        // built, so the override must cover `init_pool`. It is clamped to a 1s
        // floor, so each attempt waits a full second.
        let pool_b = {
            let _wait = EnvVar::set("HYDRA_DB_LOCK_WAIT_TIMEOUT_SECS", "1");
            super::init_pool(&test_db.url)
                .await
                .expect("pool B connects")
        };

        let started = Instant::now();
        let err = match super::begin_write(&pool_b).await {
            Ok(_) => panic!("pool B must not acquire the row while pool A holds it"),
            Err(e) => e,
        };
        let elapsed = started.elapsed();

        let number = err
            .as_database_error()
            .and_then(|db| db.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>())
            .map(sqlx::mysql::MySqlDatabaseError::number);
        assert_eq!(
            number,
            Some(1205),
            "expected ER_LOCK_WAIT_TIMEOUT, got {err:?}"
        );

        // Three 1s attempts (plus jitter); at least two proves `begin_write`
        // re-entered, the upper bound proves the retry budget stayed bounded.
        assert!(
            elapsed >= Duration::from_secs(2),
            "begin_write must retry the acquisition, elapsed {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(20),
            "the retry budget must stay bounded, elapsed {elapsed:?}"
        );

        // Releasing A must leave pool B usable.
        tx_a.rollback().await.expect("release pool A lock");
        let tx_b = super::begin_write(&pool_b)
            .await
            .expect("pool B acquires the lock after release");
        tx_b.rollback().await.expect("rollback pool B");

        pool_b.close().await;
        pool_a.close().await;
        test_db.cleanup().await;
    }
}
