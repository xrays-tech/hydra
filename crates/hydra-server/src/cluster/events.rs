//! # Invalidation bus (cluster P4)
//!
//! Auth-cache invalidations travel as **Redis Streams** entries
//! (`hydra:{ctl:events}`), so every node (edge AND leader) drops the affected
//! local cache entries — and, unlike a leader-held buffer, the stream
//! **survives leader failover** (it lives in Redis, not in a leader's
//! memory). Consumers track their last-read id; a `generation` counter
//! (`hydra:{ctl:gen}`) is bumped when the stream is trimmed past a consumer's
//! watermark, and every node then clears its local auth cache (idempotent
//! replay; entries are `{tenant_id, api_keys}` — re-applying is a no-op).
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
    pub api_keys: Vec<String>,
}

/// Redis Streams invalidation bus.
#[derive(Clone)]
pub struct InvalidationStream {
    pool: Pool,
}

impl InvalidationStream {
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Publish one invalidation (`None` tenant ⇒ all tenants).
    pub async fn publish(
        &self,
        tenant_id: Option<String>,
        api_keys: Vec<String>,
    ) -> Result<String, RedisError> {
        let mut fields: Vec<(&str, String)> = vec![("v", "1".into())];
        if let Some(t) = &tenant_id {
            fields.push(("tenant", t.clone()));
        }
        if !api_keys.is_empty() {
            fields.push(("keys", api_keys.join(",")));
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
                let mut api_keys = Vec::new();
                for (k, v) in fields {
                    match k.as_str() {
                        "tenant" => tenant_id = Some(v),
                        "keys" => api_keys = v.split(',').map(str::to_string).collect(),
                        _ => {}
                    }
                }
                out.push((
                    id,
                    Invalidation {
                        tenant_id,
                        api_keys,
                    },
                ));
            }
        }
        Ok(out)
    }

    /// Trim the stream to `maxlen` entries. Returns the number removed; the
    /// caller bumps the generation when a consumer's watermark was trimmed.
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
}

/// Apply one invalidation to a local auth cache (idempotent).
pub async fn apply_invalidation(
    cache: &crate::http::AuthCache,
    inv: &Invalidation,
    known_tenants: &[String],
) -> usize {
    match (&inv.tenant_id, inv.api_keys.is_empty()) {
        (Some(tid), _) => {
            if inv.api_keys.is_empty() {
                cache.invalidate_tenant(tid).await
            } else {
                cache.invalidate(tid, &inv.api_keys).await
            }
        }
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
                n += cache.invalidate(t, &inv.api_keys).await;
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
                    match stream.generation().await {
                        Ok(g) if g != gen => {
                            gen = g;
                            tracing::info!(
                                generation = g,
                                "invalidation generation bumped; clearing local auth cache"
                            );
                            auth.cache().clear_all();
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
        assert_eq!(
            events[0].1.api_keys,
            vec!["sk-a".to_string(), "sk-b".to_string()]
        );

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
                    api_keys: vec!["sk-a".into()],
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
                    api_keys: vec!["sk-a".into()],
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
}
