//! # Auth-cache L2 over Redis (cluster P4)
//!
//! The per-node L1 `AuthCache` (DashMap) stays the fast path (zero network).
//! On an L1 miss, the L2 (Redis) is consulted before hitting the tenant's
//! `auth_url`, so a cold node / restart is served verdicts the cluster
//! already resolved. Keys are `hydra:{auth}:{tenant}:{keyhash}` with a
//! per-tenant index `hydra:{auth:idx}:{tenant}` (single-key ops only — no
//! SCAN, plan §6.1). The plaintext api-key never appears; only its SHA-256
//! hex.

use std::time::Duration;

use fred::clients::Pool;
use fred::prelude::*;

use crate::redis::RedisError;

/// L2 key: `hydra:{auth}:{tenant}:{keyhash_hex}`.
fn l2_key(tenant_id: &str, key_hash_hex: &str) -> String {
    format!("hydra:{{auth}}:{tenant_id}:{key_hash_hex}")
}

/// Tenant index key: `hydra:{auth:idx}:{tenant}` → set of key hashes.
fn idx_key(tenant_id: &str) -> String {
    format!("hydra:{{auth:idx}}:{tenant_id}")
}

/// Fleet-wide tenant index: `hydra:{auth:idx}` → set of tenant ids that hold at
/// least one L2 verdict.
///
/// Needed because a WHOLE-cache clear cannot enumerate tenants otherwise (the
/// namespace rules allow single-key ops only — no `SCAN`), and clearing only the
/// L1 is not a clear at all: `AuthCache::check` re-hydrates an L1 miss from the
/// L2. Bounded by the number of tenants, never by the number of keys.
pub const GLOBAL_IDX_KEY: &str = "hydra:{auth:idx}";

/// Redis-backed auth-verdict L2 cache.
#[derive(Clone)]
pub struct RedisAuthL2 {
    pool: Pool,
}

impl RedisAuthL2 {
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Read a verdict: `(allowed, remaining_ttl)` when present.
    pub async fn get(
        &self,
        tenant_id: &str,
        key_hash_hex: &str,
    ) -> Result<Option<(bool, Duration)>, RedisError> {
        let key = l2_key(tenant_id, key_hash_hex);
        let raw: Option<String> = self.pool.get(&key).await?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let allowed = match raw.as_str() {
            "1" => true,
            "0" => false,
            _ => return Ok(None), // unparseable → treat as absent
        };
        let ttl_ms: i64 = self.pool.pttl(&key).await?;
        let ttl = Duration::from_millis(ttl_ms.max(0) as u64);
        Ok(Some((allowed, ttl)))
    }

    /// Store a verdict with its TTL and record the key hash in the tenant index
    /// (and the tenant in the fleet-wide index).
    ///
    /// **Index first, value second** (N2): `del_tenant` enumerates the index, so
    /// a value written without index membership would survive a tenant-wide
    /// invalidation (suspension / 欠费停机) for its whole TTL. A dangling index
    /// member is harmless — it only makes a later `del` of a key that is not
    /// there — which is why this is the safe order of the two writes.
    pub async fn set(
        &self,
        tenant_id: &str,
        key_hash_hex: &str,
        allowed: bool,
        ttl: Duration,
    ) -> Result<(), RedisError> {
        let _: i64 = self.pool.sadd(idx_key(tenant_id), key_hash_hex).await?;
        let _: i64 = self.pool.sadd(GLOBAL_IDX_KEY, tenant_id).await?;
        let key = l2_key(tenant_id, key_hash_hex);
        let _: Option<String> = self
            .pool
            .set(
                &key,
                if allowed { "1" } else { "0" },
                Some(fred::types::Expiration::PX(ttl.as_millis() as i64)),
                None,
                false,
            )
            .await?;
        Ok(())
    }

    /// Remove one verdict (key + index entry).
    pub async fn del(&self, tenant_id: &str, key_hash_hex: &str) -> Result<(), RedisError> {
        let _: i64 = self.pool.del(l2_key(tenant_id, key_hash_hex)).await?;
        let _: i64 = self.pool.srem(idx_key(tenant_id), key_hash_hex).await?;
        Ok(())
    }

    /// Remove every verdict of a tenant (index-driven; no SCAN).
    pub async fn del_tenant(&self, tenant_id: &str) -> Result<(), RedisError> {
        let idx = idx_key(tenant_id);
        let members: Vec<String> = self.pool.smembers(&idx).await?;
        for m in &members {
            let _: i64 = self.pool.del(l2_key(tenant_id, m)).await?;
        }
        let _: i64 = self.pool.del(&idx).await?;
        Ok(())
    }

    /// Remove every verdict of EVERY tenant (fleet-wide clear; index-driven, no
    /// SCAN). Returns the number of tenants cleared.
    ///
    /// This is what makes a cluster-wide "re-auth everything" (the generation
    /// bump that compensates for a trimmed invalidation) actually effective:
    /// with the L2 left in place, the next L1 miss re-hydrates the very verdict
    /// the clear was supposed to drop.
    pub async fn del_all_tenants(&self) -> Result<usize, RedisError> {
        let tenants: Vec<String> = self.pool.smembers(GLOBAL_IDX_KEY).await?;
        for t in &tenants {
            self.del_tenant(t).await?;
        }
        let _: i64 = self.pool.del(GLOBAL_IDX_KEY).await?;
        Ok(tenants.len())
    }
}

// ---------------------------------------------------------------------------
// Tests against the in-process Redis double
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A REAL Redis on its own database (dev-plan 铁律 2: no in-process mock).
    async fn l2() -> RedisAuthL2 {
        RedisAuthL2::new(crate::redis::test_redis::isolated_pool().await)
    }

    #[tokio::test]
    async fn set_get_del_roundtrip() {
        let l2 = l2().await;
        assert!(l2.get("t1", "abc").await.expect("get").is_none());
        l2.set("t1", "abc", true, Duration::from_secs(300))
            .await
            .expect("set");
        let (allowed, ttl) = l2.get("t1", "abc").await.expect("get2").unwrap();
        assert!(allowed);
        assert!(ttl > Duration::from_secs(250), "TTL ~ the allow TTL");

        l2.del("t1", "abc").await.expect("del");
        assert!(l2.get("t1", "abc").await.expect("get3").is_none());
    }

    #[tokio::test]
    async fn auth_cache_l1_miss_hydrates_from_l2() {
        let l2 = std::sync::Arc::new(l2().await);
        let cache = crate::http::AuthCache::new(Duration::from_secs(300), Duration::from_secs(30))
            .with_l2(l2.clone());
        // Seed the L2 directly (simulating another node's verdict).
        let hash = hydra_core::auth::sha256_hex(b"sk-a");
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        l2.set("t1", &hex, true, Duration::from_secs(300))
            .await
            .expect("seed l2");

        // L1 is empty → L2 hit, hydrated into L1.
        assert_eq!(cache.len(), 0);
        assert_eq!(
            cache.check("t1", "sk-a").await,
            hydra_core::auth::Verdict::Hit(true),
            "L2 serves the verdict on an L1 miss"
        );
        assert_eq!(cache.len(), 1, "L1 hydrated from L2");
    }

    #[tokio::test]
    async fn tenant_index_clears_all() {
        let l2 = l2().await;
        l2.set("t1", "k1", true, Duration::from_secs(60))
            .await
            .expect("k1");
        l2.set("t1", "k2", false, Duration::from_secs(30))
            .await
            .expect("k2");
        l2.set("t2", "k3", true, Duration::from_secs(60))
            .await
            .expect("k3");
        l2.del_tenant("t1").await.expect("clear t1");
        assert!(l2.get("t1", "k1").await.expect("a").is_none());
        assert!(l2.get("t1", "k2").await.expect("b").is_none());
        assert!(
            l2.get("t2", "k3").await.expect("c").is_some(),
            "other tenant untouched"
        );
    }

    /// T7 (design §4.2.5) — the allow-TTL cap must reach the **L2**, or the
    /// residual window is not bounded at all in a cluster.
    ///
    /// The design puts the cap on "L1/L2 allow entries", and the L2 is the
    /// authority for a node whose own L1 expired: `check` re-hydrates an L1
    /// miss from the L2 (see `clear_all_clears_the_l2_of_every_tenant` below).
    /// So if `set` clamped the L1 but pushed the tenant's raw `expires_in` into
    /// Redis, a stalled node would keep re-filling an ALLOW from the L2 long
    /// past the cap — the cap would look enforced while the window stayed open.
    #[tokio::test]
    async fn a_capped_allow_has_a_capped_l2_ttl() {
        let l2 = std::sync::Arc::new(l2().await);
        let cache = crate::http::AuthCache::new(Duration::from_secs(300), Duration::from_secs(30))
            .with_allow_ttl_max(Duration::from_secs(60))
            .with_l2(l2.clone());

        // The tenant asks for a day.
        cache
            .set("t7-cap", "sk-long", true, Duration::from_secs(86_400))
            .await;

        let (allowed, ttl) = l2
            // The one shared digest form (`hydra_core::auth::sha256_hex_string`),
            // which is what `hex_digest` delegates to.
            .get("t7-cap", &hydra_core::auth::sha256_hex_string(b"sk-long"))
            .await
            .expect("l2 get")
            .expect("the verdict must have been written to the L2");
        assert!(allowed, "an allow verdict was written");
        assert!(
            ttl <= Duration::from_secs(60),
            "the L2 must carry the CAPPED ttl, got {ttl:?} — an uncapped L2 entry \
             keeps a stalled node's stale allow alive past the cap"
        );
    }

    /// REVIEW B2 — the whole-cache clear (cluster generation bump) must clear
    /// the L2 as well, for EVERY tenant.
    ///
    /// `AuthCache::clear_all` used to clear only the L1 map, and `check`
    /// re-hydrates an L1 miss from the L2. So when the invalidation stream was
    /// trimmed past a node's watermark, the compensating full clear dropped the
    /// L1 but the very next request for an affected key was served the STALE
    /// verdict from the L2 — for the rest of that verdict's TTL, which
    /// `expires_in` can raise far beyond the 300 s default (`http.rs`).
    #[tokio::test]
    async fn clear_all_clears_the_l2_of_every_tenant() {
        let l2 = std::sync::Arc::new(l2().await);
        let cache = crate::http::AuthCache::new(Duration::from_secs(300), Duration::from_secs(30))
            .with_l2(l2.clone());

        // Two tenants, each with a cached ALLOW in both L1 and L2.
        cache
            .set("t1", "sk-a", true, Duration::from_secs(300))
            .await;
        cache
            .set("t2", "sk-b", true, Duration::from_secs(300))
            .await;
        assert_eq!(
            cache.check("t1", "sk-a").await,
            hydra_core::auth::Verdict::Hit(true)
        );
        assert_eq!(
            cache.check("t2", "sk-b").await,
            hydra_core::auth::Verdict::Hit(true)
        );

        cache.clear_all().await;

        assert_eq!(cache.len(), 0, "L1 cleared");
        assert_eq!(
            cache.check("t1", "sk-a").await,
            hydra_core::auth::Verdict::Miss,
            "the L2 must not resurrect an invalidated verdict"
        );
        assert_eq!(
            cache.check("t2", "sk-b").await,
            hydra_core::auth::Verdict::Miss,
            "EVERY tenant must be cleared, not just the last one written"
        );
    }

    /// REGRESSION (bug-2026-09-16-auth-cache-guard-deadlock) — an L2 hit must
    /// not self-deadlock while back-filling the L1.
    ///
    /// `AuthCache::check` bound the DashMap shard read guard (`Ref`) to a named
    /// local and then `.await`ed the Redis L2 with it still alive. On an L2 hit
    /// it called `self.map.insert(...)` for the SAME key — the same shard — so
    /// the task waited for a write lock it was itself holding: a permanent
    /// self-deadlock. Pingora runs one worker thread per service, so the whole
    /// data plane of that replica stopped accepting while the admin port (a
    /// different thread) kept answering `/healthz` 200 and k8s kept the pod in
    /// the Service — half the traffic silently timed out (production: 5/10
    /// requests hung for 6 s, no self-healing).
    ///
    /// The trigger is precise: the L1 entry must be PRESENT BUT EXPIRED (a Miss
    /// that still yields a shard guard). `DashMap::get` releases the guard
    /// immediately when the key is ABSENT — which is why the plain "cold L1"
    /// test above (`auth_cache_l1_miss_hydrates_from_l2`) never caught this.
    ///
    /// Shape of the test: `check` runs on its OWN thread with its own runtime,
    /// and this thread waits on a channel with a timeout. It has to be that way
    /// — the failure is a synchronous futex wait inside a single `poll`, so a
    /// `tokio::time::timeout` around `check` never fires and a plain test would
    /// hang the suite instead of failing. The runtime is deliberately leaked on
    /// the worker thread for the same reason: dropping it would wait for the
    /// parked worker.
    #[test]
    fn an_expired_l1_entry_does_not_deadlock_on_l2_backfill() {
        for allowed in [true, false] {
            let (tx, rx) = std::sync::mpsc::channel::<String>();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("test runtime");
                rt.block_on(async move {
                    let l2 = std::sync::Arc::new(l2().await);
                    let t0 = std::time::Instant::now();
                    let now = std::sync::Arc::new(std::sync::Mutex::new(t0));
                    let clock: crate::http::Clock = {
                        let now = now.clone();
                        std::sync::Arc::new(move || *now.lock().expect("test clock"))
                    };
                    let cache = crate::http::AuthCache::with_clock(
                        Duration::from_secs(300),
                        Duration::from_secs(30),
                        clock,
                    )
                    .with_l2(l2.clone());

                    // (1) L1 gets a 1 s entry; the L2 gets a live verdict for the
                    //     same key (i.e. "L1 cold, L2 warm").
                    cache
                        .set("t1", "sk-a", allowed, Duration::from_secs(1))
                        .await;
                    assert_eq!(cache.len(), 1, "seeded L1");

                    // (2) Time moves past the L1 TTL. The entry stays IN the map
                    //     (no GC sweep ran) while the decision becomes Miss —
                    //     the deadlock's precondition.
                    *now.lock().expect("test clock") = t0 + Duration::from_secs(2);
                    let hex = hydra_core::auth::sha256_hex_string(b"sk-a");
                    l2.set("t1", &hex, allowed, Duration::from_secs(300))
                        .await
                        .expect("seed l2");

                    // (3) This is the call that used to block forever on the
                    //     shard write lock it was itself holding.
                    let verdict = cache.check("t1", "sk-a").await;
                    assert_eq!(cache.len(), 1, "the L1 was back-filled from the L2");
                    let _ = tx.send(format!("{verdict:?}"));
                });
                // A pre-fix deadlock parks a worker thread inside a futex wait;
                // dropping the runtime would wait for it, so leak it and let the
                // process exit clean up.
                std::mem::forget(rt);
            });

            match rx.recv_timeout(std::time::Duration::from_secs(10)) {
                Ok(verdict) => assert_eq!(
                    verdict,
                    format!("{:?}", hydra_core::auth::Verdict::Hit(allowed)),
                    "allowed={allowed}"
                ),
                Err(_) => panic!(
                    "check() self-deadlocked (allowed={allowed}): a DashMap shard read guard \
                     was held across the Redis L2 await, so the back-filling insert could never \
                     take the write lock"
                ),
            }
        }
    }

    /// REGRESSION (found while fixing bug-2026-09-16) — the L1 sweep must
    /// actually run.
    ///
    /// `AuthCache::gc` carried the doc comment "the sweep a background GC task
    /// calls" and had NO caller anywhere in the binary: expired entries were
    /// only removed by being overwritten or invalidated. That left two real
    /// problems — the map grew for the life of the process (denials are cached
    /// too, so rotating keys grow it without bound), and the expired-but-present
    /// entries that fed the `check` self-deadlock were never cleaned up.
    #[tokio::test]
    async fn the_gc_task_evicts_expired_entries() {
        let t0 = std::time::Instant::now();
        let now = std::sync::Arc::new(std::sync::Mutex::new(t0));
        let clock: crate::http::Clock = {
            let now = now.clone();
            std::sync::Arc::new(move || *now.lock().expect("test clock"))
        };
        let cache = crate::http::AuthCache::with_clock(
            Duration::from_secs(300),
            Duration::from_secs(30),
            clock,
        );
        cache.set("t1", "sk-a", true, Duration::from_secs(1)).await;
        cache
            .set("t1", "sk-b", true, Duration::from_secs(300))
            .await;
        assert_eq!(cache.len(), 2);

        let auth = std::sync::Arc::new(
            crate::http::HttpAuthChecker::new(cache, crate::http::AuthConfig::default())
                .expect("checker"),
        );
        crate::http::spawn_gc_task(auth.clone(), Duration::from_millis(20));

        // Nothing is due yet (the clock has not moved).
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(auth.cache().len(), 2, "live entries must survive a sweep");

        // Expire ONE of them; the sweep must drop exactly that one.
        *now.lock().expect("test clock") = t0 + Duration::from_secs(2);
        for _ in 0..100 {
            if auth.cache().len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            auth.cache().len(),
            1,
            "the sweep must evict the expired entry (and only it)"
        );
        assert_eq!(
            auth.cache().check("t1", "sk-b").await,
            hydra_core::auth::Verdict::Hit(true),
            "the live entry is still served"
        );
    }

    /// REVIEW N2 — the index is written BEFORE the value, so a value can never
    /// exist without index membership (which would make it survive
    /// `del_tenant`).
    #[tokio::test]
    async fn index_is_written_before_the_value() {
        let l2 = l2().await;
        l2.set("t1", "k1", true, Duration::from_secs(60))
            .await
            .expect("set");
        // The index names the key hash, and the fleet-wide index names the
        // tenant — both must be in place alongside the value.
        let members: Vec<String> = l2.pool.smembers(idx_key("t1")).await.expect("tenant index");
        assert_eq!(members, vec!["k1".to_string()]);
        let tenants: Vec<String> = l2
            .pool
            .smembers(GLOBAL_IDX_KEY)
            .await
            .expect("global index");
        assert!(tenants.contains(&"t1".to_string()));
    }

    /// A fleet-wide clear removes every tenant's verdicts (B2's mechanism), so
    /// `del_all_tenants` is exercised independently of `AuthCache`.
    #[tokio::test]
    async fn del_all_tenants_clears_the_fleet() {
        let l2 = l2().await;
        l2.set("t1", "k1", true, Duration::from_secs(60))
            .await
            .expect("k1");
        l2.set("t2", "k2", true, Duration::from_secs(60))
            .await
            .expect("k2");
        let cleared = l2.del_all_tenants().await.expect("clear fleet");
        assert_eq!(cleared, 2, "two tenants cleared");
        assert!(l2.get("t1", "k1").await.expect("a").is_none());
        assert!(l2.get("t2", "k2").await.expect("b").is_none());
    }
}
