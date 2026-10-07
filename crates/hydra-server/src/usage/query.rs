//! The usage **read contract** (`GET /usage`), storage-neutral.
//!
//! ## Why this is a separate trait from `UsageSink`
//!
//! [`UsageSink`](crate::usage::engine::UsageSink) is a **fire-and-forget write channel**: `record`
//! buffers and returns, `shutdown` drains. Giving it a query method would change what that trait
//! means and force every implementation to grow a read path it does not have. The reader is its own
//! capability, and it comes from the SAME registry descriptor as the writer (ADR-0002): a backend
//! either reads back what it writes, or it declares why it cannot.
//!
//! ## Why the caller must not just probe for a pool
//!
//! A node in a cluster **also** has a local SQLite file while its sink is ClickHouse — so that file
//! contains no usage rows at all. A reader that decided "is there a pool?" would answer a
//! perfectly-formed `{"requests":0}` from an empty table. The capability is chosen once, from the
//! configured backend, and `usage::open` is where that choice is made.
//!
//! ## Time bounds
//!
//! `since`/`until` are the canonical `%Y-%m-%dT%H:%M:%SZ` strings produced by
//! [`crate::tenant_api::time_bound`]. They are compared as **strings** against the stored column,
//! which is valid because the form is fixed-width, zero-padded and all-UTC — and because the column
//! is written from the same formatter. No function is ever applied to the column, which is what
//! keeps the index usable.
//!
//! ## What a backend must satisfy (README for implementers)
//!
//! - a window that cannot be read is an **`Err`**, never a zero: `{"requests":0}` and "you used
//!   nothing" must not be the same answer;
//! - integer counters may arrive as **strings** (ClickHouse 24.3 does this) and a non-numeric value
//!   is a failure, not a zero;
//! - an empty grouping key is `""`, never NULL;
//! - `tenant_id` comes from the authenticated session and never from the URL.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use hydra_core::tenant_api::{UsageAggregate, UsageTotals};

/// How to group the rows of an aggregate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupBy {
    /// No grouping: only the totals row.
    None,
    Model,
    Provider,
    /// The sub-tenant the request was attributed to (v3). Rows whose key matched
    /// no enabled sub-tenant prefix group under the empty key.
    SubTenant,
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
            "sub_tenant" => Some(Self::SubTenant),
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
            Self::SubTenant => "sub_tenant",
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

// `SelectError` stood here until 2026-10-07: "unknown kind" and "no URL" were the read side's own
// error vocabulary, separate from the write side's. Both are now one type
// (`crate::usage::BackendError`) produced by one place (`crate::usage::open`), which is what makes
// "the writer was registered but the reader was forgotten" unrepresentable (ADR-0002).

/// Read-only aggregate access to the metering store.
///
/// Hand-desugared futures, matching the sibling [`UsageSink`](crate::usage::engine::UsageSink)
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
