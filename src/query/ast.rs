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
            | QueryNode::FilterModified(_, _) => false,
        }
    }
}
