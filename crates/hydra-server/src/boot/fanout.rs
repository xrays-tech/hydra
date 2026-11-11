//! Cluster Redis fanout wiring: the shared Redis backbone, the config
//! invalidation bus and the invalidation consumer (F6 split of `main.rs`).
//!
//! The execution ORDER is load-bearing and preserved by the call sites in
//! `boot::bootstrap`: the Redis pool is built BEFORE the config store is loaded,
//! and the config-bus watcher is started only AFTER the initial load +
//! cert-backfill reload, so a message that arrived during boot cannot roll the
//! snapshot backwards.

#[cfg(feature = "cluster-redis")]
use hydra_server::cluster::NodeRole;
#[cfg(feature = "cluster-redis")]
use hydra_server::redis::{RedisBackend, RedisMode};
#[cfg(feature = "cluster-redis")]
use hydra_server::store::ConfigStore;
#[cfg(feature = "cluster-redis")]
use tracing::info;

/// (2c-redis) Cluster Redis backbone: build the pool BEFORE the config store is
/// loaded. The pool does not depend on the store, and Phase 3b needs the
/// notifier (which borrows this pool) injected before the watcher starts.
///
/// A cluster node shares one Redis for the invalidation bus, shared limits and
/// breaker votes; config authority is the shared database, not Redis.
#[cfg(feature = "cluster-redis")]
pub(crate) async fn connect_redis(
    role: NodeRole,
    redis_url: Option<&str>,
) -> Result<Option<RedisBackend>, Box<dyn std::error::Error>> {
    if !role.is_cluster() {
        return Ok(None);
    }
    Ok(Some(
        RedisBackend::connect(
            redis_url.ok_or("HYDRA_REDIS_URL must be set in cluster mode (checked above)")?,
            RedisMode::from_env()?,
        )
        .await?,
    ))
}

/// (2c-bus) Config fanout (Phase 3b). The notifier is injected and the watcher
/// started only AFTER the initial load + cert-backfill reload, so a message that
/// arrived during boot cannot roll the snapshot backwards. The watcher gets a
/// clone; `store` is later moved into `AppState`.
///
/// Single-node builds keep `store` exactly as loaded: no notifier, no
/// subscriber, no poller.
#[cfg(feature = "cluster-redis")]
pub(crate) async fn wire_config_bus(
    store: ConfigStore,
    redis_backend: Option<&RedisBackend>,
    redis_url: Option<&str>,
    node_id: &str,
) -> Result<ConfigStore, Box<dyn std::error::Error>> {
    let mut store = store;
    if let Some(redis) = redis_backend {
        store = store.with_notifier(std::sync::Arc::new(
            hydra_server::cluster::config_bus::RedisConfigNotifier::new(
                redis.pool().clone(),
                node_id.to_string(),
            ),
        ));
        let subscriber = hydra_server::redis::build_subscriber(
            redis_url.ok_or("HYDRA_REDIS_URL must be set in cluster mode")?,
        )?;
        let poll = hydra_server::cluster::config_bus::poll_interval_from_env();
        hydra_server::cluster::config_bus::spawn_config_watch(store.clone(), subscriber, poll)
            .await?;
        info!(poll_ms = poll.as_millis(), "config bus watcher started");
    }
    Ok(store)
}

/// (2f-redis) Invalidation consumer (P4): every node consumes the invalidation
/// stream so config invalidations propagate cluster-wide.
///
/// F-6: the trim task is spawned LATER, once the live-node view exists (see the
/// call site) — without it a trim cannot tell "every consumer already applied
/// this" from "we just dropped an unread event".
#[cfg(feature = "cluster-redis")]
pub(crate) fn start_invalidation_consumer(
    redis_backend: Option<&RedisBackend>,
    node_id: &str,
) -> Option<hydra_server::cluster::events::InvalidationStream> {
    let b = redis_backend?;
    let stream = hydra_server::cluster::events::InvalidationStream::new(b.pool().clone());
    hydra_server::cluster::events::spawn_invalidation_consumer(stream.clone(), node_id.to_string());
    info!("invalidation consumer started");
    Some(stream)
}
