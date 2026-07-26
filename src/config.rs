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
}

impl Default for RankingConfig {
    fn default() -> Self {
        RankingConfig {
            bm25_k1: 1.2,
            bm25_b: 0.75,
            filename_boost: 2.0,
            exact_match_boost: 1.5,
            recency_boost: 1.1,
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
    /// Size, in entries, of the in-memory search-result cache.
    pub cache_size: usize,
    /// Number of worker threads used for crawling/indexing. `0` means "use
    /// all available cores" (delegated to Rayon's default).
    pub thread_count: usize,
    /// Path on disk where the binary index is persisted.
    pub index_path: PathBuf,
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
            cache_size: 256,
            thread_count: 0,
            index_path: default_index_path(),
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
