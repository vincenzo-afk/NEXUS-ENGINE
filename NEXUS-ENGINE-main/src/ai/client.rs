//! A minimal client for OpenAI-compatible chat completion endpoints,
//! shared by [`crate::ai::rerank`] and [`crate::ai::summarize`].
//!
//! Nothing here runs unless the caller explicitly constructs an
//! [`LlmClient`] from an [`AiConfig`] with `enabled = true` and a
//! non-empty `api_key` — see [`LlmClient::from_config`].

use crate::config::AiConfig;
use crate::error::{NexusError, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Serialize)]
struct ChatMessage {
    role: &'static str,
    content: String,
}

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    /// `0.0` (deterministic-as-possible) rather than a creative default:
    /// both reranking and grounded summarization want the model to stick
    /// closely to the provided material, not improvise.
    temperature: f32,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ChatResponseMessage {
    content: String,
}

/// A client for one configured OpenAI-compatible endpoint.
pub struct LlmClient {
    http: reqwest::blocking::Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl LlmClient {
    /// Builds a client from `config`, or returns `None` if AI features
    /// aren't configured (`enabled = false` or no `api_key`) — the
    /// standard way callers should check "is this available" before
    /// trying to use it, rather than constructing a client and having
    /// every call fail.
    pub fn from_config(config: &AiConfig) -> Result<Option<LlmClient>> {
        if !config.enabled || config.api_key.trim().is_empty() {
            return Ok(None);
        }
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(config.timeout_seconds))
            .build()
            .map_err(|e| NexusError::Other(format!("failed to build AI HTTP client: {e}")))?;
        Ok(Some(LlmClient {
            http,
            base_url: config.api_base_url.trim_end_matches('/').to_string(),
            api_key: config.api_key.clone(),
            model: config.model.clone(),
        }))
    }

    /// Sends a chat completion request with a system + user message pair
    /// and returns the model's reply text.
    pub fn chat(&self, system_prompt: &str, user_prompt: &str) -> Result<String> {
        let request = ChatRequest {
            model: self.model.clone(),
            messages: vec![
                ChatMessage { role: "system", content: system_prompt.to_string() },
                ChatMessage { role: "user", content: user_prompt.to_string() },
            ],
            temperature: 0.0,
        };

        let url = format!("{}/chat/completions", self.base_url);
        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&request)
            .send()
            .map_err(|e| NexusError::http(&url, format!("AI request failed: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            return Err(NexusError::http(
                &url,
                format!("AI endpoint returned {status}: {body}"),
            ));
        }

        let parsed: ChatResponse = response
            .json()
            .map_err(|e| NexusError::http(&url, format!("failed to parse AI response: {e}")))?;

        parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| NexusError::Other("AI endpoint returned no choices".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_config_returns_none_when_disabled() {
        let config = AiConfig { enabled: false, api_key: "sk-test".to_string(), ..AiConfig::default() };
        assert!(LlmClient::from_config(&config).unwrap().is_none());
    }

    #[test]
    fn from_config_returns_none_when_api_key_empty() {
        let config = AiConfig { enabled: true, api_key: String::new(), ..AiConfig::default() };
        assert!(LlmClient::from_config(&config).unwrap().is_none());
    }

    #[test]
    fn from_config_returns_client_when_properly_configured() {
        let config = AiConfig { enabled: true, api_key: "sk-test".to_string(), ..AiConfig::default() };
        assert!(LlmClient::from_config(&config).unwrap().is_some());
    }

    /// Spins up a real local HTTP server implementing just enough of the
    /// OpenAI chat-completions schema to prove `LlmClient` actually
    /// builds a correct request and parses a correct response — not a
    /// mocked-out unit test, an end-to-end one over a real socket.
    #[test]
    fn chat_sends_correct_request_and_parses_response() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).ok();

            // A single read() may only capture part of the request (TCP
            // makes no promise the client's headers+body arrive in one
            // segment) — read in a loop until we've seen the full
            // request (end of headers plus, if present, the
            // Content-Length-declared body) rather than asserting on a
            // possibly-truncated first chunk.
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = stream.read(&mut chunk).unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let text_so_far = String::from_utf8_lossy(&buf);
                if let Some(header_end) = text_so_far.find("\r\n\r\n") {
                    let content_length = text_so_far
                        .lines()
                        .find(|l| l.to_lowercase().starts_with("content-length:"))
                        .and_then(|l| l.split(':').nth(1))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    let body_so_far = buf.len().saturating_sub(header_end + 4);
                    if body_so_far >= content_length {
                        break;
                    }
                }
            }
            let request_text = String::from_utf8_lossy(&buf).to_string();

            // Confirm the request actually looks like what we expect
            // before responding, so a broken request-builder would fail
            // this test rather than just being ignored by a lenient mock.
            assert!(request_text.contains("POST /chat/completions"));
            assert!(request_text.to_lowercase().contains("authorization: bearer sk-test"));
            assert!(request_text.contains("\"model\":\"test-model\""));

            let body = r#"{"choices":[{"message":{"content":"mocked reply"}}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().ok();
        });

        let config = AiConfig {
            enabled: true,
            api_base_url: format!("http://127.0.0.1:{port}"),
            api_key: "sk-test".to_string(),
            model: "test-model".to_string(),
            ..AiConfig::default()
        };
        let client = LlmClient::from_config(&config).unwrap().unwrap();
        let reply = client.chat("system prompt", "user prompt").unwrap();
        assert_eq!(reply, "mocked reply");

        server.join().unwrap();
    }
}
