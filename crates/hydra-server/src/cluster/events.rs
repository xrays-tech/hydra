//! # Invalidation bus (cluster P4)
//!
//! Auth-cache invalidations travel as **Redis Streams** entries
//! (`hydra:{ctl:events}`), so every node (edge AND leader) drops the affected
//! local cache entries — and, unlike a leader-held buffer, the stream
//! **survives leader failover** (it lives in Redis, not in a leader's
//! memory). Consumers track their last-read id. A background trim keeps the
//! stream bounded to `maxlen`; when a trim removes entries the `generation`
//! counter (`hydra:{ctl:gen}`) is bumped and every node clears its local auth
//! cache — the removed events may not have reached a lagging consumer, and a
//! full clear is the safe, idempotent response. Entries carry `{tenant_id,
//! keyhashes}` (v=2; SHA-256 digests, never plaintext) — re-applying is a
//! no-op.
//!
//! **Key**: `hydra:{ctl:events}` (single-key ops, topology-safe).

use fred::clients::Pool;
use fred::prelude::*;

use crate::redis::RedisError;

/// The invalidation stream key.
pub const EVENTS_KEY: &str = "hydra:{ctl:events}";

/// `xread_map` return shape (aliased to keep call sites readable).
type StreamRows =
    std::collections::HashMap<String, Vec<(String, std::collections::HashMap<String, String>)>>;
/// The generation counter key (bumped on trim-overflow).
pub const GENERATION_KEY: &str = "hydra:{ctl:gen}";

/// One invalidation event as published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalidation {
    pub tenant_id: Option<String>,
    /// v=2 payload: SHA-256 hex digests. The plaintext api-key is never on the
    /// wire — only its digest. Empty ⇒ a whole-tenant / whole-cache clear.
    pub keyhashes: Vec<String>,
    /// v=1 legacy payload: plaintext api-keys, kept ONLY so events published
    /// before the v=2 switch replay correctly from the stream. Hashed on the
    /// spot at apply time; never re-broadcast or logged.
    pub legacy_keys: Vec<String>,
}

/// Redis Streams invalidation bus.
#[derive(Clone)]
pub struct InvalidationStream {
    pool: Pool,
}

/// SHA-256 hex digest of a plaintext api-key (the v=2 stream payload). Module-
/// level so both `publish` (hash at the boundary) and `apply_invalidation`
/// (hash legacy `keys` on the spot for replay) share one implementation.
fn sha256_hex_str(s: &str) -> String {
    let mut out = String::with_capacity(64);
    for b in hydra_core::auth::sha256_hex(s.as_bytes()) {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

impl InvalidationStream {
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Publish one invalidation (`None` tenant ⇒ all tenants). The api-keys are
    /// hashed here at the publish boundary: the stream carries SHA-256 digests
    /// only, so the plaintext never leaves this node (F-1).
    pub async fn publish(
        &self,
        tenant_id: Option<String>,
        api_keys: Vec<String>,
    ) -> Result<String, RedisError> {
        let mut fields: Vec<(&str, String)> = vec![("v", "2".into())];
        if let Some(t) = &tenant_id {
            fields.push(("tenant", t.clone()));
        }
        if !api_keys.is_empty() {
            // Each key is hashed WHOLE before the comma-join, so a key that
            // itself contains a comma yields one unambiguous digest.
            let keyhashes: Vec<String> = api_keys
                .iter()
                .map(|k| sha256_hex_str(k.as_str()))
                .collect();
            fields.push(("keyhashes", keyhashes.join(",")));
        }
        let id: String = self.pool.xadd(EVENTS_KEY, false, None, "*", fields).await?;
        Ok(id)
    }

    /// Read events newer than `last_id` (up to `count`). `"0"` reads from the
    /// start (idempotent replay on reconnect / restart).
    pub async fn read_since(
        &self,
        last_id: &str,
        count: u64,
    ) -> Result<Vec<(String, Invalidation)>, RedisError> {
        // Real Redis replies NIL when the stream has no newer entries, while
        // the in-process double replies an empty array — fred's typed
        // `xread_map` conversion chokes on the NIL ("Cannot convert to map"),
        // which turned an idle stream into an infinite parse-error retry
        // loop. Read the raw `Value` and treat both shapes as "no events".
        let resp: fred::types::Value = self
            .pool
            .xread(Some(count), None, vec![EVENTS_KEY], vec![last_id])
            .await?;
        if resp.is_null() || resp.array_len() == Some(0) {
            return Ok(Vec::new());
        }
        let rows: StreamRows = resp
            .flatten_array_values(2)
            .convert()
            .map_err(RedisError::from)?;
        let mut out = Vec::new();
        for (_key, entries) in rows {
            for (id, fields) in entries {
                let mut tenant_id = None;
                let mut keyhashes = Vec::new();
                let mut legacy_keys = Vec::new();
                for (k, v) in fields {
                    match k.as_str() {
                        "tenant" => tenant_id = Some(v),
                        // v=2: SHA-256 hex digests.
                        "keyhashes" => {
                            keyhashes = v.split(',').map(str::to_string).collect()
                        }
                        // v=1 legacy: plaintext keys (replayed, hashed at apply).
                        "keys" => legacy_keys = v.split(',').map(str::to_string).collect(),
                        _ => {}
                    }
                }
                out.push((
                    id,
                    Invalidation {
                        tenant_id,
                        keyhashes,
                        legacy_keys,
                    },
                ));
            }
        }
        Ok(out)
    }

    /// Trim the stream to `maxlen` entries. Returns the number removed. The
    /// periodic trim task ([`trim_and_maybe_bump`] / [`spawn_trim_task`]) bumps
    /// the generation whenever a trim actually removes entries.
    pub async fn trim(&self, maxlen: u64) -> Result<i64, RedisError> {
        let removed: i64 = self
            .pool
            .xtrim(
                EVENTS_KEY,
                (
                    fred::types::streams::XCapKind::MaxLen,
                    fred::types::streams::XCapTrim::Exact,
                    maxlen,
                ),
            )
            .await?;
        Ok(removed)
    }

    /// Bump the generation counter (trim overflow): nodes observing a bump
    /// clear their local auth caches.
    pub async fn bump_generation(&self) -> Result<i64, RedisError> {
        let n: i64 = self.pool.incr(GENERATION_KEY).await?;
        Ok(n)
    }

    /// Current generation (`0` when never bumped).
    pub async fn generation(&self) -> Result<i64, RedisError> {
        let g: Option<i64> = self.pool.get(GENERATION_KEY).await?;
        Ok(g.unwrap_or(0))
    }

    /// One trim pass (F-6): trim to `maxlen`, and if entries were removed,
    /// bump the generation so lagging consumers re-hydrate. Returns
    /// `(removed, bumped)`.
    pub async fn trim_and_maybe_bump(&self, maxlen: u64) -> Result<(i64, bool), RedisError> {
        let removed = self.trim(maxlen).await?;
        let bumped = if removed > 0 {
            self.bump_generation().await?;
            true
        } else {
            false
        };
        Ok((removed, bumped))
    }
}

/// Apply one invalidation to a local auth cache (idempotent).
pub async fn apply_invalidation(
    cache: &crate::http::AuthCache,
    inv: &Invalidation,
    known_tenants: &[String],
) -> usize {
    // Resolve the target digests: v=2 `keyhashes`, or the v=1 legacy `keys`
    // hashed on the spot (stream replay — pre-switch events must invalidate
    // the same entries; this is replay, not a compatibility fallback).
    let keyhashes: Vec<String> = if !inv.keyhashes.is_empty() {
        inv.keyhashes.clone()
    } else {
        inv.legacy_keys
            .iter()
            .map(|k| sha256_hex_str(k))
            .collect()
    };
    match (&inv.tenant_id, keyhashes.is_empty()) {
        (Some(tid), true) => cache.invalidate_tenant(tid).await,
        (Some(tid), false) => cache.invalidate_hashes(tid, &keyhashes).await,
        (None, true) => {
            // Clear every tenant's cache.
            let mut n = 0;
            for t in known_tenants {
                n += cache.invalidate_tenant(t).await;
            }
            n
        }
        (None, false) => {
            // Keys across all tenants.
            let mut n = 0;
            for t in known_tenants {
                n += cache.invalidate_hashes(t, &keyhashes).await;
            }
            n
        }
    }
}

/// Spawn the per-node invalidation consumer (cluster P4): an `XREAD` loop
/// over the stream; each event is applied to the local auth cache (idempotent
/// replay — on reconnect it re-reads from the last id); a `generation` bump
/// (the stream was trimmed past our watermark) clears the local cache.
pub fn spawn_invalidation_consumer(
    stream: InvalidationStream,
    auth: std::sync::Arc<crate::http::HttpAuthChecker>,
    store: crate::store::ConfigStore,
) {
    tokio::spawn(async move {
        let mut last_id = "0".to_string();
        let mut gen: i64 = stream.generation().await.unwrap_or(0);
        loop {
            match stream.read_since(&last_id, 100).await {
                Ok(events) => {
                    if !events.is_empty() {
                        for (id, inv) in events {
                            let known: Vec<String> = store
                                .snapshot()
                                .tenants_by_domain
                                .values()
                                .map(|t| t.id.clone())
                                .collect();
                            apply_invalidation(auth.cache(), &inv, &known).await;
                            last_id = id;
                        }
                        // Keep the local `hydra_auth_cache_size` gauge
                        // truthful: entries cleared HERE never pass through
                        // the admin invalidation handlers (which run only on
                        // the node that received the request — never on an
                        // edge data-plane node consuming the stream).
                        crate::admin::metrics::record_auth_cache_size(auth.cache().len());
                    }
                    match stream.generation().await {
                        Ok(g) if g != gen => {
                            gen = g;
                            tracing::info!(
                                generation = g,
                                "invalidation generation bumped; clearing local auth cache"
                            );
                            auth.cache().clear_all();
                            crate::admin::metrics::record_auth_cache_size(0);
                        }
                        _ => {}
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "invalidation read failed; retrying");
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    });
}

/// Spawn the periodic stream trim task (F-6): keeps the invalidation stream
/// bounded to `maxlen`. When a trim removes entries, the generation is bumped
/// (via [`InvalidationStream::trim_and_maybe_bump`]) so every node re-hydrates
/// its auth cache — a removed event may not have reached a lagging consumer,
/// and a full local clear is the safe, idempotent response.
pub fn spawn_trim_task(
    stream: InvalidationStream,
    maxlen: u64,
    interval: std::time::Duration,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // The first `tick()` completes immediately; drop it so the first trim
        // happens one interval after spawn (gives the stream time to fill).
        ticker.tick().await;
        loop {
            ticker.tick().await;
            match stream.trim_and_maybe_bump(maxlen).await {
                Ok((removed, true)) => {
                    tracing::info!(
                        removed = removed,
                        "invalidation stream trimmed past maxlen; generation bumped"
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "invalidation stream trim failed; retrying");
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Tests against the in-process Redis double
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::AuthCache;
    use crate::redis::mock::MockRedis;
    use std::time::Duration;

    async fn pool_with_mock() -> Pool {
        let mock = std::sync::Arc::new(MockRedis::new());
        let cfg = Config {
            mocks: Some(mock),
            ..Default::default()
        };
        let p = Pool::new(cfg, None, None, None, 1).expect("pool");
        p.init().await.expect("init");
        p
    }

    #[tokio::test]
    async fn publish_read_roundtrip() {
        let s = InvalidationStream::new(pool_with_mock().await);
        let id = s
            .publish(Some("t1".into()), vec!["sk-a".into(), "sk-b".into()])
            .await
            .expect("publish");
        let events = s.read_since("0", 10).await.expect("read");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, id);
        assert_eq!(events[0].1.tenant_id.as_deref(), Some("t1"));
        // v=2: the stream carries SHA-256 digests, not the plaintext keys.
        assert_eq!(
            events[0].1.keyhashes,
            vec![sha256_hex_str("sk-a"), sha256_hex_str("sk-b")]
        );
        assert!(events[0].1.legacy_keys.is_empty());

        // since=last-id → nothing new.
        let events = s.read_since(&id, 10).await.expect("read since");
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn trim_and_generation() {
        let s = InvalidationStream::new(pool_with_mock().await);
        for _ in 0..5 {
            s.publish(None, vec![]).await.expect("publish");
        }
        let removed = s.trim(2).await.expect("trim");
        assert!(removed >= 3, "trim removes the head");
        assert_eq!(s.generation().await.expect("gen"), 0);
        s.bump_generation().await.expect("bump");
        assert_eq!(s.generation().await.expect("gen2"), 1);
    }

    #[test]
    fn apply_invalidation_to_local_cache() {
        // A real in-memory AuthCache + a wiremock-free check: seed a verdict,
        // invalidate via the bus event, assert the next check goes upstream
        // (cache cleared).
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cache = AuthCache::new(Duration::from_secs(300), Duration::from_secs(30));
            cache
                .set("t1", "sk-a", true, Duration::from_secs(300))
                .await;
            assert_eq!(
                cache.check("t1", "sk-a").await,
                hydra_core::auth::Verdict::Hit(true),
                "seeded verdict is cached"
            );
            let n = apply_invalidation(
                &cache,
                &Invalidation {
                    tenant_id: Some("t1".into()),
                    keyhashes: vec![sha256_hex_str("sk-a")],
                    legacy_keys: vec![],
                },
                &["t1".into()],
            )
            .await;
            assert_eq!(n, 1);
            assert_eq!(
                cache.check("t1", "sk-a").await,
                hydra_core::auth::Verdict::Miss,
                "invalidation cleared the local entry"
            );
            // Idempotent: applying again is a no-op.
            apply_invalidation(
                &cache,
                &Invalidation {
                    tenant_id: Some("t1".into()),
                    keyhashes: vec![sha256_hex_str("sk-a")],
                    legacy_keys: vec![],
                },
                &["t1".into()],
            )
            .await;
            assert_eq!(
                cache.check("t1", "sk-a").await,
                hydra_core::auth::Verdict::Miss
            );
        });
    }

    // ---- F-1: the stream carries SHA-256 digests, never plaintext --------

    #[tokio::test]
    async fn publish_carries_hashes_not_plaintext() {
        let s = InvalidationStream::new(pool_with_mock().await);
        let _id = s
            .publish(Some("t1".into()), vec!["sk-secret-key".into(), "sk-b".into()])
            .await
            .expect("publish");
        let events = s.read_since("0", 10).await.expect("read");
        assert_eq!(events.len(), 1);
        let inv = &events[0].1;
        assert_eq!(inv.tenant_id.as_deref(), Some("t1"));
        // Exactly two digests, each 64 hex chars; the plaintext is nowhere.
        assert_eq!(inv.keyhashes.len(), 2, "two keys → two digests");
        for h in &inv.keyhashes {
            assert_eq!(h.len(), 64, "digest must be 64 hex chars: {h}");
            assert!(
                h.bytes().all(|c| c.is_ascii_hexdigit()),
                "digest must be hex: {h}"
            );
        }
        assert!(
            !inv.keyhashes.join(",").contains("sk-secret-key"),
            "plaintext key must never appear in the stream payload"
        );
        assert_eq!(inv.keyhashes[0], sha256_hex_str("sk-secret-key"));
        assert!(inv.legacy_keys.is_empty(), "v=2 carries no legacy keys");
    }

    #[tokio::test]
    async fn comma_key_invalidated_by_hash() {
        // A key containing a comma is hashed WHOLE before the comma-join, so
        // the stream carries ONE digest (never an ambiguous split).
        let s = InvalidationStream::new(pool_with_mock().await);
        let _id = s
            .publish(Some("t1".into()), vec!["a,b".into()])
            .await
            .expect("publish");
        let events = s.read_since("0", 10).await.expect("read");
        let inv = &events[0].1;
        assert_eq!(inv.keyhashes.len(), 1, "a comma key is ONE digest, not two");
        assert_eq!(inv.keyhashes[0], sha256_hex_str("a,b"));

        // A cache seeded with the SAME key is invalidated by that digest.
        let cache = AuthCache::new(Duration::from_secs(300), Duration::from_secs(30));
        cache.set("t1", "a,b", true, Duration::from_secs(300)).await;
        assert_eq!(
            cache.check("t1", "a,b").await,
            hydra_core::auth::Verdict::Hit(true),
            "seeded verdict is cached"
        );
        let n = apply_invalidation(&cache, inv, &["t1".into()]).await;
        assert_eq!(n, 1, "the comma key was invalidated by its hash");
        assert_eq!(cache.check("t1", "a,b").await, hydra_core::auth::Verdict::Miss);
    }

    #[tokio::test]
    async fn legacy_keys_replay_equivalent_to_keyhashes() {
        let pool = pool_with_mock().await;
        let s = InvalidationStream::new(pool.clone());
        // v=2 event for "sk-a".
        let _ = s
            .publish(Some("t1".into()), vec!["sk-a".into()])
            .await
            .expect("v2 publish");
        // Inject a legacy v=1 event (what a pre-switch stream entry looks like):
        // plaintext `keys` field, no `keyhashes`.
        let fields: Vec<(&str, String)> = vec![
            ("v", "1".into()),
            ("tenant", "t1".into()),
            ("keys", "sk-a".into()),
        ];
        let _legacy_id: String = pool
            .xadd(EVENTS_KEY, false, None, "*", fields)
            .await
            .expect("legacy xadd");

        let events = s.read_since("0", 20).await.expect("read");
        let inv_v2 = &events[0].1;
        let inv_legacy = &events[1].1;
        assert!(!inv_v2.keyhashes.is_empty(), "v2 has digests");
        assert!(inv_v2.legacy_keys.is_empty(), "v2 has no legacy keys");
        assert!(inv_legacy.keyhashes.is_empty(), "legacy has no digests");
        assert_eq!(
            inv_legacy.legacy_keys,
            vec!["sk-a".to_string()],
            "legacy event parses the plaintext `keys` field"
        );

        // Equivalence: applying EITHER event clears the SAME cache entry.
        let cache = AuthCache::new(Duration::from_secs(300), Duration::from_secs(30));
        cache.set("t1", "sk-a", true, Duration::from_secs(300)).await;
        assert_eq!(
            apply_invalidation(&cache, inv_v2, &["t1".into()]).await,
            1,
            "v2 event invalidated the entry"
        );
        assert_eq!(cache.check("t1", "sk-a").await, hydra_core::auth::Verdict::Miss);
        // Re-seed, apply the legacy event → the SAME entry is cleared (replay).
        cache.set("t1", "sk-a", true, Duration::from_secs(300)).await;
        assert_eq!(
            apply_invalidation(&cache, inv_legacy, &["t1".into()]).await,
            1,
            "legacy event invalidated the SAME entry (stream replay)"
        );
        assert_eq!(cache.check("t1", "sk-a").await, hydra_core::auth::Verdict::Miss);
    }

    #[test]
    fn invalidate_hashes_equivalent_to_invalidate() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // Same key, same count, same end state — via either method.
            let c1 = AuthCache::new(Duration::from_secs(300), Duration::from_secs(30));
            c1.set("t1", "sk-a", true, Duration::from_secs(300)).await;
            let c2 = AuthCache::new(Duration::from_secs(300), Duration::from_secs(30));
            c2.set("t1", "sk-a", true, Duration::from_secs(300)).await;
            let via_hashes = c1
                .invalidate_hashes("t1", &[sha256_hex_str("sk-a")])
                .await;
            let via_plain = c2.invalidate("t1", &["sk-a".to_string()]).await;
            assert_eq!(via_hashes, via_plain, "both remove exactly one entry");
            assert_eq!(via_plain, 1);
            assert_eq!(c1.len(), 0);
            assert_eq!(c2.len(), 0);
            assert_eq!(c1.check("t1", "sk-a").await, hydra_core::auth::Verdict::Miss);
            assert_eq!(c2.check("t1", "sk-a").await, hydra_core::auth::Verdict::Miss);

            // A foreign / unparseable digest is ignored (no panic, no false
            // removal).
            let c3 = AuthCache::new(Duration::from_secs(300), Duration::from_secs(30));
            c3.set("t1", "sk-a", true, Duration::from_secs(300)).await;
            assert_eq!(
                c3.invalidate_hashes("t1", &["zz".into()]).await,
                0,
                "malformed digest is ignored"
            );
            assert_eq!(c3.len(), 1, "the real entry survives");
        });
    }

    // ---- F-6: periodic trim → generation bump → consumer clear -----------

    #[tokio::test]
    async fn trim_and_maybe_bump() {
        let s = InvalidationStream::new(pool_with_mock().await);
        for _ in 0..5 {
            s.publish(None, vec![]).await.expect("publish");
        }
        assert_eq!(s.generation().await.expect("gen pre"), 0);
        // Under maxlen → nothing removed → no bump.
        let (removed, bumped) = s.trim_and_maybe_bump(10).await.expect("trim ok");
        assert_eq!(removed, 0);
        assert!(!bumped, "no removal → no generation bump");
        assert_eq!(s.generation().await.expect("gen still"), 0);
        // Over maxlen → entries removed → generation bumped.
        let (removed, bumped) = s.trim_and_maybe_bump(2).await.expect("trim remove");
        assert!(removed > 0, "trim removed {removed} entries");
        assert!(bumped, "removal → generation bump");
        assert_eq!(s.generation().await.expect("gen post"), 1);
    }

    #[tokio::test]
    async fn trim_bump_clears_consumer_cache() {
        // End-to-end: publish past maxlen → the trim task removes entries →
        // bumps the generation → the consumer observes the bump and clears its
        // local cache.
        let pool = pool_with_mock().await;
        let stream = InvalidationStream::new(pool.clone());

        let auth = std::sync::Arc::new(
            crate::http::HttpAuthChecker::new(
                AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
                crate::http::AuthConfig::default(),
            )
            .expect("checker"),
        );
        auth.cache().set("t1", "sk-a", true, Duration::from_secs(300)).await;
        assert_eq!(auth.cache().len(), 1, "seeded verdict before trim");

        // An empty store is fine: the (None, []) events are no-ops, so the
        // only thing that clears the seeded verdict is the generation bump.
        let store = crate::store::ConfigStore::from_snapshot(
            hydra_core::config::ConfigData::default(),
            std::sync::Arc::new(crate::crypto::StaticKeyProvider::new([1u8; 32], 1)),
        );

        spawn_invalidation_consumer(stream.clone(), auth.clone(), store);
        spawn_trim_task(stream.clone(), 2, Duration::from_millis(20));

        // Publish past maxlen (2) → the trim task removes 3 → bumps.
        for _ in 0..5 {
            stream.publish(None, vec![]).await.expect("publish");
        }

        // Wait for the consumer to observe the bump and clear the cache.
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if auth.cache().len() == 0 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("consumer did not clear the cache within 3s (bump not observed)");
        assert_eq!(auth.cache().len(), 0, "consumer cleared the local cache on bump");
        assert_eq!(
            stream.generation().await.expect("gen"),
            1,
            "generation bumped exactly once"
        );
    }
}
