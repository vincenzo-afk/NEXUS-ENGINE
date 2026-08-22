# Contributing to NEXUS-ENGINE

Contributions are welcome when they preserve the project’s privacy-first and resource-bounded search model. Before opening a pull request, please search the existing issues for related work and describe any command-line, API, crawler, ranking, or browser-WASM compatibility impact.

## Development setup

Install the current stable Rust toolchain, clone the repository, and run the native test suite from the repository root.

```bash
git clone https://github.com/vincenzo-afk/NEXUS-ENGINE.git
cd NEXUS-ENGINE
cargo test --all-targets
```

Changes that touch `src/browser/`, shared library code, or Cargo features must also compile the browser target.

```bash
rustup target add wasm32-unknown-unknown
cargo build --lib --release --target wasm32-unknown-unknown --features wasm
```

## Contribution expectations

Keep pull requests narrowly scoped and write commit subjects that describe the completed change. Include tests for behavior changes whenever practical, keep configuration defaults conservative, and do not commit indexes, crawled pages, API keys, credentials, or personally identifiable search data.

For crawler changes, preserve the existing robots.txt handling, request limits, retry behavior, and per-domain budget unless the change explicitly and safely modifies those policies. For ranking changes, add or update focused tests that make the intended ordering behavior clear.

## Pull requests

Use the pull-request template to explain intent, validation, compatibility, and security implications. A pull request should leave `cargo test --all-targets` passing; browser binding changes should also leave the documented WASM build producing `target/wasm32-unknown-unknown/release/nexus.wasm`.

## Reporting issues

Use the repository’s issue forms for reproducible bugs and feature proposals. Do not place sensitive vulnerability details, private index contents, access tokens, or crawled personal data in a public issue. Security-sensitive reports belong in the private vulnerability reporting channel described in [SECURITY.md](./SECURITY.md).
