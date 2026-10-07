//! One module per usage backend (ADR-0002 §4 step 2).
//!
//! A backend module owns its descriptor and the code that opens it. When a backend's transport lives
//! in its own file it goes here too (`<kind>/transport.rs`), so "which files belong to this backend"
//! is answerable by looking at one directory.
//!
//! Adding a backend is: a module here, a `usage-<kind>` feature, a line in
//! [`super::REGISTRY`](crate::usage::REGISTRY), and its tests — see `dev-docs/usage-backends.md`.

pub mod clickhouse;
pub mod none;
/// The TDengine candidate (ADR-0002 §5): in a build without its feature, only the descriptor is
/// reachable — so the rest of the module is dead code there, which is what the attribute says rather
/// than leaving a reader to wonder whether the backend is compiled.
#[cfg_attr(not(feature = "usage-tdengine"), allow(dead_code, unused_imports))]
pub mod tdengine;
