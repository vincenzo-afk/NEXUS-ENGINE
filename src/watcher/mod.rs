//! Real-time filesystem watching.
//!
//! Watches every configured indexed folder for create/modify/delete/rename
//! events and applies incremental updates to the index, so the search
//! results stay fresh without requiring a manual `rebuild`.

use crate::config::Config;
use crate::document::{is_supported, Document};
use crate::error::{NexusError, Result};
use crate::fs::CrawledFile;
use crate::index::Index;
use notify::{Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

/// Starts watching every folder in `config.indexed_folders`, returning a
/// live [`RecommendedWatcher`] (which must be kept alive for watching to
/// continue) and a channel of raw filesystem events.
pub fn start_watching(config: &Config) -> Result<(RecommendedWatcher, Receiver<Event>)> {
    let (tx, rx) = channel();

    let mut watcher = RecommendedWatcher::new(
        move |res: notify::Result<Event>| {
            if let Ok(event) = res {
                let _ = tx.send(event);
            }
        },
        NotifyConfig::default().with_poll_interval(Duration::from_secs(1)),
    )
    .map_err(|e| NexusError::Watcher(e.to_string()))?;

    for folder in &config.indexed_folders {
        watcher
            .watch(folder, RecursiveMode::Recursive)
            .map_err(|e| NexusError::Watcher(format!("failed to watch {}: {}", folder.display(), e)))?;
    }

    Ok((watcher, rx))
}

/// Applies a single filesystem event to the index, adding, updating, or
/// removing documents as appropriate. Returns a short human-readable
/// description of what changed, or `None` if the event required no action
/// (e.g. it referenced an ignored file type).
pub fn apply_event(index: &mut Index, config: &Config, event: &Event) -> Option<String> {
    match event.kind {
        EventKind::Create(_) | EventKind::Modify(_) => {
            let mut summary = None;
            for path in &event.paths {
                if let Some(s) = reindex_path(index, config, path) {
                    summary = Some(s);
                }
            }
            summary
        }
        EventKind::Remove(_) => {
            let mut summary = None;
            for path in &event.paths {
                if index.remove_by_path(path) {
                    summary = Some(format!("removed: {}", path.display()));
                }
            }
            summary
        }
        _ => None,
    }
}

/// Re-reads and re-indexes a single file, if it is one Nexus supports and
/// passes the configured ignore rules. Directories and unsupported files
/// are silently skipped.
fn reindex_path(index: &mut Index, config: &Config, path: &Path) -> Option<String> {
    if !path.is_file() || !is_supported(path) {
        return None;
    }
    if is_ignored(path, config) {
        return None;
    }

    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > config.max_file_size_bytes {
        return None;
    }

    let crawled = CrawledFile {
        path: path.to_path_buf(),
        size_bytes: metadata.len(),
        modified: metadata.modified().unwrap_or(std::time::SystemTime::now()),
    };

    let document = Document::from_crawled_file(&crawled).ok()?;
    index.index_document(document);
    Some(format!("indexed: {}", path.display()))
}

/// Returns `true` if `path` falls under a folder name in the ignore list or
/// is otherwise excluded by configuration (hidden files, ignored extensions).
fn is_ignored(path: &Path, config: &Config) -> bool {
    if config.ignore_hidden {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with('.') {
                return true;
            }
        }
    }
    for component in path.components() {
        if let Some(name) = component.as_os_str().to_str() {
            if config.ignored_folders.contains(name) {
                return true;
            }
        }
    }
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        if config.ignored_extensions.contains(&ext.to_lowercase()) {
            return true;
        }
    }
    false
}
