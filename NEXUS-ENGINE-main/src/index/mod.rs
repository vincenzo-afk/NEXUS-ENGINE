//! The Nexus index: the combination of a [`Vocabulary`], an
//! [`InvertedIndex`], and a [`DocumentStore`] that together answer "which
//! documents contain this term" and "what do I know about this document".

pub mod inverted;
pub mod posting;
pub mod store;
pub mod vocabulary;

use crate::dedup::DuplicateIndex;
use crate::document::{DocId, Document};
use crate::index::inverted::InvertedIndex;
use crate::index::store::DocumentStore;
use crate::index::vocabulary::{TermId, Vocabulary};
use crate::vector::VectorIndex;
use crate::webdoc::WebMetaStore;
use log::debug;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The current on-disk format version. Bumped whenever the serialized
/// layout of [`Index`] changes in an incompatible way.
///
/// v2 added `web`, the per-page crawl/link-graph metadata store, and
/// `duplicates`, the near-duplicate detection index, to support web
/// crawling. v3 added `vectors`, the per-document lexical hashing-trick
/// vector used for hybrid BM25 + vector re-ranking. Index files written
/// by earlier builds are not forward compatible; run `nexus rebuild`
/// after upgrading.
pub const INDEX_FORMAT_VERSION: u32 = 3;

/// The complete, persistable search index.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Index {
    /// Term <-> ID mapping.
    pub vocabulary: Vocabulary,
    /// Term ID -> posting list mapping.
    pub inverted: InvertedIndex,
    /// Document ID -> metadata mapping.
    pub store: DocumentStore,
    /// Crawl and link-graph metadata for documents sourced from the web
    /// (empty/unused for purely filesystem-indexed documents).
    #[serde(default)]
    pub web: WebMetaStore,
    /// Near-duplicate / exact-duplicate detection index, consulted while
    /// crawling so mirrors and boilerplate-only-differs pages don't get
    /// indexed twice.
    #[serde(default)]
    pub duplicates: DuplicateIndex,
    /// Per-document lexical hashing-trick vectors, used to complement
    /// BM25 with a second, differently-shaped similarity signal (see
    /// `crate::vector`'s module doc comment for exactly what this is —
    /// and isn't).
    #[serde(default)]
    pub vectors: VectorIndex,
    /// Per-chunk vectors for documents long enough that a single
    /// whole-document vector would blur together unrelated sections
    /// (see `crate::vector::ChunkVectorIndex`'s doc comment). Only
    /// populated for documents over `ChunkConfig::default().max_words`;
    /// most documents never get an entry here, only `vectors` above.
    #[serde(default)]
    pub chunk_vectors: crate::vector::ChunkVectorIndex,
    /// Entity mentions extracted from every indexed document's content,
    /// linked by co-occurrence — see `crate::graph`'s module doc comment
    /// for exactly what "entity extraction" means here (rule-based, not
    /// a trained NER model). Populated incrementally at index time by
    /// `index_document`, the same as `vectors`/`chunk_vectors` above.
    #[serde(default)]
    pub graph: crate::graph::GraphBuilder,
}

impl Index {
    /// Creates a new, empty index.
    pub fn new() -> Self {
        Index {
            vocabulary: Vocabulary::new(),
            inverted: InvertedIndex::new(),
            store: DocumentStore::new(),
            web: WebMetaStore::new(),
            duplicates: DuplicateIndex::new(),
            vectors: VectorIndex::new(),
            chunk_vectors: crate::vector::ChunkVectorIndex::new(),
            graph: crate::graph::GraphBuilder::new(),
        }
    }

    /// Indexes a single document: analyzes its content, updates the
    /// vocabulary, updates the inverted index, and records its metadata.
    /// If the document's path is already indexed, the previous version is
    /// removed first so this method is safe to call for both initial
    /// indexing and incremental re-indexing.
    pub fn index_document(&mut self, mut document: Document) -> DocId {
        if let Some(existing_id) = self.store.id_for_path(&document.metadata.path) {
            self.remove_document(existing_id);
        }

        let analyzed = inverted::analyze(&document.content);
        document.metadata.token_count = analyzed.len() as u32;

        let doc_id = self.store.allocate_id();
        let term_pairs: Vec<(TermId, u32)> = analyzed
            .into_iter()
            .map(|(term, pos)| (self.vocabulary.get_or_insert(&term), pos))
            .collect();

        self.inverted.index_document(doc_id, &term_pairs);
        self.duplicates.register(doc_id, &document.content);
        self.vectors.set(doc_id, crate::vector::embed_tf(&document.content));

        // Only chunk documents long enough that a single whole-document
        // vector would blur distinct sections together — see
        // `ChunkVectorIndex`'s doc comment. `token_count` was just
        // computed above from the same analysis pass, so this is a free
        // reuse rather than an extra text scan.
        let chunk_config = crate::document::chunking::ChunkConfig::default();
        if (document.metadata.token_count as usize) > chunk_config.max_words {
            self.chunk_vectors.index_document(doc_id, &document.content, chunk_config);
        } else {
            // Cheap no-op for first-time indexing; matters for
            // re-indexing a document that shrank below the threshold
            // (edited down) so it doesn't keep a stale chunk entry.
            self.chunk_vectors.remove(doc_id);
        }

        // Entity extraction: see `crate::graph`'s module doc comment for
        // what this does and doesn't do (rule-based, not a trained NER
        // model). `remove_document` (called above via the
        // already-indexed-path check) retracts this source's prior
        // mentions/co-occurrence contributions from `graph` before a new
        // `doc_id` is allocated, so re-indexing an edited document no
        // longer accumulates stale entity mentions from earlier versions
        // of its own content.
        self.graph.ingest(&doc_id.to_string(), &document.content, document.metadata.modified_unix);

        self.store.insert(doc_id, document.metadata);
        debug!(
            "indexed document {} -> {:?}",
            doc_id,
            self.store.get(doc_id).map(|m| &m.path)
        );
        doc_id
    }

    /// Removes a document (by ID) from the inverted index and document
    /// store. Vocabulary entries are intentionally left in place: removing
    /// terms would require a full posting-list scan to know if they are
    /// still referenced elsewhere, which is unnecessary overhead for a
    /// single deletion. A `rebuild` compacts the vocabulary if desired.
    pub fn remove_document(&mut self, doc_id: DocId) -> bool {
        if let Some(meta) = self.store.remove(doc_id) {
            debug!(
                "removing document {} ({:?})",
                doc_id, meta.path
            );
            self.inverted.remove_document(doc_id, meta.token_count);
            self.web.remove(doc_id);
            self.duplicates.remove(doc_id);
            self.vectors.remove(doc_id);
            self.chunk_vectors.remove(doc_id);
            self.graph.remove_source(&doc_id.to_string());
            true
        } else {
            false
        }
    }

    /// Removes whatever document currently lives at `path`, if any.
    /// Returns `true` if a document was found and removed.
    pub fn remove_by_path(&mut self, path: &Path) -> bool {
        if let Some(id) = self.store.id_for_path(path) {
            debug!("removing by path {:?} (doc_id {})", path, id);
            self.remove_document(id)
        } else {
            false
        }
    }

    /// Total number of documents currently indexed.
    pub fn document_count(&self) -> usize {
        self.store.len()
    }
}
