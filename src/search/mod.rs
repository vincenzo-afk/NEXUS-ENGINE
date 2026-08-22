//! Search: query evaluation against the index and snippet generation for
//! displaying results.

pub mod engine;
pub mod snippet;

#[allow(unused_imports)]
pub use engine::{
    apply_personal_boost, compute_result_features, search, search_personalized, search_query,
    Personalization, SearchMode, SearchOutcome,
};
// Federation runs on the server only (real outbound HTTP to peers).
#[cfg(not(target_family = "wasm"))]
#[allow(unused_imports)]
pub use engine::search_with_federation;
