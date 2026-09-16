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
//! See `dev-docs/waves/wave-3-boundaries.md` §2.1/§2.2 and `dev-docs/design.md`
//! §11.2–§11.6.

use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use hydra_core::auth::{
    apply_upstream, cache_decision, decide, denial_status_for_reason, sha256_hex, AuthEntry,
    AuthVerdict, CacheOp, CacheSource, Verdict, REASON_INSUFFICIENT_BALANCE,
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
    /// Invalidation counter (N1). Every invalidation bumps it, and
    /// [`Self::set_if_unchanged`] refuses to store a verdict whose epoch moved
    /// while it was being resolved — otherwise an auth response that was
    /// already in flight when the key was revoked would write the
    /// pre-revocation verdict back into the L1/L2 and silently undo the
    /// revocation for the whole TTL.
    epoch: std::sync::atomic::AtomicU64,
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
            epoch: std::sync::atomic::AtomicU64::new(0),
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

    /// The current invalidation epoch (N1): sample it before resolving a
    /// verdict upstream, then hand it to [`Self::set_if_unchanged`].
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Store a verdict obtained from the tenant auth service, but ONLY if no
    /// invalidation landed since `epoch_before` was sampled (N1).
    ///
    /// An invalidation that arrives while the upstream call is in flight has
    /// already dropped the L1/L2 entry; writing this pre-invalidation verdict
    /// afterwards would resurrect it. The caller still returns the verdict for
    /// THIS request (it was resolved legitimately); it is simply not cached, so
    /// the next request re-resolves against the tenant auth service. Returns
    /// whether the verdict was cached.
    pub async fn set_if_unchanged(
        &self,
        epoch_before: u64,
        tenant_id: &str,
        api_key: &str,
        allowed: bool,
        ttl: Duration,
    ) -> bool {
        if self.epoch() != epoch_before {
            debug!(
                tenant = %tenant_id,
                "verdict not cached: the cache was invalidated while it was being resolved"
            );
            return false;
        }
        self.set(tenant_id, api_key, allowed, ttl).await;
        true
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
    /// The L1 verdict for one key, with the shard guard released before this
    /// function returns.
    ///
    /// **Synchronous on purpose.** `DashMap::get` hands back a `Ref`, which is a
    /// *shard read lock* held by a value with a `Drop` impl — so it lives until
    /// the end of its enclosing scope, NOT until its last use. A `Ref` that
    /// survives into an `.await` will deadlock the task the moment it (or
    /// anything else) writes the same shard, and `clippy::await_holding_lock`
    /// cannot see DashMap's guard. Reading the shard inside one non-async
    /// function makes the guard's lifetime impossible to extend by accident
    /// (bug-2026-09-16-auth-cache-guard-deadlock).
    fn l1_decision(&self, key: &(String, [u8; 32])) -> Verdict {
        let entry = self.map.get(key);
        // P2-8: evaluate the decision (and its `now()`) exactly ONCE. Calling
        // `cache_decision` twice — once for the guard and once for the return —
        // let a clock that advanced between the two reads flip a live entry from
        // Hit to Miss mid-lookup.
        cache_decision(entry.as_deref(), (self.now)())
    }

    pub async fn check(&self, tenant_id: &str, api_key: &str) -> Verdict {
        let hash = sha256_hex(api_key.as_bytes());
        let key = (tenant_id.to_string(), hash);

        // (1) L1 only. The shard guard is gone by the time this returns: an
        //     entry that is present-but-EXPIRED is a Miss here while still
        //     occupying the map, and it was exactly that case which held a read
        //     guard across the L2 await below.
        if let Verdict::Hit(allowed) = self.l1_decision(&key) {
            return Verdict::Hit(allowed);
        }

        // (2) L2 back-fill. NO shard guard may be held across these awaits —
        //     the `insert` takes the same shard's WRITE lock, so holding a read
        //     guard on this task is a self-deadlock (not a race: it can never
        //     resolve).
        #[cfg(feature = "cluster-redis")]
        if let Some(l2) = &self.l2 {
            if let Ok(Some((allowed, ttl))) = l2.get(tenant_id, &hex_digest(&hash)).await {
                let expires_at = (self.now)() + ttl;
                self.map.insert(
                    key,
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
        self.epoch.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
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

    /// Force-invalidate specific api-keys given as their SHA-256 hex digests
    /// (the v=2 invalidation stream carries digests, never plaintext — F-1).
    /// Mirrors [`invalidate`] but skips the hashing step: the caller already
    /// hashed, so the SAME hex addresses the L2 (no re-hash). Returns the
    /// count removed from L1; missing / malformed digests are ignored.
    pub async fn invalidate_hashes(&self, tenant_id: &str, keyhashes: &[String]) -> usize {
        self.epoch.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let mut removed = 0;
        for h in keyhashes {
            let Some(hash) = hex_to_bytes32(h) else {
                continue; // malformed digest → skip (defensive, not a real key)
            };
            if self.map.remove(&(tenant_id.to_string(), hash)).is_some() {
                removed += 1;
            }
            #[cfg(feature = "cluster-redis")]
            if let Some(l2) = &self.l2 {
                // `h` IS the L2 key suffix (`hex_digest`) — do NOT re-hash, but
                // DO normalise: `hex_to_bytes32` accepts upper case while the L2
                // key is lower case, so a foreign publisher writing `ABCD…`
                // would delete the L1 entry and leave the L2 one to re-hydrate
                // it (audit L-4).
                let _ = l2.del(tenant_id, &h.to_ascii_lowercase()).await;
            }
        }
        removed
    }

    /// Force-invalidate ALL entries for a tenant (design §11.7). Returns the
    /// count removed.
    pub async fn invalidate_tenant(&self, tenant_id: &str) -> usize {
        self.epoch.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let before = self.map.len();
        self.map.retain(|(tid, _), _| tid != tenant_id);
        #[cfg(feature = "cluster-redis")]
        if let Some(l2) = &self.l2 {
            let _ = l2.del_tenant(tenant_id).await;
        }
        before - self.map.len()
    }

    /// Clear the whole cache — L1 **and** the L2 of every tenant (cluster P4
    /// generation bump: the invalidation stream was trimmed past our watermark,
    /// so the safe action is re-auth everything). Returns the L1 entries
    /// dropped.
    ///
    /// Clearing only the L1 was NOT a clear: `check` re-hydrates an L1 miss from
    /// the L2 (`l2.get`), so a key whose invalidation event the trim dropped was
    /// served the stale verdict again for the rest of its TTL — and that TTL is
    /// the tenant's effective allow TTL, which a tenant auth service can raise
    /// well beyond the 300 s default via `expires_in` (review B2).
    pub async fn clear_all(&self) -> usize {
        self.epoch.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let cleared = self.map.len();
        self.map.clear();
        #[cfg(feature = "cluster-redis")]
        if let Some(l2) = &self.l2 {
            if let Err(e) = l2.del_all_tenants().await {
                // Best effort, but loud: the L1 is already gone, and whatever
                // the L2 still holds can be resurrected by the next L1 miss.
                warn!(
                    error = %e,
                    "L2 clear failed after a whole-cache invalidation; stale verdicts may be re-served"
                );
            }
        }
        cleared
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
            //
            // N1: sample the invalidation epoch BEFORE the round trip. An
            // invalidation that lands while this request is being authorized
            // (a slow tenant auth service is exactly what this cache exists to
            // absorb) must not be undone by the verdict this request is about
            // to receive — it is returned for THIS request but not cached.
            let epoch_before = cache.epoch();
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
            // (4a) Out-of-band HTTP 402 (Payment Required): the tenant auth
            // service answered a raw 402 for insufficient balance. Surface it
            // verbatim and NEVER cache (design §11.3 — balance is fast-changing
            // and a cached 402 would degrade into a 401 within deny_ttl; L1 and
            // Redis L2 are both skipped because we never call cache::set). An
            // explicit 402 is a denial, not an availability anomaly, so it
            // bypasses fail_mode by design.
            if status == 402 {
                return AuthVerdict::Denied {
                    status: 402,
                    reason: "denied",
                    source: CacheSource::Miss,
                };
            }
            let op = apply_upstream(status, allow_ttl, deny_ttl);
            match op {
                CacheOp::Set { allowed: true, ttl } => {
                    // 2xx allow — but the decision may live in the body: the
                    // Dogress tenant auth service (`crates/api` `/auth/api_key`,
                    // `AuthApiKeyResponse`) ALWAYS answers HTTP 200 and flags
                    // denials as `{"status":false}`; design §11.3 likewise
                    // allows `{"allowed":false}`. An explicit false flag is a
                    // denial (cached with deny_ttl).
                    let text = resp.text().await.unwrap_or_default();
                    if body_says_denied(&text) {
                        // Reason-aware denial (design §11.3 / Dogress
                        // AuthApiKeyResponse): an insufficient_balance reason
                        // (case-insensitive) maps to 402 and is NOT cached (we
                        // never call cache::set here, so neither L1 nor Redis
                        // L2 is written — a top-up is seen on the next
                        // request). All other denials keep the legacy 401 +
                        // deny-cache semantics (see the invalid_key regression
                        // test).
                        let reason = json_string_field(&text, "\"reason\"");
                        if denial_status_for_reason(reason) == 402 {
                            return AuthVerdict::Denied {
                                status: 402,
                                reason: REASON_INSUFFICIENT_BALANCE,
                                source: CacheSource::Miss,
                            };
                        }
                        cache
                            .set_if_unchanged(
                                epoch_before,
                                &tenant_id,
                                &api_key_owned,
                                false,
                                deny_ttl,
                            )
                            .await;
                        return AuthVerdict::Denied {
                            status: 401,
                            reason: "denied",
                            source: CacheSource::Miss,
                        };
                    }
                    // Contract guard: a 2xx verdict MUST be a parseable JSON
                    // object (the §11.3 / Dogress response shapes). A webpage,
                    // HTML/WAF capture page, empty or unparseable body is NOT a
                    // trustworthy decision — treat it like any other service
                    // anomaly (fail_mode: fail-closed ⇒ 503 deny, NOT cached)
                    // instead of silently allowing (e.g. an auth_url that 301s
                    // to a login page previously ALLOWED every key).
                    if !auth_body_is_json_object(&text) {
                        warn!(tenant = %tenant_id, status, "auth upstream 2xx body is not a JSON verdict; treating as unavailable");
                        return fail_mode_verdict(fail_mode);
                    }
                    // An EXPLICIT allow verdict is required — the mirror of
                    // the guard above. A 2xx JSON object carrying neither
                    // flag (an empty `{}`, a bare `{"error":"invalid key"}`
                    // envelope, another service's schema) is not a decision:
                    // reading it as an allow authorized every key for the
                    // allow TTL whenever the tenant auth service changed
                    // shape or answered an error body with 200.
                    if !body_says_allowed(&text) {
                        warn!(tenant = %tenant_id, status, "auth upstream 2xx body carries no explicit allow verdict; treating as unavailable");
                        return fail_mode_verdict(fail_mode);
                    }
                    // optional `expires_in` overrides the default allow TTL
                    // (design §11.3).
                    let effective_ttl = parse_expires_in(&text)
                        .map(Duration::from_secs)
                        .unwrap_or(ttl);
                    cache
                        .set_if_unchanged(
                            epoch_before,
                            &tenant_id,
                            &api_key_owned,
                            true,
                            effective_ttl,
                        )
                        .await;
                    decide(Verdict::Miss, 401, "denied") // → Allowed{Miss}
                }
                CacheOp::Set {
                    allowed: false,
                    ttl,
                } => {
                    // 401/403: cache the denial with the deny TTL (design §11.2).
                    cache
                        .set_if_unchanged(epoch_before, &tenant_id, &api_key_owned, false, ttl)
                        .await;
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
// Tiny JSON helpers — `serde_json` IS a direct optional dependency (gated
// behind `runtime`, always on in server builds); the hand-built request and
// the defensive flat-field scans below remain dependency-light and precise
// for the tiny contract shapes. A real `serde_json` parse is used only for
// the 2xx contract guard ([`auth_body_is_json_object`]) where correctness,
// not allocation, is what matters.
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

/// Read the TOP-LEVEL boolean verdict flag `key` ("status" or "allowed") from a
/// JSON object body.
///
/// Uses `serde_json` rather than a substring scan so the verdict can only come
/// from a real top-level member: a nested `{"data":{"status":true}}`, or the
/// word `"status":true` inside a STRING, must not be able to decide whether a
/// key is authorized. Returns `None` when the body is not a JSON object, the
/// member is absent, or it is not a boolean.
fn top_level_bool(body: &str, key: &str) -> Option<bool> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value.as_object()?.get(key)?.as_bool()
}

/// Whether a 2xx auth response body carries an explicit DENIAL flag:
/// `{"status":false}` (Dogress `crates/api` `AuthApiKeyResponse.status`) or
/// `{"allowed":false}` (design §11.3 optional refinement).
///
/// `pub(crate)` so the admin auth-url test endpoint can classify the mock
/// response exactly as the proxy would.
pub(crate) fn body_says_denied(body: &str) -> bool {
    matches!(top_level_bool(body, "status"), Some(false))
        || matches!(top_level_bool(body, "allowed"), Some(false))
}

/// Whether a 2xx auth response body carries an explicit ALLOW flag
/// (`{"status":true}` or `{"allowed":true}`) — the mirror of
/// [`body_says_denied`], and the security-relevant half.
///
/// A 2xx JSON object with NEITHER flag is NOT a decision. Reading it as an
/// allow meant that an empty `{}`, a `{"error":"invalid api key"}` envelope, or
/// any other service's schema authorized EVERY key for the full allow TTL, and
/// propagated to the fleet through the Redis L2. The caller treats a missing
/// verdict exactly like the non-JSON case: a service anomaly, fail_mode,
/// never cached.
pub(crate) fn body_says_allowed(body: &str) -> bool {
    matches!(top_level_bool(body, "status"), Some(true))
        || matches!(top_level_bool(body, "allowed"), Some(true))
}

/// Whether a 2xx auth response body is a parseable JSON **object** (the
/// §11.3 / Dogress `AuthApiKeyResponse` shapes). Everything else — HTML,
/// empty, malformed, or a JSON array/scalar — is NOT a valid verdict and
/// must never silently allow (an auth_url that returns a webpage, WAF
/// capture page or redirect landing page previously allowed every key).
pub(crate) fn auth_body_is_json_object(body: &str) -> bool {
    matches!(
        serde_json::from_str::<serde_json::Value>(body),
        Ok(serde_json::Value::Object(_)),
    )
}

/// Extract the value of a top-level JSON string field, whitespace tolerant, in
/// the flat-scan style of parse_expires_in. `field` must
/// be the QUOTED key token (e.g. "reason", quotes included) so the split
/// consumes the key and the remainder starts at the `:` separator. Returns the
/// value without its surrounding quotes; None when the field is absent or is
/// not a quoted string. Escaped characters (\") inside the value are skipped
/// when locating the closing quote — the value is returned verbatim, which is
/// safe for the fixed vocabulary used here (no embedded escapes occur).
/// Structurally safe for the flat contract bodies — see body_says_denied.
///
/// pub(crate) so the admin auth-url test endpoint classifies the reason
/// exactly as the proxy would.
pub(crate) fn json_string_field<'a>(body: &'a str, field: &str) -> Option<&'a str> {
    let rest = body.split_once(field)?.1.trim_start();
    let rest = rest.strip_prefix(":")?.trim_start();
    let rest = rest.strip_prefix("\"")?;
    let bytes = rest.as_bytes();
    let mut end = bytes.len();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x5C {
            i += 2; // skip the escaped character (quote or backslash)
        } else if b == 0x22 {
            end = i;
            break;
        } else {
            i += 1;
        }
    }
    Some(&rest[..end])
}

/// Hex-encode a SHA-256 digest for the L2 key (no base64 dep needed).
#[cfg(feature = "cluster-redis")]
fn hex_digest(hash: &[u8; 32]) -> String {
    // One owner for the digest -> hex-string form (audit L-1): the same string
    // must address the L1 key, the L2 key and the invalidation stream payload.
    let mut out = String::with_capacity(64);
    for b in hash {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// The digest → hex-string form is `cluster-redis`-only in production code
/// (`hex_digest` above), so this equivalence test is too.
#[cfg(all(test, feature = "cluster-redis"))]
mod hex_digest_tests {
    /// The L2 key suffix and the stream digest are built by different call
    /// paths; they must stay byte-identical.
    #[test]
    fn hex_digest_matches_the_shared_helper() {
        let hash = hydra_core::auth::sha256_hex(b"sk-test");
        assert_eq!(
            super::hex_digest(&hash),
            hydra_core::auth::sha256_hex_string(b"sk-test")
        );
    }
}

/// Parse a 64-char hex string into its 32 raw bytes (the inverse of
/// [`hex_digest`]); `None` when the string is not a valid 32-byte digest.
/// Not feature-gated: the L1 path of `invalidate_hashes` always needs it.
fn hex_to_bytes32(s: &str) -> Option<[u8; 32]> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        let hi = hex_val(b[2 * i])?;
        let lo = hex_val(b[2 * i + 1])?;
        out[i] = hi * 16 + lo;
    }
    Some(out)
}

/// The value of a single hex digit (`0-9a-fA-F`), or `None` if not hex.
fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
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
    fn auth_body_is_json_object_cases() {
        // valid contract shapes → true
        assert!(auth_body_is_json_object("{}"));
        assert!(auth_body_is_json_object("{\"status\":true}"));
        assert!(auth_body_is_json_object(
            "{\"allowed\":false,\"expires_in\":60}"
        ));
        assert!(auth_body_is_json_object("  {\"status\" : true }  "));
        // non-verdict bodies → false (must never silently allow)
        assert!(!auth_body_is_json_object(""));
        assert!(!auth_body_is_json_object(
            "<html><body>sign in</body></html>"
        ));
        assert!(!auth_body_is_json_object("<!DOCTYPE html>"));
        assert!(!auth_body_is_json_object("[1,2,3]"));
        assert!(!auth_body_is_json_object("\"just a string\""));
        assert!(!auth_body_is_json_object("42"));
        assert!(!auth_body_is_json_object("{not json"));
        assert!(!auth_body_is_json_object("not json"));
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
    fn json_string_field_extracts_reason() {
        assert_eq!(
            json_string_field(
                r#"{"status":false,"reason":"insufficient_balance"}"#,
                "\"reason\""
            ),
            Some("insufficient_balance")
        );
        assert_eq!(
            json_string_field(
                r#"{ "reason" : "invalid_key" , "status":false}"#,
                "\"reason\""
            ),
            Some("invalid_key")
        );
    }

    #[test]
    fn json_string_field_absent_or_non_string() {
        assert_eq!(json_string_field(r#"{"status":false}"#, "\"reason\""), None);
        assert_eq!(json_string_field(r#"{"reason":42}"#, "\"reason\""), None);
        assert_eq!(json_string_field("not json", "\"reason\""), None);
        assert_eq!(
            json_string_field(r#"{"reason":""}"#, "\"reason\""),
            Some("")
        );
    }
}
