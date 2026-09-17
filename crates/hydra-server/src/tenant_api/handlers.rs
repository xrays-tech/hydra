//! Endpoint handlers for the tenant self-service API.
//!
//! Each handler is a thin composition over an owner that already exists:
//!
//! | endpoint | delegates to |
//! |---|---|
//! | `whoami` (T5) | the config snapshot, through the row the gate already resolved |
//! | `invalidate` (T6) | [`crate::http::AuthCache`] plus the invalidation stream and its barrier |
//! | `usage` (T8) | [`crate::usage_query::UsageQuery`] |
//!
//! Nothing here re-implements a primitive: routing and the gate live in
//! [`super`], the response envelope in [`super::respond_json`].

use pingora_proxy::Session;
use serde::Serialize;

use crate::proxy::ctx::RequestContext;
use crate::tenant_api::{respond_json, Authenticated};

/// The body of `GET /whoami`.
///
/// Every field is either the tenant's own non-secret configuration or a fact the
/// caller cannot otherwise see. In particular there is **no** token digest, no
/// certificate material and no provider key — a tenant reading its own state must
/// not be handed anything it could use against another tenant, and the fields it
/// does get (`domain`, `auth_url`) are the ones it would otherwise have to open a
/// support ticket to learn.
#[derive(Serialize)]
struct Whoami<'a> {
    tenant_id: &'a str,
    name: &'a str,
    enabled: bool,
    /// The domain this tenant is currently bound to — i.e. which `Host` traffic
    /// reaches it. Read-only here: changing it is an operator action.
    domain: &'a str,
    /// Where Hydra sends this tenant's clients' api-keys for verification.
    /// Read-only here for the same reason.
    auth_url: &'a str,
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
/// no external auth. That is what makes it work identically on a leader, a
/// standby and an edge node, and it is why it costs nothing to serve — an edge
/// with no local DB answers it, which no other endpoint can.
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
        auth_url: &t.auth_url,
        // The version the GATE read, not a fresh one: a response must never be
        // attributed to a configuration newer than the one that authorised it.
        config_version: auth.config_version,
        base_url: format!("{}{}/api/v1", crate::tenant_api::RESERVED_PREFIX, t.id),
    };
    respond_json(session, ctx, 200, &body).await
}
