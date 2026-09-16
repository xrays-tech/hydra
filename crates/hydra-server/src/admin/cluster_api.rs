//! Cluster / control-plane admin API — whole-fleet status, the internal control
//! channel, leader health and the reload endpoint.
//!
//! Moved out of `handlers.rs` verbatim (plan T0.2, Phase 0) — no logic change.
//! Only the module imports are new: the moved DTOs derive `Serialize`, and the
//! response helpers are `pub(super)` in the sibling `handlers` module.
//!
//! Keeping these here (rather than in `handlers.rs`) is what makes the plan's
//! T4/T6 edits land in a file whose whole subject is the cluster API.

use serde::Serialize;

use super::handlers::{err_json, ok_json, Resp};
use super::AdminState;
use crate::cluster::snapshot::SnapshotWire;

// ===========================================================================
// Cluster status (cluster P4) — whole-fleet view for the Admin UI Health page
// ===========================================================================

/// One fleet node as rendered by `GET /api/v1/cluster/status`.
#[derive(Serialize)]
struct ClusterNodeDto {
    node_id: String,
    role: String,
    control_url: String,
    alive: bool,
    is_lease_holder: bool,
    is_self: bool,
}

/// Whole-cluster status. `cluster=false` on the single-node build / default
/// mode — the UI then renders just the local Health panel.
#[derive(Serialize)]
struct ClusterStatusDto {
    cluster: bool,
    mode: String,
    node_id: String,
    this_node_leader: bool,
    lease_holder: Option<String>,
    nodes: Vec<ClusterNodeDto>,
}

/// `GET /api/v1/cluster/status` (admin-token gated): fleet nodes from the
/// registry (role + control URL + heartbeat liveness) plus the current
/// leader-lease holder. Single-node mode reports `cluster=false`.
#[allow(unused_variables)] // params are unused on the single-node build
pub(super) async fn cluster_status(state: &AdminState, trace_id: &str) -> Resp {
    #[cfg(feature = "cluster-redis")]
    {
        let Some(registry) = &state.cluster_registry else {
            return ok_json(200, &single_node_status());
        };
        let (nodes, holder) = match (registry.list_nodes().await, registry.lease_holder().await) {
            (Ok(nodes), Ok(holder)) => (nodes, holder),
            _ => {
                return err_json(
                    502,
                    "cluster_unavailable",
                    "cannot read the cluster registry (Redis unreachable?)",
                    trace_id,
                );
            }
        };
        let self_id = registry.node_id().to_string();
        let mode = match registry.role() {
            crate::cluster::NodeRole::Leader => "leader",
            crate::cluster::NodeRole::Edge => "edge",
            crate::cluster::NodeRole::All => "all",
        }
        .to_string();
        let dto = ClusterStatusDto {
            cluster: true,
            mode,
            this_node_leader: holder.as_deref() == Some(registry.node_id()),
            node_id: self_id.clone(),
            lease_holder: holder.clone(),
            nodes: nodes
                .into_iter()
                .map(|n| ClusterNodeDto {
                    is_lease_holder: holder.as_deref() == Some(n.node_id.as_str()),
                    is_self: n.node_id == self_id,
                    node_id: n.node_id,
                    role: n.role,
                    control_url: n.control_url,
                    alive: n.alive,
                })
                .collect(),
        };
        ok_json(200, &dto)
    }
    #[cfg(not(feature = "cluster-redis"))]
    {
        ok_json(200, &single_node_status())
    }
}

/// The single-node status payload (shared by both cfg branches).
fn single_node_status() -> ClusterStatusDto {
    ClusterStatusDto {
        cluster: false,
        mode: "single".to_string(),
        node_id: String::new(),
        this_node_leader: false,
        lease_holder: None,
        nodes: Vec::new(),
    }
}

/// Response of `POST /api/v1/reload`.
///
/// EXTENDED by plan T6 (O9) — `changed` / `version` are added, nothing is
/// removed: `status` and the counters are a documented contract consumed by the
/// Admin UI (`admin-ui/app.js`) and asserted in `tests/admin_api.rs`.
#[derive(Serialize)]
struct ReloadBody {
    status: &'static str,
    /// Whether this reload advanced the generation (false ⇒ no-op, replicas
    /// were not rebuilt). `?force=1` makes this true by construction.
    changed: bool,
    /// The version AFTER the reload (derived from the replication content, so it
    /// is the same value a replica would receive on its next poll).
    version: u64,
    tenants: usize,
    providers: usize,
    models: usize,
    keys: usize,
    certs: usize,
}

// ===========================================================================
// Internal control plane (cluster P1) — snapshot distribution
// ===========================================================================

/// Control-channel response: `snapshot` is present only when the caller's
/// `since` is older than the current version.
#[derive(Serialize)]
struct InternalControlResponse {
    version: u64,
    snapshot: Option<SnapshotWire>,
}

/// `GET /api/v1/internal/control?since=N` (cluster-token gated): serve the
/// current config snapshot (secrets sealed, versioned) to edge/standby
/// nodes. `snapshot` is `null` when the caller is already current.
pub(super) async fn internal_control(
    state: &AdminState,
    query: Option<&str>,
    trace_id: &str,
) -> Resp {
    let since: u64 = query
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("since=")))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // ONE atomic read gives both (version, content): the cheap `since >= current`
    // path and the snapshot we hand out must describe the SAME revision, and
    // `state.db()` is no longer needed here (the content carries the fidelity
    // rows). Reading `version()` first and `replication()` second would be two
    // reads and could pair new content with an old version.
    let content = state.store.replication();
    let Some(content) = content.as_deref() else {
        return err_json(
            503,
            "not_ready",
            "this node has no replication content yet",
            trace_id,
        );
    };
    let current = content.version;

    if since >= current {
        return ok_json(
            200,
            &InternalControlResponse {
                version: current,
                snapshot: None,
            },
        );
    }

    // Only a node that HOLDS THE LEASE may speak for the cluster. A non-leader
    // (or one whose lease just lapsed while its heartbeat is still fresh) must
    // not hand out a snapshot: edges would follow a stale producer and rebuild
    // their replica from it.
    //
    // `is_leader_candidate()` is the role/eligibility gate (`!edge_mode`): an
    // edge gets the pre-existing 404 "edge node: no admin API" from
    // `AdminService::response` BEFORE dispatch, so this branch is defence in
    // depth — it exists so a future role that keeps the admin API without being
    // a leader candidate cannot serve snapshots by omission.
    if !state.is_leader_candidate() {
        return err_json(
            404,
            "not_found",
            "the control snapshot endpoint is only served by leader-candidate nodes",
            trace_id,
        );
    }
    // `leader_ready` is `Some` only on leader candidates. This is the gate that
    // did not exist at all before T4.
    if let Some(is_leader) = state.leader_ready.as_ref() {
        if !is_leader() {
            return err_json(
                503,
                "not_leader",
                "this node does not hold the leader lease; retry against the active leader",
                trace_id,
            );
        }
    }

    match SnapshotWire::build(content, state.key_provider.as_ref()).await {
        Ok(snapshot) => ok_json(
            200,
            &InternalControlResponse {
                version: current,
                snapshot: Some(snapshot),
            },
        ),
        Err(e) => err_json(
            500,
            "snapshot_build_failed",
            &format!("control snapshot build failed: {e}"),
            trace_id,
        ),
    }
}

/// `GET /healthz/leader` (cluster P2): 200 while this node holds the leader
/// lease, 503 on standby, 404 on non-candidate nodes (`all` / edge).
pub(super) fn leader_health(state: &AdminState, trace_id: &str) -> Resp {
    match &state.leader_ready {
        Some(f) if f() => ok_json(200, &LeaderHealth { leader: true }),
        Some(_) => err_json(
            503,
            "not_leader",
            "this node is not the active leader",
            trace_id,
        ),
        None => err_json(
            404,
            "not_found",
            "leader health is only available on leader-candidate nodes",
            trace_id,
        ),
    }
}

#[derive(Serialize)]
struct LeaderHealth {
    leader: bool,
}

/// `POST /api/v1/reload?force=1`.
///
/// Additive contract (plan T6, O9): the response body gains `changed` and
/// `version` while every previously documented field is kept — `status`
/// (asserted as `"reloaded"` in `tests/admin_api.rs`), plus the `providers` /
/// `tenants` / `models` / `keys` / `certs` counters the Admin UI renders. The
/// error stays **400 `reload_failed`** (documented in `admin-ui/api-docs.js`).
///
/// `force` (parsed by the router — see `admin::mod`) preserves the pre-T6
/// behaviour of advancing the generation unconditionally. Without it a reload
/// whose replicated content is unchanged is a NO-OP, so replicas are not
/// rebuilt for nothing.
pub(super) async fn reload(state: &AdminState, force: bool, trace_id: &str) -> Resp {
    // Explicit reload shares the same best-effort path (reload_all only), but a
    // fatal validation failure is reported as 400 (design §5.3: the old snapshot
    // is retained). Certs follow the swap via the `ConfigStore` hook.
    // The reload lock serializes concurrent reloads; it is kept from the pre-T6
    // handler — without it two reloads can interleave a load with a publish.
    let result = {
        let _guard = state.reload_lock.lock().await;
        state.store.reload_all_with(force).await
    };
    let changed = match result {
        Ok(changed) => changed,
        Err(e) => {
            return err_json(
                400,
                "reload_failed",
                &format!("config reload failed (old snapshot retained): {e}"),
                trace_id,
            )
        }
    };
    let snap = state.store.snapshot();
    ok_json(
        200,
        &ReloadBody {
            status: "reloaded",
            changed,
            version: state.store.version(),
            tenants: snap.tenants_by_domain.len(),
            providers: snap.providers.len(),
            models: snap.models_by_key.len(),
            keys: snap.provider_keys.len(),
            certs: snap.certs.len(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{KeyProvider, StaticKeyProvider};
    use hydra_core::config::ConfigData;
    use std::sync::Arc;

    /// Build an `AdminState` for the direct-call tests below.
    ///
    /// `internal_control` is `pub(super)`, so these live in the module rather than
    /// in `tests/` (an integration test cannot see it) — which is exactly why the
    /// non-candidate branch had NEVER been executed before: over HTTP the router
    /// answers 404 for an edge before dispatch ever happens.
    async fn state(edge_mode: bool, is_leader: Option<bool>) -> Arc<AdminState> {
        let pool = crate::db::init_pool("sqlite::memory:")
            .await
            .expect("init_pool");
        crate::db::run_migrate(&pool).await.expect("migrate");
        let kp: Arc<dyn KeyProvider> = Arc::new(StaticKeyProvider::new([1u8; 32], 1));
        let store = crate::store::ConfigStore::load(pool.clone(), kp.clone())
            .await
            .expect("ConfigStore::load");
        // Seed ONE provider and reload: a fresh store has version 0, and with
        // `since=0 >= current=0` the CHEAP path would answer 200 before any gate
        // is reached — the assertions below would then be vacuous.
        crate::db::insert_provider(
            &pool,
            &hydra_core::model::Provider {
                id: "p1".into(),
                key: "k1".into(),
                name: "P".into(),
                endpoint: "http://127.0.0.1:1/".into(),
                weight: 1,
                created_at: "2026-01-01 00:00:00".into(),
                updated_at: "2026-01-01 00:00:00".into(),
                max_concurrency: None,
                max_queue_depth: None,
                queue_wait_timeout_ms: None,
            },
        )
        .await
        .expect("insert provider");
        store.reload_all().await.expect("reload");
        assert!(store.version() > 0, "fixture: the store must have content");
        let auth = Arc::new(
            crate::http::HttpAuthChecker::new(
                crate::http::AuthCache::new(
                    std::time::Duration::from_secs(300),
                    std::time::Duration::from_secs(30),
                ),
                crate::http::AuthConfig::default(),
            )
            .expect("HttpAuthChecker"),
        );
        let breaker = Arc::new(crate::proxy::breaker_wrap::CircuitBreaker::new(
            hydra_core::breaker::BreakerConfig::new(2),
        ));
        let leader_ready: Option<Arc<dyn Fn() -> bool + Send + Sync>> =
            is_leader.map(|v| Arc::new(move || v) as Arc<dyn Fn() -> bool + Send + Sync>);
        Arc::new(AdminState::new(
            Some(pool),
            store,
            auth,
            breaker,
            kp,
            Some("admin-token-16chars".to_string()),
            crate::proxy::admission::AdmissionControl::new(),
            edge_mode,
            Some("cluster-token".to_string()),
            leader_ready,
        ))
    }

    fn body_json(resp: &Resp) -> serde_json::Value {
        serde_json::from_slice(resp.body()).expect("error body is JSON")
    }

    /// (T4, defence in depth) A NON-CANDIDATE must be refused by the handler
    /// itself, not only by the router.
    ///
    /// Over HTTP an edge is 404'd before dispatch (`AdminService::response`), so
    /// this branch exists for a future role that keeps the admin API without being
    /// a leader candidate. Calling the handler directly is the only way to execute
    /// it — and it is what makes the 404 in `tests/cluster.rs` verifiably
    /// "pre-existing router behaviour" rather than a claim.
    #[tokio::test]
    async fn a_non_candidate_is_refused_by_the_handler_itself() {
        // edge_mode ⇒ is_leader_candidate() == false, *and* it claims to hold the
        // lease: the role gate must still win.
        let state = state(true, Some(true)).await;
        let resp = internal_control(&state, Some("since=0"), "t").await;
        assert_eq!(
            resp.status().as_u16(),
            404,
            "a non-candidate serves nothing"
        );
        let body = body_json(&resp);
        assert_eq!(body["error"]["code"], "not_found", "got {body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("leader-candidate")),
            "the reason must name the eligibility rule: {body}"
        );
    }

    /// A candidate WITHOUT the lease is the 503 case (the T4 fix), asserted here
    /// directly so the mapping is pinned even if the HTTP-level test is refactored.
    #[tokio::test]
    async fn a_candidate_without_the_lease_is_503_not_leader() {
        let state = state(false, Some(false)).await;
        let resp = internal_control(&state, Some("since=0"), "t").await;
        assert_eq!(resp.status().as_u16(), 503);
        let body = body_json(&resp);
        assert_eq!(body["error"]["code"], "not_leader", "got {body}");
        assert!(
            body.get("snapshot").is_none(),
            "no payload in an error: {body}"
        );
    }

    /// ...while the CHEAP path (`since >= current`) stays ungated even for a
    /// non-holder: gating it would turn every follower's quiet poll into an error
    /// storm whenever the fleet has no leader.
    #[tokio::test]
    async fn the_cheap_path_is_not_gated_by_the_lease() {
        let state = state(false, Some(false)).await;
        // Current version is whatever the store was built with; asking with a
        // `since` at least that high takes the cheap path.
        let current = state.store.version();
        let resp = internal_control(&state, Some(&format!("since={current}")), "t").await;
        assert_eq!(resp.status().as_u16(), 200, "cheap path stays 200");
        let body = body_json(&resp);
        assert_eq!(body["snapshot"], serde_json::Value::Null, "got {body}");
    }

    /// A node with NO replication content yet answers 503 `not_ready` — the guard
    /// that keeps an empty-fidelity snapshot from ever being published.
    #[tokio::test]
    async fn a_node_without_content_is_not_ready() {
        let state = state(false, Some(true)).await;
        // An edge-shaped store has no content; simulate it directly.
        let empty = Arc::new(AdminState::new(
            None,
            crate::store::ConfigStore::from_snapshot(ConfigData::default(), {
                let kp: Arc<dyn KeyProvider> = Arc::new(StaticKeyProvider::new([1u8; 32], 1));
                kp
            }),
            state.auth.clone(),
            state.breaker.clone(),
            state.key_provider.clone(),
            Some("admin-token-16chars".to_string()),
            crate::proxy::admission::AdmissionControl::new(),
            false,
            Some("cluster-token".to_string()),
            Some(Arc::new(|| true) as Arc<dyn Fn() -> bool + Send + Sync>),
        ));
        let resp = internal_control(&empty, Some("since=0"), "t").await;
        assert_eq!(resp.status().as_u16(), 503);
        assert_eq!(body_json(&resp)["error"]["code"], "not_ready");
    }
}
