use log::{debug, warn};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    pub enabled: bool,
    pub tokens_per_interval: u64,
    pub interval_seconds: u64,
    pub max_burst: u64,
    pub per_ip_enabled: bool,
    pub per_key_enabled: bool,
    pub search_quota: u64,
    pub crawl_quota: u64,
    pub concurrent_request_limit: usize,
    pub websocket_connection_limit: usize,
    pub websocket_message_limit: u64,
    pub max_query_length: usize,
    pub max_pagination_depth: usize,
    pub max_crawl_depth: u32,
    pub request_body_size_limit: u64,
    pub ban_duration_seconds: u64,
    pub trusted_proxies: Vec<String>,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            tokens_per_interval: 100,
            interval_seconds: 60,
            max_burst: 50,
            per_ip_enabled: true,
            per_key_enabled: false,
            search_quota: 1000,
            crawl_quota: 10,
            concurrent_request_limit: 50,
            websocket_connection_limit: 100,
            websocket_message_limit: 1000,
            max_query_length: 256,
            max_pagination_depth: 100,
            max_crawl_depth: 5,
            request_body_size_limit: 1024 * 1024,
            ban_duration_seconds: 3600,
            trusted_proxies: Vec::new(),
        }
    }
}

struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
    capacity: f64,
    refill_rate: f64,
    refill_interval: Duration,
}

impl TokenBucket {
    pub fn new(capacity: u64, refill_rate: u64, refill_interval: Duration) -> Self {
        let cap = capacity as f64;
        TokenBucket {
            tokens: cap,
            last_refill: Instant::now(),
            capacity: cap,
            refill_rate: refill_rate as f64,
            refill_interval,
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill);
        if elapsed >= self.refill_interval {
            let ticks = elapsed.as_nanos() / self.refill_interval.as_nanos();
            if ticks > 0 {
                let added = ticks as f64 * self.refill_rate;
                self.tokens = (self.tokens + added).min(self.capacity);
                self.last_refill = now;
            }
        }
    }

    pub fn consume(&mut self, tokens: u64) -> bool {
        self.refill();
        let needed = tokens as f64;
        if self.tokens >= needed {
            self.tokens -= needed;
            true
        } else {
            false
        }
    }
}

pub struct RateLimiter {
    buckets: Arc<Mutex<HashMap<String, TokenBucket>>>,
    config: RateLimitConfig,
    banned: Arc<Mutex<HashMap<String, Instant>>>,
    concurrent: Arc<Mutex<HashMap<String, usize>>>,
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        RateLimiter {
            buckets: Arc::new(Mutex::new(HashMap::new())),
            config,
            banned: Arc::new(Mutex::new(HashMap::new())),
            concurrent: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn consume(&self, key: &str, tokens: u64) -> bool {
        if !self.config.enabled {
            return true;
        }
        if self.is_banned(key) {
            return false;
        }
        let mut buckets = self.buckets.lock().expect("rate-limit lock poisoned");
        let bucket = buckets.entry(key.to_string()).or_insert_with(|| {
            TokenBucket::new(
                self.config.max_burst,
                self.config.tokens_per_interval,
                Duration::from_secs(self.config.interval_seconds),
            )
        });
        bucket.consume(tokens)
    }

    pub fn is_banned(&self, key: &str) -> bool {
        let banned = self.banned.lock().expect("banned lock poisoned");
        banned
            .get(key)
            .map(|&until| Instant::now() < until)
            .unwrap_or(false)
    }

    pub fn ban(&self, key: &str) {
        let duration = Duration::from_secs(self.config.ban_duration_seconds);
        let mut banned = self.banned.lock().expect("banned lock poisoned");
        banned.insert(key.to_string(), Instant::now() + duration);
        warn!("Rate limiter banned key '{}' for {:?}", key, duration);
    }

    pub fn add_concurrent(&self, key: &str) -> bool {
        let mut concurrent = self.concurrent.lock().expect("concurrent lock poisoned");
        let count = concurrent.entry(key.to_string()).or_insert(0);
        if *count >= self.config.concurrent_request_limit {
            false
        } else {
            *count += 1;
            debug!("concurrent requests for '{}': {}", key, *count);
            true
        }
    }

    pub fn remove_concurrent(&self, key: &str) {
        let mut concurrent = self.concurrent.lock().expect("concurrent lock poisoned");
        match concurrent.get_mut(key) {
            Some(count) => {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    concurrent.remove(key);
                }
            }
            None => debug!("remove_concurrent called for unknown key '{}'", key),
        }
    }

    pub fn validate_query(&self, query: &str) -> Result<(), String> {
        if query.len() > self.config.max_query_length {
            return Err(format!(
                "query length {} exceeds maximum of {}",
                query.len(),
                self.config.max_query_length
            ));
        }
        Ok(())
    }

    pub fn validate_pagination(&self, offset: usize, limit: usize) -> Result<(), String> {
        if offset > self.config.max_pagination_depth {
            return Err(format!(
                "offset {} exceeds maximum pagination depth of {}",
                offset, self.config.max_pagination_depth
            ));
        }
        if limit == 0 {
            return Err("limit must be greater than 0".to_string());
        }
        if limit > self.config.max_pagination_depth {
            return Err(format!(
                "limit {} exceeds maximum pagination depth of {}",
                limit, self.config.max_pagination_depth
            ));
        }
        Ok(())
    }

    pub fn extract_client_ip(&self, forwarded_for: Option<&str>, remote_addr: &str) -> String {
        if let Some(header) = forwarded_for {
            let proxies: Vec<&str> = header.split(',').map(|s| s.trim()).collect();
            if let Some(client_ip) = proxies.first() {
                if !self
                    .config
                    .trusted_proxies
                    .iter()
                    .any(|p| client_ip.starts_with(p))
                {
                    return client_ip.to_string();
                }
                for ip in proxies.iter().rev() {
                    if !self
                        .config
                        .trusted_proxies
                        .iter()
                        .any(|p| ip.starts_with(p))
                    {
                        return ip.to_string();
                    }
                }
            }
        }
        remote_addr.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> RateLimitConfig {
        RateLimitConfig {
            enabled: true,
            tokens_per_interval: 10,
            interval_seconds: 60,
            max_burst: 10,
            ..RateLimitConfig::default()
        }
    }

    #[test]
    fn consume_allows_within_burst() {
        let limiter = RateLimiter::new(test_config());
        assert!(limiter.consume("alice", 5));
        assert!(limiter.consume("alice", 5));
        assert!(!limiter.consume("alice", 1));
    }

    #[test]
    fn consume_disabled_always_allows() {
        let mut config = test_config();
        config.enabled = false;
        let limiter = RateLimiter::new(config);
        for _ in 0..1000 {
            assert!(limiter.consume("flood", 1));
        }
    }

    #[test]
    fn different_keys_dont_interfere() {
        let limiter = RateLimiter::new(test_config());
        assert!(limiter.consume("ip-a", 10));
        assert!(!limiter.consume("ip-a", 1));
        assert!(limiter.consume("ip-b", 10));
    }

    #[test]
    fn ban_and_is_banned() {
        let mut config = test_config();
        config.ban_duration_seconds = 3600;
        let limiter = RateLimiter::new(config);
        assert!(!limiter.is_banned("bad"));
        limiter.ban("bad");
        assert!(limiter.is_banned("bad"));
        assert!(!limiter.consume("bad", 1));
    }

    #[test]
    fn add_remove_concurrent() {
        let mut config = test_config();
        config.concurrent_request_limit = 2;
        let limiter = RateLimiter::new(config);
        assert!(limiter.add_concurrent("alice"));
        assert!(limiter.add_concurrent("alice"));
        assert!(!limiter.add_concurrent("alice"));
        limiter.remove_concurrent("alice");
        // Only 1 removed, so 1 remaining -- limit is still hit after adding 2 more
        assert!(limiter.add_concurrent("alice"));
        assert!(!limiter.add_concurrent("alice"));
        limiter.remove_concurrent("alice");
        limiter.remove_concurrent("alice");
        limiter.remove_concurrent("alice");
        // Now alice is gone; re-add should work
        assert!(limiter.add_concurrent("alice"));
        limiter.remove_concurrent("alice");
    }

    #[test]
    fn validate_query_checks_length() {
        let config = RateLimitConfig {
            max_query_length: 10,
            ..test_config()
        };
        let limiter = RateLimiter::new(config);
        assert!(limiter.validate_query("short").is_ok());
        assert!(limiter.validate_query("this is too long").is_err());
    }

    #[test]
    fn validate_pagination_checks_bounds() {
        let config = RateLimitConfig {
            max_pagination_depth: 50,
            ..test_config()
        };
        let limiter = RateLimiter::new(config);
        assert!(limiter.validate_pagination(0, 10).is_ok());
        assert!(limiter.validate_pagination(51, 10).is_err());
        assert!(limiter.validate_pagination(5, 0).is_err());
        assert!(limiter.validate_pagination(5, 51).is_err());
    }

    #[test]
    fn extract_client_ip_without_forwarded_for() {
        let limiter = RateLimiter::new(test_config());
        assert_eq!(limiter.extract_client_ip(None, "127.0.0.1"), "127.0.0.1");
    }

    #[test]
    fn extract_client_ip_with_forwarded_for() {
        let limiter = RateLimiter::new(test_config());
        assert_eq!(
            limiter.extract_client_ip(Some("203.0.113.5, 198.51.100.2"), "10.0.0.1"),
            "203.0.113.5"
        );
    }

    #[test]
    fn extract_client_ip_skips_trusted_proxy() {
        let mut config = test_config();
        config.trusted_proxies = vec!["10.0.0.".to_string()];
        let limiter = RateLimiter::new(config);
        // 10.0.0.1 is trusted (remote_addr), so the last untrusted hop is returned
        assert_eq!(
            limiter.extract_client_ip(Some("203.0.113.5, 10.0.0.1"), "10.0.0.1"),
            "203.0.113.5"
        );
    }
}
