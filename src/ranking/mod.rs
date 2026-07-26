//! Ranking: turns a set of matched terms per document into an ordered,
//! scored result list.
//!
//! The final score combines several independent multiplicative signals:
//! `BM25 * filename/title * exact-phrase * recency * PageRank *
//! domain-quality * click-history * URL-match`. Each signal defaults to a
//! neutral `1.0` when it doesn't apply to a given document (e.g. PageRank
//! is neutral for filesystem documents, which aren't part of any link
//! graph), so filesystem-only searches behave exactly as before.

pub mod bm25;
pub mod tfidf;

use crate::config::RankingConfig;
use crate::document::DocId;
use crate::index::Index;
use serde::Serialize;
use std::collections::HashMap;

/// A human-readable breakdown of how a document's final score was
/// computed, returned alongside search results so users (or the `--explain`
/// CLI flag) can understand ranking decisions.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ScoreExplanation {
    /// Raw BM25 score summed across all matched query terms.
    pub bm25_score: f32,
    /// Multiplier applied because the query matched the filename (local
    /// files) or the page title (web pages).
    pub filename_boost: f32,
    /// Multiplier applied for exact (non-fuzzy) phrase matches.
    pub exact_match_boost: f32,
    /// Multiplier applied based on document recency / crawl freshness.
    pub recency_boost: f32,
    /// Multiplier derived from the page's PageRank score within the
    /// crawled link graph. `1.0` for documents not part of any link graph.
    pub pagerank_boost: f32,
    /// Multiplier applied when the query matches a web page's URL.
    pub url_match_boost: f32,
    /// Multiplier derived from the page's domain reputation (trusted /
    /// spam domain lists).
    pub domain_quality_boost: f32,
    /// Multiplier derived from how often this result has been clicked
    /// historically.
    pub click_boost: f32,
    /// Final score: the product of every boost above times `bm25_score`.
    pub final_score: f32,
}

/// Per-document intermediate data the ranker needs: which terms matched and
/// with what per-term frequency, plus whether the match included the exact
/// phrase / filename / title / URL.
#[derive(Debug, Clone, Default)]
pub struct MatchInfo {
    /// `term -> term_frequency_in_document` for every query term that matched.
    pub term_frequencies: HashMap<String, u32>,
    /// True if the query's terms appear in the document's filename (local
    /// files) or page title (web pages — see [`crate::webdoc::WebPageMeta::title`]).
    pub filename_match: bool,
    /// True if a multi-term query matched as an exact contiguous phrase.
    pub exact_phrase_match: bool,
    /// True if the query's terms appear in a web page's URL.
    pub url_match: bool,
}

/// Scores a single document against the query, given precomputed per-term
/// document frequencies (for IDF) and match info. Looks up PageRank,
/// domain, and click-history signals from `index.web` and `clicks`
/// automatically; both are simply absent (and thus neutral) for
/// filesystem-only documents.
pub fn score_document(
    index: &Index,
    doc_id: DocId,
    match_info: &MatchInfo,
    config: &RankingConfig,
    now_unix: i64,
    clicks: Option<&crate::clicks::ClickLog>,
) -> Option<ScoreExplanation> {
    let metadata = index.store.get(doc_id)?;
    let doc_length = metadata.token_count.max(1) as f32;
    let avg_doc_length = index.inverted.average_document_length().max(1.0);
    let total_docs = index.document_count().max(1) as f32;

    let mut bm25_total = 0.0f32;
    for (term, &tf) in &match_info.term_frequencies {
        let df = index
            .vocabulary
            .get(term)
            .and_then(|id| index.inverted.postings_for(id))
            .map(|list| list.document_frequency())
            .unwrap_or(0) as f32;

        bm25_total += bm25::bm25_term_score(
            tf as f32,
            df,
            total_docs,
            doc_length,
            avg_doc_length,
            config.bm25_k1,
            config.bm25_b,
        );
    }

    let filename_boost = if match_info.filename_match {
        config.filename_boost
    } else {
        1.0
    };
    let exact_match_boost = if match_info.exact_phrase_match {
        config.exact_match_boost
    } else {
        1.0
    };
    let recency_boost = recency_multiplier(metadata.modified_unix, now_unix, config.recency_boost);
    let url_match_boost = if match_info.url_match {
        config.url_match_boost
    } else {
        1.0
    };

    let web_meta = index.web.get(doc_id);

    let pagerank_boost = match web_meta {
        Some(meta) => 1.0 + config.pagerank_weight * (meta.pagerank * total_docs).max(0.0),
        None => 1.0,
    };

    let domain_quality_boost = match web_meta {
        Some(meta) => {
            if config.spam_domains.contains(&meta.domain) {
                config.spam_domain_penalty
            } else if config.trusted_domains.contains(&meta.domain) {
                config.trusted_domain_boost
            } else {
                1.0
            }
        }
        None => 1.0,
    };

    let click_boost = match clicks {
        Some(log) => 1.0 + config.click_weight * (1.0 + log.clicks_for(doc_id) as f32).ln(),
        None => 1.0,
    };

    let final_score = bm25_total
        * filename_boost
        * exact_match_boost
        * recency_boost
        * pagerank_boost
        * url_match_boost
        * domain_quality_boost
        * click_boost;

    Some(ScoreExplanation {
        bm25_score: bm25_total,
        filename_boost,
        exact_match_boost,
        recency_boost,
        pagerank_boost,
        url_match_boost,
        domain_quality_boost,
        click_boost,
        final_score,
    })
}

/// Computes a smooth recency multiplier in the range `(1.0, max_boost]`
/// that decays exponentially with document age. Documents modified "now"
/// get the full `max_boost`; the boost approaches 1.0 as age grows,
/// with a half-life of 30 days.
fn recency_multiplier(modified_unix: i64, now_unix: i64, max_boost: f32) -> f32 {
    const HALF_LIFE_SECONDS: f64 = 30.0 * 24.0 * 60.0 * 60.0;
    let age_seconds = (now_unix - modified_unix).max(0) as f64;
    let decay = 0.5f64.powf(age_seconds / HALF_LIFE_SECONDS);
    (1.0 + (max_boost as f64 - 1.0) * decay) as f32
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::config::RankingConfig;
    use crate::document::{Document, DocumentMetadata};
    use crate::webdoc::WebPageMeta;
    use std::path::PathBuf;

    fn base_index() -> Index {
        Index::new()
    }

    fn index_doc(index: &mut Index, path: &str, content: &str) -> DocId {
        let metadata = DocumentMetadata {
            path: PathBuf::from(path),
            file_name: path.to_string(),
            extension: "html".to_string(),
            size_bytes: content.len() as u64,
            modified_unix: 0,
            token_count: 0,
        };
        let doc = Document {
            metadata,
            content: content.to_string(),
        };
        index.index_document(doc)
    }

    #[test]
    fn pagerank_and_domain_quality_boost_final_score() {
        let mut index = base_index();
        let doc_id = index_doc(&mut index, "https://example.com/a", "rust ownership guide");

        let mut match_info = MatchInfo::default();
        match_info.term_frequencies.insert("rust".to_string(), 1);

        let mut config = RankingConfig::default();
        config.trusted_domains.insert("wikipedia.org".to_string());

        let baseline = score_document(&index, doc_id, &match_info, &config, 0, None).unwrap();
        assert_eq!(baseline.pagerank_boost, 1.0);
        assert_eq!(baseline.domain_quality_boost, 1.0);

        index.web.insert(
            doc_id,
            WebPageMeta {
                url: "https://example.com/a".to_string(),
                domain: "wikipedia.org".to_string(),
                title: "Rust Ownership Guide".to_string(),
                meta_description: String::new(),
            lang: None,
            author: None,
                content_type: "html".to_string(),
                fetched_unix: 0,
                etag: None,
                last_modified: None,
                simhash: 0,
                depth: 0,
                outgoing: Vec::new(),
                incoming: Vec::new(),
                pagerank: 0.5,
            },
        );

        let boosted = score_document(&index, doc_id, &match_info, &config, 0, None).unwrap();
        assert!(boosted.pagerank_boost > 1.0);
        assert_eq!(boosted.domain_quality_boost, config.trusted_domain_boost);
        assert!(boosted.final_score > baseline.final_score);
    }

    #[test]
    fn click_history_increases_score_monotonically() {
        let mut index = base_index();
        let doc_id = index_doc(&mut index, "/tmp/a.txt", "rust programming");
        let mut match_info = MatchInfo::default();
        match_info.term_frequencies.insert("rust".to_string(), 1);
        let config = RankingConfig::default();

        let mut clicks = crate::clicks::ClickLog::default();
        let no_clicks = score_document(&index, doc_id, &match_info, &config, 0, Some(&clicks)).unwrap();
        clicks.record(doc_id);
        clicks.record(doc_id);
        clicks.record(doc_id);
        let with_clicks = score_document(&index, doc_id, &match_info, &config, 0, Some(&clicks)).unwrap();
        assert!(with_clicks.final_score > no_clicks.final_score);
    }
}
