//! The Nexus query language: lexer, parser, and AST.

pub mod ast;
pub mod lexer;
pub mod parser;

pub use ast::{CompareOp, QueryNode};
pub use parser::parse;

/// Collects every literal term referenced anywhere in a query AST (terms,
/// phrase words, prefixes, wildcards, fuzzy terms), for snippet
/// highlighting purposes. Filter-only clauses (e.g. `ext:rs`) contribute
/// nothing, since they don't correspond to literal text in the document.
pub fn collect_terms(node: &QueryNode) -> std::collections::HashSet<String> {
    use QueryNode::*;
    let mut terms = std::collections::HashSet::new();
    match node {
        Term(t) => {
            terms.insert(t.clone());
        }
        Phrase(ts) => terms.extend(ts.iter().cloned()),
        Prefix(p) => {
            terms.insert(p.clone());
        }
        Wildcard(w) => {
            terms.insert(w.clone());
        }
        Fuzzy { term, .. } => {
            terms.insert(term.clone());
        }
        And(children) | Or(children) => {
            for c in children {
                terms.extend(collect_terms(c));
            }
        }
        Not(_)
        | FilterExt(_)
        | FilterPath(_)
        | FilterName(_)
        | FilterSize(_, _)
        | FilterModified(_, _)
        | FilterSite(_)
        | FilterDate(_, _)
        | FilterLang(_)
        | FilterAuthor(_) => {}
    }
    terms
}
