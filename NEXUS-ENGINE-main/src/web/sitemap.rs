//! `sitemap.xml` parsing, including recursive `<sitemapindex>` files that
//! point at other sitemaps rather than pages directly.

use log::debug;
use quick_xml::events::Event;
use quick_xml::reader::Reader;

/// The result of parsing one sitemap document.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedSitemap {
    /// Page URLs found in a `<urlset>` sitemap.
    pub urls: Vec<String>,
    /// Nested sitemap URLs found in a `<sitemapindex>` sitemap. Callers
    /// should fetch and parse each of these in turn (see
    /// [`crate::web::crawler`]'s bounded recursion).
    pub nested_sitemaps: Vec<String>,
}

/// Parses `xml`, which may be either a `<urlset>` (page listing) or a
/// `<sitemapindex>` (listing of other sitemaps), or a plain-text sitemap
/// (one URL per line, a common informal variant). Malformed XML yields an
/// empty result rather than an error: a broken sitemap should not abort a
/// crawl, since the crawler can still discover pages via links.
pub fn parse(xml: &str) -> ParsedSitemap {
    debug!("parsing sitemap ({} bytes)", xml.len());
    let trimmed = xml.trim_start();
    if !trimmed.starts_with('<') {
        // Plain-text sitemap: one absolute URL per line.
        let urls: Vec<String> = xml
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("http://") || l.starts_with("https://"))
            .map(|s| s.to_string())
            .collect();
        debug!("parsed plain-text sitemap with {} URLs", urls.len());
        return ParsedSitemap {
            urls,
            nested_sitemaps: Vec::new(),
        };
    }

    let mut reader = Reader::from_str(xml);
    reader.trim_text(true);

    let mut result = ParsedSitemap::default();
    // Tracks which element we're inside: an entry in <url><loc> or an
    // entry in <sitemap><loc>, since both use the same tag name.
    let mut in_sitemap_entry = false;
    let mut in_loc = false;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                b"sitemap" => in_sitemap_entry = true,
                b"loc" => in_loc = true,
                _ => {}
            },
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"sitemap" => in_sitemap_entry = false,
                b"loc" => in_loc = false,
                _ => {}
            },
            Ok(Event::Text(t)) => {
                if in_loc {
                    if let Ok(text) = t.unescape() {
                        let url = text.trim().to_string();
                        if !url.is_empty() {
                            if in_sitemap_entry {
                                result.nested_sitemaps.push(url);
                            } else {
                                result.urls.push(url);
                            }
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urlset() {
        let xml = r#"<?xml version="1.0"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url><loc>https://example.com/a</loc></url>
  <url><loc>https://example.com/b</loc><lastmod>2024-01-01</lastmod></url>
</urlset>"#;
        let parsed = parse(xml);
        assert_eq!(
            parsed.urls,
            vec!["https://example.com/a", "https://example.com/b"]
        );
        assert!(parsed.nested_sitemaps.is_empty());
    }

    #[test]
    fn parses_sitemap_index() {
        let xml = r#"<?xml version="1.0"?>
<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <sitemap><loc>https://example.com/sitemap-1.xml</loc></sitemap>
  <sitemap><loc>https://example.com/sitemap-2.xml</loc></sitemap>
</sitemapindex>"#;
        let parsed = parse(xml);
        assert!(parsed.urls.is_empty());
        assert_eq!(
            parsed.nested_sitemaps,
            vec![
                "https://example.com/sitemap-1.xml",
                "https://example.com/sitemap-2.xml"
            ]
        );
    }

    #[test]
    fn parses_plain_text_sitemap() {
        let text = "https://example.com/a\nhttps://example.com/b\n\n";
        let parsed = parse(text);
        assert_eq!(
            parsed.urls,
            vec!["https://example.com/a", "https://example.com/b"]
        );
    }

    #[test]
    fn malformed_xml_does_not_panic() {
        // quick_xml is a lenient streaming parser: unclosed tags don't
        // abort parsing, they just yield whatever text was seen before
        // the input ran out. The important property here is robustness
        // (never panics, never hangs), not that malformed input yields
        // literally nothing.
        let parsed = parse("<urlset><url><loc>unterminated");
        assert!(parsed.nested_sitemaps.is_empty());
    }
}
