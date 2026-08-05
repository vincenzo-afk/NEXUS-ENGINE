//! Click history: a per-document count of how often search results have
//! been chosen, used as a ranking signal (documents people actually click
//! tend to be more relevant than the raw text-match score alone would
//! suggest). This is what a browser extension or a web UI's "record which
//! result the user opened" hook would feed in production; the CLI exposes
//! it via `nexus click <query> <result-number>` so the signal is real and
//! exercised end-to-end rather than a dead code path.

use crate::document::DocId;
use crate::error::{NexusError, Result};
use log::debug;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

pub mod last_shown;
pub use last_shown::LastShownResults;

/// Persisted click counts, `DocId -> total clicks across all queries`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ClickLog {
    counts: HashMap<DocId, u32>,
}

impl ClickLog {
    /// Loads the click log from `path`, or an empty log if none exists yet.
    pub fn load(path: &Path) -> Result<ClickLog> {
        debug!("loading click log from: {}", path.display());
        if !path.exists() {
            return Ok(ClickLog::default());
        }
        let bytes = std::fs::read(path).map_err(|e| NexusError::io(path, e))?;
        bincode::deserialize(&bytes).map_err(NexusError::Deserialize)
    }

    /// Saves the click log to `path`.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| NexusError::io(parent, e))?;
        }
        let bytes = bincode::serialize(self).map_err(NexusError::Serialize)?;
        std::fs::write(path, bytes).map_err(|e| NexusError::io(path, e))
    }

    /// Records one click on `doc_id`.
    pub fn record(&mut self, doc_id: DocId) {
        debug!("click recorded: doc_id={}", doc_id);
        *self.counts.entry(doc_id).or_insert(0) += 1;
    }

    /// Total recorded clicks for `doc_id`.
    pub fn clicks_for(&self, doc_id: DocId) -> u32 {
        self.counts.get(&doc_id).copied().unwrap_or(0)
    }

    /// Every recorded `(doc_id, click_count)` pair. Used by
    /// `crate::ranking::adaptive` to work out which kinds of sources
    /// (local file / web page / PDF / email / ...) a person tends to
    /// click, without `ClickLog` itself needing to know anything about
    /// source kinds.
    pub fn all(&self) -> impl Iterator<Item = (DocId, u32)> + '_ {
        self.counts.iter().map(|(&id, &count)| (id, count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_reads_clicks() {
        let mut log = ClickLog::default();
        log.record(7);
        log.record(7);
        log.record(3);
        assert_eq!(log.clicks_for(7), 2);
        assert_eq!(log.clicks_for(3), 1);
        assert_eq!(log.clicks_for(99), 0);
    }

    #[test]
    fn round_trips_through_disk() {
        let mut log = ClickLog::default();
        log.record(1);
        let dir = std::env::temp_dir().join(format!("nexus-clicks-test-{}", std::process::id()));
        let path = dir.join("clicks.nxc");
        log.save(&path).unwrap();
        let loaded = ClickLog::load(&path).unwrap();
        assert_eq!(loaded.clicks_for(1), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
