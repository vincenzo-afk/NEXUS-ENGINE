//! Document store: `DocId -> DocumentMetadata`, plus the reverse path
//! lookup needed for incremental re-indexing and deletion.

use crate::document::{DocId, DocumentMetadata};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Stores metadata for every indexed document and provides fast lookups by
/// ID or by path.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DocumentStore {
    documents: HashMap<DocId, DocumentMetadata>,
    path_to_id: HashMap<PathBuf, DocId>,
    next_id: DocId,
}

impl DocumentStore {
    /// Creates an empty document store.
    pub fn new() -> Self {
        DocumentStore {
            documents: HashMap::new(),
            path_to_id: HashMap::new(),
            next_id: 0,
        }
    }

    /// Allocates a fresh document ID.
    pub fn allocate_id(&mut self) -> DocId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Inserts or replaces the metadata for `doc_id`.
    pub fn insert(&mut self, doc_id: DocId, metadata: DocumentMetadata) {
        self.path_to_id.insert(metadata.path.clone(), doc_id);
        self.documents.insert(doc_id, metadata);
    }

    /// Removes a document's metadata, returning it if present.
    pub fn remove(&mut self, doc_id: DocId) -> Option<DocumentMetadata> {
        if let Some(meta) = self.documents.remove(&doc_id) {
            self.path_to_id.remove(&meta.path);
            Some(meta)
        } else {
            None
        }
    }

    /// Looks up metadata by document ID.
    pub fn get(&self, doc_id: DocId) -> Option<&DocumentMetadata> {
        self.documents.get(&doc_id)
    }

    /// Looks up the document ID currently assigned to `path`, if any.
    pub fn id_for_path(&self, path: &Path) -> Option<DocId> {
        self.path_to_id.get(path).copied()
    }

    /// Number of documents currently stored.
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// Returns `true` if the store holds no documents.
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// Iterates over all `(DocId, &DocumentMetadata)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (DocId, &DocumentMetadata)> {
        self.documents.iter().map(|(&id, meta)| (id, meta))
    }
}
