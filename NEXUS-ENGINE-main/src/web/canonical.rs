//! URL canonicalization.
//!
//! Two URLs that point at "the same" resource should map to one canonical
//! form so the crawl queue, duplicate detector, and index don't treat
//! `http://Example.com/a`, `https://example.com/a/`, and
//! `https://example.com/a?utm_source=x#top` as three different pages.

use log::debug;
use url::Url;

/// Common tracking / session query parameters that carry no bearing on the
/// resource identity and are stripped during canonicalization.
const TRACKING_PARAMS: &[&str] = &[
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_term",
    "utm_content",
    "gclid",
    "fbclid",
    "msclkid",
    "ref",
    "referrer",
    "session_id",
    "sessionid",
    "phpsessid",
    "sid",
];

/// Canonicalizes `url` in place, returning the canonical form. Rules
/// applied (a practical subset of what production crawlers use):
///
/// * scheme and host are lower-cased,
/// * default ports (80 for http, 443 for https) are dropped,
/// * the fragment (`#...`) is removed, since it identifies a location
///   within a page, not a different resource,
/// * known tracking query parameters are removed and the remaining
///   parameters are sorted for a stable representation,
/// * a lone trailing slash on the path is removed (`/a/` -> `/a`) unless
///   the whole path is just `/`.
pub fn canonicalize(url: &Url) -> Url {
    let mut u = url.clone();
    u.set_fragment(None);

    let _ = u.set_scheme(&u.scheme().to_lowercase());
    if let Some(host) = u.host_str() {
        let lower = host.to_lowercase();
        let _ = u.set_host(Some(&lower));
    }

    match (u.scheme(), u.port()) {
        ("http", Some(80)) => {
            let _ = u.set_port(None);
        }
        ("https", Some(443)) => {
            let _ = u.set_port(None);
        }
        _ => {}
    }

    let mut pairs: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| !TRACKING_PARAMS.contains(&k.to_lowercase().as_str()))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    pairs.sort();
    if pairs.is_empty() {
        u.set_query(None);
    } else {
        let query: String = pairs
            .iter()
            .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        u.set_query(Some(&query));
    }

    let path = u.path().to_string();
    if path.len() > 1 && path.ends_with('/') {
        u.set_path(path.trim_end_matches('/'));
    }

    let original = url.as_str();
    let canonical = u.as_str();
    if original != canonical {
        debug!("canonicalized '{}' -> '{}'", original, canonical);
    }

    u
}

/// Parses and canonicalizes a URL string in one step.
pub fn parse_canonical(raw: &str) -> Option<Url> {
    Url::parse(raw).ok().map(|u| canonicalize(&u))
}

/// Resolves `href` (which may be relative, protocol-relative, or absolute)
/// against `base`, returning the canonicalized absolute URL. Returns
/// `None` for hrefs that can't reasonably be resolved (e.g. `mailto:`,
/// `javascript:`, or malformed values).
pub fn resolve(base: &Url, href: &str) -> Option<Url> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    let lower = href.to_lowercase();
    if lower.starts_with("mailto:")
        || lower.starts_with("javascript:")
        || lower.starts_with("tel:")
        || lower.starts_with("data:")
    {
        return None;
    }
    let resolved = base.join(href).ok()?;
    if resolved.scheme() != "http" && resolved.scheme() != "https" {
        return None;
    }
    Some(canonicalize(&resolved))
}

/// Returns the registrable "domain" used for per-domain rate limiting and
/// domain-quality scoring: the host, without a leading `www.`.
pub fn domain_of(url: &Url) -> String {
    let domain = url
        .host_str()
        .map(|h| h.strip_prefix("www.").unwrap_or(h).to_lowercase())
        .unwrap_or_default();
    debug!("domain_of '{}' -> '{}'", url.as_str(), domain);
    domain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_fragment_and_default_port() {
        let url = Url::parse("HTTP://Example.com:80/Path#section").unwrap();
        let canonical = canonicalize(&url);
        assert_eq!(canonical.as_str(), "http://example.com/Path");
    }

    #[test]
    fn strips_tracking_params_and_sorts_rest() {
        let url = Url::parse("https://example.com/a?b=2&utm_source=x&a=1").unwrap();
        let canonical = canonicalize(&url);
        assert_eq!(canonical.as_str(), "https://example.com/a?a=1&b=2");
    }

    #[test]
    fn strips_trailing_slash() {
        let url = Url::parse("https://example.com/a/").unwrap();
        assert_eq!(canonicalize(&url).as_str(), "https://example.com/a");
        let root = Url::parse("https://example.com/").unwrap();
        assert_eq!(canonicalize(&root).as_str(), "https://example.com/");
    }

    #[test]
    fn resolves_relative_links() {
        let base = Url::parse("https://example.com/blog/post-1").unwrap();
        assert_eq!(
            resolve(&base, "/about").unwrap().as_str(),
            "https://example.com/about"
        );
        assert_eq!(
            resolve(&base, "other-post").unwrap().as_str(),
            "https://example.com/blog/other-post"
        );
        assert!(resolve(&base, "mailto:a@b.com").is_none());
        assert!(resolve(&base, "javascript:void(0)").is_none());
    }

    #[test]
    fn domain_strips_www() {
        let url = Url::parse("https://www.example.com/a").unwrap();
        assert_eq!(domain_of(&url), "example.com");
    }
}
