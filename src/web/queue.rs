//! The crawl queue: thousands of discovered URLs waiting to be fetched,
//! ordered so sitemap-listed and shallow pages are visited first, rate
//! limited per domain so a crawl doesn't hammer any one site, and
//! persisted to disk so a large crawl can be interrupted and resumed
//! rather than starting over.

use crate::error::{NexusError, Result};
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{SystemTime, UNIX_EPOCH};

static QUEUE_SAVE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One URL waiting to be crawled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueEntry {
    /// The canonical URL to fetch.
    pub url: String,
    /// Registrable domain, cached so rate limiting doesn't need to
    /// re-parse the URL on every pop.
    pub domain: String,
    /// Crawl depth from the nearest seed URL.
    pub depth: u32,
    /// Higher priority is dequeued first. Sitemap-listed URLs and seeds
    /// get a boost over URLs merely discovered via in-page links.
    pub priority: i32,
}

impl Eq for QueueEntry {}
impl Ord for QueueEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher priority first; among equal priority, shallower first.
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.depth.cmp(&self.depth))
    }
}
impl PartialOrd for QueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Priority tiers used when enqueueing. Higher wins.
pub mod priority {
    /// A URL explicitly given as a crawl seed.
    pub const SEED: i32 = 100;
    /// A URL discovered via `sitemap.xml`.
    pub const SITEMAP: i32 = 75;
    /// A URL discovered via an RSS/Atom feed item — typically fresh
    /// content worth prioritizing over ordinary discovered links, but
    /// below sitemap entries (which represent a site's authoritative,
    /// complete page list rather than just its recent items).
    pub const FEED: i32 = 50;
    /// A URL discovered by following an in-page link.
    pub const DISCOVERED: i32 = 10;
}

/// The crawl frontier: a priority queue of not-yet-fetched URLs, dedup
/// tracking so the same URL is never enqueued twice, and per-domain
/// rate-limit bookkeeping.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CrawlQueue {
    heap: BinaryHeap<QueueEntry>,
    seen: HashSet<String>,
    /// Unix-epoch millis of the last fetch per domain, for rate limiting.
    #[serde(skip)]
    last_fetch_millis: HashMap<String, u128>,
    /// Per-domain minimum interval between fetches, in milliseconds.
    /// Populated from `robots.txt`'s `Crawl-delay` or a global default.
    pub domain_delay_millis: HashMap<String, u64>,
    /// Default delay applied to domains with no specific entry.
    pub default_delay_millis: u64,
}

impl CrawlQueue {
    /// Creates an empty queue with the given default per-domain delay.
    pub fn new(default_delay_millis: u64) -> Self {
        CrawlQueue {
            heap: BinaryHeap::new(),
            seen: HashSet::new(),
            last_fetch_millis: HashMap::new(),
            domain_delay_millis: HashMap::new(),
            default_delay_millis,
        }
    }

    /// Enqueues `url` at `priority` if it has not already been seen.
    /// Returns `true` if it was newly added.
    pub fn push(&mut self, url: String, domain: String, depth: u32, priority: i32) -> bool {
        if self.seen.contains(&url) {
            return false;
        }
        self.seen.insert(url.clone());
        self.heap.push(QueueEntry {
            url: url.clone(),
            domain,
            depth,
            priority,
        });
        debug!("enqueued {} (priority={}, depth={})", url, priority, depth);
        true
    }

    /// Returns `true` if `url` has already been enqueued (at any point),
    /// so callers can skip re-fetching link targets already in the graph
    /// without needing to touch the queue.
    pub fn has_seen(&self, url: &str) -> bool {
        self.seen.contains(url)
    }

    /// Sets the crawl-delay (from `robots.txt`) for a domain.
    pub fn set_domain_delay(&mut self, domain: &str, delay_millis: u64) {
        self.domain_delay_millis
            .insert(domain.to_string(), delay_millis);
    }

    /// Pops the next entry that is both highest-priority and whose domain
    /// is not currently rate-limited. Entries whose domain is on cooldown
    /// are set aside and re-offered on the next call, so a crawl of many
    /// domains keeps making progress on other domains while one cools
    /// down, rather than blocking.
    pub fn pop_ready(&mut self) -> Option<QueueEntry> {
        let now = now_millis();
        let mut deferred: Vec<QueueEntry> = Vec::new();
        let mut ready = None;

        while let Some(entry) = self.heap.pop() {
            let delay = self
                .domain_delay_millis
                .get(&entry.domain)
                .copied()
                .unwrap_or(self.default_delay_millis);
            let last = self
                .last_fetch_millis
                .get(&entry.domain)
                .copied()
                .unwrap_or(0);
            if now.saturating_sub(last) >= delay as u128 {
                ready = Some(entry);
                break;
            } else {
                deferred.push(entry);
            }
        }

        for entry in deferred {
            self.heap.push(entry);
        }

        if let Some(entry) = &ready {
            self.last_fetch_millis.insert(entry.domain.clone(), now);
            debug!(
                "dequeued {} (domain={}, depth={})",
                entry.url, entry.domain, entry.depth
            );
        }
        ready
    }

    /// `true` if there is nothing left to (eventually) fetch.
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Number of URLs still waiting in the queue.
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Total number of distinct URLs ever enqueued (including already
    /// popped ones), useful for progress reporting.
    pub fn total_seen(&self) -> usize {
        self.seen.len()
    }

    /// Persists the queue to `path` so a large crawl can be resumed later.
    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).map_err(|e| NexusError::io(parent, e))?;
        let bytes = bincode::serialize(self).map_err(NexusError::Serialize)?;
        let file_name = path.file_name().ok_or_else(|| {
            NexusError::Other(format!("crawl queue path has no file name: {}", path.display()))
        })?;
        let (temp_path, mut file) = loop {
            let suffix = QUEUE_SAVE_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
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
            warn!("failed to save queue to {}: {}", path.display(), error);
            return Err(NexusError::io(&temp_path, error));
        }
        drop(file);
        if let Err(error) = std::fs::rename(&temp_path, path) {
            let _ = std::fs::remove_file(&temp_path);
            warn!("failed to replace queue at {}: {}", path.display(), error);
            return Err(NexusError::io(path, error));
        }
        info!(
            "saved queue with {} entries to {}",
            self.len(),
            path.display()
        );
        Ok(())
    }

    /// Loads a previously-saved queue from `path`.
    pub fn load(path: &Path) -> Result<CrawlQueue> {
        let bytes = std::fs::read(path).map_err(|e| NexusError::io(path, e))?;
        let queue: CrawlQueue = bincode::deserialize(&bytes).map_err(|e| {
            warn!("failed to deserialize queue from {}: {}", path.display(), e);
            NexusError::Deserialize(e)
        })?;
        info!(
            "loaded queue with {} entries from {}",
            queue.len(),
            path.display()
        );
        Ok(queue)
    }
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedups_pushed_urls() {
        let mut q = CrawlQueue::new(0);
        assert!(q.push("https://a.com/1".into(), "a.com".into(), 0, priority::SEED));
        assert!(!q.push("https://a.com/1".into(), "a.com".into(), 0, priority::SEED));
        assert_eq!(q.total_seen(), 1);
    }

    #[test]
    fn higher_priority_popped_first() {
        let mut q = CrawlQueue::new(0);
        q.push(
            "https://a.com/discovered".into(),
            "a.com".into(),
            2,
            priority::DISCOVERED,
        );
        q.push(
            "https://a.com/seed".into(),
            "a.com".into(),
            0,
            priority::SEED,
        );
        q.push(
            "https://a.com/sitemap".into(),
            "a.com".into(),
            1,
            priority::SITEMAP,
        );

        assert_eq!(q.pop_ready().unwrap().url, "https://a.com/seed");
        assert_eq!(q.pop_ready().unwrap().url, "https://a.com/sitemap");
        assert_eq!(q.pop_ready().unwrap().url, "https://a.com/discovered");
    }

    #[test]
    fn rate_limits_defer_same_domain() {
        let mut q = CrawlQueue::new(10_000); // 10s default delay
        q.push("https://a.com/1".into(), "a.com".into(), 0, priority::SEED);
        q.push("https://b.com/1".into(), "b.com".into(), 0, priority::SEED);

        let first = q.pop_ready().unwrap();
        assert_eq!(first.domain, "a.com");
        // a.com is now on cooldown; b.com should still be poppable.
        let second = q.pop_ready().unwrap();
        assert_eq!(second.domain, "b.com");
        // Nothing else is ready immediately.
        assert!(q.pop_ready().is_none());
    }

    #[test]
    fn save_and_load_round_trip() {
        let mut q = CrawlQueue::new(0);
        q.push("https://a.com/1".into(), "a.com".into(), 0, priority::SEED);
        let dir = std::env::temp_dir().join(format!("nexus-queue-test-{}", std::process::id()));
        let path = dir.join("queue.nxq");
        q.save(&path).unwrap();
        let loaded = CrawlQueue::load(&path).unwrap();
        assert_eq!(loaded.total_seen(), 1);
        assert_eq!(loaded.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_replaces_existing_queue_without_leaving_temp_files() {
        let dir = std::env::temp_dir().join(format!(
            "nexus-queue-atomic-test-{}-{}",
            std::process::id(),
            QUEUE_SAVE_COUNTER.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        let path = dir.join("queue.nxq");
        let mut first = CrawlQueue::new(0);
        first.push("https://a.com/1".into(), "a.com".into(), 0, priority::SEED);
        first.save(&path).unwrap();

        let mut replacement = CrawlQueue::new(0);
        replacement.push("https://b.com/1".into(), "b.com".into(), 0, priority::SEED);
        replacement.push("https://b.com/2".into(), "b.com".into(), 0, priority::SEED);
        replacement.save(&path).unwrap();

        let loaded = CrawlQueue::load(&path).unwrap();
        assert_eq!(loaded.total_seen(), 2);
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("queue.nxq")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
