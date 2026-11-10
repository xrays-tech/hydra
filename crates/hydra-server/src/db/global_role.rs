//! Global-role and global-role-provider reads and writes.

use std::collections::HashMap;

use hydra_core::model::GlobalRole;

use super::dialect;
use super::pool::{begin_write, bump_config_revision_on_conn, commit_write};
use super::rows::{flag, fmt_ts, parse_ts, GlobalRoleProviderRow, GlobalRoleRow};
use super::{Db, DbConnection, Pool};

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
