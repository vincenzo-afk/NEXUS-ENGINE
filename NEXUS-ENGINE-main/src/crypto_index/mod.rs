//! An encrypted inverted index that can be queried without decrypting
//! the whole index — built on the same audited primitives already used
//! by `crate::privacy::crypto` (X25519 + ChaCha20-Poly1305 via
//! `chacha20poly1305`, HKDF-SHA256 key derivation).
//!
//! **Read this before calling it "zero-knowledge" or "homomorphic."
//! It is neither, and claiming either would be a real, substantive
//! overstatement of what's implemented.** This is Searchable Symmetric
//! Encryption (SSE) of the well-known "static SSE" shape (à la Curtmola
//! et al.'s SSE-1/SSE-2 constructions): term → keyed-HMAC "trapdoor" →
//! encrypted posting list. It genuinely lets you:
//! - Store an index at rest such that a snapshot of the encrypted index
//!   file, without the key, reveals no term text and no readable posting
//!   lists (each posting list is ChaCha20-Poly1305-encrypted).
//! - Query by term without decrypting any other term's postings — you
//!   compute the queried term's trapdoor and decrypt only that entry.
//!
//! It does **not**:
//! - Hide *access patterns* — the server/storage layer can see which
//!   trapdoor was queried and how large the returned ciphertext is,
//!   which is a well-known SSE leakage profile (this is the standard,
//!   documented tradeoff of practical SSE vs. much more expensive
//!   fully-oblivious schemes). If access-pattern privacy against a
//!   hostile storage provider is a hard requirement, this is not
//!   sufficient on its own.
//! - Implement homomorphic encryption (arbitrary computation on
//!   encrypted data). There is no ranking math happening "inside" the
//!   ciphertexts — decryption of the matched postings happens locally,
//!   client-side, after trapdoor lookup, same as any client holding the
//!   key would need to.
//! - Support arbitrary boolean/phrase queries against the encrypted
//!   index directly; it supports single-term lookup (the base SSE
//!   primitive). Multi-term AND/OR queries are done by decrypting each
//!   term's postings locally and intersecting/unioning the resulting
//!   doc ID sets client-side — real, but not "encrypted boolean search."
//!
//! Same target use case the module doc comment on this feature request
//! described: an encrypted index file that's safe to sync to an
//! untrusted cloud store (S3/Dropbox/etc.) and query from another
//! device holding the key, without that store ever seeing plaintext
//! terms or posting lists.

use crate::privacy::crypto::{decrypt, derive_key, encrypt, SymmetricKey};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashMap;

type HmacSha256 = Hmac<Sha256>;

/// A term's blinded identifier — an HMAC-SHA256 of the normalized term
/// under the index's trapdoor key, hex-encoded. This is what's actually
/// stored as the map key, never the term text itself.
pub type Trapdoor = String;

/// One encrypted posting list entry: the document IDs that contain a
/// given term, stored as ChaCha20-Poly1305 ciphertext + nonce.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EncryptedPosting {
    nonce: [u8; 12],
    ciphertext: Vec<u8>,
}

/// The encrypted index itself: safe to serialize and store/sync
/// anywhere untrusted. Contains no plaintext terms and no readable
/// posting lists without the key used to build it.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct EncryptedIndex {
    postings: HashMap<Trapdoor, EncryptedPosting>,
}

/// Holds the two keys derived from one master key: one for computing
/// trapdoors (deterministic, so the same term always maps to the same
/// trapdoor — necessary for lookup), one for encrypting posting lists
/// (via the existing authenticated `crate::privacy::crypto::encrypt`).
/// Keeping these separate (rather than reusing one key for both roles)
/// follows standard SSE construction practice and the general
/// cryptographic hygiene principle of not reusing a key across
/// different primitives/purposes.
pub struct IndexKey {
    trapdoor_key: [u8; 32],
    encryption_key: SymmetricKey,
}

impl IndexKey {
    /// Derives both sub-keys from a master passphrase/key via
    /// HKDF-SHA256 (through the existing `derive_key` helper), with
    /// distinct salts so the two derived keys are cryptographically
    /// independent even though they come from the same input key
    /// material.
    pub fn derive(master_key: &[u8]) -> Self {
        let trapdoor_symmetric = derive_key(master_key, b"nexus-sse-trapdoor-key-v1");
        let encryption_key = derive_key(master_key, b"nexus-sse-posting-encryption-key-v1");
        IndexKey {
            trapdoor_key: trapdoor_symmetric.0,
            encryption_key,
        }
    }

    fn trapdoor_for(&self, term: &str) -> Trapdoor {
        let normalized = crate::text::normalize(term);
        let mut mac =
            HmacSha256::new_from_slice(&self.trapdoor_key).expect("HMAC accepts any key length");
        mac.update(normalized.as_bytes());
        hex_encode(&mac.finalize().into_bytes())
    }
}

impl EncryptedIndex {
    pub fn new() -> Self {
        EncryptedIndex::default()
    }

    /// Encrypts and stores/replaces the posting list (document IDs) for
    /// `term` under `key`.
    pub fn set_postings(
        &mut self,
        key: &IndexKey,
        term: &str,
        doc_ids: &[crate::document::DocId],
    ) -> Result<(), String> {
        let trapdoor = key.trapdoor_for(term);
        let plaintext = bincode_postings(doc_ids);
        let (nonce, ciphertext) = encrypt(&key.encryption_key, &plaintext)
            .map_err(|e| format!("encryption failed: {e}"))?;
        self.postings.insert(
            trapdoor,
            EncryptedPosting {
                nonce,
                ciphertext,
            },
        );
        Ok(())
    }

    /// Looks up `term`'s posting list. Computing the trapdoor requires
    /// no key material beyond `trapdoor_key` (so a party could compute
    /// trapdoors without being able to decrypt postings, if that
    /// separation is useful), but this convenience method takes the
    /// full `IndexKey` since a typical caller has both.
    pub fn query(&self, key: &IndexKey, term: &str) -> Result<Vec<crate::document::DocId>, String> {
        let trapdoor = key.trapdoor_for(term);
        let Some(entry) = self.postings.get(&trapdoor) else {
            return Ok(Vec::new()); // term not present in the index at all
        };
        let plaintext = decrypt(&key.encryption_key, &entry.nonce, &entry.ciphertext)
            .map_err(|e| format!("decryption failed (wrong key, or corrupted data): {e}"))?;
        Ok(unbincode_postings(&plaintext))
    }

    /// Multi-term AND query: decrypts each term's postings locally and
    /// intersects the resulting doc ID sets. As the module doc comment
    /// says, the encryption doesn't natively support boolean queries —
    /// this is the client doing the boolean logic after individually
    /// authorized trapdoor lookups, not a property of the ciphertext.
    pub fn query_and(&self, key: &IndexKey, terms: &[&str]) -> Result<Vec<crate::document::DocId>, String> {
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let mut sets = Vec::with_capacity(terms.len());
        for term in terms {
            let postings = self.query(key, term)?;
            sets.push(postings.into_iter().collect::<std::collections::HashSet<_>>());
        }
        let mut result = sets[0].clone();
        for s in &sets[1..] {
            result = result.intersection(s).copied().collect();
        }
        let mut result: Vec<_> = result.into_iter().collect();
        result.sort_unstable();
        Ok(result)
    }

    pub fn term_count(&self) -> usize {
        self.postings.len()
    }

    /// Saves the encrypted index to `path` as JSON — safe to write to
    /// any storage, trusted or not (that's the entire point: no term
    /// text or readable posting list is ever in this serialized form).
    pub fn save(&self, path: &std::path::Path) -> crate::error::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| crate::error::NexusError::io(parent, e))?;
        }
        let text = serde_json::to_string(self)
            .map_err(|e| crate::error::NexusError::Other(format!("failed to serialize encrypted index: {e}")))?;
        std::fs::write(path, text).map_err(|e| crate::error::NexusError::io(path, e))
    }

    /// Loads a previously saved encrypted index from `path`.
    pub fn load(path: &std::path::Path) -> crate::error::Result<EncryptedIndex> {
        let text = std::fs::read_to_string(path).map_err(|e| crate::error::NexusError::io(path, e))?;
        serde_json::from_str(&text).map_err(|e| {
            crate::error::NexusError::Other(format!("failed to parse encrypted index at {}: {e}", path.display()))
        })
    }
}

/// Builds an [`EncryptedIndex`] from a live [`crate::index::Index`]'s
/// inverted index: every term in the vocabulary gets its posting list
/// (just the `doc_id`s — term frequencies/positions aren't carried over,
/// since SSE query results are a doc ID set for the client to look up
/// via `crate::index::Store`, not a ranked/scored result; see the module
/// doc comment on what this construction does and doesn't support).
/// This is the actual activation point: everything above this function
/// is the SSE primitive itself, but nothing previously called it against
/// a real index — `nexus export-encrypted` (see `cli::commands`) is
/// the live, user-facing entry point that does.
pub fn build_from_index(
    index: &crate::index::Index,
    key: &IndexKey,
) -> Result<EncryptedIndex, String> {
    let mut encrypted = EncryptedIndex::new();
    for (term, term_id) in index.vocabulary.iter() {
        let Some(posting_list) = index.inverted.postings_for(term_id) else { continue };
        let doc_ids: Vec<crate::document::DocId> =
            posting_list.postings.iter().map(|p| p.doc_id).collect();
        encrypted.set_postings(key, term, &doc_ids)?;
    }
    Ok(encrypted)
}

fn bincode_postings(doc_ids: &[crate::document::DocId]) -> Vec<u8> {
    doc_ids.iter().flat_map(|id| id.to_le_bytes()).collect()
}

fn unbincode_postings(bytes: &[u8]) -> Vec<crate::document::DocId> {
    bytes
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes(chunk.try_into().unwrap()))
        .collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_queries_a_term() {
        let key = IndexKey::derive(b"a test master key, at least 16 bytes");
        let mut index = EncryptedIndex::new();
        index.set_postings(&key, "rust", &[1, 5, 9]).unwrap();
        let results = index.query(&key, "rust").unwrap();
        assert_eq!(results, vec![1, 5, 9]);
    }

    #[test]
    fn wrong_key_fails_to_decrypt_rather_than_returning_garbage() {
        let key = IndexKey::derive(b"correct key material, 16+ bytes");
        let wrong_key = IndexKey::derive(b"a totally different key, 16+ b");
        let mut index = EncryptedIndex::new();
        index.set_postings(&key, "rust", &[1, 2, 3]).unwrap();
        // The wrong key computes a different trapdoor, so this looks
        // like "term not found" rather than a decryption failure — which
        // is the correct SSE behavior (an attacker without the key
        // cannot distinguish "wrong key" from "term absent").
        let result = index.query(&wrong_key, "rust").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn term_not_in_index_returns_empty_not_error() {
        let key = IndexKey::derive(b"some key material, 16+ bytes long");
        let index = EncryptedIndex::new();
        assert_eq!(index.query(&key, "nonexistent").unwrap(), Vec::<u32>::new());
    }

    #[test]
    fn and_query_intersects_across_terms() {
        let key = IndexKey::derive(b"some key material, 16+ bytes long");
        let mut index = EncryptedIndex::new();
        index.set_postings(&key, "rust", &[1, 2, 3, 4]).unwrap();
        index.set_postings(&key, "ownership", &[2, 4, 6]).unwrap();
        let result = index.query_and(&key, &["rust", "ownership"]).unwrap();
        assert_eq!(result, vec![2, 4]);
    }

    #[test]
    fn encrypted_index_serializes_without_leaking_plaintext_term() {
        let key = IndexKey::derive(b"some key material, 16+ bytes long");
        let mut index = EncryptedIndex::new();
        index.set_postings(&key, "confidential-project-codename", &[42]).unwrap();
        let serialized = serde_json::to_string(&index).unwrap();
        assert!(!serialized.contains("confidential-project-codename"));
    }

    #[test]
    fn build_from_index_encrypts_every_vocabulary_term() {
        let mut nexus_index = crate::index::Index::new();
        let doc = crate::document::Document {
            metadata: crate::document::DocumentMetadata {
                path: std::path::PathBuf::from("/notes.txt"),
                file_name: "notes.txt".to_string(),
                extension: "txt".to_string(),
                size_bytes: 0,
                modified_unix: 0,
                token_count: 0,
                acl: crate::entity::Acl::public(),
            },
            content: "rust ownership borrowing".to_string(),
        };
        nexus_index.index_document(doc);

        let key = IndexKey::derive(b"a real passphrase, 16+ bytes long");
        let encrypted = build_from_index(&nexus_index, &key).unwrap();

        assert_eq!(encrypted.term_count(), nexus_index.vocabulary.len());
        let results = encrypted.query(&key, "rust").unwrap();
        assert_eq!(results, vec![0]); // first-indexed doc gets doc_id 0

        let serialized = serde_json::to_string(&encrypted).unwrap();
        assert!(!serialized.contains("rust"), "no plaintext term should leak into the serialized encrypted index");
    }
}
