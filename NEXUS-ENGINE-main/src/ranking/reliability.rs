//! Reliability scoring: a bundle of objectively computable trust-proxy
//! signals for web results, shown alongside the ranking explanation so
//! people can gauge "how much should I trust this source" at a glance.
//!
//! **This is not fact-checking.** It cannot and does not evaluate whether
//! any claim on a page is true. Every signal here is a *proxy* — things
//! that correlate with trustworthiness in aggregate (a domain you've
//! explicitly marked trusted, a page reached without a long redirect
//! chain, transport security, an attributed author, other pages in the
//! index linking to it) without verifying anything about the content
//! itself. A well-produced piece of misinformation on an HTTPS domain
//! with a byline will still score reasonably here; a legitimate but
//! obscure page with no inbound links will score lower than its content
//! deserves. Treat this as "worth a second look before trusting" input,
//! not a verdict.

use crate::config::RankingConfig;
use crate::webdoc::WebPageMeta;
use serde::Serialize;

/// A coarse three-level summary of [`ReliabilitySignals::score`], for
/// simple UI treatments (a colored dot, a badge) that don't want to
/// reason about the raw number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReliabilityLabel {
    Low,
    Moderate,
    High,
}

/// One factor contributing to the reliability score.
#[derive(Debug, Clone, Serialize)]
pub struct ReliabilityFactor {
    /// Short label, e.g. "HTTPS".
    pub label: String,
    /// Points this factor contributed (positive or negative) to the
    /// 0-100 score, before clamping.
    pub points: i32,
    /// Plain-language explanation of the factor.
    pub detail: String,
}

/// The full reliability assessment for one web result.
#[derive(Debug, Clone, Serialize)]
pub struct ReliabilitySignals {
    /// 0-100 composite score. Starts from a neutral baseline of 50 and
    /// is adjusted by each applicable factor, then clamped.
    pub score: u8,
    pub label: ReliabilityLabel,
    pub factors: Vec<ReliabilityFactor>,
}

/// Computes reliability signals for a web page. `in_degree` is the number
/// of other indexed pages linking to it (from the link graph);
/// `total_web_docs` is the size of the web-page corpus, used to scale the
/// in-degree signal proportionally rather than with an arbitrary
/// fixed cutoff that would behave differently on a 50-page crawl vs. a
/// 50,000-page one.
pub fn compute(
    meta: &WebPageMeta,
    in_degree: usize,
    total_web_docs: usize,
    config: &RankingConfig,
) -> ReliabilitySignals {
    let mut score: i32 = 50;
    let mut factors = Vec::new();

    let mut add = |points: i32, label: &str, detail: String| {
        score += points;
        factors.push(ReliabilityFactor {
            label: label.to_string(),
            points,
            detail,
        });
    };

    if meta.url.to_lowercase().starts_with("https://") {
        add(10, "HTTPS", "Served over an encrypted connection.".to_string());
    } else {
        add(
            -15,
            "No HTTPS",
            "Served over plain HTTP; the connection isn't encrypted or authenticated.".to_string(),
        );
    }

    if config.trusted_domains.contains(&meta.domain) {
        add(
            20,
            "Trusted domain",
            "This domain is on the configured trusted-domain list.".to_string(),
        );
    } else if config.spam_domains.contains(&meta.domain) {
        add(
            -40,
            "Flagged domain",
            "This domain is on the configured spam/low-quality domain list.".to_string(),
        );
    }

    if meta.domain.ends_with(".gov") || meta.domain.ends_with(".edu") {
        add(
            15,
            "Institutional domain",
            "The domain's TLD (.gov/.edu) is restricted to government or accredited educational institutions.".to_string(),
        );
    }

    if total_web_docs > 1 {
        let ratio = in_degree as f32 / total_web_docs as f32;
        // Scaled so being linked from a meaningful fraction of the
        // crawled corpus earns real points, but a single inbound link in
        // a large corpus doesn't swing the score much.
        let points = (ratio * 200.0).round().clamp(0.0, 20.0) as i32;
        if points > 0 {
            add(
                points,
                "Linked by other indexed pages",
                format!(
                    "{} other page(s) in this index link to it.",
                    in_degree
                ),
            );
        }
    }

    if meta.redirect_chain.len() > 2 {
        add(
            -10,
            "Long redirect chain",
            format!(
                "Reached via a {}-hop redirect chain, which can indicate link rot, tracking redirectors, or a moved/retired page.",
                meta.redirect_chain.len()
            ),
        );
    }

    if meta.author.is_some() {
        add(
            5,
            "Attributed author",
            "The page declares an author, unlike anonymous or unattributed content.".to_string(),
        );
    }

    let score = score.clamp(0, 100) as u8;
    let label = if score >= 70 {
        ReliabilityLabel::High
    } else if score >= 40 {
        ReliabilityLabel::Moderate
    } else {
        ReliabilityLabel::Low
    };

    ReliabilitySignals { score, label, factors }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_meta(url: &str, domain: &str) -> WebPageMeta {
        WebPageMeta {
            url: url.to_string(),
            domain: domain.to_string(),
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
            spam_score: 0.0,
            policy_flag: None,
        }
    }

    #[test]
    fn plain_http_scores_lower_than_https() {
        let config = RankingConfig::default();
        let https = compute(&base_meta("https://example.com/a", "example.com"), 0, 10, &config);
        let http = compute(&base_meta("http://example.com/a", "example.com"), 0, 10, &config);
        assert!(https.score > http.score);
    }

    #[test]
    fn trusted_domain_scores_higher_than_untrusted() {
        let mut config = RankingConfig::default();
        config.trusted_domains.clear();
        config.trusted_domains.insert("trusted.example".to_string());

        let trusted = compute(&base_meta("https://trusted.example/a", "trusted.example"), 0, 10, &config);
        let ordinary = compute(&base_meta("https://ordinary.example/a", "ordinary.example"), 0, 10, &config);
        assert!(trusted.score > ordinary.score);
        assert_eq!(trusted.label, ReliabilityLabel::High);
    }

    #[test]
    fn spam_domain_scores_low() {
        let mut config = RankingConfig::default();
        config.spam_domains.insert("spam.example".to_string());
        let result = compute(&base_meta("https://spam.example/a", "spam.example"), 0, 10, &config);
        assert_eq!(result.label, ReliabilityLabel::Low);
    }

    #[test]
    fn gov_domain_gets_institutional_bonus() {
        let config = RankingConfig::default();
        let gov = compute(&base_meta("https://example.gov/a", "example.gov"), 0, 10, &config);
        let com = compute(&base_meta("https://example.com/a", "example.com"), 0, 10, &config);
        assert!(gov.score > com.score);
    }

    #[test]
    fn heavily_linked_page_scores_higher_than_unlinked() {
        let config = RankingConfig::default();
        let linked = compute(&base_meta("https://example.com/a", "example.com"), 5, 10, &config);
        let unlinked = compute(&base_meta("https://example.com/b", "example.com"), 0, 10, &config);
        assert!(linked.score > unlinked.score);
    }

    #[test]
    fn long_redirect_chain_is_penalized() {
        let config = RankingConfig::default();
        let mut meta = base_meta("https://example.com/a", "example.com");
        meta.redirect_chain = vec!["https://a".into(), "https://b".into(), "https://c".into()];
        let result = compute(&meta, 0, 10, &config);
        assert!(result.factors.iter().any(|f| f.label == "Long redirect chain" && f.points < 0));
    }

    #[test]
    fn score_is_always_clamped_to_valid_range() {
        let mut config = RankingConfig::default();
        config.spam_domains.insert("terrible.example".to_string());
        let mut meta = base_meta("http://terrible.example/a", "terrible.example");
        meta.redirect_chain = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        let result = compute(&meta, 0, 1000, &config);
        assert!(result.score <= 100);
    }
}
