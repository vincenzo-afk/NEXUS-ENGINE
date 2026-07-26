//! The inverted index: term ID -> posting list.
//!
//! This is the core data structure that makes full-text search fast. Given
//! a term, it yields every document that contains it in O(1) average time,
//! with per-document term frequency and positions ready for ranking and
//! phrase matching.

use crate::document::DocId;
use crate::index::posting::PostingList;
use crate::index::vocabulary::TermId;
use crate::text;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Maps term IDs to their posting lists.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct InvertedIndex {
    postings: HashMap<TermId, PostingList>,
    /// Running total of tokens indexed across all documents, used for BM25's
    /// average-document-length normalization.
    pub total_token_count: u64,
    /// Number of documents currently indexed.
    pub document_count: u32,
}

impl InvertedIndex {
    /// Creates an empty inverted index.
    pub fn new() -> Self {
        InvertedIndex {
            postings: HashMap::new(),
            total_token_count: 0,
            document_count: 0,
        }
    }

    /// Indexes a pre-tokenized, stop-word-filtered stream of `(term_id,
    /// position)` pairs for a single document. Stop-word filtering happens
    /// in the caller so that this type stays agnostic of language rules.
    pub fn index_document(&mut self, doc_id: DocId, terms: &[(TermId, u32)]) {
        for &(term_id, position) in terms {
            self.postings
                .entry(term_id)
                .or_insert_with(PostingList::new)
                .add_occurrence(doc_id, position);
        }
        self.total_token_count += terms.len() as u64;
        self.document_count += 1;
    }

    /// Removes every posting for `doc_id` from every term's posting list.
    /// Used for incremental re-indexing (delete-then-reinsert) and file
    /// deletions.
    pub fn remove_document(&mut self, doc_id: DocId, removed_token_count: u32) {
        for list in self.postings.values_mut() {
            list.remove_document(doc_id);
        }
        self.total_token_count = self
            .total_token_count
            .saturating_sub(removed_token_count as u64);
        self.document_count = self.document_count.saturating_sub(1);
    }

    /// Looks up the posting list for a term ID.
    pub fn postings_for(&self, term_id: TermId) -> Option<&PostingList> {
        self.postings.get(&term_id)
    }

    /// Average document length in tokens, used by BM25. Returns 0.0 if the
    /// index is empty.
    pub fn average_document_length(&self) -> f32 {
        if self.document_count == 0 {
            0.0
        } else {
            self.total_token_count as f32 / self.document_count as f32
        }
    }

    /// Total number of distinct terms currently indexed.
    pub fn vocabulary_size(&self) -> usize {
        self.postings.len()
    }

    /// Total number of postings (term, document) pairs across the whole index.
    pub fn posting_count(&self) -> usize {
        self.postings.values().map(|l| l.postings.len()).sum()
    }
}

/// Tokenizes and normalizes `content`, filtering out stop words, and
/// returns `(term_text, position)` pairs ready for vocabulary lookup and
/// indexing. Shared by both document indexing and query parsing so the two
/// stay consistent.
pub fn analyze(content: &str) -> Vec<(String, u32)> {
    let normalized = text::normalize(content);
    text::tokenize(&normalized)
        .into_iter()
        .filter(|t| !text::is_stopword(&t.text))
        .map(|t| (t.text, t.position))
        .collect()
}
