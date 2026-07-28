//! RSS 2.0 and Atom feed parsing.
//!
//! Feeds are the fastest way to discover new content on a site — a fresh
//! blog post or news article often shows up in the feed within minutes,
//! long before it would be picked up by a stale `sitemap.xml` or found by
//! following links from the homepage. This module extracts item/entry
//! links (and titles) from either format; malformed feed XML degrades to
//! an empty result rather than erroring, since a broken feed on one site
//! shouldn't abort a crawl.

use quick_xml::events::Event;
use quick_xml::reader::Reader;

/// One item/entry discovered in a feed.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedItem {
    /// The item's/entry's link URL (may be relative; resolve against the
    /// feed's own URL before enqueueing).
    pub url: String,
    /// The item's/entry's title, if present.
    pub title: String,
}

/// The result of parsing a feed document.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedFeed {
    /// Items/entries found, in document order (typically newest-first for
    /// most feeds, but that's a convention, not something this parser
    /// enforces).
    pub items: Vec<FeedItem>,
}

/// Which element we're currently inside, for purposes of routing text
/// content (RSS `<item>` vs Atom `<entry>` both have a `<title>` child,
/// but so does the feed's own `<channel>`/`<feed>` root — we only want
/// per-item titles).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Scope {
    Root,
    Item,
}

/// Parses `xml` as either an RSS 2.0 or Atom feed, auto-detecting which
/// based on the elements actually present (`<item>` for RSS, `<entry>`
/// for Atom) rather than requiring a specific root tag, since some feeds
/// omit or vary the XML namespace declarations that would otherwise be
/// the reliable signal.
pub fn parse(xml: &str) -> ParsedFeed {
    let mut reader = Reader::from_str(xml);
    reader.trim_text(true);

    let mut items = Vec::new();
    let mut scope = Scope::Root;
    let mut in_title = false;
    // RSS's <link> is a bare element whose *text content* is the URL
    // (`<link>https://...</link>`); Atom's is a self-closing element with
    // an `href` attribute (`<link href="..."/>`). This flag is set when
    // we've entered a `<link>` that had no `href` attribute, so the
    // following Text event (RSS-style) is known to be a URL rather than
    // some other text we should ignore.
    let mut awaiting_rss_link_text = false;
    let mut current_url: Option<String> = None;
    let mut current_title = String::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = local_name(&e.name().as_ref().to_vec());
                match name.as_str() {
                    "item" | "entry" => {
                        scope = Scope::Item;
                        current_url = None;
                        current_title.clear();
                    }
                    "title" if scope == Scope::Item => in_title = true,
                    "link" if scope == Scope::Item => {
                        if let Some(href) = atom_style_href(&e) {
                            if current_url.is_none() {
                                current_url = Some(href);
                            }
                        } else {
                            // No href attribute: this is RSS's bare
                            // <link>text</link> form.
                            awaiting_rss_link_text = true;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) => {
                // Atom's <link .../> is usually a self-closing (empty)
                // element rather than Start+End, so it must be handled
                // here too, not just in the Start branch.
                let name = local_name(&e.name().as_ref().to_vec());
                if name == "link" && scope == Scope::Item {
                    if let Some(href) = atom_style_href(&e) {
                        if current_url.is_none() {
                            current_url = Some(href);
                        }
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if in_title {
                    if let Ok(text) = t.unescape() {
                        current_title.push_str(text.trim());
                    }
                } else if awaiting_rss_link_text && current_url.is_none() {
                    if let Ok(text) = t.unescape() {
                        let url = text.trim().to_string();
                        if !url.is_empty() {
                            current_url = Some(url);
                        }
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = local_name(&e.name().as_ref().to_vec());
                match name.as_str() {
                    "title" => in_title = false,
                    "link" => awaiting_rss_link_text = false,
                    "item" | "entry" => {
                        if let Some(url) = current_url.take() {
                            items.push(FeedItem {
                                url,
                                title: current_title.clone(),
                            });
                        }
                        scope = Scope::Root;
                        current_title.clear();
                        awaiting_rss_link_text = false;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    ParsedFeed { items }
}

/// Extracts `href` from an Atom-style `<link href="..." rel="...">`
/// element, but only if `rel` is `"alternate"` or unspecified (which
/// defaults to `"alternate"` per the Atom spec) — this avoids picking up
/// `rel="self"` (the feed's own URL) or `rel="enclosure"` (an attached
/// media file) instead of the actual article link.
fn atom_style_href(e: &quick_xml::events::BytesStart) -> Option<String> {
    let mut href = None;
    let mut rel = None;
    for attr in e.attributes().flatten() {
        match attr.key.as_ref() {
            b"href" => href = attr.unescape_value().ok().map(|v| v.into_owned()),
            b"rel" => rel = attr.unescape_value().ok().map(|v| v.into_owned()),
            _ => {}
        }
    }
    let is_alternate = rel.as_deref().map(|r| r == "alternate").unwrap_or(true);
    if is_alternate {
        href
    } else {
        None
    }
}

fn local_name(qname: &[u8]) -> String {
    let s = String::from_utf8_lossy(qname);
    s.rsplit(':').next().unwrap_or(&s).to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSS_SAMPLE: &str = r#"<?xml version="1.0"?>
<rss version="2.0">
<channel>
  <title>Example Blog</title>
  <link>https://example.com/</link>
  <item>
    <title>First Post</title>
    <link>https://example.com/posts/first</link>
    <pubDate>Mon, 01 Jan 2024 00:00:00 GMT</pubDate>
  </item>
  <item>
    <title>Second Post</title>
    <link>https://example.com/posts/second</link>
  </item>
</channel>
</rss>"#;

    const ATOM_SAMPLE: &str = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Example Blog</title>
  <entry>
    <title>Atom Entry One</title>
    <link rel="self" href="https://example.com/feed/atom-one"/>
    <link rel="alternate" href="https://example.com/posts/atom-one"/>
  </entry>
  <entry>
    <title>Atom Entry Two</title>
    <link href="https://example.com/posts/atom-two"/>
  </entry>
</feed>"#;

    #[test]
    fn parses_rss_items() {
        let feed = parse(RSS_SAMPLE);
        assert_eq!(feed.items.len(), 2);
        assert_eq!(feed.items[0].title, "First Post");
        assert_eq!(feed.items[0].url, "https://example.com/posts/first");
        assert_eq!(feed.items[1].url, "https://example.com/posts/second");
    }

    #[test]
    fn parses_atom_entries_preferring_alternate_link() {
        let feed = parse(ATOM_SAMPLE);
        assert_eq!(feed.items.len(), 2);
        assert_eq!(feed.items[0].title, "Atom Entry One");
        // Must pick the rel="alternate" link, not the rel="self" one.
        assert_eq!(feed.items[0].url, "https://example.com/posts/atom-one");
        // No rel attribute defaults to "alternate" per the Atom spec.
        assert_eq!(feed.items[1].url, "https://example.com/posts/atom-two");
    }

    #[test]
    fn malformed_feed_yields_empty_without_panicking() {
        let feed = parse("<rss><channel><item><title>broken");
        assert!(feed.items.is_empty());
    }

    #[test]
    fn feed_root_title_is_not_treated_as_an_item() {
        let feed = parse(RSS_SAMPLE);
        assert!(!feed.items.iter().any(|i| i.title == "Example Blog"));
    }
}
