//! Autocomplete: prefix suggestions drawn from the index vocabulary, plus
//! tracking of recent and popular searches so the CLI can surface useful
//! completions even before the user finishes typing.

mod trie;

use crate::error::{NexusError, Result};
use crate::index::vocabulary::Vocabulary;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use trie::Trie;

const MAX_RECENT_SEARCHES: usize = 50;

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
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| NexusError::io(parent, e))?;
        }
        let bytes = bincode::serialize(self).map_err(NexusError::Serialize)?;
        std::fs::write(path, bytes).map_err(|e| NexusError::io(path, e))
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
        let normalized = crate::text::normalize(prefix);
        self.trie.suggest(&normalized, limit)
    }
}
