//! # Shared HTTP/API helpers — `http-client`.
//!
//! Small, precise flat-field scans for the tiny control-surface contract bodies
//! used by HTTP handlers and their tests. They are dependency-light on purpose:
//! the bodies are bounded and the fields are top-level primitives, so a real
//! JSON parse is reserved for the object-shape guard and the flat scans below
//! stay allocation-conscious.
//!
//! The module also owns the display-atom and identity-timestamp helpers shared
//! by the admin and tenant APIs. They live here (rather than in a handler) so
//! the masking rules and the canonical RFC 3339 form have exactly one owner.

use serde_json::Value;

/// Read the TOP-LEVEL boolean verdict flag `key` ("status" or "allowed") from a
/// JSON object body.
///
/// Uses `serde_json` rather than a substring scan so the verdict can only come
/// from a real top-level member: a nested `{"data":{"status":true}}`, or the
/// word `"status":true` inside a STRING, must not be able to decide what the
/// caller sees. Returns `None` when the body is not a JSON object, the member
/// is absent, or it is not a boolean.
fn top_level_bool(body: &str, key: &str) -> Option<bool> {
    let value: Value = serde_json::from_str(body).ok()?;
    value.as_object()?.get(key)?.as_bool()
}

/// Whether a body carries an explicit DENIAL flag:
/// `{"status":false}` or `{"allowed":false}`.
#[must_use]
pub fn body_says_denied(body: &str) -> bool {
    matches!(top_level_bool(body, "status"), Some(false))
        || matches!(top_level_bool(body, "allowed"), Some(false))
}

/// Whether a body carries an explicit ALLOW flag:
/// `{"status":true}` or `{"allowed":true}`.
#[must_use]
pub fn body_says_allowed(body: &str) -> bool {
    matches!(top_level_bool(body, "status"), Some(true))
        || matches!(top_level_bool(body, "allowed"), Some(true))
}

/// Whether a body is a parseable JSON **object**.
///
/// Everything else — HTML, empty, malformed, or a JSON array/scalar — is
/// not an object-shaped contract body and must not be mistaken for one.
#[must_use]
pub fn body_is_json_object(body: &str) -> bool {
    matches!(
        serde_json::from_str::<serde_json::Value>(body),
        Ok(serde_json::Value::Object(_)),
    )
}

/// Extract the value of a top-level JSON string field, whitespace tolerant,
/// in the flat-scan style used by this module.
///
/// `field` must be the QUOTED key token (e.g. `"reason"`, quotes included)
/// so the split consumes the key and the remainder starts at the `:`
/// separator. Returns the value without its surrounding quotes; `None` when
/// the field is absent or is not a quoted string. Escaped characters inside
/// the value are skipped when locating the closing quote — the value is
/// returned verbatim, which is safe for the fixed vocabularies used here.
///
/// Structurally safe for the flat contract bodies — see [`body_says_denied`].
#[must_use]
pub fn json_string_field<'a>(body: &'a str, field: &str) -> Option<&'a str> {
    let rest = body.split_once(field)?.1.trim_start();
    let rest = rest.strip_prefix(":")?.trim_start();
    let rest = rest.strip_prefix("\"")?;
    let bytes = rest.as_bytes();
    let mut end = bytes.len();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x5C {
            i += 2; // skip the escaped character (quote or backslash)
        } else if b == 0x22 {
            end = i;
            break;
        } else {
            i += 1;
        }
    }
    Some(&rest[..end])
}

// ---------------------------------------------------------------------------
// Display atoms + canonical identity timestamps (admin & tenant APIs)
// ---------------------------------------------------------------------------

/// The first `n` characters of a value, as its own string (a display atom).
///
/// Character-based rather than byte-sliced: this alphabet is ASCII, but a byte
/// slice on a non-ASCII string PANICS, and an atom helper should not be the
/// thing that discovers that.
#[must_use]
pub fn head_chars(value: &str, n: usize) -> String {
    value.chars().take(n).collect()
}

/// The last `n` characters of a value (a display atom).
///
/// A short value is returned whole: truncation must never lose characters or
/// panic, because the caller uses this to show the tail of a credential.
#[must_use]
pub fn tail_chars(value: &str, n: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= n {
        return value.to_string();
    }
    chars[chars.len() - n..].iter().collect()
}

/// "Now" in the canonical RFC 3339 UTC form (e.g. `2026-08-09T12:34:56Z`).
///
/// Deliberately NOT `admin::handlers::now_ts`, whose `%Y-%m-%d %H:%M:%S` shape
/// sorts WRONG against a `Z`-terminated expiry: `'T'` (0x54) is greater than
/// `' '` (0x20), so mixed spellings make an expiry test lie about WHEN, not
/// about WHETHER. Lexicographic RFC 3339 comparison is safe because these
/// strings are normalized at write time.
#[must_use]
pub fn now_rfc3339() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_says_denied_dogress_status_false() {
        assert!(body_says_denied(
            "{\"status\":false,\"reason\":\"invalid_key\"}"
        ));
        assert!(body_says_denied("{ \"status\" : false }"));
    }

    #[test]
    fn body_says_denied_design_allowed_false() {
        assert!(body_says_denied(
            "{\"allowed\":false,\"reason\":\"blocked\"}"
        ));
    }

    #[test]
    fn body_says_denied_true_or_absent() {
        assert!(!body_says_denied("{\"status\":true,\"reason\":\"\"}"));
        assert!(!body_says_denied("{\"allowed\":true,\"expires_in\":300}"));
        assert!(!body_says_denied("{\"allowed\":true,\"status\":true}"));
        assert!(!body_says_denied(""));
        assert!(!body_says_denied("not json"));
        // `false` inside a string value must NOT count as a denial
        assert!(!body_says_denied("{\"reason\":\"status is false\"}"));
        // `"status":"false"` (string, not boolean) must NOT count either
        assert!(!body_says_denied("{\"status\":\"false\"}"));
    }

    #[test]
    fn body_is_json_object_cases() {
        // valid contract shapes → true
        assert!(body_is_json_object("{}"));
        assert!(body_is_json_object("{\"status\":true}"));
        assert!(body_is_json_object("{\"allowed\":false,\"expires_in\":60}"));
        assert!(body_is_json_object("  {\"status\" : true }  "));
        // non-verdict bodies → false (must never be treated as object-shaped)
        assert!(!body_is_json_object(""));
        assert!(!body_is_json_object("<html><body>sign in</body></html>"));
        assert!(!body_is_json_object("<!DOCTYPE html>"));
        assert!(!body_is_json_object("[1,2,3]"));
        assert!(!body_is_json_object("\"just a string\""));
        assert!(!body_is_json_object("42"));
        assert!(!body_is_json_object("{not json"));
        assert!(!body_is_json_object("not json"));
    }

    #[test]
    fn json_string_field_extracts_reason() {
        assert_eq!(
            json_string_field(
                r#"{"status":false,"reason":"insufficient_balance"}"#,
                "\"reason\""
            ),
            Some("insufficient_balance")
        );
        assert_eq!(
            json_string_field(
                r#"{ "reason" : "invalid_key" , "status":false}"#,
                "\"reason\""
            ),
            Some("invalid_key")
        );
    }

    #[test]
    fn json_string_field_absent_or_non_string() {
        assert_eq!(json_string_field(r#"{"status":false}"#, "\"reason\""), None);
        assert_eq!(json_string_field(r#"{"reason":42}"#, "\"reason\""), None);
        assert_eq!(json_string_field("not json", "\"reason\""), None);
        assert_eq!(
            json_string_field(r#"{"reason":""}"#, "\"reason\""),
            Some("")
        );
    }
}
