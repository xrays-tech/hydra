//! In-memory configuration model + load-time validation (pure).
//!
//! `ConfigData` is the hot-read aggregate built by the loader (W2) and held
//! behind an `ArcSwap` on the *server* side (`ConfigStore`). Here in core it is
//! just plain data: `Clone`-able, buildable by hand in tests (T1.2), indexable
//! by the pure `router::resolve`.
//!
//! ## Concurrency boundary
//!
//! Per `dev-docs/waves/wave-1-pure-core.md` §3.1, the concurrency wrappers
//! (`ArcSwap`, `DashMap`, `Arc<CircuitBreaker>`) do **not** live in core. In
//! particular `certs` is a plain `HashMap<String, CertMeta>` here; the server
//! wraps the whole `ConfigData` (and, for independent cert hot-reload per
//! design §5.2/§12.1, the certs map specifically) in `ArcSwap` at the boundary.
//! This keeps `arc-swap` out of core's dependency firewall (dev-plan §2).
//!
//! ## Validation scope (design §5.4)
//!
//! [`validate`] covers the **pure** data-graph invariants only — referential
//! integrity between the in-memory indexes plus structural sanity. The
//! I/O-dependent checks from §5.4 (endpoint-URL parseability / scheme legality,
//! cert-file readability & PEM validity, public/private-key match) are
//! **deferred to the W2 loader**: they require network/filesystem access and
//! therefore cannot live in this zero-I/O crate. The [`Severity::Fatal`]
//! variant is reserved for those loader-side fatal checks; everything the pure
//! [`validate`] emits today is [`Severity::Warn`].

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::identity::{ApiKeyIdentity, ApiPrincipal, TenantTokenIdentity};
use crate::model::{GlobalRole, User, UserApiKey, UserGroup};
use crate::model::{LimitRole, Provider, ProviderKeyBinding, SubTenant, SubTenantRoute, Tenant};

/// In-memory configuration snapshot. All indexes are built once at load time
/// and read lock-free thereafter (the server holds it inside `ArcSwap`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigData {
    /// `domain` (lowercase) → tenant (incl. the `localhost` special case).
    pub tenants_by_domain: HashMap<String, Tenant>,

    /// `tenant_id` → tenant. **派生索引**，与 `tenants_by_domain` 同源（同一次
    /// 加载、同一批行），供"由令牌反查租户"的 O(1) 取用。
    ///
    /// **不上集群线缆**（`serde(skip)`）：它是派生值，副本侧由
    /// [`ConfigData::reindex_tenants`] 从 `tenants_by_domain` 重建 —— 传输它既会让
    /// 每份快照把租户行传两遍，也会引入"旧 leader / 新副本"的字段缺失问题
    /// （缺字段时若静默成空索引，有效令牌会被判 403）。
    ///
    /// **只有一个写入口**：[`ConfigData::reindex_tenants`]。别处写它会让
    /// `ReplicationContent` 的 `PartialEq` 代际判定出现"无源变化"
    /// （`ReplicationContent` 持 `Arc<ConfigData>`，而本结构体 derive `PartialEq`）。
    #[serde(skip, default)]
    pub tenants_by_id: HashMap<String, Tenant>,

    /// `model_key` → online providers serving it (`provider_id` + weight).
    /// Only `provider_model.status == 1` entries are included by the loader.
    pub models_by_key: HashMap<String, Vec<ModelProvider>>,

    /// `tenant_id` → set of allowed `provider_id`s.
    pub tenant_providers: HashMap<String, HashSet<String>>,

    /// `tenant_id` → set of allowed `model_key`s (the access gate, §7.1).
    pub tenant_models: HashMap<String, HashSet<String>>,

    /// `provider_id` → provider (incl. endpoint / weight).
    pub providers: HashMap<String, Provider>,

    /// `provider_id` → non-empty list of api-keys (runtime picks one at random).
    pub provider_keys: HashMap<String, Vec<String>>,

    /// Enabled limit roles (priority order decided by the loader).
    pub limit_roles: Vec<LimitRole>,

    /// Enabled api-key-prefix → provider bindings (design §7.1b; only
    /// `enabled == true` rows, like `limit_roles`). Matching is longest-prefix
    /// wins; see [`crate::router::match_key_binding`].
    pub key_prefix_bindings: Vec<ProviderKeyBinding>,

    /// Enabled sub-tenants (design-sub-tenant.md §3.1). These carry the
    /// `key_prefix` — the source of the api-key-prefix routing gate (step 3.6).
    /// Only `enabled == true` rows (mirrors `key_prefix_bindings`).
    pub sub_tenants: Vec<SubTenant>,

    /// Enabled sub-tenant routes (design-sub-tenant.md §3.1). Each row pins a
    /// sub-tenant's traffic to a single provider, optionally scoped to a model.
    /// Only `enabled == true` rows (mirrors `key_prefix_bindings`).
    pub sub_tenant_routes: Vec<SubTenantRoute>,

    // --- User hierarchy + internal auth (plan §3.1/§3.2; T1 additive) -----
    //
    // These back the FOUR-level identity (tenant -> user_group -> user -> api_key)
    // and the multi-token tenant API gate. Unlike the legacy auth path, NOTHING
    // here is decided by an outbound call: the identity index is built ONCE per
    // snapshot by [`ConfigData::reindex_identity`] and read lock-free.
    /// ALL user groups — incl. `enabled == false` (plan §3.2: a disabled group
    /// answers 403 `group_disabled`, which requires the row to be PRESENT, not
    /// absent; absence would read as "unknown group" and collapse into the 401
    /// family).
    pub user_groups: Vec<UserGroup>,

    /// `user_id` → ALL users — incl. disabled AND delinquent (decision D-5: the
    /// billing state is a business fact the 402 branch needs, and the row itself
    /// is what anti-enumeration rests on).
    pub users: HashMap<String, User>,

    /// `sha256(client api-key)` → identity record. **派生索引** — see
    /// [`ConfigData::reindex_identity`], its single write entry point.
    ///
    /// **不上集群线缆**（`serde(skip)`）: built from the FULL `users` + the FULL
    /// `user_api_key` rows (incl. disabled/expired keys, disabled users/groups),
    /// so a revoked key on a replica is "known → 401", never "unknown" — the same
    /// source-of-truth rule as `tenants_by_id`: leader (`build_config`) and replica
    /// (tree decode, plan §3.5) derive it with ONE implementation, and a missing
    /// field on the wire must never degrade a valid key to 401.
    #[serde(skip, default)]
    pub users_by_api_key_hash: HashMap<String, ApiKeyIdentity>,

    /// `sha256(tenant access token)` → `{tenant_id, token_name}`. ENABLED rows
    /// ONLY (plan §3.2: unlike the api-key index, a revoked tenant token is simply
    /// GONE — the token index is not an enumeration surface, `whoami` returns the
    /// `token_name`, never any plaintext).
    pub tenant_tokens: HashMap<String, TenantTokenIdentity>,

    /// ENABLED Global Role rules, `provider_ids` joined in from
    /// `global_role_provider` (plan §3.4). Disabled rules are NOT inert-but-known
    /// (they must match NOTHING, not even as failover candidates), so — unlike
    /// the identity rows — only enabled ones load.
    pub global_roles: Vec<GlobalRole>,

    /// `tenant_id` → indexes into `global_roles`. **派生索引** — see
    /// [`ConfigData::reindex_global_roles`], its single write entry point; `serde(skip)`
    /// for the same reason as every other derived index.
    #[serde(skip, default)]
    pub global_roles_by_tenant: HashMap<String, Vec<usize>>,

    /// Indexes into `global_roles` whose `tenant_id` is the `*` wildcard (decision
    /// D-3: a wildcard-tenant rule must still intersect `tenant_providers` —
    /// fail-closed against one broad rule re-routing every tenant).
    #[serde(skip, default)]
    pub global_roles_any_tenant: Vec<usize>,

    /// `domain` → certificate metadata. Plain value here (see module docs);
    /// W1–W2 carries `CertMeta`, W4 resolves to a parsed `ResolvedCert` on the
    /// server side while this map remains the single source of truth.
    pub certs: HashMap<String, CertMeta>,
}

impl ConfigData {
    /// 从 `tenants_by_domain` 重建 `tenants_by_id`（唯一写入口）。
    ///
    /// 纯函数：无 I/O、不确定输入之外的状态。两个调用点都必须经过它 ——
    /// `store.rs::build_config`（节点从自己的数据库加载）与
    /// `cluster/snapshot.rs` 的物化路径（副本从控制面重建之后）。因为它们共用同一个实现，
    /// 派生规则不可能分叉，也不需要 `WIRE_VERSION` 变更：`serde(skip)` 之后两侧都由本函数产生。
    ///
    /// 空的 `tenants_by_domain`（尚未物化出任何配置的节点）产生空索引，这是正确的：
    /// 此时 `ConfigStore::version()` 为 0（"什么都还没应用"），令牌闸门回 503 `not_ready`，
    /// 而不是把有效令牌误判为 403 —— 也不是 401，那等于说"你的令牌是错的"。
    /// （2026-10-05 更正：原文写的是"`replication()` 为 `None`"，那是已删除的 edge store 的形状；
    /// 条件本身没变，变的是它现在由版本水位表示。）
    pub fn reindex_tenants(&mut self) {
        self.tenants_by_id.clear();
        self.tenants_by_id.extend(
            self.tenants_by_domain
                .values()
                .map(|t| (t.id.clone(), t.clone())),
        );
    }

    /// Rebuild `users_by_api_key_hash` from `self.users` + the FULL
    /// `user_api_key` rows — the ONE write entry point for the identity index
    /// (plan §3.2 "身份索引推导（承重规则）").
    ///
    /// Same-shape rule as [`reindex_tenants`]: both the leader loader
    /// (`store.rs::build_config`) and the replica materializer (plan §3.5, after
    /// tree decode) call THIS implementation, so the derived index cannot fork
    /// between the two, and the wire never carries it.
    ///
    /// `keys` MUST be the FULL table (incl. `enabled = false` / expired rows),
    /// and `self.users` / the group dimension likewise: a disabled key stays KNOWN
    /// (401 `invalid_api_key`, per the truth table), a disabled user/group is
    /// distinguishable at auth time (403), and a delinquent user is recorded on
    /// the principal (402). This is decision D-5: revocation must not become
    /// indistinguishable from never-having-existed, or every replica can enumerate
    /// live keys.
    ///
    /// A key whose `user_id` is absent from `self.users` is an ORPHAN the schema
    /// FK should prevent (`user_api_key.user_id REFERENCES user(id)`); such a key
    /// cannot yield a principal — it is SKIPPED here (unknown key → 401 either
    /// way) rather than fabricating an identity. Core has no logger, so the skip
    /// is silent; the server-side loader is the place that would report it.
    pub fn reindex_identity(&mut self, keys: &[UserApiKey]) {
        self.users_by_api_key_hash.clear();
        let group_enabled: HashMap<String, bool> = self
            .user_groups
            .iter()
            .map(|g| (g.id.clone(), g.enabled))
            .collect();
        for k in keys {
            let Some(u) = self.users.get(&k.user_id) else {
                continue;
            };
            self.users_by_api_key_hash.insert(
                k.key_hash.clone(),
                ApiKeyIdentity {
                    principal: ApiPrincipal {
                        tenant_id: u.tenant_id.clone(),
                        user_group_id: u.user_group_id.clone(),
                        user_id: u.id.clone(),
                        api_key_id: k.id.clone(),
                        billing_status: u.billing_status,
                    },
                    key_enabled: k.enabled,
                    user_enabled: u.enabled,
                    group_enabled: group_enabled
                        .get(&u.user_group_id)
                        .copied()
                        .unwrap_or(false),
                    expires_at: k.expires_at.clone(),
                },
            );
        }
    }

    /// Rebuild the Global Role indexes (`global_roles_by_tenant` /
    /// `global_roles_any_tenant`) from `self.global_roles` — the ONE write entry
    /// point for both (same derived-index rule as [`reindex_tenants`]: `serde(skip)`,
    /// loader + replica share this implementation).
    ///
    /// ENABLED rules only, and the index stores INDICES into `global_roles` (plan
    /// §3.2), so the matcher (`router` lane, T6/T7) reads the role bodies from the
    /// single vector. A rule whose `tenant_id` is the `*` wildcard goes to
    /// `global_roles_any_tenant`; matching a request takes `by_tenant[tenant]`
    /// PLUS `any_tenant` (plan §3.4 "先取 … + …, 逐条比较五维").
    pub fn reindex_global_roles(&mut self) {
        self.global_roles_by_tenant.clear();
        self.global_roles_any_tenant.clear();
        for (idx, role) in self.global_roles.iter().enumerate() {
            if !role.enabled {
                continue;
            }
            if role.tenant_id == "*" {
                self.global_roles_any_tenant.push(idx);
            } else {
                self.global_roles_by_tenant
                    .entry(role.tenant_id.clone())
                    .or_default()
                    .push(idx);
            }
        }
    }
}

/// One row of `models_by_key`: which provider serves a model and at what weight.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelProvider {
    pub provider_id: String,
    pub weight: i32,
}

/// Globally-resolved concurrency policy for one provider — concrete values
/// after defaults are applied (design-admission-queue §5).
///
/// Lives in the pure core (no `tokio`/`Semaphore`); the concurrent shell
/// (`hydra-server::proxy::admission`) consumes it.
///
/// Field semantics (matching the `Provider` overrides):
/// - `max_concurrency == 0` ⇒ **unlimited** (do not gate this provider — the
///   admission shell short-circuits and returns a no-op permit). This is the
///   safe default / opt-out path.
/// - `max_queue_depth == 0` ⇒ **fail-fast** on cap (no queue; an attempt that
///   finds no free permit immediately returns `QueueFull`).
/// - `queue_wait_timeout_ms` ⇒ bounded wait before a queued request gives up
///   with `WaitTimeout`. Must be `> 0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConcurrencyPolicy {
    /// 0 ⇒ unlimited (no gating).
    pub max_concurrency: u32,
    /// 0 ⇒ fail-fast on cap (no queue).
    pub max_queue_depth: u32,
    pub queue_wait_timeout_ms: u64,
}

/// Resolve a provider's optional overrides against global defaults, field by
/// field (design-admission-queue §5: `provider.x.unwrap_or(defaults.x)`).
///
/// `None` on every provider field is the documented opt-out — the result equals
/// `defaults`, which the caller can set to "do not gate" (`max_concurrency == 0`).
#[must_use]
pub fn resolve_policy(
    max_concurrency: Option<u32>,
    max_queue_depth: Option<u32>,
    queue_wait_timeout_ms: Option<u64>,
    defaults: ConcurrencyPolicy,
) -> ConcurrencyPolicy {
    ConcurrencyPolicy {
        max_concurrency: max_concurrency.unwrap_or(defaults.max_concurrency),
        max_queue_depth: max_queue_depth.unwrap_or(defaults.max_queue_depth),
        queue_wait_timeout_ms: queue_wait_timeout_ms.unwrap_or(defaults.queue_wait_timeout_ms),
    }
}

/// Certificate metadata carried in the config snapshot (cluster P0a).
///
/// Two forms, resolved content-first:
/// - **content** (`cert_pem` / `cert_key_pem`): PEM text in memory (migration
///   0007). The private key is sealed at rest in the DB and decrypted locally
///   by the shell; only plaintext-in-memory lives in this struct (same
///   positioning as `ProviderKey::api_key`). This is the multi-node form — no
///   files, no shared volume.
/// - **legacy paths** (`cert_file` / `cert_key`): pre-0007 single-node rows.
///   Kept for read compatibility; the loader falls back to them when no
///   content is present.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertMeta {
    pub domain: String,
    /// Legacy cert PEM path (pre-0007). `None` in content mode.
    pub cert_file: Option<String>,
    /// Legacy cert key PEM path (pre-0007). `None` in content mode.
    pub cert_key: Option<String>,
    /// Public cert PEM content (migration 0007 primary form).
    #[serde(default)]
    pub cert_pem: Option<String>,
    /// Private key PEM content, plaintext in memory only. Never persisted as
    /// plaintext; never serialised (skip on serialize) — it is only ever
    /// re-derived at the DB boundary, so it must not leak into any serialized
    /// form (admin responses, snapshots). Defaults to `None` when absent.
    #[serde(default, skip_serializing)]
    pub cert_key_pem: Option<String>,
}

// ---------------------------------------------------------------------------
// Load-time validation (pure data-graph checks only — see module docs).
// ---------------------------------------------------------------------------

/// How serious a [`ValidationIssue`] is.
///
/// `Fatal` is reserved for loader-side (W2) I/O checks — endpoint-URL
/// parseability, cert-file readability, PEM/key validity (design §5.4) — which
/// cannot run in this zero-I/O crate. The pure [`validate`] below currently
/// emits only `Warn`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Severity {
    /// Hard failure: the loader must refuse to publish this snapshot.
    Fatal,
    /// Recoverable defect: publish, but the affected rows are inert/filtered.
    Warn,
}

/// One problem found while validating a [`ConfigData`] snapshot (design §5.4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationIssue {
    pub severity: Severity,
    pub message: String,
    /// WHICH entity the issue is about, when that is known (decision D-17): the admin write path
    /// returns the warnings for the row it just wrote, and it can only filter by subject if the issue
    /// says which row it belongs to. `None` means "about the configuration as a whole".
    pub subject: Option<String>,
}

impl ValidationIssue {
    fn warn(message: String) -> Self {
        Self {
            severity: Severity::Warn,
            message,
            subject: None,
        }
    }

    /// A warning about ONE entity. The subject is what lets `POST/PUT /api/v1/limit-roles` answer with
    /// "and here is what is wrong with what you just wrote" instead of leaving it in the leader's log
    /// (decision D-17).
    fn warn_about(subject: String, message: String) -> Self {
        Self {
            severity: Severity::Warn,
            message,
            subject: Some(subject),
        }
    }
}

/// Validate the pure, in-memory data-graph invariants of a config snapshot
/// (design §5.4).
///
/// The checks performed here are exactly those that need **no I/O**:
///
/// - **Referential integrity (tenant_providers)** — every `provider_id` listed
///   in `tenant_providers` exists in `providers` (`Warn`).
/// - **Referential integrity (tenant_models)** — every `model_key` listed in
///   `tenant_models` is offered by at least one online provider, i.e. present
///   in `models_by_key` with a non-empty candidate list (`Warn`).
/// - **Provider keys** — every *online* provider (`weight != 0`) has a
///   non-empty key list in `provider_keys` (`Warn`). Soft-disabled providers
///   (`weight == 0`) are skipped: they never become candidates.
/// - **Limit roles** — no enabled role has both `limit_count` and `limit_token`
///   `None` (a role matching nothing on either dimension is meaningless)
///   (`Warn`).
///
/// The I/O-dependent §5.4 checks (endpoint-URL parsing, cert-file existence /
/// PEM validity) are the W2 loader's responsibility — see the module docs.
///
/// Returns a deterministically ordered `Vec` (sorted by message, then severity)
/// so callers and tests get a stable result despite `HashMap` iteration order.
/// A clean config yields an empty `Vec`.
pub fn validate(cfg: &ConfigData) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();

    // tenant_providers → must reference known providers.
    for (tenant_id, provider_ids) in &cfg.tenant_providers {
        for pid in provider_ids {
            if !cfg.providers.contains_key(pid) {
                issues.push(ValidationIssue::warn(format!(
                    "tenant_provider references unknown provider_id '{pid}' (tenant '{tenant_id}')"
                )));
            }
        }
    }

    // tenant_models → must be offered by ≥1 online provider.
    for (tenant_id, model_keys) in &cfg.tenant_models {
        for key in model_keys {
            // `missing`: true when the model has NO online provider (empty
            // candidate list, or the key is unknown at all) — i.e. it must be
            // flagged.
            let missing = match cfg.models_by_key.get(key) {
                Some(v) => v.is_empty(),
                None => true,
            };
            if missing {
                issues.push(ValidationIssue::warn(format!(
                    "tenant_model '{key}' has no online provider (tenant '{tenant_id}')"
                )));
            }
        }
    }

    // Online providers → must have ≥1 api_key. "Online" is `weight > 0`, which is
    // what `router::resolve` and `router::accessible_models` filter on (`<= 0` is
    // dropped). This test used `weight == 0`, so a NEGATIVE weight — accepted by
    // the DB (`CHECK (weight >= 0)` only forbids... nothing negative is enforced
    // elsewhere) — skipped validation while being silently discarded at routing
    // time: a provider that can never serve, with a clean bill of health.
    for provider in cfg.providers.values() {
        if provider.weight < 0 {
            // Negative weights are dropped by the router (`weight > 0`), so such
            // a provider can never serve — say so, instead of passing it as
            // "online" and leaving the operator with a provider that is quietly
            // never selected.
            issues.push(ValidationIssue::warn(format!(
                "provider '{}' has weight {}; the router requires weight > 0, so it can never \
                 be selected",
                provider.id, provider.weight
            )));
        }
        if provider.weight <= 0 {
            continue;
        }
        let has_keys = cfg
            .provider_keys
            .get(&provider.id)
            .is_some_and(|v| !v.is_empty());
        if !has_keys {
            issues.push(ValidationIssue::warn(format!(
                "provider '{}' has weight {} but no api_keys; it will be filtered out at candidate time",
                provider.id, provider.weight
            )));
        }
    }

    // limit roles → must constrain at least one dimension.
    for role in &cfg.limit_roles {
        if role.limit_count.is_none() && role.limit_token.is_none() {
            issues.push(ValidationIssue::warn_about(
                role.id.clone(),
                format!(
                    "limit_role '{}' has both limit_count and limit_token NULL",
                    role.id
                ),
            ));
        }
        // `matching_provider` can never match: the pre-gate builds its `MatchCtx`
        // BEFORE routing, so `provider` is always `None` there, while
        // `limit::dim_matches` requires equality with the configured value. A role
        // that declares this dimension is therefore skipped by BOTH checks — it is
        // listed, persisted and shown in the admin UI, and it enforces nothing.
        // (The accounting path DOES pass a provider, so such a role also produces a
        // window that is written and never read.) This is a WARNING rather than a
        // hard error on purpose: configs that predate the decision below must not stop
        // validating, but the trap must stop being silent.
        //
        // Decision D-11 (2026-10-09): the dimension is REJECTED at the admin write boundary —
        // `POST /limit-roles` and `PUT /limit-roles/:id` answer 400
        // (`matching_provider_cannot_match`) instead of persisting a role that can never fire.
        // This Warn remains the defense for what that boundary cannot see: rows already in a
        // legacy database, file-loaded configs and restored snapshots still validate (and warn) —
        // they just can no longer be WRITTEN through the admin API.
        if role.matching_provider.is_some() {
            issues.push(ValidationIssue::warn_about(role.id.clone(), format!(
                "limit_role '{}' declares matching_provider '{}', which CANNOT match: the                  limit pre-gate runs before routing, so no provider is known yet and this                  role is skipped entirely (its limits never apply)",
                role.id,
                role.matching_provider.as_deref().unwrap_or_default()
            )));
        }
        // A key-scoped role with NO tenant scope is a CROSS-TENANT quota.
        //
        // Measured 2026-09-30 (round 116 made the raw-key form match, so roles that used to be
        // inert went live): `matching_key` alone constrains nothing else, so the window belongs to
        // `(role_id, mask(key))` regardless of which tenant presented the key — two tenants whose
        // auth backends both accept the same key string share ONE budget, and the second tenant's
        // very first request can be refused by the first tenant's usage. The bucket has always
        // behaved this way; what changed is that the documented form now actually matches, and
        // `config::validate` warned about the inert `matching_provider` dimension while saying
        // nothing about this one. A WARNING, not an error: a deliberately shared quota between two
        // tenants is legitimate, it just must not be accidental or silent.
        if role.matching_key.is_some() && role.matching_tenant.is_none() {
            issues.push(ValidationIssue::warn_about(role.id.clone(), format!(
                "limit_role '{}' scopes on matching_key but has matching_tenant NULL: its window is \
                 shared by EVERY tenant that accepts that key (the bucket is (role_id, digest(key))), \
                 so one tenant's traffic can exhaust another's budget",
                role.id
            )));
        }
        // What the VALUE of `matching_key` costs — one warning per role, because the forms have
        // different costs and the documentation alone was the only place any of them was mentioned.
        if let Some(key) = role.matching_key.as_deref() {
            // `is_key_digest` is load-bearing (2026-10-08, decision D-16③): a `sha256:<hex>` value is
            // neither `mask_key(k) == k` nor a raw key, so without it this branch warned about the very
            // form it now RECOMMENDS. Caught by the test, not by reading — the warning text below says
            // "write the digest form" while the guard fired ON the digest form.
            if !key.is_empty()
                && crate::rewrite::mask_key(key) != key
                && !crate::limit::is_key_digest(key)
            {
                // A non-mask, non-digest value is (by the matching rules) a raw client key.
                //
                // The wording changed on 2026-10-08 with decision D-16: the column is now SEALED at
                // rest and in the config tree, so "kept and replicated in PLAINTEXT" became false —
                // and a warning that overstates its case is how operators learn to disbelieve the
                // list. What remains true, and is why the warning stays: the key is RECOVERABLE by
                // whatever holds the master key, and the admin API still hands it back in the clear.
                issues.push(ValidationIssue::warn_about(role.id.clone(), format!(
                    "limit_role '{}' stores what looks like a RAW client key in `matching_key`: the \
                     column is sealed at rest and in the config tree (decision D-16), so the key is no \
                     longer readable in a database file or a backup by itself — but it IS recoverable \
                     by anything holding the master key, and `GET /api/v1/limit-roles` returns it in \
                     the clear (the admin UI renders it), so the credential still leaves the node in \
                     every admin response. Write the DIGEST form instead (`sha256:<64 hex>`, decision \
                     D-16③): it matches the same key and stores nothing recoverable at all, not even \
                     under the master key. The MASK form also works (it is what the usage rows carry) \
                     and, since D-15②, two keys that share a mask no longer share a window either",
                    role.id
                )));
            }
            // The mask-collision warning that used to sit here is DELETED (2026-10-08, decision
            // D-15②): it said that any other key whose mask is the same string shares this window,
            // because the window was `(role_id, mask(key))`. The window is now keyed by a DIGEST of
            // the raw key, so two keys that share a mask have two windows — the warning's condition
            // cannot occur, and a warning that cannot fire is noise that trains operators to skip the
            // list. `hydra-core/tests/validate.rs` pins its absence so it cannot come back by accident.
        }
    }

    // provider_key_bindings → prefix must be USABLE (the shared shape rule; the
    // admin write boundary enforces the same one) + provider must exist +
    // the prefix must not swallow a sub-tenant prefix.
    for b in &cfg.key_prefix_bindings {
        match crate::sub_tenant::validate_prefix_shape(&b.key_prefix) {
            Ok(()) => {}
            // Keep the original wording: runbooks and the alert text quote it.
            Err(crate::sub_tenant::PrefixShapeError::Empty) => {
                issues.push(ValidationIssue::warn(format!(
                    "provider_key_binding '{}' has an empty key_prefix; it can never match",
                    b.id
                )));
            }
            Err(e) => issues.push(ValidationIssue::warn(format!(
                "provider_key_binding '{}' has an unusable key_prefix '{}': {e}; a binding \
                 match is GLOBAL and wins over the sub-tenant routing gate, so a bare prefix \
                 re-routes every tenant whose key starts with it",
                b.id, b.key_prefix
            ))),
        }
        if !cfg.providers.contains_key(&b.provider_id) {
            issues.push(ValidationIssue::warn(format!(
                "provider_key_binding '{}' references unknown provider_id '{}'",
                b.id, b.provider_id
            )));
        }
        // Cross-namespace collision: a binding that overlaps a sub-tenant prefix
        // silently disables that sub-tenant's steering (the binding wins), so it
        // is exactly the kind of overlap the sub-tenant write path rejects — but
        // a binding can be created AFTER the sub-tenant exists, which no write
        // boundary can see. Surface it here instead of at request time.
        for st in &cfg.sub_tenants {
            if crate::sub_tenant::prefixes_collide(&st.key_prefix, &b.key_prefix) {
                issues.push(ValidationIssue::warn(format!(
                    "provider_key_binding '{}' (key_prefix '{}') overlaps sub_tenant '{}' \
                     (key_prefix '{}'); the binding wins, so that sub-tenant's routing is \
                     silently bypassed",
                    b.id, b.key_prefix, st.id, st.key_prefix
                )));
            }
        }
    }

    // sub_tenants → key_prefix must be non-empty and contain a separator
    // (`_` or `-`); a bare prefix would swallow longer, unrelated prefixes.
    for st in &cfg.sub_tenants {
        if st.key_prefix.is_empty() || !st.key_prefix.contains(['_', '-']) {
            issues.push(ValidationIssue::warn(format!(
                "sub_tenant '{}' has an invalid key_prefix '{}'; it must be \
                 non-empty and contain a separator ('_' or '-')",
                st.id, st.key_prefix
            )));
        }
    }

    // sub_tenant_routes → the referenced sub-tenant must exist and the
    // provider must exist (referential-integrity orphans, Warn).
    let sub_tenant_ids: HashSet<&str> = cfg.sub_tenants.iter().map(|st| st.id.as_str()).collect();
    for r in &cfg.sub_tenant_routes {
        if !sub_tenant_ids.contains(r.sub_tenant_id.as_str()) {
            issues.push(ValidationIssue::warn(format!(
                "sub_tenant_route '{}' references unknown sub_tenant_id '{}'",
                r.id, r.sub_tenant_id
            )));
        }
        if !cfg.providers.contains_key(&r.provider_id) {
            issues.push(ValidationIssue::warn(format!(
                "sub_tenant_route '{}' references unknown provider_id '{}'",
                r.id, r.provider_id
            )));
        }
    }

    // sub_tenants → prefix overlap within the same tenant (either direction
    // `starts_with`). The store loads enabled rows only, so this is the runtime
    // backstop for drift the write path (T6) should have rejected at submit time.
    for i in 0..cfg.sub_tenants.len() {
        for j in (i + 1)..cfg.sub_tenants.len() {
            let a = &cfg.sub_tenants[i];
            let b = &cfg.sub_tenants[j];
            if a.tenant_id != b.tenant_id {
                continue;
            }
            if a.key_prefix.starts_with(&b.key_prefix) || b.key_prefix.starts_with(&a.key_prefix) {
                issues.push(ValidationIssue::warn(format!(
                    "sub_tenant '{}' and '{}' have overlapping key_prefix within \
                     tenant '{}' ('{}' vs '{}')",
                    a.id, b.id, a.tenant_id, a.key_prefix, b.key_prefix
                )));
            }
        }
    }

    // Per-provider concurrency overrides (design-admission-queue §5 / P0.1):
    // a queue without a concurrency cap is meaningless, and a zero timeout is
    // a misconfiguration.
    for provider in cfg.providers.values() {
        if let Some(depth) = provider.max_queue_depth {
            if depth > 0 {
                // A non-fail-fast queue requires an explicit concurrency cap.
                match provider.max_concurrency {
                    None => {
                        issues.push(ValidationIssue::warn(format!(
                            "provider '{}' sets max_queue_depth={} but no max_concurrency; \
                             a queue without a concurrency cap is meaningless",
                            provider.id, depth
                        )));
                    }
                    Some(0) => {
                        issues.push(ValidationIssue::warn(format!(
                            "provider '{}' sets max_queue_depth={} but max_concurrency=0 (unlimited); \
                             a queue without a concurrency cap is meaningless",
                            provider.id, depth
                        )));
                    }
                    Some(_) => { /* valid: cap + queue */ }
                }
            }
        }
        if let Some(wait) = provider.queue_wait_timeout_ms {
            if wait == 0 {
                issues.push(ValidationIssue::warn(format!(
                    "provider '{}' sets queue_wait_timeout_ms=0; must be > 0",
                    provider.id
                )));
            }
        }
    }

    // --- User hierarchy / Global Role referential integrity (plan §3.1
    //  write-time invariants, load-time WARN backstop) ---------------------
    //
    // The SQLite FKs prevent most of this at write time, but the config graph is
    // the authority a replica rebuilds from, and a hand-edited DB or a restored
    // snapshot can still dangle; WARN (not Fatal) so the snapshot still publishes
    // and the affected rows are inert at auth/match time.
    let group_ids: HashSet<&str> = cfg.user_groups.iter().map(|g| g.id.as_str()).collect();
    let api_key_ids: HashSet<&str> = cfg
        .users_by_api_key_hash
        .values()
        .map(|idn| idn.principal.api_key_id.as_str())
        .collect();
    let tenant_ids: HashSet<&str> = cfg
        .tenants_by_domain
        .values()
        .map(|t| t.id.as_str())
        .collect();

    for user in cfg.users.values() {
        if !tenant_ids.contains(user.tenant_id.as_str()) {
            issues.push(ValidationIssue::warn_about(
                user.id.clone(),
                format!(
                    "user '{}' references unknown tenant_id '{}'",
                    user.id, user.tenant_id
                ),
            ));
        }
        if !group_ids.contains(user.user_group_id.as_str()) {
            issues.push(ValidationIssue::warn_about(
                user.id.clone(),
                format!(
                    "user '{}' references unknown user_group_id '{}'",
                    user.id, user.user_group_id
                ),
            ));
        }
    }

    // global_roles: every CONCRETE dimension must resolve to a row. `*` is the
    // wildcard (decision D-2) and is never a subject of these checks. A missing
    // dimension is inert at match time (no principal can carry an id that was
    // never issued), so WARN — plan §3.1 "悬挂维度 -> Warn".
    for role in &cfg.global_roles {
        if role.tenant_id != "*" && !tenant_ids.contains(role.tenant_id.as_str()) {
            issues.push(ValidationIssue::warn_about(
                role.id.clone(),
                format!(
                    "global_role '{}' references unknown tenant_id '{}'",
                    role.id, role.tenant_id
                ),
            ));
        }
        if role.user_group_id != "*" && !group_ids.contains(role.user_group_id.as_str()) {
            issues.push(ValidationIssue::warn_about(
                role.id.clone(),
                format!(
                    "global_role '{}' references unknown user_group_id '{}'",
                    role.id, role.user_group_id
                ),
            ));
        }
        if role.user_id != "*" && !cfg.users.contains_key(&role.user_id) {
            issues.push(ValidationIssue::warn_about(
                role.id.clone(),
                format!(
                    "global_role '{}' references unknown user_id '{}'",
                    role.id, role.user_id
                ),
            ));
        }
        if role.api_key_id != "*" && !api_key_ids.contains(role.api_key_id.as_str()) {
            issues.push(ValidationIssue::warn_about(
                role.id.clone(),
                format!(
                    "global_role '{}' references unknown api_key_id '{}'",
                    role.id, role.api_key_id
                ),
            ));
        }
        // `model_key` concrete but not offered by any ONLINE provider: allowed
        // (pre-provisioning a rule before the model comes online is legitimate —
        // plan §3.1 "允许预配置"), hence WARN.
        if role.model_key != "*" {
            let missing = match cfg.models_by_key.get(&role.model_key) {
                Some(v) => v.is_empty(),
                None => true,
            };
            if missing {
                issues.push(ValidationIssue::warn_about(
                    role.id.clone(),
                    format!(
                        "global_role '{}' scopes model_key '{}' which has no online provider yet \
                         (pre-provisioned, or the model went offline)",
                        role.id, role.model_key
                    ),
                ));
            }
        }
        // D-3 backstop (best-effort, WARN): a WILDCARD-tenant rule is a CANDIDATE
        // for EVERY tenant, but `router` intersects it with each tenant's own
        // `tenant_providers` gate (fail-closed: a provider a tenant does not grant
        // is never routable for that tenant, whatever any rule says). So a provider
        // OUTSIDE SOME tenant's set means the rule silently does nothing for that
        // tenant — usually not what the operator meant by "global". Fatal would
        // be wrong: the rule is legitimate for the tenants that DO grant it, and a
        // tenant may intentionally be restricted to a subset.
        //
        // A tenant with NO `tenant_providers` entry counts as granting NOTHING (the
        // router's `get()` fail-closes the same way), so such a tenant makes every
        // concrete provider of a wildcard rule an offender.
        if role.tenant_id == "*" && !role.provider_ids.is_empty() {
            let offenders: Vec<&str> = role
                .provider_ids
                .iter()
                .filter(|p| {
                    cfg.tenants_by_id.values().any(|t| {
                        !cfg.tenant_providers
                            .get(&t.id)
                            .is_some_and(|allowed| allowed.contains(p.as_str()))
                    })
                })
                .map(|p| p.as_str())
                .collect();
            if !offenders.is_empty() {
                // Sorted: this runs inside a HashMap-derived loop, and the warning
                // text is operator-visible.
                let mut sorted = offenders.clone();
                sorted.sort();
                issues.push(ValidationIssue::warn_about(
                    role.id.clone(),
                    format!(
                        "global_role '{}' has a WILDCARD tenant (candidate for all {} tenant(s)) \
                         but grants provider(s) {} that at least one tenant does NOT allow in \
                         `tenant_providers` — for those tenants this rule can never route; the \
                         router intersects a wildcard rule with each tenant's own provider gate",
                        role.id,
                        cfg.tenants_by_id.len(),
                        sorted.join(", ")
                    ),
                ));
            }
        }
    }

    // Deterministic order across HashMap iterations.
    issues.sort_by(|a, b| a.message.cmp(&b.message).then(a.severity.cmp(&b.severity)));
    issues
}
