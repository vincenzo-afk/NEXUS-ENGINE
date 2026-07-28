//! Search: query evaluation against the index and snippet generation for
//! displaying results.

pub mod engine;
pub mod snippet;

pub use engine::search;
