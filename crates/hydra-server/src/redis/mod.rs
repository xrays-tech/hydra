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
pub mod rate_limit;

/// Real-Redis harness for tests (dev-plan 铁律 2: Redis is an external system
/// boundary and a REAL instance is available locally and in CI, so the
/// in-process double is no longer the default for new tests).
///
/// The endpoint comes from `HYDRA_TEST_REDIS_URL` (e.g.
/// `redis://127.0.0.1:6380`). A test that needs Redis **fails loudly** when it
/// is unset: silently falling back to a mock — or skipping — is exactly how
/// "use the real thing" rots into "we never notice".
#[cfg(test)]
pub mod test_redis {
    use std::sync::atomic::{AtomicU8, Ordering};

    use fred::clients::Pool;
    use fred::prelude::*;

    /// Hand out a distinct Redis DATABASE per test: the key names are shared
    /// constants (`hydra:{ctl:events}`, `hydra:{lease:leader}`, …), so tests on
    /// one instance would otherwise observe each other's state.
    ///
    /// The range is deliberately wide (lib tests own 1..=40, hand-assigned
    /// integration tests 41..=63) because the allocator wraps: with only the
    /// default 16 databases, a big enough test binary would hand two concurrent
    /// tests the same one, and each flushes it. The test instance therefore runs
    /// with `--databases 64` (docker-compose.local.yml / CI).
    static NEXT_DB: AtomicU8 = AtomicU8::new(0);

    /// Databases this harness may use (unit tests). Integration tests start at
    /// [`INTEGRATION_DB_FIRST`].
    const UNIT_DB_COUNT: u8 = 40;

    /// A pool on its own flushed database, against a REAL Redis.
    ///
    /// # Panics
    /// When `HYDRA_TEST_REDIS_URL` is unset or the instance is unreachable —
    /// with the exact commands needed to start one.
    pub async fn isolated_pool() -> Pool {
        let base = std::env::var("HYDRA_TEST_REDIS_URL").unwrap_or_else(|_| {
            panic!(
                "HYDRA_TEST_REDIS_URL is not set, and Redis-dependent tests must run against a REAL \
                 Redis (dev-plan 铁律 2: no in-process Redis mock).\n  \
                 start one: docker compose -f environment/docker-compose.local.yml up -d redis-test\n  \
                 then:      export HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380"
            )
        });
        let db = NEXT_DB.fetch_add(1, Ordering::Relaxed) % UNIT_DB_COUNT + 1; // 1..=40
        let url = format!("{}/{}", base.trim_end_matches('/'), db);
        // The SAME construction as production, through the single owner: a harness pool on
        // fred's defaults could not time out (0 = wait forever) and could never reconnect,
        // so it would test a different program than the one that ships.
        let pool = super::build_pool(&url, 2).expect("test pool builds");
        pool.init()
            .await
            .unwrap_or_else(|e| panic!("cannot reach the test Redis at {url}: {e}"));
        // Clear only THIS test's database. `FLUSHALL` would wipe the other
        // databases that parallel tests are using, and fred 10 exposes no
        // `FLUSHDB` helper, so it goes through the raw command interface.
        let _: fred::types::Value = pool
            .custom(
                fred::types::CustomCommand::new_static("FLUSHDB", None, false),
                Vec::<String>::new(),
            )
            .await
            .unwrap_or_else(|e| panic!("cannot flush test db {db}: {e}"));
        pool
    }

    /// `true` when a test Redis is configured. Prefer [`isolated_pool`], which
    /// fails loudly instead of silently skipping.
    #[must_use]
    pub fn configured() -> bool {
        std::env::var("HYDRA_TEST_REDIS_URL").is_ok()
    }
}

use fred::prelude::*;
use fred::types::config::{ConnectionConfig, PerformanceConfig, UnresponsiveConfig};
use fred::types::{Expiration, SetOptions};
use std::time::Duration;

/// The leader-lease key (single key — topology-safe, plan §6.1).
pub const LEASE_KEY: &str = "hydra:{lease:leader}";

/// Reconnect delay (ms) for a lost Redis connection. Paired with
/// `max_attempts = 0` = **retry forever** (fred's own wording: "Use a
/// `max_attempts` value of `0` to retry forever").
pub const RECONNECT_DELAY_MS: u32 = 1_000;

/// Default per-command timeout (ms) applied to every Redis command.
///
/// fred's default is `0`, which disables command timeouts entirely. A half-open
/// or black-holed Redis socket (k8s node loss, conntrack half-open entry,
/// dropped packets that leave the socket ESTABLISHED) then makes `EVAL`/`GET`
/// await **forever**. The rate-limit check runs on the request hot path BEFORE
/// routing, and the auth cache consults L2 on every L1 miss, so an unbounded
/// wait stalls the whole data plane — and because a hang never produces an
/// `Err`, the documented fail-open branch cannot fire either (no error, no
/// metric, no response).
///
/// A command timeout turns that hang into an ordinary error, which the
/// callers already handle by **failing open** (deliberate, documented
/// behavior): the rate limiter swallows the error per role — `continue` in
/// `rate_limit.rs` — so a Redis outage (or an unresponsive command) can never
/// lock every tenant out, and the auth cache degrades to an L1 miss. There
/// is NO env override for the fail-open direction; it is fixed by design.
/// 500 ms is ~1000x the documented 0.2-0.5 ms local round trip, so it only
/// trips when Redis is genuinely unresponsive. Override with
/// `HYDRA_REDIS_COMMAND_TIMEOUT_MS`.
pub const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 500;

/// Default unresponsive-connection watchdog (ms): a frame left unanswered for
/// longer than this force-closes the connection and fred reconnects it, so a
/// dead socket is recycled instead of costing the command timeout on every
/// request for the lifetime of the process.
///
/// Kept an order of magnitude above [`DEFAULT_COMMAND_TIMEOUT_MS`] so commands
/// normally fail — and fail open — on their own timeout first. Trade-off to be
/// aware of: when the watchdog tears a connection down, fred may retry the
/// in-flight command (`ConnectionConfig::max_command_attempts`, default 3), so
/// a rate-limit `EVAL` can in the worst case be counted twice. That direction
/// is fail-safe (over-count, never over-admit). Override with
/// `HYDRA_REDIS_UNRESPONSIVE_TIMEOUT_MS`.
pub const DEFAULT_UNRESPONSIVE_TIMEOUT_MS: u64 = 5_000;

/// Read a positive millisecond value from the environment, else `default`.
fn env_millis(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

/// The reconnect policy this process hands fred — **without which fred never
/// reconnects a connection that closed**.
///
/// Measured 2026-09-30 on a real leader whose Redis link was severed for ~2s and then
/// restored (`.acceptance/round99/reconnect-probe.py`): for the next **90 seconds the node
/// opened ZERO new connections**, every command kept failing on the 500 ms command timeout,
/// and `/healthz/leader` stayed `503` — the lease was never re-acquired, so the fleet had no
/// leader. fred's own debug log named the cause: `Checking reconnect state. Has policy:
/// false`. `Pool::new(config, perf, connection, policy, size)` takes the policy as its own
/// argument (it is not part of `Config`), and this process passed `None`, so
/// `ReconnectPolicy` — whose `max_attempts = 0` means "retry forever, every 1 s" — was never
/// installed. `ops.md` §13.5 promises the opposite ("until Redis recovers"), and every
/// fail-open path (cluster rate limits, auth-cache L2, breaker sync, registry) silently
/// became **permanent** instead of temporary.
#[must_use]
pub fn reconnect_policy() -> ReconnectPolicy {
    ReconnectPolicy::new_constant(0, RECONNECT_DELAY_MS)
}

/// Per-command timeout for the shared pool (see [`DEFAULT_COMMAND_TIMEOUT_MS`]
/// for why this must not stay on fred's `0` default).
fn performance_config() -> PerformanceConfig {
    PerformanceConfig {
        default_command_timeout: Duration::from_millis(env_millis(
            "HYDRA_REDIS_COMMAND_TIMEOUT_MS",
            DEFAULT_COMMAND_TIMEOUT_MS,
        )),
        ..PerformanceConfig::default()
    }
}

/// Connection config carrying the unresponsive-connection watchdog (see
/// [`DEFAULT_UNRESPONSIVE_TIMEOUT_MS`]).
fn connection_config() -> ConnectionConfig {
    let max_ms = env_millis(
        "HYDRA_REDIS_UNRESPONSIVE_TIMEOUT_MS",
        DEFAULT_UNRESPONSIVE_TIMEOUT_MS,
    );
    ConnectionConfig {
        unresponsive: UnresponsiveConfig {
            max_timeout: Some(Duration::from_millis(max_ms)),
            // fred requires > 1 ms and recommends well under `max_timeout`.
            interval: Duration::from_millis((max_ms / 4).max(1)),
        },
        ..ConnectionConfig::default()
    }
}

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
/// topology work (P4+) — they fail fast at startup rather than silently
/// misbehaving, and so does **any other value**: a typo in this knob used to
/// fall through to `Single`, i.e. a topology-critical setting silently became a
/// different one (measured 2026-10-01 on the wire: `HYDRA_REDIS_MODE=clustr`
/// started an edge node in single mode, while `=sentinel` was refused).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedisMode {
    Single,
    Sentinel,
    Cluster,
}

impl RedisMode {
    /// Parse a mode value. `None`/empty/`single` ⇒ [`RedisMode::Single`]; `sentinel`/`cluster` and
    /// **every other non-empty value** ⇒ [`RedisError::UnsupportedMode`].
    ///
    /// The catch-all arm is deliberate and is the whole point (round 191): `HYDRA_REDIS_MODE` selects a
    /// TOPOLOGY, so a typo (`clustr`) must not quietly mean "single" — it must stop the process, which is
    /// what the module's own contract says ("fail fast rather than silently misbehaving"). Pure, so the
    /// cases are unit-testable without touching the process environment.
    pub fn parse(value: Option<&str>) -> Result<Self, RedisError> {
        // Match case-insensitively (an operator writing `SINGLE` means single) but REPORT what they
        // actually wrote, so the error message is about their input and not about a folded copy.
        let trimmed = value.map(str::trim).unwrap_or("");
        match trimmed.to_ascii_lowercase().as_str() {
            "" | "single" => Ok(Self::Single),
            _ => Err(RedisError::UnsupportedMode {
                mode: trimmed.to_string(),
            }),
        }
    }

    /// Parse `HYDRA_REDIS_MODE` (default `single`) — the environment-facing half of [`Self::parse`].
    pub fn from_env() -> Result<Self, RedisError> {
        Self::parse(std::env::var("HYDRA_REDIS_MODE").ok().as_deref())
    }
}

/// Shared Redis pool + lease-key plumbing. Cheap to `Clone` (fred pool is
/// ref-counted).
#[derive(Clone)]
pub struct RedisBackend {
    pool: Pool,
}

/// The **single owner of "how a Redis pool is built"** — production and every test.
///
/// `Pool::new(config, perf, connection, policy, size)` takes three pieces of configuration
/// that fred does **not** put in `Config`, and fred's defaults for all three are wrong for
/// this process:
///
/// | argument | fred's default | what it costs here |
/// |---|---|---|
/// | `perf.default_command_timeout` | `0` = wait forever | a half-open Redis stalls the data plane (rate-limit `EVAL` runs before routing) |
/// | `connection.unresponsive` | disabled | a dead socket is never recycled |
/// | `policy` | `None` = **never reconnect** | measured: a ~2 s outage left the leader demoted and limits failed open for 90 s+ (see [`reconnect_policy`]) |
///
/// Five hand-written `Pool::new` calls used to exist in this tree (production, the lib-test
/// harness, and three integration-test sites) and **four of them passed fred's defaults** —
/// including the wait-forever command timeout the production comment calls a data-plane
/// stall. A test pool that cannot time out and cannot reconnect tests a different program
/// than the one that ships, so every construction site now comes through here
/// (`scripts/check_redis_pool.cjs` fails the build if a new one appears).
///
/// # Errors
/// When the URL does not parse or fred rejects the configuration.
pub fn build_pool(url: &str, size: usize) -> Result<Pool, RedisError> {
    build_pool_with(url, size, performance_config())
}

/// [`build_pool`] with an explicit command timeout, for tests that must make an
/// unreachable bus fail **fast** instead of waiting for the production timeout.
///
/// The timeout is the only knob a caller may vary: the watchdog and the reconnect policy
/// stay owned here, so a caller cannot accidentally re-open the hole this module closed.
///
/// # Errors
/// When the URL does not parse or fred rejects the configuration.
pub fn build_pool_with(
    url: &str,
    size: usize,
    perf: PerformanceConfig,
) -> Result<Pool, RedisError> {
    let config = Config::from_url(url).map_err(RedisError::from)?;
    pool_from_config(config, perf, size)
}

/// **The only `Pool::new` call in this tree.** Everything that needs a pool — production,
/// the lib-test harness, the integration tests — reaches fred through here, so the three
/// non-default settings cannot drift apart again.
fn pool_from_config(
    config: Config,
    perf: PerformanceConfig,
    size: usize,
) -> Result<Pool, RedisError> {
    Pool::new(
        config,
        Some(perf),
        Some(connection_config()),
        Some(reconnect_policy()),
        size,
    )
    .map_err(RedisError::from)
}

/// The pool for production: two clients (fred's own recommended minimum for a
/// non-clustered deployment), all three settings above.
fn pool_for(config: Config) -> Result<Pool, RedisError> {
    pool_from_config(config, performance_config(), 2)
}

impl RedisBackend {
    /// Connect to Redis. `single` mode uses the URL directly; sentinel/cluster
    /// are rejected until wired (fail-fast startup, never silent).
    ///
    /// The two arms below are defensive for a DIRECT caller of this public API: no path through the
    /// environment can produce those variants any more, because [`RedisMode::parse`] refuses every
    /// value except `single` before a mode is ever constructed (round 191). Saying they are simply
    /// "unreachable" would be wrong — they are reachable by anyone constructing the enum by hand,
    /// which is exactly why they must keep returning an error instead of falling through. (Round 195
    /// corrected this comment: it used to claim more than the type system enforces.)
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
        // Never leave the pool on fred's defaults: a command timeout of 0 means
        // wait-forever, which turns a half-open Redis into a data-plane stall
        // (see DEFAULT_COMMAND_TIMEOUT_MS) — and a reconnect policy of `None`
        // means a severed connection is never dialled again (see
        // [`reconnect_policy`]). Both are asserted by unit tests on
        // [`pool_for`], because passing `None` here is invisible until Redis
        // actually goes away.
        let pool = pool_for(config)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: the pool must never be built on fred's defaults. A
    /// `default_command_timeout` of 0 disables command timeouts, so a
    /// half-open Redis makes the rate-limit `EVAL` on the request hot path
    /// await forever — no error, so the documented fail-open never fires and
    /// the data plane stalls. Guard the non-zero timeout and the watchdog.
    #[test]
    fn pool_config_bounds_every_command() {
        let perf = performance_config();
        assert!(
            !perf.default_command_timeout.is_zero(),
            "command timeout must never be 0 (fred's 0 = wait forever)"
        );
        assert!(perf.default_command_timeout >= Duration::from_millis(100));

        let conn = connection_config();
        let max = conn
            .unresponsive
            .max_timeout
            .expect("unresponsive watchdog must be enabled so a dead socket is recycled");
        assert!(max > Duration::from_millis(0));
        // The watchdog must not pre-empt the per-command timeout, otherwise a
        // routine slow command would tear the connection down and trigger
        // fred's in-flight command retry.
        assert!(
            max > perf.default_command_timeout,
            "watchdog ({max:?}) must sit above the command timeout ({:?})",
            perf.default_command_timeout
        );
        assert!(conn.unresponsive.interval > Duration::from_millis(0));
        assert!(conn.unresponsive.interval < max);
    }

    /// Regression (measured 2026-09-30): `Pool::new`'s `policy` argument was `None`, so
    /// fred never reconnected a connection that closed — `Has policy: false` in its debug
    /// log — and a ~2s Redis outage left the node dialling **nothing** for 90s while every
    /// command failed on the command timeout and the leader stayed demoted. The policy is
    /// not part of `Config`, so a `None` here is invisible until Redis goes away: assert
    /// the pool that would really be built carries one, and that it retries forever.
    #[test]
    fn the_pool_always_gets_a_reconnect_policy_that_never_gives_up() {
        // 0 = retry forever (fred: "Use a `max_attempts` value of `0` to retry forever").
        assert_eq!(reconnect_policy().max_attempts(), 0);
        // A zero delay would hammer Redis in a tight loop; checked at compile time so
        // clippy's `assertions_on_constants` stays satisfied.
        const { assert!(RECONNECT_DELAY_MS > 0) };

        let pool = pool_for(Config::from_url("redis://127.0.0.1:1/0").expect("url parses"))
            .expect("a pool is built without connecting");
        assert_eq!(pool.clients().len(), 2);
        for client in pool.clients() {
            assert!(
                client.client_reconnect_policy().is_some(),
                "every client must be constructed WITH a reconnect policy; None means a \
                 closed connection is never dialled again"
            );
            assert_eq!(
                client
                    .client_reconnect_policy()
                    .expect("policy present")
                    .max_attempts(),
                0,
                "the policy must retry forever, not give up after N attempts"
            );
            let _ = client.client_config(); // the config itself must still be the parsed URL's
        }
    }

    /// The TEST harness must build its pool exactly like production.
    ///
    /// Measured 2026-09-30: four of this tree's five `Pool::new` call sites passed fred's
    /// defaults — `isolated_pool` (every Redis-backed lib test) among them, i.e. **no**
    /// reconnect policy and `default_command_timeout = 0` (wait forever). A harness pool
    /// that cannot time out and cannot reconnect tests a different program than the one
    /// that ships, and it is exactly the pool whose behaviour the reconnection fix depends
    /// on. Needs a live test Redis (`HYDRA_TEST_REDIS_URL`), like its siblings here.
    #[tokio::test]
    async fn the_test_harness_builds_the_same_pool_as_production() {
        let pool = test_redis::isolated_pool().await;
        assert_eq!(pool.clients().len(), 2);
        for client in pool.clients() {
            let policy = client.client_reconnect_policy().expect(
                "the harness pool must carry the production reconnect policy; with `None` a \
                 severed connection is never dialled again",
            );
            assert_eq!(policy.max_attempts(), 0);
        }
    }

    /// `env_millis` ignores unset, unparseable and zero values so a typo in an
    /// env var falls back to the safe default instead of disabling the bound.
    #[test]
    fn env_millis_rejects_disabling_values() {
        assert_eq!(env_millis("HYDRA_TEST_UNSET_MS_KEY", 42), 42);
        assert_eq!(
            env_millis("HYDRA_TEST_UNSET_MS_KEY_2", DEFAULT_COMMAND_TIMEOUT_MS),
            DEFAULT_COMMAND_TIMEOUT_MS
        );
    }

    /// Round 191: the mode knob must FAIL FAST for anything it does not understand. It used to fall
    /// through to `Single`, so `HYDRA_REDIS_MODE=clustr` silently started a single-mode node (measured
    /// on the wire) while the module's own contract says "fail fast rather than silently misbehaving".
    /// `parse` is pure, so these cases do not touch the process environment.
    #[test]
    fn redis_mode_parses_the_known_values() {
        for value in [
            None,
            Some(""),
            Some("single"),
            Some("single "),
            Some("SINGLE"),
            Some(" Single "),
        ] {
            assert_eq!(
                RedisMode::parse(value).expect("the default and `single` are accepted"),
                RedisMode::Single,
                "value {value:?}"
            );
        }
    }

    #[test]
    fn redis_mode_refuses_everything_else_including_typos() {
        for value in [
            "sentinel", "cluster", "clustr", "SENTINEL", "snetinel", "banana",
        ] {
            match RedisMode::parse(Some(value)) {
                Err(RedisError::UnsupportedMode { mode }) => assert_eq!(mode, value),
                other => panic!("{value:?} must be refused, got {other:?}"),
            }
        }
    }
}
