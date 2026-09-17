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

/// One key PER NODE: `hydra:{ctl:inv:applied}:<node_id>` → last fully-applied
/// stream id.
///
/// A hash would need fred's private `Map` type at the call site; more to the
/// point, the waiter already holds the live-node list, so it can `MGET` exactly
/// those keys. That also keeps the read bounded by the live set rather than by
/// every node id that has ever published.
///
/// ## Why this key exists
///
/// The consumer has always tracked its read position in a LOCAL variable
/// (`last_id` inside `spawn_invalidation_consumer`) and has never published it.
/// So before this key, nothing anywhere could answer "has the fleet applied the
/// event I just published?" — a publisher only knew it had enqueued something,
/// and the HTTP answer it gave the tenant (`published: true`) said exactly that
/// and no more. A tenant that had just banned a key could not tell whether other
/// nodes were still serving it.
///
/// The watermark is what makes the difference observable: a publisher waits
/// until every LIVE node's watermark has reached its event id, and reports the
/// truth when they have not.
pub const APPLIED_KEY_PREFIX: &str = "hydra:{ctl:inv:applied}:";

/// TTL on a watermark. It is only ever read for nodes the registry calls LIVE,
/// so the TTL exists purely to bound key growth after a node is rebuilt with a
/// new identity — not to expire the answer of a live node.
const APPLIED_TTL_SECS: i64 = 24 * 60 * 60;

/// The watermark key for one node.
#[must_use]
pub fn applied_key(node_id: &str) -> String {
    format!("{APPLIED_KEY_PREFIX}{node_id}")
}

/// Compare two Redis stream ids (`<ms>-<seq>`), which are NOT comparable as
/// plain strings (`"9-1" > "10-0"` lexicographically but not chronologically).
#[must_use]
pub fn stream_id_ge(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> (u64, u64) {
        match s.split_once('-') {
            Some((ms, seq)) => (ms.parse().unwrap_or(0), seq.parse().unwrap_or(0)),
            None => (s.parse().unwrap_or(0), 0),
        }
    };
    parse(a) >= parse(b)
}

/// The outcome of waiting for the fleet to apply one event.
///
/// There is deliberately NO `SingleNode` variant: this is a method on a stream,
/// and "there is no stream" means there is no object to call it on. The call site
/// decides that case, because only it can see the `Option`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppliedOutcome {
    /// Every live node has applied at least the given event.
    Applied {
        nodes_applied: usize,
        nodes_total: usize,
    },
    /// The deadline passed with nodes still behind. The `lagging` list names
    /// them, so an operator can act instead of guessing.
    Pending {
        nodes_applied: usize,
        nodes_total: usize,
        lagging: Vec<String>,
    },
    /// The barrier itself could not run (the bus became unreachable). Never
    /// reported as "applied": a barrier that cannot check must not claim success.
    Unavailable(String),
}

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

/// The fleet-wide result of one invalidation, in the tri-state the HTTP layer
/// reports — **shared by both entry points** (the tenant API's E2 and the
/// operator's `DELETE /api/v1/auth/cache`).
///
/// It exists because the boolean it replaces lied: `published: true` was the
/// initial value and only ever became `false` when a stream existed AND the
/// publish failed, so a build with no stream reported `true` about a broadcast
/// that never happened. `state` cannot be true-by-default — it is produced by
/// the same barrier the tenant API waits on.
#[derive(Clone, Debug, serde::Serialize)]
pub struct FleetReport {
    /// `applied` | `pending` | `single_node` | `unavailable`.
    pub state: &'static str,
    pub nodes_total: usize,
    pub nodes_applied: usize,
    /// Nodes that did not confirm, named so an operator can act.
    pub lagging: Vec<String>,
    /// The stream entry this report is about, for cross-referencing the bus.
    pub event_id: Option<String>,
    /// The HTTP status that goes with this outcome. Carried HERE so the two
    /// entry points cannot map the same state to different statuses.
    #[serde(skip)]
    pub http_status: u16,
}

impl FleetReport {
    /// No stream at all: this node's own clear IS the whole answer. Only
    /// reachable for the single-node `all` role — `main` refuses to start a
    /// `leader`/`edge` without a Redis backbone, so "cluster member with no
    /// channel" cannot occur, and a stream that exists but fails is
    /// `unavailable` (503), not this.
    #[must_use]
    pub fn single_node() -> Self {
        Self {
            state: "single_node",
            nodes_total: 1,
            nodes_applied: 1,
            lagging: Vec::new(),
            event_id: None,
            http_status: 200,
        }
    }
}

/// Publish an invalidation and wait for the fleet to confirm it.
///
/// The single implementation of the three layers (design §4.2.2): L1 fan-out
/// through the stream, L2 authority already deleted synchronously by the caller,
/// L3 confirmation through the per-node applied watermarks.
///
/// Status semantics: `200` applied (or `single_node`), `202` published but not
/// confirmed (the laggards are named — the work is in flight, which is not an
/// error), `503` the channel exists but did not answer.
pub async fn broadcast_and_confirm(
    stream: Option<&InvalidationStream>,
    tenant_id: Option<String>,
    api_keys: Vec<String>,
    live_nodes: Vec<String>,
    timeout: std::time::Duration,
) -> FleetReport {
    let Some(stream) = stream else {
        return FleetReport::single_node();
    };
    let event_id = match stream.publish(tenant_id, api_keys).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "invalidation publish failed");
            return FleetReport {
                state: "unavailable",
                nodes_total: 0,
                nodes_applied: 0,
                lagging: Vec::new(),
                event_id: None,
                http_status: 503,
            };
        }
    };
    match stream.await_applied(&event_id, &live_nodes, timeout).await {
        AppliedOutcome::Applied {
            nodes_applied,
            nodes_total,
        } => FleetReport {
            state: "applied",
            nodes_total,
            nodes_applied,
            lagging: Vec::new(),
            event_id: Some(event_id),
            http_status: 200,
        },
        AppliedOutcome::Pending {
            nodes_applied,
            nodes_total,
            lagging,
        } => FleetReport {
            state: "pending",
            nodes_total,
            nodes_applied,
            lagging,
            event_id: Some(event_id),
            http_status: 202,
        },
        AppliedOutcome::Unavailable(e) => {
            tracing::warn!(error = %e, "convergence barrier could not run");
            FleetReport {
                state: "unavailable",
                nodes_total: live_nodes.len(),
                nodes_applied: 0,
                lagging: live_nodes,
                event_id: Some(event_id),
                http_status: 503,
            }
        }
    }
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
    hydra_core::auth::sha256_hex_string(s.as_bytes())
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

    /// Record that `node_id` has FULLY applied everything up to `event_id`.
    ///
    /// **Call order is load-bearing: apply first, acknowledge second.** An
    /// acknowledgement written before the clear would let the publisher tell the
    /// tenant "applied everywhere" while this node was still serving the stale
    /// verdict — the barrier would certify exactly the thing it exists to detect.
    pub async fn mark_applied(&self, node_id: &str, event_id: &str) -> Result<(), RedisError> {
        let _: Option<String> = self
            .pool
            .set(
                applied_key(node_id),
                event_id,
                Some(Expiration::EX(APPLIED_TTL_SECS)),
                None,
                false,
            )
            .await?;
        Ok(())
    }

    /// The applied watermark of each node in `nodes`, as currently published.
    ///
    /// Read with ONE `MGET`: asking node by node would turn a bounded wait into
    /// N round trips per poll.
    pub async fn applied_watermarks(
        &self,
        nodes: &[String],
    ) -> Result<std::collections::HashMap<String, String>, RedisError> {
        let mut out = std::collections::HashMap::with_capacity(nodes.len());
        if nodes.is_empty() {
            return Ok(out);
        }
        let keys: Vec<String> = nodes.iter().map(|n| applied_key(n)).collect();
        let values: Vec<Option<String>> = self.pool.mget(keys).await?;
        for (node, v) in nodes.iter().zip(values) {
            if let Some(id) = v {
                out.insert(node.clone(), id);
            }
        }
        Ok(out)
    }

    /// Wait until every node in `live_nodes` has applied `event_id`, or the
    /// deadline passes.
    ///
    /// Only LIVE nodes are awaited: a node whose heartbeat has expired is not in
    /// the registry, is not receiving traffic from the load balancer, and must not
    /// hold a convergence decision hostage. The caller passes exactly the live
    /// set (and must filter `alive == true` from the registry, which also reports
    /// dead rows).
    ///
    /// Polling rather than a pub/sub notification, deliberately: the wait is
    /// bounded (typically tens of milliseconds), the alternative adds a second
    /// channel to keep correct, and a missed notification would turn into a
    /// timeout — i.e. a false "not applied" for a fleet that did apply.
    pub async fn await_applied(
        &self,
        event_id: &str,
        live_nodes: &[String],
        timeout: std::time::Duration,
    ) -> AppliedOutcome {
        let deadline = tokio::time::Instant::now() + timeout;
        // An empty live set is a legitimate answer, not an error: nothing else
        // can be serving this tenant.
        if live_nodes.is_empty() {
            return AppliedOutcome::Applied {
                nodes_applied: 0,
                nodes_total: 0,
            };
        }
        // The last observation, so a budget that expires between polls still
        // reports WHICH nodes were behind rather than collapsing to "all of them".
        let mut last: Option<(usize, Vec<String>)> = None;
        loop {
            // EVERY poll is bounded by what is left of the budget. Without this,
            // a Redis client that keeps retrying a dead connection would hold the
            // tenant's request open indefinitely — the deadline would never be
            // reached because a single `await` never returned. A poll that cannot
            // complete in the remaining time is reported as Unavailable
            // (fail-closed), never as "applied".
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let polled = tokio::time::timeout(remaining, self.applied_watermarks(live_nodes)).await;
            let polled = match polled {
                Ok(r) => r,
                Err(_) => {
                    return AppliedOutcome::Unavailable(
                        "the watermark read did not complete within the convergence budget".into(),
                    )
                }
            };
            match polled {
                Ok(marks) => {
                    let lagging: Vec<String> = live_nodes
                        .iter()
                        .filter(|n| {
                            marks
                                .get(n.as_str())
                                .is_none_or(|id| !stream_id_ge(id, event_id))
                        })
                        .cloned()
                        .collect();
                    if lagging.is_empty() {
                        return AppliedOutcome::Applied {
                            nodes_applied: live_nodes.len(),
                            nodes_total: live_nodes.len(),
                        };
                    }
                    let applied = live_nodes.len() - lagging.len();
                    if tokio::time::Instant::now() >= deadline {
                        return AppliedOutcome::Pending {
                            nodes_applied: applied,
                            nodes_total: live_nodes.len(),
                            lagging,
                        };
                    }
                    last = Some((applied, lagging));
                }
                Err(e) => return AppliedOutcome::Unavailable(e.to_string()),
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        // Budget exhausted between polls: report the last real observation.
        match last {
            Some((nodes_applied, lagging)) => AppliedOutcome::Pending {
                nodes_applied,
                nodes_total: live_nodes.len(),
                lagging,
            },
            // Never managed a single read inside the budget.
            None => AppliedOutcome::Pending {
                nodes_applied: 0,
                nodes_total: live_nodes.len(),
                lagging: live_nodes.to_vec(),
            },
        }
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
                        // v=2: SHA-256 hex digests. An EMPTY field means "no
                        // keys" (a whole-tenant clear), never one empty digest:
                        // `"".split(',')` yields `[""]`, which made
                        // `apply_invalidation` take the per-key branch and
                        // invalidate nothing at all — silently (audit L-3).
                        "keyhashes" => {
                            keyhashes = v
                                .split(',')
                                .filter(|s| !s.is_empty())
                                .map(str::to_string)
                                .collect();
                        }
                        // v=1 legacy: plaintext keys (replayed, hashed at apply).
                        "keys" => {
                            legacy_keys = v
                                .split(',')
                                .filter(|s| !s.is_empty())
                                .map(str::to_string)
                                .collect();
                        }
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

    /// One trim pass (F-6): trim to `maxlen`, and if entries were removed, bump
    /// the generation so lagging consumers re-hydrate. Returns
    /// `(removed, bumped)`.
    ///
    /// **One atomic script** (review B3a). The trim and its compensating bump
    /// used to be two commands: `XTRIM` first, then `INCR`, so anything that
    /// failed in between (a dropped connection, a command timeout) left the
    /// entries deleted with NO compensation — and the only backstop against
    /// dropping an invalidation nobody had read is that generation bump.
    /// Redis runs a script as a single command, so the two effects cannot be
    /// separated by a network failure any more.
    ///
    /// The bump is computed from `GET`/`SET` rather than `INCR` on purpose: a
    /// corrupt counter value would make `INCR` raise *after* the trim had
    /// already been applied (a Lua error does not roll back earlier writes in
    /// the same script), which is the very loss this fix removes.
    pub async fn trim_and_maybe_bump(&self, maxlen: u64) -> Result<(i64, bool), RedisError> {
        let removed: i64 = self
            .pool
            .eval(
                TRIM_AND_MAYBE_BUMP_SCRIPT,
                vec![EVENTS_KEY, GENERATION_KEY],
                vec![maxlen.to_string()],
            )
            .await?;
        Ok((removed, removed > 0))
    }
}

/// Trim the invalidation stream and bump the generation in ONE atomic step.
///
/// `KEYS[1]` = stream, `KEYS[2]` = generation counter, `ARGV[1]` = maxlen.
/// Exact `MAXLEN` (matching the previous `XCapTrim::Exact`), and the counter is
/// only touched when something was actually removed — a spurious bump would
/// clear every node's auth cache for nothing.
pub const TRIM_AND_MAYBE_BUMP_SCRIPT: &str = r#"
local removed = redis.call('XTRIM', KEYS[1], 'MAXLEN', ARGV[1])
if removed > 0 then
  local current = tonumber(redis.call('GET', KEYS[2])) or 0
  redis.call('SET', KEYS[2], current + 1)
end
return removed
"#;

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
        inv.legacy_keys.iter().map(|k| sha256_hex_str(k)).collect()
    };
    match (&inv.tenant_id, keyhashes.is_empty()) {
        (Some(tid), true) => cache.invalidate_tenant(tid).await,
        (Some(tid), false) => cache.invalidate_hashes(tid, &keyhashes).await,
        (None, true) => {
            // Whole-cache clear (a `tenant: None` event with no keys): clear
            // L1 AND the L2 fleet-wide. Iterating `known_tenants` instead would
            // skip both the L1 entries of a tenant this node's snapshot does not
            // know and (until B2) the L2 entirely.
            cache.clear_all().await
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

/// How many stream entries one `XREAD` asks for (`XREAD COUNT`).
const INVALIDATION_READ_BATCH: u64 = 100;

/// How long the consumer idles when a read came back with nothing to do.
/// Deliberately NOT paid after a full batch — see [`spawn_invalidation_consumer`].
const INVALIDATION_IDLE_POLL: std::time::Duration = std::time::Duration::from_millis(500);

/// Spawn the per-node invalidation consumer (cluster P4): an `XREAD` loop
/// over the stream; each event is applied to the local auth cache (idempotent
/// replay — on reconnect it re-reads from the last id); a `generation` bump
/// (the stream was trimmed past our watermark) clears the local cache.
///
/// **Drains before idling** (review B3c). The loop used to read
/// [`INVALIDATION_READ_BATCH`] entries and then sleep unconditionally, i.e. a
/// hard ceiling of batch/idle = 200 events/s per node with no back-pressure:
/// above that sustained rate the stream grows to `maxlen`, every trim then drops
/// unread entries, every drop bumps the generation, and every node then clears
/// its whole auth cache — on a schedule set by the trim interval. The sleep is
/// now only paid when a read returned fewer than a full batch, i.e. when there
/// is genuinely nothing left to read.
pub fn spawn_invalidation_consumer(
    stream: InvalidationStream,
    auth: std::sync::Arc<crate::http::HttpAuthChecker>,
    store: crate::store::ConfigStore,
    node_id: String,
) {
    tokio::spawn(async move {
        let mut last_id = "0".to_string();
        let mut gen: i64 = stream.generation().await.unwrap_or(0);
        loop {
            let mut more_to_read = false;
            match stream.read_since(&last_id, INVALIDATION_READ_BATCH).await {
                Ok(events) => {
                    // A full batch means the stream may hold more: loop again
                    // immediately instead of sleeping.
                    more_to_read = events.len() as u64 == INVALIDATION_READ_BATCH;
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
                        // ACKNOWLEDGE THE WHOLE BATCH, AFTER APPLYING IT.
                        // This is the order the barrier depends on: a publisher
                        // waits for this watermark, and an acknowledgement
                        // written before the clear would let it tell a tenant
                        // "applied everywhere" while this node was still serving
                        // the stale verdict. One write per batch, not per event:
                        // the watermark is a position, not a log.
                        if let Err(e) = stream.mark_applied(&node_id, &last_id).await {
                            // The clear DID happen here; only the evidence did
                            // not land. Reported so a publisher sees this node as
                            // lagging rather than silently believing it.
                            tracing::warn!(
                                error = %e,
                                node = %node_id,
                                "invalidation applied but the watermark write failed;                                  publishers will see this node as lagging"
                            );
                        }
                    }
                    match stream.generation().await {
                        Ok(g) if g != gen => {
                            gen = g;
                            tracing::info!(
                                generation = g,
                                "invalidation generation bumped; clearing local auth cache (L1 + L2)"
                            );
                            // DELIBERATELY NO WATERMARK ADVANCE HERE. A bump means
                            // the stream was trimmed, so the events between our last
                            // read and the trim are GONE — their ids are unknowable,
                            // and a node may not account for an event it never read.
                            // The whole-cache clear below supersedes their EFFECT
                            // (everything is gone), but the barrier must keep
                            // reporting those events as unapplied rather than claim
                            // a position it cannot name. Conservative on purpose:
                            // report "not converged" rather than lie about it.
                            // L1 **and** L2 (B2): clearing only the L1 let the
                            // next `check` re-hydrate the very verdict this
                            // clear exists to drop, for the rest of its TTL.
                            auth.cache().clear_all().await;
                            crate::admin::metrics::record_auth_cache_size(0);
                        }
                        _ => {}
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "invalidation read failed; retrying");
                }
            }
            if !more_to_read {
                tokio::time::sleep(INVALIDATION_IDLE_POLL).await;
            }
        }
    });
}

/// Spawn the periodic stream trim task (F-6): keeps the invalidation stream
/// bounded to `maxlen`. When a trim removes entries, the generation is bumped
/// (via [`InvalidationStream::trim_and_maybe_bump`]) so every node re-hydrates
/// its auth cache — a removed event may not have reached a lagging consumer,
/// and a full local clear is the safe, idempotent response.
pub fn spawn_trim_task(stream: InvalidationStream, maxlen: u64, interval: std::time::Duration) {
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
    use std::time::Duration;

    /// A REAL Redis, on its own database (dev-plan 铁律 2: the invalidation bus
    /// is the one place where a double's missing semantics hid a real failure
    /// path — its `INCR` could not fail, so "trimmed but not compensated" was
    /// untestable). Fails loudly when `HYDRA_TEST_REDIS_URL` is unset.
    async fn pool() -> Pool {
        crate::redis::test_redis::isolated_pool().await
    }

    #[tokio::test]
    async fn publish_read_roundtrip() {
        let s = InvalidationStream::new(pool().await);
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
        let s = InvalidationStream::new(pool().await);
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
        let s = InvalidationStream::new(pool().await);
        let _id = s
            .publish(
                Some("t1".into()),
                vec!["sk-secret-key".into(), "sk-b".into()],
            )
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

    /// REVIEW L-3 — an empty `keyhashes` field must mean "no keys" (a
    /// whole-tenant clear), not "one empty digest". `"".split(',')` yields
    /// `[""]`, which used to take the per-key branch and invalidate NOTHING,
    /// silently.
    #[tokio::test]
    async fn an_empty_keyhashes_field_is_a_whole_tenant_clear() {
        let pool = pool().await;
        let s = InvalidationStream::new(pool.clone());
        // Hand-written XADD with an EMPTY keyhashes field (a foreign publisher,
        // or a future one): the field is present but carries no digests.
        let _: String = pool
            .xadd(
                EVENTS_KEY,
                false,
                None,
                "*",
                vec![("v", "2"), ("tenant", "t1"), ("keyhashes", "")],
            )
            .await
            .expect("xadd");

        let events = s.read_since("0", 10).await.expect("read");
        assert_eq!(events.len(), 1);
        let inv = &events[0].1;
        assert!(
            inv.keyhashes.is_empty(),
            "an empty field must parse to NO digests, got {:?}",
            inv.keyhashes
        );

        // And it clears the tenant's cached verdicts.
        let cache = AuthCache::new(Duration::from_secs(300), Duration::from_secs(30));
        cache
            .set("t1", "sk-a", true, Duration::from_secs(300))
            .await;
        assert_eq!(cache.len(), 1);
        let cleared = apply_invalidation(&cache, inv, &["t1".to_string()]).await;
        assert_eq!(cleared, 1, "a whole-tenant clear must drop the entry");
        assert_eq!(cache.len(), 0);
    }

    #[tokio::test]
    async fn comma_key_invalidated_by_hash() {
        // A key containing a comma is hashed WHOLE before the comma-join, so
        // the stream carries ONE digest (never an ambiguous split).
        let s = InvalidationStream::new(pool().await);
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
        assert_eq!(
            cache.check("t1", "a,b").await,
            hydra_core::auth::Verdict::Miss
        );
    }

    #[tokio::test]
    async fn legacy_keys_replay_equivalent_to_keyhashes() {
        let pool = pool().await;
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
        cache
            .set("t1", "sk-a", true, Duration::from_secs(300))
            .await;
        assert_eq!(
            apply_invalidation(&cache, inv_v2, &["t1".into()]).await,
            1,
            "v2 event invalidated the entry"
        );
        assert_eq!(
            cache.check("t1", "sk-a").await,
            hydra_core::auth::Verdict::Miss
        );
        // Re-seed, apply the legacy event → the SAME entry is cleared (replay).
        cache
            .set("t1", "sk-a", true, Duration::from_secs(300))
            .await;
        assert_eq!(
            apply_invalidation(&cache, inv_legacy, &["t1".into()]).await,
            1,
            "legacy event invalidated the SAME entry (stream replay)"
        );
        assert_eq!(
            cache.check("t1", "sk-a").await,
            hydra_core::auth::Verdict::Miss
        );
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
            let via_hashes = c1.invalidate_hashes("t1", &[sha256_hex_str("sk-a")]).await;
            let via_plain = c2.invalidate("t1", &["sk-a".to_string()]).await;
            assert_eq!(via_hashes, via_plain, "both remove exactly one entry");
            assert_eq!(via_plain, 1);
            assert_eq!(c1.len(), 0);
            assert_eq!(c2.len(), 0);
            assert_eq!(
                c1.check("t1", "sk-a").await,
                hydra_core::auth::Verdict::Miss
            );
            assert_eq!(
                c2.check("t1", "sk-a").await,
                hydra_core::auth::Verdict::Miss
            );

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
        let s = InvalidationStream::new(pool().await);
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

    /// REVIEW B3a — the trim and its compensating bump are ONE atomic step.
    ///
    /// They used to be two commands (`XTRIM`, then `INCR`), so anything failing
    /// in between left the entries deleted with no compensation — and that bump
    /// is the only backstop against dropping an invalidation no consumer had
    /// read yet. Fault injection needs no mock here: a real Redis makes `INCR`
    /// fail on a non-integer counter value, and a Lua error does NOT roll back
    /// the writes the script already performed.
    #[tokio::test]
    async fn trim_and_bump_are_one_atomic_step() {
        let pool = pool().await;
        let s = InvalidationStream::new(pool.clone());
        for i in 0..5 {
            s.publish(Some("t1".into()), vec![format!("k{i}")])
                .await
                .expect("publish");
        }

        // A corrupt counter: `INCR` raises on it, and (in the old two-command
        // form) that error arrived AFTER the trim had already been applied.
        let _: () = pool
            .set(GENERATION_KEY, "not-a-number", None, None, false)
            .await
            .expect("seed corrupt counter");

        let (removed, bumped) = s
            .trim_and_maybe_bump(2)
            .await
            .expect("trim + bump must be one step, not a partial failure");
        assert_eq!(removed, 3, "three entries were dropped");
        assert!(
            bumped,
            "dropping entries MUST bump, whatever the counter held"
        );
        assert_eq!(
            s.generation().await.expect("gen"),
            1,
            "the bump is applied on top of the unparseable value (treated as 0)"
        );

        // A trim that removes nothing must NOT bump: a spurious bump clears
        // every node's auth cache for nothing.
        let (removed, bumped) = s.trim_and_maybe_bump(2).await.expect("second pass");
        assert_eq!(removed, 0);
        assert!(!bumped, "nothing dropped ⇒ no bump");
        assert_eq!(s.generation().await.expect("gen"), 1, "unchanged");
    }

    /// REVIEW B3c — the consumer must DRAIN a backlog instead of reading one
    /// batch per idle interval.
    ///
    /// It read 100 entries and then slept 500 ms unconditionally: a hard
    /// ceiling of ~200 events/s. Above that the stream grows to `maxlen`, every
    /// trim drops unread entries, every drop bumps the generation, and every
    /// node clears its whole auth cache on the trim schedule. Measured before
    /// the fix: 250 events took 1.006 s (three batches); with the drain loop it
    /// is a few milliseconds.
    #[tokio::test]
    async fn consumer_drains_a_backlog_without_idling() {
        let pool = pool().await;
        let stream = InvalidationStream::new(pool.clone());
        let auth = std::sync::Arc::new(
            crate::http::HttpAuthChecker::new(
                AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
                crate::http::AuthConfig::default(),
            )
            .expect("checker"),
        );
        let store = crate::store::ConfigStore::from_snapshot(
            hydra_core::config::ConfigData::default(),
            std::sync::Arc::new(crate::crypto::StaticKeyProvider::new([1u8; 32], 1)),
        );

        // 250 keys cached, then 250 single-key invalidations to apply.
        const N: usize = 250;
        for i in 0..N {
            auth.cache()
                .set("t1", &format!("sk-{i}"), true, Duration::from_secs(300))
                .await;
        }
        assert_eq!(auth.cache().len(), N, "seeded");
        for i in 0..N {
            stream
                .publish(Some("t1".to_string()), vec![format!("sk-{i}")])
                .await
                .expect("publish");
        }

        spawn_invalidation_consumer(stream.clone(), auth.clone(), store, "test-node".to_string());
        let started = std::time::Instant::now();
        let mut drained = false;
        for _ in 0..400 {
            if auth.cache().is_empty() {
                drained = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let elapsed = started.elapsed();
        assert!(
            drained,
            "the consumer did not apply all {N} events within 2s"
        );
        assert!(
            elapsed < Duration::from_millis(400),
            "draining {N} events took {elapsed:?} — the consumer is idling per batch again \
             (one batch per 500 ms would be ~1 s, which is what the fix removes)"
        );
    }

    #[tokio::test]
    async fn trim_bump_clears_consumer_cache() {
        // End-to-end: publish past maxlen → the trim task removes entries →
        // bumps the generation → the consumer observes the bump and clears its
        // local cache.
        let pool = pool().await;
        let stream = InvalidationStream::new(pool.clone());

        let auth = std::sync::Arc::new(
            crate::http::HttpAuthChecker::new(
                AuthCache::new(Duration::from_secs(300), Duration::from_secs(30)),
                crate::http::AuthConfig::default(),
            )
            .expect("checker"),
        );
        auth.cache()
            .set("t1", "sk-a", true, Duration::from_secs(300))
            .await;
        assert_eq!(auth.cache().len(), 1, "seeded verdict before trim");

        // An empty store is fine: the published events target ANOTHER tenant, so
        // they cannot clear the seeded `t1` verdict — only the generation bump
        // can. (A `(None, [])` event is itself a whole-cache clear and would
        // clear it directly; that path is covered by
        // `apply_invalidation_to_local_cache`.)
        let store = crate::store::ConfigStore::from_snapshot(
            hydra_core::config::ConfigData::default(),
            std::sync::Arc::new(crate::crypto::StaticKeyProvider::new([1u8; 32], 1)),
        );

        spawn_invalidation_consumer(stream.clone(), auth.clone(), store, "test-node".to_string());
        spawn_trim_task(stream.clone(), 2, Duration::from_millis(20));

        // Publish past maxlen (2) → the trim task removes 3 → bumps.
        for i in 0..5 {
            stream
                .publish(Some("t9".to_string()), vec![format!("sk-{i}")])
                .await
                .expect("publish");
        }

        // Wait for the consumer to observe the bump and clear the cache.
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if auth.cache().is_empty() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("consumer did not clear the cache within 3s (bump not observed)");
        assert_eq!(
            auth.cache().len(),
            0,
            "consumer cleared the local cache on bump"
        );
        assert_eq!(
            stream.generation().await.expect("gen"),
            1,
            "generation bumped exactly once"
        );
    }
}
