//! Search: query evaluation against the index and snippet generation for
//! displaying results.

pub mod engine;
pub mod snippet;

#[allow(unused_imports)]
pub use engine::{
    apply_personal_boost, compute_result_features, search, search_query, search_with_federation,
    Personalization, SearchMode, SearchOutcome,
};
