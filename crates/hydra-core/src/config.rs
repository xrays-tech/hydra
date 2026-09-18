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

    /// `domain` → certificate metadata. Plain value here (see module docs);
    /// W1–W2 carries `CertMeta`, W4 resolves to a parsed `ResolvedCert` on the
    /// server side while this map remains the single source of truth.
    pub certs: HashMap<String, CertMeta>,
}

impl ConfigData {
    /// 从 `tenants_by_domain` 重建 `tenants_by_id`（唯一写入口）。
    ///
    /// 纯函数：无 I/O、不确定输入之外的状态。两个调用点都必须经过它 ——
    /// `store.rs::build_config`（leader 加载）与 `cluster/snapshot.rs::SnapshotWire::hydrate`
    /// （副本从线缆反序列化之后）。因为它们共用同一个实现，派生规则不可能分叉，
    /// 也不需要 `WIRE_VERSION` 变更：`serde(skip)` 之后两侧都由本函数产生。
    ///
    /// 空的 `tenants_by_domain`（尚未收到第一帧快照的 edge）产生空索引，这是正确的：
    /// 此时 `ConfigStore::replication()` 为 `None`，令牌闸门回 503 `not_ready`，
    /// 而不是把有效令牌误判为 403。
    pub fn reindex_tenants(&mut self) {
        self.tenants_by_id.clear();
        self.tenants_by_id.extend(
            self.tenants_by_domain
                .values()
                .map(|t| (t.id.clone(), t.clone())),
        );
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
}

impl ValidationIssue {
    fn warn(message: String) -> Self {
        Self {
            severity: Severity::Warn,
            message,
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

    // online providers (weight != 0) → must have ≥1 api_key.
    for provider in cfg.providers.values() {
        if provider.weight == 0 {
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
            issues.push(ValidationIssue::warn(format!(
                "limit_role '{}' has both limit_count and limit_token NULL",
                role.id
            )));
        }
    }

    // provider_key_bindings → prefix non-empty + provider must exist.
    for b in &cfg.key_prefix_bindings {
        if b.key_prefix.is_empty() {
            issues.push(ValidationIssue::warn(format!(
                "provider_key_binding '{}' has an empty key_prefix; it can never match",
                b.id
            )));
        }
        if !cfg.providers.contains_key(&b.provider_id) {
            issues.push(ValidationIssue::warn(format!(
                "provider_key_binding '{}' references unknown provider_id '{}'",
                b.id, b.provider_id
            )));
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

    // Deterministic order across HashMap iterations.
    issues.sort_by(|a, b| a.message.cmp(&b.message).then(a.severity.cmp(&b.severity)));
    issues
}
