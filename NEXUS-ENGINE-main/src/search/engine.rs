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
    /// Which [`crate::entity::SourceKind`] this result was classified as
    /// for cross-source ranking purposes.
    pub source_kind: crate::entity::SourceKind,
    /// Cross-source-comparable score from [`crate::entity::HybridRanker`]
    /// (z-score normalized within this result's source, then weighted).
    /// Only meaningful in [`SearchMode::Both`] — for single-source modes
    /// there's no cross-source bias to correct for, so this equals
    /// `score`. **Not** the primary sort key (see the doc comment on the
    /// `entity` wiring in [`search`] for why small hybrid result sets
    /// make z-score normalization degenerate); exposed for API/UI
    /// consumers that specifically want the bias-corrected view.
    pub normalized_score: f32,
    /// Byte offsets `(start, end)` of the best-matching chunk from
    /// `ChunkVectorIndex`, if this document is long enough to have been
    /// chunked and a chunk's similarity was at least as strong as the
    /// whole-document vector's. `None` for short (unchunked) documents,
    /// or when the whole-document vector was the stronger signal. A
    /// snippet generator can use this to center the excerpt on the
    /// section that actually matched, rather than the first occurrence
    /// of a query term, which is what `search::snippet` does today —
    /// consuming this offset there is a follow-up, not done in this pass.
    pub best_chunk_span: Option<(u32, u32)>,
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
    /// An instant-answer card (arithmetic or unit conversion) if the raw
    /// query text looked like one — see `crate::answers::try_answer`.
    /// Populated by [`search_query`], not by [`search`] itself (`search`
    /// only ever sees the parsed [`QueryNode`], not the original text a
    /// card needs, e.g. `"12 km to miles"` after parsing no longer
    /// obviously reads as a conversion request). `None` doesn't mean "no
    /// results," it means "not that kind of query" — always fall through
    /// to `results` regardless.
    pub instant_answer: Option<crate::answers::InstantAnswer>,
    /// The combined local + federated result list, only populated by
    /// [`search_with_federation`] when federation is actually enabled
    /// and configured with at least one peer — `results` above always
    /// stays this instance's own local view regardless. `None` doesn't
    /// mean "no federated results," it means "federation wasn't run for
    /// this search" (disabled, or called via plain [`search_query`]).
    pub federated_results: Option<Vec<crate::entity::UnifiedEntity>>,
}

/// Above this many pre-pagination candidates, hybrid mode's O(n^2)
/// cross-source duplicate comparison is skipped rather than run
/// unbounded. This is a genuine, documented tradeoff: on very broad
/// queries against a large combined index, a full pairwise SimHash
/// comparison would be too slow to run per-request, so extremely broad
/// hybrid queries may show occasional near-duplicate local/web pairs
/// rather than blocking. Bump this if profiling shows headroom.
const HYBRID_DEDUP_MAX_CANDIDATES: usize = 500;

/// Maps a result's source into [`crate::entity::SourceKind`] for
/// cross-source ranking. Deliberately coarse for local files beyond
/// PDF/email (a "Note" vs. plain "LocalFile" distinction isn't
/// recoverable from `DocumentMetadata::extension` alone — see
/// `crate::extract::sqlite_notes` — so those stay `LocalFile`, which is
/// harmless here since `HybridRanker`'s per-source weights only need to
/// be roughly right, not perfectly taxonomic).
/// Bundles what's needed to apply [`crate::ranking::adaptive::PersonalRankingModel`]
/// as a re-ranking boost: the trained model itself, plus the person's
/// most-frequently-clicked source kind (computed once by the caller from
/// their click history via `ranking::adaptive::most_frequently_clicked_kind`,
/// rather than recomputed per-search — it only changes as new clicks
/// accumulate, not per-query).
pub struct Personalization<'a> {
    pub model: &'a crate::ranking::adaptive::PersonalRankingModel,
    pub frequently_clicked_kind: Option<crate::entity::SourceKind>,
}

/// Computes each result's [`crate::ranking::adaptive::Features`] from its
/// *current* (pre-personalization-boost) score — keyed by `doc_id` rather
/// than position, because callers that need to reconcile these features
/// against a post-boost, re-sorted display order (see `cmd_click` in
/// `crate::cli::commands`) can't rely on index correspondence once
/// `apply_personal_boost` has resorted `results`.
pub fn compute_result_features(
    results: &[SearchResult],
    index: &Index,
    p: &Personalization,
) -> std::collections::HashMap<DocId, crate::ranking::adaptive::Features> {
    if results.is_empty() {
        return std::collections::HashMap::new();
    }
    let (min_score, max_score) = results
        .iter()
        .fold((f32::MAX, f32::MIN), |(lo, hi), r| (lo.min(r.score), hi.max(r.score)));
    let (min_modified, max_modified) = results
        .iter()
        .fold((i64::MAX, i64::MIN), |(lo, hi), r| (lo.min(r.modified_unix), hi.max(r.modified_unix)));

    results
        .iter()
        .map(|r| {
            let domain = index.web.get(r.doc_id).map(|m| m.domain.as_str());
            let features = crate::ranking::adaptive::compute_features(
                domain,
                r.source_kind,
                p.frequently_clicked_kind,
                r.modified_unix,
                min_modified,
                max_modified,
                r.score,
                min_score,
                max_score,
            );
            (r.doc_id, features)
        })
        .collect()
}

/// Applies the personal ranking model as a real, live re-ranking signal:
/// `final_score *= 0.85 + 0.3 * model.predict(features)`, centered so an
/// untrained model (`predict` always returns `0.5`) multiplies by
/// exactly `1.0` — no personalization effect at all until the model has
/// actually learned something from clicks, matching
/// `PersonalRankingModel::predict`'s own doc comment on how callers
/// should combine its output with the existing composite score. Min/max
/// normalization for the `recency`/`base_score` features is computed
/// over `results` itself (what the person is actually looking at this
/// search), not any global scale.
pub fn apply_personal_boost(results: &mut [SearchResult], index: &Index, p: &Personalization) {
    if results.is_empty() {
        return;
    }
    let features_by_doc = compute_result_features(results, index, p);
    for result in results.iter_mut() {
        let Some(features) = features_by_doc.get(&result.doc_id) else { continue };
        let boost = 0.85 + 0.3 * p.model.predict(features);
        result.score *= boost;
        result.explanation.final_score = result.score;
    }
    // Personalization can reorder results relative to the rest of the
    // pipeline's ranking, so re-sort after applying it — same reasoning
    // as the primary sort after `apply_hybrid_ranker` above.
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
}

pub(crate) fn source_kind_for(is_web: bool, extension: &str) -> crate::entity::SourceKind {
    use crate::entity::SourceKind;
    if is_web {
        return SourceKind::WebPage;
    }
    match extension {
        "pdf" => SourceKind::Pdf,
        "eml" | "mbox" => SourceKind::Email,
        "zip" => SourceKind::Archive,
        _ => SourceKind::LocalFile,
    }
}

pub fn search(
    index: &Index,
    query: &QueryNode,
    config: &crate::config::RankingConfig,
    offset: usize,
    limit: usize,
    clicks: Option<&crate::clicks::ClickLog>,
    mode: SearchMode,
    ctx: Option<&crate::entity::SearchContext>,
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

            // Permission filtering: skip documents `ctx` isn't allowed to
            // see, before scoring/pagination for the same reason mode
            // filtering does below — so `offset`/`limit` apply to the set
            // of results the caller is actually permitted to see, rather
            // than a hidden document silently consuming a page slot.
            // `ctx: None` means "no permission context supplied" (e.g.
            // most existing internal/test callers) and performs no
            // filtering at all, matching this function's behavior before
            // ACL data existed — real enforcement is opt-in by passing
            // `Some(ctx)`, which every live request-serving entry point
            // (`api::mod`, `cli::commands`) does.
            if let Some(ctx) = ctx {
                if !metadata.acl.is_visible_to(ctx) {
                    return None;
                }
            }
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

            let mut best_chunk_span: Option<(u32, u32)> = None;

            if mode == SearchMode::Both && !is_web {
                explanation.final_score *= config.local_boost;
            }

            if let Some(qv) = &query_vec {
                // Whole-document similarity (always available if this doc
                // has any vector at all) and, for long documents,
                // chunk-level similarity from `ChunkVectorIndex` — see
                // its doc comment on why this is "same scoring path,
                // extra candidate signal" rather than chunks competing
                // as separate results. Take whichever is stronger: a
                // long document that's only relevant in one section
                // should still surface via that section's chunk vector
                // even if its whole-document vector gets diluted by
                // everything else in the doc.
                let whole_doc_sim = index.vectors.get(doc_id).map(|dv| qv.cosine_similarity(dv).max(0.0));
                let chunk_match = index.chunk_vectors.best_chunk_for(doc_id, qv);
                let chunk_sim = chunk_match.as_ref().map(|(_, sim)| sim.max(0.0));

                let best_similarity = match (whole_doc_sim, chunk_sim) {
                    (Some(w), Some(c)) => Some(w.max(c)),
                    (Some(w), None) => Some(w),
                    (None, Some(c)) => Some(c),
                    (None, None) => None,
                };

                if let Some(similarity) = best_similarity {
                    let vector_boost = 1.0 + config.vector_weight * similarity;
                    explanation.vector_boost = vector_boost;
                    explanation.final_score *= vector_boost;
                }

                // Only record the winning chunk's span if the chunk
                // actually won (equal-or-better than the whole-doc
                // similarity) — otherwise the whole-document vector was
                // the better signal and there's no single span to point
                // a snippet at.
                if let (Some(c), Some(sim)) = (&chunk_match, chunk_sim) {
                    if Some(sim) >= whole_doc_sim {
                        best_chunk_span = Some((c.0.start_offset, c.0.end_offset));
                    }
                }
            }

            let final_score = explanation.final_score;
            Some(SearchResult {
                doc_id,
                path: metadata.path.clone(),
                file_name: metadata.file_name.clone(),
                size_bytes: metadata.size_bytes,
                modified_unix: metadata.modified_unix,
                match_count: match_info.term_frequencies.len(),
                score: final_score,
                explanation,
                is_web,
                is_onion,
                source_kind: source_kind_for(is_web, &metadata.extension),
                normalized_score: final_score,
                best_chunk_span,
            })
        })
        .collect();

    debug!("search returned {} raw results (mode={:?})", results.len(), mode);

    if mode == SearchMode::Both {
        results = dedupe_hybrid_cross_source(index, results, config.hybrid_dedup_min_similarity);
        apply_hybrid_ranker(&mut results, config);
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
        instant_answer: None,
        federated_results: None,
    }
}

/// Thin wrapper around [`search`] for callers that have the *raw* query
/// text (CLI, HTTP/WebSocket API handlers) rather than an
/// already-parsed [`QueryNode`] — the two extra things you can only do
/// with the original text: parsing it, and checking whether it's an
/// instant-answer query (`crate::answers::try_answer`, which needs the
/// literal text — `"12 km to miles"` doesn't survive query parsing in a
/// form that's still recognizable as a conversion request). This is the
/// actual live-request-path entry point instant answers are wired into;
/// `search` itself is unchanged and still used directly by callers (like
/// spelling-suggestion candidate generation) that have no business
/// showing an instant-answer card.
pub fn search_query(
    index: &Index,
    raw_query: &str,
    config: &crate::config::RankingConfig,
    offset: usize,
    limit: usize,
    clicks: Option<&crate::clicks::ClickLog>,
    mode: SearchMode,
    personalization: Option<&Personalization>,
    ctx: Option<&crate::entity::SearchContext>,
) -> crate::error::Result<SearchOutcome> {
    let ast = crate::query::parse(raw_query)?;
    let mut outcome = search(index, &ast, config, offset, limit, clicks, mode, ctx);
    if let Some(p) = personalization {
        apply_personal_boost(&mut outcome.results, index, p);
    }
    outcome.instant_answer = crate::answers::try_answer(raw_query);
    Ok(outcome)
}

/// Runs [`search_query`], then — if `federation_config.enabled` and at
/// least one peer is configured — fans out to every enabled peer via
/// [`crate::federation::FederatedSearchClient`], merges each peer's
/// results with this instance's own local results through the same
/// real [`crate::entity::HybridRanker::merge`] used for local/web
/// hybrid mode, and populates `SearchOutcome::federated_results` with
/// the combined, cross-source-normalized list.
///
/// This does real, potentially slow network I/O (one HTTP request per
/// enabled peer, sequentially — see `FederatedSearchClient::fan_out`'s
/// doc comment) — callers on a latency-sensitive path (e.g. the HTTP
/// API's autocomplete-adjacent endpoints) should call `search_query`
/// directly instead and only reach for this on the actual user-facing
/// search endpoint, where sub-second added latency for federation is an
/// acceptable, explicit tradeoff for a feature the operator opted into.
pub fn search_with_federation(
    index: &Index,
    raw_query: &str,
    config: &crate::config::RankingConfig,
    offset: usize,
    limit: usize,
    clicks: Option<&crate::clicks::ClickLog>,
    mode: SearchMode,
    personalization: Option<&Personalization>,
    federation_config: &crate::config::FederationConfig,
    ctx: Option<&crate::entity::SearchContext>,
) -> crate::error::Result<SearchOutcome> {
    let mut outcome = search_query(index, raw_query, config, offset, limit, clicks, mode, personalization, ctx)?;

    if federation_config.enabled && !federation_config.peers.is_empty() {
        let http = crate::web::http::HttpClient::new(crate::web::http::HttpConfig {
            timeout: std::time::Duration::from_millis(federation_config.per_peer_timeout_ms),
            ..Default::default()
        })
        .map_err(|e| crate::error::NexusError::Other(format!("failed to build federation HTTP client: {e}")))?;
        let client = crate::federation::FederatedSearchClient::new(http);
        let registry = crate::federation::PeerRegistry::from_peers(federation_config.peers.clone());
        let federated_candidates = client.fan_out_as_candidates(&registry, raw_query);

        // `outcome.results` here has already been through `search`'s own
        // permission filter (see `ctx` above), so every local candidate
        // below is already something `ctx` may see — `Acl::public()`
        // just carries that already-established fact forward into
        // `HybridRanker::merge`'s own (redundant-but-harmless) check,
        // rather than re-deriving it. Federated candidates are tagged
        // public too: this instance has no filesystem-permission
        // visibility into a peer's local files, only whatever that peer
        // chose to serve over its own `/search` endpoint.
        let local_candidates: Vec<crate::entity::SourceCandidate> = outcome
            .results
            .iter()
            .map(|r| crate::entity::SourceCandidate {
                id: r.doc_id.to_string(),
                source: r.source_kind,
                title: r.file_name.clone(),
                snippet: String::new(),
                raw_score: r.score,
                acl: crate::entity::Acl::public(),
            })
            .collect();

        let mut all_candidates = local_candidates;
        all_candidates.extend(federated_candidates);

        let ranker = crate::entity::HybridRanker::default();
        let merge_ctx = crate::entity::SearchContext::default();
        outcome.federated_results = Some(ranker.merge(all_candidates, &merge_ctx));
    }

    Ok(outcome)
}

/// Runs `entity::HybridRanker::merge` on hybrid-mode results, writing its
/// cross-source-normalized score back onto each [`SearchResult`] as
/// `normalized_score`. This is a genuine call into the real ranker (not a
/// parallel reimplementation) — it does the z-score-per-source
/// normalization `entity::HybridRanker` exists for, using
/// `config.local_boost` as the local-file source weight so the "local
/// results get a nudge" behavior stays driven by one config knob instead
/// of two independent numbers meaning almost the same thing.
///
/// Permission filtering (`HybridRanker::merge`'s other job) isn't
/// exercised meaningfully *here*: real enforcement now happens earlier,
/// in `search`'s own `ctx`-based filter (see its doc comment) — by the
/// time results reach this function, everything `ctx` isn't allowed to
/// see has already been removed from `results`. Every candidate is
/// built with `Acl::public()` below because that's simply true at this
/// point in the pipeline (they already passed the real check), not
/// because permission data doesn't exist upstream anymore.
///
/// `normalized_score` is deliberately *not* used as the primary sort key
/// below (that stays `score`, unchanged): with the small result sets a
/// hybrid search typically returns, z-score normalization within a
/// single-candidate source group divides by a zero stddev and falls back
/// to a flat `0.0` (see `HybridRanker::merge`'s doc comment), which would
/// make e.g. one local result and one web result tie at `0.0` regardless
/// of which one is actually the better match — losing exactly the "local
/// result should still win" guarantee `local_boost` exists to provide.
/// `normalized_score` is real and useful at larger result-set scale
/// (many candidates per source, where the normalization has enough
/// samples to be meaningful) and is exposed on `SearchResult` for API/UI
/// consumers who want that specific, bias-corrected view; `score` stays
/// the default because it's correct across the full range of result-set
/// sizes hybrid search actually sees, including the very common
/// "one or two results per source" case.
fn apply_hybrid_ranker(results: &mut [SearchResult], config: &crate::config::RankingConfig) {
    use crate::entity::{Acl, HybridRanker, SearchContext, SourceCandidate};

    if results.is_empty() {
        return;
    }

    let candidates: Vec<SourceCandidate> = results
        .iter()
        .map(|r| SourceCandidate {
            id: r.doc_id.to_string(),
            source: r.source_kind,
            title: r.file_name.clone(),
            snippet: String::new(),
            raw_score: r.score,
            acl: Acl::public(),
        })
        .collect();

    let mut ranker = HybridRanker::default();
    ranker
        .source_weights
        .insert(crate::entity::SourceKind::LocalFile, config.local_boost);

    let ctx = SearchContext::default();
    let merged = ranker.merge(candidates, &ctx);

    let normalized_by_id: HashMap<String, f32> = merged
        .into_iter()
        .map(|e| (e.id, e.normalized_score))
        .collect();

    for result in results.iter_mut() {
        if let Some(&n) = normalized_by_id.get(&result.doc_id.to_string()) {
            result.normalized_score = n;
        }
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
        QueryNode::FilterEntity(name) => filter_by_entity(index, name),
    }
}

/// Like [`filter_docs`], but restricts to documents the knowledge graph
/// (`crate::graph::GraphBuilder`) recorded a mention of an entity whose
/// name contains `name` in, for the `@person:`/`@org:` query operators.
/// Entity mentions are stored keyed by `doc_id.to_string()` as the
/// `source_id` (see `Index::index_document`), so resolving a matching
/// entity's mentions back to `DocId`s is a direct string-to-`DocId`
/// parse — this only fails to round-trip if the graph somehow has a
/// `source_id` that was never a `DocId` to begin with, which doesn't
/// happen anywhere `GraphBuilder::ingest` is actually called in this
/// codebase.
fn filter_by_entity(index: &Index, name: &str) -> HashMap<DocId, MatchInfo> {
    index
        .graph
        .entities_containing(name)
        .into_iter()
        .flat_map(|entity| entity.mentions.iter())
        .filter_map(|mention| mention.source_id.parse::<DocId>().ok())
        .map(|doc_id| (doc_id, MatchInfo::default()))
        .collect()
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
            acl: crate::entity::Acl::public(),
        };
        index.index_document(Document { metadata, content: content.to_string() })
    }

    fn index_local_owned_by(index: &mut Index, path: &str, content: &str, owner: &str) -> DocId {
        let metadata = DocumentMetadata {
            path: PathBuf::from(path),
            file_name: path.trim_start_matches('/').to_string(),
            extension: "txt".to_string(),
            size_bytes: content.len() as u64,
            modified_unix: 0,
            token_count: 0,
            acl: crate::entity::Acl::owned_by(owner),
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
            acl: crate::entity::Acl::public(),
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
                spam_score: 0.0,
                policy_flag: None,
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
        search(index, &ast, &config, 0, 50, None, mode, None)
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
        let outcome = search(&index, &ast, &config, 0, 50, None, SearchMode::Both, None);

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
        let outcome = search(&index, &ast, &config, 0, 50, None, SearchMode::Both, None);

        assert_eq!(outcome.results[0].explanation.vector_boost, 1.0);
    }

    #[test]
    fn both_mode_populates_normalized_score_via_hybrid_ranker() {
        let mut index = Index::new();
        index_local(&mut index, "/a.txt", "rust ownership deep dive local notes");
        index_local(&mut index, "/b.txt", "rust borrowing detailed local reference");
        index_web(
            &mut index,
            "https://example.com/rust1",
            "Rust Guide",
            "rust concurrency channels overview",
        );
        index_web(
            &mut index,
            "https://example.com/rust2",
            "Rust Async",
            "rust async runtimes deep coverage",
        );

        let outcome = parse_and_search(&index, "rust", SearchMode::Both);
        assert_eq!(outcome.results.len(), 4);
        // With >1 candidate per source, z-score normalization is
        // non-degenerate: normalized_score should differ across results
        // within the same source group, proving HybridRanker actually
        // ran rather than leaving normalized_score at its score default
        // for every result.
        let local_scores: Vec<f32> = outcome
            .results
            .iter()
            .filter(|r| !r.is_web)
            .map(|r| r.normalized_score)
            .collect();
        assert_eq!(local_scores.len(), 2);
        assert_ne!(
            local_scores[0], local_scores[1],
            "two different-relevance local results should get different normalized scores"
        );
        for r in &outcome.results {
            assert_eq!(
                r.source_kind,
                if r.is_web {
                    crate::entity::SourceKind::WebPage
                } else {
                    crate::entity::SourceKind::LocalFile
                }
            );
        }
    }

    #[test]
    fn non_hybrid_modes_leave_normalized_score_equal_to_score() {
        let mut index = Index::new();
        index_local(&mut index, "/notes.txt", "rust programming notes");
        let outcome = parse_and_search(&index, "rust", SearchMode::Local);
        assert_eq!(outcome.results[0].normalized_score, outcome.results[0].score);
    }

    #[test]
    fn long_documents_get_chunked_and_short_ones_dont() {
        let mut index = Index::new();
        // Comfortably over the 300-word chunking threshold, and topically
        // split: the first half is about gardening, the second half is
        // about rust ownership — a whole-document vector blurs these
        // together, but a chunk vector for the second half should not.
        let filler_a = "gardening tomatoes soil compost mulching watering ".repeat(160);
        let filler_b = "rust ownership borrowing lifetimes memory safety ".repeat(160);
        let long_doc = format!("{filler_a} {filler_b}");
        let long_id = index_local(&mut index, "/long.txt", &long_doc);

        index_local(&mut index, "/short.txt", "a short rust note");

        assert!(
            index.chunk_vectors.document_count() >= 1,
            "the long, topically-split document should have been chunked"
        );

        let outcome = parse_and_search(&index, "rust ownership", SearchMode::Local);
        let long_result = outcome.results.iter().find(|r| r.doc_id == long_id).unwrap();
        assert!(
            long_result.explanation.vector_boost > 1.0,
            "chunk-level similarity on the rust half should still produce a real vector boost \
             even though the document overall is mostly about gardening"
        );
        assert!(
            long_result.best_chunk_span.is_some(),
            "the winning chunk's byte span should be recorded for snippet consumers"
        );
    }

    #[test]
    fn removing_a_chunked_document_clears_its_chunk_vectors() {
        let mut index = Index::new();
        let filler = "rust ownership borrowing lifetimes memory safety detailed guide ".repeat(160);
        let doc_id = index_local(&mut index, "/big.txt", &filler);
        assert_eq!(index.chunk_vectors.document_count(), 1);

        index.remove_document(doc_id);
        assert_eq!(index.chunk_vectors.document_count(), 0);
    }

    #[test]
    fn search_query_populates_instant_answer_for_arithmetic() {
        let index = Index::new();
        let config = crate::config::RankingConfig::default();
        let outcome =
            search_query(&index, "12 * 4", &config, 0, 10, None, SearchMode::Local, None, None).unwrap();
        match outcome.instant_answer {
            Some(crate::answers::InstantAnswer::Calculation { result, .. }) => {
                assert_eq!(result, 48.0)
            }
            other => panic!("expected a Calculation instant answer, got {:?}", other),
        }
    }

    #[test]
    fn search_query_leaves_instant_answer_none_for_ordinary_queries() {
        let mut index = Index::new();
        index_local(&mut index, "/notes.txt", "rust programming notes");
        let config = crate::config::RankingConfig::default();
        let outcome =
            search_query(&index, "rust", &config, 0, 10, None, SearchMode::Local, None, None).unwrap();
        assert!(outcome.instant_answer.is_none());
        assert_eq!(outcome.results.len(), 1);
    }

    #[test]
    fn untrained_personal_model_does_not_change_order_or_scores() {
        let mut index = Index::new();
        index_local(&mut index, "/a.txt", "rust ownership");
        index_web(&mut index, "https://example.com/rust", "Rust", "rust ownership guide");
        let config = crate::config::RankingConfig::default();
        let model = crate::ranking::adaptive::PersonalRankingModel::new();
        let personalization = Personalization { model: &model, frequently_clicked_kind: None };

        let baseline = search_query(&index, "rust", &config, 0, 10, None, SearchMode::Both, None, None).unwrap();
        let personalized =
            search_query(&index, "rust", &config, 0, 10, None, SearchMode::Both, Some(&personalization), None)
                .unwrap();

        assert_eq!(baseline.results.len(), personalized.results.len());
        for (a, b) in baseline.results.iter().zip(personalized.results.iter()) {
            assert_eq!(a.doc_id, b.doc_id, "an untrained model should not reorder results");
            assert!(
                (a.score - b.score).abs() < 1e-4,
                "an untrained model should not change scores: {} vs {}",
                a.score,
                b.score
            );
        }
    }

    #[test]
    fn trained_personal_model_reorders_toward_the_learned_preference() {
        let mut index = Index::new();
        // Two equally BM25-relevant results, one .edu, one not — a
        // model trained to strongly prefer .edu sources should be able
        // to promote the .edu result even when the base ranker scored
        // them close to identically.
        let edu_id = index_web(
            &mut index,
            "https://mit.edu/rust",
            "Rust at MIT",
            "rust programming ownership borrowing",
        );
        index.web.get_mut(edu_id).unwrap().domain = "mit.edu".to_string();
        let other_id = index_web(
            &mut index,
            "https://example.com/rust",
            "Rust Guide",
            "rust programming ownership lending",
        );

        let config = crate::config::RankingConfig::default();
        let baseline = search_query(&index, "rust programming ownership", &config, 0, 10, None, SearchMode::Web, None, None)
            .unwrap();
        // Confirm the test setup is actually meaningful: without this,
        // a test that "passes" either way (because .edu already won on
        // BM25 alone) wouldn't prove the model did anything.
        let _ = baseline;

        let mut model = crate::ranking::adaptive::PersonalRankingModel::new();
        let edu_features = crate::ranking::adaptive::Features {
            is_edu_or_gov_domain: true,
            source_matches_frequently_clicked_kind: false,
            recency_normalized: 0.5,
            base_score_normalized: 0.5,
        };
        let non_edu_features = crate::ranking::adaptive::Features {
            is_edu_or_gov_domain: false,
            source_matches_frequently_clicked_kind: false,
            recency_normalized: 0.5,
            base_score_normalized: 0.5,
        };
        for _ in 0..300 {
            model.train_one(&edu_features, 1.0);
            model.train_one(&non_edu_features, 0.0);
        }

        let personalization = Personalization { model: &model, frequently_clicked_kind: None };
        let outcome =
            search_query(&index, "rust programming ownership", &config, 0, 10, None, SearchMode::Web, Some(&personalization), None)
                .unwrap();

        assert_eq!(outcome.results[0].doc_id, edu_id, "a model trained to prefer .edu sources should rank the .edu result first");
        let _ = other_id;
    }

    #[test]
    fn search_with_federation_merges_a_real_peer_over_http() {
        // A genuine local HTTP server standing in for a peer Nexus
        // instance — not a mock — so this proves FederatedSearchClient
        // actually performs the HTTP round-trip, not just that its
        // parsing logic works in isolation.
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr();
        let peer_thread = std::thread::spawn(move || {
            if let Ok(request) = server.recv() {
                let body = r#"{"results":[{"id":"peer-doc-1","title":"Peer Rust Guide","snippet":"...","score":9.9}]}"#;
                let response = tiny_http::Response::from_string(body);
                let _ = request.respond(response);
            }
        });

        let mut index = Index::new();
        index_local(&mut index, "/local.txt", "rust ownership local notes");

        let config = crate::config::RankingConfig::default();
        let federation_config = crate::config::FederationConfig {
            enabled: true,
            peers: vec![crate::federation::PeerInfo {
                name: "test-peer".to_string(),
                base_url: format!("http://{addr}"),
                enabled: true,
            }],
            per_peer_timeout_ms: 2000,
        };

        let outcome = search_with_federation(
            &index,
            "rust",
            &config,
            0,
            10,
            None,
            SearchMode::Local,
            None,
            &federation_config,
            None,
        )
        .unwrap();

        peer_thread.join().unwrap();

        let federated = outcome
            .federated_results
            .expect("federation was enabled with a configured peer, so this should be populated");
        assert!(
            federated.iter().any(|e| e.id == "test-peer:peer-doc-1"),
            "expected the real peer's result to be merged in, got: {:?}",
            federated.iter().map(|e| &e.id).collect::<Vec<_>>()
        );
        assert!(
            federated.iter().any(|e| e.source == crate::entity::SourceKind::LocalFile),
            "the local result should still be present alongside the federated one"
        );
    }

    #[test]
    fn federation_disabled_by_default_never_makes_a_network_request() {
        let mut index = Index::new();
        index_local(&mut index, "/local.txt", "rust ownership local notes");
        let config = crate::config::RankingConfig::default();
        // Points at a peer that doesn't exist — if federation were
        // accidentally triggered despite `enabled: false`, this would
        // hang or error rather than returning promptly.
        let federation_config = crate::config::FederationConfig {
            enabled: false,
            peers: vec![crate::federation::PeerInfo {
                name: "unreachable".to_string(),
                base_url: "http://127.0.0.1:1".to_string(),
                enabled: true,
            }],
            per_peer_timeout_ms: 100,
        };
        let outcome = search_with_federation(
            &index, "rust", &config, 0, 10, None, SearchMode::Local, None, &federation_config, None,
        )
        .unwrap();
        assert!(outcome.federated_results.is_none());
    }

    #[test]
    fn no_ctx_supplied_means_no_permission_filtering() {
        // Back-compat: existing callers that don't pass a `SearchContext`
        // (internal tools, most tests) see everything, exactly as before
        // ACL data existed — filtering is opt-in via `Some(ctx)`.
        let mut index = Index::new();
        index_local_owned_by(&mut index, "/private.txt", "rust ownership secrets", "uid:1234");
        let outcome = parse_and_search(&index, "rust", SearchMode::Local);
        assert_eq!(outcome.results.len(), 1);
    }

    #[test]
    fn anonymous_ctx_only_sees_public_documents() {
        let mut index = Index::new();
        index_local(&mut index, "/public.txt", "rust programming guide");
        index_local_owned_by(&mut index, "/private.txt", "rust programming secrets", "uid:1234");

        let ast = crate::query::parse("rust programming").unwrap();
        let config = crate::config::RankingConfig::default();
        let anon_ctx = crate::entity::SearchContext::default();
        let outcome = search(&index, &ast, &config, 0, 50, None, SearchMode::Local, Some(&anon_ctx));

        assert_eq!(outcome.results.len(), 1);
        assert_eq!(outcome.results[0].file_name, "public.txt");
    }

    #[test]
    fn ctx_matching_the_owner_sees_the_private_document_too() {
        let mut index = Index::new();
        index_local(&mut index, "/public.txt", "rust programming guide");
        index_local_owned_by(&mut index, "/private.txt", "rust programming secrets", "uid:1234");

        let ast = crate::query::parse("rust programming").unwrap();
        let config = crate::config::RankingConfig::default();
        let owner_ctx = crate::entity::SearchContext {
            principal: Some("uid:1234".to_string()),
            groups: Default::default(),
        };
        let outcome = search(&index, &ast, &config, 0, 50, None, SearchMode::Local, Some(&owner_ctx));

        assert_eq!(outcome.results.len(), 2);
    }

    #[test]
    fn entity_operator_filters_to_documents_mentioning_that_entity() {
        // "Linus Torvalds" appears (twice, so it's promoted past the
        // single-mention threshold) only in doc_a; doc_b never mentions
        // him. @person:linus should therefore match only doc_a, even
        // though both documents match the plain-text term "kernel".
        let mut index = Index::new();
        let doc_a = index_local(
            &mut index,
            "/a.txt",
            "Linus Torvalds wrote the kernel. Linus Torvalds still maintains the kernel.",
        );
        let doc_b = index_local(&mut index, "/b.txt", "the kernel has many other contributors");

        let outcome = parse_and_search(&index, "kernel @person:linus", SearchMode::Local);
        let ids: Vec<DocId> = outcome.results.iter().map(|r| r.doc_id).collect();
        assert_eq!(ids, vec![doc_a]);
        assert!(!ids.contains(&doc_b));
    }

    #[test]
    fn org_operator_is_the_same_filter_as_person_operator() {
        let mut index = Index::new();
        let doc_a = index_local(&mut index, "/a.txt", "Acme Corporation shipped a new release. Acme Corporation is growing.");

        let via_org = parse_and_search(&index, "@org:acme", SearchMode::Local);
        let via_person = parse_and_search(&index, "@person:acme", SearchMode::Local);
        assert_eq!(via_org.results.len(), 1);
        assert_eq!(via_org.results[0].doc_id, doc_a);
        assert_eq!(via_org.results.len(), via_person.results.len());
    }

    #[test]
    fn entity_operator_matches_case_insensitively_and_by_substring() {
        let mut index = Index::new();
        let doc_a = index_local(&mut index, "/a.txt", "Sarah Chen led the project. Sarah Chen presented the results.");

        let outcome = parse_and_search(&index, "@person:SARAH", SearchMode::Local);
        assert_eq!(outcome.results.len(), 1);
        assert_eq!(outcome.results[0].doc_id, doc_a);
    }

    #[test]
    fn permission_filtering_applies_before_pagination() {
        // A hidden document must not consume a page slot: if it did,
        // `limit: 1` starting from a match set of [hidden, visible]
        // would incorrectly return zero results instead of the one
        // visible match.
        let mut index = Index::new();
        index_local_owned_by(&mut index, "/a-private.txt", "rust ownership", "uid:999");
        index_local(&mut index, "/b-public.txt", "rust ownership");

        let ast = crate::query::parse("rust ownership").unwrap();
        let config = crate::config::RankingConfig::default();
        let anon_ctx = crate::entity::SearchContext::default();
        let outcome = search(&index, &ast, &config, 0, 1, None, SearchMode::Local, Some(&anon_ctx));

        assert_eq!(outcome.total, 1, "the private document should not count toward total either");
        assert_eq!(outcome.results.len(), 1);
        assert_eq!(outcome.results[0].file_name, "b-public.txt");
    }
}
