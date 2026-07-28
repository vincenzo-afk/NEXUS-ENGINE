//! HTML content extraction.
//!
//! Raw HTML is mostly noise from a search-indexing point of view: markup,
//! scripts, stylesheets, navigation chrome, and ad slots. This module
//! parses the DOM with `scraper` (a `html5ever`-backed, browser-grade
//! parser) and pulls out just the signal: title, headings, body
//! paragraphs, meta description, image alt text, and outbound links with
//! their anchor text — everything the rest of Nexus needs and nothing it
//! doesn't.

use log::{debug, info};
use scraper::{Html, Selector};

/// A single outbound hyperlink found in a page, before URL resolution.
#[derive(Debug, Clone, PartialEq)]
pub struct RawLink {
    /// The raw `href` attribute value (may be relative).
    pub href: String,
    /// The link's visible text, whitespace-collapsed.
    pub anchor_text: String,
    /// `true` if the link carries `rel="nofollow"`.
    pub nofollow: bool,
}

/// Everything extracted from one HTML document.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExtractedContent {
    /// The `<title>` text, if present.
    pub title: String,
    /// `<meta name="description">` content, if present.
    pub meta_description: String,
    /// `<h1>`-`<h6>` text, in document order.
    pub headings: Vec<String>,
    /// Body paragraph and other flow-text content, in document order,
    /// with script/style/nav/footer/ad chrome removed.
    pub paragraphs: Vec<String>,
    /// `alt` text of every `<img>` with non-empty alt text.
    pub image_alt_texts: Vec<String>,
    /// Outbound `<a href="...">` links found anywhere in the document.
    pub links: Vec<RawLink>,
    /// `<meta name="author">` content, if present.
    pub author: Option<String>,
    /// The canonical URL declared via `<link rel="canonical">`, if any.
    pub canonical_url: Option<String>,
    /// `<html lang="...">` value, if declared.
    pub lang: Option<String>,
    /// RSS/Atom feed URLs declared via
    /// `<link rel="alternate" type="application/rss+xml">` (or `atom+xml`).
    pub feed_urls: Vec<String>,
}

impl ExtractedContent {
    /// The full indexable text of the page: title, headings, and
    /// paragraphs concatenated. This (not the raw HTML) is what gets
    /// tokenized and indexed.
    pub fn indexable_text(&self) -> String {
        let mut parts: Vec<&str> =
            Vec::with_capacity(2 + self.headings.len() + self.paragraphs.len());
        parts.push(self.title.as_str());
        parts.push(self.meta_description.as_str());
        for h in &self.headings {
            parts.push(h.as_str());
        }
        for p in &self.paragraphs {
            parts.push(p.as_str());
        }
        for a in &self.image_alt_texts {
            parts.push(a.as_str());
        }
        parts.retain(|s| !s.is_empty());
        parts.join(" \n ")
    }
}

/// Tag names whose entire subtree is junk and must never contribute text:
/// scripts, styles, and structural/decorative chrome that isn't the
/// article itself.
const NOISE_TAGS: &[&str] = &[
    "script", "style", "noscript", "nav", "footer", "header", "aside", "form", "button", "svg",
    "iframe", "template",
];

/// Class/ID substrings commonly used for ad slots, cookie banners, and
/// other non-content chrome. A best-effort heuristic, not a guarantee.
const NOISE_HINTS: &[&str] = &[
    "advert",
    "advertisement",
    "ads-",
    "ad-slot",
    "sponsor",
    "cookie-banner",
    "cookie-consent",
    "newsletter-signup",
    "site-nav",
    "breadcrumb",
    "social-share",
    "comment-section",
];

/// Parses `html` and extracts title, headings, paragraphs, metadata, alt
/// text, and links, discarding script/style/nav/ad chrome.
pub fn extract(html: &str) -> ExtractedContent {
    debug!("parsing HTML document ({} bytes)", html.len());
    let document = Html::parse_document(html);
    let mut result = ExtractedContent::default();

    if let Ok(sel) = Selector::parse("title") {
        if let Some(el) = document.select(&sel).next() {
            result.title = collapse_whitespace(&el.text().collect::<String>());
        }
    }

    if let Ok(sel) = Selector::parse(r#"meta[name="description" i]"#) {
        if let Some(el) = document.select(&sel).next() {
            if let Some(content) = el.value().attr("content") {
                result.meta_description = collapse_whitespace(content);
            }
        }
    }

    if let Ok(sel) = Selector::parse(r#"meta[name="author" i]"#) {
        if let Some(el) = document.select(&sel).next() {
            if let Some(content) = el.value().attr("content") {
                let author = collapse_whitespace(content);
                if !author.is_empty() {
                    result.author = Some(author);
                }
            }
        }
    }

    if let Ok(sel) = Selector::parse(r#"link[rel="canonical" i]"#) {
        if let Some(el) = document.select(&sel).next() {
            result.canonical_url = el.value().attr("href").map(|s| s.to_string());
        }
    }

    if let Ok(sel) = Selector::parse(r#"link[rel="alternate" i]"#) {
        for el in document.select(&sel) {
            let feed_type = el.value().attr("type").unwrap_or("").to_lowercase();
            if feed_type == "application/rss+xml" || feed_type == "application/atom+xml" {
                if let Some(href) = el.value().attr("href") {
                    result.feed_urls.push(href.to_string());
                }
            }
        }
    }

    if let Ok(sel) = Selector::parse("html") {
        if let Some(el) = document.select(&sel).next() {
            result.lang = el.value().attr("lang").map(|s| s.to_lowercase());
        }
    }

    if let Ok(sel) = Selector::parse("h1, h2, h3, h4, h5, h6") {
        for el in document.select(&sel) {
            if is_noise(&el) {
                continue;
            }
            let text = collapse_whitespace(&el.text().collect::<String>());
            if !text.is_empty() {
                result.headings.push(text);
            }
        }
    }

    if let Ok(sel) = Selector::parse("p, li, td, blockquote, article, section, div") {
        for el in document.select(&sel) {
            if is_noise(&el) {
                continue;
            }
            // Only take text belonging directly to this element (its
            // immediate text nodes), to avoid emitting the same sentence
            // once per ancestor container (e.g. once for `<div>`, again
            // for the `<p>` inside it).
            let text = collapse_whitespace(&direct_text(&el));
            if text.split_whitespace().count() >= 3 {
                result.paragraphs.push(text);
            }
        }
    }

    if let Ok(sel) = Selector::parse("img[alt]") {
        for el in document.select(&sel) {
            if let Some(alt) = el.value().attr("alt") {
                let alt = collapse_whitespace(alt);
                if !alt.is_empty() {
                    result.image_alt_texts.push(alt);
                }
            }
        }
    }

    if let Ok(sel) = Selector::parse("a[href]") {
        for el in document.select(&sel) {
            let Some(href) = el.value().attr("href") else {
                continue;
            };
            if href.trim().is_empty() {
                continue;
            }
            let anchor_text = collapse_whitespace(&el.text().collect::<String>());
            let nofollow = el
                .value()
                .attr("rel")
                .map(|r| r.to_lowercase().contains("nofollow"))
                .unwrap_or(false);
            result.links.push(RawLink {
                href: href.to_string(),
                anchor_text,
                nofollow,
            });
        }
    }

    // Deduplicate paragraphs that were captured at multiple nesting levels
    // (e.g. a `<div><p>text</p></div>` where neither `direct_text` call
    // fully avoided the overlap because the div had no other text).
    result.paragraphs.dedup();

    info!(
        "extracted: title='{}', paragraphs={}, links={}",
        result.title,
        result.paragraphs.len(),
        result.links.len()
    );
    result
}

/// Returns `true` if `element` (or, heuristically, its class/id) looks
/// like non-content chrome, or if it's nested inside a hard-noise tag.
fn is_noise(element: &scraper::ElementRef) -> bool {
    for ancestor in element.ancestors() {
        if let Some(el) = scraper::ElementRef::wrap(ancestor) {
            let tag = el.value().name();
            if NOISE_TAGS.contains(&tag) {
                return true;
            }
            if has_noise_hint(&el) {
                return true;
            }
        }
    }
    has_noise_hint(element)
}

fn has_noise_hint(element: &scraper::ElementRef) -> bool {
    let class = element.value().attr("class").unwrap_or("").to_lowercase();
    let id = element.value().attr("id").unwrap_or("").to_lowercase();
    NOISE_HINTS
        .iter()
        .any(|hint| class.contains(hint) || id.contains(hint))
}

/// Collects text from only the *direct* text-node children of `element`
/// (not descendant elements), so container elements don't duplicate text
/// already captured by their children.
fn direct_text(element: &scraper::ElementRef) -> String {
    use scraper::node::Node;
    let mut out = String::new();
    for child in element.children() {
        if let Node::Text(text) = child.value() {
            out.push_str(text);
            out.push(' ');
        }
    }
    out
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
<!DOCTYPE html>
<html lang="en">
<head>
  <title>  Rust Ownership Explained  </title>
  <meta name="Description" content="A guide to Rust's ownership model.">
  <link rel="canonical" href="https://example.com/rust-ownership">
  <link rel="alternate" type="application/rss+xml" href="/feed.xml">
  <link rel="alternate" type="application/atom+xml" href="https://example.com/atom.xml">
  <link rel="stylesheet" href="/style.css">
  <script>trackPageView();</script>
  <style>.ad { display:none; }</style>
</head>
<body>
  <nav><a href="/">Home</a><a href="/blog">Blog</a></nav>
  <div class="ads-banner">Buy crypto now!</div>
  <article>
    <h1>Rust Ownership Explained</h1>
    <p>Ownership is Rust's most unique feature.</p>
    <h2>Borrowing</h2>
    <p>References let you refer to a value without taking ownership of it.</p>
    <img src="diagram.png" alt="Diagram of the borrow checker">
    <p>See <a href="/related-post" rel="nofollow">this related post</a> for more.</p>
  </article>
  <footer>Copyright 2026 Example Corp</footer>
</body>
</html>
"#;

    #[test]
    fn discovers_rss_and_atom_feed_links_but_not_stylesheets() {
        let content = extract(SAMPLE);
        assert_eq!(content.feed_urls.len(), 2);
        assert!(content.feed_urls.contains(&"/feed.xml".to_string()));
        assert!(content.feed_urls.contains(&"https://example.com/atom.xml".to_string()));
        assert!(!content.feed_urls.iter().any(|u| u.contains("style.css")));
    }

    #[test]
    fn extracts_title_and_meta() {
        let content = extract(SAMPLE);
        assert_eq!(content.title, "Rust Ownership Explained");
        assert_eq!(
            content.meta_description,
            "A guide to Rust's ownership model."
        );
        assert_eq!(
            content.canonical_url.as_deref(),
            Some("https://example.com/rust-ownership")
        );
        assert_eq!(content.lang.as_deref(), Some("en"));
    }

    #[test]
    fn extracts_headings_and_paragraphs() {
        let content = extract(SAMPLE);
        assert!(content
            .headings
            .contains(&"Rust Ownership Explained".to_string()));
        assert!(content.headings.contains(&"Borrowing".to_string()));
        assert!(content
            .paragraphs
            .iter()
            .any(|p| p.contains("most unique feature")));
    }

    #[test]
    fn ignores_scripts_styles_nav_and_ads() {
        let content = extract(SAMPLE);
        let all_text = format!("{:?} {:?}", content.headings, content.paragraphs);
        assert!(!all_text.contains("trackPageView"));
        assert!(!all_text.contains("Home"));
        assert!(!all_text.contains("Buy crypto"));
        assert!(!all_text.contains("Copyright"));
    }

    #[test]
    fn extracts_alt_text_and_links() {
        let content = extract(SAMPLE);
        assert!(content
            .image_alt_texts
            .contains(&"Diagram of the borrow checker".to_string()));
        let related = content
            .links
            .iter()
            .find(|l| l.href == "/related-post")
            .expect("link present");
        assert_eq!(related.anchor_text, "this related post");
        assert!(related.nofollow);
    }

    #[test]
    fn indexable_text_combines_everything() {
        let content = extract(SAMPLE);
        let text = content.indexable_text();
        assert!(text.contains("Rust Ownership Explained"));
        assert!(text.contains("ownership model"));
        assert!(text.contains("borrow checker"));
    }
}
