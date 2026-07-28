//! Vocabulary: the bidirectional mapping between terms (strings) and the
//! compact integer term IDs used everywhere else in the index for speed
//! and memory efficiency.

use log::trace;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A unique identifier assigned to each distinct term in the vocabulary.
pub type TermId = u32;

/// Bidirectional term <-> ID mapping.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Vocabulary {
    term_to_id: HashMap<String, TermId>,
    id_to_term: Vec<String>,
}

impl Vocabulary {
    /// Creates an empty vocabulary.
    pub fn new() -> Self {
        Vocabulary {
            term_to_id: HashMap::new(),
            id_to_term: Vec::new(),
        }
    }

    /// Returns the ID for `term`, allocating a new one if it has not been
    /// seen before.
    pub fn get_or_insert(&mut self, term: &str) -> TermId {
        if let Some(&id) = self.term_to_id.get(term) {
            return id;
        }
        let id = self.id_to_term.len() as TermId;
        trace!("new term \"{}\" -> id={}", term, id);
        self.id_to_term.push(term.to_string());
        self.term_to_id.insert(term.to_string(), id);
        id
    }

    /// Looks up the ID for `term` without inserting it.
    pub fn get(&self, term: &str) -> Option<TermId> {
        self.term_to_id.get(term).copied()
    }

    /// Resolves a term ID back to its string form.
    pub fn term_for_id(&self, id: TermId) -> Option<&str> {
        self.id_to_term.get(id as usize).map(|s| s.as_str())
    }

    /// Number of distinct terms in the vocabulary.
    pub fn len(&self) -> usize {
        self.id_to_term.len()
    }

    /// Returns `true` if the vocabulary contains no terms.
    pub fn is_empty(&self) -> bool {
        self.id_to_term.is_empty()
    }

    /// Iterates over every term currently in the vocabulary, along with its ID.
    /// Used by autocomplete and spell-check to build auxiliary structures.
    pub fn iter(&self) -> impl Iterator<Item = (&str, TermId)> {
        self.term_to_id.iter().map(|(k, &v)| (k.as_str(), v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuses_ids_for_repeated_terms() {
        let mut vocab = Vocabulary::new();
        let a = vocab.get_or_insert("rust");
        let b = vocab.get_or_insert("parser");
        let c = vocab.get_or_insert("rust");
        assert_eq!(a, c);
        assert_ne!(a, b);
        assert_eq!(vocab.term_for_id(a), Some("rust"));
    }
}
