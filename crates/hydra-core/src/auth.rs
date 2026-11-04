//! API-key hashing and HMAC primitives (pure).
//!
//! ## Ownership split
//!
//! - [`sha256_hex`] — the real crypto digest used to key identity lookups
//!   (the plaintext api-key is never resident in derived maps);
//! - [`sha256_hex_string`] — the ONE place that turns a digest into the lower-case
//!   hex form used as a string key / stream payload;
//! - [`hmac_sha256`] — RFC 2104 HMAC-SHA256, needed where a deterministic,
//!   keyed derivation is required rather than a bare digest.
//!
//! All functions are pure and take their inputs explicitly, so testing is
//! deterministic — no hidden time, no I/O, no global state.

use sha2::{Digest, Sha256};

/// SHA-256 digest of `input`, returned as **raw 32 bytes**.
///
/// (The `_hex` suffix in the name is historical; the return type is the raw
/// digest, matching the identity-hash atoms used by the server.) Used to key
/// identity lookups so plaintext api-keys are never resident.
///
/// This is a real, pure computation (sha2) — not a stub.
#[must_use]
pub fn sha256_hex(input: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(input);
    hasher.finalize().into()
}

/// Lower-case hex of [`sha256_hex`]: the ONE place that turns a digest into the
/// string form used as a key / stream payload.
#[must_use]
pub fn sha256_hex_string(input: &[u8]) -> String {
    let mut out = String::with_capacity(64);
    for b in sha256_hex(input) {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// HMAC-SHA256 (RFC 2104), built on the digest above.
///
/// ## Why a MAC lives in this module
///
/// The Arachne config tree is **content-addressed**: its name is the hash of its
/// bytes, so a secret sealed with a random nonce renames the tree on every publish
/// and every node re-materializes for nothing. `hydra_server::cluster::arachne_entities`
/// therefore seals with a nonce DERIVED from the plaintext and the master key
/// (`hydra_server::crypto::KeyProvider::seal_deterministic`), and a plaintext-derived
/// nonce has to be a keyed function — otherwise anyone who guesses a plaintext knows
/// the nonce. That needs a MAC; `sha2` is the only digest dependency in the workspace
/// and provides none, and hand-rolling one next to the code that uses it is how
/// primitives get subtly wrong.
///
/// Implemented from RFC 2104. **Validated against the RFC 4231 test vectors**
/// below, whose expected values were produced with an independent implementation
/// (Python's `hmac`) rather than transcribed — a hand-rolled MAC checked against
/// its own output proves nothing.
///
/// Any key length is legal (`len(K) > 64` is hashed first, per the RFC); this
/// repo's master keys are 32 bytes.
#[must_use]
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    /// SHA-256's block size in bytes — the width of the ipad/opad pads.
    const BLOCK: usize = 64;

    // K' = K padded to one block, or H(K) padded when K is longer than a block.
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..32].copy_from_slice(&sha256_hex(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }

    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(message);
    let inner_digest: [u8; 32] = inner.finalize().into();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_digest);
    outer.finalize().into()
}

#[cfg(test)]
mod hmac_tests {
    use super::hmac_sha256;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// RFC 4231 test vectors, cases 1–5.
    ///
    /// The expected digests came from an INDEPENDENT implementation (Python's `hmac`), not from
    /// memory and not from this code. Case 1 is the all-`0x0b` key, case 2 the short ASCII key
    /// ("Jefe"), case 3 a 20-byte key, case 4 a 25-byte key (still shorter than a block), and case
    /// 5 a 131-byte key — the one that exercises the `len(K) > 64 ⇒ H(K)` branch, which no other
    /// case reaches.
    ///
    /// Falsification: drop the `H(K)` branch and case 5 fails alone.
    #[test]
    fn rfc4231_vectors() {
        assert_eq!(
            hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex(&hmac_sha256(&[0xaa; 20], &[0xdd; 50])),
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
        );
        assert_eq!(
            hex(&hmac_sha256(&(1u8..=25).collect::<Vec<u8>>(), &[0xcd; 50])),
            "82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b"
        );
        assert_eq!(
            hex(&hmac_sha256(&[0x0c; 131], b"Test With Truncation")),
            "547b09f769795c518c182386366bce5a368cf207843f167938cbf22ece23f368"
        );
    }

    /// The property the tree depends on: same key + same message ⇒ same MAC; a different key or a
    /// different message ⇒ a different one.
    #[test]
    fn deterministic_and_key_separated() {
        let k1 = [7u8; 32];
        let k2 = [8u8; 32];
        assert_eq!(hmac_sha256(&k1, b"sk-one"), hmac_sha256(&k1, b"sk-one"));
        assert_ne!(hmac_sha256(&k1, b"sk-one"), hmac_sha256(&k2, b"sk-one"));
        assert_ne!(hmac_sha256(&k1, b"sk-one"), hmac_sha256(&k1, b"sk-two"));
        // A message that is a prefix of another must not collide: the pads make it length-aware,
        // and GCM's tag covers the length, but this catches a "hash the concatenation" refactor.
        assert_ne!(hmac_sha256(&k1, b"sk"), hmac_sha256(&k1, b"sk-one"));
        assert_eq!(hmac_sha256(&k1, b"sk-one").len(), 32);
    }
}
