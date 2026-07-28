//! Result snippets: a short excerpt of a matched document with the query
//! terms highlighted, generated on demand at search time.
//!
//! Nexus does not persist full document text in the index (only postings
//! and metadata), so snippet generation re-reads the content for the
//! handful of top-ranked results being displayed: from disk for local
//! files, or from the [`crate::storage::content_cache::ContentCache`] for
//! crawled web pages. This keeps the index itself small while still
//! providing rich, contextual previews. Rather than simply centering on
//! the first occurrence of a query term, the generator scans every
//! candidate window and picks the one containing the most distinct query
//! terms, which in practice lands on the sentence that best represents why
//! the document matched.

use log::debug;

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

/// Generates a highlighted snippet for the file at `path`.
pub fn generate(path: &Path, terms: &HashSet<String>) -> Result<Snippet> {
    let raw = std::fs::read(path).map_err(|e| NexusError::io(path, e))?;
    let content = String::from_utf8_lossy(&raw).into_owned();
    Ok(generate_from_content(&content, terms))
}

/// Generates a highlighted snippet directly from already-loaded `content`
/// (used for web pages, whose text comes from the content cache rather
/// than a re-readable file path).
pub fn generate_from_content(content: &str, terms: &HashSet<String>) -> Snippet {
    let normalized = text::normalize(content);
    let tokens = text::tokenize(&normalized);

    let matches: Vec<&text::Token> = tokens.iter().filter(|t| terms.contains(&t.text)).collect();

    debug!(
        "generating snippet: {} query terms, {} matches in content",
        terms.len(),
        matches.len()
    );

    if matches.is_empty() {
        let preview: String = normalized.chars().take(CONTEXT_CHARS * 2).collect();
        return Snippet {
            text: preview,
            total_matches: 0,
        };
    }

    let anchor = best_anchor(&matches);
    let start = (anchor.start_offset as usize).saturating_sub(CONTEXT_CHARS);
    let end = (anchor.end_offset as usize + CONTEXT_CHARS).min(normalized.len());

    // Clamp to char boundaries since we're slicing a UTF-8 string by byte offset.
    let start = clamp_to_char_boundary(&normalized, start, false);
    let end = clamp_to_char_boundary(&normalized, end, true);

    let window = &normalized[start..end];
    let highlighted = highlight_window(window, start, &matches, terms);

    Snippet {
        text: highlighted,
        total_matches: matches.len(),
    }
}

/// Picks the match token whose `CONTEXT_CHARS`-radius window contains the
/// greatest number of *distinct* query terms (ties broken by earliest
/// position), so the returned snippet reads like the sentence most
/// representative of the match rather than just wherever the first term
/// happened to appear.
fn best_anchor<'a>(matches: &[&'a text::Token]) -> &'a text::Token {
    let mut best_idx = 0;
    let mut best_score = 0usize;

    for (i, candidate) in matches.iter().enumerate() {
        let window_start = candidate.start_offset.saturating_sub(CONTEXT_CHARS as u32);
        let window_end = candidate.end_offset + CONTEXT_CHARS as u32;

        let mut distinct: HashSet<&str> = HashSet::new();
        for other in matches {
            if other.start_offset >= window_start && other.end_offset <= window_end {
                distinct.insert(other.text.as_str());
            }
        }
        let score = distinct.len();

        if score > best_score {
            best_score = score;
            best_idx = i;
        }
    }

    matches[best_idx]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_window_with_most_distinct_terms() {
        let sparse_prefix = "In the beginning there was only rust, alone and quiet, \
            far from anything else nearby at all in this part of the document. ";
        let filler = "This filler text does not contain any of the target keywords at all, \
            it is only here to add distance between the two interesting spots. ";
        let dense_cluster = "Now consider rust and parser together: rust parser rust parser \
            combine here in a rich flurry of matches for testing purposes.";
        let content = format!("{sparse_prefix}{filler}{dense_cluster}");

        let terms: HashSet<String> = ["rust", "parser"].iter().map(|s| s.to_string()).collect();
        let snippet = generate_from_content(&content, &terms);
        // The dense cluster should be chosen over the sparse opening
        // sentence, which is far enough away (> CONTEXT_CHARS) that a
        // window anchored there cannot see the dense cluster at all.
        assert!(snippet.text.contains("combine"), "got: {}", snippet.text);
        assert!(!snippet.text.contains("beginning"), "got: {}", snippet.text);
    }

    #[test]
    fn highlights_matched_terms() {
        let content = "The rust programming language is fast and safe.";
        let terms: HashSet<String> = ["rust"].iter().map(|s| s.to_string()).collect();
        let snippet = generate_from_content(content, &terms);
        assert!(snippet.text.contains("**rust**"));
        assert_eq!(snippet.total_matches, 1);
    }

    #[test]
    fn empty_match_falls_back_to_preview() {
        let content = "Nothing relevant here at all.";
        let terms: HashSet<String> = ["rust"].iter().map(|s| s.to_string()).collect();
        let snippet = generate_from_content(content, &terms);
        assert_eq!(snippet.total_matches, 0);
    }
}
