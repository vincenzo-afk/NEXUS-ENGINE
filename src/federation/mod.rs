//! Federated search across independent Nexus instances: a query issued
//! to one instance can fan out to a configured set of peers, merge each
//! peer's results in via `crate::entity::HybridRanker`, and return a
//! combined list — without any peer's index ever being centralized or
//! shared wholesale.
//!
//! **Read this before assuming "federated" means the full peer-to-peer
//! network described in the feature request (Tor/i2p transport,
//! decentralized discovery, per-peer sharing policies).** What's
//! implemented is the useful, buildable core of that idea: a static,
//! operator-configured peer list, queried over plain HTTPS (reusing
//! `crate::web::http::HttpClient`, the same audited HTTP client the
//! crawler uses — not a new custom transport), with each peer's results
//! merged via the existing hybrid-ranking normalization. It genuinely
//! achieves "search my instance and my friends' instances without
//! centralizing an index." It does not implement:
//! - **Peer discovery.** Peers are added explicitly by URL
//!   ([`PeerRegistry::add_peer`]) — there is no gossip protocol, DHT, or
//!   automatic discovery. This is the same tradeoff most real federated
//!   systems (Mastodon/ActivityPub instance peering, for one) actually
//!   ship with in practice: an explicit peer list, not automatic mesh
//!   formation.
//! - **Tor/i2p transport.** Peer URLs can themselves be `.onion`
//!   addresses if the caller configures the underlying `HttpClient` with
//!   `crate::network::tor`'s SOCKS5 proxy support — that's an existing,
//!   separate piece of this codebase, not something this module
//!   reimplements.
//! - **Per-peer sharing policy enforcement.** A peer either answers a
//!   federated query or it doesn't (via its own `api::mod` config, e.g.
//!   an `EnterpriseLocal` profile refusing external federated queries
//!   entirely) — there's no protocol here for a peer to selectively
//!   share "topic-filtered" results. That's a real, meaningfully harder
//!   feature (it needs the *peer* to implement query-classification and
//!   partial-sharing logic) left for a future pass.

use crate::web::http::HttpClient;
use serde::{Deserialize, Serialize};

/// One configured peer instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub name: String,
    /// Base URL of the peer's search API, e.g. `https://peer.example:7878`.
    pub base_url: String,
    /// Whether to include this peer in fan-out by default; kept as a
    /// per-peer toggle so a peer can be temporarily excluded (e.g. it's
    /// been slow or unreachable) without removing its configuration.
    pub enabled: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PeerRegistry {
    peers: Vec<PeerInfo>,
}

impl PeerRegistry {
    pub fn new() -> Self {
        PeerRegistry::default()
    }

    /// Builds a registry directly from a configured peer list (e.g.
    /// `FederationConfig::peers`), rather than requiring callers to
    /// `add_peer` one at a time.
    pub fn from_peers(peers: Vec<PeerInfo>) -> Self {
        PeerRegistry { peers }
    }

    pub fn add_peer(&mut self, name: impl Into<String>, base_url: impl Into<String>) {
        self.peers.push(PeerInfo {
            name: name.into(),
            base_url: base_url.into(),
            enabled: true,
        });
    }

    pub fn set_enabled(&mut self, name: &str, enabled: bool) {
        if let Some(peer) = self.peers.iter_mut().find(|p| p.name == name) {
            peer.enabled = enabled;
        }
    }

    pub fn enabled_peers(&self) -> Vec<&PeerInfo> {
        self.peers.iter().filter(|p| p.enabled).collect()
    }

    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }
}

/// The minimal JSON shape expected back from a peer's search endpoint —
/// deliberately small so a peer running a different search engine
/// entirely could implement this contract and federate with Nexus too,
/// rather than requiring the exact internal `SearchResult` type.
#[derive(Debug, Clone, Deserialize)]
pub struct PeerResultDto {
    pub id: String,
    pub title: String,
    pub snippet: String,
    pub score: f32,
}

#[derive(Debug, Clone, Deserialize)]
struct PeerSearchResponse {
    results: Vec<PeerResultDto>,
}

/// One peer's outcome for one fan-out: either its results, or why it
/// didn't contribute any (unreachable, timed out, bad response) — a
/// caller showing "results from N of M peers" needs this, rather than
/// federation silently dropping a failed peer with no signal at all.
#[derive(Debug, Clone)]
pub enum PeerOutcome {
    Ok(Vec<PeerResultDto>),
    Unreachable(String),
}

pub struct FederatedSearchClient {
    http: HttpClient,
}

impl FederatedSearchClient {
    /// `http` should already be built with the desired per-peer timeout
    /// (via `HttpConfig`'s `timeout` field) — `HttpClient::get` has no
    /// per-call timeout override, so that's the only place a timeout
    /// budget actually takes effect. An earlier version of this struct
    /// carried its own `per_peer_timeout` field alongside `http` to
    /// document that intent, but never used it for anything (the real
    /// timeout always came from `http`'s own config) — a second number
    /// that looked like it configured something but didn't. Removed
    /// rather than kept as a no-op field.
    pub fn new(http: HttpClient) -> Self {
        FederatedSearchClient { http }
    }

    /// Queries every enabled peer in `registry` for `query`, sequentially
    /// (see the module doc comment: this is the buildable core, not a
    /// fully async/parallel fan-out engine — `crate::api::request_queue`
    /// could be layered on top by a caller wanting bounded-concurrency
    /// parallel fan-out instead). Every peer's outcome is reported, not
    /// just the successes, so a caller can surface partial-failure state
    /// rather than it being silently swallowed.
    pub fn fan_out(&self, registry: &PeerRegistry, query: &str) -> Vec<(String, PeerOutcome)> {
        registry
            .enabled_peers()
            .into_iter()
            .map(|peer| {
                let outcome = self.query_one_peer(peer, query);
                (peer.name.clone(), outcome)
            })
            .collect()
    }

    fn query_one_peer(&self, peer: &PeerInfo, query: &str) -> PeerOutcome {
        let url = format!(
            "{}/search?q={}",
            peer.base_url.trim_end_matches('/'),
            urlencoding::encode(query)
        );
        // `HttpClient::get` doesn't take a per-call timeout override in
        // its current signature — the per-peer timeout budget is applied
        // via `HttpConfig`'s own timeout field when `self.http` is
        // constructed (see `FederatedSearchClient::new`'s doc comment).
        match self.http.get(&url) {
            Ok(response) if response.status == 200 => {
                match serde_json::from_str::<PeerSearchResponse>(&response.body) {
                    Ok(parsed) => PeerOutcome::Ok(parsed.results),
                    Err(e) => PeerOutcome::Unreachable(format!("bad response format: {e}")),
                }
            }
            Ok(response) => PeerOutcome::Unreachable(format!("HTTP {}", response.status)),
            Err(e) => PeerOutcome::Unreachable(e.to_string()),
        }
    }

    /// Fans out to every enabled peer and merges every successful peer's
    /// results into [`crate::entity::SourceCandidate`]s tagged
    /// [`crate::entity::SourceKind::Federated`] plus `[peer name]`-prefixed
    /// titles, ready to hand to [`crate::entity::HybridRanker::merge`]
    /// alongside local candidates.
    pub fn fan_out_as_candidates(
        &self,
        registry: &PeerRegistry,
        query: &str,
    ) -> Vec<crate::entity::SourceCandidate> {
        self.fan_out(registry, query)
            .into_iter()
            .flat_map(|(peer_name, outcome)| match outcome {
                PeerOutcome::Ok(results) => results
                    .into_iter()
                    .map(|r| crate::entity::SourceCandidate {
                        id: format!("{peer_name}:{}", r.id),
                        source: crate::entity::SourceKind::Federated,
                        title: format!("[{peer_name}] {}", r.title),
                        snippet: r.snippet,
                        raw_score: r.score,
                        // Federated results carry no local permission
                        // context — the peer already decided what it was
                        // willing to answer with, so results returned
                        // here are treated as public to this instance's
                        // own searchers. A caller wanting finer-grained
                        // control should filter `fan_out`'s raw output
                        // before this conversion instead.
                        acl: crate::entity::Acl::public(),
                    })
                    .collect::<Vec<_>>(),
                PeerOutcome::Unreachable(_) => Vec::new(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_tracks_enabled_state() {
        let mut registry = PeerRegistry::new();
        registry.add_peer("alice-nexus", "https://alice.example:7878");
        registry.add_peer("bob-nexus", "https://bob.example:7878");
        assert_eq!(registry.enabled_peers().len(), 2);

        registry.set_enabled("bob-nexus", false);
        assert_eq!(registry.enabled_peers().len(), 1);
        assert_eq!(registry.enabled_peers()[0].name, "alice-nexus");
    }

    #[test]
    fn peer_result_dto_deserializes_expected_shape() {
        let json = r#"{"results":[{"id":"doc-1","title":"Rust ownership","snippet":"...","score":4.2}]}"#;
        let parsed: PeerSearchResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.results.len(), 1);
        assert_eq!(parsed.results[0].id, "doc-1");
    }
}
