//! Append-only admin/tenant mutation audit (H6.6, F2).
//!
//! The admin router calls [`insert_admin_audit`] best-effort AFTER a handler
//! returns, and the tenant API calls it after the post-gate write handler runs.
//! This is intentionally a plain pool write, NOT `begin_write`: the audit row is
//! not part of the config-write transaction, and an audit failure must never
//! roll back or fail the mutation it describes.
//!
//! [`run_retention_once`] / [`spawn_retention_task`] implement the retention
//! decision: rows older than `HYDRA_ADMIN_AUDIT_RETENTION_DAYS` (default 90;
//! `0` = keep forever) are deleted by a **plain DELETE**, deliberately NOT
//! `begin_write`: `admin_audit` lives outside `hydra_lock`/`config_revision`, so
//! the cleanup neither takes the write lock nor fans out a config reload.

use super::{dialect, Pool};

/// Default retention window for `admin_audit` rows, in days
/// (`HYDRA_ADMIN_AUDIT_RETENTION_DAYS`). `0` means "keep forever".
pub const DEFAULT_ADMIN_AUDIT_RETENTION_DAYS: u64 = 90;

/// Largest retention window the cleanup will honour, in days (~1000 years).
///
/// Not a policy bound: it keeps `chrono::Duration::days` from overflowing for an
/// absurd operator value while still meaning "keep everything" in practice.
const MAX_RETENTION_DAYS: u64 = 365_000;

/// One redacted mutation attempt.
///
/// `actor` is the admin surface (`admin`) or the AUTHENTICATED tenant id for a
/// tenant self-service write; `action` is the lowercased verb. `tenant_id` is
/// `Some` only for a tenant-scoped row (and, for a tenant write, equals `actor`
/// — the URL claim is never used). `entity_type`/`request_id` are optional
/// because an unknown path or a caller without a trace id is still a mutation
/// attempt worth recording. No body, credential or provider key is ever carried
/// here.
pub struct AdminAudit<'a> {
    pub actor: &'a str,
    pub action: &'a str,
    pub method: &'a str,
    pub path: &'a str,
    pub entity_type: Option<&'a str>,
    pub request_id: Option<&'a str>,
    /// The tenant this row is scoped to, if any (`None` for an admin row).
    pub tenant_id: Option<&'a str>,
}

/// Insert one audit row on the shared pool.
///
/// Callers must treat an `Err` as degraded observability, not as a failed
/// mutation: the mutation already ran and its response is authoritative.
pub async fn insert_admin_audit(pool: &Pool, entry: &AdminAudit<'_>) -> Result<(), sqlx::Error> {
    sqlx::query(&dialect::sql(
        "INSERT INTO admin_audit \
         (actor, action, method, path, entity_type, request_id, tenant_id) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    ))
    .bind(entry.actor)
    .bind(entry.action)
    .bind(entry.method)
    .bind(entry.path)
    .bind(entry.entity_type)
    .bind(entry.request_id)
    .bind(entry.tenant_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// The configured retention window, in days.
///
/// `HYDRA_ADMIN_AUDIT_RETENTION_DAYS` (default
/// [`DEFAULT_ADMIN_AUDIT_RETENTION_DAYS`]). A negative value is clamped to `0`
/// ("keep forever"); a missing or unparseable value falls back to the default
/// (a typo must not silently shorten the window to zero, nor turn off cleanup).
#[must_use]
pub fn admin_audit_retention_days_from_env() -> u64 {
    std::env::var("HYDRA_ADMIN_AUDIT_RETENTION_DAYS")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .map(|days| days.max(0) as u64)
        .unwrap_or(DEFAULT_ADMIN_AUDIT_RETENTION_DAYS)
        .min(MAX_RETENTION_DAYS)
}

/// Run one retention pass using the configured window, returning the number of
/// rows deleted.
///
/// `0` (keep forever) is a no-op that returns `0` without touching the database.
pub async fn run_retention_once(pool: &Pool) -> Result<u64, sqlx::Error> {
    delete_admin_audit_older_than(pool, admin_audit_retention_days_from_env()).await
}

/// Delete `admin_audit` rows older than `days`, returning the deleted count.
///
/// Exposed separately from [`run_retention_once`] so tests (and any future
/// operator command) can pin a window without racing on the process-wide
/// environment. `days == 0` is the documented "keep forever" no-op.
///
/// This is a **plain DELETE** on a dedicated table, deliberately not wrapped in
/// `begin_write`: no `hydra_lock`, no `config_revision` bump, no reload fan-out.
/// The `created_at` column is set by the database on insert; the cutoff is bound
/// as a timestamp so one portable statement serves both dialects.
pub async fn delete_admin_audit_older_than(pool: &Pool, days: u64) -> Result<u64, sqlx::Error> {
    if days == 0 {
        return Ok(0);
    }
    let days = days.min(MAX_RETENTION_DAYS) as i64;
    let cutoff = chrono::Utc::now().naive_utc() - chrono::Duration::days(days);
    let result = sqlx::query(&dialect::sql(
        "DELETE FROM admin_audit WHERE created_at < ?",
    ))
    .bind(cutoff)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Spawn the daily audit-retention task.
///
/// The task runs one pass immediately (a restart must not leave a backlog until
/// the next midnight) and then once per day. Every error is logged and never
/// propagated: retention is housekeeping, not part of any request. The number of
/// deleted rows is published as `hydra_admin_audit_deleted_total` so a broken or
/// disabled window is visible in the operator metrics.
///
/// Public so `main` (outside this module) can wire it at boot; wire-free in the
/// sense that this module does not edit `main.rs`.
pub fn spawn_retention_task(pool: Pool) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(24 * 60 * 60));
        loop {
            // The first tick fires immediately; later ticks are one day apart.
            ticker.tick().await;
            match run_retention_once(&pool).await {
                Ok(deleted) => {
                    #[cfg(feature = "proxy")]
                    crate::admin::metrics::record_admin_audit_deleted(deleted);
                    if deleted > 0 {
                        tracing::info!(
                            target: "hydra::db::audit",
                            deleted,
                            "admin_audit retention pass deleted expired rows"
                        );
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        target: "hydra::db::audit",
                        %error,
                        "admin_audit retention pass failed; will retry next interval"
                    );
                }
            }
        }
    });
}
