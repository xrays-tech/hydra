
//! Client credential extraction across HTTP transport forms (pure).
//!
//! Hydra accepts a client api-key from any of the transports real coding
//! agents and SDKs use
//! (dev-docs/aegis/plans/2026-09-10-inbound-credential-forms.md):
//!
//! | # | Transport | Who uses it |
//! |---|-----------|-------------|
//! | 1 | `Authorization: Bearer <k>` | OpenAI SDK, Anthropic SDK (OAuth) |
//! | 2 | `Authorization: <k>` (bare, no scheme) | self-rolled clients |
//! | 3 | `x-api-key` | Anthropic SDK |
//! | 4 | `api-key` | Azure OpenAI |
//! | 5 | `x-goog-api-key` | Gemini CLI / google-genai |
//! | 6 | query `?key=` / `?api_key=` / `?apikey=` / `?access_token=` | browser WebSocket |
//!
//! Precedence is the table order: the first transport that yields a non-empty
//! value wins. Any *later* transport carrying a **different** value is reported
//! in [`KeyExtraction::conflicts`] so the I/O shell can log one warning — the
//! values themselves are never logged.
//!
//! Pure: no I/O, no time, no global state, no allocation beyond the returned
//! `String`.

/// Where a client api-key was found. Variant order == precedence order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySource {
    /// `Authorization: Bearer <k>` (scheme case-insensitive).
    AuthorizationBearer,
    /// `Authorization: <k>` with no whitespace anywhere (no scheme at all).
    AuthorizationBare,
    /// `x-api-key`.
    XApiKey,
    /// `api-key` (Azure OpenAI style).
    ApiKey,
    /// `x-goog-api-key` (Gemini style).
    XGoogApiKey,
    /// Query-string parameter.
    Query,
}

impl KeySource {
    /// Stable label for logs/metrics — never contains the key value.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::AuthorizationBearer => "authorization_bearer",
            Self::AuthorizationBare => "authorization_bare",
            Self::XApiKey => "x_api_key",
            Self::ApiKey => "api_key",
            Self::XGoogApiKey => "x_goog_api_key",
            Self::Query => "query",
        }
    }
}

/// Query parameter names searched, in order. First present non-empty wins.
const QUERY_KEY_NAMES: [&str; 4] = ["key", "api_key", "apikey", "access_token"];

/// Raw request-derived inputs.
///
/// Header lookup is the I/O shell's job (core carries no HTTP types); values
/// are passed **verbatim**, un-trimmed, and normalised inside the parser.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClientKeyInput<'a> {
    pub authorization: Option<&'a str>,
    pub x_api_key: Option<&'a str>,
    pub api_key: Option<&'a str>,
    pub x_goog_api_key: Option<&'a str>,
    /// Raw query string without the leading `?` (i.e. `Uri::query()`).
    pub query: Option<&'a str>,
}

/// Extraction result (design §6.3 §3).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyExtraction {
    /// Highest-precedence key, if any transport carried one.
    pub key: Option<String>,
    /// Which transport produced [`Self::key`].
    pub source: Option<KeySource>,
    /// Other transports that carried a **different** non-empty value, in
    /// ascending precedence order. Never contains key values.
    pub conflicts: Vec<KeySource>,
}

/// Extract the client api-key from `input`, honouring the precedence table.
#[must_use]
pub fn extract_client_key(input: &ClientKeyInput<'_>) -> KeyExtraction {
    let mut found: Vec<(KeySource, String)> = Vec::new();

    if let Some((source, value)) = parse_authorization(input.authorization) {
        found.push((source, value.to_string()));
    }
    for (source, raw) in [
        (KeySource::XApiKey, input.x_api_key),
        (KeySource::ApiKey, input.api_key),
        (KeySource::XGoogApiKey, input.x_goog_api_key),
    ] {
        if let Some(value) = raw.and_then(trimmed_non_empty) {
            found.push((source, value.to_string()));
        }
    }
    if let Some(value) = input.query.and_then(query_key) {
        found.push((KeySource::Query, value));
    }

    let Some((source, key)) = found.first().map(|(s, v)| (*s, v.clone())) else {
        return KeyExtraction::default();
    };
    let conflicts = found
        .iter()
        .skip(1)
        .filter(|(_, value)| *value != key)
        .map(|(source, _)| *source)
        .collect();
    KeyExtraction {
        key: Some(key),
        source: Some(source),
        conflicts,
    }
}

/// Parse an `Authorization` header value.
///
/// - `Bearer <k>` (scheme case-insensitive per RFC 9110) → bearer source;
/// - a value containing **no** whitespace (and not a lone scheme name) →
///   bare-key source;
/// - anything else (`Basic …`, `Api-Key …`, a scheme name with no value, an
///   empty value) → `None`, so the caller falls through to the next
///   transport (C4).
fn parse_authorization(raw: Option<&str>) -> Option<(KeySource, &str)> {
    let value = raw?.trim();
    if value.is_empty() {
        return None;
    }
    match value.find(char::is_whitespace) {
        Some(idx) => {
            let (scheme, rest) = value.split_at(idx);
            if !scheme.eq_ignore_ascii_case("bearer") {
                return None;
            }
            let key = rest.trim();
            (!key.is_empty()).then_some((KeySource::AuthorizationBearer, key))
        }
        // No whitespace: a bare key — unless the value IS the scheme name
        // (`Authorization: Bearer` with the value trimmed away), which is not
        // a credential at all.
        None if value.eq_ignore_ascii_case("bearer") => None,
        None => Some((KeySource::AuthorizationBare, value)),
    }
}

/// Trim, mapping the empty string to "not provided".
fn trimmed_non_empty(raw: &str) -> Option<&str> {
    let value = raw.trim();
    (!value.is_empty()).then_some(value)
}

/// First non-empty value among [`QUERY_KEY_NAMES`], percent-decoded.
fn query_key(query: &str) -> Option<String> {
    for name in QUERY_KEY_NAMES {
        for pair in query.split('&') {
            let (raw_name, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
            if raw_name != name {
                continue;
            }
            let decoded = percent_decode(raw_value);
            let trimmed = decoded.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Percent-decode `%XX` escapes.
///
/// `+` is deliberately **left as-is** (never turned into a space): api-keys may
/// legitimately contain `+`, and every transport we accept uses `%XX`. Invalid
/// or truncated escapes are kept literally. Non-UTF-8 results degrade lossily
/// (no panic path).
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    if !bytes.contains(&b'%') {
        return raw.to_string();
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                out.push(high * 16 + low);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Hex digit → value, or `None` for a non-hex byte.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(
        authorization: Option<&'a str>,
        x_api_key: Option<&'a str>,
        api_key: Option<&'a str>,
        x_goog_api_key: Option<&'a str>,
        query: Option<&'a str>,
    ) -> ClientKeyInput<'a> {
        ClientKeyInput {
            authorization,
            x_api_key,
            api_key,
            x_goog_api_key,
            query,
        }
    }

    #[test]
    fn no_credentials_anywhere_is_empty() {
        let out = extract_client_key(&input(None, None, None, None, None));
        assert_eq!(out, KeyExtraction::default());
    }

    #[test]
    fn bearer_wins_over_every_later_transport() {
        let out = extract_client_key(&input(
            Some("Bearer A"),
            Some("B"),
            Some("C"),
            Some("D"),
            Some("key=E"),
        ));
        assert_eq!(out.key.as_deref(), Some("A"));
        assert_eq!(out.source, Some(KeySource::AuthorizationBearer));
        assert_eq!(
            out.conflicts,
            vec![
                KeySource::XApiKey,
                KeySource::ApiKey,
                KeySource::XGoogApiKey,
                KeySource::Query
            ]
        );
    }

    #[test]
    fn bearer_scheme_is_case_insensitive() {
        for header in ["Bearer k", "bearer k", "BEARER k", "BeArEr k"] {
            let out = extract_client_key(&input(Some(header), None, None, None, None));
            assert_eq!(out.key.as_deref(), Some("k"), "header: {header}");
            assert_eq!(out.source, Some(KeySource::AuthorizationBearer));
        }
    }

    #[test]
    fn bare_authorization_value_is_a_key() {
        let out = extract_client_key(&input(Some("sk-bare"), None, None, None, None));
        assert_eq!(out.key.as_deref(), Some("sk-bare"));
        assert_eq!(out.source, Some(KeySource::AuthorizationBare));
    }

    #[test]
    fn non_bearer_scheme_is_not_a_key_and_falls_through() {
        let out = extract_client_key(&input(
            Some("Basic dXNlcjpwYXNz"),
            Some("B"),
            None,
            None,
            None,
        ));
        assert_eq!(out.key.as_deref(), Some("B"));
        assert_eq!(out.source, Some(KeySource::XApiKey));
        assert!(
            out.conflicts.is_empty(),
            "a non-Bearer scheme is not a credential transport here"
        );
    }

    #[test]
    fn valueless_bearer_falls_through() {
        for header in ["Bearer", "Bearer   ", "bearer\t"] {
            let out = extract_client_key(&input(Some(header), Some("B"), None, None, None));
            assert_eq!(out.key.as_deref(), Some("B"), "header: {header:?}");
        }
    }

    #[test]
    fn header_values_are_trimmed_and_empties_skipped() {
        let out = extract_client_key(&input(None, Some("   "), Some("  sk-api  "), None, None));
        assert_eq!(out.key.as_deref(), Some("sk-api"));
        assert_eq!(out.source, Some(KeySource::ApiKey));
    }

    #[test]
    fn precedence_x_api_key_over_api_key_over_google() {
        let out = extract_client_key(&input(None, Some("A"), Some("B"), Some("C"), None));
        assert_eq!(out.key.as_deref(), Some("A"));
        assert_eq!(out.conflicts, vec![KeySource::ApiKey, KeySource::XGoogApiKey]);

        let out = extract_client_key(&input(None, None, Some("B"), Some("C"), None));
        assert_eq!(out.key.as_deref(), Some("B"));
        assert_eq!(out.conflicts, vec![KeySource::XGoogApiKey]);

        let out = extract_client_key(&input(None, None, None, Some("C"), None));
        assert_eq!(out.key.as_deref(), Some("C"));
        assert!(out.conflicts.is_empty());
    }

    #[test]
    fn identical_values_are_not_conflicts() {
        let out = extract_client_key(&input(
            Some("Bearer same"),
            Some("same"),
            None,
            None,
            Some("key=same"),
        ));
        assert_eq!(out.key.as_deref(), Some("same"));
        assert!(out.conflicts.is_empty());
    }

    #[test]
    fn query_names_are_searched_in_order() {
        let out = extract_client_key(&input(
            None,
            None,
            None,
            None,
            Some("access_token=A&apikey=B&key=C"),
        ));
        assert_eq!(out.key.as_deref(), Some("C"));
        assert_eq!(out.source, Some(KeySource::Query));

        let out = extract_client_key(&input(
            None,
            None,
            None,
            None,
            Some("access_token=A&apikey=B"),
        ));
        assert_eq!(out.key.as_deref(), Some("B"));

        let out = extract_client_key(&input(None, None, None, None, Some("access_token=A")));
        assert_eq!(out.key.as_deref(), Some("A"));
    }

    #[test]
    fn query_skips_empty_values_and_keeps_searching() {
        let out = extract_client_key(&input(None, None, None, None, Some("key=&api_key=real")));
        assert_eq!(out.key.as_deref(), Some("real"));

        let out = extract_client_key(&input(None, None, None, None, Some("key&api_key=real")));
        assert_eq!(out.key.as_deref(), Some("real"));

        let out = extract_client_key(&input(None, None, None, None, Some("key=")));
        assert_eq!(out.key, None);
    }

    #[test]
    fn query_values_are_percent_decoded_but_plus_is_literal() {
        let out = extract_client_key(&input(None, None, None, None, Some("key=sk%2Da%20b")));
        assert_eq!(out.key.as_deref(), Some("sk-a b"));

        let out = extract_client_key(&input(None, None, None, None, Some("key=a+b")));
        assert_eq!(out.key.as_deref(), Some("a+b"));

        let out = extract_client_key(&input(None, None, None, None, Some("key=%zz%")));
        assert_eq!(
            out.key.as_deref(),
            Some("%zz%"),
            "invalid escapes stay literal"
        );
    }

    #[test]
    fn falls_back_to_query_only_when_no_header_carried_a_key() {
        let out = extract_client_key(&input(Some("Basic x"), Some(""), None, None, Some("key=Q")));
        assert_eq!(out.key.as_deref(), Some("Q"));
        assert_eq!(out.source, Some(KeySource::Query));
        assert!(out.conflicts.is_empty());
    }

    #[test]
    fn source_labels_are_stable_and_value_free() {
        assert_eq!(KeySource::AuthorizationBearer.label(), "authorization_bearer");
        assert_eq!(KeySource::AuthorizationBare.label(), "authorization_bare");
        assert_eq!(KeySource::XApiKey.label(), "x_api_key");
        assert_eq!(KeySource::ApiKey.label(), "api_key");
        assert_eq!(KeySource::XGoogApiKey.label(), "x_goog_api_key");
        assert_eq!(KeySource::Query.label(), "query");
    }
}
