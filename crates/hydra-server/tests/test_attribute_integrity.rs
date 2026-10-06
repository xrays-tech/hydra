//! A test attribute must belong to exactly one function.
//!
//! Twice now, inserting a new test immediately above an existing one has left
//! the file in this state:
//!
//! ```ignore
//! // T5.17 — …
//! #[test]                      // ← the ORIGINAL attribute
//! /// docs of the NEW test
//! #[test]                      // ← the new test's attribute
//! fn the_new_test() { … }
//!
//! fn the_original_test() { … }  // ← now has no attribute at all
//! ```
//!
//! The effect is silent and asymmetric: the new test runs, the ORIGINAL test
//! stops running while still being cited as evidence in
//! `dev-docs/aegis/plans/2026-09-29-oracle-remediation.md`, and `cargo test`
//! reports a normal green run. `cargo clippy … -- -D warnings` does catch it
//! (`duplicate_macro_attributes` + `dead_code`), but only when clippy is run, and
//! two tests were lost this way in the 2026-09-29 remediation before anyone did.
//!
//! This guard runs in the ordinary test suite, needs no features and no
//! external service, and fails on the exact shape above: two test attributes in
//! one attribute block, i.e. one function claiming both.

use std::path::{Path, PathBuf};

/// `#[test]`, `#[test(...)]`, `#[tokio::test]`, `#[tokio::test(...)]` — but NOT
/// `#[cfg(test)]`, which is a different thing entirely.
fn is_test_attribute(trimmed: &str) -> bool {
    if !trimmed.starts_with("#[") {
        return false;
    }
    let inner = trimmed
        .trim_start_matches("#[")
        .split(']')
        .next()
        .unwrap_or_default()
        .trim();
    let path = inner.split('(').next().unwrap_or_default().trim();
    path == "test" || path == "tokio::test" || path.ends_with("::test")
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Build output is not source; vendored copies are not ours.
            if path.file_name().and_then(|n| n.to_str()) == Some("target") {
                continue;
            }
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Every test attribute must be followed by a function before the next one.
#[test]
fn no_function_claims_two_test_attributes() {
    let crates_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .to_path_buf();
    let mut files = Vec::new();
    collect_rs_files(&crates_dir, &mut files);
    assert!(
        files.len() > 20,
        "the scan found only {} source files under {} — it is not looking where \
         the tests are",
        files.len(),
        crates_dir.display()
    );

    let mut problems = Vec::new();
    for file in &files {
        let Ok(src) = std::fs::read_to_string(file) else {
            continue;
        };
        // The line number of a pending test attribute, if any.
        let mut pending: Option<usize> = None;
        for (i, line) in src.lines().enumerate() {
            let t = line.trim();
            if t.is_empty() || t.starts_with("///") || t.starts_with("//!") || t.starts_with("//") {
                continue;
            }
            if is_test_attribute(t) {
                if let Some(first) = pending {
                    problems.push(format!(
                        "{}:{} and {}:{} are two test attributes for one function — the \
                         attribute at :{} was detached from its own `fn`, which therefore \
                         no longer runs",
                        file.display(),
                        first,
                        file.display(),
                        i + 1,
                        first,
                    ));
                }
                pending = Some(i + 1);
                continue;
            }
            if t.starts_with("#[") {
                // Another attribute in the same block (`#[ignore]`, `#[should_panic]`, …).
                continue;
            }
            if t.starts_with("fn ") || t.starts_with("async fn ") || t.starts_with("pub fn ") {
                pending = None;
                continue;
            }
            // Any other item ends the attribute block.
            pending = None;
        }
    }

    assert!(
        problems.is_empty(),
        "test attributes are detached from their functions:\n{}",
        problems.join("\n")
    );
}
