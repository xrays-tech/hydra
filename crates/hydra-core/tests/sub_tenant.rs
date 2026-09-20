//! T6.3 — sub-tenant write validation (design-sub-tenant.md §3.3).
//!
//! Pure, error-level, fail-closed gate the admin write path calls before every
//! sub-tenant / sub-tenant-route insert or update. One test per rule, plus
//! overlap in both directions and the quota boundaries. This is distinct from
//! `config::validate` (a snapshot-side, Warn-only backstop): the write path is
//! the line of defence for tenant input (design §3.3, §10.9).

use std::collections::HashSet;

use hydra_core::config::ConfigData;
use hydra_core::model::{Provider, ProviderKeyBinding, SubTenant, SubTenantRoute, Tenant};
use hydra_core::sub_tenant::{
    validate_sub_tenant_write, validate_sub_tenant_write_against, SubTenantRows, SubTenantWrite,
    SubTenantWriteError, MAX_ROUTES_PER_SUB_TENANT, MAX_SUB_TENANTS_PER_TENANT,
    MAX_SUB_TENANT_NAME_LEN,
};
use pretty_assertions::assert_eq;

// --- fixtures ----------------------------------------------------------------

fn tenant(id: &str, domain: &str) -> Tenant {
    Tenant {
        id: id.into(),
        name: format!("{id} name"),
        domain: domain.into(),
        auth_url: format!("https://auth.{domain}/verify"),
        cert_key: None,
        cert_file: None,
        enabled: true,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    }
}

fn provider(id: &str, endpoint: &str, weight: i32) -> Provider {
    Provider {
        id: id.into(),
        key: format!("key_{id}"),
        name: format!("{id} name"),
        endpoint: endpoint.into(),
        weight,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        max_concurrency: None,
        max_queue_depth: None,
        queue_wait_timeout_ms: None,
    }
}

fn sub_tenant(id: &str, tenant_id: &str, name: &str, key_prefix: &str) -> SubTenant {
    SubTenant {
        id: id.into(),
        tenant_id: tenant_id.into(),
        name: name.into(),
        key_prefix: key_prefix.into(),
        enabled: true,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    }
}

fn sub_tenant_route(
    id: &str,
    sub_tenant_id: &str,
    model_key: Option<&str>,
    provider_id: &str,
) -> SubTenantRoute {
    SubTenantRoute {
        id: id.into(),
        sub_tenant_id: sub_tenant_id.into(),
        model_key: model_key.map(str::to_string),
        provider_id: provider_id.into(),
        enabled: true,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    }
}

fn binding(id: &str, prefix: &str, provider_id: &str, enabled: bool) -> ProviderKeyBinding {
    ProviderKeyBinding {
        id: id.into(),
        key_prefix: prefix.into(),
        provider_id: provider_id.into(),
        enabled,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    }
}

/// A well-formed snapshot: tenant `t1` is authorised for provider `p1`, which
/// serves model `gpt-4o` (the tenant's only allowed model).
fn base() -> ConfigData {
    let mut cfg = ConfigData::default();

    let t = tenant("t1", "acme.com");
    cfg.tenants_by_domain.insert(t.domain.clone(), t.clone());

    cfg.providers
        .insert("p1".into(), provider("p1", "https://a.io", 3));
    cfg.provider_keys.insert("p1".into(), vec!["sk-1".into()]);
    cfg.providers
        .insert("p2".into(), provider("p2", "https://b.io", 1));
    cfg.provider_keys.insert("p2".into(), vec!["sk-2".into()]);

    // `gpt-4o` is served by BOTH p1 and p2 (so a route may steer either way);
    // `solo` is served only by p2.
    cfg.models_by_key.insert(
        "gpt-4o".into(),
        vec![
            hydra_core::config::ModelProvider {
                provider_id: "p1".into(),
                weight: 3,
            },
            hydra_core::config::ModelProvider {
                provider_id: "p2".into(),
                weight: 1,
            },
        ],
    );
    cfg.models_by_key.insert(
        "solo".into(),
        vec![hydra_core::config::ModelProvider {
            provider_id: "p2".into(),
            weight: 1,
        }],
    );

    let mut tps = HashSet::new();
    tps.insert("p1".to_string());
    cfg.tenant_providers.insert("t1".into(), tps);

    let mut tms = HashSet::new();
    tms.insert("gpt-4o".to_string());
    cfg.tenant_models.insert("t1".into(), tms);

    cfg
}

fn st_write(tenant_id: &str, name: &str, key_prefix: &str) -> SubTenantWrite {
    SubTenantWrite::SubTenant {
        tenant_id: tenant_id.into(),
        name: name.into(),
        key_prefix: key_prefix.into(),
        sub_tenant_id: None,
    }
}

fn st_write_update(tenant_id: &str, name: &str, key_prefix: &str, id: &str) -> SubTenantWrite {
    SubTenantWrite::SubTenant {
        tenant_id: tenant_id.into(),
        name: name.into(),
        key_prefix: key_prefix.into(),
        sub_tenant_id: Some(id.to_string()),
    }
}

fn route_write(
    tenant_id: &str,
    sub_tenant_id: &str,
    provider_id: &str,
    model_key: Option<&str>,
) -> SubTenantWrite {
    SubTenantWrite::Route {
        tenant_id: tenant_id.into(),
        sub_tenant_id: sub_tenant_id.into(),
        provider_id: provider_id.into(),
        model_key: model_key.map(str::to_string),
        route_id: None,
    }
}

// --- rule 1: provider membership --------------------------------------------

/// A clean sub-tenant create is accepted.
#[test]
fn create_sub_tenant_valid() {
    let cfg = base();
    assert_eq!(
        validate_sub_tenant_write(&cfg, &st_write("t1", "team-a", "AAA_")),
        Ok(())
    );
}

/// Rule 1 — a route pointing at an unknown provider is rejected.
#[test]
fn route_provider_not_found() {
    let cfg = base();
    let cfg = {
        let mut c = cfg;
        c.sub_tenants
            .push(sub_tenant("st1", "t1", "team-a", "AAA_"));
        c
    };
    assert_eq!(
        validate_sub_tenant_write(&cfg, &route_write("t1", "st1", "ghost", Some("gpt-4o"))),
        Err(SubTenantWriteError::ProviderNotFound)
    );
}

/// Rule 1 — a route pointing at a known provider NOT authorised for the tenant
/// is rejected.
#[test]
fn route_provider_not_in_tenant() {
    let cfg = base();
    // t1 is only authorised for p1; p2 is a valid provider but not in t1's set.
    let cfg = {
        let mut c = cfg;
        c.sub_tenants
            .push(sub_tenant("st1", "t1", "team-a", "AAA_"));
        c
    };
    assert_eq!(
        validate_sub_tenant_write(&cfg, &route_write("t1", "st1", "p2", Some("gpt-4o"))),
        Err(SubTenantWriteError::ProviderNotInTenant)
    );
}

// --- rule 2: model membership / service -------------------------------------

/// Rule 2 — a route's model outside the tenant's whitelist is rejected.
#[test]
fn route_model_not_in_tenant() {
    let cfg = base();
    // `solo` is a real model (served by p2) but t1's whitelist only has gpt-4o.
    let cfg = {
        let mut c = cfg;
        // Authorise p2 so the provider check passes; the model check must then
        // fail (t1's tenant_models has no `solo`).
        c.tenant_providers
            .get_mut("t1")
            .unwrap()
            .insert("p2".to_string());
        c.sub_tenants
            .push(sub_tenant("st1", "t1", "team-a", "AAA_"));
        c
    };
    assert_eq!(
        validate_sub_tenant_write(&cfg, &route_write("t1", "st1", "p2", Some("solo"))),
        Err(SubTenantWriteError::ModelNotInTenant)
    );
}

/// Rule 2 — a route whose model is in the whitelist but NOT served by the
/// chosen provider is rejected.
#[test]
fn route_model_not_served_by_provider() {
    let cfg = base();
    // gpt-4o is whitelisted and served by p1, but p1 does NOT serve `solo`;
    // steer gpt-4o to p2 (allowed, served) is fine, so use a model p1 does not
    // serve. Authorise p1 only; gpt-4o is served by p1, so instead check a
    // model p1 does not serve that IS whitelisted — none exist, so widen the
    // whitelist and pick p1 for a model only p2 serves.
    let cfg = {
        let mut c = cfg;
        // Whitelist `solo` (served only by p2) but route it to p1 (authorised,
        // which does not serve it) → ModelNotServedByProvider.
        c.tenant_models
            .get_mut("t1")
            .unwrap()
            .insert("solo".to_string());
        c.sub_tenants
            .push(sub_tenant("st1", "t1", "team-a", "AAA_"));
        c
    };
    assert_eq!(
        validate_sub_tenant_write(&cfg, &route_write("t1", "st1", "p1", Some("solo"))),
        Err(SubTenantWriteError::ModelNotServedByProvider)
    );
}

// --- rule 3: prefix shape ----------------------------------------------------

/// Rule 3 — an empty `key_prefix` is rejected.
#[test]
fn prefix_empty() {
    let cfg = base();
    assert_eq!(
        validate_sub_tenant_write(&cfg, &st_write("t1", "team-a", "")),
        Err(SubTenantWriteError::PrefixEmpty)
    );
}

/// Rule 3 — a non-ASCII `key_prefix` is rejected.
#[test]
fn prefix_non_ascii() {
    let cfg = base();
    assert_eq!(
        validate_sub_tenant_write(&cfg, &st_write("t1", "team-a", "QÄCX_")),
        Err(SubTenantWriteError::PrefixNonAscii)
    );
}

/// Rule 3 — a `key_prefix` without a separator (`_` or `-`) is rejected: a bare
/// prefix would swallow longer, unrelated prefixes.
#[test]
fn prefix_no_separator() {
    let cfg = base();
    assert_eq!(
        validate_sub_tenant_write(&cfg, &st_write("t1", "team-a", "QQCX")),
        Err(SubTenantWriteError::PrefixNoSeparator)
    );
}

// --- rule 3: collisions (overlap / duplicate) --------------------------------

/// Rule 3 — the NEW prefix is a superstring of an existing same-tenant prefix
/// (`new.starts_with(existing)`).
#[test]
fn prefix_overlap_new_is_superstring() {
    let mut cfg = base();
    cfg.sub_tenants
        .push(sub_tenant("st1", "t1", "team-a", "QQCX_"));
    assert_eq!(
        validate_sub_tenant_write(&cfg, &st_write("t1", "team-b", "QQCX_WXYZ")),
        Err(SubTenantWriteError::PrefixOverlap)
    );
}

/// Rule 3 — the NEW prefix is a prefix of an existing same-tenant prefix
/// (`existing.starts_with(new)`).
#[test]
fn prefix_overlap_new_is_substring() {
    let mut cfg = base();
    cfg.sub_tenants
        .push(sub_tenant("st1", "t1", "team-a", "QQCX_WXYZ"));
    assert_eq!(
        validate_sub_tenant_write(&cfg, &st_write("t1", "team-b", "QQCX_")),
        Err(SubTenantWriteError::PrefixOverlap)
    );
}

/// Rule 3 — a prefix in a DIFFERENT tenant is allowed (uniqueness is per-tenant).
#[test]
fn prefix_same_across_tenants_allowed() {
    let mut cfg = base();
    // t2 is a separate tenant reusing the same prefix — no overlap within t1.
    let t2 = tenant("t2", "beta.com");
    cfg.tenants_by_domain.insert(t2.domain.clone(), t2);
    cfg.sub_tenants
        .push(sub_tenant("st1", "t1", "team-a", "QQCX_"));
    assert_eq!(
        validate_sub_tenant_write(&cfg, &st_write("t2", "team-x", "QQCX_")),
        Ok(())
    );
}

/// Rule 3 — an overlap with an ENABLED operator `key_prefix_binding` is
/// rejected.
#[test]
fn prefix_overlap_with_enabled_operator_binding() {
    let mut cfg = base();
    cfg.key_prefix_bindings
        .push(binding("b1", "sk_aaa_", "p1", true));
    assert_eq!(
        validate_sub_tenant_write(&cfg, &st_write("t1", "team-a", "sk_aaa_bbb")),
        Err(SubTenantWriteError::PrefixOverlap)
    );
}

/// Rule 3 — a DISABLED operator binding does not overlap (only enabled rows
/// steer traffic and only they are loaded).
#[test]
fn prefix_no_overlap_with_disabled_operator_binding() {
    let mut cfg = base();
    cfg.key_prefix_bindings
        .push(binding("b1", "sk_aaa_", "p1", false));
    assert_eq!(
        validate_sub_tenant_write(&cfg, &st_write("t1", "team-a", "sk_aaa_bbb")),
        Ok(())
    );
}

/// Rule 3 — an EXACT duplicate of an existing same-tenant prefix is a duplicate
/// (→ 409 by the admin handler), distinct from a plain overlap.
#[test]
fn prefix_duplicate() {
    let mut cfg = base();
    cfg.sub_tenants
        .push(sub_tenant("st1", "t1", "team-a", "QQCX_"));
    assert_eq!(
        validate_sub_tenant_write(&cfg, &st_write("t1", "team-b", "QQCX_")),
        Err(SubTenantWriteError::PrefixDuplicate)
    );
}

/// Rule 3 — an EXACT duplicate of an existing same-tenant name is a duplicate
/// (→ 409 by the admin handler).
#[test]
fn name_duplicate() {
    let mut cfg = base();
    cfg.sub_tenants
        .push(sub_tenant("st1", "t1", "team-a", "AAA_"));
    assert_eq!(
        validate_sub_tenant_write(&cfg, &st_write("t1", "team-a", "BBB_")),
        Err(SubTenantWriteError::NameDuplicate)
    );
}

/// Rule 3 — an UPDATE that keeps its own name/prefix is not self-rejected (the
/// row being updated is excluded from the collision scan).
#[test]
fn update_sub_tenant_keeps_own_prefix_and_name() {
    let mut cfg = base();
    cfg.sub_tenants
        .push(sub_tenant("st1", "t1", "team-a", "QQCX_"));
    assert_eq!(
        validate_sub_tenant_write(&cfg, &st_write_update("t1", "team-a", "QQCX_", "st1")),
        Ok(())
    );
}

// --- rule 4: quotas ----------------------------------------------------------

/// Rule 4 — at `MAX_SUB_TENANTS_PER_TENANT - 1` a create is accepted; at `MAX`
/// it is rejected (the boundary).
#[test]
fn sub_tenant_quota_boundary() {
    // At the boundary-minus-one: accepted.
    let mut at_minus_one = base();
    for i in 0..(MAX_SUB_TENANTS_PER_TENANT - 1) {
        at_minus_one.sub_tenants.push(sub_tenant(
            &format!("st{i}"),
            "t1",
            &format!("n{i}"),
            &format!("ST{i:02}_"),
        ));
    }
    assert_eq!(
        validate_sub_tenant_write(
            &at_minus_one,
            &st_write(
                "t1",
                "new",
                &format!("ST{:02}_", MAX_SUB_TENANTS_PER_TENANT - 1)
            )
        ),
        Ok(())
    );

    // At the boundary: rejected.
    let mut at_max = base();
    for i in 0..MAX_SUB_TENANTS_PER_TENANT {
        at_max.sub_tenants.push(sub_tenant(
            &format!("st{i}"),
            "t1",
            &format!("n{i}"),
            &format!("ST{i:02}_"),
        ));
    }
    assert_eq!(
        validate_sub_tenant_write(
            &at_max,
            &st_write(
                "t1",
                "new",
                &format!("ST{:02}_", MAX_SUB_TENANTS_PER_TENANT)
            )
        ),
        Err(SubTenantWriteError::SubTenantQuotaExceeded)
    );
}

/// Rule 4 — at `MAX_ROUTES_PER_SUB_TENANT - 1` a route create is accepted; at
/// `MAX` it is rejected (the boundary).
#[test]
fn route_quota_boundary() {
    let base = base();
    let base = {
        let mut c = base;
        c.sub_tenants
            .push(sub_tenant("st1", "t1", "team-a", "AAA_"));
        c
    };

    // At the boundary-minus-one: accepted.
    let mut at_minus_one = base.clone();
    for i in 0..(MAX_ROUTES_PER_SUB_TENANT - 1) {
        at_minus_one
            .sub_tenant_routes
            .push(sub_tenant_route(&format!("r{i}"), "st1", None, "p1"));
    }
    assert_eq!(
        validate_sub_tenant_write(&at_minus_one, &route_write("t1", "st1", "p1", None)),
        Ok(())
    );

    // At the boundary: rejected.
    let mut at_max = base.clone();
    for i in 0..MAX_ROUTES_PER_SUB_TENANT {
        at_max
            .sub_tenant_routes
            .push(sub_tenant_route(&format!("r{i}"), "st1", None, "p1"));
    }
    assert_eq!(
        validate_sub_tenant_write(&at_max, &route_write("t1", "st1", "p1", None)),
        Err(SubTenantWriteError::RouteQuotaExceeded)
    );
}

/// Rule 4, update at the boundary: a route UPDATE (`route_id = Some`) at
/// `MAX_ROUTES_PER_SUB_TENANT` is accepted — the row a natural-key upsert
/// updates must not count against its own quota. This is the idempotency
/// invariant A-2 理由 4 requires (a retried PUT after a 504 at the quota
/// boundary must converge instead of 400). A brand-new route at the cap still
/// fails.
#[test]
fn route_quota_boundary_update_excludes_self() {
    let cfg = {
        let mut c = base();
        c.sub_tenants
            .push(sub_tenant("st1", "t1", "team-a", "AAA_"));
        for i in 0..MAX_ROUTES_PER_SUB_TENANT {
            c.sub_tenant_routes
                .push(sub_tenant_route(&format!("r{i}"), "st1", None, "p1"));
        }
        c
    };

    // Updating the existing default route `r0` must pass at the cap.
    let mut update = route_write("t1", "st1", "p1", None);
    if let SubTenantWrite::Route { route_id, .. } = &mut update {
        *route_id = Some("r0".to_string());
    }
    assert_eq!(validate_sub_tenant_write(&cfg, &update), Ok(()));

    // A brand-new route at the cap is still rejected.
    assert_eq!(
        validate_sub_tenant_write(&cfg, &route_write("t1", "st1", "p1", None)),
        Err(SubTenantWriteError::RouteQuotaExceeded)
    );
}

// --- rule 1/2: default route (model_key = None) -----------------------------

/// A default route (`model_key = None`) needs no model check — only the
/// provider membership rules apply.
#[test]
fn default_route_skips_model_check() {
    let cfg = base();
    let cfg = {
        let mut c = cfg;
        c.sub_tenants
            .push(sub_tenant("st1", "t1", "team-a", "AAA_"));
        c
    };
    assert_eq!(
        validate_sub_tenant_write(&cfg, &route_write("t1", "st1", "p1", None)),
        Ok(())
    );
}

// --- D5: all-rows snapshot (v2 write core, A-2 prerequisites 1/2) -----------
//
// The transactional write core validates against the FULL set of DB rows
// (including disabled), not just the enabled `ConfigData` snapshot. These tests
// drive `validate_sub_tenant_write_against` with a caller-supplied
// `SubTenantRows` to pin that behaviour.

/// A `SubTenantRows` snapshot with the given sub-tenants and no routes.
fn rows_with(sub_tenants: Vec<SubTenant>) -> SubTenantRows<'static> {
    // Leak the vec so the borrows have a 'static lifetime for the test.
    let st = Box::leak(sub_tenants.into_boxed_slice());
    SubTenantRows {
        sub_tenants: st,
        routes: &[],
    }
}

/// A-2 finding 1 — the quota counts **all** rows (including disabled). Here the
/// tenant has `MAX` sub-tenants, ALL disabled (so none are in `cfg.sub_tenants`,
/// the enabled-only snapshot). A create must still be rejected by the quota.
#[test]
fn quota_counts_disabled_rows() {
    let cfg = base();
    let mut all = Vec::new();
    for i in 0..MAX_SUB_TENANTS_PER_TENANT {
        let mut st = sub_tenant(
            &format!("st{i}"),
            "t1",
            &format!("n{i}"),
            &format!("ST{i:02}_"),
        );
        st.enabled = false;
        all.push(st);
    }
    let rows = rows_with(all);
    assert_eq!(
        validate_sub_tenant_write_against(&cfg, &rows, &st_write("t1", "new", "ST99_")),
        Err(SubTenantWriteError::SubTenantQuotaExceeded)
    );
}

/// A-2 finding 2 — overlap is detected against a **disabled** row (all-rows
/// check). If the validator only saw the enabled `cfg.sub_tenants`, the disabled
/// row's prefix would be invisible and this create would wrongly pass.
#[test]
fn overlap_detected_against_disabled_row() {
    let cfg = base();
    let mut disabled = sub_tenant("st1", "t1", "team-a", "QQCX_");
    disabled.enabled = false;
    let rows = rows_with(vec![disabled]);
    assert_eq!(
        validate_sub_tenant_write_against(&cfg, &rows, &st_write("t1", "team-b", "QQCX_WXYZ")),
        Err(SubTenantWriteError::PrefixOverlap)
    );
}

/// Self-exclusion on update still works through the all-rows API: an update that
/// keeps its own name/prefix is not self-rejected even when the row is in the
/// supplied snapshot.
#[test]
fn self_exclusion_on_update_still_works() {
    let cfg = base();
    let existing = sub_tenant("st1", "t1", "team-a", "QQCX_");
    let rows = rows_with(vec![existing]);
    assert_eq!(
        validate_sub_tenant_write_against(
            &cfg,
            &rows,
            &st_write_update("t1", "team-a", "QQCX_", "st1")
        ),
        Ok(())
    );
}

// --- D9: name charset rule ---------------------------------------------------

/// D9 — a `name` containing `/` is rejected (it would break v2 URL-keyed
/// `PUT /sub-tenants/{name}` addressing).
#[test]
fn sub_tenant_name_with_slash_is_rejected() {
    let cfg = base();
    assert_eq!(
        validate_sub_tenant_write(&cfg, &st_write("t1", "team/a", "AAA_")),
        Err(SubTenantWriteError::NameInvalid)
    );
}

/// D9 — an empty `name` is rejected.
#[test]
fn name_empty_rejected() {
    let cfg = base();
    assert_eq!(
        validate_sub_tenant_write(&cfg, &st_write("t1", "", "AAA_")),
        Err(SubTenantWriteError::NameInvalid)
    );
}

/// D9 — a `name` with a control character is rejected.
#[test]
fn name_control_char_rejected() {
    let cfg = base();
    assert_eq!(
        validate_sub_tenant_write(&cfg, &st_write("t1", "team\n-a", "AAA_")),
        Err(SubTenantWriteError::NameInvalid)
    );
}

/// D9 — a non-ASCII `name` is rejected.
#[test]
fn name_non_ascii_rejected() {
    let cfg = base();
    assert_eq!(
        validate_sub_tenant_write(&cfg, &st_write("t1", "team-ä", "AAA_")),
        Err(SubTenantWriteError::NameInvalid)
    );
}

/// D9 — the length boundary: a `name` of exactly `MAX_SUB_TENANT_NAME_LEN` bytes
/// is accepted, one byte over is rejected.
#[test]
fn name_length_boundary() {
    let cfg = base();
    let at_max = "a".repeat(MAX_SUB_TENANT_NAME_LEN);
    assert_eq!(
        validate_sub_tenant_write(&cfg, &st_write("t1", &at_max, "AAA_")),
        Ok(())
    );
    let over = "a".repeat(MAX_SUB_TENANT_NAME_LEN + 1);
    assert_eq!(
        validate_sub_tenant_write(&cfg, &st_write("t1", &over, "AAA_")),
        Err(SubTenantWriteError::NameInvalid)
    );
}

/// D9 — a well-formed printable-ASCII `name` (no `/`, no control, short) is
/// accepted.
#[test]
fn name_valid_printable_ok() {
    let cfg = base();
    assert_eq!(
        validate_sub_tenant_write(&cfg, &st_write("t1", "team-a-1", "AAA_")),
        Ok(())
    );
}
