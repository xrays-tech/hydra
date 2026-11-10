//! Provider, provider-model, and provider-key CRUD.

use hydra_core::model::{Provider, ProviderKey, ProviderModel};

use crate::crypto::KeyProvider;

use super::dialect;
use super::pool::{begin_write, commit_write};
use super::rows::{crypto_to_sqlx, parse_ts, ProviderKeyRow, ProviderModelRow, ProviderRow};
use super::{Db, Pool};

// ---------------------------------------------------------------------------
// CRUD — Provider
// ---------------------------------------------------------------------------

/// Insert a provider. Violating the UNIQUE `provider_code` constraint returns
/// [`sqlx::Error::Database`] (UNIQUE violation).
pub async fn insert_provider(pool: &Pool, p: &Provider) -> Result<(), sqlx::Error> {
    let max_concurrency = p.max_concurrency.map(|v| v as i64);
    let max_queue_depth = p.max_queue_depth.map(|v| v as i64);
    let queue_wait_timeout_ms = p.queue_wait_timeout_ms.map(|v| v as i64);
    let mut tx = begin_write(pool).await?;
    let affected = sqlx::query(&dialect::sql(
        "INSERT INTO provider (id, provider_code, name, endpoint, weight, created_at, updated_at, \
         max_concurrency, max_queue_depth, queue_wait_timeout_ms) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    ))
    .bind(&p.id)
    .bind(&p.key)
    .bind(&p.name)
    .bind(&p.endpoint)
    .bind(i64::from(p.weight))
    .bind(parse_ts(&p.created_at)?)
    .bind(parse_ts(&p.updated_at)?)
    .bind(max_concurrency)
    .bind(max_queue_depth)
    .bind(queue_wait_timeout_ms)
    .execute(&mut *tx)
    .await?;
    commit_write(tx, affected.rows_affected() > 0).await
}

/// Fetch a single provider by id; `Err(RowNotFound)` when absent.
pub async fn get_provider(pool: &Pool, id: &str) -> Result<Provider, sqlx::Error> {
    let row = sqlx::query_as::<_, ProviderRow>(&dialect::sql(
        r#"SELECT id, provider_code, name, endpoint, weight, created_at, updated_at,
                  max_concurrency, max_queue_depth, queue_wait_timeout_ms
           FROM provider WHERE id = ?"#,
    ))
    .bind(id)
    .fetch_one(pool)
    .await?;
    Ok(row.into())
}

/// All providers, ordered by `created_at`.
pub async fn list_providers(pool: &Pool) -> Result<Vec<Provider>, sqlx::Error> {
    list_providers_on(pool).await
}

/// [`list_providers`] on an arbitrary executor (one-transaction read).
pub(crate) async fn list_providers_on<'e, E>(exec: E) -> Result<Vec<Provider>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Db>,
{
    let rows = sqlx::query_as::<_, ProviderRow>(&dialect::sql(
        r#"SELECT id, provider_code, name, endpoint, weight, created_at, updated_at,
                  max_concurrency, max_queue_depth, queue_wait_timeout_ms
           FROM provider ORDER BY created_at"#,
    ))
    .fetch_all(exec)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Update a provider's mutable fields.
pub async fn update_provider(pool: &Pool, p: &Provider) -> Result<(), sqlx::Error> {
    let max_concurrency = p.max_concurrency.map(|v| v as i64);
    let max_queue_depth = p.max_queue_depth.map(|v| v as i64);
    let queue_wait_timeout_ms = p.queue_wait_timeout_ms.map(|v| v as i64);
    let mut tx = begin_write(pool).await?;
    let affected = sqlx::query(&dialect::sql(
        "UPDATE provider SET provider_code = ?, name = ?, endpoint = ?, weight = ?, updated_at = ?, \
         max_concurrency = ?, max_queue_depth = ?, queue_wait_timeout_ms = ? \
         WHERE id = ?",
    ))
    .bind(&p.key)
    .bind(&p.name)
    .bind(&p.endpoint)
    .bind(i64::from(p.weight))
    .bind(parse_ts(&p.updated_at)?)
    .bind(max_concurrency)
    .bind(max_queue_depth)
    .bind(queue_wait_timeout_ms)
    .bind(&p.id)
    .execute(&mut *tx)
    .await?;
    commit_write(tx, affected.rows_affected() > 0).await
}

/// Delete a provider (CASCADE removes its models/keys and tenant_provider links).
pub async fn delete_provider(pool: &Pool, id: &str) -> Result<(), sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let affected = sqlx::query(&dialect::sql("DELETE FROM provider WHERE id = ?"))
        .bind(id)
        .execute(&mut *tx)
        .await?;
    commit_write(tx, affected.rows_affected() > 0).await
}

// ---------------------------------------------------------------------------
// CRUD — ProviderModel
// ---------------------------------------------------------------------------

/// Insert a provider model. The CHECK (`status IN (1,0,-1)`) and
/// UNIQUE(`model_key`,`provider_id`) constraints are enforced by the database.
pub async fn insert_provider_model(pool: &Pool, m: &ProviderModel) -> Result<(), sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let affected = sqlx::query(&dialect::sql(
        "INSERT INTO provider_model (id, model_key, name, provider_id, status) VALUES (?, ?, ?, ?, ?)",
    ))
    .bind(&m.id)
    .bind(&m.key)
    .bind(&m.name)
    .bind(&m.provider_id)
    .bind(i64::from(m.status))
    .execute(&mut *tx)
    .await?;
    commit_write(tx, affected.rows_affected() > 0).await
}

pub async fn get_provider_model(pool: &Pool, id: &str) -> Result<ProviderModel, sqlx::Error> {
    let row = sqlx::query_as::<_, ProviderModelRow>(&dialect::sql(
        r#"SELECT id, model_key, name, provider_id, status
           FROM provider_model WHERE id = ?"#,
    ))
    .bind(id)
    .fetch_one(pool)
    .await?;
    Ok(row.into())
}

pub async fn list_provider_models(pool: &Pool) -> Result<Vec<ProviderModel>, sqlx::Error> {
    list_provider_models_on(pool).await
}

/// [`list_provider_models`] on an arbitrary executor (one-transaction read).
pub(crate) async fn list_provider_models_on<'e, E>(
    exec: E,
) -> Result<Vec<ProviderModel>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Db>,
{
    let rows = sqlx::query_as::<_, ProviderModelRow>(&dialect::sql(
        r#"SELECT id, model_key, name, provider_id, status
           FROM provider_model ORDER BY model_key, provider_id"#,
    ))
    .fetch_all(exec)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Update a provider model's mutable fields (`model_key`, `name`, `status`).
pub async fn update_provider_model(pool: &Pool, m: &ProviderModel) -> Result<(), sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let affected = sqlx::query(&dialect::sql(
        "UPDATE provider_model SET model_key = ?, name = ?, status = ? WHERE id = ?",
    ))
    .bind(&m.key)
    .bind(&m.name)
    .bind(i64::from(m.status))
    .bind(&m.id)
    .execute(&mut *tx)
    .await?;
    commit_write(tx, affected.rows_affected() > 0).await
}

pub async fn delete_provider_model(pool: &Pool, id: &str) -> Result<(), sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let affected = sqlx::query(&dialect::sql("DELETE FROM provider_model WHERE id = ?"))
        .bind(id)
        .execute(&mut *tx)
        .await?;
    commit_write(tx, affected.rows_affected() > 0).await
}

pub async fn insert_provider_key(
    pool: &Pool,
    kp: &dyn KeyProvider,
    k: &ProviderKey,
) -> Result<(), sqlx::Error> {
    let sealed = kp.seal(k.api_key.as_bytes()).map_err(crypto_to_sqlx)?;
    let nonce: &[u8] = &sealed.nonce;
    let mut tx = begin_write(pool).await?;
    let affected = sqlx::query(&dialect::sql(
        "INSERT INTO provider_key (id, provider_id, api_key_ciphertext, api_key_nonce, \
         key_version, created_at) VALUES (?, ?, ?, ?, ?, ?)",
    ))
    .bind(&k.id)
    .bind(&k.provider_id)
    .bind(sealed.ciphertext)
    .bind(nonce)
    .bind(i64::from(sealed.key_version))
    .bind(parse_ts(&k.created_at)?)
    .execute(&mut *tx)
    .await?;
    commit_write(tx, affected.rows_affected() > 0).await
}

/// Atomic provider-key upsert, used by `PUT /api/v1/provider-keys/{id}`.
///
/// The admin PUT was a bare `DELETE` + `INSERT` with the delete error thrown
/// away. `provider_key.provider_id` is a `REFERENCES provider(id)` FK, so a
/// body naming a provider that does not exist made the **insert** fail *after*
/// the old, working key had already been deleted: the API answered 400/4xx, the
/// operator believed the update had simply failed, and the provider was left
/// with no key at all (`NoAvailableKey` 503) until someone noticed (audit
/// §3.15).
///
/// Both statements now run in one transaction: a failing insert rolls the delete
/// back and the previous key survives intact. Reuses the exact SQL text of
/// `insert_provider_key` / `delete_provider_key`, so the two paths cannot drift.
pub async fn upsert_provider_key(
    pool: &Pool,
    kp: &dyn KeyProvider,
    k: &ProviderKey,
) -> Result<(), sqlx::Error> {
    let sealed = kp.seal(k.api_key.as_bytes()).map_err(crypto_to_sqlx)?;
    let nonce: &[u8] = &sealed.nonce;
    let mut tx = begin_write(pool).await?;
    let deleted = sqlx::query(&dialect::sql("DELETE FROM provider_key WHERE id = ?"))
        .bind(&k.id)
        .execute(&mut *tx)
        .await?;
    let inserted = sqlx::query(&dialect::sql(
        "INSERT INTO provider_key (id, provider_id, api_key_ciphertext, api_key_nonce, \
         key_version, created_at) VALUES (?, ?, ?, ?, ?, ?)",
    ))
    .bind(&k.id)
    .bind(&k.provider_id)
    .bind(sealed.ciphertext)
    .bind(nonce)
    .bind(i64::from(sealed.key_version))
    .bind(parse_ts(&k.created_at)?)
    .execute(&mut *tx)
    .await?;
    commit_write(
        tx,
        deleted.rows_affected() > 0 || inserted.rows_affected() > 0,
    )
    .await
}

pub async fn get_provider_key(
    pool: &Pool,
    kp: &dyn KeyProvider,
    id: &str,
) -> Result<ProviderKey, sqlx::Error> {
    let row = sqlx::query_as::<_, ProviderKeyRow>(&dialect::sql(
        r#"SELECT id, provider_id, created_at, api_key_ciphertext, api_key_nonce,
                  key_version
           FROM provider_key WHERE id = ?"#,
    ))
    .bind(id)
    .fetch_one(pool)
    .await?;
    row.to_model(kp).map_err(crypto_to_sqlx)
}

/// All provider keys (the loader groups them by `provider_id`).
pub async fn list_provider_keys(
    pool: &Pool,
    kp: &dyn KeyProvider,
) -> Result<Vec<ProviderKey>, sqlx::Error> {
    list_provider_keys_on(pool, kp).await
}

/// [`list_provider_keys`] on an arbitrary executor (one-transaction read).
pub(crate) async fn list_provider_keys_on<'e, E>(
    exec: E,
    kp: &dyn KeyProvider,
) -> Result<Vec<ProviderKey>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Db>,
{
    let rows = sqlx::query_as::<_, ProviderKeyRow>(&dialect::sql(
        r#"SELECT id, provider_id, created_at, api_key_ciphertext, api_key_nonce,
                  key_version
           FROM provider_key ORDER BY provider_id, created_at, id"#,
    ))
    .fetch_all(exec)
    .await?;
    rows.iter()
        .map(|r| r.to_model(kp).map_err(crypto_to_sqlx))
        .collect()
}

/// Keys belonging to a single provider (ordered by creation).
pub async fn list_provider_keys_by_provider(
    pool: &Pool,
    kp: &dyn KeyProvider,
    provider_id: &str,
) -> Result<Vec<ProviderKey>, sqlx::Error> {
    let rows = sqlx::query_as::<_, ProviderKeyRow>(&dialect::sql(
        r#"SELECT id, provider_id, created_at, api_key_ciphertext, api_key_nonce,
                  key_version
           FROM provider_key WHERE provider_id = ? ORDER BY created_at"#,
    ))
    .bind(provider_id)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|r| r.to_model(kp).map_err(crypto_to_sqlx))
        .collect()
}

pub async fn delete_provider_key(pool: &Pool, id: &str) -> Result<(), sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let affected = sqlx::query(&dialect::sql("DELETE FROM provider_key WHERE id = ?"))
        .bind(id)
        .execute(&mut *tx)
        .await?;
    commit_write(tx, affected.rows_affected() > 0).await
}
