//! Web-specific document metadata.
//!
//! Local files have a path, a size, and a modified time. Crawled web pages
//! need more: the URL they were fetched from, caching headers for
//! incremental re-crawls, and the link-graph edges (incoming, outgoing,
//! anchor text) that power PageRank and domain-quality signals. Rather
//! than bloating [`crate::document::DocumentMetadata`] with fields that
//! are meaningless for local files, web pages get a parallel
//! [`WebPageMeta`] record, keyed by the same [`DocId`] and stored
//! alongside the rest of the index.

pub mod pagerank;

use crate::document::DocId;
use log::debug;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// One outbound or inbound link edge in the link graph.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LinkEdge {
    /// The other endpoint of the edge.
    pub doc_id: DocId,
    /// The anchor text used for this link, at the time it was last seen.
    pub anchor_text: String,
}

/// Crawl and ranking metadata for one web page, alongside its
/// `DocumentMetadata` (which continues to hold size/modified/token_count
/// generically for every document, local or web).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebPageMeta {
    /// The canonical URL this page was indexed under.
    pub url: String,
    /// Registrable domain (host without a leading `www.`), used for
    /// per-domain rate limiting and the domain-quality ranking signal.
    pub domain: String,
    /// Page title, as extracted from `<title>`.
    pub title: String,
    /// `<meta name="description">` content, if any.
    pub meta_description: String,
    /// `<html lang="...">` value, if declared, lowercased.
    pub lang: Option<String>,
    /// `<meta name="author">` content, if declared.
    pub author: Option<String>,
    /// Detected content type: `html`, `pdf`, `markdown`, `json`, `xml`.
    pub content_type: String,
    /// Unix timestamp of when this page was last fetched.
    pub fetched_unix: i64,
    /// `ETag` from the last fetch, used for conditional re-fetching.
    pub etag: Option<String>,
    /// `Last-Modified` from the last fetch, used for conditional re-fetching.
    pub last_modified: Option<String>,
    /// Every URL visited while following redirects to reach this page's
    /// final `url`, in order. Empty if the page was served directly with
    /// no redirect. Useful for auditing redirect loops/chains and for
    /// detecting when a site has moved its content (a long chain, or a
    /// chain ending on a different domain, is a signal worth surfacing).
    #[serde(default)]
    pub redirect_chain: Vec<String>,
    /// SimHash fingerprint of the extracted text, for near-duplicate
    /// detection against other crawled pages.
    pub simhash: u64,
    /// Crawl depth (0 = a seed URL) at which this page was first discovered.
    pub depth: u32,
    /// Outbound links whose target was itself indexed (i.e. both
    /// endpoints are in-graph), used to build the link graph for PageRank.
    pub outgoing: Vec<LinkEdge>,
    /// Inbound links from other indexed pages. Populated by
    /// [`build_incoming_links`] after a crawl, since a page's inbound
    /// links aren't known until the whole graph has been discovered.
    pub incoming: Vec<LinkEdge>,
    /// Cached PageRank score, refreshed by
    /// [`pagerank::compute_and_store`]. Starts at a neutral value so
    /// freshly-crawled pages not yet PageRanked don't score zero.
    pub pagerank: f32,
    /// Spam/low-quality score from [`crate::classify::spam::SpamClassifier`],
    /// computed once at crawl time (`0.0` = looks fine, `1.0` = strongly
    /// spam-like). Not present for local files or web pages crawled
    /// before this field existed, hence the default-on-deserialize.
    #[serde(default)]
    pub spam_score: f32,
    /// Set if [`crate::classify::safety::PolicyClassifier`] flagged this
    /// page at crawl time (explicit/phishing/malicious/scam), carrying
    /// the category label and confidence. `None` means the page passed
    /// the safety check (or was crawled before this field existed).
    #[serde(default)]
    pub policy_flag: Option<PolicyFlagRecord>,
}

/// A persisted, serializable summary of a [`crate::classify::safety::PolicyFlag`]
/// (that type itself isn't `Serialize`/`Deserialize`, and doesn't need to
/// be beyond this one persisted snapshot per page).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PolicyFlagRecord {
    pub category: String,
    pub confidence: f32,
}

impl WebPageMeta {
    /// Number of distinct pages linking to this one.
    pub fn in_degree(&self) -> usize {
        self.incoming.len()
    }

    /// Number of distinct pages this one links to.
    pub fn out_degree(&self) -> usize {
        self.outgoing.len()
    }
}

/// `DocId -> WebPageMeta`, plus the reverse `canonical URL -> DocId`
/// lookup incremental re-crawls and link resolution need.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct WebMetaStore {
    meta: HashMap<DocId, WebPageMeta>,
    url_to_id: HashMap<String, DocId>,
}

impl WebMetaStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        WebMetaStore::default()
    }

    /// Inserts or replaces the metadata for `doc_id`.
    pub fn insert(&mut self, doc_id: DocId, meta: WebPageMeta) {
        debug!(
            "WebMetaStore::insert(doc_id={}, url='{}')",
            doc_id, meta.url
        );
        self.url_to_id.insert(meta.url.clone(), doc_id);
        self.meta.insert(doc_id, meta);
    }

    /// Removes metadata for `doc_id`, if present.
    pub fn remove(&mut self, doc_id: DocId) -> Option<WebPageMeta> {
        debug!("WebMetaStore::remove(doc_id={})", doc_id);
        if let Some(meta) = self.meta.remove(&doc_id) {
            self.url_to_id.remove(&meta.url);
            Some(meta)
        } else {
            None
        }
    }

    /// Looks up metadata by document ID.
    pub fn get(&self, doc_id: DocId) -> Option<&WebPageMeta> {
        let result = self.meta.get(&doc_id);
        debug!(
            "WebMetaStore::get(doc_id={}) -> {}",
            doc_id,
            if result.is_some() { "hit" } else { "miss" }
        );
        result
    }

    /// Mutable lookup by document ID.
    pub fn get_mut(&mut self, doc_id: DocId) -> Option<&mut WebPageMeta> {
        self.meta.get_mut(&doc_id)
    }

    /// Looks up the document ID currently indexed for canonical `url`.
    pub fn id_for_url(&self, url: &str) -> Option<DocId> {
        self.url_to_id.get(url).copied()
    }

    /// Iterates over every `(DocId, &WebPageMeta)` pair.
    pub fn iter(&self) -> impl Iterator<Item = (DocId, &WebPageMeta)> {
        self.meta.iter().map(|(&id, meta)| (id, meta))
    }

    /// Number of web pages currently tracked.
    pub fn len(&self) -> usize {
        self.meta.len()
    }

    /// `true` if no web pages are tracked.
    pub fn is_empty(&self) -> bool {
        self.meta.is_empty()
    }
}

/// Rebuilds every page's `incoming` link list from the union of all
/// `outgoing` lists. Called once after a crawl (or batch of crawls)
/// completes, since a page's inbound links can't be known until the pages
/// linking to it have themselves been crawled and indexed.
pub fn build_incoming_links(store: &mut WebMetaStore) {
    debug!("building incoming links for {} pages", store.len());
    let mut incoming_by_target: HashMap<DocId, Vec<LinkEdge>> = HashMap::new();
    for (doc_id, meta) in store.iter() {
        for edge in &meta.outgoing {
            incoming_by_target
                .entry(edge.doc_id)
                .or_default()
                .push(LinkEdge {
                    doc_id,
                    anchor_text: edge.anchor_text.clone(),
                });
        }
    }

    let ids: Vec<DocId> = store.meta.keys().copied().collect();
    for doc_id in ids {
        let incoming = incoming_by_target.remove(&doc_id).unwrap_or_default();
        if let Some(meta) = store.get_mut(doc_id) {
            meta.incoming = incoming;
        }
    }
    debug!("incoming links built");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(url: &str) -> WebPageMeta {
        WebPageMeta {
            url: url.to_string(),
            domain: "example.com".to_string(),
            title: "Title".to_string(),
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
            pagerank: 1.0,
            spam_score: 0.0,
            policy_flag: None,
        }
    }

    #[test]
    fn builds_incoming_from_outgoing() {
        let mut store = WebMetaStore::new();
        let mut a = sample("https://example.com/a");
        a.outgoing.push(LinkEdge {
            doc_id: 1,
            anchor_text: "b page".to_string(),
        });
        store.insert(0, a);
        store.insert(1, sample("https://example.com/b"));

        build_incoming_links(&mut store);

        let b = store.get(1).unwrap();
        assert_eq!(b.incoming.len(), 1);
        assert_eq!(b.incoming[0].doc_id, 0);
        assert_eq!(b.incoming[0].anchor_text, "b page");
    }

    #[test]
    fn url_lookup_works_after_insert_and_remove() {
        let mut store = WebMetaStore::new();
        store.insert(5, sample("https://example.com/x"));
        assert_eq!(store.id_for_url("https://example.com/x"), Some(5));
        store.remove(5);
        assert_eq!(store.id_for_url("https://example.com/x"), None);
    }
}
