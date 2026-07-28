//! Cryptographic primitives for the privacy layer: X25519 key agreement
//! and authenticated (ChaCha20-Poly1305) encryption for session data.
//!
//! This intentionally does **not** implement AEAD by hand from the raw
//! `chacha20`/`poly1305` primitives. An earlier version of this module
//! did exactly that, and while it happened to round-trip correctly, it
//! deviated from the actual IETF ChaCha20-Poly1305 construction (RFC
//! 8439) — specifically, it didn't discard the unused second half of the
//! first keystream block before starting encryption — which made it
//! non-standard, non-interoperable, and unvalidated against any published
//! test vectors. Rolling your own crypto primitive composition is a well
//! known way to introduce subtle, hard-to-detect vulnerabilities even
//! when the code "looks right" and passes a round-trip test. This version
//! uses the RustCrypto `chacha20poly1305` crate, which implements the
//! standard construction and is exercised by that project's own test
//! suite against the RFC 8439 vectors.
//!
//! Key agreement still uses X25519 directly (via `x25519-dalek`, also an
//! audited implementation), but the raw Diffie-Hellman output is now
//! passed through HKDF-SHA256 before use as a symmetric key, rather than
//! being used directly. Raw ECDH output shouldn't be used as a key as-is;
//! passing it through a KDF is standard practice and cheap insurance.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce as AeadNonce};
use rand::RngCore;
use x25519_dalek::{PublicKey as XPublicKey, StaticSecret};
use zeroize::Zeroize;

use crate::error::{NexusError, Result};

/// A 32-byte symmetric key. Zeroized on drop so key material doesn't
/// linger in memory longer than necessary.
#[derive(Clone, Zeroize)]
#[zeroize(drop)]
pub struct SymmetricKey(pub [u8; 32]);

/// An X25519 public key.
#[derive(Clone, Debug)]
pub struct PublicKey(pub [u8; 32]);

/// An X25519 private key. Zeroized on drop.
#[derive(Clone, Zeroize)]
#[zeroize(drop)]
pub struct PrivateKey(pub [u8; 32]);

/// A 12-byte ChaCha20-Poly1305 nonce. Must never be reused with the same
/// key; [`generate_nonce`] draws from a CSPRNG for every call, which is
/// the standard mitigation as long as a single key isn't used for an
/// astronomically large number of messages (well beyond what a session
/// key's lifetime here would ever see).
pub type Nonce = [u8; 12];

/// Generates a fresh X25519 keypair.
pub fn generate_keypair() -> (PublicKey, PrivateKey) {
    let secret = StaticSecret::random_from_rng(rand::thread_rng());
    let public = XPublicKey::from(&secret);
    (PublicKey(*public.as_bytes()), PrivateKey(secret.to_bytes()))
}

/// Performs X25519 Diffie-Hellman key agreement and derives a symmetric
/// key from the result via HKDF-SHA256, rather than using the raw shared
/// secret directly.
pub fn key_agreement(private: &PrivateKey, public: &PublicKey) -> SymmetricKey {
    let secret = StaticSecret::from(private.0);
    let pub_key = XPublicKey::from(public.0);
    let shared = secret.diffie_hellman(&pub_key);
    derive_key(shared.as_bytes(), b"nexus-x25519-ecdh")
}

/// Encrypts `plaintext` with ChaCha20-Poly1305 under a freshly generated
/// random nonce, returning `(nonce, ciphertext_with_tag)`.
pub fn encrypt(key: &SymmetricKey, plaintext: &[u8]) -> Result<(Nonce, Vec<u8>)> {
    if plaintext.is_empty() {
        return Err(NexusError::Other("plaintext is empty".to_string()));
    }

    let cipher = ChaCha20Poly1305::new_from_slice(&key.0)
        .map_err(|e| NexusError::Other(format!("failed to initialize cipher: {}", e)))?;
    let nonce_bytes = generate_nonce();
    let nonce = AeadNonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, Payload { msg: plaintext, aad: &[] })
        .map_err(|_| NexusError::Other("encryption failed".to_string()))?;

    Ok((nonce_bytes, ciphertext))
}

/// Decrypts and authenticates `ciphertext` (as produced by [`encrypt`]).
/// Returns an error if the authentication tag doesn't match — including
/// if the ciphertext was tampered with, or the wrong key/nonce was used.
pub fn decrypt(key: &SymmetricKey, nonce: &Nonce, ciphertext: &[u8]) -> Result<Vec<u8>> {
    if ciphertext.len() < 16 {
        return Err(NexusError::Other("ciphertext too short".to_string()));
    }

    let cipher = ChaCha20Poly1305::new_from_slice(&key.0)
        .map_err(|e| NexusError::Other(format!("failed to initialize cipher: {}", e)))?;
    let nonce = AeadNonce::from_slice(nonce);

    cipher
        .decrypt(nonce, Payload { msg: ciphertext, aad: &[] })
        .map_err(|_| NexusError::Other("authentication tag mismatch".to_string()))
}

/// Derives a 32-byte symmetric key from `ikm` (input keying material) and
/// `salt` via HKDF-SHA256.
pub fn derive_key(ikm: &[u8], salt: &[u8]) -> SymmetricKey {
    use hkdf::Hkdf;
    use sha2::Sha256;
    let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut okm = [0u8; 32];
    hk.expand(&[], &mut okm)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    SymmetricKey(okm)
}

/// Generates a random 12-byte nonce from a CSPRNG.
pub fn generate_nonce() -> Nonce {
    let mut nonce = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce);
    nonce
}

/// Zeroizes a byte slice in place (best-effort; the compiler may still
/// reorder around this in unusual cases, but `zeroize` uses volatile
/// writes specifically to resist that).
pub fn zeroize_bytes(data: &mut [u8]) {
    data.zeroize();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let (pk_a, sk_a) = generate_keypair();
        let (pk_b, sk_b) = generate_keypair();
        let key_a = key_agreement(&sk_a, &pk_b);
        let key_b = key_agreement(&sk_b, &pk_a);
        assert_eq!(key_a.0, key_b.0, "ECDH shared secrets must match on both sides");

        let msg = b"hello nexus privacy layer";
        let (nonce, ciphertext) = encrypt(&key_a, msg).unwrap();
        let plaintext = decrypt(&key_b, &nonce, &ciphertext).unwrap();
        assert_eq!(plaintext, msg);
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let key = derive_key(b"some input keying material", b"test-salt");
        let (nonce, mut ciphertext) = encrypt(&key, b"authenticate me").unwrap();
        ciphertext[0] ^= 0xFF;
        assert!(decrypt(&key, &nonce, &ciphertext).is_err());
    }

    #[test]
    fn wrong_key_is_rejected() {
        let key_a = derive_key(b"key material a", b"salt");
        let key_b = derive_key(b"key material b", b"salt");
        let (nonce, ciphertext) = encrypt(&key_a, b"secret message").unwrap();
        assert!(decrypt(&key_b, &nonce, &ciphertext).is_err());
    }

    #[test]
    fn wrong_nonce_is_rejected() {
        let key = derive_key(b"key material", b"salt");
        let (_nonce, ciphertext) = encrypt(&key, b"secret message").unwrap();
        let wrong_nonce = [0u8; 12];
        assert!(decrypt(&key, &wrong_nonce, &ciphertext).is_err());
    }

    #[test]
    fn empty_plaintext_is_rejected() {
        let key = derive_key(b"key material", b"salt");
        assert!(encrypt(&key, b"").is_err());
    }

    #[test]
    fn nonces_are_unique_across_calls() {
        let a = generate_nonce();
        let b = generate_nonce();
        assert_ne!(a, b, "two independently generated nonces should not collide");
    }

    #[test]
    fn derive_key_is_deterministic_for_same_inputs() {
        let key1 = derive_key(b"input", b"salt");
        let key2 = derive_key(b"input", b"salt");
        assert_eq!(key1.0, key2.0);
    }

    #[test]
    fn derive_key_differs_for_different_salts() {
        let key1 = derive_key(b"input", b"salt-a");
        let key2 = derive_key(b"input", b"salt-b");
        assert_ne!(key1.0, key2.0);
    }

    #[test]
    fn underlying_aead_crate_behaves_as_an_aead_should() {
        // We don't hand-roll the AEAD construction anymore — this just
        // confirms the wrapper is using the crate correctly: ciphertext
        // is plaintext length + 16-byte tag, differs from the plaintext,
        // and round-trips.
        let key_bytes = [0x42u8; 32];
        let cipher = ChaCha20Poly1305::new_from_slice(&key_bytes).unwrap();
        let nonce_bytes = [0x24u8; 12];
        let nonce = AeadNonce::from_slice(&nonce_bytes);
        let plaintext = b"sunscreen would be it";

        let ciphertext = cipher
            .encrypt(nonce, Payload { msg: plaintext.as_slice(), aad: &[] })
            .unwrap();
        assert_eq!(ciphertext.len(), plaintext.len() + 16);
        assert_ne!(&ciphertext[..plaintext.len()], plaintext.as_slice());

        let decrypted = cipher
            .decrypt(nonce, Payload { msg: ciphertext.as_slice(), aad: &[] })
            .unwrap();
        assert_eq!(decrypted, plaintext);
    }
}
