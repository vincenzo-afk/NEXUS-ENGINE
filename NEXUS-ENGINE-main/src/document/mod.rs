//! Document model.
//!
//! A [`Document`] is the unit of indexing: one file on disk, its metadata,
//! and its extracted text content. This module also declares which file
//! extensions Nexus knows how to read as plain text.

pub mod chunking;

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
    "pdf", "docx", "xlsx", "pptx", "eml", "mbox", "zip", "sqlite", "sqlite3", "db", "jpg",
    "jpeg", "png", "tiff", "tif", "webp", "heic",
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
    /// Who's allowed to see this document in search results. Derived
    /// from real filesystem ownership/permissions for local files (see
    /// [`acl_for_local_path`]) and always [`crate::entity::Acl::public`]
    /// for web-crawled content (a crawled public page has no
    /// meaningful owner). `#[serde(default)]` makes deserializing an
    /// index built before this field existed produce `Acl::public()`
    /// for every document — preserving that index's previous
    /// (unfiltered) behavior exactly rather than retroactively hiding
    /// everything in it.
    #[serde(default = "crate::entity::Acl::public")]
    pub acl: crate::entity::Acl,
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

/// Derives a local file's [`crate::entity::Acl`] from its *real*
/// filesystem owner and permission bits, so search permission filtering
/// mirrors whatever access control the filesystem already enforces
/// (per the design this module's `crate::entity` doc comment describes)
/// instead of inventing a parallel, disconnected-from-reality permission
/// system.
///
/// The rule (Unix only): if the file's "other" read bit is set, it's
/// treated as [`crate::entity::Acl::public`] — anyone with a shell on
/// this machine could already `cat` it, so hiding it from search while
/// leaving it world-readable on disk wouldn't protect anything, just
/// make search less useful. Otherwise it's `Acl::owned_by("uid:<n>")`
/// (matched against [`crate::entity::current_os_principal`]), visible
/// only to a search context running as that same UID.
///
/// On non-Unix platforms, or if the metadata read fails (e.g. the file
/// vanished between being crawled and this call — a real race on a live
/// filesystem), this falls back to `Acl::public()`: the same "no
/// filtering" behavior every document had before this existed, rather
/// than silently hiding documents Nexus has no permission signal for.
fn acl_for_local_path(path: &Path) -> crate::entity::Acl {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(meta) = std::fs::metadata(path) {
            const OTHER_READ_BIT: u32 = 0o004;
            if meta.mode() & OTHER_READ_BIT != 0 {
                return crate::entity::Acl::public();
            }
            return crate::entity::Acl::owned_by(format!("uid:{}", meta.uid()));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    crate::entity::Acl::public()
}

impl Document {
    /// Reads a crawled file from disk and builds a [`Document`] from it,
    /// extracting clean indexable text according to its format rather
    /// than always indexing raw file bytes.
    ///
    /// PDFs are parsed for their text layer, HTML has its tags/scripts/
    /// styles stripped (via the same extractor the web crawler uses),
    /// and Markdown/JSON/XML have their syntax stripped down to the
    /// meaningful text. Everything else (source code, plain text,
    /// config files, ...) is indexed as UTF-8 text, with invalid byte
    /// sequences replaced (`String::from_utf8_lossy`), since real-world
    /// text files sometimes contain a handful of non-UTF8 bytes and we
    /// would rather index them approximately than skip them entirely.
    pub fn from_crawled_file(file: &CrawledFile) -> Result<Document> {
        let extension = file
            .path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();

        let format = crate::formats::DocumentFormat::from_extension(&extension);
        let content = match format {
            crate::formats::DocumentFormat::Pdf => {
                let bytes = std::fs::read(&file.path).map_err(|e| NexusError::io(&file.path, e))?;
                crate::formats::extract_pdf(&bytes)
            }
            crate::formats::DocumentFormat::Html => {
                let bytes = std::fs::read(&file.path).map_err(|e| NexusError::io(&file.path, e))?;
                crate::html::extract(&String::from_utf8_lossy(&bytes)).indexable_text()
            }
            crate::formats::DocumentFormat::Markdown => {
                let bytes = std::fs::read(&file.path).map_err(|e| NexusError::io(&file.path, e))?;
                crate::formats::extract_markdown(&String::from_utf8_lossy(&bytes))
            }
            crate::formats::DocumentFormat::Json => {
                let bytes = std::fs::read(&file.path).map_err(|e| NexusError::io(&file.path, e))?;
                crate::formats::extract_json(&String::from_utf8_lossy(&bytes))
            }
            crate::formats::DocumentFormat::Xml => {
                let bytes = std::fs::read(&file.path).map_err(|e| NexusError::io(&file.path, e))?;
                crate::formats::extract_xml(&String::from_utf8_lossy(&bytes))
            }
            crate::formats::DocumentFormat::PlainText => {
                let bytes = std::fs::read(&file.path).map_err(|e| NexusError::io(&file.path, e))?;
                String::from_utf8_lossy(&bytes).into_owned()
            }
            crate::formats::DocumentFormat::Docx => {
                let bytes = std::fs::read(&file.path).map_err(|e| NexusError::io(&file.path, e))?;
                crate::extract::office::extract_docx(&bytes).text
            }
            crate::formats::DocumentFormat::Xlsx => {
                let bytes = std::fs::read(&file.path).map_err(|e| NexusError::io(&file.path, e))?;
                crate::extract::office::extract_xlsx(&bytes).text
            }
            crate::formats::DocumentFormat::Pptx => {
                let bytes = std::fs::read(&file.path).map_err(|e| NexusError::io(&file.path, e))?;
                crate::extract::office::extract_pptx(&bytes).text
            }
            crate::formats::DocumentFormat::Eml => {
                let bytes = std::fs::read(&file.path).map_err(|e| NexusError::io(&file.path, e))?;
                crate::extract::email::parse_eml(&bytes)
                    .map(|e| e.indexable_text())
                    .unwrap_or_default()
            }
            crate::formats::DocumentFormat::Mbox => {
                let bytes = std::fs::read(&file.path).map_err(|e| NexusError::io(&file.path, e))?;
                crate::extract::email::parse_mbox(&bytes)
                    .iter()
                    .map(|e| e.indexable_text())
                    .collect::<Vec<_>>()
                    .join("\n\n---\n\n")
            }
            crate::formats::DocumentFormat::Zip => {
                let bytes = std::fs::read(&file.path).map_err(|e| NexusError::io(&file.path, e))?;
                crate::extract::archive::extract_zip(&bytes).text
            }
            crate::formats::DocumentFormat::SqliteDb => {
                crate::extract::sqlite_notes::extract_text(&file.path).text
            }
            crate::formats::DocumentFormat::Image => {
                crate::extract::image_ocr::extract_image_text(&file.path).text
            }
        };

        let file_name = file
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();

        let metadata = DocumentMetadata {
            path: file.path.clone(),
            file_name,
            extension,
            size_bytes: file.size_bytes,
            modified_unix: to_unix_seconds(file.modified),
            token_count: 0, // filled in by the indexer once tokenized
            acl: acl_for_local_path(&file.path),
        };

        debug!(
            "document created: {} ({} bytes, format={:?})",
            file.path.display(),
            file.size_bytes,
            format
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp_file(name: &str, content: &str) -> CrawledFile {
        let dir = std::env::temp_dir().join(format!(
            "nexus-doc-test-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        CrawledFile {
            path,
            size_bytes: content.len() as u64,
            modified: SystemTime::now(),
        }
    }

    #[test]
    fn html_files_get_tags_stripped_not_indexed_as_raw_markup() {
        let file = write_temp_file(
            "page.html",
            "<html><head><script>evil()</script></head><body><h1>Real Title</h1><p>This is real content here.</p></body></html>",
        );
        let doc = Document::from_crawled_file(&file).unwrap();
        assert!(doc.content.contains("Real Title"));
        assert!(doc.content.contains("real content"));
        assert!(!doc.content.contains("evil()"));
        assert!(!doc.content.contains("<h1>"));
        std::fs::remove_dir_all(file.path.parent().unwrap()).ok();
    }

    #[test]
    fn markdown_files_get_syntax_stripped() {
        let file = write_temp_file("notes.md", "# Heading\n\nSome **bold** text.");
        let doc = Document::from_crawled_file(&file).unwrap();
        assert!(doc.content.contains("Heading"));
        assert!(doc.content.contains("bold"));
        assert!(!doc.content.contains('#'));
        assert!(!doc.content.contains("**"));
        std::fs::remove_dir_all(file.path.parent().unwrap()).ok();
    }

    #[test]
    fn plain_text_files_are_indexed_verbatim() {
        let file = write_temp_file("notes.txt", "plain text content, nothing special");
        let doc = Document::from_crawled_file(&file).unwrap();
        assert_eq!(doc.content, "plain text content, nothing special");
        std::fs::remove_dir_all(file.path.parent().unwrap()).ok();
    }

    #[test]
    fn pdf_extension_is_now_supported() {
        assert!(SUPPORTED_EXTENSIONS.contains(&"pdf"));
    }

    #[cfg(unix)]
    #[test]
    fn world_readable_local_file_gets_a_public_acl() {
        let file = write_temp_file("public.txt", "anyone can read this");
        std::fs::set_permissions(&file.path, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
        let doc = Document::from_crawled_file(&file).unwrap();
        assert_eq!(doc.metadata.acl, crate::entity::Acl::public());
        std::fs::remove_dir_all(file.path.parent().unwrap()).ok();
    }

    #[cfg(unix)]
    #[test]
    fn non_world_readable_local_file_is_owned_by_its_uid() {
        let file = write_temp_file("private.txt", "only the owner can read this");
        std::fs::set_permissions(&file.path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
        let doc = Document::from_crawled_file(&file).unwrap();

        let expected_owner = format!("uid:{}", unsafe { libc::getuid() });
        assert_eq!(doc.metadata.acl.owner.as_deref(), Some(expected_owner.as_str()));
        assert!(!doc.metadata.acl.public);
        std::fs::remove_dir_all(file.path.parent().unwrap()).ok();
    }
}
