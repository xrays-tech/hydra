//! Build script: make `admin-ui/` a real build input.
//!
//! The admin UI is embedded with `include_dir!`, but cargo does not track the
//! contents of a directory a proc macro reads — so editing ONLY `admin-ui/*`
//! left the crate "fresh" and `cargo build --release` happily re-linked a binary
//! carrying the PREVIOUS UI. That is the "verified the wrong artifact" class of
//! mistake this repo has already been bitten by (a readiness probe satisfied by a
//! stale process): local e2e runs would silently exercise the old UI and pass.
//!
//! Declaring the directory here makes every UI edit rebuild the crate, so the
//! served `app.js` always matches the working tree.
fn main() {
    // Relative to this crate's root (crates/hydra-server).
    println!("cargo:rerun-if-changed=../../admin-ui");

    // `sqlx::migrate!` reads these directories at compile time. Stable cargo
    // does not track files a proc macro reads, so adding/editing a migration
    // would leave the crate "fresh" and re-link a binary carrying the PREVIOUS
    // embedded set. Declaring both backend directories invalidates it.
    println!("cargo:rerun-if-changed=migrations/postgres");
    println!("cargo:rerun-if-changed=migrations/mysql");
}
