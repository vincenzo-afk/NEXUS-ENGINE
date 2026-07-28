//! Browser/WASM module for running Nexus in the browser.
//! All code is gated behind the `wasm` feature flag.
#![cfg(feature = "wasm")]

pub mod indexeddb;
pub mod wasm;
pub mod workers;
