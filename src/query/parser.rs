//! Recursive-descent parser for the Nexus query language.
//!
//! Grammar (informal):
//!
//! ```text
//! query      := or_expr
//! or_expr    := and_expr ("OR" and_expr)*
//! and_expr   := unary ("AND"? unary)*      // whitespace implies AND
//! unary      := "NOT" unary | leaf
//! leaf       := WORD | PHRASE | FILTER | COMPARE_FILTER | FUZZY | PREFIX | WILDCARD
//! ```

use crate::error::{NexusError, Result};
use crate::query::ast::{CompareOp, QueryNode};
use crate::query::lexer::{lex, QueryToken};
use crate::text;

/// Parses a raw query string into a [`QueryNode`] tree.
pub fn parse(input: &str) -> Result<QueryNode> {
    let tokens = lex(input);
    if tokens.is_empty() {
        return Err(NexusError::QueryParse("empty query".to_string()));
    }
    let mut parser = Parser { tokens, pos: 0 };
    let node = parser.parse_or()?;
    if parser.pos != parser.tokens.len() {
        return Err(NexusError::QueryParse(format!(
            "unexpected token at position {}",
            parser.pos
        )));
    }
    Ok(node)
}

struct Parser {
    tokens: Vec<QueryToken>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&QueryToken> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) -> Option<QueryToken> {
        let tok = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        tok
    }

    fn parse_or(&mut self) -> Result<QueryNode> {
        let mut clauses = vec![self.parse_and()?];
        while matches!(self.peek(), Some(QueryToken::Or)) {
            self.advance();
            clauses.push(self.parse_and()?);
        }
        if clauses.len() == 1 {
            Ok(clauses.pop().unwrap())
        } else {
            Ok(QueryNode::Or(clauses))
        }
    }

    fn parse_and(&mut self) -> Result<QueryNode> {
        let mut clauses = vec![self.parse_unary()?];
        loop {
            match self.peek() {
                Some(QueryToken::And) => {
                    self.advance();
                    clauses.push(self.parse_unary()?);
                }
                Some(QueryToken::Or) | None => break,
                _ => clauses.push(self.parse_unary()?),
            }
        }
        if clauses.len() == 1 {
            Ok(clauses.pop().unwrap())
        } else {
            Ok(QueryNode::And(clauses))
        }
    }

    fn parse_unary(&mut self) -> Result<QueryNode> {
        if matches!(self.peek(), Some(QueryToken::Not)) {
            self.advance();
            let inner = self.parse_unary()?;
            return Ok(QueryNode::Not(Box::new(inner)));
        }
        self.parse_leaf()
    }

    fn parse_leaf(&mut self) -> Result<QueryNode> {
        let token = self
            .advance()
            .ok_or_else(|| NexusError::QueryParse("expected a term".to_string()))?;

        match token {
            QueryToken::Word(word) => Ok(word_to_node(&word)),
            QueryToken::Phrase(phrase) => {
                let normalized = text::normalize(&phrase);
                let terms: Vec<String> = text::tokenize(&normalized)
                    .into_iter()
                    .map(|t| t.text)
                    .collect();
                Ok(QueryNode::Phrase(terms))
            }
            QueryToken::Filter { key, value } => build_filter(&key, &value),
            QueryToken::CompareFilter { key, op, value } => build_compare_filter(&key, op, &value),
            QueryToken::Fuzzy { term, distance } => Ok(QueryNode::Fuzzy {
                term: text::normalize(&term),
                max_distance: distance.unwrap_or(2),
            }),
            other => Err(NexusError::QueryParse(format!(
                "unexpected token: {:?}",
                other
            ))),
        }
    }
}

/// Turns a bare word into a Term, Prefix, or Wildcard node depending on
/// whether it contains glob characters.
fn word_to_node(word: &str) -> QueryNode {
    let normalized = text::normalize(word);
    if normalized.contains('*') || normalized.contains('?') {
        if normalized.ends_with('*') && !normalized[..normalized.len() - 1].contains(['*', '?']) {
            QueryNode::Prefix(normalized[..normalized.len() - 1].to_string())
        } else {
            QueryNode::Wildcard(normalized)
        }
    } else {
        QueryNode::Term(normalized)
    }
}

fn build_filter(key: &str, value: &str) -> Result<QueryNode> {
    match key.to_lowercase().as_str() {
        "ext" => Ok(QueryNode::FilterExt(value.to_lowercase())),
        "path" => Ok(QueryNode::FilterPath(text::normalize(value))),
        "name" => Ok(QueryNode::FilterName(text::normalize(value))),
        other => Err(NexusError::QueryParse(format!("unknown filter: {}", other))),
    }
}

fn build_compare_filter(key: &str, op: char, value: &str) -> Result<QueryNode> {
    let compare_op = match op {
        '>' => CompareOp::GreaterThan,
        '<' => CompareOp::LessThan,
        _ => return Err(NexusError::QueryParse(format!("unsupported operator: {}", op))),
    };

    match key.to_lowercase().as_str() {
        "size" => {
            let bytes = parse_size(value)?;
            Ok(QueryNode::FilterSize(compare_op, bytes))
        }
        "modified" => {
            let seconds = parse_duration(value)?;
            Ok(QueryNode::FilterModified(compare_op, seconds))
        }
        other => Err(NexusError::QueryParse(format!("unknown filter: {}", other))),
    }
}

/// Parses a human size string like `100KB`, `1.5MB`, `2GB` into bytes.
fn parse_size(value: &str) -> Result<u64> {
    let value = value.trim();
    let split_at = value
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(value.len());
    let (num_part, unit_part) = value.split_at(split_at);
    let number: f64 = num_part
        .parse()
        .map_err(|_| NexusError::QueryParse(format!("invalid size number: {}", num_part)))?;

    let multiplier: f64 = match unit_part.to_uppercase().as_str() {
        "" | "B" => 1.0,
        "KB" => 1024.0,
        "MB" => 1024.0 * 1024.0,
        "GB" => 1024.0 * 1024.0 * 1024.0,
        other => {
            return Err(NexusError::QueryParse(format!(
                "unknown size unit: {}",
                other
            )))
        }
    };

    Ok((number * multiplier) as u64)
}

/// Parses a human duration string like `7d`, `24h`, `30m` into seconds.
fn parse_duration(value: &str) -> Result<i64> {
    let value = value.trim();
    let split_at = value
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(value.len());
    let (num_part, unit_part) = value.split_at(split_at);
    let number: f64 = num_part
        .parse()
        .map_err(|_| NexusError::QueryParse(format!("invalid duration number: {}", num_part)))?;

    let multiplier: f64 = match unit_part.to_lowercase().as_str() {
        "s" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        "" | "d" => 86400.0,
        "w" => 7.0 * 86400.0,
        other => {
            return Err(NexusError::QueryParse(format!(
                "unknown duration unit: {}",
                other
            )))
        }
    };

    Ok((number * multiplier) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_word() {
        assert_eq!(parse("rust").unwrap(), QueryNode::Term("rust".into()));
    }

    #[test]
    fn parses_implicit_and() {
        let node = parse("rust parser").unwrap();
        assert_eq!(
            node,
            QueryNode::And(vec![
                QueryNode::Term("rust".into()),
                QueryNode::Term("parser".into())
            ])
        );
    }

    #[test]
    fn parses_explicit_boolean() {
        let node = parse("rust AND parser").unwrap();
        assert_eq!(
            node,
            QueryNode::And(vec![
                QueryNode::Term("rust".into()),
                QueryNode::Term("parser".into())
            ])
        );

        let node = parse("rust OR parser").unwrap();
        assert_eq!(
            node,
            QueryNode::Or(vec![
                QueryNode::Term("rust".into()),
                QueryNode::Term("parser".into())
            ])
        );
    }

    #[test]
    fn parses_negation() {
        let node = parse("NOT javascript").unwrap();
        assert_eq!(
            node,
            QueryNode::Not(Box::new(QueryNode::Term("javascript".into())))
        );

        let node = parse("-javascript").unwrap();
        assert_eq!(
            node,
            QueryNode::Not(Box::new(QueryNode::Term("javascript".into())))
        );
    }

    #[test]
    fn parses_phrase() {
        let node = parse("\"hello world\"").unwrap();
        assert_eq!(
            node,
            QueryNode::Phrase(vec!["hello".into(), "world".into()])
        );
    }

    #[test]
    fn parses_extension_filter_with_term() {
        let node = parse("ext:rs parser").unwrap();
        assert_eq!(
            node,
            QueryNode::And(vec![
                QueryNode::FilterExt("rs".into()),
                QueryNode::Term("parser".into())
            ])
        );
    }

    #[test]
    fn parses_size_and_modified_filters() {
        let node = parse("size>100KB").unwrap();
        assert_eq!(node, QueryNode::FilterSize(CompareOp::GreaterThan, 102400));

        let node = parse("modified<7d").unwrap();
        assert_eq!(
            node,
            QueryNode::FilterModified(CompareOp::LessThan, 7 * 86400)
        );
    }

    #[test]
    fn parses_prefix_and_wildcard() {
        assert_eq!(parse("pars*").unwrap(), QueryNode::Prefix("pars".into()));
        assert_eq!(
            parse("wor?d").unwrap(),
            QueryNode::Wildcard("wor?d".into())
        );
    }
}
