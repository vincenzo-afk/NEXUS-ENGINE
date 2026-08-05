//! LLM-based reranking of the top retrieval candidates.
//!
//! Takes the results BM25 + PageRank + vector similarity already ranked
//! (see `crate::ranking`/`crate::vector`), sends the top N as a numbered
//! list to the configured LLM, and asks it to reorder them by relevance
//! to the query. This is a genuine additional signal — an LLM reading
//! the actual title/snippet text can catch relevance judgments the
//! numeric ranking signals miss — but it is exactly as good as whatever
//! model is configured, and it only re-*orders* the candidate set
//! retrieval already produced; it cannot surface a document that wasn't
//! retrieved in the first place.
//!
//! **Fail-safe by construction:** if the model's response can't be
//! parsed as a valid permutation of the input (wrong count, duplicates,
//! out-of-range numbers, or just not a list of numbers at all),
//! reranking is skipped and the original order is kept. A malformed or
//! unexpected LLM response should never break search or silently drop
//! results.

use crate::ai::client::LlmClient;
use crate::error::{NexusError, Result};

/// One candidate offered to the reranker. `id` is an opaque handle the
/// caller uses to map the returned order back to its own result list —
/// this module has no notion of `DocId`/`SearchResult`, keeping it
/// decoupled from ranking internals.
#[derive(Debug, Clone)]
pub struct RerankCandidate {
    pub id: usize,
    pub title: String,
    pub snippet: String,
}

/// Asks the LLM to reorder `candidates` by relevance to `query`, returning
/// the candidates' `id`s in the new order. Returns an error (rather than
/// panicking or silently returning nonsense) if the model's response
/// can't be validated as an actual permutation of the input — callers
/// should catch this and keep their original ordering.
pub fn rerank(client: &LlmClient, query: &str, candidates: &[RerankCandidate]) -> Result<Vec<usize>> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    if candidates.len() == 1 {
        return Ok(vec![candidates[0].id]);
    }

    let system_prompt = "You are a search relevance reranker. You will receive a user's \
        search query and a numbered list of search results (title and snippet). Reorder the \
        results from MOST to LEAST relevant to the query. Respond with ONLY a comma-separated \
        list of the result numbers in your new order — for example: 3,1,4,2. Every number from \
        1 to N must appear exactly once. Do not include any other text, explanation, or \
        formatting.";

    let mut user_prompt = format!("Query: {query}\n\nResults:\n");
    for (i, candidate) in candidates.iter().enumerate() {
        user_prompt.push_str(&format!(
            "{}. {}\n   {}\n",
            i + 1,
            truncate(&candidate.title, 200),
            truncate(&candidate.snippet, 400)
        ));
    }
    user_prompt.push_str("\nReordered list of numbers only:");

    let reply = client.chat(system_prompt, &user_prompt)?;
    let order = parse_permutation(&reply, candidates.len())
        .ok_or_else(|| NexusError::Other(format!("AI reranker returned an unparseable/invalid response: {reply:?}")))?;

    Ok(order.into_iter().map(|i| candidates[i].id).collect())
}

/// Parses `text` as a permutation of `1..=n`, returning 0-indexed
/// positions in the order they appeared. Returns `None` if the parsed
/// numbers aren't exactly `{1, 2, ..., n}` with no duplicates and no
/// extras — a partial or garbled response is treated as invalid rather
/// than partially trusted.
fn parse_permutation(text: &str, n: usize) -> Option<Vec<usize>> {
    let numbers: Vec<usize> = text
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<usize>().ok())
        .collect();

    if numbers.len() != n {
        return None;
    }

    let mut seen = vec![false; n + 1];
    for &num in &numbers {
        if num == 0 || num > n || seen[num] {
            return None;
        }
        seen[num] = true;
    }

    Some(numbers.into_iter().map(|num| num - 1).collect())
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

    #[test]
    fn parses_a_valid_permutation() {
        let result = parse_permutation("3,1,4,2", 4);
        assert_eq!(result, Some(vec![2, 0, 3, 1]));
    }

    #[test]
    fn parses_permutation_with_extra_whitespace_and_text() {
        let result = parse_permutation("Sure! Here you go: 2, 1, 3", 3);
        assert_eq!(result, Some(vec![1, 0, 2]));
    }

    #[test]
    fn rejects_wrong_count() {
        assert!(parse_permutation("1,2,3", 4).is_none());
        assert!(parse_permutation("1,2,3,4,5", 4).is_none());
    }

    #[test]
    fn rejects_duplicates() {
        assert!(parse_permutation("1,1,2,3", 4).is_none());
    }

    #[test]
    fn rejects_out_of_range_numbers() {
        assert!(parse_permutation("1,2,3,9", 4).is_none());
        assert!(parse_permutation("0,1,2,3", 4).is_none());
    }

    #[test]
    fn rejects_garbage_response() {
        assert!(parse_permutation("I cannot help with that.", 4).is_none());
        assert!(parse_permutation("", 4).is_none());
    }

    #[test]
    fn empty_candidates_short_circuits_to_empty() {
        let candidates: Vec<RerankCandidate> = Vec::new();
        assert!(candidates.is_empty());
    }
}
