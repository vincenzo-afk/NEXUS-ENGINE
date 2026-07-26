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

use crate::config::Config;
use crate::error::Result;
use crate::index::Index;
use crate::search::{self, snippet};
use crate::storage::content_cache::ContentCache;
use serde::Serialize;
use std::collections::HashSet;
use std::sync::Arc;
use tiny_http::{Header, Method, Response, Server};

/// One result entry in the JSON search response.
#[derive(Debug, Serialize)]
struct ApiResult {
    rank: usize,
    path: String,
    title: String,
    score: f32,
    snippet: String,
    match_count: usize,
    is_web: bool,
    domain: Option<String>,
}

/// The full JSON response for `GET /search`.
#[derive(Debug, Serialize)]
struct ApiResponse {
    query: String,
    total_returned: usize,
    took_ms: f64,
    results: Vec<ApiResult>,
}

#[derive(Debug, Serialize)]
struct ApiError {
    error: String,
}

/// Starts the search API server on `bind` (e.g. `"127.0.0.1:8080"`),
/// serving from the index and configuration currently on disk. Blocks the
/// current thread forever (or until the process is killed).
pub fn serve(bind: &str, config: &Config) -> Result<()> {
    let index = if crate::storage::exists(&config.index_path) {
        crate::storage::load(&config.index_path)?
    } else {
        Index::new()
    };
    let index = Arc::new(index);
    let content_cache = Arc::new(ContentCache::new(config.content_cache_dir.clone()));
    let ranking = Arc::new(config.ranking.clone());

    let server = Server::http(bind)
        .map_err(|e| crate::error::NexusError::Other(format!("failed to bind {bind}: {e}")))?;

    println!("Nexus search API listening on http://{bind}");
    println!("  GET /search?q=<query>&limit=<n>&offset=<n>");
    println!("  GET /health");

    for request in server.incoming_requests() {
        let index = Arc::clone(&index);
        let content_cache = Arc::clone(&content_cache);
        let ranking = Arc::clone(&ranking);
        handle_request(request, &index, &content_cache, &ranking);
    }

    Ok(())
}

fn handle_request(
    request: tiny_http::Request,
    index: &Index,
    content_cache: &ContentCache,
    ranking: &crate::config::RankingConfig,
) {
    let url = request.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let json_header = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
        .expect("static header is always valid");

    if request.method() != &Method::Get {
        let body = serde_json::to_string(&ApiError {
            error: "only GET is supported".to_string(),
        })
        .unwrap_or_default();
        let _ = request.respond(Response::from_string(body).with_status_code(405).with_header(json_header));
        return;
    }

    match path {
        "/health" => {
            let body = serde_json::json!({
                "status": "ok",
                "documents": index.document_count(),
                "web_pages": index.web.len(),
            })
            .to_string();
            let _ = request.respond(Response::from_string(body).with_header(json_header));
        }
        "/search" => {
            let params = parse_query_string(query);
            let q = params.get("q").cloned().unwrap_or_default();
            if q.trim().is_empty() {
                let body = serde_json::to_string(&ApiError {
                    error: "missing required query parameter 'q'".to_string(),
                })
                .unwrap_or_default();
                let _ = request.respond(Response::from_string(body).with_status_code(400).with_header(json_header));
                return;
            }
            let limit: usize = params.get("limit").and_then(|v| v.parse().ok()).unwrap_or(10);
            let offset: usize = params.get("offset").and_then(|v| v.parse().ok()).unwrap_or(0);

            let response = run_search(index, content_cache, ranking, &q, offset, limit);
            match response {
                Ok(body) => {
                    let _ = request.respond(
                        Response::from_string(serde_json::to_string(&body).unwrap_or_default())
                            .with_header(json_header),
                    );
                }
                Err(e) => {
                    let body = serde_json::to_string(&ApiError { error: e.to_string() }).unwrap_or_default();
                    let _ = request.respond(Response::from_string(body).with_status_code(400).with_header(json_header));
                }
            }
        }
        _ => {
            let body = serde_json::to_string(&ApiError {
                error: format!("not found: {path}"),
            })
            .unwrap_or_default();
            let _ = request.respond(Response::from_string(body).with_status_code(404).with_header(json_header));
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
) -> Result<ApiResponse> {
    let started = std::time::Instant::now();
    let ast = crate::query::parse(query_str)?;
    let results = search::search(index, &ast, ranking, offset, limit, None);
    let took_ms = started.elapsed().as_secs_f64() * 1000.0;

    let terms: HashSet<String> = crate::query::collect_terms(&ast);
    let results = results
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            let web_meta = index.web.get(r.doc_id);
            let content = match web_meta {
                Some(_) => content_cache.load(r.doc_id).ok(),
                None => std::fs::read_to_string(&r.path).ok(),
            };
            let snippet_text = content
                .map(|c| snippet::generate_from_content(&c, &terms).text)
                .unwrap_or_default();

            ApiResult {
                rank: offset + i + 1,
                path: r.path.to_string_lossy().to_string(),
                title: r.file_name.clone(),
                score: r.score,
                snippet: snippet_text,
                match_count: r.match_count,
                is_web: web_meta.is_some(),
                domain: web_meta.map(|m| m.domain.clone()),
            }
        })
        .collect::<Vec<_>>();

    Ok(ApiResponse {
        query: query_str.to_string(),
        total_returned: results.len(),
        took_ms,
        results,
    })
}

fn parse_query_string(query: &str) -> std::collections::HashMap<String, String> {
    url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}
