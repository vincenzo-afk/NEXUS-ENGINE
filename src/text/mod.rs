//! Text processing pipeline: normalization, tokenization, and stop-word
//! removal. Every document's content and every search query pass through
//! the same pipeline so that indexing and querying stay consistent.

mod normalizer;
mod stopwords;
mod tokenizer;

pub use normalizer::normalize;
pub use stopwords::is_stopword;
pub use tokenizer::{tokenize, Token};
