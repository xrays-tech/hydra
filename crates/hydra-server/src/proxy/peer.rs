//! Upstream peer construction + endpoint URL parsing (wave-4 §2.1 T1.1–T1.3).
//!
//! `parse_endpoint` turns a `Provider::endpoint` string
//! (`https://api.openai.com`, `http://up:8080`, `https://x.com:8443`) into the
//! shared [`EndpointUrl`] form used by both [`crate::proxy`] (for `HttpPeer`
//! construction + SNI/Host) and `hydra_core::rewrite::rewrite_path` (for the
//! upstream path). `build_peer` turns an `EndpointUrl` into a Pingora
//! [`HttpPeer`] with the correct TLS flag + SNI (design §6.4).

use pingora_core::upstreams::peer::HttpPeer;

// Re-export the shared parsed-endpoint type so callers depend on the core
// definition (and `rewrite_path`) without a second copy.
pub use hydra_core::rewrite::EndpointUrl;

/// Parse a provider endpoint URL into scheme / host / port / path-prefix.
///
/// Accepts `http://` and `https://` (the only schemes the W2 loader's
/// `is_usable_endpoint` allows through). When the URL omits the port the
/// scheme default is used (`443` for https, `80` for http). The path prefix is
/// the URL path with any trailing `/` stripped (so `https://gw/llm/` → `/llm`),
/// matching how `rewrite_path` re-joins it onto the request tail.
///
/// Returns `None` for malformed input (missing scheme, empty host); the loader
/// has already rejected unparseable endpoints, so reaching `None` here is a
/// post-reload data-graph inconsistency the shell logs and routes around.
pub fn parse_endpoint(endpoint: &str) -> Option<EndpointUrl> {
    // ONE parser for the dialler, the config loader and the admin write
    // boundary ([EndpointUrl::parse]). It used to be duplicated here while the
    // loader validated with a weaker prefix test, which let the loader accept
    // endpoints this function cannot parse — see audit §21.
    EndpointUrl::parse(endpoint)
}

/// Build a Pingora [`HttpPeer`] from a parsed endpoint (design §6.4).
///
/// TLS is enabled iff the scheme is `https`; the SNI is the endpoint host
/// (without port). The address is `host:port`. The caller is responsible for
/// the path rewrite (handled in `upstream_request_filter` via `rewrite_path`).
pub fn build_peer(endpoint: &EndpointUrl) -> HttpPeer {
    let tls = endpoint.scheme == "https";
    // `authority_host()` brackets an IPv6 literal, so `::1:8080` cannot be
    // produced where a socket address is expected (review D1).
    let addr = format!("{}:{}", endpoint.authority_host(), endpoint.port);
    HttpPeer::new(addr, tls, endpoint.host.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_build_https_sni() {
        let ep = parse_endpoint("https://127.0.0.1").unwrap();
        assert_eq!(ep.scheme, "https");
        assert_eq!(ep.host, "127.0.0.1");
        assert_eq!(ep.port, 443);
        let peer = build_peer(&ep);
        assert!(peer.is_tls());
        assert_eq!(peer.sni, "127.0.0.1");
    }

    #[test]
    fn peer_build_http() {
        let ep = parse_endpoint("http://127.0.0.1:8080").unwrap();
        assert_eq!(ep.scheme, "http");
        assert_eq!(ep.port, 8080);
        let peer = build_peer(&ep);
        assert!(!peer.is_tls());
        assert_eq!(peer.sni, "127.0.0.1");
    }

    #[test]
    fn peer_build_custom_port() {
        let ep = parse_endpoint("https://127.0.0.1:8443").unwrap();
        assert_eq!(ep.port, 8443);
        let peer = build_peer(&ep);
        assert!(peer.is_tls());
        assert_eq!(peer.sni, "127.0.0.1");
    }

    #[test]
    fn peer_build_with_path_prefix() {
        let ep = parse_endpoint("https://gw.provider.com/llm/").unwrap();
        assert_eq!(ep.host, "gw.provider.com");
        assert_eq!(ep.path_prefix, "/llm");
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_endpoint("ftp://x").is_none());
        assert!(parse_endpoint("https://").is_none());
        assert!(parse_endpoint("not a url").is_none());
    }

    #[test]
    fn parse_http_default_port_80() {
        let ep = parse_endpoint("http://upstream.local").unwrap();
        assert_eq!(ep.port, 80);
    }
}

#[cfg(test)]
mod dialability_tests {
    use super::*;

    /// REVIEW D1/H-5 — the second half of the contract: whatever the write
    /// boundary accepts must survive a REAL URL parser (`reqwest`, i.e. the
    /// dialler the terminate-mode proxy actually uses). A bracketed IPv6
    /// endpoint used to compose to `http://::1:8080/…`, which `Url::parse`
    /// rejects with "empty host" — accepted by the admin API, undialable for
    /// every request.
    #[test]
    fn an_ipv6_endpoint_composes_a_url_reqwest_accepts() {
        for raw in ["http://[::1]:8080/x", "https://[fd00::1]:8443"] {
            let ep = parse_endpoint(raw).unwrap_or_else(|| panic!("{raw} must parse"));
            let url = hydra_core::rewrite::rewrite_path("/v1/chat/completions", &ep);
            let parsed = reqwest::Url::parse(&url)
                .unwrap_or_else(|e| panic!("{raw} ⇒ {url} must be a usable URL: {e}"));
            // `url`'s `host_str()` keeps the brackets for an IPv6 literal, so
            // compare against the bracketed authority form.
            assert_eq!(
                parsed.host_str(),
                Some(ep.authority_host().as_ref()),
                "{raw}"
            );
            assert_eq!(parsed.port_or_known_default(), Some(ep.port), "{raw}");
        }
    }

    /// The peer address must be a socket address for the same reason.
    #[test]
    fn an_ipv6_endpoint_yields_a_socket_address() {
        let ep = parse_endpoint("http://[::1]:8080").expect("bracketed IPv6");
        let addr = format!("{}:{}", ep.authority_host(), ep.port);
        assert!(
            addr.parse::<std::net::SocketAddr>().is_ok(),
            "peer address {addr:?} must parse"
        );
    }
}
