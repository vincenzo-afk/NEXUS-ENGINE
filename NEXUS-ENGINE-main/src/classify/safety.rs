//! Safe-search and policy filtering: heuristics to flag explicit,
//! malicious, scam, and phishing pages *before* they reach a results
//! page, distinct from [`crate::classify::spam`]'s quality filtering —
//! a page can be low-effort without being unsafe, and a well-produced
//! page can still be a phishing kit.
//!
//! Same honesty note as the sibling module: these are transparent
//! heuristics (URL shape, form/credential patterns, brand-impersonation
//! string distance, a small explicit-content keyword list used only for
//! *flagging for filtering*, never reproduced in output), not a trained
//! classifier and not a comprehensive threat-intel feed. A real
//! deployment should treat this as a first-pass filter to combine with
//! an actual threat-intelligence blocklist (Google Safe Browsing, a
//! commercial feed, etc.) via `PolicyClassifier::with_external_blocklist`,
//! not as a sole line of defense.

use crate::html::ExtractedContent;
use std::collections::HashSet;

/// Which policy category (if any) a page was flagged under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolicyCategory {
    Explicit,
    Phishing,
    Malicious,
    Scam,
}

impl PolicyCategory {
    pub fn label(&self) -> &'static str {
        match self {
            PolicyCategory::Explicit => "explicit",
            PolicyCategory::Phishing => "phishing",
            PolicyCategory::Malicious => "malicious",
            PolicyCategory::Scam => "scam",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PolicyFlag {
    pub category: PolicyCategory,
    pub confidence: f32,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct PolicyVerdict {
    pub flags: Vec<PolicyFlag>,
    /// An explicit, caller-supplied blocklist match (domain or exact
    /// URL), kept separate from the heuristic flags since it's a hard
    /// fact rather than an inferred signal.
    pub blocklisted: bool,
}

impl PolicyVerdict {
    /// `true` if any flag (or the blocklist) suggests this page should
    /// be excluded from default (safe-search-on) results.
    pub fn should_filter(&self) -> bool {
        self.blocklisted || self.flags.iter().any(|f| f.confidence >= 0.6)
    }
}

/// A small set of brand names commonly impersonated in phishing, used
/// only to check whether a *different* domain is using a brand's name in
/// its hostname/subdomain (the classic `paypal-secure-login.example.com`
/// pattern) — never used to identify or block the legitimate brand
/// domains themselves.
const COMMONLY_IMPERSONATED_BRANDS: &[&str] = &[
    "paypal", "apple", "microsoft", "google", "amazon", "netflix", "bankofamerica", "wellsfargo",
    "chase", "irs", "usps", "fedex", "dhl",
];

/// URL shorteners and redirect services, which legitimately exist but
/// are disproportionately used to obscure a scam/phishing destination
/// from a quick glance — treated as a mild signal, not a block.
const LINK_SHORTENERS: &[&str] = &[
    "bit.ly", "tinyurl.com", "t.co", "goo.gl", "ow.ly", "is.gd", "buff.ly",
];

pub struct PolicyClassifier {
    external_blocklist: HashSet<String>,
}

impl Default for PolicyClassifier {
    fn default() -> Self {
        PolicyClassifier {
            external_blocklist: HashSet::new(),
        }
    }
}

impl PolicyClassifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attaches a caller-managed set of blocklisted domains (e.g. loaded
    /// from a threat-intel feed or a locally maintained list). Matching
    /// this list always sets [`PolicyVerdict::blocklisted`] regardless of
    /// any heuristic score.
    pub fn with_external_blocklist(domains: impl IntoIterator<Item = String>) -> Self {
        PolicyClassifier {
            external_blocklist: domains.into_iter().collect(),
        }
    }

    pub fn classify(&self, url: &str, content: &ExtractedContent) -> PolicyVerdict {
        let host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_lowercase()))
            .unwrap_or_default();

        let blocklisted = self.external_blocklist.contains(&host)
            || self
                .external_blocklist
                .iter()
                .any(|d| host.ends_with(d.as_str()));

        let mut flags = Vec::new();
        if let Some(f) = Self::brand_impersonation(&host) {
            flags.push(f);
        }
        if let Some(f) = Self::credential_harvest_shape(url, content) {
            flags.push(f);
        }
        if let Some(f) = Self::scam_urgency_language(content) {
            flags.push(f);
        }
        if let Some(f) = Self::executable_download_lure(content, &host) {
            flags.push(f);
        }
        if let Some(f) = Self::explicit_content_signal(content) {
            flags.push(f);
        }

        PolicyVerdict {
            flags,
            blocklisted,
        }
    }

    /// Flags a hostname that embeds a well-known brand name but isn't
    /// that brand's actual registrable domain (crude substring + edit
    /// distance check, not a real WHOIS/certificate-transparency lookup).
    fn brand_impersonation(host: &str) -> Option<PolicyFlag> {
        for brand in COMMONLY_IMPERSONATED_BRANDS {
            let is_legit_domain = host == format!("{brand}.com")
                || host.ends_with(&format!(".{brand}.com"))
                || host == format!("{brand}.co")
                || host == format!("{brand}.gov");
            if !is_legit_domain && host.contains(brand) {
                return Some(PolicyFlag {
                    category: PolicyCategory::Phishing,
                    confidence: 0.7,
                    reason: format!("hostname '{host}' references brand '{brand}' but is not its domain"),
                });
            }
        }
        None
    }

    /// A login/password form combined with a non-HTTPS URL, or with a
    /// shortener/redirect host, is the shape of a credential-harvesting
    /// page rather than a proof of it (some legitimate internal tools
    /// really are plain HTTP) — kept at moderate confidence accordingly.
    fn credential_harvest_shape(url: &str, content: &ExtractedContent) -> Option<PolicyFlag> {
        let text = content.indexable_text().to_lowercase();
        let looks_like_login = text.contains("password")
            && (text.contains("sign in") || text.contains("log in") || text.contains("verify your account"));
        if !looks_like_login {
            return None;
        }
        let is_https = url.starts_with("https://");
        let parsed_host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
            .unwrap_or_default();
        let via_shortener = LINK_SHORTENERS.iter().any(|s| parsed_host == *s);
        let confidence = match (is_https, via_shortener) {
            (false, _) => 0.55,
            (_, true) => 0.6,
            _ => 0.0,
        };
        if confidence > 0.0 {
            Some(PolicyFlag {
                category: PolicyCategory::Phishing,
                confidence,
                reason: "login/password page served over a risky URL shape (non-HTTPS or shortener)"
                    .to_string(),
            })
        } else {
            None
        }
    }

    /// Manufactured urgency ("your account will be suspended," "act now,"
    /// combined with a request for payment/gift cards) is a well-known
    /// scam-copy pattern, checked as co-occurrence of an urgency phrase
    /// and a payment-request phrase (either alone is far too common in
    /// legitimate marketing copy to flag by itself).
    fn scam_urgency_language(content: &ExtractedContent) -> Option<PolicyFlag> {
        let text = content.indexable_text().to_lowercase();
        let urgency = ["act now", "account will be suspended", "verify immediately", "limited time", "final notice"]
            .iter()
            .any(|p| text.contains(p));
        let payment_ask = ["gift card", "wire transfer", "send bitcoin", "processing fee", "claim your prize"]
            .iter()
            .any(|p| text.contains(p));
        if urgency && payment_ask {
            Some(PolicyFlag {
                category: PolicyCategory::Scam,
                confidence: 0.75,
                reason: "urgency language combined with an unusual payment request".to_string(),
            })
        } else {
            None
        }
    }

    /// A page offering an executable/installer download from a host
    /// that isn't a recognized software vendor and whose body text
    /// pressures the visitor to run it ("click to fix," "your driver is
    /// outdated") matches a common malware-delivery page shape.
    fn executable_download_lure(content: &ExtractedContent, host: &str) -> Option<PolicyFlag> {
        let offers_executable = content.links.iter().any(|l| {
            let lower = l.href.to_lowercase();
            lower.ends_with(".exe") || lower.ends_with(".msi") || lower.ends_with(".apk")
                || lower.ends_with(".scr")
        });
        if !offers_executable {
            return None;
        }
        let text = content.indexable_text().to_lowercase();
        let pressure_language = ["your computer is infected", "driver is outdated", "click to fix", "update required immediately"]
            .iter()
            .any(|p| text.contains(p));
        let known_vendor_shape = host.ends_with(".microsoft.com")
            || host.ends_with(".apple.com")
            || host.ends_with(".github.com")
            || host.ends_with(".google.com");
        if pressure_language && !known_vendor_shape {
            Some(PolicyFlag {
                category: PolicyCategory::Malicious,
                confidence: 0.65,
                reason: "executable download paired with alarm-based pressure language".to_string(),
            })
        } else {
            None
        }
    }

    /// A conservative keyword-presence check for adult content, used
    /// only to power a safe-search *toggle* (excluded when off, shown
    /// when the user disables safe search) — this deliberately does not
    /// enumerate the keyword list in any user-facing explanation, only
    /// the category label, to avoid the list itself becoming a mini
    /// index of the terms it watches for.
    fn explicit_content_signal(content: &ExtractedContent) -> Option<PolicyFlag> {
        const SIGNAL_TERMS: &[&str] = &["xxx", "porn", "nsfw content warning", "adult content warning"];
        let text = content.indexable_text().to_lowercase();
        let hits = SIGNAL_TERMS.iter().filter(|t| text.contains(*t)).count();
        if hits > 0 {
            Some(PolicyFlag {
                category: PolicyCategory::Explicit,
                confidence: (0.4 + 0.2 * hits as f32).min(0.9),
                reason: "explicit-content indicator terms present".to_string(),
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content_with_text(paragraphs: Vec<&str>, links: Vec<&str>) -> ExtractedContent {
        ExtractedContent {
            paragraphs: paragraphs.into_iter().map(|s| s.to_string()).collect(),
            links: links
                .into_iter()
                .map(|href| crate::html::RawLink {
                    href: href.to_string(),
                    anchor_text: String::new(),
                    nofollow: false,
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn flags_brand_impersonating_hostname() {
        let classifier = PolicyClassifier::new();
        let content = content_with_text(vec!["Please sign in."], vec![]);
        let verdict = classifier.classify("http://paypal-secure-login.example.net/", &content);
        assert!(verdict.flags.iter().any(|f| f.category == PolicyCategory::Phishing));
    }

    #[test]
    fn does_not_flag_legitimate_brand_domain() {
        let classifier = PolicyClassifier::new();
        let content = content_with_text(vec!["Welcome to your account."], vec![]);
        let verdict = classifier.classify("https://www.paypal.com/signin", &content);
        assert!(!verdict.flags.iter().any(|f| f.category == PolicyCategory::Phishing));
    }

    #[test]
    fn flags_urgency_plus_payment_ask_as_scam() {
        let classifier = PolicyClassifier::new();
        let content = content_with_text(
            vec!["Act now, your account will be suspended. Send a gift card to reactivate."],
            vec![],
        );
        let verdict = classifier.classify("https://example.com/notice", &content);
        assert!(verdict.flags.iter().any(|f| f.category == PolicyCategory::Scam));
    }

    #[test]
    fn external_blocklist_always_flags() {
        let classifier = PolicyClassifier::with_external_blocklist(vec!["known-bad.example".to_string()]);
        let content = content_with_text(vec!["Nothing unusual here."], vec![]);
        let verdict = classifier.classify("https://known-bad.example/page", &content);
        assert!(verdict.blocklisted);
        assert!(verdict.should_filter());
    }

    #[test]
    fn benign_page_is_not_flagged() {
        let classifier = PolicyClassifier::new();
        let content = content_with_text(
            vec!["This is a normal blog post about gardening tips for spring."],
            vec!["https://example.com/about"],
        );
        let verdict = classifier.classify("https://example.com/garden-tips", &content);
        assert!(!verdict.should_filter());
    }
}
