//! # External auth boundary — `AuthCache` + `HttpAuthChecker`.
//!
//! Wires the pure decision core (`hydra_core::auth`) to two real I/O devices:
//!
//! - a concurrent `DashMap<(tenant_id, sha256(api_key)), AuthEntry>` cache
//!   (design §11.5); plaintext api-keys are **never resident** — only their
//!   SHA-256 digest is the value half of the key;
//! - a `reqwest` async client that POSTs the design §11.3 contract to each
//!   tenant's `auth_url`, using its own independent connection pool
//!   (isolated from the Pingora upstream channel, design §11.4).
//!
//! **No internal logic is faked here.** The cache hit/expiry verdict, the
//! HTTP-status→`CacheOp` mapping and the `Verdict`→`AuthVerdict` lift are the
//! pure `hydra_core::auth` functions called directly; this module only does
//! the DashMap bookkeeping and the reqwest round-trip. In tests, a real HTTP
//! test server stands in for the *external* tenant auth service — a
//! network-layer double of a third party, never a fake of our own functions
//! (dev-plan §1 铁律 2).
//!
//! See `docs/waves/wave-3-boundaries.md` §2.1/§2.2 and `docs/design.md`
//! §11.2–§11.6.

use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use hydra_core::auth::{
    apply_upstream, cache_decision, decide, sha256_hex, AuthEntry, AuthVerdict, CacheOp,
    CacheSource, Verdict,
};
use hydra_core::model::Tenant;
use tracing::{debug, warn};

/// Clock source injected into [`AuthCache`] so TTL/expiry and GC are
/// deterministic in tests (the pure core takes an explicit `now`; the
/// concurrent wrapper just provides one). Defaults to `Instant::now`.
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// The real wall clock (`Instant::now`).
pub fn system_clock() -> Clock {
    Arc::new(Instant::now)
}

// ---------------------------------------------------------------------------
// AuthCache — concurrent wrapper over the pure decision core (§11.5)
// ---------------------------------------------------------------------------

/// Concurrent auth cache: `DashMap<(tenant_id, sha256(api_key)), AuthEntry>`
/// (design §11.5).
///
/// All cache-hit/expiry judgement is delegated to the pure
/// [`hydra_core::auth::cache_decision`]; this struct is only the threadsafe
/// map bookkeeping + TTL bookkeeping + GC sweep. The api-key is SHA-256
/// hashed *before* it ever touches the map, so plaintext keys are never
/// resident in memory (design §16.4).
pub struct AuthCache {
    map: DashMap<(String, [u8; 32]), AuthEntry>,
    allow_ttl: Duration,
    deny_ttl: Duration,
    now: Clock,
    /// Optional Redis L2 (cluster P4): consulted on L1 miss before the
    /// upstream `auth_url`. `None` in single-node mode.
    #[cfg(feature = "cluster-redis")]
    l2: Option<Arc<crate::redis::auth_cache::RedisAuthL2>>,
    /// Placeholder field (single-node builds never construct the L2).
    #[cfg(not(feature = "cluster-redis"))]
    #[allow(dead_code)]
    l2: Option<()>,
}

impl AuthCache {
    /// New cache with the given TTLs and the real wall clock.
    #[must_use]
    pub fn new(allow_ttl: Duration, deny_ttl: Duration) -> Self {
        Self::with_clock(allow_ttl, deny_ttl, system_clock())
    }

    /// New cache with an injected [`Clock`] (tests / deterministic time).
    #[must_use]
    pub fn with_clock(allow_ttl: Duration, deny_ttl: Duration, now: Clock) -> Self {
        Self {
            map: DashMap::new(),
            allow_ttl,
            deny_ttl,
            now,
            l2: None,
        }
    }

    /// Attach the Redis L2 backend (cluster P4). L1 stays the hot path; the
    /// L2 only sees L1 misses.
    #[cfg(feature = "cluster-redis")]
    #[must_use]
    pub fn with_l2(mut self, l2: Arc<crate::redis::auth_cache::RedisAuthL2>) -> Self {
        self.l2 = Some(l2);
        self
    }

    /// Default allow TTL (design §11.5; default 5 min).
    #[must_use]
    pub fn allow_ttl(&self) -> Duration {
        self.allow_ttl
    }

    /// Default deny TTL (design §11.5; default 30 s).
    #[must_use]
    pub fn deny_ttl(&self) -> Duration {
        self.deny_ttl
    }

    /// Number of cached entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the cache holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Look up a cached decision; delegates the hit/expiry verdict to the
    /// pure [`cache_decision`]. The api-key is SHA-256 hashed before lookup.
    /// On an L1 miss, consults the Redis L2 (cluster P4) and hydrates L1 from
    /// it, avoiding an upstream `auth_url` round trip on a cold node.
    pub async fn check(&self, tenant_id: &str, api_key: &str) -> Verdict {
        let hash = sha256_hex(api_key.as_bytes());
        let entry = self.map.get(&(tenant_id.to_string(), hash));
        if let Verdict::Hit(_) = cache_decision(entry.as_deref(), (self.now)()) {
            return cache_decision(entry.as_deref(), (self.now)());
        }
        #[cfg(feature = "cluster-redis")]
        if let Some(l2) = &self.l2 {
            if let Ok(Some((allowed, ttl))) = l2.get(tenant_id, &hex_digest(&hash)).await {
                let expires_at = (self.now)() + ttl;
                self.map.insert(
                    (tenant_id.to_string(), hash),
                    AuthEntry {
                        allowed,
                        expires_at,
                    },
                );
                return Verdict::Hit(allowed);
            }
        }
        Verdict::Miss
    }

    /// Store a fresh decision: `expires_at = now + ttl`. Overwrites any prior
    /// entry for the same `(tenant_id, api_key)`.
    pub async fn set(&self, tenant_id: &str, api_key: &str, allowed: bool, ttl: Duration) {
        let hash = sha256_hex(api_key.as_bytes());
        let expires_at = (self.now)() + ttl;
        self.map.insert(
            (tenant_id.to_string(), hash),
            AuthEntry {
                allowed,
                expires_at,
            },
        );
        #[cfg(feature = "cluster-redis")]
        if let Some(l2) = &self.l2 {
            let _ = l2.set(tenant_id, &hex_digest(&hash), allowed, ttl).await;
        }
    }

    /// Force-invalidate specific api-keys for a tenant (design §11.7).
    /// Returns the count actually removed; missing keys are ignored.
    pub async fn invalidate(&self, tenant_id: &str, api_keys: &[String]) -> usize {
        let mut removed = 0;
        for key in api_keys {
            let hash = sha256_hex(key.as_bytes());
            if self.map.remove(&(tenant_id.to_string(), hash)).is_some() {
                removed += 1;
            }
            #[cfg(feature = "cluster-redis")]
            if let Some(l2) = &self.l2 {
                let _ = l2.del(tenant_id, &hex_digest(&hash)).await;
            }
        }
        removed
    }

    /// Force-invalidate ALL entries for a tenant (design §11.7). Returns the
    /// count removed.
    pub async fn invalidate_tenant(&self, tenant_id: &str) -> usize {
        let before = self.map.len();
        self.map.retain(|(tid, _), _| tid != tenant_id);
        #[cfg(feature = "cluster-redis")]
        if let Some(l2) = &self.l2 {
            let _ = l2.del_tenant(tenant_id).await;
        }
        before - self.map.len()
    }

    /// Clear the LOCAL cache entirely (cluster P4 generation bump: the
    /// invalidation stream was trimmed past our watermark, so the safe action
    /// is re-auth everything; L2 entries expire by TTL).
    pub fn clear_all(&self) {
        self.map.clear();
    }

    /// Evict all entries whose TTL has elapsed (`now >= expires_at`). Returns
    /// the count evicted. This is the sweep a background GC task calls
    /// (task spawn is W4/server-main; the method itself is pure eviction over
    /// the live map).
    pub fn gc(&self) -> usize {
        let now = (self.now)();
        let before = self.map.len();
        self.map.retain(|_, e| now < e.expires_at);
        before - self.map.len()
    }
}

impl fmt::Debug for AuthCache {
    /// Shows the map (sha-256 digests as `[u8;32]` byte arrays — never the
    /// plaintext api-key) plus the TTLs. The clock closure is not `Debug` and
    /// is intentionally omitted.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthCache")
            .field("entries", &self.map)
            .field("allow_ttl", &self.allow_ttl)
            .field("deny_ttl", &self.deny_ttl)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// AuthConfig + FailMode
// ---------------------------------------------------------------------------

/// Behaviour when the tenant `auth_url` is unavailable / errors / times out
/// (design §11.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailMode {
    /// Deny with `503`, do **not** cache, do **not** forward (default;
    /// safety-first — prevent an outage of the auth service from turning into
    /// an open pipe).
    Closed,
    /// Allow without caching (availability-first; only for tenants whose auth
    /// service is independently highly available and who explicitly accept the
    /// transient over-allow risk).
    Open,
}

/// Auth subsystem configuration (design §15.1 `[auth]`).
#[derive(Clone, Debug)]
pub struct AuthConfig {
    /// Cache TTL for an *allow* decision (default 300 s).
    pub allow_ttl: Duration,
    /// Cache TTL for a *deny* decision (default 30 s — short, so a tenant-side
    /// unblock recovers quickly).
    pub deny_ttl: Duration,
    /// Per-call timeout for the `auth_url` round-trip (default 2000 ms).
    pub timeout: Duration,
    /// Fail-mode when the upstream is unavailable (default `Closed`).
    pub fail_mode: FailMode,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            allow_ttl: Duration::from_secs(300),
            deny_ttl: Duration::from_secs(30),
            timeout: Duration::from_millis(2000),
            fail_mode: FailMode::Closed,
        }
    }
}

// ---------------------------------------------------------------------------
// AuthChecker trait + HttpAuthChecker
// ---------------------------------------------------------------------------

/// External auth abstraction (design §11.6). The proxy calls [`check`] in
/// `request_filter`; the Admin service calls [`invalidate`] /
/// [`invalidate_tenant`] to force re-auth (design §11.7 / §13.2).
///
/// `check` is async and returns a `Send` future so the proxy can drive it on
/// the request task; the verdict carries the exact HTTP status to write back
/// so `request_filter` doesn't re-derive it (design §11.6).
pub trait AuthChecker: Send + Sync {
    /// Resolve the auth verdict for `(tenant, api_key)`. Cache-first; on miss
    /// calls the tenant's `auth_url`.
    fn check(&self, tenant: &Tenant, api_key: &str) -> impl Future<Output = AuthVerdict> + Send;
    /// Force-invalidate specific api-keys for a tenant; returns count removed.
    fn invalidate(
        &self,
        tenant_id: &str,
        api_keys: &[String],
    ) -> impl Future<Output = usize> + Send;
    /// Force-invalidate all entries for a tenant; returns count removed.
    fn invalidate_tenant(&self, tenant_id: &str) -> impl Future<Output = usize> + Send;
}

/// Production [`AuthChecker`] — reqwest-based upstream call to each tenant's
/// `auth_url`, backed by [`AuthCache`]. Uses its own independent reqwest
/// connection pool (design §11.4), never reusing the Pingora upstream
/// channel. `reqwest` is built with `rustls-tls` and **without** the
/// `blocking` feature (design §1.1) so it can't spawn a second runtime.
pub struct HttpAuthChecker {
    cache: AuthCache,
    client: reqwest::Client,
    config: AuthConfig,
}

impl HttpAuthChecker {
    /// Build with a freshly-constructed independent reqwest client and the
    /// given cache + config.
    ///
    /// Errors only if the reqwest client fails to build (e.g. TLS backend
    /// init failure); rustls essentially never fails here, so callers
    /// typically fail-fast at startup with `?`.
    pub fn new(cache: AuthCache, config: AuthConfig) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .pool_idle_timeout(Some(Duration::from_secs(90)))
            .tcp_nodelay(true)
            .build()?;
        Ok(Self {
            cache,
            client,
            config,
        })
    }

    /// Build with an explicit reqwest client (shared pool / tests).
    #[must_use]
    pub fn with_client(cache: AuthCache, config: AuthConfig, client: reqwest::Client) -> Self {
        Self {
            cache,
            client,
            config,
        }
    }

    /// Access the underlying cache (Admin direct GC / introspection).
    #[must_use]
    pub fn cache(&self) -> &AuthCache {
        &self.cache
    }

    /// Access the independent reqwest client (config introspection /
    /// independent-pool assertion, design §11.4).
    #[must_use]
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Access the active auth config.
    #[must_use]
    pub fn config(&self) -> &AuthConfig {
        &self.config
    }
}

impl AuthChecker for HttpAuthChecker {
    fn check(&self, tenant: &Tenant, api_key: &str) -> impl Future<Output = AuthVerdict> + Send {
        // Captures &self, &tenant, &api_key by ref — all Send/Sync, so the
        // future is Send and can be driven on the request task.
        let Self {
            cache,
            client,
            config,
        } = self;
        let auth_url = tenant.auth_url.clone();
        let tenant_id = tenant.id.clone();
        let timeout = config.timeout;
        let fail_mode = config.fail_mode;
        let allow_ttl = config.allow_ttl;
        let deny_ttl = config.deny_ttl;
        let api_key_owned = api_key.to_string();
        async move {
            // (1) auth_url mandatory (design §11.1): empty/missing → always 401.
            if auth_url.trim().is_empty() {
                return AuthVerdict::Denied {
                    status: 401,
                    reason: "no_auth_url",
                    source: CacheSource::Local,
                };
            }

            // (2) cache first (design §11.2) — pure verdict, zero network.
            match cache.check(&tenant_id, &api_key_owned).await {
                v @ Verdict::Hit(true) => {
                    debug!(tenant = %tenant_id, verdict = ?v, "auth cache hit (allowed)");
                    return decide(v, 401, "denied");
                }
                v @ Verdict::Hit(false) => {
                    debug!(tenant = %tenant_id, verdict = ?v, "auth cache hit (denied)");
                    return decide(v, 401, "denied");
                }
                Verdict::Miss => debug!(tenant = %tenant_id, "auth cache miss — going upstream"),
            }

            // (3) upstream POST (design §11.3) with the configured timeout.
            let trace_id = generate_trace_id();
            let body = auth_request_body(&api_key_owned, &tenant_id);
            let send = client
                .post(&auth_url)
                .header("authorization", format!("Bearer {}", api_key_owned))
                .header("x-hydra-tenant", &tenant_id)
                .header("x-hydra-trace-id", &trace_id)
                .header("content-type", "application/json")
                .body(body)
                .send();

            let resp = match tokio::time::timeout(timeout, send).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    warn!(tenant = %tenant_id, error = %e, "auth upstream request failed");
                    return fail_mode_verdict(fail_mode);
                }
                Err(_) => {
                    warn!(tenant = %tenant_id, timeout_ms = timeout.as_millis() as u64,
                        "auth upstream timed out");
                    return fail_mode_verdict(fail_mode);
                }
            };

            // (4) status → CacheOp via pure apply_upstream (design §11.3).
            let status = resp.status().as_u16();
            let op = apply_upstream(status, allow_ttl, deny_ttl);
            match op {
                CacheOp::Set { allowed: true, ttl } => {
                    // 2xx allow — but the decision may live in the body: the
                    // Dogress tenant auth service (`crates/api` `/auth/api_key`,
                    // `AuthApiKeyResponse`) ALWAYS answers HTTP 200 and flags
                    // denials as `{"status":false}`; design §11.3 likewise
                    // allows `{"allowed":false}`. An explicit false flag is a
                    // denial (cached with deny_ttl); any other 2xx body —
                    // `{"status":true}`, `{"allowed":true,"expires_in":60}`,
                    // empty, unparseable — stays an allow.
                    let text = resp.text().await.unwrap_or_default();
                    if body_says_denied(&text) {
                        cache.set(&tenant_id, &api_key_owned, false, deny_ttl).await;
                        return AuthVerdict::Denied {
                            status: 401,
                            reason: "denied",
                            source: CacheSource::Miss,
                        };
                    }
                    // optional `expires_in` overrides the default allow TTL
                    // (design §11.3).
                    let effective_ttl = parse_expires_in(&text)
                        .map(Duration::from_secs)
                        .unwrap_or(ttl);
                    cache
                        .set(&tenant_id, &api_key_owned, true, effective_ttl)
                        .await;
                    decide(Verdict::Miss, 401, "denied") // → Allowed{Miss}
                }
                CacheOp::Set {
                    allowed: false,
                    ttl,
                } => {
                    // 401/403: cache the denial with the deny TTL (design §11.2).
                    cache.set(&tenant_id, &api_key_owned, false, ttl).await;
                    AuthVerdict::Denied {
                        status: 401,
                        reason: "denied",
                        source: CacheSource::Miss,
                    }
                }
                CacheOp::None => {
                    // 5xx / other unmappable status — fail-mode, never cache.
                    debug!(tenant = %tenant_id, status, "auth upstream unmappable status");
                    fail_mode_verdict(fail_mode)
                }
            }
        }
    }

    async fn invalidate(&self, tenant_id: &str, api_keys: &[String]) -> usize {
        self.cache.invalidate(tenant_id, api_keys).await
    }

    async fn invalidate_tenant(&self, tenant_id: &str) -> usize {
        self.cache.invalidate_tenant(tenant_id).await
    }
}

/// Verdict the shell returns on upstream unavailability, per `fail_mode`
/// (design §11.4). Factored as a free fn so the async block above can call
/// it without borrowing `self` across `.await` points on every error branch.
fn fail_mode_verdict(fail_mode: FailMode) -> AuthVerdict {
    match fail_mode {
        FailMode::Closed => AuthVerdict::Denied {
            status: 503,
            reason: "auth_upstream_unavailable",
            source: CacheSource::Local,
        },
        FailMode::Open => AuthVerdict::Allowed {
            source: CacheSource::Local,
        },
    }
}

// ---------------------------------------------------------------------------
// Tiny JSON helpers — reqwest's `json` feature brings serde/serde_json only
// transitively, so this crate cannot `use serde_json` directly. The auth
// contract body is tiny and well-defined, so a correct, allocation-light
// hand-build (request) and a defensive scan (response `expires_in`) are both
// safe and dependency-free.
// ---------------------------------------------------------------------------

/// Escape `s` into `out` as a JSON string body (without the surrounding
/// quotes), per RFC 8259 §7. Used to safely embed untrusted `api_key` /
/// `tenant_id` text into the request JSON.
fn json_escape_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

/// Build the auth request JSON body: design §11.3
/// `{"api_key":"<api_key>","tenant_id":"<tenant_id>"}` plus the Dogress
/// `crates/api` `AuthApiKeyRequest` alias `"key":"<api_key>"`. Both sides
/// ignore unknown JSON fields, so the superset body satisfies both contracts:
/// the mock tenant / §11.3 readers use `api_key`, the Dogress `/auth/api_key`
/// handler reads `key`.
///
/// `pub(crate)` so the admin auth-url test endpoint (`tenant_auth_test`)
/// probes with the exact same body the proxy would send.
pub(crate) fn auth_request_body(api_key: &str, tenant_id: &str) -> String {
    let mut out = String::with_capacity(api_key.len() * 2 + tenant_id.len() + 48);
    out.push_str("{\"api_key\":\"");
    json_escape_into(&mut out, api_key);
    out.push_str("\",\"key\":\"");
    json_escape_into(&mut out, api_key);
    out.push_str("\",\"tenant_id\":\"");
    json_escape_into(&mut out, tenant_id);
    out.push_str("\"}");
    out
}

/// Best-effort extraction of a numeric `"expires_in"` field from a 2xx auth
/// response body (design §11.3 optional field). Returns `None` if absent or
/// unparseable — the caller falls back to the default allow TTL. A tiny
/// scan rather than a full JSON parse: the body is small and the field is a
/// flat top-level integer when present.
fn parse_expires_in(body: &str) -> Option<u64> {
    const KEY: &str = "\"expires_in\"";
    let rest = body.split_once(KEY)?.1;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(':')?;
    let rest = rest.trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// Whether a 2xx auth response body carries an explicit denial flag:
/// `{"status":false}` (Dogress `crates/api` `AuthApiKeyResponse.status`) or
/// `{"allowed":false}` (design §11.3 optional refinement). Any other body —
/// `{"status":true}`, `{"allowed":true,...}`, empty, or unparseable — stays
/// an allow. Tiny scans in the style of [`parse_expires_in`]: both flags are
/// flat top-level JSON booleans when present, so a false positive on nested /
/// string occurrences is structurally impossible for the response shapes both
/// contracts use.
///
/// `pub(crate)` so the admin auth-url test endpoint can classify the mock
/// response exactly as the proxy would.
pub(crate) fn body_says_denied(body: &str) -> bool {
    const STATUS: &str = "\"status\"";
    const ALLOWED: &str = "\"allowed\"";
    json_field_is_false(body, STATUS) || json_field_is_false(body, ALLOWED)
}

/// Scan `body` for a top-level JSON boolean field `<field>: false`
/// (whitespace tolerant; `<field>` must be the quoted key, e.g. `"\"status\""`).
fn json_field_is_false(body: &str, field: &str) -> bool {
    let Some((_, rest)) = body.split_once(field) else {
        return false;
    };
    let rest = rest.trim_start();
    let Some(rest) = rest.strip_prefix(':') else {
        return false;
    };
    rest.trim_start().starts_with("false")
}

/// Hex-encode a SHA-256 digest for the L2 key (no base64 dep needed).
#[cfg(feature = "cluster-redis")]
fn hex_digest(hash: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for b in hash {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Generate a per-request trace id (dependency-free). W4 may instead inject
/// the proxy's own `RequestContext.trace_id` for end-to-end correlation; this
/// is the W3 self-contained default so the `X-Hydra-Trace-Id` header is
/// always populated.
fn generate_trace_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("hydra-{nanos:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escape_handles_special_chars() {
        let mut out = String::new();
        json_escape_into(&mut out, "a\"b\\c\n");
        assert_eq!(out, "a\\\"b\\\\c\\n");
    }

    #[test]
    fn request_body_contains_fields() {
        let body = auth_request_body("sk-test", "t1");
        assert!(body.contains("\"api_key\":\"sk-test\""));
        assert!(body.contains("\"key\":\"sk-test\""));
        assert!(body.contains("\"tenant_id\":\"t1\""));
    }

    #[test]
    fn request_body_escapes_quotes_in_key() {
        let body = auth_request_body("sk-\"evil", "t1");
        // the embedded quote must be escaped, not terminate the string early
        assert!(body.contains("\"api_key\":\"sk-\\\"evil\""));
        assert!(body.contains("\"key\":\"sk-\\\"evil\""));
    }

    #[test]
    fn body_says_denied_dogress_status_false() {
        assert!(body_says_denied(
            "{\"status\":false,\"reason\":\"invalid_key\"}"
        ));
        assert!(body_says_denied("{ \"status\" : false }"));
    }

    #[test]
    fn body_says_denied_design_allowed_false() {
        assert!(body_says_denied(
            "{\"allowed\":false,\"reason\":\"blocked\"}"
        ));
    }

    #[test]
    fn body_says_denied_true_or_absent() {
        assert!(!body_says_denied("{\"status\":true,\"reason\":\"\"}"));
        assert!(!body_says_denied("{\"allowed\":true,\"expires_in\":300}"));
        assert!(!body_says_denied("{\"allowed\":true,\"status\":true}"));
        assert!(!body_says_denied(""));
        assert!(!body_says_denied("not json"));
        // `false` inside a string value must NOT count as a denial
        assert!(!body_says_denied("{\"reason\":\"status is false\"}"));
        // `"status":"false"` (string, not boolean) must NOT count either
        assert!(!body_says_denied("{\"status\":\"false\"}"));
    }

    #[test]
    fn parse_expires_in_present() {
        assert_eq!(
            parse_expires_in("{\"allowed\":true,\"expires_in\":60}"),
            Some(60)
        );
        assert_eq!(parse_expires_in("{ \"expires_in\" : 300 }"), Some(300));
    }

    #[test]
    fn parse_expires_in_absent() {
        assert_eq!(parse_expires_in("{\"allowed\":true}"), None);
        assert_eq!(parse_expires_in(""), None);
    }

    #[test]
    fn parse_expires_in_no_digits() {
        assert_eq!(parse_expires_in("{\"expires_in\":}"), None);
    }
}
