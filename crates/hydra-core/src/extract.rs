//! Zero-copy, strict model extraction — request side (pure).
//!
//! ## Contract
//!
//! `extract_model_field(&[u8]) -> ModelField<'_>` returns the **top-level**
//! `"model"` member of a JSON object body, with JSON string escapes **decoded**
//! exactly as the provider's own parser decodes them. It stream-parses the body
//! with `serde_json`, borrowing the input buffer: no `serde_json::Value` tree is
//! built, and the model string is materialised (decoded/allocated) only when it
//! actually contains escape sequences — a clean value stays a zero-copy borrow.
//!
//! ## Why top-level + decoded is a security property, not a convenience
//!
//! The extracted value drives AUTHORIZATION (the tenant `tenant_models`
//! whitelist and model-to-provider routing) while the request body is forwarded
//! upstream **verbatim** — the provider parses its own top-level `"model"` and
//! decodes its string escapes. Extraction is therefore only sound if it returns
//! the same, equally-decoded member the provider will read.
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
//!
//! {"model":"a\"b"}
//!   -> the raw bytes `a\"b` are not what the provider reads; it decodes the
//!      escape and reads `a"b`. Authorizing on the raw bytes would mismatch the
//!      model the provider actually serves.
//! ```
//!
//! The first two were reproduced against a live cluster. The parser below
//! stream-decodes the root object with `serde_json`, tracking the top level and
//! only accepting a `"model"` member at the root (decoys nested in `metadata`,
//! `messages` or any other member are skipped). A top-level `"model"` whose
//! value is not a string is [`ModelField::NotAString`]; a duplicated top-level
//! `"model"` (including one whose key is an escape alias such as
//! `{"\u006dodel":"a"}`) is [`ModelField::Ambiguous`].
//!
//! ## Semantics vs the former hand-written scanner
//!
//! This is a strict serde-based re-implementation. The observable differences
//! are all in the direction of failing closed:
//!
//! - **Malformed bodies** (an unclosed root object, trailing content after the
//!   value, a non-object root, or a string whose escapes serde_json rejects)
//!   return [`ModelField::Malformed`] — a distinct state from [`Absent`], so a
//!   caller cannot mistake "Hydra could not parse this body" for "the body has
//!   no `model` member". The two differ in consequence: `Absent` is the
//!   documented model-less passthrough (no whitelist), `Malformed` must be
//!   rejected. (The old scanner returned the first model it had seen for a
//!   truncated body — it never validated the body at all.)
//! - **Escapes are decoded**: a `"model"` value such as `"gpt\u002d4"` is
//!   authorized as `gpt-4`, matching the provider. The old scanner returned the
//!   raw, un-decoded bytes.
//! - **Escaped top-level keys are decoded**: a key such as `"\u006dodel"` is
//!   recognized as `"model"` (the provider decodes it too). The old scanner
//!   could not decode keys, so such a body was simply not attributed to
//!   `"model"` at all.
//!
//! [`Absent`]: ModelField::Absent
//! [`Malformed`]: ModelField::Malformed

use std::borrow::Cow;
use std::fmt;

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;

/// The decoded `"model"` value of the root object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelValue<'a> {
    /// The JSON string contained no escape sequences: a zero-copy borrow of the
    /// input buffer (no decode, no allocation).
    Borrowed(&'a str),
    /// The JSON string contained escape sequences: the decoded content. This is
    /// the only case where a model string is materialised/allocated.
    Decoded(String),
}

impl ModelValue<'_> {
    /// The decoded model content, as a borrowed view.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            ModelValue::Borrowed(s) => s,
            ModelValue::Decoded(s) => s.as_str(),
        }
    }

    /// The decoded model content, as an owned [`String`].
    ///
    /// The caller (routing/whitelist) needs an owned string; for the zero-copy
    /// [`ModelValue::Borrowed`] case this is the one allocation that copies the
    /// slice out of the input buffer.
    #[must_use]
    pub fn into_string(self) -> String {
        match self {
            ModelValue::Borrowed(s) => s.to_string(),
            ModelValue::Decoded(s) => s,
        }
    }
}

/// One `"model"` member of the root JSON object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelField<'a> {
    /// The root object is well-formed JSON and has no `"model"` member at all.
    /// This is the **only** state that means "no model to authorize": it feeds
    /// the documented model-less passthrough (`NonRouteStrategy`). A body Hydra
    /// could not parse is [`ModelField::Malformed`], never this.
    Absent,
    /// The root `"model"` is a JSON string. The content is the decoded string
    /// (escape sequences resolved), zero-copy borrowed when it had no escapes.
    Value(ModelValue<'a>),
    /// The root object has a `"model"` member whose value is not a JSON string
    /// (number, `null`, bool, array or object). Callers must NOT treat this as
    /// "no model": that equivalence is exactly the bypass described above.
    NotAString,
    /// The root object's `"model"` member is **ambiguous**: the top level
    /// carries a `"model"` key more than once, counting keys that only decode
    /// to `"model"` (e.g. `{"\u006dodel":"a","model":"b"}`). JSON parsers
    /// disagree on which duplicate wins, so the provider may read a different
    /// model than Hydra authorized; callers MUST reject the request.
    Ambiguous,
    /// The body is not a single well-formed JSON **object**: a parse error, a
    /// truncated/unterminated value, trailing content after the value, a
    /// non-object root, or a string escape serde_json rejects.
    ///
    /// Callers MUST reject this and must NOT treat it as [`ModelField::Absent`].
    /// The two are deliberately different states: `Absent` leads to the
    /// model-less passthrough (which never consults the tenant model
    /// whitelist), while the body is still forwarded upstream verbatim — so
    /// conflating them let any body Hydra could not parse skip authorization
    /// entirely, while the provider parsed its own `"model"` and served it.
    Malformed,
}

/// The result of streaming the root object's members. `Deserialize` borrows the
/// input (`'de`) so a no-escape `model` value can be returned as a zero-copy
/// slice of the body.
enum RootParse<'a> {
    Absent,
    Value(ModelValue<'a>),
    NotAString,
    Ambiguous,
}

impl<'de> Deserialize<'de> for RootParse<'de> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // The root must be a JSON object. A non-object root makes serde_json
        // report an (invalid-type) error, which the caller collapses to Absent.
        deserializer.deserialize_map(RootParseMapVisitor)
    }
}

struct RootParseMapVisitor;

impl<'de> Visitor<'de> for RootParseMapVisitor {
    type Value = RootParse<'de>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a JSON object (the request body root)")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        // Track the root object's "model" member across the ENTIRE map (no
        // early exit): the whole body must be validated as JSON, and a
        // duplicate "model" anywhere in the root must still be detected.
        let mut model_str: Option<ModelValue<'de>> = None;
        let mut not_a_string = false;
        let mut ambiguous = false;

        while let Some(key) = map.next_key::<Cow<'de, str>>()? {
            // Keys are deserialized as `Cow<'de, str>`: a clean key is a
            // zero-copy borrow, and only a key that actually carries an escape
            // sequence is decoded (allocated) so it can be compared against
            // "model". This is what lets an escaped alias like "\u006dodel" be
            // recognized as "model" (the provider's parser decodes it too),
            // while a clean key stays zero-copy.
            if key.as_ref() != "model" {
                // A decoy: never used for authorization. Ignore its value
                // (IgnoredAny discards without allocating).
                let _ = map.next_value::<de::IgnoredAny>()?;
                continue;
            }
            // A top-level "model" member. Distinguish string vs non-string.
            let value = map.next_value::<ModelValueAny<'de>>()?;
            match value {
                ModelValueAny::Str(mv) => {
                    // A second top-level "model" member (whatever its type) is
                    // ambiguous: JSON parsers disagree on which duplicate wins.
                    if model_str.is_some() || not_a_string {
                        ambiguous = true;
                    } else {
                        model_str = Some(mv);
                    }
                }
                ModelValueAny::NotAString => {
                    if model_str.is_some() || not_a_string {
                        ambiguous = true;
                    } else {
                        not_a_string = true;
                    }
                }
            }
        }

        if ambiguous {
            Ok(RootParse::Ambiguous)
        } else if let Some(mv) = model_str {
            Ok(RootParse::Value(mv))
        } else if not_a_string {
            Ok(RootParse::NotAString)
        } else {
            Ok(RootParse::Absent)
        }
    }
}

/// A model member's value: a decoded string, or "not a string".
///
/// Deserialized via `deserialize_any` so the ACTUAL JSON token decides the
/// branch: a string lands in [`ModelValueAny::Str`], a number/`null`/bool/
/// array/object lands in [`ModelValueAny::NotAString`]. (Using `deserialize_str`
/// would surface a non-string value as an undistinguishable type error, which
/// we would be forced to map to Absent — exactly the bypass this type exists to
/// prevent.)
enum ModelValueAny<'a> {
    Str(ModelValue<'a>),
    NotAString,
}

impl<'de> Deserialize<'de> for ModelValueAny<'de> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(ModelValueAnyVisitor)
    }
}

struct ModelValueAnyVisitor;

impl<'de> Visitor<'de> for ModelValueAnyVisitor {
    type Value = ModelValueAny<'de>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("the root \"model\" value")
    }

    // No escape sequences: serde_json borrows the string straight from the
    // input — zero-copy.
    fn visit_borrowed_str<E: de::Error>(self, s: &'de str) -> Result<Self::Value, E> {
        Ok(ModelValueAny::Str(ModelValue::Borrowed(s)))
    }

    // Has escape sequences: `s` borrows the deserializer's scratch buffer,
    // which is reused for the next value, so it must be copied out. This is the
    // only path where a model string is decoded/allocated. (serde_json delivers
    // strings via `visit_borrowed_str` or `visit_str`; `visit_string` is left at
    // its default and never fires for a `&[u8]` deserializer.)
    fn visit_str<E: de::Error>(self, s: &str) -> Result<Self::Value, E> {
        Ok(ModelValueAny::Str(ModelValue::Decoded(s.to_string())))
    }

    // Non-string model values: report NotAString. For containers we must still
    // consume the elements to keep the deserializer in sync; IgnoredAny discards
    // them without allocating.
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(ModelValueAny::NotAString) // null
    }
    fn visit_bool<E: de::Error>(self, _b: bool) -> Result<Self::Value, E> {
        Ok(ModelValueAny::NotAString)
    }
    fn visit_i64<E: de::Error>(self, _n: i64) -> Result<Self::Value, E> {
        Ok(ModelValueAny::NotAString)
    }
    fn visit_u64<E: de::Error>(self, _n: u64) -> Result<Self::Value, E> {
        Ok(ModelValueAny::NotAString)
    }
    fn visit_i128<E: de::Error>(self, _n: i128) -> Result<Self::Value, E> {
        Ok(ModelValueAny::NotAString)
    }
    fn visit_u128<E: de::Error>(self, _n: u128) -> Result<Self::Value, E> {
        Ok(ModelValueAny::NotAString)
    }
    fn visit_f64<E: de::Error>(self, _n: f64) -> Result<Self::Value, E> {
        Ok(ModelValueAny::NotAString)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<de::IgnoredAny>()?.is_some() {}
        Ok(ModelValueAny::NotAString)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        while map
            .next_entry::<de::IgnoredAny, de::IgnoredAny>()?
            .is_some()
        {}
        Ok(ModelValueAny::NotAString)
    }
}

/// Extract the root object's `"model"` member.
///
/// The body is stream-parsed with `serde_json`, borrowing the input. Anything
/// that is not one well-formed JSON **object** — a parse error, a truncated or
/// unterminated value, trailing content after the value, a non-object root, or
/// a string escape serde_json rejects — returns [`ModelField::Malformed`].
/// [`ModelField::Absent`] is returned only for a well-formed root object that
/// simply has no `"model"` member.
///
/// **Cost note (measured):** this is one full linear pass over the body, with
/// NO first-hit early exit, because a top-level `"model"` only resolves
/// unambiguously once the whole root object has been seen (a later duplicate
/// must still be detected). Cost scales with the body, not with where `model`
/// sits: measured in release, ~0.09 ms/MB for a body whose payload is one large
/// string and up to ~1.3 ms/MB for deeply nested or wide objects (the old
/// hand-written scanner returned at the `model` member, i.e. tens of
/// nanoseconds, so this is a real change in cost structure — for a typical
/// 3-50 KB request it is a few microseconds). The caller bounds the body by the
/// 413 hard cap (32 MiB) before calling here. A clean (escape-free) `model`
/// value stays a zero-copy borrow; escaped values/keys and serde_json's own
/// skipped-value scratch are the only allocations.
#[must_use]
pub fn extract_model_field(body: &[u8]) -> ModelField<'_> {
    let mut de = serde_json::Deserializer::from_slice(body);
    // Borrow `de` mutably for the parse (so it stays usable for `end`), then
    // require the whole body to be exactly one JSON value (no trailing garbage).
    let parsed = RootParse::deserialize(&mut de).and_then(|p| de.end().map(|()| p));
    match parsed {
        Ok(RootParse::Value(v)) => ModelField::Value(v),
        Ok(RootParse::NotAString) => ModelField::NotAString,
        Ok(RootParse::Ambiguous) => ModelField::Ambiguous,
        Ok(RootParse::Absent) => ModelField::Absent,
        // A non-object root also lands here: `RootParse` deserializes the root
        // as a map, so serde_json reports an invalid-type error for an array,
        // string, number, bool or `null` root.
        Err(_) => ModelField::Malformed,
    }
}

/// Convenience view of [`extract_model_field`] for callers that only want a
/// well-formed string value.
///
/// **Lossy by construction** — it collapses [`ModelField::Absent`],
/// [`ModelField::NotAString`], [`ModelField::Ambiguous`] and
/// [`ModelField::Malformed`] into one `None`, which is exactly the equivalence
/// that must NOT drive authorization. Authorization call sites must use
/// [`extract_model_field`] directly (see `proxy.rs`), so that a non-string,
/// ambiguous or unparsable body is rejected instead of treated as absent. The
/// name carries the warning on purpose.
#[must_use]
pub fn extract_model_lossy(body: &[u8]) -> Option<String> {
    match extract_model_field(body) {
        ModelField::Value(v) => Some(v.into_string()),
        ModelField::Absent
        | ModelField::NotAString
        | ModelField::Ambiguous
        | ModelField::Malformed => None,
    }
}
