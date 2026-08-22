//! A simple disk-backed cache mapping [`DocId`] to the extracted plain
//! text of a crawled web page. Local filesystem documents don't need
//! this — their content is just re-read from disk on demand — but a web
//! page's content only exists because we fetched it once, so it has to
//! be cached somewhere for snippet generation and re-indexing to work
//! without re-crawling.

use crate::document::DocId;
use crate::error::{NexusError, Result};
use log::debug;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static CONTENT_CACHE_SAVE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A directory of `{doc_id}.txt` files holding cached page text.
#[derive(Debug, Clone)]
pub struct ContentCache {
    dir: PathBuf,
}

impl ContentCache {
    /// Creates a cache rooted at `dir` (created lazily on first write).
    pub fn new(dir: PathBuf) -> Self {
        ContentCache { dir }
    }

    fn path_for(&self, doc_id: DocId) -> PathBuf {
        self.dir.join(format!("{doc_id}.txt"))
    }

    /// Stores `text` for `doc_id`, overwriting any previous content.
    pub fn store(&self, doc_id: DocId, text: &str) -> Result<()> {
        debug!("storing doc_id={} ({} bytes)", doc_id, text.len());
        std::fs::create_dir_all(&self.dir).map_err(|e| NexusError::io(&self.dir, e))?;
        let path = self.path_for(doc_id);
        let (temp_path, mut file) = loop {
            let suffix = CONTENT_CACHE_SAVE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let candidate = self.dir.join(format!(
                ".{doc_id}.{}.{}.tmp",
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
        if let Err(error) = file.write_all(text.as_bytes()).and_then(|_| file.sync_all()) {
            drop(file);
            let _ = std::fs::remove_file(&temp_path);
            return Err(NexusError::io(&temp_path, error));
        }
        drop(file);
        if let Err(error) = std::fs::rename(&temp_path, &path) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(NexusError::io(&path, error));
        }
        Ok(())
    }

    /// Loads cached text for `doc_id`, if present.
    pub fn load(&self, doc_id: DocId) -> Result<String> {
        debug!("loading doc_id={}", doc_id);
        let path = self.path_for(doc_id);
        std::fs::read_to_string(&path).map_err(|e| NexusError::io(&path, e))
    }

    /// Removes cached text for `doc_id`, if present. Errors from a
    /// missing file are swallowed, since "already gone" is an acceptable
    /// outcome for a cleanup operation.
    pub fn remove(&self, doc_id: DocId) {
        debug!("removing doc_id={}", doc_id);
        let _ = std::fs::remove_file(self.path_for(doc_id));
    }

    /// Root directory this cache stores files under.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_loads_content() {
        let dir =
            std::env::temp_dir().join(format!("nexus-content-cache-test-{}", std::process::id()));
        let cache = ContentCache::new(dir.clone());
        cache.store(42, "hello world").unwrap();
        assert_eq!(cache.load(42).unwrap(), "hello world");
        cache.remove(42);
        assert!(cache.load(42).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn store_replaces_existing_content_without_temp_files() {
        let dir = std::env::temp_dir().join(format!(
            "nexus-content-cache-atomic-test-{}-{}",
            std::process::id(),
            CONTENT_CACHE_SAVE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let cache = ContentCache::new(dir.clone());
        cache.store(42, "old content").unwrap();
        cache.store(42, "new content").unwrap();
        assert_eq!(cache.load(42).unwrap(), "new content");
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("42.txt")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
