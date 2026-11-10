//! Row structs mirroring the portable schema, conversions to core entities, and
//! the timestamp/flag helpers shared by the repo modules.

use chrono::{DateTime, NaiveDateTime};

use hydra_core::model::{
    BillingStatus, LimitRole, Provider, ProviderKey, ProviderModel, Tenant, TenantAccessToken,
    TenantModel, TenantProvider, User, UserApiKey, UserGroup,
};

use crate::crypto::{self, CryptoError, KeyProvider, Sealed};

use super::limit_role::open_limit_key;
use super::tenant::TenantCert;

/// Map a [`CryptoError`] into a sqlx error so the repo functions keep a single
/// `Result<_, sqlx::Error>` return type. `CryptoError` is `Send + Sync + Error`.
pub(crate) fn crypto_to_sqlx(e: CryptoError) -> sqlx::Error {
    sqlx::Error::Decode(Box::new(e))
}

/// Format a DB-native timestamp into the admin/entity string shape.
pub(crate) fn fmt_ts(value: NaiveDateTime) -> String {
    value.format("%Y-%m-%d %H:%M:%S").to_string()
}

pub(crate) fn fmt_ts_opt(value: Option<NaiveDateTime>) -> Option<String> {
    value.map(fmt_ts)
}

/// Parse an entity timestamp into the DB-native type. Accepts the two shapes
/// the application already produces (`%Y-%m-%d %H:%M:%S` and RFC3339-Z).
pub(crate) fn parse_ts(value: &str) -> Result<NaiveDateTime, sqlx::Error> {
    if let Ok(v) = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S") {
        return Ok(v);
    }
    if let Ok(v) = DateTime::parse_from_rfc3339(value) {
        return Ok(v.naive_utc());
    }
    NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%SZ")
        .map_err(|e| sqlx::Error::Decode(Box::new(e)))
}

pub(crate) fn parse_ts_opt(value: &Option<String>) -> Result<Option<NaiveDateTime>, sqlx::Error> {
    value.as_deref().map(parse_ts).transpose()
}

/// BIGINT flag encoding (0/1), matching the portable schema and row decoding.
pub(crate) fn flag(value: bool) -> i64 {
    i64::from(value)
}

// ---------------------------------------------------------------------------
// Row structs (mirror the portable column types) + conversions to core entities.
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow, Debug, Clone)]
pub(crate) struct ProviderRow {
    id: String,
    provider_code: String,
    name: String,
    endpoint: String,
    weight: i64,
    created_at: NaiveDateTime,
    updated_at: NaiveDateTime,
    /// Per-provider concurrency admission policy (design-admission-queue §5).
    /// Nullable: NULL ⇒ use ProxyConfig defaults (opt-in). BIGINT → i64.
    max_concurrency: Option<i64>,
    max_queue_depth: Option<i64>,
    queue_wait_timeout_ms: Option<i64>,
}

/// Cast a nullable BIGINT (decoded as `Option<i64>`) to `Option<u32>`,
/// rejecting negative values (defensive — a manual DB edit should not panic the
/// loader). Negative ⇒ `None` (treated as "use defaults").
pub(crate) fn to_u32_opt(v: Option<i64>) -> Option<u32> {
    v.filter(|n| *n >= 0).map(|n| n as u32)
}

/// Same as [`to_u32_opt`] but for `u64` (`queue_wait_timeout_ms`).
pub(crate) fn to_u64_opt(v: Option<i64>) -> Option<u64> {
    v.filter(|n| *n >= 0).map(|n| n as u64)
}

impl From<ProviderRow> for Provider {
    fn from(r: ProviderRow) -> Self {
        Provider {
            id: r.id,
            key: r.provider_code,
            name: r.name,
            endpoint: r.endpoint,
            weight: r.weight as i32,
            created_at: fmt_ts(r.created_at),
            updated_at: fmt_ts(r.updated_at),
            max_concurrency: to_u32_opt(r.max_concurrency),
            max_queue_depth: to_u32_opt(r.max_queue_depth),
            queue_wait_timeout_ms: to_u64_opt(r.queue_wait_timeout_ms),
        }
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
pub(crate) struct ProviderModelRow {
    id: String,
    model_key: String,
    name: String,
    provider_id: String,
    status: i64,
}

impl From<ProviderModelRow> for ProviderModel {
    fn from(r: ProviderModelRow) -> Self {
        ProviderModel {
            id: r.id,
            key: r.model_key,
            name: r.name,
            provider_id: r.provider_id,
            status: r.status as i32,
        }
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
pub(crate) struct ProviderKeyRow {
    id: String,
    provider_id: String,
    // Nullable in the schema, but always populated by `insert_provider_key`.
    // The row type keeps them `Option` to mirror the DDL; `to_model` refuses a
    // row that is missing either half of the sealed value.
    api_key_ciphertext: Option<Vec<u8>>,
    api_key_nonce: Option<Vec<u8>>,
    key_version: i64,
    created_at: NaiveDateTime,
}

impl ProviderKeyRow {
    /// Decrypt the ciphertext into a plaintext `ProviderKey` model. Plaintext
    /// exists only transiently here, then lives in-memory in the model (never
    /// persisted). Requires a [`KeyProvider`] because decryption is I/O on the
    /// master key.
    pub(crate) fn to_model(&self, kp: &dyn KeyProvider) -> Result<ProviderKey, CryptoError> {
        let ciphertext = self
            .api_key_ciphertext
            .as_ref()
            .ok_or(CryptoError::Decrypt)?;
        let nonce_bytes = self.api_key_nonce.as_ref().ok_or(CryptoError::Decrypt)?;
        let nonce: [u8; crypto::NONCE_LEN] = nonce_bytes
            .as_slice()
            .try_into()
            .map_err(|_| CryptoError::Decrypt)?;
        let sealed = Sealed {
            ciphertext: ciphertext.clone(),
            nonce,
            key_version: self.key_version as u32,
        };
        let plaintext = kp.open(&sealed)?;
        let api_key = String::from_utf8(plaintext).map_err(|_| CryptoError::NotUtf8)?;
        Ok(ProviderKey {
            id: self.id.clone(),
            provider_id: self.provider_id.clone(),
            api_key,
            created_at: fmt_ts(self.created_at),
        })
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
pub(crate) struct TenantRow {
    id: String,
    name: String,
    domain: String,
    cert_key: Option<String>,
    cert_file: Option<String>,
    enabled: i64,
    created_at: NaiveDateTime,
    updated_at: NaiveDateTime,
}

impl From<TenantRow> for Tenant {
    fn from(r: TenantRow) -> Self {
        Tenant {
            id: r.id,
            name: r.name,
            domain: r.domain,
            cert_key: r.cert_key,
            cert_file: r.cert_file,
            enabled: r.enabled != 0,
            created_at: fmt_ts(r.created_at),
            updated_at: fmt_ts(r.updated_at),
        }
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
pub(crate) struct TenantProviderRow {
    id: String,
    tenant_id: String,
    provider_id: String,
}

impl From<TenantProviderRow> for TenantProvider {
    fn from(r: TenantProviderRow) -> Self {
        TenantProvider {
            id: r.id,
            tenant_id: r.tenant_id,
            provider_id: r.provider_id,
        }
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
pub(crate) struct TenantModelRow {
    id: String,
    tenant_id: String,
    model_key: String,
}

impl From<TenantModelRow> for TenantModel {
    fn from(r: TenantModelRow) -> Self {
        TenantModel {
            id: r.id,
            tenant_id: r.tenant_id,
            model_key: r.model_key,
        }
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
pub(crate) struct LimitRoleRow {
    id: String,
    name: String,
    matching_key: Option<String>,
    matching_model: Option<String>,
    matching_tenant: Option<String>,
    matching_provider: Option<String>,
    limit_count: Option<i64>,
    limit_token: Option<i64>,
    limit_window: String,
    enabled: i64,
    created_at: NaiveDateTime,
}

impl LimitRoleRow {
    /// The in-memory row, with `matching_key` opened.
    ///
    /// # Errors
    /// [`crypto_to_sqlx`] wrapping a [`CryptoError`] when the stored value IS a sealed envelope the
    /// master key cannot open (a wrong or rotated-away key is refused, never handed back as a key).
    /// A value that is not an envelope at all is a pre-D-16 plaintext row and is returned as-is —
    /// see `crypto::Sealed::open_text`.
    pub(crate) fn to_model(&self, kp: &dyn KeyProvider) -> Result<LimitRole, sqlx::Error> {
        Ok(LimitRole {
            id: self.id.clone(),
            name: self.name.clone(),
            matching_key: open_limit_key(kp, self.matching_key.clone())?,
            matching_model: self.matching_model.clone(),
            matching_tenant: self.matching_tenant.clone(),
            matching_provider: self.matching_provider.clone(),
            limit_count: self.limit_count,
            limit_token: self.limit_token,
            window: self.limit_window.clone(),
            enabled: self.enabled != 0,
            created_at: fmt_ts(self.created_at),
        })
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
pub(crate) struct TenantCertRow {
    tenant_id: String,
    cert_pem: Option<String>,
    cert_key_ciphertext: Option<Vec<u8>>,
    cert_key_nonce: Option<Vec<u8>>,
    cert_key_version: Option<i64>,
}

impl TenantCertRow {
    /// Decrypt the sealed private key into a plaintext [`TenantCert`]. A row
    /// with ciphertext but a bad/missing nonce or version is a hard decode
    /// error (tampered / wrong master key — fail-closed, like provider keys).
    pub(crate) fn to_model(&self, kp: &dyn KeyProvider) -> Result<TenantCert, sqlx::Error> {
        let cert_key_pem = match (
            &self.cert_key_ciphertext,
            &self.cert_key_nonce,
            self.cert_key_version,
        ) {
            (Some(ct), Some(nonce), Some(version)) => {
                let nonce: [u8; crypto::NONCE_LEN] = nonce
                    .as_slice()
                    .try_into()
                    .map_err(|_| crypto_to_sqlx(CryptoError::Decrypt))?;
                let sealed = Sealed {
                    ciphertext: ct.clone(),
                    nonce,
                    key_version: version as u32,
                };
                let plaintext = kp.open(&sealed).map_err(crypto_to_sqlx)?;
                Some(
                    String::from_utf8(plaintext)
                        .map_err(|_| crypto_to_sqlx(CryptoError::NotUtf8))?,
                )
            }
            _ => None,
        };
        Ok(TenantCert {
            tenant_id: self.tenant_id.clone(),
            cert_pem: self.cert_pem.clone(),
            cert_key_pem,
        })
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
pub(crate) struct UserGroupRow {
    id: String,
    tenant_id: String,
    name: String,
    enabled: i64,
    created_at: NaiveDateTime,
    updated_at: NaiveDateTime,
}

impl From<UserGroupRow> for UserGroup {
    fn from(r: UserGroupRow) -> Self {
        UserGroup {
            id: r.id,
            tenant_id: r.tenant_id,
            name: r.name,
            enabled: r.enabled != 0,
            created_at: fmt_ts(r.created_at),
            updated_at: fmt_ts(r.updated_at),
        }
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
pub(crate) struct UserRow {
    id: String,
    tenant_id: String,
    user_group_id: String,
    external_id: Option<String>,
    name: String,
    enabled: i64,
    billing_status: String,
    billing_reason: Option<String>,
    billing_updated_at: Option<NaiveDateTime>,
    created_at: NaiveDateTime,
    updated_at: NaiveDateTime,
}

impl From<UserRow> for User {
    fn from(r: UserRow) -> Self {
        User {
            id: r.id,
            tenant_id: r.tenant_id,
            user_group_id: r.user_group_id,
            external_id: r.external_id,
            name: r.name,
            enabled: r.enabled != 0,
            // The CHECK domain is ('active','delinquent'); an out-of-domain value
            // cannot exist through the schema, and a manual edit must not crash the
            // loader — anything not exactly 'delinquent' loads as Active (the
            // safe default: never fabricate a 402 from a corrupt row).
            billing_status: if r.billing_status == "delinquent" {
                BillingStatus::Delinquent
            } else {
                BillingStatus::Active
            },
            billing_reason: r.billing_reason,
            billing_updated_at: fmt_ts_opt(r.billing_updated_at),
            created_at: fmt_ts(r.created_at),
            updated_at: fmt_ts(r.updated_at),
        }
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
pub(crate) struct UserApiKeyRow {
    id: String,
    user_id: String,
    name: String,
    key_hash: String,
    key_prefix: String,
    key_last4: String,
    enabled: i64,
    expires_at: Option<String>,
    created_at: NaiveDateTime,
    updated_at: NaiveDateTime,
}

impl From<UserApiKeyRow> for UserApiKey {
    fn from(r: UserApiKeyRow) -> Self {
        UserApiKey {
            id: r.id,
            user_id: r.user_id,
            name: r.name,
            key_hash: r.key_hash,
            key_prefix: r.key_prefix,
            key_last4: r.key_last4,
            enabled: r.enabled != 0,
            expires_at: r.expires_at,
            created_at: fmt_ts(r.created_at),
            updated_at: fmt_ts(r.updated_at),
        }
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
pub(crate) struct TenantAccessTokenRow {
    id: String,
    tenant_id: String,
    name: String,
    token_hash: String,
    token_prefix: String,
    token_last4: String,
    enabled: i64,
    expires_at: Option<String>,
    created_at: NaiveDateTime,
    updated_at: NaiveDateTime,
}

impl From<TenantAccessTokenRow> for TenantAccessToken {
    fn from(r: TenantAccessTokenRow) -> Self {
        TenantAccessToken {
            id: r.id,
            tenant_id: r.tenant_id,
            name: r.name,
            token_hash: r.token_hash,
            token_prefix: r.token_prefix,
            token_last4: r.token_last4,
            enabled: r.enabled != 0,
            expires_at: r.expires_at,
            created_at: fmt_ts(r.created_at),
            updated_at: fmt_ts(r.updated_at),
        }
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
pub(crate) struct GlobalRoleRow {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) tenant_id: String,
    pub(crate) user_group_id: String,
    pub(crate) user_id: String,
    pub(crate) api_key_id: String,
    pub(crate) model_key: String,
    pub(crate) enabled: i64,
    pub(crate) created_at: NaiveDateTime,
    pub(crate) updated_at: NaiveDateTime,
}

#[derive(sqlx::FromRow, Debug, Clone)]
pub(crate) struct GlobalRoleProviderRow {
    pub(crate) role_id: String,
    pub(crate) provider_id: String,
}
