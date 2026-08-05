//! A simple disk-backed cache mapping [`DocId`] to the extracted plain
//! text of a crawled web page. Local filesystem documents don't need
//! this — their content is just re-read from disk on demand — but a web
//! page's content only exists because we fetched it once, so it has to
//! be cached somewhere for snippet generation and re-indexing to work
//! without re-crawling.

use crate::document::DocId;
use crate::error::{NexusError, Result};
use log::debug;
use std::path::{Path, PathBuf};

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
        std::fs::write(&path, text).map_err(|e| NexusError::io(&path, e))
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
}
