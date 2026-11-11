//! `hydra` — Pingora-based LLM gateway binary (design §6.1 / §15.1).
//!
//! Thin entrypoint: the boot sequence (tracing, DB pool, config store, Redis
//! fanout, listeners, admin service, Pingora `run_forever`) lives in
//! [`boot`]. See `boot::mod` for the startup sequence and the reason the binary
//! does not use `#[tokio::main]`.

// A `[[bin]]` target is its own compilation unit: the inner attribute in
// `lib.rs` does NOT apply here, so without this line the binary — the artifact
// the image ships and the only code that boots the process — was the one place
// `unsafe` would have compiled silently. `dev-docs/HANDOFF.md` claims
// "both crates: `#![forbid(unsafe_code)]`"; `scripts/check_source_purity.cjs`
// now asserts the attribute is present on every crate ROOT, binary included.
#![forbid(unsafe_code)]

mod boot;

fn main() {
    boot::run();
}
