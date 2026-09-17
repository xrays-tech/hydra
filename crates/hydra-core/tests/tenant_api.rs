//! Tenant API pure decisions (`crates/hydra-core/src/tenant_api.rs`).
//!
//! Every assertion here is about a *silent* failure mode: a wrong comparison
//! that still returns a number, a decoder that reports zero usage instead of
//! erroring, a route matcher that accepts a near miss and hands a tenant token
//! to the proxy pipeline. None of them throw; all of them produce a
//! syntactically valid, semantically wrong answer.

use hydra_core::tenant_api::{
    canonical_le, decode_usage_json_each_row, is_canonical_timestamp, normalize_as_of,
    parse_lenient_u64, parse_route, Endpoint, TenantApiRoute, UsageRow, UsageTotals,
};
use serde_json::json;

// ---------------------------------------------------------------------------
// canonical timestamp form
// ---------------------------------------------------------------------------

#[test]
fn canonical_form_is_accepted() {
    for ok in [
        "2026-09-16T00:00:00Z",
        "2026-01-01T00:00:00Z",
        "2026-12-31T23:59:59Z",
        "2000-02-29T12:34:56Z",
    ] {
        assert!(is_canonical_timestamp(ok), "{ok:?} must be accepted");
    }
}

#[test]
fn canonical_form_rejects_other_shapes() {
    // The space-separated form is what SQLite's `datetime('now')` produces, and
    // comparing against it silently selects a whole day (see `canonical_le`).
    // A bare date is a safe prefix, but it is not the canonical form.
    for bad in [
        "2026-09-16 00:00:00",
        "2026-09-16T00:00:00",
        "2026-09-16T00:00:00+08:00",
        "2026-09-16T00:00:00.123Z",
        "2026-09-16T00:00:00z",
        "2026-9-16T00:00:00Z",
        "2026-09-16",
        "2026-09-16T00:00:0Z",
        "2026-09-16T00:00:00ZZ",
        "",
        "not-a-timestamp",
    ] {
        assert!(!is_canonical_timestamp(bad), "{bad:?} must be rejected");
    }
}

#[test]
fn canonical_form_rejects_out_of_range_fields() {
    for bad in [
        "2026-00-16T00:00:00Z", // month 0
        "2026-13-16T00:00:00Z", // month 13
        "2026-09-00T00:00:00Z", // day 0
        "2026-09-32T00:00:00Z", // day 32
        "2026-09-16T24:00:00Z", // hour 24
        "2026-09-16T00:60:00Z", // minute 60
        "2026-09-16T00:00:60Z", // second 60
    ] {
        assert!(
            !is_canonical_timestamp(bad),
            "{bad:?} must be rejected by the range check"
        );
    }
}

#[test]
fn ordering_on_the_canonical_form_equals_chronological_ordering() {
    assert!(canonical_le("2026-09-16T00:00:00Z", "2026-09-16T00:00:01Z"));
    assert!(canonical_le("2026-09-16T23:59:59Z", "2026-09-17T00:00:00Z"));
    assert!(canonical_le("2025-12-31T23:59:59Z", "2026-01-01T00:00:00Z"));
    assert!(canonical_le("2026-09-16T00:00:00Z", "2026-09-16T00:00:00Z"));
    assert!(!canonical_le(
        "2026-09-17T00:00:00Z",
        "2026-09-16T23:59:59Z"
    ));
}

#[test]
fn space_separated_input_compares_greater_than_the_t_form() {
    // THE regression this documents: 'T' (0x54) > ' ' (0x20), so a
    // space-separated bound is "greater than" every T-form value on the same
    // date. A naive `created_at >= '2026-09-16 23:59:59'` therefore matches rows
    // from 00:00:00 onward — silently adding a whole day to the window.
    assert!(!canonical_le("2026-09-16T00:00:00Z", "2026-09-16 23:59:59"));
    assert!(canonical_le("2026-09-16 23:59:59", "2026-09-16T00:00:00Z"));
}

// ---------------------------------------------------------------------------
// ClickHouse lenient integers (the "silent zero" trap)
// ---------------------------------------------------------------------------

#[test]
fn quoted_and_plain_int64_are_both_accepted() {
    // Measured on ClickHouse 24.3: `SELECT toUInt64(12345) FORMAT JSONEachRow`
    // returns {"n":"12345"} — 64-bit ints are JSON-quoted by default.
    assert_eq!(parse_lenient_u64(&json!("12345")), Some(12345));
    assert_eq!(parse_lenient_u64(&json!(12345)), Some(12345));
    assert_eq!(parse_lenient_u64(&json!("0")), Some(0));
    assert_eq!(parse_lenient_u64(&json!(0)), Some(0));
    assert_eq!(
        parse_lenient_u64(&json!("18446744073709551615")),
        Some(u64::MAX)
    );
}

#[test]
fn unparseable_numbers_are_none_never_zero() {
    assert_eq!(parse_lenient_u64(&json!("")), None);
    assert_eq!(parse_lenient_u64(&json!("   ")), None);
    assert_eq!(parse_lenient_u64(&json!("abc")), None);
    assert_eq!(parse_lenient_u64(&json!("-1")), None);
    assert_eq!(parse_lenient_u64(&json!(-1)), None);
    assert_eq!(parse_lenient_u64(&json!(-1.0)), None);
    assert_eq!(parse_lenient_u64(&json!(1.5)), None);
    assert_eq!(parse_lenient_u64(&json!(null)), None);
    assert_eq!(parse_lenient_u64(&json!(true)), None);
    assert_eq!(parse_lenient_u64(&json!([1])), None);
    assert_eq!(parse_lenient_u64(&json!({"n": 1})), None);
    assert_eq!(parse_lenient_u64(&json!("99999999999999999999999")), None);
}

// ---------------------------------------------------------------------------
// as_of normalisation
// ---------------------------------------------------------------------------

#[test]
fn as_of_maps_empty_string_and_absent_to_none() {
    // SQLite: NULL. ClickHouse on an empty set: "" (measured). Both are "no
    // records", so both must be None — an empty as_of would look like a value.
    assert_eq!(normalize_as_of(None), None);
    assert_eq!(normalize_as_of(Some("")), None);
    assert_eq!(
        normalize_as_of(Some("2026-09-15T07:32:06Z")),
        Some("2026-09-15T07:32:06Z".to_string())
    );
}

// ---------------------------------------------------------------------------
// route parsing
// ---------------------------------------------------------------------------

#[test]
fn the_three_endpoints_parse() {
    assert_eq!(
        parse_route("/tenant/t1/api/v1/whoami"),
        Some(TenantApiRoute {
            endpoint: Endpoint::Whoami,
            tenant_id: "t1"
        })
    );
    assert_eq!(
        parse_route("/tenant/t1/api/v1/auth/cache/invalidate"),
        Some(TenantApiRoute {
            endpoint: Endpoint::InvalidateAuthCache,
            tenant_id: "t1"
        })
    );
    assert_eq!(
        parse_route("/tenant/t1/api/v1/usage"),
        Some(TenantApiRoute {
            endpoint: Endpoint::Usage,
            tenant_id: "t1"
        })
    );
}

#[test]
fn near_misses_do_not_parse() {
    for bad in [
        "/tenant//api/v1/whoami",         // empty tenant id
        "/tenant/a/b/api/v1/whoami",      // tenant id contains '/'
        "/tenant/t1/api/v1",              // no endpoint
        "/tenant/t1/api/v1/usage/",       // trailing slash
        "/tenant/t1/api/v1/whoami/extra", // extra segment
        "/tenant/t1/api/v2/whoami",       // wrong version
        "/tenant/t1/api/v1/models",       // not one of ours
        "/tenant/t1/api/v1/typo",         // typo
        "/tenant/t1/api/v1/Whoami",       // case matters
        "/tenantapi/v1/whoami",           // prefix glued
        "/tenant/",                       // nothing after the prefix
        "/tenant",                        // no prefix slash
        "/v1/chat/completions",           // ordinary data-plane path
        "",                               // empty
        "/",                              // root
    ] {
        assert!(parse_route(bad).is_none(), "{bad:?} must not parse");
    }
}

// ---------------------------------------------------------------------------
// JSONEachRow decoding (T14a / T14b, the blocking negatives)
// ---------------------------------------------------------------------------

/// Exactly what the live instance returned for the §4.3.1 aggregate query.
const LIVE_AGGREGATE: &str = r#"{"requests":"8","tokens_in":"247","tokens_out":"18","cache_hit_tokens":"0","errors":"0","last_seen":"2026-09-15T07:32:06Z"}"#;

#[test]
fn decodes_the_measured_live_shape() {
    let agg = decode_usage_json_each_row(LIVE_AGGREGATE).expect("the live shape must decode");
    assert_eq!(
        agg.totals,
        UsageTotals {
            requests: 8,
            tokens_in: 247,
            tokens_out: 18,
            cache_hit_tokens: 0,
            errors: 0,
        }
    );
    assert_eq!(agg.as_of.as_deref(), Some("2026-09-15T07:32:06Z"));
    assert!(agg.rows.is_empty(), "no group_by -> no rows");
}

#[test]
fn quoted_and_unquoted_bodies_decode_identically() {
    let plain = r#"{"requests":8,"tokens_in":247,"tokens_out":18,"cache_hit_tokens":0,"errors":0,"last_seen":"2026-09-15T07:32:06Z"}"#;
    let a = decode_usage_json_each_row(LIVE_AGGREGATE).expect("quoted");
    let b = decode_usage_json_each_row(plain).expect("plain");
    assert_eq!(a, b, "the quoting default must not change the answer");
}

#[test]
fn a_broken_field_errors_rather_than_reporting_zero() {
    for bad in [
        r#"{"requests":"abc","tokens_in":"1","tokens_out":"1","cache_hit_tokens":"0","errors":"0","last_seen":""}"#,
        r#"{"requests":-1,"tokens_in":"1","tokens_out":"1","cache_hit_tokens":"0","errors":"0","last_seen":""}"#,
        r#"{"tokens_in":"1","tokens_out":"1","cache_hit_tokens":"0","errors":"0","last_seen":""}"#,
        r#"{}"#,
        r#"{"requests":"1","tokens_in":"1","tokens_out":"1","cache_hit_tokens":"0","errors":"0"}"#,
    ] {
        let r = decode_usage_json_each_row(bad);
        assert!(r.is_err(), "{bad} must be an error, got {r:?}");
    }
}

#[test]
fn syntax_errors_and_empty_bodies_are_malformed() {
    for bad in ["", "   ", "\n\n", "not json", "[1,2]", "{\"a\":1}\n[2]"] {
        let r = decode_usage_json_each_row(bad);
        assert!(r.is_err(), "{bad:?} must be an error, got {r:?}");
    }
}

#[test]
fn empty_result_set_yields_zero_totals_and_no_as_of() {
    // Measured: an empty set gives {"requests":"0",...,"last_seen":""}.
    let body = r#"{"requests":"0","tokens_in":"0","tokens_out":"0","cache_hit_tokens":"0","errors":"0","last_seen":""}"#;
    let agg = decode_usage_json_each_row(body).expect("empty set is a valid answer");
    assert_eq!(agg.totals, UsageTotals::default());
    assert_eq!(agg.as_of, None, "CH returns \"\", not null, for no rows");
}

#[test]
fn group_rows_are_parsed_and_totals_come_from_the_last_line() {
    // `GROUP BY` returns the group rows first and the overall totals last.
    let body = concat!(
        r#"{"key":"Qwen/Qwen2.5-7B-Instruct","requests":"8","tokens_in":"247","tokens_out":"18","cache_hit_tokens":"0","errors":"0","last_seen":"2026-09-15T07:32:06Z"}"#,
        "\n",
        r#"{"key":"gpt-4o","requests":"2","tokens_in":"10","tokens_out":"3","cache_hit_tokens":"0","errors":"1","last_seen":"2026-09-15T07:32:06Z"}"#,
        "\n",
        r#"{"requests":"10","tokens_in":"257","tokens_out":"21","cache_hit_tokens":"0","errors":"1","last_seen":"2026-09-15T07:32:06Z"}"#,
    );
    let agg = decode_usage_json_each_row(body).expect("grouped body decodes");
    assert_eq!(agg.totals.requests, 10, "totals come from the LAST line");
    assert_eq!(agg.rows.len(), 2);
    assert_eq!(
        agg.rows[0],
        UsageRow {
            key: "Qwen/Qwen2.5-7B-Instruct".to_string(),
            totals: UsageTotals {
                requests: 8,
                tokens_in: 247,
                tokens_out: 18,
                cache_hit_tokens: 0,
                errors: 0,
            },
        }
    );
    assert_eq!(agg.rows[1].key, "gpt-4o");
    assert_eq!(agg.rows[1].totals.errors, 1);
}

#[test]
fn a_group_row_without_a_key_is_an_error() {
    let body = concat!(
        r#"{"requests":"1","tokens_in":"1","tokens_out":"1","cache_hit_tokens":"0","errors":"0","last_seen":""}"#,
        "\n",
        r#"{"requests":"1","tokens_in":"1","tokens_out":"1","cache_hit_tokens":"0","errors":"0","last_seen":""}"#,
    );
    assert!(decode_usage_json_each_row(body).is_err());
}
