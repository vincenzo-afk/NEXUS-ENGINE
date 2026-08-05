//! True neural semantic search: sentence embeddings from a real trained
//! transformer model (e.g. `all-MiniLM-L6-v2` or `bge-small-en-v1.5`),
//! run locally via `candle` (Rust-native, no Python/PyTorch runtime
//! needed) — this is the actual upgrade path the README's "True neural
//! semantic search" section describes, not another lexical heuristic
//! wearing a neural-sounding name.
//!
//! `crate::vector`'s existing `VectorIndex`/`ChunkVectorIndex` use a
//! **lexical hashing-trick** vector (`embed_tf`): term-frequency counts
//! hashed into a fixed 256-dim bucket vector. That module says clearly
//! it cannot match synonyms ("car" vs. "automobile") because it has no
//! notion of word meaning, only which literal tokens appeared. This
//! module is the fix for that specific, named limitation: a real
//! trained model whose output vectors place semantically similar text
//! near each other regardless of exact wording.
//!
//! ## What's genuinely implemented vs. what a caller must supply
//! [`NeuralEmbedder`] loads a **local** SafeTensors model file, its
//! `config.json`, and a `tokenizer.json` (the standard Hugging Face
//! artifact trio) and runs a real forward pass through a BERT-family
//! encoder (via `candle-transformers`' BERT implementation) followed by
//! mean pooling over the token embeddings and L2 normalization — the
//! standard sentence-embedding recipe used by `all-MiniLM-L6-v2` and
//! similar `sentence-transformers` models. It does **not** bundle a
//! model file (that would bloat this repository by tens to hundreds of
//! megabytes and pin it to one specific model's license) — a caller
//! downloads one themselves (e.g. via the `hf-hub` crate, or manually
//! from Hugging Face) and points [`NeuralEmbedder::load`] at the
//! resulting directory. `models::download_hint` below just prints the
//! commands to do that; it does not fetch anything itself.
//!
//! This is gated behind the `neural_embeddings` Cargo feature (off by
//! default) because `candle-core`/`candle-transformers`/`tokenizers`
//! are a meaningfully heavier dependency tree than the rest of this
//! codebase, appropriate to opt into deliberately rather than pay for
//! on every build.
//!
//! ## Honesty about verification
//! This was written without a working Rust toolchain available to
//! compile or run it against a real model file — the general shape
//! (load config/tokenizer/weights, run `BertModel::forward`, mean-pool,
//! normalize) matches the standard, widely-documented `candle-transformers`
//! BERT embedding pattern, but `candle`'s API has changed across
//! versions before and specific method/field names here should be
//! treated as "needs a `cargo build --features neural_embeddings` check
//! and likely a small patch," not as verified-correct.
#![cfg(feature = "neural_embeddings")]

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokenizers::Tokenizer;

/// A dense neural embedding — dimension depends on the loaded model
/// (384 for MiniLM-L6, 384 for bge-small, etc.), unlike
/// `crate::vector::LexicalVector`'s fixed 256. Kept as its own type
/// rather than forced into `LexicalVector`'s fixed size for exactly that
/// reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NeuralVector(pub Vec<f32>);

impl NeuralVector {
    /// Cosine similarity. Vectors coming out of [`NeuralEmbedder::embed`]
    /// are already L2-normalized, so this is a plain dot product, same
    /// optimization `LexicalVector::cosine_similarity` makes.
    pub fn cosine_similarity(&self, other: &NeuralVector) -> f32 {
        if self.0.len() != other.0.len() {
            return 0.0; // mismatched models/dimensions: not comparable
        }
        self.0.iter().zip(other.0.iter()).map(|(a, b)| a * b).sum()
    }
}

pub struct NeuralEmbedder {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
}

impl NeuralEmbedder {
    /// Loads a model from `model_dir`, expected to contain
    /// `config.json`, `tokenizer.json`, and `model.safetensors` — the
    /// standard layout of a Hugging Face model snapshot. Runs on CPU
    /// (`Device::Cpu`); this is a local search-indexing embedder run
    /// occasionally over a document corpus, not a latency-critical
    /// inference server, so GPU acceleration is left as a future
    /// enhancement rather than a requirement.
    pub fn load(model_dir: &Path) -> Result<Self, String> {
        let config_path = model_dir.join("config.json");
        let tokenizer_path = model_dir.join("tokenizer.json");
        let weights_path = model_dir.join("model.safetensors");

        for (label, path) in [
            ("config.json", &config_path),
            ("tokenizer.json", &tokenizer_path),
            ("model.safetensors", &weights_path),
        ] {
            if !path.exists() {
                return Err(format!(
                    "missing {label} in {} — download a sentence-embedding model \
                     (e.g. sentence-transformers/all-MiniLM-L6-v2) into this directory first; \
                     see `models::download_hint`",
                    model_dir.display()
                ));
            }
        }

        let config_json = std::fs::read_to_string(&config_path)
            .map_err(|e| format!("reading config.json: {e}"))?;
        let config: BertConfig =
            serde_json::from_str(&config_json).map_err(|e| format!("parsing config.json: {e}"))?;

        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| format!("loading tokenizer.json: {e}"))?;

        let device = Device::Cpu;
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], DType::F32, &device)
                .map_err(|e| format!("loading model.safetensors: {e}"))?
        };
        let model = BertModel::load(vb, &config).map_err(|e| format!("building BERT model: {e}"))?;

        Ok(NeuralEmbedder {
            model,
            tokenizer,
            device,
        })
    }

    /// Embeds one piece of text into a mean-pooled, L2-normalized
    /// sentence vector — the standard `sentence-transformers` recipe:
    /// run the encoder, average the per-token output embeddings
    /// (respecting the attention mask, so padding tokens don't dilute
    /// the average), then normalize to unit length so cosine similarity
    /// reduces to a dot product.
    pub fn embed(&self, text: &str) -> Result<NeuralVector, String> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| format!("tokenizing: {e}"))?;
        let token_ids = encoding.get_ids();
        let attention_mask = encoding.get_attention_mask();

        let token_ids_tensor = Tensor::new(token_ids, &self.device)
            .map_err(|e| e.to_string())?
            .unsqueeze(0)
            .map_err(|e| e.to_string())?;
        let token_type_ids = token_ids_tensor
            .zeros_like()
            .map_err(|e| e.to_string())?;

        let output = self
            .model
            .forward(&token_ids_tensor, &token_type_ids, None)
            .map_err(|e| format!("model forward pass: {e}"))?;

        // Mean pooling over the sequence dimension, masked so padding
        // tokens (attention_mask == 0) don't contribute.
        let mask_tensor = Tensor::new(attention_mask, &self.device)
            .map_err(|e| e.to_string())?
            .to_dtype(DType::F32)
            .map_err(|e| e.to_string())?
            .unsqueeze(0)
            .map_err(|e| e.to_string())?
            .unsqueeze(2)
            .map_err(|e| e.to_string())?;
        let masked = output.broadcast_mul(&mask_tensor).map_err(|e| e.to_string())?;
        let summed = masked.sum(1).map_err(|e| e.to_string())?;
        let counts = mask_tensor.sum(1).map_err(|e| e.to_string())?;
        let mean_pooled = summed.broadcast_div(&counts).map_err(|e| e.to_string())?;

        let values: Vec<f32> = mean_pooled
            .squeeze(0)
            .map_err(|e| e.to_string())?
            .to_vec1()
            .map_err(|e| e.to_string())?;

        let norm: f32 = values.iter().map(|v| v * v).sum::<f32>().sqrt();
        let normalized = if norm > 1e-9 {
            values.iter().map(|v| v / norm).collect()
        } else {
            values
        };

        Ok(NeuralVector(normalized))
    }
}

/// Prints (does not run) the commands to fetch a suitable local model
/// snapshot via the Hugging Face CLI, since this module deliberately
/// does not bundle or auto-download model weights (see the module doc
/// comment).
pub fn download_hint(model_id: &str, target_dir: &str) -> String {
    format!(
        "# Nexus doesn't bundle or auto-download neural embedding models.\n\
         # Fetch one yourself, e.g. with the huggingface_hub Python CLI:\n\
         pip install huggingface_hub\n\
         huggingface-cli download {model_id} --local-dir {target_dir}\n\
         # then point NeuralEmbedder::load at {target_dir}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_similarity_of_identical_vectors_is_one() {
        let v = NeuralVector(vec![0.6, 0.8]);
        assert!((v.cosine_similarity(&v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn mismatched_dimensions_return_zero_not_a_panic() {
        let a = NeuralVector(vec![1.0, 0.0]);
        let b = NeuralVector(vec![1.0, 0.0, 0.0]);
        assert_eq!(a.cosine_similarity(&b), 0.0);
    }

    #[test]
    fn load_reports_a_clear_error_for_a_missing_model_directory() {
        let result = NeuralEmbedder::load(Path::new("/no/such/model/dir"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("config.json"));
    }
}
