//! BM25 ranking function.
//!
//! BM25 is the industry-standard probabilistic ranking function used by
//! Elasticsearch, Lucene, and most modern search engines. It improves on
//! raw TF-IDF by saturating term-frequency contribution (so a term
//! appearing 100 times isn't 100x as relevant as one appearing once) and
//! normalizing for document length.

use log::trace;

/// Computes the BM25 contribution of a single query term to a document's
/// score.
///
/// * `term_frequency` - occurrences of the term in this document.
/// * `document_frequency` - number of documents containing the term.
/// * `total_documents` - total number of documents in the index.
/// * `doc_length` - length of this document, in tokens.
/// * `avg_doc_length` - average document length across the index, in tokens.
/// * `k1` - term-frequency saturation parameter (typically 1.2-2.0).
/// * `b` - length-normalization parameter (0.0-1.0, typically 0.75).
pub fn bm25_term_score(
    term_frequency: f32,
    document_frequency: f32,
    total_documents: f32,
    doc_length: f32,
    avg_doc_length: f32,
    k1: f32,
    b: f32,
) -> f32 {
    if term_frequency <= 0.0 || document_frequency <= 0.0 {
        return 0.0;
    }

    let idf = idf(document_frequency, total_documents);
    let length_norm = 1.0 - b + b * (doc_length / avg_doc_length.max(1.0));
    let numerator = term_frequency * (k1 + 1.0);
    let denominator = term_frequency + k1 * length_norm;

    let score = idf * (numerator / denominator);
    trace!(
        "bm25_term: tf={} df={} total_docs={} doc_len={} avg_len={} k1={} b={} -> {}",
        term_frequency,
        document_frequency,
        total_documents,
        doc_length,
        avg_doc_length,
        k1,
        b,
        score
    );
    score
}

/// Robertson-Sparck-Jones inverse document frequency, as used by BM25.
/// Unlike classic TF-IDF's `ln(N/df)`, this variant is smoothed to avoid
/// negative scores for terms appearing in more than half the corpus.
pub fn idf(document_frequency: f32, total_documents: f32) -> f32 {
    ((total_documents - document_frequency + 0.5) / (document_frequency + 0.5) + 1.0).ln()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rarer_terms_score_higher() {
        let common = bm25_term_score(1.0, 900.0, 1000.0, 100.0, 100.0, 1.2, 0.75);
        let rare = bm25_term_score(1.0, 5.0, 1000.0, 100.0, 100.0, 1.2, 0.75);
        assert!(rare > common);
    }

    #[test]
    fn term_frequency_saturates() {
        let low_tf = bm25_term_score(1.0, 10.0, 1000.0, 100.0, 100.0, 1.2, 0.75);
        let high_tf = bm25_term_score(50.0, 10.0, 1000.0, 100.0, 100.0, 1.2, 0.75);
        // Score grows with term frequency but far sub-linearly.
        assert!(high_tf > low_tf);
        assert!(high_tf < low_tf * 10.0);
    }

    #[test]
    fn zero_frequency_yields_zero_score() {
        assert_eq!(
            bm25_term_score(0.0, 10.0, 1000.0, 100.0, 100.0, 1.2, 0.75),
            0.0
        );
    }
}
