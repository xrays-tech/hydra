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

    /// Store a verdict with its TTL and add the key hash to the tenant index.
    pub async fn set(
        &self,
        tenant_id: &str,
        key_hash_hex: &str,
        allowed: bool,
        ttl: Duration,
    ) -> Result<(), RedisError> {
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
        let _: i64 = self.pool.sadd(idx_key(tenant_id), key_hash_hex).await?;
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
}

// ---------------------------------------------------------------------------
// Tests against the in-process Redis double
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redis::mock::MockRedis;

    async fn l2() -> RedisAuthL2 {
        let mock = std::sync::Arc::new(MockRedis::new());
        let cfg = Config {
            mocks: Some(mock),
            ..Default::default()
        };
        let p = Pool::new(cfg, None, None, None, 1).expect("pool");
        p.init().await.expect("init");
        RedisAuthL2::new(p)
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
}
