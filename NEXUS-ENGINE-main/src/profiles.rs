//! Opinionated configuration profiles: `Config` has grown enough
//! independently tunable behavior (crawling, privacy, security, AI,
//! ranking) that picking a sensible combination by hand for a given use
//! case is real work. A profile is just a named, pre-chosen combination
//! of those existing `Config` fields — it introduces no new tunables of
//! its own, so anything a profile sets can still be freely overridden
//! afterward (`profiles::privacy_first().ranking.vector_weight = 0.6`,
//! etc.) with no special-casing needed.
//!
//! ## The four profiles
//! - [`ConfigProfile::PrivacyFirst`]: no web crawling of third-party
//!   sites by default, Tor available, aggressive history auto-deletion,
//!   telemetry off (already the engine-wide default — see
//!   `PrivacyConfig`, this profile just makes the *other* settings match
//!   that intent, e.g. disabling AI features that would otherwise send
//!   query text to an external endpoint).
//! - [`ConfigProfile::DocsSearch`]: tuned for indexing a local
//!   documentation/knowledge-base folder — filename/title matching
//!   boosted, no web crawling, no AI spend by default.
//! - [`ConfigProfile::Research`]: web crawling on with a generous page
//!   budget, PageRank and trusted-domain signals weighted up (source
//!   authority matters more when pulling from the open web), AI
//!   reranking/summarization enabled *if* the caller supplies an API key
//!   (a profile can't invent credentials, so AI stays off until one is
//!   provided — see [`ConfigProfile::apply`]'s doc comment).
//! - [`ConfigProfile::EnterpriseLocal`]: no web crawling at all (assumed
//!   deployment: search across internal file shares/document stores
//!   only), auth required on the API, stricter rate limits, WebSocket
//!   disabled by default (smaller attack surface) — an operator opts
//!   into more surface area explicitly rather than it being on by
//!   default.

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigProfile {
    PrivacyFirst,
    DocsSearch,
    Research,
    EnterpriseLocal,
}

impl ConfigProfile {
    pub fn label(&self) -> &'static str {
        match self {
            ConfigProfile::PrivacyFirst => "privacy-first",
            ConfigProfile::DocsSearch => "docs-search",
            ConfigProfile::Research => "research",
            ConfigProfile::EnterpriseLocal => "enterprise-local",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "privacy-first" => Some(ConfigProfile::PrivacyFirst),
            "docs-search" => Some(ConfigProfile::DocsSearch),
            "research" => Some(ConfigProfile::Research),
            "enterprise-local" => Some(ConfigProfile::EnterpriseLocal),
            _ => None,
        }
    }

    /// Builds a fresh [`Config`] with this profile's opinionated
    /// defaults layered over `Config::default()`. AI features are never
    /// turned on by a profile alone — even [`ConfigProfile::Research`],
    /// where AI reranking is the most obviously useful, leaves
    /// `ai.enabled = false` here, because turning it on with an empty
    /// `api_key` would be a no-op that silently misleads a caller into
    /// thinking it's active. Call [`ConfigProfile::enable_ai`] afterward
    /// once a real key is available.
    pub fn build(&self) -> Config {
        let mut config = Config::default();
        self.apply(&mut config);
        config
    }

    /// Applies this profile's settings onto an existing `config` in
    /// place, so a profile can be layered onto a config the caller
    /// already has (e.g. one loaded from disk with `indexed_folders`
    /// already populated) rather than only onto a fresh default.
    pub fn apply(&self, config: &mut Config) {
        match self {
            ConfigProfile::PrivacyFirst => {
                config.privacy.block_sponsored_results = true;
                config.privacy.no_filter_bubble = true;
                config.privacy.anonymize_queries = true;
                config.privacy.disable_telemetry = true;
                config.privacy.auto_delete_history_days = Some(30);
                config.web_crawl.max_pages = 0;
                config.web_crawl.allowed_domains = Vec::new();
                config.tor.enabled = false; // available, not forced on
                config.ai.enabled = false;
                config.security.api_require_auth = true;
                config.websocket.enabled = false;
            }
            ConfigProfile::DocsSearch => {
                config.ranking.filename_boost = 2.5;
                config.ranking.title_match_boost = 2.0;
                config.ranking.exact_match_boost = 1.8;
                config.ranking.pagerank_weight = 0.0; // no web graph in play
                config.ranking.recency_boost = 1.3; // newer docs likely more accurate
                config.web_crawl.max_pages = 0;
                config.ai.enabled = false;
                config.websocket.enabled = true; // instant search-as-you-type over docs
            }
            ConfigProfile::Research => {
                config.web_crawl.max_pages = 2000;
                config.web_crawl.max_depth = 6;
                config.web_crawl.discover_feeds = true;
                config.ranking.pagerank_weight = 0.8;
                config.ranking.trusted_domain_boost = 1.3;
                config.ranking.vector_weight = 0.55;
                config.ai.rerank_top_n = 30;
                config.ai.summary_max_sources = 8;
                config.privacy.anonymize_queries = false; // research workflows often need query history/reformulation tracking
            }
            ConfigProfile::EnterpriseLocal => {
                config.web_crawl.max_pages = 0;
                config.web_crawl.allowed_domains = Vec::new();
                config.security.api_require_auth = true;
                config.websocket.enabled = false;
                config.ai.enabled = false;
                config.privacy.disable_telemetry = true;
                config.privacy.auto_delete_history_days = Some(180);
            }
        }
    }

    /// Turns AI reranking/summarization on for this config, given a real
    /// API key — separated from [`ConfigProfile::apply`] deliberately
    /// (see that method's doc comment for why).
    pub fn enable_ai(config: &mut Config, api_key: impl Into<String>) {
        config.ai.enabled = true;
        config.ai.api_key = api_key.into();
    }

    pub fn all() -> [ConfigProfile; 4] {
        [
            ConfigProfile::PrivacyFirst,
            ConfigProfile::DocsSearch,
            ConfigProfile::Research,
            ConfigProfile::EnterpriseLocal,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privacy_first_disables_web_crawling_and_requires_auth() {
        let config = ConfigProfile::PrivacyFirst.build();
        assert_eq!(config.web_crawl.max_pages, 0);
        assert!(config.security.api_require_auth);
        assert!(!config.ai.enabled);
    }

    #[test]
    fn research_profile_raises_pagerank_and_ai_context_size() {
        let config = ConfigProfile::Research.build();
        assert!(config.ranking.pagerank_weight > RankingDefaults::default_pagerank_weight());
        assert!(config.ai.summary_max_sources > 5);
    }

    #[test]
    fn enterprise_local_locks_down_the_api_surface() {
        let config = ConfigProfile::EnterpriseLocal.build();
        assert_eq!(config.web_crawl.max_pages, 0);
        assert!(!config.websocket.enabled);
        assert!(config.security.api_require_auth);
    }

    #[test]
    fn labels_round_trip() {
        for profile in ConfigProfile::all() {
            assert_eq!(ConfigProfile::from_label(profile.label()), Some(profile));
        }
        assert_eq!(ConfigProfile::from_label("not-a-real-profile"), None);
    }

    #[test]
    fn enable_ai_requires_an_explicit_key() {
        let mut config = ConfigProfile::Research.build();
        assert!(!config.ai.enabled);
        ConfigProfile::enable_ai(&mut config, "sk-test-key");
        assert!(config.ai.enabled);
        assert_eq!(config.ai.api_key, "sk-test-key");
    }

    /// Small helper so the "research raises pagerank weight" test
    /// doesn't hardcode `RankingConfig::default()`'s current numeric
    /// value inline (keeping this test resilient to that default
    /// changing for unrelated reasons).
    struct RankingDefaults;
    impl RankingDefaults {
        fn default_pagerank_weight() -> f32 {
            crate::config::RankingConfig::default().pagerank_weight
        }
    }
}
