//! # hydra-core — pure domain core.
//!
//! The zero-I/O, zero-mock, exhaustively unit-testable heart of Hydra.
//! Everything here is a pure function / pure data structure with no async, no
//! network, no filesystem, no global mutable state. The I/O shell
//! (`hydra-server`) translates between Pingora/sqlx/reqwest and these types.
//!
//! ## Dependency firewall
//!
//! Allowed dependencies (enforced by the CI tree-gate, see
//! `tests/compile_gate.rs` and `.github/workflows/ci.yml`):
//! `serde`, `serde_json`, `memchr`, `bytes`, `sha2`.
//!
//! Forbidden (any I/O runtime / network / DB): `tokio`, `pingora`, `sqlx`,
//! `reqwest`, `hyper`. This welds the "internal logic must be pure" rule
//! (dev-plan §1 铁律 2) into the crate dependency graph.
//!
//! ## Time injection
//!
//! Any "mutable" state (breaker counts, SWRR weights, sliding-window samples,
//! auth cache entries) is driven by an explicitly-injected `now: Instant`, so
//! tests are deterministic. There is no hidden `Instant::now()` in this crate.
//!
//! See `dev-docs/dev-plan.md` §1–2 and `dev-docs/waves/wave-1-pure-core.md`.

#![forbid(unsafe_code)]

pub mod auth;
pub mod breaker;
pub mod config;
pub mod extract;
pub mod limit;
pub mod model;
pub mod rewrite;
pub mod router;
pub mod sse;
pub mod swrr;
