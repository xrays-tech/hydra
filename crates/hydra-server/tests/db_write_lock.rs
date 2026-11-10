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

use std::time::{Duration, Instant};

use hydra_server::db;

fn set_version_sql() -> &'static str {
    "UPDATE config_revision SET version = version + 1 WHERE id = 1"
}

/// A second `begin_write` transaction WAITS for the first and then succeeds;
/// both increments land.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_writer_waits_for_the_first_instead_of_failing() {
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
