//! Tenant, tenant certificate, tenant-provider, and tenant-model CRUD.

use hydra_core::model::{Tenant, TenantModel, TenantProvider};

use crate::crypto::KeyProvider;

use super::dialect;
use super::pool::{begin_write, bump_config_revision_on_conn, commit_write};
use super::rows::{
    crypto_to_sqlx, flag, parse_ts, TenantCertRow, TenantModelRow, TenantProviderRow, TenantRow,
};
use super::{Db, Pool};

// ---------------------------------------------------------------------------
// CRUD — Tenant
// ---------------------------------------------------------------------------

/// Insert a tenant.
///
/// Empty/blank `cert_file`/`cert_key` strings are normalized to `NULL`:
/// optional legacy cert paths arrive as `""` from the admin UI (and other
/// clients) when left blank, and an empty string is not a path — persisting
/// it would poison `build_config`/TLS resolution and the legacy cert
/// conversion (see `apply_tenant_cert`).
pub async fn insert_tenant(pool: &Pool, t: &Tenant) -> Result<(), sqlx::Error> {
    let cert_key = non_empty_path(t.cert_key.as_deref());
    let cert_file = non_empty_path(t.cert_file.as_deref());
    let mut tx = begin_write(pool).await?;
    let affected = sqlx::query(&dialect::sql(
        "INSERT INTO tenant (id, name, domain, cert_key, cert_file, enabled, \
         created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    ))
    .bind(&t.id)
    .bind(&t.name)
    .bind(&t.domain)
    .bind(cert_key)
    .bind(cert_file)
    .bind(flag(t.enabled))
    .bind(parse_ts(&t.created_at)?)
    .bind(parse_ts(&t.updated_at)?)
    .execute(&mut *tx)
    .await?;
    commit_write(tx, affected.rows_affected() > 0).await
}

/// Blank certificate paths are stored as `NULL` (empty string is not a path).
fn non_empty_path(p: Option<&str>) -> Option<String> {
    p.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub async fn get_tenant(pool: &Pool, id: &str) -> Result<Tenant, sqlx::Error> {
    let row = sqlx::query_as::<_, TenantRow>(&dialect::sql(
        r#"SELECT id, name, domain, cert_key, cert_file, enabled,
                  created_at, updated_at
           FROM tenant WHERE id = ?"#,
    ))
    .bind(id)
    .fetch_one(pool)
    .await?;
    Ok(row.into())
}

pub async fn list_tenants(pool: &Pool) -> Result<Vec<Tenant>, sqlx::Error> {
    list_tenants_on(pool).await
}

/// [`list_tenants`] on an arbitrary executor (one-transaction read).
pub(crate) async fn list_tenants_on<'e, E>(exec: E) -> Result<Vec<Tenant>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Db>,
{
    let rows = sqlx::query_as::<_, TenantRow>(&dialect::sql(
        r#"SELECT id, name, domain, cert_key, cert_file, enabled,
                  created_at, updated_at
           FROM tenant ORDER BY created_at"#,
    ))
    .fetch_all(exec)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Update a tenant's mutable fields (`name`, `domain`, cert paths,
/// `enabled`, `updated_at`). Blank cert paths are normalized to `NULL`
/// (same rule as `insert_tenant`).
///
/// A missing id is reported as [`sqlx::Error::RowNotFound`] **before** the
/// UPDATE, because `rows_affected` cannot answer "did this row exist?":
/// PostgreSQL reports matched rows, while MySQL reports changed rows by default
/// (unless `CLIENT_FOUND_ROWS` is negotiated). A same-body no-op would therefore
/// look absent on a changed-rows driver; the explicit probe must not depend on
/// that setting. Once the probe hits, the UPDATE's own row count is irrelevant —
/// a no-op still returns `Ok(())`.
///
/// Callers that map DB errors to HTTP must map `RowNotFound` to 404. Note that
/// `admin::handlers::classify_db_err` currently maps it to 500 (pinned by its
/// own test), so do not route a new `update_tenant` caller through that helper
/// without first deciding/updating the contract.
pub async fn update_tenant(pool: &Pool, t: &Tenant) -> Result<(), sqlx::Error> {
    let cert_key = non_empty_path(t.cert_key.as_deref());
    let cert_file = non_empty_path(t.cert_file.as_deref());
    let mut tx = begin_write(pool).await?;
    let existing: Option<(String,)> = sqlx::query_as(&dialect::sql(
        "SELECT id FROM tenant WHERE id = ? FOR UPDATE",
    ))
    .bind(&t.id)
    .fetch_optional(&mut *tx)
    .await?;
    if existing.is_none() {
        return Err(sqlx::Error::RowNotFound);
    }
    let affected = sqlx::query(&dialect::sql(
        "UPDATE tenant SET name = ?, domain = ?, cert_key = ?, cert_file = ?, \
         enabled = ?, updated_at = ? WHERE id = ?",
    ))
    .bind(&t.name)
    .bind(&t.domain)
    .bind(cert_key)
    .bind(cert_file)
    .bind(flag(t.enabled))
    .bind(parse_ts(&t.updated_at)?)
    .bind(&t.id)
    .execute(&mut *tx)
    .await?;
    commit_write(tx, affected.rows_affected() > 0).await
}

/// Everything a tenant write must persist, in ONE transaction.
pub struct TenantWrite<'a> {
    pub tenant: &'a Tenant,
    /// `true` = INSERT (create), `false` = UPDATE (edit).
    pub is_create: bool,
    /// `Some((None, None))` = clear the certificate; `Some((Some(pem), Some(key)))`
    /// = replace it; `None` = leave it alone.
    pub cert: Option<(Option<String>, Option<String>)>,
}

/// Upsert a tenant TOGETHER WITH its secret material, atomically.
///
/// Everything commits at once or not at all. Splitting these steps is the
/// "reported as failed but actually live" defect: the tenant row committed, then
/// the certificate or token write failed, and the API answered 500/400 while the
/// change was already in effect — the operator's natural retry then hits a
/// duplicate or re-applies a credential they were told had failed.
///
/// The post-write `reload` is deliberately NOT part of the transaction: it is
/// recoverable (the next successful reload fixes it) and is reported separately
/// through the response's `snapshot_stale` flag.
///
/// The SQL statements and their parameter types are byte-identical to the
/// single-purpose helpers they replace, so the two paths cannot drift.
pub async fn write_tenant(
    pool: &Pool,
    kp: &dyn KeyProvider,
    w: TenantWrite<'_>,
) -> Result<(), sqlx::Error> {
    let t = w.tenant;
    let cert_key = non_empty_path(t.cert_key.as_deref());
    let cert_file = non_empty_path(t.cert_file.as_deref());
    let mut tx = begin_write(pool).await?;
    let mut changed = w.is_create;

    if w.is_create {
        let affected = sqlx::query(&dialect::sql(
            "INSERT INTO tenant (id, name, domain, cert_key, cert_file, enabled, \
             created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        ))
        .bind(&t.id)
        .bind(&t.name)
        .bind(&t.domain)
        .bind(cert_key)
        .bind(cert_file)
        .bind(flag(t.enabled))
        .bind(parse_ts(&t.created_at)?)
        .bind(parse_ts(&t.updated_at)?)
        .execute(&mut *tx)
        .await?;
        changed |= affected.rows_affected() > 0;
    } else {
        // Same existence probe as `update_tenant`, INSIDE this transaction:
        // `rows_affected` cannot answer "did this row exist?" (PG reports
        // matched rows, MySQL changed rows), so a same-body no-op must not look
        // absent. Missing id → RowNotFound; the caller maps it to 404.
        let existing: Option<(String,)> = sqlx::query_as(&dialect::sql(
            "SELECT id FROM tenant WHERE id = ? FOR UPDATE",
        ))
        .bind(&t.id)
        .fetch_optional(&mut *tx)
        .await?;
        if existing.is_none() {
            return Err(sqlx::Error::RowNotFound);
        }
        let affected = sqlx::query(&dialect::sql(
            "UPDATE tenant SET name = ?, domain = ?, cert_key = ?, cert_file = ?, \
             enabled = ?, updated_at = ? WHERE id = ?",
        ))
        .bind(&t.name)
        .bind(&t.domain)
        .bind(cert_key)
        .bind(cert_file)
        .bind(flag(t.enabled))
        .bind(parse_ts(&t.updated_at)?)
        .bind(&t.id)
        .execute(&mut *tx)
        .await?;
        changed |= affected.rows_affected() > 0;
    }

    if let Some((cert_pem, cert_key_pem)) = &w.cert {
        let (ct, nonce, version) = match cert_key_pem {
            Some(pem) => {
                let sealed = kp.seal(pem.as_bytes()).map_err(crypto_to_sqlx)?;
                (
                    Some(sealed.ciphertext),
                    Some(sealed.nonce.to_vec()),
                    Some(sealed.key_version as i64),
                )
            }
            None => (None, None, None),
        };
        let affected = sqlx::query(&dialect::sql(
            "UPDATE tenant SET cert_pem = ?, cert_key_ciphertext = ?, cert_key_nonce = ?, \
             cert_key_version = ? WHERE id = ?",
        ))
        .bind(cert_pem)
        .bind(ct)
        .bind(nonce)
        .bind(version)
        .bind(&t.id)
        .execute(&mut *tx)
        .await?;
        changed |= affected.rows_affected() > 0;
    }

    commit_write(tx, changed).await
}

pub async fn delete_tenant(pool: &Pool, id: &str) -> Result<(), sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    #[cfg(feature = "db-mysql")]
    let affected = {
        // MySQL 8.0 can answer a server-side prepared `DELETE FROM tenant` with
        // `ER_NEED_REPREPARE` (1615) once the cascading child foreign keys are
        // in the way. Try the normal BOUND statement first; `.persistent(false)`
        // closes the server-side statement after execution, so the next delete
        // cannot reuse stale metadata. A 1615 is raised before execution, so a
        // fallback retry has no double-delete hazard.
        let result = sqlx::query(&dialect::sql("DELETE FROM tenant WHERE id = ?"))
            .bind(id)
            .persistent(false)
            .execute(&mut *tx)
            .await;
        match result {
            Ok(affected) => affected,
            // ONLY 1615 takes the text-protocol fallback. Its id literal is
            // built from the runtime bytes (injection-safe); CONVERT plus the
            // schema collation keep the comparison textual, not raw binary.
            Err(e) if is_mysql_need_reprepare(&e) => delete_tenant_fallback(&mut tx, id).await?,
            Err(e) => return Err(e),
        }
    };
    #[cfg(feature = "db-postgres")]
    let affected = sqlx::query(&dialect::sql("DELETE FROM tenant WHERE id = ?"))
        .bind(id)
        .execute(&mut *tx)
        .await?;
    commit_write(tx, affected.rows_affected() > 0).await
}

/// MySQL `ER_NEED_REPREPARE` (error number 1615).
///
/// SQLx exposes MySQL's SQLSTATE through `DatabaseError::code()`, but 1615's
/// SQLSTATE is the generic `HY000`, so the numeric error is the precise
/// discriminator. Consult the SQLSTATE only when the driver cannot surface a
/// number.
#[cfg(any(feature = "db-mysql", test))]
fn mysql_need_reprepare(code: Option<&str>, number: Option<u16>) -> bool {
    match number {
        Some(n) => n == 1615,
        None => code == Some("HY000"),
    }
}

/// True only for MySQL 1615 (`ER_NEED_REPREPARE`).
#[cfg(feature = "db-mysql")]
fn is_mysql_need_reprepare(err: &sqlx::Error) -> bool {
    let Some(db) = err.as_database_error() else {
        return false;
    };
    let code = db.code();
    let number = db
        .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
        .map(sqlx::mysql::MySqlDatabaseError::number);
    mysql_need_reprepare(code.as_deref(), number)
}

/// Text-protocol retry used only when a bound `DELETE FROM tenant` hits 1615.
///
/// The id is rendered as a hex literal from its bytes, so it needs no quoting
/// and cannot break out of the SQL. `CONVERT(... USING utf8mb4)` turns the
/// BINARY literal into utf8mb4 text; `COLLATE utf8mb4_unicode_ci` names the
/// collation the MySQL migrations declare for these columns. Without the
/// explicit collation, `CONVERT`'s result is IMPLICIT with the server default
/// (often `utf8mb4_0900_ai_ci`) and MySQL raises 1267 against the
/// `utf8mb4_unicode_ci` column.
#[cfg(feature = "db-mysql")]
async fn delete_tenant_fallback(
    tx: &mut super::Tx<'static>,
    id: &str,
) -> Result<sqlx::mysql::MySqlQueryResult, sqlx::Error> {
    let hex: String = id.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
    let sql = format!(
        "DELETE FROM tenant WHERE id = CONVERT(X'{hex}' USING utf8mb4) \
         COLLATE utf8mb4_unicode_ci"
    );
    sqlx::Executor::execute(&mut **tx, sqlx::raw_sql(&sql)).await
}

// ---------------------------------------------------------------------------
// Tenant certificate content (migration 0007, cluster P0a)
// ---------------------------------------------------------------------------
//
// The cert is the tenant row itself: `cert_pem` (public, plaintext) +
// `cert_key_*` (AES-256-GCM sealed with the same master key as provider
// api-keys). The in-memory [`TenantCert`] carries the private key as
// plaintext only — it is decrypted at the DB boundary and never persisted or
// serialised.

/// Certificate content for one tenant, plaintext in memory.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantCert {
    pub tenant_id: String,
    /// Public cert PEM (`None` = no cert configured).
    pub cert_pem: Option<String>,
    /// Private key PEM, plaintext in memory only.
    pub cert_key_pem: Option<String>,
}

/// Fetch one tenant's certificate content (`None` when the tenant is absent).
pub async fn get_tenant_cert(
    pool: &Pool,
    kp: &dyn KeyProvider,
    tenant_id: &str,
) -> Result<Option<TenantCert>, sqlx::Error> {
    let row = sqlx::query_as::<_, TenantCertRow>(&dialect::sql(
        r#"SELECT id AS tenant_id, cert_pem, cert_key_ciphertext, cert_key_nonce,
                  cert_key_version
           FROM tenant WHERE id = ?"#,
    ))
    .bind(tenant_id)
    .fetch_optional(pool)
    .await?;
    row.map(|r| r.to_model(kp)).transpose()
}

/// All tenants' certificate content (the loader overlays this onto the
/// path-based `certs` map, content-first).
pub async fn list_tenant_certs(
    pool: &Pool,
    kp: &dyn KeyProvider,
) -> Result<Vec<TenantCert>, sqlx::Error> {
    list_tenant_certs_on(pool, kp).await
}

/// [`list_tenant_certs`] on an arbitrary executor (one-transaction read).
pub(crate) async fn list_tenant_certs_on<'e, E>(
    exec: E,
    kp: &dyn KeyProvider,
) -> Result<Vec<TenantCert>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Db>,
{
    let rows = sqlx::query_as::<_, TenantCertRow>(&dialect::sql(
        r#"SELECT id AS tenant_id, cert_pem, cert_key_ciphertext, cert_key_nonce,
                  cert_key_version
           FROM tenant ORDER BY id"#,
    ))
    .fetch_all(exec)
    .await?;
    rows.iter().map(|r| r.to_model(kp)).collect()
}

/// Write a tenant's certificate content. `None` fields clear the columns
/// (explicit cert removal). The private key is sealed at the DB boundary —
/// the caller passes plaintext, this function persists ciphertext only.
pub async fn update_tenant_cert(
    pool: &Pool,
    kp: &dyn KeyProvider,
    cert: &TenantCert,
) -> Result<(), sqlx::Error> {
    let (ct, nonce, version) = match &cert.cert_key_pem {
        Some(pem) => {
            let sealed = kp.seal(pem.as_bytes()).map_err(crypto_to_sqlx)?;
            (
                Some(sealed.ciphertext),
                Some(sealed.nonce.to_vec()),
                Some(sealed.key_version as i64),
            )
        }
        None => (None, None, None),
    };
    let mut tx = begin_write(pool).await?;
    let affected = sqlx::query(&dialect::sql(
        "UPDATE tenant SET cert_pem = ?, cert_key_ciphertext = ?, cert_key_nonce = ?, \
         cert_key_version = ? WHERE id = ?",
    ))
    .bind(&cert.cert_pem)
    .bind(ct)
    .bind(nonce)
    .bind(version)
    .bind(&cert.tenant_id)
    .execute(&mut *tx)
    .await?;
    let changed = affected.rows_affected();
    if changed > 0 {
        bump_config_revision_on_conn(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// CRUD — TenantProvider
// ---------------------------------------------------------------------------

pub async fn insert_tenant_provider(pool: &Pool, tp: &TenantProvider) -> Result<(), sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let affected = sqlx::query(&dialect::sql(
        "INSERT INTO tenant_provider (id, tenant_id, provider_id) VALUES (?, ?, ?)",
    ))
    .bind(&tp.id)
    .bind(&tp.tenant_id)
    .bind(&tp.provider_id)
    .execute(&mut *tx)
    .await?;
    commit_write(tx, affected.rows_affected() > 0).await
}

pub async fn get_tenant_provider(pool: &Pool, id: &str) -> Result<TenantProvider, sqlx::Error> {
    let row = sqlx::query_as::<_, TenantProviderRow>(&dialect::sql(
        r#"SELECT id, tenant_id, provider_id
           FROM tenant_provider WHERE id = ?"#,
    ))
    .bind(id)
    .fetch_one(pool)
    .await?;
    Ok(row.into())
}

pub async fn list_tenant_providers(pool: &Pool) -> Result<Vec<TenantProvider>, sqlx::Error> {
    list_tenant_providers_on(pool).await
}

/// [`list_tenant_providers`] on an arbitrary executor (one-transaction read).
pub(crate) async fn list_tenant_providers_on<'e, E>(
    exec: E,
) -> Result<Vec<TenantProvider>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Db>,
{
    let rows = sqlx::query_as::<_, TenantProviderRow>(&dialect::sql(
        r#"SELECT id, tenant_id, provider_id
           FROM tenant_provider ORDER BY tenant_id, provider_id"#,
    ))
    .fetch_all(exec)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

pub async fn delete_tenant_provider(pool: &Pool, id: &str) -> Result<(), sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let affected = sqlx::query(&dialect::sql("DELETE FROM tenant_provider WHERE id = ?"))
        .bind(id)
        .execute(&mut *tx)
        .await?;
    commit_write(tx, affected.rows_affected() > 0).await
}

// ---------------------------------------------------------------------------
// CRUD — TenantModel
// ---------------------------------------------------------------------------

pub async fn insert_tenant_model(pool: &Pool, tm: &TenantModel) -> Result<(), sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let affected = sqlx::query(&dialect::sql(
        "INSERT INTO tenant_model (id, tenant_id, model_key) VALUES (?, ?, ?)",
    ))
    .bind(&tm.id)
    .bind(&tm.tenant_id)
    .bind(&tm.model_key)
    .execute(&mut *tx)
    .await?;
    commit_write(tx, affected.rows_affected() > 0).await
}

pub async fn get_tenant_model(pool: &Pool, id: &str) -> Result<TenantModel, sqlx::Error> {
    let row = sqlx::query_as::<_, TenantModelRow>(&dialect::sql(
        r#"SELECT id, tenant_id, model_key
           FROM tenant_model WHERE id = ?"#,
    ))
    .bind(id)
    .fetch_one(pool)
    .await?;
    Ok(row.into())
}

pub async fn list_tenant_models(pool: &Pool) -> Result<Vec<TenantModel>, sqlx::Error> {
    list_tenant_models_on(pool).await
}

/// [`list_tenant_models`] on an arbitrary executor (one-transaction read).
pub(crate) async fn list_tenant_models_on<'e, E>(exec: E) -> Result<Vec<TenantModel>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Db>,
{
    let rows = sqlx::query_as::<_, TenantModelRow>(&dialect::sql(
        r#"SELECT id, tenant_id, model_key
           FROM tenant_model ORDER BY tenant_id, model_key"#,
    ))
    .fetch_all(exec)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

pub async fn delete_tenant_model(pool: &Pool, id: &str) -> Result<(), sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let affected = sqlx::query(&dialect::sql("DELETE FROM tenant_model WHERE id = ?"))
        .bind(id)
        .execute(&mut *tx)
        .await?;
    commit_write(tx, affected.rows_affected() > 0).await
}

#[cfg(test)]
mod tests {
    use super::mysql_need_reprepare;

    #[test]
    fn need_reprepare_matches_mysql_error_number() {
        assert!(mysql_need_reprepare(Some("HY000"), Some(1615)));
        assert!(mysql_need_reprepare(None, Some(1615)));
        assert!(!mysql_need_reprepare(Some("HY000"), Some(1062)));
        assert!(!mysql_need_reprepare(Some("HY000"), Some(1451)));
    }

    #[test]
    fn need_reprepare_code_fallback_is_limited_to_hy000() {
        assert!(mysql_need_reprepare(Some("HY000"), None));
        assert!(!mysql_need_reprepare(Some("42S02"), None));
        assert!(!mysql_need_reprepare(None, None));
    }
}
