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
//! no private key material enters the tree from the config side.
//!
//! ## What the tree carries, and in which form
//!
//! A replica is not only `ConfigData`: it also needs the rows `ConfigData` is
//! derived from (disabled `limit_role` rows, offline `provider_model` rows, row
//! ids) plus the two secret-bearing sets — [`FidelityTreeEntity`]. The rows that
//! are not secret travel as plain JSON; provider api-keys and tenant token hashes
//! travel [`Sealed`], the same treatment the snapshot wire gives them, and are
//! opened only by a holder of the master key ([`FidelityTreeEntity::rows`]).
//!
//! ## Why the encoding path takes no key provider
//!
//! Because the tree is content-addressed. AES-GCM seals with a fresh random nonce,
//! so a value sealed *during* encoding differs on every publish for identical
//! plaintext — which renames the tree, advances the head, and makes every node
//! re-materialize forever. Sealing therefore happens once, outside, against stored
//! ciphertext or an equally stable representation, and arrives here as
//! [`SealedMaterial`]. One measured consequence: the same config must name the
//! same tree twice in a row, and there is a test for exactly that.
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
use hydra_core::model::{
    LimitRole, ProviderKey, ProviderKeyBinding, ProviderModel, SubTenant, SubTenantRoute,
    TenantModel, TenantProvider,
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
/// `provider_keys` hold provider API keys and `tenant_token_hashes` hold the fleet's
/// authentication material. The existing wire SEALS both, and a tree that carried them in the
/// clear would quietly undo that — so this type keeps the same split: the rows that are not
/// secret travel as plain JSON, and the two that are travel as [`Sealed`] values that only the
/// master key can open.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FidelityTreeEntity {
    /// Full `limit_role` rows, DISABLED included.
    pub limit_roles: Vec<LimitRole>,
    /// Full `provider_key_binding` rows, DISABLED included.
    pub key_prefix_bindings: Vec<ProviderKeyBinding>,
    /// Full `provider_model` rows, OFFLINE (`status != 1`) included — the derived
    /// `models_by_key` drops those, and a replica must still have them.
    pub provider_models: Vec<ProviderModel>,
    /// Full `tenant_provider` rows, ids preserved.
    pub tenant_providers: Vec<TenantProvider>,
    /// Full `tenant_model` rows, ids preserved.
    pub tenant_models: Vec<TenantModel>,
    /// Full `sub_tenant` rows, DISABLED included.
    pub sub_tenants: Vec<SubTenant>,
    /// Full `sub_tenant_route` rows, DISABLED included.
    pub sub_tenant_routes: Vec<SubTenantRoute>,
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

impl FidelityTreeEntity {
    /// Open the sealed rows and reassemble the full set.
    ///
    /// # Errors
    /// [`EntityCodecError::Crypto`] when the master key cannot open a sealed value — a wrong or
    /// rotated-away key is REFUSED, never guessed at, and the caller keeps its last-known-good
    /// config rather than installing something with secrets missing.
    pub fn rows(&self, kp: &dyn KeyProvider) -> Result<FidelityRows, EntityCodecError> {
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
            limit_roles: self.limit_roles.clone(),
            key_prefix_bindings: self.key_prefix_bindings.clone(),
            provider_keys,
            tenant_token_hashes,
            provider_models: self.provider_models.clone(),
            tenant_providers: self.tenant_providers.clone(),
            tenant_models: self.tenant_models.clone(),
            sub_tenants: self.sub_tenants.clone(),
            sub_tenant_routes: self.sub_tenant_routes.clone(),
        })
    }

    /// Assemble the entity from the rows plus the ALREADY-SEALED secret material.
    ///
    /// ## Why the caller supplies the sealed values instead of this sealing them
    ///
    /// The tree is **content-addressed**: its name is the hash of its bytes, so a value that
    /// changes while its plaintext does not renames the tree, advances the head, and makes every
    /// node re-read the whole config — forever, in a loop. AES-GCM seals with a fresh random
    /// nonce, so sealing HERE would produce different bytes on every publish for identical
    /// plaintext. (Measured while writing this: sealing in this function made one unchanged
    /// config name two different trees.)
    ///
    /// The sealed material therefore comes from where it is already stored and stable — the
    /// leader's `provider_key` and token-hash rows, exactly as `SnapshotWire::build` takes them —
    /// or from a caller that owns an equally stable representation.
    #[must_use]
    pub fn from_sealed(
        rows: &FidelityRows,
        sealed_provider_keys: Vec<SealedProviderKey>,
        sealed_tenant_token_hashes: Vec<(String, Sealed)>,
    ) -> Self {
        Self {
            limit_roles: rows.limit_roles.clone(),
            key_prefix_bindings: rows.key_prefix_bindings.clone(),
            provider_models: rows.provider_models.clone(),
            tenant_providers: rows.tenant_providers.clone(),
            tenant_models: rows.tenant_models.clone(),
            sub_tenants: rows.sub_tenants.clone(),
            sub_tenant_routes: rows.sub_tenant_routes.clone(),
            sealed_provider_keys,
            sealed_tenant_token_hashes,
        }
    }
}

impl SealedMaterial {
    /// Seal the two secret-bearing row sets from the in-memory plaintext.
    ///
    /// Use this when the caller does NOT have the stored ciphertext — a one-shot publish, or a
    /// test. A caller that does have it (the leader reading its own `provider_key` rows) must use
    /// the stored values instead, because a fresh seal is NOT reproducible: `ArachneConfigStore`
    /// names a tree by the hash of its bytes, and a value that changes while its plaintext does
    /// not would rename the tree on every publish.
    ///
    /// # Errors
    /// [`EntityCodecError::Unsupported`] when the master key refuses to seal. A seal failure must
    /// stop the publish, never degrade to plaintext.
    pub fn seal_plaintext(
        cfg: &ConfigData,
        rows: &FidelityRows,
        kp: &dyn KeyProvider,
    ) -> Result<Self, EntityCodecError> {
        let mut provider_keys = std::collections::BTreeMap::new();
        for (id, keys) in &cfg.provider_keys {
            let json = serde_json::to_vec(keys).map_err(|e| EntityCodecError::Serde {
                path: EntityPath::ProviderKey(id.clone()).to_key_segment(),
                reason: e.to_string(),
            })?;
            let sealed = kp.seal(&json).map_err(|e| EntityCodecError::Unsupported {
                reason: format!("cannot seal the provider keys of {id}: {e}"),
            })?;
            provider_keys.insert(id.clone(), sealed);
        }
        let mut fidelity_provider_keys = Vec::with_capacity(rows.provider_keys.len());
        for k in &rows.provider_keys {
            let sealed =
                kp.seal(k.api_key.as_bytes())
                    .map_err(|e| EntityCodecError::Unsupported {
                        reason: format!("cannot seal provider key {}: {e}", k.id),
                    })?;
            fidelity_provider_keys.push(SealedProviderKey {
                id: k.id.clone(),
                provider_id: k.provider_id.clone(),
                created_at: k.created_at.clone(),
                sealed,
            });
        }
        let mut tenant_token_hashes = Vec::with_capacity(rows.tenant_token_hashes.len());
        for (tenant_id, hash) in &rows.tenant_token_hashes {
            let sealed = kp
                .seal(hash.as_bytes())
                .map_err(|e| EntityCodecError::Unsupported {
                    reason: format!("cannot seal the token hash of tenant {tenant_id}: {e}"),
                })?;
            tenant_token_hashes.push((tenant_id.clone(), sealed));
        }
        // The cert private keys, sealed per domain. Sealed HERE rather than at the DB
        // boundary the way `restore_config` does it, because the tree must carry them:
        // `CertMeta::cert_key_pem` is `skip_serializing`, so the entity cannot.
        let mut cert_keys = std::collections::BTreeMap::new();
        for (domain, meta) in &cfg.certs {
            let Some(pem) = &meta.cert_key_pem else {
                continue;
            };
            let sealed = kp
                .seal(pem.as_bytes())
                .map_err(|e| EntityCodecError::Unsupported {
                    reason: format!("cannot seal the private key of the cert for {domain}: {e}"),
                })?;
            cert_keys.insert(domain.clone(), sealed);
        }
        Ok(Self {
            provider_keys,
            fidelity_provider_keys,
            tenant_token_hashes,
            cert_keys,
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
pub fn split_config(
    cfg: &ConfigData,
    fidelity: &FidelityRows,
    sealed: SealedMaterial,
) -> Result<Vec<EntityBlob>, EntityCodecError> {
    let mut out = config_entities(cfg, &sealed.provider_keys, &sealed.cert_keys)?;
    out.push(encode(
        &EntityPath::Fidelity,
        &FidelityTreeEntity::from_sealed(
            fidelity,
            sealed.fidelity_provider_keys,
            sealed.tenant_token_hashes,
        ),
    )?);
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// The already-sealed secret material a publisher supplies.
///
/// ## Why this exists at all
///
/// The tree is content-addressed, so every value must be REPRODUCIBLE: the same logical config
/// has to produce the same bytes, or the tree is renamed on every publish and the whole cluster
/// re-materializes forever. AES-GCM seals with a fresh random nonce, so anything sealed *during*
/// encoding is not reproducible — measured twice while writing this module, first for the
/// fidelity rows and then for the config's own `provider_keys`.
///
/// So sealing happens ONCE, outside, against a stable representation — the ciphertext a node
/// already stores, or a fixture sealed once — and this struct carries the result in.
///
/// **Honest scope note**: the only constructor today is [`Self::seal_plaintext`], which seals
/// afresh and is therefore NOT reproducible across calls; it exists for tests and one-shot
/// publishes. The production constructor, which reads each row's STORED ciphertext
/// (`provider_key.api_key_ciphertext`, `tenant.cert_key_ciphertext`) instead of sealing again,
/// belongs to the publish cutover (plan T3.2). `SnapshotWire::build` could not be reused for
/// this: it re-seals every secret with `kp.seal(..)` on each build, which is correct for a
/// version-labelled wire and fatal for a content-addressed tree.
///
/// A struct rather than positional arguments: the fields are several `Vec`s of different element
/// types, and swapping two of them would compile.
#[derive(Clone, Debug, Default)]
pub struct SealedMaterial {
    /// `provider_id` → that provider's api-keys sealed, with their row identity.
    ///
    /// Keyed by provider because the entity path is per provider; the value is sealed because an
    /// api-key is a secret and the snapshot wire seals it too.
    pub provider_keys: std::collections::BTreeMap<String, Sealed>,
    /// Provider api-keys with their row identity, for the fidelity entity.
    pub fidelity_provider_keys: Vec<SealedProviderKey>,
    /// `tenant_id` → the stored (stable) sealed token hash.
    pub tenant_token_hashes: Vec<(String, Sealed)>,
    /// `domain` → the sealed certificate PRIVATE KEY, for the cert's own entity.
    ///
    /// A domain with no key content is simply absent — the entity then carries
    /// `sealed_key: None`, which is the truthful description of "no private key here".
    /// A `cert_key_pem` that IS present but has no entry here is an ERROR, not an
    /// omission: the tree would otherwise drop a key the leader holds.
    pub cert_keys: std::collections::BTreeMap<String, Sealed>,
}

/// Build the CONFIG entities (no fidelity). Private: sealing is not optional, so there is no
/// public way to produce a tree whose provider keys travel in the clear.
///
/// # Errors
/// [`EntityCodecError`] when an id cannot be keyed, an entity cannot be encoded, or a secret
/// cannot be sealed.
fn config_entities(
    cfg: &ConfigData,
    sealed_provider_keys: &std::collections::BTreeMap<String, Sealed>,
    sealed_cert_keys: &std::collections::BTreeMap<String, Sealed>,
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
    for id in cfg.provider_keys.keys() {
        // The VALUE comes from the sealed material, not from the plaintext rows: SEALED, not plain JSON, a provider api-key is a secret and the snapshot wire seals it
        // (`sealed_provider_keys`). Putting the same value into the tree in the clear would
        // quietly undo that — the sealing is not decoration, and the test that guards it is
        // `the_fidelity_entity_carries_no_plaintext_secrets`.
        let path = EntityPath::ProviderKey(id.clone());
        check(&path)?;
        let sealed = sealed_provider_keys
            .get(id)
            .ok_or_else(|| EntityCodecError::Unsupported {
                reason: format!(
                    "no sealed material was supplied for the provider keys of {id}; sealing during \
                     encoding would make the tree name non-reproducible"
                ),
            })?;
        out.push(encode(&path, sealed)?);
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
        out.push(encode(&path, role)?);
    }
    for binding in &cfg.key_prefix_bindings {
        let path = EntityPath::KeyBinding(binding.id.clone());
        check(&path)?;
        out.push(encode(&path, binding)?);
    }
    for sub in &cfg.sub_tenants {
        let path = EntityPath::SubTenant(sub.id.clone());
        check(&path)?;
        out.push(encode(&path, sub)?);
    }
    for route in &cfg.sub_tenant_routes {
        let path = EntityPath::SubTenantRoute(route.id.clone());
        check(&path)?;
        out.push(encode(&path, route)?);
    }
    for (domain, cert) in &cfg.certs {
        let path = EntityPath::Cert(domain.clone());
        check(&path)?;
        // The private key is `skip_serializing` on `CertMeta`, so it MUST come from the sealed
        // material. A key that is present in memory but absent here is refused rather than
        // dropped: `restore_config` writes NULL over the replica's `cert_key_ciphertext` when the
        // decoded config has no key, so "dropped" means "deleted the node's private key".
        let sealed_key = match (&cert.cert_key_pem, sealed_cert_keys.get(domain)) {
            (None, _) => None,
            (Some(_), Some(sealed)) => Some(sealed.clone()),
            (Some(_), None) => {
                return Err(EntityCodecError::Unsupported {
                    reason: format!(
                        "no sealed material was supplied for the private key of the cert for \
                         {domain}; sealing during encoding would make the tree name \
                         non-reproducible, and omitting it would delete the key on every node \
                         that materializes this tree"
                    ),
                })
            }
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
                let role = decode::<LimitRole>(path, bytes)?;
                cfg.limit_roles.push(role);
            }
            EntityPath::KeyBinding(_) => {
                let binding = decode::<ProviderKeyBinding>(path, bytes)?;
                cfg.key_prefix_bindings.push(binding);
            }
            EntityPath::SubTenant(_) => {
                let sub = decode::<SubTenant>(path, bytes)?;
                cfg.sub_tenants.push(sub);
            }
            EntityPath::SubTenantRoute(_) => {
                let route = decode::<SubTenantRoute>(path, bytes)?;
                cfg.sub_tenant_routes.push(route);
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
                fidelity = entity.rows(kp)?;
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
    // not one invented here. The four keys below mirror `store.rs`'s `ORDER BY`s exactly:
    //
    //   limit_role           ORDER BY created_at, id
    //   provider_key_binding ORDER BY key_prefix
    //   sub_tenant           ORDER BY tenant_id, name
    //   sub_tenant_route     ORDER BY sub_tenant_id, model_key
    //
    // Each SQL key is backed by a UNIQUE constraint (`limit_role.id` is the PK;
    // `provider_key_binding.key_prefix` is UNIQUE; `sub_tenant` is UNIQUE(tenant_id, name);
    // `sub_tenant_route` is UNIQUE(sub_tenant_id, model_key)), so those orders are TOTAL and the
    // trailing `id` below only makes the sort deterministic, never reorders a tie. SQLite sorts
    // NULL first and so does `Option`'s `Ord`, which is what keeps the `model_key` column's
    // nullable default route in the same place on both sides.
    //
    // An earlier version sorted all four by `id`. It looked tidy and was a rule the loader never
    // used, so the replica held a differently-ordered config than the leader. Today that changes
    // no behaviour (all matching limit roles are enforced as independent keys; binding and
    // sub-tenant matching take the LONGEST prefix; route lookup is by unique key), which is
    // exactly why it survived review — but "this tree names this config" is only checkable by
    // EQUALITY, and a silently reordered decode makes every such comparison worthless. Caught by
    // `tests/arachne_order_fidelity.rs`, whose fixture makes the two orders disagree.
    cfg.limit_roles
        .sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
    cfg.key_prefix_bindings
        .sort_by(|a, b| (&a.key_prefix, &a.id).cmp(&(&b.key_prefix, &b.id)));
    cfg.sub_tenants
        .sort_by(|a, b| (&a.tenant_id, &a.name, &a.id).cmp(&(&b.tenant_id, &b.name, &b.id)));
    cfg.sub_tenant_routes.sort_by(|a, b| {
        (&a.sub_tenant_id, &a.model_key, &a.id).cmp(&(&b.sub_tenant_id, &b.model_key, &b.id))
    });

    cfg.reindex_tenants();
    Ok((cfg, fidelity))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hydra_core::config::{CertMeta, ModelProvider};
    use hydra_core::model::{
        Provider, ProviderKey, ProviderModel, Tenant, TenantModel, TenantProvider,
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
                    matching_key: None,
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
            key_prefix_bindings: vec![ProviderKeyBinding {
                id: "b-disabled".into(),
                key_prefix: "sk_disabled_".into(),
                provider_id: "p1".into(),
                enabled: false,
                created_at: "2026-01-03T00:00:00Z".into(),
                updated_at: "2026-01-03T00:00:00Z".into(),
            }],
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
            sub_tenants: vec![SubTenant {
                id: "st-1".into(),
                tenant_id: "t1".into(),
                name: "sub".into(),
                key_prefix: "ACME_".into(),
                enabled: false,
                created_at: String::new(),
                updated_at: String::new(),
            }],
            sub_tenant_routes: vec![SubTenantRoute {
                id: "sr-1".into(),
                sub_tenant_id: "st-1".into(),
                model_key: None,
                provider_id: "p1".into(),
                enabled: false,
                created_at: String::new(),
                updated_at: String::new(),
            }],
        }
    }

    fn kp() -> crate::crypto::StaticKeyProvider {
        crate::crypto::StaticKeyProvider::new([7u8; 32], 1)
    }

    /// The sealed material a publisher supplies — sealed ONCE, then reused.
    ///
    /// This is what makes the tree name stable, and the fixture is deliberately built this way
    /// rather than by sealing inside `split_config`: a fresh AES-GCM seal uses a random nonce, so
    /// sealing per publish would give one unchanged config two different names (measured).
    fn sealed_fixture() -> SealedMaterial {
        use std::sync::OnceLock;
        static SEALED: OnceLock<SealedMaterial> = OnceLock::new();
        SEALED
            .get_or_init(|| {
                SealedMaterial::seal_plaintext(&config(), &fidelity(), &kp()).expect("seal fixture")
            })
            .clone()
    }

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
        let blobs = split_config(&cfg, &rows, sealed_fixture()).expect("split");
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
        let blobs = split_config(&cfg, &rows, sealed_fixture()).expect("split");

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
        let t1 =
            tree_of(&split_config(&cfg, &before, sealed_fixture()).expect("split")).expect("tree");
        let t2 =
            tree_of(&split_config(&cfg, &after, sealed_fixture()).expect("split")).expect("tree");

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

    /// The SAME inputs must name the SAME tree — the property that keeps a publish from looking
    /// like a new tree to every node, and the reason the production publisher passes the STORED
    /// sealed rows rather than sealing afresh.
    ///
    /// Falsification: seal inside `split_config` and this fails, because AES-GCM picks a fresh
    /// random nonce per seal and the sealed bytes are part of the tree's identity.
    #[tokio::test]
    async fn the_same_inputs_name_the_same_tree() {
        let cfg = config();
        let rows = fidelity();
        let sealed = sealed_fixture();

        let a = tree_of(&split_config(&cfg, &rows, sealed.clone()).expect("split")).expect("tree");
        let b = tree_of(&split_config(&cfg, &rows, sealed).expect("split")).expect("tree");

        assert_eq!(
            a, b,
            "identical inputs must produce byte-identical trees, or every publish would rename the \
             tree and re-materialize the whole cluster"
        );
        assert_eq!(
            toc_of(&a).expect("toc").hash(),
            toc_of(&b).expect("toc").hash(),
            "and therefore the same name"
        );
    }

    /// The same property for the SET-valued entities, across two INDEPENDENT reads.
    ///
    /// `cfg.tenant_providers` / `cfg.tenant_models` are `HashMap<String, HashSet<String>>`, and
    /// serde serialises a `HashSet` in ITERATION order. `RandomState` is seeded per instance, so
    /// two `HashSet`s built from the SAME rows (one per loader run, i.e. one per publish) iterate
    /// in different orders and encode to different BYTES — a spurious rename, which is the D-8
    /// failure mode arriving from an entirely different direction: the head advances although
    /// nothing logical changed, and every node re-materializes.
    ///
    /// The sibling test above cannot catch this: it splits ONE config object twice, so both calls
    /// see the very same `HashSet` instances, and its fixture sets hold one element each, where
    /// order is not a question. This test builds the config twice and uses sets large enough for
    /// order to matter.
    ///
    /// Falsification: encode the `HashSet` directly (as the first version did) and this fails.
    #[tokio::test]
    async fn two_independent_builds_of_the_same_config_name_the_same_tree() {
        // Two INDEPENDENT constructions, the way two loader runs would produce them.
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
        let first = tree_of(&split_config(&build(), &rows, sealed_fixture()).expect("split"))
            .expect("tree");
        let second = tree_of(&split_config(&build(), &rows, sealed_fixture()).expect("split"))
            .expect("tree");

        assert_eq!(
            first, second,
            "two independent builds of the same logical config must name the same tree; a set \
             serialised in iteration order makes the name depend on the process's hash seed, so \
             every publish renames the tree even when nothing changed"
        );
    }

    /// A cert private key with no sealed material is REFUSED, not dropped.
    ///
    /// The distinction matters because the failure is silent on the other side: a tree that
    /// omitted the key decodes to a config with `cert_key_pem == None`, and `restore_config`
    /// then writes NULL into `cert_key_ciphertext` — deleting the private key of every node that
    /// materializes it. So the encoder must not have a "just leave it out" path.
    ///
    /// Falsification: drop the `(Some(_), None)` arm and this returns `Ok`.
    #[tokio::test]
    async fn a_cert_key_without_sealed_material_is_refused() {
        let cfg = config();
        let rows = fidelity();
        // Same fixture, but with the cert keys stripped: everything else is intact, so the
        // refusal can only come from the cert branch.
        let mut sealed = sealed_fixture();
        sealed.cert_keys.clear();

        let got = split_config(&cfg, &rows, sealed);
        match got {
            Err(EntityCodecError::Unsupported { reason }) => {
                assert!(
                    reason.contains("acme.example"),
                    "the refusal must name the cert it is about, or an operator cannot tell which \
                     tenant to look at: {reason}"
                );
            }
            Err(other) => panic!("expected a refusal naming the missing cert key, got {other:?}"),
            Ok(_) => panic!(
                "a cert private key present in memory but absent from the sealed material must be \
                 refused; encoding it anyway silently deletes the key on every replica"
            ),
        }
    }
}
