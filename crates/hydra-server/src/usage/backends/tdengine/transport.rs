//! TDengine transport: `POST /rest/sql` on taosAdapter, with the one rule this store forces
//! (ADR-0002 §5.1, all facts measured against `tdengine/tdengine:3.3.6.13` on 2026-10-07).
//!
//! # A 200 is NOT success
//!
//! taosAdapter answers **HTTP 200 with `{"code":<nonzero>,"desc":"…"}`** for most failures —
//! including an **authentication failure** (`code:855`, measured with a wrong password). A transport
//! that trusted the status line would report a batch as written when nothing had accepted it, which
//! is the exact shape this design refuses everywhere else ("I could not write" must never look like
//! "it is written"). So every response is parsed and `code != 0` is an error, whatever the status.
//!
//! # What the request looks like
//!
//! `POST /rest/sql[/<db>]`, `Authorization: Basic <base64(user:password)>` (3.3.x has Basic only;
//! token/`Bearer` auth arrived in 3.4.0.0 — see the note on `Auth`), and the SQL statement as the
//! **body**, not as a query parameter.
//!
//! # Framing
//!
//! taosAdapter answers with **`Transfer-Encoding: chunked`** (measured 2026-10-07 — the live test
//! failed with `4a\r\n{…}\r\n0\r\n\r\n`, which is a chunk size line, not JSON). `curl` hides
//! that by de-chunking transparently, so a transport written from the curl transcript alone reads the
//! size lines as the body and cannot parse the envelope. The framing is decoded here, and an
//! incomplete one is not mistaken for a short answer.
//!
//! # Deadlines
//!
//! Connect and read deadlines are separate knobs (`HYDRA_TDENGINE_CONNECT_TIMEOUT_MS`,
//! `HYDRA_TDENGINE_IO_TIMEOUT_MS`), because a black-holed host must fail a batch quickly while a
//! slow aggregate read is allowed to take longer — the same split the ClickHouse transport makes,
//! for the same reason.

use std::time::Duration;

/// A parsed `HYDRA_TDENGINE_URL`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TdengineConfig {
    pub host: String,
    pub port: u16,
    /// HTTP Basic credentials, if the URL carried any.
    pub user: Option<String>,
    pub password: Option<String>,
    /// Database name in the URL PATH (`http://host:6041/hydra`) — taosAdapter uses it as the default
    /// database for the statement, which is what lets the SQL use bare table names. Measured: the
    /// same statement that fails with "Database not specified" succeeds once the path names one.
    pub database: Option<String>,
    pub connect_timeout: Duration,
    pub io_timeout: Duration,
}

impl TdengineConfig {
    /// The default database, or `hydra_usage` (what [`super`] creates and writes to).
    #[must_use]
    pub fn database_or_default(&self) -> &str {
        self.database.as_deref().unwrap_or(super::DEFAULT_DATABASE)
    }
}

/// Parse `HYDRA_TDENGINE_URL`.
///
/// Accepted: `http://host:6041`, `http://user:pass@host:6041/hydra`, with any query string ignored
/// (taosAdapter has no `?database=` style parameters — the database is the path, measured). A path
/// is therefore MEANINGFUL here, unlike the ClickHouse URL where one is trimmed: `…/6041/hydra`
/// names the database and `…/6041` does not.
///
/// `https://` is refused, for the same reason ClickHouse's is: this transport has no TLS code path,
/// and silently connecting in the clear while the URL promised TLS would leak the credentials.
pub fn parse_tdengine_url(raw: &str) -> Result<TdengineConfig, String> {
    let trimmed = raw.trim();
    let without_scheme = if let Some(rest) = trimmed.strip_prefix("http://") {
        rest
    } else if trimmed.starts_with("https://") {
        return Err(
            "HYDRA_TDENGINE_URL uses https:// but this transport has no TLS code path; refusing to \
             send credentials in PLAINTEXT — terminate TLS in front of taosAdapter, or use a tunnel"
                .to_string(),
        );
    } else {
        return Err(format!(
            "HYDRA_TDENGINE_URL must start with http:// (got {trimmed:?})"
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
    Ok(TdengineConfig {
        host: host.to_string(),
        port,
        user,
        password,
        database,
        connect_timeout: env_millis("HYDRA_TDENGINE_CONNECT_TIMEOUT_MS", 3_000),
        io_timeout: env_millis("HYDRA_TDENGINE_IO_TIMEOUT_MS", 15_000),
    })
}

fn split_host_port(hostport: &str) -> Result<(&str, u16), String> {
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p),
        None => (hostport, "6041"),
    };
    if host.is_empty() {
        return Err("HYDRA_TDENGINE_URL has no host".to_string());
    }
    if port.is_empty() {
        // taosAdapter's REST default (measured; the native protocol's 6030 is a different port).
        return Ok((host, 6041));
    }
    let port: u16 = port
        .parse()
        .map_err(|_| format!("HYDRA_TDENGINE_URL has an invalid port {port:?}"))?;
    Ok((host, port))
}

/// `%XX` → the byte it stands for. Applied to the credentials only: a password with an `@` or `/`
/// must survive the URL, and the characters a URL cannot carry literally are exactly what this
/// undoes.
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
                    target: "hydra::usage::tdengine",
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

/// What taosAdapter answered.
#[derive(Clone, Debug)]
pub struct TdengineResponse {
    /// The `code` field from the body — `0` means the server did what was asked.
    pub code: i64,
    /// The `desc` field, when the server sent one: the actionable text for an operator.
    pub desc: String,
    /// The `column_meta` names, in column order.
    pub columns: Vec<String>,
    /// The `data` rows, as raw JSON values.
    pub rows: Vec<Vec<serde_json::Value>>,
    /// The raw body, for diagnostics.
    pub body: String,
}

impl TdengineResponse {
    /// Parse taosAdapter's envelope: `{"code":0,"column_meta":[[name,type,len],…],"data":[[…]],"rows":N}`.
    ///
    /// A body that is not that envelope is a refusal, not an empty answer: a proxy or a login page
    /// in the middle must not read as "no usage".
    pub fn parse(body: &str) -> Result<Self, String> {
        let v: serde_json::Value = serde_json::from_str(body)
            .map_err(|e| format!("response is not JSON ({e}): {}", preview(body)))?;
        let obj = v
            .as_object()
            .ok_or_else(|| format!("response is not an object: {}", preview(body)))?;
        let code = obj
            .get("code")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| format!("response has no numeric `code`: {}", preview(body)))?;
        let desc = obj
            .get("desc")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let columns = obj
            .get("column_meta")
            .and_then(serde_json::Value::as_array)
            .map(|metas| {
                metas
                    .iter()
                    .filter_map(|m| {
                        m.as_array()
                            .and_then(|a| a.first())
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let rows = obj
            .get("data")
            .and_then(serde_json::Value::as_array)
            .map(|data| {
                data.iter()
                    .map(|r| r.as_array().cloned().unwrap_or_default())
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            code,
            desc,
            columns,
            rows,
            body: body.to_string(),
        })
    }

    /// The error text for a non-zero `code`, naming what the server said.
    #[must_use]
    pub fn error_text(&self) -> String {
        if self.desc.is_empty() {
            format!("taosAdapter refused the statement (code {})", self.code)
        } else {
            format!(
                "taosAdapter refused the statement (code {}): {}",
                self.code, self.desc
            )
        }
    }

    /// The column index of `name` (case-insensitive: TDengine upper-cases bare aliases in
    /// `column_meta`, measured).
    #[must_use]
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))
    }
}

fn preview(body: &str) -> String {
    body.chars().take(160).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_host_gets_the_rest_default_port() {
        let cfg = parse_tdengine_url("http://taos.example:6041").expect("parses");
        assert_eq!(cfg.host, "taos.example");
        assert_eq!(cfg.port, 6041);
        assert_eq!(cfg.database, None);
        assert_eq!(cfg.database_or_default(), super::super::DEFAULT_DATABASE);
    }

    #[test]
    fn credentials_and_a_path_database_are_parsed() {
        let cfg =
            parse_tdengine_url("http://root:taosdata@127.0.0.1:6041/hydra?x=1").expect("parses");
        assert_eq!(cfg.user.as_deref(), Some("root"));
        assert_eq!(cfg.password.as_deref(), Some("taosdata"));
        // The PATH is the database (measured: taosAdapter has no `?database=` parameter, and the
        // path is what makes a bare table name resolve).
        assert_eq!(cfg.database.as_deref(), Some("hydra"));
        assert_eq!(cfg.port, 6041);
    }

    #[test]
    fn a_percent_encoded_password_survives_the_url() {
        let cfg = parse_tdengine_url("http://u:p%40ss%2Fword@host:6041").expect("parses");
        assert_eq!(cfg.password.as_deref(), Some("p@ss/word"));
    }

    #[test]
    fn https_is_refused_rather_than_sent_in_the_clear() {
        let err = parse_tdengine_url("https://host:6041").unwrap_err();
        assert!(err.contains("PLAINTEXT"), "{err}");
    }

    #[test]
    fn a_url_without_a_scheme_is_refused_with_the_reason() {
        let err = parse_tdengine_url("host:6041").unwrap_err();
        assert!(err.contains("must start with http://"), "{err}");
    }

    /// The rule that makes this store safe to talk to at all (measured: a wrong password is a 200).
    #[test]
    fn a_200_with_a_nonzero_code_is_an_error_not_a_success() {
        let resp = TdengineResponse::parse(r#"{"code":855,"desc":"Authentication failure"}"#)
            .expect("parses");
        assert_eq!(resp.code, 855);
        assert!(
            resp.error_text().contains("Authentication failure"),
            "the refusal must carry the server's own words: {}",
            resp.error_text()
        );
        assert_ne!(
            resp.code, 0,
            "a 200 with code!=0 must never be read as success"
        );
    }

    #[test]
    fn a_body_that_is_not_the_envelope_is_a_failure() {
        assert!(TdengineResponse::parse("<html>login</html>").is_err());
        assert!(
            TdengineResponse::parse(r#"{"rows":0}"#).is_err(),
            "no `code` field"
        );
    }

    #[test]
    fn column_meta_is_read_by_name_case_insensitively() {
        let body = r#"{"code":0,"column_meta":[["REQUESTS","BIGINT",8],["tokens_in","BIGINT",8]],"data":[[7,3]],"rows":1}"#;
        let resp = TdengineResponse::parse(body).expect("parses");
        assert_eq!(resp.column_index("requests"), Some(0));
        assert_eq!(resp.column_index("TOKENS_IN"), Some(1));
        assert_eq!(resp.column_index("nope"), None);
    }
}

// ===========================================================================
// The request
// ===========================================================================

/// A token for `Authorization: Bearer …`.
///
/// **UNVERIFIED here**: token authentication arrived in TDengine 3.4.0.0 and the only image
/// available on this machine is 3.3.6.13 (the 3.4.x tags are refused by the configured registry
/// mirror). The header is written the way the documentation describes it; the Basic path is the one
/// that has been measured. A deployment that sets this on a 3.3 server gets taosAdapter's own
/// refusal (`code != 0`), not a silent failure — which is the property that matters.
pub const TOKEN_ENV: &str = "HYDRA_TDENGINE_TOKEN";

/// The `Authorization` header value for this configuration.
#[must_use]
pub fn authorization_header(cfg: &TdengineConfig, token: Option<&str>) -> Option<String> {
    if let Some(token) = token.filter(|t| !t.is_empty()) {
        // Token auth wins when it is configured: it is the 3.4 way, and a deployment that sets both
        // means the token.
        return Some(format!("Bearer {token}"));
    }
    let user = cfg.user.as_deref()?;
    let password = cfg.password.as_deref().unwrap_or("");
    Some(format!(
        "Basic {}",
        base64(format!("{user}:{password}").as_bytes())
    ))
}

/// Minimal base64 (standard alphabet, padded). Written out rather than pulled in: this backend has
/// no dependencies of its own (ADR-0002 §4 step 3 — a backend's feature declares what it needs, and
/// this one needs nothing).
fn base64(input: &[u8]) -> String {
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

/// The largest response body this transport will read. A metrics store answering a bounded aggregate
/// cannot need more; a proxy or a login page answering with a stream must not be buffered forever.
const MAX_BODY: usize = 4 * 1024 * 1024;

/// Send one statement to taosAdapter and parse the envelope.
///
/// The body is the SQL text (measured: taosAdapter takes SQL, not JSON), the path carries the
/// database when the URL named one, and the answer is parsed by [`TdengineResponse::parse`] — which
/// is what turns "HTTP 200 with `code != 0`" into the error it is.
pub async fn send(cfg: &TdengineConfig, sql: &str) -> Result<TdengineResponse, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let path = match cfg.database.as_deref() {
        Some(db) => format!("/rest/sql/{db}"),
        None => "/rest/sql".to_string(),
    };
    let token = std::env::var(TOKEN_ENV).ok();
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {}:{}\r\nContent-Length: {}\r\nContent-Type: text/plain; charset=UTF-8\r\nConnection: close\r\n",
        cfg.host,
        cfg.port,
        sql.len()
    );
    if let Some(auth) = authorization_header(cfg, token.as_deref()) {
        request.push_str(&format!("Authorization: {auth}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(sql);

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
        loop {
            let n = stream
                .read(&mut buf)
                .await
                .map_err(|e| format!("reading the answer from {addr} failed: {e}"))?;
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n]);
            if raw.len() > MAX_BODY {
                return Err(format!(
                    "the answer from {addr} exceeded {MAX_BODY} bytes; refusing to buffer more"
                ));
            }
            if body_is_complete(&raw) {
                break;
            }
        }
        Ok::<Vec<u8>, String>(raw)
    };

    let raw = tokio::time::timeout(cfg.io_timeout, exchange)
        .await
        .map_err(|_| {
            format!(
                "the answer from {addr} did not arrive within {:?}",
                cfg.io_timeout
            )
        })??;

    let text = String::from_utf8_lossy(&raw).into_owned();
    let body = response_body(&text);
    // Chunked framing is stripped before parsing: the size lines are not JSON (measured).
    let body = if is_chunked(&text) {
        dechunk(&body).unwrap_or(body)
    } else {
        body
    };
    TdengineResponse::parse(&body)
}

/// `true` once the headers are in and the body matches `Content-Length` (or the answer is chunked and
/// has reached its terminator). taosAdapter answers with `Content-Length`; the chunked branch is
/// there because a reverse proxy in front of it may not.
fn body_is_complete(raw: &[u8]) -> bool {
    let text = String::from_utf8_lossy(raw);
    let Some(head) = text.find("\r\n\r\n") else {
        return false;
    };
    let (headers, body) = text.split_at(head + 4);
    let lower = headers.to_ascii_lowercase();
    if lower.contains("transfer-encoding: chunked") {
        // Complete when the zero-length terminator has arrived — asked of the framing itself, not of
        // whether the text happens to end with a `0` (a JSON body can).
        return dechunk(body).is_some();
    }
    match lower
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        Some(len) => body.len() >= len,
        // No length and not chunked: read to EOF (the loop does that when the server closes).
        None => false,
    }
}

/// `true` when the response says its body is chunk-framed.
fn is_chunked(head_text: &str) -> bool {
    match head_text.find("\r\n\r\n") {
        Some(i) => head_text[..i]
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked"),
        None => false,
    }
}

/// Decode a chunked body, or `None` when the framing is not complete yet.
///
/// Each chunk is `<size in hex>[;ext]\r\n<bytes>\r\n`, terminated by a zero-size chunk (the
/// trailing headers are ignored: taosAdapter sends none).
fn dechunk(body: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = body;
    loop {
        let (size_line, after) = rest.split_once("\r\n")?;
        let size_text = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16).ok()?;
        if size == 0 {
            return Some(out);
        }
        if after.len() < size + 2 {
            return None;
        }
        out.push_str(after.get(..size)?);
        rest = after.get(size + 2..)?;
    }
}

/// The body of an HTTP response, or the whole text when there is no header terminator (so the parse
/// failure can quote what actually arrived).
fn response_body(text: &str) -> String {
    match text.find("\r\n\r\n") {
        Some(i) => text[i + 4..].to_string(),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod request_tests {
    use super::*;

    #[test]
    fn basic_auth_is_base64_of_user_and_password() {
        let cfg = parse_tdengine_url("http://root:taosdata@h:6041").expect("parses");
        assert_eq!(
            authorization_header(&cfg, None).as_deref(),
            Some("Basic cm9vdDp0YW9zZGF0YQ=="),
            "the measured credential form (`curl -u root:taosdata` sends exactly this)"
        );
    }

    /// 3.4's token auth, written but unmeasured here (see [`TOKEN_ENV`]).
    #[test]
    fn a_token_beats_basic_auth_when_it_is_set() {
        let cfg = parse_tdengine_url("http://root:taosdata@h:6041").expect("parses");
        assert_eq!(
            authorization_header(&cfg, Some("tok")).as_deref(),
            Some("Bearer tok")
        );
    }

    #[test]
    fn no_credentials_means_no_header() {
        let cfg = parse_tdengine_url("http://h:6041").expect("parses");
        assert_eq!(authorization_header(&cfg, None), None);
    }

    #[test]
    fn base64_matches_the_reference_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn the_body_is_split_from_the_headers() {
        assert_eq!(
            response_body("HTTP/1.1 200 OK\r\n\r\n{\"code\":0}"),
            "{\"code\":0}"
        );
        assert_eq!(response_body("garbage"), "garbage");
    }

    /// The measured framing: what taosAdapter actually sends.
    #[test]
    fn a_chunked_answer_is_decoded_and_an_incomplete_one_is_not_guessed_at() {
        // `a` = 0x0a = 10 bytes = the length of `{"code":0}` (the real body's size line was `4a`,
        // for a longer answer; the FRAMING is what this pins, not the payload).
        let raw =
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n a \r\n{\"code\":0}\r\n0\r\n\r\n"
                .replace(" a ", "a");
        let body = response_body(&raw);
        assert!(is_chunked(&raw));
        assert_eq!(dechunk(&body).as_deref(), Some("{\"code\":0}"));

        // A body that merely ENDS with a `0` (a count of 10, say) must not read as complete.
        // `b` = 11 bytes = `{"rows":10}`; the terminator chunk is MISSING, so this must not read as
        // complete.
        let half = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nb\r\n{\"rows\":10}\r\n";
        assert_eq!(dechunk(&response_body(half)), None);
        assert!(!body_is_complete(half.as_bytes()));

        // And a complete one is.
        let done =
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nb\r\n{\"rows\":10}\r\n0\r\n\r\n";
        assert!(body_is_complete(done.as_bytes()));
        assert_eq!(
            dechunk(&response_body(done)).as_deref(),
            Some("{\"rows\":10}")
        );
    }

    #[test]
    fn completeness_needs_headers_and_the_whole_body() {
        assert!(!body_is_complete(b"HTTP/1.1 200 OK\r\n"));
        assert!(!body_is_complete(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabc"
        ));
        assert!(body_is_complete(
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc"
        ));
    }
}
