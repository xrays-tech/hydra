//! Redis config-change fanout (Phase 3b).
//!
//! Every node that writes the shared relational database bumps
//! `config_revision.version` and, after applying the new snapshot locally,
//! publishes `{version, origin}` on [`CHANNEL`]. The other nodes subscribe and
//! reload quietly when they see a version ahead of the one they serve. Redis is
//! only the fast path: a missed message (or a Redis outage) is repaired by the
//! poll fallback, which compares the shared watermark with the local version on
//! a timer.
//!
//! Neither path ever publishes. Only [`crate::store::ReloadSource::Local`]
//! (a write on this node) announces a change, so consuming another node's
//! message cannot echo it back and amplify into a broadcast storm.
//!
//! Startup order is load-bearing: the caller injects the notifier and starts
//! this watcher **after** the initial [`ConfigStore::load`], so a message that
//! arrives during boot cannot roll the snapshot backwards. A message whose
//! version is not ahead of the local one is ignored by the store's revision
//! gate anyway.

use std::time::Duration;

use fred::clients::SubscriberClient;
use fred::prelude::*;
use serde::{Deserialize, Serialize};

use crate::redis::RedisError;
use crate::store::{ConfigNotifier, ConfigStore, ReloadSource};

/// The single Redis pub/sub channel for config-change announcements.
pub const CHANNEL: &str = "hydra:config:changed";

/// Poll-fallback interval. Cluster/Redis only; unset or invalid means
/// [`DEFAULT_POLL_MS`].
pub const POLL_MS_ENV: &str = "HYDRA_CONFIG_POLL_MS";

/// Default poll-fallback interval (5 s). This is the correctness backstop for a
/// missed pub/sub message or a Redis outage, not the primary path.
pub const DEFAULT_POLL_MS: u64 = 5000;

/// Lower bound for [`poll_interval_from_env`] (50 ms). A smaller value turns the
/// fallback into a busy loop against the shared database.
pub const MIN_POLL_MS: u64 = 50;

/// Upper bound for [`poll_interval_from_env`] (1 hour). A larger value leaves a
/// missed pub/sub message unrepaired for too long; the dead-watcher alert
/// (`time() - hydra_config_watch_last_ok_unixtime > 3× poll interval`) also
/// needs a bound it can reason about.
pub const MAX_POLL_MS: u64 = 3_600_000;

/// Wire payload of one config-change announcement.
///
/// `origin` is the publishing node's id: it makes a message attributable in
/// logs and drills, and it mirrors the `origin` field of the fanout contract.
/// The receiver never trusts it for correctness; `version` is the gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigChanged {
    pub version: u64,
    pub origin: String,
}

/// [`ConfigNotifier`] backed by the shared Redis pool.
///
/// Cheap to clone; the fred pool is reference-counted. The origin is fixed at
/// construction so every announcement from this process carries the same node
/// id.
#[derive(Clone)]
pub struct RedisConfigNotifier {
    pool: fred::clients::Pool,
    origin: String,
    channel: String,
}

impl RedisConfigNotifier {
    #[must_use]
    pub fn new(pool: fred::clients::Pool, origin: String) -> Self {
        Self {
            pool,
            origin,
            channel: CHANNEL.to_string(),
        }
    }

    /// Same notifier on an explicit channel. **Test support**: Redis pub/sub is
    /// SERVER-GLOBAL, not database-scoped, so integration tests running in
    /// parallel would otherwise consume each other's messages. Production always
    /// goes through [`Self::new`] and [`CHANNEL`].
    #[doc(hidden)]
    #[must_use]
    pub fn with_channel(
        pool: fred::clients::Pool,
        origin: String,
        channel: impl Into<String>,
    ) -> Self {
        Self {
            pool,
            origin,
            channel: channel.into(),
        }
    }
}

#[async_trait::async_trait]
impl ConfigNotifier for RedisConfigNotifier {
    async fn publish(&self, version: u64) -> Result<(), String> {
        let payload = serde_json::to_string(&ConfigChanged {
            version,
            origin: self.origin.clone(),
        })
        .map_err(|e| format!("serialize config change: {e}"))?;
        // fred does not implement `PubsubInterface` on `Pool` (publish is not a
        // state-local command, but it is on the documented exclusion list), so
        // route it through one pooled connection as the crate recommends.
        let _: i64 = self
            .pool
            .next_connected()
            .publish(self.channel.as_str(), payload)
            .await
            .map_err(|e| format!("redis publish {}: {e}", self.channel))?;
        Ok(())
    }
}

/// Read and clamp a millisecond value from the environment.
///
/// Unset, unparseable and zero all fall back to [`DEFAULT_POLL_MS`]. A positive
/// value is clamped to [`MIN_POLL_MS`]..=[`MAX_POLL_MS`] and the clamp is
/// logged: a fat-fingered value must be visible in the startup log, not silently
/// accepted (or silently replaced, which hides the typo).
///
/// Kept pure (no process-environment mutation) so the parse can be unit-tested
/// while the suite runs in parallel.
fn poll_ms(raw: Option<&str>) -> u64 {
    let Some(raw) = raw else {
        return DEFAULT_POLL_MS;
    };
    let Ok(parsed) = raw.trim().parse::<u64>() else {
        return DEFAULT_POLL_MS;
    };
    if parsed == 0 {
        return DEFAULT_POLL_MS;
    }
    if parsed < MIN_POLL_MS {
        tracing::warn!(
            configured = parsed,
            min = MIN_POLL_MS,
            "HYDRA_CONFIG_POLL_MS is below the minimum; clamping"
        );
        return MIN_POLL_MS;
    }
    if parsed > MAX_POLL_MS {
        tracing::warn!(
            configured = parsed,
            max = MAX_POLL_MS,
            "HYDRA_CONFIG_POLL_MS is above the maximum; clamping"
        );
        return MAX_POLL_MS;
    }
    parsed
}

/// The configured poll interval.
///
/// Bounds: [`MIN_POLL_MS`] (50 ms) ..= [`MAX_POLL_MS`] (1 hour). Unset,
/// unparseable and zero all fall back to [`DEFAULT_POLL_MS`] (5 s), the same
/// fail-safe shape as the other bounded knobs: a typo must not disable the
/// correctness backstop. A positive value outside the bounds is clamped (and
/// logged), so an out-of-range setting can neither busy-loop nor disable repair.
#[must_use]
pub fn poll_interval_from_env() -> Duration {
    Duration::from_millis(poll_ms(
        std::env::var("HYDRA_CONFIG_POLL_MS").ok().as_deref(),
    ))
}

/// Start the subscriber + poll watcher.
///
/// The initial `init`/`subscribe` are awaited (a cluster node already requires
/// Redis, so a failure here is a startup failure). Once subscribed, the message
/// loop and the poller run in ONE detached task: a later disconnect is handled
/// by [`SubscriberClient::manage_subscriptions`] and does not stop the poller.
///
/// # Errors
/// When the subscriber cannot initialise or subscribe.
pub async fn spawn_config_watch(
    store: ConfigStore,
    subscriber: SubscriberClient,
    poll: Duration,
) -> Result<(), RedisError> {
    spawn_config_watch_on(store, subscriber, poll, CHANNEL).await
}

/// [`spawn_config_watch`] on an explicit channel. **Test support**: Redis
/// pub/sub is SERVER-GLOBAL (logical databases only scope keys), so integration
/// tests running in parallel must not share [`CHANNEL`]. Production always uses
/// [`spawn_config_watch`].
///
/// # Errors
/// When the subscriber cannot initialise or subscribe.
#[doc(hidden)]
pub async fn spawn_config_watch_on(
    store: ConfigStore,
    subscriber: SubscriberClient,
    poll: Duration,
    channel: &str,
) -> Result<(), RedisError> {
    let channel = channel.to_string();
    subscriber.init().await?;
    // Keep auto-resubscription alive for the lifetime of the process. The task
    // is detached on purpose: dropping this JoinHandle does not stop it, and
    // the subscriber itself is moved into the watcher task below.
    drop(subscriber.manage_subscriptions());
    subscriber.subscribe(channel.as_str()).await?;
    let mut rx = subscriber.message_rx();
    // Start the liveness clock at watcher start (not at 0): the alert is
    // `time() - gauge > 3× poll interval`, and leaving the gauge unset would
    // make a healthy node fire it for the whole first poll interval on boot.
    crate::admin::metrics::record_config_watch_ok();

    tokio::spawn(async move {
        // Keep the client alive with the task; dropping it would close the
        // subscribed connection.
        let _subscriber = subscriber;
        let mut ticker = tokio::time::interval(poll);
        // After a long stall, do not fire a burst of catch-up ticks.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick completes immediately; consume it so the first probe
        // happens one interval after startup, not during it.
        ticker.tick().await;

        loop {
            tokio::select! {
                message = rx.recv() => {
                    match message {
                        Ok(message) => {
                            let Some(payload) = message.value.as_string() else {
                                tracing::warn!(
                                    channel = %channel,
                                    "ignoring config-bus message with a non-string payload"
                                );
                                continue;
                            };
                            let changed: ConfigChanged = match serde_json::from_str(&payload) {
                                Ok(changed) => changed,
                                Err(e) => {
                                    // A malformed message is a warning, never a panic: one bad
                                    // publisher must not take a node's watcher down.
                                    tracing::warn!(
                                        channel = %channel,
                                        error = %e,
                                        "ignoring malformed config-bus message"
                                    );
                                    continue;
                                }
                            };
                            // The store's revision gate is authoritative; this comparison
                            // avoids even taking the reload lock for an already-applied or
                            // stale message.
                            if changed.version > store.version() {
                                if let Err(e) = store
                                    .reload_external_with_source(false, ReloadSource::PubSub)
                                    .await
                                {
                                    tracing::warn!(
                                        version = changed.version,
                                        origin = %changed.origin,
                                        error = %e,
                                        "config-bus reload failed; poll fallback will retry"
                                    );
                                }
                            }
                            // Liveness: a WELL-FORMED message was handled, whether or not it
                            // advanced the snapshot (a reload that failed still proves the
                            // watcher task is alive). Malformed/non-string payloads do not
                            // refresh this: a stream of garbage must not mask a wedged
                            // watcher in the alert `time() - gauge > 3× poll interval`.
                            crate::admin::metrics::record_config_watch_ok();
                        }
                        Err(e) => {
                            // A disconnected broadcast receiver is not fatal; the poll loop
                            // below keeps converging.
                            tracing::warn!(
                                channel = %channel,
                                error = %e,
                                "config-bus message stream ended"
                            );
                        }
                    }
                }
                _ = ticker.tick() => {
                    // Cheap probe first: one SELECT, no row rebuild or decryption. Only a
                    // genuinely newer shared revision triggers the paired-read reload.
                    match store.has_newer_revision().await {
                        Ok(newer) => {
                            if newer {
                                if let Err(e) = store
                                    .reload_external_with_source(false, ReloadSource::Poll)
                                    .await
                                {
                                    tracing::warn!(
                                        error = %e,
                                        "config poll reload failed; will retry next tick"
                                    );
                                }
                            }
                            // The tick completed: the probe answered and any reload it
                            // triggered was attempted, so the watcher task is alive even if
                            // the reload itself failed. A probe ERROR does NOT refresh this
                            // gauge: that is a broken liveness check, and the alert should
                            // fire rather than be papered over.
                            crate::admin::metrics::record_config_watch_ok();
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "config poll revision probe failed; will retry next tick"
                            );
                        }
                    }
                }
            }
        }
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_changed_round_trips_through_json() {
        let changed = ConfigChanged {
            version: 42,
            origin: "node-a".to_string(),
        };
        let encoded = serde_json::to_string(&changed).expect("serialize");
        assert_eq!(encoded, r#"{"version":42,"origin":"node-a"}"#);
        let decoded: ConfigChanged = serde_json::from_str(&encoded).expect("parse");
        assert_eq!(decoded, changed);
    }

    #[test]
    fn malformed_payload_is_an_error_not_a_panic() {
        assert!(serde_json::from_str::<ConfigChanged>("{not json").is_err());
        assert!(
            serde_json::from_str::<ConfigChanged>(r#"{"version":"nope","origin":"a"}"#).is_err()
        );
    }

    #[test]
    fn poll_ms_accepts_in_range_values_and_falls_back_otherwise() {
        assert_eq!(poll_ms(Some("250")), 250);
        assert_eq!(poll_ms(Some("5000")), 5000);
        assert_eq!(
            poll_ms(Some(" 250 ")),
            250,
            "surrounding whitespace is trimmed"
        );
        assert_eq!(poll_ms(None), DEFAULT_POLL_MS);
        assert_eq!(poll_ms(Some("")), DEFAULT_POLL_MS);
        assert_eq!(poll_ms(Some("nope")), DEFAULT_POLL_MS);
        assert_eq!(
            poll_ms(Some("0")),
            DEFAULT_POLL_MS,
            "zero would disable the backstop"
        );
    }

    #[test]
    fn poll_ms_clamps_out_of_range_values() {
        assert_eq!(poll_ms(Some("1")), MIN_POLL_MS, "below the minimum");
        assert_eq!(
            poll_ms(Some(&u64::MAX.to_string())),
            MAX_POLL_MS,
            "above the maximum"
        );
        assert_eq!(poll_ms(Some("50")), 50, "the minimum is inclusive");
        assert_eq!(
            poll_ms(Some("3600000")),
            3_600_000,
            "the maximum is inclusive"
        );
    }

    #[test]
    fn poll_interval_uses_the_millisecond_unit() {
        assert_eq!(poll_ms(Some("750")), 750);
        assert_eq!(DEFAULT_POLL_MS, 5_000);
        assert_eq!(MIN_POLL_MS, 50);
        assert_eq!(MAX_POLL_MS, 3_600_000);
    }
}
