//! Unicode-aware tokenizer.
//!
//! Splits normalized text into word tokens using Unicode word-boundary
//! rules (via `unicode-segmentation`), tracking each token's ordinal
//! position (for phrase queries) and byte offset (for snippet
//! highlighting).

use unicode_segmentation::UnicodeSegmentation;

/// A single token extracted from text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// The normalized text of the token.
    pub text: String,
    /// Zero-based ordinal position of this token within the document
    /// (after stop-word removal has *not* yet been applied — positions are
    /// assigned pre-filtering so phrase queries remain meaningful even when
    /// stop words are skipped from the index).
    pub position: u32,
    /// Byte offset of the token's first character in the *normalized*
    /// source string.
    pub start_offset: u32,
    /// Byte offset one past the token's last character.
    pub end_offset: u32,
}

/// Returns `true` if a Unicode word (as segmented by `unicode-segmentation`)
/// should be treated as an indexable word, i.e. it contains at least one
/// alphanumeric character. This filters out pure punctuation/whitespace
/// segments that `unicode_word_indices` otherwise yields.
fn is_word_like(word: &str) -> bool {
    word.chars().any(|c| c.is_alphanumeric())
}

/// Tokenizes already-normalized text into a sequence of [`Token`]s.
///
/// Callers should run [`crate::text::normalize`] on the input first so that
/// tokens are lowercase and Unicode-folded.
pub fn tokenize(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut position: u32 = 0;

    for (start, word) in text.unicode_word_indices() {
        if !is_word_like(word) {
            continue;
        }
        let end = start + word.len();
        tokens.push(Token {
            text: word.to_string(),
            position,
            start_offset: start as u32,
            end_offset: end as u32,
        });
        position += 1;
    }

    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::normalize;

    #[test]
    fn splits_simple_sentence() {
        let normalized = normalize("Hello, World! Rust is fun.");
        let tokens = tokenize(&normalized);
        let words: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(words, vec!["hello", "world", "rust", "is", "fun"]);
    }

    #[test]
    fn positions_are_sequential() {
        let normalized = normalize("one two three");
        let tokens = tokenize(&normalized);
        assert_eq!(tokens.iter().map(|t| t.position).collect::<Vec<_>>(), vec![0, 1, 2]);
    }

    #[test]
    fn offsets_point_back_into_source() {
        let normalized = normalize("rust parser");
        let tokens = tokenize(&normalized);
        let second = &tokens[1];
        assert_eq!(&normalized[second.start_offset as usize..second.end_offset as usize], "parser");
    }
}
