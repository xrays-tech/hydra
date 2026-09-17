//! # Config snapshot wire format (cluster P1 control plane).
//!
//! The leader distributes its in-memory [`ConfigData`] snapshot to edge /
//! standby nodes over the internal control channel. Secret material never
//! travels as plaintext:
//!
//! - `provider_keys` (plaintext in the leader's memory) is shipped as
//!   [`SealedDto`] blobs — AES-256-GCM under the fleet-wide
//!   `HYDRA_ENCRYPTION_KEY`, freshly re-sealed from the in-memory plaintext at
//!   build time (no DB reads on the control path, no plaintext on the wire);
//! - certificate **private keys** travel the same way
//!   ([`SealedCertDto`]); the public cert PEM is public and stays readable;
//! - `ConfigData` itself is serialized with the secret fields stripped
//!   (`provider_keys` emptied, `cert_key_pem` removed).
//!
//! The receiver ([`SnapshotWire::hydrate`]) decrypts locally with its own
//! `HYDRA_ENCRYPTION_KEY` (fail-closed: any decryption failure rejects the
//! whole snapshot and the node keeps its previous last-known-good config).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use hydra_core::config::{CertMeta, ConfigData};
use hydra_core::model::{
    LimitRole, ProviderKey, ProviderKeyBinding, ProviderModel, TenantModel, TenantProvider,
};

use crate::cluster::content::{FidelityRows, ReplicationContent};
use crate::crypto::{KeyProvider, Sealed};

/// Snapshot wire format version.
///
/// Fail-closed in BOTH directions, deliberately:
/// - a NEW reader rejects an OLD wire (missing `wire_version` / `fidelity`);
/// - an OLD reader rejects a NEW wire: the three row-sets it REQUIRES
///   (`provider_models` / `tenant_providers` / `tenant_models`) moved INSIDE
///   [`FidelityWireRows`], so it fails with `missing field provider_models`
///   **regardless of payload**. Relying only on the `sealed_provider_keys`
///   value-type change was not enough: an old reader ignores unknown fields, and
///   when the leader had no provider keys (`{}`) it would parse the new wire
///   happily and then execute the wipe this version exists to prevent.
///
/// There is deliberately NO emit switch: governing only emission would leave
/// new nodes unable to materialize the old shape (it carries no fidelity rows),
/// and governing acceptance too would let a new replica rebuild from v1 — i.e.
/// perform the very wipe fail-closed exists to stop. Upgrade/rollback are an
/// ORDER plus an accepted stall window (see `dev-docs/ops.md`).
pub const WIRE_VERSION: u32 = 2;

/// One sealed provider key WITH its row identity, so the replica keeps the
/// leader's primary key instead of minting a new one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedProviderKeyDto {
    pub id: String,
    pub created_at: String,
    pub sealed: SealedDto,
}

/// `tenant_id` → SEALED access-token hash (never the token, and sealed like every
/// other secret on this channel: the control plane is plaintext HTTP).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantTokenHashDto {
    pub tenant_id: String,
    pub hash: SealedDto,
}

/// One sealed secret on the wire: AES-256-GCM ciphertext + nonce + key
/// version (mirrors [`Sealed`], but serde-ready with a `Vec<u8>` nonce).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedDto {
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
    pub key_version: u32,
}

/// Certificate on the wire: the public PEM stays plaintext (it is public
/// material); the private key PEM is sealed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedCertDto {
    pub domain: String,
    /// Public cert PEM (`None` = tenant without a cert).
    pub cert_pem: Option<String>,
    /// Sealed private key PEM.
    pub key: Option<SealedDto>,
}

/// The fidelity rows ON THE WIRE.
///
/// This is a WIRE DTO, deliberately distinct from [`FidelityRows`]: it carries
/// no `provider_keys` (a [`ProviderKey`] cannot round-trip — its `api_key` is
/// `#[serde(skip_serializing)]`; the sealed keys WITH identity travel via
/// [`SnapshotWire::sealed_provider_keys`]), and its token hashes are sealed.
///
/// **The nesting is the mechanism**: `provider_models` / `tenant_providers` /
/// `tenant_models` were top-level fields that every pre-v2 reader requires.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FidelityWireRows {
    /// FULL `limit_role` rows, disabled ones included.
    pub limit_roles: Vec<LimitRole>,
    /// FULL `provider_key_binding` rows, disabled ones included.
    pub key_prefix_bindings: Vec<ProviderKeyBinding>,
    /// `tenant_id` → sealed access-token hash.
    pub tenant_token_hashes: Vec<TenantTokenHashDto>,
    /// Full `provider_model` rows — INCLUDING offline models (`status != 1`),
    /// which the derived `cfg.models_by_key` drops.
    pub provider_models: Vec<ProviderModel>,
    /// Full `tenant_provider` rows (join ids preserved).
    pub tenant_providers: Vec<TenantProvider>,
    /// Full `tenant_model` rows (join ids preserved).
    pub tenant_models: Vec<TenantModel>,
}

/// The control-channel snapshot: version + config (secrets stripped) + the
/// sealed secret material to rehydrate it + the fidelity rows needed to
/// rebuild a byte-faithful local DB (standby replica, P2).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotWire {
    /// Must equal [`WIRE_VERSION`]. NEVER add `#[serde(default)]` here: that
    /// would re-open the "new reader accepts an old wire" direction.
    pub wire_version: u32,
    /// Monotonic config version on the leader (the `since` watermark).
    pub version: u64,
    /// Config with `provider_keys` emptied and `cert_key_pem` stripped.
    pub cfg: ConfigData,
    /// Sealed provider api-keys WITH row identity: `provider_id` → rows.
    pub sealed_provider_keys: HashMap<String, Vec<SealedProviderKeyDto>>,
    /// Sealed cert private keys, keyed by (lowercased) domain.
    pub sealed_certs: HashMap<String, SealedCertDto>,
    /// The rebuild source — nested on purpose (see [`FidelityWireRows`]).
    pub fidelity: FidelityWireRows,
}

/// What a replica gets after unsealing: the runtime config plus the fidelity
/// rows, so "unsealing" and "what to rebuild" are one decision.
pub struct HydratedWire {
    pub version: u64,
    pub cfg: ConfigData,
    pub fidelity: FidelityRows,
}

/// Errors building or hydrating a [`SnapshotWire`].
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("crypto: {0}")]
    Crypto(#[from] crate::crypto::CryptoError),
    #[error("database: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("decrypted secret is not valid UTF-8")]
    NotUtf8,
    #[error("malformed sealed payload (bad nonce length)")]
    MalformedNonce,
    #[error(
        "snapshot wire version {found} is not supported (this binary speaks {expected}); \
         upgrade every node before the leader emits the new format"
    )]
    WireVersion { found: u32, expected: u32 },
}

impl From<&Sealed> for SealedDto {
    fn from(s: &Sealed) -> Self {
        Self {
            ciphertext: s.ciphertext.clone(),
            nonce: s.nonce.to_vec(),
            key_version: s.key_version,
        }
    }
}

impl TryFrom<SealedDto> for Sealed {
    type Error = SnapshotError;

    fn try_from(dto: SealedDto) -> Result<Self, Self::Error> {
        let nonce: [u8; crate::crypto::NONCE_LEN] = dto
            .nonce
            .as_slice()
            .try_into()
            .map_err(|_| SnapshotError::MalformedNonce)?;
        Ok(Self {
            ciphertext: dto.ciphertext,
            nonce,
            key_version: dto.key_version,
        })
    }
}

impl SnapshotWire {
    /// Leader side: encode the replication content for the control channel.
    ///
    /// Takes the content WHOLE — the version comes from `content.version` (not
    /// from a separate argument), so "what we serve" and "what we versioned"
    /// cannot drift. Emits [`WIRE_VERSION`]; there is no emit knob.
    ///
    /// Secrets are freshly re-sealed from the in-memory plaintext rather than
    /// read back from the DB, so the wire is consistent with the snapshot being
    /// served.
    pub async fn build(
        content: &ReplicationContent,
        kp: &dyn KeyProvider,
    ) -> Result<Self, SnapshotError> {
        let f = content.fidelity();

        // Sealed provider keys WITH row identity.
        let mut sealed_provider_keys: HashMap<String, Vec<SealedProviderKeyDto>> = HashMap::new();
        for k in &f.provider_keys {
            sealed_provider_keys
                .entry(k.provider_id.clone())
                .or_default()
                .push(SealedProviderKeyDto {
                    id: k.id.clone(),
                    created_at: k.created_at.clone(),
                    sealed: SealedDto::from(&kp.seal(k.api_key.as_bytes())?),
                });
        }

        // Sealed cert private keys (public PEM stays readable).
        let mut sealed_certs: HashMap<String, SealedCertDto> = HashMap::new();
        for (domain, meta) in &content.cfg.certs {
            let key = match &meta.cert_key_pem {
                Some(pem) => Some(SealedDto::from(&kp.seal(pem.as_bytes())?)),
                None => None,
            };
            sealed_certs.insert(
                domain.clone(),
                SealedCertDto {
                    domain: domain.clone(),
                    cert_pem: meta.cert_pem.clone(),
                    key,
                },
            );
        }

        // Sealed access-token hashes.
        let mut tenant_token_hashes = Vec::with_capacity(f.tenant_token_hashes.len());
        for (tenant_id, hash) in &f.tenant_token_hashes {
            tenant_token_hashes.push(TenantTokenHashDto {
                tenant_id: tenant_id.clone(),
                hash: SealedDto::from(&kp.seal(hash.as_bytes())?),
            });
        }

        // Strip secrets from the serialized config copy.
        let mut cfg = (*content.cfg).clone();
        cfg.provider_keys = HashMap::new();
        for meta in cfg.certs.values_mut() {
            meta.cert_key_pem = None;
        }

        Ok(Self {
            wire_version: WIRE_VERSION,
            version: content.version,
            cfg,
            sealed_provider_keys,
            sealed_certs,
            fidelity: FidelityWireRows {
                limit_roles: f.limit_roles.clone(),
                key_prefix_bindings: f.key_prefix_bindings.clone(),
                tenant_token_hashes,
                provider_models: f.provider_models.clone(),
                tenant_providers: f.tenant_providers.clone(),
                tenant_models: f.tenant_models.clone(),
            },
        })
    }

    /// Receiver side (edge / standby): verify the format version, decrypt the
    /// sealed material with the local master key and rebuild the replication
    /// content. **Fail-closed**: a version mismatch or a single decryption
    /// failure (wrong master key, tampered payload) rejects the whole snapshot —
    /// the caller keeps its previous last-known-good config.
    pub fn hydrate(self, kp: &dyn KeyProvider) -> Result<HydratedWire, SnapshotError> {
        if self.wire_version != WIRE_VERSION {
            return Err(SnapshotError::WireVersion {
                found: self.wire_version,
                expected: WIRE_VERSION,
            });
        }

        let mut cfg = self.cfg;
        // 派生索引不上线缆（`ConfigData.tenants_by_id` 是 `serde(skip)`），
        // 因此在**交付之前**用与 loader 相同的实现从 tenants_by_domain 重建。
        // 位置要紧：这个 cfg 随后被交给 `apply_snapshot` →
        // `ReplicationContent::from_hydrated`，重建必须发生在交付之前。
        cfg.reindex_tenants();

        // Provider keys: unseal, keep the wire's identity, and re-project
        // `cfg.provider_keys` so the hot path sees the same keys.
        let mut provider_keys: Vec<ProviderKey> = Vec::new();
        for (provider_id, rows) in self.sealed_provider_keys {
            for row in rows {
                let plaintext = kp.open(&Sealed::try_from(row.sealed)?)?;
                let api_key = String::from_utf8(plaintext).map_err(|_| SnapshotError::NotUtf8)?;
                cfg.provider_keys
                    .entry(provider_id.clone())
                    .or_default()
                    .push(api_key.clone());
                provider_keys.push(ProviderKey {
                    id: row.id,
                    provider_id: provider_id.clone(),
                    api_key,
                    created_at: row.created_at,
                });
            }
        }

        for (domain, sc) in self.sealed_certs {
            let cert_key_pem = match sc.key {
                Some(s) => {
                    let plaintext = kp.open(&Sealed::try_from(s)?)?;
                    Some(String::from_utf8(plaintext).map_err(|_| SnapshotError::NotUtf8)?)
                }
                None => None,
            };
            cfg.certs.insert(
                domain.clone(),
                CertMeta {
                    domain: domain.clone(),
                    cert_file: None,
                    cert_key: None,
                    cert_pem: sc.cert_pem,
                    cert_key_pem,
                },
            );
        }

        let mut tenant_token_hashes = Vec::with_capacity(self.fidelity.tenant_token_hashes.len());
        for dto in self.fidelity.tenant_token_hashes {
            let plaintext = kp.open(&Sealed::try_from(dto.hash)?)?;
            let hash = String::from_utf8(plaintext).map_err(|_| SnapshotError::NotUtf8)?;
            tenant_token_hashes.push((dto.tenant_id, hash));
        }

        Ok(HydratedWire {
            version: self.version,
            cfg,
            fidelity: FidelityRows {
                limit_roles: self.fidelity.limit_roles,
                key_prefix_bindings: self.fidelity.key_prefix_bindings,
                provider_keys,
                tenant_token_hashes,
                provider_models: self.fidelity.provider_models,
                tenant_providers: self.fidelity.tenant_providers,
                tenant_models: self.fidelity.tenant_models,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hydra_core::model::{Provider, Tenant};
    use serde_json;

    fn kp() -> crate::crypto::StaticKeyProvider {
        crate::crypto::StaticKeyProvider::new([7u8; 32], 1)
    }

    fn cfg_with_secrets() -> ConfigData {
        let mut cfg = ConfigData::default();
        cfg.tenants_by_domain.insert(
            "acme.com".into(),
            Tenant {
                id: "t1".into(),
                name: "T".into(),
                domain: "acme.com".into(),
                auth_url: "https://auth.acme.com/v".into(),
                cert_key: None,
                cert_file: None,
                enabled: true,
                created_at: String::new(),
                updated_at: String::new(),
            },
        );
        cfg.providers.insert(
            "p1".into(),
            Provider {
                id: "p1".into(),
                key: "openai".into(),
                name: "O".into(),
                endpoint: "https://api.openai.com".into(),
                weight: 1,
                created_at: String::new(),
                updated_at: String::new(),
                max_concurrency: None,
                max_queue_depth: None,
                queue_wait_timeout_ms: None,
            },
        );
        cfg.provider_keys
            .insert("p1".into(), vec!["sk-plain-1".into(), "sk-plain-2".into()]);
        cfg.certs.insert(
            "acme.com".into(),
            CertMeta {
                domain: "acme.com".into(),
                cert_file: None,
                cert_key: None,
                cert_pem: Some(
                    "-----BEGIN CERTIFICATE-----\nPUB\n-----END CERTIFICATE-----\n".into(),
                ),
                cert_key_pem: Some(
                    "-----BEGIN PRIVATE KEY-----\nSECRET\n-----END PRIVATE KEY-----\n".into(),
                ),
            },
        );
        cfg
    }

    /// A replication content with NO fidelity rows, for tests that only care
    /// about the secret round-trip. Production never constructs one this way:
    /// `ReplicationContent::load` (leader) and `hydrate` (replica) are the only
    /// paths, and both supply real rows.
    fn content(version: u64, cfg: ConfigData) -> ReplicationContent {
        // Mirror the production invariant EXACTLY: `ReplicationContent::load`
        // derives `cfg.provider_keys` FROM the fidelity rows, and `build` seals
        // the keys from those same rows. A content whose cfg carried keys but
        // whose fidelity did not would be a state production cannot produce.
        let mut provider_keys = Vec::new();
        for (provider_id, keys) in &cfg.provider_keys {
            for (n, api_key) in keys.iter().enumerate() {
                provider_keys.push(ProviderKey {
                    id: format!("{provider_id}-k{n}"),
                    provider_id: provider_id.clone(),
                    api_key: api_key.clone(),
                    created_at: String::new(),
                });
            }
        }
        ReplicationContent::from_hydrated(
            version,
            std::sync::Arc::new(cfg),
            FidelityRows {
                limit_roles: Vec::new(),
                key_prefix_bindings: Vec::new(),
                provider_keys,
                tenant_token_hashes: Vec::new(),
                provider_models: Vec::new(),
                tenant_providers: Vec::new(),
                tenant_models: Vec::new(),
            },
        )
    }

    #[tokio::test]
    async fn build_strips_secrets_and_hydrate_restores() {
        let kp = kp();
        let original = cfg_with_secrets();

        let wire = SnapshotWire::build(&content(7, original.clone()), &kp)
            .await
            .expect("build");
        assert_eq!(wire.wire_version, WIRE_VERSION, "emits the current version");
        assert_eq!(wire.version, 7);

        // The serialized config must not carry plaintext secrets.
        assert!(wire.cfg.provider_keys.is_empty(), "provider_keys stripped");
        for meta in wire.cfg.certs.values() {
            assert!(meta.cert_key_pem.is_none(), "cert keys stripped");
        }
        // Public cert PEM survives.
        assert!(wire.cfg.certs["acme.com"].cert_pem.is_some());
        // The wire carries sealed material.
        assert_eq!(wire.sealed_provider_keys["p1"].len(), 2);
        assert!(wire.sealed_certs["acme.com"].key.is_some());

        // JSON round-trip (this is exactly what crosses the HTTP control
        // channel).
        let json = serde_json::to_vec(&wire).expect("serialize");
        let wire2: SnapshotWire = serde_json::from_slice(&json).expect("deserialize");

        let restored = wire2.hydrate(&kp).expect("hydrate");
        assert_eq!(restored.version, 7);
        assert_eq!(
            restored.cfg.provider_keys["p1"],
            original.provider_keys["p1"]
        );
        assert_eq!(restored.cfg.certs["acme.com"], original.certs["acme.com"]);
        assert_eq!(restored.cfg.tenants_by_domain, original.tenants_by_domain);
        assert_eq!(restored.cfg.providers, original.providers);
        // The wire's fidelity block round-trips (empty here — see `content`).
        assert!(restored.fidelity.limit_roles.is_empty());
    }

    #[tokio::test]
    async fn hydrate_fails_closed_on_wrong_key() {
        let kp = kp();
        let wire = SnapshotWire::build(&content(1, cfg_with_secrets()), &kp)
            .await
            .expect("build");

        let wrong = crate::crypto::StaticKeyProvider::new([9u8; 32], 1);
        assert!(
            wire.hydrate(&wrong).is_err(),
            "wrong master key must reject the snapshot (last-known-good kept)"
        );
    }

    #[tokio::test]
    async fn cert_without_key_roundtrips() {
        let kp = kp();
        let mut cfg = ConfigData::default();
        cfg.certs.insert(
            "plain.com".into(),
            CertMeta {
                domain: "plain.com".into(),
                cert_file: None,
                cert_key: None,
                cert_pem: None,
                cert_key_pem: None,
            },
        );
        let wire = SnapshotWire::build(&content(1, cfg.clone()), &kp)
            .await
            .expect("build");
        assert!(wire.sealed_certs["plain.com"].key.is_none());
        let restored = wire.hydrate(&kp).expect("hydrate");
        assert_eq!(restored.cfg.certs, cfg.certs);
    }
}
