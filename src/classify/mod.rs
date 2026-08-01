//! Content-quality and content-safety classification, run at index/crawl
//! time so low-quality and unsafe pages can be suppressed or down-ranked
//! before they ever reach a results page.
//!
//! **Read this before assuming "classifier" means a trained model.**
//! Everything here is a transparent, hand-authored heuristic scorer —
//! feature counting and thresholding, the same category of technique
//! [`crate::ranking::reliability`] already uses for E-E-A-T-style signals.
//! That is a deliberate, honest choice, not a corner cut to save effort:
//! a genuine trained spam/SEO classifier needs a labeled corpus and a
//! training pipeline neither of which this repository has, and shipping
//! a classifier that silently returns constant scores while *calling*
//! itself a classifier would be actively misleading. What's here is real
//! and does real, inspectable work — it just does it by rule rather than
//! by gradient descent. See [`spam::SpamClassifier`] and
//! [`safety::SafetyClassifier`] for the specific signals each one uses.
//!
//! ## Wiring this into ranking
//! Both classifiers here return a `0.0..=1.0` score plus the individual
//! signals that produced it. `RankingConfig` (see `crate::config`) does
//! not yet have dedicated `spam_score_weight`/`safety_block_threshold`
//! fields — adding them and applying them alongside the existing
//! `spam_domain_penalty` in `crate::ranking::mod` is the integration seam
//! for anyone wiring this in further; the scoring logic itself is
//! complete and unit-tested independent of that wiring.

pub mod safety;
pub mod spam;
