//! Limit-role CRUD and the seal/open boundary for `limit_role.matching_key`.

use hydra_core::model::LimitRole;

use crate::crypto::{KeyProvider, Sealed};

use super::dialect;
use super::pool::{begin_write, commit_write};
use super::rows::{crypto_to_sqlx, flag, parse_ts, LimitRoleRow};
use super::{Db, Pool};

// ---------------------------------------------------------------------------
// CRUD — LimitRole
// ---------------------------------------------------------------------------
//
// `matching_key` holds a CLIENT CREDENTIAL, and it is sealed at this boundary (decision D-16,
// 2026-10-08 — the user's ruling: seal the column, and re-seal legacy rows without a manual step).
// The three functions below split that one policy three ways:
//
//   * write (`insert_limit_role` / `update_limit_role`): `kp.seal(..).to_text()`. This column never
//     receives a client key again.
//   * read (`get_limit_role` / `list_limit_roles[_on]`): `crypto::Sealed::open_text`, which opens an
//     envelope and passes a NON-envelope value through as plaintext. The migration below runs before
//     the LOADER's own read, so what that branch actually serves is every OTHER reader — chiefly an
//     admin `GET` before the migration has run. (Measured while falsifying this: see
//     `crypto::Sealed::open_text`, which carries the probe's result.) The config TREE gets no such
//     fallback on purpose; it is versioned by `TOC_FORMAT`, which D-16 bumped.
//   * [`crate::db::seal_legacy_limit_keys`]: the ONE place that rewrites a legacy row, called by the config
//     loader. That is what makes the migration automatic; a reader never writes.

/// The at-rest form of a `limit_role.matching_key` value: sealed, or `None` for SQL NULL.
fn seal_limit_key(kp: &dyn KeyProvider, key: Option<&str>) -> Result<Option<String>, sqlx::Error> {
    match key {
        None => Ok(None),
        Some(k) => Ok(Some(
            kp.seal(k.as_bytes()).map_err(crypto_to_sqlx)?.to_text(),
        )),
    }
}

/// The in-memory form of a stored `limit_role.matching_key` value.
///
/// The whole policy lives in [`crate::crypto::Sealed::open_text`] and is NOT re-derived here: the
/// config tree carries the same values in the same text form, and two copies of "is this sealed?"
/// is how the two would drift apart.
pub(crate) fn open_limit_key(
    kp: &dyn KeyProvider,
    stored: Option<String>,
) -> Result<Option<String>, sqlx::Error> {
    match stored {
        None => Ok(None),
        Some(value) => Sealed::open_text(kp, &value)
            .map(Some)
            .map_err(crypto_to_sqlx),
    }
}

pub async fn insert_limit_role(
    pool: &Pool,
    kp: &dyn KeyProvider,
    r: &LimitRole,
) -> Result<(), sqlx::Error> {
    let matching_key = seal_limit_key(kp, r.matching_key.as_deref())?;
    let mut tx = begin_write(pool).await?;
    let affected = sqlx::query(&dialect::sql(
        "INSERT INTO limit_role (id, name, matching_key, matching_model, matching_tenant, \
         matching_provider, limit_count, limit_token, limit_window, enabled, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    ))
    .bind(&r.id)
    .bind(&r.name)
    .bind(matching_key)
    .bind(&r.matching_model)
    .bind(&r.matching_tenant)
    .bind(&r.matching_provider)
    .bind(r.limit_count)
    .bind(r.limit_token)
    .bind(&r.window)
    .bind(flag(r.enabled))
    .bind(parse_ts(&r.created_at)?)
    .execute(&mut *tx)
    .await?;
    commit_write(tx, affected.rows_affected() > 0).await
}

pub async fn get_limit_role(
    pool: &Pool,
    kp: &dyn KeyProvider,
    id: &str,
) -> Result<LimitRole, sqlx::Error> {
    let row = sqlx::query_as::<_, LimitRoleRow>(&dialect::sql(
        r#"SELECT id, name, matching_key, matching_model, matching_tenant,
                  matching_provider, limit_count, limit_token, limit_window, enabled, created_at
           FROM limit_role WHERE id = ?"#,
    ))
    .bind(id)
    .fetch_one(pool)
    .await?;
    row.to_model(kp)
}

pub async fn list_limit_roles(
    pool: &Pool,
    kp: &dyn KeyProvider,
) -> Result<Vec<LimitRole>, sqlx::Error> {
    list_limit_roles_on(pool, kp).await
}

/// [`list_limit_roles`] on an arbitrary executor, so the replication content can
/// read it inside ONE transaction (see `cluster::content::ReplicationContent::load`).
///
/// NOTE for that caller: this function only READS. The legacy re-seal is
/// [`crate::db::seal_legacy_limit_keys`], which opens its own write transaction, precisely because this one is
/// called inside a plain (deferred) read transaction — a write here would have to take the write
/// lock mid-read and would fail a node whose only sin was reading while an admin write was in
/// flight.
pub(crate) async fn list_limit_roles_on<'e, E>(
    exec: E,
    kp: &dyn KeyProvider,
) -> Result<Vec<LimitRole>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Db>,
{
    let rows = sqlx::query_as::<_, LimitRoleRow>(&dialect::sql(
        r#"SELECT id, name, matching_key, matching_model, matching_tenant,
                  matching_provider, limit_count, limit_token, limit_window, enabled, created_at
           FROM limit_role ORDER BY created_at, id"#,
    ))
    .fetch_all(exec)
    .await?;
    rows.iter().map(|r| r.to_model(kp)).collect()
}

/// The rows that still hold a PLAINTEXT `matching_key` — exactly what [`crate::db::seal_legacy_limit_keys`] will
/// rewrite.
///
/// Split out from the migration on purpose: the migration's READ is one plain statement on the pool
/// and its WRITE is a compare-and-swap inside a write transaction, and the property that matters
/// (a row that changed in between is not clobbered) only exists in the gap between them. A test that
/// wants to model that gap has to be able to hold the two halves apart — otherwise it ends up
/// asserting a COPY of the UPDATE text, which stays green after the production clause is deleted.
pub(super) async fn legacy_limit_key_rows(
    pool: &Pool,
) -> Result<Vec<(String, String)>, sqlx::Error> {
    let rows: Vec<(String, String)> = sqlx::query_as(&dialect::sql(
        "SELECT id, matching_key FROM limit_role WHERE matching_key IS NOT NULL ORDER BY id",
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter(|(_, value)| Sealed::from_text(value).is_none())
        .collect())
}

/// Seal ONE legacy row: `stale` is the plaintext the reader saw, `fresh` its envelope. Returns the
/// number of rows actually rewritten (0 or 1) — 0 when the row changed, was deleted, or was sealed by
/// another node in the meantime, none of which is an error.
pub(super) async fn reseal_limit_key_row<'e, E>(
    exec: E,
    id: &str,
    stale: &str,
    fresh: &str,
) -> Result<u64, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Db>,
{
    let done = sqlx::query(&dialect::sql(
        "UPDATE limit_role SET matching_key = ? WHERE id = ? AND matching_key = ?",
    ))
    .bind(fresh)
    .bind(id)
    .bind(stale)
    .execute(exec)
    .await?;
    Ok(done.rows_affected())
}

/// Update a limit role's mutable fields.
pub async fn update_limit_role(
    pool: &Pool,
    kp: &dyn KeyProvider,
    r: &LimitRole,
) -> Result<(), sqlx::Error> {
    let matching_key = seal_limit_key(kp, r.matching_key.as_deref())?;
    let mut tx = begin_write(pool).await?;
    let affected = sqlx::query(&dialect::sql(
        "UPDATE limit_role SET name = ?, matching_key = ?, matching_model = ?, \
         matching_tenant = ?, matching_provider = ?, limit_count = ?, limit_token = ?, \
         limit_window = ?, enabled = ? WHERE id = ?",
    ))
    .bind(&r.name)
    .bind(matching_key)
    .bind(&r.matching_model)
    .bind(&r.matching_tenant)
    .bind(&r.matching_provider)
    .bind(r.limit_count)
    .bind(r.limit_token)
    .bind(&r.window)
    .bind(flag(r.enabled))
    .bind(&r.id)
    .execute(&mut *tx)
    .await?;
    commit_write(tx, affected.rows_affected() > 0).await
}

pub async fn delete_limit_role(pool: &Pool, id: &str) -> Result<(), sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let affected = sqlx::query(&dialect::sql("DELETE FROM limit_role WHERE id = ?"))
        .bind(id)
        .execute(&mut *tx)
        .await?;
    commit_write(tx, affected.rows_affected() > 0).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{StaticKeyProvider, KEY_LEN};
    use crate::db::{reseal::seal_legacy_limit_keys, testing};

    const LEGACY: &str = "sk-legacy-plaintext-key";

    /// Write a row the way the code did BEFORE decision D-16: plaintext, straight into the column.
    /// (`insert_limit_role` seals, so the legacy state cannot be produced through it any more.)
    async fn insert_plaintext_role(pool: &Pool, id: &str, key: &str) {
        sqlx::query(&dialect::sql(
            "INSERT INTO limit_role (id, name, matching_key, limit_count, limit_window, enabled) \
             VALUES (?, ?, ?, 600, 'm', 1)",
        ))
        .bind(id)
        .bind(id)
        .bind(key)
        .execute(pool)
        .await
        .expect("insert a pre-D-16 plaintext row");
    }

    async fn stored(pool: &Pool, id: &str) -> Option<String> {
        let row: Option<(Option<String>,)> = sqlx::query_as(&dialect::sql(
            "SELECT matching_key FROM limit_role WHERE id = ?",
        ))
        .bind(id)
        .fetch_optional(pool)
        .await
        .expect("select");
        row.and_then(|r| r.0)
    }

    /// The legacy re-seal is a COMPARE-AND-SWAP, driven here through BOTH production halves in the
    /// order the race happens: read the pending row, let the row change, then write the stale
    /// envelope.
    ///
    /// Why a unit test and not an integration one: this is the ONLY way to hold the two halves apart,
    /// and the first version of this test wrote the CAS statement out by hand — it stayed GREEN when
    /// the production clause was deleted, i.e. it was asserting a copy of the code. The helper split
    /// exists for this test (`legacy_limit_key_rows` + `reseal_limit_key_row`).
    ///
    /// Falsification: drop `AND matching_key = ?` from `reseal_limit_key_row` and the first two
    /// assertions fail — the stale envelope wins and the newer key is silently destroyed.
    #[tokio::test]
    async fn the_legacy_reseal_refuses_a_row_that_changed_after_the_read() {
        let pool = testing::setup_pool().await;
        let kp = StaticKeyProvider::new([3u8; KEY_LEN], 1);
        insert_plaintext_role(&pool, "r-cas", LEGACY).await;

        // 1) the migration's READ.
        let pending = legacy_limit_key_rows(&pool)
            .await
            .expect("read the pending rows");
        assert_eq!(
            pending,
            vec![("r-cas".to_string(), LEGACY.to_string())],
            "fixture: the row must be pending"
        );

        // 2) the row changes underneath it — an admin write, or another node sealing it first.
        let newer = kp.seal(b"sk-written-in-between").expect("seal").to_text();
        sqlx::query(&dialect::sql(
            "UPDATE limit_role SET matching_key = ? WHERE id = ?",
        ))
        .bind(&newer)
        .bind("r-cas")
        .execute(&pool)
        .await
        .expect("the concurrent write");

        // 3) the migration's WRITE, carrying the value it read.
        let stale = kp.seal(LEGACY.as_bytes()).expect("seal").to_text();
        let mut tx = begin_write(&pool).await.expect("tx");
        let rewritten = reseal_limit_key_row(&mut *tx, "r-cas", LEGACY, &stale)
            .await
            .expect("compare-and-swap");
        tx.commit().await.expect("commit");

        assert_eq!(
            rewritten, 0,
            "a row whose value changed after the read must NOT be rewritten (the CAS is what \
             makes the migration safe to run on a live node)"
        );
        assert_eq!(
            stored(&pool, "r-cas").await.as_deref(),
            Some(newer.as_str()),
            "the newer key must survive the migration's stale read"
        );
    }

    /// ...and the same CAS still rewrites a row that did NOT change, so "refuse everything" would not
    /// pass the test above. This one goes through the full production entry point.
    ///
    /// Falsification: return 0 from `reseal_limit_key_row` unconditionally (the lazy way to pass the
    /// test above) and this fails.
    #[tokio::test]
    async fn the_legacy_reseal_rewrites_a_row_that_did_not_change() {
        let pool = testing::setup_pool().await;
        let kp = StaticKeyProvider::new([3u8; KEY_LEN], 1);
        insert_plaintext_role(&pool, "r-still", LEGACY).await;

        assert_eq!(
            seal_legacy_limit_keys(&pool, &kp).await.expect("migrate"),
            1,
            "the one plaintext row must be rewritten"
        );
        let after = stored(&pool, "r-still").await.expect("still has a key");
        assert!(
            after.starts_with(crate::crypto::SEALED_TEXT_PREFIX) && !after.contains(LEGACY),
            "the column must hold an envelope afterwards: {after}"
        );
        assert_eq!(
            open_limit_key(&kp, Some(after)).expect("open").as_deref(),
            Some(LEGACY),
            "…and the key itself must come back"
        );
        assert_eq!(
            seal_legacy_limit_keys(&pool, &kp)
                .await
                .expect("second pass"),
            0,
            "a sealed database needs no write at all"
        );
    }
}
