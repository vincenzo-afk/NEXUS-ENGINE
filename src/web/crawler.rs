//! The crawler orchestrator: pulls URLs from the [`crate::web::queue::CrawlQueue`],
//! checks `robots.txt`, fetches (conditionally, for pages already
//! indexed), extracts content, deduplicates, indexes, and discovers new
//! links to enqueue — the full `Queue -> Scheduler -> Downloader -> Parser
//! -> Indexer` pipeline.
//!
//! This is a single-threaded, blocking implementation: straightforward to
//! reason about and sufficient for the tens-of-thousands-of-pages scale a
//! single machine can reach anyway given per-domain politeness delays.
//! The queue/scheduler split (see [`crate::web::queue`]) is exactly the
//! seam a multi-worker version would parallelize across — each worker
//! would just call `pop_ready` from a shared, mutex-guarded queue instead
//! of a private one.

use crate::config::WebCrawlConfig;
use crate::dedup::SimHash;
use crate::document::{Document, DocumentMetadata};
use crate::error::Result;
use crate::formats::{self, DocumentFormat};
use crate::html;
use crate::index::Index;
use crate::network::tor::TorConfig;
use crate::storage::content_cache::ContentCache;
use crate::web::canonical;
use crate::web::http::{HttpClient, HttpConfig};
use crate::web::queue::{priority, CrawlQueue};
use crate::web::robots::RobotsTxt;
use crate::webdoc::{self, LinkEdge, WebPageMeta};
use log::{debug, info, warn};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

/// Options for a single `crawl()` invocation.
#[derive(Debug, Clone)]
pub struct CrawlOptions {
    /// URLs to start crawling from.
    pub seeds: Vec<String>,
    /// If set, the crawl resumes a previously-saved queue at this path
    /// instead of starting fresh from `seeds` alone (seeds are still
    /// added, so re-running the same seeds is safe/idempotent).
    pub queue_path: Option<PathBuf>,
}

/// A summary of what happened during a crawl, printed by the CLI and
/// returned by the search API's crawl-trigger endpoint (if enabled).
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct CrawlReport {
    /// Number of pages successfully fetched (2xx or 304).
    pub pages_fetched: usize,
    /// Number of pages newly indexed or re-indexed.
    pub pages_indexed: usize,
    /// Number of pages skipped because they duplicated already-indexed content.
    pub pages_skipped_duplicate: usize,
    /// Number of pages skipped due to `robots.txt`.
    pub pages_skipped_robots: usize,
    /// Number of pages left unchanged since the last crawl (304 Not Modified).
    pub pages_unchanged: usize,
    /// Number of distinct new links discovered and enqueued.
    pub links_discovered: usize,
    /// Number of pages skipped because their domain had already hit
    /// `max_pages_per_domain` for this crawl run.
    pub pages_skipped_domain_budget: usize,
    /// Number of RSS/Atom feeds discovered and parsed.
    pub feeds_discovered: usize,
    /// Human-readable descriptions of non-fatal errors encountered
    /// (a failed fetch does not abort the whole crawl).
    pub errors: Vec<String>,
    /// `true` if the crawl stopped because `max_pages` was hit, meaning
    /// URLs remain in the queue for a future resumed crawl.
    pub queue_remaining: usize,
}

/// Orchestrates a crawl: fetching, extracting, deduplicating, and
/// indexing pages, following links breadth-first (bounded by priority)
/// within the configured page/depth budget.
pub struct WebCrawler {
    http: HttpClient,
    config: WebCrawlConfig,
    robots_cache: HashMap<String, RobotsTxt>,
    #[allow(dead_code)]
    tor_config: Option<TorConfig>,
}

impl WebCrawler {
    /// Builds a crawler from `config`.
    pub fn new(config: WebCrawlConfig) -> Result<Self> {
        Self::with_tor(config, None)
    }

    /// Builds a crawler with optional Tor proxy support.
    pub fn with_tor(config: WebCrawlConfig, tor_config: Option<TorConfig>) -> Result<Self> {
        let proxy_url = tor_config
            .as_ref()
            .filter(|t| t.enabled)
            .map(|t| format!("socks5://{}", t.proxy_addr));

        let http = HttpClient::new(HttpConfig {
            user_agent: config.user_agent.clone(),
            timeout: Duration::from_secs(config.timeout_seconds),
            max_redirects: 10,
            max_retries: config.max_retries,
            retry_base_delay: Duration::from_millis(500),
            proxy_url,
        })?;
        Ok(WebCrawler {
            http,
            config,
            robots_cache: HashMap::new(),
            tor_config,
        })
    }

    /// Runs a crawl, indexing discovered pages into `index` and caching
    /// their extracted text in `content_cache`.
    pub fn crawl(
        &mut self,
        index: &mut Index,
        content_cache: &ContentCache,
        options: &CrawlOptions,
    ) -> Result<CrawlReport> {
        let mut queue = match &options.queue_path {
            Some(path) if path.exists() => CrawlQueue::load(path)
                .unwrap_or_else(|_| CrawlQueue::new(self.config.default_delay_millis)),
            _ => CrawlQueue::new(self.config.default_delay_millis),
        };

        info!(
            "starting crawl with {} seed(s), max_pages={}, max_depth={}",
            options.seeds.len(),
            self.config.max_pages,
            self.config.max_depth
        );

        let mut report = CrawlReport::default();
        // Links discovered on unchanged/duplicate/new pages whose targets
        // may not yet be indexed; resolved to a link-graph edge once (if)
        // their target gets indexed later in this same crawl.
        let mut pending_links: HashMap<crate::document::DocId, Vec<(String, String)>> =
            HashMap::new();
        let mut pages_per_domain: HashMap<String, usize> = HashMap::new();
        let mut seen_feeds: std::collections::HashSet<String> = std::collections::HashSet::new();

        for seed in &options.seeds {
            let Some(url) = canonical::parse_canonical(seed) else {
                report.errors.push(format!("invalid seed URL: {seed}"));
                continue;
            };
            self.seed_domain(&url, &mut queue, &mut report);
            queue.push(
                url.to_string(),
                canonical::domain_of(&url),
                0,
                priority::SEED,
            );
        }

        while report.pages_fetched < self.config.max_pages {
            let Some(entry) = queue.pop_ready() else {
                if queue.is_empty() {
                    break;
                }
                // Nothing is ready yet (rate limits), but the queue isn't
                // empty. In a real long-running crawl we'd sleep and
                // retry; for a bounded CLI run we treat "nothing ready"
                // as "done for now" and let the queue persist for resume.
                debug!("no entries ready (rate-limited), pausing crawl");
                break;
            };

            debug!("fetching: {}", entry.url);

            if !self.domain_allowed(&entry.domain) {
                continue;
            }

            if self.config.max_pages_per_domain > 0 {
                let count = pages_per_domain.get(&entry.domain).copied().unwrap_or(0);
                if count >= self.config.max_pages_per_domain {
                    debug!(
                        "domain '{}' hit its per-domain crawl budget ({} pages), skipping {}",
                        entry.domain, self.config.max_pages_per_domain, entry.url
                    );
                    report.pages_skipped_domain_budget += 1;
                    continue;
                }
            }

            let Ok(url) = Url::parse(&entry.url) else {
                continue;
            };

            if self.config.respect_robots {
                let robots = self.robots_for(&url);
                let path_and_query = request_target(&url);
                if !robots.is_allowed(&self.config.user_agent, &path_and_query) {
                    report.pages_skipped_robots += 1;
                    continue;
                }
            }

            let existing_id = index.web.id_for_url(&entry.url);
            let existing_meta = existing_id.and_then(|id| index.web.get(id));

            let fetch_result = match existing_meta {
                Some(meta) => self.http.get_conditional(
                    &entry.url,
                    meta.etag.as_deref(),
                    meta.last_modified.as_deref(),
                ),
                None => self.http.get(&entry.url).map(Some),
            };

            let response = match fetch_result {
                Ok(Some(r)) => r,
                Ok(None) => {
                    report.pages_fetched += 1;
                    report.pages_unchanged += 1;
                    *pages_per_domain.entry(entry.domain.clone()).or_insert(0) += 1;
                    continue;
                }
                Err(e) => {
                    warn!("failed to fetch {}: {}", entry.url, e);
                    report.errors.push(format!("{}: {}", entry.url, e));
                    continue;
                }
            };

            report.pages_fetched += 1;
            *pages_per_domain.entry(entry.domain.clone()).or_insert(0) += 1;

            if response.status >= 400 {
                warn!("{} returned HTTP {}", entry.url, response.status);
                report
                    .errors
                    .push(format!("{}: HTTP {}", entry.url, response.status));
                continue;
            }
            if (response.bytes.len() as u64) > self.config.max_page_size_bytes {
                warn!("{} exceeds max page size, skipped", entry.url);
                report
                    .errors
                    .push(format!("{}: exceeds max page size, skipped", entry.url));
                continue;
            }

            let format = response
                .content_type
                .as_deref()
                .map(DocumentFormat::from_content_type)
                .unwrap_or(DocumentFormat::Html);

            let (indexable_text, discovered_links, title, meta_description, lang, author, feed_urls) =
                match format {
                    DocumentFormat::Html => {
                        let extracted = html::extract(&response.body);
                        let links: Vec<(String, String)> = extracted
                            .links
                            .iter()
                            .filter(|l| !l.nofollow)
                            .filter_map(|l| {
                                canonical::resolve(&url, &l.href)
                                    .map(|resolved| (resolved.to_string(), l.anchor_text.clone()))
                            })
                            .collect();
                        let feed_urls: Vec<String> = extracted
                            .feed_urls
                            .iter()
                            .filter_map(|href| canonical::resolve(&url, href))
                            .map(|u| u.to_string())
                            .collect();
                        (
                            extracted.indexable_text(),
                            links,
                            extracted.title,
                            extracted.meta_description,
                            extracted.lang,
                            extracted.author,
                            feed_urls,
                        )
                    }
                    DocumentFormat::Markdown => (
                        formats::extract_markdown(&response.body),
                        Vec::new(),
                        String::new(),
                        String::new(),
                        None,
                        None,
                        Vec::new(),
                    ),
                    DocumentFormat::Json => (
                        formats::extract_json(&response.body),
                        Vec::new(),
                        String::new(),
                        String::new(),
                        None,
                        None,
                        Vec::new(),
                    ),
                    DocumentFormat::Xml => (
                        formats::extract_xml(&response.body),
                        Vec::new(),
                        String::new(),
                        String::new(),
                        None,
                        None,
                        Vec::new(),
                    ),
                    DocumentFormat::Pdf => (
                        formats::extract_pdf(&response.bytes),
                        Vec::new(),
                        String::new(),
                        String::new(),
                        None,
                        None,
                        Vec::new(),
                    ),
                    DocumentFormat::PlainText => (
                        response.body.clone(),
                        Vec::new(),
                        String::new(),
                        String::new(),
                        None,
                        None,
                        Vec::new(),
                    ),
                    DocumentFormat::Docx => (
                        crate::extract::office::extract_docx(&response.bytes).text,
                        Vec::new(),
                        String::new(),
                        String::new(),
                        None,
                        None,
                        Vec::new(),
                    ),
                    DocumentFormat::Xlsx => (
                        crate::extract::office::extract_xlsx(&response.bytes).text,
                        Vec::new(),
                        String::new(),
                        String::new(),
                        None,
                        None,
                        Vec::new(),
                    ),
                    DocumentFormat::Pptx => (
                        crate::extract::office::extract_pptx(&response.bytes).text,
                        Vec::new(),
                        String::new(),
                        String::new(),
                        None,
                        None,
                        Vec::new(),
                    ),
                    DocumentFormat::Eml => (
                        crate::extract::email::parse_eml(&response.bytes)
                            .map(|e| e.indexable_text())
                            .unwrap_or_default(),
                        Vec::new(),
                        String::new(),
                        String::new(),
                        None,
                        None,
                        Vec::new(),
                    ),
                    DocumentFormat::Mbox => (
                        crate::extract::email::parse_mbox(&response.bytes)
                            .iter()
                            .map(|e| e.indexable_text())
                            .collect::<Vec<_>>()
                            .join("\n\n---\n\n"),
                        Vec::new(),
                        String::new(),
                        String::new(),
                        None,
                        None,
                        Vec::new(),
                    ),
                    DocumentFormat::Zip => (
                        crate::extract::archive::extract_zip(&response.bytes).text,
                        Vec::new(),
                        String::new(),
                        String::new(),
                        None,
                        None,
                        Vec::new(),
                    ),
                    // A crawled SQLite file over HTTP has no on-disk path for
                    // `crate::extract::sqlite_notes` to open (it needs a
                    // real file, not an in-memory byte buffer) — treated as
                    // an opaque download rather than something to extract
                    // text from mid-crawl.
                    DocumentFormat::SqliteDb => (
                        String::new(),
                        Vec::new(),
                        String::new(),
                        String::new(),
                        None,
                        None,
                        Vec::new(),
                    ),
                    DocumentFormat::Image => (
                        extract_image_bytes_via_temp_file(&response.bytes),
                        Vec::new(),
                        String::new(),
                        String::new(),
                        None,
                        None,
                        Vec::new(),
                    ),
                };

            if indexable_text.trim().is_empty() {
                warn!("{}: no extractable text, skipped", entry.url);
                report
                    .errors
                    .push(format!("{}: no extractable text, skipped", entry.url));
                continue;
            }

            if let Some(dup_id) = index.duplicates.find_duplicate(&indexable_text) {
                if Some(dup_id) != existing_id {
                    debug!("{}: duplicate of doc_id={}, skipping", entry.url, dup_id);
                    report.pages_skipped_duplicate += 1;
                    continue;
                }
            }

            let now = now_unix();
            let metadata = DocumentMetadata {
                path: PathBuf::from(&entry.url),
                file_name: if title.is_empty() {
                    entry.url.clone()
                } else {
                    title.clone()
                },
                extension: format.label().to_string(),
                size_bytes: response.bytes.len() as u64,
                modified_unix: now,
                token_count: 0,
            };
            let document = Document {
                metadata,
                content: indexable_text.clone(),
            };
            let doc_id = index.index_document(document);
            content_cache.store(doc_id, &indexable_text)?;

            let web_meta = WebPageMeta {
                url: entry.url.clone(),
                domain: entry.domain.clone(),
                title,
                meta_description,
                lang,
                author,
                content_type: format.label().to_string(),
                fetched_unix: now,
                etag: response.etag,
                last_modified: response.last_modified,
                redirect_chain: response.redirect_chain.clone(),
                simhash: SimHash::compute(&indexable_text).0,
                depth: entry.depth,
                outgoing: Vec::new(),
                incoming: Vec::new(),
                pagerank: 0.0,
            };
            index.web.insert(doc_id, web_meta);
            report.pages_indexed += 1;
            debug!("indexed doc_id={} from {}", doc_id, entry.url);

            if !discovered_links.is_empty() {
                pending_links.insert(doc_id, discovered_links.clone());
            }

            if self.config.discover_feeds {
                for feed_url in &feed_urls {
                    if seen_feeds.insert(feed_url.clone()) {
                        self.discover_and_enqueue_feed(feed_url, &mut queue, &mut report);
                    }
                }
            }

            if entry.depth < self.config.max_depth {
                for (link_url, _) in &discovered_links {
                    if let Ok(parsed) = Url::parse(link_url) {
                        let domain = canonical::domain_of(&parsed);
                        if self.domain_allowed(&domain)
                            && queue.push(
                                link_url.clone(),
                                domain,
                                entry.depth + 1,
                                priority::DISCOVERED,
                            )
                        {
                            report.links_discovered += 1;
                        }
                    }
                }
            }
        }

        // Resolve pending outgoing links now that everything fetched this
        // run is indexed (a link's target may have been crawled earlier
        // in this same loop, or in a previous run).
        for (source_id, links) in pending_links {
            let mut edges = Vec::new();
            for (target_url, anchor_text) in links {
                if let Some(target_id) = index.web.id_for_url(&target_url) {
                    if target_id != source_id {
                        edges.push(LinkEdge {
                            doc_id: target_id,
                            anchor_text,
                        });
                    }
                }
            }
            if let Some(meta) = index.web.get_mut(source_id) {
                meta.outgoing = edges;
            }
        }
        webdoc::build_incoming_links(&mut index.web);
        webdoc::pagerank::compute_and_store(&mut index.web, webdoc::pagerank::DEFAULT_DAMPING);

        report.queue_remaining = queue.len();
        if let Some(path) = &options.queue_path {
            if queue.is_empty() {
                let _ = std::fs::remove_file(path);
            } else {
                queue.save(path)?;
            }
        }

        info!("crawl complete: {} fetched, {} indexed, {} unchanged, {} duplicates skipped, {} robots-skipped, {} domain-budget-skipped, {} feeds discovered, {} links discovered, {} errors, {} remaining in queue",
            report.pages_fetched, report.pages_indexed, report.pages_unchanged,
            report.pages_skipped_duplicate, report.pages_skipped_robots, report.pages_skipped_domain_budget,
            report.feeds_discovered, report.links_discovered, report.errors.len(), report.queue_remaining);

        Ok(report)
    }

    /// Fetches `robots.txt` and discovers `sitemap.xml` URLs for a newly
    /// seeded origin, enqueueing every URL found in the sitemap(s) at
    /// [`priority::SITEMAP`].
    fn seed_domain(&mut self, url: &Url, queue: &mut CrawlQueue, report: &mut CrawlReport) {
        let robots = self.robots_for(url);
        if let Some(delay) = robots.crawl_delay(&self.config.user_agent) {
            queue.set_domain_delay(&canonical::domain_of(url), (delay * 1000.0) as u64);
        }

        let sitemap_urls: Vec<String> = if robots.sitemaps.is_empty() {
            vec![format!(
                "{}://{}/sitemap.xml",
                url.scheme(),
                url.host_str().unwrap_or("")
            )]
        } else {
            robots.sitemaps.clone()
        };

        debug!(
            "seed domain '{}': {} sitemap URL(s) to process",
            canonical::domain_of(url),
            sitemap_urls.len()
        );
        for sitemap_url in &sitemap_urls {
            self.enqueue_sitemap(sitemap_url, queue, report, 0);
        }

        if self.config.discover_feeds {
            let origin = format!("{}://{}", url.scheme(), url.host_str().unwrap_or(""));
            for well_known in ["/feed", "/feed.xml", "/rss.xml", "/atom.xml", "/rss"] {
                let feed_url = format!("{origin}{well_known}");
                self.discover_and_enqueue_feed(&feed_url, queue, report);
            }
        }
    }

    /// Fetches and parses a feed at `feed_url`, enqueueing every item's
    /// link at [`priority::FEED`]. Silently does nothing if the URL
    /// doesn't exist or isn't a feed — this is used both for guessed
    /// well-known paths (most of which won't exist on any given site) and
    /// for feed URLs a page explicitly declared, so a 404 here is
    /// expected and not worth logging as an error.
    fn discover_and_enqueue_feed(&self, feed_url: &str, queue: &mut CrawlQueue, report: &mut CrawlReport) {
        let Ok(response) = self.http.get(feed_url) else {
            return;
        };
        if response.status >= 400 {
            return;
        }
        let parsed = crate::web::feed::parse(&response.body);
        if parsed.items.is_empty() {
            return;
        }
        let Ok(feed_base) = Url::parse(feed_url) else {
            return;
        };

        report.feeds_discovered += 1;
        info!("discovered feed: {} ({} item(s))", feed_url, parsed.items.len());

        for item in &parsed.items {
            if let Some(resolved) = canonical::resolve(&feed_base, &item.url) {
                let domain = canonical::domain_of(&resolved);
                if self.domain_allowed(&domain)
                    && queue.push(resolved.to_string(), domain, 0, priority::FEED)
                {
                    report.links_discovered += 1;
                }
            }
        }
    }

    fn enqueue_sitemap(
        &self,
        sitemap_url: &str,
        queue: &mut CrawlQueue,
        report: &mut CrawlReport,
        recursion_depth: u32,
    ) {
        if recursion_depth > 2 {
            debug!("sitemap recursion depth limit reached for {}", sitemap_url);
            return;
        }
        let Ok(response) = self.http.get(sitemap_url) else {
            warn!("failed to fetch sitemap: {}", sitemap_url);
            return;
        };
        if response.status >= 400 {
            warn!("sitemap {} returned HTTP {}", sitemap_url, response.status);
            return;
        }
        info!(
            "discovered sitemap: {} ({} URLs, {} nested)",
            sitemap_url,
            response
                .body
                .lines()
                .filter(|l| l.contains("<loc>"))
                .count(),
            response.body.matches("<sitemap>").count()
        );
        let parsed = crate::web::sitemap::parse(&response.body);
        for page_url in parsed.urls {
            if let Some(canonical_url) = canonical::parse_canonical(&page_url) {
                let domain = canonical::domain_of(&canonical_url);
                if queue.push(canonical_url.to_string(), domain, 0, priority::SITEMAP) {
                    report.links_discovered += 1;
                }
            }
        }
        debug!(
            "sitemap {} yielded {} new URLs",
            sitemap_url, report.links_discovered
        );
        for nested in parsed.nested_sitemaps {
            self.enqueue_sitemap(&nested, queue, report, recursion_depth + 1);
        }
    }

    fn robots_for(&mut self, url: &Url) -> RobotsTxt {
        let origin = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
        if let Some(cached) = self.robots_cache.get(&origin) {
            return cached.clone();
        }
        let robots_url = format!("{origin}/robots.txt");
        let robots = match self.http.get(&robots_url) {
            Ok(resp) if resp.status < 400 => {
                info!("fetched robots.txt from {}", robots_url);
                RobotsTxt::parse(&resp.body)
            }
            _ => {
                debug!("robots.txt not available at {}, allowing all", robots_url);
                RobotsTxt::allow_all()
            }
        };
        self.robots_cache.insert(origin, robots.clone());
        robots
    }

    fn domain_allowed(&self, domain: &str) -> bool {
        if self.config.allowed_domains.is_empty() {
            return true;
        }
        self.config
            .allowed_domains
            .iter()
            .any(|allowed| domain == allowed || domain.ends_with(&format!(".{allowed}")))
    }
}

fn request_target(url: &Url) -> String {
    let mut target = url.path().to_string();
    if let Some(query) = url.query() {
        target.push('?');
        target.push_str(query);
    }
    target
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `crate::extract::image_ocr` operates on a filesystem path (both EXIF
/// reading and shelling out to `tesseract` expect one), but a crawled
/// image only exists as an in-memory byte buffer — this bridges the two
/// by writing to a short-lived temp file, extracting, and cleaning up
/// immediately after, rather than changing the extractor's interface
/// (which local-file callers use directly with a real path already) to
/// awkwardly support both.
fn extract_image_bytes_via_temp_file(bytes: &[u8]) -> String {
    let path = std::env::temp_dir().join(format!(
        "nexus-crawled-image-{}-{}.tmp",
        std::process::id(),
        rand::random::<u64>()
    ));
    if std::fs::write(&path, bytes).is_err() {
        return String::new();
    }
    let text = crate::extract::image_ocr::extract_image_text(&path).text;
    std::fs::remove_file(&path).ok();
    text
}
