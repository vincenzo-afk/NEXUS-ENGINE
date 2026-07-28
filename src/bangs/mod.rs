//! `!bang` shortcuts, DuckDuckGo-style: a query containing a recognized
//! `!trigger` token redirects straight to that site's own search instead
//! of running against the local index — useful for the (very common)
//! case where the person actually wants "search YouTube for X", not
//! "search whatever Nexus has indexed for X".
//!
//! The bang can appear anywhere in the query (`!yt rust talks` and
//! `rust talks !yt` both work), matching how DuckDuckGo's bangs behave,
//! since people don't reliably type it in the same position every time.

use std::collections::HashMap;

/// One recognized bang shortcut.
#[derive(Debug, Clone, Copy)]
pub struct Bang {
    /// The trigger word, without the leading `!` (e.g. `"g"`).
    pub trigger: &'static str,
    /// A human-readable name for the target site, shown in UI/CLI output.
    pub name: &'static str,
    /// The target URL template; `{}` is replaced with the URL-encoded
    /// remaining query.
    pub url_template: &'static str,
}

/// The built-in bang table. Deliberately a short, curated set rather than
/// DuckDuckGo's thousands — these are the ones a search-engine project's
/// own users are overwhelmingly likely to want, and a short list is easier
/// to keep meaningful and correct than an exhaustive one. See
/// [`crate::config::PrivacyConfig`] (or wherever bangs end up in config,
/// if extended) for adding custom ones without a code change.
pub const BUILTIN_BANGS: &[Bang] = &[
    Bang { trigger: "g", name: "Google", url_template: "https://www.google.com/search?q={}" },
    Bang { trigger: "ddg", name: "DuckDuckGo", url_template: "https://duckduckgo.com/?q={}" },
    Bang { trigger: "b", name: "Bing", url_template: "https://www.bing.com/search?q={}" },
    Bang { trigger: "w", name: "Wikipedia", url_template: "https://en.wikipedia.org/wiki/Special:Search?search={}" },
    Bang { trigger: "yt", name: "YouTube", url_template: "https://www.youtube.com/results?search_query={}" },
    Bang { trigger: "gh", name: "GitHub", url_template: "https://github.com/search?q={}" },
    Bang { trigger: "so", name: "Stack Overflow", url_template: "https://stackoverflow.com/search?q={}" },
    Bang { trigger: "r", name: "Reddit", url_template: "https://www.reddit.com/search/?q={}" },
    Bang { trigger: "npm", name: "npm", url_template: "https://www.npmjs.com/search?q={}" },
    Bang { trigger: "crates", name: "crates.io", url_template: "https://crates.io/search?q={}" },
    Bang { trigger: "docsrs", name: "docs.rs", url_template: "https://docs.rs/releases/search?query={}" },
    Bang { trigger: "mdn", name: "MDN Web Docs", url_template: "https://developer.mozilla.org/en-US/search?q={}" },
    Bang { trigger: "tw", name: "Twitter/X", url_template: "https://twitter.com/search?q={}" },
    Bang { trigger: "az", name: "Amazon", url_template: "https://www.amazon.com/s?k={}" },
    Bang { trigger: "maps", name: "Google Maps", url_template: "https://www.google.com/maps/search/{}" },
    Bang { trigger: "img", name: "Google Images", url_template: "https://www.google.com/search?tbm=isch&q={}" },
    Bang { trigger: "arxiv", name: "arXiv", url_template: "https://arxiv.org/abs/{}" },
    Bang { trigger: "wa", name: "Wolfram Alpha", url_template: "https://www.wolframalpha.com/input?i={}" },
];

/// The result of resolving a bang: where to redirect, and what's left of
/// the query once the bang token is removed.
#[derive(Debug, Clone, PartialEq)]
pub struct BangMatch {
    /// The fully resolved target URL, with the remaining query URL-encoded
    /// and substituted in.
    pub url: String,
    /// The target site's human-readable name.
    pub name: String,
    /// The query with the `!trigger` token removed and whitespace
    /// collapsed.
    pub remaining_query: String,
}

/// A bang table, supporting the built-ins plus optional user-defined
/// overrides/additions (e.g. from a `[bangs]` section in `config.toml`).
#[derive(Debug, Clone)]
pub struct BangTable {
    bangs: HashMap<String, Bang>,
}

impl Default for BangTable {
    fn default() -> Self {
        BangTable::from_builtin()
    }
}

impl BangTable {
    /// Builds a table containing just the built-in bangs.
    pub fn from_builtin() -> Self {
        let bangs = BUILTIN_BANGS
            .iter()
            .map(|b| (b.trigger.to_string(), *b))
            .collect();
        BangTable { bangs }
    }

    /// Registers or overrides a custom bang, e.g. from user configuration.
    /// `name` and `url_template` are leaked to get a `'static` lifetime
    /// matching [`Bang`]'s fields; this is fine for the small, bounded
    /// number of custom bangs a config file would realistically define
    /// (it's a one-time cost at startup, not a per-search allocation).
    pub fn add_custom(&mut self, trigger: &str, name: &str, url_template: &str) {
        let bang = Bang {
            trigger: Box::leak(trigger.to_lowercase().into_boxed_str()),
            name: Box::leak(name.to_string().into_boxed_str()),
            url_template: Box::leak(url_template.to_string().into_boxed_str()),
        };
        self.bangs.insert(bang.trigger.to_string(), bang);
    }

    /// Scans `query` for a `!trigger` token anywhere in it. If one matches
    /// a known bang, returns the resolved redirect target and the query
    /// with that token removed. Returns `None` if no recognized bang is
    /// present (an unrecognized `!something` is left as an ordinary
    /// search term — it's not an error, just not a bang).
    pub fn resolve(&self, query: &str) -> Option<BangMatch> {
        let words: Vec<&str> = query.split_whitespace().collect();
        let bang_index = words.iter().position(|w| {
            w.starts_with('!')
                && w.len() > 1
                && self.bangs.contains_key(&w[1..].to_lowercase())
        })?;

        let bang = self.bangs.get(&words[bang_index][1..].to_lowercase())?;
        let remaining_query: String = words
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != bang_index)
            .map(|(_, w)| *w)
            .collect::<Vec<_>>()
            .join(" ");

        let encoded = urlencoding::encode(&remaining_query);
        let url = bang.url_template.replace("{}", &encoded);

        Some(BangMatch {
            url,
            name: bang.name.to_string(),
            remaining_query,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_leading_bang() {
        let table = BangTable::from_builtin();
        let m = table.resolve("!g rust ownership").unwrap();
        assert_eq!(m.name, "Google");
        assert_eq!(m.remaining_query, "rust ownership");
        assert!(m.url.starts_with("https://www.google.com/search?q="));
        assert!(m.url.contains("rust%20ownership") || m.url.contains("rust+ownership"));
    }

    #[test]
    fn resolves_trailing_bang() {
        let table = BangTable::from_builtin();
        let m = table.resolve("rust talks !yt").unwrap();
        assert_eq!(m.name, "YouTube");
        assert_eq!(m.remaining_query, "rust talks");
    }

    #[test]
    fn resolves_bang_in_the_middle() {
        let table = BangTable::from_builtin();
        let m = table.resolve("borrow !so checker error").unwrap();
        assert_eq!(m.name, "Stack Overflow");
        assert_eq!(m.remaining_query, "borrow checker error");
    }

    #[test]
    fn is_case_insensitive() {
        let table = BangTable::from_builtin();
        let m = table.resolve("!G rust").unwrap();
        assert_eq!(m.name, "Google");
        let m2 = table.resolve("!GH nexus").unwrap();
        assert_eq!(m2.name, "GitHub");
    }

    #[test]
    fn unrecognized_bang_returns_none() {
        let table = BangTable::from_builtin();
        assert!(table.resolve("!notarealbang something").is_none());
    }

    #[test]
    fn query_without_bang_returns_none() {
        let table = BangTable::from_builtin();
        assert!(table.resolve("just a normal query").is_none());
    }

    #[test]
    fn bang_only_query_yields_empty_remaining_query() {
        let table = BangTable::from_builtin();
        let m = table.resolve("!w").unwrap();
        assert_eq!(m.remaining_query, "");
    }

    #[test]
    fn custom_bang_can_be_added_and_overrides_builtin() {
        let mut table = BangTable::from_builtin();
        table.add_custom("g", "My Custom Google Mirror", "https://example.com/search?q={}");
        let m = table.resolve("!g rust").unwrap();
        assert_eq!(m.name, "My Custom Google Mirror");
        assert!(m.url.starts_with("https://example.com/search?q="));
    }

    #[test]
    fn every_builtin_trigger_is_unique() {
        let mut seen = std::collections::HashSet::new();
        for bang in BUILTIN_BANGS {
            assert!(seen.insert(bang.trigger), "duplicate trigger: {}", bang.trigger);
        }
    }

    #[test]
    fn every_builtin_template_has_a_placeholder() {
        for bang in BUILTIN_BANGS {
            assert!(
                bang.url_template.contains("{}"),
                "bang '{}' has no {{}} placeholder",
                bang.trigger
            );
        }
    }
}
