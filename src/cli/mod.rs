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
        /// Which subset of the index to search: local, web, both (hybrid),
        /// or tor (.onion only). Defaults to web.
        #[arg(long, default_value = "web")]
        mode: String,
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

    /// Crawl one or more web pages (and everything linked from them,
    /// within the configured budget) and index their content.
    Crawl {
        /// Seed URLs to start crawling from.
        urls: Vec<String>,
        /// Maximum number of pages to fetch in this run (overrides config).
        #[arg(long)]
        max_pages: Option<usize>,
        /// Maximum link-following depth from a seed (overrides config).
        #[arg(long)]
        max_depth: Option<u32>,
        /// Restrict crawling to these registrable domains (and subdomains).
        /// May be given multiple times. Overrides config if provided.
        #[arg(long = "domain")]
        allowed_domains: Vec<String>,
        /// Resume a previously interrupted crawl from the saved queue
        /// instead of starting fresh.
        #[arg(long)]
        resume: bool,
        /// Ignore robots.txt (not recommended; only for infrastructure you control).
        #[arg(long)]
        ignore_robots: bool,
        /// Keep running, re-crawling the same seeds repeatedly on a fixed
        /// interval, instead of exiting after one pass. Intended to be
        /// run under a process supervisor (systemd, a container restart
        /// policy, `pm2`, etc.) for continuous/scheduled crawling — this
        /// flag makes the process itself loop, it does not daemonize or
        /// install a system-level schedule on its own.
        #[arg(long)]
        watch: bool,
        /// Interval, in seconds, between crawl passes when `--watch` is
        /// set. Defaults to 1 hour, a reasonable default for keeping a
        /// news/blog site's RSS-discovered content fresh without hammering it.
        #[arg(long, default_value_t = 3600)]
        interval: u64,
    },

    /// Recompute PageRank scores over the crawled link graph.
    Pagerank,

    /// Record that a search result was clicked, feeding the click-history
    /// ranking signal. Intended to be called by a browser extension or
    /// UI; exposed here so the signal is exercised end-to-end.
    Click {
        /// The query that was run.
        query: String,
        /// The 1-based rank of the result that was clicked, as shown by `nexus search`.
        rank: usize,
    },

    /// Start the HTTP search API server (`GET /search?q=...`).
    Serve {
        /// Address to bind to.
        #[arg(long, default_value = "127.0.0.1:8080")]
        bind: String,
    },

    /// Start the WebSocket search-as-you-type server.
    ServeWs {
        /// Address to bind to.
        #[arg(long, default_value = "127.0.0.1:8081")]
        bind: String,
    },

    /// Configure Tor proxy for private crawling.
    Tor {
        /// Enable Tor proxy.
        #[arg(long)]
        enable: bool,
        /// Disable Tor proxy.
        #[arg(long)]
        disable: bool,
        /// Tor SOCKS5 proxy address (host:port).
        #[arg(long)]
        proxy: Option<String>,
        /// Check if Tor is reachable.
        #[arg(long)]
        check: bool,
    },

    /// Run performance benchmarks.
    Benchmark {
        /// Number of search iterations.
        #[arg(long, default_value_t = 100)]
        iterations: usize,
        /// File with queries (one per line) to benchmark.
        #[arg(long)]
        queries: Option<PathBuf>,
    },

    /// Show privacy policy information.
    PrivacyPolicy,
}
