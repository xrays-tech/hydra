//! The integration-test Redis database partition is a CONTRACT, not a comment.
//!
//! `tests/common/mod.rs` documents the rule ("lib unit tests own 1..=40,
//! integration tests hand-assign 41..=63, each call flushes its database") and
//! `common::real_redis_pool` asserts the 41..=63 range — but nothing checked that
//! two test FILES do not hand-assign the SAME number. Two files sharing a database
//! means one test's `FLUSHDB` deletes the other's keys mid-assertion: a flaky
//! failure whose cause is invisible from the failing test.
//!
//! This guard reads the sibling test sources and fails when a number is claimed
//! twice. It needs no Redis.
#![cfg(feature = "cluster-redis")]

use std::collections::HashMap;
use std::path::Path;

/// Collect `real_redis_pool(<n>)` call sites per file under `tests/`.
#[test]
fn no_two_test_files_share_a_redis_database() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut owners: HashMap<u32, Vec<String>> = HashMap::new();
    let entries = std::fs::read_dir(&dir).expect("tests dir");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        // This guard itself must not be scanned (it has no call sites of its own).
        if name == "redis_db_partition.rs" {
            continue;
        }
        let src = std::fs::read_to_string(&path).expect("read test file");
        let mut rest = src.as_str();
        while let Some(at) = rest.find("real_redis_pool(") {
            rest = &rest[at + "real_redis_pool(".len()..];
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(n) = digits.parse::<u32>() {
                owners.entry(n).or_default().push(name.clone());
            }
            // Skip past this call to keep the scan linear.
            rest = rest.split_once(')').map(|(_, r)| r).unwrap_or("");
        }
    }

    assert!(
        !owners.is_empty(),
        "the scan found no `real_redis_pool(<n>)` call sites at all — the guard is \
         not looking where the tests are"
    );

    let collisions: Vec<String> = owners
        .iter()
        .filter(|(_, files)| files.len() > 1)
        .map(|(db, files)| format!("db {db}: {files:?}"))
        .collect();
    assert!(
        collisions.is_empty(),
        "two test files share a Redis database (each call FLUSHes it, so they will \
         delete each other's keys): {collisions:?}"
    );
}
