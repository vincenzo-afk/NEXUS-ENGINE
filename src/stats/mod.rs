//! Index statistics: aggregate numbers about the current index, shown by
//! the `nexus stats` CLI command.

use crate::config::Config;
use crate::index::Index;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;

/// A snapshot of index-wide statistics.
#[derive(Debug, Serialize)]
pub struct IndexStats {
    /// Total number of indexed files.
    pub indexed_files: usize,
    /// Total number of configured (top-level) indexed folders.
    pub indexed_folders: usize,
    /// Number of distinct terms in the vocabulary.
    pub vocabulary_size: usize,
    /// Total number of (term, document) posting pairs across the index.
    pub posting_count: usize,
    /// Average document length in tokens.
    pub average_document_length: f32,
    /// Total size, in bytes, of every indexed document.
    pub total_content_bytes: u64,
    /// Size, in bytes, of the persisted index file on disk (0 if not yet saved).
    pub index_file_bytes: u64,
    /// Number of documents per file extension.
    pub files_by_extension: HashMap<String, usize>,
}

/// Computes statistics for the given index and configuration.
pub fn compute(index: &Index, config: &Config) -> IndexStats {
    let mut files_by_extension: HashMap<String, usize> = HashMap::new();
    let mut total_content_bytes = 0u64;

    for (_, meta) in index.store.iter() {
        *files_by_extension.entry(meta.extension.clone()).or_insert(0) += 1;
        total_content_bytes += meta.size_bytes;
    }

    let index_file_bytes = std::fs::metadata(&config.index_path)
        .map(|m| m.len())
        .unwrap_or(0);

    IndexStats {
        indexed_files: index.document_count(),
        indexed_folders: config.indexed_folders.len(),
        vocabulary_size: index.vocabulary.len(),
        posting_count: index.inverted.posting_count(),
        average_document_length: index.inverted.average_document_length(),
        total_content_bytes,
        index_file_bytes,
        files_by_extension,
    }
}

/// Formats stats as a human-readable multi-line report for CLI display.
pub fn format_report(stats: &IndexStats) -> String {
    let mut out = String::new();
    out.push_str("Nexus Index Statistics\n");
    out.push_str("=======================\n");
    out.push_str(&format!("Indexed files:        {}\n", stats.indexed_files));
    out.push_str(&format!("Indexed folders:      {}\n", stats.indexed_folders));
    out.push_str(&format!("Vocabulary size:      {}\n", stats.vocabulary_size));
    out.push_str(&format!("Posting count:        {}\n", stats.posting_count));
    out.push_str(&format!(
        "Avg. document length: {:.1} tokens\n",
        stats.average_document_length
    ));
    out.push_str(&format!(
        "Total content size:   {}\n",
        human_bytes(stats.total_content_bytes)
    ));
    out.push_str(&format!(
        "Index file size:      {}\n",
        human_bytes(stats.index_file_bytes)
    ));
    out.push_str("\nFiles by extension:\n");
    let mut exts: Vec<(&String, &usize)> = stats.files_by_extension.iter().collect();
    exts.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    for (ext, count) in exts {
        let label = if ext.is_empty() { "(none)" } else { ext.as_str() };
        out.push_str(&format!("  .{:<10} {}\n", label, count));
    }
    out
}

/// Checks whether `path` exists on disk before treating it as an indexed
/// folder root; used by the `list` CLI command to flag folders that have
/// since been deleted or moved.
pub fn folder_exists(path: &Path) -> bool {
    path.exists()
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit_idx = 0;
    while value >= 1024.0 && unit_idx < UNITS.len() - 1 {
        value /= 1024.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{} {}", bytes, UNITS[unit_idx])
    } else {
        format!("{:.2} {}", value, UNITS[unit_idx])
    }
}
