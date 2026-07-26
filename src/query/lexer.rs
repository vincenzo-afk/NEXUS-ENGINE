//! Lexer for the Nexus query language.
//!
//! Converts a raw query string into a flat sequence of [`QueryToken`]s that
//! [`crate::query::parser`] turns into an AST. Keeping lexing and parsing
//! separate makes both stages easier to test and reason about.

/// A single lexical token in a query string.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryToken {
    /// A bare word, e.g. `rust` or `pars*` or `wor?d`.
    Word(String),
    /// A double-quoted phrase, already stripped of its quotes.
    Phrase(String),
    /// The literal `AND` keyword (case-insensitive in the source).
    And,
    /// The literal `OR` keyword (case-insensitive in the source).
    Or,
    /// The literal `NOT` keyword or a leading `-` prefix.
    Not,
    /// A `key:value` filter such as `ext:rs`.
    Filter { key: String, value: String },
    /// A comparison filter such as `size>100KB` or `modified<7d`.
    CompareFilter { key: String, op: char, value: String },
    /// A fuzzy term such as `rustt~2` or `rustt~`.
    Fuzzy { term: String, distance: Option<u32> },
}

/// Lexes `input` into a sequence of tokens.
pub fn lex(input: &str) -> Vec<QueryToken> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        if c.is_whitespace() {
            i += 1;
            continue;
        }

        if c == '"' {
            let start = i + 1;
            let mut j = start;
            while j < chars.len() && chars[j] != '"' {
                j += 1;
            }
            let phrase: String = chars[start..j.min(chars.len())].iter().collect();
            tokens.push(QueryToken::Phrase(phrase));
            i = j + 1;
            continue;
        }

        if c == '-' && i + 1 < chars.len() && !chars[i + 1].is_whitespace() {
            tokens.push(QueryToken::Not);
            i += 1;
            continue;
        }

        // Consume a bare word-like span (letters, digits, and query-syntax
        // punctuation that we'll interpret below: : > < ~ * ? _ . /).
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() && chars[i] != '"' {
            i += 1;
        }
        let raw: String = chars[start..i].iter().collect();
        tokens.push(classify_word(&raw));
    }

    tokens
}

/// Classifies a raw whitespace-delimited word into the most specific token
/// type it represents: a keyword, filter, comparison filter, fuzzy term, or
/// plain word.
fn classify_word(raw: &str) -> QueryToken {
    match raw.to_uppercase().as_str() {
        "AND" => return QueryToken::And,
        "OR" => return QueryToken::Or,
        "NOT" => return QueryToken::Not,
        _ => {}
    }

    if let Some(idx) = raw.find(['>', '<']) {
        let key = raw[..idx].to_string();
        let op = raw.as_bytes()[idx] as char;
        let value = raw[idx + 1..].to_string();
        if !key.is_empty() && !value.is_empty() {
            return QueryToken::CompareFilter { key, op, value };
        }
    }

    if let Some(idx) = raw.find(':') {
        let key = raw[..idx].to_string();
        let value = raw[idx + 1..].to_string();
        if !key.is_empty() && !value.is_empty() {
            return QueryToken::Filter { key, value };
        }
    }

    if let Some(idx) = raw.find('~') {
        let term = raw[..idx].to_string();
        let distance_str = &raw[idx + 1..];
        let distance = if distance_str.is_empty() {
            None
        } else {
            distance_str.parse::<u32>().ok()
        };
        if !term.is_empty() {
            return QueryToken::Fuzzy { term, distance };
        }
    }

    QueryToken::Word(raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexes_plain_words() {
        let tokens = lex("rust parser");
        assert_eq!(
            tokens,
            vec![
                QueryToken::Word("rust".into()),
                QueryToken::Word("parser".into())
            ]
        );
    }

    #[test]
    fn lexes_phrase() {
        let tokens = lex("\"hello world\"");
        assert_eq!(tokens, vec![QueryToken::Phrase("hello world".into())]);
    }

    #[test]
    fn lexes_boolean_keywords() {
        let tokens = lex("rust AND parser OR NOT javascript");
        assert_eq!(
            tokens,
            vec![
                QueryToken::Word("rust".into()),
                QueryToken::And,
                QueryToken::Word("parser".into()),
                QueryToken::Or,
                QueryToken::Not,
                QueryToken::Word("javascript".into()),
            ]
        );
    }

    #[test]
    fn lexes_filters_and_comparisons() {
        let tokens = lex("ext:rs size>100KB modified<7d");
        assert_eq!(
            tokens,
            vec![
                QueryToken::Filter { key: "ext".into(), value: "rs".into() },
                QueryToken::CompareFilter { key: "size".into(), op: '>', value: "100KB".into() },
                QueryToken::CompareFilter { key: "modified".into(), op: '<', value: "7d".into() },
            ]
        );
    }

    #[test]
    fn lexes_fuzzy_term() {
        let tokens = lex("rustt~2 other~");
        assert_eq!(
            tokens,
            vec![
                QueryToken::Fuzzy { term: "rustt".into(), distance: Some(2) },
                QueryToken::Fuzzy { term: "other".into(), distance: None },
            ]
        );
    }
}
