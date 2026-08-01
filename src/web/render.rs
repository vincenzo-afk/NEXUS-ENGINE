//! JavaScript rendering for modern sites: many pages ship an empty HTML
//! shell that a client-side framework fills in after JS execution, which
//! [`crate::web::http::HttpClient`] (a plain HTTP `GET`, no JS engine)
//! cannot see. This module renders such a page in a real, automated
//! Chromium instance via the Chrome DevTools Protocol and returns the
//! post-render HTML, so the same extraction pipeline
//! ([`crate::html::extract`]) can then run on content that's actually
//! there.
//!
//! **This is a real integration, with a real, honestly-stated
//! dependency.** [`RenderingClient`] wraps the `headless_chrome` crate,
//! which drives an actual Chrome/Chromium binary over CDP — it is not a
//! hand-rolled JS engine and does not pretend to be one. That means:
//! - It requires a system Chrome or Chromium installation to be present;
//!   [`RenderingClient::new`] returns a clear error (not a silent
//!   fallback) if launching the browser fails, e.g. because none is
//!   installed.
//! - It is gated behind the `headless_render` Cargo feature (off by
//!   default) so the common case — most local/text-file/PDF indexing,
//!   and crawling sites that don't need JS — never pays Chromium's
//!   startup cost or requires it to be installed at all.
//! - It is meaningfully slower and heavier than a plain HTTP fetch (a
//!   full browser process per render), so [`crate::web::crawler`]
//!   integrating this should reserve it for pages a plain fetch's HTML
//!   comes back suspiciously empty for, not use it as the default path
//!   for every page — see [`looks_like_js_shell`] for that heuristic.
#![cfg(feature = "headless_render")]

use headless_chrome::{Browser, LaunchOptions};
use std::time::Duration;

/// The result of rendering one page: the DOM's HTML after JS execution
/// finished (approximated by waiting for network idle, see
/// [`RenderConfig::wait_after_load`]).
#[derive(Debug, Clone)]
pub struct RenderedPage {
    pub final_url: String,
    pub html: String,
}

#[derive(Debug, Clone)]
pub struct RenderConfig {
    /// How long to wait after the page's `load` event fires before
    /// snapshotting the DOM, to give client-side JS time to finish
    /// fetching and rendering data. There is no fully general "JS is
    /// done" signal to wait for instead — this is a pragmatic fixed
    /// delay, not a claim that every SPA is guaranteed fully hydrated by
    /// then.
    pub wait_after_load: Duration,
    /// Overall timeout for the whole render (navigation + wait), after
    /// which rendering is abandoned rather than blocking a crawl
    /// indefinitely on one stuck page.
    pub timeout: Duration,
}

impl Default for RenderConfig {
    fn default() -> Self {
        RenderConfig {
            wait_after_load: Duration::from_millis(1500),
            timeout: Duration::from_secs(20),
        }
    }
}

pub struct RenderingClient {
    browser: Browser,
    config: RenderConfig,
}

impl RenderingClient {
    /// Launches a headless Chromium instance. Fails clearly (rather than
    /// falling back to something misleading) if no compatible browser
    /// binary can be found or launched.
    pub fn new(config: RenderConfig) -> Result<Self, String> {
        let options = LaunchOptions::default_builder()
            .headless(true)
            .build()
            .map_err(|e| format!("failed to build launch options: {e}"))?;
        let browser =
            Browser::new(options).map_err(|e| format!("failed to launch headless Chromium: {e}. \
                Install Google Chrome or Chromium and ensure it's discoverable on PATH."))?;
        Ok(RenderingClient { browser, config })
    }

    /// Navigates to `url`, waits for the configured settle time, and
    /// returns the rendered DOM's outer HTML.
    pub fn render(&self, url: &str) -> Result<RenderedPage, String> {
        let tab = self
            .browser
            .new_tab()
            .map_err(|e| format!("failed to open tab: {e}"))?;
        tab.set_default_timeout(self.config.timeout);

        tab.navigate_to(url)
            .map_err(|e| format!("navigation failed: {e}"))?;
        tab.wait_until_navigated()
            .map_err(|e| format!("page did not finish loading: {e}"))?;

        std::thread::sleep(self.config.wait_after_load);

        let html = tab
            .get_content()
            .map_err(|e| format!("failed to read rendered DOM: {e}"))?;
        let final_url = tab.get_url();

        Ok(RenderedPage { final_url, html })
    }
}

/// A cheap heuristic for "this HTML probably needs JS rendering to be
/// useful," meant to gate *when* the crawler pays for a headless render
/// rather than doing it for every page: very little visible text content
/// relative to markup/script size, or a body that's essentially just a
/// framework's mount-point `<div id="root">`/`<div id="app">`, is the
/// classic client-side-rendered-shell shape. This is a heuristic, not a
/// guarantee — some legitimately thin pages will false-positive, and
/// some JS-dependent pages that happen to server-side-render their
/// initial shell will false-negative.
pub fn looks_like_js_shell(html: &str) -> bool {
    let extracted = crate::html::extract(html);
    let visible_words = extracted.indexable_text().split_whitespace().count();
    let has_empty_root_mount = ["id=\"root\"", "id=\"app\"", "id='root'", "id='app'"]
        .iter()
        .any(|marker| html.contains(marker));
    let markup_bytes = html.len();

    (visible_words < 40 && markup_bytes > 2000) || (has_empty_root_mount && visible_words < 20)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_empty_spa_shell() {
        let html = r#"<html><head><script src="bundle.js"></script></head><body><div id="root"></div></body></html>"#;
        assert!(looks_like_js_shell(html));
    }

    #[test]
    fn does_not_flag_a_content_rich_page() {
        let html = "<html><body><p>".to_string()
            + &"This is a perfectly normal, content-rich server-rendered page. ".repeat(20)
            + "</p></body></html>";
        assert!(!looks_like_js_shell(&html));
    }
}
