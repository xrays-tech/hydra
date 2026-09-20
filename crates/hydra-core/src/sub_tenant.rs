//! Sub-tenant write validation (design-sub-tenant.md §3.3) — pure, error-level
//! fail-closed.
//!
//! The write path calls [`validate_sub_tenant_write`] (or
//! [`validate_sub_tenant_write_against`], which the transactional write core
//! uses) before every sub-tenant / sub-tenant-route insert or update; a
//! rejected write is never persisted. This is the **error-level** gate for
//! tenant input — distinct from [`crate::config::validate`], which is a
//! snapshot-side, **Warn-only** orphan backstop and explicitly *not* the sole
//! line of defence (design §3.3, §10.9).
//!
//! Pure: reads a [`ConfigData`] snapshot plus a caller-supplied snapshot of
//! ALL sub-tenant / route rows ([`SubTenantRows`], including disabled), no I/O.
//! Provider / model / tenant membership and enabled operator key-prefix
//! bindings come from the [`ConfigData`]; the row-level checks (name / prefix
//! collisions, overlap, quota) run against the supplied rows, so the quota
//! counts **all** rows (not just the enabled snapshot) and the overlap check
//! closes the validate-then-insert TOCTOU (A-2 prerequisites 1/2).
//!
//! ## Rules (design §3.3)
//!
//! 1. A route's `provider_id` must be a known provider AND be in the tenant's
//!    `tenant_providers` (from the [`ConfigData`]).
//! 2. If a route's `model_key` is set, it must be in the tenant's
//!    `tenant_models` (default-open when the tenant has no mapping) AND be
//!    served by that provider (`models_by_key`, from the [`ConfigData`]).
//! 3. A sub-tenant's `name` must be non-empty, printable ASCII (no control
//!    chars), contain no `/`, and be at most [`MAX_SUB_TENANT_NAME_LEN`] bytes
//!    (D9). Its `key_prefix` must be non-empty, ASCII, contain a separator
//!    (`_` or `-`), and must not overlap (either `starts_with` direction) any
//!    same-tenant existing `key_prefix` or any enabled operator
//!    `key_prefix_binding` (operator bindings from the [`ConfigData`],
//!    same-tenant prefixes from the supplied rows).
//! 4. Per-tenant / per-sub-tenant quotas ([`MAX_SUB_TENANTS_PER_TENANT`],
//!    [`MAX_ROUTES_PER_SUB_TENANT`]) guard against config-DoS via the API and
//!    count **all** rows in the supplied snapshot (including disabled).

use crate::config::ConfigData;
use crate::model::{SubTenant, SubTenantRoute};

/// Per-tenant cap on the number of sub-tenants (config-DoS guard, design §7.5).
pub const MAX_SUB_TENANTS_PER_TENANT: usize = 64;

/// Per-sub-tenant cap on the number of routes (config-DoS guard, design §7.5).
pub const MAX_ROUTES_PER_SUB_TENANT: usize = 32;

/// D9 — cap on a sub-tenant `name` length, in bytes. A `name` is URL-keyed in
/// v2 (`PUT /sub-tenants/{name}`), so it must stay addressable; capping the
/// length (and forbidding `/`, see [`SubTenantWriteError::NameInvalid`]) keeps
/// it well-formed as a path segment.
pub const MAX_SUB_TENANT_NAME_LEN: usize = 128;

/// A rejected sub-tenant / sub-tenant-route write (design §3.3). One variant
/// per rule so the admin handler can map it to a precise 400/409 response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubTenantWriteError {
    /// The route's `provider_id` is not a known provider (`cfg.providers`).
    ProviderNotFound,
    /// The route's `provider_id` is known but not authorised for the tenant
    /// (absent from `cfg.tenant_providers[tenant_id]`).
    ProviderNotInTenant,
    /// The route's `model_key` is outside the tenant's `tenant_models`
    /// whitelist (only checked when the tenant HAS a mapping).
    ModelNotInTenant,
    /// The route's `provider_id` does not serve `model_key` (absent from
    /// `cfg.models_by_key[model_key]`).
    ModelNotServedByProvider,
    /// `name` violates the D9 charset rule: empty, non-ASCII, a control char,
    /// a `/`, or over [`MAX_SUB_TENANT_NAME_LEN`] bytes.
    NameInvalid,
    /// `key_prefix` is empty.
    PrefixEmpty,
    /// `key_prefix` contains a non-ASCII byte.
    PrefixNonAscii,
    /// `key_prefix` has no separator (`_` or `-`); a bare prefix would swallow
    /// longer, unrelated prefixes.
    PrefixNoSeparator,
    /// `key_prefix` overlaps (either `starts_with` direction) an existing
    /// same-tenant sub-tenant `key_prefix` or an enabled operator
    /// `key_prefix_binding`.
    PrefixOverlap,
    /// The tenant already has [`MAX_SUB_TENANTS_PER_TENANT`] sub-tenants.
    SubTenantQuotaExceeded,
    /// The sub-tenant already has [`MAX_ROUTES_PER_SUB_TENANT`] routes.
    RouteQuotaExceeded,
    /// `name` is already used by another sub-tenant in the same tenant (→ 409).
    NameDuplicate,
    /// `key_prefix` is already used by another sub-tenant in the same tenant
    /// (→ 409).
    PrefixDuplicate,
}

impl std::fmt::Display for SubTenantWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            Self::ProviderNotFound => "provider does not exist",
            Self::ProviderNotInTenant => "provider is not authorised for this tenant",
            Self::ModelNotInTenant => "model is not in the tenant's allowed models",
            Self::ModelNotServedByProvider => "provider does not serve this model",
            Self::NameInvalid => {
                "name is invalid (must be non-empty printable ASCII, no '/', \
                 and at most {MAX_SUB_TENANT_NAME_LEN} bytes)"
            }
            Self::PrefixEmpty => "key_prefix must not be empty",
            Self::PrefixNonAscii => "key_prefix must be ASCII",
            Self::PrefixNoSeparator => "key_prefix must contain a separator ('_' or '-')",
            Self::PrefixOverlap => "key_prefix overlaps an existing prefix",
            Self::SubTenantQuotaExceeded => "per-tenant sub-tenant quota exceeded",
            Self::RouteQuotaExceeded => "per-sub-tenant route quota exceeded",
            Self::NameDuplicate => "name already used by another sub-tenant in this tenant",
            Self::PrefixDuplicate => "key_prefix already used by another sub-tenant in this tenant",
        };
        f.write_str(msg)
    }
}

/// The sub-tenant write being validated (create or update).
///
/// The `*_id` fields are `Some` on update (so the row's own existing prefix can
/// be excluded from self-comparison) and `None` on create. `tenant_id` is the
/// authoritative tenant for the write (the handler resolves it from the request
/// for sub-tenant writes, and from the sub-tenant row for route writes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubTenantWrite {
    /// Create or update a sub-tenant (its `name` / `key_prefix`).
    SubTenant {
        tenant_id: String,
        name: String,
        key_prefix: String,
        sub_tenant_id: Option<String>,
    },
    /// Create or update a sub-tenant route (its `provider_id` / `model_key`).
    Route {
        tenant_id: String,
        sub_tenant_id: String,
        provider_id: String,
        model_key: Option<String>,
        route_id: Option<String>,
    },
}

/// A caller-supplied snapshot of ALL sub-tenant / route rows (including
/// disabled) that the write validator checks name / prefix collisions, overlap
/// and quota against.
///
/// This is the v2 tightening (A-2 prerequisites 1/2): the write core reads the
/// live DB rows **inside the write transaction** and passes them here, so the
/// quota counts every row (not just the enabled [`ConfigData`] snapshot) and
/// the overlap check runs against what is actually in the DB (closing the
/// validate-then-insert TOCTOU). Provider / model / tenant membership and
/// enabled operator key-prefix bindings still come from the [`ConfigData`]
/// snapshot, which is the authoritative source for those.
#[derive(Clone, Copy, Debug)]
pub struct SubTenantRows<'a> {
    /// All sub-tenant rows for every tenant (including disabled).
    pub sub_tenants: &'a [SubTenant],
    /// All sub-tenant-route rows for every sub-tenant (including disabled).
    pub routes: &'a [SubTenantRoute],
}

/// Validate a sub-tenant / sub-tenant-route write against a config snapshot,
/// using the sub-tenant / route rows **already carried by** `cfg` as the row
/// snapshot.
///
/// ⚠️ **Test / convenience only — NOT the production write path.** It builds a
/// [`SubTenantRows`] from `cfg.sub_tenants` / `cfg.sub_tenant_routes` (the
/// **enabled-only** snapshot), so the quota and overlap checks here see only
/// enabled rows. The production transactional write core
/// (`hydra-server` `admin::sub_tenant_write`) reads the FULL DB rows inside the
/// write transaction and calls [`validate_sub_tenant_write_against`] directly,
/// so it counts **all** rows (including disabled) and closes the
/// validate-then-insert TOCTOU (A-2 prerequisites 1/2). Do not use this
/// enabled-only wrapper for a production write.
///
/// Returns `Ok(())` when the write satisfies every rule (design §3.3); `Err`
/// with a single [`SubTenantWriteError`] naming the first violated rule. The
/// checks are **fail-closed**: any violation is an error (never a silent
/// accept), and the caller must not persist the write.
pub fn validate_sub_tenant_write(
    cfg: &ConfigData,
    write: &SubTenantWrite,
) -> Result<(), SubTenantWriteError> {
    let rows = SubTenantRows {
        sub_tenants: &cfg.sub_tenants,
        routes: &cfg.sub_tenant_routes,
    };
    validate_sub_tenant_write_against(cfg, &rows, write)
}

/// Validate a sub-tenant / sub-tenant-route write against a [`ConfigData`]
/// snapshot (for provider / model / tenant membership and enabled operator
/// key-prefix bindings) AND a caller-supplied [`SubTenantRows`] snapshot of ALL
/// sub-tenant / route rows (for name / prefix collisions, overlap and quota,
/// including disabled rows).
///
/// This is the function the **transactional write core** calls: it runs the
/// row-level checks against the live DB rows read inside the write transaction,
/// so the quota counts every row and the overlap check closes the
/// validate-then-insert TOCTOU (A-2 prerequisites 1/2).
///
/// Returns `Ok(())` when the write satisfies every rule (design §3.3); `Err`
/// with a single [`SubTenantWriteError`] naming the first violated rule. The
/// checks are **fail-closed**: any violation is an error (never a silent
/// accept), and the caller must not persist the write.
pub fn validate_sub_tenant_write_against(
    cfg: &ConfigData,
    rows: &SubTenantRows<'_>,
    write: &SubTenantWrite,
) -> Result<(), SubTenantWriteError> {
    match write {
        SubTenantWrite::SubTenant {
            tenant_id,
            name,
            key_prefix,
            sub_tenant_id,
        } => validate_sub_tenant(
            cfg,
            rows,
            tenant_id,
            name,
            key_prefix,
            sub_tenant_id.as_deref(),
        ),
        SubTenantWrite::Route {
            tenant_id,
            sub_tenant_id,
            provider_id,
            model_key,
            route_id,
        } => validate_route(
            cfg,
            rows,
            tenant_id,
            sub_tenant_id,
            provider_id,
            model_key.as_deref(),
            route_id.as_deref(),
        ),
    }
}

/// True when `a` and `b` are both non-empty and one is a prefix of the other
/// (either direction). An empty operand never overlaps.
fn prefix_overlap(a: &str, b: &str) -> bool {
    !a.is_empty() && !b.is_empty() && (a.starts_with(b) || b.starts_with(a))
}

/// Rule 3 (name charset + prefix shape + name/prefix collisions) + rule 4
/// (sub-tenant quota). Name / prefix collisions and the quota run against the
/// supplied [`SubTenantRows`] (ALL rows, incl. disabled); operator-binding
/// overlap comes from the [`ConfigData`].
fn validate_sub_tenant(
    cfg: &ConfigData,
    rows: &SubTenantRows<'_>,
    tenant_id: &str,
    name: &str,
    key_prefix: &str,
    self_id: Option<&str>,
) -> Result<(), SubTenantWriteError> {
    // D9 — `name` charset rule (see [`valid_name`]).
    if !valid_name(name) {
        return Err(SubTenantWriteError::NameInvalid);
    }

    // Rule 3 — prefix shape.
    if key_prefix.is_empty() {
        return Err(SubTenantWriteError::PrefixEmpty);
    }
    if !key_prefix.bytes().all(|b| b.is_ascii()) {
        return Err(SubTenantWriteError::PrefixNonAscii);
    }
    if !key_prefix.contains(['_', '-']) {
        return Err(SubTenantWriteError::PrefixNoSeparator);
    }

    // Rule 3 — same-tenant name / prefix collisions (duplicate → 409, overlap
    // → 400) against ALL rows (incl. disabled). The row being updated
    // (`self_id`) is excluded so an update that keeps its own name/prefix is
    // not self-rejected.
    for st in rows
        .sub_tenants
        .iter()
        .filter(|s| s.tenant_id == tenant_id && self_id.is_none_or(|sid| s.id != sid))
    {
        if st.name == name {
            return Err(SubTenantWriteError::NameDuplicate);
        }
        if st.key_prefix == key_prefix {
            return Err(SubTenantWriteError::PrefixDuplicate);
        }
        if prefix_overlap(&st.key_prefix, key_prefix) {
            return Err(SubTenantWriteError::PrefixOverlap);
        }
    }

    // Rule 3 — overlap with enabled operator key_prefix_bindings (a global
    // namespace, from the authoritative `ConfigData` snapshot).
    for b in cfg.key_prefix_bindings.iter().filter(|b| b.enabled) {
        if prefix_overlap(&b.key_prefix, key_prefix) {
            return Err(SubTenantWriteError::PrefixOverlap);
        }
    }

    // Rule 4 — per-tenant quota over ALL rows (create only: an update does not
    // add a row). Counting every row (not just enabled) is what stops
    // disable-then-recreate from growing the DB past the cap (A-2 finding 1).
    if self_id.is_none() {
        let count = rows
            .sub_tenants
            .iter()
            .filter(|s| s.tenant_id == tenant_id)
            .count();
        if count >= MAX_SUB_TENANTS_PER_TENANT {
            return Err(SubTenantWriteError::SubTenantQuotaExceeded);
        }
    }

    Ok(())
}

/// D9 — the `name` charset rule: non-empty, at most [`MAX_SUB_TENANT_NAME_LEN`]
/// bytes, printable ASCII (no control bytes), and no `/`.
///
/// Mirrors the `key_prefix` ASCII style. A `/` would make the v2 URL-keyed
/// `PUT /sub-tenants/{name}` unaddressable (a name containing a path
/// separator), so it is rejected up front; the length cap keeps names
/// well-formed as a single path segment.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SUB_TENANT_NAME_LEN
        && !name.contains('/')
        && name.bytes().all(|b| b.is_ascii() && !b.is_ascii_control())
}

/// Rule 1 (provider membership, from `cfg`) + rule 2 (model membership /
/// service, from `cfg`) + rule 4 (route quota, over the supplied rows).
fn validate_route(
    cfg: &ConfigData,
    rows: &SubTenantRows<'_>,
    tenant_id: &str,
    sub_tenant_id: &str,
    provider_id: &str,
    model_key: Option<&str>,
    self_route_id: Option<&str>,
) -> Result<(), SubTenantWriteError> {
    // Rule 1 — provider exists and is authorised for the tenant (from the
    // authoritative `ConfigData` snapshot).
    if !cfg.providers.contains_key(provider_id) {
        return Err(SubTenantWriteError::ProviderNotFound);
    }
    let in_tenant = cfg
        .tenant_providers
        .get(tenant_id)
        .is_some_and(|set| set.contains(provider_id));
    if !in_tenant {
        return Err(SubTenantWriteError::ProviderNotInTenant);
    }

    // Rule 2 — model in the tenant whitelist (default-open with no mapping) and
    // served by the provider.
    if let Some(model) = model_key {
        if let Some(allowed) = cfg.tenant_models.get(tenant_id) {
            if !allowed.contains(model) {
                return Err(SubTenantWriteError::ModelNotInTenant);
            }
        }
        let served = cfg
            .models_by_key
            .get(model)
            .is_some_and(|list| list.iter().any(|mp| mp.provider_id == provider_id));
        if !served {
            return Err(SubTenantWriteError::ModelNotServedByProvider);
        }
    }

    // Rule 4 — per-sub-tenant route quota over ALL rows (create only: an update
    // does not add a row). Counting every row (not just enabled) is what stops
    // disable-then-recreate from growing the DB past the cap (A-2 finding 1).
    if self_route_id.is_none() {
        let count = rows
            .routes
            .iter()
            .filter(|r| r.sub_tenant_id == sub_tenant_id)
            .count();
        if count >= MAX_ROUTES_PER_SUB_TENANT {
            return Err(SubTenantWriteError::RouteQuotaExceeded);
        }
    }

    Ok(())
}
