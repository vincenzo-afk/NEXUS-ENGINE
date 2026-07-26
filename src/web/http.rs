//! Thin wrapper over a blocking `reqwest::Client` providing the behaviors
//! a well-mannered crawler needs: a descriptive user agent, bounded
//! redirect following (with the final URL reported back for canonical-URL
//! purposes), retry-with-backoff on transient failures, and conditional
//! (`If-None-Match` / `If-Modified-Since`) re-fetches for incremental
//! updates.

use crate::error::{NexusError, Result};
use std::thread::sleep;
use std::time::Duration;

/// Configuration for the HTTP client used by the crawler.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// The `User-Agent` header sent with every request.
    pub user_agent: String,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Maximum number of redirects to follow before giving up.
    pub max_redirects: usize,
    /// Maximum number of retry attempts for transient failures (connection
    /// errors, timeouts, and 5xx responses).
    pub max_retries: u32,
    /// Base delay for exponential backoff between retries.
    pub retry_base_delay: Duration,
}

impl Default for HttpConfig {
    fn default() -> Self {
        HttpConfig {
            user_agent: "NexusBot/1.0 (+https://github.com/vincenzo-afk/nexus; search-crawler)"
                .to_string(),
            timeout: Duration::from_secs(20),
            max_redirects: 10,
            max_retries: 3,
            retry_base_delay: Duration::from_millis(500),
        }
    }
}

/// The result of a full `GET` fetch.
#[derive(Debug, Clone)]
pub struct FetchResponse {
    /// HTTP status code.
    pub status: u16,
    /// The URL actually served, after following any redirects.
    pub final_url: String,
    /// `Content-Type` response header, lowercased, if present.
    pub content_type: Option<String>,
    /// `ETag` response header, if present.
    pub etag: Option<String>,
    /// `Last-Modified` response header, if present.
    pub last_modified: Option<String>,
    /// Response body decoded as UTF-8 (lossily, for mis-declared charsets).
    pub body: String,
    /// Raw response body bytes, for content types that aren't decoded as
    /// text (used by the PDF extractor).
    pub bytes: Vec<u8>,
}

/// The result of a `HEAD` request, used for cheap "has this changed?"
/// checks before committing to a full re-download.
#[derive(Debug, Clone)]
pub struct HeadResponse {
    /// HTTP status code.
    pub status: u16,
    /// `ETag` response header, if present.
    pub etag: Option<String>,
    /// `Last-Modified` response header, if present.
    pub last_modified: Option<String>,
}

/// A small, retrying HTTP client.
pub struct HttpClient {
    client: reqwest::blocking::Client,
    config: HttpConfig,
}

impl HttpClient {
    /// Builds a new client from `config`.
    pub fn new(config: HttpConfig) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(config.user_agent.clone())
            .timeout(config.timeout)
            .redirect(reqwest::redirect::Policy::limited(config.max_redirects))
            .build()
            .map_err(|e| NexusError::http("<client init>", e.to_string()))?;
        Ok(HttpClient { client, config })
    }

    /// Fetches `url` with a `GET` request, retrying transient failures
    /// with exponential backoff. 4xx/5xx statuses are returned as `Ok`
    /// with the status code set (after exhausting retries for 5xx) so
    /// callers can decide how to treat e.g. "not found" vs. a hard error;
    /// only connection-level failures that persist through all retries
    /// become `Err`.
    pub fn get(&self, url: &str) -> Result<FetchResponse> {
        self.with_retries(|status_of| {
            let resp = self
                .client
                .get(url)
                .send()
                .map_err(|e| NexusError::http(url, e.to_string()))?;
            let fetched = to_fetch_response(url, resp)?;
            status_of(fetched.status);
            Ok(fetched)
        })
    }

    /// Performs a `HEAD` request, used to check for updates before
    /// re-downloading a page already in the index. If the server rejects
    /// `HEAD` (405/501), callers should fall back to a full `GET`.
    pub fn head(&self, url: &str) -> Result<HeadResponse> {
        self.with_retries(|status_of| {
            let resp = self
                .client
                .head(url)
                .send()
                .map_err(|e| NexusError::http(url, e.to_string()))?;
            let head = HeadResponse {
                status: resp.status().as_u16(),
                etag: header_str(&resp, reqwest::header::ETAG),
                last_modified: header_str(&resp, reqwest::header::LAST_MODIFIED),
            };
            status_of(head.status);
            Ok(head)
        })
    }

    /// Fetches `url`, but sends `If-None-Match` / `If-Modified-Since`
    /// conditional headers derived from a previous crawl's `etag`/
    /// `last_modified`. A `304 Not Modified` response is surfaced as
    /// `Ok(None)` so the caller can skip re-indexing unchanged pages
    /// without treating it as an error.
    pub fn get_conditional(
        &self,
        url: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<Option<FetchResponse>> {
        let result = self.with_retries(|status_of| {
            let mut req = self.client.get(url);
            if let Some(tag) = etag {
                req = req.header(reqwest::header::IF_NONE_MATCH, tag);
            }
            if let Some(lm) = last_modified {
                req = req.header(reqwest::header::IF_MODIFIED_SINCE, lm);
            }
            let resp = req.send().map_err(|e| NexusError::http(url, e.to_string()))?;
            if resp.status().as_u16() == 304 {
                status_of(304);
                return Ok(FetchResponse {
                    status: 304,
                    final_url: url.to_string(),
                    content_type: None,
                    etag: etag.map(|s| s.to_string()),
                    last_modified: last_modified.map(|s| s.to_string()),
                    body: String::new(),
                    bytes: Vec::new(),
                });
            }
            let fetched = to_fetch_response(url, resp)?;
            status_of(fetched.status);
            Ok(fetched)
        })?;

        if result.status == 304 {
            Ok(None)
        } else {
            Ok(Some(result))
        }
    }

    /// Runs `op` up to `max_retries + 1` times, retrying on connection
    /// errors and on results whose reported status is `5xx`. `op` receives
    /// a `status_of` callback it must invoke with the HTTP status it
    /// obtained so the retry loop can decide whether to retry, without
    /// needing to know the concrete response type.
    fn with_retries<T>(&self, op: impl Fn(&dyn Fn(u16)) -> Result<T>) -> Result<T> {
        use std::cell::Cell;
        let mut attempt = 0;
        loop {
            let status_cell: Cell<u16> = Cell::new(0);
            let result = op(&|s| status_cell.set(s));
            match result {
                Ok(value) if status_cell.get() >= 500 && attempt < self.config.max_retries => {
                    attempt += 1;
                    sleep(self.config.retry_base_delay * 2u32.pow(attempt - 1));
                    let _ = value;
                }
                Ok(value) => return Ok(value),
                Err(_e) if attempt < self.config.max_retries => {
                    attempt += 1;
                    sleep(self.config.retry_base_delay * 2u32.pow(attempt - 1));
                }
                Err(e) => return Err(e),
            }
        }
    }
}

fn to_fetch_response(url: &str, resp: reqwest::blocking::Response) -> Result<FetchResponse> {
    let status = resp.status().as_u16();
    let final_url = resp.url().to_string();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_lowercase());
    let etag = header_str(&resp, reqwest::header::ETAG);
    let last_modified = header_str(&resp, reqwest::header::LAST_MODIFIED);
    let bytes = resp
        .bytes()
        .map_err(|e| NexusError::http(url, e.to_string()))?
        .to_vec();
    let body = String::from_utf8_lossy(&bytes).into_owned();

    Ok(FetchResponse {
        status,
        final_url,
        content_type,
        etag,
        last_modified,
        body,
        bytes,
    })
}

/// Extracts a header value as an owned `String`, if present and valid
/// UTF-8/ASCII.
fn header_str(
    resp: &reqwest::blocking::Response,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_sane_bounds() {
        let config = HttpConfig::default();
        assert!(config.timeout.as_secs() > 0);
        assert!(config.max_redirects > 0);
        assert!(config.user_agent.contains("NexusBot"));
    }
}
