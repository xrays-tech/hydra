//! # Redis backbone (cluster P2+) — `cluster-redis` feature.
//!
//! One Redis serves the whole cluster's shared state (v8 plan Q5/Q6):
//!
//! | subsystem        | key(s)                                        | lands |
//! |------------------|-----------------------------------------------|-------|
//! | leader lease     | `hydra:{lease:leader}`                        | P2    |
//! | node registry    | `hydra:{nodes}` / `hydra:{node:hb}:<id>`      | P4    |
//! | invalidation bus | `hydra:{ctl:events}` / `hydra:{ctl:gen}`      | P4    |
//! | rate limits      | `hydra:{rl:role:bucket}:count|tokens` (Lua)   | P4    |
//! | breaker          | `hydra:{br}:dead:{p}` / `hydra:{br}:alldead`  | P4    |
//! | auth cache L2    | `hydra:{auth}:{tenant}:{keyhash}`             | P4    |
//!
//! ## Key-namespace rules (v8 plan §6.1 — Redis Cluster safety)
//!
//! - **hash tags**: every multi-key operation (Lua scripts, transactions)
//!   must use keys sharing a `{tag}` so they land in one hash slot;
//! - **no SCAN/MATCH across shards**: cross-key work uses single-key index
//!   structures (`hydra:{br}:alldead`, `hydra:{auth:idx}:{tenant}`);
//! - single-key commands (SET/DEL/EXPIRE) are topology-safe as-is.
//!
//! The [`RedisLeaseStore`] implements the leader-lease store (P2) with a Lua
//! compare-and-renew so a renew can never clobber another holder's lease.

pub mod auth_cache;
pub mod breaker;
pub mod mock;
pub mod rate_limit;

use fred::prelude::*;
use fred::types::{Expiration, SetOptions};

/// The leader-lease key (single key — topology-safe, plan §6.1).
pub const LEASE_KEY: &str = "hydra:{lease:leader}";

/// Errors from the Redis backbone.
#[derive(Debug, thiserror::Error)]
pub enum RedisError {
    #[error("redis: {0}")]
    Fred(#[from] fred::error::Error),
    #[error("unsupported HYDRA_REDIS_MODE '{mode}' (supported: single)")]
    UnsupportedMode { mode: String },
    #[error("redis pool failed to initialise: {0}")]
    Init(String),
}

/// Redis deployment mode (`HYDRA_REDIS_MODE`). `single` is the default and
/// the fully-wired mode; sentinel/cluster config parsing lands with the
/// topology work (P4+) — they currently fail fast at startup rather than
/// silently misbehaving.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedisMode {
    Single,
    Sentinel,
    Cluster,
}

impl RedisMode {
    /// Parse `HYDRA_REDIS_MODE` (default `single`).
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::var("HYDRA_REDIS_MODE").as_deref() {
            Ok("sentinel") => Self::Sentinel,
            Ok("cluster") => Self::Cluster,
            _ => Self::Single,
        }
    }
}

/// Shared Redis pool + lease-key plumbing. Cheap to `Clone` (fred pool is
/// ref-counted).
#[derive(Clone)]
pub struct RedisBackend {
    pool: Pool,
}

impl RedisBackend {
    /// Connect to Redis. `single` mode uses the URL directly; sentinel/cluster
    /// are rejected until wired (fail-fast startup, never silent).
    pub async fn connect(url: &str, mode: RedisMode) -> Result<Self, RedisError> {
        let config = match mode {
            RedisMode::Single => Config::from_url(url).map_err(RedisError::from)?,
            RedisMode::Sentinel => {
                return Err(RedisError::UnsupportedMode {
                    mode: "sentinel".into(),
                });
            }
            RedisMode::Cluster => {
                return Err(RedisError::UnsupportedMode {
                    mode: "cluster".into(),
                });
            }
        };
        let pool = Pool::new(config, None, None, None, 2).map_err(RedisError::from)?;
        pool.init()
            .await
            .map_err(|e| RedisError::Init(e.to_string()))?;
        Ok(Self { pool })
    }

    /// The underlying pool (shared by all subsystems).
    #[must_use]
    pub fn pool(&self) -> &Pool {
        &self.pool
    }
}

/// Leader lease stored in Redis (cluster P2): `SET <key> <node> NX PX <ms>`
/// to acquire, and a **Lua compare-and-renew** to extend — a renew only
/// succeeds while the key still holds OUR node id, so it can never clobber a
/// lease that another node acquired after we lost ours.
pub struct RedisLeaseStore {
    pool: Pool,
}

/// Atomic compare-and-renew: extend the TTL only if the current holder is us.
pub const RENEW_SCRIPT: &str = r#"
if redis.call('GET', KEYS[1]) == ARGV[1] then
  redis.call('PEXPIRE', KEYS[1], ARGV[2])
  return 1
else
  return 0
end
"#;

impl RedisLeaseStore {
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Acquire the lease (`SET NX PX`). `true` = we are the new holder.
    pub async fn try_acquire(&self, node_id: &str, lease_ms: u64) -> Result<bool, RedisError> {
        let r: Option<String> = self
            .pool
            .set(
                LEASE_KEY,
                node_id,
                Some(Expiration::PX(lease_ms as i64)),
                Some(SetOptions::NX),
                false,
            )
            .await
            .map_err(RedisError::from)?;
        Ok(r.is_some())
    }

    /// Renew while we still hold it (Lua compare-and-renew). `false` = the
    /// lease was lost (another node holds it, or it expired).
    pub async fn renew(&self, node_id: &str, lease_ms: u64) -> Result<bool, RedisError> {
        let r: i64 = self
            .pool
            .eval(
                RENEW_SCRIPT,
                vec![LEASE_KEY],
                vec![node_id, &lease_ms.to_string()],
            )
            .await
            .map_err(RedisError::from)?;
        Ok(r == 1)
    }
}

/// Convenience so `RedisLeaseStore` can be held behind `Arc<dyn LeaseStore>`
/// (see [`crate::cluster::lease`]).
impl crate::cluster::lease::LeaseStore for RedisLeaseStore {
    fn try_acquire<'a>(
        &'a self,
        node_id: &'a str,
        lease_ms: u64,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<bool, crate::cluster::lease::LeaseError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.try_acquire(node_id, lease_ms)
                .await
                .map_err(|e| crate::cluster::lease::LeaseError::Store(e.to_string()))
        })
    }

    fn renew<'a>(
        &'a self,
        node_id: &'a str,
        lease_ms: u64,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<bool, crate::cluster::lease::LeaseError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.renew(node_id, lease_ms)
                .await
                .map_err(|e| crate::cluster::lease::LeaseError::Store(e.to_string()))
        })
    }
}
