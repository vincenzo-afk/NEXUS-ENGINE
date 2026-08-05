//! Extraction of indexable plain text from document formats beyond plain
//! text and HTML: Markdown, JSON, XML, and PDF. Each function is
//! best-effort and infallible where reasonable (malformed input degrades
//! to a best-guess rather than aborting indexing of the whole document),
//! since a single malformed file should never take down a crawl or a
//! bulk-index run.

use log::debug;
use pulldown_cmark::{Event, Parser as MdParser, Tag};

/// Which extractor to use for a document, inferred from a file extension
/// or an HTTP `Content-Type` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentFormat {
    /// Plain text or unrecognized (indexed byte-for-byte).
    PlainText,
    /// HTML, handled by [`crate::html`] rather than this module.
    Html,
    /// CommonMark/GFM-flavored Markdown.
    Markdown,
    /// JSON: string values are concatenated as indexable text.
    Json,
    /// Generic XML: element text content is concatenated.
    Xml,
    /// PDF: extracted via `pdf-extract`.
    Pdf,
    /// Word document (`.docx`), via `crate::extract::office`.
    Docx,
    /// Excel workbook (`.xlsx`), via `crate::extract::office`.
    Xlsx,
    /// PowerPoint deck (`.pptx`), via `crate::extract::office`.
    Pptx,
    /// Single email message (`.eml`), via `crate::extract::email`.
    Eml,
    /// Multi-message mailbox (`.mbox`), via `crate::extract::email`.
    Mbox,
    /// Generic zip archive, via `crate::extract::archive`.
    Zip,
    /// SQLite database file of unknown/app-specific schema (note stores),
    /// via `crate::extract::sqlite_notes`.
    SqliteDb,
    /// Raster image: EXIF + optional OCR, via `crate::extract::image_ocr`.
    Image,
}

impl DocumentFormat {
    /// Infers a format from a lowercase file extension (no leading dot).
    pub fn from_extension(ext: &str) -> DocumentFormat {
        match ext {
            "html" | "htm" => DocumentFormat::Html,
            "md" | "markdown" => DocumentFormat::Markdown,
            "json" => DocumentFormat::Json,
            "xml" => DocumentFormat::Xml,
            "pdf" => DocumentFormat::Pdf,
            "docx" => DocumentFormat::Docx,
            "xlsx" => DocumentFormat::Xlsx,
            "pptx" => DocumentFormat::Pptx,
            "eml" => DocumentFormat::Eml,
            "mbox" => DocumentFormat::Mbox,
            "zip" => DocumentFormat::Zip,
            "sqlite" | "sqlite3" | "db" => DocumentFormat::SqliteDb,
            "jpg" | "jpeg" | "png" | "tiff" | "tif" | "webp" | "heic" => DocumentFormat::Image,
            _ => DocumentFormat::PlainText,
        }
    }

    /// Infers a format from an HTTP `Content-Type` header value (may
    /// include a `; charset=...` suffix, which is ignored).
    pub fn from_content_type(content_type: &str) -> DocumentFormat {
        let base = content_type
            .split(';')
            .next()
            .unwrap_or(content_type)
            .trim();
        match base {
            "text/html" | "application/xhtml+xml" => DocumentFormat::Html,
            "text/markdown" | "text/x-markdown" => DocumentFormat::Markdown,
            "application/json" | "text/json" => DocumentFormat::Json,
            "application/xml" | "text/xml" | "application/rss+xml" | "application/atom+xml" => {
                DocumentFormat::Xml
            }
            "application/pdf" => DocumentFormat::Pdf,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
                DocumentFormat::Docx
            }
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => {
                DocumentFormat::Xlsx
            }
            "application/vnd.openxmlformats-officedocument.presentationml.presentation" => {
                DocumentFormat::Pptx
            }
            "message/rfc822" => DocumentFormat::Eml,
            "application/mbox" => DocumentFormat::Mbox,
            "application/zip" | "application/x-zip-compressed" => DocumentFormat::Zip,
            "image/jpeg" | "image/png" | "image/tiff" | "image/webp" | "image/heic" => {
                DocumentFormat::Image
            }
            _ => DocumentFormat::PlainText,
        }
    }

    /// Short label used for `WebPageMeta::content_type` and CLI display.
    pub fn label(&self) -> &'static str {
        match self {
            DocumentFormat::PlainText => "text",
            DocumentFormat::Html => "html",
            DocumentFormat::Markdown => "markdown",
            DocumentFormat::Json => "json",
            DocumentFormat::Xml => "xml",
            DocumentFormat::Pdf => "pdf",
            DocumentFormat::Docx => "docx",
            DocumentFormat::Xlsx => "xlsx",
            DocumentFormat::Pptx => "pptx",
            DocumentFormat::Eml => "eml",
            DocumentFormat::Mbox => "mbox",
            DocumentFormat::Zip => "zip",
            DocumentFormat::SqliteDb => "sqlite",
            DocumentFormat::Image => "image",
        }
    }
}

/// Strips Markdown syntax down to its plain reading text: headings,
/// paragraphs, list items, and emphasis all become plain words; code
/// blocks are kept (code is often exactly what's being searched for) but
/// fence markers are dropped; links keep their visible text.
pub fn extract_markdown(source: &str) -> String {
    debug!("extracting markdown ({} bytes)", source.len());
    let parser = MdParser::new(source);
    let mut out = String::with_capacity(source.len());
    for event in parser {
        match event {
            Event::Text(t) | Event::Code(t) => {
                out.push_str(&t);
                out.push(' ');
            }
            Event::SoftBreak | Event::HardBreak => out.push('\n'),
            Event::Start(Tag::Item) => out.push_str("- "),
            Event::End(Tag::Paragraph) | Event::End(Tag::Heading(..)) => out.push('\n'),
            _ => {}
        }
    }
    out
}

/// Recursively concatenates every string value (and object key) in a JSON
/// document into indexable text. Numbers and booleans are skipped (they
/// carry little full-text search value and would pollute the vocabulary
/// with noise like "true" and "42"), but keys are included since they're
/// often meaningful field names (`"error_message": "..."`).
pub fn extract_json(source: &str) -> String {
    debug!("extracting JSON ({} bytes)", source.len());
    match serde_json::from_str::<serde_json::Value>(source) {
        Ok(value) => {
            let mut out = String::new();
            collect_json_text(&value, &mut out);
            out
        }
        Err(_) => {
            // Not valid JSON (or truncated); fall back to indexing the
            // raw text so at least literal substrings remain searchable.
            source.to_string()
        }
    }
}

fn collect_json_text(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::String(s) => {
            out.push_str(s);
            out.push(' ');
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_json_text(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, val) in map {
                out.push_str(key);
                out.push(' ');
                collect_json_text(val, out);
            }
        }
        _ => {}
    }
}

/// Concatenates every text node in a generic XML document (RSS/Atom feeds,
/// data exports, etc). Element and attribute names are ignored; only
/// character data between tags is kept.
pub fn extract_xml(source: &str) -> String {
    use quick_xml::events::Event as XmlEvent;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(source);
    reader.trim_text(true);
    let mut out = String::with_capacity(source.len() / 2);
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(XmlEvent::Text(t)) => {
                if let Ok(text) = t.unescape() {
                    out.push_str(&text);
                    out.push(' ');
                }
            }
            Ok(XmlEvent::CData(t)) => {
                if let Ok(raw) = std::str::from_utf8(&t.into_inner()) {
                    out.push_str(raw);
                    out.push(' ');
                }
            }
            Ok(XmlEvent::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// Extracts text from PDF bytes. PDF parsing has a long tail of malformed
/// real-world files, so failures are swallowed into an empty string
/// (callers should treat an empty result as "could not extract, index
/// with an empty body / rely on metadata only") rather than aborting.
pub fn extract_pdf(bytes: &[u8]) -> String {
    pdf_extract::extract_text_from_mem(bytes).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_inferred_from_extension() {
        assert_eq!(
            DocumentFormat::from_extension("md"),
            DocumentFormat::Markdown
        );
        assert_eq!(DocumentFormat::from_extension("pdf"), DocumentFormat::Pdf);
        assert_eq!(
            DocumentFormat::from_extension("xyz"),
            DocumentFormat::PlainText
        );
    }

    #[test]
    fn format_inferred_from_content_type() {
        assert_eq!(
            DocumentFormat::from_content_type("application/json; charset=utf-8"),
            DocumentFormat::Json
        );
        assert_eq!(
            DocumentFormat::from_content_type("text/html"),
            DocumentFormat::Html
        );
    }

    #[test]
    fn markdown_strips_syntax() {
        let text = extract_markdown("# Title\n\nSome **bold** text and a [link](https://x.com).");
        assert!(text.contains("Title"));
        assert!(text.contains("bold"));
        assert!(text.contains("link"));
        assert!(!text.contains('#'));
        assert!(!text.contains("**"));
    }

    #[test]
    fn json_collects_strings_and_keys() {
        let text = extract_json(r#"{"title": "Rust Guide", "tags": ["systems", "safety"]}"#);
        assert!(text.contains("title"));
        assert!(text.contains("Rust Guide"));
        assert!(text.contains("systems"));
        assert!(text.contains("safety"));
    }

    #[test]
    fn invalid_json_falls_back_to_raw() {
        let text = extract_json("not actually json {{{");
        assert_eq!(text, "not actually json {{{");
    }

    #[test]
    fn xml_collects_text_nodes() {
        let text = extract_xml("<rss><item><title>Breaking News</title><description>Details here</description></item></rss>");
        assert!(text.contains("Breaking News"));
        assert!(text.contains("Details here"));
    }
}
