//! Backend-agnostic config-plane write-lock behaviour.
//!
//! SQLite's `BEGIN IMMEDIATE` / WAL single-writer semantics are gone. The
//! portable replacement is the shared `hydra_lock` row: [`db::begin_write`]
//! takes `SELECT ... FOR UPDATE` on it, so two overlapping writers serialize and
//! the second WAITS for the first to commit instead of observing a stale
//! snapshot or failing.
//!
//! This is the claim that matters for the config plane: a read-before-write
//! transaction must not fail just because another writer committed in between.
//! The test uses its own isolated schema/database (see
//! [`hydra_server::db::testing`]) and two real connections.
//!
//! It also proves the LIVE contention retry (Oracle P1-1): a real `55P03`
//! (PostgreSQL) or `1205` (MySQL) on the first attempt, a real second attempt
//! after the holder releases, and the write's increment landing. MySQL's
//! `1205` is only reachable through the driver's numeric error, which the
//! synthetic `code()` unit test cannot exercise.

use std::time::{Duration, Instant};

use hydra_server::db;

/// Serialises the tests in this file that read the process-wide acquisition
/// probe, so one test's attempts are never counted against another's.
static PROBE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn set_version_sql() -> &'static str {
    "UPDATE config_revision SET version = version + 1 WHERE id = 1"
}

/// A second `begin_write` transaction WAITS for the first and then succeeds;
/// both increments land.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_writer_waits_for_the_first_instead_of_failing() {
    let _probe = PROBE_LOCK.lock().await;

    let test_db = db::testing::isolated().await;
    let pool = test_db.pool.clone();

    // Writer 1 holds the shared lock row.
    let mut tx1 = db::begin_write(&pool).await.expect("tx1 begins");
    sqlx::query(set_version_sql())
        .execute(&mut *tx1)
        .await
        .expect("tx1 write");

    // Writer 2 starts while tx1 is open. `begin_write` must block on the row
    // lock until tx1 commits, then proceed.
    let pool2 = pool.clone();
    let started = Instant::now();
    let waiter = tokio::spawn(async move {
        let mut tx2 = db::begin_write(&pool2).await?;
        sqlx::query(set_version_sql()).execute(&mut *tx2).await?;
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

    assert_eq!(
        db::get_config_version(&pool).await.expect("read marker"),
        db::ConfigVersion::Value(2),
        "both writers' increments must be present"
    );

    pool.close().await;
    test_db.cleanup().await;
}

/// Oracle P1-1, PostgreSQL: a LIVE holder keeps `hydra_lock` past a 50ms
/// `lock_timeout`; `begin_write`'s first attempt fails with a real `55P03`, a
/// retry then acquires the lock once the holder releases, and the write lands.
#[cfg(feature = "db-postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn begin_write_retries_a_real_pg_lock_timeout_then_succeeds() {
    let _probe = PROBE_LOCK.lock().await;

    let test_db = db::testing::isolated().await;
    let pool = test_db.pool.clone();

    // A real guardrail value, no environment mutation: a 50ms transaction
    // `lock_timeout`, which the retry must turn into exactly one failed attempt.
    let guardrails = db::DbGuardrails::from_lookup(|key| match key {
        "HYDRA_DB_LOCK_TIMEOUT_MS" => Some("50".to_string()),
        _ => None,
    });

    // Holder: take the `hydra_lock` row FOR UPDATE, signal, then release after a
    // short bounded delay (one failed attempt + jitter).
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let holder_pool = pool.clone();
    let holder = tokio::spawn(async move {
        let tx = db::begin_write(&holder_pool)
            .await
            .expect("holder acquires the lock row");
        let _ = ready_tx.send(());
        tokio::time::sleep(Duration::from_millis(200)).await;
        tx.rollback().await.expect("holder releases the lock row");
    });
    ready_rx.await.expect("holder signalled acquisition");

    // Count only the measured acquisition (the holder already incremented).
    db::begin_write_probe_reset();

    let started = Instant::now();
    let mut tx = db::begin_write_with_guardrails(&pool, &guardrails)
        .await
        .expect("the retry must acquire the lock after the holder releases");
    let elapsed = started.elapsed();

    assert_eq!(
        db::begin_write_first_failure().as_deref(),
        Some("55P03"),
        "the first attempt must fail with a real PostgreSQL lock_not_available"
    );
    assert!(
        db::begin_write_attempts() >= 2,
        "the live retry loop must have re-entered after the failed attempt"
    );
    assert!(
        elapsed >= Duration::from_millis(50),
        "the first attempt must have waited out the lock_timeout, elapsed {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the retry budget must stay bounded, elapsed {elapsed:?}"
    );

    // The write's increment must actually land.
    sqlx::query(set_version_sql())
        .execute(&mut *tx)
        .await
        .expect("write through the retried transaction");
    tx.commit().await.expect("commit the retried transaction");
    assert_eq!(
        db::get_config_version(&pool).await.expect("read marker"),
        db::ConfigVersion::Value(1),
        "the increment from the retried write must be present"
    );

    tokio::time::timeout(Duration::from_secs(5), holder)
        .await
        .expect("holder must finish")
        .expect("holder join");

    pool.close().await;
    test_db.cleanup().await;
}

/// Oracle P1-1, MySQL: a LIVE holder keeps `hydra_lock` past the 1s
/// `innodb_lock_wait_timeout` clamp minimum; the first attempt fails with a real
/// numeric `1205` (`ER_LOCK_WAIT_TIMEOUT`, unreachable through `code()`), a
/// retry then acquires the lock once the holder releases, and the write lands.
#[cfg(feature = "db-mysql")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn begin_write_retries_a_real_mysql_lock_wait_timeout_then_succeeds() {
    let _probe = PROBE_LOCK.lock().await;

    let test_db = db::testing::isolated().await;
    let pool = test_db.pool.clone();

    // The clamp floor is 1s; the seam must apply it to the attempt's session.
    let guardrails = db::DbGuardrails::from_lookup(|key| match key {
        "HYDRA_DB_LOCK_WAIT_TIMEOUT_SECS" => Some("1".to_string()),
        _ => None,
    });

    // Holder: take the `hydra_lock` row FOR UPDATE and keep it PAST the 1s
    // timeout, then release.
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let holder_pool = pool.clone();
    let holder = tokio::spawn(async move {
        let tx = db::begin_write(&holder_pool)
            .await
            .expect("holder acquires the lock row");
        let _ = ready_tx.send(());
        tokio::time::sleep(Duration::from_millis(1500)).await;
        tx.rollback().await.expect("holder releases the lock row");
    });
    ready_rx.await.expect("holder signalled acquisition");

    // Count only the measured acquisition (the holder already incremented).
    db::begin_write_probe_reset();

    let started = Instant::now();
    let mut tx = db::begin_write_with_guardrails(&pool, &guardrails)
        .await
        .expect("the retry must acquire the lock after the holder releases");
    let elapsed = started.elapsed();

    assert_eq!(
        db::begin_write_first_failure().as_deref(),
        Some("1205"),
        "the first attempt must fail with a real MySQL ER_LOCK_WAIT_TIMEOUT"
    );
    assert!(
        db::begin_write_attempts() >= 2,
        "the live retry loop must have re-entered after the failed attempt"
    );
    assert!(
        elapsed >= Duration::from_millis(900),
        "the first attempt must have waited out the 1s lock-wait timeout, elapsed {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "the retry budget must stay bounded and the 1s override must have applied \
         (the pool default is 10s), elapsed {elapsed:?}"
    );

    // The write's increment must actually land.
    sqlx::query(set_version_sql())
        .execute(&mut *tx)
        .await
        .expect("write through the retried transaction");
    tx.commit().await.expect("commit the retried transaction");
    assert_eq!(
        db::get_config_version(&pool).await.expect("read marker"),
        db::ConfigVersion::Value(1),
        "the increment from the retried write must be present"
    );

    tokio::time::timeout(Duration::from_secs(5), holder)
        .await
        .expect("holder must finish")
        .expect("holder join");

    pool.close().await;
    test_db.cleanup().await;
}
