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

use crate::crypto::{KeyProvider, Sealed};

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

/// The control-channel snapshot: version + config (secrets stripped) + the
/// sealed secret material to rehydrate it + the fidelity rows needed to
/// rebuild a byte-faithful local DB (standby replica, P2).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotWire {
    /// Monotonic config version on the leader (the `since` watermark).
    pub version: u64,
    /// Config with `provider_keys` emptied and `cert_key_pem` stripped.
    pub cfg: ConfigData,
    /// Sealed provider api-keys: `provider_id` → sealed blobs (one per key).
    pub sealed_provider_keys: HashMap<String, Vec<SealedDto>>,
    /// Sealed cert private keys, keyed by (lowercased) domain.
    pub sealed_certs: HashMap<String, SealedCertDto>,
    // ── Fidelity rows (P2 replica materialization) ────────────────────────
    /// Full `provider_model` rows — INCLUDING offline models (`status != 1`),
    /// which the derived `cfg.models_by_key` drops. Without these, a promoted
    /// replica would silently lose manually-disabled models.
    pub provider_models: Vec<hydra_core::model::ProviderModel>,
    /// Full `tenant_provider` rows (join ids preserved).
    pub tenant_providers: Vec<hydra_core::model::TenantProvider>,
    /// Full `tenant_model` rows (join ids preserved).
    pub tenant_models: Vec<hydra_core::model::TenantModel>,
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
    /// Leader side: strip the secrets from an in-memory snapshot and seal
    /// them under the fleet-wide master key. Freshly re-sealing the plaintext
    /// (rather than reading DB ciphertext) keeps the wire consistent with the
    /// snapshot being served. The fidelity rows (full `provider_model` /
    /// join rows) are fetched from the DB so a promoted replica rebuilds a
    /// byte-faithful local store.
    pub async fn build(
        version: u64,
        cfg: ConfigData,
        pool: &sqlx::SqlitePool,
        kp: &dyn KeyProvider,
    ) -> Result<Self, SnapshotError> {
        let mut sealed_provider_keys: HashMap<String, Vec<SealedDto>> = HashMap::new();
        for (provider_id, keys) in &cfg.provider_keys {
            let mut sealed = Vec::with_capacity(keys.len());
            for k in keys {
                sealed.push(SealedDto::from(&kp.seal(k.as_bytes())?));
            }
            sealed_provider_keys.insert(provider_id.clone(), sealed);
        }

        let mut sealed_certs: HashMap<String, SealedCertDto> = HashMap::new();
        for (domain, meta) in &cfg.certs {
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

        // Fidelity rows (P2): full provider_model set incl. offline models,
        // and the raw join rows.
        let provider_models = crate::db::list_provider_models(pool).await?;
        let tenant_providers = crate::db::list_tenant_providers(pool).await?;
        let tenant_models = crate::db::list_tenant_models(pool).await?;

        // Strip secrets from the serialized copy.
        let mut cfg = cfg;
        cfg.provider_keys = HashMap::new();
        for meta in cfg.certs.values_mut() {
            meta.cert_key_pem = None;
        }

        Ok(Self {
            version,
            cfg,
            sealed_provider_keys,
            sealed_certs,
            provider_models,
            tenant_providers,
            tenant_models,
        })
    }

    /// Receiver side (edge / standby): decrypt the sealed material with the
    /// local master key and rebuild a full [`ConfigData`]. **Fail-closed**: a
    /// single decryption failure (wrong master key, tampered payload) rejects
    /// the whole snapshot — the caller keeps its previous snapshot.
    pub fn hydrate(self, kp: &dyn KeyProvider) -> Result<ConfigData, SnapshotError> {
        let mut cfg = self.cfg;

        for (provider_id, sealed_keys) in self.sealed_provider_keys {
            let mut keys = Vec::with_capacity(sealed_keys.len());
            for s in sealed_keys {
                let plaintext = kp.open(&Sealed::try_from(s)?)?;
                let key = String::from_utf8(plaintext).map_err(|_| SnapshotError::NotUtf8)?;
                keys.push(key);
            }
            cfg.provider_keys.insert(provider_id, keys);
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

        Ok(cfg)
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

    /// A migrated in-memory pool for `build` (which now reads fidelity rows).
    async fn pool() -> sqlx::SqlitePool {
        let p = crate::db::init_pool("sqlite::memory:")
            .await
            .expect("init_pool");
        crate::db::run_migrate(&p).await.expect("migrate");
        p
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

    #[tokio::test]
    async fn build_strips_secrets_and_hydrate_restores() {
        let kp = kp();
        let original = cfg_with_secrets();
        let p = pool().await;

        let wire = SnapshotWire::build(7, original.clone(), &p, &kp)
            .await
            .expect("build");

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
        assert_eq!(restored.provider_keys["p1"], original.provider_keys["p1"]);
        assert_eq!(restored.certs["acme.com"], original.certs["acme.com"]);
        assert_eq!(restored.tenants_by_domain, original.tenants_by_domain);
        assert_eq!(restored.providers, original.providers);
    }

    #[tokio::test]
    async fn hydrate_fails_closed_on_wrong_key() {
        let kp = kp();
        let p = pool().await;
        let wire = SnapshotWire::build(1, cfg_with_secrets(), &p, &kp)
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
        let p = pool().await;
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
        let wire = SnapshotWire::build(1, cfg.clone(), &p, &kp)
            .await
            .expect("build");
        assert!(wire.sealed_certs["plain.com"].key.is_none());
        let restored = wire.hydrate(&kp).expect("hydrate");
        assert_eq!(restored.certs, cfg.certs);
    }
}
