//! Zero-copy metadata extraction — request side (pure).
//!
//! ## Contract
//!
//! `extract_model_field(&[u8]) -> ModelField<'_>` returns the **top-level**
//! `"model"` member of a JSON object body as a borrowed slice: no JSON
//! deserialisation, no allocation, one linear pass.
//!
//! ## Why top-level is a security property, not a convenience
//!
//! The extracted value drives AUTHORIZATION (the tenant `tenant_models`
//! whitelist and model to provider routing) while the request body is
//! forwarded upstream **verbatim** — the provider parses its own top-level
//! `"model"`. Extraction is therefore only sound if it returns the same member
//! the provider will read.
//!
//! A naive first-textual-match scan is NOT sound, and the difference is
//! exploitable by any authenticated tenant:
//!
//! ```text
//! {"metadata":{"model":123},             "model":"expensive"}
//!   -> the scan wanted a string, found none, and the request fell through to
//!      the model-less passthrough, which never consults tenant_models
//!
//! {"metadata":{"model":"allowed"},       "model":"expensive"}
//!   -> Hydra authorized on the decoy while the provider served the real one
//! ```
//!
//! Both were reproduced against a live cluster. The scanner below walks the
//! body once, tracking brace/bracket depth and JSON string state (escapes
//! included), and only accepts a `"model"` member at **depth 1** — the root
//! object. Decoys nested in `metadata`, `messages` or any other member are
//! skipped, and a top-level `"model"` whose value is not a string is reported
//! as [`ModelField::NotAString`] so the caller can reject it instead of
//! silently falling back to an unauthenticated passthrough.

/// One `"model"` member of the root JSON object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelField<'a> {
    /// The root object has no `"model"` member at all.
    Absent,
    /// The root `"model"` is a JSON string. The slice is the **raw** bytes
    /// between the quotes (escape sequences are not decoded, so the slice stays
    /// a zero-copy borrow of the input).
    Value(&'a [u8]),
    /// The root object has a `"model"` member whose value is not a JSON string
    /// (number, `null`, bool, array or object). Callers must NOT treat this as
    /// "no model": that equivalence is exactly the bypass described above.
    NotAString,
}

/// Index of the `"` that closes the JSON string whose content starts at
/// `start` (i.e. `start` is the byte *after* the opening quote). `None` when
/// the string is unterminated.
fn string_close(body: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    while let Some(&b) = body.get(i) {
        match b {
            b'\\' => i += 2, // skip the escaped byte (runs past end => None)
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Advance `i` past ASCII whitespace.
fn skip_ws(body: &[u8], i: &mut usize) {
    while let Some(&b) = body.get(*i) {
        if b.is_ascii_whitespace() {
            *i += 1;
        } else {
            break;
        }
    }
}

/// Extract the root object's `"model"` member.
///
/// Every index is bounds-checked via `slice::get`, so empty, truncated or
/// malformed input returns [`ModelField::Absent`] and never panics.
#[must_use]
pub fn extract_model_field(body: &[u8]) -> ModelField<'_> {
    let n = body.len();
    let mut i = 0usize;

    // The root must be a JSON object; anything else has no top-level member.
    skip_ws(body, &mut i);
    if body.get(i) != Some(&b'{') {
        return ModelField::Absent;
    }

    let mut depth: u32 = 0;
    // True while, at depth 1, the next string token would be a member KEY.
    let mut expect_key = false;

    while i < n {
        let b = body[i];
        match b {
            b'"' => {
                let Some(close) = string_close(body, i + 1) else {
                    return ModelField::Absent; // unterminated string
                };
                if depth == 1 && expect_key {
                    let is_model = &body[i + 1..close] == b"model";
                    let mut j = close + 1;
                    skip_ws(body, &mut j);
                    if body.get(j) != Some(&b':') {
                        return ModelField::Absent; // malformed member
                    }
                    j += 1;
                    skip_ws(body, &mut j);
                    if is_model {
                        return match body.get(j) {
                            Some(&b'"') => match string_close(body, j + 1) {
                                Some(end) => ModelField::Value(&body[j + 1..end]),
                                None => ModelField::Absent,
                            },
                            Some(_) => ModelField::NotAString,
                            None => ModelField::Absent,
                        };
                    }
                    // A different member: resume scanning at its value.
                    expect_key = false;
                    i = j;
                    continue;
                }
                i = close + 1;
            }
            b'{' | b'[' => {
                depth += 1;
                if depth == 1 {
                    expect_key = true; // entering the root object
                }
                i += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return ModelField::Absent; // root closed, no "model" seen
                }
                i += 1;
            }
            b',' => {
                if depth == 1 {
                    expect_key = true;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    ModelField::Absent
}

/// Convenience view of [`extract_model_field`] for callers that only want a
/// well-formed string value.
///
/// This collapses [`ModelField::Absent`] and [`ModelField::NotAString`] into
/// `None`. Authorization call sites must use [`extract_model_field`] directly,
/// so that a non-string `"model"` is rejected rather than treated as absent.
#[must_use]
pub fn extract_model(body: &[u8]) -> Option<&[u8]> {
    match extract_model_field(body) {
        ModelField::Value(v) => Some(v),
        ModelField::Absent | ModelField::NotAString => None,
    }
}
