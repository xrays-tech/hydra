//! Usage read capability for the tenant API (`GET /usage`).
//!
//! ## Why this is a separate trait instead of a method on `UsageSink`
//!
//! [`UsageSink`](crate::sink::UsageSink) is a **fire-and-forget write channel**:
//! `record` buffers and returns, `shutdown` drains, and `build_sink` erases the
//! concrete type into `Box<dyn UsageSink>`. Giving it a query method would
//! change what that trait means and force every implementation to grow a read
//! path it does not have. So the reader is its own capability, injected at
//! startup by sink kind:
//!
//! ```text
//! HYDRA_USAGE_SINK=sqlite      -> SqliteUsageQuery      (the local pool)
//! HYDRA_USAGE_SINK=clickhouse  -> ClickHouseUsageQuery  (T8; the shared store)
//! ```
//!
//! ## Why the caller must not just probe for a pool
//!
//! In a cluster the leader **also** has a local SQLite file (`main.rs` gives a
//! pool to every role except edge) while the sink is ClickHouse — so that file
//! contains no usage rows at all. A reader that decided "is there a pool?" would
//! answer a perfectly-formed `{"requests":0}` from an empty table. That is the
//! failure this shape prevents: the capability is chosen once, from the
//! configured sink kind, and [`select`] is the single place that choice is made.
//!
//! ## Time bounds
//!
//! `since`/`until` are the canonical `%Y-%m-%dT%H:%M:%SZ` strings produced by
//! [`crate::tenant_api::time_bound`]. They are compared as **strings** against
//! the stored column, which is valid because the form is fixed-width, zero-padded
//! and all-UTC — and because the column is written from the same formatter. No
//! function is ever applied to the column, which is what keeps the index usable.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use sqlx::SqlitePool;

use hydra_core::tenant_api::{normalize_as_of, UsageAggregate, UsageRow, UsageTotals};

/// How to group the rows of an aggregate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupBy {
    /// No grouping: only the totals row.
    None,
    Model,
    Provider,
    /// Calendar day, derived from the fixed-width prefix of `created_at`.
    Day,
}

impl GroupBy {
    /// Parse the `group_by` query parameter. Unknown values are rejected by the
    /// caller (a whitelist, never an interpolation).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" | "" => Some(Self::None),
            "model" => Some(Self::Model),
            "provider" => Some(Self::Provider),
            "day" => Some(Self::Day),
            _ => None,
        }
    }

    /// The response label for this grouping.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Model => "model",
            Self::Provider => "provider",
            Self::Day => "day",
        }
    }
}

/// Why a usage query could not be answered.
#[derive(Debug)]
pub enum UsageQueryError {
    /// The store could not be reached or refused the query. Reported to the
    /// tenant as "usage is unavailable right now", never as zero usage.
    StoreUnavailable(String),
    /// The store answered with something we could not decode. Also never zero.
    Decode(String),
}

impl std::fmt::Display for UsageQueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StoreUnavailable(e) => write!(f, "usage store unavailable: {e}"),
            Self::Decode(e) => write!(f, "usage response not decodable: {e}"),
        }
    }
}

/// Why a sink kind could not be turned into a reader.
#[derive(Debug, PartialEq, Eq)]
pub enum SelectError {
    /// The kind is not `sqlite` or `clickhouse`.
    UnknownKind(String),
    /// `sqlite` without a pool.
    MissingPool,
}

/// Read-only aggregate access to the metering store.
///
/// Hand-desugared futures, matching the sibling [`UsageSink`](crate::sink::UsageSink)
/// (and `Limiter`) rather than `#[async_trait]`: one async-trait style per area.
pub trait UsageQuery: Send + Sync {
    fn aggregate<'a>(
        &'a self,
        tenant_id: &'a str,
        since: &'a str,
        until: &'a str,
        group_by: GroupBy,
    ) -> Pin<Box<dyn Future<Output = Result<UsageAggregate, UsageQueryError>> + Send + 'a>>;

    /// Which store answered. Reported to the tenant, and asserted by the
    /// startup-wiring test: the value comes from the implementation, so it
    /// cannot disagree with the code that actually ran.
    fn source(&self) -> &'static str;
}

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

/// Choose the reader for a configured sink kind.
///
/// This is a **library function, not a `main.rs` branch**, so the choice that
/// guards against the "report zero usage" failure is directly testable. The
/// ClickHouse arm arrives in T8 together with its implementation and the
/// `usage-clickhouse` feature gating.
pub fn select_sqlite(
    sink_kind: &str,
    pool: Option<&SqlitePool>,
) -> Result<std::sync::Arc<dyn UsageQuery>, SelectError> {
    match sink_kind {
        "sqlite" => {
            let pool = pool.ok_or(SelectError::MissingPool)?;
            Ok(std::sync::Arc::new(SqliteUsageQuery::new(pool.clone())))
        }
        other => Err(SelectError::UnknownKind(other.to_string())),
    }
}

/// Grouping is a whitelist; this keeps the response's own label consistent with
/// the rows it contains.
#[must_use]
pub fn group_by_label(g: GroupBy) -> &'static str {
    g.label()
}

/// A deterministic, order-independent view of the rows, used by tests that need
/// to compare two aggregates without depending on SQL ordering.
#[must_use]
pub fn rows_by_key(agg: &UsageAggregate) -> BTreeMap<String, UsageTotals> {
    agg.rows.iter().map(|r| (r.key.clone(), r.totals)).collect()
}
