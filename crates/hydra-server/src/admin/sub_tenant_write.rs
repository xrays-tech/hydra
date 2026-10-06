//! Shared transactional sub-tenant / sub-tenant-route write core (sub-tenant
//! v2, D5 — fixes the v1 post-implementation findings 1 and 2).
//!
//! Every sub-tenant / route write (create / update / delete) runs in ONE
//! SQLite transaction:
//!
//! 1. `db::begin_write(pool)` — `BEGIN IMMEDIATE`, so the write lock is held
//!    before step 2's read. A DEFERRED begin would let the later write fail
//!    with `SQLITE_BUSY`, bypassing `busy_timeout`, whenever another
//!    connection committed in between (see [`crate::db::begin_write`]),
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
//! called by the v1 admin handlers (`admin::handlers`), by the shared
//! plane-agnostic write core ([`apply_config_write`], which the data-plane
//! self-service write path calls) and by nothing else. It is **not HTTP-specific**: it takes a
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

/// Create a sub-tenant inside one transaction.
///
/// **The "idempotent upsert by `(tenant_id, name)`" (D4) guarantee belongs to the
/// TENANT-facing `PUT /tenant/{tid}/api/v1/sub-tenants/{name}` route**, which looks
/// the row up first and then calls [`update_sub_tenant`] with `self_id = Some(id)`
/// THIS function is the CREATE path: both of its
/// branches pass `self_id = None` (below), so a repeated create — a retry after a
/// leader failover, an idempotent replay, a double submit — hits the validator's
/// `NameDuplicate` and answers **409** instead of converging. (The
/// `ON CONFLICT (tenant_id, name) DO UPDATE` in `db::insert_sub_tenant` is still
/// reachable, but only for a CONCURRENT pair of creates that both saw no existing
/// row — it does not make a SEQUENTIAL repeat converge.) This
/// comment used to promise convergence on this path too, which was wrong; whether
/// the ADMIN create should also converge is a product decision (see the plan's
/// D-10), not something to change silently.
///
/// `key_prefix = None` ⇒ auto-generate, retrying (up to [`PREFIX_ATTEMPTS`]) on
/// BOTH the validator's overlap rejection AND a DB unique conflict.
/// `key_prefix = Some(p)` ⇒ validate once, no retry. `id` is the row id used
/// when a NEW row is inserted.
pub async fn create_sub_tenant(
    pool: &SqlitePool,
    cfg: &ConfigData,
    tenant_id: &str,
    name: &str,
    key_prefix: Option<&str>,
    id: &str,
    enabled: bool,
) -> Result<SubTenant, CoreError> {
    let mut tx = crate::db::begin_write(pool).await.map_err(CoreError::Db)?;
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
    let mut tx = crate::db::begin_write(pool).await.map_err(CoreError::Db)?;
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
    let mut tx = crate::db::begin_write(pool).await.map_err(CoreError::Db)?;
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
    let mut tx = crate::db::begin_write(pool).await.map_err(CoreError::Db)?;
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
    let mut tx = crate::db::begin_write(pool).await.map_err(CoreError::Db)?;
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
    let mut tx = crate::db::begin_write(pool).await.map_err(CoreError::Db)?;
    db::delete_sub_tenant_route_on(&mut *tx, id)
        .await
        .map_err(CoreError::Db)?;
    tx.commit().await.map_err(CoreError::Db)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The plane-agnostic tenant config write (moved here when the internal endpoint family retired)
// ---------------------------------------------------------------------------
//
// This lived in the module that also served the leader's INTERNAL write endpoint
// (`/api/v1/internal/tenant-config/*`). That endpoint family is retired (ADR-0001 D-6, plan T3.5)
// because the entry node now applies the write itself, and this core is the part that survived: it
// is the single write point, and it never depended on `AdminState`, `ServerSession` or HTTP.

// ---------------------------------------------------------------------------
// The plane-agnostic tenant config write (D5: admin internal + data-plane local)
// ---------------------------------------------------------------------------

/// The parsed tenant config write — the A-2 "authorised work" — independent of
/// which plane (the leader internal handler, or the data-plane local path, D8)
/// produced it. The write's declared target (`tenant_id` / `sub_tenant_id`) is
/// checked against the authenticated tenant by [`apply_config_write`] (the
/// binding gate), so a body that names another tenant is refused before any
/// lookup.
#[derive(Clone, Debug)]
pub(crate) enum TenantConfigWrite {
    /// `PUT .../sub-tenants` — upsert by natural key `(tenant_id, name)` (D4).
    /// Idempotent: a repeated PUT converges to the same row.
    UpsertSubTenant {
        tenant_id: String,
        name: String,
        key_prefix: Option<String>,
        enabled: bool,
    },
    /// `DELETE .../sub-tenants/{id}` — delete by immutable id (D4). Idempotent:
    /// an absent id is a no-op; a foreign id is a resource-scoped 404.
    DeleteSubTenant { id: String },
    /// `PUT .../sub-tenant-routes` — upsert by `(sub_tenant_id, model_key)` (D4,
    /// `None` = the default route). The sub-tenant must exist and be the
    /// authenticated tenant's own.
    UpsertRoute {
        sub_tenant_id: String,
        model_key: Option<String>,
        provider_id: String,
        enabled: bool,
    },
    /// `DELETE .../sub-tenant-routes/{id}` — delete by immutable id (D4).
    /// Idempotent: an absent id is a no-op; a foreign route is a 404.
    DeleteRoute { id: String },
}

/// The resource a successful write produced (the caller reports it with the
/// `config_version` it is live at), or the idempotent delete marker.
pub(crate) enum WriteOutcome {
    SubTenant(SubTenant),
    Route(SubTenantRoute),
    /// `DELETE` — idempotent. The id is deliberately NOT carried: the only consumer was the
    /// leader's internal response envelope, which is retired (D-6 / T3.5), and the data-plane face
    /// answers 204 with no body. Keeping the field would have kept a value nobody reads — which the
    /// compiler flagged the moment the internal face went away.
    Deleted,
}

/// Why a tenant config write was refused — by the A-2 binding gate (before the
/// write core) or by the write core itself. The caller maps each variant to a
/// precise HTTP response (the admin face and the data plane map to their own
/// envelopes, but the same variant → the same status/code).
#[derive(Debug)]
pub(crate) enum ApplyError {
    /// The write target tenant does not match the authenticated tenant (403).
    /// A pure string comparison, before any lookup — never a tenant-existence
    /// oracle.
    TenantMismatch,
    /// The resource does not exist, or belongs to another tenant (404). Missing
    /// and foreign are deliberately indistinguishable (no oracle).
    NotFound,
    /// A transactional write-core failure (400/409/500).
    Core(CoreError),
}

/// Apply a tenant config write: run the A-2 **binding gate** (the write's
/// declared target must be the authenticated tenant's own resources), then the
/// shared **transactional write core** (`sub_tenant_write`). This is the SINGLE
/// write point shared by the leader internal handler (V2/V3) and the data-plane
/// local path (V5, D8) — so the two faces cannot diverge on validation, quota or
/// natural-key upsert semantics.
///
/// It does **not** depend on `AdminState` or `ServerSession`: it takes the
/// config [`SqlitePool`], the current [`ConfigData`] (provider / model / tenant
/// membership), the authenticated tenant id (the write target, resolved from the
/// tenant token) and the parsed write. It performs the write and returns the
/// produced resource, leaving the snapshot **reload** to the caller — the admin
/// face uses the `reload_best_effort` helper (serialising lock + stale flag),
/// the data-plane local path calls `ConfigStore::reload_all` directly
/// (single-node: the node is the only writer).
pub(crate) async fn apply_config_write(
    pool: &SqlitePool,
    cfg: &ConfigData,
    tenant_id: &str,
    write: &TenantConfigWrite,
) -> Result<WriteOutcome, ApplyError> {
    match write {
        TenantConfigWrite::UpsertSubTenant {
            tenant_id: target,
            name,
            key_prefix,
            enabled,
        } => {
            // A-2 binding: the write target must be the authenticated tenant, by
            // pure string comparison BEFORE any lookup (never a tenant-existence
            // oracle).
            if target != tenant_id {
                return Err(ApplyError::TenantMismatch);
            }
            // D4 idempotent upsert by natural key `(tenant_id, name)`: resolve the
            // existing row and CONVERGE (update by its immutable id), or create it
            // (the atomic upsert handles a concurrent race). An omitted
            // `key_prefix` keeps the current one on an existing row and
            // auto-generates (Q13 / F3) on a new one.
            let existing = match crate::db::list_sub_tenants(pool).await {
                Ok(rows) => rows
                    .into_iter()
                    .find(|s| s.tenant_id == *target && s.name == *name),
                Err(e) => return Err(ApplyError::Core(CoreError::Db(e))),
            };
            let st = match existing {
                Some(st) => {
                    let prefix = key_prefix.clone().unwrap_or_else(|| st.key_prefix.clone());
                    update_sub_tenant(pool, cfg, &st.id, name, &prefix, *enabled)
                        .await
                        .map_err(ApplyError::Core)?
                }
                None => {
                    let id = crate::admin::handlers::gen_id();
                    create_sub_tenant(
                        pool,
                        cfg,
                        target,
                        name,
                        key_prefix.as_deref(),
                        &id,
                        *enabled,
                    )
                    .await
                    .map_err(ApplyError::Core)?
                }
            };
            Ok(WriteOutcome::SubTenant(st))
        }
        TenantConfigWrite::DeleteSubTenant { id } => {
            // A-2 binding: the row (if present) must be the tenant's own. A
            // foreign row is 404 (resource-scoped; never written); an absent row
            // is an idempotent no-op.
            match crate::db::get_sub_tenant(pool, id).await {
                Ok(st) if st.tenant_id == tenant_id => {}
                Ok(_) => return Err(ApplyError::NotFound),
                Err(sqlx::Error::RowNotFound) => return Ok(WriteOutcome::Deleted),
                Err(e) => return Err(ApplyError::Core(CoreError::Db(e))),
            }
            delete_sub_tenant(pool, id)
                .await
                .map_err(ApplyError::Core)?;
            Ok(WriteOutcome::Deleted)
        }
        TenantConfigWrite::UpsertRoute {
            sub_tenant_id,
            model_key,
            provider_id,
            enabled,
        } => {
            // A-2 binding: the sub-tenant must exist and be the tenant's own.
            // Missing OR foreign → 404 (resource-scoped; indistinguishable, no
            // oracle).
            let st = match crate::db::get_sub_tenant(pool, sub_tenant_id).await {
                Ok(st) => st,
                Err(sqlx::Error::RowNotFound) => return Err(ApplyError::NotFound),
                Err(e) => return Err(ApplyError::Core(CoreError::Db(e))),
            };
            if st.tenant_id != tenant_id {
                return Err(ApplyError::NotFound);
            }
            let id = crate::admin::handlers::gen_id();
            let r = create_route(
                pool,
                cfg,
                sub_tenant_id,
                provider_id,
                model_key.as_deref(),
                &id,
                *enabled,
            )
            .await
            .map_err(ApplyError::Core)?;
            Ok(WriteOutcome::Route(r))
        }
        TenantConfigWrite::DeleteRoute { id } => {
            // A-2 binding: the route (if present) must belong to the tenant's
            // sub-tenant. A foreign sub-tenant is 404 (resource-scoped); an
            // absent row is a no-op.
            match crate::db::get_sub_tenant_route(pool, id).await {
                Ok(r) => {
                    let st = match crate::db::get_sub_tenant(pool, &r.sub_tenant_id).await {
                        Ok(st) => st,
                        // A dangling sub-tenant is impossible (FK CASCADE); treat
                        // as a no-op so a replay after a cascade cannot 404.
                        Err(sqlx::Error::RowNotFound) => return Ok(WriteOutcome::Deleted),
                        Err(e) => return Err(ApplyError::Core(CoreError::Db(e))),
                    };
                    if st.tenant_id != tenant_id {
                        return Err(ApplyError::NotFound);
                    }
                }
                Err(sqlx::Error::RowNotFound) => return Ok(WriteOutcome::Deleted),
                Err(e) => return Err(ApplyError::Core(CoreError::Db(e))),
            }
            delete_route(pool, id).await.map_err(ApplyError::Core)?;
            Ok(WriteOutcome::Deleted)
        }
    }
}
