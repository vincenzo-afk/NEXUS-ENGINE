//! Query evaluation: walks a [`QueryNode`] tree and produces the set of
//! matching documents, each annotated with the per-term match information
//! the ranking stage needs.

use log::{debug, info};

use crate::document::DocId;
use crate::index::Index;
use crate::query::{CompareOp, QueryNode};
use crate::ranking::{self, MatchInfo, ScoreExplanation};
use crate::spellcheck::levenshtein_distance;
use std::collections::HashMap;

/// A single ranked search result, ready for presentation.
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// The document's ID within the index.
    pub doc_id: DocId,
    /// Full path to the matched file.
    pub path: std::path::PathBuf,
    /// File name only.
    pub file_name: String,
    /// File size in bytes.
    pub size_bytes: u64,
    /// Last-modified time (UNIX seconds).
    pub modified_unix: i64,
    /// Number of distinct query terms that matched in this document.
    pub match_count: usize,
    /// Final relevance score (higher is better).
    pub score: f32,
    /// Full scoring breakdown, useful with `--explain`.
    pub explanation: ScoreExplanation,
}

pub fn search(
    index: &Index,
    query: &QueryNode,
    config: &crate::config::RankingConfig,
    offset: usize,
    limit: usize,
    clicks: Option<&crate::clicks::ClickLog>,
) -> (Vec<SearchResult>, usize) {
    info!("search query: {:?}", query);
    let matches = evaluate(query, index);
    let total_matches = matches.len();
    let now_unix = chrono::Utc::now().timestamp();

    let mut results: Vec<SearchResult> = matches
        .into_iter()
        .filter_map(|(doc_id, mut match_info)| {
            let metadata = index.store.get(doc_id)?;
            if !match_info.filename_match {
                match_info.filename_match = !match_info.term_frequencies.is_empty()
                    && match_info
                        .term_frequencies
                        .keys()
                        .all(|term| metadata.file_name.to_lowercase().contains(term.as_str()));
            }
            if !match_info.url_match {
                let path_lower = metadata.path.to_string_lossy().to_lowercase();
                match_info.url_match = !match_info.term_frequencies.is_empty()
                    && match_info
                        .term_frequencies
                        .keys()
                        .any(|term| path_lower.contains(term.as_str()));
            }

            let explanation =
                ranking::score_document(index, doc_id, &match_info, config, now_unix, clicks)?;

            Some(SearchResult {
                doc_id,
                path: metadata.path.clone(),
                file_name: metadata.file_name.clone(),
                size_bytes: metadata.size_bytes,
                modified_unix: metadata.modified_unix,
                match_count: match_info.term_frequencies.len(),
                score: explanation.final_score,
                explanation,
            })
        })
        .collect();

    debug!("search returned {} raw results", results.len());
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let paginated = results.into_iter().skip(offset).take(limit).collect();
    (paginated, total_matches)
}

/// Recursively evaluates a query node against the index, returning the
/// matching documents and their per-term match info.
fn evaluate(node: &QueryNode, index: &Index) -> HashMap<DocId, MatchInfo> {
    match node {
        QueryNode::Term(term) => match_term(term, index),
        QueryNode::Phrase(terms) => match_phrase(terms, index),
        QueryNode::Prefix(prefix) => match_predicate(index, |t| t.starts_with(prefix.as_str())),
        QueryNode::Wildcard(pattern) => match_predicate(index, |t| wildcard_matches(pattern, t)),
        QueryNode::Fuzzy { term, max_distance } => {
            match_predicate(index, |t| levenshtein_distance(term, t) <= *max_distance)
        }
        QueryNode::And(children) => evaluate_and(children, index),
        QueryNode::Or(children) => evaluate_or(children, index),
        QueryNode::Not(inner) => evaluate_not(inner, index),
        QueryNode::FilterExt(ext) => filter_docs(index, |meta| &meta.extension == ext),
        QueryNode::FilterPath(substring) => filter_docs(index, |meta| {
            meta.path
                .to_string_lossy()
                .to_lowercase()
                .contains(substring.as_str())
        }),
        QueryNode::FilterName(substring) => filter_docs(index, |meta| {
            meta.file_name.to_lowercase().contains(substring.as_str())
        }),
        QueryNode::FilterSize(op, threshold) => {
            filter_docs(index, |meta| compare(meta.size_bytes, *op, *threshold))
        }
        QueryNode::FilterModified(op, threshold_seconds) => {
            let now = chrono::Utc::now().timestamp();
            filter_docs(index, |meta| {
                let age_seconds = now - meta.modified_unix;
                // For `modified<7d` (newer than 7 days) age must be LESS than
                // the threshold; for `modified>7d` (older) age must be
                // GREATER. This is the inverse of a naive numeric comparison,
                // which is why we special-case it here rather than reusing
                // `compare` directly against age.
                match op {
                    CompareOp::LessThan => age_seconds < *threshold_seconds,
                    CompareOp::GreaterThan => age_seconds > *threshold_seconds,
                    CompareOp::Equal => age_seconds == *threshold_seconds,
                }
            })
        }
        QueryNode::FilterSite(domain) => filter_web_docs(index, |meta| {
            meta.domain == *domain || meta.domain.ends_with(&format!(".{domain}"))
        }),
        QueryNode::FilterDate(op, threshold_unix) => filter_docs(index, |meta| match op {
            CompareOp::LessThan => meta.modified_unix < *threshold_unix,
            CompareOp::GreaterThan => meta.modified_unix > *threshold_unix,
            CompareOp::Equal => meta.modified_unix == *threshold_unix,
        }),
        QueryNode::FilterLang(lang) => {
            filter_web_docs(index, |meta| meta.lang.as_deref() == Some(lang.as_str()))
        }
        QueryNode::FilterAuthor(author) => filter_web_docs(index, |meta| {
            meta.author
                .as_deref()
                .map(|a| a.to_lowercase().contains(author.as_str()))
                .unwrap_or(false)
        }),
    }
}

fn compare(value: u64, op: CompareOp, threshold: u64) -> bool {
    match op {
        CompareOp::GreaterThan => value > threshold,
        CompareOp::LessThan => value < threshold,
        CompareOp::Equal => value == threshold,
    }
}

fn filter_docs(
    index: &Index,
    predicate: impl Fn(&crate::document::DocumentMetadata) -> bool,
) -> HashMap<DocId, MatchInfo> {
    index
        .store
        .iter()
        .filter(|(_, meta)| predicate(meta))
        .map(|(id, _)| (id, MatchInfo::default()))
        .collect()
}

/// Like [`filter_docs`], but the predicate examines a web page's crawl
/// metadata rather than its generic [`crate::document::DocumentMetadata`].
/// Documents with no web metadata (local files) never match.
fn filter_web_docs(
    index: &Index,
    predicate: impl Fn(&crate::webdoc::WebPageMeta) -> bool,
) -> HashMap<DocId, MatchInfo> {
    index
        .web
        .iter()
        .filter(|(_, meta)| predicate(meta))
        .map(|(id, _)| (id, MatchInfo::default()))
        .collect()
}

fn match_term(term: &str, index: &Index) -> HashMap<DocId, MatchInfo> {
    let mut results = HashMap::new();
    if let Some(term_id) = index.vocabulary.get(term) {
        if let Some(list) = index.inverted.postings_for(term_id) {
            for posting in &list.postings {
                let entry = results
                    .entry(posting.doc_id)
                    .or_insert_with(MatchInfo::default);
                entry
                    .term_frequencies
                    .insert(term.to_string(), posting.term_frequency);
            }
        }
    }
    results
}

/// Matches every vocabulary term satisfying `predicate`, unioning their
/// postings into a single match-info map. Used for prefix, wildcard, and
/// fuzzy queries, all of which expand to a set of concrete terms.
fn match_predicate(index: &Index, predicate: impl Fn(&str) -> bool) -> HashMap<DocId, MatchInfo> {
    let mut results: HashMap<DocId, MatchInfo> = HashMap::new();
    for (term, term_id) in index.vocabulary.iter() {
        if !predicate(term) {
            continue;
        }
        if let Some(list) = index.inverted.postings_for(term_id) {
            for posting in &list.postings {
                let entry = results
                    .entry(posting.doc_id)
                    .or_insert_with(MatchInfo::default);
                entry
                    .term_frequencies
                    .entry(term.to_string())
                    .and_modify(|tf| *tf += posting.term_frequency)
                    .or_insert(posting.term_frequency);
            }
        }
    }
    results
}

/// Matches an exact, contiguous phrase by intersecting the postings of each
/// term and verifying that their positions are consecutive.
fn match_phrase(terms: &[String], index: &Index) -> HashMap<DocId, MatchInfo> {
    if terms.is_empty() {
        return HashMap::new();
    }
    if terms.len() == 1 {
        return match_term(&terms[0], index);
    }

    let posting_lists: Option<Vec<&crate::index::posting::PostingList>> = terms
        .iter()
        .map(|t| {
            index
                .vocabulary
                .get(t)
                .and_then(|id| index.inverted.postings_for(id))
        })
        .collect();

    let posting_lists = match posting_lists {
        Some(lists) => lists,
        None => return HashMap::new(), // one of the terms never occurs at all
    };

    // Start from the rarest term's documents to minimize work.
    let (rarest_idx, _) = posting_lists
        .iter()
        .enumerate()
        .min_by_key(|(_, list)| list.document_frequency())
        .expect("terms is non-empty, so posting_lists is non-empty");

    let mut results = HashMap::new();
    for candidate in &posting_lists[rarest_idx].postings {
        let doc_id = candidate.doc_id;

        // Gather each term's position list within this document.
        let position_lists: Option<Vec<&Vec<u32>>> = posting_lists
            .iter()
            .map(|list| list.get(doc_id).map(|p| &p.positions))
            .collect();

        let position_lists = match position_lists {
            Some(lists) => lists,
            None => continue, // doc is missing one of the terms entirely
        };

        if has_consecutive_run(&position_lists) {
            let mut match_info = MatchInfo::default();
            match_info.exact_phrase_match = true;
            for (i, term) in terms.iter().enumerate() {
                let tf = posting_lists[i]
                    .get(doc_id)
                    .map(|p| p.term_frequency)
                    .unwrap_or(0);
                match_info.term_frequencies.insert(term.clone(), tf);
            }
            results.insert(doc_id, match_info);
        }
    }
    results
}

/// Returns `true` if there exists a starting position `p` such that
/// `position_lists[0]` contains `p`, `position_lists[1]` contains `p+1`,
/// `position_lists[2]` contains `p+2`, etc.
fn has_consecutive_run(position_lists: &[&Vec<u32>]) -> bool {
    use std::collections::HashSet;
    let first_term_positions = position_lists[0];
    let later_sets: Vec<HashSet<u32>> = position_lists[1..]
        .iter()
        .map(|positions| positions.iter().copied().collect())
        .collect();

    first_term_positions.iter().any(|&start| {
        later_sets
            .iter()
            .enumerate()
            .all(|(offset, set)| set.contains(&(start + offset as u32 + 1)))
    })
}

fn evaluate_and(children: &[QueryNode], index: &Index) -> HashMap<DocId, MatchInfo> {
    // Positive (non-NOT) children are intersected; NOT children subtract
    // from the running result set.
    let mut positive: Vec<&QueryNode> = Vec::new();
    let mut negative: Vec<&QueryNode> = Vec::new();
    for child in children {
        if let QueryNode::Not(inner) = child {
            negative.push(inner);
        } else {
            positive.push(child);
        }
    }

    let mut result = if let Some((first, rest)) = positive.split_first() {
        let mut acc = evaluate(first, index);
        for child in rest {
            let next = evaluate(child, index);
            acc.retain(|doc_id, _| next.contains_key(doc_id));
            for (doc_id, info) in next {
                if let Some(existing) = acc.get_mut(&doc_id) {
                    merge_match_info(existing, info);
                }
            }
        }
        acc
    } else {
        // All-negative AND (e.g. bare "NOT x"): start from the full universe.
        index
            .store
            .iter()
            .map(|(id, _)| (id, MatchInfo::default()))
            .collect()
    };

    for neg in negative {
        let excluded = evaluate(neg, index);
        result.retain(|doc_id, _| !excluded.contains_key(doc_id));
    }

    result
}

fn evaluate_or(children: &[QueryNode], index: &Index) -> HashMap<DocId, MatchInfo> {
    let mut result: HashMap<DocId, MatchInfo> = HashMap::new();
    for child in children {
        let child_matches = evaluate(child, index);
        for (doc_id, info) in child_matches {
            result
                .entry(doc_id)
                .and_modify(|existing| merge_match_info(existing, info.clone()))
                .or_insert(info);
        }
    }
    result
}

fn evaluate_not(inner: &QueryNode, index: &Index) -> HashMap<DocId, MatchInfo> {
    let excluded = evaluate(inner, index);
    index
        .store
        .iter()
        .filter(|(id, _)| !excluded.contains_key(id))
        .map(|(id, _)| (id, MatchInfo::default()))
        .collect()
}

fn merge_match_info(existing: &mut MatchInfo, other: MatchInfo) {
    for (term, tf) in other.term_frequencies {
        existing
            .term_frequencies
            .entry(term)
            .and_modify(|v| *v += tf)
            .or_insert(tf);
    }
    existing.exact_phrase_match = existing.exact_phrase_match || other.exact_phrase_match;
    existing.filename_match = existing.filename_match || other.filename_match;
    existing.url_match = existing.url_match || other.url_match;
}

/// Matches a simple glob pattern (`*` = any run of characters, `?` = any
/// single character) against a whole term.
fn wildcard_matches(pattern: &str, term: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = term.chars().collect();
    wildcard_recursive(&p, &t)
}

fn wildcard_recursive(pattern: &[char], text: &[char]) -> bool {
    match pattern.first() {
        None => text.is_empty(),
        Some('*') => {
            wildcard_recursive(&pattern[1..], text)
                || (!text.is_empty() && wildcard_recursive(pattern, &text[1..]))
        }
        Some('?') => !text.is_empty() && wildcard_recursive(&pattern[1..], &text[1..]),
        Some(c) => {
            !text.is_empty() && *c == text[0] && wildcard_recursive(&pattern[1..], &text[1..])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matching_basics() {
        assert!(wildcard_matches("wor?d", "world"));
        assert!(wildcard_matches("pars*", "parser"));
        assert!(wildcard_matches("*ing", "parsing"));
        assert!(!wildcard_matches("wor?d", "words"));
    }
}
