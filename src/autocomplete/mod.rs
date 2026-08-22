//! Autocomplete: prefix suggestions drawn from the index vocabulary, plus
//! tracking of recent and popular searches so the CLI can surface useful
//! completions even before the user finishes typing.

mod trie;

use crate::error::{NexusError, Result};
use crate::index::vocabulary::Vocabulary;
use log::debug;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use trie::Trie;

const MAX_RECENT_SEARCHES: usize = 50;
static HISTORY_SAVE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Persisted search history: recent queries (most-recent-first) and a
/// frequency count of every query ever run, used to rank "popular"
/// searches.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SearchHistory {
    recent: Vec<String>,
    popularity: HashMap<String, u32>,
}

impl SearchHistory {
    /// Loads search history from `path`, returning an empty history if the
    /// file does not exist yet.
    pub fn load(path: &Path) -> Result<SearchHistory> {
        if !path.exists() {
            return Ok(SearchHistory::default());
        }
        let bytes = std::fs::read(path).map_err(|e| NexusError::io(path, e))?;
        bincode::deserialize(&bytes).map_err(NexusError::Deserialize)
    }

    /// Saves search history to `path`.
    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).map_err(|e| NexusError::io(parent, e))?;
        let bytes = bincode::serialize(self).map_err(NexusError::Serialize)?;
        let file_name = path.file_name().ok_or_else(|| {
            NexusError::Other(format!("search-history path has no file name: {}", path.display()))
        })?;
        let (temp_path, mut file) = loop {
            let suffix = HISTORY_SAVE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let candidate = parent.join(format!(
                ".{}.{}.{}.tmp",
                file_name.to_string_lossy(),
                std::process::id(),
                suffix
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(file) => break (candidate, file),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(NexusError::io(candidate, error)),
            }
        };
        if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
            drop(file);
            let _ = std::fs::remove_file(&temp_path);
            return Err(NexusError::io(&temp_path, error));
        }
        drop(file);
        if let Err(error) = std::fs::rename(&temp_path, path) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(NexusError::io(path, error));
        }
        Ok(())
    }

    /// Records that `query` was searched, updating both recency and
    /// popularity tracking.
    pub fn record(&mut self, query: &str) {
        self.recent.retain(|q| q != query);
        self.recent.insert(0, query.to_string());
        self.recent.truncate(MAX_RECENT_SEARCHES);
        *self.popularity.entry(query.to_string()).or_insert(0) += 1;
    }

    /// Returns the `limit` most recent distinct searches, most recent first.
    pub fn recent(&self, limit: usize) -> Vec<String> {
        self.recent.iter().take(limit).cloned().collect()
    }

    /// Returns the `limit` most popular searches, most popular first.
    pub fn popular(&self, limit: usize) -> Vec<String> {
        let mut entries: Vec<(&String, &u32)> = self.popularity.iter().collect();
        entries.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        entries
            .into_iter()
            .take(limit)
            .map(|(q, _)| q.clone())
            .collect()
    }
}

/// Provides prefix-based term suggestions backed by the index vocabulary.
pub struct Autocomplete {
    trie: Trie,
}

impl Autocomplete {
    /// Builds an autocomplete index from every term currently in the
    /// vocabulary. Terms are weighted by how many documents contain them
    /// (a cheap proxy for how "common" / useful a completion is).
    pub fn build(vocabulary: &Vocabulary, document_frequencies: &HashMap<String, u32>) -> Self {
        debug!(
            "building autocomplete trie from {} vocabulary terms",
            vocabulary.len()
        );
        let mut trie = Trie::new();
        for (term, _) in vocabulary.iter() {
            let weight = document_frequencies.get(term).copied().unwrap_or(1);
            for _ in 0..weight.max(1) {
                trie.insert(term);
            }
        }
        Autocomplete { trie }
    }

    /// Returns up to `limit` vocabulary terms starting with `prefix`.
    pub fn suggest(&self, prefix: &str, limit: usize) -> Vec<String> {
        debug!("autocomplete suggest: prefix='{}', limit={}", prefix, limit);
        let normalized = crate::text::normalize(prefix);
        let results = self.trie.suggest(&normalized, limit);
        debug!(
            "autocomplete: {} suggestions for '{}'",
            results.len(),
            prefix
        );
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_save_replaces_existing_data_without_temp_files() {
        let dir = std::env::temp_dir().join(format!(
            "nexus-history-atomic-test-{}-{}",
            std::process::id(),
            HISTORY_SAVE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let path = dir.join("history.nxh");
        let mut first = SearchHistory::default();
        first.record("first");
        first.save(&path).unwrap();

        let mut replacement = SearchHistory::default();
        replacement.record("rust search");
        replacement.record("rust search");
        replacement.save(&path).unwrap();

        let loaded = SearchHistory::load(&path).unwrap();
        assert_eq!(loaded.recent(5), vec!["rust search"]);
        assert_eq!(loaded.popular(5), vec!["rust search"]);
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("history.nxh")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
