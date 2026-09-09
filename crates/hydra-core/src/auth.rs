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
pub fn sha256_hex(input: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(input);
    hasher.finalize().into()
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
