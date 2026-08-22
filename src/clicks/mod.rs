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
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static CLICK_SAVE_COUNTER: AtomicU64 = AtomicU64::new(0);

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
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).map_err(|e| NexusError::io(parent, e))?;
        let bytes = bincode::serialize(self).map_err(NexusError::Serialize)?;
        let file_name = path.file_name().ok_or_else(|| {
            NexusError::Other(format!("click-log path has no file name: {}", path.display()))
        })?;
        let (temp_path, mut file) = loop {
            let suffix = CLICK_SAVE_COUNTER.fetch_add(1, Ordering::Relaxed);
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

    #[test]
    fn save_replaces_existing_log_without_leaving_temp_files() {
        let dir = std::env::temp_dir().join(format!(
            "nexus-clicks-atomic-test-{}-{}",
            std::process::id(),
            CLICK_SAVE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let path = dir.join("clicks.nxc");
        let mut first = ClickLog::default();
        first.record(1);
        first.save(&path).unwrap();

        let mut replacement = ClickLog::default();
        replacement.record(7);
        replacement.record(7);
        replacement.save(&path).unwrap();

        let loaded = ClickLog::load(&path).unwrap();
        assert_eq!(loaded.clicks_for(1), 0);
        assert_eq!(loaded.clicks_for(7), 2);
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("clicks.nxc")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
