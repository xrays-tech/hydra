//! Append-only admin mutation audit (H6.6).
//!
//! The admin router calls [`insert_admin_audit`] best-effort AFTER a handler
//! returns. This is intentionally a plain pool write, NOT `begin_write`: the
//! audit row is not part of the config-write transaction, and an audit failure
//! must never roll back or fail the mutation it describes.

use super::{dialect, Pool};

/// One redacted admin mutation attempt.
///
/// `actor` is the admin surface ("admin"); `action` is the lowercased verb.
/// `entity_type`/`request_id` are optional because an unknown path or a caller
/// without a trace id is still a mutation attempt worth recording. No body,
/// credential or provider key is ever carried here.
pub struct AdminAudit<'a> {
    pub actor: &'a str,
    pub action: &'a str,
    pub method: &'a str,
    pub path: &'a str,
    pub entity_type: Option<&'a str>,
    pub request_id: Option<&'a str>,
}

/// Insert one audit row on the shared pool.
///
/// Callers must treat an `Err` as degraded observability, not as a failed
/// mutation: the mutation already ran and its response is authoritative.
pub async fn insert_admin_audit(pool: &Pool, entry: &AdminAudit<'_>) -> Result<(), sqlx::Error> {
    sqlx::query(&dialect::sql(
        "INSERT INTO admin_audit (actor, action, method, path, entity_type, request_id) \
         VALUES (?, ?, ?, ?, ?, ?)",
    ))
    .bind(entry.actor)
    .bind(entry.action)
    .bind(entry.method)
    .bind(entry.path)
    .bind(entry.entity_type)
    .bind(entry.request_id)
    .execute(pool)
    .await?;
    Ok(())
}
