//! Command-line interface definitions.
//!
//! Nexus is driven entirely through subcommands; see [`Commands`] for the
//! full list. Argument parsing is handled by `clap`'s derive macros so the
//! `--help` output stays in sync with this definition automatically.

pub mod commands;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Nexus - a fast local full-text desktop search engine.
#[derive(Debug, Parser)]
#[command(name = "nexus", version, about, long_about = None)]
pub struct Cli {
    /// Path to a custom configuration file (defaults to the platform config dir).
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// Enable verbose (info-level) logging.
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Enable debug-level logging (implies --verbose).
    #[arg(long, global = true)]
    pub debug: bool,

    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Commands,
}

/// All top-level Nexus subcommands.
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Index one or more folders (adds them to the config if not already present).
    Index {
        /// Folders to crawl and index. If omitted, re-crawls all currently configured folders.
        folders: Vec<PathBuf>,
    },

    /// Search the index.
    Search {
        /// The query string, e.g. `rust AND parser ext:rs`.
        query: String,
        /// Maximum number of results to return.
        #[arg(short, long, default_value_t = 10)]
        limit: usize,
        /// Number of results to skip (for pagination).
        #[arg(short, long, default_value_t = 0)]
        offset: usize,
        /// Print a detailed scoring breakdown for each result.
        #[arg(long)]
        explain: bool,
        /// Suppress snippet generation (faster, path/score only).
        #[arg(long)]
        no_snippets: bool,
    },

    /// Watch all indexed folders and apply incremental updates in real time.
    Watch,

    /// Fully rebuild the index from scratch using the configured folders.
    Rebuild,

    /// Show index statistics.
    Stats,

    /// Show the current configuration.
    Config,

    /// List all currently indexed folders.
    List,

    /// Add a folder to the configured set (does not index it immediately; run `index` after).
    AddFolder {
        /// The folder to add.
        folder: PathBuf,
    },

    /// Remove a folder from the configured set and drop its documents from the index.
    RemoveFolder {
        /// The folder to remove.
        folder: PathBuf,
    },

    /// Clear the entire index (does not change configured folders).
    Clear,

    /// Suggest completions for a partial search term.
    Suggest {
        /// The prefix to complete.
        prefix: String,
        /// Maximum number of suggestions.
        #[arg(short, long, default_value_t = 8)]
        limit: usize,
    },
}
