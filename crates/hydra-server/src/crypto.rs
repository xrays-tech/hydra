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
#[derive(Debug, Clone)]
pub struct Sealed {
    pub ciphertext: Vec<u8>,
    pub nonce: [u8; NONCE_LEN],
    pub key_version: u32,
}

/// Abstraction over master-key sources. `StaticKeyProvider` reads the key from
/// the environment; a future `KmsKeyProvider` (AWS KMS / HashiCorp Vault) will
/// implement this same trait without changing call sites.
///
/// TODO(kms): implement `KmsKeyProvider` backed by AWS KMS / HashiCorp Vault.
pub trait KeyProvider: Send + Sync {
    /// Encrypt under the provider's CURRENT key version.
    fn seal(&self, plaintext: &[u8]) -> Result<Sealed, CryptoError>;
    /// Decrypt a stored secret. Fail-closed: a version outside the provider's
    /// ring is an error, never a guess.
    fn open(&self, sealed: &Sealed) -> Result<Vec<u8>, CryptoError>;
    /// The version `seal` writes — the target of a re-seal
    /// ([`crate::db::reseal_secrets`]).
    fn version(&self) -> u32;
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
}
