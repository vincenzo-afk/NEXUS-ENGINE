//! A lightweight, self-contained lexical vector space model, used to
//! complement BM25 by re-ranking the already keyword-matched candidate
//! set with a second, differently-shaped view of the same lexical
//! evidence.
//!
//! **Read this before calling anything here "semantic search."** This is
//! not a neural embedding model. It has no notion of synonyms, paraphrase,
//! or meaning beyond shared vocabulary — "car" and "automobile" are just
//! as unrelated to it as they are to plain BM25. What a hashing-trick
//! vector genuinely adds on top of BM25 is: a fixed-width dense
//! representation of a document's overall term distribution, compared to
//! the query's by cosine similarity, which can surface documents whose
//! *proportions* of matched terms are similar to the query even when
//! BM25's per-term summation would rank them differently. That's a real,
//! useful, different signal — it is not understanding of meaning.
//!
//! Building actual semantic search would mean running a trained neural
//! embedding model (e.g. via the `candle` or `ort`/ONNX Runtime crates
//! with a downloaded model file) and comparing dense semantic embeddings.
//! That needs a model file and an inference runtime, neither of which
//! this project currently includes — seeing this module and expecting
//! synonym-aware retrieval out of it would be a mistake. See the README's
//! scope notes for what real semantic search would require.
//!
//! ## How document vs. query vectors are weighted
//! Document vectors are pure term-frequency (computed once at index time
//! and persisted in [`VectorIndex`], the same way [`crate::dedup`]'s
//! SimHash fingerprints are). Query vectors are TF-*IDF* weighted against
//! the corpus at search time. This asymmetry is deliberate: IDF requires
//! corpus-wide document-frequency statistics, which drift as documents
//! are added — embedding two documents at different points in the
//! corpus's growth with document-time IDF would give them inconsistent,
//! incomparable weightings. A query is always embedded fresh against
//! current corpus statistics, so it doesn't have that problem.

use crate::index::Index;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Fixed vector dimensionality. 256 keeps the per-document storage
/// overhead modest (~1KB as f32s) while giving the hashing trick enough
/// buckets that collisions between unrelated common terms are rare for
/// typical document vocabularies.
pub const VECTOR_DIM: usize = 256;

/// A dense vector, always exactly [`VECTOR_DIM`] long and L2-normalized,
/// so cosine similarity between two [`LexicalVector`]s reduces to a plain
/// dot product.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LexicalVector(pub Vec<f32>);

impl LexicalVector {
    /// An all-zero vector (the embedding of empty/no text).
    pub fn zero() -> Self {
        LexicalVector(vec![0.0; VECTOR_DIM])
    }

    /// Cosine similarity to `other`. Since both vectors are already
    /// L2-normalized, this is just their dot product.
    pub fn cosine_similarity(&self, other: &LexicalVector) -> f32 {
        self.0.iter().zip(other.0.iter()).map(|(a, b)| a * b).sum()
    }
}

/// Builds a raw (term-frequency-only, unweighted) hashing-trick vector
/// from `text`. Used for documents at index time — see the module doc
/// comment for why documents don't use IDF weighting.
pub fn embed_tf(text: &str) -> LexicalVector {
    embed_weighted(text, |_term| 1.0)
}

/// Builds a hashing-trick vector from `text`, weighting each token's
/// contribution by `weight_fn(term)`. Used for queries, weighted by IDF
/// against the current corpus (see [`query_vector`]).
pub fn embed_weighted(text: &str, weight_fn: impl Fn(&str) -> f32) -> LexicalVector {
    let normalized = crate::text::normalize(text);
    let tokens = crate::text::tokenize(&normalized);

    let mut buckets = vec![0.0f32; VECTOR_DIM];
    for token in &tokens {
        let (index, sign) = hash_bucket(&token.text);
        buckets[index] += sign * weight_fn(&token.text);
    }

    normalize_l2(LexicalVector(buckets))
}

/// Builds the query-side vector for `terms`, weighting each term by
/// (a smoothed variant of) inverse document frequency against `index`'s
/// current corpus statistics: rarer terms across the corpus contribute
/// more to the vector, the same intuition BM25's IDF factor already
/// captures, just expressed as a vector weight instead of a score term.
pub fn query_vector(index: &Index, terms: &std::collections::HashSet<String>) -> Option<LexicalVector> {
    if terms.is_empty() {
        return None;
    }
    let total_docs = index.document_count().max(1) as f32;
    let text = terms.iter().cloned().collect::<Vec<_>>().join(" ");
    Some(embed_weighted(&text, |term| {
        let df = index
            .vocabulary
            .get(term)
            .and_then(|id| index.inverted.postings_for(id))
            .map(|list| list.document_frequency())
            .unwrap_or(1)
            .max(1) as f32;
        // Standard smoothed IDF: ln(N/df) + 1, floored at a small
        // positive weight so even a term appearing in every document
        // still contributes something rather than vanishing to zero.
        ((total_docs / df).ln() + 1.0).max(0.1)
    }))
}

/// Hashes `term` to a `(bucket_index, sign)` pair. The sign uses a
/// different bit of the same hash than the bucket index (the standard
/// "signed hashing trick" from Weinberger et al.'s feature hashing paper),
/// so that when two different terms collide into the same bucket, their
/// contributions partially cancel on average instead of always
/// compounding into an inflated weight.
fn hash_bucket(term: &str) -> (usize, f32) {
    let mut hasher = DefaultHasher::new();
    term.hash(&mut hasher);
    let h = hasher.finish();
    let index = (h % VECTOR_DIM as u64) as usize;
    let sign = if (h >> 32) & 1 == 0 { 1.0 } else { -1.0 };
    (index, sign)
}

fn normalize_l2(mut v: LexicalVector) -> LexicalVector {
    let norm: f32 = v.0.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-8 {
        for x in v.0.iter_mut() {
            *x /= norm;
        }
    }
    v
}

/// `DocId -> LexicalVector`, persisted alongside the rest of the index.
/// Vectors are computed once at index time (see
/// [`crate::index::Index::index_document`]) from the document's full
/// content, the same way [`crate::dedup::DuplicateIndex`] fingerprints
/// are, so no re-read of the source file/content-cache is needed at
/// search time.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct VectorIndex {
    vectors: HashMap<crate::document::DocId, LexicalVector>,
}

impl VectorIndex {
    /// Creates an empty vector index.
    pub fn new() -> Self {
        VectorIndex::default()
    }

    /// Stores (or replaces) the vector for `doc_id`.
    pub fn set(&mut self, doc_id: crate::document::DocId, vector: LexicalVector) {
        self.vectors.insert(doc_id, vector);
    }

    /// Looks up the vector for `doc_id`, if indexed.
    pub fn get(&self, doc_id: crate::document::DocId) -> Option<&LexicalVector> {
        self.vectors.get(&doc_id)
    }

    /// Removes the vector for `doc_id`, if present.
    pub fn remove(&mut self, doc_id: crate::document::DocId) {
        self.vectors.remove(&doc_id);
    }

    /// Number of vectors currently stored.
    pub fn len(&self) -> usize {
        self.vectors.len()
    }

    /// `true` if no vectors are stored.
    pub fn is_empty(&self) -> bool {
        self.vectors.is_empty()
    }
}

/// True neural sentence embeddings via `candle` (feature-gated, off by
/// default — see the module doc comment for why and what it needs).
#[cfg(feature = "neural_embeddings")]
pub mod neural;

/// One chunk's embedding plus enough identifying info to map a chunk-level
/// match back to a highlighted span in the parent document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkVector {
    pub chunk_index: u32,
    pub start_offset: u32,
    pub end_offset: u32,
    pub vector: LexicalVector,
}

/// `DocId -> [per-chunk vectors]`, the chunked counterpart to
/// [`VectorIndex`]. Kept as a separate structure (rather than folding
/// into `VectorIndex`) so documents short enough to not need chunking
/// pay no extra storage cost — most local files and short web pages will
/// only ever have a `VectorIndex` entry, never a `ChunkVectorIndex` one.
///
/// ## Ranking integration seam
/// A chunk-level match should surface as "the whole document ranked, with
/// the winning chunk's offsets available for snippet generation" rather
/// than chunks competing as separate search results — `best_chunk_for`
/// below is what a search-engine integration would call per candidate
/// document to get that chunk's cosine similarity and offsets, folding
/// the similarity into the same scoring path `vector_weight` already
/// uses in `crate::ranking`. That wiring (calling this from
/// `search::engine` alongside the existing whole-document
/// `VectorIndex::get` lookup) is not yet done in this pass — the storage,
/// chunking, and per-chunk scoring math here are complete and tested
/// independent of it.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ChunkVectorIndex {
    chunks: HashMap<crate::document::DocId, Vec<ChunkVector>>,
}

impl ChunkVectorIndex {
    pub fn new() -> Self {
        ChunkVectorIndex::default()
    }

    /// Chunks `text` and stores one TF vector per chunk for `doc_id`,
    /// replacing any previously stored chunks for that document.
    pub fn index_document(
        &mut self,
        doc_id: crate::document::DocId,
        text: &str,
        config: crate::document::chunking::ChunkConfig,
    ) {
        let chunks = crate::document::chunking::chunk_text(text, config);
        let vectors = chunks
            .into_iter()
            .map(|c| ChunkVector {
                chunk_index: c.index,
                start_offset: c.start_offset,
                end_offset: c.end_offset,
                vector: embed_tf(&c.text),
            })
            .collect();
        self.chunks.insert(doc_id, vectors);
    }

    /// Removes all stored chunk vectors for `doc_id`.
    pub fn remove(&mut self, doc_id: crate::document::DocId) {
        self.chunks.remove(&doc_id);
    }

    /// Returns the best-matching chunk (by cosine similarity to
    /// `query_vector`) for `doc_id`, along with its similarity score, or
    /// `None` if the document has no stored chunks (e.g. it was short
    /// enough that only the whole-document `VectorIndex` was used).
    pub fn best_chunk_for(
        &self,
        doc_id: crate::document::DocId,
        query_vector: &LexicalVector,
    ) -> Option<(&ChunkVector, f32)> {
        self.chunks.get(&doc_id)?.iter().map(|c| (c, c.vector.cosine_similarity(query_vector))).max_by(
            |a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal),
        )
    }

    /// Total number of documents that have stored chunk vectors.
    pub fn document_count(&self) -> usize {
        self.chunks.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedding_is_l2_normalized() {
        let v = embed_tf("the quick brown fox jumps over the lazy dog");
        let norm: f32 = v.0.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4 || norm == 0.0);
    }

    #[test]
    fn identical_text_has_cosine_similarity_one() {
        let a = embed_tf("rust systems programming language");
        let b = embed_tf("rust systems programming language");
        assert!((a.cosine_similarity(&b) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn similar_text_scores_higher_than_unrelated_text() {
        let query = embed_tf("rust ownership borrowing memory safety");
        let similar = embed_tf("rust borrowing rules and ownership semantics for memory safety");
        let unrelated = embed_tf("chocolate chip cookie recipe with brown sugar and vanilla");

        let sim_score = query.cosine_similarity(&similar);
        let unrelated_score = query.cosine_similarity(&unrelated);
        assert!(sim_score > unrelated_score);
    }

    #[test]
    fn empty_text_yields_zero_vector() {
        let v = embed_tf("");
        assert!(v.0.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn does_not_understand_synonyms_by_design() {
        // This test documents the honest limitation, not a bug: "car"
        // and "automobile" share zero vocabulary, so a purely lexical
        // hashing vector gives them no more similarity credit than any
        // other two unrelated words would get. A neural embedding model
        // would score these as similar; this module cannot and does not
        // claim to.
        let car = embed_tf("car");
        let automobile = embed_tf("automobile");
        // No assertion of "should be dissimilar" beyond documenting that
        // there's no special-cased synonym handling — just confirming
        // they don't accidentally hash to a suspiciously high similarity
        // by construction (they're different tokens, so unless a hash
        // collision coincidentally aligns them, similarity is ~0).
        let sim = car.cosine_similarity(&automobile);
        assert!(sim.abs() < 1.0);
    }

    #[test]
    fn vector_index_stores_and_removes() {
        let mut index = VectorIndex::new();
        index.set(1, embed_tf("hello world"));
        assert!(index.get(1).is_some());
        assert_eq!(index.len(), 1);
        index.remove(1);
        assert!(index.get(1).is_none());
        assert!(index.is_empty());
    }

    #[test]
    fn chunk_index_finds_the_best_matching_chunk() {
        let mut index = ChunkVectorIndex::new();
        let words_a: Vec<String> = (0..400).map(|i| format!("filler{i}")).collect();
        let mut text = words_a.join(" ");
        text.push_str(" rust ownership borrowing memory safety guarantees ");
        text.push_str(&(0..400).map(|i| format!("more_filler{i}")).collect::<Vec<_>>().join(" "));

        index.index_document(
            1,
            &text,
            crate::document::chunking::ChunkConfig {
                max_words: 300,
                overlap_words: 20,
            },
        );
        assert!(index.document_count() == 1);

        let query = embed_tf("rust ownership borrowing memory safety");
        let (best_chunk, similarity) = index.best_chunk_for(1, &query).expect("should find a chunk");
        assert!(similarity > 0.0);
        assert!(best_chunk.end_offset > best_chunk.start_offset);
    }

    #[test]
    fn chunk_index_returns_none_for_unindexed_doc() {
        let index = ChunkVectorIndex::new();
        let query = embed_tf("anything");
        assert!(index.best_chunk_for(999, &query).is_none());
    }
}
