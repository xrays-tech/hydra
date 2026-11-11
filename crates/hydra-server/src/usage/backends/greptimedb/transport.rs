//! GreptimeDB transport: `POST /v1/sql?db=<db>` on GreptimeDB's HTTP SQL API.
//!
//! # What this store forced (facts measured against `greptime/greptimedb:v1.3.0`, 2026-11-11)
//!
//! * **A 2xx is not automatically success.** GreptimeDB answers most failures with a non-2xx status
//!   AND `{"code":N,"error":"…"}`, but a gateway in front of it (or a version skew) can deliver that
//!   envelope on a `200`. [`GreptimedbResponse`] therefore treats EITHER shape as a failure: any
//!   non-2xx status is refused by [`send`], and a body that carries a top-level `code`/`error` is a
//!   failure the caller checks with [`GreptimedbResponse::is_failure`] — a 200 is never assumed to
//!   mean "it landed".
//! * **Requests are form-encoded, not query-encoded.** The statement travels in the body as
//!   `sql=<percent-encoded>` with `Content-Type: application/x-www-form-urlencoded` (measured);
//!   the database is the `db` query parameter. There is no `param_*` binding, so every value the
//!   reader interpolates is escaped by the caller and grouped expressions are a whitelist.
//! * **A SELECT answers `output[0].records.{schema.column_schemas,rows}`** with JSON numbers for the
//!   integer columns and the fixed-width timestamp string as a JSON string (measured);
//!   INSERT/DDL answer `output[0].affectedrows`.
//!
//! # Version
//!
//! Measured against **`greptime/greptimedb:v1.3.0`** (standalone). The canonical DDL
//! (`PRIMARY KEY (tenant_id, trace_id)` + `ts TIMESTAMP TIME INDEX` + `append_mode='false'`) is
//! ACCEPTED by that server: two identical `(tenant_id, trace_id, ts)` inserts collapse to one row
//! (retry dedup works), so no runtime DDL fallback is implemented. See `super` for the writer/reader.
//!
//! # Deadlines and the response cap
//!
//! Connect, IO and query deadlines are separate knobs (`HYDRA_GREPTIMEDB_CONNECT_TIMEOUT_MS`,
//! `HYDRA_GREPTIMEDB_IO_TIMEOUT_MS`, `HYDRA_GREPTIMEDB_QUERY_TIMEOUT_MS`), because a black-holed host
//! must fail a batch quickly while a slow aggregate read is allowed to take its own allowance — the
//! same split the ClickHouse and TDengine transports make. A SELECT that fills
//! [`MAX_GREPTIMEDB_RESPONSE`] is reported as truncated, and the reader turns that into
//! `UsageQueryError::ResultTooLarge` rather than a decode error (retrying returns the same bytes).
//!
//! # `https://` is refused
//!
//! This transport is a bare `TcpStream` with no TLS code path. A URL that promises `https://` is
//! refused by [`parse_greptimedb_url`] (at `open`, before anything is dialled) rather than silently
//! sending the Basic credentials and usage rows in the clear.

use std::time::Duration;

/// A parsed `HYDRA_GREPTIMEDB_URL`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GreptimedbConfig {
    pub host: String,
    pub port: u16,
    /// HTTP Basic credentials, if the URL carried any.
    pub user: Option<String>,
    pub password: Option<String>,
    /// Database name in the URL PATH (`http://host:4000/hydra_usage`) — sent as `?db=<db>`.
    pub database: Option<String>,
    pub connect_timeout: Duration,
    pub io_timeout: Duration,
}

impl GreptimedbConfig {
    /// The default database, or `hydra_usage` (what [`super`] creates and writes to).
    #[must_use]
    pub fn database_or_default(&self) -> &str {
        self.database.as_deref().unwrap_or(super::DEFAULT_DATABASE)
    }
}

/// The largest response body this transport will read for a SELECT. A bounded aggregate read cannot
/// need more; a proxy or a login page answering with a stream must not be buffered forever. Over the
/// cap the read stops and the response is flagged `truncated`.
///
/// This is the BODY cap: the status line and headers are measured separately (see
/// [`MAX_HEADER_BYTES`]), so a response whose body is exactly this size is not truncated by its own
/// headers.
pub const MAX_GREPTIMEDB_RESPONSE: usize = 64 * 1024;

/// The largest response HEADER BLOCK this transport will buffer before the `\r\n\r\n` terminator
/// arrives. Generous for any real HTTP response; a server that never sends one is refused rather
/// than buffered forever.
pub const MAX_HEADER_BYTES: usize = 16 * 1024;

/// Parse `HYDRA_GREPTIMEDB_URL`.
///
/// Accepted: `http://host:4000`, `http://user:pass@host:4000/hydra_usage`, with any query string
/// ignored (`db` is the PATH here, not a query parameter). A path is MEANINGFUL: `…/4000/hydra_usage`
/// names the database and `…/4000` does not.
///
/// `https://` is refused, for the same reason ClickHouse's and TDengine's are: this transport has no
/// TLS code path, and silently connecting in the clear while the URL promised TLS would leak the
/// credentials.
pub fn parse_greptimedb_url(raw: &str) -> Result<GreptimedbConfig, String> {
    let trimmed = raw.trim();
    let without_scheme = if let Some(rest) = trimmed.strip_prefix("http://") {
        rest
    } else if trimmed.starts_with("https://") {
        return Err(
            "HYDRA_GREPTIMEDB_URL uses https:// but this build has no TLS transport for GreptimeDB; \
             refusing to send credentials and usage rows in PLAINTEXT — use http:// on a private \
             network or through a tunnel, or terminate TLS in front of the database"
                .to_string(),
        );
    } else {
        return Err(format!(
            "HYDRA_GREPTIMEDB_URL must start with http:// (got {trimmed:?})"
        ));
    };

    // Split off the path FIRST: the database may legitimately be named like a query string, and the
    // authority parse below must not see it.
    let (authority, rest) = match without_scheme.find('/') {
        Some(i) => (&without_scheme[..i], &without_scheme[i + 1..]),
        None => (without_scheme, ""),
    };
    let path = rest
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .trim_end_matches('/');
    let database = if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    };

    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, authority),
    };
    let (user, password) = match userinfo {
        None => (None, None),
        Some(u) => match u.split_once(':') {
            Some((n, p)) => (Some(percent_decode(n)), Some(percent_decode(p))),
            None => (Some(percent_decode(u)), None),
        },
    };

    let (host, port) = split_host_port(hostport)?;
    Ok(GreptimedbConfig {
        host: host.to_string(),
        port,
        user,
        password,
        database,
        connect_timeout: env_millis("HYDRA_GREPTIMEDB_CONNECT_TIMEOUT_MS", 2_000),
        io_timeout: env_millis("HYDRA_GREPTIMEDB_IO_TIMEOUT_MS", 5_000),
    })
}

fn split_host_port(hostport: &str) -> Result<(&str, u16), String> {
    // Bracketed IPv6 (`[::1]`, `[::1]:4000`) must not be split on the LAST colon: `[::1]` has none
    // after the bracket but `[::1]:4000` does, and a bare `::1` would be misread as port `1`. Keep
    // the brackets in the host so `host:port` re-assembles into a connectable `[::1]:4000`.
    if let Some(rest) = hostport.strip_prefix('[') {
        let close = rest.find(']').ok_or_else(|| {
            format!(
                "HYDRA_GREPTIMEDB_URL has an unterminated IPv6 host {hostport:?} (expected `[…]`)"
            )
        })?;
        let host = &hostport[..close + 2];
        let after = &hostport[close + 2..];
        let port = match after.strip_prefix(':') {
            None if after.is_empty() => 4000,
            None => {
                return Err(format!(
                    "HYDRA_GREPTIMEDB_URL has trailing text after the IPv6 host {hostport:?} \
                     (expected `[host]` or `[host]:port`)"
                ))
            }
            Some("") => 4000,
            Some(p) => p
                .parse::<u16>()
                .map_err(|_| format!("HYDRA_GREPTIMEDB_URL has an invalid port {p:?}"))?,
        };
        return Ok((host, port));
    }

    // An unbracketed authority with more than one colon can only be an ambiguous IPv6 literal; say
    // so instead of parsing its tail as a port (`::1` would become host `::`, port `1`).
    if hostport.matches(':').count() > 1 {
        return Err(format!(
            "HYDRA_GREPTIMEDB_URL has an ambiguous IPv6 host {hostport:?}; IPv6 hosts must be \
             bracketed (`http://[::1]:4000`)"
        ));
    }

    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p),
        None => (hostport, "4000"),
    };
    if host.is_empty() {
        return Err("HYDRA_GREPTIMEDB_URL has no host".to_string());
    }
    if port.is_empty() {
        // GreptimeDB's standalone HTTP default.
        return Ok((host, 4000));
    }
    let port: u16 = port
        .parse()
        .map_err(|_| format!("HYDRA_GREPTIMEDB_URL has an invalid port {port:?}"))?;
    Ok((host, port))
}

/// `%XX` → the byte it stands for. Applied to the credentials only: a password with an `@` or `/`
/// must survive the URL, and the characters a URL cannot carry literally are exactly what this undoes.
fn percent_decode(s: &str) -> String {
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

/// A deadline knob: milliseconds from the environment, or `default_ms`.
#[must_use]
pub fn env_millis(name: &str, default_ms: u64) -> Duration {
    match std::env::var(name) {
        Ok(v) => match v.trim().parse::<u64>() {
            Ok(ms) if ms > 0 => Duration::from_millis(ms),
            _ => {
                tracing::warn!(
                    target: "hydra::usage::greptimedb",
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

/// Percent-encode a string for a `application/x-www-form-urlencoded` value (RFC 3986 unreserved
/// characters kept; everything else `%HH`).
///
/// Used for the `db` query parameter AND for the SQL body: an unencoded `&`, `+` or `%` would
/// silently change what the server receives.
#[must_use]
pub fn url_encode(s: &str) -> String {
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

/// What GreptimeDB answered, parsed from the body.
#[derive(Clone, Debug)]
pub struct GreptimedbResponse {
    /// The first line of the HTTP response, for diagnostics.
    pub status_line: String,
    /// The raw body, for diagnostics.
    pub body: String,
    /// True when the read stopped at [`MAX_GREPTIMEDB_RESPONSE`] before EOF.
    pub truncated: bool,
    /// `output[0].records.schema.column_schemas[].name`, in column order.
    pub columns: Vec<String>,
    /// `output[0].records.rows`, as raw JSON values.
    pub rows: Vec<Vec<serde_json::Value>>,
    /// `output[0].affectedrows`, for INSERT/DDL.
    pub affected_rows: Option<i64>,
    /// Set when the body itself reported failure (a top-level `code`/`error`). `None` on success.
    pub error: Option<String>,
}

impl GreptimedbResponse {
    /// Parse a GreptimeDB JSON body.
    ///
    /// A body that is not JSON is an `Err` (a proxy or a login page in the middle must not read as
    /// "no usage"). A body that IS JSON but is missing `output[...]` parses to an EMPTY response
    /// rather than an error — that is a shape the reader's own decoder must judge as `Decode`, not a
    /// transport-level failure.
    pub fn parse(body: &str) -> Result<Self, String> {
        let value: serde_json::Value = serde_json::from_str(body)
            .map_err(|e| format!("greptimedb response is not JSON ({e}): {}", preview(body)))?;
        let obj = value
            .as_object()
            .ok_or_else(|| format!("greptimedb response is not an object: {}", preview(body)))?;

        // A top-level `code`/`error` is a failure even on a 200. A successful SELECT/INSERT does
        // not carry either.
        let mut error = None;
        if let Some(code) = obj.get("code").and_then(serde_json::Value::as_i64) {
            if code != 0 {
                let text = obj
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                error = Some(if text.is_empty() {
                    format!("greptimedb refused the statement (code {code})")
                } else {
                    format!("greptimedb refused the statement (code {code}): {text}")
                });
            }
        }
        if error.is_none() {
            if let Some(text) = obj.get("error").and_then(serde_json::Value::as_str) {
                if !text.is_empty() {
                    error = Some(format!("greptimedb refused the statement: {text}"));
                }
            }
        }

        let mut columns = Vec::new();
        let mut rows = Vec::new();
        let mut affected_rows = None;
        if let Some(first) = obj
            .get("output")
            .and_then(serde_json::Value::as_array)
            .and_then(|o| o.first())
        {
            affected_rows = first
                .get("affectedrows")
                .and_then(serde_json::Value::as_i64);
            if let Some(records) = first.get("records") {
                if let Some(schemas) = records
                    .get("schema")
                    .and_then(|s| s.get("column_schemas"))
                    .and_then(serde_json::Value::as_array)
                {
                    columns = schemas
                        .iter()
                        .filter_map(|c| {
                            c.get("name")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string)
                        })
                        .collect();
                }
                if let Some(data) = records.get("rows").and_then(serde_json::Value::as_array) {
                    rows = data
                        .iter()
                        .map(|r| r.as_array().cloned().unwrap_or_default())
                        .collect();
                }
            }
        }

        Ok(Self {
            status_line: String::new(),
            body: body.to_string(),
            truncated: false,
            columns,
            rows,
            affected_rows,
            error,
        })
    }

    /// A response for a truncated read: the body could not be fully buffered, so it is not parsed.
    #[must_use]
    fn truncated(status_line: String, body: String) -> Self {
        Self {
            status_line,
            body,
            truncated: true,
            columns: Vec::new(),
            rows: Vec::new(),
            affected_rows: None,
            error: None,
        }
    }

    /// True when this response must NOT be treated as success.
    #[must_use]
    pub fn is_failure(&self) -> bool {
        self.error.is_some()
    }

    /// The failure text, naming what the server said.
    #[must_use]
    pub fn error_text(&self) -> String {
        self.error
            .clone()
            .unwrap_or_else(|| format!("greptimedb refused: {}", preview(&self.body)))
    }

    /// The column index of `name`, exact first then case-insensitively (a server or a proxy may
    /// change the case of an alias).
    #[must_use]
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c == name).or_else(|| {
            self.columns
                .iter()
                .position(|c| c.eq_ignore_ascii_case(name))
        })
    }
}

fn preview(body: &str) -> String {
    body.chars().take(160).collect()
}

/// Whether a status line means success. Any 2xx is accepted; everything else is refused by [`send`].
#[must_use]
pub fn is_ok_status(status_line: &str) -> bool {
    matches!(status_code(status_line), Some(code) if (200..300).contains(&code))
}

/// The numeric status code from a response status line, if it parses.
#[must_use]
pub fn status_code(status_line: &str) -> Option<u16> {
    status_line.split_whitespace().nth(1)?.parse().ok()
}

/// The `Authorization` header value for this configuration: HTTP Basic, or nothing when the URL
/// carried no credentials.
#[must_use]
pub fn authorization_header(cfg: &GreptimedbConfig) -> Option<String> {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;
    let user = cfg.user.as_deref()?;
    let password = cfg.password.as_deref().unwrap_or("");
    Some(format!(
        "Basic {}",
        B64.encode(format!("{user}:{password}"))
    ))
}

/// Send one statement to GreptimeDB and parse the answer.
///
/// A non-2xx status is an `Err` (the exchange completed, the server refused). A 2xx that crossed the
/// response cap returns a `truncated` response — the caller decides between `ResultTooLarge` and a
/// retry. A 2xx body carrying a top-level `code`/`error` parses to a response with
/// [`GreptimedbResponse::is_failure`] set.
pub async fn send(cfg: &GreptimedbConfig, sql: &str) -> Result<GreptimedbResponse, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let db = cfg.database_or_default();
    let body = format!("sql={}", url_encode(sql));
    let path = format!("/v1/sql?db={}", url_encode(db));
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: application/x-www-form-urlencoded\r\nX-Greptime-Timezone: UTC\r\nContent-Length: {}\r\nConnection: close\r\n",
        cfg.host,
        cfg.port,
        body.len()
    );
    if let Some(auth) = authorization_header(cfg) {
        request.push_str(&format!("Authorization: {auth}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(&body);

    let addr = format!("{}:{}", cfg.host, cfg.port);
    let mut stream =
        tokio::time::timeout(cfg.connect_timeout, tokio::net::TcpStream::connect(&addr))
            .await
            .map_err(|_| {
                format!(
                    "connecting to {addr} timed out after {:?}",
                    cfg.connect_timeout
                )
            })?
            .map_err(|e| format!("cannot connect to {addr}: {e}"))?;

    let exchange = async {
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|e| format!("writing the statement to {addr} failed: {e}"))?;
        stream.flush().await.ok();
        let mut raw = Vec::new();
        let mut buf = [0u8; 8192];
        let mut truncated = false;
        // Where the body begins (just past the header terminator), once it has been seen. The body
        // cap only starts counting from there, so `MAX_GREPTIMEDB_RESPONSE` is a BODY cap.
        let mut body_start: Option<usize> = None;
        loop {
            let n = stream
                .read(&mut buf)
                .await
                .map_err(|e| format!("reading the answer from {addr} failed: {e}"))?;
            if n == 0 {
                break;
            }
            if append_capped(&mut raw, &buf[..n], &mut body_start)? {
                truncated = true;
                break;
            }
        }
        Ok::<(Vec<u8>, bool), String>((raw, truncated))
    };

    let (raw, truncated) = tokio::time::timeout(cfg.io_timeout, exchange)
        .await
        .map_err(|_| {
            format!(
                "the answer from {addr} did not arrive within {:?}",
                cfg.io_timeout
            )
        })??;

    let text = String::from_utf8_lossy(&raw).into_owned();
    let status_line = text.lines().next().unwrap_or("").to_string();
    let resp_body = response_body(&text);

    if !is_ok_status(&status_line) {
        return Err(format!(
            "greptimedb refused the statement (status {}): {}",
            status_line.trim(),
            preview(&resp_body)
        ));
    }
    if truncated {
        return Ok(GreptimedbResponse::truncated(status_line, resp_body));
    }
    let mut parsed = GreptimedbResponse::parse(&resp_body)?;
    parsed.status_line = status_line;
    Ok(parsed)
}

/// Append a response chunk, enforcing the two caps exactly:
///
/// * until the header terminator is seen, the header block may not exceed [`MAX_HEADER_BYTES`] — a
///   server that never terminates its headers is refused with an actionable error;
/// * from the first byte after the terminator, the body may not exceed
///   [`MAX_GREPTIMEDB_RESPONSE`].
///
/// Returns `true` when body bytes were discarded because the body cap was crossed. A body that
/// exactly fills the cap is NOT truncation (the response may still be complete).
fn append_capped(
    raw: &mut Vec<u8>,
    chunk: &[u8],
    body_start: &mut Option<usize>,
) -> Result<bool, String> {
    raw.extend_from_slice(chunk);
    if body_start.is_none() {
        if let Some(h) = find_subslice(raw, b"\r\n\r\n") {
            *body_start = Some(h + 4);
        } else if raw.len() > MAX_HEADER_BYTES {
            return Err(format!(
                "the answer's headers exceeded {MAX_HEADER_BYTES} bytes without a terminator"
            ));
        }
    }
    if let Some(start) = *body_start {
        if raw.len().saturating_sub(start) > MAX_GREPTIMEDB_RESPONSE {
            raw.truncate(start + MAX_GREPTIMEDB_RESPONSE);
            return Ok(true);
        }
    }
    Ok(false)
}

/// The first index of `needle` in `hay`.
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// The body of an HTTP response, or the whole text when there is no header terminator (so a parse
/// failure can quote what actually arrived).
fn response_body(text: &str) -> String {
    match text.find("\r\n\r\n") {
        Some(i) => text[i + 4..].to_string(),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_host_gets_the_greptimedb_default_port() {
        let cfg = parse_greptimedb_url("http://greptime.example:4000").expect("parses");
        assert_eq!(cfg.host, "greptime.example");
        assert_eq!(cfg.port, 4000);
        assert_eq!(cfg.database, None);
        assert_eq!(cfg.database_or_default(), super::super::DEFAULT_DATABASE);
    }

    #[test]
    fn credentials_and_a_path_database_are_parsed() {
        let cfg =
            parse_greptimedb_url("http://user:pass@127.0.0.1:4000/hydra?x=1").expect("parses");
        assert_eq!(cfg.user.as_deref(), Some("user"));
        assert_eq!(cfg.password.as_deref(), Some("pass"));
        assert_eq!(cfg.database.as_deref(), Some("hydra"));
        assert_eq!(cfg.port, 4000);
    }

    #[test]
    fn a_percent_encoded_password_survives_the_url() {
        let cfg = parse_greptimedb_url("http://u:p%40ss%2Fword@host:4000").expect("parses");
        assert_eq!(cfg.password.as_deref(), Some("p@ss/word"));
    }

    #[test]
    fn https_is_refused_rather_than_sent_in_the_clear() {
        let err = parse_greptimedb_url("https://host:4000").unwrap_err();
        assert!(err.contains("PLAINTEXT"), "{err}");
        assert!(err.contains("https://"), "{err}");
    }

    #[test]
    fn a_url_without_a_scheme_is_refused_with_the_reason() {
        let err = parse_greptimedb_url("host:4000").unwrap_err();
        assert!(err.contains("must start with http://"), "{err}");
    }

    #[test]
    fn the_url_carries_no_port_and_gets_the_default() {
        let cfg = parse_greptimedb_url("http://greptime").expect("parses");
        assert_eq!(cfg.port, 4000);
    }

    #[test]
    fn a_bracketed_ipv6_host_is_parsed_without_a_port() {
        let cfg = parse_greptimedb_url("http://[::1]/hydra_usage").expect("parses");
        assert_eq!(
            cfg.host, "[::1]",
            "the brackets are kept so `host:port` re-assembles"
        );
        assert_eq!(cfg.port, 4000);
        assert_eq!(cfg.database.as_deref(), Some("hydra_usage"));
    }

    #[test]
    fn a_bracketed_ipv6_host_with_a_port_keeps_the_brackets() {
        let cfg = parse_greptimedb_url("http://[2001:db8::1]:4000/hydra_usage").expect("parses");
        assert_eq!(cfg.host, "[2001:db8::1]");
        assert_eq!(cfg.port, 4000);
    }

    #[test]
    fn an_unbracketed_ipv6_host_is_refused_with_an_accurate_message() {
        let err = parse_greptimedb_url("http://::1:4000").unwrap_err();
        assert!(err.contains("bracketed"), "{err}");
    }

    #[test]
    fn an_unterminated_bracketed_host_is_refused() {
        let err = parse_greptimedb_url("http://[::1").unwrap_err();
        assert!(err.contains("unterminated"), "{err}");
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
    fn only_a_2xx_status_line_is_success() {
        assert!(is_ok_status("HTTP/1.1 200 OK"));
        assert!(is_ok_status("HTTP/1.1 201 Created"));
        assert!(!is_ok_status("HTTP/1.1 400 Bad Request"));
        assert!(!is_ok_status("HTTP/1.1 401 Unauthorized"));
        assert!(!is_ok_status("HTTP/1.1 500 Internal Server Error"));
        assert!(!is_ok_status(""));
        // The old ClickHouse check matched `" 2000 "`; a real status parse must not.
        assert!(!is_ok_status("HTTP/1.1 2000 Nope"));
    }

    #[test]
    fn authorization_is_base64_of_user_and_password() {
        let cfg = parse_greptimedb_url("http://root:taosdata@h:4000").expect("parses");
        assert_eq!(
            authorization_header(&cfg).as_deref(),
            Some("Basic cm9vdDp0YW9zZGF0YQ==")
        );
        let none = parse_greptimedb_url("http://h:4000").expect("parses");
        assert_eq!(authorization_header(&none), None);
    }

    /// A SELECT body: the schema names and the rows must be reachable by index.
    #[test]
    fn a_select_body_is_parsed_by_schema_name() {
        let body = r#"{"output":[{"records":{"schema":{"column_schemas":[{"name":"requests","data_type":"UInt64"},{"name":"tokens_in","data_type":"UInt64"}]},"rows":[[8,247]],"total_rows":1}}],"execution_time_ms":3}"#;
        let resp = GreptimedbResponse::parse(body).expect("parses");
        assert!(!resp.is_failure());
        assert_eq!(resp.column_index("requests"), Some(0));
        assert_eq!(resp.column_index("tokens_in"), Some(1));
        assert_eq!(resp.rows.len(), 1);
        assert!(!resp.truncated);
    }

    /// An INSERT/DDL body carries `affectedrows`, not `records`.
    #[test]
    fn an_insert_body_carries_affected_rows() {
        let body = r#"{"output":[{"affectedrows":3}],"execution_time_ms":1}"#;
        let resp = GreptimedbResponse::parse(body).expect("parses");
        assert_eq!(resp.affected_rows, Some(3));
        assert!(!resp.is_failure());
    }

    /// The rule that makes this store safe to talk to: a 200 is not success when the body says so.
    #[test]
    fn a_2xx_body_with_a_top_level_error_is_a_failure() {
        let resp = GreptimedbResponse::parse(r#"{"code":1004,"error":"Table not found"}"#)
            .expect("parses");
        assert!(resp.is_failure());
        assert!(
            resp.error_text().contains("Table not found"),
            "{}",
            resp.error_text()
        );
        // The bare `error` spelling is the same failure.
        let resp = GreptimedbResponse::parse(r#"{"error":"boom"}"#).expect("parses");
        assert!(resp.is_failure());
    }

    /// A body that is not JSON is an error, not an empty answer.
    #[test]
    fn a_non_json_body_is_a_failure() {
        assert!(GreptimedbResponse::parse("<html>login</html>").is_err());
    }

    #[test]
    fn the_response_body_is_split_from_the_headers() {
        assert_eq!(
            response_body("HTTP/1.1 200 OK\r\n\r\n{\"code\":0}"),
            "{\"code\":0}"
        );
        assert_eq!(response_body("garbage"), "garbage");
    }

    /// The cap is a BODY cap: the status line and headers do not eat into it.
    #[test]
    fn the_body_cap_counts_only_bytes_after_the_headers() {
        let headers = "HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n";

        // Headers plus a small body is complete even though the TOTAL exceeds nothing relevant.
        let mut raw = Vec::new();
        let mut body_start = None;
        assert!(!append_capped(&mut raw, headers.as_bytes(), &mut body_start).expect("fits"));
        assert_eq!(
            body_start,
            Some(headers.len()),
            "the body starts after the terminator"
        );
        assert!(!append_capped(&mut raw, b"0123456789", &mut body_start).expect("fits"));
        assert_eq!(raw.len(), headers.len() + 10);

        // A body crossing the cap is cut at EXACTLY MAX_GREPTIMEDB_RESPONSE bytes of body.
        let mut raw = Vec::new();
        let mut body_start = None;
        append_capped(&mut raw, headers.as_bytes(), &mut body_start).expect("headers");
        let over = vec![b'a'; MAX_GREPTIMEDB_RESPONSE + 10];
        assert!(append_capped(&mut raw, &over, &mut body_start).expect("no header error"));
        assert_eq!(
            raw.len() - headers.len(),
            MAX_GREPTIMEDB_RESPONSE,
            "the header bytes must not shrink the body cap"
        );

        // Exactly filling the body cap is not truncation.
        let mut raw = Vec::new();
        let mut body_start = None;
        append_capped(&mut raw, headers.as_bytes(), &mut body_start).expect("headers");
        let exact = vec![b'a'; MAX_GREPTIMEDB_RESPONSE];
        assert!(!append_capped(&mut raw, &exact, &mut body_start).expect("fits"));
        assert_eq!(raw.len() - headers.len(), MAX_GREPTIMEDB_RESPONSE);
    }

    #[test]
    fn oversized_headers_are_refused_with_the_bound() {
        let mut raw = Vec::new();
        let mut body_start = None;
        let err = append_capped(&mut raw, &vec![b'a'; MAX_HEADER_BYTES + 1], &mut body_start)
            .expect_err("a header block this large has no terminator");
        assert!(err.contains(&MAX_HEADER_BYTES.to_string()), "{err}");
    }

    /// A real socket: the request carries the measured shape and an over-cap answer is flagged.
    #[tokio::test]
    async fn the_request_carries_the_measured_shape() {
        use tokio::io::AsyncReadExt as _;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let _ = tx.send(String::from_utf8_lossy(&buf[..n]).to_string());
                let _ = tokio::io::AsyncWriteExt::write_all(
                    &mut sock,
                    b"HTTP/1.1 200 OK\r\nContent-Length: 41\r\nConnection: close\r\n\r\n{\"output\":[{\"affectedrows\":1}],\"execution_time_ms\":0}",
                )
                .await;
                let _ = tokio::io::AsyncWriteExt::shutdown(&mut sock).await;
            }
        });
        let mut cfg = parse_greptimedb_url(&format!(
            "http://user:pw@127.0.0.1:{}/hydra_usage",
            addr.port()
        ))
        .expect("parses");
        cfg.connect_timeout = Duration::from_millis(500);
        cfg.io_timeout = Duration::from_millis(500);
        let resp = send(&cfg, "INSERT INTO usage_record VALUES (1)")
            .await
            .expect("exchange");
        assert_eq!(resp.affected_rows, Some(1));
        let request = rx.await.expect("recorded");
        assert!(
            request.starts_with("POST /v1/sql?db=hydra_usage HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(
            request.contains("Content-Type: application/x-www-form-urlencoded\r\n"),
            "{request}"
        );
        assert!(
            request.contains("X-Greptime-Timezone: UTC\r\n"),
            "{request}"
        );
        assert!(
            request.contains("Authorization: Basic dXNlcjpwdw==\r\n"),
            "{request}"
        );
        assert!(
            request.ends_with("sql=INSERT%20INTO%20usage_record%20VALUES%20%281%29"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn a_non_2xx_is_a_failure_at_the_transport() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let _ = tokio::io::AsyncWriteExt::write_all(
                    &mut sock,
                    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 30\r\nConnection: close\r\n\r\n{\"code\":10,\"error\":\"it broke\"}",
                )
                .await;
                let _ = tokio::io::AsyncWriteExt::shutdown(&mut sock).await;
            }
        });
        let cfg = GreptimedbConfig {
            host: addr.ip().to_string(),
            port: addr.port(),
            user: None,
            password: None,
            database: None,
            connect_timeout: Duration::from_millis(500),
            io_timeout: Duration::from_millis(500),
        };
        let err = send(&cfg, "SELECT 1")
            .await
            .expect_err("a 500 is a failure");
        assert!(err.contains("500"), "{err}");
        assert!(err.contains("it broke"), "{err}");
    }

    #[tokio::test]
    async fn an_over_cap_answer_is_flagged_truncated() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let body_size = MAX_GREPTIMEDB_RESPONSE + 100;
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {body_size}\r\nConnection: close\r\n\r\n"
                );
                let mut full = header.into_bytes();
                full.extend(std::iter::repeat_n(b'a', body_size));
                let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, &full).await;
                let _ = tokio::io::AsyncWriteExt::shutdown(&mut sock).await;
            }
        });
        let cfg = GreptimedbConfig {
            host: addr.ip().to_string(),
            port: addr.port(),
            user: None,
            password: None,
            database: None,
            connect_timeout: Duration::from_millis(500),
            io_timeout: Duration::from_millis(500),
        };
        let resp = send(&cfg, "SELECT * FROM usage_record")
            .await
            .expect("exchange");
        assert!(resp.truncated);
    }
}
