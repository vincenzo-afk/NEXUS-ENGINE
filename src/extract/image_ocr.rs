//! Image indexing: EXIF metadata (always available, pure Rust) plus
//! optional OCR text (shells out to a system `tesseract` binary if one is
//! on `PATH`).
//!
//! **Be honest about what this is.** There is no bundled, in-process OCR
//! engine here — a real OCR pipeline (image preprocessing, layout
//! analysis, a trained text-recognition model) is, as the README already
//! says of headless-browser rendering, its own multi-week undertaking,
//! not something to fake with a stub that returns empty strings while
//! claiming to work. What *is* real: if `tesseract` is installed on the
//! machine running Nexus, [`ocr_image`] genuinely invokes it and returns
//! its actual output. If it isn't installed, callers get a clear
//! `warning` explaining exactly that, not a silent empty result that
//! looks like "this image had no text."
use crate::extract::ExtractedText;
use std::process::Command;

/// Extracts human-readable text from an image's EXIF tags: camera make/
/// model, and any `ImageDescription`/`XPTitle`/`XPComment`/GPS-derived
/// fields the file happens to carry. Most photos have little or none of
/// this beyond camera make/model; screenshots and scanned documents
/// often have none at all, which is exactly why OCR (below) is the part
/// that matters for "search my scanned receipts."
pub fn extract_exif_text(path: &std::path::Path) -> ExtractedText {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return ExtractedText::empty_with_warning(format!("cannot open: {e}")),
    };
    let mut bufreader = std::io::BufReader::new(&file);
    let exif = match exif::Reader::new().read_from_container(&mut bufreader) {
        Ok(e) => e,
        Err(_) => return ExtractedText::empty_with_warning("no EXIF data found"),
    };

    let mut parts = Vec::new();
    for field in exif.fields() {
        use exif::Tag;
        if matches!(
            field.tag,
            Tag::ImageDescription
                | Tag::Make
                | Tag::Model
                | Tag::Software
                | Tag::Copyright
                | Tag::UserComment
        ) {
            let value = field.display_value().with_unit(&exif).to_string();
            if !value.trim().is_empty() && value != "0" {
                parts.push(value);
            }
        }
    }

    if parts.is_empty() {
        ExtractedText::empty_with_warning("EXIF present but no text-bearing fields")
    } else {
        ExtractedText::ok(parts.join(" "))
    }
}

/// Runs `tesseract <path> stdout` and returns its recognized text.
/// Requires Tesseract OCR to be installed and on `PATH`; this is checked
/// at call time (not assumed), and a missing binary produces a clear
/// warning rather than a misleading empty success.
pub fn ocr_image(path: &std::path::Path) -> ExtractedText {
    if which_tesseract().is_none() {
        return ExtractedText::empty_with_warning(
            "OCR unavailable: `tesseract` binary not found on PATH (install Tesseract OCR to enable image text search)",
        );
    }

    let output = Command::new("tesseract")
        .arg(path)
        .arg("stdout")
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            if text.trim().is_empty() {
                ExtractedText::empty_with_warning("tesseract ran but recognized no text")
            } else {
                ExtractedText::ok(text)
            }
        }
        Ok(out) => ExtractedText::empty_with_warning(format!(
            "tesseract exited with an error: {}",
            String::from_utf8_lossy(&out.stderr)
        )),
        Err(e) => ExtractedText::empty_with_warning(format!("failed to run tesseract: {e}")),
    }
}

/// Combines EXIF metadata and OCR text (when available) into one
/// indexable blob for a single image file.
pub fn extract_image_text(path: &std::path::Path) -> ExtractedText {
    let exif = extract_exif_text(path);
    let ocr = ocr_image(path);
    let combined = format!("{} {}", exif.text, ocr.text).trim().to_string();
    let warning = match (exif.warning, ocr.warning) {
        (Some(a), Some(b)) => Some(format!("{a}; {b}")),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    ExtractedText {
        text: combined,
        warning,
    }
}

fn which_tesseract() -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var).find_map(|dir| {
        let candidate = dir.join(if cfg!(windows) {
            "tesseract.exe"
        } else {
            "tesseract"
        });
        candidate.is_file().then_some(candidate)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_degrades_to_warning() {
        let result = extract_exif_text(std::path::Path::new("/no/such/file.jpg"));
        assert!(result.text.is_empty());
        assert!(result.warning.is_some());
    }

    #[test]
    fn ocr_reports_clearly_when_tesseract_absent_or_present() {
        // We don't assert on which branch fires (depends on the machine
        // running the test), only that we never silently produce an
        // empty success — either real OCR text or an explicit warning.
        let dir = std::env::temp_dir().join(format!(
            "nexus-ocr-test-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("not_really_an_image.jpg");
        std::fs::write(&path, b"not actually image bytes").unwrap();
        let result = ocr_image(&path);
        assert!(!result.text.is_empty() || result.warning.is_some());
        std::fs::remove_dir_all(&dir).ok();
    }
}
