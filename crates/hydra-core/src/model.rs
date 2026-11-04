//! Domain entities — pure data structures (no logic).
//!
//! These mirror the SQLite schema in `dev-docs/design.md` §4.1. Timestamps are
//! stored as ISO-8601 `String` (the core has no `chrono` dependency by design;
//! the server translates to/from `DateTime` at the I/O boundary). `enabled`
//! and `status` use idiomatic Rust types (`bool`/`i32`); the DB-layer
//! mapping (INTEGER 0/1) lives in `hydra-server`.
//!
//! All entities derive `Clone, Debug, PartialEq, Eq, Serialize, Deserialize`
//! so they round-trip through JSON symmetrically (covered by T1.1).

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Provider family
// ---------------------------------------------------------------------------

/// An upstream LLM provider. `weight = 0` means soft-disabled (excluded from
/// candidates, see design §7.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    /// Stable provider keyword (globally unique).
    pub key: String,
    pub name: String,
    /// Backend base URL, e.g. `https://api.openai.com`.
    pub endpoint: String,
    /// Effective weight for SWRR. `0` = soft-disabled.
    pub weight: i32,
    pub created_at: String,
    pub updated_at: String,
    /// Max in-flight requests to this provider. `None` ⇒ use `ProxyConfig`
    /// default / unlimited (design-admission-queue §5). Opt-in per provider.
    #[serde(default)]
    pub max_concurrency: Option<u32>,
    /// Max requests waiting for a permit. `None` ⇒ default. `0` ⇒ fail-fast
    /// (no queue, 503/failover on cap).
    #[serde(default)]
    pub max_queue_depth: Option<u32>,
    /// Max wait in the queue before failover/503 (ms). `None` ⇒ default.
    #[serde(default)]
    pub queue_wait_timeout_ms: Option<u64>,
}

/// A model served by a provider. `status`: `1` online / `0` manually offline /
/// `-1` probe-offline (design §4.2). Only `status == 1` enters the candidate
/// set built into `ConfigData::models_by_key`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModel {
    pub id: String,
    /// Model keyword — the routing key (matched against the request's `model`).
    pub key: String,
    pub name: String,
    pub provider_id: String,
    pub status: i32,
}

/// A real api-key for a provider. The `api_key` field is held in memory only
/// (plaintext); it is persisted as AES-256-GCM ciphertext (see
/// `hydra-server::crypto`). Decryption happens at the DB boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderKey {
    pub id: String,
    pub provider_id: String,
    /// Plaintext in memory only (persisted as ciphertext at the DB boundary).
    /// Never serialized (`skip_serializing`) so the plaintext cannot leak into
    /// snapshots or any serialized form.
    ///
    /// **Required on deserialize** — deliberately NOT `serde(default)`: a body
    /// that omits this field must be rejected by the admin write boundary, never
    /// silently defaulted to `""`. `PUT /provider-keys/{id}` is an upsert, so a
    /// defaulted empty value would destroy a working provider credential and
    /// still answer 200.
    ///
    /// Admin responses re-expose the key only through [`ProviderKeyDto`]
    /// (always masked).
    #[serde(skip_serializing)]
    pub api_key: String,
    pub created_at: String,
}

/// The admin-API view of a [`ProviderKey`]: the api-key is ALWAYS the masked
/// form (never plaintext — P1-5). This decouples the response shape from
/// [`ProviderKey`], whose `api_key` is `skip_serializing` (the plaintext is
/// never serialized — it is sealed at the DB boundary and only re-exposed
/// through this masked DTO).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderKeyDto {
    pub id: String,
    pub provider_id: String,
    /// The masked api-key (never plaintext).
    pub api_key: String,
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// Tenant family
// ---------------------------------------------------------------------------

/// A tenant, bound to a domain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tenant {
    pub id: String,
    pub name: String,
    /// Domain (lowercased) that maps to this tenant. `localhost` is allowed.
    pub domain: String,
    pub cert_key: Option<String>,
    pub cert_file: Option<String>,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// Grants a tenant access to a provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantProvider {
    pub id: String,
    pub tenant_id: String,
    pub provider_id: String,
}

/// Access gate: a tenant may only use models listed here (design §7.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantModel {
    pub id: String,
    pub tenant_id: String,
    pub model_key: String,
}

// ---------------------------------------------------------------------------
// User hierarchy（四级：tenant -> user_group -> user -> api_key，plan §3.1）
//
// ADDITIVE ONLY (T1, decision D-17): these mirror the NEW tables created by
// migration `0014_user_hierarchy.sql`. The terminal deletion batch T10 lands
// `0015` together with the code removal.
// ---------------------------------------------------------------------------

/// A user's billing state (plan §3.2 / acceptance B4). The wire form is
/// snake_case, matching the SQLite `CHECK (billing_status IN
/// ('active','delinquent'))` domain. `Delinquent` ⇒ API-key auth answers 402
/// `user_delinquent` (never 401: the key exists and is valid, the account cannot
/// pay).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingStatus {
    /// Normal: the user pays.
    Active,
    /// 欠费: the user is over quota / suspended.
    Delinquent,
}

/// A user group: a tenant-scoped grouping of users (plan §3.1, acceptance A1).
/// `UNIQUE (tenant_id, name)` is enforced by SQLite; the loader keeps ALL rows
/// (incl. `enabled == false`) in `ConfigData::user_groups` so a disabled group
/// answers 403 `group_disabled` (known but disabled) rather than 401.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserGroup {
    pub id: String,
    pub tenant_id: String,
    pub name: String,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// A user within one tenant, belonging to exactly one group.
///
/// `billing_status` is the欠费 dimension (plan §3.2 / D-5): the identity index
/// deliberately carries it for disabled AND delinquent rows so replicas decide
/// 402/403 consistently. `external_id` is an optional operator-supplied id,
/// `UNIQUE (tenant_id, external_id)` in SQLite (NULL allowed, not deduplicated
/// there — SQLite treats NULLs as distinct, which matches "no external id").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub tenant_id: String,
    pub user_group_id: String,
    /// Optional operator-facing id.
    pub external_id: Option<String>,
    pub name: String,
    pub enabled: bool,
    pub billing_status: BillingStatus,
    /// Free-text reason set with the billing status (plan §3.3 PUT /billing
    /// `reason?`).
    pub billing_reason: Option<String>,
    /// When the billing status last changed (`None` = never explicitly set).
    pub billing_updated_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// A client API-Key owned by a user — the fourth level of the hierarchy.
///
/// ONLY the SHA-256 hex hash is stored (decision D-7): the plaintext is shown
/// once at creation and never recoverable. `key_prefix` / `key_last4` are display
/// atoms (audit-safe identity, e.g. `hk_6f2a…` + `9c1f`) — logs and error bodies
/// may name these, never the key (acceptance B2). The loader returns ALL rows
/// (incl. disabled/expired keys): a revoked key must read as "known → 401",
/// not "unknown", so anti-enumeration survives onto replicas (D-5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserApiKey {
    pub id: String,
    pub user_id: String,
    pub name: String,
    /// SHA-256 hex of the plaintext key (globally UNIQUE).
    pub key_hash: String,
    pub key_prefix: String,
    pub key_last4: String,
    pub enabled: bool,
    /// Expiry (RFC3339 UTC, normalized at write time, plan §3.1); `None` =
    /// never expires.
    pub expires_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// A tenant access token (plan §3.2/§3.3, acceptance A2): one of MANY
/// per-tenant credentials that gate the tenant self-service API. hydra generates
/// the plaintext (`ht_` + CSPRNG, decision D-18) and shows it exactly once;
/// only the hash + display atoms are stored. `token_hash` is globally UNIQUE,
/// so a cross-tenant hash collision is structurally impossible (the fail-closed
/// duplicate-hash branch of the old tenant-token gate can go away in T10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantAccessToken {
    pub id: String,
    pub tenant_id: String,
    pub name: String,
    /// SHA-256 hex of the plaintext token (globally UNIQUE).
    pub token_hash: String,
    pub token_prefix: String,
    pub token_last4: String,
    pub enabled: bool,
    /// Expiry (RFC3339 UTC); `None` = never expires.
    pub expires_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// A Global Role rule (plan §3.4, acceptance C1–C5): the five-dimensional key
/// `(tenant, user_group, user, api_key, model)` mapped to a SET of providers.
/// The literal `*` on a dimension means "any value on that dimension" (decision
/// D-2); a rule counts as a hit only when ALL five dimensions match. The
/// provider set lives in the `global_role_provider` table and is carried here as
/// `provider_ids` (the loader joins the two tables in Rust, §3.4).
///
/// `#[serde(default)]` on `provider_ids`: a serialized rule without the field
/// decodes to the EMPTY set rather than failing — an empty rule can match and
/// contribute nothing, which `config::validate` and the router both handle
/// (the union stays empty → legacy fallback).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalRole {
    pub id: String,
    pub name: String,
    pub tenant_id: String,
    pub user_group_id: String,
    pub user_id: String,
    pub api_key_id: String,
    pub model_key: String,
    /// The rule's provider set (empty = matches and contributes nothing).
    #[serde(default)]
    pub provider_ids: Vec<String>,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

// ---------------------------------------------------------------------------
// Limiting
// ---------------------------------------------------------------------------

/// A rate-limit role. Any `matching_*` of `None` means "match all" on that
/// dimension (design §10.1). `window` ∈ {"m","h","d"}.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitRole {
    pub id: String,
    pub name: String,
    pub matching_key: Option<String>,
    pub matching_model: Option<String>,
    pub matching_tenant: Option<String>,
    pub matching_provider: Option<String>,
    pub limit_count: Option<i64>,
    pub limit_token: Option<i64>,
    pub window: String,
    pub enabled: bool,
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// A single weighted candidate produced by `router::resolve`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub provider_id: String,
    /// Resolved host:port + scheme, parsed from `Provider::endpoint`.
    pub endpoint: String,
    pub weight: i32,
}

/// Why a provider that WOULD have served a request was dropped from the
/// candidate set (pipeline step 4). Attribution only: it never changes whether
/// the request succeeds, it only names the loser.
///
/// Exists because "this provider stopped being chosen" used to be invisible: the
/// only signal was a tenant-labelled `hydra_route_errors_total` on requests that
/// FAILED, so a provider dropping out of rotation while other providers still
/// served the tenant left no trace at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
// One string per variant: the metric labels below are a contract an operator
// writes alert rules against, so a serialized value must not be able to disagree
// with `as_str()` (`tenant_api.rs` sets the same precedent).
#[serde(rename_all = "snake_case")]
pub enum ExclusionReason {
    /// The breaker is open (too many recent failures). An anomaly: the provider
    /// is being routed around because it is failing.
    BreakerDead,
    /// No api-key rows: the provider cannot be called at all. An anomaly unless
    /// it was never configured.
    NoKey,
    /// `weight < 0`: a configuration ERROR that `config::validate` warns about.
    /// Counted as a fault, but DEFENSIVE: the schema forbids it
    /// (`migrations/0001_init.sql` has `CHECK (weight >= 0)`) and every provider
    /// in memory comes from those rows, so no running process produces this
    /// reason — see `admin::metrics::record_candidate_skipped`.
    InvalidWeight,
    /// `weight == 0`: a SUPPORTED, deliberate action ("soft disable", documented
    /// in `ops.md`): the configuration is kept, routing is stopped. Reported here
    /// for completeness, but the proxy deliberately does NOT count it — see
    /// `hydra_server::proxy::record_excluded_candidates`.
    SoftDisabled,
}

impl ExclusionReason {
    /// Stable label for metrics/logs, kept identical to the serialized form by
    /// `#[serde(rename_all = "snake_case")]`. These strings are part of the
    /// `hydra_candidate_skipped_total{reason=...}` contract.
    pub fn as_str(self) -> &'static str {
        match self {
            ExclusionReason::BreakerDead => "breaker_dead",
            ExclusionReason::NoKey => "no_key",
            ExclusionReason::InvalidWeight => "invalid_weight",
            ExclusionReason::SoftDisabled => "soft_disabled",
        }
    }
}

/// A provider dropped in step 4, with the reason.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExcludedCandidate {
    pub provider_id: String,
    pub reason: ExclusionReason,
}

/// The full result of [`crate::router::resolve_detailed`]: the candidates plus
/// the providers that were filtered out on the way.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveOutcome {
    pub candidates: Vec<Candidate>,
    /// Deterministically ordered by `provider_id` (same rule as `candidates`).
    pub excluded: Vec<ExcludedCandidate>,
}

/// Why routing failed. Maps to HTTP statuses by the proxy shell
/// (design §7.3): `ModelNotAllowed`→403, `ModelNotFound`→404,
/// `TenantForbidden`→403, `NoAvailableProvider`/`NoAvailableKey`→503.
///
/// Defined here (and re-exported from `router`) so the Router lane and the
/// shell share one canonical type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RouteError {
    /// `model_key` outside the tenant's configured `tenant_models` whitelist
    /// (only fires when the tenant HAS a mapping; no mapping = unrestricted).
    ModelNotAllowed,
    /// `model_key` unknown to the system (no online provider serves it).
    ModelNotFound,
    /// Tenant disabled / has no providers configured (tenant_providers gate
    /// is fail-closed; the tenant_models gate is default-open, see §7.1).
    TenantForbidden,
    /// Intersection of model-providers and tenant-providers empty, or all
    /// filtered out (dead / soft-disabled).
    NoAvailableProvider,
    /// All surviving candidates lack an api-key.
    NoAvailableKey,
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

/// Provider family, driving usage-schema normalisation (design §9.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderKind {
    OpenAi,
    Anthropic,
    /// Fallback for unknown providers (generic JSON `usage` object).
    Generic,
}

/// The client's request path selects the wire format end-to-end (design §9.4):
/// `/v1/messages` is Anthropic-native, everything else is OpenAI-compatible.
///
/// **Single owner for the rule.** Both the usage-scanner family (proxy shell)
/// and the upstream credential transport (`ProviderClient::build_request`) read
/// it, so the two can never drift apart.
///
/// Boundary (deliberate, unchanged): only the exact `/v1/messages` tail matches.
/// Sibling Anthropic endpoints (`/v1/messages/count_tokens`,
/// `/v1/messages/batches`) stay on the OpenAI-compatible branch.
#[must_use]
pub fn protocol_for_path(path: &str) -> ProviderKind {
    if path.ends_with("/v1/messages") {
        ProviderKind::Anthropic
    } else {
        ProviderKind::Generic
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_for_path_matches_only_the_v1_messages_tail() {
        assert_eq!(protocol_for_path("/v1/messages"), ProviderKind::Anthropic);
        assert_eq!(
            protocol_for_path("/gw/prefix/v1/messages"),
            ProviderKind::Anthropic
        );
        assert_eq!(
            protocol_for_path("/v1/chat/completions"),
            ProviderKind::Generic
        );
        assert_eq!(protocol_for_path("/v1/models"), ProviderKind::Generic);
        // Deliberate boundary: siblings are NOT Anthropic-native here.
        assert_eq!(
            protocol_for_path("/v1/messages/count_tokens"),
            ProviderKind::Generic
        );
        assert_eq!(
            protocol_for_path("/v1/messages/batches"),
            ProviderKind::Generic
        );
    }
}

/// Normalised, provider-neutral token usage. All fields optional: some
/// providers omit them (e.g. OpenAI without `stream_options.include_usage`).
///
/// Field names are deliberately neutral (not OpenAI/Anthropic-flavoured) so
/// the metering table has one stable schema regardless of upstream:
///
/// - `tokens_in` — tokens SENT in the request (all input, cache hits
///   included). Maps from OpenAI `prompt_tokens` / Anthropic `input_tokens`.
/// - `tokens_out` — tokens RETURNED by the model. Maps from OpenAI
///   `completion_tokens` / Anthropic `output_tokens`.
/// - `cache_hit_tokens` — tokens in the request that hit the prompt cache; a
///   SUBSET of `tokens_in`. Maps from OpenAI
///   `prompt_tokens_details.cached_tokens` / Anthropic
///   `cache_read_input_tokens`. `None` when the provider does not report it
///   (the dimension is simply absent, not zero).
///
/// There is deliberately NO `total_tokens` field: it is derivable
/// (`tokens_in + tokens_out`) and carries no billing meaning.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    /// Prompt-cache hit token count (subset of `tokens_in`). `None` when
    /// unreported.
    pub cache_hit_tokens: Option<u64>,
}

/// A persisted usage record (design §9.1). `created_at` is ISO-8601 text
/// (no `chrono` in core). The api-key is always the masked form (§9.5).
///
/// Latency dimensions:
/// - `latency_ms` — end-to-end wall clock (request start → response complete).
/// - `forward_latency_ms` — Hydra's own overhead: request start → just before
///   the upstream `send` (auth + routing + body read). `None` when no upstream
///   attempt was made.
/// - `ttft_ms` — Time To First Token: request start → first response chunk
///   received from the provider. `None` for non-streamed / errored requests
///   that never produced a chunk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRecord {
    pub tenant_id: String,
    pub provider_id: String,
    pub model_key: String,
    pub client_api_key_masked: Option<String>,
    /// WHO the request was attributed to, taken from the resolved
    /// [`hydra_core::identity::ApiPrincipal`] at record time (the internal
    /// api-key auth path) — NOT reconstructed from the stored masked key.
    /// The three levels of the user hierarchy (tenant → group → user → key)
    /// land side by side so a `/usage?group_by=` read can bucket on any of
    /// them without a join.
    ///
    /// `None` when the request was not resolved by that path (no principal
    /// ⇒ no attribution is invented; a missing dimension must stay NULL,
    /// never `""`, which is what the read side's grouping buckets reserve for
    /// "the key was known and had no value here").
    pub user_id: Option<String>,
    pub user_group_id: Option<String>,
    pub api_key_id: Option<String>,
    pub status_code: u16,
    /// Tokens sent in the request (all input, cache hits included).
    pub tokens_in: Option<u64>,
    /// Tokens returned by the model.
    pub tokens_out: Option<u64>,
    /// Prompt-cache hit tokens (subset of `tokens_in`; mirrors
    /// [`Usage::cache_hit_tokens`]).
    pub cache_hit_tokens: Option<u64>,
    pub latency_ms: u64,
    /// Hydra overhead: request start → just before upstream send.
    pub forward_latency_ms: Option<u64>,
    /// Time To First Token: request start → first response chunk.
    pub ttft_ms: Option<u64>,
    pub upstream_host: Option<String>,
    pub error: Option<String>,
    pub trace_id: String,
    pub created_at: String,
}
