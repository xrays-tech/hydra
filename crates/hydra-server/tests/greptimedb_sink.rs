//! The GreptimeDB backend's wire contract, driven against a raw TCP server that answers with the
//! bytes we choose (no GreptimeDB needed): the DDL bootstrap, the multi-row INSERT and the read SQL
//! must all carry the measured shape, and the DDL and INSERT must name the SAME columns.
//!
//! This file deliberately does NOT write the backend's kind as a string literal (the
//! `check_usage_backends.cjs` guard walks `tests/`): every reference reaches the descriptor through
//! `hydra_server::usage::backends::greptimedb::DESCRIPTOR`, so the registry stays the only owner of
//! the name.
//!
//! Run:
//! ```bash
//! cargo test -p hydra-server --features server,usage-greptimedb,db-postgres \
//!   --test greptimedb_sink
//! ```

#![cfg(feature = "usage-greptimedb")]

mod common;

use std::sync::{Arc, Mutex};

use hydra_core::model::UsageRecord;
use hydra_server::usage::backends::greptimedb::DESCRIPTOR;
use hydra_server::usage::query::GroupBy;
use hydra_server::usage::{self, BackendConfig, EnvView};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A raw-TCP GreptimeDB double: it records the `sql=` body of every request and answers a fixed
/// 200. The response body is the canned SELECT answer.
struct Mock {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
}

/// Decode the `application/x-www-form-urlencoded` `sql=` value from a request body.
///
/// The body is `sql=<percent-encoded statement>`; the value (not the whole `sql=…` pair) is what the
/// server executes, so this strips the name before decoding. Returning the whole pair here is the
/// bug that made the mock answer a SELECT with the INSERT/DDL `affectedrows` envelope.
fn form_sql(body: &str) -> String {
    for pair in body.split('&') {
        if let Some(raw) = pair.strip_prefix("sql=") {
            return percent_decode(raw);
        }
    }
    String::new()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Read one complete HTTP request (headers plus a `Content-Length` body).
async fn read_request(sock: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
            let len = headers
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if buf.len() >= pos + 4 + len {
                return buf;
            }
        }
        let n = sock.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            return buf;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

async fn spawn_mock(totals_body: &'static str, grouped_body: &'static str) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let raw = read_request(&mut sock).await;
            let text = String::from_utf8_lossy(&raw).into_owned();
            let body = match text.find("\r\n\r\n") {
                Some(i) => form_sql(&text[i + 4..]),
                None => String::new(),
            };
            recorded.lock().expect("requests").push(body.clone());

            // The real GreptimeDB answers DDL/INSERT with `output[0].affectedrows` and a SELECT
            // with `output[0].records{…}`. Answering a SELECT with the INSERT envelope is the exact
            // bug this mock makes visible.
            let upper = body.trim_start().to_ascii_uppercase();
            let payload = if upper.starts_with("SELECT") {
                if body.contains(" AS group_key") {
                    grouped_body.to_string()
                } else {
                    totals_body.to_string()
                }
            } else {
                r#"{"output":[{"affectedrows":1}],"execution_time_ms":0}"#.to_string()
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                payload.len(),
                payload
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    Mock { port, requests }
}

fn sample() -> UsageRecord {
    UsageRecord {
        tenant_id: "tenant-1".to_string(),
        provider_id: "provider-1".to_string(),
        model_key: "gpt-test".to_string(),
        client_api_key_masked: None,
        user_id: Some("user-1".into()),
        user_group_id: Some("group-1".into()),
        api_key_id: Some("key-1".into()),
        status_code: 200,
        tokens_in: Some(12),
        tokens_out: Some(34),
        cache_hit_tokens: Some(5),
        latency_ms: 250,
        forward_latency_ms: Some(8),
        ttft_ms: Some(180),
        upstream_host: Some("upstream.example".to_string()),
        error: None,
        trace_id: "trace-1".to_string(),
        created_at: "2026-11-11T00:00:00Z".to_string(),
    }
}

/// The column names in a `CREATE TABLE usage_record (…)`, excluding the table constraint.
fn schema_columns(ddl: &str) -> Vec<String> {
    let marker = "usage_record (";
    let open = ddl.find(marker).expect("table open") + marker.len();
    let bytes = ddl.as_bytes();
    let mut depth = 1usize;
    let mut i = open;
    while i < bytes.len() && depth > 0 {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    let inner = &ddl[open..i.saturating_sub(1)];
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (j, c) in inner.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(inner[start..j].trim().to_string());
                start = j + 1;
            }
            _ => {}
        }
    }
    parts.push(inner[start..].trim().to_string());
    parts
        .into_iter()
        .filter(|p| !p.to_ascii_uppercase().starts_with("PRIMARY KEY"))
        .filter_map(|p| p.split_whitespace().next().map(str::to_string))
        .filter(|s| !s.is_empty())
        .collect()
}

/// The column names in an `INSERT INTO usage_record (…)`.
fn insert_columns(sql: &str) -> Vec<String> {
    let marker = "usage_record (";
    let open = sql.find(marker).expect("insert open") + marker.len();
    let close = sql[open..].find(')').expect("insert close") + open;
    sql[open..close]
        .split(',')
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect()
}

const TOTALS_BODY: &str = r#"{"output":[{"records":{"schema":{"column_schemas":[{"name":"requests","data_type":"UInt64"},{"name":"tokens_in","data_type":"UInt64"},{"name":"tokens_out","data_type":"UInt64"},{"name":"cache_hit_tokens","data_type":"UInt64"},{"name":"errors","data_type":"UInt64"},{"name":"last_seen","data_type":"String"}]},"rows":[[2,33,14,0,0,"2026-11-11T00:20:00Z"]]}}],"execution_time_ms":1}"#;

const GROUPED_BODY: &str = r#"{"output":[{"records":{"schema":{"column_schemas":[{"name":"group_key","data_type":"String"},{"name":"requests","data_type":"UInt64"},{"name":"tokens_in","data_type":"UInt64"},{"name":"tokens_out","data_type":"UInt64"},{"name":"cache_hit_tokens","data_type":"UInt64"},{"name":"errors","data_type":"UInt64"}]},"rows":[["gpt-4o",2,33,14,0,0]]}}],"execution_time_ms":1}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_wire_mock_sees_the_ddl_insert_and_read_and_the_schema_matches() {
    let mock = spawn_mock(TOTALS_BODY, GROUPED_BODY).await;
    let pool = common::setup_pool().await;
    let url = format!("http://127.0.0.1:{}/hydra_usage", mock.port);
    let cfg = BackendConfig::new(pool).with_env(EnvView::fixed(&[(
        DESCRIPTOR.requires[0].name,
        url.as_str(),
    )]));

    // `open` bootstraps the schema against the mock and hands back both halves.
    let opened = usage::open(DESCRIPTOR.kind, &cfg).expect("the backend opens against the mock");

    // The reader decodes the mock's SELECT bodies, from the backend's own reader. A SELECT must
    // decode `output[0].records`, NEVER the `affectedrows` envelope of an INSERT/DDL.
    let reader = opened.query.expect("this backend reads");
    assert_eq!(reader.source(), DESCRIPTOR.kind);
    let agg = reader
        .aggregate(
            "tenant-1",
            "2026-11-11T00:00:00Z",
            "2026-11-11T01:00:00Z",
            GroupBy::None,
        )
        .await
        .expect("the mock's totals SELECT body decodes");
    assert_eq!(agg.totals.requests, 2);
    assert_eq!(agg.totals.tokens_in, 33);
    assert_eq!(agg.as_of.as_deref(), Some("2026-11-11T00:20:00Z"));

    let grouped = reader
        .aggregate(
            "tenant-1",
            "2026-11-11T00:00:00Z",
            "2026-11-11T01:00:00Z",
            GroupBy::Model,
        )
        .await
        .expect("the mock's grouped SELECT body decodes");
    assert_eq!(grouped.rows.len(), 1, "{grouped:?}");
    assert_eq!(grouped.rows[0].key, "gpt-4o");
    assert_eq!(grouped.rows[0].totals.requests, 2);
    assert_eq!(grouped.rows[0].totals.tokens_in, 33);

    // Write through the REAL sink (the shared engine + transport).
    let sink = opened.sink.clone();
    sink.record(sample()).await;
    let mut hostile = sample();
    hostile.trace_id = "trace-2".into();
    hostile.model_key = "m'odel".into();
    sink.record(hostile).await;
    sink.shutdown().await;

    let reqs = mock.requests.lock().expect("requests").clone();

    // The bootstrap issued the two DDL statements; the insert is a multi-row INSERT. The mock
    // answered them with `affectedrows`, which the writer reads to confirm the write landed.
    let create_table = reqs
        .iter()
        .find(|s| s.starts_with("CREATE TABLE"))
        .expect("CREATE TABLE was sent");
    assert!(
        reqs.iter().any(|s| s.starts_with("CREATE DATABASE")),
        "CREATE DATABASE was sent: {reqs:?}"
    );
    let insert = reqs
        .iter()
        .find(|s| s.starts_with("INSERT INTO"))
        .expect("INSERT was sent");
    assert!(
        insert.matches("),(").count() >= 1,
        "a batch is a MULTI-ROW INSERT: {insert}"
    );

    // PARITY: the INSERT names exactly the DDL's columns, in order.
    assert_eq!(
        insert_columns(insert),
        schema_columns(create_table),
        "the INSERT and the DDL must name the same usage_record columns"
    );

    // The DDL is the measured canonical shape.
    assert!(
        create_table.contains("ts TIMESTAMP TIME INDEX"),
        "{create_table}"
    );
    assert!(
        create_table.contains("PRIMARY KEY (tenant_id, trace_id)"),
        "{create_table}"
    );
    assert!(
        create_table.contains("'append_mode'='false'"),
        "{create_table}"
    );
    assert!(create_table.contains("'ttl'='90d'"), "{create_table}");

    // A quote in caller-influenced text is doubled, never able to end the literal.
    assert!(insert.contains("m''odel"), "{insert}");
    // Optional numbers are NULL, not 0 or ''.
    assert!(
        insert.contains(",NULL,"),
        "absent optionals are NULL: {insert}"
    );

    // The totals SELECT escaped the tenant literal (there is no bind parameter).
    let totals_sql = reqs
        .iter()
        .find(|s| s.starts_with("SELECT count(*)"))
        .expect("a totals SELECT was sent");
    assert!(
        totals_sql.contains("tenant_id = 'tenant-1'"),
        "the tenant must be an escaped literal: {totals_sql}"
    );
    // The window is a RANGE on the `ts` TIME INDEX, written as bare epoch-millisecond bounds (no
    // quotes): `2026-11-11T00:00:00Z` = 1794355200000ms, `2026-11-11T01:00:00Z` = 1794358800000ms.
    // The tenant stays an escaped literal (there is no bind parameter).
    assert!(
        totals_sql.contains("ts >= 1794355200000") && totals_sql.contains("ts < 1794358800000"),
        "the window must be a bare epoch-ms range on `ts`: {totals_sql}"
    );
    assert!(
        !totals_sql.contains("created_at >=") && !totals_sql.contains("created_at <"),
        "the window must NOT be the old `created_at` string predicate: {totals_sql}"
    );
    assert!(
        totals_sql.contains("max(created_at) AS last_seen"),
        "{totals_sql}"
    );

    // The grouped SELECT aliases the whitelisted expression as `group_key` and groups by it.
    let grouped_sql = reqs
        .iter()
        .find(|s| s.contains(" AS group_key"))
        .expect("a grouped SELECT was sent");
    assert!(
        grouped_sql.contains("model_key AS group_key"),
        "{grouped_sql}"
    );
    assert!(grouped_sql.contains("GROUP BY model_key"), "{grouped_sql}");
}

/// `https://` is refused at `open`, before anything is dialled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_https_url_is_refused_at_open() {
    let pool = common::setup_pool().await;
    let cfg = BackendConfig::new(pool).with_env(EnvView::fixed(&[(
        DESCRIPTOR.requires[0].name,
        "https://user:pass@example.com:4000",
    )]));
    let err = usage::open(DESCRIPTOR.kind, &cfg).expect_err("https must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("https://") && msg.contains("PLAINTEXT"),
        "the refusal must be actionable: {msg}"
    );
}
