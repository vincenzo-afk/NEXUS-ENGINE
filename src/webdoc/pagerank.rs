//! PageRank over the crawled link graph.
//!
//! The classic random-surfer model: a page's rank is the probability that
//! a surfer clicking links at random (and occasionally teleporting to a
//! uniformly random page, with probability `1 - damping`) is looking at
//! it at any given moment. Computed by power iteration, which converges
//! quickly (a few dozen iterations) for the sizes of graphs a
//! single-machine crawler builds.

use super::WebMetaStore;
use crate::document::DocId;
use log::{debug, info};
use std::collections::HashMap;

/// Standard PageRank damping factor (the probability of following a link
/// rather than teleporting), as used in the original paper.
pub const DEFAULT_DAMPING: f32 = 0.85;

/// Stop iterating once the largest per-page score change drops below this,
/// or after `max_iterations`, whichever comes first.
const CONVERGENCE_EPSILON: f32 = 1e-6;
const MAX_ITERATIONS: usize = 100;

/// Computes PageRank scores for every page in `store`, returning
/// `DocId -> score`. Pages with no outgoing links ("dangling nodes")
/// distribute their rank uniformly across the whole graph, as is standard
/// practice, so rank doesn't leak out of the system.
pub fn compute(store: &WebMetaStore, damping: f32) -> HashMap<DocId, f32> {
    let ids: Vec<DocId> = store.iter().map(|(id, _)| id).collect();
    let n = ids.len();
    if n == 0 {
        return HashMap::new();
    }
    let n_f = n as f32;

    let mut scores: HashMap<DocId, f32> = ids.iter().map(|&id| (id, 1.0 / n_f)).collect();

    for iteration in 0..MAX_ITERATIONS {
        info!("PageRank iteration {}/{}", iteration + 1, MAX_ITERATIONS);
        let dangling_mass: f32 = ids
            .iter()
            .filter(|&&id| store.get(id).map(|m| m.out_degree()).unwrap_or(0) == 0)
            .map(|&id| scores[&id])
            .sum();

        let mut next: HashMap<DocId, f32> = HashMap::with_capacity(n);
        let base = (1.0 - damping) / n_f + damping * dangling_mass / n_f;

        for &id in &ids {
            next.insert(id, base);
        }

        for &id in &ids {
            let meta = match store.get(id) {
                Some(m) => m,
                None => continue,
            };
            let out_degree = meta.out_degree();
            if out_degree == 0 {
                continue;
            }
            let _ = out_degree;
            // Only distribute rank across distinct outgoing targets; a
            // page linking to the same target twice shouldn't get 2x
            // credit relative to linking to two different pages.
            let mut targets: Vec<DocId> = meta.outgoing.iter().map(|e| e.doc_id).collect();
            targets.sort_unstable();
            targets.dedup();
            let per_target = damping * scores[&id] / targets.len().max(1) as f32;
            for target in targets {
                if let Some(entry) = next.get_mut(&target) {
                    *entry += per_target;
                }
            }
        }

        let max_delta = ids
            .iter()
            .map(|id| (next[id] - scores[id]).abs())
            .fold(0.0f32, f32::max);

        scores = next;
        if max_delta < CONVERGENCE_EPSILON {
            debug!(
                "PageRank converged after {} iterations (max_delta={})",
                iteration + 1,
                max_delta
            );
            break;
        }
        debug!("iteration {} max_delta={}", iteration + 1, max_delta);
    }

    scores
}

/// Computes PageRank and writes each page's score back into its
/// [`super::WebPageMeta::pagerank`] field so it can be persisted and used
/// directly by the ranking stage without recomputing on every search.
pub fn compute_and_store(store: &mut WebMetaStore, damping: f32) {
    info!(
        "computing PageRank for {} pages (damping={})",
        store.len(),
        damping
    );
    let scores = compute(store, damping);
    info!("PageRank computed, storing {} scores", scores.len());
    for (doc_id, score) in scores {
        if let Some(meta) = store.get_mut(doc_id) {
            meta.pagerank = score;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webdoc::{LinkEdge, WebPageMeta};

    fn page(url: &str) -> WebPageMeta {
        WebPageMeta {
            url: url.to_string(),
            domain: "example.com".to_string(),
            title: String::new(),
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
        }
    }

    /// Classic example: A and C both link to B, so B should end up with a
    /// materially higher score than A or C, which have no inbound links.
    #[test]
    fn heavily_linked_page_scores_higher() {
        let mut store = WebMetaStore::new();
        let mut a = page("https://example.com/a");
        a.outgoing.push(LinkEdge {
            doc_id: 1,
            anchor_text: "b".into(),
        });
        let mut c = page("https://example.com/c");
        c.outgoing.push(LinkEdge {
            doc_id: 1,
            anchor_text: "b".into(),
        });
        let b = page("https://example.com/b");

        store.insert(0, a);
        store.insert(1, b);
        store.insert(2, c);

        let scores = compute(&store, DEFAULT_DAMPING);
        assert!(scores[&1] > scores[&0]);
        assert!(scores[&1] > scores[&2]);
    }

    #[test]
    fn scores_sum_close_to_one() {
        let mut store = WebMetaStore::new();
        let mut a = page("https://example.com/a");
        a.outgoing.push(LinkEdge {
            doc_id: 1,
            anchor_text: "b".into(),
        });
        let mut b = page("https://example.com/b");
        b.outgoing.push(LinkEdge {
            doc_id: 0,
            anchor_text: "a".into(),
        });
        store.insert(0, a);
        store.insert(1, b);

        let scores = compute(&store, DEFAULT_DAMPING);
        let total: f32 = scores.values().sum();
        assert!((total - 1.0).abs() < 0.01, "total was {}", total);
    }

    #[test]
    fn empty_graph_yields_empty_scores() {
        let store = WebMetaStore::new();
        assert!(compute(&store, DEFAULT_DAMPING).is_empty());
    }

    #[test]
    fn dangling_node_does_not_leak_rank() {
        let mut store = WebMetaStore::new();
        let a = page("https://example.com/a"); // no outgoing links at all
        store.insert(0, a);
        let scores = compute(&store, DEFAULT_DAMPING);
        let total: f32 = scores.values().sum();
        assert!((total - 1.0).abs() < 0.01);
    }
}
