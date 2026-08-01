//! Query evaluation: walks a [`QueryNode`] tree and produces the set of
//! matching documents, each annotated with the per-term match information
//! the ranking stage needs.

use log::{debug, info};
use serde::{Deserialize, Serialize};

use crate::document::DocId;
use crate::index::Index;
use crate::query::{CompareOp, QueryNode};
use crate::ranking::{self, MatchInfo, ScoreExplanation};
use crate::spellcheck::levenshtein_distance;
use std::collections::{HashMap, HashSet};

/// Which subset of the index a search should draw from — the core of the
/// four-mode toggle (Local / Web / Hybrid / Tor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    /// Only filesystem-indexed documents.
    Local,
    /// Only web-crawled documents, excluding `.onion` addresses (those
    /// belong to [`SearchMode::Tor`] only, so a plain web search never
    /// surfaces a hidden-service link a browser can't open anyway).
    Web,
    /// Both filesystem and web documents, merged into one ranked list
    /// with a local-result boost and cross-source duplicate suppression.
    Both,
    /// Only web-crawled documents whose URL is a `.onion` address.
    /// Deliberately kept separate from ordinary [`SearchMode::Web`]
    /// results rather than merged in: `.onion` links need Tor Browser to
    /// open at all, so mixing them into a normal web result list would
    /// be confusing and mildly risky (an accidental click).
    Tor,
}

impl Default for SearchMode {
    fn default() -> Self {
        SearchMode::Web
    }
}

impl SearchMode {
    /// Parses a mode from a loosely-typed string (e.g. an HTTP query
    /// parameter), accepting a few reasonable synonyms. Falls back to the
    /// default ([`SearchMode::Web`]) for anything unrecognized rather than
    /// erroring — an unknown `mode` value shouldn't break a search.
    pub fn from_query_param(s: &str) -> SearchMode {
        match s.to_lowercase().as_str() {
            "local" | "fs" | "pc" | "filesystem" => SearchMode::Local,
            "web" => SearchMode::Web,
            "both" | "hybrid" => SearchMode::Both,
            "tor" | "onion" => SearchMode::Tor,
            _ => SearchMode::default(),
        }
    }
}

/// A single ranked search result, ready for presentation.
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// The document's ID within the index.
    pub doc_id: DocId,
    /// Full path to the matched file (a filesystem path for local
    /// results, a URL string for web results).
    pub path: std::path::PathBuf,
    /// File name only (for web results, this is the page title if one
    /// was extracted, matching how the rest of the ranking pipeline
    /// already treats a web page's title as its "filename" — see
    /// `ranking::MatchInfo::filename_match`'s doc comment).
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
    /// `true` if this result came from the web crawler rather than local
    /// filesystem indexing.
    pub is_web: bool,
    /// `true` if this is a web result whose URL is a `.onion` address.
    pub is_onion: bool,
}

/// The result of a search: the paginated result page, plus counts useful
/// for presentation (e.g. hybrid mode's "5 local · 12 web" split).
#[derive(Debug, Clone)]
pub struct SearchOutcome {
    /// The requested page of results.
    pub results: Vec<SearchResult>,
    /// Total matching documents after mode filtering and (for hybrid
    /// mode) cross-source dedup, before pagination.
    pub total: usize,
    /// Of `total`, how many are local filesystem results.
    pub local_count: usize,
    /// Of `total`, how many are web results.
    pub web_count: usize,
}

/// Above this many pre-pagination candidates, hybrid mode's O(n^2)
/// cross-source duplicate comparison is skipped rather than run
/// unbounded. This is a genuine, documented tradeoff: on very broad
/// queries against a large combined index, a full pairwise SimHash
/// comparison would be too slow to run per-request, so extremely broad
/// hybrid queries may show occasional near-duplicate local/web pairs
/// rather than blocking. Bump this if profiling shows headroom.
const HYBRID_DEDUP_MAX_CANDIDATES: usize = 500;

pub fn search(
    index: &Index,
    query: &QueryNode,
    config: &crate::config::RankingConfig,
    offset: usize,
    limit: usize,
    clicks: Option<&crate::clicks::ClickLog>,
    mode: SearchMode,
) -> SearchOutcome {
    info!("search query: {:?} (mode={:?})", query, mode);
    let matches = evaluate(query, index);
    let now_unix = chrono::Utc::now().timestamp();

    let query_terms = crate::query::collect_terms(query);
    let query_vec = if config.vector_weight > 0.0 {
        crate::vector::query_vector(index, &query_terms)
    } else {
        None
    };

    let mut results: Vec<SearchResult> = matches
        .into_iter()
        .filter_map(|(doc_id, mut match_info)| {
            let metadata = index.store.get(doc_id)?;
            let web_meta = index.web.get(doc_id);
            let is_web = web_meta.is_some();
            let is_onion = web_meta
                .map(|m| m.url.to_lowercase().contains(".onion"))
                .unwrap_or(false);

            // Mode filtering happens before scoring/pagination so offset
            // and limit apply to the *filtered* set, not the full match
            // set with out-of-mode results silently consuming page slots.
            match mode {
                SearchMode::Local => {
                    if is_web {
                        return None;
                    }
                }
                SearchMode::Web => {
                    if !is_web || is_onion {
                        return None;
                    }
                }
                SearchMode::Tor => {
                    if !is_web || !is_onion {
                        return None;
                    }
                }
                SearchMode::Both => {
                    // No source filtering; every match is a candidate.
                    // .onion results are still excluded from hybrid mode
                    // for the same reason Web mode excludes them: they
                    // need Tor Browser specifically, and hybrid mode's
                    // whole point is "give me an ordinary merged view."
                    if is_onion {
                        return None;
                    }
                }
            }

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

            let mut explanation =
                ranking::score_document(index, doc_id, &match_info, config, now_unix, clicks)?;

            if mode == SearchMode::Both && !is_web {
                explanation.final_score *= config.local_boost;
            }

            if let Some(qv) = &query_vec {
                if let Some(doc_vec) = index.vectors.get(doc_id) {
                    let similarity = qv.cosine_similarity(doc_vec).max(0.0);
                    let vector_boost = 1.0 + config.vector_weight * similarity;
                    explanation.vector_boost = vector_boost;
                    explanation.final_score *= vector_boost;
                }
            }

            Some(SearchResult {
                doc_id,
                path: metadata.path.clone(),
                file_name: metadata.file_name.clone(),
                size_bytes: metadata.size_bytes,
                modified_unix: metadata.modified_unix,
                match_count: match_info.term_frequencies.len(),
                score: explanation.final_score,
                explanation,
                is_web,
                is_onion,
            })
        })
        .collect();

    debug!("search returned {} raw results (mode={:?})", results.len(), mode);

    if mode == SearchMode::Both {
        results = dedupe_hybrid_cross_source(index, results, config.hybrid_dedup_min_similarity);
    }

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let total = results.len();
    let local_count = results.iter().filter(|r| !r.is_web).count();
    let web_count = total - local_count;

    let paginated = results.into_iter().skip(offset).take(limit).collect();
    SearchOutcome {
        results: paginated,
        total,
        local_count,
        web_count,
    }
}

/// Hybrid mode's cross-source dedup: when a local file and a web page are
/// near-duplicate content (SimHash similarity at or above
/// `min_similarity`), only the local result is kept — "you already have
/// this on disk" outranks "here's a copy of it on the web."
///
/// Skips the comparison entirely (returning `results` unchanged) once the
/// pre-pagination candidate count exceeds [`HYBRID_DEDUP_MAX_CANDIDATES`],
/// since this is an O(n^2) pairwise comparison; see that constant's doc
/// comment for the tradeoff this represents.
fn dedupe_hybrid_cross_source(
    index: &Index,
    results: Vec<SearchResult>,
    min_similarity: f32,
) -> Vec<SearchResult> {
    if results.len() > HYBRID_DEDUP_MAX_CANDIDATES {
        debug!(
            "hybrid dedup skipped: {} candidates exceeds cap of {}",
            results.len(),
            HYBRID_DEDUP_MAX_CANDIDATES
        );
        return results;
    }

    let local_fingerprints: Vec<(DocId, crate::dedup::SimHash)> = results
        .iter()
        .filter(|r| !r.is_web)
        .filter_map(|r| index.duplicates.simhash_of(r.doc_id).map(|s| (r.doc_id, s)))
        .collect();

    if local_fingerprints.is_empty() {
        return results;
    }

    let mut to_remove: HashSet<DocId> = HashSet::new();
    for result in results.iter().filter(|r| r.is_web) {
        let Some(web_sim) = index.duplicates.simhash_of(result.doc_id) else {
            continue;
        };
        let is_duplicate_of_a_local_file = local_fingerprints
            .iter()
            .any(|(_, local_sim)| local_sim.similarity(&web_sim) >= min_similarity);
        if is_duplicate_of_a_local_file {
            to_remove.insert(result.doc_id);
        }
    }

    if to_remove.is_empty() {
        return results;
    }

    debug!(
        "hybrid dedup: removing {} web result(s) that duplicate a local file",
        to_remove.len()
    );
    results
        .into_iter()
        .filter(|r| !to_remove.contains(&r.doc_id))
        .collect()
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
    use crate::document::{Document, DocumentMetadata};
    use crate::webdoc::WebPageMeta;
    use std::path::PathBuf;

    #[test]
    fn glob_matching_basics() {
        assert!(wildcard_matches("wor?d", "world"));
        assert!(wildcard_matches("pars*", "parser"));
        assert!(wildcard_matches("*ing", "parsing"));
        assert!(!wildcard_matches("wor?d", "words"));
    }

    fn index_local(index: &mut Index, path: &str, content: &str) -> DocId {
        let metadata = DocumentMetadata {
            path: PathBuf::from(path),
            file_name: path.trim_start_matches('/').to_string(),
            extension: "txt".to_string(),
            size_bytes: content.len() as u64,
            modified_unix: 0,
            token_count: 0,
        };
        index.index_document(Document { metadata, content: content.to_string() })
    }

    fn index_web(index: &mut Index, url: &str, title: &str, content: &str) -> DocId {
        let metadata = DocumentMetadata {
            path: PathBuf::from(url),
            file_name: title.to_string(),
            extension: "html".to_string(),
            size_bytes: content.len() as u64,
            modified_unix: 0,
            token_count: 0,
        };
        let doc_id = index.index_document(Document { metadata, content: content.to_string() });
        index.web.insert(
            doc_id,
            WebPageMeta {
                url: url.to_string(),
                domain: "example.com".to_string(),
                title: title.to_string(),
                meta_description: String::new(),
                lang: None,
                author: None,
                content_type: "html".to_string(),
                fetched_unix: 0,
                etag: None,
                last_modified: None,
                redirect_chain: Vec::new(),
                simhash: 0,
                depth: 0,
                outgoing: Vec::new(),
                incoming: Vec::new(),
                pagerank: 0.0,
            },
        );
        doc_id
    }

    fn parse_and_search(
        index: &Index,
        query: &str,
        mode: SearchMode,
    ) -> SearchOutcome {
        let ast = crate::query::parse(query).unwrap();
        let config = crate::config::RankingConfig::default();
        search(index, &ast, &config, 0, 50, None, mode)
    }

    #[test]
    fn local_mode_excludes_web_results() {
        let mut index = Index::new();
        index_local(&mut index, "/notes.txt", "rust programming notes");
        index_web(&mut index, "https://example.com/rust", "Rust Guide", "rust programming guide");

        let outcome = parse_and_search(&index, "rust", SearchMode::Local);
        assert_eq!(outcome.results.len(), 1);
        assert!(!outcome.results[0].is_web);
        assert_eq!(outcome.web_count, 0);
        assert_eq!(outcome.local_count, 1);
    }

    #[test]
    fn web_mode_excludes_local_results_and_onion_addresses() {
        let mut index = Index::new();
        index_local(&mut index, "/notes.txt", "rust programming notes");
        index_web(&mut index, "https://example.com/rust", "Rust Guide", "rust programming guide");
        index_web(
            &mut index,
            "http://exampleonionaddr1234567890abcdefghijklmnopqrstuvwxyz234567.onion/rust",
            "Onion Rust",
            "rust programming onion",
        );

        let outcome = parse_and_search(&index, "rust", SearchMode::Web);
        assert_eq!(outcome.results.len(), 1);
        assert!(outcome.results[0].is_web);
        assert!(!outcome.results[0].is_onion);
        assert_eq!(outcome.results[0].file_name, "Rust Guide");
    }

    #[test]
    fn tor_mode_only_returns_onion_addresses() {
        let mut index = Index::new();
        index_local(&mut index, "/notes.txt", "rust programming notes");
        index_web(&mut index, "https://example.com/rust", "Rust Guide", "rust programming guide");
        index_web(
            &mut index,
            "http://exampleonionaddr1234567890abcdefghijklmnopqrstuvwxyz234567.onion/rust",
            "Onion Rust",
            "rust programming onion",
        );

        let outcome = parse_and_search(&index, "rust", SearchMode::Tor);
        assert_eq!(outcome.results.len(), 1);
        assert!(outcome.results[0].is_onion);
        assert_eq!(outcome.results[0].file_name, "Onion Rust");
    }

    #[test]
    fn tor_mode_returns_empty_when_no_onion_docs_indexed() {
        let mut index = Index::new();
        index_local(&mut index, "/notes.txt", "rust programming notes");
        index_web(&mut index, "https://example.com/rust", "Rust Guide", "rust programming guide");

        let outcome = parse_and_search(&index, "rust", SearchMode::Tor);
        assert!(outcome.results.is_empty());
        assert_eq!(outcome.total, 0);
    }

    #[test]
    fn both_mode_merges_local_and_web_and_excludes_onion() {
        let mut index = Index::new();
        index_local(&mut index, "/notes.txt", "rust programming notes");
        index_web(&mut index, "https://example.com/rust", "Rust Guide", "rust programming guide");
        index_web(
            &mut index,
            "http://exampleonionaddr1234567890abcdefghijklmnopqrstuvwxyz234567.onion/rust",
            "Onion Rust",
            "rust programming onion",
        );

        let outcome = parse_and_search(&index, "rust", SearchMode::Both);
        assert_eq!(outcome.results.len(), 2, "should include the local file and the clearnet web page, not the onion one");
        assert!(outcome.results.iter().any(|r| !r.is_web));
        assert!(outcome.results.iter().any(|r| r.is_web && !r.is_onion));
        assert!(!outcome.results.iter().any(|r| r.is_onion));
        assert_eq!(outcome.local_count, 1);
        assert_eq!(outcome.web_count, 1);
    }

    #[test]
    fn both_mode_boosts_local_results_above_equally_relevant_web_results() {
        let mut index = Index::new();
        // Different surrounding text (so these aren't near-duplicates and
        // the dedup step, tested separately below, doesn't collapse them)
        // but comparable relevance to the query term "zephyr" so BM25
        // alone would be close to tied; the local_boost should still push
        // the local result to rank first.
        index_local(
            &mut index,
            "/notes.txt",
            "zephyr is a lightweight configuration format for local tools",
        );
        index_web(
            &mut index,
            "https://example.com/a",
            "A Page",
            "zephyr is a lightweight configuration format for web services",
        );

        let outcome = parse_and_search(&index, "zephyr", SearchMode::Both);
        assert_eq!(outcome.results.len(), 2);
        assert!(
            !outcome.results[0].is_web,
            "the local result should rank first due to the hybrid local boost"
        );
        assert!(outcome.results[0].score > outcome.results[1].score);
    }

    #[test]
    fn both_mode_dedupes_near_duplicate_web_result_in_favor_of_local() {
        let mut index = Index::new();
        let shared_text = "The definitive guide to rust ownership and borrowing semantics in depth.";
        index_local(&mut index, "/local-copy.txt", shared_text);
        index_web(&mut index, "https://example.com/mirror", "Mirror", shared_text);

        let outcome = parse_and_search(&index, "ownership", SearchMode::Both);
        assert_eq!(
            outcome.results.len(),
            1,
            "identical content from local + web should collapse to just the local result"
        );
        assert!(!outcome.results[0].is_web);
    }

    #[test]
    fn both_mode_keeps_both_when_content_is_not_actually_similar() {
        let mut index = Index::new();
        index_local(&mut index, "/local.txt", "rust ownership deep dive local notes");
        index_web(
            &mut index,
            "https://example.com/other",
            "Different Page",
            "rust concurrency channels and async runtimes overview",
        );

        let outcome = parse_and_search(&index, "rust", SearchMode::Both);
        assert_eq!(outcome.results.len(), 2, "unrelated local and web content about the same broad topic should not be deduped");
    }

    #[test]
    fn search_mode_from_query_param_recognizes_synonyms() {
        assert_eq!(SearchMode::from_query_param("local"), SearchMode::Local);
        assert_eq!(SearchMode::from_query_param("PC"), SearchMode::Local);
        assert_eq!(SearchMode::from_query_param("web"), SearchMode::Web);
        assert_eq!(SearchMode::from_query_param("hybrid"), SearchMode::Both);
        assert_eq!(SearchMode::from_query_param("both"), SearchMode::Both);
        assert_eq!(SearchMode::from_query_param("tor"), SearchMode::Tor);
        assert_eq!(SearchMode::from_query_param("onion"), SearchMode::Tor);
        assert_eq!(SearchMode::from_query_param("nonsense"), SearchMode::Web);
    }

    #[test]
    fn default_search_mode_is_web() {
        assert_eq!(SearchMode::default(), SearchMode::Web);
    }

    #[test]
    fn vector_similarity_reranks_within_the_bm25_matched_set() {
        let mut index = Index::new();
        // Both documents match "rust" exactly once, so their BM25 scores
        // are close/identical — but doc B's surrounding vocabulary
        // overlaps far more with the query's other implied context
        // (through the shared "ownership"/"borrowing"/"memory" terms),
        // so its lexical vector should be more similar to the query.
        let doc_a = index_local(
            &mut index,
            "/a.txt",
            "rust is a popular choice for game development and web assembly targets",
        );
        let doc_b = index_local(
            &mut index,
            "/b.txt",
            "rust enforces ownership and borrowing rules for memory safety at compile time",
        );

        let ast = crate::query::parse("rust OR ownership OR borrowing OR memory OR safety").unwrap();
        let mut config = crate::config::RankingConfig::default();
        config.vector_weight = 2.0; // exaggerate the signal so the test isn't flaky
        let outcome = search(&index, &ast, &config, 0, 50, None, SearchMode::Both);

        let a_result = outcome.results.iter().find(|r| r.doc_id == doc_a).unwrap();
        let b_result = outcome.results.iter().find(|r| r.doc_id == doc_b).unwrap();
        assert!(
            b_result.explanation.vector_boost > a_result.explanation.vector_boost,
            "doc B's content is more similar to the full query context and should get a larger vector boost"
        );
    }

    #[test]
    fn vector_weight_zero_disables_the_signal_entirely() {
        let mut index = Index::new();
        index_local(&mut index, "/a.txt", "rust ownership borrowing memory safety deep dive");

        let ast = crate::query::parse("rust ownership").unwrap();
        let mut config = crate::config::RankingConfig::default();
        config.vector_weight = 0.0;
        let outcome = search(&index, &ast, &config, 0, 50, None, SearchMode::Both);

        assert_eq!(outcome.results[0].explanation.vector_boost, 1.0);
    }
}
