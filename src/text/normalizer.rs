//! Unicode normalization.
//!
//! Text is normalized to Unicode NFKC form and lowercased before
//! tokenization. NFKC folds compatibility variants (e.g. full-width digits,
//! ligatures) into a canonical form, which keeps the vocabulary compact and
//! ensures that visually-identical strings hash to the same term.

use log::trace;
use unicode_normalization::UnicodeNormalization;

/// Normalizes a string for indexing or querying: NFKC normalization
/// followed by lowercasing.
pub fn normalize(input: &str) -> String {
    let result = input.nfkc().collect::<String>().to_lowercase();
    trace!("normalized: '{}' -> '{}'", input, result);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercases_ascii() {
        assert_eq!(normalize("Hello WORLD"), "hello world");
    }

    #[test]
    fn folds_compatibility_forms() {
        // Full-width Latin 'A' (U+FF21) should normalize to ASCII 'a'.
        let input = "\u{FF21}BC";
        assert_eq!(normalize(input), "abc");
    }
}
