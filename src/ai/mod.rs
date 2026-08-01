//! Optional AI reranking and citation-grounded summarization, via a
//! user-configured OpenAI-compatible chat completions endpoint.
//!
//! **Nothing in this module runs by default.** Nexus does not ship with
//! any LLM access, does not call out to any AI service unless you
//! configure one, and does not bundle model weights of any kind. Set
//! `[ai] enabled = true` and `api_key = "..."` in `config.toml` (pointing
//! `api_base_url` at OpenAI, or any self-hosted OpenAI-compatible server
//! — Ollama, LM Studio, vLLM, LocalAI, etc.) to turn this on. Both
//! features degrade gracefully to "not available" rather than failing
//! the underlying search when AI isn't configured or a request fails —
//! see [`client::LlmClient::from_config`].

pub mod client;
pub mod rerank;
pub mod summarize;

pub use client::LlmClient;
pub use rerank::{rerank, RerankCandidate};
#[allow(unused_imports)]
pub use summarize::{summarize, GroundedSummary, SummarySource};
