//! Abstract syntax tree for the Nexus query language.
//!
//! Example queries and how they parse:
//!
//! * `rust parser`            -> `And([Term(rust), Term(parser)])`
//! * `"hello world"`          -> `Phrase([hello, world])`
//! * `rust AND parser`        -> `And([Term(rust), Term(parser)])`
//! * `rust OR parser`         -> `Or([Term(rust), Term(parser)])`
//! * `NOT javascript`         -> `Not(Term(javascript))`
//! * `ext:rs parser`          -> `And([FilterExt(rs), Term(parser)])`
//! * `path:src parser`        -> `And([FilterPath(src), Term(parser)])`
//! * `name:main`              -> `FilterName(main)`
//! * `size>100KB`             -> `FilterSize(GreaterThan, 102400)`
//! * `modified<7d`            -> `FilterModified(NewerThan, 7 days)`
//! * `site:github.com`        -> `FilterSite(github.com)`
//! * `filetype:pdf`           -> `FilterExt(pdf)` (alias for `ext:`)
//! * `before:2024`            -> `FilterDate(LessThan, 2024-01-01T00:00:00Z)`
//! * `after:2022-06-01`       -> `FilterDate(GreaterThan, 2022-06-01T00:00:00Z)`
//! * `lang:en`                -> `FilterLang(en)`
//! * `author:jane`            -> `FilterAuthor(jane)`
//! * `intitle:rust`           -> `FilterName(rust)` (alias for `name:`)
//! * `inurl:async`            -> `FilterPath(async)` (alias for `path:`)
//! * `@person:sarah`          -> `FilterEntity(sarah)`
//! * `@org:acme`              -> `FilterEntity(acme)` (same filter as `@person:` — see `FilterEntity`'s doc comment)

/// A comparison operator for numeric filters (`size`, `modified`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    /// `>` - greater than.
    GreaterThan,
    /// `<` - less than.
    LessThan,
    /// `=` - equal to (rarely useful, but supported for completeness).
    Equal,
}

/// A node in the parsed query tree.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryNode {
    /// A single normalized term to match against the index.
    Term(String),
    /// An exact contiguous sequence of terms.
    Phrase(Vec<String>),
    /// A prefix match, e.g. `pars*` matches "parser", "parsing", etc.
    Prefix(String),
    /// A glob-style wildcard pattern (`*` = any run of characters, `?` = any
    /// single character) matched against whole terms.
    Wildcard(String),
    /// A fuzzy match against a term, tolerating up to the given Levenshtein
    /// edit distance.
    Fuzzy {
        /// The term to fuzzily match.
        term: String,
        /// Maximum allowed edit distance.
        max_distance: u32,
    },
    /// Conjunction: every child must match.
    And(Vec<QueryNode>),
    /// Disjunction: at least one child must match.
    Or(Vec<QueryNode>),
    /// Negation: the child must not match.
    Not(Box<QueryNode>),
    /// `ext:rs` - restrict to a file extension.
    FilterExt(String),
    /// `path:src` - restrict to paths containing this substring.
    FilterPath(String),
    /// `name:main` - restrict to file names containing this substring.
    FilterName(String),
    /// `size>100KB` / `size<1MB` - restrict by file size in bytes.
    FilterSize(CompareOp, u64),
    /// `modified<7d` / `modified>30d` - restrict by age in seconds.
    /// `LessThan` means "modified more recently than N seconds ago"
    /// (newer), `GreaterThan` means "modified more than N seconds ago"
    /// (older).
    FilterModified(CompareOp, i64),
    /// `site:github.com` - restrict to web pages on a given (registrable)
    /// domain. No-op filter (matches nothing) for local files, since they
    /// have no domain.
    FilterSite(String),
    /// `before:2024-01-01` / `after:2022` - restrict by an absolute
    /// modified/fetched date rather than a relative age. `LessThan` means
    /// "before this date", `GreaterThan` means "after this date".
    FilterDate(CompareOp, i64),
    /// `lang:en` - restrict to web pages declaring this `<html lang>`.
    FilterLang(String),
    /// `author:jane` - restrict to web pages whose `<meta name="author">`
    /// contains this substring.
    FilterAuthor(String),
    /// `@person:sarah` / `@org:acme` - restrict to documents whose
    /// [`crate::graph::GraphBuilder`] entity graph mentions an entity
    /// whose name contains this substring (case-insensitive).
    ///
    /// **Both operators do the exact same lookup.** `crate::graph`'s
    /// rule-based extractor cannot reliably tell a person's name apart
    /// from an organization's — "Sarah Chen" and "Acme Corporation" are
    /// both just `EntityKind::ProperNoun` (see that enum's doc comment)
    /// — so `@person:` and `@org:` are two spellings of one filter
    /// rather than two different ones that would each work only some of
    /// the time. Offering only one truthful operator (say, `@entity:`)
    /// was the other option; both are kept because a person searching
    /// `@person:sarah` or `@org:acme` has a reasonable expectation
    /// either spelling works, and being right about *what* matched
    /// matters more than pretending the split is real.
    FilterEntity(String),
}

impl QueryNode {
    /// Returns `true` if this node (or, for compound nodes, any leaf beneath
    /// it) represents a positive term/phrase match rather than purely a
    /// metadata filter. Used to decide whether a query is "filters only"
    /// (e.g. `ext:rs`) which matches every document of that type.
    pub fn has_text_clause(&self) -> bool {
        match self {
            QueryNode::Term(_)
            | QueryNode::Phrase(_)
            | QueryNode::Prefix(_)
            | QueryNode::Wildcard(_)
            | QueryNode::Fuzzy { .. } => true,
            QueryNode::And(children) | QueryNode::Or(children) => {
                children.iter().any(|c| c.has_text_clause())
            }
            QueryNode::Not(_) => false,
            QueryNode::FilterExt(_)
            | QueryNode::FilterPath(_)
            | QueryNode::FilterName(_)
            | QueryNode::FilterSize(_, _)
            | QueryNode::FilterModified(_, _)
            | QueryNode::FilterSite(_)
            | QueryNode::FilterDate(_, _)
            | QueryNode::FilterLang(_)
            | QueryNode::FilterAuthor(_)
            | QueryNode::FilterEntity(_) => false,
        }
    }
}
