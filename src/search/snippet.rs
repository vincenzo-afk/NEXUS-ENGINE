//! Result snippets: a short excerpt of a matched document with the query
//! terms highlighted, generated on demand at search time.
//!
//! Nexus does not persist full document text in the index (only postings
//! and metadata), so snippet generation re-reads the file's content for
//! the handful of top-ranked results being displayed. This keeps the
//! index small while still providing rich, contextual previews.

use crate::error::{NexusError, Result};
use crate::text;
use std::collections::HashSet;
use std::path::Path;

/// Number of characters of context to include on each side of a match.
const CONTEXT_CHARS: usize = 60;

/// A highlighted snippet plus the character offsets (within the snippet)
/// of every highlighted match, so a UI can render highlighting without
/// re-parsing marker syntax.
#[derive(Debug, Clone)]
pub struct Snippet {
    /// The excerpt text, with matches wrapped in `**...**` markers.
    pub text: String,
    /// Total number of times any query term occurs in the full document.
    pub total_matches: usize,
}

/// Generates a highlighted snippet for `path`, centered on the first
/// occurrence of any term in `terms`.
pub fn generate(path: &Path, terms: &HashSet<String>) -> Result<Snippet> {
    let raw = std::fs::read(path).map_err(|e| NexusError::io(path, e))?;
    let content = String::from_utf8_lossy(&raw).into_owned();
    let normalized = text::normalize(&content);
    let tokens = text::tokenize(&normalized);

    let matches: Vec<&text::Token> = tokens.iter().filter(|t| terms.contains(&t.text)).collect();

    if matches.is_empty() {
        let preview: String = normalized.chars().take(CONTEXT_CHARS * 2).collect();
        return Ok(Snippet {
            text: preview,
            total_matches: 0,
        });
    }

    let anchor = matches[0];
    let start = (anchor.start_offset as usize).saturating_sub(CONTEXT_CHARS);
    let end = (anchor.end_offset as usize + CONTEXT_CHARS).min(normalized.len());

    // Clamp to char boundaries since we're slicing a UTF-8 string by byte offset.
    let start = clamp_to_char_boundary(&normalized, start, false);
    let end = clamp_to_char_boundary(&normalized, end, true);

    let window = &normalized[start..end];
    let highlighted = highlight_window(window, start, &matches, terms);

    Ok(Snippet {
        text: highlighted,
        total_matches: matches.len(),
    })
}

/// Moves `offset` to the nearest valid UTF-8 char boundary, searching
/// forward if `forward` is true, otherwise backward.
fn clamp_to_char_boundary(s: &str, mut offset: usize, forward: bool) -> usize {
    offset = offset.min(s.len());
    while offset > 0 && offset < s.len() && !s.is_char_boundary(offset) {
        if forward {
            offset += 1;
        } else {
            offset -= 1;
        }
    }
    offset
}

/// Wraps each matching token within `window` in `**...**` markers. `window`
/// starts at byte `window_start` within the original normalized string, so
/// token offsets are adjusted accordingly.
fn highlight_window(
    window: &str,
    window_start: usize,
    matches: &[&text::Token],
    terms: &HashSet<String>,
) -> String {
    let mut result = String::new();
    let mut cursor = 0usize;

    let mut relevant: Vec<&&text::Token> = matches
        .iter()
        .filter(|t| {
            (t.start_offset as usize) >= window_start
                && (t.end_offset as usize) <= window_start + window.len()
        })
        .collect();
    relevant.sort_by_key(|t| t.start_offset);

    for token in relevant {
        if !terms.contains(&token.text) {
            continue;
        }
        let local_start = token.start_offset as usize - window_start;
        let local_end = token.end_offset as usize - window_start;
        if local_start < cursor {
            continue; // overlapping token, skip
        }
        result.push_str(&window[cursor..local_start]);
        result.push_str("**");
        result.push_str(&window[local_start..local_end]);
        result.push_str("**");
        cursor = local_end;
    }
    result.push_str(&window[cursor..]);

    let mut prefix = String::new();
    if window_start > 0 {
        prefix.push_str("...");
    }
    let mut suffix = String::new();
    suffix.push_str("...");

    format!("{}{}{}", prefix, result, suffix)
}
