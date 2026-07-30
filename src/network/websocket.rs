use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use log::{info, warn};
use serde::{Deserialize, Serialize};
use tungstenite::Message;

use crate::config::RankingConfig;
use crate::error::{NexusError, Result};
use crate::index::Index;
use crate::search::engine::SearchResult;
use crate::storage::content_cache::ContentCache;

#[derive(Debug, Deserialize)]
struct ClientMessage {
    #[serde(rename = "type")]
    msg_type: String,
    query: Option<String>,
    session_id: Option<String>,
    /// Search mode: "local", "web", "both"/"hybrid", or "tor"/"onion".
    /// Defaults to Web (matching the HTTP API's default) if omitted.
    mode: Option<String>,
    #[serde(default)]
    cancel: bool,
}

#[derive(Debug, Serialize)]
struct ResultsResponse {
    #[serde(rename = "type")]
    msg_type: String,
    results: Vec<JsonResult>,
    session_id: String,
    took_ms: f64,
}

#[derive(Debug, Serialize)]
struct JsonResult {
    doc_id: u32,
    path: String,
    file_name: String,
    size_bytes: u64,
    modified_unix: i64,
    match_count: usize,
    score: f32,
}

type QueryCache = Mutex<HashMap<String, Vec<SearchResult>>>;

pub struct WebSocketServer;

impl WebSocketServer {
    pub fn start(
        bind: &str,
        index: Arc<Index>,
        _content_cache: Arc<ContentCache>,
        ranking: Arc<RankingConfig>,
    ) -> Result<()> {
        let listener = TcpListener::bind(bind)
            .map_err(|e| NexusError::Other(format!("failed to bind WebSocket server: {}", e)))?;

        info!("WebSocket search server listening on {}", bind);

        let query_cache: Arc<QueryCache> = Arc::new(Mutex::new(HashMap::new()));

        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let index = Arc::clone(&index);
                    let ranking = Arc::clone(&ranking);
                    let query_cache = Arc::clone(&query_cache);

                    thread::spawn(move || {
                        if let Err(e) = handle_connection(stream, &index, &ranking, &query_cache) {
                            warn!("WebSocket connection handler error: {}", e);
                        }
                    });
                }
                Err(e) => {
                    warn!("WebSocket accept error: {}", e);
                }
            }
        }

        Ok(())
    }
}

fn handle_connection(
    stream: std::net::TcpStream,
    index: &Index,
    ranking: &RankingConfig,
    _query_cache: &QueryCache,
) -> Result<()> {
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .ok();

    let mut ws = tungstenite::accept(stream)
        .map_err(|e| NexusError::Other(format!("WebSocket upgrade failed: {}", e)))?;

    loop {
        let raw = match ws.read() {
            Ok(Message::Text(text)) => text,
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(Message::Ping(data)) => {
                ws.send(Message::Pong(data)).ok();
                continue;
            }
            Ok(Message::Pong(_)) | Ok(Message::Frame(_)) | Ok(Message::Binary(_)) => continue,
        };

        let client_msg: ClientMessage = match serde_json::from_str(&raw) {
            Ok(m) => m,
            Err(e) => {
                warn!("Invalid JSON from WebSocket client: {}", e);
                continue;
            }
        };

        if client_msg.msg_type != "search" {
            continue;
        }

        if client_msg.cancel {
            continue;
        }

        let session_id = client_msg.session_id.unwrap_or_default();
        let query_text = match client_msg.query {
            Some(q) if !q.is_empty() => q,
            _ => continue,
        };

        let mut current_query = query_text;
        let mut current_session = session_id;
        let mut current_mode = client_msg
            .mode
            .as_deref()
            .map(crate::search::SearchMode::from_query_param)
            .unwrap_or_default();

        // Debounce: sleep 150ms between reads; if a newer message arrives, process
        // the latest one instead
        loop {
            thread::sleep(Duration::from_millis(150));

            match try_read_message(&mut ws) {
                Ok(Some(new_msg)) => {
                    if new_msg.cancel {
                        break;
                    }
                    if let Some(q) = new_msg.query {
                        if !q.is_empty() {
                            current_query = q;
                        }
                    }
                    if let Some(s) = new_msg.session_id {
                        current_session = s;
                    }
                    if let Some(m) = new_msg.mode {
                        current_mode = crate::search::SearchMode::from_query_param(&m);
                    }
                    // Continue debounce loop with the newer message
                }
                _ => break,
            }
        }

        // If we broke out due to cancel, skip processing
        if current_query.is_empty() {
            continue;
        }

        let start = Instant::now();

        let query = match crate::query::parse(&current_query) {
            Ok(q) => q,
            Err(e) => {
                warn!("Query parse error for '{}': {}", current_query, e);
                continue;
            }
        };

        let outcome = crate::search::search(index, &query, ranking, 0, 100, None, current_mode);
        let results = outcome.results;
        let took_ms = start.elapsed().as_secs_f64() * 1000.0;

        // Always send the full current result set for this (debounced)
        // query, rather than diffing against previously-sent document ids.
        // A per-session "already sent" filter looks like an optimization
        // but is actually a correctness bug for search-as-you-type: a
        // document that matched an earlier, broader keystroke and was
        // sent once would never be resent for a later, narrower query
        // even though it still matches — so results would silently go
        // stale as the user kept typing. Result sets here are small
        // (bounded by the `limit` passed to `search`), so there's no
        // meaningful bandwidth cost to just sending the current, correct
        // set every time.
        let response = ResultsResponse {
            msg_type: "results".to_string(),
            results: results
                .iter()
                .map(|r| JsonResult {
                    doc_id: r.doc_id,
                    path: r.path.to_string_lossy().to_string(),
                    file_name: r.file_name.clone(),
                    size_bytes: r.size_bytes,
                    modified_unix: r.modified_unix,
                    match_count: r.match_count,
                    score: r.score,
                })
                .collect(),
            session_id: current_session,
            took_ms,
        };

        let json = serde_json::to_string(&response)
            .map_err(|e| NexusError::Other(format!("JSON serialization error: {}", e)))?;

        if let Err(e) = ws.send(Message::Text(json.into())) {
            warn!("WebSocket send error: {}", e);
            break;
        }
    }

    Ok(())
}

fn try_read_message(
    ws: &mut tungstenite::WebSocket<std::net::TcpStream>,
) -> std::result::Result<Option<ClientMessage>, ()> {
    match ws.read() {
        Ok(Message::Text(text)) => match serde_json::from_str(&text) {
            Ok(msg) => Ok(Some(msg)),
            Err(_) => Ok(None),
        },
        Ok(Message::Close(_)) => Err(()),
        Ok(Message::Ping(data)) => {
            ws.send(Message::Pong(data)).ok();
            Ok(None)
        }
        Ok(_) => Ok(None),
        Err(tungstenite::Error::Io(ref e))
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
        {
            Ok(None)
        }
        Err(_) => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Document, DocumentMetadata};
    use std::net::TcpStream;
    use std::path::PathBuf;

    fn index_with_docs() -> Index {
        let mut index = Index::new();
        for (path, content) in [
            ("/a.txt", "rust programming language"),
            ("/b.txt", "rust web assembly target"),
            ("/c.txt", "python programming language"),
        ] {
            let metadata = DocumentMetadata {
                path: PathBuf::from(path),
                file_name: path.trim_start_matches('/').to_string(),
                extension: "txt".to_string(),
                size_bytes: content.len() as u64,
                modified_unix: 0,
                token_count: 0,
            };
            index.index_document(Document {
                metadata,
                content: content.to_string(),
            });
        }
        index
    }

    fn free_port() -> u16 {
        // Binding to port 0 and reading back the assigned port is the
        // standard way to get a free ephemeral port for a test server.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }

    #[test]
    fn full_result_set_is_resent_as_query_narrows() {
        let port = free_port();
        let bind = format!("127.0.0.1:{port}");
        let index = Arc::new(index_with_docs());
        let content_cache = Arc::new(ContentCache::new(std::env::temp_dir().join("nexus-ws-test-cache")));
        let ranking = Arc::new(RankingConfig::default());

        let bind_for_server = bind.clone();
        thread::spawn(move || {
            let _ = WebSocketServer::start(&bind_for_server, index, content_cache, ranking);
        });

        // Give the listener a moment to come up.
        let mut stream = None;
        for _ in 0..50 {
            if let Ok(s) = TcpStream::connect(&bind) {
                stream = Some(s);
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let stream = stream.expect("server should be listening");
        let (mut ws, _) = tungstenite::client(format!("ws://{bind}"), stream).unwrap();
        ws.get_mut().set_read_timeout(Some(Duration::from_secs(5))).ok();

        // Broad query first: both rust documents should come back.
        // This module is not wired up to any CLI command in this codebase
        // (api::websocket::start_ws_server is the one `nexus serve-ws`
        // actually calls) — kept in sync anyway since it's compiled and
        // tested, but "both" mode is specified explicitly here since the
        // fixture docs below are local files and this module's default
        // mode (matching the rest of the app) is now Web.
        let broad = serde_json::json!({"type": "search", "query": "rust", "session_id": "s1", "mode": "both"});
        ws.send(Message::Text(broad.to_string().into())).unwrap();
        let reply = read_results(&mut ws);
        assert_eq!(reply.results.len(), 2);

        // Narrower query in the *same session*: only one doc matches now.
        // Before the fix, this doc would have been silently dropped from
        // the response because it was already "seen" in this session.
        let narrow = serde_json::json!({"type": "search", "query": "assembly", "session_id": "s1", "mode": "both"});
        ws.send(Message::Text(narrow.to_string().into())).unwrap();
        let reply = read_results(&mut ws);
        assert_eq!(reply.results.len(), 1, "narrowed query must still return its match, even though that doc was already sent once this session");
        assert!(reply.results[0].file_name.contains("b.txt"));
    }

    fn read_results(ws: &mut tungstenite::WebSocket<TcpStream>) -> ResultsResponseForTest {
        loop {
            match ws.read().unwrap() {
                Message::Text(text) => {
                    return serde_json::from_str(&text).unwrap();
                }
                _ => continue,
            }
        }
    }

    #[derive(Debug, Deserialize)]
    struct ResultsResponseForTest {
        results: Vec<JsonResultForTest>,
    }

    #[derive(Debug, Deserialize)]
    struct JsonResultForTest {
        file_name: String,
    }
}
