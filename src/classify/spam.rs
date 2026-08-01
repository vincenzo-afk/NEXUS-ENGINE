//! Heuristic detection of doorway pages, thin affiliate content, keyword
//! farms, and repetitive AI-generated filler. See the module-level doc
//! comment on `crate::classify` for why this is rule-based rather than a
//! trained model.

use crate::html::ExtractedContent;
use std::collections::HashMap;

/// One named signal contributing to a [`SpamVerdict`], kept alongside the
/// final score so a person (or the CLI's `--explain` output, if wired up)
/// can see *why* something was flagged rather than trusting a bare number.
#[derive(Debug, Clone)]
pub struct SpamSignal {
    pub name: &'static str,
    /// 0.0 (looks fine) to 1.0 (strongly indicates spam) for this signal.
    pub score: f32,
    pub detail: String,
}

/// The combined result of running every signal against one page.
#[derive(Debug, Clone)]
pub struct SpamVerdict {
    /// Weighted-average spam likelihood, `0.0..=1.0`. Above
    /// [`SUPPRESS_THRESHOLD`] is a strong suppression candidate.
    pub score: f32,
    pub signals: Vec<SpamSignal>,
}

impl SpamVerdict {
    pub fn should_suppress(&self) -> bool {
        self.score >= SUPPRESS_THRESHOLD
    }
}

/// Score at/above which [`SpamVerdict::should_suppress`] returns `true`.
/// Deliberately conservative (higher = fewer false positives suppressing
/// legitimate content) since a false suppression is worse than a missed
/// spam page still showing up ranked low.
pub const SUPPRESS_THRESHOLD: f32 = 0.72;

/// Runs every heuristic against one page's extracted content and its
/// resolved outbound-link hosts, returning a combined verdict.
pub struct SpamClassifier;

impl SpamClassifier {
    pub fn classify(content: &ExtractedContent, page_host: &str) -> SpamVerdict {
        let text = content.indexable_text();
        let signals = vec![
            Self::thin_content(&text),
            Self::keyword_stuffing(&text, &content.title),
            Self::doorway_page(content, &text),
            Self::affiliate_link_density(content, page_host),
            Self::ai_junk_repetition(&text),
            Self::boilerplate_ratio(content),
        ];

        let score = signals.iter().map(|s| s.score).sum::<f32>() / signals.len() as f32;
        SpamVerdict { score, signals }
    }

    /// Very short body text padded out with a long title/heading list is
    /// the classic "thin content" shape: enough structure to look like a
    /// real page, not enough substance to be one.
    fn thin_content(text: &str) -> SpamSignal {
        let word_count = text.split_whitespace().count();
        let score = if word_count < 50 {
            0.85
        } else if word_count < 150 {
            0.5
        } else if word_count < 300 {
            0.2
        } else {
            0.0
        };
        SpamSignal {
            name: "thin_content",
            score,
            detail: format!("{word_count} words of body text"),
        }
    }

    /// Repeating the same term far more than natural prose would (a
    /// keyword farm padding a target phrase for ranking rather than
    /// readability) is measured via max single-term frequency as a share
    /// of total words.
    fn keyword_stuffing(text: &str, title: &str) -> SpamSignal {
        let normalized = crate::text::normalize(text);
        let tokens = crate::text::tokenize(&normalized);
        if tokens.len() < 20 {
            return SpamSignal {
                name: "keyword_stuffing",
                score: 0.0,
                detail: "too little text to assess".to_string(),
            };
        }
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for t in &tokens {
            *counts.entry(t.text.as_str()).or_insert(0) += 1;
        }
        let max_freq = counts.values().copied().max().unwrap_or(0);
        let ratio = max_freq as f32 / tokens.len() as f32;
        // Natural English prose rarely puts any single non-stopword term
        // above ~3-4% of total tokens; repeated title phrases pushing
        // past 8% is a strong stuffing signal.
        let score = ((ratio - 0.03) / 0.10).clamp(0.0, 1.0);
        SpamSignal {
            name: "keyword_stuffing",
            score,
            detail: format!(
                "max term frequency {:.1}% (title: {})",
                ratio * 100.0,
                title
            ),
        }
    }

    /// A "doorway page" funnels a visitor toward one dominant outbound
    /// destination (an affiliate link, a redirect target) rather than
    /// being a destination itself: very few paragraphs of original text
    /// relative to a large outbound link count is the shape to catch.
    fn doorway_page(content: &ExtractedContent, text: &str) -> SpamSignal {
        let word_count = text.split_whitespace().count().max(1);
        let link_count = content.links.len();
        let words_per_link = word_count as f32 / link_count.max(1) as f32;
        let score = if link_count >= 5 && words_per_link < 15.0 {
            0.8
        } else if link_count >= 10 && words_per_link < 30.0 {
            0.5
        } else {
            0.0
        };
        SpamSignal {
            name: "doorway_page",
            score,
            detail: format!("{link_count} links, {words_per_link:.0} words/link"),
        }
    }

    /// Thin affiliate content: a high share of outbound links pointing at
    /// hosts other than the page's own domain, combined with thin body
    /// text, is the "this page exists to route clicks" pattern. This
    /// doesn't try to maintain a list of known affiliate networks (that
    /// list would rot immediately); it looks at the *shape* instead.
    fn affiliate_link_density(content: &ExtractedContent, page_host: &str) -> SpamSignal {
        if content.links.is_empty() {
            return SpamSignal {
                name: "affiliate_link_density",
                score: 0.0,
                detail: "no outbound links".to_string(),
            };
        }
        let external = content
            .links
            .iter()
            .filter(|l| {
                url::Url::parse(&l.href)
                    .ok()
                    .and_then(|u| u.host_str().map(|h| h.to_string()))
                    .map(|h| !page_host.is_empty() && h != page_host)
                    .unwrap_or(false)
            })
            .count();
        let ratio = external as f32 / content.links.len() as f32;
        let score = if ratio > 0.8 && content.links.len() >= 8 {
            0.7
        } else if ratio > 0.6 {
            0.35
        } else {
            0.0
        };
        SpamSignal {
            name: "affiliate_link_density",
            score,
            detail: format!("{:.0}% of links are external", ratio * 100.0),
        }
    }

    /// Fluent, generic, low-information filler (a common AI-generated-junk
    /// shape) tends to repeat sentence *structures* even when it varies
    /// vocabulary — measured here via a cheap proxy: the ratio of unique
    /// trigrams to total trigrams (low = highly repetitive phrasing
    /// patterns, whether from templating or generative padding).
    fn ai_junk_repetition(text: &str) -> SpamSignal {
        let normalized = crate::text::normalize(text);
        let tokens: Vec<String> = crate::text::tokenize(&normalized)
            .into_iter()
            .map(|t| t.text)
            .collect();
        if tokens.len() < 30 {
            return SpamSignal {
                name: "ai_junk_repetition",
                score: 0.0,
                detail: "too little text to assess".to_string(),
            };
        }
        let trigrams: Vec<String> = tokens
            .windows(3)
            .map(|w| w.join(" "))
            .collect();
        let unique: std::collections::HashSet<&String> = trigrams.iter().collect();
        let uniqueness = unique.len() as f32 / trigrams.len().max(1) as f32;
        // Natural varied prose is usually >0.85 unique trigrams; heavily
        // templated/repetitive filler drops well below that.
        let score = ((0.75 - uniqueness) / 0.4).clamp(0.0, 1.0);
        SpamSignal {
            name: "ai_junk_repetition",
            score,
            detail: format!("{:.0}% unique trigrams", uniqueness * 100.0),
        }
    }

    /// A page that's almost entirely nav/footer/ad chrome around a
    /// sliver of paragraph text reads as boilerplate-heavy (common in
    /// programmatic/scraped SEO pages) rather than an authored page.
    fn boilerplate_ratio(content: &ExtractedContent) -> SpamSignal {
        let paragraph_words: usize = content
            .paragraphs
            .iter()
            .map(|p| p.split_whitespace().count())
            .sum();
        let heading_words: usize = content
            .headings
            .iter()
            .map(|h| h.split_whitespace().count())
            .sum();
        let total = paragraph_words + heading_words;
        let score = if total == 0 {
            0.6
        } else if paragraph_words * 4 < total {
            // Headings dominate over actual paragraph prose.
            0.4
        } else {
            0.0
        };
        SpamSignal {
            name: "boilerplate_ratio",
            score,
            detail: format!("{paragraph_words} paragraph words, {heading_words} heading words"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(title: &str, paragraphs: Vec<&str>, links: Vec<&str>) -> ExtractedContent {
        ExtractedContent {
            title: title.to_string(),
            paragraphs: paragraphs.into_iter().map(|s| s.to_string()).collect(),
            links: links
                .into_iter()
                .map(|href| crate::html::RawLink {
                    href: href.to_string(),
                    anchor_text: "click here".to_string(),
                    nofollow: false,
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn thin_doorway_page_scores_high() {
        let p = page(
            "Best Deals Best Deals Best Deals Click Here",
            vec!["Check out this amazing deal now."],
            vec![
                "https://affiliate1.example/a",
                "https://affiliate2.example/b",
                "https://affiliate3.example/c",
                "https://affiliate4.example/d",
                "https://affiliate5.example/e",
            ],
        );
        let verdict = SpamClassifier::classify(&p, "myshop.example");
        assert!(verdict.score > 0.3, "expected elevated score, got {}", verdict.score);
    }

    #[test]
    fn substantial_original_content_scores_low() {
        let paragraphs = vec![
            "Rust's ownership system tracks how memory is allocated and freed without a garbage collector.",
            "Each value has a single owner, and when that owner goes out of scope the value is dropped.",
            "Borrowing lets code read or mutate a value temporarily without taking ownership of it.",
            "The borrow checker enforces these rules entirely at compile time, adding no runtime cost.",
        ];
        let p = page("Understanding Ownership in Rust", paragraphs, vec!["https://myblog.example/about"]);
        let verdict = SpamClassifier::classify(&p, "myblog.example");
        assert!(verdict.score < SUPPRESS_THRESHOLD);
    }
}
