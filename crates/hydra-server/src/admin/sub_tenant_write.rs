//! Shared transactional sub-tenant / sub-tenant-route write core (sub-tenant
//! v2, D5 — fixes the v1 post-implementation findings 1 and 2).
//!
//! Every sub-tenant / route write (create / update / delete) runs in ONE
//! SQLite transaction:
//!
//! 1. `pool.begin()`,
//! 2. read the **full** set of sub-tenant / route rows from the DB **inside**
//!    the transaction (`db::list_sub_tenants_on` / `db::list_sub_tenant_routes_on`
//!    — ALL rows, including disabled),
//! 3. validate the write against that live row snapshot **plus** the current
//!    [`ConfigData`] via [`validate_sub_tenant_write_against`] — so the quota
//!    counts **all** rows (not just the enabled snapshot, finding 1) and the
//!    prefix-overlap check runs against what is actually in the DB (closing
//!    the validate-then-insert TOCTOU, finding 2),
//! 4. perform the insert / upsert / update / delete on the transaction,
//! 5. `tx.commit()`.
//!
//! This module is the **single write path** for sub-tenant config. It is
//! called by the v1 admin handlers (`admin::handlers`) now and by the v2
//! leader internal handler later. It is **not HTTP-specific**: it takes a
//! pool + the current [`ConfigData`] and returns typed results /
//! [`CoreError`]s, never an HTTP `Resp`.
//!
//! The pure validation lives in `hydra_core::sub_tenant` (no I/O); this module
//! only wraps it in a transaction that reads the live rows first.

use hydra_core::config::ConfigData;
use hydra_core::model::{SubTenant, SubTenantRoute};
use hydra_core::sub_tenant::{
    validate_sub_tenant_write_against, SubTenantRows, SubTenantWrite, SubTenantWriteError,
};
use sqlx::SqlitePool;

use crate::db;

/// A transactional sub-tenant / route write failed. The admin layer maps each
/// variant to a precise HTTP response (400/409/500/404).
#[derive(Debug)]
pub enum CoreError {
    /// A single rule violation from the in-transaction validation. Mapped to a
    /// precise 400/409 by `sub_tenant_write_err_resp` (duplicates are 409,
    /// every other rule violation is 400).
    Validation(SubTenantWriteError),
    /// A database failure (I/O, an unexpected constraint, ...). Mapped to 500
    /// by `db_err_resp`.
    Db(sqlx::Error),
    /// An auto-generated `key_prefix` could not be made non-conflicting within
    /// [`PREFIX_ATTEMPTS`] attempts. Mapped to 400 `prefix_generation_failed`.
    PrefixGenerationFailed,
    /// The sub-tenant / route being updated (or the sub-tenant a route points
    /// at) does not exist. Mapped to 404.
    NotFound,
}

/// Number of auto-`key_prefix` generation attempts on create (Q13 / F3): the
/// loop must retry on BOTH the validator's overlap rejection AND a DB
/// `UNIQUE(tenant_id, key_prefix)` conflict (a generated 9-char prefix can be a
/// superstring of an existing shorter one, or lose a race).
pub const PREFIX_ATTEMPTS: u32 = 5;

/// Generate an 8-char `[A-Z0-9]` sub-tenant `key_prefix` plus the mandatory
/// `_` separator (Q13). The trailing `_` guarantees a separator so a bare
/// `starts_with` cannot swallow a longer, unrelated prefix.
pub fn generate_sub_tenant_prefix() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    let body: String = (0..8)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect();
    format!("{body}_")
}

/// True when the sqlx error is a UNIQUE-constraint violation (a key_prefix /
/// name / id collision the DB backstop caught).
fn is_unique_conflict(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.is_unique_violation())
}

/// Read the full sub-tenant / route row snapshot from within a transaction
/// (ALL rows, including disabled — the basis for the all-rows quota and
/// overlap checks, A-2 prerequisites 1/2). Takes the transaction's
/// **connection** (`&mut *tx`, where `tx` is the owned `sqlx::Transaction`).
async fn read_rows(
    tx: &mut sqlx::sqlite::SqliteConnection,
) -> Result<(Vec<SubTenant>, Vec<SubTenantRoute>), sqlx::Error> {
    let sub_tenants = db::list_sub_tenants_on(&mut *tx).await?;
    let routes = db::list_sub_tenant_routes_on(&mut *tx).await?;
    Ok((sub_tenants, routes))
}

/// Validate a sub-tenant create/update against the in-transaction row snapshot.
fn validate_sub_tenant_write_in_tx(
    cfg: &ConfigData,
    rows: &SubTenantRows<'_>,
    tenant_id: &str,
    name: &str,
    key_prefix: &str,
    self_id: Option<&str>,
) -> Result<(), SubTenantWriteError> {
    validate_sub_tenant_write_against(
        cfg,
        rows,
        &SubTenantWrite::SubTenant {
            tenant_id: tenant_id.to_string(),
            name: name.to_string(),
            key_prefix: key_prefix.to_string(),
            sub_tenant_id: self_id.map(str::to_string),
        },
    )
}

/// Validate a route create/update against the in-transaction row snapshot.
fn validate_route_in_tx(
    cfg: &ConfigData,
    rows: &SubTenantRows<'_>,
    tenant_id: &str,
    sub_tenant_id: &str,
    provider_id: &str,
    model_key: Option<&str>,
    self_route_id: Option<&str>,
) -> Result<(), SubTenantWriteError> {
    validate_sub_tenant_write_against(
        cfg,
        rows,
        &SubTenantWrite::Route {
            tenant_id: tenant_id.to_string(),
            sub_tenant_id: sub_tenant_id.to_string(),
            provider_id: provider_id.to_string(),
            model_key: model_key.map(str::to_string),
            route_id: self_route_id.map(str::to_string),
        },
    )
}

// ---------------------------------------------------------------------------
// Sub-tenant operations
// ---------------------------------------------------------------------------

/// Create a sub-tenant (idempotent upsert by natural key `(tenant_id, name)`,
/// D4) inside one transaction.
///
/// `key_prefix = None` ⇒ auto-generate, retrying (up to [`PREFIX_ATTEMPTS`]) on
/// BOTH the validator's overlap rejection AND a DB unique conflict.
/// `key_prefix = Some(p)` ⇒ validate once, no retry. `id` is the row id used
/// when a NEW row is inserted; on a name conflict the existing row's id is
/// retained, so a retry after a leader failover converges to the same row
/// (A-2 prerequisite 4).
pub async fn create_sub_tenant(
    pool: &SqlitePool,
    cfg: &ConfigData,
    tenant_id: &str,
    name: &str,
    key_prefix: Option<&str>,
    id: &str,
    enabled: bool,
) -> Result<SubTenant, CoreError> {
    let mut tx = pool.begin().await.map_err(CoreError::Db)?;
    let (sub_tenants, routes) = read_rows(&mut tx).await.map_err(CoreError::Db)?;
    let rows = SubTenantRows {
        sub_tenants: &sub_tenants,
        routes: &routes,
    };

    match key_prefix {
        Some(prefix) => {
            // Explicit prefix (including an empty string): validate once
            // (fail-closed), no retry, then upsert.
            validate_sub_tenant_write_in_tx(cfg, &rows, tenant_id, name, prefix, None)
                .map_err(CoreError::Validation)?;
            let st = db::upsert_sub_tenant_by_name(&mut tx, id, tenant_id, name, prefix, enabled)
                .await
                .map_err(CoreError::Db)?;
            tx.commit().await.map_err(CoreError::Db)?;
            Ok(st)
        }
        None => {
            // Auto-generate (Q13 / F3): generate → validate → upsert, retrying
            // on BOTH the validator's overlap rejection AND a DB UNIQUE conflict.
            // The generated prefix is the only thing we retry on — every other
            // validation failure is fatal.
            for _ in 0..PREFIX_ATTEMPTS {
                let prefix = generate_sub_tenant_prefix();
                match validate_sub_tenant_write_in_tx(cfg, &rows, tenant_id, name, &prefix, None) {
                    Ok(()) => {
                        match db::upsert_sub_tenant_by_name(
                            &mut tx, id, tenant_id, name, &prefix, enabled,
                        )
                        .await
                        {
                            Ok(st) => {
                                tx.commit().await.map_err(CoreError::Db)?;
                                return Ok(st);
                            }
                            Err(e) if is_unique_conflict(&e) => continue, // race: retry
                            Err(e) => return Err(CoreError::Db(e)),
                        }
                    }
                    // F3: an auto-generated prefix can be a superstring of an
                    // existing shorter prefix — overlap is retryable.
                    Err(SubTenantWriteError::PrefixOverlap) => continue,
                    Err(e) => return Err(CoreError::Validation(e)),
                }
            }
            Err(CoreError::PrefixGenerationFailed)
        }
    }
}

/// Update a sub-tenant by its immutable `id` (from the URL) inside one
/// transaction. The row's `tenant_id` is immutable and authoritative from the
/// existing row (the update never changes it). Returns the updated row (re-read
/// after commit so the server-set `updated_at` is accurate).
pub async fn update_sub_tenant(
    pool: &SqlitePool,
    cfg: &ConfigData,
    id: &str,
    name: &str,
    key_prefix: &str,
    enabled: bool,
) -> Result<SubTenant, CoreError> {
    let mut tx = pool.begin().await.map_err(CoreError::Db)?;
    let (sub_tenants, routes) = read_rows(&mut tx).await.map_err(CoreError::Db)?;
    let rows = SubTenantRows {
        sub_tenants: &sub_tenants,
        routes: &routes,
    };

    // The row being updated must exist in the live snapshot; its tenant_id is
    // immutable and authoritative (the update does not change it).
    let existing = sub_tenants
        .iter()
        .find(|s| s.id == id)
        .ok_or(CoreError::NotFound)?;

    validate_sub_tenant_write_in_tx(cfg, &rows, &existing.tenant_id, name, key_prefix, Some(id))
        .map_err(CoreError::Validation)?;

    let st = SubTenant {
        id: id.to_string(),
        tenant_id: existing.tenant_id.clone(),
        name: name.to_string(),
        key_prefix: key_prefix.to_string(),
        enabled,
        created_at: existing.created_at.clone(),
        updated_at: String::new(), // set server-side to datetime('now')
    };
    db::update_sub_tenant_on(&mut *tx, &st)
        .await
        .map_err(CoreError::Db)?;
    tx.commit().await.map_err(CoreError::Db)?;

    // Re-read after commit so the returned row carries the server-set
    // `updated_at`. (The row was just committed; a missing read here is a
    // genuine not-found, matching the v1 handler's final read.)
    match db::get_sub_tenant(pool, id).await {
        Ok(st) => Ok(st),
        Err(sqlx::Error::RowNotFound) => Err(CoreError::NotFound),
        Err(e) => Err(CoreError::Db(e)),
    }
}

/// Delete a sub-tenant by its immutable `id` inside one transaction. Idempotent:
/// deleting an absent id is a no-op success (never deletes by name, so a
/// late replay cannot remove a later-created same-name row — A-2 prerequisite
/// 4).
pub async fn delete_sub_tenant(pool: &SqlitePool, id: &str) -> Result<(), CoreError> {
    let mut tx = pool.begin().await.map_err(CoreError::Db)?;
    db::delete_sub_tenant_on(&mut *tx, id)
        .await
        .map_err(CoreError::Db)?;
    tx.commit().await.map_err(CoreError::Db)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Sub-tenant-route operations
// ---------------------------------------------------------------------------

/// Create a route (idempotent upsert by natural key `(sub_tenant_id,
/// model_key)`, D4) inside one transaction. `model_key = None` ⇒ the
/// sub-tenant's default route (the partial unique index on `model_key IS NULL`);
/// `model_key = Some(m)` ⇒ a model-specific override. The sub-tenant (FK) must
/// exist; its `tenant_id` is authoritative for validation.
pub async fn create_route(
    pool: &SqlitePool,
    cfg: &ConfigData,
    sub_tenant_id: &str,
    provider_id: &str,
    model_key: Option<&str>,
    id: &str,
    enabled: bool,
) -> Result<SubTenantRoute, CoreError> {
    let mut tx = pool.begin().await.map_err(CoreError::Db)?;
    let (sub_tenants, routes) = read_rows(&mut tx).await.map_err(CoreError::Db)?;
    let rows = SubTenantRows {
        sub_tenants: &sub_tenants,
        routes: &routes,
    };

    // Resolve the sub-tenant (FK) to obtain its authoritative tenant_id.
    let st = sub_tenants
        .iter()
        .find(|s| s.id == sub_tenant_id)
        .ok_or(CoreError::NotFound)?;

    // The natural key this upsert targets: if a row already exists for
    // `(sub_tenant_id, model_key)`, the quota must NOT count it — the upsert
    // updates it in place. Without this, a retried PUT at quota would be
    // rejected 400 instead of converging (A-2 理由 4: PUT-upsert must be
    // idempotent even after a 504 retry at the quota boundary).
    let existing_id = routes
        .iter()
        .find(|r| r.sub_tenant_id == sub_tenant_id && r.model_key.as_deref() == model_key)
        .map(|r| r.id.as_str());

    validate_route_in_tx(
        cfg,
        &rows,
        &st.tenant_id,
        sub_tenant_id,
        provider_id,
        model_key,
        existing_id,
    )
    .map_err(CoreError::Validation)?;

    let r = db::upsert_sub_tenant_route_by_key(
        &mut tx,
        id,
        sub_tenant_id,
        model_key,
        provider_id,
        enabled,
    )
    .await
    .map_err(CoreError::Db)?;
    tx.commit().await.map_err(CoreError::Db)?;
    Ok(r)
}

/// Update a route by its immutable `id` (from the URL) inside one
/// transaction. `sub_tenant_id` is immutable and authoritative from the
/// existing row. Returns the updated route (re-read after commit).
pub async fn update_route(
    pool: &SqlitePool,
    cfg: &ConfigData,
    id: &str,
    provider_id: &str,
    model_key: Option<&str>,
    enabled: bool,
) -> Result<SubTenantRoute, CoreError> {
    let mut tx = pool.begin().await.map_err(CoreError::Db)?;
    let (sub_tenants, routes) = read_rows(&mut tx).await.map_err(CoreError::Db)?;
    let rows = SubTenantRows {
        sub_tenants: &sub_tenants,
        routes: &routes,
    };

    // The route being updated must exist; its sub_tenant_id is immutable and
    // authoritative.
    let existing = routes
        .iter()
        .find(|r| r.id == id)
        .ok_or(CoreError::NotFound)?;
    let sub_tenant_id: &str = &existing.sub_tenant_id;

    // Resolve the sub-tenant (FK) for its authoritative tenant_id.
    let st = sub_tenants
        .iter()
        .find(|s| s.id == sub_tenant_id)
        .ok_or(CoreError::NotFound)?;

    validate_route_in_tx(
        cfg,
        &rows,
        &st.tenant_id,
        sub_tenant_id,
        provider_id,
        model_key,
        Some(id),
    )
    .map_err(CoreError::Validation)?;

    let r = SubTenantRoute {
        id: id.to_string(),
        sub_tenant_id: sub_tenant_id.to_string(),
        model_key: model_key.map(str::to_string),
        provider_id: provider_id.to_string(),
        enabled,
        created_at: existing.created_at.clone(),
        updated_at: String::new(), // set server-side to datetime('now')
    };
    db::update_sub_tenant_route_on(&mut *tx, &r)
        .await
        .map_err(CoreError::Db)?;
    tx.commit().await.map_err(CoreError::Db)?;

    match db::get_sub_tenant_route(pool, id).await {
        Ok(r) => Ok(r),
        Err(sqlx::Error::RowNotFound) => Err(CoreError::NotFound),
        Err(e) => Err(CoreError::Db(e)),
    }
}

/// Delete a route by its immutable `id` inside one transaction. Idempotent:
/// deleting an absent id is a no-op success.
pub async fn delete_route(pool: &SqlitePool, id: &str) -> Result<(), CoreError> {
    let mut tx = pool.begin().await.map_err(CoreError::Db)?;
    db::delete_sub_tenant_route_on(&mut *tx, id)
        .await
        .map_err(CoreError::Db)?;
    tx.commit().await.map_err(CoreError::Db)?;
    Ok(())
}
