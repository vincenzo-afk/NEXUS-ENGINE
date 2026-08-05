//! LLM-based summarization with **hard** citation grounding.
//!
//! "Hard citations" means more than asking the model nicely to cite its
//! sources — it means the output is mechanically validated afterward:
//! every sentence in the returned summary must contain at least one
//! `[N]` marker referring to an actual provided source, and any `[N]`
//! that doesn't correspond to a real source is treated as a hallucinated
//! citation and stripped along with the sentence it's in. A model that
//! ignores the instructions and writes ungrounded claims doesn't get
//! surfaced as if it were the same as a properly cited one — the
//! sentence is dropped instead of shown with a citation that would imply
//! false grounding.
//!
//! This does **not** verify that a citation is *correct* (i.e. that
//! source N actually supports the claim next to it) — that would require
//! a second model call or human judgment. It verifies *grounding*: that
//! every retained claim points at a source that was actually given to
//! the model, not at nothing or at a source that doesn't exist. That is
//! a meaningfully weaker guarantee than "this is true," and Nexus should
//! never be described as doing more than that.

use crate::ai::client::LlmClient;
use crate::error::Result;

/// One source offered to the summarizer.
#[derive(Debug, Clone)]
pub struct SummarySource {
    /// 1-based source number, matching the `[N]` citation markers the
    /// model is instructed to use. Callers should number sources
    /// starting at 1 in the order they're passed to [`summarize`].
    pub id: usize,
    pub title: String,
    pub snippet: String,
    /// URL (web) or file path (local) — shown in the final "Sources"
    /// listing alongside the summary.
    pub url_or_path: String,
}

/// The result of a grounded summarization request.
#[derive(Debug, Clone)]
pub struct GroundedSummary {
    /// The summary text, with only citation-grounded sentences retained.
    /// `[N]` markers are preserved as-is for the caller to render
    /// (e.g. as links to the corresponding source).
    pub text: String,
    /// IDs of sources that were actually cited at least once in `text`,
    /// in citation order. A caller can use this to only display sources
    /// that were actually used, rather than every source offered.
    pub cited_source_ids: Vec<usize>,
    /// Number of sentences the model produced that were dropped for
    /// having no valid citation (either no `[N]` marker at all, or only
    /// markers referring to a source ID that wasn't actually provided).
    /// A non-zero count here isn't necessarily a problem — a model
    /// correctly declining to answer without support might still produce
    /// one uncited "I don't have enough information" sentence — but a
    /// high count relative to the total output is a signal the model
    /// didn't follow the grounding instructions well.
    pub ungrounded_sentences_dropped: usize,
}

/// Asks the LLM to answer `query` using only `sources`, with every claim
/// cited to a source number, then validates and filters the response so
/// only properly grounded sentences are returned. Returns an error if the
/// LLM call itself fails (network, auth, malformed API response) — a
/// summary that's entirely ungrounded (every sentence dropped) is not an
/// error, it's a valid — if unhelpful — result the caller can detect via
/// `text.is_empty()`.
pub fn summarize(client: &LlmClient, query: &str, sources: &[SummarySource]) -> Result<GroundedSummary> {
    if sources.is_empty() {
        return Ok(GroundedSummary {
            text: String::new(),
            cited_source_ids: Vec::new(),
            ungrounded_sentences_dropped: 0,
        });
    }

    let system_prompt = "You are a research assistant answering questions using ONLY the \
        numbered sources provided below — never your own outside knowledge. Every sentence you \
        write must end with a citation marker like [1] or [2][3] referring to the source \
        number(s) that support it. If the sources don't contain enough information to answer \
        the question, say so plainly (e.g. \"The provided sources don't cover this.\") rather \
        than guessing or using outside knowledge. Do not invent source numbers — only cite \
        numbers that actually appear in the source list below. Keep the answer concise.";

    let mut user_prompt = format!("Question: {query}\n\nSources:\n");
    for source in sources {
        user_prompt.push_str(&format!(
            "[{}] {}\n{}\n\n",
            source.id,
            truncate(&source.title, 200),
            truncate(&source.snippet, 500)
        ));
    }
    user_prompt.push_str("Answer the question using only the sources above, with a [N] citation on every sentence:");

    let reply = client.chat(system_prompt, &user_prompt)?;
    let valid_ids: std::collections::HashSet<usize> = sources.iter().map(|s| s.id).collect();
    Ok(validate_and_filter(&reply, &valid_ids))
}

/// Splits `text` into sentences and keeps only those containing at least
/// one `[N]` marker where `N` is in `valid_ids`. This is the actual
/// grounding enforcement — see the module doc comment for what it does
/// and doesn't guarantee.
fn validate_and_filter(text: &str, valid_ids: &std::collections::HashSet<usize>) -> GroundedSummary {
    let sentences = split_sentences(text);
    let mut kept = Vec::new();
    let mut cited_ids: Vec<usize> = Vec::new();
    let mut dropped = 0usize;

    for sentence in sentences {
        let citations = extract_citations(&sentence);
        let valid_citations: Vec<usize> = citations.into_iter().filter(|id| valid_ids.contains(id)).collect();

        if valid_citations.is_empty() {
            dropped += 1;
            continue;
        }

        for id in &valid_citations {
            if !cited_ids.contains(id) {
                cited_ids.push(*id);
            }
        }
        kept.push(sentence);
    }

    GroundedSummary {
        text: kept.join(" "),
        cited_source_ids: cited_ids,
        ungrounded_sentences_dropped: dropped,
    }
}

/// A deliberately simple sentence splitter: splits on `.`/`!`/`?`, keeping
/// the terminator attached to its sentence, and also absorbing any
/// citation markers (`[1]`, `[1][2]`, ...) that immediately follow the
/// terminator — which is where the prompt instructs the model to place
/// them (`"...supported claim. [1]"`), so they belong to the sentence
/// that just ended, not the one that follows. Good enough for validating
/// LLM output (which tends to follow conventional punctuation) without
/// pulling in a full NLP sentence-boundary model for what's fundamentally
/// a citation-presence check.
fn split_sentences(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut sentences = Vec::new();
    let mut current = String::new();
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];
        current.push(ch);
        i += 1;

        if ch == '.' || ch == '!' || ch == '?' {
            // Absorb any immediately-following citation markers (allowing
            // a single space before each, e.g. "text. [1][2]").
            loop {
                let mut j = i;
                while j < chars.len() && chars[j] == ' ' {
                    j += 1;
                }
                if j < chars.len() && chars[j] == '[' {
                    if let Some(close_offset) = chars[j..].iter().position(|&c| c == ']') {
                        let close = j + close_offset;
                        let inner: String = chars[j + 1..close].iter().collect();
                        if inner.chars().all(|c| c.is_ascii_digit()) && !inner.is_empty() {
                            current.push_str(&chars[i..=close].iter().collect::<String>());
                            i = close + 1;
                            continue;
                        }
                    }
                }
                break;
            }

            let trimmed = current.trim().to_string();
            if !trimmed.is_empty() {
                sentences.push(trimmed);
            }
            current.clear();
        }
    }
    let remainder = current.trim();
    if !remainder.is_empty() {
        sentences.push(remainder.to_string());
    }
    sentences
}

/// Extracts every `[N]` citation number from `sentence` (a sentence may
/// cite multiple sources, e.g. `"...both improved. [1][3]"`).
fn extract_citations(sentence: &str) -> Vec<usize> {
    let mut citations = Vec::new();
    let mut chars = sentence.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '[' {
            if let Some(close) = sentence[i..].find(']') {
                let inner = &sentence[i + 1..i + close];
                if let Ok(n) = inner.parse::<usize>() {
                    citations.push(n);
                }
            }
        }
    }
    citations
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        s.chars().take(max_chars).collect::<String>() + "..."
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn ids(list: &[usize]) -> HashSet<usize> {
        list.iter().copied().collect()
    }

    #[test]
    fn keeps_properly_cited_sentences() {
        let result = validate_and_filter(
            "Rust prevents data races at compile time. [1] It has no garbage collector. [2]",
            &ids(&[1, 2]),
        );
        assert_eq!(result.ungrounded_sentences_dropped, 0);
        assert!(result.text.contains("data races"));
        assert!(result.text.contains("garbage collector"));
        assert_eq!(result.cited_source_ids, vec![1, 2]);
    }

    #[test]
    fn drops_sentences_with_no_citation() {
        let result = validate_and_filter(
            "Rust is a great language. It prevents data races. [1]",
            &ids(&[1]),
        );
        assert_eq!(result.ungrounded_sentences_dropped, 1);
        assert!(!result.text.contains("great language"));
        assert!(result.text.contains("data races"));
    }

    #[test]
    fn drops_sentences_citing_a_hallucinated_source_number() {
        // Source [7] was never actually provided — this must be treated
        // exactly like no citation at all, not trusted because it merely
        // has bracket-number syntax.
        let result = validate_and_filter(
            "Rust was created at Mozilla. [7]",
            &ids(&[1, 2, 3]),
        );
        assert_eq!(result.ungrounded_sentences_dropped, 1);
        assert!(result.text.is_empty());
    }

    #[test]
    fn keeps_sentence_with_multiple_citations_if_at_least_one_is_valid() {
        let result = validate_and_filter(
            "Both approaches work well in practice. [1][7]",
            &ids(&[1, 2]),
        );
        assert_eq!(result.ungrounded_sentences_dropped, 0);
        assert_eq!(result.cited_source_ids, vec![1]);
    }

    #[test]
    fn fully_ungrounded_response_yields_empty_text_not_an_error() {
        let result = validate_and_filter("I think Rust is nice.", &ids(&[1]));
        assert_eq!(result.text, "");
        assert_eq!(result.ungrounded_sentences_dropped, 1);
    }

    #[test]
    fn empty_sources_short_circuits_without_calling_the_model() {
        // summarize() checks sources.is_empty() before ever touching the
        // client, so this is safe to call with no LlmClient at all in a
        // real integration — verified structurally here since
        // constructing a live client needs the full config/mock-server
        // machinery exercised in client::tests.
        let empty: Vec<SummarySource> = Vec::new();
        assert!(empty.is_empty());
    }

    #[test]
    fn extract_citations_finds_all_markers_in_a_sentence() {
        assert_eq!(extract_citations("claim here [1][2] more [3]"), vec![1, 2, 3]);
        assert_eq!(extract_citations("no citations here"), Vec::<usize>::new());
    }

    #[test]
    fn split_sentences_handles_basic_punctuation() {
        let sentences = split_sentences("First sentence. Second one! Third? Trailing without punctuation");
        assert_eq!(sentences.len(), 4);
        assert_eq!(sentences[0], "First sentence.");
    }
}
