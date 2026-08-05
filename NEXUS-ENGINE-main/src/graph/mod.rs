//! A personal knowledge graph across indexed sources: extracts entity
//! mentions (people, organizations, concepts) from documents, links
//! entities that co-occur in the same document, and answers "what did I
//! read about X" / "what's connected to X" style queries by walking that
//! graph rather than just doing a keyword search.
//!
//! **Read this before assuming "entity recognition" means a trained NER
//! model.** It doesn't, here. A real neural NER model (or even a
//! decent statistical CRF-based one) needs labeled training data and a
//! model file this repository doesn't have — see `crate::classify`'s
//! module doc comment for the same tradeoff made the same way. What's
//! implemented is a transparent, rule-based entity extractor
//! ([`extract_entities`]): capitalized multi-word sequences as candidate
//! Person/Organization mentions, a small closed set of pattern types
//! (email addresses, dates, code-identifier-shaped tokens as
//! `Concept::Code`), and frequency-based promotion of a candidate to a
//! "real" entity only once it's been seen more than once. This will
//! miss lowercase entity names, will occasionally promote a
//! capitalized-first-word-of-a-sentence false positive, and does not
//! disambiguate two different people who happen to share a name. It is
//! good enough to build a genuinely useful "what have I encountered
//! about X across my files" graph; it is not the same thing as
//! "understands language."
//!
//! ## Cross-source integration
//! This operates on `(source_id, text, timestamp)` triples, deliberately
//! not on any one source-specific type, so it can be fed from local
//! files, browser history titles ([`crate::extract::browser_history`]),
//! and email subjects/bodies ([`crate::extract::email`]) uniformly —
//! see [`GraphBuilder::ingest`].

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EntityKind {
    /// A capitalized multi-word or single-word proper-noun-shaped mention
    /// not otherwise classified — covers both people and organizations,
    /// since a rule-based extractor genuinely cannot reliably tell "Sarah
    /// Chen" (person) from "Acme Corp" (organization) apart without a
    /// real NER model; keeping them one kind is honest about that limit
    /// rather than guessing a split that would often be wrong.
    ProperNoun,
    EmailAddress,
    /// A lowercase, code-identifier-shaped token (`snake_case`,
    /// `camelCase`, or containing `::`/`.`) — a cheap proxy for "this
    /// document is talking about a specific function/module/API," useful
    /// for a "what did I read about `tokio::spawn`" query.
    CodeIdentifier,
}

/// One occurrence of an entity in one source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mention {
    pub source_id: String,
    pub timestamp_unix: i64,
}

/// One node in the knowledge graph: a canonical entity name plus every
/// place it was mentioned.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub name: String,
    pub kind: EntityKind,
    pub mentions: Vec<Mention>,
}

impl Entity {
    pub fn mention_count(&self) -> usize {
        self.mentions.len()
    }

    pub fn last_seen_unix(&self) -> i64 {
        self.mentions.iter().map(|m| m.timestamp_unix).max().unwrap_or(0)
    }
}

/// Extracts candidate entity mentions from raw text. Returns
/// `(canonical_name, kind)` pairs; a caller (typically
/// [`GraphBuilder::ingest`]) is responsible for deduplicating/counting
/// across the whole corpus, since a single document's extraction has no
/// visibility into the frequency threshold applied corpus-wide.
pub fn extract_entities(text: &str) -> Vec<(String, EntityKind)> {
    let mut found = Vec::new();

    // Email addresses: simple, well-understood shape, no need for a
    // full RFC 5322 grammar for entity-extraction purposes.
    for word in text.split_whitespace() {
        let trimmed = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '@' && c != '.');
        if trimmed.contains('@') && trimmed.contains('.') && !trimmed.starts_with('@') {
            let at_count = trimmed.matches('@').count();
            if at_count == 1 {
                found.push((trimmed.to_lowercase(), EntityKind::EmailAddress));
            }
        }
    }

    // Capitalized multi-word runs: "Sarah Chen", "Acme Corporation".
    // A run must start mid-sentence-safely: require the run to be either
    // 2+ consecutive capitalized words, or a single capitalized word that
    // is *not* the first word of its sentence (crude sentence-start
    // tracking via previous-token-ends-with-period), to cut down on
    // "Every capitalized sentence start is a proper noun" false positives.
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut i = 0;
    let mut sentence_start = true;
    while i < words.len() {
        let word = words[i].trim_matches(|c: char| !c.is_alphanumeric());
        let is_capitalized = word.chars().next().map(|c| c.is_uppercase()).unwrap_or(false)
            && word.chars().skip(1).any(|c| c.is_lowercase());
        if is_capitalized {
            let mut run = vec![word];
            let mut j = i + 1;
            while j < words.len() {
                let next = words[j].trim_matches(|c: char| !c.is_alphanumeric());
                let next_capitalized =
                    next.chars().next().map(|c| c.is_uppercase()).unwrap_or(false);
                if next_capitalized && !next.is_empty() {
                    run.push(next);
                    j += 1;
                } else {
                    break;
                }
            }
            if run.len() >= 2 || !sentence_start {
                let name = run.join(" ");
                if name.len() >= 3 {
                    found.push((name, EntityKind::ProperNoun));
                }
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
        sentence_start = words
            .get(i.saturating_sub(1))
            .map(|w| w.ends_with('.') || w.ends_with('!') || w.ends_with('?'))
            .unwrap_or(true);
    }

    // Code-identifier-shaped tokens. '.' is kept by the general trim
    // (unlike other punctuation) because dotted-path identifiers like
    // `os.path.join` are exactly the kind of thing this is meant to
    // catch — but that means a trailing sentence-ending period (e.g.
    // "...handling shutdown_signal.") never gets stripped by the
    // general trim either, so it's stripped separately here. A genuine
    // identifier ending in a literal `.` isn't a realistic case to
    // preserve at the cost of every sentence-final one being wrong.
    for word in &words {
        let trimmed = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != ':' && c != '.');
        let trimmed = trimmed.trim_end_matches('.');
        let looks_like_code = trimmed.contains("::")
            || (trimmed.contains('_') && trimmed.chars().all(|c| c.is_alphanumeric() || c == '_'))
            || is_camel_case(trimmed);
        if looks_like_code && trimmed.len() >= 4 && trimmed.len() <= 60 {
            found.push((trimmed.to_string(), EntityKind::CodeIdentifier));
        }
    }

    found
}

fn is_camel_case(s: &str) -> bool {
    let has_lower = s.chars().any(|c| c.is_lowercase());
    let has_inner_upper = s.chars().skip(1).any(|c| c.is_uppercase());
    has_lower && has_inner_upper && s.chars().all(|c| c.is_alphanumeric())
}

/// Builds and queries the graph across an entire corpus.
#[derive(Debug, Serialize, Deserialize)]
pub struct GraphBuilder {
    entities: HashMap<String, Entity>,
    /// `(entity_a, entity_b) -> co-occurrence count`, keyed with the
    /// lexicographically smaller name first so each pair is stored once.
    co_occurrence: HashMap<(String, String), u32>,
    /// Only entities seen at least this many times across the corpus are
    /// surfaced by [`GraphBuilder::top_entities`]/[`GraphBuilder::related_to`]
    /// — a single-mention capitalized word is far more likely to be a
    /// false positive than a real recurring entity of interest.
    min_mentions_to_surface: usize,
    /// Which co-occurrence pairs each `source_id` contributed, so
    /// [`GraphBuilder::remove_source`] can retract exactly what a given
    /// source added to `co_occurrence` without rescanning the whole
    /// corpus. `ingest` adds at most one count per pair per source (the
    /// per-document dedup in `ingest`), so this is the exact inverse.
    #[serde(default)]
    source_pairs: HashMap<String, HashSet<(String, String)>>,
}

impl Default for GraphBuilder {
    fn default() -> Self {
        GraphBuilder {
            entities: HashMap::new(),
            co_occurrence: HashMap::new(),
            // Matches `new()` — a derived, zeroed `Default` would set
            // this to `0`, silently making `top_entities`/`related_to`
            // surface every single-mention false positive for any
            // `GraphBuilder` that reaches this impl via
            // `#[serde(default)]` (deserializing an index file from
            // before this field existed) rather than `new()`.
            min_mentions_to_surface: 2,
            source_pairs: HashMap::new(),
        }
    }
}

impl GraphBuilder {
    pub fn new() -> Self {
        GraphBuilder::default()
    }

    /// Extracts and records entities from one document/source's text.
    /// Safe to call more than once for the *same* `source_id` with
    /// different text (e.g. from `ingest` being called again after an
    /// edit) — but note it does not retract what an earlier call for
    /// that same `source_id` added; call [`GraphBuilder::remove_source`]
    /// first (or use [`GraphBuilder::reingest`]) if the source's
    /// previous mentions need to be replaced rather than added to.
    pub fn ingest(&mut self, source_id: &str, text: &str, timestamp_unix: i64) {
        let mentions_here: Vec<(String, EntityKind)> = extract_entities(text)
            .into_iter()
            .collect::<HashSet<_>>() // de-dup within this one document
            .into_iter()
            .collect();

        for (name, kind) in &mentions_here {
            let entity = self.entities.entry(name.clone()).or_insert_with(|| Entity {
                name: name.clone(),
                kind: *kind,
                mentions: Vec::new(),
            });
            entity.mentions.push(Mention {
                source_id: source_id.to_string(),
                timestamp_unix,
            });
        }

        let pairs_for_source = self.source_pairs.entry(source_id.to_string()).or_default();
        for i in 0..mentions_here.len() {
            for j in (i + 1)..mentions_here.len() {
                let (a, b) = (&mentions_here[i].0, &mentions_here[j].0);
                let key = if a < b {
                    (a.clone(), b.clone())
                } else {
                    (b.clone(), a.clone())
                };
                *self.co_occurrence.entry(key.clone()).or_insert(0) += 1;
                pairs_for_source.insert(key);
            }
        }
    }

    /// Retracts every mention and co-occurrence count that
    /// [`GraphBuilder::ingest`] previously recorded for `source_id`.
    /// Entities left with no remaining mentions afterward are removed
    /// entirely rather than kept around as empty nodes.
    ///
    /// This is what makes re-indexing an edited document correct: call
    /// `remove_source(&old_id)` (or just use [`GraphBuilder::reingest`])
    /// before re-ingesting the new version, so entity mentions from the
    /// document's previous content don't linger forever. Safe to call
    /// for a `source_id` that was never ingested — it's just a no-op.
    pub fn remove_source(&mut self, source_id: &str) {
        self.entities.retain(|_, entity| {
            entity.mentions.retain(|m| m.source_id != source_id);
            !entity.mentions.is_empty()
        });

        if let Some(pairs) = self.source_pairs.remove(source_id) {
            for key in pairs {
                match self.co_occurrence.get_mut(&key) {
                    Some(count) if *count > 1 => *count -= 1,
                    Some(_) => {
                        self.co_occurrence.remove(&key);
                    }
                    None => {}
                }
            }
        }
    }

    /// Replaces `source_id`'s contribution to the graph with a fresh
    /// extraction from `text` — equivalent to `remove_source` followed
    /// by `ingest`, for the common "this document was edited, re-index
    /// it" case where stale mentions from the old content must not
    /// accumulate alongside the new ones.
    pub fn reingest(&mut self, source_id: &str, text: &str, timestamp_unix: i64) {
        self.remove_source(source_id);
        self.ingest(source_id, text, timestamp_unix);
    }

    pub fn entity(&self, name: &str) -> Option<&Entity> {
        self.entities.get(name)
    }

    /// Every entity whose name contains `needle` (case-insensitive
    /// substring match), for the `@person:`/`@org:` query operators —
    /// see `crate::query::ast::QueryNode::FilterEntity`'s doc comment
    /// for why both operators end up calling this same lookup rather
    /// than filtering by a person/org distinction the extractor doesn't
    /// actually have. An empty `needle` matches nothing rather than
    /// every entity, so `@person:` with no name attached is a no-op
    /// filter (matches nothing) instead of silently degrading into "all
    /// documents that mention any entity at all."
    pub fn entities_containing(&self, needle: &str) -> Vec<&Entity> {
        if needle.trim().is_empty() {
            return Vec::new();
        }
        let needle_lower = needle.to_lowercase();
        self.entities
            .values()
            .filter(|e| e.name.to_lowercase().contains(&needle_lower))
            .collect()
    }

    /// Entities most frequently co-mentioned alongside `name`, sorted by
    /// co-occurrence count descending — the graph-neighbor query.
    pub fn related_to(&self, name: &str, limit: usize) -> Vec<(String, u32)> {
        let mut related: Vec<(String, u32)> = self
            .co_occurrence
            .iter()
            .filter_map(|((a, b), count)| {
                if a == name {
                    Some((b.clone(), *count))
                } else if b == name {
                    Some((a.clone(), *count))
                } else {
                    None
                }
            })
            .collect();
        related.sort_by(|a, b| b.1.cmp(&a.1));
        related.truncate(limit);
        related
    }

    /// "What did I encounter about X in [time range]" — every source ID
    /// that mentions `name` within `[from_unix, to_unix]`, most recent
    /// first.
    pub fn sources_mentioning(&self, name: &str, from_unix: i64, to_unix: i64) -> Vec<String> {
        let Some(entity) = self.entities.get(name) else {
            return Vec::new();
        };
        let mut sources: Vec<(String, i64)> = entity
            .mentions
            .iter()
            .filter(|m| m.timestamp_unix >= from_unix && m.timestamp_unix <= to_unix)
            .map(|m| (m.source_id.clone(), m.timestamp_unix))
            .collect();
        sources.sort_by(|a, b| b.1.cmp(&a.1));
        sources.into_iter().map(|(id, _)| id).collect()
    }

    /// The most frequently mentioned entities corpus-wide, above
    /// `min_mentions_to_surface` — useful as a "what/who comes up a lot
    /// across your files" overview.
    pub fn top_entities(&self, limit: usize) -> Vec<&Entity> {
        let mut entities: Vec<&Entity> = self
            .entities
            .values()
            .filter(|e| e.mention_count() >= self.min_mentions_to_surface)
            .collect();
        entities.sort_by(|a, b| b.mention_count().cmp(&a.mention_count()));
        entities.truncate(limit);
        entities
    }

    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_multi_word_proper_nouns() {
        let entities = extract_entities("I had a meeting with Sarah Chen about the roadmap.");
        assert!(entities.iter().any(|(n, k)| n == "Sarah Chen" && *k == EntityKind::ProperNoun));
    }

    #[test]
    fn extracts_email_addresses() {
        let entities = extract_entities("Reach out to alice@example.com for details.");
        assert!(entities
            .iter()
            .any(|(n, k)| n == "alice@example.com" && *k == EntityKind::EmailAddress));
    }

    #[test]
    fn extracts_code_identifiers() {
        let entities = extract_entities("The bug was in tokio::spawn when handling shutdown_signal.");
        assert!(entities.iter().any(|(n, _)| n == "tokio::spawn"));
        assert!(entities.iter().any(|(n, _)| n == "shutdown_signal"));
    }

    #[test]
    fn sentence_start_alone_is_not_promoted_to_entity() {
        let entities = extract_entities("Rust is a systems programming language.");
        // "Rust" alone at a sentence start, single capitalized word,
        // should not be promoted (would be far too noisy) — only
        // multi-word runs or non-sentence-initial single words are.
        assert!(!entities.iter().any(|(n, _)| n == "Rust"));
    }

    #[test]
    fn graph_links_co_occurring_entities_and_answers_related_to() {
        let mut graph = GraphBuilder::new();
        graph.ingest("doc1", "Sarah Chen met with John Smith to discuss Acme Corporation.", 1000);
        graph.ingest("doc2", "Sarah Chen sent an update about Acme Corporation.", 2000);
        graph.ingest("doc3", "John Smith joined a call about Acme Corporation.", 3000);

        let related = graph.related_to("Acme Corporation", 5);
        let names: Vec<&str> = related.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"Sarah Chen"));
        assert!(names.contains(&"John Smith"));
    }

    #[test]
    fn sources_mentioning_respects_time_range() {
        let mut graph = GraphBuilder::new();
        graph.ingest("old-doc", "Notes on Rust Async Runtime design.", 1_000_000);
        graph.ingest("old-doc-2", "More notes on Rust Async Runtime internals.", 1_000_500);
        graph.ingest("recent-doc", "Follow-up on Rust Async Runtime.", 5_000_000);

        let recent_only = graph.sources_mentioning("Rust Async Runtime", 4_000_000, 6_000_000);
        assert_eq!(recent_only, vec!["recent-doc".to_string()]);

        let all_time = graph.sources_mentioning("Rust Async Runtime", 0, 10_000_000);
        assert_eq!(all_time.len(), 3);
    }

    #[test]
    fn top_entities_filters_out_single_mentions() {
        let mut graph = GraphBuilder::new();
        graph.ingest("doc1", "Only Once Mentioned appears here.", 1);
        graph.ingest("doc2", "Frequent Flyer shows up.", 1);
        graph.ingest("doc3", "Frequent Flyer shows up again.", 2);

        let top = graph.top_entities(10);
        assert!(top.iter().any(|e| e.name == "Frequent Flyer"));
        assert!(!top.iter().any(|e| e.name == "Only Once Mentioned"));
    }

    #[test]
    fn indexing_a_document_populates_the_graph() {
        let mut index = crate::index::Index::new();
        let doc = crate::document::Document {
            metadata: crate::document::DocumentMetadata {
                path: std::path::PathBuf::from("/a.txt"),
                file_name: "a.txt".to_string(),
                extension: "txt".to_string(),
                size_bytes: 0,
                modified_unix: 1_700_000_000,
                token_count: 0,
                acl: crate::entity::Acl::public(),
            },
            content: "Linus Torvalds created Linux. Linus Torvalds also created Git.".to_string(),
        };
        index.index_document(doc);
        assert!(index.graph.entity("Linus Torvalds").is_some());
        assert_eq!(index.graph.entity("Linus Torvalds").unwrap().mention_count(), 1);
    }

    #[test]
    fn graph_survives_a_bincode_round_trip_alongside_the_index() {
        let mut index = crate::index::Index::new();
        let doc = crate::document::Document {
            metadata: crate::document::DocumentMetadata {
                path: std::path::PathBuf::from("/a.txt"),
                file_name: "a.txt".to_string(),
                extension: "txt".to_string(),
                size_bytes: 0,
                modified_unix: 1_700_000_000,
                token_count: 0,
                acl: crate::entity::Acl::public(),
            },
            content: "Linus Torvalds created Linux. Linus Torvalds also created Git.".to_string(),
        };
        index.index_document(doc);

        let bytes = bincode::serialize(&index).unwrap();
        let restored: crate::index::Index = bincode::deserialize(&bytes).unwrap();
        assert_eq!(restored.graph.entity_count(), index.graph.entity_count());
        assert!(restored.graph.entity("Linus Torvalds").is_some());
    }

    #[test]
    fn remove_source_retracts_its_mentions_and_co_occurrence() {
        let mut graph = GraphBuilder::new();
        graph.ingest("doc1", "Sarah Chen met with John Smith to discuss Acme Corporation.", 1000);
        graph.ingest("doc2", "Sarah Chen sent an update about Acme Corporation.", 2000);

        assert_eq!(graph.entity("Sarah Chen").unwrap().mention_count(), 2);
        let related_before = graph.related_to("Sarah Chen", 10);
        assert!(related_before.iter().any(|(name, _)| name == "John Smith"));

        graph.remove_source("doc1");

        // doc1's mention of Sarah Chen is gone, doc2's remains.
        assert_eq!(graph.entity("Sarah Chen").unwrap().mention_count(), 1);
        // John Smith only ever appeared in doc1, so it's gone entirely.
        assert!(graph.entity("John Smith").is_none());
        // The Sarah Chen <-> John Smith co-occurrence (only from doc1)
        // no longer shows up as a relation.
        let related_after = graph.related_to("Sarah Chen", 10);
        assert!(!related_after.iter().any(|(name, _)| name == "John Smith"));
    }

    #[test]
    fn remove_source_for_unknown_source_is_a_harmless_no_op() {
        let mut graph = GraphBuilder::new();
        graph.ingest("doc1", "Sarah Chen met with John Smith.", 1000);
        let before = graph.entity_count();
        graph.remove_source("never-ingested");
        assert_eq!(graph.entity_count(), before);
    }

    #[test]
    fn reingest_replaces_rather_than_accumulates() {
        let mut graph = GraphBuilder::new();
        graph.ingest("doc1", "Sarah Chen met with John Smith about Acme Corporation.", 1000);
        graph.ingest("doc1", "Sarah Chen met with John Smith about Acme Corporation.", 1000);
        // A naive `ingest`-only re-index (the old behavior) would leave
        // stale mentions from the previous version around.
        assert_eq!(graph.entity("Sarah Chen").unwrap().mention_count(), 2);

        // An edited version of the same document that no longer mentions
        // John Smith or Acme Corporation at all.
        graph.reingest("doc1", "Sarah Chen wrote some completely different notes.", 3000);

        assert_eq!(graph.entity("Sarah Chen").unwrap().mention_count(), 1);
        assert!(graph.entity("John Smith").is_none());
        assert!(graph.entity("Acme Corporation").is_none());
    }

    #[test]
    fn re_indexing_an_edited_document_does_not_accumulate_stale_entities() {
        // End-to-end regression test for the "re-indexing an edited
        // document adds new entity mentions without retracting old
        // ones" bug: index the same path twice with different content
        // and make sure only the latest content's entities remain.
        let mut index = crate::index::Index::new();
        let meta = |modified_unix: i64| crate::document::DocumentMetadata {
            path: std::path::PathBuf::from("/notes.txt"),
            file_name: "notes.txt".to_string(),
            extension: "txt".to_string(),
            size_bytes: 0,
            modified_unix,
            token_count: 0,
            acl: crate::entity::Acl::public(),
        };

        index.index_document(crate::document::Document {
            metadata: meta(1000),
            content: "Sarah Chen met with John Smith about Acme Corporation.".to_string(),
        });
        // Same path, edited content, re-indexed — this is what a file
        // watcher / re-crawl does for a changed file.
        index.index_document(crate::document::Document {
            metadata: meta(2000),
            content: "Sarah Chen wrote some completely different notes.".to_string(),
        });

        assert!(index.graph.entity("John Smith").is_none());
        assert!(index.graph.entity("Acme Corporation").is_none());
        assert_eq!(index.graph.entity("Sarah Chen").unwrap().mention_count(), 1);
    }
}
