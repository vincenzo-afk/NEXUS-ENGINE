//! Persistent configuration for Nexus.
//!
//! Configuration is stored as TOML on disk (by default under the user's
//! config directory) and controls which folders are indexed, which files
//! are ignored, ranking behavior, and runtime tuning such as thread count.

use crate::error::{NexusError, Result};
use log::{debug, info};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Ranking-related tunables, exposed so users can tweak relevance behavior
/// without recompiling the engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankingConfig {
    /// BM25 term-frequency saturation parameter.
    pub bm25_k1: f32,
    /// BM25 length-normalization parameter (0 = no normalization, 1 = full).
    pub bm25_b: f32,
    /// Multiplicative boost applied when the query matches the filename.
    pub filename_boost: f32,
    /// Multiplicative boost applied to exact (non-fuzzy) phrase matches.
    pub exact_match_boost: f32,
    /// Multiplicative boost applied to more recently modified documents.
    pub recency_boost: f32,
    /// How strongly a page's PageRank score influences its final rank.
    /// `final *= 1.0 + pagerank_weight * (pagerank * document_count)`; the
    /// `document_count` factor rescales PageRank (which sums to ~1.0
    /// across the whole graph) back up to an "average share" of 1.0 so
    /// the weight is meaningful regardless of how many pages are indexed.
    pub pagerank_weight: f32,
    /// Multiplicative boost applied when the query matches a web page's
    /// `<title>`.
    pub title_match_boost: f32,
    /// Multiplicative boost applied when the query matches a web page's URL.
    pub url_match_boost: f32,
    /// Weight applied to click history: `1.0 + click_weight * ln(1 + clicks)`.
    pub click_weight: f32,
    /// Multiplicative boost applied to pages on an explicitly trusted domain.
    pub trusted_domain_boost: f32,
    /// Multiplicative penalty applied to pages on an explicitly
    /// low-quality/spam domain (should be < 1.0).
    pub spam_domain_penalty: f32,
    /// Domains (registrable, no `www.`) that receive [`trusted_domain_boost`](Self::trusted_domain_boost).
    pub trusted_domains: HashSet<String>,
    /// Domains that receive [`spam_domain_penalty`](Self::spam_domain_penalty).
    pub spam_domains: HashSet<String>,
    /// Multiplicative boost applied to local filesystem results in hybrid
    /// (`SearchMode::Both`) search — local files rank slightly above web
    /// results at the same relevance, on the theory that you indexed them
    /// yourself and they're more likely to be exactly what you meant.
    pub local_boost: f32,
    /// Minimum SimHash similarity (0.0-1.0) for hybrid mode to treat a
    /// local file and a web page as duplicate content, in which case only
    /// the local result is kept.
    pub hybrid_dedup_min_similarity: f32,
}

impl Default for RankingConfig {
    fn default() -> Self {
        RankingConfig {
            bm25_k1: 1.2,
            bm25_b: 0.75,
            filename_boost: 2.0,
            exact_match_boost: 1.5,
            recency_boost: 1.1,
            pagerank_weight: 0.5,
            title_match_boost: 1.6,
            url_match_boost: 1.2,
            click_weight: 0.15,
            trusted_domain_boost: 1.15,
            spam_domain_penalty: 0.5,
            local_boost: 1.2,
            hybrid_dedup_min_similarity: 0.80,
            trusted_domains: [
                "wikipedia.org",
                "github.com",
                "docs.rs",
                "developer.mozilla.org",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            spam_domains: HashSet::new(),
        }
    }
}

/// Web-crawler tunables.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebCrawlConfig {
    /// `User-Agent` header sent with every crawl request.
    pub user_agent: String,
    /// Maximum number of pages fetched in a single `nexus crawl` run.
    pub max_pages: usize,
    /// Maximum link-following depth from a seed URL.
    pub max_depth: u32,
    /// Minimum delay between requests to the same domain, in milliseconds,
    /// used when `robots.txt` declares no `Crawl-delay` of its own.
    pub default_delay_millis: u64,
    /// Per-request timeout, in seconds.
    pub timeout_seconds: u64,
    /// Maximum retry attempts for transient failures.
    pub max_retries: u32,
    /// Whether to respect `robots.txt`. Disabling this is strongly
    /// discouraged and intended only for crawling infrastructure you
    /// control yourself.
    pub respect_robots: bool,
    /// If non-empty, only these registrable domains (and their
    /// subdomains) are ever fetched, regardless of what links are
    /// discovered. Leave empty to allow the crawl to follow links
    /// anywhere (bounded by `max_pages` and `max_depth`).
    pub allowed_domains: Vec<String>,
    /// Maximum response body size, in bytes, that will be downloaded.
    pub max_page_size_bytes: u64,
    /// Maximum number of pages fetched from any single domain within one
    /// crawl run, regardless of the overall `max_pages` budget. Prevents
    /// one large or infinitely-linking site (calendar pages, faceted
    /// search results, a misbehaving CMS generating endless unique URLs,
    /// etc.) from consuming the entire crawl budget and effectively
    /// spamming the index with pages from one source. `0` means
    /// unlimited (falls back to the overall `max_pages` budget only).
    #[serde(default = "default_max_pages_per_domain")]
    pub max_pages_per_domain: usize,
    /// Whether to discover RSS/Atom feeds (via `<link rel="alternate">`
    /// tags and well-known paths like `/feed`) and crawl their items at
    /// elevated priority, similar to sitemap discovery.
    #[serde(default = "default_true")]
    pub discover_feeds: bool,
}

fn default_max_pages_per_domain() -> usize {
    100
}

fn default_true() -> bool {
    true
}

impl Default for WebCrawlConfig {
    fn default() -> Self {
        WebCrawlConfig {
            user_agent: "NexusBot/1.0 (+https://github.com/vincenzo-afk/nexus; search-crawler)"
                .to_string(),
            max_pages: 500,
            max_depth: 5,
            default_delay_millis: 1000,
            timeout_seconds: 20,
            max_retries: 3,
            respect_robots: true,
            allowed_domains: Vec::new(),
            max_page_size_bytes: 10 * 1024 * 1024,
            max_pages_per_domain: default_max_pages_per_domain(),
            discover_feeds: true,
        }
    }
}

/// Privacy-related configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivacyConfig {
    /// Block sponsored/promoted results from appearing in search results.
    pub block_sponsored_results: bool,
    /// Disable filter bubble (personalized results based on history).
    pub no_filter_bubble: bool,
    /// Anonymize search queries before processing.
    pub anonymize_queries: bool,
    /// Disable telemetry and usage statistics collection.
    pub disable_telemetry: bool,
    /// Auto-delete search history after N days (None = never).
    pub auto_delete_history_days: Option<u64>,
    /// URL to the full privacy policy document.
    pub privacy_policy_url: String,
}

impl Default for PrivacyConfig {
    fn default() -> Self {
        PrivacyConfig {
            block_sponsored_results: true,
            no_filter_bubble: true,
            anonymize_queries: true,
            disable_telemetry: true,
            auto_delete_history_days: Some(90),
            privacy_policy_url: "https://github.com/vincenzo-afk/NEXUS-ENGINE/blob/main/PRIVACY.md"
                .to_string(),
        }
    }
}

/// API security configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// Require authentication for API requests.
    pub api_require_auth: bool,
    /// Valid API keys for authenticated requests.
    pub api_keys: Vec<String>,
    /// Minimum TLS version for HTTPS connections.
    pub tls_min_version: String,
    /// Enable certificate pinning.
    pub enable_certificate_pinning: bool,
    /// Allowed CORS origins.
    pub cors_allowed_origins: Vec<String>,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        SecurityConfig {
            api_require_auth: false,
            api_keys: Vec::new(),
            tls_min_version: "1.2".to_string(),
            enable_certificate_pinning: false,
            cors_allowed_origins: vec!["http://localhost:8080".to_string()],
        }
    }
}

/// WebSocket search-as-you-type server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSocketConfig {
    /// Enable the WebSocket server.
    pub enabled: bool,
    /// Address to bind the WebSocket server to.
    pub bind_address: String,
    /// Maximum concurrent WebSocket connections.
    pub max_connections: usize,
    /// WebSocket message rate limit (per minute).
    pub message_rate_limit: u64,
}

impl Default for WebSocketConfig {
    fn default() -> Self {
        WebSocketConfig {
            enabled: false,
            bind_address: "127.0.0.1:8081".to_string(),
            max_connections: 100,
            message_rate_limit: 60,
        }
    }
}

/// Tor proxy configuration for private web crawling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorProxyConfig {
    /// Enable Tor proxy for all HTTP requests.
    pub enabled: bool,
    /// SOCKS5 proxy hostname.
    pub proxy_host: String,
    /// SOCKS5 proxy port.
    pub proxy_port: u16,
    /// How often to rotate Tor identity (in minutes).
    pub identity_rotation_minutes: u64,
}

impl Default for TorProxyConfig {
    fn default() -> Self {
        TorProxyConfig {
            enabled: false,
            proxy_host: "127.0.0.1".to_string(),
            proxy_port: 9050,
            identity_rotation_minutes: 60,
        }
    }
}

/// Top-level Nexus configuration, serialized to `~/.config/nexus/config.toml`
/// (or platform equivalent) unless a custom path is supplied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Folders that are recursively indexed.
    pub indexed_folders: Vec<PathBuf>,
    /// Folder names (not full paths) that are always skipped, e.g. `node_modules`.
    pub ignored_folders: HashSet<String>,
    /// File extensions (without the leading dot) that are skipped during indexing.
    pub ignored_extensions: HashSet<String>,
    /// If true, dotfiles / dot-directories are skipped during crawling.
    pub ignore_hidden: bool,
    /// Maximum file size (in bytes) that will be read and indexed.
    pub max_file_size_bytes: u64,
    /// Ranking tunables.
    pub ranking: RankingConfig,
    /// Web crawler tunables.
    #[serde(default)]
    pub web_crawl: WebCrawlConfig,
    /// Privacy configuration.
    #[serde(default)]
    pub privacy: PrivacyConfig,
    /// Security configuration.
    #[serde(default)]
    pub security: SecurityConfig,
    /// WebSocket server configuration.
    #[serde(default)]
    pub websocket: WebSocketConfig,
    /// Tor proxy configuration.
    #[serde(default)]
    pub tor: TorProxyConfig,
    /// Size, in entries, of the in-memory search-result cache.
    pub cache_size: usize,
    /// Number of worker threads used for crawling/indexing. `0` means "use
    /// all available cores" (delegated to Rayon's default).
    pub thread_count: usize,
    /// Path on disk where the binary index is persisted.
    pub index_path: PathBuf,
    /// Directory on disk where extracted plain-text content of crawled
    /// web pages is cached, so snippets can be generated without
    /// re-fetching the page. Local filesystem documents don't need this
    /// (their content is re-read from disk on demand).
    #[serde(default = "default_content_cache_dir")]
    pub content_cache_dir: PathBuf,
    /// Path on disk where the click-history log is persisted.
    #[serde(default = "default_clicks_path")]
    pub clicks_path: PathBuf,
    /// Path on disk where the resumable crawl queue is persisted.
    #[serde(default = "default_queue_path")]
    pub crawl_queue_path: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        let ignored_folders: HashSet<String> = [
            ".git",
            "node_modules",
            "target",
            ".cargo",
            ".svn",
            ".hg",
            "__pycache__",
            ".venv",
            "venv",
            "dist",
            "build",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        Config {
            indexed_folders: Vec::new(),
            ignored_folders,
            ignored_extensions: HashSet::new(),
            ignore_hidden: true,
            max_file_size_bytes: 25 * 1024 * 1024, // 25 MiB
            ranking: RankingConfig::default(),
            web_crawl: WebCrawlConfig::default(),
            privacy: PrivacyConfig::default(),
            security: SecurityConfig::default(),
            websocket: WebSocketConfig::default(),
            tor: TorProxyConfig::default(),
            cache_size: 256,
            thread_count: 0,
            index_path: default_index_path(),
            content_cache_dir: default_content_cache_dir(),
            clicks_path: default_clicks_path(),
            crawl_queue_path: default_queue_path(),
        }
    }
}

/// Returns the default on-disk location for the persisted index.
fn default_index_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("nexus")
        .join("index.nxi")
}

/// Returns the default on-disk directory for cached web page content.
fn default_content_cache_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("nexus")
        .join("web_content_cache")
}

/// Returns the default on-disk location for the click-history log.
fn default_clicks_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("nexus")
        .join("clicks.nxc")
}

/// Returns the default on-disk location for the resumable crawl queue.
fn default_queue_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("nexus")
        .join("crawl_queue.nxq")
}

/// Returns the default on-disk location of the configuration file.
pub fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("nexus")
        .join("config.toml")
}

impl Config {
    /// Loads configuration from `path`, creating a default configuration
    /// file at that location if none exists yet.
    pub fn load_or_create(path: &Path) -> Result<Config> {
        if path.exists() {
            info!("loading config from {}", path.display());
            Config::load(path)
        } else {
            info!("creating default config at {}", path.display());
            let config = Config::default();
            config.save(path)?;
            Ok(config)
        }
    }

    /// Loads and parses configuration from an existing TOML file.
    pub fn load(path: &Path) -> Result<Config> {
        info!("loading config from {}", path.display());
        let contents = std::fs::read_to_string(path).map_err(|e| NexusError::io(path, e))?;
        let config: Config =
            toml::from_str(&contents).map_err(|e| NexusError::Config(e.to_string()))?;
        info!("config loaded from {}", path.display());
        Ok(config)
    }

    /// Serializes and writes this configuration to `path`, creating parent
    /// directories as needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        info!("saving config to {}", path.display());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| NexusError::io(parent, e))?;
        }
        let contents =
            toml::to_string_pretty(self).map_err(|e| NexusError::Config(e.to_string()))?;
        std::fs::write(path, contents).map_err(|e| NexusError::io(path, e))?;
        info!("config saved to {}", path.display());
        Ok(())
    }

    /// Adds a folder to the indexed set, returning `false` if it was already present.
    pub fn add_folder(&mut self, folder: PathBuf) -> bool {
        if self.indexed_folders.contains(&folder) {
            debug!("folder already indexed: {}", folder.display());
            false
        } else {
            debug!("adding folder: {}", folder.display());
            self.indexed_folders.push(folder);
            true
        }
    }

    /// Removes a folder from the indexed set, returning `false` if it was not present.
    pub fn remove_folder(&mut self, folder: &Path) -> bool {
        let before = self.indexed_folders.len();
        self.indexed_folders.retain(|f| f != folder);
        let removed = self.indexed_folders.len() != before;
        if removed {
            debug!("removed folder: {}", folder.display());
        } else {
            debug!("folder not found: {}", folder.display());
        }
        removed
    }
}
