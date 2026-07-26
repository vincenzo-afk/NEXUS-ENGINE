//! Spell correction: "did you mean" suggestions computed against the
//! index's vocabulary using Levenshtein distance.

mod levenshtein;

use crate::index::vocabulary::Vocabulary;
pub use levenshtein::levenshtein_distance;

/// A single spelling suggestion, with its distance from the queried term
/// (lower is closer) so callers can decide how confident to be.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    /// The suggested, correctly-spelled term.
    pub term: String,
    /// Edit distance from the original (misspelled) term.
    pub distance: u32,
}

/// Maximum edit distance considered when searching for suggestions. Terms
/// further than this from the query are not worth suggesting.
const MAX_SUGGESTION_DISTANCE: u32 = 2;

/// Returns up to `limit` spelling suggestions for `term`, drawn from
/// `vocabulary` and ordered by increasing edit distance (ties broken
/// alphabetically for determinism).
///
/// Returns an empty vector if `term` is already present in the vocabulary,
/// since no correction is needed.
pub fn suggest(term: &str, vocabulary: &Vocabulary, limit: usize) -> Vec<Suggestion> {
    if vocabulary.get(term).is_some() {
        return Vec::new();
    }

    let mut candidates: Vec<Suggestion> = vocabulary
        .iter()
        .filter_map(|(candidate, _)| {
            // Skip candidates whose length alone rules out being within the
            // max distance; a cheap pre-filter before the O(n*m) DP.
            let len_diff = (candidate.len() as i64 - term.len() as i64).unsigned_abs() as u32;
            if len_diff > MAX_SUGGESTION_DISTANCE {
                return None;
            }
            let distance = levenshtein_distance(term, candidate);
            if distance > 0 && distance <= MAX_SUGGESTION_DISTANCE {
                Some(Suggestion {
                    term: candidate.to_string(),
                    distance,
                })
            } else {
                None
            }
        })
        .collect();

    candidates.sort_by(|a, b| a.distance.cmp(&b.distance).then_with(|| a.term.cmp(&b.term)));
    candidates.truncate(limit);
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_close_terms() {
        let mut vocab = Vocabulary::new();
        vocab.get_or_insert("rust");
        vocab.get_or_insert("rest");
        vocab.get_or_insert("dust");
        vocab.get_or_insert("elephant");

        let suggestions = suggest("rusk", &vocab, 5);
        let terms: Vec<&str> = suggestions.iter().map(|s| s.term.as_str()).collect();
        assert!(terms.contains(&"rust"));
        assert!(!terms.contains(&"elephant"));
    }

    #[test]
    fn no_suggestions_for_known_term() {
        let mut vocab = Vocabulary::new();
        vocab.get_or_insert("rust");
        assert!(suggest("rust", &vocab, 5).is_empty());
    }
}
