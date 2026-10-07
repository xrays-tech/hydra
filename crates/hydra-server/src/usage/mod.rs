//! The **ONE owner** of "which usage backends exist" (ADR-0002).
//!
//! Before this module, the answer lived in two parallel `match` statements — `sink::build_sink` for
//! the write path and `usage_query::select` for the read path — and the only thing keeping them
//! consistent was a comment saying they had to be kept consistent. Nothing guaranteed that a kind
//! registered on one side was registered on the other, and adding a backend meant editing four
//! files.
//!
//! Here a backend is **one row**: a [`UsageBackend`] descriptor that declares what the kind is
//! called, which cargo feature compiles it, which environment variables it needs, how to open it,
//! and whether the reads `GET /usage` needs come from the same store. [`open`] is the only
//! selection path, and it hands back **both halves at once**, so "the writer was registered but the
//! reader was forgotten" is not a state this design can represent.
//!
//! The insertion pattern for a new backend (and the candidate matrix) is owned by
//! `dev-docs/usage-backends.md`; the decisions behind it are in
//! `dev-docs/aegis/adr/ADR-0002-usage-backends.md`.

use std::collections::BTreeMap;
use std::sync::Arc;

use sqlx::SqlitePool;

pub use engine::UsageSink;
pub use query::UsageQuery;

pub mod backends;
/// The shared batching engine. It has exactly ONE user today (the ClickHouse backend), so a build
/// without `usage-clickhouse` compiles it unused — which is what the attribute says, rather than
/// deleting the machinery that the next backend is supposed to reuse (ADR-0002 §4 step 2: a backend
/// gets the delivery policy for free; re-implementing it would be a second policy with its own
/// drop-accounting, and the drop counters are what an operator alerts on).
#[cfg_attr(not(feature = "usage-clickhouse"), allow(dead_code, unused_imports))]
pub mod engine;
pub mod query;

/// One environment variable a backend **needs**.
///
/// The startup check is generic (see [`open`]): a backend never formats its own "variable missing"
/// message, so every backend's message names the same things — the variable, and what it is for.
pub struct Requirement {
    pub name: &'static str,
    /// What the variable is FOR. Printed in the error, so an operator can fix the deployment
    /// without reading this crate.
    pub purpose: &'static str,
}

/// Where `GET /usage` can be answered from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReaderContract {
    /// The backend that receives the writes can read them back.
    SameBackend,
    /// This deployment has no readable usage store at all (usage is switched off). `why` must state
    /// what the tenant's 503 means and what to set to get a reader — it is printed at startup.
    Unavailable { why: &'static str },
}

/// Where a backend reads its configuration from.
///
/// The process environment in production; a table in tests. `Fixed` **replaces** the process
/// environment instead of merging with it: a merge would make "this variable is unset" untestable
/// on a machine that happens to have it set, and would let a stray variable change a test's meaning.
#[derive(Clone, Debug)]
pub enum EnvView {
    Process,
    Fixed(BTreeMap<String, String>),
}

impl EnvView {
    /// A fixed environment, replacing the process one.
    #[must_use]
    pub fn fixed(pairs: &[(&str, &str)]) -> Self {
        Self::Fixed(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
    }

    /// Read one variable, or `None` when it is unset.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<String> {
        match self {
            Self::Process => std::env::var(name).ok(),
            Self::Fixed(map) => map.get(name).cloned(),
        }
    }
}

/// What a backend needs in order to be opened: the node's own database, and its configuration.
pub struct BackendConfig {
    /// This node's SQLite pool. Every node has one (ADR-0001 D-2), and the single-node backends
    /// read and write their rows there.
    pub pool: SqlitePool,
    pub env: EnvView,
}

impl BackendConfig {
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            env: EnvView::Process,
        }
    }

    #[must_use]
    pub fn with_env(mut self, env: EnvView) -> Self {
        self.env = env;
        self
    }
}

/// An opened backend: the writer (mandatory) and the reader (when this backend has one).
pub struct Backend {
    pub sink: Arc<dyn UsageSink>,
    pub query: Option<Arc<dyn UsageQuery>>,
    /// The descriptor's operator-facing note, carried so the startup log can say what this
    /// deployment actually does with usage (the kind alone does not).
    pub notes: &'static str,
    /// The descriptor's read contract, carried for the same reason: `main` warns uniformly when a
    /// deployment has no readable usage store, without knowing which backend said so.
    pub reads: ReaderContract,
}

/// Hand-written because `dyn UsageSink` / `dyn UsageQuery` are not `Debug` (and should not have to
/// be: they are the two capabilities, not data). What a reader of a failed test needs is which
/// halves arrived.
impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backend")
            .field("sink", &"<dyn UsageSink>")
            .field(
                "query",
                &if self.query.is_some() {
                    "<dyn UsageQuery>"
                } else {
                    "none"
                },
            )
            .field("notes", &self.notes)
            .finish()
    }
}

impl Backend {
    /// One line an operator can see in the log: what this node does with usage.
    #[must_use]
    pub fn describe(&self) -> &'static str {
        self.notes
    }
}

/// How a backend is opened. A plain `fn` pointer, so the registry is a `static` table with no
/// allocation and no hidden control flow: `grep` for the kind finds every backend.
pub type OpenFn = fn(&BackendConfig) -> Result<Backend, BackendError>;

/// One usage backend, declared once.
pub struct UsageBackend {
    /// The `HYDRA_USAGE_SINK` value.
    pub kind: &'static str,
    /// The cargo feature that compiles the implementation. Without it, `open` reports
    /// [`BackendError::FeatureDisabled`] instead of pretending the kind does not exist.
    pub feature: &'static str,
    /// Variables this backend **needs**; missing ones refuse the start, naming them.
    pub requires: &'static [Requirement],
    /// Variables it **recognises** with a default. Listed so an operator can find every knob a
    /// backend reads from one place (and so the guard can require them to be documented).
    pub recognises: &'static [&'static str],
    /// Whether `GET /usage` can be answered from this backend.
    pub reads: ReaderContract,
    /// Opens the backend, or explains why it cannot be opened.
    pub open: OpenFn,
    /// One operator-facing line, printed at startup.
    pub notes: &'static str,
}

/// Why a configured backend could not be opened.
///
/// Every variant is actionable: the message names the variable to set, the feature to build with, or
/// the value that is not a backend. None of them is a silent fallback — ADR-0002 §2.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error(
        "HYDRA_USAGE_SINK={kind:?} is not a usage backend this build knows; \
         known values: {known}"
    )]
    UnknownKind { kind: String, known: String },

    #[error(
        "HYDRA_USAGE_SINK={kind} needs {name} ({purpose}); it is not set, so this node has \
         nowhere to write usage"
    )]
    MissingEnv {
        kind: &'static str,
        name: &'static str,
        purpose: &'static str,
    },

    #[error(
        "HYDRA_USAGE_SINK={kind} is built by the `{feature}` cargo feature, which this binary \
         does not have; rebuild with --features {feature}"
    )]
    FeatureDisabled {
        kind: &'static str,
        feature: &'static str,
    },

    #[error("HYDRA_USAGE_SINK={kind} was RETIRED and is refused: {why}. Set {instead}")]
    Retired {
        kind: &'static str,
        why: &'static str,
        instead: &'static str,
    },

    /// The configuration is present but unusable. `message` is the backend's own actionable text
    /// (it knows what its transport can and cannot do).
    #[error("{kind}: {message}")]
    Invalid { kind: &'static str, message: String },
}

/// Every backend this build knows, in the order they are listed to an operator.
pub static REGISTRY: &[&UsageBackend] = &[
    // The shared store every cluster node writes to (ADR-0002: the only delivered backend).
    &backends::clickhouse::DESCRIPTOR,
    // Switching usage OFF is a registered choice, not an absence of one (ADR-0002 D-2).
    &backends::none::DESCRIPTOR,
    // ADR-0002's first NEW backend: the row below is the whole insertion, and the proof that
    // "adding a metrics database" is one module + one feature + one line. It is NOT a delivered
    // backend: the repository ships ClickHouse (see `dev-docs/usage-backends.md` §5).
    &backends::tdengine::DESCRIPTOR,
];

/// Values that were usage backends and are NOT any more (ADR-0002 D-3, user ruling: the table is
/// dropped, not kept).
///
/// They are listed so a deployment that still sets one is TOLD what happened and what to set
/// instead, rather than being told the value is not a backend (which reads like a typo). The
/// reconciliation guard (`scripts/check_usage_backends.cjs`) checks that nothing here can be
/// selected again.
pub static RETIRED_USAGE_SINKS: &[RetiredSink] = &[RetiredSink {
    kind: "sqlite",
    why:
        "it wrote usage into the node's OWN database, so a cluster's usage was scattered across \
          nodes and the node answering GET /usage was usually not the one that recorded the request",
    instead: "clickhouse (the shared store) — or none to record nothing at all",
}];

/// A retired `HYDRA_USAGE_SINK` value.
pub struct RetiredSink {
    pub kind: &'static str,
    pub why: &'static str,
    pub instead: &'static str,
}

/// The descriptors, as a list of kinds — for error messages and for guards.
#[must_use]
pub fn known_kinds() -> String {
    REGISTRY
        .iter()
        .map(|b| b.kind)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Look a backend up by its `HYDRA_USAGE_SINK` value.
pub fn descriptor(kind: &str) -> Result<&'static UsageBackend, BackendError> {
    if let Some(retired) = RETIRED_USAGE_SINKS.iter().find(|r| r.kind == kind) {
        // Checked FIRST: "sqlite" is not a typo, it is a value this product used to ship, and the
        // operator needs the difference (and the replacement) rather than a list of valid values.
        return Err(BackendError::Retired {
            kind: retired.kind,
            why: retired.why,
            instead: retired.instead,
        });
    }
    REGISTRY
        .iter()
        .find(|b| b.kind == kind)
        .copied()
        .ok_or_else(|| BackendError::UnknownKind {
            kind: kind.to_string(),
            known: known_kinds(),
        })
}

/// **The one selection path**: look the configured kind up and open it, both halves at once.
///
/// The kind is validated here rather than left to each caller: an unknown value, a value this build
/// cannot serve, and a missing variable are all refused before anything is served, and each refusal
/// says what to do about it.
pub fn open(kind: &str, cfg: &BackendConfig) -> Result<Backend, BackendError> {
    let backend = descriptor(kind)?;
    let opened = (backend.open)(cfg)?;
    check_contract(backend, opened)
}

/// The descriptor's declared read contract and what `open` actually returned must agree — otherwise
/// a deployment would be told "your usage is readable" while `/usage` answers 503 (or the reverse: a
/// reader that nothing points at). Checked at startup, on the real deployment, because that is where
/// it costs something.
///
/// A separate function so the rule can be tested with a descriptor that lies, without putting a
/// liar in the registry where an operator could select it.
fn check_contract(backend: &UsageBackend, mut opened: Backend) -> Result<Backend, BackendError> {
    opened.notes = backend.notes;
    opened.reads = backend.reads;
    match (backend.reads, opened.query.is_some()) {
        (ReaderContract::SameBackend, false) => Err(BackendError::Invalid {
            kind: backend.kind,
            message: "this backend declares that it can read the usage it writes, but opening it \
                      produced no reader; the deployment would serve 503 on GET /usage with no way \
                      to tell why"
                .to_string(),
        }),
        (ReaderContract::Unavailable { .. }, true) => Err(BackendError::Invalid {
            kind: backend.kind,
            message: "this backend declares that it cannot read usage, but opening it produced a \
                      reader nobody would consult"
                .to_string(),
        }),
        _ => Ok(opened),
    }
}

/// The in-repo test harness: a backend-shaped sink that records, and a fixed reader.
///
/// Compiled for tests only, and deliberately **not** in [`REGISTRY`] — no `HYDRA_USAGE_SINK` value
/// can select it, so a deployment cannot look configured while dropping its usage into memory
/// (ADR-0002 D-8).
#[cfg(test)]
pub(crate) mod testing;

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        crate::db::test_pool().await
    }

    /// The registry itself is the contract: every kind an operator can configure is a row here, and
    /// the list in the error message is generated from it rather than hand-written.
    #[test]
    fn the_registry_lists_every_backend_once() {
        let kinds: Vec<&str> = REGISTRY.iter().map(|b| b.kind).collect();
        let mut sorted = kinds.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            kinds.len(),
            "duplicate kind in REGISTRY: {kinds:?}"
        );
        for b in REGISTRY {
            assert!(!b.kind.is_empty() && !b.feature.is_empty());
            assert!(
                !b.notes.is_empty(),
                "{} needs an operator-facing note",
                b.kind
            );
            for r in b.requires {
                assert!(!r.name.is_empty() && !r.purpose.is_empty());
            }
        }
        assert!(known_kinds().contains("clickhouse"));
    }

    #[tokio::test]
    async fn an_unknown_kind_names_the_known_ones() {
        let err = open("cheese", &BackendConfig::new(pool().await)).unwrap_err();
        let msg = err.to_string();
        for kind in REGISTRY.iter().map(|b| b.kind) {
            assert!(
                msg.contains(kind),
                "the refusal must list {kind}, got: {msg}"
            );
        }
    }

    /// A missing variable is refused **by name**, with its purpose — the generic check, not a
    /// per-backend message (ADR-0002 §4 step 4).
    #[cfg(feature = "usage-clickhouse")]
    #[tokio::test]
    async fn a_missing_variable_is_refused_by_name() {
        let cfg = BackendConfig::new(pool().await).with_env(EnvView::fixed(&[]));
        let err = open("clickhouse", &cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("HYDRA_CLICKHOUSE_URL"), "got: {msg}");
        assert!(msg.contains("HYDRA_USAGE_SINK=clickhouse"), "got: {msg}");
        assert!(
            msg.contains("nowhere to write usage"),
            "the message must say what the variable is for, got: {msg}"
        );
    }

    /// The `https://` refusal belongs to the backend that knows its transport has no TLS path: a
    /// node that promised TLS and then sent credentials in the clear is the failure this prevents,
    /// and it must happen when the BACKEND IS OPENED (startup), not on the first flush.
    #[cfg(feature = "usage-clickhouse")]
    #[tokio::test]
    async fn an_https_clickhouse_url_is_refused_at_open() {
        let cfg = BackendConfig::new(pool().await).with_env(EnvView::fixed(&[(
            "HYDRA_CLICKHOUSE_URL",
            "https://user:pass@ch.example.com:8443",
        )]));
        let err = open("clickhouse", &cfg).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("https://") && msg.contains("PLAINTEXT"),
            "the refusal must be actionable, got: {msg}"
        );

        // The plaintext form still opens (this is a scheme check, not a ban).
        let cfg = BackendConfig::new(pool().await).with_env(EnvView::fixed(&[(
            "HYDRA_CLICKHOUSE_URL",
            "http://127.0.0.1:8123",
        )]));
        assert!(
            open("clickhouse", &cfg).is_ok(),
            "http:// must keep working"
        );
    }

    /// A build that cannot serve a kind still KNOWS it: the refusal names the feature to build with
    /// rather than claiming the value is not a backend. Those two are different mistakes and only one
    /// of them is the operator's.
    #[cfg(not(feature = "usage-clickhouse"))]
    #[tokio::test]
    async fn a_known_kind_this_build_cannot_serve_is_not_an_unknown_kind() {
        let cfg = BackendConfig::new(pool().await).with_env(EnvView::fixed(&[(
            "HYDRA_CLICKHOUSE_URL",
            "http://127.0.0.1:8123",
        )]));
        let err = open("clickhouse", &cfg).unwrap_err();
        assert!(
            matches!(err, BackendError::FeatureDisabled { .. }),
            "expected FeatureDisabled, got {err:?}"
        );
        assert!(err.to_string().contains("--features usage-clickhouse"));
    }

    /// `none` is a REGISTERED choice, and it says out loud that it cannot read: no store, no
    /// reader, and a reason the tenant API can hand the caller (ADR-0002 D-2).
    #[tokio::test]
    async fn the_none_backend_records_nothing_and_says_why() {
        let cfg = BackendConfig::new(pool().await).with_env(EnvView::fixed(&[]));
        let opened = open("none", &cfg).expect("none opens");
        assert!(opened.query.is_none(), "there is no store to read");
        match opened.reads {
            ReaderContract::Unavailable { why } => {
                assert!(
                    why.contains("HYDRA_USAGE_SINK=none") && why.contains("clickhouse"),
                    "the reason must name the value that was set and the one to set instead: {why}"
                );
            }
            ReaderContract::SameBackend => panic!("none has no reader"),
        }

        // Every record handed to it is COUNTED as lost, on the series an operator already alerts on
        // for real drops — "we meter nothing" must not be quieter than a dropped batch.
        let before = crate::admin::metrics::usage_dropped_total("sink_disabled");
        opened
            .sink
            .record(crate::usage::testing::record("none-1"))
            .await;
        let after = crate::admin::metrics::usage_dropped_total("sink_disabled");
        assert!(
            after > before,
            "a disabled sink must count what it discards ({before} -> {after})"
        );
    }

    /// `Fixed` REPLACES the process environment. A test that asserts "unset ⇒ refused" would
    /// otherwise pass or fail depending on the machine it runs on.
    #[test]
    fn a_fixed_environment_does_not_inherit_the_process_one() {
        let view = EnvView::fixed(&[]);
        assert_eq!(
            view.get("PATH"),
            None,
            "the process environment must not leak in"
        );
        assert_eq!(EnvView::fixed(&[("A", "1")]).get("A").as_deref(), Some("1"));
    }

    /// Opening the default single-node backend gives BOTH halves — the property that used to be two
    /// separate `match` arms.
    #[tokio::test]
    async fn opening_a_backend_returns_the_writer_and_the_reader() {
        let cfg = BackendConfig::new(pool().await).with_env(EnvView::fixed(&[]));
        let opened = open("none", &cfg).expect("the none backend opens");
        assert!(
            opened.query.is_none(),
            "a backend that declares it cannot read must not hand back a reader"
        );
    }

    /// The declared read contract and what `open` returned must agree, in BOTH directions: a
    /// backend that promises a reader and produces none would serve a 503 nobody can explain, and a
    /// reader nobody consults is a promise the deployment cannot keep either way.
    #[test]
    fn a_contract_that_disagrees_with_the_opened_backend_is_refused() {
        // The two lying descriptors are LOCAL to this test on purpose: a liar in `REGISTRY` would be
        // selectable from the environment, which is exactly what the registry exists to prevent.
        let promises_a_reader = UsageBackend {
            kind: "liar",
            feature: "db",
            requires: &[],
            recognises: &[],
            reads: ReaderContract::SameBackend,
            open: |_cfg| unreachable!("the check runs before any open in this test"),
            notes: "test-only descriptor",
        };
        let err = check_contract(
            &promises_a_reader,
            Backend {
                sink: Arc::new(SilentSink),
                query: None,
                notes: "test-only",
                reads: ReaderContract::SameBackend,
            },
        )
        .expect_err("a promised reader that did not appear must be refused");
        assert!(err.to_string().contains("no way to tell why"), "got: {err}");

        let denies_a_reader = UsageBackend {
            reads: ReaderContract::Unavailable {
                why: "usage is switched off",
            },
            ..promises_a_reader
        };
        let err = check_contract(
            &denies_a_reader,
            Backend {
                sink: Arc::new(SilentSink),
                query: Some(Arc::new(SilentQuery)),
                notes: "test-only",
                reads: ReaderContract::SameBackend,
            },
        )
        .expect_err("an unconsulted reader must be refused");
        assert!(
            err.to_string().contains("nobody would consult"),
            "got: {err}"
        );
    }

    struct SilentSink;

    impl UsageSink for SilentSink {
        fn record(
            &self,
            _record: hydra_core::model::UsageRecord,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
            Box::pin(async {})
        }
    }

    struct SilentQuery;

    impl UsageQuery for SilentQuery {
        fn aggregate<'a>(
            &'a self,
            _tenant_id: &'a str,
            _since: &'a str,
            _until: &'a str,
            _group_by: crate::usage::query::GroupBy,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            hydra_core::tenant_api::UsageAggregate,
                            crate::usage::query::UsageQueryError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async { unreachable!("never called") })
        }

        fn source(&self) -> &'static str {
            "silent"
        }
    }
}
