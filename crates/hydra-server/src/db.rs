//! Backend-selected sqlx pool, migrations, transaction primitives, error
//! classification, and the repo CRUD layer over runtime-checked queries.
//!
//! There is no compile-time SQL validation any more: the SQLite `.sqlx/` offline
//! cache was tied to one dialect, so every statement is built at runtime and
//! passed through [`dialect::sql`] for placeholder rewriting. PostgreSQL sees
//! `$1..$n`; MySQL sees `?`.
//!
//! Repo functions map the portable `BIGINT` columns onto `i64` in the row
//! structs and cast to the idiomatic `i32` / `bool` used by
//! [`hydra_core::model`] at the boundary. DB-native timestamps decode into
//! `chrono::NaiveDateTime` and are formatted back into the existing entity
//! string shape at that same boundary.

// ---------------------------------------------------------------------------
// WRITE PROTOCOL (Phase 2b-2)
// ---------------------------------------------------------------------------
//
// EVERY mutating entry point below uses `begin_write` (hydra_lock FOR UPDATE),
// bumps `config_revision` in that SAME transaction, and commits through
// `commit_write`/`bump_config_revision_on_conn`. Bump only when the statement
// actually changed a row (or an insert succeeded), so a no-op does not create a
// reload storm. Bulk writers bump ONCE per transaction.
//
// Audited mutators:
//   provider              insert/update/delete
//   provider_model        insert/update/delete
//   provider_key          insert/upsert/delete
//   tenant                insert/update/write_tenant/delete/update_tenant_cert
//   tenant_provider       insert/delete
//   tenant_model          insert/delete
//   limit_role            insert/update/delete (+ startup seal_legacy_limit_keys)
//   user_group            upsert/bulk/delete          (tenant-scoped)
//   hydra_user            upsert/bulk/delete          (tenant-scoped)
//   user_api_key          insert/bulk/delete          (tenant-scoped via parent)
//   tenant_access_token   insert/delete
//   global_role*          upsert/replace/bulk/delete
//   reseal_secrets        rotation
//   set_config_version    absolute revision write (lock + commit, no second bump)
//
// `backfill_legacy_certs` and `seal_legacy_limit_keys` are STARTUP-ONLY; the
// loader is read-only. New writes must be added to this checklist.

pub mod backend;
pub mod dialect;
#[doc(hidden)]
pub mod testing;

pub use backend::{Db, DbConnection, Pool, Tx};

use std::collections::HashMap;

use chrono::{DateTime, NaiveDateTime};
use sqlx::migrate::MigrateError;

use hydra_core::model::{
    BillingStatus, GlobalRole, LimitRole, Provider, ProviderKey, ProviderModel, Tenant,
    TenantAccessToken, TenantModel, TenantProvider, User, UserApiKey, UserGroup,
};

use crate::crypto::{self, CryptoError, KeyProvider, Sealed};

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

/// Connect the selected backend and pin every connection's session timezone to
/// UTC. The shared SQL uses unquoted identifiers throughout, so no session
/// `sql_mode` override is needed on MySQL.
pub async fn init_pool(url: &str) -> Result<Pool, sqlx::Error> {
    let max = max_connections();

    #[cfg(feature = "db-postgres")]
    {
        use sqlx::postgres::PgPoolOptions;
        let pool = PgPoolOptions::new()
            .max_connections(max)
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    sqlx::query(&dialect::sql("SET TIME ZONE 'UTC'"))
                        .execute(&mut *conn)
                        .await?;
                    // A node that loses the DB mid-transaction must not hold
                    // `hydra_lock` until TCP keepalive notices; bound the waits
                    // and the idle-in-transaction window. 60s is far above any
                    // healthy config read/write and still finite on a partition.
                    sqlx::query(&dialect::sql("SET statement_timeout = '60s'"))
                        .execute(&mut *conn)
                        .await?;
                    sqlx::query(&dialect::sql(
                        "SET idle_in_transaction_session_timeout = '60s'",
                    ))
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
        let pool = MySqlPoolOptions::new()
            .max_connections(max)
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    sqlx::query(&dialect::sql("SET SESSION time_zone = '+00:00'"))
                        .execute(&mut *conn)
                        .await?;
                    // Same partition guard as PostgreSQL: a lost connection
                    // must not wait forever on a row lock. MySQL's default is
                    // 50s; 10s keeps the cluster-level write stall bounded.
                    sqlx::query(&dialect::sql("SET SESSION innodb_lock_wait_timeout = 10"))
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
        let _ = (url, max);
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

/// Begin a write transaction: begin, idempotently seed the single lock row, and
/// take `SELECT ... FOR UPDATE` on it. Every read-before-write transaction uses
/// this primitive so writers serialize on the same row across nodes.
pub async fn begin_write(pool: &Pool) -> Result<Tx<'static>, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Bound the wait for `hydra_lock`/row locks, but ONLY in write
    // transactions: a session-wide lock_timeout also applies to sqlx's
    // migration advisory lock and makes concurrent schema provisioning fail
    // with 55P03. `SET LOCAL` is transaction-scoped and leaves migrations alone.
    #[cfg(feature = "db-postgres")]
    sqlx::query(&dialect::sql("SET LOCAL lock_timeout = '5s'"))
        .execute(&mut *tx)
        .await?;

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

/// Format a DB-native timestamp into the admin/entity string shape.
fn fmt_ts(value: NaiveDateTime) -> String {
    value.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn fmt_ts_opt(value: Option<NaiveDateTime>) -> Option<String> {
    value.map(fmt_ts)
}

/// Parse an entity timestamp into the DB-native type. Accepts the two shapes
/// the application already produces (`%Y-%m-%d %H:%M:%S` and RFC3339-Z).
fn parse_ts(value: &str) -> Result<NaiveDateTime, sqlx::Error> {
    if let Ok(v) = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S") {
        return Ok(v);
    }
    if let Ok(v) = DateTime::parse_from_rfc3339(value) {
        return Ok(v.naive_utc());
    }
    NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%SZ")
        .map_err(|e| sqlx::Error::Decode(Box::new(e)))
}

fn parse_ts_opt(value: &Option<String>) -> Result<Option<NaiveDateTime>, sqlx::Error> {
    value.as_deref().map(parse_ts).transpose()
}

/// BIGINT flag encoding (0/1), matching the portable schema and row decoding.
fn flag(value: bool) -> i64 {
    i64::from(value)
}

// ---------------------------------------------------------------------------
// Row structs (mirror the portable column types) + conversions to core entities.
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow, Debug, Clone)]
struct ProviderRow {
    id: String,
    provider_code: String,
    name: String,
    endpoint: String,
    weight: i64,
    created_at: NaiveDateTime,
    updated_at: NaiveDateTime,
    /// Per-provider concurrency admission policy (design-admission-queue §5).
    /// Nullable: NULL ⇒ use ProxyConfig defaults (opt-in). BIGINT → i64.
    max_concurrency: Option<i64>,
    max_queue_depth: Option<i64>,
    queue_wait_timeout_ms: Option<i64>,
}

/// Cast a nullable BIGINT (decoded as `Option<i64>`) to `Option<u32>`,
/// rejecting negative values (defensive — a manual DB edit should not panic the
/// loader). Negative ⇒ `None` (treated as "use defaults").
fn to_u32_opt(v: Option<i64>) -> Option<u32> {
    v.filter(|n| *n >= 0).map(|n| n as u32)
}

/// Same as [`to_u32_opt`] but for `u64` (`queue_wait_timeout_ms`).
fn to_u64_opt(v: Option<i64>) -> Option<u64> {
    v.filter(|n| *n >= 0).map(|n| n as u64)
}

impl From<ProviderRow> for Provider {
    fn from(r: ProviderRow) -> Self {
        Provider {
            id: r.id,
            key: r.provider_code,
            name: r.name,
            endpoint: r.endpoint,
            weight: r.weight as i32,
            created_at: fmt_ts(r.created_at),
            updated_at: fmt_ts(r.updated_at),
            max_concurrency: to_u32_opt(r.max_concurrency),
            max_queue_depth: to_u32_opt(r.max_queue_depth),
            queue_wait_timeout_ms: to_u64_opt(r.queue_wait_timeout_ms),
        }
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
struct ProviderModelRow {
    id: String,
    model_key: String,
    name: String,
    provider_id: String,
    status: i64,
}

impl From<ProviderModelRow> for ProviderModel {
    fn from(r: ProviderModelRow) -> Self {
        ProviderModel {
            id: r.id,
            key: r.model_key,
            name: r.name,
            provider_id: r.provider_id,
            status: r.status as i32,
        }
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
struct ProviderKeyRow {
    id: String,
    provider_id: String,
    // Nullable in the schema, but always populated by `insert_provider_key`.
    // The row type keeps them `Option` to mirror the DDL; `to_model` refuses a
    // row that is missing either half of the sealed value.
    api_key_ciphertext: Option<Vec<u8>>,
    api_key_nonce: Option<Vec<u8>>,
    key_version: i64,
    created_at: NaiveDateTime,
}

impl ProviderKeyRow {
    /// Decrypt the ciphertext into a plaintext `ProviderKey` model. Plaintext
    /// exists only transiently here, then lives in-memory in the model (never
    /// persisted). Requires a [`KeyProvider`] because decryption is I/O on the
    /// master key.
    fn to_model(&self, kp: &dyn KeyProvider) -> Result<ProviderKey, CryptoError> {
        let ciphertext = self
            .api_key_ciphertext
            .as_ref()
            .ok_or(CryptoError::Decrypt)?;
        let nonce_bytes = self.api_key_nonce.as_ref().ok_or(CryptoError::Decrypt)?;
        let nonce: [u8; crypto::NONCE_LEN] = nonce_bytes
            .as_slice()
            .try_into()
            .map_err(|_| CryptoError::Decrypt)?;
        let sealed = Sealed {
            ciphertext: ciphertext.clone(),
            nonce,
            key_version: self.key_version as u32,
        };
        let plaintext = kp.open(&sealed)?;
        let api_key = String::from_utf8(plaintext).map_err(|_| CryptoError::NotUtf8)?;
        Ok(ProviderKey {
            id: self.id.clone(),
            provider_id: self.provider_id.clone(),
            api_key,
            created_at: fmt_ts(self.created_at),
        })
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
struct TenantRow {
    id: String,
    name: String,
    domain: String,
    cert_key: Option<String>,
    cert_file: Option<String>,
    enabled: i64,
    created_at: NaiveDateTime,
    updated_at: NaiveDateTime,
}

impl From<TenantRow> for Tenant {
    fn from(r: TenantRow) -> Self {
        Tenant {
            id: r.id,
            name: r.name,
            domain: r.domain,
            cert_key: r.cert_key,
            cert_file: r.cert_file,
            enabled: r.enabled != 0,
            created_at: fmt_ts(r.created_at),
            updated_at: fmt_ts(r.updated_at),
        }
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
struct TenantProviderRow {
    id: String,
    tenant_id: String,
    provider_id: String,
}

impl From<TenantProviderRow> for TenantProvider {
    fn from(r: TenantProviderRow) -> Self {
        TenantProvider {
            id: r.id,
            tenant_id: r.tenant_id,
            provider_id: r.provider_id,
        }
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
struct TenantModelRow {
    id: String,
    tenant_id: String,
    model_key: String,
}

impl From<TenantModelRow> for TenantModel {
    fn from(r: TenantModelRow) -> Self {
        TenantModel {
            id: r.id,
            tenant_id: r.tenant_id,
            model_key: r.model_key,
        }
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
struct LimitRoleRow {
    id: String,
    name: String,
    matching_key: Option<String>,
    matching_model: Option<String>,
    matching_tenant: Option<String>,
    matching_provider: Option<String>,
    limit_count: Option<i64>,
    limit_token: Option<i64>,
    limit_window: String,
    enabled: i64,
    created_at: NaiveDateTime,
}

impl LimitRoleRow {
    /// The in-memory row, with `matching_key` opened.
    ///
    /// # Errors
    /// [`crypto_to_sqlx`] wrapping a [`CryptoError`] when the stored value IS a sealed envelope the
    /// master key cannot open (a wrong or rotated-away key is refused, never handed back as a key).
    /// A value that is not an envelope at all is a pre-D-16 plaintext row and is returned as-is —
    /// see `crypto::Sealed::open_text`.
    fn to_model(&self, kp: &dyn KeyProvider) -> Result<LimitRole, sqlx::Error> {
        Ok(LimitRole {
            id: self.id.clone(),
            name: self.name.clone(),
            matching_key: open_limit_key(kp, self.matching_key.clone())?,
            matching_model: self.matching_model.clone(),
            matching_tenant: self.matching_tenant.clone(),
            matching_provider: self.matching_provider.clone(),
            limit_count: self.limit_count,
            limit_token: self.limit_token,
            window: self.limit_window.clone(),
            enabled: self.enabled != 0,
            created_at: fmt_ts(self.created_at),
        })
    }
}

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

// ---------------------------------------------------------------------------
// CRUD — ProviderKey
// ---------------------------------------------------------------------------

/// Map a [`CryptoError`] into a sqlx error so the repo functions keep a single
/// `Result<_, sqlx::Error>` return type. `CryptoError` is `Send + Sync + Error`.
fn crypto_to_sqlx(e: CryptoError) -> sqlx::Error {
    sqlx::Error::Decode(Box::new(e))
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
pub async fn update_tenant(pool: &Pool, t: &Tenant) -> Result<(), sqlx::Error> {
    let cert_key = non_empty_path(t.cert_key.as_deref());
    let cert_file = non_empty_path(t.cert_file.as_deref());
    let mut tx = begin_write(pool).await?;
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
        // MySQL 8.0 answers a server-side prepared `DELETE FROM tenant` with
        // `ER_NEED_REPREPARE` (1615) once the cascading child foreign keys are
        // in the way — even a freshly prepared statement fails. Execute it
        // through the text protocol (no bound arguments ⇒ sqlx sends
        // `COM_QUERY`) and inline the id as a hex literal, which needs no
        // quoting/escaping under any `sql_mode`. The columns are unquoted, so
        // the statement needs no session-mode override.
        //
        // `X'...'` is a BINARY literal, so `id = X'...'` compares bytes rather
        // than applying the column's utf8mb4 collation. That is correct because
        // ids are application-generated ASCII and are written through the same
        // utf8mb4 connections, so the stored bytes equal the runtime UTF-8
        // bytes. Do NOT reuse this pattern for a column whose encoding or
        // collation may differ from the runtime string.
        let hex: String = id.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
        let sql = format!("DELETE FROM tenant WHERE id = X'{hex}'");
        sqlx::Executor::execute(&mut *tx, sqlx::raw_sql(&sql)).await?
    };
    #[cfg(feature = "db-postgres")]
    let affected = sqlx::query(&dialect::sql("DELETE FROM tenant WHERE id = ?"))
        .bind(id)
        .execute(&mut *tx)
        .await?;
    commit_write(tx, affected.rows_affected() > 0).await
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

#[derive(sqlx::FromRow, Debug, Clone)]
struct TenantCertRow {
    tenant_id: String,
    cert_pem: Option<String>,
    cert_key_ciphertext: Option<Vec<u8>>,
    cert_key_nonce: Option<Vec<u8>>,
    cert_key_version: Option<i64>,
}

impl TenantCertRow {
    /// Decrypt the sealed private key into a plaintext [`TenantCert`]. A row
    /// with ciphertext but a bad/missing nonce or version is a hard decode
    /// error (tampered / wrong master key — fail-closed, like provider keys).
    fn to_model(&self, kp: &dyn KeyProvider) -> Result<TenantCert, sqlx::Error> {
        let cert_key_pem = match (
            &self.cert_key_ciphertext,
            &self.cert_key_nonce,
            self.cert_key_version,
        ) {
            (Some(ct), Some(nonce), Some(version)) => {
                let nonce: [u8; crypto::NONCE_LEN] = nonce
                    .as_slice()
                    .try_into()
                    .map_err(|_| crypto_to_sqlx(CryptoError::Decrypt))?;
                let sealed = Sealed {
                    ciphertext: ct.clone(),
                    nonce,
                    key_version: version as u32,
                };
                let plaintext = kp.open(&sealed).map_err(crypto_to_sqlx)?;
                Some(
                    String::from_utf8(plaintext)
                        .map_err(|_| crypto_to_sqlx(CryptoError::NotUtf8))?,
                )
            }
            _ => None,
        };
        Ok(TenantCert {
            tenant_id: self.tenant_id.clone(),
            cert_pem: self.cert_pem.clone(),
            cert_key_pem,
        })
    }
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

/// One-time backfill for migration 0007: tenants with legacy `cert_file` /
/// `cert_key` paths but no stored content get their files read on this node
/// and stored as content (so the DB becomes self-contained and the shared
/// cert volume can be dropped). Best-effort: a failure is logged per tenant
/// and never aborts startup — the path fallback keeps serving until the files
/// become readable or the operator re-enters the cert via the admin API.
pub async fn backfill_legacy_certs(pool: &Pool, kp: &dyn KeyProvider) {
    let tenants = match list_tenants(pool).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "cert backfill: list_tenants failed; skipping");
            return;
        }
    };
    let existing = match list_tenant_certs(pool, kp).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "cert backfill: list_tenant_certs failed; skipping");
            return;
        }
    };
    let with_content: std::collections::HashSet<&str> = existing
        .iter()
        .filter(|c| c.cert_pem.is_some())
        .map(|c| c.tenant_id.as_str())
        .collect();

    for t in tenants {
        if with_content.contains(t.id.as_str()) {
            continue;
        }
        let (Some(cert_path), Some(key_path)) = (&t.cert_file, &t.cert_key) else {
            continue;
        };
        let (Ok(cert_bytes), Ok(key_bytes)) = (std::fs::read(cert_path), std::fs::read(key_path))
        else {
            tracing::warn!(
                tenant = %t.id,
                cert_path,
                key_path,
                "cert backfill: legacy cert paths not readable on this node; path fallback remains active"
            );
            continue;
        };
        let cert = TenantCert {
            tenant_id: t.id.clone(),
            cert_pem: Some(String::from_utf8_lossy(&cert_bytes).into_owned()),
            cert_key_pem: Some(String::from_utf8_lossy(&key_bytes).into_owned()),
        };
        match update_tenant_cert(pool, kp, &cert).await {
            Ok(()) => tracing::info!(tenant = %t.id, "cert backfill: legacy paths -> content"),
            Err(e) => tracing::warn!(tenant = %t.id, error = %e, "cert backfill: store failed"),
        }
    }
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
//   * [`seal_legacy_limit_keys`]: the ONE place that rewrites a legacy row, called by the config
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
fn open_limit_key(
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
/// [`seal_legacy_limit_keys`], which opens its own write transaction, precisely because this one is
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

/// The rows that still hold a PLAINTEXT `matching_key` — exactly what [`seal_legacy_limit_keys`] will
/// rewrite.
///
/// Split out from the migration on purpose: the migration's READ is one plain statement on the pool
/// and its WRITE is a compare-and-swap inside a write transaction, and the property that matters
/// (a row that changed in between is not clobbered) only exists in the gap between them. A test that
/// wants to model that gap has to be able to hold the two halves apart — otherwise it ends up
/// asserting a COPY of the UPDATE text, which stays green after the production clause is deleted.
async fn legacy_limit_key_rows(pool: &Pool) -> Result<Vec<(String, String)>, sqlx::Error> {
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
async fn reseal_limit_key_row<'e, E>(
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

/// Seal every `limit_role.matching_key` still stored as PLAINTEXT. Returns how many rows were
/// rewritten (0 on a database that is already sealed).
///
/// ## Why this exists, and why it is not inside the reader
///
/// Decision D-16 says an upgrade needs no manual step. The rows written before that decision hold a
/// client key in the clear, so something has to rewrite them — and that something must be a WRITE,
/// with its own transaction, not a side effect of a read (see [`list_limit_roles_on`]).
///
/// ## Shape
///
/// 1. [`legacy_limit_key_rows`] reads first, with no lock: on a sealed database this function opens
///    no write transaction at all, which is what keeps "the loader ran" from meaning "the loader took
///    the write lock" on every reload.
/// 2. Each write is [`reseal_limit_key_row`]'s compare-and-swap, so an admin write (or another node)
///    landing between the read and the write wins and this pass affects nothing.
/// 3. Only rows that actually changed are counted, so the caller's log line means what it says.
///
/// # Errors
/// A [`CryptoError`] from sealing, or a SQL error from the update.
pub async fn seal_legacy_limit_keys(
    pool: &Pool,
    kp: &dyn KeyProvider,
) -> Result<usize, sqlx::Error> {
    let pending = legacy_limit_key_rows(pool).await?;
    if pending.is_empty() {
        return Ok(0); // already sealed: not even a write transaction is opened
    }

    let mut tx = begin_write(pool).await?;
    let mut resealed = 0usize;
    for (id, value) in pending {
        let fresh = kp.seal(value.as_bytes()).map_err(crypto_to_sqlx)?.to_text();
        resealed += reseal_limit_key_row(&mut *tx, &id, &value, &fresh).await? as usize;
    }
    if resealed > 0 {
        bump_config_revision_on_conn(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(resealed)
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

// ---------------------------------------------------------------------------
// CRUD — 用户层级 / 租户访问令牌 / Global Role（migration 0014, plan §3.1）
//
// T1 is ADDITIVE (decision D-17): the loader gains these reads alongside the
// surviving legacy tables at that time; `0015` now drops the remaining legacy
// `tenant` columns as part of the terminal deletion batch.
//
// These use **runtime** `sqlx::query` / `query_as` (no compile-time query
// macros anywhere in this crate). `enabled`
// maps `INTEGER`↔`bool` at the boundary; `billing_status` maps the TEXT CHECK
// domain onto [`hydra_core::model::BillingStatus`]; the loader (not the repo)
// filters where §3.2 says enabled-only.
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow, Debug, Clone)]
struct UserGroupRow {
    id: String,
    tenant_id: String,
    name: String,
    enabled: i64,
    created_at: NaiveDateTime,
    updated_at: NaiveDateTime,
}

impl From<UserGroupRow> for UserGroup {
    fn from(r: UserGroupRow) -> Self {
        UserGroup {
            id: r.id,
            tenant_id: r.tenant_id,
            name: r.name,
            enabled: r.enabled != 0,
            created_at: fmt_ts(r.created_at),
            updated_at: fmt_ts(r.updated_at),
        }
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
struct UserRow {
    id: String,
    tenant_id: String,
    user_group_id: String,
    external_id: Option<String>,
    name: String,
    enabled: i64,
    billing_status: String,
    billing_reason: Option<String>,
    billing_updated_at: Option<NaiveDateTime>,
    created_at: NaiveDateTime,
    updated_at: NaiveDateTime,
}

impl From<UserRow> for User {
    fn from(r: UserRow) -> Self {
        User {
            id: r.id,
            tenant_id: r.tenant_id,
            user_group_id: r.user_group_id,
            external_id: r.external_id,
            name: r.name,
            enabled: r.enabled != 0,
            // The CHECK domain is ('active','delinquent'); an out-of-domain value
            // cannot exist through the schema, and a manual edit must not crash the
            // loader — anything not exactly 'delinquent' loads as Active (the
            // safe default: never fabricate a 402 from a corrupt row).
            billing_status: if r.billing_status == "delinquent" {
                BillingStatus::Delinquent
            } else {
                BillingStatus::Active
            },
            billing_reason: r.billing_reason,
            billing_updated_at: fmt_ts_opt(r.billing_updated_at),
            created_at: fmt_ts(r.created_at),
            updated_at: fmt_ts(r.updated_at),
        }
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
struct UserApiKeyRow {
    id: String,
    user_id: String,
    name: String,
    key_hash: String,
    key_prefix: String,
    key_last4: String,
    enabled: i64,
    expires_at: Option<String>,
    created_at: NaiveDateTime,
    updated_at: NaiveDateTime,
}

impl From<UserApiKeyRow> for UserApiKey {
    fn from(r: UserApiKeyRow) -> Self {
        UserApiKey {
            id: r.id,
            user_id: r.user_id,
            name: r.name,
            key_hash: r.key_hash,
            key_prefix: r.key_prefix,
            key_last4: r.key_last4,
            enabled: r.enabled != 0,
            expires_at: r.expires_at,
            created_at: fmt_ts(r.created_at),
            updated_at: fmt_ts(r.updated_at),
        }
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
struct TenantAccessTokenRow {
    id: String,
    tenant_id: String,
    name: String,
    token_hash: String,
    token_prefix: String,
    token_last4: String,
    enabled: i64,
    expires_at: Option<String>,
    created_at: NaiveDateTime,
    updated_at: NaiveDateTime,
}

impl From<TenantAccessTokenRow> for TenantAccessToken {
    fn from(r: TenantAccessTokenRow) -> Self {
        TenantAccessToken {
            id: r.id,
            tenant_id: r.tenant_id,
            name: r.name,
            token_hash: r.token_hash,
            token_prefix: r.token_prefix,
            token_last4: r.token_last4,
            enabled: r.enabled != 0,
            expires_at: r.expires_at,
            created_at: fmt_ts(r.created_at),
            updated_at: fmt_ts(r.updated_at),
        }
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
struct GlobalRoleRow {
    id: String,
    name: String,
    tenant_id: String,
    user_group_id: String,
    user_id: String,
    api_key_id: String,
    model_key: String,
    enabled: i64,
    created_at: NaiveDateTime,
    updated_at: NaiveDateTime,
}

#[derive(sqlx::FromRow, Debug, Clone)]
struct GlobalRoleProviderRow {
    role_id: String,
    provider_id: String,
}

/// All user groups, ordered deterministically (`tenant_id, name` — the
/// UNIQUE key). Returns ALL rows incl. `enabled = false`; the loader keeps them
/// (plan §3.2: a disabled group answers 403, so the row must be present).
pub async fn list_user_groups(pool: &Pool) -> Result<Vec<UserGroup>, sqlx::Error> {
    list_user_groups_on(pool).await
}

/// [`list_user_groups`] on an arbitrary executor (one-transaction read).
pub(crate) async fn list_user_groups_on<'e, E>(exec: E) -> Result<Vec<UserGroup>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Db>,
{
    let rows: Vec<UserGroupRow> = sqlx::query_as(&dialect::sql(
        "SELECT id, tenant_id, name, enabled, created_at, updated_at \
         FROM user_group ORDER BY tenant_id, name",
    ))
    .fetch_all(exec)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// All users, ordered by id (deterministic restore/derivation order). Returns
/// ALL rows incl. disabled / delinquent (decision D-5 — see `ConfigData::users`).
pub async fn list_users(pool: &Pool) -> Result<Vec<User>, sqlx::Error> {
    list_users_on(pool).await
}

/// [`list_users`] on an arbitrary executor (one-transaction read).
pub(crate) async fn list_users_on<'e, E>(exec: E) -> Result<Vec<User>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Db>,
{
    let rows: Vec<UserRow> = sqlx::query_as(&dialect::sql(
        "SELECT id, tenant_id, user_group_id, external_id, name, enabled, \
         billing_status, billing_reason, billing_updated_at, created_at, updated_at \
         FROM hydra_user ORDER BY id",
    ))
    .fetch_all(exec)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// All user api-keys, ordered by id. Returns ALL rows incl. disabled/expired
/// (plan §3.2: revoked keys must read as 401 "known", not "unknown" — the
/// FULL set is what [`ConfigData::reindex_identity`] consumes).
pub async fn list_user_api_keys(pool: &Pool) -> Result<Vec<UserApiKey>, sqlx::Error> {
    list_user_api_keys_on(pool).await
}

/// [`list_user_api_keys`] on an arbitrary executor (one-transaction read).
pub(crate) async fn list_user_api_keys_on<'e, E>(exec: E) -> Result<Vec<UserApiKey>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Db>,
{
    let rows: Vec<UserApiKeyRow> = sqlx::query_as(&dialect::sql(
        "SELECT id, user_id, name, key_hash, key_prefix, key_last4, enabled, \
         expires_at, created_at, updated_at \
         FROM user_api_key ORDER BY id",
    ))
    .fetch_all(exec)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// All tenant access tokens, ordered by id. The LOADER filters to enabled-only
/// when building `ConfigData::tenant_tokens` (plan §3.2) — the repo returns
/// rows, not policy.
pub async fn list_tenant_access_tokens(pool: &Pool) -> Result<Vec<TenantAccessToken>, sqlx::Error> {
    list_tenant_access_tokens_on(pool).await
}

/// [`list_tenant_access_tokens`] on an arbitrary executor (one-transaction read).
pub(crate) async fn list_tenant_access_tokens_on<'e, E>(
    exec: E,
) -> Result<Vec<TenantAccessToken>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Db>,
{
    let rows: Vec<TenantAccessTokenRow> = sqlx::query_as(&dialect::sql(
        "SELECT id, tenant_id, name, token_hash, token_prefix, token_last4, \
         enabled, expires_at, created_at, updated_at \
         FROM tenant_access_token ORDER BY id",
    ))
    .fetch_all(exec)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Global Roles with their provider sets, joined IN Rust (two queries, one
/// deterministic order each — the plan §3.2 shape: `GlobalRole.provider_ids:
/// Vec<String>`). Returns ALL rows; the loader filters to enabled-only (a
/// disabled rule must match NOTHING, so unlike the identity rows it is dropped,
/// not kept-inert).
///
/// `provider_ids` within a role are ordered ascending (plan §3.4: "顺序确定
/// (provider_id 升序)") — the union in the router dedups via `BTreeSet` anyway,
/// but the stored vector must be deterministic so the `PartialEq` generation
/// predicate does not flicker on identical sets in different orders.
pub async fn list_global_roles(pool: &Pool) -> Result<Vec<GlobalRole>, sqlx::Error> {
    list_global_roles_on(pool).await
}

/// [`list_global_roles`] on an arbitrary executor (one-transaction read).
pub(crate) async fn list_global_roles_on<'e, E>(exec: E) -> Result<Vec<GlobalRole>, sqlx::Error>
where
    // Two fetch_all calls (role rows + provider rows) need the executor twice;
    // every executor this is ever called with is a borrow (`&pool` / `&tx`), hence
    // `Copy` — same reason `pool` is Copy at the public wrappers' call sites.
    E: sqlx::Executor<'e, Database = Db> + Copy,
{
    let role_rows: Vec<GlobalRoleRow> = sqlx::query_as(&dialect::sql(
        "SELECT id, name, tenant_id, user_group_id, user_id, api_key_id, model_key, \
         enabled, created_at, updated_at \
         FROM global_role ORDER BY id",
    ))
    .fetch_all(exec)
    .await?;
    // `ORDER BY role_id, provider_id`: deterministic per role, provider_id
    // ascending. Grouping is a linear merge over the already-ordered rows.
    let provider_rows: Vec<GlobalRoleProviderRow> = sqlx::query_as(&dialect::sql(
        "SELECT role_id, provider_id FROM global_role_provider ORDER BY role_id, provider_id",
    ))
    .fetch_all(exec)
    .await?;

    let mut by_role: HashMap<String, Vec<String>> = HashMap::new();
    for p in provider_rows {
        by_role.entry(p.role_id).or_default().push(p.provider_id);
    }
    let mut out: Vec<GlobalRole> = Vec::with_capacity(role_rows.len());
    for role in role_rows {
        // Taken BEFORE the field moves below: `role.id` is consumed by the struct.
        let provider_ids = by_role.remove(&role.id).unwrap_or_default();
        out.push(GlobalRole {
            id: role.id,
            name: role.name,
            tenant_id: role.tenant_id,
            user_group_id: role.user_group_id,
            user_id: role.user_id,
            api_key_id: role.api_key_id,
            model_key: role.model_key,
            // A role with no provider rows gets the EMPTY set: it matches and
            // contributes nothing (the union stays empty -> legacy fallback, plan
            // §3.4). Never a missing field.
            provider_ids,
            enabled: role.enabled != 0,
            created_at: fmt_ts(role.created_at),
            updated_at: fmt_ts(role.updated_at),
        });
    }
    Ok(out)
}

/// [`list_global_roles_on`] against a borrowed database connection/transaction.
///
/// The generic `Copy` bound above is awkward for a transaction: sqlx exposes a
/// transaction's connection as `&mut *tx`, which is an executor but not `Copy`.
/// A snapshot reader that needs BOTH role and provider rows in the SAME
/// transaction uses this variant instead of opening a separate pool read.
pub(crate) async fn list_global_roles_on_conn(
    conn: &mut DbConnection,
) -> Result<Vec<GlobalRole>, sqlx::Error> {
    let role_rows: Vec<GlobalRoleRow> = sqlx::query_as(&dialect::sql(
        "SELECT id, name, tenant_id, user_group_id, user_id, api_key_id, model_key, \
         enabled, created_at, updated_at \
         FROM global_role ORDER BY id",
    ))
    .fetch_all(&mut *conn)
    .await?;
    let provider_rows: Vec<GlobalRoleProviderRow> = sqlx::query_as(&dialect::sql(
        "SELECT role_id, provider_id FROM global_role_provider ORDER BY role_id, provider_id",
    ))
    .fetch_all(&mut *conn)
    .await?;

    let mut by_role: HashMap<String, Vec<String>> = HashMap::new();
    for p in provider_rows {
        by_role.entry(p.role_id).or_default().push(p.provider_id);
    }

    let mut out: Vec<GlobalRole> = Vec::with_capacity(role_rows.len());
    for role in role_rows {
        let provider_ids = by_role.remove(&role.id).unwrap_or_default();
        out.push(GlobalRole {
            id: role.id,
            name: role.name,
            tenant_id: role.tenant_id,
            user_group_id: role.user_group_id,
            user_id: role.user_id,
            api_key_id: role.api_key_id,
            model_key: role.model_key,
            provider_ids,
            enabled: role.enabled != 0,
            created_at: fmt_ts(role.created_at),
            updated_at: fmt_ts(role.updated_at),
        });
    }
    Ok(out)
}

/// Runtime-checked user-hierarchy CRUD.
///
/// Every write here is tenant-scoped (P0-1). An id owned by another tenant is
/// indistinguishable from an unknown id: the function returns
/// [`sqlx::Error::RowNotFound`] and changes nothing. Bulk callers propagate that
/// error, which drops the transaction and rolls back every row in the batch.
fn billing_status_to_str(status: BillingStatus) -> &'static str {
    match status {
        BillingStatus::Active => "active",
        BillingStatus::Delinquent => "delinquent",
    }
}

async fn upsert_user_group_on_conn(
    conn: &mut DbConnection,
    group: &UserGroup,
) -> Result<u64, sqlx::Error> {
    let existing: Option<(String,)> = sqlx::query_as(&dialect::sql(
        "SELECT tenant_id FROM user_group WHERE id = ? FOR UPDATE",
    ))
    .bind(&group.id)
    .fetch_optional(&mut *conn)
    .await?;

    let affected =
        match existing {
            // Cross-tenant conflict: reject exactly like an unknown id.
            Some((owner_tenant,)) if owner_tenant != group.tenant_id => {
                return Err(sqlx::Error::RowNotFound);
            }
            Some(_) => {
                // Ownership is immutable: update only the mutable fields.
                sqlx::query(&dialect::sql(
                    "UPDATE user_group SET name = ?, enabled = ?, updated_at = CURRENT_TIMESTAMP \
                 WHERE id = ? AND tenant_id = ?",
                ))
                .bind(&group.name)
                .bind(flag(group.enabled))
                .bind(&group.id)
                .bind(&group.tenant_id)
                .execute(&mut *conn)
                .await?
            }
            None => sqlx::query(&dialect::sql(
                "INSERT INTO user_group (id, tenant_id, name, enabled, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, CURRENT_TIMESTAMP)",
            ))
            .bind(&group.id)
            .bind(&group.tenant_id)
            .bind(&group.name)
            .bind(flag(group.enabled))
            .bind(parse_ts(&group.created_at)?)
            .execute(&mut *conn)
            .await?,
        };
    Ok(affected.rows_affected())
}

pub async fn upsert_user_group(pool: &Pool, group: &UserGroup) -> Result<u64, sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let affected = upsert_user_group_on_conn(&mut tx, group).await?;
    if affected > 0 {
        bump_config_revision_on_conn(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(affected)
}

/// Upsert MANY user groups in ONE `begin_write` transaction and commit once.
/// One cross-tenant row aborts the whole batch (no partial commit).
pub async fn upsert_user_groups_bulk(pool: &Pool, groups: &[UserGroup]) -> Result<(), sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let mut changed = false;
    for group in groups {
        changed |= upsert_user_group_on_conn(&mut tx, group).await? > 0;
    }
    commit_write(tx, changed).await
}

pub async fn delete_user_group(pool: &Pool, tenant_id: &str, id: &str) -> Result<u64, sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let affected = sqlx::query(&dialect::sql(
        "DELETE FROM user_group WHERE id = ? AND tenant_id = ?",
    ))
    .bind(id)
    .bind(tenant_id)
    .execute(&mut *tx)
    .await?;
    let changed = affected.rows_affected();
    if changed > 0 {
        bump_config_revision_on_conn(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(changed)
}

async fn upsert_user_on_conn(conn: &mut DbConnection, user: &User) -> Result<u64, sqlx::Error> {
    let billing_status = billing_status_to_str(user.billing_status);
    let billing_updated_at = parse_ts_opt(&user.billing_updated_at)?;

    // The group reference is tenant-scoped too: a stale cache must not let a
    // user point at another tenant's group.
    let group_owner: Option<(String,)> = sqlx::query_as(&dialect::sql(
        "SELECT tenant_id FROM user_group WHERE id = ? FOR UPDATE",
    ))
    .bind(&user.user_group_id)
    .fetch_optional(&mut *conn)
    .await?;
    match group_owner {
        Some((owner_tenant,)) if owner_tenant == user.tenant_id => {}
        _ => return Err(sqlx::Error::RowNotFound),
    }

    let existing: Option<(String,)> = sqlx::query_as(&dialect::sql(
        "SELECT tenant_id FROM hydra_user WHERE id = ? FOR UPDATE",
    ))
    .bind(&user.id)
    .fetch_optional(&mut *conn)
    .await?;

    let affected = match existing {
        Some((owner_tenant,)) if owner_tenant != user.tenant_id => {
            return Err(sqlx::Error::RowNotFound);
        }
        Some(_) => {
            // tenant_id is deliberately absent: ownership never moves.
            sqlx::query(&dialect::sql(
                "UPDATE hydra_user SET user_group_id = ?, external_id = ?, name = ?, enabled = ?, \
                 billing_status = ?, billing_reason = ?, billing_updated_at = ?, \
                 updated_at = CURRENT_TIMESTAMP WHERE id = ? AND tenant_id = ?",
            ))
            .bind(&user.user_group_id)
            .bind(&user.external_id)
            .bind(&user.name)
            .bind(flag(user.enabled))
            .bind(billing_status)
            .bind(&user.billing_reason)
            .bind(billing_updated_at)
            .bind(&user.id)
            .bind(&user.tenant_id)
            .execute(&mut *conn)
            .await?
        }
        None => {
            sqlx::query(&dialect::sql(
                "INSERT INTO hydra_user (id, tenant_id, user_group_id, external_id, name, \
                 enabled, billing_status, billing_reason, billing_updated_at, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)",
            ))
            .bind(&user.id)
            .bind(&user.tenant_id)
            .bind(&user.user_group_id)
            .bind(&user.external_id)
            .bind(&user.name)
            .bind(flag(user.enabled))
            .bind(billing_status)
            .bind(&user.billing_reason)
            .bind(billing_updated_at)
            .bind(parse_ts(&user.created_at)?)
            .execute(&mut *conn)
            .await?
        }
    };
    Ok(affected.rows_affected())
}

pub async fn upsert_user(pool: &Pool, user: &User) -> Result<u64, sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let affected = upsert_user_on_conn(&mut tx, user).await?;
    if affected > 0 {
        bump_config_revision_on_conn(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(affected)
}

/// Upsert MANY users in ONE `begin_write` transaction and commit once.
/// One cross-tenant row aborts the whole batch (no partial commit).
pub async fn upsert_users_bulk(pool: &Pool, users: &[User]) -> Result<(), sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let mut changed = false;
    for user in users {
        changed |= upsert_user_on_conn(&mut tx, user).await? > 0;
    }
    commit_write(tx, changed).await
}

pub async fn delete_user(pool: &Pool, tenant_id: &str, id: &str) -> Result<u64, sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let affected = sqlx::query(&dialect::sql(
        "DELETE FROM hydra_user WHERE id = ? AND tenant_id = ?",
    ))
    .bind(id)
    .bind(tenant_id)
    .execute(&mut *tx)
    .await?;
    let changed = affected.rows_affected();
    if changed > 0 {
        bump_config_revision_on_conn(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(changed)
}

async fn insert_user_api_key_on_conn(
    conn: &mut DbConnection,
    tenant_id: &str,
    api_key: &UserApiKey,
) -> Result<u64, sqlx::Error> {
    // The parent row is the ownership proof; lock it so a concurrent user move
    // (which the hardening above forbids) cannot slip between check and insert.
    let parent: Option<(String,)> = sqlx::query_as(&dialect::sql(
        "SELECT tenant_id FROM hydra_user WHERE id = ? FOR UPDATE",
    ))
    .bind(&api_key.user_id)
    .fetch_optional(&mut *conn)
    .await?;
    match parent {
        Some((owner_tenant,)) if owner_tenant == tenant_id => {}
        _ => return Err(sqlx::Error::RowNotFound),
    }

    let affected = sqlx::query(
        &dialect::sql("INSERT INTO user_api_key (id, user_id, name, key_hash, key_prefix, key_last4, \
         enabled, expires_at, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)"),
    )
    .bind(&api_key.id)
    .bind(&api_key.user_id)
    .bind(&api_key.name)
    .bind(&api_key.key_hash)
    .bind(&api_key.key_prefix)
    .bind(&api_key.key_last4)
    .bind(flag(api_key.enabled))
    .bind(&api_key.expires_at)
    .bind(parse_ts(&api_key.created_at)?)
    .execute(&mut *conn)
    .await?;
    Ok(affected.rows_affected())
}

pub async fn insert_user_api_key(
    pool: &Pool,
    tenant_id: &str,
    api_key: &UserApiKey,
) -> Result<u64, sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let affected = insert_user_api_key_on_conn(&mut tx, tenant_id, api_key).await?;
    if affected > 0 {
        bump_config_revision_on_conn(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(affected)
}

/// Insert MANY user api-keys in ONE `begin_write` transaction and commit once.
/// A key whose parent user is unknown or belongs to another tenant aborts the
/// whole batch.
pub async fn insert_user_api_keys_bulk(
    pool: &Pool,
    tenant_id: &str,
    api_keys: &[UserApiKey],
) -> Result<(), sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let mut changed = false;
    for api_key in api_keys {
        changed |= insert_user_api_key_on_conn(&mut tx, tenant_id, api_key).await? > 0;
    }
    commit_write(tx, changed).await
}

pub async fn delete_user_api_key(
    pool: &Pool,
    tenant_id: &str,
    id: &str,
) -> Result<u64, sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let affected = sqlx::query(&dialect::sql(
        "DELETE FROM user_api_key WHERE id = ? \
         AND user_id IN (SELECT id FROM hydra_user WHERE tenant_id = ?)",
    ))
    .bind(id)
    .bind(tenant_id)
    .execute(&mut *tx)
    .await?;
    let changed = affected.rows_affected();
    if changed > 0 {
        bump_config_revision_on_conn(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(changed)
}

pub async fn insert_tenant_access_token(
    pool: &Pool,
    token: &TenantAccessToken,
) -> Result<u64, sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let affected = sqlx::query(
        &dialect::sql("INSERT INTO tenant_access_token (id, tenant_id, name, token_hash, token_prefix, token_last4, \
         enabled, expires_at, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)"),
    )
        .bind(&token.id)
        .bind(&token.tenant_id)
        .bind(&token.name)
        .bind(&token.token_hash)
        .bind(&token.token_prefix)
        .bind(&token.token_last4)
        .bind(flag(token.enabled))
        .bind(&token.expires_at)
        .bind(parse_ts(&token.created_at)?)
        .execute(&mut *tx)
        .await?;
    let changed = affected.rows_affected();
    if changed > 0 {
        bump_config_revision_on_conn(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(changed)
}

pub async fn delete_tenant_access_token(pool: &Pool, id: &str) -> Result<u64, sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let affected = sqlx::query(&dialect::sql(
        "DELETE FROM tenant_access_token WHERE id = ?",
    ))
    .bind(id)
    .execute(&mut *tx)
    .await?;
    let changed = affected.rows_affected();
    if changed > 0 {
        bump_config_revision_on_conn(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(changed)
}

/// The Global Role row write (create or replace) on a borrowed connection.
///
/// `created_at` is bound ONLY in the INSERT arm: the `ON CONFLICT` arm never touches
/// it, so an UPDATE PRESERVES the original creation time — an admin rewrite of an
/// existing rule must not look like a re-creation to the operator.
async fn upsert_global_role_on_conn(
    conn: &mut DbConnection,
    role: &GlobalRole,
) -> Result<u64, sqlx::Error> {
    let existing: Option<(String,)> = sqlx::query_as(&dialect::sql(
        "SELECT id FROM global_role WHERE id = ? FOR UPDATE",
    ))
    .bind(&role.id)
    .fetch_optional(&mut *conn)
    .await?;

    let affected = if existing.is_some() {
        sqlx::query(&dialect::sql(
            "UPDATE global_role SET name = ?, tenant_id = ?, user_group_id = ?, user_id = ?, \
             api_key_id = ?, model_key = ?, enabled = ?, updated_at = CURRENT_TIMESTAMP \
             WHERE id = ?",
        ))
        .bind(&role.name)
        .bind(&role.tenant_id)
        .bind(&role.user_group_id)
        .bind(&role.user_id)
        .bind(&role.api_key_id)
        .bind(&role.model_key)
        .bind(flag(role.enabled))
        .bind(&role.id)
        .execute(&mut *conn)
        .await?
    } else {
        sqlx::query(&dialect::sql(
            "INSERT INTO global_role (id, name, tenant_id, user_group_id, user_id, api_key_id, \
             model_key, enabled, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)",
        ))
        .bind(&role.id)
        .bind(&role.name)
        .bind(&role.tenant_id)
        .bind(&role.user_group_id)
        .bind(&role.user_id)
        .bind(&role.api_key_id)
        .bind(&role.model_key)
        .bind(flag(role.enabled))
        .bind(parse_ts(&role.created_at)?)
        .execute(&mut *conn)
        .await?
    };
    Ok(affected.rows_affected())
}

/// The pool-level wrapper of [`upsert_global_role_on_conn`] (its own implicit
/// write transaction). When the provider set changes with the row — which is every
/// admin write — use [`upsert_global_role_with_providers`]: the two writes must land
/// as ONE transaction, see the note there.
pub async fn upsert_global_role(pool: &Pool, role: &GlobalRole) -> Result<u64, sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let affected = upsert_global_role_on_conn(&mut tx, role).await?;
    if affected > 0 {
        bump_config_revision_on_conn(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(affected)
}

/// The provider-set rewrite on a borrowed connection — shared by
/// [`replace_global_role_providers`] and the one-transaction role writer below.
async fn replace_global_role_providers_on_conn(
    conn: &mut DbConnection,
    role_id: &str,
    providers: &[String],
) -> Result<(u64, u64), sqlx::Error> {
    let deleted = sqlx::query(&dialect::sql(
        "DELETE FROM global_role_provider WHERE role_id = ?",
    ))
    .bind(role_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();

    let mut normalized = providers.to_vec();
    normalized.sort_unstable();
    normalized.dedup();

    let mut inserted = 0;
    for provider_id in normalized {
        let affected = sqlx::query(&dialect::sql(
            "INSERT INTO global_role_provider (id, role_id, provider_id) VALUES (?, ?, ?)",
        ))
        .bind(format!(
            "grprov:{role_id}:{provider_id}",
            role_id = role_id,
            provider_id = provider_id
        ))
        .bind(role_id)
        .bind(provider_id)
        .execute(&mut *conn)
        .await?;
        inserted += affected.rows_affected();
    }
    Ok((deleted, inserted))
}

/// Replace a Global Role's provider set in one database transaction.
///
/// The provider set is the normalized `global_role_provider` table, not a JSON
/// blob: delete old child rows, then insert the requested provider ids. The provider
/// ids are de-duplicated because `UNIQUE(role_id, provider_id)` is the table's identity
/// and `GlobalRole.provider_ids` may contain the same provider more than once.
pub async fn replace_global_role_providers(
    pool: &Pool,
    role_id: &str,
    providers: &[String],
) -> Result<u64, sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let (deleted, inserted) =
        replace_global_role_providers_on_conn(&mut tx, role_id, providers).await?;
    if deleted + inserted > 0 {
        bump_config_revision_on_conn(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(inserted)
}

/// Create-or-replace a Global Role AND replace its provider set in ONE transaction.
///
/// This is the admin PUT path's single write primitive. Doing the two writes as
/// two separate transactions leaves a window in which the rule row is committed
/// while the provider set is still the OLD one (or empty, on a fresh create): the
/// provider set IS the rule's entire effect, so a half-written rule is a live rule
/// granting the wrong providers to every matching request. A failure anywhere rolls
/// the whole thing back — no role without its intended provider set, never a
/// role row plus orphan child rows either (`ON DELETE CASCADE` + the FK).
///
/// The role row goes FIRST: `global_role_provider.role_id REFERENCES global_role(id)`,
/// so the child rows cannot be inserted before their parent exists.
///
/// Returns the ROLE row count (always 1 — `ON CONFLICT DO UPDATE` reports a row
/// even when the values are identical); the provider row count is deliberately not
/// part of the answer, the caller already knows the set it wrote.
pub async fn upsert_global_role_with_providers(
    pool: &Pool,
    role: &GlobalRole,
    providers: &[String],
) -> Result<u64, sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let affected = upsert_global_role_on_conn(&mut tx, role).await?;
    let (deleted, inserted) =
        replace_global_role_providers_on_conn(&mut tx, &role.id, providers).await?;
    if affected + deleted + inserted > 0 {
        bump_config_revision_on_conn(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(affected)
}

/// Create-or-replace MANY Global Roles AND their provider sets in ONE
/// `begin_write` transaction, then commit once.
///
/// Reuses the two `_on_conn` primitives above, so the single-row and bulk write
/// paths cannot drift in SQL or normalization (the provider set keeps its
/// `UNIQUE(role_id, provider_id)` de-duplication and ascending storage order).
pub async fn upsert_global_roles_bulk(
    pool: &Pool,
    roles: &[(GlobalRole, Vec<String>)],
) -> Result<(), sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let mut changed = false;
    for (role, providers) in roles {
        changed |= upsert_global_role_on_conn(&mut tx, role).await? > 0;
        let (deleted, inserted) =
            replace_global_role_providers_on_conn(&mut tx, &role.id, providers).await?;
        changed |= deleted + inserted > 0;
    }
    commit_write(tx, changed).await
}

pub async fn delete_global_role(pool: &Pool, id: &str) -> Result<u64, sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let affected = sqlx::query(&dialect::sql("DELETE FROM global_role WHERE id = ?"))
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let changed = affected.rows_affected();
    if changed > 0 {
        bump_config_revision_on_conn(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(changed)
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
    if test_fault::persist_should_fail() {
        return Err(sqlx::Error::Protocol(
            "injected: config_revision persist failed".into(),
        ));
    }
    let mut tx = begin_write(pool).await?;
    let version = bump_config_revision_on_conn(&mut tx).await?;
    tx.commit().await?;
    Ok(version)
}

/// Deterministic, backend-agnostic failure injection for the store's
/// persist-before-swap tests.
///
/// The old tests installed a SQLite trigger to abort the marker write. That
/// mechanism does not exist on PostgreSQL/MySQL, so the same property is pinned
/// by forcing [`bump_config_revision`] to fail before it opens a transaction.
/// The guard resets the flag even when an assertion panics, so one failing test
/// cannot poison its siblings in the same binary.
#[cfg(test)]
pub(crate) mod test_fault {
    use std::sync::atomic::{AtomicBool, Ordering};

    static FAIL_PERSIST: AtomicBool = AtomicBool::new(false);

    pub(crate) fn persist_should_fail() -> bool {
        FAIL_PERSIST.load(Ordering::SeqCst)
    }

    /// RAII switch: the flag is true until the returned guard is dropped.
    pub(crate) struct FailPersistGuard;

    impl Drop for FailPersistGuard {
        fn drop(&mut self) {
            FAIL_PERSIST.store(false, Ordering::SeqCst);
        }
    }

    pub(crate) fn fail_persist() -> FailPersistGuard {
        FAIL_PERSIST.store(true, Ordering::SeqCst);
        FailPersistGuard
    }
}

/// Commit a write transaction, bumping the shared revision first iff the
/// mutation changed at least one row. Inserts pass `true`; updates/deletes pass
/// `rows_affected() > 0` so a no-op does not create a reload storm.
async fn commit_write(mut tx: Tx<'static>, changed: bool) -> Result<(), sqlx::Error> {
    if changed {
        bump_config_revision_on_conn(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// What a re-seal pass did (see [`reseal_secrets`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResealReport {
    /// Provider api-keys rewritten under the current key version.
    pub provider_keys_resealed: usize,
    /// Tenant certificate private keys rewritten under the current key version.
    pub tenant_certs_resealed: usize,
    /// `limit_role.matching_key` values rewritten under the current key version (decision D-16;
    /// includes pre-D-16 PLAINTEXT rows, which this pass seals for the first time).
    pub limit_keys_resealed: usize,
    /// Rows already at the current version (nothing to do).
    pub already_current: usize,
    /// Rows that could NOT be opened (unknown version / wrong key), left
    /// untouched. Non-empty means the rotation is INCOMPLETE.
    pub failed: Vec<String>,
}

impl ResealReport {
    /// True when every stored secret now opens under the current key version.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Re-encrypt every stored secret under the provider's CURRENT key version.
///
/// This is the missing half of master-key rotation. Before it existed, changing
/// `HYDRA_ENCRYPTION_KEY` made every ciphertext unopenable, so
/// `ConfigStore::load` failed and the process refused to start — and the admin
/// API that could have re-entered the keys needs that same process, so there was
/// no way back in short of editing the database by hand.
///
/// The procedure is: start once with the new key as
/// `HYDRA_ENCRYPTION_KEY` (+ `HYDRA_ENCRYPTION_KEY_VERSION`), the old key as
/// `HYDRA_ENCRYPTION_KEY_PREVIOUS` (+ `..._PREVIOUS_VERSION`) and
/// `HYDRA_RESEAL_SECRETS=1`. Every row is read AND rewritten inside ONE write
/// transaction, then the previous key can be dropped. Rows that cannot be opened
/// are REPORTED and left alone (never silently rewritten with garbage), and the
/// caller must exit non-zero so nobody believes an incomplete rotation succeeded.
///
/// Reads happen through the transaction on purpose: reading them from the pool
/// first would (a) read a different snapshot than the one being written and
/// (b) deadlock on a single-connection pool (`:memory:` in tests), which is how
/// this was caught.
pub async fn reseal_secrets(
    pool: &Pool,
    kp: &dyn KeyProvider,
) -> Result<ResealReport, sqlx::Error> {
    let mut report = ResealReport::default();
    let current = kp.version();

    let mut tx = begin_write(pool).await?;

    // ---- provider api-keys -------------------------------------------------
    let rows: Vec<(String, Vec<u8>, Vec<u8>, i64)> = sqlx::query_as(&dialect::sql(
        "SELECT id, api_key_ciphertext, api_key_nonce, key_version FROM provider_key ORDER BY id",
    ))
    .fetch_all(&mut *tx)
    .await?;
    for (id, ct, nonce, version) in rows {
        // A row whose version already equals the current one is NOT automatically
        // "already current". The version is an INDEPENDENT knob from the key
        // MATERIAL: rotate the key and forget to bump
        // `HYDRA_ENCRYPTION_KEY_VERSION` and every row still carries the old
        // number while the ring's slot for that number now holds different bytes.
        // Counting such a row as fine (and exiting 0) is the worst outcome in this
        // file, because the operator's next step is to delete the only key that can
        // open it — after which the process refuses to start with no way back. So
        // the claim is VERIFIED: the row must actually open under the current key,
        // which is what "already current" is supposed to mean.
        let version = version as u32;
        if version == current {
            match <[u8; crypto::NONCE_LEN]>::try_from(nonce.as_slice()) {
                Ok(nonce) => {
                    let sealed = Sealed {
                        ciphertext: ct,
                        nonce,
                        key_version: version,
                    };
                    if kp.open(&sealed).is_ok() {
                        report.already_current += 1;
                        continue;
                    }
                    report.failed.push(format!(
                        "provider_key {id}: labelled key_version {version} (the CURRENT \
                         version) but it does NOT open under the current key — the key was \
                         most likely rotated without bumping HYDRA_ENCRYPTION_KEY_VERSION. \
                         Do NOT delete the previous key until this is resolved"
                    ));
                }
                Err(_) => report
                    .failed
                    .push(format!("provider_key {id}: nonce has the wrong length")),
            }
            continue;
        }
        let Ok(nonce) = <[u8; crypto::NONCE_LEN]>::try_from(nonce.as_slice()) else {
            report
                .failed
                .push(format!("provider_key {id}: nonce has the wrong length"));
            continue;
        };
        let sealed = Sealed {
            ciphertext: ct,
            nonce,
            key_version: version,
        };
        match kp.open(&sealed) {
            Ok(plaintext) => match kp.seal(&plaintext) {
                Ok(fresh) => {
                    sqlx::query(&dialect::sql(
                        "UPDATE provider_key SET api_key_ciphertext = ?, api_key_nonce = ?, \
                         key_version = ? WHERE id = ?",
                    ))
                    .bind(&fresh.ciphertext)
                    .bind(fresh.nonce.to_vec())
                    .bind(i64::from(fresh.key_version))
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
                    report.provider_keys_resealed += 1;
                }
                Err(e) => report
                    .failed
                    .push(format!("provider_key {id}: seal failed: {e}")),
            },
            Err(e) => report.failed.push(format!(
                "provider_key {id}: cannot open key_version {version}: {e}"
            )),
        }
    }

    // ---- tenant certificate private keys ----------------------------------
    let cert_rows: Vec<(String, Vec<u8>, Vec<u8>, i64)> = sqlx::query_as(&dialect::sql(
        "SELECT id, cert_key_ciphertext, cert_key_nonce, cert_key_version FROM tenant \
         WHERE cert_key_ciphertext IS NOT NULL AND cert_key_nonce IS NOT NULL \
           AND cert_key_version IS NOT NULL ORDER BY id",
    ))
    .fetch_all(&mut *tx)
    .await?;
    for (id, ct, nonce, version) in cert_rows {
        // A row whose version already equals the current one is NOT automatically
        // "already current". The version is an INDEPENDENT knob from the key
        // MATERIAL: rotate the key and forget to bump
        // `HYDRA_ENCRYPTION_KEY_VERSION` and every row still carries the old
        // number while the ring's slot for that number now holds different bytes.
        // Counting such a row as fine (and exiting 0) is the worst outcome in this
        // file, because the operator's next step is to delete the only key that can
        // open it — after which the process refuses to start with no way back. So
        // the claim is VERIFIED: the row must actually open under the current key,
        // which is what "already current" is supposed to mean.
        let version = version as u32;
        if version == current {
            match <[u8; crypto::NONCE_LEN]>::try_from(nonce.as_slice()) {
                Ok(nonce) => {
                    let sealed = Sealed {
                        ciphertext: ct,
                        nonce,
                        key_version: version,
                    };
                    if kp.open(&sealed).is_ok() {
                        report.already_current += 1;
                        continue;
                    }
                    report.failed.push(format!(
                        "tenant {id} cert: labelled key_version {version} (the CURRENT \
                         version) but it does NOT open under the current key — the key was \
                         most likely rotated without bumping HYDRA_ENCRYPTION_KEY_VERSION. \
                         Do NOT delete the previous key until this is resolved"
                    ));
                }
                Err(_) => report
                    .failed
                    .push(format!("tenant {id} cert: nonce has the wrong length")),
            }
            continue;
        }
        let Ok(nonce) = <[u8; crypto::NONCE_LEN]>::try_from(nonce.as_slice()) else {
            report
                .failed
                .push(format!("tenant {id} cert: nonce has the wrong length"));
            continue;
        };
        let sealed = Sealed {
            ciphertext: ct,
            nonce,
            key_version: version,
        };
        match kp.open(&sealed) {
            Ok(plaintext) => match kp.seal(&plaintext) {
                Ok(fresh) => {
                    sqlx::query(&dialect::sql(
                        "UPDATE tenant SET cert_key_ciphertext = ?, cert_key_nonce = ?, \
                         cert_key_version = ? WHERE id = ?",
                    ))
                    .bind(&fresh.ciphertext)
                    .bind(fresh.nonce.to_vec())
                    .bind(i64::from(fresh.key_version))
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
                    report.tenant_certs_resealed += 1;
                }
                Err(e) => report
                    .failed
                    .push(format!("tenant {id} cert: seal failed: {e}")),
            },
            Err(e) => report.failed.push(format!(
                "tenant {id} cert: cannot open key_version {version}: {e}"
            )),
        }
    }

    // ---- limit_role.matching_key (decision D-16) --------------------------
    //
    // A third sealed column, added 2026-10-08. It has no `key_version` column of its own — the
    // version lives inside the envelope — so the "already current" test below reads it from there.
    // Two cases are handled, and they are different pieces of work:
    //   * a pre-D-16 PLAINTEXT value: sealing it is a MIGRATION, not a rotation, and this pass is
    //     the second of the two places that do it (the loader is the first, and the usual one);
    //   * a sealed value with an older version: the rotation this command exists for.
    let key_rows: Vec<(String, String)> = sqlx::query_as(&dialect::sql(
        "SELECT id, matching_key FROM limit_role WHERE matching_key IS NOT NULL ORDER BY id",
    ))
    .fetch_all(&mut *tx)
    .await?;
    for (id, value) in key_rows {
        let Some(sealed) = Sealed::from_text(&value) else {
            match kp.seal(value.as_bytes()) {
                Ok(fresh) => {
                    sqlx::query(&dialect::sql(
                        "UPDATE limit_role SET matching_key = ? WHERE id = ?",
                    ))
                    .bind(fresh.to_text())
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
                    report.limit_keys_resealed += 1;
                }
                Err(e) => report
                    .failed
                    .push(format!("limit_role {id} matching_key: seal failed: {e}")),
            }
            continue;
        };
        if sealed.key_version == current {
            // Same rule as the two blocks above: the version tag is an independent knob from the key
            // material, so "already current" is VERIFIED by opening the value, not assumed from the
            // number. Counting an unopenable row as fine is how the operator ends up deleting the
            // only key that can read it.
            match kp.open(&sealed) {
                Ok(_) => report.already_current += 1,
                Err(e) => report.failed.push(format!(
                    "limit_role {id} matching_key: labelled key_version {current} (the CURRENT \
                     version) but it does NOT open under the current key — the key was most likely \
                     rotated without bumping HYDRA_ENCRYPTION_KEY_VERSION. Do NOT delete the \
                     previous key until this is resolved: {e}"
                )),
            }
            continue;
        }
        match kp.open(&sealed) {
            Ok(plaintext) => match kp.seal(&plaintext) {
                Ok(fresh) => {
                    sqlx::query(&dialect::sql(
                        "UPDATE limit_role SET matching_key = ? WHERE id = ?",
                    ))
                    .bind(fresh.to_text())
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
                    report.limit_keys_resealed += 1;
                }
                Err(e) => report
                    .failed
                    .push(format!("limit_role {id} matching_key: seal failed: {e}")),
            },
            Err(e) => report.failed.push(format!(
                "limit_role {id} matching_key: cannot open key_version {}: {e}",
                sealed.key_version
            )),
        }
    }

    // One commit for both tables: a re-seal that failed halfway would leave the
    // rotation in a state where neither "before" nor "after" is true of the whole
    // DB, which is exactly what an operator cannot reason about.
    if report.provider_keys_resealed + report.tenant_certs_resealed + report.limit_keys_resealed > 0
    {
        bump_config_revision_on_conn(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(report)
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
/// `testing` helper; new tests should call [`testing::setup_pool`] directly.
#[cfg(test)]
pub(crate) async fn test_pool() -> Pool {
    testing::setup_pool().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{StaticKeyProvider, KEY_LEN};

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
