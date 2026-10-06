//! External-auth verdict & cache types (pure) + api-key hashing helper +
//! the pure cache-decision functions (Auth lane, T7.x).
//!
//! ## Ownership split
//!
//! Shared **types** consumed by the request-filter / auth-cache glue:
//! - [`AuthVerdict`] / [`CacheSource`] — the final verdict the proxy writes
//!   (carrying the HTTP status, design §11.6).
//! - [`Verdict`] — the low-level cache hit/miss returned by [`cache_decision`].
//! - [`AuthEntry`] — one cached decision (design §11.5).
//! - [`CacheOp`] — how an upstream result should be written back to the cache.
//! - [`sha256_hex`] — the real crypto digest used as the `AuthCache` key
//!   (design §11.5); the cache never stores the plaintext api-key.
//!
//! The pure decision functions [`cache_decision`], [`apply_upstream`] and
//! [`decide`] take an explicit `now: Instant` where time matters, so testing is
//! deterministic — no hidden time. The concurrent `DashMap` `AuthCache` wrapper
//! (and its GC task) is W3; this module is the pure decision core it will call.
//! **No function stubs here.**
//!
//! `AuthVerdict` intentionally does **not** derive `Deserialize`: its
//! `reason: &'static str` field cannot be deserialised from JSON by
//! `serde_json` (`'static` borrowing). It is a runtime-only value.

use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

/// Where an auth decision came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheSource {
    /// Served from the in-memory cache (within TTL).
    Hit,
    /// Freshly obtained by calling the tenant's `auth_url`.
    Miss,
    /// Decided locally without a cache hit (e.g. fail-open allow, or
    /// `no_auth_url` deny).
    Local,
}

/// Low-level cache verdict produced by the pure `cache_decision` function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Cache hit within TTL; carries the cached allow/deny flag.
    Hit(bool),
    /// No usable cache entry (missing or expired) — caller must go upstream.
    Miss,
}

/// Final auth verdict handed to `request_filter`. Carries the exact HTTP
/// status to write back so the shell doesn't re-derive it (design §11.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthVerdict {
    /// Allow the request to proceed.
    Allowed { source: CacheSource },
    /// Deny; `status` is the HTTP code (401 / 402 / 503), `reason` a static label.
    Denied {
        status: u16,
        reason: &'static str,
        source: CacheSource,
    },
}

/// SHA-256 digest of `input`, returned as **raw 32 bytes**.
///
/// (The `_hex` suffix in the name is historical; the return type is the raw
/// digest, matching `AuthCacheKey.api_key_hash: [u8; 32]` in design §11.5.)
/// Used to key the auth cache so plaintext api-keys are never resident.
///
/// This is a real, pure computation (sha2) — not a stub.
/// Lower-case hex of [`sha256_hex`]: the ONE place that turns a digest into the
/// string form used as a cache key / stream payload (audit L-1: the cluster
/// invalidation stream and the auth-cache L1/L2 keys each carried their own
/// copy, so a change to either could silently stop the L2 from matching).
#[must_use]
pub fn sha256_hex_string(input: &[u8]) -> String {
    let mut out = String::with_capacity(64);
    for b in sha256_hex(input) {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

pub fn sha256_hex(input: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(input);
    hasher.finalize().into()
}

/// HMAC-SHA256 (RFC 2104), built on the digest above.
///
/// ## Why a MAC lives in this module
///
/// The Arachne config tree is **content-addressed**: its name is the hash of its bytes, so a
/// secret sealed with a random nonce renames the tree on every publish and every node
/// re-materializes for nothing. `hydra_server::cluster::arachne_entities` therefore seals with a
/// nonce DERIVED from the plaintext and the master key
/// (`hydra_server::crypto::KeyProvider::seal_deterministic`), and a plaintext-derived nonce has to
/// be a keyed function — otherwise anyone who guesses a plaintext knows the nonce. That needs a
/// MAC; `sha2` is the only digest dependency in the workspace and provides none, and hand-rolling
/// one next to the code that uses it is how primitives get subtly wrong.
///
/// Implemented from RFC 2104. **Validated against the RFC 4231 test vectors** in this module's
/// tests, whose expected values were produced with an independent implementation (Python's
/// `hmac`) rather than transcribed — a hand-rolled MAC checked against its own output proves
/// nothing.
///
/// Any key length is legal (`len(K) > 64` is hashed first, per the RFC); this repo's master keys
/// are 32 bytes.
#[must_use]
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    /// SHA-256's block size in bytes — the width of the ipad/opad pads.
    const BLOCK: usize = 64;

    // K' = K padded to one block, or H(K) padded when K is longer than a block.
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..32].copy_from_slice(&sha256_hex(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }

    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(message);
    let inner_digest: [u8; 32] = inner.finalize().into();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_digest);
    outer.finalize().into()
}

// ---------------------------------------------------------------------------
// Pure decision functions (Auth lane, T7.x) — no DashMap / no I/O here.
// ---------------------------------------------------------------------------

/// One cached auth decision (design §11.5). `expires_at` is an absolute
/// `Instant`; the concurrent `DashMap` that stores these is assembled in W3/W4.
/// Pure value — comparison and construction are side-effect-free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthEntry {
    /// Whether the tenant's `auth_url` allowed (`true`) or denied (`false`)
    /// this api-key.
    pub allowed: bool,
    /// Absolute expiry; `now >= expires_at` means the entry is stale.
    pub expires_at: Instant,
}

/// What the cache should do with an upstream auth result (design §11.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheOp {
    /// Store a fresh entry: `allowed` flag valid for `ttl` from now.
    Set { allowed: bool, ttl: Duration },
    /// Do not cache (upstream error / timeout / unmappable status) — the
    /// shell's `fail_mode` decides the response (design §11.4).
    None,
}

/// Cache lookup → low-level [`Verdict`] (design §11.2/§11.5).
///
/// - `None` entry, or an entry past its TTL (`now >= expires_at`) → [`Verdict::Miss`];
/// - a live entry → [`Verdict::Hit`]`(allowed)`.
///
/// Pure: takes the entry and `now` explicitly; the concurrent `AuthCache` map
/// (W3) is responsible for the `(tenant_id, api_key_hash)` lookup that produces
/// the `Option<&AuthEntry>` passed in here.
pub fn cache_decision(entry: Option<&AuthEntry>, now: Instant) -> Verdict {
    match entry {
        Some(e) if now < e.expires_at => Verdict::Hit(e.allowed),
        _ => Verdict::Miss,
    }
}

/// Map an upstream `auth_url` HTTP status into a cache operation (design §11.3):
///
/// - `2xx` → [`CacheOp::Set`]`(true, allow_ttl)` (allow; default 5 min);
/// - `401` / `403` → [`CacheOp::Set`]`(false, deny_ttl)` (deny; deny TTL, e.g.
///   30s, so a tenant-side unblock recovers quickly);
/// - `5xx` / any other status (incl. 3xx, 4xx≠401/403) → [`CacheOp::None`]
///   (service anomaly: do not cache; the shell applies `fail_mode`).
///
/// Pure status→op translation; timeouts / connection errors reach the shell as
/// `None` (it never calls this with a synthetic status for them).
pub fn apply_upstream(status: u16, allow_ttl: Duration, deny_ttl: Duration) -> CacheOp {
    match status {
        200..=299 => CacheOp::Set {
            allowed: true,
            ttl: allow_ttl,
        },
        401 | 403 => CacheOp::Set {
            allowed: false,
            ttl: deny_ttl,
        },
        _ => CacheOp::None,
    }
}

/// Canonical denial reason label used by the tenant auth service for an
/// insufficient-balance verdict (design §11.3; Dogress crates/api /auth/api_key
/// AuthApiKeyResponse.reason). Surfaces to the client as HTTP 402 with
/// type "insufficient_quota" (see enforce_auth in the server crate).
pub const REASON_INSUFFICIENT_BALANCE: &str = "insufficient_balance";

/// Map a tenant auth denial `reason` to the downstream HTTP status.
///
/// - an `insufficient_balance` reason (case-insensitive after trim) => 402
///   Payment Required (欠费; callers MUST NOT cache this verdict — see design
///   §11.3: balance is fast-changing and a cached 402 would degrade into a 401
///   within deny_ttl);
/// - any other / missing reason => 401 (legacy behaviour: an unclassified
///   denial is indistinguishable from an invalid key).
///
/// Pure and table-driven: extending the vocabulary means adding a row here.
pub fn denial_status_for_reason(reason: Option<&str>) -> u16 {
    match reason {
        Some(r) if r.trim().eq_ignore_ascii_case(REASON_INSUFFICIENT_BALANCE) => 402,
        _ => 401,
    }
}

/// Lift a resolved [`Verdict`] into the status-carrying [`AuthVerdict`] the
/// proxy writes back (design §11.6).
///
/// - [`Verdict::Hit`]`(true)` → [`AuthVerdict::Allowed`]`{ Hit }`;
/// - [`Verdict::Hit`]`(false)` → [`AuthVerdict::Denied`]`{ status: status_on_deny,
///   reason, Hit }` — the shell supplies the exact HTTP status (401 vs 503) so
///   `request_filter` can write it verbatim without re-deriving;
/// - [`Verdict::Miss`] → [`AuthVerdict::Allowed`]`{ Miss }`: a `Miss` handed here
///   denotes a *freshly resolved allow* (the shell went upstream on the miss and
///   the upstream returned 2xx). Denials observed straight from an upstream
///   response, and fail-open / fail-closed outcomes, are assembled by the shell
///   directly — it knows the precise `source` (`Miss`/`Local`) and `status`,
///   which a bare `Miss` cannot carry.
pub fn decide(verdict: Verdict, status_on_deny: u16, reason: &'static str) -> AuthVerdict {
    match verdict {
        Verdict::Hit(true) => AuthVerdict::Allowed {
            source: CacheSource::Hit,
        },
        Verdict::Hit(false) => AuthVerdict::Denied {
            status: status_on_deny,
            reason,
            source: CacheSource::Hit,
        },
        Verdict::Miss => AuthVerdict::Allowed {
            source: CacheSource::Miss,
        },
    }
}

#[cfg(test)]
mod hmac_tests {
    use super::hmac_sha256;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// RFC 4231 test vectors, cases 1–5.
    ///
    /// The expected digests came from an INDEPENDENT implementation (Python's `hmac`), not from
    /// memory and not from this code. Case 1 is the all-`0x0b` key, case 2 the short ASCII key
    /// ("Jefe"), case 3 a 20-byte key, case 4 a 25-byte key (still shorter than a block), and case
    /// 5 a 131-byte key — the one that exercises the `len(K) > 64 ⇒ H(K)` branch, which no other
    /// case reaches.
    ///
    /// Falsification: drop the `H(K)` branch and case 5 fails alone.
    #[test]
    fn rfc4231_vectors() {
        assert_eq!(
            hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex(&hmac_sha256(&[0xaa; 20], &[0xdd; 50])),
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
        );
        assert_eq!(
            hex(&hmac_sha256(&(1u8..=25).collect::<Vec<u8>>(), &[0xcd; 50])),
            "82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b"
        );
        assert_eq!(
            hex(&hmac_sha256(&[0x0c; 131], b"Test With Truncation")),
            "547b09f769795c518c182386366bce5a368cf207843f167938cbf22ece23f368"
        );
    }

    /// The property the tree depends on: same key + same message ⇒ same MAC; a different key or a
    /// different message ⇒ a different one.
    #[test]
    fn deterministic_and_key_separated() {
        let k1 = [7u8; 32];
        let k2 = [8u8; 32];
        assert_eq!(hmac_sha256(&k1, b"sk-one"), hmac_sha256(&k1, b"sk-one"));
        assert_ne!(hmac_sha256(&k1, b"sk-one"), hmac_sha256(&k2, b"sk-one"));
        assert_ne!(hmac_sha256(&k1, b"sk-one"), hmac_sha256(&k1, b"sk-two"));
        // A message that is a prefix of another must not collide: the pads make it length-aware,
        // and GCM's tag covers the length, but this catches a "hash the concatenation" refactor.
        assert_ne!(hmac_sha256(&k1, b"sk"), hmac_sha256(&k1, b"sk-one"));
        assert_eq!(hmac_sha256(&k1, b"sk-one").len(), 32);
    }
}
