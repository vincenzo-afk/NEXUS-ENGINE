//! A compact trie for prefix lookups, used to power autocomplete
//! suggestions over the search vocabulary.

use std::collections::HashMap;

#[derive(Debug, Default)]
struct TrieNode {
    children: HashMap<char, TrieNode>,
    /// Present (and holding a use-count) if a complete word ends at this node.
    frequency: Option<u32>,
}

/// A prefix trie mapping words to usage frequencies, supporting fast
/// "all words starting with this prefix" queries.
#[derive(Debug, Default)]
pub struct Trie {
    root: TrieNode,
}

impl Trie {
    /// Creates an empty trie.
    pub fn new() -> Self {
        Trie {
            root: TrieNode::default(),
        }
    }

    /// Inserts `word` into the trie, incrementing its frequency by one if
    /// already present.
    pub fn insert(&mut self, word: &str) {
        let mut node = &mut self.root;
        for c in word.chars() {
            node = node.children.entry(c).or_default();
        }
        node.frequency = Some(node.frequency.unwrap_or(0) + 1);
    }

    /// Returns up to `limit` words starting with `prefix`, ordered by
    /// descending frequency (most-used first), then alphabetically.
    pub fn suggest(&self, prefix: &str, limit: usize) -> Vec<String> {
        let mut node = &self.root;
        for c in prefix.chars() {
            match node.children.get(&c) {
                Some(next) => node = next,
                None => return Vec::new(),
            }
        }

        let mut results = Vec::new();
        Self::collect(node, prefix, &mut results);
        results.sort_by(|a: &(String, u32), b: &(String, u32)| {
            b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0))
        });
        results.truncate(limit);
        results.into_iter().map(|(word, _)| word).collect()
    }

    fn collect(node: &TrieNode, prefix: &str, out: &mut Vec<(String, u32)>) {
        if let Some(freq) = node.frequency {
            out.push((prefix.to_string(), freq));
        }
        for (c, child) in &node.children {
            let mut next_prefix = String::with_capacity(prefix.len() + c.len_utf8());
            next_prefix.push_str(prefix);
            next_prefix.push(*c);
            Self::collect(child, &next_prefix, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_words_by_prefix() {
        let mut trie = Trie::new();
        trie.insert("rust");
        trie.insert("rusty");
        trie.insert("rustacean");
        trie.insert("java");

        let mut results = trie.suggest("rus", 10);
        results.sort();
        assert_eq!(results, vec!["rust", "rustacean", "rusty"]);
    }

    #[test]
    fn frequency_affects_order() {
        let mut trie = Trie::new();
        trie.insert("rust");
        trie.insert("rust");
        trie.insert("rusty");

        let results = trie.suggest("rus", 10);
        assert_eq!(results[0], "rust");
    }

    #[test]
    fn empty_for_unknown_prefix() {
        let mut trie = Trie::new();
        trie.insert("rust");
        assert!(trie.suggest("xyz", 10).is_empty());
    }
}
