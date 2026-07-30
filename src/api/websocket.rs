use crate::api::rate_limit::RateLimiter;
use crate::config::RankingConfig;
use crate::index::Index;
use crate::storage::content_cache::ContentCache;
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Instant;
use tungstenite::{accept, Message};

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct WsRequest {
    #[serde(rename = "type")]
    msg_type: String,
    query: Option<String>,
    session_id: Option<String>,
    cancel: Option<bool>,
    limit: Option<usize>,
    /// Search mode: "local", "web", "both"/"hybrid", or "tor"/"onion".
    /// Defaults to Web if omitted.
    mode: Option<String>,
}

#[derive(Debug, Serialize)]
struct WsResponse {
    #[serde(rename = "type")]
    msg_type: String,
    results: Vec<serde_json::Value>,
    session_id: String,
    took_ms: f64,
    total: usize,
}

#[derive(Debug, Serialize)]
struct WsError {
    error: String,
}

/// Handle an incoming WebSocket connection.
pub fn handle_websocket(
    stream: TcpStream,
    index: Arc<Index>,
    content_cache: Arc<ContentCache>,
    ranking: Arc<RankingConfig>,
    rate_limiter: Arc<RateLimiter>,
    peer_addr: String,
) {
    let ws = match accept(stream) {
        Ok(ws) => ws,
        Err(e) => {
            warn!("WebSocket handshake failed from {}: {}", peer_addr, e);
            return;
        }
    };

    let mut ws = ws;
    let session_id = format!("ws-{}", rand::random::<u64>());
    info!(
        "WebSocket session '{}' opened from {}",
        session_id, peer_addr
    );

    let rate_key = format!("ws:{}", session_id);

    loop {
        let msg = match ws.read() {
            Ok(msg) => msg,
            Err(tungstenite::Error::ConnectionClosed)
            | Err(tungstenite::Error::Protocol(_))
            | Err(tungstenite::Error::Utf8) => {
                debug!("WebSocket session '{}' disconnected", session_id);
                break;
            }
            Err(e) => {
                warn!("WebSocket read error on '{}': {}", session_id, e);
                break;
            }
        };

        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(_) => {
                let _ = ws.send(Message::Text(
                    serde_json::to_string(&WsError {
                        error: "binary messages are not supported".to_string(),
                    })
                    .unwrap_or_default(),
                ));
                continue;
            }
            Message::Ping(data) => {
                let _ = ws.send(Message::Pong(data));
                continue;
            }
            Message::Pong(_) => continue,
            Message::Close(_) => break,
            Message::Frame(_) => continue,
        };

        let request: WsRequest = match serde_json::from_str(&text) {
            Ok(r) => r,
            Err(e) => {
                let err = serde_json::to_string(&WsError {
                    error: format!("invalid JSON: {}", e),
                })
                .unwrap_or_default();
                let _ = ws.send(Message::Text(err));
                continue;
            }
        };

        if request.cancel.unwrap_or(false) {
            let _ = ws.send(Message::Text(
                serde_json::to_string(
                    &serde_json::json!({"type": "cancelled", "session_id": session_id}),
                )
                .unwrap_or_default(),
            ));
            continue;
        }

        if !rate_limiter.consume(&rate_key, 1) {
            let _ = ws.send(Message::Text(
                serde_json::to_string(&WsError {
                    error: "rate limit exceeded, please slow down".to_string(),
                })
                .unwrap_or_default(),
            ));
            continue;
        }

        let query_text = match &request.query {
            Some(q) if !q.trim().is_empty() => q.trim().to_string(),
            _ => {
                let _ = ws.send(Message::Text(
                    serde_json::to_string(&WsError {
                        error: "missing or empty 'query' field".to_string(),
                    })
                    .unwrap_or_default(),
                ));
                continue;
            }
        };

        if let Err(e) = rate_limiter.validate_query(&query_text) {
            let _ = ws.send(Message::Text(
                serde_json::to_string(&WsError { error: e }).unwrap_or_default(),
            ));
            continue;
        }

        let limit = request.limit.unwrap_or(10);
        if let Err(e) = rate_limiter.validate_pagination(0, limit) {
            let _ = ws.send(Message::Text(
                serde_json::to_string(&WsError { error: e }).unwrap_or_default(),
            ));
            continue;
        }

        let started = Instant::now();

        let ast = match crate::query::parse(&query_text) {
            Ok(ast) => ast,
            Err(e) => {
                let err = serde_json::to_string(&WsError {
                    error: format!("query parse error: {}", e),
                })
                .unwrap_or_default();
                let _ = ws.send(Message::Text(err));
                continue;
            }
        };

        let mode = request
            .mode
            .as_deref()
            .map(crate::search::SearchMode::from_query_param)
            .unwrap_or_default();
        let outcome = crate::search::search(&index, &ast, &ranking, 0, limit, None, mode);
        let raw_results = outcome.results;

        let terms: std::collections::HashSet<String> = crate::query::collect_terms(&ast);

        let results: Vec<serde_json::Value> = raw_results
            .into_iter()
            .enumerate()
            .map(|(i, r)| {
                let web_meta = index.web.get(r.doc_id);
                let content = match web_meta {
                    Some(_) => content_cache.load(r.doc_id).ok(),
                    None => std::fs::read_to_string(&r.path).ok(),
                };
                let snippet_text = content
                    .as_ref()
                    .and_then(|c| {
                        Some(crate::search::snippet::generate_from_content(c, &terms).text)
                    })
                    .unwrap_or_default();

                serde_json::json!({
                    "rank": i + 1,
                    "path": r.path.to_string_lossy().to_string(),
                    "title": r.file_name,
                    "score": r.score,
                    "snippet": snippet_text,
                    "match_count": r.match_count,
                    "is_web": web_meta.is_some(),
                    "domain": web_meta.map(|m| m.domain.clone()),
                })
            })
            .collect();

        let took_ms = started.elapsed().as_secs_f64() * 1000.0;
        let total = outcome.total;

        let response = WsResponse {
            msg_type: "search_results".to_string(),
            results,
            session_id: session_id.clone(),
            took_ms,
            total,
        };

        let payload = serde_json::to_string(&response).unwrap_or_default();
        if let Err(e) = ws.send(Message::Text(payload)) {
            warn!("WebSocket send error on '{}': {}", session_id, e);
            break;
        }
    }

    info!("WebSocket session '{}' closed", session_id);
}

/// The WebSocket server entry point.
pub fn start_ws_server(
    bind: &str,
    index: Arc<Index>,
    content_cache: Arc<ContentCache>,
    ranking: Arc<RankingConfig>,
    rate_limiter: Arc<RateLimiter>,
) -> Result<(), crate::error::NexusError> {
    let listener = std::net::TcpListener::bind(bind).map_err(|e| {
        crate::error::NexusError::Other(format!("failed to bind WebSocket server on {bind}: {e}"))
    })?;
    info!("WebSocket server listening on ws://{}", bind);

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let peer = stream
                    .peer_addr()
                    .map(|a| a.to_string())
                    .unwrap_or_default();
                let index = Arc::clone(&index);
                let content_cache = Arc::clone(&content_cache);
                let ranking = Arc::clone(&ranking);
                let rate_limiter = Arc::clone(&rate_limiter);
                std::thread::spawn(move || {
                    handle_websocket(stream, index, content_cache, ranking, rate_limiter, peer);
                });
            }
            Err(e) => warn!("WebSocket accept error: {}", e),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_ws_request_with_query() {
        let json = r#"{"type": "search", "query": "hello world", "limit": 20}"#;
        let req: WsRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.msg_type, "search");
        assert_eq!(req.query, Some("hello world".to_string()));
        assert_eq!(req.limit, Some(20));
        assert!(req.session_id.is_none());
        assert!(req.cancel.is_none());
    }

    #[test]
    fn deserializes_ws_request_cancel() {
        let json = r#"{"type": "cancel", "cancel": true}"#;
        let req: WsRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.msg_type, "cancel");
        assert_eq!(req.cancel, Some(true));
    }

    #[test]
    fn deserializes_ws_request_with_session() {
        let json = r#"{"type": "search", "query": "test", "session_id": "abc123"}"#;
        let req: WsRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.session_id, Some("abc123".to_string()));
    }

    #[test]
    fn ws_response_serializes() {
        let resp = WsResponse {
            msg_type: "search_results".to_string(),
            results: vec![
                serde_json::json!({"rank": 1, "path": "/tmp/doc.txt", "title": "doc.txt"}),
            ],
            session_id: "sess-1".to_string(),
            took_ms: 1.5,
            total: 1,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"type\":\"search_results\""));
        assert!(json.contains("\"session_id\":\"sess-1\""));
        assert!(json.contains("\"total\":1"));
        assert!(json.contains("\"rank\":1"));
    }

    #[test]
    fn ws_error_serializes() {
        let err = WsError {
            error: "something went wrong".to_string(),
        };
        let json = serde_json::to_string(&err).unwrap();
        assert_eq!(json, r#"{"error":"something went wrong"}"#);
    }
}
