//! Endpoint handlers for the tenant self-service API.
//!
//! Each handler is a thin composition over an owner that already exists:
//!
//! | endpoint | delegates to |
//! |---|---|
//! | `whoami` (T5) | the config snapshot, through the row the gate already resolved |
//! | `usage` (T8) | [`crate::usage::query::UsageQuery`] |
//! | identity-plane reads (T5/T6) | the config snapshot, filtered to the tenant |
//!
//! Nothing here re-implements a primitive: routing and the gate live in
//! [`super`], the response envelope in [`super::respond_json`].

use std::collections::HashSet;

use pingora_proxy::Session;
use serde::Deserialize;
use serde::Serialize;
// `Engine` is the trait that provides `.encode` on the base64 alphabet below
// (`general_purpose::URL_SAFE_NO_PAD`) — pulled in as an unused-looking import
// because it is a METHOD provider, not a TYPE.
use base64::Engine;
use rand::RngCore;
use serde_json::json;

use hydra_core::auth::sha256_hex_string;
use hydra_core::config::ConfigData;
use hydra_core::model::{BillingStatus, User, UserApiKey, UserGroup};
use hydra_core::tenant_api::TenantWriteRoute;

use crate::http::{head_chars, now_rfc3339, tail_chars};
use crate::proxy::ctx::RequestContext;
use crate::proxy::AppState;
use crate::store::{ReloadGuard, ReloadSource};
use crate::tenant_api::{respond_json, Authenticated};
/// The body of `GET /whoami`.
///
/// Every field is either the tenant's own non-secret configuration or a fact the
/// caller cannot otherwise see. In particular there is **no** token digest, no
/// certificate material and no provider key — a tenant reading its own state must
/// not be handed anything it could use against another tenant, and the fields it
/// does get (`domain`) is the one it would otherwise have to open a
/// support ticket to learn.
#[derive(Serialize)]
struct Whoami<'a> {
    tenant_id: &'a str,
    name: &'a str,
    enabled: bool,
    /// The domain this tenant is currently bound to — i.e. which `Host` traffic
    /// reaches it. Read-only here: changing it is an operator action.
    domain: &'a str,
    /// The non-secret name of the tenant access token that authorised this call.
    token_name: &'a str,
    /// The configuration snapshot version the authorisation was decided against.
    /// A tenant that has just asked an operator for a change can poll this field
    /// to see when the change is live instead of guessing.
    config_version: u64,
    /// The path prefix this API is served under, for the tenant's own base URL.
    base_url: String,
}

/// `GET /tenant/{tenant_id}/api/v1/whoami`
///
/// Answers entirely from the configuration snapshot: no database, no upstream,
/// no external auth. That is why it costs nothing to serve and works identically
/// on every node.
pub async fn whoami(
    session: &mut Session,
    ctx: &mut RequestContext,
    auth: &Authenticated,
) -> pingora_core::Result<bool> {
    let t = &auth.tenant;
    let body = Whoami {
        tenant_id: &t.id,
        name: &t.name,
        enabled: t.enabled,
        domain: &t.domain,
        token_name: &auth.token_name,
        // The version the GATE read, not a fresh one: a response must never be
        // attributed to a configuration newer than the one that authorised it.
        config_version: auth.config_version,
        base_url: format!("{}{}/api/v1", crate::tenant_api::RESERVED_PREFIX, t.id),
    };
    respond_json(session, ctx, 200, &body).await
}

// ---------------------------------------------------------------------------
// E3 — GET /usage
// ---------------------------------------------------------------------------

/// The body of `GET /usage`.
///
/// The window is echoed in its **normalised** form, so a caller can see exactly
/// which rows were counted even when it sent a space-separated or epoch bound.
#[derive(Serialize)]
struct UsageView {
    /// Taken from the token, never from the query string (design §5.2).
    tenant_id: String,
    since: String,
    until: String,
    /// Newest record in the queried window, or `null` when the window has none.
    /// Never an empty string: ClickHouse answers `""` for an empty set and the
    /// decoder maps that to `None`.
    as_of: Option<String>,
    totals: hydra_core::tenant_api::UsageTotals,
    rows: Vec<hydra_core::tenant_api::UsageRow>,
    /// `none` | `model` | `provider` | `day`, echoing the accepted whitelist.
    group_by: &'static str,
    /// Which metering store answered. Reported by the reader itself, so it
    /// cannot disagree with the code that actually ran.
    source: &'static str,
}

/// `GET /tenant/{tenant_id}/api/v1/usage?since=…&until=…&group_by=…`
///
/// `since` is required and `until` defaults to now. The bounds are normalised to
/// the canonical fixed-width UTC form before they reach a store, because both
/// backends compare `created_at` **as a string** and a differently-spelled bound
/// would silently widen the window (see [`super::time_bound`]).
///
/// A read that could not be completed is a `503`, never a zeroed body: a
/// syntactically valid `{"requests":0}` is indistinguishable from "you used
/// nothing", which is the one answer this endpoint must not invent.
pub async fn usage(
    state: &AppState,
    session: &mut Session,
    ctx: &mut RequestContext,
    auth: &Authenticated,
) -> pingora_core::Result<bool> {
    // The capability, not a probe for a pool: a node can have a local database
    // handle while the configured usage backend holds no readable rows.
    #[cfg(feature = "db")]
    {
        let Some(reader) = state.usage.as_ref() else {
            return super::respond_error(
                session,
                ctx,
                503,
                "usage_store_unavailable",
                "this node has no usage store it can read",
            )
            .await;
        };

        let tenant_id = auth.tenant.id.clone();
        let raw = session.req_header().uri.query().unwrap_or("");
        let params = super::time_bound::query_params(raw);
        let get = |name: &str| {
            params
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };

        // `group_by` is a whitelist and is validated BEFORE the store is touched:
        // the column name is interpolated into SQL, so an unknown value must
        // never reach a query builder.
        let group_by = match get("group_by") {
            None => crate::usage::query::GroupBy::None,
            Some(v) => match crate::usage::query::GroupBy::parse(v) {
                Some(g) => g,
                None => {
                    return super::respond_error(
                        session,
                        ctx,
                        400,
                        "invalid_group_by",
                        "`group_by` must be one of none, model, provider, user, user_group, api_key, day",
                    )
                    .await
                }
            },
        };

        let group_by_label = crate::usage::query::group_by_label(group_by);
        let started = std::time::Instant::now();
        let window = match super::time_bound::resolve(
            get("since"),
            get("until"),
            state.tenant_api.usage_max_window_days,
            super::time_bound::now(),
        ) {
            Ok(w) => w,
            Err(e) => return super::respond_error(session, ctx, 400, e.code(), &e.message()).await,
        };

        match reader
            .aggregate(&tenant_id, &window.since, &window.until, group_by)
            .await
        {
            Ok(agg) => {
                crate::admin::metrics::record_tenant_api_usage_query(
                    reader.source(),
                    group_by_label,
                    "ok",
                    started.elapsed(),
                );
                let view = UsageView {
                    tenant_id,
                    since: window.since,
                    until: window.until,
                    as_of: agg.as_of,
                    totals: agg.totals,
                    rows: agg.rows,
                    group_by: group_by_label,
                    source: reader.source(),
                };
                super::respond_json(session, ctx, 200, &view).await
            }
            Err(e) => {
                // All three map to the same 503 + code for the tenant, but the
                // MESSAGE must differ: `store_unavailable` and `decode_error` are
                // transient ("retry or contact the operator"), while a too-large
                // result is permanent for this request — retrying returns the same
                // cut-off body, so the caller must narrow the window or shrink the
                // grouping instead. The metric label keeps the three apart for the
                // operator. Turning any of these into a zeroed 200 is exactly the
                // silent lie the capability exists to prevent.
                let (result, message) = match &e {
                    crate::usage::query::UsageQueryError::StoreUnavailable(_) => (
                        "store_unavailable",
                        "the usage store could not be read; retry or contact the operator",
                    ),
                    crate::usage::query::UsageQueryError::Decode(_) => (
                        "decode_error",
                        "the usage store could not be read; retry or contact the operator",
                    ),
                    crate::usage::query::UsageQueryError::ResultTooLarge(_) => (
                        "result_too_large",
                        "the usage result exceeded the gateway's response-size cap; narrow since/until or reduce group_by",
                    ),
                };
                tracing::warn!(
                    target: "hydra::tenant_api",
                    tenant = %tenant_id,
                    source = reader.source(),
                    result,
                    error = %e,
                    "usage query failed"
                );
                crate::admin::metrics::record_tenant_api_usage_query(
                    reader.source(),
                    group_by_label,
                    result,
                    started.elapsed(),
                );
                super::respond_error(session, ctx, 503, "usage_store_unavailable", message).await
            }
        }
    }
    // A build without `db` has no `usage` field at all, and `main` refuses to
    // start without a database, so the endpoint is unreachable. (The legacy
    // `sqlite` sink value is a retired-value error, not a selectable backend.)
    // Answering `not_ready` keeps the shape honest rather than pretending to
    // know the tenant id.
    #[cfg(not(feature = "db"))]
    {
        let _ = (state, auth);
        super::respond_error(
            session,
            ctx,
            503,
            "usage_store_unavailable",
            "this build has no usage store",
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// T7 — E4 read-only, snapshot-fed tenant views
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// T5 — the THIRD level: user groups and users
// ---------------------------------------------------------------------------
//
// Same face as T6, and for the same reason: these rows are NOT part of the config
// tree, so they do NOT go through a shared config-write core. Each handler runs ONE
// statement against the shared database and then the SAME best-effort reload tail
// [`commit_and_publish`] runs.
//
// Read/WRITE asymmetry, unchanged from T6:
//
//   * WHICH rows exist, which tenant owns them, and the version they are
//     attributed to: the config SNAPSHOT (`store.snapshot()`/`store.version()`),
//     exactly what the read-only views do. The WRITE boundary ("does this id
//     exist, and is it MINE?") is therefore decided the SAME way the read face
//     reports it — a node whose snapshot has not yet reloaded refuses a write to
//     it as `404`, the same status it would answer for reading it.
//   * What the row contains after a write: what the handler just built, echoed
//     back so the caller can confirm its own statement without a round-trip.
//
// Both models are secret-free — a [`UserGroup`] is `(id, name, enabled)` plus
// timestamps, a [`User`] is `(id, group, external_id, name, enabled)` plus the
// billing triple — so a whole row may be serialized here, unlike an api-key row
// (acceptance B2 is about digests and plaintexts; neither exists in these two).
//
// What a WRITE PRESERVES (plan §3.3): an update is NOT a re-insert.
//
//   * `created_at` of an existing row, and
//   * the billing triple (`billing_status`, `billing_reason`,
//     `billing_updated_at`) of an existing user row,
//
// are carried over from the snapshot row. `PUT /users` changes membership and
// naming; the 402 dimension belongs to `PUT /users/{uid}/billing` (T6), and a
// `PUT` that silently re-activated a delinquent user would hide a business-state
// change inside a rename. (`db::upsert_user` also leaves `created_at` alone on
// conflict, so the DB agrees; carrying the value here is what makes the RESPONSE
// body truthful, and keeps the INSERT branch honest.)
//
// Two constraints decide answers that the handler cannot see on its own:
//
//   * `user_group UNIQUE (tenant_id, name)` → a duplicate name inside the tenant
//     is `409 conflict`, via [`classify_db_err`].
//   * `user.user_group_id REFERENCES user_group(id) ON DELETE RESTRICT` → a group
//     that still has users cannot go. The snapshot answers `409 group_not_empty`
//     BEFORE the statement; the FK is what catches the RACE (a user created between
//     the check and the delete) and is mapped to the SAME answer.
//   * `user_api_key.user_id REFERENCES user(id) ON DELETE CASCADE` → deleting a
//     user takes its keys with it. Named because it is the one hierarchy delete
//     that is NOT leaf-only.
//
// Timestamps use [`crate::admin::handlers::now_ts`] (`%Y-%m-%d %H:%M:%S`), NOT
// the shared [`crate::http::now_rfc3339`]: the columns' own DDL default is
// `CURRENT_TIMESTAMP` (the same `%Y-%m-%d %H:%M:%S` shape on PostgreSQL and
// MySQL), and every upsert stamps `updated_at` with it, so a `T`-separated
// `created_at` in the same column would be two formats for one field.
// `now_rfc3339` stays right for IDENTITY expiry, which is compared
// lexicographically by `time_bound`.

/// The body of `GET .../user-groups`: the tenant's OWN groups.
#[derive(Serialize)]
struct UserGroupsView {
    /// The snapshot version the rows were read from — the same guard that supplies
    /// the data (the read-only views explain why that pairing is the point).
    config_version: u64,
    /// Every group of this tenant, in config order, ENABLED state included: a
    /// group that is disabled is still the tenant's own, and hiding it would make
    /// `PUT` to that id look like a ghost id.
    user_groups: Vec<UserGroup>,
}

/// The body of `GET .../users?group_id=`.
#[derive(Serialize)]
struct UsersView {
    config_version: u64,
    /// The filter echoed back, so a caller reconciling "did my move land?" can
    /// tell a filtered answer from an unfiltered one instead of counting rows.
    group_id: Option<String>,
    /// Sorted by id: `ConfigData::users` is a hash map, and an answer whose order
    /// moves between calls would be a diff the operator cannot read.
    users: Vec<User>,
}

/// `GET /tenant/{tenant_id}/api/v1/user-groups`
pub async fn list_user_groups(
    state: &AppState,
    session: &mut Session,
    ctx: &mut RequestContext,
    auth: &Authenticated,
) -> pingora_core::Result<bool> {
    let cfg = state.store.snapshot();
    let tenant_id = &auth.tenant.id;
    let user_groups = cfg
        .user_groups
        .iter()
        .filter(|group| group.tenant_id == *tenant_id)
        .cloned()
        .collect();
    let view = UserGroupsView {
        config_version: state.store.version(),
        user_groups,
    };
    respond_json(session, ctx, 200, &view).await
}

/// `GET /tenant/{tenant_id}/api/v1/users?group_id=`
///
/// `?group_id=` is a FILTER on this path, not a second resource — and the
/// path handed to this module carries NO query string (`proxy.rs` passes
/// `uri.path()`), so the filter is read from the URI here the way
/// [`usage`] reads `?wait=`: percent-decoded, first occurrence wins, unknown
/// parameters ignored.
///
/// A `group_id` that is not one of THIS tenant's groups is `400 invalid_group`
/// rather than an empty list: an empty list would make a typo look like "this
/// group has no users", which is a fact the caller would act on. The same
/// answer for unknown and for FOREIGN covers the enumeration question.
pub async fn list_users(
    state: &AppState,
    session: &mut Session,
    ctx: &mut RequestContext,
    auth: &Authenticated,
) -> pingora_core::Result<bool> {
    let params = super::time_bound::query_params(session.req_header().uri.query().unwrap_or(""));
    let group_id: Option<String> = params
        .iter()
        .find(|(key, _)| key == "group_id")
        .map(|(_, value)| value.as_str().to_string());
    let cfg = state.store.snapshot();
    let tenant_id = &auth.tenant.id;
    if let Some(group) = group_id.as_ref() {
        let owned = cfg
            .user_groups
            .iter()
            .any(|g| g.id == *group && g.tenant_id == *tenant_id);
        if !owned {
            return super::respond_error(
                session,
                ctx,
                400,
                "invalid_group",
                &format!(
                    "group {group:?} is not a user-group of this tenant, so it cannot \
                     filter this list",
                ),
            )
            .await;
        }
    }
    // Borrowed for the filter below; `group_id` itself moves into the view.
    let wants: Option<&String> = group_id.as_ref();
    let mut users: Vec<User> = cfg
        .users
        .values()
        .filter(|user| user.tenant_id == *tenant_id)
        .filter(|user| match wants {
            None => true,
            Some(group) => user.user_group_id == *group,
        })
        .cloned()
        .collect();
    // Deterministic order (`ConfigData::users` is a hash map — see [`UsersView`]).
    users.sort_by(|a, b| a.id.cmp(&b.id));
    let view = UsersView {
        config_version: state.store.version(),
        group_id,
        users,
    };
    respond_json(session, ctx, 200, &view).await
}

/// The body of `PUT .../user-groups` (T5): `{id?, name, enabled?}`.
///
/// The id may be client-supplied (an operator reconciling a known id) or
/// server-generated. `deny_unknown_fields` is deliberate: a misspelt field
/// (`enbled`) would otherwise be accepted and silently default, which is the
/// ghost-switch shape this repository has been bitten by.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserGroupUpsertBody {
    #[serde(default)]
    id: Option<String>,
    name: String,
    #[serde(default = "default_true")]
    enabled: bool,
}

/// The body of `PUT .../users` (T5): `{id?, user_group_id, external_id?, name, enabled?}`.
///
/// `user_group_id` has NO `?` in the contract and none here: a user with no group
/// is not a user this tenant can route, and `serde` quietly accepting an omitted
/// field as `None` would be the "accepted but meaningless" shape this plan refuses.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserUpsertBody {
    #[serde(default)]
    id: Option<String>,
    user_group_id: String,
    #[serde(default)]
    external_id: Option<String>,
    name: String,
    #[serde(default = "default_true")]
    enabled: bool,
}

/// The body of `PUT .../users/bulk`: `{"users":[{id?,user_group_id,external_id?,name,enabled?}]}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UsersBulkBody {
    users: Vec<UserUpsertBody>,
}

/// The body of `PUT .../user-groups/bulk`: `{"user_groups":[{id?,name,enabled?}]}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserGroupsBulkBody {
    user_groups: Vec<UserGroupUpsertBody>,
}

/// One key in `POST .../api-keys/bulk`. The target user is carried per entry
/// (the bulk route has no `{uid}` path segment) and the key id stays
/// server-generated.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApiKeyBulkItem {
    user_id: String,
    name: String,
    #[serde(default)]
    expires_at: Option<String>,
}

/// The body of `POST .../api-keys/bulk`: `{"keys":[{user_id,name,expires_at?}]}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApiKeysBulkBody {
    keys: Vec<ApiKeyBulkItem>,
}

/// Upper bound on entries in ONE bulk request. Larger requests are a `400`,
/// never a silent truncation: an operator reconciling 12k rows must know the
/// call was refused and chunk it.
const MAX_BULK_ENTRIES: usize = 5_000;

/// The shared `400` for an oversized bulk body.
async fn reject_oversized_bulk(
    session: &mut Session,
    ctx: &mut RequestContext,
    count: usize,
) -> pingora_core::Result<bool> {
    super::respond_error(
        session,
        ctx,
        400,
        "bulk_limit_exceeded",
        &format!(
            "this request carries {count} entries; the maximum is {MAX_BULK_ENTRIES} per \
             bulk request — split it into smaller chunks"
        ),
    )
    .await
}

/// The hierarchy-row census behind `HYDRA_MAX_HIERARCHY_ROWS` (plan §3.5): the
/// FIVE identity tables counted together, read off the PINNED snapshot the handler
/// already holds.
///
/// Why the snapshot and not a DB census: a CREATE path that reads five tables to
/// ask "may I create" adds five round trips AND turns a read failure into a new
/// failure mode of the create. The snapshot is one lock-free read the handler
/// already has.
///
/// Why the count is a GROWTH bound rather than an exact census, said so a caller
/// can trust the number: the snapshot carries ALL `user_group` and `user` rows and
/// EVERY api-key row (in the derived `users_by_api_key_hash` index, which is built
/// from the FULL key table incl. revoked/expired), but only ENABLED
/// `tenant_access_token` and `global_role` rows. Disabled rows in those two tables
/// therefore make the bound slightly LOOSE — the direction that admits a create
/// rather than refusing one, which is the safe side of a capacity guard.
#[must_use]
fn hierarchy_rows(cfg: &ConfigData) -> usize {
    cfg.user_groups.len()
        + cfg.users.len()
        + cfg.users_by_api_key_hash.len()
        + cfg.tenant_tokens.len()
        + cfg.global_roles.len()
}

/// `PUT /tenant/{tenant_id}/api/v1/user-groups`
async fn user_group_upsert(
    state: &AppState,
    reload_guard: &ReloadGuard<'_>,
    session: &mut Session,
    ctx: &mut RequestContext,
    auth: &Authenticated,
    trace_id: &str,
) -> pingora_core::Result<bool> {
    let tenant_id = &auth.tenant.id;
    let cfg = state.store.snapshot();

    let raw = match super::read_body(session, trace_id).await {
        Ok(body) => body,
        Err((status, body)) => return super::respond_raw(session, ctx, status, body).await,
    };
    let req: UserGroupUpsertBody = match parse_body(&raw) {
        Ok(parsed) => parsed,
        Err(e) => {
            return super::respond_error(
                session,
                ctx,
                400,
                "invalid_request",
                &format!(
                    "request body is not a valid user-group upsert ({{id?, name, enabled?}}): {e}"
                ),
            )
            .await;
        }
    };

    let name = req.name.trim();
    if name.is_empty() {
        return super::respond_error(
            session,
            ctx,
            400,
            "invalid_request",
            "`name` is required and must not be empty",
        )
        .await;
    }
    // An EMPTY `id` is a caller mistake, NOT "generate one": the field was NAMED,
    // so minting an id silently would answer a different request than the one
    // made. Omitting the field is the way to ask for a server-minted id.
    let id: String = match req.id {
        None => crate::admin::handlers::gen_id(),
        Some(raw_id) => {
            let trimmed = raw_id.trim();
            if trimmed.is_empty() {
                return super::respond_error(
                    session,
                    ctx,
                    400,
                    "invalid_request",
                    "`id` must not be empty — omit the field to have the server mint one",
                )
                .await;
            }
            trimmed.to_string()
        }
    };

    // UPDATE or INSERT is decided on the SNAPSHOT, tenant-scoped. An id that
    // exists under ANOTHER tenant answers the SAME 404 an unknown id does
    // (anti-enumeration) and is never upserted into THIS tenant: `db::upsert_user`
    // rewrites `tenant_id` on conflict, so refusing the cross-tenant id is
    // load-bearing, not cosmetic.
    let existing = cfg.user_groups.iter().find(|group| group.id == id);
    if let Some(group) = existing {
        if group.tenant_id != *tenant_id {
            return super::respond_error(
                session,
                ctx,
                404,
                "not_found",
                "no such user-group in this tenant",
            )
            .await;
        }
    }
    // `UNIQUE (tenant_id, name)`: asked the DB this would collide, so answer
    // BEFORE the statement and NAME the constraint instead of leaving the caller
    // with the generic `409 conflict` that `classify_db_err` would produce.
    let clash = cfg
        .user_groups
        .iter()
        .any(|other| other.tenant_id == *tenant_id && other.name == name && other.id != id);
    if clash {
        return super::respond_error(
            session,
            ctx,
            409,
            "duplicate_name",
            &format!(
                "this tenant already has a user-group named {name:?} (names are unique per tenant)"
            ),
        )
        .await;
    }

    // The global hierarchy ceiling (`HYDRA_MAX_HIERARCHY_ROWS`, plan §3.5). Checked
    // BEFORE the row is built, and ONLY when the body creates a row: an UPDATE
    // adds nothing, so a tenant sitting at the ceiling must still be able to edit
    // what it already has. Answered 400 `hierarchy_limit_exceeded` (the same code
    // the api-key ceiling uses) and never a silent partial write.
    if existing.is_none() {
        let rows = hierarchy_rows(&cfg);
        let ceiling = state.tenant_api.max_hierarchy_rows;
        if rows >= ceiling as usize {
            return super::respond_error(
                session,
                ctx,
                400,
                "hierarchy_limit_exceeded",
                &format!(
                    "the hierarchy already holds its ceiling of {ceiling} rows (user-groups + users + user api-keys + tenant tokens + global roles); HYDRA_MAX_HIERARCHY_ROWS allows no more, delete one first"
                ),
            )
                .await;
        }
    }

    // The row STARTS as whatever the tenant already has (or an honest new row)
    // and the write then changes ONLY the fields the body names, so PRESERVING
    // `created_at` is structural: a column added later cannot be "forgotten"
    // into being reset. `now_ts` matches the columns' DDL default and the
    // `CURRENT_TIMESTAMP` the upsert stamps `updated_at` with — see the section note.
    let stamped = crate::admin::handlers::now_ts();
    let mut group: UserGroup = existing.cloned().unwrap_or(UserGroup {
        id,
        tenant_id: tenant_id.to_string(),
        name: name.to_string(),
        enabled: req.enabled,
        created_at: stamped.clone(),
        updated_at: stamped.clone(),
    });
    group.name = name.to_string();
    group.enabled = req.enabled;
    group.updated_at = stamped;

    let pool = state.store.pool();
    if let Err(e) = crate::db::upsert_user_group(pool, &group).await {
        if matches!(e, sqlx::Error::RowNotFound) {
            return super::respond_error(
                session,
                ctx,
                404,
                "not_found",
                "no such user-group in this tenant",
            )
            .await;
        }
        let (status_code, code) = crate::admin::handlers::classify_db_err(&e);
        return super::respond_error(
            session,
            ctx,
            status_code,
            code,
            "the user-group could not be stored (a uniqueness constraint or FK was rejected)",
        )
        .await;
    }

    let version =
        commit_and_publish(state, reload_guard, session, ctx, tenant_id, trace_id).await?;

    // Echo the row as it now stands: the write is confirmable without a read,
    // and `config_version` is what a later call reconciles against.
    respond_json(
        session,
        ctx,
        200,
        &json!({"user_group": group, "config_version": version}),
    )
    .await
}

/// `DELETE /tenant/{tenant_id}/api/v1/user-groups/{id}`
async fn user_group_delete(
    state: &AppState,
    reload_guard: &ReloadGuard<'_>,
    session: &mut Session,
    ctx: &mut RequestContext,
    auth: &Authenticated,
    id: &str,
    trace_id: &str,
) -> pingora_core::Result<bool> {
    let tenant_id = &auth.tenant.id;
    let cfg = state.store.snapshot();
    // Unknown id and FOREIGN id answer the SAME 404: the id's existence in ANOTHER
    // tenant is not this caller's to learn.
    let known = match cfg.user_groups.iter().find(|group| group.id == id) {
        None => false,
        Some(group) => group.tenant_id == *tenant_id,
    };
    if !known {
        return super::respond_error(
            session,
            ctx,
            404,
            "not_found",
            "no such user-group in this tenant",
        )
        .await;
    }
    // `user.user_group_id ... ON DELETE RESTRICT`: a group that still has users
    // cannot go. Decided on the SNAPSHOT so the answer is the one the caller's
    // own `GET /users?group_id=` would have shown; the FK below is what catches
    // the RACE (a user created between this check and the statement).
    let populated = cfg.users.values().any(|user| user.user_group_id == id);
    if populated {
        return super::respond_error(
            session,
            ctx,
            409,
            "group_not_empty",
            "this user-group still has users; move or delete them first (the schema keeps the group referenced by ON DELETE RESTRICT)",
        )
            .await;
    }

    let pool = state.store.pool();
    let deleted = match crate::db::delete_user_group(pool, tenant_id, id).await {
        Ok(n) => n,
        Err(e) => {
            // The race above: the DB saw a user this snapshot did not. Same caller
            // mistake, so the SAME answer — `classify_db_err` would call it a
            // `400 foreign_key_violation`, which is true but not actionable.
            let fk = match e {
                sqlx::Error::Database(ref err) => err.is_foreign_key_violation(),
                _ => false,
            };
            if fk {
                return super::respond_error(
                    session,
                    ctx,
                    409,
                    "group_not_empty",
                    "the user-group gained a user between the check and the delete; retry once that user is gone",
                )
                    .await;
            }
            let (status_code, code) = crate::admin::handlers::classify_db_err(&e);
            return super::respond_error(
                session,
                ctx,
                status_code,
                code,
                "the user-group could not be deleted",
            )
            .await;
        }
    };
    if deleted == 0 {
        return super::respond_error(
            session,
            ctx,
            404,
            "not_found",
            "no such user-group in this tenant",
        )
        .await;
    }

    let _ = commit_and_publish(state, reload_guard, session, ctx, tenant_id, trace_id).await?;
    // A deletion is reported by ABSENCE: 204, no body — the same convention as
    // the api-key revocation and the identity-plane DELETE family.
    super::respond_raw(session, ctx, 204, Vec::new()).await
}

/// `PUT /tenant/{tenant_id}/api/v1/users`
async fn user_upsert(
    state: &AppState,
    reload_guard: &ReloadGuard<'_>,
    session: &mut Session,
    ctx: &mut RequestContext,
    auth: &Authenticated,
    trace_id: &str,
) -> pingora_core::Result<bool> {
    let tenant_id = &auth.tenant.id;
    let cfg = state.store.snapshot();

    let raw = match super::read_body(session, trace_id).await {
        Ok(body) => body,
        Err((status, body)) => return super::respond_raw(session, ctx, status, body).await,
    };
    let req: UserUpsertBody = match parse_body(&raw) {
        Ok(parsed) => parsed,
        Err(e) => {
            return super::respond_error(
                session,
                ctx,
                400,
                "invalid_request",
                &format!("request body is not a valid user upsert ({{id?, user_group_id, external_id?, name, enabled?}}): {e}"),
            )
                .await;
        }
    };

    // The group is validated against THIS TENANT's groups (the FK is
    // `user.user_group_id REFERENCES user_group(id)`, and the tenant boundary is
    // what makes a FOREIGN group as wrong as an unknown one — the same answer
    // covers both, so neither can be probed).
    let group_id = req.user_group_id.trim();
    if group_id.is_empty() {
        return super::respond_error(
            session,
            ctx,
            400,
            "invalid_group",
            "`user_group_id` must not be empty",
        )
        .await;
    }
    let group_owned = cfg
        .user_groups
        .iter()
        .any(|group| group.id == group_id && group.tenant_id == *tenant_id);
    if !group_owned {
        return super::respond_error(
            session,
            ctx,
            400,
            "invalid_group",
            &format!("{group_id:?} is not a user-group of this tenant, so no user of this tenant may belong to it"),
        )
            .await;
    }

    let name = req.name.trim();
    if name.is_empty() {
        return super::respond_error(
            session,
            ctx,
            400,
            "invalid_request",
            "`name` is required and must not be empty",
        )
        .await;
    }
    let id: String = match req.id {
        None => crate::admin::handlers::gen_id(),
        Some(raw_id) => {
            let trimmed = raw_id.trim();
            if trimmed.is_empty() {
                return super::respond_error(
                    session,
                    ctx,
                    400,
                    "invalid_request",
                    "`id` must not be empty — omit the field to have the server mint one",
                )
                .await;
            }
            trimmed.to_string()
        }
    };

    // An empty/whitespace `external_id` is READ as NONE rather than stored as
    // `""`: the column is `UNIQUE (tenant_id, external_id)` with NULLs distinct,
    // so two users holding `""` would collide while two users holding no external
    // id do not — and `""` carries no operator-facing meaning to preserve.
    // An ABSENT field clears it (REPLACE semantics, same as billing `reason?`).
    let external_id: Option<String> = match req.external_id {
        None => None,
        Some(raw_id) => {
            let trimmed = raw_id.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
    };

    // Same boundary rule as the group upsert: an id owned by ANOTHER tenant is a
    // 404 and is NEVER upserted, because `db::upsert_user` rewrites `tenant_id`
    // on conflict — moving another tenant's user into this one would be a
    // privilege escalation dressed up as a rename.
    let existing = cfg.users.get(&id);
    if let Some(user) = existing {
        if user.tenant_id != *tenant_id {
            return super::respond_error(
                session,
                ctx,
                404,
                "not_found",
                "no such user in this tenant",
            )
            .await;
        }
    }
    // `UNIQUE (tenant_id, external_id)`: answered BEFORE the statement, as the
    // group name is, so the caller is told WHICH id it collided with instead of
    // getting the generic `409 conflict` `classify_db_err` would return.
    // (`as_ref()` keeps the outer Option borrowable: `external_id` is still
    // assigned to the row below.)
    if let Some(external) = external_id.as_ref() {
        let clash = cfg.users.values().any(|other| {
            other.tenant_id == *tenant_id
                && other.id != id
                && other.external_id.as_deref() == Some(external.as_str())
        });
        if clash {
            return super::respond_error(
                session,
                ctx,
                409,
                "duplicate_external_id",
                &format!("another user of this tenant already carries external_id {external:?} (unique per tenant)"),
            )
                .await;
        }
    }

    // The two ceilings (plan §3.5), checked on a CREATE only — an UPDATE of an
    // existing row adds no row, so a tenant at its ceiling can still edit what it
    // has. The per-tenant user count is taken over this TENANT's own rows
    // (`cfg.users` is a node-wide map), and both counts come from the PINNED
    // snapshot the handler already holds — see [`hierarchy_rows`] for why the
    // snapshot is the right source and what its looseness is.
    if existing.is_none() {
        let ceiling = state.tenant_api.max_users_per_tenant;
        let held = cfg
            .users
            .values()
            .filter(|u| u.tenant_id == *tenant_id)
            .count();
        // Widened to the same `usize` space as the count, so an absurd count
        // cannot slip past a `u32` comparison.
        if held >= ceiling as usize {
            return super::respond_error(
                session,
                ctx,
                400,
                "hierarchy_limit_exceeded",
                &format!(
                    "this tenant already holds its ceiling of {ceiling} users; HYDRA_MAX_USERS_PER_TENANT allows no more, delete one first"
                ),
            )
                .await;
        }
        let rows = hierarchy_rows(&cfg);
        let global = state.tenant_api.max_hierarchy_rows;
        if rows >= global as usize {
            return super::respond_error(
                session,
                ctx,
                400,
                "hierarchy_limit_exceeded",
                &format!(
                    "the hierarchy already holds its ceiling of {global} rows (user-groups + users + user api-keys + tenant tokens + global roles); HYDRA_MAX_HIERARCHY_ROWS allows no more, delete one first"
                ),
            )
                .await;
        }
    }

    // See the section note: the row starts as the tenant's existing row (or an
    // honest new one), and ONLY the fields the body names are assigned. `created_at`
    // and the whole billing triple therefore survive a `PUT /users` — suspension
    // belongs to `PUT /users/{uid}/billing` (T6).
    let stamped = crate::admin::handlers::now_ts();
    let mut user: User = existing.cloned().unwrap_or(User {
        id,
        tenant_id: tenant_id.to_string(),
        user_group_id: group_id.to_string(),
        external_id: None,
        name: name.to_string(),
        enabled: req.enabled,
        billing_status: BillingStatus::Active,
        billing_reason: None,
        billing_updated_at: None,
        created_at: stamped.clone(),
        updated_at: stamped.clone(),
    });
    user.user_group_id = group_id.to_string();
    user.external_id = external_id;
    user.name = name.to_string();
    user.enabled = req.enabled;
    user.updated_at = stamped;

    let pool = state.store.pool();
    if let Err(e) = crate::db::upsert_user(pool, &user).await {
        if matches!(e, sqlx::Error::RowNotFound) {
            return super::respond_error(
                session,
                ctx,
                404,
                "not_found",
                "no such user in this tenant",
            )
            .await;
        }
        let (status_code, code) = crate::admin::handlers::classify_db_err(&e);
        return super::respond_error(
            session,
            ctx,
            status_code,
            code,
            "the user could not be stored (a uniqueness constraint or the group FK was rejected)",
        )
        .await;
    }

    let version =
        commit_and_publish(state, reload_guard, session, ctx, tenant_id, trace_id).await?;

    respond_json(
        session,
        ctx,
        200,
        &json!({"user": user, "config_version": version}),
    )
    .await
}

/// `PUT /tenant/{tenant_id}/api/v1/users/bulk`
///
/// The same row rules as [`user_upsert`], applied to a batch: every group must be
/// this tenant's, every named id must not belong to another tenant, generated ids
/// are minted for entries without one, and the whole batch lands in ONE DB
/// transaction followed by ONE reload.
async fn user_bulk_upsert(
    state: &AppState,
    reload_guard: &ReloadGuard<'_>,
    session: &mut Session,
    ctx: &mut RequestContext,
    auth: &Authenticated,
    trace_id: &str,
) -> pingora_core::Result<bool> {
    let tenant_id = &auth.tenant.id;
    let cfg = state.store.snapshot();

    let raw = match super::read_body(session, trace_id).await {
        Ok(body) => body,
        Err((status, body)) => return super::respond_raw(session, ctx, status, body).await,
    };
    let req: UsersBulkBody = match parse_body(&raw) {
        Ok(parsed) => parsed,
        Err(e) => {
            return super::respond_error(
                session,
                ctx,
                400,
                "invalid_request",
                &format!(
                    "request body is not a valid user bulk upsert \
                     ({{users:[{{id?, user_group_id, external_id?, name, enabled?}}]}}): {e}"
                ),
            )
            .await;
        }
    };
    if req.users.len() > MAX_BULK_ENTRIES {
        return reject_oversized_bulk(session, ctx, req.users.len()).await;
    }
    if req.users.is_empty() {
        return respond_json(
            session,
            ctx,
            200,
            &json!({"users": [], "config_version": state.store.version()}),
        )
        .await;
    }

    let stamped = crate::admin::handlers::now_ts();
    let mut out: Vec<User> = Vec::with_capacity(req.users.len());
    let mut seen_ids: HashSet<String> = HashSet::with_capacity(req.users.len());
    let mut seen_external: HashSet<String> = HashSet::new();
    let mut new_rows = 0usize;

    for item in req.users {
        let group_id = item.user_group_id.trim();
        if group_id.is_empty() {
            return super::respond_error(
                session,
                ctx,
                400,
                "invalid_group",
                "`user_group_id` must not be empty",
            )
            .await;
        }
        let group_owned = cfg
            .user_groups
            .iter()
            .any(|group| group.id == group_id && group.tenant_id == *tenant_id);
        if !group_owned {
            return super::respond_error(
                session,
                ctx,
                400,
                "invalid_group",
                &format!(
                    "{group_id:?} is not a user-group of this tenant, so no user of this \
                     tenant may belong to it"
                ),
            )
            .await;
        }

        let name = item.name.trim();
        if name.is_empty() {
            return super::respond_error(
                session,
                ctx,
                400,
                "invalid_request",
                "`name` is required and must not be empty",
            )
            .await;
        }

        let id: String = match item.id {
            None => crate::admin::handlers::gen_id(),
            Some(raw_id) => {
                let trimmed = raw_id.trim();
                if trimmed.is_empty() {
                    return super::respond_error(
                        session,
                        ctx,
                        400,
                        "invalid_request",
                        "`id` must not be empty — omit the field to have the server mint one",
                    )
                    .await;
                }
                trimmed.to_string()
            }
        };
        if !seen_ids.insert(id.clone()) {
            return super::respond_error(
                session,
                ctx,
                400,
                "duplicate_id",
                &format!("id {id:?} appears more than once in this bulk request"),
            )
            .await;
        }

        let external_id: Option<String> = match item.external_id {
            None => None,
            Some(raw_id) => {
                let trimmed = raw_id.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            }
        };
        if let Some(external) = external_id.as_ref() {
            if !seen_external.insert(external.clone()) {
                return super::respond_error(
                    session,
                    ctx,
                    409,
                    "duplicate_external_id",
                    &format!(
                        "external_id {external:?} appears more than once in this bulk request \
                         (unique per tenant)"
                    ),
                )
                .await;
            }
        }

        let existing = cfg.users.get(&id);
        if let Some(user) = existing {
            if user.tenant_id != *tenant_id {
                return super::respond_error(
                    session,
                    ctx,
                    404,
                    "not_found",
                    "no such user in this tenant",
                )
                .await;
            }
        } else {
            new_rows += 1;
        }
        if let Some(external) = external_id.as_ref() {
            let clash = cfg.users.values().any(|other| {
                other.tenant_id == *tenant_id
                    && other.id != id
                    && other.external_id.as_deref() == Some(external.as_str())
            });
            if clash {
                return super::respond_error(
                    session,
                    ctx,
                    409,
                    "duplicate_external_id",
                    &format!(
                        "another user of this tenant already carries external_id {external:?} \
                         (unique per tenant)"
                    ),
                )
                .await;
            }
        }

        let mut user: User = existing.cloned().unwrap_or(User {
            id: id.clone(),
            tenant_id: tenant_id.to_string(),
            user_group_id: group_id.to_string(),
            external_id: None,
            name: name.to_string(),
            enabled: item.enabled,
            billing_status: BillingStatus::Active,
            billing_reason: None,
            billing_updated_at: None,
            created_at: stamped.clone(),
            updated_at: stamped.clone(),
        });
        user.user_group_id = group_id.to_string();
        user.external_id = external_id;
        user.name = name.to_string();
        user.enabled = item.enabled;
        user.updated_at = stamped.clone();
        out.push(user);
    }

    let held = cfg
        .users
        .values()
        .filter(|u| u.tenant_id == *tenant_id)
        .count();
    let ceiling = state.tenant_api.max_users_per_tenant;
    if held + new_rows > ceiling as usize {
        return super::respond_error(
            session,
            ctx,
            400,
            "hierarchy_limit_exceeded",
            &format!(
                "this batch would grow the tenant from {held} to {} users, past its ceiling of \
                 {ceiling}; HYDRA_MAX_USERS_PER_TENANT allows no more",
                held + new_rows
            ),
        )
        .await;
    }
    let rows = hierarchy_rows(&cfg);
    let global = state.tenant_api.max_hierarchy_rows;
    if rows + new_rows > global as usize {
        return super::respond_error(
            session,
            ctx,
            400,
            "hierarchy_limit_exceeded",
            &format!(
                "this batch would grow the hierarchy from {rows} to {} rows, past its ceiling of \
                 {global}; HYDRA_MAX_HIERARCHY_ROWS allows no more",
                rows + new_rows
            ),
        )
        .await;
    }

    let pool = state.store.pool();
    if let Err(e) = crate::db::upsert_users_bulk(pool, &out).await {
        if matches!(e, sqlx::Error::RowNotFound) {
            return super::respond_error(
                session,
                ctx,
                404,
                "not_found",
                "a user in this batch is not part of this tenant",
            )
            .await;
        }
        let (status_code, code) = crate::admin::handlers::classify_db_err(&e);
        return super::respond_error(
            session,
            ctx,
            status_code,
            code,
            "the users could not be stored (a uniqueness constraint or the group FK was rejected)",
        )
        .await;
    }

    let version =
        commit_and_publish(state, reload_guard, session, ctx, tenant_id, trace_id).await?;

    respond_json(
        session,
        ctx,
        200,
        &json!({"users": out, "config_version": version}),
    )
    .await
}

/// `PUT /tenant/{tenant_id}/api/v1/user-groups/bulk`
///
/// The bulk counterpart of [`user_group_upsert`]. The batch is validated
/// tenant-locally, written in ONE transaction and reloaded ONCE.
async fn user_group_bulk_upsert(
    state: &AppState,
    reload_guard: &ReloadGuard<'_>,
    session: &mut Session,
    ctx: &mut RequestContext,
    auth: &Authenticated,
    trace_id: &str,
) -> pingora_core::Result<bool> {
    let tenant_id = &auth.tenant.id;
    let cfg = state.store.snapshot();

    let raw = match super::read_body(session, trace_id).await {
        Ok(body) => body,
        Err((status, body)) => return super::respond_raw(session, ctx, status, body).await,
    };
    let req: UserGroupsBulkBody = match parse_body(&raw) {
        Ok(parsed) => parsed,
        Err(e) => {
            return super::respond_error(
                session,
                ctx,
                400,
                "invalid_request",
                &format!(
                    "request body is not a valid user-group bulk upsert \
                     ({{user_groups:[{{id?, name, enabled?}}]}}): {e}"
                ),
            )
            .await;
        }
    };
    if req.user_groups.len() > MAX_BULK_ENTRIES {
        return reject_oversized_bulk(session, ctx, req.user_groups.len()).await;
    }
    if req.user_groups.is_empty() {
        return respond_json(
            session,
            ctx,
            200,
            &json!({"user_groups": [], "config_version": state.store.version()}),
        )
        .await;
    }

    let stamped = crate::admin::handlers::now_ts();
    let mut out: Vec<UserGroup> = Vec::with_capacity(req.user_groups.len());
    let mut seen_ids: HashSet<String> = HashSet::with_capacity(req.user_groups.len());
    let mut seen_names: HashSet<String> = HashSet::new();
    let mut new_rows = 0usize;

    for item in req.user_groups {
        let name = item.name.trim();
        if name.is_empty() {
            return super::respond_error(
                session,
                ctx,
                400,
                "invalid_request",
                "`name` is required and must not be empty",
            )
            .await;
        }
        let id: String = match item.id {
            None => crate::admin::handlers::gen_id(),
            Some(raw_id) => {
                let trimmed = raw_id.trim();
                if trimmed.is_empty() {
                    return super::respond_error(
                        session,
                        ctx,
                        400,
                        "invalid_request",
                        "`id` must not be empty — omit the field to have the server mint one",
                    )
                    .await;
                }
                trimmed.to_string()
            }
        };
        if !seen_ids.insert(id.clone()) {
            return super::respond_error(
                session,
                ctx,
                400,
                "duplicate_id",
                &format!("id {id:?} appears more than once in this bulk request"),
            )
            .await;
        }
        if !seen_names.insert(name.to_string()) {
            return super::respond_error(
                session,
                ctx,
                409,
                "duplicate_name",
                &format!(
                    "user-group name {name:?} appears more than once in this bulk request \
                     (names are unique per tenant)"
                ),
            )
            .await;
        }

        let existing = cfg.user_groups.iter().find(|group| group.id == id);
        if let Some(group) = existing {
            if group.tenant_id != *tenant_id {
                return super::respond_error(
                    session,
                    ctx,
                    404,
                    "not_found",
                    "no such user-group in this tenant",
                )
                .await;
            }
        } else {
            new_rows += 1;
        }
        let clash = cfg
            .user_groups
            .iter()
            .any(|other| other.tenant_id == *tenant_id && other.name == name && other.id != id);
        if clash {
            return super::respond_error(
                session,
                ctx,
                409,
                "duplicate_name",
                &format!(
                    "this tenant already has a user-group named {name:?} \
                     (names are unique per tenant)"
                ),
            )
            .await;
        }

        let mut group: UserGroup = existing.cloned().unwrap_or(UserGroup {
            id: id.clone(),
            tenant_id: tenant_id.to_string(),
            name: name.to_string(),
            enabled: item.enabled,
            created_at: stamped.clone(),
            updated_at: stamped.clone(),
        });
        group.name = name.to_string();
        group.enabled = item.enabled;
        group.updated_at = stamped.clone();
        out.push(group);
    }

    let rows = hierarchy_rows(&cfg);
    let ceiling = state.tenant_api.max_hierarchy_rows;
    if rows + new_rows > ceiling as usize {
        return super::respond_error(
            session,
            ctx,
            400,
            "hierarchy_limit_exceeded",
            &format!(
                "this batch would grow the hierarchy from {rows} to {} rows, past its ceiling \
                 of {ceiling}; HYDRA_MAX_HIERARCHY_ROWS allows no more",
                rows + new_rows
            ),
        )
        .await;
    }

    let pool = state.store.pool();
    if let Err(e) = crate::db::upsert_user_groups_bulk(pool, &out).await {
        if matches!(e, sqlx::Error::RowNotFound) {
            return super::respond_error(
                session,
                ctx,
                404,
                "not_found",
                "a user-group in this batch is not part of this tenant",
            )
            .await;
        }
        let (status_code, code) = crate::admin::handlers::classify_db_err(&e);
        return super::respond_error(
            session,
            ctx,
            status_code,
            code,
            "the user-groups could not be stored (a uniqueness constraint was rejected)",
        )
        .await;
    }

    let version =
        commit_and_publish(state, reload_guard, session, ctx, tenant_id, trace_id).await?;

    respond_json(
        session,
        ctx,
        200,
        &json!({"user_groups": out, "config_version": version}),
    )
    .await
}

/// `POST /tenant/{tenant_id}/api/v1/api-keys/bulk`
///
/// Mints one key per entry, all in ONE DB transaction and ONE reload. Every
/// target user must belong to this tenant; the key/user/hierarchy ceilings are
/// checked for the WHOLE batch before anything is minted, so an over-limit caller
/// never receives plaintext it cannot use. The plaintexts are shown exactly once,
/// in the response body.
async fn user_api_key_bulk_create(
    state: &AppState,
    reload_guard: &ReloadGuard<'_>,
    session: &mut Session,
    ctx: &mut RequestContext,
    auth: &Authenticated,
    trace_id: &str,
) -> pingora_core::Result<bool> {
    let tenant_id = &auth.tenant.id;
    let cfg = state.store.snapshot();

    let raw = match super::read_body(session, trace_id).await {
        Ok(body) => body,
        Err((status, body)) => return super::respond_raw(session, ctx, status, body).await,
    };
    let req: ApiKeysBulkBody = match parse_body(&raw) {
        Ok(parsed) => parsed,
        Err(e) => {
            return super::respond_error(
                session,
                ctx,
                400,
                "invalid_request",
                &format!(
                    "request body is not a valid api-key bulk mint \
                     ({{keys:[{{user_id, name, expires_at?}}]}}): {e}"
                ),
            )
            .await;
        }
    };
    if req.keys.len() > MAX_BULK_ENTRIES {
        return reject_oversized_bulk(session, ctx, req.keys.len()).await;
    }
    if req.keys.is_empty() {
        return respond_json(
            session,
            ctx,
            200,
            &json!({"api_keys": [], "config_version": state.store.version()}),
        )
        .await;
    }

    let own_users: HashSet<String> = cfg
        .users
        .iter()
        .filter(|(_, u)| u.tenant_id == *tenant_id)
        .map(|(_, u)| u.id.clone())
        .collect();

    // Validate / normalize the whole batch BEFORE minting any credential.
    let mut normalized: Vec<(String, String, Option<String>)> = Vec::with_capacity(req.keys.len());
    for item in req.keys {
        let user_id = item.user_id.trim();
        if user_id.is_empty() {
            return super::respond_error(
                session,
                ctx,
                400,
                "invalid_request",
                "`user_id` must not be empty",
            )
            .await;
        }
        if !own_users.contains(user_id) {
            return super::respond_error(
                session,
                ctx,
                404,
                "not_found",
                "no such user in this tenant",
            )
            .await;
        }
        let name = item.name.trim();
        if name.is_empty() {
            return super::respond_error(
                session,
                ctx,
                400,
                "invalid_request",
                "`name` must not be empty",
            )
            .await;
        }
        let expires_at = match item.expires_at {
            None => None,
            Some(raw_expiry) => match super::time_bound::parse_bound(&raw_expiry) {
                None => {
                    return super::respond_error(
                        session,
                        ctx,
                        400,
                        "invalid_request",
                        "`expires_at` must be RFC3339 (2026-09-16T00:00:00Z), epoch seconds \
                         or epoch milliseconds",
                    )
                    .await;
                }
                Some(dt) => Some(super::time_bound::canonical(dt)),
            },
        };
        normalized.push((user_id.to_string(), name.to_string(), expires_at));
    }

    let pool = state.store.pool();
    let rows = match crate::db::list_user_api_keys(pool).await {
        Err(e) => {
            let (status, code) = crate::admin::handlers::classify_db_err(&e);
            return super::respond_error(
                session,
                ctx,
                status,
                code,
                "the api-key rows could not be read",
            )
            .await;
        }
        Ok(r) => r,
    };
    let held = rows
        .iter()
        .filter(|k| own_users.contains(&k.user_id))
        .count();
    let ceiling = state.tenant_api.max_user_api_keys_per_tenant;
    if held + normalized.len() > ceiling as usize {
        return super::respond_error(
            session,
            ctx,
            400,
            "hierarchy_limit_exceeded",
            &format!(
                "this batch would grow the tenant from {held} to {} api-keys, past its ceiling \
                 of {ceiling}; HYDRA_MAX_USER_API_KEYS_PER_TENANT allows no more",
                held + normalized.len()
            ),
        )
        .await;
    }
    let hierarchy = hierarchy_rows(&cfg);
    let global = state.tenant_api.max_hierarchy_rows;
    if hierarchy + normalized.len() > global as usize {
        return super::respond_error(
            session,
            ctx,
            400,
            "hierarchy_limit_exceeded",
            &format!(
                "this batch would grow the hierarchy from {hierarchy} to {} rows, past its \
                 ceiling of {global}; HYDRA_MAX_HIERARCHY_ROWS allows no more",
                hierarchy + normalized.len()
            ),
        )
        .await;
    }

    let now = now_rfc3339();
    let mut keys: Vec<UserApiKey> = Vec::with_capacity(normalized.len());
    let mut out: Vec<serde_json::Value> = Vec::with_capacity(normalized.len());
    for (user_id, name, expires_at) in normalized {
        let plaintext = new_api_key_plaintext();
        let key_hash = sha256_hex_string(plaintext.as_bytes());
        let key = UserApiKey {
            id: crate::admin::handlers::gen_id(),
            user_id,
            name,
            key_hash,
            key_prefix: head_chars(&plaintext, 8),
            key_last4: tail_chars(&plaintext, 4),
            enabled: true,
            expires_at,
            created_at: now.clone(),
            updated_at: now.clone(),
        };
        out.push(json!({
            "id": key.id,
            "key": plaintext,
            "key_prefix": key.key_prefix,
            "key_last4": key.key_last4,
        }));
        keys.push(key);
    }

    if let Err(e) = crate::db::insert_user_api_keys_bulk(pool, tenant_id, &keys).await {
        if matches!(e, sqlx::Error::RowNotFound) {
            return super::respond_error(
                session,
                ctx,
                404,
                "not_found",
                "a user in this batch is not part of this tenant",
            )
            .await;
        }
        let (status, code) = crate::admin::handlers::classify_db_err(&e);
        return super::respond_error(
            session,
            ctx,
            status,
            code,
            "the api-keys could not be stored",
        )
        .await;
    }

    let version =
        commit_and_publish(state, reload_guard, session, ctx, tenant_id, trace_id).await?;

    respond_json(
        session,
        ctx,
        200,
        &json!({"api_keys": out, "config_version": version}),
    )
    .await
}

/// `DELETE /tenant/{tenant_id}/api/v1/users/{id}`
///
/// Unlike the group delete above, this one is NOT leaf-only: `user_api_key.user_id
/// REFERENCES user(id) ON DELETE CASCADE`, so the user's api-keys go WITH it. That
/// is the schema's decision (a key with no principal cannot be routed), and it is
/// said here because a caller that meant "revoke the keys, keep the user" would
/// otherwise learn the difference only after the fact. `global_role` stores `user_id`
/// as a plain dimension (`*` = wildcard, decision D-2) and has NO FK here, so a
/// role naming a deleted user is stale data, not a rejected statement — that is
/// T10's tidy-up, not this endpoint's.
async fn user_delete(
    state: &AppState,
    reload_guard: &ReloadGuard<'_>,
    session: &mut Session,
    ctx: &mut RequestContext,
    auth: &Authenticated,
    id: &str,
    trace_id: &str,
) -> pingora_core::Result<bool> {
    let tenant_id = &auth.tenant.id;
    let cfg = state.store.snapshot();
    let known = match cfg.users.get(id) {
        None => false,
        Some(user) => user.tenant_id == *tenant_id,
    };
    if !known {
        return super::respond_error(
            session,
            ctx,
            404,
            "not_found",
            "no such user in this tenant",
        )
        .await;
    }

    let pool = state.store.pool();
    let deleted = match crate::db::delete_user(pool, tenant_id, id).await {
        Ok(n) => n,
        Err(e) => {
            let (status_code, code) = crate::admin::handlers::classify_db_err(&e);
            return super::respond_error(
                session,
                ctx,
                status_code,
                code,
                "the user could not be deleted",
            )
            .await;
        }
    };
    if deleted == 0 {
        return super::respond_error(
            session,
            ctx,
            404,
            "not_found",
            "no such user in this tenant",
        )
        .await;
    }

    let _ = commit_and_publish(state, reload_guard, session, ctx, tenant_id, trace_id).await?;
    // 204, no body — reported by ABSENCE like every other delete here.
    super::respond_raw(session, ctx, 204, Vec::new()).await
}

// ---------------------------------------------------------------------------
// T6 — the FOURth level: a user's api-keys, and its billing state
// ---------------------------------------------------------------------------
//
// These endpoints do NOT go through a shared config-write core: that kind of core
// owns the config-tree resource family, and it has no shape for an api-key row or a
// user row. Each handler therefore runs ONE statement against the shared
// database and then the SAME best-effort reload tail [`commit_and_publish`] runs,
// so the two write faces cannot drift.
//
// What is read from WHERE — the asymmetry is load-bearing:
//
//   * TENANT scoping, the user row and the billing state: the config SNAPSHOT
//     (`store.snapshot()`), exactly the rule the read-only views use, so a node
//     answers like the process that wrote the row once its snapshot reloads.
//   * api-key DISPLAY atoms (`name`, `key_prefix`, `key_last4`, `created_at`): the
//     DATABASE. The snapshot does hold the key set, but only as the derived index
//     `ConfigData::users_by_api_key_hash`, whose value is the ROUTING atoms
//     (`ApiKeyIdentity`: principal, the three enabled flags, expiry) and
//     deliberately NOT the display atoms — and whose KEY is the key HASH, which no
//     endpoint may enumerate. So the rows come from `db::list_user_api_keys`,
//     filtered to the caller's OWN users.
//
// `key_hash` never enters a response or a log here (acceptance B2), and the
// plaintext exists exactly once: in the create response.

/// The body of `POST .../users/{uid}/api-keys` (T6): `{name, expires_at?}`.
/// The target user comes from the URL and the key's own id is server-generated —
/// NEITHER is ever read from the body.
#[derive(Deserialize)]
struct ApiKeyCreateBody {
    name: String,
    #[serde(default)]
    expires_at: Option<String>,
}

/// The body of `PUT .../users/{uid}/billing` (T6): `{status, reason?}`.
/// `status` is kept as a STRING and validated by hand: a bad value must answer
/// `400 invalid_request` NAMING the two accepted spellings, which a serde enum
/// would replace with a parser message that does not.
#[derive(Deserialize)]
struct BillingBody {
    status: String,
    #[serde(default)]
    reason: Option<String>,
}

/// One api-key as the tenant may see it: the display atoms ONLY.
///
/// A hand-projected view rather than `serde` on [`UserApiKey`]: the model carries
/// `key_hash`, and serialising the model would put a stolen-from-disk digest on the
/// wire (acceptance B2). Deleting a field from this struct is therefore the kind of
/// change that must be reviewed for what it EXPOSES, not what it hides.
#[derive(Serialize)]
struct ApiKeyRow {
    id: String,
    name: String,
    /// First 8 characters of the plaintext — deliberately short: enough for a
    /// human to recognise a key in a list, far too little to be worth guessing.
    key_prefix: String,
    /// Last 4 characters, for the same reason.
    key_last4: String,
    enabled: bool,
    /// Canonical RFC3339 UTC, or `None` = never expires.
    expires_at: Option<String>,
    created_at: String,
}

/// Project a key row onto the view above (the ONE place that decides what a key
/// looks like to its owner).
fn api_key_row(k: &UserApiKey) -> ApiKeyRow {
    ApiKeyRow {
        id: k.id.clone(),
        name: k.name.clone(),
        key_prefix: k.key_prefix.clone(),
        key_last4: k.key_last4.clone(),
        enabled: k.enabled,
        expires_at: k.expires_at.clone(),
        created_at: k.created_at.clone(),
    }
}

/// The body of `GET .../users/{uid}/api-keys`.
#[derive(Serialize)]
struct UserApiKeysView {
    /// The snapshot version the TENANT scoping was decided against. The rows
    /// themselves come from this node's DB, which can be a moment newer after a
    /// local write — said so here rather than papered over, because the tenant
    /// reconciles this field against `whoami`.
    config_version: u64,
    user_id: String,
    api_keys: Vec<ApiKeyRow>,
}

/// The body of `PUT .../users/{uid}/billing`: the user as it now stands.
#[derive(Serialize)]
struct UserBillingView {
    config_version: u64,
    user: User,
}

/// `GET /tenant/{tenant_id}/api/v1/users/{uid}/api-keys`
pub async fn list_user_api_keys(
    state: &AppState,
    session: &mut Session,
    ctx: &mut RequestContext,
    auth: &Authenticated,
    user_id: &str,
) -> pingora_core::Result<bool> {
    let cfg = state.store.snapshot();
    let tenant_id = &auth.tenant.id;
    // The user must exist AND be this tenant's. Unknown and foreign answer the
    // SAME 404: which half failed would tell a caller whether a user id exists
    // under ANOTHER tenant.
    let known = match cfg.users.get(user_id) {
        Some(u) => u.tenant_id == *tenant_id,
        None => false,
    };
    if !known {
        return super::respond_error(
            session,
            ctx,
            404,
            "not_found",
            "no such user in this tenant",
        )
        .await;
    }

    // Display atoms from the DB (see the section note). The whole table is read
    // and filtered in Rust — the shape `db::list_user_api_keys` already publishes
    // ("the loader returns ALL rows"); the tenant filter runs through the user
    // rows, because a key row names only its `user_id`.
    let pool = state.store.pool();
    let rows = match crate::db::list_user_api_keys(pool).await {
        Err(e) => {
            let (status, code) = crate::admin::handlers::classify_db_err(&e);
            return super::respond_error(
                session,
                ctx,
                status,
                code,
                "the api-key list could not be read",
            )
            .await;
        }
        Ok(r) => r,
    };
    let api_keys: Vec<ApiKeyRow> = rows
        .iter()
        .filter(|k| k.user_id == user_id)
        .map(api_key_row)
        .collect();
    respond_json(
        session,
        ctx,
        200,
        &UserApiKeysView {
            config_version: state.store.version(),
            user_id: user_id.to_string(),
            api_keys,
        },
    )
    .await
}

/// `POST /tenant/{tenant_id}/api/v1/users/{uid}/api-keys`
///
/// The ONE endpoint that shows a credential in plaintext, and it does so ONCE
/// (decision D-7). What is persisted is the SHA-256 hex plus the two display
/// atoms; the plaintext never touches the DB, a log line, or an error body.
async fn user_api_key_create(
    state: &AppState,
    reload_guard: &ReloadGuard<'_>,
    session: &mut Session,
    ctx: &mut RequestContext,
    auth: &Authenticated,
    user_id: &str,
    trace_id: &str,
) -> pingora_core::Result<bool> {
    let tenant_id = &auth.tenant.id;
    let cfg = state.store.snapshot();
    let known = match cfg.users.get(user_id) {
        Some(u) => u.tenant_id == *tenant_id,
        None => false,
    };
    if !known {
        return super::respond_error(
            session,
            ctx,
            404,
            "not_found",
            "no such user in this tenant",
        )
        .await;
    }

    let raw = match super::read_body(session, trace_id).await {
        Ok(b) => b,
        Err((status, body)) => return super::respond_raw(session, ctx, status, body).await,
    };
    let req: ApiKeyCreateBody = match parse_body(&raw) {
        Ok(r) => r,
        Err(e) => {
            return super::respond_error(
                session,
                ctx,
                400,
                "invalid_request",
                &format!("request body is not valid JSON: {e}"),
            )
            .await;
        }
    };
    let name = req.name.trim();
    if name.is_empty() {
        return super::respond_error(
            session,
            ctx,
            400,
            "invalid_request",
            "`name` must not be empty",
        )
        .await;
    }

    // Normalise the expiry into the canonical form the identity index compares
    // (plan §3.1): anything `time_bound::parse_bound` understands, re-rendered by
    // `canonical`. An unparseable value is a `400`, NEVER a silently-absent expiry
    // — a key the caller meant to expire must not become immortal.
    let expires_at = match req.expires_at {
        None => None,
        Some(raw_expiry) => match super::time_bound::parse_bound(&raw_expiry) {
            None => {
                return super::respond_error(
                    session,
                    ctx,
                    400,
                    "invalid_request",
                    "`expires_at` must be RFC3339 (2026-09-16T00:00:00Z), epoch seconds or epoch milliseconds",
                )
                    .await;
            }
            Some(dt) => Some(super::time_bound::canonical(dt)),
        },
    };

    // The global hierarchy ceiling (`HYDRA_MAX_HIERARCHY_ROWS`, plan §3.5). This
    // endpoint is always a CREATE, so the check is unconditional — unlike the
    // user-group / user upserts, which skip it when the row already exists.
    {
        let rows = hierarchy_rows(&cfg);
        let ceiling = state.tenant_api.max_hierarchy_rows;
        if rows >= ceiling as usize {
            return super::respond_error(
                session,
                ctx,
                400,
                "hierarchy_limit_exceeded",
                &format!(
                    "the hierarchy already holds its ceiling of {ceiling} rows (user-groups + users + user api-keys + tenant tokens + global roles); HYDRA_MAX_HIERARCHY_ROWS allows no more, delete one first"
                ),
            )
                .await;
        }
    }

    // The per-tenant ceiling: counted over the tenant's OWN users, since a key
    // row names only its `user_id`. Read BEFORE minting, so an over-limit caller
    // never gets a plaintext it cannot use.
    let pool = state.store.pool();
    let rows = match crate::db::list_user_api_keys(pool).await {
        Err(e) => {
            let (status, code) = crate::admin::handlers::classify_db_err(&e);
            return super::respond_error(
                session,
                ctx,
                status,
                code,
                "the api-key rows could not be read",
            )
            .await;
        }
        Ok(r) => r,
    };
    let own_users: HashSet<String> = cfg
        .users
        .iter()
        .filter(|(_, u)| u.tenant_id == *tenant_id)
        .map(|(_, u)| u.id.clone())
        .collect();
    let held = rows
        .iter()
        .filter(|k| own_users.contains(&k.user_id))
        .count();
    // `usize` (a row count) against the configured `u32` ceiling: the ceiling is
    // widened, never the count narrowed, so an absurd count cannot slip past.
    let ceiling = state.tenant_api.max_user_api_keys_per_tenant;
    if held >= ceiling as usize {
        return super::respond_error(
            session,
            ctx,
            400,
            "hierarchy_limit_exceeded",
            &format!(
                "this tenant already holds its ceiling of {ceiling} api-keys; HYDRA_MAX_USER_API_KEYS_PER_TENANT allows no more, delete one first"
            ),
        )
            .await;
    }

    // Mint the credential: 32 CSPRNG bytes, base64url without padding, so the
    // plaintext is `hk_` + 43 characters. It exists for the length of this
    // statement; what reaches the database is the SHA-256 hex and the two
    // display atoms (decision D-7).
    let plaintext = new_api_key_plaintext();
    let key_hash = sha256_hex_string(plaintext.as_bytes());
    let now = now_rfc3339();
    let key = UserApiKey {
        id: crate::admin::handlers::gen_id(),
        user_id: user_id.to_string(),
        name: name.to_string(),
        key_hash,
        key_prefix: head_chars(&plaintext, 8),
        key_last4: tail_chars(&plaintext, 4),
        enabled: true,
        expires_at,
        created_at: now.clone(),
        updated_at: now,
    };

    if let Err(e) = crate::db::insert_user_api_key(pool, tenant_id, &key).await {
        if matches!(e, sqlx::Error::RowNotFound) {
            return super::respond_error(
                session,
                ctx,
                404,
                "not_found",
                "no such user in this tenant",
            )
            .await;
        }
        // The ONE classifier the admin plane uses: a `UNIQUE` hit here can only
        // be the globally-unique `key_hash`, and lock contention is the retryable
        // 503 `storage_busy` — NOT a 500.
        let (status, code) = crate::admin::handlers::classify_db_err(&e);
        return super::respond_error(
            session,
            ctx,
            status,
            code,
            "the api-key could not be stored",
        )
        .await;
    }

    let version =
        commit_and_publish(state, reload_guard, session, ctx, tenant_id, trace_id).await?;

    // The plaintext is shown ONCE, here, and never re-derivable. `key_hash` is
    // deliberately absent from the body as well: a display-only view of a key
    // needs the two atoms, not the digest (acceptance B2).
    respond_json(
        session,
        ctx,
        200,
        &json!({
            "id": key.id,
            "key": plaintext,
            "key_prefix": key.key_prefix,
            "key_last4": key.key_last4,
            "config_version": version,
        }),
    )
    .await
}

/// `DELETE /tenant/{tenant_id}/api/v1/users/{uid}/api-keys/{kid}`
///
/// BOTH ids are path segments, and the PAIRING decides the answer: a `kid` that
/// exists under ANOTHER user is the SAME 404 as a missing one, so this endpoint
/// cannot be used to probe whether a key id exists somewhere else (the
/// anti-enumeration property of decision D-5, kept on the write face too).
// Route params (user_id, key_id), request context, auth, and the reload guard
// are all distinct inputs; grouping them into a struct would obscure the
// anti-enumeration pairing documented above without changing the call shape.
#[allow(clippy::too_many_arguments)]
async fn user_api_key_delete(
    state: &AppState,
    reload_guard: &ReloadGuard<'_>,
    session: &mut Session,
    ctx: &mut RequestContext,
    auth: &Authenticated,
    user_id: &str,
    key_id: &str,
    trace_id: &str,
) -> pingora_core::Result<bool> {
    let tenant_id = &auth.tenant.id;
    let cfg = state.store.snapshot();
    let known = match cfg.users.get(user_id) {
        Some(u) => u.tenant_id == *tenant_id,
        None => false,
    };
    if !known {
        return super::respond_error(
            session,
            ctx,
            404,
            "not_found",
            "no such user in this tenant",
        )
        .await;
    }

    let pool = state.store.pool();
    // Ownership is a predicate of the DELETE itself, not a prior scan: a key
    // belonging to another user or another tenant deletes 0 rows, so the
    // 0-row result is BOTH the anti-enumeration answer and the race backstop
    // (no check-then-delete window). The snapshot lookup above only keeps the
    // distinct "no such user in this tenant" answer.
    let deleted = match crate::db::delete_user_api_key(pool, tenant_id, user_id, key_id).await {
        Ok(n) => n,
        Err(e) => {
            let (status, code) = crate::admin::handlers::classify_db_err(&e);
            return super::respond_error(
                session,
                ctx,
                status,
                code,
                "the api-key could not be revoked",
            )
            .await;
        }
    };
    if deleted == 0 {
        return super::respond_error(session, ctx, 404, "not_found", "no such key for this user")
            .await;
    }

    let _ = commit_and_publish(state, reload_guard, session, ctx, tenant_id, trace_id).await?;
    // Revocation is reported by absence: 204, no body, like every other
    // identity-plane DELETE here. An ABSENT id is answered by the pairing
    // above as 404 — a caller naming a key it does not own must not be told
    // the statement "succeeded".
    super::respond_raw(session, ctx, 204, Vec::new()).await
}

/// `PUT /tenant/{tenant_id}/api/v1/users/{uid}/billing`
///
/// Sets the欠费 dimension of a user row (plan §3.3): the 402 branch of the data
/// plane reads it back off the identity index, so this write is how an operator
/// (or a tenant on its OWN behalf) suspends and restores a user. It touches no
/// credential and no provider row.
async fn user_billing_set(
    state: &AppState,
    reload_guard: &ReloadGuard<'_>,
    session: &mut Session,
    ctx: &mut RequestContext,
    auth: &Authenticated,
    user_id: &str,
    trace_id: &str,
) -> pingora_core::Result<bool> {
    let tenant_id = &auth.tenant.id;
    let cfg = state.store.snapshot();
    // The row is decided on the SNAPSHOT the gate used, tenant-scoped: an
    // unknown `uid` and one belonging to ANOTHER tenant answer the same 404.
    let current: User = match cfg.users.get(user_id) {
        Some(u) if u.tenant_id == *tenant_id => u.clone(),
        _ => {
            return super::respond_error(
                session,
                ctx,
                404,
                "not_found",
                "no such user in this tenant",
            )
            .await;
        }
    };

    let raw = match super::read_body(session, trace_id).await {
        Ok(b) => b,
        Err((status, body)) => return super::respond_raw(session, ctx, status, body).await,
    };
    let req: BillingBody = match parse_body(&raw) {
        Ok(r) => r,
        Err(e) => {
            return super::respond_error(
                session,
                ctx,
                400,
                "invalid_request",
                &format!("request body is not valid JSON: {e}"),
            )
            .await;
        }
    };
    // Two spellings, validated by hand so the message NAMES them. Case is
    // normalised; surrounding whitespace is not significant.
    let status: BillingStatus = match req.status.trim().to_ascii_lowercase().as_str() {
        "active" => BillingStatus::Active,
        "delinquent" => BillingStatus::Delinquent,
        _ => {
            return super::respond_error(
                session,
                ctx,
                400,
                "invalid_request",
                "`status` must be \"active\" or \"delinquent\"",
            )
            .await;
        }
    };

    let changed_at = now_rfc3339();
    let mut updated = current;
    updated.billing_status = status;
    // `reason` REPLACES whatever was there (an absent `reason?` clears it):
    // the field is the operator's note about THIS state, not a log to append to.
    updated.billing_reason = req.reason;
    // `billing_updated_at` is always stamped — it is "when did the billing state
    // last change", which is what a suspended user's caller reconciles against.
    // (`updated_at` is set here for the same reason, but the DB stamps that
    // column itself with `CURRENT_TIMESTAMP`, so the value below is what THIS
    // response reports, not what a later read will show.)
    updated.billing_updated_at = Some(changed_at.clone());
    updated.updated_at = changed_at;

    let pool = state.store.pool();
    if let Err(e) = crate::db::upsert_user(pool, &updated).await {
        let (status_code, code) = crate::admin::handlers::classify_db_err(&e);
        return super::respond_error(
            session,
            ctx,
            status_code,
            code,
            "the billing state could not be stored",
        )
        .await;
    }

    let version =
        commit_and_publish(state, reload_guard, session, ctx, tenant_id, trace_id).await?;

    // The caller gets the row as it now stands, so the write is confirmable
    // without a second round-trip (and without a `GET /users` route, which this
    // node does not serve yet — see the T5 arms in [`super::dispatch`]).
    respond_json(
        session,
        ctx,
        200,
        &UserBillingView {
            config_version: version,
            user: updated,
        },
    )
    .await
}

// ---------------------------------------------------------------------------
// T6 write helpers
// ---------------------------------------------------------------------------

/// The reload tail every user-hierarchy write runs after its DB transaction has
/// committed. Reload is best-effort: the write already succeeded, so a reload
/// failure is logged and marks the store's snapshot stale (which drives the
/// `hydra_config_snapshot_stale` gauge) but does NOT fail the response. Returns
/// the version this node now serves.
///
/// The caller holds the store's reload guard across the write + this tail, so
/// this goes through the locked entry point and never re-takes the lock.
async fn commit_and_publish(
    state: &AppState,
    reload_guard: &ReloadGuard<'_>,
    _session: &mut Session,
    _ctx: &mut RequestContext,
    tenant_id: &str,
    trace_id: &str,
) -> pingora_core::Result<u64> {
    match state
        .store
        .reload_with(reload_guard, false, ReloadSource::Local)
        .await
    {
        Ok(changed) => tracing::debug!(
            target: "hydra::tenant_api",
            tenant = %tenant_id,
            trace_id = %trace_id,
            changed,
            "post-write reload tail complete"
        ),
        Err(e) => {
            tracing::error!(
                target: "hydra::tenant_api",
                tenant = %tenant_id,
                trace_id = %trace_id,
                error = %e,
                "post-write reload FAILED: the write IS committed but the in-memory config \
                 snapshot is now STALE (marked in the store, alert gauge set); the response \
                 remains successful and the change takes effect after a later reload succeeds"
            );
        }
    }
    Ok(state.store.version())
}

/// A fresh api-key plaintext: `hk_` + 32 CSPRNG bytes, base64url WITHOUT padding
/// (43 characters), per plan §3.1 / decision D-7.
///
/// `fill_bytes` comes from `rand`'s CSPRNG (`thread_rng`), not from a seeded or
/// hash-based source: a credential whose space can be walked is not a credential.
/// The result is returned to exactly one caller, in exactly one response body,
/// and is NEVER logged — the value that outlives this function is only its
/// SHA-256 digest plus the two display atoms.
#[must_use]
fn new_api_key_plaintext() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(bytes.as_mut());
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    format!("hk_{encoded}")
}

// ---------------------------------------------------------------------------
// V5 — the identity-plane writes (PUT/DELETE user groups, users, api-keys, billing)
// ---------------------------------------------------------------------------
//
// The self-service WRITE endpoints. The gate (token, URL cross-check, success budget)
// already ran in [`super::dispatch`], so the authenticated tenant IS the write target.
//
// Each arm dispatches to its own handler, which reads its OWN body (these bodies are
// several shapes), runs ONE statement against the shared database, and then runs the
// best-effort reload tail via [`commit_and_publish`].
//
// The tenant Bearer and the request body are NEVER logged here.

/// Default for the optional `enabled` field: `true`.
fn default_true() -> bool {
    true
}

/// A JSON body that is empty / whitespace-only is treated as `{}`.
fn parse_body<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, String> {
    let bytes = if body.is_empty() || body.iter().all(u8::is_ascii_whitespace) {
        b"{}"
    } else {
        body
    };
    serde_json::from_slice(bytes).map_err(|e| e.to_string())
}

/// The identity-plane write endpoints: gate (already done) -> handler on THIS node.
pub async fn write(
    state: &AppState,
    session: &mut Session,
    ctx: &mut RequestContext,
    auth: &Authenticated,
    route: TenantWriteRoute<'_>,
) -> pingora_core::Result<bool> {
    let trace_id = ctx.trace_id.clone();
    // ONE write at a time, from BEFORE the DB mutation through the reload tail:
    // the store's reload lock serialises the snapshot swap with concurrent writes.
    let _write_guard = state.store.reload_guard().await;
    match route {
        // T6 (D-7 + D-5): these write ROWS in the identity tables, not the config
        // tree, and the boundary they DO need (which user inside this tenant) is carried
        // on the route variant.
        TenantWriteRoute::CreateUserApiKey { user_id, .. } => {
            return user_api_key_create(
                state,
                &_write_guard,
                session,
                ctx,
                auth,
                user_id,
                &trace_id,
            )
            .await;
        }
        TenantWriteRoute::CreateUserApiKeysBulk { .. } => {
            return user_api_key_bulk_create(state, &_write_guard, session, ctx, auth, &trace_id)
                .await;
        }
        TenantWriteRoute::DeleteUserApiKey {
            user_id, key_id, ..
        } => {
            return user_api_key_delete(
                state,
                &_write_guard,
                session,
                ctx,
                auth,
                user_id,
                key_id,
                &trace_id,
            )
            .await;
        }
        TenantWriteRoute::SetUserBilling { user_id, .. } => {
            return user_billing_set(state, &_write_guard, session, ctx, auth, user_id, &trace_id)
                .await;
        }
        // T5 writes: the user-group and user rows. Like the identity writes above,
        // each handler reads its OWN body and runs its own statement plus
        // [`commit_and_publish`]. The id lives in the BODY for the two upserts (the
        // flat suffix parses above) and in the path for the two deletes, so only the
        // delete arms bind `id`.
        TenantWriteRoute::UpsertUserGroup { .. } => {
            return user_group_upsert(state, &_write_guard, session, ctx, auth, &trace_id).await;
        }
        TenantWriteRoute::UpsertUserGroupsBulk { .. } => {
            return user_group_bulk_upsert(state, &_write_guard, session, ctx, auth, &trace_id)
                .await;
        }
        TenantWriteRoute::DeleteUserGroup { id, .. } => {
            return user_group_delete(state, &_write_guard, session, ctx, auth, id, &trace_id)
                .await;
        }
        TenantWriteRoute::UpsertUser { .. } => {
            return user_upsert(state, &_write_guard, session, ctx, auth, &trace_id).await;
        }
        TenantWriteRoute::UpsertUsersBulk { .. } => {
            return user_bulk_upsert(state, &_write_guard, session, ctx, auth, &trace_id).await;
        }
        TenantWriteRoute::DeleteUser { id, .. } => {
            return user_delete(state, &_write_guard, session, ctx, auth, id, &trace_id).await;
        }
    }
}
