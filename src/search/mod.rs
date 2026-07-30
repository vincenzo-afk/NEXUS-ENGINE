//! Search: query evaluation against the index and snippet generation for
//! displaying results.

pub mod engine;
pub mod snippet;

#[allow(unused_imports)]
pub use engine::{search, SearchMode, SearchOutcome};
