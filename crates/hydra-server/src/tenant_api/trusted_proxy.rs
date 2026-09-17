//! Trusted-proxy client-IP resolution for the tenant API's failure limiter.
//!
//! ## Why this exists (design §5.1, C)
//!
//! The per-IP auth-failure lockout keys on the caller's source IP. In the
//! documented **LB→edge** topology every tenant shares the load balancer's
//! egress IP, so the "source IP" is really the LB — and a single tenant's ~11
//! bad tokens black out the whole tenant API for the lockout window, renewable.
//!
//! The fix is to key the limiter on the REAL client IP, which the load balancer
//! records in `X-Forwarded-For`. But `X-Forwarded-For` is caller-controlled, so
//! it can only be honoured when the request comes from a **trusted** hop. The
//! trust list is operator-configured (`HYDRA_TRUSTED_PROXIES`); from any other
//! peer the header is ignored and the peer itself is the key (the old,
//! conservative behaviour).
//!
//! Multiple `X-Forwarded-For` header **lines** (as emitted by proxies that
//! append a separate line rather than merging into one comma-joined value) are
//! concatenated in order by the caller before being passed here, so the
//! rightmost-non-trusted walk sees the full chain.
//!
//! This module is pure (no I/O, no `Session`), so the rightmost-non-trusted walk
//! is unit-testable without a proxy.

use std::net::IpAddr;

/// Parse a comma-separated list of trusted proxies (`HYDRA_TRUSTED_PROXIES`).
///
/// Each entry is a bare IP (treated as `/32` for IPv4, `/128` for IPv6) or a
/// CIDR range. Empty and whitespace-only entries are ignored; an empty/
/// whitespace-only whole input yields `Ok(vec![])` = trust nobody. A malformed
/// entry is an error that NAMES the offending token, so a typo fails startup
/// rather than silently disabling XFF handling.
///
/// `ipnet`'s `FromStr` only accepts the `/prefix` form, so a bare IP is
/// normalised to its host prefix before parsing — that is what makes "a bare IP"
/// and "a CIDR" both parse.
pub fn parse_trusted_proxies(raw: &str) -> Result<Vec<ipnet::IpNet>, String> {
    let mut out = Vec::new();
    for entry in raw.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let normalized = if entry.contains('/') {
            entry.to_string()
        } else if let Ok(v4) = entry.parse::<std::net::Ipv4Addr>() {
            format!("{v4}/32")
        } else if let Ok(v6) = entry.parse::<std::net::Ipv6Addr>() {
            format!("{v6}/128")
        } else {
            return Err(format!(
                "HYDRA_TRUSTED_PROXIES entry {entry:?} is not a valid IP or CIDR"
            ));
        };
        let net = normalized.parse::<ipnet::IpNet>().map_err(|e| {
            format!("HYDRA_TRUSTED_PROXIES entry {entry:?} is not a valid IP or CIDR: {e}")
        })?;
        out.push(net);
    }
    Ok(out)
}

/// Resolve the client IP for a request, honouring `X-Forwarded-For` only when
/// the connecting peer is a trusted proxy.
///
/// - If `trusted` is empty, or `peer` is not contained in any trusted net, the
///   header is NOT consulted and `peer` is returned (the conservative default:
///   an untrusted peer cannot rotate limiter buckets).
/// - Otherwise the `xff` list is walked from the RIGHTMOST entry to the left,
///   skipping entries that are themselves in a trusted net; the first valid
///   non-trusted IP is the client.
/// - If `xff` is absent, empty, entirely trusted, or has no valid IP, `peer` is
///   returned (safe fallback).
#[must_use]
pub fn resolve_client_ip(peer: IpAddr, xff: Option<&str>, trusted: &[ipnet::IpNet]) -> IpAddr {
    // No trust configured, or this peer is not one we trust: never consult the
    // caller-controlled header.
    if trusted.is_empty() || !trusted.iter().any(|net| net.contains(&peer)) {
        return peer;
    }
    let Some(xff) = xff else {
        return peer;
    };
    // Rightmost first: the outermost (closest-to-us) hop's view of the client is
    // the last entry it appended, so we skip the trusted proxies on the way in
    // and stop at the first entry that is itself not a trusted proxy.
    for entry in xff.split(',').rev() {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let Ok(ip) = entry.parse::<IpAddr>() else {
            continue;
        };
        if trusted.iter().any(|net| net.contains(&ip)) {
            continue;
        }
        return ip;
    }
    peer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("test IP")
    }

    fn nets(raw: &str) -> Vec<ipnet::IpNet> {
        parse_trusted_proxies(raw).expect("test nets")
    }

    // --- parse_trusted_proxies ---

    #[test]
    fn parse_accepts_bare_ips_and_cidrs_and_ignores_empty_entries() {
        let v = parse_trusted_proxies("10.0.0.0/8, 192.168.1.2, 2001:db8::/32, ::1").unwrap();
        assert_eq!(v.len(), 4, "bare IPv4/IPv6 and CIDRs all parse");
        // Empty / whitespace-only input trusts nobody.
        assert!(parse_trusted_proxies("").unwrap().is_empty());
        assert!(parse_trusted_proxies("   ").unwrap().is_empty());
        // Comma/whitespace runs are ignored, not errors.
        assert_eq!(
            parse_trusted_proxies("10.0.0.0/8,,  172.16.0.0/12, ")
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn parse_rejects_malformed_entries_naming_the_offender() {
        // A single bad entry.
        let err = parse_trusted_proxies("999.0.0.1").unwrap_err();
        assert!(
            err.contains("999.0.0.1"),
            "the bad entry must be named: {err}"
        );
        // An out-of-range prefix.
        let err = parse_trusted_proxies("10.0.0.0/33").unwrap_err();
        assert!(
            err.contains("10.0.0.0/33"),
            "the bad entry must be named: {err}"
        );
        // A bad entry among valid ones: the error names the OFFENDER, not a valid
        // sibling.
        let err = parse_trusted_proxies("10.0.0.0/8, not-an-ip").unwrap_err();
        assert!(
            err.contains("not-an-ip"),
            "the offending entry must be named: {err}"
        );
        assert!(
            !err.contains("10.0.0.0/8"),
            "a valid sibling must not be named: {err}"
        );
    }

    // --- resolve_client_ip ---

    #[test]
    fn an_untrusted_peer_never_consults_xff() {
        // Empty trusted list: the peer is always returned, regardless of XFF.
        assert_eq!(
            resolve_client_ip(ip("1.2.3.4"), Some("9.9.9.9, 1.2.3.4"), &[]),
            ip("1.2.3.4")
        );
        // A peer not in the trusted list: its spoofed XFF is ignored.
        assert_eq!(
            resolve_client_ip(ip("8.8.8.8"), Some("1.1.1.1"), &nets("10.0.0.0/8")),
            ip("8.8.8.8")
        );
    }

    #[test]
    fn a_trusted_peer_uses_the_rightmost_non_trusted_xff() {
        // Peer 10.0.0.5 is in 10.0.0.0/8. Chain "203.0.113.7, 10.0.0.5":
        // right-to-left, 10.0.0.5 is trusted (skip), 203.0.113.7 is not (return).
        assert_eq!(
            resolve_client_ip(
                ip("10.0.0.5"),
                Some("203.0.113.7, 10.0.0.5"),
                &nets("10.0.0.0/8")
            ),
            ip("203.0.113.7")
        );
        // A lone non-trusted entry in the chain is the client.
        assert_eq!(
            resolve_client_ip(ip("10.0.0.5"), Some("7.7.7.7"), &nets("10.0.0.0/8")),
            ip("7.7.7.7")
        );
    }

    #[test]
    fn an_all_trusted_or_absent_xff_falls_back_to_the_peer() {
        // No XFF at all from a trusted peer: fall back to the peer.
        assert_eq!(
            resolve_client_ip(ip("10.0.0.5"), None, &nets("10.0.0.0/8")),
            ip("10.0.0.5")
        );
        // An empty XFF string: fall back to the peer.
        assert_eq!(
            resolve_client_ip(ip("10.0.0.5"), Some(""), &nets("10.0.0.0/8")),
            ip("10.0.0.5")
        );
        // Every entry in the chain is itself trusted: no client to find, so the
        // peer (the closest hop we trust) is the answer.
        assert_eq!(
            resolve_client_ip(
                ip("10.0.0.5"),
                Some("10.0.0.9, 10.0.0.5"),
                &nets("10.0.0.0/8")
            ),
            ip("10.0.0.5")
        );
        // A chain with only junk (no valid IP): fall back to the peer.
        assert_eq!(
            resolve_client_ip(
                ip("10.0.0.5"),
                Some("garbage, ,:::bad"),
                &nets("10.0.0.0/8")
            ),
            ip("10.0.0.5")
        );
    }

    #[test]
    fn ipv6_chains_resolve_the_same_way() {
        // The trusted hop is the peer itself (a /128); the client in the chain is
        // outside it, so it is returned. (A wider range such as /32 would contain
        // the client too and would correctly fall back to the peer.)
        assert_eq!(
            resolve_client_ip(
                ip("2001:db8::5"),
                Some("2001:db8::7, 2001:db8::5"),
                &nets("2001:db8::5/128")
            ),
            ip("2001:db8::7")
        );
        // A spoofed XFF from an untrusted IPv6 peer is ignored.
        assert_eq!(
            resolve_client_ip(ip("2400:3200::1"), Some("::1"), &nets("2001:db8::/32")),
            ip("2400:3200::1")
        );
    }
}
