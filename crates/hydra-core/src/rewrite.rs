//! Request-path rewrite & api-key masking (pure).
//!
//! Two pure functions over plain data (design §6.5 / §9.5):
//! - [`rewrite_path`] locates the first `/v1` in the request path and rebuilds
//!   an upstream URL against a parsed [`EndpointUrl`].
//! - [`mask_key`] redacts an api-key to `first10 + *** + last4` (never
//!   plaintext — P1-5).
//!
//! Both are allocation-only on the returned `String`; no I/O, no global state.
//! The [`EndpointUrl`] value type is the shared parsed-endpoint form consumed
//! by both the proxy shell and these helpers.

use serde::{Deserialize, Serialize};

/// A parsed upstream endpoint, derived from `Provider::endpoint`.
/// `path_prefix` is the path component of the base URL (empty for a bare host).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointUrl {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub path_prefix: String,
}

impl EndpointUrl {
    /// Parse a provider endpoint URL — **the single parser shared by the config
    /// loader, the admin write boundary and the upstream dialler**.
    ///
    /// It used to live only in the proxy shell (`proxy::peer::parse_endpoint`),
    /// while the config loader validated endpoints with a much weaker
    /// hand-rolled prefix test (`store::is_usable_endpoint`). The two disagreed,
    /// and the gap was observable: `HTTPS://host` (scheme case),
    /// `https://host:abc`, `https://host:99999` and `https://user:pass@host` all
    /// passed the loader — so the admin API answered 201, the snapshot reloaded
    /// happily, the provider showed up as healthy — and then **every single
    /// request skipped that provider** because the dialler could not parse its
    /// endpoint (audit §21). The loader and the write boundary now call THIS
    /// function, so "accepted" and "dialable" cannot drift apart again.
    ///
    /// Rules:
    /// - scheme must be `http` or `https`, compared **case-insensitively**
    ///   (RFC 3986 §3.1) and normalised to lower case in the result;
    /// - a non-empty host must follow;
    /// - an explicit `:port` must be a valid `u16`;
    /// - userinfo (`user:pass@host`) is rejected: credentials belong in
    ///   provider keys, and the dialler would silently mis-split such an
    ///   authority into host/port anyway;
    /// - the path component becomes `path_prefix` with any trailing `/`
    ///   stripped (query/fragment dropped), matching [`rewrite_path`].
    ///
    /// Returns `None` for anything the dialler could not use.
    #[must_use]
    pub fn parse(endpoint: &str) -> Option<Self> {
        // Case-insensitive scheme match (RFC 3986 §3.1) with NO allocation and
        // no copy of the host: `eq_ignore_ascii_case` on a borrowed prefix.
        // This function runs per candidate candidate on the request hot path
        // (proxy -> parse_endpoint), so an allocation here would be paid by
        // every proxied request.
        let (scheme, rest) = match endpoint.get(..8) {
            Some(head) if head.eq_ignore_ascii_case("https://") => ("https", endpoint.get(8..)?),
            _ => match endpoint.get(..7) {
                Some(head) if head.eq_ignore_ascii_case("http://") => ("http", endpoint.get(7..)?),
                _ => return None,
            },
        };

        // Split authority from path/query/fragment.
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = rest.get(..authority_end)?;
        // Reject userinfo (credentials belong in provider keys; the dialler
        // would mis-split `user:pass@host` into host/port anyway), any
        // whitespace/control character (never part of a host — would only fail
        // later at DNS), and any `%` (percent-encoding belongs in the path
        // component, never in the authority — its presence here means a
        // malformed host like `host%20name`).
        if authority.is_empty()
            || authority.contains('@')
            || authority.contains('%')
            || authority
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        {
            return None;
        }
        let tail = rest.get(authority_end..).unwrap_or("");

        // Authority = host[:port]. An IPv6 host MUST be bracketed
        // (`[::1]:8080`); a bare (unbracketed) host containing `:` is ambiguous
        // against the host:port split and is rejected. A bracketed host must be
        // a valid IPv6 literal (the brackets are stripped from the result).
        let (host, port) = if authority.starts_with('[') {
            let close = authority.find(']')?;
            let inner = authority.get(1..close)?;
            // The bracketed content must be a valid IPv6 address.
            if inner.parse::<std::net::Ipv6Addr>().is_err() {
                return None;
            }
            let after = &authority[close + 1..];
            let port = if after.is_empty() {
                default_port(scheme)
            } else if let Some(p) = after.strip_prefix(':') {
                p.parse::<u16>().ok()?
            } else {
                return None; // trailing junk after the bracket (not a `:port`)
            };
            (inner.to_string(), port)
        } else {
            let (h, port) = match authority.rsplit_once(':') {
                Some((h, p)) => (h, p.parse::<u16>().ok()?),
                None => (authority, default_port(scheme)),
            };
            // A host that still contains `:` (a bare IPv6 literal, no brackets)
            // is rejected: without brackets the host:port split is ambiguous.
            if h.contains(':') {
                return None;
            }
            (h.to_string(), port)
        };
        if host.is_empty() {
            return None;
        }

        let path_prefix = tail
            .split(['?', '#'])
            .next()
            .unwrap_or("")
            .trim_end_matches('/')
            .to_string();

        Some(Self {
            scheme: scheme.to_string(),
            host,
            port,
            path_prefix,
        })
    }
}

/// Scheme-default port (RFC 7230 §2.7).
fn default_port(scheme: &str) -> u16 {
    match scheme {
        "https" => 443,
        _ => 80,
    }
}
/// Rewrite a downstream request path onto an upstream endpoint (design §6.5).
///
/// Rules:
/// - Locate the **first** `/v1` in `req_path` and keep everything from there to
///   the end as the *tail* (so `/v1/a/v1/b` keeps the whole thing — the first
///   `/v1` wins).
/// - If there is no `/v1`, the entire `req_path` is the tail (passthrough).
/// - Prepend the endpoint base: `scheme://host[:port]` + `path_prefix`.
/// - The `:port` is omitted when it equals the scheme default (`443` for
///   `https`, `80` for `http`) and shown otherwise — matching how the W2 loader
///   fills `EndpointUrl` from a URL that carries no explicit port.
///
/// Allocation is limited to building the returned `String`.
pub fn rewrite_path(req_path: &str, endpoint: &EndpointUrl) -> String {
    let tail = match memchr::memmem::find(req_path.as_bytes(), b"/v1") {
        Some(idx) => &req_path[idx..],
        None => req_path,
    };

    let mut out = String::with_capacity(
        endpoint.scheme.len()
            + 3
            + endpoint.host.len()
            + endpoint.path_prefix.len()
            + tail.len()
            + 6,
    );
    out.push_str(&endpoint.scheme);
    out.push_str("://");
    out.push_str(&endpoint.host);
    // Only render the port when it is not the scheme default.
    let default_port = match endpoint.scheme.as_str() {
        "https" => Some(443),
        "http" => Some(80),
        _ => None,
    };
    if default_port != Some(endpoint.port) {
        out.push(':');
        out.push_str(&endpoint.port.to_string());
    }
    out.push_str(&endpoint.path_prefix);
    out.push_str(tail);
    out
}

/// Mask an api-key so it is identifiable but never plaintext (design §9.5 /
/// P1-5: the admin API NEVER returns plaintext provider keys).
///
/// Format (operates on `char` boundaries — safe for any valid `&str`):
///
/// | key length `L` | mask |
/// |----------------|------|
/// | `L >= 14` | first 10 chars + `'*'` × `(L − 14)` + last 4 chars |
/// | `6 <= L < 14` | first 2 chars + `'*'` × `(L − 4)` + last 2 chars |
/// | `L < 6` | `'*'` × `L` (fully masked) |
///
/// The three tiers ensure the masked form never reveals enough to reconstruct
/// the original: long keys expose a recognisable prefix + suffix (for
/// identification) but hide the entire middle; short keys expose less to avoid
/// revealing the whole value.
pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    let len = chars.len();

    if len >= 14 {
        // first 10 + stars(L-14) + last 4
        let star_count = len - 14;
        let mut out = String::with_capacity(len);
        for &c in &chars[..10] {
            out.push(c);
        }
        for _ in 0..star_count {
            out.push('*');
        }
        for &c in &chars[len - 4..] {
            out.push(c);
        }
        out
    } else if len >= 6 {
        // first 2 + stars(L-4) + last 2
        let star_count = len - 4;
        let mut out = String::with_capacity(len);
        for &c in &chars[..2] {
            out.push(c);
        }
        for _ in 0..star_count {
            out.push('*');
        }
        for &c in &chars[len - 2..] {
            out.push(c);
        }
        out
    } else {
        // L < 6: all stars
        "*".repeat(len)
    }
}
