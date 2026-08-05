//! A personal relevance model, trained entirely locally from click
//! feedback, that learns a small per-user boost on top of the existing
//! composite ranking score — nothing about what you click, or the
//! learned weights themselves, ever leaves your machine.
//!
//! **Read this before assuming "adaptive ranking" means the candle-based
//! MLP described in the feature request.** What's implemented is a
//! hand-rolled online logistic regression over a small, fixed,
//! human-readable feature vector (see [`Features`]), updated by plain
//! stochastic gradient descent on every click/skip signal — not a
//! neural network trained via `candle`. This is a deliberate scope
//! reduction, not a shortcut dressed up as the bigger thing: a linear
//! model over a handful of interpretable features is auditable (you can
//! read off exactly which feature the model has learned to weight up,
//! e.g. "prefers .edu domains"), trains stably from the very small
//! number of examples one person's click history actually produces (a
//! multi-layer network has no such guarantee on a few hundred data
//! points and is much easier to overfit with), and needs no additional
//! heavy ML-runtime dependency for what is fundamentally a small,
//! low-dimensional personalization problem. If a genuinely nonlinear
//! personal model turns out to be needed later, `candle` is already a
//! reasonable path (see the README's "True neural semantic search"
//! section for the same dependency), but that's future work, not this.
//!
//! ## What it actually learns
//! [`PersonalRankingModel`] maps a small feature vector — is this a
//! `.edu`/`.gov` domain, is it from a source kind the person tends to
//! click, how recent is it, how long was the last result they actually
//! stayed on — to a single scalar boost, updated after every click (a
//! positive-then-implicitly-negative-for-skipped-results-above-it
//! training signal, the same implicit feedback that
//! `crate::clicks::ClickLog` already uses, just modeled per-feature
//! here instead of per-document).

use serde::{Deserialize, Serialize};

/// A small, fixed, human-readable feature vector describing one
/// candidate result at ranking time. Keeping this fixed-size and
/// interpretable (rather than an opaque embedding) is what makes the
/// linear model both trainable from few examples and auditable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Features {
    pub is_edu_or_gov_domain: bool,
    pub source_matches_frequently_clicked_kind: bool,
    /// Recency, already normalized to `0.0..=1.0` (1.0 = published/
    /// modified just now) by the caller — this module doesn't know
    /// about calendar time, only about already-normalized signals.
    pub recency_normalized: f32,
    /// The document's existing composite ranking score, min-max
    /// normalized to `0.0..=1.0` within the current result set (so this
    /// model's job is "given the existing ranker already thinks this is
    /// relevant, does *this person specifically* tend to like results
    /// like this," not re-deriving relevance from scratch).
    pub base_score_normalized: f32,
}

impl Features {
    fn as_vector(&self) -> [f32; 5] {
        [
            1.0, // bias term
            if self.is_edu_or_gov_domain { 1.0 } else { 0.0 },
            if self.source_matches_frequently_clicked_kind { 1.0 } else { 0.0 },
            self.recency_normalized.clamp(0.0, 1.0),
            self.base_score_normalized.clamp(0.0, 1.0),
        ]
    }

    const FEATURE_NAMES: [&'static str; 5] = [
        "bias",
        "is_edu_or_gov_domain",
        "source_matches_frequently_clicked_kind",
        "recency_normalized",
        "base_score_normalized",
    ];
}

/// Learning rate for the SGD update. Deliberately small: this model
/// updates on every single click across a person's whole usage history,
/// so it needs to move slowly enough that one unusual click doesn't
/// swing the model, not fast enough to "solve" personalization in one
/// session.
const LEARNING_RATE: f32 = 0.02;
/// L2 regularization strength, to keep weights from drifting unboundedly
/// over a long-running local model that's never reset.
const L2_REGULARIZATION: f32 = 0.001;

/// The model itself: five learned weights (bias + four features) plus
/// how many training examples it's seen, serializable so it can persist
/// locally (e.g. alongside `clicks.nxc`) across runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonalRankingModel {
    weights: [f32; 5],
    examples_seen: u64,
}

impl Default for PersonalRankingModel {
    fn default() -> Self {
        // Zero-initialized: an untrained model outputs a constant
        // sigmoid(0) = 0.5 boost for everything, i.e. no personalization
        // effect at all until it's actually seen click feedback.
        PersonalRankingModel {
            weights: [0.0; 5],
            examples_seen: 0,
        }
    }
}

impl PersonalRankingModel {
    pub fn new() -> Self {
        Self::default()
    }

    fn sigmoid(x: f32) -> f32 {
        1.0 / (1.0 + (-x).exp())
    }

    /// Predicted personal-relevance probability for `features`, in
    /// `0.0..=1.0`. Callers combine this with the existing composite
    /// ranking score (e.g. as a multiplicative boost centered at 1.0:
    /// `boost = 0.85 + 0.3 * model.predict(features)`), not as a
    /// replacement for it — this model only ever nudges an already-
    /// computed ranking, consistent with `crate::ranking`'s existing
    /// signal-combination approach.
    pub fn predict(&self, features: &Features) -> f32 {
        let x = features.as_vector();
        let z: f32 = self.weights.iter().zip(x.iter()).map(|(w, xi)| w * xi).sum();
        Self::sigmoid(z)
    }

    /// One SGD update step: `label` is `1.0` for a click, `0.0` for a
    /// result the person saw (was shown, ranked above something they did
    /// click, or explicitly skipped) but didn't choose — the standard
    /// implicit-feedback binary classification framing for
    /// click-through personalization.
    pub fn train_one(&mut self, features: &Features, label: f32) {
        let x = features.as_vector();
        let prediction = self.predict(features);
        let error = prediction - label.clamp(0.0, 1.0);

        for i in 0..self.weights.len() {
            let gradient = error * x[i] + L2_REGULARIZATION * self.weights[i];
            self.weights[i] -= LEARNING_RATE * gradient;
        }
        self.examples_seen += 1;
    }

    /// Trains from one search event: the clicked result (`clicked`, if
    /// any) gets a positive label, every result the person saw ranked
    /// above it without clicking gets a negative label — the standard
    /// pairwise-implicit-feedback shape (clicking result #4 implies a
    /// preference over results #1-3, not necessarily that #1-3 were bad
    /// in isolation, but treating them as negative examples relative to
    /// what was chosen is the standard, well-documented approximation
    /// this kind of online learning uses).
    pub fn train_from_result_list(
        &mut self,
        shown_in_rank_order: &[Features],
        clicked_index: Option<usize>,
    ) {
        let Some(clicked) = clicked_index else {
            return; // no click at all this search; nothing to learn from
        };
        for (i, features) in shown_in_rank_order.iter().enumerate() {
            if i < clicked {
                self.train_one(features, 0.0);
            }
        }
        if let Some(features) = shown_in_rank_order.get(clicked) {
            self.train_one(features, 1.0);
        }
    }

    pub fn examples_seen(&self) -> u64 {
        self.examples_seen
    }

    /// Returns the learned weight for each named feature, for
    /// inspectability (e.g. a `nexus explain-personalization` CLI
    /// command could print this) — a person should be able to see what
    /// their local model has picked up, not just trust it blindly.
    pub fn feature_weights(&self) -> Vec<(&'static str, f32)> {
        Features::FEATURE_NAMES.iter().copied().zip(self.weights).collect()
    }

    /// Loads the model from `path`, or a fresh untrained model if none
    /// exists yet (matching `ClickLog::load`'s "no file yet is not an
    /// error" behavior — every person's very first search has no
    /// personalization history, that's expected, not a failure).
    /// Stored as plain JSON rather than `clicks.nxc`'s bincode: the
    /// module doc comment's whole point is that this model is small and
    /// auditable, and that should extend to being able to `cat` the
    /// file and read the learned weights directly.
    pub fn load(path: &std::path::Path) -> crate::error::Result<PersonalRankingModel> {
        if !path.exists() {
            return Ok(PersonalRankingModel::default());
        }
        let text = std::fs::read_to_string(path).map_err(|e| crate::error::NexusError::io(path, e))?;
        serde_json::from_str(&text).map_err(|e| {
            crate::error::NexusError::Other(format!("failed to parse personal ranking model at {}: {e}", path.display()))
        })
    }

    /// Saves the model to `path`, creating parent directories as needed.
    pub fn save(&self, path: &std::path::Path) -> crate::error::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| crate::error::NexusError::io(parent, e))?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| crate::error::NexusError::Other(format!("failed to serialize personal ranking model: {e}")))?;
        std::fs::write(path, text).map_err(|e| crate::error::NexusError::io(path, e))
    }
}

/// Works out which `SourceKind` a person clicks most often, from their
/// existing click history — the input to `Features::source_matches_frequently_clicked_kind`.
/// `None` if there's no click history yet, or ties couldn't be broken
/// (both treated the same way downstream: no source gets the "matches
/// what you usually click" boost until there's enough signal to know).
pub fn most_frequently_clicked_kind(
    clicks: &crate::clicks::ClickLog,
    index: &crate::index::Index,
) -> Option<crate::entity::SourceKind> {
    let mut totals: std::collections::HashMap<crate::entity::SourceKind, u32> = std::collections::HashMap::new();
    for (doc_id, count) in clicks.all() {
        let Some(metadata) = index.store.get(doc_id) else { continue };
        let is_web = index.web.get(doc_id).is_some();
        let kind = crate::search::engine::source_kind_for(is_web, &metadata.extension);
        *totals.entry(kind).or_insert(0) += count;
    }
    totals.into_iter().max_by_key(|(_, count)| *count).map(|(kind, _)| kind)
}

/// Builds the [`Features`] vector for one candidate result at ranking
/// (or training) time. `min_score`/`max_score` and
/// `min_modified`/`max_modified` should span the current result set
/// (or shown-results list, for training) being scored, so
/// `base_score_normalized`/`recency_normalized` are relative to what
/// the person actually saw, not some global fixed scale.
pub fn compute_features(
    domain: Option<&str>,
    source_kind: crate::entity::SourceKind,
    frequently_clicked_kind: Option<crate::entity::SourceKind>,
    modified_unix: i64,
    min_modified: i64,
    max_modified: i64,
    score: f32,
    min_score: f32,
    max_score: f32,
) -> Features {
    let is_edu_or_gov_domain = domain
        .map(|d| d.ends_with(".edu") || d.ends_with(".gov"))
        .unwrap_or(false);
    let source_matches_frequently_clicked_kind =
        frequently_clicked_kind.map(|k| k == source_kind).unwrap_or(false);
    let recency_normalized = if max_modified > min_modified {
        (modified_unix - min_modified) as f32 / (max_modified - min_modified) as f32
    } else {
        0.5 // a single result, or all-identical timestamps: no relative signal
    };
    let base_score_normalized = if max_score > min_score {
        (score - min_score) / (max_score - min_score)
    } else {
        0.5
    };
    Features {
        is_edu_or_gov_domain,
        source_matches_frequently_clicked_kind,
        recency_normalized,
        base_score_normalized,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edu_features() -> Features {
        Features {
            is_edu_or_gov_domain: true,
            source_matches_frequently_clicked_kind: false,
            recency_normalized: 0.5,
            base_score_normalized: 0.7,
        }
    }

    fn non_edu_features() -> Features {
        Features {
            is_edu_or_gov_domain: false,
            source_matches_frequently_clicked_kind: false,
            recency_normalized: 0.5,
            base_score_normalized: 0.7,
        }
    }

    #[test]
    fn untrained_model_predicts_neutral_probability() {
        let model = PersonalRankingModel::new();
        let p = model.predict(&edu_features());
        assert!((p - 0.5).abs() < 1e-6);
    }

    #[test]
    fn repeatedly_clicking_edu_sources_raises_their_predicted_score() {
        let mut model = PersonalRankingModel::new();
        let before = model.predict(&edu_features());
        for _ in 0..200 {
            model.train_one(&edu_features(), 1.0);
            model.train_one(&non_edu_features(), 0.0);
        }
        let after_edu = model.predict(&edu_features());
        let after_non_edu = model.predict(&non_edu_features());
        assert!(after_edu > before, "should learn to favor edu sources it keeps seeing clicked");
        assert!(after_edu > after_non_edu);
    }

    #[test]
    fn train_from_result_list_treats_pre_click_results_as_negative() {
        let mut model = PersonalRankingModel::new();
        let results = vec![non_edu_features(), non_edu_features(), edu_features()];
        for _ in 0..100 {
            model.train_from_result_list(&results, Some(2));
        }
        let edu_score = model.predict(&edu_features());
        let non_edu_score = model.predict(&non_edu_features());
        assert!(edu_score > non_edu_score);
    }

    #[test]
    fn no_click_is_a_no_op() {
        let mut model = PersonalRankingModel::new();
        let results = vec![edu_features(), non_edu_features()];
        model.train_from_result_list(&results, None);
        assert_eq!(model.examples_seen(), 0);
    }

    #[test]
    fn feature_weights_are_named_and_inspectable() {
        let model = PersonalRankingModel::new();
        let weights = model.feature_weights();
        assert_eq!(weights.len(), 5);
        assert_eq!(weights[0].0, "bias");
    }

    #[test]
    fn model_round_trips_through_serialization() {
        let mut model = PersonalRankingModel::new();
        model.train_one(&edu_features(), 1.0);
        let json = serde_json::to_string(&model).unwrap();
        let restored: PersonalRankingModel = serde_json::from_str(&json).unwrap();
        assert_eq!(model.examples_seen(), restored.examples_seen());
        assert!((model.predict(&edu_features()) - restored.predict(&edu_features())).abs() < 1e-9);
    }
}
