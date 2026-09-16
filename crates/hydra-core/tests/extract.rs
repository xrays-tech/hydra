//! `extract::extract_model_field` — strict serde-based top-level model extraction.
//!
//! The extracted value drives authorization while the request body is forwarded
//! upstream verbatim, so these tests pin the property that actually matters: the
//! returned value is the one the UPSTREAM will read — the ROOT object's `model`
//! member, with JSON string escapes DECODED exactly as the provider's parser
//! decodes them. A decoy elsewhere in the body must never be used for
//! authorization, and a body the provider cannot parse as one JSON value must
//! not authorize either (fail closed).

use hydra_core::extract::{extract_model_field, extract_model_lossy, ModelField, ModelValue};

/// The decoded content when `body`'s root `model` is a JSON string; panics
/// otherwise. Lets tests assert both "it is a Value" and "its decoded content"
/// without constructing a `ModelValue` by hand.
fn decoded(body: &[u8]) -> String {
    match extract_model_field(body) {
        ModelField::Value(v) => v.into_string(),
        other => panic!(
            "body {} is not a string model member: {other:?}",
            String::from_utf8_lossy(body)
        ),
    }
}

// ---------------------------------------------------------------------------
// Value: the root model, decoded, at any position/schema.
// ---------------------------------------------------------------------------

// T5.1 - standard form, model is the first field.
#[test]
fn extract_model_standard() {
    let body = br#"{"model":"gpt-4o","messages":[]}"#;
    assert_eq!(extract_model_lossy(body), Some("gpt-4o".to_string()));
}

// T5.2 - whitespace tolerance around the key, colon and value.
#[test]
fn extract_model_whitespace_tolerant() {
    let body = br#"{ "model" : "gpt-4o" }"#;
    assert_eq!(extract_model_lossy(body), Some("gpt-4o".to_string()));
}

// T5.3 - model is not the first field.
#[test]
fn extract_model_not_first_field() {
    let body = br#"{"a":1,"model":"x","b":2}"#;
    assert_eq!(decoded(body), "x");
}

// T5.4 - no model member at all is ABSENT (distinct from a malformed one).
#[test]
fn extract_model_missing_is_absent() {
    let body = br#"{"messages":[{"role":"user"}]}"#;
    assert_eq!(extract_model_field(body), ModelField::Absent);
    assert_eq!(extract_model_lossy(body), None);
}

// T5.5 - zero copy: a no-escape model value is a zero-copy borrow of the input
// buffer (serde's `visit_borrowed_str` fast path), never an allocation.
#[test]
fn extract_model_no_allocation() {
    let body = br#"{"model":"gpt-4o"}"#;
    let ModelField::Value(ModelValue::Borrowed(model)) = extract_model_field(body) else {
        panic!("a no-escape model value must be a zero-copy borrow");
    };

    let body_start = body.as_ptr() as usize;
    let body_end = body_start + body.len();
    let model_start = model.as_ptr() as usize;
    let model_end = model_start + model.len();

    assert!(
        model_start >= body_start && model_end <= body_end,
        "a no-escape model value must be a zero-copy borrow of the input buffer"
    );
    assert_eq!(model, "gpt-4o");
}

// T5.6 - top-level model wins when it appears first.
#[test]
fn extract_model_top_level_preferred_when_first() {
    let body = br#"{"model":"real","messages":[{"model":"x"}]}"#;
    assert_eq!(decoded(body), "real");
}

// T5.6 (REGRESSION) - a nested decoy that textually precedes the top-level key
// MUST be ignored. The previous implementation returned the decoy, which meant
// Hydra authorized on one model while the provider served another; reproduced
// live against SiliconFlow as a full whitelist bypass.
#[test]
fn extract_model_nested_decoy_is_ignored() {
    let body = br#"{"messages":[{"model":"x"}],"model":"real"}"#;
    assert_eq!(decoded(body), "real");

    let body = br#"{"metadata":{"model":"allowed-model"},"model":"expensive-model"}"#;
    assert_eq!(decoded(body), "expensive-model");
}

// T5.7 - empty / very short input never panics.
#[test]
fn extract_model_short_input_no_panic() {
    assert_eq!(extract_model_lossy(b""), None);
    assert_eq!(extract_model_lossy(b"{"), None);
    assert_eq!(extract_model_lossy(b"\"model\""), None);
    assert_eq!(extract_model_lossy(b"\"model\":"), None);
    assert_eq!(extract_model_lossy(b"\"model\" :"), None);
    assert_eq!(extract_model_lossy(b"\"model\": 1"), None);
    assert_eq!(extract_model_lossy(b"\"model\":\""), None);
    assert_eq!(extract_model_field(b"{"), ModelField::Malformed);
}

// T5.8 (SEMANTIC CHANGE) - a truncated body (the root object was never closed)
// is no longer "the first chunk is enough". Strict serde JSON parsing requires
// the whole value, so a truncated body is MALFORMED - a state distinct from
// `Absent`, because `Absent` feeds the model-less passthrough and would skip the
// tenant model whitelist (see `unparsable_body_is_malformed_not_absent`).
#[test]
fn extract_model_truncated_body_is_malformed() {
    let first_chunk = br#"{"model":"claude-3-opus""#;
    assert_eq!(extract_model_field(first_chunk), ModelField::Malformed);
    assert_eq!(extract_model_lossy(first_chunk), None);

    let chunk = br#"{"model":"qwen","#;
    assert_eq!(extract_model_field(chunk), ModelField::Malformed);
    assert_eq!(extract_model_lossy(chunk), None);
}

// ---------------------------------------------------------------------------
// NotAString: a root model member whose value is not a string.
// ---------------------------------------------------------------------------

// REGRESSION (live variant A) - the previous implementation returned None here,
// which the proxy read as "no model" and routed through the model-less
// passthrough, skipping tenant_models entirely. A non-string root model must be
// reported, never silently downgraded to absent.
#[test]
fn non_string_root_model_is_not_a_string_not_absent() {
    for body in [
        br#"{"model":123,"messages":[]}"#.as_slice(),
        br#"{"model":null}"#.as_slice(),
        br#"{"model":true}"#.as_slice(),
        br#"{"model":["a"]}"#.as_slice(),
        br#"{"model":{"a":1}}"#.as_slice(),
    ] {
        assert_eq!(
            extract_model_field(body),
            ModelField::NotAString,
            "body: {}",
            String::from_utf8_lossy(body)
        );
        assert_eq!(extract_model_lossy(body), None);
    }
}

// REGRESSION (live variant A) - a nested non-string decoy must NOT stop the
// parse from reaching the real root member.
#[test]
fn nested_non_string_decoy_does_not_hide_the_root_model() {
    let body = br#"{"metadata":{"model":123},"model":"expensive-model"}"#;
    assert_eq!(decoded(body), "expensive-model");
}

// ---------------------------------------------------------------------------
// Structural cases the JSON parser has to get right (decoys, nesting, escapes
// inside other members).
// ---------------------------------------------------------------------------

// Escaped quotes inside a preceding string value must not end it early.
#[test]
fn escaped_quotes_in_a_preceding_string_are_handled() {
    let body = br#"{"note":"said \"model\":\"x\"","model":"real"}"#;
    assert_eq!(decoded(body), "real");
}

// Braces/brackets inside strings must not change the nesting depth.
#[test]
fn braces_inside_strings_do_not_confuse_depth() {
    let body = br#"{"a":"}","model":"real"}"#;
    assert_eq!(decoded(body), "real");

    let body = br#"{"a":["]","{",{"model":"decoy"}],"model":"real"}"#;
    assert_eq!(decoded(body), "real");
}

// Deeply nested decoys are skipped at any depth.
#[test]
fn deeply_nested_decoys_are_skipped() {
    let body = br#"{"a":{"b":{"c":{"model":"decoy"}}},"model":"real"}"#;
    assert_eq!(decoded(body), "real");
}

// A non-object root is MALFORMED (it is not the JSON object Hydra authorizes
// on), never `Absent`.
#[test]
fn non_object_root_is_malformed() {
    assert_eq!(
        extract_model_field(br#"[{"model":"x"}]"#),
        ModelField::Malformed
    );
    assert_eq!(extract_model_field(br#""model""#), ModelField::Malformed);
    assert_eq!(
        extract_model_field(b"not json at all"),
        ModelField::Malformed
    );
    assert_eq!(extract_model_field(b"42"), ModelField::Malformed);
    assert_eq!(extract_model_field(b"null"), ModelField::Malformed);
}

// Unterminated strings are Malformed, never a panic and never a bogus slice.
#[test]
fn unterminated_input_is_malformed() {
    assert_eq!(extract_model_field(br#"{"model":"#), ModelField::Malformed);
    assert_eq!(
        extract_model_field(br#"{"a":"unterminated"#),
        ModelField::Malformed
    );
    assert_eq!(
        extract_model_field(br#"{"model":"unterminated"#),
        ModelField::Malformed
    );
}

// The root model is found even when every other member precedes it.
#[test]
fn root_model_found_after_other_members() {
    let body = br#"{"a":1,"b":{"model":"decoy"},"c":[1,2,3],"d":"x","model":"real"}"#;
    assert_eq!(decoded(body), "real");
}

// An empty root object has no model.
#[test]
fn empty_object_is_absent() {
    assert_eq!(extract_model_field(br#"{}"#), ModelField::Absent);
    assert_eq!(extract_model_lossy(br#"{}"#), None);
}

// ---------------------------------------------------------------------------
// Ambiguous: a duplicated top-level "model" key.
//
// The extracted value drives authorization while the body goes upstream
// verbatim, so when Hydra cannot pin down exactly which top-level member the
// provider will read, it MUST fail closed (reject) rather than guess.
// ---------------------------------------------------------------------------

// REGRESSION (F-2) - a SECOND top-level "model" key is ambiguous: JSON parsers
// disagree on which duplicate wins (first vs last), so the provider may read a
// different model than Hydra authorized. The whole root is parsed (no
// first-hit early exit) and Ambiguous is reported.
#[test]
fn duplicate_top_level_model_is_ambiguous() {
    let body = br#"{"model":"a","model":"b"}"#;
    assert_eq!(
        extract_model_field(body),
        ModelField::Ambiguous,
        "two top-level model keys must be ambiguous, never the first-hit value"
    );
    assert_eq!(extract_model_lossy(body), None);

    // A duplicate is ambiguous regardless of what the values are (even equal).
    let body = br#"{"model":"gpt-4","model":"gpt-4"}"#;
    assert_eq!(extract_model_field(body), ModelField::Ambiguous);
    assert_eq!(extract_model_lossy(body), None);

    // A nested decoy in between must not mask the duplicate top-level key.
    let body = br#"{"model":"a","metadata":{"model":"x"},"model":"b"}"#;
    assert_eq!(extract_model_field(body), ModelField::Ambiguous);
    assert_eq!(extract_model_lossy(body), None);
}

// (SEMANTIC CHANGE) - a top-level key that DECODES to "model" is now recognized
// as the model member (the provider's parser decodes the key too):
// {"\u006dodel":"a"} → model "a". The old zero-copy scanner could not decode a
// key and treated any escaped top-level key as Ambiguous. Two keys that both
// decode to "model" (an escaped alias plus a raw one) are still a duplicate →
// Ambiguous.
#[test]
fn escaped_key_that_decodes_to_model() {
    // "\u006d" decodes to "m", so the key "\u006dodel" decodes to "model".
    let body = br#"{"\u006dodel":"a"}"#;
    assert_eq!(decoded(body), "a");
    assert_eq!(extract_model_lossy(body), Some("a".to_string()));

    // Two keys that both decode to "model" (escaped alias + raw) are a duplicate.
    let body = br#"{"\u006dodel":"a","model":"b"}"#;
    assert_eq!(extract_model_field(body), ModelField::Ambiguous);
    assert_eq!(extract_model_lossy(body), None);

    // Two escaped aliases that both decode to "model" are also a duplicate.
    let body = br#"{"\u006dodel":"a","\u006dodel":"b"}"#;
    assert_eq!(extract_model_field(body), ModelField::Ambiguous);
    assert_eq!(extract_model_lossy(body), None);
}

// (SEMANTIC CHANGE) - the model VALUE is now decoded, matching the provider's
// parser. {"model":"a\"b"} authorizes on the decoded `a"b`, not the raw bytes
// `a\"b`. An escape in a sibling value is irrelevant.
#[test]
fn escaped_model_value_is_decoded() {
    let body = br#"{"model":"a\"b","x":"\u006dodel"}"#;
    assert_eq!(decoded(body), "a\"b");
    assert_eq!(extract_model_lossy(body), Some("a\"b".to_string()));
}

// "\uXXXX" escapes in the value are decoded to their code points, matching the
// provider's parser.
#[test]
fn escaped_unicode_model_value_is_decoded() {
    // "\u002d" decodes to "-", so the authorized model is "gpt-4".
    let body = br#"{"model":"gpt\u002d4"}"#;
    assert_eq!(decoded(body), "gpt-4");

    // "\u0061\u0062" decodes to "ab".
    let body = br#"{"model":"\u0061\u0062"}"#;
    assert_eq!(decoded(body), "ab");
}

// (STRICT) - trailing content after the root object is MALFORMED. The old
// scanner returned the model it had already seen; the strict parser requires
// the body to be exactly one JSON value. Trailing whitespace, though, is fine.
#[test]
fn trailing_garbage_is_malformed() {
    let body = br#"{"model":"a"}garbage"#;
    assert_eq!(extract_model_field(body), ModelField::Malformed);
    assert_eq!(extract_model_lossy(body), None);

    let body = br#"{"model":"a"}   "#;
    assert_eq!(decoded(body), "a");
}

// ---------------------------------------------------------------------------
// REGRESSION (review A1) - `Absent` must mean exactly ONE thing: "a well-formed
// JSON object with no `model` member" (the documented `NonRouteStrategy` case).
//
// Everything Hydra cannot parse as one JSON object is `Malformed` and MUST be
// rejected by the caller. Conflating the two let any unparsable body fall into
// the model-less PASSTHROUGH - which never consults `tenant_models` - while the
// body was still forwarded upstream verbatim, so the provider parsed its own
// top-level `model` and served a model the tenant was never granted.
//
// Every body below is read successfully (model included) by at least one
// mainstream provider parser: Python/Node/Go all accept an extra top-level key
// containing an unpaired-surrogate escape (grammatically valid per RFC 8259),
// and Go's `json.Decoder.Decode` also accepts trailing bytes after the value.
// ---------------------------------------------------------------------------
#[test]
fn unparsable_body_is_malformed_not_absent() {
    let malformed: [(&str, &[u8]); 8] = [
        (
            "trailing content",
            br#"{"model":"unauthorized-model","messages":[]} trailing"#,
        ),
        (
            "trailing NUL byte",
            b"{\"model\":\"unauthorized-model\",\"messages\":[]}\x00",
        ),
        (
            "unpaired-surrogate escape in an extra TOP-LEVEL key",
            br#"{"\ud800":1,"model":"unauthorized-model","messages":[]}"#,
        ),
        (
            "escape serde_json rejects in another member",
            br#"{"model":"unauthorized-model","x":"\q"}"#,
        ),
        (
            "raw control character inside a string",
            b"{\"model\":\"unauthorized-model\",\"x\":\"a\nb\"}",
        ),
        (
            "truncated (root object never closed)",
            br#"{"model":"unauthorized-model"#,
        ),
        ("non-object root", br#"[{"model":"unauthorized-model"}]"#),
        ("empty body", b""),
    ];
    for (name, body) in malformed {
        assert_eq!(
            extract_model_field(body),
            ModelField::Malformed,
            "{name} must be Malformed (never Absent - Absent skips the whitelist)"
        );
        assert_eq!(extract_model_lossy(body), None, "{name}");
    }

    // `Absent` is reserved for a valid object with no `model` member.
    for body in [
        br#"{}"#.as_slice(),
        br#"{"messages":[]}"#.as_slice(),
        br#"{"metadata":{"model":"decoy"},"stream":true}"#.as_slice(),
    ] {
        assert_eq!(
            extract_model_field(body),
            ModelField::Absent,
            "valid JSON object without a root model: {}",
            String::from_utf8_lossy(body)
        );
    }
}
