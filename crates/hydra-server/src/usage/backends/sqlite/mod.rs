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

fn open(cfg: &BackendConfig) -> Result<Backend, BackendError> {
    let sink = crate::sink::SqliteSink::new(
        cfg.pool.clone(),
        crate::sink::DEFAULT_BATCH_SIZE,
        crate::sink::DEFAULT_FLUSH_SECS,
    );
    let query = crate::usage_query::SqliteUsageQuery::new(cfg.pool.clone());
    Ok(Backend {
        sink: Arc::new(sink),
        query: Some(Arc::new(query)),
        // `open` overwrites this from the descriptor, so the note has ONE owner.
        notes: "",
    })
}
