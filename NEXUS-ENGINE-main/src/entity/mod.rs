//! A unified entity model so local files, PDFs, notes, emails, and web
//! pages can be ranked together in one results list without raw-source
//! bias, gated by a permission check before anything is ever returned.
//!
//! ## Why raw scores need normalizing before merging
//! BM25 scores, PageRank-weighted web scores, and a note app's
//! chunk-similarity score all live on different, incomparable numeric
//! scales — a local file's BM25 score of 8.2 is not "worse" than a web
//! page's composite score of 1.4 in any meaningful sense, they were
//! never computed to be compared directly. Concatenating result lists
//! and sorting by raw score would systematically favor whichever source
//! happens to produce larger numbers, which is exactly the "raw-source
//! bias" this module exists to remove. [`normalize_within_source`] fixes
//! this the standard way: z-score normalize each source's scores against
//! *that source's own* mean and standard deviation before combining, so
//! "how unusually good is this result, relative to other results from
//! the same kind of source" is what's being compared, not absolute
//! magnitude.
//!
//! ## Permission model
//! This is intentionally simple — an allow-list of principals (user or
//! group IDs) per entity, plus an `owner` and a `public` escape hatch —
//! because a search engine's permission layer should almost never be the
//! source of truth for access control; it should mirror whatever
//! authorization already exists in the underlying system (filesystem
//! ACLs, a mail server's mailbox ownership, a wiki's page permissions)
//! and deny by default when that mirroring is incomplete. See
//! [`Acl::is_visible_to`] for the exact (conservative) matching rule.
//!
//! ## Integration seam
//! [`HybridRanker::merge`] takes already-scored candidates from each
//! source; it does not itself call into `crate::search::engine`,
//! `crate::ranking`, or `crate::vector`. Wiring "have `search::engine`
//! produce `UnifiedEntity` candidates instead of its current
//! `SearchResult` list" is the integration step left for whoever adopts
//! this — the merge/normalize/permission-filter logic here is complete
//! and tested independent of that wiring, the same pattern used
//! elsewhere in this pass (see `crate::vector::ChunkVectorIndex`).

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Which underlying source produced an entity, used both for display
/// (so a UI can show a small "PDF" / "Web" / "Note" badge) and as the
/// grouping key for per-source score normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SourceKind {
    LocalFile,
    Pdf,
    Note,
    Email,
    WebPage,
    Archive,
    /// A result merged in from a peer Nexus instance via
    /// `crate::federation` — kept as its own kind (rather than folded
    /// into `WebPage`) so a federated result's score gets normalized
    /// against *other federated results*, not against this instance's
    /// own local/web scores, and so a UI can label it "from [peer]"
    /// distinctly.
    Federated,
}

/// A permission descriptor attached to one entity. Deny-by-default:
/// an entity with an empty `allowed_principals`, no `owner`, and
/// `public = false` is visible to nobody, not to everybody — the
/// opposite of accidentally-open defaults being the safer failure mode
/// for a search index.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Acl {
    pub owner: Option<String>,
    pub allowed_principals: HashSet<String>,
    pub allowed_groups: HashSet<String>,
    pub public: bool,
}

impl Acl {
    /// A fully public entity (e.g. a crawled public web page — the
    /// common case, since crawled pages have no real owner/ACL concept).
    pub fn public() -> Self {
        Acl {
            public: true,
            ..Default::default()
        }
    }

    /// An entity owned by exactly one principal (the common case for
    /// local files and personal notes/emails — indexed under the
    /// identity of whoever ran the indexer).
    pub fn owned_by(principal: impl Into<String>) -> Self {
        Acl {
            owner: Some(principal.into()),
            ..Default::default()
        }
    }

    pub fn is_visible_to(&self, ctx: &SearchContext) -> bool {
        if self.public {
            return true;
        }
        if let Some(owner) = &self.owner {
            if ctx.principal.as_deref() == Some(owner.as_str()) {
                return true;
            }
        }
        if let Some(principal) = &ctx.principal {
            if self.allowed_principals.contains(principal) {
                return true;
            }
        }
        !self.allowed_groups.is_disjoint(&ctx.groups)
    }
}

/// Who is searching, for permission filtering. `principal: None` means
/// an unauthenticated/anonymous search context, which by construction
/// can only see `Acl::public` entities.
#[derive(Debug, Clone, Default)]
pub struct SearchContext {
    pub principal: Option<String>,
    pub groups: HashSet<String>,
}

/// Resolves the identity of the OS user this process is running as, in
/// the same `uid:<n>` shape [`crate::document::acl_for_local_path`] uses
/// when it derives an `Acl::owned_by` from a local file's owner UID —
/// this is the piece that makes that comparison actually match.
///
/// **This is not an authentication mechanism.** It answers "which OS
/// account is this Nexus process running as," not "who is the person on
/// the other end of this HTTP request." That distinction matters: it's
/// the right default for the common case this is a personal,
/// single-machine tool (see the crate README) indexing files that
/// belong to whichever account runs `nexus serve`/`nexus search` — the
/// permission model here exists to *mirror* filesystem ownership so a
/// shared multi-account machine doesn't leak one account's private
/// files to another through search, not to be a network-facing auth
/// boundary. A deployment that exposes `nexus serve` beyond one trusted
/// local machine needs a real authentication layer in front of it
/// (verifying a request's claimed principal against actual login state)
/// before `SearchContext::principal` can be trusted as anything more
/// than "what the caller claims."
pub fn current_os_principal() -> Option<String> {
    #[cfg(unix)]
    {
        // SAFETY: getuid(2) takes no arguments, performs no pointer
        // dereferences, and cannot fail — it's one of the few libc
        // calls with no error path at all.
        let uid = unsafe { libc::getuid() };
        Some(format!("uid:{uid}"))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

impl SearchContext {
    /// A `SearchContext` for the OS user this process is running as —
    /// the sensible default for every request-serving entry point in
    /// this codebase (HTTP API, WebSocket, CLI) unless/until a real
    /// authentication layer supplies a more specific principal. See
    /// [`current_os_principal`]'s doc comment for what this is and
    /// isn't a substitute for.
    pub fn current_os() -> Self {
        SearchContext {
            principal: current_os_principal(),
            groups: HashSet::new(),
        }
    }
}

/// One candidate result from a single source, before cross-source
/// merging. `raw_score` is whatever that source's own ranking already
/// produced (BM25+signals composite for local/web, a note app's own
/// relevance score, etc.) — this module does not recompute it.
#[derive(Debug, Clone)]
pub struct SourceCandidate {
    pub id: String,
    pub source: SourceKind,
    pub title: String,
    pub snippet: String,
    pub raw_score: f32,
    pub acl: Acl,
}

/// One entity in the final, merged, permission-filtered result list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedEntity {
    pub id: String,
    pub source: SourceKind,
    pub title: String,
    pub snippet: String,
    /// Raw score from the originating source (kept for debugging/
    /// `--explain` style output).
    pub raw_score: f32,
    /// Z-score-normalized, cross-source-comparable score. This is what
    /// the final result list should be sorted by, not `raw_score`.
    pub normalized_score: f32,
}

pub struct HybridRanker {
    /// Per-source weight applied after normalization, so e.g. local
    /// files can still be nudged above web results at equal normalized
    /// relevance (mirroring `RankingConfig::local_boost`'s existing
    /// intent) without that nudge being confused with raw-score bias.
    pub source_weights: std::collections::HashMap<SourceKind, f32>,
}

impl Default for HybridRanker {
    fn default() -> Self {
        use SourceKind::*;
        let source_weights = [
            (LocalFile, 1.1),
            (Note, 1.1),
            (Pdf, 1.0),
            (Email, 1.0),
            (Archive, 0.95),
            (WebPage, 1.0),
            (Federated, 0.85),
        ]
        .into_iter()
        .collect();
        HybridRanker { source_weights }
    }
}

impl HybridRanker {
    /// Normalizes `candidates`' scores within each [`SourceKind`] group
    /// (z-score: `(x - mean) / stddev`, falling back to a flat `0.0` for
    /// a group with zero variance so a single-candidate or all-tied
    /// group doesn't divide by zero), applies each source's weight, then
    /// filters out anything `ctx` isn't permitted to see, and returns
    /// the rest sorted best-first.
    pub fn merge(&self, candidates: Vec<SourceCandidate>, ctx: &SearchContext) -> Vec<UnifiedEntity> {
        let mut by_source: std::collections::HashMap<SourceKind, Vec<usize>> =
            std::collections::HashMap::new();
        for (i, c) in candidates.iter().enumerate() {
            by_source.entry(c.source).or_default().push(i);
        }

        let mut normalized_scores = vec![0.0f32; candidates.len()];
        for (_source, indices) in &by_source {
            let scores: Vec<f32> = indices.iter().map(|&i| candidates[i].raw_score).collect();
            let mean = scores.iter().sum::<f32>() / scores.len() as f32;
            let variance =
                scores.iter().map(|s| (s - mean).powi(2)).sum::<f32>() / scores.len() as f32;
            let stddev = variance.sqrt();
            for &i in indices {
                normalized_scores[i] = if stddev > 1e-6 {
                    (candidates[i].raw_score - mean) / stddev
                } else {
                    0.0
                };
            }
        }

        let mut merged: Vec<UnifiedEntity> = candidates
            .into_iter()
            .enumerate()
            .filter(|(_, c)| c.acl.is_visible_to(ctx))
            .map(|(i, c)| {
                let weight = self.source_weights.get(&c.source).copied().unwrap_or(1.0);
                UnifiedEntity {
                    id: c.id,
                    source: c.source,
                    title: c.title,
                    snippet: c.snippet,
                    raw_score: c.raw_score,
                    normalized_score: normalized_scores[i] * weight,
                }
            })
            .collect();

        merged.sort_by(|a, b| {
            b.normalized_score
                .partial_cmp(&a.normalized_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        merged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, source: SourceKind, score: f32, acl: Acl) -> SourceCandidate {
        SourceCandidate {
            id: id.to_string(),
            source,
            title: id.to_string(),
            snippet: String::new(),
            raw_score: score,
            acl,
        }
    }

    #[test]
    fn low_scale_source_can_outrank_high_scale_source_after_normalization() {
        // Local BM25-ish scores in the 5-10 range; web composite scores
        // in the 1-2 range. Without normalization the web results would
        // never surface above local ones, or vice versa, purely because
        // of scale, regardless of relative relevance within each source.
        let candidates = vec![
            candidate("local-1", SourceKind::LocalFile, 6.0, Acl::public()),
            candidate("local-2", SourceKind::LocalFile, 6.1, Acl::public()),
            candidate("local-3", SourceKind::LocalFile, 20.0, Acl::public()), // outlier best local match
            candidate("web-1", SourceKind::WebPage, 1.0, Acl::public()),
            candidate("web-2", SourceKind::WebPage, 1.05, Acl::public()),
        ];
        let ranker = HybridRanker::default();
        let ctx = SearchContext::default();
        let merged = ranker.merge(candidates, &ctx);
        assert_eq!(merged[0].id, "local-3", "the clear best-within-source match should still win");
    }

    #[test]
    fn permission_check_hides_entities_the_searcher_cannot_see() {
        let candidates = vec![
            candidate("public-doc", SourceKind::WebPage, 1.0, Acl::public()),
            candidate("alices-note", SourceKind::Note, 5.0, Acl::owned_by("alice")),
        ];
        let ranker = HybridRanker::default();

        let bob_ctx = SearchContext {
            principal: Some("bob".to_string()),
            groups: HashSet::new(),
        };
        let bob_results = ranker.merge(candidates.clone(), &bob_ctx);
        assert_eq!(bob_results.len(), 1);
        assert_eq!(bob_results[0].id, "public-doc");

        let alice_ctx = SearchContext {
            principal: Some("alice".to_string()),
            groups: HashSet::new(),
        };
        let alice_results = ranker.merge(candidates, &alice_ctx);
        assert_eq!(alice_results.len(), 2);
    }

    #[test]
    fn anonymous_context_only_sees_public_entities() {
        let candidates = vec![
            candidate("public-doc", SourceKind::WebPage, 1.0, Acl::public()),
            candidate("private-file", SourceKind::LocalFile, 9.0, Acl::owned_by("alice")),
        ];
        let ranker = HybridRanker::default();
        let anon_ctx = SearchContext::default();
        let results = ranker.merge(candidates, &anon_ctx);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "public-doc");
    }

    #[test]
    fn group_membership_grants_visibility() {
        let mut acl = Acl::default();
        acl.allowed_groups.insert("engineering".to_string());
        let candidates = vec![candidate("team-doc", SourceKind::Note, 3.0, acl)];
        let ranker = HybridRanker::default();

        let mut groups = HashSet::new();
        groups.insert("engineering".to_string());
        let ctx = SearchContext {
            principal: Some("carol".to_string()),
            groups,
        };
        assert_eq!(ranker.merge(candidates, &ctx).len(), 1);
    }
}
