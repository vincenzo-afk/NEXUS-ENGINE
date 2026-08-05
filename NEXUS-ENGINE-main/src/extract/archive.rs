//! Generic `.zip` archive extraction: index the archive as one document
//! whose text is every entry's *name* (so "find the zip that has
//! `invoice_2026.pdf` in it" works) plus, for a bounded number of small
//! text-like inner files, their actual extracted content via the same
//! [`crate::formats`] dispatch used for standalone files.
//!
//! Deliberately **not** unbounded recursion into every nested archive and
//! every megabyte of every inner file: a hostile or just very large zip
//! (a "zip bomb," or simply a 4GB archive of video files) must not be
//! able to make indexing one file take unbounded time or memory. The
//! limits below are conservative and configurable by editing the
//! constants, not because the numbers are precisely tuned, but because a
//! hard cap needs to exist at all.

use crate::extract::ExtractedText;
use std::io::Read;

/// Only inner files at or under this size are extracted for content, not
/// just listed by name.
const MAX_INNER_FILE_BYTES: u64 = 5 * 1024 * 1024; // 5 MiB
/// Only the first N inner files (in zip central-directory order) get
/// content-extracted; the rest are still listed by name.
const MAX_INNER_FILES_EXTRACTED: usize = 200;

/// Extracts an indexable summary of a `.zip` archive: all entry paths,
/// plus best-effort extracted text of small, recognized inner files.
pub fn extract_zip(bytes: &[u8]) -> ExtractedText {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = match zip::ZipArchive::new(cursor) {
        Ok(a) => a,
        Err(e) => return ExtractedText::empty_with_warning(format!("not a valid zip: {e}")),
    };

    let mut out = String::new();
    let mut extracted_count = 0usize;
    let mut skipped_too_large = 0usize;

    for i in 0..archive.len() {
        let mut entry = match archive.by_index(i) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_string();
        out.push_str(&name);
        out.push('\n');

        if extracted_count >= MAX_INNER_FILES_EXTRACTED {
            continue;
        }
        if entry.size() > MAX_INNER_FILE_BYTES {
            skipped_too_large += 1;
            continue;
        }

        let extension = std::path::Path::new(&name)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();
        let format = crate::formats::DocumentFormat::from_extension(&extension);
        // Only bother reading+extracting formats we actually know how to
        // turn into text; raw binary/media entries just stay listed by
        // name from above (there is no useful "content" to index).
        if matches!(format, crate::formats::DocumentFormat::PlainText)
            && !matches!(extension.as_str(), "txt" | "csv" | "log")
        {
            continue;
        }

        let mut buf = Vec::with_capacity(entry.size() as usize);
        if entry.read_to_end(&mut buf).is_err() {
            continue;
        }
        let text = match format {
            crate::formats::DocumentFormat::Markdown => {
                crate::formats::extract_markdown(&String::from_utf8_lossy(&buf))
            }
            crate::formats::DocumentFormat::Json => {
                crate::formats::extract_json(&String::from_utf8_lossy(&buf))
            }
            crate::formats::DocumentFormat::Xml => {
                crate::formats::extract_xml(&String::from_utf8_lossy(&buf))
            }
            crate::formats::DocumentFormat::Html => {
                crate::html::extract(&String::from_utf8_lossy(&buf)).indexable_text()
            }
            crate::formats::DocumentFormat::Pdf => crate::formats::extract_pdf(&buf),
            _ => String::from_utf8_lossy(&buf).into_owned(),
        };
        out.push_str(&text);
        out.push('\n');
        extracted_count += 1;
    }

    if extracted_count == 0 && skipped_too_large > 0 {
        out.push_str(&format!(
            "\n[{skipped_too_large} inner files skipped: over the {} MiB per-entry limit]",
            MAX_INNER_FILE_BYTES / (1024 * 1024)
        ));
    }

    if out.trim().is_empty() {
        ExtractedText::empty_with_warning("empty archive")
    } else {
        ExtractedText::ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_zip(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut buf);
            let mut writer = zip::ZipWriter::new(cursor);
            let options = zip::write::FileOptions::default();
            for (name, content) in entries {
                writer.start_file(*name, options).unwrap();
                std::io::Write::write_all(&mut writer, content.as_bytes()).unwrap();
            }
            writer.finish().unwrap();
        }
        buf
    }

    #[test]
    fn lists_entry_names_and_extracts_text_content() {
        let bytes = make_zip(&[
            ("readme.txt", "hello from inside the zip"),
            ("photo.jpg", "not really jpg bytes"),
        ]);
        let result = extract_zip(&bytes);
        assert!(result.text.contains("readme.txt"));
        assert!(result.text.contains("photo.jpg"));
        assert!(result.text.contains("hello from inside the zip"));
    }

    #[test]
    fn non_zip_bytes_degrade_to_warning() {
        let result = extract_zip(b"just some plain bytes, not a zip");
        assert!(result.warning.is_some());
    }
}
