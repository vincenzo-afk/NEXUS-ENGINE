//! Persists exactly which document `nexus search` showed at each
//! absolute rank, so a later `nexus click <query> <rank>` can resolve
//! "rank N" to a document ID directly instead of re-running the search
//! and assuming nothing changed in between.
//!
//! ## The race this fixes
//! `nexus click` used to resolve `rank` -> document purely by re-running
//! the same query through `search::search` and reading `results[rank -
//! 1]`. That's usually right (search is deterministic for a fixed index
//! and click history), but "usually" isn't "always": if the click log or
//! index changed between the original `nexus search` and this
//! `nexus click` call — most commonly, another click landed in between
//! and shifted the personalization boost — the recomputed ranking can
//! legitimately differ from what the user actually saw on screen, and
//! `rank` then points at the wrong document. `nexus click` would record
//! (and train the personal model on) a click on a document the user
//! never actually looked at.
//!
//! [`LastShownResults`] closes that gap for the common case (searching,
//! then clicking a result from that same search) by recording ground
//! truth at the moment results are actually displayed, rather than
//! reconstructing it later from a process that can drift.

use crate::document::DocId;
use crate::error::{NexusError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Exactly what was shown for one `nexus search` invocation: which
/// document appeared at each absolute (`offset`-aware) 1-based rank.
/// Only overwritten by the *next* `nexus search` — clicking doesn't
/// clear it, so multiple `nexus click` calls against the same search
/// (e.g. the user opens more than one result) all resolve correctly.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct LastShownResults {
    query: String,
    mode: String,
    ranked_doc_ids: HashMap<usize, DocId>,
    saved_unix: i64,
}

/// A search's results are only trusted for rank resolution for this
/// long after being shown. Past this, a `nexus click` falls back to
/// recomputing (with a warning) rather than resolving a rank against a
/// search result list that's likely stale — e.g. the index may have
/// been rebuilt since, in which case the recorded doc IDs could now
/// refer to entirely different documents.
const MAX_AGE_SECS: i64 = 6 * 60 * 60; // 6 hours
static LAST_SHOWN_SAVE_COUNTER: AtomicU64 = AtomicU64::new(0);

impl LastShownResults {
    /// Builds the record `nexus search` should persist right after
    /// printing `query`/`mode`'s results: `results` is the page actually
    /// shown, `offset` is the pagination offset that page started at (so
    /// rank numbers match exactly what was printed to the user).
    pub fn from_shown_page(
        query: &str,
        mode: &str,
        offset: usize,
        doc_ids: impl IntoIterator<Item = DocId>,
    ) -> Self {
        let ranked_doc_ids = doc_ids
            .into_iter()
            .enumerate()
            .map(|(i, doc_id)| (offset + i + 1, doc_id))
            .collect();
        LastShownResults {
            query: query.to_string(),
            mode: mode.to_string(),
            ranked_doc_ids,
            saved_unix: chrono::Utc::now().timestamp(),
        }
    }

    /// Loads the last-shown-results record from `path`, or `None` if
    /// none has been saved yet (e.g. `nexus click` run before any
    /// `nexus search`). A read/parse failure is treated the same as
    /// "none saved" — this is a best-effort optimization, not a source
    /// of truth `nexus click` should ever hard-fail over.
    pub fn load(path: &Path) -> Option<LastShownResults> {
        if !path.exists() {
            return None;
        }
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Saves this record to `path`. Best-effort by design in callers:
    /// `nexus search` should log and move on if this fails, never fail
    /// the search itself over it.
    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).map_err(|e| NexusError::io(parent, e))?;
        let bytes = serde_json::to_vec(self).map_err(|e| NexusError::Other(e.to_string()))?;
        let file_name = path.file_name().ok_or_else(|| {
            NexusError::Other(format!("last-shown path has no file name: {}", path.display()))
        })?;
        let (temp_path, mut file) = loop {
            let suffix = LAST_SHOWN_SAVE_COUNTER.fetch_add(1, Ordering::Relaxed);
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

    /// Resolves `rank` to the exact document shown there, but only if
    /// this record is for the *same* `query`/`mode` and isn't stale.
    /// Returns `None` for anything else — a different query, a different
    /// mode, a rank that was never shown (e.g. beyond the printed page),
    /// or a record too old to trust — so the caller can fall back to
    /// recomputing.
    pub fn resolve(&self, query: &str, mode: &str, rank: usize) -> Option<DocId> {
        if self.query != query || self.mode != mode {
            return None;
        }
        let age = chrono::Utc::now().timestamp() - self.saved_unix;
        if age < 0 || age > MAX_AGE_SECS {
            return None;
        }
        self.ranked_doc_ids.get(&rank).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_a_rank_from_the_same_query_and_mode() {
        let shown = LastShownResults::from_shown_page("rust", "local", 0, vec![10, 20, 30]);
        assert_eq!(shown.resolve("rust", "local", 1), Some(10));
        assert_eq!(shown.resolve("rust", "local", 3), Some(30));
    }

    #[test]
    fn respects_a_nonzero_pagination_offset() {
        // A search with `--offset 10` showed absolute ranks 11, 12, 13.
        let shown = LastShownResults::from_shown_page("rust", "local", 10, vec![101, 102, 103]);
        assert_eq!(shown.resolve("rust", "local", 11), Some(101));
        assert_eq!(shown.resolve("rust", "local", 1), None);
    }

    #[test]
    fn does_not_resolve_for_a_different_query_or_mode() {
        let shown = LastShownResults::from_shown_page("rust", "local", 0, vec![10, 20]);
        assert_eq!(shown.resolve("python", "local", 1), None);
        assert_eq!(shown.resolve("rust", "web", 1), None);
    }

    #[test]
    fn does_not_resolve_a_rank_beyond_the_shown_page() {
        let shown = LastShownResults::from_shown_page("rust", "local", 0, vec![10, 20]);
        assert_eq!(shown.resolve("rust", "local", 3), None);
    }

    #[test]
    fn does_not_resolve_a_stale_record() {
        let mut shown = LastShownResults::from_shown_page("rust", "local", 0, vec![10]);
        shown.saved_unix = chrono::Utc::now().timestamp() - MAX_AGE_SECS - 1;
        assert_eq!(shown.resolve("rust", "local", 1), None);
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("nexus-last-shown-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("last_search.json");

        let shown = LastShownResults::from_shown_page("rust ownership", "both", 0, vec![7, 8, 9]);
        shown.save(&path).unwrap();

        let loaded = LastShownResults::load(&path).unwrap();
        assert_eq!(loaded.resolve("rust ownership", "both", 2), Some(8));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_of_a_missing_file_is_none_not_an_error() {
        let path = std::env::temp_dir().join("nexus-last-shown-definitely-does-not-exist.json");
        assert!(LastShownResults::load(&path).is_none());
    }

    #[test]
    fn save_replaces_existing_record_without_temp_files() {
        let dir = std::env::temp_dir().join(format!(
            "nexus-last-shown-atomic-test-{}-{}",
            std::process::id(),
            LAST_SHOWN_SAVE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let path = dir.join("last_search.json");
        LastShownResults::from_shown_page("old", "local", 0, vec![1])
            .save(&path)
            .unwrap();
        LastShownResults::from_shown_page("new", "web", 0, vec![7, 8])
            .save(&path)
            .unwrap();

        let loaded = LastShownResults::load(&path).unwrap();
        assert_eq!(loaded.resolve("old", "local", 1), None);
        assert_eq!(loaded.resolve("new", "web", 2), Some(8));
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("last_search.json")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
