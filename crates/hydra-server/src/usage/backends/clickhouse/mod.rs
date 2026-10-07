//! The **ClickHouse** usage backend: the cluster's shared usage store (ADR-0002's only delivered
//! backend).
//!
//! The writer and the reader are the same store, so both halves open together: the write path posts
//! one JSON object per row to `/?query=INSERT … FORMAT JSONEachRow` with a per-batch
//! `insert_deduplication_token`, and `GET /usage` reads the same table back with bound parameters.
//! The transport, the URL parsing and the TLS policy live in [`crate::clickhouse`] — one owner for
//! "what a ClickHouse URL means".

use super::super::{
    Backend, BackendConfig, BackendError, ReaderContract, Requirement, UsageBackend,
};

/// The endpoint every node writes usage to.
///
/// A constant so the descriptor's `requires`, the read that uses it and the guards that check the
/// documentation all name the same string.
pub(crate) const CLICKHOUSE_URL_ENV: &str = "HYDRA_CLICKHOUSE_URL";

const URL_PURPOSE: &str =
    "the ClickHouse HTTP endpoint that every node writes usage to (it is the cluster's shared \
     usage store, and the store GET /usage reads back)";

pub static DESCRIPTOR: UsageBackend = UsageBackend {
    kind: "clickhouse",
    feature: "usage-clickhouse",
    requires: &[Requirement {
        name: CLICKHOUSE_URL_ENV,
        purpose: URL_PURPOSE,
    }],
    // Recognised with defaults. Listed so an operator finds every knob in one place (`ops.md`), and
    // so the guard can insist that each of them is documented somewhere.
    recognises: &[
        "HYDRA_CLICKHOUSE_CONNECT_TIMEOUT_MS",
        "HYDRA_CLICKHOUSE_IO_TIMEOUT_MS",
        "HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS",
    ],
    reads: ReaderContract::SameBackend,
    open,
    notes: "HTTP INSERT of one JSON row per request (masked key, per-batch dedup token); the same \
            table answers GET /usage",
};

fn open(cfg: &BackendConfig) -> Result<Backend, BackendError> {
    #[cfg(feature = "usage-clickhouse")]
    {
        use std::sync::Arc;

        let url = cfg
            .env
            .get(CLICKHOUSE_URL_ENV)
            .ok_or(BackendError::MissingEnv {
                kind: DESCRIPTOR.kind,
                name: CLICKHOUSE_URL_ENV,
                purpose: URL_PURPOSE,
            })?;
        // Refused at STARTUP, not on the first flush: the alternative is a node that boots, serves
        // traffic and only then fails every usage flush. The credential never leaves either way, so
        // failing early is strictly better than failing late (the transport refuses `https://` too —
        // it has no TLS path, and silently connecting in the clear while the URL promised TLS would
        // leak the credentials).
        if url.trim().starts_with("https://") {
            return Err(BackendError::Invalid {
                kind: DESCRIPTOR.kind,
                message: "HYDRA_CLICKHOUSE_URL uses https:// but this build has no TLS transport \
                          for ClickHouse; refusing to start rather than send credentials and usage \
                          rows in PLAINTEXT — use http:// on a private network or through a tunnel, \
                          or terminate TLS in front of the database"
                    .to_string(),
            });
        }
        Ok(Backend {
            sink: Arc::new(crate::sink::ClickHouseSink::new(
                &url,
                crate::sink::DEFAULT_BATCH_SIZE,
                crate::sink::DEFAULT_FLUSH_SECS,
            )),
            query: Some(Arc::new(crate::usage_query::ClickHouseUsageQuery::new(
                &url,
            ))),
            notes: "",
        })
    }
    #[cfg(not(feature = "usage-clickhouse"))]
    {
        // The kind is KNOWN (so the error can say which feature to build with) even though this
        // binary cannot serve it — `usage-clickhouse` is not implied by `server`.
        let _ = &cfg.env;
        Err(BackendError::FeatureDisabled {
            kind: DESCRIPTOR.kind,
            feature: DESCRIPTOR.feature,
        })
    }
}
