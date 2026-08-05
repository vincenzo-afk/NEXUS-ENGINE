//! Splits long document text into overlapping chunks so ranking can
//! operate at chunk granularity instead of always scoring one whole
//! document as a single bag of words.
//!
//! Whole-document BM25/vector scoring is too blunt for long PDFs and
//! large docs for a simple reason: a 40-page PDF where one paragraph on
//! page 30 answers the query gets the same document-level score
//! treatment as a 400-word page entirely about the query — length
//! normalization (BM25's `b` parameter) softens this but doesn't fix it,
//! since the *relevant part* is still diluted by 39 unrelated pages
//! either way. Chunking lets [`crate::vector::VectorIndex`] (see
//! [`crate::vector::ChunkVectorIndex`]) store one vector per chunk, so a
//! query can match the specific paragraph rather than the whole
//! document's averaged term distribution.

use serde::{Deserialize, Serialize};

/// One contiguous slice of a document's text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chunk {
    /// Zero-based index of this chunk within its parent document.
    pub index: u32,
    /// Byte offset of this chunk's first character in the source text.
    pub start_offset: u32,
    /// Byte offset one past this chunk's last character.
    pub end_offset: u32,
    /// The chunk's text.
    pub text: String,
}

/// Chunking parameters. Sizes are in whitespace-delimited words, not
/// tokens/bytes, to keep this independent of `crate::text`'s tokenizer
/// (which needs normalized input) — chunking runs on raw extracted text
/// before normalization.
#[derive(Debug, Clone, Copy)]
pub struct ChunkConfig {
    /// Target chunk size, in words.
    pub max_words: usize,
    /// How many words of overlap to keep between consecutive chunks, so
    /// a sentence spanning a chunk boundary isn't split away from both
    /// the context before and after it.
    pub overlap_words: usize,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        // ~300 words is roughly 1-2 paragraphs of typical prose, small
        // enough for a chunk-level vector to be meaningfully specific,
        // large enough to keep chunk counts (and therefore vector
        // storage) reasonable for a large PDF.
        ChunkConfig {
            max_words: 300,
            overlap_words: 50,
        }
    }
}

/// Splits `text` into chunks per `config`. Short documents (at or under
/// one chunk's worth of words) return a single chunk spanning the whole
/// text, so callers can always treat a document uniformly as "one or
/// more chunks" rather than special-casing short documents.
pub fn chunk_text(text: &str, config: ChunkConfig) -> Vec<Chunk> {
    // Collect (word, byte_start, byte_end) so chunk boundaries can be
    // reported as byte offsets into the original text (needed for
    // snippet highlighting to point at the right place), while chunking
    // logic itself works in word counts.
    let words: Vec<(usize, usize)> = text.split_word_bound_indices_like();

    if words.is_empty() {
        return Vec::new();
    }

    let step = config.max_words.saturating_sub(config.overlap_words).max(1);
    let mut chunks = Vec::new();
    let mut start_word = 0usize;
    let mut index = 0u32;

    while start_word < words.len() {
        let end_word = (start_word + config.max_words).min(words.len());
        let start_byte = words[start_word].0;
        let end_byte = words[end_word - 1].1;
        chunks.push(Chunk {
            index,
            start_offset: start_byte as u32,
            end_offset: end_byte as u32,
            text: text[start_byte..end_byte].to_string(),
        });
        index += 1;
        if end_word >= words.len() {
            break;
        }
        start_word += step;
    }

    chunks
}

/// Minimal helper trait so `chunk_text` can iterate `(word_start_byte,
/// word_end_byte)` pairs without pulling in a full tokenizer pass (this
/// runs on raw pre-normalization text, unlike `crate::text::tokenize`).
trait WordBoundIndicesLike {
    fn split_word_bound_indices_like(&self) -> Vec<(usize, usize)>;
}

impl WordBoundIndicesLike for str {
    fn split_word_bound_indices_like(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut chars = self.char_indices().peekable();
        while let Some(&(start, c)) = chars.peek() {
            if c.is_whitespace() {
                chars.next();
                continue;
            }
            let mut end = start;
            while let Some(&(i, c)) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                end = i + c.len_utf8();
                chars.next();
            }
            out.push((start, end));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_yields_one_chunk() {
        let text = "just a few words here";
        let chunks = chunk_text(text, ChunkConfig::default());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, text);
    }

    #[test]
    fn long_text_yields_overlapping_chunks() {
        let words: Vec<String> = (0..1000).map(|i| format!("word{i}")).collect();
        let text = words.join(" ");
        let config = ChunkConfig {
            max_words: 300,
            overlap_words: 50,
        };
        let chunks = chunk_text(&text, config);
        assert!(chunks.len() > 1);
        // Consecutive chunks should share some text (the overlap region).
        for pair in chunks.windows(2) {
            assert!(pair[0].end_offset > pair[1].start_offset);
        }
        // Every chunk's reported offsets should slice back to its text.
        for chunk in &chunks {
            assert_eq!(
                &text[chunk.start_offset as usize..chunk.end_offset as usize],
                chunk.text
            );
        }
    }

    #[test]
    fn empty_text_yields_no_chunks() {
        assert!(chunk_text("", ChunkConfig::default()).is_empty());
    }
}
