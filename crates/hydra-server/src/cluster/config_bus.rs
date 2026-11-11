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

use std::sync::Arc;
use std::time::Duration;

use fred::clients::SubscriberClient;
use fred::prelude::*;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

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

/// Backoff after the broadcast receiver reports `Closed`.
///
/// A closed receiver has no further senders, so retrying immediately would
/// hot-spin a worker while the poll fallback still has to run (and still
/// refreshes liveness only on a completed tick). Bounded and short: the watcher
/// stays responsive if the client resubscribes.
const CLOSED_RECV_BACKOFF: Duration = Duration::from_millis(500);

/// Quiet window for coalescing external (pub/sub) reloads.
///
/// A burst of notifications arriving within this window produces ONE reload
/// against the latest shared revision instead of one full `ConfigData` rebuild
/// per message (`O(N × rows)`). The window is far below the poll interval and
/// only delays the fast path; the poll fallback remains the correctness bound.
pub const RELOAD_DEBOUNCE_MS: u64 = 50;

/// Hard cap on how long a continuous stream may defer a pending reload.
///
/// A trailing debounce alone can be starved by a writer that never goes quiet,
/// so the deadline is the minimum of "quiet until now + debounce" and "first
/// notification + cap". A continuous stream therefore converges at roughly one
/// rebuild per (cap + reload duration), rather than unboundedly.
pub const RELOAD_MAX_DELAY_MS: u64 = 250;

/// Trailing-debounce state for the external reload path.
///
/// Pure (all decisions take an explicit `now`) so the edge cases are
/// unit-testable without sleeping:
///   * the first note arms a [`RELOAD_DEBOUNCE_MS`] window from that instant;
///   * every later note while pending extends the quiet window, but the deadline
///     is capped at [`RELOAD_MAX_DELAY_MS`] after the FIRST note;
///   * taking a due deadline clears both fields, so a message that arrives while
///     the reload is in flight re-arms from scratch.
#[derive(Debug, Default)]
struct ReloadCoalescer {
    pending_since: Option<Instant>,
    pending_until: Option<Instant>,
}

impl ReloadCoalescer {
    /// Note one qualifying notification at `now`.
    ///
    /// Returns `true` when a reload was already pending (i.e. this message was
    /// coalesced into it), which is what the metric counts. The returned bool is
    /// not the reload decision: the caller always lets the deadline fire once.
    fn note(&mut self, now: Instant) -> bool {
        let already_pending = self.pending_since.is_some();
        let since = *self.pending_since.get_or_insert(now);
        let quiet_until = now + Duration::from_millis(RELOAD_DEBOUNCE_MS);
        let hard_cap = since + Duration::from_millis(RELOAD_MAX_DELAY_MS);
        self.pending_until = Some(quiet_until.min(hard_cap));
        already_pending
    }

    /// The deadline the watcher should sleep until, if a reload is pending.
    fn deadline(&self) -> Option<Instant> {
        self.pending_until
    }

    /// Clear the pending state and return the deadline that fired.
    ///
    /// MUST be called BEFORE the reload starts: while the reload is in flight the
    /// select cannot read messages, and clearing first means a notification that
    /// arrived concurrently re-arms instead of being swallowed by the pending
    /// flag. A failed reload leaves this cleared on purpose; the poll fallback is
    /// the retry path (and no flag can get stuck).
    fn take_pending(&mut self) -> Option<Instant> {
        self.pending_since = None;
        self.pending_until.take()
    }

    #[cfg(test)]
    fn is_pending(&self) -> bool {
        self.pending_since.is_some()
    }
}

/// The two failure modes of the broadcast receiver, classified separately so
/// the loop cannot lump a recoverable lag together with a dead channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecvFault {
    /// `n` messages were dropped from the ring buffer before we read them.
    Lagged(u64),
    /// No senders remain; the channel will never yield another message.
    Closed,
}

fn classify_recv_error(error: &tokio::sync::broadcast::error::RecvError) -> RecvFault {
    match error {
        tokio::sync::broadcast::error::RecvError::Lagged(n) => RecvFault::Lagged(*n),
        tokio::sync::broadcast::error::RecvError::Closed => RecvFault::Closed,
    }
}

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
/// loop and the poller run as TWO independently supervised tasks: a panic in one
/// is logged, metered (`hydra_config_watch_restarts_total`) and revived after a
/// fixed backoff, and never stops the other. A later disconnect is handled by
/// [`SubscriberClient::manage_subscriptions`] and does not stop the poller.
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
    // is detached on purpose: dropping this JoinHandle does not stop it, and the
    // subscriber itself is kept alive by the supervisor factory below. Exactly
    // one manager per client, ever: a restart must NOT re-spawn it.
    drop(subscriber.manage_subscriptions());
    subscriber.subscribe(channel.as_str()).await?;
    let rx = subscriber.message_rx();
    // Start the liveness clock at watcher start (not at 0): the alert is
    // `time() - gauge > 3× poll interval`, and leaving the gauge unset would
    // make a healthy node fire it for the whole first poll interval on boot.
    crate::admin::metrics::record_config_watch_ok();

    // The already-subscribed receiver is handed to the FIRST factory
    // invocation; every restart re-runs `init`/`subscribe` for a NEW receiver
    // instead of reusing a dead one. The slot is taken once, before any await.
    let first_rx = Arc::new(std::sync::Mutex::new(Some(rx)));
    let pubsub_factory = {
        let subscriber = subscriber.clone();
        let store = store.clone();
        let channel = channel.clone();
        let first_rx = Arc::clone(&first_rx);
        move || {
            let subscriber = subscriber.clone();
            let store = store.clone();
            let channel = channel.clone();
            let first_rx = Arc::clone(&first_rx);
            async move {
                let pending = crate::lock_gate(&first_rx).take();
                let rx = match pending {
                    Some(rx) => rx,
                    None => {
                        // Restart path: re-run `init`/`subscribe` and take a NEW
                        // receiver. `manage_subscriptions` is deliberately NOT
                        // re-spawned (one manager per client, ever).
                        //
                        // The old connection may still be subscribed during this
                        // re-subscribe, so a message can arrive twice. That is
                        // HARMLESS: the store's revision gate ignores an
                        // already-applied revision, the coalescer folds a burst
                        // into one reload, and the poll backstop re-checks anyway.
                        // Do not "fix" this into unsubscribing first — a dropped
                        // subscription window would lose messages instead.
                        if let Err(e) = subscriber.init().await {
                            tracing::warn!(
                                error = %e,
                                "config-bus subscriber re-init failed; supervisor will retry"
                            );
                            return;
                        }
                        if let Err(e) = subscriber.subscribe(channel.as_str()).await {
                            tracing::warn!(
                                error = %e,
                                "config-bus subscriber re-subscribe failed; supervisor will retry"
                            );
                            return;
                        }
                        subscriber.message_rx()
                    }
                };
                run_pubsub(subscriber, store, channel, rx).await;
            }
        }
    };

    // Two INDEPENDENT supervised tasks. A panic in either is caught by
    // `supervise`, metered and revived; one dying must not stop the other.
    tokio::spawn(supervise(PUBSUB_TASK, pubsub_factory));
    let poll_factory = move || {
        let store = store.clone();
        async move { run_poll(store, poll).await }
    };
    tokio::spawn(supervise(POLL_TASK, poll_factory));

    Ok(())
}

/// One run of the pub/sub consumer.
///
/// Returning (or panicking) hands control back to [`supervise`], which decides
/// whether/how to revive it. `_subscriber` is held for the whole run so the
/// subscribed connection is not dropped while messages can still arrive.
async fn run_pubsub(
    _subscriber: SubscriberClient,
    store: ConfigStore,
    channel: String,
    mut rx: tokio::sync::broadcast::Receiver<fred::types::Message>,
) {
    let mut coalescer = ReloadCoalescer::default();

    loop {
        // Recomputed every iteration: a message branch may arm or extend the
        // deadline, and the timer future below must see the NEW value.
        let deadline = coalescer.deadline();
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
                        // avoids even arming the timer for an already-applied or stale
                        // message.
                        if changed.version > store.version() {
                            // Do NOT reload per message: a burst of writes would
                            // rebuild the whole snapshot per notification. Arm or
                            // extend the trailing debounce; the timer branch below
                            // performs ONE reload against the latest revision.
                            if coalescer.note(Instant::now()) {
                                crate::admin::metrics::record_config_reload_coalesced();
                            }
                        }
                        // Liveness: a WELL-FORMED message was handled, whether or not it
                        // advanced the snapshot (a reload that failed still proves the
                        // watcher task is alive). Malformed/non-string payloads do not
                        // refresh this: a stream of garbage must not mask a wedged
                        // watcher in the alert `time() - gauge > 3× poll interval`.
                        crate::admin::metrics::record_config_watch_ok();
                    }
                    Err(e) => match classify_recv_error(&e) {
                        RecvFault::Lagged(missed) => {
                            // The receiver fell behind the broadcast ring buffer, so the
                            // message that advanced the shared revision may be GONE. Do
                            // not wait for the next poll tick: probe the watermark and
                            // apply the missing revision now. This stays quiet (PubSub),
                            // so a lagged consumer cannot relay another node's change.
                            crate::admin::metrics::record_config_bus_lagged(missed);
                            tracing::warn!(
                                channel = %channel,
                                missed,
                                "config-bus receiver lagged; reconciling immediately"
                            );
                            match store.has_newer_revision().await {
                                Ok(true) => {
                                    if let Err(e) = store
                                        .reload_external(false, ReloadSource::PubSub)
                                        .await
                                    {
                                        tracing::warn!(
                                            error = %e,
                                            "config-bus lag recovery reload failed; poll fallback will retry"
                                        );
                                    }
                                }
                                Ok(false) => {
                                    // Nothing newer survived the lag: the lost messages were
                                    // already-applied revisions.
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        "config-bus lag recovery probe failed; poll fallback will retry"
                                    );
                                }
                            }
                            // Deliberately do NOT refresh the liveness gauge here: a
                            // lagging subscriber is not proof that it handled the change.
                        }
                        RecvFault::Closed => {
                            // No senders remain. A closed receiver is not fatal (the poll
                            // loop below still converges), but looping without a pause
                            // would hot-spin when no messages can ever arrive. Keep the
                            // liveness gauge untouched so the dead subscription remains
                            // alertable instead of looking healthy.
                            tracing::warn!(
                                channel = %channel,
                                error = %e,
                                backoff_ms = CLOSED_RECV_BACKOFF.as_millis(),
                                "config-bus message stream closed; backing off"
                            );
                            tokio::time::sleep(CLOSED_RECV_BACKOFF).await;
                        }
                    },
                }
            }
            // Trailing-debounce fire. Guarded on a pending deadline: with no
            // pending reload this future is `pending()` and can never win.
            _ = async {
                match deadline {
                    Some(until) => tokio::time::sleep_until(until).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                // CLEAR BEFORE RELOADING. The reload below awaits the store
                // lock and rebuild; a notification that arrives during it is
                // still in the broadcast buffer and must re-arm this timer on
                // the next loop iteration. Taking the state first also means
                // a failed reload cannot leave a stuck pending flag.
                if coalescer.take_pending().is_some() {
                    // One cheap probe, then at most ONE rebuild. The probe may
                    // be false if a poll/lag path already applied the change.
                    match store.has_newer_revision().await {
                        Ok(true) => {
                            if let Err(e) = store
                                .reload_external(false, ReloadSource::PubSub)
                                .await
                            {
                                // Choice: rely on the poll backstop rather than
                                // re-arming. The pending state is already clear,
                                // so there is no stuck timer; the next poll (or
                                // a later notification) retries.
                                tracing::warn!(
                                    error = %e,
                                    "coalesced config-bus reload failed; poll fallback will retry"
                                );
                            }
                        }
                        Ok(false) => {
                            // Nothing newer survived: the burst was applied
                            // by the poll/lag path, or the messages were for
                            // revisions this node already served.
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "coalesced config-bus revision probe failed; poll fallback will retry"
                            );
                        }
                    }
                }
            }
        }
    }
}

/// One run of the poll backstop (the documented correctness bound).
///
/// The first tick is consumed so the first probe happens one interval after
/// startup, not during it, and missed ticks are not replayed in a burst. A
/// panicking `store` hook leaves this task to [`supervise`]; the pub/sub task is
/// unaffected.
async fn run_poll(store: ConfigStore, poll: Duration) {
    let mut ticker = tokio::time::interval(poll);
    // After a long stall, do not fire a burst of catch-up ticks.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick completes immediately; consume it so the first probe
    // happens one interval after startup, not during it.
    ticker.tick().await;

    loop {
        ticker.tick().await;
        // Cheap probe first: one SELECT, no row rebuild or decryption. Only a
        // genuinely newer shared revision triggers the paired-read reload.
        match store.has_newer_revision().await {
            Ok(newer) => {
                if newer {
                    if let Err(e) = store.reload_external(false, ReloadSource::Poll).await {
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

/// `hydra_config_watch_restarts_total{task}` label values.
const PUBSUB_TASK: &str = "pubsub";
const POLL_TASK: &str = "poll";

/// Fixed backoff between a supervised task's death and its restart.
///
/// Fixed (not exponential) by design: a permanently panicking task must not
/// hot-spin, and 1 s keeps the watcher responsive without a retry storm. Retrying
/// as fast as the panic occurs would burn a core and drown the log.
const SUPERVISE_RESTART_BACKOFF: Duration = Duration::from_secs(1);

/// Keep one named task alive across panics (and unexpected clean exits).
///
/// `factory` is re-invoked for every run, so a restart can re-establish its
/// resource (for the pub/sub consumer: a NEW broadcast receiver). The dead handle
/// is **awaited before the next spawn**, which is exactly what guarantees at most
/// one live consumer owns a receiver at a time.
///
/// A panic in the spawned future does not unwind this task: it surfaces as a
/// `JoinError` with `is_panic()`. It is logged, counted, and followed by a fixed
/// [`SUPERVISE_RESTART_BACKOFF`] before the factory runs again. Cancellation
/// (runtime shutdown or an explicit abort) ends the loop without restarting.
async fn supervise<F, Fut>(name: &'static str, factory: F)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    loop {
        // Sequential on purpose: the previous handle is fully awaited (joined)
        // before a new factory run starts, so two consumers never coexist.
        let handle = tokio::spawn(factory());
        match handle.await {
            Ok(()) => {
                tracing::warn!(
                    task = name,
                    backoff_ms = SUPERVISE_RESTART_BACKOFF.as_millis(),
                    "config-watch task ended unexpectedly; restarting"
                );
                crate::admin::metrics::record_config_watch_restart(name);
            }
            Err(error) if error.is_cancelled() => {
                tracing::debug!(task = name, "config-watch task cancelled; not restarting");
                break;
            }
            Err(error) => {
                // `is_panic()` is the case this supervisor exists for; any other
                // JoinError is treated the same (log + bounded restart).
                tracing::error!(
                    task = name,
                    error = %error,
                    backoff_ms = SUPERVISE_RESTART_BACKOFF.as_millis(),
                    "config-watch task panicked; restarting"
                );
                crate::admin::metrics::record_config_watch_restart(name);
            }
        }
        tokio::time::sleep(SUPERVISE_RESTART_BACKOFF).await;
    }
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
    fn receiver_faults_are_classified_separately() {
        use tokio::sync::broadcast::error::RecvError;

        assert_eq!(
            classify_recv_error(&RecvError::Lagged(7)),
            RecvFault::Lagged(7),
            "a lag carries the skipped-message count"
        );
        assert_eq!(
            classify_recv_error(&RecvError::Closed),
            RecvFault::Closed,
            "a closed channel must not be treated as a recoverable lag"
        );
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

    #[test]
    fn coalescer_first_note_sets_the_debounce_deadline() {
        let t0 = Instant::now();
        let mut c = ReloadCoalescer::default();
        assert_eq!(c.deadline(), None, "no notification means no deadline");

        assert!(
            !c.note(t0),
            "the first notification starts the window, it is not a coalesced one"
        );
        assert_eq!(
            c.deadline(),
            Some(t0 + Duration::from_millis(RELOAD_DEBOUNCE_MS)),
            "the first note arms a 50 ms quiet window"
        );
    }

    #[test]
    fn coalescer_extends_quiet_window_but_caps_at_max_delay() {
        let t0 = Instant::now();
        let mut c = ReloadCoalescer::default();
        assert!(!c.note(t0));

        // A note 30 ms later extends the quiet window to t0 + 80 ms and is
        // counted as coalesced.
        assert!(c.note(t0 + Duration::from_millis(30)));
        assert_eq!(
            c.deadline(),
            Some(t0 + Duration::from_millis(80)),
            "inside the window the debounce restarts from the new note"
        );

        // A long continuous stream cannot push the deadline past the hard cap.
        assert!(c.note(t0 + Duration::from_millis(200)));
        assert_eq!(
            c.deadline(),
            Some(t0 + Duration::from_millis(250)),
            "the deadline is the first note's cap, not this note's quiet window"
        );
        assert!(
            c.note(t0 + Duration::from_millis(240)),
            "still pending, still coalescing"
        );
        assert_eq!(
            c.deadline(),
            Some(t0 + Duration::from_millis(250)),
            "every later note is clamped to the first note's cap"
        );
    }

    #[test]
    fn coalescer_take_clears_state_and_a_later_note_rearms() {
        let t0 = Instant::now();
        let mut c = ReloadCoalescer::default();
        c.note(t0);
        assert!(c.is_pending());

        assert_eq!(
            c.take_pending(),
            Some(t0 + Duration::from_millis(RELOAD_DEBOUNCE_MS)),
            "the due deadline is taken"
        );
        assert!(!c.is_pending(), "taking a deadline clears the pending flag");
        assert_eq!(c.deadline(), None, "a cleared coalescer has no deadline");
        assert_eq!(c.take_pending(), None, "taking twice is harmless");

        // The reload is now "in flight"; a notification that arrives after the
        // take must re-arm from scratch (not be swallowed by stale state).
        let t1 = t0 + Duration::from_millis(100);
        assert!(
            !c.note(t1),
            "the re-arm starts a fresh burst, not a coalesced one"
        );
        assert!(c.is_pending());
        assert_eq!(
            c.deadline(),
            Some(t1 + Duration::from_millis(RELOAD_DEBOUNCE_MS)),
            "the new window is anchored at the re-arm instant"
        );
    }

    /// The supervisor must survive a panicking run, meter it, and revive the task
    /// with a fresh factory invocation.
    ///
    /// `deadline-poll`: a single fixed sleep is either flaky under load or wastes
    /// wall-clock; the loop yields/sleeps until the effect is observed or the
    /// deadline passes. No Redis is involved.
    #[tokio::test]
    async fn supervise_restarts_a_panicked_task_and_revives_it() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let runs = Arc::new(AtomicUsize::new(0));
        let revived = Arc::new(AtomicUsize::new(0));
        let factory = {
            let runs = Arc::clone(&runs);
            let revived = Arc::clone(&revived);
            move || {
                let run = runs.fetch_add(1, Ordering::SeqCst) + 1;
                let revived = Arc::clone(&revived);
                async move {
                    if run == 1 {
                        // Run 1 dies; `supervise` must catch the JoinError.
                        panic!("supervise test: first run panics");
                    }
                    // Run 2 is the revived task: record its effect, then model a
                    // long-running watcher so the supervisor does not respawn it
                    // again inside the assertion window.
                    revived.fetch_add(1, Ordering::SeqCst);
                    std::future::pending::<()>().await;
                }
            }
        };

        // A task label unique to this test keeps the counter assertion
        // deterministic despite the process-global registry.
        let task = "test_supervise_restart";
        let counter_before = crate::admin::metrics::config_watch_restarts_total(task);
        let supervisor = tokio::spawn(supervise(task, factory));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let restarted =
                crate::admin::metrics::config_watch_restarts_total(task) > counter_before;
            let observed = revived.load(Ordering::SeqCst) >= 1;
            if restarted && observed {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "supervise did not restart+revive before the deadline \
                 (restarted={restarted}, observed={observed})"
            );
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        supervisor.abort();
        let _ = supervisor.await;
    }
}
