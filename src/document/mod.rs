//! Document model.
//!
//! A [`Document`] is the unit of indexing: one file on disk, its metadata,
//! and its extracted text content. This module also declares which file
//! extensions Nexus knows how to read as plain text.

use crate::error::{NexusError, Result};
use crate::fs::CrawledFile;
use log::debug;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// File extensions (lowercase, no dot) that Nexus treats as indexable plain
/// text. Adding a new type is a one-line change here.
pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    "txt", "md", "rs", "c", "cpp", "hpp", "h", "py", "java", "kt", "js", "ts", "tsx", "jsx",
    "html", "htm", "css", "json", "xml", "yaml", "yml", "toml", "csv", "log", "sh", "go", "rb",
];

/// A unique, stable identifier assigned to each indexed document.
pub type DocId = u32;

/// Metadata about an indexed document, independent of its text content.
/// This is what gets persisted in the [`crate::index::store::DocumentStore`]
/// and returned alongside search results.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocumentMetadata {
    /// Absolute path to the file on disk.
    pub path: PathBuf,
    /// File name only (no directory component), cached for fast filename search.
    pub file_name: String,
    /// Lowercase file extension, no leading dot.
    pub extension: String,
    /// File size in bytes at the time it was indexed.
    pub size_bytes: u64,
    /// Last-modified time at the time it was indexed, as seconds since UNIX epoch.
    pub modified_unix: i64,
    /// Total number of tokens extracted from the document.
    pub token_count: u32,
}

/// A document ready to be indexed: metadata plus raw text content.
#[derive(Debug, Clone)]
pub struct Document {
    /// Document metadata (path, size, timestamps, etc).
    pub metadata: DocumentMetadata,
    /// Full extracted text content of the file.
    pub content: String,
}

/// Converts a [`SystemTime`] into a UNIX timestamp, saturating at zero for
/// times before the epoch (which should not occur in practice).
fn to_unix_seconds(time: SystemTime) -> i64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Document {
    /// Reads a crawled file from disk and builds a [`Document`] from it.
    ///
    /// Content is read as UTF-8, with invalid byte sequences replaced
    /// (`String::from_utf8_lossy`), since real-world text files sometimes
    /// contain a handful of non-UTF8 bytes and we would rather index them
    /// approximately than skip them entirely.
    pub fn from_crawled_file(file: &CrawledFile) -> Result<Document> {
        let bytes = std::fs::read(&file.path).map_err(|e| NexusError::io(&file.path, e))?;
        let content = String::from_utf8_lossy(&bytes).into_owned();

        let file_name = file
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();

        let extension = file
            .path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();

        let metadata = DocumentMetadata {
            path: file.path.clone(),
            file_name,
            extension,
            size_bytes: file.size_bytes,
            modified_unix: to_unix_seconds(file.modified),
            token_count: 0, // filled in by the indexer once tokenized
        };

        debug!(
            "document created: {} ({} bytes)",
            file.path.display(),
            file.size_bytes
        );
        Ok(Document { metadata, content })
    }
}

/// Returns `true` if `path`'s extension is one Nexus knows how to index.
pub fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| SUPPORTED_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}
