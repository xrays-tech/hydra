//! Router — candidate resolution (pure).
//!
//! This module re-exports the shared routing types ([`Candidate`], [`RouteError`])
//! and implements the pure [`resolve`] function.
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
//! 2. **Tenant providers** — the tenant's authorised provider set (fail-closed:
//!    absent ⇒ [`RouteError::TenantForbidden`]).
//! 3. **Intersection** of (1) and (2); empty ⇒ [`RouteError::NoAvailableProvider`].
//!    - **Key-prefix binding gate** — a client api-key matching an enabled
//!      prefix binding restricts the set to the bound provider (fail-closed;
//!      longest prefix wins; no match ⇒ no restriction).
//! 4. **Filter** — drop dead (`breaker.is_dead`), keyless (no api-keys), and
//!    soft-disabled (`weight <= 0`); empty ⇒ [`RouteError::NoAvailableProvider`].
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
//! tenant_providers → key-prefix binding → dead / weight / api-key filter).
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
use crate::model::{ProviderKeyBinding, Tenant};

pub use crate::model::{Candidate, RouteError};

/// Longest-prefix match of a client api-key against the enabled prefix
/// bindings (design §7.1b). Returns the binding with the longest `key_prefix`
/// that `api_key` starts with; `None` when no enabled binding matches
/// (⇒ no routing restriction).
pub fn match_key_binding<'a>(
    bindings: &'a [ProviderKeyBinding],
    api_key: &str,
) -> Option<&'a ProviderKeyBinding> {
    bindings
        .iter()
        .filter(|b| b.enabled && api_key.starts_with(&b.key_prefix))
        .max_by_key(|b| b.key_prefix.len())
}

/// Resolve the candidate set for one `(tenant, model_key)` request.
///
/// See the module docs for the full pipeline. The returned `Vec` is sorted by
/// `provider_id` (deterministic regardless of `HashSet` iteration order); the
/// caller then applies [`crate::swrr::order`] with its own per-`(tenant, model)`
/// state to pick the first attempt and order failover.
///
/// `client_api_key` feeds the §7.1b key-prefix binding gate; `None` (or no
/// matching binding) leaves the candidate set unrestricted.
pub fn resolve(
    cfg: &ConfigData,
    breaker: &dyn BreakerView,
    tenant: &Tenant,
    model_key: &str,
    client_api_key: Option<&str>,
) -> Result<Vec<Candidate>, RouteError> {
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

    // (2) Tenant-authorised providers.
    let tenant_ok = cfg
        .tenant_providers
        .get(&tenant.id)
        .ok_or(RouteError::TenantForbidden)?;

    // (3) Intersection.
    let mut intersection: Vec<String> = by_model.intersection(tenant_ok).cloned().collect();
    if intersection.is_empty() {
        return Err(RouteError::NoAvailableProvider);
    }

    // (3.5) Key-prefix binding gate (design §7.1b): a client api-key whose raw
    // value matches an enabled prefix binding restricts the candidate set to
    // the bound provider — fail-closed (never falls back to unbound
    // providers). Longest prefix wins; no match ⇒ no restriction.
    if let Some(api_key) = client_api_key {
        if let Some(binding) = match_key_binding(&cfg.key_prefix_bindings, api_key) {
            intersection.retain(|pid| pid == &binding.provider_id);
            if intersection.is_empty() {
                return Err(RouteError::NoAvailableProvider);
            }
        }
    }

    // (4) Filter: not dead, has ≥1 api-key, weight > 0.
    let mut candidates: Vec<Candidate> = intersection
        .into_iter()
        .filter(|pid| !breaker.is_dead(pid))
        .filter(|pid| cfg.provider_keys.get(pid).is_some_and(|k| !k.is_empty()))
        .filter_map(|pid| {
            let p = cfg.providers.get(&pid)?;
            Some(Candidate {
                provider_id: pid,
                endpoint: p.endpoint.clone(),
                weight: p.weight,
            })
        })
        .filter(|c| c.weight > 0)
        .collect();

    if candidates.is_empty() {
        return Err(RouteError::NoAvailableProvider);
    }

    // (5) Deterministic order (set only — SWRR ordering is the caller's step).
    candidates.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));
    Ok(candidates)
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
/// `tenant_providers` → key-prefix binding gate ([`match_key_binding`];
/// `client_api_key` is `None` or unmatched ⇒ no restriction) → filter.
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
    client_api_key: Option<&str>,
) -> Vec<CatalogEntry> {
    // (2) Tenant-authorised providers — catalog semantics: no mapping (or an
    // unknown tenant) means no models, not an error (per-request resolve
    // would be TenantForbidden here).
    let Some(tenant_ok) = cfg.tenant_providers.get(tenant_id) else {
        return Vec::new();
    };

    let mut entries: Vec<CatalogEntry> = Vec::new();

    for (model_key, serving) in &cfg.models_by_key {
        // (0) TenantModel gate — default-open: no mapping ⇒ unrestricted; with
        // a mapping, a model outside the whitelist is skipped entirely.
        if let Some(allowed) = cfg.tenant_models.get(tenant_id) {
            if !allowed.contains(model_key) {
                continue;
            }
        }

        // (1+2) Providers serving this model ∩ tenant-authorised providers.
        let mut providers: Vec<&str> = serving
            .iter()
            .map(|row| row.provider_id.as_str())
            .filter(|pid| tenant_ok.contains(*pid))
            .collect();

        // (3.5) Key-prefix binding gate — match ⇒ keep only the bound provider
        // (fail-closed); no match (or None key) ⇒ no restriction.
        if let Some(api_key) = client_api_key {
            if let Some(binding) = match_key_binding(&cfg.key_prefix_bindings, api_key) {
                providers.retain(|pid| *pid == binding.provider_id);
            }
        }

        // (4) Runtime filter — existence guard first (never a bare index into
        // cfg.providers): drop orphan references, breaker-dead providers,
        // soft-disabled (weight ≤ 0, read from the provider snapshot), and
        // keyless providers.
        let mut routable: Vec<String> = providers
            .into_iter()
            .filter_map(|pid| {
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
