//! Stop-word filtering.
//!
//! Common English function words carry little discriminative power in
//! full-text search and are excluded from the inverted index to keep the
//! vocabulary and posting lists smaller. They are *not* removed from the
//! token stream used for phrase-position tracking; filtering happens at
//! the point of indexing (see [`crate::index::inverted::InvertedIndex`]).

use std::collections::HashSet;
use std::sync::OnceLock;

/// The default English stop-word list.
const STOPWORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "if", "then", "else", "of", "to", "in", "on", "at",
    "for", "with", "by", "from", "up", "down", "is", "are", "was", "were", "be", "been", "being",
    "this", "that", "these", "those", "it", "its", "as", "into", "than", "so", "such", "no",
    "nor", "not", "only", "own", "same", "too", "very", "can", "will", "just", "do", "does",
    "did", "doing", "have", "has", "had", "having", "i", "you", "he", "she", "we", "they", "them",
    "his", "her", "their", "our", "my", "your",
];

static STOPWORD_SET: OnceLock<HashSet<&'static str>> = OnceLock::new();

/// Returns `true` if `word` is a stop word. `word` is expected to already be
/// normalized (lowercase).
pub fn is_stopword(word: &str) -> bool {
    STOPWORD_SET
        .get_or_init(|| STOPWORDS.iter().copied().collect())
        .contains(word)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_common_stopwords() {
        assert!(is_stopword("the"));
        assert!(is_stopword("and"));
        assert!(!is_stopword("rust"));
    }
}
