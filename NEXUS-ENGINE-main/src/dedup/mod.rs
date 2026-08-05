//! Near-duplicate detection.
//!
//! Exact duplicates (byte-identical content, e.g. the same article served
//! from a mirror) are caught with a plain content hash. Near-duplicates
//! (the same article with a different ad banner, a tracking parameter, or
//! minor edits) need something fuzzier: this module builds w-shingles
//! (overlapping windows of tokens) over the extracted text and combines
//! them into a 64-bit SimHash fingerprint, so that two documents whose
//! fingerprints differ in only a handful of bits are almost certainly the
//! same underlying content.

use log::debug;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Number of consecutive tokens per shingle. 4-5 is a common choice: long
/// enough that common short phrases don't dominate, short enough to still
/// catch near-duplicates with localized edits.
const SHINGLE_SIZE: usize = 5;

/// Two fingerprints whose Hamming distance is at or below this threshold
/// are considered near-duplicates. In a 64-bit fingerprint, a threshold
/// around 15-20 bits (roughly a quarter of the bits) catches real
/// duplicates — the same article with a different ad banner or a handful
/// of edits — while still clearly separating unrelated content, which
/// typically differs in 28+ bits.
pub const SIMHASH_DUPLICATE_THRESHOLD: u32 = 18;

/// A 64-bit SimHash fingerprint of a document's content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimHash(pub u64);

impl SimHash {
    /// Computes the SimHash of `text`: normalized, tokenized, shingled,
    /// then each shingle's hash is folded into a 64-dimension weighted
    /// vote, which is finally collapsed to a bit pattern.
    pub fn compute(text: &str) -> SimHash {
        debug!("computing SimHash ({} bytes)", text.len());
        let normalized = crate::text::normalize(text);
        let tokens: Vec<String> = crate::text::tokenize(&normalized)
            .into_iter()
            .map(|t| t.text)
            .collect();

        if tokens.is_empty() {
            return SimHash(0);
        }

        let shingles = shingles(&tokens, SHINGLE_SIZE);
        let mut weights = [0i64; 64];

        for shingle in &shingles {
            let hash = hash_shingle(shingle);
            for bit in 0..64u32 {
                if (hash >> bit) & 1 == 1 {
                    weights[bit as usize] += 1;
                } else {
                    weights[bit as usize] -= 1;
                }
            }
        }

        let mut fingerprint: u64 = 0;
        for (bit, &weight) in weights.iter().enumerate() {
            if weight > 0 {
                fingerprint |= 1 << bit;
            }
        }
        let result = SimHash(fingerprint);
        debug!("SimHash: {:016x}", result.0);
        result
    }

    /// Hamming distance to another fingerprint: the number of differing bits.
    pub fn hamming_distance(&self, other: &SimHash) -> u32 {
        (self.0 ^ other.0).count_ones()
    }

    /// Expresses the fingerprint distance to `other` as a similarity
    /// fraction in `[0.0, 1.0]`, where `1.0` is identical and `0.0` is
    /// maximally different (all 64 bits differ). This is a coarse,
    /// SimHash-specific notion of "percent similar" — useful for a
    /// human-facing threshold like "hide it if it's more than 80% similar
    /// to a result we're already showing" — not a rigorous content-overlap
    /// percentage.
    pub fn similarity(&self, other: &SimHash) -> f32 {
        1.0 - (self.hamming_distance(other) as f32 / 64.0)
    }

    /// `true` if this fingerprint is within [`SIMHASH_DUPLICATE_THRESHOLD`]
    /// bits of `other`.
    pub fn is_near_duplicate_of(&self, other: &SimHash) -> bool {
        self.hamming_distance(other) <= SIMHASH_DUPLICATE_THRESHOLD
    }
}

/// Builds overlapping windows of `size` consecutive tokens each. Returns a
/// single shingle containing every token if there are fewer than `size`
/// tokens total, so very short documents still produce a usable
/// fingerprint.
fn shingles<'a>(tokens: &'a [String], size: usize) -> Vec<Vec<&'a str>> {
    if tokens.len() <= size {
        return vec![tokens.iter().map(|s| s.as_str()).collect()];
    }
    tokens
        .windows(size)
        .map(|w| w.iter().map(|s| s.as_str()).collect())
        .collect()
}

fn hash_shingle(shingle: &[&str]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for token in shingle {
        token.hash(&mut hasher);
        // Separator so "ab" + "c" doesn't collide with "a" + "bc".
        0xFFu8.hash(&mut hasher);
    }
    hasher.finish()
}

/// A plain, exact content hash (SHA-1-strength via a 64-bit FNV-style
/// mix over normalized text), used to catch byte-for-byte duplicates
/// cheaply before falling back to the more expensive SimHash comparison
/// against every other crawled page.
pub fn exact_content_hash(text: &str) -> u64 {
    let normalized = crate::text::normalize(text);
    let mut hasher = DefaultHasher::new();
    normalized.hash(&mut hasher);
    hasher.finish()
}

/// An index of previously-seen SimHash fingerprints, used to check new
/// pages during a crawl against everything crawled so far. Linear in the
/// number of crawled pages; fine for the tens-of-thousands-of-pages scale
/// a single-machine crawler operates at. A production-scale engine would
/// bucket fingerprints (e.g. by top N bits) to avoid the O(n) scan.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DuplicateIndex {
    exact: std::collections::HashMap<u64, crate::document::DocId>,
    fuzzy: Vec<(crate::document::DocId, SimHash)>,
}

impl DuplicateIndex {
    /// Creates an empty duplicate index.
    pub fn new() -> Self {
        DuplicateIndex::default()
    }

    /// Checks whether `text` duplicates (exactly or near-) a
    /// previously-registered document, returning that document's ID if so.
    pub fn find_duplicate(&self, text: &str) -> Option<crate::document::DocId> {
        let exact_hash = exact_content_hash(text);
        if let Some(&doc_id) = self.exact.get(&exact_hash) {
            return Some(doc_id);
        }
        let sim = SimHash::compute(text);
        self.fuzzy
            .iter()
            .find(|(_, existing)| sim.is_near_duplicate_of(existing))
            .map(|(doc_id, _)| *doc_id)
    }

    /// Registers `doc_id`'s content so future documents can be checked
    /// against it. If `doc_id` was already registered (e.g. re-indexed
    /// after a file change), its previous fuzzy-match entry is replaced
    /// rather than left in place — otherwise repeated re-indexing of the
    /// same document would accumulate stale entries in `fuzzy` forever.
    pub fn register(&mut self, doc_id: crate::document::DocId, text: &str) {
        let exact_hash = exact_content_hash(text);
        self.exact.insert(exact_hash, doc_id);
        self.fuzzy.retain(|(id, _)| *id != doc_id);
        let sim = SimHash::compute(text);
        self.fuzzy.push((doc_id, sim));
    }

    /// Returns the registered fingerprint for `doc_id`, if it has been
    /// registered. Used for cross-source (local vs. web) similarity
    /// comparisons, e.g. by hybrid search mode's dedup step.
    pub fn simhash_of(&self, doc_id: crate::document::DocId) -> Option<SimHash> {
        self.fuzzy
            .iter()
            .find(|(id, _)| *id == doc_id)
            .map(|(_, sim)| *sim)
    }

    /// Removes a previously-registered document (e.g. on re-crawl/removal).
    pub fn remove(&mut self, doc_id: crate::document::DocId) {
        self.exact.retain(|_, id| *id != doc_id);
        self.fuzzy.retain(|(id, _)| *id != doc_id);
    }

    /// Number of documents currently registered.
    pub fn len(&self) -> usize {
        self.fuzzy.len()
    }

    /// Returns `true` if no documents are registered.
    pub fn is_empty(&self) -> bool {
        self.fuzzy.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_text_hashes_identically() {
        let a = SimHash::compute("The quick brown fox jumps over the lazy dog");
        let b = SimHash::compute("The quick brown fox jumps over the lazy dog");
        assert_eq!(a, b);
        assert_eq!(a.hamming_distance(&b), 0);
    }

    #[test]
    fn near_identical_text_is_near_duplicate() {
        let original = "Rust is a systems programming language that runs blazingly fast, \
             prevents segfaults, and guarantees thread safety. Many developers \
             love its expressive type system and ownership model.";
        let with_ad_banner = "SPONSORED: Buy now! Rust is a systems programming language that \
             runs blazingly fast, prevents segfaults, and guarantees thread \
             safety. Many developers love its expressive type system and \
             ownership model. Click here to subscribe!";

        let a = SimHash::compute(original);
        let b = SimHash::compute(with_ad_banner);
        assert!(
            a.is_near_duplicate_of(&b),
            "distance was {}",
            a.hamming_distance(&b)
        );
    }

    #[test]
    fn unrelated_text_is_not_duplicate() {
        let a = SimHash::compute(
            "Rust is a systems programming language focused on safety and performance.",
        );
        let b = SimHash::compute(
            "The recipe calls for two cups of flour, a teaspoon of salt, and fresh basil.",
        );
        assert!(!a.is_near_duplicate_of(&b));
    }

    #[test]
    fn duplicate_index_finds_exact_and_fuzzy_matches() {
        let mut index = DuplicateIndex::new();
        index.register(1, "Rust is a systems programming language.");
        assert_eq!(
            index.find_duplicate("Rust is a systems programming language."),
            Some(1)
        );
        assert_eq!(
            index.find_duplicate("Completely unrelated content about gardening tips."),
            None
        );
    }

    #[test]
    fn re_registering_same_doc_id_replaces_rather_than_accumulates() {
        let mut index = DuplicateIndex::new();
        index.register(1, "original content about rust programming");
        index.register(1, "updated content about python programming");
        assert_eq!(index.len(), 1, "re-registering the same doc_id should not grow the fuzzy list");
        let sim = index.simhash_of(1).unwrap();
        assert_eq!(sim, SimHash::compute("updated content about python programming"));
    }

    #[test]
    fn simhash_of_returns_none_for_unregistered_doc() {
        let index = DuplicateIndex::new();
        assert!(index.simhash_of(999).is_none());
    }

    #[test]
    fn similarity_is_one_for_identical_hashes() {
        let a = SimHash::compute("some shared content");
        assert_eq!(a.similarity(&a), 1.0);
    }

    #[test]
    fn similarity_reflects_hamming_distance() {
        let a = SimHash(0b0000_0000);
        let b = SimHash(0b0000_1111); // 4 bits differ out of 64
        let expected = 1.0 - (4.0 / 64.0);
        assert!((a.similarity(&b) - expected).abs() < 1e-6);
    }
}
