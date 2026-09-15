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
    /// The root object's `"model"` member is **ambiguous**: the top level either
    /// carries a `"model"` key more than once, or a top-level key holds an
    /// escape sequence the zero-copy scan cannot decode (it may be an alias for
    /// `"model"`, e.g. `{"\u006dodel":"a"}`). We cannot determine which member
    /// the provider will read, so callers MUST reject the request instead of
    /// authorizing on a guess.
    Ambiguous,
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
/// malformed input never panics. A truncated body (the root object was not
/// closed) returns the first model seen so far, or [`ModelField::Absent`] when
/// none was seen.
///
/// ## Why the scan runs to the END (no first-hit early exit)
///
/// The extracted value drives authorization while the body is forwarded
/// upstream verbatim, so it must be the EXACT member the provider will read.
/// A top-level `"model"` therefore only resolves unambiguously once the whole
/// root object has been seen:
///
/// - a SECOND top-level `"model"` key is [`ModelField::Ambiguous`] — JSON
///   parsers disagree on which duplicate wins (first vs last), so the provider
///   may read a different model than Hydra authorized;
/// - a top-level key carrying an escape sequence is [`ModelField::Ambiguous`] —
///   it cannot be decoded zero-copy and may be an alias for `"model"`
///   (e.g. `{"\u006dodel":"a"}`).
///
/// **Cost note:** this means one full linear pass over the body instead of
/// stopping at the first `"model"`. That is acceptable: the caller bounds the
/// body by the 413 hard cap (see `proxy.rs`, `max_request_body_hard`) before
/// calling here, and the pass is zero-copy (no allocation, one `memchr`-free
/// byte scan with the same O(1) per-byte work as before).
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
    // Whether a top-level `"model"` key has already been seen. A second one is
    // ambiguous (JSON parsers disagree on which duplicate wins), which is why
    // we scan to the end instead of returning on the first hit.
    let mut seen_model = false;
    // The value of the FIRST top-level `"model"` key, captured for the final
    // result. We keep scanning after capturing it so a later duplicate or an
    // escaped-key alias anywhere in the root object is still detected.
    let mut model_result: Option<ModelField<'_>> = None;

    while i < n {
        let b = body[i];
        match b {
            b'"' => {
                let Some(close) = string_close(body, i + 1) else {
                    return ModelField::Absent; // unterminated string
                };
                if depth == 1 && expect_key {
                    let key = &body[i + 1..close];
                    // A top-level key with an escape sequence cannot be decoded
                    // zero-copy and may be an alias for "model"
                    // (e.g. {"\u006dodel":"a"}). Fail closed.
                    if key.contains(&b'\\') {
                        return ModelField::Ambiguous;
                    }
                    let is_model = key == b"model";
                    let mut j = close + 1;
                    skip_ws(body, &mut j);
                    if body.get(j) != Some(&b':') {
                        return ModelField::Absent; // malformed member
                    }
                    j += 1;
                    skip_ws(body, &mut j);
                    if is_model {
                        let value = match body.get(j) {
                            Some(&b'"') => match string_close(body, j + 1) {
                                Some(end) => ModelField::Value(&body[j + 1..end]),
                                None => ModelField::Absent,
                            },
                            Some(_) => ModelField::NotAString,
                            None => ModelField::Absent,
                        };
                        // A SECOND top-level "model" key: we cannot know which
                        // one the provider reads → ambiguous.
                        if seen_model {
                            return ModelField::Ambiguous;
                        }
                        seen_model = true;
                        model_result = Some(value);
                    }
                    // Resume scanning at the member's value (whether or not it
                    // was the model key) so the rest of the body is still seen.
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
                    // Root closed: the whole body was scanned, so the captured
                    // first model (if any) is the definitive answer.
                    return match model_result {
                        Some(r) => r,
                        None => ModelField::Absent,
                    };
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
    // The body ended before the root object closed (truncated — e.g. only a
    // first chunk was available). Return the model seen so far, else Absent.
    match model_result {
        Some(r) => r,
        None => ModelField::Absent,
    }
}

/// Convenience view of [`extract_model_field`] for callers that only want a
/// well-formed string value.
///
/// This collapses [`ModelField::Absent`], [`ModelField::NotAString`] and
/// [`ModelField::Ambiguous`] into `None`. Authorization call sites must use
/// [`extract_model_field`] directly, so that a non-string or ambiguous
/// `"model"` is rejected rather than treated as absent.
#[must_use]
pub fn extract_model(body: &[u8]) -> Option<&[u8]> {
    match extract_model_field(body) {
        ModelField::Value(v) => Some(v),
        ModelField::Absent | ModelField::NotAString | ModelField::Ambiguous => None,
    }
}
