//! The **`none`** backend: usage is switched off on this node (ADR-0002 D-2).
//!
//! It exists because the two alternatives are both dishonest. Retiring SQLite takes away the
//! zero-dependency single-node store, and "usage must go somewhere" would then force a ClickHouse
//! container onto every lab, laptop and air-gapped deployment — while silently picking a default
//! backend for an operator who never chose one is worse. So "no usage store" is a first-class,
//! **explicit** choice, and it is loud:
//!
//! * `HYDRA_USAGE_SINK=none` must be typed out — it is never the default (ADR-0002 D-1);
//! * startup logs a WARNING carrying [`WHY`] (the same text `GET /usage` answers 503 for);
//! * every record it is handed is counted on `hydra_usage_records_dropped_total{reason="sink_disabled"}`
//!   and logged with its trace id, so "we are not metering anything" shows up in the same series an
//!   operator already alerts on for real drops;
//! * it declares [`ReaderContract::Unavailable`], so the tenant API answers the documented
//!   `503 usage_store_unavailable` rather than a perfectly-formed zero.
//!
//! What it is NOT: a backend that can be selected by accident, or a place usage is stored. There is
//! no store at all — which is exactly what the operator asked for, and what the runbook must say.

use std::future::Future;
use std::pin::Pin;

use hydra_core::model::UsageRecord;

use super::super::{Backend, BackendConfig, BackendError, ReaderContract, UsageBackend};

/// What `GET /usage` answers 503 for, and what startup warns with. One string, so the log line and
/// the tenant-facing answer cannot describe the state differently.
pub(crate) const WHY: &str =
    "usage is disabled on this node (HYDRA_USAGE_SINK=none): nothing is recorded, so GET /usage \
     answers 503. Set HYDRA_USAGE_SINK=clickhouse (+ HYDRA_CLICKHOUSE_URL) to meter";

pub static DESCRIPTOR: UsageBackend = UsageBackend {
    kind: "none",
    // Compiled wherever the usage module is (`db`): the option to switch usage off must exist in
    // every build, or a deployment could not choose it.
    feature: "db",
    requires: &[],
    recognises: &[],
    reads: ReaderContract::Unavailable { why: WHY },
    open,
    notes: "usage is DISABLED: nothing is recorded, every record is counted as sink_disabled",
};

fn open(_cfg: &BackendConfig) -> Result<Backend, BackendError> {
    Ok(Backend {
        sink: std::sync::Arc::new(NoUsageSink),
        query: None,
        notes: "",
        reads: ReaderContract::Unavailable { why: WHY },
    })
}

/// Drops every record, loudly.
///
/// It deliberately does NOT use the batching engine: there is nothing to batch, and a channel plus a
/// background task would only delay the moment the record is counted as lost.
struct NoUsageSink;

impl crate::usage::engine::UsageSink for NoUsageSink {
    fn record(&self, record: UsageRecord) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            // Counted with the SAME series as a real drop: an operator alerting on lost usage must
            // see "this node meters nothing" without a second rule. The trace id is kept, so a
            // request can still be traced to the record that was never written.
            crate::usage::engine::note_usage_drop("sink_disabled", 1);
            tracing::debug!(
                target: "hydra::usage",
                dropped_trace_id = %record.trace_id,
                "HYDRA_USAGE_SINK=none: usage record discarded (this node records nothing)"
            );
        })
    }
}
