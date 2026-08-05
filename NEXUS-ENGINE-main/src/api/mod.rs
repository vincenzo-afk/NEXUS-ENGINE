//! The search API: a minimal HTTP server exposing `GET /search?q=...` and
//! a couple of supporting endpoints, so Nexus can be queried by anything
//! that speaks HTTP (a browser extension, a web frontend, another
//! service) rather than only the CLI.
//!
//! Deliberately built on `tiny_http` rather than a full async web
//! framework: the index is loaded once, held read-only, and served from a
//! single thread per request via a small thread-per-connection loop. This
//! keeps the dependency footprint small while still being genuinely
//! concurrent enough for interactive use; a production deployment
//! fronting real traffic would put this behind a connection pool / load
//! balancer rather than scaling this process directly (see the
//! `distributed search` architecture note in the project README).

pub mod opensearch;
pub mod rate_limit;
pub mod request_queue;
pub mod result_cache;
pub mod websocket;

use crate::api::rate_limit::RateLimiter;
use crate::config::Config;
use crate::error::Result;
use crate::index::Index;
use crate::search::{self, snippet};
use crate::storage::content_cache::ContentCache;
use crate::query::{CompareOp, QueryNode};
use log::{debug, info};
use serde::Serialize;
use std::collections::{HashSet, HashMap};
use std::sync::Arc;
use std::path::PathBuf;
use tiny_http::{Header, Method, Response, Server};

/// How many `/search` requests may execute the ranking pipeline
/// concurrently before new ones queue (see `request_queue`'s doc
/// comment for why this is a separate concern from
/// `RateLimitConfig::concurrent_request_limit`, which caps concurrent
/// *connections* per client, not concurrent *expensive internal work*
/// across all clients). Not exposed as a config option yet — the search
/// pipeline is CPU-bound and fast enough on typical index sizes that
/// this is a safety net against pathological load, not a knob most
/// deployments will need to tune.
const SEARCH_QUEUE_MAX_CONCURRENT: usize = 32;
/// How long a `/search` request waits for a queue slot before giving up
/// and returning 503 rather than queueing indefinitely.
const SEARCH_QUEUE_MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
use chrono::TimeZone;

/// How often the click log and personal ranking model are re-read from
/// disk. Short enough that a `nexus click` or a retraining run is
/// reflected in live search results within a couple of seconds; long
/// enough that a busy server isn't doing two disk reads per `/search`
/// request. See [`PersonalizationCache`].
const PERSONALIZATION_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Periodically-refreshed, shared cache of the click log and personal
/// ranking model used to personalize search results.
///
/// `run_search` originally reloaded both files from disk on every single
/// request. That's fine for the tiny files these start out as at
/// low-to-moderate QPS, but it's an unnecessary disk read (and
/// deserialization) per request under real load. This replaces that with
/// a `RwLock`-guarded cache, refreshed at most every
/// [`PERSONALIZATION_REFRESH_INTERVAL`], shared across all connections
/// the same way `index`/`content_cache`/`ranking` already are in
/// [`serve`]. A `nexus click` still takes effect on the *next* search in
/// practice, just not necessarily the very next millisecond.
struct PersonalizationCache {
    clicks_path: PathBuf,
    personal_model_path: PathBuf,
    state: std::sync::RwLock<(
        Arc<crate::clicks::ClickLog>,
        Arc<crate::ranking::adaptive::PersonalRankingModel>,
        std::time::Instant,
    )>,
}

impl PersonalizationCache {
    fn new(clicks_path: PathBuf, personal_model_path: PathBuf) -> Self {
        let clicks = Arc::new(crate::clicks::ClickLog::load(&clicks_path).unwrap_or_default());
        let personal_model = Arc::new(
            crate::ranking::adaptive::PersonalRankingModel::load(&personal_model_path)
                .unwrap_or_default(),
        );
        Self {
            clicks_path,
            personal_model_path,
            state: std::sync::RwLock::new((clicks, personal_model, std::time::Instant::now())),
        }
    }

    /// Returns the current click log and personal ranking model,
    /// transparently reloading from disk first if the cached copies are
    /// older than [`PERSONALIZATION_REFRESH_INTERVAL`].
    fn get(
        &self,
    ) -> (
        Arc<crate::clicks::ClickLog>,
        Arc<crate::ranking::adaptive::PersonalRankingModel>,
    ) {
        {
            let guard = self.state.read().unwrap_or_else(|e| e.into_inner());
            if guard.2.elapsed() < PERSONALIZATION_REFRESH_INTERVAL {
                return (Arc::clone(&guard.0), Arc::clone(&guard.1));
            }
        }
        // Cache is stale: reload outside the lock. If two requests race
        // here, both just re-read the same small files independently and
        // the last one to write wins — harmless, and cheaper than
        // serializing every request behind a reload.
        let clicks = Arc::new(crate::clicks::ClickLog::load(&self.clicks_path).unwrap_or_default());
        let personal_model = Arc::new(
            crate::ranking::adaptive::PersonalRankingModel::load(&self.personal_model_path)
                .unwrap_or_default(),
        );
        let mut guard = self.state.write().unwrap_or_else(|e| e.into_inner());
        *guard = (
            Arc::clone(&clicks),
            Arc::clone(&personal_model),
            std::time::Instant::now(),
        );
        (clicks, personal_model)
    }
}

/// One result entry in the JSON search response.
#[derive(Debug, Serialize)]
struct ApiResult {
    rank: usize,
    url: String,
    /// Local-filesystem path, for Local/Hybrid mode result cards that
    /// need the raw path (breadcrumb display, the `/open` button) rather
    /// than the `/view/...` cache-viewer URL `url` points to.
    path: String,
    title: String,
    score: f32,
    snippet: String,
    match_count: usize,
    is_web: bool,
    is_onion: bool,
    /// `"fs"` or `"web"` — an explicit, self-describing companion to
    /// `is_web` for clients that would rather match on a string.
    source_type: String,
    domain: Option<String>,
    favicon: Option<String>,
    size: String,
    date: String,
    filetype: String,
    /// One-line summary of the non-baseline ranking signals that applied,
    /// e.g. "Title / filename match (+50%), Authority (+12%)".
    why: String,
    /// The full structured breakdown behind `why`, for a "why did this
    /// rank here?" expandable panel.
    explanation: Vec<crate::ranking::ExplanationReason>,
    /// Reliability signals (web results only — see the module doc
    /// comment on `ranking::reliability` for exactly what this is and
    /// isn't).
    reliability: Option<crate::ranking::reliability::ReliabilitySignals>,
}

/// The full JSON response for `GET /search`.
#[derive(Debug, Serialize)]
struct ApiResponse {
    query: String,
    mode: String,
    total: usize,
    /// Of `total`, how many are local filesystem results (meaningful
    /// mainly in hybrid mode; 0 for pure Web/Tor mode, equal to `total`
    /// for pure Local mode).
    local_count: usize,
    /// Of `total`, how many are web results.
    web_count: usize,
    took_ms: f64,
    results: Vec<ApiResult>,
    #[serde(rename = "hasMore")]
    has_more: bool,
    /// `true` if `?rerank=1` was requested AND AI reranking actually
    /// succeeded and was applied. `false` if reranking wasn't requested,
    /// AI isn't configured, or the AI's response couldn't be validated
    /// (in which case results are in their normal ranked order, not a
    /// broken partial rerank).
    ai_reranked: bool,
    /// An instant-answer card for arithmetic/unit-conversion queries
    /// (`crate::answers::try_answer`), e.g. `"2 + 2"` or
    /// `"10 km to miles"`. `null` for ordinary queries — the frontend
    /// should always render `results` regardless of whether this is
    /// present.
    #[serde(skip_serializing_if = "Option::is_none")]
    instant_answer: Option<InstantAnswerJson>,
}

#[derive(Debug, Serialize)]
struct InstantAnswerJson {
    /// Human-readable rendering, e.g. `"2 + 2 = 4"`.
    display: String,
    kind: &'static str,
}

impl From<&crate::answers::InstantAnswer> for InstantAnswerJson {
    fn from(a: &crate::answers::InstantAnswer) -> Self {
        let kind = match a {
            crate::answers::InstantAnswer::Calculation { .. } => "calculation",
            crate::answers::InstantAnswer::UnitConversion { .. } => "unit_conversion",
        };
        InstantAnswerJson { display: a.to_string(), kind }
    }
}

#[derive(Debug, Serialize)]
struct ApiError {
    error: String,
}

fn find_frontend_dir() -> Option<PathBuf> {
    // 1. Check current directory
    if let Ok(cwd) = std::env::current_dir() {
        let path = cwd.join("frontend");
        if path.join("index.html").exists() {
            return Some(path);
        }
    }
    // 2. Check executable directory hierarchy
    if let Ok(exe) = std::env::current_exe() {
        let mut current = exe.parent();
        while let Some(dir) = current {
            let path = dir.join("frontend");
            if path.join("index.html").exists() {
                return Some(path);
            }
            let parent_path = dir.join("..").join("frontend");
            if parent_path.join("index.html").exists() {
                return Some(parent_path);
            }
            current = dir.parent();
        }
    }
    None
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

/// Starts the search API server on `bind` (e.g. `"127.0.0.1:8080"`),
/// serving from the index and configuration currently on disk. Blocks the
/// current thread forever (or until the process is killed).
pub fn serve(bind: &str, config: &Config, rate_limiter: Arc<RateLimiter>) -> Result<()> {
    let index = if crate::storage::exists(&config.index_path) {
        crate::storage::load(&config.index_path)?
    } else {
        Index::new()
    };
    let index = Arc::new(index);
    let content_cache = Arc::new(ContentCache::new(config.content_cache_dir.clone()));
    let ranking = Arc::new(config.ranking.clone());

    // Build autocomplete index once on startup
    let mut doc_frequencies: HashMap<String, u32> = HashMap::new();
    for (term, term_id) in index.vocabulary.iter() {
        if let Some(list) = index.inverted.postings_for(term_id) {
            doc_frequencies.insert(term.to_string(), list.document_frequency() as u32);
        }
    }
    let autocomplete = Arc::new(crate::autocomplete::Autocomplete::build(&index.vocabulary, &doc_frequencies));
    let frontend_dir = find_frontend_dir();

    // Wired for real, into the actual request loop below (not just
    // constructed and left unused): a TTL+LRU cache in front of search
    // execution, a bounded-concurrency admission gate on the expensive
    // search path, and a Prometheus-style metrics registry.
    let result_cache: Arc<crate::api::result_cache::ResultCache<Arc<String>>> =
        Arc::new(crate::api::result_cache::ResultCache::new(500, std::time::Duration::from_secs(30)));
    let search_queue = Arc::new(crate::api::request_queue::RequestQueue::new(
        "search",
        SEARCH_QUEUE_MAX_CONCURRENT,
    ));
    let metrics = Arc::new(crate::observability::MetricsRegistry::new());
    let personalization_cache = Arc::new(PersonalizationCache::new(
        config.clicks_path.clone(),
        config.personal_model_path.clone(),
    ));

    let server = Server::http(bind)
        .map_err(|e| crate::error::NexusError::Other(format!("failed to bind {bind}: {e}")))?;

    info!("Nexus search API listening on http://{bind}");
    println!("Nexus search API listening on http://{bind}");
    println!("  GET /search?q=<query>&limit=<n>&offset=<n>");
    println!("  GET /health");
    println!("  GET /metrics");
    println!("  GET /opensearch.xml");
    if let Some(ref dir) = frontend_dir {
        println!("  Serving frontend files from {}", dir.display());
    } else {
        println!("  Warning: frontend directory not found");
    }

    for request in server.incoming_requests() {
        let index = Arc::clone(&index);
        let content_cache = Arc::clone(&content_cache);
        let ranking = Arc::clone(&ranking);
        let rate_limiter = Arc::clone(&rate_limiter);
        let autocomplete = Arc::clone(&autocomplete);
        let result_cache = Arc::clone(&result_cache);
        let search_queue = Arc::clone(&search_queue);
        let metrics = Arc::clone(&metrics);
        let personalization_cache = Arc::clone(&personalization_cache);
        let config = config.clone();
        let f_dir = frontend_dir.clone();
        std::thread::spawn(move || {
            handle_request(
                request,
                &index,
                &content_cache,
                &ranking,
                &rate_limiter,
                &autocomplete,
                &result_cache,
                &search_queue,
                &metrics,
                &personalization_cache,
                &config,
                f_dir,
            );
        });
    }

    Ok(())
}

fn handle_request(
    request: tiny_http::Request,
    index: &Index,
    content_cache: &ContentCache,
    ranking: &crate::config::RankingConfig,
    rate_limiter: &RateLimiter,
    autocomplete: &crate::autocomplete::Autocomplete,
    result_cache: &crate::api::result_cache::ResultCache<Arc<String>>,
    search_queue: &crate::api::request_queue::RequestQueue,
    metrics: &crate::observability::MetricsRegistry,
    personalization_cache: &PersonalizationCache,
    config: &Config,
    frontend_dir: Option<PathBuf>,
) {
    metrics.counter("http_requests_total").inc();
    let url = request.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    debug!("request: {} {}", request.method(), url);
    let json_header = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
        .expect("static header is always valid");
    let xml_header = Header::from_bytes(
        &b"Content-Type"[..],
        &b"application/opensearchdescription+xml"[..],
    )
    .expect("static header is always valid");

    let cors_header = Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..])
        .expect("CORS header is always valid");

    if request.method() != &Method::Get {
        let body = serde_json::to_string(&ApiError {
            error: "only GET is supported".to_string(),
        })
        .unwrap_or_default();
        let mut response = Response::from_string(body)
            .with_status_code(405)
            .with_header(json_header);
        if !config.security.cors_allowed_origins.is_empty() {
            response = response.with_header(cors_header);
        }
        let _ = request.respond(response);
        return;
    }

    let client_ip = request
        .headers()
        .iter()
        .find(|h| {
            let field_str = h.field.as_str().as_str();
            field_str.eq_ignore_ascii_case("X-Forwarded-For")
        })
        .and_then(|h| Some(h.value.as_str()))
        .unwrap_or("unknown");

    match path {
        "/metrics" => {
            let body = metrics.render_prometheus();
            let text_header = Header::from_bytes(&b"Content-Type"[..], &b"text/plain; version=0.0.4"[..])
                .expect("static header is always valid");
            let mut response = Response::from_string(body).with_header(text_header);
            if !config.security.cors_allowed_origins.is_empty() {
                response = response.with_header(cors_header);
            }
            let _ = request.respond(response);
        }
        "/health" => {
            let body = serde_json::json!({
                "status": "ok",
                "documents": index.document_count(),
                "web_pages": index.web.len(),
                // Whether a Tor SOCKS5 proxy is configured (config.toml's
                // [tor] enabled = true), not a live reachability probe —
                // an actual connection check to the Tor network would add
                // real latency to every /health call, which is too
                // expensive for something the frontend polls on every
                // switch into Tor mode. `nexus tor --check` does the full
                // live check when you actually want it.
                "tor_configured": config.tor.enabled,
            })
            .to_string();
            let mut response = Response::from_string(body).with_header(json_header);
            if !config.security.cors_allowed_origins.is_empty() {
                response = response.with_header(cors_header);
            }
            let _ = request.respond(response);
        }
        "/opensearch.xml" => {
            let bind = request
                .headers()
                .iter()
                .find(|h| {
                    let field_str = h.field.as_str().as_str();
                    field_str.eq_ignore_ascii_case("Host")
                })
                .and_then(|h| Some(format!("http://{}", h.value.as_str())))
                .unwrap_or_else(|| "http://localhost:8080".to_string());
            let xml = opensearch::generate_opensearch_xml(&bind);
            let mut response = Response::from_string(xml).with_header(xml_header);
            if !config.security.cors_allowed_origins.is_empty() {
                response = response.with_header(cors_header);
            }
            let _ = request.respond(response);
        }
        "/suggest" => {
            let params = parse_query_string(query);
            let prefix = params.get("prefix").cloned().unwrap_or_default();
            let limit: usize = params
                .get("limit")
                .and_then(|v| v.parse().ok())
                .unwrap_or(8);
            let suggestions = autocomplete.suggest(&prefix, limit);

            #[derive(Serialize)]
            struct SuggestResponse {
                suggestions: Vec<String>,
            }
            let body = serde_json::to_string(&SuggestResponse { suggestions }).unwrap_or_default();
            let mut response = Response::from_string(body).with_header(json_header);
            if !config.security.cors_allowed_origins.is_empty() {
                response = response.with_header(cors_header);
            }
            let _ = request.respond(response);
        }
        "/search" => {
            if !rate_limiter.consume(client_ip, 1) {
                let body = serde_json::to_string(&ApiError {
                    error: "rate limit exceeded, please slow down".to_string(),
                })
                .unwrap_or_default();
                let mut response = Response::from_string(body)
                    .with_status_code(429)
                    .with_header(json_header);
                if !config.security.cors_allowed_origins.is_empty() {
                    response = response.with_header(cors_header);
                }
                let _ = request.respond(response);
                return;
            }

            let params = parse_query_string(query);
            let q = params.get("q").cloned().unwrap_or_default();
            info!("search query: '{}'", q);
            if q.trim().is_empty() {
                let body = serde_json::to_string(&ApiError {
                    error: "missing required query parameter 'q'".to_string(),
                })
                .unwrap_or_default();
                let mut response = Response::from_string(body)
                    .with_status_code(400)
                    .with_header(json_header);
                if !config.security.cors_allowed_origins.is_empty() {
                    response = response.with_header(cors_header);
                }
                let _ = request.respond(response);
                return;
            }

            if let Err(e) = rate_limiter.validate_query(&q) {
                let body = serde_json::to_string(&ApiError { error: e }).unwrap_or_default();
                let mut response = Response::from_string(body)
                    .with_status_code(400)
                    .with_header(json_header);
                if !config.security.cors_allowed_origins.is_empty() {
                    response = response.with_header(cors_header);
                }
                let _ = request.respond(response);
                return;
            }

            let bang_table = crate::bangs::BangTable::from_builtin();
            if let Some(bang_match) = bang_table.resolve(&q) {
                info!("bang matched: {} -> {}", bang_match.name, bang_match.url);
                let location_header =
                    match Header::from_bytes(&b"Location"[..], bang_match.url.as_bytes()) {
                        Ok(h) => h,
                        Err(_) => {
                            let body = serde_json::to_string(&ApiError {
                                error: "failed to construct redirect".to_string(),
                            })
                            .unwrap_or_default();
                            let _ = request.respond(
                                Response::from_string(body)
                                    .with_status_code(500)
                                    .with_header(json_header),
                            );
                            return;
                        }
                    };
                let mut response = Response::from_string(format!(
                    "redirecting to {} ({})",
                    bang_match.name, bang_match.url
                ))
                .with_status_code(302)
                .with_header(location_header);
                if !config.security.cors_allowed_origins.is_empty() {
                    response = response.with_header(cors_header);
                }
                let _ = request.respond(response);
                return;
            }

            // Align limit / offset parameters with frontend
            let limit: usize = params
                .get("n")
                .or_else(|| params.get("limit"))
                .and_then(|v| v.parse().ok())
                .unwrap_or(10);
            let page: usize = params
                .get("p")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            let offset: usize = params
                .get("offset")
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| page.saturating_sub(1) * limit);

            if let Err(e) = rate_limiter.validate_pagination(offset, limit) {
                let body = serde_json::to_string(&ApiError { error: e }).unwrap_or_default();
                let mut response = Response::from_string(body)
                    .with_status_code(400)
                    .with_header(json_header);
                if !config.security.cors_allowed_origins.is_empty() {
                    response = response.with_header(cors_header);
                }
                let _ = request.respond(response);
                return;
            }

            // Cache check: a hit skips ranking entirely and returns the
            // exact same serialized response a fresh search would have
            // produced (the cache key includes every parameter that
            // affects the response body, built the same way the miss
            // path below stores it).
            let cache_extra: Vec<(&str, &str)> = params
                .iter()
                .filter(|(k, _)| k.as_str() != "q" && k.as_str() != "n" && k.as_str() != "limit" && k.as_str() != "p" && k.as_str() != "offset")
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let cache_key = crate::api::result_cache::cache_key(&q, offset, limit, &cache_extra);
            if let Some(cached_body) = result_cache.get(&cache_key) {
                metrics.counter("search_cache_hits_total").inc();
                let mut response = Response::from_string(cached_body.as_str().to_string()).with_header(json_header);
                if !config.security.cors_allowed_origins.is_empty() {
                    response = response.with_header(cors_header);
                }
                let _ = request.respond(response);
                return;
            }
            metrics.counter("search_cache_misses_total").inc();

            // Admission gate: bounds how many searches run the ranking
            // pipeline concurrently, independent of how many connections
            // are open (that's `rate_limiter`'s job) — see
            // `SEARCH_QUEUE_MAX_CONCURRENT`'s doc comment.
            let _permit = match search_queue.acquire(SEARCH_QUEUE_MAX_WAIT) {
                Some(permit) => permit,
                None => {
                    metrics.counter("search_queue_timeouts_total").inc();
                    let body = serde_json::to_string(&ApiError {
                        error: "search service is at capacity, please retry shortly".to_string(),
                    })
                    .unwrap_or_default();
                    let mut response = Response::from_string(body)
                        .with_status_code(503)
                        .with_header(json_header);
                    if !config.security.cors_allowed_origins.is_empty() {
                        response = response.with_header(cors_header);
                    }
                    let _ = request.respond(response);
                    return;
                }
            };

            let _span = crate::observability::Span::start(metrics.histogram("search_latency_ms"));
            let response = run_search(
                index,
                content_cache,
                ranking,
                &config.ai,
                personalization_cache,
                &q,
                offset,
                limit,
                &params,
            );
            let mut http_response = match response {
                Ok(body) => {
                    let serialized = serde_json::to_string(&body).unwrap_or_default();
                    result_cache.put(cache_key, Arc::new(serialized.clone()));
                    Response::from_string(serialized)
                        .with_header(json_header)
                }
                Err(e) => {
                    metrics.counter("search_errors_total").inc();
                    let body = serde_json::to_string(&ApiError {
                        error: e.to_string(),
                    })
                    .unwrap_or_default();
                    Response::from_string(body)
                        .with_status_code(400)
                        .with_header(json_header)
                }
            };
            if !config.security.cors_allowed_origins.is_empty() {
                http_response = http_response.with_header(cors_header);
            }
            let _ = request.respond(http_response);
        }
        "/ask" => {
            if !rate_limiter.consume(client_ip, 1) {
                let body = serde_json::to_string(&ApiError {
                    error: "rate limit exceeded, please slow down".to_string(),
                })
                .unwrap_or_default();
                let _ = request.respond(
                    Response::from_string(body).with_status_code(429).with_header(json_header),
                );
                return;
            }

            let params = parse_query_string(query);
            let q = params.get("q").cloned().unwrap_or_default();
            if q.trim().is_empty() {
                let body = serde_json::to_string(&ApiError {
                    error: "missing required query parameter 'q'".to_string(),
                })
                .unwrap_or_default();
                let _ = request.respond(
                    Response::from_string(body).with_status_code(400).with_header(json_header),
                );
                return;
            }

            let response = run_ask(index, content_cache, ranking, &config.ai, &q, &params);
            let http_response = match response {
                Ok(body) => Response::from_string(serde_json::to_string(&body).unwrap_or_default())
                    .with_header(json_header),
                Err(e) => {
                    let body = serde_json::to_string(&ApiError { error: e.to_string() }).unwrap_or_default();
                    Response::from_string(body).with_status_code(503).with_header(json_header)
                }
            };
            let _ = request.respond(http_response);
        }
        path if path.starts_with("/view/") => {
            let file_path_str = &path["/view/".len()..];
            if let Some(file_path) = resolve_indexed_path(index, file_path_str) {
                if let Ok(content) = std::fs::read(&file_path) {
                    let extension = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");
                    let content_type = match extension {
                        "html" | "htm" => "text/html; charset=utf-8",
                        "css" => "text/css; charset=utf-8",
                        "js" => "application/javascript; charset=utf-8",
                        "json" => "application/json; charset=utf-8",
                        "txt" | "rs" | "toml" | "py" | "c" | "cpp" | "h" | "md" | "sh" | "go" | "yaml" | "yml" => "text/plain; charset=utf-8",
                        _ => "application/octet-stream",
                    };
                    let header = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()).unwrap();
                    let mut response = Response::from_data(content).with_header(header);
                    if !config.security.cors_allowed_origins.is_empty() {
                        response = response.with_header(cors_header);
                    }
                    let _ = request.respond(response);
                    return;
                }
            }
            let mut response = Response::from_string("File Not Found").with_status_code(404);
            if !config.security.cors_allowed_origins.is_empty() {
                response = response.with_header(cors_header);
            }
            let _ = request.respond(response);
        }
        "/open" => {
            let params = parse_query_string(query);
            let requested = params.get("path").cloned().unwrap_or_default();
            if requested.is_empty() {
                let body = serde_json::to_string(&ApiError {
                    error: "missing required query parameter 'path'".to_string(),
                })
                .unwrap_or_default();
                let mut response = Response::from_string(body).with_status_code(400).with_header(json_header);
                if !config.security.cors_allowed_origins.is_empty() {
                    response = response.with_header(cors_header);
                }
                let _ = request.respond(response);
                return;
            }

            let Some(file_path) = resolve_indexed_path(index, &requested) else {
                let body = serde_json::to_string(&ApiError {
                    error: "path not found in index (only indexed local files may be opened)".to_string(),
                })
                .unwrap_or_default();
                let mut response = Response::from_string(body).with_status_code(404).with_header(json_header);
                if !config.security.cors_allowed_origins.is_empty() {
                    response = response.with_header(cors_header);
                }
                let _ = request.respond(response);
                return;
            };

            match open_in_default_app(&file_path) {
                Ok(()) => {
                    let body = serde_json::json!({"status": "opened", "path": file_path.to_string_lossy()}).to_string();
                    let mut response = Response::from_string(body).with_header(json_header);
                    if !config.security.cors_allowed_origins.is_empty() {
                        response = response.with_header(cors_header);
                    }
                    let _ = request.respond(response);
                }
                Err(e) => {
                    let body = serde_json::to_string(&ApiError { error: e }).unwrap_or_default();
                    let mut response = Response::from_string(body).with_status_code(500).with_header(json_header);
                    if !config.security.cors_allowed_origins.is_empty() {
                        response = response.with_header(cors_header);
                    }
                    let _ = request.respond(response);
                }
            }
        }
        "/" | "/index.html" | "/style.css" | "/app.js" => {
            if let Some(ref dir) = frontend_dir {
                let file_name = if path == "/" { "index.html" } else { &path[1..] };
                let file_path = dir.join(file_name);
                if file_path.exists() {
                    if let Ok(content) = std::fs::read(&file_path) {
                        let content_type = match file_name {
                            "index.html" => "text/html; charset=utf-8",
                            "style.css" => "text/css; charset=utf-8",
                            "app.js" => "application/javascript; charset=utf-8",
                            _ => "application/octet-stream",
                        };
                        let header = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()).unwrap();
                        let mut response = Response::from_data(content).with_header(header);
                        if !config.security.cors_allowed_origins.is_empty() {
                            response = response.with_header(cors_header);
                        }
                        let _ = request.respond(response);
                        return;
                    }
                }
            }
            let mut response = Response::from_string("Not Found").with_status_code(404);
            if !config.security.cors_allowed_origins.is_empty() {
                response = response.with_header(cors_header);
            }
            let _ = request.respond(response);
        }
        _ => {
            let body = serde_json::to_string(&ApiError {
                error: format!("not found: {path}"),
            })
            .unwrap_or_default();
            let mut response = Response::from_string(body)
                .with_status_code(404)
                .with_header(json_header);
            if !config.security.cors_allowed_origins.is_empty() {
                response = response.with_header(cors_header);
            }
            let _ = request.respond(response);
        }
    }
}

fn run_search(
    index: &Index,
    content_cache: &ContentCache,
    ranking: &crate::config::RankingConfig,
    ai_config: &crate::config::AiConfig,
    personalization_cache: &PersonalizationCache,
    query_str: &str,
    offset: usize,
    limit: usize,
    params: &HashMap<String, String>,
) -> Result<ApiResponse> {
    let started = std::time::Instant::now();
    let ast = crate::query::parse(query_str)?;

    // Served from `personalization_cache` rather than re-read from disk
    // on every request — see that type's doc comment for the tradeoff.
    // A click recorded via `nexus click` still shows up within
    // `PERSONALIZATION_REFRESH_INTERVAL`, not necessarily on the very
    // next request.
    let (clicks, personal_model) = personalization_cache.get();
    let frequently_clicked_kind = crate::ranking::adaptive::most_frequently_clicked_kind(&clicks, index);
    let personalization = crate::search::Personalization {
        model: &personal_model,
        frequently_clicked_kind,
    };

    // Parse and apply frontend filters to search AST
    let mut filter_nodes = Vec::new();
    if let Some(filetype) = params.get("filetype") {
        if !filetype.is_empty() {
            filter_nodes.push(QueryNode::FilterExt(filetype.to_lowercase()));
        }
    }
    if let Some(site) = params.get("site") {
        if !site.is_empty() {
            filter_nodes.push(QueryNode::FilterSite(site.to_lowercase()));
        }
    }
    if let Some(date) = params.get("date") {
        if !date.is_empty() {
            let threshold_seconds = match date.as_str() {
                "h" => Some(3600),
                "d" => Some(86400),
                "w" => Some(86400 * 7),
                "m" => Some(86400 * 30),
                "y" => Some(86400 * 365),
                _ => None,
            };
            if let Some(secs) = threshold_seconds {
                filter_nodes.push(QueryNode::FilterModified(CompareOp::LessThan, secs));
            }
        }
    }

    let combined_ast = if filter_nodes.is_empty() {
        ast
    } else {
        let mut and_children = vec![ast];
        and_children.extend(filter_nodes);
        QueryNode::And(and_children)
    };

    let mode = params
        .get("mode")
        .map(|s| search::SearchMode::from_query_param(s))
        .unwrap_or_default();

    let outcome = search::search(index, &combined_ast, ranking, offset, limit, Some(&clicks), mode, Some(&crate::entity::SearchContext::current_os()));
    let mut results = outcome.results;
    crate::search::apply_personal_boost(&mut results, index, &personalization);
    let (total, local_count, web_count) = (outcome.total, outcome.local_count, outcome.web_count);
    let took_ms_pre_rerank = started.elapsed().as_secs_f64() * 1000.0;

    let terms: HashSet<String> = crate::query::collect_terms(&combined_ast);

    let rerank_requested = params.get("rerank").map(|v| v == "1" || v == "true").unwrap_or(false);
    let mut ai_reranked = false;
    if rerank_requested {
        if let Ok(Some(client)) = crate::ai::LlmClient::from_config(ai_config) {
            let candidates: Vec<crate::ai::RerankCandidate> = results
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let web_meta = index.web.get(r.doc_id);
                    let content = match web_meta {
                        Some(_) => content_cache.load(r.doc_id).ok(),
                        None => std::fs::read_to_string(&r.path).ok(),
                    };
                    let snippet = content
                        .as_deref()
                        .map(|c| snippet::generate_from_content(c, &terms).text)
                        .unwrap_or_default();
                    crate::ai::RerankCandidate { id: i, title: r.file_name.clone(), snippet }
                })
                .collect();

            if let Ok(order) = crate::ai::rerank(&client, query_str, &candidates) {
                let original = std::mem::take(&mut results);
                results = order.into_iter().map(|i| original[i].clone()).collect();
                ai_reranked = true;
            }
            // On any rerank error (network failure, malformed response),
            // `results` is left in its original order — reranking is a
            // best-effort enhancement, never a hard requirement for
            // search to return results.
        }
    }
    let took_ms = if rerank_requested { started.elapsed().as_secs_f64() * 1000.0 } else { took_ms_pre_rerank };

    let results = results
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            let web_meta = index.web.get(r.doc_id);
            let metadata = index.store.get(r.doc_id);
            let content = match web_meta {
                Some(_) => content_cache.load(r.doc_id).ok(),
                None => std::fs::read_to_string(&r.path).ok(),
            };
            let snippet_text = content
                .map(|c| snippet::generate_from_content(&c, &terms).text)
                .unwrap_or_default();

            let extension = metadata.map(|m| m.extension.clone()).unwrap_or_default();
            let size_bytes = metadata.map(|m| m.size_bytes).unwrap_or(0);
            let size_str = human_bytes(size_bytes);
            let date_str = chrono::Utc.timestamp_opt(r.modified_unix, 0)
                .single()
                .map(|dt| dt.format("%Y-%m-%d").to_string())
                .unwrap_or_default();

            let url = if web_meta.is_some() {
                r.path.to_string_lossy().to_string()
            } else {
                format!("/view/{}", urlencoding::encode(&r.path.to_string_lossy()))
            };

            let favicon = web_meta.map(|m| {
                format!("https://www.google.com/s2/favicons?domain={}", m.domain)
            });

            let reliability = web_meta.map(|m| {
                crate::ranking::reliability::compute(m, m.in_degree(), index.web.len(), ranking)
            });

            ApiResult {
                rank: offset + i + 1,
                url,
                path: r.path.to_string_lossy().to_string(),
                title: r.file_name.clone(),
                score: r.score,
                snippet: snippet_text,
                match_count: r.match_count,
                is_web: r.is_web,
                is_onion: r.is_onion,
                source_type: if r.is_web { "web".to_string() } else { "fs".to_string() },
                domain: web_meta.map(|m| m.domain.clone()),
                favicon,
                size: size_str,
                date: date_str,
                filetype: extension,
                why: r.explanation.summary_line(),
                reliability,
                explanation: r.explanation.reasons(),
            }
        })
        .collect::<Vec<_>>();

    let has_more = offset + results.len() < total;

    Ok(ApiResponse {
        query: query_str.to_string(),
        mode: format!("{:?}", mode).to_lowercase(),
        total,
        local_count,
        web_count,
        took_ms,
        results,
        has_more,
        ai_reranked,
        instant_answer: crate::answers::try_answer(query_str).as_ref().map(InstantAnswerJson::from),
    })
}

#[derive(Debug, Serialize)]
struct AskSource {
    id: usize,
    title: String,
    url: String,
    cited: bool,
}

#[derive(Debug, Serialize)]
struct AskResponse {
    query: String,
    answer: String,
    sources: Vec<AskSource>,
    ungrounded_sentences_dropped: usize,
    ai_reranked: bool,
    took_ms: f64,
}

/// Handles `GET /ask`: retrieves candidates the same way `/search` does,
/// optionally reranks them, then asks the configured LLM for a
/// citation-grounded summary. Returns `Err` (surfaced as HTTP 503, "AI
/// unavailable" rather than "your search is broken") if AI isn't
/// configured or the LLM call itself fails — a missing/failed AI feature
/// should never look like a generic server error.
fn run_ask(
    index: &Index,
    content_cache: &ContentCache,
    ranking: &crate::config::RankingConfig,
    ai_config: &crate::config::AiConfig,
    query_str: &str,
    params: &HashMap<String, String>,
) -> Result<AskResponse> {
    let started = std::time::Instant::now();

    let Some(client) = crate::ai::LlmClient::from_config(ai_config)? else {
        return Err(crate::error::NexusError::Other(
            "AI features aren't configured on this server (set [ai] enabled = true and api_key in config.toml)".to_string(),
        ));
    };

    let mode = params
        .get("mode")
        .map(|s| search::SearchMode::from_query_param(s))
        .unwrap_or_default();
    let use_rerank = params.get("rerank").map(|v| v == "1" || v == "true").unwrap_or(false);

    let ast = crate::query::parse(query_str)?;
    let candidate_limit = ai_config.rerank_top_n.max(ai_config.summary_max_sources);
    let outcome = search::search(index, &ast, ranking, 0, candidate_limit, None, mode, Some(&crate::entity::SearchContext::current_os()));

    if outcome.results.is_empty() {
        return Ok(AskResponse {
            query: query_str.to_string(),
            answer: String::new(),
            sources: Vec::new(),
            ungrounded_sentences_dropped: 0,
            ai_reranked: false,
            took_ms: started.elapsed().as_secs_f64() * 1000.0,
        });
    }

    let terms: HashSet<String> = crate::query::collect_terms(&ast);
    let load_content = |r: &search::engine::SearchResult| -> Option<String> {
        if index.web.get(r.doc_id).is_some() {
            content_cache.load(r.doc_id).ok()
        } else {
            std::fs::read_to_string(&r.path).ok()
        }
    };

    let mut ordered: Vec<&search::engine::SearchResult> = outcome.results.iter().collect();
    let mut ai_reranked = false;

    if use_rerank {
        let candidates: Vec<crate::ai::RerankCandidate> = ordered
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let snippet = load_content(r)
                    .as_deref()
                    .map(|c| snippet::generate_from_content(c, &terms).text)
                    .unwrap_or_default();
                crate::ai::RerankCandidate { id: i, title: r.file_name.clone(), snippet }
            })
            .collect();
        if let Ok(order) = crate::ai::rerank(&client, query_str, &candidates) {
            ordered = order.into_iter().map(|i| ordered[i]).collect();
            ai_reranked = true;
        }
    }

    let sources: Vec<crate::ai::SummarySource> = ordered
        .iter()
        .take(ai_config.summary_max_sources)
        .enumerate()
        .map(|(i, r)| {
            let snippet = load_content(r)
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

    let summary = crate::ai::summarize(&client, query_str, &sources)?;

    let ask_sources = sources
        .iter()
        .map(|s| AskSource {
            id: s.id,
            title: s.title.clone(),
            url: s.url_or_path.clone(),
            cited: summary.cited_source_ids.contains(&s.id),
        })
        .collect();

    Ok(AskResponse {
        query: query_str.to_string(),
        answer: summary.text,
        sources: ask_sources,
        ungrounded_sentences_dropped: summary.ungrounded_sentences_dropped,
        ai_reranked,
        took_ms: started.elapsed().as_secs_f64() * 1000.0,
    })
}

/// Resolves a `/view/` or `/open` path parameter to an actual filesystem
/// path — but only if that exact path is already present in the index as
/// a local (non-web) document.
///
/// This is the path-traversal protection for both endpoints: rather than
/// attempting to sanitize an arbitrary attacker-controlled path string
/// (blocklisting `..`, symlink checks, etc. — an easy category of bugs to
/// get subtly wrong), only paths that are already known-good (because
/// they came from indexing a real, explicitly-added folder) are ever
/// served or opened. A request for `/view/%2Fetc%2Fpasswd` or
/// `/open?path=../../etc/passwd` simply won't match anything in the
/// index and gets a 404, regardless of how the traversal is encoded.
fn resolve_indexed_path(index: &Index, raw_path_param: &str) -> Option<PathBuf> {
    let decoded = urlencoding::decode(raw_path_param).ok()?;
    if decoded.contains("..") {
        return None;
    }
    let candidate = PathBuf::from(decoded.as_ref());
    let doc_id = index.store.id_for_path(&candidate)?;
    let metadata = index.store.get(doc_id)?;
    if index.web.get(doc_id).is_some() {
        // It's a web document (its "path" is a URL, not a filesystem
        // path) — never valid for /view or /open.
        return None;
    }
    if metadata.path.exists() && metadata.path.is_file() {
        Some(metadata.path.clone())
    } else {
        None
    }
}

/// Opens `path` in the OS's default application for its file type:
/// `xdg-open` on Linux, `open` on macOS, `cmd /C start` on Windows.
fn open_in_default_app(path: &std::path::Path) -> std::result::Result<(), String> {
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(path).spawn()
    } else if cfg!(target_os = "windows") {
        std::process::Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(path)
            .spawn()
    } else {
        std::process::Command::new("xdg-open").arg(path).spawn()
    };

    match result {
        Ok(mut child) => {
            // Don't block the request thread waiting for the opened
            // application to exit (a text editor or browser can stay
            // open indefinitely) — just confirm the launcher command
            // itself started successfully, then let it run detached.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            Ok(())
        }
        Err(e) => Err(format!("failed to launch default application: {e}")),
    }
}

fn parse_query_string(query: &str) -> std::collections::HashMap<String, String> {
    url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Document, DocumentMetadata};

    fn index_with_local_file() -> (Index, PathBuf) {
        let dir = std::env::temp_dir().join(format!("nexus-api-test-{}-{}", std::process::id(), rand::random::<u32>()));
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("notes.txt");
        std::fs::write(&file_path, "hello from a real indexed file").unwrap();

        let mut index = Index::new();
        let metadata = DocumentMetadata {
            path: file_path.clone(),
            file_name: "notes.txt".to_string(),
            extension: "txt".to_string(),
            size_bytes: 30,
            modified_unix: 0,
            token_count: 0,
            acl: crate::entity::Acl::public(),
        };
        index.index_document(Document {
            metadata,
            content: "hello from a real indexed file".to_string(),
        });

        (index, file_path)
    }

    #[test]
    fn resolves_a_path_that_is_actually_indexed() {
        let (index, file_path) = index_with_local_file();
        let resolved = resolve_indexed_path(&index, &file_path.to_string_lossy());
        assert_eq!(resolved, Some(file_path.clone()));
        std::fs::remove_dir_all(file_path.parent().unwrap()).ok();
    }

    #[test]
    fn rejects_path_traversal_attempts() {
        let (index, file_path) = index_with_local_file();
        // Even if an attacker guesses the real indexed path's directory
        // and appends a traversal sequence, the literal ".." check plus
        // the "must match an indexed path exactly" rule both reject it.
        let traversal = format!("{}/../../../etc/passwd", file_path.parent().unwrap().display());
        assert!(resolve_indexed_path(&index, &traversal).is_none());
        assert!(resolve_indexed_path(&index, "../../../etc/passwd").is_none());
        assert!(resolve_indexed_path(&index, "/etc/passwd").is_none());
        std::fs::remove_dir_all(file_path.parent().unwrap()).ok();
    }

    #[test]
    fn rejects_paths_not_present_in_the_index() {
        let (index, file_path) = index_with_local_file();
        let sibling = file_path.parent().unwrap().join("not-indexed.txt");
        std::fs::write(&sibling, "not part of the index").unwrap();
        assert!(resolve_indexed_path(&index, &sibling.to_string_lossy()).is_none());
        std::fs::remove_dir_all(file_path.parent().unwrap()).ok();
    }

    #[test]
    fn rejects_web_document_paths() {
        let mut index = Index::new();
        let metadata = DocumentMetadata {
            path: PathBuf::from("https://example.com/page"),
            file_name: "Example Page".to_string(),
            extension: "html".to_string(),
            size_bytes: 10,
            modified_unix: 0,
            token_count: 0,
            acl: crate::entity::Acl::public(),
        };
        let doc_id = index.index_document(Document {
            metadata,
            content: "web content".to_string(),
        });
        index.web.insert(
            doc_id,
            crate::webdoc::WebPageMeta {
                url: "https://example.com/page".to_string(),
                domain: "example.com".to_string(),
                title: "Example Page".to_string(),
                meta_description: String::new(),
                lang: None,
                author: None,
                content_type: "html".to_string(),
                fetched_unix: 0,
                etag: None,
                last_modified: None,
                redirect_chain: Vec::new(),
                simhash: 0,
                depth: 0,
                outgoing: Vec::new(),
                incoming: Vec::new(),
                pagerank: 0.0,
                spam_score: 0.0,
                policy_flag: None,
            },
        );

        assert!(resolve_indexed_path(&index, "https://example.com/page").is_none());
    }

    #[test]
    fn empty_path_param_is_rejected() {
        let index = Index::new();
        assert!(resolve_indexed_path(&index, "").is_none());
    }
}
