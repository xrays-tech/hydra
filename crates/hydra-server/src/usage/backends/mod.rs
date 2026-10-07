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
pub mod sqlite;
