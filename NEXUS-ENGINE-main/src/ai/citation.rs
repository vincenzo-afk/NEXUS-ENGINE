//! Span-level citation verification: checks whether a cited *claim* is
//! actually supported by the *specific* passage span it cites, not just
//! whether the citation number refers to a real, provided source.
//!
//! [`crate::ai::summarize`] already enforces "hard" citation grounding —
//! every retained sentence must cite a source ID that was actually given
//! to the model, and fabricated `[N]` references get the sentence
//! dropped. That check is structural: it confirms the *pointer* is real.
//! It does not confirm the *claim* is actually what's written at that
//! pointer's target — a model could cite a real source `[3]` for a claim
//! that source's text doesn't actually say. This module is the second,
//! narrower check: given a claim and the exact span of text it's cited
//! against, does the span provide lexical support for the claim.
//!
//! **What this is and isn't.** This is a lexical overlap heuristic
//! (shared significant-word ratio, plus a numeric/entity consistency
//! check), not a trained entailment/NLI model. It will correctly flag
//! "cites source but source text shares essentially no vocabulary with
//! the claim" and "claim states a different number than the span
//! contains," which cover a meaningful share of real hallucinated-citation
//! cases. It will not catch subtler failures, like negation flips
//! ("the drug reduced symptoms" cited against a span that says it did
//! *not* reduce symptoms) beyond a simple negation-word mismatch check,
//! or claims that are technically entailed but phrased with no shared
//! vocabulary. Treat [`VerificationVerdict::Unsupported`] as "flag for
//! review," and [`VerificationVerdict::Supported`] as "no red flag
//! found," not as a proof of correctness in either direction.

use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq)]
pub enum VerificationVerdict {
    /// The span provides reasonable lexical support for the claim.
    Supported,
    /// The span and claim share too little vocabulary, or contain
    /// conflicting numbers/negation, to consider the claim supported.
    Unsupported,
    /// The cited span was empty or the claim was empty; nothing to check.
    Indeterminate,
}

#[derive(Debug, Clone)]
pub struct VerificationResult {
    pub verdict: VerificationVerdict,
    /// Fraction (0.0-1.0) of the claim's significant (non-stopword)
    /// terms that also appear in the cited span.
    pub term_overlap: f32,
    pub reason: String,
}

/// Verifies `claim` (one cited sentence from a generated summary)
/// against `span` (the exact passage text the citation points at, e.g.
/// [`crate::search::snippet`]'s source region or a
/// [`crate::vector::ChunkVectorIndex`] chunk's text).
pub fn verify_claim_against_span(claim: &str, span: &str) -> VerificationResult {
    if claim.trim().is_empty() || span.trim().is_empty() {
        return VerificationResult {
            verdict: VerificationVerdict::Indeterminate,
            term_overlap: 0.0,
            reason: "claim or cited span is empty".to_string(),
        };
    }

    let claim_terms = significant_terms(claim);
    let span_terms = significant_terms(span);

    if claim_terms.is_empty() {
        return VerificationResult {
            verdict: VerificationVerdict::Indeterminate,
            term_overlap: 0.0,
            reason: "claim has no significant terms to check".to_string(),
        };
    }

    let overlap_count = claim_terms.intersection(&span_terms).count();
    let overlap = overlap_count as f32 / claim_terms.len() as f32;

    if let Some(reason) = numeric_conflict(claim, span) {
        return VerificationResult {
            verdict: VerificationVerdict::Unsupported,
            term_overlap: overlap,
            reason,
        };
    }

    if let Some(reason) = negation_conflict(claim, span, &claim_terms, &span_terms) {
        return VerificationResult {
            verdict: VerificationVerdict::Unsupported,
            term_overlap: overlap,
            reason,
        };
    }

    // Below this overlap, the claim is using essentially different
    // vocabulary than the cited span — either the wrong span was cited,
    // or the model paraphrased so loosely that a reviewer should check
    // it by hand.
    const MIN_SUPPORTED_OVERLAP: f32 = 0.35;
    if overlap < MIN_SUPPORTED_OVERLAP {
        VerificationResult {
            verdict: VerificationVerdict::Unsupported,
            term_overlap: overlap,
            reason: format!(
                "only {:.0}% of the claim's significant terms appear in the cited span",
                overlap * 100.0
            ),
        }
    } else {
        VerificationResult {
            verdict: VerificationVerdict::Supported,
            term_overlap: overlap,
            reason: format!("{:.0}% term overlap with the cited span", overlap * 100.0),
        }
    }
}

/// Batch-verifies every `(claim, span)` pair (e.g. every cited sentence
/// in a [`crate::ai::summarize::GroundedSummary`] against its source's
/// text) and reports how many were flagged, for a single "grounding
/// rate" style metric (see [`crate::metrics`]).
pub fn verify_all(pairs: &[(String, String)]) -> Vec<VerificationResult> {
    pairs
        .iter()
        .map(|(claim, span)| verify_claim_against_span(claim, span))
        .collect()
}

fn significant_terms(text: &str) -> HashSet<String> {
    let normalized = crate::text::normalize(text);
    crate::text::tokenize(&normalized)
        .into_iter()
        .map(|t| t.text)
        .filter(|t| !crate::text::is_stopword(t) && t.len() > 1)
        .collect()
}

/// Extracts standalone numbers from `text` (years, percentages, counts).
fn numbers_in(text: &str) -> HashSet<String> {
    text.split(|c: char| !c.is_ascii_digit() && c != '.')
        .filter(|s| !s.is_empty() && s.chars().any(|c| c.is_ascii_digit()))
        .map(|s| s.trim_matches('.').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Flags a claim that states a specific number the cited span never
/// mentions at all, while the span does contain *some* numbers (if the
/// span has no numbers, there's nothing to conflict with — the claim's
/// number might be supported by context this heuristic can't see).
fn numeric_conflict(claim: &str, span: &str) -> Option<String> {
    let claim_numbers = numbers_in(claim);
    let span_numbers = numbers_in(span);
    if claim_numbers.is_empty() || span_numbers.is_empty() {
        return None;
    }
    if claim_numbers.is_disjoint(&span_numbers) {
        Some(format!(
            "claim cites number(s) {:?} not present in the cited span",
            claim_numbers
        ))
    } else {
        None
    }
}

const NEGATION_WORDS: &[&str] = &["not", "no", "never", "neither", "without", "isn't", "wasn't", "didn't", "doesn't"];

/// Splits `text` into rough clauses on sentence/clause punctuation so a
/// negation word can be checked against *its own* clause rather than the
/// text as a whole. A negation word in an unrelated trailing clause (e.g.
/// "...compile time, without a garbage collector" negating "garbage
/// collector", not the claim's subject) shouldn't count as contradicting
/// a claim about a completely different part of the sentence.
fn split_clauses(text: &str) -> Vec<&str> {
    text.split(|c: char| matches!(c, ',' | '.' | ';' | ':'))
        .filter(|c| !c.trim().is_empty())
        .collect()
}

/// Whether `text` contains a negation word inside a clause that itself
/// shares vocabulary with `other_terms` — i.e. a negation that's actually
/// about the same subject matter being checked, not incidental negation
/// elsewhere in the sentence.
fn has_relevant_negation(text: &str, other_terms: &HashSet<String>) -> bool {
    split_clauses(text).into_iter().any(|clause| {
        let lower = clause.to_lowercase();
        let has_neg = NEGATION_WORDS.iter().any(|w| {
            lower
                .split_whitespace()
                .any(|word| word.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'') == *w)
        });
        has_neg && !significant_terms(clause).is_disjoint(other_terms)
    })
}

/// Flags a claim whose negation status (contains a *relevant* negation
/// word or not) disagrees with the cited span's, *when* the two
/// otherwise share substantial vocabulary (high overlap but opposite
/// polarity is exactly the "the source said the opposite" failure mode
/// this exists to catch; low overlap is already caught by the overlap
/// threshold on its own).
fn negation_conflict(
    claim: &str,
    span: &str,
    claim_terms: &HashSet<String>,
    span_terms: &HashSet<String>,
) -> Option<String> {
    let claim_negated = has_relevant_negation(claim, span_terms);
    let span_negated = has_relevant_negation(span, claim_terms);
    if claim_negated == span_negated {
        return None;
    }
    let overlap = claim_terms.intersection(span_terms).count() as f32 / claim_terms.len().max(1) as f32;
    if overlap > 0.5 {
        Some("claim and cited span share substantial vocabulary but disagree on negation".to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_supported_claim_passes() {
        let claim = "Rust's borrow checker enforces memory safety at compile time.";
        let span = "The borrow checker in Rust enforces memory safety rules entirely at compile time, without a garbage collector.";
        let result = verify_claim_against_span(claim, span);
        assert_eq!(result.verdict, VerificationVerdict::Supported);
    }

    #[test]
    fn unrelated_span_is_flagged() {
        let claim = "The company's revenue grew 40% in Q4.";
        let span = "The chef recommends letting the dough rest for at least an hour before baking.";
        let result = verify_claim_against_span(claim, span);
        assert_eq!(result.verdict, VerificationVerdict::Unsupported);
    }

    #[test]
    fn conflicting_number_is_flagged_even_with_topic_overlap() {
        let claim = "The report says unemployment fell to 3.2 percent.";
        let span = "The report says unemployment fell to 5.7 percent last quarter.";
        let result = verify_claim_against_span(claim, span);
        assert_eq!(result.verdict, VerificationVerdict::Unsupported);
    }

    #[test]
    fn negation_flip_is_flagged() {
        let claim = "The trial found the treatment reduced symptoms significantly.";
        let span = "The trial found the treatment did not reduce symptoms significantly.";
        let result = verify_claim_against_span(claim, span);
        assert_eq!(result.verdict, VerificationVerdict::Unsupported);
    }

    #[test]
    fn empty_inputs_are_indeterminate() {
        let result = verify_claim_against_span("", "some span text");
        assert_eq!(result.verdict, VerificationVerdict::Indeterminate);
    }
}
