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
mod autocomplete;
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

fn main() {
    let cli = Cli::parse();
    configure_logging(cli.verbose, cli.debug);

    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(config::default_config_path);

    if let Err(err) = cli::commands::run(cli.command, &config_path) {
        eprintln!("error: {}", err);
        std::process::exit(1);
    }
}

/// Sets the verbosity used by the (very small) internal logger. Nexus
/// keeps logging intentionally lightweight: `--verbose` prints progress
/// information, `--debug` additionally enables internal diagnostics.
fn configure_logging(verbose: bool, debug: bool) {
    let level = if debug {
        "debug"
    } else if verbose {
        "info"
    } else {
        "warn"
    };
    std::env::set_var("NEXUS_LOG", level);
}
