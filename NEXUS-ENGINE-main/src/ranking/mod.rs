//! Ranking: turns a set of matched terms per document into an ordered,
//! scored result list.
//!
//! The final score combines several independent multiplicative signals:
//! `BM25 * filename/title * exact-phrase * recency * PageRank *
//! domain-quality * click-history * URL-match`. Each signal defaults to a
//! neutral `1.0` when it doesn't apply to a given document (e.g. PageRank
//! is neutral for filesystem documents, which aren't part of any link
//! graph), so filesystem-only searches behave exactly as before.

pub mod adaptive;
pub mod bm25;
pub mod reliability;
pub mod tfidf;

use log::debug;

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
    /// Multiplicative penalty from `classify::spam::SpamClassifier`'s
    /// crawl-time per-page score (`1.0` = no penalty, lower = more
    /// spam-like). Distinct from `domain_quality_boost`'s manual
    /// per-domain list — this is automatic and per-page.
    pub spam_score_boost: f32,
    /// Multiplier derived from how often this result has been clicked
    /// historically.
    pub click_boost: f32,
    /// Multiplier from the lexical vector similarity signal (see
    /// `crate::vector`) — `1.0` if vector retrieval found no similarity
    /// beyond what BM25 already captured, or if this document has no
    /// stored vector.
    pub vector_boost: f32,
    /// Final score: the product of every boost above times `bm25_score`.
    pub final_score: f32,
}

/// One human-readable reason contributing to a result's rank, as returned
/// by [`ScoreExplanation::reasons`]. This is the data behind "explainable
/// ranking" — surfaced via `--explain` in the CLI and an `explanation`
/// field in the API response, rather than hidden the way most search
/// engines keep ranking signals opaque.
#[derive(Debug, Clone, Serialize)]
pub struct ExplanationReason {
    /// Short label for the signal, e.g. "Exact phrase match".
    pub label: String,
    /// How much this signal changed the score, as a percentage
    /// (`+50.0` for a 1.5x boost, `-20.0` for a 0.8x penalty). `0.0` for
    /// the baseline BM25 entry, which isn't a multiplier.
    pub impact_percent: f32,
    /// A plain-language sentence explaining the signal.
    pub detail: String,
}

impl ScoreExplanation {
    /// Builds a human-readable breakdown of why this document scored the
    /// way it did. Only includes signals that actually moved the score
    /// (multiplier != 1.0), plus the baseline BM25 relevance entry, so
    /// the result is a short, genuinely informative list rather than
    /// always showing all eight possible signals including no-ops.
    pub fn reasons(&self) -> Vec<ExplanationReason> {
        let mut reasons = vec![ExplanationReason {
            label: "Keyword relevance (BM25)".to_string(),
            impact_percent: 0.0,
            detail: format!(
                "Base text-match score of {:.2} from how well your search terms matched the document's content — this is the foundation every other signal below adjusts.",
                self.bm25_score
            ),
        }];

        let mut push = |label: &str, boost: f32, detail: String| {
            if (boost - 1.0).abs() > 0.001 {
                reasons.push(ExplanationReason {
                    label: label.to_string(),
                    impact_percent: (boost - 1.0) * 100.0,
                    detail,
                });
            }
        };

        push(
            "Title / filename match",
            self.filename_boost,
            "Your search terms appear in the page title or file name, which is a stronger relevance signal than appearing only in the body.".to_string(),
        );
        push(
            "Exact phrase match",
            self.exact_match_boost,
            "Your search terms matched as an exact, contiguous phrase rather than scattered individual words.".to_string(),
        );
        if self.recency_boost > 1.0 {
            push(
                "Recency",
                self.recency_boost,
                "This document was modified or crawled recently.".to_string(),
            );
        }
        if self.pagerank_boost > 1.0 {
            push(
                "Authority (PageRank)",
                self.pagerank_boost,
                "Other pages in the index link to this one, which PageRank treats as a vote of relevance/trust from the rest of the crawled web graph.".to_string(),
            );
        }
        push(
            "URL match",
            self.url_match_boost,
            "Your search terms appear directly in the page's URL.".to_string(),
        );
        if self.domain_quality_boost > 1.0 {
            push(
                "Trusted domain",
                self.domain_quality_boost,
                "This page's domain is on the configured trusted-domain list.".to_string(),
            );
        } else if self.domain_quality_boost < 1.0 {
            push(
                "Low-quality domain",
                self.domain_quality_boost,
                "This page's domain is on the configured spam/low-quality domain list, which penalizes it rather than excluding it outright.".to_string(),
            );
        }
        if self.click_boost > 1.0 {
            push(
                "Click history",
                self.click_boost,
                "Other searches have resulted in this document being clicked before, which this engine treats as a signal it was actually useful.".to_string(),
            );
        }
        if self.spam_score_boost < 1.0 {
            push(
                "Spam signals",
                self.spam_score_boost,
                "This page's content matched heuristic signals for low-quality, doorway, or repetitive filler content, which penalizes it rather than excluding it outright.".to_string(),
            );
        }
        if self.vector_boost > 1.0 {
            push(
                "Content similarity (vector)",
                self.vector_boost,
                "This document's overall word distribution is similar to your query beyond just the exact term matches BM25 already counted — see the vector-retrieval scope notes for what this does and doesn't mean (it's lexical similarity, not synonym/meaning understanding).".to_string(),
            );
        }

        reasons
    }

    /// A single-line summary listing every non-baseline reason, e.g.
    /// `"Exact phrase match (+50%), Authority (+12%), Trusted domain (+15%)"`.
    /// Returns `"Matched your search terms"` if no boost signal applied
    /// beyond plain keyword relevance.
    pub fn summary_line(&self) -> String {
        let boosts: Vec<String> = self
            .reasons()
            .iter()
            .skip(1) // skip the baseline BM25 entry
            .map(|r| format!("{} ({:+.0}%)", r.label, r.impact_percent))
            .collect();
        if boosts.is_empty() {
            "Matched your search terms".to_string()
        } else {
            boosts.join(", ")
        }
    }
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

    // Safety hard-filter: a page flagged at crawl time with confidence at
    // or above the configured threshold is excluded from results outright
    // rather than merely down-ranked — see `RankingConfig::safety_block_threshold`.
    if let Some(meta) = web_meta {
        if let Some(flag) = &meta.policy_flag {
            if flag.confidence >= config.safety_block_threshold {
                debug!(
                    "score doc_id={}: excluded, safety flag '{}' confidence={:.2} >= threshold={:.2}",
                    doc_id, flag.category, flag.confidence, config.safety_block_threshold
                );
                return None;
            }
        }
    }

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

    // Per-page spam penalty from `classify::spam::SpamClassifier`, distinct
    // from `domain_quality_boost`'s manual per-domain list: this applies
    // automatically to every crawled page based on its own content, not
    // just pages on a domain someone already flagged.
    let spam_score_boost = match web_meta {
        Some(meta) => (1.0 - config.spam_score_weight * meta.spam_score).max(0.05),
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
        * spam_score_boost
        * click_boost;

    debug!(
        "score doc_id={}: bm25={:.4} filename={:.4} exact={:.4} recency={:.4} pagerank={:.4} url={:.4} domain={:.4} spam={:.4} click={:.4} final={:.4}",
        doc_id, bm25_total, filename_boost, exact_match_boost, recency_boost,
        pagerank_boost, url_match_boost, domain_quality_boost, spam_score_boost, click_boost, final_score
    );

    Some(ScoreExplanation {
        bm25_score: bm25_total,
        filename_boost,
        exact_match_boost,
        recency_boost,
        pagerank_boost,
        url_match_boost,
        domain_quality_boost,
        spam_score_boost,
        click_boost,
        vector_boost: 1.0,
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
            acl: crate::entity::Acl::public(),
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
            redirect_chain: Vec::new(),
                simhash: 0,
                depth: 0,
                outgoing: Vec::new(),
                incoming: Vec::new(),
                pagerank: 0.5,
                spam_score: 0.0,
                policy_flag: None,
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
        let no_clicks =
            score_document(&index, doc_id, &match_info, &config, 0, Some(&clicks)).unwrap();
        clicks.record(doc_id);
        clicks.record(doc_id);
        clicks.record(doc_id);
        let with_clicks =
            score_document(&index, doc_id, &match_info, &config, 0, Some(&clicks)).unwrap();
        assert!(with_clicks.final_score > no_clicks.final_score);
    }

    #[test]
    fn reasons_always_includes_baseline_bm25() {
        let explanation = ScoreExplanation {
            bm25_score: 3.5,
            filename_boost: 1.0,
            exact_match_boost: 1.0,
            recency_boost: 1.0,
            pagerank_boost: 1.0,
            url_match_boost: 1.0,
            domain_quality_boost: 1.0,
            spam_score_boost: 1.0,
            click_boost: 1.0,
            vector_boost: 1.0,
            final_score: 3.5,
        };
        let reasons = explanation.reasons();
        assert_eq!(reasons.len(), 1);
        assert_eq!(reasons[0].label, "Keyword relevance (BM25)");
        assert_eq!(reasons[0].impact_percent, 0.0);
        assert_eq!(explanation.summary_line(), "Matched your search terms");
    }

    #[test]
    fn reasons_only_lists_non_neutral_signals() {
        let explanation = ScoreExplanation {
            bm25_score: 3.5,
            filename_boost: 2.0,   // +100%
            exact_match_boost: 1.0, // neutral, excluded
            recency_boost: 1.0,
            pagerank_boost: 1.5,   // +50%
            url_match_boost: 1.0,
            domain_quality_boost: 1.0,
            spam_score_boost: 1.0,
            click_boost: 1.0,
            vector_boost: 1.0,
            final_score: 10.5,
        };
        let reasons = explanation.reasons();
        // baseline + filename + pagerank = 3
        assert_eq!(reasons.len(), 3);
        assert!(reasons.iter().any(|r| r.label == "Title / filename match" && (r.impact_percent - 100.0).abs() < 0.01));
        assert!(reasons.iter().any(|r| r.label == "Authority (PageRank)" && (r.impact_percent - 50.0).abs() < 0.01));
        assert!(!reasons.iter().any(|r| r.label == "Exact phrase match"));
    }

    #[test]
    fn low_quality_domain_boost_is_labeled_as_a_penalty() {
        let explanation = ScoreExplanation {
            bm25_score: 3.5,
            filename_boost: 1.0,
            exact_match_boost: 1.0,
            recency_boost: 1.0,
            pagerank_boost: 1.0,
            url_match_boost: 1.0,
            domain_quality_boost: 0.5, // -50%
            spam_score_boost: 1.0,
            click_boost: 1.0,
            vector_boost: 1.0,
            final_score: 1.75,
        };
        let reasons = explanation.reasons();
        let domain_reason = reasons.iter().find(|r| r.label == "Low-quality domain").unwrap();
        assert!((domain_reason.impact_percent - (-50.0)).abs() < 0.01);
    }

    #[test]
    fn summary_line_joins_multiple_boosts_readably() {
        let explanation = ScoreExplanation {
            bm25_score: 3.5,
            filename_boost: 1.5,
            exact_match_boost: 1.0,
            recency_boost: 1.0,
            pagerank_boost: 1.2,
            url_match_boost: 1.0,
            domain_quality_boost: 1.0,
            spam_score_boost: 1.0,
            click_boost: 1.0,
            vector_boost: 1.0,
            final_score: 6.3,
        };
        let summary = explanation.summary_line();
        assert!(summary.contains("Title / filename match (+50%)"));
        assert!(summary.contains("Authority (PageRank) (+20%)"));
    }
}
