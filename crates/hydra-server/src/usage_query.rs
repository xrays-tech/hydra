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
    /// The store answered, but the answer was larger than the gateway's
    /// response-size cap, so it was cut off mid-stream. Unlike the other two,
    /// retrying returns the SAME truncated bytes: the caller must narrow
    /// `since`/`until` or reduce `group_by` instead. The `usize` is the cap in
    /// bytes, reported so the tenant can see the limit it ran into.
    ResultTooLarge(usize),
}

impl std::fmt::Display for UsageQueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StoreUnavailable(e) => write!(f, "usage store unavailable: {e}"),
            Self::Decode(e) => write!(f, "usage response not decodable: {e}"),
            Self::ResultTooLarge(cap) => {
                write!(
                    f,
                    "usage result exceeded the {cap}-byte gateway response cap"
                )
            }
        }
    }
}

/// Why a sink kind could not be turned into a reader.
#[derive(Debug, PartialEq, Eq)]
pub enum SelectError {
    /// The kind is not `sqlite` or `clickhouse` — or it IS `clickhouse` in a
    /// build without the `usage-clickhouse` feature, where the kind cannot occur
    /// at all because `sink::build_sink` refuses it at startup. Reporting the
    /// kind as unknown keeps the two guards from drifting apart.
    UnknownKind(String),
    /// `sqlite` without a pool.
    MissingPool,
    /// `clickhouse` without a URL.
    MissingUrl,
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

// ---------------------------------------------------------------------------
// ClickHouse (cluster deployments: the shared external store)
// ---------------------------------------------------------------------------

/// The ClickHouse-backed reader.
///
/// ## Two queries, not one
///
/// The SQLite arm issues the totals query and (when grouping) the group query
/// separately, and this arm does the same. A single query with a `UNION ALL` of
/// the two would be fewer round-trips, but it **cannot be decoded
/// positionally**: measured on the bundled ClickHouse 24.3, the same statement
/// returned the totals row FIRST in 1 of 6 runs, and adding `ORDER BY is_total`
/// did not stabilise it (a bare `ORDER BY` after a `UNION ALL` binds to the last
/// branch only). A positional decoder would then read a *group* row as the
/// overall totals — one model's numbers reported as the tenant's whole usage.
/// Two queries have no ordering assumption to get wrong.
///
/// ## Binding, never interpolation
///
/// `tenant_id` and the bounds travel as `{name:String}` placeholders bound
/// through `param_*`, and both the SQL and every bound value are percent-encoded
/// by [`crate::clickhouse::send`]. Measured: a bound value of `x' OR 1=1 --`
/// arrives as a literal string, so a tenant id — attacker-influenced text — can
/// never become SQL. `group_by` is a **whitelist** mapped to a column name, so
/// it is the one fragment that is interpolated, and only ever with a constant.
#[cfg(feature = "usage-clickhouse")]
pub struct ClickHouseUsageQuery {
    cfg: crate::clickhouse::ClickHouseConfig,
}

/// Totals row. `MAX(created_at)` is `""` (not NULL) for an empty set on
/// ClickHouse, which `normalize_as_of` maps to `None`.
#[cfg(feature = "usage-clickhouse")]
const CH_TOTALS_SELECT: &str = "\
SELECT count()                                     AS requests, \
       COALESCE(sum(tokens_in), 0)                 AS tokens_in, \
       COALESCE(sum(tokens_out), 0)                AS tokens_out, \
       COALESCE(sum(cache_hit_tokens), 0)          AS cache_hit_tokens, \
       COALESCE(sum(if(status_code >= 400, 1, 0)), 0) AS errors, \
       MAX(created_at)                             AS last_seen \
FROM usage_record \
WHERE tenant_id = {t:String} AND created_at >= {s:String} AND created_at < {e:String} \
FORMAT JSONEachRow";

/// `group_by` → the ClickHouse expression that keys a row. A whitelist: the
/// string is interpolated into the query, so it must never come from input.
#[cfg(feature = "usage-clickhouse")]
fn ch_group_expr(g: GroupBy) -> Option<&'static str> {
    match g {
        GroupBy::None => None,
        GroupBy::Model => Some("model_key"),
        GroupBy::Provider => Some("provider_id"),
        // `created_at` is a fixed-width `YYYY-MM-DDTHH:MM:SSZ` string on both
        // backends, so the date is a safe 10-byte slice. No date function: it
        // would both change semantics and drop the primary-key pruning.
        GroupBy::Day => Some("substr(created_at, 1, 10)"),
    }
}

#[cfg(feature = "usage-clickhouse")]
impl ClickHouseUsageQuery {
    /// Built from the configured URL. Parsing stays in
    /// [`crate::clickhouse`], the one owner of what a ClickHouse URL means.
    fn new(url: &str) -> Self {
        let mut cfg = crate::clickhouse::parse_clickhouse_url(url);
        // The reader has its own deadline (`HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS`),
        // independent of the writer's: a tenant waiting on a slow query must not
        // inherit the flush task's much longer allowance, and vice versa.
        cfg.io_timeout = crate::clickhouse::env_millis("HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS", 5_000);
        Self { cfg }
    }

    /// Run one statement and return its response body.
    ///
    /// A non-2xx is a *failure*, not a body to decode: ClickHouse answers a
    /// failed query with HTTP 404 and a `Code: N. DB::Exception: …` text
    /// (measured), so the status and the body are both kept for the operator.
    async fn run(&self, sql: &str, params: &[(&str, &str)]) -> Result<String, UsageQueryError> {
        let result = crate::clickhouse::send(&self.cfg, sql, params, b"")
            .await
            .map_err(UsageQueryError::StoreUnavailable)?;
        if !crate::clickhouse::is_ok_status(&result.status_line) {
            return Err(UsageQueryError::StoreUnavailable(format!(
                "clickhouse said {:?}: {}",
                result.status_line.trim(),
                crate::clickhouse::response_body(&result.body)
            )));
        }
        // A body that filled the transport's cap was cut off mid-stream: it is a
        // TOO-BIG answer, not a malformed one, and the two must not be conflated.
        // Retrying a too-big answer returns the same truncated bytes, so this is
        // the one 503 the caller fixes by narrowing the window or shrinking the
        // grouping — never by calling again.
        if result.truncated {
            return Err(UsageQueryError::ResultTooLarge(
                crate::clickhouse::MAX_CLICKHOUSE_RESPONSE,
            ));
        }
        Ok(crate::clickhouse::response_body(&result.body))
    }
}

#[cfg(feature = "usage-clickhouse")]
impl UsageQuery for ClickHouseUsageQuery {
    fn aggregate<'a>(
        &'a self,
        tenant_id: &'a str,
        since: &'a str,
        until: &'a str,
        group_by: GroupBy,
    ) -> Pin<Box<dyn Future<Output = Result<UsageAggregate, UsageQueryError>> + Send + 'a>> {
        Box::pin(async move {
            let params: [(&str, &str); 3] = [("t", tenant_id), ("s", since), ("e", until)];
            let totals_body = self.run(CH_TOTALS_SELECT, &params).await?;
            // The totals query always projects `last_seen`, so a decode failure
            // here is a shape drift or a non-numeric counter — either way it must
            // surface as a failure, never as a zeroed `totals`.
            let totals = hydra_core::tenant_api::decode_usage_json_each_row(&totals_body)
                .map_err(|e| UsageQueryError::Decode(format!("totals: {e:?}")))?;

            let mut rows = Vec::new();
            if let Some(expr) = ch_group_expr(group_by) {
                let sql = format!(
                    "SELECT {expr} AS key, count() AS requests, \
                            COALESCE(sum(tokens_in), 0) AS tokens_in, \
                            COALESCE(sum(tokens_out), 0) AS tokens_out, \
                            COALESCE(sum(cache_hit_tokens), 0) AS cache_hit_tokens, \
                            COALESCE(sum(if(status_code >= 400, 1, 0)), 0) AS errors \
                     FROM usage_record \
                     WHERE tenant_id = {{t:String}} AND created_at >= {{s:String}} \
                       AND created_at < {{e:String}} \
                     GROUP BY key ORDER BY key FORMAT JSONEachRow"
                );
                let body = self.run(&sql, &params).await?;
                rows = hydra_core::tenant_api::decode_usage_rows_json_each_row(&body)
                    .map_err(|e| UsageQueryError::Decode(format!("rows: {e:?}")))?;
            }

            Ok(UsageAggregate {
                totals: totals.totals,
                rows,
                as_of: totals.as_of,
            })
        })
    }

    fn source(&self) -> &'static str {
        "clickhouse"
    }
}

/// Choose the reader for a configured sink kind.
///
/// This is a **library function, not a `main.rs` branch**, so the choice that
/// guards against the "report zero usage" failure is directly testable — the
/// injection point lives in the binary and no integration test can reach it.
///
/// The signature is the same with and without `usage-clickhouse`; only the arm
/// is gated. Passing the ClickHouse **URL** rather than a parsed config keeps
/// this signature free of a `usage-clickhouse`-gated type (and of the
/// `pub(crate)` visibility of that type), and leaves `clickhouse` the single
/// owner of how a URL becomes a transport: this function never parses one.
// Without `usage-clickhouse` there is no arm that reads the URL, but the
// parameter stays in the signature: one call shape for `main` and for the tests,
// in both feature combinations.
#[cfg_attr(not(feature = "usage-clickhouse"), allow(unused_variables))]
pub fn select(
    sink_kind: &str,
    pool: Option<&SqlitePool>,
    ch_url: Option<&str>,
) -> Result<std::sync::Arc<dyn UsageQuery>, SelectError> {
    match sink_kind {
        "sqlite" => {
            let pool = pool.ok_or(SelectError::MissingPool)?;
            Ok(std::sync::Arc::new(SqliteUsageQuery::new(pool.clone())))
        }
        #[cfg(feature = "usage-clickhouse")]
        "clickhouse" => {
            let url = ch_url.ok_or(SelectError::MissingUrl)?;
            Ok(std::sync::Arc::new(ClickHouseUsageQuery::new(url)))
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
