//! Implementations of each CLI subcommand.
//!
//! Each function here owns one subcommand's behavior end-to-end: loading
//! whatever state it needs, performing the operation, printing results,
//! and persisting any changes back to disk.

use crate::api::rate_limit::{RateLimitConfig, RateLimiter};
use crate::autocomplete::{Autocomplete, SearchHistory};
use crate::cli::Commands;
use crate::clicks::ClickLog;
use crate::config::{Config, WebCrawlConfig};
use crate::document::Document;
use crate::error::{NexusError, Result};
use crate::fs::{crawl_folder, CrawlOptions};
use crate::index::Index;
use crate::network;
use crate::query;
use crate::search::snippet;
use crate::spellcheck;
use crate::stats;
use crate::storage;
use crate::storage::content_cache::ContentCache;
use crate::web::{WebCrawlOptions, WebCrawler};
use log::{debug, info, warn};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Loads the index from disk, or returns a fresh empty index if none has
/// been saved yet.
fn load_index(config: &Config) -> Result<Index> {
    if storage::exists(&config.index_path) {
        info!("loading index from {}", config.index_path.display());
        let index = storage::load(&config.index_path)?;
        info!(
            "index loaded: {} documents, {} terms",
            index.document_count(),
            index.vocabulary.len()
        );
        Ok(index)
    } else {
        info!("no existing index found, creating new empty index");
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
    info!("loading config from {}", config_path.display());
    let mut config = Config::load_or_create(config_path)?;
    debug!(
        "config loaded: {} indexed folders",
        config.indexed_folders.len()
    );

    match command {
        Commands::Index { folders } => cmd_index(&mut config, config_path, folders),
        Commands::Search {
            query,
            limit,
            offset,
            explain,
            no_snippets,
            mode,
        } => cmd_search(&config, &query, limit, offset, explain, no_snippets, &mode),
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
            seed_file,
            max_pages,
            max_depth,
            allowed_domains,
            resume,
            ignore_robots,
            watch,
            interval,
        } => cmd_crawl(
            &config,
            urls,
            seed_file,
            max_pages,
            max_depth,
            allowed_domains,
            resume,
            ignore_robots,
            watch,
            interval,
        ),
        Commands::Pagerank => cmd_pagerank(&config),
        Commands::Click { query, rank, mode } => cmd_click(&config, &query, rank, &mode),
        Commands::Related { name, limit } => cmd_related(&config, &name, limit),
        Commands::TopEntities { limit } => cmd_top_entities(&config, limit),
        Commands::ExportEncrypted { output, passphrase } => cmd_export_encrypted(&config, &output, &passphrase),
        Commands::SearchEncrypted { index_path, passphrase, terms } => {
            cmd_search_encrypted(&index_path, &passphrase, &terms)
        }
        Commands::Serve { bind } => cmd_serve(&config, &bind),
        Commands::Ask { query, mode, rerank } => cmd_ask(&config, &query, &mode, rerank),
        Commands::ServeWs { bind } => cmd_serve_ws(&config, &bind),
        Commands::Tor {
            enable,
            disable,
            proxy,
            check,
        } => cmd_tor(&config, enable, disable, proxy, check),
        Commands::Benchmark {
            iterations,
            queries,
        } => cmd_benchmark(&config, iterations, queries),
        Commands::PrivacyPolicy => cmd_privacy_policy(&config),
    }
}

fn cmd_index(config: &mut Config, config_path: &Path, folders: Vec<PathBuf>) -> Result<()> {
    info!("index command: folders={:?}", folders);
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
        println!(
            "no folders configured; use `nexus add-folder <path>` or `nexus index <path>` first"
        );
        return Ok(());
    }

    let mut index = load_index(config)?;
    let started = Instant::now();
    let mut total_indexed = 0usize;

    for folder in &targets {
        if !folder.exists() {
            warn!("folder does not exist, skipping: {}", folder.display());
            continue;
        }
        info!("crawling folder: {}", folder.display());
        let options = CrawlOptions::from_config(config);
        let crawled = crawl_folder(folder, &options);
        debug!("found {} files in {}", crawled.len(), folder.display());

        // Parallel read + parse across all discovered files; indexing the
        // resulting Documents into the shared structures happens serially
        // afterward since the index is not internally synchronized.
        let documents: Vec<Document> = crawled
            .par_iter()
            .filter_map(|f| Document::from_crawled_file(f).ok())
            .collect();

        debug!(
            "parsed {} documents from {}",
            documents.len(),
            folder.display()
        );

        for doc in documents {
            index.index_document(doc);
            total_indexed += 1;
        }
    }

    debug!(
        "saving index to {} ({} docs, {} terms)",
        config.index_path.display(),
        index.document_count(),
        index.vocabulary.len()
    );
    storage::save(&index, &config.index_path)?;
    let elapsed = started.elapsed();
    info!(
        "indexed {} files in {:.2}s (total documents: {}, terms: {})",
        total_indexed,
        elapsed.as_secs_f32(),
        index.document_count(),
        index.vocabulary.len()
    );
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
    mode_str: &str,
) -> Result<()> {
    debug!(
        "search command: query='{}', limit={}, offset={}, mode={}",
        query_str, limit, offset, mode_str
    );
    let mode = crate::search::SearchMode::from_query_param(mode_str);

    let bang_table = crate::bangs::BangTable::from_builtin();
    if let Some(bang_match) = bang_table.resolve(query_str) {
        println!("!bang matched: {} -> {}", bang_match.name, bang_match.url);
        if bang_match.remaining_query.is_empty() {
            println!("(open the URL above in a browser)");
        } else {
            println!("(query was: \"{}\")", bang_match.remaining_query);
        }
        return Ok(());
    }

    let index = load_index(config)?;
    if index.document_count() == 0 {
        println!("index is empty; run `nexus index <folder>` or `nexus crawl <url>` first");
        return Ok(());
    }

    if mode == crate::search::SearchMode::Tor && index.web.iter().all(|(_, m)| !m.url.to_lowercase().contains(".onion")) {
        println!("Tor mode: no .onion pages are currently indexed.");
        println!("Crawl some first (e.g. `nexus crawl <onion-url> --ignore-robots` over a Tor proxy), then search again.");
        return Ok(());
    }

    let clicks = ClickLog::load(&config.clicks_path).unwrap_or_default();
    let content_cache = ContentCache::new(config.content_cache_dir.clone());

    let personal_model = crate::ranking::adaptive::PersonalRankingModel::load(&config.personal_model_path)
        .unwrap_or_default();
    let frequently_clicked_kind = crate::ranking::adaptive::most_frequently_clicked_kind(&clicks, &index);
    let personalization = crate::search::Personalization {
        model: &personal_model,
        frequently_clicked_kind,
    };

    let started = Instant::now();
    let outcome = crate::search::search_with_federation(
        &index,
        query_str,
        &config.ranking,
        offset,
        limit,
        Some(&clicks),
        mode,
        Some(&personalization),
        &config.federation,
        Some(&crate::entity::SearchContext::current_os()),
    )?;
    if let Some(federated) = &outcome.federated_results {
        let federated_count = federated.iter().filter(|e| e.source == crate::entity::SourceKind::Federated).count();
        if federated_count > 0 {
            println!(
                "\u{1F310} including {} result(s) from {} peer(s)\n",
                federated_count,
                config.federation.peers.iter().filter(|p| p.enabled).count()
            );
        }
    }
    if let Some(answer) = &outcome.instant_answer {
        println!("\u{1F4A1} {}\n", answer);
    }
    let had_instant_answer = outcome.instant_answer.is_some();
    let results = outcome.results;
    let elapsed = started.elapsed();
    debug!(
        "search returned {} results in {:.2}ms",
        results.len(),
        elapsed.as_secs_f64() * 1000.0
    );

    if results.is_empty() {
        if had_instant_answer {
            // The instant-answer card above already answered the query
            // ("2 + 2 = 4") — printing "no results, did you mean...?"
            // underneath it would be actively confusing, not just
            // redundant, since there's nothing to have misspelled.
            return Ok(());
        }
        info!("no results for query '{}'", query_str);
        println!("no results for '{}'", query_str);
        offer_spelling_suggestions(&index, query_str);
        return Ok(());
    }

    println!(
        "{} result(s) in {:.2}ms ({} local, {} web)\n",
        outcome.total,
        elapsed.as_secs_f64() * 1000.0,
        outcome.local_count,
        outcome.web_count,
    );

    for (rank, result) in results.iter().enumerate() {
        let web_meta = index.web.get(result.doc_id);
        let badge = if result.is_onion {
            "\u{1F9C5}" // onion
        } else if result.is_web {
            "\u{1F310}" // globe (web)
        } else {
            "\u{1F4BB}" // laptop (local)
        };
        let label = web_meta.map(|m| m.title.as_str()).filter(|t| !t.is_empty());
        println!(
            "{}. {} {}  [score {:.3}]",
            offset + rank + 1,
            badge,
            label.unwrap_or_else(|| result.path.to_str().unwrap_or_default()),
            result.score
        );
        if web_meta.is_some() {
            println!("   {}", result.path.display());
        } else {
            println!("   \u{1F512} {} (local file, nothing leaves your machine)", result.path.display());
        }
        println!(
            "   {} | {} matches{}",
            human_bytes(result.size_bytes),
            result.match_count,
            web_meta
                .map(|m| format!(" | {}", m.domain))
                .unwrap_or_default()
        );

        if !no_snippets {
            let terms: HashSet<String> = query::parse(query_str)
                .map(|ast| query::collect_terms(&ast))
                .unwrap_or_default();
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
            println!("   why this ranked here:");
            for reason in result.explanation.reasons() {
                if reason.impact_percent == 0.0 {
                    println!("     - {}: {}", reason.label, reason.detail);
                } else {
                    println!(
                        "     - {} ({:+.0}%): {}",
                        reason.label, reason.impact_percent, reason.detail
                    );
                }
            }
            println!("     final score: {:.3}", result.explanation.final_score);
        }
        println!();
    }

    // Record exactly which document was shown at each rank, so a later
    // `nexus click <query> <rank>` (same query, same mode) resolves the
    // rank directly instead of recomputing the search and risking a
    // click/rank race if something (another click, the index) changed
    // in between — see `clicks::last_shown`'s module doc comment.
    // Best-effort: a failure to persist this just means `nexus click`
    // falls back to its own recompute path, not a reason to fail the
    // search that already succeeded and was already printed above.
    let shown = crate::clicks::LastShownResults::from_shown_page(
        query_str,
        mode_str,
        offset,
        results.iter().map(|r| r.doc_id),
    );
    if let Err(e) = shown.save(&config.last_search_path) {
        debug!("failed to persist last-shown-results state: {}", e);
    }

    // Record search history for autocomplete/popular-searches, best-effort.
    if let Ok(mut history) = SearchHistory::load(&history_path(config)) {
        history.record(query_str);
        let _ = history.save(&history_path(config));
    }

    Ok(())
}

fn offer_spelling_suggestions(index: &Index, query_str: &str) {
    debug!("checking spelling suggestions for '{}'", query_str);
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
        warn!("no folders configured to watch");
        println!("no folders configured to watch; use `nexus add-folder <path>` first");
        return Ok(());
    }

    let mut index = load_index(config)?;
    info!(
        "watching {} folder(s) for changes",
        config.indexed_folders.len()
    );
    println!(
        "watching {} folder(s) for changes (Ctrl+C to stop)...",
        config.indexed_folders.len()
    );

    let (_watcher, rx) = crate::watcher::start_watching(config)?;

    loop {
        match rx.recv() {
            Ok(event) => {
                debug!("filesystem event received: {:?}", event);
                if let Some(summary) = crate::watcher::apply_event(&mut index, config, &event) {
                    info!("watcher: {}", summary);
                    println!("{}", summary);
                    if let Err(e) = storage::save(&index, &config.index_path) {
                        warn!("failed to persist index update: {}", e);
                        eprintln!("warning: failed to persist index update: {}", e);
                    }
                }
            }
            Err(_) => {
                info!("watcher channel closed, stopping");
                break;
            }
        }
    }
    Ok(())
}

fn cmd_rebuild(config: &Config) -> Result<()> {
    info!(
        "rebuilding index from {} configured folder(s)",
        config.indexed_folders.len()
    );
    println!(
        "rebuilding index from {} configured folder(s)...",
        config.indexed_folders.len()
    );
    let started = Instant::now();
    let mut index = Index::new();

    for folder in &config.indexed_folders {
        if !folder.exists() {
            warn!("folder no longer exists, skipping: {}", folder.display());
            println!("warning: '{}' no longer exists, skipping", folder.display());
            continue;
        }
        debug!("rebuild: crawling {}", folder.display());
        let options = CrawlOptions::from_config(config);
        let crawled = crawl_folder(folder, &options);
        let documents: Vec<Document> = crawled
            .par_iter()
            .filter_map(|f| Document::from_crawled_file(f).ok())
            .collect();
        debug!(
            "rebuild: {} documents from {}",
            documents.len(),
            folder.display()
        );
        for doc in documents {
            index.index_document(doc);
        }
    }

    debug!("rebuild: saving index to {}", config.index_path.display());
    storage::save(&index, &config.index_path)?;
    info!(
        "rebuild complete: {} documents, {} terms, in {:.2}s",
        index.document_count(),
        index.vocabulary.len(),
        started.elapsed().as_secs_f32()
    );
    println!(
        "rebuild complete: {} documents, {} terms, in {:.2}s",
        index.document_count(),
        index.vocabulary.len(),
        started.elapsed().as_secs_f32()
    );
    Ok(())
}

fn cmd_stats(config: &Config) -> Result<()> {
    debug!("stats command");
    let index = load_index(config)?;
    let computed = stats::compute(&index, config);
    info!("index statistics computed");
    print!("{}", stats::format_report(&computed));
    Ok(())
}

fn cmd_config(config: &Config, config_path: &Path) -> Result<()> {
    debug!("config command showing configuration");
    println!("configuration file: {}", config_path.display());
    println!("{}", toml::to_string_pretty(config).unwrap_or_default());
    Ok(())
}

fn cmd_list(config: &Config) -> Result<()> {
    debug!("list command showing indexed folders");
    if config.indexed_folders.is_empty() {
        info!("no folders configured");
        println!("no folders configured");
        return Ok(());
    }
    for folder in &config.indexed_folders {
        let marker = if stats::folder_exists(folder) {
            ""
        } else {
            " (missing)"
        };
        println!("{}{}", folder.display(), marker);
    }
    Ok(())
}

fn cmd_add_folder(config: &mut Config, config_path: &Path, folder: PathBuf) -> Result<()> {
    debug!("add-folder command: {}", folder.display());
    if !folder.exists() {
        warn!("folder does not exist: {}", folder.display());
        return Err(crate::error::NexusError::PathNotFound(folder));
    }
    let canonical = folder.canonicalize().unwrap_or(folder);
    if config.add_folder(canonical.clone()) {
        info!("added folder: {}", canonical.display());
        config.save(config_path)?;
        println!("added: {}", canonical.display());
        println!("run `nexus index` to crawl it");
    } else {
        info!("folder already configured: {}", canonical.display());
        println!("already configured: {}", canonical.display());
    }
    Ok(())
}

fn cmd_remove_folder(config: &mut Config, config_path: &Path, folder: PathBuf) -> Result<()> {
    debug!("remove-folder command: {}", folder.display());
    let canonical = folder.canonicalize().unwrap_or(folder.clone());
    let removed_from_config = config.remove_folder(&canonical) || config.remove_folder(&folder);
    if !removed_from_config {
        warn!("folder not indexed: {}", folder.display());
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
    info!(
        "removing {} indexed documents from folder",
        paths_to_remove.len()
    );
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
    info!("clearing index");
    let index = Index::new();
    storage::save(&index, &config.index_path)?;
    info!("index cleared successfully");
    println!("index cleared");
    Ok(())
}

fn cmd_suggest(config: &Config, prefix: &str, limit: usize) -> Result<()> {
    debug!("suggest command: prefix='{}', limit={}", prefix, limit);
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
    seed_file: Option<std::path::PathBuf>,
    max_pages: Option<usize>,
    max_depth: Option<u32>,
    allowed_domains: Vec<String>,
    resume: bool,
    ignore_robots: bool,
    watch: bool,
    interval: u64,
) -> Result<()> {
    let mut urls = urls;
    if let Some(path) = &seed_file {
        let seeds_from_file = load_seed_file(path)?;
        info!("loaded {} seed URL(s) from {}", seeds_from_file.len(), path.display());
        urls.extend(seeds_from_file);
    }
    // A URL given directly *and* present in the seed file (or repeated
    // within the seed file itself) shouldn't be fetched twice —
    // `WebCrawlQueue` also dedups across the life of the crawl, but
    // deduping the seed list itself keeps `--watch` mode's per-pass log
    // output ("N seeds") honest, order-preserving so the first
    // occurrence (command-line before file, file in the order it's
    // listed) is what's kept.
    let mut seen = std::collections::HashSet::new();
    urls.retain(|u| seen.insert(u.clone()));

    info!(
        "crawl command: urls={:?}, max_pages={:?}, max_depth={:?}, resume={}, ignore_robots={}, watch={}, interval={}s",
        urls, max_pages, max_depth, resume, ignore_robots, watch, interval
    );

    if urls.is_empty() && !resume {
        warn!("no seed URLs and not resuming");
        println!(
            "no seed URLs given; pass one or more URLs, or --resume to continue a saved crawl"
        );
        return Ok(());
    }

    if watch {
        if urls.is_empty() {
            println!(
                "--watch requires at least one seed URL (--resume alone has nothing to re-crawl on each pass)"
            );
            return Ok(());
        }
        println!(
            "watch mode: re-crawling {} seed(s) every {}s. Press Ctrl+C to stop.",
            urls.len(),
            interval
        );
        println!(
            "(this loops the current process; run it under a supervisor like systemd for unattended, restart-on-crash scheduling)"
        );
        loop {
            if let Err(e) = run_crawl_pass(
                config,
                urls.clone(),
                max_pages,
                max_depth,
                allowed_domains.clone(),
                ignore_robots,
            ) {
                warn!("crawl pass failed: {}", e);
                println!("crawl pass failed: {} (will retry next interval)", e);
            }
            println!("next crawl pass in {}s...\n", interval);
            std::thread::sleep(Duration::from_secs(interval));
        }
    } else {
        run_crawl_pass(config, urls, max_pages, max_depth, allowed_domains, ignore_robots)
    }
}

/// Runs one crawl pass to completion. Separated from [`cmd_crawl`] so
/// `--watch` mode can call it repeatedly on an interval without
/// duplicating the setup/reporting logic.
/// Reads seed URLs from a plain-text file for `nexus crawl --seed-file`:
/// one URL per line, blank lines and lines starting with `#` ignored (so
/// a curated seed list can carry comments — e.g. `# news sites` grouping
/// a block of URLs — without those lines being treated as malformed
/// URLs). A line that isn't blank, isn't a comment, but still doesn't
/// look like a URL (no `://`) is skipped with a warning rather than
/// aborting the whole crawl over one bad line in an otherwise-good
/// seed list — a large hand-maintained file is exactly the kind of
/// input likely to have one stray typo in it somewhere.
fn load_seed_file(path: &std::path::Path) -> Result<Vec<String>> {
    let content = std::fs::read_to_string(path).map_err(|e| crate::error::NexusError::io(path, e))?;
    let mut urls = Vec::new();
    for (line_no, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !line.contains("://") {
            warn!(
                "{}:{}: skipping seed line that doesn't look like a URL: {:?}",
                path.display(),
                line_no + 1,
                line
            );
            continue;
        }
        urls.push(line.to_string());
    }
    Ok(urls)
}

fn run_crawl_pass(
    config: &Config,
    urls: Vec<String>,
    max_pages: Option<usize>,
    max_depth: Option<u32>,
    allowed_domains: Vec<String>,
    ignore_robots: bool,
) -> Result<()> {
    let mut web_config: WebCrawlConfig = config.web_crawl.clone();
    if let Some(n) = max_pages {
        web_config.max_pages = n;
        debug!("override max_pages={}", n);
    }
    if let Some(d) = max_depth {
        web_config.max_depth = d;
        debug!("override max_depth={}", d);
    }
    if !allowed_domains.is_empty() {
        web_config.allowed_domains = allowed_domains;
        debug!("override allowed_domains");
    }
    if ignore_robots {
        web_config.respect_robots = false;
        warn!("robots.txt checks disabled for this crawl");
        println!("warning: robots.txt checks disabled for this crawl");
    }

    let mut index = load_index(config)?;
    let content_cache = ContentCache::new(config.content_cache_dir.clone());
    let tor_config = if config.tor.enabled {
        Some(network::tor::TorConfig {
            enabled: true,
            proxy_addr: format!("{}:{}", config.tor.proxy_host, config.tor.proxy_port)
                .parse()
                .unwrap_or_else(|_| "127.0.0.1:9050".parse().unwrap()),
            identity_rotation_minutes: config.tor.identity_rotation_minutes,
            ..Default::default()
        })
    } else {
        None
    };
    let mut crawler = WebCrawler::with_tor(web_config, tor_config)?;

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
    debug!("crawl completed, saving index");
    storage::save(&index, &config.index_path)?;

    info!(
        "crawl complete in {:.2}s: {} fetched, {} indexed, {} unchanged, {} duplicate, {} robots-blocked, {} new links",
        started.elapsed().as_secs_f32(),
        report.pages_fetched,
        report.pages_indexed,
        report.pages_unchanged,
        report.pages_skipped_duplicate,
        report.pages_skipped_robots,
        report.links_discovered,
    );

    println!(
        "crawl complete in {:.2}s: {} fetched, {} indexed, {} unchanged, {} duplicate, {} robots-blocked, {} domain-budget-blocked, {} feeds discovered, {} new links discovered",
        started.elapsed().as_secs_f32(),
        report.pages_fetched,
        report.pages_indexed,
        report.pages_unchanged,
        report.pages_skipped_duplicate,
        report.pages_skipped_robots,
        report.pages_skipped_domain_budget,
        report.feeds_discovered,
        report.links_discovered,
    );
    if report.queue_remaining > 0 {
        info!("{} URL(s) remain queued", report.queue_remaining);
        println!(
            "{} URL(s) remain queued; run `nexus crawl --resume` to continue",
            report.queue_remaining
        );
    }
    if !report.errors.is_empty() {
        warn!("{} crawl error(s)", report.errors.len());
        println!("{} error(s):", report.errors.len());
        for e in report.errors.iter().take(10) {
            warn!("crawl error: {}", e);
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
    info!("PageRank command");
    let mut index = load_index(config)?;
    if index.web.is_empty() {
        warn!("no crawled web pages to compute PageRank");
        println!("no crawled web pages in the index; run `nexus crawl <url>` first");
        return Ok(());
    }
    let started = Instant::now();
    info!(
        "building incoming links graph for {} pages",
        index.web.len()
    );
    crate::webdoc::build_incoming_links(&mut index.web);
    info!("computing PageRank");
    crate::webdoc::pagerank::compute_and_store(
        &mut index.web,
        crate::webdoc::pagerank::DEFAULT_DAMPING,
    );
    storage::save(&index, &config.index_path)?;

    let mut ranked: Vec<(String, f32)> = index
        .web
        .iter()
        .map(|(_, meta)| (meta.url.clone(), meta.pagerank))
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    info!(
        "PageRank computed for {} page(s) in {:.2}s",
        index.web.len(),
        started.elapsed().as_secs_f32()
    );
    println!(
        "recomputed PageRank for {} page(s) in {:.2}s",
        index.web.len(),
        started.elapsed().as_secs_f32()
    );
    println!("top pages by PageRank:");
    for (url, score) in ranked.iter().take(10) {
        info!("PageRank top: {:.5}  {}", score, url);
        println!("  {:.5}  {}", score, url);
    }
    Ok(())
}

fn cmd_click(config: &Config, query_str: &str, rank: usize, mode_str: &str) -> Result<()> {
    debug!("click command: query='{}', rank={}, mode={}", query_str, rank, mode_str);
    if rank == 0 {
        return Err(crate::error::NexusError::Other(
            "rank must be 1-based (the number shown by `nexus search`)".to_string(),
        ));
    }
    let index = load_index(config)?;
    let existing_clicks = ClickLog::load(&config.clicks_path).unwrap_or_default();
    let mut personal_model =
        crate::ranking::adaptive::PersonalRankingModel::load(&config.personal_model_path).unwrap_or_default();
    let frequently_clicked_kind =
        crate::ranking::adaptive::most_frequently_clicked_kind(&existing_clicks, &index);
    let personalization = crate::search::Personalization {
        model: &personal_model,
        frequently_clicked_kind,
    };

    // Ground truth first: if `nexus search` for this *exact* query/mode
    // ran recently, it recorded exactly which document appeared at each
    // rank (see `clicks::last_shown`'s module doc comment for why that's
    // more trustworthy than recomputing). Only fall back to recomputing
    // the search below when there's no matching, fresh record — e.g.
    // `nexus click` run without a preceding `nexus search`, or against a
    // different query/mode than was last searched.
    let persisted_doc_id = crate::clicks::LastShownResults::load(&config.last_search_path)
        .and_then(|shown| shown.resolve(query_str, mode_str, rank));

    let mode = crate::search::SearchMode::from_query_param(mode_str);
    let ast = query::parse(query_str)?;
    // Re-running search still happens regardless of whether
    // `persisted_doc_id` resolved: the personal-model training step below
    // needs feature vectors for every result *up to* `rank`, and
    // recomputing is a fine approximation for that context even when it
    // isn't trusted for "which exact document was clicked" anymore.
    let mut outcome = crate::search::search(
        &index,
        &ast,
        &config.ranking,
        0,
        rank,
        Some(&existing_clicks),
        mode,
        Some(&crate::entity::SearchContext::current_os()),
    );
    let features_by_doc = crate::search::compute_result_features(&outcome.results, &index, &personalization);
    crate::search::apply_personal_boost(&mut outcome.results, &index, &personalization);
    let results = outcome.results;

    let clicked_doc_id = match persisted_doc_id {
        Some(doc_id) => doc_id,
        None => {
            debug!("no persisted last-shown-results match for this query/mode; resolving rank via recomputed search");
            let Some(result) = results.get(rank - 1) else {
                warn!("no result at rank {} for '{}'", rank, query_str);
                println!("no result at rank {} for '{}'", rank, query_str);
                return Ok(());
            };
            result.doc_id
        }
    };

    let mut clicks = existing_clicks;
    clicks.record(clicked_doc_id);
    clicks.save(&config.clicks_path)?;

    // Train from the actually-displayed (post-boost) order: everything
    // ranked above the click was seen and skipped, the clicked result is
    // the positive example — see `PersonalRankingModel::train_from_result_list`.
    // This uses the recomputed `results` list as training *context* (an
    // approximation of what was shown above the click), not as the
    // source of truth for which document was clicked — that's
    // `clicked_doc_id`, already resolved above.
    let shown_in_rank_order: Vec<crate::ranking::adaptive::Features> = results
        .iter()
        .take(rank)
        .filter_map(|r| features_by_doc.get(&r.doc_id).copied())
        .collect();
    if shown_in_rank_order.len() == rank {
        personal_model.train_from_result_list(&shown_in_rank_order, Some(rank - 1));
        personal_model.save(&config.personal_model_path)?;
        debug!(
            "personal ranking model updated: {} example(s) seen",
            personal_model.examples_seen()
        );
    } else {
        warn!("could not resolve features for every shown result; skipping personal model update this time");
    }

    match index.store.get(clicked_doc_id) {
        Some(meta) => {
            info!("recorded click on doc_id={}: {}", clicked_doc_id, meta.path.display());
            println!("recorded click on: {}", meta.path.display());
        }
        None => {
            // Can happen if the persisted record is from before the last
            // re-index and the document's `doc_id` no longer exists —
            // the click is still recorded against that ID (harmless: a
            // stray count for a since-removed document), just nothing
            // sensible to print as a path.
            info!("recorded click on doc_id={} (document no longer in the index)", clicked_doc_id);
            println!("recorded click on doc_id={}", clicked_doc_id);
        }
    }
    Ok(())
}

fn cmd_export_encrypted(config: &Config, output: &std::path::Path, passphrase: &str) -> Result<()> {
    if passphrase.len() < 16 {
        return Err(crate::error::NexusError::Other(
            "passphrase must be at least 16 bytes (used directly as key material via HKDF — a \
             short passphrase means a weak, guessable key regardless of the encryption algorithm's \
             own strength)"
                .to_string(),
        ));
    }
    let index = load_index(config)?;
    let key = crate::crypto_index::IndexKey::derive(passphrase.as_bytes());
    info!("encrypting {} vocabulary terms...", index.vocabulary.len());
    let encrypted = crate::crypto_index::build_from_index(&index, &key)
        .map_err(|e| crate::error::NexusError::Other(format!("encryption failed: {e}")))?;
    encrypted.save(output)?;
    println!(
        "exported encrypted index with {} term(s) to {}",
        encrypted.term_count(),
        output.display()
    );
    println!("safe to sync this file anywhere — it contains no readable term text or posting lists.");
    Ok(())
}

fn cmd_search_encrypted(index_path: &std::path::Path, passphrase: &str, terms: &[String]) -> Result<()> {
    if terms.is_empty() {
        return Err(crate::error::NexusError::Other("provide at least one term to search for".to_string()));
    }
    let encrypted = crate::crypto_index::EncryptedIndex::load(index_path)?;
    let key = crate::crypto_index::IndexKey::derive(passphrase.as_bytes());
    let term_refs: Vec<&str> = terms.iter().map(|s| s.as_str()).collect();
    let doc_ids = encrypted
        .query_and(&key, &term_refs)
        .map_err(|e| crate::error::NexusError::Other(format!("decryption failed: {e}")))?;
    if doc_ids.is_empty() {
        println!("no matches (or wrong passphrase — SSE can't distinguish the two, by design; see crypto_index)");
        return Ok(());
    }
    println!("{} matching document ID(s):", doc_ids.len());
    for id in doc_ids {
        println!("  doc_id={id}");
    }
    println!(
        "\nnote: this returns matched document IDs only, not ranked/scored results or paths — \
         encrypted search doesn't have access to the (unencrypted) document store. Cross-reference \
         these IDs against your own local index if you need the actual file paths."
    );
    Ok(())
}

fn cmd_related(config: &Config, name: &str, limit: usize) -> Result<()> {
    let index = load_index(config)?;
    let Some(entity) = index.graph.entity(name) else {
        println!("no entity named '{}' in the graph (names are case-sensitive; try `nexus top-entities` to see what's there)", name);
        return Ok(());
    };
    println!(
        "{} ({:?}, mentioned {} time(s), last seen {})",
        entity.name,
        entity.kind,
        entity.mention_count(),
        format_unix(entity.last_seen_unix())
    );
    let related = index.graph.related_to(name, limit);
    if related.is_empty() {
        println!("\nno co-occurring entities found (each entity needs 2+ shared mentions to surface)");
        return Ok(());
    }
    println!("\nrelated entities:");
    for (related_name, count) in related {
        println!("  {} (co-occurred {} time(s))", related_name, count);
    }
    Ok(())
}

fn cmd_top_entities(config: &Config, limit: usize) -> Result<()> {
    let index = load_index(config)?;
    let top = index.graph.top_entities(limit);
    if top.is_empty() {
        println!("no entities found yet (each needs 2+ mentions across your indexed documents to surface)");
        return Ok(());
    }
    println!("top {} entities across {} indexed document(s):\n", top.len(), index.store.len());
    for entity in top {
        println!(
            "  {} — {:?}, {} mention(s), last seen {}",
            entity.name,
            entity.kind,
            entity.mention_count(),
            format_unix(entity.last_seen_unix())
        );
    }
    Ok(())
}

fn format_unix(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn cmd_serve(config: &Config, bind: &str) -> Result<()> {
    info!("starting HTTP server on {}", bind);
    let rate_limiter = Arc::new(RateLimiter::new(RateLimitConfig::default()));
    crate::api::serve(bind, config, rate_limiter)
}

fn cmd_ask(config: &Config, query_str: &str, mode_str: &str, use_rerank: bool) -> Result<()> {
    let Some(client) = crate::ai::LlmClient::from_config(&config.ai)? else {
        println!("AI features aren't configured.");
        println!("Set [ai] enabled = true and api_key = \"...\" in your config.toml to use `nexus ask`.");
        println!("(config file: run `nexus config` to see its path)");
        println!();
        println!("This works with OpenAI directly, or any self-hosted OpenAI-compatible server");
        println!("(Ollama, LM Studio, vLLM, LocalAI, ...) by pointing api_base_url at it.");
        return Ok(());
    };

    let index = load_index(config)?;
    if index.document_count() == 0 {
        println!("index is empty; run `nexus index <folder>` or `nexus crawl <url>` first");
        return Ok(());
    }

    let mode = crate::search::SearchMode::from_query_param(mode_str);
    let ast = query::parse(query_str)?;
    let candidate_limit = config.ai.rerank_top_n.max(config.ai.summary_max_sources);
    let outcome = crate::search::search(&index, &ast, &config.ranking, 0, candidate_limit, None, mode, Some(&crate::entity::SearchContext::current_os()));

    if outcome.results.is_empty() {
        println!("no results for '{}' — nothing to summarize.", query_str);
        return Ok(());
    }

    let content_cache = ContentCache::new(config.content_cache_dir.clone());
    let terms: HashSet<String> = query::collect_terms(&ast);

    // Build snippets for every candidate once, up front, since both
    // reranking and summarization need title+snippet text.
    let mut ordered: Vec<&crate::search::engine::SearchResult> = outcome.results.iter().collect();

    if use_rerank {
        let candidates: Vec<crate::ai::RerankCandidate> = ordered
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let content = load_result_content(&index, &content_cache, r);
                let snippet = content
                    .as_deref()
                    .map(|c| snippet::generate_from_content(c, &terms).text)
                    .unwrap_or_default();
                crate::ai::RerankCandidate { id: i, title: r.file_name.clone(), snippet }
            })
            .collect();

        match crate::ai::rerank(&client, query_str, &candidates) {
            Ok(order) => {
                let reranked: Vec<&crate::search::engine::SearchResult> =
                    order.iter().map(|&i| ordered[i]).collect();
                ordered = reranked;
                println!("(AI reranking applied)\n");
            }
            Err(e) => {
                warn!("AI reranking failed, keeping original order: {}", e);
                println!("(AI reranking failed — {} — showing normally-ranked results)\n", e);
            }
        }
    }

    let sources: Vec<crate::ai::SummarySource> = ordered
        .iter()
        .take(config.ai.summary_max_sources)
        .enumerate()
        .map(|(i, r)| {
            let content = load_result_content(&index, &content_cache, r);
            let snippet = content
                .as_deref()
                .map(|c| snippet::generate_from_content(c, &terms).text)
                .unwrap_or_default();
            crate::ai::SummarySource {
                id: i + 1,
                title: r.file_name.clone(),
                snippet,
                url_or_path: r.path.to_string_lossy().to_string(),
            }
        })
        .collect();

    println!("Answering from {} source(s)...\n", sources.len());
    let summary = crate::ai::summarize(&client, query_str, &sources)?;

    if summary.text.is_empty() {
        println!("The AI couldn't produce a grounded answer from these sources (every candidate");
        println!("sentence lacked a valid citation). Try `nexus search` instead, or rephrase.");
        return Ok(());
    }

    println!("{}\n", summary.text);
    if summary.ungrounded_sentences_dropped > 0 {
        println!(
            "(note: {} ungrounded sentence(s) from the AI's response were dropped for lacking a valid citation)\n",
            summary.ungrounded_sentences_dropped
        );
    }

    println!("Sources:");
    for source in &sources {
        let cited_marker = if summary.cited_source_ids.contains(&source.id) { "" } else { " (not cited)" };
        println!("  [{}] {}{}", source.id, source.url_or_path, cited_marker);
    }

    Ok(())
}

/// Loads a result's full content for summarization/reranking: from the
/// content cache for web results, or by re-reading the file for local
/// results. Returns `None` (rather than erroring the whole command) if
/// the content can't be loaded, so one unreadable source doesn't break
/// `ask` for every other source.
fn load_result_content(
    index: &Index,
    content_cache: &ContentCache,
    result: &crate::search::engine::SearchResult,
) -> Option<String> {
    if index.web.get(result.doc_id).is_some() {
        content_cache.load(result.doc_id).ok()
    } else {
        std::fs::read_to_string(&result.path).ok()
    }
}

fn cmd_serve_ws(config: &Config, bind: &str) -> Result<()> {
    info!("starting WebSocket server on {}", bind);
    let index = if storage::exists(&config.index_path) {
        storage::load(&config.index_path)?
    } else {
        info!("no index found, starting with empty index");
        Index::new()
    };
    let index = Arc::new(index);
    let content_cache = Arc::new(ContentCache::new(config.content_cache_dir.clone()));
    let ranking = Arc::new(config.ranking.clone());
    let rate_limiter = Arc::new(RateLimiter::new(RateLimitConfig::default()));

    info!("WebSocket search server listening on ws://{}", bind);
    println!("WebSocket search server listening on ws://{}", bind);

    crate::api::websocket::start_ws_server(bind, index, content_cache, ranking, rate_limiter)
        .map_err(|e| NexusError::Other(format!("WebSocket server failed: {}", e)))
}

fn cmd_tor(
    config: &Config,
    enable: bool,
    disable: bool,
    proxy: Option<String>,
    check: bool,
) -> Result<()> {
    if check {
        let tor_config = network::tor::TorConfig {
            enabled: true,
            ..Default::default()
        };
        if network::tor::check_tor_reachable(&tor_config) {
            println!("Tor is reachable via {}", tor_config.proxy_addr);
            info!("Tor reachability check passed");
        } else {
            println!("Tor is NOT reachable");
            info!("Tor reachability check failed");
        }
        return Ok(());
    }

    if enable {
        println!("Tor proxy enabled (configure in config.toml under [tor])");
        info!("Tor proxy enabled via CLI");
    }
    if disable {
        println!("Tor proxy disabled (configure in config.toml under [tor])");
        info!("Tor proxy disabled via CLI");
    }
    if let Some(ref addr) = proxy {
        println!("Tor proxy set to {}", addr);
        info!("Tor proxy address set to {}", addr);
    }
    if !enable && !disable && proxy.is_none() && !check {
        let status = if config.tor.enabled {
            "enabled"
        } else {
            "disabled"
        };
        println!("Tor proxy: {}", status);
        println!(
            "  proxy: {}:{}",
            config.tor.proxy_host, config.tor.proxy_port
        );
        println!(
            "  identity rotation: {} min",
            config.tor.identity_rotation_minutes
        );
        println!();
        println!("use --enable, --disable, --proxy <host:port>, or --check to manage Tor");
    }
    Ok(())
}

fn cmd_benchmark(config: &Config, iterations: usize, queries_file: Option<PathBuf>) -> Result<()> {
    info!("running benchmark: {} iterations", iterations);
    let index = load_index(config)?;
    if index.document_count() == 0 {
        println!("index is empty; nothing to benchmark");
        return Ok(());
    }

    let queries: Vec<String> = if let Some(path) = queries_file {
        let content = std::fs::read_to_string(&path).map_err(|e| NexusError::io(&path, e))?;
        content
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    } else {
        vec!["test".to_string(), "rust".to_string(), "search".to_string()]
    };

    if queries.is_empty() {
        println!("no queries to benchmark");
        return Ok(());
    }

    println!(
        "Benchmark: {} iterations, {} queries",
        iterations,
        queries.len()
    );
    println!(
        "Index: {} documents, {} terms",
        index.document_count(),
        index.vocabulary.len()
    );
    println!();

    let mut total_time = 0.0f64;
    let mut total_results = 0usize;
    let mut min_time = f64::MAX;
    let mut max_time = 0.0f64;

    for i in 0..iterations {
        let query_str = &queries[i % queries.len()];
        let started = Instant::now();
        let ast = query::parse(query_str).unwrap_or_else(|_| query::parse("test").unwrap());
        let outcome = crate::search::search(
            &index,
            &ast,
            &config.ranking,
            0,
            10,
            None,
            crate::search::SearchMode::Both,
            None,
        );
        let results = outcome.results;
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;

        total_time += elapsed;
        total_results += results.len();
        min_time = min_time.min(elapsed);
        max_time = max_time.max(elapsed);
    }

    let avg_time = total_time / iterations as f64;
    let avg_results = total_results as f64 / iterations as f64;

    println!("Results:");
    println!(
        "  Total time: {:.2}ms across {} searches",
        total_time, iterations
    );
    println!("  Average:    {:.3}ms per search", avg_time);
    println!("  Min:        {:.3}ms", min_time);
    println!("  Max:        {:.3}ms", max_time);
    println!("  Avg results: {:.1} per query", avg_results);
    Ok(())
}

fn cmd_privacy_policy(config: &Config) -> Result<()> {
    println!("=== Nexus Privacy Policy ===");
    println!();
    println!("Privacy-first search engine");
    println!();
    println!("Key privacy features:");
    println!(
        "  - Block sponsored results: {}",
        if config.privacy.block_sponsored_results {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "  - No filter bubble:        {}",
        if config.privacy.no_filter_bubble {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "  - Query anonymization:     {}",
        if config.privacy.anonymize_queries {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "  - Telemetry disabled:      {}",
        if config.privacy.disable_telemetry {
            "yes"
        } else {
            "no"
        }
    );
    println!(
        "  - Auto-delete history:     {}",
        match config.privacy.auto_delete_history_days {
            Some(days) => format!("after {} days", days),
            None => "never".to_string(),
        }
    );
    println!();
    println!("Data collection:");
    println!("  - No personal data is collected");
    println!("  - Search queries stay on your device");
    println!("  - No tracking cookies");
    println!("  - No third-party analytics");
    println!();
    println!("Full policy: {}", config.privacy.privacy_policy_url);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nexus-cli-test-{}-{}",
            std::process::id(),
            name
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn load_seed_file_parses_urls_skips_blanks_and_comments() {
        let path = temp_file(
            "seeds.txt",
            "# news sites\nhttps://example.com\n\nhttps://example.org/blog\n  # a trailing comment\n",
        );
        let urls = load_seed_file(&path).unwrap();
        assert_eq!(urls, vec!["https://example.com", "https://example.org/blog"]);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn load_seed_file_skips_lines_that_are_not_urls_with_a_warning_not_an_error() {
        let path = temp_file("seeds-bad-line.txt", "https://example.com\nnot a url\nhttps://example.org\n");
        let urls = load_seed_file(&path).unwrap();
        assert_eq!(urls, vec!["https://example.com", "https://example.org"]);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn load_seed_file_missing_file_is_an_error() {
        let path = std::env::temp_dir().join("nexus-cli-test-definitely-missing-seeds.txt");
        assert!(load_seed_file(&path).is_err());
    }
}
