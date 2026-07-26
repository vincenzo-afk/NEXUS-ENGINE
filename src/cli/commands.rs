//! Implementations of each CLI subcommand.
//!
//! Each function here owns one subcommand's behavior end-to-end: loading
//! whatever state it needs, performing the operation, printing results,
//! and persisting any changes back to disk.

use crate::autocomplete::{Autocomplete, SearchHistory};
use crate::cli::Commands;
use crate::config::Config;
use crate::document::Document;
use crate::error::Result;
use crate::fs::{crawl_folder, CrawlOptions};
use crate::index::Index;
use crate::query;
use crate::search::snippet;
use crate::spellcheck;
use crate::stats;
use crate::storage;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Loads the index from disk, or returns a fresh empty index if none has
/// been saved yet.
fn load_index(config: &Config) -> Result<Index> {
    if storage::exists(&config.index_path) {
        storage::load(&config.index_path)
    } else {
        Ok(Index::new())
    }
}

/// Returns the path used to persist recent/popular search history,
/// alongside the main index file.
fn history_path(config: &Config) -> PathBuf {
    config
        .index_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("history.nxh")
}

/// Dispatches a parsed [`Commands`] to its handler.
pub fn run(command: Commands, config_path: &Path) -> Result<()> {
    let mut config = Config::load_or_create(config_path)?;

    match command {
        Commands::Index { folders } => cmd_index(&mut config, config_path, folders),
        Commands::Search {
            query,
            limit,
            offset,
            explain,
            no_snippets,
        } => cmd_search(&config, &query, limit, offset, explain, no_snippets),
        Commands::Watch => cmd_watch(&config),
        Commands::Rebuild => cmd_rebuild(&config),
        Commands::Stats => cmd_stats(&config),
        Commands::Config => cmd_config(&config, config_path),
        Commands::List => cmd_list(&config),
        Commands::AddFolder { folder } => cmd_add_folder(&mut config, config_path, folder),
        Commands::RemoveFolder { folder } => cmd_remove_folder(&mut config, config_path, folder),
        Commands::Clear => cmd_clear(&config),
        Commands::Suggest { prefix, limit } => cmd_suggest(&config, &prefix, limit),
    }
}

fn cmd_index(config: &mut Config, config_path: &Path, folders: Vec<PathBuf>) -> Result<()> {
    let targets: Vec<PathBuf> = if folders.is_empty() {
        config.indexed_folders.clone()
    } else {
        for folder in &folders {
            if !folder.exists() {
                println!("warning: '{}' does not exist, skipping", folder.display());
                continue;
            }
            let canonical = folder.canonicalize().unwrap_or_else(|_| folder.clone());
            if config.add_folder(canonical) {
                println!("added folder to configuration: {}", folder.display());
            }
        }
        config.save(config_path)?;
        folders
    };

    if targets.is_empty() {
        println!("no folders configured; use `nexus add-folder <path>` or `nexus index <path>` first");
        return Ok(());
    }

    let mut index = load_index(config)?;
    let started = Instant::now();
    let mut total_indexed = 0usize;

    for folder in &targets {
        if !folder.exists() {
            continue;
        }
        println!("crawling {}...", folder.display());
        let options = CrawlOptions::from_config(config);
        let crawled = crawl_folder(folder, &options);

        // Parallel read + parse across all discovered files; indexing the
        // resulting Documents into the shared structures happens serially
        // afterward since the index is not internally synchronized.
        let documents: Vec<Document> = crawled
            .par_iter()
            .filter_map(|f| Document::from_crawled_file(f).ok())
            .collect();

        for doc in documents {
            index.index_document(doc);
            total_indexed += 1;
        }
    }

    storage::save(&index, &config.index_path)?;
    let elapsed = started.elapsed();
    println!(
        "indexed {} files in {:.2}s ({} total documents, {} terms)",
        total_indexed,
        elapsed.as_secs_f32(),
        index.document_count(),
        index.vocabulary.len()
    );
    Ok(())
}

fn cmd_search(
    config: &Config,
    query_str: &str,
    limit: usize,
    offset: usize,
    explain: bool,
    no_snippets: bool,
) -> Result<()> {
    let index = load_index(config)?;
    if index.document_count() == 0 {
        println!("index is empty; run `nexus index <folder>` first");
        return Ok(());
    }

    let started = Instant::now();
    let ast = query::parse(query_str)?;
    let results = crate::search::search(&index, &ast, &config.ranking, offset, limit);
    let elapsed = started.elapsed();

    if results.is_empty() {
        println!("no results for '{}'", query_str);
        offer_spelling_suggestions(&index, query_str);
        return Ok(());
    }

    println!(
        "{} result(s) in {:.2}ms\n",
        results.len(),
        elapsed.as_secs_f64() * 1000.0
    );

    for (rank, result) in results.iter().enumerate() {
        println!(
            "{}. {}  [score {:.3}]",
            offset + rank + 1,
            result.path.display(),
            result.score
        );
        println!(
            "   {} | {} matches",
            human_bytes(result.size_bytes),
            result.match_count
        );

        if !no_snippets {
            let terms: HashSet<String> = collect_query_terms(&ast);
            if !terms.is_empty() {
                if let Ok(snippet) = snippet::generate(&result.path, &terms) {
                    println!("   {}", snippet.text.replace('\n', " "));
                }
            }
        }

        if explain {
            println!(
                "   explain: bm25={:.3} filename_boost={:.2} exact_boost={:.2} recency_boost={:.2}",
                result.explanation.bm25_score,
                result.explanation.filename_boost,
                result.explanation.exact_match_boost,
                result.explanation.recency_boost
            );
        }
        println!();
    }

    // Record search history for autocomplete/popular-searches, best-effort.
    if let Ok(mut history) = SearchHistory::load(&history_path(config)) {
        history.record(query_str);
        let _ = history.save(&history_path(config));
    }

    Ok(())
}

/// Collects every literal term referenced anywhere in the query AST, for
/// snippet highlighting purposes.
fn collect_query_terms(node: &query::QueryNode) -> HashSet<String> {
    use query::QueryNode::*;
    let mut terms = HashSet::new();
    match node {
        Term(t) => {
            terms.insert(t.clone());
        }
        Phrase(ts) => terms.extend(ts.iter().cloned()),
        Prefix(p) => {
            terms.insert(p.clone());
        }
        Wildcard(w) => {
            terms.insert(w.clone());
        }
        Fuzzy { term, .. } => {
            terms.insert(term.clone());
        }
        And(children) | Or(children) => {
            for c in children {
                terms.extend(collect_query_terms(c));
            }
        }
        Not(_) => {}
        FilterExt(_) | FilterPath(_) | FilterName(_) | FilterSize(_, _) | FilterModified(_, _) => {}
    }
    terms
}

fn offer_spelling_suggestions(index: &Index, query_str: &str) {
    let normalized = crate::text::normalize(query_str);
    for word in normalized.split_whitespace() {
        let suggestions = spellcheck::suggest(word, &index.vocabulary, 3);
        if !suggestions.is_empty() {
            let names: Vec<&str> = suggestions.iter().map(|s| s.term.as_str()).collect();
            println!("did you mean: {}?", names.join(", "));
        }
    }
}

fn cmd_watch(config: &Config) -> Result<()> {
    if config.indexed_folders.is_empty() {
        println!("no folders configured to watch; use `nexus add-folder <path>` first");
        return Ok(());
    }

    let mut index = load_index(config)?;
    println!(
        "watching {} folder(s) for changes (Ctrl+C to stop)...",
        config.indexed_folders.len()
    );

    let (_watcher, rx) = crate::watcher::start_watching(config)?;

    loop {
        match rx.recv() {
            Ok(event) => {
                if let Some(summary) = crate::watcher::apply_event(&mut index, config, &event) {
                    println!("{}", summary);
                    if let Err(e) = storage::save(&index, &config.index_path) {
                        eprintln!("warning: failed to persist index update: {}", e);
                    }
                }
            }
            Err(_) => break,
        }
    }
    Ok(())
}

fn cmd_rebuild(config: &Config) -> Result<()> {
    println!("rebuilding index from {} configured folder(s)...", config.indexed_folders.len());
    let started = Instant::now();
    let mut index = Index::new();

    for folder in &config.indexed_folders {
        if !folder.exists() {
            println!("warning: '{}' no longer exists, skipping", folder.display());
            continue;
        }
        let options = CrawlOptions::from_config(config);
        let crawled = crawl_folder(folder, &options);
        let documents: Vec<Document> = crawled
            .par_iter()
            .filter_map(|f| Document::from_crawled_file(f).ok())
            .collect();
        for doc in documents {
            index.index_document(doc);
        }
    }

    storage::save(&index, &config.index_path)?;
    println!(
        "rebuild complete: {} documents, {} terms, in {:.2}s",
        index.document_count(),
        index.vocabulary.len(),
        started.elapsed().as_secs_f32()
    );
    Ok(())
}

fn cmd_stats(config: &Config) -> Result<()> {
    let index = load_index(config)?;
    let computed = stats::compute(&index, config);
    print!("{}", stats::format_report(&computed));
    Ok(())
}

fn cmd_config(config: &Config, config_path: &Path) -> Result<()> {
    println!("configuration file: {}", config_path.display());
    println!("{}", toml::to_string_pretty(config).unwrap_or_default());
    Ok(())
}

fn cmd_list(config: &Config) -> Result<()> {
    if config.indexed_folders.is_empty() {
        println!("no folders configured");
        return Ok(());
    }
    for folder in &config.indexed_folders {
        let marker = if stats::folder_exists(folder) { "" } else { " (missing)" };
        println!("{}{}", folder.display(), marker);
    }
    Ok(())
}

fn cmd_add_folder(config: &mut Config, config_path: &Path, folder: PathBuf) -> Result<()> {
    if !folder.exists() {
        return Err(crate::error::NexusError::PathNotFound(folder));
    }
    let canonical = folder.canonicalize().unwrap_or(folder);
    if config.add_folder(canonical.clone()) {
        config.save(config_path)?;
        println!("added: {}", canonical.display());
        println!("run `nexus index` to crawl it");
    } else {
        println!("already configured: {}", canonical.display());
    }
    Ok(())
}

fn cmd_remove_folder(config: &mut Config, config_path: &Path, folder: PathBuf) -> Result<()> {
    let canonical = folder.canonicalize().unwrap_or(folder.clone());
    let removed_from_config = config.remove_folder(&canonical) || config.remove_folder(&folder);
    if !removed_from_config {
        return Err(crate::error::NexusError::FolderNotIndexed(folder));
    }
    config.save(config_path)?;

    // Also drop any already-indexed documents under this folder.
    let mut index = load_index(config)?;
    let paths_to_remove: Vec<PathBuf> = index
        .store
        .iter()
        .filter(|(_, meta)| meta.path.starts_with(&canonical) || meta.path.starts_with(&folder))
        .map(|(_, meta)| meta.path.clone())
        .collect();
    for path in &paths_to_remove {
        index.remove_by_path(path);
    }
    storage::save(&index, &config.index_path)?;

    println!(
        "removed folder from configuration and dropped {} indexed document(s)",
        paths_to_remove.len()
    );
    Ok(())
}

fn cmd_clear(config: &Config) -> Result<()> {
    let index = Index::new();
    storage::save(&index, &config.index_path)?;
    println!("index cleared");
    Ok(())
}

fn cmd_suggest(config: &Config, prefix: &str, limit: usize) -> Result<()> {
    let index = load_index(config)?;

    let mut doc_frequencies: HashMap<String, u32> = HashMap::new();
    for (term, term_id) in index.vocabulary.iter() {
        if let Some(list) = index.inverted.postings_for(term_id) {
            doc_frequencies.insert(term.to_string(), list.document_frequency() as u32);
        }
    }

    let autocomplete = Autocomplete::build(&index.vocabulary, &doc_frequencies);
    let suggestions = autocomplete.suggest(prefix, limit);

    if suggestions.is_empty() {
        println!("no suggestions for '{}'", prefix);
    } else {
        for s in &suggestions {
            println!("{}", s);
        }
    }

    if let Ok(history) = SearchHistory::load(&history_path(config)) {
        let recent = history.recent(5);
        if !recent.is_empty() {
            println!("\nrecent searches:");
            for r in recent {
                println!("  {}", r);
            }
        }
    }

    Ok(())
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit_idx = 0;
    while value >= 1024.0 && unit_idx < UNITS.len() - 1 {
        value /= 1024.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{} {}", bytes, UNITS[unit_idx])
    } else {
        format!("{:.1} {}", value, UNITS[unit_idx])
    }
}
