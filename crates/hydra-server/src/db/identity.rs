//! User-group, user, user-api-key, and tenant-access-token reads and writes.

use hydra_core::model::{BillingStatus, TenantAccessToken, User, UserApiKey, UserGroup};

use super::dialect;
use super::pool::{begin_write, bump_config_revision_on_conn, commit_write};
use super::rows::{
    flag, parse_ts, parse_ts_opt, TenantAccessTokenRow, UserApiKeyRow, UserGroupRow, UserRow,
};
use super::{Db, DbConnection, Pool};

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

/// Delete one user's api-key, scoping the statement by ALL THREE ids.
///
/// The route's 404 contract is "no such key **for this user**", not "some key
/// of this tenant": `id`, `user_id`, and the user's `tenant_id` are predicates
/// of the same `DELETE`, so a key that exists under another user or another
/// tenant deletes 0 rows and is indistinguishable from a missing id. Folding
/// the ownership proof into the statement also removes the old
/// check-then-delete TOCTOU window.
pub async fn delete_user_api_key(
    pool: &Pool,
    tenant_id: &str,
    user_id: &str,
    key_id: &str,
) -> Result<u64, sqlx::Error> {
    let mut tx = begin_write(pool).await?;
    let affected = sqlx::query(&dialect::sql(
        "DELETE FROM user_api_key WHERE id = ? AND user_id = ? \
         AND user_id IN (SELECT id FROM hydra_user WHERE tenant_id = ?)",
    ))
    .bind(key_id)
    .bind(user_id)
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
