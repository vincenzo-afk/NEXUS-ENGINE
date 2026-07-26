//! Ranking: turns a set of matched terms per document into an ordered,
//! scored result list.

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
    /// Multiplier applied because the query matched the filename.
    pub filename_boost: f32,
    /// Multiplier applied for exact (non-fuzzy) phrase matches.
    pub exact_match_boost: f32,
    /// Multiplier applied based on document recency.
    pub recency_boost: f32,
    /// Final score: `bm25_score * filename_boost * exact_match_boost * recency_boost`.
    pub final_score: f32,
}

/// Per-document intermediate data the ranker needs: which terms matched and
/// with what per-term frequency, plus whether the match included the exact
/// phrase / filename.
#[derive(Debug, Clone, Default)]
pub struct MatchInfo {
    /// `term -> term_frequency_in_document` for every query term that matched.
    pub term_frequencies: HashMap<String, u32>,
    /// True if the query's terms appear in the document's filename.
    pub filename_match: bool,
    /// True if a multi-term query matched as an exact contiguous phrase.
    pub exact_phrase_match: bool,
}

/// Scores a single document against the query, given precomputed per-term
/// document frequencies (for IDF) and match info.
pub fn score_document(
    index: &Index,
    doc_id: DocId,
    match_info: &MatchInfo,
    config: &RankingConfig,
    now_unix: i64,
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

    let final_score = bm25_total * filename_boost * exact_match_boost * recency_boost;

    Some(ScoreExplanation {
        bm25_score: bm25_total,
        filename_boost,
        exact_match_boost,
        recency_boost,
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
