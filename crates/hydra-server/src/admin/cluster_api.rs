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
            // Record it through the SAME owner as the post-write path: the snapshot IS stale after
            // this, and `ops.md` §9.1 tells operators to alert on `hydra_config_snapshot_stale`.
            // This endpoint used to report the 400 to the caller and leave the gauge at 0.
            super::handlers::note_reload_outcome(state, true);
            return err_json(
                400,
                "reload_failed",
                &format!("config reload failed (old snapshot retained): {e}"),
                trace_id,
            );
        }
    };
    // ...and clear it on success: without this, a gauge set by an earlier failing WRITE kept the
    // documented alert firing after the documented recovery (a successful reload).
    super::handlers::note_reload_outcome(state, false);
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
