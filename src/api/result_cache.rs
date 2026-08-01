//! A small TTL + capacity-bounded cache in front of search execution,
//! so a hot or repeated query (the same handful of queries a real
//! deployment tends to see disproportionately often) doesn't re-run the
//! full ranking pipeline every single time. This sits alongside
//! [`crate::api::rate_limit`] (which limits *how often* a client may
//! request) rather than replacing it — caching helps *cost*, rate
//! limiting helps *fairness/abuse*, and a request can need either or
//! both independently.
//!
//! Deliberately a plain `Mutex<LinkedHashMap>`-style structure rather
//! than pulling in an external caching crate: the eviction policy needed
//! here (LRU by access order, expire by absolute TTL, bounded by entry
//! count) is small enough to implement directly and keep fully visible/
//! auditable, matching this codebase's general preference (see e.g.
//! `crate::dedup`'s hand-rolled SimHash) for owning the small stuff
//! rather than adding a dependency for it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Entry<V> {
    value: V,
    inserted_at: Instant,
    last_accessed_at: Instant,
}

/// A cache from `String` keys (callers typically hash/normalize a query
/// + filters + pagination into one cache key) to a cloneable value type
/// `V` (typically a serialized/`Arc`-wrapped search response).
pub struct ResultCache<V: Clone> {
    entries: Mutex<HashMap<String, Entry<V>>>,
    capacity: usize,
    ttl: Duration,
    hits: std::sync::atomic::AtomicU64,
    misses: std::sync::atomic::AtomicU64,
}

impl<V: Clone> ResultCache<V> {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        ResultCache {
            entries: Mutex::new(HashMap::new()),
            capacity,
            ttl,
            hits: std::sync::atomic::AtomicU64::new(0),
            misses: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Returns a cached value for `key` if present and not expired.
    /// Bumps its recency for LRU purposes on a hit.
    pub fn get(&self, key: &str) -> Option<V> {
        let mut entries = self.entries.lock().expect("result cache mutex poisoned");
        if let Some(entry) = entries.get_mut(key) {
            if entry.inserted_at.elapsed() > self.ttl {
                entries.remove(key);
                self.misses.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return None;
            }
            entry.last_accessed_at = Instant::now();
            self.hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Some(entry.value.clone());
        }
        self.misses.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        None
    }

    /// Inserts/replaces `key`'s cached value, evicting the least
    /// recently accessed entry first if this would exceed `capacity`.
    pub fn put(&self, key: String, value: V) {
        let mut entries = self.entries.lock().expect("result cache mutex poisoned");
        if entries.len() >= self.capacity && !entries.contains_key(&key) {
            if let Some(lru_key) = entries
                .iter()
                .min_by_key(|(_, e)| e.last_accessed_at)
                .map(|(k, _)| k.clone())
            {
                entries.remove(&lru_key);
            }
        }
        let now = Instant::now();
        entries.insert(
            key,
            Entry {
                value,
                inserted_at: now,
                last_accessed_at: now,
            },
        );
    }

    /// Drops every cached entry, e.g. after a re-index makes all cached
    /// results potentially stale.
    pub fn clear(&self) {
        self.entries.lock().expect("result cache mutex poisoned").clear();
    }

    pub fn len(&self) -> usize {
        self.entries.lock().expect("result cache mutex poisoned").len()
    }

    /// Hit rate across this cache's lifetime, for the `/metrics` endpoint
    /// (see `crate::observability`).
    pub fn hit_rate(&self) -> f64 {
        let hits = self.hits.load(std::sync::atomic::Ordering::Relaxed);
        let misses = self.misses.load(std::sync::atomic::Ordering::Relaxed);
        let total = hits + misses;
        if total == 0 {
            0.0
        } else {
            hits as f64 / total as f64
        }
    }
}

/// Builds a stable cache key from a query and whatever pagination/filter
/// parameters affect the result set — everything that changes the
/// response needs to be part of the key, or two different requests would
/// incorrectly share a cached answer.
pub fn cache_key(query: &str, offset: usize, limit: usize, extra: &[(&str, &str)]) -> String {
    let mut key = format!("q={query}&o={offset}&l={limit}");
    let mut extras: Vec<&(&str, &str)> = extra.iter().collect();
    extras.sort();
    for (k, v) in extras {
        key.push_str(&format!("&{k}={v}"));
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_retrieves_a_value() {
        let cache = ResultCache::new(10, Duration::from_secs(60));
        cache.put("k1".to_string(), 42);
        assert_eq!(cache.get("k1"), Some(42));
        assert_eq!(cache.get("missing"), None);
    }

    #[test]
    fn expires_entries_past_ttl() {
        let cache = ResultCache::new(10, Duration::from_millis(10));
        cache.put("k1".to_string(), "value");
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(cache.get("k1"), None);
    }

    #[test]
    fn evicts_least_recently_used_when_over_capacity() {
        let cache = ResultCache::new(2, Duration::from_secs(60));
        cache.put("a".to_string(), 1);
        cache.put("b".to_string(), 2);
        // Access "a" so "b" becomes the least-recently-used entry.
        assert_eq!(cache.get("a"), Some(1));
        cache.put("c".to_string(), 3);
        assert_eq!(cache.get("b"), None, "b should have been evicted as LRU");
        assert_eq!(cache.get("a"), Some(1));
        assert_eq!(cache.get("c"), Some(3));
    }

    #[test]
    fn hit_rate_reflects_gets() {
        let cache = ResultCache::new(10, Duration::from_secs(60));
        cache.put("k".to_string(), 1);
        cache.get("k"); // hit
        cache.get("missing"); // miss
        assert!((cache.hit_rate() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn cache_key_differs_by_pagination_and_extras() {
        let a = cache_key("rust", 0, 10, &[("safe", "on")]);
        let b = cache_key("rust", 10, 10, &[("safe", "on")]);
        let c = cache_key("rust", 0, 10, &[("safe", "off")]);
        assert_ne!(a, b);
        assert_ne!(a, c);
    }
}
