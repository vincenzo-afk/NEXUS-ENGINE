//! Recursive filesystem crawler.
//!
//! Walks a directory tree, applying hidden-file, ignored-folder, and
//! extension filters, and yields the set of files that are eligible for
//! indexing. The actual reading/parsing of file contents happens later in
//! the pipeline (see [`crate::document`]).

use crate::config::Config;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use walkdir::{DirEntry, WalkDir};

/// A file discovered by the crawler, along with the metadata needed to
/// decide whether (and how) to index it.
#[derive(Debug, Clone)]
pub struct CrawledFile {
    /// Absolute path to the file.
    pub path: PathBuf,
    /// File size in bytes, as reported by the filesystem.
    pub size_bytes: u64,
    /// Last modification time.
    pub modified: std::time::SystemTime,
}

/// Filtering options that control which files the crawler yields.
#[derive(Debug, Clone)]
pub struct CrawlOptions {
    /// Skip dotfiles and dot-directories.
    pub ignore_hidden: bool,
    /// Directory *names* (not full paths) to skip entirely, e.g. `target`.
    pub ignored_folders: HashSet<String>,
    /// File extensions to skip (lowercase, no leading dot).
    pub ignored_extensions: HashSet<String>,
    /// Only files at most this large (in bytes) are yielded.
    pub max_file_size_bytes: u64,
    /// If non-empty, only files whose extension is in this set are yielded.
    /// An empty set means "no restriction" (all non-ignored extensions pass).
    pub include_extensions: HashSet<String>,
}

impl CrawlOptions {
    /// Builds crawl options from the global application configuration.
    pub fn from_config(config: &Config) -> Self {
        CrawlOptions {
            ignore_hidden: config.ignore_hidden,
            ignored_folders: config.ignored_folders.clone(),
            ignored_extensions: config.ignored_extensions.clone(),
            max_file_size_bytes: config.max_file_size_bytes,
            include_extensions: crate::document::SUPPORTED_EXTENSIONS
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

/// Returns `true` if a directory entry's file name starts with a dot.
fn is_hidden(entry: &DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .map(|s| s.starts_with('.'))
        .unwrap_or(false)
}

/// Recursively crawls `root`, returning every file that passes the supplied
/// [`CrawlOptions`] filters. Directories that fail the filters are pruned
/// entirely (not just their files), so ignoring `node_modules` avoids
/// descending into it at all.
pub fn crawl_folder(root: &Path, options: &CrawlOptions) -> Vec<CrawledFile> {
    let walker = WalkDir::new(root).into_iter().filter_entry(|entry| {
        // Always allow the root itself.
        if entry.depth() == 0 {
            return true;
        }
        if options.ignore_hidden && is_hidden(entry) {
            return false;
        }
        if entry.file_type().is_dir() {
            if let Some(name) = entry.file_name().to_str() {
                if options.ignored_folders.contains(name) {
                    return false;
                }
            }
        }
        true
    });

    let mut results = Vec::new();
    for entry in walker.filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();

        if options.ignored_extensions.contains(&ext) {
            continue;
        }
        if !options.include_extensions.is_empty() && !options.include_extensions.contains(&ext) {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        if metadata.len() > options.max_file_size_bytes {
            continue;
        }

        let modified = metadata.modified().unwrap_or(std::time::SystemTime::now());

        results.push(CrawledFile {
            path: path.to_path_buf(),
            size_bytes: metadata.len(),
            modified,
        });
    }

    results
}
