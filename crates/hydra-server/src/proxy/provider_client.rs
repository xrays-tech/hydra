//! Upstream HTTP client for the **terminate-in-Pingora** proxy mode
//! (design-change `terminate-mode` §4.2).
//!
//! In terminate mode the proxy no longer hands the body off to Pingora's
//! upstream dialler. Instead `request_filter` reads the full downstream body,
//! resolves a route, then uses this client to issue the request to the chosen
//! provider itself, streaming the response back chunk-by-chunk through the
//! downstream `Session`.
//!
//! ## Why a *separate* reqwest client
//!
//! The proxy shell already owns two other reqwest clients — the auth-upstream
//! client ([`crate::http::HttpAuthChecker`]) and the breaker probe client
//! ([`crate::proxy::breaker_wrap::probe_task`]). Those use **short** timeouts
//! (sub-second to ~1.5 s) because they must fail fast. LLM chat-completion
//! requests — especially streaming SSE — can take **minutes** to first byte.
//! Sharing a short-timeout client would abort legitimate long generations, so
//! `ProviderClient` carries its own long-lived (300 s timeout) connection pool.
//!
//! ## Failover replay is free
//!
//! The request body is held as [`bytes::Bytes`] by the caller.
//! [`Bytes::clone`] is an O(1) refcount bump, so the failover loop can hand
//! the *same* body bytes to N candidate providers with zero memcpy — the
//! "body replay is free" property that the old stream-through design fought
//! Pingora's retry machinery to approximate (design §4.3).

use std::time::Duration;

use bytes::Bytes;
use hydra_core::model::{protocol_for_path, Provider, ProviderKind};
use hydra_core::rewrite::rewrite_path;
use pingora_http::RequestHeader;

use crate::proxy::peer::parse_endpoint;

/// Why sending an upstream request failed.
///
/// The distinction is LOAD-BEARING for failover: the proxy may only replay an
/// attempt when the upstream can PROVE it never saw the request. A connect
/// error is that proof; a first-byte timeout is the opposite — the request was
/// written, so a replay may double-bill.
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    /// No response headers within the first-byte bound. The request WAS sent,
    /// so the upstream may already have generated (and billed) a completion:
    /// treat it like any other post-send failure.
    #[error("upstream first byte timed out after {secs}s (request was already sent)")]
    FirstByteTimeout { secs: u64 },
    #[error("upstream transport error: {0}")]
    Transport(#[from] reqwest::Error),
}

/// Long-lived upstream HTTP client used by the terminate-mode proxy to call LLM
/// providers (design-change §4.2). One instance lives on [`crate::proxy::HydraProxy`]
/// for the whole server lifetime; the inner `reqwest::Client` owns a shared
/// connection pool.
pub struct ProviderClient {
    client: reqwest::Client,
}

impl ProviderClient {
    /// Build a client tuned for long-lived LLM/SSE upstream calls: a generous
    /// per-host idle pool, a long idle timeout, and a 300 s overall timeout so
    /// slow first-token providers are not aborted. Uses `rustls` (matched by
    /// the `http-client` feature in `Cargo.toml`).
    ///
    /// Infallible: on the (catastrophic, essentially never happens with rustls)
    /// case the TLS backend fails to initialise, we fall back to reqwest's
    /// default client rather than panicking — production code must not `unwrap`.
    #[must_use]
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(32)
            .pool_idle_timeout(Duration::from_secs(90))
            .timeout(Duration::from_secs(300))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client }
    }

    /// Construct the upstream request for one failover attempt.
    ///
    /// Performs the three rewrites the stream-through design did across
    /// `upstream_request_filter` (design §6.5), now collapsed into one call:
    ///
    /// 1. **Path rewrite** — `rewrite_path` re-joins the downstream `/v1/…`
    ///    tail onto the provider's parsed endpoint base (`scheme://host[:port]`
    ///    + path prefix).
    /// 2. **Key swap** — every client credential header is replaced by the
    ///    provider api-key chosen for this attempt, in the transport the wire
    ///    format requires (see the credential-injection block below).
    /// 3. **Host** — the `Host` header is set to the provider host.
    ///
    /// The body is attached as `reqwest::Body::from(body.clone())` — an O(1)
    /// `Bytes` refcount bump, so each failover attempt reuses the same bytes
    /// (design §4.3, Oracle correction #2).
    ///
    /// Returns a `RequestBuilder` (infallible to construct) so the failover
    /// loop can rebuild fresh per candidate without owning a `Client` handle.
    /// The downstream-relevant headers (`Accept`, `Content-Type`) are forwarded
    /// from the original request where present, then overridden as needed.
    pub fn build_request(
        &self,
        original: &RequestHeader,
        provider: &Provider,
        upstream_key: &str,
        body: &Bytes,
        trace_id: &str,
    ) -> reqwest::RequestBuilder {
        let endpoint = parse_endpoint(&provider.endpoint);
        let url = match &endpoint {
            Some(ep) => rewrite_path(original.uri.path(), ep),
            None => {
                // Malformed endpoint (loader normally rejects these; a reload
                // data-graph inconsistency can still reach here). Fall back to
                // the raw endpoint string + the request path so the attempt
                // fails loudly at the network layer rather than panicking.
                format!(
                    "{}/{}",
                    provider.endpoint.trim_end_matches('/'),
                    original.uri.path().trim_start_matches('/')
                )
            }
        };

        let mut builder = self
            .client
            .request(original.method.clone(), &url)
            .header("X-Hydra-Trace-Id", trace_id)
            .body(body.clone());

        if let Some(ep) = &endpoint {
            builder = builder.header("Host", &ep.host);
        }

        // Credential injection (§6.5). The wire format decides the transport,
        // and exactly ONE credential header is ever sent: several gateways
        // reject a request carrying both, which is why QwenLM/qwen-code
        // reverted exactly such a double-emit (PR #4385 "regresses
        // IdeaLab-style proxies").
        //   /v1/messages    → Anthropic-native   ⇒ `x-api-key`
        //   everything else → OpenAI-compatible  ⇒ `Authorization: Bearer`
        match protocol_for_path(original.uri.path()) {
            ProviderKind::Anthropic => {
                builder = builder.header("x-api-key", upstream_key);
                // Anthropic-only headers that affect provider behaviour must
                // survive the hop: without `anthropic-version` the real API
                // answers 400 `invalid_request_error`.
                for name in ["anthropic-version", "anthropic-beta"] {
                    if let Some(value) = original.headers.get(name).and_then(|v| v.to_str().ok()) {
                        builder = builder.header(name, value);
                    }
                }
            }
            _ => {
                builder = builder.header("Authorization", format!("Bearer {upstream_key}"));
            }
        }

        // Forward a minimal set of client hints that affect provider behaviour.
        // We deliberately do NOT forward any client credential header (all of
        // them are replaced above) and we let providers set their own
        // `Content-Type` default when the client omitted one.
        if let Some(accept) = original.headers.get("accept").and_then(|v| v.to_str().ok()) {
            builder = builder.header("Accept", accept);
        }
        if let Some(ct) = original
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
        {
            builder = builder.header("Content-Type", ct);
        }

        builder
    }

    /// Execute a built request and return the streaming response. The failover
    /// loop's single seam for "send to upstream", now bounded by TWO clocks on
    /// purpose:
    ///
    /// - `first_byte_secs` wraps `send()` ONLY. `send()` resolves when the
    ///   response HEADERS arrive, so this is a true time-to-first-byte bound: an
    ///   upstream that accepts the connection and then says nothing used to burn
    ///   the client-level 300s timeout on EVERY attempt (the documented
    ///   "connected but never answers" symptom).
    /// - the client-level 300s timeout still covers the whole exchange,
    ///   including body reads. Setting `RequestBuilder::timeout` instead would
    ///   cap the streaming body and truncate long SSE responses.
    pub async fn send(
        &self,
        req: reqwest::RequestBuilder,
        first_byte_secs: u64,
    ) -> Result<reqwest::Response, SendError> {
        match tokio::time::timeout(Duration::from_secs(first_byte_secs), req.send()).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(e)) => Err(SendError::Transport(e)),
            Err(_) => Err(SendError::FirstByteTimeout {
                secs: first_byte_secs,
            }),
        }
    }
}

impl Default for ProviderClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T9.1 — an upstream that accepts the connection and then says NOTHING must
    /// fail within the first-byte bound instead of burning the 300s exchange
    /// timeout. This is the documented "connected but never answers" symptom:
    /// without a first-byte bound every attempt (and every failover hop) waits
    /// out the full 300s.
    #[tokio::test]
    async fn a_silent_upstream_fails_within_the_first_byte_bound() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        // Hold every accepted connection open and never write a byte.
        let keep: std::sync::Arc<std::sync::Mutex<Vec<std::net::TcpStream>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let keep2 = keep.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(s) = stream else { break };
                keep2.lock().expect("lock").push(s);
            }
        });

        let client = ProviderClient::new();
        let req = client
            .client
            .post(format!("http://{addr}/v1/chat/completions"))
            .body("{}");
        let started = std::time::Instant::now();
        let err = client
            .send(req, 1)
            .await
            .expect_err("a silent upstream must not produce a response");
        let elapsed = started.elapsed();
        match err {
            SendError::FirstByteTimeout { secs } => assert_eq!(secs, 1),
            other => panic!("expected FirstByteTimeout, got {other:?}"),
        }
        assert!(
            elapsed < Duration::from_secs(10),
            "the bound must be honoured, not the 300s exchange timeout (took {elapsed:?})"
        );
        assert!(
            err.to_string().contains("already sent"),
            "the message must say the request WAS sent (double-billing semantics): {err}"
        );
    }

    /// A connection that is REFUSED is a transport error — the caller may treat
    /// it as proof the request never arrived (and thus fail over safely).
    #[tokio::test]
    async fn a_refused_upstream_is_a_transport_error() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr").port()
        };
        let client = ProviderClient::new();
        let req = client
            .client
            .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
            .body("{}");
        let err = client.send(req, 1).await.expect_err("refused ⇒ error");
        assert!(
            matches!(err, SendError::Transport(_)),
            "a refusal must stay a transport error, got {err:?}"
        );
    }

    /// The first-byte bound must NOT truncate a slow STREAM: a response whose
    /// HEADERS arrive promptly but whose body keeps flowing for longer than the
    /// bound has to complete. (This is why the bound wraps `send()` and not
    /// `RequestBuilder::timeout`, which caps the whole body and would cut long
    /// SSE completions off mid-answer.)
    #[tokio::test]
    async fn a_slow_body_is_not_truncated_by_the_first_byte_bound() {
        // A raw HTTP/1.1 responder: headers immediately, body in two halves with
        // a pause in between.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let first = "data: {\"x\":1}\n\n";
        let second = "data: [DONE]\n\n";
        let total = first.len() + second.len();
        let first_owned = first.to_string();
        let second_owned = second.to_string();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf); // request head
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                     Content-Length: {total}\r\nConnection: close\r\n\r\n"
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(first_owned.as_bytes());
                let _ = s.flush();
                // The body takes LONGER than the 1s first-byte bound below.
                std::thread::sleep(Duration::from_millis(1500));
                let _ = s.write_all(second_owned.as_bytes());
                let _ = s.flush();
            }
        });

        let client = ProviderClient::new();
        let req = client
            .client
            .post(format!("http://{addr}/v1/chat/completions"))
            .body("{}");
        let resp = client
            .send(req, 1)
            .await
            .expect("the first-byte bound is about headers, not the body");
        assert_eq!(resp.status().as_u16(), 200);
        let text = resp.text().await.expect("body");
        assert_eq!(
            text,
            format!("{first}{second}"),
            "the streamed body must arrive intact — a truncated SSE completion is \
             a silent data loss"
        );
    }

    fn header(path: &str) -> RequestHeader {
        let mut h = RequestHeader::build("POST", path.as_bytes(), Some(0)).expect("build header");
        let uri: http::Uri = path.parse().expect("parse uri");
        h.set_uri(uri);
        h
    }

    /// Same as [`header`] with one extra downstream header attached.
    fn header_with(path: &str, name: &str, value: &str) -> RequestHeader {
        let mut h = header(path);
        h.insert_header(name.to_string(), value)
            .expect("insert header");
        h
    }

    #[test]
    fn build_rewrites_path_and_swaps_key() {
        let pc = ProviderClient::new();
        let provider = Provider {
            id: "p1".into(),
            key: "openai".into(),
            name: "OpenAI".into(),
            endpoint: "https://api.openai.com".into(),
            weight: 1,
            created_at: "t".into(),
            updated_at: "t".into(),
            max_concurrency: None,
            max_queue_depth: None,
            queue_wait_timeout_ms: None,
        };
        let body = Bytes::from_static(b"{\"model\":\"gpt-4\"}");
        // We can't easily inspect a RequestBuilder's headers/url without
        // building it; build() is infallible for well-formed input.
        let rb = pc.build_request(
            &header("/v1/chat/completions"),
            &provider,
            "sk-upstream-secret",
            &body,
            "trace-1",
        );
        let req = rb.build().expect("build request");
        assert_eq!(req.method(), reqwest::Method::POST);
        assert_eq!(
            req.url().as_str(),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            req.headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer sk-upstream-secret")
        );
        assert!(
            req.headers().get("x-api-key").is_none(),
            "exactly one credential header may be sent (OpenAI path)"
        );
        assert_eq!(
            req.headers().get("host").and_then(|v| v.to_str().ok()),
            Some("api.openai.com")
        );
        assert_eq!(
            req.headers()
                .get("x-hydra-trace-id")
                .and_then(|v| v.to_str().ok()),
            Some("trace-1")
        );
    }

    #[test]
    fn build_applies_endpoint_path_prefix() {
        let pc = ProviderClient::new();
        let provider = Provider {
            id: "gw".into(),
            key: "gw".into(),
            name: "GW".into(),
            endpoint: "https://gw.example.com/llm/".into(),
            weight: 1,
            created_at: "t".into(),
            updated_at: "t".into(),
            max_concurrency: None,
            max_queue_depth: None,
            queue_wait_timeout_ms: None,
        };
        let body = Bytes::from_static(b"{}");
        let rb = pc.build_request(
            &header("/v1/chat/completions"),
            &provider,
            "k",
            &body,
            "trace",
        );
        let req = rb.build().expect("build");
        assert_eq!(
            req.url().as_str(),
            "https://gw.example.com/llm/v1/chat/completions"
        );
    }

    /// The Anthropic-native path gets `x-api-key` (and NOT `Authorization`),
    /// and the Anthropic-only headers the client sent survive the hop — without
    /// `anthropic-version` the real API answers 400.
    #[test]
    fn build_uses_x_api_key_and_forwards_anthropic_headers_for_v1_messages() {
        let pc = ProviderClient::new();
        let provider = Provider {
            id: "p1".into(),
            key: "anthropic".into(),
            name: "Anthropic".into(),
            endpoint: "https://api.anthropic.com".into(),
            weight: 1,
            created_at: "t".into(),
            updated_at: "t".into(),
            max_concurrency: None,
            max_queue_depth: None,
            queue_wait_timeout_ms: None,
        };
        let body = Bytes::from_static(b"{\"model\":\"claude-3-5-sonnet\"}");
        let original = header_with("/v1/messages", "anthropic-version", "2023-06-01");
        let rb = pc.build_request(&original, &provider, "sk-ant-secret", &body, "trace-2");
        let req = rb.build().expect("build request");

        assert_eq!(
            req.headers().get("x-api-key").and_then(|v| v.to_str().ok()),
            Some("sk-ant-secret")
        );
        assert!(
            req.headers().get("authorization").is_none(),
            "exactly one credential header may be sent (Anthropic path)"
        );
        assert_eq!(
            req.headers()
                .get("anthropic-version")
                .and_then(|v| v.to_str().ok()),
            Some("2023-06-01")
        );
        assert_eq!(req.url().as_str(), "https://api.anthropic.com/v1/messages");
    }

    /// The OpenAI-compatible path never leaks the Anthropic-only headers it
    /// may have received (they are meaningless to that family).
    #[test]
    fn build_does_not_forward_anthropic_headers_on_the_openai_path() {
        let pc = ProviderClient::new();
        let provider = Provider {
            id: "p1".into(),
            key: "openai".into(),
            name: "OpenAI".into(),
            endpoint: "https://api.openai.com".into(),
            weight: 1,
            created_at: "t".into(),
            updated_at: "t".into(),
            max_concurrency: None,
            max_queue_depth: None,
            queue_wait_timeout_ms: None,
        };
        let body = Bytes::from_static(b"{}");
        let original = header_with("/v1/chat/completions", "anthropic-version", "2023-06-01");
        let rb = pc.build_request(&original, &provider, "sk-secret", &body, "trace-3");
        let req = rb.build().expect("build request");

        assert_eq!(
            req.headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer sk-secret")
        );
        assert!(req.headers().get("anthropic-version").is_none());
        assert!(req.headers().get("x-api-key").is_none());
    }
}
