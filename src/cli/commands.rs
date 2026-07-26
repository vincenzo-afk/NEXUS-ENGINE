//! Implementations of each CLI subcommand.
//!
//! Each function here owns one subcommand's behavior end-to-end: loading
//! whatever state it needs, performing the operation, printing results,
//! and persisting any changes back to disk.

use crate::autocomplete::{Autocomplete, SearchHistory};
use crate::cli::Commands;
use crate::clicks::ClickLog;
use crate::config::{Config, WebCrawlConfig};
use crate::document::Document;
use crate::error::Result;
use crate::fs::{crawl_folder, CrawlOptions};
use crate::index::Index;
use crate::query;
use crate::search::snippet;
use crate::spellcheck;
use crate::stats;
use crate::storage;
use crate::storage::content_cache::ContentCache;
use crate::web::{WebCrawlOptions, WebCrawler};
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
        Commands::Crawl {
            urls,
            max_pages,
            max_depth,
            allowed_domains,
            resume,
            ignore_robots,
        } => cmd_crawl(&config, urls, max_pages, max_depth, allowed_domains, resume, ignore_robots),
        Commands::Pagerank => cmd_pagerank(&config),
        Commands::Click { query, rank } => cmd_click(&config, &query, rank),
        Commands::Serve { bind } => cmd_serve(&config, &bind),
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
        println!("index is empty; run `nexus index <folder>` or `nexus crawl <url>` first");
        return Ok(());
    }

    let clicks = ClickLog::load(&config.clicks_path).unwrap_or_default();
    let content_cache = ContentCache::new(config.content_cache_dir.clone());

    let started = Instant::now();
    let ast = query::parse(query_str)?;
    let results = crate::search::search(&index, &ast, &config.ranking, offset, limit, Some(&clicks));
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
        let web_meta = index.web.get(result.doc_id);
        let label = web_meta.map(|m| m.title.as_str()).filter(|t| !t.is_empty());
        println!(
            "{}. {}  [score {:.3}]",
            offset + rank + 1,
            label.unwrap_or_else(|| result.path.to_str().unwrap_or_default()),
            result.score
        );
        if web_meta.is_some() {
            println!("   {}", result.path.display());
        }
        println!(
            "   {} | {} matches{}",
            human_bytes(result.size_bytes),
            result.match_count,
            web_meta.map(|m| format!(" | {}", m.domain)).unwrap_or_default()
        );

        if !no_snippets {
            let terms: HashSet<String> = query::collect_terms(&ast);
            if !terms.is_empty() {
                let content = match web_meta {
                    Some(_) => content_cache.load(result.doc_id).ok(),
                    None => std::fs::read_to_string(&result.path).ok(),
                };
                if let Some(content) = content {
                    let snippet = snippet::generate_from_content(&content, &terms);
                    println!("   {}", snippet.text.replace('\n', " "));
                }
            }
        }

        if explain {
            println!(
                "   explain: bm25={:.3} filename/title={:.2} exact={:.2} recency={:.2} pagerank={:.2} url={:.2} domain={:.2} clicks={:.2}",
                result.explanation.bm25_score,
                result.explanation.filename_boost,
                result.explanation.exact_match_boost,
                result.explanation.recency_boost,
                result.explanation.pagerank_boost,
                result.explanation.url_match_boost,
                result.explanation.domain_quality_boost,
                result.explanation.click_boost,
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

fn cmd_crawl(
    config: &Config,
    urls: Vec<String>,
    max_pages: Option<usize>,
    max_depth: Option<u32>,
    allowed_domains: Vec<String>,
    resume: bool,
    ignore_robots: bool,
) -> Result<()> {
    if urls.is_empty() && !resume {
        println!("no seed URLs given; pass one or more URLs, or --resume to continue a saved crawl");
        return Ok(());
    }

    let mut web_config: WebCrawlConfig = config.web_crawl.clone();
    if let Some(n) = max_pages {
        web_config.max_pages = n;
    }
    if let Some(d) = max_depth {
        web_config.max_depth = d;
    }
    if !allowed_domains.is_empty() {
        web_config.allowed_domains = allowed_domains;
    }
    if ignore_robots {
        web_config.respect_robots = false;
        println!("warning: robots.txt checks disabled for this crawl");
    }

    let mut index = load_index(config)?;
    let content_cache = ContentCache::new(config.content_cache_dir.clone());
    let mut crawler = WebCrawler::new(web_config)?;

    println!(
        "crawling {} seed(s) (max {} pages, depth {})...",
        urls.len().max(1),
        crawler_max_pages(config, max_pages),
        crawler_max_depth(config, max_depth)
    );

    let started = Instant::now();
    let options = WebCrawlOptions {
        seeds: urls,
        queue_path: Some(config.crawl_queue_path.clone()),
    };
    let report = crawler.crawl(&mut index, &content_cache, &options)?;
    storage::save(&index, &config.index_path)?;

    println!(
        "crawl complete in {:.2}s: {} fetched, {} indexed, {} unchanged, {} duplicate, {} robots-blocked, {} new links discovered",
        started.elapsed().as_secs_f32(),
        report.pages_fetched,
        report.pages_indexed,
        report.pages_unchanged,
        report.pages_skipped_duplicate,
        report.pages_skipped_robots,
        report.links_discovered,
    );
    if report.queue_remaining > 0 {
        println!(
            "{} URL(s) remain queued; run `nexus crawl --resume` to continue",
            report.queue_remaining
        );
    }
    if !report.errors.is_empty() {
        println!("{} error(s):", report.errors.len());
        for e in report.errors.iter().take(10) {
            println!("  - {}", e);
        }
        if report.errors.len() > 10 {
            println!("  ... and {} more", report.errors.len() - 10);
        }
    }
    Ok(())
}

fn crawler_max_pages(config: &Config, override_value: Option<usize>) -> usize {
    override_value.unwrap_or(config.web_crawl.max_pages)
}

fn crawler_max_depth(config: &Config, override_value: Option<u32>) -> u32 {
    override_value.unwrap_or(config.web_crawl.max_depth)
}

fn cmd_pagerank(config: &Config) -> Result<()> {
    let mut index = load_index(config)?;
    if index.web.is_empty() {
        println!("no crawled web pages in the index; run `nexus crawl <url>` first");
        return Ok(());
    }
    let started = Instant::now();
    crate::webdoc::build_incoming_links(&mut index.web);
    crate::webdoc::pagerank::compute_and_store(&mut index.web, crate::webdoc::pagerank::DEFAULT_DAMPING);
    storage::save(&index, &config.index_path)?;

    let mut ranked: Vec<(String, f32)> = index
        .web
        .iter()
        .map(|(_, meta)| (meta.url.clone(), meta.pagerank))
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    println!(
        "recomputed PageRank for {} page(s) in {:.2}s",
        index.web.len(),
        started.elapsed().as_secs_f32()
    );
    println!("top pages by PageRank:");
    for (url, score) in ranked.iter().take(10) {
        println!("  {:.5}  {}", score, url);
    }
    Ok(())
}

fn cmd_click(config: &Config, query_str: &str, rank: usize) -> Result<()> {
    if rank == 0 {
        return Err(crate::error::NexusError::Other(
            "rank must be 1-based (the number shown by `nexus search`)".to_string(),
        ));
    }
    let index = load_index(config)?;
    let existing_clicks = ClickLog::load(&config.clicks_path).unwrap_or_default();
    let ast = query::parse(query_str)?;
    // Re-running search deterministically reproduces the same ranking the
    // user saw (absent an index change in between), so we don't need to
    // separately persist "last shown results" state just to resolve which
    // document a rank number refers to.
    let results = crate::search::search(
        &index,
        &ast,
        &config.ranking,
        0,
        rank,
        Some(&existing_clicks),
    );
    let Some(result) = results.get(rank - 1) else {
        println!("no result at rank {} for '{}'", rank, query_str);
        return Ok(());
    };

    let mut clicks = existing_clicks;
    clicks.record(result.doc_id);
    clicks.save(&config.clicks_path)?;
    println!("recorded click on: {}", result.path.display());
    Ok(())
}

fn cmd_serve(config: &Config, bind: &str) -> Result<()> {
    crate::api::serve(bind, config)
}
