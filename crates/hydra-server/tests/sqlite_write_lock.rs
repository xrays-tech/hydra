//! SQLite write-lock behaviour for the config plane.
//!
//! Regression guard for the DEFERRED-transaction hole: SQLite's plain `BEGIN`
//! starts as a reader and upgrades on the first write, and under WAL that
//! upgrade fails immediately with `SQLITE_BUSY` — **without consulting
//! `busy_timeout`** — if any other connection committed in between. Every
//! transaction in this crate reads before it writes, so with `pool.begin()` a
//! concurrent writer (a usage-sink flush, another admin write) turned one of the
//! two into a `500 database_error`.
//!
//! `db::begin_write` uses `BEGIN IMMEDIATE`, so the lock is taken where
//! `busy_timeout` applies and the second writer waits its turn.
//!
//! These tests need a FILE-backed pool with more than one connection, which is
//! why they do not use `common::setup_pool()` (`:memory:` is pinned to a single
//! connection by design).

use std::time::{Duration, Instant};

use hydra_server::db;
use sqlx::SqlitePool;

/// A migrated, file-backed pool (8 connections, like production).
async fn file_pool(tag: &str) -> (SqlitePool, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "hydra-write-lock-{tag}-{}-{:?}.db",
        std::process::id(),
        std::thread::current().id()
    ));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let pool = db::init_pool(&url).await.expect("file pool");
    db::run_migrate(&pool).await.expect("migrate");
    (pool, path)
}

fn cleanup(path: &std::path::Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

/// The core assertion: a second write transaction that overlaps a first one
/// WAITS (up to `busy_timeout`) and then succeeds — it does not fail, and it
/// cannot observe a stale snapshot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_writer_waits_for_the_first_instead_of_failing() {
    let (pool, path) = file_pool("waits").await;

    // Writer 1 holds the write lock.
    let mut tx1 = db::begin_write(&pool).await.expect("tx1 begins");
    sqlx::query("INSERT INTO config_meta (key, value) VALUES ('k1','v1')")
        .execute(&mut *tx1)
        .await
        .expect("tx1 write");

    // Writer 2 starts while tx1 is open, and does the read-then-write shape that
    // used to blow up. It must block until tx1 commits, then proceed.
    let pool2 = pool.clone();
    let started = Instant::now();
    let waiter = tokio::spawn(async move {
        let mut tx2 = db::begin_write(&pool2).await?;
        // Read first (this is what makes a DEFERRED begin take a snapshot)…
        let _read: Option<(String,)> =
            sqlx::query_as("SELECT value FROM config_meta WHERE key = 'k1'")
                .fetch_optional(&mut *tx2)
                .await?;
        // …then write (this is where the stale snapshot used to fail).
        sqlx::query("INSERT INTO config_meta (key, value) VALUES ('k2','v2')")
            .execute(&mut *tx2)
            .await?;
        tx2.commit().await?;
        Ok::<(), sqlx::Error>(())
    });

    // Let writer 2 reach the lock, then release it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    tx1.commit().await.expect("tx1 commits");

    let outcome = tokio::time::timeout(Duration::from_secs(15), waiter)
        .await
        .expect("writer 2 must not hang")
        .expect("join");
    assert!(
        outcome.is_ok(),
        "the second writer must succeed after waiting, got {:?}",
        outcome.err()
    );
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "writer 2 must actually have WAITED for the lock (elapsed {:?})",
        started.elapsed()
    );

    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM config_meta")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(n, 2, "both writers' rows must be present");

    pool.close().await;
    cleanup(&path);
}

/// The DEFERRED read-then-write hazard FIRES, and it fires as SQLITE_BUSY /
/// SQLITE_BUSY_SNAPSHOT — which is what `classify_db_err` maps to the retryable
/// `503 storage_busy`, and the whole reason the config plane takes its lock up
/// front with `db::begin_write` (`BEGIN IMMEDIATE`).
///
/// This test used to accept BOTH outcomes (`Ok(_) => {}` alongside the error arm),
/// so it could never fail: it was cited as the arm that "would fail loudly if a
/// future refactor quietly routed the config plane back through `pool.begin()`",
/// while in fact it exercises raw SQL on its own pool and would not notice that
/// refactor at all. The arm that really guards the choice is
/// `a_second_writer_waits_for_the_first_instead_of_failing` (it fails unless
/// `begin_write` is used); THIS one is here to pin that the hazard is real, so
/// nobody deletes `begin_write` as "unnecessary".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deferred_begin_is_the_hazard_begin_write_removes() {
    let (pool, path) = file_pool("deferred").await;

    let mut tx1 = db::begin_write(&pool).await.expect("tx1 begins");
    sqlx::query("INSERT INTO config_meta (key, value) VALUES ('k1','v1')")
        .execute(&mut *tx1)
        .await
        .expect("tx1 write");

    // DEFERRED begin: reads without the write lock…
    let mut tx2 = pool.begin().await.expect("tx2 begins (DEFERRED)");
    let _read: Option<(String,)> = sqlx::query_as("SELECT value FROM config_meta WHERE key = 'k1'")
        .fetch_optional(&mut *tx2)
        .await
        .expect("tx2 read");

    tx1.commit().await.expect("tx1 commits");

    // tx2's snapshot predates tx1's commit, so its upgrade to a writer cannot
    // succeed: SQLite refuses it rather than letting the two writers interleave.
    let e = sqlx::query("INSERT INTO config_meta (key, value) VALUES ('k2','v2')")
        .execute(&mut *tx2)
        .await
        .expect_err(
            "a DEFERRED read-then-write MUST fail once another connection committed in \
             between — that is the hazard `begin_write` (BEGIN IMMEDIATE) removes from \
             the config plane. If this ever starts succeeding, SQLite/sqlx changed its \
             snapshot semantics and the whole BEGIN IMMEDIATE rationale needs revisiting",
        );
    let code = match &e {
        sqlx::Error::Database(db) => db.code().map(|c| c.to_string()),
        _ => None,
    };
    assert!(
        matches!(code.as_deref(), Some("5") | Some("517")),
        "the DEFERRED hazard must surface as SQLITE_BUSY(5)/BUSY_SNAPSHOT(517) — the \
         codes `classify_db_err` turns into a retryable 503 storage_busy — got {e:?}"
    );
    let _ = tx2.rollback().await;

    pool.close().await;
    cleanup(&path);
}
