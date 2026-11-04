//! # Splitting `ConfigData` into per-entity values (ADR-0001, plan T3.1 I/O half)
//!
//! [`crate::cluster::arachne_store`] publishes a **tree**: one value per entity,
//! keyed by the entity's path, named by the hash of a table of contents. The key
//! layout has existed since T2.1 and the commit point since T2.2, but until now
//! nothing produced a tree from the runtime config — the wire format sealed the
//! whole config as ONE payload (`SnapshotWire`), so a tree would have been a
//! single value wearing a tree's clothes.
//!
//! This module is that missing producer and its inverse. It is a pure function
//! pair: no cluster, no database. `CertMeta::cert_key_pem` **is not serialised at
//! all** (`skip_serializing`, by design: it is re-derived at the DB boundary), so
//! the cert entity carries it separately and sealed.
//!
//! ## What the tree carries, and in which form
//!
//! A replica is not only `ConfigData`: it also needs the rows `ConfigData` is
//! derived from (disabled `limit_role` rows, offline `provider_model` rows, row
//! ids) plus the secret-bearing sets — [`FidelityTreeEntity`], the per-provider key
//! entities, the limit-role entities and the cert entities. The rows that are not
//! secret travel as plain JSON; provider api-keys, cert private keys, tenant token
//! hashes and `limit_role.matching_key` travel sealed (see [`sealed_limit_role`]),
//! and are opened only by a holder of the master key.
//!
//! ## Why the encoder holds a key provider — and why that is safe now
//!
//! It could not, at first. The tree is content-addressed — its name is the hash of
//! its bytes — and `KeyProvider::seal` uses a FRESH RANDOM nonce, so sealing during
//! encoding gave one unchanged config a different tree name on every publish: the
//! head advanced, and every node re-materialized the whole config for nothing. The
//! escape was to make the CALLER supply already-sealed material
//! (`SealedMaterial`), which only moved the question: that material has to be
//! reproducible too, and one of the three secret sets (the tenant token hash) has
//! no stored ciphertext to copy in the first place.
//!
//! [`KeyProvider::seal_deterministic`] removes the premise instead: its nonce is
//! derived from the plaintext and the master key, so the same logical config seals
//! to the same bytes on every node. The tree's name is now a function of the
//! LOGICAL config, the caller-supplied-material type is gone, and the property is
//! pinned by `a_publish_that_changes_nothing_names_the_same_tree_twice` — which
//! builds the config twice on purpose.
//!
//! ## Why one value per entity, again
//!
//! Because the alternative is a config that rewrites itself. A whole-config
//! payload has one hash, so adding a single tenant changes it, so EVERY node
//! re-publishes and re-reads the entire config — and the log grows by the size of
//! the config rather than the size of the change. Per-entity values make the
//! change proportional to what changed, which is also what makes the toc's
//! per-entity content hashes worth carrying.

use std::collections::HashSet;

use hydra_core::config::{CertMeta, ConfigData};
use hydra_core::identity::TenantTokenIdentity;
use hydra_core::model::{
    GlobalRole, LimitRole, ProviderKey, ProviderModel, TenantAccessToken, TenantModel,
    TenantProvider, User, UserApiKey, UserGroup,
};

use super::arachne_keys::{
    content_hash, validate_entities, EntityPath, KeysError, Toc, TocEntry, TOC_FORMAT,
};
use super::arachne_store::ConfigTree;
use crate::cluster::content::FidelityRows;
use crate::crypto::{KeyProvider, Sealed};

/// One `Cert` entity: the domain's certificate, with its private key SEALED.
///
/// ## Why this is not just `CertMeta`
///
/// `CertMeta::cert_key_pem` is `#[serde(skip_serializing)]`, and rightly so: a
/// private key must not reach ANY serialised form, and `db::restore_config`
/// re-seals it at the DB boundary from the in-memory value. The Redis-era wire
/// compensates with a separate sealed field (`SnapshotWire::sealed_certs`); the
/// tree needs the same thing, for a sharper reason — `restore_config` does not
/// merely OMIT a missing key, it writes NULL into `cert_key_ciphertext`, so a tree
/// without the sealed key DELETES the private key a materializing node already
/// had. Caught by `tests/arachne_cert_fidelity.rs`.
///
/// The key rides the cert's OWN entity rather than a singleton: one entity per
/// domain is what keeps "this tenant's cert changed" from rewriting every cert.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CertTreeEntity {
    /// The certificate as `CertMeta` serialises it (`cert_key_pem` absent by the
    /// skip; it is re-supplied from [`Self::sealed_key`] on decode).
    pub meta: CertMeta,
    /// The private key PEM, sealed. `None` when the tenant has no key content (a
    /// legacy `cert_file`/`cert_key` path pair, or no certificate at all).
    pub sealed_key: Option<Sealed>,
}

/// The rows a replica needs beyond `ConfigData`, as it travels in the tree.
///
/// ## Why the secrets stay sealed
///
/// `provider_keys` hold provider API keys, `tenant_token_hashes` hold the fleet's authentication
/// material and `limit_roles` carry a `matching_key` that may be a live CLIENT key. The existing wire
/// SEALS all of them, and a tree that carried them in the clear would quietly undo that — so this
/// type keeps the same split: the rows that are not secret travel as plain JSON, and every secret
/// column travels sealed. `limit_roles` is the one set where a SINGLE COLUMN is sealed rather than
/// the whole row ([`sealed_limit_role`]), because D-16 seals the column, not the table.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FidelityTreeEntity {
    /// Full `limit_role` rows, DISABLED included, each with its `matching_key` sealed.
    pub limit_roles: Vec<LimitRole>,
    /// Full `provider_model` rows, OFFLINE (`status != 1`) included — the derived
    /// `models_by_key` drops those, and a replica must still have them.
    pub provider_models: Vec<ProviderModel>,
    /// Full `tenant_provider` rows, ids preserved.
    pub tenant_providers: Vec<TenantProvider>,
    /// Full `tenant_model` rows, ids preserved.
    pub tenant_models: Vec<TenantModel>,
    /// Provider api-keys WITH their row identity, sealed.
    pub sealed_provider_keys: Vec<SealedProviderKey>,
    /// `tenant_id` → sealed access-token hash.
    pub sealed_tenant_token_hashes: Vec<(String, Sealed)>,
}

/// One provider key's identity plus its sealed api-key.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SealedProviderKey {
    /// The row's primary key — preserved so a rebuild does not re-mint it.
    pub id: String,
    /// The provider this key belongs to.
    pub provider_id: String,
    /// When the row was created — preserved for the same reason.
    pub created_at: String,
    /// The api-key, sealed under the master key.
    pub sealed: Sealed,
}

/// One user API-key row with its `key_hash` sealed.
///
/// This is the tree carrier for `user_api_key/<id>`: every non-secret column is plain JSON, and the
/// only authentication material (`key_hash`) is sealed deterministically. `enabled=false` and expired
/// keys still travel here — they decode into `FidelityRows.user_api_keys`, and `reindex_identity` later
/// turns them into known-but-invalid identity records on the replica (D-5).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SealedUserApiKey {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub key_prefix: String,
    pub key_last4: String,
    pub enabled: bool,
    pub expires_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Sealed SHA-256 hash (`key_hash`), never serialized in the clear by this entity.
    pub sealed_key_hash: Sealed,
}

/// One tenant access-token row with its `token_hash` sealed.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SealedTenantAccessToken {
    pub id: String,
    pub tenant_id: String,
    pub name: String,
    pub token_prefix: String,
    pub token_last4: String,
    pub enabled: bool,
    pub expires_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Sealed SHA-256 hash (`token_hash`), never serialized in the clear by this entity.
    pub sealed_token_hash: Sealed,
}

/// Global Role entity: the rule row without its provider vector plus the provider vector itself.
///
/// `role.provider_ids` is emptied before serialisation so the provider set appears exactly once, as
/// the entity's dedicated `provider_ids` field. Disabled rules still travel under this path and are
/// restored from `FidelityRows.global_roles`; only enabled ones enter `ConfigData.global_roles`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GlobalRoleTreeEntity {
    /// The Global Role row itself. `role.provider_ids` is always empty on the wire.
    pub role: GlobalRole,
    /// The role's provider ids, in loader order (provider_id ascending).
    pub provider_ids: Vec<String>,
}

/// Seal a `limit_role`'s `matching_key` for the tree: the row comes back with that ONE column
/// replaced by its sealed text form (`sealed:v1:…`), every other column untouched.
///
/// ## Why the sealed form lives in `matching_key` instead of a sibling field
///
/// Decision D-16 (2026-10-08): the column is sealed. In the DATABASE the two possible values —
/// envelope or pre-D-16 plaintext — are told apart by the value itself (`crypto::Sealed::
/// from_text`), because there is only one column and it has held plaintext since the first release.
/// The tree carries the SAME text form, so there is one rule for both carriers rather than a second
/// "is this the tree's flavour of sealed?" question that only ever drifts.
///
/// `seal_deterministic`, not `seal`: the tree is content-addressed (its name is the hash of its
/// bytes), so a fresh random nonce per publish would rename an UNCHANGED config — the trap
/// documented at the top of this module, and the reason the provider keys above do the same.
fn sealed_limit_role(
    role: &LimitRole,
    kp: &dyn KeyProvider,
) -> Result<LimitRole, EntityCodecError> {
    let Some(key) = role.matching_key.as_deref() else {
        // NULL means "this role matches any key" (design §10.1) and must NOT become `Some("")`.
        return Ok(role.clone());
    };
    let sealed =
        kp.seal_deterministic(key.as_bytes())
            .map_err(|e| EntityCodecError::Unsupported {
                reason: format!(
                    "cannot seal the matching key of limit role {}: {e}",
                    role.id
                ),
            })?;
    let mut out = role.clone();
    out.matching_key = Some(sealed.to_text());
    Ok(out)
}

/// The inverse of [`sealed_limit_role`]: open a row's `matching_key` after it came out of the tree.
///
/// ## Why the tree side has NO plaintext fallback, when the database side does
///
/// Anything this build decoded from an entity is an envelope, and a value that is not one is
/// REFUSED by name. The compatibility question for trees is answered one level UP, on purpose:
/// D-16 changed what a `limit_role` entity's bytes MEAN, so it is a `TOC_FORMAT` bump (`3 → 4`) and
/// a tree written by an older build is refused by the toc before any entity is decoded. A fallback
/// here would be a compat rule that cannot fire — and worse, it would keep a MIS-DECODED tree
/// alive: an old build's plaintext and a new build's envelope live in the same field, so "accept
/// both" is exactly how one node ends up matching against `sealed:v1:…` and silently stops
/// enforcing the limit.
///
/// # Errors
/// [`EntityCodecError::Crypto`] when the value is not an envelope at all, or is one the master key
/// cannot open. Either way the caller keeps its last-known-good config.
fn opened_limit_role(
    role: &LimitRole,
    path: &EntityPath,
    kp: &dyn KeyProvider,
) -> Result<LimitRole, EntityCodecError> {
    let Some(stored) = role.matching_key.as_deref() else {
        return Ok(role.clone());
    };
    let Some(sealed) = Sealed::from_text(stored) else {
        return Err(EntityCodecError::Crypto {
            path: path.to_key_segment(),
            reason: format!(
                "the matching key of limit role {} is not a sealed value; this build speaks \
                 TOC_FORMAT {TOC_FORMAT}, where that column is always sealed (decision D-16)",
                role.id
            ),
        });
    };
    let key = kp.open(&sealed).map_err(|e| EntityCodecError::Crypto {
        path: path.to_key_segment(),
        reason: format!(
            "cannot open the matching key of limit role {}: {e}",
            role.id
        ),
    })?;
    let key = String::from_utf8(key).map_err(|_| EntityCodecError::Crypto {
        path: path.to_key_segment(),
        reason: format!(
            "the matching key of limit role {} is not valid UTF-8 after unsealing",
            role.id
        ),
    })?;
    let mut out = role.clone();
    out.matching_key = Some(key);
    Ok(out)
}

impl FidelityTreeEntity {
    /// Open the sealed rows and reassemble the full set.
    ///
    /// # Errors
    /// [`EntityCodecError::Crypto`] when the master key cannot open a sealed value — a wrong or
    /// rotated-away key is REFUSED, never guessed at, and the caller keeps its last-known-good
    /// config rather than installing something with secrets missing.
    pub fn rows(&self, kp: &dyn KeyProvider) -> Result<FidelityRows, EntityCodecError> {
        let mut limit_roles = Vec::with_capacity(self.limit_roles.len());
        for r in &self.limit_roles {
            limit_roles.push(opened_limit_role(r, &EntityPath::Fidelity, kp)?);
        }

        let mut provider_keys = Vec::with_capacity(self.sealed_provider_keys.len());
        for k in &self.sealed_provider_keys {
            let api_key = kp.open(&k.sealed).map_err(|e| EntityCodecError::Crypto {
                path: EntityPath::Fidelity.to_key_segment(),
                reason: format!("cannot open provider key {}: {e}", k.id),
            })?;
            let api_key = String::from_utf8(api_key).map_err(|_| EntityCodecError::Crypto {
                path: EntityPath::Fidelity.to_key_segment(),
                reason: format!("provider key {} is not valid UTF-8 after unsealing", k.id),
            })?;
            provider_keys.push(ProviderKey {
                id: k.id.clone(),
                provider_id: k.provider_id.clone(),
                api_key,
                created_at: k.created_at.clone(),
            });
        }

        let mut tenant_token_hashes = Vec::with_capacity(self.sealed_tenant_token_hashes.len());
        for (tenant_id, sealed) in &self.sealed_tenant_token_hashes {
            let hash = kp.open(sealed).map_err(|e| EntityCodecError::Crypto {
                path: EntityPath::Fidelity.to_key_segment(),
                reason: format!("cannot open the token hash of tenant {tenant_id}: {e}"),
            })?;
            let hash = String::from_utf8(hash).map_err(|_| EntityCodecError::Crypto {
                path: EntityPath::Fidelity.to_key_segment(),
                reason: format!("the token hash of tenant {tenant_id} is not valid UTF-8"),
            })?;
            tenant_token_hashes.push((tenant_id.clone(), hash));
        }

        Ok(FidelityRows {
            limit_roles,
            provider_keys,
            tenant_token_hashes,
            provider_models: self.provider_models.clone(),
            tenant_providers: self.tenant_providers.clone(),
            tenant_models: self.tenant_models.clone(),
            // The user-hierarchy fields are deliberately not carried by `_fidelity`; they arrive
            // through their own `user_group/<id>`, `user/<id>`, ... entity paths.
            ..FidelityRows::default()
        })
    }

    /// Assemble the entity from the rows, sealing the two secret-bearing sets.
    ///
    /// ## Why sealing may live here now
    ///
    /// It could not, at first. The tree is **content-addressed** — its name is the hash of its
    /// bytes — and a fresh AES-GCM nonce per seal gave one unchanged config a new tree name on
    /// every publish (measured: the first version of this function named two different trees for
    /// one config). The workaround was to have the CALLER supply already-sealed material, which
    /// then had to come from somewhere reproducible.
    ///
    /// [`KeyProvider::seal_deterministic`] removes the premise: the nonce is derived from the
    /// plaintext and the master key, so sealing here produces the same bytes for the same logical
    /// config on every node. The caller-supplied-material type is therefore gone.
    ///
    /// # Errors
    /// [`EntityCodecError::Unsupported`] when the master key refuses to seal. A failure stops the
    /// publish; it never degrades to plaintext.
    pub fn from_plaintext(
        rows: &FidelityRows,
        kp: &dyn KeyProvider,
    ) -> Result<Self, EntityCodecError> {
        let mut limit_roles = Vec::with_capacity(rows.limit_roles.len());
        for r in &rows.limit_roles {
            limit_roles.push(sealed_limit_role(r, kp)?);
        }

        let mut sealed_provider_keys = Vec::with_capacity(rows.provider_keys.len());
        for k in &rows.provider_keys {
            let sealed = kp.seal_deterministic(k.api_key.as_bytes()).map_err(|e| {
                EntityCodecError::Unsupported {
                    reason: format!("cannot seal provider key {}: {e}", k.id),
                }
            })?;
            sealed_provider_keys.push(SealedProviderKey {
                id: k.id.clone(),
                provider_id: k.provider_id.clone(),
                created_at: k.created_at.clone(),
                sealed,
            });
        }
        let mut sealed_tenant_token_hashes = Vec::with_capacity(rows.tenant_token_hashes.len());
        for (tenant_id, hash) in &rows.tenant_token_hashes {
            let sealed = kp.seal_deterministic(hash.as_bytes()).map_err(|e| {
                EntityCodecError::Unsupported {
                    reason: format!("cannot seal the token hash of tenant {tenant_id}: {e}"),
                }
            })?;
            sealed_tenant_token_hashes.push((tenant_id.clone(), sealed));
        }
        Ok(Self {
            limit_roles,
            provider_models: rows.provider_models.clone(),
            tenant_providers: rows.tenant_providers.clone(),
            tenant_models: rows.tenant_models.clone(),
            sealed_provider_keys,
            sealed_tenant_token_hashes,
        })
    }
}

/// Why a config could not be split or rebuilt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntityCodecError {
    /// An entity id cannot be carried by a key.
    Keys(KeysError),
    /// An entity could not be encoded, or a stored entity could not be decoded.
    Serde { path: String, reason: String },
    /// The builder was handed a value it cannot place in a tree.
    Unsupported { reason: String },
    /// A sealed row could not be opened (a wrong or rotated-away master key).
    Crypto { path: String, reason: String },
}

impl std::fmt::Display for EntityCodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Keys(e) => write!(f, "config key space: {e}"),
            Self::Serde { path, reason } => {
                write!(f, "entity {path} could not be decoded: {reason}")
            }
            Self::Unsupported { reason } => write!(f, "cannot build a config tree: {reason}"),
            Self::Crypto { path, reason } => {
                write!(f, "cannot open the sealed rows of {path}: {reason}")
            }
        }
    }
}

impl std::error::Error for EntityCodecError {}

impl From<KeysError> for EntityCodecError {
    fn from(e: KeysError) -> Self {
        Self::Keys(e)
    }
}

/// One entity's bytes, tagged with the path they belong to.
///
/// Returned as a list rather than a map so a caller can see the ORDER the
/// publisher used — which matters, because the toc's bytes are the tree's name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityBlob {
    /// Where this entity lives in the tree.
    pub path: EntityPath,
    /// Its JSON encoding.
    pub bytes: Vec<u8>,
}

fn encode<T: serde::Serialize>(
    path: &EntityPath,
    value: &T,
) -> Result<EntityBlob, EntityCodecError> {
    let bytes = serde_json::to_vec(value).map_err(|e| EntityCodecError::Serde {
        path: path.to_key_segment(),
        reason: e.to_string(),
    })?;
    Ok(EntityBlob {
        path: path.clone(),
        bytes,
    })
}

fn decode<T: serde::de::DeserializeOwned>(
    path: &EntityPath,
    bytes: &[u8],
) -> Result<T, EntityCodecError> {
    serde_json::from_slice(bytes).map_err(|e| EntityCodecError::Serde {
        path: path.to_key_segment(),
        reason: e.to_string(),
    })
}

/// Split `cfg` (and the fidelity rows) into one blob per entity.
///
/// The output is **sorted by path** (so the encoding is deterministic and two publishers of the
/// same config produce the same tree name).
///
/// # Errors
/// [`EntityCodecError`] when an id cannot be keyed, an entity cannot be encoded, or a secret
/// cannot be sealed. All three are refused rather than skipped: a config tree that silently
/// dropped an entity would serve a config nobody configured.
///
/// `kp` is required because the secret-bearing entities are SEALED here (deterministically — see
/// the module docs); the decode side takes the same provider to open them.
pub fn split_config(
    cfg: &ConfigData,
    fidelity: &FidelityRows,
    kp: &dyn KeyProvider,
) -> Result<Vec<EntityBlob>, EntityCodecError> {
    let mut out = config_entities(cfg, kp)?;
    out.push(encode(
        &EntityPath::Fidelity,
        &FidelityTreeEntity::from_plaintext(fidelity, kp)?,
    )?);
    add_fidelity_row_blobs(&mut out, fidelity, kp)?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Deterministically seal a tree-carried secret. The tree is content-addressed, so a fresh nonce
/// per publish would rename an unchanged config.
fn sealed_deterministic(
    value: &str,
    what: String,
    kp: &dyn KeyProvider,
) -> Result<Sealed, EntityCodecError> {
    kp.seal_deterministic(value.as_bytes())
        .map_err(|e| EntityCodecError::Unsupported {
            reason: format!("cannot seal {what}: {e}"),
        })
}

/// Open a tree-carried sealed string. A wrong master key is refused, never downgraded to plaintext.
fn open_sealed_string(
    sealed: &Sealed,
    what: &str,
    path: &EntityPath,
    kp: &dyn KeyProvider,
) -> Result<String, EntityCodecError> {
    let bytes = kp.open(sealed).map_err(|e| EntityCodecError::Crypto {
        path: path.to_key_segment(),
        reason: format!("cannot open {what}: {e}"),
    })?;
    String::from_utf8(bytes).map_err(|_| EntityCodecError::Crypto {
        path: path.to_key_segment(),
        reason: format!("{what} is not valid UTF-8 after unsealing"),
    })
}

/// Add the FULL user-hierarchy rows from `FidelityRows` as per-row entities.
///
/// These paths are the authority for the new tables (plan §3.5 / D-13): disabled and expired rows
/// travel here too, but the same rows are NOT duplicated into the `_fidelity` singleton.
fn add_fidelity_row_blobs(
    out: &mut Vec<EntityBlob>,
    fidelity: &FidelityRows,
    kp: &dyn KeyProvider,
) -> Result<(), EntityCodecError> {
    let check = |path: &EntityPath| -> Result<(), EntityCodecError> {
        validate_entities(&[TocEntry {
            path: path.clone(),
            content_hash: [0u8; 32],
            len: 0,
        }])?;
        Ok(())
    };

    for group in &fidelity.user_groups {
        let path = EntityPath::UserGroup(group.id.clone());
        check(&path)?;
        out.push(encode(&path, group)?);
    }
    for user in &fidelity.users {
        let path = EntityPath::User(user.id.clone());
        check(&path)?;
        out.push(encode(&path, user)?);
    }
    for key in &fidelity.user_api_keys {
        let path = EntityPath::UserApiKey(key.id.clone());
        check(&path)?;
        let sealed_key_hash =
            sealed_deterministic(&key.key_hash, format!("user api key {}", key.id), kp)?;
        out.push(encode(
            &path,
            &SealedUserApiKey {
                id: key.id.clone(),
                user_id: key.user_id.clone(),
                name: key.name.clone(),
                key_prefix: key.key_prefix.clone(),
                key_last4: key.key_last4.clone(),
                enabled: key.enabled,
                expires_at: key.expires_at.clone(),
                created_at: key.created_at.clone(),
                updated_at: key.updated_at.clone(),
                sealed_key_hash,
            },
        )?);
    }
    for token in &fidelity.tenant_access_tokens {
        let path = EntityPath::TenantAccessToken(token.id.clone());
        check(&path)?;
        let sealed_token_hash = sealed_deterministic(
            &token.token_hash,
            format!("tenant access token {}", token.id),
            kp,
        )?;
        out.push(encode(
            &path,
            &SealedTenantAccessToken {
                id: token.id.clone(),
                tenant_id: token.tenant_id.clone(),
                name: token.name.clone(),
                token_prefix: token.token_prefix.clone(),
                token_last4: token.token_last4.clone(),
                enabled: token.enabled,
                expires_at: token.expires_at.clone(),
                created_at: token.created_at.clone(),
                updated_at: token.updated_at.clone(),
                sealed_token_hash,
            },
        )?);
    }
    for role in &fidelity.global_roles {
        let path = EntityPath::GlobalRole(role.id.clone());
        check(&path)?;
        let mut without_providers = role.clone();
        without_providers.provider_ids.clear();
        out.push(encode(
            &path,
            &GlobalRoleTreeEntity {
                role: without_providers,
                provider_ids: role.provider_ids.clone(),
            },
        )?);
    }
    Ok(())
}

/// Build the CONFIG entities (no fidelity). Private: sealing is not optional, so there is no
/// public way to produce a tree whose provider keys travel in the clear.
///
/// # Errors
/// [`EntityCodecError`] when an id cannot be keyed, an entity cannot be encoded, or a secret
/// cannot be sealed.
fn config_entities(
    cfg: &ConfigData,
    kp: &dyn KeyProvider,
) -> Result<Vec<EntityBlob>, EntityCodecError> {
    let mut out: Vec<EntityBlob> = Vec::new();

    // Every id is checked HERE, when the blob is produced. The check also runs later, inside
    // `toc_of`, but by then the caller has already built a tree from unvalidated paths — and the
    // failure mode that matters (an id containing `/` addressing another entity's namespace)
    // has to be refused before anything is constructed, not before it is published.
    let check = |path: &EntityPath| -> Result<(), EntityCodecError> {
        validate_entities(&[TocEntry {
            path: path.clone(),
            content_hash: [0u8; 32],
            len: 0,
        }])?;
        Ok(())
    };

    for (domain, tenant) in &cfg.tenants_by_domain {
        // Keyed by the tenant's immutable id, not by the domain it is reachable at: the domain
        // is a route and can be re-pointed, while the id is what every other entity refers to.
        let path = EntityPath::Tenant(tenant.id.clone());
        check(&path)?;
        out.push(encode(&path, tenant)?);
        let _ = domain;
    }
    for (key, providers) in &cfg.models_by_key {
        let path = EntityPath::Model(key.clone());
        check(&path)?;
        out.push(encode(&path, providers)?);
    }
    for (id, provider) in &cfg.providers {
        let path = EntityPath::Provider(id.clone());
        check(&path)?;
        out.push(encode(&path, provider)?);
    }
    for (id, keys) in &cfg.provider_keys {
        // SEALED, not plain JSON: an api-key is a secret and the snapshot wire seals it too.
        // Putting the same value into the tree in the clear would quietly undo that — the seal is
        // not decoration, and `the_fidelity_entity_carries_no_plaintext_secrets` is what says so.
        //
        // Deterministic sealing (`seal_deterministic`) is what makes this legal inside the encoder:
        // a random nonce here would give one unchanged config a new tree name per publish.
        let path = EntityPath::ProviderKey(id.clone());
        check(&path)?;
        let json = serde_json::to_vec(keys).map_err(|e| EntityCodecError::Serde {
            path: path.to_key_segment(),
            reason: e.to_string(),
        })?;
        let sealed = kp
            .seal_deterministic(&json)
            .map_err(|e| EntityCodecError::Unsupported {
                reason: format!("cannot seal the provider keys of {id}: {e}"),
            })?;
        out.push(encode(&path, &sealed)?);
    }
    for (tenant_id, providers) in &cfg.tenant_providers {
        let path = EntityPath::TenantProvider(tenant_id.clone());
        check(&path)?;
        out.push(encode(&path, &sorted_members(providers))?);
    }
    for (tenant_id, models) in &cfg.tenant_models {
        let path = EntityPath::TenantModel(tenant_id.clone());
        check(&path)?;
        out.push(encode(&path, &sorted_members(models))?);
    }
    for role in &cfg.limit_roles {
        let path = EntityPath::LimitRole(role.id.clone());
        check(&path)?;
        // SEALED, like the provider keys above: `matching_key` may hold a live client key, and a
        // tree that carried it in the clear would put the credential in every node's Arachne store
        // — the exact thing decision D-16 was taken to stop. Deterministic for the same reason.
        out.push(encode(&path, &sealed_limit_role(role, kp)?)?);
    }
    for (domain, cert) in &cfg.certs {
        let path = EntityPath::Cert(domain.clone());
        check(&path)?;
        // The private key is `skip_serializing` on `CertMeta`, so it HAS to travel separately —
        // omitted, `restore_config` would write NULL over the replica's `cert_key_ciphertext`,
        // i.e. delete the key on every node that materializes this tree. Sealed deterministically
        // for the same reason as the provider keys above.
        let sealed_key = match &cert.cert_key_pem {
            None => None,
            Some(pem) => Some(kp.seal_deterministic(pem.as_bytes()).map_err(|e| {
                EntityCodecError::Unsupported {
                    reason: format!("cannot seal the private key of the cert for {domain}: {e}"),
                }
            })?),
        };
        out.push(encode(
            &path,
            &CertTreeEntity {
                meta: cert.clone(),
                sealed_key,
            },
        )?);
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// A set's members in a FIXED order, for encoding.
///
/// ## Why the set is not encoded directly
///
/// `HashSet` iterates in an order decided by its own random seed, and serde serialises a set in
/// ITERATION order — so encoding `&HashSet<String>` makes the entity's bytes depend on the
/// process, not on the config. Two loader runs of the same unchanged rows then produce two
/// different trees: the head advances for nothing and every node re-materializes. It is the D-8
/// failure mode from a second direction (there: a random nonce; here: a random hash seed), and it
/// is invisible in a fixture whose sets hold one element, where order is not a question.
///
/// Caught by `two_independent_builds_of_the_same_config_name_the_same_tree`, which builds the
/// config twice on purpose. Do NOT "simplify" this back to `encode(&path, providers)?`.
fn sorted_members(set: &HashSet<String>) -> Vec<&String> {
    let mut members: Vec<&String> = set.iter().collect();
    members.sort();
    members
}

/// The tree these blobs form, ready for [`crate::cluster::arachne_store`].
///
/// # Errors
/// [`EntityCodecError::Unsupported`] when two entities claim the same path, which
/// would make the tree's contents depend on insertion order.
pub fn tree_of(blobs: &[EntityBlob]) -> Result<ConfigTree, EntityCodecError> {
    let mut tree = ConfigTree::new();
    for blob in blobs {
        if tree.insert(blob.path.clone(), blob.bytes.clone()).is_some() {
            return Err(EntityCodecError::Unsupported {
                reason: format!(
                    "two entities claim the path {}; a tree cannot name both",
                    blob.path.to_key_segment()
                ),
            });
        }
    }
    Ok(tree)
}

/// The toc describing `tree`, with one entry per entity.
///
/// # Errors
/// [`EntityCodecError`] when an id cannot be keyed.
pub fn toc_of(tree: &ConfigTree) -> Result<Toc, EntityCodecError> {
    let entries: Vec<TocEntry> = tree
        .iter()
        .map(|(path, bytes)| TocEntry {
            path: path.clone(),
            content_hash: content_hash(bytes),
            len: bytes.len() as u32,
        })
        .collect();
    Ok(Toc::new(TOC_FORMAT, entries)?)
}

/// Rebuild the `ConfigData` from a tree, ignoring the fidelity entity.
///
/// # Errors
/// [`EntityCodecError`] when an entity cannot be decoded or a path is not one this build knows.
/// Unknown paths are refused rather than ignored: they mean this node is reading a tree written
/// by a different version.
pub fn build_config(
    tree: &ConfigTree,
    kp: &dyn KeyProvider,
) -> Result<ConfigData, EntityCodecError> {
    Ok(build_config_with_fidelity(tree, kp)?.0)
}

/// Rebuild BOTH the config and the fidelity rows — what a replica needs to rebuild its own
/// SQLite tables byte-faithfully.
///
/// The derived index `tenants_by_id` is rebuilt here, because `serde(skip)` on that field means
/// the tree cannot carry it — and a missing index is not cosmetic: the tenant-token gate
/// resolves a token to a tenant through it, so an empty index turns every valid token into a 403.
///
/// # Errors
/// [`EntityCodecError`] when an entity cannot be decoded, a path is not one this build knows, or
/// a sealed fidelity row cannot be opened (a wrong or rotated-away master key is refused, never
/// guessed).
pub fn build_config_with_fidelity(
    tree: &ConfigTree,
    kp: &dyn KeyProvider,
) -> Result<(ConfigData, FidelityRows), EntityCodecError> {
    let mut cfg = ConfigData::default();
    let mut fidelity = FidelityRows::default();

    for (path, bytes) in tree {
        match path {
            EntityPath::Tenant(_) => {
                let tenant = decode::<hydra_core::model::Tenant>(path, bytes)?;
                // Keyed by the LOWERCASED domain, which is the loader's rule
                // (`store::build_config`) and the one the data plane matches on:
                // `proxy::resolve_tenant` lowercases the `Host` before the lookup. Re-deriving the
                // key from the value's own spelling instead put a mixed-case tenant under a key no
                // request looks up — the tenant resolved on the leader and not on any replica.
                // The VALUE keeps the stored spelling, exactly as the loader leaves it.
                cfg.tenants_by_domain
                    .insert(tenant.domain.to_lowercase(), tenant);
            }
            EntityPath::Model(key) => {
                let providers = decode(path, bytes)?;
                cfg.models_by_key.insert(key.clone(), providers);
            }
            EntityPath::Provider(id) => {
                let provider = decode(path, bytes)?;
                cfg.providers.insert(id.clone(), provider);
            }
            EntityPath::ProviderKey(id) => {
                // Sealed on the way in (see `split_config`), so it must be opened here. A wrong
                // or rotated-away master key is refused, never guessed at.
                let sealed: Sealed = decode(path, bytes)?;
                let json = kp.open(&sealed).map_err(|e| EntityCodecError::Crypto {
                    path: path.to_key_segment(),
                    reason: format!("cannot open the provider keys of {id}: {e}"),
                })?;
                let keys = serde_json::from_slice(&json).map_err(|e| EntityCodecError::Serde {
                    path: path.to_key_segment(),
                    reason: e.to_string(),
                })?;
                cfg.provider_keys.insert(id.clone(), keys);
            }
            EntityPath::TenantProvider(id) => {
                let members: Vec<String> = decode(path, bytes)?;
                cfg.tenant_providers
                    .insert(id.clone(), members.into_iter().collect());
            }
            EntityPath::TenantModel(id) => {
                let members: Vec<String> = decode(path, bytes)?;
                cfg.tenant_models
                    .insert(id.clone(), members.into_iter().collect());
            }
            EntityPath::LimitRole(_) => {
                // Sealed on the way in (see `split_config`): the row's `matching_key` is an envelope
                // here, and the matching engine needs the key itself.
                let role = decode::<LimitRole>(path, bytes)?;
                cfg.limit_roles.push(opened_limit_role(&role, path, kp)?);
            }
            EntityPath::Cert(domain) => {
                let entity = decode::<CertTreeEntity>(path, bytes)?;
                let mut meta = entity.meta;
                // Re-supply the private key the `skip_serializing` on `CertMeta` removed. A
                // sealed value that will not open is REFUSED, never downgraded to "no key":
                // `restore_config` would then write NULL over the node's key columns.
                if let Some(sealed) = &entity.sealed_key {
                    let pem = kp.open(sealed).map_err(|e| EntityCodecError::Crypto {
                        path: path.to_key_segment(),
                        reason: format!(
                            "cannot open the private key of the cert for {domain}: {e}"
                        ),
                    })?;
                    let pem = String::from_utf8(pem).map_err(|_| EntityCodecError::Crypto {
                        path: path.to_key_segment(),
                        reason: format!(
                            "the private key of the cert for {domain} is not valid UTF-8 after \
                                 unsealing"
                        ),
                    })?;
                    meta.cert_key_pem = Some(pem);
                }
                cfg.certs.insert(domain.clone(), meta);
            }
            EntityPath::Fidelity => {
                let entity = decode::<FidelityTreeEntity>(path, bytes)?;
                let legacy = entity.rows(kp)?;
                // Assign ONLY the old fidelity fields. The user-hierarchy rows are per-row
                // entities, so assigning the whole `FidelityRows` here could erase rows already
                // decoded from `user_group/<id>`, `user/<id>`, etc. depending on path order.
                fidelity.limit_roles = legacy.limit_roles;
                fidelity.provider_keys = legacy.provider_keys;
                fidelity.tenant_token_hashes = legacy.tenant_token_hashes;
                fidelity.provider_models = legacy.provider_models;
                fidelity.tenant_providers = legacy.tenant_providers;
                fidelity.tenant_models = legacy.tenant_models;
            }
            EntityPath::UserGroup(_) => {
                let group = decode::<UserGroup>(path, bytes)?;
                fidelity.user_groups.push(group.clone());
                // ConfigData.user_groups is the AUTH gate's known-state view, not an enabled-only
                // projection: a disabled group must still answer 403 `group_disabled`, so the full
                // row belongs here too (decision D-5 / loader rule above).
                cfg.user_groups.push(group);
            }
            EntityPath::User(_) => {
                let user = decode::<User>(path, bytes)?;
                fidelity.users.push(user.clone());
                // Same identity-known rule: disabled AND delinquent users remain present so
                // auth can distinguish 403/402 from unknown. `reindex_identity` below consumes
                // these plus the decoded key rows.
                cfg.users.insert(user.id.clone(), user);
            }
            EntityPath::UserApiKey(_) => {
                let entity = decode::<SealedUserApiKey>(path, bytes)?;
                let key_hash =
                    open_sealed_string(&entity.sealed_key_hash, "user api key hash", path, kp)?;
                fidelity.user_api_keys.push(UserApiKey {
                    id: entity.id.clone(),
                    user_id: entity.user_id.clone(),
                    name: entity.name.clone(),
                    key_prefix: entity.key_prefix.clone(),
                    key_hash,
                    key_last4: entity.key_last4.clone(),
                    enabled: entity.enabled,
                    expires_at: entity.expires_at.clone(),
                    created_at: entity.created_at.clone(),
                    updated_at: entity.updated_at.clone(),
                });
            }
            EntityPath::TenantAccessToken(_) => {
                let entity = decode::<SealedTenantAccessToken>(path, bytes)?;
                let token_hash = open_sealed_string(
                    &entity.sealed_token_hash,
                    "tenant access token hash",
                    path,
                    kp,
                )?;
                let token = TenantAccessToken {
                    id: entity.id.clone(),
                    tenant_id: entity.tenant_id.clone(),
                    name: entity.name.clone(),
                    token_prefix: entity.token_prefix.clone(),
                    token_hash: token_hash.clone(),
                    token_last4: entity.token_last4.clone(),
                    enabled: entity.enabled,
                    expires_at: entity.expires_at.clone(),
                    created_at: entity.created_at.clone(),
                    updated_at: entity.updated_at.clone(),
                };
                fidelity.tenant_access_tokens.push(token.clone());
                // Unlike identity rows, tenant-token config is enabled-only: a revoked token is
                // simply GONE (plan §3.3), so only enabled rows enter `ConfigData.tenant_tokens`.
                if token.enabled {
                    cfg.tenant_tokens.insert(
                        token_hash.clone(),
                        TenantTokenIdentity {
                            tenant_id: token.tenant_id.clone(),
                            token_name: token.name.clone(),
                            expires_at: token.expires_at.clone(),
                        },
                    );
                }
            }
            EntityPath::GlobalRole(_) => {
                let entity = decode::<GlobalRoleTreeEntity>(path, bytes)?;
                let mut role = entity.role;
                role.provider_ids = entity.provider_ids.clone();
                fidelity.global_roles.push(role.clone());
                // Disabled Global Roles must match NOTHING, not even as failover candidates,
                // so only enabled rows enter the config vector while the full rows stay in
                // fidelity for faithful restore.
                if role.enabled {
                    cfg.global_roles.push(role);
                }
            }
            // Not part of the config tree (yet). Refused rather than skipped: a skipped path
            // means this node is reading a tree written by another version and would serve a
            // config silently missing that entity.
            other => {
                return Err(EntityCodecError::Unsupported {
                    reason: format!(
                        "entity {} is not part of the config tree this build knows",
                        other.to_key_segment()
                    ),
                })
            }
        }
    }

    // ## Restoring the LOADER's vector orders
    //
    // The tree stores one entity per row and this rebuilds the vectors by re-collecting them, so
    // the order has to be reproduced here — and it must be the order the LEADER's loader produced,
    // not one invented here. The one key below mirrors `store.rs`'s `ORDER BY` exactly:
    //
    //   limit_role           ORDER BY created_at, id
    //
    // That SQL key is backed by a UNIQUE constraint (`limit_role.id` is the PK), so the order is
    // TOTAL and the trailing `id` below only makes the sort deterministic, never reorders a tie.
    //
    // An earlier version sorted every vector by `id`. It looked tidy and was a rule the loader
    // never used, so the replica held a differently-ordered config than the leader. Today that
    // changes no behaviour (all matching limit roles are enforced as independent keys), which is
    // exactly why it survived review — but "this tree names this config" is only checkable by
    // EQUALITY, and a silently reordered decode makes every such comparison worthless. Caught by
    // `tests/arachne_order_fidelity.rs`, whose fixture makes the two orders disagree.
    cfg.limit_roles
        .sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));

    // User-hierarchy vectors restore the loader's SQL orders (plan §3.2/§3.5):
    //   user_group           ORDER BY tenant_id, name
    //   user                 ORDER BY id
    //   user_api_key         ORDER BY id
    //   tenant_access_token  ORDER BY id
    //   global_role          ORDER BY id
    // The trailing `id` on the user-group sort only makes the order total; the table's
    // UNIQUE(tenant_id, name) makes it a no-op for ties.
    cfg.user_groups
        .sort_by(|a, b| (&a.tenant_id, &a.name, &a.id).cmp(&(&b.tenant_id, &b.name, &b.id)));
    fidelity
        .user_groups
        .sort_by(|a, b| (&a.tenant_id, &a.name, &a.id).cmp(&(&b.tenant_id, &b.name, &b.id)));
    fidelity.users.sort_by(|a, b| a.id.cmp(&b.id));
    fidelity.user_api_keys.sort_by(|a, b| a.id.cmp(&b.id));
    fidelity
        .tenant_access_tokens
        .sort_by(|a, b| a.id.cmp(&b.id));
    cfg.global_roles.sort_by(|a, b| a.id.cmp(&b.id));
    fidelity.global_roles.sort_by(|a, b| a.id.cmp(&b.id));

    cfg.reindex_tenants();
    // Identity and Global Role derived indexes now have their full inputs on the tree:
    // full user rows + full user-api-key rows, and full/enabled-appropriate global roles.
    // `reindex_identity` consumes `fidelity.user_api_keys` (full table), NOT the enabled-only
    // config vector, exactly like `store::build_config` does.
    cfg.reindex_identity(&fidelity.user_api_keys);
    cfg.reindex_global_roles();
    Ok((cfg, fidelity))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hydra_core::config::{CertMeta, ModelProvider};
    use hydra_core::model::{
        BillingStatus, Provider, ProviderKey, ProviderModel, Tenant, TenantModel, TenantProvider,
    };

    fn tenant(id: &str, domain: &str) -> Tenant {
        Tenant {
            id: id.to_string(),
            name: format!("name-{id}"),
            domain: domain.to_string(),
            auth_url: format!("https://auth.{domain}"),
            cert_key: None,
            cert_file: None,
            enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn provider(id: &str) -> Provider {
        Provider {
            id: id.to_string(),
            key: format!("key-{id}"),
            name: format!("provider-{id}"),
            endpoint: format!("https://{id}.example"),
            weight: 1,
            created_at: String::new(),
            updated_at: String::new(),
            max_concurrency: None,
            max_queue_depth: None,
            queue_wait_timeout_ms: None,
        }
    }

    fn config() -> ConfigData {
        let mut cfg = ConfigData::default();
        cfg.tenants_by_domain
            .insert("acme.example".into(), tenant("t1", "acme.example"));
        cfg.tenants_by_domain
            .insert("globex.example".into(), tenant("t2", "globex.example"));
        cfg.providers.insert("p1".into(), provider("p1"));
        cfg.provider_keys
            .insert("p1".into(), vec!["sk-one".into(), "sk-two".into()]);
        // One enabled role with a client key: the `limit_role/<id>` entity carries the SAME secret
        // the fidelity entity does, and it is a separate encoder path.
        cfg.limit_roles.push(LimitRole {
            id: "r-config".into(),
            name: "config role".into(),
            matching_key: Some(LIMIT_KEY.into()),
            matching_model: None,
            matching_tenant: None,
            matching_provider: None,
            limit_count: Some(10),
            limit_token: None,
            window: "m".into(),
            enabled: true,
            created_at: "2026-01-06T00:00:00Z".into(),
        });
        cfg.models_by_key.insert(
            "gpt-x".into(),
            vec![ModelProvider {
                provider_id: "p1".into(),
                weight: 1,
            }],
        );
        cfg.tenant_models
            .insert("t1".into(), HashSet::from(["gpt-x".to_string()]));
        cfg.certs.insert(
            "acme.example".into(),
            CertMeta {
                domain: "acme.example".into(),
                cert_file: None,
                cert_key: None,
                cert_pem: Some("-----BEGIN CERTIFICATE-----".into()),
                cert_key_pem: Some("-----BEGIN PRIVATE KEY-----".into()),
            },
        );
        cfg.reindex_tenants();
        cfg
    }

    /// The fidelity rows a replica needs BEYOND `ConfigData`, including the rows the derived
    /// maps drop: a DISABLED limit role, a DISABLED binding, an OFFLINE model, and the
    /// provider-key identity a rebuild must not re-mint.
    fn fidelity() -> FidelityRows {
        FidelityRows {
            limit_roles: vec![
                LimitRole {
                    id: "r-enabled".into(),
                    name: "enabled role".into(),
                    // A live client key: the one column decision D-16 seals. It is here so the
                    // "no plaintext secret" test below has something to fail on.
                    matching_key: Some(LIMIT_KEY.into()),
                    matching_model: None,
                    matching_tenant: None,
                    matching_provider: None,
                    limit_count: Some(10),
                    limit_token: None,
                    window: "1m".into(),
                    enabled: true,
                    created_at: "2026-01-01T00:00:00Z".into(),
                },
                LimitRole {
                    id: "r-disabled".into(),
                    name: "disabled role".into(),
                    matching_key: None,
                    matching_model: None,
                    matching_tenant: None,
                    matching_provider: None,
                    limit_count: None,
                    limit_token: None,
                    window: "1m".into(),
                    enabled: false,
                    created_at: "2026-01-02T00:00:00Z".into(),
                },
            ],
            provider_keys: vec![
                ProviderKey {
                    id: "pk-1".into(),
                    provider_id: "p1".into(),
                    api_key: "sk-one".into(),
                    created_at: "2026-01-04T00:00:00Z".into(),
                },
                ProviderKey {
                    id: "pk-2".into(),
                    provider_id: "p1".into(),
                    api_key: "sk-two".into(),
                    created_at: "2026-01-05T00:00:00Z".into(),
                },
            ],
            tenant_token_hashes: vec![("t1".into(), "a".repeat(64))],
            provider_models: vec![
                ProviderModel {
                    id: "pm-online".into(),
                    key: "gpt-x".into(),
                    name: "GPT X".into(),
                    provider_id: "p1".into(),
                    status: 1,
                },
                ProviderModel {
                    id: "pm-offline".into(),
                    key: "gpt-old".into(),
                    name: "GPT old".into(),
                    provider_id: "p1".into(),
                    status: -1,
                },
            ],
            tenant_providers: vec![TenantProvider {
                id: "tp-1".into(),
                tenant_id: "t1".into(),
                provider_id: "p1".into(),
            }],
            tenant_models: vec![TenantModel {
                id: "tm-1".into(),
                tenant_id: "t1".into(),
                model_key: "gpt-x".into(),
            }],
            // This fixture predates the T2 user-hierarchy paths; keep those fields empty here
            // so the test below remains about the legacy fidelity carrier it was written for.
            // Dedicated `new_user_hierarchy...` tests cover the per-row paths.
            ..FidelityRows::default()
        }
    }

    fn kp() -> crate::crypto::StaticKeyProvider {
        crate::crypto::StaticKeyProvider::new([7u8; 32], 1)
    }

    /// The client key the fixtures put in `matching_key` (decision D-16 sealed this column).
    const LIMIT_KEY: &str = "sk-limit-role-secret";

    /// The whole reason the fidelity entity exists: a replica must be able to rebuild its
    /// SQLite tables BYTE-FAITHFULLY, which the derived maps cannot express.
    ///
    /// Every assertion below is a row the old path would have silently lost: a disabled limit
    /// role and binding (never in `ConfigData`), an offline model (dropped by
    /// `models_by_key`), and the provider-key PRIMARY KEYS a rebuild must not re-mint.
    ///
    /// Falsification: send only `ConfigData` and the disabled/offline assertions fail.
    #[tokio::test]
    async fn fidelity_rows_survive_the_tree_so_a_replica_can_rebuild_byte_faithfully() {
        let cfg = config();
        let rows = fidelity();
        let blobs = split_config(&cfg, &rows, &kp()).expect("split");
        let tree = tree_of(&blobs).expect("tree");

        let (rebuilt_cfg, rebuilt_rows) =
            build_config_with_fidelity(&tree, &kp()).expect("rebuild");

        // The config must come back WHOLE, cert private keys included: `CertMeta::cert_key_pem`
        // is `skip_serializing` (a private key never enters a serialised form), so the entity
        // carries it SEALED and the decode re-supplies it. Asserting the key is back is the point
        // — an equality that had zeroed it would have accepted a tree that loses the key.
        assert_eq!(rebuilt_cfg, cfg, "the config must round trip");
        assert_eq!(
            rebuilt_cfg
                .certs
                .get("acme.example")
                .and_then(|c| c.cert_key_pem.as_deref()),
            Some("-----BEGIN PRIVATE KEY-----"),
            "the cert private key must survive the round trip, or `restore_config` writes NULL \
             over the key a node already had"
        );
        assert_eq!(
            rebuilt_rows, rows,
            "the fidelity rows must round trip, disabled and offline rows included"
        );
        // Spelled out, because "they are equal" is only convincing if the rows exist at all.
        assert_eq!(
            rebuilt_rows.limit_roles.len(),
            2,
            "the disabled role must survive"
        );
        assert!(
            rebuilt_rows.limit_roles.iter().any(|r| !r.enabled),
            "a DISABLED limit role exists in the database and must reach the replica"
        );
        assert!(
            rebuilt_rows.provider_models.iter().any(|m| m.status != 1),
            "an OFFLINE model is not in `models_by_key` and must reach the replica"
        );
        assert_eq!(
            rebuilt_rows.provider_keys[0].id, "pk-1",
            "provider-key identity must be preserved, not re-minted"
        );
        assert_eq!(
            rebuilt_rows.provider_keys[0].created_at,
            "2026-01-04T00:00:00Z"
        );
    }

    /// The fidelity entity must not become a plaintext channel for secrets.
    ///
    /// The sealed-rows pattern is not decoration: `provider_keys` hold API keys and
    /// `tenant_token_hashes` are the fleet's auth material, and the existing wire seals BOTH.
    /// A tree that carried them in the clear would quietly undo that.
    ///
    /// Falsification: serialize the rows as-is and the "no plaintext secret" assertions fail.
    #[tokio::test]
    async fn the_fidelity_entity_carries_no_plaintext_secrets() {
        let cfg = config();
        let rows = fidelity();
        let blobs = split_config(&cfg, &rows, &kp()).expect("split");

        for blob in &blobs {
            let text = String::from_utf8_lossy(&blob.bytes);
            assert!(
                !text.contains("sk-one") && !text.contains("sk-two"),
                "entity {} carries a provider API key in the clear",
                blob.path.to_key_segment()
            );
            assert!(
                !text.contains(&"a".repeat(64)),
                "entity {} carries a tenant token hash in the clear",
                blob.path.to_key_segment()
            );
            assert!(
                !text.contains("PRIVATE KEY"),
                "entity {} carries private key material",
                blob.path.to_key_segment()
            );
        }

        // ...and the values DO come back, through the key provider.
        let tree = tree_of(&blobs).expect("tree");
        let (_, rebuilt) = build_config_with_fidelity(&tree, &kp()).expect("rebuild");
        assert_eq!(rebuilt.provider_keys[0].api_key, "sk-one");
        assert_eq!(rebuilt.tenant_token_hashes[0].1, "a".repeat(64));

        // A different master key must refuse rather than hand back garbage.
        let wrong = crate::crypto::StaticKeyProvider::new([9u8; 32], 1);
        let got = build_config(&tree, &wrong);
        assert!(
            got.is_err(),
            "the wrong master key must be refused, not silently accepted"
        );
    }

    /// `limit_role.matching_key` must not reach the store in the clear — in EITHER carrier.
    ///
    /// The role travels twice: once as its own `limit_role/<id>` entity (the ENABLED rows, which is
    /// what the matcher reads) and once inside the fidelity entity (FULL rows, disabled included).
    /// Both were plain JSON until decision D-16① sealed the column, and a tree carrying a live client
    /// key puts it in every node's Arachne store — the thing the decision exists to stop. Two encoder
    /// paths means two chances to forget, so this test walks EVERY blob rather than the one it
    /// remembers.
    ///
    /// The last part is the other half of the same decision: a PLAINTEXT value in a current-format
    /// entity is refused rather than read. Compatibility with an older build lives in the TOC FORMAT
    /// (D-16 bumped it), not in a per-field fallback — see [`opened_limit_role`] for why a fallback
    /// there is the fail-open direction.
    ///
    /// Falsification: encode `role` directly in `split_config` (the pre-D-16 code) and the
    /// `contains(LIMIT_KEY)` assertion fails; seal in the fidelity entity but not the per-role entity
    /// and it fails on the other blob; read a plaintext value as plaintext (the pre-bump rule) and
    /// the refusal assertion fails.
    #[tokio::test]
    async fn no_limit_role_entity_carries_a_plaintext_matching_key() {
        let cfg = config();
        let rows = fidelity();
        let blobs = split_config(&cfg, &rows, &kp()).expect("split");

        assert!(
            cfg.limit_roles
                .iter()
                .any(|r| r.matching_key.as_deref() == Some(LIMIT_KEY)),
            "fixture: the CONFIG entity path must carry a limit key"
        );
        assert!(
            rows.limit_roles
                .iter()
                .any(|r| r.matching_key.as_deref() == Some(LIMIT_KEY)),
            "fixture: the FIDELITY entity path must carry a limit key"
        );

        for blob in &blobs {
            let text = String::from_utf8_lossy(&blob.bytes);
            assert!(
                !text.contains(LIMIT_KEY),
                "entity {} carries a client key in the clear",
                blob.path.to_key_segment()
            );
        }

        // ...and both come back, through the key provider.
        let tree = tree_of(&blobs).expect("tree");
        let (rebuilt_cfg, rebuilt_rows) =
            build_config_with_fidelity(&tree, &kp()).expect("rebuild");
        assert_eq!(
            rebuilt_cfg
                .limit_roles
                .iter()
                .find(|r| r.id == "r-config")
                .and_then(|r| r.matching_key.as_deref()),
            Some(LIMIT_KEY),
            "the config entity's key must survive the round trip"
        );
        assert_eq!(
            rebuilt_rows
                .limit_roles
                .iter()
                .find(|r| r.id == "r-enabled")
                .and_then(|r| r.matching_key.as_deref()),
            Some(LIMIT_KEY),
            "the fidelity entity's key must survive the round trip"
        );

        // A different master key must refuse rather than hand back garbage — for THIS column too,
        // because a silently-empty `matching_key` is not "no match", it is `None` = match-all.
        let wrong = crate::crypto::StaticKeyProvider::new([9u8; 32], 1);
        assert!(
            build_config(&tree, &wrong).is_err(),
            "the wrong master key must not yield a config whose limit roles lost their keys"
        );

        // A PLAINTEXT value in a current-format entity is REFUSED, not read. Compatibility with an
        // older build is the TOC FORMAT's job (decision D-16 bumped it to 4), not a per-field
        // fallback: an old build's plaintext and this build's envelope are the same field, so
        // "accept both" is how a node ends up matching against `sealed:v1:…` and quietly stops
        // enforcing a limit. Falsification: read it as plaintext (the pre-bump rule) and this fails.
        let legacy_role = LimitRole {
            id: "r-legacy".into(),
            matching_key: Some(LIMIT_KEY.into()),
            ..cfg.limit_roles[0].clone()
        };
        let legacy = tree_of(&[EntityBlob {
            path: EntityPath::LimitRole("r-legacy".into()),
            bytes: serde_json::to_vec(&legacy_role).expect("encode"),
        }])
        .expect("tree");
        let refused = build_config(&legacy, &kp());
        assert!(
            matches!(refused, Err(EntityCodecError::Crypto { .. })),
            "a plaintext matching key in a TOC_FORMAT {TOC_FORMAT} entity must be refused by \
             name, got {refused:?}"
        );
    }

    /// A publish that changes fidelity but not the entities still names a new tree: the
    /// disabled rows are part of the replicated state, so a change to them must replicate.
    #[tokio::test]
    async fn a_fidelity_change_names_a_new_tree() {
        let cfg = config();
        let before = fidelity();
        // `after` is a CLONE of `before` with one row flipped: `config()` seals its provider keys
        // with a fresh AES-GCM nonce, so calling it twice would compare two DIFFERENT configs and
        // the extra differing entity would be the fixture's fault, not the codec's.
        let mut after = before.clone();
        after.limit_roles[1].enabled = true;

        // The provider-key entity is sealed per call with a fresh nonce, so it differs between
        // two splits BY CONSTRUCTION — which is exactly the determinism problem the production
        // path avoids by taking the STORED ciphertext. This test is about the fidelity entity, so
        // the comparison is restricted to it, and the determinism property is asserted separately
        // by `the_same_inputs_name_the_same_tree`.
        let t1 = tree_of(&split_config(&cfg, &before, &kp()).expect("split")).expect("tree");
        let t2 = tree_of(&split_config(&cfg, &after, &kp()).expect("split")).expect("tree");

        assert_ne!(
            t1.get(&EntityPath::Fidelity),
            t2.get(&EntityPath::Fidelity),
            "the fidelity entity must change when a row does"
        );
        // Every CONFIG entity is untouched, which is the point of keying by path: the row that
        // changed lives in the fidelity entity, and no tenant/provider/model is rewritten.
        let differing_config: Vec<_> = t1
            .iter()
            .filter(|(p, b)| **p != EntityPath::Fidelity && t2.get(*p) != Some(b))
            .map(|(p, _)| p.to_key_segment())
            .collect();
        assert!(
            differing_config.is_empty(),
            "only the fidelity entity may change, but these config entities did: {differing_config:?}"
        );
    }

    /// Two INDEPENDENT publisher runs must name the SAME tree. This is THE property the tree's
    /// name rests on, and it has three separate ways to fail:
    ///
    /// 1. **Sealed secrets.** The tree carries provider api-keys, cert private keys and tenant
    ///    token hashes SEALED. A random nonce per seal made one unchanged config name two trees,
    ///    which is why the encoder uses `seal_deterministic` (and why an earlier version made the
    ///    CALLER supply pre-sealed material instead — a workaround that only moved the question).
    /// 2. **Set iteration order.** `cfg.tenant_providers` / `cfg.tenant_models` are
    ///    `HashMap<String, HashSet<String>>`, and serde serialises a `HashSet` in ITERATION order,
    ///    which `RandomState` decides per instance. Encoding the set directly gave the same
    ///    logical config different bytes on the next read.
    /// 3. **Entity order.** Handled by sorting the blobs by path; asserted by
    ///    `a_toc_round_trips_and_its_order_is_part_of_its_identity`.
    ///
    /// The fixture therefore has to defeat BOTH traps at once, and the two builds must be
    /// genuinely independent — this test builds the config twice, on purpose. A test that splits
    /// ONE config object twice would pass with every one of these bugs present, because both calls
    /// would see the same `HashSet` instances and the same already-sealed values. (That is exactly
    /// how the first versions of this file passed while both bugs were live.)
    ///
    /// Falsification: swap `seal_deterministic` back to `seal`, or `sorted_members` back to the raw
    /// set, and this fails.
    #[test]
    fn new_user_hierarchy_rows_round_trip_without_double_encoding() {
        let mut cfg = config();
        let groups = vec![
            UserGroup {
                id: "g-disabled".into(),
                tenant_id: "t1".into(),
                name: "a-disabled".into(),
                enabled: false,
                created_at: "2026-02-01T00:00:00Z".into(),
                updated_at: "2026-02-01T00:00:00Z".into(),
            },
            UserGroup {
                id: "g-enabled".into(),
                tenant_id: "t1".into(),
                name: "b-enabled".into(),
                enabled: true,
                created_at: "2026-02-01T00:00:00Z".into(),
                updated_at: "2026-02-01T00:00:00Z".into(),
            },
        ];
        let users = vec![
            User {
                id: "u-disabled".into(),
                tenant_id: "t1".into(),
                user_group_id: "g-disabled".into(),
                external_id: Some("e-disabled".into()),
                name: "Disabled user".into(),
                enabled: false,
                billing_status: BillingStatus::Delinquent,
                billing_reason: Some("over quota".into()),
                billing_updated_at: Some("2026-02-02T00:00:00Z".into()),
                created_at: "2026-02-02T00:00:00Z".into(),
                updated_at: "2026-02-02T00:00:00Z".into(),
            },
            User {
                id: "u-enabled".into(),
                tenant_id: "t1".into(),
                user_group_id: "g-enabled".into(),
                external_id: None,
                name: "Enabled user".into(),
                enabled: true,
                billing_status: BillingStatus::Active,
                billing_reason: None,
                billing_updated_at: None,
                created_at: "2026-02-03T00:00:00Z".into(),
                updated_at: "2026-02-03T00:00:00Z".into(),
            },
        ];
        let keys = vec![
            UserApiKey {
                id: "k-disabled".into(),
                user_id: "u-disabled".into(),
                name: "revoked key".into(),
                key_hash: "d".repeat(64),
                key_prefix: "hk_dis".into(),
                key_last4: "1111".into(),
                enabled: false,
                expires_at: Some("2026-03-01T00:00:00Z".into()),
                created_at: "2026-02-04T00:00:00Z".into(),
                updated_at: "2026-02-04T00:00:00Z".into(),
            },
            UserApiKey {
                id: "k-enabled".into(),
                user_id: "u-enabled".into(),
                name: "live key".into(),
                key_hash: "e".repeat(64),
                key_prefix: "hk_ena".into(),
                key_last4: "2222".into(),
                enabled: true,
                expires_at: None,
                created_at: "2026-02-05T00:00:00Z".into(),
                updated_at: "2026-02-05T00:00:00Z".into(),
            },
        ];
        let tokens = vec![
            TenantAccessToken {
                id: "tok-disabled".into(),
                tenant_id: "t1".into(),
                name: "disabled token".into(),
                token_hash: "f".repeat(64),
                token_prefix: "ht_dis".into(),
                token_last4: "3333".into(),
                enabled: false,
                expires_at: None,
                created_at: "2026-02-06T00:00:00Z".into(),
                updated_at: "2026-02-06T00:00:00Z".into(),
            },
            TenantAccessToken {
                id: "tok-enabled".into(),
                tenant_id: "t1".into(),
                name: "enabled token".into(),
                token_hash: "a".repeat(64),
                token_prefix: "ht_ena".into(),
                token_last4: "4444".into(),
                enabled: true,
                expires_at: None,
                created_at: "2026-02-07T00:00:00Z".into(),
                updated_at: "2026-02-07T00:00:00Z".into(),
            },
        ];
        let roles = vec![
            GlobalRole {
                id: "gr-disabled".into(),
                name: "disabled role".into(),
                tenant_id: "t1".into(),
                user_group_id: "g-disabled".into(),
                user_id: "u-disabled".into(),
                api_key_id: "k-disabled".into(),
                model_key: "gpt-x".into(),
                provider_ids: vec!["p-disabled".into()],
                enabled: false,
                created_at: "2026-02-08T00:00:00Z".into(),
                updated_at: "2026-02-08T00:00:00Z".into(),
            },
            GlobalRole {
                id: "gr-enabled".into(),
                name: "enabled role".into(),
                tenant_id: "t1".into(),
                user_group_id: "g-enabled".into(),
                user_id: "u-enabled".into(),
                api_key_id: "k-enabled".into(),
                model_key: "gpt-x".into(),
                provider_ids: vec!["p1".into()],
                enabled: true,
                created_at: "2026-02-09T00:00:00Z".into(),
                updated_at: "2026-02-09T00:00:00Z".into(),
            },
        ];

        cfg.user_groups = groups.clone();
        cfg.users.clear();
        for user in &users {
            cfg.users.insert(user.id.clone(), user.clone());
        }
        cfg.tenant_tokens.clear();
        for token in &tokens {
            if token.enabled {
                cfg.tenant_tokens.insert(
                    token.token_hash.clone(),
                    TenantTokenIdentity {
                        tenant_id: token.tenant_id.clone(),
                        token_name: token.name.clone(),
                        expires_at: token.expires_at.clone(),
                    },
                );
            }
        }
        cfg.global_roles = roles.iter().filter(|r| r.enabled).cloned().collect();
        cfg.reindex_tenants();
        cfg.reindex_identity(&keys);
        cfg.reindex_global_roles();

        let rows = FidelityRows {
            user_groups: groups.clone(),
            users: users.clone(),
            user_api_keys: keys.clone(),
            tenant_access_tokens: tokens.clone(),
            global_roles: roles.clone(),
            ..FidelityRows::default()
        };

        let blobs = split_config(&cfg, &rows, &kp()).expect("split new hierarchy config");
        for segment in &[
            "user_group/g-disabled",
            "user_group/g-enabled",
            "user/u-disabled",
            "user/u-enabled",
            "user_api_key/k-disabled",
            "user_api_key/k-enabled",
            "tenant_access_token/tok-disabled",
            "tenant_access_token/tok-enabled",
            "global_role/gr-disabled",
            "global_role/gr-enabled",
        ] {
            assert!(
                blobs
                    .iter()
                    .any(|blob| blob.path.to_key_segment().as_str() == *segment),
                "missing per-row entity for {segment}",
            );
        }

        for blob in &blobs {
            let text = String::from_utf8_lossy(&blob.bytes).to_string();
            assert!(
                !text.contains("d".repeat(64).as_str()) && !text.contains("f".repeat(64).as_str()),
                "entity {} carries a user-key/tenant-token hash in the clear",
                blob.path.to_key_segment(),
            );
        }

        let fid_blob = blobs
            .iter()
            .find(|blob| blob.path.to_key_segment() == "_fidelity")
            .expect("fidelity entity");
        let fid_text = String::from_utf8_lossy(&fid_blob.bytes).to_string();
        for forbidden in &[
            "user_groups",
            "users",
            "user_api_keys",
            "tenant_access_tokens",
            "global_roles",
            "g-disabled",
            "u-disabled",
            "k-disabled",
            "tok-disabled",
            "gr-disabled",
        ] {
            assert!(
                !fid_text.contains(forbidden),
                "the _fidelity entity carries {forbidden} again; new hierarchy rows must be single-encoded",
            );
        }

        let tree = tree_of(&blobs).expect("tree of new hierarchy blobs");
        let (rebuilt_cfg, rebuilt_rows) =
            build_config_with_fidelity(&tree, &kp()).expect("decode new hierarchy tree");

        assert_eq!(
            rebuilt_rows.user_groups, groups,
            "full user-group rows must round-trip"
        );
        assert_eq!(rebuilt_rows.users, users, "full user rows must round-trip");
        assert_eq!(
            rebuilt_rows.user_api_keys, keys,
            "full user API-key rows must round-trip"
        );
        assert_eq!(
            rebuilt_rows.tenant_access_tokens, tokens,
            "full tenant access-token rows must round-trip",
        );
        assert_eq!(
            rebuilt_rows.global_roles, roles,
            "full global-role rows must round-trip"
        );

        assert_eq!(
            rebuilt_cfg.user_groups, groups,
            "config keeps disabled groups as known state"
        );
        assert_eq!(
            rebuilt_cfg.users, cfg.users,
            "config keeps disabled/delinquent users"
        );
        assert_eq!(
            rebuilt_cfg.tenant_tokens, cfg.tenant_tokens,
            "tenant token config remains enabled-only"
        );
        assert_eq!(
            rebuilt_cfg.global_roles, cfg.global_roles,
            "global-role config remains enabled-only"
        );
        assert!(
            rebuilt_cfg
                .users_by_api_key_hash
                .values()
                .any(|id| id.principal.api_key_id == "k-disabled"),
            "a revoked key must remain known, not vanish into an unknown 401",
        );
        assert!(
            rebuilt_cfg
                .global_roles
                .iter()
                .any(|role| role.id == "gr-enabled"),
            "enabled global role must be present on the replica",
        );
        assert!(
            rebuilt_cfg
                .global_roles
                .iter()
                .any(|role| role.id == "gr-enabled" && role.provider_ids == vec!["p1".to_string()]),
            "global-role provider vector must survive per-row entity decode",
        );
    }

    #[test]
    #[ignore = "capacity guard: run manually when changing per-row entity overhead"]
    fn new_user_hierarchy_toc_and_values_stay_within_target_budgets() {
        for n in [1usize, 10usize, 100usize, 500usize] {
            let mut cfg = config();
            let provider_id = format!("cap_p{n}");
            let tenant = Tenant {
                id: format!("cap_t{n}"),
                name: format!("name-cap_t{n}"),
                domain: format!("cap{n}.example"),
                auth_url: format!("https://auth.cap{n}.example"),
                cert_key: None,
                cert_file: None,
                enabled: true,
                created_at: String::new(),
                updated_at: String::new(),
            };
            cfg.tenants_by_domain
                .insert(tenant.domain.clone(), tenant.clone());
            cfg.providers
                .insert(provider_id.clone(), provider(provider_id.as_str()));

            let group_id = format!("g{n}");
            let user_id = format!("u{n}");
            let key_id = format!("k{n}");
            let token_id = format!("tok{n}");
            let key_hash = format!("{:064x}", 0x1000 + n);
            let token_hash = format!("{:064x}", 0x2000 + n);
            let user_group = UserGroup {
                id: group_id.clone(),
                tenant_id: tenant.id.clone(),
                name: group_id.clone(),
                enabled: true,
                created_at: String::new(),
                updated_at: String::new(),
            };
            let user = User {
                id: user_id.clone(),
                tenant_id: tenant.id.clone(),
                user_group_id: group_id.clone(),
                external_id: None,
                name: user_id.clone(),
                enabled: true,
                billing_status: BillingStatus::Active,
                billing_reason: None,
                billing_updated_at: None,
                created_at: String::new(),
                updated_at: String::new(),
            };
            let key = UserApiKey {
                id: key_id.clone(),
                user_id: user_id.clone(),
                name: key_id.clone(),
                key_hash: key_hash.clone(),
                key_prefix: key_id.clone(),
                key_last4: key_id.clone(),
                enabled: true,
                expires_at: None,
                created_at: String::new(),
                updated_at: String::new(),
            };
            let token = TenantAccessToken {
                id: token_id.clone(),
                tenant_id: tenant.id.clone(),
                name: token_id.clone(),
                token_hash: token_hash.clone(),
                token_prefix: token_id.clone(),
                token_last4: token_id.clone(),
                enabled: true,
                expires_at: None,
                created_at: String::new(),
                updated_at: String::new(),
            };
            let role_count = n.saturating_sub(4);
            let global_roles: Vec<GlobalRole> = (0usize..role_count)
                .map(|i| GlobalRole {
                    id: format!("gr{n}-{i:06}"),
                    name: format!("role {n} {i}"),
                    tenant_id: tenant.id.clone(),
                    user_group_id: group_id.clone(),
                    user_id: user_id.clone(),
                    api_key_id: key_id.clone(),
                    model_key: "gpt-x".into(),
                    provider_ids: vec![provider_id.clone()],
                    enabled: true,
                    created_at: String::new(),
                    updated_at: String::new(),
                })
                .collect();

            cfg.user_groups = vec![user_group.clone()];
            cfg.users.clear();
            cfg.users.insert(user_id.clone(), user.clone());
            cfg.tenant_tokens.clear();
            cfg.tenant_tokens.insert(
                token_hash.clone(),
                TenantTokenIdentity {
                    tenant_id: tenant.id.clone(),
                    token_name: token_id.clone(),
                    expires_at: None,
                },
            );
            cfg.global_roles = global_roles.clone();
            cfg.reindex_tenants();
            cfg.reindex_identity(std::slice::from_ref(&key));
            cfg.reindex_global_roles();

            let rows = FidelityRows {
                user_groups: vec![user_group.clone()],
                users: vec![user.clone()],
                user_api_keys: vec![key.clone()],
                tenant_access_tokens: vec![token.clone()],
                global_roles: global_roles.clone(),
                ..FidelityRows::default()
            };

            let blobs = split_config(&cfg, &rows, &kp()).expect("capacity split");
            let tree = tree_of(&blobs).expect("capacity tree");
            let toc = toc_of(&tree).expect("capacity toc");
            let entity_count = blobs.len();
            let value_bytes: usize = blobs.iter().map(|blob| blob.bytes.len()).sum();
            let toc_bytes = toc.encode().len();

            assert!(
                entity_count < 4096,
                "{n}-row hierarchy: expected fewer than 4096 entities, got {entity_count}",
            );
            assert!(
                value_bytes < 1024 * 1024,
                "{n}-row hierarchy: expected under 1 MiB of entity values, got {value_bytes} bytes",
            );
            assert!(
                toc_bytes < 1024 * 1024,
                "{n}-row hierarchy: expected under 1 MiB of TOC, got {toc_bytes} bytes",
            );
        }
    }
    #[tokio::test]
    async fn two_independent_builds_of_the_same_config_name_the_same_tree() {
        // Two INDEPENDENT constructions, the way two loader runs would produce them: fresh
        // `HashSet`s (new hash seeds) and fresh plaintext to seal.
        fn build() -> ConfigData {
            let mut cfg = config();
            cfg.tenant_models.insert(
                "t1".to_string(),
                (0..64).map(|i| format!("model-{i:02}")).collect(),
            );
            cfg.tenant_providers.insert(
                "t1".to_string(),
                (0..64).map(|i| format!("provider-{i:02}")).collect(),
            );
            cfg
        }

        let rows = fidelity();
        let first = tree_of(&split_config(&build(), &rows, &kp()).expect("split")).expect("tree");
        let second = tree_of(&split_config(&build(), &rows, &kp()).expect("split")).expect("tree");

        // NON-VACUOUS: the fixture really does carry secrets, and they really are sealed —
        // otherwise "the two trees are equal" would be asserting that two empty things match.
        let sealed_blobs = split_config(&build(), &rows, &kp()).expect("split");
        for blob in &sealed_blobs {
            let text = String::from_utf8_lossy(&blob.bytes);
            assert!(
                !text.contains("sk-one") && !text.contains("PRIVATE KEY"),
                "fixture: entity {} carries a secret in the clear, so this test is not exercising \
                 deterministic sealing at all",
                blob.path.to_key_segment()
            );
        }

        assert_eq!(
            first, second,
            "two independent builds of the same logical config must name the same tree; a set \
             serialised in iteration order makes the name depend on the process's hash seed, so \
             every publish renames the tree even when nothing changed"
        );
    }
}
