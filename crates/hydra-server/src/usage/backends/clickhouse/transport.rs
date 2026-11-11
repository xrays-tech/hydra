//! ClickHouse transport: URL/credential interpretation, request shape, deadlines, status
//! classification (moved verbatim from `src/clickhouse.rs`, ADR-0002 T1.3).
//!
//! The single owner of "how to talk to ClickHouse". The writer and the reader in this backend both
//! go through here; nothing else in the crate knows ClickHouse's HTTP interface.
//!
use std::time::Duration;

use crate::usage::http::{self, BasicAuth, Endpoint, Overflow, ReadLimits, Request};

/// Re-exported so `transport::env_millis` / `transport::url_encode` / `transport::response_body`
/// (used by the backend's `mod.rs` and its contract tests) keep resolving after the move to
/// `usage::http`.
pub(crate) use crate::usage::http::{env_millis, response_body, url_encode};

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
    /// The URL asked for `https://`, which this transport CANNOT provide.
    ///
    /// Recorded rather than ignored: `parse_clickhouse_url` used to strip both
    /// `http://` and `https://` alike and keep only `host:port`, so an operator who
    /// configured the documented `https://host:port` form got a PLAIN TCP
    /// connection with `Authorization: Basic <user:pass>` on the wire — a silent
    /// credential disclosure dressed up as encryption, with symptoms (connection
    /// succeeds, request rejected or times out against a TLS-only port) that point
    /// at certificates instead of at this. `send` now refuses such a config.
    pub(crate) tls_requested: bool,
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

// `env_millis` moved to `crate::usage::http::env_millis` (re-exported above) with the other two
// backends.

/// Parse a ClickHouse URL into transport + credentials. Accepted forms:
///
/// - `http://host:port` / bare `host:port` (anonymous); **`https://` is parsed but
///   then REFUSED by `send`** — see `ClickHouseConfig::tls_requested`;
/// - `http://user:pass@host:port` — userinfo becomes HTTP Basic auth;
/// - any of the above plus a query string (`?database=dogress`,
///   `?user=x&password=y`), which is passed through verbatim.
///
/// CR/LF in credentials are stripped (header-injection guard); the password is
/// otherwise sent as-is inside the Basic auth header.
pub(crate) fn parse_clickhouse_url(url: &str) -> ClickHouseConfig {
    let tls_requested = url.starts_with("https://");
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
        tls_requested,
        // Trim any PATH and a trailing '/': we only dial host:port and always POST to `/`.
        // Measured 2026-09-30: this used to be `trim_end_matches('/')`, which trimmed the
        // slash but KEPT a path — `http://host:8123/clickhouse` then failed to parse its
        // port and the sink logged `clickhouse connect 127.0.0.1:8123/clickhouse: invalid
        // port value` while the comment above claimed "and any path". Splitting on '/' makes
        // the comment true and turns a confusing failure into a working connection.
        host_port: host_port.split('/').next().unwrap_or(host_port).to_string(),
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

// `url_encode` moved to `crate::usage::http::url_encode` (re-exported above): ClickHouse is the
// backend that needs it for both the SQL and every `param_*` value, but the encoder itself is shared.

/// The result of a complete ClickHouse HTTP exchange.
#[derive(Debug)]
pub(crate) struct SendResult {
    /// The first line of the HTTP response (e.g. `HTTP/1.1 200 OK`).
    pub(crate) status_line: String,
    /// The raw response bytes (status line + headers + body).
    pub(crate) body: Vec<u8>,
    /// True when the read stopped because [`MAX_CLICKHOUSE_RESPONSE`] was
    /// reached before EOF. A truncated body is not decodable as JSON; the
    /// reader must treat this as an actionable `ResultTooLarge` rather than a
    /// generic decode failure.
    pub(crate) truncated: bool,
}

/// Send one request over ClickHouse's HTTP interface and return the response.
///
/// `query` is the SQL (INSERT or SELECT), `params` are `param_*` bindings for
/// the `{name:Type}` placeholders in it (empty for an INSERT built as a literal
/// statement), and `body` is the request payload (`FORMAT JSONEachRow` rows, or
/// empty for a SELECT).
///
/// Returns a [`SendResult`] on **any** complete HTTP response —
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
) -> Result<SendResult, String> {
    // Fail BEFORE anything is dialled or written, so the credentials in `cfg.auth`
    // have no chance to leave in plaintext. This is the only place that opens the
    // connection, so one guard covers the writer and the reader.
    if cfg.tls_requested {
        return Err(
            "HYDRA_CLICKHOUSE_URL uses https:// but this build has no TLS transport for \
             ClickHouse: refusing to send credentials and usage rows in PLAINTEXT. Use \
             http:// on a private network or through a tunnel, or terminate TLS in \
             front of the database"
                .to_string(),
        );
    }
    // POST /?<passthrough params>&query=<url-encoded SQL>[&param_k=v …] HTTP/1.1
    // The `param_*` bindings appear only when the caller passes `params`: the usage
    // WRITER passes none — its rows ride in the `FORMAT JSONEachRow` body (measured
    // 2026-09-30 against a mock ClickHouse) — while the tenant API's READER binds.
    let mut path = String::with_capacity(query.len() * 2 + 256);
    path.push_str("/?");
    if !cfg.query_params.is_empty() {
        path.push_str(&cfg.query_params);
        path.push('&');
    }
    path.push_str("query=");
    path.push_str(&url_encode(query));
    for (k, v) in params {
        path.push_str("&param_");
        path.push_str(&url_encode(k));
        path.push('=');
        path.push_str(&url_encode(v));
    }

    let endpoint = endpoint_for(cfg)?;
    let auth = cfg.auth.as_ref().map(|(user, pass)| BasicAuth {
        user: user.clone(),
        pass: pass.clone(),
    });
    let request = Request {
        path_and_query: &path,
        host_header: &cfg.host_port,
        auth,
        content_type: None,
        extra_headers: &[],
        body,
    };
    // ClickHouse's cap is a TOTAL cap: the whole response may not exceed 64 KiB, and crossing it is
    // a truncated answer the reader reports as `ResultTooLarge`.
    let limits = ReadLimits {
        max_header_bytes: MAX_CLICKHOUSE_RESPONSE,
        max_body_bytes: None,
        overflow: Overflow::FlagTruncated,
    };
    let resp = http::exchange(&endpoint, &request, &limits).await?;
    Ok(SendResult {
        status_line: resp.status_line,
        body: resp.body,
        truncated: resp.truncated,
    })
}

/// `host:port` → an [`Endpoint`]. `parse_clickhouse_url` deliberately keeps the authority opaque, so
/// the split the shared transport needs happens here.
fn endpoint_for(cfg: &ClickHouseConfig) -> Result<Endpoint, String> {
    let (host, port) = cfg
        .host_port
        .rsplit_once(':')
        .ok_or_else(|| format!("clickhouse address {:?} has no port", cfg.host_port))?;
    let port: u16 = port
        .parse()
        .map_err(|_| format!("clickhouse address {:?} has an invalid port", cfg.host_port))?;
    Ok(Endpoint {
        host: host.to_string(),
        port,
        connect_timeout: cfg.connect_timeout,
        io_timeout: cfg.io_timeout,
    })
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

// The HTTP body of a response, with `Transfer-Encoding: chunked` decoded and trimmed, moved to
// `crate::usage::http::response_body` (re-exported above): ClickHouse's chunked framing is the
// reason the decoder exists, but it is shared with the other backends.
//
// `fill_capped`, `find_subslice` and `dechunk` moved to `crate::usage::http`, where the shared read
// loop (`ReadLimits`) and `response_body` own the framing and the caps.

#[cfg(feature = "usage-clickhouse")]
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    fn parse_url_trims_a_path() {
        // Measured 2026-09-30: `http://host:8123/clickhouse` used to KEEP the path, so the
        // port parse failed and the sink logged `invalid port value` — while the code
        // comment claimed paths were trimmed. The sink always POSTs to `/`, so a path can
        // only be noise.
        for url in [
            "http://clickhouse:8123/clickhouse",
            "http://u:p@clickhouse:8123/clickhouse",
            "http://clickhouse:8123/clickhouse/",
        ] {
            assert_eq!(
                parse_clickhouse_url(url).host_port,
                "clickhouse:8123",
                "{url}"
            );
        }
        // ...and the documented forms keep working unchanged.
        assert_eq!(
            parse_clickhouse_url("http://clickhouse:8123").host_port,
            "clickhouse:8123"
        );
        assert_eq!(
            parse_clickhouse_url("http://clickhouse:8123/").host_port,
            "clickhouse:8123"
        );
        assert_eq!(
            parse_clickhouse_url("http://u:p@clickhouse:8123/?database=d").host_port,
            "clickhouse:8123"
        );
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
    // Status / error classification (ClickHouse's OWN success policy)
    // -----------------------------------------------------------------------

    #[test]
    fn only_a_200_status_line_is_success() {
        assert!(is_ok_status("HTTP/1.1 200 OK\r"));
        assert!(is_ok_status("HTTP/1.1 200 OK"));
        assert!(!is_ok_status("HTTP/1.1 500 Internal Server Error\r"));
        assert!(!is_ok_status("HTTP/1.1 404 Not Found\r"));
        assert!(!is_ok_status(""));
    }

    // `url_encode`, `response_body`, `dechunk` and their unit tests live in `crate::usage::http`;
    // the transport-specific wire tests below are what stays here.

    // -----------------------------------------------------------------------
    // Transport: real sockets, real deadlines
    // -----------------------------------------------------------------------

    /// `https://` must NOT be silently downgraded to a plaintext connection.
    ///
    /// This parser stripped `http://` and `https://` alike and kept only
    /// `host:port`, and the transport is a bare `TcpStream` with
    /// `Authorization: Basic <user:pass>` in the request — so an operator who used
    /// the documented `https://host:port` form sent credentials IN THE CLEAR while
    /// believing the connection was encrypted, and the failure mode on a TLS-only
    /// port (connects, then the request is rejected or times out) pointed at
    /// certificates rather than at this.
    ///
    /// Falsification: delete the `tls_requested` guard in `send` and the call does
    /// not fail with OUR message — it tries to dial and comes back with a network
    /// error instead.
    #[tokio::test]
    async fn an_https_url_is_refused_rather_than_sent_in_plaintext() {
        let cfg = parse_clickhouse_url("https://alice:secret@ch.example.com:8443/?database=usage");
        assert!(
            cfg.tls_requested,
            "the https scheme must be REMEMBERED, not silently stripped"
        );
        assert_eq!(cfg.host_port, "ch.example.com:8443");
        assert_eq!(
            cfg.auth,
            Some(("alice".to_string(), "secret".to_string())),
            "the credentials are still parsed — which is exactly why the transport must refuse"
        );

        // Refused BEFORE anything is dialled: no DNS lookup, no socket, so the
        // password cannot leave even if the host resolves.
        match send(&cfg, "SELECT 1", &[], b"").await {
            Ok(_) => panic!("an https:// ClickHouse config must never be sent in plaintext"),
            Err(err) => {
                assert!(
                    err.contains("https://") && err.contains("PLAINTEXT"),
                    "the refusal must name the cause and the risk, got: {err}"
                );
            }
        }

        // A plaintext URL is unaffected: this is a scheme check, not a new ban.
        let plain = parse_clickhouse_url("http://127.0.0.1:8123");
        assert!(!plain.tls_requested);
        assert_eq!(plain.host_port, "127.0.0.1:8123");
    }

    fn cfg_for(addr: std::net::SocketAddr, connect_ms: u64, io_ms: u64) -> ClickHouseConfig {
        ClickHouseConfig {
            host_port: addr.to_string(),
            auth: None,
            query_params: String::new(),
            tls_requested: false,
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
        assert!(is_ok_status(&out.status_line), "{:?}", out.status_line);
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
        let result = send(&cfg, "SELECT * FROM nope", &[], b"")
            .await
            .expect("a 404 is a complete exchange");
        assert!(
            !is_ok_status(&result.status_line),
            "{:?}",
            result.status_line
        );
        assert!(
            result.status_line.contains("404"),
            "{:?}",
            result.status_line
        );
        // The raw body reaches the caller, which is what lets the reader (T8)
        // classify a failing query from its `Code: N` text.
        assert!(
            response_body(&result.body).contains("DB::Exception"),
            "error body must survive the transport"
        );
    }

    // -----------------------------------------------------------------------
    // The reader's config differs from the writer's ONLY in its deadlines
    // -----------------------------------------------------------------------

    /// A response that exceeds [`MAX_CLICKHOUSE_RESPONSE`] must be flagged as
    /// truncated so the reader can distinguish "the result is too large to
    /// decode" from "the store answered garbage".
    #[tokio::test]
    async fn an_over_cap_response_is_flagged_truncated() {
        // Build a response whose total size exceeds the cap.
        let body_size = MAX_CLICKHOUSE_RESPONSE + 100;
        let header =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {body_size}\r\nConnection: close\r\n\r\n");
        let body = vec![b'a'; body_size];
        let mut full_response = Vec::with_capacity(header.len() + body_size);
        full_response.extend_from_slice(header.as_bytes());
        full_response.extend_from_slice(&body);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let _ = sock.write_all(&full_response).await;
                let _ = sock.shutdown().await;
            }
        });

        let cfg = cfg_for(addr, 500, 500);
        let result = send(&cfg, "SELECT 1", &[], b"")
            .await
            .expect("a complete exchange");
        assert!(
            result.truncated,
            "a response over the cap must be flagged truncated"
        );
        assert_eq!(
            result.body.len(),
            MAX_CLICKHOUSE_RESPONSE,
            "the buffer must be exactly at the cap"
        );
    }

    /// A response within the cap must NOT be flagged truncated.
    #[tokio::test]
    async fn a_within_cap_response_is_not_flagged_truncated() {
        let addr =
            spawn_responder("HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await;
        let cfg = cfg_for(addr, 500, 500);
        let result = send(&cfg, "SELECT 1", &[], b"").await.expect("exchange");
        assert!(
            !result.truncated,
            "a small response must not be flagged truncated"
        );
    }
}
