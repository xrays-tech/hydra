//! Router — candidate resolution (pure).
//!
//! This module re-exports the shared routing types ([`Candidate`], [`RouteError`],
//! [`ResolveOutcome`], [`ExcludedCandidate`], [`ExclusionReason`]) and implements
//! the pure [`resolve_detailed`] function, plus [`resolve`] — the same pipeline
//! reduced to the candidate set, for callers that do not need the attribution.
//! [`resolve_with_principal`] and [`accessible_models_with_principal`] are the
//! principal-aware twins: identical gates, but step 2 may take its candidate
//! base from the Global Role rules ([`crate::role`]) instead of `tenant_providers`.
//!
//! ## Contract
//! `resolve(&ConfigData, &dyn BreakerView, &Tenant, model_key) ->
//! Result<Vec<Candidate>, RouteError>`
//!
//! ## Purity / time-injection
//! Pure and deterministic: no I/O, no global state, no time. Inputs fully
//! describe the output.
//!
//! ## Pipeline (design §7.1)
//! 0. **TenantModel gate** — *default-open*: a tenant with **no** `tenant_models`
//!    mapping is unrestricted (every model allowed). Once any mapping exists it
//!    becomes a whitelist: `model_key` outside it ⇒
//!    [`RouteError::ModelNotAllowed`].
//! 1. **Online model providers** — providers serving `model_key` (the loader
//!    only indexes `status == 1` rows into `models_by_key`); empty ⇒
//!    [`RouteError::ModelNotFound`].
//! 2. **Candidate selection** — Global Role union when a principal matches, else
//!    the legacy tenant-authorised provider set ([`crate::role::match_global_roles`]
//!    + [`crate::role::union_role_providers`]):
//!    - matched rules with a NON-empty provider union ⇒ the union IS the candidate
//!      base. If ANY matched rule pins `tenant_id == "*"`, the union is additionally
//!      intersected with `tenant_providers[tenant]` (fail-closed — a missing entry
//!      ⇒ [`RouteError::TenantForbidden`, same as the legacy gate; decision D-3).
//!      Concrete-tenant rules do NOT consult `tenant_providers` (the rule itself
//!      already pins the tenant).
//!    - no matched rule (or an empty union, which [`crate::role`] documents as
//!      "no Global Role candidate") ⇒ legacy path: the tenant's authorised provider
//!      set (fail-closed: absent ⇒ [`RouteError::TenantForbidden`]).
//!      A `None` principal matches only the `*` identity dimensions (see [`crate::role`]),
//!      so [`resolve_detailed`] (principal-less legacy callers) keeps legacy behaviour
//!      unless the snapshot contains rules whose identity dims are all `*`.
//! 3. **Intersection with (1)**; empty ⇒ [`RouteError::NoAvailableProvider`].
//!    - (The raw-key gates this pipeline once ran — the operator key-prefix
//!      binding (3.5) and the sub-tenant route gate (3.6) — are retired: the
//!      routing interface is Global Role (step 2) plus the tenant's own grants.
//!      A client api-key therefore never narrows the candidate set.)
//! 4. **Filter** — drop dead (`breaker.is_dead`), keyless (no api-keys), and
//!    soft-disabled (`weight == 0` = a deliberate action, `weight < 0` = a
//!    configuration error). Every drop is reported in
//!    [`ResolveOutcome::excluded`] with its reason.
//!    - [`resolve_detailed`]: an empty set is **`Ok`** with the attribution
//!      attached — returning `Err` here would throw away exactly the case where
//!      naming the losers matters most.
//!    - [`resolve`]: an empty set ⇒ [`RouteError::NoAvailableProvider`] (the
//!      historical contract, preserved for every other caller).
//! 5. **Order** — the returned candidates are sorted by `provider_id` for a
//!    deterministic set. SWRR ordering is a *subsequent* step applied by the
//!    caller, which owns the per-`(tenant, model)` [`SwrrState`] (T2.11: only
//!    the set is finalised here).
//!
//! ## Catalog — resolve semantics as an enumeration (design §2.1)
//! [`accessible_models`] is the **infallible, all-set twin** of [`resolve`]:
//! it walks every `models_by_key` row (loader-guaranteed `status == 1`) and
//! emits one [`CatalogEntry`] per model that keeps ≥ 1 routable provider,
//! mirroring the same gates (tenant_models whitelist → serving providers ∩
//! tenant_providers → dead / weight / api-key filter).
//! [`accessible_models_with_principal`] mirrors [`resolve_with_principal`]: the
//! same walk, but per model the candidate base comes from the Global Role union
//! when the principal matches a rule with a non-empty provider set, and from the
//! legacy `tenant_providers` intersection otherwise.
//! Catalog semantics: it **never fails** — an unknown tenant, a tenant without
//! a `tenant_providers` entry, or a model whose providers are all filtered
//! contributes nothing (empty `Vec`, where per-request `resolve` would 403).
//! Serving rows referencing a provider absent from `cfg.providers` (orphans)
//! are silently dropped via the same existence guard `resolve` uses. Output
//! is sorted by `model`, then `provider_id` (deterministic).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::breaker::BreakerView;
use crate::config::ConfigData;
use crate::identity::ApiPrincipal;
use crate::model::Tenant;

pub use crate::model::{Candidate, ExcludedCandidate, ExclusionReason, ResolveOutcome, RouteError};

/// [`resolve_detailed`], candidates only.
///
/// Kept because most callers (and most tests) only care about the candidate set;
/// the attribution is what the proxy records, and it has a second field. One
/// implementation, two views — not two code paths.
pub fn resolve(
    cfg: &ConfigData,
    breaker: &dyn BreakerView,
    tenant: &Tenant,
    model_key: &str,
) -> Result<Vec<Candidate>, RouteError> {
    let outcome = resolve_detailed(cfg, breaker, tenant, model_key)?;
    if outcome.candidates.is_empty() {
        // Every candidate was dropped in step 4 — the same failure this function
        // has always reported (and what `route_error_status` maps to 503).
        return Err(RouteError::NoAvailableProvider);
    }
    Ok(outcome.candidates)
}

/// Resolve the candidate set for one `(tenant, model_key)` request, principal-less
/// (legacy §7.1 entry point).
///
/// See the module docs for the full pipeline. The returned `Vec` is sorted by
/// `provider_id` (deterministic regardless of `HashSet` iteration order); the
/// caller then applies [`crate::swrr::order`] with its own per-`(tenant, model)`
/// state to pick the first attempt and order failover.
///
/// `client_api_key` feeds the §7.1b key-prefix binding gate (3.5) and, only
/// when no operator binding matches, the sub-tenant route gate (3.6,
/// design-sub-tenant.md §4.1); `None` (or no match for either) leaves the
/// candidate set unrestricted.
///
/// Equivalent to [`resolve_with_principal`] with `principal = None`: no request
/// identity is presented, so [`crate::role::match_global_roles`] can only accept
/// rules whose group/user/api-key dimensions are all `*`. Snapshots without
/// Global Role rows therefore keep byte-identical legacy behaviour.
pub fn resolve_detailed(
    cfg: &ConfigData,
    breaker: &dyn BreakerView,
    tenant: &Tenant,
    model_key: &str,
) -> Result<ResolveOutcome, RouteError> {
    resolve_impl(cfg, breaker, tenant, model_key, None)
}

/// [`resolve_detailed`]'s pipeline with the request's api-key PRINCIPAL instead
/// of the raw key, so step 2 can consult the Global Role rules (plan §3.4).
///
/// Same gates 0 → 1 → 2 → 3 → 4 → 5, and the same `ExcludedCandidate`
/// attribution. There is deliberately **no** `client_api_key` parameter here:
/// the §7.1b operator binding (3.5) and the sub-tenant prefix route (3.6) are
/// raw-key predicates, so this entry point never applies them — a caller that
/// has both the raw key and the principal runs [`resolve_detailed`] and gets
/// the principal-aware step 2 once the shell passes the principal.
///
/// Step 2 (candidate selection) — [`crate::role::match_global_roles`] for
/// `(tenant.id, principal, model_key)`:
/// - non-empty union ⇒ the union of the matched rules' `provider_ids` is the
///   candidate base; when ANY matched rule pins `tenant_id == "*"`, that union
///   is additionally intersected with `cfg.tenant_providers[tenant.id]` (fail
///   closed — a tenant with no provider entry ⇒ [`RouteError::TenantForbidden`],
///   decision D-3);
/// - no match (or an empty union, which [`crate::role`] documents as "no Global
///   Role candidate") ⇒ the legacy intersection with `tenant_providers`
///   (fail-closed: missing entry ⇒ [`RouteError::TenantForbidden`]).
///
/// `principal = None` matches only the `*` identity dimensions (see
/// [`crate::role`]).
pub fn resolve_with_principal(
    cfg: &ConfigData,
    breaker: &dyn BreakerView,
    tenant: &Tenant,
    model_key: &str,
    principal: Option<&ApiPrincipal>,
) -> Result<ResolveOutcome, RouteError> {
    resolve_impl(cfg, breaker, tenant, model_key, principal)
}

/// Wildcard literal on a Global Role dimension (decision D-2; `crate::role`
/// owns the matcher and keeps its own private copy of this constant).
const WILDCARD: &str = "*";

/// The ONE resolve pipeline, seen through both entry points: `principal` drives
/// the Global Role gate (step 2) and `None` selects the legacy candidate base.
/// Both wrappers share this body, so neither can fork into a second
/// implementation.
fn resolve_impl(
    cfg: &ConfigData,
    breaker: &dyn BreakerView,
    tenant: &Tenant,
    model_key: &str,
    principal: Option<&ApiPrincipal>,
) -> Result<ResolveOutcome, RouteError> {
    // (0) TenantModel access gate (design §7.1, revised — default-open): a
    // tenant with NO `tenant_models` mapping is unrestricted (all models
    // allowed); once any mapping exists it is a whitelist — a model outside
    // it is ModelNotAllowed.
    if let Some(allowed) = cfg.tenant_models.get(&tenant.id) {
        if !allowed.contains(model_key) {
            return Err(RouteError::ModelNotAllowed);
        }
    }

    // (1) Providers serving this model (online only — the loader guarantees
    //     `models_by_key` holds `status == 1` rows).
    let by_model: HashSet<String> = cfg
        .models_by_key
        .get(model_key)
        .map(|v| v.iter().map(|m| m.provider_id.clone()).collect())
        .unwrap_or_default();
    if by_model.is_empty() {
        return Err(RouteError::ModelNotFound);
    }

    // (2) Candidate base: the Global Role union when the principal matches a
    // rule with providers, otherwise the legacy tenant-authorised set.
    let roles = crate::role::match_global_roles(cfg, &tenant.id, principal, model_key);
    let role_union = crate::role::union_role_providers(&roles);
    let mut intersection: Vec<String>;
    if role_union.is_empty() {
        // No Global Role candidate ⇒ legacy path. Tenant-authorised providers
        // (fail-closed: absent ⇒ TenantForbidden).
        let tenant_ok = cfg
            .tenant_providers
            .get(&tenant.id)
            .ok_or(RouteError::TenantForbidden)?;
        intersection = by_model.intersection(tenant_ok).cloned().collect();
    } else {
        intersection = role_union;
        // A rule pinned to the `*` tenant is broad by construction, so its
        // union must still clear the tenant's own authorisation (decision D-3):
        // fail closed against one broad rule re-routing every tenant. A rule
        // already pinned to this tenant needs no such intersection.
        if roles.iter().any(|role| role.tenant_id.as_str() == WILDCARD) {
            let tenant_ok = cfg
                .tenant_providers
                .get(&tenant.id)
                .ok_or(RouteError::TenantForbidden)?;
            intersection.retain(|pid| tenant_ok.contains(pid));
        }
    }

    // (3) The candidate base must serve the model; empty ⇒ NoAvailableProvider.
    intersection.retain(|pid| by_model.contains(pid));
    if intersection.is_empty() {
        return Err(RouteError::NoAvailableProvider);
    }

    // (4) Filter: not dead, has ≥1 api-key, weight > 0.
    //
    // Each drop is recorded WITH the provider and the reason (`excluded`), not
    // just as "some provider was missing": a provider whose last api-key was
    // deleted used to disappear from rotation with no per-provider signal
    // anywhere, so an operator could not tell WHICH one had gone quiet — see
    // `ExclusionReason`.
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut excluded: Vec<ExcludedCandidate> = Vec::new();
    for pid in intersection {
        if breaker.is_dead(&pid) {
            excluded.push(ExcludedCandidate {
                provider_id: pid,
                reason: ExclusionReason::BreakerDead,
            });
            continue;
        }
        if !cfg.provider_keys.get(&pid).is_some_and(|k| !k.is_empty()) {
            excluded.push(ExcludedCandidate {
                provider_id: pid,
                reason: ExclusionReason::NoKey,
            });
            continue;
        }
        let Some(p) = cfg.providers.get(&pid) else {
            // Unknown provider row (the intersection came from `models_by_key`
            // and `tenant_providers`, so this is a data-graph inconsistency, not
            // a configuration the operator wrote). Not a candidate, and not
            // attributable either: nothing here says what the operator did.
            continue;
        };
        // Two different weights, two different meanings — see `ExclusionReason`.
        if p.weight == 0 {
            excluded.push(ExcludedCandidate {
                provider_id: pid,
                reason: ExclusionReason::SoftDisabled,
            });
            continue;
        }
        if p.weight < 0 {
            excluded.push(ExcludedCandidate {
                provider_id: pid,
                reason: ExclusionReason::InvalidWeight,
            });
            continue;
        }
        candidates.push(Candidate {
            provider_id: pid,
            endpoint: p.endpoint.clone(),
            weight: p.weight,
        });
    }

    // An empty candidate set is NOT an error here, deliberately: returning `Err`
    // would discard `excluded` exactly when the attribution matters most (every
    // candidate was dropped, so the caller can say WHO rather than only "nothing
    // available"). [`resolve`] restores the historical error for its callers.
    //
    // (5) Deterministic order (set only — SWRR ordering is the caller's step).
    candidates.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));
    excluded.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));
    Ok(ResolveOutcome {
        candidates,
        excluded,
    })
}

/// One catalog entry: a model key plus the providers that can currently route
/// it (deterministically sorted and deduplicated `provider_id`s).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub model: String,
    pub providers: Vec<String>,
}

/// Enumerate every model a tenant can currently route (design §2.1 — the
/// tenant model catalog; `resolve`'s five-gate semantics as an all-set).
///
/// Mirrors [`resolve`] gate-for-gate across **all** rows of
/// `models_by_key` (which the loader guarantees are `status == 1`):
/// tenant_models whitelist (default-open) → serving providers ∩
/// `tenant_providers` → runtime filter. (The key-prefix binding and sub-tenant
/// route gates this once mirrored are gone — the routing interface is Global
/// Role.) `client_api_key` is accepted for call-compatibility only.
///
/// Equivalent to [`accessible_models_with_principal`] with `principal = None`:
/// no request identity, so a Global Role rule can only be accepted when its
/// group/user/api-key dimensions are all `*`. Snapshots without Global Role
/// rows therefore keep byte-identical legacy behaviour.
///
/// Unlike [`resolve`] this is **infallible** (catalog semantics): an unknown
/// tenant, a tenant without a `tenant_providers` entry, or a model whose
/// every candidate provider is filtered out simply yields no entry — a fully
/// empty catalog returns `Vec::new()` where the per-request chat path would
/// 403 `TenantForbidden` / `NoAvailableProvider`.
///
/// **Existence guard** — each surviving provider `P` is looked up with
/// `cfg.providers.get(P)`; a serving row referencing a provider missing from
/// `cfg.providers` (an orphan; `config::validate` only Warns) is silently
/// dropped, mirroring `resolve`'s `filter_map`. `cfg.providers` is never
/// bare-indexed. `weight` is read from the provider snapshot returned by
/// that guard (`Provider.weight`), **not** from the load-time mirror row in
/// `models_by_key`.
///
/// Filter gate (per candidate `P`): exists in `cfg.providers` ∧ not
/// `breaker.is_dead(P)` ∧ `weight > 0` ∧ ≥ 1 api-key in
/// `cfg.provider_keys[P]`.
///
/// Deterministic: entries sorted by `model`; each `providers` list sorted
/// by `provider_id` and deduplicated (duplicate serving rows collapse).
pub fn accessible_models(
    cfg: &ConfigData,
    breaker: &dyn BreakerView,
    tenant_id: &str,
) -> Vec<CatalogEntry> {
    accessible_models_impl(cfg, breaker, tenant_id, None)
}

/// [`accessible_models`]'s catalog walk with the request's api-key PRINCIPAL,
/// so each model's candidate base comes from the Global Role union when the
/// principal matches (mirrors [`resolve_with_principal`] per model).
///
/// Per model: `match_global_roles(cfg, tenant_id, principal, model_key)` →
/// non-empty provider union ⇒ the union (intersected with
/// `tenant_providers[tenant_id]` when any matched rule pins `tenant_id == "*"`,
/// decision D-3; no `tenant_providers` entry ⇒ that model contributes NOTHING,
/// which is catalog semantics for `resolve`'s `TenantForbidden`); no match /
/// empty union ⇒ the legacy serving ∩ `tenant_providers` intersection (again:
/// no entry ⇒ NO models, not an error). Then the same filter and the same
/// deterministic ordering.
///
/// There is no `client_api_key` parameter — the raw-key gates (3.5 / 3.6) do
/// not apply here, exactly as in [`resolve_with_principal`]. An anonymous
/// (`None`) principal matches only the `*` identity dimensions.
pub fn accessible_models_with_principal(
    cfg: &ConfigData,
    breaker: &dyn BreakerView,
    tenant_id: &str,
    principal: Option<&ApiPrincipal>,
) -> Vec<CatalogEntry> {
    accessible_models_impl(cfg, breaker, tenant_id, principal)
}

/// The ONE catalog walk, seen through both entry points (same single-implementation
/// rule as [`resolve_impl`]): `principal` drives the Global Role gate and
/// `None` selects the legacy candidate base.
fn accessible_models_impl(
    cfg: &ConfigData,
    breaker: &dyn BreakerView,
    tenant_id: &str,
    principal: Option<&ApiPrincipal>,
) -> Vec<CatalogEntry> {
    // (2) Tenant-authorised providers — legacy-fallback branch only (catalog
    // semantics: no mapping, or an unknown tenant, means NO models rather than
    // an error; per-request `resolve` would answer TenantForbidden there). A
    // Global Role candidate base does not need it unless a matched rule pins
    // the `*` tenant, so the lookup is hoisted, not an early return.
    let tenant_ok: Option<&HashSet<String>> = cfg.tenant_providers.get(tenant_id);

    let mut entries: Vec<CatalogEntry> = Vec::new();

    for (model_key, serving) in &cfg.models_by_key {
        // (0) TenantModel gate — default-open: no mapping ⇒ unrestricted; with
        // a mapping, a model outside the whitelist is skipped entirely.
        if let Some(allowed) = cfg.tenant_models.get(tenant_id) {
            if !allowed.contains(model_key) {
                continue;
            }
        }

        // (1+2) Candidate base for this model: the Global Role union when the
        // principal matches rules that carry providers, otherwise serving ∩
        // `tenant_providers`. Both branches end intersected with the model's
        // serving rows, so a rule naming a provider that does not serve this
        // model contributes nothing (fail-closed, mirroring
        // `resolve_with_principal`).
        let serving_ids: HashSet<String> =
            serving.iter().map(|row| row.provider_id.clone()).collect();
        let roles = crate::role::match_global_roles(cfg, tenant_id, principal, model_key);
        let role_union = crate::role::union_role_providers(&roles);
        let providers: Vec<String> = if role_union.is_empty() {
            // No Global Role candidate ⇒ legacy intersection. Catalog semantics:
            // a tenant with no `tenant_providers` entry has NO models (per-request
            // `resolve` would answer TenantForbidden there).
            match tenant_ok {
                None => Vec::new(),
                Some(ok) => serving
                    .iter()
                    .map(|row| row.provider_id.clone())
                    .filter(|pid| ok.contains(pid.as_str()))
                    .collect(),
            }
        } else {
            let role_candidates = role_union
                .iter()
                .filter(|pid| serving_ids.contains(pid.as_str()))
                .cloned();
            if roles.iter().any(|role| role.tenant_id.as_str() == WILDCARD) {
                // A `*`-tenant rule must still clear the tenant's own grants
                // (decision D-3); no grant entry ⇒ NOTHING for this model.
                role_candidates
                    .filter(|pid| tenant_ok.is_some_and(|ok| ok.contains(pid.as_str())))
                    .collect()
            } else {
                // Concrete-tenant rule: the rule already pinned this tenant,
                // so no `tenant_providers` intersection is applied.
                role_candidates.collect()
            }
        };

        // (4) Runtime filter — existence guard first (never a bare index into
        // cfg.providers): drop orphan references, breaker-dead providers,
        // not routable by weight (`<= 0` — a deliberate `0` or an invalid `< 0`;
        // this catalog reports the union, so it does not need to tell them apart —
        // see `ExclusionReason` for where the data plane does), and keyless
        // providers.
        let mut routable: Vec<String> = providers
            .into_iter()
            .filter_map(|pid| {
                let pid = pid.as_str();
                let p = cfg.providers.get(pid)?;
                if p.weight <= 0 || breaker.is_dead(pid) {
                    return None;
                }
                if !cfg
                    .provider_keys
                    .get(pid)
                    .is_some_and(|keys| !keys.is_empty())
                {
                    return None;
                }
                Some(pid.to_string())
            })
            .collect();

        // (5) Deterministic per-entry order + dedup (resolve sorts the set by
        // provider_id; here the full entry list is sorted below).
        routable.sort_unstable();
        routable.dedup();
        if !routable.is_empty() {
            entries.push(CatalogEntry {
                model: model_key.clone(),
                providers: routable,
            });
        }
    }

    // (6) Deterministic overall order (model asc; providers asc per entry).
    entries.sort_by(|a, b| a.model.cmp(&b.model));
    entries
}

// ---------------------------------------------------------------------------
// T6/T7 lane — Global Role candidate selection (plan §3.4) through
// `resolve_with_principal` / `accessible_models_with_principal`.
//
// Fixtures mirror `tests/router.rs` (tenant `t_acme`; `p_a/p_b/p_c` all serve
// `gpt-4o` online at weight 1, each with one api-key) so the role-gate deltas
// are the only difference from the legacy expectations the integration tests
// already pin.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crate::breaker::Breaker;
    use crate::config::{ConfigData, ModelProvider};
    use crate::identity::ApiPrincipal;
    use crate::model::{BillingStatus, GlobalRole, Provider, ResolveOutcome, RouteError, Tenant};

    use super::{accessible_models, accessible_models_with_principal};
    use super::{resolve_detailed, resolve_with_principal};

    fn provider(id: &str, weight: i32) -> Provider {
        Provider {
            id: id.into(),
            key: format!("k_{id}"),
            name: format!("{id} name"),
            endpoint: format!("https://{id}.example.com"),
            weight,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            max_concurrency: None,
            max_queue_depth: None,
            queue_wait_timeout_ms: None,
        }
    }

    fn tenant() -> Tenant {
        Tenant {
            id: "t_acme".into(),
            name: "Acme".into(),
            domain: "acme.com".into(),
            cert_key: None,
            cert_file: None,
            enabled: true,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    fn serving(pid: &str, weight: i32) -> ModelProvider {
        ModelProvider {
            provider_id: pid.into(),
            weight,
        }
    }

    /// A provider that EXISTS, is keyed, weighted 1 and ONLINE for `gpt-4o`,
    /// but is deliberately NOT in `tenant_providers` — the grant gate is what
    /// separates the role branches.
    fn grant_excluded_online(mut cfg: ConfigData, pid: &str) -> ConfigData {
        cfg.providers.insert(pid.into(), provider(pid, 1));
        cfg.provider_keys
            .insert(pid.into(), vec![format!("sk-{pid}")]);
        cfg.models_by_key
            .get_mut("gpt-4o")
            .unwrap()
            .push(serving(pid, 1));
        cfg
    }

    /// Base config: `t_acme` whitelists `{gpt-4o}`, is authorised for
    /// `{p_a,p_b,p_c}`, and all three serve `gpt-4o` online at weight 1.
    fn base_cfg() -> ConfigData {
        let mut cfg = ConfigData::default();
        cfg.tenant_models
            .insert("t_acme".into(), HashSet::from(["gpt-4o".into()]));
        cfg.models_by_key.insert(
            "gpt-4o".into(),
            vec![serving("p_a", 1), serving("p_b", 1), serving("p_c", 1)],
        );
        cfg.tenant_providers.insert(
            "t_acme".into(),
            HashSet::from(["p_a".into(), "p_b".into(), "p_c".into()]),
        );
        for pid in ["p_a", "p_b", "p_c"] {
            cfg.providers.insert(pid.into(), provider(pid, 1));
            cfg.provider_keys
                .insert(pid.into(), vec![format!("sk-{pid}")]);
        }
        cfg
    }

    fn alive() -> Breaker {
        Breaker::default()
    }

    /// The request identity: `t_acme` / group `g_1` / user `u_1` / key `k_1`.
    fn principal() -> ApiPrincipal {
        ApiPrincipal {
            tenant_id: "t_acme".into(),
            user_group_id: "g_1".into(),
            user_id: "u_1".into(),
            api_key_id: "k_1".into(),
            billing_status: BillingStatus::Active,
        }
    }

    /// One rule: identity dimensions all `*`, model `*`, tenant as given.
    fn rule(id: &str, tenant_id: &str, providers: Vec<String>) -> GlobalRole {
        GlobalRole {
            id: id.into(),
            name: format!("name-{id}"),
            tenant_id: tenant_id.into(),
            user_group_id: "*".into(),
            user_id: "*".into(),
            api_key_id: "*".into(),
            model_key: "*".into(),
            provider_ids: providers,
            enabled: true,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    /// Same rule with its `user_id` dimension pinned to someone ELSE — the
    /// request's principal must never reach it.
    fn rule_for_other_user(id: &str, tenant_id: &str, providers: Vec<String>) -> GlobalRole {
        let mut r = rule(id, tenant_id, providers);
        r.user_id = "u_other".into();
        r
    }

    /// Push the rules, then rebuild the derived indices through the ONE sanctioned
    /// entry point (`ConfigData::reindex_global_roles`).
    fn with_roles(cfg: ConfigData, rules: Vec<GlobalRole>) -> ConfigData {
        let mut cfg = cfg;
        for r in rules {
            cfg.global_roles.push(r);
        }
        cfg.reindex_global_roles();
        cfg
    }

    /// Candidates as their provider ids, in reported (sorted) order.
    fn ids_of(out: &ResolveOutcome) -> Vec<String> {
        out.candidates
            .iter()
            .map(|c| c.provider_id.clone())
            .collect()
    }

    // =========================================================================
    // (A) Concrete-tenant rules: the union IS the candidate base — deduplicated
    // and sorted — and `tenant_providers` is NOT consulted (the rule already
    // pinned this tenant), so a provider the rule names may serve even without
    // a grant row.
    // =========================================================================

    #[test]
    fn role_union_is_deduped_and_sorted() {
        let cfg = with_roles(
            grant_excluded_online(base_cfg(), "p_z"),
            vec![
                // p_a listed twice; p_c from this rule, p_b from the other,
                // and the ungranted-but-online p_z.
                rule(
                    "r_a",
                    "t_acme",
                    vec!["p_c".to_string(), "p_a".to_string(), "p_a".to_string()],
                ),
                rule("r_b", "t_acme", vec!["p_b".to_string()]),
                rule("r_z", "t_acme", vec!["p_z".to_string()]),
            ],
        );
        let p = principal();
        let out = resolve_with_principal(&cfg, &alive(), &tenant(), "gpt-4o", Some(&p))
            .expect("role union candidate base");
        assert_eq!(
            ids_of(&out),
            vec![
                "p_a".to_string(),
                "p_b".to_string(),
                "p_c".to_string(),
                "p_z".to_string(),
            ],
            "union deduped, sorted by provider_id, and NOT intersected with the grants",
        );
        assert!(out.excluded.is_empty(), "nothing was filtered in step 4");
    }

    // =========================================================================
    // (B) A `*`-tenant rule (decision D-3): its union must still clear
    // `tenant_providers`, and that gate stays FAIL-CLOSED — no entry at all
    // ⇒ `TenantForbidden`, exactly like the legacy gate.
    // =========================================================================

    #[test]
    fn wildcard_tenant_rule_intersects_tenant_providers() {
        // Union {p_a (granted), p_z (online but NOT granted)} ∪ concrete {p_b}.
        let cfg = with_roles(
            grant_excluded_online(base_cfg(), "p_z"),
            vec![
                rule("r_any", "*", vec!["p_a".to_string(), "p_z".to_string()]),
                rule("r_tn", "t_acme", vec!["p_b".to_string()]),
            ],
        );
        let p = principal();
        let out = resolve_with_principal(&cfg, &alive(), &tenant(), "gpt-4o", Some(&p))
            .expect("wildcard union intersected with the grants");
        assert_eq!(
            ids_of(&out),
            vec!["p_a".to_string(), "p_b".to_string()],
            "p_z is dropped by the grant intersection a `*`-tenant rule requires",
        );

        // Same rule, tenant with NO `tenant_providers` entry ⇒ fail closed.
        let mut stripped = with_roles(
            grant_excluded_online(base_cfg(), "p_z"),
            vec![rule(
                "r_any",
                "*",
                vec!["p_a".to_string(), "p_z".to_string()],
            )],
        );
        stripped.tenant_providers.remove("t_acme");
        assert_eq!(
            resolve_with_principal(&stripped, &alive(), &tenant(), "gpt-4o", Some(&p),)
                .unwrap_err(),
            RouteError::TenantForbidden,
            "a wildcard rule never replaces the fail-closed grant gate",
        );
        // Catalog twin of the same snapshot: infallible ⇒ contributes NOTHING.
        assert!(
            accessible_models_with_principal(&stripped, &alive(), "t_acme", Some(&p),).is_empty(),
            "catalog mirrors the fail-closed gate as an empty set",
        );
    }

    // =========================================================================
    // (C) No matching rule (and no matching providers) ⇒ the LEGACY path:
    // `models_by_key` ∩ `tenant_providers`, fail-closed on a missing entry.
    // =========================================================================

    #[test]
    fn no_matching_rule_falls_back_to_tenant_providers() {
        // The only rule names the online-but-ungranted p_z and is pinned to a
        // different user, so the principal never matches it.
        let cfg = with_roles(
            grant_excluded_online(base_cfg(), "p_z"),
            vec![rule_for_other_user(
                "r_other",
                "t_acme",
                vec!["p_z".to_string()],
            )],
        );
        let p = principal();
        let out = resolve_with_principal(&cfg, &alive(), &tenant(), "gpt-4o", Some(&p))
            .expect("no Global Role candidate → legacy path");
        assert_eq!(
            ids_of(&out),
            vec!["p_a".to_string(), "p_b".to_string(), "p_c".to_string(),],
            "legacy intersection; the unmatched rule's p_z never appears",
        );

        // Matching rules with an EMPTY provider set are "no Global Role
        // candidate" too (see `crate::role`) ⇒ still the legacy path.
        let empty_union = with_roles(base_cfg(), vec![rule("r_empty", "t_acme", Vec::new())]);
        assert_eq!(
            ids_of(
                &resolve_with_principal(&empty_union, &alive(), &tenant(), "gpt-4o", Some(&p))
                    .expect("empty union → legacy fallback"),
            ),
            vec!["p_a".to_string(), "p_b".to_string(), "p_c".to_string(),],
        );

        // And the legacy gate keeps its fail-closed TenantForbidden.
        let mut stripped = with_roles(
            base_cfg(),
            vec![rule_for_other_user(
                "r_other",
                "t_acme",
                vec!["p_a".to_string()],
            )],
        );
        stripped.tenant_providers.remove("t_acme");
        assert_eq!(
            resolve_with_principal(&stripped, &alive(), &tenant(), "gpt-4o", Some(&p),)
                .unwrap_err(),
            RouteError::TenantForbidden,
        );
    }

    // =========================================================================
    // (D) A rule may only route through providers ONLINE for the model: the
    // union is intersected with `models_by_key`, so an offline/unknown provider
    // contributes nothing (fail-closed).
    // =========================================================================

    #[test]
    fn role_provider_not_online_for_the_model_is_dropped() {
        let mut cfg = base_cfg();
        // p_d is granted, exists, weighted 1 and keyed — but serves NOTHING.
        cfg.providers.insert("p_d".into(), provider("p_d", 1));
        cfg.provider_keys.insert("p_d".into(), vec!["sk-d".into()]);
        cfg.tenant_providers
            .get_mut("t_acme")
            .unwrap()
            .insert("p_d".into());
        let cfg = with_roles(cfg, vec![rule("r_d", "t_acme", vec!["p_d".to_string()])]);
        let p = principal();
        assert_eq!(
            resolve_with_principal(&cfg, &alive(), &tenant(), "gpt-4o", Some(&p)).unwrap_err(),
            RouteError::NoAvailableProvider,
            "the rule's only provider does not serve the model",
        );

        // Mixed union: the non-serving provider drops, the serving one stays
        // (no fall-through to the legacy set, deliberately).
        let mixed = with_roles(
            cfg,
            vec![rule(
                "r_mix",
                "t_acme",
                vec!["p_d".to_string(), "p_b".to_string()],
            )],
        );
        let out = resolve_with_principal(&mixed, &alive(), &tenant(), "gpt-4o", Some(&p))
            .expect("p_b still serves");
        assert_eq!(ids_of(&out), vec!["p_b".to_string()]);
    }

    // =========================================================================
    // (E) Catalog twin: `accessible_models_with_principal` enumerates exactly
    // the set `resolve_with_principal` resolved, per model.
    // =========================================================================

    #[test]
    fn accessible_models_mirrors_the_role_resolved_set() {
        let cfg = with_roles(
            grant_excluded_online(base_cfg(), "p_z"),
            vec![
                rule("r_any", "*", vec!["p_a".to_string(), "p_z".to_string()]),
                rule("r_tn", "t_acme", vec!["p_b".to_string()]),
            ],
        );
        let p = principal();
        let out = resolve_with_principal(&cfg, &alive(), &tenant(), "gpt-4o", Some(&p))
            .expect("resolved set");
        let entries = accessible_models_with_principal(&cfg, &alive(), "t_acme", Some(&p));
        assert_eq!(entries.len(), 1, "one whitelisted model");
        assert_eq!(
            entries[0].providers,
            ids_of(&out),
            "catalog providers == resolved candidate set (p_z cleared by the grants)",
        );

        // Concrete-tenant rule naming an UNGRANTED provider: both views agree
        // that it serves (no grant intersection on that branch).
        let concrete = with_roles(
            grant_excluded_online(base_cfg(), "p_z"),
            vec![rule("r_z", "t_acme", vec!["p_z".to_string()])],
        );
        let out2 = resolve_with_principal(&concrete, &alive(), &tenant(), "gpt-4o", Some(&p))
            .expect("concrete rule serves its own provider");
        let entries2 = accessible_models_with_principal(&concrete, &alive(), "t_acme", Some(&p));
        assert_eq!(
            ids_of(&out2),
            vec!["p_z".to_string()],
            "no grant intersection here"
        );
        assert_eq!(
            entries2[0].providers,
            ids_of(&out2),
            "catalog matches the resolved set for the same rule",
        );
    }

    // =========================================================================
    // (F) The legacy wrappers stay byte-identical: `resolve_detailed` /
    // `accessible_models` are the same pipeline with `principal = None`, so an
    // anonymous request reaches only rules whose identity dims are all `*`.
    // =========================================================================

    #[test]
    fn legacy_wrappers_keep_principal_less_behaviour() {
        // (a) Snapshot without Global Role rows: the wrapper equals the
        // principal-aware entry point and the legacy set.
        let cfg = base_cfg();
        let detailed =
            resolve_detailed(&cfg, &alive(), &tenant(), "gpt-4o").expect("legacy resolve");
        let principal_less =
            resolve_with_principal(&cfg, &alive(), &tenant(), "gpt-4o", None).expect("same path");
        assert_eq!(detailed, principal_less, "wrapper delegates, does not fork");
        assert_eq!(
            ids_of(&detailed),
            vec!["p_a".to_string(), "p_b".to_string(), "p_c".to_string(),],
        );
        assert_eq!(
            accessible_models(&cfg, &alive(), "t_acme"),
            accessible_models_with_principal(&cfg, &alive(), "t_acme", None),
            "catalog wrappers agree when no rules exist",
        );

        // (b) A rule pinned to THIS user never matches an anonymous request:
        // the wrapper keeps the legacy tenant answer.
        let identified_only = with_roles(
            grant_excluded_online(base_cfg(), "p_z"),
            vec![rule_for_other_user(
                "r_u_other",
                "t_acme",
                vec!["p_z".to_string()],
            )],
        );
        assert_eq!(
            ids_of(
                &resolve_detailed(&identified_only, &alive(), &tenant(), "gpt-4o")
                    .expect("anonymous → legacy"),
            ),
            vec!["p_a".to_string(), "p_b".to_string(), "p_c".to_string(),],
        );

        // (c) An all-`*` identity rule DOES match an anonymous request (see
        // `crate::role`), so `resolve_detailed` follows the role base there too
        // — documented, not a fork: same `ExcludedCandidate` attribution.
        let all_wild = with_roles(
            grant_excluded_online(base_cfg(), "p_z"),
            vec![rule("r_wild", "t_acme", vec!["p_z".to_string()])],
        );
        assert_eq!(
            ids_of(
                &resolve_detailed(&all_wild, &alive(), &tenant(), "gpt-4o")
                    .expect("all-* rule matches the anonymous request"),
            ),
            vec!["p_z".to_string()],
        );
    }
}
