//! # Shared circuit breaker (cluster P4)
//!
//! Every node keeps its local trip logic (own consecutive failures — its own
//! network problems surface fastest locally) but **announces** trips to the
//! cluster and **converges** on a shared dead-set:
//!
//! - `vote_dead(p)`: Lua `SADD` this node's vote + add `p` to the index +
//!   `PEXPIRE` the vote (30 s). Votes are **heartbeats**: the sync task
//!   re-votes while this node still considers `p` dead, so a vote lapses
//!   automatically (TTL) when the node stops believing it;
//! - `vote_alive(p)`: remove this node's vote (local probe / success);
//! - `sync()`: read the index, count live votes per provider, apply the
//!   cluster view to the local breaker's `cluster_dead` set (`is_dead` stays
//!   a lock-free local read — zero hot-path Redis).
//!
//! **Keys** (same `{br}` hash tag → one slot): `hydra:{br}:dead:{p}` (vote
//! set) and `hydra:{br}:alldead` (index set). Single-key ops + one Lua
//! script; no SCAN.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use fred::clients::Pool;
use fred::prelude::*;

use crate::proxy::breaker_wrap::CircuitBreaker;
use crate::redis::RedisError;

/// Vote set key: `hydra:{br}:dead:{provider}` → set of voting node ids.
pub fn vote_key(provider: &str) -> String {
    format!("hydra:{{br}}:dead:{provider}")
}

/// Index set key: providers with ≥1 live vote.
pub const ALL_DEAD_KEY: &str = "hydra:{br}:alldead";

/// Vote TTL (ms): the sync task re-votes while the provider stays dead, so a
/// vote lapses 30 s after a node stops believing the provider is dead.
const VOTE_TTL_MS: u64 = 30_000;

/// Cluster-wide breaker coordination (votes + sync).
#[derive(Clone)]
pub struct SharedBreaker {
    pool: Pool,
    node_id: String,
    breaker: Arc<CircuitBreaker>,
    /// Minimum live votes for a provider to be cluster-dead (default 1).
    quorum: usize,
}

impl SharedBreaker {
    #[must_use]
    pub fn new(pool: Pool, node_id: String, breaker: Arc<CircuitBreaker>, quorum: usize) -> Self {
        Self {
            pool,
            node_id,
            breaker,
            quorum: quorum.max(1),
        }
    }

    /// Vote `provider` dead on behalf of this node (idempotent; extends the
    /// vote TTL so repeated calls act as a heartbeat).
    ///
    /// Three single-key commands (each topology-safe): SADD vote, SADD index,
    /// PEXPIRE vote. Not atomic — a crash in between may leave an index entry
    /// with no live vote; `sync()`'s lazy cleanup removes it on the next tick.
    pub async fn vote_dead(&self, provider: &str) -> Result<(), RedisError> {
        let _: i64 = self.pool.sadd(vote_key(provider), &self.node_id).await?;
        let _: i64 = self.pool.sadd(ALL_DEAD_KEY, provider).await?;
        let _: i64 = self
            .pool
            .pexpire(vote_key(provider), VOTE_TTL_MS as i64, None)
            .await?;
        Ok(())
    }

    /// Withdraw this node's vote (local revive). The index cleans itself
    /// lazily: if the vote set is empty the provider's index entry expires
    /// with it (the sync only counts live votes, and an empty vote set is
    /// skipped there too).
    pub async fn vote_alive(&self, provider: &str) -> Result<(), RedisError> {
        let _: i64 = self.pool.srem(vote_key(provider), &self.node_id).await?;
        Ok(())
    }

    /// One sync tick: (a) re-vote (heartbeat) for providers this node still
    /// considers dead locally; (b) read the cluster votes and apply the
    /// quorum view to the local breaker's `cluster_dead`.
    pub async fn sync(&self) -> Result<(), RedisError> {
        for p in self.breaker.locally_dead_providers() {
            let _ = self.vote_dead(&p).await; // heartbeat; failures are tolerated
        }
        let index: Vec<String> = self.pool.smembers(ALL_DEAD_KEY).await?;
        let mut cluster_dead = HashSet::new();
        for p in index {
            let votes: Vec<String> = self.pool.smembers(vote_key(&p)).await?;
            if votes.len() >= self.quorum {
                cluster_dead.insert(p);
            } else if votes.is_empty() {
                // Lazy cleanup: an index entry with no live votes (crash
                // between the SADDs, or all votes expired) is stale.
                let _: i64 = self.pool.srem(ALL_DEAD_KEY, &p).await?;
            }
        }
        self.breaker.apply_cluster_dead(&cluster_dead);
        Ok(())
    }
}

/// Spawn the periodic breaker sync (1 s default interval).
pub fn spawn_breaker_sync(shared: Arc<SharedBreaker>, interval: Duration) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(e) = shared.sync().await {
                tracing::warn!(error = %e, "breaker sync failed; local view retained");
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
    use crate::redis::mock::MockRedis;
    use hydra_core::breaker::BreakerConfig;

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
    async fn vote_propagates_via_sync() {
        let pool = pool_with_mock().await;
        // Node A trips locally and votes; node B (separate breaker, SAME
        // Redis) converges on the shared dead-set via sync.
        let breaker_a = Arc::new(CircuitBreaker::new(BreakerConfig::new(1)));
        let breaker_b = Arc::new(CircuitBreaker::new(BreakerConfig::new(1)));
        let a = SharedBreaker::new(pool.clone(), "a".into(), breaker_a.clone(), 1);
        let b = SharedBreaker::new(pool.clone(), "b".into(), breaker_b.clone(), 1);

        breaker_a.on_failure("p1"); // trips locally
        a.vote_dead("p1").await.expect("vote");

        // B has not tripped locally but the cluster view excludes p1.
        b.sync().await.expect("sync");
        assert!(breaker_b.is_dead("p1"), "cluster vote excludes p1 on B");

        // A revives (probe success) and withdraws its vote; B converges back.
        breaker_a.on_success("p1");
        a.vote_alive("p1").await.expect("unvote");
        b.sync().await.expect("sync2");
        assert!(
            !breaker_b.is_dead("p1"),
            "vote withdrawn → p1 live again on B"
        );
    }

    #[tokio::test]
    async fn quorum_requires_enough_votes() {
        let pool = pool_with_mock().await;
        let breaker = Arc::new(CircuitBreaker::new(BreakerConfig::new(1)));
        let b = SharedBreaker::new(pool.clone(), "b".into(), breaker.clone(), 2);

        // One node votes → below quorum 2 → NOT cluster-dead.
        SharedBreaker::new(
            pool.clone(),
            "a".into(),
            Arc::new(CircuitBreaker::new(BreakerConfig::new(1))),
            2,
        )
        .vote_dead("p1")
        .await
        .expect("vote a");
        b.sync().await.expect("sync");
        assert!(!breaker.is_dead("p1"), "single vote below quorum 2");

        // Second node votes → quorum met.
        SharedBreaker::new(
            pool.clone(),
            "c".into(),
            Arc::new(CircuitBreaker::new(BreakerConfig::new(1))),
            2,
        )
        .vote_dead("p1")
        .await
        .expect("vote c");
        b.sync().await.expect("sync2");
        assert!(breaker.is_dead("p1"), "two votes meet quorum 2");
    }
}
