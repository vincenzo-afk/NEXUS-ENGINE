// Several public functions (e.g. the classic TF-IDF scorer, vocabulary
// introspection helpers) are part of Nexus's public API surface for future
// consumers and are exercised by unit tests, but are not yet called from
// the CLI itself. Allow dead_code at the crate root rather than scattering
// individual #[allow] attributes across otherwise-clean modules.
#![allow(dead_code)]

//! Nexus: a fast, local, full-text desktop search engine.
//!
//! This binary is a thin wrapper around the library modules declared
//! below: it parses CLI arguments, configures logging, and dispatches to
//! the appropriate command handler in [`cli::commands`].

mod api;
mod bangs;
mod autocomplete;
mod browser;
mod cli;
mod clicks;
mod config;
mod dedup;
mod document;
mod error;
mod formats;
mod fs;
mod html;
mod index;
mod network;
mod privacy;
mod query;
mod ranking;
mod search;
mod spellcheck;
mod stats;
mod storage;
mod text;
mod watcher;
mod web;
mod webdoc;

use clap::Parser;
use cli::Cli;
use log::{debug, error, info};

fn main() {
    let cli = Cli::parse();
    configure_logging(cli.verbose, cli.debug);

    info!("Nexus search engine starting");
    debug!(
        "CLI configuration: verbose={}, debug={}, config={:?}",
        cli.verbose, cli.debug, cli.config
    );

    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(config::default_config_path);

    debug!("using config path: {}", config_path.display());

    if let Err(err) = cli::commands::run(cli.command, &config_path) {
        error!("command failed: {}", err);
        eprintln!("error: {}", err);
        std::process::exit(1);
    }

    info!("Nexus finished successfully");
}

/// Initializes the `env_logger` with the appropriate log level based on
/// CLI flags. `--debug` enables debug-level logs, `--verbose` enables
/// info-level, and the default is `warn` (only warnings and errors).
fn configure_logging(verbose: bool, debug: bool) {
    let level = if debug {
        "debug"
    } else if verbose {
        "info"
    } else {
        "warn"
    };

    std::env::set_var("RUST_LOG", level);

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level))
        .format_timestamp_millis()
        .init();

    debug!("logging initialized at level: {}", level);
}
