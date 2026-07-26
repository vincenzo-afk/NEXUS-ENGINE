//! Levenshtein edit distance.
//!
//! Used both for fuzzy query matching (`term~2`) and "did you mean"
//! spelling suggestions. Implemented with the classic two-row dynamic
//! programming approach, which is O(n*m) time and O(min(n,m)) space.

/// Computes the Levenshtein edit distance between two strings, i.e. the
/// minimum number of single-character insertions, deletions, or
/// substitutions required to turn `a` into `b`.
pub fn levenshtein_distance(a: &str, b: &str) -> u32 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();

    if a.is_empty() {
        return b.len() as u32;
    }
    if b.is_empty() {
        return a.len() as u32;
    }

    let mut prev_row: Vec<u32> = (0..=b.len() as u32).collect();
    let mut curr_row = vec![0u32; b.len() + 1];

    for i in 1..=a.len() {
        curr_row[0] = i as u32;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr_row[j] = (prev_row[j] + 1) // deletion
                .min(curr_row[j - 1] + 1) // insertion
                .min(prev_row[j - 1] + cost); // substitution
        }
        std::mem::swap(&mut prev_row, &mut curr_row);
    }

    prev_row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_strings_have_zero_distance() {
        assert_eq!(levenshtein_distance("rust", "rust"), 0);
    }

    #[test]
    fn single_substitution() {
        assert_eq!(levenshtein_distance("rust", "rest"), 1);
    }

    #[test]
    fn insertion_and_deletion() {
        assert_eq!(levenshtein_distance("rust", "rusty"), 1);
        assert_eq!(levenshtein_distance("rusty", "rust"), 1);
    }

    #[test]
    fn classic_example() {
        assert_eq!(levenshtein_distance("kitten", "sitting"), 3);
    }
}
