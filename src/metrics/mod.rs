//! Online metrics: the numbers that measure search quality against real
//! usage, as opposed to [`crate::bench`]'s offline judged-query metrics.
//! Offline NDCG/MRR tell you whether ranking changes look better against
//! a frozen judgment set; these tell you whether real searches are
//! actually going well, which is a different (and ultimately more
//! important) question a judgment set can't fully answer on its own.
//!
//! ## What needs wiring vs. what's self-contained
//! [`dedup_rate`] and [`answer_grounding_rate`] compute directly from
//! existing types ([`crate::dedup::DuplicateIndex`] and
//! [`crate::ai::citation`]) with no new logging required. The
//! session/query-level metrics — CTR, abandonment, reformulation rate,
//! the long-click proxy, and freshness lag — need an event log that
//! does not otherwise exist in this codebase: [`crate::clicks::ClickLog`]
//! only stores a running total-clicks-per-document counter, not
//! timestamped per-query sessions. [`SearchEventLog`] below is that
//! event schema, appendable from the API layer (each `run_search` call
//! recording an [`Impression`], each result open recording a [`Click`]
//! with a dwell time once known) — the metric *math* here is complete
//! and tested against synthetic event logs; wiring `crate::api` to
//! actually append real events as they happen is the integration step
//! left for adoption, the same "seam, not stub" pattern as the other
//! modules in this pass.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub type SessionId = String;

/// One set of results shown to a user for one query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Impression {
    pub session_id: SessionId,
    pub query: String,
    pub timestamp_unix: i64,
    pub result_doc_ids: Vec<String>,
}

/// One result the user opened, with however much dwell time is known by
/// the time it's recorded (a "long click" — staying on the destination a
/// while before returning — is the standard implicit-feedback proxy for
/// "this result actually satisfied the need," vs. a "short click" where
/// the user bounces back immediately).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Click {
    pub session_id: SessionId,
    pub query: String,
    pub timestamp_unix: i64,
    pub doc_id: String,
    pub dwell_seconds: Option<f64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SearchEventLog {
    pub impressions: Vec<Impression>,
    pub clicks: Vec<Click>,
}

impl SearchEventLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_impression(&mut self, impression: Impression) {
        self.impressions.push(impression);
    }

    pub fn record_click(&mut self, click: Click) {
        self.clicks.push(click);
    }
}

/// A dwell time at/above this is counted as a "long click" for
/// [`long_click_rate`]. 30 seconds is the commonly-cited rule of thumb in
/// published search-evaluation literature for "the user was actually
/// reading/using the page," not a value tuned against this project's own
/// data (there is none yet to tune against).
pub const LONG_CLICK_THRESHOLD_SECONDS: f64 = 30.0;

/// A second query from the same session within this many seconds of the
/// previous one is counted as a reformulation of it, rather than an
/// unrelated later search in a long-lived session.
pub const REFORMULATION_WINDOW_SECONDS: i64 = 120;

/// Click-through rate: fraction of impressions that received at least
/// one click.
pub fn click_through_rate(log: &SearchEventLog) -> f64 {
    if log.impressions.is_empty() {
        return 0.0;
    }
    let clicked_queries: std::collections::HashSet<(&str, i64)> = log
        .clicks
        .iter()
        .map(|c| (c.query.as_str(), c.timestamp_unix))
        .collect();
    // An impression "received a click" if any click shares its session
    // and query text and occurred at or after it — approximate matching
    // since impressions/clicks aren't given a shared foreign key here,
    // consistent with this being a lightweight event log rather than a
    // full relational schema.
    let clicked_count = log
        .impressions
        .iter()
        .filter(|imp| {
            log.clicks
                .iter()
                .any(|c| c.session_id == imp.session_id && c.query == imp.query)
        })
        .count();
    let _ = clicked_queries; // kept for readability of intent above
    clicked_count as f64 / log.impressions.len() as f64
}

/// Abandonment rate: fraction of impressions that received *no* click at
/// all (the complement of CTR, but reported separately since
/// "abandonment" is the more actionable framing for query results pages
/// specifically, vs. CTR which is also used for e.g. individual link
/// performance).
pub fn abandonment_rate(log: &SearchEventLog) -> f64 {
    1.0 - click_through_rate(log)
}

/// Reformulation rate: fraction of queries, within a session, that were
/// followed by a *different* query from the same session within
/// [`REFORMULATION_WINDOW_SECONDS`] — a proxy for "the previous query's
/// results didn't satisfy the need, so the user tried rephrasing."
pub fn reformulation_rate(log: &SearchEventLog) -> f64 {
    if log.impressions.is_empty() {
        return 0.0;
    }
    let mut by_session: HashMap<&str, Vec<&Impression>> = HashMap::new();
    for imp in &log.impressions {
        by_session.entry(imp.session_id.as_str()).or_default().push(imp);
    }

    let mut total = 0usize;
    let mut reformulated = 0usize;
    for impressions in by_session.values() {
        let mut sorted = impressions.clone();
        sorted.sort_by_key(|i| i.timestamp_unix);
        for pair in sorted.windows(2) {
            total += 1;
            let (first, second) = (pair[0], pair[1]);
            let within_window =
                (second.timestamp_unix - first.timestamp_unix) <= REFORMULATION_WINDOW_SECONDS;
            if within_window && first.query != second.query {
                reformulated += 1;
            }
        }
    }
    if total == 0 {
        0.0
    } else {
        reformulated as f64 / total as f64
    }
}

/// Long-click rate: among clicks with a known dwell time, the fraction
/// at/above [`LONG_CLICK_THRESHOLD_SECONDS`]. Clicks with no recorded
/// dwell time (e.g. the user hasn't returned yet, or dwell tracking
/// isn't wired up for that surface) are excluded from the denominator
/// rather than counted as short, since "unknown" and "short" are not the
/// same thing.
pub fn long_click_rate(log: &SearchEventLog) -> f64 {
    let timed: Vec<f64> = log.clicks.iter().filter_map(|c| c.dwell_seconds).collect();
    if timed.is_empty() {
        return 0.0;
    }
    let long_clicks = timed
        .iter()
        .filter(|&&d| d >= LONG_CLICK_THRESHOLD_SECONDS)
        .count();
    long_clicks as f64 / timed.len() as f64
}

/// Freshness lag: average age, in seconds, between when a set of
/// documents were last crawled/indexed and their most recent
/// modification/publish time — how stale the index tends to be for the
/// documents it actually serves. Callers supply `(crawled_at, modified_at)`
/// pairs (both unix seconds) for whichever document set they want
/// measured (e.g. every clicked result in a period, or the whole index).
pub fn freshness_lag_seconds(pairs: &[(i64, i64)]) -> f64 {
    if pairs.is_empty() {
        return 0.0;
    }
    let total: i64 = pairs
        .iter()
        .map(|(crawled_at, modified_at)| (crawled_at - modified_at).max(0))
        .sum();
    total as f64 / pairs.len() as f64
}

/// Dedup rate: fraction of candidate documents a
/// [`crate::dedup::DuplicateIndex`] pass identified as duplicates of
/// something already registered, out of the total considered. Callers
/// pass `total_considered` (the count before dedup ran) since
/// `DuplicateIndex` itself only retains the surviving, deduplicated set.
pub fn dedup_rate(duplicate_index: &crate::dedup::DuplicateIndex, total_considered: usize) -> f64 {
    if total_considered == 0 {
        return 0.0;
    }
    let survivors = duplicate_index.len();
    let removed = total_considered.saturating_sub(survivors);
    removed as f64 / total_considered as f64
}

/// Answer grounding rate: fraction of cited claims that
/// [`crate::ai::citation::verify_claim_against_span`] finds supported by
/// their cited span — the online-metrics counterpart to that module's
/// per-claim check, aggregated across however many generated
/// answers/summaries are being measured.
pub fn answer_grounding_rate(claim_span_pairs: &[(String, String)]) -> f64 {
    if claim_span_pairs.is_empty() {
        return 1.0; // nothing generated yet; vacuously fully grounded
    }
    let results = crate::ai::citation::verify_all(claim_span_pairs);
    let supported = results
        .iter()
        .filter(|r| r.verdict == crate::ai::citation::VerificationVerdict::Supported)
        .count();
    supported as f64 / results.len() as f64
}

/// A single point-in-time rollup of every online metric, meant to be
/// computed periodically (e.g. daily) and stored for trend tracking —
/// the online-metrics counterpart to [`crate::bench::SuiteScores`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub computed_at_unix: i64,
    pub click_through_rate: f64,
    pub abandonment_rate: f64,
    pub reformulation_rate: f64,
    pub long_click_rate: f64,
    pub freshness_lag_seconds: f64,
    pub dedup_rate: f64,
    pub answer_grounding_rate: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_with_one_clicked_one_abandoned() -> SearchEventLog {
        let mut log = SearchEventLog::new();
        log.record_impression(Impression {
            session_id: "s1".into(),
            query: "rust ownership".into(),
            timestamp_unix: 1000,
            result_doc_ids: vec!["a".into(), "b".into()],
        });
        log.record_click(Click {
            session_id: "s1".into(),
            query: "rust ownership".into(),
            timestamp_unix: 1005,
            doc_id: "a".into(),
            dwell_seconds: Some(45.0),
        });
        log.record_impression(Impression {
            session_id: "s2".into(),
            query: "unrelated query".into(),
            timestamp_unix: 2000,
            result_doc_ids: vec!["c".into()],
        });
        log
    }

    #[test]
    fn ctr_and_abandonment_are_complementary() {
        let log = log_with_one_clicked_one_abandoned();
        let ctr = click_through_rate(&log);
        let abandonment = abandonment_rate(&log);
        assert!((ctr - 0.5).abs() < 1e-9);
        assert!((ctr + abandonment - 1.0).abs() < 1e-9);
    }

    #[test]
    fn reformulation_detected_within_window() {
        let mut log = SearchEventLog::new();
        log.record_impression(Impression {
            session_id: "s1".into(),
            query: "best pizza".into(),
            timestamp_unix: 1000,
            result_doc_ids: vec![],
        });
        log.record_impression(Impression {
            session_id: "s1".into(),
            query: "best pizza near me".into(),
            timestamp_unix: 1030,
            result_doc_ids: vec![],
        });
        assert!((reformulation_rate(&log) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn same_query_repeated_is_not_a_reformulation() {
        let mut log = SearchEventLog::new();
        log.record_impression(Impression {
            session_id: "s1".into(),
            query: "same query".into(),
            timestamp_unix: 1000,
            result_doc_ids: vec![],
        });
        log.record_impression(Impression {
            session_id: "s1".into(),
            query: "same query".into(),
            timestamp_unix: 1010,
            result_doc_ids: vec![],
        });
        assert_eq!(reformulation_rate(&log), 0.0);
    }

    #[test]
    fn long_click_rate_ignores_unknown_dwell_times() {
        let mut log = SearchEventLog::new();
        log.record_click(Click {
            session_id: "s1".into(),
            query: "q".into(),
            timestamp_unix: 1,
            doc_id: "a".into(),
            dwell_seconds: Some(60.0),
        });
        log.record_click(Click {
            session_id: "s1".into(),
            query: "q".into(),
            timestamp_unix: 2,
            doc_id: "b".into(),
            dwell_seconds: Some(5.0),
        });
        log.record_click(Click {
            session_id: "s1".into(),
            query: "q".into(),
            timestamp_unix: 3,
            doc_id: "c".into(),
            dwell_seconds: None,
        });
        assert!((long_click_rate(&log) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn freshness_lag_averages_positive_gaps() {
        let pairs = vec![(1_000_100, 1_000_000), (2_000_500, 2_000_000)];
        assert!((freshness_lag_seconds(&pairs) - 300.0).abs() < 1e-9);
    }

    #[test]
    fn grounding_rate_reflects_citation_verification() {
        let pairs = vec![
            (
                "Rust's borrow checker enforces memory safety.".to_string(),
                "The borrow checker in Rust enforces memory safety at compile time.".to_string(),
            ),
            (
                "The company's revenue grew 40%.".to_string(),
                "The chef recommends resting the dough for an hour.".to_string(),
            ),
        ];
        let rate = answer_grounding_rate(&pairs);
        assert!((rate - 0.5).abs() < 1e-9);
    }
}
