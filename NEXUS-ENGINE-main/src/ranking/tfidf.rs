//! Classic TF-IDF scoring.
//!
//! Simpler and cheaper than BM25, kept as an alternative ranking mode for
//! cases (like autocomplete relevance ordering) where BM25's extra
//! parameters aren't needed.

/// Term frequency component: raw count of the term in the document.
pub fn term_frequency(count: u32) -> f32 {
    count as f32
}

/// Inverse document frequency: `ln(total_documents / document_frequency)`,
/// with a `+1` inside the log to avoid division by zero and negative
/// values when a term appears in every document.
pub fn inverse_document_frequency(document_frequency: f32, total_documents: f32) -> f32 {
    (total_documents / (document_frequency + 1.0)).ln() + 1.0
}

/// Combined TF-IDF score for a single term in a single document.
pub fn tfidf_score(term_count: u32, document_frequency: f32, total_documents: f32) -> f32 {
    term_frequency(term_count) * inverse_document_frequency(document_frequency, total_documents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn more_occurrences_increase_score() {
        let low = tfidf_score(1, 10.0, 1000.0);
        let high = tfidf_score(5, 10.0, 1000.0);
        assert!(high > low);
    }

    #[test]
    fn rarer_terms_score_higher() {
        let common = tfidf_score(1, 900.0, 1000.0);
        let rare = tfidf_score(1, 5.0, 1000.0);
        assert!(rare > common);
    }
}
