## Summary

Describe the intent of this change and link the related issue when one exists.

## Validation

- [ ] `cargo test --all-targets`
- [ ] `cargo build --lib --release --target wasm32-unknown-unknown --features wasm` when browser bindings or shared library code changed
- [ ] Documentation and examples are updated when behavior, commands, or configuration changed

## Impact review

- [ ] This does not add secrets, personal data, or generated index files
- [ ] This does not change API compatibility, or the compatibility impact is documented above
- [ ] This does not bypass crawl budgets, robots.txt protections, or security controls without an explicit justification
