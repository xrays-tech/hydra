//! Lexical parsing and normalisation of E3's `since` / `until` bounds.
//!
//! ## Why this lives in the shell and not in `hydra-core`
//!
//! `hydra-core` is a pure crate whose dependency whitelist
//! (`crates/hydra-core/Cargo.toml`) has **no `chrono`**, and RFC3339 offsets and
//! epoch conversion need calendar arithmetic. So the split is:
//!
//! - **here** — lexical parsing (RFC3339 / epoch seconds / epoch milliseconds),
//!   normalisation to the canonical form, and the window-length decision;
//! - **`hydra_core::tenant_api`** — the *shape* and range rules of the canonical
//!   form, and the ordering comparison, which are pure string operations.
//!
//! ## Why the canonical form is not cosmetic
//!
//! `usage_record.created_at` is written by the application as
//! `%Y-%m-%dT%H:%M:%SZ` and compared as a **string**. That is only sound because
//! the form is fixed-width, zero-padded and all-UTC, so lexicographic order is
//! chronological order. A bound left in a different spelling is not "close
//! enough": `'T'` (0x54) sorts after `' '` (0x20), so `2026-09-16 00:00:00`
//! compares *less* than every row of that day and the window silently grows by a
//! whole day. Everything that reaches a query therefore goes through
//! [`Window`], which can only be built from normalised strings.

use chrono::{DateTime, NaiveDateTime, TimeZone as _, Utc};

/// Default ceiling on an E3 window, in days (`HYDRA_TENANT_API_USAGE_MAX_WINDOW_DAYS`).
///
/// Not an optimisation: in ClickHouse the table's sorting key leads with
/// `created_at`, so a wide window scans every tenant's rows inside it before
/// filtering by `tenant_id`. The cap is what keeps one tenant's query from
/// becoming a scan over everyone else's data.
pub const DEFAULT_USAGE_MAX_WINDOW_DAYS: u32 = 31;

/// Why a bound or a window was refused. The variant decides the HTTP error code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoundError {
    /// `since` is missing, unparseable, or ordered after `until`.
    InvalidSince,
    /// `until` is present but unparseable.
    InvalidUntil,
    /// The window is wider than the configured ceiling.
    WindowTooLarge { max_days: u32 },
}

impl BoundError {
    /// The `error.code` reported to the tenant.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidSince => "invalid_since",
            Self::InvalidUntil => "invalid_until",
            Self::WindowTooLarge { .. } => "window_too_large",
        }
    }

    /// The `error.message` reported to the tenant. Names the accepted forms, so
    /// a caller can fix the request without reading a document.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::InvalidSince => {
                "`since` must be RFC3339 (2026-09-16T00:00:00Z), epoch seconds or \
                 epoch milliseconds, and must not be later than `until`"
                    .to_string()
            }
            Self::InvalidUntil => {
                "`until` must be RFC3339 (2026-09-17T00:00:00Z), epoch seconds or \
                 epoch milliseconds"
                    .to_string()
            }
            Self::WindowTooLarge { max_days } => {
                format!("the requested window is wider than the {max_days}-day maximum")
            }
        }
    }
}

/// A validated window. Constructible only through [`resolve`], so a query can
/// never be handed an un-normalised bound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    /// Canonical `%Y-%m-%dT%H:%M:%SZ`, inclusive lower bound.
    pub since: String,
    /// Canonical `%Y-%m-%dT%H:%M:%SZ`, **exclusive** upper bound.
    pub until: String,
}

/// Render an instant in the canonical form. The single formatter: the writer and
/// the reader must agree byte for byte or the string comparison is meaningless.
#[must_use]
pub fn canonical(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// The instant "now", as an owner that tests can substitute.
#[must_use]
pub fn now() -> DateTime<Utc> {
    Utc::now()
}

/// Above this many seconds an all-digit input is read as milliseconds rather
/// than seconds: `100_000_000_000` seconds is the year 5138, while the same
/// number of milliseconds is 1973 — so the ranges cannot overlap in practice.
const EPOCH_MILLIS_THRESHOLD: i64 = 100_000_000_000;

/// Parse one bound. Accepted spellings, all interpreted as UTC unless they carry
/// their own offset:
///
/// - RFC3339: `2026-09-16T00:00:00Z`, `2026-09-16T00:00:00.500Z`,
///   `2026-09-16T08:00:00+08:00`;
/// - `2026-09-16T00:00:00` (no zone — the "someone dropped the Z" case);
/// - `2026-09-16 00:00:00` (the space-separated historical form);
/// - epoch seconds (`1789516800`) or milliseconds (`1789516800000`).
///
/// Returns `None` rather than a guess: a bound that cannot be understood must
/// never be rounded into a different window.
#[must_use]
pub fn parse_bound(raw: &str) -> Option<DateTime<Utc>> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }

    // Epoch.
    if s.chars().all(|c| c.is_ascii_digit()) {
        let n: i64 = s.parse().ok()?;
        return if n >= EPOCH_MILLIS_THRESHOLD {
            Utc.timestamp_millis_opt(n).single()
        } else {
            Utc.timestamp_opt(n, 0).single()
        };
    }

    // Explicit offset (`Z` or `±HH:MM`).
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }

    // Zoneless: `T`-separated and space-separated, with or without a fraction.
    for fmt in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M:%S%.f",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(Utc.from_utc_datetime(&naive));
        }
    }
    None
}

/// Normalise and validate the request's bounds.
///
/// `until` is optional and defaults to `now`, so the documented "usage after a
/// timestamp" call needs one parameter. A zero-width window (`since == until`)
/// is allowed and answers an empty set — it is a coherent question.
pub fn resolve(
    since: Option<&str>,
    until: Option<&str>,
    max_days: u32,
    now: DateTime<Utc>,
) -> Result<Window, BoundError> {
    let raw_since = since.unwrap_or("").trim();
    if raw_since.is_empty() {
        return Err(BoundError::InvalidSince);
    }
    let since = parse_bound(raw_since).ok_or(BoundError::InvalidSince)?;

    let until = match until.map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => parse_bound(raw).ok_or(BoundError::InvalidUntil)?,
        None => now,
    };

    if since > until {
        return Err(BoundError::InvalidSince);
    }
    // The ceiling is on the window's LENGTH, so `>` is the correct comparison:
    // exactly `max_days` is allowed, one second more is not.
    let max_secs = i64::from(max_days) * 86_400;
    if (until - since).num_seconds() > max_secs {
        return Err(BoundError::WindowTooLarge { max_days });
    }

    Ok(Window {
        since: canonical(since),
        until: canonical(until),
    })
}

/// Percent-decode one query-string component.
///
/// `+` is kept **literal** rather than read as a space: RFC 3986 puts a space in
/// a query as `%20`, and treating `+` as a space would corrupt the offset form
/// `2026-09-16T00:00:00+08:00`, which a client may legitimately send unencoded.
/// A malformed escape is passed through unchanged, and the bound parser then
/// rejects it — a 400 is the right answer for a broken encoding.
#[must_use]
pub fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
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

/// The decoded `key=value` pairs of a query string. A repeated key keeps its
/// FIRST occurrence, so a second `since=` appended by an intermediary cannot
/// widen the window the first one asked for.
#[must_use]
pub fn query_params(raw: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        let k = percent_decode(k);
        if out.iter().any(|(existing, _)| *existing == k) {
            continue;
        }
        out.push((k, percent_decode(v)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        parse_bound(s).expect("parseable")
    }

    #[test]
    fn the_canonical_form_is_produced_for_every_accepted_input() {
        for raw in [
            "2026-09-16T00:00:00Z",
            "2026-09-16T00:00:00",
            "2026-09-16 00:00:00",
            "2026-09-16T08:00:00+08:00",
            "1789516800",
            "1789516800000",
        ] {
            assert_eq!(canonical(at(raw)), "2026-09-16T00:00:00Z", "input {raw}");
        }
    }

    #[test]
    fn a_fractional_second_is_truncated_not_rounded_into_the_next_second() {
        assert_eq!(
            canonical(at("2026-09-16T00:00:00.999Z")),
            "2026-09-16T00:00:00Z"
        );
    }

    #[test]
    fn an_offset_bound_is_converted_to_utc() {
        assert_eq!(
            canonical(at("2026-09-16T00:00:00-05:00")),
            "2026-09-16T05:00:00Z"
        );
    }

    #[test]
    fn nonsense_is_rejected_rather_than_guessed() {
        for raw in [
            "",
            "   ",
            "yesterday",
            "2026-13-45T00:00:00Z",
            "2026-09-16T25:00:00Z",
            "1e9",
        ] {
            assert!(parse_bound(raw).is_none(), "must reject {raw:?}");
        }
    }

    #[test]
    fn a_missing_since_is_refused() {
        assert_eq!(
            resolve(None, None, 31, at("2026-09-17T00:00:00Z")),
            Err(BoundError::InvalidSince)
        );
        assert_eq!(
            resolve(Some("  "), None, 31, at("2026-09-17T00:00:00Z")),
            Err(BoundError::InvalidSince)
        );
    }

    #[test]
    fn until_defaults_to_now() {
        let w = resolve(
            Some("2026-09-16T00:00:00Z"),
            None,
            31,
            at("2026-09-17T00:00:00Z"),
        )
        .expect("window");
        assert_eq!(w.until, "2026-09-17T00:00:00Z");
    }

    #[test]
    fn the_window_ceiling_is_inclusive() {
        let now = at("2026-10-01T00:00:00Z");
        // Exactly 31 days.
        assert!(resolve(
            Some("2026-08-17T00:00:00Z"),
            Some("2026-09-17T00:00:00Z"),
            31,
            now
        )
        .is_ok());
        // One second more.
        assert_eq!(
            resolve(
                Some("2026-08-16T23:59:59Z"),
                Some("2026-09-17T00:00:00Z"),
                31,
                now
            ),
            Err(BoundError::WindowTooLarge { max_days: 31 })
        );
    }

    #[test]
    fn a_zero_width_window_is_allowed() {
        let w = resolve(
            Some("2026-09-16T00:00:00Z"),
            Some("2026-09-16T00:00:00Z"),
            31,
            at("2026-09-17T00:00:00Z"),
        )
        .expect("a coherent empty question");
        assert_eq!(w.since, w.until);
    }

    #[test]
    fn a_reversed_window_is_an_invalid_since() {
        assert_eq!(
            resolve(
                Some("2026-09-17T00:00:00Z"),
                Some("2026-09-16T00:00:00Z"),
                31,
                at("2026-09-18T00:00:00Z")
            ),
            Err(BoundError::InvalidSince)
        );
    }

    #[test]
    fn an_unparseable_until_is_its_own_error() {
        assert_eq!(
            resolve(
                Some("2026-09-16T00:00:00Z"),
                Some("tomorrow"),
                31,
                at("2026-09-17T00:00:00Z")
            ),
            Err(BoundError::InvalidUntil)
        );
    }

    #[test]
    fn percent_decoding_handles_escapes_and_leaves_plus_literal() {
        assert_eq!(
            percent_decode("2026-09-16%2000:00:00"),
            "2026-09-16 00:00:00"
        );
        assert_eq!(percent_decode("a%2Bb"), "a+b");
        assert_eq!(percent_decode("t1%27%20OR%201%3D1"), "t1' OR 1=1");
        assert_eq!(percent_decode("+08:00"), "+08:00");
        // A truncated or non-hex escape is passed through, not dropped.
        assert_eq!(percent_decode("50%"), "50%");
        assert_eq!(percent_decode("a%zzb"), "a%zzb");
    }

    #[test]
    fn a_repeated_parameter_keeps_the_first_value() {
        let p = query_params("since=2026-09-16T00:00:00Z&since=2020-01-01T00:00:00Z");
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].1, "2026-09-16T00:00:00Z");
    }

    #[test]
    fn a_valueless_parameter_is_kept_with_an_empty_value() {
        let p = query_params("group_by&since=x");
        assert_eq!(p[0], ("group_by".to_string(), String::new()));
        assert_eq!(p[1], ("since".to_string(), "x".to_string()));
    }
}
