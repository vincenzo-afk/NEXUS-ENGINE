//! A relevance benchmark suite: judged queries with graded golden result
//! sets, standard IR metrics (NDCG, MRR, Precision@k), and a regression
//! harness that compares a ranking run against a saved baseline.
//!
//! "Perfect search" is a moving target that only improves if ranking
//! changes are actually measured against something repeatable — this
//! module is that something. It is deliberately independent of
//! `crate::search::engine`: it scores *any* ordered list of document IDs
//! against *any* set of judgments, so it can be pointed at a real search
//! run's output, at a `crate::vector`-only ranking, at a hybrid-merged
//! `crate::entity` list, or at a hand-constructed test case, without
//! needing to know how that ranking was produced.
//!
//! ## What's real here vs. what needs populating
//! The metrics (`ndcg_at_k`, `mrr`, `precision_at_k`) and the regression
//! comparison are complete, correct, standard IR formulas with unit
//! tests. The included [`sample_judged_queries`] fixture is a small,
//! genuinely hand-judged example set (five queries against short
//! invented documents) meant to demonstrate the format and give the test
//! suite something to run against out of the box — it is not a
//! substitute for a real judged corpus. A real deployment of this needs
//! actual queries and actual human relevance judgments for actual
//! indexed content, which by definition can't be manufactured generically
//! here; `JudgedQuery`/`RelevanceJudgment`'s job is to make recording
//! those judgments (by hand, or via a judgment-collection tool) and
//! running the suite against them straightforward once they exist.

use std::collections::HashMap;

/// A graded relevance judgment for one document against one query.
/// Grades follow the common 0-3 TREC-style scale: 0 = not relevant,
/// 1 = marginally relevant, 2 = relevant, 3 = highly relevant/perfect
/// answer. Any document not given a judgment is treated as grade 0 by
/// the metrics below (an unjudged document is assumed non-relevant,
/// standard practice for a small/curated judgment set rather than a
/// full pooled TREC-scale one).
pub type Grade = u8;

/// One benchmarked query: its text and the graded judgments for
/// documents relevant to it, keyed by document ID (a path, URL, or
/// synthetic ID — whatever a given ranking run uses to identify
/// documents).
#[derive(Debug, Clone)]
pub struct JudgedQuery {
    pub query: String,
    pub judgments: HashMap<String, Grade>,
}

impl JudgedQuery {
    pub fn new(query: impl Into<String>, judgments: Vec<(&str, Grade)>) -> Self {
        JudgedQuery {
            query: query.into(),
            judgments: judgments
                .into_iter()
                .map(|(id, g)| (id.to_string(), g))
                .collect(),
        }
    }

    fn grade_of(&self, doc_id: &str) -> Grade {
        self.judgments.get(doc_id).copied().unwrap_or(0)
    }

    /// The ideal ordering's judged grades, descending — used as the
    /// denominator ("ideal DCG") in NDCG.
    fn ideal_grades(&self) -> Vec<Grade> {
        let mut grades: Vec<Grade> = self.judgments.values().copied().collect();
        grades.sort_unstable_by(|a, b| b.cmp(a));
        grades
    }
}

/// Discounted Cumulative Gain at rank `k`: `sum((2^grade - 1) / log2(rank + 1))`
/// for the top `k` results, the standard graded-relevance DCG formula.
fn dcg_at_k(grades_in_rank_order: &[Grade], k: usize) -> f64 {
    grades_in_rank_order
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, &grade)| {
            let rank = i + 1;
            let gain = (2f64.powi(grade as i32)) - 1.0;
            let discount = (rank as f64 + 1.0).log2();
            gain / discount
        })
        .sum()
}

/// Normalized DCG at `k`: `dcg_at_k / ideal_dcg_at_k`, in `0.0..=1.0`
/// (1.0 = the ranking returned documents in exactly judged-best-first
/// order within the top `k`). Returns `1.0` if there are no relevant
/// (nonzero-grade) documents in the judgment set at all — nothing to
/// rank badly, vacuously perfect.
pub fn ndcg_at_k(results: &[String], query: &JudgedQuery, k: usize) -> f64 {
    let ideal = dcg_at_k(&query.ideal_grades(), k);
    if ideal <= 0.0 {
        return 1.0;
    }
    let grades: Vec<Grade> = results.iter().map(|id| query.grade_of(id)).collect();
    (dcg_at_k(&grades, k) / ideal).min(1.0)
}

/// Reciprocal rank of the first relevant (grade > 0) result: `1 / rank`,
/// or `0.0` if no relevant result appears at all. Mean Reciprocal Rank
/// (MRR) across queries is just the average of this over a query set.
pub fn reciprocal_rank(results: &[String], query: &JudgedQuery) -> f64 {
    for (i, id) in results.iter().enumerate() {
        if query.grade_of(id) > 0 {
            return 1.0 / (i as f64 + 1.0);
        }
    }
    0.0
}

/// Precision at `k`: the fraction of the top `k` results that are
/// relevant (grade > 0).
pub fn precision_at_k(results: &[String], query: &JudgedQuery, k: usize) -> f64 {
    let top_k = &results[..results.len().min(k)];
    if top_k.is_empty() {
        return 0.0;
    }
    let relevant = top_k.iter().filter(|id| query.grade_of(id) > 0).count();
    relevant as f64 / top_k.len() as f64
}

/// Aggregate scores across an entire query set, produced by
/// [`run_suite`]. `k` is the cutoff used for `mean_ndcg`/`mean_precision`.
#[derive(Debug, Clone, PartialEq)]
pub struct SuiteScores {
    pub k: usize,
    pub mean_ndcg: f64,
    pub mean_reciprocal_rank: f64,
    pub mean_precision: f64,
    pub query_count: usize,
}

/// `search_fn` should take a query string and return an ordered list of
/// document IDs (best-first) exactly as a live ranking run would.
/// Keeping this a plain closure (rather than requiring callers to
/// implement a trait against `crate::search::engine`'s concrete types)
/// is what makes this usable against any of the ranking layers described
/// in the module doc comment.
pub fn run_suite(
    queries: &[JudgedQuery],
    k: usize,
    mut search_fn: impl FnMut(&str) -> Vec<String>,
) -> SuiteScores {
    let mut ndcg_sum = 0.0;
    let mut rr_sum = 0.0;
    let mut precision_sum = 0.0;

    for query in queries {
        let results = search_fn(&query.query);
        ndcg_sum += ndcg_at_k(&results, query, k);
        rr_sum += reciprocal_rank(&results, query);
        precision_sum += precision_at_k(&results, query, k);
    }

    let n = queries.len().max(1) as f64;
    SuiteScores {
        k,
        mean_ndcg: ndcg_sum / n,
        mean_reciprocal_rank: rr_sum / n,
        mean_precision: precision_sum / n,
        query_count: queries.len(),
    }
}

/// Compares a new [`SuiteScores`] run against a previously saved
/// baseline, flagging a regression if any metric drops by more than
/// `tolerance` (absolute). This is meant to run in CI: save a baseline
/// after a ranking change is reviewed and accepted, then fail the build
/// if a later change regresses relevance beyond the tolerance.
pub struct RegressionCheck {
    pub tolerance: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RegressionReport {
    pub passed: bool,
    pub ndcg_delta: f64,
    pub mrr_delta: f64,
    pub precision_delta: f64,
}

impl RegressionCheck {
    pub fn compare(&self, baseline: &SuiteScores, current: &SuiteScores) -> RegressionReport {
        let ndcg_delta = current.mean_ndcg - baseline.mean_ndcg;
        let mrr_delta = current.mean_reciprocal_rank - baseline.mean_reciprocal_rank;
        let precision_delta = current.mean_precision - baseline.mean_precision;
        let passed = ndcg_delta >= -self.tolerance
            && mrr_delta >= -self.tolerance
            && precision_delta >= -self.tolerance;
        RegressionReport {
            passed,
            ndcg_delta,
            mrr_delta,
            precision_delta,
        }
    }
}

/// A small, genuinely hand-judged example query set — see the module doc
/// comment for why this is a demonstration fixture, not a real benchmark
/// corpus.
pub fn sample_judged_queries() -> Vec<JudgedQuery> {
    vec![
        JudgedQuery::new(
            "rust ownership borrowing",
            vec![
                ("doc-rust-ownership-guide", 3),
                ("doc-rust-borrow-checker-explained", 3),
                ("doc-rust-general-syntax", 1),
                ("doc-python-tutorial", 0),
            ],
        ),
        JudgedQuery::new(
            "how to boil an egg",
            vec![
                ("doc-boiled-egg-times", 3),
                ("doc-egg-nutrition-facts", 1),
                ("doc-omelette-recipe", 1),
            ],
        ),
        JudgedQuery::new(
            "pagerank algorithm",
            vec![
                ("doc-pagerank-original-paper-summary", 3),
                ("doc-graph-theory-intro", 2),
                ("doc-web-crawling-basics", 1),
            ],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn perfect_ranking_scores_ndcg_one() {
        let query = JudgedQuery::new("q", vec![("a", 3), ("b", 2), ("c", 1)]);
        let results = ids(&["a", "b", "c"]);
        assert!((ndcg_at_k(&results, &query, 3) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn reversed_ranking_scores_lower_ndcg() {
        let query = JudgedQuery::new("q", vec![("a", 3), ("b", 2), ("c", 1)]);
        let perfect = ndcg_at_k(&ids(&["a", "b", "c"]), &query, 3);
        let reversed = ndcg_at_k(&ids(&["c", "b", "a"]), &query, 3);
        assert!(reversed < perfect);
    }

    #[test]
    fn reciprocal_rank_finds_first_relevant() {
        let query = JudgedQuery::new("q", vec![("a", 0), ("b", 2)]);
        let results = ids(&["a", "b"]);
        assert!((reciprocal_rank(&results, &query) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn reciprocal_rank_zero_when_nothing_relevant_returned() {
        let query = JudgedQuery::new("q", vec![("a", 3)]);
        let results = ids(&["z", "y"]);
        assert_eq!(reciprocal_rank(&results, &query), 0.0);
    }

    #[test]
    fn precision_at_k_counts_relevant_in_top_k() {
        let query = JudgedQuery::new("q", vec![("a", 2), ("b", 0), ("c", 1)]);
        let results = ids(&["a", "b", "c"]);
        assert!((precision_at_k(&results, &query, 2) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn run_suite_averages_across_queries() {
        let queries = sample_judged_queries();
        // A trivial "search function" that just returns the ideal order
        // for each query, so the suite should score ~1.0 everywhere.
        let scores = run_suite(&queries, 5, |q| {
            let query = queries.iter().find(|jq| jq.query == q).unwrap();
            let mut ranked: Vec<(String, Grade)> = query
                .judgments
                .iter()
                .map(|(id, g)| (id.clone(), *g))
                .collect();
            ranked.sort_by(|a, b| b.1.cmp(&a.1));
            ranked.into_iter().map(|(id, _)| id).collect()
        });
        assert!(scores.mean_ndcg > 0.99);
        assert!(scores.mean_reciprocal_rank > 0.99);
    }

    #[test]
    fn regression_check_flags_a_drop_beyond_tolerance() {
        let baseline = SuiteScores {
            k: 10,
            mean_ndcg: 0.80,
            mean_reciprocal_rank: 0.75,
            mean_precision: 0.6,
            query_count: 10,
        };
        let regressed = SuiteScores {
            mean_ndcg: 0.65,
            ..baseline.clone()
        };
        let check = RegressionCheck { tolerance: 0.05 };
        let report = check.compare(&baseline, &regressed);
        assert!(!report.passed);
        assert!(report.ndcg_delta < 0.0);
    }

    #[test]
    fn regression_check_passes_within_tolerance() {
        let baseline = SuiteScores {
            k: 10,
            mean_ndcg: 0.80,
            mean_reciprocal_rank: 0.75,
            mean_precision: 0.6,
            query_count: 10,
        };
        let slightly_lower = SuiteScores {
            mean_ndcg: 0.78,
            ..baseline.clone()
        };
        let check = RegressionCheck { tolerance: 0.05 };
        assert!(check.compare(&baseline, &slightly_lower).passed);
    }
}
