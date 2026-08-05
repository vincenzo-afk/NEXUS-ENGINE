//! Local extractors for formats the README previously listed as missing:
//! Office documents (.docx/.xlsx/.pptx), email (.eml/.mbox), SQLite-backed
//! note stores, browser history databases, images (EXIF + optional OCR),
//! and generic archives (.zip).
//!
//! Same philosophy as [`crate::formats`]: every extractor here is
//! best-effort and infallible where reasonable. A single corrupt
//! attachment, a locked browser-history file, or a password-protected
//! zip should degrade to "no text extracted" for that one item, never
//! abort a bulk index run.
//!
//! ## What's genuinely real here vs. what depends on the runtime
//! - Office/email/archive/generic-SQLite extraction are pure Rust (`zip`,
//!   `roxmltree`, `mail-parser`, `rusqlite` with the `bundled` SQLite
//!   amalgamation) and work with no external tools installed.
//! - Browser-history reading depends on the *schema* of the browser's
//!   `History`/`places.sqlite` file, which does change across browser
//!   versions; the queries here target the long-stable Chromium
//!   (`urls`/`visits` tables) and Firefox (`moz_places`/`moz_historyvisits`)
//!   schemas as of this writing, but are not guaranteed to track every
//!   future schema migration.
//! - OCR (`image_ocr::ocr_image`) shells out to a system `tesseract`
//!   binary if present on `PATH`. This is a real integration, not a stub
//!   — but it does nothing useful on a machine without Tesseract
//!   installed. EXIF metadata extraction (`image_ocr::extract_exif_text`)
//!   has no such dependency and always works on images that carry EXIF.

pub mod archive;
pub mod browser_history;
pub mod email;
pub mod image_ocr;
pub mod office;
pub mod sqlite_notes;

/// Result of extracting indexable text (and, where applicable, structured
/// sub-items) from a non-plain-text local source. Kept deliberately
/// simple/flat rather than a large enum-per-format tree, since every
/// caller just wants "here is the text, and optionally here is why it's
/// empty" — see [`document::Document::from_crawled_file`](crate::document::Document::from_crawled_file)
/// for how [`crate::formats::DocumentFormat`] dispatches into these.
#[derive(Debug, Clone, Default)]
pub struct ExtractedText {
    /// Concatenated indexable text.
    pub text: String,
    /// Human-readable note about degraded extraction (e.g. "password
    /// protected", "OCR unavailable: tesseract not found on PATH"), shown
    /// in verbose/debug CLI output rather than silently swallowed.
    pub warning: Option<String>,
}

impl ExtractedText {
    fn ok(text: String) -> Self {
        ExtractedText {
            text,
            warning: None,
        }
    }

    fn empty_with_warning(warning: impl Into<String>) -> Self {
        ExtractedText {
            text: String::new(),
            warning: Some(warning.into()),
        }
    }
}
