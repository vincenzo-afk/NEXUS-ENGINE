//! robots.txt parsing and evaluation (RFC 9309, practical subset).
//!
//! Supports `User-agent`, `Allow`, `Disallow`, `Crawl-delay`, and
//! `Sitemap` directives, with the standard "most specific rule wins, ties
//! go to Allow" precedence, and falls back to the wildcard (`*`) group
//! when no rule group matches our user agent by name.

use log::{debug, warn};
use serde::{Deserialize, Serialize};

/// One `Allow`/`Disallow` rule within a user-agent group.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Rule {
    prefix: String,
    allow: bool,
}

/// One `User-agent: ...` group and the rules that apply to it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Group {
    agents: Vec<String>,
    rules: Vec<Rule>,
    crawl_delay: Option<f32>,
}

/// A parsed robots.txt document, ready to answer "can I fetch this path?"
/// for a specific user agent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RobotsTxt {
    groups: Vec<Group>,
    /// Every `Sitemap:` URL declared in the file, in declaration order.
    pub sitemaps: Vec<String>,
}

impl RobotsTxt {
    /// Returns a permissive [`RobotsTxt`] with no rules at all: used when
    /// robots.txt is missing (404) or fails to fetch, per the standard
    /// convention that crawling is then unrestricted.
    pub fn allow_all() -> Self {
        RobotsTxt::default()
    }

    /// Parses raw robots.txt text.
    pub fn parse(body: &str) -> RobotsTxt {
        debug!("parsing robots.txt ({} bytes)", body.len());
        let mut groups: Vec<Group> = Vec::new();
        let mut sitemaps = Vec::new();
        let mut current: Option<Group> = None;
        // True once a non-user-agent directive has been seen in the
        // current block, so a following `User-agent:` line starts a new
        // group rather than extending the current one (per spec, a run of
        // consecutive `User-agent` lines share one group).
        let mut group_started = false;

        for raw_line in body.lines() {
            let line = strip_comment(raw_line).trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim().to_lowercase();
            let value = value.trim();

            match key.as_str() {
                "user-agent" => {
                    if group_started || current.is_none() {
                        if let Some(g) = current.take() {
                            groups.push(g);
                        }
                        current = Some(Group::default());
                        group_started = false;
                    }
                    if let Some(g) = current.as_mut() {
                        g.agents.push(value.to_lowercase());
                    }
                }
                "disallow" => {
                    group_started = true;
                    if let Some(g) = current.as_mut() {
                        if !value.is_empty() {
                            g.rules.push(Rule {
                                prefix: value.to_string(),
                                allow: false,
                            });
                        } else {
                            // `Disallow:` with an empty value means "allow
                            // everything" for this group.
                            g.rules.push(Rule {
                                prefix: String::new(),
                                allow: true,
                            });
                        }
                    }
                }
                "allow" => {
                    group_started = true;
                    if let Some(g) = current.as_mut() {
                        g.rules.push(Rule {
                            prefix: value.to_string(),
                            allow: true,
                        });
                    }
                }
                "crawl-delay" => {
                    group_started = true;
                    if let Some(g) = current.as_mut() {
                        g.crawl_delay = value.parse::<f32>().ok();
                        if g.crawl_delay.is_none() {
                            warn!("failed to parse Crawl-delay value: '{}'", value);
                        }
                    }
                }
                "sitemap" => {
                    sitemaps.push(value.to_string());
                }
                _ => {}
            }
        }
        if let Some(g) = current.take() {
            groups.push(g);
        }

        RobotsTxt { groups, sitemaps }
    }

    /// Finds the most specific matching group for `user_agent`: an exact
    /// (case-insensitive) product-token match if one exists, otherwise the
    /// wildcard `*` group, otherwise `None`.
    fn group_for(&self, user_agent: &str) -> Option<&Group> {
        let agent_lower = user_agent.to_lowercase();
        let named = self.groups.iter().find(|g| {
            g.agents
                .iter()
                .any(|a| a != "*" && agent_lower.contains(a.as_str()))
        });
        named.or_else(|| {
            self.groups
                .iter()
                .find(|g| g.agents.iter().any(|a| a == "*"))
        })
    }

    /// Returns `true` if `path` (the request-target: path + optional
    /// query, no scheme/host) may be fetched by `user_agent`.
    ///
    /// Precedence: the rule with the longest matching prefix wins; if the
    /// longest-matching allow and disallow rules are the same length,
    /// `Allow` wins (the more permissive, standards-recommended tie-break).
    pub fn is_allowed(&self, user_agent: &str, path: &str) -> bool {
        let Some(group) = self.group_for(user_agent) else {
            debug!(
                "robots.txt: no matching group for '{}', allowing '{}'",
                user_agent, path
            );
            return true;
        };

        let mut best: Option<&Rule> = None;
        for rule in &group.rules {
            if path.starts_with(rule.prefix.as_str()) {
                match best {
                    None => best = Some(rule),
                    Some(b) if rule.prefix.len() > b.prefix.len() => best = Some(rule),
                    Some(b) if rule.prefix.len() == b.prefix.len() && rule.allow && !b.allow => {
                        best = Some(rule)
                    }
                    _ => {}
                }
            }
        }
        let allowed = best.map(|r| r.allow).unwrap_or(true);
        debug!(
            "robots.txt: {} for '{}' on '{}'",
            if allowed { "ALLOW" } else { "DISALLOW" },
            user_agent,
            path
        );
        allowed
    }

    /// The crawl-delay (in seconds) requested for `user_agent`, if any.
    pub fn crawl_delay(&self, user_agent: &str) -> Option<f32> {
        self.group_for(user_agent).and_then(|g| g.crawl_delay)
    }
}

fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(idx) => &line[..idx],
        None => line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "
User-agent: *
Disallow: /private/
Allow: /private/public-page.html
Crawl-delay: 2

User-agent: NexusBot
Disallow: /no-bots-allowed/
Sitemap: https://example.com/sitemap.xml
";

    #[test]
    fn wildcard_group_disallows_private() {
        let robots = RobotsTxt::parse(SAMPLE);
        assert!(!robots.is_allowed("SomeOtherBot/1.0", "/private/secret.html"));
        assert!(robots.is_allowed("SomeOtherBot/1.0", "/public/page.html"));
    }

    #[test]
    fn longest_prefix_wins_over_shorter_disallow() {
        let robots = RobotsTxt::parse(SAMPLE);
        assert!(robots.is_allowed("SomeOtherBot/1.0", "/private/public-page.html"));
    }

    #[test]
    fn named_agent_group_used_when_matching() {
        let robots = RobotsTxt::parse(SAMPLE);
        assert!(!robots.is_allowed("NexusBot/1.0", "/no-bots-allowed/x"));
        // NexusBot's group has no rule for /private/, so it is allowed
        // (named groups do not inherit the wildcard group's rules).
        assert!(robots.is_allowed("NexusBot/1.0", "/private/secret.html"));
    }

    #[test]
    fn crawl_delay_parsed() {
        let robots = RobotsTxt::parse(SAMPLE);
        assert_eq!(robots.crawl_delay("SomeOtherBot/1.0"), Some(2.0));
    }

    #[test]
    fn sitemaps_collected() {
        let robots = RobotsTxt::parse(SAMPLE);
        assert_eq!(robots.sitemaps, vec!["https://example.com/sitemap.xml"]);
    }

    #[test]
    fn missing_file_allows_everything() {
        let robots = RobotsTxt::allow_all();
        assert!(robots.is_allowed("NexusBot", "/anything"));
    }

    #[test]
    fn empty_disallow_value_allows_everything() {
        let robots = RobotsTxt::parse("User-agent: *\nDisallow:\n");
        assert!(robots.is_allowed("NexusBot", "/anything"));
    }
}
