//! T5.1-T5.8 - `extract::extract_model_field` top-level, zero-copy extraction.
//!
//! The extracted value drives authorization while the request body is forwarded
//! upstream verbatim, so these tests pin the property that actually matters: the
//! returned value is the one the UPSTREAM will read, i.e. the ROOT object model
//! member. A decoy elsewhere in the body must never be used for authorization.

use hydra_core::extract::{extract_model, extract_model_field, ModelField};

// T5.1 - standard form, model is the first field.
#[test]
fn extract_model_standard() {
    let body = br#"{"model":"gpt-4o","messages":[]}"#;
    assert_eq!(extract_model(body), Some(&b"gpt-4o"[..]));
}

// T5.2 - whitespace tolerance around the key, colon and value.
#[test]
fn extract_model_whitespace_tolerant() {
    let body = br#"{ "model" : "gpt-4o" }"#;
    assert_eq!(extract_model(body), Some(&b"gpt-4o"[..]));
}

// T5.3 - model is not the first field.
#[test]
fn extract_model_not_first_field() {
    let body = br#"{"a":1,"model":"x","b":2}"#;
    assert_eq!(extract_model(body), Some(&b"x"[..]));
}

// T5.4 - no model member at all is ABSENT (distinct from a malformed one).
#[test]
fn extract_model_missing_is_absent() {
    let body = br#"{"messages":[{"role":"user"}]}"#;
    assert_eq!(extract_model_field(body), ModelField::Absent);
    assert_eq!(extract_model(body), None);
}

// T5.5 - zero allocation: the returned slice must be a sub-range of the input.
#[test]
fn extract_model_no_allocation() {
    let body = br#"{"model":"gpt-4o"}"#;
    let Some(ModelField::Value(model)) = Some(extract_model_field(body)) else {
        panic!("model present");
    };

    let body_start = body.as_ptr() as usize;
    let body_end = body_start + body.len();
    let model_start = model.as_ptr() as usize;
    let model_end = model_start + model.len();

    assert!(
        model_start >= body_start && model_end <= body_end,
        "returned slice must lie within the input buffer"
    );
    assert_eq!(model, b"gpt-4o");
}

// T5.6 - top-level model wins when it appears first.
#[test]
fn extract_model_top_level_preferred_when_first() {
    let body = br#"{"model":"real","messages":[{"model":"x"}]}"#;
    assert_eq!(extract_model(body), Some(&b"real"[..]));
}

// T5.6 (REGRESSION) - a nested decoy that textually precedes the top-level key
// MUST be ignored. The previous implementation returned the decoy, which meant
// Hydra authorized on one model while the provider served another; reproduced
// live against SiliconFlow as a full whitelist bypass.
#[test]
fn extract_model_nested_decoy_is_ignored() {
    let body = br#"{"messages":[{"model":"x"}],"model":"real"}"#;
    assert_eq!(extract_model(body), Some(&b"real"[..]));

    let body = br#"{"metadata":{"model":"allowed-model"},"model":"expensive-model"}"#;
    assert_eq!(extract_model(body), Some(&b"expensive-model"[..]));
}

// T5.7 - empty / very short input never panics.
#[test]
fn extract_model_short_input_no_panic() {
    assert_eq!(extract_model(b""), None);
    assert_eq!(extract_model(b"{"), None);
    assert_eq!(extract_model(b"\"model\""), None);
    assert_eq!(extract_model(b"\"model\":"), None);
    assert_eq!(extract_model(b"\"model\" :"), None);
    assert_eq!(extract_model(b"\"model\": 1"), None);
    assert_eq!(extract_model(b"\"model\":\""), None);
    assert_eq!(extract_model_field(b"{"), ModelField::Absent);
}

// T5.8 - extraction never reads past the given slice (a first chunk is enough).
#[test]
fn extract_model_first_chunk_only() {
    // A first chunk containing the root model suffices; no full-body scan.
    let first_chunk = br#"{"model":"claude-3-opus""#;
    assert_eq!(extract_model(first_chunk), Some(&b"claude-3-opus"[..]));

    // Truncated right after the value comma, still complete enough.
    let chunk = br#"{"model":"qwen","#;
    assert_eq!(extract_model(chunk), Some(&b"qwen"[..]));
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
        // The convenience view still collapses it to None...
        assert_eq!(extract_model(body), None);
    }
}

// REGRESSION (live variant A) - a nested non-string decoy must NOT stop the
// scan from reaching the real root member.
#[test]
fn nested_non_string_decoy_does_not_hide_the_root_model() {
    let body = br#"{"metadata":{"model":123},"model":"expensive-model"}"#;
    assert_eq!(extract_model(body), Some(&b"expensive-model"[..]));
}

// ---------------------------------------------------------------------------
// Structural cases the depth/string tracker has to get right.
// ---------------------------------------------------------------------------

// Escaped quotes inside a preceding string value must not end it early.
#[test]
fn escaped_quotes_in_a_preceding_string_are_handled() {
    let body = br#"{"note":"said \"model\":\"x\"","model":"real"}"#;
    assert_eq!(extract_model(body), Some(&b"real"[..]));
}

// Braces/brackets inside strings must not change the nesting depth.
#[test]
fn braces_inside_strings_do_not_confuse_depth() {
    let body = br#"{"a":"}","model":"real"}"#;
    assert_eq!(extract_model(body), Some(&b"real"[..]));

    let body = br#"{"a":["]","{",{"model":"decoy"}],"model":"real"}"#;
    assert_eq!(extract_model(body), Some(&b"real"[..]));
}

// Deeply nested decoys are skipped at any depth.
#[test]
fn deeply_nested_decoys_are_skipped() {
    let body = br#"{"a":{"b":{"c":{"model":"decoy"}}},"model":"real"}"#;
    assert_eq!(extract_model(body), Some(&b"real"[..]));
}

// A non-object root has no top-level member.
#[test]
fn non_object_root_is_absent() {
    assert_eq!(
        extract_model_field(br#"[{"model":"x"}]"#),
        ModelField::Absent
    );
    assert_eq!(extract_model_field(br#""model""#), ModelField::Absent);
    assert_eq!(extract_model_field(b"not json at all"), ModelField::Absent);
}

// Unterminated strings are Absent, never a panic and never a bogus slice.
#[test]
fn unterminated_input_is_absent() {
    assert_eq!(extract_model_field(br#"{"model":"#,), ModelField::Absent);
    assert_eq!(
        extract_model_field(br#"{"a":"unterminated"#),
        ModelField::Absent
    );
    assert_eq!(
        extract_model_field(br#"{"model":"unterminated"#),
        ModelField::Absent
    );
}

// The root model is found even when every other member precedes it.
#[test]
fn root_model_found_after_other_members() {
    let body = br#"{"a":1,"b":{"model":"decoy"},"c":[1,2,3],"d":"x","model":"real"}"#;
    assert_eq!(extract_model(body), Some(&b"real"[..]));
}
