//! The **single owner of "how to talk to ClickHouse"**.
//!
//! Both the usage *writer* (`sink.rs`'s batched `INSERT`) and the usage
//! *reader* (the tenant API's aggregate `SELECT`) go through this module, so the
//! URL/credential interpretation, the request-line shape, the deadlines and the
//! status classification exist exactly once. Before this module existed the
//! reader would have had to re-implement all of it — the second owner the design
//! forbids.
//!
//! ## Why a hand-written HTTP request on raw TCP
//!
//! The `clickhouse` crate pinned in `Cargo.toml` is an empty placeholder on
//! crates.io, and `Cargo.toml` is owned by another lane. ClickHouse's HTTP
//! interface is first-class and stable, so this module speaks it directly with
//! `tokio` (already a `runtime` dependency). Switching to a real driver later is
//! a change confined to [`send`].
//!
//! ## Deadlines are load-bearing
//!
//! ClickHouse answering a connection and then never replying used to pin the
//! single flush task forever: the bounded channel filled and every subsequent
//! usage record was dropped (audit §3.9). Every step below is therefore
//! deadline-bound, the response read is additionally size-capped and does not
//! wait for EOF, and the two deadlines live in [`ClickHouseConfig`] so a caller
//! can choose its own values (the writer uses the measured defaults; the reader
//! uses the query timeout) **without changing this module**.
//!
//! ## Feature gating
//!
//! The whole module is gated on `usage-clickhouse`, which `server` does **not**
//! imply. Without it `sink_kind == "clickhouse"` cannot occur at all:
//! `sink::build_sink` fails at startup for that kind. So the gates around the
//! users of this module describe a *compile-time* split, not a runtime branch.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Configuration parsed once from the ClickHouse URL.
#[derive(Clone)]
pub(crate) struct ClickHouseConfig {
    /// `host:port` for the TCP connection (e.g. `127.0.0.1:8123`).
    pub(crate) host_port: String,
    /// HTTP Basic credentials from `user:pass@host` URL userinfo, if present.
    /// Sent as `Authorization: Basic <base64(user:pass)>` — ClickHouse's HTTP
    /// interface accepts Basic auth natively.
    pub(crate) auth: Option<(String, String)>,
    /// Any query string from the URL (e.g. `?database=dogress` or
    /// `?user=x&password=y`), WITHOUT the leading `?`. Appended to the POST
    /// request's query string; empty when the URL had none.
    pub(crate) query_params: String,
    /// Deadline for the TCP connect
    /// (`HYDRA_CLICKHOUSE_CONNECT_TIMEOUT_MS`, default 3000).
    pub(crate) connect_timeout: Duration,
    /// Deadline for EACH of write / flush / response read
    /// (`HYDRA_CLICKHOUSE_IO_TIMEOUT_MS`, default 15000).
    pub(crate) io_timeout: Duration,
}

/// Largest response we buffer from ClickHouse. A successful `INSERT` answers
/// `200` with an empty body; only error text is ever sent, so anything past
/// this is truncation-safe and keeps a misbehaving server from growing our heap.
/// An aggregate `SELECT` answer is far smaller than this, so it is bounded too.
pub(crate) const MAX_CLICKHOUSE_RESPONSE: usize = 64 * 1024;

/// Read `KEY` as a positive millisecond count, falling back to `default_ms`.
pub(crate) fn env_millis(key: &str, default_ms: u64) -> Duration {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_millis(default_ms))
}

/// Parse a ClickHouse URL into transport + credentials. Accepted forms:
///
/// - `http://host:port` / `https://host:port` / bare `host:port` (anonymous);
/// - `http://user:pass@host:port` — userinfo becomes HTTP Basic auth;
/// - any of the above plus a query string (`?database=dogress`,
///   `?user=x&password=y`), which is passed through verbatim.
///
/// CR/LF in credentials are stripped (header-injection guard); the password is
/// otherwise sent as-is inside the Basic auth header.
pub(crate) fn parse_clickhouse_url(url: &str) -> ClickHouseConfig {
    let stripped = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    // Split off the query string first (kept for passthrough).
    let (authority, query) = match stripped.split_once('?') {
        Some((a, q)) => (a, Some(q)),
        None => (stripped, None),
    };
    // Split off userinfo (`user[:pass]@`).
    let (userinfo, host_port) = match authority.split_once('@') {
        Some((ui, hp)) => (Some(ui), hp),
        None => (None, authority),
    };
    let auth = userinfo.and_then(|ui| {
        let (user, pass) = match ui.split_once(':') {
            Some((u, p)) => (u, p),
            None => (ui, ""),
        };
        if user.is_empty() {
            None
        } else {
            Some((strip_crlf(user), strip_crlf(pass)))
        }
    });
    ClickHouseConfig {
        // Trim a trailing '/' (and any path): we only dial host:port.
        host_port: host_port.trim_end_matches('/').to_string(),
        auth,
        query_params: query.unwrap_or("").to_string(),
        connect_timeout: env_millis("HYDRA_CLICKHOUSE_CONNECT_TIMEOUT_MS", 3_000),
        io_timeout: env_millis("HYDRA_CLICKHOUSE_IO_TIMEOUT_MS", 15_000),
    }
}

/// Strip CR/LF so user-supplied URL credentials can never inject HTTP headers.
fn strip_crlf(s: &str) -> String {
    s.chars().filter(|&c| c != '\r' && c != '\n').collect()
}

/// Percent-encode a string for use in a query-string segment (RFC 3986
/// unreserved characters kept; everything else `%HH`).
///
/// Used for the SQL itself **and** for every `param_*` value: both travel in the
/// query string, and an unencoded `&`, `+` or `%` in a tenant id or a timestamp
/// would silently change the bound value (a `+` may even decode to a space).
pub(crate) fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

/// Send one request over ClickHouse's HTTP interface and return the response.
///
/// `query` is the SQL (INSERT or SELECT), `params` are `param_*` bindings for
/// the `{name:Type}` placeholders in it (empty for an INSERT built as a literal
/// statement), and `body` is the request payload (`FORMAT JSONEachRow` rows, or
/// empty for a SELECT).
///
/// Returns `(status_line, raw_body)` on **any** complete HTTP response —
/// including non-2xx, because classifying those belongs to the caller
/// ([`is_ok_status`] for the writer, the `Code: N` body for the reader). Returns
/// `Err` only when the exchange itself did not complete (connect failed, or a
/// deadline expired), which the writer treats as "the batch may not have landed"
/// and retries.
///
/// Every step is deadline-bound and the read is size-capped (audit §3.9).
pub(crate) async fn send(
    cfg: &ClickHouseConfig,
    query: &str,
    params: &[(&str, &str)],
    body: &[u8],
) -> Result<(String, Vec<u8>), String> {
    // POST /?<passthrough params>&query=<url-encoded SQL>&param_k=v … HTTP/1.1
    let mut request = String::with_capacity(body.len() + query.len() * 2 + 256);
    request.push_str("POST /?");
    if !cfg.query_params.is_empty() {
        request.push_str(&cfg.query_params);
        request.push('&');
    }
    request.push_str("query=");
    request.push_str(&url_encode(query));
    for (k, v) in params {
        request.push_str("&param_");
        request.push_str(&url_encode(k));
        request.push('=');
        request.push_str(&url_encode(v));
    }
    request.push_str(" HTTP/1.1\r\n");
    request.push_str("Host: ");
    request.push_str(&cfg.host_port);
    request.push_str("\r\n");
    if let Some((user, pass)) = &cfg.auth {
        use base64::engine::general_purpose::STANDARD as B64;
        use base64::Engine as _;
        let token = B64.encode(format!("{user}:{pass}"));
        request.push_str("Authorization: Basic ");
        request.push_str(&token);
        request.push_str("\r\n");
    }
    request.push_str("Content-Length: ");
    request.push_str(&body.len().to_string());
    request.push_str("\r\nConnection: close\r\n\r\n");

    // Every step is deadline-bound (audit §3.9): the writer runs on the single
    // sink flush task, so one blocked await here would stop ALL usage metering
    // for the whole node and silently overflow the channel behind it.
    let hp = cfg.host_port.clone();
    let mut stream = tokio::time::timeout(
        cfg.connect_timeout,
        tokio::net::TcpStream::connect(&cfg.host_port),
    )
    .await
    .map_err(|_| {
        format!(
            "clickhouse connect {hp}: timed out after {}ms",
            cfg.connect_timeout.as_millis()
        )
    })?
    .map_err(|e| format!("clickhouse connect {hp}: {e}"))?;

    // Header block, then the payload.
    tokio::time::timeout(cfg.io_timeout, stream.write_all(request.as_bytes()))
        .await
        .map_err(|_| {
            format!(
                "clickhouse write: timed out after {}ms",
                cfg.io_timeout.as_millis()
            )
        })?
        .map_err(|e| format!("clickhouse write: {e}"))?;
    if !body.is_empty() {
        tokio::time::timeout(cfg.io_timeout, stream.write_all(body))
            .await
            .map_err(|_| {
                format!(
                    "clickhouse write: timed out after {}ms",
                    cfg.io_timeout.as_millis()
                )
            })?
            .map_err(|e| format!("clickhouse write: {e}"))?;
    }
    tokio::time::timeout(cfg.io_timeout, stream.flush())
        .await
        .map_err(|_| {
            format!(
                "clickhouse flush: timed out after {}ms",
                cfg.io_timeout.as_millis()
            )
        })?
        .map_err(|e| format!("clickhouse flush: {e}"))?;

    // Read the response with BOTH a deadline and a size cap. `read_to_end`
    // alone would wait for EOF: a server that sends a status line and then keeps
    // the connection open would hang us until the deadline, and a chatty server
    // could grow `resp` without bound.
    let mut resp = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = tokio::time::timeout(cfg.io_timeout, stream.read(&mut chunk))
            .await
            .map_err(|_| {
                format!(
                    "clickhouse read: timed out after {}ms",
                    cfg.io_timeout.as_millis()
                )
            })?
            .map_err(|e| format!("clickhouse read: {e}"))?;
        if n == 0 {
            break;
        }
        let room = MAX_CLICKHOUSE_RESPONSE.saturating_sub(resp.len());
        if room == 0 {
            // Enough to classify the status/error; stop waiting for EOF.
            break;
        }
        resp.extend_from_slice(&chunk[..n.min(room)]);
    }

    let status_line = String::from_utf8_lossy(&resp)
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    Ok((status_line, resp))
}

/// Whether a response status line means success.
///
/// Deliberately the **same substring test the writer has always used**, moved
/// here unchanged: this commit is a move, and tightening the comparison (it
/// would also match `" 2000 "`) is a behaviour change that belongs in its own
/// reviewed change.
pub(crate) fn is_ok_status(status_line: &str) -> bool {
    status_line.contains(" 200 ")
}

/// The HTTP body of a response, with `Transfer-Encoding: chunked` **decoded**,
/// trimmed.
///
/// ClickHouse frames its answers as chunks (measured: `Transfer-Encoding:
/// chunked`, no `Content-Length`), so the bytes after the header block begin
/// with a hex chunk size and end with a `0` chunk. Reading them undecoded yields
/// `"7C\r\n{...}"`, which a JSON parser rejects with "trailing characters at
/// line 1 column 2". The **live** ClickHouse test caught exactly that; a
/// `Content-Length`-based double cannot reproduce it.
///
/// Truncation is tolerated: the transport caps the read at
/// [`MAX_CLICKHOUSE_RESPONSE`], so a cut-off body returns whatever chunk data was
/// complete. A caller that needs a *valid* body (the reader) fails its own decode
/// check rather than acting on a partial one.
pub(crate) fn response_body(resp: &[u8]) -> String {
    let Some(split) = find_subslice(resp, b"\r\n\r\n") else {
        return String::new();
    };
    let head = String::from_utf8_lossy(&resp[..split]);
    let body = &resp[split + 4..];
    let chunked = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    let decoded = if chunked {
        dechunk(body)
    } else {
        body.to_vec()
    };
    String::from_utf8_lossy(&decoded).trim().to_string()
}

/// The first index of `needle` in `hay`.
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// Decode HTTP/1.1 chunked framing. Stops at the terminating `0` chunk, and on
/// anything malformed or truncated returns what was decoded so far.
fn dechunk(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut rest = body;
    // `while let` rather than `loop { let … else { break } }`: the condition is
    // re-evaluated per iteration in both spellings, and every other exit here is
    // a `break` (the terminating `0` chunk, a malformed size, a truncated chunk),
    // which `while let` supports unchanged.
    while let Some(pos) = find_subslice(rest, b"\r\n") {
        // A chunk-size line may carry extensions after `;`.
        let size_line = &rest[..pos];
        let hex = size_line.split(|b| *b == b';').next().unwrap_or(b"");
        let Ok(hex) = std::str::from_utf8(hex) else {
            break;
        };
        let Ok(n) = usize::from_str_radix(hex.trim(), 16) else {
            break;
        };
        if n == 0 {
            break;
        }
        let start = pos + 2;
        let end = start.saturating_add(n);
        if rest.len() < end {
            out.extend_from_slice(&rest[start.min(rest.len())..]);
            break;
        }
        out.extend_from_slice(&rest[start..end]);
        rest = &rest[end..];
        if rest.starts_with(b"\r\n") {
            rest = &rest[2..];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // URL parsing (moved here with the parser)
    // -----------------------------------------------------------------------

    #[test]
    fn parse_url_anonymous() {
        let cfg = parse_clickhouse_url("http://127.0.0.1:8123");
        assert_eq!(cfg.host_port, "127.0.0.1:8123");
        assert!(cfg.auth.is_none());
        assert_eq!(cfg.query_params, "");
    }

    #[test]
    fn parse_url_bare_host() {
        let cfg = parse_clickhouse_url("clickhouse:8123");
        assert_eq!(cfg.host_port, "clickhouse:8123");
        assert!(cfg.auth.is_none());
    }

    #[test]
    fn parse_url_userinfo_becomes_basic_auth() {
        let cfg = parse_clickhouse_url("http://sh_admin:sH_9527!@clickhouse:8123");
        assert_eq!(cfg.host_port, "clickhouse:8123");
        assert_eq!(cfg.auth, Some(("sh_admin".into(), "sH_9527!".into())));
        assert_eq!(cfg.query_params, "");
    }

    #[test]
    fn parse_url_user_only() {
        let cfg = parse_clickhouse_url("http://alice@clickhouse:8123");
        assert_eq!(cfg.auth, Some(("alice".into(), "".into())));
    }

    #[test]
    fn parse_url_query_passthrough() {
        let cfg = parse_clickhouse_url("http://clickhouse:8123/?database=dogress&user=x");
        assert_eq!(cfg.host_port, "clickhouse:8123");
        assert!(cfg.auth.is_none(), "query user must NOT become Basic auth");
        assert_eq!(cfg.query_params, "database=dogress&user=x");
    }

    #[test]
    fn parse_url_userinfo_and_query() {
        let cfg = parse_clickhouse_url("http://u:p@clickhouse:8123/?database=dogress");
        assert_eq!(cfg.auth, Some(("u".into(), "p".into())));
        assert_eq!(cfg.query_params, "database=dogress");
    }

    #[test]
    fn parse_url_strips_crlf_from_credentials() {
        let cfg = parse_clickhouse_url("http://u\r\n:pa\r\nss@clickhouse:8123");
        assert_eq!(cfg.auth, Some(("u".into(), "pass".into())));
    }

    // -----------------------------------------------------------------------
    // Encoding: the SQL AND every param value travel in the query string
    // -----------------------------------------------------------------------

    #[test]
    fn url_encode_keeps_unreserved_and_escapes_everything_else() {
        assert_eq!(url_encode("abcXYZ019-_.~"), "abcXYZ019-_.~");
        assert_eq!(url_encode("a b"), "a%20b");
        assert_eq!(url_encode("a&b=c"), "a%26b%3Dc");
        assert_eq!(url_encode("a+b"), "a%2Bb");
        assert_eq!(url_encode("a%b"), "a%25b");
        assert_eq!(url_encode("'"), "%27");
        assert_eq!(url_encode("SELECT 1"), "SELECT%201");
    }

    // -----------------------------------------------------------------------
    // Status / error classification
    // -----------------------------------------------------------------------

    #[test]
    fn only_a_200_status_line_is_success() {
        assert!(is_ok_status("HTTP/1.1 200 OK\r"));
        assert!(is_ok_status("HTTP/1.1 200 OK"));
        assert!(!is_ok_status("HTTP/1.1 500 Internal Server Error\r"));
        assert!(!is_ok_status("HTTP/1.1 404 Not Found\r"));
        assert!(!is_ok_status(""));
    }

    #[test]
    fn response_body_takes_the_part_after_the_headers() {
        let resp = b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 60\r\nConnection: close\r\n\r\nCode: 60. DB::Exception: Table usage_record does not exist";
        assert_eq!(
            response_body(resp),
            "Code: 60. DB::Exception: Table usage_record does not exist"
        );
        // A response with no body is an empty string, not a panic.
        assert_eq!(response_body(b"HTTP/1.1 200 OK\r\n\r\n"), "");
    }

    /// ClickHouse frames its answers as chunks, so the bytes after the headers
    /// are `size\r\ndata\r\n…0\r\n\r\n`. Reading them undecoded is what the
    /// LIVE instance exposed; a `Content-Length` double cannot.
    #[test]
    fn a_chunked_response_is_decoded() {
        let body = "{\"requests\":\"8\"}";
        let resp = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: application/x-ndjson\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
            body.len(),
            body
        );
        assert_eq!(response_body(resp.as_bytes()), body);
    }

    /// Multiple chunks, and a chunk extension (`;name=value`), are both legal.
    #[test]
    fn multiple_chunks_and_extensions_are_decoded() {
        let resp = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4;ext=1\r\nabcd\r\n3\r\nefg\r\n0\r\n\r\n";
        assert_eq!(response_body(resp.as_bytes()), "abcdefg");
    }

    /// The read is size-capped, so a cut-off chunk must return the complete part
    /// instead of panicking — the reader then fails its own decode check.
    #[test]
    fn a_truncated_chunked_body_returns_what_was_complete() {
        let resp = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nabcd\r\n5\r\nxy";
        assert_eq!(response_body(resp.as_bytes()), "abcdxy");
    }

    /// A chunked header on a body that is not chunked must not invent data.
    #[test]
    fn a_malformed_chunk_size_yields_no_body_rather_than_a_guess() {
        // `zz` is not hex, so the decoder stops instead of guessing.
        let resp = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nhello";
        assert_eq!(response_body(resp.as_bytes()), "");
    }

    /// No header terminator at all: nothing to report rather than a panic.
    #[test]
    fn a_response_without_a_header_terminator_has_no_body() {
        assert_eq!(response_body(b"HTTP/1.1 200 OK\r\n"), "");
    }

    // -----------------------------------------------------------------------
    // Transport: real sockets, real deadlines
    // -----------------------------------------------------------------------

    fn cfg_for(addr: std::net::SocketAddr, connect_ms: u64, io_ms: u64) -> ClickHouseConfig {
        ClickHouseConfig {
            host_port: addr.to_string(),
            auth: None,
            query_params: String::new(),
            connect_timeout: Duration::from_millis(connect_ms),
            io_timeout: Duration::from_millis(io_ms),
        }
    }

    /// A TCP server that answers every connection with `response` verbatim.
    async fn spawn_responder(response: &'static str) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        addr
    }

    /// The request must actually carry what we asked for, encoded.
    #[tokio::test]
    async fn the_request_line_carries_the_encoded_query_and_params() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let _ = tx.send(String::from_utf8_lossy(&buf[..n]).to_string());
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
                let _ = sock.shutdown().await;
            }
        });
        let cfg = cfg_for(addr, 500, 500);
        let out = tokio::time::timeout(
            Duration::from_secs(5),
            send(&cfg, "SELECT {t:String}", &[("t", "a b&c")], b""),
        )
        .await
        .expect("must not hang")
        .expect("a 200 is a complete exchange");
        assert!(is_ok_status(&out.0), "{:?}", out.0);
        let request = rx.await.expect("the server recorded the request");
        assert!(
            request.starts_with("POST /?query=SELECT%20%7Bt%3AString%7D"),
            "unexpected request line: {request}"
        );
        assert!(
            request.contains("&param_t=a%20b%26c"),
            "param values must be percent-encoded too: {request}"
        );
        assert!(request.contains("Content-Length: 0"), "{request}");
    }

    /// A black-holed ClickHouse must fail on its own deadline, not hang the task.
    #[tokio::test]
    async fn a_clickhouse_that_never_answers_times_out() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            if let Ok((sock, _)) = listener.accept().await {
                // Hold the connection open and stay silent.
                tokio::time::sleep(Duration::from_secs(30)).await;
                drop(sock);
            }
        });
        let cfg = cfg_for(addr, 500, 300);
        let started = tokio::time::Instant::now();
        let out = tokio::time::timeout(Duration::from_secs(5), send(&cfg, "SELECT 1", &[], b""))
            .await
            .expect(
                "the sender must give up: one stuck flush pinned ALL usage metering on the node",
            );
        let msg = out.expect_err("a black-holed ClickHouse must not report success");
        assert!(msg.contains("timed out"), "unexpected error: {msg}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "must fail fast on its own deadline, took {:?}",
            started.elapsed()
        );
    }

    /// A non-200 is a *complete exchange*, so `send` returns it rather than
    /// erroring — the caller decides. The reader relies on this to classify a
    /// failing query from its `Code: N` body.
    #[tokio::test]
    async fn a_non_200_response_is_returned_for_the_caller_to_classify() {
        let addr = spawn_responder(
            "HTTP/1.1 404 Not Found\r\nContent-Length: 60\r\nConnection: close\r\n\r\nCode: 60. DB::Exception: Table usage_record does not exist",
        )
        .await;
        let cfg = cfg_for(addr, 500, 500);
        let (status, body) = send(&cfg, "SELECT * FROM nope", &[], b"")
            .await
            .expect("a 404 is a complete exchange");
        assert!(!is_ok_status(&status), "{status}");
        assert!(status.contains("404"), "{status}");
        // The raw body reaches the caller, which is what lets the reader (T8)
        // classify a failing query from its `Code: N` text.
        assert!(
            response_body(&body).contains("DB::Exception"),
            "error body must survive the transport"
        );
    }

    // -----------------------------------------------------------------------
    // The reader's config differs from the writer's ONLY in its deadlines
    // -----------------------------------------------------------------------
}
