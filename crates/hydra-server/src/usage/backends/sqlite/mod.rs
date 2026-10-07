//! The **SQLite** usage backend: rows in the node's own database.
//!
//! **Retired by ADR-0002** (user ruling D-3: the table is dropped), and still registered here
//! because Phase 1 of that ADR is a pure refactor: this row keeps the behaviour identical while the
//! selection path moves into the registry. Phase 2 deletes this module, `SqliteSink`,
//! `SqliteUsageQuery` and `DEFAULT_USAGE_SINK`; `sqlite` then becomes a refused value.
//!
//! Why it goes: a per-node usage table is the wrong answer in a cluster (the node that answers
//! `GET /usage` is not necessarily the node that recorded the request, and the leader's own table is
//! empty because the writer went to ClickHouse), and the repository's own delivered stacks have
//! hard-coded `HYDRA_USAGE_SINK=clickhouse` in all three compose files for exactly that reason.

use std::sync::Arc;

use super::super::{Backend, BackendConfig, BackendError, ReaderContract, UsageBackend};

pub static DESCRIPTOR: UsageBackend = UsageBackend {
    kind: "sqlite",
    feature: "db",
    // It uses the node's own database (`HYDRA_DB_URL`, which has a default), so it needs nothing.
    requires: &[],
    recognises: &[],
    reads: ReaderContract::SameBackend,
    open,
    notes: "usage rows go into this node's own SQLite database; correct for exactly one node — \
            retired by ADR-0002 (deleted in Phase 2)",
};

// ---------------------------------------------------------------------------
// The reader (moved verbatim from `usage_query.rs`, ADR-0002 T1.3)
// ---------------------------------------------------------------------------

use std::future::Future;
use std::pin::Pin;

use hydra_core::model::UsageRecord;
use hydra_core::tenant_api::{normalize_as_of, UsageAggregate, UsageRow, UsageTotals};

use crate::usage::engine::{
    drain_on_drop, note_usage_drop, run_channel_sink, UsageSink, MAX_FLUSH_RETRY_WINDOW,
    MAX_SHUTDOWN_WAIT,
};

use crate::usage::query::{GroupBy, UsageQuery, UsageQueryError};

/// The SQLite-backed reader (single-node deployments).
pub struct SqliteUsageQuery {
    pool: SqlitePool,
}

impl SqliteUsageQuery {
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

/// The five counter columns, and how `errors` counts non-2xx rows.
///
/// `COALESCE` is not decoration: the token columns are nullable (a provider that
/// reports nothing is stored as NULL, deliberately, so it cannot masquerade as a
/// zero count), so a window of NULL-token rows must sum to 0 rather than NULL.
const TOTALS_SELECT: &str = "\
SELECT COUNT(*)                                AS requests, \
       COALESCE(SUM(tokens_in), 0)             AS tokens_in, \
       COALESCE(SUM(tokens_out), 0)            AS tokens_out, \
       COALESCE(SUM(cache_hit_tokens), 0)      AS cache_hit_tokens, \
       COALESCE(SUM(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END), 0) AS errors, \
       MAX(created_at)                         AS last_seen \
FROM usage_record \
WHERE tenant_id = ? AND created_at >= ? AND created_at < ?";

/// `group_by` → the SQL expression that keys a row. A whitelist: this string is
/// interpolated into the query, so it must never come from user input directly.
fn group_expr(g: GroupBy) -> Option<&'static str> {
    match g {
        GroupBy::None => None,
        GroupBy::Model => Some("model_key"),
        GroupBy::Provider => Some("provider_id"),
        // COALESCE so an unattributed (NULL) row groups under the explicit ""
        // key — identical on both backends, independent of the sqlx NULL->""
        // quirk. Not cosmetic: the decoder requires a string `key`, so a NULL
        // here fails the entire read rather than one group of it.
        GroupBy::SubTenant => Some("COALESCE(sub_tenant_id, '')"),
        // `created_at` is fixed-width `YYYY-MM-DDTHH:MM:SSZ`, so the date is a
        // safe 10-byte prefix and needs no date function (which would also make
        // the index unusable).
        GroupBy::Day => Some("substr(created_at, 1, 10)"),
    }
}

impl UsageQuery for SqliteUsageQuery {
    fn aggregate<'a>(
        &'a self,
        tenant_id: &'a str,
        since: &'a str,
        until: &'a str,
        group_by: GroupBy,
    ) -> Pin<Box<dyn Future<Output = Result<UsageAggregate, UsageQueryError>> + Send + 'a>> {
        Box::pin(async move {
            let totals =
                sqlx::query_as::<_, (i64, i64, i64, i64, i64, Option<String>)>(TOTALS_SELECT)
                    .bind(tenant_id)
                    .bind(since)
                    .bind(until)
                    .fetch_one(&self.pool)
                    .await
                    .map_err(|e| UsageQueryError::StoreUnavailable(e.to_string()))?;

            let mut rows = Vec::new();
            if let Some(expr) = group_expr(group_by) {
                let sql = format!(
                    "SELECT {expr} AS grp, COUNT(*) AS requests, \
                            COALESCE(SUM(tokens_in), 0) AS tokens_in, \
                            COALESCE(SUM(tokens_out), 0) AS tokens_out, \
                            COALESCE(SUM(cache_hit_tokens), 0) AS cache_hit_tokens, \
                            COALESCE(SUM(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END), 0) AS errors \
                     FROM usage_record \
                     WHERE tenant_id = ? AND created_at >= ? AND created_at < ? \
                     GROUP BY grp ORDER BY grp"
                );
                let grouped = sqlx::query_as::<_, (String, i64, i64, i64, i64, i64)>(&sql)
                    .bind(tenant_id)
                    .bind(since)
                    .bind(until)
                    .fetch_all(&self.pool)
                    .await
                    .map_err(|e| UsageQueryError::StoreUnavailable(e.to_string()))?;
                rows = grouped
                    .into_iter()
                    .map(
                        |(key, requests, tokens_in, tokens_out, cache_hit, errors)| UsageRow {
                            key,
                            totals: UsageTotals {
                                requests: requests.max(0) as u64,
                                tokens_in: tokens_in.max(0) as u64,
                                tokens_out: tokens_out.max(0) as u64,
                                cache_hit_tokens: cache_hit.max(0) as u64,
                                errors: errors.max(0) as u64,
                            },
                        },
                    )
                    .collect();
            }

            Ok(UsageAggregate {
                totals: UsageTotals {
                    requests: totals.0.max(0) as u64,
                    tokens_in: totals.1.max(0) as u64,
                    tokens_out: totals.2.max(0) as u64,
                    cache_hit_tokens: totals.3.max(0) as u64,
                    errors: totals.4.max(0) as u64,
                },
                rows,
                // SQLite yields NULL for an empty window; `normalize_as_of` is the
                // shared rule that also maps ClickHouse's `""` (T8) to None.
                as_of: normalize_as_of(totals.5.as_deref()),
            })
        })
    }

    fn source(&self) -> &'static str {
        "sqlite"
    }
}

// ===========================================================================
// SqliteSink (design §9.2) — feature `db`
// ===========================================================================

// ---------------------------------------------------------------------------
// The writer (moved verbatim from `sink.rs`, ADR-0002 T1.3)
// ---------------------------------------------------------------------------

// ===========================================================================
// SqliteSink (design §9.2) — feature `db`
// ===========================================================================

#[cfg(feature = "db")]
use sqlx::SqlitePool;

#[cfg(any(feature = "db", feature = "usage-clickhouse"))]
use tokio::sync::mpsc;

/// Default `UsageSink` — batched INSERTs into the W2 `usage_record` table.
///
/// `record()` pushes onto a bounded mpsc channel (non-blocking; drops + logs on
/// overflow). A background task flushes on `batch_size` reached OR every
/// `flush_secs`, retrying transient DB errors with exponential backoff. On
/// `Drop` the channel closes and the sink best-effort synchronously drains +
/// final-flushes (requires a multi-threaded tokio runtime; on a current-thread
/// runtime it detaches, still completing the flush asynchronously).
#[cfg(feature = "db")]
pub struct SqliteSink {
    /// `None` once the sink has been shut down (explicitly or via `Drop`).
    tx: std::sync::Mutex<Option<mpsc::Sender<UsageRecord>>>,
    join: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[cfg(feature = "db")]
impl SqliteSink {
    /// Spawn the sink: creates a bounded channel and a background flush task on
    /// the current tokio runtime. Panics (via tokio) if called outside a
    /// runtime — bring the runtime up first (design: sink is constructed during
    /// server startup, after the runtime exists).
    #[must_use]
    pub fn new(pool: SqlitePool, batch_size: usize, flush_secs: u64) -> Self {
        let batch_size = batch_size.max(1);
        let capacity = batch_size.max(16);
        let (tx, rx) = mpsc::channel(capacity);

        let pool_for_task = pool.clone();
        let inserter = move |_batch_id: &str, batch: Vec<UsageRecord>| {
            let pool = pool_for_task.clone();
            async move {
                match insert_batch_sqlite(&pool, &batch).await {
                    Ok(()) => Ok(()),
                    Err(e) => Err((batch, e.to_string())),
                }
            }
        };

        let join = tokio::spawn(run_channel_sink(
            rx,
            batch_size,
            flush_secs,
            MAX_FLUSH_RETRY_WINDOW,
            inserter,
        ));
        Self {
            tx: std::sync::Mutex::new(Some(tx)),
            join: std::sync::Mutex::new(Some(join)),
        }
    }
}

#[cfg(feature = "db")]
impl UsageSink for SqliteSink {
    fn record(&self, record: UsageRecord) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let tx = {
                let guard = self
                    .tx
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match guard.as_ref() {
                    Some(tx) => tx.clone(),
                    None => {
                        // The sink is already shut down: this record cannot be
                        // delivered. Count it instead of dropping it silently.
                        note_usage_drop("channel_closed", 1);
                        return;
                    }
                }
            };
            if let Err(err) = tx.try_send(record) {
                // Non-blocking: channel either full (backpressure) or closed
                // (shutting down). Either way never block the caller.
                let dropped_trace = match &err {
                    mpsc::error::TrySendError::Full(r) | mpsc::error::TrySendError::Closed(r) => {
                        r.trace_id.clone()
                    }
                };
                let (reason, dropped) = match &err {
                    mpsc::error::TrySendError::Full(_) => ("channel_full", 1u64),
                    mpsc::error::TrySendError::Closed(_) => ("channel_closed", 1u64),
                };
                note_usage_drop(reason, dropped);
                tracing::warn!(
                    dropped_trace_id = %dropped_trace,
                    error = %err,
                    reason,
                    "usage sink channel full/closed; dropping usage record"
                );
            }
        })
    }

    fn shutdown(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let tx = self
                .tx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            let join = self
                .join
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            drop(tx); // the bg task observes closure and does its final drain
            if let Some(join) = join {
                let _ = tokio::time::timeout(MAX_SHUTDOWN_WAIT, join).await;
            }
        })
    }
}

#[cfg(feature = "db")]
impl Drop for SqliteSink {
    fn drop(&mut self) {
        // Take both out BEFORE the blocking wait: the guards must not be held
        // across drain_on_drop's block_in_place/block_on.
        let tx = self
            .tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let join = self
            .join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drain_on_drop(tx, join);
    }
}

/// Insert a batch inside a single transaction (atomicity + speed). The core
/// struct's `client_api_key_masked` maps to the `client_api_key` column; the
/// `trace_id` field has no corresponding column in the W2 schema and is
/// intentionally not persisted here (it lives in logs/metrics, design §4.1).
///
/// Uses runtime-checked `query` (see `db.rs` header: in-memory pools can't be
/// introspected at compile time).
#[cfg(feature = "db")]
async fn insert_batch_sqlite(
    pool: &SqlitePool,
    records: &[UsageRecord],
) -> Result<(), sqlx::Error> {
    if records.is_empty() {
        return Ok(());
    }
    let mut tx = crate::db::begin_write(pool).await?;
    for r in records {
        sqlx::query(
            "INSERT INTO usage_record \
             (tenant_id, provider_id, model_key, client_api_key, sub_tenant_id, status_code, \
              tokens_in, tokens_out, cache_hit_tokens, \
              latency_ms, forward_latency_ms, ttft_ms, \
              upstream_host, error, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&r.tenant_id)
        .bind(&r.provider_id)
        .bind(&r.model_key)
        .bind(&r.client_api_key_masked)
        .bind(&r.sub_tenant_id)
        .bind(i64::from(r.status_code))
        .bind(r.tokens_in.map(|v| v as i64))
        .bind(r.tokens_out.map(|v| v as i64))
        .bind(r.cache_hit_tokens.map(|v| v as i64))
        .bind(r.latency_ms as i64)
        .bind(r.forward_latency_ms.map(|v| v as i64))
        .bind(r.ttft_ms.map(|v| v as i64))
        .bind(&r.upstream_host)
        .bind(&r.error)
        .bind(&r.created_at)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

fn open(cfg: &BackendConfig) -> Result<Backend, BackendError> {
    let sink = crate::usage::backends::sqlite::SqliteSink::new(
        cfg.pool.clone(),
        crate::usage::engine::DEFAULT_BATCH_SIZE,
        crate::usage::engine::DEFAULT_FLUSH_SECS,
    );
    let query = SqliteUsageQuery::new(cfg.pool.clone());
    Ok(Backend {
        sink: Arc::new(sink),
        query: Some(Arc::new(query)),
        // `open` overwrites this from the descriptor, so the note has ONE owner.
        notes: "",
        reads: ReaderContract::SameBackend,
    })
}
