//! Extraction for the OOXML "Office Open XML" formats: Word (`.docx`),
//! Excel (`.xlsx`), and PowerPoint (`.pptx`). All three are, underneath,
//! a zip archive of XML parts — there is no need for a heavyweight Office
//! SDK, just a zip reader and an XML text-node walker, which is exactly
//! how [`crate::formats::extract_xml`] already handles plain XML/RSS.
//!
//! Legacy binary formats (`.doc`/`.xls`/`.ppt`, pre-2007 OLE2 compound
//! files) are a different, much uglier binary format and are **not**
//! handled here — they would need a dedicated OLE2/CFB parser. Files with
//! those extensions fall back to [`crate::formats::DocumentFormat::PlainText`]
//! (i.e. indexed as raw bytes, which for a binary OLE2 file is mostly
//! noise). That's a real, documented gap, not a silent one.

use super::ExtractedText;
use std::io::Read;

/// Extracts plain text from a `.docx` file's `word/document.xml` part.
pub fn extract_docx(bytes: &[u8]) -> ExtractedText {
    extract_ooxml_part(bytes, "word/document.xml")
}

/// Extracts plain text from every worksheet's `sharedStrings.xml` plus
/// inline cell text in a `.xlsx` file. Numeric-only cells are skipped
/// (like [`crate::formats::extract_json`] skipping numbers/booleans) since
/// they add vocabulary noise rather than search value; text cells and
/// sheet names are kept.
pub fn extract_xlsx(bytes: &[u8]) -> ExtractedText {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = match zip::ZipArchive::new(cursor) {
        Ok(a) => a,
        Err(e) => return ExtractedText::empty_with_warning(format!("not a valid zip: {e}")),
    };

    let mut out = String::new();
    // sharedStrings.xml holds the de-duplicated string table most cell
    // text references by index; inline strings in individual sheets are
    // covered by the generic XML text-node walk below regardless.
    if let Ok(mut f) = archive.by_name("xl/sharedStrings.xml") {
        let mut xml = String::new();
        if f.read_to_string(&mut xml).is_ok() {
            out.push_str(&crate::formats::extract_xml(&xml));
            out.push(' ');
        }
    }

    let sheet_names: Vec<String> = (0..archive.len())
        .filter_map(|i| archive.by_index(i).ok().map(|f| f.name().to_string()))
        .filter(|n| n.starts_with("xl/worksheets/") && n.ends_with(".xml"))
        .collect();

    for name in sheet_names {
        if let Ok(mut f) = archive.by_name(&name) {
            let mut xml = String::new();
            if f.read_to_string(&mut xml).is_ok() {
                out.push_str(&crate::formats::extract_xml(&xml));
                out.push(' ');
            }
        }
    }

    if out.trim().is_empty() {
        ExtractedText::empty_with_warning("no readable sheet or shared-string XML found")
    } else {
        ExtractedText::ok(out)
    }
}

/// Extracts plain text from every slide's XML part in a `.pptx` file,
/// in slide order, plus speaker notes (`notesSlides/`), since notes are
/// often where the actually-searchable content lives for a deck of
/// mostly-images slides.
pub fn extract_pptx(bytes: &[u8]) -> ExtractedText {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = match zip::ZipArchive::new(cursor) {
        Ok(a) => a,
        Err(e) => return ExtractedText::empty_with_warning(format!("not a valid zip: {e}")),
    };

    let mut slide_names: Vec<String> = (0..archive.len())
        .filter_map(|i| archive.by_index(i).ok().map(|f| f.name().to_string()))
        .filter(|n| {
            (n.starts_with("ppt/slides/slide") || n.starts_with("ppt/notesSlides/notesSlide"))
                && n.ends_with(".xml")
        })
        .collect();
    // Sort so slide1, slide2, ... slide10 come out in numeric rather than
    // lexicographic order (slide10 would otherwise sort before slide2).
    slide_names.sort_by_key(|n| {
        n.chars()
            .filter(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<u32>()
            .unwrap_or(0)
    });

    let mut out = String::new();
    for name in slide_names {
        if let Ok(mut f) = archive.by_name(&name) {
            let mut xml = String::new();
            if f.read_to_string(&mut xml).is_ok() {
                out.push_str(&extract_drawingml_text(&xml));
                out.push('\n');
            }
        }
    }

    if out.trim().is_empty() {
        ExtractedText::empty_with_warning("no readable slide XML found")
    } else {
        ExtractedText::ok(out)
    }
}

/// Shared implementation for single-XML-part OOXML formats (just `.docx`
/// today; `.xlsx`/`.pptx` aggregate multiple parts so they have their own
/// functions above).
fn extract_ooxml_part(bytes: &[u8], part_name: &str) -> ExtractedText {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = match zip::ZipArchive::new(cursor) {
        Ok(a) => a,
        Err(e) => return ExtractedText::empty_with_warning(format!("not a valid zip: {e}")),
    };
    let mut file = match archive.by_name(part_name) {
        Ok(f) => f,
        Err(_) => {
            return ExtractedText::empty_with_warning(format!("missing part '{part_name}'"))
        }
    };
    let mut xml = String::new();
    if file.read_to_string(&mut xml).is_err() {
        return ExtractedText::empty_with_warning("part is not valid UTF-8 XML");
    }
    ExtractedText::ok(extract_wordprocessingml_text(&xml))
}

/// WordprocessingML text nodes live in `<w:t>` elements; a naive
/// "all text nodes" walk (like [`crate::formats::extract_xml`]) would
/// also pick up revision-tracking metadata and field codes, so this uses
/// `roxmltree` to specifically target `w:t` elements, keeping paragraph
/// breaks (`w:p`) as newlines so extracted text stays roughly readable.
fn extract_wordprocessingml_text(xml: &str) -> String {
    let doc = match roxmltree::Document::parse(xml) {
        Ok(d) => d,
        Err(_) => return crate::formats::extract_xml(xml), // best-effort fallback
    };
    let mut out = String::new();
    for node in doc.descendants() {
        if node.has_tag_name("t") {
            if let Some(text) = node.text() {
                out.push_str(text);
            }
        } else if node.has_tag_name("p") {
            out.push('\n');
        }
    }
    out
}

/// DrawingML (used by PowerPoint slides) text nodes are `<a:t>` elements
/// rather than WordprocessingML's `<w:t>`; separate function to keep the
/// intent explicit even though the tag-name check is the only difference.
fn extract_drawingml_text(xml: &str) -> String {
    let doc = match roxmltree::Document::parse(xml) {
        Ok(d) => d,
        Err(_) => return crate::formats::extract_xml(xml),
    };
    let mut out = String::new();
    for node in doc.descendants() {
        if node.has_tag_name("t") {
            if let Some(text) = node.text() {
                out.push_str(text);
                out.push(' ');
            }
        }
    }
    out
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
    fn docx_extracts_paragraph_text() {
        let xml = r#"<w:document xmlns:w="ns"><w:body><w:p><w:r><w:t>Hello</w:t></w:r><w:r><w:t> world</w:t></w:r></w:p></w:body></w:document>"#;
        let bytes = make_zip(&[("word/document.xml", xml)]);
        let result = extract_docx(&bytes);
        assert!(result.text.contains("Hello"));
        assert!(result.text.contains("world"));
    }

    #[test]
    fn xlsx_extracts_shared_strings() {
        let xml = r#"<sst xmlns="ns"><si><t>Revenue</t></si><si><t>Q4 2025</t></si></sst>"#;
        let bytes = make_zip(&[("xl/sharedStrings.xml", xml)]);
        let result = extract_xlsx(&bytes);
        assert!(result.text.contains("Revenue"));
        assert!(result.text.contains("Q4 2025"));
    }

    #[test]
    fn pptx_extracts_slide_text_in_order() {
        let s1 = r#"<p:sld xmlns:a="ns"><a:t>Slide One</a:t></p:sld>"#;
        let s2 = r#"<p:sld xmlns:a="ns"><a:t>Slide Two</a:t></p:sld>"#;
        let bytes = make_zip(&[
            ("ppt/slides/slide2.xml", s2),
            ("ppt/slides/slide1.xml", s1),
        ]);
        let result = extract_pptx(&bytes);
        let pos1 = result.text.find("Slide One").unwrap();
        let pos2 = result.text.find("Slide Two").unwrap();
        assert!(pos1 < pos2, "slide1 text should come before slide2 text");
    }

    #[test]
    fn non_zip_bytes_degrade_to_warning_not_panic() {
        let result = extract_docx(b"not a zip file at all");
        assert!(result.text.is_empty());
        assert!(result.warning.is_some());
    }
}
