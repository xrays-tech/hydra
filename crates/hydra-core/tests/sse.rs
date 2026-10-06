//! T5.9–T5.25 — `sse::UsageScanner` zero-copy response-side usage scanning.
//!
//! Each chunk is scanned with `memchr` for `"usage"` (zero-alloc, ~10 GB/s).
//! On a hit, only the ~50-byte `data:` payload (or the brace-matched usage
//! object for non-stream JSON) is deserialised. `ProviderKind` drives schema
//! normalisation; Anthropic usage fields are cumulative (last-wins).
//! `data: [DONE]` terminates.
//!
//! The normalised result uses provider-NEUTRAL names (design §9.5):
//! `tokens_in` / `tokens_out` / `cache_hit_tokens` — the JSON payloads below
//! deliberately keep the raw provider field names (`prompt_tokens`,
//! `input_tokens`, …) to prove the mapping.

use hydra_core::model::{ProviderKind, Usage};
use hydra_core::sse::{ScanResult, UsageScanner};

// T5.9 — no `"usage"` in the chunk ⇒ Skip, no deserialisation, no allocation.
#[test]
fn usage_scan_no_usage_skips_zero_alloc() {
    let mut scanner = UsageScanner::new(ProviderKind::OpenAi);
    let chunk = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n";
    assert_eq!(scanner.scan_chunk(chunk), ScanResult::Skip);
    assert_eq!(scanner.finalize(), None);
}

// T5.10 — a chunk containing `"usage"` ⇒ Found and only that chunk is parsed.
#[test]
fn usage_scan_finds_usage_memchr() {
    let mut scanner = UsageScanner::new(ProviderKind::OpenAi);
    let chunk = b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":4,\"total_tokens\":7}}\n";
    assert_eq!(scanner.scan_chunk(chunk), ScanResult::Found);
    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            tokens_in: Some(3),
            tokens_out: Some(4),
            ..Default::default()
        })
    );
}

// T5.11 — OpenAI final chunk carries the authoritative usage object.
#[test]
fn usage_openai_final_chunk() {
    let mut scanner = UsageScanner::new(ProviderKind::OpenAi);
    // Stream of content chunks, then the final usage-bearing chunk.
    scanner.scan_chunk(b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n");
    let final_chunk = b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":50,\"total_tokens\":150}}\n";
    assert_eq!(scanner.scan_chunk(final_chunk), ScanResult::Found);
    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            tokens_in: Some(100),
            tokens_out: Some(50),
            ..Default::default()
        })
    );
}

// T5.12 — Anthropic message_delta: input_tokens→tokens_in, output_tokens→tokens_out.
#[test]
fn usage_anthropic_message_delta() {
    let mut scanner = UsageScanner::new(ProviderKind::Anthropic);
    let chunk = b"event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":12,\"output_tokens\":8}}\n\n";
    assert_eq!(scanner.scan_chunk(chunk), ScanResult::Found);
    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            tokens_in: Some(12),
            tokens_out: Some(8),
            // Anthropic reports no cache-hit field here ⇒ cache_hit_tokens None.
            ..Default::default()
        })
    );
}

// T5.13 — Anthropic cumulative usage: output_tokens is a running total, so
// last-wins (not delta-sum) is correct across message_delta events.
#[test]
fn usage_anthropic_cumulative_last_wins() {
    let mut scanner = UsageScanner::new(ProviderKind::Anthropic);
    // message_start establishes input_tokens and an initial output_tokens=1.
    scanner.scan_chunk(
        b"event: message_start\ndata: {\"type\":\"message_start\",\"usage\":{\"input_tokens\":40,\"output_tokens\":1}}\n\n",
    );
    // message_delta carries the CUMULATIVE output_tokens (not a delta):
    // 5, then 12 — the running total, not 5+3+2.
    scanner.scan_chunk(
        b"event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n\n",
    );
    scanner.scan_chunk(
        b"event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":12}}\n\n",
    );

    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            tokens_in: Some(40),  // single input_tokens occurrence
            tokens_out: Some(12), // last-wins: 12, NOT 1+5+12=18
            ..Default::default()
        })
    );
}

// T5.14 — `data: [DONE]` terminates the stream; later bytes are ignored.
#[test]
fn usage_done_marker() {
    let mut scanner = UsageScanner::new(ProviderKind::OpenAi);
    let usage = b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n";
    scanner.scan_chunk(usage);
    assert_eq!(scanner.scan_chunk(b"data: [DONE]\n"), ScanResult::Done);
    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            tokens_in: Some(1),
            tokens_out: Some(1),
            ..Default::default()
        })
    );
}

// T5.15 — `"usage"` line split across two chunks ⇒ tail buffer rejoins it.
#[test]
fn usage_cross_chunk_boundary() {
    let mut scanner = UsageScanner::new(ProviderKind::OpenAi);
    // First chunk ends mid-`data:` line, before `"usage"` is complete.
    scanner.scan_chunk(b"data: {\"choices\":[]}\ndata: {\"usa");
    // Second chunk completes the usage payload.
    let result = scanner
        .scan_chunk(b"ge\":{\"prompt_tokens\":9,\"completion_tokens\":1,\"total_tokens\":10}}\n");
    assert_eq!(result, ScanResult::Found);
    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            tokens_in: Some(9),
            tokens_out: Some(1),
            ..Default::default()
        })
    );
}

// T5.16 — non-streaming JSON: a single full JSON object (no SSE framing).
#[test]
fn usage_non_stream_json() {
    let mut scanner = UsageScanner::new(ProviderKind::OpenAi);
    let body = b"{\"id\":\"chatcmpl-1\",\"object\":\"chat.completion\",\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":14,\"total_tokens\":21}}";
    assert_eq!(scanner.scan_chunk(body), ScanResult::Found);
    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            tokens_in: Some(7),
            tokens_out: Some(14),
            ..Default::default()
        })
    );
}

/// A non-SSE `application/json` response whose usage object is split across two
/// reads must still be accounted.
///
/// The scanner's tail only activated for `data:`-framed lines, so for a plain
/// JSON body the first chunk held `"usage"` (but no closing brace) and the second
/// chunk held the rest — with no `"usage"` in it — so nothing ever absorbed the
/// object and the tokens were NULL for a successful request.
#[test]
fn non_sse_usage_split_across_reads_is_reassembled() {
    let mut s = UsageScanner::new(ProviderKind::OpenAi);
    // Split INSIDE the usage object, and — crucially — so that the second read
    // contains no `"usage"` key at all.
    assert_eq!(
        s.scan_chunk(b"{\"id\":\"x\",\"usage\":{\"prompt_tokens\":123"),
        ScanResult::Skip,
        "nothing complete yet"
    );
    s.scan_chunk(b"4,\"completion_tokens\":5}}");
    assert_eq!(
        s.finalize(),
        Some(Usage {
            tokens_in: Some(1234),
            tokens_out: Some(5),
            cache_hit_tokens: None,
        }),
        "the split usage object must be reassembled, not silently dropped"
    );
}

/// The tail is BOUNDED: a huge non-streaming body must not make the scanner hold
/// (and re-scan) an unbounded buffer.
#[test]
fn a_huge_incomplete_json_usage_is_not_buffered_forever() {
    let mut s = UsageScanner::new(ProviderKind::OpenAi);
    // `"usage"` early, then MORE THAN the cap with no closing brace: the tail
    // candidate is the rest of the buffer, i.e. over the cap, so it is dropped.
    let mut body = Vec::from(&b"{\"usage\":{\"prompt_tokens\":1"[..]);
    body.extend(std::iter::repeat_n(b'a', 200 * 1024));
    s.scan_chunk(&body);
    s.scan_chunk(b"2,\"completion_tokens\":3}}");
    assert_eq!(
        s.finalize(),
        None,
        "an over-cap tail must be dropped, not accumulated"
    );
}

/// The `MAX_JSON_TAIL` bound must apply to the SSE path too, not just to a
/// non-streaming JSON body.
///
/// The cap was checked only on the non-SSE branch, so a single unterminated
/// `data:` line longer than 64 KiB was retained **in full** and re-scanned by
/// every subsequent `scan_chunk` — the quadratic work the cap exists to prevent,
/// advertised as bounded. An over-cap tail is now dropped.
///
/// Falsification: delete the `trailing.len() > MAX_JSON_TAIL` check in
/// `buffer_incomplete_tail` and this returns `Some(12/5)` instead of `None`.
#[test]
fn an_oversized_unterminated_sse_tail_is_dropped_rather_than_retained() {
    let mut s = UsageScanner::new(ProviderKind::OpenAi);
    // The padding sits INSIDE a JSON string so the completed object stays
    // parseable: padding a bare number (`1aaaa…`) would make the JSON invalid and
    // the test would pass for the wrong reason — it must fail *because* the tail
    // was dropped, not because the payload was unparseable.
    let mut first = Vec::from(&b"data: {\"usage\":{\"prompt_tokens\":1,\"pad\":\""[..]);
    first.extend(std::iter::repeat_n(b'a', 200 * 1024));
    s.scan_chunk(&first);
    // The rest of the object arrives on the next read. If the tail had been
    // retained, the reassembly would succeed; dropping it is the point.
    s.scan_chunk(b"\",\"completion_tokens\":5}}\n\n");
    assert_eq!(
        s.finalize(),
        None,
        "an over-cap SSE tail must be dropped, not carried and re-scanned forever"
    );
}

/// A SECOND usage object that omits fields must not erase the values an earlier
/// one carried.
///
/// The Anthropic branch only assigns `Some`; the OpenAI/Generic branch assigned
/// unconditionally, so `{"usage":{"completion_tokens":5}}` (or `{"usage":{}}`, or
/// a gateway's `{"usage":{"total_tokens":…}}`) reset `tokens_in` to `None`. The
/// totals then came out too LOW, i.e. quota and billing under-counted — the
/// fail-open direction — with no metric to notice it.
///
/// Falsification: restore the unconditional assignments and `tokens_in` is `None`.
#[test]
fn a_later_partial_usage_object_does_not_erase_earlier_values() {
    let mut s = UsageScanner::new(ProviderKind::OpenAi);
    s.scan_chunk(b"data: {\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":22}}\n\n");
    s.scan_chunk(b"data: {\"usage\":{\"completion_tokens\":33}}\n\n");
    assert_eq!(
        s.finalize(),
        Some(Usage {
            tokens_in: Some(11),
            tokens_out: Some(33),
            cache_hit_tokens: None,
        }),
        "the second object must update what it carries and leave the rest alone"
    );
}

/// The schema is chosen by the REQUEST path but the field names come from
/// whichever upstream answered, so the OpenAI struct must also accept
/// Anthropic's cache spelling — otherwise cached tokens on such a route are
/// silently NULL.
#[test]
fn the_openai_schema_also_reads_the_anthropic_cache_field() {
    let mut s = UsageScanner::new(ProviderKind::OpenAi);
    s.scan_chunk(
        b"data: {\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"cache_read_input_tokens\":7}}\n\n",
    );
    assert_eq!(
        s.finalize(),
        Some(Usage {
            tokens_in: Some(1),
            tokens_out: Some(2),
            cache_hit_tokens: Some(7),
        }),
        "an Anthropic-styled upstream behind an OpenAI route must still report cache hits"
    );
}

// T5.17 — schema dispatch by ProviderKind from the same payload.
#[test]
fn usage_schema_dispatch_by_provider() {
    // Payload carries BOTH OpenAI-style and Anthropic-style field names.
    let payload =
        b"data: {\"usage\":{\"prompt_tokens\":7,\"input_tokens\":99,\"output_tokens\":5}}\n";

    let mut oai = UsageScanner::new(ProviderKind::OpenAi);
    oai.scan_chunk(payload);
    let oai_usage = oai.finalize().expect("openai usage");
    // OpenAI prefers prompt_tokens → tokens_in.
    assert_eq!(oai_usage.tokens_in, Some(7));

    let mut ant = UsageScanner::new(ProviderKind::Anthropic);
    ant.scan_chunk(payload);
    let ant_usage = ant.finalize().expect("anthropic usage");
    // Anthropic reads input_tokens/output_tokens.
    assert_eq!(ant_usage.tokens_in, Some(99));
    assert_eq!(ant_usage.tokens_out, Some(5));

    // A SCHEMA MISMATCH must not silently record NULL tokens. The scanner is
    // picked by the request path (a client POSTing `/v1/messages` gets the
    // Anthropic schema, which also decides the credential header), while the field
    // names come from the upstream. A `/v1/messages` request routed to an
    // OpenAI-compatible upstream used to parse the usage object, find none of the
    // three Anthropic fields, and still report `Some(Usage { all None })` — a 200
    // with tokens silently NULL in the meter.
    let openai_only = b"data: {\"usage\":{\"prompt_tokens\":1234,\"completion_tokens\":56}}\n";
    let mut mismatched = UsageScanner::new(ProviderKind::Anthropic);
    mismatched.scan_chunk(openai_only);
    let u = mismatched.finalize().expect("usage located");
    assert_eq!(
        u.tokens_in,
        Some(1234),
        "an Anthropic-path request answered by an OpenAI-field upstream must still \
         account its prompt tokens"
    );
    assert_eq!(u.tokens_out, Some(56));
    // …and the mirror image: an OpenAI-path request answered with Anthropic names.
    let anthropic_only = b"data: {\"usage\":{\"input_tokens\":11,\"output_tokens\":22}}\n";
    let mut mismatched2 = UsageScanner::new(ProviderKind::OpenAi);
    mismatched2.scan_chunk(anthropic_only);
    let u2 = mismatched2.finalize().expect("usage located");
    assert_eq!(u2.tokens_in, Some(11));
    assert_eq!(u2.tokens_out, Some(22));

    // Generic falls back to OpenAI-style field names.
    let mut generic = UsageScanner::new(ProviderKind::Generic);
    generic.scan_chunk(payload);
    let generic_usage = generic.finalize().expect("generic usage");
    assert_eq!(generic_usage.tokens_in, Some(7));
}

// T5.18 — malformed JSON containing `"usage"` is skipped without panicking.
#[test]
fn usage_malformed_chunk_skipped() {
    let mut scanner = UsageScanner::new(ProviderKind::OpenAi);
    // Garbage after "usage" that is not valid JSON.
    let malformed = b"data: {\"usage\":!!!not-json}\n";
    // No panic; the malformed line is skipped (no usage absorbed).
    assert_eq!(scanner.scan_chunk(malformed), ScanResult::Skip);
    // A separate scanner fed only the malformed chunk reports no usage.
    let mut only_bad = UsageScanner::new(ProviderKind::OpenAi);
    only_bad.scan_chunk(malformed);
    assert_eq!(only_bad.finalize(), None);

    // The original scanner survives and parses a subsequent valid chunk.
    let valid =
        b"data: {\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":2,\"total_tokens\":4}}\n";
    assert_eq!(scanner.scan_chunk(valid), ScanResult::Found);
    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            tokens_in: Some(2),
            tokens_out: Some(2),
            ..Default::default()
        })
    );
}

// T5.19 — OpenAI stream without `stream_options.include_usage` ⇒ finalize None.
#[test]
fn usage_openai_no_include_usage() {
    let mut scanner = UsageScanner::new(ProviderKind::OpenAi);
    scanner.scan_chunk(b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n");
    scanner.scan_chunk(b"data: {\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\n");
    scanner.scan_chunk(b"data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n");
    scanner.scan_chunk(b"data: [DONE]\n");
    // No usage ever observed.
    assert_eq!(scanner.finalize(), None);
}

// T5.20 — OpenAI `usage.prompt_tokens_details.cached_tokens` ⇒ cache_hit_tokens.
#[test]
fn usage_openai_cached_tokens_from_details() {
    let mut scanner = UsageScanner::new(ProviderKind::OpenAi);
    let chunk = b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":50,\"total_tokens\":150,\"prompt_tokens_details\":{\"cached_tokens\":42}}}\n";
    assert_eq!(scanner.scan_chunk(chunk), ScanResult::Found);
    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            tokens_in: Some(100),
            tokens_out: Some(50),
            cache_hit_tokens: Some(42),
        })
    );
}

// T5.20b — OpenAI usage WITHOUT `prompt_tokens_details` ⇒ cache_hit_tokens None.
#[test]
fn usage_openai_no_details_cached_tokens_none() {
    let mut scanner = UsageScanner::new(ProviderKind::OpenAi);
    let chunk = b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":50,\"total_tokens\":150}}\n";
    scanner.scan_chunk(chunk);
    let u = scanner.finalize().expect("usage");
    assert_eq!(u.cache_hit_tokens, None);
}

// T5.20c — Anthropic `cache_read_input_tokens` ⇒ cache_hit_tokens.
#[test]
fn usage_anthropic_cached_tokens() {
    let mut scanner = UsageScanner::new(ProviderKind::Anthropic);
    let chunk = b"event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":120,\"output_tokens\":30,\"cache_read_input_tokens\":77}}\n\n";
    assert_eq!(scanner.scan_chunk(chunk), ScanResult::Found);
    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            tokens_in: Some(120),
            tokens_out: Some(30),
            cache_hit_tokens: Some(77),
        })
    );
}

// T5.20d — Generic provider surfacing a top-level `cached_tokens`.
#[test]
fn usage_generic_top_level_cached_tokens() {
    let mut scanner = UsageScanner::new(ProviderKind::Generic);
    let chunk = b"data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15,\"cached_tokens\":8}}\n";
    scanner.scan_chunk(chunk);
    let u = scanner.finalize().expect("generic usage");
    assert_eq!(u.cache_hit_tokens, Some(8));
}

// T5.20e — OpenAI details present but `cached_tokens` key absent ⇒ None.
#[test]
fn usage_openai_details_without_cached_field() {
    let mut scanner = UsageScanner::new(ProviderKind::OpenAi);
    // `prompt_tokens_details` present with an unrelated field only.
    let chunk = b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"total_tokens\":5,\"prompt_tokens_details\":{\"audio_tokens\":1}}}\n";
    scanner.scan_chunk(chunk);
    let u = scanner.finalize().expect("usage");
    assert_eq!(u.cache_hit_tokens, None);
}

// ===========================================================================
// Anthropic coalesced-chunk regression (BUG 1 + BUG 2 fix).
//
// When multiple usage-bearing SSE events arrive in ONE TCP chunk, ALL must be
// parsed (BUG 1 fix). And Anthropic usage fields are cumulative → last-wins,
// so message_delta's output_tokens/cache_read_input_tokens override
// message_start's initial values (BUG 2 fix).
// ===========================================================================

// T5.21 — Anthropic message_start + message_delta COALESCED in one chunk.
// Before the BUG 1 fix only message_start's usage (input=42, output=1) was
// parsed; message_delta (output=13, cache_read=7) was silently dropped.
#[test]
fn usage_anthropic_coalesced_in_one_chunk() {
    let mut scanner = UsageScanner::new(ProviderKind::Anthropic);
    // A single chunk carrying two usage-bearing events back-to-back — exactly
    // what happens when a fast upstream or coalescing proxy delivers the whole
    // SSE body in one TCP segment.
    let chunk = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":42,\"output_tokens\":1}}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":13,\"cache_read_input_tokens\":7}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    assert_eq!(scanner.scan_chunk(chunk.as_bytes()), ScanResult::Found);
    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            tokens_in: Some(42),       // input_tokens from message_start
            tokens_out: Some(13),      // output_tokens: last-wins (13, not 1)
            cache_hit_tokens: Some(7), // cache_read_input_tokens from message_delta
        })
    );
}

// T5.22 — Same events fed as TWO separate chunks → identical result.
// Proves the coalesced (one-chunk) and separate (two-chunk) code paths agree.
#[test]
fn usage_anthropic_separate_chunks() {
    let mut scanner = UsageScanner::new(ProviderKind::Anthropic);
    scanner.scan_chunk(
        concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":42,\"output_tokens\":1}}}\n\n",
        )
        .as_bytes(),
    );
    scanner.scan_chunk(
        concat!(
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":13,\"cache_read_input_tokens\":7}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        )
        .as_bytes(),
    );
    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            tokens_in: Some(42),
            tokens_out: Some(13),
            cache_hit_tokens: Some(7),
        })
    );
}

// T5.23 — OpenAI/Generic single-chunk regression: the multi-line refactor must
// not change the OpenAI path (single usage object, last-wins, unchanged).
#[test]
fn usage_openai_single_chunk_unchanged() {
    let mut scanner = UsageScanner::new(ProviderKind::Generic);
    let chunk = b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n";
    assert_eq!(scanner.scan_chunk(chunk), ScanResult::Found);
    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            tokens_in: Some(10),
            tokens_out: Some(5),
            ..Default::default()
        })
    );
}

// T5.24 — Anthropic usage object split mid-JSON across two chunks: the tail
// buffer reassembles it, and message_delta's cumulative output_tokens still
// overrides message_start's initial value after reassembly.
#[test]
fn usage_anthropic_split_across_chunks() {
    let mut scanner = UsageScanner::new(ProviderKind::Anthropic);
    // Chunk 1: message_start fully received, message_delta's usage line cut
    // mid-JSON (no closing brace yet).
    scanner.scan_chunk(
        concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"usage\":{\"input_tokens\":42,\"output_tokens\":1}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":13,\"cache_read_input",
        )
        .as_bytes(),
    );
    // Chunk 2 completes the usage object.
    scanner
        .scan_chunk(b"_tokens\":7}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");

    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            tokens_in: Some(42),
            tokens_out: Some(13),
            cache_hit_tokens: Some(7),
        })
    );
}

// T5.25 — OpenAI usage object split mid-JSON across two chunks: the multi-line
// loop + tail buffer must not break the original cross-chunk reassembly path.
#[test]
fn usage_openai_split_across_chunks_no_double_count() {
    let mut scanner = UsageScanner::new(ProviderKind::OpenAi);
    // Chunk 1 carries a complete usage object on a `\n`-terminated line, then
    // starts a second data: line without a trailing newline.
    scanner.scan_chunk(
        b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\
         data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_to",
    );
    // Chunk 2 completes the second usage line. The first chunk already
    // buffered the incomplete trailing data: line via the tail.
    let result = scanner.scan_chunk(b"kens\":5,\"total_tokens\":15}}\n");
    assert_eq!(result, ScanResult::Found);
    assert_eq!(
        scanner.finalize(),
        Some(Usage {
            tokens_in: Some(10),
            tokens_out: Some(5),
            ..Default::default()
        })
    );
}
