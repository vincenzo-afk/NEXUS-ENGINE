// Core library: pure-Rust index, query, search, and ranking code. These
// modules are platform-agnostic and are what the WASM build ships to the
// browser. The server-only modules below depend on system APIs (files,
// sockets, blocking network clients, SQLite, etc.) that cannot exist on
// `wasm32-unknown-unknown` and are therefore gated out of the WASM build.

pub mod vector;
pub mod autocomplete;
// Browser (WASM) module: compiled only for the `wasm32` target because it
// depends on `wasm-bindgen` and `web-sys`, which don't exist on native.
#[cfg(all(target_arch = "wasm32", feature = "wasm"))]
pub mod browser;
pub mod classify;
pub mod clicks;
pub mod config;
pub mod crypto_index;
pub mod dedup;
pub mod document;
pub mod entity;
pub mod error;
pub mod extract;
pub mod formats;
pub mod fs;
pub mod graph;
pub mod html;
pub mod index;
pub mod query;
pub mod ranking;
pub mod search;
pub mod spellcheck;
pub mod text;
pub mod storage;

// Server-only modules: CLI, HTTP/WebSocket APIs, crawlers, AI/Llm,
// federation, and everything built on blocking network clients or
// file-system/network OS primitives. The WASM build cannot use any of
// these, and they cannot compile on `wasm32-unknown-unknown`.
#[cfg(not(target_family = "wasm"))]
pub mod answers;
#[cfg(not(target_family = "wasm"))]
pub mod api;
#[cfg(not(target_family = "wasm"))]
pub mod bangs;
#[cfg(not(target_family = "wasm"))]
pub mod bench;
#[cfg(not(target_family = "wasm"))]
pub mod ai;
#[cfg(not(target_family = "wasm"))]
pub mod cli;
#[cfg(not(target_family = "wasm"))]
pub mod federation;
#[cfg(not(target_family = "wasm"))]
pub mod metrics;
#[cfg(not(target_family = "wasm"))]
pub mod network;
#[cfg(not(target_family = "wasm"))]
pub mod observability;
// privacy is pure crypto (chacha20poly1305, x25519, hkdf, rand) and works
// on wasm — crypto_index and the encrypted crypto index live in core.
pub mod privacy;
#[cfg(not(target_family = "wasm"))]
pub mod profiles;
#[cfg(not(target_family = "wasm"))]
pub mod stats;
#[cfg(not(target_family = "wasm"))]
pub mod watcher;
#[cfg(not(target_family = "wasm"))]
pub mod web;
// webdoc is pure (document/classify only) and stays core.
pub mod webdoc;
