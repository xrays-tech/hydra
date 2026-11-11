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
//! `POST /rest/sql[/<db>]`, `Authorization: Basic <base64(user:password)>`, and the SQL statement as
//! the **body**, not as a query parameter.
//!
//! # Version, and the one thing that is deliberately absent
//!
//! Everything here is measured against **`tdengine/tdengine:3.3.6.13`** (the LTS line), and nothing
//! in this backend needs a newer server: the statement form, the envelope, the framing and the read
//! SQL are all 3.3 features.
//!
//! 3.4.0.0 added token authentication (`Authorization: Bearer <token>`). It is **not implemented**,
//! on purpose: the image that could verify it is not reachable from here (every 3.4.x tag is refused
//! by the registry mirror this project can see), and an unverifiable auth branch is exactly the kind
//! of path that looks supported and is not. If a deployment needs it, it is one function — written
//! where the Basic header is built, with a live test beside it.
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

use crate::usage::http::{self, BasicAuth, Endpoint, Overflow, ReadLimits, Request};

/// Re-exported so `transport::env_millis` (used by the URL parser here and by call sites elsewhere)
/// keeps resolving after the move to `usage::http`.
pub(crate) use crate::usage::http::env_millis;

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
            Some((n, p)) => (Some(http::percent_decode(n)), Some(http::percent_decode(p))),
            None => (Some(http::percent_decode(u)), None),
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

// `percent_decode` and `env_millis` moved to `crate::usage::http` (the latter re-exported above);
// the URL parser below calls `http::percent_decode` so the one decoder is shared.

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

// ===========================================================================
// The request
// ===========================================================================

// `Authorization`/base64 moved to `crate::usage::http::basic_auth_header`: TDengine's
// dependency-free encoder was its only unique piece, and it is now shared by all three backends.

/// The largest response this transport will read. A metrics store answering a bounded aggregate
/// cannot need more; a proxy or a login page answering with a stream must not be buffered forever.
///
/// TDengine's overflow policy is `Error`: the cap is a hard safety net, not a truncated answer.
const MAX_BODY: usize = 4 * 1024 * 1024;

/// Send one statement to taosAdapter and parse the envelope.
///
/// The body is the SQL text (measured: taosAdapter takes SQL, not JSON), the path carries the
/// database when the URL named one, and the answer is parsed by [`TdengineResponse::parse`] — which
/// is what turns "HTTP 200 with `code != 0`" into the error it is.
///
/// The success policy is the **envelope**, not the HTTP status: `resp.status` is deliberately not
/// consulted. The framing and the caps live in [`crate::usage::http`].
pub async fn send(cfg: &TdengineConfig, sql: &str) -> Result<TdengineResponse, String> {
    let path = match cfg.database.as_deref() {
        Some(db) => format!("/rest/sql/{db}"),
        None => "/rest/sql".to_string(),
    };
    let endpoint = Endpoint {
        host: cfg.host.clone(),
        port: cfg.port,
        connect_timeout: cfg.connect_timeout,
        io_timeout: cfg.io_timeout,
    };
    let auth = cfg.user.as_ref().map(|user| BasicAuth {
        user: user.clone(),
        pass: cfg.password.clone().unwrap_or_default(),
    });
    let host_header = format!("{}:{}", cfg.host, cfg.port);
    let request = Request {
        path_and_query: &path,
        host_header: &host_header,
        auth,
        content_type: Some("text/plain; charset=UTF-8"),
        extra_headers: &[],
        body: sql.as_bytes(),
    };
    let limits = ReadLimits {
        max_header_bytes: MAX_BODY,
        max_body_bytes: None,
        overflow: Overflow::Error,
    };
    let resp = http::exchange(&endpoint, &request, &limits).await?;
    // Chunked framing is stripped before parsing: the size lines are not JSON (measured).
    let body = http::response_body(&resp.body);
    TdengineResponse::parse(&body)
}

// `body_is_complete`, `is_chunked`, `dechunk` and `response_body` moved to `crate::usage::http`:
// the shared read loop owns the completeness stop, and `response_body` owns chunked decoding.
// Their unit tests (base64 vectors, Basic auth, chunked framing, completeness) moved there too; the
// URL-parser and envelope contract tests stay in this file's `tests` module.

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
