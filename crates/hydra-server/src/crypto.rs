//! At-rest encryption for persisted secrets (provider upstream api-keys).
//!
//! Boundary module: the DB stores ciphertext; `hydra_core::ProviderKey` holds
//! plaintext only in memory and is never persisted as plaintext. Pure-crypto
//! dependencies (aes-gcm / base64 / rand) live here, never in hydra-core.
//!
//! Algorithm: AES-256-GCM. A fresh 96-bit random nonce is generated per seal.
//! `key_version` is bound as GCM additional authenticated data (AAD), so a
//! ciphertext sealed under one key version fails to open under another.

use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes256Gcm, Nonce,
};
use base64::Engine;
use rand::RngCore;
use std::path::Path;
use thiserror::Error;

/// AES-256 key length, in bytes.
pub const KEY_LEN: usize = 32;
/// GCM nonce length, in bytes (96 bits).
pub const NONCE_LEN: usize = 12;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error(
        "master key must be {expected} bytes, got {got}: HYDRA_ENCRYPTION_KEY holds the BASE64 \
         of those bytes, while HYDRA_ENCRYPTION_KEY_FILE holds the RAW bytes — base64 text in \
         the file (or a raw key in the variable) produces exactly this"
    )]
    KeyLength { expected: usize, got: usize },
    #[error("master key is not configured: set HYDRA_ENCRYPTION_KEY (base64 of 32 bytes) or HYDRA_ENCRYPTION_KEY_FILE")]
    KeyMissing,
    #[error("master key is not valid base64: {0}")]
    KeyEncoding(#[from] base64::DecodeError),
    #[error("could not read master key file {path}: {source}")]
    KeyFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("stored key_version {stored} does not match provider key_version {provider}; re-enter the key or rotate")]
    KeyVersionMismatch { stored: u32, provider: u32 },
    #[error("decryption failed (wrong master key or tampered ciphertext)")]
    Decrypt,
    #[error("decrypted key is not valid UTF-8")]
    NotUtf8,
    #[error("{var} must be a positive integer (a key version), got {value:?}")]
    KeyVersionInvalid { var: String, value: String },
}

/// One encrypted secret as persisted: ciphertext + nonce + key version.
///
/// Serialisable because it now travels inside a config-tree entity
/// (`arachne_entities::FidelityTreeEntity`), not only through the snapshot wire's DTOs. The
/// serialized form carries the same three fields the DTO does, so nothing new is exposed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Sealed {
    pub ciphertext: Vec<u8>,
    pub nonce: [u8; NONCE_LEN],
    pub key_version: u32,
}

/// Marks a TEXT value that carries a [`Sealed`] rather than plaintext.
///
/// ## Why a prefix, when `provider_key` uses dedicated columns
///
/// `provider_key` splits a secret over `api_key_ciphertext` / `api_key_nonce` / `key_version`, so
/// "is this row sealed?" is a property of the SCHEMA. `limit_role.matching_key` (decision D-16,
/// 2026-10-08 — the user's ruling: seal the column, and re-seal legacy rows automatically) has ONE
/// TEXT column that has held PLAINTEXT since the first release. Adding sibling columns would leave
/// two carriers of the same secret and a row in which both are populated; instead the value says
/// what it is, which is what makes the migration a READ-time question ("does this value parse as an
/// envelope?") rather than a schema migration.
///
/// The `v1` tag is the format version: a future layout change is a new tag, never a silent
/// reinterpretation of bytes already in someone's database or config tree.
pub const SEALED_TEXT_PREFIX: &str = "sealed:v1:";

impl Sealed {
    /// This sealed value in the single TEXT form shared by `limit_role.matching_key` and the config
    /// tree: `sealed:v1:<key_version>:<base64(nonce ‖ ciphertext)>`.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut raw = Vec::with_capacity(NONCE_LEN + self.ciphertext.len());
        raw.extend_from_slice(&self.nonce);
        raw.extend_from_slice(&self.ciphertext);
        format!(
            "{SEALED_TEXT_PREFIX}{}:{}",
            self.key_version,
            base64::engine::general_purpose::STANDARD.encode(raw)
        )
    }

    /// Parse [`Self::to_text`]'s form. `None` means "this value is NOT an envelope" — a plaintext
    /// value written before the column was sealed, or something an operator typed.
    ///
    /// STRUCTURAL only: this answers "does this value look like a sealed block?", not "does it
    /// open?". The two questions are deliberately different — see [`Self::open_text`].
    #[must_use]
    pub fn from_text(value: &str) -> Option<Self> {
        let (version, payload) = value.strip_prefix(SEALED_TEXT_PREFIX)?.split_once(':')?;
        let key_version: u32 = version.parse().ok()?;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .ok()?;
        // A nonce AND at least a GCM tag: anything shorter cannot be a ciphertext this code wrote,
        // so it is not an envelope (and must not be reported as a decryption failure).
        if raw.len() <= NONCE_LEN {
            return None;
        }
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&raw[..NONCE_LEN]);
        Some(Sealed {
            ciphertext: raw[NONCE_LEN..].to_vec(),
            nonce,
            key_version,
        })
    }

    /// Open a TEXT value that may be an envelope OR a plaintext value from before D-16.
    ///
    /// ## The fallback is narrow on purpose — and this is exactly what reaches it
    ///
    /// * A value that does NOT parse as an envelope is a pre-D-16 plaintext client key, and it is
    ///   returned as-is. The loader re-seals what it finds (`db::seal_legacy_limit_keys` for the
    ///   column, the encoder for the config tree).
    ///
    ///   MEASURED while falsifying this (round 210), because the first version of this comment
    ///   overstated the case: the config LOADER migrates the column BEFORE it reads it, so the loader
    ///   path alone does NOT depend on this branch (making the branch an error left the loader test
    ///   green). What does depend on it is every other reader: `GET /api/v1/limit-roles` on a node
    ///   whose migration has not run yet, and any other caller of `db::list_limit_roles`. "Not an
    ///   envelope" means "written before D-16", not "corrupt", and inventing an error for it would
    ///   break those paths over a value that is perfectly usable.
    ///
    ///   The config TREE is deliberately NOT one of those carriers: D-16 changed what a `limit_role`
    ///   entity's bytes mean, so it is a `TOC_FORMAT` bump and an older tree is refused by the toc.
    ///   The tree decoder refuses a plaintext value outright
    ///   (`cluster::arachne_entities::opened_limit_role`) — a per-field fallback there would let a
    ///   mis-decoded tree through, which is how a limit silently stops being enforced.
    /// * A value that DOES parse but does not open is an ERROR. A wrong or rotated-away master key
    ///   is never handed back as "the key", and there is no path here that returns ciphertext.
    ///
    /// # Errors
    /// [`CryptoError::Decrypt`] / [`CryptoError::KeyVersionMismatch`] for an envelope the master key
    /// ring cannot open; [`CryptoError::NotUtf8`] when the plaintext is not UTF-8.
    pub fn open_text(kp: &dyn KeyProvider, value: &str) -> Result<String, CryptoError> {
        let Some(sealed) = Self::from_text(value) else {
            return Ok(value.to_string());
        };
        let plaintext = kp.open(&sealed)?;
        String::from_utf8(plaintext).map_err(|_| CryptoError::NotUtf8)
    }
}

/// Abstraction over master-key sources. `StaticKeyProvider` reads the key from
/// the environment; a future `KmsKeyProvider` (AWS KMS / HashiCorp Vault) will
/// implement this same trait without changing call sites.
///
/// TODO(kms): implement `KmsKeyProvider` backed by AWS KMS / HashiCorp Vault.
pub trait KeyProvider: Send + Sync {
    /// Encrypt under the provider's CURRENT key version, with a FRESH RANDOM nonce.
    ///
    /// This is the right choice for anything persisted at rest (a `provider_key` row, a tenant's
    /// cert key): nothing depends on those bytes being reproducible, and a random nonce hides even
    /// the fact that two rows hold the same secret. Use [`Self::seal_deterministic`] only where the
    /// ciphertext is part of a VALUE'S IDENTITY.
    fn seal(&self, plaintext: &[u8]) -> Result<Sealed, CryptoError>;
    /// Encrypt under the provider's CURRENT key version, with a nonce DERIVED from the key and the
    /// plaintext — so the same plaintext always seals to the same bytes.
    ///
    /// ## Why this exists
    ///
    /// The Arachne config tree is content-addressed: its name is the hash of its bytes. A secret
    /// sealed with a random nonce therefore gives ONE UNCHANGED CONFIG a new name on every publish
    /// — the head advances, and every node re-materializes the whole config for nothing. (Measured
    /// while building it: the first version sealed during encoding and named two different trees
    /// for one config.) Deterministic sealing is what makes the tree's name a function of the
    /// LOGICAL config, on any node.
    ///
    /// ## What it costs, stated plainly
    ///
    /// * **Equality is visible.** The same plaintext under the same key version seals to the same
    ///   ciphertext, so a holder of the store can tell that two secrets are equal. For this use
    ///   case the tree is content-addressed anyway — the toc already publishes a content hash per
    ///   entity, so a reader can already tell that two TENANTS are identical; this extends that to
    ///   the secrets inside them.
    /// * **Not SIV, and not misuse-resistant.** This derives a nonce, it does not authenticate the
    ///   message with a synthetic IV. What it does guarantee is the property that matters for
    ///   GCM's catastrophic failure mode: two DIFFERENT plaintexts cannot share a nonce (the nonce
    ///   is a keyed function of the plaintext, so that would need an HMAC collision), and the key
    ///   version is part of the nonce input, so a rotation changes every nonce.
    /// * It is only as strong as the master key, like everything else here.
    ///
    /// A plaintext-derived nonce must be KEYED — an unkeyed hash would let anyone who guesses a
    /// plaintext compute the nonce — which is why this is HMAC-SHA256
    /// ([`hydra_core::auth::hmac_sha256`], RFC-4231-vector-checked) and not a bare digest.
    fn seal_deterministic(&self, plaintext: &[u8]) -> Result<Sealed, CryptoError>;
    /// Decrypt a stored secret. Fail-closed: a version outside the provider's
    /// ring is an error, never a guess.
    fn open(&self, sealed: &Sealed) -> Result<Vec<u8>, CryptoError>;
    /// The version `seal` writes — the target of a re-seal
    /// ([`crate::db::reseal_secrets`]).
    fn version(&self) -> u32;
}

/// Domain separator for [`KeyProvider::seal_deterministic`]'s nonce.
///
/// A literal tag so this derivation can never collide with any other use of the same master key,
/// and a version number inside it so a future change to the derivation is a new tag rather than a
/// silent reinterpretation of existing ciphertext.
const NONCE_DOMAIN: &[u8] = b"hydra/config-tree/v1";

/// The deterministic nonce: `HMAC-SHA256(key, domain ‖ key_version ‖ plaintext)[..12]`.
///
/// Twelve bytes because that is AES-GCM's nonce length; truncating a MAC is safe here because the
/// nonce needs to be UNIQUE per (key, plaintext) and unpredictable without the key, not
/// collision-resistant at 256 bits (a truncated-MAC collision would need 2^48 work, and its only
/// consequence is the equality leak this design already accepts).
fn deterministic_nonce(key: &[u8], version: u32, plaintext: &[u8]) -> [u8; NONCE_LEN] {
    let mut message = Vec::with_capacity(NONCE_DOMAIN.len() + 4 + plaintext.len());
    message.extend_from_slice(NONCE_DOMAIN);
    message.extend_from_slice(&version.to_le_bytes());
    message.extend_from_slice(plaintext);
    let mac = hydra_core::auth::hmac_sha256(key, &message);
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(&mac[..NONCE_LEN]);
    nonce
}

/// Master key from `HYDRA_ENCRYPTION_KEY` (base64, 32 bytes) or
/// `HYDRA_ENCRYPTION_KEY_FILE` (raw 32-byte file). AES-256-GCM, fresh nonce
/// per seal, `key_version` bound as AAD.
///
/// Holds a small **key ring** (version → key): `seal` always uses the CURRENT
/// version, while `open` accepts any version in the ring. That is what makes the
/// version/AAD machinery real — it used to be dead code, because the version was
/// hard-coded to 1 at the only construction site, so "rotate the master key" had
/// no path at all: every stored ciphertext became unopenable, `ConfigStore::load`
/// failed, and the process refused to start with no way back in (the admin API
/// that could re-enter keys needs the same process).
///
/// The previous version stays in the ring only while
/// `HYDRA_ENCRYPTION_KEY_PREVIOUS` is set: that is the rotation window, during
/// which `hydra --reseal` (see `db::reseal_secrets`) rewrites every row under the
/// current version, after which the previous key can be dropped again.
pub struct StaticKeyProvider {
    /// version → key. `BTreeMap` so `current()` (the max) is well-defined.
    keys: std::collections::BTreeMap<u32, [u8; KEY_LEN]>,
    current: u32,
}

impl StaticKeyProvider {
    /// Construct from an already-loaded raw 32-byte key and a version tag.
    pub fn new(key: [u8; KEY_LEN], version: u32) -> Self {
        let mut keys = std::collections::BTreeMap::new();
        keys.insert(version, key);
        Self {
            keys,
            current: version,
        }
    }

    /// A ring of `current` (+ its version) and an older `previous` key, so
    /// existing ciphertext still opens while a re-seal is pending.
    #[must_use]
    pub fn with_previous(
        current_key: [u8; KEY_LEN],
        current_version: u32,
        previous_key: [u8; KEY_LEN],
        previous_version: u32,
    ) -> Self {
        let mut keys = std::collections::BTreeMap::new();
        keys.insert(previous_version, previous_key);
        keys.insert(current_version, current_key);
        Self {
            keys,
            current: current_version,
        }
    }

    /// Current master-key version tag (the one `seal` writes).
    pub fn version(&self) -> u32 {
        self.current
    }

    /// Whether `version` is in the ring (`open` can attempt it).
    #[must_use]
    pub fn has_version(&self, version: u32) -> bool {
        self.keys.contains_key(&version)
    }

    /// Load from the environment, preferring the file form. Returns
    /// `CryptoError::KeyMissing` (fail-closed) when neither var is set.
    pub fn from_env() -> Result<Self, CryptoError> {
        let raw = if let Some(p) = std::env::var_os("HYDRA_ENCRYPTION_KEY_FILE") {
            let path = Path::new(&p);
            let bytes = std::fs::read(path).map_err(|e| CryptoError::KeyFile {
                path: path.display().to_string(),
                source: e,
            })?;
            trim_key_bytes(bytes)
        } else if let Some(b64) = std::env::var_os("HYDRA_ENCRYPTION_KEY") {
            let s = b64.to_string_lossy().into_owned();
            base64::engine::general_purpose::STANDARD
                .decode(s.trim())
                .map_err(CryptoError::KeyEncoding)?
        } else {
            return Err(CryptoError::KeyMissing);
        };
        if raw.len() != KEY_LEN {
            return Err(CryptoError::KeyLength {
                expected: KEY_LEN,
                got: raw.len(),
            });
        }
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(&raw);
        // `HYDRA_ENCRYPTION_KEY_VERSION` (default 1) makes the version tag
        // configurable, so a rotation can actually DECLARE the new version — it
        // used to be hard-coded here, which is why `key_version` was always 1 and
        // the versioned-AAD story was dead code.
        let version = version_from_env("HYDRA_ENCRYPTION_KEY_VERSION", 1)?;
        // Optional PREVIOUS key: the rotation window, during which stored rows
        // still open before `db::reseal_secrets` rewrites them.
        let previous = match std::env::var_os("HYDRA_ENCRYPTION_KEY_PREVIOUS") {
            Some(b64) => {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(b64.to_string_lossy().trim())
                    .map_err(CryptoError::KeyEncoding)?;
                if decoded.len() != KEY_LEN {
                    return Err(CryptoError::KeyLength {
                        expected: KEY_LEN,
                        got: decoded.len(),
                    });
                }
                let mut prev = [0u8; KEY_LEN];
                prev.copy_from_slice(&decoded);
                let prev_version = version_from_env(
                    "HYDRA_ENCRYPTION_KEY_PREVIOUS_VERSION",
                    version.saturating_sub(1),
                )?;
                Some((prev, prev_version))
            }
            None => None,
        };
        Ok(match previous {
            Some((prev, prev_version)) => Self::with_previous(key, version, prev, prev_version),
            None => Self::new(key, version),
        })
    }
}

/// Parse a master-key version from an env var, refusing `0` and garbage.
///
/// `0` is rejected rather than defaulted: it is the value a `parse().ok()`-style
/// fallback would silently produce for a typo, and a version of 0 would then
/// re-seal (or look for) rows that no deployment ever wrote.
fn version_from_env(var: &str, default: u32) -> Result<u32, CryptoError> {
    match std::env::var(var) {
        Err(_) => Ok(default),
        Ok(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(default);
            }
            match trimmed.parse::<u32>() {
                Ok(v) if v > 0 => Ok(v),
                _ => Err(CryptoError::KeyVersionInvalid {
                    var: var.to_string(),
                    value: raw,
                }),
            }
        }
    }
}

impl KeyProvider for StaticKeyProvider {
    fn version(&self) -> u32 {
        self.current
    }

    fn seal(&self, plaintext: &[u8]) -> Result<Sealed, CryptoError> {
        let key = self
            .keys
            .get(&self.current)
            .ok_or(CryptoError::KeyVersionMismatch {
                stored: self.current,
                provider: self.current,
            })?;
        let cipher = Aes256Gcm::new(key.into());
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ct = cipher
            .encrypt(
                nonce,
                Payload {
                    msg: plaintext,
                    aad: &self.current.to_le_bytes(),
                },
            )
            .map_err(|_| CryptoError::Decrypt)?;
        Ok(Sealed {
            ciphertext: ct,
            nonce: nonce_bytes,
            key_version: self.current,
        })
    }

    fn seal_deterministic(&self, plaintext: &[u8]) -> Result<Sealed, CryptoError> {
        let key = self
            .keys
            .get(&self.current)
            .ok_or(CryptoError::KeyVersionMismatch {
                stored: self.current,
                provider: self.current,
            })?;
        let cipher = Aes256Gcm::new(key.into());
        // Same AAD as `seal`: `key_version` is authenticated, so a ciphertext moved to another
        // version fails to open rather than being silently accepted.
        let nonce_bytes = deterministic_nonce(key.as_slice(), self.current, plaintext);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ct = cipher
            .encrypt(
                nonce,
                Payload {
                    msg: plaintext,
                    aad: &self.current.to_le_bytes(),
                },
            )
            .map_err(|_| CryptoError::Decrypt)?;
        Ok(Sealed {
            ciphertext: ct,
            nonce: nonce_bytes,
            key_version: self.current,
        })
    }

    fn open(&self, sealed: &Sealed) -> Result<Vec<u8>, CryptoError> {
        // `key_version` is also bound as AAD below, so a version mismatch would
        // fail the GCM tag check. We surface a clearer error up-front.
        // The ring may hold several versions during a rotation window; a version
        // OUTSIDE it is fail-closed, exactly as before.
        let Some(key) = self.keys.get(&sealed.key_version) else {
            return Err(CryptoError::KeyVersionMismatch {
                stored: sealed.key_version,
                provider: self.current,
            });
        };
        let cipher = Aes256Gcm::new(key.into());
        let nonce = Nonce::from_slice(&sealed.nonce);
        cipher
            .decrypt(
                nonce,
                Payload {
                    msg: &sealed.ciphertext,
                    aad: &sealed.key_version.to_le_bytes(),
                },
            )
            .map_err(|_| CryptoError::Decrypt)
    }
}

/// Trim a single trailing newline (and optional CR) so `echo -n` vs `echo`
/// both produce the same raw key bytes.
fn trim_key_bytes(mut b: Vec<u8>) -> Vec<u8> {
    if matches!(b.last(), Some(b'\n')) {
        b.pop();
    }
    if matches!(b.last(), Some(b'\r')) {
        b.pop();
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kp() -> StaticKeyProvider {
        StaticKeyProvider::new([7u8; KEY_LEN], 1)
    }

    /// The wrong-length error must EXPLAIN the two forms.
    ///
    /// Measured 2026-09-30 (`integration/test_master_key_sources.py` S8): pointing
    /// `HYDRA_ENCRYPTION_KEY_FILE` at a file holding BASE64 (the classic mix-up — the inline
    /// variable wants base64, the file wants raw bytes) produced only
    /// "master key must be 32 bytes, got 44", which does not tell an operator what to change.
    #[test]
    fn the_length_error_names_both_forms() {
        let msg = CryptoError::KeyLength {
            expected: KEY_LEN,
            got: 44,
        }
        .to_string();
        assert!(msg.contains("44"), "{msg}");
        assert!(
            msg.contains("HYDRA_ENCRYPTION_KEY") && msg.contains("HYDRA_ENCRYPTION_KEY_FILE"),
            "the message must name both variables: {msg}"
        );
        assert!(
            msg.contains("BASE64") && msg.contains("RAW"),
            "and say which form is which: {msg}"
        );
    }

    #[test]
    fn round_trip() {
        let kp = kp();
        let sealed = kp.seal(b"sk-provider-secret-123").unwrap();
        assert_eq!(kp.open(&sealed).unwrap(), b"sk-provider-secret-123");
    }

    /// `HYDRA_ENCRYPTION_KEY_VERSION` decides which key decrypts every stored
    /// secret, and `version_from_env` — the whole parse — was referenced by no
    /// test anywhere in the crate. A wrong answer here IS the rotation trap: a
    /// value that silently falls back to the default leaves a new key labelled
    /// with the old version, so `reseal_secrets` reports "already current" and the
    /// operator deletes the only key that can open the data (see `db.rs`).
    ///
    /// So it must be total (never panic) and fail CLOSED (garbage is an error, not
    /// a default). Each case uses its own variable name, so these cannot race with
    /// each other or with any other test in this binary.
    #[test]
    fn version_from_env_is_total_and_fail_closed() {
        let unset = "HYDRA_TEST_KEY_VERSION_UNSET";
        std::env::remove_var(unset);
        assert_eq!(
            version_from_env(unset, 3).expect("an unset var takes the default"),
            3
        );

        let cases: [(&str, &str, Option<u32>); 6] = [
            // Blank/whitespace is "not configured", not garbage: an empty value in
            // a compose/k8s manifest should behave like an absent one.
            ("HYDRA_TEST_KEY_VERSION_EMPTY", "", Some(3)),
            ("HYDRA_TEST_KEY_VERSION_BLANK", "   ", Some(3)),
            ("HYDRA_TEST_KEY_VERSION_OK", "2", Some(2)),
            ("HYDRA_TEST_KEY_VERSION_PADDED", " 7 ", Some(7)),
            // 0 and garbage are ERRORS. 0 would be a version no key can carry
            // (`KeyVersionInvalid` is refused at startup, deliberately).
            ("HYDRA_TEST_KEY_VERSION_ZERO", "0", None),
            ("HYDRA_TEST_KEY_VERSION_JUNK", "abc", None),
        ];
        for (var, raw, expected) in cases {
            std::env::set_var(var, raw);
            match expected {
                Some(v) => assert_eq!(
                    version_from_env(var, 3)
                        .unwrap_or_else(|e| panic!("{var}={raw:?} must parse, got {e}")),
                    v,
                    "{var}={raw:?}"
                ),
                None => assert!(
                    version_from_env(var, 3).is_err(),
                    "{var}={raw:?} must be REFUSED (fail closed), not defaulted"
                ),
            }
            std::env::remove_var(var);
        }

        // A negative number is not a version either.
        let neg = "HYDRA_TEST_KEY_VERSION_NEG";
        std::env::set_var(neg, "-1");
        assert!(version_from_env(neg, 1).is_err(), "-1 must be refused");
        std::env::remove_var(neg);
    }

    #[test]
    fn nonce_is_unique_per_seal() {
        let kp = kp();
        let a = kp.seal(b"same").unwrap();
        let b = kp.seal(b"same").unwrap();
        assert_ne!(a.nonce, b.nonce);
        assert_ne!(a.ciphertext, b.ciphertext);
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let kp = kp();
        let mut sealed = kp.seal(b"secret").unwrap();
        sealed.ciphertext[0] ^= 0xff;
        assert!(matches!(kp.open(&sealed), Err(CryptoError::Decrypt)));
    }

    #[test]
    fn wrong_master_key_is_rejected() {
        let seal_kp = StaticKeyProvider::new([1u8; KEY_LEN], 1);
        let open_kp = StaticKeyProvider::new([2u8; KEY_LEN], 1);
        let sealed = seal_kp.seal(b"secret").unwrap();
        // version matches (both 1) but key differs -> tag failure
        assert!(matches!(open_kp.open(&sealed), Err(CryptoError::Decrypt)));
    }

    #[test]
    fn version_mismatch_is_rejected() {
        let seal_kp = StaticKeyProvider::new([1u8; KEY_LEN], 1);
        let open_kp = StaticKeyProvider::new([1u8; KEY_LEN], 2);
        let sealed = seal_kp.seal(b"secret").unwrap();
        assert!(matches!(
            open_kp.open(&sealed),
            Err(CryptoError::KeyVersionMismatch { .. })
        ));
    }

    /// `seal` must stay RANDOM, `seal_deterministic` must not be.
    ///
    /// These two are easy to "unify" — one of them looks redundant next to the other — and the
    /// consequence differs by direction:
    ///
    /// * making `seal` deterministic would make at-rest ciphertexts comparable across rows and
    ///   across backups, for no benefit (nothing hashes those bytes);
    /// * making `seal_deterministic` random renames the config tree on every publish, so every node
    ///   re-materializes the whole config for nothing — the failure the tree's content-addressing
    ///   cannot tolerate.
    ///
    /// Falsification: call `seal` from `seal_deterministic` and the first pair of assertions fails;
    /// route both through `deterministic_nonce` and the second pair fails.
    #[test]
    fn random_sealing_stays_random_and_deterministic_sealing_does_not() {
        let kp = kp();
        let a = kp.seal(b"sk-one").expect("seal");
        let b = kp.seal(b"sk-one").expect("seal");
        assert_ne!(
            a, b,
            "`seal` must use a fresh random nonce: two seals of the same secret must differ"
        );

        let c = kp.seal_deterministic(b"sk-one").expect("seal");
        let d = kp.seal_deterministic(b"sk-one").expect("seal");
        assert_eq!(
            c, d,
            "the same plaintext must seal to the same bytes, nonce included"
        );
        assert_ne!(
            c,
            kp.seal_deterministic(b"sk-two").expect("seal"),
            "a different plaintext must produce a different ciphertext (and nonce)"
        );

        // A separate instance of the SAME key must agree — that is what lets any node publish the
        // same tree. Two providers built from one key material, as two processes would be.
        let other = StaticKeyProvider::new([7u8; KEY_LEN], 1);
        assert_eq!(
            c,
            other.seal_deterministic(b"sk-one").expect("seal"),
            "determinism must survive a new process, not just a second call"
        );

        // The version is part of the nonce input, so a rotation re-seals everything.
        let rotated = StaticKeyProvider::with_previous([7u8; KEY_LEN], 2, [7u8; KEY_LEN], 1);
        let e = rotated.seal_deterministic(b"sk-one").expect("seal");
        assert_ne!(
            c.nonce, e.nonce,
            "the nonce must change with the key version, or a rotation would reuse it"
        );

        // ...and every one of them still opens, with the version they were sealed under.
        for sealed in [&c, &e] {
            let open_kp = if sealed.key_version == 2 {
                &rotated
            } else {
                &kp
            };
            assert_eq!(
                open_kp.open(sealed).expect("open"),
                b"sk-one",
                "a deterministically sealed value must open like any other"
            );
        }
    }

    /// The TEXT envelope round-trips, and it holds no plaintext of the key.
    ///
    /// This is the form `limit_role.matching_key` is stored in (decision D-16) and the form the
    /// config tree carries, so "the column is not the key" is a property of ONE function.
    ///
    /// Falsification: return the plaintext from `to_text` and the `contains` assertion fails; drop
    /// the nonce from the layout and the round-trip fails.
    #[test]
    fn the_text_envelope_round_trips_and_hides_the_key() {
        let kp = kp();
        let secret = "sk-live-customer-key-0001";
        let text = kp.seal(secret.as_bytes()).expect("seal").to_text();

        assert!(
            !text.contains(secret),
            "the envelope must not contain the key: {text}"
        );
        assert!(
            text.starts_with(SEALED_TEXT_PREFIX),
            "the envelope must be recognisable: {text}"
        );

        let sealed = Sealed::from_text(&text).expect("parses as an envelope");
        assert_eq!(
            kp.open(&sealed).expect("open"),
            secret.as_bytes(),
            "the envelope must open back to the key"
        );
        assert_eq!(
            Sealed::open_text(&kp, &text).expect("open_text"),
            secret,
            "the TEXT entry point must agree with the typed one"
        );

        // v1's layout is nonce ‖ ciphertext; a hand-built envelope must agree with the codec, or
        // the format is only round-tripping against itself.
        let raw = base64::engine::general_purpose::STANDARD
            .decode(
                text.strip_prefix(SEALED_TEXT_PREFIX)
                    .unwrap()
                    .split_once(':')
                    .unwrap()
                    .1,
            )
            .expect("base64");
        assert_eq!(&raw[..NONCE_LEN], &sealed.nonce, "nonce comes first");
        assert_eq!(&raw[NONCE_LEN..], sealed.ciphertext.as_slice());
    }

    /// A value that is NOT an envelope is a pre-D-16 plaintext row, and it is returned as-is.
    ///
    /// This is the migration (the user's ruling: legacy rows are re-sealed automatically, which
    /// first requires being able to READ them). It is also the one place where the code deliberately
    /// does not fail closed, so the boundary is pinned: only a STRUCTURAL non-envelope is passed
    /// through. An envelope that will not open is an error, and never the ciphertext.
    ///
    /// Falsification: return `Err` for the plaintext case and the first assertion fails; fall back to
    /// plaintext when `open` fails and the last one does (it would return the envelope text).
    #[test]
    fn open_text_passes_plaintext_through_and_refuses_an_unopenable_envelope() {
        let kp = kp();
        let legacy = "sk-legacy-plaintext-key";
        assert_eq!(
            Sealed::open_text(&kp, legacy).expect("plaintext passes through"),
            legacy
        );

        // Nearly-an-envelope strings are plaintext, not "corrupt ciphertext": a wrong tag, a wrong
        // version field, a truncated payload.
        for almost in [
            "sealed:v2:1:AAAA",
            "sealed:v1:x:AAAA",
            "sealed:v1:1:not base64!",
            "sealed:v1:1:AAAA",
            "sealed:v1:1:",
            "unsealed:v1:1:AAAA",
        ] {
            assert!(
                Sealed::from_text(almost).is_none(),
                "{almost:?} must not parse as an envelope"
            );
            assert_eq!(
                Sealed::open_text(&kp, almost).expect("read as plaintext"),
                almost,
                "{almost:?} must be read as a plaintext value"
            );
        }

        // A REAL envelope under the wrong key: an error, and NOT the envelope text.
        let wrong = StaticKeyProvider::new([9u8; KEY_LEN], 1);
        let text = kp.seal(b"sk-under-another-key").expect("seal").to_text();
        assert!(
            matches!(Sealed::open_text(&wrong, &text), Err(CryptoError::Decrypt)),
            "an envelope that does not open must be refused, not handed back"
        );
        // ...and a version outside the ring is refused as such.
        let rotated = StaticKeyProvider::new([7u8; KEY_LEN], 5);
        assert!(
            matches!(
                Sealed::open_text(&rotated, &text),
                Err(CryptoError::KeyVersionMismatch { .. })
            ),
            "a version outside the ring must be refused by name"
        );
    }
}
