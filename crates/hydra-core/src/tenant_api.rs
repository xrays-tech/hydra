//! Tenant self-service API — pure decisions (design `design-tenant-api.md` §4).
//!
//! Everything here is a pure function over plain data: no I/O, no global state,
//! no HTTP types, and — deliberately — **no `chrono`**. `hydra-core` allows only
//! `serde`/`serde_json`/`memchr`/`bytes`/`sha2` (see `Cargo.toml` and the
//! "no `chrono` in core" note in `model.rs`), so the *lexical* parsing of the
//! caller's `since`/`until` (RFC 3339 with offsets, epoch seconds/millis) and
//! the window-length arithmetic live in the server shell
//! (`hydra-server/src/tenant_api/time_bound.rs`). This module only ever sees the
//! **canonical** form `%Y-%m-%dT%H:%M:%SZ`:
//!
//! - [`is_canonical_timestamp`] — fixed-width shape **and** numeric range check
//!   (pure string work; no calendar arithmetic, so no leap-year rules here).
//! - [`canonical_le`] — ordering. On the canonical fixed-width UTC form the
//!   lexicographic order *is* the chronological order, which is what makes the
//!   `created_at >= ?` string comparison in both metering stores correct.
//! - [`parse_route`] — the data-plane reserved path → `(tenant_id, endpoint)`.
//! - [`parse_lenient_u64`] / [`normalize_as_of`] / [`decode_usage_json_each_row`]
//!   — ClickHouse `FORMAT JSONEachRow` decoding.
//!
//! ## Why the decoder belongs here rather than next to the transport
//!
//! `hydra-server/src/clickhouse.rs` is gated on `usage-clickhouse`, and that
//! feature is **not** implied by `server` (CI adds it explicitly for exactly
//! this reason). If decoding lived there, the two most dangerous negative tests
//! ("a quoted 64-bit integer must still decode", "an empty-string `last_seen`
//! must become `None`") would only run in the secondary feature matrix. In core
//! they run in `cargo test -p hydra-core`, i.e. in CI's first job, always.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Length of the canonical timestamp form `YYYY-MM-DDTHH:MM:SSZ`.
pub const CANONICAL_TS_LEN: usize = 20;

/// Whether `s` is exactly the canonical metering timestamp form.
///
/// Shape: 20 bytes, ASCII digits at every position except the four separators,
/// which must be `-`, `-`, `T`, `:`, `:`, `Z` at fixed offsets — plus numeric
/// range checks on month/day/hour/minute/second so `2026-13-45T99:99:99Z` is
/// rejected rather than silently compared as a string.
///
/// The day range is `01..=31`: **this function does not do calendar
/// arithmetic** (no month lengths, no leap years), because that would need a
/// date library and core must not have one. Its job is to guarantee the
/// *fixed width and digits* that make lexicographic comparison equal
/// chronological comparison; the shell, which does have `chrono`, is where a
/// real date is parsed and normalised.
#[must_use]
pub fn is_canonical_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != CANONICAL_TS_LEN {
        return false;
    }
    // Separators first: this also proves every other byte is a digit candidate.
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return false;
    }
    if b[19] != b'Z' {
        return false;
    }
    for (i, c) in b.iter().enumerate() {
        if matches!(i, 4 | 7 | 10 | 13 | 16 | 19) {
            continue;
        }
        if !c.is_ascii_digit() {
            return false;
        }
    }
    let num = |from: usize| -> u32 {
        // Safe: the digit check above ran over every position.
        let hi = u32::from(b[from] - b'0');
        let lo = u32::from(b[from + 1] - b'0');
        hi * 10 + lo
    };
    let (month, day, hour, minute, second) = (num(5), num(8), num(11), num(14), num(17));
    (1..=12).contains(&month)
        && (1..=31).contains(&day)
        && hour <= 23
        && minute <= 59
        && second <= 59
}

/// Ordering on the canonical form: `a <= b`.
///
/// Valid **only** when both operands satisfy [`is_canonical_timestamp`]: the
/// form is fixed-width, zero-padded, all-UTC and has constant `T`/`Z`
/// separators, so byte comparison agrees with chronological comparison. Two
/// consequences worth stating because both have bitten:
///
/// - A space-separated value (`2026-09-16 23:59:59`, which is what SQLite's
///   `datetime('now')` produces) compares as **greater** than any `T`-form
///   value on the same date, because `'T'` (0x54) > `' '` (0x20) — a
///   comparison against it silently selects the whole day.
/// - This does **not** generalise to other shapes (offsets, fractional
///   seconds), which is why the shell normalises before calling in.
#[must_use]
pub fn canonical_le(a: &str, b: &str) -> bool {
    a <= b
}

/// A numeric field from a ClickHouse `JSONEachRow` object.
///
/// **`UInt64` arrives as a JSON string by default.** Measured against the
/// project's own ClickHouse (24.3): `SELECT toUInt64(12345) FORMAT JSONEachRow`
/// returns `{"n":"12345"}` — `output_format_json_quote_64bit_integers` defaults
/// to 1, and `count()`/`sum()` are `UInt64` too. A decoder that only accepted
/// JSON numbers would read every aggregate as absent and report **zero usage**
/// for a tenant that has plenty: a syntactically valid, semantically wrong 200.
/// So both forms are accepted, and anything unparseable is `None` (which the
/// caller must surface as an error — never as 0).
#[must_use]
pub fn parse_lenient_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64().or_else(|| {
            // Reject negatives and fractions explicitly rather than truncating.
            let f = n.as_f64()?;
            if f.is_finite() && f >= 0.0 && f.fract() == 0.0 && f <= u64::MAX as f64 {
                Some(f as u64)
            } else {
                None
            }
        }),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                return None;
            }
            t.parse::<u64>().ok()
        }
        _ => None,
    }
}

/// `MAX(created_at)` → `Option<String>`.
///
/// SQLite yields `NULL` for an empty set, but **ClickHouse yields `""`** for a
/// `String` column (measured: `{"last_seen":""}`). Both must become `None`, or
/// the response carries an empty-string `as_of` that looks like a real value.
#[must_use]
pub fn normalize_as_of(raw: Option<&str>) -> Option<String> {
    match raw {
        None | Some("") => None,
        Some(s) => Some(s.to_string()),
    }
}

/// Which tenant self-service endpoint a request path addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Endpoint {
    Whoami,
    InvalidateAuthCache,
    Usage,
}

/// A parsed tenant API path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TenantApiRoute<'a> {
    pub endpoint: Endpoint,
    pub tenant_id: &'a str,
}

/// Parse `/tenant/{tenant_id}/api/v1/...` into `(tenant_id, endpoint)`.
///
/// Exact matching: the suffix must be one of the three known endpoints, so a
/// near miss (`.../models`, `.../typo`, a trailing slash, an extra segment) is
/// `None`. The caller then answers `404` **locally** — it must not let such a
/// path fall through to the proxy pipeline, where a tenant token would be read
/// as a client api-key and POSTed to the tenant's `auth_url` (design §3.2).
#[must_use]
pub fn parse_route(path: &str) -> Option<TenantApiRoute<'_>> {
    let rest = path.strip_prefix("/tenant/")?;
    let (tenant_id, suffix) = rest.split_once('/')?;
    if tenant_id.is_empty() {
        return None;
    }
    let endpoint = match suffix {
        "api/v1/whoami" => Endpoint::Whoami,
        "api/v1/auth/cache/invalidate" => Endpoint::InvalidateAuthCache,
        "api/v1/usage" => Endpoint::Usage,
        _ => return None,
    };
    Some(TenantApiRoute {
        endpoint,
        tenant_id,
    })
}

/// Token totals for a window (or for one group within it).
///
/// `Default` is all-zero, which is exactly the `COALESCE(..., 0)` semantics the
/// aggregate SQL asks the store for: the token columns are nullable (a provider
/// that reports nothing must not masquerade as a zero count), so "no rows" and
/// "rows with NULL tokens" both end up as 0 here — but only after the *store*
/// said so, never because decoding failed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub requests: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_hit_tokens: u64,
    pub errors: u64,
}

/// One `group_by` row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRow {
    pub key: String,
    #[serde(flatten)]
    pub totals: UsageTotals,
}

/// Why a metering-store response could not be decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The body was not valid `JSONEachRow` / not an object.
    Malformed(String),
    /// A required field was missing or unparseable. Carries the field name.
    ///
    /// This is an error and **never** a zero: silently substituting 0 is the
    /// "syntactically valid, semantically wrong 200" this module exists to
    /// prevent.
    Field(String),
}

/// Decoded usage aggregate.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageAggregate {
    pub totals: UsageTotals,
    /// Group rows; empty when the query had no `group_by`.
    pub rows: Vec<UsageRow>,
    /// Newest record in the queried window, or `None` when the window has no
    /// records. Lets the caller state its own consistency window (`as_of`).
    pub as_of: Option<String>,
}

/// Decode a ClickHouse `FORMAT JSONEachRow` aggregate response.
///
/// The aggregate query returns **one** line; with `group_by` it returns the
/// group rows first and the overall totals last. Every numeric field goes
/// through [`parse_lenient_u64`] (quoted-`UInt64` tolerant) and the trailing
/// `last_seen` through [`normalize_as_of`] (empty-string tolerant).
///
/// A missing or unparseable field is [`DecodeError::Field`] — deliberately
/// *not* a zero.
///
/// ## Do not feed this a `UNION ALL`
///
/// The "rows first, totals last" order is a property of the BODY, and a
/// ClickHouse `UNION ALL` does not guarantee it: measured on the bundled
/// ClickHouse 24.3 this shape came back totals-first in 1 of 6 runs, and a bare
/// `ORDER BY` after the `UNION ALL` did not stabilise it. The reader therefore
/// issues the totals and grouped queries separately and uses
/// [`decode_usage_rows_json_each_row`] for the second. Use this function only for
/// a body whose last line is the totals **by construction**.
pub fn decode_usage_json_each_row(body: &str) -> Result<UsageAggregate, DecodeError> {
    let mut objects: Vec<Value> = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line)
            .map_err(|e| DecodeError::Malformed(format!("{e}: {line}")))?;
        if !v.is_object() {
            return Err(DecodeError::Malformed(format!("not an object: {line}")));
        }
        objects.push(v);
    }
    let Some(last) = objects.pop() else {
        return Err(DecodeError::Malformed("empty body".to_string()));
    };
    let totals = totals_from(&last)?;
    let mut rows = Vec::with_capacity(objects.len());
    for o in &objects {
        let key = o
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| DecodeError::Field("key".to_string()))?
            .to_string();
        rows.push(UsageRow {
            key,
            totals: totals_from(o)?,
        });
    }
    // `last_seen` must be PRESENT — the aggregate SQL always projects it
    // (`MAX(created_at) AS last_seen`), so an absent column means we are not
    // talking to the query we think we are, and silently reading it as "no
    // records" would hide a shape drift. Being present-but-empty is the
    // legitimate "empty result set" case (ClickHouse returns `""`, not null),
    // and `normalize_as_of` turns exactly that into `None`.
    let last_seen = last
        .get("last_seen")
        .and_then(Value::as_str)
        .ok_or_else(|| DecodeError::Field("last_seen".to_string()))?;
    let as_of = normalize_as_of(Some(last_seen));
    Ok(UsageAggregate {
        totals,
        rows,
        as_of,
    })
}

/// Decode the **grouped** `JSONEachRow` response of the ClickHouse reader: every
/// non-empty line is one group row.
///
/// This is a separate function from [`decode_usage_json_each_row`] because the
/// two backends cannot share a body shape. The SQLite reader issues two queries
/// (totals, then groups) and so must the ClickHouse reader, because a single
/// `UNION ALL` of the two **does not preserve branch order**: measured on the
/// bundled ClickHouse 24.3, the same query returned the totals row first in 1 of
/// 6 runs, and adding `ORDER BY is_total` did not fix it (a bare `ORDER BY` after
/// a `UNION ALL` binds to the last branch only). An order-dependent decoder would
/// therefore read a *group* row as the overall totals — a 200 that is
/// syntactically fine and semantically wrong, which is the exact failure class
/// this module exists to prevent.
///
/// An empty body is **not** an error here: a tenant with no matching rows has no
/// group rows at all, and ClickHouse answers the grouped query with an empty body.
pub fn decode_usage_rows_json_each_row(body: &str) -> Result<Vec<UsageRow>, DecodeError> {
    let mut rows = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line)
            .map_err(|e| DecodeError::Malformed(format!("{e}: {line}")))?;
        if !v.is_object() {
            return Err(DecodeError::Malformed(format!("not an object: {line}")));
        }
        let key = v
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| DecodeError::Field("key".to_string()))?
            .to_string();
        rows.push(UsageRow {
            key,
            totals: totals_from(&v)?,
        });
    }
    Ok(rows)
}

/// The five numeric fields of one aggregate line. Every one is required.
fn totals_from(o: &Value) -> Result<UsageTotals, DecodeError> {
    let field = |name: &'static str| -> Result<u64, DecodeError> {
        o.get(name)
            .and_then(parse_lenient_u64)
            .ok_or_else(|| DecodeError::Field(name.to_string()))
    };
    Ok(UsageTotals {
        requests: field("requests")?,
        tokens_in: field("tokens_in")?,
        tokens_out: field("tokens_out")?,
        cache_hit_tokens: field("cache_hit_tokens")?,
        errors: field("errors")?,
    })
}
