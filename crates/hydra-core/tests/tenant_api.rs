//! Tenant API pure decisions (`crates/hydra-core/src/tenant_api.rs`).
//!
//! Every assertion here is about a *silent* failure mode: a wrong comparison
//! that still returns a number, a decoder that reports zero usage instead of
//! erroring, a route matcher that accepts a near miss and hands a tenant token
//! to the proxy pipeline. None of them throw; all of them produce a
//! syntactically valid, semantically wrong answer.

use hydra_core::tenant_api::{
    canonical_le, decode_usage_json_each_row, decode_usage_rows_json_each_row,
    is_canonical_timestamp, normalize_as_of, parse_lenient_u64, parse_route, parse_write_route,
    Endpoint, TenantApiRoute, TenantWriteRoute, UsageRow, UsageTotals,
};
use serde_json::json;

// ---------------------------------------------------------------------------
// canonical timestamp form
// ---------------------------------------------------------------------------

#[test]
fn canonical_form_is_accepted() {
    for ok in [
        "2026-09-16T00:00:00Z",
        "2026-01-01T00:00:00Z",
        "2026-12-31T23:59:59Z",
        "2000-02-29T12:34:56Z",
    ] {
        assert!(is_canonical_timestamp(ok), "{ok:?} must be accepted");
    }
}

#[test]
fn canonical_form_rejects_other_shapes() {
    // The space-separated form is what SQLite's `datetime('now')` produces, and
    // comparing against it silently selects a whole day (see `canonical_le`).
    // A bare date is a safe prefix, but it is not the canonical form.
    for bad in [
        "2026-09-16 00:00:00",
        "2026-09-16T00:00:00",
        "2026-09-16T00:00:00+08:00",
        "2026-09-16T00:00:00.123Z",
        "2026-09-16T00:00:00z",
        "2026-9-16T00:00:00Z",
        "2026-09-16",
        "2026-09-16T00:00:0Z",
        "2026-09-16T00:00:00ZZ",
        "",
        "not-a-timestamp",
    ] {
        assert!(!is_canonical_timestamp(bad), "{bad:?} must be rejected");
    }
}

#[test]
fn canonical_form_rejects_out_of_range_fields() {
    for bad in [
        "2026-00-16T00:00:00Z", // month 0
        "2026-13-16T00:00:00Z", // month 13
        "2026-09-00T00:00:00Z", // day 0
        "2026-09-32T00:00:00Z", // day 32
        "2026-09-16T24:00:00Z", // hour 24
        "2026-09-16T00:60:00Z", // minute 60
        "2026-09-16T00:00:60Z", // second 60
    ] {
        assert!(
            !is_canonical_timestamp(bad),
            "{bad:?} must be rejected by the range check"
        );
    }
}

#[test]
fn ordering_on_the_canonical_form_equals_chronological_ordering() {
    assert!(canonical_le("2026-09-16T00:00:00Z", "2026-09-16T00:00:01Z"));
    assert!(canonical_le("2026-09-16T23:59:59Z", "2026-09-17T00:00:00Z"));
    assert!(canonical_le("2025-12-31T23:59:59Z", "2026-01-01T00:00:00Z"));
    assert!(canonical_le("2026-09-16T00:00:00Z", "2026-09-16T00:00:00Z"));
    assert!(!canonical_le(
        "2026-09-17T00:00:00Z",
        "2026-09-16T23:59:59Z"
    ));
}

#[test]
fn space_separated_input_compares_greater_than_the_t_form() {
    // THE regression this documents: 'T' (0x54) > ' ' (0x20), so a
    // space-separated bound is "greater than" every T-form value on the same
    // date. A naive `created_at >= '2026-09-16 23:59:59'` therefore matches rows
    // from 00:00:00 onward — silently adding a whole day to the window.
    assert!(!canonical_le("2026-09-16T00:00:00Z", "2026-09-16 23:59:59"));
    assert!(canonical_le("2026-09-16 23:59:59", "2026-09-16T00:00:00Z"));
}

// ---------------------------------------------------------------------------
// ClickHouse lenient integers (the "silent zero" trap)
// ---------------------------------------------------------------------------

#[test]
fn quoted_and_plain_int64_are_both_accepted() {
    // Measured on ClickHouse 24.3: `SELECT toUInt64(12345) FORMAT JSONEachRow`
    // returns {"n":"12345"} — 64-bit ints are JSON-quoted by default.
    assert_eq!(parse_lenient_u64(&json!("12345")), Some(12345));
    assert_eq!(parse_lenient_u64(&json!(12345)), Some(12345));
    assert_eq!(parse_lenient_u64(&json!("0")), Some(0));
    assert_eq!(parse_lenient_u64(&json!(0)), Some(0));
    assert_eq!(
        parse_lenient_u64(&json!("18446744073709551615")),
        Some(u64::MAX)
    );
}

#[test]
fn unparseable_numbers_are_none_never_zero() {
    assert_eq!(parse_lenient_u64(&json!("")), None);
    assert_eq!(parse_lenient_u64(&json!("   ")), None);
    assert_eq!(parse_lenient_u64(&json!("abc")), None);
    assert_eq!(parse_lenient_u64(&json!("-1")), None);
    assert_eq!(parse_lenient_u64(&json!(-1)), None);
    assert_eq!(parse_lenient_u64(&json!(-1.0)), None);
    assert_eq!(parse_lenient_u64(&json!(1.5)), None);
    assert_eq!(parse_lenient_u64(&json!(null)), None);
    assert_eq!(parse_lenient_u64(&json!(true)), None);
    assert_eq!(parse_lenient_u64(&json!([1])), None);
    assert_eq!(parse_lenient_u64(&json!({"n": 1})), None);
    assert_eq!(parse_lenient_u64(&json!("99999999999999999999999")), None);
}

// ---------------------------------------------------------------------------
// as_of normalisation
// ---------------------------------------------------------------------------

#[test]
fn as_of_maps_empty_string_and_absent_to_none() {
    // SQLite: NULL. ClickHouse on an empty set: "" (measured). Both are "no
    // records", so both must be None — an empty as_of would look like a value.
    assert_eq!(normalize_as_of(None), None);
    assert_eq!(normalize_as_of(Some("")), None);
    assert_eq!(
        normalize_as_of(Some("2026-09-15T07:32:06Z")),
        Some("2026-09-15T07:32:06Z".to_string())
    );
}

// ---------------------------------------------------------------------------
// route parsing
// ---------------------------------------------------------------------------

#[test]
fn the_three_endpoints_parse() {
    assert_eq!(
        parse_route("/tenant/t1/api/v1/whoami"),
        Some(TenantApiRoute {
            endpoint: Endpoint::Whoami,
            tenant_id: "t1",
            user_id: None
        })
    );
    assert_eq!(
        parse_route("/tenant/t1/api/v1/auth/cache/invalidate"),
        Some(TenantApiRoute {
            endpoint: Endpoint::InvalidateAuthCache,
            tenant_id: "t1",
            user_id: None
        })
    );
    assert_eq!(
        parse_route("/tenant/t1/api/v1/usage"),
        Some(TenantApiRoute {
            endpoint: Endpoint::Usage,
            tenant_id: "t1",
            user_id: None
        })
    );
}

/// T7.1: the two read-only sub-tenant endpoints parse. They are FLAT paths
/// (`api/v1/sub-tenants`, `api/v1/sub-tenant-routes`), matching the admin
/// resource shape, so `parse_route` keeps returning the same
/// `(tenant_id, endpoint)` pair — no new `TenantApiRoute` shape.
#[test]
fn the_sub_tenant_endpoints_parse() {
    assert_eq!(
        parse_route("/tenant/t1/api/v1/sub-tenants"),
        Some(TenantApiRoute {
            endpoint: Endpoint::ListSubTenants,
            tenant_id: "t1",
            user_id: None
        })
    );
    assert_eq!(
        parse_route("/tenant/t1/api/v1/sub-tenant-routes"),
        Some(TenantApiRoute {
            endpoint: Endpoint::ListSubTenantRoutes,
            tenant_id: "t1",
            user_id: None
        })
    );
}

#[test]
fn near_misses_do_not_parse() {
    for bad in [
        "/tenant//api/v1/whoami",               // empty tenant id
        "/tenant/a/b/api/v1/whoami",            // tenant id contains '/'
        "/tenant/t1/api/v1",                    // no endpoint
        "/tenant/t1/api/v1/usage/",             // trailing slash
        "/tenant/t1/api/v1/whoami/extra",       // extra segment
        "/tenant/t1/api/v2/whoami",             // wrong version
        "/tenant/t1/api/v1/models",             // not one of ours
        "/tenant/t1/api/v1/typo",               // typo
        "/tenant/t1/api/v1/Whoami",             // case matters
        "/tenant/t1/api/v1/sub-tenant",         // one char short of `sub-tenants`
        "/tenant/t1/api/v1/sub-tenants/",       // trailing slash
        "/tenant/t1/api/v1/sub-tenants/1",      // an item id is not a route
        "/tenant/t1/api/v1/sub-tenant-route",   // one char short of `sub-tenant-routes`
        "/tenant/t1/api/v1/sub-tenant-routes/", // trailing slash
        "/tenantapi/v1/whoami",                 // prefix glued
        "/tenant/",                             // nothing after the prefix
        "/tenant",                              // no prefix slash
        "/v1/chat/completions",                 // ordinary data-plane path
        "",                                     // empty
        "/",                                    // root
    ] {
        assert!(parse_route(bad).is_none(), "{bad:?} must not parse");
    }
}

// ---------------------------------------------------------------------------
// method-aware write-route parsing (sub-tenant v2, D9)
// ---------------------------------------------------------------------------

/// The four write endpoints parse to their (method, path)-determined routes.
/// `PUT /sub-tenants/{name}` and `DELETE /sub-tenants/{id}` share a path shape
/// but are distinguished by method (name vs. immutable id); the flat
/// `PUT /sub-tenant-routes` is a write, not the GET list endpoint.
#[test]
fn the_write_endpoints_parse() {
    assert_eq!(
        parse_write_route("PUT", "/tenant/t1/api/v1/sub-tenants/team-a"),
        Some(TenantWriteRoute::UpsertSubTenant {
            tenant_id: "t1",
            name: "team-a"
        })
    );
    assert_eq!(
        parse_write_route("DELETE", "/tenant/t1/api/v1/sub-tenants/id-123"),
        Some(TenantWriteRoute::DeleteSubTenant {
            tenant_id: "t1",
            id: "id-123"
        })
    );
    assert_eq!(
        parse_write_route("PUT", "/tenant/t1/api/v1/sub-tenant-routes"),
        Some(TenantWriteRoute::UpsertRoute { tenant_id: "t1" })
    );
    assert_eq!(
        parse_write_route("DELETE", "/tenant/t1/api/v1/sub-tenant-routes/r-9"),
        Some(TenantWriteRoute::DeleteRoute {
            tenant_id: "t1",
            id: "r-9"
        })
    );
}

/// The same path shape resolves to a different route under a different method
/// (D9's reason for a method-aware parse): `.../sub-tenants/x` is an upsert-by-
/// name under PUT and a delete-by-id under DELETE.
#[test]
fn the_same_path_shape_differs_by_method() {
    assert!(matches!(
        parse_write_route("PUT", "/tenant/t1/api/v1/sub-tenants/x"),
        Some(TenantWriteRoute::UpsertSubTenant { .. })
    ));
    assert!(matches!(
        parse_write_route("DELETE", "/tenant/t1/api/v1/sub-tenants/x"),
        Some(TenantWriteRoute::DeleteSubTenant { .. })
    ));
    // A method the endpoint does not define is a miss, not a guess.
    assert!(parse_write_route("GET", "/tenant/t1/api/v1/sub-tenants/x").is_none());
    assert!(parse_write_route("PATCH", "/tenant/t1/api/v1/sub-tenants/x").is_none());
    assert!(parse_write_route("POST", "/tenant/t1/api/v1/sub-tenant-routes").is_none());
}

/// Every write-route near-miss is `None` (a local 404, never a fall-through):
/// wrong method, missing/extra segment, wrong version, empty tenant id, a `/`
/// in the segment, or the flat read path under a write method.
#[test]
fn write_route_near_misses_do_not_parse() {
    for (method, bad) in [
        // Missing the trailing segment.
        ("PUT", "/tenant/t1/api/v1/sub-tenants"),
        ("DELETE", "/tenant/t1/api/v1/sub-tenants"),
        ("DELETE", "/tenant/t1/api/v1/sub-tenant-routes"),
        // Extra segments (a `/` in the segment / a deeper path).
        ("PUT", "/tenant/t1/api/v1/sub-tenants/a/b"),
        ("DELETE", "/tenant/t1/api/v1/sub-tenant-routes/a/b"),
        // Trailing slash leaves an empty segment.
        ("PUT", "/tenant/t1/api/v1/sub-tenants/"),
        ("DELETE", "/tenant/t1/api/v1/sub-tenant-routes/"),
        // Wrong method for a read (flat) path.
        ("GET", "/tenant/t1/api/v1/sub-tenant-routes"),
        ("POST", "/tenant/t1/api/v1/sub-tenant-routes"),
        // Wrong version / wrong resource.
        ("PUT", "/tenant/t1/api/v2/sub-tenant-routes"),
        ("PUT", "/tenant/t1/api/v1/sub-tenant"),
        ("DELETE", "/tenant/t1/api/v1/sub-tenant-rout"),
        // Empty tenant id.
        ("PUT", "/tenant//api/v1/sub-tenant-routes"),
        // Prefix glued.
        ("PUT", "/tenantapi/v1/sub-tenant-routes"),
        // A name that is just a slash.
        ("PUT", "/tenant/t1/api/v1/sub-tenants//"),
    ] {
        assert!(
            parse_write_route(method, bad).is_none(),
            "{method} {bad:?} must not parse"
        );
    }
}

/// The `{name}` / `{id}` segment is taken raw (no percent-decoding): a literal
/// `%2F` in the segment is a legal single segment for the parser (the write core
/// / D9 charset rule is what rejects an invalid `name`, not the route parse).
#[test]
fn the_write_route_segment_is_read_raw() {
    assert_eq!(
        parse_write_route("PUT", "/tenant/t1/api/v1/sub-tenants/a%2Fb"),
        Some(TenantWriteRoute::UpsertSubTenant {
            tenant_id: "t1",
            name: "a%2Fb"
        })
    );
}

// ---------------------------------------------------------------------------
// user-hierarchy READ routes (T5 flat lists + T6 per-user api-key list)
// ---------------------------------------------------------------------------

/// The two flat user-hierarchy lists parse like the sub-tenant lists: the item id
/// is NEVER part of a READ route, so `.../user-groups/{id}` and `.../users/{id}`
/// belong to the DELETE family below, not here.
#[test]
fn the_user_hierarchy_reads_parse() {
    assert_eq!(
        parse_route("/tenant/t1/api/v1/user-groups"),
        Some(TenantApiRoute {
            endpoint: Endpoint::ListUserGroups,
            tenant_id: "t1",
            user_id: None
        })
    );
    assert_eq!(
        parse_route("/tenant/t1/api/v1/users"),
        Some(TenantApiRoute {
            endpoint: Endpoint::ListUsers,
            tenant_id: "t1",
            user_id: None
        })
    );
}

/// The ONE read route that carries an item id (T6): the per-user api-key list
/// puts `{uid}` on [`TenantApiRoute.user_id`] rather than inventing a second
/// endpoint per user, so one label (`user-api-keys`) covers an unbounded number
/// of users in the metric.
#[test]
fn the_per_user_api_key_read_carries_the_user_id() {
    assert_eq!(
        parse_route("/tenant/t1/api/v1/users/u_9/api-keys"),
        Some(TenantApiRoute {
            endpoint: Endpoint::ListUserApiKeys,
            tenant_id: "t1",
            user_id: Some("u_9")
        })
    );
    // Every OTHER read keeps the field None, so a reader cannot mistake a flat
    // route for a per-user one.
    assert_eq!(
        parse_route("/tenant/t1/api/v1/users")
            .expect("flat list")
            .user_id,
        None
    );
}

// ---------------------------------------------------------------------------
// api-key + billing WRITE routes (T6)
// ---------------------------------------------------------------------------

/// The three self-service identity writes: `POST .../users/{uid}/api-keys` mints,
/// `DELETE .../users/{uid}/api-keys/{kid}` revokes, `PUT .../users/{uid}/billing`
/// sets the欠费 dimension. The target user is a PATH segment on all three —
/// unlike the flat `PUT api/v1/users` upsert, whose row id is in the BODY.
#[test]
fn the_api_key_and_billing_writes_parse() {
    assert_eq!(
        parse_write_route("POST", "/tenant/t1/api/v1/users/u_9/api-keys"),
        Some(TenantWriteRoute::CreateUserApiKey {
            tenant_id: "t1",
            user_id: "u_9"
        })
    );
    assert_eq!(
        parse_write_route("DELETE", "/tenant/t1/api/v1/users/u_9/api-keys/k_7"),
        Some(TenantWriteRoute::DeleteUserApiKey {
            tenant_id: "t1",
            user_id: "u_9",
            key_id: "k_7"
        })
    );
    assert_eq!(
        parse_write_route("PUT", "/tenant/t1/api/v1/users/u_9/billing"),
        Some(TenantWriteRoute::SetUserBilling {
            tenant_id: "t1",
            user_id: "u_9"
        })
    );
}

/// The pairing on the DELETE is what decides the shape: `.../api-keys` with no
/// `{kid}` is NOT a "delete every key of this user" shortcut, and it is NOT a
/// `DeleteUser` of a user named `u_9/api-keys` either — both readings are
/// refused, and the caller gets a local 404.
#[test]
fn the_key_delete_needs_both_ids() {
    assert!(
        parse_write_route("DELETE", "/tenant/t1/api/v1/users/u_9/api-keys").is_none(),
        "no kid"
    );
    assert!(
        parse_write_route("DELETE", "/tenant/t1/api/v1/users/u_9/api-keys/").is_none(),
        "empty kid"
    );
    assert!(
        parse_write_route("DELETE", "/tenant/t1/api/v1/users/u_9/api-keys/k/1").is_none(),
        "nested kid"
    );
    assert!(
        parse_write_route("DELETE", "/tenant/t1/api/v1/users//api-keys/k_7").is_none(),
        "empty uid"
    );
    // The key DELETE is tried BEFORE the user DELETE, and the user DELETE still
    // works for its own shape.
    assert_eq!(
        parse_write_route("DELETE", "/tenant/t1/api/v1/users/u_9"),
        Some(TenantWriteRoute::DeleteUser {
            tenant_id: "t1",
            id: "u_9"
        })
    );
    assert_eq!(
        parse_write_route("DELETE", "/tenant/t1/api/v1/user-groups/g_3"),
        Some(TenantWriteRoute::DeleteUserGroup {
            tenant_id: "t1",
            id: "g_3"
        })
    );
    assert_eq!(
        parse_write_route("PUT", "/tenant/t1/api/v1/user-groups"),
        Some(TenantWriteRoute::UpsertUserGroup { tenant_id: "t1" })
    );
    assert_eq!(
        parse_write_route("PUT", "/tenant/t1/api/v1/users"),
        Some(TenantWriteRoute::UpsertUser { tenant_id: "t1" })
    );
}

/// Near misses on the identity paths are `None` (a local 404, never a
/// fall-through to the proxy pipeline, where a tenant token would be read as a
/// client api-key).
#[test]
fn identity_path_near_misses_do_not_parse() {
    for (method, bad) in [
        // A READ under a write method (the create is POST, not GET).
        ("GET", "/tenant/t1/api/v1/users/u_9/api-keys"),
        // `POST api/v1/users` mints nothing: the key list is per-user.
        ("POST", "/tenant/t1/api/v1/users"),
        ("POST", "/tenant/t1/api/v1/users/u_9/api-keys/"),
        ("POST", "/tenant/t1/api/v1/users/u_9/api-keys/k_7"),
        ("POST", "/tenant/t1/api/v1/users/u_9/billing"),
        // The billing write is a PUT, and a trailing/extra segment is not billing.
        ("PUT", "/tenant/t1/api/v1/users/u_9/api-keys"),
        ("PUT", "/tenant/t1/api/v1/users/u_9/billing/"),
        ("PUT", "/tenant/t1/api/v1/users/u_9/billing/extra"),
        ("PUT", "/tenant/t1/api/v1/users/u_9/billingx"),
        ("PUT", "/tenant/t1/api/v1/users/u_9/x/billing"),
        // A user id with a `/` in it is two segments, not one.
        ("PUT", "/tenant/t1/api/v1/users/a/b/billing"),
        // Wrong version / wrong resource name.
        ("POST", "/tenant/t1/api/v2/users/u_9/api-keys"),
        ("POST", "/tenant/t1/api/v1/user/u_9/api-keys"),
        ("DELETE", "/tenant/t1/api/v1/users/u_9/api-key/k_7"),
        // Empty tenant id.
        ("POST", "/tenant//api/v1/users/u_9/api-keys"),
    ] {
        assert!(
            parse_write_route(method, bad).is_none(),
            "{method} {bad:?} must not parse"
        );
    }
    for bad in [
        "/tenant/t1/api/v1/users/u_9/api-keys/",
        "/tenant/t1/api/v1/users/u_9/api-keys/k_7",
        "/tenant/t1/api/v1/users/u_9/billing",
        "/tenant/t1/api/v1/users//api-keys",
        "/tenant/t1/api/v1/users/u_9/api-keys/extra/more",
        "/tenant/t1/api/v1/user-groups/g_3",
    ] {
        assert!(
            parse_route(bad).is_none(),
            "{bad:?} must not parse as a read"
        );
    }
}

/// The metric label of EVERY write variant, pinned. These strings ARE the
/// `hydra_tenant_api_requests_total` label set, so a rename is a dashboard break
/// and belongs in a change that says so.
#[test]
fn write_route_labels_are_pinned() {
    let cases: &[(&'static str, TenantWriteRoute<'static>)] = &[
        (
            "sub-tenant-upsert",
            TenantWriteRoute::UpsertSubTenant {
                tenant_id: "t",
                name: "n",
            },
        ),
        (
            "sub-tenant-delete",
            TenantWriteRoute::DeleteSubTenant {
                tenant_id: "t",
                id: "i",
            },
        ),
        (
            "sub-tenant-route-upsert",
            TenantWriteRoute::UpsertRoute { tenant_id: "t" },
        ),
        (
            "sub-tenant-route-delete",
            TenantWriteRoute::DeleteRoute {
                tenant_id: "t",
                id: "i",
            },
        ),
        (
            "user-group-upsert",
            TenantWriteRoute::UpsertUserGroup { tenant_id: "t" },
        ),
        (
            "user-group-delete",
            TenantWriteRoute::DeleteUserGroup {
                tenant_id: "t",
                id: "i",
            },
        ),
        (
            "user-upsert",
            TenantWriteRoute::UpsertUser { tenant_id: "t" },
        ),
        (
            "user-delete",
            TenantWriteRoute::DeleteUser {
                tenant_id: "t",
                id: "i",
            },
        ),
        (
            "user-api-key-create",
            TenantWriteRoute::CreateUserApiKey {
                tenant_id: "t",
                user_id: "u",
            },
        ),
        (
            "user-api-key-delete",
            TenantWriteRoute::DeleteUserApiKey {
                tenant_id: "t",
                user_id: "u",
                key_id: "k",
            },
        ),
        (
            "user-billing-set",
            TenantWriteRoute::SetUserBilling {
                tenant_id: "t",
                user_id: "u",
            },
        ),
    ];
    for (expected, route) in cases.iter().copied() {
        assert_eq!(route.label(), expected, "label of {route:?}");
    }
}

/// `tenant_id()` is what the dispatcher cross-checks the URL against the
/// credential with, BEFORE the gate. A variant that failed to yield its tenant
/// would silently drop that check, so EVERY variant is covered here.
#[test]
fn every_write_route_names_its_tenant() {
    let routes: [TenantWriteRoute<'static>; 11] = [
        TenantWriteRoute::UpsertSubTenant {
            tenant_id: "t_1",
            name: "n",
        },
        TenantWriteRoute::DeleteSubTenant {
            tenant_id: "t_1",
            id: "i",
        },
        TenantWriteRoute::UpsertRoute { tenant_id: "t_1" },
        TenantWriteRoute::DeleteRoute {
            tenant_id: "t_1",
            id: "i",
        },
        TenantWriteRoute::UpsertUserGroup { tenant_id: "t_1" },
        TenantWriteRoute::DeleteUserGroup {
            tenant_id: "t_1",
            id: "i",
        },
        TenantWriteRoute::UpsertUser { tenant_id: "t_1" },
        TenantWriteRoute::DeleteUser {
            tenant_id: "t_1",
            id: "i",
        },
        TenantWriteRoute::CreateUserApiKey {
            tenant_id: "t_1",
            user_id: "u",
        },
        TenantWriteRoute::DeleteUserApiKey {
            tenant_id: "t_1",
            user_id: "u",
            key_id: "k",
        },
        TenantWriteRoute::SetUserBilling {
            tenant_id: "t_1",
            user_id: "u",
        },
    ];
    for route in routes.iter() {
        assert_eq!(route.tenant_id(), "t_1", "tenant_id() of {route:?}");
    }
}

// ---------------------------------------------------------------------------
// JSONEachRow decoding (T14a / T14b, the blocking negatives)
// ---------------------------------------------------------------------------

/// Exactly what the live instance returned for the §4.3.1 aggregate query.
const LIVE_AGGREGATE: &str = r#"{"requests":"8","tokens_in":"247","tokens_out":"18","cache_hit_tokens":"0","errors":"0","last_seen":"2026-09-15T07:32:06Z"}"#;

#[test]
fn decodes_the_measured_live_shape() {
    let agg = decode_usage_json_each_row(LIVE_AGGREGATE).expect("the live shape must decode");
    assert_eq!(
        agg.totals,
        UsageTotals {
            requests: 8,
            tokens_in: 247,
            tokens_out: 18,
            cache_hit_tokens: 0,
            errors: 0,
        }
    );
    assert_eq!(agg.as_of.as_deref(), Some("2026-09-15T07:32:06Z"));
    assert!(agg.rows.is_empty(), "no group_by -> no rows");
}

#[test]
fn quoted_and_unquoted_bodies_decode_identically() {
    let plain = r#"{"requests":8,"tokens_in":247,"tokens_out":18,"cache_hit_tokens":0,"errors":0,"last_seen":"2026-09-15T07:32:06Z"}"#;
    let a = decode_usage_json_each_row(LIVE_AGGREGATE).expect("quoted");
    let b = decode_usage_json_each_row(plain).expect("plain");
    assert_eq!(a, b, "the quoting default must not change the answer");
}

#[test]
fn a_broken_field_errors_rather_than_reporting_zero() {
    for bad in [
        r#"{"requests":"abc","tokens_in":"1","tokens_out":"1","cache_hit_tokens":"0","errors":"0","last_seen":""}"#,
        r#"{"requests":-1,"tokens_in":"1","tokens_out":"1","cache_hit_tokens":"0","errors":"0","last_seen":""}"#,
        r#"{"tokens_in":"1","tokens_out":"1","cache_hit_tokens":"0","errors":"0","last_seen":""}"#,
        r#"{}"#,
        r#"{"requests":"1","tokens_in":"1","tokens_out":"1","cache_hit_tokens":"0","errors":"0"}"#,
    ] {
        let r = decode_usage_json_each_row(bad);
        assert!(r.is_err(), "{bad} must be an error, got {r:?}");
    }
}

#[test]
fn syntax_errors_and_empty_bodies_are_malformed() {
    for bad in ["", "   ", "\n\n", "not json", "[1,2]", "{\"a\":1}\n[2]"] {
        let r = decode_usage_json_each_row(bad);
        assert!(r.is_err(), "{bad:?} must be an error, got {r:?}");
    }
}

#[test]
fn empty_result_set_yields_zero_totals_and_no_as_of() {
    // Measured: an empty set gives {"requests":"0",...,"last_seen":""}.
    let body = r#"{"requests":"0","tokens_in":"0","tokens_out":"0","cache_hit_tokens":"0","errors":"0","last_seen":""}"#;
    let agg = decode_usage_json_each_row(body).expect("empty set is a valid answer");
    assert_eq!(agg.totals, UsageTotals::default());
    assert_eq!(agg.as_of, None, "CH returns \"\", not null, for no rows");
}

#[test]
fn group_rows_are_parsed_and_totals_come_from_the_last_line() {
    // `GROUP BY` returns the group rows first and the overall totals last.
    let body = concat!(
        r#"{"key":"Qwen/Qwen2.5-7B-Instruct","requests":"8","tokens_in":"247","tokens_out":"18","cache_hit_tokens":"0","errors":"0","last_seen":"2026-09-15T07:32:06Z"}"#,
        "\n",
        r#"{"key":"gpt-4o","requests":"2","tokens_in":"10","tokens_out":"3","cache_hit_tokens":"0","errors":"1","last_seen":"2026-09-15T07:32:06Z"}"#,
        "\n",
        r#"{"requests":"10","tokens_in":"257","tokens_out":"21","cache_hit_tokens":"0","errors":"1","last_seen":"2026-09-15T07:32:06Z"}"#,
    );
    let agg = decode_usage_json_each_row(body).expect("grouped body decodes");
    assert_eq!(agg.totals.requests, 10, "totals come from the LAST line");
    assert_eq!(agg.rows.len(), 2);
    assert_eq!(
        agg.rows[0],
        UsageRow {
            key: "Qwen/Qwen2.5-7B-Instruct".to_string(),
            totals: UsageTotals {
                requests: 8,
                tokens_in: 247,
                tokens_out: 18,
                cache_hit_tokens: 0,
                errors: 0,
            },
        }
    );
    assert_eq!(agg.rows[1].key, "gpt-4o");
    assert_eq!(agg.rows[1].totals.errors, 1);
}

#[test]
fn a_group_row_without_a_key_is_an_error() {
    let body = concat!(
        r#"{"requests":"1","tokens_in":"1","tokens_out":"1","cache_hit_tokens":"0","errors":"0","last_seen":""}"#,
        "\n",
        r#"{"requests":"1","tokens_in":"1","tokens_out":"1","cache_hit_tokens":"0","errors":"0","last_seen":""}"#,
    );
    assert!(decode_usage_json_each_row(body).is_err());
}

// ---------------------------------------------------------------------------
// The grouped (rows-only) body — measured ClickHouse shapes
// ---------------------------------------------------------------------------

/// The grouped query's body is **rows only** and has no `last_seen` column: the
/// reader issues totals and groups as two separate queries, because a single
/// `UNION ALL` of them does not preserve branch order (measured: totals-first in
/// 1 of 6 runs on ClickHouse 24.3, and `ORDER BY is_total` did not fix it).
#[test]
fn grouped_rows_decode_without_a_last_seen_column() {
    let body = concat!(
        r#"{"key":"Qwen/Qwen2.5-7B-Instruct","requests":"8","tokens_in":"247","tokens_out":"18","cache_hit_tokens":"0","errors":"0"}"#,
        "\n",
        r#"{"key":"gpt-4o","requests":"2","tokens_in":"10","tokens_out":"3","cache_hit_tokens":"5","errors":"1"}"#,
    );
    let rows = decode_usage_rows_json_each_row(body).expect("grouped body decodes");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].key, "Qwen/Qwen2.5-7B-Instruct");
    assert_eq!(rows[0].totals.requests, 8);
    assert_eq!(rows[1].key, "gpt-4o");
    assert_eq!(rows[1].totals.cache_hit_tokens, 5);
    assert_eq!(rows[1].totals.errors, 1);
}

/// ClickHouse answers the grouped query with an **empty body** when nothing
/// matches, so "no rows" must be `Ok(vec![])` — not an error, and not a row.
#[test]
fn an_empty_grouped_body_is_no_rows_rather_than_an_error() {
    assert!(decode_usage_rows_json_each_row("")
        .expect("empty")
        .is_empty());
    assert!(decode_usage_rows_json_each_row("\n  \n")
        .expect("blank lines")
        .is_empty());
}

/// A grouped line without a key cannot be attributed to anything, so reading it
/// as a row named "" would fabricate a group.
#[test]
fn a_grouped_line_without_a_key_is_an_error() {
    let body =
        r#"{"requests":"1","tokens_in":"1","tokens_out":"1","cache_hit_tokens":"0","errors":"0"}"#;
    assert!(decode_usage_rows_json_each_row(body).is_err());
}

/// The quoted-integer trap applies here too: a non-numeric counter must be
/// refused instead of silently read as 0.
#[test]
fn a_non_numeric_grouped_counter_is_an_error_not_a_zero() {
    let body = r#"{"key":"gpt-4o","requests":"abc","tokens_in":"1","tokens_out":"1","cache_hit_tokens":"0","errors":"0"}"#;
    assert!(decode_usage_rows_json_each_row(body).is_err());
}

/// Both integer spellings are accepted on this path as well (CH-A).
#[test]
fn grouped_counters_decode_from_strings_and_from_numbers() {
    let quoted = r#"{"key":"k","requests":"8","tokens_in":"1","tokens_out":"1","cache_hit_tokens":"0","errors":"0"}"#;
    let plain =
        r#"{"key":"k","requests":8,"tokens_in":1,"tokens_out":1,"cache_hit_tokens":0,"errors":0}"#;
    assert_eq!(
        decode_usage_rows_json_each_row(quoted).expect("quoted")[0].totals,
        decode_usage_rows_json_each_row(plain).expect("plain")[0].totals
    );
}
