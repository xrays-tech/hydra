//! The two copies of the `usage_record` DDL must not drift apart.
//!
//! There are TWO places that create the usage table:
//!
//! 1. `environment/clickhouse/init.sql` — mounted by the OFFICIAL stacks
//!    (`environment/docker-compose.yml` and `docker-compose.cluster.yml` both
//!    mount it into `/docker-entrypoint-initdb.d/`), so this is what a fresh
//!    production instance is built from.
//! 2. the inline DDL in `environment/docker-compose.local.yml`, used by the
//!    local test environment.
//!
//! They had already drifted: the retry-idempotency setting
//! `non_replicated_deduplication_window` — without which ClickHouse accepts
//! `insert_deduplication_token` and SILENTLY IGNORES it — was an executable
//! statement in the local compose and only a COMMENT in `init.sql`. So every
//! fresh instance from the official stacks lost exactly the protection the
//! setting exists for, and a re-sent batch (response lost after commit)
//! double-counted usage/quota/billing, while the local environment could never
//! show it. `dev-docs/ops.md` meanwhile said fresh instances got it from
//! `init.sql`.
//!
//! This guard needs no ClickHouse: it parses both files and compares them.

use std::path::PathBuf;

/// Where the ClickHouse usage INSERT lives. A constant so the parse below and every failure message
/// name the same file (the messages used to say `src/sink.rs` in three places).
const INSERT_SOURCE: &str = "crates/hydra-server/src/usage/backends/clickhouse/mod.rs";

fn repo_file(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Drop `--` comments so a statement that exists only as documentation cannot
/// be mistaken for one the database actually executes.
fn without_comments(text: &str) -> String {
    text.lines()
        .map(|l| match l.find("--") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The column names of the `usage_record` `CREATE TABLE`, in declaration order.
///
/// Line-based on purpose: a naive "first `(` … first `)`" scan is defeated by the
/// nested parens in `Nullable(String)`.
fn columns_of(ddl: &str) -> Vec<String> {
    let mut lines = ddl.lines();
    // Skip to the line that opens the column list.
    let mut found_open = false;
    let mut out = Vec::new();
    for line in lines.by_ref() {
        if line.contains('(') && line.trim_end().ends_with('(') {
            found_open = true;
            break;
        }
    }
    assert!(found_open, "no column list in:\n{ddl}");
    for line in lines {
        let t = line.trim();
        if t.starts_with(')') {
            break;
        }
        let t = t.trim_end_matches(',').trim();
        if t.is_empty() || t.starts_with("--") {
            continue;
        }
        if let Some(name) = t.split_whitespace().next() {
            out.push(name.to_string());
        }
    }
    out
}

/// `init.sql` holds the statement bare and terminates it with `;`.
fn init_sql_create(init_sql: &str) -> String {
    let ex = without_comments(init_sql);
    let start = ex.find("CREATE TABLE").expect("CREATE TABLE");
    let rest = &ex[start..];
    let end = rest.find(';').expect("statement terminator") + 1;
    rest[..end].to_string()
}

/// The compose file embeds the statement in a shell-quoted
/// `clickhouse-client --query "…"`, so the region ends at the closing quote.
/// Comments are stripped FIRST: the file also mentions `CREATE TABLE` inside a
/// comment above the real statement.
fn compose_create(compose: &str) -> String {
    let ex = without_comments(compose);
    let start = ex
        .find("CREATE TABLE IF NOT EXISTS usage_record")
        .expect("CREATE TABLE for usage_record");
    let rest = &ex[start..];
    let end = rest.find('"').expect("closing quote of the query string");
    rest[..end].to_string()
}

fn assert_same_columns(a: &[String], b: &[String], a_name: &str, b_name: &str) {
    assert!(
        !a.is_empty() && !b.is_empty(),
        "one of the two DDLs parsed to zero columns ({a_name}: {}, {b_name}: {}); \
         the guard is not looking at what it thinks it is",
        a.len(),
        b.len()
    );
    let mut sa = a.to_vec();
    let mut sb = b.to_vec();
    sa.sort();
    sb.sort();
    assert_eq!(
        sa, sb,
        "{a_name} and {b_name} declare different usage_record columns"
    );
}

#[test]
fn both_usage_record_ddls_declare_the_same_columns() {
    let init = repo_file("environment/clickhouse/init.sql");
    let compose = repo_file("environment/docker-compose.local.yml");
    assert_same_columns(
        &columns_of(&init_sql_create(&init)),
        &columns_of(&compose_create(&compose)),
        "environment/clickhouse/init.sql",
        "environment/docker-compose.local.yml",
    );
}

/// The column names the SINK writes, parsed from the ClickHouse backend's
/// `INSERT INTO usage_record (…)` statement.
///
/// The statement spans several source lines with `\` continuations, so the parse
/// splits on commas and trims the continuation markers.
fn insert_columns() -> Vec<String> {
    // Moved 2026-10-07 with the ClickHouse backend (`src/sink.rs` → this module, ADR-0002 T1.3):
    // this test is the canary for that file moving, and it fired exactly as the plan said it would.
    // If the INSERT moves again, this path must move with it — the failure is `cannot read …`.
    let src = without_comments(&repo_file(INSERT_SOURCE));
    let start = src
        .find("INSERT INTO usage_record (")
        .expect("the usage INSERT statement in the ClickHouse backend");
    let rest = &src[start..];
    let open = rest.find('(').expect("column list opens");
    let close = rest.find(')').expect("column list closes");
    rest[open + 1..close]
        .split(',')
        // Keep only identifier characters: the statement wraps across source lines
        // with `\` continuations, so a chunk can carry a leading backslash and
        // newline (the first attempt only trimmed at the END and produced
        // `"\\\n     status_code"`).
        .map(|c| {
            c.chars()
                .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect::<String>()
        })
        .filter(|c| !c.is_empty())
        .collect()
}

/// The WRITE path must name exactly the columns the DDL declares.
///
/// `both_usage_record_ddls_declare_the_same_columns` above compares the two DDL
/// copies; this is the other half of the same drift. A column added to the INSERT
/// without the DDL (or dropped from a DDL while the INSERT still names it) makes
/// ClickHouse reject EVERY usage insert, and the sink can only COUNT the loss
/// (`hydra_usage_records_dropped_total`) — the node stops metering usage/quota/
/// billing without any test failing. Measured live on 2026-09-29: the local
/// stack's `default.usage_record` predated `sub_tenant_id`, i.e. exactly this
/// state, and the current binary could not have written to it.
///
/// Falsification: add a column to the INSERT list, or delete one from
/// `environment/clickhouse/init.sql`, and this fails.
#[test]
fn the_usage_insert_names_exactly_the_ddl_columns() {
    let init = repo_file("environment/clickhouse/init.sql");
    let mut ddl_cols = columns_of(&init_sql_create(&init));
    let mut ins_cols = insert_columns();
    ddl_cols.sort();
    ins_cols.sort();
    assert!(
        !ins_cols.is_empty(),
        "the INSERT parse found no columns — the pattern in the ClickHouse backend changed"
    );
    assert_eq!(
        ins_cols, ddl_cols,
        "the ClickHouse INSERT and environment/clickhouse/init.sql name different usage_record columns"
    );
}

/// The dedup window must be EXECUTED by both, not merely documented by one.
///
/// Falsification: delete the `SETTINGS` line from `init.sql` (its pre-2026-09-29
/// state, where it existed only inside a `--` comment) and this fails.
#[test]
fn both_usage_record_ddls_enable_the_dedup_window() {
    let init = repo_file("environment/clickhouse/init.sql");
    let compose = repo_file("environment/docker-compose.local.yml");
    for (name, ddl) in [
        (
            "environment/clickhouse/init.sql",
            init_sql_create(&init).as_str(),
        ),
        (
            "environment/docker-compose.local.yml",
            compose_create(&compose).as_str(),
        ),
    ] {
        let executable = without_comments(ddl);
        assert!(
            executable.contains("SETTINGS")
                && executable.contains("non_replicated_deduplication_window"),
            "{name} must EXECUTE `SETTINGS non_replicated_deduplication_window = …` \
             in its CREATE TABLE: without it ClickHouse accepts \
             `insert_deduplication_token` and silently ignores it, so a re-sent \
             batch double-counts usage. A comment is not a setting.\n\
             executable part was:\n{executable}"
        );
    }
}

/// Row order is part of the table's identity for these queries; a divergence
/// here would silently change the primary index.
#[test]
fn both_usage_record_ddls_agree_on_the_ordering_key() {
    let init = repo_file("environment/clickhouse/init.sql");
    let compose = repo_file("environment/docker-compose.local.yml");
    let order_of = |ddl: &str| -> String {
        let ex = without_comments(ddl);
        let start = ex.find("ORDER BY").expect("ORDER BY") + "ORDER BY".len();
        let rest = ex[start..].trim_start();
        let end = rest.find(')').map_or(rest.len(), |i| i + 1);
        rest[..end].split_whitespace().collect::<Vec<_>>().join(" ")
    };
    assert_eq!(
        order_of(&init_sql_create(&init)),
        order_of(&compose_create(&compose)),
        "the two usage_record DDLs must use the same ordering key"
    );
}
