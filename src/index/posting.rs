//! Postings: the per-term, per-document occurrence records that make up the
//! inverted index.

use crate::document::DocId;
use serde::{Deserialize, Serialize};

/// A single term's occurrence record within one document: how many times
/// it appeared and at which token positions (used for phrase queries and
/// snippet generation).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Posting {
    /// The document this posting belongs to.
    pub doc_id: DocId,
    /// Number of times the term occurs in the document (term frequency).
    pub term_frequency: u32,
    /// Ordinal token positions at which the term occurs, sorted ascending.
    pub positions: Vec<u32>,
}

/// The list of all postings for a single term, kept sorted by `doc_id` to
/// allow efficient merge-style intersection/union during boolean query
/// evaluation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PostingList {
    /// Postings sorted by ascending `doc_id`.
    pub postings: Vec<Posting>,
}

impl PostingList {
    /// Creates an empty posting list.
    pub fn new() -> Self {
        PostingList {
            postings: Vec::new(),
        }
    }

    /// Inserts or updates the posting for `doc_id`, maintaining sort order.
    /// Because documents are indexed one at a time in increasing `doc_id`
    /// order during a full build, this is typically an O(1) append; the
    /// binary search keeps incremental re-indexing correct as well.
    pub fn add_occurrence(&mut self, doc_id: DocId, position: u32) {
        match self.postings.last_mut() {
            Some(p) if p.doc_id == doc_id => {
                p.term_frequency += 1;
                p.positions.push(position);
            }
            _ => {
                if let Ok(idx) = self.postings.binary_search_by_key(&doc_id, |p| p.doc_id) {
                    let p = &mut self.postings[idx];
                    p.term_frequency += 1;
                    p.positions.push(position);
                } else {
                    self.postings.push(Posting {
                        doc_id,
                        term_frequency: 1,
                        positions: vec![position],
                    });
                    self.postings.sort_by_key(|p| p.doc_id);
                }
            }
        }
    }

    /// Removes every posting belonging to `doc_id`. Used when a document is
    /// deleted or re-indexed.
    pub fn remove_document(&mut self, doc_id: DocId) {
        self.postings.retain(|p| p.doc_id != doc_id);
    }

    /// Returns the posting for a specific document, if the term occurs in it.
    pub fn get(&self, doc_id: DocId) -> Option<&Posting> {
        self.postings
            .binary_search_by_key(&doc_id, |p| p.doc_id)
            .ok()
            .map(|idx| &self.postings[idx])
    }

    /// Document frequency: number of distinct documents containing this term.
    pub fn document_frequency(&self) -> usize {
        self.postings.len()
    }
}
