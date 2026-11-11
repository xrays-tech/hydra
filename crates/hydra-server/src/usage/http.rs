//! The shared usage-backend **HTTP transport**: framing, auth header construction, the deadline-bound
//! read loop, and the response-size limits. It is the single owner of "how a usage HTTP request is
//! written and a response is bounded", so each backend's `transport.rs` keeps ONLY what is specific
//! to its store (URL text, envelope shape, success policy).
//!
//! This module deliberately does **not** own success classification: ClickHouse's loose
//! `" 200 "` substring test, TDengine's "ignore the status, the envelope decides", and GreptimeDB's
//! true-2xx gate are three different policies and stay where they are (see each backend). `exchange`
//! only reports the parsed [`Response::status`]; the caller decides what it means.
//!
//! # Limits, and the two overflow policies
//!
//! [`ReadLimits`] carries both the caps and what crossing them MEANS, because the three stores disagree
//! and that disagreement is measured:
//!
//! * ClickHouse and GreptimeDB read a bounded aggregate answer and **flag** an over-cap read as
//!   `truncated` (the reader turns that into `ResultTooLarge` — retrying returns the same bytes);
//! * TDengine **refuses** an over-cap answer with an `Err` (its 4 MiB bound is a hard safety net).
//!
//! A split limit (`max_body_bytes: Some(_)`) keeps the header block and the body on separate budgets,
//! so the body cap is the **body** cap and the status line/headers cannot shrink it — the accounting
//! GreptimeDB uses after this stage-1 split (its original implementation used a total cap; the body
//! limit is therefore slightly larger by Δ=header bytes). A total limit (`max_body_bytes: None`)
//! caps the whole response at `max_header_bytes`; that is what ClickHouse and TDengine have always
//! used.
//!
//! The `Authorization` base64 is written out here (standard alphabet, padded) rather than pulled from
//! a crate: it is the one dependency-free helper the TDengine backend used to own, now shared by all
//! three, which is what lets `usage-clickhouse`/`usage-greptimedb` stop declaring `base64`.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Where a request goes: the host/port and the two deadlines.
pub(crate) struct Endpoint {
    pub host: String,
    pub port: u16,
    pub connect_timeout: Duration,
    pub io_timeout: Duration,
}

impl Endpoint {
    /// `host:port`, bracketed-host-safe (an IPv6 host is stored with its brackets).
    #[must_use]
    pub(crate) fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// HTTP Basic credentials to send as `Authorization`.
pub(crate) struct BasicAuth {
    pub user: String,
    pub pass: String,
}

/// One HTTP request: the request-target, the `Host` header, optional credentials, and the body.
pub(crate) struct Request<'a> {
    pub path_and_query: &'a str,
    pub host_header: &'a str,
    pub auth: Option<BasicAuth>,
    pub content_type: Option<&'a str>,
    pub extra_headers: &'a [(&'a str, &'a str)],
    pub body: &'a [u8],
}

/// What crossing a [`ReadLimits`] cap does.
pub(crate) enum Overflow {
    /// Stop at the cap and report `truncated` (the response is a too-big answer, not garbage).
    FlagTruncated,
    /// Refuse the exchange with an `Err` (the cap is a hard safety net).
    Error,
}

/// The read budget for one response.
pub(crate) struct ReadLimits {
    /// The header-block cap. In split mode (`max_body_bytes: Some`) this bounds only the status
    /// line + headers, and exceeding it is a structural error. In total mode (`max_body_bytes: None`)
    /// this bounds the WHOLE response, subject to `overflow`.
    pub max_header_bytes: usize,
    /// `Some(n)` splits the budget: the body (everything after the header terminator) is capped at
    /// `n`. `None` makes `max_header_bytes` a total cap.
    pub max_body_bytes: Option<usize>,
    /// What to do when a cap is crossed.
    pub overflow: Overflow,
}

/// A completed (or truncated) HTTP exchange.
#[derive(Debug)]
pub(crate) struct Response {
    /// The first line of the response.
    pub status_line: String,
    /// The parsed numeric status, when the status line carried one.
    pub status: Option<u16>,
    /// The raw response bytes (status line + headers + body), so each backend can decode its own
    /// framing via [`response_body`].
    pub body: Vec<u8>,
    /// True when the read stopped at the body cap with bytes discarded.
    pub truncated: bool,
}

impl ReadLimits {
    /// Append `chunk` to `raw`, enforcing the caps. Returns `true` when the body cap cut bytes off.
    fn enforce(&self, raw: &mut Vec<u8>, chunk: &[u8]) -> Result<bool, String> {
        match self.max_body_bytes {
            None => {
                // Total cap.
                raw.extend_from_slice(chunk);
                if raw.len() > self.max_header_bytes {
                    match self.overflow {
                        Overflow::FlagTruncated => {
                            raw.truncate(self.max_header_bytes);
                            Ok(true)
                        }
                        Overflow::Error => Err(format!(
                            "the answer exceeded {} bytes; refusing to buffer more",
                            self.max_header_bytes
                        )),
                    }
                } else {
                    Ok(false)
                }
            }
            Some(max_body) => {
                raw.extend_from_slice(chunk);
                let body_start = match find_subslice(raw, b"\r\n\r\n") {
                    Some(h) => h + 4,
                    None => {
                        // Still in the headers. A header block this large without a terminator is a
                        // structural failure, not a truncated body — say so.
                        if raw.len() > self.max_header_bytes {
                            return Err(format!(
                                "the answer's headers exceeded {} bytes without a terminator",
                                self.max_header_bytes
                            ));
                        }
                        return Ok(false);
                    }
                };
                if body_start > self.max_header_bytes {
                    return Err(format!(
                        "the answer's headers exceeded {} bytes",
                        self.max_header_bytes
                    ));
                }
                if raw.len().saturating_sub(body_start) > max_body {
                    match self.overflow {
                        Overflow::FlagTruncated => {
                            raw.truncate(body_start + max_body);
                            Ok(true)
                        }
                        Overflow::Error => Err(format!(
                            "the answer's body exceeded {max_body} bytes; refusing to buffer more"
                        )),
                    }
                } else {
                    Ok(false)
                }
            }
        }
    }
}

/// Write one request to `ep` and read the answer, bounded by `lim`.
///
/// Every step is deadline-bound: the writer runs on the single sink flush task, so one blocked await
/// here would stop ALL usage metering for the node and silently overflow the channel behind it.
pub(crate) async fn exchange(
    ep: &Endpoint,
    req: &Request<'_>,
    lim: &ReadLimits,
) -> Result<Response, String> {
    let addr = ep.addr();
    let mut stream =
        tokio::time::timeout(ep.connect_timeout, tokio::net::TcpStream::connect(&addr))
            .await
            .map_err(|_| {
                format!(
                    "connecting to {addr} timed out after {:?}",
                    ep.connect_timeout
                )
            })?
            .map_err(|e| format!("cannot connect to {addr}: {e}"))?;

    let body = build_request(req);
    tokio::time::timeout(ep.io_timeout, stream.write_all(&body))
        .await
        .map_err(|_| {
            format!(
                "writing the request to {addr} timed out after {:?}",
                ep.io_timeout
            )
        })?
        .map_err(|e| format!("writing the request to {addr} failed: {e}"))?;
    tokio::time::timeout(ep.io_timeout, stream.flush())
        .await
        .map_err(|_| {
            format!(
                "flushing the request to {addr} timed out after {:?}",
                ep.io_timeout
            )
        })?
        .map_err(|e| format!("flushing the request to {addr} failed: {e}"))?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = tokio::time::timeout(ep.io_timeout, stream.read(&mut buf))
            .await
            .map_err(|_| {
                format!(
                    "reading the answer from {addr} timed out after {:?}",
                    ep.io_timeout
                )
            })?
            .map_err(|e| format!("reading the answer from {addr} failed: {e}"))?;
        if n == 0 {
            break;
        }
        if lim.enforce(&mut raw, &buf[..n])? {
            truncated = true;
            break;
        }
        if body_complete(&raw) {
            break;
        }
    }

    let status_line = String::from_utf8_lossy(&raw)
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    let status = status_code(&status_line);
    Ok(Response {
        status_line,
        status,
        body: raw,
        truncated,
    })
}

/// The bytes of one HTTP/1.1 request: request line, headers, blank line, body.
fn build_request(req: &Request<'_>) -> Vec<u8> {
    let mut head = String::with_capacity(req.body.len() + 256);
    head.push_str("POST ");
    head.push_str(req.path_and_query);
    head.push_str(" HTTP/1.1\r\nHost: ");
    head.push_str(req.host_header);
    head.push_str("\r\n");
    if let Some(ct) = req.content_type {
        head.push_str("Content-Type: ");
        head.push_str(ct);
        head.push_str("\r\n");
    }
    if let Some(auth) = basic_auth_header(req.auth.as_ref()) {
        head.push_str("Authorization: ");
        head.push_str(&auth);
        head.push_str("\r\n");
    }
    for (k, v) in req.extra_headers {
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
    head.push_str("Content-Length: ");
    head.push_str(&req.body.len().to_string());
    head.push_str("\r\nConnection: close\r\n\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(req.body);
    out
}

/// `true` once the headers are in and the body is complete — its `Content-Length` bytes have arrived,
/// or its chunked framing has reached the terminating `0` chunk.
///
/// A response with neither (an EOF-delimited body) reports `false`, and the read loop runs until the
/// server closes.
fn body_complete(raw: &[u8]) -> bool {
    let Some(head) = find_subslice(raw, b"\r\n\r\n") else {
        return false;
    };
    let headers = String::from_utf8_lossy(&raw[..head]).to_ascii_lowercase();
    let body = &raw[head + 4..];
    if headers.lines().any(|l| {
        let l = l.trim();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    }) {
        return chunked_complete(body);
    }
    match headers
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        Some(len) => body.len() >= len,
        None => false,
    }
}

/// `true` when a chunked body has reached its zero-size terminator.
fn chunked_complete(body: &[u8]) -> bool {
    let mut rest = body;
    loop {
        let Some(pos) = find_subslice(rest, b"\r\n") else {
            return false;
        };
        let size_line = &rest[..pos];
        let hex = size_line.split(|b| *b == b';').next().unwrap_or(b"");
        let Ok(hex) = std::str::from_utf8(hex) else {
            return false;
        };
        let Ok(n) = usize::from_str_radix(hex.trim(), 16) else {
            return false;
        };
        if n == 0 {
            return true;
        }
        let start = pos + 2;
        let end = start.saturating_add(n);
        if rest.len() < end {
            return false;
        }
        rest = &rest[end..];
        if rest.starts_with(b"\r\n") {
            rest = &rest[2..];
        }
    }
}

/// Read `KEY` as a positive millisecond count, falling back to `default_ms`.
#[must_use]
pub(crate) fn env_millis(name: &str, default_ms: u64) -> Duration {
    match std::env::var(name) {
        Ok(v) => match v.trim().parse::<u64>() {
            Ok(ms) if ms > 0 => Duration::from_millis(ms),
            _ => {
                tracing::warn!(
                    target: "hydra::usage::http",
                    variable = name,
                    value = %v,
                    "not a positive number of milliseconds; using the default"
                );
                Duration::from_millis(default_ms)
            }
        },
        Err(_) => Duration::from_millis(default_ms),
    }
}

/// `%XX` → the byte it stands for, for URL userinfo.
pub(crate) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode a string for a URL/query value or a form field (RFC 3986 unreserved characters
/// kept; everything else `%HH`).
#[must_use]
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

/// The `Authorization` header value for `auth`: HTTP Basic, or nothing when there are no credentials.
#[must_use]
pub(crate) fn basic_auth_header(auth: Option<&BasicAuth>) -> Option<String> {
    let auth = auth?;
    Some(format!(
        "Basic {}",
        base64_encode(format!("{}:{}", auth.user, auth.pass).as_bytes())
    ))
}

/// Minimal base64 (standard alphabet, padded). Written out rather than pulled in: the usage backends
/// need nothing but this from a base64 crate, and a dependency-free helper keeps a usage feature from
/// forcing one on.
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// The HTTP body of a response, with `Transfer-Encoding: chunked` **decoded**, trimmed.
///
/// The bytes after the header block of a chunked response begin with a hex chunk size and end with a
/// `0` chunk; reading them undecoded yields `"7C\r\n{...}"`, which a JSON parser rejects. Truncation
/// is tolerated: a cut-off body returns whatever chunk data was complete, and a caller that needs a
/// valid body fails its own decode check rather than acting on a partial one.
#[must_use]
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

/// Decode HTTP/1.1 chunked framing. Stops at the terminating `0` chunk, and on anything malformed or
/// truncated returns what was decoded so far.
#[must_use]
pub(crate) fn dechunk(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut rest = body;
    while let Some(pos) = find_subslice(rest, b"\r\n") {
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

/// The first index of `needle` in `hay`.
#[must_use]
pub(crate) fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// The numeric status code from a response status line, if it parses.
///
/// Shared so a backend that needs a real 2xx gate can express it; the classification itself is NOT
/// shared (each store has its own success policy).
#[must_use]
pub(crate) fn status_code(status_line: &str) -> Option<u16> {
    status_line.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_reference_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn basic_auth_is_base64_of_user_and_password() {
        let auth = BasicAuth {
            user: "root".into(),
            pass: "taosdata".into(),
        };
        assert_eq!(
            basic_auth_header(Some(&auth)).as_deref(),
            Some("Basic cm9vdDp0YW9zZGF0YQ==")
        );
        assert_eq!(basic_auth_header(None), None);
    }

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

    #[test]
    fn percent_decode_round_trips_the_characters_a_url_cannot_carry() {
        assert_eq!(percent_decode("p%40ss%2Fword"), "p@ss/word");
        // A bad or incomplete escape is left as written, never mis-decoded.
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%4"), "%4");
        assert_eq!(percent_decode("plain"), "plain");
    }

    #[test]
    fn status_code_parses_only_a_real_status() {
        assert_eq!(status_code("HTTP/1.1 200 OK"), Some(200));
        assert_eq!(status_code("HTTP/1.1 404 Not Found"), Some(404));
        assert_eq!(status_code("HTTP/1.1 2000 Nope"), Some(2000));
        assert_eq!(status_code(""), None);
    }

    #[test]
    fn response_body_takes_the_part_after_the_headers() {
        let resp = b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 60\r\nConnection: close\r\n\r\nCode: 60. DB::Exception: Table usage_record does not exist";
        assert_eq!(
            response_body(resp),
            "Code: 60. DB::Exception: Table usage_record does not exist"
        );
        assert_eq!(response_body(b"HTTP/1.1 200 OK\r\n\r\n"), "");
        // No header terminator: nothing to report rather than a panic.
        assert_eq!(response_body(b"HTTP/1.1 200 OK\r\n"), "");
    }

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

    #[test]
    fn multiple_chunks_and_extensions_are_decoded() {
        let resp = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4;ext=1\r\nabcd\r\n3\r\nefg\r\n0\r\n\r\n";
        assert_eq!(response_body(resp.as_bytes()), "abcdefg");
    }

    #[test]
    fn a_truncated_chunked_body_returns_what_was_complete() {
        let resp = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nabcd\r\n5\r\nxy";
        assert_eq!(response_body(resp.as_bytes()), "abcdxy");
    }

    #[test]
    fn a_malformed_chunk_size_yields_no_body_rather_than_a_guess() {
        let resp = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nhello";
        assert_eq!(response_body(resp.as_bytes()), "");
    }

    /// A chunked terminator must not be guessed at from a body that merely ends in `0`.
    #[test]
    fn chunked_completeness_asks_the_framing_not_the_last_byte() {
        let half = b"b\r\n{\"rows\":10}\r\n";
        assert!(!chunked_complete(half), "the `0` chunk is missing");
        let done = b"b\r\n{\"rows\":10}\r\n0\r\n\r\n";
        assert!(chunked_complete(done));
    }

    // -----------------------------------------------------------------------
    // The read budget: split vs total, and each overflow policy
    // -----------------------------------------------------------------------

    fn split(max_header: usize, max_body: usize, overflow: Overflow) -> ReadLimits {
        ReadLimits {
            max_header_bytes: max_header,
            max_body_bytes: Some(max_body),
            overflow,
        }
    }

    fn total(cap: usize, overflow: Overflow) -> ReadLimits {
        ReadLimits {
            max_header_bytes: cap,
            max_body_bytes: None,
            overflow,
        }
    }

    /// A split limit's body cap is a BODY cap: headers do not eat into it.
    #[test]
    fn a_split_body_cap_is_not_shrunk_by_the_headers() {
        let headers = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n";
        let lim = split(16 * 1024, 10, Overflow::FlagTruncated);
        let mut raw = Vec::new();
        assert!(!lim.enforce(&mut raw, headers).expect("fits"));
        assert!(!lim.enforce(&mut raw, b"0123456789").expect("fits"));
        assert_eq!(raw.len(), headers.len() + 10);

        // Crossing the BODY cap is truncation at exactly the body cap.
        let mut raw = Vec::new();
        lim.enforce(&mut raw, headers).expect("headers");
        assert!(lim.enforce(&mut raw, &[b'a'; 20]).expect("no error"));
        assert_eq!(raw.len() - headers.len(), 10);
    }

    /// Oversized headers in split mode are a structural error, not a truncated body.
    #[test]
    fn oversized_headers_in_split_mode_are_refused() {
        let lim = split(8, 64 * 1024, Overflow::FlagTruncated);
        let err = lim
            .enforce(&mut Vec::new(), b"HTTP/1.1 200 OK\r\nX: y\r\n\r\n")
            .expect_err("no terminator within the header budget");
        assert!(err.contains("headers exceeded 8 bytes"), "{err}");
    }

    /// The total cap flags truncation exactly at the cap for a flagging overflow.
    #[test]
    fn a_total_cap_flags_truncation() {
        let lim = total(10, Overflow::FlagTruncated);
        let mut raw = Vec::new();
        assert!(!lim.enforce(&mut raw, b"0123456789").expect("fits"));
        assert!(lim.enforce(&mut raw, b"abc").expect("crosses"));
        assert_eq!(raw.len(), 10);
    }

    /// The total cap with the `Error` policy refuses the exchange.
    #[test]
    fn a_total_cap_with_error_overflow_refuses() {
        let lim = total(10, Overflow::Error);
        let mut raw = Vec::new();
        lim.enforce(&mut raw, b"0123456789").expect("fits");
        let err = lim
            .enforce(&mut raw, b"abc")
            .expect_err("crossing a hard cap is an error");
        assert!(err.contains("exceeded 10 bytes"), "{err}");
    }

    // -----------------------------------------------------------------------
    // A real socket: the request bytes and the cap behavior
    // -----------------------------------------------------------------------

    async fn spawn_responder(response: Vec<u8>) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let _ = sock.write_all(&response).await;
                let _ = sock.shutdown().await;
            }
        });
        addr
    }

    fn ep_for(addr: std::net::SocketAddr) -> Endpoint {
        Endpoint {
            host: addr.ip().to_string(),
            port: addr.port(),
            connect_timeout: Duration::from_millis(500),
            io_timeout: Duration::from_millis(500),
        }
    }

    #[tokio::test]
    async fn the_request_carries_the_headers_and_body_we_built() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = tokio::sync::oneshot::channel::<Vec<u8>>();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let _ = tx.send(buf[..n].to_vec());
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
                let _ = sock.shutdown().await;
            }
        });
        let req = Request {
            path_and_query: "/v1/sql?db=hydra_usage",
            host_header: "127.0.0.1:4000",
            auth: Some(BasicAuth {
                user: "user".into(),
                pass: "pw".into(),
            }),
            content_type: Some("application/x-www-form-urlencoded"),
            extra_headers: &[("X-Greptime-Timezone", "UTC")],
            body: b"sql=SELECT%201",
        };
        let ep = ep_for(addr);
        let lim = total(64 * 1024, Overflow::FlagTruncated);
        let resp = exchange(&ep, &req, &lim).await.expect("exchange");
        assert_eq!(resp.status, Some(200));
        let sent = String::from_utf8(rx.await.expect("recorded")).expect("utf8");
        assert!(
            sent.starts_with("POST /v1/sql?db=hydra_usage HTTP/1.1\r\nHost: 127.0.0.1:4000\r\n")
        );
        assert!(sent.contains("Content-Type: application/x-www-form-urlencoded\r\n"));
        assert!(sent.contains("Authorization: Basic dXNlcjpwdw==\r\n"));
        assert!(sent.contains("X-Greptime-Timezone: UTC\r\n"));
        assert!(sent.ends_with("sql=SELECT%201"));
        assert!(sent.contains("Content-Length: 14\r\n"));
    }

    #[tokio::test]
    async fn an_over_cap_total_response_is_flagged_truncated() {
        let cap = 64 * 1024;
        let body_size = cap + 100;
        let header =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {body_size}\r\nConnection: close\r\n\r\n");
        let mut full = header.into_bytes();
        full.extend(std::iter::repeat_n(b'a', body_size));
        let addr = spawn_responder(full).await;
        let ep = ep_for(addr);
        let lim = total(cap, Overflow::FlagTruncated);
        let req = Request {
            path_and_query: "/",
            host_header: "h",
            auth: None,
            content_type: None,
            extra_headers: &[],
            body: b"",
        };
        let resp = exchange(&ep, &req, &lim).await.expect("exchange");
        assert!(resp.truncated);
        assert_eq!(resp.body.len(), cap);
    }

    #[tokio::test]
    async fn a_total_cap_with_error_overflow_turns_the_exchange_into_an_err() {
        let cap = 8;
        let addr =
            spawn_responder(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nabcdefghij".to_vec())
                .await;
        let ep = ep_for(addr);
        let lim = total(cap, Overflow::Error);
        let req = Request {
            path_and_query: "/",
            host_header: "h",
            auth: None,
            content_type: None,
            extra_headers: &[],
            body: b"",
        };
        let err = exchange(&ep, &req, &lim)
            .await
            .expect_err("a hard cap refuses");
        assert!(err.contains("exceeded 8 bytes"), "{err}");
    }

    #[tokio::test]
    async fn a_black_holed_host_fails_on_its_own_deadline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            if let Ok((sock, _)) = listener.accept().await {
                tokio::time::sleep(Duration::from_secs(30)).await;
                drop(sock);
            }
        });
        let ep = Endpoint {
            host: addr.ip().to_string(),
            port: addr.port(),
            connect_timeout: Duration::from_millis(500),
            io_timeout: Duration::from_millis(300),
        };
        let lim = total(64 * 1024, Overflow::FlagTruncated);
        let req = Request {
            path_and_query: "/",
            host_header: "h",
            auth: None,
            content_type: None,
            extra_headers: &[],
            body: b"",
        };
        let started = tokio::time::Instant::now();
        let err = tokio::time::timeout(Duration::from_secs(5), exchange(&ep, &req, &lim))
            .await
            .expect("must give up")
            .expect_err("a silent host must not report success");
        assert!(err.contains("timed out"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "must fail fast, took {:?}",
            started.elapsed()
        );
    }
}
