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
use std::path::{Path, PathBuf};
use tiny_http::{Header, Method, Response, Server};
use chrono::TimeZone;

/// One result entry in the JSON search response.
#[derive(Debug, Serialize)]
struct ApiResult {
    rank: usize,
    url: String,
    title: String,
    score: f32,
    snippet: String,
    match_count: usize,
    is_web: bool,
    domain: Option<String>,
    size: String,
    date: String,
    filetype: String,
}

/// The full JSON response for `GET /search`.
#[derive(Debug, Serialize)]
struct ApiResponse {
    query: String,
    total: usize,
    took_ms: f64,
    results: Vec<ApiResult>,
    #[serde(rename = "hasMore")]
    has_more: bool,
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

    let server = Server::http(bind)
        .map_err(|e| crate::error::NexusError::Other(format!("failed to bind {bind}: {e}")))?;

    info!("Nexus search API listening on http://{bind}");
    println!("Nexus search API listening on http://{bind}");
    println!("  GET /search?q=<query>&limit=<n>&offset=<n>");
    println!("  GET /health");
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
        let config = config.clone();
        let f_dir = frontend_dir.clone();
        std::thread::spawn(move || {
            handle_request(request, &index, &content_cache, &ranking, &rate_limiter, &autocomplete, &config, f_dir);
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
    config: &Config,
    frontend_dir: Option<PathBuf>,
) {
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
        "/health" => {
            let body = serde_json::json!({
                "status": "ok",
                "documents": index.document_count(),
                "web_pages": index.web.len(),
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

            let response = run_search(index, content_cache, ranking, &q, offset, limit, &params);
            let mut http_response = match response {
                Ok(body) => {
                    Response::from_string(serde_json::to_string(&body).unwrap_or_default())
                        .with_header(json_header)
                }
                Err(e) => {
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
        path if path.starts_with("/view/") => {
            let file_path_str = &path["/view/".len()..];
            if let Ok(decoded) = urlencoding::decode(file_path_str) {
                let file_path = Path::new(decoded.as_ref());
                if file_path.exists() && file_path.is_file() {
                    if let Ok(content) = std::fs::read(file_path) {
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
            }
            let mut response = Response::from_string("File Not Found").with_status_code(404);
            if !config.security.cors_allowed_origins.is_empty() {
                response = response.with_header(cors_header);
            }
            let _ = request.respond(response);
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
    query_str: &str,
    offset: usize,
    limit: usize,
    params: &HashMap<String, String>,
) -> Result<ApiResponse> {
    let started = std::time::Instant::now();
    let ast = crate::query::parse(query_str)?;

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

    let (results, total) = search::search(index, &combined_ast, ranking, offset, limit, None);
    let took_ms = started.elapsed().as_secs_f64() * 1000.0;

    let terms: HashSet<String> = crate::query::collect_terms(&combined_ast);
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

            ApiResult {
                rank: offset + i + 1,
                url,
                title: r.file_name.clone(),
                score: r.score,
                snippet: snippet_text,
                match_count: r.match_count,
                is_web: web_meta.is_some(),
                domain: web_meta.map(|m| m.domain.clone()),
                size: size_str,
                date: date_str,
                filetype: extension,
            }
        })
        .collect::<Vec<_>>();

    let has_more = offset + results.len() < total;

    Ok(ApiResponse {
        query: query_str.to_string(),
        total,
        took_ms,
        results,
        has_more,
    })
}

fn parse_query_string(query: &str) -> std::collections::HashMap<String, String> {
    url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}
