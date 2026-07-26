//! Persistent configuration for Nexus.
//!
//! Configuration is stored as TOML on disk (by default under the user's
//! config directory) and controls which folders are indexed, which files
//! are ignored, ranking behavior, and runtime tuning such as thread count.

use crate::error::{NexusError, Result};
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
            Config::load(path)
        } else {
            let config = Config::default();
            config.save(path)?;
            Ok(config)
        }
    }

    /// Loads and parses configuration from an existing TOML file.
    pub fn load(path: &Path) -> Result<Config> {
        let contents =
            std::fs::read_to_string(path).map_err(|e| NexusError::io(path, e))?;
        toml::from_str(&contents).map_err(|e| NexusError::Config(e.to_string()))
    }

    /// Serializes and writes this configuration to `path`, creating parent
    /// directories as needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| NexusError::io(parent, e))?;
        }
        let contents =
            toml::to_string_pretty(self).map_err(|e| NexusError::Config(e.to_string()))?;
        std::fs::write(path, contents).map_err(|e| NexusError::io(path, e))
    }

    /// Adds a folder to the indexed set, returning `false` if it was already present.
    pub fn add_folder(&mut self, folder: PathBuf) -> bool {
        if self.indexed_folders.contains(&folder) {
            false
        } else {
            self.indexed_folders.push(folder);
            true
        }
    }

    /// Removes a folder from the indexed set, returning `false` if it was not present.
    pub fn remove_folder(&mut self, folder: &Path) -> bool {
        let before = self.indexed_folders.len();
        self.indexed_folders.retain(|f| f != folder);
        self.indexed_folders.len() != before
    }
}
